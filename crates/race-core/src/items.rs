//! Power-ups (spec 5.4), the by-place odds table, and the world's pickups and
//! hazards: projectiles, cacti, coins and Power Egg targets.

use glam::Vec3;
use rand::Rng;
use serde::{Deserialize, Serialize};

/// A held power-up. A "x3" Feather is `Feather` with 3 uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Item {
    /// Turbo Feather: a boost per use.
    Feather,
    /// Rolling Egg: fired forward (or back while looking back), bounces off walls.
    RollingEgg,
    /// Hunter Egg: homes on the racer one place ahead.
    HunterEgg,
    /// Cactus: dropped behind.
    Cactus,
    /// HODL Shield: blocks one hit.
    Shield,
    /// Lightning Strike: zaps everyone ahead.
    Lightning,
    /// Mega Laser: a huge beam straight ahead.
    MegaLaser,
    /// Sat Magnet: pulls coins in.
    Magnet,
    /// Transfers opponents’ race Bitcoin, never their banked savings.
    Laptop,
    /// Invincible automatic steering.
    OrangePill,
    /// Globally swaps good and bad eggs for a short time.
    EggScrambler,
    /// Everyone else eats cake: you take their good power-ups, and their bad
    /// luck ends.
    SharedCake,
    /// Hand-cranked, auto-aimed dollar bills that unseat opponents.
    MoneyPrinter,
    /// A persistent NPC rug trap placed at the roadside.
    RugPull,
    /// Catch-up rocket: the ostrich fires a laser from its backside and
    /// blasts up the track, smashing racers aside. Never offered to the front.
    LaserTrump,
    /// A rose seller runs to every other racer in turn and leans on them for a
    /// few seconds each. Never offered to the front.
    Rose,
}

impl Item {
    /// All items, in a fixed order (wire index = position).
    pub const ALL: [Item; 16] = [
        Item::Feather,
        Item::RollingEgg,
        Item::HunterEgg,
        Item::Cactus,
        Item::Shield,
        Item::Lightning,
        Item::MegaLaser,
        Item::Magnet,
        Item::Laptop,
        Item::OrangePill,
        Item::EggScrambler,
        Item::SharedCake,
        Item::MoneyPrinter,
        Item::RugPull,
        Item::LaserTrump,
        Item::Rose,
    ];

    /// Wire index (0..16).
    pub fn index(self) -> u8 {
        Item::ALL.iter().position(|&i| i == self).unwrap_or(0) as u8
    }

    /// Inverse of [`Item::index`].
    pub fn from_index(i: u8) -> Option<Item> {
        Item::ALL.get(i as usize).copied()
    }

    /// Whether the item acts on its holder (a boost, a shield, a pull, an
    /// immunity, the rocket, the cake) rather than being thrown or dropped at
    /// others. A Shared Cake takes these.
    pub fn is_good(self) -> bool {
        matches!(
            self,
            Item::Feather
                | Item::Shield
                | Item::Magnet
                | Item::OrangePill
                | Item::LaserTrump
                | Item::SharedCake
        )
    }

    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            Item::Feather => "Turbo Feather",
            Item::RollingEgg => "Rolling Egg",
            Item::HunterEgg => "Hunter Egg",
            Item::Cactus => "Cactus",
            Item::Shield => "HODL Shield",
            Item::Lightning => "Lightning Strike",
            Item::MegaLaser => "Laser Eyes",
            Item::Magnet => "Sat Magnet",
            Item::Laptop => "Bitcoin Hack",
            Item::OrangePill => "Orange Pill",
            Item::EggScrambler => "Egg Scrambler",
            Item::SharedCake => "Shared Cake",
            Item::MoneyPrinter => "Printer Goes Burr",
            Item::RugPull => "Rug Pull",
            Item::LaserTrump => "Laser Trump",
            Item::Rose => "Rose",
        }
    }
}

/// One entry of the odds table: an item and how many uses it comes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRoll {
    /// Item.
    pub item: Item,
    /// Uses.
    pub uses: u8,
}

/// Most power-ups a racer can have queued behind the held one.
pub const MAX_QUEUE: usize = 8;

/// A racer's queue off the wire: at most [`MAX_QUEUE`] entries, each with at
/// least one use.
pub(crate) fn bounded_queue<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ItemRoll>, D::Error> {
    struct Queue;
    impl<'de> serde::de::Visitor<'de> for Queue {
        type Value = Vec<ItemRoll>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most eight queued items")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            if seq.size_hint().unwrap_or(0) > MAX_QUEUE {
                return Err(serde::de::Error::custom("too many queued items"));
            }
            let mut queue = Vec::with_capacity(MAX_QUEUE);
            while let Some(roll) = seq.next_element::<ItemRoll>()? {
                if queue.len() == MAX_QUEUE || roll.uses == 0 {
                    return Err(serde::de::Error::custom("invalid item queue"));
                }
                queue.push(roll);
            }
            Ok(queue)
        }
    }
    d.deserialize_seq(Queue)
}

/// The rolls the odds table can give, column order of [`ITEM_ODDS`].
pub const ITEM_ROLLS: [ItemRoll; 8] = [
    ItemRoll {
        item: Item::MegaLaser,
        uses: 1,
    },
    ItemRoll { item: Item::RugPull, uses: 1 },
    ItemRoll {
        item: Item::Laptop,
        uses: 1,
    },
    ItemRoll {
        item: Item::OrangePill,
        uses: 1,
    },
    ItemRoll {
        item: Item::EggScrambler,
        uses: 1,
    },
    ItemRoll {
        item: Item::SharedCake,
        uses: 1,
    },
    ItemRoll {
        item: Item::MoneyPrinter,
        uses: 1,
    },
    ItemRoll { item: Item::Rose, uses: 1 },
];

// Roll tuning. Everything that shapes the odds lives in this block.

/// Percentages for Laser Eyes, Rug, Laptop, Orange Pill, Whisk, Cake, Printer
/// and Rose, one row per [`odds_bucket`] (front to back). These are the base
/// weights; the gap factors below then scale them for the racer actually
/// rolling. The Cake's weight was halved (its share handed to the others in
/// proportion, the Scrambler's fixed at three), because it took power-ups too
/// often for the racers it was used on.
pub const ITEM_ODDS: [[u16; 8]; 4] = [
    [7, 34, 7, 16, 3, 20, 13, 0],
    [18, 17, 12, 17, 3, 12, 17, 4],
    [18, 9, 17, 20, 3, 8, 16, 9],
    [15, 2, 22, 27, 3, 2, 21, 8],
];

/// How far ahead (m) the Printer can pick a target: the auto-aim range in
/// `mischief.rs`, which uses this value.
pub const PRINTER_REACH: f32 = 55.0;
/// Length (m) of the Mega Laser beam. Mirrors `Tunables::mega_range` (a test
/// keeps the two equal).
pub const LASER_REACH: f32 = 60.0;
/// Rug Pull: nearest racer behind within this many metres gives full weight.
pub const RUG_FULL_GAP: f32 = 60.0;
/// Rug Pull: weight has fallen to nothing by this gap behind (m).
pub const RUG_ZERO_GAP: f32 = 150.0;
/// Orange Pill: weight factor when nobody is ahead.
pub const PILL_ALONE_FACTOR: f32 = 0.5;
/// Orange Pill: extra weight (on top of 1.0) at a full [`PILL_GAP_SPAN`] ahead.
pub const PILL_GAP_BONUS: f32 = 0.5;
/// Orange Pill: the gap ahead (m) at which the bonus is complete.
pub const PILL_GAP_SPAN: f32 = 60.0;

/// Laser Trump: chance for the very last place before the gap factor.
pub const TRUMP_MAX_CHANCE: f32 = 0.45;
/// Laser Trump: a gap ahead (m) of this or more leaves the chance unreduced.
pub const TRUMP_FULL_GAP: f32 = 40.0;
/// Laser Trump: the smallest the gap factor can make the chance (a pack
/// still gets a quarter of it).
pub const TRUMP_MIN_GAP_FACTOR: f32 = 0.25;

/// Column of Shared Cake in [`ITEM_ROLLS`]: the fallback roll.
const ROLL_CAKE: usize = 5;

/// Where a rolling racer sits relative to the others, in metres of course
/// progress. The place bucket gives the base odds; these scale them.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RollGaps {
    /// Gap to the nearest unfinished racer ahead (`None`: nobody ahead).
    pub ahead: Option<f32>,
    /// Gap to the nearest unfinished racer behind (`None`: nobody behind).
    pub behind: Option<f32>,
    /// Whether any other racer holds a coin the Bitcoin Hack could take.
    pub others_hold_coins: bool,
}

/// The odds bucket for a place (1-based) in a field of `n`.
pub fn odds_bucket(place: u8, n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    let f = (place.max(1) - 1) as f32 / (n - 1) as f32;
    if f < 0.15 {
        0
    } else if f < 1.0 / 3.0 {
        1
    } else if f < 2.0 / 3.0 {
        2
    } else {
        3
    }
}

/// 1.0 up to `reach`, falling linearly to 0.0 at twice `reach`; 0.0 with no
/// racer to hit.
fn reach_factor(gap_ahead: Option<f32>, reach: f32) -> f32 {
    match gap_ahead {
        None => 0.0,
        Some(g) => (2.0 - g.max(0.0) / reach).clamp(0.0, 1.0),
    }
}

/// The 0..=1.5 factor applied to roll `i` of [`ITEM_ROLLS`] for these gaps.
pub fn gap_factor(roll: usize, gaps: &RollGaps) -> f32 {
    match ITEM_ROLLS[roll].item {
        Item::MoneyPrinter => reach_factor(gaps.ahead, PRINTER_REACH),
        Item::MegaLaser => reach_factor(gaps.ahead, LASER_REACH),
        Item::RugPull => match gaps.behind {
            None => 0.0,
            Some(g) => {
                ((RUG_ZERO_GAP - g.max(0.0)) / (RUG_ZERO_GAP - RUG_FULL_GAP)).clamp(0.0, 1.0)
            }
        },
        Item::OrangePill => match gaps.ahead {
            None => PILL_ALONE_FACTOR,
            Some(g) => 1.0 + PILL_GAP_BONUS * (g / PILL_GAP_SPAN).clamp(0.0, 1.0),
        },
        Item::Laptop => {
            if gaps.others_hold_coins {
                1.0
            } else {
                0.0
            }
        }
        // Nobody else racing: nobody to sell to.
        Item::Rose => {
            if gaps.ahead.is_some() || gaps.behind.is_some() {
                1.0
            } else {
                0.0
            }
        }
        _ => 1.0,
    }
}

/// Rolls an item for a racer in `place` of `n`: the bucket's base odds, each
/// scaled by [`gap_factor`]. Falls back to Shared Cake if every weight is 0.
pub fn roll_item<R: Rng>(rng: &mut R, place: u8, n: usize, gaps: RollGaps) -> ItemRoll {
    let row = &ITEM_ODDS[odds_bucket(place, n)];
    // Integer thousandths keep the draw exact and portable.
    let mut weights = [0u32; 8];

    for (i, w) in weights.iter_mut().enumerate() {
        *w = (row[i] as f32 * gap_factor(i, &gaps) * 1000.0).round() as u32;
    }
    let total: u32 = weights.iter().sum();
    if total == 0 {
        return ITEM_ROLLS[ROLL_CAKE];
    }
    let mut x = rng.gen_range(0..total);
    for (i, &w) in weights.iter().enumerate() {
        if x < w {
            return ITEM_ROLLS[i];
        }
        x -= w;
    }
    ITEM_ROLLS[ROLL_CAKE]
}

/// Chance (0..1) that an egg gives this racer a Laser Trump instead of its roll.
/// Grows with the square of how far back the racer is, and is cut when a
/// racer is close ahead (a pack needs no rocket). The leader, a field of fewer
/// than three, or a racer with nobody ahead gets 0.
pub fn trump_chance(place: u8, n: usize, gap_ahead: Option<f32>) -> f32 {
    let Some(gap) = gap_ahead else {
        return 0.0;
    };
    if n < 3 || place <= 1 {
        return 0.0;
    }
    let f = (place.min(n as u8) - 1) as f32 / (n - 1) as f32;
    let gap_factor = (gap / TRUMP_FULL_GAP).clamp(TRUMP_MIN_GAP_FACTOR, 1.0);
    TRUMP_MAX_CHANCE * f * f * gap_factor
}

/// Projectile kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectileKind {
    /// Rolling Egg.
    RollingEgg,
    /// Hunter Egg.
    HunterEgg,
    /// A guided paper dollar bill.
    DollarBill,
}

/// A live egg.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Projectile {
    /// Kind.
    pub kind: ProjectileKind,
    /// Position (centre of the egg).
    pub pos: Vec3,
    /// Velocity.
    pub vel: Vec3,
    /// Who fired it.
    pub owner: u8,
    /// Homing target (Hunter Egg).
    pub target: Option<u8>,
    /// Time alive.
    pub age: f32,
    /// Wall bounces so far.
    pub bounces: u8,
    /// Track hint (sample index).
    pub hint: u32,
    /// Hunter: arc length it is following.
    pub s: f32,
    /// Hunter: lateral offset it is following.
    pub lateral: f32,
    /// Unique id (for renderer interpolation).
    pub id: u32,
}

/// Hazard kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HazardKind {
    /// A dropped cactus.
    Cactus,
}

/// A static hazard on the road.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hazard {
    /// Kind.
    pub kind: HazardKind,
    /// Position (base on the ground).
    pub pos: Vec3,
    /// Who dropped it.
    pub owner: u8,
    /// Time alive.
    pub age: f32,
    /// Unique id.
    pub id: u32,
}

/// A fixed coin spot (index is stable for the netcode bit vector).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoinSpot {
    /// Where it lives.
    pub home: Vec3,
    /// Where it is now (moves toward a magnet).
    pub pos: Vec3,
    /// Visible and collectable.
    pub alive: bool,
    /// Time until it respawns.
    pub respawn_t: f32,
}

/// A dropped coin (scattered behind a hit racer), a pickup for a while.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LooseCoin {
    /// Position.
    pub pos: Vec3,
    /// Velocity (decays).
    pub vel: Vec3,
    /// Remaining life.
    pub ttl: f32,
    /// The racer who dropped it cannot pick it up for a moment.
    pub dropped_by: u8,
    /// Age.
    pub age: f32,
    /// Track hint.
    pub hint: u32,
}

/// A contact pickup laid by a passing ostrich.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// Centre.
    pub pos: Vec3,
    /// Which row (index of the `TargetRow` feature among target rows).
    pub row: u16,
    /// Alive.
    pub alive: bool,
    /// Time until respawn.
    pub respawn_t: f32,
    /// The one bad egg in its row, before any scrambler inversion.
    pub bad: bool,
    /// Legacy drill marker (ordinary eggs never interact with lasers).
    pub beam_t: f32,
}
