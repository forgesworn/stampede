//! Challenges: ten one-off scored modes (spec 16, 17). Racer 0 is the player; the
//! sim updates [`ChallengeState`] from its own events, like a lesson.

use serde::{Deserialize, Serialize};

use crate::bot::BotLevel;
use crate::track::TrackId;

/// Seconds between eliminations in [`ChallengeId::Elimination`].
pub const ELIMINATION_INTERVAL: f32 = 25.0;

/// Laser Sniper: seconds added per miss (laser fired [`SNIPER_MISS_TIME`] without a hit).
pub const SNIPER_MISS_PENALTY: f32 = 2.0;
/// Laser Sniper: firing time without a hit that counts as one miss.
pub const SNIPER_MISS_TIME: f32 = 0.6;

/// The challenges, in menu order (the original six, then four more).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChallengeId {
    /// 90 s on Oasis Run: grab as many bitcoins as you can.
    CoinRush,
    /// Destroy 20 Power Eggs on Baobab Bayou as fast as you can.
    EggHunt,
    /// Every 25 s the last-placed ostrich is out; be the last one standing.
    Elimination,
    /// 2 laps of Red Rock Canyon alone: score drift points.
    DriftKing,
    /// 60 s on the Training Ground: bash everyone.
    BashBrawl,
    /// 3 laps of Halving Highway alone, three Turbo Feathers in hand.
    TurboTimeAttack,
    /// Destroy 15 Power Eggs on Volcano Vault; every miss costs 2 s.
    LaserSniper,
    /// 2 laps of Oasis Run alone: land ramp tricks, chain them while boosting.
    RampRider,
    /// 120 s on Halving Highway at night: coins; boost pads bring the lines back.
    NightCoinRush,
    /// Battle on Dust Bowl with one feather against five three-feather rivals.
    LastFeather,
}

impl ChallengeId {
    /// All challenges in menu order.
    pub const ALL: [ChallengeId; 10] = [
        ChallengeId::CoinRush,
        ChallengeId::EggHunt,
        ChallengeId::Elimination,
        ChallengeId::DriftKing,
        ChallengeId::BashBrawl,
        ChallengeId::TurboTimeAttack,
        ChallengeId::LaserSniper,
        ChallengeId::RampRider,
        ChallengeId::NightCoinRush,
        ChallengeId::LastFeather,
    ];

    /// 1-based number.
    pub fn number(self) -> u8 {
        ChallengeId::ALL
            .iter()
            .position(|&c| c == self)
            .unwrap_or(0) as u8
            + 1
    }

    /// Title.
    pub fn title(self) -> &'static str {
        match self {
            ChallengeId::CoinRush => "Coin Rush",
            ChallengeId::EggHunt => "Egg Hunt",
            ChallengeId::Elimination => "Elimination",
            ChallengeId::DriftKing => "Drift King",
            ChallengeId::BashBrawl => "Bash Brawl",
            ChallengeId::TurboTimeAttack => "Turbo Time Attack",
            ChallengeId::LaserSniper => "Laser Sniper",
            ChallengeId::RampRider => "Ramp Rider",
            ChallengeId::NightCoinRush => "Night Coin Rush",
            ChallengeId::LastFeather => "Last Feather",
        }
    }

    /// One-line description.
    pub fn description(self) -> &'static str {
        match self {
            ChallengeId::CoinRush => {
                "90 seconds on Oasis Run. Grab as many bitcoins as you can: they grow back in 3 seconds."
            }
            ChallengeId::EggHunt => {
                "Collect 20 eggs on Baobab Bayou as fast as you can. The eggs grow back quickly."
            }
            ChallengeId::Elimination => {
                "Seven rivals on Dune Drift. Every 25 seconds the last-placed ostrich is out. Be the last one standing."
            }
            ChallengeId::DriftKing => {
                "Two laps of Red Rock Canyon, alone. Long, high-tier drifts score the most."
            }
            ChallengeId::BashBrawl => {
                "60 seconds on the Training Ground with five rivals. Head-bash them; stealing an item counts double."
            }
            ChallengeId::TurboTimeAttack => {
                "Three laps of Halving Highway, alone, with three Turbo Feathers to spend. Beat the clock."
            }
            ChallengeId::LaserSniper => {
                "Burn 15 opponents’ tails on Volcano Vault as fast as you can. Every miss (0.6 s of laser that hits nothing) costs 2 seconds."
            }
            ChallengeId::RampRider => {
                "Two laps of Oasis Run, alone. Land ramp tricks; hit the next ramp while boosting to chain them for double."
            }
            ChallengeId::NightCoinRush => {
                "120 seconds on Halving Highway at night. Grab bitcoins: boost pads bring the coin lines back."
            }
            ChallengeId::LastFeather => {
                "Battle in the Dust Bowl with one feather against five rivals with three. Outlast them."
            }
        }
    }

    /// The track the challenge is played on.
    pub fn track(self) -> TrackId {
        match self {
            ChallengeId::CoinRush => TrackId::Oasis,
            ChallengeId::EggHunt => TrackId::Baobab,
            ChallengeId::Elimination => TrackId::Dune,
            ChallengeId::DriftKing => TrackId::Canyon,
            ChallengeId::BashBrawl => TrackId::Training,
            ChallengeId::TurboTimeAttack => TrackId::Halving,
            ChallengeId::LaserSniper => TrackId::Volcano,
            ChallengeId::RampRider => TrackId::Oasis,
            ChallengeId::NightCoinRush => TrackId::Halving,
            ChallengeId::LastFeather => TrackId::DustBowl,
        }
    }

    /// `RaceConfig::laps` for the challenge. Only Drift King, Turbo Time
    /// Attack and Ramp Rider end on finishing their laps; for the others the lap counter is
    /// meaningless and a HUD should hide it.
    pub fn laps(self) -> u8 {
        match self {
            ChallengeId::DriftKing => 2,
            ChallengeId::TurboTimeAttack => 3,
            ChallengeId::RampRider => 2,
            _ => 5,
        }
    }

    /// Whether the challenge ends when the player finishes the race laps.
    pub fn finishing(self) -> bool {
        matches!(
            self,
            ChallengeId::DriftKing | ChallengeId::TurboTimeAttack | ChallengeId::RampRider
        )
    }

    /// Fixed time limit (seconds from GO), if the challenge has one. Coin Rush,
    /// Night Coin Rush and Bash Brawl end at it; Egg Hunt and Laser Sniper give
    /// up at it (no medal); Last Feather is the battle clock.
    pub fn time_limit(self) -> Option<f32> {
        match self {
            ChallengeId::CoinRush => Some(90.0),
            ChallengeId::BashBrawl => Some(60.0),
            ChallengeId::EggHunt => Some(150.0),
            ChallengeId::LaserSniper => Some(150.0),
            ChallengeId::NightCoinRush => Some(120.0),
            ChallengeId::LastFeather => Some(180.0),
            _ => None,
        }
    }

    /// The counter goal, where there is one: Egg Hunt 20 eggs; Elimination 7
    /// eliminations (one racer left of eight); Laser Sniper 15 rival hits; Last
    /// Feather 5 rivals out.
    pub fn goal(self) -> u32 {
        match self {
            ChallengeId::EggHunt => 20,
            ChallengeId::Elimination => 7,
            ChallengeId::LaserSniper => 15,
            ChallengeId::LastFeather => 5,
            _ => 0,
        }
    }

    /// The rivals that join the player (seat 0), in seat order.
    pub fn rivals(self) -> Vec<BotLevel> {
        match self {
            ChallengeId::Elimination => vec![BotLevel::Normal; 7],
            ChallengeId::LastFeather => vec![BotLevel::Normal; 5],
            ChallengeId::BashBrawl => vec![
                BotLevel::Easy,
                BotLevel::Easy,
                BotLevel::Easy,
                BotLevel::Dummy,
                BotLevel::Dummy,
            ],
            _ => Vec::new(),
        }
    }

    /// Whether a higher score is better (coins, drift points, bashes), or a
    /// lower one (times, Elimination place).
    pub fn higher_is_better(self) -> bool {
        matches!(
            self,
            ChallengeId::CoinRush
                | ChallengeId::DriftKing
                | ChallengeId::BashBrawl
                | ChallengeId::RampRider
                | ChallengeId::NightCoinRush
        )
    }

    /// Bronze, silver and gold thresholds. With [`ChallengeId::higher_is_better`]
    /// a score at or above the value earns the medal (ascending); otherwise at
    /// or below it (descending: bronze is the loosest).
    pub fn medal_thresholds(self) -> [f32; 3] {
        match self {
            ChallengeId::CoinRush => [70.0, 100.0, 130.0],
            ChallengeId::EggHunt => [80.0, 62.0, 48.0],
            ChallengeId::Elimination => [3.0, 2.0, 1.0],
            ChallengeId::DriftKing => [15.0, 35.0, 60.0],
            ChallengeId::BashBrawl => [5.0, 10.0, 16.0],
            ChallengeId::TurboTimeAttack => [150.0, 138.0, 128.0],
            ChallengeId::LaserSniper => LASER_SNIPER_MEDALS,
            ChallengeId::RampRider => RAMP_RIDER_MEDALS,
            ChallengeId::NightCoinRush => NIGHT_COIN_RUSH_MEDALS,
            ChallengeId::LastFeather => [3.0, 2.0, 1.0],
        }
    }

    /// Medal for `score`: 0 none, 1 bronze, 2 silver, 3 gold.
    pub fn medal_for(self, score: f32) -> u8 {
        let t = self.medal_thresholds();
        let hit = |th: f32| {
            if self.higher_is_better() {
                score >= th
            } else {
                score <= th
            }
        };
        if hit(t[2]) {
            3
        } else if hit(t[1]) {
            2
        } else if hit(t[0]) {
            1
        } else {
            0
        }
    }

    /// What the score counts, for the HUD and results.
    pub fn score_label(self) -> &'static str {
        match self {
            ChallengeId::CoinRush => "Bitcoins",
            ChallengeId::EggHunt => "Time",
            ChallengeId::Elimination => "Place",
            ChallengeId::DriftKing => "Drift points",
            ChallengeId::BashBrawl => "Bashes",
            ChallengeId::TurboTimeAttack => "Time",
            ChallengeId::LaserSniper => "Time",
            ChallengeId::RampRider => "Trick points",
            ChallengeId::NightCoinRush => "Bitcoins",
            ChallengeId::LastFeather => "Place",
        }
    }
}

/// A challenge in progress (`World::challenge`). The sim updates it from its
/// own events; racer 0 is the player.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChallengeState {
    /// Which challenge.
    pub id: ChallengeId,
    /// The score so far (see [`ChallengeId::score_label`]). For time challenges
    /// it runs with the clock and freezes at the final time; for Elimination it
    /// is the player's current place, final once the player is out or has won.
    /// Laser Sniper: time + penalties. Ramp Rider: tricks landed + 2 x chained
    /// tricks. Last Feather: the player's battle place.
    pub score: f32,
    /// Seconds left on the clock: Coin Rush and Bash Brawl count down to 0;
    /// Egg Hunt counts down to its give-up limit; Elimination shows the time to
    /// the next elimination; Laser Sniper counts to its give-up limit, Night
    /// Coin Rush to 0, Last Feather is the battle clock; 0 otherwise.
    pub time_left: f32,
    /// Racers removed so far (seat indices), in elimination order.
    pub eliminated: Vec<usize>,
    /// Over.
    pub done: bool,
    /// 0 none, 1 bronze, 2 silver, 3 gold. Final once `done`; for the
    /// higher-is-better challenges it also tracks the score live.
    pub medal: u8,
    /// Seconds since GO (frozen once done: in Last Feather, the time survived).
    pub time: f32,
    /// Counter: coins grabbed, eggs destroyed, racers eliminated, drift
    /// releases that scored, bashes landed (not doubled), laps completed (Turbo),
    /// rival hits (Laser Sniper), tricks landed (Ramp Rider), coins (Night Coin
    /// Rush), rivals out (Last Feather).
    pub progress: u32,
    /// The counter's goal (`ChallengeId::goal`), 0 if none.
    pub goal: u32,
    /// Laser Sniper: seconds of miss penalties so far (included in `score`).
    #[serde(default)]
    pub penalty: f32,
    /// Laser Sniper: misses so far. Ramp Rider: chained tricks so far.
    #[serde(default)]
    pub misses: u32,
    /// Laser Sniper: firing time since the last kill that has not hit anything.
    #[serde(default)]
    pub miss_t: f32,
    /// Ramp Rider: a trick has been landed since the last trick started (the
    /// next one chains if it starts while boosting).
    #[serde(default)]
    pub chain: bool,
    /// Ramp Rider: the trick in the air started while boosting after a landed
    /// one; the chain bonus is paid when it lands.
    #[serde(default)]
    pub chain_pending: bool,
}

impl ChallengeState {
    /// A fresh challenge.
    pub fn new(id: ChallengeId) -> Self {
        ChallengeState {
            id,
            score: 0.0,
            time_left: match id {
                ChallengeId::Elimination => ELIMINATION_INTERVAL,
                _ => id.time_limit().unwrap_or(0.0),
            },
            eliminated: Vec::new(),
            done: false,
            medal: 0,
            time: 0.0,
            progress: 0,
            goal: id.goal(),
            penalty: 0.0,
            misses: 0,
            miss_t: 0.0,
            chain: false,
            chain_pending: false,
        }
    }
}

/// Laser Sniper medals (seconds incl. penalties; lower is better).
const LASER_SNIPER_MEDALS: [f32; 3] = [75.0, 62.0, 52.0];
/// Ramp Rider medals (trick points; higher is better; 16 is the maximum).
const RAMP_RIDER_MEDALS: [f32; 3] = [6.0, 9.0, 12.0];
/// Night Coin Rush medals (coins; higher is better).
const NIGHT_COIN_RUSH_MEDALS: [f32; 3] = [95.0, 130.0, 165.0];
