//! The contract between the menus (`ui/screens`) and gameplay (`game/`).
//!
//! Menus build a [`RaceLaunch`] and push `game::race_screen(launch)`. When a
//! race ends, gameplay calls `ui::screens::after_race::after_race(outcome)`
//! for the next screen (results, the next Grand Prix round, back to the lesson
//! list). Shared 3D backdrops (title attract race, garage turntable, podium)
//! go through `game::stage` (see its docs). Change these types only with the
//! lead's say-so: two teams code against them.

use race_core::{ChallengeId, DrillId, Event, LessonId, RaceConfig, TrackId};

/// What kind of event a race is part of (decides what happens after it).
#[derive(Clone, Debug)]
pub enum RaceKind {
    Quick,
    /// `round` is 0-based into `tracks`; `points[i]` is racer i's total so far
    /// (racer order = `RaceConfig::racers`, the same every round).
    GrandPrix { tracks: Vec<TrackId>, round: usize, points: Vec<u32> },
    TimeTrial,
    Training(LessonId),
    /// One of the five open-ground drills on the Paddock (SPEC 18.2); the race screen runs its own results card.
    Drill(DrillId),
    /// One of the ten Challenges (SPEC 16, 17).
    Challenge(ChallengeId),
    /// Feather Frenzy (SPEC 17.1) against bots, on an arena.
    Battle,
    /// Multiplayer; the session handle lives in the online module.
    Online,
}

#[derive(Clone, Debug)]
pub struct RaceLaunch {
    pub config: RaceConfig,
    pub kind: RaceKind,
    /// Index into `config.racers` of the player on this device.
    pub local_seat: usize,
}

#[derive(Clone, Debug)]
pub struct RacerResult {
    pub name: String,
    pub character: u8,
    /// 1-based.
    pub place: u8,
    /// None if not finished (placed by progress).
    pub time_s: Option<f32>,
    pub best_lap_s: Option<f32>,
    pub coins: u32,
    pub is_local: bool,
    pub is_bot: bool,
    /// Set in a battle: the feathers left, hits landed and taken. `time_s` / `best_lap_s` are None there.
    pub battle: Option<BattleRow>,
}

/// One racer's battle line (`RacerResult::battle`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BattleRow {
    /// Feathers left (0 = out).
    pub feathers: u8,
    /// Feathers this racer popped off others.
    pub hits: u16,
    /// Feathers lost.
    pub taken: u16,
    /// 1 = first out, 2 = second out, ...: the order they went out in; None while still in.
    pub out_order: Option<u8>,
}

#[derive(Clone, Debug)]
pub struct LessonResult {
    pub lesson: LessonId,
    pub done: bool,
    /// 0-3.
    pub stars: u8,
    pub time_s: f32,
}

/// How a Challenge went (`RaceOutcome::challenge`).
#[derive(Clone, Debug)]
pub struct ChallengeResult {
    pub id: ChallengeId,
    /// The score on the challenge's own scale (coins, seconds, place, points, bashes).
    pub score: f32,
    /// 0 none, 1 bronze, 2 silver, 3 gold.
    pub medal: u8,
    /// The challenge reached its end (false after a give-up or a quit).
    pub done: bool,
    /// Elimination: how many rivals went out; egg/time challenges: the counter.
    pub progress: u32,
}

#[derive(Clone, Debug)]
pub struct RaceOutcome {
    pub launch: RaceLaunch,
    /// Sorted by place.
    pub results: Vec<RacerResult>,
    pub lesson: Option<LessonResult>,
    /// Set in `RaceKind::Challenge` races.
    pub challenge: Option<ChallengeResult>,
    /// The player quit from the pause menu.
    pub quit: bool,
    /// Time Trial: what the ghost did with this run.
    pub ghost: Option<GhostResult>,
    /// Set in a battle: the clock and how it ended.
    pub battle: Option<BattleSummary>,
    /// What the local player did during the race (for achievements).
    pub stats: RaceStats,
    /// Set when the race screen already wrote the final Grand Prix round's win to
    /// the profile as the race ended, so the standings must not write it again.
    pub gp_recorded: Option<GpRecorded>,
}

/// The cup result written when the final round of a Grand Prix ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpRecorded {
    /// The local player won the cup (the win and its cup are already saved).
    pub won: bool,
}

/// Counters the race screen keeps for the local player from the sim's events
/// (`RaceStats::absorb`), read by `ui::progress` for achievements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RaceStats {
    /// Head-bashes landed.
    pub bashes: u16,
    /// Power Eggs destroyed.
    pub eggs: u16,
    /// Purple (tier 3) mini-turbos.
    pub purple_turbos: u16,
    /// Tricks landed.
    pub tricks: u16,
    /// Wall hits (the sim's head-on hits, strength over 0.2).
    pub wall_hits: u16,
}

impl RaceStats {
    /// Count the events that belong to racer `seat`.
    pub fn absorb(&mut self, events: &[Event], seat: usize) {
        let me = |r: &u8| *r as usize == seat;
        for e in events {
            match e {
                Event::Bash { attacker, .. } if me(attacker) => self.bashes = self.bashes.saturating_add(1),
                Event::TargetDestroyed { racer, .. } if me(racer) => self.eggs = self.eggs.saturating_add(1),
                Event::Boost { racer, tier: 3 } if me(racer) => self.purple_turbos = self.purple_turbos.saturating_add(1),
                Event::TrickBoost { racer } if me(racer) => self.tricks = self.tricks.saturating_add(1),
                Event::WallHit { racer, .. } if me(racer) => self.wall_hits = self.wall_hits.saturating_add(1),
                _ => {}
            }
        }
    }
}

/// What happened to the Time Trial ghost (`RaceOutcome::ghost`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GhostResult {
    /// This run's time.
    pub time: f32,
    /// The ghost it raced against, if one was loaded.
    pub raced: Option<f32>,
    /// This run became the saved ghost (a new best, or the first one).
    pub saved: bool,
    /// The file could not be written.
    pub failed: bool,
}

/// A battle's overall result (`RaceOutcome::battle`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BattleSummary {
    /// Seconds the battle lasted.
    pub duration: f32,
    /// Ended on the clock (not with one ostrich left).
    pub time_up: bool,
}
