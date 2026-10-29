//! Staged screens for `--shot` / `--screen`: a race (or the stage) with no
//! input, the bot driving the player. Registered in `ui::screens::SCREENS`.

use super::geometry;
use super::present::Presenter;
use super::race::{self, Staging};
use super::source::{Forced, LocalRace, RaceSource};
use super::stage::{self, WorldKey};
use crate::flow::{RaceKind, RaceLaunch};
use crate::platform::data_dir::CameraView;
use crate::ui::screens::{Ctx, Screen, Transition};
use super::ghosts::Playback;
use crate::platform::data_dir::Profile;
use race_core::{BattleRules, BotLevel, ChallengeId, Item, LessonId, RaceConfig, RacerKind, TrackId};

fn quick(name: &'static str, track: TrackId, st: Staging) -> Box<dyn Screen> {
    let cfg = RaceConfig::quick(track, 7, BotLevel::Normal, 5);
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Quick, local_seat: 0 }, cam_override(st)).named(name))
}

const AUTO: Staging = Staging { autopilot: Some(BotLevel::Hard), look_back: false, laser: false, drift: false, item_at: None, camera: None, paused: false, give: None, left_handed: false, mounted: false };

/// Dev: `STAMPEDE_CAM=chase|fp|far` forces the camera in any staged race shot.
fn cam_override(mut st: Staging) -> Staging {
    if st.camera.is_none() {
        st.camera = match crate::dev::var("STAMPEDE_CAM").as_deref() {
            Some("chase") => Some(CameraView::Chase),
            Some("fp") => Some(CameraView::FirstPerson),
            Some("far") => Some(CameraView::Far),
            _ => None,
        };
    }
    st
}

pub fn race_tilt() -> Box<dyn Screen> {
    quick("race_tilt", TrackId::Training, Staging { autopilot: None, ..AUTO })
}

/// A finished online player following the remaining racers, without sockets.
pub fn race_following() -> Box<dyn Screen> {
    struct WaitingRace { local: LocalRace, racers: Vec<race_core::Racer> }
    impl RaceSource for WaitingRace {
        fn view(&self) -> super::source::RaceView<'_> {
            let mut v = self.local.view();
            v.racers = &self.racers;
            v.prev = &self.racers;
            v.phase = race_core::RacePhase::Racing;
            v
        }
        fn local_seat(&self) -> Option<usize> { Some(0) }
        fn advance(&mut self, _: f32, _: &mut dyn FnMut() -> race_core::Input) -> u32 { 0 }
        fn drain_events(&mut self) -> Vec<race_core::Event> { Vec::new() }
        fn net_status(&self) -> Option<super::source::NetStatus> { Some(super::source::NetStatus { waiting: true, ..Default::default() }) }
        fn ready_to_finish(&self) -> bool { false }
        fn pauses_sim(&self) -> bool { false }
    }
    let mut cfg = RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 5);
    for (r, name) in cfg.racers.iter_mut().zip(["You", "Nia", "Moss", "Ash"]) { r.name = name.into(); }
    let mut local = LocalRace::new(cfg.clone(), Some(0));
    local.set_autopilot(Some(BotLevel::Normal), Forced::default());
    for _ in 0..1000 { local.advance(race_core::math::DT, &mut race_core::Input::default); }
    let mut racers = local.view().racers.to_vec();
    racers[0].finished_at = Some(50.0);
    racers[0].place = 1;
    for (i, r) in racers.iter_mut().enumerate().skip(1) { r.place = i as u8 + 1; }
    let launch = RaceLaunch { config: cfg, kind: RaceKind::Online, local_seat: 0 };
    Box::new(race::with_source(launch, Staging::default(), Box::new(WaitingRace { local, racers })).named("race_following"))
}
pub fn race_dune() -> Box<dyn Screen> {
    quick("race_dune", TrackId::Dune, AUTO)
}
pub fn race_savanna() -> Box<dyn Screen> {
    quick("race_savanna", TrackId::Savanna, AUTO)
}
pub fn race_canyon() -> Box<dyn Screen> {
    quick("race_canyon", TrackId::Canyon, AUTO)
}
pub fn race_midnight() -> Box<dyn Screen> {
    quick("race_midnight", TrackId::Midnight, AUTO)
}
pub fn race_fp() -> Box<dyn Screen> {
    quick("race_fp", TrackId::Savanna, Staging { camera: Some(CameraView::FirstPerson), ..AUTO })
}
pub fn race_far() -> Box<dyn Screen> {
    quick("race_far", TrackId::Savanna, Staging { camera: Some(CameraView::Far), ..AUTO })
}
pub fn race_mirror() -> Box<dyn Screen> {
    quick("race_mirror", TrackId::Dune, Staging { look_back: true, ..AUTO })
}
pub fn race_countdown() -> Box<dyn Screen> {
    quick("race_countdown", TrackId::Savanna, AUTO)
}
pub fn race_finish() -> Box<dyn Screen> {
    let mut cfg = RaceConfig::quick(TrackId::Training, 5, BotLevel::Normal, 9);
    cfg.laps = 1;
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Quick, local_seat: 0 }, AUTO).named("race_finish"))
}
pub fn race_left() -> Box<dyn Screen> {
    quick("race_left", TrackId::Midnight, Staging { left_handed: true, give: Some((Item::Feather, 3)), ..AUTO })
}
pub fn race_items() -> Box<dyn Screen> {
    quick("race_items", TrackId::Canyon, Staging { give: Some((Item::MegaLaser, 1)), ..AUTO })
}
pub fn race_pill() -> Box<dyn Screen> {
    quick("race_pill", TrackId::Savanna, Staging { give: Some((Item::OrangePill, 1)), ..AUTO })
}
/// The Orange Pill a moment after use: the radial timer over the item button.
pub fn race_pill_timer() -> Box<dyn Screen> {
    quick("race_pill_timer", TrackId::Savanna, Staging { give: Some((Item::OrangePill, 1)), item_at: Some(0.5), ..AUTO })
}
/// Shared Cake a moment after use: the others eating, and the "Cake!" line (`STAMPEDE_CAM=chase` for the chase view).
pub fn race_cake() -> Box<dyn Screen> {
    quick("race_cake", TrackId::Savanna, Staging { give: Some((Item::SharedCake, 1)), item_at: Some(0.5), ..AUTO })
}
/// The item button with three power-ups queued behind the held one ("+3"), as
/// after a Shared Cake has taken them (take it with `--t 4`, just after the start).
pub fn race_queue() -> Box<dyn Screen> {
    let cfg = RaceConfig::quick(TrackId::Savanna, 7, BotLevel::Normal, 5);
    let mut src = LocalRace::new(cfg.clone(), Some(0));
    for (it, n) in [(Item::Shield, 1), (Item::Feather, 3), (Item::Magnet, 1), (Item::OrangePill, 1)] {
        src.give_local_item(it, n);
    }
    src.set_autopilot(AUTO.autopilot, Forced::default());
    let launch = RaceLaunch { config: cfg, kind: RaceKind::Quick, local_seat: 0 };
    Box::new(race::with_source(launch, cam_override(AUTO), Box::new(src)).named("race_queue"))
}
/// The player drifting through the corners (legs in a wheel, dust plume).
pub fn race_drift() -> Box<dyn Screen> {
    quick("race_drift", TrackId::Midnight, Staging { drift: true, ..AUTO })
}
/// The player in seat 7 (the grid is random) fires a Laser Trump a second into the race, through the pack.
pub fn race_trump() -> Box<dyn Screen> {
    let cfg = RaceConfig::quick(TrackId::Savanna, 7, BotLevel::Normal, 5);
    let st = Staging { give: Some((Item::LaserTrump, 1)), item_at: Some(1.0), ..AUTO };
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Quick, local_seat: 7 }, cam_override(st)).named("race_trump"))
}
pub fn race_pause() -> Box<dyn Screen> {
    quick("race_pause", TrackId::Dune, Staging { paused: true, ..AUTO })
}
/// The Figure Eight with the bot at the wheel; the overpass views are timed with `--t`: about 8 s has the
/// camera under the deck on the lower diagonal, about 27 s has it on the deck (`STAMPEDE_CAM=far` for a wider view).
pub fn race_eight_under() -> Box<dyn Screen> {
    quick("race_eight_under", TrackId::Eight, AUTO)
}
pub fn race_eight_deck() -> Box<dyn Screen> {
    quick("race_eight_deck", TrackId::Eight, AUTO)
}
/// The player standing on the Paddock (no autopilot, no input).
pub fn paddock() -> Box<dyn Screen> {
    quick("paddock", TrackId::Field, Staging { autopilot: None, ..AUTO })
}
/// A full race with the bot at the wheel; ends in `after_race` by itself.
pub fn race_auto() -> Box<dyn Screen> {
    let mut cfg = RaceConfig::quick(TrackId::Training, 5, BotLevel::Normal, 9);
    cfg.laps = 1;
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Quick, local_seat: 0 }, AUTO).named("race_auto"))
}

/// A full Cup 2 race with the bot at the wheel (1 lap, Oasis Run); ends in the real results screen.
pub fn race_auto_cup2() -> Box<dyn Screen> {
    let mut cfg = RaceConfig::quick(TrackId::Oasis, 3, BotLevel::Normal, 9);
    cfg.laps = 1;
    cfg.racers[0].name = "Rider".into();
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Quick, local_seat: 0 }, AUTO).named("race_auto_cup2"))
}

fn lesson(name: &'static str, id: LessonId, st: Staging) -> Box<dyn Screen> {
    let cfg = RaceConfig::training(id, RacerKind::Human, 3);
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Training(id), local_seat: 0 }, cam_override(st)).named(name))
}
pub fn training_laser() -> Box<dyn Screen> {
    lesson("training_laser", LessonId::LaserEyes, Staging { laser: true, ..AUTO })
}
pub fn training_steer() -> Box<dyn Screen> {
    lesson("training_steer", LessonId::RunSteer, AUTO)
}
pub fn training_jump() -> Box<dyn Screen> {
    lesson("training_jump", LessonId::Jump, AUTO)
}

fn challenge(name: &'static str, id: ChallengeId, st: Staging) -> Box<dyn Screen> {
    let mut cfg = RaceConfig::challenge(id, 3, 7);
    cfg.racers[0].name = "Rider".into();
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Challenge(id), local_seat: 0 }, cam_override(st)).named(name))
}
pub fn challenge_hud_coinrush() -> Box<dyn Screen> {
    challenge("challenge_hud_coinrush", ChallengeId::CoinRush, AUTO)
}
pub fn challenge_hud_egghunt() -> Box<dyn Screen> {
    challenge("challenge_hud_egghunt", ChallengeId::EggHunt, AUTO)
}
pub fn challenge_hud_elimination() -> Box<dyn Screen> {
    challenge("challenge_hud_elimination", ChallengeId::Elimination, AUTO)
}
pub fn challenge_hud_drift() -> Box<dyn Screen> {
    challenge("challenge_hud_drift", ChallengeId::DriftKing, AUTO)
}
pub fn challenge_hud_bash() -> Box<dyn Screen> {
    challenge("challenge_hud_bash", ChallengeId::BashBrawl, AUTO)
}
pub fn challenge_hud_turbo() -> Box<dyn Screen> {
    challenge("challenge_hud_turbo", ChallengeId::TurboTimeAttack, AUTO)
}
pub fn challenge_hud_sniper() -> Box<dyn Screen> {
    challenge("challenge_hud_sniper", ChallengeId::LaserSniper, Staging { laser: true, ..AUTO })
}
pub fn challenge_hud_ramp() -> Box<dyn Screen> {
    challenge("challenge_hud_ramp", ChallengeId::RampRider, AUTO)
}
pub fn challenge_hud_night() -> Box<dyn Screen> {
    challenge("challenge_hud_night", ChallengeId::NightCoinRush, AUTO)
}
pub fn challenge_hud_lastfeather() -> Box<dyn Screen> {
    challenge("challenge_hud_lastfeather", ChallengeId::LastFeather, AUTO)
}

/// Mid-battle on Dust Bowl against seven bots with the autopilot driving: feathers have been lost by `--t 30`.
pub fn battle_hud_sample() -> Box<dyn Screen> {
    let seed = crate::dev::var("STAMPEDE_SEED").and_then(|v| v.parse().ok()).unwrap_or(3);
    let arena = if crate::dev::var("STAMPEDE_ARENA").as_deref() == Some("plaza") { TrackId::MempoolPlaza } else { TrackId::DustBowl };
    let mut cfg = RaceConfig::battle_bots(arena, 8, BotLevel::Normal, BattleRules::default(), seed);
    cfg.racers[0].kind = RacerKind::Human;
    cfg.racers[0].name = "Rider".into();
    cfg.racers[0].character = 2;
    let mut p = Profile::default();
    p.character = 2;
    p.name = "Rider".into();
    crate::ui::screens::common::seat_player(&mut cfg, &p);
    Box::new(race::new(RaceLaunch { config: cfg, kind: RaceKind::Battle, local_seat: 0 }, cam_override(AUTO)).named("battle_hud_sample"))
}

/// A bot's recorded Time Trial (for the ghost shot and tests).
pub fn bot_ghost(track: TrackId, laps: u8, level: BotLevel, seed: u64) -> Option<race_core::Ghost> {
    use super::source::{Forced, LocalRace, RaceSource};
    let mut cfg = RaceConfig::quick(track, 0, BotLevel::Normal, seed);
    cfg.laps = laps;
    cfg.mode = race_core::RaceMode::TimeTrial;
    let mut race = LocalRace::new(cfg, Some(0));
    race.set_autopilot(Some(level), Forced::default());
    race.record_ghost(true);
    for _ in 0..(60 * 600) {
        race.advance(1.0 / 60.0, &mut race_core::Input::default);
        if let Some(g) = race.take_ghost() {
            return Some(g);
        }
    }
    None
}

/// Time Trial on Savanna with a (faster) bot's ghost ahead: the split pill and the GHOST tag.
pub fn time_trial_ghost_sample() -> Box<dyn Screen> {
    let track = TrackId::Savanna;
    let mut cfg = RaceConfig::quick(track, 0, BotLevel::Normal, 11);
    cfg.laps = 1;
    cfg.mode = race_core::RaceMode::TimeTrial;
    cfg.racers[0].name = "Rider".into();
    let mut screen = race::new(RaceLaunch { config: cfg, kind: RaceKind::TimeTrial, local_seat: 0 }, cam_override(Staging { autopilot: Some(BotLevel::Easy), ..AUTO })).named("time_trial_ghost_sample");
    if let Some(g) = bot_ghost(track, 1, BotLevel::Hard, 11) {
        screen = screen.with_ghost(Playback::new(g, &track.track()));
    }
    Box::new(screen)
}

/// A Cup 2 race for the track shots.
pub fn race_cup2(id: TrackId, name: &'static str) -> Box<dyn Screen> {
    quick(name, id, AUTO)
}
pub fn race_baobab() -> Box<dyn Screen> {
    race_cup2(TrackId::Baobab, "race_baobab")
}
pub fn race_volcano() -> Box<dyn Screen> {
    race_cup2(TrackId::Volcano, "race_volcano")
}
pub fn race_oasis() -> Box<dyn Screen> {
    race_cup2(TrackId::Oasis, "race_oasis")
}
pub fn race_halving() -> Box<dyn Screen> {
    race_cup2(TrackId::Halving, "race_halving")
}

/// A bare stage screen: requests the stage every frame (for the stage shots).
pub struct StageShot {
    mode: u8,
}

impl StageShot {
    fn new(mode: u8) -> StageShot {
        match mode {
            // Run the stage clock ahead: a headless shot paints only the last frame.
            0 => stage::fast_forward(9.0),
            1 => stage::fast_forward(2.2),
            _ => stage::fast_forward(3.0),
        }
        StageShot { mode }
    }
}

impl Screen for StageShot {
    fn name(&self) -> &'static str {
        match self.mode {
            0 => "stage_attract",
            1 => "stage_garage",
            _ => "stage_podium",
        }
    }
    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        stage::set_quality(ctx.profile.quality);
        match self.mode {
            0 => stage::show_attract(TrackId::Savanna),
            1 => stage::show_garage(6, 0.55 + ctx.time as f32 * 0.5),
            _ => stage::show_podium([Some(2), Some(0), Some(5)], TrackId::Dune),
        }
        Transition::None
    }
    fn paint_scene(&mut self, pass: &mut crate::render::ScenePass) {
        stage::paint(pass, 0.0);
    }
}

pub fn stage_attract() -> Box<dyn Screen> {
    Box::new(StageShot::new(0))
}
pub fn stage_garage() -> Box<dyn Screen> {
    Box::new(StageShot::new(1))
}
pub fn stage_podium() -> Box<dyn Screen> {
    Box::new(StageShot::new(2))
}

/// Every item icon on a sheet (dev check of the procedural art).
pub struct IconSheet;

impl Screen for IconSheet {
    fn name(&self) -> &'static str {
        "icons_dev"
    }
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let p = ui.layer_painter(egui::LayerId::background());
        p.rect_filled(ctx.viewport.screen, 0.0, egui::Color32::from_rgb(40, 22, 66));
        let s = ctx.viewport.safe;
        for (i, it) in Item::ALL.iter().enumerate() {
            let columns = 7;
            let c = egui::pos2(
                s.left() + s.width() * ((i % columns) as f32 + 0.5) / columns as f32,
                s.center().y - 55.0 + (i / columns) as f32 * 120.0,
            );
            p.circle_filled(c, 38.0, egui::Color32::from_black_alpha(120));
            super::icons::item(&p, *it, c, 28.0, ctx.time as f32);
            p.text(c + egui::vec2(0.0, 52.0), egui::Align2::CENTER_CENTER, super::icons::item_label(*it), crate::ui::theme::numbers(10.0), egui::Color32::WHITE);
        }
        super::icons::bitcoin(&p, egui::pos2(s.center().x, s.center().y - 90.0), 24.0);
        Transition::None
    }
}

pub fn icons_dev() -> Box<dyn Screen> {
    Box::new(IconSheet)
}

// ---------------------------------------------------------------- track views ----

/// A HUD-free look at a track for visual checks: `--shot stage_trk_<track>` with `STAMPEDE_VIEW=chase|grid|over|side|mill`
/// (chase by default) and `--t` seconds of race (the grid view wants about 1.5, so the start lights are mid-count).
/// A bot drives the player; it logs the renderer's draw calls and triangles for the frame.
pub struct TrackView {
    name: &'static str,
    id: TrackId,
    view: String,
    race: LocalRace,
    pres: Presenter,
    scene: Option<race_render::FrameScene>,
    printed: bool,
}

impl TrackView {
    fn new(name: &'static str, id: TrackId) -> TrackView {
        let cfg = RaceConfig::quick(id, 7, BotLevel::Normal, 5);
        let mut race = LocalRace::new(cfg, Some(0));
        race.set_autopilot(Some(BotLevel::Hard), Forced::default());
        let view = crate::dev::var("STAMPEDE_VIEW").unwrap_or_else(|| "chase".into());
        TrackView { name, id, view, race, pres: Presenter::new(), scene: None, printed: false }
    }
}

impl Screen for TrackView {
    fn name(&self) -> &'static str {
        self.name
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        use glam::Vec3;
        use race_core::math::forward;
        stage::set_quality(ctx.profile.quality);
        let dt = ctx.dt;
        self.race.advance(dt, &mut race_core::Input::default);
        let events = self.race.drain_events();
        let v = self.race.view();
        let mut poses = self.pres.poses(&v, dt, None);
        let mirror_on = crate::dev::var_set("STAMPEDE_MIRROR");
        if mirror_on {
            poses[0].look_back = 1.0;
        }
        let tr = v.track.clone();
        let first = (0..poses.len()).min_by_key(|&i| v.racers[i].place).unwrap_or(0);
        let me = poses[0];
        let (eye, target, fov, fog, local): (Vec3, Vec3, f32, f32, usize) = match self.view.as_str() {
            "grid" => {
                let f = forward(me.yaw);
                (me.pos - f * 6.5 + Vec3::Y * 2.7, me.pos + f * 24.0 + Vec3::Y * 3.2, 66.0, 1.0, 0)
            }
            "over" => {
                let p = poses[first];
                let f = forward(p.yaw);
                let r = Vec3::new(f.z, 0.0, -f.x);
                (p.pos - f * 60.0 + Vec3::Y * 44.0 + r * 22.0, p.pos + f * 45.0, 62.0, 0.3, usize::MAX)
            }
            // The Paddock's centre island (windmill and hay bales) seen from outside its fence.
            "mill" => (Vec3::new(30.0, 6.5, 34.0), Vec3::new(0.0, 6.0, 0.0), 62.0, 0.8, usize::MAX),
            "side" => {
                let p = poses[first];
                let s = v.racers[first].s + 34.0;
                let sm = tr.sample_at(s);
                let lat = sm.width * 0.5 + sm.shoulder_r + 8.0;
                let mut e = tr.point_at(s, lat) + Vec3::Y * 2.4;
                e.y = e.y.max(tr.terrain_height(e.x, e.z) + 1.6);
                (e, p.pos + Vec3::Y * 1.4, 50.0, 0.8, usize::MAX)
            }
            _ => {
                let f = forward(me.yaw);
                (me.pos - f * 5.0 + Vec3::Y * 2.1, me.pos + f * 2.5 + Vec3::Y * 1.9, 72.0, 1.0, 0)
            }
        };
        let cam = race_render::Camera { eye, target, up: Vec3::Y, fov_y: fov.to_radians(), near: 0.3, far: 700.0 };
        let mirror = mirror_on.then(|| race_render::MirrorView { camera: super::camera::mirror_camera(&poses[0]), rect_px: [880.0, 20.0, 640.0, 270.0], scale: 1.0 });
        let scene = self.pres.scene(&v, local, poses, &events, cam, Vec3::ZERO, mirror, fog);
        if stage::needs_history() {
            stage::push_history(&scene);
        }
        self.scene = Some(scene);
        Transition::None
    }

    fn paint_scene(&mut self, pass: &mut crate::render::ScenePass) {
        let Some(scene) = self.scene.as_ref() else { return };
        let id = self.id;
        let course = geometry::Course::of(id, race_core::RaceMode::Race);
        stage::draw(pass, WorldKey::Track(id, course), || geometry::cached_course(id, course), scene, 1.0 / 60.0);
        if !self.printed {
            if let Some(st) = stage::stats() {
                if st.draw_calls > 0 {
                    log::info!("trackview {}: {} draws, {} tris, {} particles", self.name, st.draw_calls, st.triangles, st.particles);
                    self.printed = true;
                }
            }
        }
    }
}

macro_rules! track_views {
    ($($f:ident => $name:literal, $id:ident;)*) => {
        $(pub fn $f() -> Box<dyn Screen> { Box::new(TrackView::new($name, TrackId::$id)) })*
    };
}
track_views! {
    trk_savanna => "stage_trk_savanna", Savanna;
    trk_dune => "stage_trk_dune", Dune;
    trk_canyon => "stage_trk_canyon", Canyon;
    trk_midnight => "stage_trk_midnight", Midnight;
    trk_baobab => "stage_trk_baobab", Baobab;
    trk_volcano => "stage_trk_volcano", Volcano;
    trk_oasis => "stage_trk_oasis", Oasis;
    trk_halving => "stage_trk_halving", Halving;
    trk_eight => "stage_trk_eight", Eight;
    trk_paddock => "stage_trk_paddock", Field;
}
