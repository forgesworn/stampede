//! Vector icons drawn with the painter (no icon font, no images): crisp at any
//! scale and tintable. Each icon lives in a unit box centred on the given point.

use egui::{vec2, Color32, Painter, Pos2, Shape, Stroke};
use std::f32::consts::{PI, TAU};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Flag,
    Trophy,
    Stopwatch,
    Globe,
    Wheel,
    Ostrich,
    Gear,
    Info,
    ChevronLeft,
    ChevronRight,
    Lock,
    Star,
    Check,
    Plus,
    Minus,
    Close,
    Backspace,
    // training controls
    Steer,
    Drift,
    Jump,
    Laser,
    Bash,
    Item,
    // multiplayer
    Host,
    Join,
    Nearby,
    Egg,
    Medal,
}

fn arc(c: Pos2, r: f32, a0: f32, a1: f32, n: usize) -> Vec<Pos2> {
    (0..=n).map(|i| c + vec2((a0 + (a1 - a0) * i as f32 / n as f32).cos(), (a0 + (a1 - a0) * i as f32 / n as f32).sin()) * r).collect()
}

fn star_points(c: Pos2, r: f32, rot: f32) -> Vec<Pos2> {
    (0..10)
        .map(|i| {
            let a = rot - PI / 2.0 + i as f32 * PI / 5.0;
            let rr = if i % 2 == 0 { r } else { r * 0.46 };
            c + vec2(a.cos(), a.sin()) * rr
        })
        .collect()
}

/// Fill a star-shaped polygon (every point visible from `c`) as a triangle fan.
pub fn fan(p: &Painter, c: Pos2, pts: &[Pos2], fill: Color32) {
    let mut m = egui::epaint::Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    m.vertices.push(egui::epaint::Vertex { pos: c, uv, color: fill });
    for q in pts {
        m.vertices.push(egui::epaint::Vertex { pos: *q, uv, color: fill });
    }
    let n = pts.len() as u32;
    for i in 0..n {
        m.indices.extend([0, 1 + i, 1 + (i + 1) % n]);
    }
    p.add(Shape::mesh(m));
    // a hairline of the same colour smooths the edge
    p.add(Shape::closed_line(pts.to_vec(), Stroke::new(0.6_f32, fill)));
}

/// A five-point star centred on `c` (outer radius `r`).
pub fn star(p: &Painter, c: Pos2, r: f32, fill: Color32, stroke: Stroke) {
    let pts = star_points(c, r, 0.0);
    fan(p, c, &pts, fill);
    if stroke.width > 0.0 {
        p.add(Shape::closed_line(pts, stroke));
    }
}

fn thick(p: &Painter, pts: Vec<Pos2>, w: f32, col: Color32) {
    if let (Some(a), Some(b)) = (pts.first().copied(), pts.last().copied()) {
        p.circle_filled(a, w / 2.0, col);
        p.circle_filled(b, w / 2.0, col);
    }
    p.add(Shape::line(pts, Stroke::new(w, col)));
}

/// Draw `icon` centred on `c`, `s` points across, in `col`.
pub fn draw(p: &Painter, icon: Icon, c: Pos2, s: f32, col: Color32) {
    let u = |x: f32, y: f32| c + vec2(x, y) * s;
    let w = s * 0.11;
    let st = Stroke::new(w, col);
    match icon {
        Icon::Flag => {
            thick(p, vec![u(-0.30, -0.44), u(-0.30, 0.46)], w, col);
            // chequered cloth, three by two
            let (x0, y0, cw, ch) = (-0.30, -0.42, 0.22, 0.17);
            for i in 0..3 {
                for j in 0..2 {
                    let dark = (i + j) % 2 == 0;
                    let wave = (i as f32 * 0.9).sin() * 0.025;
                    let r = egui::Rect::from_min_size(u(x0 + cw * i as f32, y0 + ch * j as f32 + wave), vec2(cw * s, ch * s));
                    p.rect_filled(r, 0.0, if dark { col } else { col.gamma_multiply(0.38) });
                }
            }
        }
        Icon::Trophy => {
            let cup = vec![u(-0.28, -0.40), u(0.28, -0.40), u(0.24, -0.06), u(0.12, 0.10), u(-0.12, 0.10), u(-0.24, -0.06)];
            p.add(Shape::convex_polygon(cup, col, Stroke::NONE));
            thick(p, arc(u(-0.30, -0.22), 0.13 * s, PI * 0.5, PI * 1.5, 10), w * 0.8, col);
            thick(p, arc(u(0.30, -0.22), 0.13 * s, -PI * 0.5, PI * 0.5, 10), w * 0.8, col);
            p.rect_filled(egui::Rect::from_center_size(u(0.0, 0.18), vec2(0.12 * s, 0.16 * s)), 0.0, col);
            p.rect_filled(egui::Rect::from_center_size(u(0.0, 0.34), vec2(0.46 * s, 0.11 * s)), s * 0.03, col);
        }
        Icon::Stopwatch => {
            p.circle_stroke(u(0.0, 0.06), 0.34 * s, st);
            p.rect_filled(egui::Rect::from_center_size(u(0.0, -0.40), vec2(0.14 * s, 0.09 * s)), s * 0.02, col);
            thick(p, vec![u(0.22, -0.18), u(0.30, -0.28)], w * 0.8, col);
            thick(p, vec![u(0.0, 0.06), u(0.0, -0.16)], w * 0.8, col);
            thick(p, vec![u(0.0, 0.06), u(0.14, 0.12)], w * 0.8, col);
        }
        Icon::Globe => {
            p.circle_stroke(c, 0.38 * s, st);
            let thin = Stroke::new(w * 0.7, col);
            p.hline((c.x - 0.38 * s)..=(c.x + 0.38 * s), c.y, thin);
            let mer: Vec<Pos2> = (0..=24).map(|i| {
                let a = i as f32 / 24.0 * TAU;
                c + vec2(a.cos() * 0.17 * s, a.sin() * 0.38 * s)
            }).collect();
            p.add(Shape::line(mer, thin));
            p.add(Shape::line(arc(u(0.0, -0.62), 0.5 * s, PI * 0.30, PI * 0.70, 12), thin));
            p.add(Shape::line(arc(u(0.0, 0.62), 0.5 * s, -PI * 0.70, -PI * 0.30, 12), thin));
        }
        Icon::Wheel => {
            p.circle_stroke(c, 0.38 * s, Stroke::new(w * 1.1, col));
            p.circle_filled(c, 0.10 * s, col);
            for a in [PI * 0.5 + 0.0, PI * 0.5 + TAU / 3.0, PI * 0.5 + 2.0 * TAU / 3.0] {
                thick(p, vec![c, c + vec2(a.cos(), a.sin()) * 0.34 * s], w * 0.9, col);
            }
        }
        Icon::Ostrich => {
            // body, neck, head, beak, eye, a tuft of tail feathers
            let body: Vec<Pos2> = (0..24).map(|i| {
                let a = i as f32 / 24.0 * TAU;
                u(-0.12 + a.cos() * 0.27, 0.27 + a.sin() * 0.17)
            }).collect();
            fan(p, u(-0.12, 0.27), &body, col);
            thick(p, vec![u(-0.10, 0.22), u(-0.10, 0.02), u(0.0, -0.16), u(0.05, -0.28)], w * 1.7, col);
            p.circle_filled(u(0.08, -0.32), 0.11 * s, col);
            p.add(Shape::convex_polygon(vec![u(0.15, -0.39), u(0.44, -0.29), u(0.15, -0.24)], col, Stroke::NONE));
            p.circle_filled(u(0.09, -0.34), 0.026 * s, Color32::from_rgb(0xFF, 0x2D, 0x3F));
            for (dx, dy) in [(-0.30, 0.10), (-0.36, 0.20), (-0.38, 0.30)] {
                thick(p, vec![u(-0.28, 0.24), u(dx, dy)], w * 0.8, col);
            }
        }
        Icon::Gear => {
            for i in 0..8 {
                let a = i as f32 / 8.0 * TAU;
                let d = vec2(a.cos(), a.sin());
                thick(p, vec![c + d * 0.26 * s, c + d * 0.41 * s], w * 1.5, col);
            }
            p.circle_stroke(c, 0.27 * s, Stroke::new(w * 1.5, col));
            p.circle_filled(c, 0.06 * s, col);
        }
        Icon::Info => {
            p.circle_stroke(c, 0.38 * s, st);
            p.circle_filled(u(0.0, -0.18), 0.05 * s, col);
            thick(p, vec![u(0.0, -0.05), u(0.0, 0.20)], w * 1.1, col);
        }
        Icon::ChevronLeft => thick(p, vec![u(0.14, -0.30), u(-0.12, 0.0), u(0.14, 0.30)], w * 1.3, col),
        Icon::ChevronRight => thick(p, vec![u(-0.14, -0.30), u(0.12, 0.0), u(-0.14, 0.30)], w * 1.3, col),
        Icon::Lock => {
            thick(p, arc(u(0.0, -0.08), 0.15 * s, PI, TAU, 12), w, col);
            p.rect_filled(egui::Rect::from_center_size(u(0.0, 0.14), vec2(0.46 * s, 0.34 * s)), s * 0.06, col);
            p.circle_filled(u(0.0, 0.13), 0.04 * s, Color32::from_black_alpha(160));
        }
        Icon::Star => star(p, c, 0.44 * s, col, Stroke::NONE),
        Icon::Check => thick(p, vec![u(-0.28, 0.02), u(-0.08, 0.24), u(0.30, -0.22)], w * 1.4, col),
        Icon::Plus => {
            thick(p, vec![u(-0.28, 0.0), u(0.28, 0.0)], w * 1.4, col);
            thick(p, vec![u(0.0, -0.28), u(0.0, 0.28)], w * 1.4, col);
        }
        Icon::Minus => thick(p, vec![u(-0.28, 0.0), u(0.28, 0.0)], w * 1.4, col),
        Icon::Close => {
            thick(p, vec![u(-0.24, -0.24), u(0.24, 0.24)], w * 1.3, col);
            thick(p, vec![u(0.24, -0.24), u(-0.24, 0.24)], w * 1.3, col);
        }
        Icon::Backspace => {
            let body = vec![u(-0.42, 0.0), u(-0.20, -0.26), u(0.40, -0.26), u(0.40, 0.26), u(-0.20, 0.26)];
            p.add(Shape::closed_line(body, Stroke::new(w * 0.9, col)));
            thick(p, vec![u(-0.02, -0.10), u(0.22, 0.10)], w * 0.9, col);
            thick(p, vec![u(0.22, -0.10), u(-0.02, 0.10)], w * 0.9, col);
        }
        Icon::Steer => {
            p.circle_stroke(c, 0.20 * s, Stroke::new(w * 0.9, col));
            p.circle_filled(c, 0.07 * s, col);
            thick(p, vec![u(-0.44, 0.0), u(-0.30, 0.0)], w, col);
            thick(p, vec![u(-0.36, -0.08), u(-0.44, 0.0), u(-0.36, 0.08)], w * 0.8, col);
            thick(p, vec![u(0.44, 0.0), u(0.30, 0.0)], w, col);
            thick(p, vec![u(0.36, -0.08), u(0.44, 0.0), u(0.36, 0.08)], w * 0.8, col);
        }
        Icon::Drift => {
            thick(p, arc(u(0.12, 0.10), 0.34 * s, PI * 0.95, PI * 1.75, 14), w * 1.3, col);
            thick(p, vec![u(-0.10, -0.34), u(0.12, -0.24), u(-0.06, -0.10)], w, col);
            for (x, y) in [(0.30, 0.30), (0.38, 0.14), (0.20, 0.40)] {
                p.circle_filled(u(x, y), 0.04 * s, theme_orange());
            }
        }
        Icon::Jump => {
            thick(p, vec![u(0.0, 0.36), u(0.0, -0.22)], w * 1.4, col);
            thick(p, vec![u(-0.20, -0.04), u(0.0, -0.26), u(0.20, -0.04)], w * 1.4, col);
            thick(p, arc(u(0.0, 0.46), 0.36 * s, PI * 1.12, PI * 1.88, 10), w * 0.8, col.gamma_multiply(0.55));
        }
        Icon::Laser => {
            p.circle_filled(u(-0.28, 0.0), 0.12 * s, col);
            thick(p, vec![u(-0.14, -0.04), u(0.42, -0.18)], w, theme_laser());
            thick(p, vec![u(-0.14, 0.04), u(0.42, 0.18)], w, theme_laser());
            thick(p, vec![u(-0.14, 0.0), u(0.44, 0.0)], w * 1.4, theme_laser());
        }
        Icon::Bash => {
            p.circle_filled(u(-0.20, 0.06), 0.15 * s, col);
            thick(p, vec![u(-0.30, 0.30), u(-0.22, 0.12)], w * 1.4, col);
            let burst = star_points(u(0.26, -0.10), 0.22 * s, 0.3);
            fan(p, u(0.26, -0.10), &burst, theme_orange());
        }
        Icon::Item => {
            p.add(Shape::convex_polygon((0..24).map(|i| {
                let a = i as f32 / 24.0 * TAU;
                u(a.cos() * 0.22, 0.04 + a.sin() * 0.30 + if a.sin() < 0.0 { a.sin() * 0.04 } else { 0.0 })
            }).collect(), col, Stroke::NONE));
            for (x, y, r) in [(0.30, -0.30, 0.10), (-0.30, -0.22, 0.07)] {
                thick(p, vec![u(x, y - r), u(x, y + r)], w * 0.6, col);
                thick(p, vec![u(x - r, y), u(x + r, y)], w * 0.6, col);
            }
        }
        Icon::Host => {
            p.circle_filled(c, 0.07 * s, col);
            for r in [0.20, 0.34] {
                thick(p, arc(c, r * s, -PI * 0.28, PI * 0.28, 8), w * 0.9, col);
                thick(p, arc(c, r * s, PI - PI * 0.28, PI + PI * 0.28, 8), w * 0.9, col);
            }
            thick(p, vec![c + vec2(0.0, 0.07 * s), u(0.0, 0.42)], w, col);
        }
        Icon::Join => {
            // a '#' for a code
            thick(p, vec![u(-0.12, -0.34), u(-0.20, 0.34)], w, col);
            thick(p, vec![u(0.20, -0.34), u(0.12, 0.34)], w, col);
            thick(p, vec![u(-0.34, -0.12), u(0.34, -0.12)], w, col);
            thick(p, vec![u(-0.34, 0.14), u(0.34, 0.14)], w, col);
        }
        Icon::Nearby => {
            p.circle_filled(u(0.0, 0.26), 0.06 * s, col);
            for r in [0.20, 0.36, 0.52] {
                thick(p, arc(u(0.0, 0.26), r * s, -PI * 0.78, -PI * 0.22, 10), w * 0.95, col);
            }
        }
        Icon::Medal => {
            // two ribbon tails, a disc and a star
            p.add(Shape::convex_polygon(vec![u(-0.26, -0.44), u(-0.08, -0.44), u(0.08, 0.04), u(-0.10, 0.04)], col.gamma_multiply(0.7), Stroke::NONE));
            p.add(Shape::convex_polygon(vec![u(0.26, -0.44), u(0.08, -0.44), u(-0.08, 0.04), u(0.10, 0.04)], col.gamma_multiply(0.7), Stroke::NONE));
            p.circle_filled(u(0.0, 0.16), 0.27 * s, col);
            p.circle_stroke(u(0.0, 0.16), 0.27 * s, Stroke::new(w * 0.5, col.gamma_multiply(0.5)));
            star(p, u(0.0, 0.16), 0.17 * s, theme_dark(), Stroke::NONE);
        }
        Icon::Egg => {
            p.add(Shape::convex_polygon((0..24).map(|i| {
                let a = i as f32 / 24.0 * TAU;
                let k = if a.sin() < 0.0 { 1.12 } else { 0.95 };
                u(a.cos() * 0.24, a.sin() * 0.32 * k)
            }).collect(), col, Stroke::NONE));
        }
    }
}

fn theme_dark() -> Color32 {
    crate::ui::theme::OUTLINE
}

fn theme_orange() -> Color32 {
    crate::ui::theme::ACCENT
}

fn theme_laser() -> Color32 {
    crate::ui::theme::LASER
}
