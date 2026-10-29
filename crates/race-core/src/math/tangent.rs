// Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
// Developed at SunPro, a Sun Microsystems, Inc. business.
// Permission to use, copy, modify, and distribute this software is freely
// granted, provided that this notice is preserved.
//
// Rust adaptation under MIT, preserving the original permissive notice.
// Algorithm: glibc-2.39 sysdeps/ieee754/flt-32/{s_tanf.c,k_tanf.c}.
// Range reduction shares the separately noticed Arm kernel. No libc wrappers
// or floating-point exception handling is included.

const T: [f32; 13] = [
    f32::from_bits(0x3eaaaaab),
    f32::from_bits(0x3e088889),
    f32::from_bits(0x3d5d0dd1),
    f32::from_bits(0x3cb327a4),
    f32::from_bits(0x3c11371f),
    f32::from_bits(0x3b6b6916),
    f32::from_bits(0x3abede48),
    f32::from_bits(0x3a1a26c8),
    f32::from_bits(0x398137b9),
    f32::from_bits(0x38a3f445),
    f32::from_bits(0x3895c07a),
    f32::from_bits(0xb79bae5f),
    f32::from_bits(0x37d95384),
];
fn kernel(mut x: f32, mut y: f32, reciprocal: bool) -> f32 {
    let bits = x.to_bits();
    let magnitude = bits & 0x7fffffff;
    let mode = if reciprocal { -1.0 } else { 1.0 };
    if magnitude < 0x39000000 {
        return if reciprocal {
            if magnitude == 0 {
                1.0 / x.abs()
            } else {
                -1.0 / x
            }
        } else {
            x
        };
    }
    let sign = if bits >> 31 == 0 { 1.0 } else { -1.0 };
    if magnitude >= 0x3f2ca140 {
        if bits >> 31 != 0 {
            x = -x;
            y = -y;
        }
        x = (f32::from_bits(0x3f490fda) - x) + (f32::from_bits(0x33222168) - y);
        y = 0.0;
        if x.abs() < f32::from_bits(0x39000000) {
            return sign * mode * (1.0 - 2.0 * mode * x);
        }
    }
    let z = x * x;
    let w = z * z;
    let r = T[1] + w * (T[3] + w * (T[5] + w * (T[7] + w * (T[9] + w * T[11]))));
    let v = z * (T[2] + w * (T[4] + w * (T[6] + w * (T[8] + w * (T[10] + w * T[12])))));
    let s = z * x;
    let r = (y + z * (s * (r + v) + y)) + T[0] * s;
    let w = x + r;
    if magnitude >= 0x3f2ca140 {
        return sign * (mode - 2.0 * (x - (w * w / (w + mode) - r)));
    }
    if !reciprocal {
        return w;
    }
    let z = f32::from_bits(w.to_bits() & 0xfffff000);
    let v = r - (z - x);
    let a = -1.0 / w;
    let t = f32::from_bits(a.to_bits() & 0xfffff000);
    t + a * ((1.0 + t * z) + t * v)
}
pub(super) fn tan(value: f32) -> f32 {
    let magnitude = value.to_bits() & 0x7fffffff;
    if magnitude <= 0x3f490fda {
        return kernel(value, 0.0, false);
    }
    if magnitude >= 0x7f800000 {
        return f32::NAN;
    }
    let (high, low, quadrant) = super::sine::reduce_tangent(value);
    kernel(high, low, quadrant & 1 != 0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_long_savanna_bank_preserves_the_native_tangent_bit() {
        for (input, expected) in [
            (0xbc47f0dd, 0xbc47f368),
            (0x3c47f0dd, 0x3c47f368),
            (0x00000000, 0x00000000),
            (0x80000000, 0x80000000),
            (0x3dcccccd, 0x3dcd7c44),
            (0x3e99999a, 0x3e9e6153),
            (0x3f000000, 0x3f0bda7b),
        ] {
            assert_eq!(
                super::tan(f32::from_bits(input)).to_bits(),
                expected,
                "{input:08x}"
            );
        }
    }
}
