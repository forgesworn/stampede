//! The race HUD (SPEC 13) and the on-screen controls, drawn with egui shapes
//! straight onto a painter (touches are read separately in `controls.rs`).
//! Everything is laid out inside `Viewport::safe`; the middle of the screen is
//! left clear for the road.

use super::controls::{Btn, Circle, Controller, Layout, PAD_FULL};
use super::icons::{self, stroke};
use crate::platform::data_dir::SteerMode;
use crate::ui::theme;
use egui::{pos2, vec2, Align2, Color32, Painter, Pos2, Rect, Shape, Stroke};
use glam::Vec3;
use race_core::{HintControl, Item, Track};
use race_render::ease_out_elastic;

pub const TIER_COLORS: [Color32; 3] = [Color32::from_rgb(70, 160, 255), Color32::from_rgb(255, 150, 40), Color32::from_rgb(196, 92, 255)];

pub fn place_color(place: u8) -> Color32 {
    match place {
        1 => Color32::from_rgb(255, 208, 64),
        2 => Color32::from_rgb(214, 224, 240),
        3 => Color32::from_rgb(222, 146, 84),
        _ => theme::TEXT,
    }
}

pub fn ordinal(n: u8) -> (String, &'static str) {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    (n.to_string(), suffix)
}

pub fn fmt_time(t: f32) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0).floor() as u32;
    format!("{}:{:05.2}", m, t - m as f32 * 60.0)
}

/// Text with a soft drop shadow.
pub fn shadow_text(p: &Painter, pos: Pos2, align: Align2, text: &str, font: egui::FontId, color: Color32) -> Rect {
    let a = (color.a() as f32 * 0.75) as u8;
    let sh = Color32::from_black_alpha(a);
    for d in [vec2(-1.2, 0.0), vec2(1.2, 0.0), vec2(0.0, -1.2), vec2(0.0, 1.2), vec2(1.6, 2.2)] {
        p.text(pos + d, align, text, font.clone(), sh);
    }
    p.text(pos, align, text, font, color)
}

// ------------------------------------------------------------- minimap ----

/// The track outline, prepared once.
pub struct MiniMap {
    pts: Vec<Pos2>,
    min: Pos2,
    max: Pos2,
    start: (Pos2, Pos2),
    /// Open ground (the Paddock): the outline is the field's edge, drawn as a plain disc.
    field: bool,
    /// Overpass spans, drawn again over the rest of the track so the upper branch reads as crossing the lower.
    bridges: Vec<Vec<Pos2>>,
}

fn top_down(v: Vec3) -> Pos2 {
    pos2(-v.x, -v.z)
}

impl MiniMap {
    pub fn new(track: &Track) -> MiniMap {
        let step = (track.samples.len() / 140).max(1);
        let field = track.id.is_open_field();
        // On open ground the centreline is a ring in the middle of a 270 m band: outline the band's edge instead.
        let pts: Vec<Pos2> = if field {
            track.samples.iter().step_by(step).map(|s| top_down(s.pos + s.right * (s.width * 0.5 + s.shoulder_r))).collect()
        } else {
            track.samples.iter().step_by(step).map(|s| top_down(s.pos)).collect()
        };
        let bridges: Vec<Vec<Pos2>> = track
            .bridges
            .iter()
            .map(|b| {
                let (a, z) = (b.s0 - 5.0, b.s1 + 5.0);
                let n = ((z - a) / track.ds).ceil() as usize + 1;
                (0..n).map(|k| top_down(track.sample_at(track.wrap_s(a + k as f32 * track.ds)).pos)).collect()
            })
            .collect();
        let (mut min, mut max) = (pos2(f32::MAX, f32::MAX), pos2(f32::MIN, f32::MIN));
        for p in &pts {
            min = pos2(min.x.min(p.x), min.y.min(p.y));
            max = pos2(max.x.max(p.x), max.y.max(p.y));
        }
        let s0 = &track.samples[0];
        let a = top_down(s0.pos + s0.right * s0.width * 0.5);
        let b = top_down(s0.pos - s0.right * s0.width * 0.5);
        MiniMap { pts, min, max, start: (a, b), field, bridges }
    }

    /// Map a top-down point into `inner` (aspect preserved, centred).
    fn fit(&self, inner: Rect) -> impl Fn(Pos2) -> Pos2 {
        let (min, max) = (self.min, self.max);
        let ext = vec2((max.x - min.x).max(1.0), (max.y - min.y).max(1.0));
        let k = (inner.width() / ext.x).min(inner.height() / ext.y);
        let origin = inner.center() - ext * k * 0.5;
        move |p: Pos2| origin + (p - min) * k
    }

    pub fn world_to_rect(&self, w: Vec3, inner: Rect) -> Pos2 {
        (self.fit(inner))(top_down(w))
    }

    pub fn draw(&self, p: &Painter, rect: Rect, dots: &[Dot]) {
        p.rect_filled(rect, 10.0, Color32::from_rgba_unmultiplied(14, 8, 28, 150));
        p.rect_stroke(rect, 10.0, stroke(1.0, Color32::from_white_alpha(40)), egui::StrokeKind::Inside);
        let inner = rect.shrink(9.0);
        let f = self.fit(inner);
        let line: Vec<Pos2> = self.pts.iter().map(|&q| f(q)).collect();
        if self.field {
            p.add(Shape::convex_polygon(line, Color32::from_rgba_unmultiplied(222, 205, 235, 80), stroke(2.0, Color32::from_rgb(222, 205, 235))));
        } else {
            p.add(Shape::closed_line(line.clone(), stroke(6.0, Color32::from_black_alpha(190))));
            p.add(Shape::closed_line(line, stroke(3.0, Color32::from_rgb(222, 205, 235))));
            for span in &self.bridges {
                let l: Vec<Pos2> = span.iter().map(|&q| f(q)).collect();
                p.add(Shape::line(l.clone(), stroke(6.0, Color32::from_black_alpha(220))));
                p.add(Shape::line(l, stroke(3.0, Color32::from_rgb(255, 240, 205))));
            }
            p.line_segment([f(self.start.0), f(self.start.1)], stroke(2.0, Color32::WHITE));
        }
        for d in dots.iter().filter(|d| !d.local) {
            let c = f(top_down(d.pos));
            p.circle_filled(c, 3.6, Color32::from_black_alpha(200));
            p.circle_filled(c, 2.6, d.color);
        }
        for d in dots.iter().filter(|d| d.local) {
            let c = f(top_down(d.pos));
            p.circle_filled(c, 6.5, Color32::from_black_alpha(190));
            p.circle_filled(c, 5.2, Color32::WHITE);
            p.circle_filled(c, 3.6, d.color);
        }
    }
}

#[derive(Clone, Copy)]
pub struct Dot {
    pub pos: Vec3,
    pub color: Color32,
    pub local: bool,
}

// -------------------------------------------------------------- banners ----

#[derive(Clone, Debug)]
pub struct Banner {
    pub text: String,
    pub sub: Option<String>,
    pub color: Color32,
    pub t: f32,
    pub dur: f32,
    pub big: bool,
    /// Lesson stars to draw under the text (0-3).
    pub stars: Option<u8>,
}

impl Banner {
    pub fn new(text: impl Into<String>, color: Color32, dur: f32) -> Banner {
        Banner { text: text.into(), sub: None, color, t: 0.0, dur, big: false, stars: None }
    }
}

/// A five-point star as a triangle fan.
pub fn star(p: &Painter, c: Pos2, r: f32, color: Color32) {
    let mut m = egui::Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    m.vertices.push(egui::epaint::Vertex { pos: c, uv, color });
    for i in 0..10 {
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        let rr = if i % 2 == 0 { r } else { r * 0.45 };
        m.vertices.push(egui::epaint::Vertex { pos: c + vec2(a.cos(), a.sin()) * rr, uv, color });
    }
    for i in 0..10u32 {
        m.indices.extend([0, 1 + i, 1 + (i + 1) % 10]);
    }
    p.add(Shape::mesh(m));
}

fn draw_banner(p: &Painter, centre: Pos2, b: &Banner) {
    let pop = ease_out_elastic((b.t / 0.35).clamp(0.0, 1.0));
    let fade = ((b.dur - b.t) / 0.3).clamp(0.0, 1.0);
    if fade <= 0.0 {
        return;
    }
    let size = if b.big { 54.0 } else { 36.0 } * (0.5 + 0.5 * pop);
    let col = b.color.gamma_multiply(fade);
    shadow_text(p, centre, Align2::CENTER_CENTER, &b.text, theme::title(size), col);
    if let Some(sub) = &b.sub {
        shadow_text(p, centre + vec2(0.0, size * 0.75), Align2::CENTER_CENTER, sub, theme::numbers(if b.big { 22.0 } else { 16.0 }), theme::TEXT.gamma_multiply(fade));
    }
    if let Some(n) = b.stars {
        for i in 0..3 {
            let k = ease_out_elastic(((b.t - 0.25 - 0.18 * i as f32) / 0.4).clamp(0.0, 1.0));
            let on = (i as u8) < n;
            let col = if on { Color32::from_rgb(255, 208, 64) } else { Color32::from_white_alpha(50) };
            star(p, centre + vec2((i as f32 - 1.0) * 46.0, size * 0.75 + 36.0), 20.0 * k, col.gamma_multiply(fade));
        }
    }
}

// ---------------------------------------------------------------- frame ----

#[derive(Clone, Debug)]
pub struct LessonHud {
    pub number: u8,
    pub title: &'static str,
    pub instruction: &'static str,
    pub hint: &'static str,
    pub control: HintControl,
    pub progress: u32,
    pub goal: u32,
    pub time: f32,
    pub limit: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
pub struct Countdown {
    /// Riders will run and jump onto their ostriches at GO.
    pub run_start: bool,
    /// Seconds to GO.
    pub left: f32,
    /// The player revved inside the start-boost window.
    pub revved: bool,
    pub window: f32,
}

pub struct Frame {
    pub spectating: bool,
    pub recording: bool,
    pub safe: Rect,
    pub layout: Layout,
    pub steer_mode: SteerMode,
    pub place: u8,
    pub racers: usize,
    pub lap: u8,
    pub laps: u8,
    pub time: f32,
    pub coins: u32,
    pub item: Option<(Item, u8)>,
    /// Power-ups queued behind the held one (taken with a Shared Cake).
    pub item_queued: u8,
    /// A timed item this racer fired that is still running (the item, and the
    /// fraction left 0..1): counted down on the item button; a held item waits.
    pub item_active: Option<(Item, f32)>,
    /// Item roulette: the icon cycling now, and 0..1 progress.
    pub roulette: Option<(Item, f32)>,
    pub heat: f32,
    pub overheated: bool,
    pub lasering: bool,
    pub speed_kmh: f32,
    pub speed_frac: f32,
    pub boosting: bool,
    pub drifting: bool,
    pub drift_charge: f32,
    pub drift_tier: u8,
    pub tiers: [f32; 3],
    pub countdown: Option<Countdown>,
    pub go_t: Option<f32>,
    pub wrong_way: bool,
    pub lesson: Option<LessonHud>,
    /// Set in an open-ground drill: the level badge, strikes, stars and pop-ups replace place, laps, clock, coins and the minimap.
    pub drill: Option<super::drill_hud::DrillHud>,
    /// Set in a Challenge: the score block replaces place, timer, coins and (mostly) the lap counter.
    pub challenge: Option<super::challenge_hud::ChallengeHud>,
    /// Set in a battle (and Last Feather): feathers, ranking and clock replace place, laps and the race clock.
    pub battle: Option<super::battle_hud::BattleHud>,
    /// Time Trial with a ghost: the split and the tag.
    pub ghost: Option<super::ghost_hud::GhostHud>,
    pub mirror: Option<(Rect, f32)>,
    pub dots: Vec<Dot>,
    pub camera_name: &'static str,
    pub camera_flash: f32,
    pub place_pulse: f32,
    pub place_gain: bool,
    pub final_lap_t: f32,
    pub banner: Option<Banner>,
    pub fade: f32,
    pub magnet: bool,
    pub shield: bool,
    /// Pinned against a wall: tell the player the ostrich is backing out.
    pub unstick_hint: bool,
}

const RIM: Color32 = Color32::from_rgb(255, 200, 120);
fn panel_fill(press: f32) -> Color32 {
    theme::mix(Color32::from_rgb(26, 14, 46), Color32::from_rgb(110, 56, 40), press * 0.7).gamma_multiply(0.82)
}

/// One round button: squashes when pressed, glows when active.
fn round_button(p: &Painter, c: Circle, press: f32, active: bool, rim: Color32, icon: impl FnOnce(Pos2, f32)) {
    let sc = 1.0 - 0.13 * press;
    let r = c.r * sc;
    if active {
        p.circle_filled(c.c, r * 1.28, rim.gamma_multiply(0.22));
        p.circle_stroke(c.c, r * 1.12, stroke(2.0, rim.gamma_multiply(0.5)));
    }
    p.circle_filled(c.c + vec2(0.0, 2.5), r, Color32::from_black_alpha(80));
    p.circle_filled(c.c, r, panel_fill(press));
    if press > 0.0 {
        p.circle_filled(c.c, r, rim.gamma_multiply(0.18 * press));
    }
    p.circle_stroke(c.c, r, stroke(2.4, rim.gamma_multiply(0.9)));
    icon(c.c, r);
}

fn small_label(p: &Painter, pos: Pos2, text: &str, size: f32, col: Color32) {
    p.text(pos, Align2::CENTER_CENTER, text, theme::numbers(size), col);
}

pub fn draw(p: &Painter, f: &Frame, ctl: &Controller, minimap: &MiniMap, t: f32) {
    let s = f.safe;
    let l = &f.layout;

    // --- Minimap and place (top-left) ---
    let mini = Rect::from_min_size(pos2(s.left() + 8.0, s.top() + 8.0), vec2(108.0, 86.0));
    let drilling = f.drill.is_some() || f.recording;
    if !drilling {
        minimap.draw(p, mini, &f.dots);
    }
    if f.challenge.is_none() && f.battle.is_none() && !drilling {
        let (num, suf) = ordinal(f.place);
        let k = 1.0 + 0.32 * f.place_pulse;
        let pc = place_color(f.place);
        let base = pos2(mini.right() + 12.0, s.top() + 38.0);
        let big = theme::numbers(46.0 * k);
        let r = shadow_text(p, base, Align2::LEFT_CENTER, &num, big, pc);
        shadow_text(p, pos2(r.right() + 2.0, base.y - 8.0), Align2::LEFT_CENTER, suf, theme::numbers(19.0), pc);
        shadow_text(p, pos2(r.left() + 2.0, base.y + 26.0), Align2::LEFT_CENTER, &format!("OF {}", f.racers), theme::ui_bold(11.0), theme::TEXT);
        if f.place_pulse > 0.05 {
            let col = if f.place_gain { Color32::from_rgb(110, 240, 140) } else { Color32::from_rgb(255, 110, 100) };
            let a = f.place_pulse;
            let c = pos2(r.right() + 40.0, base.y - 2.0);
            // An arrow: up when we gained a place, down when we lost one.
            let d = if f.place_gain { -1.0 } else { 1.0 };
            let pts = vec![c + vec2(-7.0, -5.0 * d), c + vec2(7.0, -5.0 * d), c + vec2(0.0, 7.0 * d)];
            p.add(Shape::convex_polygon(pts, col.gamma_multiply(a), Stroke::NONE));
        }
    }

    // --- Lap, time, coins (right, under the top bar) ---
    let rx = s.right() - 12.0;
    let show_lap = f.lesson.is_none() && !drilling && f.battle.is_none() && f.challenge.as_ref().is_none_or(|c| c.show_lap);
    if show_lap {
        shadow_text(p, pos2(rx, s.top() + 62.0), Align2::RIGHT_CENTER, &format!("LAP {}/{}", f.lap.max(1).min(f.laps), f.laps), theme::numbers(17.0), theme::TEXT);
    }
    // Time and coins give way to the challenge score block.
    let cy = if f.challenge.is_none() { s.top() + 112.0 } else { s.top() + if show_lap { 78.0 } else { 62.0 } };
    if f.challenge.is_none() && !drilling {
        if f.battle.is_none() {
            shadow_text(p, pos2(rx, s.top() + 86.0), Align2::RIGHT_CENTER, &fmt_time(f.time), theme::numbers(22.0), Color32::from_rgb(255, 232, 170));
        }
        let txt = shadow_text(p, pos2(rx, cy), Align2::RIGHT_CENTER, &format!("{}", f.coins), theme::numbers(19.0), theme::TEXT);
        icons::bitcoin(p, pos2(txt.left() - 12.0, cy), 9.5);
    }
    let cy = if f.challenge.is_none() { cy } else { cy - 22.0 };
    if f.magnet {
        shadow_text(p, pos2(rx, cy + 22.0), Align2::RIGHT_CENTER, "MAGNET", theme::ui_bold(11.0), Color32::from_rgb(240, 90, 100));
    }
    if f.shield {
        shadow_text(p, pos2(rx, cy + if f.magnet { 38.0 } else { 22.0 }), Align2::RIGHT_CENTER, "SHIELD", theme::ui_bold(11.0), Color32::from_rgb(190, 150, 255));
    }

    if f.spectating {
        top_bar(p, f, ctl);
        if let Some(b) = &f.banner {
            draw_banner(p, pos2(s.center().x, s.top() + s.height() * 0.34), b);
        }
        return;
    }

    // --- Speed bar + drift tier (bottom centre) ---
    let cx = s.center().x;
    let bar = Rect::from_center_size(pos2(cx, s.bottom() - 15.0), vec2(190.0, 8.0));
    p.rect_filled(bar.expand(2.5), 7.0, Color32::from_black_alpha(130));
    let frac = (f.speed_frac / 1.4).clamp(0.0, 1.0);
    let fill = Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height()));
    let col = if f.boosting { Color32::from_rgb(120, 220, 255) } else { theme::mix(theme::SAND, Color32::from_rgb(255, 80, 60), (frac * 1.3 - 0.35).clamp(0.0, 1.0)) };
    p.rect_filled(fill, 4.0, col);
    if f.boosting {
        p.rect_stroke(bar.expand(2.5), 7.0, stroke(1.5, Color32::from_rgb(150, 230, 255).gamma_multiply(0.6 + 0.4 * (t * 18.0).sin())), egui::StrokeKind::Outside);
    }
    shadow_text(p, pos2(bar.left() - 6.0, bar.center().y), Align2::RIGHT_CENTER, &format!("{:.0}", f.speed_kmh), theme::numbers(15.0), theme::TEXT);
    shadow_text(p, pos2(bar.right() + 6.0, bar.center().y), Align2::LEFT_CENTER, "KM/H", theme::ui_bold(10.0), theme::TEXT);
    if f.drifting {
        let w = 190.0 / 3.0 - 3.0;
        for i in 0..3 {
            let x0 = bar.left() + i as f32 * (w + 4.5);
            let seg = Rect::from_min_size(pos2(x0, bar.top() - 18.0), vec2(w, 6.0));
            let lo = if i == 0 { 0.0 } else { f.tiers[i - 1] };
            let hi = f.tiers[i];
            let fillf = ((f.drift_charge - lo) / (hi - lo)).clamp(0.0, 1.0);
            p.rect_filled(seg, 3.0, Color32::from_black_alpha(130));
            p.rect_filled(Rect::from_min_size(seg.min, vec2(seg.width() * fillf, seg.height())), 3.0, TIER_COLORS[i].gamma_multiply(if fillf >= 1.0 { 1.0 } else { 0.8 }));
        }
    }

    // --- Steer pad ---
    if f.steer_mode != SteerMode::Buttons {
        match ctl.pad_state() {
            Some((c, k)) => {
                let dx = (k.x - c.x).clamp(-PAD_FULL, PAD_FULL);
                let knob = pos2(c.x + dx, c.y + (k.y - c.y).clamp(-18.0, 18.0));
                p.circle_filled(c, 54.0, Color32::from_white_alpha(18));
                p.circle_stroke(c, 54.0, stroke(2.0, Color32::from_white_alpha(70)));
                for sgn in [-1.0f32, 1.0] {
                    let e = c + vec2(sgn * PAD_FULL, 0.0);
                    p.circle_stroke(e, 7.0, stroke(1.5, Color32::from_white_alpha(40)));
                }
                p.line_segment([c, knob], stroke(3.0, Color32::from_white_alpha(60)));
                p.circle_filled(knob, 25.0, theme::ACCENT.gamma_multiply(0.75));
                p.circle_stroke(knob, 25.0, stroke(2.5, Color32::from_white_alpha(200)));
            }
            None => {
                let c = l.pad_rest;
                p.circle_stroke(c, 42.0, stroke(2.0, Color32::from_white_alpha(46)));
                for sgn in [-1.0f32, 1.0] {
                    icons::glyph(p, if sgn < 0.0 { "left" } else { "right" }, c + vec2(sgn * 26.0, 0.0), 9.0, Color32::from_white_alpha(70));
                }
            }
        }
    }
    if f.steer_mode == SteerMode::Tilt {
        let label = if ctl.tilt_ready() { "TILT · PAD ALSO WORKS" } else { "TILT WAITING · USE PAD" };
        small_label(p, l.pad_rest + vec2(0.0, 62.0), label, 9.0, theme::TEXT_DIM);
        if let Some(c) = l.circle(Btn::Centre) {
            round_button(p, c, ctl.pressed_amount(Btn::Centre), false, RIM, |cc, r| {
                p.circle_stroke(cc + vec2(0.0, -5.0), r * 0.28, stroke(1.5, theme::TEXT));
                p.line_segment([cc + vec2(-11.0, -5.0), cc + vec2(11.0, -5.0)], stroke(1.5, theme::TEXT));
                p.line_segment([cc + vec2(0.0, -16.0), cc + vec2(0.0, 6.0)], stroke(1.5, theme::TEXT));
                small_label(p, cc + vec2(0.0, r * 0.55), "CENTRE", 8.0, theme::TEXT);
            });
        }
    }

    // --- Buttons ---
    let press = |b: Btn| ctl.pressed_amount(b);
    // Lessons pulse their control all the way; a drill only while its intro card is up.
    let hint = f.lesson.as_ref().map(|ls| ls.control).or_else(|| f.drill.as_ref().filter(|d| d.age < super::drill_hud::INTRO_SECS).map(|d| d.id.control()));
    let pulse = 0.5 + 0.5 * (t * 6.0).sin();
    let hint_ring = |b: Btn| {
        if let Some(c) = l.circle(b) {
            p.circle_stroke(c.c, c.r + 5.0 + 4.0 * pulse, stroke(3.0, theme::ACCENT.gamma_multiply(0.5 + 0.5 * pulse)));
        }
    };
    if let Some(c) = l.circle(Btn::Drift) {
        let tier_col = if f.drifting && f.drift_tier > 0 { TIER_COLORS[f.drift_tier as usize - 1] } else { RIM };
        let active = ctl.is_held(Btn::Drift) || f.drifting;
        round_button(p, c, press(Btn::Drift), active, tier_col, |cc, r| {
            icons::glyph(p, "drift", cc + vec2(0.0, -r * 0.15), r * 0.55, Color32::WHITE);
            small_label(p, cc + vec2(0.0, r * 0.52), "DRIFT", 13.0, theme::TEXT);
        });
        if f.drifting {
            let next = f.tiers.get(f.drift_tier as usize).copied();
            let lo = if f.drift_tier == 0 { 0.0 } else { f.tiers[f.drift_tier as usize - 1] };
            let frac = match next {
                Some(hi) => ((f.drift_charge - lo) / (hi - lo)).clamp(0.0, 1.0),
                None => 1.0,
            };
            let col = TIER_COLORS[(f.drift_tier as usize).min(2)];
            icons::ring_arc(p, c.c, c.r + 7.0, frac, 4.0, col);
        }
    }
    if let Some(c) = l.circle(Btn::Jump) {
        round_button(p, c, press(Btn::Jump), ctl.is_held(Btn::Jump), RIM, |cc, r| {
            icons::glyph(p, "jump", cc + vec2(0.0, -r * 0.12), r * 0.6, Color32::WHITE);
            small_label(p, cc + vec2(0.0, r * 0.55), "JUMP", 10.5, theme::TEXT);
        });
    }
    if let Some(c) = l.circle(Btn::Laser) {
        let hot = if f.overheated { Color32::from_rgb(255, 60, 60) } else { Color32::from_rgb(255, 90, 70) };
        round_button(p, c, press(Btn::Laser), f.lasering, hot, |cc, r| {
            icons::glyph(p, "eye", cc + vec2(0.0, -r * 0.1), r * 0.62, Color32::WHITE);
            small_label(p, cc + vec2(0.0, r * 0.58), "LASER", 10.5, theme::TEXT);
        });
        let hc = if f.overheated { Color32::from_rgb(255, 60, 60).gamma_multiply(0.6 + 0.4 * (t * 16.0).sin().abs()) } else { theme::mix(Color32::from_rgb(255, 220, 90), Color32::from_rgb(255, 50, 40), f.heat) };
        p.circle_stroke(c.c, c.r + 6.0, stroke(3.0, Color32::from_black_alpha(90)));
        icons::ring_arc(p, c.c, c.r + 6.0, if f.overheated { 1.0 } else { f.heat }, 4.0, hc);
    }
    if let Some(c) = l.circle(Btn::Item) {
        let holding = f.item.is_some() || f.roulette.is_some();
        let running = f.item_active.is_some();
        round_button(p, c, press(Btn::Item), holding || running, Color32::from_rgb(255, 214, 120), |cc, r| {
            if let Some((it, prog)) = f.roulette {
                let slide = (prog * 30.0).fract();
                icons::item(p, it, cc + vec2(0.0, (slide - 0.5) * r * 0.5), r * 0.62, t);
            } else if let Some((it, uses)) = f.item {
                let bob = if running { 0.0 } else { (t * 4.0).sin() * 0.03 * r };
                icons::item(p, it, cc + vec2(0.0, bob), r * 0.66, t);
                if uses > 1 {
                    let bc = cc + vec2(r * 0.62, r * 0.62);
                    p.circle_filled(bc, r * 0.34, Color32::from_black_alpha(190));
                    p.text(bc, Align2::CENTER_CENTER, format!("{uses}"), theme::numbers(r * 0.5), theme::ACCENT);
                }
                if running {
                    // Held, but not usable until the running item ends.
                    p.circle_filled(cc, r * 0.94, Color32::from_black_alpha(120));
                }
                if f.item_queued > 0 {
                    // More waiting behind this one: "+N" at the top left.
                    let bc = cc + vec2(-r * 0.66, -r * 0.62);
                    let label = format!("+{}", f.item_queued);
                    p.rect_filled(Rect::from_center_size(bc, vec2(r * 0.84, r * 0.56)), r * 0.28, Color32::from_rgb(255, 214, 120));
                    p.text(bc, Align2::CENTER_CENTER, label, theme::numbers(r * 0.42), Color32::from_rgb(40, 20, 60));
                }

            } else if let Some((it, _)) = f.item_active {
                icons::item(p, it, cc, r * 0.62, t);
            } else {
                p.circle_stroke(cc, r * 0.5, stroke(1.5, Color32::from_white_alpha(46)));
                small_label(p, cc, "ITEM", 9.5, Color32::from_white_alpha(70));
            }
            if let Some((_, left)) = f.item_active {
                icons::radial_timer(p, cc, r * 0.94, left, theme::ACCENT);
            }
        });
    }
    if let Some(c) = l.circle(Btn::Brake) {
        round_button(p, c, press(Btn::Brake), ctl.is_held(Btn::Brake), Color32::from_rgb(255, 120, 110), |cc, r| {
            icons::glyph(p, "stop", cc + vec2(0.0, -r * 0.16), r * 0.42, Color32::WHITE);
            small_label(p, cc + vec2(0.0, r * 0.52), "BRAKE", 8.0, theme::TEXT);
        });
    }
    if let Some(c) = l.circle(Btn::Go) {
        round_button(p, c, press(Btn::Go), ctl.is_held(Btn::Go), Color32::from_rgb(120, 230, 140), |cc, _r| {
            small_label(p, cc, "GO", 17.0, Color32::WHITE);
        });
    }
    for (b, name) in [(Btn::Left, "left"), (Btn::Right, "right")] {
        if let Some(c) = l.circle(b) {
            round_button(p, c, press(b), ctl.is_held(b), RIM, |cc, r| icons::glyph(p, name, cc, r * 0.5, Color32::WHITE));
        }
    }
    top_bar(p, f, ctl);
    // Lesson hint on the control.
    if let Some(h) = hint {
        match h {
            HintControl::Steer => {
                if ctl.pad_state().is_none() && f.steer_mode == SteerMode::Pad {
                    let c = l.pad_rest;
                    p.circle_stroke(c, 52.0 + 4.0 * pulse, stroke(3.0, theme::ACCENT.gamma_multiply(0.5 + 0.5 * pulse)));
                }
            }
            HintControl::Drift => hint_ring(Btn::Drift),
            HintControl::Jump => hint_ring(Btn::Jump),
            HintControl::Laser => hint_ring(Btn::Laser),
            HintControl::Bash => hint_ring(Btn::Bash),
            HintControl::Item => hint_ring(Btn::Item),
        }
    }

    // --- Drill: badge, strikes, stars, timer, pop-ups ---
    if let Some(d) = &f.drill {
        super::drill_hud::draw(p, s, d, f.mirror.is_some(), t);
    }

    // --- Challenge score block ---
    if let Some(ch) = &f.challenge {
        super::challenge_hud::draw(p, s, ch, f.mirror.is_some(), t);
    }

    // --- Time Trial ghost: split pill and the tag over its head ---
    if let Some(g) = &f.ghost {
        super::ghost_hud::draw(p, s, g, f.mirror.is_some(), t);
    }

    // --- Battle: feathers, ranking, clock ---
    if let Some(b) = &f.battle {
        super::battle_hud::draw(p, s, b, f.mirror.is_some(), f.challenge.is_none(), t);
    }

    // --- Mirror frame (the panel itself is drawn by the renderer) ---
    if let Some((rect, scale)) = f.mirror {
        if scale > 0.9 {
            p.rect_stroke(rect.expand(2.0), 12.0, stroke(3.0, theme::ACCENT.gamma_multiply(0.9)), egui::StrokeKind::Outside);
            shadow_text(p, rect.center_top() + vec2(0.0, 11.0), Align2::CENTER_CENTER, "REAR", theme::ui_bold(9.0), Color32::from_white_alpha(170));
        }
    }

    // --- Lesson panel ---
    if let Some(ls) = &f.lesson {
        lesson_panel(p, Rect::from_min_size(pos2(s.left() + 8.0, s.top() + 104.0), vec2(250.0, 100.0)), ls, t);
    }

    // --- Centre messages ---
    let mid = pos2(s.center().x, s.top() + s.height() * 0.36);
    if let Some(cd) = f.countdown {
        countdown(p, mid, cd, t);
    } else if let Some(gt) = f.go_t {
        let a = (1.0 - gt / 0.9).clamp(0.0, 1.0);
        let k = ease_out_elastic((gt / 0.3).clamp(0.0, 1.0));
        shadow_text(p, mid, Align2::CENTER_CENTER, "GO!", theme::title(80.0 * (0.6 + 0.4 * k)), Color32::from_rgb(120, 255, 150).gamma_multiply(a));
    }
    if f.wrong_way {
        let a = 0.55 + 0.45 * (t * 9.0).sin().abs();
        let c = pos2(s.center().x, s.top() + s.height() * 0.5);
        shadow_text(p, c, Align2::CENTER_CENTER, "WRONG WAY", theme::title(34.0), Color32::from_rgb(255, 70, 60).gamma_multiply(a));
        shadow_text(p, c + vec2(0.0, 30.0), Align2::CENTER_CENTER, "TURN AROUND", theme::numbers(13.0), theme::TEXT.gamma_multiply(a));
    }
    if f.unstick_hint {
        let c = pos2(s.center().x, s.top() + s.height() * 0.55);
        shadow_text(p, c, Align2::CENTER_CENTER, "STUCK? STEER AWAY", theme::numbers(16.0), Color32::from_rgb(255, 214, 90).gamma_multiply(0.7 + 0.3 * (t * 8.0).sin().abs()));
        shadow_text(p, c + vec2(0.0, 22.0), Align2::CENTER_CENTER, "Hold left or right to turn back onto the track", theme::ui_bold(11.5), theme::TEXT);
    }
    if f.final_lap_t > 0.0 {
        let tt = 2.2 - f.final_lap_t;
        let x = s.center().x + (1.0 - ease_out_elastic((tt / 0.5).clamp(0.0, 1.0))) * 240.0;
        let a = (f.final_lap_t / 0.4).clamp(0.0, 1.0);
        shadow_text(p, pos2(x, s.top() + s.height() * 0.3), Align2::CENTER_CENTER, "FINAL LAP", theme::title(40.0), Color32::from_rgb(255, 200, 60).gamma_multiply(a));
    }
    if let Some(b) = &f.banner {
        // The GO! text owns 0.36 for its first moment: the banner (PERFECT START) drops below it.
        let by = if f.go_t.is_some() { 0.56 } else { 0.34 };
        draw_banner(p, pos2(s.center().x, s.top() + s.height() * by), b);
    }
    if f.fade > 0.0 {
        p.rect_filled(Rect::EVERYTHING, 0.0, Color32::from_black_alpha((f.fade.clamp(0.0, 1.0) * 255.0) as u8));
    }
}

fn top_bar(p: &Painter, f: &Frame, ctl: &Controller) {
    for (b, name) in [(Btn::Pause, "pause"), (Btn::Camera, "camera"), (Btn::LookBack, "back")] {
        if f.spectating && b == Btn::LookBack { continue; }
        if let Some(c) = f.layout.circle(b) {
            round_button(p, c, ctl.pressed_amount(b), ctl.is_held(b), RIM, |cc, r| icons::glyph(p, name, cc, r * 0.52, Color32::WHITE));
        }
    }
    if f.camera_flash > 0.0 {
        if let Some(c) = f.layout.circle(Btn::Camera) {
            shadow_text(p, pos2(c.c.x, c.c.y + 32.0), Align2::CENTER_CENTER, f.camera_name, theme::numbers(11.0), theme::ACCENT.gamma_multiply(f.camera_flash.min(1.0)));
        }
    }
}

fn countdown(p: &Painter, mid: Pos2, cd: Countdown, t: f32) {
    let n = cd.left.ceil().max(1.0);
    let frac = n - cd.left; // 0 at the start of each count, 1 at the end
    let k = ease_out_elastic((frac / 0.35).clamp(0.0, 1.0));
    let a = (1.0 - ((frac - 0.75) / 0.25).clamp(0.0, 1.0)).max(0.0);
    let col = if n <= 1.0 {
        Color32::from_rgb(255, 214, 80)
    } else {
        theme::TEXT
    };
    shadow_text(
        p,
        mid,
        Align2::CENTER_CENTER,
        &format!("{}", n as u32),
        theme::title(86.0 * (0.55 + 0.45 * k)),
        col.gamma_multiply(a),
    );
    if cd.run_start {
        shadow_text(
            p,
            mid + vec2(0.0, 68.0),
            Align2::CENTER_CENTER,
            "RUN AT GO · JUMP TO MOUNT",
            theme::numbers(13.0),
            theme::ACCENT,
        );
    }
    // (A drill has no start-boost window, so no cursor bar either.)
    if cd.left <= 1.25 && cd.window > 0.0 {
        // The start-boost window: a cursor runs left to right, tap when it is in the green.
        let w = 190.0;
        let bar = Rect::from_center_size(mid + vec2(0.0, 62.0), vec2(w, 12.0));
        p.rect_filled(bar.expand(2.5), 8.0, Color32::from_black_alpha(150));
        let zone = (cd.window / 1.25) * w;
        let zr = Rect::from_min_max(pos2(bar.right() - zone, bar.top()), bar.right_bottom());
        let ok = Color32::from_rgb(110, 240, 140);
        p.rect_filled(zr, 6.0, if cd.revved { ok } else { ok.gamma_multiply(0.55 + 0.25 * (t * 14.0).sin()) });
        let x = bar.left() + w * (1.0 - (cd.left / 1.25)).clamp(0.0, 1.0);
        p.rect_filled(Rect::from_center_size(pos2(x, bar.center().y), vec2(5.0, 22.0)), 2.0, Color32::WHITE);
        let msg = if cd.revved { "REVVED!" } else { "TAP AT GO" };
        shadow_text(p, bar.center() + vec2(0.0, 24.0), Align2::CENTER_CENTER, msg, theme::numbers(13.0), if cd.revved { ok } else { theme::TEXT });
    }
}

fn lesson_panel(p: &Painter, rect: Rect, ls: &LessonHud, t: f32) {
    let wrap = rect.width() - 20.0;
    let g = p.layout(ls.instruction.to_string(), theme::ui_medium(12.5), theme::TEXT, wrap);
    let hg = p.layout(ls.hint.to_string(), theme::ui_bold(11.0), theme::ACCENT, wrap);
    let h = 10.0 + 18.0 + g.size().y + 3.0 + hg.size().y + 6.0 + 14.0 + 10.0;
    let rect = Rect::from_min_size(rect.min, vec2(rect.width(), h));
    p.rect_filled(rect, 12.0, Color32::from_rgba_unmultiplied(14, 8, 28, 185));
    p.rect_stroke(rect, 12.0, stroke(1.0, theme::ACCENT.gamma_multiply(0.6)), egui::StrokeKind::Inside);
    let x = rect.left() + 10.0;
    let mut y = rect.top() + 9.0;
    p.text(pos2(x, y), Align2::LEFT_TOP, format!("LESSON {}  {}", ls.number, ls.title.to_uppercase()), theme::numbers(11.5), theme::ACCENT);
    y += 18.0;
    p.galley(pos2(x, y), g.clone(), theme::TEXT);
    y += g.size().y + 3.0;
    p.galley(pos2(x, y), hg.clone(), theme::ACCENT);
    y += hg.size().y + 7.0;
    // Progress bar, counter, and the timer on one row.
    let bar = Rect::from_min_size(pos2(x, y), vec2(wrap - 104.0, 9.0));
    p.rect_filled(bar, 5.0, Color32::from_black_alpha(140));
    let f = (ls.progress as f32 / ls.goal.max(1) as f32).clamp(0.0, 1.0);
    p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f, bar.height())), 5.0, Color32::from_rgb(120, 230, 140));
    p.text(pos2(bar.right() + 7.0, bar.center().y), Align2::LEFT_CENTER, format!("{}/{}", ls.progress, ls.goal), theme::numbers(12.0), theme::TEXT);
    let tt = match ls.limit {
        Some(l) => format!("{:.0}s", (l - ls.time).max(0.0)),
        None => fmt_time(ls.time),
    };
    let urgent = ls.limit.is_some_and(|l| l - ls.time < 8.0);
    let col = if urgent { Color32::from_rgb(255, 90, 80).gamma_multiply(0.7 + 0.3 * (t * 8.0).sin().abs()) } else { Color32::from_rgb(255, 232, 170) };
    p.text(pos2(rect.right() - 10.0, bar.center().y), Align2::RIGHT_CENTER, tt, theme::numbers(12.0), col);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinals_and_times() {
        assert_eq!(ordinal(1), ("1".into(), "st"));
        assert_eq!(ordinal(2).1, "nd");
        assert_eq!(ordinal(3).1, "rd");
        assert_eq!(ordinal(4).1, "th");
        assert_eq!(ordinal(11).1, "th");
        assert_eq!(ordinal(12).1, "th");
        assert_eq!(fmt_time(83.456), "1:23.46");
        assert_eq!(fmt_time(5.0), "0:05.00");
    }

    #[test]
    fn minimap_fits_in_its_rect() {
        let t = race_core::TrackId::Dune.track();
        let m = MiniMap::new(&t);
        let r = Rect::from_min_size(pos2(10.0, 10.0), vec2(108.0, 86.0)).shrink(9.0);
        for s in t.samples.iter().step_by(25) {
            let q = m.world_to_rect(s.pos, r);
            assert!(r.expand(0.5).contains(q), "{q:?} outside {r:?}");
        }
    }
}
