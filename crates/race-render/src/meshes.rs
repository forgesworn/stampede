//! The shared store of instanced meshes (ostrich parts, accessories, coins,
//! eggs, targets, hazards, shield) in one vertex and one index buffer.
//! Indices are absolute, so draws never need a base vertex (GLES 3 safe).

use crate::mesh::{limb, MeshData, Style, Vertex};
use crate::ostrich as o;
use glam::{Mat4, Vec3};
use std::f32::consts::{PI, TAU};
use wgpu::util::DeviceExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum MeshId {
    Body,
    Tail,
    WingL,
    WingR,
    NeckSeg,
    Ball,
    Head,
    Beak,
    Thigh,
    Shin,
    Foot,
    Glow,
    Scarf,
    ScarfTail,
    Goggles,
    Crown,
    StarHeadband,
    VikingHelmet,
    Mohawk,
    Shades,
    Chain,
    Cap,
    Coin,
    Egg,
    HunterEgg,
    TargetEgg,
    CactusHazard,
    Shield,
    DrillCone,
    DrillPylon,
    DrillLog,
    DrillRingCone,
    Flame,
    SmallBall,
    Block,
    Shell,
    JockeyHead,
    JockeyHelmet,
    Jersey,
    Saddle,
    LaptopBase,
    LaptopLid,
    CakeSlice,
    Plate,
    FrogFoot,
    SkinBall,
    RoseBloom,
    Basket,
    MarkerFlag,
    TrailDust,
    FinishGate,
}

pub const MESH_COUNT: usize = MeshId::FinishGate as usize + 1;

pub(crate) const ALL: [MeshId; MESH_COUNT] = [
    MeshId::Body,
    MeshId::Tail,
    MeshId::WingL,
    MeshId::WingR,
    MeshId::NeckSeg,
    MeshId::Ball,
    MeshId::Head,
    MeshId::Beak,
    MeshId::Thigh,
    MeshId::Shin,
    MeshId::Foot,
    MeshId::Glow,
    MeshId::Scarf,
    MeshId::ScarfTail,
    MeshId::Goggles,
    MeshId::Crown,
    MeshId::StarHeadband,
    MeshId::VikingHelmet,
    MeshId::Mohawk,
    MeshId::Shades,
    MeshId::Chain,
    MeshId::Cap,
    MeshId::Coin,
    MeshId::Egg,
    MeshId::HunterEgg,
    MeshId::TargetEgg,
    MeshId::CactusHazard,
    MeshId::Shield,
    MeshId::DrillCone,
    MeshId::DrillPylon,
    MeshId::DrillLog,
    MeshId::DrillRingCone,
    MeshId::Flame,
    MeshId::SmallBall,
    MeshId::Block,
    MeshId::Shell,
    MeshId::JockeyHead,
    MeshId::JockeyHelmet,
    MeshId::Jersey,
    MeshId::Saddle,
    MeshId::LaptopBase,
    MeshId::LaptopLid,
    MeshId::CakeSlice,
    MeshId::Plate,
    MeshId::FrogFoot,
    MeshId::SkinBall,
    MeshId::RoseBloom,
    MeshId::Basket,
    MeshId::MarkerFlag,
    MeshId::TrailDust,
    MeshId::FinishGate,
];

pub fn build_mesh(id: MeshId) -> MeshData {
    match id {
        MeshId::JockeyHead => crate::assets::head(),
        MeshId::JockeyHelmet => crate::assets::helmet(),
        MeshId::Jersey => crate::assets::jersey(),
        MeshId::Saddle => crate::assets::saddle(),
        MeshId::LaptopBase => crate::assets::laptop_base(),
        MeshId::LaptopLid => crate::assets::laptop_lid(),
        MeshId::CakeSlice => crate::assets::cake(),
        MeshId::Plate => crate::assets::plate(),
        MeshId::FrogFoot => crate::assets::frog_foot(),
        MeshId::RoseBloom => crate::assets::rose_bloom(),
        MeshId::Basket => crate::assets::basket(),

        MeshId::SkinBall => {
            let mut b = MeshData::new();
            b.sphere(&Mat4::IDENTITY, 10, 6, &Style::tint0(1.0).sheen(0.22).ao(0.12));
            b
        }
        MeshId::Flame => {
            let mut b = MeshData::new();
            let profile = [(0.0, 0.0), (0.28, 0.12), (0.23, 0.34), (0.13, 0.65), (0.0, 1.0)];
            b.lathe(&Mat4::IDENTITY, 7, &profile, &Style::tint0(1.0).emit(0.4));
            b.lathe(&(Mat4::from_translation(Vec3::new(0.0, 0.02, 0.13)) * Mat4::from_scale(Vec3::new(0.6, 0.6, 0.6))), 6, &profile, &Style::col(1.0, 0.72, 0.04).emit(0.3));
            b
        }
        MeshId::SmallBall => o::build_ball(5, 3, 0.0),
        MeshId::Block => { let mut b = MeshData::new(); b.cube(&Mat4::IDENTITY, &Style::tint0(1.0)); b }
        MeshId::Shell => {
            let mut b = MeshData::new();
            b.lathe(&Mat4::IDENTITY, 9, &[(0.0, 0.5), (0.7, 0.3), (1.0, 0.0)], &Style::tint0(1.0));
            b
        }
        MeshId::Body => o::build_body(),
        MeshId::Tail => o::build_tail(),
        MeshId::WingL => o::build_wing(1.0),
        MeshId::WingR => o::build_wing(-1.0),
        MeshId::NeckSeg => o::build_neck_seg(),
        MeshId::Ball => o::build_ball(8, 5, 0.0),
        MeshId::Glow => o::build_ball(8, 5, 1.0),
        MeshId::Head => o::build_head(),
        MeshId::Beak => o::build_beak(),
        MeshId::Thigh => o::build_thigh(),
        MeshId::Shin => o::build_shin(),
        MeshId::Foot => o::build_foot(),
        MeshId::Scarf => o::build_scarf(),
        MeshId::ScarfTail => o::build_scarf_tail(),
        MeshId::Goggles => o::build_goggles(),
        MeshId::Crown => o::build_crown(),
        MeshId::StarHeadband => o::build_star_headband(),
        MeshId::VikingHelmet => o::build_viking(),
        MeshId::Mohawk => o::build_mohawk(),
        MeshId::Shades => o::build_shades(),
        MeshId::Chain => o::build_chain(),
        MeshId::Cap => o::build_cap(),
        MeshId::Coin => build_coin(),
        MeshId::Egg => build_egg(false),
        MeshId::HunterEgg => build_egg(true),
        MeshId::TargetEgg => build_target_egg(),
        MeshId::CactusHazard => build_cactus_hazard(),
        MeshId::DrillCone => crate::props::cone(false),
        MeshId::DrillPylon => crate::props::pylon(),
        MeshId::DrillLog => crate::props::log(),
        MeshId::DrillRingCone => crate::props::cone(true),
        MeshId::MarkerFlag => crate::props::flag(),
        MeshId::TrailDust => crate::props::dust(),
        MeshId::FinishGate => crate::props::finish(),
        MeshId::Shield => {
            let mut b = MeshData::new();
            b.sphere(&Mat4::IDENTITY, 20, 14, &Style::tint0(1.0));
            b
        }
    }
}

// ----------------------------------------------------------------- coin ----

/// Raised stroke along a polyline on the plane z = `z` (flat quads, no sides).
fn stroke(b: &mut MeshData, pts: &[(f32, f32)], w: f32, z: f32, flip_x: bool, st: &Style) {
    for seg in pts.windows(2) {
        let sx = if flip_x { -1.0 } else { 1.0 };
        let a = Vec3::new(seg[0].0 * sx, seg[0].1, z);
        let c = Vec3::new(seg[1].0 * sx, seg[1].1, z);
        let dir = c - a;
        let len = dir.length();
        if len < 1e-5 {
            continue;
        }
        let u = dir / len;
        let n = Vec3::Z.cross(u).normalize() * (w * 0.5);
        let e = u * (w * 0.4);
        b.quad4([a - e + n, a - e - n, c + e - n, c + e + n], Vec3::Z * z.signum(), st);
    }
}

/// The raised bitcoin sign (both faces), about 0.56 tall and 0.3 wide, centred near the origin, at z = +-0.092.
pub(crate) fn btc_glyph(b: &mut MeshData, st: &Style) {
    let w = 0.052;
    let stem = [(-0.09, -0.2), (-0.09, 0.2)];
    let top = [(-0.09, 0.2), (0.06, 0.2), (0.14, 0.14), (0.14, 0.08), (0.06, 0.012), (-0.09, 0.012)];
    let bot = [(-0.09, 0.012), (0.09, 0.012), (0.18, -0.06), (0.18, -0.13), (0.09, -0.2), (-0.09, -0.2)];
    let tick_a = [(-0.03, 0.2), (-0.03, 0.28)];
    let tick_b = [(0.05, 0.2), (0.05, 0.28)];
    let tick_c = [(-0.03, -0.2), (-0.03, -0.28)];
    let tick_d = [(0.05, -0.2), (0.05, -0.28)];
    for (z, flip) in [(0.092, false), (-0.092, true)] {
        let xs = |pts: &[(f32, f32)]| pts.iter().map(|p| (p.0 - 0.02, p.1)).collect::<Vec<_>>();
        for pts in [&stem[..], &top[..], &bot[..], &tick_a[..], &tick_b[..], &tick_c[..], &tick_d[..]] {
            stroke(b, &xs(pts), w, z, flip, st);
        }
    }
}

fn build_coin() -> MeshData {
    let mut b = MeshData::new();
    let rim = Style::col(0.80, 0.56, 0.08).sheen(0.65);
    let face = Style::col(1.0, 0.80, 0.26).sheen(0.80);
    // Axis along Z: rotate the +Y cylinders.
    let to_z = Mat4::from_rotation_x(PI * 0.5);
    b.cyl(&(Mat4::from_translation(Vec3::new(0.0, 0.0, -0.06)) * to_z * Mat4::from_scale(Vec3::new(0.52, 0.12, 0.52))), 16, 1.0, 1.0, true, &rim);
    b.cyl(&(Mat4::from_translation(Vec3::new(0.0, 0.0, -0.075)) * to_z * Mat4::from_scale(Vec3::new(0.42, 0.15, 0.42))), 16, 1.0, 1.0, true, &face);
    // The raised B with its two vertical ticks, on both faces.
    let glyph = Style::col(0.88, 0.45, 0.02).sheen(0.5);
    let w = 0.052;
    let stem = [(-0.09, -0.2), (-0.09, 0.2)];
    let top = [(-0.09, 0.2), (0.06, 0.2), (0.14, 0.14), (0.14, 0.08), (0.06, 0.012), (-0.09, 0.012)];
    let bot = [(-0.09, 0.012), (0.09, 0.012), (0.18, -0.06), (0.18, -0.13), (0.09, -0.2), (-0.09, -0.2)];
    let tick_a = [(-0.03, 0.2), (-0.03, 0.28)];
    let tick_b = [(0.05, 0.2), (0.05, 0.28)];
    let tick_c = [(-0.03, -0.2), (-0.03, -0.28)];
    let tick_d = [(0.05, -0.2), (0.05, -0.28)];
    for (z, flip) in [(0.092, false), (-0.092, true)] {
        let xs = |pts: &[(f32, f32)]| pts.iter().map(|p| (p.0 - 0.02, p.1)).collect::<Vec<_>>();
        for pts in [&stem[..], &top[..], &bot[..], &tick_a[..], &tick_b[..], &tick_c[..], &tick_d[..]] {
            stroke(&mut b, &xs(pts), w, z, flip, &glyph);
        }
    }
    b
}

// ----------------------------------------------------------------- eggs ----

fn egg_profile(n: u32) -> Vec<(f32, f32)> {
    // y from -0.5 .. 0.5, fatter at the bottom.
    let mut v = vec![(0.0, -0.5)];
    for i in 1..n {
        let t = i as f32 / n as f32;
        let y = -0.5 + t;
        let r = 0.36 * (1.0 - (2.0 * y).powi(2)).max(0.0).sqrt() * (1.0 - 0.18 * (2.0 * y));
        v.push((r, y));
    }
    v.push((0.0, 0.5));
    v
}

fn build_egg(hunter: bool) -> MeshData {
    let mut b = MeshData::new();
    // Long axis along Z (rolling forward).
    let to_z = Mat4::from_rotation_x(PI * 0.5);
    let scale = Mat4::from_scale(Vec3::splat(1.6));
    if !hunter {
        b.lathe(&(scale * to_z), 14, &egg_profile(10), &Style::col(0.98, 0.94, 0.82).ao(0.1));
        for i in 0..9 {
            let a = TAU * (i as f32 * 0.618);
            let y = -0.3 + 0.6 * ((i * 5 % 9) as f32 / 9.0);
            let r = 0.36 * (1.0 - (2.0 * y).powi(2)).max(0.0).sqrt() * (1.0 - 0.18 * 2.0 * y);
            let p = Vec3::new(a.cos() * r, y, a.sin() * r) * 0.99;
            b.sphere(&(scale * to_z * Mat4::from_translation(p) * Mat4::from_scale(Vec3::new(0.07, 0.035, 0.07))), 5, 4, &Style::col(0.55, 0.38, 0.22));
        }
    } else {
        b.lathe(&(scale * to_z), 14, &egg_profile(10), &Style::col(0.50, 0.07, 0.10).ao(0.15));
        // Golden spikes ringing the middle, glowing eye band.
        for i in 0..8 {
            let a = TAU * i as f32 / 8.0;
            let p = Vec3::new(a.cos() * 0.34, 0.05, a.sin() * 0.34);
            let m = scale * to_z * limb(p, p * 1.45 + Vec3::Y * 0.05, 0.07);
            b.cone(&m, 5, &Style::col(1.0, 0.8, 0.2).emit(0.3));
        }
        b.torus(&(scale * to_z * Mat4::from_translation(Vec3::new(0.0, -0.18, 0.0))), 16, 4, 0.33, 0.03, &Style::col(1.0, 0.15, 0.1).emit(0.9));
    }
    b
}

fn build_target_egg() -> MeshData {
    let mut b = MeshData::new();
    // Upright golden Power Egg with a bitcoin-orange belt.
    let s = Mat4::from_scale(Vec3::splat(2.0));
    b.lathe(&s, 16, &egg_profile(12), &Style::tint0(1.0).sheen(0.45).ao(0.12));
    b.torus(&(s * Mat4::from_translation(Vec3::new(0.0, -0.04, 0.0))), 20, 5, 0.34, 0.035, &Style::col(1.0, 0.5, 0.05).emit(0.18));
    // A raised ₿-ish bar and dot cluster on both sides.
    let glyph = Style::col(1.0, 0.45, 0.02).sheen(0.35);
    for sgn in [1.0f32, -1.0] {
        let m = Mat4::from_rotation_y(if sgn > 0.0 { 0.0 } else { PI });
        b.cube(&(m * Mat4::from_translation(Vec3::new(0.0, 0.1, 0.70)) * Mat4::from_scale(Vec3::new(0.09, 0.42, 0.06))), &glyph);
        b.cube(&(m * Mat4::from_translation(Vec3::new(0.14, 0.26, 0.68)) * Mat4::from_scale(Vec3::new(0.2, 0.08, 0.06))), &glyph);
        b.cube(&(m * Mat4::from_translation(Vec3::new(0.16, 0.08, 0.68)) * Mat4::from_scale(Vec3::new(0.24, 0.08, 0.06))), &glyph);
        b.cube(&(m * Mat4::from_translation(Vec3::new(0.14, -0.1, 0.68)) * Mat4::from_scale(Vec3::new(0.2, 0.08, 0.06))), &glyph);
    }
    b
}

fn build_cactus_hazard() -> MeshData {
    let mut b = MeshData::new();
    // Barrel cactus, base at y=0.
    let green = Style::col(0.16, 0.55, 0.24).ao(0.35).flat();
    b.lathe(
        &Mat4::from_scale(Vec3::new(1.0, 1.0, 1.0)),
        10,
        &[(0.0, 0.0), (0.38, 0.03), (0.5, 0.3), (0.47, 0.6), (0.3, 0.82), (0.0, 0.88)],
        &green,
    );
    // Ribs.
    for i in 0..10 {
        let a = TAU * i as f32 / 10.0;
        let base = Vec3::new(a.cos() * 0.46, 0.3, a.sin() * 0.46);
        b.cube(&(Mat4::from_translation(base) * Mat4::from_rotation_y(-a) * Mat4::from_scale(Vec3::new(0.04, 0.5, 0.05))), &Style::col(0.1, 0.42, 0.18));
    }
    // Spines.
    for i in 0..16 {
        let a = TAU * (i as f32 * 0.382);
        let y = 0.12 + 0.6 * ((i * 7 % 16) as f32 / 16.0);
        let r = if y > 0.6 { 0.34 } else { 0.5 };
        let n = Vec3::new(a.cos(), 0.15, a.sin()).normalize();
        let base = Vec3::new(a.cos() * r, y, a.sin() * r);
        b.cone(&limb(base, base + n * 0.2, 0.025), 4, &Style::col(0.98, 0.95, 0.85));
    }
    // Warning flower.
    b.sphere(&(Mat4::from_translation(Vec3::new(0.0, 0.9, 0.0)) * Mat4::from_scale(Vec3::new(0.16, 0.1, 0.16))), 8, 5, &Style::col(1.0, 0.2, 0.3).emit(0.4));
    b.sphere(&(Mat4::from_translation(Vec3::new(0.0, 0.96, 0.0)) * Mat4::from_scale(Vec3::splat(0.07))), 6, 4, &Style::col(1.0, 0.85, 0.1).emit(0.5));
    b
}

// ---------------------------------------------------------------- store ----

pub struct MeshStore {
    pub vb: wgpu::Buffer,
    pub ib: wgpu::Buffer,
    /// (first_index, index_count) per `MeshId`.
    pub ranges: [(u32, u32); MESH_COUNT],
}

impl MeshStore {
    pub fn new(device: &wgpu::Device) -> MeshStore {
        let mut verts: Vec<Vertex> = Vec::new();
        let mut idx: Vec<u32> = Vec::new();
        let mut ranges = [(0u32, 0u32); MESH_COUNT];
        for id in ALL {
            let m = build_mesh(id);
            let base = verts.len() as u32;
            let first = idx.len() as u32;
            verts.extend_from_slice(&m.verts);
            idx.extend(m.idx.iter().map(|i| i + base));
            ranges[id as usize] = (first, m.idx.len() as u32);
        }
        let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("meshstore_vb"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("meshstore_ib"),
            contents: bytemuck::cast_slice(&idx),
            usage: wgpu::BufferUsages::INDEX,
        });
        MeshStore { vb, ib, ranges }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ostrich::ostrich_parts;
    use crate::types::*;

    #[test]
    fn every_mesh_is_valid() {
        let mut total = 0;
        for id in ALL {
            let m = build_mesh(id);
            assert!(!m.idx.is_empty(), "{id:?} empty");
            assert_eq!(m.idx.len() % 3, 0);
            for &i in &m.idx {
                assert!((i as usize) < m.verts.len(), "{id:?} index out of range");
            }
            for v in &m.verts {
                assert!(v.pos.iter().all(|c| c.is_finite()), "{id:?} non-finite");
                let n = Vec3::new(v.nrm[0] as f32, v.nrm[1] as f32, v.nrm[2] as f32) / 127.0;
                assert!(n.length() > 0.9 && n.length() < 1.1, "{id:?} bad normal {n:?}");
            }
            total += m.tri_count();
            assert!(m.tri_count() < 6000, "{id:?} too heavy: {}", m.tri_count());
        }
        assert!(total < 40_000, "mesh store too big: {total}");
    }

    #[test]
    fn ostrich_triangle_budget() {
        let p = OstrichPose { accessory: Accessory::VikingHelmet, laser_on: true, ..Default::default() };
        let tris: usize = ostrich_parts(&p, 0.0)
            .iter()
            .map(|part| build_mesh(part.mesh).tri_count())
            .sum();
        assert!(tris < 5500, "ostrich has {tris} triangles");
    }
}

