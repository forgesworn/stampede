//! The Time Trial ghost's HUD: a "GHOST +0.84" split pill at the top centre and a
//! "GHOST" tag floating over the pale ostrich.

use super::ghosts::fmt_split;
use super::hud::shadow_text;
use super::icons::stroke;
use crate::ui::theme;
use egui::{pos2, vec2, Align2, Color32, Painter, Pos2, Rect};

/// What the ghost HUD shows this frame.
#[derive(Clone, Copy, Debug)]
pub struct GhostHud {
    /// Seconds behind (+) or ahead (-) of the ghost at the same place on the track.
    pub split: Option<f32>,
    /// Where the ghost is on screen (UI points), if it is in view.
    pub tag: Option<Pos2>,
    /// The ghost has crossed the line already.
    pub finished: bool,
    /// The ghost's time.
    pub best: f32,
}

pub const GHOST_BLUE: Color32 = Color32::from_rgb(150, 215, 255);

pub fn draw(p: &Painter, safe: Rect, g: &GhostHud, mirror: bool, t: f32) {
    let y = safe.top() + if mirror { 120.0 } else { 26.0 };
    let r = Rect::from_center_size(pos2(safe.center().x, y), vec2(172.0, 30.0));
    p.rect_filled(r.translate(vec2(0.0, 2.0)), 15.0, Color32::from_black_alpha(70));
    p.rect_filled(r, 15.0, Color32::from_rgba_unmultiplied(14, 8, 28, 190));
    p.rect_stroke(r, 15.0, stroke(2.0, GHOST_BLUE.gamma_multiply(0.75)), egui::StrokeKind::Inside);
    // a small ghost glyph
    let gc = pos2(r.left() + 19.0, r.center().y);
    p.circle_filled(gc + vec2(0.0, -1.0), 7.0, GHOST_BLUE);
    p.rect_filled(Rect::from_min_max(gc + vec2(-7.0, -1.0), gc + vec2(7.0, 7.0)), 0.0, GHOST_BLUE);
    for k in 0..3 {
        p.circle_filled(gc + vec2(-4.7 + k as f32 * 4.7, 7.0), 2.35, GHOST_BLUE);
    }
    p.circle_filled(gc + vec2(-2.6, -2.0), 1.4, Color32::from_rgb(20, 14, 40));
    p.circle_filled(gc + vec2(2.6, -2.0), 1.4, Color32::from_rgb(20, 14, 40));
    shadow_text(p, pos2(r.left() + 36.0, r.center().y), Align2::LEFT_CENTER, "GHOST", theme::ui_bold(11.5), GHOST_BLUE);
    match g.split {
        Some(s) => {
            // Behind is red, ahead is green; a hair either way is neutral white.
            let col = if s > 0.05 {
                Color32::from_rgb(255, 120, 108)
            } else if s < -0.05 {
                Color32::from_rgb(120, 240, 150)
            } else {
                Color32::WHITE
            };
            shadow_text(p, pos2(r.right() - 14.0, r.center().y), Align2::RIGHT_CENTER, &fmt_split(s), theme::numbers(19.0), col);
        }
        None => {
            let txt = if g.finished { "finished".to_string() } else { "--".to_string() };
            shadow_text(p, pos2(r.right() - 14.0, r.center().y), Align2::RIGHT_CENTER, &txt, theme::ui_bold(12.0), Color32::from_white_alpha(150));
        }
    }
    // the tag over the ghost
    if let Some(c) = g.tag {
        let c = pos2(c.x.clamp(safe.left() + 30.0, safe.right() - 30.0), c.y.clamp(safe.top() + 50.0, safe.bottom() - 60.0));
        let bob = (t * 3.0).sin() * 1.5;
        let tr = Rect::from_center_size(c + vec2(0.0, bob), vec2(58.0, 17.0));
        p.rect_filled(tr, 8.5, Color32::from_rgba_unmultiplied(14, 8, 28, 170));
        p.rect_stroke(tr, 8.5, stroke(1.2, GHOST_BLUE.gamma_multiply(0.8)), egui::StrokeKind::Inside);
        p.text(tr.center(), Align2::CENTER_CENTER, "GHOST", theme::ui_bold(10.0), GHOST_BLUE);
        p.add(egui::Shape::convex_polygon(vec![tr.center_bottom() + vec2(-4.0, 0.0), tr.center_bottom() + vec2(4.0, 0.0), tr.center_bottom() + vec2(0.0, 5.0)], GHOST_BLUE.gamma_multiply(0.8), egui::Stroke::NONE));
    }
}
