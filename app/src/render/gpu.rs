//! wgpu device setup for a window surface or for headless (offscreen) use.

/// The GPU handles shared by the renderer and egui.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// Unique per device created in this process (see `stage`: a renderer belongs to one device).
    pub id: u64,
}

/// A fresh wgpu instance with every native backend this build has.
pub fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

impl Gpu {
    /// Pick an adapter (compatible with `surface` if given) and open a device.
    /// Requests downlevel limits raised to the adapter's resolution, as
    /// Axe'n'Stax does, so GLES-class phones still get a device.
    pub async fn new(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Gpu, String> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: surface,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| format!("no compatible graphics adapter: {e}"))?;
        log::info!("GPU adapter: {} ({:?})", adapter.get_info().name, adapter.get_info().backend);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("stampede_device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("could not open the graphics device: {e}"))?;
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Ok(Gpu { instance, adapter, device, queue, id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) })
    }

    /// Headless device (screenshots, tests on a GPU box).
    pub fn headless() -> Result<Gpu, String> {
        pollster::block_on(Gpu::new(instance(), None))
    }
}

/// Choose the surface's format: prefer an sRGB one (the composite writes linear).
pub fn pick_surface_format(formats: &[wgpu::TextureFormat]) -> wgpu::TextureFormat {
    formats.iter().copied().find(|f| f.is_srgb()).or_else(|| formats.first().copied()).unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb)
}
