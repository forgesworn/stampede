//! The bad egg hits the lens; gameplay status stays above the splatter.

use crate::ui::theme;
use egui::{pos2, vec2, Align2, Color32, Painter, Rect, Shape, Stroke};
use race_core::playful::ItemNote;
use race_core::Racer;

/// What the rose seller says, spelt so.
const ROSE_LINE: &str = "Want a rose, fren?";
const ROSE_RED: Color32 = Color32::from_rgb(255, 92, 110);


pub fn draw(
    p: &Painter,
    safe: Rect,
    r: &Racer,
    scramble: f32,
    splat_duration: f32,
    phase: race_core::RacePhase,
    time: f32,
) {
    let e = r.effects;
    if e.splat > 0.0 {
        let age = (splat_duration - e.splat).max(0.0);
        let fade = (e.splat / 0.45).clamp(0.0, 1.0);
        let grow = (age / 0.16).clamp(0.0, 1.0);
        let radius = safe.height() * 0.24 * (0.5 + 0.5 * grow);
        let c = safe.center() + vec2(-safe.width() * 0.06, safe.height() * 0.015);
        let white = Color32::from_rgba_unmultiplied(255, 252, 232, (240.0 * fade) as u8);
        // A single wet silhouette avoids seams between translucent blobs.
        let mut mesh = egui::epaint::Mesh::default();
        let uv = egui::epaint::WHITE_UV;
        mesh.vertices.push(egui::epaint::Vertex {
            pos: c,
            uv,
            color: white,
        });
        let mut edge = Vec::new();
        for k in 0..96 {
            let a = k as f32 * std::f32::consts::TAU / 96.0;
            let r = radius * (1.0 + 0.12 * (a * 3.0).sin() + 0.075 * (a * 7.0 + 0.4).cos());
            let q = c + vec2(a.cos() * 1.18, a.sin() * 0.86) * r;
            edge.push(q);
            mesh.vertices.push(egui::epaint::Vertex {
                pos: q,
                uv,
                color: white,
            });
            mesh.indices.extend([0, k + 1, (k + 1) % 96 + 1]);
        }
        p.add(Shape::mesh(mesh));
        p.add(Shape::closed_line(edge, Stroke::new(1.0_f32, white)));
        let yolk = c + vec2(radius * 0.17, -radius * 0.02);
        p.circle_filled(
            yolk + vec2(2.0, 3.0),
            radius * 0.52,
            Color32::from_rgba_unmultiplied(210, 119, 7, (220.0 * fade) as u8),
        );
        p.circle_filled(
            yolk,
            radius * 0.48,
            Color32::from_rgba_unmultiplied(255, 190, 15, (255.0 * fade) as u8),
        );
        p.circle_filled(
            yolk + vec2(-radius * 0.12, -radius * 0.15),
            radius * 0.13,
            Color32::from_rgba_unmultiplied(255, 238, 145, (220.0 * fade) as u8),
        );
        // Shell flies away while the wet contents stick to the lens.
        if age < 0.6 {
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::TAU / 8.0;
                let d = vec2(a.cos(), a.sin());
                let q = c + d * radius * (1.0 + age * 5.0) + vec2(0.0, age * age * 140.0);
                let side = vec2(-d.y, d.x) * 13.0;
                p.add(Shape::convex_polygon(
                    vec![q + d * 19.0, q + side, q - side],
                    white,
                    Stroke::new(1.0_f32, Color32::from_white_alpha((180.0 * fade) as u8)),
                ));
            }
        }
    }
    // Seconds left on the timed items are counted down on the item button instead.
    let mut statuses: Vec<(String, Color32)> = Vec::new();
    if e.burn > 0.0 {
        statuses.push((
            format!("TAIL ON FIRE  {:.1}s", e.burn),
            Color32::from_rgb(255, 132, 45),
        ));
    }
    if e.pill > 0.0 {
        statuses.push(("INVINCIBLE".into(), theme::ACCENT));
    }
    if e.laptop > 0.0 {
        statuses.push((
            "BITCOIN HACK — RACE COINS ONLY".into(),
            Color32::from_rgb(120, 240, 160),
        ));
    }
    // Once per cake, over in 1.5 s.
    if e.cake_line(race_core::playful::Balance::default().cake_time) {
        statuses.push(("Cake time!".into(), Color32::from_rgb(255, 226, 186)));
    }
    if e.note_t > 0.0 {
        match e.note {
            ItemNote::Cake(took) => {
                let line = match took {
                    0 => "Cake! Nobody had anything: have a Feather".to_string(),
                    1 => "Cake! You took 1 power-up".to_string(),
                    n => format!("Cake! You took {n} power-ups"),
                };
                statuses.push((line, theme::ACCENT));
            }
            ItemNote::Rose => statuses.push(("Rose seller sent!".into(), ROSE_RED)),
            ItemNote::None => {}
        }
    }
    if e.dismount > 0.0 && phase != race_core::RacePhase::Countdown {
        let label = if e.le_mans {
            "RUN TO YOUR OSTRICH · JUMP TO MOUNT"
        } else {
            "CHASE YOUR OSTRICH · JUMP TO MOUNT"
        };
        statuses.push((label.into(), theme::ACCENT));
    }
    if scramble > 0.0 {
        statuses.push(("EGGS SCRAMBLED".into(), theme::ACCENT));
    }
    if e.tumble > 0.0 {
        let label = if e.tumble > 2.1 {
            "WHOOPS!"
        } else if e.tumble > 0.6 {
            "HA HA HA!"
        } else {
            "BACK ON OUR FEET!"
        };
        p.text(
            safe.center() + vec2(0.0, -safe.height() * 0.15 + (time * 12.0).sin() * 3.0),
            Align2::CENTER_CENTER,
            label,
            theme::title(30.0),
            Color32::from_rgb(190, 255, 120),
        );
    }
    if e.rose > 0.0 {
        // The seller's patter, over the road where it can't be missed.
        let c = safe.center() + vec2(0.0, -safe.height() * 0.3 + (time * 7.0).sin() * 2.0);
        let font = theme::title(26.0);
        let w = p.layout_no_wrap(ROSE_LINE.into(), font.clone(), Color32::WHITE).size().x;
        let bubble = Rect::from_center_size(c + vec2(18.0, 0.0), vec2(w + 78.0, 46.0));
        p.rect_filled(bubble, 23.0, Color32::from_black_alpha(175));
        p.rect_stroke(bubble, 23.0, Stroke::new(2.0_f32, ROSE_RED), egui::StrokeKind::Inside);
        super::icons::item(p, race_core::Item::Rose, pos2(bubble.left() + 28.0, c.y), 15.0, time);
        p.text(c + vec2(30.0, 0.0), Align2::CENTER_CENTER, ROSE_LINE, font, Color32::from_rgb(255, 236, 240));
    }
    for (k, (label, color)) in statuses.iter().enumerate() {
        let c = pos2(safe.center().x, safe.top() + 96.0 + k as f32 * 23.0);
        p.rect_filled(
            Rect::from_center_size(c, vec2(300.0_f32.min(safe.width() * 0.58), 21.0)),
            8.0,
            Color32::from_black_alpha(170),
        );
        p.text(
            c,
            Align2::CENTER_CENTER,
            label,
            theme::ui_medium(12.0),
            *color,
        );
    }
}
