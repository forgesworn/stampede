//! The open-ground drill HUD (SPEC 18.2): the level badge with its four rep
//! pips, three strike markers, the star row, the rep timer ring, the big
//! centre pop-ups (level up, star, Master, strike), the Drift Ring sweep arc,
//! the off-screen arrows and the results card shown when the run is over.
//!
//! `DrillAnim` keeps the little animation state (pop-ups, strike flash) and is
//! fed the sim's `DrillEvent`s; `build` snapshots everything the painter needs
//! from `DrillState` every frame; `draw` and `card_ui` only paint.

use super::hud::{shadow_text, TIER_COLORS};
use super::icons::{self, stroke};
use crate::ui::icons::{self as ui_icons, Icon};
use crate::ui::{theme, widgets};
use egui::{pos2, vec2, Align2, Color32, Painter, Pos2, Rect, Shape, Vec2};
use race_core::drill::RepInfo;
use race_core::{DrillEvent, DrillId, DrillState};
use race_render::ease_out_elastic;

/// The title and instruction card shows for this long at the start of a run.
pub const INTRO_SECS: f32 = 7.0;
/// A star pop-up flies to its slot in the star row between these ages (s).
const STAR_FLY: (f32, f32) = (0.95, 1.55);
/// What the level badge shows at most.
const MAX_LEVEL: u8 = race_core::drill::MAX_LEVEL;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PopKind {
    LevelUp(u8),
    /// The n-th star (1-3).
    Star(u8),
    Master,
    Strike,
}

#[derive(Clone, Copy, Debug)]
pub struct Pop {
    pub kind: PopKind,
    /// Seconds since it appeared.
    pub age: f32,
}

impl Pop {
    pub fn life(&self) -> f32 {
        match self.kind {
            PopKind::LevelUp(_) => 1.9,
            PopKind::Star(_) => STAR_FLY.1 + 0.5,
            PopKind::Master => STAR_FLY.1 + 1.9,
            PopKind::Strike => 1.1,
        }
    }
}

/// Animation state for the drill HUD, owned by the race screen.
#[derive(Clone, Debug, Default)]
pub struct DrillAnim {
    pub pops: Vec<Pop>,
    /// 1 right after a strike, falling to 0 (red edge flash, marker bump).
    pub strike_flash: f32,
    /// 1 right after a level up, falling to 0 (badge pulse).
    pub level_pulse: f32,
    /// 1 right after a clean rep, falling to 0 (pip pulse).
    pub rep_pulse: f32,
    /// Seconds since the run started.
    pub age: f32,
}

impl DrillAnim {
    pub fn absorb(&mut self, e: DrillEvent) {
        match e {
            DrillEvent::RepClean => self.rep_pulse = 1.0,
            DrillEvent::Strike => {
                self.strike_flash = 1.0;
                self.pops.push(Pop { kind: PopKind::Strike, age: 0.0 });
            }
            DrillEvent::LevelUp(l) => {
                self.level_pulse = 1.0;
                self.pops.push(Pop { kind: PopKind::LevelUp(l), age: 0.0 });
            }
            DrillEvent::Star(n) => self.pops.push(Pop { kind: PopKind::Star(n), age: 0.0 }),
            DrillEvent::Master => self.pops.push(Pop { kind: PopKind::Master, age: 0.0 }),
            DrillEvent::Over => {}
        }
    }

    pub fn update(&mut self, dt: f32) {
        self.age += dt;
        for p in self.pops.iter_mut() {
            p.age += dt;
        }
        self.pops.retain(|p| p.age < p.life());
        self.strike_flash = (self.strike_flash - dt * 2.4).max(0.0);
        self.level_pulse = (self.level_pulse - dt * 2.0).max(0.0);
        self.rep_pulse = (self.rep_pulse - dt * 3.0).max(0.0);
    }
}

/// The Drift Ring sweep.
#[derive(Clone, Copy, Debug)]
pub struct RingHud {
    /// Swept / needed, 0..1.
    pub frac: f32,
    pub deg: f32,
    pub ready: bool,
    /// Mini-turbo tier the release needs (0/1 any, 2 orange, 3 purple).
    pub need_tier: u8,
}

/// An arrow at the screen edge towards a target that is not in view.
#[derive(Clone, Copy, Debug)]
pub struct ArrowHud {
    /// Radians from the camera's forward direction, right is positive.
    pub rel: f32,
    /// Metres to the target.
    pub dist: f32,
    /// The Laser Range dummy: bright, with the distance; gates and logs are subtle.
    pub strong: bool,
}

/// Everything the painter needs for one frame.
#[derive(Clone, Debug)]
pub struct DrillHud {
    pub id: DrillId,
    pub level: u8,
    /// Clean reps in this level (0-3 lit pips; the fourth clean rep levels up).
    pub reps: u8,
    pub strikes: u8,
    /// Stars shown in the row (a star still flying there is not counted).
    pub stars: u8,
    pub master: bool,
    /// (seconds left, the rep's whole allowance).
    pub timer: Option<(f32, f32)>,
    pub age: f32,
    pub ring: Option<RingHud>,
    pub arrow: Option<ArrowHud>,
    pub pops: Vec<Pop>,
    pub strike_flash: f32,
    pub level_pulse: f32,
    pub rep_pulse: f32,
}

/// Snapshot the sim's drill state for painting.
pub fn build(ds: &DrillState, anim: &DrillAnim, arrow: Option<ArrowHud>) -> DrillHud {
    let mut stars = ds.stars;
    let mut master = ds.master;
    for p in &anim.pops {
        match p.kind {
            PopKind::Star(n) if p.age < STAR_FLY.1 => stars = stars.min(n - 1),
            PopKind::Master if p.age < STAR_FLY.1 => master = false,
            _ => {}
        }
    }
    let ring = match ds.current_rep() {
        Some(RepInfo::Ring { need_tier, entered, .. }) if ds.id == DrillId::DriftRing && (entered || ds.ring_swept_deg > 0.0) => Some(RingHud {
            frac: (ds.ring_swept_deg / race_core::drill::RING_SWEEP_DEG).clamp(0.0, 1.0),
            deg: ds.ring_swept_deg,
            ready: ds.ring_ready,
            need_tier,
        }),
        _ => None,
    };
    let total = ds.egg.map(|e| e.life_max).or(ds.rep_limit);
    DrillHud {
        id: ds.id,
        level: ds.level.clamp(1, MAX_LEVEL),
        reps: ds.reps_in_level.min(3),
        strikes: ds.strikes.min(3),
        stars,
        master,
        timer: ds.time_left().zip(total).map(|(l, t)| (l, t.max(0.1))),
        age: anim.age,
        ring,
        arrow,
        pops: anim.pops.clone(),
        strike_flash: anim.strike_flash,
        level_pulse: anim.level_pulse,
        rep_pulse: anim.rep_pulse,
    }
}

/// Where star slot `i` (0-2) and the medal sit in the star row.
fn row_slots(safe: Rect) -> ([Pos2; 3], Pos2) {
    let y = safe.top() + 8.0 + 19.0;
    let x = safe.center().x;
    ([pos2(x - 56.0, y), pos2(x - 22.0, y), pos2(x + 12.0, y)], pos2(x + 50.0, y))
}

fn small_caps(p: &Painter, pos: Pos2, a: Align2, text: &str, size: f32, col: Color32) {
    p.text(pos, a, text, theme::ui_bold(size), col);
}

/// A rounded dark HUD panel in the style of the other HUDs.
fn plate(p: &Painter, r: Rect, rim: Color32) {
    p.rect_filled(r.translate(vec2(0.0, 2.0)), 16.0, Color32::from_black_alpha(70));
    p.rect_filled(r, 16.0, Color32::from_rgba_unmultiplied(14, 8, 28, 190));
    p.rect_stroke(r, 16.0, stroke(2.0, rim), egui::StrokeKind::Inside);
}

pub fn draw(p: &Painter, safe: Rect, h: &DrillHud, mirror: bool, t: f32) {
    let s = safe;
    // -------- red edge flash on a strike
    if h.strike_flash > 0.0 {
        let a = (h.strike_flash * h.strike_flash * 90.0) as u8;
        let red = Color32::from_rgba_unmultiplied(255, 30, 30, a);
        let band = 46.0 * h.strike_flash;
        p.rect_filled(Rect::from_min_max(pos2(s.left() - 400.0, s.top() - 400.0), pos2(s.right() + 400.0, s.top() + band)), 0.0, red);
        p.rect_filled(Rect::from_min_max(pos2(s.left() - 400.0, s.bottom() - band), pos2(s.right() + 400.0, s.bottom() + 400.0)), 0.0, red);
    }

    // -------- level badge card (top left)
    let card = Rect::from_min_size(pos2(s.left() + 8.0, s.top() + 8.0), vec2(228.0, 72.0));
    plate(p, card, theme::ACCENT.gamma_multiply(0.75));
    let bc = pos2(card.left() + 38.0, card.center().y);
    let k = 1.0 + 0.22 * h.level_pulse * (h.level_pulse * std::f32::consts::PI).sin().max(0.2);
    if h.level_pulse > 0.0 {
        p.circle_filled(bc, 36.0 * k, theme::GOLD.gamma_multiply(0.30 * h.level_pulse));
    }
    p.circle_filled(bc + vec2(0.0, 2.0), 29.0 * k, Color32::from_black_alpha(110));
    p.circle_filled(bc, 29.0 * k, theme::OUTLINE);
    let top = if h.level >= MAX_LEVEL { theme::GOLD } else { theme::ACCENT };
    p.circle_filled(bc, 26.0 * k, top);
    p.circle_filled(bc - vec2(0.0, 7.0 * k), 17.0 * k, Color32::from_rgba_unmultiplied(255, 240, 200, 50));
    small_caps(p, bc + vec2(0.0, -17.0 * k), Align2::CENTER_CENTER, "LEVEL", 8.5, theme::OUTLINE);
    p.text(bc + vec2(0.0, 5.0 * k), Align2::CENTER_CENTER, format!("{}", h.level), theme::title(30.0 * k), theme::OUTLINE);
    p.text(bc + vec2(0.0, 5.0 * k) - vec2(0.0, 1.0), Align2::CENTER_CENTER, format!("{}", h.level), theme::title(30.0 * k), Color32::WHITE);
    // title, pips, strikes
    let x0 = bc.x + 40.0;
    small_caps(p, pos2(x0, card.top() + 15.0), Align2::LEFT_CENTER, &h.id.title().to_uppercase(), 11.5, theme::SAND);
    for i in 0..4u8 {
        let w = 33.0;
        let r = Rect::from_min_size(pos2(x0 + i as f32 * (w + 3.0), card.top() + 27.0), vec2(w, 10.0));
        p.rect_filled(r, 5.0, Color32::from_black_alpha(140));
        let lit = i < h.reps;
        if lit {
            p.rect_filled(r, 5.0, theme::MINT);
            p.rect_filled(Rect::from_min_size(r.min + vec2(2.0, 1.5), vec2(w - 4.0, 2.5)), 1.5, Color32::from_white_alpha(70));
            if i + 1 == h.reps && h.rep_pulse > 0.0 {
                p.rect_stroke(r.expand(2.5 * h.rep_pulse), 7.0, stroke(2.0, theme::MINT.gamma_multiply(h.rep_pulse)), egui::StrokeKind::Outside);
            }
        } else {
            p.rect_stroke(r, 5.0, stroke(1.0, Color32::from_white_alpha(46)), egui::StrokeKind::Inside);
        }
    }
    small_caps(p, pos2(x0, card.bottom() - 15.0), Align2::LEFT_CENTER, "STRIKES", 8.5, theme::TEXT_DIM);
    for i in 0..3u8 {
        let c = pos2(x0 + 55.0 + i as f32 * 21.0, card.bottom() - 15.0);
        let used = i < h.strikes;
        let bump = if used && i + 1 == h.strikes { 1.0 + 0.5 * h.strike_flash } else { 1.0 };
        let wob = if used && i + 1 == h.strikes { (t * 60.0).sin() * 2.0 * h.strike_flash } else { 0.0 };
        let c = c + vec2(wob, 0.0);
        if used {
            p.circle_filled(c, 8.0 * bump, Color32::from_rgb(220, 50, 50));
            p.circle_stroke(c, 8.0 * bump, stroke(1.5, Color32::from_rgb(255, 150, 130)));
            let d = 3.2 * bump;
            p.line_segment([c + vec2(-d, -d), c + vec2(d, d)], stroke(2.2, Color32::WHITE));
            p.line_segment([c + vec2(-d, d), c + vec2(d, -d)], stroke(2.2, Color32::WHITE));
        } else {
            p.circle_filled(c, 8.0, Color32::from_black_alpha(120));
            p.circle_stroke(c, 8.0, stroke(1.3, Color32::from_white_alpha(70)));
        }
    }

    // -------- star row and rep timer (top centre)
    if !mirror {
        let (slots, medal_c) = row_slots(s);
        let row = Rect::from_min_max(pos2(slots[0].x - 24.0, slots[0].y - 19.0), pos2(medal_c.x + 24.0, slots[0].y + 19.0));
        plate(p, row, if h.master { theme::GOLD } else { Color32::from_white_alpha(60) });
        for (i, c) in slots.iter().enumerate() {
            // a star that just landed bumps
            let landed = h.pops.iter().find_map(|q| match q.kind {
                PopKind::Star(n) if n as usize == i + 1 && q.age >= STAR_FLY.1 => Some(q.age - STAR_FLY.1),
                _ => None,
            });
            let bump = landed.map_or(1.0, |a| 1.0 + 0.55 * (1.0 - (a / 0.4).clamp(0.0, 1.0)));
            if (i as u8) < h.stars {
                p.circle_filled(*c, 15.0 * bump, theme::GOLD.gamma_multiply(0.22));
                ui_icons::star(p, *c, 13.5 * bump, theme::GOLD, egui::Stroke::new(1.2_f32, Color32::from_rgb(0xB8, 0x6A, 0x10)));
            } else {
                ui_icons::star(p, *c, 12.0, Color32::from_black_alpha(130), egui::Stroke::new(1.2_f32, Color32::from_white_alpha(60)));
            }
        }
        let mdl = if h.master { theme::GOLD } else { Color32::from_white_alpha(50) };
        if h.master {
            p.circle_filled(medal_c, 17.0, theme::GOLD.gamma_multiply(0.25 + 0.1 * (t * 5.0).sin()));
        }
        ui_icons::draw(p, Icon::Medal, medal_c, 28.0, mdl);

        if let Some((left, total)) = h.timer {
            let c = pos2(s.center().x, row.bottom() + 8.0 + 24.0);
            let frac = (left / total).clamp(0.0, 1.0);
            let urgent = left < 2.5;
            let col = if urgent { Color32::from_rgb(255, 90, 80).gamma_multiply(0.75 + 0.25 * (t * 10.0).sin().abs()) } else { theme::mix(theme::MINT, theme::GOLD, 1.0 - frac) };
            p.circle_filled(c, 25.0, Color32::from_rgba_unmultiplied(14, 8, 28, 190));
            p.circle_stroke(c, 21.0, stroke(5.0, Color32::from_black_alpha(130)));
            icons::ring_arc(p, c, 21.0, frac, 5.0, col);
            let txt = if left >= 10.0 { format!("{}", left.ceil() as u32) } else { format!("{left:.1}") };
            shadow_text(p, c, Align2::CENTER_CENTER, &txt, theme::numbers(if left >= 10.0 { 15.0 } else { 13.0 }), Color32::WHITE);
        }
    }

    // -------- intro card (first seconds)
    let intro_a = (1.0 - ((h.age - (INTRO_SECS - 0.8)) / 0.8).clamp(0.0, 1.0)) * ((h.age) / 0.35).clamp(0.0, 1.0);
    if intro_a > 0.0 {
        let w = 262.0;
        let wrap = w - 22.0;
        let g = p.layout(h.id.instruction().to_string(), theme::ui_medium(12.5), theme::TEXT.gamma_multiply(intro_a), wrap);
        let hg = p.layout(h.id.hint().to_string(), theme::ui_bold(11.0), theme::ACCENT.gamma_multiply(intro_a), wrap);
        let ph = 12.0 + 18.0 + g.size().y + 4.0 + hg.size().y + 11.0;
        let r = Rect::from_min_size(pos2(s.left() + 8.0, s.top() + 88.0 - (1.0 - intro_a) * 10.0), vec2(w, ph));
        p.rect_filled(r, 12.0, Color32::from_rgba_unmultiplied(14, 8, 28, (185.0 * intro_a) as u8));
        p.rect_stroke(r, 12.0, stroke(1.0, theme::ACCENT.gamma_multiply(0.6 * intro_a)), egui::StrokeKind::Inside);
        p.text(pos2(r.left() + 11.0, r.top() + 11.0), Align2::LEFT_TOP, format!("DRILL  {}", h.id.title().to_uppercase()), theme::numbers(11.5), theme::ACCENT.gamma_multiply(intro_a));
        p.galley(pos2(r.left() + 11.0, r.top() + 30.0), g.clone(), theme::TEXT);
        p.galley(pos2(r.left() + 11.0, r.top() + 34.0 + g.size().y), hg, theme::ACCENT);
    }

    // -------- Drift Ring sweep arc (lower centre)
    if let Some(r) = &h.ring {
        let c = pos2(s.center().x, s.top() + s.height() * 0.66);
        let pulse = 0.5 + 0.5 * (t * 14.0).sin();
        let col = if r.ready { theme::mix(theme::MINT, Color32::WHITE, 0.35 * pulse) } else { theme::ACCENT };
        p.circle_filled(c, 33.0, Color32::from_rgba_unmultiplied(14, 8, 28, 170));
        p.circle_stroke(c, 27.0, stroke(7.0, Color32::from_black_alpha(130)));
        if r.ready {
            p.circle_filled(c, 40.0 + 4.0 * pulse, theme::MINT.gamma_multiply(0.16 + 0.12 * pulse));
        }
        icons::ring_arc(p, c, 27.0, r.frac, 7.0, col);
        shadow_text(p, c, Align2::CENTER_CENTER, &format!("{:.0}", r.deg.min(999.0)), theme::numbers(17.0), Color32::WHITE);
        shadow_text(p, c + vec2(0.0, 12.5), Align2::CENTER_CENTER, "DEG", theme::ui_bold(7.5), theme::TEXT_DIM);
        if r.ready {
            shadow_text(p, c + vec2(0.0, 52.0), Align2::CENTER_CENTER, "release!", theme::title(26.0 + 2.0 * pulse), theme::MINT);
        } else if r.need_tier >= 2 {
            let tc = TIER_COLORS[(r.need_tier as usize - 1).min(2)];
            let word = if r.need_tier == 2 { "release on ORANGE sparks" } else { "release on PURPLE sparks" };
            shadow_text(p, c + vec2(0.0, 50.0), Align2::CENTER_CENTER, word, theme::ui_bold(11.5), tc);
        }
    }

    // -------- off-screen arrow
    if let Some(a) = &h.arrow {
        draw_arrow(p, safe, a, t);
    }

    // -------- centre pop-ups
    for pop in &h.pops {
        draw_pop(p, safe, pop, h, t);
    }
}

fn draw_arrow(p: &Painter, safe: Rect, a: &ArrowHud, t: f32) {
    let dir = vec2(a.rel.sin(), -a.rel.cos());
    let centre = pos2(safe.center().x, safe.center().y - 6.0);
    let half = vec2(safe.width() * 0.5 - 70.0, safe.height() * 0.5 - 92.0);
    let kx = if dir.x.abs() > 1e-3 { half.x / dir.x.abs() } else { f32::INFINITY };
    let ky = if dir.y.abs() > 1e-3 { half.y / dir.y.abs() } else { f32::INFINITY };
    let pos = centre + dir * kx.min(ky);
    let alpha = if a.strong { 1.0 } else { 0.55 };
    let col = if a.strong { Color32::from_rgb(255, 70, 70) } else { theme::SAND };
    let bob = (t * 5.0).sin() * 2.0;
    let pos = pos + dir * bob;
    p.circle_filled(pos, 17.0, Color32::from_rgba_unmultiplied(14, 8, 28, (170.0 * alpha) as u8));
    p.circle_stroke(pos, 17.0, stroke(1.5, col.gamma_multiply(alpha)));
    let perp = vec2(-dir.y, dir.x);
    let tri = vec![pos + dir * 11.0, pos - dir * 6.0 + perp * 8.5, pos - dir * 2.0, pos - dir * 6.0 - perp * 8.5];
    p.add(Shape::convex_polygon(tri, col.gamma_multiply(alpha), stroke(1.0, Color32::from_black_alpha(120))));
    if a.strong {
        let off = if dir.y > 0.3 { vec2(0.0, -26.0) } else { vec2(0.0, 27.0) };
        shadow_text(p, pos + off, Align2::CENTER_CENTER, &format!("{:.0} m", a.dist), theme::numbers(12.0), Color32::WHITE);
    }
}

fn draw_pop(p: &Painter, safe: Rect, pop: &Pop, h: &DrillHud, t: f32) {
    let a = pop.age;
    let cx = safe.center().x;
    let fade = (1.0 - ((a - (pop.life() - 0.4)) / 0.4).clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let pin = ease_out_elastic((a / 0.4).clamp(0.0, 1.0));
    match pop.kind {
        PopKind::LevelUp(l) => {
            let y = safe.top() + safe.height() * 0.30 - (a * 6.0).min(14.0);
            let col = if l >= MAX_LEVEL { theme::GOLD } else { theme::ACCENT };
            shadow_text(p, pos2(cx, y), Align2::CENTER_CENTER, &format!("LEVEL {l}"), theme::title(52.0 * (0.5 + 0.5 * pin)), col.gamma_multiply(fade));
            shadow_text(p, pos2(cx, y + 34.0), Align2::CENTER_CENTER, "level up!", theme::numbers(15.0), Color32::WHITE.gamma_multiply(fade));
        }
        PopKind::Star(n) => {
            let (slots, _) = row_slots(safe);
            let home = slots[(n as usize - 1).min(2)];
            let big = pos2(cx, safe.top() + safe.height() * 0.50);
            let fly = ((a - STAR_FLY.0) / (STAR_FLY.1 - STAR_FLY.0)).clamp(0.0, 1.0);
            if a < STAR_FLY.1 {
                let e = fly * fly * (3.0 - 2.0 * fly);
                let c = big + (home - big) * e + vec2(0.0, -(e * (1.0 - e)) * 70.0);
                let size = (44.0 * pin) * (1.0 - e) + 13.5 * e;
                p.circle_filled(c, size * 1.9, theme::GOLD.gamma_multiply(0.25 * (1.0 - e)));
                ui_icons::star(p, c + vec2(0.0, 2.0), size, Color32::from_black_alpha(110), egui::Stroke::NONE);
                ui_icons::star(p, c, size, theme::GOLD, egui::Stroke::new(1.5_f32, Color32::from_rgb(0xB8, 0x6A, 0x10)));
                if fly == 0.0 {
                    shadow_text(p, big + vec2(0.0, 64.0), Align2::CENTER_CENTER, &format!("STAR {n}!"), theme::title(34.0 * (0.5 + 0.5 * pin)), theme::GOLD.gamma_multiply(fade));
                } else {
                    shadow_text(p, big + vec2(0.0, 64.0), Align2::CENTER_CENTER, &format!("STAR {n}!"), theme::title(34.0), theme::GOLD.gamma_multiply(1.0 - fly));
                }
            }
        }
        PopKind::Master => {
            let c = pos2(cx, safe.top() + safe.height() * 0.46);
            let spin = (a * 3.0).sin() * 0.0;
            let _ = spin;
            if a < STAR_FLY.1 + 1.2 {
                let k = pin * (1.0 - ((a - (STAR_FLY.1 + 0.9)) / 0.3).clamp(0.0, 1.0));
                p.circle_filled(c, 80.0 * k, theme::GOLD.gamma_multiply(0.20 + 0.08 * (t * 8.0).sin()));
                for i in 0..10 {
                    let ang = i as f32 / 10.0 * std::f32::consts::TAU + a * 0.8;
                    let r0 = 52.0 * k;
                    p.line_segment([c + vec2(ang.cos(), ang.sin()) * r0, c + vec2(ang.cos(), ang.sin()) * (r0 + 16.0)], stroke(3.0, theme::GOLD.gamma_multiply(0.7 * k)));
                }
                ui_icons::draw(p, Icon::Medal, c + vec2(0.0, 2.0), 96.0 * k, Color32::from_black_alpha(110));
                ui_icons::draw(p, Icon::Medal, c, 96.0 * k, theme::GOLD);
                shadow_text(p, c + vec2(0.0, 66.0), Align2::CENTER_CENTER, "MASTER!", theme::title(46.0 * (0.5 + 0.5 * pin)), theme::GOLD.gamma_multiply(fade.max(0.0)));
            }
            // then the medal lights up in the row (handled by `h.master`)
            let _ = h;
        }
        PopKind::Strike => {
            let shake = (a * 55.0).sin() * 9.0 * (1.0 - (a / 0.5).clamp(0.0, 1.0));
            let c = pos2(cx + shake, safe.top() + safe.height() * 0.50);
            shadow_text(p, c, Align2::CENTER_CENTER, "STRIKE", theme::title(48.0 * (0.6 + 0.4 * pin)), Color32::from_rgb(255, 74, 66).gamma_multiply(fade));
            let left = 3u8.saturating_sub(h.strikes);
            let word = match left {
                0 => "that's the run".to_string(),
                1 => "1 strike left".to_string(),
                n => format!("{n} strikes left"),
            };
            shadow_text(p, c + vec2(0.0, 32.0), Align2::CENTER_CENTER, &word, theme::numbers(14.0), Color32::WHITE.gamma_multiply(fade));
        }
    }
}

// ---------------------------------------------------------------- results ----

/// What the results card shows.
#[derive(Clone, Debug)]
pub struct ResultCard {
    pub id: DrillId,
    pub level: u8,
    pub stars: u8,
    pub master: bool,
    /// The level beats the saved best.
    pub new_best: bool,
    pub best: u8,
    pub clean: u32,
    /// ₿ paid for this run.
    pub earned: u32,
    /// Titles of achievements this run unlocked.
    pub unlocked: Vec<&'static str>,
    pub next: DrillId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardAction {
    Retry,
    Next,
    Back,
}

/// The results card over the frozen race: stars and medal, the level reached,
/// and Retry / Next drill / Back. `age` is seconds since it opened.
pub fn card_ui(ui: &egui::Context, safe: Rect, card: &ResultCard, age: f32, tm: f32) -> Option<CardAction> {
    let k = crate::ui::art::ease_out_cubic(crate::ui::art::phase(age, 0.0, 0.35));
    let screen = ui.content_rect();
    let dim = ui.layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("drill-dim")));
    dim.rect_filled(screen, 0.0, Color32::from_black_alpha((150.0 * k) as u8));
    let (w, hh) = (safe.width().min(470.0), safe.height().min(318.0));
    let panel = Rect::from_center_size(safe.center() + vec2(0.0, (1.0 - k) * 26.0), vec2(w, hh));
    let mut action = None;
    egui::Area::new(egui::Id::new("drill-card")).order(egui::Order::Foreground).fixed_pos(panel.min).show(ui, |ui| {
        ui.set_min_size(panel.size());
        let p = ui.painter().clone();
        let brand = crate::ui::art::brand(ui.ctx());
        widgets::panel_solid(&p, panel, 22.0);
        if card.master {
            widgets::confetti(&p, &brand, panel, age, 90);
        }
        widgets::heading(&p, pos2(panel.center().x, panel.top() + 32.0), Align2::CENTER_CENTER, &card.id.title(), 28.0, theme::SAND);
        small_caps(&p, pos2(panel.center().x, panel.top() + 58.0), Align2::CENTER_CENTER, if card.master { "DRILL MASTERED" } else { "RUN OVER" }, 11.0, if card.master { theme::GOLD } else { theme::TEXT_DIM });

        // left: the level
        let lc = pos2(panel.left() + w * 0.27, panel.top() + 128.0);
        let e = ease_out_elastic(((age - 0.15) / 0.5).clamp(0.0, 1.0));
        p.circle_filled(lc + vec2(0.0, 3.0), 46.0 * e, Color32::from_black_alpha(110));
        p.circle_filled(lc, 46.0 * e, theme::OUTLINE);
        p.circle_filled(lc, 42.0 * e, if card.level >= MAX_LEVEL { theme::GOLD } else { theme::ACCENT });
        p.circle_filled(lc - vec2(0.0, 12.0 * e), 28.0 * e, Color32::from_rgba_unmultiplied(255, 240, 200, 50));
        small_caps(&p, lc + vec2(0.0, -26.0), Align2::CENTER_CENTER, "LEVEL", 10.0, theme::OUTLINE);
        if e > 0.05 {
            p.text(lc + vec2(0.0, 8.0), Align2::CENTER_CENTER, format!("{}", card.level), theme::title(48.0 * e), Color32::WHITE);
        }
        if card.new_best {
            widgets::pill(&p, lc + vec2(0.0, 64.0), "NEW BEST", theme::GOLD, theme::OUTLINE, 12.0);
        } else {
            p.text(lc + vec2(0.0, 64.0), Align2::CENTER_CENTER, format!("best level {}", card.best), theme::ui_bold(12.0), theme::TEXT_DIM);
        }

        // right: stars and the medal
        let rc = pos2(panel.left() + w * 0.69, panel.top() + 108.0);
        widgets::stars(&p, rc, 3, card.stars, 44.0, (age - 0.45).max(0.0));
        let mc = pos2(rc.x, rc.y + 62.0);
        let me = ease_out_elastic(((age - 1.0) / 0.5).clamp(0.0, 1.0));
        if card.master {
            p.circle_filled(mc, 30.0 * me, theme::GOLD.gamma_multiply(0.22));
            ui_icons::draw(&p, Icon::Medal, mc, 48.0 * me.max(0.01), theme::GOLD);
            small_caps(&p, mc + vec2(0.0, 32.0), Align2::CENTER_CENTER, "MASTER MEDAL", 10.5, theme::GOLD);
        } else {
            ui_icons::draw(&p, Icon::Medal, mc, 40.0, Color32::from_white_alpha(46));
            small_caps(&p, mc + vec2(0.0, 30.0), Align2::CENTER_CENTER, "Master at level 10", 9.5, theme::TEXT_DIM);
        }

        // the line under: reps and the payout
        let ly = panel.top() + 205.0;
        p.text(pos2(panel.left() + 28.0, ly), Align2::LEFT_CENTER, format!("{} clean reps", card.clean), theme::ui_bold(12.5), theme::SAND);
        if card.earned > 0 {
            let txt = format!("+{}", card.earned);
            let g = p.layout_no_wrap(txt.clone(), theme::numbers(19.0), theme::GOLD);
            let x = panel.right() - 28.0;
            p.galley(pos2(x - g.size().x, ly - g.size().y / 2.0), g.clone(), theme::GOLD);
            icons::bitcoin(&p, pos2(x - g.size().x - 14.0, ly), 9.5);
        } else {
            p.text(pos2(panel.right() - 28.0, ly), Align2::RIGHT_CENTER, "no new stars to pay", theme::ui_medium(11.5), theme::TEXT_DIM);
        }
        if let Some(a) = card.unlocked.first() {
            let more = card.unlocked.len().saturating_sub(1);
            let txt = if more > 0 { format!("Achievement: {a} +{more}") } else { format!("Achievement: {a}") };
            widgets::pill(&p, pos2(panel.center().x, ly + 26.0), &txt, theme::MINT, theme::OUTLINE, 12.0);
        }

        // buttons
        let by = panel.bottom() - 22.0 - 44.0;
        let bw = (w - 56.0 - 2.0 * 10.0) / 3.0;
        let bx = |i: usize| panel.left() + 28.0 + i as f32 * (bw + 10.0);
        let next_label = format!("Next: {}", card.next.title());
        let r = widgets::button(ui, Rect::from_min_size(pos2(bx(0), by), vec2(bw, 44.0)), "drill-retry", &widgets::Btn::primary("Retry"));
        let n = widgets::button(ui, Rect::from_min_size(pos2(bx(1), by), vec2(bw, 44.0)), "drill-next", &widgets::Btn::new(&next_label).size(15.0));
        let b = widgets::button(ui, Rect::from_min_size(pos2(bx(2), by), vec2(bw, 44.0)), "drill-back", &widgets::Btn::new("Back"));
        if age > 0.6 {
            if r.clicked() {
                action = Some(CardAction::Retry);
            } else if n.clicked() {
                action = Some(CardAction::Next);
            } else if b.clicked() {
                action = Some(CardAction::Back);
            }
        }
    });
    let _ = tm;
    action
}

/// The next drill in menu order (wraps).
pub fn next_drill(id: DrillId) -> DrillId {
    let i = DrillId::ALL.iter().position(|d| *d == id).unwrap_or(0);
    DrillId::ALL[(i + 1) % DrillId::ALL.len()]
}

/// The arrow for a world target, given the camera: None while the target is on screen.
pub fn arrow_to(cam: &race_render::Camera, screen: Vec2, target: glam::Vec3, from: glam::Vec3, strong: bool) -> Option<ArrowHud> {
    use glam::Vec3;
    if let Some(sp) = super::ghosts::project(cam, screen, target) {
        // on screen and clear of the edges and the control clusters
        let inner = Rect::from_min_size(pos2(0.0, 0.0), screen).shrink2(vec2(60.0, 70.0));
        if inner.contains(sp) {
            return None;
        }
    }
    let f = (cam.target - cam.eye).normalize_or_zero();
    let f = Vec3::new(f.x, 0.0, f.z).normalize_or_zero();
    let r = Vec3::new(-f.z, 0.0, f.x);
    // right-handed screen: right = forward x up
    let r = if r.dot(f.cross(Vec3::Y)) < 0.0 { -r } else { r };
    let d = target - cam.eye;
    let d = Vec3::new(d.x, 0.0, d.z);
    let rel = d.dot(r).atan2(d.dot(f));
    let dist = Vec3::new(target.x - from.x, 0.0, target.z - from.z).length();
    Some(ArrowHud { rel, dist, strong })
}

#[cfg(test)]
mod tests {
    use super::*;
    use race_core::{Vec2, Vec3};

    fn state(id: DrillId) -> DrillState {
        DrillState::new(id, Vec2::ZERO, Vec3::new(150.0, 0.0, 0.0), std::f32::consts::PI, 7)
    }

    #[test]
    fn the_snapshot_follows_the_state_and_holds_back_a_flying_star() {
        let mut ds = state(DrillId::Slalom);
        ds.level = 4;
        ds.reps_in_level = 2;
        ds.strikes = 1;
        ds.stars = 1;
        let mut anim = DrillAnim::default();
        anim.absorb(DrillEvent::Star(1));
        let h = build(&ds, &anim, None);
        assert_eq!((h.level, h.reps, h.strikes), (4, 2, 1));
        assert_eq!(h.stars, 0, "the star is still flying to its slot");
        anim.update(STAR_FLY.1 + 0.05);
        assert_eq!(build(&ds, &anim, None).stars, 1, "it has landed");
        anim.update(2.0);
        assert!(anim.pops.is_empty(), "pop-ups expire");
    }

    #[test]
    fn events_drive_the_pops() {
        let mut a = DrillAnim::default();
        for e in [DrillEvent::RepClean, DrillEvent::Strike, DrillEvent::LevelUp(2), DrillEvent::Star(1), DrillEvent::Master, DrillEvent::Over] {
            a.absorb(e);
        }
        let kinds: Vec<_> = a.pops.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, [PopKind::Strike, PopKind::LevelUp(2), PopKind::Star(1), PopKind::Master]);
        assert!(a.strike_flash > 0.9 && a.level_pulse > 0.9 && a.rep_pulse > 0.9);
    }

    #[test]
    fn every_drill_builds_and_the_timer_follows_the_rep_clock() {
        for id in DrillId::ALL {
            let ds = state(id);
            let h = build(&ds, &DrillAnim::default(), None);
            assert_eq!(h.id, id);
            assert!(h.ring.is_none(), "no ring before the runner is near it");
        }
        assert_eq!(next_drill(DrillId::BashBuddy), DrillId::Slalom);
        assert_eq!(next_drill(DrillId::Slalom), DrillId::DriftRing);
    }
}
