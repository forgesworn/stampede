//! Every gameplay tunable (spec sections 4-8) in one serde struct, plus the
//! character roster. `Tunables::default()` gives the spec's starting values.

use serde::{Deserialize, Serialize};

/// All tunable gameplay values. Units: metres, seconds, m/s, m/s^2, rad/s.
/// `#[serde(default)]` so a partial JSON on device overrides only what it names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tunables {
    // ---- driving (spec 4.2) ----
    /// Base top speed on road.
    pub top_speed: f32,
    /// Acceleration at standstill; falls linearly toward top speed.
    pub accel: f32,
    /// The linear fall-off of `accel` is floored at this fraction, so top speed
    /// is actually reached (0 = the spec's pure linear curve, which never gets there).
    pub accel_floor: f32,
    /// Deceleration with the throttle released (and the drag above top speed).
    pub coast_drag: f32,
    /// Brake deceleration.
    pub brake: f32,
    /// Max reverse speed.
    pub reverse_max: f32,
    /// Turn rate at standstill (x min(1, speed/5)).
    pub turn_rate_low: f32,
    /// Turn rate at top speed.
    pub turn_rate_high: f32,
    /// Lateral velocity decay rate (1/s).
    pub grip: f32,
    /// Fraction of the lateral speed removed by grip that is handed to forward speed
    /// (grip turns the velocity rather than scrubbing it; 0 = pure scrub).
    pub grip_redirect: f32,
    /// Lateral decay while drifting.
    pub drift_grip: f32,
    /// Minimum speed to start or hold a drift.
    pub drift_min_speed: f32,
    /// Fraction of speed a drift takes over its ease-in (the wheelspin).
    pub drift_scrub: f32,
    /// Drift turn multiplier at full steer into the drift, once eased in. The
    /// drift turn is proportional to the steer: none centred or steering out.
    pub drift_turn_max: f32,
    /// Visual yaw offset of the body while drifting.
    pub drift_body_angle: f32,
    /// Fastest a slide goes, as a fraction of top speed.
    pub drift_speed_cap: f32,
    /// How fast (1/s) a slide over that speed is pulled back to it.
    pub drift_cap_rate: f32,
    /// Ease-in (s): the extra turn and the loose grip build up after the press.
    pub drift_ease_in: f32,
    /// Ease-out (s): grip and carried turn blend back to normal after release.
    pub drift_ease_out: f32,
    /// Time constant (s) of the carried drift turn with the steering centred:
    /// the slide unwinds and the grip comes back.
    pub drift_carry_time: f32,
    /// Time constant (s) of the drift turn following steering into the drift.
    pub drift_steer_time: f32,
    /// Time constant (s) of the drift turn when counter-steering.
    pub drift_counter_time: f32,
    /// Extra grip (times) at full counter-steer: slip comes down quickly.
    pub drift_counter_grip: f32,
    /// Slip angle (rad) where the heading starts to stop swinging round further.
    pub drift_slip_soft: f32,
    /// Slip angle (rad) where it has stopped: steering alone cannot go beyond this.
    pub drift_slip_cap: f32,
    /// Slip angle (rad) above which a drift spins the racer out.
    pub drift_spin_slip: f32,
    /// How long (s) the slip must stay above `drift_spin_slip` to spin out.
    pub drift_spin_hold: f32,
    /// Speed (m/s) below which a drift never spins out.
    pub drift_spin_speed: f32,
    /// Spin-out length (s) when a drift is over-rotated.
    pub drift_spin_time: f32,
    /// Slip angle (rad) below which the drift charge stops building (no
    /// farming boosts by sliding straight).
    pub drift_charge_slip: f32,
    /// Charge (s) for blue / orange / purple sparks (the exit boost tiers).
    pub drift_tiers: [f32; 3],
    /// Exit boost length (s) for tiers 1..3.
    pub drift_boost_time: [f32; 3],
    /// Exit boost strength for tiers 1..3, as a fraction of `boost_speed`.
    pub drift_boost_power: [f32; 3],
    /// Slip angle (rad) under which the exit boost fires.
    pub drift_boost_slip: f32,
    /// How long (s) an exit boost waits for the slip to come down.
    pub drift_boost_hold: f32,
    /// How long (s) a charged drift must run straight, still held, before
    /// the exit boost fires without letting go.
    pub drift_straight_hold: f32,
    /// Top-speed bonus while boosting (0.4 = +40 %).
    pub boost_speed: f32,
    /// Push while boosting below the boosted top speed.
    pub boost_push: f32,
    /// Jump take-off speed.
    pub jump_vy: f32,
    /// Gravity.
    pub gravity: f32,
    /// Turn-rate multiplier in the air.
    pub air_steer: f32,
    /// Top-speed multiplier off road (shoulders).
    pub offroad_top: f32,
    /// Extra drag above the off-road top speed.
    pub offroad_drag: f32,
    /// Lateral restitution on walls.
    pub wall_bounce: f32,
    /// Fraction of speed lost when a racer first meets a wall, x head-on-ness.
    /// Sliding along a wall afterwards costs only the into-wall component.
    pub wall_scrub: f32,
    /// Steering floor (fraction of the turn rate) within `wall_turn_reach` of a wall
    /// at very low speed, so a racer pinned against a wall can always turn away.
    pub wall_turn_floor: f32,
    /// Distance from the wall (m) within which the steering floor applies.
    pub wall_turn_reach: f32,
    /// Rad/s the heading is eased round to run along a wall it is pointing into
    /// (a glancing wall assist; 0 turns it off).
    pub wall_glance: f32,
    /// Top-speed bonus per held coin.
    pub coin_bonus: f32,
    /// Max held coins.
    pub coin_max: u8,
    /// Ostrich-ostrich collision radius.
    pub racer_radius: f32,
    /// Speed decay while spun out.
    pub stun_decel: f32,
    /// Stride length for the gait phase.
    pub stride: f32,
    /// Stride length (m per gait cycle) at top speed; `stride` is the
    /// stride at a standstill. Longer strides at speed, not just faster legs.
    pub stride_fast: f32,
    /// Falls longer than this give a hard landing event.
    pub hard_landing: f32,
    /// Pressing jump this long before leaving a ramp lip is a trick.
    pub trick_window: f32,
    /// Boost on landing a trick.
    pub trick_boost: f32,
    /// Boost from a boost pad.
    pub boost_pad_time: f32,
    /// Throttle in the last this-many seconds of the countdown = start boost.
    pub start_boost_window: f32,
    /// Start boost length.
    pub start_boost_time: f32,
    /// Throttle held longer than this before GO = stumble.
    pub start_stumble_early: f32,
    /// Start stumble length.
    pub start_stumble_time: f32,
    /// Top-speed multiplier while stumbling.
    pub stumble_slow_mult: f32,
    /// Look-back head turn easing rate (1/s).
    pub look_back_rate: f32,

    // ---- interactions (spec 5) ----
    /// Restitution of ostrich-ostrich bumps.
    pub bump_restitution: f32,
    /// No bump when one racer is this much higher.
    pub bump_height: f32,
    /// Bash cooldown after a hit.
    pub bash_cooldown: f32,
    /// Bash cooldown after a whiff.
    pub bash_whiff_cooldown: f32,
    /// Bash lateral reach (min, max) in the attacker's frame.
    pub bash_lateral: [f32; 2],
    /// Bash longitudinal reach (|d| <).
    pub bash_long: f32,
    /// Bash max height difference.
    pub bash_height: f32,
    /// Sideways kick on the victim (x power / mass).
    pub bash_kick: f32,
    /// Victim keeps this fraction of its speed.
    pub bash_speed_keep: f32,
    /// Victim slow time.
    pub bash_slow_time: f32,
    /// Victim slow multiplier.
    pub bash_slow_mult: f32,
    /// Coins the victim drops.
    pub bash_coins: u8,
    /// Attacker recoil speed.
    pub bash_recoil: f32,
    /// Head swing animation length.
    pub bash_anim_time: f32,
    /// Free practice beams for the dedicated laser lesson, drill and challenge.
    pub practice_lasers: bool,
    /// Laser range.
    pub laser_range: f32,
    /// Heat per second firing.
    pub laser_heat_rate: f32,
    /// Cooling per second not firing.
    pub laser_cool_rate: f32,
    /// Lock time once overheated.
    pub laser_overheat_lock: f32,
    /// Beam time to destroy a Power Egg target.
    pub target_kill_time: f32,
    /// Target respawn time.
    pub target_respawn: f32,
    /// Beam time on an opponent to dazzle it.
    pub dazzle_time: f32,
    /// Dazzle slow time.
    pub dazzle_slow_time: f32,
    /// Dazzle slow multiplier.
    pub dazzle_slow_mult: f32,
    /// Dazzle immunity per attacker.
    pub dazzle_immunity: f32,
    /// Turbo Feather boost per use.
    pub feather_boost: f32,
    /// Rolling Egg speed.
    pub egg_speed: f32,
    /// Rolling Egg life.
    pub egg_life: f32,
    /// Rolling Egg wall bounces before it breaks.
    pub egg_bounces: u8,
    /// Spin from an egg hit.
    pub egg_spin: f32,
    /// Coins dropped by an egg hit.
    pub egg_coins: u8,
    /// Hunter Egg speed.
    pub hunter_speed: f32,
    /// Hunter Egg life.
    pub hunter_life: f32,
    /// Hunter Egg steers straight in below this distance.
    pub hunter_home_dist: f32,
    /// Spin from a cactus.
    pub cactus_spin: f32,
    /// Max cacti alive.
    pub cactus_max: usize,
    /// Shield time.
    pub shield_time: f32,
    /// Lightning slow time.
    pub lightning_slow_time: f32,
    /// Lightning slow multiplier.
    pub lightning_slow_mult: f32,
    /// Mega Laser time.
    pub mega_time: f32,
    /// Mega Laser range.
    pub mega_range: f32,
    /// Mega Laser half width.
    pub mega_width: f32,
    /// Spin from the Mega Laser.
    pub mega_spin: f32,
    /// Sat Magnet time.
    pub magnet_time: f32,
    /// Sat Magnet radius.
    pub magnet_radius: f32,
    /// Coin pickup radius.
    pub coin_radius: f32,
    /// Coin respawn time.
    pub coin_respawn: f32,
    /// Mini-boost on a coin pickup.
    pub coin_boost: f32,
    /// Life of a dropped coin.
    pub loose_coin_life: f32,
    /// Stumble time from a log hurdle.
    pub hurdle_stumble: f32,

    /// Playful interactions and item durations.
    pub playful: crate::playful::Balance,

    // ---- race rules (spec 6) ----
    /// Countdown length (3, 2, 1, GO).
    pub countdown: f32,
    /// After the first human finishes, the race ends this much later.
    pub finish_grace: f32,
    /// Respawn fade time.
    pub respawn_fade: f32,
    /// Respawn this far behind the fall point.
    pub respawn_back: f32,
    /// Respawn speed as a fraction of top speed.
    pub respawn_speed: f32,
    /// Driving backwards this long shows WRONG WAY.
    pub wrong_way_time: f32,

    // ---- bots (spec 8) ----
    /// Bot top-speed factor for Easy / Normal / Hard.
    pub bot_speed: [f32; 3],
    /// Max rubber-band bonus when far behind the leading human.
    pub rubber_up: f32,
    /// Max rubber-band penalty when far ahead.
    pub rubber_down: f32,
}

impl Default for Tunables {
    fn default() -> Self {
        Self {
            top_speed: 32.0,
            accel: 24.0,
            accel_floor: 0.25,
            coast_drag: 6.0,
            brake: 38.0,
            reverse_max: 7.0,
            turn_rate_low: 3.5,
            turn_rate_high: 1.85,
            grip: 11.0,
            grip_redirect: 0.9,
            drift_grip: 2.6,
            drift_min_speed: 12.0,
            drift_scrub: 0.05,
            drift_turn_max: 1.40,
            drift_body_angle: 0.42,
            drift_speed_cap: 1.1,
            drift_cap_rate: 8.0,
            drift_ease_in: 0.60,
            drift_ease_out: 0.35,
            drift_carry_time: 0.25,
            drift_steer_time: 0.08,
            drift_counter_time: 0.12,
            drift_counter_grip: 1.5,
            drift_slip_soft: 0.87,
            drift_slip_cap: 1.05,
            drift_spin_slip: 1.13,
            drift_spin_hold: 0.10,
            drift_spin_speed: 14.0,
            drift_spin_time: 1.0,
            drift_charge_slip: 0.10,
            drift_tiers: [0.8, 1.6, 2.4],
            drift_boost_time: [0.4, 0.7, 1.0],
            drift_boost_power: [0.55, 0.8, 1.0],
            drift_boost_slip: 0.26,
            drift_boost_hold: 0.6,
            drift_straight_hold: 0.12,
            boost_speed: 0.4,
            boost_push: 30.0,
            jump_vy: 10.8,
            gravity: 24.0,
            air_steer: 0.35,
            offroad_top: 0.55,
            offroad_drag: 10.0,
            wall_bounce: 0.35,
            wall_scrub: 0.12,
            wall_turn_floor: 0.5,
            wall_turn_reach: 1.2,
            wall_glance: 2.0,
            coin_bonus: 0.012,
            coin_max: 10,
            racer_radius: 0.9,
            stun_decel: 20.0,
            stride: 1.8,
            stride_fast: 6.0,
            hard_landing: 3.0,
            trick_window: 0.25,
            trick_boost: 0.8,
            boost_pad_time: 1.0,
            start_boost_window: 0.4,
            start_boost_time: 1.0,
            start_stumble_early: 1.2,
            start_stumble_time: 0.6,
            stumble_slow_mult: 0.5,
            look_back_rate: 4.0,

            bump_restitution: 0.4,
            bump_height: 1.2,
            bash_cooldown: 0.9,
            bash_whiff_cooldown: 0.5,
            bash_lateral: [0.6, 3.2],
            bash_long: 2.4,
            bash_height: 1.0,
            bash_kick: 7.0,
            bash_speed_keep: 0.7,
            bash_slow_time: 1.2,
            bash_slow_mult: 0.75,
            bash_coins: 2,
            bash_recoil: 2.0,
            bash_anim_time: 0.35,
            practice_lasers: false,
            laser_range: 48.0,
            laser_heat_rate: 0.9,
            laser_cool_rate: 0.6,
            laser_overheat_lock: 1.4,
            target_kill_time: 0.22,
            target_respawn: 4.0,
            dazzle_time: 0.12,
            dazzle_slow_time: 0.5,
            dazzle_slow_mult: 0.9,
            dazzle_immunity: 1.0,
            feather_boost: 1.4,
            egg_speed: 52.0,
            egg_life: 6.0,
            egg_bounces: 3,
            egg_spin: 1.0,
            egg_coins: 3,
            hunter_speed: 50.0,
            hunter_life: 9.0,
            hunter_home_dist: 25.0,
            cactus_spin: 0.8,
            cactus_max: 24,
            shield_time: 8.0,
            lightning_slow_time: 2.5,
            lightning_slow_mult: 0.65,
            mega_time: 2.5,
            mega_range: 60.0,
            mega_width: 2.2,
            mega_spin: 0.8,
            magnet_time: 6.0,
            magnet_radius: 14.0,
            coin_radius: 1.4,
            coin_respawn: 8.0,
            coin_boost: 0.15,
            loose_coin_life: 6.0,
            hurdle_stumble: 0.5,

            playful: crate::playful::Balance::default(),
            countdown: 3.0,
            finish_grace: 25.0,
            respawn_fade: 0.6,
            respawn_back: 10.0,
            respawn_speed: 0.4,
            wrong_way_time: 1.0,

            bot_speed: [0.86, 0.95, 1.0],
            rubber_up: 0.07,
            rubber_down: 0.04,
        }
    }
}

/// Character class (spec 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CharClass {
    /// Quick and agile, fragile.
    Light,
    /// All-rounder.
    Medium,
    /// Fast top end, hits hard, hard to push.
    Heavy,
}

/// Multipliers a class applies to the base tunables.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClassMods {
    /// x `accel`.
    pub accel: f32,
    /// x turn rates.
    pub turn: f32,
    /// x `top_speed`.
    pub top: f32,
    /// Bash power.
    pub bash: f32,
    /// Mass (collisions, bash kicks).
    pub mass: f32,
}

impl CharClass {
    /// The class multipliers (spec 4.2).
    pub fn mods(self) -> ClassMods {
        match self {
            CharClass::Light => ClassMods {
                accel: 1.12,
                turn: 1.10,
                top: 0.97,
                bash: 0.8,
                mass: 0.85,
            },
            CharClass::Medium => ClassMods {
                accel: 1.0,
                turn: 1.0,
                top: 1.0,
                bash: 1.0,
                mass: 1.0,
            },
            CharClass::Heavy => ClassMods {
                accel: 0.90,
                turn: 0.93,
                top: 1.03,
                bash: 1.3,
                mass: 1.25,
            },
        }
    }
}

/// Per-character accessory (the mesh is race-render's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accessory {
    /// Red scarf.
    Scarf,
    /// Goggles.
    Goggles,
    /// Crown.
    Crown,
    /// Star headband.
    StarHeadband,
    /// Viking helmet.
    VikingHelmet,
    /// Mohawk.
    Mohawk,
    /// Gold chain + shades.
    GoldChainShades,
    /// Baseball cap.
    Cap,
}

/// A playable ostrich. Colours are sRGB 0..1. Serialises as its `id` only.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(into = "u8")]
pub struct Character {
    /// Index into [`CHARACTERS`].
    pub id: u8,
    /// Display name.
    pub name: &'static str,
    /// Class.
    pub class: CharClass,
    /// Body feathers.
    pub body: [f32; 3],
    /// Neck.
    pub neck: [f32; 3],
    /// Legs.
    pub legs: [f32; 3],
    /// Beak.
    pub beak: [f32; 3],
    /// Accessory / accent colour.
    pub accent: [f32; 3],
    /// Accessory.
    pub accessory: Accessory,
}

impl From<Character> for u8 {
    fn from(c: Character) -> u8 {
        c.id
    }
}

impl TryFrom<u8> for Character {
    type Error = String;
    fn try_from(id: u8) -> Result<Self, String> {
        CHARACTERS
            .get(id as usize)
            .copied()
            .ok_or_else(|| format!("no character {id}"))
    }
}

impl<'de> Deserialize<'de> for Character {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = u8::deserialize(d)?;
        Character::try_from(id).map_err(serde::de::Error::custom)
    }
}

impl Character {
    /// The character for an id (wraps out-of-range ids).
    pub fn get(id: u8) -> &'static Character {
        &CHARACTERS[id as usize % CHARACTERS.len()]
    }
}

/// The eight ostriches (spec 11).
pub const CHARACTERS: [Character; 8] = [
    Character {
        id: 0,
        name: "Dusty",
        class: CharClass::Medium,
        body: [0.85, 0.72, 0.52],
        neck: [0.93, 0.83, 0.67],
        legs: [0.80, 0.60, 0.46],
        beak: [0.95, 0.74, 0.36],
        accent: [0.86, 0.14, 0.12],
        accessory: Accessory::Scarf,
    },
    Character {
        id: 1,
        name: "Blaze",
        class: CharClass::Light,
        body: [0.08, 0.08, 0.09],
        neck: [0.98, 0.42, 0.08],
        legs: [0.56, 0.36, 0.30],
        beak: [0.95, 0.70, 0.22],
        accent: [1.0, 0.78, 0.10],
        accessory: Accessory::Goggles,
    },
    Character {
        id: 2,
        name: "Satoshi",
        class: CharClass::Medium,
        body: [0.97, 0.58, 0.10],
        neck: [0.97, 0.96, 0.92],
        legs: [0.80, 0.60, 0.45],
        beak: [0.98, 0.85, 0.42],
        accent: [1.0, 0.84, 0.0],
        accessory: Accessory::Crown,
    },
    Character {
        id: 3,
        name: "Nova",
        class: CharClass::Light,
        body: [0.85, 0.15, 0.65],
        neck: [0.98, 0.66, 0.88],
        legs: [0.72, 0.48, 0.58],
        beak: [0.98, 0.80, 0.45],
        accent: [1.0, 0.95, 0.30],
        accessory: Accessory::StarHeadband,
    },
    Character {
        id: 4,
        name: "Boomer",
        class: CharClass::Heavy,
        body: [0.35, 0.40, 0.47],
        neck: [0.60, 0.65, 0.70],
        legs: [0.55, 0.50, 0.48],
        beak: [0.90, 0.72, 0.40],
        accent: [0.78, 0.78, 0.80],
        accessory: Accessory::VikingHelmet,
    },
    Character {
        id: 5,
        name: "Ziggy",
        class: CharClass::Light,
        body: [0.10, 0.65, 0.62],
        neck: [0.56, 0.90, 0.85],
        legs: [0.62, 0.55, 0.50],
        beak: [0.98, 0.78, 0.40],
        accent: [0.98, 0.25, 0.45],
        accessory: Accessory::Mohawk,
    },
    Character {
        id: 6,
        name: "Hodl",
        class: CharClass::Heavy,
        body: [0.30, 0.12, 0.45],
        neck: [0.55, 0.36, 0.70],
        legs: [0.52, 0.42, 0.50],
        beak: [0.95, 0.78, 0.40],
        accent: [1.0, 0.80, 0.15],
        accessory: Accessory::GoldChainShades,
    },
    Character {
        id: 7,
        name: "Pip",
        class: CharClass::Medium,
        body: [0.96, 0.92, 0.80],
        neck: [1.0, 0.97, 0.90],
        legs: [0.82, 0.66, 0.52],
        beak: [0.98, 0.80, 0.45],
        accent: [0.55, 0.78, 0.98],
        accessory: Accessory::Cap,
    },
];
