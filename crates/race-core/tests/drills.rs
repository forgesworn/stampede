//! Open-ground drills (spec 18.2): the pure `DrillState` core with a
//! kinematic test driver, then `World` smoke tests.

mod common;

use std::sync::Arc;

use race_core::drill::*;
use race_core::math::{forward, mix64, right, wrap_angle, yaw_of, DT};
use race_core::*;
use rand_pcg::Pcg32;

const TOP: f32 = 32.0;

fn rng_for(seed: u64, tick: u64) -> Pcg32 {
    Pcg32::new(mix64(seed ^ mix64(tick + 1)), 0xDA3E_39CB_94B9_5BDB)
}

fn p2(v: Vec3) -> Vec2 {
    Vec2::new(v.x, v.z)
}

// ---------------------------------------------------------------------------
// A kinematic driver: moves a runner (and the Bash Buddy dummy) by hand.

#[derive(Clone, Copy, PartialEq)]
enum Style {
    /// Does the task perfectly.
    Ideal,
    /// Stands still and ignores everything.
    Ignore,
}

struct Sim {
    style: Style,
    pos: Vec3,
    yaw: f32,
    orbit: Option<(f32, f32)>, // (angle round the pylon, swept)
    lock: f32,
    dummy: Option<(Vec3, f32)>, // pos, yaw
}

impl Sim {
    fn new(style: Style) -> Sim {
        Sim {
            style,
            pos: Vec3::new(150.0, 0.0, 0.0),
            yaw: std::f32::consts::FRAC_PI_2,
            orbit: None,
            lock: 0.0,
            dummy: None,
        }
    }

    fn apply_respawn(&mut self, ds: &DrillState) {
        if let Some((p, y)) = ds.dummy.and_then(|c| c.respawn) {
            self.dummy = Some((p, y));
        }
    }

    fn move_toward(&mut self, aim: Vec3, speed: f32) {
        let d = Vec3::new(aim.x - self.pos.x, 0.0, aim.z - self.pos.z);
        if d.length() > 1e-4 {
            self.yaw = yaw_of(d);
            self.pos += d.normalize() * (speed * DT).min(d.length().max(speed * DT));
        }
    }

    /// Advances the driver one tick and returns what the drill sees.
    fn view(&mut self, ds: &DrillState) -> RunnerView {
        let mut v = RunnerView::at(self.pos, self.yaw);
        // The dummy always runs (it is the drill's, not the runner's).
        if let (Some((dp, dy)), Some(cmd)) = (self.dummy.as_mut(), ds.dummy) {
            let to = cmd.goal - *dp;
            let err = wrap_angle(yaw_of(to) - *dy);
            *dy += err.clamp(-2.5 * DT, 2.5 * DT);
            *dp += forward(*dy) * cmd.speed_frac * TOP * DT;
        }
        let dummy_view = self.dummy.map(|(p, y)| DummyView {
            pos: p,
            vel: forward(y) * 20.0,
            yaw: y,
        });
        if self.style == Style::Ignore {
            v.dummy = dummy_view;
            return v;
        }
        match ds.current_rep() {
            Some(RepInfo::Gate { centre, yaw, .. }) => {
                // Square up 5 m before the gate, then go through its middle.
                let along = (self.pos - centre).dot(forward(yaw));
                let aim = if along < -5.2 {
                    centre - forward(yaw) * 5.0
                } else {
                    centre + forward(yaw) * 0.5
                };
                self.move_toward(aim, 15.0);
            }
            Some(RepInfo::Logs { yaw, remaining }) => {
                let aim = remaining[0] + forward(yaw) * 0.5;
                let near = (aim - self.pos).length() < 7.0;
                self.move_toward(aim, 15.0);
                v.air_h = if near { 1.0 } else { 0.0 };
                v.grounded = !near;
            }
            Some(RepInfo::Ring { centre, radius, .. }) => {
                let orbit_r = radius + 3.0;
                match self.orbit {
                    None => {
                        self.move_toward(centre, 14.0);
                        let rel = p2(self.pos) - p2(centre);
                        if rel.length() <= orbit_r {
                            self.orbit = Some((rel.x.atan2(rel.y), 0.0));
                        }
                    }
                    Some((ang, swept)) => {
                        let w = 14.0 / orbit_r * DT;
                        let (ang, swept) = (ang + w, swept + w);
                        self.pos = centre
                            + Vec3::new(ang.sin(), 0.0, ang.cos()) * orbit_r;
                        self.yaw = ang + std::f32::consts::FRAC_PI_2;
                        v.drifting = true;
                        v.spark_tier = 3;
                        if swept.to_degrees() >= 330.0 {
                            v.boost_released = Some(3);
                            v.drifting = false;
                            self.orbit = None;
                        } else {
                            self.orbit = Some((ang, swept));
                        }
                    }
                }
            }
            Some(RepInfo::Egg { pos: Some(p) }) => {
                let want = yaw_of(Vec3::new(p.x - self.pos.x, 0.0, p.z - self.pos.z));
                let err = wrap_angle(want - self.yaw);
                self.yaw += err.clamp(-5.0 * DT, 5.0 * DT);
                if err.abs() < 0.2 {
                    self.lock += DT;
                    if self.lock >= 0.22 {
                        v.egg_destroyed = true;
                        self.lock = 0.0;
                    }
                } else {
                    self.lock = 0.0;
                }
            }
            Some(RepInfo::Buddy) => {
                if let Some((dp, dy)) = self.dummy {
                    let aim = dp + right(dy) * 2.0;
                    let d = aim - self.pos;
                    let step = (d.length() * 20.0).min(90.0) * DT;
                    if d.length() > 1e-4 {
                        self.pos += d.normalize() * step.min(d.length());
                    }
                    self.yaw = dy;
                    // Bash when the dummy is in the real window.
                    let rel = dp - self.pos;
                    let lat = rel.dot(right(self.yaw)).abs();
                    let lon = rel.dot(forward(self.yaw)).abs();
                    if (0.6..=3.2).contains(&lat) && lon < 2.4 {
                        v.bash_hit = true;
                    }
                }
            }
            _ => {}
        }
        v.pos = self.pos;
        v.yaw = self.yaw;
        v.vel = forward(self.yaw) * 15.0;
        v.dummy = dummy_view;
        v
    }
}

struct Run {
    events: Vec<DrillEvent>,
    ds: DrillState,
    ticks: u64,
    min_r: f32,
    max_r: f32,
    dummy_min_r: f32,
    dummy_max_r: f32,
    props_final: Vec<DrillProp>,
}

fn run(id: DrillId, seed: u64, style: Style, max_secs: f32) -> Run {
    let mut sim = Sim::new(style);
    let mut ds = DrillState::new(id, Vec2::ZERO, sim.pos, sim.yaw, seed);
    sim.apply_respawn(&ds);
    let mut out = Run {
        events: Vec::new(),
        ds: ds.clone(),
        ticks: 0,
        min_r: f32::MAX,
        max_r: 0.0,
        dummy_min_r: f32::MAX,
        dummy_max_r: 0.0,
        props_final: Vec::new(),
    };
    let max_ticks = (max_secs / DT) as u64;
    while !ds.over && out.ticks < max_ticks {
        let view = sim.view(&ds);
        let mut rng = rng_for(seed, out.ticks);
        let ev = ds.step(&view, DT, &mut rng);
        sim.apply_respawn(&ds);
        out.events.extend(ev);
        out.ticks += 1;
        for p in ds.props() {
            let r = p2(p.pos).length();
            out.min_r = out.min_r.min(r);
            out.max_r = out.max_r.max(r);
        }
        if let Some(e) = ds.egg {
            let r = p2(e.pos).length();
            out.min_r = out.min_r.min(r);
            out.max_r = out.max_r.max(r);
        }
        if let Some((dp, _)) = sim.dummy {
            let r = p2(dp).length();
            out.dummy_min_r = out.dummy_min_r.min(r);
            out.dummy_max_r = out.dummy_max_r.max(r);
        }
    }
    out.props_final = ds.props();
    out.ds = ds;
    out
}

fn count(ev: &[DrillEvent], e: DrillEvent) -> usize {
    ev.iter().filter(|&&x| x == e).count()
}

// ---------------------------------------------------------------------------
// Pure core

#[test]
fn ideal_runner_reaches_master_on_every_drill() {
    for id in DrillId::ALL {
        let r = run(id, 7, Style::Ideal, 600.0);
        let ev = &r.events;
        assert!(r.ds.master, "{id:?}: no Master; events {ev:?}");
        assert!(r.ds.over, "{id:?}");
        assert_eq!(r.ds.strikes, 0, "{id:?}: {ev:?}");
        assert_eq!(count(ev, DrillEvent::RepClean), 40, "{id:?}: {ev:?}");
        assert_eq!(count(ev, DrillEvent::Strike), 0, "{id:?}");
        assert_eq!(r.ds.level, 10, "{id:?}");
        assert_eq!(r.ds.stars, 3, "{id:?}");
        assert_eq!(r.ds.best_level, 10, "{id:?}");
        // LevelUp(2..=10) in order; stars land right after L3, L6, L9.
        let ups: Vec<u8> = ev
            .iter()
            .filter_map(|e| match e {
                DrillEvent::LevelUp(n) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(ups, (2..=10).collect::<Vec<u8>>(), "{id:?}");
        for (lvl, star) in [(3u8, 1u8), (6, 2), (9, 3)] {
            let i = ev.iter().position(|e| *e == DrillEvent::LevelUp(lvl)).unwrap();
            assert_eq!(ev[i + 1], DrillEvent::Star(star), "{id:?}");
        }
        let n = ev.len();
        assert_eq!(&ev[n - 3..], &[DrillEvent::RepClean, DrillEvent::Master, DrillEvent::Over], "{id:?}");
    }
}

#[test]
fn runner_that_ignores_the_task_gets_three_strikes_and_over() {
    for id in DrillId::ALL {
        let r = run(id, 11, Style::Ignore, 300.0);
        let ev = &r.events;
        assert_eq!(
            *ev,
            vec![
                DrillEvent::Strike,
                DrillEvent::Strike,
                DrillEvent::Strike,
                DrillEvent::Over
            ],
            "{id:?}"
        );
        assert_eq!(r.ds.strikes, 3, "{id:?}");
        assert!(r.ds.over && !r.ds.master, "{id:?}");
        assert_eq!(r.ds.level, 1, "{id:?}");
        assert!(r.ds.elapsed < 90.0, "{id:?}: {}", r.ds.elapsed);
    }
}

#[test]
fn same_seed_gives_the_identical_event_log_and_props() {
    for id in DrillId::ALL {
        let a = run(id, 99, Style::Ideal, 600.0);
        let b = run(id, 99, Style::Ideal, 600.0);
        assert_eq!(a.events, b.events, "{id:?}");
        assert_eq!(a.ticks, b.ticks, "{id:?}");
        assert_eq!(a.ds, b.ds, "{id:?}");
        let c = run(id, 100, Style::Ignore, 300.0);
        let d = run(id, 101, Style::Ignore, 300.0);
        if matches!(id, DrillId::Slalom | DrillId::Hurdles | DrillId::DriftRing) {
            assert_ne!(c.props_final, d.props_final, "{id:?}: seeds should differ");
        }
    }
}

#[test]
fn props_stay_between_the_island_and_230_m() {
    for id in DrillId::ALL {
        for seed in 0..6u64 {
            let r = run(id, seed, Style::Ideal, 600.0);
            assert!(r.ds.master, "{id:?} seed {seed}");
            if r.min_r != f32::MAX {
                assert!(r.min_r >= FIELD_MIN_R, "{id:?} seed {seed}: prop at {} m", r.min_r);
                assert!(r.max_r <= FIELD_MAX_R, "{id:?} seed {seed}: prop at {} m", r.max_r);
            }
            if id == DrillId::BashBuddy {
                assert!(r.dummy_min_r >= FIELD_MIN_R && r.dummy_max_r <= FIELD_MAX_R,
                    "buddy seed {seed}: dummy {}..{}", r.dummy_min_r, r.dummy_max_r);
            }
            // Also after strikes (re-anchoring on the runner).
            let r = run(id, seed, Style::Ignore, 300.0);
            if r.min_r != f32::MAX {
                assert!(r.min_r >= FIELD_MIN_R && r.max_r <= FIELD_MAX_R, "{id:?} ignore seed {seed}");
            }
        }
    }
}

#[test]
fn only_the_current_rep_and_the_next_exist() {
    let r = run(DrillId::Slalom, 3, Style::Ideal, 600.0);
    assert!(r.ds.master);
    let ds = DrillState::new(DrillId::Slalom, Vec2::ZERO, Vec3::new(150.0, 0.0, 0.0), 1.5, 3);
    assert_eq!(ds.props().len(), 4, "two gates of two cones");
    let ds = DrillState::new(DrillId::DriftRing, Vec2::ZERO, Vec3::new(150.0, 0.0, 0.0), 1.5, 3);
    assert_eq!(ds.props().len(), 18, "two rings: pylon + 8 cones each");
    let ds = DrillState::new(DrillId::LaserRange, Vec2::ZERO, Vec3::new(150.0, 0.0, 0.0), 1.5, 3);
    assert!(ds.props().is_empty());
    assert!(ds.egg.is_none(), "the egg pops up after a beat");
}

#[test]
fn difficulty_interpolates_from_level_1_to_10() {
    let (a, z) = (Difficulty::at(1), Difficulty::at(10));
    assert_eq!((a.gate_width, z.gate_width), (9.0, 4.5));
    assert_eq!((a.side_offset, z.side_offset), (3.0, 5.5));
    assert_eq!((a.gate_spacing, z.gate_spacing), (32.0, 22.0));
    assert_eq!((a.ring_radius, z.ring_radius), (14.0, 6.0));
    assert_eq!((a.ring_time, z.ring_time), (12.0, 7.0));
    assert_eq!((a.hurdle_spacing, z.hurdle_spacing), (30.0, 14.0));
    assert_eq!((a.egg_angle_deg, z.egg_angle_deg), (10.0, 40.0));
    assert_eq!((a.egg_life, z.egg_life), (5.0, 1.8));
    assert_eq!((a.buddy_speed, z.buddy_speed), (0.6, 0.95));
    assert_eq!((a.buddy_time, z.buddy_time), (10.0, 5.0));
    for (l, t) in [(1, 1), (3, 1), (4, 2), (6, 2), (7, 3), (10, 3)] {
        assert_eq!(Difficulty::at(l).ring_tier, t, "L{l}");
    }
    assert!(!Difficulty::at(3).dodge && Difficulty::at(4).dodge);
    assert_eq!(Difficulty::at(4).double_chance, 0.0);
    assert!(Difficulty::at(5).double_chance > 0.0);
    for l in 1..10u8 {
        let (p, q) = (Difficulty::at(l), Difficulty::at(l + 1));
        assert!(q.gate_width < p.gate_width && q.gate_spacing < p.gate_spacing);
        assert!(q.ring_radius < p.ring_radius && q.egg_life < p.egg_life);
    }
}

#[test]
fn slalom_knocked_cone_and_outside_pass_are_strikes() {
    let start = Vec3::new(150.0, 0.0, 0.0);
    let mut ds = DrillState::new(DrillId::Slalom, Vec2::ZERO, start, 1.5, 5);
    let Some(RepInfo::Gate { yaw, .. }) = ds.current_rep() else {
        panic!("slalom has a gate");
    };
    // Drive through the left cone.
    let cone = ds.props()[0];
    let from = cone.pos - forward(yaw) * 3.0;
    let mut rng = rng_for(5, 0);
    ds.step(&RunnerView::at(from, yaw), DT, &mut rng);
    let ev = ds.step(&RunnerView::at(cone.pos, yaw), DT, &mut rng);
    assert_eq!(ev, vec![DrillEvent::Strike]);
    assert!(ds.props().iter().any(|p| p.knocked > 0.0), "the knocked cone stays visible");
    assert_eq!(ds.strikes, 1);
    // A fresh gate: pass outside it.
    let Some(RepInfo::Gate { centre, yaw, half_width: hw }) = ds.current_rep() else {
        panic!("replaced by a gate");
    };
    let out = centre + right(yaw) * (hw + CONE_RADIUS + 3.0);
    let ev1 = ds.step(&RunnerView::at(out - forward(yaw) * 0.5, yaw), DT, &mut rng);
    let ev2 = ds.step(&RunnerView::at(out + forward(yaw) * 0.5, yaw), DT, &mut rng);
    assert!(ev1.is_empty());
    assert_eq!(ev2, vec![DrillEvent::Strike]);
    assert_eq!(ds.strikes, 2);
}

#[test]
fn hurdle_on_the_ground_is_a_strike_with_a_stumble() {
    let start = Vec3::new(150.0, 0.0, 0.0);
    let mut ds = DrillState::new(DrillId::Hurdles, Vec2::ZERO, start, 1.5, 8);
    let Some(RepInfo::Logs { yaw, remaining }) = ds.current_rep() else {
        panic!("hurdles have a log");
    };
    let at = remaining[0];
    let mut rng = rng_for(8, 0);
    ds.step(&RunnerView::at(at - forward(yaw) * 0.4, yaw), DT, &mut rng);
    let ev = ds.step(&RunnerView::at(at + forward(yaw) * 0.4, yaw), DT, &mut rng);
    assert_eq!(ev, vec![DrillEvent::Strike]);
    assert!(ds.fx.stumble);
    // Round the end of the log is a strike too, with no stumble.
    let Some(RepInfo::Logs { yaw, remaining }) = ds.current_rep() else {
        panic!()
    };
    let end = remaining[0] + right(yaw) * (LOG_HALF_LENGTH + 3.0);
    ds.step(&RunnerView::at(end - forward(yaw) * 0.4, yaw), DT, &mut rng);
    let ev = ds.step(&RunnerView::at(end + forward(yaw) * 0.4, yaw), DT, &mut rng);
    assert_eq!(ev, vec![DrillEvent::Strike]);
    assert!(!ds.fx.stumble);
}

#[test]
fn drift_ring_needs_the_sweep_and_the_tier() {
    let start = Vec3::new(150.0, 0.0, 0.0);
    let orbit = |ds: &mut DrillState, tier_at_release: u8, degrees: f32| -> Vec<DrillEvent> {
        let Some(RepInfo::Ring { centre, radius, .. }) = ds.current_rep() else {
            panic!()
        };
        let r = radius + 3.0;
        let mut rng = rng_for(1, 0);
        let mut ev = Vec::new();
        let steps = (degrees.to_radians() / 0.02) as usize;
        for k in 0..steps {
            let a = k as f32 * 0.02;
            let mut v = RunnerView::at(centre + Vec3::new(a.sin(), 0.0, a.cos()) * r, a);
            v.drifting = true;
            v.spark_tier = 3;
            ev.extend(ds.step(&v, DT, &mut rng));
        }
        let a = steps as f32 * 0.02;
        let mut v = RunnerView::at(centre + Vec3::new(a.sin(), 0.0, a.cos()) * r, a);
        v.boost_released = Some(tier_at_release);
        ev.extend(ds.step(&v, DT, &mut rng));
        ev
    };
    // Too short a sweep: releasing even a top-tier turbo does nothing (no strike).
    let mut ds = DrillState::new(DrillId::DriftRing, Vec2::ZERO, start, 1.5, 2);
    assert!(orbit(&mut ds, 3, 200.0).is_empty());
    assert_eq!(ds.strikes, 0);
    // Full sweep, any tier at L1.
    let mut ds = DrillState::new(DrillId::DriftRing, Vec2::ZERO, start, 1.5, 2);
    assert_eq!(orbit(&mut ds, 1, 340.0), vec![DrillEvent::RepClean]);
    // At level 4 an orange tier is needed.
    let mut ds = DrillState::new(DrillId::DriftRing, Vec2::ZERO, start, 1.5, 2);
    ds.level = 4;
    assert_eq!(orbit(&mut ds, 1, 340.0), vec![DrillEvent::Strike]);
    let mut ds = DrillState::new(DrillId::DriftRing, Vec2::ZERO, start, 1.5, 2);
    ds.level = 7;
    assert_eq!(orbit(&mut ds, 2, 340.0), vec![DrillEvent::Strike]);
    let mut ds = DrillState::new(DrillId::DriftRing, Vec2::ZERO, start, 1.5, 2);
    ds.level = 7;
    assert_eq!(orbit(&mut ds, 3, 340.0), vec![DrillEvent::RepClean]);
    // Clipping a ring cone is a strike.
    let mut ds = DrillState::new(DrillId::DriftRing, Vec2::ZERO, start, 1.5, 2);
    let cone = ds.props()[1];
    let mut rng = rng_for(2, 0);
    ds.step(&RunnerView::at(cone.pos + Vec3::new(0.0, 0.0, -3.0), 0.0), DT, &mut rng);
    let ev = ds.step(&RunnerView::at(cone.pos, 0.0), DT, &mut rng);
    assert_eq!(ev, vec![DrillEvent::Strike]);
}

#[test]
fn state_roundtrips_through_json() {
    let r = run(DrillId::Hurdles, 4, Style::Ignore, 5.0);
    let s = serde_json::to_string(&r.ds).unwrap();
    let back: DrillState = serde_json::from_str(&s).unwrap();
    assert_eq!(back, r.ds);
}

#[test]
fn metadata_is_complete() {
    assert_eq!(DrillId::ALL.len(), 5);
    for id in DrillId::ALL {
        assert!(!id.title().is_empty() && !id.instruction().is_empty() && !id.hint().is_empty());
    }
    assert_eq!(DrillId::Slalom.control(), HintControl::Steer);
    assert_eq!(DrillId::DriftRing.control(), HintControl::Drift);
    assert_eq!(DrillId::Hurdles.control(), HintControl::Jump);
    assert_eq!(DrillId::LaserRange.control(), HintControl::Laser);
    assert_eq!(DrillId::BashBuddy.control(), HintControl::Bash);
}

// ---------------------------------------------------------------------------
// World level

/// A flat open field: a 24-point circle of radius 150 m, 270 m wide, centred on
/// the origin (the Paddock's shape, until `TrackId::Field` lands).
fn field_track() -> Track {
    let points = (0..24)
        .map(|k| {
            let a = k as f32 / 24.0 * std::f32::consts::TAU;
            TrackPoint {
                pos: Vec3::new(150.0 * a.cos(), 0.0, 150.0 * a.sin()),
                width: 270.0,
                bank: 0.0,
            }
        })
        .collect();
    Track::build(&TrackDef {
        id: TrackId::Training,
        name: "Field".into(),
        theme: TrackId::Savanna.def().theme,
        laps: 3,
        points,
        shoulder: [3.0, 3.0],
        features: vec![],
        seed: 9,
    })
}

fn put(w: &mut World, i: usize, pos: Vec3, yaw: f32, speed: f32) {
    let tr = w.track.clone();
    let pr = tr.project(pos, 0);
    // A full search: the hint walk may start far from the point.
    let mut best = pr;
    for h in (0..tr.samples.len()).step_by(7) {
        let c = tr.project(pos, h);
        if (c.lateral.abs()) < best.lateral.abs() {
            best = c;
        }
    }
    let r = &mut w.racers[i];
    r.pos = Vec3::new(pos.x, best.ground_y, pos.z);
    r.yaw = yaw;
    r.visual_yaw = yaw;
    r.vel = forward(yaw) * speed;
    r.speed = speed;
    r.on_ground = true;
    r.track_idx = best.idx as u32;
    r.s = best.s;
    r.lateral = best.lateral;
    r.ground_y = best.ground_y;
}

/// A drill world on the open field with the runner at the start of the countdown.
fn field_world(id: DrillId, seed: u64) -> World {
    let mut w = World::new(RaceConfig::drill(id, TrackId::DustBowl, seed));
    w.track = Arc::new(field_track());
    let (pos, yaw) = (Vec3::new(150.0, 0.0, 0.0), std::f32::consts::PI);
    put(&mut w, 0, pos, yaw, 0.0);
    let ds = DrillState::new(id, Vec2::ZERO, pos, yaw, mix64(seed));
    if let Some((p, y)) = ds.dummy.and_then(|c| c.respawn) {
        put(&mut w, 1, p, y, 0.0);
    }
    w.drill = Some(ds);
    w
}

fn drill_events(evs: &[Event]) -> Vec<DrillEvent> {
    evs.iter()
        .filter_map(|e| match e {
            Event::Drill(d) => Some(*d),
            _ => None,
        })
        .collect()
}

/// A simple autopilot for the drill's current rep.
fn pilot(w: &World, jump: bool) -> Vec<Input> {
    let r = &w.racers[0];
    let mut inp = Input::default();
    let steer_to = |aim: Vec3, r: &Racer| {
        let err = wrap_angle(yaw_of(Vec3::new(aim.x - r.pos.x, 0.0, aim.z - r.pos.z)) - r.yaw);
        (-err * 2.5).clamp(-1.0, 1.0)
    };
    let ds = w.drill.as_ref().unwrap();
    match ds.current_rep() {
        Some(RepInfo::Gate { centre, yaw, .. }) => {
            inp.steer = steer_to(centre + forward(yaw) * 0.5, r);
            inp.throttle = if r.speed > 11.0 { 0.0 } else { 1.0 };
        }
        Some(RepInfo::Logs { yaw, remaining }) => {
            let aim = remaining[0] + forward(yaw) * 2.0;
            inp.steer = steer_to(aim, r);
            inp.throttle = if r.speed > 12.0 { 0.0 } else { 1.0 };
            let d = (Vec3::new(aim.x - r.pos.x, 0.0, aim.z - r.pos.z)).length();
            inp.jump = jump && d < 4.6 && d > 1.0;
        }
        Some(RepInfo::Egg { pos: Some(p) }) => {
            inp.steer = steer_to(p, r);
            let error = wrap_angle(yaw_of(p - r.pos) - r.yaw);
            inp.steer = (-error * 6.0).clamp(-1.0, 1.0);
            inp.laser = (p - r.pos).dot(r.right()).abs() < 0.8; // aim the straight ray before spending heat
        }
        Some(RepInfo::Buddy) => {
            let d = &w.racers[1];
            let aim = d.pos + right(d.yaw) * -2.0 + forward(d.yaw) * 6.0;
            inp.steer = steer_to(aim, r);
            let rel = d.pos - r.pos;
            let lon = rel.dot(forward(r.yaw));
            inp.throttle = if r.speed < d.speed + (lon * 0.8).clamp(-4.0, 6.0) { 1.0 } else { 0.0 };
            let lat = rel.dot(right(r.yaw)).abs();
            inp.bash = (0.6..3.2).contains(&lat) && lon.abs() < 2.0 && w.tick % 2 == 0;
        }
        _ => {}
    }
    vec![inp]
}

fn play(w: &mut World, secs: f32, jump: bool, mut stop: impl FnMut(&[DrillEvent]) -> bool) -> Vec<Event> {
    common::skip_countdown(w);
    let mut all = Vec::new();
    for _ in 0..(secs / DT) as usize {
        let inp = pilot(w, jump);
        w.step(&inp);
        let evs = w.drain_events();
        let done = stop(&drill_events(&evs));
        all.extend(evs);
        if done || w.phase == RacePhase::Finished {
            break;
        }
    }
    all
}

#[test]
fn an_idle_runner_ends_every_drill_through_world_step() {
    for id in DrillId::ALL {
        let mut w = World::new(RaceConfig::drill(id, TrackId::DustBowl, 21));
        assert!(w.drill.is_some());
        common::skip_countdown(&mut w);
        let mut all = Vec::new();
        for _ in 0..(120.0 / DT) as usize {
            w.step(&[Input::default()]);
            all.extend(w.drain_events());
            if w.phase == RacePhase::Finished {
                break;
            }
        }
        let d = drill_events(&all);
        assert_eq!(
            d,
            vec![DrillEvent::Strike, DrillEvent::Strike, DrillEvent::Strike, DrillEvent::Over],
            "{id:?}"
        );
        assert_eq!(w.phase, RacePhase::Finished, "{id:?}");
        assert!(all.contains(&Event::RaceOver), "{id:?}");
        assert!(!all.iter().any(|e| matches!(e, Event::Lap { .. } | Event::Finished { .. })), "{id:?}: laps must not end a drill");
        assert_eq!(w.drill.as_ref().unwrap().strikes, 3);
    }
}

#[test]
fn laps_never_end_a_drill_run() {
    // Drive a full lap round the track with a drill live: no lap, no finish.
    let mut w = World::new(RaceConfig::drill(DrillId::Slalom, TrackId::Savanna, 2));
    common::skip_countdown(&mut w);
    let mut all = Vec::new();
    for _ in 0..(60.0 / DT) as usize {
        let inp = bot_input(&w, 0, BotLevel::Hard);
        // Keep the drill alive: it can't end by strikes in this test.
        w.drill.as_mut().unwrap().strikes = 0;
        w.step(&[inp]);
        all.extend(w.drain_events());
    }
    assert!(!all.iter().any(|e| matches!(e, Event::Lap { .. } | Event::FinalLap { .. } | Event::Finished { .. })));
    assert!(w.racers[0].finished_at.is_none());
}

#[test]
fn laser_range_world_rep_hits_dummy_opponents_with_the_forward_laser() {
    let mut w = field_world(DrillId::LaserRange, 5);
    let evs = play(&mut w, 60.0, false, |d| d.contains(&DrillEvent::LevelUp(2)));
    let d = drill_events(&evs);
    assert!(d.iter().filter(|e| **e == DrillEvent::RepClean).count() >= 4, "{d:?}");
    assert!(d.contains(&DrillEvent::LevelUp(2)), "{d:?}");
    assert!(evs.iter().any(|e| matches!(e, Event::LaserZap { by: 0, victim: 1 })));
    assert!(w.targets.iter().all(|t| !t.alive), "no laserable eggs in the drill");
    assert!(w.racers[0].item.is_none(), "drill eggs give no items");
}

#[test]
fn hurdles_world_jump_is_clean_and_a_trip_stumbles() {
    let mut w = field_world(DrillId::Hurdles, 6);
    let evs = play(&mut w, 40.0, true, |d| d.contains(&DrillEvent::RepClean));
    assert!(drill_events(&evs).contains(&DrillEvent::RepClean), "{:?}", drill_events(&evs));
    let mut w = field_world(DrillId::Hurdles, 6);
    let evs = play(&mut w, 40.0, false, |d| d.contains(&DrillEvent::Strike));
    assert!(drill_events(&evs).contains(&DrillEvent::Strike));
    assert!(evs.iter().any(|e| matches!(e, Event::Stumble { racer: 0 })), "a log hit stumbles");
    assert!(evs.iter().any(|e| matches!(e, Event::Hit { cause: HitCause::Hurdle, .. })));
}

#[test]
fn slalom_world_gates_pass_through_world_step() {
    let mut w = field_world(DrillId::Slalom, 12);
    let evs = play(&mut w, 60.0, false, |d| d.iter().filter(|e| **e == DrillEvent::RepClean).count() >= 3);
    let d = drill_events(&evs);
    assert!(d.iter().filter(|e| **e == DrillEvent::RepClean).count() >= 1, "{d:?}");
}

#[test]
fn legacy_bash_buddy_does_not_reenable_bashing() {
    let mut w = field_world(DrillId::BashBuddy, 14);
    assert_eq!(w.racers.len(), 2, "the dummy is appended");
    let evs = play(&mut w, 60.0, false, |d| d.contains(&DrillEvent::RepClean));
    let d = drill_events(&evs);
    assert!(!d.contains(&DrillEvent::RepClean), "retired bash drill cannot award clean reps");
    assert!(!evs.iter().any(|e| matches!(e, Event::Bash { .. } | Event::BashWhiff { .. })));
    // The dummy moved under the drill's control.
    assert!(w.racers[1].speed > 5.0);
}

#[test]
fn drill_world_is_deterministic() {
    let run_once = || {
        let mut w = field_world(DrillId::Slalom, 31);
        let evs = play(&mut w, 20.0, false, |_| false);
        (evs, w.racers[0].pos, w.drill.clone())
    };
    assert_eq!(run_once(), run_once());
}

#[test]
fn drill_config_serde_default_is_none() {
    let cfg = RaceConfig::quick(TrackId::Savanna, 1, BotLevel::Easy, 1);
    let mut v: serde_json::Value = serde_json::to_value(&cfg).unwrap();
    v.as_object_mut().unwrap().remove("drill");
    let back: RaceConfig = serde_json::from_value(v).unwrap();
    assert_eq!(back.drill, None);
    let d = RaceConfig::drill(DrillId::Hurdles, TrackId::DustBowl, 3);
    assert_eq!(d.drill, Some(DrillId::Hurdles));
    assert_eq!(d.racers.len(), 1);
}
