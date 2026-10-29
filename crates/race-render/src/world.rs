//! Static world baking: road ribbon, shoulders, curbs, walls, terrain,
//! scenery, start arch and boost pads, binned into spatial chunks that share
//! one vertex and one index buffer (for frustum culling and range merging).

use crate::bridge::{self, Bridges};
use crate::color::{lerp3, mul3};
use crate::decor::{self, DecorData, DecorVertex};
use crate::mesh::{basis, hash01, limb, noise2, vertex, MeshData, Style, Vertex};
use crate::types::*;
use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use std::collections::HashMap;
use std::f32::consts::PI;

pub const CHUNK: f32 = 64.0;

#[derive(Clone, Copy, Debug)]
pub struct ChunkInfo {
    pub lo: Vec3,
    pub hi: Vec3,
    pub first: u32,
    pub count: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct PadVertex {
    pub pos: [f32; 3],
    /// Local metres: x across (right positive), y along from the start.
    pub uv: [f32; 2],
    /// Half width, length.
    pub size: [f32; 2],
}

pub struct BakedWorld {
    pub verts: Vec<Vertex>,
    pub idx: Vec<u32>,
    pub chunks: Vec<ChunkInfo>,
    pub pad_verts: Vec<PadVertex>,
    pub pad_idx: Vec<u32>,
    pub decor_verts: Vec<DecorVertex>,
    pub decor_idx: Vec<u32>,
    pub decor_chunks: Vec<ChunkInfo>,
    pub ground: GroundQuery,
}

// ------------------------------------------------------------- ground ----

/// CPU ground height lookup (road plane, else terrain) for shadows and particles.
pub struct GroundQuery {
    road: Vec<RoadSample>,
    cell: f32,
    grid: HashMap<(i32, i32), Vec<u32>>,
    terrain: Option<Terrain>,
}

impl GroundQuery {
    pub fn new(w: &WorldGeometry) -> GroundQuery {
        let cell = 16.0;
        let mut grid: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for (i, s) in w.road.iter().enumerate() {
            if s.flags & road_flags::GAP != 0 {
                continue;
            }
            let k = ((s.pos.x / cell).floor() as i32, (s.pos.z / cell).floor() as i32);
            grid.entry(k).or_default().push(i as u32);
        }
        GroundQuery { road: w.road.clone(), cell, grid, terrain: w.terrain.clone() }
    }

    pub fn empty() -> GroundQuery {
        GroundQuery { road: Vec::new(), cell: 16.0, grid: HashMap::new(), terrain: None }
    }

    /// Nearest centreline sample by horizontal distance.
    pub fn nearest(&self, x: f32, z: f32) -> Option<(usize, f32)> {
        self.nearest_y(x, None, z)
    }

    /// Nearest centreline sample; with `y` the vertical gap counts too, so at an overpass a point on the
    /// lower road finds the lower road and a point on the deck finds the deck. The distance returned is
    /// squared (and includes the vertical gap).
    pub fn nearest_y(&self, x: f32, y: Option<f32>, z: f32) -> Option<(usize, f32)> {
        let (cx, cz) = ((x / self.cell).floor() as i32, (z / self.cell).floor() as i32);
        let mut best: Option<(usize, f32)> = None;
        for dx in -1..=1 {
            for dz in -1..=1 {
                if let Some(v) = self.grid.get(&(cx + dx, cz + dz)) {
                    for &i in v {
                        let s = &self.road[i as usize];
                        let mut d = (s.pos.x - x).powi(2) + (s.pos.z - z).powi(2);
                        if let Some(y) = y {
                            d += (s.pos.y - y).powi(2);
                        }
                        if best.map_or(true, |b| d < b.1) {
                            best = Some((i as usize, d));
                        }
                    }
                }
            }
        }
        best
    }

    /// Signed lateral offset (right positive) and whether the point is on the
    /// drivable band of the nearest sample.
    pub fn band(&self, x: f32, z: f32, margin: f32) -> Option<(usize, f32, bool)> {
        self.band_y(x, None, z, margin)
    }

    /// [`GroundQuery::band`] choosing the nearest sample in 3D (see [`GroundQuery::nearest_y`]).
    pub fn band_y(&self, x: f32, y: Option<f32>, z: f32, margin: f32) -> Option<(usize, f32, bool)> {
        let (i, _) = self.nearest_y(x, y, z)?;
        let s = &self.road[i];
        let lat = (x - s.pos.x) * s.right.x + (z - s.pos.z) * s.right.z;
        let lim = s.width * 0.5 + if lat < 0.0 { s.shoulder_l } else { s.shoulder_r } + margin;
        let fwd = s.up.cross(s.right);
        let lon = (x - s.pos.x) * fwd.x + (z - s.pos.z) * fwd.z;
        Some((i, lat, lat.abs() <= lim && lon.abs() < 2.5))
    }

    fn road_plane(s: &RoadSample, x: f32, z: f32) -> f32 {
        let n = s.up;
        if n.y.abs() < 0.2 {
            return s.pos.y;
        }
        s.pos.y - (n.x * (x - s.pos.x) + n.z * (z - s.pos.z)) / n.y
    }

    /// Lowest road-surface height (with cross-slope) among the samples whose drivable
    /// band, widened by `reach` at its edges, covers (x, z); `None` if the road is not near.
    /// Beside the band the cross-slope is held at its edge value, so a banked road does
    /// not dig a trench.
    pub fn road_cap(&self, x: f32, z: f32, reach: f32) -> Option<f32> {
        let (cx, cz) = ((x / self.cell).floor() as i32, (z / self.cell).floor() as i32);
        let mut cap: Option<f32> = None;
        for dx in -2..=2 {
            for dz in -2..=2 {
                let Some(v) = self.grid.get(&(cx + dx, cz + dz)) else { continue };
                for &i in v {
                    let s = &self.road[i as usize];
                    let (ox, oz) = (x - s.pos.x, z - s.pos.z);
                    let lat = ox * s.right.x + oz * s.right.z;
                    let half = s.width * 0.5 + if lat < 0.0 { s.shoulder_l } else { s.shoulder_r };
                    if lat.abs() > half + reach {
                        continue;
                    }
                    // Along the road, only the samples next to the point count.
                    let fwd = s.up.cross(s.right);
                    if (ox * fwd.x + oz * fwd.z).abs() > 3.0 {
                        continue;
                    }
                    let n = s.up;
                    let y = if n.y.abs() > 0.2 {
                        s.pos.y - (n.x * s.right.x + n.z * s.right.z) * lat.clamp(-half, half) / n.y
                    } else {
                        s.pos.y
                    };
                    cap = Some(cap.map_or(y, |c: f32| c.min(y)));
                }
            }
        }
        cap
    }

    pub fn terrain_height(&self, x: f32, z: f32) -> Option<f32> {
        let t = self.terrain.as_ref()?;
        let fx = ((x - t.origin.x) / t.cell).clamp(0.0, (t.w - 1) as f32 - 0.001);
        let fz = ((z - t.origin.y) / t.cell).clamp(0.0, (t.h - 1) as f32 - 0.001);
        let (ix, iz) = (fx.floor() as usize, fz.floor() as usize);
        let (tx, tz) = (fx - ix as f32, fz - iz as f32);
        let h = |x: usize, z: usize| t.heights[z * t.w + x];
        let a = h(ix, iz) + (h(ix + 1, iz) - h(ix, iz)) * tx;
        let b = h(ix, iz + 1) + (h(ix + 1, iz + 1) - h(ix, iz + 1)) * tx;
        Some(a + (b - a) * tz)
    }

    /// Ground height and normal at (x, z): road, else terrain, else flat 0.
    pub fn height_normal(&self, x: f32, z: f32) -> (f32, Vec3) {
        self.height_normal_y(x, None, z)
    }

    /// [`GroundQuery::height_normal`] for something at height `y`: at an overpass the road beneath it, not the
    /// other branch.
    pub fn height_normal_y(&self, x: f32, y: Option<f32>, z: f32) -> (f32, Vec3) {
        if let Some((i, _, on)) = self.band_y(x, y, z, 0.0) {
            if on {
                let s = &self.road[i];
                return (Self::road_plane(s, x, z), s.up);
            }
        }
        if let Some(h) = self.terrain_height(x, z) {
            let e = 1.0;
            let hx = self.terrain_height(x + e, z).unwrap_or(h) - self.terrain_height(x - e, z).unwrap_or(h);
            let hz = self.terrain_height(x, z + e).unwrap_or(h) - self.terrain_height(x, z - e).unwrap_or(h);
            return (h, Vec3::new(-hx / (2.0 * e), 1.0, -hz / (2.0 * e)).normalize());
        }
        if let Some((i, d2)) = self.nearest_y(x, y, z) {
            if d2 < 40.0 * 40.0 {
                return (self.road[i].pos.y, Vec3::Y);
            }
        }
        (0.0, Vec3::Y)
    }

    pub fn height(&self, x: f32, z: f32) -> f32 {
        self.height_normal(x, z).0
    }

    /// [`GroundQuery::height`] for something at height `y` (see [`GroundQuery::height_normal_y`]).
    pub fn height_y(&self, x: f32, y: f32, z: f32) -> f32 {
        self.height_normal_y(x, Some(y), z).0
    }
}

// --------------------------------------------------------------- bake ----

pub(crate) struct Cells {
    map: HashMap<(i32, i32), MeshData>,
}

/// The animated decor, binned the same way.
struct DecorCells {
    map: HashMap<(i32, i32), DecorData>,
}

impl DecorCells {
    fn at(&mut self, p: Vec3) -> &mut DecorData {
        self.map.entry(((p.x / CHUNK).floor() as i32, (p.z / CHUNK).floor() as i32)).or_default()
    }
}

impl Cells {
    pub(crate) fn at(&mut self, p: Vec3) -> &mut MeshData {
        self.map.entry(((p.x / CHUNK).floor() as i32, (p.z / CHUNK).floor() as i32)).or_default()
    }
}

/// Build all static geometry of a track.
pub fn bake(w: &WorldGeometry) -> BakedWorld {
    let ground = GroundQuery::new(w);
    let mut cells = Cells { map: HashMap::new() };
    let mut dcells = DecorCells { map: HashMap::new() };
    let pal = &w.palette;
    let br = Bridges::new(w);
    if w.field.is_none() {
        bake_road(w, &br, &mut cells);
    }
    bake_walls(w, &br, &mut cells);
    bridge::bake(w, &br, &ground, &mut cells);
    if let Some(t) = &w.terrain {
        bake_terrain(w, t, &ground, &br, &mut cells);
    }
    if let Some(f) = w.field.as_ref().filter(|f| f.landmark) {
        bake_field(w, f, &mut cells);
    }
    bake_scenery(w, &mut cells, &mut dcells);
    bake_start(w, &ground, &mut cells, &mut dcells);
    for f in &w.fx.fluids {
        decor::bake_fluid(dcells.at(f.pos), f);
    }
    let (pad_verts, pad_idx) = bake_pads(w, &ground);
    let _ = pal;

    // Flatten cells to shared buffers.
    let mut keys: Vec<_> = cells.map.keys().copied().collect();
    keys.sort();
    let mut verts: Vec<Vertex> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    let mut chunks = Vec::new();
    for k in keys {
        let m = &cells.map[&k];
        if m.idx.is_empty() {
            continue;
        }
        let base = verts.len() as u32;
        let first = idx.len() as u32;
        let (lo, hi) = m.bounds();
        verts.extend_from_slice(&m.verts);
        idx.extend(m.idx.iter().map(|i| i + base));
        chunks.push(ChunkInfo { lo, hi, first, count: m.idx.len() as u32 });
    }
    // The animated decor: same flattening, own buffers.
    let mut dkeys: Vec<_> = dcells.map.keys().copied().collect();
    dkeys.sort();
    let mut decor_verts: Vec<DecorVertex> = Vec::new();
    let mut decor_idx: Vec<u32> = Vec::new();
    let mut decor_chunks = Vec::new();
    for k in dkeys {
        let m = &dcells.map[&k];
        if m.idx.is_empty() {
            continue;
        }
        let base = decor_verts.len() as u32;
        let first = decor_idx.len() as u32;
        let (lo, hi) = m.bounds();
        decor_verts.extend_from_slice(&m.verts);
        decor_idx.extend(m.idx.iter().map(|i| i + base));
        decor_chunks.push(ChunkInfo { lo, hi, first, count: m.idx.len() as u32 });
    }
    BakedWorld { verts, idx, chunks, pad_verts, pad_idx, decor_verts, decor_idx, decor_chunks, ground }
}

fn seg_pairs(w: &WorldGeometry) -> Vec<(usize, usize)> {
    let n = w.road.len();
    let mut v = Vec::new();
    if n < 2 {
        return v;
    }
    let last = if w.closed { n } else { n - 1 };
    for i in 0..last {
        let j = (i + 1) % n;
        if w.road[i].flags & road_flags::GAP != 0 || w.road[j].flags & road_flags::GAP != 0 {
            continue;
        }
        v.push((i, j));
    }
    v
}

fn lat_pt(s: &RoadSample, lat: f32, up: f32) -> Vec3 {
    s.pos + s.right * lat + s.up * up
}

/// A strip between two lateral offsets (and heights) over one segment.
#[allow(clippy::too_many_arguments)]
fn strip(b: &mut MeshData, a: &RoadSample, c: &RoadSample, l0: f32, h0: f32, l1: f32, h1: f32, st: &Style) {
    let p = [lat_pt(a, l0, h0), lat_pt(a, l1, h1), lat_pt(c, l1, h1), lat_pt(c, l0, h0)];
    b.quad4(p, a.up.lerp(c.up, 0.5), &Style { flat: true, ..*st });
}

fn bake_road(w: &WorldGeometry, br: &Bridges, cells: &mut Cells) {
    let pal = &w.palette;
    for (i, j) in seg_pairs(w) {
        let (a, c) = (&w.road[i], &w.road[j]);
        let mid = (a.pos + c.pos) * 0.5;
        let b = cells.at(mid);
        let hw = a.width * 0.5;
        let hwc = c.width * 0.5;
        let _ = hwc;
        let dash = i % 8 < 4;
        // A deck overhead shades the ground beneath it (lateral offset `l` from the centreline).
        let shd = |l: f32| if br.is_empty() { 1.0 } else { br.shade(w, mid + a.right * l) };
        // Shoulders (rise gently outwards) with a darker skirt below the outer edge.
        for (side, sh, flag) in [(-1.0f32, a.shoulder_l, road_flags::CURB_L), (1.0, a.shoulder_r, road_flags::CURB_R)] {
            let curb = a.flags & flag != 0 && c.flags & flag != 0;
            let inner = hw + if curb { 0.7 } else { 0.0 };
            let col = mul3(pal.shoulder, (0.9 + 0.2 * noise2(i as f32 * 0.2, side * 7.0, 9)) * shd(side * (hw + sh * 0.5)));
            if sh > inner - hw {
                strip(b, a, c, side * (inner), 0.0, side * (hw + sh), 0.22, &Style::rgb(col));
                // Skirt hides the underside of raised road.
                let e = hw + sh;
                let p = [lat_pt(a, side * e, 0.22), lat_pt(a, side * e, -1.4), lat_pt(c, side * e, -1.4), lat_pt(c, side * e, 0.22)];
                b.quad4(p, a.right * side, &Style::rgb(mul3(col, 0.55)).flat());
            }
            if curb {
                let on = (i / 2) % 2 == 0;
                let cc = mul3(if on { pal.curb_a } else { pal.curb_b }, shd(side * hw));
                strip(b, a, c, side * hw, 0.09, side * (hw + 0.7), 0.09, &Style::rgb(cc));
                // Inner face and outer slope.
                let p = [lat_pt(a, side * hw, 0.0), lat_pt(a, side * hw, 0.09), lat_pt(c, side * hw, 0.09), lat_pt(c, side * hw, 0.0)];
                b.quad4(p, -a.right * side, &Style::rgb(mul3(cc, 0.8)).flat());
                strip(b, a, c, side * (hw + 0.7), 0.09, side * (hw + 0.7), 0.0, &Style::rgb(mul3(cc, 0.7)));
            }
        }
        // Tarmac as non-overlapping strips (no coplanar layers, so no z-fighting): the body in five
        // lanes with subtle tone and patch variation, edge lines, the centre dashes, skid marks in
        // drift corners. Painted as lateral intervals, later layers cutting earlier ones.
        let n = w.road.len();
        let night = pal.night;
        let mut segs: Vec<(f32, f32, [f32; 3], f32)> = Vec::with_capacity(12);
        let lanes = [-hw, -hw * 0.55, -hw * 0.12, hw * 0.12, hw * 0.55, hw];
        for k in 0..5 {
            let tone = 0.975 + 0.05 * noise2(i as f32 * 0.07 + k as f32 * 13.1, k as f32 * 3.7, 5);
            // Wheel tracks (lanes 1 and 3) are a touch darker and glossier, the verge lanes dustier.
            let lane = if k == 1 || k == 3 { 0.97 } else if k == 0 || k == 4 { 1.015 } else { 1.0 };
            let patch = noise2(i as f32 * 0.045 + 40.0, k as f32 * 5.0 + 2.0, 8);
            let pm = if patch > 0.8 { 1.035 } else if patch < 0.17 { 0.98 } else { 1.0 };
            segs.push((lanes[k], lanes[k + 1], mul3(pal.road, tone * lane * pm * shd((lanes[k] + lanes[k + 1]) * 0.5)), 0.0));
        }
        let paint = |segs: &mut Vec<(f32, f32, [f32; 3], f32)>, lo: f32, hi: f32, col: [f32; 3], em: f32| {
            let mut out = Vec::with_capacity(segs.len() + 2);
            for &(l, h, c, e) in segs.iter() {
                if h <= lo || l >= hi {
                    out.push((l, h, c, e));
                    continue;
                }
                if l < lo {
                    out.push((l, lo, c, e));
                }
                if h > hi {
                    out.push((hi, h, c, e));
                }
            }
            out.push((lo, hi, col, em));
            out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            *segs = out;
        };
        // Skid marks: dashed dark pairs along the inside of a tight corner (drift lines).
        let fwd_at = |j: usize| {
            let s = &w.road[j % n];
            s.up.cross(s.right)
        };
        let turn = (fwd_at(i + 3) - fwd_at(i + n - 3)).dot(a.right) / 6.0;
        if turn.abs() > 0.011 && hash01(i as i32 / 7, 3, 71) > 0.3 {
            let c0 = turn.signum() * hw * 0.42;
            let dark = mul3(pal.road, 0.62);
            for off in [-0.5f32, 0.5] {
                paint(&mut segs, c0 + off - 0.1, c0 + off + 0.1, dark, 0.0);
            }
        }
        let line = mul3(pal.road_stripe, 0.95);
        let (edge_c, edge_e) = if night { (pal.curb_a, 0.9) } else { (line, 0.0) };
        paint(&mut segs, -hw + 0.18, -hw + 0.42, edge_c, edge_e);
        paint(&mut segs, hw - 0.42, hw - 0.18, edge_c, edge_e);
        if dash {
            paint(&mut segs, -0.11, 0.11, line, if night { 0.3 } else { 0.0 });
        }
        for &(l, h, col, e) in &segs {
            let st = if e > 0.0 { Style::rgb(col).emit(e) } else { Style::rgb(col) };
            strip(b, a, c, l, 0.0, h, 0.0, &st);
        }
    }
}

// ---------------------------------------------------------------- walls ----

fn bake_walls(w: &WorldGeometry, br: &Bridges, cells: &mut Cells) {
    let pal = &w.palette;
    let n = w.road.len();
    if n < 2 || w.wall == WallStyle::None {
        return;
    }
    let step = match w.wall {
        WallStyle::RockWall => 3,
        WallStyle::FencePosts => 5,
        WallStyle::NeonRail => 3,
        WallStyle::DuneBerm => 2,
        WallStyle::None => 1,
    };
    let count = if w.closed { n } else { n - 1 };
    let mut i0 = 0;
    while i0 < count {
        let i1 = i0 + step;
        let a = &w.road[i0 % n];
        let c = &w.road[i1.min(if w.closed { i1 } else { n - 1 }) % n];
        // Skip across gaps.
        let gap = (i0..=i1).any(|k| w.road[k % n].flags & road_flags::GAP != 0);
        // A deck carries its own parapet.
        if !gap && !br.covers(i0, i1) {
            for side in [-1.0f32, 1.0] {
                let sa = side * (a.width * 0.5 + if side < 0.0 { a.shoulder_l } else { a.shoulder_r } + 0.3);
                let sc = side * (c.width * 0.5 + if side < 0.0 { c.shoulder_l } else { c.shoulder_r } + 0.3);
                let g = (i0 / step) as i32;
                let seed = g * 2 + (side > 0.0) as i32;
                // Wall base heights follow the road edge plus the shoulder rise.
                let pa = a.pos + a.right * sa + Vec3::Y * 0.2;
                let pc = c.pos + c.right * sc + Vec3::Y * 0.2;
                let b = cells.at((pa + pc) * 0.5);
                let out_a = a.right * side;
                let out_c = c.right * side;
                match w.wall {
                    WallStyle::RockWall => rock_facet(b, pal, &w.fx, pa, pc, out_a, out_c, seed),
                    WallStyle::FencePosts => fence_section(b, pal, pa, pc, seed),
                    WallStyle::NeonRail => neon_section(b, pal, pa, pc, out_a, out_c, seed),
                    WallStyle::DuneBerm => berm_section(b, pal, pa, pc, out_a, out_c, seed),
                    WallStyle::None => {}
                }
            }
        }
        i0 = i1;
    }
}

#[allow(clippy::too_many_arguments)]
fn rock_facet(b: &mut MeshData, pal: &Palette, fx: &WorldFx, pa: Vec3, pc: Vec3, out_a: Vec3, out_c: Vec3, seed: i32) {
    let h = 2.6 + 2.6 * hash01(seed, 1, 11);
    let j = |k: i32| hash01(seed, k, 12) - 0.5;
    // Cross-section: (outward, height) from the base.
    let prof = [(0.0f32, 0.0f32), (0.15 + 0.3 * j(1), h * 0.5), (0.5 + 0.6 * j(2), h), (2.2, h - 0.5 - 0.5 * hash01(seed, 3, 13))];
    let tone = 0.96 + 0.08 * hash01(seed, 4, 14);
    let cols = [mul3(pal.wall, 0.82 * tone), mul3(pal.wall, tone), mul3(pal.wall, 1.08 * tone)];
    // Shared height bands keep neighbouring facets part of the same rock face.
    let strata = [0.92f32, 1.02, 0.96, 1.06, 0.94, 1.0];
    for k in 0..3 {
        let (l0, h0) = prof[k];
        let (l1, h1) = prof[k + 1];
        let toward = if k == 2 { Vec3::Y } else { -(out_a + out_c) * 0.5 + Vec3::Y * 0.2 };
        // The two near faces are cut into horizontal bands when the world asks for strata.
        let cuts: Vec<f32> = if fx.wall_strata && k < 2 {
            let step = 0.7;
            let mut cuts = vec![0.0];
            for layer in 1..9 {
                let f = (layer as f32 * step - h0) / (h1 - h0);
                if f > 0.001 && f < 0.999 { cuts.push(f); }
            }
            cuts.push(1.0);
            cuts
        } else { vec![0.0, 1.0] };
        for band in cuts.windows(2) {
            let (f0, f1) = (band[0], band[1]);
            let at = |base: Vec3, out: Vec3, f: f32| base + out * (l0 + (l1 - l0) * f) + Vec3::Y * (h0 + (h1 - h0) * f);
            let p = [at(pa, out_a, f0), at(pa, out_a, f1), at(pc, out_c, f1), at(pc, out_c, f0)];
            let col = if fx.wall_strata && k < 2 {
                let height = h0 + (h1 - h0) * (f0 + f1) * 0.5;
                let layer = (height / 0.7) as usize;
                let hue = lerp3(pal.wall, [0.75, 0.47, 0.31], 0.10 + 0.10 * (height * 0.8).sin());
                mul3(hue, strata[layer % strata.len()] * tone)
            } else { cols[k] };
            b.quad4(p, toward, &Style::rgb(col).flat());
        }
        // Glowing lava veins: a thin zig-zag crack up some facets.
        if let (Some(glow), true) = (fx.wall_glow, k < 2) {
            if hash01(seed, 31 + k as i32, 18) < 0.55 {
                let along = (pc - pa).normalize_or_zero() * 0.07;
                let o = (out_a + out_c) * -0.5 * 0.04;
                let pt = |u: f32, f: f32| {
                    let fa = pa + out_a * (l0 + (l1 - l0) * f) + Vec3::Y * (h0 + (h1 - h0) * f);
                    let fc = pc + out_c * (l0 + (l1 - l0) * f) + Vec3::Y * (h0 + (h1 - h0) * f);
                    fa + (fc - fa) * u + o
                };
                let mut u = 0.2 + 0.6 * hash01(seed, 40, 19);
                let mut f = 0.04 + 0.2 * hash01(seed, 41, 19);
                for j in 0..4 {
                    let u2 = (u + (hash01(seed, 50 + j, 19) - 0.5) * 0.5).clamp(0.08, 0.92);
                    let f2 = (f + 0.18 + 0.12 * hash01(seed, 60 + j, 19)).min(1.0);
                    let (p0, p1) = (pt(u, f), pt(u2, f2));
                    let q = [p0 - along, p0 + along, p1 + along, p1 - along];
                    b.quad4(q, toward, &Style::rgb(glow).emit(1.0).flat());
                    u = u2;
                    f = f2;
                    if f >= 1.0 {
                        break;
                    }
                }
            }
        }
    }
    // Back-fill so the wall is not paper thin from the side.
    let dpa = pa + out_a * 2.2 + Vec3::Y * prof[3].1;
    let dpc = pc + out_c * 2.2 + Vec3::Y * prof[3].1;
    b.quad4([dpa, dpa - Vec3::Y * (prof[3].1 + 2.0), dpc - Vec3::Y * (prof[3].1 + 2.0), dpc], (out_a + out_c) * 0.5, &Style::rgb(mul3(pal.wall, 0.5)).flat());
    // An occasional boulder at the foot.
    if hash01(seed, 5, 15) < 0.35 {
        let c = (pa + pc) * 0.5 + (out_a + out_c) * 0.5 * (0.6 + 0.5 * hash01(seed, 6, 16));
        let r = 0.8 + 0.8 * hash01(seed, 7, 17);
        b.sphere_jit(&(Mat4::from_translation(c + Vec3::Y * r * 0.4) * Mat4::from_scale(Vec3::new(r, r * 0.8, r))), 7, 5, &Style::rgb(mul3(pal.wall, 0.9)).flat(), 0.25, seed);
    }
}

fn fence_section(b: &mut MeshData, pal: &Palette, pa: Vec3, pc: Vec3, seed: i32) {
    let wood = mul3(pal.wall, 0.95 + 0.2 * hash01(seed, 1, 21));
    b.aabb(pa + Vec3::new(-0.09, -0.2, -0.09), pa + Vec3::new(0.09, 1.25, 0.09), &Style::rgb(wood).ao(0.3));
    b.aabb(pa + Vec3::new(-0.11, 1.25, -0.11), pa + Vec3::new(0.11, 1.33, 0.11), &Style::rgb(mul3(wood, 1.25)));
    for hgt in [0.5f32, 0.95] {
        let a = pa + Vec3::Y * hgt;
        let c = pc + Vec3::Y * hgt;
        let d = c - a;
        let len = d.length();
        if len < 0.01 {
            continue;
        }
        let u = d / len;
        let side = Vec3::Y.cross(u).normalize_or_zero();
        let m = basis((a + c) * 0.5, u * len, Vec3::Y * 0.1, side * 0.06);
        b.cube(&m, &Style::rgb(mul3(wood, 1.1)));
    }
}

fn neon_section(b: &mut MeshData, pal: &Palette, pa: Vec3, pc: Vec3, out_a: Vec3, out_c: Vec3, seed: i32) {
    let dark = mul3(pal.wall, 0.8);
    let a_in = pa + out_a * 0.0;
    let c_in = pc + out_c * 0.0;
    // Low barrier body.
    let p = |base: Vec3, out: Vec3, lat: f32, h: f32| base + out * lat + Vec3::Y * h;
    let toward = -(out_a + out_c) * 0.5;
    b.quad4([p(a_in, out_a, 0.0, 0.0), p(a_in, out_a, 0.0, 0.62), p(c_in, out_c, 0.0, 0.62), p(c_in, out_c, 0.0, 0.0)], toward, &Style::rgb(dark).flat());
    b.quad4([p(a_in, out_a, 0.0, 0.62), p(a_in, out_a, 0.5, 0.62), p(c_in, out_c, 0.5, 0.62), p(c_in, out_c, 0.0, 0.62)], Vec3::Y, &Style::rgb(mul3(dark, 1.2)).flat());
    // Glowing strip on the road-facing side.
    let g = Style::rgb(pal.curb_a).emit(1.0).flat();
    b.quad4([p(a_in, out_a, -0.012, 0.36), p(a_in, out_a, -0.012, 0.5), p(c_in, out_c, -0.012, 0.5), p(c_in, out_c, -0.012, 0.36)], toward, &g);
    if seed % 2 == 0 {
        b.aabb(pa + out_a * 0.25 + Vec3::new(-0.08, 0.0, -0.08), pa + out_a * 0.25 + Vec3::new(0.08, 1.25, 0.08), &Style::rgb(dark));
        b.sphere(&(Mat4::from_translation(pa + out_a * 0.25 + Vec3::Y * 1.3) * Mat4::from_scale(Vec3::splat(0.14))), 6, 4, &g);
    }
}

fn berm_section(b: &mut MeshData, pal: &Palette, pa: Vec3, pc: Vec3, out_a: Vec3, out_c: Vec3, seed: i32) {
    let hgt = 1.4 + 1.0 * hash01(seed >> 2, 1, 31);
    let prof = [(0.0f32, 0.0f32), (0.7, 0.55 * hgt), (1.7, hgt), (3.2, 0.8 * hgt), (5.2, 0.15 * hgt)];
    let col = |k: usize| mul3(pal.wall, 0.8 + 0.12 * k as f32);
    for k in 0..4 {
        let (l0, h0) = prof[k];
        let (l1, h1) = prof[k + 1];
        let p = [
            pa + out_a * l0 + Vec3::Y * h0,
            pa + out_a * l1 + Vec3::Y * h1,
            pc + out_c * l1 + Vec3::Y * h1,
            pc + out_c * l0 + Vec3::Y * h0,
        ];
        let toward = if k >= 2 { Vec3::Y + out_a * 0.2 } else { -(out_a + out_c) * 0.3 + Vec3::Y };
        b.quad4(p, toward, &Style::rgb(col(k)));
    }
}

// ---------------------------------------------------------- open field ----

/// Mown-grass variation on the open field: concentric stripes, and a rougher island in the middle.
fn mow(f: &OpenField, x: f32, z: f32, c: [f32; 3]) -> [f32; 3] {
    let r = (x - f.centre.x).hypot(z - f.centre.z);
    if r > f.outer_r + 4.0 {
        return c;
    }
    if r < f.island_r + 1.0 {
        // Unmown: greener and darker.
        return lerp3(mul3(c, 0.9), [0.42, 0.5, 0.2], 0.4);
    }
    let stripe = ((r - f.island_r) / 18.0).floor() as i32;
    let tone = if stripe % 2 == 0 { 1.04 } else { 0.96 };
    // Fade the pattern out at the fence so the hills beyond are plain.
    let fade = 1.0 - ((r - f.outer_r + 6.0) / 10.0).clamp(0.0, 1.0);
    mul3(c, 1.0 + (tone - 1.0) * fade)
}

/// The Paddock's landmark: a small windmill on the centre island.
fn bake_field(w: &WorldGeometry, f: &OpenField, cells: &mut Cells) {
    let pal = &w.palette;
    let b = cells.at(f.centre);
    let plaster = mul3(pal.wall, 0.95);
    let brick = [0.55, 0.3, 0.22];
    let roof = [0.5, 0.2, 0.14];
    let wood = [0.36, 0.24, 0.14];
    let sail = [0.96, 0.92, 0.82];
    let o = f.centre;
    // Brick footing, plastered tower, red cap.
    b.cyl(&(Mat4::from_translation(o) * Mat4::from_scale(Vec3::new(3.1, 1.3, 3.1))), 10, 1.0, 0.94, true, &Style::rgb(brick).flat().ao(0.3));
    b.cyl(&(Mat4::from_translation(o + Vec3::Y * 1.3) * Mat4::from_scale(Vec3::new(2.9, 8.2, 2.9))), 10, 1.0, 0.62, true, &Style::rgb(plaster).flat().ao(0.2));
    b.cyl(&(Mat4::from_translation(o + Vec3::Y * 9.4) * Mat4::from_scale(Vec3::new(2.15, 0.5, 2.15))), 10, 1.0, 1.0, true, &Style::rgb(wood).flat());
    b.cone(&(Mat4::from_translation(o + Vec3::Y * 9.9) * Mat4::from_scale(Vec3::new(2.3, 2.6, 2.3))), 10, &Style::rgb(roof).flat().ao(0.2));
    // Door and two windows on the +z face.
    b.cube(&(Mat4::from_translation(o + Vec3::new(0.0, 1.5, 3.0)) * Mat4::from_scale(Vec3::new(1.1, 2.2, 0.5))), &Style::rgb(wood).flat());
    for y in [5.0f32, 7.4] {
        b.cube(&(Mat4::from_translation(o + Vec3::new(0.0, y, 2.2)) * Mat4::from_scale(Vec3::new(0.7, 0.9, 0.5))), &Style::rgb([0.2, 0.22, 0.3]).flat());
    }
    // Hub and four sails on the +z face.
    let hub = o + Vec3::new(0.0, 8.4, 2.4);
    b.sphere(&(Mat4::from_translation(hub + Vec3::Z * 0.2) * Mat4::from_scale(Vec3::splat(0.55))), 7, 4, &Style::rgb(wood).flat());
    for k in 0..4 {
        let a = 0.4 + k as f32 * std::f32::consts::FRAC_PI_2;
        let d = Vec3::new(a.cos(), a.sin(), 0.0);
        let n = Vec3::new(-a.sin(), a.cos(), 0.0);
        b.cube(&basis(hub + d * 3.7 + Vec3::Z * 0.55, d * 7.4, n * 0.22, Vec3::Z * 0.2), &Style::rgb(wood).flat());
        b.cube(&basis(hub + d * 4.1 + n * 0.75 + Vec3::Z * 0.7, d * 5.6, n * 1.2, Vec3::Z * 0.08), &Style::rgb(sail).flat());
    }
}

// -------------------------------------------------------------- terrain ----

fn bake_terrain(w: &WorldGeometry, t: &Terrain, ground: &GroundQuery, br: &Bridges, cells: &mut Cells) {
    if t.w < 2 || t.h < 2 || t.heights.len() < t.w * t.h {
        return;
    }
    let pal = &w.palette;
    let at = |x: usize, z: usize| (t.origin.x + x as f32 * t.cell, t.origin.y + z as f32 * t.cell);
    let mut hts = t.heights.clone();
    // Sink the terrain under the drivable band, and a cell beyond it so no triangle
    // reaching the edge stands above the road: capped below the lowest road surface
    // there (banking included).
    for z in 0..t.h {
        for x in 0..t.w {
            let (wx, wz) = at(x, z);
            // (The open field has no ribbon: its terrain is already flat at the road height.)
            if w.field.is_some() {
                continue;
            }
            if let Some(cap) = ground.road_cap(wx, wz, t.cell * 1.1 + 0.5) {
                let k = z * t.w + x;
                hts[k] = hts[k].min(cap - 0.12);
            }
        }
    }
    let hn = |x: usize, z: usize| hts[z * t.w + x];
    let mut vs: Vec<(Vec3, Vec3, [f32; 3])> = Vec::with_capacity(t.w * t.h);
    for z in 0..t.h {
        for x in 0..t.w {
            let (wx, wz) = at(x, z);
            let h = hn(x, z);
            let xl = hn(x.saturating_sub(1), z);
            let xr = hn((x + 1).min(t.w - 1), z);
            let zl = hn(x, z.saturating_sub(1));
            let zr = hn(x, (z + 1).min(t.h - 1));
            let n = Vec3::new((xl - xr) / (2.0 * t.cell), 1.0, (zl - zr) / (2.0 * t.cell)).normalize();
            let big = noise2(wx * 0.021, wz * 0.021, 3);
            let fine = noise2(wx * 0.31, wz * 0.31, 4);
            let mut c = lerp3(pal.ground, pal.ground_alt, big);
            c = mul3(c, 0.92 + 0.16 * fine);
            // Steep slopes show rock.
            let slope = 1.0 - n.y;
            c = lerp3(c, mul3(pal.wall, 0.95), (slope * 2.2 - 0.25).clamp(0.0, 0.8));
            // Trampled ground beside the track.
            if let Some((i, lat, _)) = ground.band(wx, wz, 0.0) {
                let s = &ground.road[i];
                let edge = s.width * 0.5 + if lat < 0.0 { s.shoulder_l } else { s.shoulder_r };
                let d = lat.abs() - edge;
                if d < 14.0 {
                    c = lerp3(c, pal.shoulder, 0.55 * (1.0 - (d.max(0.0) / 14.0)));
                }
            }
            if !br.is_empty() {
                c = mul3(c, br.shade(w, Vec3::new(wx, h, wz)).max(0.72));
            }
            if let Some(f) = &w.field {
                c = mow(f, wx, wz, c);
            }
            vs.push((Vec3::new(wx, h, wz), n, c));
        }
    }
    for z in 0..t.h - 1 {
        for x in 0..t.w - 1 {
            let (i00, i10, i01, i11) = (z * t.w + x, z * t.w + x + 1, (z + 1) * t.w + x, (z + 1) * t.w + x + 1);
            let mid = (vs[i00].0 + vs[i11].0) * 0.5;
            let b = cells.at(mid);
            let base = b.verts.len() as u32;
            let st = Style::col(0.0, 0.0, 0.0);
            for &k in &[i00, i10, i01, i11] {
                let (p, n, c) = &vs[k];
                let mut v = vertex(*p, *n, &st, 1.0);
                v.col = [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, 0];
                b.verts.push(v);
            }
            // Pick the diagonal that follows the terrain.
            let flip = (vs[i00].0.y - vs[i11].0.y).abs() > (vs[i10].0.y - vs[i01].0.y).abs();
            if !flip {
                b.idx.extend_from_slice(&[base, base + 2, base + 1, base + 1, base + 2, base + 3]);
            } else {
                b.idx.extend_from_slice(&[base, base + 2, base + 3, base, base + 3, base + 1]);
            }
        }
    }
}

// ------------------------------------------------------------- scenery ----

struct Kit {
    acacia: MeshData,
    cactus: MeshData,
    rock: MeshData,
    dune: MeshData,
    mesa: MeshData,
    pylon: MeshData,
    grass: MeshData,
    fence: MeshData,
    bush: MeshData,
}

fn build_kit(pal: &Palette) -> Kit {
    let night = pal.night;
    let leaf = if night { [0.13, 0.2, 0.22] } else { lerp3([0.32, 0.5, 0.14], pal.ground_alt, 0.25) };
    let bark = if night { [0.14, 0.12, 0.16] } else { [0.36, 0.25, 0.16] };
    let cact = if night { [0.14, 0.3, 0.3] } else { [0.24, 0.52, 0.26] };

    let mut acacia = MeshData::new();
    acacia.cyl(&limb(Vec3::ZERO, Vec3::new(0.15, 1.9, 0.0), 0.26), 7, 1.0, 0.65, true, &Style::rgb(bark).flat().ao(0.3));
    for (dx, dz, s) in [(-0.9f32, 0.0f32, 1.0f32), (0.8, 0.5, 0.9), (0.1, -0.9, 0.8)] {
        acacia.cyl(&limb(Vec3::new(0.15, 1.8, 0.0), Vec3::new(dx, 3.0 + s * 0.2, dz), 0.14), 6, 1.0, 0.6, true, &Style::rgb(bark).flat());
    }
    for (i, (pos, size, shade)) in [
        (Vec3::new(-1.25, 3.45, 0.15), Vec3::new(1.9, 0.60, 2.1), 0.94),
        (Vec3::new(1.25, 3.50, 0.45), Vec3::new(1.9, 0.68, 1.8), 1.02),
        (Vec3::new(0.15, 3.55, -1.3), Vec3::new(2.0, 0.55, 1.55), 0.91),
        (Vec3::new(-0.5, 3.94, 0.45), Vec3::new(1.7, 0.60, 1.8), 1.12),
        (Vec3::new(0.9, 3.88, -0.8), Vec3::new(1.3, 0.50, 1.35), 1.06),
    ].into_iter().enumerate() {
        acacia.sphere_jit(&(Mat4::from_translation(pos) * Mat4::from_scale(size)), 8, 5,
            &Style::rgb(mul3(leaf, shade)).flat().ao(0.22), 0.09, 3 + i as i32);
    }
    for i in 0..4 {
        let a = i as f32 * PI * 0.5;
        acacia.cyl(&limb(Vec3::new(a.cos() * 0.43, 0.0, a.sin() * 0.43), Vec3::new(0.04, 0.6, 0.0), 0.09),
            5, 1.0, 0.7, true, &Style::rgb(bark).flat().ao(0.2));
    }

    let mut cactus = MeshData::new();
    let cs = Style::rgb(cact).flat().ao(0.3);
    cactus.cyl(&limb(Vec3::ZERO, Vec3::new(0.0, 3.0, 0.0), 0.32), 8, 1.0, 1.0, true, &cs);
    cactus.sphere(&(Mat4::from_translation(Vec3::new(0.0, 3.0, 0.0)) * Mat4::from_scale(Vec3::splat(0.32))), 8, 4, &cs);
    for (sx, y0, y1) in [(-1.0f32, 1.3f32, 2.4f32), (1.0, 1.7, 2.7)] {
        cactus.cyl(&limb(Vec3::new(0.0, y0, 0.0), Vec3::new(sx * 0.75, y0, 0.0), 0.18), 7, 1.0, 1.0, true, &cs);
        cactus.cyl(&limb(Vec3::new(sx * 0.75, y0, 0.0), Vec3::new(sx * 0.75, y1, 0.0), 0.17), 7, 1.0, 1.0, true, &cs);
        cactus.sphere(&(Mat4::from_translation(Vec3::new(sx * 0.75, y1, 0.0)) * Mat4::from_scale(Vec3::splat(0.17))), 7, 4, &cs);
    }

    let rock_c = mul3(pal.wall, 1.05);
    let mut rock = MeshData::new();
    rock.sphere_jit(&(Mat4::from_translation(Vec3::new(0.0, 0.35, 0.0)) * Mat4::from_scale(Vec3::new(1.5, 1.0, 1.2))), 7, 5, &Style::rgb(rock_c).flat().ao(0.4), 0.28, 9);
    rock.sphere_jit(&(Mat4::from_translation(Vec3::new(1.1, 0.2, 0.5)) * Mat4::from_scale(Vec3::new(0.8, 0.55, 0.7))), 6, 4, &Style::rgb(mul3(rock_c, 0.9)).flat().ao(0.4), 0.25, 10);

    let mut dune = MeshData::new();
    let dune_c = lerp3(pal.ground, pal.shoulder, 0.5);
    dune.lathe(
        &Mat4::from_scale(Vec3::new(14.0, 3.6, 9.0)),
        14,
        // Stepped wind ripples up the slope, flat shaded so every crest catches the light.
        &[(1.0, -0.2), (0.96, 0.08), (0.9, 0.2), (0.85, 0.18), (0.78, 0.36), (0.72, 0.34), (0.63, 0.55), (0.57, 0.53), (0.45, 0.76), (0.4, 0.74), (0.22, 0.92), (0.0, 1.0)],
        &Style::rgb(dune_c).flat().ao(0.25),
    );

    let mut mesa = MeshData::new();
    let tiers = [(0.0f32, 0.55f32, 1.0f32, 0.86f32, 0.9f32), (0.55, 0.82, 0.78, 0.72, 1.12), (0.82, 1.0, 0.62, 0.58, 0.82)];
    for (y0, y1, r0, r1, shade) in tiers {
        mesa.cyl(
            &(Mat4::from_translation(Vec3::new(0.0, y0 * 16.0, 0.0)) * Mat4::from_scale(Vec3::new(11.0, (y1 - y0) * 16.0, 8.0))),
            8,
            r0,
            r1,
            true,
            &Style::rgb(mul3(pal.wall, shade)).flat().ao(0.3),
        );
    }
    mesa.cyl(&(Mat4::from_translation(Vec3::new(0.0, -2.0, 0.0)) * Mat4::from_scale(Vec3::new(12.0, 3.0, 9.0))), 8, 1.1, 1.0, true, &Style::rgb(mul3(pal.wall, 0.75)).flat());

    let mut pylon = MeshData::new();
    let dark = mul3(pal.wall, 0.55);
    pylon.cube(&(Mat4::from_translation(Vec3::new(0.0, 0.3, 0.0)) * Mat4::from_scale(Vec3::new(1.1, 0.6, 1.1))), &Style::rgb(dark).flat());
    pylon.cube(&(Mat4::from_translation(Vec3::new(0.0, 4.0, 0.0)) * Mat4::from_scale(Vec3::new(0.5, 8.0, 0.5))), &Style::rgb(mul3(dark, 1.3)).flat());
    let glow = Style::rgb(pal.curb_a).emit(1.0).flat();
    for y in [1.5f32, 3.5, 5.5] {
        pylon.cube(&(Mat4::from_translation(Vec3::new(0.0, y, 0.0)) * Mat4::from_scale(Vec3::new(0.66, 0.22, 0.66))), &glow);
    }
    pylon.cone(&(Mat4::from_translation(Vec3::new(0.0, 8.0, 0.0)) * Mat4::from_scale(Vec3::new(0.45, 1.3, 0.45))), 4, &glow);

    let mut grass = MeshData::new();
    let g_c = lerp3(pal.ground_alt, [0.6, 0.65, 0.2], 0.4);
    for i in 0..7 {
        let a = i as f32 * 0.9;
        let lean = Vec3::new(a.cos(), 0.0, a.sin()) * (0.18 + 0.1 * (i % 3) as f32);
        let h = 0.7 + 0.35 * ((i * 5 % 7) as f32 / 7.0);
        grass.cone(&limb(Vec3::new(a.cos() * 0.05, 0.0, a.sin() * 0.05), Vec3::new(0.0, h, 0.0) + lean, 0.06), 4, &Style::rgb(mul3(g_c, 0.85 + 0.08 * i as f32)).flat());
    }

    let mut fence = MeshData::new();
    let wood = pal.wall;
    for x in [-3.0f32, 0.0, 3.0] {
        fence.cube(&(Mat4::from_translation(Vec3::new(x, 0.65, 0.0)) * Mat4::from_scale(Vec3::new(0.18, 1.3, 0.18))), &Style::rgb(wood).flat().ao(0.3));
    }
    for y in [0.55f32, 1.0] {
        fence.cube(&(Mat4::from_translation(Vec3::new(0.0, y, 0.0)) * Mat4::from_scale(Vec3::new(6.2, 0.1, 0.07))), &Style::rgb(mul3(wood, 1.1)).flat());
    }

    let mut bush = MeshData::new();
    for (x, z, r) in [(0.0f32, 0.0f32, 0.9f32), (0.8, 0.3, 0.65), (-0.6, 0.5, 0.6)] {
        bush.sphere_jit(&(Mat4::from_translation(Vec3::new(x, r * 0.6, z)) * Mat4::from_scale(Vec3::new(r, r * 0.8, r))), 7, 4, &Style::rgb(mul3(leaf, 0.9 + x * 0.1)).flat().ao(0.4), 0.15, (x * 10.0) as i32);
    }
    Kit { acacia, cactus, rock, dune, mesa, pylon, grass, fence, bush }
}

/// A low log on posts spanning `span` metres along x (the road's right axis), facing +z.
/// The log top is 0.6 m (race-core's hurdle height); orange and cream stripes read as "jump this".
fn hurdle_mesh(span: f32) -> MeshData {
    let mut b = MeshData::new();
    let wood = [0.34, 0.21, 0.12];
    let orange = [0.97, 0.58, 0.10];
    let cream = [0.96, 0.91, 0.80];
    let half = span * 0.5;
    // Posts at both ends and about every 3.2 m between.
    let n = (span / 3.2).ceil().max(1.0) as i32;
    for k in 0..=n {
        let x = -half + 0.14 + (span - 0.28) * k as f32 / n as f32;
        b.cube(&(Mat4::from_translation(Vec3::new(x, 0.28, 0.0)) * Mat4::from_scale(Vec3::new(0.2, 1.06, 0.2))), &Style::rgb(wood).flat().ao(0.3));
        // Orange cap so each post shows up from afar.
        b.cube(&(Mat4::from_translation(Vec3::new(x, 0.82, 0.0)) * Mat4::from_scale(Vec3::new(0.26, 0.1, 0.26))), &Style::rgb(orange).flat());
    }
    // The log: alternating stripes, each about 0.5 m.
    let seg = ((span / 0.5).round() as i32).max(2);
    let len = span / seg as f32;
    let r = 0.17;
    for i in 0..seg {
        let col = if i % 2 == 0 { orange } else { cream };
        let x0 = -half + i as f32 * len;
        b.cyl(&(Mat4::from_translation(Vec3::new(x0, 0.43, 0.0)) * Mat4::from_rotation_z(-std::f32::consts::FRAC_PI_2) * Mat4::from_scale(Vec3::new(r, len * 1.001, r))), 8, 1.0, 1.0, true, &Style::rgb(col).ao(0.1));
    }
    // Two small marker cones on the end posts.
    for s in [-1.0f32, 1.0] {
        b.cone(&(Mat4::from_translation(Vec3::new(s * (half - 0.14), 0.87, 0.0)) * Mat4::from_scale(Vec3::new(0.18, 0.42, 0.18))), 6, &Style::rgb(orange).emit(0.3).flat());
    }
    b
}

/// A gate arch with its posts `span` metres apart along x, facing +z: bitcoin-orange posts and a
/// banner with white bitcoin signs on both faces.
fn gate_mesh(span: f32, pal: &Palette) -> MeshData {
    let mut b = MeshData::new();
    let orange = [0.97, 0.58, 0.10];
    let dark = [0.16, 0.13, 0.22];
    let half = span * 0.5;
    let h = 4.8;
    let glow = Style::rgb(orange).emit(0.8).flat();
    let _ = pal;
    for s in [-1.0f32, 1.0] {
        let x = s * half;
        b.cube(&(Mat4::from_translation(Vec3::new(x, 0.0, 0.0)) * Mat4::from_scale(Vec3::new(1.0, 0.8, 1.0))), &Style::rgb(dark).flat());
        b.cube(&(Mat4::from_translation(Vec3::new(x, h * 0.5, 0.0)) * Mat4::from_scale(Vec3::new(0.46, h, 0.46))), &Style::rgb(orange).flat().ao(0.2));
        for y in [0.9f32, 2.0] {
            b.cube(&(Mat4::from_translation(Vec3::new(x, y, 0.0)) * Mat4::from_scale(Vec3::new(0.56, 0.16, 0.56))), &Style::rgb(dark).flat());
        }
        b.cube(&(Mat4::from_translation(Vec3::new(x, h + 0.12, 0.0)) * Mat4::from_scale(Vec3::new(0.62, 0.24, 0.62))), &glow);
    }
    // Banner: a dark frame, an orange panel and the bitcoin signs, both faces.
    let (yb, bh) = (h - 0.95, 1.45);
    let len = span + 0.4;
    b.cube(&(Mat4::from_translation(Vec3::new(0.0, yb, 0.0)) * Mat4::from_scale(Vec3::new(len, bh, 0.14))), &Style::rgb(dark).flat());
    for z in [0.085f32, -0.085] {
        b.cube(&(Mat4::from_translation(Vec3::new(0.0, yb, z)) * Mat4::from_scale(Vec3::new(len - 0.3, bh - 0.3, 0.05))), &Style::rgb(orange).flat());
    }
    let mut sign = MeshData::new();
    crate::meshes::btc_glyph(&mut sign, &Style::rgb([1.0, 0.97, 0.9]).flat());
    let gs = (bh - 0.5) / 0.56;
    let n = ((span / 3.2).round() as i32).max(1);
    for i in 0..n {
        let x = -half + span * (i as f32 + 0.5) / n as f32;
        let m = Mat4::from_translation(Vec3::new(x, yb, 0.0)) * Mat4::from_scale(Vec3::new(gs, gs, 1.4));
        b.append(&sign, &m);
    }
    // Glowing top bar.
    b.cube(&(Mat4::from_translation(Vec3::new(0.0, yb + bh * 0.5 + 0.1, 0.0)) * Mat4::from_scale(Vec3::new(len, 0.16, 0.3))), &glow);
    b
}

fn bake_scenery(w: &WorldGeometry, cells: &mut Cells, dcells: &mut DecorCells) {
    if w.scenery.is_empty() {
        return;
    }
    let kit = build_kit(&w.palette);
    let kit2 = decor::build_kit2(&w.palette);
    for s in &w.scenery {
        let hs = |k: i32| hash01((s.pos.x * 7.0) as i32, (s.pos.z * 7.0) as i32, 99 + k);
        let mesh = match s.kind {
            SceneryKind::Acacia => &kit.acacia,
            SceneryKind::Cactus => &kit.cactus,
            SceneryKind::Rock => &kit.rock,
            SceneryKind::Dune => &kit.dune,
            SceneryKind::Mesa => &kit.mesa,
            SceneryKind::NeonPylon => &kit.pylon,
            SceneryKind::GrassTuft => &kit.grass,
            SceneryKind::Fence => &kit.fence,
            SceneryKind::Bush => &kit.bush,
            SceneryKind::Baobab => &kit2.baobab,
            SceneryKind::Palm => &kit2.palm,
            SceneryKind::Reed => &kit2.reed,
            SceneryKind::HayBale => &kit2.hay,
            SceneryKind::Barrel => &kit2.barrel,
            SceneryKind::TyreStack => &kit2.tyres,
            SceneryKind::Tent => &kit2.tent,
            SceneryKind::Bones => &kit2.bones,
            SceneryKind::Hoodoo => &kit2.hoodoo,
            SceneryKind::LavaRock => &kit2.lava_rock,
            SceneryKind::Smoke => &kit2.smoke,
            SceneryKind::LilyPad => &kit2.lily,
            SceneryKind::Stands => &kit2.stands,
            SceneryKind::Tower => &kit2.towers[(hs(5) * 4.0) as usize % 4],
            SceneryKind::Hurdle | SceneryKind::Gate => {
                // Sized by `scale` (a span in metres), not uniformly scaled.
                let span = s.scale.max(1.0);
                let part = if s.kind == SceneryKind::Hurdle { hurdle_mesh(span) } else { gate_mesh(span, &w.palette) };
                let m = Mat4::from_translation(s.pos) * Mat4::from_rotation_y(s.yaw);
                cells.at(s.pos).append(&part, &m);
                continue;
            }
            // Animated / glowing decor goes to the decor buffers.
            SceneryKind::Spectator | SceneryKind::Flag | SceneryKind::CheckFlag | SceneryKind::Banner | SceneryKind::BtcSign | SceneryKind::Billboard => {
                let m = Mat4::from_translation(s.pos) * Mat4::from_rotation_y(s.yaw) * Mat4::from_scale(Vec3::splat(s.scale.max(0.01)));
                let phase = hs(1);
                let u8f = |x: f32| (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                let yaw8 = u8f((s.yaw / std::f32::consts::TAU).rem_euclid(1.0));
                let d = dcells.at(s.pos);
                match s.kind {
                    SceneryKind::Spectator => {
                        let mesh = &kit2.spectators[(hs(2) * 6.0) as usize % 6];
                        d.append(mesh, &m, 0.9 + 0.2 * hs(3), |lp, _| [decor::A_BOB, u8f(phase), u8f(lp.y / 2.3), 0]);
                    }
                    SceneryKind::Flag | SceneryKind::CheckFlag => {
                        let mesh = if s.kind == SceneryKind::Flag { &kit2.flag } else { &kit2.check_flag };
                        d.append(mesh, &m, 1.0, |lp, _| if lp.x > 0.06 { [decor::A_FLUTTER, u8f(phase), u8f((lp.x - 0.05) / 1.8), yaw8] } else { [0, 0, 0, 0] });
                    }
                    SceneryKind::Banner => {
                        d.append(&kit2.banner, &m, 1.0, |lp, _| if lp.x > 0.1 && lp.y < 6.85 { [decor::A_FLUTTER, u8f(phase), u8f((6.8 - lp.y) / 4.8), yaw8] } else { [0, 0, 0, 0] });
                    }
                    SceneryKind::BtcSign => d.append(&kit2.btc_sign, &m, 1.0, |_, e| if e > 0.5 { [decor::A_PULSE, u8f(phase), 0, 0] } else { [0, 0, 0, 0] }),
                    _ => d.append(&kit2.billboard, &m, 1.0, |_, e| if e > 0.5 { [decor::A_PULSE, u8f(phase), 0, 0] } else { [0, 0, 0, 0] }),
                }
                continue;
            }
        };
        let m = Mat4::from_translation(s.pos) * Mat4::from_rotation_y(s.yaw) * Mat4::from_scale(Vec3::splat(s.scale.max(0.01)));
        let shade = 0.88 + 0.24 * hs(0);
        cells.at(s.pos).append_shaded(mesh, &m, shade);
    }
}

// ---------------------------------------------------------- start & pads ----

fn bake_start(w: &WorldGeometry, ground: &GroundQuery, cells: &mut Cells, dcells: &mut DecorCells) {
    let Some(st) = w.start else { return };
    let pal = &w.palette;
    let right = Vec3::new(st.right.x, 0.0, st.right.z).normalize_or_zero();
    if right == Vec3::ZERO {
        return;
    }
    let fwd = Vec3::Y.cross(right).normalize();
    let hw = st.width * 0.5;
    let b = cells.at(st.pos);
    // Chequered line on the tarmac, following the road plane.
    let n = (st.width / 0.75).round().max(2.0) as i32;
    let cw = st.width / n as f32;
    let depth = 0.6;
    let hy = |p: Vec3| ground.height(p.x, p.z) + 0.03;
    for row in 0..2 {
        for k in 0..n {
            let x0 = -hw + k as f32 * cw;
            let col = if (k + row) % 2 == 0 { [0.96, 0.96, 0.94] } else { [0.08, 0.08, 0.1] };
            let z0 = (row as f32 - 1.0) * depth;
            let mk = |x: f32, z: f32| {
                let p = st.pos + right * x + fwd * z;
                Vec3::new(p.x, hy(p), p.z)
            };
            b.quad4([mk(x0, z0), mk(x0 + cw, z0), mk(x0 + cw, z0 + depth), mk(x0, z0 + depth)], Vec3::Y, &Style::rgb(col).flat());
        }
    }
    // Arch: two pillars and a beam with a chequered banner.
    let arch = st.pos + fwd * 0.3;
    let metal = [0.22, 0.22, 0.28];
    let glow = Style::rgb(pal.curb_a).emit(0.9).flat();
    for s in [-1.0f32, 1.0] {
        let c = arch + right * (s * (hw + 1.3));
        let y0 = ground.height(c.x, c.z);
        b.aabb(Vec3::new(c.x - 0.5, y0 - 0.5, c.z - 0.5), Vec3::new(c.x + 0.5, y0 + 7.2, c.z + 0.5), &Style::rgb(metal).flat().ao(0.3));
        b.aabb(Vec3::new(c.x - 0.55, y0 + 1.0, c.z - 0.55), Vec3::new(c.x + 0.55, y0 + 1.5, c.z + 0.55), &glow);
        b.aabb(Vec3::new(c.x - 0.55, y0 + 4.0, c.z - 0.55), Vec3::new(c.x + 0.55, y0 + 4.5, c.z + 0.55), &glow);
    }
    let yc = ground.height(arch.x, arch.z).max(st.pos.y);
    let beam_len = st.width + 3.6;
    let m = basis(arch + Vec3::Y * (yc + 6.7 - arch.y.min(yc)), right * beam_len, Vec3::Y * 1.1, fwd * 1.0);
    b.cube(&m, &Style::rgb(metal).flat());
    let top = basis(arch + Vec3::Y * (yc + 7.35 - arch.y.min(yc)), right * (beam_len - 0.6), Vec3::Y * 0.14, fwd * 0.6);
    b.cube(&top, &glow);
    // Banner.
    let cols = (beam_len / 0.9) as i32;
    let cwb = beam_len / cols as f32;
    for r in 0..2 {
        for k in 0..cols {
            let col = if (k + r) % 2 == 0 { [0.95, 0.95, 0.93] } else { [0.08, 0.08, 0.1] };
            let x0 = -beam_len * 0.5 + k as f32 * cwb;
            let yb = yc + 5.9 - 0.45 * r as f32 - 0.1;
            let p = [
                arch + right * x0 + Vec3::Y * (yb - arch.y.min(yc)),
                arch + right * (x0 + cwb) + Vec3::Y * (yb - arch.y.min(yc)),
                arch + right * (x0 + cwb) + Vec3::Y * (yb + 0.45 - arch.y.min(yc)),
                arch + right * x0 + Vec3::Y * (yb + 0.45 - arch.y.min(yc)),
            ];
            b.quad4(p, fwd, &Style::rgb(col).flat());
            b.quad4(p, -fwd, &Style::rgb(mul3(col, 0.9)).flat());
        }
    }
    // Countdown lights: a dark bar under the banner with five pods per face, lit by the frame's `start_lights`.
    let base_y = arch.y.max(yc);
    let bar_y = base_y + 4.2;
    let dark = Style::rgb([0.08, 0.08, 0.1]).flat();
    let faces = [fwd, -fwd];
    b.cube(&basis(Vec3::new(arch.x, bar_y, arch.z), right * 7.2, Vec3::Y * 1.2, fwd * 0.6), &dark);
    for sgn in [-1.0f32, 1.0] {
        let hx = arch + right * (sgn * 3.2);
        b.aabb(Vec3::new(hx.x - 0.05, bar_y + 0.6, hx.z - 0.05), Vec3::new(hx.x + 0.05, base_y + 5.9, hx.z + 0.05), &dark);
    }
    let mut pod = MeshData::new();
    pod.sphere(&(Mat4::from_scale(Vec3::splat(0.46))), 8, 5, &Style::rgb([1.0, 1.0, 1.0]).emit(1.0).flat());
    let d = dcells.at(st.pos);
    for face in faces {
        for k in 0..5 {
            let x = (k as f32 - 2.0) * 1.35;
            let c = arch + right * x + face * 0.38;
            let m = Mat4::from_translation(Vec3::new(c.x, bar_y, c.z));
            let a = (((k as f32) + 0.5) / 5.0 * 255.0) as u8;
            d.append(&pod, &m, 1.0, |_, _| [decor::A_LIGHT, a, 0, 0]);
        }
    }
}

fn bake_pads(w: &WorldGeometry, ground: &GroundQuery) -> (Vec<PadVertex>, Vec<u32>) {
    let mut vs = Vec::new();
    let mut idx = Vec::new();
    // Ramp chevrons share the pad pipeline: a negative half width in `size.x` selects the ramp look.
    for (p, ramp) in w.pads.iter().map(|p| (p, false)).chain(w.fx.ramps.iter().map(|p| (p, true))) {
        let fwd = Vec3::new(p.yaw.sin(), 0.0, p.yaw.cos());
        let right = Vec3::new(-p.yaw.cos(), 0.0, p.yaw.sin());
        let nl = ((p.length / 1.2).ceil() as usize).clamp(2, 40);
        let nw = 4usize;
        let base = vs.len() as u32;
        for iz in 0..=nl {
            let z = -p.length * 0.5 + p.length * iz as f32 / nl as f32;
            for ix in 0..=nw {
                let x = -p.width * 0.5 + p.width * ix as f32 / nw as f32;
                let q = p.pos + right * x + fwd * z;
                let y = if ground.band(q.x, q.z, 1.0).map_or(false, |b| b.2) { ground.height(q.x, q.z) } else { p.pos.y };
                vs.push(PadVertex {
                    pos: [q.x, y + 0.05, q.z],
                    uv: [x, z + p.length * 0.5],
                    size: [if ramp { -p.width * 0.5 } else { p.width * 0.5 }, p.length],
                });
            }
        }
        let row = (nw + 1) as u32;
        for iz in 0..nl as u32 {
            for ix in 0..nw as u32 {
                let a = base + iz * row + ix;
                let (b, c, d) = (a + 1, a + row, a + row + 1);
                // Seen from above: right = -X ... orientation fixed by cull_mode None on the pad pipeline.
                idx.extend_from_slice(&[a, c, b, b, c, d]);
            }
        }
    }
    (vs, idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec2;

    #[test]
    fn training_meshes_have_the_right_size() {
        let (lo, hi) = hurdle_mesh(12.0).bounds();
        // Log top at the sim's hurdle height (0.6 m), spanning the road, posts a little taller.
        assert!(hi.y > 0.6 && hi.y < 1.4 && lo.y < 0.0, "{lo:?} {hi:?}");
        assert!((hi.x - lo.x - 12.0).abs() < 0.5, "{lo:?} {hi:?}");
        let (lo, hi) = gate_mesh(15.0, &Palette::savanna()).bounds();
        assert!(hi.y > 4.5 && (hi.x - lo.x) > 15.0, "{lo:?} {hi:?}");
    }

    pub fn oval_world(n: usize) -> WorldGeometry {
        let mut road = Vec::new();
        for i in 0..n {
            let a = std::f32::consts::TAU * i as f32 / n as f32;
            let pos = Vec3::new(a.cos() * 120.0, (a * 2.0).sin() * 2.0, a.sin() * 70.0);
            let tan = Vec3::new(-a.sin() * 120.0, 0.0, a.cos() * 70.0).normalize();
            let right = tan.cross(Vec3::Y).normalize();
            let flags = if i % 50 < 10 { road_flags::CURB } else { 0 };
            road.push(RoadSample { pos, right, up: Vec3::Y, width: 14.0, shoulder_l: 4.0, shoulder_r: 4.0, flags });
        }
        WorldGeometry {
            road,
            closed: true,
            terrain: Some(Terrain { origin: Vec2::new(-200.0, -150.0), cell: 8.0, w: 50, h: 38, heights: vec![0.0; 50 * 38] }),
            wall: WallStyle::RockWall,
            palette: Palette::canyon(),
            scenery: vec![Scenery { kind: SceneryKind::Mesa, pos: Vec3::new(150.0, 0.0, 60.0), yaw: 0.3, scale: 1.0 }],
            pads: vec![BoostPad { pos: Vec3::new(120.0, 0.0, 0.0), yaw: std::f32::consts::PI, length: 8.0, width: 5.0 }],
            start: Some(StartLine { pos: Vec3::new(120.0, 0.0, 0.0), right: Vec3::new(-1.0, 0.0, 0.0), width: 14.0 }),
            fx: WorldFx::default(),
            bridges: Vec::new(),
            field: None,
        }
    }

    /// A lower road along x (y = 0) and a deck road along z at y = 7, concatenated (open ribbon).
    fn overpass_world() -> WorldGeometry {
        let mut w = oval_world(8);
        w.road.clear();
        for i in -100..=100 {
            w.road.push(RoadSample { pos: Vec3::new(i as f32, 0.0, 0.0), right: Vec3::Z, up: Vec3::Y, width: 14.0, shoulder_l: 4.0, shoulder_r: 4.0, flags: 0 });
        }
        let first = w.road.len();
        for i in -40..=40 {
            w.road.push(RoadSample { pos: Vec3::new(0.0, 7.0, i as f32), right: -Vec3::X, up: Vec3::Y, width: 14.0, shoulder_l: 4.0, shoulder_r: 4.0, flags: 0 });
        }
        w.closed = false;
        w.wall = WallStyle::FencePosts;
        w.start = None;
        w.pads.clear();
        w.scenery.clear();
        w.bridges = vec![Bridge { first: first + 20, last: first + 60 }];
        w
    }

    #[test]
    fn a_bridge_adds_a_deck_pillars_and_a_shadow() {
        let mut with = overpass_world();
        // Count the bridge itself without the plain fence it replaces along the deck.
        with.wall = WallStyle::None;
        // An overhead sun keeps the shadow on the lower road in this fixture.
        with.palette.sun_dir = Vec3::Y;
        let mut without = with.clone();
        without.bridges.clear();
        let (a, b) = (bake(&with), bake(&without));
        let extra = (a.idx.len() - b.idx.len()) / 3;
        assert!(extra > 100 && extra < 1500, "{extra} triangles added by one bridge");
        // Pillar footings sit at the terrain (y = 0) less 0.3 m: at least four columns of 8-gon caps.
        let feet = a.verts.iter().filter(|v| (v.pos[1] + 0.3).abs() < 0.01).count();
        assert!(feet >= 4 * 8, "{feet} pillar base vertices");
        let br = Bridges::new(&with);
        assert!(br.shade(&with, Vec3::new(0.0, 0.0, 0.0)) < 1.0, "the road under the deck is in its shade");
        assert_eq!(br.shade(&with, Vec3::new(60.0, 0.0, 0.0)), 1.0);
        assert!(br.covers(with.road.len() - 60, with.road.len() - 59));
        // 3D ground lookup: under the deck the lower road, on the deck the deck.
        assert!(a.ground.height_y(0.0, 0.2, 0.0) < 0.5);
        assert!((a.ground.height_y(0.0, 7.2, 0.0) - 7.0).abs() < 0.5);
    }

    #[test]
    fn open_field_draws_no_ribbon() {
        let mut w = oval_world(400);
        let plain = bake(&w).idx.len();
        w.field = Some(OpenField { centre: Vec3::ZERO, island_r: 15.0, outer_r: 100.0, landmark: true });
        w.start = None;
        let b = bake(&w);
        assert!(b.idx.len() < plain, "no ribbon, no start gantry");
        assert!(!b.chunks.is_empty());
    }

    #[test]
    fn bake_is_valid_for_every_wall_style() {
        for style in [WallStyle::RockWall, WallStyle::FencePosts, WallStyle::NeonRail, WallStyle::DuneBerm, WallStyle::None] {
            let mut w = oval_world(400);
            w.wall = style;
            let b = bake(&w);
            assert!(!b.chunks.is_empty());
            for &i in &b.idx {
                assert!((i as usize) < b.verts.len());
            }
            let mut end = 0;
            for c in &b.chunks {
                assert_eq!(c.first, end);
                end += c.count;
                assert!(c.lo.is_finite() && c.hi.is_finite() && c.lo.x <= c.hi.x);
            }
            assert_eq!(end as usize, b.idx.len());
            assert!(b.idx.len() / 3 < 250_000, "{style:?}: {} tris", b.idx.len() / 3);
            for v in &b.verts {
                assert!(v.pos.iter().all(|x| x.is_finite()));
            }
        }
    }

    #[test]
    fn ground_query_finds_road_and_terrain() {
        let w = oval_world(400);
        let g = GroundQuery::new(&w);
        let on = w.road[0].pos;
        let (h, n) = g.height_normal(on.x, on.z);
        assert!((h - on.y).abs() < 0.2, "{h} vs {}", on.y);
        assert!(n.y > 0.9);
        let (h2, _) = g.height_normal(1000.0, 1000.0);
        assert!(h2.is_finite());
    }

    #[test]
    fn terrain_never_covers_the_road() {
        let mut w = oval_world(400);
        for s in w.road.iter_mut() {
            s.pos.y = 0.0;
        }
        for i in 100..140 {
            w.road[i].flags |= road_flags::GAP;
        }
        w.start = None;
        w.pads.clear();
        w.scenery.clear();
        w.wall = WallStyle::None;
        let t = w.terrain.as_mut().unwrap();
        for h in t.heights.iter_mut() {
            *h = 0.6;
        }
        let b = bake(&w);
        let g = GroundQuery::new(&w);
        let mut bad = 0;
        for v in &b.verts {
            let (x, y, z) = (v.pos[0], v.pos[1], v.pos[2]);
            if let Some((i, lat, on)) = g.band(x, z, -0.5) {
                let s = &w.road[i];
                if on && lat.abs() < s.width * 0.5 && y > 0.3 {
                    bad += 1;
                }
            }
        }
        assert_eq!(bad, 0, "{bad} vertices above the tarmac");
    }

    #[test]
    fn gap_leaves_no_road() {
        let mut w = oval_world(400);
        let full = bake(&w).idx.len();
        for i in 100..140 {
            w.road[i].flags |= road_flags::GAP;
        }
        assert!(bake(&w).idx.len() < full);
    }
}
