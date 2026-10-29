//! Progression: the ₿ wallet ("sats"), what races pay, the colour variants sold in
//! the Garage, and the achievements. Pure logic over [`Profile`] (no drawing), so it
//! is all unit-tested; `progress_ui` draws it.
//!
//! The wallet is `Profile::progress.sats` (a `u64`: it cannot go negative). Earnings
//! are added by [`settle`], once per finished race, by the first `update` of the
//! screen shown after it. Purchases go through [`buy`], which checks and deducts in
//! one step; the caller saves the profile straight after.

use crate::flow::{RaceKind, RaceOutcome, RaceStats};
use crate::platform::data_dir::{Profile, OSTRICHES};
use race_core::TrackId;

// ── earning ─────────────────────────────────────────────────────────────────

/// ₿ for finishing 1st, 2nd, 3rd and anywhere else (races and battles).
pub const PLACE_PAY: [u32; 4] = [50, 30, 20, 10];
/// ₿ for the first bronze, silver and gold in a challenge (each tier pays once).
pub const MEDAL_PAY: [u32; 3] = [30, 60, 120];
/// ₿ per new star in a training lesson (only the stars above your best so far).
pub const STAR_PAY: u32 = 20;
/// ₿ per new star in an open-ground drill (only the stars above your best so far).
pub const DRILL_STAR_PAY: u32 = 15;
/// ₿ for the Master medal of a drill, paid once per drill.
pub const DRILL_MASTER_PAY: u32 = 50;
/// The wallet never holds more than this (keeps sums from overflowing).
pub const WALLET_CAP: u64 = 999_999_999;

/// What a place is worth. Alone on the track (a Time Trial) there is nobody to beat: the
/// lowest rate.
pub fn place_pay(place: u8, rivals: usize) -> u32 {
    if rivals == 0 || place == 0 {
        return PLACE_PAY[3];
    }
    PLACE_PAY[(place as usize - 1).min(3)]
}

/// What a drill run pays: new stars above the saved ones, plus the Master medal the first time.
pub fn drill_pay(prev_stars: u8, stars: u8, prev_master: bool, master: bool) -> u32 {
    DRILL_STAR_PAY * stars.min(3).saturating_sub(prev_stars.min(3)) as u32 + if master && !prev_master { DRILL_MASTER_PAY } else { 0 }
}

/// What the player earned for one finished race.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Earn {
    /// ₿ coins collected.
    pub coins: u32,
    /// The place bonus (0 for lessons and challenges).
    pub place: u32,
    /// The place it was for (0 = none).
    pub place_n: u8,
    /// New training stars, or new challenge medals, paid.
    pub bonus: u32,
    /// What `bonus` is for: 0 none, 1 stars, 2 medals.
    pub bonus_kind: u8,
}

impl Earn {
    pub fn total(&self) -> u32 {
        self.coins.saturating_add(self.place).saturating_add(self.bonus)
    }

    /// A race, battle or online race: coins and a place bonus.
    pub fn race(coins: u32, place: u8, racers: usize) -> Earn {
        Earn { coins, place: place_pay(place, racers.saturating_sub(1)), place_n: place, ..Earn::default() }
    }

    /// The earnings of a finished offline race, from what the profile held before it
    /// (`prev_stars` of the lesson, `prev_medal` of the challenge).
    pub fn of_outcome(out: &RaceOutcome, prev_stars: u8, prev_medal: u8) -> Earn {
        let me = out.results.iter().find(|r| r.is_local);
        let coins = me.map_or(0, |m| m.coins as u32);
        match &out.launch.kind {
            RaceKind::Training(_) => {
                let stars = out.lesson.as_ref().filter(|l| l.done).map_or(0, |l| l.stars.min(3));
                Earn { bonus: STAR_PAY * stars.saturating_sub(prev_stars) as u32, bonus_kind: 1, ..Earn::default() }
            }
            RaceKind::Challenge(_) => {
                let medal = out.challenge.as_ref().map_or(0, |c| c.medal.min(3));
                let bonus: u32 = (prev_medal.min(3)..medal).map(|k| MEDAL_PAY[k as usize]).sum();
                Earn { coins, bonus, bonus_kind: 2, ..Earn::default() }
            }
            _ => match me {
                Some(m) => Earn::race(coins, m.place, out.results.len()),
                None => Earn::default(),
            },
        }
    }
}

// ── what happened, for the counters ─────────────────────────────────────────

/// The kinds of finished race the counters tell apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Race,
    GrandPrix,
    TimeTrial,
    Battle,
    Training,
    Challenge,
    Online,
}

/// One finished race, as far as achievements care.
#[derive(Clone, Copy, Debug)]
pub struct Played {
    pub kind: Kind,
    pub track: Option<TrackId>,
    /// The local player's place (0 = unknown).
    pub place: u8,
    pub racers: usize,
    /// The local player crossed the line.
    pub finished: bool,
    pub coins: u32,
    pub stats: RaceStats,
    /// A Grand Prix just won: which cup (0 = Cup 1, 1 = Cup 2).
    pub cup_won: Option<usize>,
}

impl Played {
    pub fn of_outcome(out: &RaceOutcome) -> Played {
        let me = out.results.iter().find(|r| r.is_local);
        let kind = match &out.launch.kind {
            RaceKind::Quick => Kind::Race,
            RaceKind::GrandPrix { .. } => Kind::GrandPrix,
            RaceKind::TimeTrial => Kind::TimeTrial,
            RaceKind::Training(_) | RaceKind::Drill(_) => Kind::Training,
            RaceKind::Challenge(_) => Kind::Challenge,
            RaceKind::Battle => Kind::Battle,
            RaceKind::Online => Kind::Online,
        };
        Played {
            kind,
            track: Some(out.launch.config.track),
            place: me.map_or(0, |m| m.place),
            racers: out.results.len(),
            finished: me.is_some_and(|m| m.time_s.is_some()) || kind == Kind::Battle,
            coins: if kind == Kind::Training { 0 } else { me.map_or(0, |m| m.coins as u32) },
            stats: out.stats,
            cup_won: None,
        }
    }

    fn is_win(&self) -> bool {
        self.place == 1 && self.racers >= 2
    }
}

/// Add a finished race to the wallet and the counters, then unlock any achievements
/// it completed. Returns the ones unlocked just now. Call after the profile holds the
/// race's lesson stars and challenge medals (achievements read them).
pub fn settle(p: &mut Profile, played: &Played, earn: &Earn) -> Vec<Ach> {
    let g = &mut p.progress;
    let add = earn.total() as u64;
    g.sats = (g.sats + add).min(WALLET_CAP);
    g.lifetime_sats = (g.lifetime_sats + add).min(WALLET_CAP);
    g.coins = g.coins.saturating_add(played.coins);
    g.bashes = g.bashes.saturating_add(played.stats.bashes as u32);
    g.eggs = g.eggs.saturating_add(played.stats.eggs as u32);
    g.purple_turbos = g.purple_turbos.saturating_add(played.stats.purple_turbos as u32);
    g.best_tricks = g.best_tricks.max(played.stats.tricks as u32);
    let racing = matches!(played.kind, Kind::Race | Kind::GrandPrix | Kind::Online);
    if racing && played.is_win() {
        g.wins = g.wins.saturating_add(1);
        if played.kind == Kind::Online {
            g.online_wins = g.online_wins.saturating_add(1);
        }
        if let Some(i) = played.track.and_then(|t| TrackId::RACE_ALL.iter().position(|r| *r == t)) {
            g.track_wins |= 1 << i;
        }
    }
    if played.kind == Kind::Battle && played.is_win() {
        g.battle_wins = g.battle_wins.saturating_add(1);
    }
    // Online events may be thinner than the host's, so a "no wall hit" there proves nothing.
    if matches!(played.kind, Kind::Race | Kind::GrandPrix | Kind::TimeTrial) && played.finished && played.stats.wall_hits == 0 {
        g.clean_races = g.clean_races.saturating_add(1);
    }
    if let Some(c) = played.cup_won {
        g.cups_won |= 1 << c.min(1);
    }
    unlock_new(p)
}

/// Which cup a Grand Prix track list is (0 = Cup 1, 1 = Cup 2).
pub fn cup_of(tracks: &[TrackId]) -> usize {
    usize::from(tracks.first().is_some_and(|t| TrackId::CUP2.contains(t)))
}

// ── achievements ────────────────────────────────────────────────────────────

/// The achievements. The order is the bit order in `Progress::achievements`: only append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ach {
    FirstWin,
    Cup1Sweep,
    Cup2Sweep,
    GpCup1,
    GpCup2,
    AllLessons,
    AnyGold,
    AllGold,
    Coins100,
    Coins1000,
    Bash50,
    Eggs100,
    Purple25,
    Frenzy,
    OnlineWin,
    Tricks10,
    CleanRace,
    DrillSergeant,
    ConeZone,
}

pub const ACHIEVEMENTS: [Ach; 19] = [
    Ach::FirstWin,
    Ach::Cup1Sweep,
    Ach::Cup2Sweep,
    Ach::GpCup1,
    Ach::GpCup2,
    Ach::AllLessons,
    Ach::AnyGold,
    Ach::AllGold,
    Ach::Coins100,
    Ach::Coins1000,
    Ach::Bash50,
    Ach::Eggs100,
    Ach::Purple25,
    Ach::Frenzy,
    Ach::OnlineWin,
    Ach::Tricks10,
    Ach::CleanRace,
    Ach::DrillSergeant,
    Ach::ConeZone,
];

/// What picture an achievement wears (drawn by `progress_ui`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pic {
    Icon(crate::ui::icons::Icon),
    Coin,
}

impl Ach {
    pub fn bit(self) -> u32 {
        1 << ACHIEVEMENTS.iter().position(|a| *a == self).unwrap_or(0)
    }

    pub fn title(self) -> &'static str {
        match self {
            Ach::FirstWin => "First Win",
            Ach::Cup1Sweep => "Cup 1 Sweep",
            Ach::Cup2Sweep => "Cup 2 Sweep",
            Ach::GpCup1 => "Cup 1 Champion",
            Ach::GpCup2 => "Cup 2 Champion",
            Ach::AllLessons => "Top of the Class",
            Ach::AnyGold => "Gold Standard",
            Ach::AllGold => "Golden Flock",
            Ach::Coins100 => "Pocket Change",
            Ach::Coins1000 => "Stacking Sats",
            Ach::Bash50 => "Head Case",
            Ach::Eggs100 => "Omelette Maker",
            Ach::Purple25 => "Purple Reign",
            Ach::Frenzy => "Last Feather",
            Ach::OnlineWin => "Online Champ",
            Ach::Tricks10 => "Show Off",
            Ach::CleanRace => "Clean Lines",
            Ach::DrillSergeant => "Drill Sergeant",
            Ach::ConeZone => "Cone Zone",
        }
    }

    pub fn desc(self) -> &'static str {
        match self {
            Ach::FirstWin => "Win a race against rivals",
            Ach::Cup1Sweep => "Win a race on every Cup 1 track",
            Ach::Cup2Sweep => "Win a race on every Cup 2 track",
            Ach::GpCup1 => "Win the Cup 1 Grand Prix",
            Ach::GpCup2 => "Win the Cup 2 Grand Prix",
            Ach::AllLessons => "Three stars on all five lessons",
            Ach::AnyGold => "Earn a gold medal in a challenge",
            Ach::AllGold => "Gold in all ten challenges",
            Ach::Coins100 => "Collect 100 coins",
            Ach::Coins1000 => "Collect 1,000 coins",
            Ach::Bash50 => "Land 50 head-bashes",
            Ach::Eggs100 => "Collect 100 eggs",
            Ach::Purple25 => "Fire 25 purple mini-turbos",
            Ach::Frenzy => "Win a Feather Frenzy battle",
            Ach::OnlineWin => "Win an online race",
            Ach::Tricks10 => "Land 10 tricks in one race",
            Ach::CleanRace => "Finish a race without hitting a wall",
            Ach::DrillSergeant => "Three stars on all five drills",
            Ach::ConeZone => "Earn the Master medal on any drill",
        }
    }

    pub fn pic(self) -> Pic {
        use crate::ui::icons::Icon;
        match self {
            Ach::FirstWin | Ach::Cup1Sweep | Ach::Cup2Sweep => Pic::Icon(Icon::Flag),
            Ach::GpCup1 | Ach::GpCup2 => Pic::Icon(Icon::Trophy),
            Ach::AllLessons => Pic::Icon(Icon::Wheel),
            Ach::AnyGold | Ach::AllGold => Pic::Icon(Icon::Medal),
            Ach::Coins100 | Ach::Coins1000 => Pic::Coin,
            Ach::Bash50 => Pic::Icon(Icon::Bash),
            Ach::Eggs100 => Pic::Icon(Icon::Egg),
            Ach::Purple25 => Pic::Icon(Icon::Drift),
            Ach::Frenzy => Pic::Icon(Icon::Item),
            Ach::OnlineWin => Pic::Icon(Icon::Globe),
            Ach::Tricks10 => Pic::Icon(Icon::Jump),
            Ach::CleanRace => Pic::Icon(Icon::Steer),
            Ach::DrillSergeant => Pic::Icon(Icon::Wheel),
            Ach::ConeZone => Pic::Icon(Icon::Medal),
        }
    }

    /// (done so far, needed), from the profile.
    pub fn progress(self, p: &Profile) -> (u32, u32) {
        let g = &p.progress;
        let tracks = |cup: &[TrackId; 4]| cup.iter().filter(|t| g.track_wins & (1 << TrackId::RACE_ALL.iter().position(|r| r == *t).unwrap_or(15)) != 0).count() as u32;
        let (cur, goal) = match self {
            Ach::FirstWin => (g.wins.min(1), 1),
            Ach::Cup1Sweep => (tracks(&TrackId::RACE), 4),
            Ach::Cup2Sweep => (tracks(&TrackId::CUP2), 4),
            Ach::GpCup1 => (u32::from(g.cups_won & 1 != 0), 1),
            Ach::GpCup2 => (u32::from(g.cups_won & 2 != 0), 1),
            Ach::AllLessons => (p.lesson_stars.iter().enumerate().filter(|(i, s)| *i != 4 && **s >= 3).count() as u32, 5),
            Ach::AnyGold => (u32::from(p.challenge_medals.contains(&3)), 1),
            Ach::AllGold => (p.challenge_medals.iter().enumerate().filter(|(i, m)| *i != 4 && **m >= 3).count() as u32, 9),
            Ach::Coins100 => (g.coins, 100),
            Ach::Coins1000 => (g.coins, 1000),
            Ach::Bash50 => (g.bashes, 50),
            Ach::Eggs100 => (g.eggs, 100),
            Ach::Purple25 => (g.purple_turbos, 25),
            Ach::Frenzy => (g.battle_wins.min(1), 1),
            Ach::OnlineWin => (g.online_wins.min(1), 1),
            Ach::Tricks10 => (g.best_tricks, 10),
            Ach::CleanRace => (g.clean_races.min(1), 1),
            Ach::DrillSergeant => (p.drill_stars[..4].iter().filter(|s| **s >= 3).count() as u32, 4),
            Ach::ConeZone => (u32::from(p.drill_master.iter().any(|m| *m)), 1),
        };
        (cur.min(goal), goal)
    }

    /// Is the goal met by the counters (whether or not it was unlocked and saved yet)?
    pub fn met(self, p: &Profile) -> bool {
        let (c, g) = self.progress(p);
        c >= g
    }

    pub fn unlocked(self, p: &Profile) -> bool {
        p.progress.achievements & self.bit() != 0
    }
}

/// Unlock every achievement whose goal is met and that is not unlocked yet; the new ones.
pub fn unlock_new(p: &mut Profile) -> Vec<Ach> {
    let new: Vec<Ach> = ACHIEVEMENTS.iter().copied().filter(|a| *a != Ach::Bash50 && !a.unlocked(p) && a.met(p)).collect();
    for a in &new {
        p.progress.achievements |= a.bit();
    }
    new
}

/// How many are unlocked.
pub fn unlocked_count(p: &Profile) -> usize {
    ACHIEVEMENTS.iter().filter(|a| **a != Ach::Bash50 && a.unlocked(p)).count()
}

// ── colour variants ─────────────────────────────────────────────────────────

/// Variant 0 is the original look, 1 and 2 are bought, 3 is Laser Gold.
pub const VARIANTS: usize = 4;
/// The Laser Gold variant id.
pub const LASER_GOLD: u8 = 3;
/// What variants 1 and 2 cost, in ₿.
pub const VARIANT_PRICE: [u64; 3] = [0, 1_000, 2_500];

/// A recolouring (sRGB 0..1, like `race_core::Character`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub name: &'static str,
    pub body: [f32; 3],
    pub neck: [f32; 3],
    pub legs: [f32; 3],
    pub beak: [f32; 3],
    pub accent: [f32; 3],
}

fn hex(c: u32) -> [f32; 3] {
    [((c >> 16) & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, (c & 0xFF) as f32 / 255.0]
}

/// (name, body, neck, legs, beak, accent) for variants 1 and 2 of each ostrich.
const ALTS: [[(&str, u32, u32, u32, u32, u32); 2]; OSTRICHES] = [
    // Dusty
    [("Midnight", 0x1E2447, 0x3E4F8F, 0x2B2F4D, 0xF2C14E, 0x6FE3FF), ("Golden", 0xE8B923, 0xFFE48A, 0xB9852A, 0xFFF0B0, 0xD21A1A)],
    // Blaze
    [("Inferno", 0x8A1212, 0xFF7A1A, 0x4A2020, 0xFFD24A, 0xFFEB3B), ("Arctic", 0xDDEBF5, 0x7FD0F0, 0x6C8CA6, 0xFFB347, 0x1B6FA8)],
    // Satoshi
    [("Silver Sats", 0xBFC8D8, 0xF5F8FF, 0x8892A6, 0xFFD97A, 0xE8EEFF), ("Halving Night", 0x1B1236, 0xF7931A, 0x5A3A70, 0xFFC94D, 0xFFE14D)],
    // Nova
    [("Aurora", 0x23C9A8, 0xBFFFE9, 0x4C9A8A, 0xFFD65E, 0xFF4FB6), ("Supernova", 0xFF6A1A, 0xFFD27A, 0xB8532A, 0xFFF08A, 0xFFFFFF)],
    // Boomer
    [("Obsidian", 0x1A1A22, 0x4A4A58, 0x33333C, 0xE0A030, 0xE03A2E), ("Rust Belt", 0xA8582A, 0xD9946A, 0x6E3A22, 0xF0C060, 0xE8D8B8)],
    // Ziggy
    [("Voltage", 0x7A2CF0, 0xC9A6FF, 0x5A4A9A, 0xFFE04A, 0x39FF14), ("Coral Reef", 0xFF6F61, 0xFFC9B8, 0xB05A4E, 0xFFE08A, 0x00E5D0)],
    // Hodl
    [("Diamond Hands", 0x6FC8F0, 0xD8F4FF, 0x5A8AAA, 0xFFE08A, 0xFFFFFF), ("Bull Run", 0x1E8C4A, 0x7FE0A0, 0x4A6A52, 0xFFD24A, 0xFFD700)],
    // Pip
    [("Strawberry", 0xFFB3C7, 0xFFE6EE, 0xC98A98, 0xFFD06A, 0xE8305A), ("Shadow", 0x2E2A3A, 0x6A6480, 0x4A4458, 0xFFC24A, 0xB07CFF)],
];

/// The recolouring of `variant` on ostrich `character`; `None` for 0 (the original).
pub fn palette(character: u8, variant: u8) -> Option<Palette> {
    let c = (character as usize).min(OSTRICHES - 1);
    match variant {
        1 | 2 => {
            let (name, body, neck, legs, beak, accent) = ALTS[c][variant as usize - 1];
            Some(Palette { name, body: hex(body), neck: hex(neck), legs: hex(legs), beak: hex(beak), accent: hex(accent) })
        }
        LASER_GOLD => Some(Palette { name: "Laser Gold", body: hex(0xF5B800), neck: hex(0xFFE27A), legs: hex(0xB87800), beak: hex(0xFFF3C0), accent: hex(0xFF2D3F) }),
        _ => None,
    }
}

/// The name of a variant ("Original" for 0).
pub fn variant_name(character: u8, variant: u8) -> &'static str {
    palette(character, variant).map_or("Original", |p| p.name)
}

/// Gold in every challenge: Laser Gold is free for all ostriches.
pub fn laser_gold_unlocked(p: &Profile) -> bool {
    p.challenge_medals.iter().enumerate().all(|(i, m)| i == 4 || *m >= 3)
}

/// Can the player wear `variant` on `character` right now?
pub fn owns(p: &Profile, character: u8, variant: u8) -> bool {
    let c = (character as usize).min(OSTRICHES - 1);
    match variant {
        0 => true,
        1 | 2 => p.progress.variants_owned[c] & (1 << variant) != 0,
        LASER_GOLD => laser_gold_unlocked(p),
        _ => false,
    }
}

/// The variant worn on `character` (always one the player owns).
pub fn equipped(p: &Profile, character: u8) -> u8 {
    let v = p.progress.variant_equipped[(character as usize).min(OSTRICHES - 1)];
    if owns(p, character, v) {
        v
    } else {
        0
    }
}

/// Wear `variant` on `character`; false if it is not owned.
pub fn equip(p: &mut Profile, character: u8, variant: u8) -> bool {
    if !owns(p, character, variant) {
        return false;
    }
    p.progress.variant_equipped[(character as usize).min(OSTRICHES - 1)] = variant;
    true
}

/// Why a purchase did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuyError {
    /// Already bought.
    Owned,
    /// Not enough ₿: how many more are needed.
    Short(u64),
    /// Not for sale (Laser Gold is earned, not bought).
    NotForSale,
}

/// Buy variant 1 or 2 of `character`: checks the price and deducts it in one step.
/// The caller saves the profile at once (the wallet and the unlock go to disk together).
pub fn buy(p: &mut Profile, character: u8, variant: u8) -> Result<u64, BuyError> {
    if !(1..=2).contains(&variant) {
        return Err(BuyError::NotForSale);
    }
    let c = (character as usize).min(OSTRICHES - 1);
    if p.progress.variants_owned[c] & (1 << variant) != 0 {
        return Err(BuyError::Owned);
    }
    let price = VARIANT_PRICE[variant as usize];
    if p.progress.sats < price {
        return Err(BuyError::Short(price - p.progress.sats));
    }
    p.progress.sats -= price;
    p.progress.variants_owned[c] |= 1 << variant;
    Ok(price)
}

/// Accessories and clothing are visual only; ownership is shared across characters.
pub const ATTIRE_NAMES: [[&str; 4]; 2] = [
    ["Original", "Flying scarf", "Golden crown", "Racing goggles"],
    ["Team colours", "Orange silks", "Purple velvet", "Mint jacket"],
];
/// Banked Bitcoin prices (the original appearance is always free).
pub const ATTIRE_PRICES: [u64; 4] = [0, 80, 140, 110];

/// Whether this accessory or outfit is available to wear.
pub fn owns_attire(p: &Profile, slot: usize, id: u8) -> bool {
    slot < 2 && id <= 3 && (id == 0 || p.progress.attire_owned[slot] & (1 << id) != 0)
}

/// Validated colour, ostrich accessory and jockey clothing for a character.
pub fn attire(p: &Profile, character: u8) -> [u8; 3] {
    let c = character.min(7) as usize;
    let e = p.progress.attire_equipped[c];
    [equipped(p, character), if owns_attire(p, 0, e[0]) { e[0] } else { 0 },
        if owns_attire(p, 1, e[1]) { e[1] } else { 0 }]
}

/// Buy if necessary and equip an outfit atomically. No racing stats are touched.
pub fn wear_attire(p: &mut Profile, character: u8, slot: usize, id: u8) -> Result<u64, BuyError> {
    if slot >= 2 || id > 3 { return Err(BuyError::NotForSale); }
    let price = if owns_attire(p, slot, id) { 0 } else { ATTIRE_PRICES[id as usize] };
    if p.progress.sats < price { return Err(BuyError::Short(price - p.progress.sats)); }
    p.progress.sats -= price;
    p.progress.attire_owned[slot] |= 1 << id;
    p.progress.attire_equipped[character.min(7) as usize][slot] = id;
    Ok(price)
}

/// "1,840" style number.
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ── hand-over between the race and the screen after it ──────────────────────

thread_local! {
    static STASH: std::cell::RefCell<Option<RaceStats>> = const { std::cell::RefCell::new(None) };
}

/// Leave the race's counters for a screen that has no `RaceOutcome` (the training list,
/// the online results). The next [`take_stats`] gets them once.
pub fn stash_stats(s: RaceStats) {
    STASH.with(|x| *x.borrow_mut() = Some(s));
}

pub fn take_stats() -> RaceStats {
    STASH.with(|x| x.borrow_mut().take()).unwrap_or_default()
}

#[cfg(test)]
mod attire_tests {
    use super::*;

    #[test]
    fn wardrobe_spends_only_banked_bitcoin_and_failed_purchases_change_nothing() {
        let mut p = Profile::default();
        p.progress.sats = 125;
        p.progress.lifetime_sats = 500;
        let character = p.character;
        assert_eq!(wear_attire(&mut p, character, 0, 1), Ok(80));
        assert_eq!(p.progress.sats, 45);
        assert_eq!(p.progress.lifetime_sats, 500);
        assert_eq!(p.character, character);
        assert_eq!(attire(&p, character)[1], 1);
        let before = p.clone();
        assert_eq!(wear_attire(&mut p, character, 1, 2), Err(BuyError::Short(95)));
        assert_eq!(p, before);
        assert_eq!(wear_attire(&mut p, character, 0, 1), Ok(0));
        assert_eq!(p.progress.sats, 45, "wearing an owned accessory is free");
        assert_eq!(wear_attire(&mut p, character, 1, 200), Err(BuyError::NotForSale));
    }

    #[test]
    fn old_profiles_keep_their_savings_and_default_to_original_attire() {
        let p: Profile = serde_json::from_str(r#"{"progress":{"sats":125}}"#).unwrap();
        assert_eq!(p.progress.sats, 125);
        for c in 0..8 { assert_eq!(attire(&p, c), [0; 3]); }
        let mut p = p;
        p.progress.attire_equipped[0] = [255, 3];
        p.progress.sanitize();
        assert_eq!(attire(&p, 0), [0; 3], "unknown and unowned clothes cannot equip");
    }
}

#[cfg(test)]
mod tests {
    use race_core::ChallengeId;
    use super::*;
    use crate::flow::{LessonResult, RaceLaunch, RacerResult};
    use race_core::{BotLevel, LessonId, RaceConfig};

    fn racer(place: u8, coins: u32, local: bool, time: Option<f32>) -> RacerResult {
        RacerResult { name: "x".into(), character: 0, place, time_s: time, best_lap_s: None, coins, is_local: local, is_bot: !local, battle: None }
    }

    fn outcome(kind: RaceKind, place: u8, coins: u32, n: usize) -> RaceOutcome {
        let cfg = RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 1);
        let results = (1..=n as u8).map(|pl| racer(pl, if pl == place { coins } else { 0 }, pl == place, Some(60.0))).collect();
        RaceOutcome { launch: RaceLaunch { config: cfg, kind, local_seat: 0 }, results, lesson: None, challenge: None, quit: false, ghost: None, battle: None, stats: RaceStats::default(), gp_recorded: None }
    }

    #[test]
    fn drill_pay_only_for_new_stars_and_the_first_master_and_the_two_achievements() {
        assert_eq!(drill_pay(0, 2, false, false), 30);
        assert_eq!(drill_pay(2, 2, false, false), 0, "nothing new pays nothing");
        assert_eq!(drill_pay(2, 3, false, true), DRILL_STAR_PAY + DRILL_MASTER_PAY);
        assert_eq!(drill_pay(3, 3, true, true), 0, "the medal pays once");
        let mut p = Profile::default();
        p.drill_stars = [3, 3, 3, 2, 0];
        assert!(unlock_new(&mut p).is_empty());
        assert_eq!(Ach::DrillSergeant.progress(&p), (3, 4));
        p.drill_stars[3] = 3;
        p.drill_master[1] = true;
        let new = unlock_new(&mut p);
        assert!(new.contains(&Ach::DrillSergeant) && new.contains(&Ach::ConeZone), "{new:?}");
        assert!(unlock_new(&mut p).is_empty(), "each unlocks once");
    }

    #[test]
    fn place_pay_table() {
        assert_eq!([1, 2, 3, 4, 8].map(|p| place_pay(p, 7)), [50, 30, 20, 10, 10]);
        assert_eq!(place_pay(1, 0), 10, "alone on the track pays the lowest rate");
    }

    #[test]
    fn races_pay_coins_plus_place() {
        let o = outcome(RaceKind::Quick, 1, 7, 4);
        let e = Earn::of_outcome(&o, 0, 0);
        assert_eq!((e.coins, e.place, e.total()), (7, 50, 57));
        let e = Earn::of_outcome(&outcome(RaceKind::Quick, 6, 3, 8), 0, 0);
        assert_eq!(e.total(), 13);
        let e = Earn::of_outcome(&outcome(RaceKind::TimeTrial, 1, 5, 1), 0, 0);
        assert_eq!(e.total(), 15);
        let mut b = outcome(RaceKind::Battle, 2, 0, 6);
        b.results[1].coins = 0;
        assert_eq!(Earn::of_outcome(&b, 0, 0).total(), 30);
    }

    #[test]
    fn lesson_stars_pay_once() {
        let mut o = outcome(RaceKind::Training(LessonId::Jump), 1, 0, 1);
        o.lesson = Some(LessonResult { lesson: LessonId::Jump, done: true, stars: 3, time_s: 20.0 });
        assert_eq!(Earn::of_outcome(&o, 0, 0).total(), 60);
        assert_eq!(Earn::of_outcome(&o, 2, 0).total(), 20, "only the new star");
        assert_eq!(Earn::of_outcome(&o, 3, 0).total(), 0, "nothing the second time");
        o.lesson = Some(LessonResult { lesson: LessonId::Jump, done: false, stars: 3, time_s: 20.0 });
        assert_eq!(Earn::of_outcome(&o, 0, 0).total(), 0, "a failed lesson pays nothing");
    }

    #[test]
    fn challenge_medals_pay_each_tier_once() {
        let id = ChallengeId::CoinRush;
        let mut o = outcome(RaceKind::Challenge(id), 1, 4, 1);
        let res = |m| Some(crate::flow::ChallengeResult { id, score: 1.0, medal: m, done: true, progress: 0 });
        o.challenge = res(3);
        assert_eq!(Earn::of_outcome(&o, 0, 0).bonus, 210, "bronze + silver + gold on a first gold");
        assert_eq!(Earn::of_outcome(&o, 0, 1).bonus, 180);
        assert_eq!(Earn::of_outcome(&o, 0, 2).bonus, 120);
        assert_eq!(Earn::of_outcome(&o, 0, 3).bonus, 0);
        o.challenge = res(1);
        assert_eq!(Earn::of_outcome(&o, 0, 2).bonus, 0, "a worse medal pays nothing");
        assert_eq!(Earn::of_outcome(&o, 0, 0).total(), 34, "coins + bronze");
    }

    #[test]
    fn settle_fills_the_wallet_and_counters() {
        let mut p = Profile::default();
        let o = outcome(RaceKind::Quick, 1, 9, 4);
        let e = Earn::of_outcome(&o, 0, 0);
        let mut played = Played::of_outcome(&o);
        played.stats = RaceStats { bashes: 3, eggs: 2, purple_turbos: 1, tricks: 4, wall_hits: 0 };
        let new = settle(&mut p, &played, &e);
        assert_eq!((p.progress.sats, p.progress.lifetime_sats), (59, 59));
        assert_eq!((p.progress.coins, p.progress.bashes, p.progress.eggs, p.progress.best_tricks), (9, 3, 2, 4));
        assert!(new.contains(&Ach::FirstWin) && new.contains(&Ach::CleanRace));
        assert_eq!(p.progress.track_wins, 1);
        // the same achievement is not announced twice
        let again = settle(&mut p, &played, &e);
        assert!(again.is_empty());
        assert_eq!(p.progress.sats, 118);
        assert_eq!(p.progress.best_tricks, 4, "best, not a sum");
    }

    #[test]
    fn a_wall_hit_spoils_a_clean_race_and_online_never_counts_it() {
        let mut p = Profile::default();
        let o = outcome(RaceKind::Quick, 3, 0, 4);
        let mut played = Played::of_outcome(&o);
        played.stats.wall_hits = 1;
        settle(&mut p, &played, &Earn::default());
        assert_eq!(p.progress.clean_races, 0);
        played.stats.wall_hits = 0;
        played.kind = Kind::Online;
        settle(&mut p, &played, &Earn::default());
        assert_eq!(p.progress.clean_races, 0);
        played.kind = Kind::Race;
        played.finished = false;
        settle(&mut p, &played, &Earn::default());
        assert_eq!(p.progress.clean_races, 0, "quitting is not finishing");
    }

    #[test]
    fn track_sweeps_and_cups() {
        let mut p = Profile::default();
        for (i, t) in TrackId::RACE.iter().enumerate() {
            let mut played = Played::of_outcome(&outcome(RaceKind::Quick, 1, 0, 3));
            played.track = Some(*t);
            let new = settle(&mut p, &played, &Earn::default());
            assert_eq!(new.contains(&Ach::Cup1Sweep), i == 3);
        }
        assert_eq!(Ach::Cup1Sweep.progress(&p), (4, 4));
        assert_eq!(Ach::Cup2Sweep.progress(&p), (0, 4));
        // a Time Trial win does not count, a Grand Prix round does
        let mut tt = Played::of_outcome(&outcome(RaceKind::TimeTrial, 1, 0, 1));
        tt.track = Some(TrackId::Baobab);
        settle(&mut p, &tt, &Earn::default());
        assert_eq!(Ach::Cup2Sweep.progress(&p).0, 0);
        let mut gp = Played::of_outcome(&outcome(RaceKind::GrandPrix { tracks: TrackId::CUP2.to_vec(), round: 3, points: vec![] }, 1, 0, 8));
        gp.track = Some(TrackId::Halving);
        gp.cup_won = Some(cup_of(&TrackId::CUP2));
        let new = settle(&mut p, &gp, &Earn::default());
        assert!(new.contains(&Ach::GpCup2) && !new.contains(&Ach::GpCup1));
        assert_eq!(Ach::Cup2Sweep.progress(&p).0, 1);
    }

    #[test]
    fn profile_driven_achievements() {
        let mut p = Profile::default();
        assert!(unlock_new(&mut p).is_empty());
        p.lesson_stars = [3; 6];
        p.challenge_medals[0] = 3;
        let new = unlock_new(&mut p);
        assert!(new.contains(&Ach::AllLessons) && new.contains(&Ach::AnyGold) && !new.contains(&Ach::AllGold));
        assert_eq!(Ach::AllGold.progress(&p), (1, 9));
        p.challenge_medals = [3; 10];
        assert_eq!(unlock_new(&mut p), vec![Ach::AllGold]);
        p.progress.coins = 1000;
        let new = unlock_new(&mut p);
        assert!(new.contains(&Ach::Coins100) && new.contains(&Ach::Coins1000));
        assert_eq!(Ach::Bash50.progress(&p), (0, 50));
        p.progress.bashes = 80;
        assert_eq!(Ach::Bash50.progress(&p), (50, 50), "progress is capped at the goal");
        // bits are distinct
        let all: u32 = ACHIEVEMENTS.iter().map(|a| a.bit()).fold(0, |a, b| a | b);
        assert_eq!(all.count_ones() as usize, ACHIEVEMENTS.len());
    }

    #[test]
    fn online_and_battle_wins() {
        let mut p = Profile::default();
        let mut o = Played::of_outcome(&outcome(RaceKind::Online, 1, 0, 4));
        o.kind = Kind::Online;
        assert!(settle(&mut p, &o, &Earn::default()).contains(&Ach::OnlineWin));
        let b = Played::of_outcome(&outcome(RaceKind::Battle, 1, 0, 5));
        assert!(settle(&mut p, &b, &Earn::default()).contains(&Ach::Frenzy));
        assert_eq!(p.progress.wins, 1, "a battle win is not a race win");
    }

    #[test]
    fn buying_checks_funds_and_never_double_charges() {
        let mut p = Profile::default();
        p.progress.sats = 900;
        assert_eq!(buy(&mut p, 2, 1), Err(BuyError::Short(100)));
        assert_eq!(p.progress.sats, 900, "a refused purchase costs nothing");
        assert!(!owns(&p, 2, 1));
        p.progress.sats = 1000;
        assert_eq!(buy(&mut p, 2, 1), Ok(1000));
        assert_eq!(p.progress.sats, 0);
        assert!(owns(&p, 2, 1) && !owns(&p, 2, 2) && !owns(&p, 3, 1));
        p.progress.sats = 5000;
        assert_eq!(buy(&mut p, 2, 1), Err(BuyError::Owned));
        assert_eq!(p.progress.sats, 5000, "buying twice is refused");
        assert_eq!(buy(&mut p, 2, 2), Ok(2500));
        assert_eq!(p.progress.sats, 2500);
        assert_eq!(buy(&mut p, 2, LASER_GOLD), Err(BuyError::NotForSale));
        assert_eq!(buy(&mut p, 2, 0), Err(BuyError::NotForSale));
    }

    #[test]
    fn equip_needs_ownership_and_laser_gold_needs_all_golds() {
        let mut p = Profile::default();
        assert!(!equip(&mut p, 1, 1));
        assert_eq!(equipped(&p, 1), 0);
        p.progress.sats = 1000;
        buy(&mut p, 1, 1).unwrap();
        assert!(equip(&mut p, 1, 1));
        assert_eq!(equipped(&p, 1), 1);
        assert_eq!(equipped(&p, 0), 0, "per ostrich");
        assert!(!equip(&mut p, 1, LASER_GOLD));
        p.challenge_medals = [3; 10];
        assert!(owns(&p, 6, LASER_GOLD) && equip(&mut p, 6, LASER_GOLD));
        p.challenge_medals[0] = 2;
        assert_eq!(equipped(&p, 6), 0, "worn gold goes if the golds are not all there");
    }

    #[test]
    fn every_ostrich_has_two_distinct_named_alternates() {
        for c in 0..OSTRICHES as u8 {
            let (a, b) = (palette(c, 1).unwrap(), palette(c, 2).unwrap());
            assert_ne!(a.name, b.name);
            assert_ne!(a.body, b.body);
            let orig = race_core::Character::get(c);
            assert!(a.body != orig.body && b.body != orig.body, "ostrich {c}");
        }
        assert!(palette(0, 0).is_none());
        assert_eq!(palette(0, LASER_GOLD), palette(5, LASER_GOLD));
    }

    #[test]
    fn grouping() {
        assert_eq!([0, 999, 1000, 12_345, 2_500_000].map(grouped), ["0", "999", "1,000", "12,345", "2,500,000"]);
    }

    #[test]
    fn stats_count_only_the_local_seat() {
        use race_core::Event;
        let mut s = RaceStats::default();
        s.absorb(
            &[
                Event::Bash { attacker: 0, victim: 1, stole: None },
                Event::Bash { attacker: 2, victim: 0, stole: None },
                Event::Boost { racer: 0, tier: 3 },
                Event::Boost { racer: 0, tier: 2 },
                Event::TrickBoost { racer: 0 },
                Event::TrickBoost { racer: 1 },
                Event::WallHit { racer: 0, strength: 0.9 },
                Event::TargetDestroyed { racer: 0, target: 3, item: None, uses: 0 },
            ],
            0,
        );
        assert_eq!(s, RaceStats { bashes: 1, eggs: 1, purple_turbos: 1, tricks: 1, wall_hits: 1 });
    }
}
