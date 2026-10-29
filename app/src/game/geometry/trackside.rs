//! Trackside decoration derived from a track: crowds, flags, banners, tyre walls, ponds and lava,
//! per-theme props, the sky look. All of it render-side: it only adds renderer scenery, fluids and
//! effects (and carves basins into the render terrain); the sim never sees any of it.

use glam::{Vec2, Vec3};
use race_core::math::yaw_of;
use race_core::{Feature, Track, TrackId};
use race_render::{BoostPad, Fluid, FluidKind, HillStyle, MoteKind, Scenery, SceneryKind, SkyFx, Terrain, WorldFx};

struct Rng(std::cell::Cell<u64>);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(std::cell::Cell::new(seed ^ 0x9E37_79B9_7F4A_7C15))
    }
    fn next(&self) -> u64 {
        self.0.set(self.0.get().wrapping_add(0x9E37_79B9_7F4A_7C15));
        let mut z = self.0.get();
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// 0..1
    fn f(&self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.f()
    }
    fn side(&self) -> f32 {
        if self.f() < 0.5 {
            -1.0
        } else {
            1.0
        }
    }
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A run of tight curvature.
#[derive(Clone, Copy, Debug)]
struct Corner {
    s0: f32,
    s1: f32,
    /// +1 turning left, -1 right.
    dir: f32,
    peak: f32,
}

fn corners(t: &Track) -> Vec<Corner> {
    let n = t.samples.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if t.samples[i].curvature.abs() > 0.008 {
            let s0 = i;
            let dir = t.samples[i].curvature.signum();
            let mut pk = 0.0f32;
            while i < n && t.samples[i].curvature.abs() > 0.006 && t.samples[i].curvature.signum() == dir {
                pk = pk.max(t.samples[i].curvature.abs());
                i += 1;
            }
            if i - s0 >= 18 {
                out.push(Corner { s0: s0 as f32, s1: i as f32, dir, peak: pk });
            }
        }
        i += 1;
    }
    out
}

/// Stretches of road that are nearly straight for at least `min_len` metres: (start, end).
fn straights(t: &Track, min_len: usize) -> Vec<(f32, f32)> {
    let n = t.samples.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if t.samples[i].curvature.abs() < 0.003 {
            let s0 = i;
            while i < n && t.samples[i].curvature.abs() < 0.0045 {
                i += 1;
            }
            if i - s0 >= min_len {
                out.push((s0 as f32, i as f32));
            }
        }
        i += 1;
    }
    out
}

struct Ctx<'a> {
    t: &'a Track,
    terrain: &'a mut Terrain,
    rng: Rng,
    out: Vec<Scenery>,
    fluids: Vec<Fluid>,
    /// Basins already carved: (centre x, centre z, radius).
    basins: Vec<(f32, f32, f32)>,
    wall_out: f32,
    gaps: Vec<(f32, f32)>,
}

impl Ctx<'_> {
    fn len(&self) -> f32 {
        self.t.length
    }

    fn sm(&self, s: f32) -> race_core::TrackSample {
        self.t.sample_at(self.t.wrap_s(s))
    }

    fn in_gap(&self, s: f32) -> bool {
        let s = self.t.wrap_s(s);
        self.gaps.iter().any(|&(a, b)| s > a - 6.0 && s < b + 6.0)
    }

    /// Terrain height under (x, z), bilinear on the render terrain.
    fn th(&self, x: f32, z: f32) -> f32 {
        let t = &*self.terrain;
        let fx = ((x - t.origin.x) / t.cell).clamp(0.0, (t.w - 1) as f32 - 0.001);
        let fz = ((z - t.origin.y) / t.cell).clamp(0.0, (t.h - 1) as f32 - 0.001);
        let (ix, iz) = (fx.floor() as usize, fz.floor() as usize);
        let (tx, tz) = (fx - ix as f32, fz - iz as f32);
        let h = |x: usize, z: usize| t.heights[z * t.w + x];
        let a = h(ix, iz) + (h(ix + 1, iz) - h(ix, iz)) * tx;
        let b = h(ix, iz + 1) + (h(ix + 1, iz + 1) - h(ix, iz + 1)) * tx;
        a + (b - a) * tz
    }

    /// Distance from the road edge (the drivable band) out to where decoration may start on `side`.
    fn edge(&self, s: f32, side: f32) -> f32 {
        let sm = self.sm(s);
        sm.width * 0.5 + if side < 0.0 { sm.shoulder_l } else { sm.shoulder_r } + self.wall_out
    }

    /// A point `lat` metres out from the road edge on `side` at `s`, on the ground.
    fn beside(&self, s: f32, side: f32, out: f32) -> Vec3 {
        let sm = self.sm(s);
        let lat = side * (self.edge(s, side) + out);
        let p = sm.pos + sm.right * lat;
        Vec3::new(p.x, self.th(p.x, p.z), p.z)
    }

    /// Yaw that makes a +Z-facing prop look at the road from `side` at `s`.
    fn face_road(&self, s: f32, side: f32) -> f32 {
        let sm = self.sm(s);
        yaw_of(-sm.right * side)
    }

    fn put(&mut self, kind: SceneryKind, pos: Vec3, yaw: f32, scale: f32) {
        self.out.push(Scenery { kind, pos, yaw, scale });
    }

    /// Is (x, z) clear of every part of the road (a margin beyond its drivable band) and of other basins?
    fn clear(&self, x: f32, z: f32, margin: f32) -> bool {
        self.t.distance_to_centreline(x, z) > self.t.max_half_band + margin && !self.basins.iter().any(|b| (b.0 - x).hypot(b.1 - z) < b.2 + margin * 0.5)
    }

    /// Carve a bowl into the render terrain and lay a fluid surface in it. `rx` across, `rz` along (half
    /// extents of the bowl), rotated by `yaw`. Returns the surface height.
    #[allow(clippy::too_many_arguments)]
    fn basin(&mut self, kind: FluidKind, c: Vec2, rx: f32, rz: f32, yaw: f32, drop: f32, depth: f32, deep: [f32; 3], shallow: [f32; 3]) -> f32 {
        let surface = self.th(c.x, c.y) - drop;
        let cell = self.terrain.cell;
        let (sy, cy) = yaw.sin_cos();
        let (fwd, right) = (Vec2::new(sy, cy), Vec2::new(-cy, sy));
        let reach = rx.max(rz) + cell * 2.0;
        let t = &mut *self.terrain;
        let (x0, x1) = ((((c.x - reach - t.origin.x) / t.cell).floor().max(0.0)) as usize, (((c.x + reach - t.origin.x) / t.cell).ceil() as usize).min(t.w - 1));
        let (z0, z1) = ((((c.y - reach - t.origin.y) / t.cell).floor().max(0.0)) as usize, (((c.y + reach - t.origin.y) / t.cell).ceil() as usize).min(t.h - 1));
        for zi in z0..=z1 {
            for xi in x0..=x1 {
                let p = Vec2::new(t.origin.x + xi as f32 * t.cell, t.origin.y + zi as f32 * t.cell) - c;
                let (u, v) = (p.dot(right) / rx, p.dot(fwd) / rz);
                let d = (u * u + v * v).sqrt();
                let w = 1.0 - smooth(0.8, 1.0 + cell / rx.min(rz), d);
                if w > 0.0 {
                    let hh = &mut t.heights[zi * t.w + xi];
                    let target = surface - depth;
                    *hh += (hh.min(target) - *hh) * w;
                }
            }
        }
        self.fluids.push(Fluid { kind, pos: Vec3::new(c.x, surface, c.y), yaw, half: Vec2::new(rx + cell * 0.9, rz + cell * 0.9), round: true, deep, shallow });
        self.basins.push((c.x, c.y, rx.max(rz) + cell * 2.0));
        surface
    }

    /// Try to lay a pond or lava lake out beside the road at `s` on `side`; false if it would touch the road.
    #[allow(clippy::too_many_arguments)]
    fn pond(&mut self, kind: FluidKind, s: f32, side: f32, gap: f32, rx: f32, rz: f32, drop: f32, depth: f32, deep: [f32; 3], shallow: [f32; 3]) -> Option<(Vec2, f32, f32)> {
        let sm = self.sm(s);
        let yaw = yaw_of(sm.tangent) + self.rng.range(-0.3, 0.3);
        let c3 = self.beside(s, side, gap + rx);
        let c = Vec2::new(c3.x, c3.z);
        // The whole outline (and a margin) must stay clear of every road.
        let (sy, cy) = yaw.sin_cos();
        let (fwd, right) = (Vec2::new(sy, cy), Vec2::new(-cy, sy));
        for k in 0..8 {
            let a = k as f32 * std::f32::consts::TAU / 8.0;
            let p = c + right * (a.cos() * (rx + 5.0)) + fwd * (a.sin() * (rz + 5.0));
            if !self.clear(p.x, p.y, 5.0) {
                return None;
            }
        }
        if !self.clear(c.x, c.y, 5.0) {
            return None;
        }
        let surface = self.basin(kind, c, rx, rz, yaw, drop, depth, deep, shallow);
        Some((c, surface, yaw))
    }

    /// Scatter props on a ring around a basin.
    fn ring(&mut self, kind: SceneryKind, c: Vec2, yaw: f32, rx: f32, rz: f32, n: u32, jitter: f32, scale: (f32, f32)) {
        let (sy, cy) = yaw.sin_cos();
        let (fwd, right) = (Vec2::new(sy, cy), Vec2::new(-cy, sy));
        for k in 0..n {
            let a = (k as f32 + self.rng.f() * 0.8) * std::f32::consts::TAU / n as f32;
            let r = 1.0 + self.rng.range(-jitter, jitter);
            let p = c + right * (a.cos() * rx * r) + fwd * (a.sin() * rz * r);
            if !self.clear(p.x, p.y, 3.0) {
                continue;
            }
            let sc = self.rng.range(scale.0, scale.1);
            let y = self.th(p.x, p.y);
            let yw = self.rng.range(0.0, std::f32::consts::TAU);
            self.put(kind, Vec3::new(p.x, y, p.y), yw, sc);
        }
    }
}

/// A custom track is styled like the built-in race track whose look it borrows.
fn look_of(id: TrackId) -> TrackId {
    if id == TrackId::Custom {
        race_core::custom::look()
    } else {
        id
    }
}

/// Which prop forms the tyre wall at corner exits.
fn wall_prop(id: TrackId) -> &'static [SceneryKind] {
    match look_of(id) {
        TrackId::Savanna | TrackId::Baobab | TrackId::Training | TrackId::Eight | TrackId::Field | TrackId::Custom => &[SceneryKind::HayBale],
        TrackId::Dune | TrackId::DustBowl => &[SceneryKind::HayBale, SceneryKind::Barrel],
        TrackId::Canyon | TrackId::Volcano => &[SceneryKind::TyreStack],
        TrackId::Midnight | TrackId::MempoolPlaza | TrackId::Halving => &[SceneryKind::Barrel, SceneryKind::TyreStack],
        TrackId::Oasis => &[SceneryKind::Barrel, SceneryKind::HayBale],
    }
}

fn sky_for(id: TrackId) -> (SkyFx, MoteKind, [f32; 3]) {
    let d = SkyFx::default();
    match look_of(id) {
        TrackId::Savanna | TrackId::Training | TrackId::Eight | TrackId::Field | TrackId::Custom => (SkyFx { cloud_cover: 0.55, cloud_color: [1.0, 0.99, 0.95], band_strength: 0.45, band_color: [1.0, 0.86, 0.55], sun_scale: 1.25, ..d }, MoteKind::Dust, [1.0, 0.92, 0.6]),
        TrackId::Dune | TrackId::DustBowl => (
            SkyFx { cloud_cover: 0.22, cloud_color: [1.0, 0.92, 0.8], band_strength: 0.55, band_color: [1.0, 0.76, 0.46], sun_scale: 1.6, hills: HillStyle::Dunes, hill_color: Some([0.62, 0.36, 0.17]), ..d },
            MoteKind::Dust,
            [0.98, 0.78, 0.5],
        ),
        TrackId::Canyon => (
            SkyFx { cloud_cover: 0.45, cloud_color: [1.0, 0.62, 0.5], band_strength: 0.8, band_color: [1.0, 0.5, 0.38], sun_scale: 1.8, hills: HillStyle::Mesas, hill_color: Some([0.38, 0.15, 0.14]), ..d },
            MoteKind::Dust,
            [0.95, 0.62, 0.42],
        ),
        TrackId::Midnight | TrackId::MempoolPlaza => (
            SkyFx {
                cloud_cover: 0.4,
                cloud_color: [0.13, 0.15, 0.36],
                band_strength: 0.55,
                band_color: [0.45, 0.2, 0.62],
                sun_scale: 2.4,
                hills: HillStyle::Rolling,
                hill_color: Some([0.04, 0.04, 0.12]),
                hill_glow: [0.95, 0.4, 0.1],
                hill_glow_strength: 0.12,
                ..d
            },
            MoteKind::Sparks,
            [1.0, 0.6, 0.2],
        ),
        TrackId::Baobab => (
            SkyFx { cloud_cover: 0.6, cloud_color: [1.0, 0.78, 0.55], band_strength: 0.75, band_color: [0.35, 0.78, 0.7], sun_scale: 1.7, hills: HillStyle::Rolling, hill_color: Some([0.08, 0.28, 0.26]), ..d },
            MoteKind::Fireflies,
            [0.8, 1.0, 0.35],
        ),
        TrackId::Volcano => (
            SkyFx {
                cloud_cover: 0.75,
                cloud_color: [0.3, 0.13, 0.1],
                band_strength: 0.7,
                band_color: [1.0, 0.3, 0.08],
                sun_scale: 2.0,
                hills: HillStyle::Peaks,
                hill_color: Some([0.07, 0.04, 0.04]),
                hill_glow: [1.0, 0.32, 0.06],
                hill_glow_strength: 0.6,
                ..d
            },
            MoteKind::Embers,
            [1.0, 0.45, 0.1],
        ),
        TrackId::Oasis => (
            SkyFx { cloud_cover: 0.4, cloud_color: [1.0, 1.0, 1.0], band_strength: 0.35, band_color: [0.82, 0.96, 1.0], sun_scale: 1.2, hills: HillStyle::Dunes, hill_color: Some([0.86, 0.66, 0.42]), ..d },
            MoteKind::None,
            [1.0, 1.0, 1.0],
        ),
        TrackId::Halving => (
            SkyFx {
                cloud_cover: 0.28,
                cloud_color: [0.2, 0.12, 0.32],
                band_strength: 0.5,
                band_color: [0.55, 0.2, 0.75],
                sun_scale: 2.1,
                hills: HillStyle::Skyline,
                hill_color: Some([0.05, 0.03, 0.1]),
                hill_glow: [0.95, 0.45, 0.2],
                hill_glow_strength: 0.45,
                ..d
            },
            MoteKind::Sparks,
            [0.45, 0.82, 1.0],
        ),
    }
}

/// The decoration for `track`: extra scenery (appended to / filtered from `scenery`), the render effects, and
/// carved basins in `terrain`.
pub fn decorate(track: &Track, id: TrackId, terrain: &mut Terrain, scenery: &mut Vec<Scenery>) -> WorldFx {
    use race_core::WallStyle;
    let id = look_of(id);
    let wall_out = match track.theme.wall_style {
        WallStyle::Fence | WallStyle::Paddock => 1.4,
        WallStyle::DuneBerm => 6.5,
        WallStyle::RockWall => 5.0,
        WallStyle::NeonRail => 1.6,
    };
    let gaps: Vec<(f32, f32)> = track.features.iter().filter_map(|f| if let Feature::Gap { s, len } = *f { Some((s, s + len)) } else { None }).collect();
    let mut c = Ctx { t: track, terrain, rng: Rng::new(track.seed ^ 0xDEC0), out: Vec::new(), fluids: Vec::new(), basins: Vec::new(), wall_out, gaps };
    let (sky, motes, mote_color) = sky_for(id);

    // The Paddock is open ground: no start grandstands, flags or corner crowds.
    if !track.id.is_open_field() {
        start_area(&mut c, id);
        corner_decor(&mut c, id);
    }
    theme_decor(&mut c, id, scenery);

    // Props never stand in a basin.
    let basins = c.basins.clone();
    scenery.retain(|s| !basins.iter().any(|b| (b.0 - s.pos.x).hypot(b.1 - s.pos.z) < b.2));
    scenery.append(&mut c.out);

    // Ramp chevrons.
    let mut ramps = Vec::new();
    for f in &track.features {
        if let Feature::Ramp { s, len, .. } = *f {
            let sm = track.sample_at(s + len * 0.5);
            ramps.push(BoostPad { pos: track.point_at(s + len * 0.5, 0.0), yaw: yaw_of(sm.tangent), length: len, width: sm.width * 0.62 });
        }
    }
    let wall_strata = matches!(id, TrackId::Canyon);
    let wall_glow = matches!(id, TrackId::Volcano).then_some([1.0, 0.4, 0.06]);
    WorldFx { sky, motes, mote_color, fluids: c.fluids, light_trails: id == TrackId::Halving, ramps, wall_strata, wall_glow }
}

/// Grandstands with cheering fans either side of the start/finish, flags and banners.
fn start_area(c: &mut Ctx, id: TrackId) {
    let n_stands = if id == TrackId::Training { 2 } else { 3 };
    for side in [-1.0f32, 1.0] {
        for k in 0..n_stands {
            let s = -12.0 + k as f32 * 8.6;
            let sm = c.sm(s);
            let p = c.beside(s, side, 2.2);
            let yaw = c.face_road(s, side);
            let _ = sm;
            stand_with_fans(c, p, yaw);
        }
    }
    // Flags and banners along the straight; chequered flags at the gantry.
    for (i, s) in [-70.0f32, -52.0, 20.0, 36.0, 54.0, 72.0].into_iter().enumerate() {
        for side in [-1.0f32, 1.0] {
            let p = c.beside(s, side, 1.0 + (i % 2) as f32 * 0.8);
            let yaw = c.face_road(s, side);
            // The cloth lies along the pole's +X: turn it to run with the road.
            let kind = if i % 2 == 0 { SceneryKind::Banner } else { SceneryKind::Flag };
            c.put(kind, p, yaw, 1.0);
        }
    }
    for side in [-1.0f32, 1.0] {
        let sm = c.sm(0.0);
        let p = c.beside(0.0, side, 0.2);
        let _ = sm;
        let yaw = c.face_road(0.0, side);
        c.put(SceneryKind::CheckFlag, p, yaw, 1.15);
    }
}

fn stand_with_fans(c: &mut Ctx, base: Vec3, yaw: f32) {
    c.put(SceneryKind::Stands, base, yaw, 1.0);
    let (sy, cy) = yaw.sin_cos();
    let world = |x: f32, y: f32, z: f32| Vec3::new(base.x + x * cy + z * sy, base.y + y, base.z - x * sy + z * cy);
    for row in 0..3 {
        let y = 0.5 * (row + 1) as f32;
        let z = -1.4 * row as f32 - 0.7;
        let xs: &[f32] = if row % 2 == 0 { &[-3.2, -1.6, 0.0, 1.6, 3.2] } else { &[-2.4, -0.8, 0.8, 2.4] };
        for &x in xs {
            let jx = c.rng.range(-0.25, 0.25);
            let ph = c.rng.range(-0.3, 0.3);
            c.put(SceneryKind::Spectator, world(x + jx, y, z), yaw + ph, c.rng.range(0.92, 1.08));
        }
    }
}

/// A knot of fans standing along the outside of a corner.
fn crowd_cluster(c: &mut Ctx, s: f32, side: f32, n: u32) {
    for k in 0..n {
        let ss = s + (k as f32 - n as f32 * 0.5) * 2.0 + c.rng.range(-0.5, 0.5);
        let row = (k % 2) as f32;
        let p = c.beside(ss, side, 2.4 + row * 1.5 + c.rng.range(0.0, 0.5));
        let yaw = c.face_road(ss, side) + c.rng.range(-0.4, 0.4);
        c.put(SceneryKind::Spectator, p, yaw, c.rng.range(0.92, 1.1));
    }
}

fn corner_decor(c: &mut Ctx, id: TrackId) {
    let cs = corners(c.t);
    let props = wall_prop(id);
    // Sharpest first, for the crowds.
    let mut order: Vec<usize> = (0..cs.len()).collect();
    order.sort_by(|&a, &b| cs[b].peak.partial_cmp(&cs[a].peak).unwrap_or(std::cmp::Ordering::Equal));
    for (rank, &ci) in order.iter().enumerate() {
        let k = cs[ci];
        let side = -k.dir; // the outside of the turn
        if k.s0 < 30.0 && k.s1 > c.len() - 30.0 {
            continue;
        }
        // A short tyre wall on the outside of the exit, tall stacks every few metres.
        let mut s = k.s1 - 16.0;
        let mut n = 0;
        while s < k.s1 - 1.0 && n < 6 {
            if !c.in_gap(s) {
                let kind = props[(n as usize + ci) % props.len()];
                let out = if matches!(kind, SceneryKind::HayBale) { 0.9 } else { 0.8 };
                let p = c.beside(s, side, out);
                let yaw = c.face_road(s, side);
                let sc = 1.0 + 0.15 * c.rng.f();
                c.put(kind, p, yaw, sc);
                // A second row for the hay.
                if matches!(kind, SceneryKind::HayBale) {
                    let p2 = c.beside(s + 1.4, side, out + 1.1);
                    c.put(kind, p2, yaw, 1.0);
                }
            }
            s += 3.0;
            n += 1;
        }
        // Apex: a banner, and fans at the sharper bends.
        let apex = (k.s0 + k.s1) * 0.5;
        if !c.in_gap(apex) {
            let p = c.beside(apex, side, 1.6);
            let yaw = c.face_road(apex, side);
            c.put(if rank % 2 == 0 { SceneryKind::Banner } else { SceneryKind::Flag }, p, yaw, 1.0);
            if rank < 3 {
                crowd_cluster(c, apex + 6.0, side, 9);
            }
        }
    }
}

fn theme_decor(c: &mut Ctx, id: TrackId, scenery: &mut Vec<Scenery>) {
    let len = c.len();
    match look_of(id) {
        TrackId::Field => {
            // Tufts outside the fence ring only (the right-hand side of the circle), and a hay-bale
            // pen round the windmill on the centre island.
            let n = (len / 5.0) as u32;
            for _ in 0..n {
                let s = c.rng.range(0.0, len);
                let p = c.beside(s, 1.0, c.rng.range(1.0, 40.0));
                for _ in 0..3 {
                    let q = p + Vec3::new(c.rng.range(-1.2, 1.2), 0.0, c.rng.range(-1.2, 1.2));
                    let y = c.th(q.x, q.z);
                    c.put(SceneryKind::GrassTuft, Vec3::new(q.x, y, q.z), c.rng.range(0.0, 6.28), c.rng.range(1.0, 1.9));
                }
            }
            let y = c.th(0.0, 0.0);
            for k in 0..7 {
                let a = k as f32 / 7.0 * std::f32::consts::TAU + 0.3;
                c.put(SceneryKind::HayBale, Vec3::new(a.cos() * 10.0, y, a.sin() * 10.0), a + 1.2, 1.0);
            }
            // A two-high stack beside the mill.
            c.put(SceneryKind::HayBale, Vec3::new(-5.0, y, 4.0), 0.5, 1.0);
            c.put(SceneryKind::HayBale, Vec3::new(-5.0, y + 1.1, 4.0), 0.9, 0.9);
        }
        TrackId::Savanna | TrackId::Training | TrackId::Eight | TrackId::Custom => {
            // Tall golden grass clumps along both sides and a few rocks.
            let n = (len / 7.0) as u32;
            for _ in 0..n {
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let p = c.beside(s, side, c.rng.range(1.0, 26.0));
                for j in 0..3 {
                    let q = p + Vec3::new(c.rng.range(-1.2, 1.2), 0.0, c.rng.range(-1.2, 1.2));
                    let y = c.th(q.x, q.z);
                    c.put(SceneryKind::GrassTuft, Vec3::new(q.x, y, q.z), c.rng.range(0.0, 6.28), c.rng.range(1.0, 1.9) + j as f32 * 0.0);
                }
            }
        }
        TrackId::Dune | TrackId::DustBowl => {
            // Sun-bleached bones and rocks scattered on the sand.
            for _ in 0..22 {
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let p = c.beside(s, side, c.rng.range(9.0, 60.0));
                if !c.clear(p.x, p.z, 4.0) {
                    continue;
                }
                let yaw = c.rng.range(0.0, 6.28);
                let sc = c.rng.range(0.9, 1.7);
                c.put(SceneryKind::Bones, p, yaw, sc);
            }
        }
        TrackId::Canyon => {
            // Stacked hoodoo pillars standing behind the walls.
            for _ in 0..46 {
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let p = c.beside(s, side, c.rng.range(6.0, 60.0));
                if !c.clear(p.x, p.z, 5.0) {
                    continue;
                }
                let yaw = c.rng.range(0.0, 6.28);
                let sc = c.rng.range(0.8, 1.9);
                c.put(SceneryKind::Hoodoo, p, yaw, sc);
            }
        }
        TrackId::Midnight | TrackId::MempoolPlaza => {
            // Big glowing bitcoin signs on the straights, and shallow mirror pools on the salt.
            let st = straights(c.t, 90);
            for (i, (s0, s1)) in st.iter().enumerate().take(4) {
                let s = (s0 + s1) * 0.5;
                let side = if i % 2 == 0 { 1.0 } else { -1.0 };
                let p = c.beside(s, side, 22.0);
                let yaw = c.face_road(s, side);
                c.put(SceneryKind::BtcSign, p, yaw, 1.15);
            }
            for _ in 0..40 {
                if c.fluids.len() >= 9 {
                    break;
                }
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let (rx, rz) = (c.rng.range(10.0, 20.0), c.rng.range(22.0, 45.0));
                let gap = c.rng.range(14.0, 50.0);
                c.pond(FluidKind::Water, s, side, gap, rx, rz, 0.05, 0.3, [0.06, 0.08, 0.2], [0.22, 0.28, 0.55]);
            }
        }
        TrackId::Baobab => {
            // Ponds with lily pads and reeds; some acacias become baobabs.
            let mut made = 0;
            for _ in 0..60 {
                if made >= 11 {
                    break;
                }
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let (rx, rz) = (c.rng.range(8.0, 17.0), c.rng.range(14.0, 34.0));
                let gap = c.rng.range(5.0, 22.0);
                if let Some((p, surface, yaw)) = c.pond(FluidKind::Water, s, side, gap, rx, rz, 0.25, 0.9, [0.02, 0.3, 0.38], [0.1, 0.8, 0.72]) {
                    made += 1;
                    c.ring(SceneryKind::Reed, p, yaw, rx * 1.05, rz * 1.05, 14, 0.18, (1.0, 1.7));
                    for _ in 0..4 {
                        let a = c.rng.range(0.0, 6.28);
                        let r = c.rng.range(0.1, 0.6);
                        let (sy, cy) = yaw.sin_cos();
                        let q = p + Vec2::new(-cy, sy) * (a.cos() * rx * r) + Vec2::new(sy, cy) * (a.sin() * rz * r);
                        c.put(SceneryKind::LilyPad, Vec3::new(q.x, surface, q.y), c.rng.range(0.0, 6.28), c.rng.range(1.0, 1.8));
                    }
                }
            }
            let mut k = 0u32;
            for s in scenery.iter_mut() {
                if s.kind == SceneryKind::Acacia {
                    k += 1;
                    if k % 3 == 0 {
                        s.kind = SceneryKind::Baobab;
                        s.scale *= 1.0 + 0.3 * ((k % 5) as f32 / 5.0);
                    }
                }
            }
        }
        TrackId::Volcano => {
            // Lava in every gap, lakes in the low ground, glowing rocks and smoke columns.
            let gaps = c.gaps.clone();
            for (a, b) in gaps {
                let mid = (a + b) * 0.5;
                let sm = c.sm(mid);
                let half = Vec2::new(sm.width * 0.5 + sm.shoulder_l.max(sm.shoulder_r) + c.terrain.cell * 0.8, (b - a) * 0.5 + c.terrain.cell * 0.8);
                c.fluids.push(Fluid { kind: FluidKind::Lava, pos: Vec3::new(sm.pos.x, sm.pos.y - 8.0, sm.pos.z), yaw: yaw_of(sm.tangent), half, round: false, deep: [0.14, 0.05, 0.04], shallow: [1.0, 0.42, 0.06] });
            }
            let mut made = 0;
            for _ in 0..80 {
                if made >= 8 {
                    break;
                }
                let s = c.rng.range(0.0, len);
                if c.in_gap(s) {
                    continue;
                }
                let side = c.rng.side();
                let (rx, rz) = (c.rng.range(9.0, 18.0), c.rng.range(14.0, 34.0));
                let gap = c.rng.range(8.0, 28.0);
                if let Some((p, _, yaw)) = c.pond(FluidKind::Lava, s, side, gap, rx, rz, 0.4, 1.4, [0.14, 0.05, 0.04], [1.0, 0.42, 0.06]) {
                    made += 1;
                    c.ring(SceneryKind::LavaRock, p, yaw, rx * 1.15, rz * 1.15, 7, 0.2, (0.8, 1.6));
                }
            }
            for _ in 0..40 {
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let p = c.beside(s, side, c.rng.range(5.0, 50.0));
                if c.clear(p.x, p.z, 4.0) {
                    let sc = c.rng.range(0.9, 2.0);
                    c.put(SceneryKind::LavaRock, p, c.rng.range(0.0, 6.28), sc);
                }
            }
            let mut smokes = 0;
            for _ in 0..40 {
                if smokes >= 6 {
                    break;
                }
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let p = c.beside(s, side, c.rng.range(60.0, 170.0));
                if c.clear(p.x, p.z, 40.0) {
                    smokes += 1;
                    let sc = c.rng.range(0.9, 1.5);
                    c.put(SceneryKind::Smoke, p, c.rng.range(0.0, 6.28), sc);
                }
            }
        }
        TrackId::Oasis => {
            // Pools ringed with palms, market tents by the start, palms along the road.
            let mut made = 0;
            for _ in 0..60 {
                if made >= 6 {
                    break;
                }
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let (rx, rz) = (c.rng.range(9.0, 16.0), c.rng.range(12.0, 24.0));
                let gap = c.rng.range(6.0, 24.0);
                if let Some((p, surface, yaw)) = c.pond(FluidKind::Water, s, side, gap, rx, rz, 0.2, 0.8, [0.02, 0.35, 0.42], [0.35, 0.85, 0.8]) {
                    made += 1;
                    c.ring(SceneryKind::Palm, p, yaw, rx * 1.2, rz * 1.2, 8, 0.2, (0.9, 1.35));
                    c.ring(SceneryKind::Reed, p, yaw, rx * 1.02, rz * 1.02, 8, 0.1, (0.9, 1.4));
                    let (sy, cy) = yaw.sin_cos();
                    let q = p + Vec2::new(-cy, sy) * (rx * 0.3) + Vec2::new(sy, cy) * (rz * 0.2);
                    c.put(SceneryKind::LilyPad, Vec3::new(q.x, surface, q.y), 0.5, 1.5);
                }
            }
            for k in 0..5 {
                let s = -50.0 + k as f32 * 22.0;
                let side = if k % 2 == 0 { 1.0 } else { -1.0 };
                let p = c.beside(s, side, 14.0 + (k % 3) as f32 * 6.0);
                if c.clear(p.x, p.z, 4.0) {
                    let yaw = c.face_road(s, side);
                    c.put(SceneryKind::Tent, p, yaw, 1.1);
                }
            }
            for _ in 0..34 {
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let p = c.beside(s, side, c.rng.range(3.0, 30.0));
                if c.clear(p.x, p.z, 3.0) {
                    let sc = c.rng.range(0.9, 1.5);
                    c.put(SceneryKind::Palm, p, c.rng.range(0.0, 6.28), sc);
                }
            }
        }
        TrackId::Halving => {
            // A skyline of towers beyond the rails, billboards counting down to the halving.
            let mut towers = 0;
            for _ in 0..400 {
                if towers >= 42 {
                    break;
                }
                let s = c.rng.range(0.0, len);
                let side = c.rng.side();
                let p = c.beside(s, side, c.rng.range(70.0, 260.0));
                if c.clear(p.x, p.z, 55.0) {
                    towers += 1;
                    let sc = c.rng.range(0.7, 1.7);
                    c.put(SceneryKind::Tower, p, c.rng.range(0.0, 6.28), sc);
                }
            }
            let st = straights(c.t, 120);
            for (i, (s0, s1)) in st.iter().enumerate().take(5) {
                let s = (s0 + s1) * 0.5;
                let side = if i % 2 == 0 { 1.0 } else { -1.0 };
                let p = c.beside(s, side, 20.0);
                let yaw = c.face_road(s, side);
                c.put(SceneryKind::Billboard, p, yaw, 1.0);
            }
        }
    }
}


