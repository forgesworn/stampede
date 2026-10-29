//! Interactions (spec 5), race rules (spec 6), determinism and the net form.

mod common;
use common::*;
use race_core::math::DT;
use race_core::*;

fn idle(n: usize) -> Vec<Input> {
    vec![Input::default(); n]
}

fn count<F: Fn(&Event) -> bool>(ev: &[Event], f: F) -> usize {
    ev.iter().filter(|e| f(e)).count()
}

#[test]
fn racer_countdown_step_and_pick_coin_are_what_the_world_does() {
    // A client predicting its own racer runs these alone; they must give the
    // world's own result, launch for launch.
    let w0 = humans(TrackId::Savanna, 3, 1);
    let mut w = w0.clone();
    let mut solo: Vec<Racer> = w0.racers.clone();
    let mut launches = vec![None; 3];
    while w.phase == RacePhase::Countdown {
        let left = w.countdown_left();
        let inputs = vec![
            Input { throttle: if left <= 0.3 { 1.0 } else { 0.0 }, ..Default::default() },
            Input::go(),
            Input::default(),
        ];
        let last = w.tick + 1 >= w.go_tick;
        for (i, r) in solo.iter_mut().enumerate() {
            if let Some(l) = r.countdown_step(&inputs[i], last, &w.tun) {
                launches[i] = Some(l);
            }
        }
        w.step(&inputs);
    }
    assert_eq!(launches, vec![Some(Launch::StartBoost), Some(Launch::Stumble), None]);
    for (a, b) in solo.iter().zip(&w.racers) {
        assert_eq!((a.boost_t, a.stumble_t, a.launch_t, a.prev_input), (b.boost_t, b.stumble_t, b.launch_t, b.prev_input));
    }

    let tun = Tunables::default();
    let mut r = w0.racers[0].clone();
    assert!(r.reaches_coin(r.body(), &tun));
    assert!(!r.reaches_coin(r.body() + Vec3::X * (tun.coin_radius + 0.5), &tun));
    r.coins = u32::MAX; // a hostile snapshot must not overflow it
    assert_eq!(r.pick_coin(&tun), u32::MAX);
    r.coins = 0;
    assert_eq!(r.pick_coin(&tun), 1);
    assert_eq!(r.boost_t, tun.coin_boost);
    r.respawn_t = 0.5;
    assert!(!r.reaches_coin(r.body(), &tun));
}

#[test]
fn countdown_then_go_with_start_boost_and_stumble() {
    let mut w = humans(TrackId::Savanna, 3, 1);
    let mut ev = Vec::new();
    let go = w.go_tick;
    while w.phase == RacePhase::Countdown {
        let left = w.countdown_left();
        let inputs = vec![
            Input {
                throttle: if left <= 0.3 { 1.0 } else { 0.0 },
                ..Default::default()
            }, // perfect
            Input::go(),      // held all along
            Input::default(), // nothing
        ];
        w.step(&inputs);
        ev.extend(w.drain_events());
    }
    assert_eq!(w.tick, go);
    let nums: Vec<u8> = ev
        .iter()
        .filter_map(|e| {
            if let Event::Countdown { n } = e {
                Some(*n)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(nums, vec![3, 2, 1]);
    assert_eq!(count(&ev, |e| matches!(e, Event::Go)), 1);
    assert!(ev.contains(&Event::StartBoost { racer: 0 }));
    assert!(ev.contains(&Event::Stumble { racer: 1 }));
    assert!(!ev.iter().any(|e| matches!(
        e,
        Event::StartBoost { racer: 2 } | Event::Stumble { racer: 2 }
    )));
    assert!(w.racers[0].boost_t > 0.9);
    assert!(w.racers[1].stumble_t > 0.5);
    // Nobody moved during the countdown.
    assert!(w.racers.iter().all(|r| r.vel.length() < 1e-6));
    assert_eq!(w.race_time(), 0.0);
}

#[test]
fn legacy_bash_input_does_not_hit_steal_or_drop_coins() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 40.0, -2.0, 20.0);
    place(&mut w, 1, 40.0, 1.0, 20.0);
    w.racers[1].coins = 5;
    w.give_item(1, Item::RollingEgg, 1);
    let mut inputs = idle(2);
    inputs[0].bash = true;
    w.step(&inputs);
    assert!(!w.drain_events().iter().any(|e| matches!(e, Event::Bash { .. } | Event::BashWhiff { .. } | Event::CoinsDropped { .. })));
    assert_eq!(w.racers[1].coins, 5);
    assert_eq!(w.racers[1].item, Some(Item::RollingEgg));
    assert_eq!(w.racers[0].item, None);
    assert_eq!(w.racers[1].slow_t, 0.0);
}

#[test]
fn removed_bash_does_not_consume_a_shield() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 40.0, -2.0, 20.0);
    place(&mut w, 1, 40.0, 1.0, 20.0);
    w.racers[1].shield_t = 5.0;
    let mut inputs = idle(2);
    inputs[0].bash = true;
    w.step(&inputs);
    assert!(w.racers[1].shield_t > 4.9);
    assert_eq!(w.racers[0].bash_cd, 0.0);
    assert!(!w.drain_events().iter().any(|e| matches!(e, Event::ShieldBlock { .. } | Event::Bash { .. })));
}

#[test]
fn racers_bump_apart_but_can_jump_over() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 40.0, 0.0, 20.0);
    place(&mut w, 1, 41.0, 0.3, 0.0);
    w.step(&idle(2));
    let d = (w.racers[0].pos - w.racers[1].pos) * Vec3::new(1.0, 0.0, 1.0);
    assert!(
        d.length() >= 2.0 * w.tun.racer_radius - 1e-3,
        "separated: {}",
        d.length()
    );
    assert!(w.racers[1].vel.length() > 1.0, "rear-ended racer pushed");
    // One 1.5 m up: no contact.
    place(&mut w, 0, 100.0, 0.0, 0.0);
    place(&mut w, 1, 100.5, 0.0, 0.0);
    w.racers[1].pos.y += 1.5;
    w.racers[1].on_ground = false;
    let before = w.racers[0].pos;
    w.step(&idle(2));
    assert!((w.racers[0].pos - before).length() < 0.1);
}

/// First target row of the Savanna and a spot 25 m before it.
#[test]
fn contact_eggs_grant_an_item_then_race_bitcoin() {
    let mut w = humans(TrackId::Savanna, 1, 1);
    skip_countdown(&mut w);
    let ids: Vec<_> = w.targets.iter().enumerate().filter(|(_, t)| !t.bad).take(2).map(|(i, _)| i).collect();
    for (n, k) in ids.into_iter().enumerate() {
        let pr = w.track.project_global(w.targets[k].pos);
        place(&mut w, 0, pr.s, pr.lateral, 0.0);
        w.targets[k].alive = true;
        w.step(&[]);
        assert!(!w.targets[k].alive);
        assert!(w.racers[0].item.is_some());
        assert_eq!(w.racers[0].coins, n as u32);
    }
}


#[test]
fn laser_dazzles_an_opponent_once_per_second() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    w.tun.practice_lasers = true; // focused practice-beam collision test
    skip_countdown(&mut w);
    // Savanna straight from the start: 126 m long.
    place(&mut w, 0, 10.0, 0.0, 0.0);
    place(&mut w, 1, 30.0, 0.0, 0.0);
    let ev = run(&mut w, 60, |_| {
        vec![
            Input {
                laser: true,
                ..Default::default()
            },
            Input::default(),
        ]
    });
    let zaps = count(&ev, |e| matches!(e, Event::LaserZap { by: 0, victim: 1 }));
    assert_eq!(zaps, 1, "dazzled once in the first second (immunity)");
    assert!(
        w.racers[1].slow_t > 0.0
            || ev.iter().any(|e| matches!(
                e,
                Event::Hit {
                    victim: 1,
                    cause: HitCause::Laser,
                    ..
                }
            ))
    );
}

fn use_item(w: &mut World, i: usize) -> Vec<Event> {
    let n = w.racers.len();
    let mut inp = idle(n);
    inp[i].item = true;
    w.step(&inp);
    let mut ev = w.drain_events();
    ev.extend(run(w, 1, |w| idle(w.racers.len())));
    ev
}

#[test]
fn feather_boosts_per_use() {
    let mut w = humans(TrackId::Savanna, 1, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 20.0, 0.0, 20.0);
    w.give_item(0, Item::Feather, 3);
    let ev = use_item(&mut w, 0);
    assert!(ev.contains(&Event::ItemUsed {
        racer: 0,
        item: Item::Feather
    }));
    assert!(w.racers[0].boost_t > 1.3);
    assert_eq!(
        (w.racers[0].item, w.racers[0].item_uses),
        (Some(Item::Feather), 2)
    );
    use_item(&mut w, 0);
    use_item(&mut w, 0);
    assert_eq!(w.racers[0].item, None);
}

#[test]
fn rolling_egg_spins_the_racer_ahead() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 10.0, 0.0, 0.0);
    place(&mut w, 1, 40.0, 0.0, 0.0);
    w.racers[1].coins = 5;
    w.give_item(0, Item::RollingEgg, 1);
    let mut ev = use_item(&mut w, 0);
    assert_eq!(w.projectiles.len(), 1);
    ev.extend(run(&mut w, 60, |_| idle(2)));
    assert!(
        ev.contains(&Event::Hit {
            victim: 1,
            by: Some(0),
            cause: HitCause::RollingEgg
        }),
        "{ev:?}"
    );
    assert!(w.racers[1].stun_t > 0.0);
    assert_eq!(w.racers[1].coins, 2);
    assert!(w.projectiles.is_empty());
}

#[test]
fn rolling_egg_fires_backwards_while_looking_back() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 60.0, 0.0, 0.0);
    place(&mut w, 1, 35.0, 0.0, 0.0);
    w.give_item(0, Item::RollingEgg, 1);
    let ev = run(&mut w, 70, |w| {
        let mut i = idle(2);
        i[0].look_back = true;
        i[0].item = w.tick % 70 == 60;
        i
    });
    assert!(
        ev.contains(&Event::Hit {
            victim: 1,
            by: Some(0),
            cause: HitCause::RollingEgg
        }),
        "{ev:?}"
    );
}

#[test]
fn hunter_egg_homes_on_the_racer_ahead() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 100.0, 0.0, 0.0);
    place(&mut w, 1, 300.0, 3.0, 0.0);
    w.tun.accel = 0.0; // stationary victim isolates the homing path
    w.step(&idle(2));
    assert_eq!(w.racers[0].place, 2);
    w.give_item(0, Item::HunterEgg, 1);
    let mut ev = use_item(&mut w, 0);
    assert_eq!(w.projectiles[0].target, Some(1));
    ev.extend(run(&mut w, 300, |_| idle(2)));
    assert!(
        ev.contains(&Event::Hit {
            victim: 1,
            by: Some(0),
            cause: HitCause::HunterEgg
        }),
        "{ev:?}"
    );
}

#[test]
fn cactus_trips_the_racer_behind() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 60.0, 0.0, 0.0);
    place(&mut w, 1, 20.0, 0.0, 20.0);
    w.give_item(0, Item::Cactus, 1);
    let mut ev = use_item(&mut w, 0);
    assert_eq!(w.hazards.len(), 1);
    // Racer 0 moves on; racer 1 runs into the cactus.
    ev.extend(run(&mut w, 150, |_| {
        vec![
            Input {
                steer: 0.0,
                throttle: 0.0,
                ..Default::default()
            },
            Input::go(),
        ]
    }));
    assert!(
        ev.contains(&Event::Hit {
            victim: 1,
            by: Some(0),
            cause: HitCause::Cactus
        }),
        "{ev:?}"
    );
    assert!(w.hazards.is_empty());
}

#[test]
fn shield_blocks_one_egg() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 10.0, 0.0, 0.0);
    place(&mut w, 1, 40.0, 0.0, 0.0);
    w.give_item(1, Item::Shield, 1);
    let ev = use_item(&mut w, 1);
    assert!(ev.contains(&Event::ShieldUp { racer: 1 }));
    assert!(close(w.racers[1].shield_t, 8.0 - DT, 0.05));
    w.give_item(0, Item::RollingEgg, 1);
    let mut ev = use_item(&mut w, 0);
    ev.extend(run(&mut w, 60, |_| idle(2)));
    assert!(ev.contains(&Event::ShieldBlock {
        racer: 1,
        by: Some(0)
    }));
    assert_eq!(w.racers[1].stun_t, 0.0);
    assert_eq!(w.racers[1].shield_t, 0.0);
}

#[test]
fn lightning_zaps_everyone_ahead() {
    let mut w = humans(TrackId::Savanna, 4, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 50.0, 0.0, 0.0);
    place(&mut w, 1, 200.0, 0.0, 10.0);
    place(&mut w, 2, 300.0, 0.0, 10.0);
    place(&mut w, 3, 20.0, 0.0, 0.0);
    w.step(&idle(4));
    w.give_item(1, Item::Feather, 3);
    w.racers[2].shield_t = 3.0;
    w.give_item(0, Item::Lightning, 1);
    let ev = use_item(&mut w, 0);
    assert!(ev.contains(&Event::Lightning { by: 0 }));
    assert!(ev.contains(&Event::Hit {
        victim: 1,
        by: Some(0),
        cause: HitCause::Lightning
    }));
    assert!(ev.contains(&Event::ShieldBlock {
        racer: 2,
        by: Some(0)
    }));
    assert!(w.racers[1].slow_t > 2.0 && close(w.racers[1].slow_mult, 0.65, 1e-6));
    assert_eq!(w.racers[1].item, None);
    assert_eq!(w.racers[3].slow_t, 0.0, "racer behind untouched");
    assert_eq!(w.racers[0].slow_t, 0.0, "user untouched");
}

#[test]
fn laser_eyes_item_burns_a_rival_and_leaves_eggs_alone() {
    let mut w = humans(TrackId::Savanna, 2, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 10.0, 0.0, 0.0);
    place(&mut w, 1, 30.0, 0.0, 0.0);
    for t in &mut w.targets { t.alive = true; }
    w.give_item(0, Item::MegaLaser, 1);
    let mut ev = use_item(&mut w, 0);
    ev.extend(run(&mut w, 30, |_| idle(2)));
    assert!(ev.contains(&Event::MegaLaser { racer: 0 }));
    assert!(ev.contains(&Event::LaserZap { by: 0, victim: 1 }));
    assert!(w.racers[1].effects.burn > 3.0);
    assert_eq!(w.racers[1].stun_t, 0.0);
    assert!(w.targets.iter().all(|t| t.alive));
    assert!(!ev.iter().any(|e| matches!(e, Event::TargetDestroyed { .. })));
}


#[test]
fn magnet_pulls_coins_in() {
    let line = TrackId::Savanna
        .def()
        .features
        .iter()
        .find_map(|f| {
            if let Feature::CoinLine { s, lateral, .. } = *f {
                Some((s, lateral))
            } else {
                None
            }
        })
        .unwrap();
    let mut w = humans(TrackId::Savanna, 1, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, line.0 + 6.0, line.1 + 8.0, 0.0);
    w.give_item(0, Item::Magnet, 1);
    let mut ev = use_item(&mut w, 0);
    ev.extend(run(&mut w, 120, |_| idle(1)));
    let picked = count(&ev, |e| matches!(e, Event::CoinPicked { racer: 0, .. }));
    assert!(picked >= 4, "magnet collected {picked}");
}

#[test]
fn coins_keep_accumulating_give_a_mini_boost_and_respawn() {
    let line = TrackId::Savanna
        .def()
        .features
        .iter()
        .find_map(|f| {
            if let Feature::CoinLine {
                s, lateral, count, ..
            } = *f
            {
                Some((s, lateral, count))
            } else {
                None
            }
        })
        .unwrap();
    let mut w = humans(TrackId::Savanna, 1, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, line.0 - 10.0, line.1, 15.0);
    w.racers[0].coins = 8;
    let mut boosted = false;
    let mut ev = Vec::new();
    for _ in 0..90 {
        w.step(&[Input {
            throttle: 0.3,
            ..Default::default()
        }]);
        let e = w.drain_events();
        if e.iter().any(|e| matches!(e, Event::CoinPicked { .. })) {
            boosted |= w.racers[0].boost_t > 0.1;
        }
        ev.extend(e);
    }
    let picked = count(&ev, |e| matches!(e, Event::CoinPicked { racer: 0, .. }));
    assert_eq!(picked, line.2 as usize);
    assert!(boosted);
    assert_eq!(w.racers[0].coins, 8 + picked as u32, "all race coins are retained");
    let dead = w.coins.iter().filter(|c| !c.alive).count();
    assert_eq!(dead, line.2 as usize);
    let n = (w.tun.coin_respawn / DT) as usize + 2;
    run(&mut w, n, |_| idle(1));
    assert!(w.coins.iter().all(|c| c.alive));
}

#[test]
fn wrong_way_flag() {
    let mut w = humans(TrackId::Savanna, 1, 1);
    skip_countdown(&mut w);
    place(&mut w, 0, 100.0, 0.0, 0.0);
    w.racers[0].yaw += std::f32::consts::PI;
    let ev = run(&mut w, 150, |_| vec![Input::go()]);
    assert!(ev.contains(&Event::WrongWay { racer: 0, on: true }));
    assert!(w.racers[0].wrong_way);
}

#[test]
fn a_bot_driven_human_finishes_three_laps_and_the_race_ends() {
    let mut w = World::new(RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 5));
    let mut ev = Vec::new();
    while w.phase != RacePhase::Finished && w.tick < 60 * 300 {
        let me = bot_input(&w, 0, BotLevel::Hard);
        w.step(&[me]);
        ev.extend(w.drain_events());
    }
    assert_eq!(w.phase, RacePhase::Finished);
    assert_eq!(w.lap_times[0].len(), 3);
    let fin = ev.iter().find_map(|e| {
        if let Event::Finished {
            racer: 0,
            place,
            time,
        } = e
        {
            Some((*place, *time))
        } else {
            None
        }
    });
    let (place, time) = fin.expect("human finished");
    assert_eq!(w.racers[0].place, place);
    assert!(close(w.lap_times[0].iter().sum::<f32>(), time, 0.05));
    assert_eq!(count(&ev, |e| matches!(e, Event::FinalLap { racer: 0 })), 1);
    assert_eq!(count(&ev, |e| matches!(e, Event::Lap { racer: 0, .. })), 2);
    assert_eq!(count(&ev, |e| matches!(e, Event::RaceOver)), 1);
    let st = w.standings();
    assert_eq!(st.len(), 4);
    for (p, &i) in st.iter().enumerate() {
        assert_eq!(w.racers[i].place as usize, p + 1);
    }
    // The world keeps running (finished racers drive on under bot control).
    let s0 = w.racers[0].s;
    run(&mut w, 60, |_| vec![Input::default()]);
    assert!((w.racers[0].s - s0).abs() > 5.0);
}

#[test]
fn race_waits_for_a_delayed_human_beyond_the_old_grace_period() {
    let racers = vec![
        RacerSetup {
            name: "A".into(),
            character: 0,
            kind: RacerKind::Human,
        },
        RacerSetup {
            name: "B".into(),
            character: 1,
            kind: RacerKind::Human,
        },
    ];
    let mut w = World::new(RaceConfig {
        track: TrackId::Savanna,
        laps: 1,
        seed: 3,
        mode: RaceMode::Race,
        racers,
        drill: None,
        attire: [[0; 3]; 8],
        le_mans: false,
    });
    while w.phase != RacePhase::Finished && w.tick < 60 * 200 {
        let a = bot_input(&w, 0, BotLevel::Hard);
        w.racers[1].effects.tumble = 999.0; // a delayed racer cannot finish before the grace timer
        w.step(&[a, Input::default()]);
    }
    assert_eq!(w.phase, RacePhase::Racing);
    assert!(w.race_time() > w.first_human_finish.unwrap() + 25.0);
    assert!(w.ended_at.is_none());
    assert_eq!(w.racers[0].place, 1);
    assert_eq!(w.racers[1].place, 2);
    assert!(w.racers[1].finished_at.is_none());
}

#[test]
fn attract_mode_never_ends() {
    let mut cfg = RaceConfig::all_bots(TrackId::Midnight, 4, BotLevel::Normal, 2);
    cfg.mode = RaceMode::Attract;
    cfg.laps = 1;
    let mut w = World::new(cfg);
    run(&mut w, 60 * 100, |_| vec![]);
    assert_eq!(w.phase, RacePhase::Racing);
    assert!(w
        .racers
        .iter()
        .all(|r| r.finished_at.is_none() && r.lap >= 2));
}

fn digest(w: &World) -> Vec<u8> {
    bincode::serialize(w).unwrap()
}

#[test]
fn deterministic_for_seed_and_inputs() {
    let make = || World::new(RaceConfig::quick(TrackId::Dune, 7, BotLevel::Normal, 42));
    let (mut a, mut b) = (make(), make());
    let mut seq = 7u32;
    let mut ea = 0;
    let mut eb = 0;
    for _ in 0..3000 {
        seq = seq.wrapping_mul(1664525).wrapping_add(1013904223);
        let mut inp = Input::unpack(seq.to_le_bytes());
        inp.throttle = 1.0;
        inp.brake = false;
        a.step(&[inp]);
        b.step(&[inp]);
        ea += a.drain_events().len();
        eb += b.drain_events().len();
    }
    assert_eq!(ea, eb);
    assert_eq!(digest(&a), digest(&b));
    // A different seed diverges.
    let mut c = World::new(RaceConfig::quick(TrackId::Dune, 7, BotLevel::Normal, 43));
    for _ in 0..3000 {
        c.step(&[Input::go()]);
    }
    assert_ne!(digest(&a), digest(&c));
}

#[test]
fn world_serde_roundtrip() {
    let mut w = World::new(RaceConfig::all_bots(TrackId::Canyon, 4, BotLevel::Hard, 9));
    run(&mut w, 600, |_| vec![]);
    let bytes = bincode::serialize(&w).unwrap();
    let mut back: World = bincode::deserialize(&bytes).unwrap();
    assert_eq!(digest(&back), bytes);
    // And the copy keeps stepping identically.
    run(&mut w, 300, |_| vec![]);
    run(&mut back, 300, |_| vec![]);
    assert_eq!(digest(&w), digest(&back));
}

#[test]
fn racer_net_is_compact_and_round_trips() {
    let mut w = World::new(RaceConfig::all_bots(
        TrackId::Midnight,
        8,
        BotLevel::Hard,
        11,
    ));
    run(&mut w, 60 * 20, |_| vec![]);
    let r = &w.racers[3];
    let net = r.to_net();
    let size = bincode::serialized_size(&net).unwrap();
    // Two extra bytes carry the printer/chase mode and its ground offset, one the Laser Trump timer.
    assert!(size <= 56, "RacerNet is {size} bytes");
    let mut copy = Racer::new(0, r.character, Vec3::ZERO, 0.0, 0);
    copy.apply_net(&net);
    assert_eq!(copy.idx, r.idx);
    assert!((copy.pos - r.pos).length() < 1e-4);
    assert!((copy.vel - r.vel).length() < 0.02);
    assert!(math::wrap_angle(copy.yaw - r.yaw).abs() < 1e-3);
    assert!(math::wrap_angle(copy.visual_yaw - r.visual_yaw).abs() < 0.01);
    assert_eq!(
        (
            copy.on_ground,
            copy.drift.active,
            copy.drift.tier,
            copy.drift.dir
        ),
        (r.on_ground, r.drift.active, r.drift.tier, r.drift.dir)
    );
    assert_eq!(
        (copy.coins, copy.lap, copy.place),
        (r.coins, r.lap, r.place)
    );
    assert!(close(copy.boost_t, r.boost_t.min(2.55), 0.01));
    // The snapshot budget: 7 others + a full own racer + some eggs in 1024 bytes.
    let own = bincode::serialized_size(&w.racers[0]).unwrap();
    let eggs = 5 * bincode::serialized_size(&Projectile {
        kind: ProjectileKind::RollingEgg,
        pos: Vec3::ZERO,
        vel: Vec3::ZERO,
        owner: 0,
        target: None,
        age: 0.0,
        bounces: 0,
        hint: 0,
        s: 0.0,
        lateral: 0.0,
        id: 0,
    })
    .unwrap();
    let total = 7 * size + own + eggs;
    eprintln!("RacerNet {size} B, own Racer {own} B, 5 eggs {eggs} B, total {total} B");
    // Fixed-width lengths and tags here: the item queue's length alone is 8 B
    // (1 B on the varint wire, where race-session's budget test is exact).
    assert!(
        total <= 1010,
        "snapshot payload {total} bytes (own racer {own})"
    );
}

#[test]
fn laser_beams_stop_at_walls() {
    let mut w = humans(TrackId::Savanna, 1, 1);
    w.tun.practice_lasers = true; // focused practice-beam collision test
    skip_countdown(&mut w);
    place(&mut w, 0, 40.0, 0.0, 0.0);
    // Face the right-hand wall.
    w.racers[0].yaw -= std::f32::consts::FRAC_PI_2;
    w.step(&[Input {
        laser: true,
        ..Default::default()
    }]);
    let r = &w.racers[0];
    assert_eq!(r.laser.hit_kind, LaserHit::Wall);
    assert!((r.laser.hit - r.laser.origin).length() < 20.0);
}

#[test]
fn hurdles_trip_racers_on_the_ground_only() {
    let mut w = World::new(RaceConfig::training(LessonId::Jump, RacerKind::Human, 1));
    skip_countdown(&mut w);
    let hurdle = w
        .track
        .features
        .iter()
        .find_map(|f| {
            if let Feature::Hurdle { s } = *f {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();
    place(&mut w, 0, hurdle - 20.0, 0.0, 20.0);
    let ev = run(&mut w, 70, |_| vec![Input::go()]);
    assert!(ev.contains(&Event::Stumble { racer: 0 }), "{ev:?}");
    // Jumping clears it.
    place(&mut w, 0, hurdle - 20.0, 0.0, 20.0);
    let ev = run(&mut w, 70, |w| {
        let d = w.track.s_delta(w.racers[0].s, hurdle);
        vec![Input {
            throttle: 1.0,
            jump: d > 2.0 && d < 7.0,
            ..Default::default()
        }]
    });
    assert!(ev.contains(&Event::HurdleCleared { racer: 0 }), "{ev:?}");
}
