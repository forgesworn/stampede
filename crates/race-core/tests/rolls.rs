//! Gap-aware item rolls and the Laser Trump curve (pure functions, no world).

use race_core::*;
use rand::SeedableRng;
use rand_pcg::Pcg32;

fn col(item: Item) -> usize {
    ITEM_ROLLS.iter().position(|r| r.item == item).unwrap()
}

/// Rolls `draws` times with a fresh seeded rng and returns how often each
/// column of `ITEM_ROLLS` came up, as a fraction.
fn shares(place: u8, n: usize, gaps: RollGaps, draws: u32) -> [f32; 8] {
    let mut rng = Pcg32::seed_from_u64(0x57A3_9EDE);
    let mut counts = [0u32; 8];
    for _ in 0..draws {
        let roll = roll_item(&mut rng, place, n, gaps);
        counts[col(roll.item)] += 1;
    }
    counts.map(|c| c as f32 / draws as f32)
}

fn open_field() -> RollGaps {
    RollGaps {
        ahead: Some(20.0),
        behind: Some(20.0),
        others_hold_coins: true,
    }
}

#[test]
fn every_odds_row_is_a_percentage_and_the_scrambler_is_three() {
    for row in ITEM_ODDS {
        assert_eq!(row.iter().sum::<u16>(), 100);
        assert_eq!(row[col(Item::EggScrambler)], 3);
    }
}

#[test]
fn reach_constants_match_the_real_beam() {
    assert_eq!(LASER_REACH, Tunables::default().mega_range);
    assert_eq!(PRINTER_REACH, 55.0);
}

#[test]
fn trump_chance_is_zero_for_first_and_grows_with_place() {
    for n in 3..=8usize {
        assert_eq!(trump_chance(1, n, Some(100.0)), 0.0);
        let mut prev = 0.0;
        for place in 1..=n as u8 {
            let c = trump_chance(place, n, Some(100.0));
            assert!(c >= prev, "n={n} place {place}: {c} < {prev}");
            assert!((0.0..=1.0).contains(&c));
            prev = c;
        }
    }
}

#[test]
fn trump_chance_matches_the_designed_curve() {
    let expect = [(8u8, 0.45), (7, 0.33), (6, 0.23), (5, 0.15), (4, 0.08), (3, 0.04), (2, 0.009)];
    for (place, want) in expect {
        let got = trump_chance(place, 8, Some(100.0));
        assert!((got - want).abs() < 0.005, "n=8 place {place}: {got} vs {want}");
    }
    assert!((trump_chance(3, 3, Some(100.0)) - 0.45).abs() < 0.005);
    assert!((trump_chance(2, 3, Some(100.0)) - 0.1125).abs() < 0.005);
}

#[test]
fn trump_chance_needs_three_racers_and_somebody_ahead() {
    assert_eq!(trump_chance(2, 2, Some(100.0)), 0.0);
    assert_eq!(trump_chance(8, 8, None), 0.0);
    // A close pack cuts it to a quarter; 40 m or more leaves it whole.
    let full = trump_chance(8, 8, Some(40.0));
    assert!((trump_chance(8, 8, Some(5.0)) - full * 0.25).abs() < 1e-6);
    assert!((trump_chance(8, 8, Some(20.0)) - full * 0.5).abs() < 1e-6);
    assert_eq!(trump_chance(8, 8, Some(400.0)), full);
}

#[test]
fn the_leader_never_rolls_laser_eyes_or_the_printer_and_pills_are_halved() {
    let gaps = RollGaps {
        ahead: None,
        behind: Some(20.0),
        others_hold_coins: true,
    };
    let s = shares(1, 8, gaps, 20_000);
    assert_eq!(s[col(Item::MegaLaser)], 0.0);
    assert_eq!(s[col(Item::MoneyPrinter)], 0.0);
    // Front bucket weights: Rug 34, Hack 7, Pill 16 x 0.5, Scrambler 3, Cake 20.
    let total = 34.0 + 7.0 + 8.0 + 3.0 + 20.0;
    assert!((s[col(Item::OrangePill)] - 8.0 / total).abs() < 0.015);
}

#[test]
fn last_place_never_rolls_a_rug() {
    let gaps = RollGaps {
        ahead: Some(30.0),
        behind: None,
        others_hold_coins: true,
    };
    for place in [6u8, 7, 8] {
        assert_eq!(shares(place, 8, gaps, 5_000)[col(Item::RugPull)], 0.0);
    }
}

#[test]
fn the_hack_needs_coins_to_steal() {
    let gaps = RollGaps {
        others_hold_coins: false,
        ..open_field()
    };
    for place in 1..=8u8 {
        assert_eq!(shares(place, 8, gaps, 5_000)[col(Item::Laptop)], 0.0);
    }
    // And with coins about it is gap-independent: the bucket's own share.
    let s = shares(8, 8, RollGaps { ahead: Some(300.0), behind: None, others_hold_coins: true }, 20_000);
    // Laser, Printer and Rug are out of reach; Pill is at its full 1.5 x 27.
    let total = 22.0 + 40.5 + 3.0 + 2.0 + 8.0;
    assert!((s[col(Item::Laptop)] - 22.0 / total).abs() < 0.015);
}

#[test]
fn a_racer_far_behind_everyone_never_rolls_a_ranged_weapon() {
    for place in [5u8, 8] {
        let gaps = RollGaps {
            ahead: Some(200.0),
            behind: None,
            others_hold_coins: true,
        };
        let s = shares(place, 8, gaps, 10_000);
        assert_eq!(s[col(Item::MoneyPrinter)], 0.0);
        assert_eq!(s[col(Item::MegaLaser)], 0.0);
    }
}

#[test]
fn pack_position_rolls_sit_near_expectation() {
    // Bucket 2, 20 m either side, coins about: Laser 18, Rug 9, Hack 17,
    // Pill 20 x (1 + 0.5 x 20/60), Scrambler 3, Cake 8, Printer 16, Rose 9.
    let w = [18.0, 9.0, 17.0, 20.0 * (1.0 + 0.5 / 3.0), 3.0, 8.0, 16.0, 9.0];
    let total: f32 = w.iter().sum();
    let s = shares(5, 8, open_field(), 20_000);
    for i in 0..8 {
        assert!((s[i] - w[i] / total).abs() < 0.015, "column {i}: {} vs {}", s[i], w[i] / total);
    }
}

#[test]
fn straggler_rolls_sit_near_expectation() {
    // Bucket 3, 100 m behind the next racer, nobody behind: Laser 15 x 1/3,
    // Rug 0, Hack 22, Pill 27 x 1.5, Scrambler 3, Cake 2, Printer 21 x (2 - 100/55),
    // Rose 8.
    let w = [15.0 / 3.0, 0.0, 22.0, 40.5, 3.0, 2.0, 21.0 * (2.0 - 100.0 / 55.0), 8.0];
    let total: f32 = w.iter().sum();
    let gaps = RollGaps {
        ahead: Some(100.0),
        behind: None,
        others_hold_coins: true,
    };
    let s = shares(8, 8, gaps, 20_000);
    for i in 0..8 {

        assert!((s[i] - w[i] / total).abs() < 0.015, "column {i}: {} vs {}", s[i], w[i] / total);
    }
}

#[test]
fn the_same_seed_and_inputs_give_the_same_rolls() {
    let run = || {
        let mut rng = Pcg32::seed_from_u64(77);
        (0..200)
            .map(|k| roll_item(&mut rng, 1 + (k % 8) as u8, 8, open_field()))
            .collect::<Vec<_>>()
    };
    assert_eq!(run(), run());
}

#[test]
fn the_cake_has_half_the_weight_it_had_and_each_row_still_adds_up() {
    let cake = col(Item::SharedCake);
    // Was 40, 24, 16 and 5 (the last rounds down).
    let now: Vec<u16> = ITEM_ODDS.iter().map(|row| row[cake]).collect();
    assert_eq!(now, [20, 12, 8, 2]);
    // Nothing else lost weight to make room, bar the Rose's 0 at the front.
    let old = [
        [5u16, 25, 5, 12, 3, 40, 10, 0],
        [15, 15, 10, 15, 3, 24, 15, 3],
        [16, 8, 16, 18, 3, 16, 15, 8],
        [15, 2, 21, 26, 3, 5, 20, 8],
    ];
    for (row, was) in ITEM_ODDS.iter().zip(old) {
        for c in 0..8 {
            if c != cake {
                assert!(row[c] >= was[c], "column {c} lost weight: {row:?}");
            }
        }
    }
}
