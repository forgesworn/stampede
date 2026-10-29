//! Runs an all-bot race on every race track and prints lap times, the
//! finishing order, event counts and the longest time any racer was stuck.
//!
//! `cargo run -p race-core --example race_sim --release [-- <level: easy|normal|hard> <seed>]`

use std::collections::BTreeMap;

use race_core::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level = match args.get(1).map(|s| s.as_str()) {
        Some("easy") => BotLevel::Easy,
        Some("hard") => BotLevel::Hard,
        _ => BotLevel::Normal,
    };
    let seed: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7);
    for id in TrackId::RACE_ALL {
        let mut w = World::new(RaceConfig::all_bots(id, 8, level, seed));
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let limit = 60 * 60 * 6;
        while w.phase != RacePhase::Finished && w.tick < limit {
            w.step(&[]);
            for e in w.drain_events() {
                let name = format!("{e:?}");
                let name = name.split([' ', '{']).next().unwrap_or("").to_string();
                *counts.entry(name).or_default() += 1;
            }
        }
        println!(
            "== {} ({:.0} m, {:?}, seed {seed}) ==",
            w.track.name, w.track.length, level
        );
        if w.phase != RacePhase::Finished {
            println!("  !! race did not finish in {} s", limit / 60);
        }
        let mut best = f32::INFINITY;
        let mut worst = 0.0f32;
        for (i, laps) in w.lap_times.iter().enumerate() {
            for &l in laps {
                best = best.min(l);
                worst = worst.max(l);
            }
            let _ = i;
        }
        for (p, &i) in w.standings().iter().enumerate() {
            let r = &w.racers[i];
            let laps: Vec<String> = w.lap_times[i].iter().map(|t| format!("{t:5.1}")).collect();
            println!(
                "  {}. {:<8} {:>6}  laps [{}]  coins {:>2}  max stuck {:.1} s",
                p + 1,
                w.cfg.racers[i].name,
                r.finished_at
                    .map(|t| format!("{t:.1}"))
                    .unwrap_or_else(|| "DNF".into()),
                laps.join(" "),
                r.coins,
                w.max_stuck[i]
            );
        }
        let max_stuck = w.max_stuck.iter().cloned().fold(0.0, f32::max);
        println!("  best lap {best:.1} s, worst lap {worst:.1} s (ratio {:.2}), max stuck {max_stuck:.1} s", worst / best);
        let summary: Vec<String> = counts
            .iter()
            .filter(|(k, _)| k.as_str() != "Footstep")
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        println!("  events: {}", summary.join(", "));
    }
}
