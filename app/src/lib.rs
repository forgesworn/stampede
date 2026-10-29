//! Stampede: the app shell (desktop Linux + Android).
//!
//! Module map:
//! * [`platform`] - lifecycle, insets/viewport/UI scale, raw touch + keys, data
//!   dir and profile, OS services ([`platform::Platform`]), Android JNI glue.
//! * [`haptics`]  - [`haptics::Haptics`] and [`haptics::Pulse`]; Android vibrator over JNI.
//! * [`render`]   - wgpu device, egui painter, offscreen capture, the frame path, frame-time stats.
//! * [`ui`]       - egui theme and the screens (`ui::screens`: `Screen`, `Transition`, `Router`).
//! * [`engine`]   - the platform-independent frame: input, router, egui, services.
//! * [`app`]      - the winit window, surface and event loop; [`shot`] - headless screenshots.
//! * [`cli`]      - desktop command line.

pub mod app;
pub mod cli;
pub mod custom_tracks;
pub mod dev;
pub mod engine;
pub mod flow;
pub mod game;
pub mod games;
pub mod haptics;
pub mod keeps;
pub mod nostr_fetch;
pub mod online;
pub mod platform;
pub mod render;
pub mod shot;
pub mod sound;
pub mod ui;
pub mod update;

use cli::Options;
use engine::Engine;
use platform::data_dir::Profile;
use std::path::PathBuf;
use ui::screens::{self, Screen};

/// "v0.1.0 . ff5ed5f" - the version and the git commit it was built from.
pub fn version_label() -> String {
    format!("v{} \u{b7} {}", env!("CARGO_PKG_VERSION"), env!("STAMPEDE_BUILD"))
}

/// The first screen for these options: `--screen NAME`, else the splash.
fn first_screen(opts: &Options) -> Box<dyn Screen> {
    if let Some(name) = &opts.screen {
        match screens::by_name(name) {
            Some(s) => return s,
            None => log::warn!("unknown screen {name:?}; screens: {}", screens::names().join(", ")),
        }
    }
    Box::new(screens::splash::SplashScreen::new())
}

fn build_engine(opts: Options, data_dir: PathBuf) -> Engine {
    let profile = Profile::load(&data_dir);
    let haptics = haptics::platform(profile.haptics);
    let platform = platform::os::platform(opts.join.as_deref().map(|c| format!("stampede://join#{c}")));
    game::stage::diag_init(&data_dir);
    let mut opts = opts;
    if opts.screen.is_none() {
        opts.screen = game::stage::diag_screen(); // DIAG builds only: `screen=NAME` in diag.txt
    }
    let first = first_screen(&opts);
    // One Audio for the app, on the main thread (idempotent; menus may call it too).
    sound::init(false);
    // The player's relays for every nostr use (room codes; update checks read them from the engine).
    platform::set_relays(profile.relays.clone());
    let mut engine = Engine::new(first, profile, data_dir, opts, platform, haptics);
    // A real run may check for updates (screenshots and tests never do). Without a pinned release key nothing is checked.
    engine.svc.updates.enabled = true;
    engine
}

/// Desktop entry point. Returns the process exit code.
#[cfg(not(target_os = "android"))]
pub fn run_desktop() -> i32 {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,wgpu_core=warn,wgpu_hal=warn,naga=warn,egui_wgpu=error")).init();
    install_panic_hook();
    let opts = match Options::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            // --help is not an error.
            let help = e.starts_with("Stampede (desktop)");
            if help {
                println!("{e}");
            } else {
                eprintln!("{e}");
            }
            return if help { 0 } else { 2 };
        }
    };
    dev::init(opts.dev);
    if opts.shot.is_some() {
        return match shot::run(opts) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("screenshot failed: {e}");
                1
            }
        };
    }
    let data_dir = opts.data_dir.clone().unwrap_or_else(platform::data_dir::default_dir);
    let quit_after = opts.quit_after;
    let engine = build_engine(opts, data_dir);
    let event_loop = match winit::event_loop::EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("no display: {e}");
            return 1;
        }
    };
    let mut app = app::App::new(engine);
    app.quit_after = quit_after;
    match app::run(event_loop, app) {
        Ok(app) => {
            if let Some(f) = app.stats.average_fps() {
                eprintln!("average fps: {f:.1}");
            }
            0
        }
        Err(e) => {
            eprintln!("event loop error: {e}");
            1
        }
    }
}

/// Route every panic (message, location, backtrace) through `log::error!`.
/// On Android stderr goes nowhere and release builds abort on panic, so
/// without this a crash leaves only "Fatal signal 6" in logcat. Installed
/// once per process; the previous hook still runs afterwards (desktop stderr).
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            log::error!("{}", panic_report(info, &std::backtrace::Backtrace::force_capture()));
            prev(info);
        }));
    });
}

/// The text the panic hook logs: thread, message, location, backtrace.
pub fn panic_report(info: &std::panic::PanicHookInfo, bt: &std::backtrace::Backtrace) -> String {
    let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_string()
    };
    let at = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_else(|| "unknown location".into());
    let thread = std::thread::current();
    format!("PANIC on thread '{}' at {at}: {msg}\nbacktrace:\n{bt}", thread.name().unwrap_or("<unnamed>"))
}

/// Android entry point (NativeActivity loads the cdylib and calls this).
///
/// Runs once per process: the game loop ends when its activity is destroyed,
/// and then the process ends, so the next launch gets a fresh one (winit
/// cannot build a second `EventLoop` in a process). See
/// [`platform::lifecycle`].
#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(android: android_activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("stampede"));
    install_panic_hook();
    if !platform::lifecycle::claim_event_loop() {
        // A new activity while this process's loop still runs (or ran). It
        // can't have a loop, and the old one must not be handed its activity:
        // end the process, the next launch starts clean.
        log::warn!("lifecycle: android_main again in a process that already ran the game; ending the process");
        std::process::exit(0);
    }
    platform::android::init(&android);
    dev::init(false);
    let data_dir = android.internal_data_path().unwrap_or_else(|| PathBuf::from("."));
    let engine = build_engine(Options::default(), data_dir);
    log::info!("stampede {} starting", version_label());
    match winit::event_loop::EventLoop::builder().with_android_app(android).build() {
        Ok(event_loop) => match app::run(event_loop, app::App::new(engine)) {
            // Dropped here, before the exit: the profile was saved in `App::exiting`.
            Ok(app) => drop(app),
            Err(e) => log::error!("event loop error: {e}"),
        },
        Err(e) => log::error!("event loop: {e}"),
    }
    log::info!("lifecycle: game loop ended; ending the process so the next launch starts fresh");
    std::process::exit(0);
}

pub mod account;
