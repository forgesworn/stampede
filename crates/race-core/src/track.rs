//! Tracks (spec 7): a closed Catmull-Rom centreline sampled every ~1 m, with
//! road + shoulders + walls, features, a racing line, terrain and scenery.

use std::sync::{Arc, Mutex, OnceLock};

use glam::Vec3;
use rand::{Rng, SeedableRng};
use rand_pcg::Pcg32;
use serde::{Deserialize, Serialize};

use crate::math::{flat, lerp, mix64, smoothstep, wrap_angle, yaw_of};

/// Shoulders rise this much per metre away from the road edge.
pub const SHOULDER_RISE: f32 = 0.06;
/// Lateral width of a boost pad.
pub const PAD_WIDTH: f32 = 4.5;
/// Height of a Power Egg target above the road.
pub const TARGET_HEIGHT: f32 = 0.55;
/// Height of a coin above the road.
pub const COIN_HEIGHT: f32 = 1.0;
/// Height of a log hurdle.
pub const HURDLE_HEIGHT: f32 = 0.6;
/// Terrain extends this far beyond the track bounds.
pub const TERRAIN_MARGIN: f32 = 160.0;
/// The terrain also ignores the higher branch this far beyond each end of a
/// bridge span, so the ramp next to the deck does not lift the ground under it.
pub const BRIDGE_TERRAIN_PAD: f32 = 10.0;
/// How far the renderer's deck slab runs beyond a bridge span (m); see [`Track::deck_surface_above`].
pub const DECK_RUN_OUT: f32 = 12.0;
/// Width the deck's parapet adds beyond the drivable edge (m).
const DECK_PARAPET: f32 = 0.4;

/// Which track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TrackId {
    /// 0: Savanna Sprint.
    Savanna,
    /// 1: Dune Drift.
    Dune,
    /// 2: Red Rock Canyon.
    Canyon,
    /// 3: Mempool Midnight.
    Midnight,
    /// T: Training Ground.
    Training,
    /// 4: Baobab Bayou (Cup 2).
    Baobab,
    /// 5: Volcano Vault (Cup 2).
    Volcano,
    /// 6: Oasis Run (Cup 2).
    Oasis,
    /// 7: Halving Highway (Cup 2).
    Halving,
    /// Battle arena: Dust Bowl (day desert, a wide loop round a mesa island).
    DustBowl,
    /// Battle arena: Mempool Plaza (night neon, a square loop round a pillar island).
    MempoolPlaza,
    /// Training venue: the Figure Eight (two loops joined by two crossing
    /// diagonals; one diagonal rises on an overpass). Appended last so saved
    /// indices do not shift.
    Eight,
    /// The Paddock: open flat ground for drills and the track recorder
    /// (a circle centreline with a road band 270 m wide; no road visible).
    Field,
    /// A player-made track (spec 19): resolves through the process-wide
    /// registry ([`crate::custom::install`]). Not in [`TrackId::ALL`]; its
    /// [`TrackId::index`] is `ALL.len()`. With nothing installed it falls back
    /// to the Training Ground.
    Custom,
}

impl TrackId {
    /// The four original race tracks ("Cup 1") in Grand Prix order.
    pub const RACE: [TrackId; 4] = [
        TrackId::Savanna,
        TrackId::Dune,
        TrackId::Canyon,
        TrackId::Midnight,
    ];
    /// The four "Cup 2" race tracks in Grand Prix order.
    pub const CUP2: [TrackId; 4] = [
        TrackId::Baobab,
        TrackId::Volcano,
        TrackId::Oasis,
        TrackId::Halving,
    ];
    /// All eight race tracks: Cup 1 then Cup 2.
    pub const RACE_ALL: [TrackId; 8] = [
        TrackId::Savanna,
        TrackId::Dune,
        TrackId::Canyon,
        TrackId::Midnight,
        TrackId::Baobab,
        TrackId::Volcano,
        TrackId::Oasis,
        TrackId::Halving,
    ];
    /// The two Battle ("Feather Frenzy") arenas. Not race tracks: they are
    /// in [`TrackId::ALL`] but not in `RACE`, `CUP2` or `RACE_ALL`.
    pub const ARENAS: [TrackId; 2] = [TrackId::DustBowl, TrackId::MempoolPlaza];
    /// Every track (race tracks, the training venues and the battle arenas),
    /// in discriminant order (index = [`TrackId::index`]).
    pub const ALL: [TrackId; 13] = [
        TrackId::Savanna,
        TrackId::Dune,
        TrackId::Canyon,
        TrackId::Midnight,
        TrackId::Training,
        TrackId::Baobab,
        TrackId::Volcano,
        TrackId::Oasis,
        TrackId::Halving,
        TrackId::DustBowl,
        TrackId::MempoolPlaza,
        TrackId::Eight,
        TrackId::Field,
    ];

    /// Whether this is open ground rather than a road ([`TrackId::Field`]):
    /// the whole band is drivable and bots cannot meaningfully race it.
    pub fn is_open_field(self) -> bool {
        self == TrackId::Field
    }

    /// Whether this is a battle arena ([`TrackId::ARENAS`]).
    pub fn is_arena(self) -> bool {
        TrackId::ARENAS.contains(&self)
    }

    /// Index into [`TrackId::ALL`] / [`crate::track_defs`]. [`TrackId::Custom`]
    /// is `ALL.len()` (one past the end).
    pub fn index(self) -> usize {
        self as usize
    }

    /// Inverse of [`TrackId::index`] (`ALL.len()` is [`TrackId::Custom`]).
    pub fn from_index(i: usize) -> Option<TrackId> {
        if i == TrackId::ALL.len() {
            return Some(TrackId::Custom);
        }
        TrackId::ALL.get(i).copied()
    }

    /// The track's authored definition. [`TrackId::Custom`] is the installed
    /// custom definition, or the Training Ground's if none is installed.
    pub fn def(self) -> TrackDef {
        if self == TrackId::Custom {
            return crate::custom::installed_def().unwrap_or_else(|| crate::tracks::def(TrackId::Training));
        }
        crate::tracks::def(self)
    }

    /// A shared, built track (built once per process and cached).
    /// [`TrackId::Custom`] is the installed custom track (rebuilt on every
    /// [`crate::custom::install`]), or the Training Ground if none is installed.
    pub fn track(self) -> Arc<Track> {
        if self == TrackId::Custom {
            return match crate::custom::current() {
                Some((_, t)) => t,
                None => TrackId::Training.track(),
            };
        }
        static CACHE: OnceLock<Mutex<Vec<Option<Arc<Track>>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(vec![None; TrackId::ALL.len()]));
        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = &c[self.index()] {
            return t.clone();
        }
        let t = Arc::new(Track::build(&self.def()));
        c[self.index()] = Some(t.clone());
        t
    }
}

/// A centreline control point.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackPoint {
    /// Centre of the road surface.
    pub pos: Vec3,
    /// Road width (full, kerb to kerb).
    pub width: f32,
    /// Bank angle (rad); positive lowers the right edge.
    pub bank: f32,
}

/// Track features, positioned by arc length `s` (metres from the start line).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Feature {
    /// A kicker ramp across the road: rises over `len` to `height`, then falls
    /// back over `lip` metres (0 = a sheer lip).
    Ramp {
        /// Start.
        s: f32,
        /// Length of the rising part.
        len: f32,
        /// Height at the lip.
        height: f32,
        /// Length of the back slope.
        lip: f32,
    },
    /// A boost pad (`PAD_WIDTH` wide) centred at `lateral`.
    BoostPad {
        /// Start.
        s: f32,
        /// Lateral centre (+ = right).
        lateral: f32,
        /// Length.
        len: f32,
    },
    /// A row of Power Egg targets across the road.
    TargetRow {
        /// Position.
        s: f32,
        /// 3..=5.
        count: u8,
    },
    /// A line of coins along the road.
    CoinLine {
        /// First coin.
        s: f32,
        /// Lateral offset (+ = right).
        lateral: f32,
        /// Coins in the line.
        count: u8,
        /// Spacing along the road.
        spacing: f32,
    },
    /// A gap in the road (canyon jump); falling in is a respawn.
    Gap {
        /// Start.
        s: f32,
        /// Length.
        len: f32,
    },
    /// A log hurdle across the road (training).
    Hurdle {
        /// Position.
        s: f32,
    },
    /// A gate arch (training).
    Gate {
        /// Position.
        s: f32,
    },
}

/// How the walls look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WallStyle {
    /// Wooden fence posts (savanna).
    Fence,
    /// Sand berms (dunes).
    DuneBerm,
    /// Rock walls (canyon).
    RockWall,
    /// Neon rails (midnight).
    NeonRail,
    /// Paddock rails (training).
    Paddock,
}

/// Scenery kinds placed outside the walls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SceneryKind {
    /// Acacia tree.
    Acacia,
    /// Cactus.
    Cactus,
    /// Boulder.
    Rock,
    /// Sand dune.
    Dune,
    /// Mesa.
    Mesa,
    /// Neon bitcoin-orange pylon.
    NeonPylon,
    /// Grass tuft.
    GrassTuft,
    /// Hay bale.
    HayBale,
}

/// One placed scenery instance.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneryItem {
    /// Kind.
    pub kind: SceneryKind,
    /// Base position (on the terrain).
    pub pos: Vec3,
    /// Yaw.
    pub yaw: f32,
    /// Uniform scale (1 = nominal size).
    pub scale: f32,
}

/// A track's look: palette (sRGB 0..1), walls, scenery, terrain shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    /// Sky at the zenith.
    pub sky_top: [f32; 3],
    /// Sky at the horizon.
    pub sky_horizon: [f32; 3],
    /// Fog colour.
    pub fog: [f32; 3],
    /// Exponential fog density (1/m).
    pub fog_density: f32,
    /// Direction toward the sun (normalised).
    pub sun_dir: [f32; 3],
    /// Sun colour x intensity.
    pub sun_colour: [f32; 3],
    /// Hemisphere ambient from above.
    pub ambient_sky: [f32; 3],
    /// Hemisphere ambient from below.
    pub ambient_ground: [f32; 3],
    /// Terrain.
    pub ground: [f32; 3],
    /// Terrain variation colour.
    pub ground_alt: [f32; 3],
    /// Road.
    pub road: [f32; 3],
    /// Road centre stripe.
    pub road_stripe: [f32; 3],
    /// Shoulders (off road).
    pub shoulder: [f32; 3],
    /// Walls.
    pub wall: [f32; 3],
    /// Curb colour A.
    pub curb_a: [f32; 3],
    /// Curb colour B.
    pub curb_b: [f32; 3],
    /// Night track (stars, glows).
    pub night: bool,
    /// Wall look.
    pub wall_style: WallStyle,
    /// Scenery kinds with density (instances per 100 m of track, both sides).
    pub scenery: Vec<(SceneryKind, f32)>,
    /// Terrain noise amplitude (m).
    pub terrain_amp: f32,
    /// Terrain noise feature size (m).
    pub terrain_scale: f32,
    /// Terrain rises this much away from the track (m).
    pub terrain_rise: f32,
    /// Distance over which the terrain blends from the track height to noise.
    pub terrain_blend: f32,
}

/// An authored track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackDef {
    /// Id.
    pub id: TrackId,
    /// Display name.
    pub name: String,
    /// Look.
    pub theme: Theme,
    /// Default lap count.
    pub laps: u8,
    /// Closed centreline control points (Catmull-Rom).
    pub points: Vec<TrackPoint>,
    /// Shoulder width left / right.
    pub shoulder: [f32; 2],
    /// Features.
    pub features: Vec<Feature>,
    /// Seed for terrain and scenery.
    pub seed: u64,
}

/// Surface of the road band at a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Surface {
    /// Plain road.
    Road,
    /// On a ramp.
    Ramp,
    /// Over a gap (no ground).
    Gap,
}

/// One centreline sample (every `Track::ds` ~ 1 m).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackSample {
    /// Arc length from the start line.
    pub s: f32,
    /// Road centre (base height, ramps excluded).
    pub pos: Vec3,
    /// Unit tangent (3D, follows the slope).
    pub tangent: Vec3,
    /// Unit right (horizontal).
    pub right: Vec3,
    /// Road normal (tilted by bank).
    pub up: Vec3,
    /// Road width.
    pub width: f32,
    /// Left shoulder width.
    pub shoulder_l: f32,
    /// Right shoulder width.
    pub shoulder_r: f32,
    /// Bank (rad, + lowers the right edge).
    pub bank: f32,
    /// Signed horizontal curvature (1/m, + = turning left).
    pub curvature: f32,
    /// Ramp height above the base road here.
    pub ramp: f32,
    /// Surface.
    pub surface: Surface,
}

/// A point on the racing line (same indexing as `Track::samples`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinePoint {
    /// Position (base road height).
    pub pos: Vec3,
    /// Lateral offset from the centreline.
    pub offset: f32,
    /// Signed curvature of the line (1/m, + = left).
    pub curvature: f32,
}

/// Result of [`Track::project`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    /// Arc length (0..length).
    pub s: f32,
    /// Lateral offset (+ = right of the centreline).
    pub lateral: f32,
    /// Sample index at or before the point (use as the next hint).
    pub idx: usize,
    /// Fraction between `idx` and `idx + 1`.
    pub u: f32,
    /// Ground height under the point.
    pub ground_y: f32,
}

/// An overpass: the section of the higher branch of a [`Crossing`] (spec 18.1).
///
/// The span covers the higher branch within `w_lower / 2 + 6` m (along the
/// road) of the crossing point. `s0` is wrapped into `0..length`; `s1 = s0 +
/// span`, so it may exceed the lap length when the span straddles the start
/// line. Use [`Bridge::contains`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bridge {
    /// Arc length where the span starts (`0..length`).
    pub s0: f32,
    /// Arc length where the span ends (`s0 + span`; may exceed the lap length).
    pub s1: f32,
}

impl Bridge {
    /// Whether the (wrapped) arc length `s` lies on the span, on a lap of `length`.
    pub fn contains(&self, s: f32, length: f32) -> bool {
        let d = (s - self.s0).rem_euclid(length);
        d <= self.s1 - self.s0
    }

    /// [`Bridge::contains`] with the span widened by `pad` metres at both ends.
    pub fn contains_padded(&self, s: f32, length: f32, pad: f32) -> bool {
        let d = (s - (self.s0 - pad)).rem_euclid(length);
        d <= self.s1 - self.s0 + 2.0 * pad
    }
}

/// Where two parts of the lap pass over each other (found by [`Track::build`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crossing {
    /// Arc length of the higher branch at the crossing (the bridge's centre).
    pub s_high: f32,
    /// Arc length of the lower branch at the crossing.
    pub s_low: f32,
    /// Centreline height of the higher branch at the crossing.
    pub y_high: f32,
    /// Centreline height of the lower branch at the crossing.
    pub y_low: f32,
    /// Road width of the lower branch at the crossing.
    pub w_low: f32,
}

impl Crossing {
    /// Height difference between the branches (m, >= 0).
    pub fn gap(&self) -> f32 {
        self.y_high - self.y_low
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct TerrainGrid {
    x0: f32,
    z0: f32,
    cell: f32,
    nx: usize,
    nz: usize,
    near_y: Vec<f32>,
    dist: Vec<f32>,
}

/// A built track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Id.
    pub id: TrackId,
    /// Display name.
    pub name: String,
    /// Look.
    pub theme: Theme,
    /// Default laps.
    pub laps: u8,
    /// Centreline samples.
    pub samples: Vec<TrackSample>,
    /// Lap length (m).
    pub length: f32,
    /// Sample spacing (= length / samples.len(), ~1 m).
    pub ds: f32,
    /// Bot racing line, one point per sample.
    pub racing_line: Vec<LinePoint>,
    /// Features.
    pub features: Vec<Feature>,
    /// Falling below this is a respawn.
    pub kill_y: f32,
    /// Ground height inside a gap.
    pub gap_floor: f32,
    /// The widest half band (half road + shoulder) anywhere.
    pub max_half_band: f32,
    /// Terrain / scenery seed.
    pub seed: u64,
    /// Grade-separated crossings found at build time (spec 18.1): pairs of
    /// samples more than 40 m apart along the lap (and more than 1.6 mean road
    /// widths, so a very wide band is not mistaken for a crossing) whose
    /// horizontal distance is below `(w_a + w_b) / 2 + 1` m.
    #[serde(default)]
    pub crossings: Vec<Crossing>,
    /// One overpass span per crossing: the higher branch within
    /// `w_lower / 2 + 6` m of the crossing. Terrain and
    /// [`Track::distance_to_centreline`] ignore samples on a bridge, and for
    /// [`BRIDGE_TERRAIN_PAD`] m beyond it (the terrain follows the lower road);
    /// the renderer draws a deck, side faces
    /// and pillars here. Empty on tracks with no crossing.
    #[serde(default)]
    pub bridges: Vec<Bridge>,
    grid: TerrainGrid,
}

fn catmull(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3, t: f32) -> Vec3 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

fn hash01(seed: u64, x: i64, z: i64) -> f32 {
    let h = mix64(
        seed ^ (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (z as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F),
    );
    (h >> 40) as f32 / (1u64 << 24) as f32
}

fn value_noise(seed: u64, x: f32, z: f32) -> f32 {
    let (fx, fz) = (x.floor(), z.floor());
    let (ix, iz) = (fx as i64, fz as i64);
    let (tx, tz) = (x - fx, z - fz);
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sz = tz * tz * (3.0 - 2.0 * tz);
    let a = hash01(seed, ix, iz);
    let b = hash01(seed, ix + 1, iz);
    let c = hash01(seed, ix, iz + 1);
    let d = hash01(seed, ix + 1, iz + 1);
    lerp(lerp(a, b, sx), lerp(c, d, sx), sz) * 2.0 - 1.0
}

/// Finds crossings (see [`Track::crossings`]) and the bridge span of each.
fn detect_crossings(samples: &[TrackSample], ds: f32, length: f32) -> (Vec<Crossing>, Vec<Bridge>) {
    let n = samples.len();
    // For each sample, its nearest crossing partner (index, horizontal distance).
    let mut partner: Vec<Option<(usize, f32)>> = vec![None; n];
    for i in 0..n {
        let a = &samples[i];
        for j in 0..n {
            let b = &samples[j];
            let gap = i.abs_diff(j);
            let sep = gap.min(n - gap) as f32 * ds;
            let mean_w = 0.5 * (a.width + b.width);
            if sep <= 40.0f32.max(1.6 * mean_w) {
                continue;
            }
            let d = flat(a.pos - b.pos).length();
            if d < mean_w + 1.0 && partner[i].map_or(true, |(_, best)| d < best) {
                partner[i] = Some((j, d));
            }
        }
    }
    // Contiguous (cyclic) runs of samples with a partner are the branches.
    let mut run = vec![usize::MAX; n];
    let mut runs = 0usize;
    let Some(start) = (0..n).find(|&i| partner[i].is_none()) else {
        return (vec![], vec![]);
    };
    for k in 1..=n {
        let i = (start + k) % n;
        if partner[i].is_some() {
            let prev = (i + n - 1) % n;
            if partner[prev].is_some() {
                run[i] = run[prev];
            } else {
                run[i] = runs;
                runs += 1;
            }
        }
    }
    // The closest pair per pair of runs is the crossing point.
    let mut best: Vec<((usize, usize), (usize, usize, f32))> = Vec::new();
    for i in 0..n {
        let Some((j, d)) = partner[i] else { continue };
        let key = (run[i].min(run[j]), run[i].max(run[j]));
        match best.iter_mut().find(|(k, _)| *k == key) {
            Some((_, b)) => {
                if d < b.2 {
                    *b = (i, j, d);
                }
            }
            None => best.push((key, (i, j, d))),
        }
    }
    let mut crossings = Vec::new();
    let mut bridges = Vec::new();
    for (_, (i, j, _)) in best {
        let (hi, lo) = if samples[i].pos.y >= samples[j].pos.y { (i, j) } else { (j, i) };
        let (h, l) = (&samples[hi], &samples[lo]);
        let reach = 0.5 * l.width + 6.0;
        let s0 = (h.s - reach).rem_euclid(length);
        crossings.push(Crossing {
            s_high: h.s,
            s_low: l.s,
            y_high: h.pos.y,
            y_low: l.pos.y,
            w_low: l.width,
        });
        bridges.push(Bridge { s0, s1: s0 + 2.0 * reach });
    }
    (crossings, bridges)
}

/// Half the width of the band of [`Track::open_arena`] (m): the Paddock's circle (r = 150 m) reaches
/// the middle and 10 m past its old rim, so the open ground is a disc of radius 310 m.
pub const ARENA_HALF_BAND: f32 = 160.0;

impl Track {
    /// The Paddock for a solo run (Run a Track): the same circle, but the drivable band takes in
    /// the middle island too, so the runner can start at the centre and nothing holds them in
    /// but the outer rim. The Paddock itself is unchanged. Built once.
    pub fn open_arena() -> Arc<Track> {
        static ARENA: OnceLock<Arc<Track>> = OnceLock::new();
        ARENA
            .get_or_init(|| {
                let mut def = TrackId::Field.def();
                for p in def.points.iter_mut() {
                    p.width = 2.0 * ARENA_HALF_BAND;
                }
                Arc::new(Track::build(&def))
            })
            .clone()
    }

    /// Builds a track from its definition: samples, features, racing line, terrain.
    pub fn build(def: &TrackDef) -> Track {
        let m = def.points.len();
        assert!(m >= 4, "a track needs at least 4 points");
        // 1. Dense polyline through the closed Catmull-Rom spline.
        const SUB: usize = 12;
        let mut dense: Vec<(Vec3, f32, f32)> = Vec::with_capacity(m * SUB + 1);
        for k in 0..m {
            let p0 = def.points[(k + m - 1) % m];
            let p1 = def.points[k];
            let p2 = def.points[(k + 1) % m];
            let p3 = def.points[(k + 2) % m];
            for j in 0..SUB {
                let t = j as f32 / SUB as f32;
                let p = catmull(p0.pos, p1.pos, p2.pos, p3.pos, t);
                dense.push((p, lerp(p1.width, p2.width, t), lerp(p1.bank, p2.bank, t)));
            }
        }
        dense.push(dense[0]);
        let mut cum = vec![0.0f32; dense.len()];
        for i in 1..dense.len() {
            cum[i] = cum[i - 1] + flat(dense[i].0 - dense[i - 1].0).length();
        }
        let total = *cum.last().unwrap();
        let n = total.round().max(16.0) as usize;
        let ds = total / n as f32;

        // 2. Resample at ds.
        let mut pos = Vec::with_capacity(n);
        let mut widths = Vec::with_capacity(n);
        let mut banks = Vec::with_capacity(n);
        let mut j = 0usize;
        for i in 0..n {
            let s = i as f32 * ds;
            while j + 1 < dense.len() - 1 && cum[j + 1] < s {
                j += 1;
            }
            let seg = (cum[j + 1] - cum[j]).max(1e-6);
            let t = ((s - cum[j]) / seg).clamp(0.0, 1.0);
            pos.push(dense[j].0.lerp(dense[j + 1].0, t));
            widths.push(lerp(dense[j].1, dense[j + 1].1, t));
            banks.push(lerp(dense[j].2, dense[j + 1].2, t));
        }

        // 3. Frames and curvature.
        let mut samples = Vec::with_capacity(n);
        let yaws: Vec<f32> = (0..n)
            .map(|i| yaw_of(flat(pos[(i + 1) % n] - pos[(i + n - 1) % n])))
            .collect();
        for i in 0..n {
            let tangent = (pos[(i + 1) % n] - pos[(i + n - 1) % n]).normalize_or_zero();
            let th = flat(tangent).normalize_or_zero();
            let right = Vec3::new(-th.z, 0.0, th.x);
            let k = wrap_angle(yaws[(i + 2) % n] - yaws[(i + n - 2) % n]) / (4.0 * ds);
            let bank = banks[i];
            let up = (Vec3::Y * crate::math::cos(bank) + right * crate::math::sin(bank)).normalize();
            samples.push(TrackSample {
                s: i as f32 * ds,
                pos: pos[i],
                tangent,
                right,
                up,
                width: widths[i],
                shoulder_l: def.shoulder[0],
                shoulder_r: def.shoulder[1],
                bank,
                curvature: k,
                ramp: 0.0,
                surface: Surface::Road,
            });
        }
        // Smooth curvature a little.
        let raw: Vec<f32> = samples.iter().map(|s| s.curvature).collect();
        for i in 0..n {
            let mut acc = 0.0;
            for d in -3i32..=3 {
                acc += raw[(i as i32 + d).rem_euclid(n as i32) as usize];
            }
            samples[i].curvature = acc / 7.0;
        }

        let length = total;
        // 4. Features baked into samples.
        for f in &def.features {
            match *f {
                Feature::Ramp {
                    s,
                    len,
                    height,
                    lip,
                } => {
                    let steps = (len + lip) / ds;
                    for k in 0..=(steps.ceil() as usize + 1) {
                        let x = k as f32 * ds;
                        let h = if x <= len {
                            let u = x / len;
                            height * u * u
                        } else if lip > 0.0 && x <= len + lip {
                            height * (1.0 - (x - len) / lip)
                        } else {
                            0.0
                        };
                        let idx = ((s + x) / ds).round() as i64;
                        let idx = idx.rem_euclid(n as i64) as usize;
                        if h > samples[idx].ramp {
                            samples[idx].ramp = h;
                            samples[idx].surface = Surface::Ramp;
                        }
                    }
                }
                Feature::Gap { s, len } => {
                    let a = (s / ds).ceil() as i64;
                    let b = ((s + len) / ds).floor() as i64;
                    for k in a..=b {
                        let idx = k.rem_euclid(n as i64) as usize;
                        samples[idx].surface = Surface::Gap;
                        samples[idx].ramp = 0.0;
                    }
                }
                _ => {}
            }
        }
        let min_y = samples
            .iter()
            .map(|s| s.pos.y)
            .fold(f32::INFINITY, f32::min);
        let kill_y = min_y - 8.0;
        let gap_floor = min_y - 40.0;
        let max_half_band = samples
            .iter()
            .map(|s| 0.5 * s.width + s.shoulder_l.max(s.shoulder_r))
            .fold(0.0, f32::max);

        let (crossings, bridges) = detect_crossings(&samples, ds, length);
        let mut track = Track {
            id: def.id,
            name: def.name.clone(),
            theme: def.theme.clone(),
            laps: def.laps,
            samples,
            length,
            ds,
            racing_line: Vec::new(),
            features: def.features.clone(),
            kill_y,
            gap_floor,
            max_half_band,
            seed: def.seed,
            crossings,
            bridges,
            grid: TerrainGrid {
                x0: 0.0,
                z0: 0.0,
                cell: 1.0,
                nx: 0,
                nz: 0,
                near_y: vec![],
                dist: vec![],
            },
        };
        track.racing_line = track.compute_racing_line();
        track.grid = track.compute_grid();
        track
    }

    /// Number of samples.
    pub fn n(&self) -> usize {
        self.samples.len()
    }

    /// Wraps an arc length into 0..length.
    pub fn wrap_s(&self, s: f32) -> f32 {
        s.rem_euclid(self.length)
    }

    /// Signed shortest arc distance from `a` to `b` (-length/2 .. length/2].
    pub fn s_delta(&self, a: f32, b: f32) -> f32 {
        let mut d = (b - a).rem_euclid(self.length);
        if d > self.length * 0.5 {
            d -= self.length;
        }
        d
    }

    /// Sample index at arc length `s` (floor, wrapped).
    pub fn idx_at(&self, s: f32) -> usize {
        ((self.wrap_s(s) / self.ds) as usize).min(self.n() - 1)
    }

    /// Interpolated centreline sample at arc length `s` (`ramp` and `surface` from the nearer sample).
    pub fn sample_at(&self, s: f32) -> TrackSample {
        let s = self.wrap_s(s);
        let a = self.idx_at(s);
        let b = (a + 1) % self.n();
        let u = ((s - a as f32 * self.ds) / self.ds).clamp(0.0, 1.0);
        let (sa, sb) = (&self.samples[a], &self.samples[b]);
        TrackSample {
            s,
            pos: sa.pos.lerp(sb.pos, u),
            tangent: sa.tangent.lerp(sb.tangent, u).normalize_or_zero(),
            right: sa.right.lerp(sb.right, u).normalize_or_zero(),
            up: sa.up.lerp(sb.up, u).normalize_or_zero(),
            width: lerp(sa.width, sb.width, u),
            shoulder_l: lerp(sa.shoulder_l, sb.shoulder_l, u),
            shoulder_r: lerp(sa.shoulder_r, sb.shoulder_r, u),
            bank: lerp(sa.bank, sb.bank, u),
            curvature: lerp(sa.curvature, sb.curvature, u),
            ramp: lerp(sa.ramp, sb.ramp, u),
            surface: if u < 0.5 { sa.surface } else { sb.surface },
        }
    }

    /// A point on the ground at arc length `s` and lateral offset.
    pub fn point_at(&self, s: f32, lateral: f32) -> Vec3 {
        let sm = self.sample_at(s);
        let mut p = sm.pos + sm.right * lateral;
        let a = self.idx_at(s);
        let pr = self.project(p, a);
        p.y = pr.ground_y;
        p
    }

    /// Half the drivable band (half road + shoulder) on the side of `lateral` at `s`.
    pub fn half_band(&self, s: f32, lateral: f32) -> f32 {
        let sm = self.sample_at(s);
        0.5 * sm.width
            + if lateral < 0.0 {
                sm.shoulder_l
            } else {
                sm.shoulder_r
            }
    }

    /// Projects a world position onto the track, walking from `hint` (the
    /// caller's last sample index), so it is O(1) per tick and never jumps to a
    /// nearby other part of the track. Use [`Track::locate`] for a cold start.
    pub fn project(&self, pos: Vec3, hint: usize) -> Projection {
        let n = self.n();
        let mut i = hint % n;
        let mut last = 0i32;
        let mut t = 0.0;
        for _ in 0..n / 2 {
            let sm = &self.samples[i];
            let d = pos - sm.pos;
            let th = Vec3::new(sm.right.z, 0.0, -sm.right.x);
            t = d.x * th.x + d.z * th.z;
            if t > 0.5 * self.ds {
                if last == -1 {
                    break;
                }
                i = (i + 1) % n;
                last = 1;
            } else if t < -0.5 * self.ds {
                if last == 1 {
                    break;
                }
                i = (i + n - 1) % n;
                last = -1;
            } else {
                break;
            }
        }
        let a = if t >= 0.0 { i } else { (i + n - 1) % n };
        let b = (a + 1) % n;
        let (sa, sb) = (&self.samples[a], &self.samples[b]);
        let v = flat(sb.pos - sa.pos);
        let u = (flat(pos - sa.pos).dot(v) / v.length_squared().max(1e-6)).clamp(0.0, 1.0);
        let c = sa.pos.lerp(sb.pos, u);
        let r = sa.right.lerp(sb.right, u).normalize_or_zero();
        let lateral = flat(pos - c).dot(r);
        let s = self.wrap_s(a as f32 * self.ds + u * self.ds);
        let mut p = Projection {
            s,
            lateral,
            idx: a,
            u,
            ground_y: 0.0,
        };
        p.ground_y = self.ground_at(&p, pos);
        p
    }

    /// Nearest sample index by brute force (spawns, cold starts), by 3D
    /// distance so that at a crossing the branch at the right height wins.
    pub fn locate(&self, pos: Vec3) -> usize {
        let mut best = (f32::INFINITY, 0usize);
        for (i, s) in self.samples.iter().enumerate() {
            let d = (s.pos - pos).length_squared();
            if d < best.0 {
                best = (d, i);
            }
        }
        best.1
    }

    /// [`Track::project`] from a brute-force [`Track::locate`] (3D nearest sample, so
    /// a point on the lower road under a bridge projects onto the lower branch).
    pub fn project_global(&self, pos: Vec3) -> Projection {
        self.project(pos, self.locate(pos))
    }

    /// Ground height for a projection (`pos` is used beyond the walls for terrain).
    pub fn ground_at(&self, p: &Projection, pos: Vec3) -> f32 {
        let n = self.n();
        let (sa, sb) = (&self.samples[p.idx], &self.samples[(p.idx + 1) % n]);
        let u = p.u;
        let near = if u < 0.5 { sa } else { sb };
        let y = lerp(sa.pos.y, sb.pos.y, u);
        let hw = 0.5 * lerp(sa.width, sb.width, u);
        let sh = if p.lateral < 0.0 {
            lerp(sa.shoulder_l, sb.shoulder_l, u)
        } else {
            lerp(sa.shoulder_r, sb.shoulder_r, u)
        };
        let lat = p.lateral.abs();
        if near.surface == Surface::Gap && lat <= hw + sh + 0.01 {
            return self.gap_floor;
        }
        let tb = crate::math::tan(lerp(sa.bank, sb.bank, u));
        if lat <= hw {
            y - p.lateral * tb + lerp(sa.ramp, sb.ramp, u)
        } else if lat <= hw + sh + 0.01 {
            y - p.lateral.signum() * hw * tb + (lat - hw) * SHOULDER_RISE
        } else {
            self.terrain_height(pos.x, pos.z)
        }
    }

    /// Ground height under `pos` (walking from `hint`).
    pub fn ground_height(&self, pos: Vec3, hint: usize) -> f32 {
        self.project(pos, hint).ground_y
    }

    /// Height of the lowest bridge deck surface directly above the ground point
    /// `(x, z)` that is at least `min_y` high, or `None` with no such deck.
    ///
    /// For callers that must not clip a deck (the chase camera). A deck counts over
    /// its whole width (road, shoulders and parapet) and for [`DECK_RUN_OUT`] m
    /// beyond the span, where the renderer's slab still runs. Deck points within
    /// `own_reach` m of arc length of `own_s` are skipped: they are the caller's own
    /// road (a racer climbing onto an overpass must not see the road ahead as a
    /// ceiling). The deck's underside is a fixed depth below the surface returned.
    pub fn deck_surface_above(&self, x: f32, z: f32, min_y: f32, own_s: f32, own_reach: f32) -> Option<f32> {
        let n = self.n();
        let mut best: Option<f32> = None;
        for b in &self.bridges {
            let first = self.idx_at(b.s0 - DECK_RUN_OUT);
            let count = ((b.s1 - b.s0 + 2.0 * DECK_RUN_OUT) / self.ds).ceil() as usize + 2;
            for k in 0..count.min(n) {
                let (sa, sb) = (&self.samples[(first + k) % n], &self.samples[(first + k + 1) % n]);
                let ab = flat(sb.pos - sa.pos);
                let len2 = ab.length_squared();
                if len2 < 1e-6 {
                    continue;
                }
                let q = flat(Vec3::new(x, 0.0, z) - sa.pos);
                let t_raw = q.dot(ab) / len2;
                // A little overlap between neighbouring segments leaves no holes on the outside of a bend.
                if !(-0.15..=1.15).contains(&t_raw) {
                    continue;
                }
                let t = t_raw.clamp(0.0, 1.0);
                let lat = (q - ab * t).dot(sa.right.lerp(sb.right, t));
                let half = 0.5 * lerp(sa.width, sb.width, t)
                    + if lat < 0.0 { lerp(sa.shoulder_l, sb.shoulder_l, t) } else { lerp(sa.shoulder_r, sb.shoulder_r, t) }
                    + DECK_PARAPET;
                if lat.abs() > half {
                    continue;
                }
                if self.s_delta(own_s, sa.s + t * self.ds).abs() < own_reach {
                    continue;
                }
                let y = lerp(sa.pos.y, sb.pos.y, t) + lerp(sa.ramp, sb.ramp, t);
                if y >= min_y && best.is_none_or(|m| y < m) {
                    best = Some(y);
                }
            }
        }
        best
    }

    /// Terrain height anywhere (outside the walls): pinned to the track height
    /// near the track, value noise further out. Deterministic.
    pub fn terrain_height(&self, x: f32, z: f32) -> f32 {
        let (ny, d) = self.grid_sample(x, z);
        let th = &self.theme;
        let edge = self.max_half_band + 1.5;
        let b = smoothstep(edge, edge + th.terrain_blend, d);
        let sc = th.terrain_scale.max(1.0);
        let nz = value_noise(self.seed, x / sc, z / sc) * 0.7
            + value_noise(self.seed ^ 0x55, x / (sc * 0.37), z / (sc * 0.37)) * 0.3;
        ny + b * (th.terrain_rise + th.terrain_amp * nz)
    }

    /// Distance (approx.) from (x, z) to the nearest centreline sample
    /// (samples on a bridge span are ignored).
    pub fn distance_to_centreline(&self, x: f32, z: f32) -> f32 {
        self.grid_sample(x, z).1
    }

    fn grid_sample(&self, x: f32, z: f32) -> (f32, f32) {
        let g = &self.grid;
        if g.nx < 2 || g.nz < 2 {
            return (0.0, 1e4);
        }
        let fx = ((x - g.x0) / g.cell).clamp(0.0, (g.nx - 1) as f32 - 0.001);
        let fz = ((z - g.z0) / g.cell).clamp(0.0, (g.nz - 1) as f32 - 0.001);
        let (ix, iz) = (fx as usize, fz as usize);
        let (tx, tz) = (fx - ix as f32, fz - iz as f32);
        let at = |v: &Vec<f32>, i: usize, j: usize| v[j * g.nx + i];
        let bil = |v: &Vec<f32>| {
            lerp(
                lerp(at(v, ix, iz), at(v, ix + 1, iz), tx),
                lerp(at(v, ix, iz + 1), at(v, ix + 1, iz + 1), tx),
                tz,
            )
        };
        (bil(&g.near_y), bil(&g.dist))
    }

    fn compute_grid(&self) -> TerrainGrid {
        let (lo, hi) = self.bounds();
        let cell = 6.0;
        let x0 = lo.x - TERRAIN_MARGIN;
        let z0 = lo.z - TERRAIN_MARGIN;
        let nx = ((hi.x - lo.x + 2.0 * TERRAIN_MARGIN) / cell).ceil() as usize + 1;
        let nz = ((hi.z - lo.z + 2.0 * TERRAIN_MARGIN) / cell).ceil() as usize + 1;
        let mut near_y = vec![0.0; nx * nz];
        let mut dist = vec![0.0; nx * nz];
        let pts: Vec<(f32, f32, f32, f32)> = self
            .samples
            .iter()
            .step_by(2)
            .filter(|s| !self.bridges.iter().any(|b| b.contains_padded(s.s, self.length, BRIDGE_TERRAIN_PAD)))
            .map(|s| {
                let (y, w) = if s.surface == Surface::Gap {
                    (s.pos.y - 25.0, 30.0)
                } else {
                    (s.pos.y, 1.0)
                };
                (s.pos.x, s.pos.z, y, w)
            })
            .collect();
        for j in 0..nz {
            for i in 0..nx {
                let x = x0 + i as f32 * cell;
                let z = z0 + j as f32 * cell;
                let mut wsum = 0.0f64;
                let mut ysum = 0.0f64;
                let mut dmin = f32::INFINITY;
                for &(px, pz, py, pw) in &pts {
                    let d2 = (px - x) * (px - x) + (pz - z) * (pz - z);
                    dmin = dmin.min(d2);
                    let w = pw as f64 / ((d2 as f64 + 100.0) * (d2 as f64 + 100.0));
                    wsum += w;
                    ysum += w * py as f64;
                }
                near_y[j * nx + i] = (ysum / wsum.max(1e-30)) as f32;
                dist[j * nx + i] = dmin.sqrt();
            }
        }
        TerrainGrid {
            x0,
            z0,
            cell,
            nx,
            nz,
            near_y,
            dist,
        }
    }

    /// Min-curvature racing line: an elastic band relaxed on a coarse grid,
    /// kept inside the road by a margin.
    fn compute_racing_line(&self) -> Vec<LinePoint> {
        let n = self.n();
        let step = 4usize;
        let m = n / step;
        let idx: Vec<usize> = (0..m).map(|k| k * n / m).collect();
        let c: Vec<Vec3> = idx.iter().map(|&i| flat(self.samples[i].pos)).collect();
        let r: Vec<Vec3> = idx.iter().map(|&i| self.samples[i].right).collect();
        let lim: Vec<f32> = idx
            .iter()
            .map(|&i| (0.5 * self.samples[i].width - 2.4).max(0.0))
            .collect();
        let mut o = vec![0.0f32; m];
        for _ in 0..1500 {
            for k in 0..m {
                let a = (k + m - 1) % m;
                let b = (k + 1) % m;
                let pa = c[a] + r[a] * o[a];
                let pb = c[b] + r[b] * o[b];
                let mid = (pa + pb) * 0.5;
                let want = (mid - c[k]).dot(r[k]);
                o[k] = lerp(o[k], want, 0.7).clamp(-lim[k], lim[k]);
            }
        }
        // Upsample offsets to every sample.
        let mut off = vec![0.0f32; n];
        for k in 0..m {
            let i0 = idx[k];
            let i1 = if k + 1 < m { idx[k + 1] } else { n };
            let o1 = o[(k + 1) % m];
            for i in i0..i1 {
                let t = (i - i0) as f32 / (i1 - i0) as f32;
                off[i] = lerp(o[k], o1, t);
            }
        }
        let pts: Vec<Vec3> = (0..n)
            .map(|i| self.samples[i].pos + self.samples[i].right * off[i])
            .collect();
        let w = 4usize;
        (0..n)
            .map(|i| {
                let p0 = pts[(i + n - w) % n];
                let p1 = pts[i];
                let p2 = pts[(i + w) % n];
                let h0 = yaw_of(flat(p1 - p0));
                let h1 = yaw_of(flat(p2 - p1));
                let len = 0.5 * (flat(p1 - p0).length() + flat(p2 - p1).length());
                LinePoint {
                    pos: p1,
                    offset: off[i],
                    curvature: wrap_angle(h1 - h0) / len.max(0.1),
                }
            })
            .collect()
    }

    /// Axis-aligned bounds of the drivable band (min, max).
    pub fn bounds(&self) -> (Vec3, Vec3) {
        let mut lo = Vec3::splat(f32::INFINITY);
        let mut hi = Vec3::splat(f32::NEG_INFINITY);
        for s in &self.samples {
            for side in [-1.0f32, 1.0] {
                let hb = 0.5 * s.width
                    + if side < 0.0 {
                        s.shoulder_l
                    } else {
                        s.shoulder_r
                    };
                let p = s.pos + s.right * side * hb;
                lo = lo.min(p);
                hi = hi.max(p);
            }
        }
        (lo, hi)
    }

    /// Bounds of the terrain the renderer should draw (bounds + `TERRAIN_MARGIN`).
    pub fn terrain_bounds(&self) -> (Vec3, Vec3) {
        let (lo, hi) = self.bounds();
        let m = Vec3::new(TERRAIN_MARGIN, 0.0, TERRAIN_MARGIN);
        (lo - m, hi + m)
    }

    /// The 4 sector checkpoints (arc lengths); a lap counts only after passing 1, 2, 3 in order.
    pub fn checkpoints(&self) -> [f32; 4] {
        let l = self.length;
        [0.0, l * 0.25, l * 0.5, l * 0.75]
    }

    /// Whether arc length `s` lies within `[a, a + len]` (wrapping).
    pub fn s_in(&self, s: f32, a: f32, len: f32) -> bool {
        let d = (s - a).rem_euclid(self.length);
        d <= len
    }

    /// Deterministic scenery outside the walls (kinds and density from the theme).
    pub fn scenery(&self) -> Vec<SceneryItem> {
        let mut out = Vec::new();
        let mut rng = Pcg32::seed_from_u64(mix64(self.seed ^ 0x5CE7E));
        let l = self.length;
        for &(kind, density) in &self.theme.scenery {
            if kind == SceneryKind::NeonPylon {
                // Regular pylons along both walls.
                let spacing = (100.0 / density.max(0.1)).max(8.0) * 2.0;
                let count = (l / spacing) as usize;
                for k in 0..count {
                    let s = k as f32 * spacing;
                    for side in [-1.0f32, 1.0] {
                        let sm = self.sample_at(s);
                        let hb = 0.5 * sm.width
                            + if side < 0.0 {
                                sm.shoulder_l
                            } else {
                                sm.shoulder_r
                            };
                        let mut p = sm.pos + sm.right * side * (hb + 1.6);
                        if self.distance_to_centreline(p.x, p.z) < self.max_half_band + 1.0 {
                            continue;
                        }
                        p.y = sm.pos.y;
                        out.push(SceneryItem {
                            kind,
                            pos: p,
                            yaw: yaw_of(sm.tangent),
                            scale: 1.0,
                        });
                    }
                }
                continue;
            }
            let (dmin, dmax, smin, smax) = match kind {
                SceneryKind::Acacia => (4.0, 70.0, 0.8, 1.4),
                SceneryKind::Cactus => (3.0, 50.0, 0.7, 1.3),
                SceneryKind::Rock => (3.0, 60.0, 0.5, 1.8),
                SceneryKind::Dune => (25.0, 140.0, 0.8, 2.0),
                SceneryKind::Mesa => (45.0, 150.0, 0.7, 1.6),
                SceneryKind::GrassTuft => (0.8, 25.0, 0.6, 1.4),
                SceneryKind::HayBale => (2.0, 20.0, 0.9, 1.1),
                SceneryKind::NeonPylon => unreachable!(),
            };
            let count = (density * l / 100.0).round() as usize;
            for _ in 0..count {
                let s = rng.gen::<f32>() * l;
                let side = if rng.gen::<bool>() { 1.0 } else { -1.0 };
                let off = dmin + rng.gen::<f32>().powf(1.5) * (dmax - dmin);
                let yaw = rng.gen::<f32>() * std::f32::consts::TAU;
                let scale = lerp(smin, smax, rng.gen::<f32>());
                let sm = self.sample_at(s);
                let hb = 0.5 * sm.width
                    + if side < 0.0 {
                        sm.shoulder_l
                    } else {
                        sm.shoulder_r
                    };
                let mut p = sm.pos + sm.right * side * (hb + off);
                let clear = match kind {
                    SceneryKind::Mesa => 40.0 * scale,
                    SceneryKind::Dune => 14.0 * scale,
                    SceneryKind::Acacia => 4.0,
                    _ => 1.5,
                };
                if self.distance_to_centreline(p.x, p.z) < self.max_half_band + clear {
                    continue;
                }
                p.y = self.terrain_height(p.x, p.z);
                out.push(SceneryItem {
                    kind,
                    pos: p,
                    yaw,
                    scale,
                });
            }
        }
        out
    }
}
