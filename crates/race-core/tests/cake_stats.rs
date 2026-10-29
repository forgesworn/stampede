//! Measures how much each Shared Cake takes in full seeded bot races, and how
//! the bots hold and use the good power-ups. Slow, so ignored; run with
//!
//!   cargo test -p race-core --test cake_stats -- --ignored --nocapture

use race_core::math::mix64;
use race_core::playful::ItemNote;
use race_core::*;
use std::collections::BTreeMap;

const TRACKS: [TrackId; 4] = [TrackId::Savanna, TrackId::Dune, TrackId::Canyon, TrackId::Training];
/// Races per track; `CAKE_STATS_SEEDS` overrides it.
const SEEDS: u64 = 10;
/// Chance per tick that the human-like seat lets go of a cake it holds
/// (about one use in three seconds after the pickup).
const HUMAN_USE_PER_TICK: u64 = 180;

#[derive(Default)]
struct Dist {
    /// Takes of 0, 1, 2 and 3 or more.
    n: [u32; 4],
}

impl Dist {
    fn add(&mut self, took: u8) {
        self.n[usize::from(took).min(3)] += 1;
    }
    fn total(&self) -> u32 {
        self.n.iter().sum()
    }
    fn show(&self, name: &str) -> String {
        let t = self.total().max(1) as f32;
        let p = |k: usize| 100.0 * self.n[k] as f32 / t;
        format!(
            "{name:<22} uses {:>4}   0: {:>5.1}%  1: {:>5.1}%  2: {:>5.1}%  3+: {:>5.1}%",
            self.total(),
            p(0),
            p(1),
            p(2),
            p(3)
        )
    }
}

#[derive(Default)]
struct Use {
    got: u32,
    used: u32,
    /// Seconds from pickup to use, one entry per use.
    held: Vec<f32>,
}

#[test]
#[ignore]
fn cake_take_distribution_and_bot_item_use() {
    let mut bots = Dist::default();
    let mut human = Dist::default();
    let mut cakes_rolled = [0u32; 2]; // [bots, human seat]
    let mut pickups = [0u32; 2];
    let mut uses: BTreeMap<String, Use> = BTreeMap::new();
    let mut races = 0;
    let seeds = std::env::var("CAKE_STATS_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(SEEDS);
    for &track in &TRACKS {
        for seed in 0..seeds {
            let mut cfg = RaceConfig::all_bots(track, 8, BotLevel::Normal, 1000 + seed);
            cfg.racers[0].kind = RacerKind::Human;
            let mut w = World::new(cfg);
            let mut got_at = [0u64; 8];
            while w.phase != RacePhase::Finished && w.tick < 60 * 420 {
                // Seat 0: driven like a bot, but a cake goes at a random moment.
                let racing = w.phase != RacePhase::Countdown;
                update_mem(&mut w.bot_mem[0], &w.racers[0], racing);
                let mut inputs = vec![Input::default(); 8];
                inputs[0] = bot_input(&w, 0, BotLevel::Normal);
                if w.racers[0].item == Some(Item::SharedCake) {
                    let roll = mix64(seed ^ mix64(w.tick) ^ 0x5eed) % HUMAN_USE_PER_TICK;
                    inputs[0].item = roll == 0 && !w.racers[0].prev_input.item;
                }
                w.step(&inputs);
                for e in w.drain_events() {
                    match e {
                        Event::TargetDestroyed { racer, item: Some(it), .. } => {
                            got_at[racer as usize] = w.tick;
                            uses.entry(format!("{it:?}")).or_default().got += 1;
                            if it == Item::SharedCake {
                                cakes_rolled[usize::from(racer == 0)] += 1;
                            }
                            pickups[usize::from(racer == 0)] += 1;
                        }
                        Event::ItemUsed { racer, item } => {
                            let u = uses.entry(format!("{item:?}")).or_default();
                            u.used += 1;
                            u.held.push((w.tick - got_at[racer as usize]) as f32 / 60.0);
                            if item == Item::SharedCake {
                                let ItemNote::Cake(took) = w.racers[racer as usize].effects.note else {
                                    panic!("a cake was used without a note");
                                };
                                if racer == 0 {
                                    human.add(took);
                                } else {
                                    bots.add(took);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            races += 1;
        }
    }
    println!("races {races}   eggs picked up: bots {} human seat {}", pickups[0], pickups[1]);
    println!("cakes rolled: bots {} human seat {}", cakes_rolled[0], cakes_rolled[1]);
    println!("{}", bots.show("bots use (rule)"));
    println!("{}", human.show("seat 0 (random moment)"));
    println!("\nitem            got  used  mean hold s  median  max   (pickup to use, all racers)");
    for (name, u) in &uses {
        let mut h = u.held.clone();
        h.sort_by(|a, b| a.total_cmp(b));
        let mean = h.iter().sum::<f32>() / h.len().max(1) as f32;
        let med = h.get(h.len() / 2).copied().unwrap_or(0.0);
        let max = h.last().copied().unwrap_or(0.0);
        println!("{name:<14} {:>4} {:>5}   {mean:>8.1} {med:>9.1} {max:>6.1}", u.got, u.used);
    }
}
