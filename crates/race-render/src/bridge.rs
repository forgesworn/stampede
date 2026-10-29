//! Overpass decks (spec 18.1): the extra geometry around a bridge span of the road ribbon, and the
//! deck's shadow on the road beneath. The ribbon itself (top surface, shoulders, skirt) is still baked
//! by `bake_road`; this adds the underside, a parapet with a deeper fascia, and pillars down to the
//! terrain, all in the same lit pipeline and chunk buffers (a few hundred triangles per bridge).

use crate::color::mul3;
use crate::mesh::{basis, Style};
use crate::types::*;
use crate::world::{Cells, GroundQuery};
use glam::{Mat4, Vec3};

/// How far the slab runs beyond the span the track reports (m): room for pillars clear of the lower road.
const EXTEND: f32 = 12.0;
/// Thickness of the slab below the road surface (matches the ribbon skirt's depth).
pub const DECK_DEPTH: f32 = 1.4;
/// Parapet height above the deck and the thickness it adds beyond the drivable edge.
const PARAPET_H: f32 = 1.05;
const PARAPET_IN: f32 = 0.15;
const PARAPET_OUT: f32 = 0.4;
/// Pillar radius.
const PILLAR_R: f32 = 0.85;
/// The deck's shade on the road beneath (colour multiplier).
const SHADOW: f32 = 0.6;

pub(crate) struct Bridges {
    /// Per road sample: on an (extended) deck.
    pub deck: Vec<bool>,
    /// Extended slabs as absolute sample ranges `first..=last` (may run past `road.len()`).
    slabs: Vec<(i64, i64)>,
    sun: Vec3,
}

impl Bridges {
    pub fn new(w: &WorldGeometry) -> Bridges {
        let n = w.road.len();
        let mut deck = vec![false; n];
        let mut slabs = Vec::new();
        if n > 8 && !w.bridges.is_empty() {
            let ds = (w.road[1].pos - w.road[0].pos).length().max(0.2);
            let ext = (EXTEND / ds).round() as i64;
            for b in &w.bridges {
                let (f, l) = (b.first as i64 - ext, b.last as i64 + ext);
                // Never wrap the whole lap.
                if l - f + 1 >= n as i64 {
                    continue;
                }
                for k in f..=l {
                    deck[k.rem_euclid(n as i64) as usize] = true;
                }
                slabs.push((f, l));
            }
        }
        let sun = if w.palette.sun_dir.y > 0.05 { w.palette.sun_dir.normalize() } else { Vec3::new(0.3, 0.9, 0.2).normalize() };
        Bridges { deck, slabs, sun }
    }

    pub fn is_empty(&self) -> bool {
        self.slabs.is_empty()
    }

    /// Whether the wall segment covering samples `i0..=i1` runs along a deck.
    pub fn covers(&self, i0: usize, i1: usize) -> bool {
        let n = self.deck.len();
        (i0..=i1).any(|k| self.deck[k % n])
    }

    /// Colour multiplier at a point on the ground (the road, shoulders, terrain) shaded by a deck above it.
    pub fn shade(&self, w: &WorldGeometry, p: Vec3) -> f32 {
        let n = w.road.len() as i64;
        for &(f, l) in &self.slabs {
            let mid = &w.road[((f + l) / 2).rem_euclid(n) as usize];
            let reach = (l - f) as f32 * 0.5 + 40.0;
            if (mid.pos.x - p.x).abs() > reach || (mid.pos.z - p.z).abs() > reach {
                continue;
            }
            let dy = mid.pos.y - p.y;
            if dy < 2.0 {
                continue;
            }
            // Where the ray from `p` towards the sun crosses the deck's height.
            let q = Vec3::new(p.x + self.sun.x / self.sun.y * dy, 0.0, p.z + self.sun.z / self.sun.y * dy);
            let mut best = (f32::MAX, 0i64);
            for k in f..=l {
                let s = &w.road[k.rem_euclid(n) as usize];
                let d = (s.pos.x - q.x).powi(2) + (s.pos.z - q.z).powi(2);
                if d < best.0 {
                    best = (d, k);
                }
            }
            let s = &w.road[best.1.rem_euclid(n) as usize];
            let (ox, oz) = (q.x - s.pos.x, q.z - s.pos.z);
            let lat = ox * s.right.x + oz * s.right.z;
            let fwd = s.up.cross(s.right);
            let lon = ox * fwd.x + oz * fwd.z;
            let e = s.width * 0.5 + if lat < 0.0 { s.shoulder_l } else { s.shoulder_r } + PARAPET_OUT;
            let ends_ok = !(best.1 == f && lon < -0.7 || best.1 == l && lon > 0.7);
            if lat.abs() <= e && ends_ok {
                return SHADOW;
            }
        }
        1.0
    }
}

fn at(w: &WorldGeometry, k: i64) -> &RoadSample {
    &w.road[k.rem_euclid(w.road.len() as i64) as usize]
}

fn lat_pt(s: &RoadSample, lat: f32, up: f32) -> Vec3 {
    s.pos + s.right * lat + s.up * up
}

/// Half width of a sample's drivable band plus shoulder on `side` (-1 left, +1 right).
fn edge(s: &RoadSample, side: f32) -> f32 {
    s.width * 0.5 + if side < 0.0 { s.shoulder_l } else { s.shoulder_r }
}

/// Underside, fascia, parapet and pillars of every bridge.
pub(crate) fn bake(w: &WorldGeometry, br: &Bridges, ground: &GroundQuery, cells: &mut Cells) {
    let n = w.road.len() as i64;
    let concrete = mul3(w.palette.wall, 0.82);
    let under = Style::rgb(mul3(concrete, 0.5)).flat();
    let fascia = Style::rgb(mul3(concrete, 0.72)).flat();
    let top = Style::rgb(mul3(concrete, 1.12)).flat();
    let inner = Style::rgb(mul3(concrete, 0.95)).flat();
    for &(f, l) in &br.slabs {
        // Underside and fascia, one sample pair at a time.
        for k in f..l {
            let (a, c) = (at(w, k), at(w, k + 1));
            let b = cells.at((a.pos + c.pos) * 0.5);
            let (el_a, er_a) = (edge(a, -1.0) + PARAPET_OUT, edge(a, 1.0) + PARAPET_OUT);
            let (el_c, er_c) = (edge(c, -1.0) + PARAPET_OUT, edge(c, 1.0) + PARAPET_OUT);
            let down = -(a.up + c.up) * 0.5;
            b.quad4([lat_pt(a, -el_a, -DECK_DEPTH), lat_pt(a, er_a, -DECK_DEPTH), lat_pt(c, er_c, -DECK_DEPTH), lat_pt(c, -el_c, -DECK_DEPTH)], down, &under);
            for (side, ea, ec) in [(-1.0f32, el_a, el_c), (1.0, er_a, er_c)] {
                let out = (a.right + c.right) * 0.5 * side;
                // Fascia: from the parapet top down to the underside, on the outer face.
                b.quad4([lat_pt(a, side * ea, PARAPET_H), lat_pt(a, side * ea, -DECK_DEPTH), lat_pt(c, side * ec, -DECK_DEPTH), lat_pt(c, side * ec, PARAPET_H)], out, &fascia);
                // Parapet top and inner face.
                let (ia, ic) = (ea - PARAPET_OUT - PARAPET_IN, ec - PARAPET_OUT - PARAPET_IN);
                b.quad4([lat_pt(a, side * ia, PARAPET_H), lat_pt(a, side * ea, PARAPET_H), lat_pt(c, side * ec, PARAPET_H), lat_pt(c, side * ic, PARAPET_H)], Vec3::Y, &top);
                b.quad4([lat_pt(a, side * ia, 0.1), lat_pt(a, side * ia, PARAPET_H), lat_pt(c, side * ic, PARAPET_H), lat_pt(c, side * ic, 0.1)], -out, &inner);
            }
        }
        // The two ends: the slab's cut face (the ribbon carries on as a plain skirted road).
        for (k, dir) in [(f, -1.0f32), (l, 1.0)] {
            let s = at(w, k);
            let b = cells.at(s.pos);
            let fwd = s.up.cross(s.right) * dir;
            let (el, er) = (edge(s, -1.0) + PARAPET_OUT, edge(s, 1.0) + PARAPET_OUT);
            // Below the road surface across the width, and the parapet's own end on each side.
            b.quad4([lat_pt(s, -el, 0.25), lat_pt(s, er, 0.25), lat_pt(s, er, -DECK_DEPTH), lat_pt(s, -el, -DECK_DEPTH)], fwd, &fascia);
            for (side, e) in [(-1.0f32, el), (1.0, er)] {
                let i = e - PARAPET_OUT - PARAPET_IN;
                b.quad4([lat_pt(s, side * i, 0.1), lat_pt(s, side * e, 0.1), lat_pt(s, side * e, PARAPET_H), lat_pt(s, side * i, PARAPET_H)], fwd, &top);
            }
        }
        pillars(w, f, l, ground, cells, &concrete, n);
    }
}

/// Pillars: per side the clear stations nearest each end of the slab and nearest the middle of each half,
/// where "clear" means outside every lower road's band (and its wall) by the pillar's footing.
fn pillars(w: &WorldGeometry, f: i64, l: i64, ground: &GroundQuery, cells: &mut Cells, concrete: &[f32; 3], n: i64) {
    let mid = at(w, (f + l) / 2);
    let deck_y = mid.pos.y;
    // Lower-road samples near the slab (the other branch).
    let low: Vec<&RoadSample> = w
        .road
        .iter()
        .filter(|s| s.flags & road_flags::GAP == 0 && s.pos.y < deck_y - 3.0 && (s.pos.x - mid.pos.x).abs() < 80.0 && (s.pos.z - mid.pos.z).abs() < 80.0)
        .collect();
    let clear = |p: Vec3, r: f32| -> bool {
        low.iter().all(|s| {
            let d = ((s.pos.x - p.x).powi(2) + (s.pos.z - p.z).powi(2)).sqrt();
            d > s.width * 0.5 + s.shoulder_l.max(s.shoulder_r) + 0.7 + r + 1.0
        })
    };
    let stations: Vec<i64> = (f..=l).step_by(2).collect();
    let mut chosen: Vec<(i64, f32)> = Vec::new();
    for side in [-1.0f32, 1.0] {
        let ok: Vec<i64> = stations
            .iter()
            .copied()
            .filter(|&k| {
                let s = at(w, k);
                let lat = side * (edge(s, side) - 2.6);
                clear(s.pos + s.right * lat, PILLAR_R * 1.5)
            })
            .collect();
        if ok.is_empty() {
            continue;
        }
        let centre = (f + l) as f32 * 0.5;
        // Slab ends, and the stations nearest the middle on each half (short spans).
        let mut pick = vec![ok[0], *ok.last().unwrap()];
        if let Some(&k) = ok.iter().filter(|&&k| (k as f32) < centre).max() {
            pick.push(k);
        }
        if let Some(&k) = ok.iter().filter(|&&k| (k as f32) >= centre).min() {
            pick.push(k);
        }
        pick.sort();
        pick.dedup();
        // Drop picks closer than 7 m (samples) to an earlier one.
        let mut last: Option<i64> = None;
        for k in pick {
            if last.map_or(true, |p| k - p >= 7) {
                chosen.push((k, side));
                last = Some(k);
            }
        }
    }
    let col = Style::rgb(mul3(*concrete, 0.9)).ao(0.3);
    let foot = Style::rgb(mul3(*concrete, 0.7)).flat();
    let cap = Style::rgb(mul3(*concrete, 0.6)).flat();
    for (k, side) in chosen {
        let s = at(w, k);
        let lat = side * (edge(s, side) - 2.6);
        let p = s.pos + s.right * lat;
        let base_y = ground.terrain_height(p.x, p.z).unwrap_or(s.pos.y - 8.0) - 0.3;
        let top_y = lat_pt(s, lat, -DECK_DEPTH).y + 0.05;
        if top_y - base_y < 0.8 {
            continue;
        }
        let b = cells.at(p);
        let fwd = s.up.cross(s.right);
        // Shaft (slightly tapered), flared footing, and a capital block under the deck.
        b.cyl(&(Mat4::from_translation(Vec3::new(p.x, base_y, p.z)) * Mat4::from_scale(Vec3::new(PILLAR_R, top_y - base_y, PILLAR_R))), 8, 1.0, 0.85, false, &col);
        b.cyl(&(Mat4::from_translation(Vec3::new(p.x, base_y, p.z)) * Mat4::from_scale(Vec3::new(PILLAR_R * 1.5, 0.7, PILLAR_R * 1.5))), 8, 1.0, 0.7, true, &foot);
        let cap_m = basis(Vec3::new(p.x, top_y - 0.3, p.z), fwd * (PILLAR_R * 3.4), Vec3::Y * 0.6, s.right * (PILLAR_R * 2.6));
        b.cube(&cap_m, &cap);
        let _ = n;
    }
}
