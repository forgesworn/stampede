//! `race-core`: Stampede's pure race simulation. No IO, no clocks, no threads
//! (beyond a process-wide track cache).
//!
//! * [`math`]: coordinate conventions (`forward`, `right`, `DT`, `TICK_HZ`).
//! * [`Tunables`]: every gameplay number (spec 4-8), serde, `Default` = spec.
//! * [`Track`] / [`TrackDef`] / [`track_defs`]: the nine tracks (two cups of
//!   four, the training ground and two battle arenas).
//! * [`Racer`]: the ostrich; [`Racer::integrate`] is the pure driving model
//!   (clients replay it for prediction); [`RacerNet`] its compact wire form.
//! * [`World`]: a race. [`World::step`] advances one 60 Hz tick; read state
//!   from its public fields, take [`Event`]s with [`World::drain_events`].
//! * [`bot_input`]: the bots. [`LessonState`]: training. [`ChallengeState`]: challenges.
//! * [`BattleState`]: Battle mode "Feather Frenzy" on the two arenas.
//! * [`GhostRecorder`] / [`Ghost`]: Time Trial ghosts (record, store, replay).
//!
//! Coordinates: metres, seconds, radians; right-handed, Y up; yaw 0 faces +Z;
//! positive steer turns right (decreases yaw).

#![deny(missing_docs)]

mod battle;
mod bot;
mod challenge;
pub mod custom;
pub mod drill;
mod ghost;
mod input;
mod items;
mod lesson;
pub mod math;
pub mod mischief;
mod racer;
pub mod playful;
mod track;
mod tracks;
mod tunables;
mod world;

pub use battle::{
    roll_battle_item, BattleRules, BattleState, BATTLE_ODDS, BATTLE_OUT_FADE, BATTLE_ROLLS,
};
pub use bot::{bot_input, update_mem, BotLevel, BotMem};
pub use challenge::{
    ChallengeId, ChallengeState, ELIMINATION_INTERVAL, SNIPER_MISS_PENALTY, SNIPER_MISS_TIME,
};
pub use custom::{
    auto_fill, custom_track_from, place_ok, shape, to_def, Corner, CustomError, CustomFeature, CustomFeatureKind,
    CustomTrack, PlaceError, Recorder, RecorderStatus, ShapeError, Shaped, ShapedCrossing,
};
pub use drill::{DrillEvent, DrillId, DrillState};
pub use ghost::{
    track_hash, tunables_hash, Ghost, GhostError, GhostFrame, GhostPose, GhostRecorder,
    GHOST_EVERY, GHOST_MAGIC, GHOST_VERSION,
};
pub use glam::{Vec2, Vec3};
pub use input::Input;
pub use mischief::KnockKind;
pub use playful::{ActiveItem, PILL_SPEED, TRUMP_REACH, TRUMP_SPEED, TRUMP_TAIL, TRUMP_TIME};
pub use items::{
    gap_factor, odds_bucket, roll_item, trump_chance, CoinSpot, Hazard, HazardKind, Item, ItemRoll,
    LooseCoin, Projectile, ProjectileKind, RollGaps, Target, ITEM_ODDS, ITEM_ROLLS, LASER_REACH, MAX_QUEUE,
    PRINTER_REACH,
};
pub use lesson::{HintControl, LessonId, LessonState};
pub use racer::{
    Drift, Laser, LaserHit, Launch, MotionFx, Racer, RacerNet, BODY_HEIGHT, EYE_FORWARD, EYE_HEIGHT,
};
pub use track::{
    Bridge, Crossing, Feature, LinePoint, Projection, SceneryItem, SceneryKind, Surface, Theme, Track, TrackDef,
    TrackId, TrackPoint, TrackSample, WallStyle, COIN_HEIGHT, HURDLE_HEIGHT, PAD_WIDTH,
    SHOULDER_RISE, TARGET_HEIGHT, TERRAIN_MARGIN,
};
pub use tracks::{track_defs, turtle, Seg};
pub use tunables::{Accessory, CharClass, Character, ClassMods, Tunables, CHARACTERS};
pub use world::{
    grid_slots, Event, HitCause, RaceConfig, RaceMode, RacePhase, RacerKind, RacerSetup, World, GP_POINTS,
};
