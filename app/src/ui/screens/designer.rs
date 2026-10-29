//! Plan editor and the short build lobby for a recorded Paddock run.

use super::common;
use super::{Ctx, Screen, Transition};
use crate::ui::art::{self, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Btn, Seg};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect, Sense, Shape, Stroke};
use race_core::custom::{self, CustomFeature, CustomFeatureKind as Kind, CustomTrack, ShapeError, Shaped};
use race_core::{BotLevel, TrackId, Vec2};
use std::cell::RefCell;
use std::rc::Rc;

/// `Move` is the Shape tool in the rail.
#[derive(Clone, Copy, PartialEq)]
enum Tool { Draw, Move, Start, Remove, Feature(Kind) }

/// A button in the footer.
#[derive(Clone, Copy, PartialEq)]
enum Act { Test, Race, Save, Invite, Place, Edit, Discard }

/// The item kinds in the Items strip, with their short names.
pub(crate) const ITEMS: [(Kind, &str); 6] = [(Kind::Eggs, "Eggs"), (Kind::Ramp, "Jump"), (Kind::Gap, "Gap"), (Kind::Log, "Log"), (Kind::Boost, "Boost"), (Kind::Coins, "Coins")];

/// Height of the Items strip above the plan, with its margins (points).
const ITEM_STRIP_H: f32 = 50.0;

/// Half the length of road redrawn over a crossing to show the bridge (m).
const BRIDGE_REACH: f32 = 25.0;

/// An error message opening a sentence.
fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// A Draw stroke starting this close to an end of an open line (screen points)
/// carries the line on instead of being ignored.
const JOIN_REACH_PX: f32 = 40.0;

#[derive(Clone)]
struct Draft {
    raw: Vec<Vec2>,
    track: CustomTrack,
    shaped: Option<Shaped>,
    error: Option<ShapeError>,
}

pub struct DesignerScreen {
    t: f32,
    draft: Draft,
    undo: Vec<Draft>,
    tool: Tool,
    /// The item kind Items falls back to when it is switched on again.
    last_kind: Kind,
    centre: Vec2,
    scale: f32,
    needs_fit: bool,
    drawing: bool,
    moving: Option<(Draft, usize, Vec2)>,
    gesture: Option<(egui::Pos2, f32)>,
    block_stroke: bool,
    build: bool,
    name_edit: Option<String>,
    /// Asking whether to leave with unsaved work.
    leave_ask: bool,
    saved: Option<u64>,
    /// Where Drive & place leaves the items it dropped, for `update` to adopt.
    placed: Rc<RefCell<Option<Vec<CustomFeature>>>>,
}

impl Default for DesignerScreen { fn default() -> Self { Self::new() } }

impl DesignerScreen {
    pub fn new() -> Self {
        let track = CustomTrack { v: 1, name: "My Track".into(), look: TrackId::Savanna, laps: 3, width_dm: 180, points: vec![], features: vec![], seed: common::fresh_seed() as u32, hills: 0 };
        Self { t: 0.0, draft: Draft { raw: vec![], track, shaped: None, error: None }, undo: vec![], tool: Tool::Draw, last_kind: Kind::Eggs, centre: Vec2::ZERO, scale: 1.0, needs_fit: false, drawing: false, moving: None, gesture: None, block_stroke: false, build: false, name_edit: None, leave_ask: false, saved: None, placed: Rc::default() }
    }

    pub fn edit(track: CustomTrack) -> Self {
        let mut s = Self::new();
        s.saved = Some(track.hash());
        s.tool = Tool::Move;
        s.draft.raw = track.points_m();
        s.draft.shaped = track.shaped().ok();
        s.draft.track = track;
        s.needs_fit = true;
        s
    }

    pub fn from_run(track: CustomTrack) -> Self {
        let mut s = Self::edit(track);
        s.saved = None;
        s.build = true;
        s
    }

    /// A recorded run that could not be shaped: shows the line with the
    /// problem marked, so the run is not thrown away.
    pub fn from_failed_run(raw: Vec<Vec2>, name: String) -> Self {
        let mut s = Self::new();
        s.draft.track.name = name;
        s.draft.raw = raw;
        s.reshape(true);
        // A closed run that breaks a shape rule is still a loop: the Shape nodes pull it right.
        if let Some(shaped) = &s.draft.shaped { s.draft.raw = shaped.points.clone(); s.tool = Tool::Move; }
        s.needs_fit = true;
        s
    }

    pub fn sample() -> Self { Self::edit(sample_track("Sunset Loop")) }
    pub fn hills_sample() -> Self {
        let mut track = sample_track("Sunset Loop");
        track.hills = 2;
        Self::edit(track)
    }
    pub fn build_sample() -> Self { Self::from_run(sample_track("Run 1")) }
    /// A figure of eight: one bridge, where the upper road passes over the lower.
    pub fn bridge_sample() -> Self {
        let raw: Vec<_> = (0..=160).map(|i| { let a = i as f32 / 160.0 * std::f32::consts::TAU; Vec2::new(a.cos() * 140.0, a.sin() * a.cos() * 190.0) }).collect();
        let shaped = custom::shape(&raw, false, 18.0).expect("bridge loop");
        let mut s = Self::edit(custom::custom_track_from(&shaped, "Figure of Eight", TrackId::Savanna, 3, 180, custom::auto_fill(&shaped, 7), 7));
        s.tool = Tool::Feature(Kind::Eggs);
        s
    }
    /// The Items strip open, as it looks mid-edit.
    pub fn items_sample() -> Self {
        let mut s = Self::sample();
        s.tool = Tool::Feature(Kind::Boost);
        s.last_kind = Kind::Boost;
        s
    }
    /// A closed loop whose two long sides run 10 m apart: the road is drawn closed with the
    /// marked spot, and the Shape nodes show so it can be pulled apart.
    pub fn close_sample() -> Self { Self::from_failed_run(pinched_loop(), "Run 2".into()) }
    pub fn error_sample() -> Self {
        let mut s = Self::new();
        s.draft.raw = vec![Vec2::new(-120.0, -70.0), Vec2::new(20.0, -100.0), Vec2::new(130.0, 45.0)];
        s.reshape(false);
        s
    }

    /// Takes on the items a Drive & place left behind, as one step to undo.
    fn adopt_placed(&mut self) {
        let Some(features) = self.placed.borrow_mut().take() else { return };
        if features != self.draft.track.features {
            self.remember();
            self.draft.track.features = features;
        }
    }

    fn remember(&mut self) {
        self.undo.push(self.draft.clone());
        if self.undo.len() > 40 { self.undo.remove(0); }
    }

    fn fit(&mut self, rect: Rect) {
        if self.draft.raw.is_empty() { return; }
        let (mut lo, mut hi) = (self.draft.raw[0], self.draft.raw[0]);
        for p in &self.draft.raw { lo = lo.min(*p); hi = hi.max(*p); }
        self.centre = (lo + hi) * 0.5;
        let extent = hi - lo;
        let padding = self.draft.track.width() + 45.0;
        self.scale = (800.0 / (extent.x + padding)).min(800.0 * rect.height() / rect.width() / (extent.y + padding)).clamp(0.4, 5.0);
    }

    fn reshape(&mut self, close: bool) {
        match custom::shape_lenient(&self.draft.raw, close, self.draft.track.width()) {
            // A closed loop that breaks a shape rule stays a loop to reshape, but has no track points,
            // so nothing that needs a valid track can use it until the error clears.
            Ok((shaped, error)) => {
                self.draft.track.points = if error.is_none() { custom::custom_track_from(&shaped, "x", self.draft.track.look, self.draft.track.laps, self.draft.track.width_dm, vec![], 1).points } else { vec![] };
                self.draft.shaped = Some(shaped);
                self.draft.error = error;
            }
            Err(e) => { self.draft.track.points.clear(); self.draft.shaped = None; self.draft.error = Some(e); }
        }
    }

    /// Starts a Draw stroke at `at`. An empty plan starts a new line; a stroke
    /// near either end of an open line carries it on (from the start end, the
    /// line is reversed first). Anything else leaves the line alone.
    fn begin_stroke(&mut self, at: Vec2, reach: f32) -> Result<(), &'static str> {
        // A line of fewer than two points is a stray tap, not a line to carry on: start afresh.
        if self.draft.raw.len() < 2 {
            self.remember();
            self.draft.raw.clear();
            self.draft.track.points.clear(); self.draft.track.features.clear(); self.draft.shaped = None; self.draft.error = None;
            self.draft.raw.push(at);
        } else if self.draft.shaped.is_some() {
            return Err("Your loop is closed. Shape to bend it, or Clear to draw a new one");
        } else if self.draft.raw.last().is_some_and(|end| end.distance(at) <= reach) {
            self.remember();
        } else if self.draft.raw[0].distance(at) <= reach {
            self.remember();
            self.draft.raw.reverse();
        } else {
            return Err("Carry on from the end of your line, or Clear to start again");
        }
        self.drawing = true;
        Ok(())
    }

    fn begin_move(&mut self, at: Vec2, reach: f32) {
        let Some(shaped) = &self.draft.shaped else { return };
        let Some((index, point)) = shaped.points.iter().enumerate()
            .min_by(|(_, a), (_, b)| a.distance_squared(at).total_cmp(&b.distance_squared(at))) else { return };
        if point.distance(at) > reach { return; }
        let baseline = self.draft.clone();
        self.remember();
        self.moving = Some((baseline, index, at));
    }

    fn move_line(&mut self, at: Vec2) {
        let Some((baseline, index, start)) = self.moving.clone() else { return };
        let Some(old) = &baseline.shaped else { return };
        let delta = at - start;
        let reach = (70.0 + delta.length()).min(old.length * 0.25);
        let n = old.points.len();
        let raw: Vec<_> = old.points.iter().enumerate().map(|(i, point)| {
            let steps = i.abs_diff(index).min(n - i.abs_diff(index));
            let u = (steps as f32 * old.spacing() / reach).min(1.0);
            *point + delta * (1.0 - u * u).powi(3)
        }).collect();
        self.draft = baseline.clone();
        self.draft.raw = raw;
        self.reshape(true);
        if let Some(shaped) = &self.draft.shaped {
            // Keep each feature at the same fraction of the lap as the road changes.
            for feature in &mut self.draft.track.features {
                feature.s_dm = (feature.s() / old.length * shaped.length * 10.0).round() as u16;
            }
        }
    }

    fn finish_move(&mut self, ctx: &mut Ctx) {
        if let Err(reason) = self.settle_move() { ctx.say(reason); }
    }

    /// Ends a drag: keeps the loop if it is valid, or was already faulty and still a loop;
    /// otherwise puts it back and says why.
    fn settle_move(&mut self) -> Result<(), String> {
        let Some((baseline, _, _)) = self.moving.take() else { return Ok(()) };
        if self.ready() {
            self.draft.raw = self.draft.track.points_m();
        } else if baseline.error.is_some() && self.draft.shaped.is_some() {
            // The loop was already faulty: drop it where it is and check it again.
            if let Some(shaped) = &self.draft.shaped { self.draft.raw = shaped.points.clone(); }
        } else {
            let reason = self.draft.error.as_ref().map(ToString::to_string)
                .unwrap_or_else(|| "A feature needs more room. Move it first.".into());
            self.draft = baseline;
            self.undo.pop();
            return Err(reason);
        }
        Ok(())
    }

    fn recheck_width(&mut self) {
        if self.draft.track.points.is_empty() { self.reshape(false); return; }
        match self.draft.track.shaped() {
            Ok(shaped) => { self.draft.shaped = Some(shaped); self.draft.error = None; },
            Err(custom::CustomError::Shape(_)) => { self.draft.raw = self.draft.track.points_m(); self.reshape(false); },
            Err(_) => { self.draft.shaped = None; },
        }
    }

    /// Hill heights along the shaped road, when the track has hills.
    fn hill_profile(&self) -> Option<Vec<f32>> {
        let shaped = self.draft.shaped.as_ref().filter(|_| self.draft.error.is_none())?;
        (self.draft.track.hills > 0).then(|| custom::hill_heights(&self.draft.track, shaped))
    }

    /// A line is drawn and the track is not what was last saved (or was never saved).
    fn unsaved(&self) -> bool { !self.draft.raw.is_empty() && self.saved != Some(self.draft.track.hash()) }

    fn ready(&self) -> bool { self.draft.shaped.is_some() && self.draft.error.is_none() && custom::to_def(&self.draft.track).is_ok() }

    fn save(&mut self, ctx: &mut Ctx) {
        let result = match self.saved {
            Some(old_hash) => crate::custom_tracks::replace(ctx.data_dir, old_hash, &self.draft.track),
            None => crate::custom_tracks::save(ctx.data_dir, &self.draft.track),
        };
        match result {
            Ok(hash) => { self.saved = Some(hash); ctx.say("Saved to My Tracks"); feedback::select(ctx); }
            Err(e) => ctx.say(e),
        }
    }

    fn action(&mut self, ctx: &mut Ctx, act: Act) -> Transition {
        if let Some(error) = &self.draft.error {
            if !matches!(act, Act::Edit | Act::Discard) { ctx.say(format!("Fix the marked spot first: {error}")); return Transition::None; }
        }
        match act {
            Act::Test | Act::Race => match common::custom_launch(ctx.profile, &self.draft.track, act == Act::Test, 7, BotLevel::Normal) {
                Ok(launch) => { feedback::select(ctx); Transition::Push(crate::game::race_screen(launch)) }
                Err(e) => { ctx.say(e); Transition::None }
            },
            Act::Save => { self.save(ctx); Transition::None }
            Act::Invite => super::lobby::LobbyScreen::invite_custom(ctx, &self.draft.track),
            Act::Place => match crate::game::placing::PlaceItemsScreen::new(ctx.profile, self.draft.track.clone(), self.placed.clone()) {
                Ok(screen) => { feedback::select(ctx); Transition::Push(Box::new(screen)) }
                Err(e) => { ctx.say(e); Transition::None }
            },
            Act::Edit => { self.build = false; self.tool = Tool::Move; Transition::None }
            Act::Discard => Transition::Pop,
        }
    }

    /// Moves the start line to the road at `at` (`s` m along it), or reverses
    /// the direction when `at` is on the flag. Keeps the features and hills.
    fn move_start(&mut self, at: Vec2, s: f32) -> Result<(), String> {
        let Some(shaped) = self.draft.shaped.as_ref() else { return Err("Close the loop first".into()) };
        if self.draft.error.is_some() { return Err("Fix the marked spot first".into()); }
        let idx = ((s / shaped.spacing()).round() as usize) % shaped.points.len();
        let flip = at.distance(shaped.points[0]) < 16.0;
        let positions: Vec<_> = self.draft.track.features.iter().map(|f| (f.kind, shaped.point_at(f.s()), if flip { -f.lateral_dm } else { f.lateral_dm })).collect();
        let next = shaped.restart(if flip { 0 } else { idx }, flip).map_err(|e| e.to_string())?;
        let features: Vec<_> = positions.iter().map(|(kind, p, lateral_dm)| CustomFeature { kind: *kind, s_dm: (next.nearest(*p).0 * 10.0).round() as u16, lateral_dm: *lateral_dm }).collect();
        let mut candidate = custom::custom_track_from(&next, self.draft.track.name.clone(), self.draft.track.look, self.draft.track.laps, self.draft.track.width_dm, features, self.draft.track.seed);
        candidate.hills = self.draft.track.hills;
        custom::to_def(&candidate).map_err(|e| e.to_string())?;
        self.remember();
        self.draft.raw = next.points.clone();
        self.draft.track = candidate;
        self.draft.shaped = Some(next);
        self.draft.error = None;
        Ok(())
    }

    fn road_tap(&mut self, at: Vec2, ctx: &mut Ctx) {
        let Some(shaped) = self.draft.shaped.as_ref() else { ctx.say("Close the loop first"); return };
        if self.draft.error.is_some() { ctx.say("Fix the marked spot first"); return; }
        let (s, distance) = shaped.nearest(at);
        if distance > shaped.width * 0.5 + 8.0 { ctx.say("Tap on the road"); return; }
        if self.tool == Tool::Start {
            if let Err(e) = self.move_start(at, s) { ctx.say(e); }
        } else if self.tool == Tool::Remove {
            if let Some(i) = self.draft.track.features.iter().enumerate()
                .min_by(|(_, a), (_, b)| feature_point(shaped, a).distance(at).total_cmp(&feature_point(shaped, b).distance(at)))
                .filter(|(_, f)| feature_point(shaped, f).distance(at) < 6.0).map(|(i, _)| i)
            {
                self.remember();
                self.draft.track.features.remove(i);
            }
        } else if let Tool::Feature(kind) = self.tool {
            if self.draft.track.features.len() >= custom::MAX_FEATURES { ctx.say("This track has enough features"); return; }
            let feature = feature_at(shaped, kind, s, at);
            self.remember();
            self.draft.track.features.push(feature);
        }
    }

    fn canvas(&mut self, ctx: &mut Ctx, ui: &mut egui::Ui, rect: Rect) {
        if self.needs_fit { self.fit(rect); self.needs_fit = false; }
        let p = ui.painter().with_clip_rect(rect);
        p.rect_filled(rect, 14.0, Color32::from_rgb(44, 59, 43));
        widgets::well(&p, rect, 14.0);
        let metres = 800.0 / self.scale;
        let k = rect.width() / metres;
        let to_screen = |v: Vec2, centre: Vec2| rect.center() + vec2(-(v.x - centre.x) * k, -(v.y - centre.y) * k);
        let to_world = |v: egui::Pos2, centre: Vec2| centre - Vec2::new(v.x - rect.center().x, v.y - rect.center().y) / k;
        let resp = ui.interact(rect, ui.id().with("designer-plan"), Sense::click_and_drag());
        let fingers: Vec<_> = ctx.touches.active().iter().filter(|f| !self.leave_ask && rect.contains(f.start)).collect();
        if fingers.len() >= 2 {
            let midpoint = fingers[0].pos + (fingers[1].pos - fingers[0].pos) * 0.5;
            let distance = fingers[0].pos.distance(fingers[1].pos).max(1.0);
            if let Some((previous, span)) = self.gesture {
                self.centre += Vec2::new(midpoint.x - previous.x, midpoint.y - previous.y) / k;
                let anchor = to_world(midpoint, self.centre);
                self.scale = (self.scale * distance / span).clamp(0.4, 5.0);
                let new_k = rect.width() * self.scale / 800.0;
                self.centre = anchor + Vec2::new(midpoint.x - rect.center().x, midpoint.y - rect.center().y) / new_k;
            }
            self.gesture = Some((midpoint, distance));
            if self.drawing {
                if let Some(previous) = self.undo.pop() { self.draft = previous; }
                self.drawing = false;
            }
            if let Some((baseline, _, _)) = self.moving.take() {
                self.draft = baseline;
                self.undo.pop();
            }
            self.block_stroke = true;
        } else {
            self.gesture = None;
            if self.block_stroke && fingers.is_empty() { self.block_stroke = false; }
            if !self.block_stroke && !self.build {
                if self.tool == Tool::Move && resp.drag_started() {
                    let start = fingers.first().map(|finger| finger.start)
                        .or_else(|| resp.interact_pointer_pos().map(|at| at - resp.drag_delta()));
                    if let Some(start) = start {
                        self.begin_move(to_world(start, self.centre), self.draft.track.width() * 0.5 + 14.0 / k);
                    }
                }
                if self.moving.is_some() {
                    if let Some(q) = resp.interact_pointer_pos() { self.move_line(to_world(q, self.centre)); }
                    if resp.drag_stopped() { self.finish_move(ctx); }
                }
                if self.tool == Tool::Draw && resp.drag_started() {
                    let start = fingers.first().map(|finger| finger.start)
                        .or_else(|| resp.interact_pointer_pos().map(|at| at - resp.drag_delta()));
                    if let Some(start) = start {
                        let reach = (JOIN_REACH_PX / k).max(self.draft.track.width() * 0.5);
                        if let Err(e) = self.begin_stroke(to_world(start, self.centre), reach) { ctx.say(e); }
                    }
                }
                if self.drawing {
                    if let Some(q) = resp.interact_pointer_pos() {
                        let at = to_world(q, self.centre);
                        if self.draft.raw.last().is_none_or(|last| last.distance(at) >= 2.0) && self.draft.raw.len() < 1800 {
                            self.draft.raw.push(at);
                            if self.draft.raw.len() > 8 { self.reshape(false); }
                        }
                    }
                    if resp.drag_stopped() {
                        self.drawing = false;
                        self.reshape(false);
                        // A closed loop is done with: switch to Move so a stray drag can't redraw it.
                        if self.draft.shaped.is_some() { self.tool = Tool::Move; }
                    }
                } else if resp.clicked() && !matches!(self.tool, Tool::Draw | Tool::Move) {
                    if let Some(q) = resp.interact_pointer_pos() { self.road_tap(to_world(q, self.centre), ctx); }
                }
                if resp.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    self.scale = (self.scale * (scroll * 0.002).exp()).clamp(0.4, 5.0);
                }
            }
        }
        // A fixed metre grid makes the road's width and the drawing scale visible.
        let hi = to_world(rect.min, self.centre);
        let lo = to_world(rect.max, self.centre);
        for x in ((lo.x / 50.0).floor() as i32)..=((hi.x / 50.0).ceil() as i32) {
            let px = to_screen(Vec2::new(x as f32 * 50.0, 0.0), self.centre).x;
            p.line_segment([pos2(px, rect.top()), pos2(px, rect.bottom())], Stroke::new(0.7_f32, rgba(188, 207, 148, 0.12)));
        }
        for y in ((lo.y / 50.0).floor() as i32)..=((hi.y / 50.0).ceil() as i32) {
            let py = to_screen(Vec2::new(0.0, y as f32 * 50.0), self.centre).y;
            p.line_segment([pos2(rect.left(), py), pos2(rect.right(), py)], Stroke::new(0.7_f32, rgba(188, 207, 148, 0.12)));
        }
        let line_progress = if self.build { (self.t / 0.9).clamp(0.0, 1.0) } else { 1.0 };
        let road_progress = if self.build { ((self.t - 0.8) / 0.8).clamp(0.0, 1.0) } else { 1.0 };
        if let Some(shaped) = &self.draft.shaped {
            let n = ((shaped.points.len() as f32 * line_progress) as usize).max(2).min(shaped.points.len());
            let pts: Vec<_> = shaped.points[..n].iter().map(|v| to_screen(*v, self.centre)).collect();
            let mk = |pts, stroke| if n == shaped.points.len() { Shape::closed_line(pts, stroke) } else { Shape::line(pts, stroke) };
            p.add(mk(pts.clone(), Stroke::new(shaped.width * k * road_progress + 3.0, theme::OUTLINE)));
            p.add(mk(pts.clone(), Stroke::new((shaped.width * k * road_progress).max(1.5), theme::SAND.gamma_multiply(0.85))));
            if let Some(heights) = self.hill_profile() {
                // Hills show as a band down the road: dark in the dips, pale on the crests.
                let lo = heights.iter().copied().fold(f32::INFINITY, f32::min);
                let span = (heights.iter().copied().fold(f32::NEG_INFINITY, f32::max) - lo).max(0.01);
                let band = (shaped.width * k * 0.55 * road_progress).max(1.5);
                let shade = |i: usize| theme::mix(theme::PLUM, theme::TEXT, ((heights[i % heights.len()] - lo) / span).clamp(0.0, 1.0));
                for i in 0..n {
                    if i + 1 < n || n == shaped.points.len() {
                        let a = to_screen(shaped.points[i], self.centre);
                        let b = to_screen(shaped.points[(i + 1) % shaped.points.len()], self.centre);
                        p.line_segment([a, b], Stroke::new(band, shade(i)));
                    }
                    p.circle_filled(to_screen(shaped.points[i], self.centre), band * 0.5, shade(i));
                }
            }
            p.add(mk(pts, Stroke::new(0.8_f32, rgba(255, 247, 214, 0.6))));
            if n == shaped.points.len() {
                // At each crossing, redraw the upper road over everything with a dark
                // edge either side, so it visibly passes over the lower one.
                let width = (shaped.width * k * road_progress).max(1.5);
                let deck = theme::mix(Color32::from_rgb(44, 59, 43), theme::SAND, 0.85);
                for crossing in &shaped.crossings {
                    let steps = 12;
                    let span: Vec<_> = (0..=steps).map(|i| to_screen(shaped.point_at(crossing.s_high + BRIDGE_REACH * (2.0 * i as f32 / steps as f32 - 1.0)), self.centre)).collect();
                    p.add(Shape::line(span.clone(), Stroke::new(width + 7.0, theme::OUTLINE)));
                    p.add(Shape::line(span.clone(), Stroke::new(width, deck)));
                    p.add(Shape::line(span, Stroke::new(0.8_f32, rgba(255, 247, 214, 0.6))));
                }
            }
            if !self.build || self.t >= 1.6 {
                for (i, f) in self.draft.track.features.iter().enumerate() {
                    let show = if self.build { ((self.t - 1.6 - i as f32 * 0.08) / 0.2).clamp(0.0, 1.0) } else { 1.0 };
                    let centre = to_screen(feature_point(shaped, f), self.centre);
                    if show > 0.05 { p.circle_filled(centre, 9.0 * show, theme::INDIGO); icons::draw(&p, feature_icon(f.kind), centre, 14.0 * show, theme::ACCENT); }
                }
            }
            if self.tool == Tool::Move && !self.build {
                for point in shaped.points.iter().step_by(6) {
                    let at = to_screen(*point, self.centre);
                    p.circle_filled(at, 3.5, theme::INDIGO);
                    p.circle_stroke(at, 3.5, Stroke::new(1.0_f32, theme::SAND));
                }
                if let Some((_, index, _)) = &self.moving {
                    let at = to_screen(shaped.points[(*index).min(shaped.points.len() - 1)], self.centre);
                    p.circle_stroke(at, 10.0, Stroke::new(2.0_f32, theme::ACCENT));
                }
            }
            let start = to_screen(shaped.points[0], self.centre);
            let dir = shaped.dir_at(0.0);
            let side = vec2(-dir.y, dir.x) * shaped.width * k * 0.6;
            p.line_segment([start - side, start + side], Stroke::new(3.0_f32, Color32::WHITE));
            let tip = start - vec2(dir.x, dir.y) * 15.0;
            p.arrow(start, tip - start, Stroke::new(2.0_f32, theme::ACCENT));
        } else if self.draft.raw.len() > 1 {
            let pts = self.draft.raw.iter().map(|v| to_screen(*v, self.centre)).collect();
            p.add(Shape::line(pts, Stroke::new(self.draft.track.width() * k, rgba(235, 154, 91, 0.55))));
            // Show both ends of the open line and the gap left to close.
            let (first, last) = (self.draft.raw[0], self.draft.raw[self.draft.raw.len() - 1]);
            let (a, b) = (to_screen(first, self.centre), to_screen(last, self.centre));
            let gap = first.distance(last);
            if gap > 1.0 {
                p.extend(Shape::dashed_line(&[b, a], Stroke::new(2.0_f32, rgba(255, 247, 214, 0.7)), 6.0, 5.0));
                p.text(a + (b - a) * 0.5 + vec2(0.0, -12.0), Align2::CENTER_CENTER, format!("{gap:.0} m to close"), theme::ui_bold(11.0), theme::TEXT);
            }
            if gap <= custom::CLOSE_GAP {
                art::glow(&p, a, 22.0, rgba(110, 240, 140, 0.45));
                p.circle_stroke(a, custom::CLOSE_GAP * k, Stroke::new(2.0_f32, Color32::from_rgb(110, 240, 140)));
            }
            p.circle_filled(a, 7.0, Color32::from_rgb(110, 240, 140));
            p.circle_filled(b, 6.0, Color32::WHITE);
            p.circle_stroke(b, 9.0, Stroke::new(2.0_f32, theme::ACCENT));
        }
        if let Some(at) = self.draft.error.as_ref().and_then(ShapeError::at) {
            let centre = to_screen(at, self.centre);
            art::glow(&p, centre, 20.0, rgba(255, 70, 60, 0.5));
            p.circle_stroke(centre, 12.0, Stroke::new(2.0_f32, theme::ACCENT));
        }
        let label = if let (Some(s), Some(_)) = (&self.draft.shaped, &self.draft.error) { format!("{:.0} m · fix the marked spot", s.length) } else if let Some(s) = &self.draft.shaped { format!("{:.0} m · {} bridge{}", s.length, s.crossings.len(), if s.crossings.len() == 1 { "" } else { "s" }) } else { "Draw a loop · 300–3000 m".into() };
        p.text(rect.left_bottom() + vec2(12.0, -12.0), Align2::LEFT_BOTTOM, label, theme::ui_bold(11.0), theme::TEXT);
        if self.draft.raw.is_empty() {
            p.text(rect.center() - vec2(0.0, 12.0), Align2::CENTER_CENTER, "Draw your own track", theme::title(25.0), theme::SAND);
            p.text(rect.center() + vec2(0.0, 16.0), Align2::CENTER_CENTER, "One finger to draw · two to pan and zoom", theme::ui_medium(12.0), theme::TEXT_DIM);
        }
    }
}

impl Screen for DesignerScreen {
    fn orientation(&self) -> crate::platform::os::Orientation {
        crate::platform::os::Orientation::Landscape
    }

    fn name(&self) -> &'static str { if self.build { "track_build" } else { "designer" } }
    fn update(&mut self, ctx: &mut Ctx) -> Transition { common::begin(ctx, &mut self.t); self.adopt_placed(); Transition::None }
    fn on_background(&mut self, _ctx: &mut Ctx) { feedback::on_background(); }
    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        if self.name_edit.take().is_some() { return Transition::None; }
        if self.leave_ask { self.leave_ask = false; feedback::back(ctx); return Transition::None; }
        if self.unsaved() { self.leave_ask = true; feedback::select(ctx); return Transition::None; }
        feedback::back(ctx); Transition::Pop
    }
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let mut out = Transition::None;
        common::page(ctx.viewport.screen, ctx.time as f32, ui, 0.1, |ui, _| {
            let hd = widgets::header(ui, safe, if self.build { "Your track" } else { "Track Designer" }, widgets::enter(self.t, 0));
            if hd.back.clicked() { out = self.back(ctx); }
            let body = hd.body;
            let rail_w = if self.build { 150.0 } else { 86.0 };
            let settings_w = (body.width() * 0.25).clamp(155.0, 200.0);
            let footer_h = 42.0;
            let area = Rect::from_min_max(body.min + vec2(rail_w + 8.0, 0.0), pos2(body.right() - settings_w - 10.0, body.bottom() - footer_h - 25.0));
            // The Items strip sits above the plan rather than over its top, so the road is not hidden.
            let strip_h = if !self.build && matches!(self.tool, Tool::Feature(_)) { ITEM_STRIP_H } else { 0.0 };
            let plan = Rect::from_min_max(area.min + vec2(0.0, strip_h), area.max);
            if self.name_edit.is_none() {
                self.canvas(ctx, ui, plan);
                if !self.build {
                    let tools = [("Draw", Tool::Draw), ("Shape", Tool::Move), ("Start", Tool::Start), ("Items", Tool::Feature(self.last_kind)), ("Remove", Tool::Remove)];
                    let h = ((area.height() - (tools.len() - 1) as f32 * 6.0) / tools.len() as f32).min(40.0);
                    for (i, (name, tool)) in tools.into_iter().enumerate() {
                        let rect = Rect::from_min_size(body.min + vec2(0.0, i as f32 * (h + 6.0)), vec2(rail_w, h));
                        let on = if name == "Items" { matches!(self.tool, Tool::Feature(_)) } else { self.tool == tool };
                        if widgets::button(ui, rect, name, &Btn::new(name).selected(on).size(15.0)).clicked() { self.tool = tool; feedback::tap(ctx); }
                    }
                    if let Tool::Feature(current) = self.tool {
                        // The Items strip: one button per kind along the top of the plan.
                        let strip = Rect::from_min_size(area.min + vec2(4.0, 4.0), vec2(area.width() - 8.0, 36.0));
                        let gap = 4.0;
                        let w = (strip.width() - gap * (ITEMS.len() - 1) as f32) / ITEMS.len() as f32;
                        ui.painter().rect_filled(strip.expand(4.0), 12.0, rgba(10, 4, 24, 0.94));
                        for (i, (kind, name)) in ITEMS.into_iter().enumerate() {
                            let rect = Rect::from_min_size(pos2(strip.left() + i as f32 * (w + gap), strip.top()), vec2(w, strip.height()));
                            let resp = ui.interact(rect, ui.id().with(("designer-item", i)), Sense::click());
                            let on = kind == current;
                            let p = ui.painter();
                            p.rect_filled(rect, 9.0, if on { theme::ACCENT } else { rgba(110, 60, 170, 0.55) });
                            p.rect_stroke(rect, 9.0, Stroke::new(if on { 2.0_f32 } else { 1.0 }, if on { theme::GOLD } else { theme::OUTLINE }), egui::StrokeKind::Inside);
                            let ink = if on { Color32::WHITE } else { theme::TEXT };
                            let size = art::fit_size(p, name, theme::title, 14.0, w - 30.0);
                            icons::draw(p, feature_icon(kind), pos2(rect.left() + 13.0, rect.center().y), 15.0, if on { Color32::WHITE } else { theme::SAND });
                            p.text(pos2(rect.left() + 24.0 + (w - 28.0) * 0.5, rect.center().y), Align2::CENTER_CENTER, name, theme::title(size), ink);
                            if resp.clicked() { self.tool = Tool::Feature(kind); self.last_kind = kind; feedback::tap(ctx); }
                        }
                    }
                } else {
                    // Setting first: where the track is decides how it looks and sounds.
                    ui.painter().text(pos2(body.left() + 4.0, body.top() + 9.0), Align2::LEFT_CENTER, "Where is it?", theme::title(16.0), theme::SAND);
                    let top = body.top() + 24.0;
                    let gap = 4.0;
                    let h = ((plan.bottom() - top - gap * 7.0) / 8.0).min(28.0);
                    for (i, id) in TrackId::RACE_ALL.iter().enumerate() {
                        let rect = Rect::from_min_size(pos2(body.left(), top + i as f32 * (h + gap)), vec2(rail_w, h));
                        let name = id.def().name.to_string();
                        let on = self.draft.track.look == *id;
                        let btn = if on { Btn::primary(&name) } else { Btn::new(&name) };
                        if widgets::button(ui, rect, &format!("track-setting-{i}"), &btn.size(13.0)).clicked() && !on { self.remember(); self.draft.track.look = *id; feedback::tap(ctx); }
                    }
                }
                let settings = Rect::from_min_max(pos2(body.right() - settings_w, body.top()), pos2(body.right(), plan.bottom()));
                widgets::panel(ui.painter(), settings, 14.0);
                let inner = settings.shrink(10.0);
                let mut y = inner.top();
                let row_h = ((inner.height() - 48.0) / 5.0 - 5.0).clamp(22.0, 30.0);
                let name_rect = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), row_h));
                let name_size = art::fit_size(ui.painter(), &self.draft.track.name, theme::title, 17.0, inner.width() - 16.0);
                if widgets::button(ui, name_rect, "track-name", &Btn::new(&self.draft.track.name).size(name_size)).clicked() { self.name_edit = Some(self.draft.track.name.clone()); }
                y += row_h + 5.0;
                if !self.build {
                    let look = TrackId::RACE_ALL.iter().position(|id| *id == self.draft.track.look).unwrap_or(0);
                    let look_rect = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), row_h));
                    let label = format!("Setting: {}", self.draft.track.look.def().name);
                    let size = art::fit_size(ui.painter(), &label, theme::ui_bold, 12.0, inner.width() - 12.0);
                    if widgets::button(ui, look_rect, "track-look", &Btn::new(&label).size(size)).clicked() { self.remember(); self.draft.track.look = TrackId::RACE_ALL[(look + 1) % 8]; feedback::tap(ctx); }
                    y += row_h + 5.0;
                }
                let lap_rect = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), row_h));
                ui.painter().text(pos2(lap_rect.left(), lap_rect.center().y), Align2::LEFT_CENTER, "Laps", theme::ui_bold(11.0), theme::SAND);
                for (i, label) in ["−", "+"].into_iter().enumerate() {
                    let rect = Rect::from_min_size(pos2(lap_rect.left() + 45.0 + i as f32 * (lap_rect.width() - 45.0 - row_h), y), vec2(row_h, row_h));
                    let enabled = if i == 0 { self.draft.track.laps > 1 } else { self.draft.track.laps < 9 };
                    if widgets::button(ui, rect, &format!("track-laps-{i}"), &Btn::new(label).enabled(enabled).size(20.0)).clicked() { self.remember(); if i == 0 { self.draft.track.laps -= 1; } else { self.draft.track.laps += 1; } feedback::tap(ctx); }
                }
                ui.painter().text(pos2(lap_rect.left() + (lap_rect.width() + 45.0) * 0.5, lap_rect.center().y), Align2::CENTER_CENTER, self.draft.track.laps.to_string(), theme::title(20.0), theme::TEXT);
                y += row_h + 5.0;
                let width_rect = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), row_h));
                ui.painter().text(pos2(width_rect.left(), width_rect.center().y), Align2::LEFT_CENTER, "Width", theme::ui_bold(11.0), theme::SAND);
                let widths = [140, 180, 220];
                let control = Rect::from_min_max(pos2(width_rect.left() + 45.0, y), width_rect.max);
                if let Some(i) = widgets::segmented(ui, control, "track-width", &[Seg::new("S"), Seg::new("M"), Seg::new("L")], widths.iter().position(|w| *w == self.draft.track.width_dm).unwrap_or(1)) { self.remember(); self.draft.track.width_dm = widths[i]; self.recheck_width(); feedback::tap(ctx); }
                y += row_h + 5.0;
                let hills_rect = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), row_h));
                ui.painter().text(pos2(hills_rect.left(), hills_rect.center().y), Align2::LEFT_CENTER, "Hills", theme::ui_bold(11.0), theme::SAND);
                let control = Rect::from_min_max(pos2(hills_rect.left() + 45.0, y), hills_rect.max);
                if let Some(i) = widgets::segmented(ui, control, "track-hills", &[Seg::new("0"), Seg::new("1"), Seg::new("2"), Seg::new("3")], self.draft.track.hills.min(custom::HILLS_MAX) as usize) { self.remember(); self.draft.track.hills = i as u8; feedback::tap(ctx); }
                y += row_h + 5.0;
                if !self.build {
                    let h = ((inner.bottom() - y - 4.0) * 0.5).min(30.0);
                    for (i, (a, b)) in [("Close loop", "Auto-fill"), ("Undo", "Clear")].into_iter().enumerate() {
                        for (j, label) in [a, b].into_iter().enumerate() {
                            let rect = Rect::from_min_size(pos2(inner.left() + j as f32 * (inner.width() + 4.0) * 0.5, y + i as f32 * (h + 4.0)), vec2((inner.width() - 4.0) * 0.5, h));
                            let enabled = match label { "Undo" => !self.undo.is_empty(), "Auto-fill" => self.draft.shaped.is_some() && self.draft.error.is_none(), _ => !self.draft.raw.is_empty() };
                            if widgets::button(ui, rect, label, &Btn::new(label).enabled(enabled).size(11.0)).clicked() {
                                feedback::tap(ctx);
                                match label {
                                    "Undo" => if let Some(previous) = self.undo.pop() { self.draft = previous; },
                                    "Clear" => { self.remember(); self.draft.raw.clear(); self.draft.track.points.clear(); self.draft.track.features.clear(); self.draft.shaped = None; self.draft.error = None; },
                                    "Close loop" => { self.remember(); self.reshape(true); if let Some(s) = &self.draft.shaped { self.draft.raw = s.points.clone(); } },
                                    _ => if self.draft.error.is_none() { self.remember(); if let Some(s) = &self.draft.shaped { self.draft.track.features = custom::auto_fill(s, self.draft.track.seed); } },
                                }
                            }
                        }
                    }
                }
                let hint = if let Some(error) = &self.draft.error { if self.tool == Tool::Move && self.draft.shaped.is_some() { format!("{}. {}", capitalised(&error.to_string()), if matches!(error, ShapeError::TooTight { .. }) { "Drag the corner wider where it's marked" } else { "Drag the road apart where it's marked" }) } else { capitalised(&error.to_string()) } } else if self.build { if self.t < 2.5 { "Shaping your road…".into() } else { "Pick where it is, then test it".into() } } else if self.tool == Tool::Move { "Drag the road to reshape a bend".into() } else if self.tool == Tool::Start { "Tap the road to move the start; tap the flag to reverse".into() } else if self.tool == Tool::Remove { "Tap a feature to remove it".into() } else if matches!(self.tool, Tool::Feature(_)) { "Tap anywhere on the road to place · Use Remove or Undo to take one out".into() } else {
                    let base = "Draw · close the loop · add features · test it";
                    let climb = self.hill_profile().map(|h| h.iter().copied().fold(f32::NEG_INFINITY, f32::max) - h.iter().copied().fold(f32::INFINITY, f32::min));
                    match climb {
                        Some(c) if art::fit_size(ui.painter(), &format!("{base} · climb {c:.0} m"), theme::ui_medium, 11.0, plan.width()) >= 11.0 => format!("{base} · climb {c:.0} m"),
                        _ => base.into(),
                    }
                };
                let hint_size = art::fit_size(ui.painter(), &hint, theme::ui_medium, 11.0, plan.width());
                ui.painter().text(pos2(plan.left(), plan.bottom() + 10.0), Align2::LEFT_CENTER, hint, theme::ui_medium(hint_size), if self.draft.error.is_some() { theme::ACCENT } else { theme::TEXT_DIM });
                let acts: Vec<(Act, &str)> = if self.build {
                    vec![(Act::Test, "Test run"), (Act::Race, "Race bots"), (Act::Save, "Save"), (Act::Place, "Drive & place"), (Act::Edit, "Edit"), (Act::Discard, "Discard")]
                } else {
                    vec![(Act::Test, "Test"), (Act::Race, "Race"), (Act::Save, "Save"), (Act::Invite, "Invite"), (Act::Place, "Drive & place")]
                };
                // The buttons share the width evenly, so six fit as cleanly as five.
                let gap = 6.0;
                let w = (body.width() - gap * (acts.len() - 1) as f32) / acts.len() as f32;
                let enabled = self.ready() && (!self.build || self.t >= 2.5);
                for (i, (act, label)) in acts.into_iter().enumerate() {
                    let rect = Rect::from_min_size(pos2(body.left() + i as f32 * (w + gap), body.bottom() - footer_h), vec2(w, footer_h));
                    if widgets::button(ui, rect, &format!("designer-action-{i}"), &Btn::new(label).enabled(enabled || act == Act::Discard).size(17.0)).clicked() {
                        out = self.action(ctx, act);
                    }
                }
            }
            if self.leave_ask {
                let (title, keep, leave) = if self.build { ("Discard this run?", "Keep", "Discard") } else { ("Leave without saving?", "Keep editing", "Leave") };
                ui.painter().rect_filled(safe, 0.0, rgba(6, 2, 14, 0.9));
                // Nothing behind the card takes a tap.
                ui.interact(ctx.viewport.screen, ui.id().with("leave-veil"), Sense::click_and_drag());
                let card = Rect::from_center_size(safe.center(), vec2(350.0, 138.0));
                widgets::panel_solid(ui.painter(), card, 18.0);
                ui.painter().text(card.center_top() + vec2(0.0, 28.0), Align2::CENTER_CENTER, title, theme::title(20.0), theme::TEXT);
                ui.painter().text(card.center_top() + vec2(0.0, 56.0), Align2::CENTER_CENTER, if self.build { "This run has not been saved" } else { "Your changes have not been saved" }, theme::ui_medium(11.0), theme::TEXT_DIM);
                for (i, label) in [keep, leave].into_iter().enumerate() {
                    let rect = Rect::from_min_size(card.left_bottom() + vec2(12.0 + i as f32 * 166.0, -54.0), vec2(158.0, 42.0));
                    if widgets::button(ui, rect, &format!("designer-leave-{i}"), &Btn::new(label).size(18.0)).clicked() {
                        self.leave_ask = false;
                        if i == 1 {
                            if self.build { ctx.say("Run discarded"); }
                            feedback::back(ctx);
                            out = Transition::Pop;
                        }
                    }
                }
            }
            if let Some(mut name) = self.name_edit.take() {
                match name_keyboard(ui, safe, &mut name) {
                    Some(true) => { let name = name.trim(); if !name.is_empty() { self.remember(); self.draft.track.name = name.into(); } },
                    Some(false) => {},
                    None => self.name_edit = Some(name),
                }
            }
        });
        out
    }
}

/// The road's right-hand side at `s` in plan coordinates. Plan `(x, y)` is world `(x, z)`, and
/// race-core's right is `(-t.z, 0, t.x)`, so this is `(-t.y, t.x)`.
pub(crate) fn right_of(shaped: &Shaped, s: f32) -> Vec2 {
    let tangent = (shaped.point_at(s + 0.1) - shaped.point_at(s - 0.1)).normalize_or_zero();
    Vec2::new(-tangent.y, tangent.x)
}

/// The feature `kind` as dropped at plan point `at`, whose nearest point on the road is `s` m along.
/// Only coins keep how far right of the centreline they were put (negative is left).
pub(crate) fn feature_at(shaped: &Shaped, kind: Kind, s: f32, at: Vec2) -> CustomFeature {
    let lateral = (at - shaped.point_at(s)).dot(right_of(shaped, s)).clamp(-shaped.width * 0.5, shaped.width * 0.5);
    CustomFeature { kind, s_dm: (s * 10.0).floor() as u16, lateral_dm: if kind == Kind::Coins { (lateral * 10.0).round() as i16 } else { 0 } }
}

fn feature_point(shaped: &Shaped, feature: &CustomFeature) -> Vec2 {
    let s = feature.s();
    shaped.point_at(s) + right_of(shaped, s) * feature.lateral()
}

pub(crate) fn feature_icon(kind: Kind) -> Icon { match kind { Kind::Eggs => Icon::Laser, Kind::Ramp | Kind::Gap => Icon::Jump, Kind::Log => Icon::Jump, Kind::Boost => Icon::Drift, Kind::Coins => Icon::Item } }

pub(crate) fn sample_track(name: &str) -> CustomTrack {
    let raw: Vec<_> = (0..=160).map(|i| { let a = i as f32 / 160.0 * std::f32::consts::TAU; Vec2::new(a.cos() * 130.0, a.sin() * 95.0) }).collect();
    let shaped = custom::shape(&raw, false, 18.0).expect("sample loop");
    custom::custom_track_from(&shaped, name, TrackId::Savanna, 3, 180, custom::auto_fill(&shaped, 7), 7)
}

fn name_keyboard(ui: &mut egui::Ui, safe: Rect, name: &mut String) -> Option<bool> {
    ui.painter().rect_filled(safe, 0.0, rgba(6, 2, 14, 0.95));
    let card = Rect::from_center_size(safe.center(), vec2(safe.width().min(600.0) - 20.0, safe.height().min(300.0) - 16.0));
    widgets::panel_solid(ui.painter(), card, 18.0);
    let inner = card.shrink(12.0);
    let size = art::fit_size(ui.painter(), name, theme::title, 23.0, inner.width() - 20.0);
    ui.painter().text(inner.left_top(), Align2::LEFT_TOP, name.as_str(), theme::title(size), theme::TEXT);
    let rows = ["1234567890", "QWERTYUIOP", "ASDFGHJKL", "ZXCVBNM"];
    let h = (inner.height() - 42.0) / 5.0 - 4.0;
    let w = (inner.width() - 9.0 * 4.0) / 10.0;
    for (r, row) in rows.iter().enumerate() {
        let x = inner.left() + (inner.width() - (row.len() as f32 * (w + 4.0) - 4.0)) * 0.5;
        for (i, c) in row.chars().enumerate() {
            let rect = Rect::from_min_size(pos2(x + i as f32 * (w + 4.0), inner.top() + 38.0 + r as f32 * (h + 4.0)), vec2(w, h));
            if widgets::button(ui, rect, &format!("track-key-{c}"), &Btn::new(&c.to_string()).size(17.0)).clicked() && name.chars().count() < custom::MAX_NAME {
                name.push(if name.is_empty() || name.ends_with(' ') { c } else { c.to_ascii_lowercase() });
            }
        }
    }
    let labels = ["Cancel", "Delete", "Space", "Done"];
    let w = (inner.width() - 12.0) / 4.0;
    for (i, label) in labels.into_iter().enumerate() {
        let rect = Rect::from_min_size(pos2(inner.left() + i as f32 * (w + 4.0), inner.bottom() - h), vec2(w, h));
        if widgets::button(ui, rect, label, &Btn::new(label).size(15.0)).clicked() {
            match i { 0 => return Some(false), 1 => { name.pop(); }, 2 => if name.chars().count() < custom::MAX_NAME && !name.is_empty() && !name.ends_with(' ') { name.push(' '); }, _ => return Some(true) }
        }
    }
    None
}

/// A closed loop pinched to a 10 m neck in the middle: the two passes there are too close.
pub(crate) fn pinched_loop() -> Vec<Vec2> {
    (0..=160).map(|i| { let a = i as f32 / 160.0 * std::f32::consts::TAU; let x = a.cos() * 160.0; Vec2::new(x, a.sin() * (5.0 + 55.0 * (1.0 - (-(x / 60.0).powi(2)).exp()))) }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tapping_right_of_the_road_gives_a_positive_lateral() {
        let shaped = sample_track("Right").shaped().unwrap();
        for along in [40.0, 180.0, 330.0, 500.0] {
            let tangent = (shaped.point_at(along + 0.1) - shaped.point_at(along - 0.1)).normalize_or_zero();
            // Facing along the road, with plan y as world z: right is (-t.y, t.x).
            let right = Vec2::new(-tangent.y, tangent.x);
            let (s, _) = shaped.nearest(shaped.point_at(along) + right * 3.0);
            let f = feature_at(&shaped, Kind::Coins, s, shaped.point_at(along) + right * 3.0);
            assert!((f.lateral_dm - 30).abs() <= 2, "{along}: {f:?}");
            let f = feature_at(&shaped, Kind::Coins, s, shaped.point_at(along) - right * 3.0);
            assert!((f.lateral_dm + 30).abs() <= 2, "{along}: {f:?}");
            // And the marker shown for it is back where it was tapped.
            let at = shaped.point_at(along) + right * 3.0;
            let f = feature_at(&shaped, Kind::Coins, shaped.nearest(at).0, at);
            assert!(feature_point(&shaped, &f).distance(at) < 0.5);
        }
    }

    #[test]
    fn moving_a_bend_preserves_the_loop_and_features_and_can_be_undone() {
        let mut s = DesignerScreen::sample();
        assert!(s.tool == Tool::Move);
        let previous = s.draft.track.clone();
        let point = s.draft.shaped.as_ref().unwrap().points[35];
        s.begin_move(point, 20.0);
        s.move_line(point + Vec2::new(8.0, 4.0));
        assert!(s.ready());
        assert_ne!(s.draft.track.points, previous.points);
        assert_eq!(s.draft.track.features.len(), previous.features.len());
        assert_eq!(s.draft.track.name, previous.name);
        assert_eq!(s.draft.track.laps, previous.laps);
        let old = previous.shaped().unwrap();
        let next = s.draft.shaped.as_ref().unwrap();
        let opposite = old.points[(35 + old.points.len() / 2) % old.points.len()];
        assert!(next.nearest(opposite).1 < 2.0, "the opposite bend should stay put");
        // Pointer frames always deform the original line, never accumulate changes.
        let moved = s.draft.track.clone();
        s.move_line(point + Vec2::new(8.0, 4.0));
        assert_eq!(s.draft.track, moved);
        s.draft = s.undo.pop().unwrap();
        assert_eq!(s.draft.track, previous);
    }

    #[test]
    fn a_closed_run_that_breaks_a_shape_rule_stays_a_loop_to_reshape() {
        let mut s = DesignerScreen::from_failed_run(pinched_loop(), "Run 2".into());
        assert!(s.draft.shaped.is_some() && s.draft.error.is_some(), "{:?}", s.draft.error);
        assert!(matches!(s.draft.error, Some(ShapeError::TooClose { .. })));
        assert!(s.tool == Tool::Move);
        assert!(!s.ready());
        assert!(s.draft.track.points.is_empty(), "no track points while faulty");
        assert!(s.hill_profile().is_none());
        // Take hold of the road where the fault is marked and pull that pass away from the other.
        let at = s.draft.error.as_ref().and_then(ShapeError::at).unwrap();
        let node = s.draft.shaped.as_ref().unwrap().points.iter().copied().min_by(|a, b| a.distance(at).total_cmp(&b.distance(at))).unwrap();
        s.begin_move(node, 20.0);
        assert!(s.moving.is_some());
        let away = Vec2::new(0.0, if node.y >= 0.0 { 40.0 } else { -40.0 });
        s.move_line(node + away);
        assert!(s.draft.shaped.is_some());
        s.settle_move().unwrap();
        assert!(s.draft.error.is_none(), "pulled apart: {:?}", s.draft.error);
        assert!(s.ready());
        assert!(!s.draft.track.points.is_empty() && s.draft.raw.len() == s.draft.track.points.len());
        // The fix is one undo step, and undoing it brings the fault back.
        s.draft = s.undo.pop().unwrap();
        assert!(s.draft.error.is_some() && s.draft.shaped.is_some());
    }

    #[test]
    fn a_faulty_loop_stays_faulty_when_a_drag_does_not_fix_it_and_blocks_what_needs_a_track() {
        let mut s = DesignerScreen::from_failed_run(pinched_loop(), "Run 2".into());
        let node = s.draft.shaped.as_ref().unwrap().points[0];
        s.begin_move(node, 20.0);
        s.move_line(node + Vec2::new(0.0, 1.0));
        s.settle_move().unwrap();
        assert!(s.draft.shaped.is_some() && s.draft.error.is_some() && !s.ready());
        assert!(s.move_start(node, 0.0).is_err());
        assert!(s.draft.track.features.is_empty());
        // A hand-drawn closed stroke behaves the same way.
        let mut d = DesignerScreen::new();
        d.draft.raw = pinched_loop();
        d.reshape(false);
        assert!(d.draft.shaped.is_some() && matches!(d.draft.error, Some(ShapeError::TooClose { .. })));
        assert!(d.begin_stroke(d.draft.raw[0], 10.0).is_err(), "a closed loop is not redrawn");
    }

    #[test]
    fn items_placed_while_driving_are_adopted_as_one_undo_step() {
        let mut s = DesignerScreen::sample();
        let before = s.draft.track.clone();
        // Nothing waiting: nothing changes, nothing to undo.
        s.adopt_placed();
        assert!(s.undo.is_empty() && s.draft.track == before);
        let mut features = before.features.clone();
        features.push(CustomFeature { kind: Kind::Log, s_dm: 1000, lateral_dm: 0 });
        features.push(CustomFeature { kind: Kind::Coins, s_dm: 2000, lateral_dm: -30 });
        *s.placed.borrow_mut() = Some(features.clone());
        s.adopt_placed();
        assert_eq!(s.draft.track.features, features);
        assert!(s.placed.borrow().is_none(), "the slot is cleared");
        assert!(s.ready());
        // One Undo takes the whole drive back.
        assert_eq!(s.undo.len(), 1);
        s.draft = s.undo.pop().unwrap();
        assert_eq!(s.draft.track, before);
        // Handing back the list it already has is not an edit.
        *s.placed.borrow_mut() = Some(before.features.clone());
        s.adopt_placed();
        assert!(s.undo.is_empty());
    }

    #[test]
    fn hills_survive_moving_the_start_and_saving() {
        let mut s = DesignerScreen::hills_sample();
        assert_eq!(s.draft.track.hills, 2);
        let shaped = s.draft.shaped.clone().unwrap();
        // The tint has one height per road point; flat tracks have no profile.
        assert_eq!(s.hill_profile().unwrap().len(), shaped.points.len());
        let halfway = shaped.length * 0.5;
        s.move_start(shaped.point_at(halfway), halfway).unwrap();
        assert_eq!(s.draft.track.hills, 2);
        assert!(s.draft.shaped.is_some() && s.ready());
        assert_eq!(s.undo.last().unwrap().track.hills, 2);
        // Tapping the flag reverses the direction and still keeps them.
        let start = s.draft.shaped.as_ref().unwrap().points[0];
        s.move_start(start, 0.0).unwrap();
        assert_eq!(s.draft.track.hills, 2);
        // Changing the level is part of the track and of its hash.
        let level_two = s.draft.track.clone();
        s.draft.track.hills = 3;
        assert_ne!(s.draft.track.hash(), level_two.hash());
        s.draft.track.hills = 0;
        assert!(s.hill_profile().is_none());
        // A saved hilly track loads back with its level and the same hash.
        let dir = std::env::temp_dir().join(format!("stampede-designer-hills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hash = crate::custom_tracks::save(&dir, &level_two).unwrap();
        assert_eq!(hash, level_two.hash());
        let loaded = crate::custom_tracks::load(&dir, hash).unwrap();
        assert_eq!((loaded.hills, loaded.hash()), (2, hash));
        assert_eq!(DesignerScreen::edit(loaded).draft.track.hills, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn strokes_carry_on_from_either_end_and_never_wipe_the_line() {
        let full = sample_track("Stroke").points_m();
        let mut s = DesignerScreen::new();
        s.draft.raw = full[..full.len() / 2].to_vec();
        s.reshape(false);
        let open = s.draft.raw.clone();
        // Away from both ends: refused, nothing changes.
        let middle = open[open.len() / 2] + Vec2::new(60.0, 60.0);
        assert!(s.begin_stroke(middle, 10.0).is_err());
        assert_eq!(s.draft.raw, open);
        assert!(!s.drawing && s.undo.is_empty());
        // From the far end: carries on.
        s.begin_stroke(*open.last().unwrap(), 10.0).unwrap();
        assert!(s.drawing);
        assert_eq!(s.draft.raw, open);
        s.drawing = false;
        // From the start end: the line turns round so it carries on from there.
        s.begin_stroke(open[0], 10.0).unwrap();
        assert_eq!(s.draft.raw.last(), Some(&open[0]));
        assert_eq!(s.draft.raw.len(), open.len());
        // A closed loop is never redrawn by a stray stroke.
        let mut closed = DesignerScreen::sample();
        let before = closed.draft.track.clone();
        assert!(closed.begin_stroke(closed.draft.raw[0], 10.0).is_err());
        assert_eq!(closed.draft.track, before);
    }

    #[test]
    fn grabbing_empty_space_does_not_replace_the_line() {
        let mut s = DesignerScreen::sample();
        let previous = s.draft.track.clone();
        s.begin_move(Vec2::ZERO, 20.0);
        assert!(s.moving.is_none());
        assert!(s.undo.is_empty());
        assert_eq!(s.draft.track, previous);
    }

    #[test]
    fn drawn_track_and_undo_preserve_settings_and_features() {
        let mut s = DesignerScreen::sample();
        let previous = s.draft.track.clone();
        assert!(s.ready());
        s.remember();
        s.draft.track.width_dm = 220;
        s.recheck_width();
        assert!(s.ready());
        s.draft = s.undo.pop().unwrap();
        assert_eq!(s.draft.track, previous);
        assert!(s.ready());
        s.draft.raw.truncate(15);
        s.reshape(false);
        assert!(!s.ready());
        assert!(s.draft.error.is_some());
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crate::cli::Options;
    use crate::engine::Engine;
    use crate::haptics::NoHaptics;
    use crate::platform::data_dir::Profile;
    use crate::platform::os::RecordingPlatform;
    use crate::platform::TouchPhase;

    fn frame(e: &mut Engine, n: usize) { for _ in 0..n { e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default()); } }
    fn tap(e: &mut Engine, x: f64, y: f64) {
        e.touch_event(101, TouchPhase::Started, (x * 3.0, y * 3.0)); frame(e, 2);
        e.touch_event(101, TouchPhase::Ended, (x * 3.0, y * 3.0)); frame(e, 2);
    }

    #[test]
    fn saving_an_edited_track_keeps_the_authored_geometry() {
        let track = sample_track("Authored Loop");
        let dir = std::env::temp_dir().join(format!("stampede-designer-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::new(Box::new(DesignerScreen::edit(track.clone())), Profile::default(), dir.clone(), Options { shot: Some("designer".into()), ..Options::default() }, Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        frame(&mut e, 90);
        // Save is the third of the five buttons along the bottom bar.
        tap(&mut e, 398.0, 323.0);
        frame(&mut e, 20);
        assert_eq!(crate::custom_tracks::list(&dir), vec![track]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drawing_a_closed_loop_can_be_saved_from_the_designer() {
        let dir = std::env::temp_dir().join(format!("stampede-designer-draw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::new(Box::new(DesignerScreen::new()), Profile::default(), dir.clone(),
            Options { shot: Some("designer".into()), ..Options::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        frame(&mut e, 60);
        e.touch_event(101, TouchPhase::Started, (420.0 * 3.0, 170.0 * 3.0));
        frame(&mut e, 2);
        for i in 1..=72 {
            let a = i as f64 / 72.0 * std::f64::consts::TAU;
            e.touch_event(101, TouchPhase::Moved, ((330.0 + 90.0 * a.cos()) * 3.0, (170.0 + 60.0 * a.sin()) * 3.0));
            frame(&mut e, 2);
        }
        e.touch_event(101, TouchPhase::Ended, (420.0 * 3.0, 170.0 * 3.0));
        frame(&mut e, 4);
        tap(&mut e, 398.0, 323.0);
        let saved = crate::custom_tracks::list(&dir);
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0].laps, saved[0].width_dm), (3, 180));
        assert!(saved[0].to_def().is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_stroke_from_the_end_carries_the_line_on_and_closes_it() {
        let dir = std::env::temp_dir().join(format!("stampede-designer-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::new(Box::new(DesignerScreen::new()), Profile::default(), dir.clone(),
            Options { shot: Some("designer".into()), ..Options::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        frame(&mut e, 60);
        let at = |i: usize| { let a = i as f64 / 72.0 * std::f64::consts::TAU; ((330.0 + 90.0 * a.cos()) * 3.0, (170.0 + 60.0 * a.sin()) * 3.0) };
        let stroke = |e: &mut Engine, from: usize, to: usize| {
            e.touch_event(101, TouchPhase::Started, at(from));
            frame(e, 2);
            for i in from + 1..=to { e.touch_event(101, TouchPhase::Moved, at(i)); frame(e, 2); }
            e.touch_event(101, TouchPhase::Ended, at(to));
            frame(e, 4);
        };
        // Two thirds of the loop, then lift the finger: the line stays open.
        stroke(&mut e, 0, 48);
        // A stray stroke away from both ends leaves the line alone.
        e.touch_event(101, TouchPhase::Started, (330.0 * 3.0, 170.0 * 3.0)); frame(&mut e, 2);
        e.touch_event(101, TouchPhase::Moved, (300.0 * 3.0, 150.0 * 3.0)); frame(&mut e, 2);
        e.touch_event(101, TouchPhase::Ended, (300.0 * 3.0, 150.0 * 3.0)); frame(&mut e, 4);
        tap(&mut e, 398.0, 323.0);
        assert!(crate::custom_tracks::list(&dir).is_empty(), "an open line cannot be saved");
        // Carry on from where the finger lifted and finish the loop.
        stroke(&mut e, 48, 72);
        frame(&mut e, 4);
        tap(&mut e, 398.0, 323.0);
        let saved = crate::custom_tracks::list(&dir);
        assert_eq!(saved.len(), 1, "the joined loop saves");
        assert!(saved[0].to_def().is_ok());
        let length = saved[0].shaped().unwrap().length;
        assert!(length > 600.0, "both strokes make up the loop, got {length:.0} m");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_open_stroke_cannot_be_saved_until_the_loop_is_closed() {
        let mut s = DesignerScreen::new();
        let full = sample_track("Stroke");
        s.draft.raw = full.points_m();
        s.draft.raw.truncate(s.draft.raw.len() - 20);
        s.reshape(false);
        assert!(!s.ready());
        s.reshape(true);
        assert!(s.ready());
    }

    fn engine_for(screen: DesignerScreen, name: &str) -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("stampede-designer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let e = Engine::new(Box::new(screen), Profile::default(), dir.clone(), Options { shot: Some("designer".into()), ..Options::default() }, Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        (e, dir)
    }

    /// The centre of the confirm card's button `i` (0 keeps, 1 leaves).
    fn leave_button(e: &mut Engine, i: usize) -> (f64, f64) {
        let safe = e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default()).viewport.safe;
        let card = Rect::from_center_size(safe.center(), vec2(350.0, 138.0));
        let c = card.left_bottom() + vec2(12.0 + i as f32 * 166.0 + 79.0, -54.0 + 21.0);
        (c.x as f64, c.y as f64)
    }

    /// The designer pushed over a menu, so a pop shows.
    fn engine_over_menu(screen: DesignerScreen, name: &str) -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("stampede-designer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::new(Box::new(crate::ui::screens::menu::RaceMenuScreen::new()), Profile::default(), dir.clone(), Options { shot: Some("designer".into()), ..Options::default() }, Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        e.router.apply(Transition::Push(Box::new(screen)));
        (e, dir)
    }

    #[test]
    fn back_with_unsaved_work_asks_first_and_leave_pops() {
        let mut s = DesignerScreen::sample();
        s.draft.track.laps = 5; // edited since it was loaded
        let (mut e, dir) = engine_over_menu(s, "leave");
        frame(&mut e, 90);
        e.request_back();
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu", "designer"], "Back asks, it does not leave");
        // Back again closes the question.
        e.request_back();
        frame(&mut e, 40);
        e.request_back();
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu", "designer"]);
        // Keep editing stays; the card goes away.
        let (x, y) = leave_button(&mut e, 0);
        tap(&mut e, x, y);
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu", "designer"]);
        // Asked again, Leave goes.
        e.request_back();
        frame(&mut e, 40);
        let (x, y) = leave_button(&mut e, 1);
        tap(&mut e, x, y);
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn back_with_nothing_drawn_or_everything_saved_pops_at_once() {
        let (mut e, dir) = engine_over_menu(DesignerScreen::new(), "leave-empty");
        frame(&mut e, 60);
        e.request_back();
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu"]);
        let _ = std::fs::remove_dir_all(dir);
        // A track as saved asks nothing; nor does a run once it has been saved.
        let (mut e, dir) = engine_over_menu(DesignerScreen::sample(), "leave-saved");
        frame(&mut e, 60);
        e.request_back();
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu"]);
        let _ = std::fs::remove_dir_all(dir);
        let mut s = DesignerScreen::build_sample();
        assert!(s.unsaved(), "a run is unsaved until it is saved");
        s.saved = Some(s.draft.track.hash());
        assert!(!s.unsaved());
    }

    #[test]
    fn back_on_a_run_asks_whether_to_discard_it() {
        let (mut e, dir) = engine_over_menu(DesignerScreen::build_sample(), "leave-run");
        frame(&mut e, 200);
        e.request_back();
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu", "track_build"]);
        let (x, y) = leave_button(&mut e, 1);
        tap(&mut e, x, y);
        frame(&mut e, 40);
        assert_eq!(e.router.names(), ["race_menu"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stray_one_point_line_does_not_block_the_next_stroke() {
        let mut s = DesignerScreen::new();
        s.begin_stroke(Vec2::new(10.0, 10.0), 20.0).unwrap();
        s.drawing = false;
        assert_eq!(s.draft.raw.len(), 1);
        // Far from the stray point: still a fresh line, not a refusal.
        s.begin_stroke(Vec2::new(200.0, 90.0), 20.0).unwrap();
        assert_eq!(s.draft.raw, vec![Vec2::new(200.0, 90.0)]);
    }

    #[test]
    fn the_items_strip_places_the_chosen_kind_and_items_remembers_it() {
        let (mut e, dir) = engine_for(DesignerScreen::sample(), "items");
        frame(&mut e, 90);
        let before = sample_track("Sunset Loop").features.len();
        // Rail: Items (4th). Strip: Boost (5th of 6). Then tap the right-hand road.
        tap(&mut e, 59.0, 214.0);
        tap(&mut e, 460.0, 92.0);
        tap(&mut e, 454.0, 172.0);
        // Remove, then Items again: it falls back to Boost, the last kind used.
        tap(&mut e, 59.0, 268.0);
        tap(&mut e, 59.0, 214.0);
        tap(&mut e, 454.0, 200.0);
        tap(&mut e, 398.0, 323.0);
        let saved = crate::custom_tracks::list(&dir);
        assert_eq!(saved.len(), 1);
        let boosts = |t: &CustomTrack| t.features.iter().filter(|f| f.kind == Kind::Boost).count();
        let was = boosts(&sample_track("Sunset Loop"));
        assert_eq!(saved[0].features.len(), before + 2);
        assert_eq!(boosts(&saved[0]), was + 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drive_and_place_is_on_both_footers_and_opens_the_drive() {
        // Designer footer: Test, Race, Save, Invite, Drive & place.
        let (mut e, dir) = engine_for(DesignerScreen::sample(), "place");
        frame(&mut e, 90);
        tap(&mut e, 706.0, 323.0);
        frame(&mut e, 20);
        assert_eq!(e.router.names(), ["designer", "place_items"]);
        let _ = std::fs::remove_dir_all(dir);
        // Build footer: Test run, Race bots, Save, Drive & place, Edit, Discard.
        let (mut e, dir) = engine_for(DesignerScreen::build_sample(), "buildplace");
        frame(&mut e, 200);
        tap(&mut e, 456.0, 323.0);
        frame(&mut e, 20);
        assert_eq!(e.router.names(), ["track_build", "place_items"]);
        let _ = std::fs::remove_dir_all(dir);
        // Not offered until there is a loop to drive.
        let (mut e, dir) = engine_for(DesignerScreen::new(), "noplace");
        frame(&mut e, 90);
        tap(&mut e, 706.0, 323.0);
        frame(&mut e, 20);
        assert_eq!(e.router.names(), ["designer"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_save_and_the_rest_are_refused_while_the_loop_has_an_error() {
        let (mut e, dir) = engine_for(DesignerScreen::close_sample(), "faulty");
        frame(&mut e, 90);
        // Test, Race, Save, Invite, Drive & place.
        for x in [90.0, 244.0, 398.0, 552.0, 706.0] {
            tap(&mut e, x, 323.0);
            frame(&mut e, 20);
            assert_eq!(e.router.names(), ["designer"], "button at {x}");
        }
        assert!(crate::custom_tracks::list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_build_footer_still_saves() {
        let (mut e, dir) = engine_for(DesignerScreen::build_sample(), "build");
        frame(&mut e, 200);
        // Test run, Race bots, Save, Drive & place, Edit, Discard: Save is third of six.
        tap(&mut e, 334.0, 323.0);
        frame(&mut e, 20);
        assert_eq!(crate::custom_tracks::list(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_build_setting_chips_choose_where_the_track_is() {
        let (mut e, dir) = engine_for(DesignerScreen::build_sample(), "setting");
        frame(&mut e, 200);
        // Third chip down the left: Red Rock Canyon.
        tap(&mut e, 90.0, 142.0);
        tap(&mut e, 334.0, 323.0);
        let saved = crate::custom_tracks::list(&dir);
        assert_eq!(saved[0].look, TrackId::RACE_ALL[2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bridge_is_found_in_the_figure_of_eight() {
        let s = DesignerScreen::bridge_sample();
        assert_eq!(s.draft.shaped.as_ref().unwrap().crossings.len(), 1);
        assert!(s.ready());
    }
}
