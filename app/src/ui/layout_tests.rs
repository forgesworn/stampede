//! Every registered screen, at both phone sizes and with a camera-cutout inset,
//! must keep all of its text inside the safe area and survive a few seconds of
//! frames without panicking (headless: no GPU, silent audio).

use crate::cli::Options;
use crate::engine::Engine;
use crate::haptics::NoHaptics;
use crate::platform::data_dir::Profile;
use crate::platform::os::RecordingPlatform;
use crate::platform::Insets;
use crate::ui::screens;
use egui::epaint::Shape;
use egui::{pos2, Rect};

/// (pixel size, ui scale): OnePlus 8T and Pixel 8, held sideways.
const PHONES: [((u32, u32), f32); 2] = [((2400, 1080), 3.0), ((2400, 1080), 2.625)];

fn text_rects(shape: &Shape, out: &mut Vec<Rect>) {
    match shape {
        Shape::Vec(v) => v.iter().for_each(|s| text_rects(s, out)),
        Shape::Text(t) => {
            if t.galley.rows.iter().any(|r| !r.row.glyphs.is_empty()) && t.opacity_factor > 0.05 {
                let r = t.galley.rect;
                out.push(Rect::from_min_size(t.pos + r.min.to_vec2(), r.size()));
            }
        }
        _ => {}
    }
}

fn run(name: &str, size: (u32, u32), ppp: f32, insets: Insets, frames: usize) -> (Vec<(Rect, Rect)>, usize) {
    let opts = Options { shot: Some(name.into()), insets, ..Options::default() };
    let first = screens::by_name(name).expect("registered screen");
    let dir = std::env::temp_dir().join(format!("stampede-layout-{}-{name}", std::process::id()));
    let mut profile = Profile::default();
    let mut platform = RecordingPlatform::default();
    if matches!(name, "settings_tilt" | "race_tilt") {
        profile.steer_mode = crate::platform::data_dir::SteerMode::Tilt;
        platform.tilt = crate::platform::tilt::Reading::Sample { gravity: [0.0, 0.0, 9.81], rotation: 1, generation: 1, age: 0.01 };
    }
    let mut engine = Engine::new(first, profile, dir.clone(), opts, Box::new(platform), Box::new(NoHaptics::default()));
    let mut last = None;
    for _ in 0..frames {
        last = Some(engine.frame(1.0 / 60.0, size, ppp, egui::RawInput::default()));
    }
    let out = last.unwrap();
    let safe = out.viewport.safe;
    let mut bad = Vec::new();
    let mut count = 0;
    for cs in &out.egui.shapes {
        let mut rects = Vec::new();
        text_rects(&cs.shape, &mut rects);
        count += rects.len();
        for r in rects {
            // Text a clip rectangle hides entirely (a scrolled page's rows) is not on screen; only the visible part counts.
            let r = r.intersect(cs.clip_rect);
            if r.is_negative() || r.width() <= 0.0 || r.height() <= 0.0 {
                continue;
            }
            // outlined text spreads a few points past its box
            if !safe.expand(6.0).contains_rect(r) {
                bad.push((r, safe));
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    (bad, count)
}

#[test]
fn every_screen_keeps_its_text_inside_the_safe_area() {
    let mut failures = Vec::new();
    for name in screens::names() {
        // 3D stage previews (gameplay dev shots) are backdrops with no UI.
        if name.starts_with("stage_") || name.starts_with("playful_") {
            continue;
        }
        // The splash is a cinematic: its tagline and credit are checked, its fade has no text.
        for (size, ppp) in PHONES {
            for insets in [Insets::ZERO, Insets { left: 34.0, top: 0.0, right: 0.0, bottom: 0.0 }] {
                let (bad, count) = run(name, size, ppp, insets, 150);
                assert!(count > 0, "{name} drew no text at all");
                for (r, safe) in bad {
                    failures.push(format!("{name} @ {ppp} insets {:?}: text {:?} outside safe {:?}", insets.left, r, safe));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn staged_screens_are_registered() {
    for n in ["results_sample", "gp_standings_sample", "training_sample", "training_fresh_sample", "quick_race_training_sample", "time_trial_training_sample", "drill_slalom", "drill_ring", "drill_over", "lobby_sample", "settings_keypad", "gp_trophy_sample"] {
        assert!(screens::by_name(n).is_some(), "{n}");
    }
    let _ = pos2(0.0, 0.0);
}

#[test]
fn a_finished_online_player_can_follow_another_racer_by_touch_or_keyboard() {
    use crate::platform::KeyCode;
    for left_handed in [false, true] {
        let mut e = engine_for("race_following", Profile { left_handed, steer_mode: crate::platform::data_dir::SteerMode::Tilt, ..Profile::default() });
        let labels = |e: &mut Engine| {
            let out = e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
            fn collect(shape: &Shape, words: &mut Vec<String>) {
                match shape {
                    Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, words)),
                    Shape::Text(t) => words.push(t.galley.job.text.clone()),
                    _ => {}
                }
            }
            let mut words = Vec::new();
            for s in &out.egui.shapes { collect(&s.shape, &mut words); }
            words
        };
        frames(&mut e, 3);
        assert!(labels(&mut e).contains(&"FOLLOWING Nia".to_string()));
        assert!(!e.router.top().uses_tilt(), "watching does not run the steering sensor");
        // 800x360 UI points: the next button is at (544, 302).
        tap(&mut e, 1632.0, 906.0);
        assert!(labels(&mut e).contains(&"FOLLOWING Moss".to_string()));
        e.svc.keys.key(KeyCode::ArrowRight, true);
        assert!(labels(&mut e).contains(&"FOLLOWING Ash".to_string()));
        e.svc.keys.key(KeyCode::ArrowRight, false);
        frames(&mut e, 1);
        e.svc.keys.key(KeyCode::ArrowLeft, true);
        let words = labels(&mut e);
        assert!(words.contains(&"FOLLOWING Moss".to_string()));
        assert!(!words.iter().any(|w| matches!(w.as_str(), "DRIFT" | "JUMP" | "LASER" | "CENTRE")), "driving controls give way to following");
    }
}

#[test]
fn the_le_mans_start_waits_for_a_touch_jump_before_restoring_riding_controls() {
    fn words(e: &mut Engine) -> Vec<String> {
        fn collect(s: &Shape, out: &mut Vec<String>) {
            match s {
                Shape::Vec(v) => v.iter().for_each(|s| collect(s, out)),
                Shape::Text(t) => out.push(t.galley.job.text.clone()),
                _ => {}
            }
        }
        let frame = e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        let mut out = Vec::new();
        for s in &frame.egui.shapes {
            collect(&s.shape, &mut out);
        }
        out
    }
    for left_handed in [false, true] {
        let mut e = engine_for(
            "race_tilt",
            Profile {
                left_handed,
                ..Profile::default()
            },
        );
        frames(&mut e, 280);
        let before = words(&mut e);
        assert!(
            before.iter().any(|s| s.contains("JUMP TO MOUNT")),
            "the jockey waits beside the ostrich"
        );
        assert!(!before.iter().any(|s| s == "DRIFT"));
        // Jump takes the large thumb button while running, mirrored for left handers.
        tap(
            &mut e,
            if left_handed { 62.0 * 3.0 } else { 738.0 * 3.0 },
            294.0 * 3.0,
        );
        frames(&mut e, 30);
        let after = words(&mut e);
        assert!(!after.iter().any(|s| s.contains("JUMP TO MOUNT")));
        assert!(
            after.iter().any(|s| s == "DRIFT"),
            "mounting restores riding controls"
        );
        assert!(!after.iter().any(|s| s == "LASER"), "ordinary races use the laser power-up");
    }
}

fn engine_for(name: &str, profile: Profile) -> Engine {
    let opts = Options { shot: Some(name.into()), ..Options::default() };
    let first = screens::by_name(name).expect("registered screen");
    let dir = std::env::temp_dir().join(format!("stampede-interact-{}-{name}", std::process::id()));
    Engine::new(first, profile, dir, opts, Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()))
}

fn frames(e: &mut Engine, n: usize) {
    for _ in 0..n {
        e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
    }
}

fn tap(e: &mut Engine, x: f64, y: f64) {
    use crate::platform::TouchPhase;
    e.touch_event(7, TouchPhase::Started, (x, y));
    frames(e, 2);
    e.touch_event(7, TouchPhase::Ended, (x, y));
    frames(e, 2);
}

#[test]
fn title_tap_opens_the_menu_and_the_first_run_nudge_comes_first() {
    // returning player: straight to the menu
    let mut e = engine_for("home", Profile { seen_intro: true, ..Profile::default() });
    frames(&mut e, 40);
    tap(&mut e, 1200.0, 540.0);
    frames(&mut e, 30);
    assert_eq!(e.router.names(), ["home", "menu"]);
    // Back from the menu lands on a title that still answers taps
    e.request_back();
    frames(&mut e, 30);
    assert_eq!(e.router.names(), ["home"]);
    tap(&mut e, 1200.0, 540.0);
    frames(&mut e, 30);
    assert_eq!(e.router.names(), ["home", "menu"]);

    // first run: a nudge, then the menu with Training on top, and the flag is set
    let mut e = engine_for("home", Profile::default());
    frames(&mut e, 40);
    tap(&mut e, 1200.0, 540.0);
    frames(&mut e, 30);
    assert_eq!(e.router.names(), ["home"], "the nudge shows first");
    // "Start training" is the left button of the card centred on screen
    tap(&mut e, 900.0, 740.0);
    frames(&mut e, 60);
    assert!(e.svc.profile.seen_intro);
    assert_eq!(e.router.names(), ["home", "menu", "training"]);
}

#[test]
fn menu_buttons_open_their_screens() {
    let mut e = engine_for("menu", Profile::default());
    frames(&mut e, 60);
    // Race hero button: right column, top
    tap(&mut e, 1700.0, 300.0);
    frames(&mut e, 30);
    assert_eq!(e.router.names(), ["menu", "race_menu"]);
    e.request_back();
    frames(&mut e, 30);
    assert_eq!(e.router.names(), ["menu"], "system Back goes back one screen");
}

#[test]
fn settings_keypad_edits_the_name() {
    let mut e = engine_for("settings_keypad", Profile::default());
    frames(&mut e, 60);
    // Done is bottom centre-right; the sample opens with "Rider" typed
    tap(&mut e, 1560.0, 940.0);
    frames(&mut e, 10);
    assert_eq!(e.svc.profile.name, "Rider");
}

#[test]
fn after_race_routes_each_kind() {
    use crate::flow::RaceKind;
    use crate::ui::screens::after_race::{after_race, sample_outcome, SampleKind};
    use crate::ui::screens::Transition;
    let q = sample_outcome(SampleKind::Quick);
    assert!(matches!(after_race(q.clone()), Transition::Replace(_)));
    let mut quit = q.clone();
    quit.quit = true;
    assert!(matches!(after_race(quit), Transition::Pop));
    let mut t = q;
    t.launch.kind = RaceKind::Training(race_core::LessonId::Jump);
    // The lesson list is still under the race: pop back to it (no extra screen per lesson).
    assert!(matches!(after_race(t.clone()), Transition::Pop));
    t.launch.kind = RaceKind::Online;
    assert!(matches!(after_race(t), Transition::PopTo("multiplayer")));
    assert!(matches!(after_race(sample_outcome(SampleKind::Gp(1))), Transition::Replace(_)));
}

#[test]
fn a_lesson_result_handed_back_is_recorded_by_the_list_under_the_race() {
    use crate::flow::LessonResult;
    use race_core::LessonId;
    let mut e = engine_for("training", Profile { seen_intro: true, ..Profile::default() });
    frames(&mut e, 5);
    crate::ui::screens::training::hand_back(LessonResult { lesson: LessonId::Jump, done: true, stars: 2, time_s: 20.0 });
    frames(&mut e, 5);
    assert_eq!(e.svc.profile.lesson_stars[LessonId::ALL.iter().position(|l| *l == LessonId::Jump).unwrap()], 2);
    assert_eq!(e.router.names(), ["training"]);
}

#[test]
fn a_drill_result_saves_progress_and_pays_new_stars_only_once() {
    let mut e = engine_for("drill_over", Profile::default());
    frames(&mut e, 180);
    assert_eq!(e.svc.profile.drill_best[0], 7);
    assert_eq!(e.svc.profile.drill_stars[0], 2);
    assert_eq!(e.svc.profile.progress.sats, 30);
    frames(&mut e, 180);
    assert_eq!(e.svc.profile.progress.sats, 30, "results settle once per run");

    let profile = e.svc.profile.clone();
    let mut retry = engine_for("drill_over", profile);
    frames(&mut retry, 180);
    assert_eq!(retry.svc.profile.progress.sats, 30, "repeating stars never pays again");
    retry.request_back();
    frames(&mut retry, 30);
    assert_eq!(retry.svc.profile.drill_stars[0], 2, "Back keeps the earned stars");
}

#[test]
fn a_second_finger_can_tap_a_button_while_the_first_stays_down() {
    use crate::platform::TouchPhase;
    let mut e = engine_for("home", Profile { seen_intro: true, ..Profile::default() });
    frames(&mut e, 40);
    // A thumb rests on empty glass (the steering pad); the other finger taps the title.
    e.touch_event(3, TouchPhase::Started, (100.0, 900.0));
    frames(&mut e, 2);
    tap(&mut e, 1200.0, 540.0);
    frames(&mut e, 30);
    assert_eq!(e.router.names(), ["home", "menu"], "the second finger's tap reached egui");
}

#[test]
fn settings_with_many_relays_keeps_its_text_inside_the_safe_area() {
    let profile = Profile { relays: (0..32).map(|i| format!("wss://relay-number-{i}.example.com")).collect(), ..Profile::default() };
    for name in ["settings", "settings_relays"] {
        let opts = Options { shot: Some(name.into()), ..Options::default() };
        let first = screens::by_name(name).unwrap();
        let dir = std::env::temp_dir().join(format!("stampede-layout-many-{}-{name}", std::process::id()));
        let mut e = Engine::new(first, profile.clone(), dir.clone(), opts, Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        let mut last = None;
        for _ in 0..90 {
            last = Some(e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default()));
        }
        let out = last.unwrap();
        let safe = out.viewport.safe;
        let mut texts = Vec::new();
        for cs in &out.egui.shapes {
            let mut found = Vec::new();
            text_rects(&cs.shape, &mut found);
            texts.extend(found.into_iter().map(|r| r.intersect(cs.clip_rect)).filter(|r| r.width() > 0.0 && r.height() > 0.0));
        }
        assert!(!texts.is_empty());
        for r in texts {
            assert!(safe.expand(6.0).contains_rect(r), "{name}: text {r:?} outside {safe:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn the_home_update_pill_opens_settings_and_the_rest_of_the_screen_still_starts() {
    // The pill sits top right (points 704,24 on an 800x360 phone screen).
    let mut e = engine_for("home_update_sample", Profile { seen_intro: true, ..Profile::default() });
    frames(&mut e, 40);
    tap(&mut e, 2112.0, 72.0);
    frames(&mut e, 40);
    assert_eq!(e.router.names(), ["home", "settings"], "the pill opens Settings");
    let mut e = engine_for("home_update_sample", Profile { seen_intro: true, ..Profile::default() });
    frames(&mut e, 40);
    tap(&mut e, 1200.0, 540.0);
    frames(&mut e, 40);
    assert_eq!(e.router.names(), ["home", "menu"], "a tap anywhere else starts");
}

#[test]
fn staged_update_and_games_screens_are_registered() {
    for n in ["settings_relays", "settings_relay_keypad", "settings_updates_sample", "settings_downloading_sample", "more_games", "more_games_none_sample", "more_games_downloading_sample", "more_games_open_sample", "home_update_sample"] {
        assert!(screens::by_name(n).is_some(), "{n}");
    }
}

#[test]
fn main_menu_exit_saves_progress_and_uses_the_platform_exit_from_above_home() {
    use crate::platform::TouchPhase;
    use crate::ui::screens::Transition;
    for android in [false, true] {
        let mut e = engine_for("home", Profile { seen_intro: true, tilt_sensitivity: 1.4, ..Profile::default() });
        let dir = std::env::temp_dir().join(format!("stampede-menu-exit-{}-{android}", std::process::id()));
        e.svc.data_dir = dir.clone();
        e.svc.platform = Box::new(RecordingPlatform { leave_goes_on: android, ..Default::default() });
        e.router.apply(Transition::Push(screens::by_name("menu").unwrap()));
        frames(&mut e, 60);
        e.touch_event(7, TouchPhase::Started, (528.0, 1020.0));
        frames(&mut e, 2);
        e.touch_event(7, TouchPhase::Ended, (528.0, 1020.0));
        let out = e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        assert_eq!(e.svc.platform.recorded().unwrap().left, 1, "Exit must leave the app, even with Home below it");
        assert_eq!(out.quit, !android);
        let saved: Profile = serde_json::from_slice(&std::fs::read(dir.join("profile.json")).unwrap()).unwrap();
        assert_eq!(saved.tilt_sensitivity, 1.4);
        let _ = std::fs::remove_dir_all(dir);
    }
}
