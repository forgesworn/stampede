//! Contact eggs, shared items and the harmless frog pratfall.

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::math::{flat, mix64, wrap_angle, yaw_of, DT};
use crate::mischief::KnockKind;
use crate::{Event, Feature, HitCause, Item, RaceMode, Racer, Track, Tunables, World, MAX_QUEUE};

// Laser Trump constants. They live here, not in `Tunables` or `Balance`: new
// fields there would change `tunables_hash` and silently reject every saved ghost.

/// Seconds of rocket.
pub const TRUMP_TIME: f32 = 3.5;
/// Rocket speed, times the racer's normal unboosted class top speed.
pub const TRUMP_SPEED: f32 = 1.75;
/// Orange Pill top speed, times the racer's normal top speed. A constant here for
/// the same reason as the rocket's: `Balance` is hashed into saved ghosts.
pub const PILL_SPEED: f32 = 1.25;
/// Contact radius that smashes a racer aside; wide, so a pass hits what it overtakes.
pub const TRUMP_REACH: f32 = 3.5;
/// Length of the laser beam behind the tail, in metres.
pub const TRUMP_TAIL: f32 = 8.0;
/// Boost carried over when the rocket runs out, so the racer doesn't stall.
pub const TRUMP_EXIT_BOOST: f32 = 1.0;
/// Turn-rate multiplier while rocketing, so it can follow the road at that speed.
pub const TRUMP_TURN: f32 = 2.0;
/// How far ahead along the road the rocket aims, in metres.
pub const TRUMP_LOOK: f32 = 20.0;
/// Seconds to reach full rocket speed from a standstill.
pub const TRUMP_RAMP: f32 = 0.4;
/// Rocket time left once the trumper reaches the top two places.
pub const TRUMP_WIND_DOWN: f32 = 0.4;

/// Seconds the user's "Cake!" line shows.
pub const CAKE_NOTE: f32 = 2.5;
/// Seconds the eater's "Cake time!" line shows, from the first bite.
pub const CAKE_LINE: f32 = 1.5;

// Rose seller constants, here for the same reason as the rocket's.

/// Seconds the rose seller pesters each racer.
pub const ROSE_SALE: f32 = 5.0;
/// Seller running speed, times the fastest class's boosted top speed, so it
/// always catches the leader.
pub const ROSE_SPEED: f32 = 1.5;
/// Seconds the seller takes to run off the road at the end.
pub const ROSE_LEAVE: f32 = 1.5;
/// Most seconds a seller waits beside a racer who is down before moving on.
pub const ROSE_WAIT: f32 = 5.0;
/// How far beside the pestered racer the seller runs, centre to centre (m).
pub const ROSE_SIDE: f32 = 1.4;
/// Sideways shove on the pestered racer, away from the seller (m/s^2).
pub const ROSE_PUSH: f32 = 5.0;
/// Speed the pestered racer loses to the seller, as a fraction per second.
pub const ROSE_DRAG: f32 = 0.3;
/// Seconds the user's "Rose seller sent!" line shows.
pub const ROSE_NOTE: f32 = 2.0;
/// Most rose sellers out at once: one per racer.
pub const MAX_ROSES: usize = 8;
/// Most extra rounds a seller can be asked for by further Roses.
pub const ROSE_MAX_AGAIN: u8 = 3;

// Glide constants: jump held past the apex. Here for the same reason as the rocket's.

/// Gravity while gliding, times normal gravity.
pub const GLIDE_GRAVITY: f32 = 0.25;
/// Fastest fall while gliding, in m/s (negative is down).
pub const GLIDE_MAX_FALL: f32 = -3.0;
/// m/s^2 of wing-flare when the fall is already faster than the glide's cap.
pub const GLIDE_FLARE: f32 = 40.0;

/// One tick of falling speed while gliding: light gravity, capped. A fall already
/// faster than the cap is eased down to it rather than snapped.
pub fn glide_vy(vy: f32, gravity: f32, dt: f32) -> f32 {
    let v = vy - gravity * GLIDE_GRAVITY * dt;
    if v >= GLIDE_MAX_FALL {
        v
    } else {
        (vy + GLIDE_FLARE * dt).min(GLIDE_MAX_FALL)
    }
}

/// Initial playtest values, in seconds and speed multipliers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Balance {
    /// Tail fire duration.
    pub burn_time: f32,
    /// Burning speed fraction.
    pub burn_speed: f32,
    /// Screen obstruction duration.
    pub splat_time: f32,
    /// Invincible, faster Orange Pill duration.
    pub pill_time: f32,
    /// Global egg inversion duration; another whisk refreshes it.
    pub scramble_time: f32,
    /// Laptop typing animation duration.
    pub laptop_time: f32,
    /// Shared eating duration.
    pub cake_time: f32,
    /// Unused since the cake stopped changing speed. Kept so `tunables_hash`,
    /// and with it every saved ghost, stays the same.
    pub cake_speed: f32,
    /// Unused, kept for the same reason as `cake_speed`.
    pub cake_boost_time: f32,
    /// Tumble, laughter and getting back up, together.
    pub tumble_time: f32,
}

impl Default for Balance {
    fn default() -> Self {
        Self {
            burn_time: 4.0,
            burn_speed: 0.7,
            splat_time: 3.0,
            pill_time: 7.0,
            scramble_time: 8.0,
            laptop_time: 2.0,
            cake_time: 2.5,
            cake_speed: 0.85,
            cake_boost_time: 3.0,
            tumble_time: 2.8,
        }
    }
}

/// What the user's own last item did, for a line on their screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemNote {
    /// Nothing to say.
    #[default]
    None,
    /// A Shared Cake took this many power-ups.
    Cake(u8),
    /// A rose seller was sent.
    Rose,
}

/// Racer effects; only race Bitcoin belongs in the simulation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Effects {
    /// Tail fire.
    pub burn: f32,
    /// Fried egg and shell pieces.
    pub splat: f32,
    /// Invincible, faster Orange Pill (the player steers).
    pub pill: f32,
    /// Laser Trump rocket time left (0 = off). Invincible, autopiloted, smashing.
    pub trump: f32,
    /// Jockey typing.
    pub laptop: f32,
    /// Jockey eating (someone else's Shared Cake). The buttons are locked; the
    /// speed is not touched.
    pub cake_eat: f32,
    /// Being pestered by a rose seller: seconds of the sale left. The host sets
    /// it every tick of the sale.
    pub rose: f32,
    /// Which side of the racer the rose seller is on (-1 left, +1 right, 0 none).
    /// `Racer::integrate` leans the racer away from it while `rose` runs, so
    /// clients predict the shove. Full own racer only, like `rose`'s exact value.
    pub rose_side: i8,
    /// Turbo Feather boost time left (the boost itself is `Racer::boost_t`), so a
    /// Shared Cake can tell a feather from a pad. Host-only, like
    /// `Racer::trump_hit`: a host migration carries it beside the world.
    #[serde(skip)]
    pub feather: f32,
    /// Frog collision recovery.
    pub tumble: f32,
    /// Remaining printer firing window.
    pub printer: f32,
    /// Unseated jockey recovery; steering controls the running jockey.
    pub dismount: f32,
    /// Brief protection against another bill after mounting.
    pub remount_guard: f32,
    /// Jockey's ground offset from the ostrich, in world metres.
    pub chase_x: f32,
    /// Jockey's world-Z offset from the ostrich.
    pub chase_z: f32,
    /// Independent heading of the running jockey.
    pub chase_yaw: f32,
    /// Waiting on foot for the opening Le Mans run and jump.
    pub le_mans: bool,
    /// How the rider came off while unseated (or, for a Laser Trump smash,
    /// while spun out). Picks the throw and the ostrich's reaction.
    pub knock: KnockKind,
    /// A line for the user about the item they just used, and `note_t`, the
    /// seconds it still shows. Full own racer only: remote racers never send it.
    pub note: ItemNote,
    /// Seconds `note` still shows.
    pub note_t: f32,
    /// This racer's own Egg Scrambler is running (the timer itself is global,
    /// `PlayState::scramble`). Full own racer only.
    pub scramble_mine: bool,
}

impl Effects {
    /// Whether the eater's "Cake time!" line shows: only in the first
    /// [`CAKE_LINE`] seconds of a cake of `cake_time`, so it appears once per
    /// cake, is gone before the cake is, and never stacks.
    pub fn cake_line(&self, cake_time: f32) -> bool {
        self.cake_eat > (cake_time - CAKE_LINE).max(0.0)
    }

    /// Advance timers.
    pub fn step(&mut self, dt: f32, _b: &Balance) {
        for v in [
            &mut self.burn,
            &mut self.splat,
            &mut self.pill,
            &mut self.trump,
            &mut self.laptop,
            &mut self.cake_eat,
            &mut self.rose,
            &mut self.feather,
            &mut self.tumble,
            &mut self.printer,
            &mut self.remount_guard,
            &mut self.note_t,
        ] {
            *v = (*v - dt).max(0.0);
        }
        if self.note_t <= 0.0 {
            self.note = ItemNote::None;
        }
        if self.rose <= 0.0 {
            self.rose_side = 0;
        }
        if self.dismount > 0.0 {
            self.dismount = (self.dismount - dt).max(if self.le_mans { 0.0 } else { 0.1 });
        }
        if self.dismount <= 0.0 {
            self.chase_x = 0.0;
            self.chase_z = 0.0;
            self.chase_yaw = 0.0;
            self.le_mans = false;
        }
    }

    /// Whether attacks, hazards and bumps pass through (orange pill or Laser Trump).
    pub fn invincible(&self) -> bool {
        self.pill > 0.0 || self.trump > 0.0
    }

    /// Remote presentation: half-second burn/splat, decisecond timers, and
    /// 1/8-metre chase offsets. Unseating cancels printing, so those share a byte.
    /// The last byte is the Laser Trump timer (0..=99 deciseconds), or, when no
    /// rocket is running, 100 plus the [`KnockKind`] code.
    /// The full own racer retains exact floats for motion replay.
    pub fn pack(self) -> [u8; 11] {
        let mut packed = [0; 11];
        packed[0] = (self.burn * 2.0).ceil().clamp(0.0, 15.0) as u8
            | ((self.splat * 2.0).ceil().clamp(0.0, 15.0) as u8) << 4;
        let timers = [
            self.pill,
            self.laptop,
            self.cake_eat,
            self.rose,
            self.tumble,
        ]
        .map(|v| (v * 10.0).ceil().clamp(0.0, 255.0) as u8);
        packed[1..6].copy_from_slice(&timers);
        // 0..95: printing; 96..175: chase; 176..255: Le Mans run.
        packed[6] = if self.dismount > 0.0 {
            (if self.le_mans { 175 } else { 95 })
                + (self.dismount * 10.0).ceil().clamp(1.0, 80.0) as u8
        } else {
            (self.printer * 10.0).ceil().clamp(0.0, 95.0) as u8
        };
        packed[7] = (self.chase_x * 8.0).round().clamp(-127.0, 127.0) as i8 as u8;
        packed[8] = (self.chase_z * 8.0).round().clamp(-127.0, 127.0) as i8 as u8;
        packed[9] = (self.chase_yaw / std::f32::consts::PI * 127.0).round().clamp(-127.0, 127.0) as i8 as u8;
        packed[10] = if self.trump > 0.0 || self.knock == KnockKind::None {
            (self.trump * 10.0).ceil().clamp(0.0, 99.0) as u8
        } else {
            100 + self.knock.code()
        };
        packed
    }

    /// Decode remote presentation timers.
    pub fn unpack(bytes: [u8; 11]) -> Self {
        let v = bytes.map(|x| x as f32 * 0.1);
        Self {
            burn: (bytes[0] & 15) as f32 * 0.5,
            splat: (bytes[0] >> 4) as f32 * 0.5,
            pill: v[1],
            trump: if bytes[10] < 100 { v[10] } else { 0.0 },
            knock: if bytes[10] >= 100 { KnockKind::from_code(bytes[10] - 100) } else { KnockKind::None },
            laptop: v[2],
            cake_eat: v[3],
            rose: v[4],
            // Remote racers are drawn, not predicted: the side is not sent.
            rose_side: 0,
            feather: 0.0,
            tumble: v[5],
            printer: if bytes[6] < 96 { v[6] } else { 0.0 },
            dismount: if bytes[6] >= 176 {
                (bytes[6] - 175) as f32 * 0.1
            } else if bytes[6] >= 96 {
                (bytes[6] - 95) as f32 * 0.1
            } else {
                0.0
            },
            chase_x: bytes[7] as i8 as f32 * 0.125,
            chase_z: bytes[8] as i8 as f32 * 0.125,
            chase_yaw: bytes[9] as i8 as f32 / 127.0 * std::f32::consts::PI,
            le_mans: bytes[6] >= 176,
            remount_guard: 0.0,
            note: ItemNote::None,
            note_t: 0.0,
            scramble_mine: false,
        }
    }
}

/// A timed item a racer fired and that is still running: what the item button
/// counts down, and what blocks the next item from being used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveItem {
    /// The item that is running.
    pub item: Item,
    /// Seconds left.
    pub left: f32,
    /// Seconds it lasts in all (so `left / total` is the radial timer).
    pub total: f32,
}

impl ActiveItem {
    /// Fraction of the item still to run, 0..=1.
    pub fn fraction(&self) -> f32 {
        (self.left / self.total.max(1e-3)).clamp(0.0, 1.0)
    }
}

impl Racer {
    /// The timed item this racer fired that is still running, or `None`. With
    /// several, the one with the most time left. `scramble_left` is the world's scrambler clock. A held
    /// item cannot be used while this is `Some`; the app counts it down on the
    /// item button. Only the user's own items count: the Egg Scrambler affects
    /// everybody, so it is matched by `Effects::scramble_mine`. Shared Cake is
    /// over the moment it is used.
    pub fn active_item(&self, t: &Tunables, scramble_left: f32) -> Option<ActiveItem> {
        let b = &t.playful;
        let e = &self.effects;
        [
            (Item::OrangePill, e.pill, b.pill_time),
            (Item::LaserTrump, e.trump, TRUMP_TIME),
            (Item::MoneyPrinter, e.printer, crate::mischief::PRINTER_TIME),
            (Item::Laptop, e.laptop, b.laptop_time),
            (Item::Magnet, self.magnet_t, t.magnet_time),
            (Item::Shield, self.shield_t, t.shield_time),
            (Item::MegaLaser, self.mega_t, t.mega_time),
            (
                Item::EggScrambler,
                if e.scramble_mine { scramble_left } else { 0.0 },
                b.scramble_time,
            ),
        ]
        .into_iter()
        .filter(|&(_, left, _)| left > 0.0)
        .max_by(|a, c| a.1.total_cmp(&c.1))
        .map(|(item, left, total)| ActiveItem { item, left, total: total.max(left) })
    }
}

/// One moving frog, controlled by the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Frog {
    /// Feet position, including the hop.
    pub pos: Vec3,
    /// Heading.
    pub yaw: f32,
    /// Remaining shared pratfall time.
    pub tumble: f32,
    /// Racer laughing alongside this frog (255 = nobody).
    pub victim: u8,
    /// Seconds of its hopping path spent lying down. The frog resumes the path
    /// from where it fell instead of jumping to where the clock says it should be.
    /// Host-only bookkeeping: clients draw the positions they are sent.
    #[serde(skip)]
    pub paused: f32,
}

/// A rose seller sent by a Rose. Runs to each other racer in turn (nearest
/// first), pesters them for [`ROSE_SALE`] seconds, then runs off the road.
/// Controlled by the host; clients draw what they are sent.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoseSeller {
    /// Feet position.
    pub pos: Vec3,
    /// Heading.
    pub yaw: f32,
    /// Arc length along the track.
    pub s: f32,
    /// Lateral offset from the centreline.
    pub lateral: f32,
    /// Who sent it.
    pub owner: u8,
    /// The racer it is running to or pestering (255: running off).
    pub victim: u8,
    /// Racers already tried, a bit per racer index.
    pub tried: u8,
    /// Further rounds of everyone asked for by more Roses.
    pub again: u8,
    /// Which side of the victim it runs on (-1 left, +1 right).
    pub side: i8,
    /// Seconds of the sale left while alongside; 0 while still running to the
    /// victim, and below 0 while waiting beside one who is down (see
    /// [`ROSE_WAIT`]); running off, seconds until it is gone.
    pub t: f32,
}

impl RoseSeller {
    /// Alongside a victim and selling.
    pub fn selling(&self) -> bool {
        self.victim != 255 && self.t > 0.0
    }

    /// Running off the road at the end.
    pub fn leaving(&self) -> bool {
        self.victim == 255
    }
}

fn bounded_roses<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<RoseSeller>, D::Error> {
    struct Roses;
    impl<'de> serde::de::Visitor<'de> for Roses {
        type Value = Vec<RoseSeller>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most eight rose sellers")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            if seq.size_hint().unwrap_or(0) > MAX_ROSES {
                return Err(serde::de::Error::custom("too many rose sellers"));
            }
            let mut roses = Vec::with_capacity(MAX_ROSES);
            while let Some(r) = seq.next_element::<RoseSeller>()? {
                if roses.len() == MAX_ROSES
                    || !r.pos.is_finite()
                    || !r.yaw.is_finite()
                    || !r.s.is_finite()
                    || !r.lateral.is_finite()
                    || !(-ROSE_WAIT..=ROSE_SALE).contains(&r.t)
                    || r.owner >= 8
                    || (r.victim >= 8 && r.victim != 255)
                    || r.again > ROSE_MAX_AGAIN
                    || r.side.abs() != 1
                {
                    return Err(serde::de::Error::custom("invalid rose seller"));
                }
                roses.push(r);
            }
            Ok(roses)
        }
    }
    d.deserialize_seq(Roses)
}

/// Shared world state carried in snapshots.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayState {
    /// Global egg scrambler time left. A whisk refreshes rather than toggles it.
    pub scramble: f32,
    /// Moving obstacles (three in a race, absent in tutorials and drills).
    #[serde(deserialize_with = "bounded_frogs")]
    pub frogs: Vec<Frog>,
    /// Bounded, player-placed trackside rug traps.
    #[serde(deserialize_with = "crate::mischief::bounded_rugs")]
    pub rugs: Vec<crate::mischief::Rug>,
    /// Rose sellers out on the track, at most one per racer.
    #[serde(default, deserialize_with = "bounded_roses")]
    pub roses: Vec<RoseSeller>,
}

fn bounded_frogs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Frog>, D::Error> {
    struct Frogs;
    impl<'de> serde::de::Visitor<'de> for Frogs {
        type Value = Vec<Frog>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most three frogs")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Vec<Frog>, A::Error> {
            if seq.size_hint().unwrap_or(0) > 3 {
                return Err(serde::de::Error::custom("too many frogs"));
            }
            let mut frogs = Vec::with_capacity(3);
            while let Some(f) = seq.next_element()? {
                if frogs.len() == 3 {
                    return Err(serde::de::Error::custom("too many frogs"));
                }
                frogs.push(f);
            }
            Ok(frogs)
        }
    }
    d.deserialize_seq(Frogs)
}

/// Egg layer's crossing phase, or `None` while off stage.
/// Crosses in 1.6 seconds every twelve seconds, with staggered row starts.
pub fn laying_phase(time: f32, row: u16) -> Option<f32> {
    let t = time - 0.5 - row as f32 * 0.31;
    if t < 0.0 {
        return None;
    }
    let p = t.rem_euclid(12.0) / 1.6;
    (p <= 1.0).then_some(p)
}

/// Course-following steering for the running jockey, replayable by clients.
pub fn autosteer(r: &Racer, track: &Track, t: &Tunables) -> f32 {
    autosteer_look(r, track, t, 7.0 + r.speed.abs() * 0.32)
}

/// [`autosteer`] aiming `look` metres ahead along the course.
pub fn autosteer_look(r: &Racer, track: &Track, t: &Tunables, look: f32) -> f32 {
    let pr = track.project(r.pos, r.track_idx as usize);
    let target = track.point_at(pr.s + look, 0.0);
    let err = wrap_angle(yaw_of(flat(target - r.pos)) - r.yaw);
    let rate = crate::math::lerp(
        t.turn_rate_low,
        t.turn_rate_high,
        (r.speed.abs() / t.top_speed).clamp(0.0, 1.0),
    ) * r.class.mods().turn;
    (-err * 3.8 / rate).clamp(-1.0, 1.0)
}

/// Autopilot for the Laser Trump rocket. The pill's lookahead grows with speed
/// and would cut corners at rocket speed, so the rocket looks a fixed distance
/// ahead instead.
pub fn trump_steer(r: &Racer, track: &Track, t: &Tunables) -> f32 {
    autosteer_look(r, track, t, TRUMP_LOOK)
}

impl World {
    pub(crate) fn playful_item(&mut self, i: usize, item: Item) {
        let b = &self.tun.playful;
        match item {
            Item::Laptop => {
                // No profile or banked balance is available to this code.
                let mut stolen = 0u32;
                for (j, r) in self.racers.iter_mut().enumerate() {
                    if j != i {
                        stolen = stolen.saturating_add(r.coins);
                        r.coins = 0;
                    }
                }
                self.racers[i].coins = self.racers[i].coins.saturating_add(stolen);
                self.racers[i].effects.laptop = b.laptop_time;
            }
            Item::OrangePill => {
                let r = &mut self.racers[i];
                r.effects.pill = b.pill_time;
                r.effects.burn = 0.0;
                r.effects.tumble = 0.0;
                r.effects.dismount = 0.0;
                r.effects.knock = KnockKind::None;
                r.effects.le_mans = false;
                r.effects.chase_x = 0.0;
                r.effects.chase_z = 0.0;
                r.stun_t = 0.0;
                r.slow_t = 0.0;
                r.stumble_t = 0.0;
                r.drift = Default::default();
            }
            Item::LaserTrump => {
                let r = &mut self.racers[i];
                r.effects.trump = TRUMP_TIME;
                r.trump_hit = 0;
                r.effects.burn = 0.0;
                r.effects.tumble = 0.0;
                r.effects.dismount = 0.0;
                r.effects.knock = KnockKind::None;
                r.effects.le_mans = false;
                r.effects.chase_x = 0.0;
                r.effects.chase_z = 0.0;
                r.stun_t = 0.0;
                r.slow_t = 0.0;
                r.stumble_t = 0.0;
                r.drift = Default::default();
                r.trick_pending = false;
                r.trick_armed = 0.0;
            }
            Item::EggScrambler => {
                self.play.scramble = b.scramble_time;
                // A whisk refreshes the clock and moves it to the newest user.
                for (j, r) in self.racers.iter_mut().enumerate() {
                    r.effects.scramble_mine = j == i;
                }
            }
            Item::MoneyPrinter => self.racers[i].effects.printer = crate::mischief::PRINTER_TIME,
            Item::RugPull => {
                self.play.rugs.push(crate::mischief::Rug {
                    s: self.racers[i].s,
                    owner: i as u8,
                    ..Default::default()
                });
            }
            Item::SharedCake => self.shared_cake(i),
            Item::Rose => self.send_rose(i),
            _ => {}
        }
    }

    /// Sends a rose seller from beside the user. With one already out, it goes
    /// round everyone once more when it is done, or at once if it was leaving.
    fn send_rose(&mut self, i: usize) {
        self.racers[i].effects.note = ItemNote::Rose;
        self.racers[i].effects.note_t = ROSE_NOTE;
        if let Some(r) = self.play.roses.iter_mut().find(|r| r.owner as usize == i) {
            if r.leaving() {
                // Called back: the next update picks the nearest from where it is.
                r.victim = r.owner;
                r.tried = 0;
                r.t = 0.0;
            } else {
                r.again = (r.again + 1).min(ROSE_MAX_AGAIN);
            }
            return;
        }
        if i >= 8 || self.play.roses.len() >= MAX_ROSES {
            return;
        }
        let u = &self.racers[i];
        let side: i8 = if u.lateral > 0.0 { -1 } else { 1 };
        let lateral = u.lateral + side as f32 * ROSE_SIDE;
        self.play.roses.push(RoseSeller {
            pos: self.track.point_at(u.s, lateral),
            yaw: u.yaw,
            s: u.s,
            lateral,
            owner: i as u8,
            // The owner is never a victim: the first update picks the nearest.
            victim: i as u8,
            tried: 0,
            again: 0,
            side,
            t: 0.0,
        });
    }

    /// A racer the seller may run to or keep pestering. One who is only down
    /// for now (tumbling, unseated, respawning) still is: see [`Self::rose_ready`].
    fn rose_target(&self, r: &RoseSeller, j: usize) -> bool {
        let Some(o) = self.racers.get(j) else {
            return false;
        };
        j != r.owner as usize
            && j < 8
            && r.tried & (1 << j) == 0
            && o.finished_at.is_none()
            && !o.effects.invincible()
            && !self.parked(j)
    }

    /// Whether a sale to racer `j` can start now. A racer who is down (tumbling,
    /// unseated, respawning) is waited for alongside, not skipped.
    fn rose_ready(&self, j: usize) -> bool {
        self.racers[j].respawn_t <= 0.0 && !self.immune(j)
    }

    /// A seller pestering racer `j` gives up on them (they are eating cake).
    pub(crate) fn rose_moves_on(&mut self, j: usize) {
        if j >= 8 {
            return;
        }
        for r in &mut self.play.roses {
            if r.victim as usize == j {
                r.tried |= 1 << j;
                r.t = 0.0;
            }
        }
        self.racers[j].effects.rose = 0.0;
        self.racers[j].effects.rose_side = 0;
    }

    /// Rose sellers, once per tick: pick the next victim, run to them, pester
    /// them, and at the end run off the road.
    pub(crate) fn update_roses(&mut self) {
        let tun = &self.tun;
        let speed = tun.top_speed * 1.03 * (1.0 + tun.boost_speed) * ROSE_SPEED;
        let mut k = 0;
        while k < self.play.roses.len() {
            let mut r = self.play.roses[k];
            // The racer just served, while a new round picks its first victim.
            let mut last = None;
            // Next victim: the nearest untried racer along the road from here.
            while !r.leaving() && !self.rose_target(&r, r.victim as usize) {
                if r.t > 0.0 {
                    if let Some(old) = self.racers.get_mut(r.victim as usize) {
                        old.effects.rose = 0.0;
                        old.effects.rose_side = 0;
                    }
                }
                if (r.victim as usize) < 8 && r.victim != r.owner {
                    r.tried |= 1 << r.victim;
                }
                let nearest = |skip: Option<usize>| {
                    (0..self.racers.len())
                        .filter(|&j| self.rose_target(&r, j) && Some(j) != skip)
                        .min_by(|&a, &b| {
                            let d = |j: usize| self.track.s_delta(r.s, self.racers[j].s).abs();
                            d(a).total_cmp(&d(b))
                        })
                };
                // A new round starts with someone other than the racer just served,
                // when there is anyone else.
                let next = nearest(last).or_else(|| nearest(None));
                last = None;
                r.t = 0.0;
                match next {
                    Some(j) => {
                        r.victim = j as u8;
                        r.side = if self.racers[j].lateral > 0.0 { -1 } else { 1 };
                    }
                    None if r.again > 0 => {
                        r.again -= 1;
                        last = Some(r.victim as usize).filter(|&j| j < 8 && j != r.owner as usize);
                        r.tried = 0;
                        r.victim = r.owner;
                        if !(0..self.racers.len()).any(|j| self.rose_target(&r, j)) {
                            r.again = 0;
                        }
                    }
                    None => {
                        r.victim = 255;
                        r.t = ROSE_LEAVE;
                    }
                }
            }
            let prev = r.pos;
            if r.leaving() {
                // Off to the side of the road, then gone.
                r.t -= DT;
                if r.t <= 0.0 {
                    self.play.roses.remove(k);
                    continue;
                }
                let out = if r.lateral < 0.0 { -1.0 } else { 1.0 };
                r.s = self.track.wrap_s(r.s + 6.0 * DT);
                r.lateral += out * 9.0 * DT;
                r.pos = self.track.point_at(r.s, r.lateral);
            } else if r.selling() {
                let j = r.victim as usize;
                let v = &mut self.racers[j];
                r.s = v.s;
                r.lateral = v.lateral + r.side as f32 * ROSE_SIDE;
                r.pos = self.track.point_at(r.s, r.lateral);
                // The shove itself is the racer's own (`Racer::integrate`, from
                // `effects.rose` and `rose_side`), so clients predict it.
                r.t -= DT;
                if r.t <= 0.0 {
                    // Sale over: the next victim is picked on the next tick.
                    r.tried |= 1 << j;
                    r.t = 0.0;
                    v.effects.rose = 0.0;
                    v.effects.rose_side = 0;
                } else {
                    v.effects.rose = r.t;
                    v.effects.rose_side = r.side;
                }
            } else {
                // Running to the victim along the road, far faster than any racer.
                let j = r.victim as usize;
                let v = &self.racers[j];
                let goal = v.lateral + r.side as f32 * ROSE_SIDE;
                let ds = self.track.s_delta(r.s, v.s);
                let step = speed * DT;
                r.s = self.track.wrap_s(r.s + ds.clamp(-step, step));
                r.lateral += (goal - r.lateral).clamp(-step * 0.5, step * 0.5);
                r.pos = self.track.point_at(r.s, r.lateral);
                let beside = ds.abs() < 2.0 && (goal - r.lateral).abs() < 1.0;
                if beside && !self.rose_ready(j) {
                    // Waiting for them to get up, but not for ever.
                    r.t -= DT;
                    if r.t <= -ROSE_WAIT {
                        r.tried |= 1 << j;
                        r.t = 0.0;
                    }
                } else if beside {
                    if self.shield_check(j, Some(r.owner)) {
                        r.t = ROSE_SALE;
                        self.racers[j].effects.rose = ROSE_SALE;
                        self.racers[j].effects.rose_side = r.side;
                    } else {
                        // Shielded: on to the next.
                        r.tried |= 1 << j;
                    }
                }
            }
            let moved = flat(r.pos - prev);
            if moved.length_squared() > 1e-6 {
                r.yaw = yaw_of(moved);
            }
            self.play.roses[k] = r;
            k += 1;
        }
    }

    /// Shared Cake: everyone else still racing eats. The user takes their good
    /// power-ups, running or held, into the slot and queue, and their bad luck
    /// ends. A jockey on the ground stays there, and a rose seller moves on.
    /// A racer already eating is left alone: whatever they pick up meanwhile
    /// loads (it only waits for the end of the cake), and a second cake does not
    /// take it, nor stretch the cake they are eating.
    fn shared_cake(&mut self, i: usize) {
        let cake_time = self.tun.playful.cake_time;
        let mut took = 0u8;
        for j in 0..self.racers.len() {
            let o = &self.racers[j];
            if j == i
                || o.finished_at.is_some()
                || o.respawn_t > 0.0
                || o.effects.cake_eat > 0.0
                || self.parked(j)
            {
                continue;
            }
            // What the user still has room for. Anything more stays with the
            // victim: running on, or held where it was. Nothing is lost.
            let u = &self.racers[i];
            let mut room = usize::from(u.item.is_none()) + MAX_QUEUE.saturating_sub(u.queue.len());
            let v = &mut self.racers[j];
            let mut taken = Vec::new();
            // A good power-up running ends and becomes one use for the user.
            if room > 0 && v.effects.feather > 0.0 && v.boost_t > 0.0 {
                v.boost_t = 0.0;
                v.boost_tier = 0;
                v.effects.feather = 0.0;
                taken.push((Item::Feather, 1));
                room -= 1;
            }
            if room > 0 && v.shield_t > 0.0 {
                v.shield_t = 0.0;
                taken.push((Item::Shield, 1));
                room -= 1;
            }
            if room > 0 && v.magnet_t > 0.0 {
                v.magnet_t = 0.0;
                taken.push((Item::Magnet, 1));
                room -= 1;
            }
            if room > 0 && v.effects.pill > 0.0 {
                v.effects.pill = 0.0;
                taken.push((Item::OrangePill, 1));
                room -= 1;
            }
            if room > 0 && v.effects.trump > 0.0 {
                v.effects.trump = 0.0;
                v.trump_hit = 0;
                // Out of rocket, as when it runs out: the speed carries on as a boost.
                v.grant_boost(TRUMP_EXIT_BOOST);
                taken.push((Item::LaserTrump, 1));
                room -= 1;
            }
            // Held good power-ups move across with their uses; the rest stay, in
            // their order (they all fit back, as they were all there before).
            let mut held: Vec<(Item, u8)> = v.item.map(|it| (it, v.item_uses)).into_iter().collect();
            held.extend(v.queue.iter().map(|q| (q.item, q.uses)));
            v.clear_items();
            for (it, uses) in held {
                if it.is_good() && room > 0 {
                    taken.push((it, uses));
                    room -= 1;
                } else {
                    v.stash(it, uses);
                }
            }
            // Bad luck ends.
            v.effects.burn = 0.0;
            v.effects.splat = 0.0;
            v.effects.tumble = 0.0;
            if v.effects.dismount <= 0.0 {
                v.effects.knock = KnockKind::None;
            }
            v.stun_t = 0.0;
            v.slow_t = 0.0;
            v.stumble_t = 0.0;
            v.effects.rose = 0.0;
            v.effects.rose_side = 0;
            v.effects.cake_eat = cake_time;
            for (it, uses) in taken {
                let fits = self.racers[i].stash(it, uses);
                debug_assert!(fits, "the user's room was counted");
                took = took.saturating_add(1);
            }
            self.rose_moves_on(j);
        }
        let r = &mut self.racers[i];
        // Nobody had anything: a Feather, so a cake is never wasted.
        if took == 0 {
            r.stash(Item::Feather, 1);
        }
        r.effects.note = ItemNote::Cake(took);
        r.effects.note_t = CAKE_NOTE;
    }

    /// Laser Trump smash, once per tick: every other racer the rocket touches
    /// (contact circle, or the beam out of its tail) spins out, once per rocket.
    /// Cacti and eggs in its way are swept aside.
    pub(crate) fn update_trump(&mut self) {
        let tun = self.tun.clone();
        let rad = tun.racer_radius;
        for i in 0..self.racers.len() {
            let t = &self.racers[i];
            if t.effects.trump <= 0.0
                || t.respawn_t > 0.0
                || t.finished_at.is_some()
                || self.parked(i)
            {
                continue;
            }
            let (pos, fwd, right) = (t.pos, flat(t.forward()), flat(t.right()));
            self.hazards
                .retain(|h| flat(h.pos - pos).length() >= TRUMP_REACH);
            self.projectiles.retain(|p| {
                p.kind == crate::ProjectileKind::DollarBill
                    || p.owner as usize == i
                    || flat(p.pos - pos).length() >= TRUMP_REACH
            });
            for j in 0..self.racers.len() {
                let o = &self.racers[j];
                // Already spinning, shielded once, immune or gone: left alone, so a
                // rocket hits each victim at most once.
                if j == i
                    || o.respawn_t > 0.0
                    || o.finished_at.is_some()
                    || o.stun_t > 0.0
                    || self.racers[i].trump_hit & (1 << j) != 0
                    || self.parked(j)
                    || self.immune(j)
                    || (o.pos.y - pos.y).abs() >= 1.8
                {
                    continue;
                }
                let rel = flat(o.pos - pos);
                let contact = rel.length() < TRUMP_REACH;
                let along = -rel.dot(fwd);
                let beam = along > 0.0 && along <= rad + TRUMP_TAIL && rel.dot(right).abs() < rad;
                if !contact && !beam {
                    continue;
                }
                self.racers[i].trump_hit |= 1 << j;
                self.spin_hit(
                    j,
                    Some(i as u8),
                    HitCause::LaserTrump,
                    tun.egg_spin,
                    tun.egg_coins,
                );
                if contact && self.racers[j].stun_t > 0.0 {
                    // Thrown clear of the rocket's path.
                    let side = if rel.dot(right) < 0.0 { -1.0 } else { 1.0 };
                    self.racers[j].vel += right * side * 9.0;
                }
            }
        }
    }

    /// The rocket never carries anyone to an easy win: it winds down once the
    /// racer is in the top two, and stops for a finisher.
    pub(crate) fn trump_wind_down(&mut self) {
        let over = self.phase == crate::RacePhase::Finished;
        for r in &mut self.racers {
            if r.effects.trump <= 0.0 {
                continue;
            }
            if over || r.finished_at.is_some() {
                r.effects.trump = 0.0;
            } else if r.place <= 2 {
                r.effects.trump = r.effects.trump.min(TRUMP_WIND_DOWN);
            }
        }
    }

    pub(crate) fn update_playful(&mut self) {
        self.update_roses();
        self.play.scramble = (self.play.scramble - DT).max(0.0);

        if self.play.scramble <= 0.0 {
            for r in &mut self.racers {
                r.effects.scramble_mine = false;
            }
        }
        let now = (self.tick + 1).saturating_sub(self.go_tick) as f32 * DT;
        if self.drill.is_none() {
            // Lay each egg at the instant the ostrich passes its slot. Empty
            // slots stay empty until the next crossing, even on a packet loss.
            let mut row = 0u16;
            for f in &self.track.features {
                if let Feature::TargetRow { .. } = f {
                    let ids: Vec<_> = self
                        .targets
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.row == row)
                        .map(|(k, _)| k)
                        .collect();
                    let offset = 0.5 + row as f32 * 0.31;
                    let time = now - offset;
                    let before = time - DT;
                    for (slot, k) in ids.iter().enumerate() {
                        let drop =
                            0.15 + 1.3 * slot as f32 / (ids.len().saturating_sub(1).max(1)) as f32;
                        let a = ((before - drop) / 12.0).floor();
                        let b = ((time - drop) / 12.0).floor();
                        if time >= drop && a < b {
                            self.targets[*k].alive = true;
                        }
                    }
                    row += 1;
                }
            }
            for k in 0..self.targets.len() {
                if !self.targets[k].alive {
                    continue;
                }
                let pos = self.targets[k].pos;
                let hit = self
                    .racers
                    .iter()
                    .enumerate()
                    .find(|(i, r)| {
                        !self.parked(*i)
                            && !r.disabled()
                            && r.finished_at.is_none()
                            && flat(r.pos - pos).length_squared() < 1.35 * 1.35
                            && (r.pos.y - pos.y).abs() < 1.2
                    })
                    .map(|(i, _)| i);
                if let Some(i) = hit {
                    self.targets[k].alive = false;
                    if self.targets[k].bad ^ (self.play.scramble > 0.0) {
                        if !self.racers[i].effects.invincible() {
                            self.racers[i].effects.splat = self.tun.playful.splat_time;
                        }
                        self.emit(crate::Event::TargetDestroyed {
                            racer: i as u8,
                            target: k as u16,
                            item: None,
                            uses: 0,
                        });
                    } else {
                        self.reward_target(i, k);
                    }
                }
            }
        }
        let enabled =
            matches!(self.cfg.mode, RaceMode::Race | RaceMode::Attract) && self.drill.is_none();
        if enabled && self.play.frogs.is_empty() {
            self.play.frogs = vec![
                Frog {
                    victim: 255,
                    ..Default::default()
                };
                3
            ];
        }
        for k in 0..self.play.frogs.len() {
            let f = &mut self.play.frogs[k];
            if f.tumble > 0.0 {
                f.tumble = (f.tumble - DT).max(0.0);
                f.paused += DT;
                continue;
            }
            if self
                .racers
                .get(f.victim as usize)
                .is_none_or(|r| flat(r.pos - f.pos).length() > 3.0)
            {
                f.victim = 255;
            }
            // Seeded meandering hops rather than random teleports.
            let seed = (mix64(self.cfg.seed ^ k as u64) >> 40) as f32 * 0.000001;
            let t = (now - f.paused) * 1.8 + seed;
            let s = self.track.length * (k as f32 + 0.4) / 3.0 + crate::math::sin(t * 0.24) * 13.0;
            let width = self.track.sample_at(s).width;
            let lat = crate::math::sin(t * 0.38) * (width * 0.5 - 1.0).max(0.0);
            let p = self.track.point_at(s, lat);
            let prev = f.pos;
            f.pos = p + Vec3::Y * crate::math::sin(t * std::f32::consts::PI).abs() * 0.65;
            if (flat(f.pos - prev)).length_squared() > 0.00001 {
                f.yaw = yaw_of(flat(f.pos - prev));
            }
            let hit = self
                .racers
                .iter()
                .enumerate()
                .find(|(i, r)| {
                    *i != f.victim as usize
                        && !r.disabled()
                        && r.effects.remount_guard <= 0.0
                        && !r.effects.invincible()
                        && r.finished_at.is_none()
                        && flat(r.pos - f.pos).length_squared() < 1.7 * 1.7
                        && r.air_h < 0.8
                        && (r.pos.y - f.pos.y).abs() < 1.2
                })
                .map(|(i, _)| i);
            if let Some(i) = hit {
                // Pepe falls over where he stands, then gets up and hops on from there.
                f.pos.y = p.y;
                f.tumble = self.tun.playful.tumble_time;
                f.victim = i as u8;
                let toss = flat(self.racers[i].pos - f.pos);
                let mut side = toss.dot(self.racers[i].right());
                // Away from Pepe, unless that would land the jockey off the road edge.
                let lateral = self.racers[i].lateral;
                if side * lateral > 0.0 && lateral.abs() > 4.0 {
                    side = -side;
                }
                if self.shield_check(i, None) {
                    // The rider is bowled off, away from Pepe. No coin loss.
                    self.racers[i].knock_off(KnockKind::Pepe, side);
                    self.emit(Event::Hit {
                        victim: i as u8,
                        by: None,
                        cause: HitCause::Pepe,
                    });
                }
            }
        }
    }
}
