// Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
// Developed at SunPro, a Sun Microsystems, Inc. business.
// Permission to use, copy, modify, and distribute this software is freely
// granted, provided that this notice is preserved.
//
// Rust adaptation under MIT, preserving the original permissive notice.
// Algorithms: glibc-2.39 sysdeps/ieee754/flt-32/{s_atanf.c,e_atan2f.c}.
// No libc alias, exception or platform wrappers are included.

const HI: [f32; 4] = [0.46364760399, 0.78539812565, 0.98279368877, 1.5707962513];
const LO: [f32; 4] = [
    5.0121582440e-9,
    3.7748947079e-8,
    3.4473217170e-8,
    7.5497894159e-8,
];
const A: [f32; 11] = [
    0.33333334327,
    -0.20000000298,
    0.14285714924,
    -0.11111110449,
    0.090908870101,
    -0.076918758452,
    0.066610731184,
    -0.058335702866,
    0.049768779427,
    -0.036531571299,
    0.016285819933,
];

fn atan(mut x: f32) -> f32 {
    let bits = x.to_bits();
    let magnitude = bits & 0x7fffffff;
    if magnitude >= 0x4c000000 {
        if magnitude > 0x7f800000 {
            return x + x;
        }
        return if bits >> 31 == 0 {
            HI[3] + LO[3]
        } else {
            -HI[3] - LO[3]
        };
    }
    let id = if magnitude < 0x3ee00000 {
        if magnitude < 0x31000000 {
            return x;
        }
        None
    } else {
        x = x.abs();
        if magnitude < 0x3f980000 {
            if magnitude < 0x3f300000 {
                x = (2.0 * x - 1.0) / (2.0 + x);
                Some(0)
            } else {
                x = (x - 1.0) / (x + 1.0);
                Some(1)
            }
        } else if magnitude < 0x401c0000 {
            x = (x - 1.5) / (1.0 + 1.5 * x);
            Some(2)
        } else {
            x = -1.0 / x;
            Some(3)
        }
    };
    let z = x * x;
    let w = z * z;
    let s1 = z * (A[0] + w * (A[2] + w * (A[4] + w * (A[6] + w * (A[8] + w * A[10])))));
    let s2 = w * (A[1] + w * (A[3] + w * (A[5] + w * (A[7] + w * A[9]))));
    match id {
        None => x - x * (s1 + s2),
        Some(id) => {
            let z = HI[id] - ((x * (s1 + s2) - LO[id]) - x);
            if bits >> 31 != 0 {
                -z
            } else {
                z
            }
        }
    }
}

pub(super) fn atan2(y: f32, x: f32) -> f32 {
    let hx = x.to_bits();
    let hy = y.to_bits();
    let ix = hx & 0x7fffffff;
    let iy = hy & 0x7fffffff;
    let pi = f32::from_bits(0x40490fdb);
    let half = f32::from_bits(0x3fc90fdb);
    let quarter = f32::from_bits(0x3f490fdb);
    let low = f32::from_bits(0xb3bbbd2e);
    if ix > 0x7f800000 || iy > 0x7f800000 {
        return x + y;
    }
    if hx == 0x3f800000 {
        return atan(y);
    }
    let quadrant = ((hy >> 31) & 1) | ((hx >> 30) & 2);
    if iy == 0 {
        return match quadrant {
            0 | 1 => y,
            2 => pi,
            _ => -pi,
        };
    }
    if ix == 0 {
        return if hy >> 31 != 0 { -half } else { half };
    }
    if ix == 0x7f800000 {
        if iy == 0x7f800000 {
            return match quadrant {
                0 => quarter,
                1 => -quarter,
                2 => 3.0 * quarter,
                _ => -3.0 * quarter,
            };
        }
        return match quadrant {
            0 => 0.0,
            1 => -0.0,
            2 => pi,
            _ => -pi,
        };
    }
    if iy == 0x7f800000 {
        return if hy >> 31 != 0 { -half } else { half };
    }
    let difference = ((iy as i32) - (ix as i32)) >> 23;
    let z = if difference > 60 {
        half + 0.5 * low
    } else if hx >> 31 != 0 && difference < -60 {
        0.0
    } else {
        atan((y / x).abs())
    };
    match quadrant {
        0 => z,
        1 => f32::from_bits(z.to_bits() ^ 0x80000000),
        2 => pi - (z - low),
        _ => (z - low) - pi,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn captured_native_angles_cover_quadrants_and_reduction() {
        for (y, x, expected) in [
            (0x00000000, 0x3f800000, 0x00000000),
            (0x80000000, 0xbf800000, 0xc0490fdb),
            (0x3f800000, 0x00000000, 0x3fc90fdb),
            (0xbf800000, 0x00000000, 0xbfc90fdb),
            (0x3f800000, 0x3f800000, 0x3f490fdb),
            (0x3f800000, 0xbf800000, 0x4016cbe4),
            (0xbf800000, 0xbf800000, 0xc016cbe4),
            (0x3dcccccd, 0x40133333, 0x3d31f991),
            (0x40133333, 0x3dcccccd, 0x3fc3800e),
            (0x1e3ce508, 0xbf800000, 0x40490fdb),
            (0x60ad78ec, 0x3dcccccd, 0x3fc90fdb),
            (0x38d1b717, 0x3f7fbe77, 0x38d1ecd4),
        ] {
            assert_eq!(
                super::atan2(f32::from_bits(y), f32::from_bits(x)).to_bits(),
                expected,
                "{y:08x} {x:08x}"
            );
        }
    }
}
