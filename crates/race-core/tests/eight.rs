//! The Figure Eight with its overpass, the Paddock, the wider Training oval
//! and the generic crossing support in `Track::build` (spec 18.1).

use race_core::math::flat;
use race_core::*;

fn eight() -> std::sync::Arc<Track> {
    TrackId::Eight.track()
}

/// Max grade (rise / horizontal run) between neighbouring samples.
fn max_grade(tr: &Track) -> f32 {
    let n = tr.n();
    (0..n)
        .map(|i| {
            let (a, b) = (&tr.samples[i], &tr.samples[(i + 1) % n]);
            (b.pos.y - a.pos.y).abs() / flat(b.pos - a.pos).length().max(1e-3)
        })
        .fold(0.0, f32::max)
}

#[test]
fn eight_has_one_overpass() {
    let tr = eight();
    assert_eq!(tr.crossings.len(), 1, "exactly one crossing");
    assert_eq!(tr.bridges.len(), 1, "exactly one bridge");
    let c = tr.crossings[0];
    assert!(c.gap() >= 6.5, "height gap {:.2}", c.gap());
    assert!((c.y_low).abs() < 0.05, "the other branch stays at 0");
    assert!((c.w_low - 18.0).abs() < 0.01);
    let g = max_grade(&tr);
    assert!(g <= 0.15, "max grade {:.3}", g);
    // Span: higher branch within w_lower/2 + 6 m of the crossing.
    let b = tr.bridges[0];
    let reach = 0.5 * c.w_low + 6.0;
    assert!((b.s1 - b.s0 - 2.0 * reach).abs() < 0.01);
    assert!(b.contains(c.s_high, tr.length));
    assert!(!b.contains(c.s_high + reach + 1.0, tr.length));
    assert!(!b.contains(c.s_low, tr.length), "the lower branch is not a bridge");
    // Ramps of at least 45 m on each side: still above 1 m 45 m from the crossing.
    for side in [-1.0f32, 1.0] {
        let s = c.s_high + side * 45.0;
        assert!(tr.sample_at(s).pos.y > 1.0, "ramp reaches {:.2} m at 45 m", tr.sample_at(s).pos.y);
    }
    // Net heights: only the one diagonal rises.
    let high = tr.samples.iter().filter(|s| s.pos.y > 0.01).count();
    assert!(high as f32 * tr.ds < 2.0 * 95.0, "only one diagonal rises ({high})");
    assert_eq!(tr.laps, 3);
    assert!(tr.samples.iter().all(|s| (s.width - 18.0).abs() < 0.01));
    assert_eq!(tr.theme.wall_style, WallStyle::Paddock);
}

#[test]
fn eight_has_the_oval_feature_kinds_clear_of_the_crossing() {
    let tr = eight();
    let c = tr.crossings[0];
    let at = tr.sample_at(c.s_low).pos;
    let (mut gates, mut hurdles, mut rows, mut ramps, mut pads, mut lines) = (0, 0, 0, 0, 0, 0);
    let check = |s: f32, span: f32| {
        let mut x = 0.0;
        while x <= span {
            let sm = tr.sample_at(s + x);
            assert!(
                flat(sm.pos - at).length() > 25.0,
                "feature at s {:.0} is {:.1} m from the crossing",
                s + x,
                flat(sm.pos - at).length()
            );
            assert!(sm.pos.y < 0.01, "feature at s {:.0} is on the bridge ramp (y {:.2})", s + x, sm.pos.y);
            x += 1.0;
        }
    };
    for f in &tr.features {
        match *f {
            Feature::Gate { s } => {
                gates += 1;
                check(s, 0.0);
            }
            Feature::Hurdle { s } => {
                hurdles += 1;
                check(s, 0.0);
            }
            Feature::TargetRow { s, count } => {
                rows += 1;
                assert!((3..=5).contains(&count));
                check(s, 0.0);
            }
            Feature::Ramp { s, len, lip, .. } => {
                ramps += 1;
                check(s, len + lip);
            }
            Feature::BoostPad { s, len, .. } => {
                pads += 1;
                check(s, len);
            }
            Feature::CoinLine { s, count, spacing, lateral } => {
                lines += 1;
                assert!(lateral.abs() < 9.0 - 1.0);
                check(s, count as f32 * spacing);
            }
            Feature::Gap { .. } => panic!("no gaps on the Figure Eight"),
        }
    }
    assert_eq!(gates, 8);
    assert!(hurdles >= 5);
    assert_eq!(rows, 3);
    assert_eq!(ramps, 1);
    assert_eq!(pads, 1);
    assert_eq!(lines, 6);
}

#[test]
fn eight_net_turn_is_zero_and_loops_balance() {
    let tr = eight();
    let k: f32 = tr.samples.iter().map(|s| s.curvature * tr.ds).sum();
    assert!(k.abs() < 0.3, "net turn {k} rad");
    let left = tr.samples.iter().filter(|s| s.curvature > 0.005).count();
    let right = tr.samples.iter().filter(|s| s.curvature < -0.005).count();
    assert!((left as i32 - right as i32).abs() < 20, "left {left} right {right}");
    assert!((tr.length - 842.0).abs() < 20.0, "length {}", tr.length);
}

#[test]
fn racers_keep_their_branch_through_the_crossing() {
    let tr = eight();
    let c = tr.crossings[0];
    // Drive 2 laps on the centre line, then 2 laps weaving across the road,
    // 0.6 m per tick (36 m/s), projecting from the previous index each tick.
    for weave in [false, true] {
        let mut hint = 0usize;
        let mut prev_s = 0.0f32;
        let mut s = 0.0f32;
        let mut over_bridge = 0;
        let mut under_bridge = 0;
        while s < 2.0 * tr.length {
            let lat = if weave { (s * 0.21).sin() * 6.0 } else { 0.0 };
            let p = tr.point_at(s, lat);
            let pr = tr.project(p, hint);
            assert!(
                tr.s_delta(prev_s, pr.s).abs() <= 5.0,
                "weave {weave}: s jumped {:.1} -> {:.1} at lap s {:.1}",
                prev_s,
                pr.s,
                tr.wrap_s(s)
            );
            assert!((tr.s_delta(pr.s, s)).abs() < 1.0, "projected s {:.1} vs {:.1}", pr.s, s);
            assert!((pr.lateral - lat).abs() < 1.0);
            assert!((pr.ground_y - p.y).abs() < 0.05, "ground {:.2} vs {:.2}", pr.ground_y, p.y);
            let near = |a: f32| tr.s_delta(tr.wrap_s(s), a).abs() < 5.0;
            if near(c.s_high) {
                over_bridge += 1;
                assert!(pr.ground_y > 6.0, "on the deck: {:.2}", pr.ground_y);
            }
            if near(c.s_low) {
                under_bridge += 1;
                assert!(pr.ground_y < 0.5, "on the lower road: {:.2}", pr.ground_y);
            }
            hint = pr.idx;
            prev_s = pr.s;
            s += 0.6;
        }
        assert!(over_bridge > 10 && under_bridge > 10);
    }
}

#[test]
fn project_global_picks_the_branch_by_height() {
    let tr = eight();
    let c = tr.crossings[0];
    let low = tr.sample_at(c.s_low);
    let high = tr.sample_at(c.s_high);
    // Same plan position (within a metre): a point on the lower road under the
    // bridge, and a point on the deck.
    assert!(flat(low.pos - high.pos).length() < 2.0);
    let under = tr.project_global(Vec3::new(low.pos.x, low.pos.y + 0.3, low.pos.z));
    assert!(tr.s_delta(under.s, c.s_low).abs() < 2.0, "under: s {:.1} want {:.1}", under.s, c.s_low);
    assert!(under.ground_y < 0.5);
    let over = tr.project_global(Vec3::new(high.pos.x, high.pos.y + 0.3, high.pos.z));
    assert!(tr.s_delta(over.s, c.s_high).abs() < 2.0, "over: s {:.1} want {:.1}", over.s, c.s_high);
    assert!(over.ground_y > 6.5);
    // A little to each side of the lower road's centre, still the lower branch.
    for lat in [-6.0f32, 6.0] {
        let p = low.pos + low.right * lat;
        let pr = tr.project_global(Vec3::new(p.x, 0.2, p.z));
        assert!(tr.s_delta(pr.s, c.s_low).abs() < 3.0 && pr.ground_y < 0.5);
    }
    assert_eq!(tr.locate(Vec3::new(high.pos.x, high.pos.y + 1.0, high.pos.z)), tr.idx_at(c.s_high));
}

#[test]
fn terrain_under_the_bridge_follows_the_lower_road() {
    let tr = eight();
    let c = tr.crossings[0];
    let mid = tr.sample_at(c.s_high).pos;
    let h = tr.terrain_height(mid.x, mid.z);
    assert!((h - c.y_low).abs() < 0.5, "terrain {h:.2} vs lower road {:.2}", c.y_low);
    // Across the whole width of the lower road the ground stays near it: it
    // never rises towards the deck.
    let mut d = -0.5 * c.w_low;
    while d <= 0.5 * c.w_low {
        let p = tr.sample_at(c.s_high + d).pos;
        let h = tr.terrain_height(p.x, p.z);
        assert!((h - c.y_low).abs() < 1.5, "terrain {h:.2} at {d:.0} m from the crossing");
        d += 1.0;
    }
}

#[test]
fn field_is_open_ground() {
    assert!(TrackId::Field.is_open_field());
    assert!(!TrackId::Eight.is_open_field() && !TrackId::Training.is_open_field());
    let tr = TrackId::Field.track();
    assert_eq!(tr.laps, 1);
    assert!(tr.features.is_empty());
    assert!(tr.crossings.is_empty() && tr.bridges.is_empty());
    assert!(tr.samples.iter().all(|s| (s.width - 270.0).abs() < 0.01 && s.shoulder_l == 0.0 && s.shoulder_r == 0.0));
    // The centreline is a circle of radius 150 m about the origin.
    for s in tr.samples.iter().step_by(37) {
        let r = flat(s.pos).length();
        assert!((r - 150.0).abs() < 1.0, "centreline radius {r}");
    }
    for r in [16.0f32, 20.0, 150.0, 280.0, 284.0] {
        for ang in [0.0f32, 1.1, 2.9, 4.4, 6.0] {
            let p = Vec3::new(r * ang.cos(), 0.0, r * ang.sin());
            let pr = tr.project_global(p);
            assert!(pr.lateral.abs() < 135.0, "r {r}: lateral {:.1}", pr.lateral);
            assert!(pr.ground_y.abs() < 0.01, "r {r}: ground {:.2}", pr.ground_y);
            assert_eq!(tr.sample_at(pr.s).surface, Surface::Road);
        }
    }
    // Theme: no road visible.
    let t = &tr.theme;
    assert_eq!(t.road, t.ground);
    assert_eq!(t.road_stripe, t.ground);
    assert_eq!(t.curb_a, t.ground);
    assert_eq!(t.curb_b, t.ground);
    assert_eq!(t.wall_style, WallStyle::Paddock);
    // Open ground: from r = 15 m to r = 285 m, then the fence.
    assert!((tr.max_half_band - 135.0).abs() < 0.01);
}

#[test]
fn training_oval_is_wider() {
    let tr = TrackId::Training.track();
    let want = 2.0 * 160.0 + 2.0 * std::f32::consts::PI * 65.0;
    assert!((tr.length - want).abs() < want * 0.02, "lap {} vs {want}", tr.length);
    assert!(tr.samples.iter().all(|s| (s.width - 20.0).abs() < 0.01));
    assert!(tr.crossings.is_empty() && tr.bridges.is_empty());
    // Same feature kinds and counts as before.
    let count = |f: fn(&Feature) -> bool| tr.features.iter().filter(|x| f(x)).count();
    assert_eq!(count(|f| matches!(f, Feature::Gate { .. })), 8);
    assert_eq!(count(|f| matches!(f, Feature::Hurdle { .. })), 5);
    assert_eq!(count(|f| matches!(f, Feature::TargetRow { .. })), 3);
    assert_eq!(count(|f| matches!(f, Feature::Ramp { .. })), 1);
    assert_eq!(count(|f| matches!(f, Feature::BoostPad { .. })), 1);
    assert_eq!(count(|f| matches!(f, Feature::CoinLine { .. })), 6);
    // Gates are evenly spaced over the new lap.
    let mut gates: Vec<f32> = tr
        .features
        .iter()
        .filter_map(|f| if let Feature::Gate { s } = f { Some(*s) } else { None })
        .collect();
    gates.sort_by(|a, b| a.total_cmp(b));
    for w in gates.windows(2) {
        assert!((w[1] - w[0] - tr.length / 8.0).abs() < 0.05 * tr.length / 8.0);
    }
    // No feature sticks out past the lap.
    for f in &tr.features {
        let s = match *f {
            Feature::Gate { s } | Feature::Hurdle { s } => s,
            Feature::TargetRow { s, .. } | Feature::BoostPad { s, .. } | Feature::CoinLine { s, .. } | Feature::Ramp { s, .. } => s,
            Feature::Gap { s, .. } => s,
        };
        assert!(s > 0.0 && s < tr.length, "{f:?}");
    }
}

#[test]
fn crossing_detection_is_generic() {
    // A lemniscate laid flat crosses itself once at the origin; the builder
    // finds it (an at-grade crossing reports a zero gap, which the rule for
    // authored tracks forbids).
    let n = 48;
    let pts: Vec<TrackPoint> = (0..n)
        .map(|k| {
            let t = k as f32 / n as f32 * std::f32::consts::TAU;
            TrackPoint { pos: Vec3::new(130.0 * t.sin(), 0.0, 70.0 * (2.0 * t).sin()), width: 14.0, bank: 0.0 }
        })
        .collect();
    let def = TrackDef {
        id: TrackId::Training,
        name: "lemniscate".into(),
        theme: TrackId::Training.def().theme,
        laps: 1,
        points: pts,
        shoulder: [2.0, 2.0],
        features: vec![],
        seed: 3,
    };
    let tr = Track::build(&def);
    assert_eq!(tr.crossings.len(), 1, "{:?}", tr.crossings);
    assert_eq!(tr.bridges.len(), 1);
    assert!(tr.crossings[0].gap() < 0.01);
    // A plain oval has none.
    assert!(TrackId::Savanna.track().crossings.is_empty());
}

#[test]
fn bots_race_two_laps_on_the_eight() {
    let tr = eight();
    let mut cfg = RaceConfig::all_bots(TrackId::Eight, 8, BotLevel::Normal, 21);
    cfg.laps = 2;
    let mut w = World::new(cfg);
    let mut prev: Vec<f32> = w.racers.iter().map(|r| r.s).collect();
    let mut max_jump = 0.0f32;
    let mut on_deck = vec![false; 8];
    let mut under = vec![false; 8];
    while w.phase != RacePhase::Finished && w.tick < 60 * 300 {
        w.step(&[]);
        for (i, r) in w.racers.iter().enumerate() {
            max_jump = max_jump.max(tr.s_delta(prev[i], r.s).abs());
            prev[i] = r.s;
            if r.pos.y > 6.0 {
                on_deck[i] = true;
            }
            if r.pos.y < 1.0 && flat(r.pos - tr.sample_at(tr.crossings[0].s_low).pos).length() < 6.0 {
                under[i] = true;
            }
        }
    }
    assert_eq!(w.phase, RacePhase::Finished, "race did not finish");
    for (i, laps) in w.lap_times.iter().enumerate() {
        assert_eq!(laps.len(), 2, "racer {i} ran {} laps", laps.len());
        assert!(w.racers[i].finished_at.is_some());
        assert!(on_deck[i] && under[i], "racer {i} should cross both branches");
    }
    assert!(max_jump <= 5.0, "s jumped {max_jump:.1} m in one tick");
    let stuck = w.max_stuck.iter().cloned().fold(0.0, f32::max);
    assert!(stuck <= 3.0, "a racer was stuck {stuck:.1} s");
}

#[test]
fn field_is_drivable_with_no_respawns() {
    use race_core::math::DT;
    // A human circles the Paddock with a bot dropped on it too: nothing falls
    // out of the world and nobody leaves the band (the fence is at r = 15 / 285).
    let mut cfg = RaceConfig::quick(TrackId::Field, 1, BotLevel::Normal, 5);
    cfg.laps = 1;
    let mut w = World::new(cfg);
    let mut respawns = 0;
    for _ in 0..(40.0 / DT) as usize {
        w.step(&[Input { steer: 0.25, throttle: 1.0, ..Default::default() }]);
        respawns += w.drain_events().iter().filter(|e| matches!(e, Event::Respawned { .. })).count();
        for r in &w.racers {
            let rad = flat(r.pos).length();
            assert!(r.pos.is_finite() && r.pos.y.abs() < 3.0, "pos {:?}", r.pos);
            assert!(rad > 14.0 && rad < 286.0, "left the band: r = {rad:.1}");
        }
    }
    assert_eq!(respawns, 0);
    assert!(flat(w.racers[0].pos).length() > 20.0);
}

#[test]
fn a_deck_is_found_over_the_lower_road_and_not_over_its_own() {
    let tr = eight();
    let c = tr.crossings[0];
    let cp = tr.sample_at(c.s_low).pos;
    // Over the lower road at the crossing: the deck, 7 m up; a racer on that deck has none above.
    let own_low = tr.sample_at(c.s_low).s;
    let d = tr.deck_surface_above(cp.x, cp.z, 2.5, own_low, 30.0).expect("deck above the lower road");
    assert!((d - c.y_high).abs() < 0.6, "deck at {d}, crossing says {}", c.y_high);
    let hp = tr.sample_at(c.s_high).pos;
    assert_eq!(tr.deck_surface_above(hp.x, hp.z, hp.y + 2.5, c.s_high, 30.0), None);
    // The deck's own road is skipped even when it is above the query height.
    assert_eq!(tr.deck_surface_above(hp.x, hp.z, 2.5, c.s_high, 30.0), None);
    // Away from any bridge: nothing.
    let far = tr.sample_at(tr.wrap_s(c.s_low + tr.length * 0.25)).pos;
    assert_eq!(tr.deck_surface_above(far.x, far.z, 2.5, 0.0, 30.0), None);
    // A point well off to the side of the deck: nothing.
    let right = tr.sample_at(c.s_high).right;
    let side = hp + right * 40.0;
    assert_eq!(tr.deck_surface_above(side.x, side.z, 2.5, own_low, 30.0), None);
}
