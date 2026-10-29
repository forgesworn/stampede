//! Coordinate conventions and small helpers (spec section 3).
//!
//! Metres, seconds, radians. Right-handed, Y up. Yaw 0 faces +Z.
//! `forward = (sin yaw, 0, cos yaw)`, `right = forward x up = (-cos yaw, 0, sin yaw)`.
//! Positive steer turns right, which decreases yaw.

use glam::Vec3;

/// Simulation rate in ticks per second.
pub const TICK_HZ: u32 = 60;
/// Fixed simulation step in seconds.
pub const DT: f32 = 1.0 / TICK_HZ as f32;

#[cfg(any(target_arch = "wasm32", test))]
mod angle;
#[cfg(any(target_arch = "wasm32", test))]
mod sine;
#[cfg(any(target_arch = "wasm32", test))]
mod tangent;

/// Match the native single-precision sine path in the Wasm simulation.
#[inline]
pub(crate) fn sin(value: f32) -> f32 {
    #[cfg(target_arch = "wasm32")]
    {
        sine::sin(value)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        value.sin()
    }
}

#[inline]
pub(crate) fn cos(value: f32) -> f32 {
    #[cfg(target_arch = "wasm32")]
    {
        sine::cos(value)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        value.cos()
    }
}

#[inline]
pub(crate) fn tan(value: f32) -> f32 {
    #[cfg(target_arch = "wasm32")]
    {
        tangent::tan(value)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        value.tan()
    }
}

#[inline]
pub(crate) fn atan2(y: f32, x: f32) -> f32 {
    #[cfg(target_arch = "wasm32")]
    {
        angle::atan2(y, x)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        y.atan2(x)
    }
}

/// Unit forward vector (horizontal) for a yaw.
#[inline]
pub fn forward(yaw: f32) -> Vec3 {
    Vec3::new(sin(yaw), 0.0, cos(yaw))
}

/// Unit right vector (horizontal) for a yaw: `forward x up`.
#[inline]
pub fn right(yaw: f32) -> Vec3 {
    Vec3::new(-cos(yaw), 0.0, sin(yaw))
}

/// Yaw of a (horizontal) direction; inverse of [`forward`].
#[inline]
pub fn yaw_of(dir: Vec3) -> f32 {
    atan2(dir.x, dir.z)
}

/// Wraps an angle into (-PI, PI].
#[inline]
pub fn wrap_angle(a: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let mut a = a % tau;
    if a > std::f32::consts::PI {
        a -= tau;
    } else if a <= -std::f32::consts::PI {
        a += tau;
    }
    a
}

/// Moves `cur` toward `target` by at most `step`.
#[inline]
pub fn approach(cur: f32, target: f32, step: f32) -> f32 {
    if cur < target {
        (cur + step).min(target)
    } else {
        (cur - step).max(target)
    }
}

/// Linear interpolation.
#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Hermite smoothstep of `x` between `e0` and `e1`.
#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Horizontal (XZ) part of a vector.
#[inline]
pub fn flat(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z)
}

/// A 64-bit mix (splitmix64) used to derive deterministic seeds.
#[inline]
pub fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}
