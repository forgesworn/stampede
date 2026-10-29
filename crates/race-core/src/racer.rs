//! The ostrich: state, the pure per-tick driving model (spec 4.3) and the
//! compact network form.

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::input::Input;
use crate::items::Item;
use crate::math::{approach, flat, forward, lerp, right, wrap_angle, yaw_of, DT};
use crate::track::{Feature, Surface, Track, PAD_WIDTH};
use crate::tunables::{CharClass, Character, Tunables};

/// Eye height above the feet (laser origin).
pub const EYE_HEIGHT: f32 = 2.35;
/// Eye distance ahead of the body centre.
pub const EYE_FORWARD: f32 = 0.55;
/// Body centre height above the feet (hit tests, coins).
pub const BODY_HEIGHT: f32 = 1.3;

/// What a racer's launch earned at GO ([`Racer::countdown_step`], spec 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Launch {
    /// Throttle pressed in the last moment before GO: start boost.
    StartBoost,
    /// Throttle held too early: stumble.
    Stumble,
}

/// Drift state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Drift {
    /// Drifting (sliding, charging).
    pub active: bool,
    /// Drift held, waiting for a steer to pick the side (the name is historical).
    pub hop: bool,
    /// -1 left, +1 right, 0 none.
    pub dir: i8,
    /// Charge in seconds-equivalent.
    pub charge: f32,
    /// 0 none, 1 blue, 2 orange, 3 purple.
    pub tier: u8,
    /// Slide blend, 0..=255 for 0..1 (see [`Drift::blend`]). Whole numbers keep the
    /// state exact and small on the wire.
    pub blend_q: u8,
    /// Carried drift turn multiplier in hundredths (see [`Drift::carry`]).
    pub carry_q: u8,
    /// Ticks the slip has been over the spin-out threshold or, while a charged
    /// drift is held, settled straight (the two never overlap).
    pub over_ticks: u8,
    /// Exit boost waiting for the slip to come down (tier, 0 = none).
    pub pending: u8,
    /// Ticks left to wait for the pending boost.
    pub pending_ticks: u8,
    /// Charge the pending boost was earned with, in tenths of a second.
    pub pending_charge_q: u8,
}

impl Drift {
    /// How far a steer `u` (+1 into the drift, -1 against) leans into the drift,
    /// 0..1: proportional, with a small soft dead zone round the centre.
    pub(crate) fn lean(u: f32) -> f32 {
        let x = ((u - 0.05) / 0.15).clamp(0.0, 1.0);
        u.clamp(0.0, 1.0) * x * x * (3.0 - 2.0 * x)
    }

    /// The drift turn multiplier at full lean for slide blend `b`: plain
    /// steering as the drift starts, `drift_turn_max` once it has eased in.
    pub(crate) fn full_turn(b: f32, t: &Tunables) -> f32 {
        lerp(1.0, t.drift_turn_max, b)
    }

    /// The drift turn multiplier a steady steer `u` asks for at blend `b`.
    /// Like wheelspin it only adds to the steering already there: none
    /// centred or steering out of the drift.
    pub(crate) fn steer_target(u: f32, b: f32, t: &Tunables) -> f32 {
        Self::lean(u) * Self::full_turn(b, t)
    }

    /// 0..1: how loose the grip is, from the carried turn. Steering into the
    /// drift breaks it away progressively; straightening up gets it back as
    /// the turn unwinds.
    pub(crate) fn looseness(&self, t: &Tunables) -> f32 {
        (self.carry() / (0.5 * t.drift_turn_max)).clamp(0.0, 1.0)
    }

    /// 0..1 slide blend: eases up while drifting and down after release, so grip
    /// (and the carried turn) never snap. Outlives `active` while easing out.
    pub fn blend(&self) -> f32 {
        f32::from(self.blend_q) / 255.0
    }

    /// Carried drift turn multiplier (low-passed from the steering).
    pub fn carry(&self) -> f32 {
        f32::from(self.carry_q) / 100.0
    }

    fn set_blend(&mut self, b: f32) {
        self.blend_q = (b.clamp(0.0, 1.0) * 255.0).round() as u8;
    }

    /// Low-passes the carried turn towards what the steering asks for. Steering
    /// into the drift answers fast, counter-steering faster still; with the
    /// steering centred it unwinds over `drift_carry_time`, so the slide
    /// straightens smoothly rather than snapping.
    fn follow_steer(&mut self, steer: f32, t: &Tunables, dt: f32) {
        let u = steer * self.dir as f32;
        let x = ((u.abs() - 0.15) / 0.2).clamp(0.0, 1.0);
        let centred = 1.0 - x * x * (3.0 - 2.0 * x);
        let target = Self::steer_target(u, self.blend(), t);
        let steered = if u < 0.0 { t.drift_counter_time } else { t.drift_steer_time };
        let tau = lerp(steered, t.drift_carry_time, centred).max(1e-3);
        let from = self.carry();
        let want = from + (target - from) * (1.0 - (-dt / tau).exp());
        let mut q = (want * 100.0).round().clamp(0.0, 255.0) as u8;
        if q == self.carry_q && (target - from).abs() >= 0.01 {
            // Never stall short of the target for want of a whole step.
            q = if target > from { q.saturating_add(1) } else { q.saturating_sub(1) };
        }
        self.carry_q = q;
    }

    /// Ends the drift but keeps the blend, side and carried turn so the slide
    /// eases out; a boost still waiting to fire is kept too.
    fn release(&mut self) {
        *self = Drift {
            dir: self.dir,
            blend_q: self.blend_q,
            carry_q: self.carry_q,
            pending: self.pending,
            pending_ticks: self.pending_ticks,
            pending_charge_q: self.pending_charge_q,
            ..Drift::default()
        };
    }
}

/// What the laser beam ended on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaserHit {
    /// Not firing.
    #[default]
    None,
    /// Full range, nothing hit.
    Air,
    /// A wall.
    Wall,
    /// The ground.
    Ground,
    /// A Power Egg target (index into `World::targets`).
    Target(u16),
    /// A racer.
    Racer(u8),
}

/// Laser eyes state (spec 5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Laser {
    /// Beams on this tick.
    pub firing: bool,
    /// 0..1.
    pub heat: f32,
    /// Locked out after overheating.
    pub overheated: bool,
    /// Remaining overheat lock.
    pub lock_t: f32,
    /// Beam origin (between the eyes).
    pub origin: Vec3,
    /// Aim point on the forward ray.
    pub aim: Vec3,
    /// Where the beam ends.
    pub hit: Vec3,
    /// What it ended on.
    pub hit_kind: LaserHit,
    /// Racer currently held in the beam, and for how long.
    pub on_racer: Option<u8>,
    /// Time on `on_racer`.
    pub on_racer_t: f32,
}

/// Motion side effects of one [`Racer::integrate`] (World turns them into events).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MotionFx {
    /// Jumped.
    pub jumped: bool,
    /// Landed.
    pub landed: bool,
    /// Landed from a fall > `hard_landing`.
    pub hard_landing: bool,
    /// A foot hit the ground.
    pub footstep: bool,
    /// Drift boost of this tier fired (0 = none).
    pub drift_boost: u8,
    /// The exit boost fired: the drift's charge (s) times its tier (Drift King points).
    pub drift_points: f32,
    /// Drift sparks reached this tier (0 = no change).
    pub drift_tier: u8,
    /// Started a drift.
    pub drift_start: bool,
    /// Trick off a ramp.
    pub trick: bool,
    /// Landed a trick: boost.
    pub trick_boost: bool,
    /// Hit a boost pad.
    pub boost_pad: bool,
    /// Fell below the kill height.
    pub fell: bool,
    /// Placed back on the track.
    pub respawned: bool,
    /// Laser overheated.
    pub overheated: bool,
    /// Wall hit with this head-on-ness (0 = none).
    pub wall_hit: f32,
}

/// One racing ostrich. Everything a renderer and HUD need is a public field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Racer {
    /// Index in `World::racers`.
    pub idx: u8,
    /// Character id (see [`crate::CHARACTERS`]).
    pub character: u8,
    /// Class (from the character).
    pub class: CharClass,
    /// Feet position.
    pub pos: Vec3,
    /// Velocity.
    pub vel: Vec3,
    /// Heading.
    pub yaw: f32,
    /// Body yaw for rendering (heading + drift angle, eased).
    pub visual_yaw: f32,
    /// Yaw rate this tick (rad/s, + = left).
    pub yaw_rate: f32,
    /// Signed speed along the heading.
    pub speed: f32,
    /// Feet on the ground.
    pub on_ground: bool,
    /// Height above the ground (0 on the ground).
    pub air_h: f32,
    /// Highest point of the current airtime.
    pub air_peak: f32,
    /// Time in the air.
    pub air_t: f32,
    /// Left the ground from a ramp.
    pub from_ramp: bool,
    /// Track sample hint.
    pub track_idx: u32,
    /// Arc length on the track.
    pub s: f32,
    /// Lateral offset from the centreline.
    pub lateral: f32,
    /// Ground height under the racer.
    pub ground_y: f32,
    /// On the shoulder (off road).
    pub offroad: bool,
    /// Drift state.
    pub drift: Drift,
    /// Boost time left.
    pub boost_t: f32,
    /// Strength of the running boost: 0 = full (pads and the rest), 1..3 = the
    /// exit boost of that drift tier (see [`Tunables::drift_boost_power`]).
    pub boost_tier: u8,
    /// Spun out: time left.
    pub stun_t: f32,
    /// Spin rate while spun out.
    pub spin_rate: f32,
    /// Slowed: time left.
    pub slow_t: f32,
    /// Top-speed multiplier while slowed.
    pub slow_mult: f32,
    /// Shield time left.
    pub shield_t: f32,
    /// Stumbling: time left (no throttle, slowed).
    pub stumble_t: f32,
    /// Mega Laser time left.
    pub mega_t: f32,
    /// Battle: victims (bit per racer index) already hit by the current Mega
    /// Laser activation. One activation pops at most one feather per victim.
    /// Host-side bookkeeping, not on the wire.
    pub mega_hit: u32,
    /// Laser Trump: victims (bit per racer index) already smashed by the
    /// current rocket. One rocket hits each victim at most once.
    /// Host-side bookkeeping, not on the wire.
    #[serde(skip)]
    pub trump_hit: u32,
    /// Sat Magnet time left.
    pub magnet_t: f32,
    /// Trick animation time left.
    pub trick_t: f32,
    /// Jump was pressed on a ramp this recently (trick window).
    pub trick_armed: f32,
    /// A trick is in progress: boost on landing.
    pub trick_pending: bool,
    /// Respawn fade time left (frozen while > 0).
    pub respawn_t: f32,
    /// Landing dust burst time left.
    pub landing_t: f32,
    /// Temporary gameplay effects, shared with remote renderers.
    pub effects: crate::playful::Effects,
    /// Purely visual outfit indices: colour, ostrich attire, jockey attire.
    pub attire: [u8; 3],
    /// Bitcoin collected in this race, separate from the permanent wallet.
    pub coins: u32,
    /// Held item.
    pub item: Option<Item>,
    /// Uses left of the held item.
    pub item_uses: u8,
    /// Power-ups waiting behind the held one, oldest first (taken with a Shared
    /// Cake). The next moves into the slot when the held one is used up, so the
    /// queue is only ever non-empty while `item` is `Some`.
    #[serde(deserialize_with = "crate::items::bounded_queue")]
    pub queue: Vec<crate::items::ItemRoll>,
    /// Laps started (0 on the grid, 1 after crossing the line at the start).
    pub lap: u8,
    /// Next sector checkpoint (1..=3; 4 = lap valid).
    pub next_cp: u8,
    /// (lap - 1) x length + s.
    pub progress: f32,
    /// 1-based place.
    pub place: u8,
    /// Race time at the finish.
    pub finished_at: Option<f32>,
    /// Laser eyes.
    pub laser: Laser,
    /// Bash cooldown left.
    #[serde(skip)]
    pub bash_cd: f32,
    /// Head-swing animation time left.
    #[serde(skip)]
    pub bash_t: f32,
    /// Head-swing side (-1 left, +1 right).
    pub bash_side: f32,
    /// Head-swing pose, signed by side (-1..1).
    #[serde(skip)]
    pub bash_anim: f32,
    /// Head turned round (0..1, eased).
    pub look_back: f32,
    /// Run cycle phase (0..1).
    pub gait_phase: f32,
    /// Dust emission (0..1).
    pub dust: f32,
    /// Driving the wrong way.
    pub wrong_way: bool,
    /// Time spent going the wrong way.
    pub wrong_way_t: f32,
    /// Top-speed scale (bots: level x rubber band; humans 1).
    pub speed_scale: f32,
    /// Input of the previous tick (edge detection).
    pub prev_input: Input,
    /// Dazzle immunity per attacker, in ticks.
    pub dazzle_imm: [u8; 8],
    /// Continuous time stopped (for stats and bots).
    pub stuck_t: f32,
    /// Countdown: how long the throttle has been held.
    pub launch_t: f32,
}

impl Racer {
    /// A racer standing at `pos` facing `yaw`, at track sample `hint`.
    pub fn new(idx: u8, character: u8, pos: Vec3, yaw: f32, hint: usize) -> Racer {
        Racer {
            idx,
            character,
            class: Character::get(character).class,
            pos,
            vel: Vec3::ZERO,
            yaw,
            visual_yaw: yaw,
            yaw_rate: 0.0,
            speed: 0.0,
            on_ground: true,
            air_h: 0.0,
            air_peak: pos.y,
            air_t: 0.0,
            from_ramp: false,
            track_idx: hint as u32,
            s: 0.0,
            lateral: 0.0,
            ground_y: pos.y,
            offroad: false,
            drift: Drift::default(),
            boost_t: 0.0,
            boost_tier: 0,
            stun_t: 0.0,
            spin_rate: 0.0,
            slow_t: 0.0,
            slow_mult: 1.0,
            shield_t: 0.0,
            stumble_t: 0.0,
            mega_t: 0.0,
            mega_hit: 0,
            trump_hit: 0,
            magnet_t: 0.0,
            trick_t: 0.0,
            trick_armed: 0.0,
            trick_pending: false,
            respawn_t: 0.0,
            landing_t: 0.0,
            effects: Default::default(),
            attire: [0; 3],
            coins: 0,
            item: None,
            item_uses: 0,
            queue: Vec::new(),
            lap: 0,
            next_cp: 4,
            progress: 0.0,
            place: idx + 1,
            finished_at: None,
            laser: Laser::default(),
            bash_cd: 0.0,
            bash_t: 0.0,
            bash_side: 1.0,
            bash_anim: 0.0,
            look_back: 0.0,
            gait_phase: (idx as f32 * 0.37).fract(),
            dust: 0.0,
            wrong_way: false,
            wrong_way_t: 0.0,
            speed_scale: 1.0,
            prev_input: Input::default(),
            dazzle_imm: [0; 8],
            stuck_t: 0.0,
            launch_t: 0.0,
        }
    }

    /// Takes a power-up: into the slot when it is empty, else to the back of
    /// the queue. Returns false (and takes nothing) when the queue is full.
    pub fn stash(&mut self, item: Item, uses: u8) -> bool {
        if self.item.is_none() {
            self.item = Some(item);
            self.item_uses = uses.max(1);
            true
        } else if self.queue.len() < crate::items::MAX_QUEUE {
            self.queue.push(crate::items::ItemRoll { item, uses: uses.max(1) });
            true
        } else {
            false
        }
    }

    /// With the slot empty, moves the next queued power-up into it. Returns
    /// whether one moved in.
    pub fn refill(&mut self) -> bool {
        if self.item.is_some() || self.queue.is_empty() {
            return false;
        }
        let next = self.queue.remove(0);
        self.item = Some(next.item);
        self.item_uses = next.uses.max(1);
        true
    }

    /// Loses the held power-up and everything queued behind it.
    pub fn clear_items(&mut self) {
        self.item = None;
        self.item_uses = 0;
        self.queue.clear();
    }

    /// Unit forward of the heading.
    pub fn forward(&self) -> Vec3 {
        forward(self.yaw)
    }

    /// Unit right of the heading.
    pub fn right(&self) -> Vec3 {
        right(self.yaw)
    }

    /// Body centre (hit tests).
    pub fn body(&self) -> Vec3 {
        self.pos + Vec3::Y * BODY_HEIGHT
    }

    /// Direction the head (and lasers) face: backwards while looking back.
    pub fn head_dir(&self) -> Vec3 {
        if self.look_back > 0.5 && !self.laser.firing && self.mega_t <= 0.0 {
            -self.forward()
        } else {
            self.forward()
        }
    }

    /// Laser origin between the eyes.
    pub fn eye(&self) -> Vec3 {
        let d = self.head_dir();
        self.pos + Vec3::Y * EYE_HEIGHT + d * EYE_FORWARD
    }

    /// Grants a full-power boost of `secs`, keeping any longer one running.
    /// Clears the weaker drift-exit tier so a feather, trick, coin or pad
    /// never runs at the exit boost's reduced power. One that outlasts a running
    /// Turbo Feather boost takes its place, so a Shared Cake no longer sees a
    /// Feather in it (using a Feather marks it again afterwards).
    pub fn grant_boost(&mut self, secs: f32) {
        if secs > self.boost_t {
            self.effects.feather = 0.0;
        }
        self.boost_t = self.boost_t.max(secs);
        self.boost_tier = 0;
    }

    /// Strength of the running boost as a fraction of `boost_speed`.
    pub fn boost_power(&self, t: &Tunables) -> f32 {
        match self.boost_tier {
            0 => 1.0,
            n => t.drift_boost_power[(n as usize - 1).min(2)],
        }
    }

    /// Effective top speed right now (class, coins, slow, boost, surface, scale).
    pub fn top_speed(&self, t: &Tunables) -> f32 {
        if self.effects.le_mans {
            return 0.0;
        }
        if self.effects.dismount > 0.0 {
            return 3.0;
        }
        let cls = self.class.mods();
        if self.effects.trump > 0.0 {
            return t.top_speed * cls.top * crate::playful::TRUMP_SPEED;
        }
        let base = t.top_speed
            * cls.top
            * self.speed_scale
            * (1.0 + self.coins.min(t.coin_max as u32) as f32 * t.coin_bonus);
        let mut top = base;
        if self.boost_t > 0.0 {
            top *= 1.0 + t.boost_speed * self.boost_power(t);
        } else if self.offroad {
            top *= t.offroad_top;
        }
        if self.slow_t > 0.0 {
            top *= self.slow_mult;
        }
        if self.stumble_t > 0.0 {
            top *= t.stumble_slow_mult;
        }
        let e = &self.effects;
        if e.burn > 0.0 { top *= t.playful.burn_speed; }
        if e.pill > 0.0 { top *= crate::playful::PILL_SPEED; }
        top
    }

    /// Rocketing under a Laser Trump.
    pub fn trumping(&self) -> bool {
        self.effects.trump > 0.0
    }

    /// Spun out / respawning: no control.
    pub fn disabled(&self) -> bool {
        self.stun_t > 0.0
            || self.respawn_t > 0.0
            || self.effects.tumble > 0.0
            || self.effects.dismount > 0.0
    }

    /// One grid-countdown tick (spec 6 start boost): counts how long the
    /// throttle has been held, records `prev_input`, keeps the head forward,
    /// and on the last countdown tick (`last`: `tick + 1 >= go_tick`) awards
    /// the launch. [`crate::World::step`] runs this for every racer before GO;
    /// clients replay it to predict their own start. Raises no events: the
    /// caller turns the result into `StartBoost` / `Stumble`.
    pub fn countdown_step(&mut self, input: &Input, last: bool, t: &Tunables) -> Option<Launch> {
        if self.effects.le_mans {
            self.launch_t = 0.0;
            self.prev_input = *input;
            self.look_back = 0.0;
            return None;
        }
        if input.throttle > 0.5 {
            self.launch_t += DT;
        } else {
            self.launch_t = 0.0;
        }
        self.prev_input = *input;
        self.look_back = 0.0;
        if !last {
            return None;
        }
        let lt = self.launch_t;
        self.launch_t = 0.0;
        if lt > 0.0 && lt <= t.start_boost_window + 1e-4 {
            self.boost_t = t.start_boost_time;
            self.boost_tier = 0;
            Some(Launch::StartBoost)
        } else if lt > t.start_stumble_early {
            self.stumble_t = t.start_stumble_time;
            Some(Launch::Stumble)
        } else {
            None
        }
    }

    /// Whether this racer collects a coin sitting at `pos` (body within
    /// reach, not respawning). The world's pick-up test; clients replay it.
    pub fn reaches_coin(&self, pos: Vec3, t: &Tunables) -> bool {
        self.respawn_t <= 0.0 && (self.body() - pos).length() < t.coin_radius + 0.4
    }

    /// Collects one race coin, saturating only at u32::MAX, and its micro-boost.
    /// Returns the coins now held. Raises no event (the world emits
    /// `CoinPicked`).
    pub fn pick_coin(&mut self, t: &Tunables) -> u32 {
        self.coins = self.coins.saturating_add(1);
        self.grant_boost(t.coin_boost);
        self.coins
    }

    /// Starts a spin-out of `dur` seconds (two full turns). Cancels a drift.
    pub fn spin_out(&mut self, dur: f32) {
        self.stun_t = dur;
        self.spin_rate = 2.0 * std::f32::consts::TAU / dur.max(0.05);
        self.drift = Drift::default();
        self.trick_pending = false;
    }

    /// Applies a slow (the stronger of overlapping slows wins).
    pub fn slow(&mut self, time: f32, mult: f32) {
        if self.slow_t <= 0.0 {
            self.slow_mult = mult;
        } else {
            self.slow_mult = self.slow_mult.min(mult);
        }
        self.slow_t = self.slow_t.max(time);
    }

    /// Motion only (spec 4.3): timers, speed, steering, grip, drift, jump,
    /// ground, walls, boost pads, dust and gait. A pure function of the racer,
    /// the input and the track (no other racers, no pickups): clients replay
    /// it for prediction. Updates `prev_input`.
    pub fn integrate(&mut self, input: &Input, track: &Track, t: &Tunables) -> MotionFx {
        let dt = DT;
        let raw_input = *input;
        let trumping = self.effects.trump > 0.0;
        let eating = self.effects.cake_eat > 0.0;
        let controlled = if trumping {
            // The rocket ignores every control; the autopilot steers below.
            Input::default()
        } else if self.effects.dismount > 0.0 {
            Input {
                steer: input.steer,
                ..Input::default()
            }
        } else if eating {
            // Distracted by the cake: steering and throttle work, no button does. The
            // raw input still goes to `prev_input`, so a button held through the last
            // bite does not fire as the cake ends; a fresh press afterwards does.
            Input {
                drift: false,
                jump: false,
                laser: false,
                item: false,
                bash: false,
                ..*input
            }
        } else {
            *input
        };
        let input = &controlled;
        let mut fx = MotionFx::default();
        let cls = self.class.mods();
        let prev = self.prev_input;
        self.prev_input = raw_input;
        let prev_yaw = self.yaw;
        let jump_edge = input.jump && !prev.jump;
        let drift_edge = input.drift && !prev.drift;
        let old_pos = self.pos;
        let steer = if trumping {
            crate::playful::trump_steer(self, track, t)
        } else if self.effects.dismount > 0.0 {
            crate::playful::autosteer(self, track, t)
        } else { input.steer.clamp(-1.0, 1.0) };

        self.effects.step(dt, &t.playful);
        if self.effects.dismount <= 0.0 && self.stun_t <= 0.0 {
            self.effects.knock = crate::mischief::KnockKind::None;
        }
        if trumping && self.effects.trump <= 0.0 {
            // Out of rocket: carry the speed on as a boost rather than stalling.
            self.grant_boost(crate::playful::TRUMP_EXIT_BOOST);
        }
        // 1. Timers.
        let dec = |x: &mut f32| *x = (*x - dt).max(0.0);
        dec(&mut self.boost_t);
        if self.boost_t <= 0.0 {
            self.boost_tier = 0;
            self.effects.feather = 0.0;
        }
        dec(&mut self.slow_t);
        if self.slow_t <= 0.0 {
            self.slow_mult = 1.0;
        }
        dec(&mut self.shield_t);
        dec(&mut self.stumble_t);
        dec(&mut self.mega_t);
        dec(&mut self.magnet_t);
        dec(&mut self.trick_t);
        dec(&mut self.trick_armed);
        dec(&mut self.bash_cd);
        dec(&mut self.bash_t);
        dec(&mut self.landing_t);
        for x in self.dazzle_imm.iter_mut() {
            *x = x.saturating_sub(1);
        }
        self.look_back = approach(
            self.look_back,
            if input.look_back { 1.0 } else { 0.0 },
            t.look_back_rate * dt,
        );
        self.bash_anim = if self.bash_t > 0.0 {
            self.bash_side * crate::math::sin(std::f32::consts::PI * (1.0 - self.bash_t / t.bash_anim_time))
        } else {
            0.0
        };

        if self.respawn_t > 0.0 {
            self.respawn_t -= dt;
            self.laser.firing = false;
            self.dust = 0.0;
            if self.respawn_t <= 0.0 {
                self.respawn_t = 0.0;
                self.respawn(track, t);
                fx.respawned = true;
            }
            return fx;
        }

        if self.effects.dismount > 0.0 {
            let walking = if eating { Input { jump: false, ..raw_input } } else { raw_input };
            self.walk_and_chase(&walking, prev.jump, track, dt);
            return fx;
        }

        // Laser heat (pure: depends on the input only).
        let want_fire = t.practice_lasers && input.laser && !self.disabled() && self.mega_t <= 0.0;
        if self.laser.overheated {
            self.laser.lock_t -= dt;
            self.laser.heat = (self.laser.heat - t.laser_cool_rate * dt).max(0.0);
            self.laser.firing = false;
            if self.laser.lock_t <= 0.0 {
                self.laser.overheated = false;
                self.laser.lock_t = 0.0;
            }
        } else if want_fire {
            self.laser.heat += t.laser_heat_rate * dt;
            self.laser.firing = true;
            if self.laser.heat >= 1.0 {
                self.laser.heat = 1.0;
                self.laser.overheated = true;
                self.laser.lock_t = t.laser_overheat_lock;
                self.laser.firing = false;
                fx.overheated = true;
            }
        } else {
            self.laser.heat = (self.laser.heat - t.laser_cool_rate * dt).max(0.0);
            self.laser.firing = false;
        }
        if !self.laser.firing {
            self.laser.on_racer = None;
            self.laser.on_racer_t = 0.0;
        }

        if self.effects.tumble > 0.0 {
            self.vel = Vec3::ZERO;
            self.speed = 0.0;
            self.drift = Drift::default();
            self.laser.firing = false;
            self.dust = 0.0;
            return fx;
        }
        // Surface under us.
        let proj0 = track.project(self.pos, self.track_idx as usize);
        let sm0 = track.sample_at(proj0.s);
        let half_road = 0.5 * sm0.width;
        let on_ramp = sm0.surface == Surface::Ramp && proj0.lateral.abs() <= half_road;
        self.offroad =
            self.on_ground && proj0.lateral.abs() > half_road && sm0.surface != Surface::Gap;
        if self.effects.rose > 0.0 {
            // A rose seller alongside leans in like a rider would: a steady shove
            // away from them and a little drag, never enough to unseat anyone.
            // Here rather than in the seller's update, so clients predict it.
            let side = f32::from(self.effects.rose_side.signum());
            self.vel -= sm0.right * side * crate::playful::ROSE_PUSH * dt;
            let fwd = forward(self.yaw);
            let along = self.vel.dot(fwd);
            if along > 0.0 {
                self.vel -= fwd * along * crate::playful::ROSE_DRAG * dt;
            }
        }

        let top = self.top_speed(t);
        let base_top = t.top_speed * cls.top;
        let boosting = self.boost_t > 0.0;
        let f = forward(self.yaw);
        let r = right(self.yaw);
        let mut hv = flat(self.vel);
        let mut vf = hv.dot(f);
        let vl = hv.dot(r);
        // The slide eases in while the drift is held and out after release.
        let blend_time = if self.drift.active { t.drift_ease_in } else { t.drift_ease_out };
        let blend = approach(
            self.drift.blend(),
            if self.drift.active { 1.0 } else { 0.0 },
            dt / blend_time.max(0.05),
        );
        self.drift.set_blend(blend);
        let blend = self.drift.blend();
        let sliding = self.drift.active || self.drift.blend_q > 0;
        // A drift turns at the rate for its actual speed: sliding sideways must not
        // read as slow, or the turn tightens into a spin.
        let turn_speed = lerp(vf.abs(), hv.length(), blend);
        let turn_base = lerp(
            t.turn_rate_low,
            t.turn_rate_high,
            (turn_speed / base_top).clamp(0.0, 1.0),
        ) * cls.turn
            * if trumping {
                crate::playful::TRUMP_TURN
            } else {
                1.0
            };

        // 2-4. Spin-out, speed along the heading, steering.
        if self.stun_t > 0.0 {
            self.stun_t = (self.stun_t - dt).max(0.0);
            self.yaw = wrap_angle(self.yaw + self.spin_rate * dt);
            self.yaw_rate = self.spin_rate;
            let sp = hv.length();
            if sp > 1e-4 {
                hv *= (sp - t.stun_decel * dt).max(0.0) / sp;
            }
            self.drift = Drift::default();
        } else if self.on_ground {
            let throttle = if self.stumble_t > 0.0 { 0.0 } else { 1.0 };
            if vf < 0.0 { vf = (vf + t.brake * dt).min(0.0); }
            else if vf < top {
                let mut a = t.accel * cls.accel * (1.0 - vf / top).max(t.accel_floor) * throttle;
                if boosting { a = a.max(t.boost_push); }
                if trumping { a = top / crate::playful::TRUMP_RAMP; }
                vf = (vf + a * dt).min(top);
            }
            if vf > top {
                let drag = t.coast_drag + if self.offroad { t.offroad_drag } else { 0.0 };
                vf = (vf - drag * dt).max(top);
            }
            let mut speed_fac = if vf < -0.5 {
                1.0
            } else {
                (vf.abs() / 5.0).min(1.0)
            };
            // Pinned against a wall the speed is ~0 and so would be the turn rate:
            // near a wall (and only there) there is a floor, so you can always turn away.
            if speed_fac < t.wall_turn_floor
                && track.half_band(proj0.s, proj0.lateral) - proj0.lateral.abs() < t.wall_turn_reach
            {
                speed_fac = t.wall_turn_floor;
            }
            let inv = if vf < -0.5 { -1.0 } else { 1.0 };
            let normal_rate = -steer * turn_base * speed_fac * inv;
            let rate = if sliding {
                let d = self.drift.dir as f32;
                if self.drift.active {
                    self.drift.follow_steer(steer, t, dt);
                }
                let mut drift_rate = -d * self.drift.carry() * turn_base * speed_fac;
                // Out past the soft limit the heading stops swinging further round, so
                // steering alone can never take a slide up to the spin-out angle.
                if vl * d < 0.0 {
                    let slip_in = crate::math::atan2(vl.abs(), hv.dot(f));
                    let over = (slip_in - t.drift_slip_soft) / (t.drift_slip_cap - t.drift_slip_soft);
                    drift_rate *= 1.0 - over.clamp(0.0, 1.0);
                }
                if self.drift.active {
                    drift_rate
                } else {
                    // Released: the carried turn fades out with the grip.
                    lerp(normal_rate, drift_rate, blend)
                }
            } else {
                normal_rate
            };
            self.yaw_rate = rate;
            self.yaw = wrap_angle(self.yaw + rate * dt);
            hv = f * vf + r * vl;
        } else {
            let mut rate = -steer * turn_base * t.air_steer;
            // The same soft/hard slip limit as on the ground: steering alone in a long
            // jump must not swing the heading round to the spin-out angle.
            if sliding && hv.length() > 1.0 {
                let slip_of = |yaw: f32| crate::math::atan2(hv.dot(right(yaw)).abs(), hv.dot(forward(yaw)));
                let slip_now = slip_of(self.yaw);
                if slip_of(wrap_angle(self.yaw + rate * dt)) > slip_now {
                    let over = (slip_now - t.drift_slip_soft) / (t.drift_slip_cap - t.drift_slip_soft);
                    rate *= 1.0 - over.clamp(0.0, 1.0);
                }
            }
            self.yaw_rate = rate;
            self.yaw = wrap_angle(self.yaw + rate * dt);
        }

        // 5. Grip: lateral velocity decays in the new heading frame.
        let f2 = forward(self.yaw);
        let r2 = right(self.yaw);
        let mut vl_after = 0.0;
        if self.on_ground {
            let vf2 = hv.dot(f2);
            let vl2 = hv.dot(r2);
            let g = if self.stun_t > 0.0 {
                2.0
            } else if sliding {
                let slide_grip = if self.drift.active {
                    // Like wheelspin: the grip breaks away as far as the drift is
                    // turned in, and comes back as it unwinds. Counter-steering
                    // firms it up further so the slip comes down quickly.
                    let counter = (-steer * self.drift.dir as f32).clamp(0.0, 1.0);
                    lerp(t.grip, t.drift_grip, self.drift.looseness(t))
                        * (1.0 + (t.drift_counter_grip - 1.0) * counter)
                } else {
                    t.drift_grip
                };
                lerp(t.grip, slide_grip, blend)
            } else {
                t.grip
            };
            let sp_before = hv.length();
            let vl3 = vl2 * (-g * dt).exp();
            let mut vf3 = vf2;
            if self.stun_t <= 0.0 && vf2.abs() > 1.0 {
                vf3 += vf2.signum() * (vl2.abs() - vl3.abs()) * t.grip_redirect;
                if vf3 > vf2 {
                    vf3 = vf3.min(top.max(vf2));
                }
            }
            hv = f2 * vf3 + r2 * vl3;
            vl_after = vl3;
            // Grip turns a slide, it does not add speed to it.
            let sp = hv.length();
            if sliding && sp > sp_before && sp > 1e-4 {
                hv *= sp_before / sp;
            }
            // A slide carries about top speed, not more: the sideways speed would
            // otherwise add on top of the forward speed.
            let sp = hv.length();
            let cap = top * t.drift_speed_cap;
            if sp > cap {
                hv *= 1.0 - (1.0 - cap / sp) * (t.drift_cap_rate * dt * blend).min(1.0);
            }
        }

        // 6. Drift. Speed, not forward speed: a slide turns the heading away from the motion.
        let hspeed = hv.length();
        // Slip: the angle between the heading and the way we are going.
        let slip = if hspeed > 1.0 {
            crate::math::atan2(hv.dot(r2).abs(), hv.dot(f2))
        } else {
            0.0
        };
        // Over-rotated (sustained, at speed): the only way a drift spins out.
        if self.drift.active
            && self.on_ground
            && self.stun_t <= 0.0
            && hspeed > t.drift_spin_speed
            && slip > t.drift_spin_slip
        {
            self.drift.over_ticks = self.drift.over_ticks.saturating_add(1);
            if f32::from(self.drift.over_ticks) * dt >= t.drift_spin_hold - 1e-4 {
                self.spin_out(t.drift_spin_time);
            }
        } else if self.drift.active
            && input.drift
            && self.drift.tier > 0
            && steer.abs() < 0.15
            && slip < t.drift_boost_slip
        {
            // Still held but straightened up: counts towards the straight-line payoff.
            self.drift.over_ticks = self.drift.over_ticks.saturating_add(1);
        } else {
            self.drift.over_ticks = 0;
        }
        if self.drift.active {
            // Straightened up with the button still held: the charge drives you on.
            let straightened = slip < t.drift_boost_slip
                && f32::from(self.drift.over_ticks) * dt >= t.drift_straight_hold - 1e-4;
            let end = !input.drift
                || straightened
                || (self.on_ground && hspeed < t.drift_min_speed)
                || self.stun_t > 0.0;
            if end {
                if (!input.drift || straightened) && self.drift.tier > 0 && self.stun_t <= 0.0 {
                    // Held long enough: the exit boost fires once the slip has come down.
                    self.drift.pending = self.drift.tier;
                    self.drift.pending_ticks = (t.drift_boost_hold / dt).round().clamp(1.0, 255.0) as u8;
                    self.drift.pending_charge_q = (self.drift.charge * 10.0).round().clamp(0.0, 255.0) as u8;
                }
                self.drift.release();
            } else {
                // Sliding straight earns nothing: once eased in, the charge builds
                // only while there is slip.
                if slip > t.drift_charge_slip || self.drift.blend_q < 255 {
                    self.drift.charge += dt;
                }
                let tier = t
                    .drift_tiers
                    .iter()
                    .filter(|&&x| self.drift.charge + 1e-5 >= x)
                    .count() as u8;
                if tier > self.drift.tier {
                    self.drift.tier = tier;
                    fx.drift_tier = tier;
                }
            }
        }
        // The exit boost: waits (up to the hold time) for the slip to come down.
        if self.drift.pending > 0 {
            self.drift.pending_ticks = self.drift.pending_ticks.saturating_sub(1);
            if self.stun_t > 0.0 || self.drift.pending_ticks == 0 {
                self.drift.pending = 0;
            } else if slip < t.drift_boost_slip {
                let tier = self.drift.pending.min(3);
                let i = tier as usize - 1;
                // A boost already running keeps the stronger of the two.
                if self.boost_t <= 0.0 || self.boost_power(t) < t.drift_boost_power[i] {
                    self.boost_tier = if t.drift_boost_power[i] >= 1.0 { 0 } else { tier };
                }
                if t.drift_boost_time[i] > self.boost_t {
                    self.effects.feather = 0.0;
                }
                self.boost_t = self.boost_t.max(t.drift_boost_time[i]);
                fx.drift_boost = tier;
                fx.drift_points = f32::from(self.drift.pending_charge_q) * 0.1 * f32::from(tier);
                self.drift.pending = 0;
            }
            if self.drift.pending == 0 {
                self.drift.pending_ticks = 0;
                self.drift.pending_charge_q = 0;
            }
        }
        // Not a hop: pressed, it stays armed while held (in the air until landing),
        // and only a real steer picks the side, so going straight it never turns.
        if drift_edge && self.stun_t <= 0.0 && !self.drift.active {
            self.drift.hop = true;
            self.drift.charge = 0.0;
        }
        if self.drift.hop {
            if self.stun_t > 0.0 {
                self.drift = Drift::default();
            } else if !input.drift {
                self.drift.release();
            } else if self.on_ground && steer.abs() > 0.25 && hspeed > t.drift_min_speed {
                let dir = steer.signum() as i8;
                // It starts from the turn the steering already gives: no snap.
                let carry = Drift::steer_target(steer.abs(), self.drift.blend(), t);
                self.drift = Drift {
                    active: true,
                    hop: false,
                    dir,
                    charge: 0.0,
                    tier: 0,
                    carry_q: (carry * 100.0).round().clamp(0.0, 255.0) as u8,
                    over_ticks: 0,
                    ..self.drift
                };
                fx.drift_start = true;
            }
        }
        // The wheelspin takes its speed over the ease-in rather than in one jolt.
        if self.drift.active && self.drift.blend_q < 255 {
            hv *= (1.0 - t.drift_scrub).powf(dt / t.drift_ease_in.max(0.05));
        }
        let mut vy = self.vel.y;

        // 7. Jump / trick.
        if jump_edge && self.stun_t <= 0.0 {
            if self.on_ground {
                if on_ramp {
                    self.trick_armed = t.trick_window;
                } else {
                    vy = t.jump_vy;
                    self.on_ground = false;
                    self.air_t = 0.0;
                    self.air_peak = self.pos.y;
                    self.from_ramp = false;
                    fx.jumped = true;
                }
            } else if self.from_ramp && self.air_t < 0.15 && !self.trick_pending {
                self.start_trick(&mut fx);
            }
        }

        // 8-9. Move, walls, ground.
        let y_prev = self.pos.y;
        let vy_prev = self.vel.y;
        let (x_prev, z_prev) = (self.pos.x, self.pos.z);
        self.pos.x += hv.x * dt;
        self.pos.z += hv.z * dt;
        let mut proj = track.project(self.pos, proj0.idx);
        if proj.ground_y - y_prev > 1.0 {
            // A cliff face (the far side of a gap, seen from below): blocked.
            self.pos.x = x_prev;
            self.pos.z = z_prev;
            hv *= -0.1;
            proj = track.project(self.pos, proj0.idx);
            fx.wall_hit = 1.0;
        }
        let hb = track.half_band(proj.s, proj.lateral);
        let lim = (hb - 0.45).max(0.5);
        if proj.lateral.abs() > lim {
            let side = proj.lateral.signum();
            let n = track.sample_at(proj.s).right * side;
            let push = proj.lateral.abs() - lim;
            self.pos -= n * push;
            let vn = hv.dot(n);
            let sp = hv.length();
            if vn > 0.0 && sp > 1e-3 {
                let head_on = vn / sp;
                // Only the into-wall component is lost (partly rebounded): the tangential
                // speed is kept, so a racer pressed into a wall slides along it. A crash
                // from open road (not already against the wall) also costs a little speed.
                hv -= n * vn * (1.0 + t.wall_bounce);
                if track.half_band(proj0.s, proj0.lateral) - proj0.lateral.abs() > t.wall_turn_reach + 0.45 {
                    hv *= 1.0 - t.wall_scrub * head_on;
                }
                fx.wall_hit = head_on;
            }
            // Pointing into the wall: ease the nose round to run along it, so a racer
            // that drove (or is pushed) into a wall is never pinned facing it.
            let fwd = forward(self.yaw);
            let fn_ = fwd.dot(n);
            if self.on_ground && self.stun_t <= 0.0 && fn_ > 0.02 && t.wall_glance > 0.0 {
                let along = flat(fwd - n * fn_);
                if along.length_squared() > 1e-6 {
                    let want = yaw_of(along);
                    let diff = wrap_angle(want - self.yaw);
                    self.yaw = wrap_angle(self.yaw + diff.clamp(-t.wall_glance * dt, t.wall_glance * dt));
                }
            }
            proj = track.project(self.pos, proj.idx);
        }
        self.track_idx = proj.idx as u32;
        self.s = proj.s;
        self.lateral = proj.lateral;
        let g = proj.ground_y;
        self.ground_y = g;

        if self.on_ground {
            let y_ball = y_prev + vy_prev * dt - 0.5 * t.gravity * dt * dt;
            if g >= y_ball - 0.03 {
                vy = ((g - y_prev) / dt).clamp(-40.0, 40.0);
                self.pos.y = g;
            } else {
                // The ground fell away faster than we do: airborne.
                self.on_ground = false;
                vy = vy_prev - t.gravity * dt;
                self.pos.y = y_ball;
                self.air_t = 0.0;
                self.air_peak = self.pos.y;
                self.from_ramp = sm0.surface == Surface::Ramp || sm0.ramp > 0.0;
                if self.from_ramp && self.trick_armed > 0.0 {
                    self.start_trick(&mut fx);
                }
            }
        } else {
            // Wings out: past the apex, jump held, falls slowly. Not mid-trick (that
            // press is the trick's), not stunned or tumbling. Rising is untouched.
            let glide = vy < 0.0
                && input.jump
                && self.stun_t <= 0.0
                && self.effects.tumble <= 0.0
                && !self.trick_pending;
            vy = if glide {
                crate::playful::glide_vy(vy, t.gravity, dt)
            } else {
                vy - t.gravity * dt
            };
            self.pos.y += vy * dt;
            self.air_t += dt;
            self.air_peak = self.air_peak.max(self.pos.y);
            if self.pos.y <= g && (vy <= 0.0 || self.air_t > 0.05) {
                let fall = self.air_peak - g;
                self.pos.y = g;
                vy = 0.0;
                self.on_ground = true;
                fx.landed = true;
                fx.hard_landing = fall > t.hard_landing;
                self.landing_t = 0.3;
                if self.trick_pending {
                    self.trick_pending = false;
                    self.grant_boost(t.trick_boost);
                    fx.trick_boost = true;
                }
            }
        }
        self.vel = Vec3::new(hv.x, vy, hv.z);
        self.air_h = (self.pos.y - g).max(0.0);

        if self.pos.y < track.kill_y {
            self.respawn_t = t.respawn_fade;
            self.drift = Drift::default();
            fx.fell = true;
        }

        // Boost pads.
        if self.on_ground {
            for f in &track.features {
                if let Feature::BoostPad { s, lateral, len } = *f {
                    if track.s_in(self.s, s, len)
                        && (self.lateral - lateral).abs() <= 0.5 * if track.id == crate::TrackId::Custom { sm0.width } else { PAD_WIDTH }
                    {
                        if self.boost_t < t.boost_pad_time - 0.1 {
                            fx.boost_pad = true;
                        }
                        self.grant_boost(t.boost_pad_time);
                    }
                }
            }
        }

        // Presentation: speed, body angle, gait, dust.
        self.speed = hv.dot(f2);
        // The body angle is an eased offset from the heading (no lag while turning).
        let target_off = -(self.drift.dir as f32) * t.drift_body_angle * self.drift.blend();
        let off = wrap_angle(self.visual_yaw - prev_yaw);
        let off = if self.stun_t > 0.0 {
            0.0
        } else {
            off + (target_off - off) * (12.0 * dt).min(1.0)
        };
        self.visual_yaw = wrap_angle(self.yaw + off);
        if self.on_ground && self.speed.abs() > 0.3 {
            let old = self.gait_phase;
            let k = (self.speed.abs() / t.top_speed).clamp(0.0, 1.0);
            let stride = t.stride + (t.stride_fast - t.stride) * k;
            self.gait_phase = (self.gait_phase + self.speed.abs() * dt / stride).fract();
            if (old < 0.5 && self.gait_phase >= 0.5) || self.gait_phase < old {
                fx.footstep = true;
            }
        }
        self.dust = if self.on_ground {
            let sp = (self.speed.abs() / base_top).clamp(0.0, 1.0);
            let surf = if self.offroad { 1.0 } else { 0.25 };
            let drift = 0.6 * self.drift.blend();
            let burst = if self.landing_t > 0.0 { 1.0 } else { 0.0 };
            (sp * surf + vl_after.abs() / 6.0 + drift + burst).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.chase_step(old_pos, &raw_input, track, dt);
        fx
    }

    fn start_trick(&mut self, fx: &mut MotionFx) {
        self.trick_pending = true;
        self.trick_t = 0.6;
        self.trick_armed = 0.0;
        fx.trick = true;
    }

    /// Places the racer back on the centreline `respawn_back` metres behind its
    /// last position (before any gap's run-up), at `respawn_speed` x top speed.
    pub fn respawn(&mut self, track: &Track, t: &Tunables) {
        let mut s = self.s - t.respawn_back;
        for f in &track.features {
            if let Feature::Gap { s: gs, len } = *f {
                if track.s_in(s, gs - 70.0, len + 75.0) {
                    s = gs - 70.0;
                }
            }
        }
        let s = track.wrap_s(s);
        let sm = track.sample_at(s);
        let idx = track.idx_at(s);
        let mut p = sm.pos;
        let pr = track.project(p, idx);
        p.y = pr.ground_y;
        self.pos = p;
        self.yaw = yaw_of(sm.tangent);
        self.visual_yaw = self.yaw;
        let top = t.top_speed * self.class.mods().top * self.speed_scale;
        self.vel = forward(self.yaw) * top * t.respawn_speed;
        self.speed = top * t.respawn_speed;
        self.on_ground = true;
        self.air_h = 0.0;
        self.air_peak = p.y;
        self.track_idx = pr.idx as u32;
        self.s = pr.s;
        self.lateral = pr.lateral;
        self.ground_y = pr.ground_y;
        self.drift = Drift::default();
        self.stun_t = 0.0;
        self.trick_pending = false;
        self.respawn_t = 0.0;
    }

    /// Compact network form (others in a snapshot).
    pub fn to_net(&self) -> RacerNet {
        let q16 = |v: f32| (v * 100.0).round().clamp(-32767.0, 32767.0) as i16;
        let cs = |v: f32| (v * 100.0).round().clamp(0.0, 255.0) as u8;
        let ds = |v: f32| (v * 10.0).round().clamp(0.0, 255.0) as u8;
        let mut flags = 0u16;
        let bits = [
            self.on_ground,
            self.laser.firing,
            self.laser.overheated,
            self.wrong_way,
            self.drift.active,
            self.drift.hop,
            self.mega_t > 0.0,
            self.magnet_t > 0.0,
            self.finished_at.is_some(),
            self.respawn_t > 0.0,
            self.stumble_t > 0.0,
            self.offroad,
            self.item.is_some(),
            self.landing_t > 0.0,
        ];
        for (i, b) in bits.into_iter().enumerate() {
            if b {
                flags |= 1 << i;
            }
        }
        let dir = match self.drift.dir {
            d if d < 0 => 1u8,
            d if d > 0 => 2u8,
            _ => 0u8,
        };
        // Laser hit relative to the eye, in the head frame.
        let eye = self.eye();
        let d = self.laser.hit - eye;
        let dist = d.length();
        let (lyaw, lpitch) = if dist > 1e-3 {
            let hy = yaw_of(self.head_dir());
            let yo = wrap_angle(yaw_of(d) - hy);
            let pi = (d.y / dist).clamp(-1.0, 1.0).asin();
            (yo, pi)
        } else {
            (0.0, 0.0)
        };
        RacerNet {
            effects: self.effects.pack(),
            attire: self.attire,
            idx: self.idx,
            pos: self.pos.to_array(),
            vel: [q16(self.vel.x), q16(self.vel.y), q16(self.vel.z)],
            yaw: ((wrap_angle(self.yaw) / std::f32::consts::TAU + 0.5) * 65535.0).round() as u16,
            vis_yaw: (wrap_angle(self.visual_yaw - self.yaw) * 100.0)
                .round()
                .clamp(-127.0, 127.0) as i8,
            flags,
            drift: self.drift.tier.min(3) | (dir << 2),
            timers: [
                cs(self.boost_t),
                cs(self.stun_t),
                ds(self.slow_t),
                ds(self.shield_t),
                cs(self.trick_t),
            ],
            bash: (self.bash_anim * 127.0).round().clamp(-127.0, 127.0) as i8,
            look_back: (self.look_back * 255.0).round() as u8,
            gait: (self.gait_phase * 255.0).round() as u8,
            dust: (self.dust * 255.0).round() as u8,
            coins: self.coins,
            lap: self.lap,
            place: self.place,
            laser: [
                (dist / 0.3).round().clamp(0.0, 255.0) as u8,
                (lyaw * 100.0).round().clamp(-127.0, 127.0) as i8 as u8,
                (lpitch * 100.0).round().clamp(-127.0, 127.0) as i8 as u8,
            ],
        }
    }

    /// Applies a [`RacerNet`] onto this racer (for rendering remote racers).
    /// Fields not carried keep their values.
    pub fn apply_net(&mut self, n: &RacerNet) {
        let bit = |i: u16| n.flags & (1 << i) != 0;
        self.effects = crate::playful::Effects::unpack(n.effects);
        self.attire = n.attire;
        self.idx = n.idx;
        self.pos = Vec3::from_array(n.pos);
        self.vel = Vec3::new(n.vel[0] as f32, n.vel[1] as f32, n.vel[2] as f32) / 100.0;
        self.yaw = (n.yaw as f32 / 65535.0 - 0.5) * std::f32::consts::TAU;
        self.visual_yaw = wrap_angle(self.yaw + n.vis_yaw as f32 / 100.0);
        self.speed = flat(self.vel).dot(forward(self.yaw));
        self.on_ground = bit(0);
        self.laser.firing = bit(1);
        self.laser.overheated = bit(2);
        self.wrong_way = bit(3);
        self.drift.active = bit(4);
        self.drift.hop = bit(5);
        self.mega_t = if bit(6) { self.mega_t.max(0.1) } else { 0.0 };
        self.magnet_t = if bit(7) { self.magnet_t.max(0.1) } else { 0.0 };
        if bit(8) && self.finished_at.is_none() {
            self.finished_at = Some(0.0);
        }
        self.respawn_t = if bit(9) { self.respawn_t.max(0.1) } else { 0.0 };
        self.stumble_t = if bit(10) {
            self.stumble_t.max(0.1)
        } else {
            0.0
        };
        self.offroad = bit(11);
        self.landing_t = if bit(13) {
            self.landing_t.max(0.1)
        } else {
            0.0
        };
        self.drift.tier = n.drift & 3;
        self.drift.dir = match (n.drift >> 2) & 3 {
            1 => -1,
            2 => 1,
            _ => 0,
        };
        self.boost_t = n.timers[0] as f32 / 100.0;
        self.stun_t = n.timers[1] as f32 / 100.0;
        self.slow_t = n.timers[2] as f32 / 10.0;
        self.shield_t = n.timers[3] as f32 / 10.0;
        self.trick_t = n.timers[4] as f32 / 100.0;
        self.bash_anim = n.bash as f32 / 127.0;
        self.look_back = n.look_back as f32 / 255.0;
        self.gait_phase = n.gait as f32 / 255.0;
        self.dust = n.dust as f32 / 255.0;
        self.coins = n.coins;
        self.lap = n.lap;
        self.place = n.place;
        self.laser.origin = self.eye();
        let dist = n.laser[0] as f32 * 0.3;
        let hy = yaw_of(self.head_dir()) + (n.laser[1] as i8) as f32 / 100.0;
        let pitch = (n.laser[2] as i8) as f32 / 100.0;
        let dir = forward(hy) * crate::math::cos(pitch) + Vec3::Y * crate::math::sin(pitch);
        self.laser.hit = self.laser.origin + dir * dist;
        self.laser.aim = self.laser.hit;
        self.air_h = 0.0;
    }
}

/// Compact racer state for snapshots (56 bytes with bincode 1 fixed-width options).
/// Positions are f32; velocity in cm/s (i16); yaw as u16 of a full turn;
/// timers in centiseconds (boost, stun, trick) or deciseconds (slow, shield).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RacerNet {
    /// Temporary effects in deciseconds.
    pub effects: [u8; 11],
    /// Visual outfit indices.
    pub attire: [u8; 3],
    /// Racer index.
    pub idx: u8,
    /// Position.
    pub pos: [f32; 3],
    /// Velocity, cm/s.
    pub vel: [i16; 3],
    /// Yaw, 0..65535 = -PI..PI.
    pub yaw: u16,
    /// Visual yaw offset, centiradians.
    pub vis_yaw: i8,
    /// Bit flags: 0 on_ground, 1 laser firing, 2 overheated, 3 wrong way, 4 drifting,
    /// 5 drift hop, 6 mega laser, 7 magnet, 8 finished, 9 respawning, 10 stumbling,
    /// 11 off road, 12 holds an item, 13 landing burst.
    pub flags: u16,
    /// Drift tier (bits 0-1) and dir (bits 2-3: 1 left, 2 right).
    pub drift: u8,
    /// boost cs, stun cs, slow ds, shield ds, trick cs.
    pub timers: [u8; 5],
    /// Reserved legacy pose; omitted from current network/ghost records.
    #[serde(skip)]
    pub bash: i8,
    /// Look back x255.
    pub look_back: u8,
    /// Gait phase x255.
    pub gait: u8,
    /// Dust x255.
    pub dust: u8,
    /// Coins.
    pub coins: u32,
    /// Lap.
    pub lap: u8,
    /// Place.
    pub place: u8,
    /// Laser hit: distance (x0.3 m), yaw offset (i8 crad), pitch (i8 crad).
    pub laser: [u8; 3],
}
