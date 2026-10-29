//! Laser Trump: the catch-up rocket (offer odds, motion, smash, wind-down).

mod common;
use common::*;
use race_core::math::DT;
use race_core::*;
use std::sync::Arc;

/// `n` human seats on the proving ground in a race, racer 0 last and racer
/// `n - 1` first, 20 m apart.
fn field(n: usize, seed: u64) -> World {
    let mut cfg = RaceConfig::quick(TrackId::Training, 0, BotLevel::Normal, seed);
    cfg.racers = (0..n)
        .map(|i| RacerSetup {
            name: format!("Rider {i}"),
            character: 0,
            kind: RacerKind::Human,
        })
        .collect();
    cfg.mode = RaceMode::Race;
    let mut w = World::with_track(cfg, Arc::new(proving_ground()));
    skip_countdown(&mut w);
    reset_field(&mut w);
    w.step(&vec![Input::default(); n]);
    w.drain_events();
    w
}

fn reset_field(w: &mut World) {
    for i in 0..w.racers.len() {
        place(w, i, 100.0 + i as f32 * 20.0, 0.0, 0.0);
    }
}

fn idle(w: &World) -> Vec<Input> {
    vec![Input::default(); w.racers.len()]
}

fn use_trump(w: &mut World, i: usize) -> Vec<Event> {
    w.give_item(i, Item::LaserTrump, 1);
    let mut inputs = idle(w);
    inputs[i].item = true;
    w.step(&inputs);
    w.drain_events()
}

fn hits(ev: &[Event], victim: usize) -> usize {
    ev.iter()
        .filter(|e| {
            matches!(e, Event::Hit { victim: v, by: Some(0), cause: HitCause::LaserTrump }
                if *v as usize == victim)
        })
        .count()
}

/// Opens an egg under racer `i` and returns the item it rolled.
fn roll(w: &mut World, i: usize) -> Option<Item> {
    reset_field(w);
    w.racers[i].item = None;
    w.targets.push(Target {
        pos: w.racers[i].pos,
        row: 0,
        alive: true,
        respawn_t: 0.0,
        bad: false,
        beam_t: 0.0,
    });
    let k = w.targets.len() - 1;
    w.step(&idle(w));
    w.drain_events();
    let item = w.racers[i].item;
    w.targets.remove(k);
    item
}

#[test]
fn laser_trump_keeps_its_wire_index_and_the_rose_follows_it() {
    assert_eq!(Item::ALL.len(), 16);
    assert_eq!(Item::LaserTrump.index(), 14);
    assert_eq!(Item::from_index(14), Some(Item::LaserTrump));
    assert_eq!(Item::Rose.index(), 15);
    assert_eq!(Item::from_index(15), Some(Item::Rose));
    assert_eq!(Item::from_index(16), None);

    assert_eq!(Item::LaserTrump.name(), "Laser Trump");
    // The roulette odds are untouched.
    assert!(ITEM_ROLLS.iter().all(|r| r.item != Item::LaserTrump));
}

#[test]
fn trump_is_never_offered_to_the_leader_or_to_small_fields() {
    for n in [2usize, 3, 4, 8] {
        let mut w = field(n, 5);
        for i in 0..n {
            let place = n - i;
            let trumps = (0..400)
                .filter(|_| roll(&mut w, i) == Some(Item::LaserTrump))
                .count();
            // Racers sit 20 m apart, the leader with nobody ahead.
            let ahead = (place > 1).then_some(20.0);
            if trump_chance(place as u8, n, ahead) > 0.05 {
                assert!(trumps > 0, "n={n} place {place} should be offered it");
            } else if n < 3 || place == 1 {
                assert_eq!(trumps, 0, "n={n} place {place} must never be offered it");
            }
        }
    }
}

#[test]
fn the_leader_is_never_granted_a_trump_whatever_the_seed() {
    for seed in 0..40u64 {
        let mut w = field(8, seed);
        for _ in 0..50 {
            assert_ne!(roll(&mut w, 7), Some(Item::LaserTrump), "seed {seed}");
        }
    }
}

#[test]
fn trump_odds_follow_the_curve_with_a_twenty_metre_gap() {
    let mut w = field(8, 11);
    let draws = 3000;
    for (i, place) in [(0usize, 8u8), (1, 7), (2, 6)] {
        let rate = (0..draws)
            .filter(|_| roll(&mut w, i) == Some(Item::LaserTrump))
            .count() as f32
            / draws as f32;
        let want = trump_chance(place, 8, Some(20.0));
        assert!((rate - want).abs() < 0.04, "place {place} rate {rate} vs {want}");
    }
}

#[test]
fn trump_is_never_rolled_in_a_time_trial_or_while_holding_one() {
    let mut w = field(4, 3);
    w.cfg.mode = RaceMode::TimeTrial;
    for _ in 0..300 {
        assert_ne!(roll(&mut w, 0), Some(Item::LaserTrump));
    }
    w.cfg.mode = RaceMode::Race;
    for _ in 0..300 {
        w.racers[0].effects.trump = 2.0;
        assert_ne!(roll(&mut w, 0), Some(Item::LaserTrump));
    }
}

#[test]
fn using_it_starts_an_immune_autopiloted_rocket() {
    let mut w = field(4, 7);
    // Only the trumper: park the others far away so nothing is in the way.
    for i in 1..4 {
        place(&mut w, i, 600.0, 0.0, 0.0);
    }
    place(&mut w, 0, 100.0, 0.0, 20.0);
    w.racers[0].stun_t = 0.0;
    w.racers[0].slow_t = 3.0;
    w.racers[0].stumble_t = 1.0;
    let ev = use_trump(&mut w, 0);
    assert!(ev.contains(&Event::ItemUsed {
        racer: 0,
        item: Item::LaserTrump
    }));
    let r = &w.racers[0];
    assert!(r.trumping());
    assert_eq!(r.slow_t, 0.0);
    assert_eq!(r.stumble_t, 0.0);
    assert!(r.effects.trump > TRUMP_TIME - 0.1);
    let top = Tunables::default().top_speed * r.class.mods().top * TRUMP_SPEED;
    let start_s = r.s;
    // Full left lock, drift and look-back pressed: all ignored.
    let wild = Input {
        steer: -1.0,
        drift: true,
        jump: true,
        look_back: true,
        ..Input::go()
    };
    for _ in 0..40 {
        w.step(&[wild, Input::default(), Input::default(), Input::default()]);
    }
    let r = &w.racers[0];
    assert!(r.speed > top * 0.97, "speed {} of {top}", r.speed);
    assert!(
        r.lateral.abs() < 3.0,
        "autopilot drove off line: {}",
        r.lateral
    );
    assert!(!r.drift.active && r.on_ground);
    assert!(r.s - start_s > 30.0);
}

#[test]
fn rocket_brushes_aside_cacti_and_eggs_and_shrugs_off_a_dollar_bill() {
    let mut w = field(3, 9);
    for i in 1..3 {
        place(&mut w, i, 600.0, 0.0, 0.0);
    }
    place(&mut w, 0, 100.0, 0.0, 20.0);
    let cactus_at = w.racers[0].pos + w.racers[0].forward() * 25.0;
    w.hazards.push(Hazard {
        kind: HazardKind::Cactus,
        pos: cactus_at,
        owner: 1,
        age: 5.0,
        id: 900,
    });
    use_trump(&mut w, 0);
    let mut seen = vec![];
    for _ in 0..90 {
        w.step(&idle(&w));
        seen.extend(w.drain_events());
        if w.racers[0].s > 180.0 {
            break;
        }
    }
    assert!(w.hazards.is_empty(), "cactus should be smashed");
    assert_eq!(w.racers[0].stun_t, 0.0);
    assert!(!seen
        .iter()
        .any(|e| matches!(e, Event::Hit { victim: 0, .. })));
    // A bill fired at the rocket does nothing.
    let r = w.racers[0].clone();
    w.projectiles.push(Projectile {
        kind: ProjectileKind::DollarBill,
        pos: r.body(),
        vel: Vec3::ZERO,
        owner: 1,
        target: Some(0),
        age: 0.0,
        bounces: 0,
        hint: r.track_idx,
        s: r.s,
        lateral: r.lateral,
        id: 901,
    });
    w.step(&idle(&w));
    assert!(w
        .drain_events()
        .iter()
        .all(|e| !matches!(e, Event::Hit { victim: 0, .. })));
    assert_eq!(w.racers[0].effects.dismount, 0.0);
}

#[test]
fn rocket_drives_straight_over_a_rug_pull_that_trips_everyone_else() {
    let s = 400.0;
    let rig = |trump: bool| {
        let mut w = field(4, 4);
        for i in 1..4 {
            place(&mut w, i, 800.0, 0.0, 0.0);
        }
        w.play.rugs = vec![mischief::Rug {
            s,
            rest: 45,
            owner: 1,
            ..Default::default()
        }];
        place(&mut w, 0, s - 30.0, 0.0, 20.0);
        if trump {
            use_trump(&mut w, 0);
        }
        for _ in 0..80 {
            w.step(&idle(&w));
        }
        w.racers[0].effects.dismount
    };
    // The trap works on an ordinary racer at that spot (control), not on the rocket.
    let mut control = field(2, 4);
    place(&mut control, 1, 20.0, 0.0, 0.0);
    control.play.rugs = vec![mischief::Rug {
        s,
        rest: 45,
        owner: 1,
        ..Default::default()
    }];
    place(&mut control, 0, s - 3.0, 0.0, 10.0);
    let mut tripped = false;
    for _ in 0..30 {
        control.step(&idle(&control));
        tripped |= control.racers[0].effects.dismount > 0.0;
    }
    assert!(tripped, "control racer should be pulled");
    assert_eq!(rig(true), 0.0);
}

#[test]
fn a_racer_in_its_path_spins_out_with_the_trump_hit_and_is_thrown_aside() {
    let mut w = field(3, 2);
    place(&mut w, 2, 600.0, 0.0, 0.0);
    place(&mut w, 0, 100.0, 0.0, 25.0);
    place(&mut w, 1, 125.0, 1.0, 0.0);
    w.racers[1].coins = 5;
    let tun = Tunables::default();
    let mut ev = use_trump(&mut w, 0);
    for _ in 0..90 {
        w.step(&idle(&w));
        ev.extend(w.drain_events());
    }
    assert_eq!(hits(&ev, 1), 1, "exactly one hit on the victim: {ev:?}");
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::CoinsDropped { racer: 1, .. })));
    assert_eq!(w.racers[1].coins, 5 - tun.egg_coins.min(5) as u32);
    assert!(
        w.racers[1].lateral.abs() > 1.0,
        "thrown to the side: {}",
        w.racers[1].lateral
    );
    // The rocket itself is never hit and never bumped.
    assert!(!ev.iter().any(|e| matches!(e, Event::Hit { victim: 0, .. })));
}

#[test]
fn the_rocket_hits_each_victim_once_and_leaves_spinning_racers_alone() {
    let mut w = field(4, 6);
    place(&mut w, 3, 600.0, 0.0, 0.0);
    place(&mut w, 0, 100.0, 0.0, 25.0);
    place(&mut w, 1, 140.0, 0.0, 0.0);
    place(&mut w, 2, 170.0, 0.0, 0.0);
    // Racer 2 is already spinning when the rocket arrives, with coins to lose.
    w.racers[2].spin_out(5.0);
    w.racers[2].coins = 9;
    w.racers[1].coins = 9;
    let mut ev = use_trump(&mut w, 0);
    for _ in 0..90 {
        w.step(&idle(&w));
        ev.extend(w.drain_events());
    }
    assert_eq!(hits(&ev, 1), 1);
    assert_eq!(hits(&ev, 2), 0, "a spinning racer is not hit again");
    assert_eq!(w.racers[2].coins, 9);
    let drops = ev
        .iter()
        .filter(|e| matches!(e, Event::CoinsDropped { racer: 1, .. }))
        .count();
    assert_eq!(drops, 1, "one coin drop per victim per pass");
}

#[test]
fn a_shielded_racer_loses_the_shield_instead_and_is_not_hit_again() {
    let mut w = field(3, 8);
    place(&mut w, 2, 600.0, 0.0, 0.0);
    place(&mut w, 0, 100.0, 0.0, 25.0);
    place(&mut w, 1, 150.0, 0.0, 0.0);
    w.racers[1].shield_t = 20.0;
    let mut ev = use_trump(&mut w, 0);
    for _ in 0..90 {
        w.step(&idle(&w));
        ev.extend(w.drain_events());
    }
    assert!(ev.iter().any(|e| matches!(
        e,
        Event::ShieldBlock {
            racer: 1,
            by: Some(0)
        }
    )));
    assert_eq!(hits(&ev, 1), 0);
    assert_eq!(w.racers[1].shield_t, 0.0);
    assert_eq!(w.racers[1].stun_t, 0.0, "the shield took the hit");
}

#[test]
fn a_racer_just_behind_is_caught_in_the_tail_beam() {
    let mut w = field(3, 12);
    place(&mut w, 2, 600.0, 0.0, 0.0);
    place(&mut w, 0, 200.0, 0.0, 0.0);
    // Four metres back in the same lane: outside the contact circle, inside the beam.
    place(&mut w, 1, 196.0, 0.0, 0.0);
    let ev = use_trump(&mut w, 0);
    assert_eq!(hits(&ev, 1), 1, "{ev:?}");
    assert!(w.racers[1].stun_t > 0.0);
    // Beside the beam, or beyond its end, is safe.
    let mut w = field(4, 12);
    place(&mut w, 3, 600.0, 0.0, 0.0);
    place(&mut w, 0, 200.0, 0.0, 0.0);
    place(&mut w, 1, 196.0, 3.5, 0.0);
    place(&mut w, 2, 200.0 - TRUMP_TAIL - 3.0, 0.0, 0.0);
    let ev = use_trump(&mut w, 0);
    assert_eq!(hits(&ev, 1) + hits(&ev, 2), 0, "{ev:?}");
}

#[test]
fn trump_blasts_a_last_placer_up_the_field() {
    let mut w = field(6, 21);
    // Spread racers 25 m apart, all standing still, racer 0 far at the back.
    for i in 0..6 {
        place(&mut w, i, 100.0 + i as f32 * 25.0, 0.0, 0.0);
    }
    w.step(&idle(&w));
    assert_eq!(w.racers[0].place, 6);
    use_trump(&mut w, 0);
    for _ in 0..(TRUMP_TIME * 60.0) as usize {
        w.step(&idle(&w));
    }
    assert!(w.racers[0].place <= 2, "place {}", w.racers[0].place);
}

#[test]
fn rocket_winds_down_once_in_the_top_two() {
    let mut w = field(4, 14);
    // Racer 0 is already out in front (place 1) when it fires.
    place(&mut w, 0, 400.0, 0.0, 25.0);
    for i in 1..4 {
        place(&mut w, i, 100.0 + i as f32 * 20.0, 0.0, 0.0);
    }
    w.step(&idle(&w));
    assert_eq!(w.racers[0].place, 1);
    use_trump(&mut w, 0);
    assert!(
        w.racers[0].effects.trump <= playful::TRUMP_WIND_DOWN + 1e-4,
        "trump left {}",
        w.racers[0].effects.trump
    );
    // And at the back it does not wind down.
    let mut w = field(4, 14);
    use_trump(&mut w, 0);
    assert!(w.racers[0].effects.trump > TRUMP_TIME - 0.1);
}

#[test]
fn expiry_carries_a_boost_so_the_racer_does_not_stall() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 100.0, 0.0, 0);
    set_speed(&mut r, 56.0);
    r.effects.trump = DT * 0.5;
    r.integrate(&Input::go(), &tr, &t);
    assert_eq!(r.effects.trump, 0.0);
    assert!(!r.trumping());
    assert!(
        r.boost_t >= playful::TRUMP_EXIT_BOOST - DT - 1e-4,
        "boost {}",
        r.boost_t
    );
    // It keeps going well above normal top speed for a moment.
    drive(&mut r, &tr, &t, Input::go(), 30);
    assert!(r.speed > t.top_speed * 1.2, "speed {}", r.speed);
}

#[test]
fn integrate_is_pure_with_trump_active() {
    let tr = TrackId::Canyon.track();
    let t = Tunables::default();
    let mut a = racer_at(&tr, 300.0, 0.0, 2);
    let mut b = a.clone();
    let mut seq = 99u32;
    for k in 0..1500 {
        seq = seq.wrapping_mul(1664525).wrapping_add(1013904223);
        let inp = Input {
            throttle: 1.0,
            ..Input::unpack(seq.to_le_bytes())
        };
        if k % 400 < 250 {
            a.effects.trump = 2.0;
            b.effects.trump = 2.0;
        }
        assert_eq!(a.integrate(&inp, &tr, &t), b.integrate(&inp, &tr, &t));
    }
    assert_eq!(a, b);
}

#[test]
fn the_world_with_a_trump_is_deterministic() {
    let run_it = || {
        let mut w = World::new(RaceConfig::all_bots(TrackId::Dune, 8, BotLevel::Normal, 31));
        skip_countdown(&mut w);
        let mut evs = vec![];
        for k in 0..60 * 20 {
            if k == 600 {
                w.give_item(7, Item::LaserTrump, 1);
            }
            w.step(&[]);
            evs.extend(w.drain_events());
        }
        (format!("{:?}", w.racers), evs)
    };
    let (a, ea) = run_it();
    let (b, eb) = run_it();
    assert_eq!(a, b);
    assert_eq!(ea, eb);
}

#[test]
fn bots_use_a_trump_when_they_get_one() {
    let mut w = World::new(RaceConfig::all_bots(
        TrackId::Savanna,
        4,
        BotLevel::Normal,
        5,
    ));
    skip_countdown(&mut w);
    run(&mut w, 120, |_| vec![]);
    // The grid is random, so take whoever is last: the front two get no rocket.
    let last = (0..4).max_by_key(|&i| w.racers[i].place).unwrap();
    w.give_item(last, Item::LaserTrump, 1);
    let mut used = false;
    for _ in 0..60 {
        w.step(&[]);
        used |= w.drain_events().iter().any(|e| {
            matches!(e, Event::ItemUsed { racer, item: Item::LaserTrump } if *racer as usize == last)
        });
        if used {
            break;
        }
    }
    assert!(used);
    assert!(w.racers[last].trumping());
}

#[test]
fn effects_pack_carries_the_trump_timer_to_remote_racers() {
    let e = playful::Effects {
        trump: 2.3,
        pill: 1.0,
        ..Default::default()
    };
    let back = playful::Effects::unpack(e.pack());
    assert!((back.trump - 2.3).abs() < 0.11);
    assert!((back.pill - 1.0).abs() < 0.11);
    let tr = proving_ground();
    let mut r = racer_at(&tr, 100.0, 0.0, 0);
    r.effects.trump = 2.0;
    let mut copy = racer_at(&tr, 100.0, 0.0, 0);
    copy.apply_net(&r.to_net());
    assert!(copy.trumping());
}

#[test]
fn the_rocket_stays_on_every_built_in_track_for_a_full_lap() {
    let t = Tunables::default();
    for id in TrackId::RACE_ALL {
        let tr = id.track();
        for character in 0..8u8 {
            let mut r = racer_at(&tr, 50.0, 0.0, character);
            set_speed(&mut r, 30.0);
            let mut walls = 0;
            let mut travelled = 0.0;
            let mut last = r.s;
            let mut ticks = 0;
            while travelled < tr.length && ticks < 60 * 200 {
                // Keep re-arming the timer: a full lap at rocket speed.
                r.effects.trump = 2.0;
                let fx = r.integrate(&Input::go(), &tr, &t);
                if fx.wall_hit > 0.2 {
                    walls += 1;
                }
                let pr = tr.project(r.pos, r.track_idx as usize);
                let half = tr.half_band(pr.s, pr.lateral);
                assert!(
                    pr.lateral.abs() <= half,
                    "{id:?} char {character}: lateral {} beyond {half} at s {}",
                    pr.lateral,
                    pr.s
                );
                travelled += tr.s_delta(last, r.s).max(0.0);
                last = r.s;
                ticks += 1;
            }
            assert!(
                travelled >= tr.length,
                "{id:?} char {character} never finished the lap"
            );
            // A scrape on a hairpin is acceptable; bouncing along the walls is not.
            assert!(walls <= 3, "{id:?} char {character}: {walls} wall hits");
        }
    }
}

/// Eight bots, the trumper last, the rest spread `spacing` metres apart.
fn spread_field(id: TrackId, seed: u64, spacing: f32) -> World {
    let mut w = World::new(RaceConfig::all_bots(id, 8, BotLevel::Normal, seed));
    skip_countdown(&mut w);
    run(&mut w, 60 * 3, |_| vec![]);
    for i in 0..8 {
        place(&mut w, i, 300.0 + i as f32 * spacing, 0.0, 30.0);
        let r = &mut w.racers[i];
        r.lap = 1;
        r.next_cp = 4;
        r.progress = r.s;
        r.item = None;
    }
    w.step(&[]);
    w
}

/// Tuning check, printed with `--nocapture`: places gained by a last-place
/// trumper in a spread field of eight on a normal track.
#[test]
fn trump_gains_most_of_the_field_but_not_the_win_from_a_spread_field() {
    let tracks = [
        TrackId::Savanna,
        TrackId::Dune,
        TrackId::Canyon,
        TrackId::Midnight,
        TrackId::Baobab,
        TrackId::Oasis,
    ];
    for (spacing, min_mean, max_mean) in [(14.0f32, 4.0, 7.0), (20.0, 3.5, 6.0), (26.0, 2.5, 5.0)] {
        let (mut gained, mut runs) = (0.0, 0);
        let mut ends = [0; 9];
        for id in tracks {
            for seed in 1..=3u64 {
                let mut w = spread_field(id, seed, spacing);
                assert_eq!(w.racers[0].place, 8);
                use_trump(&mut w, 0);
                run(&mut w, ((TRUMP_TIME + 1.5) * 60.0) as usize, |_| vec![]);
                let p = w.racers[0].place;
                gained += 8.0 - p as f32;
                ends[p as usize] += 1;
                runs += 1;
            }
        }
        let mean = gained / runs as f32;
        eprintln!(
            "trump spacing {spacing} m: mean places gained {mean:.2}, end place histogram (1st..8th) {:?}",
            &ends[1..]
        );
        assert!(
            (min_mean..=max_mean).contains(&mean),
            "spacing {spacing}: mean gain {mean}"
        );
    }
}

#[test]
fn rocket_is_not_felled_by_a_frog_but_an_ordinary_racer_is() {
    let tumble_after_frog = |trump: bool| {
        let mut w = field(4, 17);
        for i in 1..4 {
            place(&mut w, i, 800.0, 0.0, 0.0);
        }
        if trump {
            use_trump(&mut w, 0);
        }
        w.step(&idle(&w));
        let mut frog = w.play.frogs[0].pos;
        frog.y = w
            .track
            .project(frog, w.racers[0].track_idx as usize)
            .ground_y;
        let r = &mut w.racers[0];
        r.pos = frog;
        r.vel = Vec3::ZERO;
        r.speed = 0.0;
        w.step(&idle(&w));
        w.racers[0].effects.dismount
    };
    assert!(
        tumble_after_frog(false) > 0.0,
        "control racer should be tumbled"
    );
    assert_eq!(tumble_after_frog(true), 0.0);
}
