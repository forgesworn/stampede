//! Custom tracks (spec 19): shaping, auto-fill, the canonical encoding, the
//! `TrackId::Custom` registry, conversion to a track, bot races, the recorder.

use std::f32::consts::{PI, TAU};
use std::sync::{Arc, Mutex, MutexGuard};

use race_core::custom::{self, min_radius};
use race_core::math::flat;
use race_core::*;

use rand::{Rng, SeedableRng};
use rand_pcg::Pcg32;

/// The registry is process-wide and tests run in parallel threads.
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const W: f32 = 18.0;

// -- stroke generators -------------------------------------------------------

/// A closed ellipse stroke sampled every ~2 m, with seeded jitter, starting at angle 0.
fn ellipse(a: f32, b: f32, jitter: f32, seed: u64) -> Vec<Vec2> {
    let mut rng = Pcg32::seed_from_u64(seed);
    let perim = TAU * ((a * a + b * b) * 0.5).sqrt();
    let n = (perim / 2.0) as usize;
    (0..=n)
        .map(|i| {
            let t = TAU * i as f32 / n as f32;
            let j = |rng: &mut Pcg32| rng.gen_range(-jitter..=jitter);
            Vec2::new(a * t.cos() + j(&mut rng), b * t.sin() + j(&mut rng))
        })
        .collect()
}

/// A lumpy closed loop (radius wobbles with angle) like a hand-drawn track.
fn blob(r: f32, seed: u64) -> Vec<Vec2> {
    let mut rng = Pcg32::seed_from_u64(seed);
    let (p1, p2) = (rng.gen_range(0.0..TAU), rng.gen_range(0.0..TAU));
    let n = (TAU * r / 2.0) as usize;
    (0..=n)
        .map(|i| {
            let t = TAU * i as f32 / n as f32;
            let rr = r * (1.0 + 0.22 * (2.0 * t + p1).sin() + 0.12 * (3.0 * t + p2).sin());
            Vec2::new(rr * t.cos() + rng.gen_range(-1.0..=1.0), rr * t.sin() + rng.gen_range(-1.0..=1.0))
        })
        .collect()
}

/// A figure eight (lemniscate-like) stroke, `t0` radians into the parameter.
fn eight_stroke(a: f32, b: f32, t0: f32) -> Vec<Vec2> {
    let n = 500;
    (0..=n)
        .map(|i| {
            let t = t0 + TAU * i as f32 / n as f32;
            Vec2::new(a * t.sin(), b * t.sin() * t.cos())
        })
        .collect()
}

/// A stadium: two straights joined by semicircles, sampled every 2 m.
fn stadium(straight: f32, r: f32) -> Vec<Vec2> {
    let mut v = Vec::new();
    let mut x = 0.0;
    while x < straight {
        v.push(Vec2::new(x, 0.0));
        x += 2.0;
    }
    let arc = PI * r;
    let mut s = 0.0;
    while s < arc {
        let a = s / r;
        v.push(Vec2::new(straight + r * a.sin(), r - r * a.cos()));
        s += 2.0;
    }
    let mut x = straight;
    while x > 0.0 {
        v.push(Vec2::new(x, 2.0 * r));
        x -= 2.0;
    }
    let mut s = 0.0;
    while s < arc {
        let a = s / r;
        v.push(Vec2::new(-r * a.sin(), 2.0 * r - r + r * a.cos()));
        s += 2.0;
    }
    v.push(v[0]);
    v
}

fn assert_valid(sh: &Shaped) {
    let n = sh.points.len();
    assert!(n <= 600, "{n} points");
    assert!(sh.length >= 300.0 && sh.length <= 3000.0, "length {}", sh.length);
    let r = min_radius(&sh.points);
    assert!(r >= 14.0 - 1e-3, "min radius {r}");
    for i in 0..n {
        let d = sh.points[i].distance(sh.points[(i + 1) % n]);
        assert!((4.5..=5.5).contains(&d), "spacing {d} at {i}");
        // Everything is on the decimetre grid.
        let q = sh.points[i] * 10.0;
        assert!((q.x - q.x.round()).abs() < 1e-2 && (q.y - q.y.round()).abs() < 1e-2);
    }
}

fn save(sh: &Shaped, features: Vec<CustomFeature>) -> CustomTrack {
    custom_track_from(sh, "Test Loop", TrackId::Savanna, 2, 180, features, 7)
}

// -- shaping -------------------------------------------------------------------

#[test]
fn noisy_loop_shapes_into_a_valid_track() {
    for (seed, a, b) in [(1u64, 140.0, 90.0), (2, 100.0, 100.0), (3, 180.0, 60.0)] {
        let sh = shape(&ellipse(a, b, 1.5, seed), false, W).unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
        assert_valid(&sh);
        // Close to the drawn shape: length within 6 % of the ellipse perimeter.
        let perim = TAU * ((a * a + b * b) * 0.5).sqrt();
        assert!((sh.length - perim).abs() < 0.06 * perim, "length {} vs {}", sh.length, perim);
        // The start is near the stroke start (a, 0) and heads along the stroke (+y).
        assert!(sh.points[0].distance(Vec2::new(a, 0.0)) < 6.0, "start {:?}", sh.points[0]);
        let d = (sh.points[1] - sh.points[0]).normalize();
        assert!(d.dot(Vec2::Y) > 0.8, "start direction {d:?}");
        assert!(sh.crossings.is_empty());
    }
}

#[test]
fn lumpy_hand_drawn_loops_shape() {
    for seed in 0..12u64 {
        let sh = shape(&blob(110.0, seed), false, W).unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
        assert_valid(&sh);
    }
}

#[test]
fn tight_corners_are_widened_to_14_m() {
    // A square with sharp corners: every corner must be rounded to >= 14 m.
    let mut v = Vec::new();
    let pts = [(0.0, 0.0), (160.0, 0.0), (160.0, 160.0), (0.0, 160.0), (0.0, 0.0)];
    for w in pts.windows(2) {
        let (a, b) = (Vec2::new(w[0].0, w[0].1), Vec2::new(w[1].0, w[1].1));
        let n = (a.distance(b) / 2.0) as usize;
        for i in 0..n {
            v.push(a.lerp(b, i as f32 / n as f32));
        }
    }
    v.push(Vec2::ZERO);
    let sh = shape(&v, false, W).expect("a square shapes");
    assert_valid(&sh);
    assert_eq!(sh.corners.len(), 4, "{:?}", sh.corners);
    assert!(sh.corners.iter().all(|c| c.radius >= 12.0));
    assert!(sh.straights.len() == 4, "{:?}", sh.straights);
}

#[test]
fn open_stroke_closes_smoothly_with_auto_close() {
    // 300 degrees of an ellipse: the ends are ~100 m apart.
    let mut v = ellipse(150.0, 100.0, 0.6, 5);
    v.truncate(v.len() * 5 / 6);
    let gap = v[0].distance(*v.last().unwrap());
    assert!(gap > 60.0, "gap {gap}");
    match shape(&v, false, W) {
        Err(ShapeError::NotClosed { gap: g }) => assert!((g - gap).abs() < 1.0),
        other => panic!("expected NotClosed, got {other:?}"),
    }
    let sh = shape(&v, true, W).expect("auto close");
    assert_valid(&sh);
    // No corner under 14 m anywhere, in particular around the join.
    let join = (v[0] + *v.last().unwrap()) * 0.5;
    let near_join: Vec<f32> = (0..sh.points.len())
        .filter(|&i| sh.points[i].distance(join) < 40.0)
        .map(|i| {
            let n = sh.points.len();
            min_radius(&[sh.points[(i + n - 1) % n], sh.points[i], sh.points[(i + 1) % n]])
        })
        .collect();
    assert!(!near_join.is_empty());
    assert!(near_join.iter().all(|&r| r >= 14.0 - 1e-3), "{near_join:?}");
}

#[test]
fn small_gap_is_closed_without_auto_close() {
    let mut v = ellipse(120.0, 80.0, 0.5, 9);
    v.truncate(v.len() - 4); // ends ~8 m apart
    let sh = shape(&v, false, W).expect("a small gap closes itself");
    assert_valid(&sh);
}

#[test]
fn overshooting_the_start_is_trimmed() {
    let mut v = ellipse(130.0, 90.0, 0.4, 11);
    // Carry on past the start for 25 m.
    let extra: Vec<Vec2> = ellipse(130.0, 90.0, 0.4, 11).into_iter().skip(1).take(12).collect();
    v.extend(extra);
    let sh = shape(&v, false, W).expect("overshoot trims");
    assert_valid(&sh);
    assert!(sh.crossings.is_empty());
}

#[test]
fn figure_eight_has_one_crossing_and_builds_one_bridge() {
    for t0 in [0.0f32, 0.9, 2.2] {
        let sh = shape(&eight_stroke(150.0, 100.0, t0), false, W).unwrap_or_else(|e| panic!("t0 {t0}: {e:?}"));
        assert_valid(&sh);
        assert_eq!(sh.crossings.len(), 1, "t0 {t0}");
        let x = sh.crossings[0];
        assert!(x.angle_deg >= 25.0);
        assert!(x.pos.length() < 8.0, "the crossing is at the origin: {:?}", x.pos);
        let ct = save(&sh, vec![]);
        let def = to_def(&ct).expect("to_def");
        let tr = Track::build(&def);
        assert_eq!(tr.crossings.len(), 1, "t0 {t0}");
        assert_eq!(tr.bridges.len(), 1);
        let c = tr.crossings[0];
        assert!(c.gap() >= 6.5, "gap {}", c.gap());
        assert!((c.w_low - 18.0).abs() < 0.01);
        let n = tr.n();
        let grade = (0..n)
            .map(|i| {
                let (a, b) = (&tr.samples[i], &tr.samples[(i + 1) % n]);
                (b.pos.y - a.pos.y).abs() / flat(b.pos - a.pos).length().max(1e-3)
            })
            .fold(0.0, f32::max);
        assert!(grade <= 0.15, "max grade {grade}");
        // The rising branch is the one shape reported.
        assert!(tr.s_delta(c.s_high, x.s_high).abs() < 8.0, "{} vs {}", c.s_high, x.s_high);
        // The start line is flat.
        assert!(tr.samples[0].pos.y < 0.01);
        // Heights are flat apart from the bridge.
        assert!(tr.samples.iter().filter(|s| s.pos.y > 0.01).count() as f32 * tr.ds < 2.0 * 85.0);
    }
}

#[test]
fn two_crossings_make_two_separate_bridges() {
    // A three-lobe chain (Lissajous 3:1) has two crossings.
    let n = 1500;
    let v: Vec<Vec2> = (0..=n)
        .map(|i| {
            let t = TAU * i as f32 / n as f32;
            Vec2::new(90.0 * (3.0 * t + 1.5707).sin(), 230.0 * t.sin())
        })
        .collect();
    let sh = shape(&v, false, W).expect("two crossings");
    assert_valid(&sh);
    assert_eq!(sh.crossings.len(), 2);
    let tr = Track::build(&to_def(&save(&sh, vec![])).unwrap());
    assert_eq!(tr.crossings.len(), 2);
    assert_eq!(tr.bridges.len(), 2);
    assert!(tr.crossings.iter().all(|c| c.gap() >= 6.5));
    // Each bridge rises on the branch shape named.
    for x in &sh.crossings {
        assert!(tr.crossings.iter().any(|c| tr.s_delta(c.s_high, x.s_high).abs() < 8.0), "{x:?}");
    }
    let mut a = auto_fill(&sh, 5);
    check_spacing(&sh, &a);
    a.truncate(10);
    assert_eq!(Track::build(&to_def(&save(&sh, a)).unwrap()).crossings.len(), 2);
}

#[test]
fn three_crossings_are_rejected() {
    // A trefoil has three crossings.
    let n = 900;
    let v: Vec<Vec2> = (0..=n)
        .map(|i| {
            let t = TAU * i as f32 / n as f32;
            Vec2::new(t.sin() + 2.0 * (2.0 * t).sin(), t.cos() - 2.0 * (2.0 * t).cos()) * 55.0
        })
        .collect();
    match shape(&v, false, W) {
        Err(ShapeError::TooManyCrossings { at }) => assert!(at.length() < 400.0),
        other => panic!("expected TooManyCrossings, got {other:?}"),
    }
}

#[test]
fn a_shallow_crossing_is_rejected() {
    // Two long laps that cross at ~12 degrees.
    let a = 170.0f32;
    let n = 600;
    let v: Vec<Vec2> = (0..=n)
        .map(|i| {
            let t = TAU * i as f32 / n as f32;
            Vec2::new(a * t.sin(), 22.0 * t.sin() * t.cos())
        })
        .collect();
    match shape(&v, false, W) {
        Err(ShapeError::ShallowCrossing { at }) => assert!(at.length() < 15.0, "{at:?}"),
        other => panic!("expected ShallowCrossing, got {other:?}"),
    }
}

#[test]
fn a_hairpin_pair_ten_metres_apart_is_too_close() {
    let mut v = Vec::new();
    let mut x = 0.0;
    while x <= 300.0 {
        v.push(Vec2::new(x, 0.0));
        x += 2.0;
    }
    let mut s = 0.0;
    while s < PI * 5.0 {
        let a = s / 5.0;
        v.push(Vec2::new(300.0 + 5.0 * a.sin(), 5.0 - 5.0 * a.cos()));
        s += 1.0;
    }
    let mut x = 300.0;
    while x >= 0.0 {
        v.push(Vec2::new(x, 10.0));
        x -= 2.0;
    }
    match shape(&v, false, W) {
        Err(ShapeError::TooClose { at }) => {
            assert!(at.y > -2.0 && at.y < 12.0, "{at:?}");
        }
        other => panic!("expected TooClose, got {other:?}"),
    }
}

#[test]
fn the_lenient_shaper_keeps_a_closed_loop_that_breaks_a_rule() {
    // The same hairpin pair: strict shaping refuses it, lenient keeps the loop and names the fault.
    let mut v = Vec::new();
    let mut x = 0.0;
    while x <= 300.0 {
        v.push(Vec2::new(x, 0.0));
        x += 2.0;
    }
    let mut s = 0.0;
    while s < PI * 5.0 {
        let a = s / 5.0;
        v.push(Vec2::new(300.0 + 5.0 * a.sin(), 5.0 - 5.0 * a.cos()));
        s += 1.0;
    }
    let mut x = 300.0;
    while x >= 0.0 {
        v.push(Vec2::new(x, 10.0));
        x -= 2.0;
    }
    let strict = shape(&v, false, W).unwrap_err();
    let (loop_, fault) = custom::shape_lenient(&v, false, W).expect("a closed loop comes back");
    assert_eq!(fault, Some(strict));
    assert!(loop_.points.len() > 100 && loop_.length > 600.0);
    assert!(loop_.crossings.is_empty());
    // It is a closed ring at the usual spacing.
    let n = loop_.points.len();
    assert!((0..n).all(|i| loop_.points[i].distance(loop_.points[(i + 1) % n]) < 8.0));
}

#[test]
fn the_lenient_shaper_matches_strict_for_valid_loops_and_fails_when_there_is_no_loop() {
    for seed in 0..4 {
        let v = blob(120.0, seed);
        let strict = shape(&v, false, W).unwrap();
        let (loop_, fault) = custom::shape_lenient(&v, false, W).unwrap();
        assert_eq!(fault, None);
        assert_eq!(loop_, strict);
    }
    // An open line, a short loop and a stub carry no loop.
    let open: Vec<Vec2> = (0..100).map(|i| Vec2::new(i as f32 * 4.0, 0.0)).collect();
    assert!(matches!(custom::shape_lenient(&open, false, W), Err(ShapeError::NotClosed { .. })));
    let r = 200.0 / TAU;
    let small: Vec<Vec2> = (0..=100).map(|i| Vec2::new(r * (TAU * i as f32 / 100.0).cos(), r * (TAU * i as f32 / 100.0).sin())).collect();
    assert!(matches!(custom::shape_lenient(&small, false, W), Err(ShapeError::TooShort { .. })));
    assert!(matches!(custom::shape_lenient(&[Vec2::ZERO, Vec2::new(1.0, 0.0)], false, W), Err(ShapeError::TooShort { .. })));
}

#[test]
fn a_200_m_loop_is_too_short() {
    let r = 200.0 / TAU;
    let n = 100;
    let v: Vec<Vec2> = (0..=n).map(|i| Vec2::new(r * (TAU * i as f32 / n as f32).cos(), r * (TAU * i as f32 / n as f32).sin())).collect();
    match shape(&v, false, W) {
        Err(ShapeError::TooShort { len }) => assert!((len - 200.0).abs() < 10.0, "{len}"),
        other => panic!("expected TooShort, got {other:?}"),
    }
    // Something absurdly long is rejected too.
    match shape(&ellipse(900.0, 700.0, 0.0, 1), false, W) {
        Err(ShapeError::TooLong { .. }) => {}
        other => panic!("expected TooLong, got {other:?}"),
    }
}

#[test]
fn a_tiny_scribble_loop_is_cut_out() {
    // An ellipse with a small curl (a loop under 60 m) drawn on it.
    let mut v = ellipse(150.0, 100.0, 0.0, 3);
    let at = v.len() / 8;
    let base = v[at];
    let dir = (v[at + 1] - v[at]).normalize();
    let norm = Vec2::new(-dir.y, dir.x);
    let curl: Vec<Vec2> = (1..=20)
        .map(|i| {
            let t = TAU * i as f32 / 20.0;
            base + dir * (8.0 * t.sin() + 4.0 * t / TAU * 8.0) + norm * (8.0 * (1.0 - t.cos()))
        })
        .collect();
    v.splice(at + 1..at + 1, curl);
    let sh = shape(&v, false, W).expect("the curl is cut");
    assert_valid(&sh);
    assert!(sh.crossings.is_empty(), "{:?}", sh.crossings);
}

#[test]
fn garbage_input_never_panics() {
    let mut rng = Pcg32::seed_from_u64(99);
    for k in 0..40 {
        let n = rng.gen_range(0..300usize);
        let scale = [1.0f32, 50.0, 400.0, 5000.0][k % 4];
        let v: Vec<Vec2> = (0..n).map(|_| Vec2::new(rng.gen_range(-scale..scale), rng.gen_range(-scale..scale))).collect();
        let _ = shape(&v, k % 2 == 0, W);
    }
    assert!(shape(&[], true, W).is_err());
    assert!(shape(&[Vec2::NAN; 20], true, W).is_err());
    assert!(shape(&[Vec2::ZERO; 50], true, W).is_err());
}

#[test]
fn restart_moves_and_flips_the_start() {
    let sh = shape(&ellipse(140.0, 90.0, 0.5, 4), false, W).unwrap();
    let n = sh.points.len();
    let r = sh.restart(n / 3, false).unwrap();
    assert_eq!(r.points[0], sh.points[n / 3]);
    assert_eq!(r.points[1], sh.points[n / 3 + 1]);
    let f = sh.restart(n / 3, true).unwrap();
    assert_eq!(f.points[0], sh.points[n / 3]);
    assert_eq!(f.points[1], sh.points[n / 3 - 1]);
    assert_valid(&f);
}

// -- auto-fill and placement ---------------------------------------------------------

fn cyc(a: f32, b: f32, len: f32) -> f32 {
    let d = (a - b).rem_euclid(len);
    d.min(len - d)
}

fn check_spacing(sh: &Shaped, fs: &[CustomFeature]) {
    for (i, f) in fs.iter().enumerate() {
        let (s, kind) = (f.s(), f.kind);
        assert!(cyc(s, 0.0, sh.length) >= 30.0, "{kind:?} at {s} too close to the start");
        for (j, g) in fs.iter().enumerate() {
            if i != j {
                assert!(cyc(s, g.s(), sh.length) >= 40.0, "{kind:?} at {s} and {:?} at {}", g.kind, g.s());
            }
        }
        for x in &sh.crossings {
            assert!(cyc(s, x.s_high, sh.length) >= custom::BRIDGE_REACH + 30.0 - 1.0, "{kind:?} at {s} on a ramp");
            assert!(cyc(s, x.s_low, sh.length) >= 30.0, "{kind:?} at {s} under a bridge");
        }
        // And it passes the designer's check against all the others.
        let others: Vec<CustomFeature> = fs.iter().enumerate().filter(|(k, _)| *k != i).map(|(_, g)| *g).collect();
        assert_eq!(place_ok(sh, &others, kind, s), Ok(()), "{kind:?} at {s}");
    }
}

#[test]
fn auto_fill_follows_the_spacing_rules_and_is_deterministic() {
    let stroke = stadium(200.0, 55.0);
    let sh = shape(&stroke, false, W).expect("stadium");
    assert_valid(&sh);
    let a = auto_fill(&sh, 42);
    assert_eq!(a, auto_fill(&sh, 42), "deterministic");
    check_spacing(&sh, &a);
    let count = |k: CustomFeatureKind| a.iter().filter(|f| f.kind == k).count();
    assert!(count(CustomFeatureKind::Eggs) >= 1, "at least one egg row");
    assert_eq!(count(CustomFeatureKind::Boost), 1);
    assert_eq!(count(CustomFeatureKind::Ramp), 1, "two long straights: a ramp on the second");
    assert!(count(CustomFeatureKind::Coins) >= 2, "{a:?}");
    // The boost sits on the longest straight; coins sit inside their corner.
    let sp = sh.spacing();
    let on_straight = |s: f32| sh.straights.iter().any(|&(i, _, len)| (s - i as f32 * sp).rem_euclid(sh.length) <= len);
    for f in a.iter().filter(|f| matches!(f.kind, CustomFeatureKind::Boost | CustomFeatureKind::Ramp | CustomFeatureKind::Eggs)) {
        assert!(on_straight(f.s()), "{:?} at {} is not on a straight", f.kind, f.s());
    }
    for f in a.iter().filter(|f| f.kind == CustomFeatureKind::Coins) {
        let p = sh.point_at(f.s() + 9.0);
        let c = sh.corners.iter().min_by(|x, y| sh.points[x.idx].distance(p).total_cmp(&sh.points[y.idx].distance(p))).unwrap();
        assert_eq!(f.lateral_dm.signum() as i8, -c.inside_sign, "coins go on the inside");
    }
    // Different seeds may differ, but stay legal.
    for seed in [0u32, 1, 7, 12345] {
        check_spacing(&sh, &auto_fill(&sh, seed));
    }
    // Saved and converted, the features land where asked.
    let ct = save(&sh, a.clone());
    let def = to_def(&ct).unwrap();
    let rows = def.features.iter().filter(|f| matches!(f, Feature::TargetRow { count: 4, .. })).count();
    assert_eq!(rows, count(CustomFeatureKind::Eggs));
}

#[test]
fn auto_fill_keeps_clear_of_bridges() {
    let sh = shape(&eight_stroke(170.0, 110.0, 0.0), false, W).expect("eight");
    assert_eq!(sh.crossings.len(), 1);
    for seed in 0..6 {
        check_spacing(&sh, &auto_fill(&sh, seed));
    }
}

#[test]
fn manual_placement_allows_neighbours_start_corners_and_bridges() {
    let sh = shape(&stadium(200.0, 55.0), false, W).unwrap();
    use CustomFeatureKind::*;
    // Find the longest straight and its start arc.
    let sp = sh.spacing();
    let &(i0, _, len) = sh.straights.iter().max_by(|a, b| a.2.total_cmp(&b.2)).unwrap();
    let a0 = i0 as f32 * sp;
    let mid = (a0 + len * 0.5).rem_euclid(sh.length);
    assert_eq!(place_ok(&sh, &[], Gap, mid), Ok(()));
    assert_eq!(place_ok(&sh, &[], Log, 10.0), Ok(()));
    assert_eq!(place_ok(&sh, &[], Log, sh.length - 10.0), Ok(()));
    assert_eq!(place_ok(&sh, &[], Log, -1.0), Err(PlaceError::OutOfRange));
    assert_eq!(place_ok(&sh, &[], Log, sh.length + 1.0), Err(PlaceError::OutOfRange));
    let f = CustomFeature { kind: Log, s_dm: (mid * 10.0) as u16, lateral_dm: 0 };
    assert_eq!(place_ok(&sh, &[f], Eggs, mid), Ok(()));
    assert_eq!(place_ok(&sh, &[f], Coins, mid + 0.1), Ok(()));
    assert_eq!(place_ok(&sh, &[f], Eggs, mid + 39.0), Ok(()));
    assert_eq!(place_ok(&sh, &[f], Eggs, mid + 41.0), Ok(()));
    // A gap in a corner is refused.
    let c = sh.corners[0];
    assert_eq!(place_ok(&sh, &[], Gap, c.idx as f32 * sp), Ok(()));
    // A gap near the end of a straight is refused (its landing must be straight too).
    assert_eq!(place_ok(&sh, &[], Gap, (a0 + len - 5.0).rem_euclid(sh.length)), Ok(()));
    // Bridges: nothing on the ramp.
    let e = shape(&eight_stroke(170.0, 110.0, 0.9), false, W).unwrap();
    let x = e.crossings[0];
    for d in [0.0f32, 40.0, 80.0, 100.0] {
        let r = place_ok(&e, &[], Log, (x.s_high + d).rem_euclid(e.length));
        assert_eq!(r, Ok(()), "d {d}");
    }
    assert_eq!(place_ok(&e, &[], Log, (x.s_low + 10.0).rem_euclid(e.length)), Ok(()));
}

// -- encoding, hash, registry ----------------------------------------------------------

/// A hand-made track that does not depend on the shaper.
fn fixture() -> CustomTrack {
    // Integer arithmetic only, so the bytes are identical on every platform.
    let points: Vec<[i16; 2]> =
        (0..60i32).map(|i| [((i * 37 + i * i) % 997 - 400) as i16, ((i * 91) % 641 - 300) as i16]).collect();
    CustomTrack {
        v: 1,
        name: "Pinned Track".into(),
        look: TrackId::Dune,
        laps: 3,
        width_dm: 180,
        points,
        features: vec![
            CustomFeature { kind: CustomFeatureKind::Eggs, s_dm: 600, lateral_dm: 0 },
            CustomFeature { kind: CustomFeatureKind::Coins, s_dm: 1500, lateral_dm: -40 },
            CustomFeature { kind: CustomFeatureKind::Gap, s_dm: 2400, lateral_dm: 0 },
        ],
        seed: 0xDEAD_BEEF,
        hills: 0,
    }
}

#[test]
fn bytes_round_trip_and_the_hash_is_pinned() {
    let t = fixture();
    let bytes = t.to_bytes();
    assert_eq!(bytes.len(), 1 + 1 + 12 + 1 + 1 + 1 + 2 + 60 * 4 + 1 + 3 * 5 + 4);
    assert_eq!(CustomTrack::from_bytes(&bytes), Ok(t.clone()));
    // The wire format and hash must not change: saved tracks, bests and ghosts are keyed on it.
    assert_eq!(t.hash(), PINNED_HASH, "hash changed: {:#018x}", t.hash());
    let mut u = t.clone();
    u.features[0].s_dm += 1;
    assert_ne!(u.hash(), t.hash());
    let mut u = t.clone();
    u.name = "Other".into();
    assert_ne!(u.hash(), t.hash());
    // A shaped track survives too.
    let sh = shape(&blob(100.0, 3), false, W).unwrap();
    let c = save(&sh, auto_fill(&sh, 1));
    assert_eq!(CustomTrack::from_bytes(&c.to_bytes()), Ok(c.clone()));
    assert!(c.to_bytes().len() <= 900 * 4, "fits a few chunks: {}", c.to_bytes().len());
    // And through serde JSON (the My Tracks file).
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(serde_json::from_str::<CustomTrack>(&json).unwrap(), c);
}

/// FNV-1a 64 of the fixture's canonical bytes.
const PINNED_HASH: u64 = 0x2e8a_8a4a_b987_bb44;

#[test]
fn malformed_bytes_are_rejected_without_panicking() {
    let t = fixture();
    let good = t.to_bytes();
    let mut rng = Pcg32::seed_from_u64(2024);
    // Truncations of a valid encoding.
    for n in 0..good.len() {
        assert!(CustomTrack::from_bytes(&good[..n]).is_err(), "prefix {n}");
    }
    // Trailing bytes.
    let mut more = good.clone();
    more.push(0);
    assert!(CustomTrack::from_bytes(&more).is_err());
    // Random byte strings.
    let mut accepted = 0;
    for _ in 0..4000 {
        let n = rng.gen_range(0..3200usize);
        let b: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
        if CustomTrack::from_bytes(&b).is_ok() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 0);
    // Random mutations of a valid encoding.
    for _ in 0..4000 {
        let mut b = good.clone();
        for _ in 0..rng.gen_range(1..5) {
            let i = rng.gen_range(0..b.len());
            b[i] = rng.gen();
        }
        if let Ok(t) = CustomTrack::from_bytes(&b) {
            // Whatever decodes is sane and re-encodes to the same bytes.
            assert!(t.validate().is_ok());
            assert_eq!(t.to_bytes(), b);
        }
    }
    // Specific limits.
    let mut big = t.clone();
    big.points = vec![[0, 0]; 601];
    assert!(CustomTrack::from_bytes(&big.to_bytes()).is_err());
    let mut big = t.clone();
    big.features = vec![t.features[0]; 65];
    assert!(CustomTrack::from_bytes(&big.to_bytes()).is_err());
    let mut long = t.clone();
    long.name = "x".repeat(25);
    assert!(CustomTrack::from_bytes(&long.to_bytes()).is_err());
    long.name = "x".repeat(24);
    assert!(CustomTrack::from_bytes(&long.to_bytes()).is_ok());
    let mut bad = t.clone();
    bad.look = TrackId::Training;
    assert_eq!(CustomTrack::from_bytes(&bad.to_bytes()), Err(CustomError::Look));
    let mut bad = t.clone();
    bad.laps = 0;
    assert_eq!(bad.validate(), Err(CustomError::Laps));
    bad.laps = 10;
    assert_eq!(bad.validate(), Err(CustomError::Laps));
    let mut bad = t.clone();
    bad.width_dm = 139;
    assert_eq!(bad.validate(), Err(CustomError::Width));
    bad.width_dm = 221;
    assert_eq!(bad.validate(), Err(CustomError::Width));
}

#[test]
fn to_def_rejects_bad_geometry_and_features() {
    let sh = shape(&blob(100.0, 8), false, W).unwrap();
    let ok = save(&sh, vec![]);
    assert!(to_def(&ok).is_ok());
    let mut t = ok.clone();
    t.points.truncate(30);
    assert!(to_def(&t).is_err());
    let mut t = ok.clone();
    t.points[10] = [t.points[10][0] + 400, t.points[10][1]];
    assert!(to_def(&t).is_err(), "a spike breaks the spacing");
    let mut t = ok.clone();
    t.features.push(CustomFeature { kind: CustomFeatureKind::Log, s_dm: 65000, lateral_dm: 0 });
    assert_eq!(to_def(&t), Err(CustomError::Feature(0)));
    let mut t = ok.clone();
    t.features.push(CustomFeature { kind: CustomFeatureKind::Log, s_dm: 400, lateral_dm: 120 });
    assert_eq!(to_def(&t), Err(CustomError::Feature(0)));
    let mut t = ok.clone();
    t.features.push(CustomFeature { kind: CustomFeatureKind::Gap, s_dm: 100, lateral_dm: 0 });
    assert!(to_def(&t).is_ok(), "manual gaps may wrap their run-up across the start line");
    // A fresh to_def of untouched data is identical.
    assert_eq!(to_def(&ok), to_def(&ok));
}

#[test]
fn imported_tracks_reject_tight_corners_and_reversals() {
    let sh = shape(&stadium(200.0, 55.0), false, W).unwrap();
    let mut t = save(&sh, vec![]);
    // Each edge is 5 m, so spacing validation alone accepts these square corners.
    t.points = (0..40).map(|i| [i * 50, 0])
        .chain((0..40).map(|i| [2000, i * 50]))
        .chain((0..40).map(|i| [2000 - i * 50, 2000]))
        .chain((0..40).map(|i| [0, 2000 - i * 50])).collect();
    let decoded = CustomTrack::from_bytes(&t.to_bytes()).unwrap();
    assert!(matches!(to_def(&decoded), Err(CustomError::Shape(ShapeError::TooTight { .. }))));
    t.points.insert(5, t.points[3]);
    assert_eq!(min_radius(&t.points_m()), 0.0);
    assert!(matches!(to_def(&t), Err(CustomError::Shape(ShapeError::TooTight { .. }))));
}

#[test]
fn custom_races_keep_nine_laps() {
    let _serial = serial();
    let sh = shape(&stadium(200.0, 55.0), false, W).unwrap();
    let mut t = save(&sh, vec![]);
    t.laps = 9;
    custom::install(to_def(&t).unwrap());
    let mut cfg = RaceConfig::quick(TrackId::Custom, 0, BotLevel::Easy, 1);
    cfg.laps = 9;
    assert_eq!(World::new(cfg.clone()).cfg.laps, 9);
    cfg.laps = 255;
    assert_eq!(World::new(cfg.clone()).cfg.laps, 9);
    cfg.track = TrackId::Savanna;
    assert_eq!(World::new(cfg).cfg.laps, 5);
}

#[test]
fn to_def_maps_features_and_look() {
    let sh = shape(&stadium(200.0, 55.0), false, W).unwrap();
    use CustomFeatureKind::*;
    let fs = vec![
        CustomFeature { kind: Eggs, s_dm: 500, lateral_dm: 0 },
        CustomFeature { kind: Ramp, s_dm: 1000, lateral_dm: 0 },
        CustomFeature { kind: Gap, s_dm: 1500, lateral_dm: 0 },
        CustomFeature { kind: Log, s_dm: 2000, lateral_dm: 0 },
        CustomFeature { kind: Boost, s_dm: 2500, lateral_dm: 30 },
        CustomFeature { kind: Coins, s_dm: 3000, lateral_dm: -40 },
    ];
    let t = custom_track_from(&sh, "A very long name that must be cut short", TrackId::Midnight, 12, 100, fs, 5);
    assert_eq!(t.name.chars().count(), 24);
    assert_eq!((t.laps, t.width_dm), (9, 140));
    let t = CustomTrack { width_dm: 180, laps: 4, ..t };
    let d = to_def(&t).unwrap();
    let look = TrackId::Midnight.def();
    assert_eq!(d.id, TrackId::Custom);
    assert_eq!(d.theme, look.theme);
    assert_eq!(d.shoulder, look.shoulder);
    assert_eq!(d.laps, 4);
    assert!(d.points.iter().all(|p| p.pos.y == 0.0 && (p.width - 18.0).abs() < 1e-4));
    assert_eq!(d.points.len(), sh.points.len());
    assert_eq!(
        d.features,
        vec![
            Feature::TargetRow { s: 50.0, count: 4 },
            Feature::Ramp { s: 100.0, len: 12.0, height: 1.5, lip: 1.0 },
            Feature::Ramp { s: 137.0, len: 13.0, height: 2.0, lip: 0.0 },
            Feature::Gap { s: 150.0, len: 11.0 },
            Feature::Hurdle { s: 200.0 },
            Feature::BoostPad { s: 250.0, lateral: 0.0, len: 6.0 },
            Feature::CoinLine { s: 300.0, lateral: -4.0, count: 6, spacing: 3.0 },
        ]
    );
    // The seed depends on the track seed.
    let d2 = to_def(&CustomTrack { seed: 6, ..t.clone() }).unwrap();
    assert_ne!(d.seed, d2.seed);
}

#[test]
fn install_makes_custom_resolve_to_the_track() {
    let _g = serial();
    assert_eq!(TrackId::Custom.index(), TrackId::ALL.len());
    assert_eq!(TrackId::from_index(TrackId::ALL.len()), Some(TrackId::Custom));
    assert_eq!(TrackId::from_index(TrackId::ALL.len() + 1), None);
    assert!(!TrackId::ALL.contains(&TrackId::Custom));
    for id in TrackId::ALL {
        assert_eq!(TrackId::from_index(id.index()), Some(id));
    }
    let sh = shape(&blob(100.0, 21), false, W).unwrap();
    let def = to_def(&save(&sh, auto_fill(&sh, 3))).unwrap();
    let g0 = custom::generation();
    let g1 = custom::install(def.clone());
    assert_eq!(g1, g0 + 1);
    assert_eq!(custom::generation(), g1);
    let (g, cur) = custom::current().expect("installed");
    assert_eq!(g, g1);
    let t = TrackId::Custom.track();
    assert!(Arc::ptr_eq(&t, &cur));
    assert_eq!(t.id, TrackId::Custom);
    assert_eq!(t.name, "Test Loop");
    assert_eq!(TrackId::Custom.def(), TrackDef { id: TrackId::Custom, ..def.clone() });
    assert!(Arc::ptr_eq(&TrackId::Custom.track(), &t), "cached until the next install");
    // A second install replaces it and bumps the generation.
    let sh2 = shape(&blob(120.0, 22), false, W).unwrap();
    let def2 = to_def(&custom_track_from(&sh2, "Second", TrackId::Canyon, 3, 150, vec![], 1)).unwrap();
    let g2 = custom::install(def2);
    assert_eq!(g2, g1 + 1);
    let t2 = TrackId::Custom.track();
    assert!(!Arc::ptr_eq(&t2, &t));
    assert_eq!(t2.name, "Second");
    assert_eq!(t2.theme.wall_style, WallStyle::RockWall);
    assert_eq!(custom::look(), TrackId::Canyon, "the borrowed look is recoverable for per-track styling");
    // TrackId serde still resolves.
    let json = serde_json::to_string(&TrackId::Custom).unwrap();
    assert_eq!(serde_json::from_str::<TrackId>(&json).unwrap(), TrackId::Custom);
}

#[test]
fn install_is_thread_safe() {
    let _g = serial();
    let sh = shape(&blob(100.0, 30), false, W).unwrap();
    let def = to_def(&save(&sh, vec![])).unwrap();
    let g0 = custom::generation();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let d = def.clone();
            std::thread::spawn(move || {
                for _ in 0..3 {
                    custom::install(d.clone());
                    let _ = TrackId::Custom.track();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(custom::generation(), g0 + 12);
}

#[test]
fn custom_with_nothing_installed_is_the_training_ground() {
    // Only meaningful before any install in this process; the lock keeps it
    // from racing the installs, and the fallback is exercised by id equality.
    let _g = serial();
    if custom::current().is_none() {
        assert_eq!(TrackId::Custom.def().name, TrackId::Training.def().name);
        assert!(Arc::ptr_eq(&TrackId::Custom.track(), &TrackId::Training.track()));
    }
}

// -- racing on a custom track ---------------------------------------------------------------

#[test]
fn a_bot_race_of_two_laps_completes_on_a_drawn_track() {
    let _g = serial();
    let sh = shape(&blob(130.0, 5), false, W).expect("drawn-like loop");
    let fs = auto_fill(&sh, 9);
    assert!(!fs.is_empty());
    let def = to_def(&custom_track_from(&sh, "Bot Test", TrackId::Savanna, 2, 180, fs, 4)).unwrap();
    custom::install(def);
    let mut cfg = RaceConfig::all_bots(TrackId::Custom, 4, BotLevel::Normal, 11);
    cfg.laps = 2;
    let mut w = World::new(cfg);
    while w.phase != RacePhase::Finished && w.tick < 60 * 400 {
        w.step(&[]);
    }
    assert_eq!(w.phase, RacePhase::Finished, "race did not finish");
    for (i, laps) in w.lap_times.iter().enumerate() {
        assert_eq!(laps.len(), 2, "racer {i}: {laps:?}");
    }
    let stuck = w.max_stuck.iter().cloned().fold(0.0, f32::max);
    assert!(stuck <= 3.0, "a racer was stuck {stuck:.1} s");
}

#[test]
fn a_bot_race_completes_on_an_eight_with_a_bridge() {
    let _g = serial();
    let sh = shape(&eight_stroke(170.0, 110.0, 0.0), false, W).expect("eight");
    let def = to_def(&custom_track_from(&sh, "Eight Test", TrackId::Dune, 2, 180, auto_fill(&sh, 2), 4)).unwrap();
    custom::install(def);
    let tr = TrackId::Custom.track();
    assert_eq!(tr.bridges.len(), 1);
    let mut cfg = RaceConfig::all_bots(TrackId::Custom, 3, BotLevel::Normal, 5);
    cfg.laps = 2;
    let mut w = World::new(cfg);
    while w.phase != RacePhase::Finished && w.tick < 60 * 500 {
        w.step(&[]);
    }
    assert_eq!(w.phase, RacePhase::Finished, "race did not finish");
}

// -- hills ------------------------------------------------------------------------------------------

/// The (peak-to-peak, max grade) targets per hills level.
const HILL_TARGETS: [(f32, f32); 3] = [(4.0, 0.05), (9.0, 0.08), (15.0, 0.12)];

fn hilly(sh: &Shaped, hills: u8, seed: u32) -> CustomTrack {
    CustomTrack { hills, seed, ..custom_track_from(sh, "Hill Test", TrackId::Savanna, 2, 180, vec![], seed) }
}

fn p2p_and_grade(sh: &Shaped, h: &[f32]) -> (f32, f32) {
    let n = h.len();
    let hi = h.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let lo = h.iter().cloned().fold(f32::INFINITY, f32::min);
    let grade = (0..n)
        .map(|k| (h[(k + 1) % n] - h[k]).abs() / sh.points[k].distance(sh.points[(k + 1) % n]))
        .fold(0.0, f32::max);
    (hi - lo, grade)
}

#[test]
fn hills_survive_the_bytes_and_change_the_hash() {
    let flat = fixture();
    let flat_len = flat.to_bytes().len();
    assert_eq!(flat_len, 1 + 1 + 12 + 1 + 1 + 1 + 2 + 60 * 4 + 1 + 3 * 5 + 4, "flat tracks keep the old length");
    assert_eq!(flat.to_bytes()[0], 1);
    for hills in 1..=custom::HILLS_MAX {
        let t = CustomTrack { hills, ..fixture() };
        let b = t.to_bytes();
        assert_eq!(b.len(), flat_len + 1);
        assert_eq!(b[0], 2);
        assert_eq!(*b.last().unwrap(), hills);
        let back = CustomTrack::from_bytes(&b).unwrap();
        assert_eq!(back, CustomTrack { v: 2, ..t.clone() });
        assert_eq!(back.hash(), t.hash());
        assert_ne!(t.hash(), flat.hash());
    }
    // The version byte comes from `hills`, not from `v`.
    assert_eq!(CustomTrack { v: 2, ..fixture() }.to_bytes(), flat.to_bytes());
    assert_eq!(CustomTrack { v: 1, hills: 2, ..fixture() }.to_bytes()[0], 2);
    // Version 2 needs a level of 1 to 3.
    let mut b = CustomTrack { hills: 1, ..fixture() }.to_bytes();
    for bad in [0u8, 4, 255] {
        *b.last_mut().unwrap() = bad;
        assert!(matches!(CustomTrack::from_bytes(&b), Err(CustomError::Malformed(_))), "hills {bad}");
    }
    // Unknown versions are still refused, and a v1 body with a spare byte is trailing.
    let mut b9 = flat.to_bytes();
    b9[0] = 9;
    assert_eq!(CustomTrack::from_bytes(&b9), Err(CustomError::Version(9)));
    let mut spare = flat.to_bytes();
    spare.push(1);
    assert!(CustomTrack::from_bytes(&spare).is_err());
    // validate() bounds the level.
    assert_eq!(CustomTrack { hills: 4, ..fixture() }.validate(), Err(CustomError::Hills));
    assert_eq!(CustomTrack { v: 3, ..fixture() }.validate(), Err(CustomError::Version(3)));
}

#[test]
fn json_without_hills_loads_flat() {
    let t = fixture();
    let mut v: serde_json::Value = serde_json::to_value(&t).unwrap();
    v.as_object_mut().unwrap().remove("hills");
    let back: CustomTrack = serde_json::from_str(&v.to_string()).unwrap();
    assert_eq!(back.hills, 0);
    assert_eq!(back, t);
    assert_eq!(back.hash(), PINNED_HASH);
    let h = CustomTrack { hills: 3, ..fixture() };
    assert_eq!(serde_json::from_str::<CustomTrack>(&serde_json::to_string(&h).unwrap()).unwrap(), h);
}

#[test]
fn flat_tracks_have_flat_defs() {
    let sh = shape(&stadium(200.0, 55.0), false, W).unwrap();
    let t = save(&sh, vec![]);
    assert_eq!(t.hills, 0);
    assert!(custom::hill_heights(&t, &sh).iter().all(|&h| h == 0.0));
    let d = to_def(&t).unwrap();
    assert!(d.points.iter().all(|p| p.pos.y == 0.0 && p.bank == 0.0));
}

#[test]
fn hills_respect_height_and_grade_and_keep_the_start_flat() {
    let loops: Vec<Shaped> = vec![
        shape(&blob(130.0, 5), false, W).unwrap(),
        shape(&stadium(220.0, 60.0), false, W).unwrap(),
        shape(&ellipse(200.0, 120.0, 1.0, 3), false, W).unwrap(),
    ];
    for (li, sh) in loops.iter().enumerate() {
        assert!(sh.crossings.is_empty());
        for hills in 1..=3u8 {
            for seed in [1u32, 7, 12345] {
                let t = hilly(sh, hills, seed);
                let h = custom::hill_heights(&t, sh);
                assert_eq!(h.len(), sh.points.len());
                assert_eq!(h, custom::hill_heights(&t, sh), "deterministic");
                let (target, cap) = HILL_TARGETS[hills as usize - 1];
                let (p2p, grade) = p2p_and_grade(sh, &h);
                let tag = format!("loop {li} hills {hills} seed {seed}");
                assert!(p2p > 1.0, "{tag}: hills are visible, p2p {p2p}");
                assert!(p2p <= target + 0.01, "{tag}: p2p {p2p}");
                assert!(grade <= cap + 0.002, "{tag}: grade {grade}");
                // Flat within 40 m of the start line, either side.
                let mut s: f32 = 0.0;
                for k in 0..h.len() {
                    let d = s.min(sh.length - s);
                    if d < 40.0 {
                        assert!(h[k].abs() < 1e-3, "{tag}: y {} at s {s}", h[k]);
                    }
                    s += sh.points[k].distance(sh.points[(k + 1) % h.len()]);
                }
                let def = to_def(&t).unwrap();
                assert!(def.points.iter().zip(&h).all(|(p, &y)| (p.pos.y - y).abs() < 1e-4));
                let tr = Track::build(&def);
                assert!(tr.n() >= 4, "{tag}: builds");
            }
        }
    }
    // Different levels and seeds give different hills.
    let sh = &loops[0];
    assert_ne!(custom::hill_heights(&hilly(sh, 2, 1), sh), custom::hill_heights(&hilly(sh, 2, 2), sh));
}

#[test]
fn hills_leave_the_bridge_alone() {
    let sh = shape(&eight_stroke(170.0, 110.0, 0.0), false, W).expect("eight");
    assert_eq!(sh.crossings.len(), 1);
    let flat = hilly(&sh, 0, 4);
    let hill = hilly(&sh, 3, 4);
    let h = custom::hill_heights(&hill, &sh);
    assert!(h.iter().any(|&y| y.abs() > 0.1), "the eight still has some hills");
    let (df, dh) = (to_def(&flat).unwrap(), to_def(&hill).unwrap());
    let x = sh.crossings[0];
    let n = sh.points.len();
    let mut s = 0.0;
    for k in 0..n {
        let near = |c: f32| {
            let d = (s - c).abs();
            d.min(sh.length - d) < 80.0
        };
        if near(x.s_high) || near(x.s_low) {
            assert!(
                (df.points[k].pos.y - dh.points[k].pos.y).abs() < 1e-3,
                "s {s}: {} vs {}",
                df.points[k].pos.y,
                dh.points[k].pos.y
            );
        }
        s += sh.points[k].distance(sh.points[(k + 1) % n]);
    }
    let top = dh.points.iter().map(|p| p.pos.y).fold(0.0f32, f32::max);
    assert!(top >= 7.0 - 1e-3);
    let tr = Track::build(&dh);
    assert_eq!(tr.crossings.len(), 1);
    assert_eq!(tr.bridges.len(), 1);
    assert!(tr.crossings[0].gap() >= 6.5, "gap {}", tr.crossings[0].gap());
}

#[test]
fn a_bot_race_completes_on_a_steep_hilly_track() {
    let _g = serial();
    let sh = shape(&blob(130.0, 5), false, W).expect("drawn-like loop");
    let fs = auto_fill(&sh, 9);
    let t = CustomTrack { hills: 3, ..custom_track_from(&sh, "Hill Bots", TrackId::Savanna, 2, 180, fs, 4) };
    let def = to_def(&t).unwrap();
    assert!(def.points.iter().any(|p| p.pos.y > 3.0), "the lap has real climbs");
    custom::install(def);
    let mut cfg = RaceConfig::all_bots(TrackId::Custom, 4, BotLevel::Normal, 11);
    cfg.laps = 2;
    let mut w = World::new(cfg);
    while w.phase != RacePhase::Finished && w.tick < 60 * 400 {
        w.step(&[]);
    }
    assert_eq!(w.phase, RacePhase::Finished, "race did not finish");
    for (i, laps) in w.lap_times.iter().enumerate() {
        assert_eq!(laps.len(), 2, "racer {i}: {laps:?}");
    }
    let stuck = w.max_stuck.iter().cloned().fold(0.0, f32::max);
    assert!(stuck <= 3.0, "a racer was stuck {stuck:.1} s");
}

// -- recorder -------------------------------------------------------------------------------------

/// Feeds a path of `pts` at 0.6 m per tick; returns the status after each push.
fn run(rec: &mut Recorder, pts: &[Vec2]) -> RecorderStatus {
    let mut last = rec.status();
    let mut prev = pts[0];
    for &p in &pts[1..] {
        let d = p - prev;
        let n = (d.length() / 0.6).ceil().max(1.0) as usize;
        for k in 1..=n {
            last = rec.push(prev + d * (k as f32 / n as f32));
            if last != (RecorderStatus::Recording { len: rec.len() }) && !matches!(last, RecorderStatus::Recording { .. }) {
                return last;
            }
        }
        prev = p;
    }
    last
}

fn circle_path(r: f32, from: f32, to: f32, ccw: bool) -> Vec<Vec2> {
    // A circle through the origin: centre (0, r) for a start at the origin heading +x.
    let n = 400;
    (0..=n)
        .map(|i| {
            let a = from + (to - from) * i as f32 / n as f32;
            if ccw {
                Vec2::new(r * a.sin(), r - r * a.cos())
            } else {
                Vec2::new(r * a.sin(), -(r - r * a.cos()))
            }
        })
        .collect()
}

#[test]
fn recorder_closes_a_400_m_loop() {
    let r = 400.0 / TAU;
    let path = circle_path(r, 0.0, TAU + 0.05, true);
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    let st = run(&mut rec, &path);
    assert_eq!(st, RecorderStatus::Closed);
    assert!(rec.len() >= 300.0 && rec.len() < 420.0, "{}", rec.len());
    let pts = rec.points();
    assert!((pts[1].distance(pts[0]) - 2.0).abs() < 1e-3, "a point every 2 m");
    assert!(pts.windows(2).all(|w| (w[0].distance(w[1]) - 2.0).abs() < 1e-3));
    // Further pushes change nothing.
    let before = rec.points().len();
    assert_eq!(rec.push(Vec2::new(100.0, 100.0)), RecorderStatus::Closed);
    assert_eq!(rec.points().len(), before);
    // The recorded stroke shapes into a track.
    let sh = shape(rec.points(), false, W).expect("a recorded loop shapes");
    assert_valid(&sh);
}

#[test]
fn recorder_does_not_close_a_200_m_loop() {
    let r = 200.0 / TAU;
    let path = circle_path(r, 0.0, 2.0 * TAU + 0.05, true).into_iter().take(500).collect::<Vec<_>>();
    // Two laps of the 200 m circle is 400 m though: use a single pass plus a bit.
    let one = circle_path(r, 0.0, TAU + 0.1, true);
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    let st = run(&mut rec, &one);
    assert!(matches!(st, RecorderStatus::Recording { len } if len > 190.0 && len < 230.0), "{st:?}");
    let _ = path;
}

#[test]
fn recorder_waits_for_the_right_heading() {
    // Out along +x for 150 m, round a wide loop and back to the start line
    // heading the WRONG way (-x), double back and cross it going +x.
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    let mut path = vec![Vec2::ZERO];
    // Up 80 m, out along +x 160 m, down, back toward the origin along y = 0 from +x side: crosses
    // the start line (x = 0) heading -x at the origin.
    path.extend([Vec2::new(0.0, 0.0), Vec2::new(0.0, 80.0), Vec2::new(160.0, 80.0), Vec2::new(160.0, 0.0), Vec2::new(-20.0, 0.0)]);
    // 80 + 160 + 80 + 180 = 500 m so far: crossed x = 0 heading -x. Not closed.
    let st = run(&mut rec, &path);
    assert!(matches!(st, RecorderStatus::Recording { .. }), "closed too early: {st:?}");
    // Turn round beyond the line and come back across it heading +x.
    let st = run(&mut rec, &[Vec2::new(-20.0, 0.0), Vec2::new(-20.0, -15.0), Vec2::new(-5.0, -15.0), Vec2::new(-5.0, -3.0), Vec2::new(-5.0, -3.0)]);
    assert!(matches!(st, RecorderStatus::Recording { .. }), "{st:?}");
    let st = run(&mut rec, &[Vec2::new(-5.0, -3.0), Vec2::new(10.0, -3.0)]);
    assert_eq!(st, RecorderStatus::Closed, "heading +x across the line closes it");
}

#[test]
fn recorder_ignores_a_crossing_far_from_the_start() {
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    // A 400 m loop whose start line is crossed 40 m to the side.
    let path = vec![
        Vec2::ZERO,
        Vec2::new(0.0, 100.0),
        Vec2::new(-100.0, 100.0),
        Vec2::new(-100.0, -40.0),
        Vec2::new(-5.0, -40.0),
        Vec2::new(5.0, -40.0),
        Vec2::new(5.0, 40.0),
        Vec2::new(5.0, 100.0),
    ];
    let st = run(&mut rec, &path);
    // The start line is the x = 0 plane; at y = -40 it is 40 m from the start.
    assert!(matches!(st, RecorderStatus::Recording { .. }), "{st:?}");
}

#[test]
fn recorder_reports_too_long() {
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    let path = vec![Vec2::ZERO, Vec2::new(0.0, 1500.0), Vec2::new(1500.0, 1500.0), Vec2::new(1500.0, 0.0), Vec2::new(3000.0, 0.0)];
    assert_eq!(run(&mut rec, &path), RecorderStatus::TooLong);
    assert_eq!(rec.push(Vec2::ZERO), RecorderStatus::TooLong);
}

#[test]
fn recorder_ignores_garbage() {
    let mut rec = Recorder::new(Vec2::new(5.0, 5.0), Vec2::ZERO);
    assert!(matches!(rec.push(Vec2::NAN), RecorderStatus::Recording { .. }));
    assert_eq!(rec.push(Vec2::new(5.0, 5.5)), RecorderStatus::Recording { len: 0.0 });
    assert!(rec.is_empty());
    assert_eq!(rec.push(Vec2::new(1.0e9, 0.0)), RecorderStatus::TooLong);
}

/// Like [`run`], but collects every event on the way.
fn run_events(rec: &mut Recorder, pts: &[Vec2]) -> (RecorderStatus, Vec<custom::RecordEvent>) {
    let mut events = Vec::new();
    let mut last = rec.status();
    let mut prev = pts[0];
    for &p in &pts[1..] {
        let d = p - prev;
        let n = (d.length() / 0.6).ceil().max(1.0) as usize;
        for k in 1..=n {
            last = rec.push(prev + d * (k as f32 / n as f32));
            events.extend(rec.take_event());
        }
        prev = p;
    }
    (last, events)
}

#[test]
fn recorder_says_nothing_about_the_wiggle_at_the_start() {
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    // Back and forth over the line a few times in the first metres, then away.
    let path = [Vec2::ZERO, Vec2::new(4.0, 0.0), Vec2::new(-3.0, 1.0), Vec2::new(3.0, 1.0), Vec2::new(-2.0, 0.0), Vec2::new(40.0, 0.0)];
    let (st, events) = run_events(&mut rec, &path);
    assert!(matches!(st, RecorderStatus::Recording { .. }), "{st:?}");
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(rec.take_event(), None);
}

#[test]
fn recorder_reports_a_wrong_way_crossing_once_armed() {
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    // Out 50 m, round, and back through the start heading -x: 50 + 20 + 70 = 140 m.
    let path = [Vec2::ZERO, Vec2::new(50.0, 0.0), Vec2::new(50.0, 20.0), Vec2::new(10.0, 20.0), Vec2::new(10.0, 2.0), Vec2::new(-20.0, 2.0)];
    let (st, events) = run_events(&mut rec, &path);
    assert!(matches!(st, RecorderStatus::Recording { .. }), "{st:?}");
    assert_eq!(events, [custom::RecordEvent::WrongWay]);
    assert_eq!(rec.take_event(), None, "one-shot");
    // Turning round and crossing the right way, still short: told how far is left.
    let (st, events) = run_events(&mut rec, &[Vec2::new(-20.0, 2.0), Vec2::new(-20.0, -4.0), Vec2::new(-8.0, -4.0), Vec2::new(-8.0, -1.0), Vec2::new(10.0, -1.0)]);
    assert!(matches!(st, RecorderStatus::Recording { .. }), "{st:?}");
    assert_eq!(events.len(), 1, "{events:?}");
    let custom::RecordEvent::Early { remaining } = events[0] else { panic!("{events:?}") };
    let crossed_at = rec.len() - 10.0;
    assert!((remaining - (custom::RECORD_MIN - crossed_at)).abs() < 2.5, "{remaining} vs {crossed_at}");
    // A wrong-way crossing far from the start says nothing.
    let mut far = Recorder::new(Vec2::ZERO, Vec2::X);
    let (_, events) = run_events(&mut far, &[Vec2::ZERO, Vec2::new(50.0, 0.0), Vec2::new(50.0, 40.0), Vec2::new(-20.0, 40.0)]);
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn recorder_reports_a_skewed_crossing_after_300_m() {
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    // Out, round and back along the line itself: past 300 m, within 12 m of the gate, but
    // heading about 80 degrees off the way it faces.
    let path = [
        Vec2::ZERO,
        Vec2::new(150.0, 0.0),
        Vec2::new(150.0, -120.0),
        Vec2::new(-1.0, -120.0),
        Vec2::new(-1.0, -3.0),
        Vec2::new(1.0, 3.0),
        Vec2::new(1.0, 40.0),
    ];
    let (st, events) = run_events(&mut rec, &path);
    assert!(matches!(st, RecorderStatus::Recording { .. }), "{st:?}");
    assert!(rec.len() >= custom::RECORD_MIN);
    assert_eq!(events, [custom::RecordEvent::Skewed]);
    assert_eq!(rec.take_event(), None, "one-shot");
    // The same lap coming in square to the gate closes the loop, with no event.
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    let (st, events) = run_events(&mut rec, &[Vec2::ZERO, Vec2::new(150.0, 0.0), Vec2::new(150.0, -120.0), Vec2::new(-40.0, -120.0), Vec2::new(-40.0, 0.0), Vec2::new(20.0, 0.0)]);
    assert_eq!(st, RecorderStatus::Closed);
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn recorder_reports_an_early_crossing_and_still_closes_later() {
    // A 200 m circle passes the start the right way at 200 m: too early.
    let r = 200.0 / TAU;
    let mut rec = Recorder::new(Vec2::ZERO, Vec2::X);
    let (st, events) = run_events(&mut rec, &circle_path(r, 0.0, TAU + 0.1, true));
    assert!(matches!(st, RecorderStatus::Recording { .. }), "{st:?}");
    assert_eq!(events.len(), 1, "{events:?}");
    let custom::RecordEvent::Early { remaining } = events[0] else { panic!("{events:?}") };
    assert!((remaining - 100.0).abs() < 3.0, "{remaining}");
    // Round again: the second pass is past 300 m and closes the loop, with no event.
    let (st, events) = run_events(&mut rec, &circle_path(r, TAU + 0.1, 2.0 * TAU + 0.1, true));
    assert_eq!(st, RecorderStatus::Closed);
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(rec.start(), Vec2::ZERO);
    assert_eq!(rec.dir(), Vec2::X);
}

#[test]
fn a_session_world_uses_its_own_custom_track_after_the_registry_changes() {
    let _guard = serial();
    let first = shape(&ellipse(100.0, 80.0, 0.0, 1), false, W).unwrap();
    let first = custom_track_from(&first, "First room", TrackId::Savanna, 9, 180, vec![], 1);
    let owned = Arc::new(Track::build(&first.to_def().unwrap()));
    let second = shape(&ellipse(140.0, 90.0, 0.0, 2), false, W).unwrap();
    let second = custom_track_from(&second, "Second room", TrackId::Dune, 3, 180, vec![], 2);
    custom::install(second.to_def().unwrap());
    let mut cfg = RaceConfig::quick(TrackId::Custom, 1, BotLevel::Easy, 1);
    cfg.laps = 9;
    let world = World::with_track(cfg, owned.clone());
    assert!(Arc::ptr_eq(&world.track, &owned));
    assert_eq!(world.track.name, "First room");
    assert_eq!(world.cfg.laps, 9);
    assert_eq!(TrackId::Custom.track().name, "Second room");
}

#[test]
fn authored_analysis_preserves_points_and_start_when_the_width_changes() {
    let shaped = shape(&ellipse(120.0, 90.0, 0.0, 3), false, W).unwrap();
    let mut saved = custom_track_from(&shaped, "Authored", TrackId::Savanna, 3, 180, vec![], 3);
    let points = saved.points_m();
    assert_eq!(saved.shaped().unwrap().points, points);
    saved.width_dm = 220;
    let wide = saved.shaped().unwrap();
    assert_eq!(wide.points, points);
    assert_eq!(wide.width, 22.0);
    assert!(saved.to_def().is_ok());
}
