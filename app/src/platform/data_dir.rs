//! Where the app keeps its files (profile and settings).
//!
//! Desktop: `--data-dir`, else `$XDG_DATA_HOME/stampede`, else
//! `$HOME/.local/share/stampede`. Android: the activity's internal data path
//! (set by `android_main`). Everything is one JSON file, `profile.json`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The default desktop data directory.
pub fn default_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x).join("stampede");
    }
    if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h).join(".local/share/stampede");
    }
    PathBuf::from(".stampede")
}

/// How the player steers on a touch screen (SPEC 12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerMode {
    #[default]
    Pad,
    Buttons,
    /// Android motion sensor steering, with touch / keyboard fallback.
    Tilt,
}

/// The race camera (SPEC 11); the HUD button cycles it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraView {
    #[default]
    Chase,
    FirstPerson,
    Far,
}

/// Graphics quality (SPEC 13, Settings).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Low,
    #[default]
    Balanced,
    High,
}

/// Challenges in the profile (`ChallengeId::ALL.len()`, asserted in the tests).
pub const CHALLENGES: usize = 10;

/// A fixed array read from a list of any length: a shorter one (a profile from
/// before the four newer challenges had 6) is padded with defaults, a longer
/// one cut. Challenge slots follow `ChallengeId::ALL`, new ids are appended.
fn any_len_array<'de, D, T, const N: usize>(d: D) -> Result<[T; N], D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default + Copy,
{
    let v = Vec::<T>::deserialize(d)?;
    let mut out = [T::default(); N];
    for (o, x) in out.iter_mut().zip(v) {
        *o = x;
    }
    Ok(out)
}

/// Ostriches in the game (`race_core::CHARACTERS.len()`, asserted in the tests).
pub const OSTRICHES: usize = 8;

/// Progression: the ₿ wallet, cosmetic unlocks and the counters achievements read.
/// Part of `profile.json` under `progress`; every field defaults, so profiles from
/// before the wallet load with an empty one. The wallet is a `u64`, so it cannot go
/// negative; `ui::progress` is the only code that changes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    /// Spendable ₿ ("sats").
    pub sats: u64,
    /// Everything ever earned (never reduced by spending).
    pub lifetime_sats: u64,
    /// Per ostrich: bit 1 = colour variant 1 bought, bit 2 = variant 2 bought (variant 0 is free).
    #[serde(deserialize_with = "any_len_array")]
    pub variants_owned: [u8; OSTRICHES],
    /// Per ostrich: the colour variant worn in races (0 = the original).
    #[serde(deserialize_with = "any_len_array")]
    pub variant_equipped: [u8; OSTRICHES],
    /// Bought ostrich accessories and jockey outfits, bits 1..=3 (0 is free).
    pub attire_owned: [u8; 2],
    /// Equipped attire for each character: ostrich accessory, jockey outfit.
    #[serde(deserialize_with = "any_len_array")]
    pub attire_equipped: [[u8; 2]; OSTRICHES],
    /// Lifetime coins collected in races.
    pub coins: u32,
    /// Lifetime head-bashes landed.
    pub bashes: u32,
    /// Lifetime Power Eggs destroyed.
    pub eggs: u32,
    /// Lifetime purple (tier 3) mini-turbos.
    pub purple_turbos: u32,
    /// Most tricks landed in one race.
    pub best_tricks: u32,
    /// Races won against rivals (Quick Race, Grand Prix rounds, online).
    pub wins: u32,
    pub online_wins: u32,
    /// Battles (Feather Frenzy) won.
    pub battle_wins: u32,
    /// Races finished without a wall hit.
    pub clean_races: u32,
    /// Bit per race track (`TrackId::RACE_ALL` order, Cup 1 then Cup 2) won.
    pub track_wins: u16,
    /// Bit 0 = Cup 1 Grand Prix won, bit 1 = Cup 2.
    pub cups_won: u8,
    /// Achievements unlocked, one bit each (`ui::progress::Ach` order).
    pub achievements: u32,
}

impl Progress {
    /// Clamp loaded values into range (a hand-edited file must not break the wallet).
    pub fn sanitize(&mut self) {
        /// Far beyond anything earnable; keeps sums from overflowing.
        const CAP: u64 = 999_999_999;
        self.sats = self.sats.min(CAP);
        self.lifetime_sats = self.lifetime_sats.min(CAP).max(self.sats);
        for v in self.variants_owned.iter_mut() {
            *v &= 0b110;
        }
        for (e, o) in self.variant_equipped.iter_mut().zip(self.variants_owned) {
            // 0 is always worn-able, 1 and 2 only if bought; 3 (Laser Gold) is checked against the medals at use.
            *e = match *e {
                0 | 3 => *e,
                1 | 2 if o & (1 << *e) != 0 => *e,
                _ => 0,
            };
        }
        for v in &mut self.attire_owned { *v &= 0b1110; }
        for e in &mut self.attire_equipped {
            for (slot, v) in e.iter_mut().enumerate() {
                if *v > 3 || (*v > 0 && self.attire_owned[slot] & (1 << *v) == 0) { *v = 0; }
            }
        }
        self.track_wins &= 0xFF;
        self.cups_won &= 0b11;
        self.achievements &= (1 << 20) - 1;
    }
}

/// The player's name and settings. Stored as `profile.json`; every field has a
/// default, so an older file (or a missing one) still loads. Add new settings
/// here with `#[serde(default)]` semantics (the struct is `#[serde(default)]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Display name, 12 characters max.
    pub name: String,
    pub music_volume: f32,
    pub effects_volume: f32,
    pub haptics: bool,
    /// The ostrich runs by itself (a Go pedal appears when off).
    pub auto_accelerate: bool,
    pub steer_mode: SteerMode,
    /// Tilt gain: 0.5 gentle .. 2.0 sensitive (1.0 = full lock at 25 degrees).
    pub tilt_sensitivity: f32,
    pub quality: Quality,
    /// Mirror the thumb layout.
    pub left_handed: bool,
    pub camera: CameraView,
    /// The player's ostrich (race_core::CHARACTERS id).
    pub character: u8,
    /// Best race time (s) per track, keyed by `TrackId` debug name.
    pub best_times: std::collections::BTreeMap<String, f32>,
    /// Best lap (s) per track, keyed like `best_times`.
    pub best_laps: std::collections::BTreeMap<String, f32>,
    /// Custom-track Time Trial bests, keyed by the saved track's hash.
    pub custom_bests: std::collections::BTreeMap<u64, f32>,
    /// Best laps on custom tracks, keyed by the same hash.
    pub custom_laps: std::collections::BTreeMap<u64, f32>,
    /// Stars (0-3) per training lesson, `LessonId::ALL` order.
    pub lesson_stars: [u8; 6],
    /// Best level reached (0 = not played, else 1-10) per open-ground drill, `DrillId::ALL` order.
    #[serde(deserialize_with = "any_len_array")]
    pub drill_best: [u8; 5],
    /// Stars (0-3) per drill: one at level 3, two at level 6, three at level 9.
    #[serde(deserialize_with = "any_len_array")]
    pub drill_stars: [u8; 5],
    /// The gold Master medal (level 10 cleared) per drill.
    #[serde(deserialize_with = "any_len_array")]
    pub drill_master: [bool; 5],
    /// Grand Prix wins (count of 1st overall).
    pub gp_wins: u32,
    /// Best medal (0 none .. 3 gold) per Challenge, `ChallengeId::ALL` order.
    #[serde(deserialize_with = "any_len_array")]
    pub challenge_medals: [u8; CHALLENGES],
    /// Best score per Challenge on its own scale (0 = not played).
    #[serde(deserialize_with = "any_len_array")]
    pub challenge_best: [f32; CHALLENGES],
    /// Time Trial: race the saved best ghost (SPEC 17.2).
    pub show_ghost: bool,
    /// The splash and first-run hint ("try Training first") have been seen.
    pub seen_intro: bool,
    /// Random seed the default name comes from (0 = not drawn yet; `load` draws it).
    pub name_seed: u32,
    /// The ₿ wallet, unlocks and achievement counters.
    pub progress: Progress,
    /// Nostr relays for room codes and update checks, in priority order (Settings, RELAYS).
    pub relays: Vec<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            name: "Player".into(),
            music_volume: 0.6,
            effects_volume: 0.9,
            haptics: false, // off by default
            auto_accelerate: true,
            steer_mode: SteerMode::Pad,
            tilt_sensitivity: 1.0,
            quality: Quality::Balanced,
            left_handed: false,
            camera: CameraView::Chase,
            character: 0,
            best_times: Default::default(),
            best_laps: Default::default(),
            custom_bests: Default::default(),
            custom_laps: Default::default(),
            lesson_stars: [0; 6],
            drill_best: [0; 5],
            drill_stars: [0; 5],
            drill_master: [false; 5],
            gp_wins: 0,
            challenge_medals: [0; CHALLENGES],
            challenge_best: [0.0; CHALLENGES],
            show_ghost: true,
            seen_intro: false,
            name_seed: 0,
            progress: Progress::default(),
            relays: default_relays(),
        }
    }
}

const FILE: &str = "profile.json";

// ─── Relays (ported from Twang Thang's data_dir.rs; spec 20.1) ───────────────

/// The most relays the player can have (a sanity cap, not a design limit).
pub const MAX_RELAYS: usize = 32;

/// The default relay list.
pub fn default_relays() -> Vec<String> {
    decented_net::DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
}

/// A loaded relay list, cleaned: only `wss://` entries (a hand-edited file may
/// hold anything; plain `ws://` would send room codes and update queries in the
/// clear), no repeats (by normalised address), at most [`MAX_RELAYS`], and the
/// defaults when nothing is left.
pub fn clean_relays(mut list: Vec<String>) -> Vec<String> {
    list.retain(|r| r.starts_with("wss://"));
    let mut seen = Vec::new();
    list.retain(|r| {
        let k = decented_relays::normalize_relay_url(r);
        !seen.contains(&k) && {
            seen.push(k);
            true
        }
    });
    list.truncate(MAX_RELAYS);
    if list.is_empty() {
        list = default_relays();
    }
    list
}

/// What the player typed in "Add a relay", as a relay address: `wss://host[:port][/path]`
/// or a bare host (`wss://` is added). Normalised (lower-case host, no default port, no
/// trailing slash). Plain `ws://` and anything else is refused.
pub fn parse_relay_input(input: &str) -> Result<String, String> {
    const BAD: &str = "That doesn't look like a relay address. Try wss://relay.example.com";
    let t = input.trim();
    if t.is_empty() || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(BAD.into());
    }
    let full = match t.split_once("://") {
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("wss") => t.to_string(),
        Some(_) => return Err("Relays must use wss:// (encrypted).".into()),
        None => format!("wss://{t}"),
    };
    let rest = &full[full.find("://").map_or(0, |i| i + 3)..];
    if rest.contains(['?', '#', '\\']) {
        return Err(BAD.into());
    }
    let (authority, _path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if let Some(p) = port {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || !p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n)) {
            return Err(BAD.into());
        }
    }
    let label_ok = |l: &str| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let host_ok = host.split('.').all(label_ok) && (host.contains('.') || host.eq_ignore_ascii_case("localhost"));
    if !host_ok {
        return Err(BAD.into());
    }
    Ok(decented_relays::normalize_relay_url(&full))
}

/// Move relay `i` one place up (`up`) or down the list: the list is the
/// priority order, the top is tried first. Returns whether anything moved
/// (not at the end it was asked to move past, and `i` is in the list).
pub fn move_relay(list: &mut [String], i: usize, up: bool) -> bool {
    let j = if up { i.checked_sub(1) } else { i.checked_add(1) };
    match j {
        Some(j) if i < list.len() && j < list.len() => {
            list.swap(i, j);
            true
        }
        _ => false,
    }
}

/// Add what the player typed to `list`: parsed, not already there (same relay by
/// normalised address), and under [`MAX_RELAYS`]. Returns the relay as stored.
pub fn add_relay(list: &mut Vec<String>, input: &str) -> Result<String, String> {
    let url = parse_relay_input(input)?;
    if list.iter().any(|r| decented_relays::normalize_relay_url(r) == url) {
        return Err("That relay is already in the list.".into());
    }
    if list.len() >= MAX_RELAYS {
        return Err(format!("{MAX_RELAYS} relays is the most. Remove one first."));
    }
    list.push(url.clone());
    Ok(url)
}

/// The name a profile has until the player picks one.
pub const PLAIN_NAME: &str = "Player";

const NAME_ADJ: [&str; 16] = ["Dusty", "Swift", "Zippy", "Lucky", "Sunny", "Bold", "Wild", "Jolly", "Brave", "Rapid", "Sandy", "Fast", "Zany", "Tiny", "Mega", "Epic"];
const NAME_NOUN: [&str; 14] = ["Dash", "Beak", "Strut", "Egg", "Dune", "Kick", "Wing", "Stomp", "Hop", "Dart", "Zoom", "Neck", "Feet", "Run"];

/// A fun default name like "DustyDash 27" from `seed`: always the same for the
/// same seed, and at most 12 characters (the name limit, also the online one).
pub fn default_name(seed: u32) -> String {
    // A small mix so neighbouring seeds do not give neighbouring words.
    let h = seed.wrapping_mul(0x9E37_79B1) ^ (seed >> 15);
    let adj = NAME_ADJ[(h % NAME_ADJ.len() as u32) as usize];
    let room = 12 - adj.len() - 3; // a space and two digits
    let fits: Vec<&str> = NAME_NOUN.iter().copied().filter(|n| n.len() <= room).collect();
    let noun = fits[((h >> 8) % fits.len() as u32) as usize];
    format!("{adj}{noun} {:02}", (h >> 16) % 100)
}

/// A random non-zero seed (the clock and the process id; this is not security).
fn random_seed() -> u32 {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x1234_5678);
    let m = n ^ (n >> 32) ^ ((std::process::id() as u64) << 11);
    (m as u32).max(1)
}

impl Profile {
    /// Load `profile.json` from `dir`; defaults if it is missing or unreadable
    /// (a corrupt file is kept as `profile.json.bad`).
    pub fn load(dir: &Path) -> Profile {
        let path = dir.join(FILE);
        let Ok(text) = std::fs::read_to_string(&path) else { return Profile::default().with_default_name() };
        match serde_json::from_str::<Profile>(&text) {
            Ok(mut p) => {
                p.sanitize();
                p.with_default_name()
            }
            Err(e) => {
                log::warn!("profile: {} is unreadable ({e}); using defaults", path.display());
                let _ = std::fs::rename(&path, dir.join("profile.json.bad"));
                Profile::default().with_default_name()
            }
        }
    }

    /// First run, or a profile from before default names that still says "Player":
    /// draw the name seed (once) and take the generated name. A name the player
    /// chose later is never touched, even if it is "Player".
    fn with_default_name(mut self) -> Profile {
        if self.name_seed == 0 {
            self.name_seed = random_seed();
            if self.name == PLAIN_NAME {
                self.name = default_name(self.name_seed);
            }
        }
        self
    }

    /// What an emptied name field becomes.
    pub fn fallback_name(&self) -> String {
        if self.name_seed == 0 {
            PLAIN_NAME.into()
        } else {
            default_name(self.name_seed)
        }
    }

    /// Write `profile.json` (to a temp file first, then renamed over it).
    pub fn save(&self, dir: &Path) {
        if let Err(e) = self.try_save(dir) {
            log::warn!("profile: could not save: {e}");
        }
    }

    fn try_save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = dir.join("profile.json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, dir.join(FILE)).map_err(|e| e.to_string())
    }

    /// Clamp loaded values into range.
    pub fn sanitize(&mut self) {
        self.name = self.name.chars().filter(|c| !c.is_control()).take(12).collect();
        if self.name.trim().is_empty() {
            self.name = self.fallback_name();
        }
        self.tilt_sensitivity = if self.tilt_sensitivity.is_finite() { self.tilt_sensitivity.clamp(0.5, 2.0) } else { 1.0 };
        for s in self.lesson_stars.iter_mut() {
            *s = (*s).min(3);
        }
        for s in self.drill_stars.iter_mut() {
            *s = (*s).min(3);
        }
        for b in self.drill_best.iter_mut() {
            *b = (*b).min(10);
        }
        self.best_times.retain(|_, t| t.is_finite() && *t > 0.0);
        self.best_laps.retain(|_, t| t.is_finite() && *t > 0.0);
        self.custom_bests.retain(|_, t| t.is_finite() && *t > 0.0);
        self.custom_laps.retain(|_, t| t.is_finite() && *t > 0.0);
        self.music_volume = self.music_volume.clamp(0.0, 1.0);
        self.effects_volume = self.effects_volume.clamp(0.0, 1.0);
        for m in self.challenge_medals.iter_mut() {
            *m = (*m).min(3);
        }
        for b in self.challenge_best.iter_mut() {
            if !b.is_finite() || *b < 0.0 {
                *b = 0.0;
            }
        }
        self.progress.sanitize();
        self.relays = clean_relays(std::mem::take(&mut self.relays));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_partial_files() {
        let dir = std::env::temp_dir().join(format!("stampede-profile-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // First run: a generated name and a drawn seed, nothing else different.
        let first = Profile::load(&dir);
        assert_ne!(first.name_seed, 0);
        assert_eq!(first.name, default_name(first.name_seed));
        assert_eq!(Profile { name: "Player".into(), name_seed: 0, ..first }, Profile::default());
        let p = Profile { name: "Ada".into(), haptics: false, left_handed: true, name_seed: 7,
            custom_bests: [(0x1234, 180.5)].into(), custom_laps: [(0x1234, 59.8)].into(), ..Profile::default() };
        p.save(&dir);
        assert_eq!(Profile::load(&dir), p);
        // An older file with only some fields still loads.
        std::fs::write(dir.join(FILE), r#"{"name":"Bo","music_volume":7.0}"#).unwrap();
        let q = Profile::load(&dir);
        assert_eq!(q.name, "Bo");
        assert_eq!(q.music_volume, 1.0);
        assert!(q.auto_accelerate);
        // Garbage falls back to defaults and is kept aside.
        std::fs::write(dir.join(FILE), "not json").unwrap();
        assert_eq!(Profile::load(&dir).seen_intro, false);
        assert!(dir.join("profile.json.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_six_challenge_profile_loads_and_grows_to_ten() {
        assert_eq!(CHALLENGES, race_core::ChallengeId::ALL.len());
        let dir = std::env::temp_dir().join(format!("stampede-profile-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(FILE),
            r#"{"name":"Ada","challenge_medals":[3,2,1,0,0,2],"challenge_best":[138.0,55.4,3.0,0.0,0.0,141.5],"lesson_stars":[1,2,3,0,0,0]}"#,
        )
        .unwrap();
        let mut p = Profile::load(&dir);
        assert_eq!(p.name, "Ada");
        assert_eq!(p.challenge_medals, [3, 2, 1, 0, 0, 2, 0, 0, 0, 0]);
        assert_eq!(p.challenge_best[5], 141.5);
        assert_eq!(&p.challenge_best[6..], &[0.0; 4]);
        assert!(p.show_ghost, "ghosts are on by default");
        // The new slots work through the shared helper, and a round trip keeps ten.
        assert!(crate::ui::screens::common::record_challenge(&mut p, race_core::ChallengeId::LastFeather, 2.0, 2, true));
        p.save(&dir);
        let q = Profile::load(&dir);
        assert_eq!((q.challenge_medals[9], q.challenge_best[9]), (2, 2.0));
        // A longer list (a later build) is cut, a bad one falls back to defaults.
        std::fs::write(dir.join(FILE), r#"{"challenge_medals":[1,1,1,1,1,1,1,1,1,1,3,3]}"#).unwrap();
        assert_eq!(Profile::load(&dir).challenge_medals, [1; 10]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_profile_from_before_drills_loads_with_empty_drill_progress_and_drills_round_trip() {
        assert_eq!(race_core::DrillId::ALL.len(), 5);
        let dir = std::env::temp_dir().join(format!("stampede-profile-drills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), r#"{"name":"Ada","lesson_stars":[1,2,3,0,0,0],"seen_intro":true}"#).unwrap();
        let mut p = Profile::load(&dir);
        assert_eq!(p.name, "Ada");
        assert_eq!(p.lesson_stars, [1, 2, 3, 0, 0, 0]);
        assert_eq!((p.drill_best, p.drill_stars, p.drill_master), ([0; 5], [0; 5], [false; 5]));
        p.drill_best = [7, 0, 10, 3, 1];
        p.drill_stars = [2, 0, 3, 1, 0];
        p.drill_master[2] = true;
        p.save(&dir);
        let q = Profile::load(&dir);
        assert_eq!((q.drill_best, q.drill_stars, q.drill_master), ([7, 0, 10, 3, 1], [2, 0, 3, 1, 0], [false, false, true, false, false]));
        // A shorter list pads with defaults; out-of-range values are clamped.
        std::fs::write(dir.join(FILE), r#"{"drill_best":[4,99],"drill_stars":[9],"drill_master":[true]}"#).unwrap();
        let r = Profile::load(&dir);
        assert_eq!((r.drill_best, r.drill_stars, r.drill_master), ([4, 10, 0, 0, 0], [3, 0, 0, 0, 0], [true, false, false, false, false]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_names_are_distinct_short_and_stable() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in 1..2000u32 {
            let n = default_name(seed);
            assert!(n.chars().count() <= 12, "{n}");
            assert_eq!(n, default_name(seed));
            seen.insert(n);
        }
        assert!(seen.len() > 500, "only {} distinct names", seen.len());
    }

    #[test]
    fn an_old_player_profile_gets_a_name_once() {
        let dir = std::env::temp_dir().join(format!("stampede-profile-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), r#"{"name":"Player","gp_wins":2}"#).unwrap();
        let p = Profile::load(&dir);
        assert_ne!(p.name, "Player");
        assert_eq!(p.gp_wins, 2);
        p.save(&dir);
        assert_eq!(Profile::load(&dir).name, p.name, "the generated name sticks");
        // The player picks "Player" on purpose: it stays.
        let mut q = p.clone();
        q.name = "Player".into();
        q.save(&dir);
        assert_eq!(Profile::load(&dir).name, "Player");
        // A named profile from before is left alone.
        std::fs::write(dir.join(FILE), r#"{"name":"Ada"}"#).unwrap();
        assert_eq!(Profile::load(&dir).name, "Ada");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_edited_values_are_pulled_into_range() {
        let mut p = Profile { steer_mode: SteerMode::Tilt, lesson_stars: [9, 2, 0, 4, 1, 3], ..Profile::default() };
        p.best_times.insert("Dune".into(), 0.0);
        p.best_times.insert("Savanna".into(), 61.5);
        p.best_laps.insert("Dune".into(), f32::NAN);
        p.sanitize();
        assert_eq!(p.steer_mode, SteerMode::Tilt);
        assert_eq!(p.lesson_stars, [3, 2, 0, 3, 1, 3]);
        assert_eq!(p.best_times.len(), 1);
        assert!(p.best_laps.is_empty());
    }

    #[test]
    fn tilt_settings_survive_reload_and_old_profiles_get_normal_sensitivity() {
        let old: Profile = serde_json::from_str(r#"{"name":"Rider","steer_mode":"tilt"}"#).unwrap();
        assert_eq!(old.tilt_sensitivity, 1.0);
        let mut p = Profile { steer_mode: SteerMode::Tilt, tilt_sensitivity: 9.0, ..Profile::default() };
        p.sanitize(); assert_eq!(p.tilt_sensitivity, 2.0);
        let q: Profile = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(q.steer_mode, SteerMode::Tilt); assert_eq!(q.tilt_sensitivity, 2.0);
        p.tilt_sensitivity = f32::NAN; p.sanitize(); assert_eq!(p.tilt_sensitivity, 1.0);
    }

    #[test]
    fn a_profile_from_before_the_wallet_loads_with_an_empty_one() {
        assert_eq!(OSTRICHES, race_core::CHARACTERS.len());
        let dir = std::env::temp_dir().join(format!("stampede-profile-wallet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // An old file: no `progress` at all.
        std::fs::write(dir.join(FILE), r#"{"name":"Ada","character":3,"gp_wins":1,"challenge_medals":[3,3,3,3,3,3,3,3,3,3]}"#).unwrap();
        let mut p = Profile::load(&dir);
        assert_eq!((p.name.as_str(), p.character, p.gp_wins), ("Ada", 3, 1));
        assert_eq!(p.progress, Progress::default());
        assert_eq!(p.progress.sats, 0);
        // Earn, buy, save, load: it all comes back.
        p.progress.sats = 3_500;
        assert!(crate::ui::progress::buy(&mut p, 3, 2).is_ok());
        assert!(crate::ui::progress::equip(&mut p, 3, 2));
        p.save(&dir);
        let q = Profile::load(&dir);
        assert_eq!(q.progress.sats, 1_000);
        assert_eq!(crate::ui::progress::equipped(&q, 3), 2);
        // A partly written `progress`, short arrays and absurd values are repaired.
        std::fs::write(dir.join(FILE), r#"{"progress":{"sats":5,"variants_owned":[6],"variant_equipped":[2,1,3,0,0,0,0,0,0,0,9],"track_wins":65535,"achievements":4294967295,"lifetime_sats":1}}"#).unwrap();
        let r = Profile::load(&dir);
        assert_eq!(r.progress.sats, 5);
        assert_eq!(r.progress.variants_owned, [6, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r.progress.variant_equipped, [2, 0, 3, 0, 0, 0, 0, 0], "an unbought variant is dropped, Laser Gold is checked at use");
        assert_eq!(r.progress.track_wins, 0xFF);
        assert_eq!(r.progress.lifetime_sats, 5, "lifetime is at least what is in the wallet");
        // A negative wallet in a hand-edited file is no profile at all, never a debt.
        std::fs::write(dir.join(FILE), r#"{"progress":{"sats":-40}}"#).unwrap();
        assert_eq!(Profile::load(&dir).progress.sats, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relay_input_is_normalised_deduped_rejected_and_capped() {
        // Full addresses and bare hosts, tidied.
        assert_eq!(parse_relay_input("wss://Relay.Example.com/").as_deref(), Ok("wss://relay.example.com"));
        assert_eq!(parse_relay_input("  relay.example.com ").as_deref(), Ok("wss://relay.example.com"));
        assert_eq!(parse_relay_input("relay.example.com:7777").as_deref(), Ok("wss://relay.example.com:7777"));
        assert_eq!(parse_relay_input("wss://relay.example.com:443").as_deref(), Ok("wss://relay.example.com"));
        assert_eq!(parse_relay_input("WSS://a.b/nostr/v1/").as_deref(), Ok("wss://a.b/nostr/v1"));
        assert_eq!(parse_relay_input("localhost:4869").as_deref(), Ok("wss://localhost:4869"));
        // Not relays.
        for bad in ["", "   ", "relay", "ws://relay.example.com", "https://relay.example.com", "wss://", "wss://a b.com", "a..b.com", "-a.com", "relay.example.com:0", "relay.example.com:99999", "relay.example.com:x", "wss://a.com/x?y=1", "a.com#f", "wss://:80"] {
            assert!(parse_relay_input(bad).is_err(), "{bad:?} should be refused");
        }
        // Adding: stored normalised, the same relay under another spelling refused.
        let mut list = default_relays();
        assert_eq!(add_relay(&mut list, "Relay.Example.com").as_deref(), Ok("wss://relay.example.com"));
        assert_eq!(list.len(), 4);
        assert!(add_relay(&mut list, "wss://relay.example.com/").unwrap_err().contains("already"));
        assert!(add_relay(&mut list, "WSS://NOS.LOL:443").unwrap_err().contains("already"), "a default under another spelling");
        assert_eq!(list.len(), 4);
        assert!(add_relay(&mut list, "nope").is_err());
        assert_eq!(list.len(), 4);
        // Any number, up to the cap.
        for i in list.len()..MAX_RELAYS {
            add_relay(&mut list, &format!("r{i}.example.com")).unwrap();
        }
        assert_eq!(list.len(), MAX_RELAYS);
        let full = add_relay(&mut list, "one-more.example.com").unwrap_err();
        assert!(full.contains("32"), "{full}");
        assert_eq!(list.len(), MAX_RELAYS);
    }

    #[test]
    fn moving_a_relay_up_or_down_swaps_it_with_its_neighbour_and_stops_at_the_ends() {
        let mut l: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert!(!move_relay(&mut l, 0, true), "nothing above the top");
        assert!(!move_relay(&mut l, 2, false), "nothing below the bottom");
        assert!(!move_relay(&mut l, 9, true), "not in the list");
        assert_eq!(l, ["a", "b", "c"]);
        assert!(move_relay(&mut l, 1, true));
        assert_eq!(l, ["b", "a", "c"]);
        assert!(move_relay(&mut l, 1, false));
        assert_eq!(l, ["b", "c", "a"]);
    }

    #[test]
    fn a_reordered_relay_list_is_saved_and_loaded_in_that_order() {
        let dir = std::env::temp_dir().join(format!("stampede-relay-order-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Profile::default();
        add_relay(&mut p.relays, "relay.example.com").unwrap();
        let last = p.relays.len() - 1;
        for i in (1..=last).rev() {
            assert!(move_relay(&mut p.relays, i, true));
        }
        assert_eq!(p.relays[0], "wss://relay.example.com", "moved to the top");
        p.save(&dir);
        assert_eq!(Profile::load(&dir).relays, p.relays);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_loaded_relay_list_keeps_only_wss() {
        let list = vec!["ws://plain.example.com".to_string(), "wss://relay.example.com".to_string(), "https://x.example.com".to_string()];
        assert_eq!(clean_relays(list), vec!["wss://relay.example.com".to_string()]);
        assert_eq!(clean_relays(vec!["ws://plain.example.com".to_string()]), default_relays(), "nothing left: the defaults");
    }

    #[test]
    fn added_relays_survive_a_save_and_load() {
        let dir = std::env::temp_dir().join(format!("stampede-relays-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Profile::default();
        add_relay(&mut p.relays, "relay.example.com").unwrap();
        add_relay(&mut p.relays, "wss://two.example.org:8443/nostr").unwrap();
        p.save(&dir);
        let q = Profile::load(&dir);
        assert_eq!(q.relays, p.relays);
        assert_eq!(q.relays.len(), 5);
        // A hand-edited file with repeats and an over-long list is tidied on load.
        let mut many: Vec<String> = (0..40).map(|i| format!("wss://m{i}.example.com")).collect();
        many.push("wss://M0.example.com/".into());
        let messy = Profile { relays: many, ..Profile::default() };
        messy.save(&dir);
        let l = Profile::load(&dir);
        assert_eq!(l.relays.len(), MAX_RELAYS);
        assert_eq!(l.relays.iter().filter(|r| r.to_lowercase().contains("m0.")).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
