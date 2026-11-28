//! Animated GIF, WebP and PNG (APNG): composited onto the canvas with exact disposal,
//! transparency, embedded ICC-to-sRGB colour management, optional display-size downsampling,
//! and linear-light mip generation.
//!
//! Small animations (whose scaled decoded frames fit within [`CACHED_BUDGET`]) keep every frame
//! in memory for zero-CPU playback. Larger animations stream frames on demand through a single
//! shared background stepper (keeping only frame 0, the encoded bytes, and the active/prefetched
//! frame) so a long high-DPI GIF uses a few MiB instead of tens or hundreds of MiB, and drops
//! its streaming decoder as soon as it leaves the screen.

use std::io::Cursor;
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::{HashMap, VecDeque};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Condvar, Mutex, Weak};

use image::{AnimationDecoder, ImageDecoder};

use super::{Decoded, ImageError, Mip, icc, maybe_downsample_rgba, mip_chain, scaled_dimensions};

/// Maximum decoded pixel bytes kept eagerly in RAM for one cached animation (4 MiB). Larger
/// animations switch to on-demand streaming on native targets (or fall back to still on web
/// past [`WEB_BUDGET`]).
const CACHED_BUDGET: usize = 4 << 20;

/// Web fallback cap when streaming is unavailable (matches legacy 256 MiB ceiling).
#[cfg(target_arch = "wasm32")]
const WEB_BUDGET: usize = 256 << 20;

/// Borrowed or shared mip slice for one animation frame.
pub(crate) enum FrameMips<'a> {
    Borrowed(&'a [Mip]),
    #[cfg(not(target_arch = "wasm32"))]
    Owned(Arc<Vec<Mip>>),
}

impl<'a> std::ops::Deref for FrameMips<'a> {
    type Target = [Mip];

    fn deref(&self) -> &[Mip] {
        match self {
            FrameMips::Borrowed(mips) => mips,
            #[cfg(not(target_arch = "wasm32"))]
            FrameMips::Owned(mips) => mips,
        }
    }
}

/// The frames after the first (which is [`Decoded::mips`]) and when each frame ends.
pub(crate) struct Motion {
    /// Frames 1.. as mip chains when cached eagerly (empty when streaming).
    pub frames: Vec<Vec<Mip>>,
    /// On-demand streaming state for animations exceeding [`CACHED_BUDGET`].
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) stream: Option<Arc<StreamSlot>>,
    /// Seconds from the start of a play to the end of each frame, frame 0's first; the last is
    /// the length of a play.
    pub ends: Vec<f64>,
    /// How many times it plays; `None` loops forever.
    pub plays: Option<u32>,
}

impl Motion {
    /// The frame `t` seconds after the animation started, and when (on the same clock) the
    /// next one shows; `None` once its last play has ended and it rests on its last frame.
    pub(crate) fn at(&self, t: f64) -> (usize, Option<f64>) {
        let last = self.ends.len() - 1;
        let period = self.ends[last];
        if period <= 0.0 {
            return (0, None);
        }
        let play = (t.max(0.0) / period).floor();
        if self.plays.is_some_and(|n| play >= f64::from(n)) { return (last, None); }
        let within = t.max(0.0) - play * period;
        let i = self.ends.partition_point(|&end| end <= within).min(last);
        (i, Some(play * period + self.ends[i]))
    }

    /// Frame `i`'s mips, given frame 0's, plus the actual frame index returned (which may briefly
    /// hold the previous frame while a streaming animation prefetches after a jump).
    pub(crate) fn frame_for<'a>(
        &'a self,
        first: &'a [Mip],
        i: usize,
        source: &Arc<rune_core::live::Source>,
    ) -> (usize, FrameMips<'a>) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(stream) = &self.stream {
            return stream.frame_for(first, i, self.ends.len(), source);
        }
        #[cfg(target_arch = "wasm32")]
        let _ = source;
        if i == 0 {
            (0, FrameMips::Borrowed(first))
        } else {
            (i, FrameMips::Borrowed(&self.frames[i - 1]))
        }
    }

    /// Live CPU memory held by this motion (cached frames, or streaming source + active buffers).
    pub(crate) fn decoded_bytes(&self) -> usize {
        let cached: usize = self
            .frames
            .iter()
            .flat_map(|chain| chain.iter())
            .fold(0usize, |acc, mip| acc.saturating_add(mip.pixels.len()));
        #[cfg(not(target_arch = "wasm32"))]
        let streamed = self.stream.as_ref().map_or(0, |slot| slot.decoded_bytes());
        #[cfg(target_arch = "wasm32")]
        let streamed = 0;
        cached.saturating_add(streamed)
    }

    /// Drops transient streaming buffers and closes the background decoder when offscreen.
    pub(crate) fn trim_idle(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(stream) = &self.stream {
            stream.trim_idle();
        }
    }
}

/// Which animated format `bytes` sniff as. A web build hands GIF and WebP to the browser,
/// which decodes their first frame.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    #[cfg(not(target_arch = "wasm32"))]
    Gif,
    #[cfg(not(target_arch = "wasm32"))]
    WebP,
    Png,
}

fn sniff(bytes: &[u8]) -> Option<Kind> {
    #[cfg(not(target_arch = "wasm32"))]
    if bytes.starts_with(b"GIF8") { return Some(Kind::Gif); }
    #[cfg(not(target_arch = "wasm32"))]
    if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" { return Some(Kind::WebP); }
    bytes.starts_with(b"\x89PNG\r\n\x1a\n").then_some(Kind::Png)
}

fn decoder_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(super::MAX_SIDE);
    limits.max_image_height = Some(super::MAX_SIDE);
    limits.max_alloc = Some(super::MAX_ALLOC);
    limits
}

fn delay_secs(frame: &image::Frame) -> f64 {
    let (numer, denom) = frame.delay().numer_denom_ms();
    let ms = f64::from(numer) / f64::from(denom.max(1));
    // Browsers show a frame of 10 ms or less for 100 ms: such files mean "as fast as you
    // can", which would otherwise spin.
    if ms <= 10.0 { 0.1 } else { ms / 1000.0 }
}

fn process_frame(
    frame: image::Frame,
    transform: Option<&icc::Transform>,
    max_px: Option<u32>,
) -> Result<Vec<Mip>, ImageError> {
    let mut buffer = frame.into_buffer();
    let (w, h) = (buffer.width(), buffer.height());
    if let Some(transform) = transform {
        transform.apply_rgba(&mut buffer);
    }
    let (nw, nh, raw) = maybe_downsample_rgba(w, h, buffer.into_raw(), max_px)?;
    Ok(mip_chain(nw, nh, &raw)?.mips)
}

/// An animation of more than one frame, or `None` for a still (or a format that isn't
/// animated), which decodes as any image does.
///
/// When `max_px` is `Some`, each composited frame is downsampled in linear-premultiplied light
/// to fit `max_px` square (preserving aspect ratio and never upscaling) before mip generation.
/// `alive` is checked cooperatively between frames so abandoned decodes abort early.
pub(super) fn decode(
    bytes: Arc<[u8]>,
    max_px: Option<u32>,
    alive: &dyn Fn() -> bool,
) -> Option<Result<Decoded, ImageError>> {
    let kind = sniff(&bytes)?;
    let error = |e: image::ImageError| ImageError(e.to_string());
    let cursor = Cursor::new(bytes.as_ref());
    let limits = decoder_limits();
    // Each format's own loop count: a GIF's says how often it repeats after its first play, a
    // WebP's and an APNG's how often it plays (as browsers count them).
    let (size, icc_profile, mut frames, plays) = match kind {
        #[cfg(not(target_arch = "wasm32"))]
        Kind::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(cursor).ok()?;
            decoder.set_limits(limits).ok()?;
            let icc_profile = decoder.icc_profile().ok().flatten();
            let plays = finite(decoder.loop_count()).map(|n| n.saturating_add(1));
            (decoder.dimensions(), icc_profile, decoder.into_frames(), plays)
        }
        #[cfg(not(target_arch = "wasm32"))]
        Kind::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(cursor).ok()?;
            decoder.set_limits(limits).ok()?;
            if !decoder.has_animation() { return None; }
            let icc_profile = decoder.icc_profile().ok().flatten();
            // Browsers composite on transparency, whatever background the file names.
            decoder.set_background_color(image::Rgba([0, 0, 0, 0])).ok()?;
            let plays = finite(decoder.loop_count());
            (decoder.dimensions(), icc_profile, decoder.into_frames(), plays)
        }
        Kind::Png => {
            let mut decoder = image::codecs::png::PngDecoder::with_limits(cursor, limits).ok()?;
            if !decoder.is_apng().ok()? { return None; }
            let size = decoder.dimensions();
            let icc_profile = decoder.icc_profile().ok().flatten();
            let decoder = decoder.apng().ok()?;
            let plays = finite(decoder.loop_count());
            (size, icc_profile, decoder.into_frames(), plays)
        }
    };
    let (w, h) = size;
    if w == 0 || h == 0 || w > super::MAX_SIDE || h > super::MAX_SIDE {
        return Some(Err(ImageError("animation is too large".into())));
    }
    let (nw, nh) = match max_px {
        Some(limit) => scaled_dimensions(w, h, limit),
        None => (w, h),
    };
    let per_frame = (nw as usize)
        .saturating_mul(nh as usize)
        .saturating_mul(16)
        / 3;
    let transform = icc_profile.as_deref().and_then(icc::Transform::to_srgb);

    // Fast path on native: for GIFs whose scanned frame count already exceeds CACHED_BUDGET,
    // decode only frame 0 and switch directly to streaming without decoding frames 1..N up front.
    #[cfg(not(target_arch = "wasm32"))]
    if kind == Kind::Gif
        && let Some(ends) = scan_gif_ends(&bytes)
        && ends.len() >= 2
        && ends.len().saturating_mul(per_frame) > CACHED_BUDGET
    {
        if !alive() {
            return Some(Err(ImageError("image decode cancelled".into())));
        }
        let first_frame = match frames.next()? {
            Ok(frame) => frame,
            Err(e) => return Some(Err(error(e))),
        };
        drop(frames);
        let first = match process_frame(first_frame, transform.as_ref(), max_px) {
            Ok(mips) => mips,
            Err(e) => return Some(Err(e)),
        };
        let slot = StreamSlot::new(bytes, kind, max_px, icc_profile, (w, h), ends.len());
        return Some(Ok(Decoded {
            mips: first,
            motion: Some(Box::new(Motion {
                frames: Vec::new(),
                stream: Some(slot),
                ends,
                plays,
            })),
            intrinsic: None,
        }));
    }

    let mut chains = Vec::new();
    let mut ends = Vec::new();
    let mut elapsed = 0.0;
    #[cfg(not(target_arch = "wasm32"))]
    let mut switched_to_stream = false;

    while let Some(frame) = frames.next() {
        if !alive() {
            return Some(Err(ImageError("image decode cancelled".into())));
        }
        let frame = match frame {
            Ok(frame) => frame,
            // A truncated file plays the frames it has, as browsers do.
            Err(_) if !chains.is_empty() => break,
            Err(e) => return Some(Err(error(e))),
        };
        #[cfg(target_arch = "wasm32")]
        if !chains.is_empty() && (chains.len() + 1).saturating_mul(per_frame) > WEB_BUDGET {
            return Some(still(chains));
        }
        #[cfg(not(target_arch = "wasm32"))]
        if !chains.is_empty() && (chains.len() + 1).saturating_mul(per_frame) > CACHED_BUDGET {
            // Exceeds cached budget: free frames 1.. immediately, finish scanning remaining frame
            // delays without retaining decoded pixels, and serve subsequent frames via streaming.
            chains.truncate(1);
            elapsed += delay_secs(&frame);
            ends.push(elapsed);
            drop(frame);
            for rest in &mut frames {
                if !alive() {
                    return Some(Err(ImageError("image decode cancelled".into())));
                }
                match rest {
                    Ok(f) => {
                        elapsed += delay_secs(&f);
                        ends.push(elapsed);
                    }
                    Err(_) => break,
                }
            }
            switched_to_stream = true;
            break;
        }
        elapsed += delay_secs(&frame);
        match process_frame(frame, transform.as_ref(), max_px) {
            Ok(mips) => chains.push(mips),
            Err(e) => return Some(Err(e)),
        }
        ends.push(elapsed);
    }

    drop(frames);

    #[cfg(not(target_arch = "wasm32"))]
    if switched_to_stream {
        let Some(first) = chains.into_iter().next() else {
            return Some(Err(ImageError("animation has no frames".into())));
        };
        if ends.len() < 2 {
            return Some(Ok(Decoded { mips: first, motion: None, intrinsic: None }));
        }
        let slot = StreamSlot::new(bytes, kind, max_px, icc_profile, (w, h), ends.len());
        return Some(Ok(Decoded {
            mips: first,
            motion: Some(Box::new(Motion {
                frames: Vec::new(),
                stream: Some(slot),
                ends,
                plays,
            })),
            intrinsic: None,
        }));
    }

    if chains.len() < 2 { return (!chains.is_empty()).then(|| still(chains)); }
    let mut chains = chains.into_iter();
    let Some(first) = chains.next() else {
        return Some(Err(ImageError("animation has no frames".into())));
    };
    Some(Ok(Decoded {
        mips: first,
        motion: Some(Box::new(Motion {
            frames: chains.collect(),
            #[cfg(not(target_arch = "wasm32"))]
            stream: None,
            ends,
            plays,
        })),
        intrinsic: None,
    }))
}

/// The first frame alone, for an animation too large to hold.
fn still(chains: Vec<Vec<Mip>>) -> Result<Decoded, ImageError> {
    let first = chains.into_iter().next().ok_or_else(|| ImageError("animation has no frames".into()))?;
    Ok(Decoded { mips: first, motion: None, intrinsic: None })
}

fn finite(count: image::metadata::LoopCount) -> Option<u32> {
    match count {
        image::metadata::LoopCount::Infinite => None,
        image::metadata::LoopCount::Finite(n) => Some(n.get()),
    }
}

/// Scans a GIF's block structure to collect cumulative frame end times without decompressing
/// LZW pixel payloads. Returns `None` on malformed or truncated streams so the caller falls
/// back to the standard decoder loop.
#[cfg(not(target_arch = "wasm32"))]
fn scan_gif_ends(bytes: &[u8]) -> Option<Vec<f64>> {
    if bytes.len() < 13 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    let packed = bytes[10];
    let gct = if packed & 0x80 != 0 { 3usize << ((packed & 0x07) + 1) } else { 0 };
    let mut at = 13 + gct;
    if at > bytes.len() {
        return None;
    }
    let mut ends = Vec::new();
    let mut elapsed = 0.0;
    let mut pending_delay_cs: u16 = 0;
    while at < bytes.len() {
        match bytes[at] {
            // Extension block
            0x21 => {
                let label = *bytes.get(at + 1)?;
                at += 2;
                if label == 0xF9 {
                    // Graphic Control Extension: block size 4, [packed, delay_lo, delay_hi, trans_idx], 0
                    let sz = *bytes.get(at)? as usize;
                    if sz >= 4 && at + 1 + sz <= bytes.len() {
                        pending_delay_cs = u16::from_le_bytes([bytes[at + 2], bytes[at + 3]]);
                    }
                }
                at = skip_gif_sub_blocks(bytes, at)?;
            }
            // Image descriptor
            0x2C => {
                let lct_packed = *bytes.get(at + 9)?;
                let lct = if lct_packed & 0x80 != 0 { 3usize << ((lct_packed & 0x07) + 1) } else { 0 };
                // Descriptor (10) + local colour table + LZW minimum code size (1)
                at = at.checked_add(11 + lct)?;
                if at > bytes.len() {
                    return None;
                }
                at = skip_gif_sub_blocks(bytes, at)?;
                let ms = f64::from(pending_delay_cs) * 10.0;
                elapsed += if ms <= 10.0 { 0.1 } else { ms / 1000.0 };
                ends.push(elapsed);
                pending_delay_cs = 0;
            }
            // Trailer
            0x3B => break,
            _ => return None,
        }
    }
    (!ends.is_empty()).then_some(ends)
}

#[cfg(not(target_arch = "wasm32"))]
fn skip_gif_sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = usize::from(*bytes.get(at)?);
        at += 1;
        if len == 0 {
            return Some(at);
        }
        at = at.checked_add(len)?;
        if at > bytes.len() {
            return None;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn mips_bytes(mips: &[Mip]) -> usize {
    mips.iter().fold(0usize, |acc, m| acc.saturating_add(m.pixels.len()))
}

/// Shared Send+Sync handle for one streaming animation; the actual `!Send` `image::Frames<'static>`
/// iterator lives exclusively on the shared `rune-anim` background stepper thread.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct StreamSlot {
    id: u64,
    bytes: Arc<[u8]>,
    kind: Kind,
    max_px: Option<u32>,
    icc_profile: Option<Vec<u8>>,
    canvas_bytes: usize,
    total_frames: usize,
    state: Mutex<SlotState>,
}

#[cfg(not(target_arch = "wasm32"))]
struct SlotState {
    current: Option<(usize, Arc<Vec<Mip>>)>,
    next: Option<(usize, Arc<Vec<Mip>>)>,
    /// Whether the worker currently holds a live decoder canvas for this slot.
    decoder_live: bool,
    /// Monotonic counter bumped by `trim_idle()` so in-flight worker results are discarded
    /// cleanly instead of resurrecting buffers after eviction.
    trim_epoch: u64,
    /// Frame index currently being decoded by the worker, avoiding duplicate rewind requests.
    in_flight: Option<usize>,
    /// Pending frame index requested from the worker (coalesced latest-wins).
    pending_target: Option<usize>,
    /// Whether a request for this slot is currently queued on the worker.
    queued: bool,
    /// First frame index known to fail decoding (e.g. corrupt/truncated LZW payload); requests
    /// at or past this frame clamp cleanly instead of spinning in a wake loop.
    failed_from: Option<usize>,
    /// Wake source when the requested frame lands and is needed immediately.
    wake: Option<Weak<rune_core::live::Source>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl StreamSlot {
    fn new(
        bytes: Arc<[u8]>,
        kind: Kind,
        max_px: Option<u32>,
        icc_profile: Option<Vec<u8>>,
        (w, h): (u32, u32),
        total_frames: usize,
    ) -> Arc<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            bytes,
            kind,
            max_px,
            icc_profile,
            canvas_bytes: (w as usize).saturating_mul(h as usize).saturating_mul(4),
            total_frames,
            state: Mutex::new(SlotState {
                current: None,
                next: None,
                decoder_live: false,
                trim_epoch: 0,
                in_flight: None,
                pending_target: None,
                queued: false,
                failed_from: None,
                wake: None,
            }),
        })
    }

    fn decoded_bytes(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cur = state.current.as_ref().map_or(0, |(_, m)| mips_bytes(m));
        let nxt = state.next.as_ref().map_or(0, |(_, m)| mips_bytes(m));
        let icc = self.icc_profile.as_ref().map_or(0, Vec::len);
        let canvas = if state.decoder_live { self.canvas_bytes } else { 0 };
        self.bytes
            .len()
            .saturating_add(icc)
            .saturating_add(cur)
            .saturating_add(nxt)
            .saturating_add(canvas)
    }

    fn trim_idle(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.trim_epoch = state.trim_epoch.wrapping_add(1);
        state.current = None;
        state.next = None;
        state.pending_target = None;
        state.wake = None;
        let should_drop = state.decoder_live || state.in_flight.is_some();
        state.decoder_live = false;
        // Enqueue Drop while holding `slot.state` (preserving `slot.state` -> `ANIM_WORKER`
        // lock order) so a concurrent `frame_for` cannot interleave `Decode` ahead of `Drop`.
        if should_drop {
            stream_worker_drop(self.id);
        }
    }

    fn frame_for<'a>(
        self: &Arc<Self>,
        first: &'a [Mip],
        i: usize,
        total: usize,
        source: &Arc<rune_core::live::Source>,
    ) -> (usize, FrameMips<'a>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let effective_total = state.failed_from.map_or(total, |limit| limit.min(total));
        if effective_total <= 1 {
            return (0, FrameMips::Borrowed(first));
        }
        let target = i.min(effective_total - 1);
        let next_idx = if target + 1 < effective_total { target + 1 } else { 0 };

        // Frame 0 always lives in Decoded::mips; prefetch frame 1 unless already held.
        if target == 0 {
            if next_idx > 0
                && state.current.as_ref().is_none_or(|(idx, _)| *idx != next_idx)
                && state.next.as_ref().is_none_or(|(idx, _)| *idx != next_idx)
            {
                self.schedule_locked(&mut state, next_idx, None);
            }
            return (0, FrameMips::Borrowed(first));
        }

        // Exact hit on current frame: return immediately and ensure next frame is prefetched.
        if let Some((idx, mips)) = &state.current
            && *idx == target
        {
            let mips = Arc::clone(mips);
            if next_idx > 0 && state.next.as_ref().is_none_or(|(n, _)| *n != next_idx) {
                self.schedule_locked(&mut state, next_idx, None);
            }
            return (target, FrameMips::Owned(mips));
        }

        // Exact hit on prefetched next frame: promote to current in 0 ms and prefetch next_idx.
        if let Some((_, mips)) = state.next.take_if(|(idx, _)| *idx == target) {
            state.current = Some((target, Arc::clone(&mips)));
            if next_idx > 0 {
                self.schedule_locked(&mut state, next_idx, None);
            }
            return (target, FrameMips::Owned(mips));
        }

        // Miss (jump, wrap, or worker still finishing): request `target` with wakeup and show
        // the latest available frame without flickering blank.
        self.schedule_locked(&mut state, target, Some(Arc::downgrade(source)));
        if let Some((idx, mips)) = &state.current {
            (*idx, FrameMips::Owned(Arc::clone(mips)))
        } else {
            (0, FrameMips::Borrowed(first))
        }
    }

    fn schedule_locked(
        self: &Arc<Self>,
        state: &mut SlotState,
        target: usize,
        wake: Option<Weak<rune_core::live::Source>>,
    ) {
        if target == 0 || target >= self.total_frames {
            return;
        }
        if state.failed_from.is_some_and(|limit| target >= limit) {
            return;
        }
        if wake.is_some() {
            state.wake = wake;
        }
        // If the worker is already decoding this exact frame, wait for it instead of queuing a
        // duplicate request that would rewind the decoder from frame 0.
        if state.in_flight == Some(target) {
            return;
        }
        state.pending_target = Some(target);
        if !state.queued {
            state.queued = true;
            if !stream_worker_push(AnimCmd::Decode(Arc::downgrade(self))) {
                state.queued = false;
                state.pending_target = None;
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for StreamSlot {
    fn drop(&mut self) {
        stream_worker_drop(self.id);
    }
}

#[cfg(not(target_arch = "wasm32"))]
enum AnimCmd {
    Decode(Weak<StreamSlot>),
    Drop(u64),
}

#[cfg(not(target_arch = "wasm32"))]
struct WorkerQueue {
    cmds: VecDeque<AnimCmd>,
    running: bool,
}

#[cfg(not(target_arch = "wasm32"))]
static ANIM_WORKER: (Mutex<WorkerQueue>, Condvar) = (
    Mutex::new(WorkerQueue { cmds: VecDeque::new(), running: false }),
    Condvar::new(),
);

/// Pushes `cmd` to the shared animation worker, spawning it lazily if stopped. Returns `false`
/// if the worker could not be started.
#[cfg(not(target_arch = "wasm32"))]
fn stream_worker_push(cmd: AnimCmd) -> bool {
    let (lock, cv) = &ANIM_WORKER;
    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
    q.cmds.push_back(cmd);
    if !q.running {
        q.running = true;
        drop(q);
        if rune_core::task::spawn("rune-anim", anim_worker_loop).is_err() {
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            q.running = false;
            q.cmds.clear();
            return false;
        }
        return true;
    }
    cv.notify_one();
    true
}

#[cfg(not(target_arch = "wasm32"))]
fn stream_worker_drop(id: u64) {
    let (lock, cv) = &ANIM_WORKER;
    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
    if !q.running {
        return;
    }
    q.cmds.push_back(AnimCmd::Drop(id));
    cv.notify_one();
}

#[cfg(not(target_arch = "wasm32"))]
struct LiveStream {
    frames: image::Frames<'static>,
    /// Index of the next frame that `frames.next()` will yield (0 right after opening).
    next_pos: usize,
    transform: Option<icc::Transform>,
    last_used: rune_core::time::Instant,
}

#[cfg(not(target_arch = "wasm32"))]
fn open_live_stream(slot: &StreamSlot) -> Option<LiveStream> {
    let cursor = Cursor::new(Arc::clone(&slot.bytes));
    let limits = decoder_limits();
    let frames = match slot.kind {
        Kind::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(cursor).ok()?;
            decoder.set_limits(limits).ok()?;
            decoder.into_frames()
        }
        Kind::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(cursor).ok()?;
            decoder.set_limits(limits).ok()?;
            decoder.set_background_color(image::Rgba([0, 0, 0, 0])).ok()?;
            decoder.into_frames()
        }
        Kind::Png => image::codecs::png::PngDecoder::with_limits(cursor, limits)
            .ok()?
            .apng()
            .ok()?
            .into_frames(),
    };
    let transform = slot.icc_profile.as_deref().and_then(icc::Transform::to_srgb);
    Some(LiveStream {
        frames,
        next_pos: 0,
        transform,
        last_used: rune_core::time::now(),
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn step_live_stream(slot: &StreamSlot, live: &mut LiveStream, target: usize) -> Option<Vec<Mip>> {
    if target < live.next_pos {
        *live = open_live_stream(slot)?;
    }
    while live.next_pos < target {
        match live.frames.next() {
            Some(Ok(_)) => live.next_pos += 1,
            _ => return None,
        }
    }
    let frame = live.frames.next()?.ok()?;
    live.next_pos += 1;
    live.last_used = rune_core::time::now();
    process_frame(frame, live.transform.as_ref(), slot.max_px).ok()
}

/// Idle timeout before the animation stepper drops untouched decoders and exits cleanly.
#[cfg(not(target_arch = "wasm32"))]
const WORKER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(not(target_arch = "wasm32"))]
fn sweep_stale_streams(
    live: &mut HashMap<u64, LiveStream>,
    slots: &mut HashMap<u64, Weak<StreamSlot>>,
    now: rune_core::time::Instant,
) {
    live.retain(|id, stream| {
        let keep = now.saturating_duration_since(stream.last_used) < WORKER_IDLE_TIMEOUT;
        if !keep {
            if let Some(slot) = slots.remove(id).and_then(|w| w.upgrade()) {
                slot.state.lock().unwrap_or_else(|e| e.into_inner()).decoder_live = false;
            }
        }
        keep
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn anim_worker_loop() {
    let (lock, cv) = &ANIM_WORKER;
    let mut live: HashMap<u64, LiveStream> = HashMap::new();
    let mut slots: HashMap<u64, Weak<StreamSlot>> = HashMap::new();
    let mut last_sweep = rune_core::time::now();

    loop {
        let cmd = {
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(cmd) = q.cmds.pop_front() {
                    break Some(cmd);
                }
                if live.is_empty() {
                    q.running = false;
                    return;
                }
                let (guard, timeout) = cv
                    .wait_timeout(q, WORKER_IDLE_TIMEOUT)
                    .unwrap_or_else(|e| e.into_inner());
                q = guard;
                if timeout.timed_out() && q.cmds.is_empty() {
                    break None;
                }
            }
        };

        let now = rune_core::time::now();
        if cmd.is_none() || now.saturating_duration_since(last_sweep) >= WORKER_IDLE_TIMEOUT {
            sweep_stale_streams(&mut live, &mut slots, now);
            last_sweep = now;
        }
        let Some(cmd) = cmd else { continue };

        match cmd {
            AnimCmd::Drop(id) => {
                live.remove(&id);
                slots.remove(&id);
            }
            AnimCmd::Decode(weak) => {
                let Some(slot) = weak.upgrade() else { continue };
                let (target, epoch) = {
                    let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.queued = false;
                    let Some(target) = state.pending_target.take() else {
                        continue;
                    };
                    // Skip if frame is already held in current or next.
                    let already_have = state.current.as_ref().is_some_and(|(idx, _)| *idx == target)
                        || state.next.as_ref().is_some_and(|(idx, _)| *idx == target);
                    if already_have {
                        continue;
                    }
                    state.in_flight = Some(target);
                    (target, state.trim_epoch)
                };

                let decoded = match live.entry(slot.id) {
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        step_live_stream(&slot, entry.get_mut(), target).map(Arc::new)
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        if let Some(opened) = open_live_stream(&slot) {
                            slots.insert(slot.id, Arc::downgrade(&slot));
                            let stream = entry.insert(opened);
                            step_live_stream(&slot, stream, target).map(Arc::new)
                        } else {
                            None
                        }
                    }
                };

                let mut trimmed = false;
                let wake = {
                    let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.in_flight = None;
                    if state.trim_epoch != epoch {
                        // `trim_idle()` ran while decoding: discard result and drop decoder.
                        trimmed = true;
                        state.decoder_live = false;
                        state.wake = None;
                        None
                    } else if let Some(mips) = decoded {
                        state.decoder_live = true;
                        state.next = Some((target, mips));
                        if state.pending_target.is_some() && !state.queued {
                            state.queued = true;
                            if !stream_worker_push(AnimCmd::Decode(Arc::downgrade(&slot))) {
                                state.queued = false;
                            }
                        }
                        state.wake.take()
                    } else {
                        // Frame failed to decode (corrupt/truncated payload): record the limit
                        // and do not notify `wake` so the UI never enters a tight redraw loop.
                        state.failed_from = Some(
                            state
                                .failed_from
                                .map_or(target, |prev| prev.min(target)),
                        );
                        state.decoder_live = live.contains_key(&slot.id);
                        state.wake = None;
                        None
                    }
                };
                if trimmed {
                    live.remove(&slot.id);
                    slots.remove(&slot.id);
                }
                if let Some(source) = wake.and_then(|w| w.upgrade()) {
                    source.notify();
                }
            }
        }
    }
}
