//! The authored tracks (spec 7 and 16) and the two battle arenas (spec 17). Each is a turtle path (straights and
//! constant-radius arcs, with heights at segment ends) turned into Catmull-Rom
//! control points, plus features placed by segment and offset.

use glam::Vec3;

use crate::track::{Feature, SceneryKind, Theme, TrackDef, TrackId, TrackPoint, WallStyle};

/// A turtle segment for authoring tracks. Angles in degrees, + = left turn;
/// the last field is the road height at the segment end.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Seg {
    /// Straight of `len` metres.
    S(f32, f32),
    /// Arc of `deg` degrees (+ left) at `radius`.
    T(f32, f32, f32),
}

/// Turns turtle segments into closed control points (every ~5 m). Returns the
/// points and the arc length at the start of every segment (for feature placement).
///
/// Turn angles are normalised to sum to exactly +-360 degrees and the closing
/// position error is spread linearly along the lap. `bank_k / radius` gives
/// each corner's bank (clamped to 0.14 rad).
pub fn turtle(segs: &[Seg], width: f32, bank_k: f32) -> (Vec<TrackPoint>, Vec<f32>) {
    turtle_core(segs, width, bank_k, true, None)
}

/// [`turtle`] for paths whose net turn is not +-360 degrees (a figure eight
/// turns by 0): turn angles are used exactly as given, so the caller must
/// supply a closing geometry (any small residual is still spread along the
/// lap). Heights come from `height(s)` (a function of the arc length from the
/// start) instead of the per-segment end heights.
pub(crate) fn turtle_free(
    segs: &[Seg],
    width: f32,
    bank_k: f32,
    height: &dyn Fn(f32) -> f32,
) -> (Vec<TrackPoint>, Vec<f32>) {
    turtle_core(segs, width, bank_k, false, Some(height))
}

fn turtle_core(
    segs: &[Seg],
    width: f32,
    bank_k: f32,
    normalise: bool,
    height_fn: Option<&dyn Fn(f32) -> f32>,
) -> (Vec<TrackPoint>, Vec<f32>) {
    let step = 5.0f32;
    // Normalise turn angles to exactly +-360 so the heading closes.
    let turn: f32 = segs
        .iter()
        .map(|s| if let Seg::T(d, _, _) = s { *d } else { 0.0 })
        .sum();
    let ang_scale = if normalise { 360.0f32.copysign(turn) / turn } else { 1.0 };
    let mut pts: Vec<(Vec3, f32, f32)> = Vec::new(); // (pos xz, s, bank)
    let mut seg_s = Vec::new();
    let mut keys: Vec<(f32, f32)> = vec![(0.0, 0.0)]; // (s, y)
    let (mut x, mut z, mut yaw, mut s) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for seg in segs {
        seg_s.push(s);
        match *seg {
            Seg::S(len, y) => {
                let k = (len / step).ceil().max(1.0) as usize;
                for _ in 0..k {
                    pts.push((Vec3::new(x, 0.0, z), s, 0.0));
                    x += crate::math::sin(yaw) * len / k as f32;
                    z += crate::math::cos(yaw) * len / k as f32;
                    s += len / k as f32;
                }
                keys.push((s, y));
            }
            Seg::T(deg, radius, y) => {
                let d = (deg * ang_scale).to_radians();
                let arc = d.abs() * radius;
                let k = (arc / step).ceil().max(2.0) as usize;
                let bank = (-d.signum() * bank_k / radius).clamp(-0.14, 0.14);
                for _ in 0..k {
                    pts.push((Vec3::new(x, 0.0, z), s, bank));
                    let h = yaw + 0.5 * d / k as f32;
                    x += crate::math::sin(h) * arc / k as f32;
                    z += crate::math::cos(h) * arc / k as f32;
                    yaw += d / k as f32;
                    s += arc / k as f32;
                }
                keys.push((s, y));
            }
        }
    }
    let total = s;
    // Close the residual by a linear correction along s.
    let err = Vec3::new(x, 0.0, z);
    for p in pts.iter_mut() {
        p.0 -= err * (p.1 / total);
    }
    // Heights: periodic Catmull-Rom over the segment-end keys.
    keys.pop();
    let nk = keys.len();
    let height = |s: f32| -> f32 {
        let mut k = 0;
        while k + 1 < nk && keys[k + 1].0 <= s {
            k += 1;
        }
        let (s1, y1) = keys[k];
        let (s2, y2) = if k + 1 < nk {
            keys[k + 1]
        } else {
            (total, keys[0].1)
        };
        let (s0, y0) = if k > 0 {
            keys[k - 1]
        } else {
            (keys[nk - 1].0 - total, keys[nk - 1].1)
        };
        let (s3, y3) = if k + 2 < nk {
            keys[k + 2]
        } else {
            (keys[(k + 2) % nk].0 + total, keys[(k + 2) % nk].1)
        };
        let t = ((s - s1) / (s2 - s1).max(1e-3)).clamp(0.0, 1.0);
        // Cardinal spline with tangents from neighbouring slopes (non-uniform spacing).
        let m1 = (y2 - y0) / (s2 - s0).max(1e-3) * (s2 - s1);
        let m2 = (y3 - y1) / (s3 - s1).max(1e-3) * (s2 - s1);
        let t2 = t * t;
        let t3 = t2 * t;
        (2.0 * t3 - 3.0 * t2 + 1.0) * y1
            + (t3 - 2.0 * t2 + t) * m1
            + (-2.0 * t3 + 3.0 * t2) * y2
            + (t3 - t2) * m2
    };
    // Smooth banks over neighbours.
    let n = pts.len();
    let banks: Vec<f32> = (0..n)
        .map(|i| {
            let mut a = 0.0;
            for d in -3i32..=3 {
                a += pts[(i as i32 + d).rem_euclid(n as i32) as usize].2;
            }
            a / 7.0
        })
        .collect();
    let points = pts
        .iter()
        .zip(banks)
        .map(|(p, b)| TrackPoint {
            pos: Vec3::new(p.0.x, height_fn.map_or_else(|| height(p.1), |f| f(p.1)), p.0.z),
            width,
            bank: b,
        })
        .collect();
    (points, seg_s)
}

/// Every track definition, indexed by [`TrackId::index`].
pub fn track_defs() -> Vec<TrackDef> {
    TrackId::ALL.iter().map(|&id| def(id)).collect()
}

pub(crate) fn def(id: TrackId) -> TrackDef {
    match id {
        TrackId::Savanna => savanna(),
        TrackId::Dune => dune(),
        TrackId::Canyon => canyon(),
        TrackId::Midnight => midnight(),
        TrackId::Training => training(),
        TrackId::Baobab => baobab(),
        TrackId::Volcano => volcano(),
        TrackId::Oasis => oasis(),
        TrackId::Halving => halving(),
        TrackId::DustBowl => dust_bowl(),
        TrackId::MempoolPlaza => mempool_plaza(),
        TrackId::Eight => figure_eight(),
        TrackId::Field => paddock(),
        TrackId::Custom => unreachable!("TrackId::Custom resolves through the custom registry"),
    }
}

fn coins(at: f32, lateral: f32, count: u8) -> Feature {
    Feature::CoinLine {
        s: at,
        lateral,
        count,
        spacing: 3.0,
    }
}

fn savanna() -> TrackDef {
    use Seg::*;
    let segs = [
        S(126.7, 0.0),
        T(103.7, 80.0, 6.0),
        S(32.5, 9.0),
        T(-46.7, 75.0, 12.0),
        S(25.0, 12.0),
        T(146.0, 70.0, 4.5),
        S(119.3, 0.0),
        T(56.8, 90.0, -3.0),
        T(-58.7, 80.0, 3.0),
        T(89.2, 65.0, 9.0),
        T(69.7, 85.0, 3.0),
        S(36.6, 0.0),
    ];
    let (points, g) = turtle(&segs, 18.0, 2.0);
    let features = vec![
        Feature::BoostPad {
            s: g[0] + 40.0,
            lateral: 4.0,
            len: 6.0,
        },
        Feature::TargetRow {
            s: g[0] + 95.0,
            count: 4,
        },
        coins(g[0] + 15.0, -4.0, 5),
        coins(g[1] + 30.0, -5.0, 5),
        coins(g[1] + 120.0, 4.0, 4),
        coins(g[3] + 20.0, 5.0, 5),
        Feature::TargetRow {
            s: g[5] + 30.0,
            count: 5,
        },
        coins(g[5] + 90.0, -5.0, 6),
        coins(g[6] + 10.0, 0.0, 4),
        Feature::BoostPad {
            s: g[6] + 38.0,
            lateral: 0.0,
            len: 6.0,
        },
        Feature::Ramp {
            s: g[6] + 55.0,
            len: 12.0,
            height: 1.5,
            lip: 1.0,
        },
        coins(g[6] + 95.0, 4.0, 5),
        coins(g[7] + 40.0, -4.0, 5),
        Feature::TargetRow {
            s: g[9] + 20.0,
            count: 4,
        },
        coins(g[9] + 60.0, 5.0, 5),
        coins(g[10] + 50.0, -3.0, 6),
    ];
    TrackDef {
        id: TrackId::Savanna,
        name: "Savanna Sprint".into(),
        theme: Theme {
            sky_top: [0.27, 0.52, 0.86],
            sky_horizon: [0.86, 0.88, 0.80],
            fog: [0.86, 0.84, 0.72],
            fog_density: 0.0025,
            sun_dir: [0.3, 0.9, 0.3],
            sun_colour: [1.05, 1.0, 0.88],
            ambient_sky: [0.55, 0.62, 0.72],
            ambient_ground: [0.45, 0.38, 0.22],
            ground: [0.80, 0.68, 0.33],
            ground_alt: [0.66, 0.60, 0.28],
            road: [0.62, 0.48, 0.32],
            road_stripe: [0.92, 0.86, 0.70],
            shoulder: [0.72, 0.66, 0.30],
            wall: [0.50, 0.36, 0.22],
            curb_a: [0.86, 0.18, 0.14],
            curb_b: [0.96, 0.94, 0.90],
            night: false,
            wall_style: WallStyle::Fence,
            scenery: vec![
                (SceneryKind::Acacia, 3.0),
                (SceneryKind::GrassTuft, 14.0),
                (SceneryKind::Rock, 1.5),
            ],
            terrain_amp: 3.0,
            terrain_scale: 70.0,
            terrain_rise: 1.0,
            terrain_blend: 40.0,
        },
        laps: 3,
        points,
        shoulder: [4.5, 4.5],
        features,
        seed: 0x5A7A_0001,
    }
}

fn dune() -> TrackDef {
    use Seg::*;
    let segs = [
        S(205.5, 0.0),
        T(180.0, 17.0, 2.5),
        S(124.3, 8.0),
        T(-140.0, 35.0, 2.5),
        S(115.0, -2.5),
        T(180.0, 18.0, 0.0),
        S(98.1, 5.0),
        T(-133.1, 40.0, 0.0),
        T(103.6, 30.0, -5.0),
        S(96.5, 0.0),
        T(114.0, 17.0, 2.5),
        S(73.1, 5.0),
        T(-71.2, 40.0, 0.0),
        T(126.6, 30.0, -2.5),
        S(94.0, 0.0),
    ];
    let (points, g) = turtle(&segs, 16.0, 1.6);
    let features = vec![
        coins(g[0] + 20.0, 3.0, 5),
        Feature::TargetRow {
            s: g[0] + 120.0,
            count: 4,
        },
        coins(g[0] + 160.0, -4.0, 5),
        coins(g[1] + 20.0, -3.0, 5),
        Feature::Ramp {
            s: g[2] + 50.0,
            len: 11.0,
            height: 1.4,
            lip: 2.0,
        },
        coins(g[2] + 85.0, 0.0, 5),
        coins(g[3] + 40.0, 4.0, 4),
        coins(g[4] + 30.0, -3.0, 5),
        Feature::TargetRow {
            s: g[4] + 80.0,
            count: 5,
        },
        coins(g[5] + 25.0, -4.0, 5),
        coins(g[6] + 40.0, 3.0, 5),
        coins(g[8] + 20.0, -4.0, 4),
        Feature::Ramp {
            s: g[9] + 40.0,
            len: 11.0,
            height: 1.4,
            lip: 2.0,
        },
        coins(g[9] + 70.0, 0.0, 5),
        Feature::TargetRow {
            s: g[11] + 30.0,
            count: 4,
        },
        coins(g[12] + 20.0, 4.0, 5),
        coins(g[14] + 30.0, -3.0, 6),
    ];
    TrackDef {
        id: TrackId::Dune,
        name: "Dune Drift".into(),
        theme: Theme {
            sky_top: [0.36, 0.55, 0.85],
            sky_horizon: [0.98, 0.80, 0.58],
            fog: [0.96, 0.78, 0.55],
            fog_density: 0.0035,
            sun_dir: [-0.4, 0.75, 0.5],
            sun_colour: [1.15, 0.98, 0.80],
            ambient_sky: [0.60, 0.60, 0.66],
            ambient_ground: [0.62, 0.42, 0.24],
            ground: [0.92, 0.62, 0.32],
            ground_alt: [0.86, 0.54, 0.26],
            road: [0.80, 0.56, 0.34],
            road_stripe: [0.98, 0.90, 0.72],
            shoulder: [0.95, 0.74, 0.46],
            wall: [0.86, 0.58, 0.30],
            curb_a: [0.97, 0.58, 0.10],
            curb_b: [0.98, 0.95, 0.88],
            night: false,
            wall_style: WallStyle::DuneBerm,
            scenery: vec![
                (SceneryKind::Dune, 4.0),
                (SceneryKind::Cactus, 3.0),
                (SceneryKind::Rock, 1.0),
            ],
            terrain_amp: 7.0,
            terrain_scale: 55.0,
            terrain_rise: 2.0,
            terrain_blend: 35.0,
        },
        laps: 3,
        points,
        shoulder: [5.0, 5.0],
        features,
        seed: 0xD00E_0002,
    }
}

fn canyon() -> TrackDef {
    use Seg::*;
    let segs = [
        S(182.5, 2.0),
        T(101.9, 45.0, 6.0),
        T(-106.4, 28.0, 11.0),
        S(116.5, 15.0),
        T(160.0, 20.0, 16.0),
        S(152.8, 17.0),
        T(-40.2, 50.0, 14.0),
        T(80.0, 60.0, 11.0),
        S(250.8, 3.0),
        T(83.5, 35.0, 1.0),
        T(-110.0, 30.0, -2.0),
        T(80.7, 26.0, -3.0),
        T(110.5, 60.0, -1.0),
        S(128.2, 0.0),
    ];
    let (points, g) = turtle(&segs, 13.0, 2.0);
    let gap_s = g[8] + 128.0;
    let features = vec![
        coins(g[0] + 30.0, -2.5, 5),
        Feature::TargetRow {
            s: g[0] + 120.0,
            count: 3,
        },
        coins(g[1] + 25.0, 2.5, 5),
        coins(g[3] + 30.0, 0.0, 6),
        Feature::TargetRow {
            s: g[3] + 90.0,
            count: 4,
        },
        coins(g[5] + 40.0, -3.0, 5),
        coins(g[5] + 110.0, 3.0, 4),
        coins(g[7] + 20.0, -2.5, 5),
        coins(g[8] + 30.0, 0.0, 5),
        Feature::BoostPad {
            s: g[8] + 85.0,
            lateral: 0.0,
            len: 7.0,
        },
        Feature::Ramp {
            s: gap_s - 13.0,
            len: 13.0,
            height: 2.0,
            lip: 0.0,
        },
        Feature::Gap {
            s: gap_s,
            len: 11.0,
        },
        coins(g[8] + 175.0, 2.5, 5),
        Feature::TargetRow {
            s: g[8] + 210.0,
            count: 3,
        },
        coins(g[10] + 20.0, -2.5, 4),
        coins(g[12] + 30.0, 2.5, 5),
        coins(g[13] + 40.0, 0.0, 5),
    ];
    TrackDef {
        id: TrackId::Canyon,
        name: "Red Rock Canyon".into(),
        theme: Theme {
            sky_top: [0.24, 0.20, 0.45],
            sky_horizon: [0.98, 0.55, 0.35],
            fog: [0.80, 0.45, 0.35],
            fog_density: 0.004,
            sun_dir: [0.7, 0.25, -0.65],
            sun_colour: [1.2, 0.72, 0.45],
            ambient_sky: [0.45, 0.38, 0.55],
            ambient_ground: [0.50, 0.25, 0.18],
            ground: [0.70, 0.30, 0.18],
            ground_alt: [0.58, 0.24, 0.15],
            road: [0.46, 0.32, 0.28],
            road_stripe: [0.95, 0.82, 0.60],
            shoulder: [0.66, 0.38, 0.24],
            wall: [0.62, 0.26, 0.16],
            curb_a: [0.95, 0.85, 0.75],
            curb_b: [0.70, 0.18, 0.12],
            night: false,
            wall_style: WallStyle::RockWall,
            scenery: vec![
                (SceneryKind::Mesa, 1.2),
                (SceneryKind::Rock, 6.0),
                (SceneryKind::Cactus, 1.0),
            ],
            terrain_amp: 6.0,
            terrain_scale: 45.0,
            terrain_rise: 14.0,
            terrain_blend: 22.0,
        },
        laps: 3,
        points,
        shoulder: [2.5, 2.5],
        features,
        seed: 0xCA1F_0003,
    }
}

fn midnight() -> TrackDef {
    use Seg::*;
    let segs = [
        S(248.0, 0.0),
        T(111.8, 40.0, 0.0),
        S(99.7, 0.0),
        T(55.0, 22.0, 0.0),
        T(-110.0, 22.0, 0.0),
        T(55.0, 22.0, 0.0),
        S(169.7, 0.0),
        T(103.5, 35.0, 0.0),
        S(93.7, 0.0),
        T(-99.6, 45.0, 0.0),
        S(69.8, 0.0),
        T(152.5, 30.0, 0.0),
        S(182.5, 0.0),
        T(-55.0, 22.0, 0.0),
        T(110.0, 22.0, 0.0),
        T(-55.0, 22.0, 0.0),
        S(102.5, 0.0),
        T(91.8, 45.0, 0.0),
        S(78.0, 0.0),
    ];
    let (points, g) = turtle(&segs, 16.0, 0.0);
    let features = vec![
        Feature::BoostPad {
            s: g[0] + 50.0,
            lateral: -4.0,
            len: 6.0,
        },
        coins(g[0] + 80.0, 4.0, 6),
        Feature::BoostPad {
            s: g[0] + 150.0,
            lateral: 4.0,
            len: 6.0,
        },
        Feature::TargetRow {
            s: g[0] + 200.0,
            count: 5,
        },
        coins(g[1] + 30.0, -4.0, 5),
        Feature::BoostPad {
            s: g[2] + 40.0,
            lateral: 0.0,
            len: 6.0,
        },
        coins(g[2] + 60.0, 3.0, 5),
        coins(g[4] + 10.0, 0.0, 4),
        Feature::BoostPad {
            s: g[6] + 50.0,
            lateral: -3.0,
            len: 6.0,
        },
        coins(g[6] + 90.0, 3.0, 6),
        Feature::TargetRow {
            s: g[6] + 140.0,
            count: 4,
        },
        coins(g[7] + 30.0, -4.0, 5),
        Feature::BoostPad {
            s: g[8] + 40.0,
            lateral: 3.0,
            len: 6.0,
        },
        coins(g[9] + 30.0, 4.0, 5),
        coins(g[11] + 40.0, -4.0, 5),
        Feature::BoostPad {
            s: g[12] + 40.0,
            lateral: 0.0,
            len: 6.0,
        },
        Feature::TargetRow {
            s: g[12] + 110.0,
            count: 5,
        },
        coins(g[12] + 140.0, -3.0, 6),
        coins(g[14] + 10.0, 0.0, 4),
        Feature::BoostPad {
            s: g[16] + 50.0,
            lateral: 0.0,
            len: 6.0,
        },
        coins(g[17] + 30.0, 4.0, 5),
        coins(g[18] + 20.0, -2.0, 5),
    ];
    TrackDef {
        id: TrackId::Midnight,
        name: "Mempool Midnight".into(),
        theme: Theme {
            sky_top: [0.02, 0.02, 0.08],
            sky_horizon: [0.10, 0.08, 0.22],
            fog: [0.06, 0.05, 0.14],
            fog_density: 0.006,
            sun_dir: [-0.3, 0.6, 0.4],
            sun_colour: [0.40, 0.45, 0.70],
            ambient_sky: [0.20, 0.20, 0.38],
            ambient_ground: [0.18, 0.16, 0.22],
            ground: [0.78, 0.78, 0.84],
            ground_alt: [0.66, 0.66, 0.74],
            road: [0.12, 0.12, 0.16],
            road_stripe: [0.97, 0.58, 0.10],
            shoulder: [0.55, 0.55, 0.62],
            wall: [0.97, 0.58, 0.10],
            curb_a: [0.97, 0.58, 0.10],
            curb_b: [0.15, 0.15, 0.20],
            night: true,
            wall_style: WallStyle::NeonRail,
            scenery: vec![(SceneryKind::NeonPylon, 3.0), (SceneryKind::Rock, 0.8)],
            terrain_amp: 0.3,
            terrain_scale: 60.0,
            terrain_rise: 0.0,
            terrain_blend: 30.0,
        },
        laps: 3,
        points,
        shoulder: [3.0, 3.0],
        features,
        seed: 0x3E3B_0004,
    }
}

/// The Training Ground / Figure Eight / Paddock palette (a dusty paddock).
fn training_theme() -> Theme {
    Theme {
        sky_top: [0.35, 0.58, 0.88],
        sky_horizon: [0.90, 0.86, 0.76],
        fog: [0.88, 0.82, 0.70],
        fog_density: 0.003,
        sun_dir: [0.4, 0.85, 0.2],
        sun_colour: [1.05, 0.98, 0.86],
        ambient_sky: [0.55, 0.60, 0.70],
        ambient_ground: [0.45, 0.36, 0.24],
        ground: [0.74, 0.62, 0.40],
        ground_alt: [0.62, 0.56, 0.32],
        road: [0.66, 0.52, 0.36],
        road_stripe: [0.95, 0.92, 0.82],
        shoulder: [0.70, 0.60, 0.36],
        wall: [0.90, 0.88, 0.82],
        curb_a: [0.97, 0.58, 0.10],
        curb_b: [0.96, 0.94, 0.90],
        night: false,
        wall_style: WallStyle::Paddock,
        scenery: vec![
            (SceneryKind::HayBale, 4.0),
            (SceneryKind::GrassTuft, 10.0),
            (SceneryKind::Acacia, 1.0),
        ],
        terrain_amp: 1.5,
        terrain_scale: 60.0,
        terrain_rise: 0.5,
        terrain_blend: 30.0,
    }
}

/// Training oval length of the straights, turn radius and road width (spec 18.1).
const OVAL_STRAIGHT: f32 = 160.0;
const OVAL_RADIUS: f32 = 65.0;

fn training() -> TrackDef {
    use Seg::*;
    let segs = [
        S(OVAL_STRAIGHT, 0.0),
        T(180.0, OVAL_RADIUS, 0.0),
        S(OVAL_STRAIGHT, 0.0),
        T(180.0, OVAL_RADIUS, 0.0),
    ];
    let (points, g) = turtle(&segs, 20.0, 1.5);
    let half_arc = std::f32::consts::PI * OVAL_RADIUS;
    // Features were authored for 120 m straights and 40 m turns: scale offsets.
    let ks = OVAL_STRAIGHT / 120.0;
    let mut features = vec![
        Feature::BoostPad {
            s: g[0] + 12.0 * ks,
            lateral: 0.0,
            len: 6.0,
        },
        Feature::Hurdle { s: g[0] + 60.0 * ks },
        Feature::Hurdle { s: g[0] + 95.0 * ks },
        Feature::TargetRow {
            s: g[1] + 0.5 * half_arc,
            count: 4,
        },
        Feature::Ramp {
            s: g[2] + 12.0 * ks,
            len: 10.0,
            height: 1.2,
            lip: 1.0,
        },
        Feature::Hurdle { s: g[2] + 55.0 * ks },
        Feature::Hurdle { s: g[2] + 82.0 * ks },
        Feature::Hurdle { s: g[2] + 109.0 * ks },
        Feature::TargetRow {
            s: g[3] + 0.3 * half_arc,
            count: 4,
        },
        Feature::TargetRow {
            s: g[3] + 0.75 * half_arc,
            count: 3,
        },
        coins(g[0] + 25.0 * ks, -4.0, 5),
        coins(g[1] + 0.16 * half_arc, -4.0, 5),
        coins(g[1] + 0.64 * half_arc, 3.0, 5),
        coins(g[2] + 30.0 * ks, 4.0, 5),
        coins(g[3] + 0.24 * half_arc, -4.0, 5),
        coins(g[3] + 0.72 * half_arc, 3.0, 5),
    ];
    let lap = 2.0 * OVAL_STRAIGHT + 2.0 * half_arc;
    for k in 0..8 {
        features.push(Feature::Gate {
            s: 8.0 + k as f32 * lap / 8.0,
        });
    }
    TrackDef {
        id: TrackId::Training,
        name: "Training Ground".into(),
        theme: training_theme(),
        laps: 3,
        points,
        shoulder: [4.0, 4.0],
        features,
        seed: 0x7EA1_0005,
    }
}

/// Figure Eight geometry (spec 18.1): two loops of radius `EIGHT_R` joined by
/// two diagonals that cross at the origin at +-`EIGHT_ALPHA` to the x axis.
const EIGHT_R: f32 = 55.0;
const EIGHT_ALPHA_DEG: f32 = 30.0;
/// Overpass: height at the crossing, the flat crown (half-length) and the
/// ramp length on each side (grade <= 1.5 * 7 / 88 = 11.9 %).
const EIGHT_DECK: f32 = 7.0;
const EIGHT_CROWN: f32 = 4.0;
const EIGHT_RAMP: f32 = 88.0;

/// The figure eight: diagonal 1 (flat), loop B (clockwise), diagonal 2 (the
/// overpass), loop A (counter-clockwise). Net turn 0, so it is built with
/// [`turtle_free`]. The turns sweep 240 degrees each.
fn figure_eight() -> TrackDef {
    use Seg::*;
    let alpha = EIGHT_ALPHA_DEG.to_radians();
    let c = EIGHT_R / crate::math::sin(alpha);
    let diag = 2.0 * (c * c - EIGHT_R * EIGHT_R).sqrt();
    let sweep = 180.0 + 2.0 * EIGHT_ALPHA_DEG;
    let arc = sweep.to_radians() * EIGHT_R;
    let segs = [
        S(diag, 0.0),
        T(-sweep, EIGHT_R, 0.0),
        S(diag, 0.0),
        T(sweep, EIGHT_R, 0.0),
    ];
    // Arc length of the middle of diagonal 2 (the overpass crown).
    let s_cross_hi = diag + arc + 0.5 * diag;
    let s_cross_lo = 0.5 * diag;
    let height = move |s: f32| {
        let d = ((s - s_cross_hi).abs() - EIGHT_CROWN).max(0.0);
        let u = (d / EIGHT_RAMP).clamp(0.0, 1.0);
        EIGHT_DECK * (1.0 - u * u * (3.0 - 2.0 * u))
    };
    let (points, g) = turtle_free(&segs, 18.0, 1.5, &height);
    let lap = 2.0 * diag + 2.0 * arc;
    // Keep-out zones for features: the lower road within 25 m of the crossing,
    // and the whole bridge ramp (plus a margin) on the elevated diagonal.
    let ramp_reach = EIGHT_CROWN + EIGHT_RAMP;
    let hi0 = s_cross_hi - ramp_reach - 4.0;
    let hi1 = s_cross_hi + ramp_reach + 4.0;
    let lo0 = s_cross_lo - 25.0;
    let lo1 = s_cross_lo + 25.0;
    let mut features = vec![
        Feature::BoostPad {
            s: 14.0,
            lateral: 0.0,
            len: 6.0,
        },
        Feature::Hurdle { s: 42.0 },
        Feature::Hurdle { s: 54.0 },
        Feature::Hurdle { s: 66.0 },
        Feature::TargetRow {
            s: g[1] + 0.5 * arc,
            count: 4,
        },
        Feature::Ramp {
            s: lo1 + 12.0,
            len: 10.0,
            height: 1.2,
            lip: 1.0,
        },
        Feature::Hurdle { s: lo1 + 38.0 },
        Feature::Hurdle { s: lo1 + 56.0 },
        Feature::TargetRow {
            s: g[3] + 0.3 * arc,
            count: 4,
        },
        Feature::TargetRow {
            s: g[3] + 0.75 * arc,
            count: 3,
        },
        coins(22.0, -4.0, 5),
        coins(g[1] + 0.16 * arc, -4.0, 5),
        coins(g[1] + 0.64 * arc, 3.0, 5),
        coins(lo1 + 24.0, 4.0, 5),
        coins(g[3] + 0.24 * arc, -4.0, 5),
        coins(g[3] + 0.72 * arc, 3.0, 5),
    ];
    // Eight gates evenly spaced along the lap's free length (the keep-out
    // zones are skipped).
    let free: [(f32, f32); 3] = [(0.0, lo0), (lo1, hi0), (hi1, lap)];
    let total: f32 = free.iter().map(|(a, b)| b - a).sum();
    for k in 0..8 {
        let mut a = (k as f32 + 0.5) * total / 8.0;
        for &(f0, f1) in &free {
            if a <= f1 - f0 {
                features.push(Feature::Gate { s: f0 + a });
                break;
            }
            a -= f1 - f0;
        }
    }
    TrackDef {
        id: TrackId::Eight,
        name: "Figure Eight".into(),
        theme: training_theme(),
        laps: 3,
        points,
        shoulder: [4.0, 4.0],
        features,
        seed: 0x7EA1_000B,
    }
}

/// The Paddock (spec 18.1): a circle centreline of radius 150 m about the
/// world origin and a 270 m wide road band (open ground r = 15..285 m). The
/// road, stripe and curbs are the grass colour, so no road shows.
fn paddock() -> TrackDef {
    use Seg::*;
    const RADIUS: f32 = 150.0;
    let (mut points, _) = turtle(&[T(180.0, RADIUS, 0.0), T(180.0, RADIUS, 0.0)], 270.0, 0.0);
    // The turtle starts at the origin heading +z and turns left: its circle is
    // centred at +x. Shift it onto the origin.
    for p in points.iter_mut() {
        p.pos.x -= RADIUS;
    }
    let mut theme = training_theme();
    theme.road = theme.ground;
    theme.road_stripe = theme.ground;
    theme.curb_a = theme.ground;
    theme.curb_b = theme.ground;
    // Hills beyond the fence.
    theme.terrain_amp = 4.0;
    theme.terrain_scale = 90.0;
    theme.terrain_rise = 8.0;
    theme.terrain_blend = 60.0;
    TrackDef {
        id: TrackId::Field,
        name: "The Paddock".into(),
        theme,
        laps: 1,
        points,
        shoulder: [0.0, 0.0],
        features: vec![],
        seed: 0x7EA1_000C,
    }
}

fn pad(at: f32, lateral: f32) -> Feature {
    Feature::BoostPad {
        s: at,
        lateral,
        len: 6.0,
    }
}

fn row(at: f32, count: u8) -> Feature {
    Feature::TargetRow { s: at, count }
}

/// Cup 2, track 1: dusk wetlands, flowing S-curves, an esses section and one
/// long left sweeper. 15 m road, ~1.4 km.
fn baobab() -> TrackDef {
    use Seg::*;
    let segs = [
        S(60.0, 2.0),
        T(35.0, 85.0, 5.0),
        T(-35.0, 85.0, 6.0),
        S(59.6, 5.0),
        T(-30.0, 75.0, 2.5),
        T(30.0, 75.0, 1.0),
        S(89.6, 0.0),
        T(175.0, 80.0, -3.0), // the long left sweeper
        S(78.7, 0.0),
        T(-40.0, 36.0, 2.0), // esses
        T(80.0, 36.0, 3.5),
        T(-80.0, 36.0, 2.0),
        T(40.0, 36.0, 0.0),
        S(68.7, 0.0),
        T(70.0, 157.5, 2.5),
        T(-30.0, 123.4, 5.0),
        T(145.0, 40.0, 3.0), // the one tight corner
        S(69.6, 0.0),
    ];
    let (points, g) = turtle(&segs, 15.0, 1.8);
    let features = vec![
        coins(g[0] + 20.0, -3.0, 5),
        coins(g[1] + 15.0, 3.0, 5),
        coins(g[3] + 8.0, 0.0, 5),
        row(g[3] + 30.0, 4),
        coins(g[4] + 12.0, -3.0, 4),
        pad(g[6] + 25.0, 0.0),
        coins(g[6] + 52.0, 3.0, 5),
        coins(g[7] + 60.0, -4.0, 6),
        coins(g[7] + 150.0, 4.0, 6),
        pad(g[8] + 30.0, 3.0),
        row(g[8] + 65.0, 5),
        coins(g[9] + 10.0, 0.0, 4),
        coins(g[11] + 15.0, 0.0, 4),
        coins(g[13] + 4.0, -3.0, 4),
        row(g[13] + 28.0, 4),
        Feature::Ramp {
            s: g[13] + 46.0,
            len: 10.0,
            height: 1.0,
            lip: 1.5,
        },
        coins(g[14] + 60.0, 3.0, 5),
        coins(g[17] + 8.0, 0.0, 4),
        pad(g[17] + 36.0, -3.0),
    ];
    TrackDef {
        id: TrackId::Baobab,
        name: "Baobab Bayou".into(),
        theme: Theme {
            sky_top: [0.10, 0.22, 0.34],
            sky_horizon: [0.96, 0.72, 0.34],
            fog: [0.30, 0.54, 0.50],
            fog_density: 0.0065,
            sun_dir: [0.7, 0.28, 0.4],
            sun_colour: [1.10, 0.80, 0.48],
            ambient_sky: [0.34, 0.56, 0.56],
            ambient_ground: [0.20, 0.32, 0.22],
            ground: [0.20, 0.42, 0.30],
            ground_alt: [0.15, 0.34, 0.30],
            road: [0.36, 0.32, 0.26],
            road_stripe: [0.95, 0.80, 0.40],
            shoulder: [0.28, 0.50, 0.32],
            wall: [0.42, 0.30, 0.18],
            curb_a: [0.95, 0.78, 0.25],
            curb_b: [0.10, 0.46, 0.42],
            night: false,
            wall_style: WallStyle::Fence,
            scenery: vec![
                (SceneryKind::Acacia, 3.5),
                (SceneryKind::GrassTuft, 15.0),
                (SceneryKind::Rock, 1.0),
            ],
            terrain_amp: 2.0,
            terrain_scale: 50.0,
            terrain_rise: 0.5,
            terrain_blend: 35.0,
        },
        laps: 3,
        points,
        shoulder: [3.5, 3.5],
        features,
        seed: 0xBA0B_0006,
    }
}

/// Cup 2, track 2: black rock and lava glow, two peaks and a crater, big climbs
/// and plunging descents, two gap jumps. 12.5 m road, ~1.5 km. The hardest.
fn volcano() -> TrackDef {
    use Seg::*;
    let segs = [
        S(116.7, 8.0),
        T(159.1, 27.0, 13.0),
        S(107.4, 7.0),
        T(-141.8, 25.0, 3.0),
        S(181.8, 11.0),
        T(141.2, 27.0, 19.0),
        S(257.7, 3.0), // the plunge
        T(48.4, 30.0, 1.0),
        S(112.7, -2.0),
        T(55.9, 45.0, -2.0),
        S(162.1, 0.0),
        T(63.1, 40.0, 1.0),
        S(78.3, 2.0),
        T(34.0, 45.0, 2.0),
        S(98.9, 0.0),
    ];
    let (points, g) = turtle(&segs, 12.5, 2.0);
    let gap1 = g[4] + 113.0;
    let gap2 = g[6] + 183.0;
    let features = vec![
        coins(g[0] + 20.0, -2.5, 5),
        coins(g[2] + 25.0, 2.5, 5),
        row(g[2] + 68.0, 3),
        coins(g[4] + 12.0, 0.0, 5),
        Feature::BoostPad {
            s: g[4] + 50.0,
            lateral: 0.0,
            len: 7.0,
        },
        Feature::Ramp {
            s: gap1 - 13.0,
            len: 13.0,
            height: 2.0,
            lip: 0.0,
        },
        Feature::Gap { s: gap1, len: 11.0 },
        coins(g[4] + 138.0, 2.5, 5),
        coins(g[6] + 15.0, -2.5, 5),
        row(g[6] + 55.0, 4),
        Feature::BoostPad {
            s: g[6] + 138.0,
            lateral: 0.0,
            len: 7.0,
        },
        Feature::Ramp {
            s: gap2 - 13.0,
            len: 13.0,
            height: 2.0,
            lip: 0.0,
        },
        Feature::Gap { s: gap2, len: 11.0 },
        coins(g[6] + 208.0, -2.5, 5),
        coins(g[8] + 30.0, 0.0, 5),
        row(g[10] + 50.0, 3),
        coins(g[10] + 105.0, 2.5, 5),
        coins(g[12] + 20.0, -2.5, 4),
        coins(g[14] + 30.0, 0.0, 5),
    ];
    TrackDef {
        id: TrackId::Volcano,
        name: "Volcano Vault".into(),
        theme: Theme {
            sky_top: [0.10, 0.04, 0.05],
            sky_horizon: [0.78, 0.26, 0.08],
            fog: [0.40, 0.15, 0.09],
            fog_density: 0.0085,
            sun_dir: [-0.25, 0.32, 0.55],
            sun_colour: [1.30, 0.55, 0.25],
            ambient_sky: [0.46, 0.20, 0.15],
            ambient_ground: [0.50, 0.15, 0.05],
            ground: [0.09, 0.08, 0.08],
            ground_alt: [0.16, 0.06, 0.04],
            road: [0.17, 0.15, 0.15],
            road_stripe: [1.0, 0.45, 0.08],
            shoulder: [0.24, 0.17, 0.15],
            wall: [0.11, 0.09, 0.09],
            curb_a: [1.0, 0.40, 0.05],
            curb_b: [0.12, 0.10, 0.10],
            night: false,
            wall_style: WallStyle::RockWall,
            scenery: vec![(SceneryKind::Rock, 6.5), (SceneryKind::Mesa, 1.4)],
            terrain_amp: 5.0,
            terrain_scale: 45.0,
            terrain_rise: 8.0,
            terrain_blend: 24.0,
        },
        laps: 3,
        points,
        shoulder: [2.5, 2.5],
        features,
        seed: 0x70C4_0007,
    }
}

/// Cup 2, track 3: bright midday desert, wide fast curves, three ramps and many
/// coin lines. 19 m road, ~1.2 km.
fn oasis() -> TrackDef {
    use Seg::*;
    let segs = [
        S(131.1, 2.0),
        T(61.9, 88.0, 5.0),
        S(40.2, 7.0),
        T(71.1, 79.2, 9.0),
        S(66.0, 5.0),
        T(79.0, 70.4, 2.0),
        S(66.3, 0.0),
        T(-68.3, 61.6, -3.5),
        S(50.3, -5.0),
        T(91.0, 70.4, -3.5),
        S(59.0, 0.0),
        T(73.4, 70.4, 3.5),
        S(61.5, 2.0),
        T(51.8, 88.0, 2.0),
        S(107.7, 0.0),
    ];
    let (points, g) = turtle(&segs, 19.0, 3.0);
    let ramp = |at: f32, len: f32, height: f32| Feature::Ramp {
        s: at,
        len,
        height,
        lip: 1.5,
    };
    let features = vec![
        coins(g[0] + 20.0, -5.0, 6),
        ramp(g[0] + 70.0, 12.0, 1.6),
        row(g[0] + 105.0, 5),
        coins(g[1] + 30.0, 5.0, 5),
        pad(g[2] + 8.0, 4.0),
        coins(g[2] + 14.0, -5.0, 4),
        coins(g[3] + 30.0, 0.0, 6),
        ramp(g[4] + 18.0, 12.0, 1.4),
        coins(g[4] + 42.0, -5.0, 5),
        coins(g[5] + 30.0, 5.0, 6),
        row(g[6] + 30.0, 4),
        coins(g[7] + 25.0, 0.0, 5),
        pad(g[8] + 12.0, -4.0),
        coins(g[8] + 30.0, 5.0, 4),
        coins(g[9] + 40.0, -5.0, 6),
        ramp(g[10] + 12.0, 11.0, 1.4),
        coins(g[10] + 35.0, 4.0, 5),
        coins(g[11] + 30.0, 0.0, 6),
        coins(g[13] + 25.0, 5.0, 5),
        coins(g[14] + 8.0, -5.0, 4),
        pad(g[14] + 30.0, 0.0),
        row(g[14] + 62.0, 5),
        coins(g[14] + 88.0, 5.0, 4),
    ];
    TrackDef {
        id: TrackId::Oasis,
        name: "Oasis Run".into(),
        theme: Theme {
            sky_top: [0.20, 0.50, 0.92],
            sky_horizon: [0.96, 0.92, 0.78],
            fog: [0.96, 0.89, 0.74],
            fog_density: 0.0018,
            sun_dir: [0.2, 0.95, 0.2],
            sun_colour: [1.20, 1.10, 0.92],
            ambient_sky: [0.66, 0.73, 0.84],
            ambient_ground: [0.72, 0.60, 0.40],
            ground: [0.97, 0.85, 0.56],
            ground_alt: [0.92, 0.78, 0.48],
            road: [0.52, 0.45, 0.38],
            road_stripe: [1.0, 0.96, 0.85],
            shoulder: [0.99, 0.88, 0.60],
            wall: [0.94, 0.80, 0.52],
            curb_a: [0.10, 0.62, 0.58],
            curb_b: [0.98, 0.96, 0.90],
            night: false,
            wall_style: WallStyle::DuneBerm,
            scenery: vec![
                (SceneryKind::Dune, 4.5),
                (SceneryKind::Cactus, 3.5),
                (SceneryKind::Rock, 0.6),
            ],
            terrain_amp: 8.0,
            terrain_scale: 60.0,
            terrain_rise: 1.5,
            terrain_blend: 35.0,
        },
        laps: 3,
        points,
        shoulder: [5.0, 5.0],
        features,
        seed: 0x0A51_0008,
    }
}

/// Cup 2, track 4: a night neon highway, very long fast straights joined by
/// drift hairpins, many boost pads and a mega ramp near the end. ~1.9 km.
fn halving() -> TrackDef {
    use Seg::*;
    let segs = [
        S(208.0, 5.0),
        T(180.0, 78.0, 10.0),
        S(412.0, 15.0),
        T(180.0, 26.0, 15.0),
        S(308.0, 8.0),
        T(-180.0, 26.0, 8.0),
        S(292.0, 5.0),
        T(180.0, 26.0, 3.0),
        S(188.0, 0.0),
    ];
    let (points, g) = turtle(&segs, 17.0, 1.5);
    let features = vec![
        coins(g[0] + 20.0, -4.0, 5),
        pad(g[0] + 60.0, -4.0),
        row(g[0] + 110.0, 5),
        pad(g[0] + 145.0, 4.0),
        coins(g[0] + 170.0, 4.0, 5),
        coins(g[1] + 90.0, 0.0, 6),
        coins(g[2] + 25.0, -4.0, 6),
        pad(g[2] + 60.0, 0.0),
        coins(g[2] + 110.0, 4.0, 6),
        pad(g[2] + 170.0, -4.0),
        row(g[2] + 225.0, 4),
        pad(g[2] + 290.0, 4.0),
        coins(g[2] + 360.0, -3.0, 6),
        coins(g[4] + 25.0, 3.0, 5),
        pad(g[4] + 60.0, 3.0),
        coins(g[4] + 120.0, -3.0, 6),
        pad(g[4] + 190.0, -3.0),
        coins(g[4] + 250.0, 4.0, 5),
        coins(g[6] + 25.0, -4.0, 5),
        pad(g[6] + 50.0, 0.0),
        coins(g[6] + 110.0, 3.0, 6),
        pad(g[6] + 170.0, -3.0),
        row(g[6] + 230.0, 5),
        pad(g[8] + 25.0, 0.0),
        coins(g[8] + 60.0, -4.0, 6),
        Feature::Ramp {
            s: g[8] + 105.0,
            len: 24.0,
            height: 4.0,
            lip: 3.0,
        },
    ];
    TrackDef {
        id: TrackId::Halving,
        name: "Halving Highway".into(),
        theme: Theme {
            sky_top: [0.03, 0.01, 0.07],
            sky_horizon: [0.20, 0.07, 0.22],
            fog: [0.11, 0.04, 0.16],
            fog_density: 0.0052,
            sun_dir: [-0.3, 0.55, 0.45],
            sun_colour: [0.52, 0.36, 0.82],
            ambient_sky: [0.26, 0.18, 0.42],
            ambient_ground: [0.18, 0.12, 0.22],
            ground: [0.10, 0.09, 0.15],
            ground_alt: [0.07, 0.06, 0.12],
            road: [0.09, 0.09, 0.12],
            road_stripe: [0.97, 0.58, 0.10],
            shoulder: [0.20, 0.17, 0.26],
            wall: [1.0, 0.62, 0.12],
            curb_a: [0.97, 0.58, 0.10],
            curb_b: [0.12, 0.10, 0.18],
            night: true,
            wall_style: WallStyle::NeonRail,
            scenery: vec![(SceneryKind::NeonPylon, 4.5), (SceneryKind::Rock, 0.4)],
            terrain_amp: 0.3,
            terrain_scale: 60.0,
            terrain_rise: 0.0,
            terrain_blend: 30.0,
        },
        laps: 3,
        points,
        shoulder: [3.0, 3.0],
        features,
        seed: 0x4A17_0009,
    }
}

/// Battle arena 1 (spec 17): a short, very wide loop (40 m road) round a
/// raised mesa island (the terrain rises sharply beyond the walls, inside and
/// out: a bowl). Two kickers, two sand mounds, 8 rows of 5 Power Eggs, coins.
fn dust_bowl() -> TrackDef {
    use Seg::*;
    let segs = [
        S(36.0, 0.0),
        T(90.0, 35.0, 0.0),
        S(36.0, 0.0),
        T(90.0, 35.0, 0.0),
        S(36.0, 0.0),
        T(90.0, 35.0, 0.0),
        S(36.0, 0.0),
        T(90.0, 35.0, 0.0),
    ];
    let (points, g) = turtle(&segs, 40.0, 0.0);
    let corner = std::f32::consts::FRAC_PI_2 * 35.0;
    let mound = |at: f32| Feature::Ramp {
        s: at,
        len: 6.0,
        height: 0.7,
        lip: 6.0,
    };
    let kicker = |at: f32| Feature::Ramp {
        s: at,
        len: 8.0,
        height: 1.4,
        lip: 1.0,
    };
    let mut features = vec![
        mound(g[0] + 12.0),
        kicker(g[2] + 10.0),
        mound(g[4] + 12.0),
        kicker(g[6] + 10.0),
        pad(g[0] + 2.0, -12.0),
        pad(g[4] + 2.0, 12.0),
        row(g[0] + 30.0, 5),
        row(g[2] + 28.0, 5),
        row(g[4] + 30.0, 5),
        row(g[6] + 28.0, 5),
    ];
    for k in 0..4 {
        let c = g[2 * k + 1];
        features.push(row(c + 0.5 * corner, 5));
        features.push(coins(c + 6.0, -13.0, 5));
        features.push(coins(c + 0.62 * corner, 13.0, 5));
    }
    features.push(coins(g[2] + 2.0, 12.0, 4));
    features.push(coins(g[6] + 2.0, -12.0, 4));
    TrackDef {
        id: TrackId::DustBowl,
        name: "Dust Bowl".into(),
        theme: Theme {
            sky_top: [0.34, 0.56, 0.88],
            sky_horizon: [0.97, 0.84, 0.64],
            fog: [0.95, 0.80, 0.60],
            fog_density: 0.003,
            sun_dir: [0.35, 0.8, 0.45],
            sun_colour: [1.15, 1.0, 0.84],
            ambient_sky: [0.60, 0.62, 0.70],
            ambient_ground: [0.64, 0.44, 0.26],
            ground: [0.86, 0.56, 0.32],
            ground_alt: [0.74, 0.44, 0.26],
            road: [0.84, 0.64, 0.42],
            road_stripe: [0.98, 0.92, 0.76],
            shoulder: [0.92, 0.72, 0.46],
            wall: [0.78, 0.48, 0.28],
            curb_a: [0.97, 0.58, 0.10],
            curb_b: [0.98, 0.95, 0.88],
            night: false,
            wall_style: WallStyle::DuneBerm,
            scenery: vec![
                (SceneryKind::Rock, 4.0),
                (SceneryKind::Cactus, 3.0),
                (SceneryKind::Mesa, 0.8),
                (SceneryKind::Dune, 1.0),
            ],
            terrain_amp: 2.0,
            terrain_scale: 30.0,
            terrain_rise: 9.0,
            terrain_blend: 7.0,
        },
        laps: 3,
        points,
        shoulder: [3.0, 3.0],
        features,
        seed: 0xB0E1_0009,
    }
}

/// Battle arena 2 (spec 17): night neon, a wide (40 m) square-ish loop round a
/// pillar island ringed with neon pylons; eight boost pads, two kickers, 8 rows
/// of 5 Power Eggs, coins.
fn mempool_plaza() -> TrackDef {
    use Seg::*;
    let segs = [
        S(50.0, 0.0),
        T(90.0, 32.0, 0.0),
        S(50.0, 0.0),
        T(90.0, 32.0, 0.0),
        S(50.0, 0.0),
        T(90.0, 32.0, 0.0),
        S(50.0, 0.0),
        T(90.0, 32.0, 0.0),
    ];
    let (points, g) = turtle(&segs, 40.0, 0.0);
    let corner = std::f32::consts::FRAC_PI_2 * 32.0;
    let mut features = vec![
        Feature::Ramp {
            s: g[2] + 20.0,
            len: 8.0,
            height: 1.3,
            lip: 1.0,
        },
        Feature::Ramp {
            s: g[6] + 20.0,
            len: 8.0,
            height: 1.3,
            lip: 1.0,
        },
    ];
    for k in 0..4 {
        let st = g[2 * k];
        let c = g[2 * k + 1];
        let side = if k % 2 == 0 { 1.0 } else { -1.0 };
        features.push(pad(st + 4.0, 11.0 * side));
        features.push(pad(st + 4.0, -11.0 * side));
        features.push(row(st + 40.0, 5));
        features.push(row(c + 0.5 * corner, 5));
        features.push(coins(st + 8.0, 0.0, 5));
        features.push(coins(c + 0.3 * corner, 13.0 * side, 5));
    }
    TrackDef {
        id: TrackId::MempoolPlaza,
        name: "Mempool Plaza".into(),
        theme: Theme {
            sky_top: [0.02, 0.01, 0.07],
            sky_horizon: [0.14, 0.07, 0.22],
            fog: [0.08, 0.04, 0.14],
            fog_density: 0.005,
            sun_dir: [0.3, 0.6, -0.4],
            sun_colour: [0.45, 0.42, 0.75],
            ambient_sky: [0.22, 0.20, 0.40],
            ambient_ground: [0.20, 0.14, 0.24],
            ground: [0.16, 0.14, 0.22],
            ground_alt: [0.22, 0.18, 0.30],
            road: [0.10, 0.10, 0.15],
            road_stripe: [0.97, 0.58, 0.10],
            shoulder: [0.30, 0.26, 0.40],
            wall: [0.97, 0.58, 0.10],
            curb_a: [0.97, 0.58, 0.10],
            curb_b: [0.55, 0.20, 0.85],
            night: true,
            wall_style: WallStyle::NeonRail,
            scenery: vec![(SceneryKind::NeonPylon, 5.0), (SceneryKind::Rock, 0.6)],
            terrain_amp: 0.4,
            terrain_scale: 40.0,
            terrain_rise: 2.5,
            terrain_blend: 5.0,
        },
        laps: 3,
        points,
        shoulder: [3.0, 3.0],
        features,
        seed: 0x3E3B_000A,
    }
}
