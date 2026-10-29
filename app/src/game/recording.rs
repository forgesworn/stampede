//! Run a loop on the Paddock, then turn it into a custom track.

use super::race::{self, RaceScreen, Staging};
use crate::render::ScenePass;
use crate::ui::screens::{common, Ctx, Screen, Transition};
use crate::ui::{feedback, theme, widgets};
use egui::{pos2, vec2, Align2, Color32, Pos2, Rect, Shape, Stroke};
use race_core::custom::{RecordEvent, RECORD_MIN, RECORD_STEP};
use race_core::{auto_fill, custom_track_from, shape, BotLevel, RaceMode, RacePhase, Recorder, RecorderStatus, TrackId, Vec2, Vec3};
use race_render::{DrillPropInst, DrillPropKind};

/// A dust patch is laid every this many metres of the run.
const DUST_EVERY: f32 = 4.0;
/// How long a wrong-way or too-soon note stays up (s).
const NOTE_SECS: f32 = 2.2;
/// Half the width of the gate and the trail: the road the run becomes is 18 m wide.
const HALF_ROAD: f32 = 9.0;

pub struct RunTrackScreen {
    race: RaceScreen,
    recorder: Option<Recorder>,
    start_y: f32,
    error: Option<String>,
    /// The closed run that would not shape, for the designer.
    failed: Option<(Vec<Vec2>, String)>,
    trail: Vec<DrillPropInst>,
    next_dust: f32,
    note: Option<(String, f32)>,
    /// Once the run is long enough: the way to the gate, relative to the runner's heading
    /// (radians, positive to the right), and the distance to it (m).
    home: Option<(f32, f32)>,
    /// `--shot run_track_sample`: start with a few hundred metres already run.
    sample: bool,
}

impl RunTrackScreen {
    pub fn new(profile: &crate::platform::data_dir::Profile) -> Self {
        let mut launch = common::quick_launch(profile, TrackId::Field, 1, 0, BotLevel::Easy);
        launch.config.mode = RaceMode::Solo;
        Self {
            race: race::new(launch, Staging::default()).recording(),
            recorder: None,
            start_y: 0.0,
            error: None,
            failed: None,
            trail: Vec::new(),
            next_dust: DUST_EVERY,
            note: None,
            home: None,
            sample: false,
        }
    }

    /// For the shot: the runner stands 80 m short of the gate, a 330 m loop beside them.
    pub fn sample(profile: &crate::platform::data_dir::Profile) -> Self {
        Self { sample: true, ..Self::new(profile) }
    }

    fn props(&self) -> Vec<DrillPropInst> {
        let Some(recorder) = &self.recorder else { return Vec::new() };
        let start = recorder.start();
        let mut props = self.trail.clone();
        props.push(DrillPropInst {
            kind: DrillPropKind::Finish,
            pos: Vec3::new(start.x, self.start_y, start.y),
            yaw: race_core::math::yaw_of(Vec3::new(recorder.dir().x, 0.0, recorder.dir().y)),
            knocked: 0.0,
            size: Vec3::new(2.0 * HALF_ROAD + 1.0, 6.5, 10.0),
        });
        props
    }

    /// A dust patch for every 4 m run since the last call.
    fn lay_trail(&mut self) {
        let Some(recorder) = &self.recorder else { return };
        let pts = recorder.points();
        while recorder.len() >= self.next_dust {
            let i = (self.next_dust / RECORD_STEP).round() as usize;
            let (Some(&p), Some(&back)) = (pts.get(i), pts.get(i.saturating_sub(2))) else { break };
            let d = p - back;
            self.trail.push(DrillPropInst {
                kind: DrillPropKind::Dust,
                pos: Vec3::new(p.x, self.start_y, p.y),
                yaw: d.x.atan2(d.y),
                knocked: 0.0,
                size: Vec3::new(2.0 * HALF_ROAD, 0.05, 7.0),
            });
            self.next_dust += DUST_EVERY;
        }
    }

    /// The way back to the gate from `pos` heading `yaw`, once the run is long enough. It leads
    /// round behind the gate first, so the runner goes through it the right way.
    fn way_home(recorder: &Recorder, pos: Vec3, yaw: f32) -> Option<(f32, f32)> {
        if recorder.len() < RECORD_MIN {
            return None;
        }
        let (g, d) = (recorder.start(), recorder.dir());
        let p = Vec2::new(pos.x, pos.z);
        let side_dir = Vec2::new(d.y, -d.x);
        let (s, lat) = ((p - g).dot(d), (p - g).dot(side_dir));
        let aim = if s < -2.0 && lat.abs() < 6.0 - s * 1.2 {
            // Behind the gate and roughly lined up: straight through it.
            g + d * 10.0
        } else if s >= -2.0 && lat.abs() < HALF_ROAD + 6.0 {
            // In front of it: step aside before coming round.
            g + side_dir * lat.signum() * 30.0
        } else {
            g - d * 25.0
        };
        let to = aim - p;
        let r = race_core::math::right(yaw);
        let f = race_core::math::forward(yaw);
        let turn = (to.x * r.x + to.y * r.z).atan2(to.x * f.x + to.y * f.z);
        Some((turn, p.distance(g)))
    }

    fn fail(&mut self, ctx: &Ctx, message: String, raw: Option<Vec<Vec2>>) {
        self.error = Some(message);
        self.failed = raw.map(|raw| (raw, Self::free_name(ctx)));
    }

    fn free_name(ctx: &Ctx) -> String {
        let saved = crate::custom_tracks::list(ctx.data_dir);
        let n = (1..).find(|n| !saved.iter().any(|t| t.name == format!("Run {n}"))).unwrap();
        format!("Run {n}")
    }

    /// Fills the recorder with a 330 m loop ending at the runner, the gate 80 m ahead.
    fn fake_run(pos: Vec3, yaw: f32) -> Recorder {
        let f = race_core::math::forward(yaw);
        let l = -race_core::math::right(yaw);
        let (fwd, left) = (Vec2::new(f.x, f.z), Vec2::new(l.x, l.z));
        let at = |u: f32, v: f32| Vec2::new(pos.x, pos.z) + fwd * (u + 80.0) + left * v;
        let mut path = vec![];
        let r = 20.0;
        for k in 0..=60 {
            path.push(at(k as f32, 0.0));
        }
        for k in 1..=60 {
            let a = k as f32 / 60.0 * std::f32::consts::PI;
            path.push(at(60.0 + r * a.sin(), r - r * a.cos()));
        }
        for k in 1..=140 {
            path.push(at(60.0 - k as f32, 2.0 * r));
        }
        for k in 1..=60 {
            let a = k as f32 / 60.0 * std::f32::consts::PI;
            path.push(at(-80.0 - r * a.sin(), r + r * a.cos()));
        }
        let mut rec = Recorder::new(path[0], fwd);
        for p in path {
            rec.push(p);
        }
        rec
    }
}

impl Screen for RunTrackScreen {
    fn orientation(&self) -> crate::platform::os::Orientation {
        crate::platform::os::Orientation::Landscape
    }

    fn name(&self) -> &'static str {
        if self.sample {
            "run_track_sample"
        } else {
            "run_track"
        }
    }
    fn keep_awake(&self) -> bool {
        true
    }
    fn on_background(&mut self, ctx: &mut Ctx) {
        self.race.on_background(ctx);
    }
    fn clear_color(&self, ctx: &Ctx) -> [f32; 4] {
        self.race.clear_color(ctx)
    }
    fn paint_scene(&mut self, pass: &mut ScenePass) {
        self.race.paint_scene(pass);
    }
    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        if self.error.is_some() {
            self.race.stop_recording();
            return Transition::Pop;
        }
        self.race.back(ctx)
    }
    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        if let Some((_, t)) = self.note.as_mut() {
            *t -= ctx.dt;
            if *t <= 0.0 {
                self.note = None;
            }
        }
        if self.error.is_none() {
            let transition = self.race.update(ctx);
            if !matches!(transition, Transition::None) {
                self.race.stop_recording();
                return transition;
            }
            let (pos, yaw, phase, paused) = self.race.record_view();
            if self.recorder.is_none() {
                self.start_y = pos.y;
                self.recorder = Some(if self.sample { Self::fake_run(pos, yaw) } else { Recorder::new(Vec2::new(pos.x, pos.z), Vec2::new(yaw.sin(), yaw.cos())) });
            }
            if phase == RacePhase::Racing && !paused {
                let recorder = self.recorder.as_mut().expect("started above");
                let status = recorder.push(Vec2::new(pos.x, pos.z));
                match recorder.take_event() {
                    Some(RecordEvent::WrongWay) => self.note = Some(("Wrong way – go through from the other side".into(), NOTE_SECS)),
                    Some(RecordEvent::Early { remaining }) => self.note = Some((format!("Keep going – {:.0} m more", remaining.ceil()), NOTE_SECS)),
                    Some(RecordEvent::Skewed) => self.note = Some(("Come through the gate straighter".into(), NOTE_SECS)),
                    None => {}
                }
                match status {
                    RecorderStatus::Closed => {
                        let points = recorder.points().to_vec();
                        match shape(&points, false, 2.0 * HALF_ROAD) {
                            Ok(shaped) => {
                                let name = Self::free_name(ctx);
                                let seed = common::fresh_seed() as u32;
                                let features = auto_fill(&shaped, seed);
                                let track = custom_track_from(&shaped, name, TrackId::Savanna, 3, 180, features, seed);
                                if let Err(e) = track.to_def() {
                                    self.fail(ctx, e.to_string(), Some(points));
                                } else {
                                    self.race.stop_recording();
                                    return Transition::Replace(Box::new(crate::ui::screens::designer::DesignerScreen::from_run(track)));
                                }
                            }
                            Err(e) => self.fail(ctx, e.to_string(), Some(points)),
                        }
                    }
                    RecorderStatus::TooLong => self.fail(ctx, "Over 3 km. Try a shorter loop.".into(), None),
                    RecorderStatus::Recording { .. } => {}
                }
            }
            self.lay_trail();
            self.home = self.recorder.as_ref().and_then(|r| Self::way_home(r, pos, yaw));
        }
        if self.error.is_some() {
            self.race.stop_recording();
        }
        self.race.record_hud(self.props());
        Transition::None
    }
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let transition = self.race.draw(ctx, ui);
        if !matches!(transition, Transition::None) {
            self.race.stop_recording();
            return transition;
        }
        if self.race.record_view().3 {
            return Transition::None;
        }
        let safe = ctx.viewport.safe;
        let p = ui.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("record-hud")));
        let panel = Rect::from_min_size(safe.min + vec2(10.0, 10.0), vec2(126.0, 126.0));
        p.rect_filled(panel, 16.0, Color32::from_black_alpha(185));
        let k = panel.width() / 610.0;
        let map = |v: Vec2| pos2(panel.center().x - v.x * k, panel.center().y - v.y * k);
        let mut line = Some("Run a loop, then finish through the gate");
        if let Some(recorder) = &self.recorder {
            let points: Vec<_> = recorder.points().iter().map(|&v| map(v)).collect();
            p.add(Shape::line(points, Stroke::new(2.0_f32, theme::SAND)));
            // The gate: a short bar across the start, a tick the way through.
            let (g, d) = (recorder.start(), recorder.dir());
            let across = Vec2::new(d.y, -d.x) * 32.0;
            let gate = Color32::from_rgb(110, 240, 140);
            p.line_segment([map(g - across), map(g + across)], Stroke::new(3.0_f32, gate));
            p.line_segment([map(g), map(g + d * 34.0)], Stroke::new(2.0_f32, gate));
            if let Some(&last) = recorder.points().last() {
                p.circle_filled(map(last), 3.0, Color32::WHITE);
            }
            let text = format!("RUN A TRACK   {:.0} m / 3000 m", recorder.len());
            super::hud::shadow_text(&p, pos2(safe.center().x, safe.top() + 21.0), Align2::CENTER_CENTER, &text, theme::ui_bold(16.0), theme::SAND);
            if let Some((turn, dist)) = self.home {
                // In place of the hint line: an arrow to the gate and how far it is.
                line = None;
                let y = safe.top() + 52.0;
                arrow(&p, pos2(safe.center().x - 58.0, y), turn, 16.0, theme::GOLD);
                super::hud::shadow_text(&p, pos2(safe.center().x - 34.0, y), Align2::LEFT_CENTER, &format!("Finish {:.0} m", dist), theme::ui_bold(17.0), theme::GOLD);
            }
        }
        if let Some(line) = line {
            super::hud::shadow_text(&p, pos2(safe.center().x, safe.top() + 43.0), Align2::CENTER_CENTER, line, theme::ui_medium(12.0), theme::TEXT);
        }
        if let Some((note, t)) = &self.note {
            let alpha = (t / 0.3).clamp(0.0, 1.0);
            let c = pos2(safe.center().x, safe.center().y - safe.height() * 0.18);
            let colour = theme::TEXT.gamma_multiply(alpha);
            let g = p.layout_no_wrap(note.clone(), theme::ui_bold(18.0), colour);
            let r = Rect::from_center_size(c, g.size() + vec2(32.0, 16.0));
            p.rect_filled(r, 12.0, Color32::from_black_alpha((200.0 * alpha) as u8));
            p.galley(r.min + vec2(16.0, 8.0), g, colour);
        }
        if let Some(error) = self.error.clone() {
            let mut out = Transition::None;
            let labels: &[&str] = if self.failed.is_some() { &["Try again", "Open in designer", "Discard"] } else { &["Try again", "Discard"] };
            let width = 30.0 + labels.len() as f32 * 175.0 - 15.0;
            egui::Area::new(egui::Id::new("record-error"))
                .fixed_pos(safe.center() - vec2(width * 0.5, 55.0))
                .show(ui, |ui| {
                    let rect = Rect::from_min_size(ui.min_rect().min, vec2(width, 110.0));
                    widgets::panel(ui.painter(), rect, 16.0);
                    let g = ui.painter().layout(error, theme::ui_bold(14.0), theme::TEXT, width - 30.0);
                    ui.painter().galley(rect.min + vec2(15.0, 12.0), g, theme::TEXT);
                    for (i, &label) in labels.iter().enumerate() {
                        let button = Rect::from_min_size(rect.min + vec2(15.0 + i as f32 * 175.0, 66.0), vec2(160.0, 32.0));
                        if widgets::button(ui, button, &format!("record-error-{i}"), &widgets::Btn::new(label)).clicked() {
                            feedback::tap(ctx);
                            self.race.stop_recording();
                            out = match label {
                                "Try again" => Transition::Replace(Box::new(Self::new(ctx.profile))),
                                "Open in designer" => {
                                    let (raw, name) = self.failed.take().expect("offered only with a failed run");
                                    Transition::Replace(Box::new(crate::ui::screens::designer::DesignerScreen::from_failed_run(raw, name)))
                                }
                                _ => Transition::Pop,
                            };
                        }
                    }
                });
            return out;
        }
        Transition::None
    }
}

/// A HUD arrow at `c` pointing `turn` radians right of straight up.
fn arrow(p: &egui::Painter, c: Pos2, turn: f32, size: f32, fill: Color32) {
    let (s, co) = turn.sin_cos();
    let rot = |x: f32, y: f32| c + vec2(x * co - y * s, x * s + y * co) * size;
    let pts = [(0.0, -1.0), (0.8, 0.0), (0.3, 0.0), (0.3, 0.9), (-0.3, 0.9), (-0.3, 0.0), (-0.8, 0.0)];
    let outline: Vec<Pos2> = pts.iter().map(|&(x, y)| rot(x, y)).collect();
    // Stem then head: egui fills convex shapes only, and the head's outline covers the join.
    p.add(Shape::convex_polygon(vec![outline[2], outline[3], outline[4], outline[5]], fill, Stroke::new(2.0_f32, theme::OUTLINE)));
    p.add(Shape::convex_polygon(vec![outline[0], outline[1], outline[6]], fill, Stroke::new(2.0_f32, theme::OUTLINE)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Options;
    use crate::engine::Engine;
    use crate::haptics::NoHaptics;
    use crate::platform::data_dir::Profile;
    use crate::platform::os::RecordingPlatform;
    use crate::platform::TouchPhase;

    fn frames(engine: &mut Engine, n: usize) {
        for _ in 0..n {
            engine.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        }
    }

    fn tap(engine: &mut Engine, y: f64) {
        engine.touch_event(8, TouchPhase::Started, (1200.0, y));
        frames(engine, 2);
        engine.touch_event(8, TouchPhase::Ended, (1200.0, y));
        frames(engine, 30);
    }

    #[test]
    fn the_way_home_leads_through_the_gate_the_right_way() {
        // Heading +z, the gate 80 m ahead, a 330 m loop beside.
        let rec = RunTrackScreen::fake_run(Vec3::ZERO, 0.0);
        assert!(rec.len() >= RECORD_MIN);
        let (gate, dir) = (rec.start(), rec.dir());
        assert!(gate.distance(Vec2::new(0.0, 80.0)) < 0.5 && dir.distance(Vec2::Y) < 1e-4, "{gate} {dir}");
        let (turn, dist) = RunTrackScreen::way_home(&rec, Vec3::ZERO, 0.0).expect("long enough");
        assert!(turn.abs() < 0.05 && (dist - 80.0).abs() < 0.5, "{turn} {dist}");
        // Facing away from it: turn right round.
        let (turn, _) = RunTrackScreen::way_home(&rec, Vec3::ZERO, std::f32::consts::PI).unwrap();
        assert!(turn.abs() > 3.0, "{turn}");
        // Just past the gate, facing on: step aside rather than back through it.
        let (turn, _) = RunTrackScreen::way_home(&rec, Vec3::new(2.0, 0.0, 90.0), 0.0).unwrap();
        assert!(turn.abs() > 1.2, "{turn}");
        // Far off to the side: aim behind the gate, not at it.
        let (turn, _) = RunTrackScreen::way_home(&rec, Vec3::new(-100.0, 0.0, 80.0), std::f32::consts::FRAC_PI_2).unwrap();
        assert!(turn < -0.1, "towards z < 80, on the left: {turn}");
        // Too short: no arrow yet.
        let short = Recorder::new(Vec2::ZERO, Vec2::Y);
        assert!(RunTrackScreen::way_home(&short, Vec3::ZERO, 0.0).is_none());
    }

    #[test]
    fn the_trail_lays_a_road_wide_patch_every_four_metres() {
        let mut screen = RunTrackScreen::new(&Profile::default());
        screen.recorder = Some(RunTrackScreen::fake_run(Vec3::ZERO, 0.0));
        screen.lay_trail();
        let len = screen.recorder.as_ref().unwrap().len();
        assert_eq!(screen.trail.len(), (len / DUST_EVERY) as usize);
        assert!(screen.trail.iter().all(|p| p.kind == DrillPropKind::Dust && p.size.x == 18.0 && p.pos.is_finite()));
        // Nothing twice, and the gate comes on top.
        screen.lay_trail();
        assert_eq!(screen.trail.len(), (len / DUST_EVERY) as usize);
        let props = screen.props();
        assert_eq!(props.iter().filter(|p| p.kind == DrillPropKind::Finish).count(), 1);
        assert_eq!(props.len(), screen.trail.len() + 1);
    }

    #[test]
    fn pause_restart_keeps_the_recorder_and_quit_returns_without_a_race_payout() {
        let profile = Profile::default();
        let before = profile.progress.clone();
        let dir = std::env::temp_dir().join(format!("stampede-record-controls-{}", std::process::id()));
        let mut engine = Engine::new(
            Box::new(crate::ui::screens::menu::RaceMenuScreen::new()),
            profile.clone(),
            dir.clone(),
            Options {
                shot: Some("run_track".into()),
                ..Options::default()
            },
            Box::new(RecordingPlatform::default()),
            Box::new(NoHaptics::default()),
        );
        engine.router.apply(Transition::Push(Box::new(RunTrackScreen::new(&profile))));
        frames(&mut engine, 60);
        engine.request_back();
        frames(&mut engine, 2);
        tap(&mut engine, 451.5);
        assert_eq!(engine.router.names(), ["race_menu", "run_track"]);
        engine.request_back();
        frames(&mut engine, 2);
        tap(&mut engine, 892.5);
        assert_eq!(engine.router.names(), ["race_menu"]);
        assert_eq!(engine.svc.profile.progress, before);
        assert!(crate::custom_tracks::list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    struct LoopSource {
        world: race_core::World,
        angle: f32,
    }

    impl super::super::source::RaceSource for LoopSource {
        fn view(&self) -> super::super::source::RaceView<'_> {
            let w = &self.world;
            super::super::source::RaceView {
                cfg: &w.cfg,
                track: &w.track,
                tun: &w.tun,
                racers: &w.racers,
                prev: &w.racers,
                alpha: 1.0,
                projectiles: &w.projectiles,
                hazards: &w.hazards,
                coins: &w.coins,
                loose_coins: &w.loose_coins,
                targets: &w.targets,
            play: &w.play,
                phase: w.phase,
                race_time: self.angle,
                countdown_left: 0.0,
                lesson: None,
                challenge: None,
                battle: None,
                lap_times: &w.lap_times,
                hurdles_active: false,
            }
        }
        fn local_seat(&self) -> Option<usize> {
            Some(0)
        }
        fn advance(&mut self, _dt: f32, _input: &mut dyn FnMut() -> race_core::Input) -> u32 {
            self.angle += 0.02;
            let r = &mut self.world.racers[0];
            r.pos = Vec3::new(130.0 * self.angle.cos(), 0.0, 130.0 * self.angle.sin());
            r.yaw = -self.angle;
            r.visual_yaw = r.yaw;
            1
        }
        fn drain_events(&mut self) -> Vec<race_core::Event> {
            vec![]
        }
    }

    #[test]
    fn closing_a_recorded_loop_opens_the_build_lobby_without_saving_or_paying() {
        let profile = Profile::default();
        let before = profile.progress.clone();
        let mut screen = RunTrackScreen::new(&profile);
        let mut launch = common::quick_launch(&profile, TrackId::Field, 1, 0, BotLevel::Easy);
        launch.config.mode = RaceMode::Solo;
        let mut world = race_core::World::new(launch.config.clone());
        world.phase = RacePhase::Racing;
        screen.race = race::with_source(launch, Staging::default(), Box::new(LoopSource { world, angle: 0.0 })).recording();
        let dir = std::env::temp_dir().join(format!("stampede-record-loop-{}", std::process::id()));
        let mut engine = Engine::new(
            Box::new(screen),
            profile,
            dir.clone(),
            Options {
                shot: Some("run_track".into()),
                ..Options::default()
            },
            Box::new(RecordingPlatform::default()),
            Box::new(NoHaptics::default()),
        );
        frames(&mut engine, 400);
        assert_eq!(engine.router.names(), ["track_build"]);
        assert_eq!(engine.svc.profile.progress, before);
        assert!(crate::custom_tracks::list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn solo_world() -> race_core::World {
        let profile = Profile::default();
        let mut launch = common::quick_launch(&profile, TrackId::Field, 1, 0, BotLevel::Easy);
        launch.config.mode = RaceMode::Solo;
        launch.config.le_mans = true;
        race_core::World::new(launch.config)
    }

    #[test]
    fn a_run_a_track_world_is_an_empty_field_with_the_runner_in_the_middle() {
        let w = solo_world();
        assert!(w.play.frogs.is_empty() && w.play.rugs.is_empty());
        assert!(w.hazards.is_empty() && w.coins.is_empty() && w.targets.is_empty() && w.loose_coins.is_empty());
        assert!(w.track.features.is_empty());
        assert!(!w.hurdles_active() || w.track.features.is_empty());
        let r = &w.racers[0];
        assert!(r.pos.x.hypot(r.pos.z) < 2.0, "starts at {:?}", r.pos);
        assert!(r.effects.chase_x.hypot(r.effects.chase_z) < 10.0, "the jockey starts close by");
        let screen = RunTrackScreen::new(&Profile::default());
        let (pos, _, _, _) = screen.race.record_view();
        assert!(pos.x.hypot(pos.z) < 2.0, "the recorder starts at {pos:?}");
        let mut recorder = Recorder::new(Vec2::new(pos.x, pos.z), Vec2::Y);
        assert!(recorder.start().length() < 2.0);
        recorder.push(Vec2::new(pos.x, pos.z));
    }

    #[test]
    fn the_run_a_track_ground_is_flat_and_open_out_to_the_rim() {
        let mut w = solo_world();
        let y0 = w.racers[0].pos.y;
        // Dry ground at the middle, and as far out as the rim.
        for r in [0.0f32, 10.0, 15.0, 80.0, 150.0, 250.0, 300.0] {
            let h = w.track.ground_at(&w.track.project(Vec3::new(r, y0, 0.0), 0), Vec3::new(r, y0, 0.0));
            assert!((h - y0).abs() < 0.05, "ground {h} at {r} m from the middle");
        }
        // Run straight on: nothing stops the runner for 12 s, nor lifts it off the flat.
        w.phase = RacePhase::Racing;
        let go = race_core::Input { throttle: 1.0, ..Default::default() };
        let mut far = 0.0f32;
        for _ in 0..(12 * 60) {
            w.step(&[go]);
            let r = &w.racers[0];
            far = far.max(r.pos.x.hypot(r.pos.z));
            assert!((r.pos.y - y0).abs() < 0.5, "left the ground at {:?}", r.pos);
        }
        assert!(far > 20.0, "only got {far} m from the middle");
    }
}
