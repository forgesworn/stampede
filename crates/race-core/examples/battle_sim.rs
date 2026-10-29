//! Runs all-bot battles on both arenas and prints duration, feathers popped
//! per minute, the winner, the out order and the longest time anyone was stuck.
//!
//! `cargo run -p race-core --example battle_sim --release [-- <easy|normal|hard> <bots> <seeds>]`

use race_core::math::DT;
use race_core::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level = match args.get(1).map(|s| s.as_str()) {
        Some("easy") => BotLevel::Easy,
        Some("hard") => BotLevel::Hard,
        _ => BotLevel::Normal,
    };
    let bots: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let seeds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    for arena in TrackId::ARENAS {
        for seed in 1..=seeds {
            let cfg = RaceConfig::battle_bots(arena, bots, level, BattleRules::default(), seed);
            let mut w = World::new(cfg);
            let (mut pops, mut steals, mut zaps, mut items) = (0, 0, 0, 0);
            let limit = 60 * 60 * 4;
            while w.phase != RacePhase::Finished && w.tick < limit {
                w.step(&[]);
                for e in w.drain_events() {
                    match e {
                        Event::FeatherLost { .. } => pops += 1,
                        Event::FeatherStolen { .. } => steals += 1,
                        Event::LaserZap { .. } => zaps += 1,
                        Event::ItemUsed { .. } => items += 1,
                        _ => {}
                    }
                }
            }
            let b = w.battle.as_ref().unwrap();
            let t = w.race_time().max(DT);
            let stuck = w.max_stuck.iter().cloned().fold(0.0f32, f32::max);
            let rank = w.standings();
            println!(
                "{:<14} {:?} seed {seed}: {:5.1} s, {pops} feathers ({:.1}/min), {steals} steals, {zaps} zaps, {items} items, winner {} ({} feathers, {} hits), out {:?}, max stuck {:.1} s{}",
                w.track.name,
                level,
                t,
                pops as f32 / t * 60.0,
                rank[0],
                b.feathers[rank[0]],
                b.hits[rank[0]],
                b.out_order,
                stuck,
                if w.phase != RacePhase::Finished { "  !! not finished" } else { "" }
            );
        }
    }
}
