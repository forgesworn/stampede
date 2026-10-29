//! The Rose: a rose seller who pesters every other racer in turn.

mod common;
use common::*;
use race_core::playful::{RoseSeller, ROSE_LEAVE, ROSE_SALE, ROSE_WAIT};
use race_core::*;
use std::sync::Arc;

/// Idle humans on the proving ground's first straight, 20 m apart.
fn race(n: usize) -> World {
    let track = proving_ground();
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

fn send_rose(w: &mut World, i: usize) {
    w.give_item(i, Item::Rose, 1);
    let mut inputs = vec![Input::default(); w.racers.len()];
    inputs[i].item = true;
    w.step(&inputs);
}

fn idle(w: &mut World, ticks: usize) -> Vec<Event> {
    run(w, ticks, |w| vec![Input::default(); w.racers.len()])
}

fn seller(w: &World) -> RoseSeller {
    w.play.roses[0]
}

/// Steps until the seller is alongside `victim`, at most `secs` seconds.
fn until_selling_to(w: &mut World, victim: u8, secs: f32) {
    for _ in 0..(secs / math::DT) as usize {
        if w.play.roses.first().is_some_and(|r| r.selling() && r.victim == victim) {
            return;
        }
        idle(w, 1);
    }
    panic!("never reached racer {victim}: {:?}", w.play.roses);
}

#[test]
fn the_seller_pesters_each_other_racer_in_turn_then_runs_off() {
    let mut w = race(3);
    // The same world without a seller, to measure the shove against.
    let mut calm = w.clone();
    send_rose(&mut w, 0);
    calm.step(&[Input::default(); 3]);
    assert_eq!(w.play.roses.len(), 1);
    assert_eq!(w.racers[0].effects.note, playful::ItemNote::Rose, "the user is told");
    assert!(w.racers[0].effects.note_t > 0.0);
    assert!(w.racers[0].item.is_none());
    // Nearest first: racer 1, 20 m away, reached in well under a second.
    until_selling_to(&mut w, 1, 2.0);
    let start = w.tick;
    let mut pushed = 0.0f32;
    while w.play.roses[0].victim == 1 && w.play.roses[0].selling() {
        assert!(w.racers[1].effects.rose > 0.0, "racer 1 is told");
        assert_eq!(w.racers[2].effects.rose, 0.0);
        assert_eq!(w.racers[1].effects.dismount, 0.0, "never unseated");
        idle(&mut w, 1);
        while calm.tick < w.tick {
            calm.step(&[Input::default(); 3]);
        }
        pushed = pushed.max((w.racers[1].lateral - calm.racers[1].lateral).abs());
    }
    let sale = (w.tick - start) as f32 * math::DT;
    assert!((sale - ROSE_SALE).abs() < 0.05, "sold for {sale} s");
    assert!(pushed > 0.5, "leaned on sideways: {pushed} m");
    // Then racer 2, from where racer 1 is.
    until_selling_to(&mut w, 2, 2.0);
    assert_eq!(w.racers[1].effects.rose, 0.0);
    assert_eq!(seller(&w).tried & 0b110, 0b010);
    idle(&mut w, (ROSE_SALE / math::DT) as usize + 2);
    assert!(seller(&w).leaving(), "everyone tried: {:?}", seller(&w));
    assert_eq!(w.racers[2].effects.rose, 0.0);
    assert_eq!(w.racers[0].effects.rose, 0.0, "the user is never pestered");
    idle(&mut w, (ROSE_LEAVE / math::DT) as usize + 2);
    assert!(w.play.roses.is_empty(), "gone");
}

#[test]
fn the_seller_is_faster_than_a_boosted_racer() {
    let mut w = race(2);
    place(&mut w, 1, 300.0, 0.0, 40.0);
    send_rose(&mut w, 0);
    // Racer 1 drives away boosted; the seller still gets there.
    let mut caught = false;
    for _ in 0..(12.0 / math::DT) as usize {
        w.racers[1].grant_boost(1.0);
        let inp = vec![Input::default(), Input::go()];
        w.step(&inp);
        if w.play.roses[0].selling() {
            caught = true;
            break;
        }
    }
    assert!(caught, "{:?}", w.play.roses);
}

#[test]
fn a_shield_sends_the_seller_on_and_is_used_up() {
    let mut w = race(3);
    w.racers[1].shield_t = 5.0;
    send_rose(&mut w, 0);
    let mut ev = Vec::new();
    for _ in 0..60 {
        ev.extend(idle(&mut w, 1));
        assert!(!(w.play.roses[0].selling() && w.play.roses[0].victim == 1), "a shield blocks the sale");
    }
    assert_eq!(w.racers[1].shield_t, 0.0);
    assert!(ev.iter().any(|e| matches!(e, Event::ShieldBlock { racer: 1, by: Some(0) })), "{ev:?}");
    until_selling_to(&mut w, 2, 2.0);
}

#[test]
fn immune_and_finished_racers_are_skipped_and_nobody_left_means_it_leaves() {
    let mut w = race(3);
    w.racers[1].effects.pill = 6.0;
    w.racers[2].finished_at = Some(10.0);
    send_rose(&mut w, 0);
    idle(&mut w, 1);
    assert!(seller(&w).leaving(), "{:?}", seller(&w));
    idle(&mut w, (ROSE_LEAVE / math::DT) as usize + 2);
    assert!(w.play.roses.is_empty());
}

#[test]
fn a_racer_who_is_down_is_waited_for_not_skipped() {
    for down in ["tumble", "dismount", "respawn"] {
        let mut w = race(3);
        let knock = |w: &mut World| match down {
            "tumble" => w.racers[1].effects.tumble = 1.0,
            "dismount" => w.racers[1].effects.dismount = 1.0,
            _ => w.racers[1].respawn_t = 1.0,
        };
        knock(&mut w);
        send_rose(&mut w, 0);
        // The seller reaches racer 1 and waits beside them until they are up.
        for _ in 0..(0.8 / math::DT) as usize {
            idle(&mut w, 1);
            assert_eq!(seller(&w).victim, 1, "{down}: {:?}", seller(&w));
            assert!(!seller(&w).selling(), "{down}: no sale while down");
            assert_eq!(seller(&w).tried, 0, "{down}");
        }
        // Idle, a jockey on foot never jumps back on: back on the bird by hand.
        w.racers[1].effects.dismount = 0.0;
        until_selling_to(&mut w, 1, 4.0);
        // Down again mid-sale: the sale carries on.
        knock(&mut w);
        idle(&mut w, 1);
        assert!(seller(&w).selling() && seller(&w).victim == 1, "{down}: {:?}", seller(&w));
        idle(&mut w, (ROSE_SALE / math::DT) as usize);
        assert_eq!(seller(&w).tried & 0b10, 0b10, "{down}: served");
        assert_eq!(w.racers[1].effects.rose, 0.0, "{down}");
    }
}

#[test]
fn a_racer_who_stays_down_is_given_up_on() {
    let mut w = race(3);
    w.racers[1].effects.dismount = 1.0;
    send_rose(&mut w, 0);
    // A jockey left on foot: the seller waits beside them, then moves on.
    for _ in 0..((ROSE_WAIT + 3.0) / math::DT) as usize {
        w.racers[1].effects.dismount = 1.0;
        idle(&mut w, 1);
    }
    assert_eq!(seller(&w).tried & 0b10, 0b10, "{:?}", seller(&w));
    assert_eq!(w.racers[1].effects.rose, 0.0);
    until_selling_to(&mut w, 2, 4.0);
}

#[test]
fn a_racer_who_finishes_mid_sale_is_dropped() {
    let mut w = race(3);
    send_rose(&mut w, 0);
    until_selling_to(&mut w, 1, 2.0);
    w.racers[1].finished_at = Some(12.0);
    idle(&mut w, 1);
    assert_eq!(w.racers[1].effects.rose, 0.0);
    until_selling_to(&mut w, 2, 2.0);
}

#[test]
fn cake_on_a_pestered_racer_sends_the_seller_on() {
    let mut w = race(3);
    send_rose(&mut w, 0);
    until_selling_to(&mut w, 1, 2.0);
    w.give_item(2, Item::SharedCake, 1);
    let mut inputs = vec![Input::default(); 3];
    inputs[2].item = true;
    w.step(&inputs);
    assert_eq!(w.racers[1].effects.rose, 0.0);
    assert!(w.racers[1].effects.cake_eat > 0.0);
    // Racer 1 is done; racer 2 used the cake but is next in line.
    until_selling_to(&mut w, 2, 2.0);
}

#[test]
fn a_second_rose_sends_the_same_seller_round_again() {
    let mut w = race(2);
    send_rose(&mut w, 0);
    until_selling_to(&mut w, 1, 2.0);
    send_rose(&mut w, 0);
    assert_eq!(w.play.roses.len(), 1, "one seller per user");
    assert_eq!(seller(&w).again, 1);
    idle(&mut w, (ROSE_SALE / math::DT) as usize + 2);
    // Round two: racer 1 again.
    until_selling_to(&mut w, 1, 2.0);
    idle(&mut w, (ROSE_SALE / math::DT) as usize + 2);
    assert!(seller(&w).leaving());
}

#[test]
fn a_new_round_starts_with_the_nearest_other_racer() {
    let mut w = race(4);
    send_rose(&mut w, 0);
    until_selling_to(&mut w, 1, 2.0);
    send_rose(&mut w, 0);
    until_selling_to(&mut w, 2, ROSE_SALE + 2.0);
    until_selling_to(&mut w, 3, ROSE_SALE + 2.0);
    // Round two: not racer 3 again straight away but the nearest other, racer 2,
    // and still everyone in the round.
    while seller(&w).selling() {
        idle(&mut w, 1);
    }
    while !seller(&w).selling() {
        idle(&mut w, 1);
    }
    assert_eq!(seller(&w).victim, 2, "{:?}", seller(&w));
    let mut served = vec![2];
    while !seller(&w).leaving() {
        idle(&mut w, 1);
        let r = seller(&w);
        if r.selling() && served.last() != Some(&r.victim) {
            served.push(r.victim);
        }
    }
    served.sort();
    assert_eq!(served, [1, 2, 3]);
}

#[test]
fn a_rose_used_while_the_seller_is_leaving_calls_it_back() {
    let mut w = race(2);
    send_rose(&mut w, 0);
    until_selling_to(&mut w, 1, 2.0);
    idle(&mut w, (ROSE_SALE / math::DT) as usize + 2);
    assert!(seller(&w).leaving());
    idle(&mut w, 10);
    send_rose(&mut w, 0);
    assert_eq!(w.racers[0].effects.note, playful::ItemNote::Rose);
    assert_eq!(w.play.roses.len(), 1, "the same seller");
    assert!(!seller(&w).leaving(), "{:?}", seller(&w));
    until_selling_to(&mut w, 1, 2.0);
    idle(&mut w, (ROSE_SALE / math::DT) as usize + 2);
    assert!(seller(&w).leaving());
}

#[test]
fn sellers_survive_the_wire_and_bad_ones_are_rejected() {
    let mut w = race(3);
    send_rose(&mut w, 0);
    until_selling_to(&mut w, 1, 2.0);
    let bytes = bincode::serialize(&w.play).unwrap();
    let back: playful::PlayState = bincode::deserialize(&bytes).unwrap();
    assert_eq!(back, w.play);
    for bad in [
        RoseSeller { owner: 8, ..seller(&w) },
        RoseSeller { victim: 9, ..seller(&w) },
        RoseSeller { t: f32::NAN, ..seller(&w) },
        RoseSeller { t: 6.0, ..seller(&w) },
        RoseSeller { side: 0, ..seller(&w) },
        RoseSeller { again: 9, ..seller(&w) },
    ] {
        let mut p = w.play.clone();
        p.roses[0] = bad;
        let bytes = bincode::serialize(&p).unwrap();
        assert!(bincode::deserialize::<playful::PlayState>(&bytes).is_err(), "{bad:?}");
    }
    let mut p = w.play.clone();
    p.roses = vec![seller(&w); playful::MAX_ROSES + 1];
    assert!(bincode::deserialize::<playful::PlayState>(&bincode::serialize(&p).unwrap()).is_err());
}

#[test]
fn rose_is_only_rolled_mid_and_back_and_never_alone() {
    let rose = ITEM_ROLLS.iter().position(|r| r.item == Item::Rose).unwrap();
    assert_eq!(ITEM_ODDS[0][rose], 0, "never at the front");
    assert!(ITEM_ODDS[2][rose] > 0 && ITEM_ODDS[3][rose] > 0);
    assert_eq!(gap_factor(rose, &RollGaps::default()), 0.0, "nobody to sell to");
    assert!(!Item::Rose.is_good());
}
