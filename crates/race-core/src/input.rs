//! Per-tick controls (spec 4.1) and their 4-byte wire form.

use serde::{Deserialize, Serialize};

/// One tick of controls for one racer. Buttons are "held" levels; the sim
/// edge-detects against the racer's previous input where it matters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Input {
    /// -1 (left) .. +1 (right).
    pub steer: f32,
    /// Countdown rev (0..1); racing always uses full throttle.
    pub throttle: f32,
    /// Legacy wire bit, ignored by the full-throttle driving model.
    pub brake: bool,
    /// Drift (held).
    pub drift: bool,
    /// Jump (held; edge-detected).
    pub jump: bool,
    /// Laser eyes (held).
    pub laser: bool,
    /// Use item (held; fires on press).
    pub item: bool,
    /// Legacy wire bit; ignored by the simulation.
    pub bash: bool,
    /// Look back (held).
    pub look_back: bool,
}

impl Input {
    /// Full throttle, nothing else.
    pub fn go() -> Self {
        Input {
            throttle: 1.0,
            ..Default::default()
        }
    }

    /// Packs into 4 bytes: steer as i8 (x127), throttle as u8 (x255), button bits, spare 0.
    pub fn pack(&self) -> [u8; 4] {
        let steer = (self.steer.clamp(-1.0, 1.0) * 127.0).round() as i8;
        let throttle = (self.throttle.clamp(0.0, 1.0) * 255.0).round() as u8;
        let mut b = 0u8;
        for (i, on) in [
            self.brake,
            self.drift,
            self.jump,
            self.laser,
            self.item,
            self.bash,
            self.look_back,
        ]
        .into_iter()
        .enumerate()
        {
            if on {
                b |= 1 << i;
            }
        }
        [steer as u8, throttle, b, 0]
    }

    /// Inverse of [`Input::pack`] (steer and throttle are quantised).
    pub fn unpack(p: [u8; 4]) -> Self {
        let bit = |i: u8| p[2] & (1 << i) != 0;
        Input {
            steer: (p[0] as i8) as f32 / 127.0,
            throttle: p[1] as f32 / 255.0,
            brake: bit(0),
            drift: bit(1),
            jump: bit(2),
            laser: bit(3),
            item: bit(4),
            bash: bit(5),
            look_back: bit(6),
        }
    }

    /// The input with all edge-triggered buttons released (used when repeating a
    /// stale input: steer and held levels kept, presses cleared).
    pub fn without_edges(&self) -> Self {
        Input {
            jump: false,
            item: false,
            bash: false,
            ..*self
        }
    }
}
