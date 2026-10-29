//! Paints an egui frame with egui-wgpu on top of the scene (LoadOp::Load).

use std::collections::HashMap;

pub struct EguiPainter {
    renderer: egui_wgpu::Renderer,
    format: wgpu::TextureFormat,
    /// CPU copy of every texture egui has sent (the font atlas above all).
    mirror: TextureMirror,
}

fn new_renderer(device: &wgpu::Device, format: wgpu::TextureFormat) -> egui_wgpu::Renderer {
    egui_wgpu::Renderer::new(
        device,
        format,
        egui_wgpu::RendererOptions { msaa_samples: 1, depth_stencil_format: None, dithering: true, predictable_texture_filtering: false },
    )
}

impl EguiPainter {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> EguiPainter {
        EguiPainter { renderer: new_renderer(device, format), format, mirror: TextureMirror::default() }
    }

    /// The colour format this painter draws into.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// A fresh renderer for `device` / `format` (a new GPU device, or a
    /// surface with another format), with every texture egui has already
    /// sent re-uploaded. egui sends each texture (the font atlas: `Managed(0)`)
    /// once and then only patches it, so a bare new renderer would draw
    /// "Missing texture" forever.
    pub fn rebuild(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) {
        self.renderer = new_renderer(device, format);
        self.format = format;
        for (id, delta) in self.mirror.full_deltas() {
            self.renderer.update_texture(device, queue, id, &delta);
        }
    }

    /// Hand one egui texture delta to the renderer (and the mirror).
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, id: egui::TextureId, delta: &egui::epaint::ImageDelta) {
        self.mirror.apply(id, delta);
        // egui-wgpu panics ("Tried to update a texture that has not been
        // allocated yet") on a patch for a texture it doesn't hold; that was
        // the SIGABRT after a resume in Twang Thang (device test 1, bug 2). Upload
        // the whole mirrored texture instead.
        if delta.pos.is_some() && self.renderer.texture(&id).is_none() {
            match self.mirror.full_delta(id) {
                Some(full) => self.renderer.update_texture(device, queue, id, &full),
                None => log::warn!("egui: dropped a patch for unknown texture {id:?}"),
            }
            return;
        }
        self.renderer.update_texture(device, queue, id, delta);
    }

    /// Tessellate and draw `output` into `view`.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        ctx: &egui::Context,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size_px: (u32, u32),
        ppp: f32,
        output: egui::FullOutput,
    ) {
        let desc = egui_wgpu::ScreenDescriptor { size_in_pixels: [size_px.0, size_px.1], pixels_per_point: ppp };
        let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
        for (id, delta) in &output.textures_delta.set {
            self.upload(device, queue, *id, delta);
        }
        self.renderer.update_buffers(device, queue, encoder, &jobs, &desc);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            let mut pass = pass.forget_lifetime();
            self.renderer.render(&mut pass, &jobs, &desc);
        }
        for id in &output.textures_delta.free {
            self.mirror.free(*id);
            self.renderer.free_texture(id);
        }
    }
}

/// The current contents of every egui texture, kept on the CPU by replaying
/// egui's texture deltas, so a new renderer can be given them whole.
#[derive(Default)]
pub struct TextureMirror {
    textures: HashMap<egui::TextureId, (egui::ColorImage, egui::TextureOptions)>,
}

impl TextureMirror {
    /// Replay one delta: a full image replaces the texture, a patch is
    /// copied into it (a patch for a texture never seen is ignored).
    pub fn apply(&mut self, id: egui::TextureId, delta: &egui::epaint::ImageDelta) {
        let egui::ImageData::Color(img) = &delta.image;
        match delta.pos {
            None => {
                self.textures.insert(id, ((**img).clone(), delta.options));
            }
            Some([x0, y0]) => {
                let Some((dst, opts)) = self.textures.get_mut(&id) else { return };
                *opts = delta.options;
                let [w, h] = img.size;
                let [dw, dh] = dst.size;
                let n = w.min(dw.saturating_sub(x0));
                for y in 0..h.min(dh.saturating_sub(y0)) {
                    let d = (y0 + y) * dw + x0;
                    dst.pixels[d..d + n].copy_from_slice(&img.pixels[y * w..y * w + n]);
                }
            }
        }
    }

    pub fn free(&mut self, id: egui::TextureId) {
        self.textures.remove(&id);
    }

    /// Every texture as a full-image delta.
    pub fn full_deltas(&self) -> Vec<(egui::TextureId, egui::epaint::ImageDelta)> {
        self.textures.keys().filter_map(|id| Some((*id, self.full_delta(*id)?))).collect()
    }

    /// One texture as a full-image delta.
    pub fn full_delta(&self, id: egui::TextureId) -> Option<egui::epaint::ImageDelta> {
        self.textures.get(&id).map(|(img, opts)| egui::epaint::ImageDelta::full(img.clone(), *opts))
    }

    pub fn get(&self, id: egui::TextureId) -> Option<&egui::ColorImage> {
        self.textures.get(&id).map(|(img, _)| img)
    }
}

#[cfg(test)]
mod tests {
    use super::{EguiPainter, TextureMirror};
    use egui::epaint::ImageDelta;
    use egui::{Color32, ColorImage, TextureId, TextureOptions};

    fn img(w: usize, h: usize, c: Color32) -> ColorImage {
        ColorImage::new([w, h], vec![c; w * h])
    }

    /// Device-test-1 bug 3: after a resume the new renderer needs the font
    /// atlas as egui last left it (the full image plus every patch since).
    #[test]
    fn mirror_replays_full_then_patches() {
        let font = TextureId::Managed(0);
        let mut m = TextureMirror::default();
        m.apply(font, &ImageDelta::full(img(4, 3, Color32::BLACK), TextureOptions::LINEAR));
        m.apply(font, &ImageDelta::partial([1, 1], img(2, 2, Color32::WHITE), TextureOptions::LINEAR));
        // A patch hanging off the edge is clipped, not a panic.
        m.apply(font, &ImageDelta::partial([3, 2], img(3, 3, Color32::RED), TextureOptions::LINEAR));
        let got = m.get(font).expect("mirrored");
        let px = |x: usize, y: usize| got.pixels[y * 4 + x];
        assert_eq!(px(0, 0), Color32::BLACK);
        assert_eq!(px(1, 1), Color32::WHITE);
        assert_eq!(px(2, 2), Color32::WHITE);
        assert_eq!(px(3, 1), Color32::BLACK);
        assert_eq!(px(3, 2), Color32::RED);
        let full = m.full_deltas();
        assert_eq!(full.len(), 1);
        assert!(full[0].1.pos.is_none(), "re-upload is a full image");
        assert_eq!(full[0].1.image.size(), [4, 3]);
        // A patch for an unknown texture is ignored; free forgets.
        m.apply(TextureId::Managed(7), &ImageDelta::partial([0, 0], img(1, 1, Color32::RED), TextureOptions::LINEAR));
        assert!(m.get(TextureId::Managed(7)).is_none());
        m.free(font);
        assert!(m.full_deltas().is_empty());
    }

    /// Twang Thang device test 1, bug 2: after returning to the app the renderer was
    /// rebuilt without the font atlas, new glyphs arrived as a patch, and
    /// egui-wgpu panicked (abort on Android). Needs a GPU; skipped without one.
    #[test]
    fn painter_survives_a_patch_after_a_rebuild() {
        use egui::epaint::ImageDelta;
        use egui::{Color32, ColorImage, TextureId, TextureOptions};
        let Ok(gpu) = crate::render::Gpu::headless() else {
            eprintln!("no GPU adapter: skipped");
            return;
        };
        let fmt = wgpu::TextureFormat::Rgba8UnormSrgb;
        let font = TextureId::Managed(0);
        let full = ImageDelta::full(ColorImage::new([8, 8], vec![Color32::BLACK; 64]), TextureOptions::LINEAR);
        let patch = ImageDelta::partial([2, 2], ColorImage::new([2, 2], vec![Color32::WHITE; 4]), TextureOptions::LINEAR);
        let mut p = EguiPainter::new(&gpu.device, fmt);
        p.upload(&gpu.device, &gpu.queue, font, &full);
        p.rebuild(&gpu.device, &gpu.queue, fmt);
        p.upload(&gpu.device, &gpu.queue, font, &patch);
        // A renderer that never saw the texture: the patch is dropped, no panic.
        let mut fresh = EguiPainter::new(&gpu.device, fmt);
        fresh.upload(&gpu.device, &gpu.queue, font, &patch);
        // The bare renderer swap the old resume path did: also no panic now.
        p.renderer = super::new_renderer(&gpu.device, fmt);
        p.upload(&gpu.device, &gpu.queue, font, &patch);
        assert!(p.renderer.texture(&font).is_some(), "re-uploaded whole from the mirror");
    }
}
