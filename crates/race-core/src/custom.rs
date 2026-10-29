//! Custom tracks (spec 19): the saved data model, the shaping pipeline that
//! turns a finger stroke or a recorded run into a legal closed track, feature
//! auto-fill and placement rules, the conversion to a [`TrackDef`], the
//! process-wide registry behind [`TrackId::Custom`], and the run [`Recorder`].
//!
//! Plan coordinates are metres in a [`Vec2`] that map to the world as
//! `(x, y) -> (x, height, z = y)`. Lateral offsets follow [`Feature`]'s
//! convention (`+` = right of the direction of travel, in world terms).

use std::fmt;
use std::sync::{Arc, Mutex};

use glam::{Vec2, Vec3};
use rand::{Rng, SeedableRng};
use rand_pcg::Pcg32;
use serde::{Deserialize, Serialize};

use crate::math::mix64;
use crate::track::{Feature, Track, TrackDef, TrackId, TrackPoint};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Highest hills level (0 flat, 1 rolling, 2 hilly, 3 steep).
pub const HILLS_MAX: u8 = 3;
/// Per hills level 1..=3: target peak-to-peak height (m) and the steepest
/// allowed grade (rise over run).
const HILL_LEVELS: [(f32, f32); 3] = [(4.0, 0.05), (9.0, 0.08), (15.0, 0.12)];

/// Spacing of shaped centreline points (m).
pub const SPACING: f32 = 5.0;
/// Smallest allowed corner radius (m), measured as the circumradius of three
/// consecutive shaped points ([`min_radius`]).
pub const MIN_RADIUS: f32 = 14.0;
/// Shortest lap (m).
pub const MIN_LAP: f32 = 300.0;
/// Longest lap (m).
pub const MAX_LAP: f32 = 3000.0;
/// Most centreline points in a saved track.
pub const MAX_POINTS: usize = 600;
/// Most features in a saved track.
pub const MAX_FEATURES: usize = 64;
/// Longest canonical encoding ([`CustomTrack::to_bytes`]): 600 points, 64 features,
/// a 24-character name of four-byte characters, and the hills byte of format 2.
pub const MAX_BYTES: usize = 2831;
/// Longest track name (characters).
pub const MAX_NAME: usize = 24;
/// Strokes whose ends are further apart than this need `auto_close` (m).
pub const CLOSE_GAP: f32 = 25.0;
/// Self-crossings with a loop shorter than this are cut out (m).
pub const LOOP_CUT: f32 = 60.0;
/// A crossing must be at least this sharp to become a bridge (degrees).
pub const MIN_CROSS_ANGLE: f32 = 25.0;
/// Most crossings (bridges) a track may have.
pub const MAX_CROSSINGS: usize = 2;
/// Height of a bridge deck above the lower road (m).
pub const BRIDGE_HEIGHT: f32 = 7.0;
/// Half-length of the flat crown at the top of a bridge (m).
pub const BRIDGE_CROWN: f32 = 4.0;
/// Length of each cosine ramp up to a bridge crown (m). The steepest grade is
/// `BRIDGE_HEIGHT / 2 * PI / BRIDGE_RAMP` = 13.7 % (spec 18.1 asks for <= 15 %).
pub const BRIDGE_RAMP: f32 = 80.0;
/// Total reach of a bridge along the lap each side of its crown centre (m).
pub const BRIDGE_REACH: f32 = BRIDGE_CROWN + BRIDGE_RAMP;
/// Features are at least this far apart along the lap (m).
pub const FEATURE_GAP: f32 = 40.0;
/// Features keep this far from the start line (m).
pub const START_CLEAR: f32 = 30.0;
/// Features keep this far from a bridge ramp (and from the lower road at a
/// crossing) (m).
pub const CROSSING_CLEAR: f32 = 30.0;
/// A [`Gap`](CustomFeatureKind::Gap) needs a straight at least this long (m).
pub const GAP_STRAIGHT: f32 = 60.0;
/// A straight must be at least this long to be listed in [`Shaped::straights`] (m).
pub const MIN_STRAIGHT: f32 = 40.0;
/// Points with a +-15 m radius at or above this count as straight (m).
const STRAIGHT_RADIUS: f32 = 120.0;
/// Corners are runs with a +-10 m radius below this (m).
const CORNER_RADIUS: f32 = 80.0;

/// Kicker ramp numbers (the Savanna kicker): length, height, lip.
const KICKER: (f32, f32, f32) = (12.0, 1.5, 1.0);
/// Canyon jump numbers: ramp length / height, gap length.
const JUMP_RAMP: (f32, f32) = (13.0, 2.0);
const JUMP_GAP: f32 = 11.0;
const PAD_LEN: f32 = 6.0;
const COIN_COUNT: u8 = 6;
const COIN_SPACING: f32 = 3.0;
const EGG_COUNT: u8 = 4;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// What a custom feature is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CustomFeatureKind {
    /// A row of 4 Power Egg targets.
    Eggs,
    /// A kicker ramp (the Savanna one).
    Ramp,
    /// A canyon jump: ramp then gap.
    Gap,
    /// A log hurdle.
    Log,
    /// A boost pad.
    Boost,
    /// A line of 6 coins.
    Coins,
}

impl CustomFeatureKind {
    /// Every kind, in wire order.
    pub const ALL: [CustomFeatureKind; 6] = [
        CustomFeatureKind::Eggs,
        CustomFeatureKind::Ramp,
        CustomFeatureKind::Gap,
        CustomFeatureKind::Log,
        CustomFeatureKind::Boost,
        CustomFeatureKind::Coins,
    ];

    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(c: u8) -> Option<Self> {
        Self::ALL.get(c as usize).copied()
    }

    /// Metres the feature occupies before / after its anchor `s`.
    fn footprint(self) -> (f32, f32) {
        match self {
            CustomFeatureKind::Eggs | CustomFeatureKind::Log => (0.0, 0.0),
            CustomFeatureKind::Ramp => (0.0, KICKER.0 + KICKER.2),
            CustomFeatureKind::Gap => (JUMP_RAMP.0, JUMP_GAP),
            CustomFeatureKind::Boost => (0.0, PAD_LEN),
            CustomFeatureKind::Coins => (0.0, COIN_COUNT as f32 * COIN_SPACING),
        }
    }
}

/// One placed feature of a custom track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CustomFeature {
    /// Kind.
    pub kind: CustomFeatureKind,
    /// Arc length of the feature's anchor from the start line, decimetres. For
    /// a [`Gap`](CustomFeatureKind::Gap) it is where the hole starts (its ramp
    /// is the 13 m before).
    pub s_dm: u16,
    /// Lateral offset from the centreline, decimetres (+ = right).
    pub lateral_dm: i16,
}

impl CustomFeature {
    /// Arc length in metres.
    pub fn s(&self) -> f32 {
        self.s_dm as f32 / 10.0
    }

    /// Lateral offset in metres.
    pub fn lateral(&self) -> f32 {
        self.lateral_dm as f32 / 10.0
    }
}

/// A saved custom track (spec 19.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomTrack {
    /// Format version: 1, or 2 when `hills` is non-zero (derived by
    /// [`CustomTrack::to_bytes`]).
    pub v: u8,
    /// Name, at most [`MAX_NAME`] characters.
    pub name: String,
    /// A built-in race track whose theme, shoulders and scenery are borrowed.
    pub look: TrackId,
    /// Laps, 1..=9.
    pub laps: u8,
    /// Road width in decimetres, 140..=220.
    pub width_dm: u8,
    /// The shaped centreline in decimetres, closed, ~5 m apart, the start
    /// line at point 0 facing point 1.
    pub points: Vec<[i16; 2]>,
    /// Features.
    pub features: Vec<CustomFeature>,
    /// Seed for terrain and scenery.
    pub seed: u32,
    /// Hills, 0..=[`HILLS_MAX`]: 0 flat, 1 rolling, 2 hilly, 3 steep. Absent in
    /// older saves, which are flat.
    #[serde(default)]
    pub hills: u8,
}

/// Why a [`CustomTrack`] (or its bytes) is not acceptable.
#[derive(Clone, Debug, PartialEq)]
pub enum CustomError {
    /// Unknown format version.
    Version(u8),
    /// Name empty, too long or not valid text.
    Name,
    /// `look` is not one of the eight race tracks.
    Look,
    /// Laps outside 1..=9.
    Laps,
    /// Width outside 140..=220 dm.
    Width,
    /// Hills outside 0..=3.
    Hills,
    /// Wrong number of points, or points not about 5 m apart.
    Points,
    /// A feature (by index) is out of range or malformed.
    Feature(usize),
    /// The bytes are truncated, oversized or otherwise malformed.
    Malformed(&'static str),
    /// The geometry is not a legal track.
    Shape(ShapeError),
}

impl fmt::Display for CustomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CustomError::Version(v) => write!(f, "unknown track version {v}"),
            CustomError::Name => write!(f, "bad track name"),
            CustomError::Look => write!(f, "the look must be one of the eight race tracks"),
            CustomError::Laps => write!(f, "laps must be 1 to 9"),
            CustomError::Width => write!(f, "width must be 14 to 22 m"),
            CustomError::Hills => write!(f, "hills must be 0 to 3"),
            CustomError::Points => write!(f, "bad centreline"),
            CustomError::Feature(i) => write!(f, "feature {i} is out of range"),
            CustomError::Malformed(m) => write!(f, "malformed track data: {m}"),
            CustomError::Shape(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CustomError {}

impl From<ShapeError> for CustomError {
    fn from(e: ShapeError) -> Self {
        CustomError::Shape(e)
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CustomError> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len());
        let end = end.ok_or(CustomError::Malformed("truncated"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CustomError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, CustomError> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn i16(&mut self) -> Result<i16, CustomError> {
        let s = self.take(2)?;
        Ok(i16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, CustomError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
}

impl CustomTrack {
    /// Checks every range and count (name, look, laps, width, point and
    /// feature counts). Does not check the geometry; [`to_def`] does.
    pub fn validate(&self) -> Result<(), CustomError> {
        if self.v != 1 && self.v != 2 {
            return Err(CustomError::Version(self.v));
        }
        let chars = self.name.chars().count();
        if chars == 0 || chars > MAX_NAME || self.name.chars().any(|c| c.is_control()) {
            return Err(CustomError::Name);
        }
        if !TrackId::RACE_ALL.contains(&self.look) {
            return Err(CustomError::Look);
        }
        if !(1..=9).contains(&self.laps) {
            return Err(CustomError::Laps);
        }
        if !(140..=220).contains(&self.width_dm) {
            return Err(CustomError::Width);
        }
        if self.points.len() < 48 || self.points.len() > MAX_POINTS {
            return Err(CustomError::Points);
        }
        if self.features.len() > MAX_FEATURES {
            return Err(CustomError::Feature(MAX_FEATURES));
        }
        if self.hills > HILLS_MAX {
            return Err(CustomError::Hills);
        }
        Ok(())
    }

    /// The canonical byte encoding: version, name (length byte + UTF-8), look
    /// (its [`TrackId::index`]), laps, width, points (u16 count, then x, y as
    /// little-endian i16), features (u8 count, then kind byte, `s_dm` u16,
    /// `lateral_dm` i16), seed (u32). A hilly track (`hills` > 0) is version 2
    /// and adds one trailing byte, the hills level; flat tracks keep the
    /// version 1 bytes (and so their hashes). The version byte is derived from
    /// `hills`, never from `v`. Stable across serde versions; this is what
    /// [`CustomTrack::hash`] covers and what the online transfer chunks.
    ///
    /// Counts beyond what the format can express are truncated; call
    /// [`CustomTrack::validate`] first if the track is untrusted.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(16 + self.name.len() + self.points.len() * 4 + self.features.len() * 5);
        o.push(if self.hills > 0 { 2 } else { 1 });
        let name = self.name.as_bytes();
        let nl = name.len().min(255);
        o.push(nl as u8);
        o.extend_from_slice(&name[..nl]);
        o.push(self.look.index().min(255) as u8);
        o.push(self.laps);
        o.push(self.width_dm);
        let np = self.points.len().min(u16::MAX as usize);
        o.extend_from_slice(&(np as u16).to_le_bytes());
        for p in &self.points[..np] {
            o.extend_from_slice(&p[0].to_le_bytes());
            o.extend_from_slice(&p[1].to_le_bytes());
        }
        let nf = self.features.len().min(255);
        o.push(nf as u8);
        for f in &self.features[..nf] {
            o.push(f.kind.code());
            o.extend_from_slice(&f.s_dm.to_le_bytes());
            o.extend_from_slice(&f.lateral_dm.to_le_bytes());
        }
        o.extend_from_slice(&self.seed.to_le_bytes());
        if self.hills > 0 {
            o.push(self.hills);
        }
        o
    }

    /// Decodes [`CustomTrack::to_bytes`]. Rejects (never panics on) truncated,
    /// oversized (> 600 points, > 64 features, name > 24 characters), trailing
    /// or otherwise malformed input, and runs [`CustomTrack::validate`].
    pub fn from_bytes(bytes: &[u8]) -> Result<CustomTrack, CustomError> {
        // 1 + 1 + 96 + 1 + 1 + 1 + 2 + 600 * 4 + 1 + 64 * 5 + 4 + 1 (hills, v2)
        if bytes.len() > MAX_BYTES {
            return Err(CustomError::Malformed("too long"));
        }
        let mut r = Reader { b: bytes, at: 0 };
        let v = r.u8()?;
        if v != 1 && v != 2 {
            return Err(CustomError::Version(v));
        }
        let nl = r.u8()? as usize;
        if nl > MAX_NAME * 4 {
            return Err(CustomError::Name);
        }
        let name = std::str::from_utf8(r.take(nl)?).map_err(|_| CustomError::Name)?.to_string();
        if name.chars().count() > MAX_NAME {
            return Err(CustomError::Name);
        }
        let look = TrackId::from_index(r.u8()? as usize).ok_or(CustomError::Look)?;
        let laps = r.u8()?;
        let width_dm = r.u8()?;
        let np = r.u16()? as usize;
        if np > MAX_POINTS {
            return Err(CustomError::Points);
        }
        let mut points = Vec::with_capacity(np);
        for _ in 0..np {
            let x = r.i16()?;
            let y = r.i16()?;
            points.push([x, y]);
        }
        let nf = r.u8()? as usize;
        if nf > MAX_FEATURES {
            return Err(CustomError::Feature(nf));
        }
        let mut features = Vec::with_capacity(nf);
        for i in 0..nf {
            let kind = CustomFeatureKind::from_code(r.u8()?).ok_or(CustomError::Feature(i))?;
            let s_dm = r.u16()?;
            let lateral_dm = r.i16()?;
            features.push(CustomFeature { kind, s_dm, lateral_dm });
        }
        let seed = r.u32()?;
        let hills = if v == 2 { r.u8()? } else { 0 };
        if v == 2 && !(1..=HILLS_MAX).contains(&hills) {
            return Err(CustomError::Malformed("bad hills"));
        }
        if r.at != bytes.len() {
            return Err(CustomError::Malformed("trailing bytes"));
        }
        let t = CustomTrack { v, name, look, laps, width_dm, points, features, seed, hills };
        t.validate()?;
        Ok(t)
    }

    /// FNV-1a 64 over [`CustomTrack::to_bytes`]: the key for bests, ghosts,
    /// file names and online transfer.
    pub fn hash(&self) -> u64 {
        fnv1a(&self.to_bytes())
    }

    /// The centreline in metres.
    pub fn points_m(&self) -> Vec<Vec2> {
        self.points.iter().map(|p| Vec2::new(p[0] as f32 / 10.0, p[1] as f32 / 10.0)).collect()
    }

    /// Road width in metres.
    pub fn width(&self) -> f32 {
        self.width_dm as f32 / 10.0
    }

    /// Analyse an authored track without moving or smoothing its saved points.
    pub fn shaped(&self) -> Result<Shaped, CustomError> {
        self.validate()?;
        let pts = self.points_m();
        let n = pts.len();
        for i in 0..n {
            let d = pts[i].distance(pts[(i + 1) % n]);
            if !(1.0..=12.0).contains(&d) {
                return Err(CustomError::Points);
            }
        }
        let (i, radius) = argmin_radius(&pts);
        if radius < MIN_RADIUS - 1e-3 {
            return Err(CustomError::Shape(ShapeError::TooTight { at: pts[i] }));
        }
        analyse(pts, self.width(), 1.05).map_err(CustomError::from)
    }

    /// Method form of [`to_def`].
    pub fn to_def(&self) -> Result<TrackDef, CustomError> {
        to_def(self)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

struct Installed {
    def: TrackDef,
    track: Arc<Track>,
    look: TrackId,
}

struct Registry {
    generation: u64,
    cur: Option<Installed>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry { generation: 0, cur: None });

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Makes `def` the current custom track (its `id` is forced to
/// [`TrackId::Custom`]), builds it, and returns the new generation (geometry
/// caches key on it). Panics only if `def` has fewer than 4 points (use
/// [`to_def`], which validates).
pub fn install(mut def: TrackDef) -> u64 {
    def.id = TrackId::Custom;
    let track = Arc::new(Track::build(&def));
    let look = TrackId::RACE_ALL.iter().copied().find(|id| id.def().theme == def.theme).unwrap_or(TrackId::Savanna);
    let mut r = registry();
    r.generation += 1;
    r.cur = Some(Installed { def, track, look });
    r.generation
}

/// The built-in race track whose theme the installed custom track borrows
/// (Savanna if none is installed or the theme matches none). For per-track
/// styling keyed on a [`TrackId`] that cannot see the theme (trackside decor,
/// sky, music).
pub fn look() -> TrackId {
    registry().cur.as_ref().map_or(TrackId::Savanna, |i| i.look)
}

/// How many times a track has been installed (0 = never).
pub fn generation() -> u64 {
    registry().generation
}

/// The installed custom track and its generation, if any.
pub fn current() -> Option<(u64, Arc<Track>)> {
    let r = registry();
    r.cur.as_ref().map(|i| (r.generation, i.track.clone()))
}

pub(crate) fn installed_def() -> Option<TrackDef> {
    registry().cur.as_ref().map(|i| i.def.clone())
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn cross(a: Vec2, b: Vec2) -> f32 {
    a.x * b.y - a.y * b.x
}

fn cyc_dist(a: f32, b: f32, len: f32) -> f32 {
    let d = (a - b).rem_euclid(len);
    d.min(len - d)
}

/// Cumulative arc lengths of a closed polyline: `n + 1` entries, the last the lap.
fn cum_lengths(pts: &[Vec2]) -> Vec<f32> {
    let n = pts.len();
    let mut c = Vec::with_capacity(n + 1);
    c.push(0.0);
    for i in 0..n {
        let l = c[i] + pts[i].distance(pts[(i + 1) % n]);
        c.push(l);
    }
    c
}

fn closed_len(pts: &[Vec2]) -> f32 {
    *cum_lengths(pts).last().unwrap_or(&0.0)
}

/// Arc position (along a closed polyline) of the point nearest to `a`, and its distance.
fn nearest_arc(pts: &[Vec2], cum: &[f32], a: Vec2) -> (f32, f32) {
    let n = pts.len();
    let mut best = (0.0, f32::INFINITY);
    for i in 0..n {
        let p = pts[i];
        let q = pts[(i + 1) % n];
        let d = q - p;
        let l2 = d.length_squared();
        let t = if l2 > 1e-12 { ((a - p).dot(d) / l2).clamp(0.0, 1.0) } else { 0.0 };
        let dist = (p + d * t).distance(a);
        if dist < best.1 {
            best = (cum[i] + t * l2.sqrt(), dist);
        }
    }
    best
}

/// Resamples a closed polyline to `round(L / spacing)` equally spaced points,
/// the first at the arc position nearest `anchor` (or at point 0).
fn resample_closed(pts: &[Vec2], spacing: f32, anchor: Option<Vec2>) -> Vec<Vec2> {
    let n = pts.len();
    if n < 2 {
        return pts.to_vec();
    }
    let cum = cum_lengths(pts);
    let total = cum[n];
    if total < 1e-3 {
        return pts.to_vec();
    }
    let m = ((total / spacing).round() as usize).max(8);
    let step = total / m as f32;
    let s0 = anchor.map_or(0.0, |a| nearest_arc(pts, &cum, a).0);
    let mut out = Vec::with_capacity(m);
    let mut s = s0.rem_euclid(total);
    let mut seg = 0usize;
    while seg + 1 < n && cum[seg + 1] <= s {
        seg += 1;
    }
    for _ in 0..m {
        if s >= total {
            s -= total;
            seg = 0;
        }
        while seg + 1 < n && cum[seg + 1] <= s {
            seg += 1;
        }
        let l = (cum[seg + 1] - cum[seg]).max(1e-9);
        let t = ((s - cum[seg]) / l).clamp(0.0, 1.0);
        out.push(pts[seg].lerp(pts[(seg + 1) % n], t));
        s += step;
    }
    out
}

/// Circumradius of the points `k` steps either side of `i` on a closed polyline.
fn radius_k(pts: &[Vec2], i: usize, k: usize) -> f32 {
    let n = pts.len();
    let a = pts[(i + n - k % n) % n];
    let b = pts[i];
    let c = pts[(i + k) % n];
    let (ab, bc, ac) = (a.distance(b), b.distance(c), a.distance(c));
    let area2 = cross(b - a, c - a).abs();
    if ab < 1e-4 || bc < 1e-4 {
        return f32::INFINITY;
    }
    if area2 < 1e-6 {
        return if (b - a).dot(c - b) <= 0.0 { 0.0 } else { f32::INFINITY };
    }
    ab * bc * ac / (2.0 * area2)
}

/// The smallest corner radius of a closed polyline: the circumradius of three
/// consecutive points. Shaped tracks have this >= [`MIN_RADIUS`].
pub fn min_radius(points: &[Vec2]) -> f32 {
    if points.len() < 3 {
        return f32::INFINITY;
    }
    (0..points.len()).map(|i| radius_k(points, i, 1)).fold(f32::INFINITY, f32::min)
}

fn argmin_radius(pts: &[Vec2]) -> (usize, f32) {
    let mut best = (0, f32::INFINITY);
    for i in 0..pts.len() {
        let r = radius_k(pts, i, 1);
        if r < best.1 {
            best = (i, r);
        }
    }
    best
}

fn seg_hit(a: Vec2, b: Vec2, c: Vec2, d: Vec2) -> Option<(f32, f32)> {
    if a.x.max(b.x) < c.x.min(d.x)
        || c.x.max(d.x) < a.x.min(b.x)
        || a.y.max(b.y) < c.y.min(d.y)
        || c.y.max(d.y) < a.y.min(b.y)
    {
        return None;
    }
    let r = b - a;
    let s = d - c;
    let den = cross(r, s);
    if den.abs() < 1e-9 {
        return None;
    }
    let q = c - a;
    let t = cross(q, s) / den;
    let u = cross(q, r) / den;
    // A little slack so a crossing exactly at a vertex is not lost to rounding
    // (the caller merges the duplicates).
    const EPS: f32 = 1e-4;
    if (-EPS..=1.0 + EPS).contains(&t) && (-EPS..=1.0 + EPS).contains(&u) {
        Some((t, u))
    } else {
        None
    }
}

fn quantise(p: Vec2) -> Vec2 {
    (p * 10.0).round() / 10.0
}

/// Cyclic runs of `true`: `(start, count)`. All-true is one run `(0, n)`.
fn cyclic_runs(flags: &[bool]) -> Vec<(usize, usize)> {
    let n = flags.len();
    let Some(base) = (0..n).find(|&i| !flags[i]) else {
        return if n > 0 { vec![(0, n)] } else { vec![] };
    };
    let mut runs = Vec::new();
    let mut k = 1;
    while k <= n {
        let i = (base + k) % n;
        if flags[i] {
            let mut c = 0;
            while c < n && flags[(i + c) % n] {
                c += 1;
            }
            runs.push((i, c));
            k += c;
        } else {
            k += 1;
        }
    }
    runs
}

// ---------------------------------------------------------------------------
// Shaping (spec 19.2)
// ---------------------------------------------------------------------------

/// Why a stroke cannot become a track. Each variant carries the world position
/// of the problem (plan metres) so the designer can highlight it, except the
/// whole-lap ones.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeError {
    /// The ends are `gap` m apart and `auto_close` was off.
    NotClosed {
        /// Distance between the ends (m).
        gap: f32,
    },
    /// The lap is shorter than 300 m.
    TooShort {
        /// Lap length (m).
        len: f32,
    },
    /// The lap is longer than 3000 m.
    TooLong {
        /// Lap length (m).
        len: f32,
    },
    /// More than two crossings; `at` is the third.
    TooManyCrossings {
        /// Where.
        at: Vec2,
    },
    /// A crossing sharper than 25 degrees would not read as a bridge.
    ShallowCrossing {
        /// Where.
        at: Vec2,
    },
    /// Two road sections that do not cross are closer than road width + 4 m,
    /// or a crossing is too near another crossing (or its own other branch)
    /// for its bridge ramps.
    TooClose {
        /// Where.
        at: Vec2,
    },
    /// A corner is still tighter than 14 m after smoothing.
    TooTight {
        /// Where.
        at: Vec2,
    },
    /// The start line would sit on a bridge ramp; move the start.
    StartOnBridge {
        /// The crossing.
        at: Vec2,
    },
}

impl ShapeError {
    /// The location to highlight, if the error has one.
    pub fn at(&self) -> Option<Vec2> {
        match *self {
            ShapeError::TooManyCrossings { at }
            | ShapeError::ShallowCrossing { at }
            | ShapeError::TooClose { at }
            | ShapeError::TooTight { at }
            | ShapeError::StartOnBridge { at } => Some(at),
            _ => None,
        }
    }
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShapeError::NotClosed { gap } => write!(f, "the loop is open by {gap:.0} m"),
            ShapeError::TooShort { len } => write!(f, "too short: {len:.0} m (min {MIN_LAP:.0})"),
            ShapeError::TooLong { len } => write!(f, "too long: {len:.0} m (max {MAX_LAP:.0})"),
            ShapeError::TooManyCrossings { .. } => write!(f, "more than two crossings"),
            ShapeError::ShallowCrossing { .. } => write!(f, "this crossing is too shallow for a bridge"),
            ShapeError::TooClose { .. } => write!(f, "the road is too close to itself here"),
            ShapeError::TooTight { .. } => write!(f, "this corner is too tight"),
            ShapeError::StartOnBridge { .. } => write!(f, "move the start line off this bridge"),
        }
    }
}

impl std::error::Error for ShapeError {}

/// A crossing found by [`shape`]: where two sections of the lap pass over each other.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedCrossing {
    /// Index of the first edge (points `a_idx` -> `a_idx + 1`), `a_idx < b_idx`.
    pub a_idx: usize,
    /// Index of the second edge.
    pub b_idx: usize,
    /// The crossing point (plan metres).
    pub pos: Vec2,
    /// The angle between the two roads, 0..=90 degrees.
    pub angle_deg: f32,
    /// Arc length of the branch that rises onto the bridge (m).
    pub s_high: f32,
    /// Arc length of the branch that stays on the ground (m).
    pub s_low: f32,
}

/// A corner of a shaped track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Corner {
    /// Index of the tightest point.
    pub idx: usize,
    /// Its radius (m), taken over +-10 m.
    pub radius: f32,
    /// +1 if it turns left (in world terms, toward negative lateral), -1 if
    /// right. The inside of the corner is at lateral `-inside_sign * d`.
    pub inside_sign: i8,
}

/// A shaped, legal track centreline.
#[derive(Clone, Debug, PartialEq)]
pub struct Shaped {
    /// Closed centreline, ~5 m spacing, on the decimetre grid; point 0 is the
    /// start, facing point 1.
    pub points: Vec<Vec2>,
    /// Lap length (m).
    pub length: f32,
    /// Road width the shape was checked for (m).
    pub width: f32,
    /// Crossings (at most 2).
    pub crossings: Vec<ShapedCrossing>,
    /// Corners, in lap order.
    pub corners: Vec<Corner>,
    /// Straights of at least [`MIN_STRAIGHT`]: `(start_idx, end_idx, len)`;
    /// the indices are inclusive and may wrap past the last point.
    pub straights: Vec<(usize, usize, f32)>,
}

impl Shaped {
    /// Mean point spacing (m).
    pub fn spacing(&self) -> f32 {
        self.length / self.points.len() as f32
    }

    /// The centreline point at arc length `s` (wraps).
    pub fn point_at(&self, s: f32) -> Vec2 {
        let n = self.points.len();
        let f = s.rem_euclid(self.length) / self.spacing();
        let i = (f.floor() as usize).min(n - 1);
        self.points[i].lerp(self.points[(i + 1) % n], f - i as f32)
    }

    /// Unit direction of travel at arc length `s`.
    pub fn dir_at(&self, s: f32) -> Vec2 {
        let n = self.points.len();
        let i = ((s.rem_euclid(self.length) / self.spacing()).floor() as usize).min(n - 1);
        (self.points[(i + 1) % n] - self.points[i]).normalize_or_zero()
    }

    /// Nearest point of the centreline to `p`: `(arc length, distance)`.
    pub fn nearest(&self, p: Vec2) -> (f32, f32) {
        nearest_arc(&self.points, &cum_lengths(&self.points), p)
    }

    /// The same track with a different start line: point `idx` becomes the
    /// start, and `flip` reverses the direction of travel (the start stays at
    /// `idx`). Re-runs the checks, so it can fail with
    /// [`ShapeError::StartOnBridge`].
    pub fn restart(&self, idx: usize, flip: bool) -> Result<Shaped, ShapeError> {
        let n = self.points.len();
        let idx = idx % n;
        let mut pts: Vec<Vec2> = (0..n).map(|k| self.points[(idx + k) % n]).collect();
        if flip {
            pts[1..].reverse();
        }
        analyse(pts, self.width, 1.0)
    }
}

struct RawX {
    i: usize,
    j: usize,
    pos: Vec2,
    s_a: f32,
    s_b: f32,
    angle: f32,
}

fn find_crossings(pts: &[Vec2], cum: &[f32]) -> Vec<RawX> {
    let n = pts.len();
    let mut out: Vec<RawX> = Vec::new();
    for i in 0..n {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        for j in (i + 2)..n {
            if i == 0 && j == n - 1 {
                continue;
            }
            let (c, d) = (pts[j], pts[(j + 1) % n]);
            let Some((t, u)) = seg_hit(a, b, c, d) else { continue };
            // A crossing at a vertex shows up on both edges that meet there.
            let pos = a.lerp(b, t);
            if out.iter().any(|x| x.pos.distance(pos) < 2.0) {
                continue;
            }
            let (da, db) = ((b - a).normalize_or_zero(), (d - c).normalize_or_zero());
            let angle = da.dot(db).abs().clamp(0.0, 1.0).acos().to_degrees();
            out.push(RawX {
                i,
                j,
                pos,
                s_a: cum[i] + t * (cum[i + 1] - cum[i]),
                s_b: cum[j] + u * (cum[j + 1] - cum[j]),
                angle,
            });
        }
    }
    out.sort_by(|p, q| p.s_a.total_cmp(&q.s_a));
    out
}

/// Cuts out the smaller side of every self-crossing whose loop is shorter than
/// [`LOOP_CUT`], then resamples.
fn cut_loops(mut pts: Vec<Vec2>, anchor: Vec2) -> Vec<Vec2> {
    for _ in 0..16 {
        if pts.len() < 8 {
            break;
        }
        let cum = cum_lengths(&pts);
        let total = cum[pts.len()];
        let xs = find_crossings(&pts, &cum);
        let pick = xs
            .iter()
            .map(|x| (x, (x.s_b - x.s_a).min(total - (x.s_b - x.s_a))))
            .filter(|(_, l)| *l < LOOP_CUT)
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let Some((x, _)) = pick else { break };
        let mut new: Vec<Vec2>;
        if x.s_b - x.s_a < total - (x.s_b - x.s_a) {
            new = pts[..=x.i].to_vec();
            new.push(x.pos);
            new.extend_from_slice(&pts[x.j + 1..]);
        } else {
            new = vec![x.pos];
            new.extend_from_slice(&pts[x.i + 1..=x.j]);
        }
        pts = resample_closed(&new, SPACING, Some(anchor));
    }
    pts
}

/// The closest pair of non-crossing road sections nearer than `width + 4` m.
fn closest_pair(pts: &[Vec2], cum: &[f32], xs: &[RawX], width: f32) -> Option<Vec2> {
    let n = pts.len();
    let total = cum[n];
    let sp = total / n as f32;
    let sep_min = 40.0f32.max(1.6 * width);
    let limit = width + 4.0;
    let zones: Vec<(f32, f32, f32)> = xs
        .iter()
        .map(|x| (x.s_a, x.s_b, limit / crate::math::sin(x.angle.max(MIN_CROSS_ANGLE).to_radians()) + 4.0))
        .collect();
    let mut best: Option<(f32, Vec2)> = None;
    for i in 0..n {
        for j in (i + 1)..n {
            let arc = (j - i) as f32 * sp;
            if arc.min(total - arc) <= sep_min {
                continue;
            }
            let d = pts[i].distance(pts[j]);
            if d >= limit || best.map_or(false, |(b, _)| d >= b) {
                continue;
            }
            let (si, sj) = (cum[i], cum[j]);
            let exempt = zones.iter().any(|&(sa, sb, z)| {
                (cyc_dist(si, sa, total) <= z && cyc_dist(sj, sb, total) <= z)
                    || (cyc_dist(si, sb, total) <= z && cyc_dist(sj, sa, total) <= z)
            });
            if !exempt {
                best = Some((d, pts[i].lerp(pts[j], 0.5)));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Picks which branch of each crossing rises and checks the bridge ramps fit:
/// the start line is clear of the ramp, the two branches of a crossing are
/// far enough apart along the lap, and ramps and lower roads of different
/// crossings do not meet. Returns `(s_high, s_low)` per crossing.
fn plan_bridges(xs: &[RawX], total: f32) -> Result<Vec<(f32, f32)>, ShapeError> {
    let mut plan: Vec<(f32, f32)> = Vec::new();
    for x in xs {
        if cyc_dist(x.s_a, x.s_b, total) < BRIDGE_REACH + 10.0 {
            return Err(ShapeError::TooClose { at: x.pos });
        }
        // The later branch rises, unless that puts the start on the ramp.
        let clear = |s: f32| cyc_dist(s, 0.0, total) >= BRIDGE_REACH + START_CLEAR;
        let (hi, lo) = if clear(x.s_b) {
            (x.s_b, x.s_a)
        } else if clear(x.s_a) {
            (x.s_a, x.s_b)
        } else {
            return Err(ShapeError::StartOnBridge { at: x.pos });
        };
        plan.push((hi, lo));
    }
    for k in 1..plan.len() {
        for m in 0..k {
            let (hi, lo) = plan[k];
            let (hj, lj) = plan[m];
            let ok = cyc_dist(hi, hj, total) >= 2.0 * BRIDGE_REACH + 8.0
                && cyc_dist(hi, lj, total) >= BRIDGE_REACH + 10.0
                && cyc_dist(lo, hj, total) >= BRIDGE_REACH + 10.0;
            if !ok {
                return Err(ShapeError::TooClose { at: xs[k].pos });
            }
        }
    }
    Ok(plan)
}

/// Too many crossings, or one too shallow to read as a bridge.
fn crossing_counts(xs: &[RawX]) -> Result<(), ShapeError> {
    if xs.len() > MAX_CROSSINGS {
        return Err(ShapeError::TooManyCrossings { at: xs[MAX_CROSSINGS].pos });
    }
    if let Some(x) = xs.iter().find(|x| x.angle < MIN_CROSS_ANGLE) {
        return Err(ShapeError::ShallowCrossing { at: x.pos });
    }
    Ok(())
}

/// Runs every check on a finished centreline and extracts crossings, corners
/// and straights. `lap_slack` widens the lap bounds (1.0 = exact).
fn analyse(points: Vec<Vec2>, width: f32, lap_slack: f32) -> Result<Shaped, ShapeError> {
    analyse_with(points, width, lap_slack, false).map(|(shaped, _)| shaped)
}

/// [`analyse`], optionally lenient: a geometric fault (crossings, closeness,
/// bridge room) is returned beside the loop, which then carries no crossings,
/// instead of failing. A lap outside its length bounds always fails.
fn analyse_with(points: Vec<Vec2>, width: f32, lap_slack: f32, lenient: bool) -> Result<(Shaped, Option<ShapeError>), ShapeError> {
    let n = points.len();
    let cum = cum_lengths(&points);
    let length = cum[n];
    if length < MIN_LAP / lap_slack {
        return Err(ShapeError::TooShort { len: length });
    }
    if length > MAX_LAP * lap_slack {
        return Err(ShapeError::TooLong { len: length });
    }
    let xs = find_crossings(&points, &cum);
    let checked = (|| {
        for x in &xs {
            if (x.s_b - x.s_a).min(length - (x.s_b - x.s_a)) < LOOP_CUT {
                return Err(ShapeError::TooClose { at: x.pos });
            }
        }
        crossing_counts(&xs)?;
        if let Some(at) = closest_pair(&points, &cum, &xs, width) {
            return Err(ShapeError::TooClose { at });
        }
        plan_bridges(&xs, length)
    })();
    let (plan, fault) = match checked {
        Ok(plan) => (plan, None),
        Err(e) if lenient => (Vec::new(), Some(e)),
        Err(e) => return Err(e),
    };
    let crossings = xs
        .iter()
        .zip(&plan)
        .map(|(x, &(s_high, s_low))| ShapedCrossing {
            a_idx: x.i,
            b_idx: x.j,
            pos: x.pos,
            angle_deg: x.angle,
            s_high,
            s_low,
        })
        .collect();
    let sp = length / n as f32;

    // Corners: cyclic runs of tight points, one corner per run.
    let r2: Vec<f32> = (0..n).map(|i| radius_k(&points, i, 2)).collect();
    let tight: Vec<bool> = r2.iter().map(|&r| r < CORNER_RADIUS).collect();
    let mut corners = Vec::new();
    for (start, count) in cyclic_runs(&tight) {
        let best = (0..count).map(|t| (start + t) % n).min_by(|&a, &b| r2[a].total_cmp(&r2[b])).unwrap_or(start);
        let (a, b) = (points[(best + n - 1) % n], points[(best + 1) % n]);
        // Plan (x, y) maps to world (x, z = y): a counter-clockwise plan turn
        // is a right turn in the world.
        let inside_sign = if cross(points[best] - a, b - points[best]) > 0.0 { -1 } else { 1 };
        corners.push(Corner { idx: best, radius: r2[best], inside_sign });
    }
    corners.sort_by_key(|c| c.idx);

    // Straights: cyclic runs of points with a large +-15 m radius.
    let straight: Vec<bool> = (0..n).map(|i| radius_k(&points, i, 3) >= STRAIGHT_RADIUS).collect();
    let mut straights = Vec::new();
    for (start, count) in cyclic_runs(&straight) {
        let len = count as f32 * sp;
        if len >= MIN_STRAIGHT && count < n {
            straights.push((start, (start + count - 1) % n, len));
        }
    }
    straights.sort_by_key(|s| s.0);
    Ok((Shaped { points, length, width, crossings, corners, straights }, fault))
}

/// Unit tangent at the end (`at_end`) or start of an open stroke, over ~10 m.
fn stroke_tangent(v: &[Vec2], at_end: bool) -> Vec2 {
    let n = v.len();
    let anchor = if at_end { v[n - 1] } else { v[0] };
    let mut other = anchor;
    for k in 1..n {
        other = if at_end { v[n - 1 - k] } else { v[k] };
        if other.distance(anchor) >= 10.0 {
            break;
        }
    }
    if at_end {
        (anchor - other).normalize_or_zero()
    } else {
        (other - anchor).normalize_or_zero()
    }
}

/// Turns a finger stroke (or a recorded run) in metres into a legal closed track.
///
/// `width` is the road width in metres; it drives the "road too close to
/// itself" rule (two non-crossing sections nearer than `width + 4` m).
///
/// 1. Near-duplicate points are dropped and a tail that overshoots the start
///    is trimmed. If the ends are more than 25 m apart, `auto_close` joins them
///    with a smooth Hermite curve and otherwise the result is
///    [`ShapeError::NotClosed`]; a smaller gap is always joined smoothly.
/// 2. The loop is denoised, resampled to 5 m and smoothed (adaptively, only
///    where the radius is under 14 m) until every corner has radius >= 14 m
///    ([`ShapeError::TooTight`] if it will not converge).
/// 3. The lap must be 300-3000 m.
/// 4. Tiny loops (self-crossings under 60 m along the lap) are cut out.
/// 5. Up to two real crossings (>= 25 degrees) are kept as bridges. More, a
///    shallow crossing, or two road sections closer than `width + 4` m without
///    crossing is an error carrying the location. Crossings must also leave
///    room for their bridge ramps ([`BRIDGE_REACH`]) clear of the start line
///    and of each other.
///
/// Point 0 is the point of the result nearest the stroke's start, facing along
/// the stroke. The returned points are on the decimetre grid so they round-trip
/// through [`custom_track_from`] unchanged.
pub fn shape(raw: &[Vec2], auto_close: bool, width: f32) -> Result<Shaped, ShapeError> {
    shape_with(raw, auto_close, width, false).map(|(shaped, _)| shaped)
}

/// [`shape`], but a loop that closes and has a sensible length is returned
/// even when it breaks a geometric rule, beside the first rule it breaks
/// ([`ShapeError::TooClose`], [`ShapeError::TooTight`], too many or shallow
/// crossings). The designer shows such a loop so it can be reshaped. A faulty
/// loop has no bridges planned and is not smoothed past the fault. Only
/// failures that leave no closed loop (too few points, not closed, a lap
/// outside 300-3000 m) are `Err`. A loop with no fault is exactly what
/// [`shape`] returns.
pub fn shape_lenient(raw: &[Vec2], auto_close: bool, width: f32) -> Result<(Shaped, Option<ShapeError>), ShapeError> {
    shape_with(raw, auto_close, width, true)
}

fn shape_with(raw: &[Vec2], auto_close: bool, width: f32, lenient: bool) -> Result<(Shaped, Option<ShapeError>), ShapeError> {
    let width = if width.is_finite() { width.clamp(8.0, 40.0) } else { 18.0 };
    // 1. Drop near-duplicates (and non-finite points).
    let mut v: Vec<Vec2> = Vec::with_capacity(raw.len().min(4096));
    for &p in raw {
        if !p.is_finite() {
            continue;
        }
        if v.last().map_or(true, |l| l.distance(p) >= 0.75) {
            v.push(p);
        }
    }
    let stroke_len: f32 = v.windows(2).map(|w| w[0].distance(w[1])).sum();
    if v.len() < 3 || stroke_len < 20.0 {
        return Err(ShapeError::TooShort { len: stroke_len });
    }
    if stroke_len > MAX_LAP * 2.0 {
        return Err(ShapeError::TooLong { len: stroke_len });
    }
    let anchor = v[0];

    // Trim an overshoot: the last 40 m crossing the first 40 m.
    {
        let mut cum = vec![0.0f32];
        for w in v.windows(2) {
            cum.push(cum[cum.len() - 1] + w[0].distance(w[1]));
        }
        let total = cum[cum.len() - 1];
        if total > 100.0 {
            let head: Vec<usize> = (0..v.len() - 1).filter(|&i| cum[i] < 40.0).collect();
            let tail: Vec<usize> = (0..v.len() - 1).filter(|&j| cum[j] > total - 40.0).collect();
            'find: for &i in &head {
                for &j in tail.iter().rev() {
                    if j <= i + 1 {
                        continue;
                    }
                    if let Some((t, _)) = seg_hit(v[i], v[i + 1], v[j], v[j + 1]) {
                        let x = v[i].lerp(v[i + 1], t);
                        let mut nv = vec![x];
                        nv.extend_from_slice(&v[i + 1..=j]);
                        v = nv;
                        break 'find;
                    }
                }
            }
        }
    }
    // Close the ends.
    let gap = v[0].distance(v[v.len() - 1]);
    if gap > CLOSE_GAP && !auto_close {
        return Err(ShapeError::NotClosed { gap });
    }
    let mut ring = v.clone();
    if gap > 2.0 {
        let (end, start) = (v[v.len() - 1], v[0]);
        let (m0, m1) = (stroke_tangent(&v, true) * gap, stroke_tangent(&v, false) * gap);
        let steps = ((gap / 2.5).ceil() as usize).max(2);
        for k in 1..steps {
            let t = k as f32 / steps as f32;
            let (t2, t3) = (t * t, t * t * t);
            ring.push(
                end * (2.0 * t3 - 3.0 * t2 + 1.0)
                    + m0 * (t3 - 2.0 * t2 + t)
                    + start * (-2.0 * t3 + 3.0 * t2)
                    + m1 * (t3 - t2),
            );
        }
    }

    // 2. Denoise at 2.5 m, then work at 5 m.
    let mut p = resample_closed(&ring, 2.5, None);
    for _ in 0..3 {
        let n = p.len();
        p = (0..n).map(|i| p[(i + n - 1) % n] * 0.25 + p[i] * 0.5 + p[(i + 1) % n] * 0.25).collect();
    }
    let len = closed_len(&p);
    if len > MAX_LAP + 100.0 {
        return Err(ShapeError::TooLong { len });
    }
    let mut p = resample_closed(&p, SPACING, Some(anchor));
    // 4. Tiny loops.
    p = cut_loops(p, anchor);
    // 3. Lap length (smoothing only shortens, so too short now is too short).
    let len = closed_len(&p);
    if len < MIN_LAP {
        return Err(ShapeError::TooShort { len });
    }
    if len > MAX_LAP + 50.0 {
        return Err(ShapeError::TooLong { len });
    }
    // Sections that are already too close cannot be fixed by smoothing.
    let mut first_fault = None;
    {
        let cum = cum_lengths(&p);
        let xs = find_crossings(&p, &cum);
        let fault = crossing_counts(&xs).err().or_else(|| closest_pair(&p, &cum, &xs, width).map(|at| ShapeError::TooClose { at }));
        if let Some(e) = fault {
            if !lenient {
                return Err(e);
            }
            first_fault = Some(e);
        }
    }
    // 2. Smooth until every radius is fine (also once snapped to the decimetre grid).
    let mut target = MIN_RADIUS + 0.8;
    let mut snapped = Vec::new();
    let unsmoothed = if lenient { p.clone() } else { Vec::new() };
    for _round in 0..6 {
        relax(&mut p, target, anchor);
        snapped = p.iter().map(|&x| quantise(x)).collect();
        if min_radius(&snapped) >= MIN_RADIUS - 1e-3 {
            break;
        }
        target += 1.5;
    }
    if lenient && closed_len(&snapped) < closed_len(&unsmoothed) * 0.85 {
        // Smoothing a corner that cannot be opened out (a hairpin) collapses the loop:
        // keep the loop as drawn so there is something to pull apart.
        snapped = unsmoothed.iter().map(|&x| quantise(x)).collect();
    }
    if min_radius(&snapped) < MIN_RADIUS - 1e-3 {
        let (i, _) = argmin_radius(&snapped);
        if !lenient {
            return Err(ShapeError::TooTight { at: snapped[i] });
        }
        first_fault = first_fault.or(Some(ShapeError::TooTight { at: snapped[i] }));
    }
    let (shaped, fault) = analyse_with(snapped, width, 1.0, lenient)?;
    Ok((shaped, first_fault.or(fault)))
}

/// Adaptive smoothing: pulls points that sit on a too-tight corner (and their
/// neighbours) toward the midpoint of their neighbours until the minimum radius
/// reaches `target` or the iteration cap is hit.
fn relax(p: &mut Vec<Vec2>, target: f32, anchor: Vec2) {
    const KERNEL: [f32; 3] = [0.5, 0.35, 0.15];
    for _ in 0..4000 {
        let n = p.len();
        if n < 8 {
            return;
        }
        let r: Vec<f32> = (0..n).map(|i| radius_k(p, i, 1)).collect();
        if r.iter().all(|&x| x >= target) {
            return;
        }
        let mut w = vec![0.0f32; n];
        for i in 0..n {
            if r[i] < target {
                for (d, &k) in KERNEL.iter().enumerate() {
                    for idx in [(i + d) % n, (i + n - d) % n] {
                        w[idx] = w[idx].max(k);
                    }
                }
            }
        }
        let new: Vec<Vec2> = (0..n)
            .map(|i| {
                let mid = (p[(i + n - 1) % n] + p[(i + 1) % n]) * 0.5;
                p[i] + (mid - p[i]) * w[i]
            })
            .collect();
        *p = resample_closed(&new, SPACING, Some(anchor));
    }
}


// ---------------------------------------------------------------------------
// Features: placement rules and auto-fill (spec 19.2)
// ---------------------------------------------------------------------------

/// Why a feature cannot go here (for the designer's tap-to-place).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceError {
    /// `s` is not on the lap.
    OutOfRange,
    /// Within 30 m of the start line.
    NearStart,
    /// Within 40 m of another feature (its index in the list).
    NearFeature(usize),
    /// On a bridge ramp (or within 30 m of one), or within 30 m of the lower
    /// road at a crossing.
    NearCrossing,
    /// A gap needs a straight of at least 60 m.
    NeedsStraight,
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaceError::OutOfRange => write!(f, "not on the road"),
            PlaceError::NearStart => write!(f, "too close to the start line"),
            PlaceError::NearFeature(_) => write!(f, "too close to another feature"),
            PlaceError::NearCrossing => write!(f, "too close to a bridge"),
            PlaceError::NeedsStraight => write!(f, "a gap needs a straight of 60 m"),
        }
    }
}

impl std::error::Error for PlaceError {}

/// Distance along a lap of `total` from the point `c` to the interval `[lo, hi]`.
fn interval_dist(lo: f32, hi: f32, c: f32, total: f32) -> f32 {
    if (c - lo).rem_euclid(total) <= hi - lo {
        0.0
    } else {
        cyc_dist(c, lo, total).min(cyc_dist(c, hi, total))
    }
}

fn make_feature(kind: CustomFeatureKind, s: f32, lateral: f32) -> CustomFeature {
    CustomFeature {
        kind,
        s_dm: (s * 10.0).round().clamp(0.0, u16::MAX as f32) as u16,
        lateral_dm: (lateral * 10.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16,
    }
}

/// Manual placement accepts any finite position on the road, including nearby
/// features and the start line. Automatic filling keeps its wider clearances.
pub fn place_ok(shaped: &Shaped, _existing: &[CustomFeature], _kind: CustomFeatureKind, s: f32) -> Result<(), PlaceError> {
    if s.is_finite() && s >= 0.0 && s < shaped.length { Ok(()) }
    else { Err(PlaceError::OutOfRange) }
}

fn auto_place_ok(
    shaped: &Shaped,
    existing: &[CustomFeature],
    kind: CustomFeatureKind,
    s: f32,
) -> Result<(), PlaceError> {
    let total = shaped.length;
    if !s.is_finite() || s < 0.0 || s >= total {
        return Err(PlaceError::OutOfRange);
    }
    let (before, after) = kind.footprint();
    let (lo, hi) = (s - before, s + after);
    if lo < START_CLEAR || hi > total - START_CLEAR {
        return Err(PlaceError::NearStart);
    }
    for (i, e) in existing.iter().enumerate() {
        if cyc_dist(s, e.s(), total) < FEATURE_GAP {
            return Err(PlaceError::NearFeature(i));
        }
    }
    for c in &shaped.crossings {
        if interval_dist(lo, hi, c.s_high, total) < BRIDGE_REACH + CROSSING_CLEAR
            || interval_dist(lo, hi, c.s_low, total) < CROSSING_CLEAR
        {
            return Err(PlaceError::NearCrossing);
        }
    }
    if kind == CustomFeatureKind::Gap {
        let sp = shaped.spacing();
        let (glo, ghi) = (lo - 6.0, hi + 6.0);
        let fits = shaped.straights.iter().any(|&(a, _, len)| {
            len >= GAP_STRAIGHT && (glo - a as f32 * sp).rem_euclid(total) + (ghi - glo) <= len
        });
        if !fits {
            return Err(PlaceError::NeedsStraight);
        }
    }
    Ok(())
}

/// Places features by the spec 19.2 rules, deterministically from `seed`:
/// a boost pad 25 m into the longest straight (where the corner before it
/// spits you out), a kicker ramp in the middle of the second-longest straight
/// if it is at least 80 m, an egg row about every 250 m on straights (and at
/// least one per lap if any spot is legal), and a coin line on the inside of
/// each of the three tightest corners. Every feature passes [`place_ok`].
/// The result is sorted by `s_dm`.
pub fn auto_fill(shaped: &Shaped, seed: u32) -> Vec<CustomFeature> {
    let mut rng = Pcg32::seed_from_u64(0xA070_F111_0000_0000 ^ seed as u64);
    let total = shaped.length;
    let sp = shaped.spacing();
    let mut out: Vec<CustomFeature> = Vec::new();
    let mut straights: Vec<(f32, f32)> =
        shaped.straights.iter().map(|&(a, _, len)| (a as f32 * sp, len)).collect();
    straights.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.total_cmp(&b.0)));

    let try_place = |out: &mut Vec<CustomFeature>, kind: CustomFeatureKind, s: f32, lateral: f32| -> bool {
        let s = s.rem_euclid(total);
        let f = make_feature(kind, s, lateral);
        if auto_place_ok(shaped, out, kind, f.s()).is_ok() {
            out.push(f);
            true
        } else {
            false
        }
    };

    // Boost: near the start of the longest straight.
    if let Some(&(a0, len)) = straights.first() {
        let mut off = 25.0;
        while off + PAD_LEN + 10.0 <= len {
            if try_place(&mut out, CustomFeatureKind::Boost, a0 + off, 0.0) {
                break;
            }
            off += 5.0;
        }
    }
    // Ramp: middle of the second-longest straight, if it is long enough.
    if let Some(&(a0, len)) = straights.get(1) {
        if len >= 80.0 {
            let mid = len * 0.5 - 6.0;
            'ramp: for k in 0..12 {
                for sign in [1.0f32, -1.0] {
                    let off = mid + sign * 5.0 * k as f32;
                    if off >= 10.0 && off <= len - 25.0 && try_place(&mut out, CustomFeatureKind::Ramp, a0 + off, 0.0) {
                        break 'ramp;
                    }
                }
            }
        }
    }
    // Egg rows about every 250 m, on straights.
    let usable: Vec<(f32, f32)> =
        straights.iter().filter(|s| s.1 >= 30.0).map(|&(a0, len)| (a0 + 15.0, a0 + len - 15.0)).collect();
    let mut eggs = 0;
    if !usable.is_empty() {
        let phase = rng.gen_range(0.0..250.0f32);
        let mut t = phase;
        while t < total {
            // The straight whose interior is nearest the target.
            let best = usable
                .iter()
                .map(|&(a, b)| (interval_dist(a, b.max(a), t, total), a, b.max(a)))
                .min_by(|x, y| x.0.total_cmp(&y.0));
            if let Some((d, a, b)) = best {
                if d <= 125.0 {
                    let centre = if d == 0.0 { t } else if cyc_dist(t, a, total) < cyc_dist(t, b, total) { a } else { b };
                    'egg: for k in 0..14 {
                        for sign in [1.0f32, -1.0] {
                            let s = centre + sign * 5.0 * k as f32;
                            if interval_dist(a, b, s, total) == 0.0 && try_place(&mut out, CustomFeatureKind::Eggs, s, 0.0) {
                                eggs += 1;
                                break 'egg;
                            }
                        }
                    }
                }
            }
            t += 250.0;
        }
    }
    if eggs == 0 {
        // At least one per lap: any legal spot, straights first.
        let mut spots: Vec<f32> = straights.iter().map(|&(a0, len)| a0 + len * 0.5).collect();
        spots.extend((0..(total / 5.0) as usize).map(|k| k as f32 * 5.0 + 2.5));
        for s in spots {
            if try_place(&mut out, CustomFeatureKind::Eggs, s, 0.0) {
                break;
            }
        }
    }
    // Coins on the inside of the three tightest corners.
    let mut corners = shaped.corners.clone();
    corners.sort_by(|a, b| a.radius.total_cmp(&b.radius).then(a.idx.cmp(&b.idx)));
    let inside = (0.28 * shaped.width).min(shaped.width * 0.5 - 3.0).max(0.0);
    for c in corners.iter().take(3) {
        let base = c.idx as f32 * sp - 0.5 * COIN_COUNT as f32 * COIN_SPACING;
        for off in [0.0f32, -10.0, 10.0, -20.0, 20.0, -30.0, 30.0] {
            if try_place(&mut out, CustomFeatureKind::Coins, base + off, -(c.inside_sign as f32) * inside) {
                break;
            }
        }
    }
    out.sort_by_key(|f| (f.s_dm, f.kind.code()));
    out
}

// ---------------------------------------------------------------------------
// CustomTrack <-> TrackDef
// ---------------------------------------------------------------------------

/// Builds the saved form of a shaped track, flat (`hills` 0; set the field
/// afterwards for hills). Points are quantised to decimetres
/// (lossless for [`shape`] output); the name is cut to 24 characters, `laps`
/// clamped to 1..=9 and `width_dm` to 140..=220 (an empty name becomes "My
/// Track"), and a `look` that is not one
/// of the eight race tracks becomes Savanna.
pub fn custom_track_from(
    shaped: &Shaped,
    name: impl Into<String>,
    look: TrackId,
    laps: u8,
    width_dm: u8,
    features: Vec<CustomFeature>,
    seed: u32,
) -> CustomTrack {
    let q = |v: f32| (v * 10.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    let mut name: String = name.into().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    if name.trim().is_empty() {
        name = "My Track".into();
    }
    let mut features = features;
    features.truncate(MAX_FEATURES);
    CustomTrack {
        v: 1,
        name,
        look: if TrackId::RACE_ALL.contains(&look) { look } else { TrackId::Savanna },
        laps: laps.clamp(1, 9),
        width_dm: width_dm.clamp(140, 220),
        points: shaped.points.iter().map(|p| [q(p.x), q(p.y)]).collect(),
        features,
        seed,
        hills: 0,
    }
}

/// Height of a bridge deck at `d` metres along the lap from its crown centre.
fn bridge_profile(d: f32) -> f32 {
    if d <= BRIDGE_CROWN {
        BRIDGE_HEIGHT
    } else if d < BRIDGE_REACH {
        BRIDGE_HEIGHT * 0.5 * (1.0 + crate::math::cos(std::f32::consts::PI * (d - BRIDGE_CROWN) / BRIDGE_RAMP))
    } else {
        0.0
    }
}

fn smoothstep(x: f32) -> f32 {
    x * x * (3.0 - 2.0 * x)
}

/// Hill height (m) at each shaped point (same indexing as `shaped.points`),
/// all zero when `track.hills` is 0. A sum of up to eight harmonics of the lap
/// (so it closes), seeded from the track seed and level, faded to flat around
/// the start line, the crossings and the gap jumps, then scaled so the
/// peak-to-peak height is at most the level's target and no grade between
/// neighbouring points exceeds its cap. Returns zeros if the lap is too flat
/// to scale.
pub fn hill_heights(track: &CustomTrack, shaped: &Shaped) -> Vec<f32> {
    let n = shaped.points.len();
    let zeros = vec![0.0f32; n];
    if track.hills == 0 || track.hills > HILLS_MAX || n < 3 {
        return zeros;
    }
    let (target, cap) = HILL_LEVELS[track.hills as usize - 1];
    let cum = cum_lengths(&shaped.points);
    let total = cum[n];
    let harmonics = ((total / 180.0).floor() as usize).clamp(1, 8);
    let mut rng = Pcg32::seed_from_u64(mix64(track.seed as u64 ^ 0x4849_4C4C_5300_0000 ^ track.hills as u64));
    let waves: Vec<(f32, f32)> = (1..=harmonics)
        .map(|k| {
            let amp = rng.gen_range(0.6f32..1.0) / (k as f32).powf(0.8);
            let phase = rng.gen_range(0.0f32..std::f32::consts::TAU);
            (amp, phase)
        })
        .collect();
    let mut pins: Vec<(f32, f32)> = vec![(0.0, 50.0)];
    for c in &shaped.crossings {
        pins.push((c.s_high, BRIDGE_REACH + 6.0));
        pins.push((c.s_low, BRIDGE_REACH + 6.0));
    }
    for f in track.features.iter().filter(|f| f.kind == CustomFeatureKind::Gap) {
        pins.push((f.s(), 30.0));
        pins.push(((f.s() - JUMP_RAMP.0).rem_euclid(total), 30.0));
    }
    let h: Vec<f32> = (0..n)
        .map(|k| {
            let s = cum[k];
            let raw: f32 = waves
                .iter()
                .enumerate()
                .map(|(i, &(amp, phase))| amp * crate::math::cos(std::f32::consts::TAU * (i + 1) as f32 * s / total + phase))
                .sum();
            let window: f32 =
                pins.iter().map(|&(ps, r)| smoothstep(((cyc_dist(s, ps, total) - r) / 40.0).clamp(0.0, 1.0))).product();
            raw * window
        })
        .collect();
    let hi = h.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let lo = h.iter().copied().fold(f32::INFINITY, f32::min);
    let p2p = hi - lo;
    if p2p < 1e-3 {
        return zeros;
    }
    let grade = (0..n)
        .map(|k| {
            let run = shaped.points[k].distance(shaped.points[(k + 1) % n]).max(1e-3);
            (h[(k + 1) % n] - h[k]).abs() / run
        })
        .fold(0.0f32, f32::max);
    let mut scale = target / p2p;
    if grade > 1e-6 {
        scale = scale.min(cap / grade);
    }
    h.iter().map(|v| v * scale).collect()
}

/// Converts a saved track into a [`TrackDef`] ready for [`install`].
///
/// Validates the data ([`CustomTrack::validate`], point spacing, geometry via
/// the same checks as [`shape`] with 5 % slack on the lap length, feature
/// ranges), so it is safe on a track received over the network.
///
/// * Heights are flat except at bridges and hills: at each crossing the later
///   branch (or the earlier one if the later would put the start line on the
///   ramp) rises to 7 m ([`BRIDGE_HEIGHT`]) over a flat crown of +-4 m and
///   cosine ramps of 80 m ([`BRIDGE_RAMP`]), a grade of at most 13.7 %. With
///   `hills` > 0 the [`hill_heights`] are added to every point; they are flat
///   at the start line, the crossings and gap jumps, so the bridge is unchanged.
/// * Theme, shoulders and scenery come from the `look` track; `id` is
///   [`TrackId::Custom`].
/// * Features: Eggs a TargetRow of 4; Ramp the Savanna kicker (12 m, 1.5 m, 1 m
///   lip); Gap the Canyon pair (a 13 m ramp 2 m high, then an 11 m gap at `s`);
///   Log a Hurdle; Boost a 6 m BoostPad; Coins a CoinLine of 6 at 3 m.
pub fn to_def(t: &CustomTrack) -> Result<TrackDef, CustomError> {
    let shaped = t.shaped()?;
    let total = shaped.length;
    let cum = cum_lengths(&shaped.points);
    let half = t.width() * 0.5;
    let mut features = Vec::with_capacity(t.features.len() + 1);
    for (i, f) in t.features.iter().enumerate() {
        let (s, lateral) = (f.s(), f.lateral());
        if !s.is_finite() || s < 0.0 || s >= total || lateral.abs() > half {
            return Err(CustomError::Feature(i));
        }
        match f.kind {
            CustomFeatureKind::Eggs => features.push(Feature::TargetRow { s, count: EGG_COUNT }),
            CustomFeatureKind::Ramp => {
                features.push(Feature::Ramp { s, len: KICKER.0, height: KICKER.1, lip: KICKER.2 })
            }
            CustomFeatureKind::Gap => {
                features.push(Feature::Ramp { s: (s - JUMP_RAMP.0).rem_euclid(total), len: JUMP_RAMP.0, height: JUMP_RAMP.1, lip: 0.0 });
                features.push(Feature::Gap { s, len: JUMP_GAP });
            }
            CustomFeatureKind::Log => features.push(Feature::Hurdle { s }),
            CustomFeatureKind::Boost => features.push(Feature::BoostPad { s, lateral: 0.0, len: PAD_LEN }),
            CustomFeatureKind::Coins => {
                features.push(Feature::CoinLine { s, lateral, count: COIN_COUNT, spacing: COIN_SPACING })
            }
        }
    }
    let look = t.look.def();
    let hills = hill_heights(t, &shaped);
    let points = shaped
        .points
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let h = shaped
                .crossings
                .iter()
                .map(|c| bridge_profile(cyc_dist(cum[k], c.s_high, total)))
                .fold(0.0f32, f32::max)
                + hills[k];
            TrackPoint { pos: Vec3::new(p.x, h, p.y), width: t.width(), bank: 0.0 }
        })
        .collect();
    Ok(TrackDef {
        id: TrackId::Custom,
        name: t.name.clone(),
        theme: look.theme,
        laps: t.laps,
        points,
        shoulder: look.shoulder,
        features,
        seed: mix64(look.seed ^ (t.seed as u64) ^ 0xC057_0000_0000_0000),
    })
}

// ---------------------------------------------------------------------------
// Recorder (run mode, spec 19.2)
// ---------------------------------------------------------------------------

/// Spacing of recorded points (m).
pub const RECORD_STEP: f32 = 2.0;
/// The run must be at least this long to close (m).
pub const RECORD_MIN: f32 = 300.0;
/// The runner must cross the start line within this distance of the start (m).
pub const RECORD_CLOSE_DIST: f32 = 12.0;
/// ... heading within this angle of the start direction (degrees).
pub const RECORD_CLOSE_ANGLE: f32 = 70.0;
/// Crossings of the start line count for [`RecordEvent`]s only once the run is this long (m),
/// so the first steps away from the start never do.
pub const RECORD_ARMED: f32 = 30.0;

/// Something the runner should be told, from [`Recorder::take_event`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RecordEvent {
    /// Crossed the start line backwards, near the start.
    WrongWay,
    /// Crossed it the right way before the run was long enough.
    Early {
        /// Metres still to run.
        remaining: f32,
    },
    /// Crossed it the right way after 300 m, near the start, but not heading through the gate
    /// (more than 70 degrees off): the loop does not close.
    Skewed,
}

/// State after a [`Recorder::push`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RecorderStatus {
    /// Still running; `len` metres recorded.
    Recording {
        /// Path length so far (m).
        len: f32,
    },
    /// The loop is closed: [`Recorder::points`] is the raw stroke for [`shape`].
    Closed,
    /// The run passed 3000 m.
    TooLong,
}

/// Records a run on the Paddock into a raw stroke (spec 19.2 / 19.4).
#[derive(Clone, Debug)]
pub struct Recorder {
    start: Vec2,
    dir: Vec2,
    pts: Vec<Vec2>,
    len: f32,
    status: RecorderStatus,
    event: Option<RecordEvent>,
}

impl Recorder {
    /// Starts a recording at `start` facing `dir` (any length; normalised).
    pub fn new(start: Vec2, dir: Vec2) -> Recorder {
        let dir = if dir.length_squared() > 1e-9 { dir.normalize() } else { Vec2::Y };
        Recorder { start, dir, pts: vec![start], len: 0.0, status: RecorderStatus::Recording { len: 0.0 }, event: None }
    }

    /// The recorded points so far (one every 2 m, the first at the start).
    pub fn points(&self) -> &[Vec2] {
        &self.pts
    }

    /// Path length so far (m).
    pub fn len(&self) -> f32 {
        self.len
    }

    /// Whether nothing beyond the start has been recorded.
    pub fn is_empty(&self) -> bool {
        self.pts.len() <= 1
    }

    /// The current status.
    pub fn status(&self) -> RecorderStatus {
        self.status
    }

    /// Where the run started: the finish gate.
    pub fn start(&self) -> Vec2 {
        self.start
    }

    /// The way the start faces (unit length): the gate is passed going this way.
    pub fn dir(&self) -> Vec2 {
        self.dir
    }

    /// The latest crossing of the start line that did not close the loop, once; `None` after.
    pub fn take_event(&mut self) -> Option<RecordEvent> {
        self.event.take()
    }

    /// Feeds the runner's position. A point is recorded every 2 m (a longer
    /// jump is filled in along the straight line). The loop closes once the
    /// path is at least 300 m, and the runner crosses the start line (going
    /// the way the start faces) within 12 m of the start, heading within 70
    /// degrees of the start direction. Over 3000 m it reports `TooLong`. After
    /// either, further pushes change nothing. Once the run is 30 m long, other
    /// crossings near the start leave a [`RecordEvent`] for [`Recorder::take_event`].
    pub fn push(&mut self, pos: Vec2) -> RecorderStatus {
        if !matches!(self.status, RecorderStatus::Recording { .. }) || !pos.is_finite() {
            return self.status;
        }
        let last = self.pts[self.pts.len() - 1];
        let d = pos - last;
        let dist = d.length();
        if dist > MAX_LAP {
            self.status = RecorderStatus::TooLong;
            return self.status;
        }
        let steps = (dist / RECORD_STEP).floor() as usize;
        for k in 1..=steps {
            let p = last + d / dist * (RECORD_STEP * k as f32);
            self.add(p);
            if !matches!(self.status, RecorderStatus::Recording { .. }) {
                return self.status;
            }
        }
        self.status
    }

    fn add(&mut self, p: Vec2) {
        let prev = self.pts[self.pts.len() - 1];
        self.pts.push(p);
        self.len += prev.distance(p);
        if self.len > MAX_LAP {
            self.status = RecorderStatus::TooLong;
            return;
        }
        let (s0, s1) = ((prev - self.start).dot(self.dir), (p - self.start).dot(self.dir));
        let forward = s0 < 0.0 && s1 >= 0.0;
        let backward = s0 >= 0.0 && s1 < 0.0;
        if forward || backward {
            let x = prev.lerp(p, -s0 / (s1 - s0));
            let near = x.distance(self.start) <= RECORD_CLOSE_DIST;
            // Heading over the last ~6 m.
            let back = self.pts[self.pts.len().saturating_sub(4)];
            let heading = (p - back).normalize_or_zero();
            let ahead = heading.dot(self.dir) >= crate::math::cos(RECORD_CLOSE_ANGLE.to_radians());
            if forward && near && ahead && self.len >= RECORD_MIN {
                self.status = RecorderStatus::Closed;
                return;
            }
            if near && self.len >= RECORD_ARMED {
                if backward {
                    self.event = Some(RecordEvent::WrongWay);
                } else if ahead {
                    self.event = Some(RecordEvent::Early { remaining: RECORD_MIN - self.len });
                } else if self.len >= RECORD_MIN {
                    self.event = Some(RecordEvent::Skewed);
                }
            }
        }
        self.status = RecorderStatus::Recording { len: self.len };
    }
}
