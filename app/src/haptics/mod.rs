//! Haptics (SPEC 12): short one-shot vibrations, best effort.
//!
//! Desktop is a no-op. Android goes through `android::AndroidHaptics`: `jni` +
//! `ndk-context` calls to `Vibrator` / `VibrationEffect.createOneShot`. Any
//! error there disables haptics silently. Use `ctx.haptics.pulse(Pulse::Bash)`.

#[cfg(target_os = "android")]
pub mod android;

/// What happened; each kind maps to a length and a strength.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pulse {
    /// A menu button press (very light).
    Tap,
    /// A bitcoin coin picked up (light).
    Coin,
    /// Landing after a jump.
    Landing,
    /// A boost starts (drift release, pad, mushroom-style item).
    Boost,
    /// You were hit by something.
    Hit,
    /// You bashed another ostrich (strongest).
    Bash,
}

impl Pulse {
    /// Vibration length in milliseconds.
    pub fn millis(self) -> u32 {
        match self {
            Pulse::Tap => 8,
            Pulse::Coin => 10,
            Pulse::Landing => 22,
            Pulse::Boost => 40,
            Pulse::Hit => 70,
            Pulse::Bash => 55,
        }
    }

    /// Strength 1..=255 (Android 8+ with amplitude control; else ignored).
    pub fn amplitude(self) -> u8 {
        match self {
            Pulse::Tap => 60,
            Pulse::Coin => 80,
            Pulse::Landing => 140,
            Pulse::Boost => 170,
            Pulse::Hit => 230,
            Pulse::Bash => 255,
        }
    }
}

pub trait Haptics {
    /// Fire a one-shot vibration of `ms` at `amplitude`. Never blocks, never panics.
    fn vibrate(&mut self, ms: u32, amplitude: u8);
    fn set_enabled(&mut self, on: bool);
    fn enabled(&self) -> bool;

    /// Fire the vibration for `p` (nothing when haptics are off).
    fn pulse(&mut self, p: Pulse) {
        if HAPTICS_ALLOWED && self.enabled() {
            self.vibrate(p.millis(), p.amplitude());
        }
    }
}

/// Desktop: does nothing (records the last pulse for tests/debug).
#[derive(Debug, Default)]
pub struct NoHaptics {
    pub on: bool,
    pub last_ms: Option<u32>,
    pub count: u32,
}

impl Haptics for NoHaptics {
    fn vibrate(&mut self, ms: u32, _amplitude: u8) {
        self.last_ms = Some(ms);
        self.count += 1;
    }
    fn set_enabled(&mut self, on: bool) {
        self.on = on;
    }
    fn enabled(&self) -> bool {
        self.on
    }
}

/// The platform's haptics.
/// Master switch: haptics are disabled in this build.
/// Flip to true to bring haptics back (the Settings toggle then applies).
pub const HAPTICS_ALLOWED: bool = false;

pub fn platform(enabled: bool) -> Box<dyn Haptics> {
    #[cfg(target_os = "android")]
    {
        let mut h = android::AndroidHaptics::new();
        h.set_enabled(enabled);
        Box::new(h)
    }
    #[cfg(not(target_os = "android"))]
    {
        Box::new(NoHaptics { on: enabled, ..NoHaptics::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_silent_and_bash_is_the_strongest() {
        let mut h = NoHaptics { on: true, ..Default::default() };
        h.pulse(Pulse::Bash);
        let fired = if HAPTICS_ALLOWED { 1 } else { 0 };
        assert_eq!(h.last_ms, HAPTICS_ALLOWED.then_some(55));
        h.set_enabled(false);
        h.pulse(Pulse::Coin);
        assert_eq!(h.count, fired);
        assert!([Pulse::Tap, Pulse::Coin, Pulse::Landing, Pulse::Boost, Pulse::Hit].iter().all(|p| p.amplitude() <= Pulse::Bash.amplitude()));
    }
}
