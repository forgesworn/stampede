//! Shared plumbing for the menu screens: the page frame (backdrop + panel),
//! the wordmark, time formatting, the 3D stage hook and race-launch helpers.

use crate::flow::{RaceKind, RaceLaunch, RacerResult};
use crate::platform::data_dir::Profile;
use crate::render::ScenePass;
use crate::ui::art::{self, Brand, Scene};
use crate::ui::screens::Ctx;
use crate::ui::widgets::Btn;
use crate::ui::{theme, widgets};
use egui::{Color32, LayerId, Pos2, Rect, Ui};
use race_core::{BotLevel, ChallengeId, Character, RaceConfig, RaceMode, RacerKind, RacerSetup, TrackId};
use std::sync::Arc;

/// Shared choice between side-by-side landscape panels and a portrait stack.
#[derive(Clone, Copy)]
pub struct Layout {
    pub portrait: bool,
}

impl Layout {
    pub fn new(safe: Rect) -> Self { Self { portrait: safe.width() < safe.height() } }

    /// First panel on the left or above the second. Heights are points in portrait.
    pub fn split(self, area: Rect, right_width: f32, top_height: f32, gap: f32) -> (Rect, Rect) {
        if self.portrait {
            let y = area.top() + top_height.min((area.height() - gap).max(0.0));
            (Rect::from_min_max(area.min, egui::pos2(area.right(), y)),
             Rect::from_min_max(egui::pos2(area.left(), y + gap), area.max))
        } else {
            (Rect::from_min_max(area.min, egui::pos2(area.right() - right_width - gap, area.bottom())),
             Rect::from_min_max(egui::pos2(area.right() - right_width, area.top()), area.max))
        }
    }
}

/// Scroll a hand-painted page, clipping both its drawing and its input to the body.
pub fn scroll<R>(ui: &mut Ui, area: Rect, id: &str, height: f32, add: impl FnOnce(&mut Ui, Rect) -> R) -> R {
    ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
        ui.set_clip_rect(area.intersect(ui.clip_rect()));
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
            let rect = Rect::from_min_size(ui.next_widget_position(), egui::vec2(ui.available_width(), height.max(area.height())));
            ui.allocate_rect(rect, egui::Sense::hover());
            add(ui, rect)
        }).inner
    }).inner
}

/// Per-screen frame prologue: audio/haptics boot and the entry clock.
pub fn begin(ctx: &mut Ctx, t: &mut f32) {
    crate::ui::feedback::boot(ctx);
    crate::game::stage::set_quality(ctx.profile.quality);
    *t += ctx.dt;
}

/// Paint the backdrop for a screen and run `add` in a transparent central panel.
/// `veil` darkens the scenery (0 = full sunset, ~0.4 = a quiet settings page).
pub fn page(screen: Rect, time: f32, ui: &egui::Context, veil: f32, add: impl FnOnce(&mut Ui, &Arc<Brand>)) {
    let brand = art::brand(ui);
    let over = crate::game::stage::is_drawing();
    widgets::set_over_3d(over);
    let p = ui.layer_painter(LayerId::background());
    if over {
        art::backdrop_over_3d(&p, screen, 0.0);
    } else {
        art::backdrop(&p, &brand, screen, Scene { t: time, sun: 0.0, veil, night: 0.0, horizon: true });
    }
    #[allow(deprecated)]
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| add(ui, &brand));
}

/// Drives `game::stage::paint` from `paint_scene` with a frame delta.
#[derive(Default)]
pub struct StagePaint {
    last: Option<f32>,
}

impl StagePaint {
    pub fn paint(&mut self, pass: &mut ScenePass) {
        let dt = self.last.map(|l| (pass.time - l).clamp(0.0, 0.1)).unwrap_or(0.0);
        self.last = Some(pass.time);
        crate::game::stage::paint(pass, dt);
    }
}

/// The STAMPEDE wordmark: Lilita One with an orange gradient fill and a plum outline.
pub fn wordmark(p: &egui::Painter, centre: Pos2, size: f32, alpha: f32) -> Rect {
    art::gradient_text(
        p,
        centre,
        "STAMPEDE",
        theme::title(size),
        Color32::from_rgb(0xFF, 0xCE, 0x5C),
        Color32::from_rgb(0xE2, 0x4F, 0x16),
        Color32::from_rgb(0x3B, 0x12, 0x52),
        size * 0.085,
        alpha,
    )
}

/// `1:23.45` (or `23.45` under a minute).
pub fn fmt_time(s: f32) -> String {
    if !s.is_finite() || s < 0.0 {
        return "--".into();
    }
    let cs = (s * 100.0).round() as u32;
    let (m, rest) = (cs / 6000, cs % 6000);
    if m > 0 {
        format!("{m}:{:02}.{:02}", rest / 100, rest % 100)
    } else {
        format!("{}.{:02}", rest / 100, rest % 100)
    }
}

/// `1st`, `2nd`, ...
pub fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The profile key for a track's best times.
pub fn best_key(id: TrackId) -> String {
    if id == TrackId::Custom {
        if let Some(track) = crate::custom_tracks::active() {
            return format!("custom-{:016x}", track.hash());
        }
    }
    format!("{id:?}")
}

pub fn best_time(profile: &Profile, id: TrackId) -> Option<f32> {
    if id == TrackId::Custom {
        return crate::custom_tracks::active().and_then(|t| profile.custom_bests.get(&t.hash()).copied());
    }
    profile.best_times.get(&best_key(id)).copied()
}

pub fn best_lap(profile: &Profile, id: TrackId) -> Option<f32> {
    if id == TrackId::Custom {
        return crate::custom_tracks::active().and_then(|t| profile.custom_laps.get(&t.hash()).copied());
    }
    profile.best_laps.get(&best_key(id)).copied()
}

thread_local! {
    static MENU_TRACK: std::cell::Cell<Option<TrackId>> = const { std::cell::Cell::new(None) };
}

/// The track the 3D attract race shows behind the title and the menus: one per app run
/// (so going from the title to the menu does not restart the race), any of the eight.
pub fn menu_track() -> TrackId {
    MENU_TRACK.with(|c| {
        c.get().unwrap_or_else(|| {
            let t = TrackId::RACE_ALL[(fresh_seed() >> 8) as usize % TrackId::RACE_ALL.len()];
            c.set(Some(t));
            t
        })
    })
}

/// A fresh race seed (menus may read the wall clock; the sim never does).
pub fn fresh_seed() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1234)
}

/// Select a saved or drawn track and race its authored lap count.
pub fn custom_launch(profile: &Profile, track: &race_core::CustomTrack, trial: bool, bots: usize, level: BotLevel) -> Result<RaceLaunch, String> {
    crate::custom_tracks::activate(track)?;
    let mut config = RaceConfig::quick(TrackId::Custom, if trial { 0 } else { bots }, level, fresh_seed());
    if trial { config.mode = RaceMode::TimeTrial; }
    config.laps = track.laps;
    seat_player(&mut config, profile);
    Ok(RaceLaunch { config, kind: if trial { RaceKind::TimeTrial } else { RaceKind::Quick }, local_seat: 0 })
}

/// Easy / Normal / Hard as chosen on the setup screens.
pub const LEVELS: [(&str, BotLevel); 3] = [("Easy", BotLevel::Easy), ("Normal", BotLevel::Normal), ("Hard", BotLevel::Hard)];

/// Fill seat 0 with the player and give the bots the ostriches the player did not pick.
pub fn seat_player(cfg: &mut RaceConfig, profile: &Profile) {
    let me = profile.character.min(7);
    if let Some(first) = cfg.racers.first_mut() {
        first.name = profile.name.clone();
        first.character = me;
    }
    let mut others = (0..8u8).filter(|&c| c != me);
    for r in cfg.racers.iter_mut().skip(1) {
        if matches!(r.kind, RacerKind::Bot(_)) {
            if let Some(c) = others.next() {
                r.character = c;
                r.name = Character::get(c).name.into();
            }
        }
    }
}

/// A Quick Race launch.
pub fn quick_launch(profile: &Profile, track: TrackId, laps: u8, bots: usize, level: BotLevel) -> RaceLaunch {
    let mut config = RaceConfig::quick(track, bots, level, fresh_seed());
    config.laps = laps.clamp(1, 5);
    seat_player(&mut config, profile);
    RaceLaunch { config, kind: RaceKind::Quick, local_seat: 0 }
}

/// A Time Trial launch: just you.
pub fn time_trial_launch(profile: &Profile, track: TrackId, laps: u8) -> RaceLaunch {
    let mut config = RaceConfig::quick(track, 0, BotLevel::Normal, fresh_seed());
    config.laps = laps.clamp(1, 5);
    config.mode = RaceMode::TimeTrial;
    seat_player(&mut config, profile);
    RaceLaunch { config, kind: RaceKind::TimeTrial, local_seat: 0 }
}

/// The two four-track cups (SPEC 16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cup {
    One,
    Two,
}

impl Cup {
    pub const ALL: [Cup; 2] = [Cup::One, Cup::Two];

    pub fn tracks(self) -> [TrackId; 4] {
        match self {
            Cup::One => TrackId::RACE,
            Cup::Two => TrackId::CUP2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Cup::One => "Cup 1",
            Cup::Two => "Cup 2",
        }
    }

    /// The cup a track belongs to (the Training Ground counts as Cup 1).
    pub fn of(id: TrackId) -> Cup {
        if TrackId::CUP2.contains(&id) {
            Cup::Two
        } else {
            Cup::One
        }
    }
}

/// The track after `id` in the list of all eight race tracks (wraps).
pub fn next_track(id: TrackId) -> TrackId {
    let cur = TrackId::RACE_ALL.iter().position(|t| *t == id).unwrap_or(0);
    TrackId::RACE_ALL[(cur + 1) % TrackId::RACE_ALL.len()]
}

/// Round 0 of a Grand Prix: the four tracks of `cup` in order, eight racers.
pub fn grand_prix_launch(profile: &Profile, cup: Cup, laps: u8, level: BotLevel) -> RaceLaunch {
    let tracks = cup.tracks().to_vec();
    let mut config = RaceConfig::quick(tracks[0], 7, level, fresh_seed());
    config.laps = laps.clamp(1, 5);
    seat_player(&mut config, profile);
    let n = config.racers.len();
    RaceLaunch { config, kind: RaceKind::GrandPrix { tracks, round: 0, points: vec![0; n] }, local_seat: 0 }
}

/// A Battle launch (Feather Frenzy): you plus `bots` (1-7) on `arena`, default rules.
pub fn battle_launch(profile: &Profile, arena: TrackId, bots: usize, level: BotLevel) -> RaceLaunch {
    let bots = bots.clamp(1, 7);
    let arena = if TrackId::ARENAS.contains(&arena) { arena } else { TrackId::ARENAS[0] };
    let mut config = RaceConfig::battle_bots(arena, bots + 1, level, race_core::BattleRules::default(), fresh_seed());
    if let Some(first) = config.racers.first_mut() {
        first.kind = RacerKind::Human;
    }
    seat_player(&mut config, profile);
    RaceLaunch { config, kind: RaceKind::Battle, local_seat: 0 }
}

/// The other arena (for "Next" on the battle results).
pub fn next_arena(id: TrackId) -> TrackId {
    let cur = TrackId::ARENAS.iter().position(|t| *t == id).unwrap_or(0);
    TrackId::ARENAS[(cur + 1) % TrackId::ARENAS.len()]
}

/// A Challenge launch: you plus the challenge's own rivals.
pub fn challenge_launch(profile: &Profile, id: ChallengeId) -> RaceLaunch {
    let mut config = RaceConfig::challenge(id, profile.character.min(7), fresh_seed());
    if let Some(first) = config.racers.first_mut() {
        first.name = profile.name.clone();
    }
    RaceLaunch { config, kind: RaceKind::Challenge(id), local_seat: 0 }
}

/// Total race times count as a personal best only over the standard length.
pub const BEST_TIME_LAPS: u8 = 3;

/// What a finished race changed in the profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bests {
    pub new_time: bool,
    pub new_lap: bool,
}

/// Write the local racer's time and best lap into the profile (the one place
/// that does; the race screen leaves profile writes to the screens after it).
/// A best lap counts at any length; a best total time only over [`BEST_TIME_LAPS`].
pub fn record_bests(profile: &mut Profile, track: TrackId, laps: u8, me: &RacerResult) -> Bests {
    if track == TrackId::Custom {
        let mut out = Bests::default();
        let Some(t) = crate::custom_tracks::active() else { return out };
        let hash = t.hash();
        if laps == t.laps {
            if let Some(time) = me.time_s.filter(|s| s.is_finite() && *s > 0.0) {
                if profile.custom_bests.get(&hash).is_none_or(|old| time < *old) {
                    profile.custom_bests.insert(hash, time);
                    out.new_time = true;
                }
            }
        }
        if let Some(lap) = me.best_lap_s.filter(|s| s.is_finite() && *s > 0.0) {
            if profile.custom_laps.get(&hash).is_none_or(|old| lap < *old) {
                profile.custom_laps.insert(hash, lap);
                out.new_lap = true;
            }
        }
        return out;
    }
    let key = best_key(track);
    let mut out = Bests::default();
    if laps == BEST_TIME_LAPS {
        if let Some(t) = me.time_s.filter(|t| t.is_finite() && *t > 0.0) {
            let old = profile.best_times.get(&key).copied();
            if old.is_none_or(|o| t < o) {
                profile.best_times.insert(key.clone(), t);
                out.new_time = true;
            }
        }
    }
    if let Some(l) = me.best_lap_s.filter(|l| l.is_finite() && *l > 0.0) {
        let old = profile.best_laps.get(&key).copied();
        if old.is_none_or(|o| l < o) {
            profile.best_laps.insert(key, l);
            out.new_lap = true;
        }
    }
    out
}

/// Did the player complete the challenge (Egg Hunt gives up at its time limit with the eggs not all down)?
pub fn challenge_completed(c: &crate::flow::ChallengeResult) -> bool {
    c.done && (!matches!(c.id, ChallengeId::EggHunt | ChallengeId::LaserSniper) || c.progress >= c.id.goal())
}

/// Write a challenge result into the profile; true if it is a new best score.
/// `done`: the challenge was completed (see [`challenge_completed`]); only then does the score count.
pub fn record_challenge(profile: &mut Profile, id: ChallengeId, score: f32, medal: u8, done: bool) -> bool {
    let i = ChallengeId::ALL.iter().position(|c| *c == id).unwrap_or(0);
    profile.challenge_medals[i] = profile.challenge_medals[i].max(medal.min(3));
    if !done || !score.is_finite() {
        return false;
    }
    let old = profile.challenge_best[i];
    let better = old <= 0.0 || if id.higher_is_better() { score > old } else { score < old };
    if better && score > 0.0 {
        profile.challenge_best[i] = score;
        return true;
    }
    false
}

/// How a challenge score reads: coins "104", time "1:02.30", place "2nd".
pub fn fmt_score(id: ChallengeId, score: f32) -> String {
    match id {
        ChallengeId::EggHunt | ChallengeId::TurboTimeAttack | ChallengeId::LaserSniper => fmt_time(score),
        ChallengeId::Elimination | ChallengeId::LastFeather => ordinal(score.round().clamp(1.0, 8.0) as u8),
        _ => format!("{}", score.round() as i64),
    }
}

/// `Bronze`, `Silver`, `Gold`.
pub fn medal_name(m: u8) -> &'static str {
    match m {
        1 => "Bronze",
        2 => "Silver",
        3 => "Gold",
        _ => "No medal",
    }
}

/// The next Grand Prix round after `launch` with the new `points`.
pub fn next_round(launch: &RaceLaunch, points: Vec<u32>) -> Option<RaceLaunch> {
    let RaceKind::GrandPrix { tracks, round, .. } = &launch.kind else { return None };
    let next = round + 1;
    let track = *tracks.get(next)?;
    let mut config = launch.config.clone();
    config.track = track;
    config.seed = fresh_seed();
    Some(RaceLaunch { config, kind: RaceKind::GrandPrix { tracks: tracks.clone(), round: next, points }, local_seat: launch.local_seat })
}

/// A retry of `launch` with a fresh seed.
pub fn retry(launch: &RaceLaunch) -> RaceLaunch {
    let mut l = launch.clone();
    l.config.seed = fresh_seed();
    l
}

/// A (name, character) pair used to map results back to racer seats.
pub fn seat_of(config: &RaceConfig, name: &str, character: u8, taken: &[bool]) -> Option<usize> {
    config.racers.iter().enumerate().position(|(i, RacerSetup { name: n, character: c, .. })| !taken[i] && n == name && *c == character)
}

#[cfg(test)]
mod tests {
    #[test]
    fn pickers_use_the_right_track_sets() {
        // race pickers never offer an arena, battle pickers only arenas
        for cup in Cup::ALL {
            assert!(cup.tracks().iter().all(|t| !t.is_arena()));
        }
        assert!(TrackId::RACE_ALL.iter().all(|t| !t.is_arena()));
        assert_eq!(TrackId::RACE_ALL.len(), 8);
        assert!(TrackId::ARENAS.iter().all(|t| t.is_arena()));
        // "Next" on a race result cycles only race tracks, from any of them
        for t in TrackId::RACE_ALL {
            assert!(!next_track(t).is_arena());
        }
        // even from a stale arena it lands on a race track
        assert!(!next_track(TrackId::DustBowl).is_arena());
        // the other arena button swaps between the two
        assert_eq!(next_arena(next_arena(TrackId::DustBowl)), TrackId::DustBowl);
        // the title attract race is never an arena, whatever the seed
        for _ in 0..4 {
            assert!(!menu_track().is_arena());
        }
        // a Time Trial or Quick launch on every picker track uses a race track
        let p = Profile::default();
        for t in TrackId::RACE_ALL {
            assert!(!quick_launch(&p, t, 3, 3, BotLevel::Normal).config.track.is_arena());
            assert!(!time_trial_launch(&p, t, 3).config.track.is_arena());
        }
        // a GP is made of race tracks only
        for cup in Cup::ALL {
            let l = grand_prix_launch(&p, cup, 3, BotLevel::Normal);
            let RaceKind::GrandPrix { tracks, .. } = &l.kind else { panic!() };
            assert!(tracks.iter().all(|t| !t.is_arena()));
        }
    }

    use super::*;

    fn me(time: Option<f32>, lap: Option<f32>) -> RacerResult {
        RacerResult { name: "Rider".into(), character: 0, place: 1, time_s: time, best_lap_s: lap, coins: 0, is_local: true, is_bot: false, battle: None }
    }

    #[test]
    fn bests_are_written_once_and_only_improve() {
        let mut p = Profile::default();
        let b = record_bests(&mut p, TrackId::Dune, 3, &me(Some(100.0), Some(31.0)));
        assert_eq!(b, Bests { new_time: true, new_lap: true });
        // the same race again is not "new"
        assert_eq!(record_bests(&mut p, TrackId::Dune, 3, &me(Some(100.0), Some(31.0))), Bests::default());
        assert_eq!(record_bests(&mut p, TrackId::Dune, 3, &me(Some(120.0), Some(29.0))), Bests { new_time: false, new_lap: true });
        assert_eq!((p.best_times["Dune"], p.best_laps["Dune"]), (100.0, 29.0));
        // a 1-lap race does not set a total-time best, but its lap counts
        let b = record_bests(&mut p, TrackId::Baobab, 1, &me(Some(30.0), Some(30.0)));
        assert_eq!(b, Bests { new_time: false, new_lap: true });
        assert!(p.best_times.get("Baobab").is_none() && p.best_laps["Baobab"] == 30.0);
        // a DNF records nothing
        assert_eq!(record_bests(&mut p, TrackId::Halving, 3, &me(None, None)), Bests::default());
    }

    #[test]
    fn challenge_medals_and_bests_keep_the_better() {
        let mut p = Profile::default();
        let cr = ChallengeId::CoinRush;
        assert!(record_challenge(&mut p, cr, 104.0, cr.medal_for(104.0), true));
        assert_eq!((p.challenge_medals[0], p.challenge_best[0]), (2, 104.0));
        assert!(!record_challenge(&mut p, cr, 80.0, cr.medal_for(80.0), true), "worse score");
        assert_eq!((p.challenge_medals[0], p.challenge_best[0]), (2, 104.0));
        // lower is better for time and place
        let eh = ChallengeId::EggHunt;
        assert!(record_challenge(&mut p, eh, 70.0, eh.medal_for(70.0), true));
        assert!(record_challenge(&mut p, eh, 55.0, eh.medal_for(55.0), true));
        assert!(!record_challenge(&mut p, eh, 90.0, eh.medal_for(90.0), true));
        assert_eq!((p.challenge_medals[1], p.challenge_best[1]), (2, 55.0));
        // a give-up is no best and no medal
        let mut q = Profile::default();
        assert!(!record_challenge(&mut q, eh, 150.0, 0, false));
        assert_eq!((q.challenge_medals[1], q.challenge_best[1]), (0, 0.0));
        let el = ChallengeId::Elimination;
        assert!(record_challenge(&mut q, el, 5.0, 0, true) && !record_challenge(&mut q, el, 6.0, 0, true));
    }

    #[test]
    fn grand_prix_cups_and_track_cycle_cover_all_eight() {
        let p = Profile::default();
        let g2 = grand_prix_launch(&p, Cup::Two, 3, BotLevel::Normal);
        let RaceKind::GrandPrix { tracks, .. } = &g2.kind else { panic!() };
        assert_eq!(tracks.as_slice(), &TrackId::CUP2);
        assert_eq!(g2.config.track, TrackId::Baobab);
        let mut t = TrackId::RACE_ALL[0];
        let mut seen = Vec::new();
        for _ in 0..8 {
            seen.push(t);
            t = next_track(t);
        }
        assert_eq!(seen.as_slice(), &TrackId::RACE_ALL);
        assert_eq!(t, TrackId::RACE_ALL[0]);
        assert_eq!((Cup::of(TrackId::Dune), Cup::of(TrackId::Oasis)), (Cup::One, Cup::Two));
        let c = challenge_launch(&p, ChallengeId::Elimination);
        assert_eq!((c.config.racers.len(), c.config.track), (8, TrackId::Dune));
        assert!(matches!(c.kind, RaceKind::Challenge(ChallengeId::Elimination)));
    }

    #[test]
    fn times_and_places_read_well() {
        assert_eq!(fmt_time(83.456), "1:23.46");
        assert_eq!(fmt_time(9.0), "9.00");
        assert_eq!(fmt_time(f32::NAN), "--");
        assert_eq!([ordinal(1), ordinal(2), ordinal(3), ordinal(4), ordinal(11), ordinal(12), ordinal(21)], ["1st", "2nd", "3rd", "4th", "11th", "12th", "21st"]);
    }
}

/// The two buttons of a [`prompt`] of height `h` on `screen`: (left, right).
pub fn prompt_buttons(screen: Rect, h: f32) -> (Rect, Rect) {
    use egui::{pos2, vec2};
    let card = Rect::from_center_size(screen.center(), vec2(screen.width().min(460.0), h));
    let bw = (card.width() - 48.0 - widgets::S3) / 2.0;
    let by = card.bottom() - 18.0 - 44.0;
    (Rect::from_min_size(pos2(card.left() + 24.0, by), vec2(bw, 44.0)), Rect::from_min_size(pos2(card.left() + 24.0 + bw + widgets::S3, by), vec2(bw, 44.0)))
}

/// A question card over a dimmed screen. `Some(true)` = the right (primary)
/// button, `Some(false)` = the left one, `None` = still asking.
#[allow(clippy::too_many_arguments)]
pub fn prompt(ui: &egui::Context, screen: Rect, id: &str, h: f32, title: &str, msg: &str, left: &str, right: &str) -> Option<bool> {
    use egui::{pos2, vec2, Align2, Color32};
    let dim = ui.layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new((id, "dim"))));
    dim.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
    let card = Rect::from_center_size(screen.center(), vec2(screen.width().min(460.0), h));
    let (lr, rr) = prompt_buttons(screen, h);
    let mut answer = None;
    egui::Area::new(egui::Id::new(id)).order(egui::Order::Foreground).fixed_pos(card.min).show(ui, |ui| {
        ui.set_min_size(card.size());
        let p = ui.painter().clone();
        widgets::panel_solid(&p, card, 22.0);
        p.text(pos2(card.center().x, card.top() + 34.0), Align2::CENTER_CENTER, title, theme::title(24.0), theme::TEXT);
        let g = p.layout(msg.to_string(), theme::ui_medium(14.0), theme::TEXT_DIM, card.width() - 48.0);
        p.galley(pos2(card.left() + 24.0, card.top() + 64.0), g, theme::TEXT_DIM);
        if widgets::button(ui, lr, &format!("{id}-left"), &Btn::new(left)).clicked() {
            answer = Some(false);
        }
        if widgets::button(ui, rr, &format!("{id}-right"), &Btn::primary(right)).clicked() {
            answer = Some(true);
        }
    });
    answer
}

/// A join link that arrived while the player is in a room: what to ask before leaving it.
pub struct LeaveAsk {
    pub target: crate::keeps::JoinTarget,
    /// An invite for a Keep that is not in My Keeps: one card both asks to leave and to join.
    unknown: Option<[u8; 32]>,
    /// The Keep's name (a saved Keep) or the room's code, for the question.
    label: String,
    /// Seconds the card has been up for this link.
    age: f32,
}

impl LeaveAsk {
    pub fn new(data_dir: &std::path::Path, target: crate::keeps::JoinTarget) -> LeaveAsk {
        let (mut unknown, mut label) = (None, target.label());
        if let crate::keeps::JoinTarget::Invite { host_key, .. } = &target {
            let hex = race_session::invite::hex_encode(host_key);
            match crate::keeps::load(data_dir).into_iter().find(|k| k.host_key == hex) {
                Some(k) => label = k.name,
                None => unknown = Some(*host_key),
            }
        }
        LeaveAsk { target, unknown, label, age: 0.0 }
    }

    /// Count the time the card has been up (call from `update`).
    pub fn tick(&mut self, dt: f32) {
        self.age += dt;
    }
}

/// How long a "Join a Keep?" card ignores its Join button after the Keep it names changed.
pub const CONFIRM_GUARD_SECS: f32 = 0.5;

/// "Leave this room and join ...?": a join link arrived while you are in a room.
/// A Keep not in My Keeps gets one card that asks both questions. `Some(true)` =
/// leave and join, `Some(false)` = stay, `None` = still asking. For an unknown Keep
/// the agreement is also recorded in `ctx` (in memory only) so the join screen does
/// not ask a second time.
pub fn leave_prompt(ctx: &mut super::Ctx, ui: &egui::Context, ask: &LeaveAsk) -> Option<bool> {
    let screen = ctx.viewport.screen;
    match ask.unknown {
        None => prompt(ui, screen, "join-prompt", 190.0, "Join another game?", &format!("Leave this room and join {}?", ask.label), "Stay", "Leave and join"),
        Some(host_key) => {
            let msg = format!("Leave this room and join {}? You haven't played on it. Only join if you trust the person who sent the link.", ask.label);
            // A tap meant for another link's card must not answer this one.
            match prompt(ui, screen, "keep-confirm", KEEP_CONFIRM_H, "Join a Keep?", &msg, "Not now", "Join") {
                Some(true) if ask.age < CONFIRM_GUARD_SECS => None,
                Some(true) => {
                    ctx.confirm_join(host_key);
                    Some(true)
                }
                other => other,
            }
        }
    }
}

/// Card height of the unseen-Keep question.
pub const KEEP_CONFIRM_H: f32 = 250.0;

/// "Join a Keep?": an invite for a Keep that is not in My Keeps. Never shows
/// the secret. `Some(true)` = Join, `Some(false)` = Not now.
pub fn keep_confirm_prompt(ui: &egui::Context, screen: Rect, host_hex: &str) -> Option<bool> {
    let msg = format!("Someone sent you an invite to a Keep you haven't played on. Keep ID: {}. Only join if you trust the person who sent it.", crate::keeps::keep_id(host_hex));
    prompt(ui, screen, "keep-confirm", KEEP_CONFIRM_H, "Join a Keep?", &msg, "Not now", "Join")
}
