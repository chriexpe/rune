//! Backdrops: system materials behind the window (`El::system_material`) and blur of the
//! window's own content (`El::backdrop_blur`).
use crate::El;

impl El {
    /// Back this node with a system backdrop material: the platform blurs what is behind the
    /// window under the node's rect and corner radius (the largest of its corners), which follow
    /// layout and animation like any other paint. Native on macOS in transparent windows, where
    /// it replaces the fill and the node's shadows skip its interior; elsewhere, in opaque
    /// windows and in headless renders, the fill (or a neutral tint without one) draws instead.
    /// Native materials sit under everything the window draws and ignore ancestor clips.
    /// `Glass` also turns on `continuous_corners`, the only curve `NSGlassEffectView` draws, so
    /// the fill, clip and shadow hole match the glass; `Popover` follows `continuous_corners`.
    pub fn system_material(mut self, material: crate::SystemMaterial) -> Self {
        if material == crate::SystemMaterial::Glass { self.vis_flags.set(crate::el::VisualFlags::CONTINUOUS_CORNERS, true); }
        self.cold_mut().material = Some(material);
        self
    }

    /// Blurs what is already drawn behind this element in the same window, inside its rounded
    /// (or continuous) corners and clip, then draws the element's fill over the blur as a tint,
    /// with its border and children on top: `el().backdrop_blur(8.0).fill(tint)` is a frosted
    /// bar. `sigma` is the Gaussian standard deviation in points, as in CSS
    /// `backdrop-filter: blur()`; 0 turns it off. Blurs past about 78 points at 2x stop growing.
    ///
    /// Unlike `El::system_material`, which shows what is behind the window, this blurs Rune's
    /// own content and looks the same on every platform. The element's opacity fades the blur
    /// with it, and its outer shadows leave the blurred interior clear. The first
    /// `MAX_BACKDROP_BLURS` blurred elements in a frame blur; any past that, and any drawn into a
    /// target that can't be copied (no `COPY_SRC`, or a caller's own `wgpu::RenderPass` via
    /// `Renderer::draw_range`), draw only their fill. Inside an ancestor drawn as one layer
    /// (`El::group_opacity`, `El::blur`, `El::cache`) the backdrop is what that layer has drawn
    /// so far, as in CSS.
    pub fn backdrop_blur(mut self, sigma: f32) -> Self {
        let s = if sigma.is_finite() { sigma.max(0.0) } else { 0.0 };
        if s != 0.0 || !self.vis.cold.is_empty() {
            self.vis.cold_mut().backdrop_blur = s;
        }
        self
    }

    /// Saturates the blurred backdrop by `factor` before the fill tints it, as CSS
    /// `backdrop-filter: blur() saturate()`: 1 leaves its colours, above 1 strengthens them
    /// (frosted glass usually takes 1.5 to 1.8), 0 makes it grey. Has no effect without
    /// `El::backdrop_blur`.
    pub fn backdrop_saturate(mut self, factor: f32) -> Self {
        let f = if factor.is_finite() { factor.max(0.0) } else { 1.0 };
        // Stored as given; 0 in storage means "unchanged", so a grey backdrop keeps a
        // hair above it.
        let stored = if f == 1.0 { 0.0 } else { f.max(1e-6) };
        if stored != 0.0 || !self.vis.cold.is_empty() {
            self.vis.cold_mut().backdrop_saturate = stored;
        }
        self
    }
}
