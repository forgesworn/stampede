//! Drive & place: drive your own custom track alone and drop items where you are.

use super::race::{self, RaceScreen, Staging};
use crate::platform::data_dir::Profile;
use crate::render::ScenePass;
use crate::ui::icons;
use crate::ui::screens::designer::{feature_at, feature_icon, right_of, ITEMS};
use crate::ui::screens::{common, Ctx, Screen, Transition};
use crate::ui::{art, feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect, Sense, Stroke};
use race_core::custom::{self, CustomFeature, CustomFeatureKind as Kind, CustomTrack, Shaped};
use crate::flow::RaceLaunch;
use race_core::{BotLevel, RaceMode, Vec2, Vec3};
use race_render::{DrillPropInst, DrillPropKind};
use std::cell::RefCell;
use std::rc::Rc;

/// How far past the road's edge the runner may be and still place an item (m).
const EDGE_SLACK: f32 = 4.0;
/// Height of the bar along the top (points).
const BAR_H: f32 = 38.0;
/// Space kept clear at the right for the pause, camera and look-back buttons (points).
const CONTROLS_W: f32 = 150.0;

/// Where the design screen is told the finished item list.
pub type Handback = Rc<RefCell<Option<Vec<CustomFeature>>>>;

pub struct PlaceItemsScreen {
    race: RaceScreen,
    /// The track with everything placed so far.
    track: CustomTrack,
    shaped: Shaped,
    /// What this drive has dropped, with the ground height it was dropped at.
    placed: Vec<(CustomFeature, f32)>,
    result: Handback,
    sample: bool,
}

impl PlaceItemsScreen {
    pub fn new(profile: &Profile, track: CustomTrack, result: Handback) -> Result<Self, String> {
        let shaped = track.shaped().map_err(|e| e.to_string())?;
        let launch = Self::launch(profile, &track)?;
        Ok(Self { race: race::new(launch, Staging { mounted: true, ..Staging::default() }).recording().without_restart(), track, shaped, placed: Vec::new(), result, sample: false })
    }

    /// Alone on the track, with no lap count to run out: the sim never ends this drive (the
    /// mode Run a Track uses), only Done, Back or Quit does.
    fn launch(profile: &Profile, track: &CustomTrack) -> Result<RaceLaunch, String> {
        let mut launch = common::custom_launch(profile, track, true, 0, BotLevel::Normal)?;
        launch.config.mode = RaceMode::Solo;
        Ok(launch)
    }

    /// For the shot: three items already dropped just ahead of the start line.
    pub fn sample(profile: &Profile) -> Self {
        let mut s = Self::new(profile, crate::ui::screens::designer::sample_track("Sunset Loop"), Handback::default()).expect("sample track launches");
        let at = |s: &Self, along: f32, across: f32| {
            let q = s.shaped.point_at(along) + normal(&s.shaped, along) * across;
            Vec3::new(q.x, 0.0, q.y)
        };
        for (kind, along, across) in [(Kind::Boost, 22.0, 0.0), (Kind::Log, 42.0, 0.0), (Kind::Coins, 62.0, 3.0)] {
            let pos = at(&s, along, across);
            let _ = s.place(kind, pos);
        }
        s.sample = true;
        s
    }

    /// Drops `kind` at the ground position `pos`: on the road, with the track still valid and
    /// not over the feature limit. Returns the note to show.
    fn place(&mut self, kind: Kind, pos: Vec3) -> Result<String, String> {
        if self.track.features.len() >= custom::MAX_FEATURES {
            return Err("This track has enough features".into());
        }
        let at = Vec2::new(pos.x, pos.z);
        let (s, distance) = self.shaped.nearest(at);
        if distance > self.shaped.width * 0.5 + EDGE_SLACK {
            return Err("Get back on the road".into());
        }
        let s = s.rem_euclid(self.shaped.length);
        let feature = feature_at(&self.shaped, kind, s, at);
        let mut candidate = self.track.clone();
        candidate.features.push(feature);
        custom::to_def(&candidate).map_err(|e| e.to_string())?;
        self.track = candidate;
        self.placed.push((feature, pos.y));
        Ok(format!("{} placed", name_of(kind)))
    }

    /// Takes back the last item dropped this drive.
    fn undo(&mut self) -> bool {
        if self.placed.pop().is_some() {
            self.track.features.pop();
            true
        } else {
            false
        }
    }

    /// Hands the list back to the designer, if anything was placed.
    fn commit(&self) {
        if !self.placed.is_empty() {
            *self.result.borrow_mut() = Some(self.track.features.clone());
        }
    }

    /// A marker in the world for each item dropped this drive.
    fn props(&self) -> Vec<DrillPropInst> {
        self.placed
            .iter()
            .map(|(f, y)| {
                let s = f.s();
                let n = normal(&self.shaped, s);
                let p = self.shaped.point_at(s) + n * f.lateral();
                let yaw = race_core::math::yaw_of(Vec3::new(-n.y, 0.0, n.x));
                let (kind, size) = match f.kind {
                    Kind::Eggs => (DrillPropKind::Cone, Vec3::new(1.6, 3.4, 1.6)),
                    Kind::Ramp => (DrillPropKind::Pylon, Vec3::new(1.6, 5.0, 1.6)),
                    Kind::Gap => (DrillPropKind::RingCone, Vec3::new(2.4, 2.4, 2.4)),
                    Kind::Log => (DrillPropKind::Log, Vec3::new(10.0, 1.1, 1.1)),
                    Kind::Boost => (DrillPropKind::Flag, Vec3::new(2.6, 6.0, 1.8)),
                    Kind::Coins => (DrillPropKind::Flag, Vec3::new(1.8, 4.0, 1.2)),
                };
                DrillPropInst { kind, pos: Vec3::new(p.x, *y, p.y), yaw, knocked: 0.0, size }
            })
            .collect()
    }

    fn finish(&mut self) -> Transition {
        self.commit();
        self.race.stop_recording();
        Transition::Pop
    }

    /// The bar along the top: six items, Undo, Done.
    fn bar(safe: Rect) -> (Vec<Rect>, Rect, Rect) {
        let left = safe.left() + 8.0;
        let right = (safe.right() - CONTROLS_W).max(left + 200.0);
        let (gap, undo_w, done_w) = (4.0, 54.0, 60.0);
        let item_w = (right - left - gap * 7.0 - undo_w - done_w) / ITEMS.len() as f32;
        let y = safe.top() + 8.0;
        let mut x = left;
        let mut next = |w: f32| {
            let r = Rect::from_min_size(pos2(x, y), vec2(w, BAR_H));
            x += w + gap;
            r
        };
        let items = (0..ITEMS.len()).map(|_| next(item_w)).collect();
        let undo = next(undo_w);
        let done = next(done_w);
        (items, undo, done)
    }
}

fn normal(shaped: &Shaped, s: f32) -> Vec2 {
    right_of(shaped, s)
}

fn name_of(kind: Kind) -> &'static str {
    ITEMS.iter().find(|(k, _)| *k == kind).map_or("Item", |(_, n)| n)
}

impl Screen for PlaceItemsScreen {
    fn orientation(&self) -> crate::platform::os::Orientation {
        crate::platform::os::Orientation::Landscape
    }

    fn name(&self) -> &'static str {
        if self.sample {
            "place_items_sample"
        } else {
            "place_items"
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
    fn back(&mut self, _ctx: &mut Ctx) -> Transition {
        self.finish()
    }
    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        let transition = self.race.update(ctx);
        if !matches!(transition, Transition::None) {
            self.commit();
            self.race.stop_recording();
            return transition;
        }
        self.race.record_hud(self.props());
        Transition::None
    }
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let transition = self.race.draw(ctx, ui);
        if !matches!(transition, Transition::None) {
            self.commit();
            self.race.stop_recording();
            return transition;
        }
        if self.race.record_view().3 {
            return Transition::None;
        }
        let safe = ctx.viewport.safe;
        let (items, undo, done) = Self::bar(safe);
        let mut out = Transition::None;
        let mut chosen = None;
        let mut take_back = false;
        egui::Area::new(egui::Id::new("place-bar")).order(egui::Order::Foreground).fixed_pos(items[0].min).show(ui, |ui| {
            let backdrop = items[0].union(done).expand(4.0);
            ui.painter().rect_filled(backdrop, 12.0, Color32::from_black_alpha(185));
            for (i, (kind, name)) in ITEMS.into_iter().enumerate() {
                let rect = items[i];
                let resp = ui.interact(rect, egui::Id::new(("place-item", i)), Sense::click());
                let p = ui.painter();
                let fill = if resp.is_pointer_button_down_on() { theme::ACCENT } else { Color32::from_rgba_unmultiplied(110, 60, 170, 140) };
                p.rect_filled(rect, 9.0, fill);
                p.rect_stroke(rect, 9.0, Stroke::new(1.0_f32, theme::OUTLINE), egui::StrokeKind::Inside);
                let size = art::fit_size(p, name, theme::title, 14.0, rect.width() - 30.0);
                icons::draw(p, feature_icon(kind), pos2(rect.left() + 13.0, rect.center().y), 15.0, theme::SAND);
                p.text(pos2(rect.left() + 24.0 + (rect.width() - 28.0) * 0.5, rect.center().y), Align2::CENTER_CENTER, name, theme::title(size), theme::TEXT);
                if resp.clicked() {
                    chosen = Some(kind);
                }
            }
            for (rect, label, id, enabled) in [(undo, "Undo", "place-undo", !self.placed.is_empty()), (done, "Done", "place-done", true)] {
                let resp = ui.interact(rect, egui::Id::new(id), if enabled { Sense::click() } else { Sense::hover() });
                let p = ui.painter();
                let primary = label == "Done";
                let fill = if primary { theme::ACCENT } else { Color32::from_rgba_unmultiplied(64, 38, 100, 230) };
                let alpha = if enabled { 1.0 } else { 0.4 };
                p.rect_filled(rect, 9.0, if resp.is_pointer_button_down_on() { fill.gamma_multiply(0.8) } else { fill }.gamma_multiply(alpha));
                let ink = if primary { Color32::from_rgb(40, 18, 8) } else { theme::TEXT };
                p.text(rect.center(), Align2::CENTER_CENTER, label, theme::title(art::fit_size(p, label, theme::title, 15.0, rect.width() - 8.0)), ink.gamma_multiply(alpha));
                if resp.clicked() {
                    if primary {
                        out = self.finish();
                    } else {
                        take_back = true;
                    }
                }
            }
        });
        if take_back && self.undo() {
            feedback::tap(ctx);
            ctx.say("Last item taken back");
        }
        if let Some(kind) = chosen {
            let (pos, ..) = self.race.record_view();
            match self.place(kind, pos) {
                Ok(note) => {
                    feedback::tap(ctx);
                    ctx.say(note);
                }
                Err(e) => ctx.say(e),
            }
        }
        let text = match self.placed.len() {
            0 => "Tap an item to drop it here".to_string(),
            n => format!("Tap an item to drop it here  ·  {n} placed"),
        };
        let p = ui.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("place-hud")));
        super::hud::shadow_text(&p, pos2((items[0].left() + done.right()) * 0.5, items[0].bottom() + 14.0), Align2::CENTER_CENTER, &text, theme::ui_medium(12.0), theme::TEXT);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Options;
    use crate::engine::Engine;
    use crate::haptics::NoHaptics;
    use crate::platform::os::RecordingPlatform;
    use crate::platform::TouchPhase;
    use crate::ui::screens::designer::sample_track;

    fn screen() -> (PlaceItemsScreen, Handback) {
        let result = Handback::default();
        (PlaceItemsScreen::new(&Profile::default(), sample_track("Placing"), result.clone()).expect("launches"), result)
    }

    /// The ground position `along` m round the lap and `across` m to the right of the centreline.
    fn spot(s: &PlaceItemsScreen, along: f32, across: f32) -> Vec3 {
        let q = s.shaped.point_at(along) + normal(&s.shaped, along) * across;
        Vec3::new(q.x, 1.5, q.y)
    }

    #[test]
    fn an_item_lands_where_the_runner_is() {
        let (mut s, _) = screen();
        let before = s.track.features.len();
        // Centred kinds ignore the offset; the position along the road is the runner's.
        let note = s.place(Kind::Boost, spot(&s, 123.4, 2.0)).unwrap();
        assert_eq!(note, "Boost placed");
        let f = *s.track.features.last().unwrap();
        assert_eq!(s.track.features.len(), before + 1);
        assert_eq!((f.kind, f.lateral_dm), (Kind::Boost, 0));
        assert!((f.s_dm as i32 - 1234).abs() <= 2, "{}", f.s_dm);
        // Coins keep the lateral offset...
        let at = spot(&s, 300.0, 3.0);
        s.place(Kind::Coins, at).unwrap();
        let f = *s.track.features.last().unwrap();
        assert!((f.s_dm as i32 - 3000).abs() <= 2 && (f.lateral_dm - 30).abs() <= 2, "{f:?}");
        // ...clamped to the road, when the runner is on the verge.
        let at = spot(&s, 450.0, -11.0);
        s.place(Kind::Coins, at).unwrap();
        assert_eq!(s.track.features.last().unwrap().lateral_dm, -90);
        // Each is a marker at the same spot, at the ground height it was dropped at.
        let props = s.props();
        assert_eq!(props.len(), 3);
        assert!(props.iter().all(|p| p.pos.y == 1.5 && p.pos.is_finite()));
        let q = s.shaped.point_at(123.4);
        assert!(Vec2::new(props[0].pos.x, props[0].pos.z).distance(q) < 0.3);
        assert!(custom::to_def(&s.track).is_ok());
    }

    #[test]
    fn the_drive_never_ends_on_its_own() {
        let launch = PlaceItemsScreen::launch(&Profile::default(), &sample_track("Placing")).unwrap();
        assert_eq!(launch.config.mode, RaceMode::Solo);
        // With every lap run, a time trial finishes the runner and this mode does not.
        let finished_after_laps = |mode| {
            let mut config = launch.config.clone();
            config.mode = mode;
            let mut world = race_core::World::new(config);
            for _ in 0..60 * 8 {
                world.step(&[race_core::Input::default()]);
            }
            world.racers[0].lap = launch.config.laps + 1;
            for _ in 0..3 {
                world.step(&[race_core::Input::default()]);
            }
            world.racers[0].finished_at.is_some()
        };
        assert!(finished_after_laps(RaceMode::TimeTrial), "the check can see a finish");
        assert!(!finished_after_laps(RaceMode::Solo));
    }

    #[test]
    fn coins_land_on_the_side_of_the_road_the_runner_stood() {
        for (along, across) in [(150.0, 3.5), (320.0, -3.5), (480.0, 2.0)] {
            let (mut s2, _) = screen();
            s2.track.features.clear();
            // The runner stands `across` m to the right of the road, found with the world's own right.
            let def = custom::to_def(&s2.track).unwrap();
            let world = race_core::Track::build(&def);
            let stood = world.point_at(along, across);
            s2.place(Kind::Coins, stood).unwrap();
            let coins = *s2.track.features.last().unwrap();
            let def = custom::to_def(&s2.track).unwrap();
            let world = race_core::Track::build(&def);
            let line = def.features.iter().find_map(|f| match *f {
                race_core::Feature::CoinLine { s, lateral, .. } => Some((s, lateral)),
                _ => None,
            }).expect("a coin line");
            assert_eq!(line.1.signum(), across.signum(), "{coins:?}");
            let at = world.point_at(line.0, line.1);
            let distance = Vec2::new(at.x, at.z).distance(Vec2::new(stood.x, stood.z));
            assert!(distance < 1.0, "coins {distance} m from where the runner stood ({coins:?})");
        }
    }

    #[test]
    fn off_the_road_places_nothing() {
        let (mut s, _) = screen();
        let before = s.track.clone();
        // Past half the width plus the slack.
        let err = s.place(Kind::Eggs, spot(&s, 200.0, s.shaped.width * 0.5 + EDGE_SLACK + 1.0)).unwrap_err();
        assert_eq!(err, "Get back on the road");
        assert!(s.place(Kind::Eggs, Vec3::new(0.0, 0.0, 0.0)).is_err());
        assert_eq!(s.track, before);
        assert!(s.placed.is_empty() && s.props().is_empty());
        // Just inside the slack still counts.
        assert!(s.place(Kind::Eggs, spot(&s, 200.0, s.shaped.width * 0.5 + EDGE_SLACK - 0.5)).is_ok());
    }

    #[test]
    fn an_invalid_candidate_or_a_full_track_refuses_the_item() {
        let (mut s, _) = screen();
        // A candidate that does not make a valid track is dropped, with the reason, and the
        // track is left as it was.
        s.track.points.truncate(10);
        let broken = s.track.clone();
        let err = s.place(Kind::Gap, spot(&s, 520.0, 0.0)).unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(s.track, broken);
        assert!(s.placed.is_empty());
        // The feature limit.
        let (mut s, _) = screen();
        s.track.features = vec![CustomFeature { kind: Kind::Eggs, s_dm: 100, lateral_dm: 0 }; custom::MAX_FEATURES];
        let before = s.track.clone();
        assert_eq!(s.place(Kind::Boost, spot(&s, 50.0, 0.0)).unwrap_err(), "This track has enough features");
        assert_eq!(s.track, before);
    }

    #[test]
    fn undo_takes_back_only_this_drives_last_item() {
        let (mut s, _) = screen();
        let original = s.track.features.clone();
        assert!(!s.undo(), "nothing placed: the track's own items stay");
        assert_eq!(s.track.features, original);
        let a = spot(&s, 150.0, 0.0);
        let b = spot(&s, 350.0, 0.0);
        s.place(Kind::Log, a).unwrap();
        s.place(Kind::Ramp, b).unwrap();
        assert!(s.undo());
        assert_eq!(s.track.features.len(), original.len() + 1);
        assert_eq!(s.track.features.last().unwrap().kind, Kind::Log);
        assert_eq!(s.props().len(), 1);
        assert!(s.undo() && !s.undo());
        assert_eq!(s.track.features, original);
    }

    #[test]
    fn done_hands_back_the_whole_list_and_nothing_placed_hands_back_nothing() {
        let (mut s, result) = screen();
        assert!(matches!(s.finish(), Transition::Pop));
        assert!(result.borrow().is_none());
        let (mut s, result) = screen();
        let at = spot(&s, 250.0, 0.0);
        s.place(Kind::Log, at).unwrap();
        assert!(matches!(s.finish(), Transition::Pop));
        assert_eq!(result.borrow().as_ref(), Some(&s.track.features));
        assert_eq!(result.borrow().as_ref().unwrap().len(), sample_track("Placing").features.len() + 1);
    }

    fn frames(engine: &mut Engine, n: usize) {
        for _ in 0..n {
            engine.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        }
    }

    fn tap(engine: &mut Engine, at: egui::Pos2) {
        engine.touch_event(9, TouchPhase::Started, (at.x as f64 * 3.0, at.y as f64 * 3.0));
        frames(engine, 2);
        engine.touch_event(9, TouchPhase::Ended, (at.x as f64 * 3.0, at.y as f64 * 3.0));
        frames(engine, 4);
    }

    #[test]
    fn the_bar_drops_an_item_where_the_ostrich_stands_and_done_hands_it_back() {
        let (screen, result) = screen();
        let before = screen.track.features.len();
        let dir = std::env::temp_dir().join(format!("stampede-place-bar-{}", std::process::id()));
        let mut engine = Engine::new(
            Box::new(crate::ui::screens::menu::RaceMenuScreen::new()),
            Profile::default(),
            dir.clone(),
            Options { shot: Some("place_items".into()), ..Options::default() },
            Box::new(RecordingPlatform::default()),
            Box::new(NoHaptics::default()),
        );
        engine.router.apply(Transition::Push(Box::new(screen)));
        frames(&mut engine, 30);
        let safe = engine.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default()).viewport.safe;
        let (items, undo, done) = PlaceItemsScreen::bar(safe);
        // Boost, then Coins, then take the coins back, then Done.
        tap(&mut engine, items[4].center());
        tap(&mut engine, items[5].center());
        tap(&mut engine, undo.center());
        assert_eq!(engine.router.names(), ["race_menu", "place_items"]);
        assert!(result.borrow().is_none(), "nothing is handed back before Done");
        tap(&mut engine, done.center());
        frames(&mut engine, 30);
        assert_eq!(engine.router.names(), ["race_menu"]);
        let features = result.borrow().clone().expect("handed back");
        assert_eq!(features.len(), before + 1);
        let boost = features.last().unwrap();
        assert_eq!((boost.kind, boost.lateral_dm), (Kind::Boost, 0));
        let _ = std::fs::remove_dir_all(dir);
    }
}
