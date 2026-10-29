//! Check the real menu drawing at phone sizes, including cutouts and a live resize.
use super::*;
use crate::engine::Engine;
use crate::haptics::NoHaptics;
use crate::platform::{data_dir::Profile, os::{Orientation, RecordingPlatform}, Insets};
use egui::{Rect, Shape};

fn text_bounds(shape: &Shape, clip: Rect, safe: Rect, errors: &mut Vec<String>) {
    match shape {
        Shape::Vec(shapes) => for shape in shapes { text_bounds(shape, clip, safe, errors); },
        Shape::Text(t) if t.fallback_color.a() > 0 => {
            let rect = t.galley.rect.translate(t.pos.to_vec2());
            // Scrolled rows and animated toasts may cross the vertical clip edge.
            if rect.intersects(clip) && (!safe.expand(1.0).contains_rect(rect.intersect(clip))
                || rect.left() < safe.left() - 1.0 || rect.right() > safe.right() + 1.0) {
                errors.push(format!("text {:?} at {rect:?}, safe {safe:?}", t.galley.text()));
            }
        }
        _ => {}
    }
}

#[test]
fn rotatable_screens_keep_buttons_and_text_inside_the_safe_area_after_rotation() {
    let mut errors = Vec::new();
    crate::ui::widgets::BUTTON_RECTS.with(|r| *r.borrow_mut() = Some(Vec::new()));
    for &(name, make) in SCREENS {
        if name == "lobby_host" || name.starts_with("stage_") || name == "icons_dev" { continue; }
        let screen = make();
        if screen.orientation() != Orientation::Free { continue; }
        let mut e = Engine::new(screen, Profile::default(), std::env::temp_dir().join(format!("stampede-rotation-{}-{name}", std::process::id())),
            Options { shot: Some(name.into()), ..Default::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        // Keep the same screen instance while rotating both ways.
        for size in [(1080, 2400), (2400, 1080), (1080, 2400)] {
            e.insets = if size.0 < size.1 { Insets { top: 28.0, bottom: 12.0, ..Insets::ZERO } }
                else { Insets { left: 28.0, right: 12.0, ..Insets::ZERO } };
            let mut out = e.frame(0.0, size, 2.625, Default::default());
            for _ in 0..168 {
                crate::ui::widgets::BUTTON_RECTS.with(|r| r.borrow_mut().as_mut().unwrap().clear());
                out = e.frame(1.0 / 60.0, size, 2.625, Default::default());
            }
            let mut screen_errors = Vec::new();
            let safe = out.viewport.safe;
            crate::ui::widgets::BUTTON_RECTS.with(|r| {
                for (id, rect, clip) in r.borrow().as_ref().unwrap().iter() {
                    if rect.intersects(*clip) && (!rect.is_finite() || rect.width() <= 0.0 || rect.height() <= 0.0
                        || !safe.expand(1.0).contains_rect(rect.intersect(*clip))
                        || rect.left() < safe.left() - 1.0 || rect.right() > safe.right() + 1.0) {
                        screen_errors.push(format!("button {id} at {rect:?}, safe {safe:?}"));
                    }
                }
            });
            for s in out.egui.shapes { text_bounds(&s.shape, s.clip_rect, safe, &mut screen_errors); }
            errors.extend(screen_errors.into_iter().map(|s| format!("{name} {size:?}: {s}")));
        }
    }
    crate::ui::widgets::BUTTON_RECTS.with(|r| *r.borrow_mut() = None);
    errors.sort();
    errors.dedup();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn portrait_lists_scroll_to_the_last_card() {
    for (name, label) in [("challenges_sample", "Last Feather"), ("training_sample", "Coins & Items")] {
        let mut e = Engine::new(by_name(name).unwrap(), Profile::default(), std::env::temp_dir().join("stampede-scroll-check"),
            Options { shot: Some(name.into()), ..Default::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        let size = (1080, 2400);
        for _ in 0..120 { e.frame(1.0 / 60.0, size, 2.625, Default::default()); }
        let mut last = None;
        for _ in 0..20 {
            let raw = egui::RawInput { events: vec![
                egui::Event::PointerMoved(egui::pos2(200.0, 600.0)),
                egui::Event::MouseWheel { phase: egui::TouchPhase::Move, unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -90.0), modifiers: Default::default() },
            ], ..Default::default() };
            last = Some(e.frame(1.0 / 60.0, size, 2.625, raw));
        }
        fn visible(shape: &Shape, clip: Rect, label: &str) -> bool {
            match shape {
                Shape::Vec(v) => v.iter().any(|s| visible(s, clip, label)),
                Shape::Text(t) => t.galley.text() == label && clip.contains_rect(t.galley.rect.translate(t.pos.to_vec2())),
                _ => false,
            }
        }
        assert!(last.unwrap().egui.shapes.iter().any(|s| visible(&s.shape, s.clip_rect, label)), "{name}: last card {label} must be reachable by scrolling");
    }
}
