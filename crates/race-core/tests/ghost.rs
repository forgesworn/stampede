//! Time Trial ghosts: size of a 3-lap run, exact round trip, rejection of
//! foreign files, and interpolation accuracy against the 60 Hz racer.

use race_core::math::DT;
use race_core::*;

/// A solo bot time trial on `track`; returns the ghost and every tick's
/// (race time, position, respawning) of the racer.
fn record(track: TrackId, level: BotLevel, seed: u64) -> (World, Ghost, Vec<(f32, Vec3, bool)>) {
    let mut cfg = RaceConfig::all_bots(track, 1, level, seed);
    cfg.mode = RaceMode::TimeTrial;
    let mut w = World::new(cfg);
    let mut rec = GhostRecorder::new(&w, 0);
    let mut truth = Vec::new();
    while w.phase != RacePhase::Finished && w.tick < 60 * 400 {
        w.step(&[]);
        rec.record(&w);
        if w.tick >= w.go_tick {
            let r = &w.racers[0];
            truth.push((w.race_time(), r.pos, r.respawn_t > 0.0));
        }
    }
    let g = rec.finish_world(&w).expect("finished");
    (w, g, truth)
}

#[test]
fn a_three_lap_ghost_is_small_and_round_trips_exactly() {
    for track in [TrackId::Halving, TrackId::Volcano] {
        let (w, g, _) = record(track, BotLevel::Normal, 7);
        let bytes = g.to_bytes();
        let per = bytes.len() as f32 / g.frames.len() as f32;
        eprintln!(
            "{track:?}: {:.1} s, {} frames, {} bytes ({per:.1} B/frame)",
            g.time,
            g.frames.len(),
            bytes.len()
        );
        assert!(bytes.len() < 60_000, "{track:?}: {} bytes", bytes.len());
        assert_eq!(&bytes[..4], &GHOST_MAGIC);
        assert_eq!(g.lap_times.len(), 3);
        assert_eq!(Some(g.time), w.racers[0].finished_at);
        assert!((g.duration() - g.time).abs() <= Ghost::INTERVAL + 1e-4);
        let back = Ghost::from_bytes(&bytes, track, &w.tun).expect("valid");
        assert_eq!(back, g);
    }
}

#[test]
fn foreign_or_broken_ghosts_are_rejected() {
    let (w, g, _) = record(TrackId::Savanna, BotLevel::Hard, 3);
    let bytes = g.to_bytes();
    assert_eq!(
        Ghost::from_bytes(&bytes, TrackId::Dune, &w.tun),
        Err(GhostError::WrongTrack)
    );
    let mut tun = w.tun.clone();
    tun.top_speed += 0.5;
    assert_eq!(
        Ghost::from_bytes(&bytes, TrackId::Savanna, &tun),
        Err(GhostError::TunablesChanged)
    );
    let mut moved = g.clone();
    moved.track_hash ^= 1;
    assert_eq!(
        Ghost::from_bytes(&moved.to_bytes(), TrackId::Savanna, &w.tun),
        Err(GhostError::TrackChanged)
    );
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert_eq!(Ghost::decode(&bad), Err(GhostError::BadMagic));
    let mut v2 = bytes.clone();
    v2[4] = 99;
    assert_eq!(Ghost::decode(&v2), Err(GhostError::Version(99)));
    assert_eq!(
        Ghost::decode(&bytes[..bytes.len() / 2]),
        Err(GhostError::Corrupt)
    );
    assert!(Ghost::decode(&[]).is_err());
    assert!(Ghost::decode(&[0xFF; 300]).is_err());
    // Hashes are stable and tell tracks apart.
    assert_eq!(track_hash(TrackId::Savanna), track_hash(TrackId::Savanna));
    assert_ne!(track_hash(TrackId::Savanna), track_hash(TrackId::Dune));
    assert_eq!(tunables_hash(&Tunables::default()), tunables_hash(&w.tun));
}

#[test]
fn poses_interpolate_close_to_the_real_racer() {
    let (_, g, truth) = record(TrackId::Canyon, BotLevel::Hard, 5);
    let mut worst = 0.0f32;
    let mut sum = 0.0f32;
    let mut n = 0;
    for (k, &(t, pos, respawning)) in truth.iter().enumerate() {
        if t > g.time || respawning {
            continue;
        }
        // Skip the frames around a respawn teleport.
        let near_respawn = truth[k.saturating_sub(6)..(k + 6).min(truth.len())]
            .iter()
            .any(|x| x.2);
        if near_respawn {
            continue;
        }
        let e = g.pose_at(t).pos.distance(pos);
        worst = worst.max(e);
        sum += e;
        n += 1;
    }
    let mean = sum / n as f32;
    eprintln!("ghost vs racer over {n} ticks: worst {worst:.3} m, mean {mean:.4} m");
    // The bot can run an Orange Pill (1.25 x top speed), where a sample gap covers more road.
    assert!(worst < 0.3, "worst {worst}");
    assert!(mean < 0.03, "mean {mean}");
    // Sample times are exact (cm quantisation only).
    let f = g.frames[100].pose();
    let at = g.pose_at(100.0 * Ghost::INTERVAL);
    assert!(at.pos.distance(f.pos) < 1e-3);
    // Yaw goes the short way round, held at the ends.
    let a = g.pose_at(-5.0);
    assert_eq!(a.pos, g.frames[0].pose().pos);
    let z = g.pose_at(g.time + 50.0);
    assert_eq!(z.pos, g.frames.last().unwrap().pose().pos);
    // Absurd times hold the ends too (no overflow in debug builds).
    for t in [f32::MAX, f32::INFINITY, f32::NAN, f32::MIN] {
        let p = g.pose_at(t).pos;
        assert!(p == g.frames.last().unwrap().pose().pos || p == g.frames[0].pose().pos, "{t}");
    }
    for k in 0..g.frames.len() - 1 {
        let (p, q) = (g.frames[k].pose(), g.frames[k + 1].pose());
        let mid = g.pose_at((k as f32 + 0.5) * Ghost::INTERVAL);
        let d1 = math::wrap_angle(mid.yaw - p.yaw).abs();
        let d2 = math::wrap_angle(q.yaw - p.yaw).abs();
        assert!(d1 <= d2 * 0.5 + 1e-3, "frame {k}: yaw jumped the long way");
        assert!((0.0..1.0).contains(&mid.gait_phase));
    }
    let _ = DT;
}

#[test]
fn a_recorder_fed_late_fills_missed_frames() {
    let mut cfg = RaceConfig::all_bots(TrackId::Training, 1, BotLevel::Normal, 1);
    cfg.mode = RaceMode::TimeTrial;
    let mut w = World::new(cfg);
    let mut rec = GhostRecorder::new(&w, 0);
    assert!(rec.is_empty());
    for _ in 0..(w.go_tick + 30) {
        w.step(&[]);
        if w.tick % 7 == 0 {
            rec.record(&w);
        }
    }
    rec.record(&w);
    assert_eq!(rec.len(), 30 / GHOST_EVERY as usize + 1);
    let g = rec.finish(0.5, vec![]);
    assert_eq!(g.frames.len(), 11);
}
