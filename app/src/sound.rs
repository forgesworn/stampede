//! The app's one `race_audio::Audio`, on the main thread (it is not `Send`).
//! Anyone plays through [`with`]; screenshots and tests get a silent one.
//!
//! ```ignore
//! sound::with(|a| a.play(Sfx::MenuTap, 1.0, 1.0));
//! ```

use std::cell::{Cell, RefCell};

use race_audio::Audio;

thread_local! {
    static AUDIO: RefCell<Option<Audio>> = const { RefCell::new(None) };
    static SUSPENDED: Cell<bool> = const { Cell::new(false) };
}

/// Open the device and synthesise the bank (call once at start-up, after the
/// window exists). `silent` for --shot and tests.
pub fn init(silent: bool) {
    let silent = silent || (crate::game::stage::diag_build() && option_env!("STAMPEDE_QUIET") == Some("1"))
        || crate::dev::var_set("STAMPEDE_QUIET");
    AUDIO.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let audio = if silent { Audio::silent() } else { Audio::new() };
            audio.set_suspended(SUSPENDED.get());
            *slot = Some(audio);
        }
    });
}

/// Run `f` with the audio (a silent instance if `init` was never called).
pub fn with<R>(f: impl FnOnce(&mut Audio) -> R) -> R {
    AUDIO.with(|cell| {
        let mut slot = cell.borrow_mut();
        f(slot.get_or_insert_with(|| {
            let audio = Audio::silent();
            audio.set_suspended(SUSPENDED.get());
            audio
        }))
    })
}

/// Apply lifecycle silence without opening a device before normal audio init.
pub fn set_suspended(suspended: bool) {
    SUSPENDED.set(suspended);
    AUDIO.with(|cell| {
        if let Some(audio) = cell.borrow().as_ref() { audio.set_suspended(suspended); }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_mute_survives_lazy_audio_start_without_opening_a_device_early() {
        std::thread::spawn(|| {
            set_suspended(false); // First foreground event precedes audio init.
            assert!(AUDIO.with(|cell| cell.borrow().is_none()));
            set_suspended(true);
            assert!(AUDIO.with(|cell| cell.borrow().is_none()));
            init(true);
            assert!(with(|a| a.is_suspended()));
            with(|a| a.resume_all());
            assert!(with(|a| a.is_suspended()));
            set_suspended(false);
            assert!(!with(|a| a.is_suspended()));
        }).join().unwrap();
    }
}
