//! Item and button icons drawn procedurally with egui shapes (no assets).

use crate::ui::theme;
use egui::epaint::{PathShape, PathStroke};
use egui::{pos2, vec2, Align2, Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};
use race_core::Item;
use std::f32::consts::{PI, TAU};

/// Stroke with the width typed (avoids float-literal inference warnings).
pub fn stroke(w: f32, c: Color32) -> Stroke {
    Stroke::new(w, c)
}

fn poly(p: &Painter, pts: Vec<Pos2>, fill: Color32, stroke: Stroke) {
    p.add(Shape::Path(PathShape::convex_polygon(pts, fill, PathStroke::from(stroke))));
}

fn rot(v: Vec2, a: f32) -> Vec2 {
    let (s, c) = a.sin_cos();
    vec2(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// Points of an ellipse centred at `c`, radii `(rx, ry)`, rotated by `a`.
pub fn ellipse(c: Pos2, rx: f32, ry: f32, a: f32, n: usize) -> Vec<Pos2> {
    (0..n)
        .map(|i| {
            let t = i as f32 / n as f32 * TAU;
            c + rot(vec2(t.cos() * rx, t.sin() * ry), a)
        })
        .collect()
}

/// An egg: wider at the bottom. `s` is the half height.
fn egg_points(c: Pos2, s: f32, a: f32) -> Vec<Pos2> {
    (0..28)
        .map(|i| {
            let t = i as f32 / 28.0 * TAU;
            let y = t.cos();
            // Taper toward the top.
            let w = t.sin() * (0.78 - 0.18 * y);
            c + rot(vec2(w * s, -y * s), a)
        })
        .collect()
}

pub fn arc_points(c: Pos2, r: f32, a0: f32, a1: f32, n: usize) -> Vec<Pos2> {
    (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / n as f32;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

/// Angles (radians, screen space, clockwise from 12 o'clock) of the part of a
/// radial timer that has run out when `left` (0..1) of it remains: from the top,
/// growing clockwise, so the timer depletes clockwise.
pub fn timer_elapsed_angles(left: f32) -> (f32, f32) {
    let gone = (1.0 - left).clamp(0.0, 1.0);
    (-PI / 2.0, -PI / 2.0 + TAU * gone)
}

/// A radial item timer over a button of radius `r`: the elapsed part is a dark
/// wedge over the icon, the part still to run a bright rim arc.
pub fn radial_timer(p: &Painter, c: Pos2, r: f32, left: f32, color: Color32) {
    let (a0, a1) = timer_elapsed_angles(left);
    if a1 - a0 > 0.01 {
        let n = ((a1 - a0) / TAU * 48.0).ceil().max(2.0) as usize;
        let mut mesh = egui::epaint::Mesh::default();
        let shade = Color32::from_black_alpha(150);
        mesh.colored_vertex(c, shade);
        for q in arc_points(c, r, a0, a1, n) {
            mesh.colored_vertex(q, shade);
        }
        for k in 0..n as u32 {
            mesh.add_triangle(0, k + 1, k + 2);
        }
        p.add(Shape::mesh(mesh));
    }
    let left = left.clamp(0.0, 1.0);
    if left > 0.001 {
        p.add(Shape::line(arc_points(c, r, a1, -PI / 2.0 + TAU, (48.0 * left).ceil().max(2.0) as usize), stroke(4.0, color)));
    }
}

pub fn ring_arc(p: &Painter, c: Pos2, r: f32, frac: f32, width: f32, color: Color32) {
    if frac <= 0.001 {
        return;
    }
    let pts = arc_points(c, r, -PI / 2.0, -PI / 2.0 + TAU * frac.min(1.0), (48.0 * frac.min(1.0)).ceil().max(2.0) as usize);
    p.add(Shape::line(pts, stroke(width, color)));
}

/// A bitcoin coin: gold disc, darker rim, a bold B with the two strokes through it.
pub fn bitcoin(p: &Painter, c: Pos2, r: f32) {
    p.circle_filled(c, r, theme::ACCENT);
    p.circle_stroke(c, r * 0.8, stroke((r * 0.12).max(1.0), Color32::from_rgb(255, 214, 120)));
    p.circle_stroke(c, r, stroke((r * 0.14).max(1.0), Color32::from_rgb(150, 70, 10)));
    let col = Color32::from_rgb(110, 48, 8);
    p.text(c + vec2(0.0, r * 0.04), Align2::CENTER_CENTER, "B", theme::numbers(r * 1.25), col);
    for dx in [-0.16f32, 0.16] {
        p.line_segment([c + vec2(dx * r, -r * 0.72), c + vec2(dx * r, -r * 0.5)], stroke((r * 0.13).max(1.0), col));
        p.line_segment([c + vec2(dx * r, r * 0.5), c + vec2(dx * r, r * 0.72)], stroke((r * 0.13).max(1.0), col));
    }
}

/// An item icon, fitting a circle of radius `s` around `c`.
pub fn item(p: &Painter, it: Item, c: Pos2, s: f32, t: f32) {
    let dark = Color32::from_black_alpha(130);
    match it {
        Item::Feather => {
            let a = 0.6;
            // Leaf body + gold tip + spine.
            poly(p, ellipse(c, s * 0.30, s * 0.86, a, 20), Color32::from_rgb(255, 236, 190), stroke(1.5, dark));
            poly(p, ellipse(c + rot(vec2(0.0, -s * 0.52), a), s * 0.2, s * 0.34, a, 14), theme::ACCENT, Stroke::NONE);
            p.line_segment([c + rot(vec2(0.0, s * 0.95), a), c + rot(vec2(0.0, -s * 0.78), a)], stroke(1.6, Color32::from_rgb(160, 100, 40)));
            for k in -2..=2 {
                let y = k as f32 * s * 0.26;
                p.line_segment([c + rot(vec2(0.0, y), a), c + rot(vec2(s * 0.24, y - s * 0.16), a)], stroke(1.0, Color32::from_rgb(200, 150, 80)));
            }
            // Speed streaks.
            for k in 0..2 {
                let o = vec2(-s * 0.85, s * (0.2 + 0.3 * k as f32));
                p.line_segment([c + o, c + o + vec2(-s * 0.3, s * 0.12)], stroke(2.0, Color32::from_white_alpha(120)));
            }
        }
        Item::RollingEgg => {
            poly(p, egg_points(c + vec2(s * 0.08, 0.0), s * 0.82, 0.0), Color32::from_rgb(252, 238, 214), stroke(1.5, dark));
            for (dx, dy) in [(-0.2f32, -0.1f32), (0.25, 0.2), (0.0, 0.42), (0.28, -0.3)] {
                p.circle_filled(c + vec2(s * (0.08 + dx * 0.6), s * dy * 0.9), s * 0.07, Color32::from_rgb(190, 150, 110));
            }
            for k in 0..3 {
                let y = -s * 0.4 + k as f32 * s * 0.4;
                p.line_segment([c + vec2(-s * 0.95, y), c + vec2(-s * 0.5, y)], stroke(2.0, Color32::from_white_alpha(110)));
            }
        }
        Item::HunterEgg => {
            poly(p, egg_points(c, s * 0.8, 0.0), Color32::from_rgb(214, 56, 50), stroke(1.5, dark));
            let rr = s * 0.5;
            p.circle_stroke(c, rr, stroke(2.0, Color32::from_rgb(255, 232, 200)));
            p.line_segment([c + vec2(-rr * 1.4, 0.0), c + vec2(rr * 1.4, 0.0)], stroke(1.6, Color32::from_rgb(255, 232, 200)));
            p.line_segment([c + vec2(0.0, -rr * 1.4), c + vec2(0.0, rr * 1.4)], stroke(1.6, Color32::from_rgb(255, 232, 200)));
            p.circle_filled(c, s * 0.08, Color32::from_rgb(255, 232, 200));
        }
        Item::Cactus => {
            let g = Color32::from_rgb(70, 170, 80);
            let gd = Color32::from_rgb(40, 110, 55);
            let r = |x0: f32, y0: f32, x1: f32, y1: f32, cr: f32, col: Color32| {
                p.rect_filled(Rect::from_min_max(c + vec2(x0, y0) * s, c + vec2(x1, y1) * s), cr * s, col);
            };
            r(-0.2, -0.85, 0.2, 0.9, 0.2, g);
            r(-0.62, -0.1, -0.2, 0.12, 0.1, g);
            r(-0.62, -0.5, -0.38, 0.12, 0.12, g);
            r(0.2, 0.15, 0.62, 0.37, 0.1, g);
            r(0.38, -0.3, 0.62, 0.37, 0.12, g);
            for k in 0..4 {
                let y = -0.6 + k as f32 * 0.4;
                p.line_segment([c + vec2(0.0, y) * s, c + vec2(0.0, y + 0.18) * s], stroke(1.5, gd));
            }
            r(-0.5, 0.88, 0.5, 1.0, 0.05, Color32::from_rgb(150, 100, 60));
        }
        Item::Shield => {
            let top = -0.85f32;
            let mut pts = vec![pos2(c.x - s * 0.7, c.y + top * s), pos2(c.x + s * 0.7, c.y + top * s)];
            pts.extend(arc_points(c + vec2(0.0, -0.1 * s), s * 0.8, 0.0, PI * 0.5 + 0.5, 8).into_iter().map(|q| pos2(c.x + (q.x - c.x) * 0.875, q.y + s * 0.2)));
            pts.push(pos2(c.x, c.y + s * 0.98));
            pts.extend(arc_points(c + vec2(0.0, -0.1 * s), s * 0.8, PI * 0.5 - 0.5, PI, 8).into_iter().map(|q| pos2(c.x + (q.x - c.x) * 0.875, q.y + s * 0.2)));
            // Convex shield from a hand-picked outline.
            let outline = vec![
                pos2(c.x - s * 0.72, c.y - s * 0.8),
                pos2(c.x + s * 0.72, c.y - s * 0.8),
                pos2(c.x + s * 0.74, c.y + s * 0.05),
                pos2(c.x + s * 0.5, c.y + s * 0.6),
                pos2(c.x, c.y + s * 0.98),
                pos2(c.x - s * 0.5, c.y + s * 0.6),
                pos2(c.x - s * 0.74, c.y + s * 0.05),
            ];
            let _ = pts;
            poly(p, outline.clone(), Color32::from_rgb(120, 70, 200), stroke(2.0, Color32::from_rgb(255, 214, 120)));
            let inner: Vec<Pos2> = outline.iter().map(|q| c + (*q - c) * 0.72).collect();
            poly(p, inner, Color32::from_rgb(160, 110, 235), Stroke::NONE);
            p.text(c + vec2(0.0, -s * 0.05), Align2::CENTER_CENTER, "H", theme::numbers(s * 1.0), Color32::from_rgb(255, 232, 160));
        }
        Item::Lightning => {
            let col = Color32::from_rgb(255, 224, 70);
            let o = Color32::from_rgb(180, 110, 20);
            // Two convex pieces make a bolt.
            poly(p, vec![c + vec2(0.18, -0.98) * s, c + vec2(-0.5, 0.1) * s, c + vec2(0.05, 0.1) * s, c + vec2(0.55, -0.98) * s], col, stroke(1.5, o));
            poly(p, vec![c + vec2(0.22, -0.1) * s, c + vec2(-0.25, -0.1) * s, c + vec2(-0.3, 0.98) * s, c + vec2(0.55, -0.1) * s], col, stroke(1.5, o));
            poly(p, vec![c + vec2(-0.5, 0.1) * s, c + vec2(0.05, 0.1) * s, c + vec2(0.22, -0.1) * s, c + vec2(-0.25, -0.1) * s], col, Stroke::NONE);
        }
        Item::MegaLaser => {
            let wob = 1.0 + 0.1 * (t * 18.0).sin();
            // Beam first, then the eye on top.
            p.rect_filled(Rect::from_min_max(c + vec2(0.1, -0.2 * wob) * s, c + vec2(1.05, 0.2 * wob) * s), 0.1 * s, Color32::from_rgb(255, 60, 60));
            p.rect_filled(Rect::from_min_max(c + vec2(0.1, -0.09) * s, c + vec2(1.05, 0.09) * s), 0.05 * s, Color32::from_rgb(255, 230, 220));
            poly(p, ellipse(c + vec2(-0.2, 0.0) * s, s * 0.62, s * 0.4, 0.0, 22), Color32::from_rgb(250, 244, 236), stroke(1.5, dark));
            p.circle_filled(c + vec2(-0.1, 0.0) * s, s * 0.26, Color32::from_rgb(220, 30, 40));
            p.circle_filled(c + vec2(-0.1, 0.0) * s, s * 0.11, Color32::BLACK);
        }
        Item::Laptop => {
            let screen = Rect::from_center_size(c + vec2(0.0, -s * 0.2), vec2(s * 1.5, s * 1.05));
            p.rect_filled(screen, s * 0.09, Color32::from_rgb(35, 42, 54));
            p.rect_filled(screen.shrink(s * 0.10), s * 0.03, Color32::from_rgb(35, 130, 85));
            bitcoin(p, screen.center(), s * 0.31);
            poly(p, vec![c + vec2(-0.75, 0.34) * s, c + vec2(0.75, 0.34) * s, c + vec2(0.94, 0.68) * s, c + vec2(-0.94, 0.68) * s], Color32::from_rgb(150, 160, 175), stroke(1.2, dark));
            for k in -2..=2 { p.line_segment([c + vec2(k as f32 * 0.23, 0.40) * s, c + vec2(k as f32 * 0.25, 0.57) * s], stroke(1.0, dark)); }
        }
        Item::OrangePill => {
            let a = -0.65;
            poly(p, ellipse(c, s * 0.46, s * 0.85, a, 28), Color32::from_rgb(255, 238, 210), stroke(1.5, dark));
            poly(p, ellipse(c + rot(vec2(0.0, -s * 0.33), a), s * 0.44, s * 0.50, a, 24), theme::ACCENT, Stroke::NONE);
            p.line_segment([c + rot(vec2(-s * 0.38, 0.0), a), c + rot(vec2(s * 0.38, 0.0), a)], stroke(1.5, dark));
        }
        Item::EggScrambler => {
            let a = 0.55;
            let head = c + rot(vec2(0.0, -0.40), a) * s;
            poly(p, ellipse(head, s * 0.51, s * 0.68, a, 28), Color32::from_rgba_unmultiplied(255, 130, 10, 45), Stroke::NONE);
            p.line_segment([c + rot(vec2(0.0, 0.16), a) * s, c + rot(vec2(0.0, 0.88), a) * s], stroke(s * 0.20, Color32::from_rgb(155, 65, 12)));
            p.line_segment([c + rot(vec2(-0.025, 0.22), a) * s, c + rot(vec2(-0.025, 0.80), a) * s], stroke(s * 0.09, theme::ACCENT));
            // Balloon whisk: three steel wire loops meet at the handle.
            for width in [0.37, 0.22, 0.055] {
                let wire = ellipse(head, s * width, s * 0.54, a, 28);
                p.add(Shape::closed_line(wire.clone(), stroke((s * 0.075).max(2.0), Color32::from_rgb(105, 113, 123))));
                p.add(Shape::closed_line(wire, stroke((s * 0.035).max(1.0), Color32::from_rgb(247, 241, 219))));
            }
            for k in 0..4 { let a = t * 1.5 + k as f32 * PI * 0.5; p.circle_filled(head + vec2(a.cos(), a.sin()) * s * 0.65, s * 0.05, theme::ACCENT); }
        }
        Item::RugPull => {
            let rug = Rect::from_center_size(c, vec2(s * 1.5, s));
            p.rect_filled(rug, s * 0.1, Color32::from_rgb(165, 45, 72));
            p.line_segment([rug.left_center(), rug.right_center()], stroke(s * 0.1, Color32::GOLD));
        }
        Item::MoneyPrinter => {
            let machine = Rect::from_center_size(c, vec2(s * 1.4, s * 0.95));
            p.rect_filled(machine, s * 0.12, Color32::from_rgb(65, 86, 73));
            p.rect_filled(
                Rect::from_center_size(c + vec2(0.0, -0.53) * s, vec2(s * 0.9, s * 0.2)),
                1.0,
                Color32::from_rgb(230, 238, 205),
            );
            let bill = Rect::from_center_size(c + vec2(0.0, 0.45) * s, vec2(s * 0.95, s * 0.6));
            p.rect_filled(bill, 1.0, Color32::from_rgb(140, 212, 120));
            p.text(
                bill.center(),
                Align2::CENTER_CENTER,
                "$",
                theme::numbers(s * 0.48),
                Color32::from_rgb(18, 80, 40),
            );
            let crank = c + vec2(s * 0.9, 0.0);
            let hand = crank + vec2((t * 8.0).cos(), (t * 8.0).sin()) * s * 0.35;
            p.line_segment(
                [c + vec2(s * 0.6, 0.0), crank],
                stroke(s * 0.1, theme::SAND),
            );
            p.line_segment([crank, hand], stroke(s * 0.12, theme::SAND));
            p.circle_filled(hand, s * 0.13, theme::TEXT);
        }
        Item::SharedCake => {
            p.add(Shape::convex_polygon(ellipse(c + vec2(0.0, 0.7) * s, s * 0.95, s * 0.22, 0.0, 24), Color32::from_rgb(240, 240, 250), Stroke::NONE));
            for (y, color) in [(0.35, theme::ACCENT), (0.0, Color32::from_rgb(255, 248, 230)), (-0.35, theme::ACCENT)] {
                p.rect_filled(Rect::from_center_size(c + vec2(0.0, y) * s, vec2(1.35 * s, 0.36 * s)), 0.03 * s, color);
            }
            poly(p, ellipse(c + vec2(0.0, -0.53) * s, s * 0.675, s * 0.18, 0.0, 24), Color32::from_rgb(255, 248, 230), Stroke::NONE);
            p.circle_filled(c + vec2(0.0, -0.68) * s, s * 0.14, theme::ACCENT);
        }
        Item::LaserTrump => {
            // A puff of green gas with a laser streak shooting out of it.
            let pulse = 0.85 + 0.15 * (t * 18.0).sin();
            let gas = Color32::from_rgb(150, 205, 90);
            let gas_dark = Color32::from_rgb(98, 150, 60);
            let src = c + vec2(-s * 0.22, 0.0);
            for (dx, dy, r, col) in [(-0.34f32, 0.2f32, 0.34f32, gas_dark), (-0.2, -0.22, 0.36, gas_dark), (-0.52, -0.05, 0.3, gas_dark), (-0.3, 0.12, 0.36, gas), (-0.18, -0.2, 0.32, gas), (-0.5, -0.04, 0.26, gas)] {
                p.circle_filled(c + vec2(dx, dy) * s + vec2(s * 0.14, 0.0), s * r, col);
            }
            // The beam: a wide green glow, a lighter body and a white core.
            let tip = c + vec2(s * 1.0, -s * 0.08);
            p.line_segment([src, tip], stroke(s * 0.5 * pulse, Color32::from_rgba_unmultiplied(70, 255, 100, 90)));
            p.line_segment([src, tip], stroke(s * 0.28, Color32::from_rgb(120, 255, 140)));
            p.line_segment([src, tip], stroke(s * 0.11, Color32::from_rgb(255, 255, 240)));
            p.circle_filled(src, s * 0.2 * pulse, Color32::from_rgb(240, 255, 220));
            // Speed streaks above and below.
            for k in 0..2 {
                let y = (0.38 + 0.22 * k as f32) * if k == 0 { -1.0 } else { 1.0 };
                p.line_segment([c + vec2(s * 0.1, y * s), c + vec2(s * 0.78, y * s - s * 0.03)], stroke(1.6, Color32::from_white_alpha(120)));
            }
        }
        Item::Rose => {
            // One red rose on a long stem, with a leaf.
            let green = Color32::from_rgb(70, 170, 80);
            let stem_top = c + vec2(s * 0.05, -s * 0.3);
            p.line_segment([c + vec2(-s * 0.12, s * 0.95), stem_top], stroke(s * 0.12, green));
            poly(p, ellipse(c + vec2(s * 0.22, s * 0.35), s * 0.28, s * 0.12, -0.6, 14), green, Stroke::NONE);
            let bloom = c + vec2(s * 0.05, -s * 0.48);
            let dark = Color32::from_rgb(150, 18, 40);
            let red = Color32::from_rgb(222, 34, 58);
            let light = Color32::from_rgb(255, 92, 110);
            // Sepals, then petals outside in.
            for a in [-0.9f32, 0.0, 0.9] {
                poly(p, ellipse(bloom + rot(vec2(0.0, s * 0.32), a), s * 0.09, s * 0.2, a, 10), green, Stroke::NONE);
            }
            poly(p, ellipse(bloom, s * 0.5, s * 0.42, 0.0, 22), dark, Stroke::NONE);
            for k in 0..5 {
                let a = k as f32 / 5.0 * TAU + 0.3;
                p.circle_filled(bloom + vec2(a.cos(), a.sin() * 0.8) * s * 0.26, s * 0.21, red);
            }
            p.circle_filled(bloom + vec2(0.0, -s * 0.04), s * 0.2, light);
            p.add(Shape::line(arc_points(bloom + vec2(s * 0.02, -s * 0.04), s * 0.11, 0.2, 5.0, 12), stroke(s * 0.05, dark)));
        }
        Item::Magnet => {
            let red = Color32::from_rgb(230, 50, 60);
            let r = s * 0.5;
            let cc = c + vec2(0.0, s * 0.1);
            p.add(Shape::line(arc_points(cc, r, PI, TAU, 18), stroke(s * 0.36, red)));
            for sx in [-1.0f32, 1.0] {
                let x = cc.x + sx * r;
                p.rect_filled(Rect::from_min_max(pos2(x - s * 0.18, cc.y), pos2(x + s * 0.18, cc.y + s * 0.52)), 0.0, red);
                p.rect_filled(Rect::from_min_max(pos2(x - s * 0.18, cc.y + s * 0.52), pos2(x + s * 0.18, cc.y + s * 0.8)), 0.0, Color32::from_rgb(240, 240, 248));
            }
            // Sparkles.
            let a = 0.5 + 0.5 * (t * 6.0).sin();
            p.circle_filled(c + vec2(0.0, -s * 0.9), s * 0.07 * (0.6 + a), theme::ACCENT);
        }
    }
}

/// Short names for tutorials.
pub fn item_label(it: Item) -> &'static str {
    match it {
        Item::Feather => "FEATHER",
        Item::RollingEgg => "EGG",
        Item::HunterEgg => "HUNTER",
        Item::Cactus => "CACTUS",
        Item::Shield => "HODL",
        Item::Lightning => "BOLT",
        Item::MegaLaser => "LASER",
        Item::Magnet => "MAGNET",
        Item::Laptop => "LAPTOP",
        Item::OrangePill => "PILL",
        Item::EggScrambler => "WHISK",
        Item::MoneyPrinter => "PRINTER",
        Item::RugPull => "RUG",
        Item::SharedCake => "CAKE",
        Item::LaserTrump => "LASER TRUMP",
        Item::Rose => "ROSE",
    }

}

/// A small icon for the buttons: a pause bar pair, a camera, a U-turn arrow, chevrons.
pub fn glyph(p: &Painter, name: &str, c: Pos2, s: f32, col: Color32) {
    let st = stroke((s * 0.22).max(1.5), col);
    match name {
        "pause" => {
            for dx in [-0.3f32, 0.3] {
                p.rect_filled(Rect::from_center_size(c + vec2(dx * s, 0.0), vec2(0.28 * s, 0.9 * s)), 0.05 * s, col);
            }
        }
        "camera" => {
            p.rect_stroke(Rect::from_center_size(c + vec2(0.0, 0.08 * s), vec2(1.3 * s, 0.9 * s)), 0.18 * s, st, egui::StrokeKind::Middle);
            p.circle_stroke(c + vec2(0.0, 0.08 * s), 0.26 * s, st);
            p.rect_filled(Rect::from_center_size(c + vec2(0.0, -0.5 * s), vec2(0.5 * s, 0.2 * s)), 0.04 * s, col);
        }
        "back" => {
            // A U-turn arrow.
            p.add(Shape::line(arc_points(c + vec2(0.0, -0.1 * s), 0.45 * s, PI, TAU + 0.6, 14).into_iter().map(|q| pos2(q.x, q.y)).collect(), st));
            p.add(Shape::line(vec![c + vec2(-0.45 * s, -0.1 * s), c + vec2(-0.45 * s, 0.45 * s)], st));
            let tip = c + vec2(0.45 * s, 0.05 * s);
            poly(p, vec![tip + vec2(-0.3, -0.05) * s, tip + vec2(0.3, -0.05) * s, tip + vec2(0.0, 0.38) * s], col, Stroke::NONE);
        }
        "left" | "right" => {
            let d = if name == "left" { -1.0 } else { 1.0 };
            for k in 0..2 {
                let x = (k as f32 * 0.4 - 0.2) * d;
                p.add(Shape::line(vec![c + vec2((x - 0.2 * d) * s, -0.45 * s), c + vec2((x + 0.2 * d) * s, 0.0), c + vec2((x - 0.2 * d) * s, 0.45 * s)], st));
            }
        }
        "stop" => {
            p.rect_filled(Rect::from_center_size(c, vec2(0.8 * s, 0.8 * s)), 0.12 * s, col);
        }
        "jump" => {
            for k in 0..2 {
                let y = (k as f32 * 0.5 - 0.25) * s;
                p.add(Shape::line(vec![c + vec2(-0.5 * s, y + 0.2 * s), c + vec2(0.0, y - 0.25 * s), c + vec2(0.5 * s, y + 0.2 * s)], st));
            }
        }
        "drift" => {
            // Two skid curves.
            for k in 0..2 {
                let o = k as f32 * 0.42 - 0.21;
                p.add(Shape::line(arc_points(c + vec2((0.4 + o) * s, 0.5 * s), 0.8 * s, PI * 1.05, PI * 1.5, 10), st));
            }
        }
        "bash" => {
            // Impact star.
            for k in 0..8 {
                let a = k as f32 / 8.0 * TAU;
                let (r0, r1) = if k % 2 == 0 { (0.18, 0.62) } else { (0.18, 0.4) };
                p.line_segment([c + vec2(a.cos(), a.sin()) * r0 * s, c + vec2(a.cos(), a.sin()) * r1 * s], st);
            }
        }
        "eye" => {
            poly(p, ellipse(c, 0.62 * s, 0.38 * s, 0.0, 20), Color32::from_rgb(250, 244, 236), stroke(1.2, Color32::from_black_alpha(120)));
            p.circle_filled(c + vec2(0.1 * s, 0.0), 0.23 * s, Color32::from_rgb(220, 30, 40));
            p.circle_filled(c + vec2(0.1 * s, 0.0), 0.09 * s, Color32::BLACK);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_item_timer_depletes_clockwise_from_the_top() {
        // Full: nothing has run out. Empty: the whole disc has.
        let (a0, a1) = timer_elapsed_angles(1.0);
        assert!((a1 - a0).abs() < 1e-5);
        assert!((a0 + PI / 2.0).abs() < 1e-5, "starts at 12 o'clock");
        let (_, a1) = timer_elapsed_angles(0.0);
        assert!((a1 - a0 - TAU).abs() < 1e-5);
        // A quarter gone ends at 3 o'clock (clockwise on screen); the gone part only grows.
        let (_, q) = timer_elapsed_angles(0.75);
        assert!(q.abs() < 1e-5, "a quarter is gone: {q}");
        let mut last = a0;
        for k in (0..=20).rev() {
            let (_, end) = timer_elapsed_angles(k as f32 / 20.0);
            assert!(end >= last - 1e-6);
            last = end;
        }
    }
}
