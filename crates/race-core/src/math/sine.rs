// Copyright (c) 2018-2024, Arm Limited.
// SPDX-License-Identifier: MIT
// Rust adaptation of Arm Optimized Routines v26.01 math/sinf.c,
// sincosf.h and sincosf_data.c, under their MIT licence option.
// https://github.com/ARM-software/optimized-routines/tree/v26.01/math
// See ../../ARM-MATH-LICENSE. Wasm has no floating-point exception flags.

const INV_HALF_PI: f64 = f64::from_bits(0x41645f306dc9c883);
const HALF_PI: f64 = f64::from_bits(0x3ff921fb54442d18);
const PI_63: f64 = f64::from_bits(0x3c1921fb54442d18);
const COS: [f64; 5] = [
    1.0,
    f64::from_bits(0xbfdffffffd0c621c),
    f64::from_bits(0x3fa55553e1068f19),
    f64::from_bits(0xbf56c087e89a359d),
    f64::from_bits(0x3ef99343027bf8c3),
];
const SIN: [f64; 3] = [
    f64::from_bits(0xbfc555545995a603),
    f64::from_bits(0x3f81107605230bc4),
    f64::from_bits(0xbf2994eb3774cf24),
];
const INV_PI_4: [u32; 24] = [
    0xa2, 0xa2f9, 0xa2f983, 0xa2f9836e, 0xf9836e4e, 0x836e4e44, 0x6e4e4415, 0x4e441529, 0x441529fc,
    0x1529fc27, 0x29fc2757, 0xfc2757d1, 0x2757d1f5, 0x57d1f534, 0xd1f534dd, 0xf534ddc0, 0x34ddc0db,
    0xddc0db62, 0xc0db6295, 0xdb629599, 0x6295993c, 0x95993c43, 0x993c4390, 0x3c439041,
];

fn polynomial(x: f64, x2: f64, quadrant: u32, negative_cosine: bool) -> f32 {
    if quadrant & 1 == 0 {
        let x3 = x * x2;
        let s1 = SIN[1] + x2 * SIN[2];
        let x7 = x3 * x2;
        let s = x + x3 * SIN[0];
        (s + x7 * s1) as f32
    } else {
        let sign = if negative_cosine { -1.0 } else { 1.0 };
        let x4 = x2 * x2;
        let c2 = sign * COS[3] + x2 * (sign * COS[4]);
        let c1 = sign * COS[0] + x2 * (sign * COS[1]);
        let x6 = x4 * x2;
        let c = c1 + x4 * (sign * COS[2]);
        (c + x6 * c2) as f32
    }
}

fn reduce_large(bits: u32) -> (f64, u32) {
    let index = ((bits >> 26) & 15) as usize;
    let shift = (bits >> 23) & 7;
    let xi = u64::from(((bits & 0xffffff) | 0x800000) << shift);
    let res0 = xi * u64::from(INV_PI_4[index]);
    let res1 = xi * u64::from(INV_PI_4[index + 4]);
    let res2 = xi * u64::from(INV_PI_4[index + 8]);
    let res0 = ((res2 >> 32) | (res0 << 32)).wrapping_add(res1);
    let quadrant = res0.wrapping_add(1 << 61) >> 62;
    let remainder = res0.wrapping_sub(quadrant << 62);
    ((remainder as i64 as f64) * PI_63, quadrant as u32)
}

/// Same high/low reduction used by native single-precision tangent.
pub(super) fn reduce_tangent(value: f32) -> (f32, f32, u32) {
    let x = f64::from(value);
    let (remainder, quadrant) = if value.abs() < 120.0 {
        let n = (((x * INV_HALF_PI) as i32) + 0x800000) >> 24;
        (x - f64::from(n) * HALF_PI, n as u32)
    } else {
        let (remainder, quadrant) = reduce_large(value.to_bits());
        (
            if value.is_sign_negative() {
                -remainder
            } else {
                remainder
            },
            quadrant,
        )
    };
    let high = remainder as f32;
    (high, (remainder - f64::from(high)) as f32, quadrant)
}

pub(super) fn sin(value: f32) -> f32 {
    let bits = value.to_bits();
    let top = (bits >> 20) & 0x7ff;
    let x = f64::from(value);
    if top < ((std::f32::consts::FRAC_PI_4.to_bits() >> 20) & 0x7ff) {
        if top < ((0x39800000u32 >> 20) & 0x7ff) {
            return value;
        }
        return polynomial(x, x * x, 0, false);
    }
    let (x, quadrant, sign) = if top < ((120.0f32.to_bits() >> 20) & 0x7ff) {
        let scaled = (x * INV_HALF_PI) as i32;
        let n = (scaled + 0x800000) >> 24;
        (x - f64::from(n) * HALF_PI, n as u32, 0)
    } else if value.is_finite() {
        let (x, n) = reduce_large(bits);
        (x, n, bits >> 31)
    } else {
        return f32::NAN;
    };
    let signs = [1.0, -1.0, -1.0, 1.0];
    let signed_quadrant = quadrant.wrapping_add(sign);
    polynomial(
        x * signs[(signed_quadrant & 3) as usize],
        x * x,
        quadrant,
        signed_quadrant & 2 != 0,
    )
}

pub(super) fn cos(value: f32) -> f32 {
    let bits = value.to_bits();
    let top = (bits >> 20) & 0x7ff;
    let x = f64::from(value);
    if top < ((std::f32::consts::FRAC_PI_4.to_bits() >> 20) & 0x7ff) {
        if top < ((0x39800000u32 >> 20) & 0x7ff) {
            return 1.0;
        }
        return polynomial(x, x * x, 1, false);
    }
    let (x, quadrant, sign) = if top < ((120.0f32.to_bits() >> 20) & 0x7ff) {
        let scaled = (x * INV_HALF_PI) as i32;
        let n = (scaled + 0x800000) >> 24;
        (x - f64::from(n) * HALF_PI, n as u32, 0)
    } else if value.is_finite() {
        let (x, n) = reduce_large(bits);
        (x, n, bits >> 31)
    } else {
        return f32::NAN;
    };
    let signs = [1.0, -1.0, -1.0, 1.0];
    let signed = quadrant.wrapping_add(sign);
    polynomial(
        x * signs[(signed & 3) as usize],
        x * x,
        quadrant ^ 1,
        signed & 2 != 0,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn captured_native_cosines_cover_full_float_range() {
        for (input, expected) in [
            (0x00000000, 0x3f800000),
            (0x80000000, 0x3f800000),
            (0x3dcccccd, 0x3f7eb898),
            (0x3f800000, 0x3f0a5140),
            (0x3fc90fdb, 0xb33bbd2e),
            (0x40490fdb, 0xbf800000),
            (0x4111b193, 0xbf731738),
            (0x42f00000, 0x3f506e2a),
            (0x49742400, 0x3f6fcefd),
            (0x60ad78ec, 0x3f411723),
            (0x7f7fffff, 0x3f5a5f96),
        ] {
            assert_eq!(
                super::cos(f32::from_bits(input)).to_bits(),
                expected,
                "{input:08x}"
            );
        }
    }
    #[test]
    fn captured_native_vectors_cover_reduction_boundaries_and_frog_height() {
        for (input, expected) in [
            (0x00000000, 0x00000000),
            (0x80000000, 0x80000000),
            (0x3dcccccd, 0x3dcc7577),
            (0x3f800000, 0x3f576aa4),
            (0x3f490fdb, 0x3f3504f3),
            (0x3fc90fdb, 0x3f800000),
            (0x40490fdb, 0xb3bbbd2e),
            (0x4096cbe4, 0xbf800000),
            (0x4111b193, 0x3ea08915),
            (0x42effae1, 0x3f128b58),
            (0x42f00000, 0x3f14a2ef),
            (0x49742400, 0xbeb33259),
            (0x60ad78ec, 0x3f281569),
            (0x7f7fffff, 0xbf0599b3),
        ] {
            assert_eq!(
                super::sin(f32::from_bits(input)).to_bits(),
                expected,
                "{input:08x}"
            );
        }
        assert!(super::sin(f32::INFINITY).is_nan());
        assert!(super::sin(f32::NAN).is_nan());
    }
}
