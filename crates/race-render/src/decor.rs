//! Trackside decoration: the kit of extra scenery meshes (static ones are baked
//! into the world chunks with the rest of the scenery) and the *animated* decor
//! (spectators, flags, banners, water, lava, the start lights), which lives in a
//! second chunked vertex buffer drawn by the `decor` pipeline. That pipeline
//! animates in the vertex shader from a per-vertex `anim` word, so a bobbing crowd
//! or a lava lake costs no CPU per frame, no instance buffer and no extra pass.
//!
//! `anim = [kind, a, b, c]` (all unorm8):
//! * 0 static (lit like any mesh)
//! * 1 bob: a = phase, b = height fraction (0 feet .. 1 head: how much the vertex lifts)
//! * 2 flutter: a = phase, b = distance from the pole (0..1), c = facing yaw / tau
//! * 3 water: a, b, c = shallow colour (sRGB), the vertex colour is the deep colour
//! * 4 lava: a, b, c = hot colour (sRGB), the vertex colour is the crust colour
//! * 5 start light: a = (index + 0.5) / 5
//! * 6 pulse: a = phase (emissive vertices breathe)

use crate::color::{lerp3, mul3};
use crate::mesh::{hash01, limb, MeshData, Style};
use crate::types::*;
use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use std::f32::consts::{PI, TAU};

#[allow(dead_code)]
pub const A_STATIC: u8 = 0;
pub const A_BOB: u8 = 1;
pub const A_FLUTTER: u8 = 2;
pub const A_WATER: u8 = 3;
pub const A_LAVA: u8 = 4;
pub const A_LIGHT: u8 = 5;
pub const A_PULSE: u8 = 6;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct DecorVertex {
    pub pos: [f32; 3],
    pub nrm: [i8; 4],
    pub col: [u8; 4],
    pub anim: [u8; 4],
}

pub const DECOR_ATTRS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Snorm8x4, 2 => Unorm8x4, 3 => Unorm8x4];

pub fn decor_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<DecorVertex>() as u64, step_mode: wgpu::VertexStepMode::Vertex, attributes: &DECOR_ATTRS }
}

fn u8f(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[derive(Clone, Debug, Default)]
pub struct DecorData {
    pub verts: Vec<DecorVertex>,
    pub idx: Vec<u32>,
}

impl DecorData {
    /// Append `m` transformed by `xf`, colours scaled by `shade`; `anim(local position, emissive)` gives each vertex's anim word.
    pub fn append(&mut self, m: &MeshData, xf: &Mat4, shade: f32, anim: impl Fn(Vec3, f32) -> [u8; 4]) {
        let base = self.verts.len() as u32;
        let nm = crate::mesh::normal_matrix(xf);
        for v in &m.verts {
            let lp = Vec3::from_array(v.pos);
            let p = xf.transform_point3(lp);
            let n = nm * Vec3::new(v.nrm[0] as f32, v.nrm[1] as f32, v.nrm[2] as f32);
            let n = n.normalize_or_zero() * 127.0;
            let sh = |c: u8| ((c as f32 * shade).clamp(0.0, 255.0)) as u8;
            self.verts.push(DecorVertex {
                pos: p.to_array(),
                nrm: [n.x as i8, n.y as i8, n.z as i8, v.nrm[3]],
                col: [sh(v.col[0]), sh(v.col[1]), sh(v.col[2]), 255],
                anim: anim(lp, v.nrm[3].max(0) as f32 / 127.0),
            });
        }
        let flip = xf.determinant() < 0.0;
        for t in m.idx.chunks_exact(3) {
            if flip {
                self.idx.extend_from_slice(&[base + t[0], base + t[2], base + t[1]]);
            } else {
                self.idx.extend_from_slice(&[base + t[0], base + t[1], base + t[2]]);
            }
        }
    }

    pub fn bounds(&self) -> (Vec3, Vec3) {
        let mut lo = Vec3::splat(f32::MAX);
        let mut hi = Vec3::splat(f32::MIN);
        for v in &self.verts {
            let p = Vec3::from_array(v.pos);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        (lo, hi)
    }
}

fn st(c: [f32; 3]) -> Style {
    Style::rgb(c).flat().ao(0.3)
}

fn tr(p: Vec3) -> Mat4 {
    Mat4::from_translation(p)
}

fn sc(s: Vec3) -> Mat4 {
    Mat4::from_scale(s)
}

// ------------------------------------------------------------------- kit ----

/// The extra static meshes and the animated sources, built once per world from its palette.
pub struct Kit2 {
    pub baobab: MeshData,
    pub palm: MeshData,
    pub reed: MeshData,
    pub hay: MeshData,
    pub barrel: MeshData,
    pub tyres: MeshData,
    pub tent: MeshData,
    pub bones: MeshData,
    pub hoodoo: MeshData,
    pub lava_rock: MeshData,
    pub smoke: MeshData,
    pub lily: MeshData,
    pub spectators: Vec<MeshData>,
    pub flag: MeshData,
    pub check_flag: MeshData,
    pub banner: MeshData,
    pub btc_sign: MeshData,
    pub billboard: MeshData,
    pub towers: Vec<MeshData>,
    pub stands: MeshData,
}

pub fn build_kit2(pal: &Palette) -> Kit2 {
    let night = pal.night;
    let leaf = if night { [0.13, 0.2, 0.22] } else { lerp3([0.28, 0.46, 0.16], pal.ground_alt, 0.25) };
    Kit2 {
        baobab: baobab(leaf),
        palm: palm(night),
        reed: reed(night),
        hay: hay(),
        barrel: barrel(pal),
        tyres: tyres(pal),
        tent: tent(pal),
        bones: bones(),
        hoodoo: hoodoo(pal),
        lava_rock: lava_rock(),
        smoke: smoke(),
        lily: lily(),
        spectators: (0..6).map(spectator).collect(),
        flag: flag(1.8, false),
        check_flag: flag(1.8, true),
        banner: banner(),
        btc_sign: btc_sign(pal),
        billboard: billboard(pal),
        towers: (0..4).map(|k| tower(pal, k)).collect(),
        stands: stands(pal),
    }
}

fn stands(pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    let conc = [0.72, 0.72, 0.76];
    for k in 0..3 {
        let (z0, z1) = (-1.4 * (k + 1) as f32, -1.4 * k as f32);
        let y = 0.5 * (k + 1) as f32;
        b.aabb(Vec3::new(-4.0, 0.0, z0), Vec3::new(4.0, y, z1), &st(mul3(conc, 0.88 + 0.08 * k as f32)));
        // A lighter edge strip on each step.
        b.aabb(Vec3::new(-4.0, y - 0.04, z1 - 0.12), Vec3::new(4.0, y + 0.01, z1), &Style::rgb(mul3(conc, 1.15)).flat());
    }
    let orange = pal.curb_a;
    // Back panel with a bitcoin sign, roof and posts.
    b.aabb(Vec3::new(-4.0, 1.5, -4.35), Vec3::new(4.0, 4.0, -4.2), &st(orange));
    let mut g = MeshData::new();
    crate::meshes::btc_glyph(&mut g, &Style::rgb([1.0, 0.98, 0.92]).flat());
    for x in [-2.0f32, 2.0] {
        b.append(&g, &(tr(Vec3::new(x, 2.75, -4.17)) * sc(Vec3::new(2.4, 2.4, 0.3))));
    }
    for x in [-4.0f32, 4.0] {
        b.aabb(Vec3::new(x - 0.07, 0.0, -4.3), Vec3::new(x + 0.07, 4.3, -4.15), &st([0.2, 0.2, 0.26]));
        b.aabb(Vec3::new(x - 0.07, 1.5, 0.05), Vec3::new(x + 0.07, 4.3, 0.19), &st([0.2, 0.2, 0.26]));
    }
    b.aabb(Vec3::new(-4.15, 4.3, -4.5), Vec3::new(4.15, 4.45, 0.3), &st([0.16, 0.16, 0.2]));
    b.aabb(Vec3::new(-4.15, 4.45, 0.0), Vec3::new(4.15, 4.6, 0.3), &Style::rgb(orange).flat());
    b
}

fn baobab(leaf: [f32; 3]) -> MeshData {
    let mut b = MeshData::new();
    let bark = [0.52, 0.43, 0.36];
    // A fat bottle trunk.
    b.lathe(
        &Mat4::IDENTITY,
        10,
        &[(0.0, -0.2), (1.35, -0.2), (1.5, 0.3), (1.25, 1.5), (0.95, 3.0), (0.85, 4.2), (1.05, 4.9), (0.9, 5.3), (0.0, 5.3)],
        &st(bark),
    );
    // Short thick limbs, each with a leaf pad.
    for k in 0..5 {
        let a = k as f32 * TAU / 5.0 + 0.4;
        let (dx, dz) = (a.cos(), a.sin());
        let r = 1.9 + 0.5 * hash01(k, 1, 5);
        let top = Vec3::new(dx * r, 6.3 + 0.7 * hash01(k, 2, 5), dz * r);
        b.cyl(&limb(Vec3::new(dx * 0.3, 4.8, dz * 0.3), top, 0.3), 5, 1.0, 0.55, true, &st(bark));
        let lc = mul3(leaf, 0.9 + 0.3 * hash01(k, 3, 5));
        b.sphere_jit(&(tr(top + Vec3::Y * 0.2) * sc(Vec3::new(1.5, 0.7, 1.5))), 7, 4, &st(lc), 0.12, k + 3);
    }
    b.sphere_jit(&(tr(Vec3::new(0.0, 6.6, 0.0)) * sc(Vec3::new(1.8, 0.8, 1.8))), 7, 4, &st(mul3(leaf, 1.15)), 0.1, 11);
    b
}

fn palm(night: bool) -> MeshData {
    let mut b = MeshData::new();
    let bark = if night { [0.2, 0.17, 0.2] } else { [0.5, 0.36, 0.22] };
    let frond = if night { [0.14, 0.3, 0.26] } else { [0.2, 0.52, 0.2] };
    // Leaning trunk in five segments.
    let pt = |t: f32| Vec3::new(0.8 * t * t, 5.4 * t, 0.12 * t);
    for i in 0..5 {
        let (a, c) = (pt(i as f32 / 5.0), pt((i + 1) as f32 / 5.0));
        let col = if i % 2 == 0 { bark } else { mul3(bark, 0.82) };
        b.cyl(&limb(a, c, 0.26 - 0.02 * i as f32), 6, 1.0, 0.9, i == 0, &st(col));
    }
    let top = pt(1.0);
    for k in 0..4 {
        let a = k as f32 * 1.7 + 0.3;
        b.sphere(&(tr(top + Vec3::new(a.cos() * 0.15, -0.15, a.sin() * 0.15)) * sc(Vec3::splat(0.14))), 5, 3, &st([0.32, 0.2, 0.1]));
    }
    // Eight drooping fronds: a tapering ribbon along an arc, seen from both sides.
    for k in 0..8 {
        let a = k as f32 * TAU / 8.0 + 0.2;
        let (dx, dz) = (a.cos(), a.sin());
        let len = 2.3 + 0.4 * hash01(k, 7, 3);
        let col = mul3(frond, 0.85 + 0.3 * hash01(k, 8, 3));
        let curve = |u: f32| top + Vec3::new(dx * u * len, (u * 0.9 - u * u * 1.7) * 1.0 + 0.25, dz * u * len);
        let side = Vec3::new(-dz, 0.0, dx);
        let n = 4;
        for i in 0..n {
            let (u0, u1) = (i as f32 / n as f32, (i + 1) as f32 / n as f32);
            let (w0, w1) = (0.42 * (1.0 - u0 * 0.8), 0.42 * (1.0 - u1 * 0.8));
            let q = [curve(u0) + side * w0, curve(u0) - side * w0, curve(u1) - side * w1, curve(u1) + side * w1];
            let cc = mul3(col, 0.85 + 0.25 * u0);
            b.quad4(q, Vec3::Y, &Style::rgb(cc).flat());
            b.quad4(q, -Vec3::Y, &Style::rgb(mul3(cc, 0.8)).flat());
        }
    }
    b
}

fn reed(night: bool) -> MeshData {
    let mut b = MeshData::new();
    let g = if night { [0.14, 0.26, 0.2] } else { [0.45, 0.55, 0.2] };
    for k in 0..10 {
        let a = k as f32 * 2.4;
        let h = 1.1 + 0.8 * hash01(k, 3, 9);
        let lean = Vec3::new(a.cos(), 0.0, a.sin()) * (0.1 + 0.28 * hash01(k, 4, 9));
        let base = Vec3::new(a.cos() * 0.12, 0.0, a.sin() * 0.12);
        b.cone(&limb(base, base + Vec3::new(0.0, h, 0.0) + lean, 0.045), 4, &Style::rgb(mul3(g, 0.8 + 0.4 * hash01(k, 5, 9))).flat());
        if k % 3 == 0 {
            let tip = base + Vec3::new(0.0, h, 0.0) + lean;
            b.cyl(&limb(tip - Vec3::Y * 0.28, tip + Vec3::Y * 0.12, 0.055), 5, 1.0, 0.7, true, &Style::rgb([0.36, 0.2, 0.1]).flat());
        }
    }
    b
}

fn hay() -> MeshData {
    let mut b = MeshData::new();
    let straw = [0.88, 0.72, 0.3];
    let to_x = Mat4::from_rotation_z(-PI * 0.5);
    b.cyl(&(tr(Vec3::new(-0.55, 0.6, 0.0)) * to_x * sc(Vec3::new(0.6, 1.1, 0.6))), 10, 1.0, 1.0, true, &Style::rgb(straw).ao(0.25));
    for x in [-0.3f32, 0.3] {
        b.cyl(&(tr(Vec3::new(x - 0.04, 0.6, 0.0)) * to_x * sc(Vec3::new(0.615, 0.08, 0.615))), 10, 1.0, 1.0, false, &Style::rgb([0.45, 0.28, 0.1]).flat());
    }
    b
}

fn barrel(pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    b.cyl(&sc(Vec3::new(0.34, 1.0, 0.34)), 8, 1.0, 1.0, true, &Style::rgb(pal.curb_a).ao(0.2));
    b.cyl(&(tr(Vec3::new(0.0, 0.3, 0.0)) * sc(Vec3::new(0.352, 0.22, 0.352))), 8, 1.0, 1.0, false, &Style::rgb(pal.curb_b).flat());
    b.cyl(&(tr(Vec3::new(0.0, 0.97, 0.0)) * sc(Vec3::new(0.30, 0.04, 0.30))), 8, 1.0, 1.0, true, &Style::rgb(mul3(pal.curb_a, 0.6)).flat());
    b
}

fn tyres(pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    for (i, y) in [0.17f32, 0.5, 0.83].into_iter().enumerate() {
        let col = if i == 1 { pal.curb_b } else { [0.06, 0.06, 0.07] };
        b.torus(&tr(Vec3::new(0.0, y, 0.0)), 9, 5, 0.42, 0.17, &Style::rgb(col).ao(0.2));
    }
    b.cyl(&(tr(Vec3::new(0.0, 0.0, 0.0)) * sc(Vec3::new(0.3, 0.02, 0.3))), 6, 1.0, 1.0, true, &Style::rgb([0.05, 0.05, 0.05]));
    b
}

fn tent(pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    let wood = [0.42, 0.28, 0.16];
    let (ca, cb) = (pal.curb_a, [0.97, 0.94, 0.86]);
    let h = 2.6;
    for (x, z) in [(-1.8f32, -1.8f32), (1.8, -1.8), (-1.8, 1.8), (1.8, 1.8)] {
        b.cyl(&(tr(Vec3::new(x, 0.0, z)) * sc(Vec3::new(0.08, h, 0.08))), 4, 1.0, 1.0, true, &st(wood));
    }
    // Pyramid roof in alternating stripes: each side split in two tris.
    let apex = Vec3::new(0.0, h + 1.1, 0.0);
    let corners = [Vec3::new(-2.3, h, -2.3), Vec3::new(2.3, h, -2.3), Vec3::new(2.3, h, 2.3), Vec3::new(-2.3, h, 2.3)];
    for i in 0..4 {
        let (p, q) = (corners[i], corners[(i + 1) % 4]);
        let m = (p + q) * 0.5;
        let out = Vec3::new(m.x, 0.3, m.z);
        b.tri([apex, p, m], out, &Style::rgb(if i % 2 == 0 { ca } else { cb }).flat());
        b.tri([apex, m, q], out, &Style::rgb(if i % 2 == 0 { cb } else { ca }).flat());
        // Scalloped valance.
        let (pd, qd) = (p - Vec3::Y * 0.4, q - Vec3::Y * 0.4);
        b.quad4([p, q, qd, pd], out, &Style::rgb(if i % 2 == 0 { cb } else { ca }).flat());
    }
    // A stall counter with goods.
    b.aabb(Vec3::new(-1.5, 0.0, 0.9), Vec3::new(1.5, 0.95, 1.5), &st(wood));
    for (k, col) in [[0.9, 0.3, 0.2], [0.95, 0.75, 0.2], [0.3, 0.6, 0.85], [0.8, 0.4, 0.7]].into_iter().enumerate() {
        let x = -1.1 + k as f32 * 0.75;
        b.aabb(Vec3::new(x - 0.25, 0.95, 1.0), Vec3::new(x + 0.25, 1.25, 1.4), &st(col));
    }
    b
}

fn bones() -> MeshData {
    let mut b = MeshData::new();
    let bone = [0.93, 0.9, 0.8];
    // Spine and rib arches half buried in the sand.
    b.cyl(&limb(Vec3::new(0.0, 0.7, -0.9), Vec3::new(0.0, 0.72, 1.0), 0.07), 5, 1.0, 0.9, false, &st(bone));
    for k in 0..5 {
        let z = -0.7 + k as f32 * 0.38;
        let rad = 0.62 - 0.05 * ((k as f32) - 2.0).abs();
        let n = 5;
        for i in 0..n {
            let (a0, a1) = (PI * 0.08 + PI * 0.84 * i as f32 / n as f32, PI * 0.08 + PI * 0.84 * (i + 1) as f32 / n as f32);
            let p0 = Vec3::new(a0.cos() * rad, 0.7 + a0.sin() * rad * 0.9 - 0.45, z);
            let p1 = Vec3::new(a1.cos() * rad, 0.7 + a1.sin() * rad * 0.9 - 0.45, z);
            b.cyl(&limb(p0, p1, 0.045), 4, 1.0, 0.9, false, &st(bone));
        }
    }
    // A skull at the near end.
    b.sphere(&(tr(Vec3::new(0.15, 0.2, 1.7)) * sc(Vec3::new(0.3, 0.25, 0.42))), 6, 4, &st(bone));
    b.cone(&limb(Vec3::new(0.15, 0.17, 2.05), Vec3::new(0.15, 0.13, 2.45), 0.1), 4, &st(mul3(bone, 0.92)));
    for s in [-1.0f32, 1.0] {
        b.sphere(&(tr(Vec3::new(0.15 + s * 0.12, 0.3, 1.95)) * sc(Vec3::splat(0.06))), 4, 3, &Style::rgb([0.1, 0.07, 0.05]).flat());
    }
    b
}

fn hoodoo(pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    let tiers = [(1.9f32, 1.55f32, 2.4f32, 0.78f32), (1.55, 1.15, 2.2, 1.12), (1.15, 1.0, 1.6, 0.9), (1.0, 1.9, 1.1, 1.18), (1.9, 1.5, 0.8, 0.96)];
    let mut y = 0.0;
    for (i, (r0, r1, h, shade)) in tiers.into_iter().enumerate() {
        b.cyl(&(tr(Vec3::new(0.0, y, 0.0)) * sc(Vec3::new(1.0, h, 1.0))), 7, r0, r1, true, &st(mul3(pal.wall, shade * (0.92 + 0.12 * hash01(i as i32, 2, 41)))));
        y += h;
    }
    b.sphere_jit(&(tr(Vec3::new(0.0, 0.1, 0.0)) * sc(Vec3::new(2.6, 0.9, 2.6))), 7, 3, &st(mul3(pal.wall, 0.7)), 0.15, 5);
    b
}

fn lava_rock() -> MeshData {
    let mut b = MeshData::new();
    let dark = [0.13, 0.1, 0.1];
    b.sphere_jit(&(tr(Vec3::new(0.0, 0.7, 0.0)) * sc(Vec3::new(1.5, 1.0, 1.3))), 7, 5, &st(dark), 0.3, 21);
    b.sphere_jit(&(tr(Vec3::new(1.2, 0.4, 0.5)) * sc(Vec3::new(0.8, 0.6, 0.7))), 6, 4, &st(mul3(dark, 1.3)), 0.25, 22);
    let glow = Style::rgb([1.0, 0.42, 0.06]).emit(1.0).flat();
    for k in 0..5 {
        let a = k as f32 * 1.3 + 0.4;
        let p = Vec3::new(a.cos() * 1.0, 0.5 + 0.7 * hash01(k, 1, 23), a.sin() * 0.9);
        let q = p + Vec3::new(a.sin() * 0.5, 0.35 * (hash01(k, 2, 23) - 0.3), -a.cos() * 0.5);
        b.cyl(&limb(p, q, 0.07), 4, 1.0, 0.8, false, &glow);
    }
    b
}

fn smoke() -> MeshData {
    let mut b = MeshData::new();
    let base = [0.2, 0.15, 0.15];
    for k in 0..11 {
        let t = k as f32 / 10.0;
        let r = 2.0 + 7.0 * t;
        let pos = Vec3::new(t * t * 16.0 + 1.5 * (k as f32 * 2.1).sin(), 1.0 + t * 38.0, 1.8 * (k as f32 * 1.3).cos());
        let col = lerp3(base, [0.34, 0.26, 0.26], t);
        b.sphere_jit(&(tr(pos) * sc(Vec3::new(r, r * 0.85, r))), 7, 4, &Style::rgb(col).flat().ao(0.35), 0.18, k + 31);
    }
    b
}

fn lily() -> MeshData {
    let mut b = MeshData::new();
    for k in 0..6 {
        let a = k as f32 * 2.1;
        let r = 0.5 + 1.0 * hash01(k, 1, 51);
        let p = Vec3::new(a.cos() * r, 0.05, a.sin() * r);
        let rad = 0.34 + 0.2 * hash01(k, 2, 51);
        b.disc(&(tr(p) * sc(Vec3::new(rad, 1.0, rad))), 8, &Style::rgb(lerp3([0.2, 0.5, 0.2], [0.35, 0.62, 0.25], hash01(k, 3, 51))).flat());
    }
    b.cone(&(tr(Vec3::new(0.2, 0.05, 0.1)) * sc(Vec3::new(0.2, 0.22, 0.2))), 6, &Style::rgb([0.98, 0.62, 0.78]).flat());
    b
}

/// Ostrich fan in the stands: fans cheer in team colours (bitcoin orange most of all).
fn spectator(k: i32) -> MeshData {
    let mut b = MeshData::new();
    let fans: [([f32; 3], [f32; 3]); 6] = [
        ([0.97, 0.58, 0.1], [0.98, 0.95, 0.9]),
        ([0.9, 0.22, 0.2], [0.98, 0.95, 0.9]),
        ([0.22, 0.5, 0.92], [0.98, 0.85, 0.3]),
        ([0.96, 0.84, 0.25], [0.2, 0.2, 0.25]),
        ([0.28, 0.74, 0.42], [0.98, 0.95, 0.9]),
        ([0.12, 0.12, 0.15], [0.97, 0.58, 0.1]),
    ];
    let (body, accent) = fans[(k as usize) % 6];
    let skin = [0.88, 0.7, 0.62];
    // Legs.
    for s in [-1.0f32, 1.0] {
        b.cyl(&limb(Vec3::new(s * 0.12, 0.0, 0.0), Vec3::new(s * 0.12, 0.8, -0.02), 0.06), 4, 1.0, 0.85, false, &Style::rgb(skin).flat());
    }
    // Body, tail tuft and cheering wings up.
    b.sphere(&(tr(Vec3::new(0.0, 1.05, -0.05)) * sc(Vec3::new(0.46, 0.4, 0.58))), 6, 3, &Style::rgb(body).flat().ao(0.3));
    b.sphere(&(tr(Vec3::new(0.0, 1.25, -0.5)) * sc(Vec3::new(0.2, 0.22, 0.2))), 4, 2, &Style::rgb(accent).flat());
    for s in [-1.0f32, 1.0] {
        b.sphere(&(tr(Vec3::new(s * 0.5, 1.45, 0.0)) * Mat4::from_rotation_z(s * -0.3) * sc(Vec3::new(0.12, 0.42, 0.26))), 4, 2, &Style::rgb(accent).flat());
    }
    // Neck, head, beak and a little cap.
    b.cyl(&limb(Vec3::new(0.0, 1.25, 0.2), Vec3::new(0.0, 1.9, 0.3), 0.09), 5, 1.0, 0.8, false, &Style::rgb(skin).flat());
    b.sphere(&(tr(Vec3::new(0.0, 1.98, 0.34)) * sc(Vec3::new(0.14, 0.12, 0.17))), 4, 3, &Style::rgb(skin).flat());
    b.cone(&limb(Vec3::new(0.0, 1.96, 0.46), Vec3::new(0.0, 1.93, 0.68), 0.07), 4, &Style::rgb([0.95, 0.7, 0.2]).flat());
    b.cone(&limb(Vec3::new(0.0, 2.05, 0.32), Vec3::new(0.0, 2.3, 0.3), 0.12), 4, &Style::rgb(accent).flat());
    b
}

/// A pole with a cloth (plain orange pennant with the bitcoin sign, or chequered), along +X from the pole top.
fn flag(len: f32, chequered: bool) -> MeshData {
    let mut b = MeshData::new();
    let h = 4.5;
    b.cyl(&(sc(Vec3::new(0.05, h, 0.05))), 5, 1.0, 0.8, true, &st([0.82, 0.82, 0.86]));
    b.sphere(&(tr(Vec3::new(0.0, h + 0.05, 0.0)) * sc(Vec3::splat(0.1))), 5, 3, &Style::rgb([1.0, 0.8, 0.3]).flat());
    let hh = 1.0;
    let y0 = h - hh - 0.1;
    if chequered {
        let (nx, ny) = (6, 3);
        for i in 0..nx {
            for j in 0..ny {
                let col = if (i + j) % 2 == 0 { [0.96, 0.96, 0.94] } else { [0.07, 0.07, 0.09] };
                let (x0, x1) = (0.05 + len * i as f32 / nx as f32, 0.05 + len * (i + 1) as f32 / nx as f32);
                let (z0, z1) = (y0 + hh * j as f32 / ny as f32, y0 + hh * (j + 1) as f32 / ny as f32);
                let q = [Vec3::new(x0, z0, 0.0), Vec3::new(x1, z0, 0.0), Vec3::new(x1, z1, 0.0), Vec3::new(x0, z1, 0.0)];
                b.quad4(q, Vec3::Z, &Style::rgb(col).flat());
                b.quad4(q, -Vec3::Z, &Style::rgb(mul3(col, 0.9)).flat());
            }
        }
    } else {
        let nx = 4;
        for i in 0..nx {
            let (x0, x1) = (0.05 + len * i as f32 / nx as f32, 0.05 + len * (i + 1) as f32 / nx as f32);
            // The fly end tapers to a swallow tail.
            let (t0, t1) = (1.0 - 0.0 * i as f32, 1.0);
            let q = [Vec3::new(x0, y0, 0.0), Vec3::new(x1, y0, 0.0), Vec3::new(x1, y0 + hh * t1, 0.0), Vec3::new(x0, y0 + hh * t0, 0.0)];
            let col = [0.97, 0.58, 0.1];
            b.quad4(q, Vec3::Z, &Style::rgb(col).flat());
            b.quad4(q, -Vec3::Z, &Style::rgb(mul3(col, 0.9)).flat());
        }
        let mut g = MeshData::new();
        crate::meshes::btc_glyph(&mut g, &Style::rgb([1.0, 0.98, 0.92]).flat());
        b.append(&g, &(tr(Vec3::new(0.05 + len * 0.5, y0 + hh * 0.5, 0.0)) * sc(Vec3::new(1.2, 1.2, 0.2))));
    }
    b
}

/// A tall pole with a cross arm and a hanging banner: bitcoin orange with a big white sign.
fn banner() -> MeshData {
    let mut b = MeshData::new();
    let h = 7.0;
    let metal = [0.2, 0.2, 0.26];
    b.cyl(&(sc(Vec3::new(0.08, h, 0.08))), 5, 1.0, 0.8, true, &st(metal));
    b.aabb(Vec3::new(-0.05, h - 0.12, -0.05), Vec3::new(1.95, h - 0.04, 0.05), &st(metal));
    let (w, bh) = (1.6, 4.2);
    let (x0, top) = (0.15, h - 0.2);
    let ny = 4;
    let orange = [0.97, 0.58, 0.1];
    for j in 0..ny {
        let (y0, y1) = (top - bh * (j + 1) as f32 / ny as f32, top - bh * j as f32 / ny as f32);
        let q = [Vec3::new(x0, y0, 0.0), Vec3::new(x0 + w, y0, 0.0), Vec3::new(x0 + w, y1, 0.0), Vec3::new(x0, y1, 0.0)];
        b.quad4(q, Vec3::Z, &Style::rgb(orange).flat());
        b.quad4(q, -Vec3::Z, &Style::rgb(mul3(orange, 0.92)).flat());
    }
    // A pointed bottom edge.
    let tip = [Vec3::new(x0, top - bh, 0.0), Vec3::new(x0 + w, top - bh, 0.0), Vec3::new(x0 + w * 0.5, top - bh - 0.6, 0.0)];
    b.tri(tip, Vec3::Z, &Style::rgb(orange).flat());
    b.tri(tip, -Vec3::Z, &Style::rgb(mul3(orange, 0.92)).flat());
    let mut g = MeshData::new();
    crate::meshes::btc_glyph(&mut g, &Style::rgb([1.0, 0.98, 0.92]).flat());
    b.append(&g, &(tr(Vec3::new(x0 + w * 0.5, top - bh * 0.45, 0.0)) * sc(Vec3::new(4.2, 4.2, 0.2))));
    b
}

fn btc_sign(pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    let metal = mul3(pal.wall, 0.7);
    let glow = pal.curb_a;
    for x in [-1.6f32, 1.6] {
        b.aabb(Vec3::new(x - 0.25, 0.0, -0.25), Vec3::new(x + 0.25, 7.0, 0.25), &st(metal));
    }
    let c = Vec3::new(0.0, 7.2, 0.0);
    let to_xy = Mat4::from_rotation_x(PI * 0.5);
    b.cyl(&(tr(c + Vec3::new(0.0, 0.0, -0.07)) * to_xy * sc(Vec3::new(4.0, 0.14, 4.0))), 20, 1.0, 1.0, true, &st([0.05, 0.04, 0.08]));
    b.torus(&(tr(c) * to_xy), 20, 4, 3.9, 0.16, &Style::rgb(glow).emit(1.0).flat());
    let mut g = MeshData::new();
    crate::meshes::btc_glyph(&mut g, &Style::rgb([1.0, 0.62, 0.12]).emit(1.0).flat());
    b.append(&g, &(tr(c) * sc(Vec3::new(9.0, 9.0, 1.2))));
    b
}

/// Seven-segment digit `d` (0..=9) in a box `w` x `h`, facing +Z, centred at the origin; `lit` colour.
fn digit(b: &mut MeshData, d: u8, w: f32, h: f32, t: f32, style: &Style) {
    // a b c d e f g
    const SEG: [[bool; 7]; 10] = [
        [true, true, true, true, true, true, false],
        [false, true, true, false, false, false, false],
        [true, true, false, true, true, false, true],
        [true, true, true, true, false, false, true],
        [false, true, true, false, false, true, true],
        [true, false, true, true, false, true, true],
        [true, false, true, true, true, true, true],
        [true, true, true, false, false, false, false],
        [true, true, true, true, true, true, true],
        [true, true, true, true, false, true, true],
    ];
    let (hw, hh) = (w * 0.5, h * 0.5);
    let bars: [(f32, f32, f32, f32); 7] = [
        (0.0, hh, w - t, t),
        (hw, hh * 0.5, t, hh - t),
        (hw, -hh * 0.5, t, hh - t),
        (0.0, -hh, w - t, t),
        (-hw, -hh * 0.5, t, hh - t),
        (-hw, hh * 0.5, t, hh - t),
        (0.0, 0.0, w - t, t),
    ];
    for (i, (cx, cy, sx, sy)) in bars.into_iter().enumerate() {
        if SEG[d as usize][i] {
            b.cube(&(tr(Vec3::new(cx, cy, 0.0)) * sc(Vec3::new(sx.max(t), sy.max(t), 0.25))), style);
        }
    }
}

fn billboard(pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    let metal = mul3(pal.wall, 0.6);
    let (w, h) = (16.0, 6.5);
    let y0 = 6.0;
    for x in [-5.5f32, 5.5] {
        b.aabb(Vec3::new(x - 0.3, 0.0, -0.3), Vec3::new(x + 0.3, y0 + 0.5, 0.3), &st(metal));
    }
    let c = Vec3::new(0.0, y0 + h * 0.5, 0.0);
    b.cube(&(tr(c) * sc(Vec3::new(w, h, 0.5))), &st([0.05, 0.04, 0.09]));
    let glow = Style::rgb(pal.curb_a).emit(1.0).flat();
    for (cy, sy) in [(h * 0.5 - 0.15, 0.3f32), (-h * 0.5 + 0.15, 0.3)] {
        b.cube(&(tr(c + Vec3::new(0.0, cy, 0.0)) * sc(Vec3::new(w + 0.3, sy, 0.7))), &glow);
    }
    for x in [-w * 0.5, w * 0.5] {
        b.cube(&(tr(c + Vec3::new(x, 0.0, 0.0)) * sc(Vec3::new(0.3, h, 0.7))), &glow);
    }
    // 3.125 in lit seven-segment digits (the halving reward), then the bitcoin sign.
    let lit = Style::rgb([1.0, 0.7, 0.18]).emit(1.0).flat();
    let (dw, dh, tt) = (1.9, 3.4, 0.42);
    let xs = [-6.2f32, -3.6, -2.0, 0.6];
    for (x, d) in [(xs[0], 3u8), (xs[2] + 0.2, 1), (xs[3] + 1.4, 2), (xs[3] + 4.2, 5)] {
        let mut dm = MeshData::new();
        digit(&mut dm, d, dw, dh, tt, &lit);
        b.append(&dm, &(tr(c + Vec3::new(x, 0.35, 0.3))));
    }
    // The decimal point.
    b.cube(&(tr(c + Vec3::new(-4.55, -1.5 + 0.35, 0.3)) * sc(Vec3::new(0.5, 0.5, 0.25))), &lit);
    let mut g = MeshData::new();
    crate::meshes::btc_glyph(&mut g, &Style::rgb([1.0, 0.8, 0.3]).emit(1.0).flat());
    b.append(&g, &(tr(c + Vec3::new(6.2, 0.35, 0.25)) * sc(Vec3::new(5.6, 5.6, 0.6))));
    // A row of block cells counting down to the next halving.
    for i in 0..24 {
        let on = i < 17;
        let col = if on { Style::rgb([1.0, 0.62, 0.15]).emit(1.0).flat() } else { Style::rgb([0.2, 0.12, 0.08]).flat() };
        b.cube(&(tr(c + Vec3::new(-6.9 + i as f32 * 0.6, -2.5, 0.28)) * sc(Vec3::new(0.46, 0.5, 0.2))), &col);
    }
    b
}

fn tower(pal: &Palette, k: i32) -> MeshData {
    let mut b = MeshData::new();
    let dark = mul3(pal.wall, 0.55 + 0.1 * hash01(k, 1, 61));
    let (w, d) = (1.0 - 0.15 * hash01(k, 2, 61), 0.85 + 0.2 * hash01(k, 3, 61));
    let h = 1.0;
    b.aabb(Vec3::new(-6.0 * w, 0.0, -6.0 * d), Vec3::new(6.0 * w, 60.0 * h, 6.0 * d), &Style::rgb(dark).flat());
    // Lit window bands: per floor, per face, a few runs.
    let cols = [[1.0, 0.72, 0.28], [0.55, 0.85, 1.0], [1.0, 0.9, 0.7], [1.0, 0.45, 0.2]];
    let floors = 11;
    for f in 0..floors {
        let y = 3.0 + f as f32 * 4.9;
        for face in 0..4 {
            let (n, u) = match face {
                0 => (Vec3::Z, Vec3::X),
                1 => (-Vec3::Z, -Vec3::X),
                2 => (Vec3::X, -Vec3::Z),
                _ => (-Vec3::X, Vec3::Z),
            };
            let half = if face < 2 { 6.0 * w } else { 6.0 * d };
            let depth = if face < 2 { 6.0 * d } else { 6.0 * w };
            let runs = 2;
            for r in 0..runs {
                let hsh = hash01(k * 100 + f, face * 7 + r, 62);
                if hsh < 0.38 {
                    continue;
                }
                let a = -half + 1.0 + (half * 2.0 - 2.0) * (r as f32 / runs as f32);
                let wlen = (half * 2.0 - 2.0) / runs as f32 * (0.55 + 0.4 * hash01(k * 100 + f, face * 7 + r, 63));
                let col = cols[((hsh * 17.0) as usize) % 4];
                let p = n * (depth + 0.03);
                let q = [p + u * a + Vec3::Y * y, p + u * (a + wlen) + Vec3::Y * y, p + u * (a + wlen) + Vec3::Y * (y + 1.6), p + u * a + Vec3::Y * (y + 1.6)];
                b.quad4(q, n, &Style::rgb(col).emit(1.0).flat());
            }
        }
    }
    // Neon crown and mast.
    let crown = Style::rgb(if k % 2 == 0 { pal.curb_a } else { [0.4, 0.85, 1.0] }).emit(1.0).flat();
    b.aabb(Vec3::new(-6.2 * w, 58.5 * h, -6.2 * d), Vec3::new(6.2 * w, 60.4 * h, 6.2 * d), &crown);
    b.cyl(&(tr(Vec3::new(0.0, 60.4 * h, 0.0)) * sc(Vec3::new(0.3, 8.0, 0.3))), 4, 1.0, 0.5, true, &Style::rgb(dark).flat());
    b.sphere(&(tr(Vec3::new(0.0, 68.6 * h, 0.0)) * sc(Vec3::splat(0.5))), 4, 3, &Style::rgb([1.0, 0.1, 0.1]).emit(1.0).flat());
    b
}

// ----------------------------------------------------------- fluid bake ----

/// Bake a fluid patch (flat polygon with the animated-surface anim word) into `out`.
pub fn bake_fluid(out: &mut DecorData, f: &Fluid) {
    let (sy, cy) = f.yaw.sin_cos();
    let fwd = Vec3::new(sy, 0.0, cy);
    let right = Vec3::new(-cy, 0.0, sy);
    let kind = if f.kind == FluidKind::Water { A_WATER } else { A_LAVA };
    let anim = [kind, u8f(f.shallow[0]), u8f(f.shallow[1]), u8f(f.shallow[2])];
    let col = [u8f(f.deep[0]), u8f(f.deep[1]), u8f(f.deep[2]), 255];
    let mut pts: Vec<Vec3> = Vec::new();
    if f.round {
        let n = 22;
        for i in 0..n {
            let a = TAU * i as f32 / n as f32;
            pts.push(f.pos + right * (a.cos() * f.half.x) + fwd * (a.sin() * f.half.y));
        }
    } else {
        pts.push(f.pos - right * f.half.x - fwd * f.half.y);
        pts.push(f.pos + right * f.half.x - fwd * f.half.y);
        pts.push(f.pos + right * f.half.x + fwd * f.half.y);
        pts.push(f.pos - right * f.half.x + fwd * f.half.y);
    }
    let base = out.verts.len() as u32;
    let mk = |p: Vec3| DecorVertex { pos: p.to_array(), nrm: [0, 127, 0, 0], col, anim };
    if f.round {
        out.verts.push(mk(f.pos));
        for p in &pts {
            out.verts.push(mk(*p));
        }
        let n = pts.len() as u32;
        for i in 0..n {
            // right x fwd = +Y, so (centre, this, next) faces up.
            out.idx.extend_from_slice(&[base, base + 1 + i, base + 1 + (i + 1) % n]);
        }
    } else {
        for p in &pts {
            out.verts.push(mk(*p));
        }
        out.idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

// ----------------------------------------------------------------- tests ----

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_meshes_are_sane() {
        for pal in [Palette::savanna(), Palette::neon_night()] {
            let k = build_kit2(&pal);
            let all: Vec<(&str, &MeshData)> = vec![
                ("baobab", &k.baobab),
                ("palm", &k.palm),
                ("reed", &k.reed),
                ("hay", &k.hay),
                ("barrel", &k.barrel),
                ("tyres", &k.tyres),
                ("tent", &k.tent),
                ("bones", &k.bones),
                ("hoodoo", &k.hoodoo),
                ("lava", &k.lava_rock),
                ("smoke", &k.smoke),
                ("lily", &k.lily),
                ("spec0", &k.spectators[0]),
                ("flag", &k.flag),
                ("check", &k.check_flag),
                ("banner", &k.banner),
                ("sign", &k.btc_sign),
                ("billboard", &k.billboard),
                ("tower", &k.towers[0]),
            ];
            for (name, m) in all {
                assert!(!m.idx.is_empty(), "{name} is empty");
                assert!(m.verts.iter().all(|v| v.pos.iter().all(|c| c.is_finite())), "{name} has NaNs");
                assert!(m.tri_count() < 2500, "{name}: {} tris", m.tri_count());
            }
            assert!(k.spectators.iter().all(|s| s.tri_count() < 200));
        }
    }

    #[test]
    fn fluid_bake_is_valid() {
        let f = Fluid { kind: FluidKind::Water, pos: Vec3::new(1.0, 2.0, 3.0), yaw: 0.7, half: glam::Vec2::new(10.0, 6.0), round: true, deep: [0.1, 0.3, 0.4], shallow: [0.3, 0.7, 0.7] };
        let mut d = DecorData::default();
        bake_fluid(&mut d, &f);
        assert!(d.idx.iter().all(|&i| (i as usize) < d.verts.len()));
        let mut d2 = DecorData::default();
        bake_fluid(&mut d2, &Fluid { round: false, kind: FluidKind::Lava, ..f });
        assert_eq!(d2.idx.len(), 6);
    }
}
