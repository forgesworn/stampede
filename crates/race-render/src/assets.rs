//! Small authored meshes shared by the articulated riders and race props.

use crate::mesh::{limb, MeshData, Style};
use glam::{Mat4, Vec3};
use std::f32::consts::PI;

fn tr(p: Vec3, s: Vec3) -> Mat4 {
    Mat4::from_translation(p) * Mat4::from_scale(s)
}

pub(crate) fn head() -> MeshData {
    let mut b = MeshData::new();
    let skin = Style::tint0(1.0).ao(0.15).sheen(0.10);
    b.sphere(&tr(Vec3::ZERO, Vec3::new(0.22, 0.25, 0.21)), 10, 6, &skin);
    for side in [-1.0, 1.0] {
        b.sphere(&tr(Vec3::new(side * 0.215, -0.01, 0.0), Vec3::new(0.046, 0.075, 0.045)), 6, 4, &skin);
    }
    b.sphere(&tr(Vec3::new(0.0, -0.035, 0.205), Vec3::new(0.05, 0.057, 0.07)), 8, 4, &skin);
    b.sphere(&tr(Vec3::new(0.0, -0.145, 0.025), Vec3::new(0.16, 0.10, 0.15)), 8, 5, &skin);
    b
}

pub(crate) fn helmet() -> MeshData {
    let mut b = MeshData::new();
    let shell = Style::tint0(1.0).sheen(0.55).ao(0.12);
    b.lathe(&Mat4::IDENTITY, 14, &[(0.22, 0.06), (0.245, 0.13), (0.23, 0.22), (0.16, 0.30), (0.0, 0.33)], &shell);
    b.bevel_box(&tr(Vec3::new(0.0, 0.09, 0.235), Vec3::new(0.43, 0.045, 0.25)), 0.18, &shell);
    // A sewn centre stripe follows the crown rather than floating over it.
    let stripe = Style::col(0.98, 0.93, 0.79).sheen(0.22);
    let path = [(0.06, -0.224), (0.235, -0.21), (0.316, -0.1), (0.333, 0.0), (0.314, 0.1), (0.232, 0.21)];
    for pair in path.windows(2) {
        let [(y0, z0), (y1, z1)] = [pair[0], pair[1]];
        let normal = Vec3::new(0.0, z1 - z0, y0 - y1).normalize();
        b.quad4([Vec3::new(-0.035, y0, z0), Vec3::new(0.035, y0, z0), Vec3::new(0.035, y1, z1), Vec3::new(-0.035, y1, z1)], normal, &stripe);
    }
    b
}

pub(crate) fn jersey() -> MeshData {
    let mut b = MeshData::new();
    b.lathe(
        &tr(Vec3::new(0.0, -0.36, 0.0), Vec3::new(0.27, 0.72, 0.21)),
        10,
        &[(0.74, 0.0), (0.92, 0.16), (1.0, 0.55), (0.93, 0.84), (0.50, 1.0), (0.0, 1.0)],
        &Style::tint0(1.0).ao(0.24),
    );
    let trim = Style::col(0.97, 0.93, 0.80);
    // Cream racing sash, collar and piping give clothing a readable shape.
    b.bevel_box(&tr(Vec3::new(0.0, 0.08, 0.195), Vec3::new(0.46, 0.085, 0.025)), 0.12, &trim);
    b.lathe(&tr(Vec3::new(0.0, 0.31, 0.0), Vec3::new(0.145, 0.06, 0.12)), 10, &[(1.0, 0.0), (1.0, 0.8), (0.7, 1.0)], &trim);
    b
}

pub(crate) fn saddle() -> MeshData {
    let mut b = MeshData::new();
    let leather = Style::col(0.22, 0.10, 0.065).ao(0.25).sheen(0.22);
    b.bevel_box(&tr(Vec3::ZERO, Vec3::new(0.82, 0.13, 0.80)), 0.20, &Style::tint0(0.72).ao(0.2));
    b.sphere(&tr(Vec3::new(0.0, 0.07, 0.0), Vec3::new(0.31, 0.08, 0.32)), 8, 4, &leather);
    for z in [-0.31, 0.27] {
        b.sphere(&tr(Vec3::new(0.0, 0.13, z), Vec3::new(0.31, 0.09, 0.07)), 6, 3, &leather);
    }
    for side in [-1.0, 1.0] {
        for k in 0..4 {
            b.cube(&tr(Vec3::new(side * 0.38, 0.012, -0.27 + k as f32 * 0.17), Vec3::new(0.018, 0.045, 0.065)), &Style::col(0.97, 0.87, 0.65));
        }
        b.cyl(&limb(Vec3::new(side * 0.33, 0.0, 0.08), Vec3::new(side * 0.54, -0.49, 0.16), 0.026), 5, 1.0, 1.0, false, &leather);
    }
    b
}

pub(crate) fn laptop_base() -> MeshData {
    let mut b = MeshData::new();
    b.bevel_box(&tr(Vec3::ZERO, Vec3::new(0.58, 0.045, 0.40)), 0.13, &Style::col(0.23, 0.29, 0.36).sheen(0.55));
    let keys = Style::col(0.055, 0.075, 0.10).sheen(0.15);
    for row in 0..3 {
        for col in 0..6 {
            b.cube(&tr(Vec3::new((col as f32 - 2.5) * 0.075, 0.026, -0.11 + row as f32 * 0.065), Vec3::new(0.055, 0.008, 0.043)), &keys);
        }
    }
    b.bevel_box(&tr(Vec3::new(0.0, 0.023, 0.12), Vec3::new(0.17, 0.003, 0.08)), 0.12, &Style::col(0.38, 0.46, 0.53).sheen(0.4));
    b.cyl(
        &(Mat4::from_translation(Vec3::new(-0.245, 0.025, 0.175)) * Mat4::from_rotation_z(-PI * 0.5) * Mat4::from_scale(Vec3::new(0.018, 0.49, 0.018))),
        8,
        1.0,
        1.0,
        true,
        &Style::col(0.09, 0.12, 0.17).sheen(0.5),
    );
    b
}

pub(crate) fn laptop_lid() -> MeshData {
    let mut b = MeshData::new();
    b.bevel_box(&tr(Vec3::ZERO, Vec3::new(0.58, 0.39, 0.037)), 0.10, &Style::col(0.12, 0.16, 0.22).sheen(0.45));
    b.bevel_box(&tr(Vec3::new(0.0, 0.008, -0.021), Vec3::new(0.50, 0.30, 0.008)), 0.08, &Style::col(0.025, 0.11, 0.13).emit(0.2));
    for row in 0..6 {
        let y = 0.105 - row as f32 * 0.034;
        let len = [0.33, 0.20, 0.28, 0.15, 0.30, 0.22][row];
        b.cube(&tr(Vec3::new(-0.2 + len * 0.5, y, -0.027), Vec3::new(len, 0.009, 0.002)), &Style::col(0.18, 0.92, 0.64).emit(0.65));
    }
    let mut mark = MeshData::new();
    crate::meshes::btc_glyph(&mut mark, &Style::col(1.0, 0.58, 0.08).sheen(0.45));
    b.append(&mark, &tr(Vec3::new(0.0, 0.0, 0.026), Vec3::new(0.34, 0.34, 0.12)));
    b
}

pub(crate) fn plate() -> MeshData {
    let mut b = MeshData::new();
    b.lathe(&Mat4::IDENTITY, 16, &[(0.0, 0.0), (0.22, 0.0), (0.28, 0.025), (0.3, 0.04), (0.305, 0.02), (0.27, -0.015), (0.0, -0.025)], &Style::col(0.96, 0.94, 0.87).sheen(0.55));
    b
}

pub(crate) fn cake() -> MeshData {
    let mut b = MeshData::new();
    let outline = [(-0.145, -0.105), (0.145, -0.105), (0.15, -0.09), (0.015, 0.155), (-0.015, 0.155), (-0.15, -0.09)];
    for (y, col) in [(0.0, [0.97, 0.46, 0.08]), (0.06, [1.0, 0.95, 0.80]), (0.10, [1.0, 0.55, 0.11]), (0.16, [1.0, 0.98, 0.89])] {
        let h = if y == 0.06 || y == 0.16 { 0.035 } else { 0.055 };
        let st = Style::rgb(col).ao(0.10).sheen(if y == 0.16 { 0.2 } else { 0.0 });
        for i in 0..outline.len() {
            let (x0, z0) = outline[i];
            let (x1, z1) = outline[(i + 1) % outline.len()];
            let n = Vec3::new(z1 - z0, 0.0, x0 - x1).normalize();
            b.quad4([Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x1, y + h, z1), Vec3::new(x0, y + h, z0)], n, &st);
            b.push_tris(&[Vec3::new(0.0, y + h, 0.0), Vec3::new(x0, y + h, z0), Vec3::new(x1, y + h, z1)], &[Vec3::Y; 3], &[1.0; 3], &[[0, 1, 2]], &st);
        }
    }
    for k in 0..3 {
        b.sphere(&tr(Vec3::new((k as f32 - 1.0) * 0.08, 0.21, -0.065), Vec3::new(0.035, 0.032, 0.035)), 7, 4, &Style::col(1.0, 0.98, 0.89).sheen(0.15));
    }
    b.sphere(&tr(Vec3::new(0.0, 0.232, -0.065), Vec3::new(0.028, 0.035, 0.025)), 8, 4, &Style::col(1.0, 0.40, 0.015).sheen(0.5));
    b
}

pub(crate) fn frog_foot() -> MeshData {
    let mut b = MeshData::new();
    let skin = Style::tint0(1.0).sheen(0.18).ao(0.18);
    b.sphere(&tr(Vec3::new(0.0, 0.0, 0.02), Vec3::new(0.2, 0.06, 0.18)), 8, 4, &skin);
    for k in -1i32..=1 {
        let a = Vec3::new(k as f32 * 0.065, 0.0, 0.12);
        let c = Vec3::new(k as f32 * 0.17, -0.015, 0.28 - k.abs() as f32 * 0.04);
        b.cyl(&limb(a, c, 0.036), 6, 1.0, 0.7, true, &skin);
        b.sphere(&tr(c, Vec3::splat(0.047)), 6, 4, &skin);
    }
    b
}

/// An oversized red rose, the bloom centred on the origin, the stem joining
/// below it (-y). Fixed colours: it is the one thing a rose seller must show.
pub(crate) fn rose_bloom() -> MeshData {
    let mut b = MeshData::new();
    let petal = |shade: f32| Style::col(0.86 * shade, 0.06 * shade, 0.12 * shade).sheen(0.35).ao(0.10);
    // Cupped rings of petals, outer and open to inner and tight.
    b.lathe(&Mat4::IDENTITY, 12, &[(0.0, -0.12), (0.12, -0.10), (0.20, -0.02), (0.23, 0.06), (0.20, 0.08)], &petal(0.82));
    for k in 0..5 {
        let a = k as f32 / 5.0 * 2.0 * PI;
        let c = Vec3::new(a.cos() * 0.15, 0.03, a.sin() * 0.15);
        b.sphere(&(Mat4::from_translation(c) * Mat4::from_rotation_y(-a) * Mat4::from_scale(Vec3::new(0.06, 0.09, 0.11))), 8, 4, &petal(0.95));
    }
    b.lathe(&tr(Vec3::new(0.0, 0.02, 0.0), Vec3::splat(0.62)), 10, &[(0.0, -0.08), (0.12, -0.05), (0.17, 0.05), (0.13, 0.12)], &petal(1.0));
    b.sphere(&tr(Vec3::new(0.0, 0.09, 0.0), Vec3::new(0.07, 0.06, 0.07)), 8, 4, &petal(0.7));
    // A swirl at the heart.
    b.sphere(&tr(Vec3::new(0.02, 0.12, 0.01), Vec3::new(0.035, 0.025, 0.03)), 6, 3, &petal(0.55));
    // Green sepals under the bloom.
    let green = Style::col(0.16, 0.52, 0.18).sheen(0.15);
    for k in 0..5 {
        let a = (k as f32 + 0.5) / 5.0 * 2.0 * PI;
        let c = Vec3::new(a.cos() * 0.07, -0.12, a.sin() * 0.07);
        b.sphere(&(Mat4::from_translation(c) * Mat4::from_rotation_y(-a) * Mat4::from_scale(Vec3::new(0.025, 0.02, 0.07))), 6, 3, &green);
    }
    b
}

/// A wicker basket of roses, hung from a hand at the origin by its handle.
pub(crate) fn basket() -> MeshData {
    let mut b = MeshData::new();
    let wicker = Style::col(0.72, 0.52, 0.27).ao(0.25).sheen(0.05);
    let base = Mat4::from_translation(Vec3::new(0.0, -0.38, 0.0));
    b.lathe(&base, 12, &[(0.0, 0.0), (0.13, 0.0), (0.17, 0.05), (0.19, 0.16), (0.2, 0.18)], &wicker);
    // Weave bands.
    for y in [0.05f32, 0.11] {
        let r = 0.175 + y * 0.15;
        b.lathe(&Mat4::from_translation(Vec3::new(0.0, -0.38 + y, 0.0)), 12, &[(r, 0.0), (r + 0.01, 0.015), (r, 0.03)], &Style::col(0.58, 0.39, 0.18).ao(0.1));
    }
    // The handle, an arch of short segments up to the hand.
    let arch = |t: f32| Vec3::new((t * PI).cos() * 0.18, -0.2 + (t * PI).sin() * 0.2, 0.0);
    for k in 0..6 {
        let (a, c) = (arch(k as f32 / 6.0), arch((k + 1) as f32 / 6.0));
        b.cyl(&limb(a, c, 0.014), 6, 1.0, 1.0, false, &wicker);
    }
    // Spare roses peeping out.
    for (x, z) in [(-0.08f32, 0.04f32), (0.06, -0.05), (0.02, 0.08), (0.09, 0.05)] {
        b.sphere(&tr(Vec3::new(x, -0.2, z), Vec3::new(0.05, 0.045, 0.05)), 7, 4, &Style::col(0.82, 0.07, 0.13).sheen(0.3));
        b.sphere(&tr(Vec3::new(x * 1.2, -0.23, z * 1.2 - 0.03), Vec3::new(0.035, 0.012, 0.05)), 5, 3, &Style::col(0.18, 0.5, 0.2));
    }
    b
}
