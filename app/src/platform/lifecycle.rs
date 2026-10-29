//! One game loop per process (Android).
//!
//! winit allows one `EventLoop` per process, but android-activity calls
//! `android_main` again for every new `StampedeActivity` in a process that is
//! still alive. In 0.2.0 that second call failed with "EventLoop can't be
//! recreated" and closed the activity. Worse, winit ignores `onDestroy`, so the
//! old loop never ended, and android-activity's `onDestroy` waits on the Java
//! UI thread until `android_main` returns: the UI thread hung for good and
//! every later launch stuck on the splash screen.
//!
//! So: the loop ends when its activity is destroyed (`App::about_to_wait`
//! checks `Activity.isDestroyed()`), and the process ends when the loop does
//! (`android_main`), so the next launch starts a fresh process. A second
//! `android_main` while the first loop still runs (a launch with
//! `FLAG_ACTIVITY_CLEAR_TASK`, a recreation the manifest's `configChanges`
//! does not swallow) cannot get a loop either: it ends the process at once,
//! and the next tap starts clean.

use std::sync::atomic::{AtomicBool, Ordering};

static LOOP_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim this process's one game loop. True the first time only.
pub fn claim_event_loop() -> bool {
    claim(&LOOP_CLAIMED)
}

fn claim(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_claim_gets_the_loop() {
        let flag = AtomicBool::new(false);
        assert!(claim(&flag), "the first android_main runs the game");
        assert!(!claim(&flag), "a second android_main in the same process must not build an EventLoop");
        assert!(!claim(&flag));
    }

    #[test]
    fn claims_race_to_exactly_one_winner() {
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let winners: usize = (0..8)
            .map(|_| {
                let f = flag.clone();
                std::thread::spawn(move || claim(&f))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum();
        assert_eq!(winners, 1);
    }
}
