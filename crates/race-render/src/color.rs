//! sRGB <-> linear helpers (the API takes sRGB colours, shaders light in linear).

pub fn srgb_to_linear(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn srgb3_to_linear(c: [f32; 3]) -> [f32; 3] {
    [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2])]
}

/// Drift spark tier colour (sRGB): 1 blue, 2 orange, 3 purple. Tier 0 has none.
pub fn drift_tier_rgb(tier: u8) -> Option<[f32; 3]> {
    match tier {
        0 => None,
        1 => Some([0.25, 0.55, 1.0]),
        2 => Some([1.0, 0.55, 0.12]),
        _ => Some([0.75, 0.25, 1.0]),
    }
}

pub fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

pub fn mul3(a: [f32; 3], k: f32) -> [f32; 3] {
    [a[0] * k, a[1] * k, a[2] * k]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints() {
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
        assert!((srgb_to_linear(0.5) - 0.214).abs() < 0.002);
    }
}
