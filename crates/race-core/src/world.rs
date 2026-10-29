//! The race: racers, interactions (spec 5), race rules (spec 6), events.

use std::sync::Arc;

use glam::Vec3;
use rand::Rng;
use rand_pcg::Pcg32;
use serde::{Deserialize, Serialize};

use crate::battle::{roll_battle_item, BattleRules, BattleState};
use crate::bot::{bot_input, update_mem, BotLevel, BotMem};
use crate::challenge::{
    ChallengeId, ChallengeState, ELIMINATION_INTERVAL, SNIPER_MISS_PENALTY, SNIPER_MISS_TIME,
};
use crate::drill::{DrillEvent, DrillId, DrillState, DummyView, RunnerView};
use crate::input::Input;
use crate::items::{
    roll_item, trump_chance, CoinSpot, Hazard, HazardKind, Item, ItemRoll, LooseCoin, Projectile, ProjectileKind, RollGaps, Target,
};
use crate::lesson::{LessonId, LessonState};
use crate::math::{flat, forward, mix64, yaw_of, DT, TICK_HZ};
use crate::racer::{LaserHit, Launch, MotionFx, Racer};
use crate::track::{Feature, Track, TrackId, COIN_HEIGHT, HURDLE_HEIGHT, TARGET_HEIGHT};
use crate::tunables::{Character, Tunables, CHARACTERS};

/// Grand Prix points by finishing place (spec 6).
pub const GP_POINTS: [u8; 8] = [15, 12, 10, 8, 6, 4, 2, 1];

/// Human or bot seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RacerKind {
    /// Controlled by `World::step` inputs.
    Human,
    /// Controlled by [`bot_input`].
    Bot(BotLevel),
}

/// One seat in a race.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RacerSetup {
    /// Display name.
    pub name: String,
    /// Character id.
    pub character: u8,
    /// Human or bot.
    pub kind: RacerKind,
}

/// What kind of session this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RaceMode {
    /// A race (quick race, Grand Prix round, multiplayer).
    Race,
    /// Alone against the clock.
    TimeTrial,
    /// A training lesson on the Training Ground.
    Training(LessonId),
    /// All-bot race behind the title menu: never ends, laps never finish.
    Attract,
    /// A scored challenge (spec 16); racer 0 is the player.
    Challenge(ChallengeId),
    /// Battle "Feather Frenzy" (spec 17) on an arena: feathers, last one standing.
    Battle(BattleRules),
    /// Alone on a track you are making (Run a Track, Drive & place): never
    /// ends, and nothing roams the road (no frogs).
    Solo,
}

impl RaceMode {
    /// The battle rules when this mode is a battle: [`RaceMode::Battle`], or
    /// the Last Feather challenge (default rules; the player starts with 1).
    pub fn battle_rules(self) -> Option<BattleRules> {
        match self {
            RaceMode::Battle(r) => Some(r),
            RaceMode::Challenge(ChallengeId::LastFeather) => Some(BattleRules::default()),
            _ => None,
        }
    }
}

/// The starting grid of a race: `slots[i]` is the grid slot of seat `i`, slot 0
/// being pole, then two to a row, back along the road. A seeded Fisher-Yates
/// shuffle, so a seed always gives the same grid on the host and every client.
pub fn grid_slots(seed: u64, n: usize) -> Vec<usize> {
    let mut slots: Vec<usize> = (0..n).collect();
    for k in (1..n).rev() {
        let j = (mix64(mix64(seed ^ 0x6721_D5EA_0000_0000).wrapping_add(k as u64)) % (k as u64 + 1)) as usize;
        slots.swap(k, j);
    }
    slots
}

/// Race setup (spec 6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RaceConfig {
    /// Track.
    pub track: TrackId,
    /// Laps (1..=5, or 1..=9 for a custom track).
    pub laps: u8,
    /// Seed (items, bots, scatter).
    pub seed: u64,
    /// Mode.
    pub mode: RaceMode,
    /// Up to 8 seats. In a [`RaceMode::Race`] the grid is a seeded random
    /// permutation ([`grid_slots`]); in the other modes seat 0 is the grid's pole.
    pub racers: Vec<RacerSetup>,
    /// Visual selections by racer: colour, ostrich attire, jockey attire.
    #[serde(default)]
    pub attire: [[u8; 3]; 8],
    /// An open-ground drill (spec 18.2): one human, no laps, no finish.
    #[serde(default)]
    pub drill: Option<DrillId>,
    /// Start on foot beside the stationary ostrich; run and jump to mount.
    #[serde(default)]
    pub le_mans: bool,
}

impl RaceConfig {
    /// One human (character 0) plus `bots` bots of `level`.
    pub fn quick(track: TrackId, bots: usize, level: BotLevel, seed: u64) -> Self {
        let mut racers = vec![RacerSetup {
            name: "You".into(),
            character: 0,
            kind: RacerKind::Human,
        }];
        for i in 0..bots.min(7) {
            let c = (i + 1) as u8;
            racers.push(RacerSetup {
                name: Character::get(c).name.into(),
                character: c,
                kind: RacerKind::Bot(level),
            });
        }
        RaceConfig {
            track,
            laps: 3,
            seed,
            mode: RaceMode::Race,
            racers,
            drill: None,
            attire: [[0; 3]; 8],
            le_mans: false,
        }
    }

    /// `n` bots of `level`, no humans.
    pub fn all_bots(track: TrackId, n: usize, level: BotLevel, seed: u64) -> Self {
        let racers = (0..n.min(8))
            .map(|i| RacerSetup {
                name: Character::get(i as u8).name.into(),
                character: i as u8,
                kind: RacerKind::Bot(level),
            })
            .collect();
        RaceConfig {
            track,
            laps: 3,
            seed,
            mode: RaceMode::Race,
            racers,
            drill: None,
            attire: [[0; 3]; 8],
            le_mans: false,
        }
    }

    /// A training lesson with one racer of `kind`.
    pub fn training(lesson: LessonId, kind: RacerKind, seed: u64) -> Self {
        RaceConfig {
            track: TrackId::Training,
            laps: 3,
            seed,
            mode: RaceMode::Training(lesson),
            racers: vec![RacerSetup {
                name: "You".into(),
                character: 0,
                kind,
            }],
            drill: None,
            attire: [[0; 3]; 8],
            le_mans: false,
        }
    }

    /// An open-ground drill (spec 18.2) on `track` (the Paddock, `TrackId::Field`,
    /// in the app): one human racer, no laps, no bots (Bash Buddy adds its dummy).
    pub fn drill(id: DrillId, track: TrackId, seed: u64) -> Self {
        RaceConfig {
            track,
            laps: 3,
            seed,
            mode: RaceMode::Race,
            racers: vec![RacerSetup {
                name: "You".into(),
                character: 0,
                kind: RacerKind::Human,
            }],
            drill: Some(id),
            attire: [[0; 3]; 8],
            le_mans: false,
        }
    }
}

impl RaceConfig {
    /// A battle (spec 17) on `arena` (one of [`TrackId::ARENAS`]) with these
    /// seats (at most 8). Racers start spread round the loop.
    pub fn battle(arena: TrackId, racers: Vec<RacerSetup>, rules: BattleRules, seed: u64) -> Self {
        RaceConfig {
            track: arena,
            laps: 3, // unused in a battle (no laps); not 1, so no HUD shows "final lap"
            seed,
            mode: RaceMode::Battle(rules),
            racers,
            drill: None,
            attire: [[0; 3]; 8],
            le_mans: false,
        }
    }

    /// An all-bot battle: `n` bots of `level` (attract screens, tests, tuning).
    pub fn battle_bots(arena: TrackId, n: usize, level: BotLevel, rules: BattleRules, seed: u64) -> Self {
        let racers = (0..n.min(8))
            .map(|i| RacerSetup {
                name: Character::get(i as u8).name.into(),
                character: i as u8,
                kind: RacerKind::Bot(level),
            })
            .collect();
        RaceConfig::battle(arena, racers, rules, seed)
    }

    /// A challenge: the player (`character`) in seat 0 plus the challenge's
    /// rivals (see [`ChallengeId::rivals`]), on the challenge's track.
    pub fn challenge(id: ChallengeId, character: u8, seed: u64) -> Self {
        let mut racers = vec![RacerSetup {
            name: "You".into(),
            character,
            kind: RacerKind::Human,
        }];
        let pool: Vec<u8> = (0..CHARACTERS.len() as u8)
            .filter(|&c| c != character)
            .collect();
        for (k, level) in id.rivals().into_iter().enumerate() {
            let c = pool[k % pool.len()];
            racers.push(RacerSetup {
                name: Character::get(c).name.into(),
                character: c,
                kind: RacerKind::Bot(level),
            });
        }
        RaceConfig {
            track: id.track(),
            laps: id.laps(),
            seed,
            mode: RaceMode::Challenge(id),
            racers,
            drill: None,
            attire: [[0; 3]; 8],
            le_mans: false,
        }
    }
}

/// Race phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RacePhase {
    /// 3, 2, 1 (racers frozen).
    Countdown,
    /// Racing.
    Racing,
    /// Race over (racers keep running under bot control).
    Finished,
}

/// Why a racer got hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HitCause {
    /// Rolling Egg.
    RollingEgg,
    /// Hunter Egg.
    HunterEgg,
    /// Cactus.
    Cactus,
    /// Lightning Strike.
    Lightning,
    /// Mega Laser.
    MegaLaser,
    /// Head-bash.
    Bash,
    /// Laser dazzle.
    Laser,
    /// Log hurdle.
    Hurdle,
    /// Dollar bill from a hand-cranked money printer.
    MoneyPrinter,
    /// NPC pulling its rug out from under a racer.
    RugPull,
    /// Smashed aside by a Laser Trump.
    LaserTrump,
    /// Bowled over by Pepe the frog (added last: the wire uses the variant index).
    Pepe,
}

/// Things that happened this tick (sounds, FX, HUD, lessons). Racer fields are indices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Countdown beep (3, 2, 1).
    Countdown {
        /// Number shown.
        n: u8,
    },
    /// GO.
    Go,
    /// Perfect start.
    StartBoost {
        /// Racer.
        racer: u8,
    },
    /// Stumbled (early throttle at the start, or a hurdle).
    Stumble {
        /// Racer.
        racer: u8,
    },
    /// A foot hit the ground.
    Footstep {
        /// Racer.
        racer: u8,
    },
    /// Jumped.
    Jump {
        /// Racer.
        racer: u8,
    },
    /// Drift hop. No longer raised (drifts start with a handbrake); kept so the wire format holds.
    DriftHop {
        /// Racer.
        racer: u8,
    },
    /// Drift started.
    DriftStart {
        /// Racer.
        racer: u8,
        /// -1 left, +1 right.
        dir: i8,
    },
    /// Drift sparks reached a tier.
    DriftTier {
        /// Racer.
        racer: u8,
        /// 1 blue, 2 orange, 3 purple.
        tier: u8,
    },
    /// Mini-turbo from a drift release.
    Boost {
        /// Racer.
        racer: u8,
        /// Tier.
        tier: u8,
    },
    /// Boost pad.
    BoostPad {
        /// Racer.
        racer: u8,
    },
    /// Ramp trick.
    Trick {
        /// Racer.
        racer: u8,
    },
    /// Landed a trick: boost.
    TrickBoost {
        /// Racer.
        racer: u8,
    },
    /// Landed.
    Landed {
        /// Racer.
        racer: u8,
        /// Hard landing (camera shake).
        hard: bool,
    },
    /// Hit a wall.
    WallHit {
        /// Racer.
        racer: u8,
        /// Head-on-ness 0..1.
        strength: f32,
    },
    /// Fell off the track.
    Fell {
        /// Racer.
        racer: u8,
    },
    /// Placed back on the track.
    Respawned {
        /// Racer.
        racer: u8,
    },
    /// Picked up a coin.
    CoinPicked {
        /// Racer.
        racer: u8,
        /// Coins now held.
        coins: u32,
    },
    /// Dropped coins.
    CoinsDropped {
        /// Racer.
        racer: u8,
        /// How many.
        n: u8,
    },
    /// Destroyed a Power Egg target.
    TargetDestroyed {
        /// Shooter.
        racer: u8,
        /// Target index.
        target: u16,
        /// Item granted (None: already holding one, got a coin; or Mega Laser).
        item: Option<Item>,
        /// Uses of the granted item.
        uses: u8,
    },
    /// Used an item.
    ItemUsed {
        /// Racer.
        racer: u8,
        /// Item.
        item: Item,
    },
    /// A racer got hit.
    Hit {
        /// Victim.
        victim: u8,
        /// Attacker, if any.
        by: Option<u8>,
        /// Cause.
        cause: HitCause,
    },
    /// A head-bash landed.
    Bash {
        /// Attacker.
        attacker: u8,
        /// Victim.
        victim: u8,
        /// The victim's item, stolen by the attacker.
        stole: Option<Item>,
    },
    /// A head-bash hit nothing.
    BashWhiff {
        /// Racer.
        racer: u8,
    },
    /// Laser dazzled an opponent.
    LaserZap {
        /// Shooter.
        by: u8,
        /// Victim.
        victim: u8,
    },
    /// Laser overheated.
    Overheated {
        /// Racer.
        racer: u8,
    },
    /// Shield raised.
    ShieldUp {
        /// Racer.
        racer: u8,
    },
    /// Shield blocked a hit (and is used up).
    ShieldBlock {
        /// Shield owner.
        racer: u8,
        /// Attacker, if any.
        by: Option<u8>,
    },
    /// Lightning Strike.
    Lightning {
        /// User.
        by: u8,
    },
    /// Mega Laser fired.
    MegaLaser {
        /// User.
        racer: u8,
    },
    /// Cleared a hurdle in the air.
    HurdleCleared {
        /// Racer.
        racer: u8,
    },
    /// Passed a training gate.
    Gate {
        /// Racer.
        racer: u8,
    },
    /// Started a new lap (2.. after the start crossing).
    Lap {
        /// Racer.
        racer: u8,
        /// Lap now on.
        lap: u8,
    },
    /// Started the final lap.
    FinalLap {
        /// Racer.
        racer: u8,
    },
    /// Finished.
    Finished {
        /// Racer.
        racer: u8,
        /// Place.
        place: u8,
        /// Race time.
        time: f32,
    },
    /// WRONG WAY on/off.
    WrongWay {
        /// Racer.
        racer: u8,
        /// Now going the wrong way.
        on: bool,
    },
    /// Lesson counter changed.
    LessonProgress {
        /// Counter.
        progress: u32,
        /// Goal.
        goal: u32,
    },
    /// Lesson complete.
    LessonDone {
        /// 1-3.
        stars: u8,
    },
    /// Lesson time ran out.
    LessonFailed,
    /// The race is over.
    RaceOver,
    /// Drift King: a drift was released with a tier boost, worth `points`
    /// (charge seconds x tier 1/2/3). Emitted for every racer, only in that challenge.
    DriftPoints {
        /// Racer.
        racer: u8,
        /// Charge seconds x tier multiplier.
        points: f32,
    },
    /// Elimination: a racer was removed from the race with a puff at `pos`.
    Eliminated {
        /// Racer (seat index).
        racer: u8,
        /// Their final place.
        place: u8,
        /// Where they were standing (the racer is parked out of sight afterwards).
        pos: [f32; 3],
    },
    /// The challenge score changed (coins, drift points, bashes, Elimination place).
    ChallengeScore {
        /// New score.
        score: f32,
    },
    /// The challenge is over.
    ChallengeDone {
        /// 0 none, 1 bronze, 2 silver, 3 gold.
        medal: u8,
    },
    /// Battle: a racer lost a feather (egg, cactus, Mega Laser, Lightning or bash).
    FeatherLost {
        /// Who lost it.
        racer: u8,
        /// Attacker, if any (`None`: own cactus).
        by: Option<u8>,
    },
    /// Battle: a bash stole a feather (follows the victim's `FeatherLost`).
    FeatherStolen {
        /// The thief (now holds one more).
        racer: u8,
        /// The victim.
        from: u8,
    },
    /// Battle: a racer lost its last feather and is out. It stops where it is
    /// (frozen, out of every interaction) and fades (`BattleState::fade`).
    Out {
        /// Racer.
        racer: u8,
        /// Final place.
        place: u8,
        /// Where it went out.
        pos: [f32; 3],
    },
    /// Battle: over (one left, the clock ran out, or the Last Feather player is
    /// out). Followed by `RaceOver` (and, in Last Feather, `ChallengeDone`).
    BattleOver,
    /// A drill event (spec 18.2): rep clean, strike, level up, star, Master, over.
    Drill(DrillEvent),
    /// A rug pull took every coin the victim held and gave them to the rugger.
    /// Follows the victim's `Hit`. Coins of a rug with no living owner scatter
    /// loose instead (`CoinsDropped`).
    CoinsTaken {
        /// The rugged racer, who now holds none.
        from: u8,
        /// The rugger, who gained exactly `count`.
        to: u8,
        /// How many coins moved.
        count: u32,
    },
}

/// The race world. Deterministic for a given config and input stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct World {
    /// Setup (training dummies are appended to `racers`).
    pub cfg: RaceConfig,
    /// Tunables.
    pub tun: Tunables,
    /// Ticks stepped.
    pub tick: u64,
    /// Phase.
    pub phase: RacePhase,
    /// Tick at which GO happens.
    pub go_tick: u64,
    /// Race time at which the race ended.
    pub ended_at: Option<f32>,
    /// Racers (index = seat).
    pub racers: Vec<Racer>,
    /// Live eggs.
    pub projectiles: Vec<Projectile>,
    /// Cacti.
    pub hazards: Vec<Hazard>,
    /// Fixed coin spots.
    pub coins: Vec<CoinSpot>,
    /// Dropped coins.
    pub loose_coins: Vec<LooseCoin>,
    /// Power Egg targets.
    pub targets: Vec<Target>,
    /// Shared cake, scrambler and moving frog state.
    pub play: crate::playful::PlayState,
    /// The track (serialised as its id; deserialising rebuilds it from the cache).
    #[serde(with = "track_by_id")]
    pub track: Arc<Track>,
    /// Training lesson state.
    pub lesson: Option<LessonState>,
    /// Challenge state (`RaceMode::Challenge`).
    pub challenge: Option<ChallengeState>,
    /// Battle state (`RaceMode::Battle` and the Last Feather challenge).
    #[serde(default)]
    pub battle: Option<BattleState>,
    /// Open-ground drill state (`RaceConfig::drill`).
    #[serde(default)]
    pub drill: Option<DrillState>,
    /// Lap times per racer.
    pub lap_times: Vec<Vec<f32>>,
    /// Race time when each racer's current lap started.
    pub lap_start: Vec<f32>,
    /// Longest time each racer has been stopped while racing.
    pub max_stuck: Vec<f32>,
    /// Bot memory (unstick, item hold).
    pub bot_mem: Vec<BotMem>,
    /// Race time of the first human finish.
    pub first_human_finish: Option<f32>,
    /// Racers finished.
    pub finish_count: u8,
    /// Event counter for stats (all events ever emitted).
    pub event_count: u64,
    rng_counter: u64,
    pub(crate) next_id: u32,
    events: Vec<Event>,
}

const CHECKPOINT_WINDOW: f32 = 60.0;

mod track_by_id {
    use super::*;
    pub fn serialize<S: serde::Serializer>(t: &Arc<Track>, s: S) -> Result<S::Ok, S::Error> {
        t.id.serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Arc<Track>, D::Error> {
        Ok(TrackId::deserialize(d)?.track())
    }
}

impl World {
    /// A new race on the grid, countdown about to start.
    pub fn new(cfg: RaceConfig) -> World {
        World::with_tunables(cfg, Tunables::default())
    }

    /// [`World::new`] with custom tunables.
    pub fn with_tunables(cfg: RaceConfig, tun: Tunables) -> World {
        // A solo run on the Paddock (Run a Track) gets the arena with the middle opened up.
        let track = if cfg.mode == RaceMode::Solo && cfg.track.is_open_field() { Track::open_arena() } else { cfg.track.track() };
        Self::from_track(cfg, tun, track)
    }

    /// Build against a session's own track, independent of the custom registry.
    pub fn with_track(mut cfg: RaceConfig, track: Arc<Track>) -> World {
        cfg.track = track.id;
        Self::from_track(cfg, Tunables::default(), track)
    }

    fn from_track(mut cfg: RaceConfig, tun: Tunables, track: Arc<Track>) -> World {
        let mut tun = tun;
        tun.practice_lasers = cfg.drill == Some(DrillId::LaserRange)
            || matches!(cfg.mode, RaceMode::Training(LessonId::LaserEyes) | RaceMode::Challenge(ChallengeId::LaserSniper));
        cfg.laps = cfg.laps.clamp(1, if cfg.track == TrackId::Custom { 9 } else { 5 });
        cfg.racers.truncate(8);
        let lesson = match cfg.mode {
            RaceMode::Training(id) => Some(LessonState::new(id)),
            _ => None,
        };
        let challenge = match cfg.mode {
            RaceMode::Challenge(id) => {
                let mut cs = ChallengeState::new(id);
                if id == ChallengeId::Elimination {
                    cs.goal = cfg.racers.len().saturating_sub(1) as u32;
                    cs.score = 1.0;
                }
                match id {
                    ChallengeId::CoinRush => tun.coin_respawn = tun.coin_respawn.min(3.0),
                    ChallengeId::EggHunt => tun.target_respawn = tun.target_respawn.min(1.5),
                    // Coins come back only when the player hits a boost pad.
                    ChallengeId::NightCoinRush => tun.coin_respawn = 1.0e6,
                    _ => {}
                }
                Some(cs)
            }
            _ => None,
        };
        if matches!(cfg.mode, RaceMode::Training(LessonId::HeadBash | LessonId::LaserEyes) | RaceMode::Challenge(ChallengeId::LaserSniper)) {
            let count = if cfg.mode == RaceMode::Challenge(ChallengeId::LaserSniper) { 7 } else { 3 };
            let player = cfg.racers.first().map(|r| r.character);
            let characters: Vec<u8> = (0..CHARACTERS.len() as u8).rev().filter(|c| Some(*c) != player).collect();
            for k in 0..count {
                if cfg.racers.len() >= 8 {
                    break;
                }
                cfg.racers.push(RacerSetup {
                    name: format!("Dummy {}", k + 1),
                    character: characters[k % characters.len()],
                    kind: RacerKind::Bot(BotLevel::Dummy),
                });
            }
        }
        if matches!(cfg.drill, Some(DrillId::BashBuddy | DrillId::LaserRange)) && cfg.racers.len() < 8 {
            cfg.racers.push(RacerSetup {
                name: "Buddy".into(),
                character: 7,
                kind: RacerKind::Bot(BotLevel::Dummy),
            });
        }
        if cfg.drill.is_some() {
            // Drill target markers are managed by the exercise, not egg rows.
            tun.target_respawn = 1.0e9;
        }
        let battle_rules = cfg.mode.battle_rules();
        if let Some(rules) = battle_rules {
            tun.target_respawn = tun.target_respawn.min(rules.egg_respawn());
        }
        let l = track.length;
        let open_arena = cfg.mode == RaceMode::Solo && track.id.is_open_field();
        let nr = cfg.racers.len().max(1);
        // Races (not time trials, lessons, drills or challenges) draw the grid at
        // random from the seed: every peer builds the same one, and seat 0 is not
        // always on pole.
        let slots: Vec<usize> = if cfg.mode == RaceMode::Race && cfg.drill.is_none() {
            grid_slots(cfg.seed, nr)
        } else {
            (0..nr).collect()
        };
        let mut racers = Vec::new();
        let mut dummy_k = 0;
        for (i, setup) in cfg.racers.iter().enumerate() {
            let (s, lat) = if battle_rules.is_some() {
                // Spread round the loop, alternating sides.
                (
                    (i as f32 + 0.5) * l / nr as f32,
                    if i % 2 == 0 { -6.0 } else { 6.0 },
                )
            } else if setup.kind == RacerKind::Bot(BotLevel::Dummy) {
                dummy_k += 1;
                (
                    40.0 + 80.0 * (dummy_k - 1) as f32,
                    if dummy_k % 2 == 0 { 3.0 } else { -3.0 },
                )
            } else {
                let slot = slots.get(i).copied().unwrap_or(i);
                let row = slot / 2;
                (
                    l - 6.0 - row as f32 * 6.0,
                    if slot % 2 == 0 { -3.0 } else { 3.0 },
                )
            };
            let sm = track.sample_at(s);
            let idx = track.idx_at(s);
            let mut p = sm.pos + sm.right * lat;
            if open_arena {
                // The middle of the circle, facing the way the start line faced.
                p = Vec3::new(0.0, sm.pos.y, 0.0);
            }
            let pr = track.project(p, idx);
            p.y = pr.ground_y;
            let mut r = Racer::new(i as u8, setup.character, p, yaw_of(sm.tangent), pr.idx);
            r.attire = cfg.attire.get(i).copied().unwrap_or_default().map(|v| v.min(3));
            r.s = pr.s;
            r.lateral = pr.lateral;
            r.ground_y = pr.ground_y;
            r.progress = pr.s - l;
            racers.push(r);
        }
        let mut coins = Vec::new();
        let mut targets = Vec::new();
        let mut row = 0u16;
        let drilling = cfg.drill.is_some();
        for f in track.features.iter().filter(|_| !drilling) {
            match *f {
                Feature::CoinLine {
                    s,
                    lateral,
                    count,
                    spacing,
                } => {
                    for k in 0..count {
                        let p =
                            track.point_at(s + k as f32 * spacing, lateral) + Vec3::Y * COIN_HEIGHT;
                        coins.push(CoinSpot {
                            home: p,
                            pos: p,
                            alive: true,
                            respawn_t: 0.0,
                        });
                    }
                }
                Feature::TargetRow { s, .. } => {
                    let w = track.sample_at(s).width;
                    let count = ((w / 2.5).round() as usize).clamp(3, 8);
                    let bad = (mix64(cfg.seed ^ row as u64) % count as u64) as usize;
                    let spacing = (w - 2.0).max(1.0) / (count - 1) as f32;
                    for k in 0..count {
                        let lat = (k as f32 - (count - 1) as f32 * 0.5) * spacing;
                        let p = track.point_at(s, lat) + Vec3::Y * TARGET_HEIGHT;
                        targets.push(Target { pos: p, row, alive: false, bad: k == bad,
                            respawn_t: 0.0, beam_t: 0.0 });
                    }
                    row += 1;
                }
                _ => {}
            }
        }
        if cfg.mode == RaceMode::Challenge(ChallengeId::TurboTimeAttack) {
            if let Some(r) = racers.first_mut() {
                r.item = Some(Item::Feather);
                r.item_uses = 3;
            }
        }
        if drilling {
            // The one drill egg slot (re-armed by `update_drill`).
            targets.push(Target {
                pos: Vec3::ZERO,
                bad: false,
                row: 0,
                alive: false,
                respawn_t: 0.0,
                beam_t: 0.0,
            });
        }
        let drill = cfg.drill.and_then(|id| {
            racers.first().map(|r| {
                DrillState::new(
                    id,
                    glam::Vec2::ZERO,
                    r.pos,
                    r.yaw,
                    mix64(cfg.seed ^ 0xD121_1A55),
                )
            })
        });
        if let Some((pos, yaw)) = drill.as_ref().and_then(|d| d.dummy).and_then(|c| c.respawn) {
            if let Some(r) = racers.get_mut(1) {
                World::teleport(&track, r, pos, yaw, 0.0);
            }
        }
        if cfg.le_mans && !drilling && !matches!(cfg.mode, RaceMode::Training(_)) {
            for r in &mut racers {
                let pr = track.project(r.pos, r.track_idx as usize);
                let half = track.sample_at(pr.s).width * 0.5;
                // The open arena has no road edge to start beside: the jockey stands 6 m off.
                let reach = if open_arena { 6.0 } else { (half + 0.5 + pr.lateral).clamp(4.0, 13.0) };
                let offset = -r.right() * reach - r.forward() * 1.5;
                r.effects.dismount = crate::mischief::DISMOUNT_TIME;
                r.effects.chase_x = offset.x;
                r.effects.chase_z = offset.z;
                r.effects.le_mans = true;
            }
        }
        let n = racers.len();
        let battle = battle_rules.map(|rules| {
            let mut b = BattleState::new(rules, n);
            if cfg.mode == RaceMode::Challenge(ChallengeId::LastFeather) && n > 0 {
                b.feathers[0] = 1;
            }
            b
        });
        let go_tick = (tun.countdown * TICK_HZ as f32).round() as u64;
        let mut world = World {
            cfg,
            tun,
            tick: 0,
            phase: RacePhase::Countdown,
            go_tick,
            ended_at: None,
            racers,
            projectiles: Vec::new(),
            hazards: Vec::new(),
            coins,
            loose_coins: Vec::new(),
            targets,
            play: Default::default(),
            track,
            lesson,
            challenge,
            battle,
            drill,
            lap_times: vec![Vec::new(); n],
            lap_start: vec![0.0; n],
            max_stuck: vec![0.0; n],
            bot_mem: vec![BotMem::default(); n],
            first_human_finish: None,
            finish_count: 0,
            event_count: 0,
            rng_counter: 0,
            next_id: 1,
            events: Vec::new(),
        };
        // The grid is drawn at random, so the seat number says nothing about who
        // starts in front: show the real order from the first frame, not "1st"
        // from the back row until GO.
        world.update_places();
        world
    }

    /// Race time (0 until GO).
    pub fn race_time(&self) -> f32 {
        if self.tick >= self.go_tick {
            (self.tick - self.go_tick) as f32 * DT
        } else {
            0.0
        }
    }

    /// Seconds of countdown left (0 once racing).
    pub fn countdown_left(&self) -> f32 {
        self.go_tick.saturating_sub(self.tick) as f32 * DT
    }

    /// Takes the events emitted since the last call.
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Racer indices in place order (1st first).
    pub fn standings(&self) -> Vec<usize> {
        if let Some(b) = &self.battle {
            return b.ranking();
        }
        let mut order: Vec<usize> = (0..self.racers.len()).collect();
        let out_rank = |i: usize| -> Option<usize> {
            self.challenge
                .as_ref()
                .and_then(|c| c.eliminated.iter().position(|&e| e == i))
        };
        order.sort_by(|&a, &b| {
            // Eliminated racers go last, the latest-eliminated first.
            match (out_rank(a), out_rank(b)) {
                (Some(x), Some(y)) => return y.cmp(&x),
                (Some(_), None) => return std::cmp::Ordering::Greater,
                (None, Some(_)) => return std::cmp::Ordering::Less,
                (None, None) => {}
            }
            let (ra, rb) = (&self.racers[a], &self.racers[b]);
            match (ra.finished_at, rb.finished_at) {
                (Some(_), Some(_)) => ra.place.cmp(&rb.place),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => rb
                    .progress
                    .partial_cmp(&ra.progress)
                    .unwrap_or(std::cmp::Ordering::Equal),
            }
            .then(a.cmp(&b))
        });
        order
    }

    /// The seat's kind.
    pub fn kind(&self, idx: usize) -> RacerKind {
        self.cfg
            .racers
            .get(idx)
            .map(|s| s.kind)
            .unwrap_or(RacerKind::Human)
    }

    /// Whether the racer has been eliminated (Elimination challenge): parked
    /// out of sight and out of every interaction.
    pub fn is_eliminated(&self, idx: usize) -> bool {
        self.challenge
            .as_ref()
            .is_some_and(|c| c.eliminated.contains(&idx))
    }

    /// Whether the racer is out of a battle (lost its last feather): frozen
    /// where it went out, out of every interaction, fading (`BattleState::fade`).
    pub fn is_out(&self, idx: usize) -> bool {
        self.battle.as_ref().is_some_and(|b| b.is_out(idx))
    }

    /// Out of the race for good: eliminated (Elimination) or out of a battle.
    pub(crate) fn parked(&self, idx: usize) -> bool {
        self.is_eliminated(idx) || self.is_out(idx)
    }

    /// Battle: attacks pass through this racer (post-hit invulnerability, or out).
    pub(crate) fn immune(&self, idx: usize) -> bool {
        self.racers[idx].effects.invincible()
            || self.racers[idx].effects.tumble > 0.0
            || self.racers[idx].effects.dismount > 0.0
            || self
                .battle
                .as_ref()
                .is_some_and(|b| !b.done && (b.flashing(idx) || b.is_out(idx)))
    }

    /// Whether bots drive this racer this tick (bots, finished humans, and everyone once the race is over).
    pub fn bot_level_for(&self, idx: usize) -> Option<BotLevel> {
        match self.kind(idx) {
            RacerKind::Bot(l) => Some(l),
            RacerKind::Human => {
                if self.racers[idx].finished_at.is_some() || self.phase == RacePhase::Finished {
                    Some(BotLevel::Normal)
                } else {
                    None
                }
            }
        }
    }

    /// Whether hurdles trip racers (Training Jump lesson, or non-training modes).
    pub fn hurdles_active(&self) -> bool {
        if self.cfg.drill.is_some() {
            return false;
        }
        match self.cfg.mode {
            RaceMode::Training(id) => id == LessonId::Jump,
            RaceMode::Challenge(id) => id != ChallengeId::BashBrawl,
            _ => true,
        }
    }

    /// A deterministic RNG for bot `idx` at the current tick.
    pub fn bot_rng(&self, idx: usize) -> Pcg32 {
        Pcg32::new(
            mix64(self.cfg.seed ^ mix64(self.tick.wrapping_mul(31).wrapping_add(idx as u64 + 1))),
            0x0A02_BDBF_7BB3_C0A7,
        )
    }

    fn rng(&mut self) -> Pcg32 {
        self.rng_counter += 1;
        Pcg32::new(
            mix64(self.cfg.seed ^ mix64(self.rng_counter ^ 0xC0FF_EE00)),
            0xDA3E_39CB_94B9_5BDB,
        )
    }

    pub(crate) fn emit(&mut self, e: Event) {
        self.event_count += 1;
        self.events.push(e);
    }

    /// Advances one tick. `inputs[i]` drives racer `i` (missing = no input);
    /// bot-driven racers' inputs are replaced by [`bot_input`].
    pub fn step(&mut self, inputs: &[Input]) {
        let n = self.racers.len();
        let mut inp: Vec<Input> = (0..n)
            .map(|i| inputs.get(i).copied().unwrap_or_default())
            .collect();
        for (i, slot) in inp.iter_mut().enumerate() {
            if self.parked(i) {
                continue;
            }
            if let Some(input) = self.drill_dummy_input(i) {
                *slot = input;
            } else if let Some(level) = self.bot_level_for(i) {
                *slot = bot_input(self, i, level);
            }
        }
        let racing = self.phase != RacePhase::Countdown;
        for i in 0..n {
            if self.bot_level_for(i).is_some() && !self.parked(i) {
                let r = &self.racers[i];
                update_mem(&mut self.bot_mem[i], r, racing);
            }
        }
        let ev_start = self.events.len();
        if self.phase == RacePhase::Countdown {
            self.step_countdown(&inp);
        } else {
            self.step_race(&inp);
        }
        self.update_lesson(ev_start);
        self.update_challenge(ev_start);
        self.update_drill(ev_start);
        self.tick += 1;
    }

    fn step_countdown(&mut self, inp: &[Input]) {
        let t = self.tick;
        if t % TICK_HZ as u64 == 0 && t < self.go_tick {
            let n = ((self.go_tick - t) / TICK_HZ as u64) as u8;
            self.emit(Event::Countdown { n });
        }
        let last = t + 1 >= self.go_tick;
        let tun = &self.tun;
        let launches: Vec<Option<Launch>> = self
            .racers
            .iter_mut()
            .zip(inp)
            .map(|(r, input)| r.countdown_step(input, last, tun))
            .collect();
        if last {
            self.phase = RacePhase::Racing;
            self.emit(Event::Go);
            for (i, l) in launches.into_iter().enumerate() {
                match l {
                    Some(Launch::StartBoost) => self.emit(Event::StartBoost { racer: i as u8 }),
                    Some(Launch::Stumble) => self.emit(Event::Stumble { racer: i as u8 }),
                    None => {}
                }
            }
        }
    }

    fn speed_scale(&self, i: usize) -> f32 {
        if let Some(frac) = self.drill_dummy_speed(i) {
            return frac;
        }
        let t = &self.tun;
        match self.kind(i) {
            RacerKind::Human => 1.0,
            RacerKind::Bot(level) => {
                let base = match level {
                    BotLevel::Easy => t.bot_speed[0],
                    BotLevel::Normal => t.bot_speed[1],
                    BotLevel::Hard => t.bot_speed[2],
                    BotLevel::Dummy => 0.42,
                };
                if !matches!(level, BotLevel::Easy | BotLevel::Normal) || self.battle.is_some() {
                    return base;
                }
                let lead = (0..self.racers.len())
                    .filter(|&j| self.kind(j) == RacerKind::Human)
                    .map(|j| self.racers[j].progress)
                    .fold(f32::NEG_INFINITY, f32::max);
                if !lead.is_finite() {
                    return base;
                }
                let gap = lead - self.racers[i].progress;
                let rb = if gap > 40.0 {
                    t.rubber_up * ((gap - 40.0) / 200.0).clamp(0.0, 1.0)
                } else if gap < -40.0 {
                    -t.rubber_down * ((-gap - 40.0) / 200.0).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                base * (1.0 + rb)
            }
        }
    }

    fn step_race(&mut self, inp: &[Input]) {
        let n = self.racers.len();
        let tun = self.tun.clone();
        let track = self.track.clone();
        let now = (self.tick + 1).saturating_sub(self.go_tick) as f32 * DT;

        let item_edge: Vec<bool> = (0..n)
            // Eating cake: no item use (the sim masks the same buttons).
            .map(|i| inp[i].item && !self.racers[i].prev_input.item && self.racers[i].effects.cake_eat <= 0.0)
            .collect();
        let prev_s: Vec<f32> = self.racers.iter().map(|r| r.s).collect();

        // Motion.
        let drift_king = self.cfg.mode == RaceMode::Challenge(ChallengeId::DriftKing);
        for i in 0..n {
            if self.parked(i) {
                continue;
            }
            if i == 1 && self.cfg.drill == Some(DrillId::LaserRange) {
                self.racers[i].effects.step(DT, &tun.playful);
                continue;
            }
            let sc = self.speed_scale(i);
            self.racers[i].speed_scale = sc;
            let fx = self.racers[i].integrate(&inp[i], &track, &tun);
            let points = fx.drift_points;
            self.motion_events(i, fx);
            if drift_king && points > 0.0 {
                self.emit(Event::DriftPoints {
                    racer: i as u8,
                    points,
                });
            }
        }

        self.hurdles_and_gates(&prev_s);
        self.collide_racers();
        self.lasers(now);
        for i in 0..n {
            if item_edge[i] {
                self.use_item(i);
            }
        }
        self.update_projectiles();
        self.update_hazards();
        self.update_coins();
        self.update_playful();
        self.update_mischief();
        self.update_trump();
        self.progress_and_laps(&prev_s, now);
        self.update_battle(now);
        if self.phase == RacePhase::Racing {
            self.update_places();
            self.check_race_end(now);
        }
        self.trump_wind_down();
        // Stuck metric.
        for i in 0..n {
            let r = &mut self.racers[i];
            if self.phase == RacePhase::Racing
                && r.finished_at.is_none()
                && r.respawn_t <= 0.0
                && r.stun_t <= 0.0
                && r.effects.tumble <= 0.0
                && r.effects.dismount <= 0.0
            {
                if r.speed.abs() < 1.0 {
                    r.stuck_t += DT;
                } else {
                    r.stuck_t = 0.0;
                }
            } else {
                r.stuck_t = 0.0;
            }
            self.max_stuck[i] = self.max_stuck[i].max(r.stuck_t);
        }
    }

    fn motion_events(&mut self, i: usize, fx: MotionFx) {
        let racer = i as u8;
        if fx.footstep {
            self.emit(Event::Footstep { racer });
        }
        if fx.jumped {
            self.emit(Event::Jump { racer });
        }
        if fx.drift_start {
            let dir = self.racers[i].drift.dir;
            self.emit(Event::DriftStart { racer, dir });
        }
        if fx.drift_tier > 0 {
            self.emit(Event::DriftTier {
                racer,
                tier: fx.drift_tier,
            });
        }
        if fx.drift_boost > 0 {
            self.emit(Event::Boost {
                racer,
                tier: fx.drift_boost,
            });
        }
        if fx.boost_pad {
            self.emit(Event::BoostPad { racer });
        }
        if fx.trick {
            self.emit(Event::Trick { racer });
        }
        if fx.trick_boost {
            self.emit(Event::TrickBoost { racer });
        }
        if fx.landed {
            self.emit(Event::Landed {
                racer,
                hard: fx.hard_landing,
            });
        }
        if fx.wall_hit > 0.2 {
            self.emit(Event::WallHit {
                racer,
                strength: fx.wall_hit,
            });
        }
        if fx.fell {
            self.emit(Event::Fell { racer });
        }
        if fx.respawned {
            self.emit(Event::Respawned { racer });
        }
        if fx.overheated {
            self.emit(Event::Overheated { racer });
        }
    }

    fn crossed(&self, a: f32, b: f32, mark: f32) -> bool {
        let d = self.track.s_delta(a, b);
        if d <= 0.0 || d > 30.0 {
            return false;
        }
        let m = self.track.s_delta(a, mark);
        m > 0.0 && m <= d
    }

    /// The hurdle stumble: slows the racer and trips it for `hurdle_stumble` seconds.
    fn hurdle_stumble(&mut self, i: usize) {
        if self.immune(i) { return; }
        let r = &mut self.racers[i];
        r.stumble_t = self.tun.hurdle_stumble;
        let sp = r.vel * Vec3::new(0.55, 1.0, 0.55);
        r.vel = sp;
        r.drift = Default::default();
        self.emit(Event::Stumble { racer: i as u8 });
        self.emit(Event::Hit {
            victim: i as u8,
            by: None,
            cause: HitCause::Hurdle,
        });
    }

    fn hurdles_and_gates(&mut self, prev_s: &[f32]) {
        let training = matches!(self.cfg.mode, RaceMode::Training(_));
        let hurdles = self.hurdles_active();
        let feats = self.track.features.clone();
        for i in 0..self.racers.len() {
            if self.racers[i].respawn_t > 0.0 {
                continue;
            }
            for f in &feats {
                match *f {
                    Feature::Hurdle { s } if hurdles => {
                        if self.crossed(prev_s[i], self.racers[i].s, s) {
                            let r = &mut self.racers[i];
                            if r.air_h < HURDLE_HEIGHT * 0.9 {
                                self.hurdle_stumble(i);
                            } else {
                                self.emit(Event::HurdleCleared { racer: i as u8 });
                            }
                        }
                    }
                    Feature::Gate { s } if training => {
                        if self.crossed(prev_s[i], self.racers[i].s, s) {
                            self.emit(Event::Gate { racer: i as u8 });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn keep_in_band(track: &Track, r: &mut Racer) {
        let pr = track.project(r.pos, r.track_idx as usize);
        let hb = track.half_band(pr.s, pr.lateral);
        let lim = (hb - 0.45).max(0.5);
        if pr.lateral.abs() > lim {
            let nrm = track.sample_at(pr.s).right * pr.lateral.signum();
            r.pos -= nrm * (pr.lateral.abs() - lim);
            let vn = r.vel.dot(nrm);
            if vn > 0.0 {
                r.vel -= nrm * vn;
            }
        }
        let pr = track.project(r.pos, pr.idx);
        r.track_idx = pr.idx as u32;
        r.s = pr.s;
        r.lateral = pr.lateral;
    }

    fn collide_racers(&mut self) {
        let n = self.racers.len();
        let rad = self.tun.racer_radius;
        let e = self.tun.bump_restitution;
        let mut touched = vec![false; n];
        for i in 0..n {
            for j in (i + 1)..n {
                if self.cfg.drill == Some(DrillId::LaserRange) && (i == 1 || j == 1) { continue; }
                let (a, b) = (&self.racers[i], &self.racers[j]);
                if a.respawn_t > 0.0 || b.respawn_t > 0.0 || self.immune(i) || self.immune(j) {
                    continue;
                }
                if (a.pos.y - b.pos.y).abs() >= self.tun.bump_height {
                    continue;
                }
                let d = flat(b.pos - a.pos);
                let dist = d.length();
                if dist >= 2.0 * rad {
                    continue;
                }
                let nrm = if dist > 1e-4 { d / dist } else { a.right() };
                let (ma, mb) = (a.class.mods().mass, b.class.mods().mass);
                let overlap = 2.0 * rad - dist;
                let vrel = (b.vel - a.vel).dot(nrm);
                let (ra, rb) = self.racers.split_at_mut(j);
                let a = &mut ra[i];
                let b = &mut rb[0];
                a.pos -= nrm * overlap * mb / (ma + mb);
                b.pos += nrm * overlap * ma / (ma + mb);
                if vrel < 0.0 {
                    let jimp = -(1.0 + e) * vrel / (1.0 / ma + 1.0 / mb);
                    a.vel -= nrm * (jimp / ma);
                    b.vel += nrm * (jimp / mb);
                }
                touched[i] = true;
                touched[j] = true;
            }
        }
        let track = self.track.clone();
        for i in 0..n {
            if touched[i] {
                World::keep_in_band(&track, &mut self.racers[i]);
            }
        }
    }

    pub(crate) fn drop_coins(&mut self, i: usize, n: u8) {
        let n = (n as u32).min(self.racers[i].coins) as u8;
        if n == 0 {
            return;
        }
        self.racers[i].coins -= n as u32;
        let mut rng = self.rng();
        let r = &self.racers[i];
        let (pos, fwd, hint) = (r.pos, r.forward(), r.track_idx);
        for _ in 0..n {
            let a = rng.gen_range(-1.2f32..1.2) + std::f32::consts::PI;
            let sp = rng.gen_range(4.0f32..8.0);
            let dir = forward(yaw_of(fwd) + a);
            self.loose_coins.push(LooseCoin {
                pos: pos + Vec3::Y * COIN_HEIGHT - fwd * 1.0,
                vel: dir * sp,
                ttl: self.tun.loose_coin_life,
                dropped_by: i as u8,
                age: 0.0,
                hint,
            });
        }
        self.emit(Event::CoinsDropped { racer: i as u8, n });
    }

    /// Applies a hit to `victim`: returns false if a shield blocked it.
    pub(crate) fn shield_check(&mut self, victim: usize, by: Option<u8>) -> bool {
        if self.immune(victim) {
            return false;
        }
        if self.racers[victim].shield_t > 0.0 {
            self.racers[victim].shield_t = 0.0;
            self.emit(Event::ShieldBlock {
                racer: victim as u8,
                by,
            });
            false
        } else {
            true
        }
    }

    pub(crate) fn spin_hit(
        &mut self,
        victim: usize,
        by: Option<u8>,
        cause: HitCause,
        spin: f32,
        coins: u8,
    ) {
        if self.racers[victim].respawn_t > 0.0 || self.immune(victim) {
            return;
        }
        if !self.shield_check(victim, by) {
            return;
        }
        self.racers[victim].spin_out(spin);
        // Always the new cause's kind (even "none"): an egg landing in the middle of
        // a Laser Trump spin must not replay the Trump pose.
        self.racers[victim].effects.knock = crate::mischief::KnockKind::from_cause(cause);
        self.emit(Event::Hit {
            victim: victim as u8,
            by,
            cause,
        });
        self.drop_coins(victim, coins);
        self.battle_hit(victim, by);
    }

    /// Battle: `victim` loses a feather to `by` (after the hit has landed).
    /// Returns whether a feather was lost. At zero the victim is out.
    fn battle_hit(&mut self, victim: usize, by: Option<u8>) -> bool {
        let n = self.racers.len();
        let Some(b) = self.battle.as_mut() else {
            return false;
        };
        if b.done || b.is_out(victim) || b.flashing(victim) || b.feathers[victim] == 0 {
            return false;
        }
        b.feathers[victim] -= 1;
        b.taken[victim] = b.taken[victim].saturating_add(1);
        b.invuln[victim] = b.rules.invuln();
        let by = by.filter(|&a| (a as usize) < n && a as usize != victim);
        if let Some(a) = by {
            b.hits[a as usize] = b.hits[a as usize].saturating_add(1);
        }
        // The last ostrich standing is never knocked out: when the final two
        // lose their last feathers on the same tick, the one processed last
        // keeps its place as the winner (it ends the battle at 0 feathers)
        // rather than the winner also being "out".
        let gone = b.feathers[victim] == 0 && b.alive().len() > 1;
        self.emit(Event::FeatherLost {
            racer: victim as u8,
            by,
        });
        if gone {
            self.knock_out(victim);
        }
        true
    }

    /// Battle: racer `i` is out. It stops where it is, frozen (`respawn_t`
    /// set, like an eliminated racer, so every interaction skips it) and fades.
    fn knock_out(&mut self, i: usize) {
        let Some(b) = self.battle.as_mut() else {
            return;
        };
        if b.is_out(i) {
            return;
        }
        let place = b.alive().len() as u8;
        b.out_order.push(i);
        b.out_t[i] = 0.0;
        b.invuln[i] = 0.0;
        let r = &mut self.racers[i];
        let pos = r.pos;
        r.place = place;
        r.vel = Vec3::ZERO;
        r.speed = 0.0;
        r.clear_items();
        r.laser.firing = false;
        r.drift = Default::default();
        r.boost_t = 0.0;
        r.effects.feather = 0.0;
        r.stun_t = 0.0;
        r.mega_t = 0.0;
        r.magnet_t = 0.0;
        r.shield_t = 0.0;
        r.respawn_t = 1.0e9;
        self.emit(Event::Out {
            racer: i as u8,
            place,
            pos: pos.to_array(),
        });
    }

    /// Battle bookkeeping once per racing tick: clocks, invulnerability, and
    /// the end (one left or time up).
    fn update_battle(&mut self, now: f32) {
        let Some(b) = self.battle.as_mut() else {
            return;
        };
        for i in 0..b.out_t.len() {
            if b.is_out(i) {
                b.out_t[i] += DT;
            }
            b.invuln[i] = (b.invuln[i] - DT).max(0.0);
        }
        if b.done || self.phase != RacePhase::Racing {
            return;
        }
        b.time_left = (b.rules.time_limit_s() - now).max(0.0);
        if b.alive().len() <= 1 || b.time_left <= 0.0 {
            if self.challenge.is_some() {
                // Last Feather: the challenge ends the race (ChallengeDone first).
                b.done = true;
                self.emit(Event::BattleOver);
            } else {
                self.end_battle(now);
            }
        }
    }

    /// Ends the battle (once) and the race.
    fn end_battle(&mut self, now: f32) {
        let Some(b) = self.battle.as_mut() else {
            return;
        };
        if b.done {
            return;
        }
        b.done = true;
        self.emit(Event::BattleOver);
        if self.phase == RacePhase::Racing {
            self.end_race(now);
        }
    }


    fn march(&self, from: Vec3, to: Vec3, hint: usize) -> (Vec3, LaserHit) {
        let d = to - from;
        let len = d.length();
        if len < 1e-3 {
            return (to, LaserHit::Air);
        }
        let dir = d / len;
        let step = 1.5;
        let mut h = hint;
        let mut x = step;
        while x < len {
            let p = from + dir * x;
            let pr = self.track.project(p, h);
            h = pr.idx;
            let hb = self.track.half_band(pr.s, pr.lateral);
            if pr.lateral.abs() > hb + 0.3 {
                return (p, LaserHit::Wall);
            }
            if p.y < pr.ground_y {
                return (p, LaserHit::Ground);
            }
            x += step;
        }
        (to, LaserHit::Air)
    }

    fn lasers(&mut self, _now: f32) {
        let tun = self.tun.clone();
        for i in 0..self.racers.len() {
            let r = &self.racers[i];
            let mega = r.mega_t > 0.0;
            if (!mega && !r.laser.firing) || r.disabled() {
                self.racers[i].laser.hit_kind = LaserHit::None;
                continue;
            }
            let origin = r.pos + Vec3::Y * crate::EYE_HEIGHT + r.forward() * crate::EYE_FORWARD;
            let dir = r.forward();
            let range = if mega { tun.mega_range } else { tun.laser_range };
            let mut distance = range;
            let mut kind = LaserHit::Air;
            for (j, o) in self.racers.iter().enumerate() {
                if j == i || o.respawn_t > 0.0 { continue; }
                let v = o.pos + Vec3::Y * 1.8 - origin;
                let along = v.dot(dir);
                let radius = if mega { 1.8 } else { 1.05 };
                let perp2 = (v - dir * along).length_squared();
                if along > 0.0 && perp2 < radius * radius {
                    let near = (along - (radius * radius - perp2).sqrt()).max(0.0);
                    if near < distance { distance = near; kind = LaserHit::Racer(j as u8); }
                }
            }
            let aim = origin + dir * distance;
            let (hit, blocked) = self.march(origin, aim, r.track_idx as usize);
            if blocked != LaserHit::Air { kind = blocked; }
            let l = &mut self.racers[i].laser;
            l.origin = origin;
            l.aim = aim;
            l.hit = hit;
            l.hit_kind = kind;
            if let LaserHit::Racer(j) = kind {
                if l.on_racer == Some(j) { l.on_racer_t += DT; }
                else { l.on_racer = Some(j); l.on_racer_t = DT; }
                if mega || l.on_racer_t >= tun.dazzle_time {
                    let j = j as usize;
                    let bit = 1u32 << j;
                    let can_hit = self.racers[j].effects.burn <= 0.0
                        && self.racers[j].dazzle_imm[i] == 0
                        && (!mega || self.racers[i].mega_hit & bit == 0);
                    if can_hit {
                        self.racers[j].dazzle_imm[i] = (tun.dazzle_immunity * 60.0).ceil().clamp(0.0, 255.0) as u8;
                    }
                    if can_hit && self.shield_check(j, Some(i as u8)) {
                        if mega { self.racers[i].mega_hit |= bit; }
                        self.racers[j].effects.burn = tun.playful.burn_time;
                        self.emit(Event::LaserZap { by: i as u8, victim: j as u8 });
                        self.emit(Event::Hit { victim: j as u8, by: Some(i as u8), cause: HitCause::Laser });
                        if mega { self.battle_hit(j, Some(i as u8)); }
                    }
                }
            } else { l.on_racer = None; l.on_racer_t = 0.0; }
        }
    }

    /// Where racer `i` sits among the others for an item roll: metres of
    /// progress to the nearest unfinished racer ahead and behind, and whether
    /// anyone else holds a coin. A tie in progress counts as ahead.
    pub(crate) fn roll_gaps(&self, i: usize) -> RollGaps {
        let mine = self.racers[i].progress;
        let mut gaps = RollGaps::default();
        for (j, o) in self.racers.iter().enumerate() {
            if j == i {
                continue;
            }
            gaps.others_hold_coins |= o.coins > 0;
            if o.finished_at.is_some() {
                continue;
            }
            if o.progress >= mine {
                let d = o.progress - mine;
                gaps.ahead = Some(gaps.ahead.map_or(d, |g| g.min(d)));
            } else {
                let d = mine - o.progress;
                gaps.behind = Some(gaps.behind.map_or(d, |g| g.min(d)));
            }
        }
        gaps
    }

    pub(crate) fn reward_target(&mut self, i: usize, k: usize) {
        if self.drill.is_some() {
            // Legacy drill markers pay only the drill rep.
            self.emit(Event::TargetDestroyed {
                racer: i as u8,
                target: k as u16,
                item: None,
                uses: 0,
            });
            return;
        }
        if self.racers[i].item.is_none() {
            let place = self.racers[i].place;
            let n = self.racers.len();
            let mut rng = self.rng();
            let gaps = self.roll_gaps(i);
            let mut roll = if self.battle.is_some() {
                roll_battle_item(&mut rng)
            } else {
                roll_item(&mut rng, place, n, gaps)
            };
            // Bots get the cake about a quarter as often as before: half the
            // time a cake they roll is swapped for a fresh roll, same rng.
            if roll.item == Item::SharedCake
                && self.battle.is_none()
                && matches!(self.kind(i), RacerKind::Bot(_))
                && rng.gen::<bool>()
            {
                roll = roll_item(&mut rng, place, n, gaps);
            }
            if roll.item == Item::RugPull && (n < 2 || self.play.rugs.len() >= crate::mischief::MAX_RUGS) {
                roll = ItemRoll { item: Item::Feather, uses: 1 };
            }
            // Catch-up: a racer well back may get a Laser Trump instead, more
            // often the further back, less when the pack is close ahead.
            // Race mode only; battles and time trials never roll it.
            let r = &self.racers[i];
            if self.cfg.mode == RaceMode::Race
                && self.battle.is_none()
                && !r.trumping()
                && r.finished_at.is_none()
            {
                let chance = trump_chance(place, n, gaps.ahead);
                if chance > 0.0 && rng.gen::<f32>() < chance {
                    roll = ItemRoll {
                        item: Item::LaserTrump,
                        uses: 1,
                    };
                }
            }
            self.racers[i].item = Some(roll.item);
            self.racers[i].item_uses = roll.uses;
            self.emit(Event::TargetDestroyed {
                racer: i as u8,
                target: k as u16,
                item: Some(roll.item),
                uses: roll.uses,
            });
        } else if self
            .challenge
            .as_ref()
            .is_some_and(|c| c.id == ChallengeId::NightCoinRush)
        {
            // Night Coin Rush counts coins on the road only (spec 17.3): an
            // egg collected while holding an item pays nothing, so camping a
            // respawning row cannot farm points.
            self.emit(Event::TargetDestroyed {
                racer: i as u8,
                target: k as u16,
                item: None,
                uses: 0,
            });
        } else {
            let r = &mut self.racers[i];
            r.coins = r.coins.saturating_add(1);
            let coins = r.coins;
            self.emit(Event::TargetDestroyed {
                racer: i as u8,
                target: k as u16,
                item: None,
                uses: 0,
            });
            self.emit(Event::CoinPicked {
                racer: i as u8,
                coins,
            });
        }
    }

    /// Gives a racer an item directly (tests, debug, training scripts): into the
    /// slot, or behind the held one in the queue (dropped when the queue is full).
    pub fn give_item(&mut self, idx: usize, item: Item, uses: u8) {
        self.racers[idx].stash(item, uses);
    }

    fn use_item(&mut self, i: usize) {
        if self.phase == RacePhase::Countdown || self.racers[i].disabled() {
            return;
        }
        let Some(item) = self.racers[i].item else {
            return;
        };
        if item == Item::RugPull && self.play.rugs.len() >= crate::mischief::MAX_RUGS {
            return;
        }
        // One timed item at a time: the press does nothing (and wastes nothing)
        // until the running one ends. The pickup of the next egg is not blocked.
        if self.racers[i].active_item(&self.tun, self.play.scramble).is_some() {
            return;
        }
        let tun = self.tun.clone();
        {
            let r = &mut self.racers[i];
            r.item_uses = r.item_uses.saturating_sub(1);
            if r.item_uses == 0 {
                r.item = None;
                // The next queued power-up moves up; a bot weighs it afresh.
                if r.refill() {
                    if let Some(m) = self.bot_mem.get_mut(i) {
                        m.hold_t = 0.0;
                    }
                }
            }
        }
        self.emit(Event::ItemUsed {
            racer: i as u8,
            item,
        });
        let id = self.next_id;
        let r = self.racers[i].clone();
        match item {
            Item::Laptop
            | Item::OrangePill
            | Item::EggScrambler
            | Item::SharedCake
            | Item::MoneyPrinter
            | Item::RugPull
            | Item::LaserTrump
            | Item::Rose => self.playful_item(i, item),

            Item::Feather => {
                self.racers[i].grant_boost(tun.feather_boost);
                self.racers[i].effects.feather = tun.feather_boost;
            }
            Item::RollingEgg => {
                let dir = if r.look_back > 0.5 {
                    -r.forward()
                } else {
                    r.forward()
                };
                let sp = if r.look_back > 0.5 {
                    tun.egg_speed * 0.6
                } else {
                    tun.egg_speed.max(r.speed + 12.0)
                };
                self.next_id += 1;
                self.projectiles.push(Projectile {
                    kind: ProjectileKind::RollingEgg,
                    pos: r.pos + dir * 1.6 + Vec3::Y * 0.45,
                    vel: dir * sp,
                    owner: i as u8,
                    target: None,
                    age: 0.0,
                    bounces: 0,
                    hint: r.track_idx,
                    s: r.s,
                    lateral: r.lateral,
                    id,
                });
            }
            Item::HunterEgg => {
                let target = if self.battle.is_some() {
                    // Battle: the nearest racer ahead along the loop.
                    let l = self.track.length;
                    (0..self.racers.len())
                        .filter(|&j| j != i && !self.parked(j) && self.racers[j].respawn_t <= 0.0)
                        .map(|j| (j, (self.racers[j].s - r.s).rem_euclid(l)))
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(j, _)| j as u8)
                } else if r.place > 1 {
                    self.racers
                        .iter()
                        .position(|o| o.place == r.place - 1)
                        .map(|j| j as u8)
                } else {
                    None
                };
                self.next_id += 1;
                self.projectiles.push(Projectile {
                    kind: ProjectileKind::HunterEgg,
                    pos: r.pos + r.forward() * 1.6 + Vec3::Y * 0.5,
                    vel: r.forward() * tun.hunter_speed,
                    owner: i as u8,
                    target,
                    age: 0.0,
                    bounces: 0,
                    hint: r.track_idx,
                    s: r.s + 1.6,
                    lateral: r.lateral,
                    id,
                });
            }
            Item::Cactus => {
                if self.hazards.len() >= tun.cactus_max {
                    self.hazards.remove(0);
                }
                let mut p = r.pos - r.forward() * 2.0;
                let pr = self.track.project(p, r.track_idx as usize);
                p.y = pr.ground_y;
                self.next_id += 1;
                self.hazards.push(Hazard {
                    kind: HazardKind::Cactus,
                    pos: p,
                    owner: i as u8,
                    age: 0.0,
                    id,
                });
            }
            Item::Shield => {
                self.racers[i].shield_t = tun.shield_time;
                self.emit(Event::ShieldUp { racer: i as u8 });
            }
            Item::Lightning => {
                self.emit(Event::Lightning { by: i as u8 });
                let battle = self.battle.is_some();
                // Battle: zap the worst-ranked first, so when the last racers
                // all go out on this tick the best-ranked one is the survivor
                // (not whoever has the highest seat).
                let order: Vec<usize> = match self.battle.as_ref() {
                    Some(b) => b.ranking().into_iter().rev().collect(),
                    None => (0..self.racers.len()).collect(),
                };
                for j in order {
                    // A battle has no "ahead": every opponent still in is zapped.
                    let skip = if battle {
                        j == i || self.racers[j].respawn_t > 0.0 || self.immune(j)
                    } else {
                        j == i
                            || self.racers[j].place >= r.place
                            || self.racers[j].finished_at.is_some()
                    };
                    if skip {
                        continue;
                    }
                    if !self.shield_check(j, Some(i as u8)) {
                        continue;
                    }
                    let v = &mut self.racers[j];
                    v.slow(tun.lightning_slow_time, tun.lightning_slow_mult);
                    v.clear_items();
                    v.drift = Default::default();
                    if v.on_ground {
                        v.vel.y = 3.0;
                        v.on_ground = false;
                        v.air_t = 0.0;
                        v.air_peak = v.pos.y;
                    }
                    self.emit(Event::Hit {
                        victim: j as u8,
                        by: Some(i as u8),
                        cause: HitCause::Lightning,
                    });
                    self.battle_hit(j, Some(i as u8));
                }
            }
            Item::MegaLaser => {
                self.racers[i].mega_t = tun.mega_time;
                self.racers[i].mega_hit = 0;
                self.emit(Event::MegaLaser { racer: i as u8 });
            }
            Item::Magnet => {
                self.racers[i].magnet_t = tun.magnet_time;
            }
        }
    }

    fn update_projectiles(&mut self) {
        let tun = self.tun.clone();
        let track = self.track.clone();
        let mut k = 0;
        while k < self.projectiles.len() {
            let mut p = self.projectiles[k].clone();
            p.age += DT;
            let mut dead = false;
            match p.kind {
                ProjectileKind::DollarBill => {
                    if let Some(o) = p
                        .target
                        .and_then(|i| self.racers.get(i as usize))
                        .filter(|o| o.finished_at.is_none() && o.respawn_t <= 0.0)
                    {
                        let toward = o.body() - p.pos;
                        // Guidance can follow a target ahead, but never turn a bill back.
                        if o.effects.dismount <= 0.0 && flat(toward).dot(flat(p.vel)) > 0.0 {
                            p.vel = toward.normalize_or_zero() * 52.0;
                        } else {
                            p.target = None;
                        }
                    }
                    p.pos += p.vel * DT;
                    dead = p.age > 1.5;
                }
                ProjectileKind::RollingEgg => {
                    p.pos.x += p.vel.x * DT;
                    p.pos.z += p.vel.z * DT;
                    let mut pr = track.project(p.pos, p.hint as usize);
                    let hb = track.half_band(pr.s, pr.lateral) - 0.4;
                    if pr.lateral.abs() > hb {
                        let nrm = track.sample_at(pr.s).right * pr.lateral.signum();
                        p.pos -= nrm * (pr.lateral.abs() - hb);
                        let vn = p.vel.dot(nrm);
                        if vn > 0.0 {
                            p.vel -= nrm * 2.0 * vn;
                        }
                        p.bounces += 1;
                        pr = track.project(p.pos, pr.idx);
                    }
                    p.hint = pr.idx as u32;
                    let g = pr.ground_y + 0.45;
                    if p.pos.y > g + 1.0 || p.vel.y < -0.5 {
                        p.vel.y -= tun.gravity * DT;
                        p.pos.y += p.vel.y * DT;
                        if p.pos.y <= g {
                            p.pos.y = g;
                            p.vel.y = 0.0;
                        }
                    } else {
                        p.pos.y = g;
                        p.vel.y = 0.0;
                    }
                    p.s = pr.s;
                    p.lateral = pr.lateral;
                    if p.age > tun.egg_life || p.bounces > tun.egg_bounces || p.pos.y < track.kill_y
                    {
                        dead = true;
                    }
                }
                ProjectileKind::HunterEgg => {
                    let tgt = p
                        .target
                        .map(|t| t as usize)
                        .filter(|&t| t < self.racers.len() && self.racers[t].respawn_t <= 0.0);
                    let home = tgt.map(|t| {
                        (
                            self.racers[t].body(),
                            (self.racers[t].body() - p.pos).length(),
                        )
                    });
                    match home {
                        Some((tp, dist)) if dist < tun.hunter_home_dist => {
                            let d = flat(tp - p.pos).normalize_or_zero();
                            p.vel = d * tun.hunter_speed;
                            p.pos += p.vel * DT;
                            let pr = track.project(p.pos, p.hint as usize);
                            p.hint = pr.idx as u32;
                            p.s = pr.s;
                            p.lateral = pr.lateral;
                            p.pos.y = pr.ground_y.max(track.sample_at(pr.s).pos.y) + 0.5;
                        }
                        _ => {
                            p.s = track.wrap_s(p.s + tun.hunter_speed * DT);
                            let want = tgt.map(|t| self.racers[t].lateral).unwrap_or(0.0);
                            p.lateral += (want - p.lateral).clamp(-6.0 * DT, 6.0 * DT);
                            let sm = track.sample_at(p.s);
                            let np = sm.pos + sm.right * p.lateral + Vec3::Y * (0.5 + sm.ramp);
                            p.vel = (np - p.pos) / DT;
                            p.pos = np;
                            p.hint = track.idx_at(p.s) as u32;
                        }
                    }
                    if p.age > tun.hunter_life {
                        dead = true;
                    }
                }
            }
            // Hits.
            if !dead {
                let cause = if p.kind == ProjectileKind::RollingEgg {
                    HitCause::RollingEgg
                } else {
                    HitCause::HunterEgg
                };
                let mut victim = None;
                for (j, o) in self.racers.iter().enumerate() {
                    if p.kind == ProjectileKind::DollarBill
                        && (j as u8 == p.owner || o.finished_at.is_some()
                            || o.effects.dismount > 0.0 || o.effects.remount_guard > 0.0)
                    {
                        continue;
                    }
                    if (j as u8 == p.owner && p.age < 0.6) || o.respawn_t > 0.0 || self.immune(j) {
                        continue;
                    }
                    let d = o.body() - p.pos;
                    if flat(d).length() < tun.racer_radius + 0.5 && d.y.abs() < 1.8 {
                        victim = Some(j);
                        break;
                    }
                }
                if let Some(j) = victim {
                    dead = true;
                    if p.kind == ProjectileKind::DollarBill {
                        if self.shield_check(j, Some(p.owner)) {
                            self.racers[j].knock_off(crate::mischief::KnockKind::Printer, 1.0);
                            self.emit(Event::Hit {
                                victim: j as u8,
                                by: Some(p.owner),
                                cause: HitCause::MoneyPrinter,
                            });
                            self.battle_hit(j, Some(p.owner));
                        }
                    } else {
                        self.spin_hit(j, Some(p.owner), cause, tun.egg_spin, tun.egg_coins);
                    }
                }
            }
            if !dead {
                if let Some(h) = self
                    .hazards
                    .iter()
                    .position(|h| flat(h.pos - p.pos).length() < 1.2)
                {
                    self.hazards.remove(h);
                    dead = true;
                }
            }
            if dead {
                self.projectiles.remove(k);
            } else {
                self.projectiles[k] = p;
                k += 1;
            }
        }
    }

    fn update_hazards(&mut self) {
        let tun = self.tun.clone();
        let mut k = 0;
        while k < self.hazards.len() {
            self.hazards[k].age += DT;
            let h = self.hazards[k].clone();
            let mut hit = None;
            for (j, o) in self.racers.iter().enumerate() {
                if (j as u8 == h.owner && h.age < 1.0)
                    || o.respawn_t > 0.0
                    || o.air_h > 1.0
                    || self.immune(j)
                {
                    continue;
                }
                if flat(o.pos - h.pos).length() < tun.racer_radius + 0.45 {
                    hit = Some(j);
                    break;
                }
            }
            if let Some(j) = hit {
                self.hazards.remove(k);
                let by = if h.owner as usize == j {
                    None
                } else {
                    Some(h.owner)
                };
                self.spin_hit(j, by, HitCause::Cactus, tun.cactus_spin, 0);
            } else {
                k += 1;
            }
        }
    }

    fn update_coins(&mut self) {
        let tun = self.tun.clone();
        let n = self.racers.len();
        // Fixed spots.
        for c in 0..self.coins.len() {
            if !self.coins[c].alive {
                self.coins[c].respawn_t -= DT;
                if self.coins[c].respawn_t <= 0.0 {
                    self.coins[c].alive = true;
                    self.coins[c].pos = self.coins[c].home;
                }
                continue;
            }
            let pos = self.coins[c].pos;
            let magnet = (0..n).find(|&i| {
                let r = &self.racers[i];
                r.magnet_t > 0.0
                    && r.respawn_t <= 0.0
                    && (r.body() - pos).length() < tun.magnet_radius
            });
            let home = self.coins[c].home;
            let newpos = match magnet {
                Some(i) => {
                    let d = self.racers[i].body() - pos;
                    pos + d.normalize_or_zero() * (40.0 * DT).min(d.length())
                }
                None => {
                    let d = home - pos;
                    pos + d.normalize_or_zero() * (12.0 * DT).min(d.length())
                }
            };
            self.coins[c].pos = newpos;
            let picker = (0..n).find(|&i| self.racers[i].reaches_coin(newpos, &tun));
            if let Some(i) = picker {
                self.coins[c].alive = false;
                self.coins[c].respawn_t = tun.coin_respawn;
                self.pick_coin(i);
            }
        }
        // Loose coins.
        let track = self.track.clone();
        let mut k = 0;
        while k < self.loose_coins.len() {
            let mut lc = self.loose_coins[k].clone();
            lc.age += DT;
            lc.ttl -= DT;
            let magnet = (0..n).find(|&i| {
                let r = &self.racers[i];
                r.magnet_t > 0.0 && (r.body() - lc.pos).length() < tun.magnet_radius
            });
            if let Some(i) = magnet {
                let d = self.racers[i].body() - lc.pos;
                lc.pos += d.normalize_or_zero() * (40.0 * DT).min(d.length());
            } else {
                lc.pos += lc.vel * DT;
                lc.vel *= (-3.0 * DT).exp();
                let pr = track.project(lc.pos, lc.hint as usize);
                lc.hint = pr.idx as u32;
                let hb = track.half_band(pr.s, pr.lateral) - 0.5;
                if pr.lateral.abs() > hb {
                    lc.pos -=
                        track.sample_at(pr.s).right * pr.lateral.signum() * (pr.lateral.abs() - hb);
                }
                let g = pr.ground_y.max(track.sample_at(pr.s).pos.y) + COIN_HEIGHT;
                lc.pos.y += (g - lc.pos.y) * (8.0 * DT).min(1.0);
            }
            let picker = (0..n).find(|&i| {
                let r = &self.racers[i];
                !(i as u8 == lc.dropped_by && lc.age < 0.8) && r.reaches_coin(lc.pos, &tun)
            });
            if let Some(i) = picker {
                self.loose_coins.remove(k);
                self.pick_coin(i);
            } else if lc.ttl <= 0.0 {
                self.loose_coins.remove(k);
            } else {
                self.loose_coins[k] = lc;
                k += 1;
            }
        }
    }

    fn pick_coin(&mut self, i: usize) {
        let coins = self.racers[i].pick_coin(&self.tun);
        self.emit(Event::CoinPicked {
            racer: i as u8,
            coins,
        });
    }

    fn progress_and_laps(&mut self, prev_s: &[f32], now: f32) {
        if self.battle.is_some() || self.drill.is_some() {
            // No laps, places or WRONG WAY in a battle or a drill: drive any way you like.
            return;
        }
        let l = self.track.length;
        let cps = self.track.checkpoints();
        let laps = self.cfg.laps;
        let finishing = match self.cfg.mode {
            RaceMode::Race | RaceMode::TimeTrial => true,
            RaceMode::Challenge(id) => id.finishing(),
            _ => false,
        };
        for i in 0..self.racers.len() {
            if self.is_eliminated(i) {
                continue;
            }
            if finishing && self.racers[i].lap > laps && self.racers[i].finished_at.is_none()
                && self.racers[i].effects.dismount <= 0.0 && self.phase == RacePhase::Racing
            {
                self.finish(i, now);
            }
            let (a, b) = (prev_s[i], self.racers[i].s);
            let fwd_cross = a > l - CHECKPOINT_WINDOW && b < CHECKPOINT_WINDOW;
            let back_cross = a < CHECKPOINT_WINDOW && b > l - CHECKPOINT_WINDOW;
            if fwd_cross {
                let r = &self.racers[i];
                if r.lap == 0 || r.next_cp >= 4 {
                    let old = r.lap;
                    self.racers[i].lap += 1;
                    self.racers[i].next_cp = 1;
                    let lap = self.racers[i].lap;
                    if old >= 1
                        && self.racers[i].finished_at.is_none()
                        && self.phase == RacePhase::Racing
                    {
                        self.lap_times[i].push(now - self.lap_start[i]);
                        self.lap_start[i] = now;
                        if finishing && lap > laps {
                            if self.racers[i].effects.dismount <= 0.0 { self.finish(i, now); }
                        } else {
                            self.emit(Event::Lap {
                                racer: i as u8,
                                lap,
                            });
                            if finishing && lap == laps {
                                self.emit(Event::FinalLap { racer: i as u8 });
                            }
                        }
                    }
                }
            } else if back_cross {
                let r = &mut self.racers[i];
                if r.lap > 0 {
                    r.lap -= 1;
                    r.next_cp = 4;
                }
            } else {
                let r = &self.racers[i];
                if (1..=3).contains(&r.next_cp) && self.crossed(a, b, cps[r.next_cp as usize]) {
                    self.racers[i].next_cp += 1;
                }
            }
            let r = &mut self.racers[i];
            r.progress = (r.lap as f32 - 1.0) * l + r.s;
            // Wrong way.
            let tangent = self.track.sample_at(r.s).tangent;
            let along = flat(r.vel).dot(flat(tangent));
            let facing = r.forward().dot(flat(tangent).normalize_or_zero());
            if along < -2.0 && facing < -0.3 && r.respawn_t <= 0.0 {
                r.wrong_way_t += DT;
            } else {
                r.wrong_way_t = (r.wrong_way_t - 2.0 * DT).max(0.0);
            }
            let ww = r.wrong_way_t > self.tun.wrong_way_time;
            if ww != r.wrong_way {
                r.wrong_way = ww;
                self.emit(Event::WrongWay {
                    racer: i as u8,
                    on: ww,
                });
            }
        }
    }

    fn finish(&mut self, i: usize, now: f32) {
        self.finish_count += 1;
        let place = self.finish_count;
        let r = &mut self.racers[i];
        r.finished_at = Some(now);
        r.place = place;
        if self.kind(i) == RacerKind::Human && self.first_human_finish.is_none() {
            self.first_human_finish = Some(now);
        }
        self.emit(Event::Finished {
            racer: i as u8,
            place,
            time: now,
        });
    }

    fn update_places(&mut self) {
        let order = self.standings();
        for (p, &i) in order.iter().enumerate() {
            if self.racers[i].finished_at.is_none() {
                self.racers[i].place = p as u8 + 1;
            }
        }
    }

    fn check_race_end(&mut self, now: f32) {
        if self.drill.is_some() {
            // A drill ends from `update_drill` (Master, three strikes).
            return;
        }
        let over = match self.cfg.mode {
            // Battles end from `update_battle`.
            RaceMode::Attract
            | RaceMode::Solo
            | RaceMode::Training(_)
            | RaceMode::Challenge(_)
            | RaceMode::Battle(_) => false,
            RaceMode::Race | RaceMode::TimeTrial => {
                let humans: Vec<usize> = (0..self.racers.len())
                    .filter(|&i| self.kind(i) == RacerKind::Human)
                    .collect();
                if humans.is_empty() {
                    self.racers.iter().all(|r| r.finished_at.is_some())
                } else {
                    humans.iter().all(|&i| self.racers[i].finished_at.is_some())
                }
            }
        };
        if over {
            self.end_race(now);
        }
    }

    fn end_race(&mut self, now: f32) {
        self.update_places();
        self.phase = RacePhase::Finished;
        self.ended_at = Some(now);
        self.emit(Event::RaceOver);
    }

    fn update_challenge(&mut self, ev_start: usize) {
        let Some(mut cs) = self.challenge.clone() else {
            return;
        };
        if cs.done || self.phase == RacePhase::Countdown {
            return;
        }
        let id = cs.id;
        cs.time = self.race_time() + DT;
        let score_before = cs.score;
        let mut finished_time = None;
        let mut respawn_coins = false;
        for e in &self.events[ev_start..] {
            match (id, e) {
                (ChallengeId::CoinRush, Event::CoinPicked { racer: 0, .. }) => {
                    cs.score += 1.0;
                    cs.progress += 1;
                }
                (ChallengeId::EggHunt, Event::TargetDestroyed { racer: 0, .. }) => {
                    cs.progress += 1;
                }
                (ChallengeId::DriftKing, Event::DriftPoints { racer: 0, points }) => {
                    cs.score += *points;
                    cs.progress += 1;
                }
                (
                    ChallengeId::BashBrawl,
                    Event::Bash {
                        attacker: 0, stole, ..
                    },
                ) => {
                    cs.score += if stole.is_some() { 2.0 } else { 1.0 };
                    cs.progress += 1;
                }
                (
                    ChallengeId::DriftKing | ChallengeId::TurboTimeAttack | ChallengeId::RampRider,
                    Event::Finished { racer: 0, time, .. },
                ) => finished_time = Some(*time),
                // Laser Sniper counts straight-beam hits on opponents.
                (ChallengeId::LaserSniper, Event::LaserZap { by: 0, .. })
                    if self.racers[0].mega_t <= 0.0 =>
                {
                    cs.progress += 1;
                    cs.miss_t = 0.0;
                }
                (ChallengeId::RampRider, Event::TrickBoost { racer: 0 }) => {
                    cs.score += 1.0;
                    cs.progress += 1;
                    // The chain bonus is paid on landing, not at take-off.
                    if cs.chain_pending {
                        cs.score += 2.0;
                        cs.misses += 1;
                    }
                    cs.chain_pending = false;
                    cs.chain = true;
                }
                (ChallengeId::RampRider, Event::Trick { racer: 0 }) => {
                    // A trick started while boosting after a landed one is a
                    // chain; a failed trick (no landing) pays nothing.
                    cs.chain_pending = cs.chain && self.racers[0].boost_t > 0.0;
                    cs.chain = false;
                }
                (ChallengeId::NightCoinRush, Event::CoinPicked { racer: 0, .. }) => {
                    cs.score += 1.0;
                    cs.progress += 1;
                }
                (ChallengeId::NightCoinRush, Event::BoostPad { racer: 0 }) => respawn_coins = true,
                _ => {}
            }
        }
        if respawn_coins {
            for c in self.coins.iter_mut().filter(|c| !c.alive) {
                c.alive = true;
                c.pos = c.home;
                c.respawn_t = 0.0;
            }
        }
        let mut new_events = Vec::new();
        match id {
            ChallengeId::CoinRush | ChallengeId::BashBrawl | ChallengeId::NightCoinRush => {
                let lim = id.time_limit().unwrap_or(0.0);
                cs.time_left = (lim - cs.time).max(0.0);
                cs.done = cs.time >= lim;
            }
            ChallengeId::EggHunt => {
                let lim = id.time_limit().unwrap_or(0.0);
                cs.time_left = (lim - cs.time).max(0.0);
                cs.score = cs.time;
                if cs.progress >= cs.goal {
                    cs.done = true;
                } else if cs.time >= lim {
                    cs.done = true;
                    cs.score = lim; // gave up: no medal
                }
            }
            ChallengeId::DriftKing | ChallengeId::RampRider => {
                cs.done = finished_time.is_some();
            }
            ChallengeId::LaserSniper => {
                let lim = id.time_limit().unwrap_or(0.0);
                let l = &self.racers[0].laser;
                if l.firing && !matches!(l.hit_kind, LaserHit::Racer(_)) {
                    cs.miss_t += DT;
                    if cs.miss_t >= SNIPER_MISS_TIME - 1e-4 {
                        cs.miss_t = 0.0;
                        cs.penalty += SNIPER_MISS_PENALTY;
                        cs.misses += 1;
                    }
                }
                cs.time_left = (lim - cs.time).max(0.0);
                cs.score = cs.time + cs.penalty;
                if cs.progress >= cs.goal {
                    cs.done = true;
                } else if cs.time >= lim {
                    cs.done = true;
                    cs.score = lim + cs.penalty; // gave up: no medal
                }
            }
            ChallengeId::LastFeather => {
                if let Some(b) = self.battle.as_mut() {
                    cs.time_left = b.time_left;
                    cs.progress = b.out_order.iter().filter(|&&i| i != 0).count() as u32;
                    cs.score = b.place_of(0) as f32;
                    if b.is_out(0) && !b.done {
                        b.done = true;
                        new_events.push(Event::BattleOver);
                    }
                    cs.done = b.done;
                }
            }
            ChallengeId::TurboTimeAttack => {
                cs.progress = self.lap_times[0].len() as u32;
                match finished_time {
                    Some(t) => {
                        cs.score = t;
                        cs.done = true;
                    }
                    None => cs.score = cs.time,
                }
            }
            ChallengeId::Elimination => {
                let n = self.racers.len();
                let k = cs.eliminated.len();
                if k + 1 < n && cs.time >= ELIMINATION_INTERVAL * (k + 1) as f32 {
                    let order = self.standings();
                    if let Some(&v) = order.iter().rev().find(|&&i| !cs.eliminated.contains(&i)) {
                        let place = (n - k) as u8;
                        let pos = self.racers[v].pos;
                        {
                            let r = &mut self.racers[v];
                            r.place = place;
                            r.vel = Vec3::ZERO;
                            r.clear_items();
                            r.laser.firing = false;
                            r.drift = Default::default();
                            r.respawn_t = 1.0e9; // frozen, and skipped by every interaction
                            r.pos.y = -1000.0;
                        }
                        cs.eliminated.push(v);
                        cs.progress += 1;
                        new_events.push(Event::Eliminated {
                            racer: v as u8,
                            place,
                            pos: pos.to_array(),
                        });
                    }
                }
                // Places among the survivors; the player's score is their place.
                if cs.eliminated.contains(&0) {
                    cs.done = true;
                } else {
                    let order = self.standings();
                    cs.score = order.iter().position(|&i| i == 0).unwrap_or(0) as f32 + 1.0;
                    cs.done = cs.eliminated.len() + 1 >= n;
                }
                if cs.done && cs.eliminated.contains(&0) {
                    cs.score = self.racers[0].place as f32;
                }
                let next = ELIMINATION_INTERVAL * (cs.eliminated.len() + 1) as f32;
                cs.time_left = if cs.done {
                    0.0
                } else {
                    (next - cs.time).max(0.0)
                };
            }
        }
        if cs.score != score_before
            && !matches!(
                id,
                ChallengeId::EggHunt | ChallengeId::TurboTimeAttack | ChallengeId::LaserSniper
            )
        {
            new_events.push(Event::ChallengeScore { score: cs.score });
        }
        if cs.done {
            cs.medal = id.medal_for(cs.score);
            new_events.push(Event::ChallengeDone { medal: cs.medal });
        } else if id.higher_is_better() {
            cs.medal = id.medal_for(cs.score);
        }
        let end = cs.done;
        self.challenge = Some(cs);
        for e in new_events {
            self.emit(e);
        }
        if end && self.phase == RacePhase::Racing {
            let now = self.race_time() + DT;
            self.end_race(now);
        }
    }

    /// Puts a racer at `pos` facing `yaw` with `speed` along it (drill dummy placement).
    fn teleport(track: &Track, r: &mut Racer, pos: Vec3, yaw: f32, speed: f32) {
        let pr = track.project(pos, r.track_idx as usize);
        r.pos = Vec3::new(pos.x, pr.ground_y.max(pos.y), pos.z);
        r.yaw = yaw;
        r.visual_yaw = yaw;
        r.yaw_rate = 0.0;
        r.vel = forward(yaw) * speed;
        r.speed = speed;
        r.on_ground = true;
        r.air_h = 0.0;
        r.stun_t = 0.0;
        r.slow_t = 0.0;
        r.stumble_t = 0.0;
        r.drift = Default::default();
        r.track_idx = pr.idx as u32;
        r.s = pr.s;
        r.lateral = pr.lateral;
        r.ground_y = pr.ground_y;
    }

    /// Bash Buddy: the dummy's speed scale (a fraction of top speed).
    fn drill_dummy_speed(&self, i: usize) -> Option<f32> {
        let cmd = self.drill.as_ref()?.dummy?;
        (i == 1 && self.drill.as_ref()?.id == DrillId::BashBuddy).then_some(cmd.speed_frac)
    }

    /// Bash Buddy: the dummy's input, steering at the drill's goal point.
    fn drill_dummy_input(&self, i: usize) -> Option<Input> {
        let cmd = self.drill.as_ref()?.dummy?;
        if i != 1 || self.drill.as_ref()?.id != DrillId::BashBuddy {
            return None;
        }
        let mut inp = Input::default();
        if self.phase == RacePhase::Countdown || self.drill.as_ref()?.over {
            return Some(inp);
        }
        let r = self.racers.get(i)?;
        let to = cmd.goal - r.pos;
        let err = crate::math::wrap_angle(yaw_of(to) - r.yaw);
        inp.steer = (-err * 2.5).clamp(-1.0, 1.0);
        inp.throttle = 1.0;
        Some(inp)
    }

    fn update_drill(&mut self, ev_start: usize) {
        let Some(mut ds) = self.drill.take() else {
            return;
        };
        if ds.over || self.phase != RacePhase::Racing {
            self.drill = Some(ds);
            return;
        }
        let r = &self.racers[0];
        let mut view = RunnerView {
            pos: r.pos,
            vel: r.vel,
            yaw: r.yaw,
            grounded: r.on_ground,
            air_h: r.air_h,
            drifting: r.drift.active,
            spark_tier: r.drift.tier,
            boost_released: None,
            egg_destroyed: false,
            bash_hit: false,
            dummy: (ds.id == DrillId::BashBuddy)
                .then(|| self.racers.get(1))
                .flatten()
                .map(|d| DummyView {
                    pos: d.pos,
                    vel: d.vel,
                    yaw: d.yaw,
                }),
        };
        for e in &self.events[ev_start..] {
            match *e {
                Event::Boost { racer: 0, tier } => view.boost_released = Some(tier),
                Event::LaserZap { by: 0, victim: 1 } if ds.id == DrillId::LaserRange => view.egg_destroyed = true,
                Event::TargetDestroyed {
                    racer: 0,
                    target: 0,
                    ..
                } => view.egg_destroyed = true,
                Event::Bash {
                    attacker: 0,
                    victim: 1,
                    ..
                } if ds.id == DrillId::BashBuddy => view.bash_hit = true,
                _ => {}
            }
        }
        let mut rng = self.rng();
        let evs = ds.step(&view, DT, &mut rng);

        // Effects back on the world.
        if ds.fx.stumble {
            self.hurdle_stumble(0);
        }
        if ds.id == DrillId::LaserRange {
            if let Some(t) = self.targets.first_mut() { t.alive = false; }
            if let Some(dummy) = self.racers.get_mut(1) {
                match ds.egg {
                    Some(target) if ds.fx.egg_spawned => {
                        World::teleport(&self.track, dummy, target.pos - Vec3::Y * TARGET_HEIGHT, 0.0, 0.0);
                        dummy.effects.burn = 0.0;
                        dummy.dazzle_imm = [0; 8];
                        dummy.respawn_t = 0.0;
                    }
                    None => dummy.respawn_t = 1.0e6,
                    _ => {}
                }
            }
        }
        if let Some((pos, yaw)) = ds.dummy.and_then(|c| c.respawn) {
            let track = self.track.clone();
            let speed = ds.dummy.map_or(0.0, |c| c.speed_frac) * self.tun.top_speed;
            if let Some(d) = self.racers.get_mut(1) {
                World::teleport(&track, d, pos, yaw, speed);
            }
        }
        let over = ds.over;
        self.drill = Some(ds);
        for e in evs {
            self.emit(Event::Drill(e));
        }
        if over && self.phase == RacePhase::Racing {
            let now = self.race_time() + DT;
            self.end_race(now);
        }
    }

    fn update_lesson(&mut self, ev_start: usize) {
        let Some(mut ls) = self.lesson.clone() else {
            return;
        };
        if ls.done || ls.failed || self.phase == RacePhase::Countdown {
            return;
        }
        ls.time = self.race_time() + DT;
        let before = ls.progress;
        for e in &self.events[ev_start..] {
            match (ls.id, e) {
                (LessonId::RunSteer, Event::Gate { racer: 0 }) => ls.progress += 1,
                (LessonId::DriftBoost, Event::Boost { racer: 0, tier }) if *tier >= 1 => {
                    ls.progress += 1
                }
                (LessonId::Jump, Event::HurdleCleared { racer: 0 }) => ls.progress += 1,
                (LessonId::Jump, Event::Stumble { racer: 0 }) => ls.progress = 0,
                (LessonId::LaserEyes, Event::LaserZap { by: 0, .. }) => ls.progress += 1,
                (LessonId::HeadBash, Event::Bash { attacker: 0, .. }) => ls.progress += 1,
                (LessonId::CoinsItems, Event::CoinPicked { racer: 0, .. }) => ls.coins += 1,
                (LessonId::CoinsItems, Event::ItemUsed { racer: 0, .. }) => ls.items += 1,
                _ => {}
            }
        }
        if ls.id == LessonId::CoinsItems {
            ls.progress = ls.coins.min(10) + ls.items.min(2);
        }
        ls.progress = ls.progress.min(ls.goal);
        let mut new_events = Vec::new();
        if ls.progress != before {
            new_events.push(Event::LessonProgress {
                progress: ls.progress,
                goal: ls.goal,
            });
        }
        if ls.progress >= ls.goal {
            ls.done = true;
            ls.stars = LessonState::stars_for(ls.id, ls.time);
            new_events.push(Event::LessonDone { stars: ls.stars });
        } else if ls.id.time_limit().is_some_and(|lim| ls.time >= lim) {
            ls.failed = true;
            new_events.push(Event::LessonFailed);
        }
        let end = ls.done || ls.failed;
        self.lesson = Some(ls);
        for e in new_events {
            self.emit(e);
        }
        if end && self.phase == RacePhase::Racing {
            let now = self.race_time() + DT;
            self.end_race(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::challenge::ChallengeId;

    fn ramp_world() -> World {
        let mut w = World::new(RaceConfig::challenge(ChallengeId::RampRider, 0, 1));
        while w.phase == RacePhase::Countdown {
            w.step(&[]);
        }
        w.drain_events();
        w
    }

    fn feed(w: &mut World, e: Event) {
        let start = w.events.len();
        w.emit(e);
        w.update_challenge(start);
    }

    #[test]
    fn bots_roll_a_shared_cake_about_half_as_often_as_a_human_in_the_same_place() {
        // Racer 0 is the human, racer 1 a bot; both rolling from fourth place.
        let mut w = World::new(RaceConfig::quick(TrackId::Savanna, 7, BotLevel::Normal, 5));
        while w.phase == RacePhase::Countdown {
            w.step(&[]);
        }
        let k = w.targets.iter().position(|t| !t.bad).unwrap();
        let mut cakes = [0u32; 2];
        let trials = 4000;
        for _ in 0..trials {
            for i in 0..2 {
                w.racers[i].place = 4;
                w.racers[i].clear_items();
                w.reward_target(i, k);
                cakes[i] += u32::from(w.racers[i].item == Some(Item::SharedCake));
            }
            w.drain_events();
        }
        let (human, bot) = (cakes[0] as f32 / trials as f32, cakes[1] as f32 / trials as f32);
        // The table gives 8% there, a little more once the gap factors thin the others.
        assert!((0.05..0.13).contains(&human), "human {human}");
        assert!(bot > human * 0.35 && bot < human * 0.7, "bot {bot} vs human {human}");
    }

    #[test]
    fn ramp_rider_pays_the_chain_bonus_on_landing_not_take_off() {
        let mut w = ramp_world();
        // First trick: lands (+1), nothing to chain.
        feed(&mut w, Event::Trick { racer: 0 });
        feed(&mut w, Event::TrickBoost { racer: 0 });
        let cs = w.challenge.clone().unwrap();
        assert_eq!((cs.score, cs.misses, cs.chain), (1.0, 0, true));
        // Chained trick while boosting: nothing at take-off ...
        w.racers[0].boost_t = 0.5;
        feed(&mut w, Event::Trick { racer: 0 });
        let cs = w.challenge.clone().unwrap();
        assert_eq!((cs.score, cs.misses), (1.0, 0), "no bonus before landing");
        // ... +1 +2 when it lands.
        feed(&mut w, Event::TrickBoost { racer: 0 });
        let cs = w.challenge.clone().unwrap();
        assert_eq!((cs.score, cs.misses, cs.progress), (4.0, 1, 2));

        // A chained trick that never lands (fell, respawned) pays nothing, and
        // does not leak into the next trick.
        w.racers[0].boost_t = 0.5;
        feed(&mut w, Event::Trick { racer: 0 });
        w.racers[0].boost_t = 0.0;
        feed(&mut w, Event::Trick { racer: 0 }); // next ramp, not boosting
        feed(&mut w, Event::TrickBoost { racer: 0 });
        let cs = w.challenge.clone().unwrap();
        assert_eq!((cs.score, cs.misses), (5.0, 1), "{cs:?}");
    }
}
