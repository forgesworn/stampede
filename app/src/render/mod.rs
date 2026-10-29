//! GPU plumbing: the wgpu device, egui painting, the offscreen target for
//! `--shot`, and the one render path both the window and `--shot` use.
//!
//! A frame is: clear to the screen's colour, let the screen draw its own scene
//! ([`Screen::paint_scene`](crate::ui::screens::Screen::paint_scene), a hook
//! for the 3D renderer), then paint egui on top.

pub mod capture;
pub mod gpu;
pub mod painter;
pub mod perf;

pub use gpu::Gpu;
pub use painter::EguiPainter;

use crate::engine::{Engine, FrameOut};

/// What a screen gets to draw its own scene into, between the clear and egui.
/// Render passes should use `LoadOp::Load` on `view` (or `Clear` with their own
/// colour), `size_px` is the full target, `format` its colour format (build
/// pipelines for it; it can change if the surface is recreated).
pub struct ScenePass<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub view: &'a wgpu::TextureView,
    pub size_px: (u32, u32),
    pub format: wgpu::TextureFormat,
    /// Which GPU device `device` is (`Gpu::id`); a cached renderer is only valid for one.
    pub device_id: u64,
    /// UI pixels per point.
    pub ppp: f32,
    /// Engine time, seconds.
    pub time: f32,
}

/// sRGB (0..1) to linear, for clear colours on an sRGB target.
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Encode one frame into `view`: clear, the screen's scene, then egui.
/// `view`'s format must be `format`. Textures egui sent this frame are
/// uploaded by the painter.
#[allow(clippy::too_many_arguments)]
pub fn encode_frame(gpu: &Gpu, painter: &mut EguiPainter, engine: &mut Engine, enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, size_px: (u32, u32), format: wgpu::TextureFormat, out: FrameOut) {
    let c = out.clear;
    let clear = wgpu::Color { r: srgb_to_linear(c[0]) as f64, g: srgb_to_linear(c[1]) as f64, b: srgb_to_linear(c[2]) as f64, a: 1.0 };
    {
        let _pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment { view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store } })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
    }
    let ppp = out.viewport.ppp;
    let mut pass = ScenePass { device: &gpu.device, queue: &gpu.queue, encoder: enc, view, size_px, format, device_id: gpu.id, ppp, time: engine.time as f32 };
    engine.router.top_mut().paint_scene(&mut pass);
    painter.paint(&engine.egui, &gpu.device, &gpu.queue, enc, view, size_px, ppp, out.egui);
}
