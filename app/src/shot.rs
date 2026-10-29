//! Headless screenshot mode: run a real screen on a virtual 60 Hz clock (no
//! window, no input), render the last frame offscreen and write a PNG.
//!
//! `stampede --shot home --out home.png [--size 2400x1080] [--ppp 2.625] [--t 1.5]`
//!
//! The engine, router, egui and the render path are the ones the window uses,
//! so what the PNG shows is what the screen draws. Needs a GPU adapter
//! (Vulkan or GL); software Vulkan (lavapipe) is fine.

use crate::cli::Options;
use crate::engine::Engine;
use crate::haptics::NoHaptics;
use crate::platform::data_dir::Profile;
use crate::platform::os::RecordingPlatform;
use crate::render::capture::{write_png, Offscreen, SHOT_FORMAT};
use crate::render::{self, EguiPainter, Gpu};
use crate::ui::screens;

/// The virtual frame time.
pub const DT: f32 = 1.0 / 60.0;

pub fn run(opts: Options) -> Result<(), String> {
    let name = opts.shot.clone().ok_or("no --shot screen")?;
    let path = opts.out.clone().ok_or("no --out file")?;
    let first = screens::by_name(&name).ok_or_else(|| format!("unknown screen {name:?}; screens: {}", screens::names().join(", ")))?;
    let (w, h) = opts.size;
    let ppp = opts.ppp;
    let frames = ((opts.t / DT).round() as u32).max(1);
    let data_dir = std::env::temp_dir().join(format!("stampede-shot-{}", std::process::id()));
    let mut platform = RecordingPlatform::default();
    if let Some(code) = &opts.join {
        platform.launch.push_back(format!("stampede://join#{code}"));
    }
    let mut profile = Profile::default();
    if matches!(name.as_str(), "settings_tilt" | "race_tilt") {
        profile.steer_mode = crate::platform::data_dir::SteerMode::Tilt;
        platform.tilt = crate::platform::tilt::Reading::Sample { gravity: [0.0, 0.0, 9.81], rotation: 1, generation: 1, age: 0.01 };
    }
    let mut engine = Engine::new(first, profile, data_dir.clone(), opts, Box::new(platform), Box::new(NoHaptics::default()));

    let t0 = std::time::Instant::now();
    let mut textures = egui::TexturesDelta::default();
    let mut last = None;
    for _ in 0..frames {
        let mut out = engine.frame(DT, (w, h), ppp, egui::RawInput::default());
        textures.append(std::mem::take(&mut out.egui.textures_delta));
        last = Some(out);
    }
    let mut out = last.expect("at least one frame");
    out.egui.textures_delta = textures;
    log::info!("simulated {:.2} s in {:.2} s ({frames} frames)", frames as f32 * DT, t0.elapsed().as_secs_f32());

    let gpu = Gpu::headless()?;
    let target = Offscreen::new(&gpu.device, w, h);
    let mut painter = EguiPainter::new(&gpu.device, SHOT_FORMAT);
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("shot") });
    let screen_name = engine.screen_name();
    let ui_ppp = out.viewport.ppp;
    render::encode_frame(&gpu, &mut painter, &mut engine, &mut enc, &target.view, (w, h), SHOT_FORMAT, out);
    gpu.queue.submit(std::iter::once(enc.finish()));
    let rgba = target.read_rgba(&gpu)?;
    write_png(&path, w, h, &rgba)?;
    let _ = std::fs::remove_dir_all(&data_dir);
    println!("wrote {} ({w}x{h}, screen {screen_name}, t={:.2}s, ui scale {ui_ppp:.3})", path.display(), frames as f32 * DT);
    Ok(())
}
