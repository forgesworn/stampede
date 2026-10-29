//! The starting grid: a seeded random permutation in races, unchanged elsewhere.

mod common;
use common::*;
use race_core::*;

/// Each racer's grid slot, read back from where the world put it (pole is slot 0:
/// the front row, left, then right, then the next row back).
fn slots_of(w: &World) -> Vec<usize> {
    let len = w.track.length;
    w.racers
        .iter()
        .map(|r| {
            let row = ((len - 6.0 - r.s) / 6.0).round() as usize;
            let left = r.lateral < 0.0;
            row * 2 + if left { 0 } else { 1 }
        })
        .collect()
}

fn race(seed: u64, n: usize) -> World {
    humans(TrackId::Savanna, n, seed)
}

#[test]
fn the_same_seed_gives_the_same_grid_and_it_is_a_permutation() {
    for n in 1..=8 {
        for seed in [1u64, 2, 99, 12345, u64::MAX] {
            let a = grid_slots(seed, n);
            assert_eq!(a, grid_slots(seed, n));
            let mut sorted = a.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "n {n} seed {seed}: {a:?}");
        }
    }
    // Two worlds from one config start every racer on the same spot.
    let (a, b) = (race(77, 8), race(77, 8));
    for (x, y) in a.racers.iter().zip(&b.racers) {
        assert_eq!(x.pos, y.pos);
    }
    assert_ne!(
        a.racers.iter().map(|r| r.pos).collect::<Vec<_>>(),
        race(78, 8).racers.iter().map(|r| r.pos).collect::<Vec<_>>(),
        "another seed, another grid"
    );
}

#[test]
fn the_world_puts_each_seat_in_its_drawn_slot() {
    for seed in [3u64, 4, 5, 600] {
        let w = race(seed, 8);
        assert_eq!(slots_of(&w), grid_slots(seed, 8), "seed {seed}");
    }
    // Fewer racers: the front rows still fill from pole.
    let w = race(9, 3);
    let mut s = slots_of(&w);
    s.sort_unstable();
    assert_eq!(s, vec![0, 1, 2]);
}

#[test]
fn over_many_seeds_the_host_is_spread_across_the_grid_and_not_always_on_pole() {
    let n = 8;
    let seeds = 2000u64;
    let mut host = vec![0u32; n];
    let mut sum = vec![0u64; n];
    for seed in 0..seeds {
        let g = grid_slots(seed * 7919 + 13, n);
        host[g[0]] += 1;
        for (i, s) in g.iter().enumerate() {
            sum[i] += *s as u64;
        }
    }
    let even = seeds as f32 / n as f32;
    for (slot, &c) in host.iter().enumerate() {
        assert!(
            (c as f32) > even * 0.7 && (c as f32) < even * 1.3,
            "host on slot {slot} in {c} of {seeds} races (even would be {even})"
        );
    }
    assert!(host[0] < (seeds as f32 * 0.2) as u32, "the host is not on pole every time");
    // Every seat averages the middle of the grid, so nobody is favoured.
    for (i, s) in sum.iter().enumerate() {
        let mean = *s as f32 / seeds as f32;
        assert!((mean - 3.5).abs() < 0.3, "seat {i} averages slot {mean}");
    }
}

#[test]
fn other_modes_keep_seat_zero_on_pole() {
    for mode in [RaceMode::TimeTrial, RaceMode::Attract] {
        for seed in 0..20 {
            let mut cfg = RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, seed);
            cfg.mode = mode;
            let w = World::new(cfg);
            assert_eq!(slots_of(&w), vec![0, 1, 2, 3], "{mode:?} seed {seed}");
        }
    }
    for seed in 0..20 {
        let w = World::new(RaceConfig::challenge(ChallengeId::Elimination, 0, seed));
        assert_eq!(slots_of(&w)[0], 0, "challenges start as before");
    }
}

#[test]
fn places_match_the_drawn_grid_from_the_first_frame() {
    let mut back_row_seat_zero = 0;
    for seed in [3u64, 4, 5, 600, 77, 2024] {
        let mut w = race(seed, 8);
        let slots = slots_of(&w);
        let places: Vec<u8> = w.racers.iter().map(|r| r.place).collect();
        for (i, r) in w.racers.iter().enumerate() {
            // Side-by-side seats are level; the front row holds 1st and 2nd, and so on.
            assert_eq!((r.place as usize - 1) / 2, slots[i] / 2, "seed {seed} seat {i}: place {}", r.place);
        }
        let mut sorted = places.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (1..=8).collect::<Vec<u8>>(), "every place is given once");
        if slots[0] >= 2 {
            back_row_seat_zero += 1;
            assert!(w.racers[0].place > 2, "seat 0 on the back rows is not 1st or 2nd");
        }
        // The countdown changes nothing, and the rows hold once the race is on.
        while w.phase == RacePhase::Countdown {
            w.step(&[]);
            let now: Vec<u8> = w.racers.iter().map(|r| r.place).collect();
            assert_eq!(now, places, "seed {seed}: places changed during the countdown");
        }
        w.step(&[]);
        for (i, r) in w.racers.iter().enumerate() {
            assert_eq!((r.place as usize - 1) / 2, slots[i] / 2, "seed {seed} seat {i} just after GO");
        }
    }
    assert!(back_row_seat_zero > 0, "the seeds include seat 0 on a back row");
}
