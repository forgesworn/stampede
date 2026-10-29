//! The screen router: a stack of [`Screen`]s, [`Transition`]s between them, and
//! the [`Ctx`] every screen method gets. To add a screen: implement [`Screen`]
//! in a new file here and add a line to [`SCREENS`] (see `docs/api/app-shell.md`).

pub mod account;
pub mod after_race;
pub mod battle;
pub mod challenges;
pub mod common;
#[cfg(test)]
mod layout_tests;
pub mod credits;
pub mod designer;
pub mod games;
pub mod garage;
pub mod wardrobe;
pub mod home;
pub mod join;
pub mod lobby;
pub mod menu;
pub mod migration;
pub mod multiplayer;
pub mod my_tracks;
pub mod nearby;
pub mod online_results;
pub mod settings;
pub mod settings_cards;
pub mod setup;
pub mod splash;
pub mod tracks;
pub mod training;

use crate::cli::Options;
use crate::haptics::Haptics;
use crate::platform::data_dir::Profile;
use crate::platform::{Keys, Platform, Touches, Viewport};
use crate::render::ScenePass;
use crate::ui::theme;
use std::path::Path;

/// How long a message from `Ctx::say` shows.
pub const TOAST_SECS: f32 = 2.6;
/// Dip to the background colour before a screen change, seconds.
pub const FADE_OUT: f32 = 0.14;
/// ...and back up from it.
pub const FADE_IN: f32 = 0.22;

/// A short message over any screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub text: String,
    pub left: f32,
}

/// Everything a screen may use. Built by the engine for every `update`, `draw`
/// and `back` call; not stored.
pub struct Ctx<'a> {
    /// Seconds since the previous frame (0.1 at most).
    pub dt: f32,
    /// Engine time in seconds (a virtual clock in `--shot`).
    pub time: f64,
    /// Screen size, insets and safe rectangle in UI points.
    pub viewport: Viewport,
    /// Every finger down (raw multi-touch). Empty while a screen change fades out.
    pub touches: &'a Touches,
    /// Keyboard (desktop).
    pub keys: &'a Keys,
    pub haptics: &'a mut dyn Haptics,
    /// Clipboard, launch links, wake lock, multicast lock.
    pub platform: &'a mut dyn Platform,
    pub tilt: &'a mut crate::platform::tilt::Steering,
    /// The player's name and settings. Changes are saved on exit and when the app is backgrounded.
    pub profile: &'a mut Profile,
    pub data_dir: &'a Path,
    /// A room (`stampede://join#CODE`) or a Keep (`stampede://keep/...`) from a launch link, waiting for a screen to take it.
    pub join_code: &'a mut Option<crate::keeps::JoinTarget>,
    /// The host key of a Keep the player has just said yes to in a "Leave this room and
    /// join Keep ...?" card. Set only by that card, kept in memory, never by a link.
    pub join_confirmed: &'a mut Option<[u8; 32]>,
    /// A screen change is fading this screen out: it is still updated, but not on top for long.
    pub input_blocked: bool,
    /// The app is backgrounded or unfocused: keep sound off.
    pub muted: bool,
    pub opts: &'a Options,
    pub(crate) toast: &'a mut Option<Toast>,
    /// The app's online session (lobby, race, results, join); see `crate::online`.
    pub online: &'a crate::online::OnlineSlot,
    /// In-app updates, and the download / install path More games shares.
    pub updates: &'a mut crate::update::Updater,
    pub account: &'a mut Option<crate::account::Account>,
}

impl Ctx<'_> {
    /// Show a message over the screen for a couple of seconds.
    pub fn say(&mut self, text: impl Into<String>) {
        *self.toast = Some(Toast { text: text.into(), left: TOAST_SECS });
    }

    /// Take the waiting join target, if a join link arrived.
    pub fn take_join_code(&mut self) -> Option<crate::keeps::JoinTarget> {
        self.join_code.take()
    }

    /// The player agreed to join this Keep in a card: the join screen need not ask again.
    pub fn confirm_join(&mut self, host_key: [u8; 32]) {
        *self.join_confirmed = Some(host_key);
    }

    /// Was `host_key` just agreed to? Always clears the agreement, so it covers one join only.
    pub fn take_join_confirmed(&mut self, host_key: &[u8; 32]) -> bool {
        matches!(self.join_confirmed.take(), Some(k) if k == *host_key)
    }

    /// Write the profile now.
    pub fn save_profile(&self) {
        self.profile.save(self.data_dir);
    }
}

/// What a screen asks the router to do after a call.
pub enum Transition {
    None,
    /// Put a screen on top (Back returns to the current one), with a fade.
    Push(Box<dyn Screen>),
    /// Swap the current screen for another, with a fade.
    Replace(Box<dyn Screen>),
    /// Swap with no fade.
    Cut(Box<dyn Screen>),
    /// Go back one screen. On the first screen this quits (Android: to the background).
    Pop,
    /// Pop screens until the one with this `name()` is on top.
    PopTo(&'static str),
    /// Leave the app.
    Quit,
}

impl Transition {
    pub fn is_none(&self) -> bool {
        matches!(self, Transition::None)
    }
}

/// One screen of the app.
pub trait Screen {
    /// Stable lower-case name: the key `--shot NAME` / `--screen NAME` use, and
    /// what `Transition::PopTo` matches.
    fn name(&self) -> &'static str;

    /// Logic, once per frame, before drawing: read `ctx.touches` / `ctx.keys`,
    /// advance state by `ctx.dt`. A returned transition takes effect this frame.
    fn update(&mut self, _ctx: &mut Ctx) -> Transition {
        Transition::None
    }

    /// Draw the UI with egui (and read egui widget input), once per frame after `update`.
    fn draw(&mut self, _ctx: &mut Ctx, _ui: &egui::Context) -> Transition {
        Transition::None
    }

    /// System Back (Android Back key, Esc on desktop). Default: go back one screen.
    fn back(&mut self, _ctx: &mut Ctx) -> Transition {
        Transition::Pop
    }

    /// The colour (sRGB 0..1, rgba) the frame is cleared to before anything is drawn.
    fn clear_color(&self, _ctx: &Ctx) -> [f32; 4] {
        theme::bg_clear()
    }

    /// Draw your own scene (the 3D race view) into the frame, after the clear
    /// and before egui. See [`ScenePass`].
    fn paint_scene(&mut self, _pass: &mut ScenePass) {}

    /// Driving and wide editing tools keep the phone sideways.
    fn orientation(&self) -> crate::platform::os::Orientation {
        crate::platform::os::Orientation::Free
    }

    /// Keep the screen on while this screen is showing (a race running).
    fn keep_awake(&self) -> bool {
        false
    }

    /// A race or the tilt setup screen; the engine also checks the setting and foreground state.
    fn uses_tilt(&self) -> bool { false }

    /// The app went to the background (or lost focus): pause what must pause.
    fn on_background(&mut self, _ctx: &mut Ctx) {}

    /// Does this screen show the app's online session (lobby, race, results,
    /// join)? The engine pumps the session whichever screen is on top, pulls
    /// the player back to the nearest such screen when the room needs them, and
    /// leaves the room when no screen on the stack uses it.
    fn uses_online(&self) -> bool {
        false
    }
}

/// The screens `--shot` and `--screen` can name, plus staged variants (`*_sample`)
/// that fake the state a screen needs fingers or a finished race for.
pub const SCREENS: &[(&str, fn() -> Box<dyn Screen>)] = &[
    ("migration_consent_sample", || Box::new(migration::SampleScreen::new(0))),
    ("migration_picker_sample", || Box::new(migration::SampleScreen::new(1))),
    ("migration_sheet_sample", || Box::new(migration::SampleScreen::new(2))),
    ("migration_stopped_sample", || Box::new(migration::SampleScreen::new(3))),
    ("signin", || Box::new(account::AccountScreen::sample())),
    ("play_online", || Box::new(account::AccountScreen::online(None))),
    ("designer", || Box::new(designer::DesignerScreen::new())),
    ("designer_sample", || Box::new(designer::DesignerScreen::sample())),
    ("designer_hills", || Box::new(designer::DesignerScreen::hills_sample())),
    ("designer_error", || Box::new(designer::DesignerScreen::error_sample())),
    ("designer_close", || Box::new(designer::DesignerScreen::close_sample())),
    ("track_build", || Box::new(designer::DesignerScreen::build_sample())),
    ("designer_items", || Box::new(designer::DesignerScreen::items_sample())),
    ("designer_bridge", || Box::new(designer::DesignerScreen::bridge_sample())),
    ("my_tracks", || Box::new(my_tracks::MyTracksScreen::new())),
    ("my_tracks_sample", || Box::new(my_tracks::MyTracksScreen::sample())),
    ("my_tracks_empty", || Box::new(my_tracks::MyTracksScreen::empty_sample())),
    ("my_tracks_new", || Box::new(my_tracks::MyTracksScreen::chooser_sample())),
    ("run_track", || Box::new(crate::game::recording::RunTrackScreen::new(&Profile::default()))),
    ("run_track_sample", || Box::new(crate::game::recording::RunTrackScreen::sample(&Profile::default()))),
    ("place_items", || Box::new(crate::game::placing::PlaceItemsScreen::new(&Profile::default(), designer::sample_track("Sunset Loop"), Default::default()).expect("sample track launches"))),
    ("place_items_sample", || Box::new(crate::game::placing::PlaceItemsScreen::sample(&Profile::default()))),
    ("drill_hurdles", crate::game::shots_training::drill_hurdles),
    ("splash", || Box::new(splash::SplashScreen::new())),
    ("home", || Box::new(home::HomeScreen::new())),
    ("home_update_sample", || Box::new(home::HomeScreen::update_sample())),
    ("first_run_sample", || Box::new(home::HomeScreen::first_run_sample())),
    ("menu", || Box::new(menu::MenuScreen::new())),
    ("menu_wallet_sample", || Box::new(menu::MenuScreen::demo())),
    ("race_menu", || Box::new(menu::RaceMenuScreen::new())),
    ("quick_race", || Box::new(setup::SetupScreen::new(setup::Mode::Quick))),
    ("quick_race_sample", || Box::new(setup::SetupScreen::sample(setup::Mode::Quick))),
    ("quick_race_cup2_sample", || Box::new(setup::SetupScreen::sample_cup2(setup::Mode::Quick))),
    ("quick_race_training_sample", || Box::new(setup::SetupScreen::sample_training(setup::Mode::Quick))),
    ("grand_prix", || Box::new(setup::SetupScreen::new(setup::Mode::GrandPrix))),
    ("grand_prix_sample", || Box::new(setup::SetupScreen::sample(setup::Mode::GrandPrix))),
    ("grand_prix_cup2_sample", || Box::new(setup::SetupScreen::sample_cup2(setup::Mode::GrandPrix))),
    ("time_trial", || Box::new(setup::SetupScreen::new(setup::Mode::TimeTrial))),
    ("time_trial_sample", || Box::new(setup::SetupScreen::sample(setup::Mode::TimeTrial))),
    ("time_trial_training_sample", || Box::new(setup::SetupScreen::sample_training(setup::Mode::TimeTrial))),
    ("battle_setup", || Box::new(battle::BattleSetupScreen::new())),
    ("battle_setup_sample", || Box::new(battle::BattleSetupScreen::sample())),
    ("battle_results_sample", || Box::new(battle::BattleResultsScreen::sample())),
    ("battle_hud_sample", crate::game::shots::battle_hud_sample),
    ("time_trial_ghost_sample", crate::game::shots::time_trial_ghost_sample),
    ("challenge_hud_sniper", crate::game::shots::challenge_hud_sniper),
    ("challenge_hud_ramp", crate::game::shots::challenge_hud_ramp),
    ("challenge_hud_night", crate::game::shots::challenge_hud_night),
    ("challenge_hud_lastfeather", crate::game::shots::challenge_hud_lastfeather),
    ("challenges", || Box::new(challenges::ChallengesScreen::new())),
    ("challenges_sample", || Box::new(challenges::ChallengesScreen::sample())),
    ("challenge_result_gold_sample", || Box::new(challenges::ChallengeResultScreen::sample(race_core::ChallengeId::CoinRush, 134.0, 134))),
    ("challenge_result_none_sample", || Box::new(challenges::ChallengeResultScreen::sample(race_core::ChallengeId::EggHunt, 150.0, 14))),
    ("challenge_result_silver_sample", || Box::new(challenges::ChallengeResultScreen::sample(race_core::ChallengeId::Elimination, 2.0, 6))),
    ("challenge_result_lastfeather_sample", || Box::new(challenges::ChallengeResultScreen::sample(race_core::ChallengeId::LastFeather, 1.0, 5))),
    ("challenge_result_sniper_sample", || Box::new(challenges::ChallengeResultScreen::sample(race_core::ChallengeId::LaserSniper, 56.0, 15))),
    ("training", || Box::new(training::TrainingScreen::new())),
    ("training_sample", || Box::new(training::TrainingScreen::sample())),
    ("training_fresh_sample", || Box::new(training::TrainingScreen::fresh_sample())),
    ("training_menu", crate::game::shots_training::training_menu),
    ("drill_slalom", crate::game::shots_training::drill_slalom),
    ("drill_ring", crate::game::shots_training::drill_ring),
    ("drill_hurdles", crate::game::shots_training::drill_hurdles),
    ("drill_over", crate::game::shots_training::drill_over),
    ("training_result_sample", || Box::new(training::TrainingScreen::result_sample())),
    ("achievements", || Box::new(crate::ui::progress_ui::AchievementsScreen::new())),
    ("achievements_sample", || Box::new(crate::ui::progress_ui::AchievementsScreen::sample())),
    ("garage", || Box::new(garage::GarageScreen::new())),
    ("wardrobe", || Box::new(wardrobe::WardrobeScreen::new())),
    ("garage_sample", || Box::new(garage::GarageScreen::sample(4))),
    ("garage_locked_sample", || Box::new(garage::GarageScreen::locked_sample())),
    ("garage_buy_confirm_sample", || Box::new(garage::GarageScreen::buy_confirm_sample())),
    ("multiplayer", || Box::new(multiplayer::MultiplayerScreen::new())),
    ("lobby_host", || Box::new(lobby::LobbyScreen::open_host())),
    ("host_sample", || Box::new(lobby::LobbyScreen::host_sample())),
    ("lobby_battle_sample", || Box::new(lobby::LobbyScreen::battle_sample())),
    ("lobby_battle_guest_sample", || Box::new(lobby::LobbyScreen::battle_guest_sample())),
    ("lobby_sample", || Box::new(lobby::LobbyScreen::lobby_sample())),
    ("lobby_custom_sample", || Box::new(lobby::LobbyScreen::custom_host_sample())),
    ("lobby_custom_guest_sample", || Box::new(lobby::LobbyScreen::custom_guest_sample())),
    ("lobby_custom_downloading_sample", || Box::new(lobby::LobbyScreen::custom_downloading_sample())),
    ("join", || Box::new(join::JoinScreen::new())),
    ("join_sample", || Box::new(join::JoinScreen::sample())),
    ("join_failed_sample", || Box::new(join::JoinScreen::failed_sample())),
    ("join_keep_failed_sample", || Box::new(join::JoinScreen::failed_keep_sample())),
    ("multiplayer_keeps_sample", || Box::new(multiplayer::MultiplayerScreen::keeps_sample())),
    ("multiplayer_many_keeps_sample", || Box::new(multiplayer::MultiplayerScreen::many_keeps_sample())),
    ("nearby_sample", || Box::new(nearby::NearbyScreen::sample())),
    ("nearby_empty_sample", || Box::new(nearby::NearbyScreen::empty_sample())),
    ("online_battle_results_sample", || Box::new(online_results::OnlineResultsScreen::battle_sample())),
    ("online_results_sample", || Box::new(online_results::OnlineResultsScreen::sample())),
    ("settings", || Box::new(settings::SettingsScreen::new())),
    ("settings_tilt", || Box::new(settings::SettingsScreen::new())),
    ("settings_keypad", || Box::new(settings::SettingsScreen::keypad_sample())),
    ("settings_relays", || Box::new(settings::SettingsScreen::at_cards())),
    ("settings_relay_keypad", || Box::new(settings::SettingsScreen::relay_keypad_sample())),
    ("settings_updates_sample", || Box::new(settings::SettingsScreen::updates_sample(false))),
    ("settings_downloading_sample", || Box::new(settings::SettingsScreen::updates_sample(true))),
    ("more_games", || Box::new(games::GamesScreen::sample_get())),
    ("more_games_none_sample", || Box::new(games::GamesScreen::sample_none())),
    ("more_games_downloading_sample", || Box::new(games::GamesScreen::sample_downloading())),
    ("more_games_open_sample", || Box::new(games::GamesScreen::sample_open())),
    ("credits", || Box::new(credits::CreditsScreen::new())),
    ("results_ghost_sample", || Box::new(after_race::ResultsScreen::new(after_race::ghost_sample_outcome()))),
    ("results_sats_sample", || Box::new(after_race::ResultsScreen::sats_sample())),
    ("results_sample", || Box::new(after_race::ResultsScreen::new(after_race::sample_outcome(after_race::SampleKind::Quick)))),
    ("results_win_sample", || Box::new(after_race::ResultsScreen::new(after_race::sample_outcome(after_race::SampleKind::QuickWin)))),
    ("gp_standings_sample", || Box::new(after_race::GpScreen::sample(1))),
    ("gp_final_sample", || Box::new(after_race::GpScreen::sample(3))),
    ("gp_trophy_sample", || Box::new(after_race::GpScreen::trophy_sample(false))),
    ("gp_trophy_win_sample", || Box::new(after_race::GpScreen::trophy_sample(true))),
    // Gameplay shot/dev entries (the bot drives the player; see game/shots.rs).
    ("race_dune", crate::game::shots::race_dune),
    ("race_savanna", crate::game::shots::race_savanna),
    ("race_canyon", crate::game::shots::race_canyon),
    ("race_midnight", crate::game::shots::race_midnight),
    ("race_baobab", crate::game::shots::race_baobab),
    ("race_volcano", crate::game::shots::race_volcano),
    ("race_oasis", crate::game::shots::race_oasis),
    ("race_halving", crate::game::shots::race_halving),
    ("race_eight_under", crate::game::shots::race_eight_under),
    ("race_eight_deck", crate::game::shots::race_eight_deck),
    ("paddock", crate::game::shots::paddock),
    ("stage_trk_eight", crate::game::shots::trk_eight),
    ("stage_trk_paddock", crate::game::shots::trk_paddock),
    ("stage_trk_savanna", crate::game::shots::trk_savanna),
    ("stage_trk_dune", crate::game::shots::trk_dune),
    ("stage_trk_canyon", crate::game::shots::trk_canyon),
    ("stage_trk_midnight", crate::game::shots::trk_midnight),
    ("stage_trk_baobab", crate::game::shots::trk_baobab),
    ("stage_trk_volcano", crate::game::shots::trk_volcano),
    ("stage_trk_oasis", crate::game::shots::trk_oasis),
    ("stage_trk_halving", crate::game::shots::trk_halving),
    ("challenge_hud_coinrush", crate::game::shots::challenge_hud_coinrush),
    ("challenge_hud_egghunt", crate::game::shots::challenge_hud_egghunt),
    ("challenge_hud_elimination", crate::game::shots::challenge_hud_elimination),
    ("challenge_hud_drift", crate::game::shots::challenge_hud_drift),
    ("challenge_hud_bash", crate::game::shots::challenge_hud_bash),
    ("challenge_hud_turbo", crate::game::shots::challenge_hud_turbo),
    ("race_fp", crate::game::shots::race_fp),
    ("race_far", crate::game::shots::race_far),
    ("race_mirror", crate::game::shots::race_mirror),
    ("race_pause", crate::game::shots::race_pause),
    ("race_countdown", crate::game::shots::race_countdown),
    ("race_tilt", crate::game::shots::race_tilt),
    ("race_following", crate::game::shots::race_following),
    ("race_finish", crate::game::shots::race_finish),
    ("race_left", crate::game::shots::race_left),
    ("race_items", crate::game::shots::race_items),
    ("race_pill", crate::game::shots::race_pill),
    ("race_pill_timer", crate::game::shots::race_pill_timer),
    ("race_cake", crate::game::shots::race_cake),
    ("race_queue", crate::game::shots::race_queue),
    ("race_drift", crate::game::shots::race_drift),
    ("race_trump", crate::game::shots::race_trump),
    ("icons_dev", crate::game::shots::icons_dev),
    ("playful_fire", crate::game::shots_playful::fire),
    ("playful_laptop", crate::game::shots_playful::laptop),
    ("playful_cake", crate::game::shots_playful::cake),
    ("playful_splat", crate::game::shots_playful::splat),
    ("playful_frog", crate::game::shots_playful::frog),
    ("playful_eggs", crate::game::shots_playful::eggs),
    ("playful_pill", crate::game::shots_playful::pill),
    ("playful_printer", crate::game::shots_playful::printer),
    ("playful_chase", crate::game::shots_playful::chase),
    ("playful_rug", crate::game::shots_playful::rug),
    ("playful_rose", crate::game::shots_playful::rose),
    ("race_auto", crate::game::shots::race_auto),
    ("race_auto_cup2", crate::game::shots::race_auto_cup2),
    ("training_laser", crate::game::shots::training_laser),
    ("training_steer", crate::game::shots::training_steer),
    ("training_jump", crate::game::shots::training_jump),
    ("stage_attract", crate::game::shots::stage_attract),
    ("stage_garage", crate::game::shots::stage_garage),
    ("stage_podium", crate::game::shots::stage_podium),
];

/// A new screen by name (see [`SCREENS`]).
pub fn by_name(name: &str) -> Option<Box<dyn Screen>> {
    SCREENS.iter().filter(|(n, _)| !is_dev_only(n)).find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, f)| f())
}

/// Registry entries that open a live network room: developer builds only.
fn is_dev_only(name: &str) -> bool {
    name == "lobby_host" && !crate::dev::on()
}

/// The names in [`SCREENS`].
pub fn names() -> Vec<&'static str> {
    SCREENS.iter().map(|(n, _)| *n).filter(|n| !is_dev_only(n)).collect()
}

enum Fade {
    Idle,
    /// Fading to the background; the transition runs at the end.
    Out { t: f32, then: Transition },
    /// Fading back in.
    In { t: f32 },
}

/// Did a transition ask to leave the app?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Stay,
    Quit,
}

/// The stack of screens and the fade between them.
pub struct Router {
    stack: Vec<Box<dyn Screen>>,
    fade: Fade,
}

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Router {
    pub fn new(first: Box<dyn Screen>) -> Router {
        Router { stack: vec![first], fade: Fade::Idle }
    }

    pub fn top(&self) -> &dyn Screen {
        self.stack.last().expect("the router always holds a screen").as_ref()
    }

    pub fn top_mut(&mut self) -> &mut (dyn Screen + 'static) {
        self.stack.last_mut().expect("the router always holds a screen").as_mut()
    }

    /// Names of the stack, bottom first.
    pub fn names(&self) -> Vec<&'static str> {
        self.stack.iter().map(|s| s.name()).collect()
    }

    /// Does any screen on the stack show the online session?
    pub fn any_uses_online(&self) -> bool {
        self.stack.iter().any(|s| s.uses_online())
    }

    /// The topmost screen that shows the online session.
    pub fn nearest_online(&self) -> Option<&'static str> {
        self.stack.iter().rev().find(|s| s.uses_online()).map(|s| s.name())
    }

    /// Input is held back while the old screen fades out.
    pub fn input_blocked(&self) -> bool {
        matches!(self.fade, Fade::Out { .. })
    }

    /// Opacity (0..1) of the background-coloured sheet drawn over everything.
    pub fn fade_alpha(&self) -> f32 {
        match &self.fade {
            Fade::Idle => 0.0,
            Fade::Out { t, .. } => ease(t / FADE_OUT),
            Fade::In { t } => 1.0 - ease(t / FADE_IN),
        }
    }

    /// Ask for a transition. Fades start here and finish in [`Router::tick`];
    /// while one fades out, further requests are dropped.
    pub fn apply(&mut self, t: Transition) -> Applied {
        match t {
            Transition::None => Applied::Stay,
            // A fade-out in progress owns the next change.
            _ if matches!(self.fade, Fade::Out { .. }) => Applied::Stay,
            Transition::Cut(s) => {
                self.stack.pop();
                self.stack.push(s);
                self.fade = Fade::Idle;
                Applied::Stay
            }
            Transition::Pop if self.stack.len() <= 1 => Applied::Quit,
            Transition::Quit => Applied::Quit,
            other => {
                self.fade = Fade::Out { t: 0.0, then: other };
                Applied::Stay
            }
        }
    }

    /// Advance the fade by `dt`; at the bottom of a fade-out the pending
    /// transition happens.
    pub fn tick(&mut self, dt: f32) {
        match std::mem::replace(&mut self.fade, Fade::Idle) {
            Fade::Idle => {}
            Fade::Out { t, then } => {
                let t = t + dt;
                if t < FADE_OUT {
                    self.fade = Fade::Out { t, then };
                } else {
                    self.perform(then);
                    self.fade = Fade::In { t: 0.0 };
                }
            }
            Fade::In { t } => {
                let t = t + dt;
                if t < FADE_IN {
                    self.fade = Fade::In { t };
                }
            }
        }
    }

    fn perform(&mut self, t: Transition) {
        match t {
            Transition::Push(s) => self.stack.push(s),
            Transition::Replace(s) | Transition::Cut(s) => {
                self.stack.pop();
                self.stack.push(s);
            }
            Transition::Pop => {
                if self.stack.len() > 1 {
                    self.stack.pop();
                }
            }
            Transition::PopTo(name) => {
                if self.stack.iter().any(|s| s.name() == name) {
                    while self.stack.len() > 1 && self.stack.last().is_some_and(|s| s.name() != name) {
                        self.stack.pop();
                    }
                } else {
                    // No such screen under us (a link opened the join screen from
                    // the menu): leave the online screens and stop at the first other one.
                    while self.stack.len() > 1 && self.stack.last().is_some_and(|s| s.uses_online()) {
                        self.stack.pop();
                    }
                }
            }
            Transition::None | Transition::Quit => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);
    impl Screen for Named {
        fn name(&self) -> &'static str {
            self.0
        }
    }

    fn settle(r: &mut Router) {
        for _ in 0..60 {
            r.tick(1.0 / 60.0);
        }
    }

    #[test]
    fn push_pop_and_quit_on_the_first_screen() {
        let mut r = Router::new(Box::new(Named("a")));
        assert_eq!(r.apply(Transition::Push(Box::new(Named("b")))), Applied::Stay);
        assert!(r.input_blocked());
        assert_eq!(r.names(), ["a"], "the change waits for the fade");
        settle(&mut r);
        assert_eq!(r.names(), ["a", "b"]);
        assert_eq!(r.fade_alpha(), 0.0);
        assert_eq!(r.apply(Transition::Pop), Applied::Stay);
        settle(&mut r);
        assert_eq!(r.names(), ["a"]);
        assert_eq!(r.apply(Transition::Pop), Applied::Quit, "Back on the first screen leaves the app");
    }

    #[test]
    fn replace_cut_and_pop_to() {
        let mut r = Router::new(Box::new(Named("a")));
        r.apply(Transition::Cut(Box::new(Named("b"))));
        assert_eq!(r.names(), ["b"]);
        r.apply(Transition::Push(Box::new(Named("c"))));
        settle(&mut r);
        r.apply(Transition::Push(Box::new(Named("d"))));
        settle(&mut r);
        r.apply(Transition::PopTo("b"));
        settle(&mut r);
        assert_eq!(r.names(), ["b"]);
        r.apply(Transition::Replace(Box::new(Named("e"))));
        settle(&mut r);
        assert_eq!(r.names(), ["e"]);
    }

    #[test]
    fn fade_dips_and_returns() {
        let mut r = Router::new(Box::new(Named("a")));
        r.apply(Transition::Push(Box::new(Named("b"))));
        let mut peak = 0.0f32;
        for _ in 0..60 {
            r.tick(1.0 / 60.0);
            peak = peak.max(r.fade_alpha());
        }
        assert!(peak > 0.95, "fully dark at the swap, got {peak}");
        assert_eq!(r.fade_alpha(), 0.0);
    }

    #[test]
    fn registry_names() {
        for n in names() {
            // staged variants (`*_sample`, `settings_keypad`) report the screen they stage
            let got = by_name(n).unwrap().name();
            assert!(got == n || n.ends_with("_sample") || n.starts_with(got), "{n} reports itself as {got}");
        }
        assert!(by_name("nope").is_none());
    }
}
