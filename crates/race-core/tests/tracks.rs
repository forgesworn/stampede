//! Track geometry and authoring rules (spec 7).

mod common;
use common::*;
use race_core::*;

const WALL: f32 = 1.0;

fn half_band(s: &TrackSample, side: f32) -> f32 {
    0.5 * s.width
        + if side < 0.0 {
            s.shoulder_l
        } else {
            s.shoulder_r
        }
}

#[test]
fn thirteen_tracks_in_order() {
    let defs = track_defs();
    assert_eq!(defs.len(), 13);
    assert_eq!(TrackId::ALL.len(), 13);
    for (i, d) in defs.iter().enumerate() {
        assert_eq!(d.id.index(), i);
        assert_eq!(TrackId::from_index(i), Some(d.id));
    }
    assert_eq!(defs[0].name, "Savanna Sprint");
    assert_eq!(defs[1].name, "Dune Drift");
    assert_eq!(defs[2].name, "Red Rock Canyon");
    assert_eq!(defs[3].name, "Mempool Midnight");
    assert_eq!(defs[4].name, "Training Ground");
    assert_eq!(defs[5].name, "Baobab Bayou");
    assert_eq!(defs[6].name, "Volcano Vault");
    assert_eq!(defs[7].name, "Oasis Run");
    assert_eq!(defs[8].name, "Halving Highway");
    assert_eq!(defs[9].name, "Dust Bowl");
    assert_eq!(defs[10].name, "Mempool Plaza");
    // The training venues are appended last: saved indices never shift.
    assert_eq!(defs[11].name, "Figure Eight");
    assert_eq!(defs[12].name, "The Paddock");
    assert_eq!(TrackId::Eight.index(), 11);
    assert_eq!(TrackId::Field.index(), 12);
    assert!(TrackId::ALL.iter().all(|t| t.is_open_field() == (*t == TrackId::Field)));
    for t in [TrackId::Eight, TrackId::Field] {
        assert!(!t.is_arena());
        assert!(!TrackId::RACE_ALL.contains(&t) && !TrackId::ARENAS.contains(&t));
    }
    assert_eq!(TrackId::ARENAS, [TrackId::DustBowl, TrackId::MempoolPlaza]);
    for a in TrackId::ARENAS {
        assert!(a.is_arena());
        assert!(!TrackId::RACE_ALL.contains(&a) && !TrackId::RACE.contains(&a) && !TrackId::CUP2.contains(&a));
    }
    assert!(TrackId::RACE_ALL.iter().all(|t| !t.is_arena()));
    // Cup 1 is unchanged; RACE_ALL is Cup 1 then Cup 2.
    assert_eq!(
        TrackId::RACE,
        [
            TrackId::Savanna,
            TrackId::Dune,
            TrackId::Canyon,
            TrackId::Midnight
        ]
    );
    assert_eq!(TrackId::RACE_ALL[..4], TrackId::RACE);
    assert_eq!(TrackId::RACE_ALL[4..], TrackId::CUP2);
}

#[test]
fn lengths_match_the_spec() {
    for (id, want) in [
        (TrackId::Savanna, 1100.0),
        (TrackId::Dune, 1300.0),
        (TrackId::Canyon, 1400.0),
        (TrackId::Midnight, 1500.0),
        (TrackId::Baobab, 1300.0),
        (TrackId::Volcano, 1500.0),
        (TrackId::Oasis, 1200.0),
        (TrackId::Halving, 1800.0),
    ] {
        let tr = id.track();
        assert!(
            (tr.length - want).abs() <= want * 0.15,
            "{:?} is {:.0} m, want ~{want}",
            id,
            tr.length
        );
        assert!((tr.ds - 1.0).abs() < 0.01);
    }
}

#[test]
fn no_self_overlap() {
    for id in TrackId::ALL {
        let tr = id.track();
        let n = tr.samples.len();
        for i in 0..n {
            let a = &tr.samples[i];
            let ha = half_band(a, -1.0).max(half_band(a, 1.0));
            for j in (i + 1)..n {
                let b = &tr.samples[j];
                let hb = half_band(b, -1.0).max(half_band(b, 1.0));
                let sep = ((j - i) as f32).min((n - (j - i)) as f32) * tr.ds;
                if sep <= 1.6 * (ha + hb) {
                    continue; // neighbouring section
                }
                if (a.pos.y - b.pos.y).abs() >= 4.0 {
                    continue; // grade-separated (an overpass), see the crossings test
                }
                let d = (a.pos - b.pos) * Vec3::new(1.0, 0.0, 1.0);
                assert!(
                    d.length() > ha + hb + 2.0 * WALL,
                    "{:?}: s {:.0} and s {:.0} are {:.1} m apart (need {:.1})",
                    id,
                    a.s,
                    b.s,
                    d.length(),
                    ha + hb + 2.0 * WALL
                );
            }
        }
    }
}

#[test]
fn corners_never_fold_the_road() {
    for id in TrackId::ALL {
        let tr = id.track();
        for s in &tr.samples {
            let r = 1.0 / s.curvature.abs().max(1e-6);
            let inside = if s.curvature > 0.0 {
                half_band(s, -1.0)
            } else {
                half_band(s, 1.0)
            };
            assert!(
                r > inside + 2.0,
                "{:?} s {:.0}: radius {r:.1} vs inside band {inside:.1}",
                id,
                s.s
            );
        }
        for l in &tr.racing_line {
            let s = &tr.samples[0];
            assert!(
                l.offset.abs() <= 0.5 * s.width,
                "{:?} racing line leaves the road",
                id
            );
        }
    }
}

#[test]
fn projection_walks_and_is_consistent() {
    for id in TrackId::ALL {
        let tr = id.track();
        let mut hint = 0;
        let mut s = 0.0;
        while s < tr.length * 2.0 {
            let lat = ((s * 0.37).sin()) * 0.5 * tr.sample_at(s).width;
            let p = tr.point_at(s, lat);
            let pr = tr.project(p, hint);
            // The 40 m arenas project points 20 m off a corner's centreline:
            // the arc length there is a little coarser. On the Paddock (270 m
            // band, R 150 m) the radial lines converge towards the island, so
            // s is only good to a few metres at r = 15 m (it is the lateral and
            // the road surface that matter on open ground).
            let tol = if id.is_open_field() {
                0.3 + 0.03 * lat.abs()
            } else if id.is_arena() {
                0.3 + 0.015 * lat.abs()
            } else {
                0.3
            };
            assert!(
                tr.s_delta(pr.s, s).abs() < tol,
                "{:?}: s {:.1} projected to {:.1}",
                id,
                s,
                pr.s
            );
            assert!(
                (pr.lateral - lat).abs() < tol,
                "{:?}: lateral {:.2} vs {:.2}",
                id,
                pr.lateral,
                lat
            );
            assert!((pr.ground_y - p.y).abs() < 0.05);
            hint = pr.idx;
            s += 3.7;
        }
        // A cold start finds the right place too.
        let p = tr.point_at(tr.length * 0.6, 2.0);
        assert!(tr.s_delta(tr.project_global(p).s, tr.length * 0.6).abs() < 0.3);
    }
}

#[test]
fn features_follow_the_authoring_rules() {
    for id in TrackId::RACE_ALL {
        let tr = id.track();
        let mut rows = 0;
        let mut lines = 0;
        let mut ramps = 0;
        let mut pads = 0;
        let mut gaps = Vec::new();
        for f in &tr.features {
            match *f {
                Feature::TargetRow { count, .. } => {
                    rows += 1;
                    assert!((3..=5).contains(&count));
                }
                Feature::CoinLine {
                    count, lateral, s, ..
                } => {
                    lines += 1;
                    assert!((4..=6).contains(&count));
                    assert!(
                        lateral.abs() < 0.5 * tr.sample_at(s).width - 1.0,
                        "{:?} coins off road",
                        id
                    );
                }
                Feature::Ramp { .. } => ramps += 1,
                Feature::BoostPad { .. } => pads += 1,
                Feature::Gap { s, len } => gaps.push((s, len)),
                _ => {}
            }
        }
        assert!((2..=4).contains(&rows), "{:?}: {rows} target rows", id);
        assert!((8..=14).contains(&lines), "{:?}: {lines} coin lines", id);
        match id {
            TrackId::Savanna => {
                assert_eq!(ramps, 1);
                assert!(tr.samples.iter().all(|s| s.width >= 18.0));
            }
            TrackId::Dune => {
                assert_eq!(ramps, 2);
                assert!(tr.samples[0].shoulder_l >= 5.0);
            }
            TrackId::Canyon => {
                assert_eq!(gaps.len(), 1);
                assert!(tr.samples.iter().all(|s| s.width <= 13.0));
                let (gs, _) = gaps[0];
                let pad_before = tr.features.iter().any(|f| matches!(*f, Feature::BoostPad { s, .. } if tr.s_delta(s, gs) > 10.0 && tr.s_delta(s, gs) < 80.0));
                assert!(pad_before, "boost pad before the canyon gap");
                let ys: Vec<f32> = tr.samples.iter().map(|s| s.pos.y).collect();
                let range = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
                    - ys.iter().cloned().fold(f32::INFINITY, f32::min);
                assert!(range > 12.0, "canyon climbs and drops: {range}");
            }
            TrackId::Midnight => {
                assert!(pads >= 6, "many boost pads: {pads}");
                assert!(tr.samples.iter().all(|s| s.pos.y.abs() < 1e-3), "flat");
                assert!(tr.theme.night);
            }
            TrackId::Baobab => {
                assert!(tr.samples.iter().all(|s| (14.0..=16.0).contains(&s.width)));
                assert!(tr.theme.fog_density >= 0.005, "low mist");
                assert!(gaps.is_empty());
            }
            TrackId::Volcano => {
                assert_eq!(gaps.len(), 2, "two gap jumps");
                assert!(tr.samples.iter().all(|s| s.width <= 13.0));
                for &(gs, _) in &gaps {
                    let pad_before = tr.features.iter().any(|f| matches!(*f, Feature::BoostPad { s, .. } if tr.s_delta(s, gs) > 10.0 && tr.s_delta(s, gs) < 80.0));
                    assert!(pad_before, "boost pad before each gap");
                }
                let ys: Vec<f32> = tr.samples.iter().map(|s| s.pos.y).collect();
                let range = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
                    - ys.iter().cloned().fold(f32::INFINITY, f32::min);
                assert!(range > 18.0, "big climbs and drops: {range}");
            }
            TrackId::Oasis => {
                assert_eq!(ramps, 3);
                assert!(tr.samples.iter().all(|s| s.width >= 18.5));
                assert!(lines >= 12, "many coin lines: {lines}");
            }
            TrackId::Halving => {
                assert!(pads >= 8, "many boost pads: {pads}");
                assert!(tr.theme.night);
                // The mega ramp is in the last quarter.
                let mega = tr.features.iter().any(|f| matches!(*f, Feature::Ramp { s, height, .. } if height >= 3.5 && s > 0.85 * tr.length));
                assert!(mega, "mega ramp near the end");
            }
            TrackId::Training | TrackId::DustBowl | TrackId::MempoolPlaza | TrackId::Eight | TrackId::Field | TrackId::Custom => unreachable!(),
        }
    }
    let tr = TrackId::Training.track();
    let gates = tr
        .features
        .iter()
        .filter(|f| matches!(f, Feature::Gate { .. }))
        .count();
    let hurdles = tr
        .features
        .iter()
        .filter(|f| matches!(f, Feature::Hurdle { .. }))
        .count();
    assert_eq!(gates, 8);
    assert!(hurdles >= 5);
    assert!(tr
        .features
        .iter()
        .any(|f| matches!(f, Feature::Ramp { .. })));
    assert!(tr
        .features
        .iter()
        .any(|f| matches!(f, Feature::BoostPad { .. })));
    assert!(tr
        .features
        .iter()
        .any(|f| matches!(f, Feature::TargetRow { .. })));
}

#[test]
fn scenery_stays_outside_the_walls_and_terrain_is_finite() {
    for id in TrackId::ALL {
        let tr = id.track();
        let sc = tr.scenery();
        assert!(!sc.is_empty());
        assert_eq!(sc, tr.scenery(), "deterministic");
        for it in &sc {
            assert!(
                tr.distance_to_centreline(it.pos.x, it.pos.z) > tr.max_half_band,
                "{:?} {:?} inside the band",
                id,
                it.kind
            );
            assert!(it.pos.y.is_finite());
        }
        let (lo, hi) = tr.terrain_bounds();
        for k in 0..400 {
            let x = lo.x + (hi.x - lo.x) * ((k * 37 % 400) as f32 / 400.0);
            let z = lo.z + (hi.z - lo.z) * ((k * 91 % 400) as f32 / 400.0);
            assert!(tr.terrain_height(x, z).is_finite());
        }
        // Just outside a wall, the terrain is near the road height (no cliffs at the fence).
        if id != TrackId::Canyon && id != TrackId::Volcano {
            for i in (0..tr.samples.len()).step_by(25) {
                let s = &tr.samples[i];
                if s.surface == Surface::Gap {
                    continue;
                }
                // The terrain follows the lower road under an overpass, not the deck.
                if tr.crossings.iter().any(|c| tr.s_delta(s.s, c.s_high).abs() < 60.0) {
                    continue;
                }
                let p = s.pos + s.right * (half_band(s, 1.0) + 1.0);
                let h = tr.terrain_height(p.x, p.z);
                assert!(
                    (h - s.pos.y).abs() < 3.0,
                    "{:?} s {:.0}: terrain {h:.1} road {:.1}",
                    id,
                    s.s,
                    s.pos.y
                );
            }
        }
    }
}

#[test]
fn theme_and_characters() {
    assert_eq!(CHARACTERS.len(), 8);
    let names: Vec<&str> = CHARACTERS.iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        ["Dusty", "Blaze", "Satoshi", "Nova", "Boomer", "Ziggy", "Hodl", "Pip"]
    );
    let classes: Vec<CharClass> = CHARACTERS.iter().map(|c| c.class).collect();
    use CharClass::*;
    assert_eq!(
        classes,
        [Medium, Light, Medium, Light, Heavy, Light, Heavy, Medium]
    );
    for (i, c) in CHARACTERS.iter().enumerate() {
        assert_eq!(c.id as usize, i);
        let j = serde_json::to_string(c).unwrap();
        assert_eq!(j, i.to_string());
        let back: Character = serde_json::from_str(&j).unwrap();
        assert_eq!(&back, c);
    }
    // Tunables: JSON round trip and partial override.
    let t = Tunables::default();
    let back: Tunables = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
    assert_eq!(back, t);
    let part: Tunables = serde_json::from_str(r#"{"top_speed": 40.0}"#).unwrap();
    assert_eq!(part.top_speed, 40.0);
    assert_eq!(part.accel, t.accel);
}

#[test]
fn ground_follows_ramps_banks_and_shoulders() {
    let tr = proving_ground();
    let ramp = pg_back_straight() + 200.0;
    let top = tr.point_at(ramp + 11.9, 0.0).y;
    assert!((top - 1.5).abs() < 0.15, "ramp top {top}");
    // Ramp only across the road, not the shoulder.
    let sh = tr.point_at(ramp + 11.9, 11.0).y;
    assert!(sh < 0.5);
    // Shoulders rise gently.
    let edge = tr.point_at(30.0, 10.0).y;
    let out = tr.point_at(30.0, 13.5).y;
    assert!(out > edge && out - edge < 0.3);
}

#[test]
fn crossings_are_grade_separated() {
    for id in TrackId::ALL {
        let tr = id.track();
        assert_eq!(tr.crossings.len(), tr.bridges.len(), "{id:?}");
        for c in &tr.crossings {
            assert!(c.gap() >= 4.0, "{id:?}: crossing at s {:.0} has only {:.2} m of clearance", c.s_high, c.gap());
        }
        match id {
            TrackId::Eight => assert_eq!(tr.crossings.len(), 1),
            _ => assert!(tr.crossings.is_empty(), "{id:?} should not cross itself"),
        }
    }
}
