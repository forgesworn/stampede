//! Battle mode "Feather Frenzy" (spec 17): every ostrich carries feathers;
//! hits pop them, a bash steals one, at zero you are out. Last one standing
//! (or the best record when the clock runs out) wins.
//!
//! The world keeps a [`BattleState`] in `World::battle` whenever the mode is
//! [`crate::RaceMode::Battle`] or the Last Feather challenge; every battle rule
//! in the sim keys on that, not on the mode.

use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::items::ItemRoll;

/// How long an out racer takes to fade (seconds); the renderer reads
/// [`BattleState::fade`].
pub const BATTLE_OUT_FADE: f32 = 1.5;

/// Battle settings. Integer fields so [`crate::RaceMode`] stays `Copy + Eq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct BattleRules {
    /// Feathers each ostrich starts with (default 3).
    pub feathers: u8,
    /// Most feathers a racer can hold; a bash only steals below this (default 5).
    pub max_feathers: u8,
    /// Seconds on the clock (default 180).
    pub time_limit: u16,
    /// Invulnerability after losing a feather, in deciseconds (default 15 = 1.5 s).
    pub invuln_ds: u8,
    /// Power Egg respawn, in deciseconds (default 30 = 3 s).
    pub egg_respawn_ds: u8,
}

impl Default for BattleRules {
    fn default() -> Self {
        BattleRules {
            feathers: 3,
            max_feathers: 5,
            time_limit: 180,
            invuln_ds: 15,
            egg_respawn_ds: 30,
        }
    }
}

impl BattleRules {
    /// Seconds of invulnerability after losing a feather.
    pub fn invuln(&self) -> f32 {
        self.invuln_ds as f32 / 10.0
    }

    /// Power Egg respawn time (seconds).
    pub fn egg_respawn(&self) -> f32 {
        self.egg_respawn_ds as f32 / 10.0
    }

    /// The clock (seconds).
    pub fn time_limit_s(&self) -> f32 {
        self.time_limit as f32
    }
}

/// A battle in progress (`World::battle`). Vectors are indexed by racer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BattleState {
    /// The rules.
    pub rules: BattleRules,
    /// Seconds left on the clock.
    pub time_left: f32,
    /// Feathers held (0 = out).
    pub feathers: Vec<u8>,
    /// Feathers popped by each racer (hits landed; bash steals included).
    pub hits: Vec<u16>,
    /// Feathers each racer has lost.
    pub taken: Vec<u16>,
    /// Invulnerability left (seconds). While > 0 the racer flashes and attacks
    /// pass through it (eggs, cacti, bashes, Mega Laser, Lightning).
    pub invuln: Vec<f32>,
    /// Racers out, in the order they went out.
    pub out_order: Vec<usize>,
    /// Seconds since each racer went out (0 while in).
    pub out_t: Vec<f32>,
    /// Over (one left, the clock ran out, or the Last Feather player is out).
    pub done: bool,
}

impl BattleState {
    /// A fresh battle for `n` racers.
    pub fn new(rules: BattleRules, n: usize) -> BattleState {
        BattleState {
            rules,
            time_left: rules.time_limit_s(),
            feathers: vec![rules.feathers.max(1); n],
            hits: vec![0; n],
            taken: vec![0; n],
            invuln: vec![0.0; n],
            out_order: Vec::new(),
            out_t: vec![0.0; n],
            done: false,
        }
    }

    /// Whether racer `i` is out.
    pub fn is_out(&self, i: usize) -> bool {
        self.out_order.contains(&i)
    }

    /// Whether racer `i` is in its post-hit invulnerability (draw it flashing).
    pub fn flashing(&self, i: usize) -> bool {
        self.invuln.get(i).is_some_and(|&t| t > 0.0)
    }

    /// Opacity for an out racer's fade: 1 while in, then 1 -> 0 over
    /// [`BATTLE_OUT_FADE`] seconds after going out. (The sim freezes an out
    /// racer with `respawn_t` set, so a renderer that hides respawning racers
    /// must special-case this.)
    pub fn fade(&self, i: usize) -> f32 {
        if !self.is_out(i) {
            return 1.0;
        }
        (1.0 - self.out_t.get(i).copied().unwrap_or(0.0) / BATTLE_OUT_FADE).clamp(0.0, 1.0)
    }

    /// Racers still in.
    pub fn alive(&self) -> Vec<usize> {
        (0..self.feathers.len())
            .filter(|&i| !self.is_out(i))
            .collect()
    }

    /// The final place an out racer got (`None` while in).
    pub fn out_place(&self, i: usize) -> Option<u8> {
        let k = self.out_order.iter().position(|&e| e == i)?;
        Some((self.feathers.len() - k) as u8)
    }

    /// Racer indices, 1st first: racers still in by feathers (most first),
    /// then hits landed (most first), then hits taken (fewest first), then
    /// index; then the out racers, the latest out first.
    pub fn ranking(&self) -> Vec<usize> {
        let mut alive = self.alive();
        alive.sort_by(|&a, &b| {
            self.feathers[b]
                .cmp(&self.feathers[a])
                .then(self.hits[b].cmp(&self.hits[a]))
                .then(self.taken[a].cmp(&self.taken[b]))
                .then(a.cmp(&b))
        });
        alive.extend(self.out_order.iter().rev().copied());
        alive
    }

    /// 1-based place of racer `i` in [`BattleState::ranking`].
    pub fn place_of(&self, i: usize) -> u8 {
        self.ranking()
            .iter()
            .position(|&r| r == i)
            .map_or(0, |p| p as u8 + 1)
    }
}

/// Legacy battle pickups; race eggs use the five playful power-ups.
pub const BATTLE_ROLLS: [ItemRoll; 9] = [
    ItemRoll { item: crate::Item::Feather, uses: 1 },
    ItemRoll { item: crate::Item::Feather, uses: 3 },
    ItemRoll { item: crate::Item::RollingEgg, uses: 1 },
    ItemRoll { item: crate::Item::HunterEgg, uses: 1 },
    ItemRoll { item: crate::Item::Cactus, uses: 1 },
    ItemRoll { item: crate::Item::Shield, uses: 1 },
    ItemRoll { item: crate::Item::Lightning, uses: 1 },
    ItemRoll { item: crate::Item::MegaLaser, uses: 1 },
    ItemRoll { item: crate::Item::Magnet, uses: 1 },
];

/// Battle item weights, by the columns of [`BATTLE_ROLLS`] (F1, F3, Egg, Hunter,
/// Cactus, Shield, Lightning, Mega, Magnet): mostly things that pop feathers.
pub const BATTLE_ODDS: [u16; 9] = [6, 4, 36, 14, 16, 12, 2, 6, 4];

/// Rolls a battle item (place does not matter in a battle).
pub fn roll_battle_item<R: Rng>(rng: &mut R) -> ItemRoll {
    let total: u32 = BATTLE_ODDS.iter().map(|&w| w as u32).sum();
    let mut x = rng.gen_range(0..total);
    for (k, &w) in BATTLE_ODDS.iter().enumerate() {
        if x < w as u32 {
            return BATTLE_ROLLS[k];
        }
        x -= w as u32;
    }
    BATTLE_ROLLS[2]
}
