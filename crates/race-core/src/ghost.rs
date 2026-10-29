//! Time Trial ghosts (spec 17.3): record one racer every [`GHOST_EVERY`]
//! ticks, store it compactly, replay it as an interpolated pose.
//!
//! ```ignore
//! let mut rec = GhostRecorder::new(&world, 0);
//! loop { world.step(&inputs); rec.record(&world); /* until finished */ }
//! let ghost = rec.finish_world(&world).unwrap();       // the racer's time and laps
//! let bytes = ghost.to_bytes();                        // store per track
//! let g = Ghost::from_bytes(&bytes, track, &world.tun)?; // rejects other tracks / tunables
//! let pose = g.pose_at(world.race_time());             // draw translucent
//! ```
//!
//! On disk: `GHOST_MAGIC`, `GHOST_VERSION` (u16 LE), then bincode (varint) of
//! the header and the frames, positions in centimetres coded as second
//! differences (smooth motion costs ~1 byte per axis).

use bincode::Options;
use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::math::{wrap_angle, DT};
use crate::racer::Racer;
use crate::track::TrackId;
use crate::tunables::Tunables;
use crate::world::World;

/// File magic.
pub const GHOST_MAGIC: [u8; 4] = *b"STGH";
/// File format version; other versions are rejected.
pub const GHOST_VERSION: u16 = 4;
/// Ticks between samples (20 Hz).
pub const GHOST_EVERY: u64 = 3;
/// Decode guard.
const GHOST_MAX_BYTES: u64 = 4 << 20;

/// An interpolated ghost pose for the renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GhostPose {
    /// Feet position.
    pub pos: Vec3,
    /// Heading.
    pub yaw: f32,
    /// Body yaw (heading + drift angle).
    pub visual_yaw: f32,
    /// Run cycle phase 0..1.
    pub gait_phase: f32,
    /// Speed along the heading (m/s, 0.5 m/s steps).
    pub speed: f32,
    /// In the air.
    pub airborne: bool,
    /// Drifting.
    pub drifting: bool,
    /// Boosting.
    pub boosting: bool,
    /// Doing a ramp trick.
    pub trick: bool,
    /// Spun out.
    pub spun: bool,
}

/// One quantised sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhostFrame {
    /// Feet position, centimetres.
    pub pos: [i32; 3],
    /// Heading, 65536 per turn (0 = yaw 0).
    pub yaw: u16,
    /// Visual yaw offset from the heading, centiradians.
    pub vis: i8,
    /// Gait phase x 256.
    pub gait: u8,
    /// Speed, 0.5 m/s units.
    pub speed: i8,
    /// Bits: 0 airborne, 1 drifting, 2 boosting, 3 trick, 4 spun out.
    pub flags: u8,
}

const TURN: f32 = std::f32::consts::TAU;

impl GhostFrame {
    /// Quantises a racer.
    pub fn of(r: &Racer) -> GhostFrame {
        let cm = |v: f32| (v * 100.0).round().clamp(-2.0e9, 2.0e9) as i32;
        let yaw = (wrap_angle(r.yaw).rem_euclid(TURN) / TURN * 65536.0).round() as u32 % 65536;
        let mut flags = 0u8;
        for (k, b) in [
            !r.on_ground,
            r.drift.active,
            r.boost_t > 0.0,
            r.trick_t > 0.0,
            r.stun_t > 0.0,
        ]
        .into_iter()
        .enumerate()
        {
            if b {
                flags |= 1 << k;
            }
        }
        GhostFrame {
            pos: [cm(r.pos.x), cm(r.pos.y), cm(r.pos.z)],
            yaw: yaw as u16,
            vis: (wrap_angle(r.visual_yaw - r.yaw) * 100.0)
                .round()
                .clamp(-127.0, 127.0) as i8,
            gait: (r.gait_phase.rem_euclid(1.0) * 256.0).floor().clamp(0.0, 255.0) as u8,
            speed: (r.speed * 2.0).round().clamp(-128.0, 127.0) as i8,
            flags,
        }
    }

    /// The frame as a pose (no interpolation).
    pub fn pose(&self) -> GhostPose {
        let yaw = wrap_angle(self.yaw as f32 / 65536.0 * TURN);
        GhostPose {
            pos: Vec3::new(
                self.pos[0] as f32,
                self.pos[1] as f32,
                self.pos[2] as f32,
            ) / 100.0,
            yaw,
            visual_yaw: wrap_angle(yaw + self.vis as f32 / 100.0),
            gait_phase: self.gait as f32 / 256.0,
            speed: self.speed as f32 * 0.5,
            airborne: self.flags & 1 != 0,
            drifting: self.flags & 2 != 0,
            boosting: self.flags & 4 != 0,
            trick: self.flags & 8 != 0,
            spun: self.flags & 16 != 0,
        }
    }
}

/// Why a ghost file was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GhostError {
    /// Not a ghost file.
    BadMagic,
    /// Another format version.
    Version(u16),
    /// Truncated or garbage.
    Corrupt,
    /// Recorded on another track.
    WrongTrack,
    /// The track's geometry changed since it was recorded.
    TrackChanged,
    /// The tunables changed since it was recorded (the run is not comparable).
    TunablesChanged,
}

impl std::fmt::Display for GhostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for GhostError {}

/// A recorded run: one sample every [`GHOST_EVERY`] ticks from GO (frame `k`
/// is the racer at race time `k x GHOST_EVERY / 60` s).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ghost {
    /// [`GHOST_VERSION`] it was made with.
    pub version: u16,
    /// Track.
    pub track: TrackId,
    /// [`track_hash`] of the track when recorded.
    pub track_hash: u64,
    /// [`tunables_hash`] of the tunables when recorded.
    pub tunables_hash: u64,
    /// Character id (for drawing).
    pub character: u8,
    /// The run's time (finish time; or the recorded length).
    pub time: f32,
    /// Lap times.
    pub lap_times: Vec<f32>,
    /// Samples from GO.
    pub frames: Vec<GhostFrame>,
}

#[derive(Serialize, Deserialize)]
struct Packed {
    dd: [i32; 3],
    dyaw: i32,
    vis: i8,
    gait: u8,
    speed: i8,
    flags: u8,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    track: TrackId,
    track_hash: u64,
    tunables_hash: u64,
    character: u8,
    time: f32,
    lap_times: Vec<f32>,
    first: Option<GhostFrame>,
    rest: Vec<Packed>,
}

fn opts() -> impl Options {
    bincode::DefaultOptions::new().with_limit(GHOST_MAX_BYTES)
}

impl Ghost {
    /// Seconds between frames.
    pub const INTERVAL: f32 = GHOST_EVERY as f32 * DT;

    /// Recorded length in seconds (time of the last frame).
    pub fn duration(&self) -> f32 {
        self.frames.len().saturating_sub(1) as f32 * Ghost::INTERVAL
    }

    /// The pose at race time `t` (seconds since GO), interpolated between
    /// frames (position linearly, yaws along the shorter arc, gait forward);
    /// held at the ends.
    pub fn pose_at(&self, t: f32) -> GhostPose {
        let Some(first) = self.frames.first() else {
            return GhostPose::default();
        };
        let x = (t / Ghost::INTERVAL).max(0.0);
        let i = x.floor() as usize;
        if !x.is_finite() || i.saturating_add(1) >= self.frames.len() {
            return self.frames.last().unwrap_or(first).pose();
        }
        let f = if i + 2 == self.frames.len() && self.time < self.duration() {
            ((t - i as f32 * Ghost::INTERVAL) / (self.time - i as f32 * Ghost::INTERVAL).max(1e-6)).clamp(0.0, 1.0)
        } else { x - i as f32 };
        let (a, b) = (self.frames[i].pose(), self.frames[i + 1].pose());
        let yaw = wrap_angle(a.yaw + wrap_angle(b.yaw - a.yaw) * f);
        let vis_a = wrap_angle(a.visual_yaw - a.yaw);
        let vis_b = wrap_angle(b.visual_yaw - b.yaw);
        let dg = b.gait_phase - a.gait_phase;
        let dg = if a.speed >= 0.0 {
            dg.rem_euclid(1.0)
        } else {
            -(-dg).rem_euclid(1.0)
        };
        let near = if f < 0.5 { a } else { b };
        GhostPose {
            pos: a.pos.lerp(b.pos, f),
            yaw,
            visual_yaw: wrap_angle(yaw + vis_a + (vis_b - vis_a) * f),
            gait_phase: (a.gait_phase + dg * f).rem_euclid(1.0),
            speed: a.speed + (b.speed - a.speed) * f,
            ..near
        }
    }

    /// Serialises: magic, version, then bincode (varint) with positions and
    /// yaw as second differences.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut rest = Vec::with_capacity(self.frames.len().saturating_sub(1));
        let mut prev_d = [0i32; 3];
        let mut prev_dy = 0i32;
        for w in self.frames.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            let mut dd = [0i32; 3];
            for k in 0..3 {
                let d = b.pos[k].wrapping_sub(a.pos[k]);
                dd[k] = d.wrapping_sub(prev_d[k]);
                prev_d[k] = d;
            }
            let dy = b.yaw.wrapping_sub(a.yaw) as i16 as i32;
            rest.push(Packed {
                dd,
                dyaw: dy - prev_dy,
                vis: b.vis,
                gait: b.gait,
                speed: b.speed,
                flags: b.flags,
            });
            prev_dy = dy;
        }
        let wire = Wire {
            track: self.track,
            track_hash: self.track_hash,
            tunables_hash: self.tunables_hash,
            character: self.character,
            time: self.time,
            lap_times: self.lap_times.clone(),
            first: self.frames.first().copied(),
            rest,
        };
        let mut out = Vec::with_capacity(16 + self.frames.len() * 9);
        out.extend_from_slice(&GHOST_MAGIC);
        out.extend_from_slice(&GHOST_VERSION.to_le_bytes());
        out.extend(opts().serialize(&wire).unwrap_or_default());
        out
    }

    /// Parses a ghost without checking what it was recorded on.
    pub fn decode(bytes: &[u8]) -> Result<Ghost, GhostError> {
        if bytes.len() < 6 || bytes[..4] != GHOST_MAGIC {
            return Err(GhostError::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != GHOST_VERSION {
            return Err(GhostError::Version(version));
        }
        let wire: Wire = opts()
            .deserialize(&bytes[6..])
            .map_err(|_| GhostError::Corrupt)?;
        let mut frames = Vec::with_capacity(wire.rest.len() + 1);
        if let Some(first) = wire.first {
            frames.push(first);
            let mut cur = first;
            let mut d = [0i32; 3];
            let mut dy = 0i32;
            for p in &wire.rest {
                for k in 0..3 {
                    d[k] = d[k].wrapping_add(p.dd[k]);
                    cur.pos[k] = cur.pos[k].wrapping_add(d[k]);
                }
                dy = dy.wrapping_add(p.dyaw);
                cur.yaw = cur.yaw.wrapping_add(dy as i16 as u16);
                cur.vis = p.vis;
                cur.gait = p.gait;
                cur.speed = p.speed;
                cur.flags = p.flags;
                frames.push(cur);
            }
        } else if !wire.rest.is_empty() {
            return Err(GhostError::Corrupt);
        }
        Ok(Ghost {
            version,
            track: wire.track,
            track_hash: wire.track_hash,
            tunables_hash: wire.tunables_hash,
            character: wire.character,
            time: wire.time,
            lap_times: wire.lap_times,
            frames,
        })
    }

    /// Parses a ghost for `track` under `tun`: rejects another magic or
    /// version, garbage, another track, or a changed track / tunables.
    pub fn from_bytes(bytes: &[u8], track: TrackId, tun: &Tunables) -> Result<Ghost, GhostError> {
        let g = Ghost::decode(bytes)?;
        g.check(track, tun)?;
        Ok(g)
    }

    /// Whether this ghost is valid for `track` under `tun`.
    pub fn check(&self, track: TrackId, tun: &Tunables) -> Result<(), GhostError> {
        if self.track != track {
            return Err(GhostError::WrongTrack);
        }
        if self.track_hash != track_hash(track) {
            return Err(GhostError::TrackChanged);
        }
        if self.tunables_hash != tunables_hash(tun) {
            return Err(GhostError::TunablesChanged);
        }
        Ok(())
    }
}

/// Records one racer of a world as a [`Ghost`].
#[derive(Clone, Debug)]
pub struct GhostRecorder {
    racer: usize,
    track: TrackId,
    track_hash: u64,
    tunables_hash: u64,
    character: u8,
    frames: Vec<GhostFrame>,
}

impl GhostRecorder {
    /// Starts recording racer `racer` of `world` (before or at GO).
    pub fn new(world: &World, racer: usize) -> GhostRecorder {
        GhostRecorder {
            racer,
            track: world.cfg.track,
            track_hash: track_hash(world.cfg.track),
            tunables_hash: tunables_hash(&world.tun),
            character: world.racers.get(racer).map_or(0, |r| r.character),
            frames: Vec::new(),
        }
    }

    /// Call after every `World::step`: from GO on it appends a frame every
    /// [`GHOST_EVERY`] ticks (if calls were skipped, the current state fills
    /// the missed frames).
    pub fn record(&mut self, world: &World) {
        if world.tick < world.go_tick {
            return;
        }
        let Some(r) = world.racers.get(self.racer) else {
            return;
        };
        let k = ((world.tick - world.go_tick) / GHOST_EVERY) as usize;
        let f = GhostFrame::of(r);
        while self.frames.len() <= k {
            self.frames.push(f);
        }
    }

    /// Appends a frame of `r` now (for callers that keep their own clock: one
    /// call per [`Ghost::INTERVAL`], the first at GO).
    pub fn push(&mut self, r: &Racer) {
        self.frames.push(GhostFrame::of(r));
    }

    /// Frames so far.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// No frames yet.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The ghost of a run of `time` seconds (frames past it are dropped).
    pub fn finish(mut self, time: f32, lap_times: Vec<f32>) -> Ghost {
        let keep = (time / Ghost::INTERVAL).ceil().max(0.0) as usize + 1;
        self.frames.truncate(keep);
        Ghost {
            version: GHOST_VERSION,
            track: self.track,
            track_hash: self.track_hash,
            tunables_hash: self.tunables_hash,
            character: self.character,
            time,
            lap_times,
            frames: self.frames,
        }
    }

    /// The ghost of the recorded racer's finished run (`None` if it has not
    /// finished).
    pub fn finish_world(mut self, world: &World) -> Option<Ghost> {
        let r = world.racers.get(self.racer)?;
        let time = r.finished_at?;
        let laps = world.lap_times.get(self.racer).cloned().unwrap_or_default();
        let last = (time / Ghost::INTERVAL).ceil().max(0.0) as usize;
        while self.frames.len() <= last { self.frames.push(GhostFrame::of(r)); }
        if let Some(f) = self.frames.get_mut(last) { *f = GhostFrame::of(r); }
        Some(self.finish(time, laps))
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    crate::math::mix64(h)
}

/// Hash of a track's geometry (centreline, widths, shoulders, features; not
/// its look): a ghost recorded on another layout is rejected.
pub fn track_hash(id: TrackId) -> u64 {
    let d = id.def();
    let bytes = bincode::serialize(&(&d.points, &d.shoulder, &d.features)).unwrap_or_default();
    fnv(&bytes)
}

/// Hash of every tunable: a ghost recorded under other physics is rejected.
pub fn tunables_hash(t: &Tunables) -> u64 {
    fnv(&bincode::serialize(t).unwrap_or_default())
}
