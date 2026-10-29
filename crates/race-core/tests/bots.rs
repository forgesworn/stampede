//! Bots lap every track (spec 8) and drive the training lessons (spec 9).

mod common;
use race_core::math::DT;
use race_core::*;

fn all_bot_race(id: TrackId, level: BotLevel, seed: u64) {
    // Compare driving consistency without random combat interruptions.
    // Full races with power-ups are exercised by headless/session tests.
    let mut cfg = RaceConfig::all_bots(id, 8, level, seed);
    cfg.mode = RaceMode::TimeTrial;
    let mut w = World::new(cfg);
    w.targets.clear();
    let mut drifts = 0;
    while w.phase != RacePhase::Finished && w.tick < 60 * 400 {
        w.step(&[]);
        drifts += w
            .drain_events()
            .iter()
            .filter(|e| matches!(e, Event::DriftStart { .. }))
            .count();
    }
    assert_eq!(
        w.phase,
        RacePhase::Finished,
        "{:?} {:?}: race did not finish",
        id,
        level
    );
    let mut best = f32::INFINITY;
    let mut worst = 0.0f32;
    for (i, laps) in w.lap_times.iter().enumerate() {
        assert_eq!(laps.len(), 3, "{:?}: racer {i} ran {} laps", id, laps.len());
        assert!(w.racers[i].finished_at.is_some());
        for &l in laps {
            best = best.min(l);
            worst = worst.max(l);
        }
    }
    assert!(
        worst <= 1.6 * best,
        "{:?} {:?}: worst lap {worst:.1} vs best {best:.1}",
        id,
        level
    );
    let stuck = w.max_stuck.iter().cloned().fold(0.0, f32::max);
    assert!(
        stuck <= 3.0,
        "{:?} {:?}: a racer was stuck {stuck:.1} s",
        id,
        level
    );
    // Savanna and Oasis are all wide sweepers (R > 59 m): nothing to drift.
    if !matches!(id, TrackId::Savanna | TrackId::Oasis) && level != BotLevel::Easy {
        assert!(
            drifts > 10,
            "{:?}: bots should attempt corner drifts ({drifts})",
            id
        );
    }
}

#[test]
fn all_bot_race_savanna() {
    all_bot_race(TrackId::Savanna, BotLevel::Normal, 1);
}

#[test]
fn all_bot_race_dune() {
    all_bot_race(TrackId::Dune, BotLevel::Normal, 2);
}

#[test]
fn all_bot_race_canyon() {
    all_bot_race(TrackId::Canyon, BotLevel::Normal, 3);
}

#[test]
fn all_bot_race_midnight() {
    all_bot_race(TrackId::Midnight, BotLevel::Normal, 4);
}

#[test]
fn all_bot_race_baobab() {
    all_bot_race(TrackId::Baobab, BotLevel::Normal, 5);
}

#[test]
fn all_bot_race_volcano() {
    all_bot_race(TrackId::Volcano, BotLevel::Normal, 6);
}

#[test]
fn all_bot_race_oasis() {
    all_bot_race(TrackId::Oasis, BotLevel::Normal, 7);
}

#[test]
fn all_bot_race_halving() {
    all_bot_race(TrackId::Halving, BotLevel::Normal, 8);
}

#[test]
fn easy_and_hard_bots_also_lap() {
    for id in TrackId::RACE_ALL {
        all_bot_race(id, BotLevel::Easy, 10);
        all_bot_race(id, BotLevel::Hard, 11);
    }
}

#[test]
fn bot_levels_are_ordered() {
    // Alone on the track (no combat): Hard is faster than Easy.
    let lap = |level| {
        let mut w = World::new(RaceConfig::all_bots(TrackId::Dune, 1, level, 5));
        while w.phase != RacePhase::Finished && w.tick < 60 * 300 {
            w.step(&[]);
        }
        w.racers[0].finished_at.unwrap()
    };
    let (e, h) = (lap(BotLevel::Easy), lap(BotLevel::Hard));
    assert!(h < e * 0.95, "hard {h:.1} easy {e:.1}");
}

#[test]
fn bot_input_is_pure() {
    let mut w = World::new(RaceConfig::all_bots(
        TrackId::Canyon,
        8,
        BotLevel::Normal,
        6,
    ));
    for _ in 0..1200 {
        w.step(&[]);
    }
    for i in 0..8 {
        assert_eq!(
            bot_input(&w, i, BotLevel::Normal),
            bot_input(&w, i, BotLevel::Normal)
        );
    }
}

fn lesson_run(id: LessonId, secs: f32) -> World {
    let mut w = World::new(RaceConfig::training(
        id,
        RacerKind::Bot(BotLevel::Normal),
        3,
    ));
    let mut progressed = false;
    while w.tick < (secs / DT) as u64 {
        w.step(&[]);
        progressed |= w
            .drain_events()
            .iter()
            .any(|e| matches!(e, Event::LessonProgress { .. }));
        if w.lesson.as_ref().unwrap().done {
            break;
        }
    }
    let ls = w.lesson.as_ref().unwrap();
    assert!(
        progressed && ls.progress > 0,
        "{:?}: counter never advanced",
        id
    );
    w
}

#[test]
fn lesson_1_run_and_steer_completes_with_a_bot() {
    let w = lesson_run(LessonId::RunSteer, 90.0);
    let ls = w.lesson.unwrap();
    assert!(ls.done, "progress {}/{}", ls.progress, ls.goal);
    assert!((1..=3).contains(&ls.stars));
    assert_eq!(w.phase, RacePhase::Finished);
}

#[test]
fn every_lesson_counter_advances() {
    for id in LessonId::ALL.into_iter().filter(|id| *id != LessonId::HeadBash) {
        let w = lesson_run(id, 90.0);
        let ls = w.lesson.unwrap();
        println!(
            "{:?}: {}/{} done {} in {:.1} s",
            id, ls.progress, ls.goal, ls.done, ls.time
        );
    }
}

#[test]
fn head_bash_lesson_adds_dummies() {
    let w = World::new(RaceConfig::training(
        LessonId::HeadBash,
        RacerKind::Human,
        1,
    ));
    assert_eq!(w.racers.len(), 4);
    assert!(w.cfg.racers[1..]
        .iter()
        .all(|r| r.kind == RacerKind::Bot(BotLevel::Dummy)));
}

#[test]
fn laser_lesson_fails_after_40_s_when_idle() {
    let mut w = World::new(RaceConfig::training(
        LessonId::LaserEyes,
        RacerKind::Human,
        1,
    ));
    let mut failed = false;
    for _ in 0..(46.0 / DT) as usize {
        w.step(&[]);
        failed |= w.drain_events().contains(&Event::LessonFailed);
    }
    assert!(failed && w.lesson.as_ref().unwrap().failed);
}
