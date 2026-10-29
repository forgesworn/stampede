//! Battle mode "Feather Frenzy" (spec 17): feather loss per cause, steals and
//! their cap, invulnerability, going out, the end (one left / the clock) and
//! its ranking, the arenas, and all-bot battles that finish and actually fight.

mod common;
use common::*;
use race_core::math::DT;
use race_core::*;

fn battle_humans(arena: TrackId, n: usize, rules: BattleRules) -> World {
    let racers = (0..n)
        .map(|i| RacerSetup {
            name: format!("P{i}"),
            character: i as u8,
            kind: RacerKind::Human,
        })
        .collect();
    let mut w = World::new(RaceConfig::battle(arena, racers, rules, 5));
    skip_countdown(&mut w);
    w
}

fn idle(n: usize) -> Vec<Input> {
    vec![Input::default(); n]
}

fn press(n: usize, who: usize, f: impl Fn(&mut Input)) -> Vec<Input> {
    let mut v = idle(n);
    f(&mut v[who]);
    v
}

fn step_n(w: &mut World, ticks: usize) -> Vec<Event> {
    let n = w.racers.len();
    run(w, ticks, |_| idle(n))
}

fn feathers(w: &World) -> Vec<u8> {
    w.battle.as_ref().unwrap().feathers.clone()
}

fn lost(ev: &[Event]) -> Vec<(u8, Option<u8>)> {
    ev.iter()
        .filter_map(|e| match e {
            Event::FeatherLost { racer, by } => Some((*racer, *by)),
            _ => None,
        })
        .collect()
}

fn egg_on(w: &mut World, victim: usize, owner: u8, kind: ProjectileKind, id: u32) {
    let r = &w.racers[victim];
    let p = Projectile {
        kind,
        pos: r.body(),
        vel: Vec3::ZERO,
        owner,
        target: Some(victim as u8),
        age: 1.0,
        bounces: 0,
        hint: r.track_idx,
        s: r.s,
        lateral: r.lateral,
        id,
    };
    w.projectiles.push(p);
}

fn cactus_on(w: &mut World, victim: usize, owner: u8) {
    let pos = w.racers[victim].pos;
    w.hazards.push(Hazard {
        kind: HazardKind::Cactus,
        pos,
        owner,
        age: 2.0,
        id: 77,
    });
}

#[test]
fn arenas_are_wide_short_loops_full_of_eggs() {
    for a in TrackId::ARENAS {
        let tr = a.track();
        assert!(
            (300.0..=420.0).contains(&tr.length),
            "{a:?}: {} m",
            tr.length
        );
        assert!(tr.samples.iter().all(|s| s.width >= 38.0 && s.width <= 46.0));
        let w = World::new(RaceConfig::battle_bots(a, 8, BotLevel::Normal, BattleRules::default(), 1));
        assert!(w.targets.len() >= 30, "{a:?}: {} eggs", w.targets.len());
        assert!(w.coins.len() >= 30, "{a:?}: {} coins", w.coins.len());
        assert!(tr.features.iter().any(|f| matches!(f, Feature::Ramp { .. })));
        assert!(!tr
            .features
            .iter()
            .any(|f| matches!(f, Feature::Hurdle { .. } | Feature::Gate { .. } | Feature::Gap { .. })));
        // Racers start spread round the loop.
        let mut ss: Vec<f32> = w.racers.iter().map(|r| r.s).collect();
        ss.sort_by(f32::total_cmp);
        for k in 1..ss.len() {
            assert!(ss[k] - ss[k - 1] > 20.0, "{a:?}: start spacing {ss:?}");
        }
    }
    assert!(TrackId::MempoolPlaza.track().theme.night);
    assert!(TrackId::MempoolPlaza
        .track()
        .features
        .iter()
        .filter(|f| matches!(f, Feature::BoostPad { .. }))
        .count()
        >= 6);
    assert!(!TrackId::DustBowl.track().theme.night);
}

#[test]
fn every_track_keeps_its_eggs_inside_the_net_bitmask() {
    for id in TrackId::ALL {
        let w = World::new(RaceConfig::quick(id, 0, BotLevel::Normal, 1));
        assert!(w.targets.len() <= 64, "{id:?}: {} targets", w.targets.len());
    }
}

#[test]
fn battle_state_is_set_up_from_the_rules() {
    let rules = BattleRules {
        feathers: 4,
        time_limit: 90,
        ..BattleRules::default()
    };
    let w = World::new(RaceConfig::battle_bots(TrackId::DustBowl, 6, BotLevel::Normal, rules, 3));
    let b = w.battle.as_ref().expect("battle state");
    assert_eq!(b.feathers, vec![4; 6]);
    assert_eq!(b.time_left, 90.0);
    assert!(!b.done && b.out_order.is_empty());
    assert_eq!(w.cfg.mode, RaceMode::Battle(rules));
    assert_eq!(w.cfg.mode.battle_rules(), Some(rules));
    assert_eq!(RaceMode::Race.battle_rules(), None);
    // No battle state outside battles.
    assert!(World::new(RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 1)).battle.is_none());
    // Power Eggs respawn in 3 s.
    assert!((w.tun.target_respawn - 3.0).abs() < 1e-6);
}

#[test]
fn every_attack_pops_a_feather_and_credits_the_attacker() {
    // Rolling Egg, Hunter Egg, Cactus (someone else's and your own).
    for (k, kind) in [ProjectileKind::RollingEgg, ProjectileKind::HunterEgg].into_iter().enumerate() {
        let mut w = battle_humans(TrackId::DustBowl, 4, BattleRules::default());
        egg_on(&mut w, 1, 2, kind, 10 + k as u32);
        let ev = step_n(&mut w, 2);
        assert_eq!(lost(&ev), vec![(1, Some(2))], "{kind:?}");
        assert!(ev.iter().any(|e| matches!(e, Event::Hit { victim: 1, .. })));
        assert_eq!(feathers(&w), vec![3, 2, 3, 3]);
        let b = w.battle.as_ref().unwrap();
        assert_eq!((b.hits[2], b.taken[1]), (1, 1));
    }
    let mut w = battle_humans(TrackId::DustBowl, 4, BattleRules::default());
    cactus_on(&mut w, 3, 0);
    let ev = step_n(&mut w, 1);
    assert_eq!(lost(&ev), vec![(3, Some(0))]);
    let mut w = battle_humans(TrackId::DustBowl, 4, BattleRules::default());
    cactus_on(&mut w, 3, 3);
    let ev = step_n(&mut w, 1);
    assert_eq!(lost(&ev), vec![(3, None)], "own cactus: no credit");
    assert_eq!(w.battle.as_ref().unwrap().hits, vec![0; 4]);

    // Mega Laser: the victim 12 m straight ahead.
    let mut w = battle_humans(TrackId::DustBowl, 4, BattleRules::default());
    let (s0, l0) = (w.racers[0].s, w.racers[0].lateral);
    place(&mut w, 1, s0 + 12.0, l0, 0.0);
    w.give_item(0, Item::MegaLaser, 1);
    let mut ev = run(&mut w, 1, |w| press(w.racers.len(), 0, |i| i.item = true));
    ev.extend(step_n(&mut w, 30));
    assert!(ev.iter().any(|e| matches!(e, Event::MegaLaser { racer: 0 })));
    assert_eq!(lost(&ev), vec![(1, Some(0))]);

    // Lightning: everyone else.
    let mut w = battle_humans(TrackId::MempoolPlaza, 4, BattleRules::default());
    w.give_item(2, Item::Lightning, 1);
    let ev = run(&mut w, 1, |w| press(w.racers.len(), 2, |i| i.item = true));
    let mut got = lost(&ev);
    got.sort();
    assert_eq!(got, vec![(0, Some(2)), (1, Some(2)), (3, Some(2))]);
    assert_eq!(feathers(&w), vec![2, 2, 3, 2]);
    assert_eq!(w.battle.as_ref().unwrap().hits[2], 3);
}

fn side_by_side(w: &mut World) {
    let (s0, l0) = (w.racers[0].s, w.racers[0].lateral);
    place(w, 0, s0, l0, 0.0);
    place(w, 1, s0, l0 + 2.0, 0.0);
}

#[test]
fn legacy_bash_input_cannot_pop_or_steal_a_feather() {
    let mut w = battle_humans(TrackId::DustBowl, 3, BattleRules::default());
    side_by_side(&mut w);
    let ev = run(&mut w, 1, |w| press(w.racers.len(), 0, |i| i.bash = true));
    assert!(lost(&ev).is_empty());
    assert!(!ev.iter().any(|e| matches!(e, Event::Bash { .. } | Event::FeatherStolen { .. })));
    assert_eq!(feathers(&w), vec![3, 3, 3]);
}

#[test]
fn laser_dazzle_and_shields_do_not_pop_feathers() {
    let mut w = battle_humans(TrackId::DustBowl, 2, BattleRules::default());
    w.tun.practice_lasers = true; // legacy practice beam, separate from the item
    w.targets.clear(); // nothing else in the cone
    let (s0, l0) = (w.racers[0].s, w.racers[0].lateral);
    place(&mut w, 1, s0 + 15.0, l0, 0.0);
    let ev = run(&mut w, 60, |w| press(w.racers.len(), 0, |i| i.laser = true));
    assert!(ev.iter().any(|e| matches!(e, Event::LaserZap { by: 0, victim: 1 })), "dazzled");
    assert!(lost(&ev).is_empty());
    assert_eq!(feathers(&w), vec![3, 3]);

    let mut w = battle_humans(TrackId::DustBowl, 2, BattleRules::default());
    w.racers[1].shield_t = 5.0;
    egg_on(&mut w, 1, 0, ProjectileKind::RollingEgg, 5);
    let ev = step_n(&mut w, 2);
    assert!(ev.iter().any(|e| matches!(e, Event::ShieldBlock { racer: 1, .. })));
    assert!(lost(&ev).is_empty());
    assert_eq!(feathers(&w), vec![3, 3]);
}

#[test]
fn invulnerability_lets_attacks_pass_for_one_and_a_half_seconds() {
    let mut w = battle_humans(TrackId::DustBowl, 3, BattleRules::default());
    egg_on(&mut w, 1, 0, ProjectileKind::RollingEgg, 1);
    let ev = step_n(&mut w, 1);
    assert_eq!(lost(&ev).len(), 1);
    assert!(w.battle.as_ref().unwrap().flashing(1));
    // A second egg and a cactus pass through while it flashes; the shield is not used up.
    w.racers[1].shield_t = 5.0;
    egg_on(&mut w, 1, 2, ProjectileKind::RollingEgg, 2);
    let ev = step_n(&mut w, 1);
    assert!(lost(&ev).is_empty());
    assert!(w.projectiles.iter().any(|p| p.id == 2), "the egg passed through");
    w.projectiles.clear();
    cactus_on(&mut w, 1, 2);
    let ev = step_n(&mut w, 1);
    assert!(lost(&ev).is_empty());
    assert_eq!(w.hazards.len(), 1, "the cactus is still there");
    assert!(w.racers[1].shield_t > 0.0, "shield untouched");
    w.hazards.clear();
    w.projectiles.clear();
    w.racers[1].shield_t = 0.0;
    // Still flashing just before 1.5 s, not after.
    step_n(&mut w, (1.5 / DT) as usize - 4);
    assert!(w.battle.as_ref().unwrap().flashing(1));
    step_n(&mut w, 4);
    assert!(!w.battle.as_ref().unwrap().flashing(1));
    egg_on(&mut w, 1, 2, ProjectileKind::RollingEgg, 3);
    let ev = step_n(&mut w, 1);
    assert_eq!(lost(&ev), vec![(1, Some(2))]);
    assert_eq!(feathers(&w)[1], 1);
}

#[test]
fn at_zero_feathers_a_racer_is_out_frozen_and_fades() {
    let mut w = battle_humans(TrackId::MempoolPlaza, 4, BattleRules::default());
    w.battle.as_mut().unwrap().feathers[2] = 1;
    egg_on(&mut w, 2, 0, ProjectileKind::RollingEgg, 1);
    let ev = step_n(&mut w, 1);
    let out: Vec<&Event> = ev.iter().filter(|e| matches!(e, Event::Out { .. })).collect();
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], Event::Out { racer: 2, place: 4, .. }));
    assert!(w.is_out(2) && !w.is_out(0));
    assert_eq!(w.battle.as_ref().unwrap().out_order, vec![2]);
    assert_eq!(w.racers[2].place, 4);
    let pos = w.racers[2].pos;
    assert!(w.battle.as_ref().unwrap().fade(2) > 0.95);
    // Frozen where it went out, and out of every interaction.
    step_n(&mut w, 60);
    assert_eq!(w.racers[2].pos, pos);
    assert!(w.racers[2].respawn_t > 0.0);
    let f = w.battle.as_ref().unwrap().fade(2);
    assert!(f > 0.25 && f < 0.42, "fade after 1 s: {f}");
    step_n(&mut w, 31);
    assert_eq!(w.battle.as_ref().unwrap().fade(2), 0.0);
    egg_on(&mut w, 2, 0, ProjectileKind::RollingEgg, 9);
    let ev = step_n(&mut w, 1);
    assert!(lost(&ev).is_empty());
    assert_eq!(*w.standings().last().unwrap(), 2);
    assert_eq!(w.phase, RacePhase::Racing, "three left: still on");
}

#[test]
fn the_battle_ends_when_one_is_left() {
    let mut w = battle_humans(TrackId::DustBowl, 2, BattleRules::default());
    w.battle.as_mut().unwrap().feathers[0] = 1;
    egg_on(&mut w, 0, 1, ProjectileKind::HunterEgg, 1);
    let ev = step_n(&mut w, 2);
    let names: Vec<&Event> = ev
        .iter()
        .filter(|e| matches!(e, Event::Out { .. } | Event::BattleOver | Event::RaceOver))
        .collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(matches!(names[0], Event::Out { racer: 0, place: 2, .. }));
    assert_eq!(*names[1], Event::BattleOver);
    assert_eq!(*names[2], Event::RaceOver);
    assert_eq!(w.phase, RacePhase::Finished);
    assert!(w.battle.as_ref().unwrap().done);
    assert_eq!(w.standings(), vec![1, 0]);
    assert_eq!((w.racers[1].place, w.racers[0].place), (1, 2));
    // Done is final: no more feathers change hands.
    egg_on(&mut w, 1, 0, ProjectileKind::RollingEgg, 2);
    let ev = step_n(&mut w, 2);
    assert!(lost(&ev).is_empty());
}

#[test]
fn on_the_clock_the_ranking_is_feathers_then_hits_then_fewest_taken() {
    let rules = BattleRules {
        time_limit: 5,
        ..BattleRules::default()
    };
    let mut w = battle_humans(TrackId::DustBowl, 5, rules);
    {
        let b = w.battle.as_mut().unwrap();
        b.feathers = vec![3, 3, 3, 1, 3];
        b.hits = vec![2, 2, 5, 0, 2];
        b.taken = vec![1, 0, 0, 2, 1];
    }
    let ev = step_n(&mut w, (5.0 / DT) as usize + 5);
    assert!(ev.contains(&Event::BattleOver));
    assert!(ev.contains(&Event::RaceOver));
    let b = w.battle.as_ref().unwrap();
    assert!(b.done);
    assert_eq!(b.time_left, 0.0);
    assert!((w.ended_at.unwrap() - 5.0).abs() < 2.0 * DT);
    // 2 (5 hits), 1 (2 hits, 0 taken), 0 and 4 (2 hits, 1 taken: by index), 3 (1 feather).
    assert_eq!(w.standings(), vec![2, 1, 0, 4, 3]);
    assert_eq!(b.ranking(), w.standings());
    for (p, &i) in w.standings().iter().enumerate() {
        assert_eq!(w.racers[i].place as usize, p + 1);
        assert_eq!(b.place_of(i) as usize, p + 1);
    }
}

#[test]
fn battle_items_and_hunters_ignore_places() {
    // Hunter Egg: homes on the nearest racer ahead along the loop.
    let mut w = battle_humans(TrackId::DustBowl, 4, BattleRules::default());
    w.give_item(0, Item::HunterEgg, 1);
    run(&mut w, 1, |w| press(w.racers.len(), 0, |i| i.item = true));
    let l = w.track.length;
    let ahead = |j: usize| (w.racers[j].s - w.racers[0].s).rem_euclid(l);
    let nearest = (1..4).min_by(|&a, &b| ahead(a).total_cmp(&ahead(b))).unwrap();
    assert_eq!(w.projectiles[0].target, Some(nearest as u8));
    // The battle odds: nothing but battle-useful rolls, mostly eggs.
    let mut rng = rand_pcg::Pcg32::new(1, 2);
    let mut eggs = 0;
    for _ in 0..1000 {
        let r = roll_battle_item(&mut rng);
        if matches!(r.item, Item::RollingEgg | Item::HunterEgg) {
            eggs += 1;
        }
    }
    assert!(eggs > 400, "{eggs}");
}

/// An all-bot battle: returns (seconds, feathers popped, max stuck, alive at the end).
fn bot_battle(arena: TrackId, n: usize, level: BotLevel, seed: u64) -> (f32, usize, f32, usize) {
    let mut w = World::new(RaceConfig::battle_bots(arena, n, level, BattleRules::default(), seed));
    let mut pops = 0;
    let limit = ((3.0 + 200.0) / DT) as u64;
    while w.phase != RacePhase::Finished && w.tick < limit {
        w.step(&[]);
        pops += w
            .drain_events()
            .iter()
            .filter(|e| matches!(e, Event::FeatherLost { .. }))
            .count();
    }
    assert_eq!(w.phase, RacePhase::Finished, "{arena:?} {level:?} seed {seed}: did not end");
    let b = w.battle.as_ref().unwrap();
    assert!(b.done);
    let alive = b.alive().len();
    assert!(alive == 1 || b.time_left <= 0.0, "{arena:?}: ended with {alive} left");
    let stuck = w.max_stuck.iter().cloned().fold(0.0f32, f32::max);
    (w.race_time(), pops, stuck, alive)
}

#[test]
fn all_bot_battles_finish_fight_and_never_get_stuck() {
    let mut runs = vec![];
    for arena in TrackId::ARENAS {
        for seed in 1..=2 {
            runs.push((arena, 8, BotLevel::Normal, seed));
        }
        runs.push((arena, 6, BotLevel::Hard, 3));
        runs.push((arena, 6, BotLevel::Easy, 4));
    }
    for (arena, n, level, seed) in runs {
        let (t, pops, stuck, alive) = bot_battle(arena, n, level, seed);
        let per_min = pops as f32 / t * 60.0;
        eprintln!(
            "{arena:?} {n} x {level:?} seed {seed}: {t:.1} s, {pops} feathers ({per_min:.1}/min), {alive} left, max stuck {stuck:.1} s"
        );
        assert!(stuck < 3.0, "{arena:?} {level:?}: stuck {stuck:.1} s");
        // With bash removed, attacks come only from power-ups.
        assert!(per_min >= 5.0 - 1e-3, "{arena:?} {level:?}: only {per_min:.1} feathers a minute");
    }
}

#[test]
fn one_mega_laser_pops_a_victim_at_most_once() {
    // The beam lasts 2.5 s but the post-hit invulnerability only 1.5 s: a
    // victim held in the beam for the whole activation loses one feather.
    let mut w = battle_humans(TrackId::DustBowl, 3, BattleRules::default());
    let (s0, l0) = (w.racers[0].s, w.racers[0].lateral);
    place(&mut w, 1, s0 + 12.0, l0, 0.0);
    place(&mut w, 2, s0 + 8.0, l0 + 0.8, 0.0);
    w.give_item(0, Item::MegaLaser, 1);
    let mut ev = run(&mut w, 1, |w| press(w.racers.len(), 0, |i| i.item = true));
    ev.extend(step_n(&mut w, 170)); // > 2.5 s, 2.83 s total
    assert!(w.racers[0].mega_t <= 0.0, "beam over");
    let mut got = lost(&ev);
    got.sort();
    assert_eq!(got, vec![(2, Some(0))], "the nearest racer intercepts the forward beam");
    assert_eq!(feathers(&w), vec![3, 3, 2]);
    assert_eq!(w.battle.as_ref().unwrap().hits[0], 1);

    // A second activation hits them again.
    let (s0, l0) = (w.racers[0].s, w.racers[0].lateral);
    place(&mut w, 1, s0 + 12.0, l0, 0.0);
    place(&mut w, 2, s0 + 8.0, l0 + 0.8, 0.0);
    w.racers[1].stun_t = 0.0;
    w.racers[2].stun_t = 0.0;
    w.racers[2].effects.burn = 0.0;
    w.give_item(0, Item::MegaLaser, 1);
    let mut ev = run(&mut w, 1, |w| press(w.racers.len(), 0, |i| i.item = true));
    ev.extend(step_n(&mut w, 10));
    let mut got = lost(&ev);
    got.sort();
    assert_eq!(got, vec![(2, Some(0))], "new activation hits the nearest racer again");
}

#[test]
fn the_last_two_going_out_together_leave_one_winner_who_is_not_out() {
    let mut w = battle_humans(TrackId::DustBowl, 2, BattleRules::default());
    {
        let b = w.battle.as_mut().unwrap();
        b.feathers = vec![1, 1];
    }
    egg_on(&mut w, 0, 1, ProjectileKind::HunterEgg, 1);
    egg_on(&mut w, 1, 0, ProjectileKind::HunterEgg, 2);
    let ev = step_n(&mut w, 2);
    let outs: Vec<&Event> = ev.iter().filter(|e| matches!(e, Event::Out { .. })).collect();
    assert_eq!(outs.len(), 1, "exactly one is out: {outs:?}");
    assert!(ev.contains(&Event::BattleOver));
    let b = w.battle.as_ref().unwrap();
    assert!(b.done);
    assert_eq!(b.alive().len(), 1);
    let winner = b.alive()[0];
    assert_eq!(b.ranking()[0], winner);
    assert_eq!(w.standings()[0] as usize, winner);
    assert_eq!(b.place_of(winner), 1);
    assert_eq!(w.racers[winner].place, 1);
    assert!(!b.is_out(winner));
}

#[test]
fn lightning_in_a_battle_knocks_out_the_best_ranked_last() {
    let mut w = battle_humans(TrackId::DustBowl, 4, BattleRules::default());
    {
        let b = w.battle.as_mut().unwrap();
        b.feathers = vec![3, 1, 1, 1];
        b.hits = vec![0, 0, 5, 0];
    }
    w.give_item(0, Item::Lightning, 1);
    run(&mut w, 1, |w| press(w.racers.len(), 0, |i| i.item = true));
    let b = w.battle.as_ref().unwrap();
    assert_eq!(b.out_order, vec![3, 1, 2], "worst first, the 5-hit racer goes out last");
    assert_eq!(b.ranking(), vec![0, 2, 1, 3]);
}
