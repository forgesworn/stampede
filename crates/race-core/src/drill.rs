//! Open-ground training drills (spec 18.2).
//!
//! A drill is a pure, seeded state machine. [`DrillState::step`] takes a
//! [`RunnerView`] (what the drill needs to know about the one human runner)
//! and returns [`DrillEvent`]s. It owns a *course line* that it steers itself,
//! and builds the current *rep* and the next one ahead of the runner along it
//! (only those two exist). The line bends back toward the field centre
//! whenever the next rep would sit too far out, and away from the centre
//! island when it gets too close, so props always stay in
//! `FIELD_MIN_R..=FIELD_MAX_R` of the origin.
//!
//! `World` hosts a drill: it builds the view from racer 0 and its events,
//! calls `step`, and applies the effects back (a stumble on a log hit, the
//! Laser Range marker, the Bash Buddy dummy's pose and speed).

use glam::{Vec2, Vec3};
use rand::Rng;
use rand_pcg::Pcg32;
use serde::{Deserialize, Serialize};

use crate::lesson::HintControl;
use crate::math::{mix64, wrap_angle};
use crate::track::{HURDLE_HEIGHT, TARGET_HEIGHT};

/// Props never spawn closer to the origin than this (the centre island).
pub const FIELD_MIN_R: f32 = 40.0;
/// Props never spawn farther from the origin than this.
pub const FIELD_MAX_R: f32 = 230.0;
/// Standing height of a slalom cone (big, so it reads from far away).
pub const CONE_HEIGHT: f32 = 2.6;
/// Collision radius of a slalom cone.
pub const CONE_RADIUS: f32 = 0.6;
/// Collision radius of a (small) Drift Ring cone.
pub const RING_CONE_RADIUS: f32 = 0.35;
/// Radius of the Drift Ring pylon.
pub const PYLON_RADIUS: f32 = 0.8;
/// Half the length of a log (it lies across the path).
pub const LOG_HALF_LENGTH: f32 = 6.0;
/// Radius (thickness) of a log.
pub const LOG_RADIUS: f32 = 0.4;
/// The runner's collision radius against props.
pub const RUNNER_RADIUS: f32 = 0.9;
/// Strikes that end a run.
pub const MAX_STRIKES: u8 = 3;
/// Clean reps that raise the level.
pub const REPS_PER_LEVEL: u8 = 4;
/// The last level; clearing it earns Master.
pub const MAX_LEVEL: u8 = 10;
/// Drift Ring: heading change (degrees) needed round the pylon.
pub const RING_SWEEP_DEG: f32 = 300.0;
/// Drift Ring: the runner must stay within `radius` to `radius + RING_BAND`.
pub const RING_BAND: f32 = 10.0;
/// Slalom / Hurdles / Drift Ring approach timeout per rep (s).
pub const REP_TIMEOUT: f32 = 10.0;
/// Drift Ring: time allowed to reach the ring before its own clock starts (s).
pub const RING_APPROACH_TIMEOUT: f32 = 15.0;
/// Hurdles doubles: gap between the two logs (m).
pub const DOUBLE_GAP: f32 = 6.0;
/// How long a failed rep's knocked props stay visible after the rep is replaced (s).
pub const DEBRIS_TIME: f32 = 1.5;
/// Seconds an egg waits after the previous rep before it pops up.
pub const EGG_POP_DELAY: f32 = 0.5;

// The course line itself stays inside this band, so the props around it
// (at most ~16 m off the line) stay inside FIELD_MIN_R..FIELD_MAX_R.
const LINE_MIN_R: f32 = 58.0;
const LINE_MAX_R: f32 = 212.0;
const DUMMY_MIN_R: f32 = 70.0;
const DUMMY_MAX_R: f32 = 195.0;
const FIRST_DISTANCE: f32 = 40.0;
const RING_SPACING: f32 = 60.0;
const BUDDY_AHEAD: f32 = 7.0;
const BUDDY_SIDE: f32 = 4.0;
const LINE_WANDER: f32 = 0.22;
const KNOCK_RATE: f32 = 3.0;
const BASH_LOOKAHEAD: f32 = 12.0;
const DODGE_ANGLE: f32 = 0.6;

/// The five drills, in menu order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrillId {
    /// Steer between pairs of tall cones.
    Slalom,
    /// Circle a pylon in a drift and release a mini-turbo.
    DriftRing,
    /// Jump log hurdles.
    Hurdles,
    /// Line up and hit opponent dummies with the forward laser.
    LaserRange,
    /// Bash a dummy ostrich running beside you.
    BashBuddy,
}

impl DrillId {
    /// All drills in menu order.
    pub const ALL: [DrillId; 5] = [
        DrillId::Slalom,
        DrillId::DriftRing,
        DrillId::Hurdles,
        DrillId::LaserRange,
        DrillId::BashBuddy,
    ];

    /// Title.
    pub fn title(self) -> &'static str {
        match self {
            DrillId::Slalom => "Slalom",
            DrillId::DriftRing => "Drift Ring",
            DrillId::Hurdles => "Hurdles",
            DrillId::LaserRange => "Laser Range",
            DrillId::BashBuddy => "Bash Buddy",
        }
    }

    /// One-line instruction.
    pub fn instruction(self) -> &'static str {
        match self {
            DrillId::Slalom => "Steer between the two cones of every gate. Don't clip one.",
            DrillId::DriftRing => "Drift once round the pylon, outside the cones, then let go.",
            DrillId::Hurdles => "Jump every log. Tripping on one is a strike.",
            DrillId::LaserRange => "Hit each dummy with your forward laser before it vanishes.",
            DrillId::BashBuddy => "Pull alongside the dummy and bash it before time runs out.",
        }
    }

    /// Short hint shown next to the drawn control.
    pub fn hint(self) -> &'static str {
        match self {
            DrillId::Slalom => "Drag the left pad to weave",
            DrillId::DriftRing => "Steer + hold Drift round the ring, release when the sparks glow",
            DrillId::Hurdles => "Tap Jump just before a log",
            DrillId::LaserRange => "Line up the dummy and hold Laser",
            DrillId::BashBuddy => "Tap Bash or flick toward the dummy",
        }
    }

    /// The control the hint draws.
    pub fn control(self) -> HintControl {
        match self {
            DrillId::Slalom => HintControl::Steer,
            DrillId::DriftRing => HintControl::Drift,
            DrillId::Hurdles => HintControl::Jump,
            DrillId::LaserRange => HintControl::Laser,
            DrillId::BashBuddy => HintControl::Bash,
        }
    }
}

fn lerpl(level: u8, at1: f32, at10: f32) -> f32 {
    let t = (level.clamp(1, MAX_LEVEL) - 1) as f32 / (MAX_LEVEL - 1) as f32;
    at1 + (at10 - at1) * t
}

/// Every difficulty number of a level, interpolated L1 to L10 (spec 18.2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Difficulty {
    /// Level (1..=10).
    pub level: u8,
    /// Slalom: clear gap between the two cones of a gate (m), 9 to 4.5.
    pub gate_width: f32,
    /// Slalom: sideways offset of each gate from the line (m), 3 to 5.5.
    pub side_offset: f32,
    /// Slalom: distance between gates (m), 32 to 22.
    pub gate_spacing: f32,
    /// Drift Ring: ring radius (m), 14 to 6.
    pub ring_radius: f32,
    /// Drift Ring: time per ring once in it (s), 12 to 7.
    pub ring_time: f32,
    /// Drift Ring: minimum mini-turbo tier on release (1 any, 2 orange, 3 top).
    pub ring_tier: u8,
    /// Hurdles: distance between rep logs (m), 30 to 14.
    pub hurdle_spacing: f32,
    /// Hurdles: chance a rep is a double (0 below L5).
    pub double_chance: f32,
    /// Laser Range: dummy angle off the heading (degrees, either side), 10 to 40.
    pub egg_angle_deg: f32,
    /// Laser Range: dummy lifetime (s), 5 to 1.8.
    pub egg_life: f32,
    /// Bash Buddy: dummy speed as a fraction of top speed, 0.6 to 0.95.
    pub buddy_speed: f32,
    /// Bash Buddy: time to land the bash (s), 10 to 5.
    pub buddy_time: f32,
    /// Bash Buddy: whether the dummy dodges sideways (L4+).
    pub dodge: bool,
    /// Bash Buddy: base period of the dodge (s).
    pub dodge_period: f32,
}

impl Difficulty {
    /// The numbers at `level` (clamped to 1..=10).
    pub fn at(level: u8) -> Difficulty {
        let level = level.clamp(1, MAX_LEVEL);
        Difficulty {
            level,
            gate_width: lerpl(level, 9.0, 4.5),
            side_offset: lerpl(level, 3.0, 5.5),
            gate_spacing: lerpl(level, 32.0, 22.0),
            ring_radius: lerpl(level, 14.0, 6.0),
            ring_time: lerpl(level, 12.0, 7.0),
            ring_tier: match level {
                1..=3 => 1,
                4..=6 => 2,
                _ => 3,
            },
            hurdle_spacing: lerpl(level, 30.0, 14.0),
            double_chance: if level >= 5 {
                0.25 + 0.06 * (level - 5) as f32
            } else {
                0.0
            },
            egg_angle_deg: lerpl(level, 10.0, 40.0),
            egg_life: lerpl(level, 5.0, 1.8),
            buddy_speed: lerpl(level, 0.6, 0.95),
            buddy_time: lerpl(level, 10.0, 5.0),
            dodge: level >= 4,
            dodge_period: lerpl(level, 2.2, 0.8),
        }
    }
}

/// What a prop is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PropKind {
    /// Big slalom cone (`CONE_HEIGHT` tall).
    Cone,
    /// The Drift Ring's central pylon.
    Pylon,
    /// A log lying across the path (long axis along the prop's right vector).
    Log,
    /// A small cone in the Drift Ring.
    RingCone,
    /// A marker flag (reserved for the renderer's rep markers).
    Flag,
}

/// A world-positioned drill prop.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DrillProp {
    /// What it is.
    pub kind: PropKind,
    /// World position (base of the prop).
    pub pos: Vec3,
    /// Heading. For a knocked cone, the direction it fell.
    pub yaw: f32,
    /// 0 standing, rising to 1 as it falls; stays down until its rep is removed.
    pub knocked: f32,
}

/// Something the HUD, audio or the app should react to.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum DrillEvent {
    /// A rep was cleaned.
    RepClean,
    /// A miss, a knocked cone or a timeout; the rep is replaced.
    Strike,
    /// Reached this level (2..=10).
    LevelUp(u8),
    /// Earned this star (1..=3).
    Star(u8),
    /// Cleared level 10: the gold Master medal.
    Master,
    /// The run is over (three strikes, or Master).
    Over,
}

/// The dummy as the drill sees it (Bash Buddy).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DummyView {
    /// Position.
    pub pos: Vec3,
    /// Velocity.
    pub vel: Vec3,
    /// Heading.
    pub yaw: f32,
}

/// What a drill needs to know about the runner this tick.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunnerView {
    /// Position.
    pub pos: Vec3,
    /// Velocity.
    pub vel: Vec3,
    /// Heading.
    pub yaw: f32,
    /// On the ground.
    pub grounded: bool,
    /// Height above the ground (0 on it).
    pub air_h: f32,
    /// Drifting (sliding and charging).
    pub drifting: bool,
    /// Current spark tier of the drift (0 none, 1 blue, 2 orange, 3 purple).
    pub spark_tier: u8,
    /// A mini-turbo was released this tick, with this tier.
    pub boost_released: Option<u8>,
    /// The forward laser hit the drill dummy this tick (legacy field name).
    pub egg_destroyed: bool,
    /// A bash landed on the dummy this tick.
    pub bash_hit: bool,
    /// The Bash Buddy dummy, if there is one.
    pub dummy: Option<DummyView>,
}

impl RunnerView {
    /// A standing runner at `pos` facing `yaw` (tests, initial placement).
    pub fn at(pos: Vec3, yaw: f32) -> RunnerView {
        RunnerView {
            pos,
            vel: Vec3::ZERO,
            yaw,
            grounded: true,
            air_h: 0.0,
            drifting: false,
            spark_tier: 0,
            boost_released: None,
            egg_destroyed: false,
            bash_hit: false,
            dummy: None,
        }
    }
}

/// Laser Range dummy marker (legacy name); World places a hittable ostrich here.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DrillEgg {
    /// Position (at target height).
    pub pos: Vec3,
    /// Seconds of life left.
    pub life: f32,
    /// Lifetime at spawn.
    pub life_max: f32,
}

/// Bash Buddy: what the dummy should do. Consumers (World, test drivers) steer
/// the dummy toward `goal` at `speed_frac` of top speed, and teleport it when
/// `respawn` is set (read it after [`DrillState::new`] and after every step).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DummyCmd {
    /// A point ahead of the dummy to steer at.
    pub goal: Vec3,
    /// Target speed as a fraction of top speed.
    pub speed_frac: f32,
    /// Teleport the dummy here (position, heading) for a fresh rep.
    pub respawn: Option<(Vec3, f32)>,
}

/// Side effects of the last [`DrillState::step`] that are not events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DrillFx {
    /// The runner hit a log: stumble it (the existing hurdle stumble).
    pub stumble: bool,
    /// A new egg popped up this step (re-arm the target).
    pub egg_spawned: bool,
}

/// A read-only description of the current rep (HUD arrows, test drivers).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RepInfo {
    /// Slalom: pass between the cones.
    Gate {
        /// Gate centre.
        centre: Vec3,
        /// Direction of travel through the gate.
        yaw: f32,
        /// Half the clear gap.
        half_width: f32,
    },
    /// Drift Ring.
    Ring {
        /// Pylon position.
        centre: Vec3,
        /// Ring radius.
        radius: f32,
        /// Required mini-turbo tier on release.
        need_tier: u8,
        /// The runner has entered the ring band (the ring clock is running).
        entered: bool,
    },
    /// Hurdles: logs still to clear, nearest first.
    Logs {
        /// Direction of travel across the logs.
        yaw: f32,
        /// Log centres still to clear.
        remaining: Vec<Vec3>,
    },
    /// Laser Range: the dummy position, once it has appeared (legacy variant name).
    Egg {
        /// Egg position.
        pos: Option<Vec3>,
    },
    /// Bash Buddy.
    Buddy,
}

// ---------------------------------------------------------------------------
// 2D helpers (x, z plane; Vec2 is (x, z))

fn dir2(yaw: f32) -> Vec2 {
    Vec2::new(crate::math::sin(yaw), crate::math::cos(yaw))
}
fn right2(yaw: f32) -> Vec2 {
    Vec2::new(-crate::math::cos(yaw), crate::math::sin(yaw))
}
fn yaw2(v: Vec2) -> f32 {
    crate::math::atan2(v.x,v.y)
}
fn flat(v: Vec3) -> Vec2 {
    Vec2::new(v.x, v.z)
}
fn lift(v: Vec2, y: f32) -> Vec3 {
    Vec3::new(v.x, y, v.y)
}

fn rand_sign(rng: &mut Pcg32) -> f32 {
    if rng.gen::<bool>() {
        1.0
    } else {
        -1.0
    }
}

/// Moves `p` radially into `lo..=hi` of `origin`.
fn clamp_radius(p: Vec2, origin: Vec2, lo: f32, hi: f32, fallback: Vec2) -> Vec2 {
    let rel = p - origin;
    let r = rel.length();
    if r < 1e-3 {
        return origin + fallback.normalize_or_zero() * lo;
    }
    origin + rel / r * r.clamp(lo, hi)
}

/// Turns `yaw` (at most `step`) back toward the band `lo..=hi` round `origin`
/// when the point `at` is outside it: inward when too far, outward when too
/// close, with a sideways bias so the path sweeps round the island instead of
/// straight through it.
fn bend(yaw: f32, at: Vec2, origin: Vec2, lo: f32, hi: f32, step: f32) -> f32 {
    let rel = at - origin;
    let r = rel.length();
    if r <= hi && r >= lo {
        return yaw;
    }
    let radial = if r < 1e-3 { dir2(yaw) } else { rel / r };
    let want = if r > hi { yaw2(-radial) } else { yaw2(radial) };
    let diff = wrap_angle(want - yaw);
    let target = want - diff.signum() * 0.6;
    yaw + wrap_angle(target - yaw).clamp(-step, step)
}

/// Distance from `p` to the segment `a..b`.
fn seg_dist(a: Vec2, b: Vec2, p: Vec2) -> f32 {
    let ab = b - a;
    let l2 = ab.length_squared();
    let t = if l2 < 1e-9 {
        0.0
    } else {
        ((p - a).dot(ab) / l2).clamp(0.0, 1.0)
    };
    (a + ab * t - p).length()
}

// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Line {
    pos: Vec2,
    yaw: f32,
}

impl Line {
    /// Moves the line `dist` along its (wandering) heading, bending it back
    /// into the band `LINE_MIN_R..=LINE_MAX_R` when the next point would leave it.
    fn advance(&mut self, dist: f32, origin: Vec2, rng: &mut Pcg32) {
        let mut yaw = self.yaw + rng.gen_range(-LINE_WANDER..LINE_WANDER);
        let mut p = self.pos + dir2(yaw) * dist;
        for _ in 0..12 {
            let r = (p - origin).length();
            if (LINE_MIN_R..=LINE_MAX_R).contains(&r) {
                break;
            }
            yaw = bend(yaw, p, origin, LINE_MIN_R, LINE_MAX_R, 0.35);
            p = self.pos + dir2(yaw) * dist;
        }
        // Hard guarantee.
        let q = clamp_radius(p, origin, LINE_MIN_R, LINE_MAX_R, dir2(yaw));
        if (q - p).length() > 1e-4 && (q - self.pos).length() > 0.1 {
            yaw = yaw2(q - self.pos);
        }
        self.pos = q;
        self.yaw = yaw;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum RepKind {
    Gate {
        centre: Vec3,
        yaw: f32,
        half_w: f32,
    },
    Ring {
        centre: Vec3,
        radius: f32,
        swept: f32,
        last_ang: Option<f32>,
        entered: bool,
    },
    Logs {
        yaw: f32,
        centres: Vec<Vec3>,
        done: usize,
    },
    Egg {
        pop_in: f32,
    },
    Buddy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Rep {
    props: Vec<DrillProp>,
    kind: RepKind,
    limit: Option<f32>,
}

#[derive(Clone, Copy, PartialEq)]
enum Res {
    Pending,
    Clean,
    Strike,
}

/// A drill run in progress (`World::drill`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DrillState {
    /// Which drill.
    pub id: DrillId,
    /// Current level (1..=10).
    pub level: u8,
    /// Clean reps cleared in this level (0..4).
    pub reps_in_level: u8,
    /// Strikes so far (3 end the run).
    pub strikes: u8,
    /// Stars earned (0..=3).
    pub stars: u8,
    /// Cleared level 10.
    pub master: bool,
    /// The run is over; `step` does nothing more.
    pub over: bool,
    /// Highest level reached this run.
    pub best_level: u8,
    /// Clean reps in the whole run.
    pub clean_total: u32,
    /// Seconds stepped.
    pub elapsed: f32,
    /// Seconds into the current rep (a Drift Ring restarts it on entering the ring).
    pub rep_t: f32,
    /// Time limit of the current rep, if it has one.
    pub rep_limit: Option<f32>,
    /// Drift Ring: heading change swept round the pylon so far (degrees, absolute).
    pub ring_swept_deg: f32,
    /// Drift Ring: swept enough and the sparks are at the needed tier: let go now.
    pub ring_ready: bool,
    /// Laser Range: the live dummy marker (legacy field name).
    pub egg: Option<DrillEgg>,
    /// Bash Buddy: what the dummy should do.
    pub dummy: Option<DummyCmd>,
    /// Side effects of the last step.
    pub fx: DrillFx,
    origin: Vec2,
    line: Line,
    cur: Option<Rep>,
    next: Option<Rep>,
    debris: Vec<DrillProp>,
    debris_t: f32,
    prev: Option<Vec3>,
    ground_y: f32,
    side: f32,
    dummy_yaw: f32,
    dodge_t: f32,
    dodge_dir: f32,
}

impl DrillState {
    /// A fresh run centred on `origin` (x, z). `start`/`yaw` is where the
    /// runner stands; the first rep goes up ahead of it.
    pub fn new(id: DrillId, origin: Vec2, start: Vec3, yaw: f32, seed: u64) -> DrillState {
        let mut rng = Pcg32::new(mix64(seed ^ 0xD211_1000), 0x5EED_D211_0A02_BDBF);
        let mut s = DrillState {
            id,
            level: 1,
            reps_in_level: 0,
            strikes: 0,
            stars: 0,
            master: false,
            over: false,
            best_level: 1,
            clean_total: 0,
            elapsed: 0.0,
            rep_t: 0.0,
            rep_limit: None,
            ring_swept_deg: 0.0,
            ring_ready: false,
            egg: None,
            dummy: None,
            fx: DrillFx::default(),
            origin,
            line: Line {
                pos: flat(start),
                yaw,
            },
            cur: None,
            next: None,
            debris: Vec::new(),
            debris_t: 0.0,
            prev: None,
            ground_y: start.y,
            side: 1.0,
            dummy_yaw: yaw,
            dodge_t: 0.0,
            dodge_dir: 0.0,
        };
        s.side = rand_sign(&mut rng);
        s.start_reps(&RunnerView::at(start, yaw), &mut rng);
        s
    }

    /// The field centre this drill is built round.
    pub fn origin(&self) -> Vec2 {
        self.origin
    }

    /// All live props: the current rep, the next, and (briefly) the knocked
    /// props of a replaced rep.
    pub fn props(&self) -> Vec<DrillProp> {
        let mut v = Vec::new();
        for r in self.cur.iter().chain(self.next.iter()) {
            v.extend_from_slice(&r.props);
        }
        v.extend_from_slice(&self.debris);
        v
    }

    /// The current rep, for HUD arrows and test drivers.
    pub fn current_rep(&self) -> Option<RepInfo> {
        let cur = self.cur.as_ref()?;
        Some(match &cur.kind {
            RepKind::Gate { centre, yaw, half_w } => RepInfo::Gate {
                centre: *centre,
                yaw: *yaw,
                half_width: *half_w,
            },
            RepKind::Ring {
                centre,
                radius,
                entered,
                ..
            } => RepInfo::Ring {
                centre: *centre,
                radius: *radius,
                need_tier: Difficulty::at(self.level).ring_tier,
                entered: *entered,
            },
            RepKind::Logs { yaw, centres, done } => RepInfo::Logs {
                yaw: *yaw,
                remaining: centres[*done..].to_vec(),
            },
            RepKind::Egg { .. } => RepInfo::Egg {
                pos: self.egg.map(|e| e.pos),
            },
            RepKind::Buddy => RepInfo::Buddy,
        })
    }

    /// Seconds left on the current rep, if it has a clock.
    pub fn time_left(&self) -> Option<f32> {
        if let Some(e) = &self.egg {
            return Some(e.life.max(0.0));
        }
        self.rep_limit.map(|l| (l - self.rep_t).max(0.0))
    }

    /// The numbers of the current level.
    pub fn difficulty(&self) -> Difficulty {
        Difficulty::at(self.level)
    }

    // -- building reps ------------------------------------------------------

    fn start_reps(&mut self, runner: &RunnerView, rng: &mut Pcg32) {
        self.egg = None;
        match self.id {
            DrillId::Slalom | DrillId::DriftRing | DrillId::Hurdles => {
                self.line = Line {
                    pos: flat(runner.pos),
                    yaw: runner.yaw,
                };
                self.line.advance(FIRST_DISTANCE, self.origin, rng);
                self.cur = Some(self.gen_rep(rng, runner));
                self.next = Some(self.gen_rep(rng, runner));
            }
            DrillId::LaserRange | DrillId::BashBuddy => {
                self.next = None;
                self.cur = Some(self.gen_rep(rng, runner));
            }
        }
        self.begin_rep();
    }

    fn begin_rep(&mut self) {
        self.rep_t = 0.0;
        self.rep_limit = self.cur.as_ref().and_then(|r| r.limit);
        self.ring_swept_deg = 0.0;
        self.ring_ready = false;
    }

    /// Builds a rep at the line and moves the line on.
    fn gen_rep(&mut self, rng: &mut Pcg32, runner: &RunnerView) -> Rep {
        let d = Difficulty::at(self.level);
        let y = self.ground_y;
        let o = self.origin;
        match self.id {
            DrillId::Slalom => {
                let l = self.line.yaw;
                self.side = -self.side;
                let centre2 = self.line.pos + right2(l) * self.side * d.side_offset;
                let half_w = d.gate_width * 0.5;
                let off = right2(l) * (half_w + CONE_RADIUS);
                let props = [centre2 - off, centre2 + off]
                    .iter()
                    .map(|&p| DrillProp {
                        kind: PropKind::Cone,
                        pos: lift(p, y),
                        yaw: l,
                        knocked: 0.0,
                    })
                    .collect();
                self.line.advance(d.gate_spacing, o, rng);
                Rep {
                    props,
                    kind: RepKind::Gate {
                        centre: lift(centre2, y),
                        yaw: l,
                        half_w,
                    },
                    limit: Some(REP_TIMEOUT),
                }
            }
            DrillId::DriftRing => {
                let c = self.line.pos;
                let r = d.ring_radius;
                let mut props = vec![DrillProp {
                    kind: PropKind::Pylon,
                    pos: lift(c, y),
                    yaw: 0.0,
                    knocked: 0.0,
                }];
                for k in 0..8 {
                    let a = k as f32 * std::f32::consts::TAU / 8.0;
                    props.push(DrillProp {
                        kind: PropKind::RingCone,
                        pos: lift(c + dir2(a) * r, y),
                        yaw: a,
                        knocked: 0.0,
                    });
                }
                self.line.advance(RING_SPACING, o, rng);
                Rep {
                    props,
                    kind: RepKind::Ring {
                        centre: lift(c, y),
                        radius: r,
                        swept: 0.0,
                        last_ang: None,
                        entered: false,
                    },
                    limit: Some(RING_APPROACH_TIMEOUT),
                }
            }
            DrillId::Hurdles => {
                let l = self.line.yaw;
                let double = d.double_chance > 0.0 && rng.gen::<f32>() < d.double_chance;
                let mut centres = vec![lift(self.line.pos, y)];
                if double {
                    centres.push(lift(self.line.pos + dir2(l) * DOUBLE_GAP, y));
                }
                let props = centres
                    .iter()
                    .map(|&p| DrillProp {
                        kind: PropKind::Log,
                        pos: p,
                        yaw: l,
                        knocked: 0.0,
                    })
                    .collect();
                let extra = if double { DOUBLE_GAP } else { 0.0 };
                self.line.advance(d.hurdle_spacing + extra, o, rng);
                Rep {
                    props,
                    kind: RepKind::Logs {
                        yaw: l,
                        centres,
                        done: 0,
                    },
                    limit: Some(REP_TIMEOUT),
                }
            }
            DrillId::LaserRange => Rep {
                props: Vec::new(),
                kind: RepKind::Egg {
                    pop_in: EGG_POP_DELAY,
                },
                limit: None,
            },
            DrillId::BashBuddy => {
                let side = rand_sign(rng);
                let p = flat(runner.pos)
                    + dir2(runner.yaw) * BUDDY_AHEAD
                    + right2(runner.yaw) * side * BUDDY_SIDE;
                let p = clamp_radius(p, o, DUMMY_MIN_R, DUMMY_MAX_R, dir2(runner.yaw));
                self.dummy_yaw = runner.yaw;
                self.dodge_dir = 0.0;
                self.dodge_t = 0.8;
                let py = runner.pos.y - runner.air_h;
                self.dummy = Some(DummyCmd {
                    goal: lift(p + dir2(runner.yaw) * BASH_LOOKAHEAD, py),
                    speed_frac: d.buddy_speed,
                    respawn: Some((lift(p, py), runner.yaw)),
                });
                Rep {
                    props: Vec::new(),
                    kind: RepKind::Buddy,
                    limit: Some(d.buddy_time),
                }
            }
        }
    }

    // -- stepping -----------------------------------------------------------

    /// Advances the drill one tick. Everything is deterministic given the
    /// seed, the views and `rng`.
    pub fn step(&mut self, runner: &RunnerView, dt: f32, rng: &mut Pcg32) -> Vec<DrillEvent> {
        let mut ev = Vec::new();
        if self.over {
            return ev;
        }
        self.fx = DrillFx::default();
        if let Some(c) = self.dummy.as_mut() {
            c.respawn = None;
        }
        self.elapsed += dt;
        self.ground_y = runner.pos.y - runner.air_h;
        let prev = self.prev.unwrap_or(runner.pos);
        self.prev = Some(runner.pos);

        // Knock animation and debris.
        let rate = dt * KNOCK_RATE;
        let fall = |p: &mut DrillProp| {
            if p.knocked > 0.0 && p.knocked < 1.0 {
                p.knocked = (p.knocked + rate).min(1.0);
            }
        };
        for r in self.cur.iter_mut().chain(self.next.iter_mut()) {
            r.props.iter_mut().for_each(fall);
        }
        self.debris.iter_mut().for_each(fall);
        if self.debris_t > 0.0 {
            self.debris_t -= dt;
            if self.debris_t <= 0.0 {
                self.debris.clear();
            }
        }

        self.rep_t += dt;
        let Some(mut cur) = self.cur.take() else {
            return ev;
        };

        // Knocked cones, in the current or the next rep.
        let heading = if flat(runner.vel).length() > 0.5 {
            yaw2(flat(runner.vel))
        } else {
            runner.yaw
        };
        let mut knocked = Self::knock(&mut cur.props, prev, runner.pos, heading);
        if let Some(n) = self.next.as_mut() {
            knocked |= Self::knock(&mut n.props, prev, runner.pos, heading);
        }
        let mut res = if knocked {
            Res::Strike
        } else {
            self.eval(&mut cur, runner, prev, dt, rng)
        };
        if res == Res::Pending {
            if let Some(l) = self.rep_limit {
                if self.rep_t >= l {
                    res = Res::Strike;
                }
            }
        }
        self.cur = Some(cur);
        match res {
            Res::Pending => {}
            Res::Clean => self.on_clean(&mut ev, runner, rng),
            Res::Strike => self.on_strike(&mut ev, runner, rng),
        }
        ev
    }

    fn knock(props: &mut [DrillProp], prev: Vec3, pos: Vec3, heading: f32) -> bool {
        let (a, b) = (flat(prev), flat(pos));
        let mut hit = false;
        for p in props.iter_mut() {
            let rad = match p.kind {
                PropKind::Cone => CONE_RADIUS,
                PropKind::RingCone => RING_CONE_RADIUS,
                _ => continue,
            };
            if p.knocked == 0.0 && seg_dist(a, b, flat(p.pos)) < rad + RUNNER_RADIUS {
                p.knocked = 0.001;
                p.yaw = heading;
                hit = true;
            }
        }
        hit
    }

    fn eval(
        &mut self,
        cur: &mut Rep,
        runner: &RunnerView,
        prev: Vec3,
        dt: f32,
        rng: &mut Pcg32,
    ) -> Res {
        let d = Difficulty::at(self.level);
        let (a, b) = (flat(prev), flat(runner.pos));
        match &mut cur.kind {
            RepKind::Gate { centre, yaw, half_w } => {
                let c = flat(*centre);
                let n = dir2(*yaw);
                let (d0, d1) = ((a - c).dot(n), (b - c).dot(n));
                if d0 < 0.0 && d1 >= 0.0 {
                    let t = -d0 / (d1 - d0);
                    let lat = (a + (b - a) * t - c).dot(right2(*yaw)).abs();
                    if lat <= *half_w {
                        return Res::Clean;
                    } else if lat < 25.0 {
                        return Res::Strike;
                    }
                }
                Res::Pending
            }
            RepKind::Ring {
                centre,
                radius,
                swept,
                last_ang,
                entered,
            } => {
                let c = flat(*centre);
                let rel = b - c;
                let r = rel.length();
                if !*entered && r < *radius + RING_BAND {
                    *entered = true;
                    self.rep_t = 0.0;
                    self.rep_limit = Some(d.ring_time);
                }
                if let Some(tier) = runner.boost_released {
                    let full = swept.abs().to_degrees() >= RING_SWEEP_DEG;
                    *swept = 0.0;
                    *last_ang = None;
                    self.ring_swept_deg = 0.0;
                    self.ring_ready = false;
                    if full {
                        return if tier >= d.ring_tier {
                            Res::Clean
                        } else {
                            Res::Strike
                        };
                    }
                }
                let in_band = r > *radius && r < *radius + RING_BAND;
                if in_band && runner.drifting {
                    let ang = crate::math::atan2(rel.x,rel.y);
                    if let Some(l) = *last_ang {
                        *swept += wrap_angle(ang - l);
                    }
                    *last_ang = Some(ang);
                } else {
                    *swept = 0.0;
                    *last_ang = None;
                }
                self.ring_swept_deg = swept.abs().to_degrees();
                self.ring_ready = self.ring_swept_deg >= RING_SWEEP_DEG
                    && runner.drifting
                    && runner.spark_tier >= d.ring_tier;
                Res::Pending
            }
            RepKind::Logs { yaw, centres, done } => {
                let c = flat(centres[*done]);
                let n = dir2(*yaw);
                let (d0, d1) = ((a - c).dot(n), (b - c).dot(n));
                if d0 < 0.0 && d1 >= 0.0 {
                    let t = -d0 / (d1 - d0);
                    let lat = (a + (b - a) * t - c).dot(right2(*yaw)).abs();
                    if lat < 25.0 {
                        if lat > LOG_HALF_LENGTH + RUNNER_RADIUS {
                            // Went round the end of the log.
                            return Res::Strike;
                        }
                        if runner.air_h >= HURDLE_HEIGHT {
                            *done += 1;
                            if *done >= centres.len() {
                                return Res::Clean;
                            }
                        } else {
                            cur.props[*done].knocked = 0.001;
                            self.fx.stumble = true;
                            return Res::Strike;
                        }
                    }
                }
                Res::Pending
            }
            RepKind::Egg { pop_in } => {
                if let Some(e) = self.egg.as_mut() {
                    e.life -= dt;
                    if runner.egg_destroyed {
                        self.egg = None;
                        return Res::Clean;
                    }
                    if e.life <= 0.0 {
                        self.egg = None;
                        return Res::Strike;
                    }
                    return Res::Pending;
                }
                *pop_in -= dt;
                if *pop_in <= 0.0 {
                    let ang = rand_sign(rng)
                        * d.egg_angle_deg.to_radians()
                        * rng.gen_range(0.8..1.0);
                    let dist = rng.gen_range(25.0..45.0);
                    let p = flat(runner.pos) + dir2(runner.yaw + ang) * dist;
                    let p = clamp_radius(
                        p,
                        self.origin,
                        FIELD_MIN_R + 4.0,
                        FIELD_MAX_R - 4.0,
                        dir2(runner.yaw),
                    );
                    self.egg = Some(DrillEgg {
                        pos: lift(p, self.ground_y + TARGET_HEIGHT),
                        life: d.egg_life,
                        life_max: d.egg_life,
                    });
                    self.fx.egg_spawned = true;
                }
                Res::Pending
            }
            RepKind::Buddy => {
                if runner.bash_hit {
                    return Res::Clean;
                }
                if let (Some(dm), Some(cmd)) = (runner.dummy, self.dummy.as_mut()) {
                    let at = flat(dm.pos);
                    self.dummy_yaw = bend(
                        self.dummy_yaw,
                        at,
                        self.origin,
                        DUMMY_MIN_R,
                        DUMMY_MAX_R,
                        1.0 * dt,
                    );
                    if d.dodge {
                        self.dodge_t -= dt;
                        if self.dodge_t <= 0.0 {
                            self.dodge_dir = if self.dodge_dir == 0.0 {
                                rand_sign(rng)
                            } else {
                                -self.dodge_dir
                            };
                            self.dodge_t = rng.gen_range(0.6..1.0) * d.dodge_period;
                        }
                    } else {
                        self.dodge_dir = 0.0;
                    }
                    let aim = self.dummy_yaw + self.dodge_dir * DODGE_ANGLE;
                    cmd.goal = lift(at + dir2(aim) * BASH_LOOKAHEAD, dm.pos.y);
                    cmd.speed_frac = d.buddy_speed;
                }
                Res::Pending
            }
        }
    }

    fn on_clean(&mut self, ev: &mut Vec<DrillEvent>, runner: &RunnerView, rng: &mut Pcg32) {
        ev.push(DrillEvent::RepClean);
        self.clean_total += 1;
        self.reps_in_level += 1;
        if self.reps_in_level >= REPS_PER_LEVEL {
            if self.level >= MAX_LEVEL {
                self.master = true;
                self.over = true;
                ev.push(DrillEvent::Master);
                ev.push(DrillEvent::Over);
                return;
            }
            self.level += 1;
            self.reps_in_level = 0;
            self.best_level = self.best_level.max(self.level);
            ev.push(DrillEvent::LevelUp(self.level));
            let star = match self.level {
                3 => 1,
                6 => 2,
                9 => 3,
                _ => 0,
            };
            if star > 0 {
                self.stars = star;
                ev.push(DrillEvent::Star(star));
            }
        }
        match self.id {
            DrillId::Slalom | DrillId::DriftRing | DrillId::Hurdles => {
                self.cur = self.next.take();
                self.next = Some(self.gen_rep(rng, runner));
            }
            _ => {
                self.cur = Some(self.gen_rep(rng, runner));
            }
        }
        self.begin_rep();
    }

    fn on_strike(&mut self, ev: &mut Vec<DrillEvent>, runner: &RunnerView, rng: &mut Pcg32) {
        self.strikes += 1;
        ev.push(DrillEvent::Strike);
        self.egg = None;
        // Knocked props stay visible for a moment as the rep is replaced.
        let mut debris = Vec::new();
        for r in self.cur.iter().chain(self.next.iter()) {
            debris.extend(r.props.iter().filter(|p| p.knocked > 0.0).copied());
        }
        if !debris.is_empty() {
            self.debris = debris;
            self.debris_t = DEBRIS_TIME;
        }
        if self.strikes >= MAX_STRIKES {
            self.over = true;
            ev.push(DrillEvent::Over);
            return;
        }
        self.start_reps(runner, rng);
    }
}
