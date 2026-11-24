//! A frame frosts every lens a UI's floating surfaces need, not the first few.

use rune::render::Gpu;
use rune_fx::*;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const SIDE: u32 = 512;

fn backdrop(gpu: &Gpu) -> wgpu::TextureView {
    gpu.device
        .create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

#[test]
fn twelve_frosted_lenses_all_blur_their_backdrop() {
    let gpu = Gpu::headless();
    let mut fx = FxRenderer::new(&gpu.device, FORMAT, FxOptions::default());
    let bd = backdrop(&gpu);
    // A pyramid of six levels at this size: twelve lenses take 72 passes.
    let layers: Vec<FxLayer> = (0..12)
        .map(|n| {
            let (x, y) = ((n % 4) as f32 * 120.0 + 8.0, (n / 4) as f32 * 160.0 + 8.0);
            Fx::lens(LensParams { frost: 1.0, ..Default::default() }).on(Shape::rounded(x, y, 100.0, 40.0, 14.0)).at(0.5)
        })
        .collect();
    fx.prepare(&gpu.device, &gpu.queue, &FxFrame { viewport: [SIDE, SIDE], scale: 1.0, layers: &layers, backdrop: Some(&bd), sources: &[] });
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    let blurred: Vec<bool> = (0..layers.len()).map(|k| fx.blur_backdrop(&gpu.device, &gpu.queue, &mut enc, &bd, k)).collect();
    gpu.queue.submit([enc.finish()]);
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    assert!(blurred.iter().all(|b| *b), "lenses left without the pyramid: {blurred:?}");
}
