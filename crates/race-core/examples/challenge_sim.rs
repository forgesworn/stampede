//! Plays every challenge with a bot in the player's seat (seat 0) and prints
//! the score and medal, to sanity-check the medal thresholds.
//!
//! `cargo run -p race-core --example challenge_sim --release [-- <level: easy|normal|hard> <seeds>]`

use race_core::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let level = match args.get(1).map(|s| s.as_str()) {
        Some("easy") => BotLevel::Easy,
        Some("hard") => BotLevel::Hard,
        _ => BotLevel::Normal,
    };
    let seeds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    for id in ChallengeId::ALL {
        let th = id.medal_thresholds();
        println!(
            "== {} on {:?}: {} ({}), medals {:?} ==",
            id.title(),
            id.track(),
            id.score_label(),
            if id.higher_is_better() {
                "higher"
            } else {
                "lower"
            },
            th
        );
        for seed in 1..=seeds {
            let mut cfg = RaceConfig::challenge(id, 0, seed);
            cfg.racers[0].kind = RacerKind::Bot(level);
            let mut w = World::new(cfg);
            while w.phase != RacePhase::Finished && w.tick < 60 * 400 {
                w.step(&[]);
                w.drain_events();
            }
            let cs = w.challenge.as_ref().unwrap();
            println!(
                "  seed {seed}: score {:7.2}  medal {}  done {}  t {:6.1} s  progress {}/{}  out {:?}",
                cs.score, cs.medal, cs.done, cs.time, cs.progress, cs.goal, cs.eliminated
            );
        }
    }
}
