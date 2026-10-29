//! The Feather Frenzy HUD (SPEC 17.1): the local ostrich's feathers (big, one
//! flutters off when it is lost), the battle clock, a compact live ranking with
//! every racer's feathers, and the spectate note once the player is out. Read off
//! the sim's `BattleState` every frame, so it is the same online and offline.

use super::hud::shadow_text;
use super::icons::stroke;
use super::source::RaceView;
use crate::ui::theme;
use egui::{pos2, vec2, Align2, Color32, Painter, Pos2, Rect, Shape};
use race_core::{BattleState, Character};

/// One line of the live ranking.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub rank: u8,
    pub racer: usize,
    pub name: String,
    pub color: Color32,
    pub feathers: u8,
    pub out: bool,
    pub me: bool,
    pub invuln: bool,
}

/// What the battle HUD shows this frame.
#[derive(Clone, Debug, Default)]
pub struct BattleHud {
    /// The local racer's feathers.
    pub feathers: u8,
    /// Empty slots to show behind them (the rules' starting count).
    pub slots: u8,
    pub invuln: bool,
    pub out: bool,
    pub time_left: f32,
    pub rows: Vec<Row>,
    /// Feathers that flutter off (slot, seconds since the loss).
    pub falling: Vec<(u8, f32)>,
    /// Seconds since a feather was gained (a steal), for the pop.
    pub gain_age: f32,
    pub popups: Vec<Popup>,
    /// Following another ostrich after being knocked out (its name, time speed-up).
    pub spectating: Option<(String, f32)>,
    /// The challenge block (Last Feather) takes the top centre: the feathers go below it.
    pub below_challenge: bool,
}

/// Seconds a lost feather takes to flutter away.
pub const FALL_SECS: f32 = 1.0;
/// Seconds of the pop when a feather is stolen.
pub const GAIN_SECS: f32 = 0.6;

/// Lost-feather animation state, owned by the race screen.
#[derive(Default, Debug)]
pub struct BattleAnim {
    last: Option<u8>,
    pub falling: Vec<(u8, f32)>,
    pub gain_age: f32,
    pub popups: Vec<Popup>,
}

/// A line of text under the feathers ("-1  HIT BY NOVA"), rising and fading.
#[derive(Clone, Debug, PartialEq)]
pub struct Popup {
    pub text: String,
    pub color: Color32,
    pub age: f32,
}

/// Seconds a popup shows.
pub const POPUP_SECS: f32 = 1.8;

impl BattleAnim {
    pub fn new() -> BattleAnim {
        BattleAnim { last: None, falling: Vec::new(), gain_age: 9.0, popups: Vec::new() }
    }

    /// Show `text` under the feathers (at most three at once).
    pub fn popup(&mut self, text: impl Into<String>, color: Color32) {
        self.popups.push(Popup { text: text.into(), color, age: 0.0 });
        if self.popups.len() > 3 {
            self.popups.remove(0);
        }
    }

    /// Advance by `dt` with the local racer's feather count `now`.
    pub fn update(&mut self, dt: f32, now: u8) {
        if let Some(prev) = self.last {
            if now < prev {
                for slot in now..prev {
                    self.falling.push((slot, 0.0));
                }
            } else if now > prev {
                self.gain_age = 0.0;
            }
        }
        self.last = Some(now);
        for f in self.falling.iter_mut() {
            f.1 += dt;
        }
        self.falling.retain(|f| f.1 < FALL_SECS);
        for p in self.popups.iter_mut() {
            p.age += dt;
        }
        self.popups.retain(|p| p.age < POPUP_SECS);
        self.gain_age = (self.gain_age + dt).min(9.0);
    }
}

fn c32(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0].clamp(0.0, 1.0) * 255.0) as u8, (c[1].clamp(0.0, 1.0) * 255.0) as u8, (c[2].clamp(0.0, 1.0) * 255.0) as u8)
}

/// A racer's colour on the ranking (the accent, lifted when it is dark).
pub fn racer_color(character: u8) -> Color32 {
    let a = Character::get(character.min(7)).accent;
    let lift = a.iter().cloned().fold(0.0, f32::max);
    let a = if lift < 0.5 { [a[0] + 0.4, a[1] + 0.4, a[2] + 0.4] } else { a };
    c32(a)
}

/// The ranking rows, best first (`BattleState::ranking`).
pub fn rows(b: &BattleState, names: &[String], characters: &[u8], me: usize) -> Vec<Row> {
    b.ranking()
        .into_iter()
        .enumerate()
        .map(|(k, i)| Row {
            rank: k as u8 + 1,
            racer: i,
            name: names.get(i).cloned().unwrap_or_else(|| format!("Racer {}", i + 1)),
            color: racer_color(characters.get(i).copied().unwrap_or(0)),
            feathers: b.feathers.get(i).copied().unwrap_or(0),
            out: b.is_out(i),
            me: i == me,
            invuln: b.flashing(i),
        })
        .collect()
}

/// Build the panel's data from the view.
pub fn build(v: &RaceView, b: &BattleState, seat: usize, anim: &BattleAnim) -> BattleHud {
    let names: Vec<String> = v.cfg.racers.iter().map(|r| r.name.to_uppercase()).collect();
    let chars: Vec<u8> = v.racers.iter().map(|r| r.character).collect();
    let mine = b.feathers.get(seat).copied().unwrap_or(0);
    BattleHud {
        feathers: mine,
        slots: b.rules.feathers.max(mine),
        invuln: b.flashing(seat),
        out: b.is_out(seat),
        time_left: b.time_left,
        rows: rows(b, &names, &chars, seat),
        falling: anim.falling.clone(),
        gain_age: anim.gain_age,
        popups: anim.popups.clone(),
        spectating: None,
        below_challenge: v.challenge.is_some(),
    }
}

/// `2:41`, the clock counting down.
pub fn fmt_clock(s: f32) -> String {
    let s = s.max(0.0).ceil() as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// A feather icon centred at `c`, `s` tall-ish: cream blade, gold tip, spine. `tilt` radians, `alpha` 0..1, `lit` false = an empty slot.
pub fn feather(p: &Painter, c: Pos2, s: f32, tilt: f32, alpha: f32, lit: bool) {
    let rot = |v: egui::Vec2| {
        let (sn, cs) = tilt.sin_cos();
        vec2(v.x * cs - v.y * sn, v.x * sn + v.y * cs)
    };
    let a = |col: Color32| col.gamma_multiply(alpha);
    // blade: an ellipse leaning right
    let lean: f32 = 0.45;
    let blade: Vec<Pos2> = (0..24)
        .map(|i| {
            let t = i as f32 / 24.0 * std::f32::consts::TAU;
            let (x, y) = (t.cos() * s * 0.30, t.sin() * s * 0.5);
            let (sn, cs) = lean.sin_cos();
            c + rot(vec2(x * cs - y * sn, x * sn + y * cs))
        })
        .collect();
    let spine = |t: f32| {
        let (sn, cs) = lean.sin_cos();
        let (x, y) = (0.0, t * s * 0.62);
        c + rot(vec2(x * cs - y * sn, x * sn + y * cs))
    };
    if lit {
        p.add(Shape::convex_polygon(blade.clone(), a(Color32::from_rgb(255, 238, 196)), stroke(1.6, a(Color32::from_rgb(70, 36, 12)))));
        // gold tip
        let tip: Vec<Pos2> = blade.iter().map(|q| *q).enumerate().filter(|(i, _)| *i > 14 || *i < 3).map(|(_, q)| q).collect();
        if tip.len() > 2 {
            p.add(Shape::convex_polygon(tip, a(theme::ACCENT), egui::Stroke::NONE));
        }
        p.line_segment([spine(1.0), spine(-1.0)], stroke(1.4, a(Color32::from_rgb(160, 100, 40))));
        for k in -2..=2 {
            let y = k as f32 * 0.32;
            let from = spine(y);
            let to = from + rot(vec2(s * 0.2, -s * 0.11));
            p.line_segment([from, to], stroke(0.9, a(Color32::from_rgb(210, 160, 90))));
        }
    } else {
        p.add(Shape::closed_line(blade, stroke(1.4, Color32::from_white_alpha((70.0 * alpha) as u8))));
    }
}

fn pill(p: &Painter, r: Rect, rim: Color32) {
    p.rect_filled(r.translate(vec2(0.0, 2.0)), r.height() * 0.5, Color32::from_black_alpha(70));
    p.rect_filled(r, r.height() * 0.5, Color32::from_rgba_unmultiplied(14, 8, 28, 182));
    p.rect_stroke(r, r.height() * 0.5, stroke(2.0, rim), egui::StrokeKind::Inside);
}

/// Draw the feathers row, the ranking and the clock. `mirror`: the rear-view mirror owns the top centre.
pub fn draw(p: &Painter, safe: Rect, h: &BattleHud, mirror: bool, show_clock: bool, t: f32) {
    // --- my feathers, top centre ---
    let n = h.slots.max(1) as f32;
    let step = 33.0;
    let w = (n * step + 22.0).max(96.0);
    let top = if h.below_challenge { 112.0 } else if mirror { 102.0 } else { 8.0 };
    let panel = Rect::from_center_size(pos2(safe.center().x, safe.top() + top + 22.0), vec2(w, 44.0));
    let flash = h.invuln && (t * 10.0).sin() > 0.0;
    let rim = if h.out {
        Color32::from_rgb(255, 100, 90)
    } else if flash {
        Color32::from_rgb(140, 230, 255)
    } else if h.feathers <= 1 {
        Color32::from_rgb(255, 120, 90).gamma_multiply(0.7 + 0.3 * (t * 8.0).sin().abs())
    } else {
        theme::ACCENT.gamma_multiply(0.8)
    };
    pill(p, panel, rim);
    let x0 = panel.center().x - (n - 1.0) * step * 0.5;
    for slot in 0..h.slots {
        let c = pos2(x0 + slot as f32 * step, panel.center().y);
        let have = slot < h.feathers;
        let mut scale = 1.0;
        if have && slot + 1 == h.feathers && h.gain_age < GAIN_SECS {
            scale = 0.5 + 0.5 * race_render::ease_out_elastic((h.gain_age / GAIN_SECS).clamp(0.0, 1.0));
        }
        let sway = (t * 2.0 + slot as f32).sin() * 0.05;
        feather(p, c, 34.0 * scale, sway, 1.0, have);
    }
    for &(slot, age) in &h.falling {
        let k = (age / FALL_SECS).clamp(0.0, 1.0);
        let c = pos2(x0 + slot as f32 * step + (k * 5.0).sin() * 9.0, panel.center().y + k * k * 54.0);
        feather(p, c, 34.0, k * 2.6 + 0.2, 1.0 - k, true);
    }
    let mut popup_y = panel.bottom() + 12.0;
    if flash {
        shadow_text(p, pos2(panel.center().x, popup_y), Align2::CENTER_CENTER, "SAFE", theme::ui_bold(10.0), Color32::from_rgb(140, 230, 255));
        popup_y += 14.0;
    }
    for pop in &h.popups {
        let k = (pop.age / POPUP_SECS).clamp(0.0, 1.0);
        let pin = race_render::ease_out_elastic((pop.age / 0.3).clamp(0.0, 1.0));
        let fade = 1.0 - ((k - 0.6) / 0.4).clamp(0.0, 1.0);
        shadow_text(p, pos2(panel.center().x, popup_y), Align2::CENTER_CENTER, &pop.text, theme::title(15.0 * (0.6 + 0.4 * pin)), pop.color.gamma_multiply(fade));
        popup_y += 18.0;
    }

    // --- the live ranking, top left beside the minimap ---
    let rx = safe.left() + 8.0 + 108.0 + 9.0;
    let rh = 15.5;
    let rows = h.rows.len() as f32;
    let panel = Rect::from_min_size(pos2(rx, safe.top() + 8.0), vec2(126.0, 9.0 + rows * rh));
    p.rect_filled(panel, 9.0, Color32::from_rgba_unmultiplied(14, 8, 28, 150));
    p.rect_stroke(panel, 9.0, stroke(1.0, Color32::from_white_alpha(40)), egui::StrokeKind::Inside);
    for (k, r) in h.rows.iter().enumerate() {
        let y = panel.top() + 5.0 + k as f32 * rh + rh * 0.5;
        let dim = if r.out { 0.42 } else { 1.0 };
        if r.me {
            p.rect_filled(Rect::from_min_size(pos2(panel.left() + 2.0, y - rh * 0.5), vec2(panel.width() - 4.0, rh)), 6.0, theme::ACCENT.gamma_multiply(0.30));
        }
        p.text(pos2(panel.left() + 10.0, y), Align2::CENTER_CENTER, format!("{}", r.rank), theme::numbers(10.5), Color32::from_white_alpha((200.0 * dim) as u8));
        p.circle_filled(pos2(panel.left() + 22.0, y), 3.6, r.color.gamma_multiply(dim));
        let name: String = r.name.chars().take(7).collect();
        let col = if r.me { Color32::WHITE } else { theme::TEXT.gamma_multiply(dim) };
        p.text(pos2(panel.left() + 30.0, y), Align2::LEFT_CENTER, name, theme::ui_bold(10.0), col);
        if r.out {
            p.text(pos2(panel.right() - 6.0, y), Align2::RIGHT_CENTER, "OUT", theme::ui_bold(9.0), Color32::from_rgb(255, 110, 100).gamma_multiply(0.8));
        } else {
            let shown = r.feathers.min(5);
            for f in 0..shown {
                let fx = panel.right() - 9.0 - (shown - 1 - f) as f32 * 8.0;
                feather(p, pos2(fx, y), 11.0, 0.0, if r.invuln && (t * 12.0).sin() > 0.0 { 0.5 } else { 1.0 }, true);
            }
            if r.feathers > 5 {
                p.text(pos2(panel.right() - 54.0, y), Align2::RIGHT_CENTER, format!("x{}", r.feathers), theme::numbers(9.5), theme::SAND);
            }
        }
    }

    // --- the clock, right under the buttons ---
    if show_clock {
        let rx = safe.right() - 12.0;
        let low = h.time_left < 30.0;
        let col = if low { Color32::from_rgb(255, 96, 84).gamma_multiply(0.75 + 0.25 * (t * 8.0).sin().abs()) } else { Color32::from_rgb(255, 232, 170) };
        shadow_text(p, pos2(rx, safe.top() + 62.0), Align2::RIGHT_CENTER, "TIME LEFT", theme::ui_bold(10.5), theme::SAND);
        shadow_text(p, pos2(rx, safe.top() + 86.0), Align2::RIGHT_CENTER, &fmt_clock(h.time_left), theme::numbers(24.0), col);
    }

    // --- spectating ---
    if let Some((name, speed)) = &h.spectating {
        let c = pos2(safe.center().x, safe.bottom() - 46.0);
        let r = Rect::from_center_size(c, vec2(230.0, 30.0));
        pill(p, r, Color32::from_rgb(255, 110, 100).gamma_multiply(0.8));
        let sp = if *speed > 1.5 { format!("   FAST x{speed:.0}") } else { String::new() };
        shadow_text(p, c, Align2::CENTER_CENTER, &format!("WATCHING {name}{sp}"), theme::ui_bold(11.5), Color32::WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use race_core::BattleRules;

    #[test]
    fn rows_follow_the_ranking_with_out_racers_last() {
        let mut b = BattleState::new(BattleRules::default(), 4);
        b.feathers = vec![2, 3, 1, 0];
        b.out_order = vec![3];
        let names: Vec<String> = (0..4).map(|i| format!("R{i}")).collect();
        let r = rows(&b, &names, &[0, 1, 2, 3], 2);
        assert_eq!(r.iter().map(|x| x.racer).collect::<Vec<_>>(), vec![1, 0, 2, 3]);
        assert_eq!(r.iter().map(|x| x.rank).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert!(r[3].out && !r[0].out && r[2].me && r[2].feathers == 1);
    }

    #[test]
    fn a_lost_feather_flutters_off_then_is_gone() {
        let mut a = BattleAnim::new();
        a.update(0.016, 3);
        assert!(a.falling.is_empty());
        a.update(0.016, 2);
        assert_eq!(a.falling.len(), 1);
        assert_eq!(a.falling[0].0, 2, "the slot of the feather that went");
        a.update(0.016, 0);
        assert_eq!(a.falling.len(), 3, "two more slots");
        for _ in 0..80 {
            a.update(0.016, 0);
        }
        assert!(a.falling.is_empty());
        a.update(0.016, 1);
        assert!(a.gain_age < 0.1, "a steal pops the new feather");
    }

    #[test]
    fn clock_text() {
        assert_eq!(fmt_clock(180.0), "3:00");
        assert_eq!(fmt_clock(59.2), "1:00");
        assert_eq!(fmt_clock(-1.0), "0:00");
    }
}
