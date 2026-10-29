//! Gameplay contracts for the playful racing revision.

mod common;
use common::*;
use race_core::*;
use race_core::playful::ItemNote;
use std::sync::Arc;

fn race(n: usize) -> World {
    let mut track = proving_ground();
    track
        .features
        .push(Feature::TargetRow { s: 90.0, count: 1 });
    let mut cfg = RaceConfig::quick(TrackId::Training, 0, BotLevel::Normal, 19);
    cfg.racers = (0..n)
        .map(|i| RacerSetup {
            name: format!("Rider {i}"),
            character: 0,
            kind: RacerKind::Human,
        })
        .collect();
    cfg.mode = RaceMode::TimeTrial;
    let mut w = World::with_track(cfg, Arc::new(track));
    skip_countdown(&mut w);
    for i in 0..n {
        place(&mut w, i, 20.0 + i as f32 * 20.0, 0.0, 0.0);
    }
    w
}

fn use_item(w: &mut World, i: usize, item: Item) {
    w.give_item(i, item, 1);
    let mut inputs = vec![Input::default(); w.racers.len()];
    inputs[i].item = true;
    w.step(&inputs);
    w.drain_events();
}

#[test]
fn lasers_are_forward_rays_and_never_open_eggs() {
    let mut w = race(3);
    place(&mut w, 2, 30.0, 4.0, 0.0); // closer, but outside the ray
    let eye = w.racers[0].eye();
    w.targets[0].pos = eye + w.racers[0].forward() * 10.0;
    w.targets[0].alive = true;
    w.give_item(0, Item::MegaLaser, 1);
    let ev = run(&mut w, 15, |w| {
        vec![
            Input {
                item: w.racers[0].item.is_some(),
                look_back: true,
                ..Default::default()
            },
            Input::default(),
            Input::default(),
        ]
    });
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::LaserZap { by: 0, victim: 1 })));
    assert_eq!(w.racers[0].laser.hit_kind, LaserHit::Racer(1));
    let l = w.racers[0].laser;
    assert!((l.aim - l.origin).normalize().dot(w.racers[0].forward()) > 0.99999);
    assert!(w.targets[0].alive);
    assert!(w.racers[0].item.is_none());
    assert_eq!(w.racers[2].effects.burn, 0.0);
    assert!(w.racers[1].effects.burn > 3.0);
    let normal = Racer::new(0, 0, Vec3::ZERO, 0.0, 0).top_speed(&w.tun);
    assert!((w.racers[1].top_speed(&w.tun) / normal - 0.7).abs() < 0.001);
    run(&mut w, 250, |_| vec![Input::default(); 3]);
    assert_eq!(w.racers[1].effects.burn, 0.0);
}

#[test]
fn egg_rows_are_small_have_one_bad_egg_and_are_laid_in_order() {
    let mut w = race(1);
    assert_eq!(w.targets.len(), 8); // 20m road; author-supplied count is replaced
    assert_eq!(w.targets.iter().filter(|t| t.bad).count(), 1);
    assert!(w.targets.iter().all(|t| !t.alive && t.pos.y < 1.0));
    run(&mut w, 48, |_| vec![Input::default()]);
    let halfway = w.targets.iter().filter(|t| t.alive).count();
    assert!(halfway > 0 && halfway < 8);
    run(&mut w, 80, |_| vec![Input::default()]);
    assert!(w.targets.iter().all(|t| t.alive));
    let good = w.targets.iter().position(|t| !t.bad).unwrap();
    let p = w.targets[good].pos;
    w.racers[0].pos = p - Vec3::Y * TARGET_HEIGHT;
    w.racers[0].vel = Vec3::ZERO;
    w.step(&[]);
    assert!(!w.targets[good].alive);
    assert!(w.racers[0].item.is_some()); // no laser or action input
}

#[test]
fn bad_egg_splats_then_clears_and_whisk_inverts_contents_globally() {
    for scramble in [false, true] {
        for bad in [false, true] {
            let mut w = race(2);
            if scramble {
                use_item(&mut w, 1, Item::EggScrambler);
            }
            let k = w.targets.iter().position(|t| t.bad == bad).unwrap();
            w.targets[k].alive = true;
            w.racers[0].pos = w.targets[k].pos - Vec3::Y * TARGET_HEIGHT;
            w.racers[0].vel = Vec3::ZERO;
            w.step(&[]);
            assert_eq!(w.racers[0].effects.splat > 0.0, bad ^ scramble);
            assert_eq!(w.racers[0].item.is_some(), !(bad ^ scramble));
            run(&mut w, 185, |_| vec![Input::default(); 2]);
            assert_eq!(w.racers[0].effects.splat, 0.0);
        }
    }
}

#[test]
fn laptop_conserves_race_bitcoin_beyond_the_old_ten_and_u8_caps() {
    let mut w = race(3);
    for (r, coins) in w.racers.iter_mut().zip([300, 450, 1000]) {
        r.coins = coins;
    }
    use_item(&mut w, 1, Item::Laptop);
    assert_eq!(
        w.racers.iter().map(|r| r.coins).collect::<Vec<_>>(),
        [0, 1750, 0]
    );
    assert!(w.racers[1].effects.laptop > 0.0);
    // A second hack cannot duplicate those coins.
    use_item(&mut w, 0, Item::Laptop);
    assert_eq!(w.racers.iter().map(|r| r.coins).sum::<u32>(), 1750);
    assert_eq!(w.racers[0].coins, 1750);
}

#[test]
fn eating_cake_blocks_item_use_until_the_cake_is_gone() {
    let mut w = race(3);
    use_item(&mut w, 1, Item::SharedCake);
    // Laser Eyes is not a good power-up, so nobody takes it.
    w.give_item(0, Item::MegaLaser, 1);
    let press = |on: bool| {
        move |_: &World| vec![Input { item: on, ..Default::default() }, Input::default(), Input::default()]
    };
    // Mashing the item button for the whole cake does nothing.
    for tick in 0..120 {
        w.step(&press(tick % 2 == 0)(&w));
        assert!(w.racers[0].effects.cake_eat > 0.0);
        assert!(w.racers[0].item.is_some(), "item used while eating (tick {tick})");
    }
    // Held through the end of the cake: still no phantom use.
    run(&mut w, 120, press(true));
    assert_eq!(w.racers[0].effects.cake_eat, 0.0);
    assert!(w.racers[0].item.is_some(), "a held button fired as the cake ended");
    // Release and press afresh: it works.
    run(&mut w, 2, press(false));
    run(&mut w, 2, press(true));
    assert!(w.racers[0].item.is_none(), "a fresh press after the cake uses the item");
}

#[test]
fn everyone_but_the_user_eats_the_cake_and_it_expires() {
    let mut w = race(3);
    use_item(&mut w, 1, Item::SharedCake);
    assert_eq!(w.racers[1].effects.cake_eat, 0.0, "the user does not eat");
    assert!(w.racers[0].effects.cake_eat > 2.0);
    assert_eq!(w.racers[0].effects.cake_eat, w.racers[2].effects.cake_eat);
    assert!(w.racers[1].effects.note_t > 0.0, "the user is told what the cake took");
    assert_eq!(w.racers[1].effects.note, playful::ItemNote::Cake(0), "nobody had anything");
    run(&mut w, 152, |_| vec![Input::default(); 3]);
    assert!(w.racers.iter().all(|r| r.effects.cake_eat == 0.0));
    assert_eq!(w.racers[1].item, Some(Item::Feather), "the Feather for an empty cake");
}

#[test]
fn an_eating_racer_is_exactly_as_fast_as_one_who_is_not() {
    let w = race(1);
    let mut eating = w.racers[0].clone();
    let mut plain = eating.clone();
    eating.effects.cake_eat = 2.5;
    for r in [&mut eating, &mut plain] {
        r.boost_t = 1.0;
        r.coins = 3;
    }
    assert_eq!(eating.top_speed(&w.tun), plain.top_speed(&w.tun));
    for _ in 0..120 {
        eating.integrate(&Input::go(), &w.track, &w.tun);
        plain.integrate(&Input::go(), &w.track, &w.tun);
    }
    assert_eq!(eating.speed, plain.speed);
    assert_eq!(eating.pos, plain.pos);
}

#[test]
fn cake_moves_a_held_shield_to_the_user_but_never_a_hunter_egg() {
    let mut w = race(3);
    w.give_item(0, Item::Shield, 1);
    w.give_item(2, Item::HunterEgg, 1);
    use_item(&mut w, 1, Item::SharedCake);
    assert_eq!(w.racers[1].item, Some(Item::Shield));
    assert_eq!(w.racers[1].item_uses, 1);
    assert!(w.racers[1].queue.is_empty());
    assert_eq!(w.racers[1].effects.note, playful::ItemNote::Cake(1));
    assert!(w.racers[0].item.is_none(), "the shield left its holder");
    assert_eq!(w.racers[2].item, Some(Item::HunterEgg), "eggs stay where they are");
}

#[test]
fn cake_ends_a_running_feather_boost_and_the_user_gets_a_feather() {
    let mut w = race(3);
    use_item(&mut w, 0, Item::Feather);
    assert!(w.racers[0].boost_t > 0.0 && w.racers[0].effects.feather > 0.0);
    // A pad boost is not a power-up: nothing to take from racer 2.
    w.racers[2].grant_boost(1.0);
    use_item(&mut w, 1, Item::SharedCake);
    assert_eq!(w.racers[0].boost_t, 0.0, "the victim's boost ends");
    assert_eq!(w.racers[0].effects.feather, 0.0);
    assert_eq!(w.racers[1].item, Some(Item::Feather));
    assert_eq!(w.racers[1].item_uses, 1);
    assert!(w.racers[1].queue.is_empty());
    assert!(w.racers[2].boost_t > 0.0, "a pad boost stays");
}

#[test]
fn a_feather_cut_short_or_outlasted_leaves_no_feather_for_a_cake_to_find() {
    // Cut short (as a knock-off does), then a pad boost before the feather's
    // time would have run out.
    let mut w = race(3);
    use_item(&mut w, 0, Item::Feather);
    w.racers[0].boost_t = 0.0;
    w.step(&[Input::default(); 3]);
    w.racers[0].grant_boost(1.0);
    use_item(&mut w, 1, Item::SharedCake);
    assert!(w.racers[0].boost_t > 0.0, "the pad boost stays");
    assert_eq!(w.racers[1].effects.note, ItemNote::Cake(0), "no free Feather taken");
    // Outlasted by a longer pad boost: that boost is the one running now.
    let mut w = race(3);
    use_item(&mut w, 0, Item::Feather);
    let longer = w.racers[0].boost_t + 2.0;
    w.racers[0].grant_boost(longer);
    use_item(&mut w, 1, Item::SharedCake);
    assert!(w.racers[0].boost_t > 0.0);
    assert_eq!(w.racers[1].effects.note, ItemNote::Cake(0));
}

#[test]
fn cake_takes_every_running_good_power_up_and_stacks_them() {
    let mut w = race(3);
    w.racers[0].shield_t = 5.0;
    w.racers[0].magnet_t = 3.0;
    w.racers[2].effects.pill = 4.0;
    w.give_item(2, Item::Feather, 3);
    w.give_item(2, Item::Cactus, 1);
    use_item(&mut w, 1, Item::SharedCake);
    let r = &w.racers[1];
    let mut got: Vec<(Item, u8)> = r.item.map(|i| (i, r.item_uses)).into_iter().collect();
    got.extend(r.queue.iter().map(|q| (q.item, q.uses)));
    assert_eq!(
        got,
        vec![(Item::Shield, 1), (Item::Magnet, 1), (Item::OrangePill, 1), (Item::Feather, 3)]
    );
    assert_eq!(r.effects.note, playful::ItemNote::Cake(4));
    assert_eq!(w.racers[0].shield_t, 0.0);
    assert_eq!(w.racers[0].magnet_t, 0.0);
    assert_eq!(w.racers[2].effects.pill, 0.0);
    // The cactus was queued behind the feather; it moves up into the slot.
    assert_eq!(w.racers[2].item, Some(Item::Cactus));
    assert!(w.racers[2].queue.is_empty());
}

#[test]
fn a_cake_that_finds_nothing_gives_the_user_a_feather() {
    let mut w = race(3);
    use_item(&mut w, 1, Item::SharedCake);
    assert_eq!(w.racers[1].effects.note, ItemNote::Cake(0));
    assert_eq!((w.racers[1].item, w.racers[1].item_uses), (Some(Item::Feather), 1));
}

#[test]
fn a_laser_trump_taken_by_cake_leaves_the_victim_the_exit_boost() {
    let mut w = race(3);
    w.racers[0].effects.trump = 2.0;
    use_item(&mut w, 1, Item::SharedCake);
    assert_eq!(w.racers[1].item, Some(Item::LaserTrump));
    let v = &w.racers[0];
    assert_eq!(v.effects.trump, 0.0);
    assert!(v.boost_t > playful::TRUMP_EXIT_BOOST - 0.1, "boost {}", v.boost_t);
    assert_eq!(v.boost_tier, 0, "at full power");
    assert_eq!(v.effects.feather, 0.0, "and it is no Feather");
}

#[test]
fn a_finished_racer_driven_by_the_bot_keeps_its_cake() {
    let mut w = race(3);
    w.racers[2].shield_t = 5.0;
    w.give_item(0, Item::SharedCake, 1);
    // A finished human is driven by the bot from here on.
    w.racers[0].finished_at = Some(30.0);
    assert!(w.bot_level_for(0).is_some());
    for _ in 0..(3.0 / math::DT) as usize {
        w.step(&[Input::default(); 3]);
    }
    assert_eq!(w.racers[0].item, Some(Item::SharedCake), "not used once finished");
    assert!(w.racers[2].shield_t > 0.0);
}

#[test]
fn what_does_not_fit_in_the_users_queue_stays_with_the_victim() {
    let mut w = race(3);
    // Room for one more once the cake is used: the next cactus moves up into the
    // slot, and the queue is one short of full.
    w.give_item(1, Item::SharedCake, 1);
    for _ in 0..MAX_QUEUE {
        w.give_item(1, Item::Cactus, 1);
    }
    w.racers[0].shield_t = 5.0;
    w.racers[0].magnet_t = 3.0;
    w.give_item(0, Item::Feather, 3);
    w.give_item(0, Item::RollingEgg, 1);
    w.give_item(0, Item::Magnet, 2);
    press_item(&mut w, 1);
    let u = &w.racers[1];
    assert_eq!(u.item, Some(Item::Cactus));
    assert_eq!(u.queue.len(), MAX_QUEUE, "full now");
    assert_eq!(u.queue.last().map(|q| q.item), Some(Item::Shield), "the first running one");
    assert_eq!(u.effects.note, playful::ItemNote::Cake(1));
    let v = &w.racers[0];
    assert_eq!(v.shield_t, 0.0);
    assert!(v.magnet_t > 0.0, "the magnet runs on");
    let mut held: Vec<(Item, u8)> = v.item.map(|i| (i, v.item_uses)).into_iter().collect();
    held.extend(v.queue.iter().map(|q| (q.item, q.uses)));
    assert_eq!(held, [(Item::Feather, 3), (Item::RollingEgg, 1), (Item::Magnet, 2)], "kept, in order");
}

#[test]
fn cake_ends_the_bad_luck_but_leaves_a_jockey_on_the_ground() {
    let mut w = race(3);
    w.racers[0].stun_t = 1.5;
    w.racers[0].effects.burn = 3.0;
    w.racers[0].slow_t = 2.0;
    w.racers[0].stumble_t = 1.0;
    w.racers[0].effects.tumble = 2.0;
    w.racers[2].effects.dismount = 5.0;
    w.racers[2].effects.chase_x = 3.0;
    w.racers[2].effects.knock = mischief::KnockKind::Rug;
    use_item(&mut w, 1, Item::SharedCake);
    let v = &w.racers[0];
    assert_eq!((v.stun_t, v.slow_t, v.stumble_t), (0.0, 0.0, 0.0));
    assert_eq!((v.effects.burn, v.effects.tumble), (0.0, 0.0));
    assert!(v.effects.cake_eat > 0.0);
    let d = &w.racers[2];
    assert!(d.effects.dismount > 0.0, "still on foot");
    assert_eq!(d.effects.knock, mischief::KnockKind::Rug);
    assert!(d.effects.chase_x != 0.0);
}

#[test]
fn the_queue_moves_up_when_the_held_item_runs_out_and_stays_bounded() {
    let mut w = race(2);
    w.give_item(0, Item::Feather, 2);
    w.give_item(0, Item::Cactus, 1);
    assert_eq!(w.racers[0].item, Some(Item::Feather));
    assert_eq!(w.racers[0].queue.len(), 1);
    press_item(&mut w, 0);
    assert_eq!(w.racers[0].item, Some(Item::Feather), "one use left");
    assert_eq!(w.racers[0].item_uses, 1);
    press_item(&mut w, 0);
    assert_eq!(w.racers[0].item, Some(Item::Cactus), "the next one moves up");
    assert!(w.racers[0].queue.is_empty());
    press_item(&mut w, 0);
    assert!(w.racers[0].item.is_none());
    // At most MAX_QUEUE wait behind the slot.
    for _ in 0..20 {
        w.give_item(1, Item::Feather, 1);
    }
    assert_eq!(w.racers[1].queue.len(), MAX_QUEUE);
    assert!(!w.racers[1].stash(Item::Shield, 1));
    // Using the cake frees one place (the next feather moves up), so the shield
    // fits and the magnet runs on with the victim, not lost.
    w.racers[1].item = Some(Item::SharedCake);
    w.racers[0].effects.feather = 0.0; // its own feathers are spent
    w.racers[0].shield_t = 3.0;
    w.racers[0].magnet_t = 3.0;
    press_item(&mut w, 1);
    assert_eq!(w.racers[0].shield_t, 0.0);
    assert!(w.racers[0].magnet_t > 0.0);
    assert_eq!(w.racers[1].item, Some(Item::Feather));
    assert_eq!(w.racers[1].queue.len(), MAX_QUEUE);
    assert_eq!(w.racers[1].queue.last().map(|q| q.item), Some(Item::Shield));
    assert_eq!(w.racers[1].effects.note, playful::ItemNote::Cake(1));
    assert_eq!(w.racers[0].item, None);
}

#[test]
fn a_queue_off_the_wire_is_bounded() {
    let w = race(1);
    let mut r = w.racers[0].clone();
    r.item = Some(Item::Shield);
    r.item_uses = 1;
    r.queue = vec![ItemRoll { item: Item::Feather, uses: 3 }; MAX_QUEUE];
    let ok = bincode::serialize(&r).unwrap();
    let back: Racer = bincode::deserialize(&ok).unwrap();
    assert_eq!(back.queue, r.queue);
    r.queue.push(ItemRoll { item: Item::Feather, uses: 1 });
    assert!(bincode::deserialize::<Racer>(&bincode::serialize(&r).unwrap()).is_err());
    r.queue = vec![ItemRoll { item: Item::Feather, uses: 0 }];
    assert!(bincode::deserialize::<Racer>(&bincode::serialize(&r).unwrap()).is_err());
}

#[test]
fn full_throttle_ignores_accelerator_and_brake_inputs() {
    let w = race(1);
    let mut a = w.racers[0].clone();
    let mut b = a.clone();
    for _ in 0..180 {
        a.integrate(&Input::default(), &w.track, &w.tun);
        b.integrate(
            &Input {
                throttle: 1.0,
                brake: true,
                ..Default::default()
            },
            &w.track,
            &w.tun,
        );
    }
    assert_eq!(a.pos, b.pos);
    assert_eq!(a.speed, b.speed);
    assert!(a.speed > w.tun.top_speed * 0.9);
}

#[test]
fn orange_pill_leaves_the_steering_to_the_player_and_blocks_laser_fire() {
    let mut w = race(2);
    use_item(&mut w, 1, Item::OrangePill);
    let yaw = w.racers[1].yaw;
    let input = Input {
        steer: 1.0,
        throttle: 1.0,
        ..Default::default()
    };
    for _ in 0..30 {
        w.racers[1].integrate(&input, &w.track, &w.tun);
    }
    assert!(
        race_core::math::wrap_angle(w.racers[1].yaw - yaw) < -0.15,
        "no autopilot: full lock turns the pill racer"
    );
    // And with the stick centred it does not steer itself back onto the road.
    let mut off = race(2);
    use_item(&mut off, 1, Item::OrangePill);
    off.racers[1].yaw += 0.5;
    let yaw = off.racers[1].yaw;
    for _ in 0..30 {
        off.racers[1].integrate(&Input { throttle: 1.0, ..Default::default() }, &off.track, &off.tun);
    }
    assert!(
        (off.racers[1].yaw - yaw).abs() < 0.01,
        "centred stick holds the heading"
    );
    run(&mut w, 25, |_| {
        vec![
            Input {
                laser: true,
                ..Default::default()
            },
            Input::default(),
        ]
    });
    assert_eq!(w.racers[1].effects.burn, 0.0);
    assert!(w.racers[1].effects.pill > 6.0);
}

#[test]
fn orange_pill_runs_a_quarter_faster_and_is_still_invincible() {
    let mut w = race(2);
    let plain = w.racers[0].top_speed(&w.tun);
    use_item(&mut w, 1, Item::OrangePill);
    let pilled = w.racers[1].top_speed(&w.tun);
    assert!((pilled / plain - 1.25).abs() < 1e-4, "{pilled} vs {plain}");
    assert!(w.racers[1].effects.invincible());
    // It actually reaches it, not just reports it.
    let go = Input::go();
    for _ in 0..(5.0 / race_core::math::DT) as usize {
        w.racers[1].effects.pill = 3.0;
        w.racers[1].integrate(&go, &w.track, &w.tun);
    }
    assert!(
        w.racers[1].speed > w.tun.top_speed * 1.2,
        "speed {} against top {}",
        w.racers[1].speed,
        w.tun.top_speed
    );
    // A cactus underneath does nothing to it, but spins out the plain racer.
    let pos = w.racers[1].pos;
    let plain_pos = w.racers[0].pos;
    w.hazards.push(Hazard { kind: HazardKind::Cactus, pos, owner: 0, age: 5.0, id: 5 });
    w.hazards.push(Hazard { kind: HazardKind::Cactus, pos: plain_pos, owner: 1, age: 5.0, id: 6 });
    w.step(&[Input::default(), Input::go()]);
    assert!(w.racers[0].stun_t > 0.0, "the plain racer is spun out");
    assert_eq!(w.racers[1].stun_t, 0.0, "the pill passes through the cactus");
    // The boost is gone with the pill.
    w.racers[1].effects.pill = 0.0;
    assert!((w.racers[1].top_speed(&w.tun) - plain).abs() < 1e-3);
}

#[test]
fn frog_pratfall_recovers_without_coin_loss_or_stopping_others() {
    let mut w = race(2);
    w.cfg.mode = RaceMode::Race;
    w.step(&[]); // initialise and move the frogs
    let p = w.play.frogs[0].pos;
    w.racers[0].pos = p;
    w.racers[0].vel = Vec3::ZERO;
    w.racers[0].coins = 45;
    w.step(&[]);
    assert!(w.racers[0].effects.dismount > 0.0, "Pepe unseats the rider");
    assert_eq!(w.racers[0].effects.knock, KnockKind::Pepe);
    assert!(w.play.frogs[0].tumble > 2.7, "Pepe falls over too");
    assert_eq!(w.play.frogs[0].victim, 0);
    let other = w.racers[1].pos;
    run(&mut w, 60, |_| vec![Input::default(); 2]);
    assert!(w.racers[0].effects.dismount > 0.0);
    assert!((w.racers[1].pos - other).length() > 3.0);
    // The jockey runs back alone and a jump mounts it again.
    let mut mounted = false;
    for _ in 0..240 {
        let jump = bot_input(&w, 0, BotLevel::Normal);
        w.step(&[jump, Input::default()]);
        mounted |= w.racers[0].effects.dismount == 0.0;
        if mounted {
            break;
        }
    }
    assert!(mounted);
    assert_eq!(w.racers[0].coins, 45, "Pepe takes no coins");
    run(&mut w, 150, |_| vec![Input::default(); 2]);
    assert!(w.racers[0].speed > 0.0);
}

#[test]
fn pepe_stays_where_he_fell_and_hops_on_from_there() {
    let mut w = race(2);
    w.cfg.mode = RaceMode::Race;
    w.step(&[]);
    let hit_at = w.play.frogs[0].pos;
    w.racers[0].pos = hit_at;
    w.racers[0].vel = Vec3::ZERO;
    w.step(&[]);
    assert!(w.play.frogs[0].tumble > 2.7);
    let fell_at = w.play.frogs[0].pos;
    let flat_gap = |a: Vec3, b: Vec3| Vec3::new(a.x - b.x, 0.0, a.z - b.z).length();
    assert!(flat_gap(fell_at, hit_at) < 0.5);
    // Keep the riders clear of him so he is not knocked over twice.
    let mut moved_on = false;
    for k in 0..400 {
        place(&mut w, 0, 400.0, 0.0, 0.0);
        place(&mut w, 1, 450.0, 0.0, 0.0);
        w.step(&[]);
        let f = w.play.frogs[0];
        if f.tumble > 0.0 {
            assert_eq!(f.pos, fell_at, "he lies where he fell (tick {k})");
        } else if !moved_on && f.pos != fell_at {
            // Today the clock has moved on by the whole fall and he would land metres away.
            assert!(
                flat_gap(f.pos, fell_at) < 0.3,
                "tick {k}: he got up {:.1} m from where he fell",
                flat_gap(f.pos, fell_at)
            );
            moved_on = true;
        }
    }
    assert!(moved_on);
    // Over the next seconds he wanders off along his path from there, no jump.
    let mut last = w.play.frogs[0].pos;
    for _ in 0..120 {
        place(&mut w, 0, 400.0, 0.0, 0.0);
        place(&mut w, 1, 450.0, 0.0, 0.0);
        w.step(&[]);
        assert!(flat_gap(w.play.frogs[0].pos, last) < 1.0, "a hop, not a teleport");
        last = w.play.frogs[0].pos;
    }
}

#[test]
fn attire_changes_no_physics_and_effects_survive_remote_snapshots() {
    let w = race(1);
    let mut a = w.racers[0].clone();
    let mut b = a.clone();
    b.attire = [3; 3];
    for _ in 0..180 {
        a.integrate(&Input::go(), &w.track, &w.tun);
        b.integrate(&Input::go(), &w.track, &w.tun);
    }
    assert_eq!(a.pos, b.pos);
    b.effects = playful::Effects {
        burn: 4.0,
        splat: 3.0,
        pill: 7.0,
        laptop: 2.0,
        cake_eat: 2.5,
        rose: 3.0,
        tumble: 2.8,
        ..Default::default()
    };
    b.coins = 9999;
    a.apply_net(&b.to_net());
    assert_eq!(a.effects, b.effects);
    assert_eq!(a.attire, b.attire);
    assert_eq!(a.coins, b.coins);
}

/// A release then a press, so the item button always has a fresh edge.
fn press_item(w: &mut World, i: usize) -> Vec<Event> {
    let mut idle = vec![Input::default(); w.racers.len()];
    w.step(&idle);
    idle[i].item = true;
    w.step(&idle);
    w.drain_events()
}

#[test]
fn a_held_item_waits_while_another_timed_item_runs() {
    for timed in [
        Item::OrangePill,
        Item::MoneyPrinter,
        Item::Magnet,
        Item::Shield,
        Item::Laptop,
        Item::MegaLaser,
        Item::LaserTrump,
        Item::EggScrambler,
    ] {
        let mut w = race(2);
        use_item(&mut w, 1, timed);
        let active = w.racers[1].active_item(&w.tun, w.play.scramble);
        assert_eq!(active.map(|a| a.item), Some(timed), "{timed:?} counts as running");
        assert!(w.racers[1].item.is_none(), "{timed:?}: the slot empties on use");
        // The next item is picked up and held, but not used.
        w.give_item(1, Item::Feather, 1);
        let ev = press_item(&mut w, 1);
        assert!(!ev.iter().any(|e| matches!(e, Event::ItemUsed { racer: 1, .. })), "{timed:?} blocks the press: {ev:?}");
        assert_eq!(w.racers[1].item, Some(Item::Feather), "{timed:?}: nothing was wasted");
        assert_eq!(w.racers[1].item_uses, 1);
    }
}

#[test]
fn the_held_item_is_usable_the_moment_the_running_one_ends() {
    let mut w = race(2);
    use_item(&mut w, 1, Item::Magnet);
    w.give_item(1, Item::Feather, 1);
    assert!(press_item(&mut w, 1).iter().all(|e| !matches!(e, Event::ItemUsed { .. })));
    // Run the magnet out (a bit over its duration) while the button stays released.
    let idle = vec![Input::default(); 2];
    for _ in 0..((w.tun.magnet_time + 0.2) / race_core::math::DT) as usize {
        w.step(&idle);
    }
    assert!(w.racers[1].active_item(&w.tun, w.play.scramble).is_none());
    let ev = press_item(&mut w, 1);
    assert!(ev.iter().any(|e| matches!(e, Event::ItemUsed { racer: 1, item: Item::Feather })), "{ev:?}");
    assert!(w.racers[1].item.is_none());
    assert!(w.racers[1].boost_t > 0.0);
}

#[test]
fn the_next_egg_can_be_collected_while_an_item_runs() {
    let mut w = race(2);
    use_item(&mut w, 1, Item::OrangePill);
    assert!(w.racers[1].item.is_none());
    w.targets[0].alive = true;
    let spot = w.targets[0].pos;
    w.racers[1].pos = spot;
    w.racers[1].vel = Vec3::ZERO;
    let ev = run(&mut w, 3, |_| vec![Input::default(); 2]);
    assert!(
        ev.iter().any(|e| matches!(e, Event::TargetDestroyed { racer: 1, item: Some(_), .. })),
        "{ev:?}"
    );
    assert!(w.racers[1].item.is_some(), "the egg is held for later");
    assert!(w.racers[1].effects.pill > 0.0, "the pill is still running");
}

#[test]
fn cake_and_scrambler_only_block_the_racer_who_used_them() {
    // The cake is over for its user at once; the others are eating, so they
    // cannot use an item.
    let mut w = race(2);
    use_item(&mut w, 0, Item::SharedCake);
    assert!(w.racers[0].active_item(&w.tun, w.play.scramble).is_none());
    // Set aside the Feather an empty cake gives.
    w.racers[0].clear_items();
    w.give_item(1, Item::Cactus, 1);
    let ev = press_item(&mut w, 1);
    assert!(!ev.iter().any(|e| matches!(e, Event::ItemUsed { racer: 1, .. })), "{ev:?}");
    w.give_item(0, Item::Cactus, 1);
    let ev = press_item(&mut w, 0);
    assert!(ev.iter().any(|e| matches!(e, Event::ItemUsed { racer: 0, item: Item::Cactus })), "{ev:?}");
    {
        let item = Item::EggScrambler;
        let mut w = race(2);
        use_item(&mut w, 0, item);
        // The effect reaches everyone, but only the user has it running.
        assert!(w.racers[0].active_item(&w.tun, w.play.scramble).is_some());
        assert!(w.racers[1].active_item(&w.tun, w.play.scramble).is_none(), "{item:?}");
        w.give_item(1, Item::Feather, 1);
        let ev = press_item(&mut w, 1);
        let used = ev.iter().any(|e| matches!(e, Event::ItemUsed { racer: 1, item: Item::Feather }));
        assert!(used, "{item:?}: {ev:?}");
    }
    // A later whisk by someone else takes the timer over.
    let mut w = race(2);
    use_item(&mut w, 0, Item::EggScrambler);
    use_item(&mut w, 1, Item::EggScrambler);
    assert!(w.racers[0].active_item(&w.tun, w.play.scramble).is_none());
    assert!(w.racers[1].active_item(&w.tun, w.play.scramble).is_some());
}

#[test]
fn the_active_item_reports_its_remaining_fraction() {
    let mut w = race(1);
    use_item(&mut w, 0, Item::OrangePill);
    let a = w.racers[0].active_item(&w.tun, 0.0).unwrap();
    assert_eq!(a.item, Item::OrangePill);
    assert!((a.total - w.tun.playful.pill_time).abs() < 1e-4);
    assert!(a.fraction() > 0.95);
    w.racers[0].effects.pill = w.tun.playful.pill_time * 0.5;
    assert!((w.racers[0].active_item(&w.tun, 0.0).unwrap().fraction() - 0.5).abs() < 1e-4);
}


#[test]
fn a_feather_over_a_weak_exit_boost_runs_at_full_power() {
    let mut w = race(1);
    w.racers[0].boost_t = 5.0;
    w.racers[0].boost_tier = 1;
    assert!(w.racers[0].boost_power(&w.tun) < 0.9);
    use_item(&mut w, 0, Item::Feather);
    assert_eq!(w.racers[0].boost_tier, 0);
    assert!((w.racers[0].boost_power(&w.tun) - 1.0).abs() < 1e-6);
}

/// Racer `i` sits on the first live egg and a few ticks pass.
fn over_an_egg(w: &mut World, i: usize) {
    let k = w.targets.iter().position(|t| !t.bad).unwrap();
    w.targets[k].alive = true;
    w.racers[i].pos = w.targets[k].pos;
    w.racers[i].vel = Vec3::ZERO;
    let n = w.racers.len();
    run(w, 2, |_| vec![Input::default(); n]);
}

#[test]
fn an_egg_hit_while_eating_cake_loads_and_waits_for_the_end_of_the_cake() {
    let mut w = race(3);
    use_item(&mut w, 1, Item::SharedCake);
    assert!(w.racers[0].effects.cake_eat > 0.0 && w.racers[0].item.is_none());
    for t in w.targets.iter_mut() {
        t.alive = false;
    }
    over_an_egg(&mut w, 0);
    let got = w.racers[0].item.expect("the egg loaded while eating");
    // Mashing the button does not use it.
    let press = |on: bool| move |_: &World| vec![Input { item: on, ..Default::default() }, Input::default(), Input::default()];
    for tick in 0..100 {
        w.step(&press(tick % 2 == 0)(&w));
        assert!(w.racers[0].effects.cake_eat > 0.0, "tick {tick}");
        assert_eq!(w.racers[0].item, Some(got), "tick {tick}");
    }
    run(&mut w, 60, |_| vec![Input::default(); 3]);
    assert_eq!(w.racers[0].effects.cake_eat, 0.0);
    assert_eq!(w.racers[0].item, Some(got), "still there when the cake ends");
}

#[test]
fn an_egg_hit_while_eating_with_a_bad_item_held_still_pays_like_any_other() {
    // No empty slot: the egg is a coin, as it always is. Eating changes nothing.
    let mut w = race(3);
    use_item(&mut w, 1, Item::SharedCake);
    w.give_item(0, Item::Cactus, 1);
    over_an_egg(&mut w, 0);
    assert_eq!(w.racers[0].item, Some(Item::Cactus));
    assert_eq!(w.racers[0].coins, 1);
}

#[test]
fn a_second_cake_does_not_take_what_an_eating_racer_has_just_loaded() {
    let mut w = race(3);
    use_item(&mut w, 1, Item::SharedCake);
    for t in w.targets.iter_mut() {
        t.alive = false;
    }
    over_an_egg(&mut w, 0);
    // Whatever the egg gave, make it a good one so a cake would want it.
    w.racers[0].clear_items();
    w.give_item(0, Item::Shield, 1);
    let eat = w.racers[0].effects.cake_eat;
    // The first cake's user is not eating, so can use another at once (the
    // first one's Feather set aside).
    w.racers[1].clear_items();
    use_item(&mut w, 1, Item::SharedCake);
    assert_eq!(w.racers[0].item, Some(Item::Shield), "taken from a racer already eating");
    assert!(w.racers[0].effects.cake_eat <= eat, "the cake is not extended");
    assert!(w.racers[1].item == Some(Item::Feather) && w.racers[1].queue.is_empty());
    assert_eq!(w.racers[1].effects.note, playful::ItemNote::Cake(0));
}

/// A three-racer world with racer 0 a bot holding a cake, the others racing.
fn bot_with_a_cake() -> World {
    let mut w = race(3);
    w.cfg.racers[0].kind = RacerKind::Bot(BotLevel::Normal);
    w.give_item(0, Item::SharedCake, 1);
    w
}

fn let_the_bot_think(w: &mut World) {
    run(w, (3.0 / math::DT) as usize, |_| vec![Input::default(); 3]);
}

#[test]
fn a_bot_keeps_its_cake_while_the_others_have_only_one_good_thing_between_them() {
    let mut w = bot_with_a_cake();
    w.racers[1].shield_t = 60.0;
    let_the_bot_think(&mut w);
    assert_eq!(w.racers[0].item, Some(Item::SharedCake), "one thing is not worth it");
    assert!(w.racers[1].shield_t > 0.0 && w.racers[1].effects.cake_eat == 0.0);
    // Bad items count for nothing.
    w.give_item(2, Item::Cactus, 1);
    w.give_item(2, Item::MegaLaser, 1);
    let_the_bot_think(&mut w);
    assert_eq!(w.racers[0].item, Some(Item::SharedCake));
}

#[test]
fn a_bot_uses_its_cake_once_two_good_things_can_be_taken_across_the_others() {
    // Two different racers, one thing each.
    let mut w = bot_with_a_cake();
    w.racers[1].shield_t = 60.0;
    w.racers[2].magnet_t = 60.0;
    let_the_bot_think(&mut w);
    assert_eq!(w.racers[0].item, Some(Item::Shield), "the cake went, and brought the shield");
    assert_eq!(w.racers[0].queue.len(), 1, "and the magnet");
    assert!(w.racers[1].effects.cake_eat > 0.0 && w.racers[1].shield_t == 0.0);
    // Two things held by the one racer count as well.
    let mut w = bot_with_a_cake();
    w.give_item(1, Item::Shield, 1);
    w.give_item(1, Item::Feather, 1);
    let_the_bot_think(&mut w);
    assert_eq!(w.racers[0].item, Some(Item::Shield), "the cake went, and brought both");
    assert_eq!(w.racers[0].queue.len(), 1);
    assert!(w.racers[1].item.is_none());
}

#[test]
fn a_racer_who_is_already_eating_is_not_counted_by_the_bots_cake_rule() {
    let mut w = bot_with_a_cake();
    w.racers[1].shield_t = 60.0;
    w.racers[2].magnet_t = 60.0;
    w.racers[2].effects.cake_eat = 20.0;
    let_the_bot_think(&mut w);
    assert_eq!(w.racers[0].item, Some(Item::SharedCake), "a cake would not touch the eater");
}

#[test]
fn the_eaters_cake_time_line_shows_once_for_at_most_a_second_and_a_half() {
    let mut w = race(3);
    let cake_time = w.tun.playful.cake_time;
    assert!(!w.racers[0].effects.cake_line(cake_time));
    use_item(&mut w, 1, Item::SharedCake);
    assert!(w.racers[0].effects.cake_line(cake_time), "shown from the first bite");
    let mut shown = 0;
    let mut gaps = 0;
    let mut was = true;
    for _ in 0..(cake_time / math::DT) as usize + 30 {
        w.step(&[Input::default(); 3]);
        let now = w.racers[0].effects.cake_line(cake_time);
        shown += usize::from(now);
        gaps += usize::from(was && !now);
        // A second cake while still eating changes nothing, so it cannot repeat.
        if shown == 30 {
            use_item(&mut w, 1, Item::SharedCake);
        }
        was = now;
    }
    assert_eq!(gaps, 1, "one run of the line");
    assert!(shown as f32 * math::DT <= playful::CAKE_LINE + 0.05, "{shown} ticks");
    assert!(w.racers[0].effects.cake_eat == 0.0 || !w.racers[0].effects.cake_line(cake_time));
}
