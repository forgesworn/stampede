//! Jumpable rug pulls and the money printer's dismount-and-chase recovery.

use crate::math::{flat, forward, wrap_angle, yaw_of};
use crate::{Event, HitCause, Input, Projectile, ProjectileKind, Racer, Track, World};
use glam::Vec3;
use serde::{Deserialize, Serialize};

/// Maximum simultaneous rug traps, including on custom tracks.
pub const MAX_RUGS: usize = 2;
/// Seconds spent cranking and firing the printer.
pub const PRINTER_TIME: f32 = 5.0;
/// Fall animation clock; race recovery remains active until the jockey jumps on.
pub const DISMOUNT_TIME: f32 = 8.0;
/// Seconds a thrown jockey spends in the air and on the ground before running.
/// Was 0.6; shortened so a typical dismount-to-riding time drops from about
/// 1.85 s to about 1.1 s.
pub const DISMOUNT_FALL: f32 = 0.4;
/// Metres per second the unseated jockey runs back to the ostrich (was 7).
pub const RUN_SPEED: f32 = 10.0;
/// Seconds of knock-off protection once the jockey is back on.
pub const REMOUNT_GUARD: f32 = 1.5;

/// How the jockey came off, which picks the throw and the ostrich's reaction.
/// Carried in the unused part of the trump byte of a remote racer's packed
/// effects, so it costs nothing on the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnockKind {
    /// Not knocked off.
    #[default]
    None,
    /// Dollar bill: stumble, slide off backwards.
    Printer,
    /// Rug yanked away: pitched forward, flung up and forward.
    Rug,
    /// Bowled sideways by Pepe: a roll and a cartwheel.
    Pepe,
    /// Laser Trump smash: spun flat like a coin, jockey thrown straight up.
    Trump,
}

impl KnockKind {
    /// Every real knock-off, in wire order.
    pub const ALL: [KnockKind; 4] = [
        KnockKind::Printer,
        KnockKind::Rug,
        KnockKind::Pepe,
        KnockKind::Trump,
    ];

    /// The come-off a hit cause produces (`None` for hits that only spin the ostrich).
    pub fn from_cause(cause: HitCause) -> Self {
        match cause {
            HitCause::MoneyPrinter => KnockKind::Printer,
            HitCause::RugPull => KnockKind::Rug,
            HitCause::Pepe => KnockKind::Pepe,
            HitCause::LaserTrump => KnockKind::Trump,
            _ => KnockKind::None,
        }
    }

    /// Wire code, 0..=4.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Inverse of [`KnockKind::code`]; unknown codes read as `None`.
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => KnockKind::Printer,
            2 => KnockKind::Rug,
            3 => KnockKind::Pepe,
            4 => KnockKind::Trump,
            _ => KnockKind::None,
        }
    }

    /// Where the thrown jockey lands, in the ostrich's frame: (forward, right) metres.
    /// The distances differ with the cause (rug 11.1 m, Pepe 7.6 m, printer 5.4 m
    /// from the ostrich), so the run back is longest after a rug.
    fn landing(self, side: f32) -> (f32, f32) {
        match self {
            KnockKind::Rug => (11.0, 1.5 * side),
            KnockKind::Pepe => (-1.5, 7.5 * side),
            _ => (-5.0, 2.0 * side),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
/// One player-placed, NPC-operated rug, anchored along the road.
#[serde(default)]
pub struct Rug {
    /// Arc length where the item was placed.
    pub s: f32,
    /// Placer, immune to their own trap (255 for a synthetic fixture).
    pub owner: u8,
    /// Ticks remaining in the pulling animation.
    pub pull: u8,
    /// Roll-out age, capped at 45 ticks.
    pub rest: u16,
    /// The single victim of this trap (255 before it is pulled).
    pub victim: u8,
}

impl Default for Rug {
    fn default() -> Self {
        Self {
            s: 0.0,
            owner: 255,
            pull: 0,
            rest: 0,
            victim: 255,
        }
    }
}

pub(crate) fn bounded_rugs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Rug>, D::Error> {
    struct Rugs;
    impl<'de> serde::de::Visitor<'de> for Rugs {
        type Value = Vec<Rug>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most two rugs")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            if seq.size_hint().unwrap_or(0) > MAX_RUGS {
                return Err(serde::de::Error::custom("too many rugs"));
            }
            let mut rugs = Vec::with_capacity(MAX_RUGS);
            while let Some(rug) = seq.next_element()? {
                if rugs.len() == MAX_RUGS {
                    return Err(serde::de::Error::custom("too many rugs"));
                }
                let rug: Rug = rug;
                if !rug.s.is_finite()
                    || !(0.0..=6553.5).contains(&rug.s)
                    || rug.pull > 36
                    || rug.rest > 45
                    || (rug.owner >= 8 && rug.owner != 255)
                    || (rug.victim >= 8 && rug.victim != 255)
                {
                    return Err(serde::de::Error::custom("invalid rug"));
                }
                rugs.push(rug);
            }
            Ok(rugs)
        }
    }
    d.deserialize_seq(Rugs)
}

/// Position, heading and road width; shared by collisions and presentation.
pub fn rug_site(track: &Track, rug: &Rug) -> (Vec3, f32, f32) {
    let sample = track.sample_at(rug.s);
    (
        track.point_at(rug.s, 0.0),
        yaw_of(sample.tangent),
        sample.width,
    )
}

/// Visible and collidable fraction of the unrolled rug (0..1).
pub fn rug_extension(_time: f32, _index: usize, rug: &Rug) -> f32 {
    if rug.pull > 0 {
        rug.pull as f32 / 36.0
    } else {
        (rug.rest as f32 / 45.0).min(1.0)
    }
}

impl Racer {
    /// Unseated jockey's ground offset, relative to the ostrich.
    pub fn chase_offset(&self) -> Vec3 {
        Vec3::new(self.effects.chase_x, 0.0, self.effects.chase_z)
    }

    /// Throws the jockey off. `side` (+1 right, -1 left) picks the side it lands on.
    pub(crate) fn knock_off(&mut self, kind: KnockKind, side: f32) {
        if self.effects.dismount > 0.0 || self.effects.remount_guard > 0.0 {
            return;
        }
        self.effects.dismount = DISMOUNT_TIME;
        self.effects.knock = kind;
        self.effects.le_mans = false;
        let (ahead, aside) = kind.landing(if side < 0.0 { -1.0 } else { 1.0 });
        let gap = self.forward() * ahead + self.right() * aside;
        // Lands facing the ostrich, so it runs straight back.
        self.effects.chase_yaw = yaw_of(flat(-gap));
        self.effects.chase_x = gap.x;
        self.effects.chase_z = gap.z;
        self.effects.printer = 0.0;
        self.boost_t = 0.0;
        self.effects.feather = 0.0;
        self.drift = Default::default();
        self.trick_pending = false;
        self.trick_t = 0.0;
        self.trick_armed = 0.0;
        self.yaw_rate = 0.0;
        self.visual_yaw = self.yaw;
        self.dust = 0.0;
        self.vel *= 0.2;
        self.speed *= 0.2;
        self.laser.firing = false;
    }

    /// A loose ostrich walks forward along the road, independently of steering.
    pub(crate) fn walk_and_chase(
        &mut self,
        input: &Input,
        previous_jump: bool,
        track: &Track,
        dt: f32,
    ) {
        let old = self.pos;
        if !self.effects.le_mans {
            let projection = track.project(self.pos, self.track_idx as usize);
            self.s = track.wrap_s(projection.s + 3.0 * dt);
            self.lateral = projection.lateral * (1.0 - dt * 2.0);
            self.pos = track.point_at(self.s, self.lateral);
            let at = track.project(self.pos, projection.idx);
            self.track_idx = at.idx as u32;
            self.yaw = yaw_of(track.sample_at(self.s).tangent);
            self.visual_yaw = self.yaw;
            self.vel = (self.pos - old) / dt;
            self.speed = 3.0;
            self.on_ground = true;
            self.air_h = 0.0;
            self.gait_phase = (self.gait_phase + dt * 1.1).fract();
        } else {
            self.vel = Vec3::ZERO;
            self.speed = 0.0;
        }
        let mut action = *input;
        action.jump &= !previous_jump;
        self.chase_step(old, &action, track, dt);
    }

    /// The unseated jockey runs back to the bird by itself; a fresh jump near it mounts.
    pub(crate) fn chase_step(&mut self, old_pos: Vec3, input: &Input, track: &Track, dt: f32) {
        if self.effects.dismount <= 0.0 {
            return;
        }
        let mut runner = old_pos + self.chase_offset();
        let toward = flat(self.pos - runner);
        if self.effects.le_mans || self.effects.dismount < DISMOUNT_TIME - DISMOUNT_FALL {
            let yaw = if self.effects.le_mans {
                yaw_of(toward) - input.steer.clamp(-1.0, 1.0) * 1.1
            } else {
                // Turn towards the ostrich rather than snapping, so the jockey reads as running.
                let turn = wrap_angle(yaw_of(toward) - self.effects.chase_yaw);
                let step = 8.0 * dt;
                self.effects.chase_yaw =
                    wrap_angle(self.effects.chase_yaw + turn.clamp(-step, step));
                self.effects.chase_yaw
            };
            let direction = forward(yaw);
            // Stop alongside the mount instead of running through it without jumping.
            let distance = (toward.length() - 1.2).max(0.0);
            let speed = if self.effects.le_mans { 14.0 } else { RUN_SPEED };
            runner += direction * (speed * dt).min(distance);
        }
        let pr = track.project(runner, self.track_idx as usize);
        let limit = (track.half_band(pr.s, pr.lateral) - 0.6).max(0.5);
        if pr.lateral.abs() > limit {
            runner -= track.sample_at(pr.s).right * (pr.lateral - pr.lateral.clamp(-limit, limit));
        }
        let offset = flat(runner - self.pos);
        self.effects.chase_x = offset.x;
        self.effects.chase_z = offset.z;
        if input.jump && offset.length() < 2.0 {
            self.effects.dismount = 0.0;
            self.effects.chase_x = 0.0;
            self.effects.chase_z = 0.0;
            self.effects.chase_yaw = 0.0;
            self.effects.le_mans = false;
            self.effects.remount_guard = REMOUNT_GUARD;
        }
    }
}

impl World {
    /// A rugged racer loses every coin it holds. The rugger gains exactly that
    /// many (the total is conserved); with no living rugger the coins scatter
    /// loose on the road instead. Banked savings are never in the simulation.
    fn rugged_coins(&mut self, victim: usize, rugger: Option<u8>) {
        let held = self.racers[victim].coins;
        if held == 0 {
            return;
        }
        match rugger.map(usize::from).filter(|&to| to != victim && to < self.racers.len()) {
            Some(to) => {
                self.racers[victim].coins = 0;
                self.racers[to].coins = self.racers[to].coins.saturating_add(held);
                self.emit(Event::CoinsTaken {
                    from: victim as u8,
                    to: to as u8,
                    count: held,
                });
            }
            None => {
                // One burst per 255 coins: a drop is counted in a byte.
                while self.racers[victim].coins > 0 {
                    self.drop_coins(victim, self.racers[victim].coins.min(255) as u8);
                }
            }
        }
    }

    pub(crate) fn update_mischief(&mut self) {
        // A deployed rug persists indefinitely, and disappears after its one pull.
        self.play.rugs.retain(|r| r.pull > 1 || r.victim == 255);
        for k in 0..self.play.rugs.len() {
            let (pos, yaw, width) = rug_site(&self.track, &self.play.rugs[k]);
            let rug = &mut self.play.rugs[k];
            if rug.pull > 0 {
                rug.pull -= 1;
                continue;
            }
            rug.rest = (rug.rest + 1).min(45);
            if rug.rest < 45 {
                continue;
            }
            let owner = rug.owner;
            let victim = self
                .racers
                .iter()
                .enumerate()
                .find(|(i, r)| {
                    let d = r.pos - pos;
                    *i != owner as usize
                        && r.finished_at.is_none()
                        && !r.disabled()
                        && r.effects.remount_guard <= 0.0
                        && !r.effects.invincible()
                        && d.dot(forward(yaw)).abs() < 1.6
                        && d.dot(crate::math::right(yaw)).abs() < width * 0.46
                        && r.air_h < 0.8
                        && d.y.abs() < 0.8
                })
                .map(|(i, _)| i);
            if let Some(i) = victim {
                self.play.rugs[k].pull = 36;
                self.play.rugs[k].victim = i as u8;
                let by = (owner < self.racers.len() as u8).then_some(owner);
                if self.shield_check(i, by) {
                    // The rug is yanked: the rider is flung up and forward.
                    let side = if self.racers[i].lateral > 0.0 { -1.0 } else { 1.0 };
                    self.racers[i].knock_off(KnockKind::Rug, side);
                    self.emit(Event::Hit {
                        victim: i as u8,
                        by,
                        cause: HitCause::RugPull,
                    });
                    self.rugged_coins(i, by);
                }
            }
        }
        for i in 0..self.racers.len() {
            let r = &self.racers[i];
            if r.effects.printer <= 0.0
                || r.disabled()
                || r.finished_at.is_some()
                || (self.tick + i as u64 * 7) % 15 != 0
                || self.projectiles.len() >= 24
            {
                continue;
            }
            let target = self
                .racers
                .iter()
                .enumerate()
                .filter(|(j, o)| {
                    *j != i
                        && !self.parked(*j)
                        && !self.immune(*j)
                        && o.finished_at.is_none()
                        && o.respawn_t <= 0.0
                        && o.effects.dismount <= 0.0
                        && o.effects.remount_guard <= 0.0
                        && flat(o.pos - r.pos).normalize_or_zero().dot(r.forward()) >= 0.5
                })
                .map(|(j, o)| (j, flat(o.pos - r.pos).length_squared()))
                .filter(|(_, d)| *d < crate::items::PRINTER_REACH * crate::items::PRINTER_REACH)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(j, _)| j);
            let Some(j) = target else {
                continue;
            };
            let pos = r.body() + Vec3::Y * 0.6;
            let dir = (self.racers[j].body() - pos).normalize_or_zero();
            self.projectiles.push(Projectile {
                kind: ProjectileKind::DollarBill,
                pos: pos + dir * 1.8,
                vel: dir * 52.0,
                owner: i as u8,
                target: Some(j as u8),
                age: 0.0,
                bounces: 0,
                hint: r.track_idx,
                s: r.s,
                lateral: r.lateral,
                id: self.next_id,
            });
            self.next_id = self.next_id.wrapping_add(1);
        }
    }
}
