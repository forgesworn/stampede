//! The windowed app: winit 0.30 `ApplicationHandler` (desktop + Android),
//! wgpu surface, input routing, the frame loop.
//!
//! Boot pattern after Axe'n'Stax (MIT): the window + surface are created in
//! `resumed` and dropped in `suspended` (Android destroys the surface when
//! backgrounded); the GPU device and egui's textures live on across that. The
//! event loop polls and redraws continuously; the surface is Fifo (vsync) so
//! `present` paces it.
//!
//! One `EventLoop` per process: winit cannot build a second one, so on Android
//! the loop ends when its activity is destroyed and the process ends with it
//! (see `platform::lifecycle`).

use crate::engine::Engine;
use crate::platform::TouchPhase;
use crate::render::perf::FrameStats;
use crate::render::{self, gpu, EguiPainter, Gpu};
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

/// The GPU device and everything uploaded to it. Kept across Android's
/// surface loss (pause/resume): only the window surface is rebuilt, so what
/// egui uploaded once (the font atlas, `Managed(0)`) stays valid.
struct Device {
    gpu: Gpu,
    painter: EguiPainter,
}

/// The window and its surface: dropped in `suspended`, rebuilt in `resumed`.
struct Gfx {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    egui_state: egui_winit::State,
}

pub struct App {
    engine: Engine,
    device: Option<Device>,
    gfx: Option<Gfx>,
    last: Option<Instant>,
    pub stats: FrameStats,
    /// Quit after this many seconds (smoke tests); None = run until closed.
    pub quit_after: Option<f32>,
    started: Instant,
    /// The window has input focus. On Android nothing is drawn without it
    /// (activity paused under a dialog, the shade or another app).
    focused: bool,
}

impl App {
    pub fn new(engine: Engine) -> App {
        App { engine, device: None, gfx: None, last: None, stats: FrameStats::default(), quit_after: None, started: Instant::now(), focused: true }
    }

    fn create_gfx(&mut self, el: &ActiveEventLoop) -> Result<Gfx, String> {
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes().with_title("Stampede");
        #[cfg(not(target_os = "android"))]
        {
            let (w, h) = crate::platform::DESKTOP_SIZE;
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(w as f64, h as f64)).with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 300.0));
        }
        // Linux desktop: Wayland app_id / X11 WM_CLASS, so the dock and task
        // switcher match the installed `org.decented.stampede.desktop` (and its icon).
        #[cfg(target_os = "linux")]
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            attrs = WindowAttributesExtWayland::with_name(attrs, "org.decented.stampede", "stampede");
            attrs = WindowAttributesExtX11::with_name(attrs, "org.decented.stampede", "stampede");
        }
        let window = Arc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        // Keep the device from before a pause if it can draw to the new surface.
        let mut kept = None;
        if let Some(d) = self.device.take() {
            match d.gpu.instance.create_surface(window.clone()) {
                Ok(surface) if d.gpu.adapter.is_surface_supported(&surface) => kept = Some((d, surface)),
                _ => {
                    log::warn!("graphics: the old device can't draw to the new window; opening a new one");
                    self.device = Some(d);
                }
            }
        }
        let (mut device, surface) = match kept {
            Some(k) => {
                log::info!("graphics: device kept, new surface");
                k
            }
            None => {
                let instance = gpu::instance();
                let surface = instance.create_surface(window.clone()).map_err(|e| e.to_string())?;
                let gpu = pollster::block_on(Gpu::new(instance, Some(&surface)))?;
                let format = gpu::pick_surface_format(&surface.get_capabilities(&gpu.adapter).formats);
                let painter = match self.device.take() {
                    // A replacement device: hand it everything egui sent to the old one.
                    Some(mut old) => {
                        old.painter.rebuild(&gpu.device, &gpu.queue, format);
                        old.painter
                    }
                    None => EguiPainter::new(&gpu.device, format),
                };
                (Device { painter, gpu }, surface)
            }
        };
        let caps = surface.get_capabilities(&device.gpu.adapter);
        let format = gpu::pick_surface_format(&caps.formats);
        if format != device.painter.format() {
            // Same device, a surface in another format: pipelines are per format.
            device.painter.rebuild(&device.gpu.device, &device.gpu.queue, format);
        }
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: present_mode(&caps.present_modes),
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device.gpu.device, &config);
        self.engine.set_surface((config.width, config.height), window.scale_factor() as f32);
        let egui_state = egui_winit::State::new(self.engine.egui.clone(), egui::ViewportId::ROOT, &window, None, None, None);
        log::info!("surface {:?} {}x{} {:?} scale {:.3} ui-scale {:.3}", format, config.width, config.height, config.present_mode, window.scale_factor(), self.engine.ppp());
        self.device = Some(device);
        Ok(Gfx { window, surface, config, egui_state })
    }

    fn redraw(&mut self, el: &ActiveEventLoop) {
        let (Some(g), Some(d)) = (self.gfx.as_mut(), self.device.as_mut()) else { return };
        let now = Instant::now();
        let dt = self.last.map(|t| now.duration_since(t).as_secs_f32()).unwrap_or(1.0 / 60.0).min(0.1);
        self.last = Some(now);
        let scale = g.window.scale_factor() as f32;
        let size = g.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        #[cfg(target_os = "android")]
        {
            self.engine.insets = crate::platform::android::insets(self.engine.ppp());
        }
        let raw = g.egui_state.take_egui_input(&g.window);
        let mut out = self.engine.frame(dt, (size.width, size.height), scale, raw);
        let platform = std::mem::take(&mut out.egui.platform_output);
        g.egui_state.handle_platform_output(&g.window, platform);
        if out.quit {
            el.exit();
            return;
        }
        let frame = match g.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            // The surface was lost or no longer matches the window: rebuild it and skip this frame.
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                g.config.width = size.width;
                g.config.height = size.height;
                g.surface.configure(&d.gpu.device, &g.config);
                return;
            }
            other => {
                log::debug!("skipping frame: {other:?}");
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let px = (g.config.width, g.config.height);
        let mut enc = d.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        render::encode_frame(&d.gpu, &mut d.painter, &mut self.engine, &mut enc, &view, px, g.config.format, out);
        d.gpu.queue.submit(std::iter::once(enc.finish()));
        frame.present();
        self.stats.frame(Instant::now(), now.elapsed().as_secs_f32() * 1000.0);
        if self.quit_after.is_some_and(|q| self.started.elapsed().as_secs_f32() >= q) {
            el.exit();
        }
    }
}

impl App {
    /// How the event loop waits while nothing is drawn: until an event, or, with
    /// an online session alive, a short while (the session needs ticking).
    fn idle_flow(&self) -> ControlFlow {
        if self.engine.wants_background_tick() {
            ControlFlow::WaitUntil(Instant::now() + BACKGROUND_TICK)
        } else {
            ControlFlow::Wait
        }
    }
}

/// How often an online session is driven while the app has no window (~30 Hz).
const BACKGROUND_TICK: std::time::Duration = std::time::Duration::from_millis(33);

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.focused { self.engine.on_foreground(); }
        #[cfg(target_os = "android")]
        {
            // Immersive mode, insets, a link we were brought forward with; frames back on.
            crate::platform::android::on_resume();
            el.set_control_flow(ControlFlow::Poll);
        }
        self.stats.restart();
        self.last = None;
        if self.gfx.is_some() {
            return;
        }
        match self.create_gfx(el) {
            Ok(g) => {
                g.window.request_redraw();
                self.gfx = Some(g);
            }
            Err(e) => {
                log::error!("could not start graphics: {e}");
                eprintln!("Stampede could not start graphics: {e}");
                el.exit();
            }
        }
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.engine.on_exit();
    }

    fn suspended(&mut self, _el: &ActiveEventLoop) {
        self.focused = false;
        self.engine.on_background();
        // Android destroys the native window: drop the surface (it must go
        // before the window does) but keep the device and its textures.
        self.gfx = None;
        #[cfg(target_os = "android")]
        {
            // Backgrounded: sleep instead of spinning the poll loop (but keep
            // ticking an online session, see `idle_flow`).
            _el.set_control_flow(self.idle_flow());
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::Focused(true) => {
                self.focused = true;
                self.engine.on_foreground();
                self.stats.restart();
                self.last = None;
            }
            WindowEvent::Focused(false) => {
                self.focused = false;
                self.engine.on_background();
            }
            _ => {}
        }
        let (Some(g), Some(d)) = (self.gfx.as_mut(), self.device.as_ref()) else { return };
        // egui gets keyboard, IME and focus events from egui-winit. Pointer
        // and touch go through the engine instead (UI-scale aware, raw
        // multi-touch kept for the screens), so egui-winit never sees them.
        if !matches!(event, WindowEvent::Touch(_) | WindowEvent::CursorMoved { .. } | WindowEvent::CursorLeft { .. } | WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. }) {
            let _ = g.egui_state.on_window_event(&g.window, &event);
        }
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                self.engine.clear_touches();
                #[cfg(target_os = "android")]
                crate::platform::android::on_resize();
                g.config.width = size.width.max(1);
                g.config.height = size.height.max(1);
                g.surface.configure(&d.gpu.device, &g.config);
                self.engine.set_surface((g.config.width, g.config.height), g.window.scale_factor() as f32);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = g.window.inner_size();
                self.engine.set_surface((size.width.max(1), size.height.max(1)), scale_factor as f32);
            }
            WindowEvent::CursorMoved { position, .. } => self.engine.cursor_moved((position.x, position.y)),
            WindowEvent::CursorLeft { .. } => self.engine.cursor_left(),
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                match button {
                    MouseButton::Left => self.engine.mouse_button(true, pressed),
                    MouseButton::Right => self.engine.mouse_button(false, pressed),
                    _ => {}
                }
            }
            WindowEvent::Touch(t) => {
                let phase = match t.phase {
                    winit::event::TouchPhase::Started => TouchPhase::Started,
                    winit::event::TouchPhase::Moved => TouchPhase::Moved,
                    winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled => TouchPhase::Ended,
                };
                self.engine.touch_event(t.id, phase, (t.location.x, t.location.y));
            }
            // Android's Back key arrives as BrowserBack; Esc is Back on desktop.
            WindowEvent::KeyboardInput { event: KeyEvent { logical_key: Key::Named(NamedKey::BrowserBack | NamedKey::Escape), state: ElementState::Pressed, repeat: false, .. }, .. } => {
                self.engine.request_back();
            }
            WindowEvent::KeyboardInput { event: KeyEvent { physical_key: PhysicalKey::Code(code), state, repeat: false, .. }, .. } => {
                self.engine.key_event(code, state == ElementState::Pressed);
            }
            WindowEvent::RedrawRequested => self.redraw(el),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // winit drops Android's onDestroy, and android-activity's onDestroy
        // blocks the Java UI thread until `android_main` returns: end the
        // loop as soon as the activity is destroyed (checked every iteration,
        // before the early returns: by then there is no window and no focus).
        #[cfg(target_os = "android")]
        if crate::platform::android::activity_destroyed() {
            log::info!("lifecycle: activity destroyed; ending the game loop");
            el.exit();
            return;
        }
        // Android: without a surface or focus the activity is paused (or
        // covered); draw nothing and sleep until an event, as when backgrounded.
        // An online game goes on being driven from here (a bot steers), so the
        // host does not drop us for silence while we are away.
        if cfg!(target_os = "android") && (self.gfx.is_none() || !self.focused) {
            if self.engine.wants_background_tick() {
                self.engine.background_tick();
            }
            el.set_control_flow(self.idle_flow());
            return;
        }
        let Some(g) = &self.gfx else { return };
        el.set_control_flow(ControlFlow::Poll);
        g.window.request_redraw();
    }
}

/// Fifo (vsync) unless `STAMPEDE_NO_VSYNC=1` asks for Immediate (desktop
/// benchmarking) and the surface has it.
fn present_mode(modes: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    if std::env::var_os("STAMPEDE_NO_VSYNC").is_some() && modes.contains(&wgpu::PresentMode::Immediate) {
        return wgpu::PresentMode::Immediate;
    }
    if modes.contains(&wgpu::PresentMode::Fifo) {
        wgpu::PresentMode::Fifo
    } else {
        modes[0]
    }
}

/// Run the event loop until the window closes.
pub fn run(event_loop: EventLoop<()>, mut app: App) -> Result<App, String> {
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    Ok(app)
}

