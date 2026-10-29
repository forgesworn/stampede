//! Headless helpers: an offscreen colour target and GPU read-back, for tests
//! and screenshots. Write the PNG with any encoder (the showcase example uses
//! the `png` crate).

/// Offscreen output format: sRGB so the read-back bytes are display-ready.
pub const SHOT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct Offscreen {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub size: (u32, u32),
}

impl Offscreen {
    pub fn new(device: &wgpu::Device, w: u32, h: u32) -> Offscreen {
        Offscreen::with_format(device, w, h, SHOT_FORMAT)
    }

    /// Any 4-byte colour format (use a non-sRGB one to test that path).
    pub fn with_format(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat) -> Offscreen {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("race_offscreen"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Offscreen { texture, view, size: (w, h) }
    }

    /// Copy the texture back as tightly packed RGBA8 rows.
    pub fn read_rgba(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Vec<u8>, String> {
        let (w, h) = self.size;
        let bpr = (w * 4).div_ceil(256) * 256;
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("race_readback"),
            size: (bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("race_readback") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        queue.submit(std::iter::once(enc.finish()));
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("map failed: {e}"))?;
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let o = (y * bpr) as usize;
            out.extend_from_slice(&data[o..o + (w * 4) as usize]);
        }
        drop(data);
        staging.unmap();
        Ok(out)
    }
}
