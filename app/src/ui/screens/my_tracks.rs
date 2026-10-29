//! Saved tracks, their best runs and routes into the plan editor or a race.

use super::common;
use super::designer::{self, DesignerScreen};
use super::{Ctx, Screen, Transition};
use crate::ui::art::{self, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Btn};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Rect, Shape, Stroke};
use race_core::{BotLevel, CustomTrack, Vec2};

pub struct MyTracksScreen {
    t: f32,
    tracks: Vec<CustomTrack>,
    selected: usize,
    page: usize,
    trial: bool,
    bots: usize,
    level: BotLevel,
    refresh: bool,
    demo: bool,
    remove: Option<u64>,
    /// The New track chooser is open.
    chooser: bool,
}

impl Default for MyTracksScreen { fn default() -> Self { Self::new() } }

impl MyTracksScreen {
    pub fn new() -> Self { Self::picker(false, 7, BotLevel::Normal) }
    pub fn picker(trial: bool, bots: usize, level: BotLevel) -> Self {
        Self { t: 0.0, tracks: vec![], selected: 0, page: 0, trial, bots, level, refresh: true, demo: false, remove: None, chooser: false }
    }
    pub fn sample() -> Self { Self { demo: true, ..Self::new() } }
    pub fn empty_sample() -> Self { Self { refresh: false, ..Self::new() } }
    /// Saved tracks with the New track chooser open.
    pub fn chooser_sample() -> Self { Self { chooser: true, ..Self::sample() } }
}

impl MyTracksScreen {
    /// Opens the plan editor (0) or the Paddock run (1) for a new track.
    fn start_new(&mut self, ctx: &mut Ctx, how: usize) -> Transition {
        self.refresh = true;
        if how == 0 { Transition::Push(Box::new(DesignerScreen::new())) }
        else { Transition::Push(Box::new(crate::game::recording::RunTrackScreen::new(ctx.profile))) }
    }
}

impl Screen for MyTracksScreen {
    fn name(&self) -> &'static str { "my_tracks" }
    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if self.refresh {
            self.refresh = false;
            self.tracks = if self.demo { ["Sunset Loop", "Run 1", "Dust Circuit"].into_iter().map(designer::sample_track).collect() } else { crate::custom_tracks::list(ctx.data_dir) };
            self.selected = self.selected.min(self.tracks.len().saturating_sub(1));
            self.page = self.selected / 4;
        }
        Transition::None
    }
    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        if self.chooser { self.chooser = false; return Transition::None; }
        if self.remove.take().is_some() { return Transition::None; }
        feedback::back(ctx); Transition::Pop
    }
    fn on_background(&mut self, _ctx: &mut Ctx) { feedback::on_background(); }
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let mut out = Transition::None;
        common::page(ctx.viewport.screen, ctx.time as f32, ui, 0.18, |ui, _| {
            let hd = widgets::header(ui, safe, "My Tracks", widgets::enter(self.t, 0));
            if hd.back.clicked() { out = self.back(ctx); }
            let body = hd.body;
            let portrait = common::Layout::new(safe).portrait;
            let footer_h = 42.0;
            let footer_total = if portrait { 3.0 * (footer_h + 6.0) } else { footer_h };
            let grid = Rect::from_min_max(body.min, pos2(body.right(), body.bottom() - footer_total - 35.0));
            if self.tracks.is_empty() {
                ui.painter().text(pos2(body.center().x, body.top() + 16.0), Align2::CENTER_CENTER, "Your first track starts here", theme::title(25.0), theme::SAND);
                let area = Rect::from_min_max(body.min + vec2(0.0, 40.0), body.max);
                if self.chooser { self.chooser = false; }
                if let Some(i) = choice_cards(ui, area, "empty") { feedback::tap(ctx); out = self.start_new(ctx, i); }
            } else {
                let gap = 10.0;
                let cols = if portrait { 1 } else { 2 };
                let rows = if portrait { 4 } else { 2 };
                let w = (grid.width() - gap * (cols - 1) as f32) / cols as f32;
                let h = (grid.height() - gap * (rows - 1) as f32) / rows as f32;
                for i in (self.page * 4)..((self.page * 4 + 4).min(self.tracks.len())) {
                    let n = i % 4;
                    let rect = Rect::from_min_size(grid.min + vec2((n % cols) as f32 * (w + gap), (n / cols) as f32 * (h + gap)), vec2(w, h));
                    let track = &self.tracks[i];
                    if track_card(ui, rect, track, self.selected == i, ctx.profile.custom_bests.get(&track.hash()).copied()).clicked() {
                        feedback::tap(ctx); self.selected = i;
                    }
                }
                let page_y = grid.bottom() + 5.0;
                let small = 65.0;
                for (i, label) in ["Prev", "Next"].into_iter().enumerate() {
                    let rect = Rect::from_min_size(pos2(body.right() - small * (2 - i) as f32 - 6.0, page_y), vec2(small - 4.0, 24.0));
                    let enabled = if i == 0 { self.page > 0 } else { (self.page + 1) * 4 < self.tracks.len() };
                    if widgets::button(ui, rect, label, &Btn::ghost(label).enabled(enabled).size(12.0)).clicked() {
                        if i == 0 { self.page -= 1; } else { self.page += 1; }
                        self.selected = self.page * 4;
                    }
                }
                ui.painter().text(pos2(body.left(), page_y + 12.0), Align2::LEFT_CENTER, format!("{} / 50 saved · page {}", self.tracks.len(), self.page + 1), theme::ui_medium(11.0), theme::TEXT_DIM);
            }
            if !self.tracks.is_empty() {
                let labels = [if self.trial { "Test run" } else { "Race bots" }, "Edit", "Invite", "Remove"];
                // Four track actions share the width with one wider New track button.
                let gap = 6.0;
                let unit = if portrait { (body.width() - gap) * 0.5 } else { (body.width() - gap * 4.0) / (labels.len() as f32 + 1.6) };
                for (i, label) in labels.into_iter().enumerate() {
                    let rect = Rect::from_min_size(pos2(body.left() + (if portrait { i % 2 } else { i }) as f32 * (unit + gap), body.bottom() - footer_total + if portrait { (i / 2) as f32 * (footer_h + gap) } else { 0.0 }), vec2(unit, footer_h));
                    if widgets::button(ui, rect, &format!("my-tracks-action-{i}"), &Btn::new(label).size(16.0)).clicked() && self.remove.is_none() && !self.chooser {
                        feedback::tap(ctx);
                        match i {
                            0 => {
                                match common::custom_launch(ctx.profile, &self.tracks[self.selected], self.trial, self.bots, self.level) {
                                    Ok(launch) => { self.refresh = true; out = Transition::Push(crate::game::race_screen(launch)); },
                                    Err(e) => ctx.say(e),
                                }
                            }
                            1 => { self.refresh = true; out = Transition::Push(Box::new(DesignerScreen::edit(self.tracks[self.selected].clone()))); },
                            2 => out = super::lobby::LobbyScreen::invite_custom(ctx, &self.tracks[self.selected]),
                            _ => self.remove = Some(self.tracks[self.selected].hash()),
                        }
                    }
                }
                let rect = Rect::from_min_max(pos2(if portrait { body.left() } else { body.right() - unit * 1.6 }, body.bottom() - footer_h), body.max);
                if widgets::button(ui, rect, "my-tracks-new", &Btn::primary("New track").icon(Icon::Plus).size(17.0)).clicked() && self.remove.is_none() && !self.chooser {
                    feedback::tap(ctx);
                    self.chooser = true;
                }
            }
            if self.chooser {
                // Dim everything; a tap outside the card closes it.
                ui.painter().rect_filled(safe, 0.0, rgba(6, 2, 14, 0.9));
                let veil = ui.interact(safe, ui.id().with("chooser-veil"), egui::Sense::click());
                let card = Rect::from_center_size(safe.center(), vec2((safe.width() - 24.0).min(640.0), (safe.height() - 16.0).min(if portrait { 560.0 } else { 290.0 })));
                widgets::panel_solid(ui.painter(), card, 18.0);
                ui.painter().text(card.center_top() + vec2(0.0, 26.0), Align2::CENTER_CENTER, "New track", theme::title(22.0), theme::TEXT);
                let area = Rect::from_min_max(card.min + vec2(12.0, 46.0), pos2(card.right() - 12.0, card.bottom() - 58.0));
                let picked = choice_cards(ui, area, "chooser");
                let cancel = Rect::from_center_size(pos2(card.center().x, card.bottom() - 28.0), vec2(150.0, 38.0));
                let cancelled = widgets::button(ui, cancel, "chooser-cancel", &Btn::ghost("Cancel").size(16.0)).clicked();
                if let Some(i) = picked { feedback::tap(ctx); self.chooser = false; out = self.start_new(ctx, i); }
                else if cancelled || (veil.clicked() && !card.contains(veil.interact_pointer_pos().unwrap_or(card.center()))) { feedback::back(ctx); self.chooser = false; }
            }
            if let Some(hash) = self.remove {
                ui.painter().rect_filled(safe, 0.0, rgba(6, 2, 14, 0.9));
                let card = Rect::from_center_size(safe.center(), vec2(350.0, 138.0));
                widgets::panel_solid(ui.painter(), card, 18.0);
                ui.painter().text(card.center_top() + vec2(0.0, 28.0), Align2::CENTER_CENTER, "Remove this saved track?", theme::title(20.0), theme::TEXT);
                ui.painter().text(card.center_top() + vec2(0.0, 56.0), Align2::CENTER_CENTER, "Your best time stays in your profile", theme::ui_medium(11.0), theme::TEXT_DIM);
                for (i, label) in ["Keep", "Remove"].into_iter().enumerate() {
                    let rect = Rect::from_min_size(card.left_bottom() + vec2(12.0 + i as f32 * 166.0, -54.0), vec2(158.0, 42.0));
                    if widgets::button(ui, rect, &format!("track-remove-{i}"), &Btn::new(label).size(19.0)).clicked() {
                        if i == 1 {
                            if self.demo { self.tracks.retain(|t| t.hash() != hash); }
                            else { match crate::custom_tracks::remove(ctx.data_dir, hash) { Ok(()) => self.refresh = true, Err(e) => ctx.say(e) } }
                        }
                        self.remove = None;
                    }
                }
            }
        });
        out
    }
}

pub(super) fn track_card(ui: &mut egui::Ui, rect: Rect, track: &CustomTrack, selected: bool, best: Option<f32>) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(("saved-track", track.hash())), egui::Sense::click());
    let p = ui.painter().clone();
    widgets::panel(&p, rect, 14.0);
    if selected { p.rect_stroke(rect, 14.0, Stroke::new(2.0_f32, theme::ACCENT), egui::StrokeKind::Inside); }
    let map = Rect::from_min_max(rect.min + vec2(12.0, 12.0), pos2(rect.left() + rect.width() * 0.4, rect.bottom() - 12.0));
    draw_outline(&p, map, &track.points_m(), theme::SAND);
    let tx = map.right() + 10.0;
    let name_size = art::fit_size(&p, &track.name, theme::title, 20.0, rect.right() - tx - 10.0);
    p.text(pos2(tx, rect.top() + 23.0), Align2::LEFT_CENTER, &track.name, theme::title(name_size), theme::TEXT);
    p.text(pos2(tx, rect.center().y + 4.0), Align2::LEFT_CENTER, format!("{} laps · {} m wide", track.laps, track.width_dm / 10), theme::ui_medium(11.0), theme::TEXT_DIM);
    let text = best.map(|t| format!("Best {}", common::fmt_time(t))).unwrap_or_else(|| "No time yet".into());
    p.text(pos2(tx, rect.bottom() - 17.0), Align2::LEFT_CENTER, text, theme::ui_bold(11.0), theme::GOLD);
    resp
}

pub(super) fn draw_outline(p: &egui::Painter, rect: Rect, points: &[Vec2], colour: egui::Color32) {
    if points.len() < 2 { return; }
    let (mut lo, mut hi) = (points[0], points[0]);
    for v in points { lo = lo.min(*v); hi = hi.max(*v); }
    let centre = (lo + hi) * 0.5;
    let ext = hi - lo;
    let scale = (rect.width() / ext.x.max(1.0)).min(rect.height() / ext.y.max(1.0)) * 0.85;
    let pts: Vec<_> = points.iter().map(|v| rect.center() + vec2(-(v.x - centre.x) * scale, -(v.y - centre.y) * scale)).collect();
    p.add(Shape::closed_line(pts.clone(), Stroke::new(4.5_f32, theme::OUTLINE)));
    p.add(Shape::closed_line(pts.clone(), Stroke::new(2.5_f32, colour)));
    p.circle_filled(pts[0], 3.5, theme::ACCENT);
}

/// The two ways to make a track as big cards side by side in `area`:
/// "Draw it" (0) and "Run it" (1). Returns the one tapped.
fn choice_cards(ui: &mut egui::Ui, area: Rect, salt: &str) -> Option<usize> {
    let gap = 12.0;
    let stacked = area.width() < 400.0;
    let w = if stacked { area.width() } else { (area.width() - gap) * 0.5 };
    let h = if stacked { (area.height() - gap) * 0.5 } else { area.height() };
    let mut picked = None;
    for (i, (title, text)) in [("Draw it", "Sketch a loop with your finger"), ("Run it", "Ride a loop and it becomes the road")].into_iter().enumerate() {
        let rect = Rect::from_min_size(area.min + if stacked { vec2(0.0, i as f32 * (h + gap)) } else { vec2(i as f32 * (w + gap), 0.0) }, vec2(w, h));
        let resp = ui.interact(rect, ui.id().with(("new-track-card", salt, i)), egui::Sense::click());
        let (scale, sink) = widgets::press_fx(ui, ui.id().with(("new-track-card", salt, i)), &resp);
        let rect = Rect::from_center_size(rect.center(), rect.size() * scale);
        let p = ui.painter().clone();
        widgets::panel(&p, rect, 16.0);
        p.rect_stroke(rect, 16.0, Stroke::new(if resp.is_pointer_button_down_on() { 2.5_f32 } else { 1.5 }, if sink > 0.0 { theme::GOLD } else { rgba(0xE8, 0xC6, 0x8A, 0.45) }), egui::StrokeKind::Inside);
        let pic = Rect::from_min_max(rect.min + vec2(12.0, 10.0), pos2(rect.right() - 12.0, rect.bottom() - 62.0));
        let loop_pts: Vec<_> = (0..=48).map(|n| { let a = n as f32 / 48.0 * std::f32::consts::TAU; Vec2::new(a.cos() * 1.5, a.sin() * 1.0) }).collect();
        // Draw it: an open line with a finger dot at its loose end. Run it: a closed road with the ostrich on it.
        let shown = if i == 0 { &loop_pts[..40] } else { &loop_pts[..48] };
        let scale = (pic.width() / 3.4).min(pic.height() / 2.4);
        let at = |v: &Vec2| pic.center() + vec2(-v.x * scale, -v.y * scale);
        let pts: Vec<_> = shown.iter().map(at).collect();
        let road = |pts: Vec<egui::Pos2>, stroke| if i == 0 { Shape::line(pts, stroke) } else { Shape::closed_line(pts, stroke) };
        p.add(road(pts.clone(), Stroke::new(9.0_f32, theme::OUTLINE)));
        p.add(road(pts.clone(), Stroke::new(5.0_f32, theme::SAND)));
        if i == 0 {
            let end = *pts.last().unwrap();
            p.circle_filled(end, 7.0, theme::ACCENT);
            p.circle_stroke(end, 10.0, Stroke::new(2.0_f32, theme::GOLD));
        } else {
            icons::draw(&p, Icon::Ostrich, pts[8], 26.0, theme::TEXT);
        }
        let size = art::fit_size(&p, title, theme::title, 23.0, rect.width() - 20.0);
        p.text(pos2(rect.center().x, rect.bottom() - 42.0), Align2::CENTER_CENTER, title, theme::title(size), theme::TEXT);
        let small = art::fit_size(&p, text, theme::ui_medium, 12.0, rect.width() - 20.0);
        p.text(pos2(rect.center().x, rect.bottom() - 20.0), Align2::CENTER_CENTER, text, theme::ui_medium(small), theme::TEXT_DIM);
        if resp.clicked() { picked = Some(i); }
    }
    picked
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

    fn engine(screen: MyTracksScreen) -> Engine {
        let dir = std::env::temp_dir().join(format!("stampede-my-tracks-{}", std::process::id()));
        Engine::new(Box::new(screen), Profile::default(), dir, Options { shot: Some("my_tracks".into()), ..Options::default() }, Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()))
    }
    fn frame(e: &mut Engine, n: usize) { for _ in 0..n { e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default()); } }
    fn tap(e: &mut Engine, x: f64, y: f64) {
        e.touch_event(101, TouchPhase::Started, (x * 3.0, y * 3.0)); frame(e, 2);
        e.touch_event(101, TouchPhase::Ended, (x * 3.0, y * 3.0)); frame(e, 2);
    }

    #[test]
    fn new_track_opens_a_chooser_that_pushes_the_designer_or_the_run_screen() {
        for (card, expected) in [((243.0, 174.0), "designer"), ((557.0, 174.0), "run_track")] {
            let mut e = engine(MyTracksScreen::sample());
            frame(&mut e, 60);
            tap(&mut e, 678.0, 323.0);
            assert_eq!(e.router.names(), ["my_tracks"]);
            tap(&mut e, card.0, card.1);
            frame(&mut e, 60);
            assert_eq!(e.router.names(), ["my_tracks", expected]);
        }
    }

    #[test]
    fn back_and_cancel_close_the_chooser_without_leaving() {
        let mut e = engine(MyTracksScreen::sample());
        frame(&mut e, 60);
        tap(&mut e, 678.0, 323.0);
        e.request_back();
        frame(&mut e, 60);
        assert_eq!(e.router.names(), ["my_tracks"]);
        // Closed: the card spot now selects a saved track instead of starting one.
        tap(&mut e, 243.0, 174.0);
        frame(&mut e, 60);
        assert_eq!(e.router.names(), ["my_tracks"]);
        tap(&mut e, 678.0, 323.0);
        tap(&mut e, 400.0, 281.0);
        tap(&mut e, 243.0, 174.0);
        frame(&mut e, 60);
        assert_eq!(e.router.names(), ["my_tracks"]);
    }

    #[test]
    fn the_empty_state_shows_both_cards_directly() {
        for (card, expected) in [((204.0, 224.0), "designer"), ((595.0, 224.0), "run_track")] {
            let mut e = engine(MyTracksScreen::empty_sample());
            frame(&mut e, 60);
            tap(&mut e, card.0, card.1);
            frame(&mut e, 60);
            assert_eq!(e.router.names(), ["my_tracks", expected]);
        }
    }
}
