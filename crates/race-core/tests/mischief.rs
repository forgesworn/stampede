//! Real collision, pursuit and recovery contracts for the rug and printer.

mod common;
use common::*;
use race_core::*;
use std::sync::Arc;

fn race(n: usize) -> World {
    let mut cfg = RaceConfig::quick(TrackId::Training, 0, BotLevel::Normal, 19);
    cfg.racers = (0..n)
        .map(|i| RacerSetup {
            name: format!("Rider {i}"),
            character: 0,
            kind: RacerKind::Human,
        })
        .collect();
    cfg.mode = RaceMode::TimeTrial;
    let mut world = World::with_track(cfg, Arc::new(proving_ground()));
    skip_countdown(&mut world);
    for i in 0..n {
        place(&mut world, i, 20.0 + i as f32 * 20.0, 0.0, 0.0);
    }
    world
}

fn bill(w: &mut World, owner: usize, victim: usize) {
    let r = &w.racers[victim];
    w.projectiles.push(Projectile {
        kind: ProjectileKind::DollarBill,
        pos: r.body(),
        vel: Vec3::ZERO,
        owner: owner as u8,
        target: Some(victim as u8),
        age: 0.0,
        bounces: 0,
        hint: r.track_idx,
        s: r.s,
        lateral: r.lateral,
        id: 999,
    });
}

#[test]
fn scramble_is_rare_at_every_position_and_every_pool_row_is_a_percentage() {
    let whisk = ITEM_ROLLS
        .iter()
        .position(|r| r.item == Item::EggScrambler)
        .unwrap();
    for row in ITEM_ODDS {
        assert_eq!(row.iter().sum::<u16>(), 100);
        assert_eq!(row[whisk], 3);
    }
}

#[test]
fn laser_button_cannot_fire_without_a_power_up_in_an_ordinary_race() {
    let mut w = race(2);
    let before = w.racers[1].effects.burn;
    for _ in 0..30 {
        w.step(&[Input {
            laser: true,
            ..Input::default()
        }]);
        assert!(!w.racers[0].laser.firing);
    }
    assert_eq!(w.racers[1].effects.burn, before);
    w.give_item(0, Item::MegaLaser, 1);
    w.step(&[Input {
        item: true,
        ..Input::default()
    }]);
    assert!(w.racers[0].mega_t > 0.0);
    w.step(&[]);
    let l = w.racers[0].laser;
    assert!(
        (l.hit - l.origin)
            .normalize_or_zero()
            .dot(w.racers[0].forward())
            > 0.9999
    );
}

#[test]
fn drift_needs_eight_tenths_of_a_second_and_pays_out_by_tier() {
    let tr = arena();
    let tun = Tunables::default();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = Input {
        drift: true,
        steer: 1.0,
        ..Input::default()
    };
    let release = Input { steer: 1.0, ..Input::default() };
    for ticks in [40usize, 80, 200] {
        r.drift = Drift {
            active: true,
            dir: 1,
            carry_q: 130,
            blend_q: 255,
            ..Drift::default()
        };
        r.boost_t = 0.0;
        set_speed(&mut r, 28.0);
        for _ in 0..ticks {
            r.integrate(&hold, &tr, &tun);
        }
        let tier = r.drift.tier;
        let charge = r.drift.charge;
        let fx = r.integrate(&release, &tr, &tun);
        assert_eq!(r.drift.charge, 0.0, "release always clears charge");
        // The boost fires once the slip comes down: within the hold time.
        let mut fired = fx.drift_boost;
        let mut points = fx.drift_points;
        for _ in 0..90 {
            let fx = r.integrate(&release, &tr, &tun);
            if fx.drift_boost > 0 {
                fired = fx.drift_boost;
                points = fx.drift_points;
            }
        }
        assert_eq!(fired, tier, "{ticks} ticks");
        if ticks >= 48 {
            // The charge is the time held (it builds every tick of the slide), and the
            // points are that charge to a tenth of a second, times the tier.
            assert!((charge - ticks as f32 * race_core::math::DT).abs() < 0.05, "charge {charge} after {ticks} ticks");
            assert!(tier >= 1);
            let want = (charge * 10.0).round() * 0.1 * tier as f32;
            assert!((points - want).abs() < 0.01, "points {points}, want {want} (tier {tier})");
            assert!(r.drift.pending == 0 && r.boost_t <= tun.drift_boost_time[tier as usize - 1]);
        } else {
            assert_eq!(tier, 0);
            assert_eq!(r.boost_t, 0.0);
        }
    }
}

#[test]
fn le_mans_players_run_to_stationary_ostriches_and_jump_to_mount() {
    let mut cfg = RaceConfig::quick(TrackId::Savanna, 1, BotLevel::Normal, 9);
    cfg.le_mans = true;
    let mut w = World::with_track(cfg, Arc::new(proving_ground()));
    assert!(w
        .racers
        .iter()
        .all(|r| r.effects.le_mans && r.effects.dismount > 0.0));
    let before = w.racers[0].pos;
    let initial_gap = w.racers[0].chase_offset().length();
    while w.phase == RacePhase::Countdown {
        w.step(&[Input {
            throttle: if w.countdown_left() < 0.3 { 1.0 } else { 0.0 },
            ..Input::default()
        }]);
    }
    assert_eq!(
        w.racers[0].boost_t, 0.0,
        "the run start has no mounted rev bonus"
    );
    for _ in 0..60 {
        w.step(&[]);
    }
    assert!(
        w.racers[0].pos.distance(before) < 0.01,
        "the ostrich waits for its jockey"
    );
    assert!(
        w.racers[0].effects.le_mans,
        "getting close alone does not mount"
    );
    assert!(w.racers[0].chase_offset().length() < initial_gap);
    assert!(!w.racers[1].effects.le_mans, "bots run and jump as well");
    w.step(&[Input {
        jump: true,
        ..Input::default()
    }]);
    assert!(!w.racers[0].effects.le_mans);
    assert_eq!(w.racers[0].effects.dismount, 0.0);
    for _ in 0..30 {
        w.step(&[]);
    }
    assert!(w.racers[0].pos.distance(before) > 1.0);
}

#[test]
fn a_real_jump_before_a_rug_clears_the_trap() {
    let mut w = race(1);
    w.cfg.mode = RaceMode::Race;
    w.tick = w.go_tick + 120;
    w.play.rugs = vec![mischief::Rug {
        s: w.track.length * 0.31,
        rest: 45,
        ..Default::default()
    }];
    let s = w.track.length * 0.31;
    place(&mut w, 0, s - 7.0, 0.0, 18.0);
    for tick in 0..35 {
        w.step(&[Input {
            jump: tick == 0,
            ..Input::default()
        }]);
    }
    assert_eq!(w.play.rugs[0].pull, 0);
    assert_eq!(w.racers[0].effects.tumble, 0.0);
}

#[test]
fn printer_auto_aims_across_lanes_and_unseats_without_touching_the_wallet() {
    let mut w = race(3);
    place(&mut w, 1, 40.0, 4.0, 0.0);
    place(&mut w, 2, 100.0, 0.0, 0.0);
    w.racers[1].coins = 45;
    w.give_item(0, Item::MoneyPrinter, 1);
    let mut hits = Vec::new();
    for tick in 0..100 {
        w.step(&[
            Input {
                item: tick == 0,
                ..Default::default()
            },
            Input::default(),
            Input::default(),
        ]);
        hits.extend(w.drain_events());
        if w.racers[1].effects.dismount > 0.0 {
            break;
        }
    }
    assert!(hits.iter().any(|e| matches!(
        e,
        Event::Hit {
            victim: 1,
            by: Some(0),
            cause: HitCause::MoneyPrinter
        }
    )));
    assert!(w.racers[0].effects.printer > 0.0);
    assert!(w.racers[1].effects.dismount > 0.0);
    assert!(w.racers[1].chase_offset().length() > 4.0);
    assert_eq!(w.racers[1].coins, 45);
}

#[test]
fn a_knocked_off_rider_runs_back_by_itself_and_only_needs_jump() {
    let mut w = race(2);
    bill(&mut w, 0, 1);
    w.step(&[]);
    assert!(w.racers[1].effects.dismount > 0.0);
    for _ in 0..45 {
        w.step(&[]);
    }
    let mut left = w.racers[1].clone();
    let mut right = left.clone();
    for _ in 0..15 {
        left.integrate(
            &Input {
                steer: -1.0,
                ..Default::default()
            },
            &w.track,
            &w.tun,
        );
        right.integrate(
            &Input {
                steer: 1.0,
                ..Default::default()
            },
            &w.track,
            &w.tun,
        );
    }
    assert_eq!(left.pos, right.pos, "the loose ostrich follows the track");
    assert_eq!(
        left.chase_offset(),
        right.chase_offset(),
        "steering does not move the jockey"
    );
    // With no input at all the jockey reaches the ostrich but waits for a jump.
    let mut idle = w.clone();
    for _ in 0..240 {
        idle.step(&[]);
    }
    assert!(idle.racers[1].effects.dismount > 0.0, "no jump, no mount");
    assert!(idle.racers[1].chase_offset().length() < 2.0, "the jockey ran back alone");
    let mut caught_at = None;
    for tick in 0..420 {
        let jump = Input {
            jump: tick % 2 == 0,
            ..Default::default()
        };
        w.step(&[Input::default(), jump]);
        if w.racers[1].effects.dismount == 0.0 {
            caught_at = Some(tick);
            break;
        }
    }
    assert!(
        caught_at.is_some_and(|t| t < 350),
        "the jockey catches the mount before the recovery timeout"
    );
    assert_eq!(w.racers[1].chase_offset(), Vec3::ZERO);
    assert!(!w.racers[1].disabled());
}

#[test]
fn shields_and_pills_block_bills_and_a_rider_cannot_be_repeatedly_unseated() {
    for pill in [false, true] {
        let mut w = race(2);
        if pill {
            w.racers[1].effects.pill = 5.0;
        } else {
            w.racers[1].shield_t = 5.0;
        }
        bill(&mut w, 0, 1);
        w.step(&[]);
        assert_eq!(w.racers[1].effects.dismount, 0.0);
    }
    let mut w = race(2);
    bill(&mut w, 0, 1);
    w.step(&[]);
    let before = w.racers[1].effects.dismount;
    bill(&mut w, 0, 1);
    w.step(&[]);
    assert!(
        w.racers[1].effects.dismount < before,
        "another bill does not reset the chase"
    );
}

#[test]
fn grounded_racers_trigger_the_rug_but_a_jump_clears_it() {
    for jump in [false, true] {
        let mut w = race(2);
        w.cfg.mode = RaceMode::Race;
        w.tick = w.go_tick + 120;
        w.play.rugs = vec![mischief::Rug {
            s: w.track.length * 0.31,
            rest: 45,
            ..Default::default()
        }];
        let (pos, yaw, _) = mischief::rug_site(&w.track, &w.play.rugs[0]);
        let hint = w.track.project(pos, 0).idx;
        w.racers[0] = Racer::new(
            0,
            0,
            pos + Vec3::Y * if jump { 1.3 } else { 0.0 },
            yaw,
            hint,
        );
        if jump {
            w.racers[0].on_ground = false;
            w.racers[0].air_h = 1.3;
        }
        w.racers[0].coins = 32;
        let other = w.racers[1].pos;
        w.step(&[]);
        assert_eq!(w.racers[0].effects.dismount > 0.0, !jump);
        assert_eq!(w.play.rugs[0].pull > 0, !jump);
        assert_eq!(w.racers[0].effects.tumble, 0.0, "the rug unseats, it does not pratfall");
        // No living rugger: a rugged racer's coins scatter loose.
        assert_eq!(w.racers[0].coins, if jump { 32 } else { 0 });
        assert_eq!(w.loose_coins.len(), if jump { 0 } else { 32 });
        for _ in 0..180 {
            w.step(&[]);
        }
        assert!(w.racers[1].pos.distance(other) > 3.0);
    }
}

#[test]
fn tutorials_and_time_trials_have_no_rug_traps_and_network_lists_are_bounded() {
    let mut w = race(1);
    for _ in 0..10 {
        w.step(&[]);
    }
    assert!(w.play.rugs.is_empty());
    let too_many = serde_json::json!({"scramble":0.0,"frogs":[],"rugs":vec![
        serde_json::json!({"pull":0,"rest":0,"victim":255}); mischief::MAX_RUGS + 1
    ]});
    assert!(serde_json::from_value::<playful::PlayState>(too_many).is_err());
}

#[test]
fn multiplayer_waits_for_the_last_human_but_not_for_unfinished_bots() {
    let mut w = race(3);
    w.cfg.mode = RaceMode::Race;
    w.cfg.racers[2].kind = RacerKind::Bot(BotLevel::Normal);
    w.racers[0].finished_at = Some(1.0);
    w.first_human_finish = Some(1.0);
    w.tick = w.go_tick + 60 * 180;
    w.step(&[]);
    assert_eq!(w.phase, RacePhase::Racing);
    assert!(w.ended_at.is_none());
    w.racers[1].finished_at = Some(w.race_time());
    w.step(&[]);
    assert_eq!(w.phase, RacePhase::Finished);
    assert!(w.racers[2].finished_at.is_none());
}

#[test]
fn printer_only_acquires_riders_ahead_even_when_a_chaser_is_closer() {
    let mut w = race(3);
    place(&mut w, 0, 50.0, 0.0, 0.0);
    place(&mut w, 1, 45.0, 0.0, 0.0);
    place(&mut w, 2, 75.0, 4.0, 0.0);
    w.give_item(0, Item::MoneyPrinter, 1);
    let mut fired = false;
    for tick in 0..15 {
        w.step(&[Input {
            item: tick == 0,
            ..Default::default()
        }]);
        for p in &w.projectiles {
            if p.kind == ProjectileKind::DollarBill {
                assert_eq!(p.target, Some(2));
                assert!(p.vel.dot(w.racers[0].forward()) > 0.0);
                fired = true;
            }
        }
    }
    assert!(fired);
    let mut w = race(2);
    place(&mut w, 0, 50.0, 0.0, 0.0);
    place(&mut w, 1, 45.0, 0.0, 0.0);
    w.give_item(0, Item::MoneyPrinter, 1);
    for tick in 0..30 {
        w.step(&[Input {
            item: tick == 0,
            ..Default::default()
        }]);
    }
    assert!(w.projectiles.is_empty());
    assert_eq!(w.racers[1].effects.dismount, 0.0);
}

#[test]
fn a_bill_never_turns_back_after_its_target_passes_it() {
    let mut w = race(2);
    let heading = w.racers[0].forward();
    let start = w.racers[0].body();
    place(&mut w, 1, 5.0, 0.0, 0.0);
    w.projectiles.push(Projectile {
        kind: ProjectileKind::DollarBill,
        pos: start,
        vel: heading * 52.0,
        owner: 0,
        target: Some(1),
        age: 0.0,
        bounces: 0,
        hint: 0,
        s: 20.0,
        lateral: 0.0,
        id: 42,
    });
    w.step(&[]);
    assert!(w.projectiles[0].vel.dot(heading) > 0.0);
    assert_eq!(w.projectiles[0].target, None);
}

#[test]
fn unseated_ostrich_walks_forward_and_recovery_requires_a_fresh_jump() {
    let mut w = race(2);
    bill(&mut w, 0, 1);
    w.step(&[]);
    let start = w.racers[1].s;
    for _ in 0..600 {
        let mut input = bot_input(&w, 1, BotLevel::Normal);
        input.jump = false;
        w.step(&[Input::default(), input]);
    }
    assert!(
        w.racers[1].effects.dismount > 0.0,
        "no automatic timeout mount"
    );
    assert!(w.track.s_delta(start, w.racers[1].s) > 25.0);
    assert!((w.racers[1].speed - 3.0).abs() < 0.01);
    assert!(w.racers[1].chase_offset().length() < 2.0);
    w.step(&[
        Input::default(),
        Input {
            jump: true,
            ..Default::default()
        },
    ]);
    assert_eq!(w.racers[1].effects.dismount, 0.0);
    bill(&mut w, 0, 1);
    w.step(&[]);
    assert_eq!(
        w.racers[1].effects.dismount, 0.0,
        "mounting has a short grace period"
    );
}

#[test]
fn a_printer_volley_causes_one_fall_and_does_not_reset_the_chase() {
    let mut w = race(2);
    for _ in 0..4 {
        bill(&mut w, 0, 1);
    }
    w.step(&[]);
    assert_eq!(
        w.drain_events()
            .iter()
            .filter(|e| matches!(
                e,
                Event::Hit {
                    victim: 1,
                    cause: HitCause::MoneyPrinter,
                    ..
                }
            ))
            .count(),
        1
    );
    let offset = w.racers[1].chase_offset();
    let time = w.racers[1].effects.dismount;
    bill(&mut w, 0, 1);
    w.step(&[]);
    assert!(w.racers[1].effects.dismount < time);
    assert!(w.racers[1].chase_offset().distance(offset) < 0.2);
}

#[test]
fn rugs_only_appear_when_placed_and_wait_until_one_opponent_is_caught() {
    let mut w = race(2);
    w.cfg.mode = RaceMode::Race;
    w.step(&[]);
    assert!(w.play.rugs.is_empty());
    w.give_item(0, Item::RugPull, 1);
    w.step(&[Input {
        item: true,
        ..Default::default()
    }]);
    assert_eq!(w.play.rugs.len(), 1);
    assert_eq!(w.play.rugs[0].owner, 0);
    let s = w.play.rugs[0].s;
    w.play.rugs[0].rest = 45;
    w.tick += 60 * 300;
    place(&mut w, 0, s, 0.0, 0.0);
    place(&mut w, 1, s + 100.0, 0.0, 0.0);
    w.step(&[]);
    assert_eq!(w.play.rugs.len(), 1, "no expiry and no self-trigger");
    place(&mut w, 1, s, 0.0, 0.0);
    w.step(&[]);
    assert!(w.racers[1].effects.dismount > 0.0);
    assert_eq!(w.racers[1].effects.knock, KnockKind::Rug);
    assert_eq!(w.play.rugs[0].victim, 1);
    for _ in 0..40 {
        w.step(&[]);
    }
    assert!(w.play.rugs.is_empty(), "a used rug does not reset itself");
}

#[test]
fn airborne_riders_clear_a_hopping_pepe() {
    for jump in [false, true] {
        let mut w = race(1);
        w.cfg.mode = RaceMode::Race;
        w.step(&[]);
        let p = w.play.frogs[0].pos;
        let pr = w.track.project(p, 0);
        place(&mut w, 0, pr.s, pr.lateral, 0.0);
        if jump {
            w.racers[0].on_ground = false;
            w.racers[0].air_h = 1.2;
            w.racers[0].pos.y = pr.ground_y + 1.2;
        }
        w.step(&[]);
        assert_eq!(w.racers[0].effects.dismount > 0.0, !jump);
    }
}

#[test]
fn custom_boost_pads_work_at_both_road_edges() {
    let mut track = proving_ground();
    track.id = TrackId::Custom;
    let s = track
        .features
        .iter()
        .find_map(|f| match f {
            Feature::BoostPad { s, .. } => Some(*s),
            _ => None,
        })
        .unwrap();
    for lateral in [-9.0, 0.0, 9.0] {
        let mut racer = racer_at(&track, s + 1.0, lateral, 0);
        racer.integrate(&Input::default(), &track, &Tunables::default());
        assert!(racer.boost_t > 0.0, "no boost at lateral {lateral}");
    }
}

#[test]
fn a_loose_ostrich_cannot_finish_until_its_rider_mounts() {
    let mut w = race(1);
    let s = w.track.length - 0.01;
    place(&mut w, 0, s, 0.0, 0.0);
    w.racers[0].lap = w.cfg.laps;
    w.racers[0].next_cp = 4;
    w.racers[0].effects.dismount = 7.0;
    let gap = -w.racers[0].forward() * 1.2;
    w.racers[0].effects.chase_x = gap.x;
    w.racers[0].effects.chase_z = gap.z;
    w.step(&[]);
    assert!(w.racers[0].finished_at.is_none());
    assert_eq!(w.phase, RacePhase::Racing);
    w.step(&[Input {
        jump: true,
        ..Default::default()
    }]);
    assert!(w.racers[0].finished_at.is_some());
    assert_eq!(w.phase, RacePhase::Finished);
}

#[test]
fn placed_rug_network_state_rejects_invalid_positions_and_counts() {
    let valid = serde_json::json!({"s":42.0,"owner":0,"pull":0,"rest":45,"victim":255});
    let state = |rug| serde_json::json!({"scramble":0.0,"frogs":[],"rugs":[rug]});
    assert!(serde_json::from_value::<playful::PlayState>(state(valid.clone())).is_ok());
    for (field, value) in [
        ("s", serde_json::json!(-1.0)),
        ("s", serde_json::json!(7000.0)),
        ("owner", serde_json::json!(8)),
        ("pull", serde_json::json!(37)),
        ("rest", serde_json::json!(46)),
    ] {
        let mut rug = valid.clone();
        rug[field] = value.clone();
        assert!(
            serde_json::from_value::<playful::PlayState>(state(rug)).is_err(),
            "accepted {field}={value}"
        );
    }
}

#[test]
fn a_runner_can_turn_back_after_passing_the_ostrich() {
    let mut w = race(2);
    bill(&mut w, 0, 1);
    w.step(&[]);
    let offset = w.racers[1].forward() * 8.0;
    w.racers[1].effects.chase_x = offset.x;
    w.racers[1].effects.chase_z = offset.z;
    for _ in 0..360 {
        let input = bot_input(&w, 1, BotLevel::Normal);
        w.step(&[Input::default(), input]);
        if w.racers[1].effects.dismount == 0.0 { break; }
    }
    assert_eq!(w.racers[1].effects.dismount, 0.0, "a missed mount remains recoverable");
}

#[test]
fn falling_clears_the_previous_drift_and_trick_pose() {
    let mut w = race(2);
    w.racers[1].yaw_rate = 1.5;
    w.racers[1].dust = 1.0;
    w.racers[1].trick_pending = true;
    w.racers[1].trick_t = 1.0;
    w.racers[1].trick_armed = 1.0;
    bill(&mut w, 0, 1);
    w.step(&[]);
    let r = &w.racers[1];
    assert_eq!((r.yaw_rate, r.dust), (0.0, 0.0));
    assert_eq!(r.visual_yaw, r.yaw);
    assert!(!r.trick_pending);
    assert_eq!((r.trick_t, r.trick_armed), (0.0, 0.0));
}

/// Seconds from the fall to riding again before the faster remount (bot, any speed).
/// Measured at 0.4.8 (commit a73bfa9); fine as a fixed number.
const OLD_REMOUNT_SECS: f32 = 1.85;

fn knock(kind: KnockKind) -> World {
    let mut w = race(3);
    w.cfg.mode = RaceMode::Race;
    match kind {
        KnockKind::Printer => bill(&mut w, 0, 1),
        KnockKind::Rug => {
            w.play.rugs = vec![mischief::Rug {
                s: w.track.length * 0.31,
                rest: 45,
                owner: 0,
                ..Default::default()
            }];
            let (pos, yaw, _) = mischief::rug_site(&w.track, &w.play.rugs[0]);
            let hint = w.track.project(pos, 0).idx;
            w.racers[1] = Racer::new(1, 0, pos, yaw, hint);
        }
        KnockKind::Pepe => {
            w.step(&[]);
            let pr = w.track.project(w.play.frogs[0].pos, 0);
            place(&mut w, 1, pr.s, pr.lateral, 0.0);
        }
        _ => unreachable!(),
    }
    w.step(&[]);
    assert_eq!(w.racers[1].effects.knock, kind);
    assert!(w.racers[1].effects.dismount > 0.0);
    w
}

fn remount_secs(mut w: World) -> f32 {
    for tick in 0..900 {
        let jump = bot_input(&w, 1, BotLevel::Normal);
        w.step(&[Input::default(), jump, Input::default()]);
        if w.racers[1].effects.dismount == 0.0 {
            return (tick + 1) as f32 / 60.0;
        }
    }
    f32::INFINITY
}

#[test]
fn every_knock_off_remounts_about_forty_percent_quicker_than_before() {
    // On the road centre the printer and the rug are matched. Pepe hops at the
    // road edge, where the loose ostrich walks back towards the middle and the
    // jockey has less to run, so he is only bounded.
    let mut times = vec![];
    for kind in [KnockKind::Printer, KnockKind::Rug, KnockKind::Pepe] {
        let t = remount_secs(knock(kind));
        eprintln!("{kind:?}: remount in {t:.2} s (was {OLD_REMOUNT_SECS:.2} s)");
        assert!(t <= OLD_REMOUNT_SECS * 0.65, "{kind:?} took {t:.2} s");
        assert!(t >= 0.5, "{kind:?}: still a visible fall and run ({t:.2} s)");
        if kind != KnockKind::Pepe {
            times.push(t);
        }
    }
    assert!((times[0] - times[1]).abs() < 0.2, "about equally punishing: {times:?}");
}

#[test]
fn the_jockey_waits_out_the_fall_then_runs_faster_than_before() {
    let mut w = knock(KnockKind::Printer);
    let start = w.racers[1].chase_offset();
    let mut moved_at = None;
    for tick in 0..60 {
        w.step(&[]);
        if moved_at.is_none() && (w.racers[1].chase_offset() - start).length() > 1.0 {
            moved_at = Some(tick);
        }
    }
    assert!(moved_at.is_some_and(|t| (t as f32 / 60.0) < 0.6 && t >= 20), "{moved_at:?}");
    // Closing speed on the walking ostrich (3 m/s) once running: 10 m/s runner.
    let a = w.racers[1].chase_offset().length();
    w.step(&[]);
    let b = w.racers[1].chase_offset().length();
    assert!(a - b > (mischief::RUN_SPEED - 3.0) / 60.0 * 0.9, "{a} -> {b}");
}

fn rugged(owner_alive: bool) -> World {
    let mut w = race(3);
    w.cfg.mode = RaceMode::Race;
    w.play.rugs = vec![mischief::Rug {
        s: w.track.length * 0.31,
        rest: 45,
        owner: if owner_alive { 0 } else { 255 },
        ..Default::default()
    }];
    let (pos, yaw, _) = mischief::rug_site(&w.track, &w.play.rugs[0]);
    let hint = w.track.project(pos, 0).idx;
    w.racers[1] = Racer::new(1, 0, pos, yaw, hint);
    w.racers[0].coins = 3;
    w.racers[1].coins = 7;
    w.racers[2].coins = 5;
    w
}

#[test]
fn a_rugged_racer_loses_every_coin_to_the_rugger_and_the_total_is_unchanged() {
    let mut w = rugged(true);
    let total = |w: &World| w.racers.iter().map(|r| r.coins).sum::<u32>() + w.loose_coins.len() as u32;
    assert_eq!(total(&w), 15);
    w.step(&[]);
    assert_eq!(w.racers[1].effects.knock, KnockKind::Rug);
    assert_eq!(w.racers[1].coins, 0, "the victim loses all of them");
    assert_eq!(w.racers[0].coins, 10, "the rugger gains exactly that");
    assert_eq!(w.racers[2].coins, 5, "bystanders are untouched");
    assert!(w.loose_coins.is_empty(), "nothing scatters when the rugger is alive");
    assert_eq!(total(&w), 15);
    let events = w.drain_events();
    assert!(events.contains(&Event::CoinsTaken { from: 1, to: 0, count: 7 }), "{events:?}");
}

#[test]
fn a_rug_with_no_living_owner_scatters_the_coins_loose() {
    let mut w = rugged(false);
    w.step(&[]);
    assert_eq!(w.racers[1].coins, 0);
    assert_eq!(w.racers[0].coins, 3);
    assert_eq!(w.loose_coins.len(), 7);
    assert_eq!(
        w.racers.iter().map(|r| r.coins).sum::<u32>() + w.loose_coins.len() as u32,
        15,
        "nothing created or destroyed"
    );
    let events = w.drain_events();
    assert!(!events.iter().any(|e| matches!(e, Event::CoinsTaken { .. })));
    assert!(events.contains(&Event::CoinsDropped { racer: 1, n: 7 }));
}

#[test]
fn a_shield_stops_the_rug_taking_coins() {
    let mut w = rugged(true);
    w.racers[1].shield_t = 5.0;
    w.step(&[]);
    assert_eq!((w.racers[0].coins, w.racers[1].coins), (3, 7));
    assert_eq!(w.racers[1].effects.dismount, 0.0);
}

#[test]
fn a_protected_rider_is_not_rugged_or_flattened_by_pepe_again() {
    let mut w = rugged(true);
    w.racers[1].effects.remount_guard = 1.0;
    w.step(&[]);
    assert_eq!(w.racers[1].effects.dismount, 0.0);
    assert_eq!(w.play.rugs[0].victim, 255, "the rug waits for someone it can catch");
    assert_eq!(w.racers[1].coins, 7);
    // And Pepe: a protected rider on top of him is neither flattened nor laughed at.
    let mut w = on_pepe();
    w.racers[1].effects.remount_guard = 1.0;
    w.drain_events();
    w.step(&[]);
    assert_eq!(w.racers[1].effects.dismount, 0.0);
    assert_eq!(w.racers[1].effects.knock, KnockKind::None);
    assert_eq!(w.play.frogs[0].tumble, 0.0, "Pepe does not fall over for a protected rider");
    assert_eq!(w.play.frogs[0].victim, 255);
    assert!(!w.drain_events().iter().any(|e| matches!(e, Event::Hit { cause: HitCause::Pepe, .. })));
    // Without the protection the same spot does flatten the rider (the set-up is sound).
    let mut w = on_pepe();
    w.step(&[]);
    assert!(w.racers[1].effects.dismount > 0.0);
    assert_eq!(w.racers[1].effects.knock, KnockKind::Pepe);
}

#[test]
fn each_knock_off_cause_has_its_own_kind_and_the_kind_survives_the_wire() {
    let causes = [
        HitCause::MoneyPrinter,
        HitCause::RugPull,
        HitCause::Pepe,
        HitCause::LaserTrump,
    ];
    let kinds: Vec<_> = causes.iter().map(|&c| KnockKind::from_cause(c)).collect();
    for (i, a) in kinds.iter().enumerate() {
        assert_ne!(*a, KnockKind::None);
        assert_eq!(KnockKind::from_code(a.code()), *a);
        for b in &kinds[i + 1..] {
            assert_ne!(a, b, "causes must not share a kind");
        }
    }
    assert_eq!(kinds.len(), KnockKind::ALL.len());
    for c in [HitCause::RollingEgg, HitCause::Cactus, HitCause::Laser, HitCause::Hurdle, HitCause::Bash] {
        assert_eq!(KnockKind::from_cause(c), KnockKind::None);
    }
    for kind in KnockKind::ALL {
        for (dismount, trump) in [(3.0, 0.0), (0.0, 0.0), (0.0, 2.0)] {
            let e = playful::Effects { knock: kind, dismount, trump, ..Default::default() };
            let back = playful::Effects::unpack(e.pack());
            // A running rocket owns the byte; it never overlaps a knock-off in play.
            assert_eq!(back.knock, if trump > 0.0 { KnockKind::None } else { kind });
            assert!((back.trump - trump).abs() < 0.11);
        }
    }
}

/// An egg already in flight and sitting on `victim`, thrown by `owner`.
fn egg(w: &mut World, owner: usize, victim: usize) {
    let r = &w.racers[victim];
    w.projectiles.push(Projectile {
        kind: ProjectileKind::RollingEgg,
        pos: r.body(),
        vel: Vec3::ZERO,
        owner: owner as u8,
        target: None,
        age: 1.0,
        bounces: 0,
        hint: r.track_idx,
        s: r.s,
        lateral: r.lateral,
        id: 998,
    });
}

/// Racer 0 in a Laser Trump rocket with racer 1 square in its path; steps until
/// the smash has landed. Everything goes through the real hit code.
fn trump_smash() -> World {
    let mut w = race(3);
    w.cfg.mode = RaceMode::Race;
    place(&mut w, 2, 600.0, 0.0, 0.0);
    place(&mut w, 0, 100.0, 0.0, 25.0);
    place(&mut w, 1, 125.0, 1.0, 0.0);
    assert_eq!(w.racers[1].effects.knock, KnockKind::None, "nothing recorded before the hit");
    w.give_item(0, Item::LaserTrump, 1);
    let mut inputs = vec![Input::default(); 3];
    inputs[0].item = true;
    w.step(&inputs);
    for _ in 0..120 {
        if w.racers[1].stun_t > 0.0 {
            break;
        }
        w.step(&[]);
    }
    assert!(w.racers[1].stun_t > 0.0, "the rocket smashed racer 1");
    assert_eq!(w.racers[1].effects.knock, KnockKind::Trump, "a Trump smash records its kind");
    w
}

#[test]
fn a_laser_trump_smash_records_its_kind_until_the_spin_ends() {
    let mut w = trump_smash();
    // Leave the rocket behind and let the spin run out: the kind goes with it.
    w.racers[0].effects.trump = 0.0;
    for _ in 0..180 {
        w.step(&[]);
        if w.racers[1].stun_t <= 0.0 {
            break;
        }
    }
    assert!(w.racers[1].stun_t <= 0.0);
    w.step(&[]);
    assert_eq!(w.racers[1].effects.knock, KnockKind::None, "cleared once the spin is over");
}

#[test]
fn an_egg_landing_in_a_trump_spin_does_not_replay_the_trump_pose() {
    let mut w = trump_smash();
    egg(&mut w, 0, 1);
    w.step(&[]);
    let ev = w.drain_events();
    assert!(
        ev.iter().any(|e| matches!(e, Event::Hit { victim: 1, cause: HitCause::RollingEgg, .. })),
        "the egg hit through the real path: {ev:?}"
    );
    assert!(w.racers[1].stun_t > 0.0);
    assert_eq!(w.racers[1].effects.knock, KnockKind::None, "the new cause owns the pose");
}

/// Racer 1 standing on the first Pepe in a race.
fn on_pepe() -> World {
    let mut w = race(3);
    w.cfg.mode = RaceMode::Race;
    w.step(&[]);
    let pr = w.track.project(w.play.frogs[0].pos, 0);
    place(&mut w, 1, pr.s, pr.lateral, 0.0);
    w
}

#[test]
fn a_shield_blocks_pepe_but_pepe_still_falls_over_and_laughs() {
    let mut w = on_pepe();
    w.racers[1].shield_t = 5.0;
    w.drain_events();
    w.step(&[]);
    let ev = w.drain_events();
    assert!(ev.iter().any(|e| matches!(e, Event::ShieldBlock { racer: 1, .. })), "{ev:?}");
    assert!(!ev.iter().any(|e| matches!(e, Event::Hit { cause: HitCause::Pepe, .. })), "{ev:?}");
    assert_eq!(w.racers[1].shield_t, 0.0, "the shield is used up");
    assert_eq!(w.racers[1].effects.dismount, 0.0, "the rider stays in the saddle");
    assert_eq!(w.racers[1].effects.knock, KnockKind::None);
    assert!(w.play.frogs[0].tumble > 0.0, "Pepe still falls over");
    assert_eq!(w.play.frogs[0].victim, 1, "and laughs at the shielded racer");
}
