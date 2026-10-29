#![allow(dead_code)]

use race_core::math::DT;
use race_core::*;

pub fn close(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

fn theme() -> Theme {
    TrackId::Savanna.def().theme
}

/// A long flat oval: 700 m straights, 120 m radius ends, 20 m road.
/// Features on the back straight (segment 2): a ramp at +200, a boost pad at +420.
pub fn proving_ground() -> Track {
    let segs = [
        Seg::S(700.0, 0.0),
        Seg::T(180.0, 120.0, 0.0),
        Seg::S(700.0, 0.0),
        Seg::T(180.0, 120.0, 0.0),
    ];
    let (points, g) = turtle(&segs, 20.0, 0.0);
    let features = vec![
        Feature::Ramp {
            s: g[2] + 200.0,
            len: 12.0,
            height: 1.5,
            lip: 1.0,
        },
        Feature::BoostPad {
            s: g[2] + 420.0,
            lateral: 0.0,
            len: 6.0,
        },
    ];
    Track::build(&TrackDef {
        id: TrackId::Training,
        name: "Proving Ground".into(),
        theme: theme(),
        laps: 3,
        points,
        shoulder: [4.0, 4.0],
        features,
        seed: 1,
    })
}

/// Arc length where the back straight of the proving ground starts.
pub fn pg_back_straight() -> f32 {
    700.0 + std::f32::consts::PI * 120.0
}

/// A huge circle (radius 400 m, 300 m wide road): room to drift without walls.
pub fn arena() -> Track {
    let segs = [Seg::T(180.0, 400.0, 0.0), Seg::T(180.0, 400.0, 0.0)];
    let (points, _) = turtle(&segs, 300.0, 0.0);
    Track::build(&TrackDef {
        id: TrackId::Training,
        name: "Arena".into(),
        theme: theme(),
        laps: 3,
        points,
        shoulder: [4.0, 4.0],
        features: vec![],
        seed: 2,
    })
}

/// A racer standing on `track` at arc length `s`, lateral offset `lat`, facing along the track.
pub fn racer_at(track: &Track, s: f32, lat: f32, character: u8) -> Racer {
    let sm = track.sample_at(s);
    let mut p = sm.pos + sm.right * lat;
    let pr = track.project(p, track.idx_at(s));
    p.y = pr.ground_y;
    let mut r = Racer::new(0, character, p, math::yaw_of(sm.tangent), pr.idx);
    r.s = pr.s;
    r.lateral = pr.lateral;
    r
}

/// Gives a racer a speed along its heading.
pub fn set_speed(r: &mut Racer, v: f32) {
    r.vel = r.forward() * v;
    r.speed = v;
}

/// Integrates `ticks` ticks with a constant input; returns the effects.
pub fn drive(
    r: &mut Racer,
    track: &Track,
    t: &Tunables,
    input: Input,
    ticks: usize,
) -> Vec<MotionFx> {
    (0..ticks).map(|_| r.integrate(&input, track, t)).collect()
}

pub fn secs(ticks: usize) -> f32 {
    ticks as f32 * DT
}

/// Steps a world until the countdown is over (all inputs idle).
pub fn skip_countdown(w: &mut World) {
    while w.phase == RacePhase::Countdown {
        w.step(&[]);
    }
    w.drain_events();
}

/// A race with `n` human seats (idle unless driven) on `track`.
pub fn humans(track: TrackId, n: usize, seed: u64) -> World {
    let racers = (0..n)
        .map(|i| RacerSetup {
            name: format!("P{i}"),
            character: (i % 8) as u8,
            kind: RacerKind::Human,
        })
        .collect();
    World::new(RaceConfig {
        track,
        laps: 3,
        seed,
        mode: RaceMode::Race,
        racers,
        drill: None,
        attire: [[0; 3]; 8],
        le_mans: false,
    })
}

/// Puts racer `i` at (s, lateral) facing along the track with speed `v`.
pub fn place(w: &mut World, i: usize, s: f32, lat: f32, v: f32) {
    let tr = w.track.clone();
    let sm = tr.sample_at(s);
    let mut p = sm.pos + sm.right * lat;
    let pr = tr.project(p, tr.idx_at(s));
    p.y = pr.ground_y;
    let r = &mut w.racers[i];
    r.pos = p;
    r.yaw = math::yaw_of(sm.tangent);
    r.visual_yaw = r.yaw;
    r.vel = r.forward() * v;
    r.speed = v;
    r.on_ground = true;
    r.track_idx = pr.idx as u32;
    r.s = pr.s;
    r.lateral = pr.lateral;
}

/// Steps with per-racer inputs from a closure, collecting events.
pub fn run<F: FnMut(&World) -> Vec<Input>>(w: &mut World, ticks: usize, mut f: F) -> Vec<Event> {
    let mut ev = Vec::new();
    for _ in 0..ticks {
        let inp = f(w);
        w.step(&inp);
        ev.extend(w.drain_events());
    }
    ev
}
