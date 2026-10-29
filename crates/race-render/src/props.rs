//! Instanced drill props: unit meshes, sized by the app to match the sim.

use crate::mesh::{MeshData, Style};
use crate::meshes::MeshId;
use crate::types::{DrillPropInst, DrillPropKind};
use glam::{Mat4, Vec3};
use std::f32::consts::FRAC_PI_2;

pub(crate) fn mesh(kind: DrillPropKind) -> MeshId {
    match kind {
        DrillPropKind::Cone => MeshId::DrillCone,
        DrillPropKind::Pylon => MeshId::DrillPylon,
        DrillPropKind::Log => MeshId::DrillLog,
        DrillPropKind::RingCone => MeshId::DrillRingCone,
        DrillPropKind::Flag => MeshId::MarkerFlag,
        DrillPropKind::Dust => MeshId::TrailDust,
        DrillPropKind::Finish => MeshId::FinishGate,
    }
}

pub(crate) fn model(p: &DrillPropInst) -> Mat4 {
    let mut tilt = 0.0;
    let mut lift = 0.0;
    if matches!(p.kind, DrillPropKind::Cone | DrillPropKind::RingCone) {
        let fall = p.knocked.clamp(0.0, 1.0);
        let radius = p.size.z * 0.5;
        // At rest the cone's side lies on the ground, rather than balancing on its base edge.
        let end = FRAC_PI_2 + radius.atan2(p.size.y);
        tilt = end * fall * fall;
        let (s, c) = tilt.sin_cos();
        // The base edge or the small flat tip is the lowest point as the cone falls forwards (+Z).
        let base = radius * s - (p.size.y * 0.045 * c).min(0.0);
        lift = base.max(-p.size.y * c + p.size.z * 0.025 * s).max(0.0);
    }
    Mat4::from_translation(p.pos + Vec3::Y * lift)
        * Mat4::from_rotation_y(p.yaw)
        * Mat4::from_rotation_x(tilt)
        * Mat4::from_scale(p.size)
}

pub(crate) fn cone(small: bool) -> MeshData {
    let mut b = MeshData::new();
    let orange = Style::rgb(if small { [1.0, 0.63, 0.08] } else { [1.0, 0.32, 0.04] }).flat();
    let white = Style::rgb([0.98, 0.97, 0.9]).flat();
    let rubber = Style::rgb([0.16, 0.17, 0.18]).flat();
    b.cyl(&Mat4::from_scale(Vec3::new(0.5, 0.045, 0.5)), 12, 1.0, 1.0, true, &rubber);
    // A single tapered body split at the reflective stripes, with no overlapping shells.
    let radius = |y: f32| 0.46 + (0.025 - 0.46) * (y - 0.045) / 0.955;
    let cuts = [0.045, 0.28, 0.39, 0.62, 0.73, 1.0];
    for (i, pair) in cuts.windows(2).enumerate() {
        let (y0, y1) = (pair[0], pair[1]);
        let m = Mat4::from_translation(Vec3::Y * y0) * Mat4::from_scale(Vec3::new(1.0, y1 - y0, 1.0));
        b.cyl(&m, 12, radius(y0), radius(y1), i == 4, if i % 2 == 1 { &white } else { &orange });
    }
    b
}

pub(crate) fn pylon() -> MeshData {
    let mut b = MeshData::new();
    let yellow = Style::rgb([1.0, 0.74, 0.07]).flat();
    let dark = Style::rgb([0.2, 0.23, 0.26]).flat();
    let white = Style::rgb([0.98, 0.97, 0.9]).flat();
    for (y0, y1, r0, r1, st) in [
        (0.0, 0.08, 0.5, 0.5, &dark),
        (0.08, 0.44, 0.34, 0.32, &yellow),
        (0.44, 0.54, 0.32, 0.32, &white),
        (0.54, 0.83, 0.32, 0.3, &yellow),
        (0.83, 0.93, 0.3, 0.3, &white),
        (0.93, 1.0, 0.3, 0.2, &yellow),
    ] {
        let m = Mat4::from_translation(Vec3::Y * y0) * Mat4::from_scale(Vec3::new(1.0, y1 - y0, 1.0));
        b.cyl(&m, 12, r0, r1, true, st);
    }
    b
}

pub(crate) fn log() -> MeshData {
    let mut b = MeshData::new();
    let bark = Style::rgb([0.4, 0.23, 0.12]).flat();
    let end = Style::rgb([0.81, 0.61, 0.34]).flat();
    let ring = Style::rgb([0.6, 0.4, 0.22]).flat();
    let m = Mat4::from_translation(Vec3::new(-0.5, 0.5, 0.0))
        * Mat4::from_rotation_z(-FRAC_PI_2) * Mat4::from_scale(Vec3::new(0.5, 1.0, 0.5));
    b.cyl(&m, 12, 1.0, 1.0, false, &bark);
    // End grain, inset into the bark's outer radius. Ends remain inside the physical log length.
    for side in [-1.0f32, 1.0] {
        for (r, inset, thickness, st) in [(0.5, 0.004, 0.001, &end), (0.31, 0.0025, 0.001, &ring), (0.28, 0.001, 0.0005, &end), (0.15, 0.0004, 0.0004, &ring)] {
            let m = Mat4::from_translation(Vec3::new(side * (0.5 - inset), 0.5, 0.0))
                * Mat4::from_rotation_z(-side * FRAC_PI_2) * Mat4::from_scale(Vec3::new(r, thickness, r));
            b.cyl(&m, 12, 1.0, 1.0, true, st);
        }
    }
    b
}

pub(crate) fn flag() -> MeshData {
    let mut b = MeshData::new();
    let pole = Style::rgb([0.94, 0.9, 0.78]).flat();
    let orange = Style::rgb([1.0, 0.47, 0.06]).flat();
    let white = Style::rgb([1.0, 0.98, 0.92]).flat();
    b.cyl(&Mat4::from_scale(Vec3::new(0.025, 1.0, 0.025)), 8, 1.0, 1.0, true, &pole);
    // Cloth has two faces, so a route marker reads when approached from either side.
    for (z, normal) in [(0.006, Vec3::Z), (-0.006, -Vec3::Z)] {
        b.quad4([
            Vec3::new(0.0, 0.94, z), Vec3::new(0.5, 0.94, z),
            Vec3::new(0.4, 0.72, z), Vec3::new(0.0, 0.72, z),
        ], normal, &orange);
        b.quad4([
            Vec3::new(0.04, 0.94, z * 1.1), Vec3::new(0.12, 0.94, z * 1.1),
            Vec3::new(0.12, 0.72, z * 1.1), Vec3::new(0.04, 0.72, z * 1.1),
        ], normal, &white);
    }
    b
}

/// A round flat patch at the top of the unit box, so `size.y` lifts it clear of the ground.
pub(crate) fn dust() -> MeshData {
    let mut b = MeshData::new();
    let m = Mat4::from_translation(Vec3::Y) * Mat4::from_scale(Vec3::new(0.5, 1.0, 0.5));
    b.disc(&m, 20, &Style::rgb([0.64, 0.45, 0.28]).flat());
    b
}

/// The finish gate in the unit box, drawn for a size of about 19 x 6.5 x 10 m: posts at the
/// sides, a banner near the top, a stripe on the ground across the middle and two arrows in
/// the front half pointing +Z.
pub(crate) fn finish() -> MeshData {
    let mut b = MeshData::new();
    let white = Style::rgb([0.98, 0.97, 0.94]).flat();
    let black = Style::rgb([0.08, 0.08, 0.09]).flat();
    let red = Style::rgb([0.86, 0.12, 0.1]).flat();
    // Posts: red and white bands (0.45 m square at the intended size).
    let (pw, pd) = (0.45 / 19.0, 0.45 / 10.0);
    for side in [-1.0f32, 1.0] {
        let x = side * (0.5 - pw * 0.5);
        for k in 0..5 {
            let (y0, y1) = (k as f32 * 0.2, (k + 1) as f32 * 0.2);
            b.aabb(Vec3::new(x - pw * 0.5, y0, -pd * 0.5), Vec3::new(x + pw * 0.5, y1, pd * 0.5), if k % 2 == 0 { &red } else { &white });
        }
    }
    // Chequered banner between the posts, two cells high, both faces.
    let (cols, y0, y1, half) = (16, 0.76, 0.96, 0.007);
    let x0 = -0.5 + pw;
    let cw = (1.0 - 2.0 * pw) / cols as f32;
    b.aabb(Vec3::new(x0, y0, -half * 0.9), Vec3::new(-x0, y1, half * 0.9), &black);
    for row in 0..2 {
        for col in 0..cols {
            if (row + col) % 2 == 1 {
                continue;
            }
            let (xa, xb) = (x0 + col as f32 * cw, x0 + (col + 1) as f32 * cw);
            let (ya, yb) = (y0 + row as f32 * (y1 - y0) * 0.5, y0 + (row + 1) as f32 * (y1 - y0) * 0.5);
            for z in [half, -half] {
                b.quad4([Vec3::new(xa, ya, z), Vec3::new(xb, ya, z), Vec3::new(xb, yb, z), Vec3::new(xa, yb, z)], Vec3::Z * z, &white);
            }
        }
    }
    // The line on the ground: 2 m deep, chequered, just above the ground.
    let (gy, gz) = (0.007, 0.1);
    for row in 0..2 {
        for col in 0..cols {
            let (xa, xb) = (x0 + col as f32 * cw, x0 + (col + 1) as f32 * cw);
            let (za, zb) = (-gz + row as f32 * gz, row as f32 * gz);
            let st = if (row + col) % 2 == 0 { &white } else { &black };
            b.quad4([Vec3::new(xa, gy, za), Vec3::new(xb, gy, za), Vec3::new(xb, gy, zb), Vec3::new(xa, gy, zb)], Vec3::Y, st);
        }
    }
    // Two arrows beyond the line, the way through: 4.6 m wide, arms 0.8 m thick.
    let (w, t) = (2.3 / 19.0, 0.08);
    for tip in [0.3f32, 0.47] {
        let back = tip - 0.2;
        for side in [-1.0f32, 1.0] {
            let p = [
                Vec3::new(0.0, gy, tip),
                Vec3::new(side * w, gy, back),
                Vec3::new(side * w, gy, back - t),
                Vec3::new(0.0, gy, tip - t),
            ];
            b.quad4(p, Vec3::Y, &white);
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(kind: DrillPropKind, size: Vec3) -> DrillPropInst {
        DrillPropInst { kind, size, pos: Vec3::new(12.0, 3.0, -5.0), yaw: 0.0, knocked: 0.0 }
    }

    #[test]
    fn cones_fall_in_the_heading_direction_and_stay_on_the_ground() {
        for (kind, size) in [
            (DrillPropKind::Cone, Vec3::new(1.2, 2.6, 1.2)),
            (DrillPropKind::RingCone, Vec3::new(0.7, 1.2, 0.7)),
        ] {
            let mesh = crate::meshes::build_mesh(mesh(kind));
            for yaw in [0.0, FRAC_PI_2, -1.3] {
                let mut p = prop(kind, size);
                p.yaw = yaw;
                for k in 0..=20 {
                    p.knocked = k as f32 / 20.0;
                    let m = model(&p);
                    let low = mesh.verts.iter().map(|v| m.transform_point3(Vec3::from_array(v.pos)).y).fold(f32::INFINITY, f32::min);
                    assert!((low - p.pos.y).abs() < 0.001, "{kind:?}, fall {}: lowest {low}", p.knocked);
                }
                let tip = model(&p).transform_point3(Vec3::Y) - p.pos;
                let heading = Vec3::new(yaw.sin(), 0.0, yaw.cos());
                assert!(tip.dot(heading) > size.y * 0.9);
                assert!(tip.y < 0.1, "a fallen cone rests along the ground");
                let down = model(&p);
                p.knocked = 2.0;
                assert_eq!(model(&p), down, "finished fall stays down");
            }
        }
    }

    #[test]
    fn logs_match_the_physical_span_and_lie_across_the_heading() {
        let mut p = prop(DrillPropKind::Log, Vec3::new(12.0, 0.8, 0.8));
        p.yaw = FRAC_PI_2;
        p.knocked = 1.0;
        let m = model(&p);
        let mut lo = Vec3::splat(f32::INFINITY);
        let mut hi = Vec3::splat(f32::NEG_INFINITY);
        for v in log().verts {
            let pos = m.transform_point3(Vec3::from_array(v.pos));
            lo = lo.min(pos);
            hi = hi.max(pos);
        }
        assert!((hi.z - lo.z - 12.0).abs() < 0.001);
        assert!((hi.y - lo.y - 0.8).abs() < 0.001);
        assert!((lo.y - p.pos.y).abs() < 0.001);
        assert!((hi.x - lo.x - 0.8).abs() < 0.001);
    }

    #[test]
    fn standing_props_have_grounded_bases_and_fit_the_supplied_size() {
        for kind in [DrillPropKind::Cone, DrillPropKind::Pylon, DrillPropKind::Log, DrillPropKind::RingCone, DrillPropKind::Flag, DrillPropKind::Finish] {
            let p = prop(kind, Vec3::new(1.6, 4.0, 1.6));
            let m = model(&p);
            let mut low = f32::INFINITY;
            let mut high = f32::NEG_INFINITY;
            for v in crate::meshes::build_mesh(mesh(kind)).verts {
                let q = m.transform_point3(Vec3::from_array(v.pos)) - p.pos;
                assert!(q.x.abs() <= p.size.x * 0.5 + 0.001);
                assert!(q.z.abs() <= p.size.z * 0.5 + 0.001);
                low = low.min(q.y);
                high = high.max(q.y);
            }
            assert!(low.abs() < 0.001, "{kind:?} base {low}");
            assert!((high - p.size.y).abs() < 0.001, "{kind:?} height {high}");
        }
    }
}
