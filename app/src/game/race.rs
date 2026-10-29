//! The race screen: runs the sim through a [`RaceSource`], draws it in 3D via
//! the shared stage renderer, and layers controls, HUD, pause and the finish.

use super::camera::{self, CameraRig, Follow};
use super::controls::{self, Adjacent, Config, Controller, Layout, Unstick};
use super::fx::{Fx, Reactions};
use super::geometry;
use super::icons::stroke;
use super::battle_fx;
use super::battle_hud::{self, BattleAnim};
use super::challenge_hud::{self, ChallengeHud, Pop};
use super::drill_hud::{self, CardAction, DrillAnim, ResultCard};
use super::ghost_hud::GhostHud;
use super::ghosts::{self, Playback};
use super::hud::{self, Banner, Countdown, Dot, LessonHud, MiniMap};
use super::outcome;
use super::present::Presenter;
use super::source::{Forced, LocalRace, RaceSource};
use super::spectate;
use super::stage::{self, WorldKey};
use crate::flow::{RaceKind, RaceLaunch};
use crate::platform::data_dir::{CameraView, Quality};
use crate::render::ScenePass;
use crate::ui::screens::after_race::after_race;
use crate::ui::screens::{Ctx, Screen, Transition};
use crate::ui::theme;
use egui::{pos2, vec2, Align2, Color32, Rect};
use glam::Vec3;
use race_core::{BotLevel, ChallengeId, DrillEvent, DrillId, Item, RacePhase};
use race_render::{Camera, FrameScene, MirrorView};

/// Test hooks for `--shot` / `--screen` entries: the bot drives the player.
#[derive(Clone, Copy, Debug, Default)]
pub struct Staging {
    pub autopilot: Option<BotLevel>,
    pub look_back: bool,
    pub laser: bool,
    /// Hold the drift button (the bot still steers).
    pub drift: bool,
    /// Fire the held item at this race time (seconds).
    pub item_at: Option<f32>,
    pub camera: Option<CameraView>,
    pub paused: bool,
    pub give: Option<(Item, u8)>,
    pub left_handed: bool,
    /// Start already mounted: no Le Mans run to the ostrich.
    pub mounted: bool,
}

struct End {
    t: f32,
    wait: f32,
}

/// The drill's results card, once the run is over and its prize is paid.
struct DrillEnd {
    age: f32,
    card: ResultCard,
}

/// How long the last pop-up (the strike that ended the run, the Master medal) plays before the card opens.
const DRILL_CARD_DELAY: f32 = 1.5;

struct Roulette {
    t: f32,
    last: i32,
    /// What the spin lands on (it may not be in the odds table, like Laser Trump).
    item: Item,
}

const ROULETTE_SECS: f32 = 0.95;
/// Mirror panel size in UI points (about 2.4:1 like the 384x160 pass).
const MIRROR_PT: (f32, f32) = (210.0, 88.0);

pub struct RaceScreen {
    launch: RaceLaunch,
    recording: bool,
    /// Hide Restart in the pause menu (a recording screen that is not Run a Track).
    no_restart: bool,
    staging: Staging,
    src: Box<dyn RaceSource>,
    seat: usize,
    pres: Presenter,
    rig: CameraRig,
    ctl: Controller,
    fx: Fx,
    minimap: MiniMap,
    // Frame products.
    scene: Option<FrameScene>,
    frame: Option<hud::Frame>,
    mirror_rect: Rect,
    frame_dt: f32,
    // State.
    inited: bool,
    paused: bool,
    end: Option<End>,
    done: bool,
    /// The final Grand Prix round's cup result, saved the moment the race ended.
    gp_recorded: Option<crate::flow::GpRecorded>,
    elapsed: f32,
    // HUD animation.
    banner: Option<Banner>,
    queue: Vec<Banner>,
    go_t: Option<f32>,
    final_lap_t: f32,
    place_pulse: f32,
    place_gain: bool,
    last_place: u8,
    camera_flash: f32,
    fade_in: f32,
    roulette: Option<Roulette>,
    revved: bool,
    stat_t: f32,
    ms: [f32; 3],
    ms_n: u32,
    cam_view: CameraView,
    pause_selected_camera: bool,
    /// Online host: QUIT was tapped once and asks "end the race for everyone?".
    quit_ask: bool,
    staged_pause: bool,
    unstick: Unstick,
    name: &'static str,
    // Challenges (SPEC 16).
    challenge: Option<ChallengeId>,
    pops: Vec<Pop>,
    score_pulse: f32,
    last_score: f32,
    last_medal: u8,
    last_cam: Option<Camera>,
    demo_n: u32,
    pop_seq: u32,
    last_misses: u32,
    last_tricks: u32,
    /// Laser Sniper: 1 right after a miss penalty, falling to 0.
    miss_flash: f32,
    // Battle (SPEC 17.1).
    battle_anim: BattleAnim,
    /// The local seat, a surviving battle racer, or a chosen unfinished online racer.
    watch: usize,
    spectating: bool,
    /// Time speed-up while spectating an offline battle.
    spec_speed: f32,
    // Open-ground drills (SPEC 18.2).
    drill: Option<DrillId>,
    drill_anim: DrillAnim,
    /// The run is over; seconds since.
    drill_over_t: Option<f32>,
    drill_settled: bool,
    drill_end: Option<DrillEnd>,
    // Time Trial ghost (SPEC 17.2).
    ghost: Option<Playback>,
    ghost_split: Option<f32>,
    /// What the local player did (for achievements).
    stats: crate::flow::RaceStats,
    /// The host-migration chip, shown for a few seconds after it changes.
    mig_note: crate::ui::screens::migration::GameNote,
    mig_delay: crate::ui::screens::migration::BehindDelay,
}

pub fn new(mut launch: RaceLaunch, mut staging: Staging) -> RaceScreen {
    launch.config.le_mans = !staging.mounted && !matches!(launch.kind, RaceKind::Training(_) | RaceKind::Drill(_));
    // Developer builds (DIAG APK with the autopilot switch on): the bot drives local races too,
    // so a device run needs no thumbs. Lessons teach controls, so they stay manual.
    if staging.autopilot.is_none() && crate::dev::autopilot() && !matches!(launch.kind, RaceKind::Training(_) | RaceKind::Drill(_)) {
        staging.autopilot = Some(BotLevel::Normal);
    }
    let seat = launch.local_seat.min(launch.config.racers.len().saturating_sub(1));
    let mut src = LocalRace::new(launch.config.clone(), Some(seat));
    if let Some((it, n)) = staging.give {
        src.give_local_item(it, n);
    }
    src.set_autopilot(staging.autopilot, Forced { look_back: staging.look_back, laser: staging.laser, drift: staging.drift, item_at: staging.item_at });
    // Time Trial: every run is recorded; the best per track is kept when the run ends.
    src.record_ghost(matches!(launch.kind, RaceKind::TimeTrial));
    with_source(launch, staging, Box::new(src))
}

/// The race screen over any [`RaceSource`] (online races: `source_net`).
pub fn with_source(launch: RaceLaunch, staging: Staging, src: Box<dyn RaceSource>) -> RaceScreen {
    let seat = launch.local_seat.min(launch.config.racers.len().saturating_sub(1));
    let track = launch.config.track.track();
    let challenge = match launch.config.mode {
        race_core::RaceMode::Challenge(id) => Some(id),
        _ => None,
    };
    RaceScreen {
        challenge,
        drill: launch.config.drill,
        drill_anim: DrillAnim::default(),
        drill_over_t: None,
        drill_settled: false,
        drill_end: None,
        pops: Vec::new(),
        score_pulse: 0.0,
        last_score: 0.0,
        last_medal: 0,
        last_cam: None,
        demo_n: 0,
        pop_seq: 0,
        last_misses: 0,
        last_tricks: 0,
        miss_flash: 0.0,
        battle_anim: BattleAnim::new(),
        watch: seat,
        spectating: false,
        spec_speed: 1.0,
        ghost: None,
        ghost_split: None,
        stats: Default::default(),
        minimap: MiniMap::new(&track),
        launch,
        recording: false,
        no_restart: false,
        staging,
        src,
        seat,
        pres: Presenter::new(),
        rig: CameraRig::new(),
        ctl: Controller::new(),
        fx: Fx::new(),
        scene: None,
        frame: None,
        mirror_rect: Rect::from_min_size(pos2(0.0, 0.0), vec2(MIRROR_PT.0, MIRROR_PT.1)),
        frame_dt: 0.0,
        inited: false,
        paused: false,
        staged_pause: staging.paused,
        unstick: Unstick::default(),
        name: "race",
        end: None,
        done: false,
        gp_recorded: None,
        elapsed: 0.0,
        banner: None,
        queue: Vec::new(),
        go_t: None,
        final_lap_t: 0.0,
        place_pulse: 0.0,
        place_gain: true,
        last_place: 0,
        camera_flash: 0.0,
        fade_in: 0.0,
        roulette: None,
        revved: false,
        stat_t: 0.0,
        ms: [0.0; 3],
        ms_n: 0,
        cam_view: CameraView::Chase,
        pause_selected_camera: false,
        quit_ask: false,
        mig_note: Default::default(),
        mig_delay: Default::default(),
    }
}

fn c32(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}

impl RaceScreen {
    pub fn recording(mut self) -> Self {
        self.recording = true;
        self.staging.autopilot = None;
        self.src.set_autopilot(None, Forced::default());
        self
    }

    /// No Restart in the pause menu: for a recording screen whose restart is not Run a Track's.
    pub fn without_restart(mut self) -> Self {
        self.no_restart = true;
        self
    }

    pub fn record_view(&self) -> (Vec3, f32, RacePhase, bool) {
        let v = self.src.view();
        let r = &v.racers[self.seat];
        (r.pos, r.yaw, v.phase, self.paused)
    }

    pub fn record_hud(&mut self, props: Vec<race_render::DrillPropInst>) {
        if let Some(scene) = self.scene.as_mut() {
            scene.drill_props = props;
        }
    }

    pub fn stop_recording(&mut self) {
        self.fx.stop();
        self.done = true;
    }

    /// Race against this ghost (shot entries; the real flow loads it in `init`).
    pub fn with_ghost(mut self, pb: Playback) -> RaceScreen {
        self.ghost = Some(pb);
        self
    }

    /// Another registry name (shot entries).
    pub fn named(mut self, name: &'static str) -> RaceScreen {
        self.name = name;
        self
    }

    fn init(&mut self, ctx: &mut Ctx) {
        self.inited = true;
        stage::reset_governor();
        crate::sound::init(ctx.opts.shot.is_some());
        if self.staging.left_handed {
            ctx.profile.left_handed = true;
        }
        ctx.haptics.set_enabled(ctx.profile.haptics);
        let me = self.launch.config.racers.get(self.seat).map_or(ctx.profile.character, |r| r.character);
        self.src.set_attire(crate::ui::progress::attire(ctx.profile, me));
        self.fx.start(ctx.profile.music_volume, ctx.profile.effects_volume, ctx.muted);
        if matches!(self.launch.kind, RaceKind::TimeTrial) && ctx.profile.show_ghost && !self.recording && self.ghost.is_none() {
            let (track, laps) = (self.launch.config.track, self.launch.config.laps);
            let g = ghosts::load(ctx.data_dir, track, laps, self.src.view().tun);
            self.ghost = g.map(|g| Playback::new(g, &track.track()));
            log::info!("race: ghost {}", self.ghost.as_ref().map_or("none".to_string(), |p| format!("{:.2} s", p.time())));
        }
        log::info!("race: start {:?} {:?} laps {} racers {} seat {}", self.launch.config.track, self.launch.kind, self.launch.config.laps, self.launch.config.racers.len(), self.seat);
    }

    fn push_banner(&mut self, b: Banner) {
        if self.banner.is_none() {
            self.banner = Some(b);
        } else {
            self.queue.push(b);
        }
    }

    fn react(&mut self, re: &Reactions, place: u8, time: f32) {
        if re.shake > 0.0 {
            self.rig.add_shake(re.shake);
        }
        if re.snap_camera {
            self.rig.snap();
        }
        if re.respawned {
            self.fade_in = 0.45;
        }
        if re.go {
            self.go_t = Some(0.0);
        }
        if re.perfect_start {
            self.push_banner(Banner::new("PERFECT START!", Color32::from_rgb(120, 255, 150), 1.5));
        }
        if re.too_early {
            self.push_banner(Banner::new("TOO EARLY!", Color32::from_rgb(255, 100, 90), 1.2));
        }
        let laps_matter = !self.recording && self.challenge.is_none_or(|c| c.finishing());
        if re.final_lap && laps_matter {
            self.final_lap_t = 2.2;
        } else if let (Some(l), true) = (re.lap, laps_matter) {
            self.push_banner(Banner::new(format!("LAP {l}"), theme::TEXT, 1.0));
        }
        if let (Some(_), Some(_)) = (re.finished, self.challenge) {
            // A challenge says its own thing when it ends (challenge_react).
            self.end.get_or_insert(End { t: 0.0, wait: 3.4 });
        } else if let Some((p, t)) = re.finished {
            let (n, s) = hud::ordinal(p);
            let mut b = Banner::new("FINISH!", hud::place_color(p), 3.0);
            b.big = true;
            b.sub = Some(format!("{n}{} PLACE   {}", s.to_uppercase(), hud::fmt_time(t)));
            self.banner = Some(b);
            self.queue.clear();
            self.end.get_or_insert(End { t: 0.0, wait: 3.4 });
        }
        if let Some(stars) = re.lesson_done {
            let mut b = Banner::new("LESSON COMPLETE!", Color32::from_rgb(120, 255, 150), 3.0);
            b.big = true;
            b.sub = Some(hud::fmt_time(time));
            b.stars = Some(stars);
            self.banner = Some(b);
            self.queue.clear();
            self.end.get_or_insert(End { t: 0.0, wait: 3.2 });
        }
        if re.lesson_failed {
            let mut b = Banner::new("TIME UP", Color32::from_rgb(255, 100, 90), 2.6);
            b.big = true;
            b.sub = Some("Try again!".into());
            self.banner = Some(b);
            self.queue.clear();
            self.end.get_or_insert(End { t: 0.0, wait: 2.6 });
        }
        if let Some((it, _)) = re.item_got {
            self.roulette = Some(Roulette { t: 0.0, last: -1, item: it });
        }
        // A drill ends on its own results card, not on the sim's RaceOver.
        if re.race_over && self.drill.is_none() {
            self.end.get_or_insert(End { t: 0.0, wait: 1.2 });
        }
        let _ = place;
    }

    /// Challenge events: drift points, eliminations, the end (banner + medal).
    fn challenge_react(&mut self, re: &Reactions) {
        let Some(id) = self.challenge else { return };
        // Dev (shots): STAMPEDE_DEMO_POPS=1 feeds sample drift points so the popup layout can be checked.
        let demo: Vec<f32> = if crate::dev::var_set("STAMPEDE_DEMO_POPS") && id == ChallengeId::DriftKing {
            self.demo_n += 1;
            if self.demo_n % 20 == 1 { vec![[1.4, 4.2, 9.5, 2.1, 6.0][(self.demo_n / 20) as usize % 5]] } else { Vec::new() }
        } else {
            Vec::new()
        };
        for &p in re.drift_points.iter().chain(demo.iter()) {
            let (color, size) = if p >= 8.0 {
                (Color32::from_rgb(210, 140, 255), 38.0)
            } else if p >= 3.0 {
                (Color32::from_rgb(255, 170, 60), 32.0)
            } else {
                (Color32::from_rgb(150, 200, 255), 26.0)
            };
            self.pop_seq = self.pop_seq.wrapping_add(1);
            self.pops.push(Pop { text: format!("+{p:.1}"), age: 0.0, color, size, slot: (self.pop_seq % 3) as u8 });
            if self.pops.len() > 6 {
                self.pops.remove(0);
            }
        }
        // Laser Sniper: a miss costs seconds (red flash + "+2s"); Ramp Rider: tricks and chains count up.
        if let Some((misses, tricks)) = self.src.view().challenge.map(|c| (c.misses, c.progress)) {
            if misses > self.last_misses {
                let n = (misses - self.last_misses) as f32;
                match id {
                    ChallengeId::LaserSniper => {
                        self.miss_flash = 1.0;
                        self.pop_seq = self.pop_seq.wrapping_add(1);
                        self.pops.push(Pop { text: format!("+{:.0}s MISS", n * race_core::SNIPER_MISS_PENALTY), age: 0.0, color: Color32::from_rgb(255, 96, 84), size: 30.0, slot: (self.pop_seq % 3) as u8 });
                    }
                    ChallengeId::RampRider => {
                        self.pop_seq = self.pop_seq.wrapping_add(1);
                        self.pops.push(Pop { text: "CHAIN +2".into(), age: 0.0, color: Color32::from_rgb(255, 214, 80), size: 36.0, slot: 2 });
                    }
                    _ => {}
                }
            }
            if id == ChallengeId::RampRider && tricks > self.last_tricks {
                self.pops.push(Pop { text: "TRICK +1".into(), age: 0.0, color: Color32::from_rgb(150, 220, 255), size: 28.0, slot: 0 });
            }
            if self.pops.len() > 6 {
                self.pops.remove(0);
            }
            self.last_misses = misses;
            self.last_tricks = tricks;
        }
        let (names, alive, cs): (Vec<String>, usize, Option<(f32, u8)>) = {
            let v = self.src.view();
            (v.cfg.racers.iter().map(|r| r.name.to_uppercase()).collect(), v.racers.len().saturating_sub(v.challenge.map_or(0, |c| c.eliminated.len())), v.challenge.map(|c| (c.score, c.medal)))
        };
        let mut player_out = false;
        for &(racer, place) in &re.eliminated {
            if racer as usize == self.seat {
                player_out = true;
                let (n, s) = hud::ordinal(place);
                let medal = cs.map(|c| c.1).unwrap_or(0);
                let mut b = Banner::new("YOU'RE OUT!", Color32::from_rgb(255, 100, 90), 3.0);
                b.big = true;
                b.sub = Some(if medal > 0 { format!("{n}{} PLACE  -  {} MEDAL", s.to_uppercase(), medal_word(medal)) } else { format!("{n}{} PLACE", s.to_uppercase()) });
                self.banner = Some(b);
                self.queue.clear();
            } else {
                let name = names.get(racer as usize).cloned().unwrap_or_default();
                let mut b = Banner::new(format!("ELIMINATED: {name}"), Color32::from_rgb(255, 150, 70), 2.2);
                b.sub = Some(format!("{alive} LEFT"));
                self.push_banner(b);
            }
        }
        if let Some(medal) = re.challenge_done {
            self.end.get_or_insert(End { t: 0.0, wait: 3.4 });
            if !player_out {
                let score = cs.map(|c| c.0).unwrap_or(0.0);
                let mut b = if medal > 0 { Banner::new(format!("{} MEDAL!", medal_word(medal)), crate::ui::screens::challenges::medal_colors(medal).0, 3.2) } else { Banner::new("NO MEDAL", theme::TEXT, 3.0) };
                b.big = true;
                b.sub = Some(challenge_hud::score_text(id, score));
                self.banner = Some(b);
                self.queue.clear();
            }
        }
    }

    /// Battle events: outs, stolen feathers, the end. (Last Feather's own end is `challenge_react`'s.)
    fn battle_react(&mut self, re: &Reactions) {
        let (names, winner): (Vec<String>, Option<usize>) = {
            let v = self.src.view();
            let Some(b) = v.battle else { return };
            (v.cfg.racers.iter().map(|r| r.name.to_uppercase()).collect(), b.ranking().first().copied())
        };
        let name = |i: u8| names.get(i as usize).cloned().unwrap_or_default();
        let last_feather = self.challenge.is_some();
        for &(racer, place) in &re.battle_out {
            if racer as usize == self.seat {
                if !last_feather {
                    let (n, s) = hud::ordinal(place);
                    let mut b = Banner::new("YOU'RE OUT!", Color32::from_rgb(255, 100, 90), 3.0);
                    b.big = true;
                    b.sub = Some(format!("{n}{} PLACE", s.to_uppercase()));
                    self.banner = Some(b);
                    self.queue.clear();
                }
            } else {
                let mut b = Banner::new(format!("OUT: {}", name(racer)), Color32::from_rgb(255, 150, 70), 2.0);
                // `place` is the number still in just before this racer went out
                // (the reliable event's own data, right even when several go out
                // in one frame or the view's snapshot lags).
                b.sub = Some(format!("{} LEFT", place.saturating_sub(1)));
                self.push_banner(b);
            }
        }
        for &(victim, by) in &re.feather_lost {
            if victim as usize == self.seat {
                let text = match by {
                    Some(a) if a as usize != self.seat => format!("-1  HIT BY {}", name(a)),
                    _ => "-1 FEATHER".to_string(),
                };
                self.battle_anim.popup(text, Color32::from_rgb(255, 110, 100));
            } else if by.is_some_and(|a| a as usize == self.seat) {
                self.battle_anim.popup(format!("HIT {}", name(victim)), Color32::from_rgb(255, 214, 100));
            }
        }
        for &(thief, from) in &re.feather_stolen {
            if thief as usize == self.seat {
                self.battle_anim.popup(format!("+1  STOLE FROM {}", name(from)), Color32::from_rgb(120, 240, 150));
            } else if from as usize == self.seat {
                self.battle_anim.popup(format!("{} STOLE A FEATHER", name(thief)), Color32::from_rgb(255, 110, 100));
            }
        }
        if re.battle_over && !last_feather {
            let me_won = winner == Some(self.seat);
            let mut b = if me_won {
                Banner::new("YOU WIN!", Color32::from_rgb(255, 208, 64), 3.4)
            } else {
                Banner::new(format!("{} WINS", winner.map(|w| name(w as u8)).unwrap_or_default()), Color32::from_rgb(255, 208, 64), 3.4)
            };
            b.big = true;
            b.sub = Some("BATTLE OVER".into());
            self.banner = Some(b);
            self.queue.clear();
            self.end.get_or_insert(End { t: 0.0, wait: 3.6 });
        }
    }

    /// The final round of a local Grand Prix has just ended: the placings are
    /// final, so save the cup result now rather than when the standings screen
    /// opens a few seconds later (the game may be paused, backgrounded or killed
    /// in between). The outcome carries a flag so the standings do not repeat it.
    fn record_cup(&mut self, ctx: &mut Ctx) {
        if self.recording || self.src.net_status().is_some() || !matches!(self.launch.kind, RaceKind::GrandPrix { .. }) {
            return;
        }
        let o = outcome::build(&self.launch, &self.src.view(), false);
        self.gp_recorded = crate::ui::screens::after_race::record_final_round(ctx, &self.launch, &o.results);
    }

    /// A Grand Prix racer who has finished cannot restart the round.
    fn restart_locked(&self) -> bool {
        self.end.is_some() && matches!(self.launch.kind, RaceKind::GrandPrix { .. })
    }

    fn pause(&mut self, on: bool) {
        if self.paused != on {
            self.paused = on;
            self.quit_ask = false;
            self.ctl.release_all();
            self.fx.set_paused(on);
        }
    }

    fn finish(&mut self, ctx: &mut Ctx, quit: bool) -> Transition {
        // Across the line already: the result stands, whatever Quit was tapped in the
        // wait for the results (it must not throw the standings, and the cup, away).
        // Local races only: online, Quit means leave the room (or end it for everyone),
        // and the source's exit_transition does that.
        let quit = quit && !(self.src.net_status().is_none() && self.src.view().racers.get(self.seat).is_some_and(|r| r.finished_at.is_some()));
        if self.recording {
            self.stop_recording();
            return Transition::Pop;
        }
        // Online: the source decides where leaving goes (results with the session, or the menu).
        if let Some(t) = self.src.exit_transition(ctx, quit) {
            if !quit {
                crate::ui::progress::stash_stats(self.stats);
            }
            log::info!("race: online exit (quit {quit})");
            self.fx.stop();
            self.done = true;
            return t;
        }
        let mut o = outcome::build(&self.launch, &self.src.view(), quit);
        o.stats = self.stats;
        o.gp_recorded = self.gp_recorded;
        // Time Trial: keep this run as the ghost if it is the best one for the track.
        if !quit && matches!(self.launch.kind, RaceKind::TimeTrial) {
            if let Some(g) = self.src.take_ghost() {
                let raced = self.ghost.as_ref().map(|p| p.time());
                let res = ghosts::finish_run(ctx.data_dir, self.launch.config.laps, &g, raced);
                log::info!("race: ghost {:.2} s saved {} (raced {:?})", res.time, res.saved, raced);
                o.ghost = Some(res);
            }
        }
        log::info!(
            "race: over (quit {quit}) -> after_race: local place {:?}, {} results, lesson {:?}",
            o.results.iter().find(|r| r.is_local).map(|r| r.place),
            o.results.len(),
            o.lesson.as_ref().map(|l| (l.done, l.stars))
        );
        self.fx.stop();
        self.done = true;
        after_race(o)
    }
}

impl Screen for RaceScreen {
    fn orientation(&self) -> crate::platform::os::Orientation {
        crate::platform::os::Orientation::Landscape
    }

    fn uses_online(&self) -> bool {
        self.src.net_status().is_some()
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        let t0 = std::time::Instant::now();
        let t = self.update_inner(ctx);
        self.ms[0] += t0.elapsed().as_secs_f32() * 1000.0;
        t
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let t0 = std::time::Instant::now();
        let t = self.draw_inner(ctx, ui);
        self.ms[1] += t0.elapsed().as_secs_f32() * 1000.0;
        t
    }

    fn paint_scene(&mut self, pass: &mut ScenePass) {
        let t0 = std::time::Instant::now();
        self.paint_inner(pass);
        self.ms[2] += t0.elapsed().as_secs_f32() * 1000.0;
        self.ms_n += 1;
    }

    fn clear_color(&self, _ctx: &Ctx) -> [f32; 4] {
        let c = self.launch.config.track.track().theme.fog;
        [c[0], c[1], c[2], 1.0]
    }

    fn keep_awake(&self) -> bool {
        true
    }

    fn uses_tilt(&self) -> bool { !self.paused && !self.done && !self.spectating && self.staging.autopilot.is_none() }

    fn name(&self) -> &'static str {
        self.name
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        if self.drill_end.is_some() && !self.done {
            // System Back on the results card is its Back button.
            return self.finish(ctx, false);
        }
        if !self.done {
            let p = !self.paused;
            self.pause(p);
        }
        Transition::None
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        // An autopilot run keeps going when the window is unfocused (CI, smoke tests).
        if !self.done && self.staging.autopilot.is_none() {
            self.pause(true);
        }
    }
}

impl RaceScreen {
    fn update_inner(&mut self, ctx: &mut Ctx) -> Transition {
        if !self.inited {
            self.init(ctx);
        }
        if self.done {
            return Transition::None;
        }
        if self.staged_pause && self.elapsed >= 4.0 {
            self.staged_pause = false;
            self.pause(true);
        }
        if self.src.net_status().is_some() {
            // A join link while racing online: not now (it would fire long after).
            if let Some(target) = ctx.take_join_code() {
                match target {
                    crate::keeps::JoinTarget::BadInvite => ctx.say(crate::keeps::NOT_AN_INVITE),
                    target => ctx.say(format!("Finish or leave this race to join {}", target.label())),
                }
            }
        }
        stage::set_quality(ctx.profile.quality);
        self.fx.apply_volumes(ctx.profile.music_volume, ctx.profile.effects_volume, ctx.muted);
        let dt = ctx.dt;
        let layout = Layout::new(
            ctx.viewport.safe,
            ctx.profile.left_handed,
            ctx.profile.steer_mode,
            ctx.profile.auto_accelerate,
        );
        let layout = if self.src.view().tun.practice_lasers { layout } else { layout.without_laser() };
        let layout = if self.src.view().racers[self.seat].effects.dismount > 0.0 {
            layout.running()
        } else {
            layout
        };
        self.cam_view = self.staging.camera.unwrap_or(ctx.profile.camera);

        // Online, the room keeps racing under the pause menu: keep feeding the source.
        let live_pause = self.paused && !self.src.pauses_sim();
        if self.paused && !live_pause {
            self.frame_dt = 0.0;
            // Keep the HUD fresh for layout changes but do not run anything.
            if let Some(f) = self.frame.as_mut() {
                f.layout = layout;
            }
            return Transition::None;
        }
        self.frame_dt = dt;
        self.elapsed += dt;

        // ---- controls ----
        let (cd_left, window) = {
            let v = self.src.view();
            ((v.phase == RacePhase::Countdown).then_some(v.countdown_left), (v.tun.start_boost_window - 0.02))
        };
        let adj = {
            let v = self.src.view();
            adjacency(&v, self.seat)
        };
        let cfg = Config { steer_mode: ctx.profile.steer_mode, auto_accel: ctx.profile.auto_accelerate };
        let watching = !spectate::candidates(&self.src.view(), self.seat, self.src.net_status().is_some_and(|n| n.waiting)).is_empty();
        let mut want_pause = false;
        if self.staging.autopilot.is_none() && !live_pause {
            self.ctl.set_tilt(ctx.tilt.steer());
            let out = self.ctl.update(dt, ctx.time, ctx.touches, ctx.keys, &layout, cfg, adj, cd_left, window);
            if out.centre && !watching {
                let centred = ctx.tilt.centre();
                ctx.say(if centred { "Steering centred" } else { "Waiting for motion sensor — the pad still works" });
            }
            if out.camera {
                let next = camera::next_view(ctx.profile.camera);
                ctx.profile.camera = next;
                self.cam_view = next;
                self.camera_flash = 1.6;
                self.rig.snap();
            }
            want_pause = out.pause;
            if cd_left.is_some() {
                self.revved = self.ctl.revved();
            }
        }
        if want_pause && self.drill_end.is_none() {
            self.pause(true);
            return Transition::None;
        }

        // ---- the sim ----
        self.src.set_hands_off(live_pause);
        // Knocked out of an offline battle: the rest plays out at speed while the camera follows the leader.
        let sim_k = {
            let v = self.src.view();
            let out = v.battle.is_some_and(|b| b.is_out(self.seat) && !b.done) && self.challenge.is_none();
            if out && self.src.pauses_sim() {
                4.0
            } else {
                1.0
            }
        };
        self.spec_speed = sim_k;
        {
            let ctl = &mut self.ctl;
            // sim_k runs the same frame several times over, so the speed-up holds at any frame rate
            for _ in 0..sim_k as usize {
                self.src.advance(dt, &mut || {
                    if live_pause || watching {
                        return race_core::Input::default();
                    }
                    ctl.next_tick_input()
                });
            }
        }
        let mut events = self.src.drain_events();
        if self.recording {
            events.retain(|e| !matches!(e, race_core::Event::Lap { .. } | race_core::Event::FinalLap { .. } | race_core::Event::WrongWay { .. }));
        }
        self.stats.absorb(&events, self.seat);
        if self.src.net_status().is_some_and(|n| n.lost.is_some()) && self.end.is_none() {
            // Online and the host is gone: the source says why and returns to the menu.
            return self.finish(ctx, false);
        }
        {
            let v = self.src.view();
            let r = &v.racers[self.seat];
            let parked = v
                .challenge
                .is_some_and(|c| c.done || c.eliminated.contains(&self.seat))
                || v.battle.is_some_and(|b| b.is_out(self.seat));
            let ok = v.phase == RacePhase::Racing
                && r.stun_t <= 0.0
                && r.respawn_t <= 0.0
                && r.on_ground
                && !parked;
            self.unstick.update(
                dt,
                r.speed,
                ok && r.effects.tumble <= 0.0 && r.effects.dismount <= 0.0,
                true,
            );
        }
        let (re, place_now, race_time) = {
            let v = self.src.view();
            let re = self.fx.handle(&events, &v, Some(self.seat), ctx.haptics);
            let r = &v.racers[self.seat];
            self.fx.loops(r, v.tun.top_speed, dt, v.phase != RacePhase::Countdown);
            (re, r.place, v.race_time)
        };
        self.challenge_react(&re);
        self.battle_react(&re);
        self.drill_react(ctx, &events, dt);
        self.react(&re, place_now, race_time);
        if re.race_over && self.gp_recorded.is_none() {
            self.record_cup(ctx);
        }

        // Lessons end on their own state too.
        {
            let v = self.src.view();
            if let Some(l) = v.lesson {
                if (l.done || l.failed) && self.end.is_none() {
                    self.end = Some(End { t: 0.0, wait: 2.6 });
                }
            }
        }

        // ---- presentation ----
        let v = self.src.view();
        let seats = spectate::candidates(&v, self.seat, self.src.net_status().is_some_and(|n| n.waiting));
        self.spectating = !seats.is_empty();
        // Out of a battle: follow the best ostrich still in.
        let watch = match v.battle {
            Some(b) if b.is_out(self.seat) => {
                if self.watch == self.seat || b.is_out(self.watch) {
                    b.ranking().into_iter().find(|&i| !b.is_out(i)).unwrap_or(self.seat)
                } else {
                    self.watch
                }
            }
            _ if self.spectating => {
                let current = spectate::target(&seats, self.watch).unwrap_or(self.seat);
                let step = if ctx.keys.pressed(crate::platform::KeyCode::ArrowLeft) { -1 }
                    else if ctx.keys.pressed(crate::platform::KeyCode::ArrowRight) { 1 } else { 0 };
                if self.paused { current } else { spectate::cycle(&seats, current, step).unwrap_or(current) }
            }
            _ => self.seat,
        };
        if watch != self.watch {
            self.watch = watch;
            self.rig.snap();
        }
        let on_foot = v.racers[watch].effects.dismount > 0.0;
        let view = if on_foot {
            CameraView::Chase
        } else {
            self.cam_view
        };
        let fp = view == CameraView::FirstPerson;
        let poses = self.pres.poses(&v, dt, fp.then_some(watch));
        let r = &v.racers[self.seat];
        let rw = &v.racers[watch];
        let mut lp = poses[watch];
        if lp.dismounted {
            lp.pos += lp.chase_offset;
        }
        let lp_me = poses[self.seat];
        let top = v.tun.top_speed;
        let eye_guess = lp.pos - race_core::math::forward(lp.yaw) * 7.0;
        let floor = v.track.ground_height(Vec3::new(eye_guess.x, lp.pos.y, eye_guess.z), rw.track_idx as usize) + 0.9;
        let follow = Follow {
            pose: &lp,
            speed_frac: (rw.speed.max(0.0) / top).clamp(0.0, 1.4),
            boosting: rw.boost_t > 0.0,
            drifting: rw.drift.active,
            spun_out: rw.stun_t > 0.0,
            hold: rw.respawn_t > 0.0 && watch == self.seat && v.battle.is_none(),
            floor_y: floor,
            lateral: rw.lateral,
            intro: if v.phase == RacePhase::Countdown { 1.0 } else { 1.0 - smooth01(v.race_time / 1.8) },
            ceiling: (view != CameraView::FirstPerson).then(|| camera::deck_ceiling(&v.track, &lp, rw.track_idx as usize, rw.speed, view == CameraView::Far)).flatten(),
        };
        let out_of_it = v.challenge.is_some_and(|c| c.eliminated.contains(&self.seat));
        let base_cam = match (out_of_it, self.last_cam) {
            // Knocked out: the ostrich is parked out of sight, so hold the last view.
            (true, Some(c)) => c,
            _ => self.rig.update(dt, view, &follow),
        };
        self.last_cam = Some(base_cam);
        // Tilt steering: the 3D world stays level with the real ground; the HUD stays on the screen.
        let roll = (ctx.profile.steer_mode == crate::platform::data_dir::SteerMode::Tilt).then(|| ctx.tilt.roll()).flatten();
        let cam = camera::with_roll(base_cam, roll);
        let shake = self.rig.shake_vec(self.pres.time);
        let mirror = (lp_me.look_back > 0.003 && watch == self.seat).then(|| MirrorView { camera: camera::with_roll(camera::mirror_camera(&lp_me), roll), rect_px: [0.0; 4], scale: lp_me.look_back });
        let mut scene = self.pres.scene(&v, watch, poses, &events, cam, shake, mirror, 1.0);
        battle_fx::scene_bursts(&events, &v, &mut scene);
        if let Some(drill) = self.src.drill() {
            scene.drill_props = super::drill_props::instances(drill);
        }
        // The Time Trial ghost: pale blue-white ostrich + soft glow, no dust.
        let mut ghost_hud = None;
        if let Some(pb) = &self.ghost {
            let t = v.race_time.max(0.0);
            // The player's ostrich is drawn `alpha` of the way from the previous
            // tick to this one, so sample the ghost at that same instant.
            let gp = pb.pose(t - (1.0 - v.alpha) * race_core::math::DT);
            let mut pose = ghosts::ostrich_pose(&gp, pb.ghost.character, self.pres.time);
            if t > pb.time() + 2.5 {
                pose.scale = 0.001;
            }
            let tag = ghosts::project(&cam, ctx.viewport.screen.size(), pose.pos + Vec3::Y * 3.2);
            self.ghost_split = pb.split(v.race_time, r.progress);
            ghost_hud = Some(GhostHud { split: self.ghost_split, tag: tag.filter(|_| pose.scale > 0.01), finished: t > pb.time(), best: pb.time() });
            ghosts::add_to_scene(&mut scene, pose, (self.elapsed * 60.0) as u32 % 5 == 0);
        }
        if stage::needs_history() {
            stage::push_history(&scene);
        }
        self.scene = Some(scene);

        // ---- HUD state ----
        self.camera_flash = (self.camera_flash - dt).max(0.0);
        self.fade_in = (self.fade_in - dt).max(0.0);
        self.place_pulse = (self.place_pulse - dt * 2.2).max(0.0);
        if r.place != self.last_place {
            if self.last_place != 0 && v.phase == RacePhase::Racing {
                self.place_pulse = 1.0;
                self.place_gain = r.place < self.last_place;
            }
            self.last_place = r.place;
        }
        self.final_lap_t = (self.final_lap_t - dt).max(0.0);
        if let Some(g) = self.go_t.as_mut() {
            *g += dt;
            if *g > 0.9 {
                self.go_t = None;
            }
        }
        if let Some(b) = self.banner.as_mut() {
            b.t += dt;
            if b.t >= b.dur {
                self.banner = if self.queue.is_empty() { None } else { Some(self.queue.remove(0)) };
            }
        }
        let roul = self.roulette.as_mut().map(|rt| {
            rt.t += dt;
            let prog = (rt.t / ROULETTE_SECS).min(1.0);
            let phase = 14.0 * (1.0 - (1.0 - prog) * (1.0 - prog));
            let step = phase.floor() as i32;
            (rt.t >= ROULETTE_SECS, step, prog)
        });
        let mut roulette_view = None;
        if let Some((finished, step, prog)) = roul {
            if finished {
                self.roulette = None;
                self.fx.roulette_done();
            } else {
                if let Some(rt) = self.roulette.as_mut() {
                    if step != rt.last {
                        rt.last = step;
                        self.fx.roulette_tick();
                    }
                }
                let rolls: &[race_core::ItemRoll] = if v.battle.is_some() {
                    &race_core::BATTLE_ROLLS
                } else {
                    &race_core::ITEM_ROLLS
                };
                // The spin cycles the odds table and ends on what was really won.
                let landed = self.roulette.as_ref().map(|rt| rt.item);
                let shown = match landed {
                    Some(it) if step >= 13 => it,
                    _ => rolls[step.rem_euclid(rolls.len() as i32) as usize].item,
                };
                roulette_view = Some((shown, prog));
            }
        }

        let lesson = v.lesson.map(|l| LessonHud {
            number: l.id.number(),
            title: l.id.title(),
            instruction: l.id.instruction(),
            hint: l.id.hint(),
            control: l.id.control(),
            progress: l.progress,
            goal: l.goal,
            time: l.time,
            limit: l.id.time_limit(),
        });
        let dots: Vec<Dot> = v
            .racers
            .iter()
            .enumerate()
            .filter(|(i, rr)| rr.respawn_t <= 0.0 && !v.challenge.is_some_and(|c| c.eliminated.contains(i)))
            .map(|(i, rr)| {
                let a = race_core::Character::get(rr.character).accent;
                let lift = a.iter().cloned().fold(0.0, f32::max);
                let a = if lift < 0.5 { [a[0] + 0.4, a[1] + 0.4, a[2] + 0.4] } else { a };
                Dot { pos: rr.pos, color: c32([a[0].min(1.0), a[1].min(1.0), a[2].min(1.0)]), local: i == self.seat }
            })
            .collect();
        let respawn_fade = if r.respawn_t > 0.0 { (1.0 - r.respawn_t / v.tun.respawn_fade).clamp(0.0, 1.0) } else { (self.fade_in / 0.45).clamp(0.0, 1.0) };
        let safe = ctx.viewport.safe;
        let mrect = Rect::from_center_size(pos2(safe.center().x, safe.top() + 10.0 + MIRROR_PT.1 * 0.5), vec2(MIRROR_PT.0, MIRROR_PT.1));
        self.mirror_rect = mrect;
        // Challenge score block: read off the sim's state every frame (no per-event animation).
        let medal_now = v.challenge.map(|c| if c.id.higher_is_better() { c.medal } else { 0 }).unwrap_or(0);
        let challenge: Option<ChallengeHud> = v.challenge.map(|cs| {
            if (cs.score - self.last_score).abs() > 1e-4 {
                self.last_score = cs.score;
                // Egg Hunt and Turbo Time Attack show the clock as their score: it changes every tick.
                if !matches!(cs.id, ChallengeId::EggHunt | ChallengeId::TurboTimeAttack) {
                    self.score_pulse = 1.0;
                }
            }
            let mut h = challenge_hud::build(cs, v.racers.len());
            h.pulse = self.score_pulse;
            h.flash = self.miss_flash;
            h.pops = self.pops.clone();
            h
        });
        self.score_pulse = (self.score_pulse - dt * 3.5).max(0.0);
        self.miss_flash = (self.miss_flash - dt * 1.6).max(0.0);
        let battle_hud = v.battle.map(|b| {
            self.battle_anim.update(dt, b.feathers.get(self.seat).copied().unwrap_or(0));
            let mut h = battle_hud::build(&v, b, self.seat, &self.battle_anim);
            if watch != self.seat {
                let name = v.cfg.racers.get(watch).map(|r| r.name.to_uppercase()).unwrap_or_default();
                h.spectating = Some((name, self.spec_speed));
            }
            h
        });
        for pop in self.pops.iter_mut() {
            pop.age += dt;
        }
        self.pops.retain(|p| p.age < challenge_hud::POP_SECS);
        let medal_up = medal_now > self.last_medal && !v.challenge.is_some_and(|c| c.done);
        self.last_medal = medal_now;
        let tier_cap = v.tun.drift_tiers;
        let drill_hud = self.src.drill().map(|ds| {
            let live = v.phase == RacePhase::Racing && !ds.over;
            let arrow = if live { drill_arrow(ds, &cam, ctx.viewport.screen.size(), r.pos) } else { None };
            drill_hud::build(ds, &self.drill_anim, arrow)
        });
        let frame = hud::Frame {
            spectating: self.spectating,
            safe,
            recording: self.recording,
            layout,
            steer_mode: ctx.profile.steer_mode,
            place: r.place.max(1),
            racers: v.racers.len(),
            lap: r.lap,
            laps: v.cfg.laps,
            time: v.lesson.map(|l| l.time).unwrap_or_else(|| if self.spectating { r.finished_at.unwrap_or(v.race_time) } else { v.race_time }),
            coins: r.coins,
            item: r.item.map(|i| (i, r.item_uses)),
            item_queued: r.queue.len() as u8,
            item_active: r.active_item(&v.tun, v.play.scramble).map(|a| (a.item, a.fraction())),
            roulette: roulette_view,
            heat: r.laser.heat,
            overheated: r.laser.overheated,
            lasering: r.laser.firing,
            speed_kmh: r.speed.max(0.0) * 3.6,
            speed_frac: r.speed.max(0.0) / top,
            boosting: r.boost_t > 0.0,
            drifting: r.drift.active,
            drift_charge: r.drift.charge,
            drift_tier: r.drift.tier,
            tiers: tier_cap,
            countdown: (v.phase == RacePhase::Countdown).then_some(Countdown {
                run_start: r.effects.le_mans,
                left: v.countdown_left,
                revved: self.revved,
                window: if self.drill.is_some() || r.effects.le_mans {
                    0.0
                } else {
                    v.tun.start_boost_window
                },
            }),
            go_t: self.go_t,
            wrong_way: !self.recording && r.wrong_way && v.phase == RacePhase::Racing,
            lesson,
            drill: drill_hud,
            challenge,
            battle: battle_hud,
            ghost: ghost_hud,
            mirror: (lp_me.look_back > 0.003 && watch == self.seat).then_some((mrect, lp_me.look_back)),
            dots,
            camera_name: camera::view_name(self.cam_view),
            camera_flash: self.camera_flash,
            place_pulse: self.place_pulse,
            place_gain: self.place_gain,
            final_lap_t: self.final_lap_t,
            banner: self.banner.clone(),
            fade: respawn_fade,
            magnet: r.magnet_t > 0.0,
            shield: r.shield_t > 0.0,
            unstick_hint: self.unstick.hint(),
        };
        self.frame = Some(frame);
        let (log_t, log_place) = (v.race_time, r.place);
        drop(v);
        if medal_up {
            let c = crate::ui::screens::challenges::medal_colors(medal_now).0;
            let mut b = Banner::new(format!("{}!", medal_word(medal_now)), c, 1.1);
            b.sub = None;
            self.push_banner(b);
        }

        // Periodic renderer stats for the device log.
        self.stat_t += dt;
        if self.stat_t >= 5.0 {
            self.stat_t = 0.0;
            if let Some(s) = stage::stats() {
                let n = self.ms_n.max(1) as f32;
                log::info!("race: t={:.1}s place {} {:?} render {} draws, {} tris, {} particles | cpu ms/frame update {:.2} draw {:.2} paint {:.2}", log_t, log_place, ctx.profile.quality, s.draw_calls, s.triangles, s.particles, self.ms[0] / n, self.ms[1] / n, self.ms[2] / n);
                self.ms = [0.0; 3];
                self.ms_n = 0;
            }
        }

        // ---- the end ----
        if let Some(e) = self.end.as_mut() {
            e.t += dt;
            if e.t >= e.wait && self.src.ready_to_finish() {
                return self.finish(ctx, false);
            }
        }
        Transition::None
    }

    fn draw_inner(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let t = ctx.time as f32;
        let p = ui.layer_painter(egui::LayerId::background());
        {
            let v = self.src.view();
            if let Some(r) = v.racers.get(self.watch) {
                super::playful_hud::draw(
                    &p,
                    ctx.viewport.safe,
                    r,
                    v.play.scramble,
                    v.tun.playful.splat_time,
                    v.phase,
                    t,
                );
            }
        }
        if let Some(f) = &self.frame {
            hud::draw(&p, f, &self.ctl, &self.minimap, t);
        }
        if let Some(n) = self.src.net_status() {
            net_indicator(&p, ctx.viewport.safe, &n);
            // How host migration stands (protected, paused, reconnecting...).
            if let Some((state, host)) = ctx.online.with(|s| (s.migration_state(), s.is_host())) {
                let safe = ctx.viewport.safe;
                let state = self.mig_delay.state(&state, decented_signet::sharing::now_ms());
                crate::ui::screens::migration::race_status(ui, &p, ctx.viewport.screen, safe, &state, host, &mut self.mig_note, ctx.dt);
            }
        }
        // A drill's results card: Retry / Next drill / Back.
        let action = self.drill_end.as_ref().and_then(|e| drill_hud::card_ui(ui, ctx.viewport.safe, &e.card, e.age, t));
        if let (Some(a), false) = (action, self.done) {
            return self.drill_card_action(ctx, a);
        }
        if self.paused && !self.done && self.drill_end.is_none() {
            return self.pause_ui(ctx, ui);
        }
        if self.spectating && !self.done {
            let v = self.src.view();
            let seats = spectate::candidates(&v, self.seat, self.src.net_status().is_some_and(|n| n.waiting));
            if let Some(watch) = spectate::ui(ui, ctx.viewport.safe, &v, &seats, self.watch) {
                self.watch = watch;
                self.rig.snap();
            }
        }
        Transition::None
    }

    fn paint_inner(&mut self, pass: &mut ScenePass) {
        let ppp = pass.ppp;
        let rect = self.mirror_rect;
        let Some(scene) = self.scene.as_mut() else { return };
        if let Some(m) = scene.mirror.as_mut() {
            m.rect_px = [rect.min.x * ppp, rect.min.y * ppp, rect.width() * ppp, rect.height() * ppp];
        }
        let track = self.launch.config.track;
        let mut course = geometry::Course::of(track, self.launch.config.mode);
        // Only the Paddock goes bare; any other track keeps the key it was loaded under.
        if self.recording && track == race_core::TrackId::Field {
            course = course.bare();
        }
        stage::draw(pass, WorldKey::Track(track, course), || geometry::cached_course(track, course), scene, self.frame_dt);
        let _: Option<Camera> = None;
    }
}

impl RaceScreen {
    fn pause_ui(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let screen = ctx.viewport.screen;
        let safe = ctx.viewport.safe;
        let dim = ui.layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("race-dim")));
        dim.rect_filled(screen, 0.0, Color32::from_black_alpha(150));
        // Online there is no restart (the room decides the next race).
        let online = self.src.net_status().is_some();
        // No restart online, nor once a Grand Prix racer has crossed the line: a replay
        // would run the same round again after its result was already saved.
        let no_restart = online || self.restart_locked() || self.no_restart;
        let panel = Rect::from_center_size(safe.center(), vec2(248.0, if no_restart { 273.0 } else { 322.0 }));
        dim.rect_filled(panel.translate(vec2(0.0, 4.0)), 18.0, Color32::from_black_alpha(100));
        dim.rect_filled(panel, 18.0, Color32::from_rgb(28, 14, 50));
        dim.rect_stroke(panel, 18.0, stroke(2.0, theme::ACCENT.gamma_multiply(0.8)), egui::StrokeKind::Inside);
        dim.text(pos2(panel.center().x, panel.top() + 34.0), Align2::CENTER_CENTER, "PAUSED", theme::title(28.0), theme::ACCENT);
        let mut action = None;
        let q = match ctx.profile.quality {
            Quality::Low => "LOW",
            Quality::Balanced => "BALANCED",
            Quality::High => "HIGH",
        };
        let net = self.src.net_status();
        let host = net.as_ref().is_some_and(|n| n.host);
        let quit_label = if host && self.quit_ask { "END FOR EVERYONE?" } else { "QUIT" };
        let labels = ["RESUME".to_string(), "RESTART".to_string(), format!("CAMERA: {}", camera::view_name(ctx.profile.camera)), format!("GRAPHICS: {q}"), quit_label.to_string()];
        if online {
            let note = if host { "The race goes on. A bot steers for you while paused." } else { "The others keep racing. A bot steers for you while paused." };
            dim.text(pos2(panel.center().x, panel.bottom() + 16.0), Align2::CENTER_CENTER, note, theme::ui_medium(11.5), Color32::from_white_alpha(190));
        }
        egui::Area::new(egui::Id::new("race-pause")).order(egui::Order::Foreground).fixed_pos(panel.min).show(ui, |ui| {
            ui.set_min_size(panel.size());
            let mut slot = 0;
            for (i, label) in labels.iter().enumerate() {
                if no_restart && i == 1 {
                    continue;
                }
                let rect = Rect::from_min_size(panel.min + vec2(24.0, 62.0 + slot as f32 * 49.0), vec2(panel.width() - 48.0, 41.0));
                slot += 1;
                let resp = ui.interact(rect, egui::Id::new(("race-pause-btn", i)), egui::Sense::click());
                let down = resp.is_pointer_button_down_on();
                let (fill, text) = match i {
                    0 => (theme::ACCENT, Color32::from_rgb(40, 18, 8)),
                    4 => (Color32::from_rgb(70, 30, 50), theme::TEXT),
                    _ => (Color32::from_rgb(64, 38, 100), theme::TEXT),
                };
                let r = if down { rect.shrink(2.5) } else { rect };
                ui.painter().rect_filled(r, 12.0, fill);
                ui.painter().text(r.center(), Align2::CENTER_CENTER, label, theme::numbers(13.5), text);
                if resp.clicked() {
                    action = Some(i);
                }
            }
        });
        match action {
            Some(0) => {
                self.quit_ask = false;
                self.pause(false);
                Transition::None
            }
            Some(1) if !no_restart => {
                if self.recording {
                    self.stop_recording();
                    return Transition::Replace(Box::new(super::recording::RunTrackScreen::new(ctx.profile)));
                }
                // The new race keeps the race music: only silence this one's loops.
                self.fx.stop_loops();
                // A drill restarts on a fresh course; a race on the same launch.
                let launch = match self.drill {
                    Some(id) => crate::ui::screens::training::drill_launch(ctx.profile, id),
                    None => self.launch.clone(),
                };
                Transition::Replace(Box::new(new(launch, Staging { paused: false, ..self.staging })))
            }
            Some(2) => {
                ctx.profile.camera = camera::next_view(ctx.profile.camera);
                self.pause_selected_camera = true;
                self.rig.snap();
                Transition::None
            }
            Some(3) => {
                ctx.profile.quality = match ctx.profile.quality {
                    Quality::Low => Quality::Balanced,
                    Quality::Balanced => Quality::High,
                    Quality::High => Quality::Low,
                };
                Transition::None
            }
            Some(4) => {
                // The host's quit shuts the room: ask once.
                if host && !self.quit_ask {
                    self.quit_ask = true;
                    Transition::None
                } else {
                    // Stars already earned in a drill are kept when you quit.
                    self.drill_settle(ctx, false);
                    self.finish(ctx, true)
                }
            }
            _ => Transition::None,
        }
    }
}

/// Opponents beside the local racer (for flick-to-bash).
fn adjacency(v: &super::source::RaceView, seat: usize) -> Adjacent {
    let me = &v.racers[seat];
    let right = race_core::math::right(me.yaw);
    let fwd = race_core::math::forward(me.yaw);
    controls::adjacent_sides(v.racers.iter().enumerate().filter(|(i, r)| *i != seat && r.respawn_t <= 0.0).map(|(_, r)| {
        let d = r.pos - me.pos;
        (d.dot(right), d.dot(fwd))
    }))
}

fn medal_word(m: u8) -> &'static str {
    match m {
        1 => "BRONZE",
        2 => "SILVER",
        _ => "GOLD",
    }
}

// ---- open-ground drills (SPEC 18.2) ----

impl RaceScreen {
    /// Drill events: pop-ups, sounds, the end of the run and its results card.
    fn drill_react(&mut self, ctx: &mut Ctx, events: &[race_core::Event], dt: f32) {
        if self.drill.is_none() {
            return;
        }
        for e in events {
            if let race_core::Event::Drill(d) = e {
                self.drill_anim.absorb(*d);
                drill_sound(ctx, *d);
                if *d == DrillEvent::Strike {
                    self.rig.add_shake(0.3);
                }
            }
        }
        self.drill_anim.update(dt);
        if self.src.drill().is_some_and(|d| d.over) {
            let t = self.drill_over_t.unwrap_or(0.0) + dt;
            self.drill_over_t = Some(t);
            if t >= DRILL_CARD_DELAY && self.drill_end.is_none() {
                self.drill_settle(ctx, true);
            }
        }
        if let Some(e) = self.drill_end.as_mut() {
            e.age += dt;
        }
    }

    /// Save the run (best level, stars, Master), pay the ₿ and, when the run is over, open the results card.
    /// Once per run. Leaving early keeps what was earned but only if something was.
    fn drill_settle(&mut self, ctx: &mut Ctx, over: bool) {
        use crate::ui::progress::{self, Earn, Kind, Played};
        use crate::ui::progress_ui::Report;
        if self.drill_settled {
            return;
        }
        let Some(ds) = self.src.drill().cloned() else { return };
        if !over && ds.clean_total == 0 && ds.stars == 0 {
            return;
        }
        self.drill_settled = true;
        let i = DrillId::ALL.iter().position(|d| *d == ds.id).unwrap_or(0);
        let level = ds.best_level.clamp(1, race_core::drill::MAX_LEVEL);
        let (prev_best, prev_stars, prev_master) = (ctx.profile.drill_best[i], ctx.profile.drill_stars[i], ctx.profile.drill_master[i]);
        let stars = ds.stars.min(3);
        ctx.profile.drill_best[i] = prev_best.max(level);
        ctx.profile.drill_stars[i] = prev_stars.max(stars);
        ctx.profile.drill_master[i] = prev_master || ds.master;
        let earn = Earn { bonus: progress::drill_pay(prev_stars, stars, prev_master, ds.master), bonus_kind: 3, ..Earn::default() };
        let played = Played { kind: Kind::Training, track: Some(self.launch.config.track), place: 0, racers: 1, finished: true, coins: 0, stats: self.stats, cup_won: None };
        let report = Report::settle(ctx, &played, earn);
        if over {
            self.drill_end = Some(DrillEnd {
                age: 0.0,
                card: ResultCard {
                    id: ds.id,
                    level,
                    stars,
                    master: ds.master,
                    new_best: level > prev_best,
                    best: ctx.profile.drill_best[i],
                    clean: ds.clean_total,
                    earned: earn.total(),
                    unlocked: report.unlocked.iter().map(|a| a.title()).collect(),
                    next: drill_hud::next_drill(ds.id),
                },
            });
        }
    }

    fn drill_card_action(&mut self, ctx: &mut Ctx, a: CardAction) -> Transition {
        let id = self.drill.unwrap_or(DrillId::Slalom);
        match a {
            CardAction::Back => self.finish(ctx, false),
            CardAction::Retry | CardAction::Next => {
                let id = if a == CardAction::Next { drill_hud::next_drill(id) } else { id };
                self.fx.stop_loops();
                let launch = crate::ui::screens::training::drill_launch(ctx.profile, id);
                Transition::Replace(Box::new(new(launch, Staging { paused: false, ..self.staging })))
            }
        }
    }
}

/// The existing effects, one per drill event.
fn drill_sound(ctx: &Ctx, d: DrillEvent) {
    use race_audio::Sfx;
    if ctx.muted {
        return;
    }
    let (sfx, gain, pitch) = match d {
        DrillEvent::RepClean => (Sfx::Coin, 0.7, 1.25),
        DrillEvent::Strike => (Sfx::ShieldBlock, 0.9, 0.55),
        DrillEvent::LevelUp(_) => (Sfx::Lap, 1.0, 1.0),
        DrillEvent::Star(_) => (Sfx::Finish, 0.8, 1.1),
        DrillEvent::Master => (Sfx::FinalLap, 1.0, 1.0),
        DrillEvent::Over => (Sfx::MenuBack, 0.8, 0.8),
    };
    crate::sound::with(|a| a.play(sfx, gain, pitch));
}

/// Where to point the edge arrow: the next gate, log, ring or the egg.
fn drill_arrow(ds: &race_core::DrillState, cam: &Camera, screen: egui::Vec2, from: Vec3) -> Option<drill_hud::ArrowHud> {
    use race_core::drill::RepInfo;
    let (target, strong) = match ds.current_rep()? {
        RepInfo::Gate { centre, .. } | RepInfo::Ring { centre, .. } => (centre, false),
        RepInfo::Logs { remaining, .. } => (*remaining.first()?, false),
        RepInfo::Egg { pos } => (pos?, true),
        RepInfo::Buddy => return None,
    };
    drill_hud::arrow_to(cam, screen, target, from, strong)
}

fn smooth01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Online: a small connection-quality dot with the round trip (bottom left),
/// and a note while you wait for the others.
fn net_indicator(p: &egui::Painter, safe: Rect, n: &super::source::NetStatus) {
    let col = match n.quality {
        0 => Color32::from_rgb(0x6B, 0xE0, 0xA8),
        1 => Color32::from_rgb(0xFF, 0xC8, 0x3D),
        _ => Color32::from_rgb(0xFF, 0x5A, 0x4A),
    };
    let c = pos2(safe.left() + 14.0, safe.bottom() - 12.0);
    p.circle_filled(c, 5.0, Color32::from_black_alpha(120));
    p.circle_filled(c, 3.6, col);
    let label = match (n.host, n.rtt_ms) {
        (true, None) => "HOST".to_string(),
        (true, Some(r)) => format!("HOST  {r} ms"),
        (false, Some(r)) => format!("{r} ms"),
        (false, None) => "...".to_string(),
    };
    p.text(c + vec2(10.0, 0.0), Align2::LEFT_CENTER, label, theme::ui_bold(10.5), Color32::from_white_alpha(200));
    if n.waiting {
        p.text(pos2(safe.center().x, safe.top() + 22.0), Align2::CENTER_CENTER, "Waiting for the others to finish...", theme::ui_bold(12.5), theme::SAND);
    }
    // A player left the race: say so for a few seconds, under the HUD's top row.
    if let Some(text) = &n.notice {
        let at = pos2(safe.center().x, safe.top() + if n.waiting { 46.0 } else { 22.0 });
        let galley = p.layout_no_wrap(text.clone(), theme::ui_bold(12.5), theme::SAND);
        let bg = Rect::from_center_size(at, galley.size() + vec2(20.0, 8.0));
        p.rect_filled(bg, 10.0, Color32::from_black_alpha(140));
        p.galley(bg.center() - galley.size() / 2.0, galley, theme::SAND);
    }
}
