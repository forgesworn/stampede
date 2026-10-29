//! Developer hooks (autopilot, shortened test races, camera and popup overrides,
//! the host-lobby shortcut). They never ship active: a release build ignores them.
//! They are on in debug builds (`cargo run`, `cargo test`), in DIAG builds
//! (`DIAG=1 tools/build-apk.sh`, which also reads `diag.txt`; see `game::stage`), and on
//! the desktop with `--dev`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static CLI: AtomicBool = AtomicBool::new(false);
static AUTOPILOT: OnceLock<bool> = OnceLock::new();

/// Called once at start-up: `--dev` on the command line, and the autopilot switch is read here (once).
pub fn init(cli_flag: bool) {
    CLI.store(cli_flag, Ordering::Relaxed);
    let _ = autopilot();
}

/// Are the developer hooks enabled in this build / run?
pub fn on() -> bool {
    cfg!(debug_assertions) || crate::game::stage::diag_build() || CLI.load(Ordering::Relaxed)
}

/// The value of environment variable `name`, only when developer hooks are on.
pub fn var(name: &str) -> Option<String> {
    if on() {
        std::env::var(name).ok()
    } else {
        None
    }
}

/// Is `name` set at all, only when developer hooks are on.
pub fn var_set(name: &str) -> bool {
    on() && std::env::var_os(name).is_some()
}

/// Should the bot AI drive the player (`STAMPEDE_AUTOPILOT`, or on Android
/// `adb shell setprop debug.stampede.autopilot 1` before the app starts)? Read once.
pub fn autopilot() -> bool {
    *AUTOPILOT.get_or_init(|| {
        if !on() {
            return false;
        }
        if std::env::var_os("STAMPEDE_AUTOPILOT").is_some() {
            return true;
        }
        #[cfg(target_os = "android")]
        {
            if let Ok(o) = std::process::Command::new("getprop").arg("debug.stampede.autopilot").output() {
                return String::from_utf8_lossy(&o.stdout).trim() == "1";
            }
        }
        false
    })
}

/// Should the host's lobby offer "Protect this room"? The current Protect flow
/// is clunky and will be replaced by automatic protection, so players never
/// see the button. For testing it is on with developer hooks enabled and
/// either `STAMPEDE_PROTECT=1` in the environment or `protect=1` in
/// `diag.txt` (DIAG builds, re-read once a second). Off by default.
pub fn show_protect() -> bool {
    #[cfg(test)]
    if let Some(v) = TEST_SHOW_PROTECT.with(|c| c.get()) {
        return v;
    }
    if !on() {
        return false;
    }
    if var("STAMPEDE_PROTECT").as_deref() == Some("1") {
        return true;
    }
    crate::game::stage::diag_build() && diag_protect()
}

/// `protect=1` in `diag.txt`, looked up at most once a second.
fn diag_protect() -> bool {
    thread_local! {
        static POLL: std::cell::Cell<Option<(std::time::Instant, bool)>> = const { std::cell::Cell::new(None) };
    }
    POLL.with(|c| match c.get() {
        Some((at, v)) if at.elapsed().as_secs_f32() <= 1.0 => v,
        _ => {
            let v = crate::game::stage::diag_text().split_whitespace().any(|w| w == "protect=1");
            c.set(Some((std::time::Instant::now(), v)));
            v
        }
    })
}

#[cfg(test)]
thread_local! {
    static TEST_SHOW_PROTECT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Tests: force [`show_protect`] on this thread until the guard drops.
#[cfg(test)]
pub struct ShowProtectGuard;

#[cfg(test)]
impl Drop for ShowProtectGuard {
    fn drop(&mut self) {
        TEST_SHOW_PROTECT.with(|c| c.set(None));
    }
}

#[cfg(test)]
pub fn force_show_protect(on: bool) -> ShowProtectGuard {
    TEST_SHOW_PROTECT.with(|c| c.set(Some(on)));
    ShowProtectGuard
}
