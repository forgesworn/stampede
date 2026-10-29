//! Challenges (spec 16): each score advances from the sim's own events, ends
//! correctly and awards the right medal; Elimination removes one racer per 25 s.

mod common;
use common::*;
use race_core::math::DT;
use race_core::*;

/// A challenge world whose seat 0 is a bot of `level` (so it plays itself).
fn bot_world(id: ChallengeId, level: BotLevel, seed: u64) -> World {
    let mut cfg = RaceConfig::challenge(id, 0, seed);
    cfg.racers[0].kind = RacerKind::Bot(level);
    World::new(cfg)
}

fn idle(n: usize) -> Vec<Input> {
    vec![Input::default(); n]
}

/// Steps until the race ends (or `limit_s`), collecting events.
fn play(w: &mut World, limit_s: f32) -> Vec<Event> {
    let mut ev = Vec::new();
    while w.phase != RacePhase::Finished && (w.tick as f32) * DT < limit_s {
        w.step(&[]);
        ev.extend(w.drain_events());
    }
    ev
}

#[test]
fn definitions_are_consistent() {
    assert_eq!(ChallengeId::ALL.len(), 10);
    let want = [
        (ChallengeId::CoinRush, TrackId::Oasis),
        (ChallengeId::EggHunt, TrackId::Baobab),
        (ChallengeId::Elimination, TrackId::Dune),
        (ChallengeId::DriftKing, TrackId::Canyon),
        (ChallengeId::BashBrawl, TrackId::Training),
        (ChallengeId::TurboTimeAttack, TrackId::Halving),
        (ChallengeId::LaserSniper, TrackId::Volcano),
        (ChallengeId::RampRider, TrackId::Oasis),
        (ChallengeId::NightCoinRush, TrackId::Halving),
        (ChallengeId::LastFeather, TrackId::DustBowl),
    ];
    for (i, (id, track)) in want.iter().enumerate() {
        assert_eq!(ChallengeId::ALL[i], *id);
        assert_eq!(id.number() as usize, i + 1);
        assert_eq!(id.track(), *track);
        assert!(!id.title().is_empty() && !id.description().is_empty());
        assert!(!id.score_label().is_empty());
        let t = id.medal_thresholds();
        if id.higher_is_better() {
            assert!(t[0] < t[1] && t[1] < t[2], "{id:?} {t:?}");
            assert_eq!(id.medal_for(t[0] - 0.5), 0);
        } else {
            assert!(t[0] > t[1] && t[1] > t[2], "{id:?} {t:?}");
            assert_eq!(id.medal_for(t[0] + 0.5), 0);
        }
        assert_eq!(id.medal_for(t[0]), 1);
        assert_eq!(id.medal_for(t[1]), 2);
        assert_eq!(id.medal_for(t[2]), 3);
        let w = World::new(RaceConfig::challenge(*id, 3, 1));
        assert_eq!(w.cfg.track, *track);
        assert_eq!(w.cfg.mode, RaceMode::Challenge(*id));
        assert_eq!(w.cfg.racers[0].kind, RacerKind::Human);
        assert_eq!(w.cfg.racers[0].character, 3);
        assert!(w.cfg.racers[1..].iter().all(|r| r.character != 3));
        assert!(w.challenge.as_ref().is_some_and(|c| c.id == *id && !c.done));
    }
    // Rivals.
    let n = |id: ChallengeId| RaceConfig::challenge(id, 0, 1).racers.len();
    assert_eq!(n(ChallengeId::CoinRush), 1);
    assert_eq!(n(ChallengeId::EggHunt), 1);
    assert_eq!(n(ChallengeId::Elimination), 8);
    assert_eq!(n(ChallengeId::DriftKing), 1);
    assert_eq!(n(ChallengeId::BashBrawl), 6);
    assert_eq!(n(ChallengeId::TurboTimeAttack), 1);
    assert_eq!(n(ChallengeId::LaserSniper), 1);
    assert_eq!(n(ChallengeId::RampRider), 1);
    assert_eq!(n(ChallengeId::NightCoinRush), 1);
    assert_eq!(n(ChallengeId::LastFeather), 6);
    assert_eq!(ChallengeId::RampRider.laps(), 2);
    assert!(ChallengeId::RampRider.finishing());
    assert!(!ChallengeId::LaserSniper.finishing() && !ChallengeId::LastFeather.finishing());
    assert_eq!(ChallengeId::LaserSniper.goal(), 15);
    let el = RaceConfig::challenge(ChallengeId::Elimination, 0, 1);
    assert!(el.racers[1..]
        .iter()
        .all(|r| r.kind == RacerKind::Bot(BotLevel::Normal)));
    let bb = RaceConfig::challenge(ChallengeId::BashBrawl, 0, 1);
    assert!(bb.racers[1..].iter().all(|r| matches!(
        r.kind,
        RacerKind::Bot(BotLevel::Easy) | RacerKind::Bot(BotLevel::Dummy)
    )));
    assert_eq!(ChallengeId::DriftKing.laps(), 2);
    assert_eq!(ChallengeId::TurboTimeAttack.laps(), 3);
    // Config and state survive serde (the app sends them over the wire / saves them).
    let cfg = RaceConfig::challenge(ChallengeId::BashBrawl, 2, 9);
    let back: RaceConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(back, cfg);
    let w = World::new(cfg);
    let cs = w.challenge.clone().unwrap();
    let back: ChallengeState = serde_json::from_str(&serde_json::to_string(&cs).unwrap()).unwrap();
    assert_eq!(back, cs);
}

#[test]
fn coin_rush_counts_every_pickup_and_ends_at_90_s() {
    let mut w = bot_world(ChallengeId::CoinRush, BotLevel::Normal, 2);
    assert_eq!(w.tun.coin_respawn, 3.0);
    let ev = play(&mut w, 120.0);
    let picked = ev
        .iter()
        .filter(|e| matches!(e, Event::CoinPicked { racer: 0, .. }))
        .count();
    let cs = w.challenge.as_ref().unwrap();
    assert!(cs.done && w.phase == RacePhase::Finished);
    assert_eq!(cs.score, picked as f32);
    assert_eq!(cs.progress as usize, picked);
    assert!(cs.score > 30.0, "a bot should grab plenty: {}", cs.score);
    assert!(
        w.racers[0].coins > 10 && cs.score > 10.0,
        "every Bitcoin stays in the race wallet"
    );
    assert!(close(cs.time, 90.0, 0.05), "ended at {} s", cs.time);
    assert_eq!(cs.time_left, 0.0);
    assert_eq!(cs.medal, ChallengeId::CoinRush.medal_for(cs.score));
    assert!(ev.contains(&Event::ChallengeDone { medal: cs.medal }));
    assert!(ev.contains(&Event::RaceOver));
    // Coins respawn in 3 s: the same spot can be taken twice in 30 s.
    let mut w = World::new(RaceConfig::challenge(ChallengeId::CoinRush, 0, 1));
    skip_countdown(&mut w);
    let spot = w.coins[0].home;
    let mut taken = Vec::new();
    for _ in 0..(12.0 / DT) as usize {
        // Park the racer on the coin spot.
        w.racers[0].pos = spot - Vec3::Y * 1.0;
        w.racers[0].vel = Vec3::ZERO;
        w.racers[0].respawn_t = 0.0;
        let before = w.coins[0].alive;
        w.step(&[]);
        if before && !w.coins[0].alive {
            taken.push(w.race_time());
        }
        w.drain_events();
    }
    assert!(taken.len() >= 3, "{taken:?}");
    assert!(close(taken[1] - taken[0], 3.0, 0.1), "{taken:?}");
}

#[test]
fn coin_rush_timer_counts_down() {
    let mut w = World::new(RaceConfig::challenge(ChallengeId::CoinRush, 0, 1));
    assert_eq!(w.challenge.as_ref().unwrap().time_left, 90.0);
    skip_countdown(&mut w);
    for _ in 0..600 {
        w.step(&[]);
    }
    let cs = w.challenge.as_ref().unwrap();
    assert!(close(cs.time_left, 80.0, 0.05), "{}", cs.time_left);
    assert!(close(cs.time, 10.0, 0.05));
    assert!(!cs.done);
}

#[test]
fn egg_hunt_ends_on_the_twentieth_egg() {
    let mut w = bot_world(ChallengeId::EggHunt, BotLevel::Normal, 1);
    assert_eq!(w.tun.target_respawn, 1.5);
    // Contact pickups drive the counter, independently of bot line choice.
    skip_countdown(&mut w);
    let mut ev = Vec::new();
    for _ in 0..20 {
        let k = w.targets.iter().position(|t| !t.bad).unwrap();
        w.targets[k].alive = true;
        let pr = w.track.project_global(w.targets[k].pos);
        place(&mut w, 0, pr.s, pr.lateral, 0.0);
        w.step(&[Input::default()]);
        ev.extend(w.drain_events());
    }
    let eggs = ev
        .iter()
        .filter(|e| matches!(e, Event::TargetDestroyed { racer: 0, .. }))
        .count();
    let cs = w.challenge.as_ref().unwrap();
    assert!(cs.done, "{} eggs by {} s", cs.progress, cs.time);
    assert_eq!(eggs, 20, "the clock stops on egg 20");
    assert_eq!(cs.progress, 20);
    assert_eq!(cs.goal, 20);
    assert!(cs.score > 0.0 && cs.score < 150.0);
    assert!(close(cs.score, cs.time, 0.02), "score is the time");
    assert_eq!(cs.medal, ChallengeId::EggHunt.medal_for(cs.score));
    assert!(ev.contains(&Event::ChallengeDone { medal: cs.medal }));
    assert_eq!(w.phase, RacePhase::Finished);
}

#[test]
fn egg_hunt_gives_up_at_150_s_with_no_medal() {
    let mut w = World::new(RaceConfig::challenge(ChallengeId::EggHunt, 0, 1));
    skip_countdown(&mut w);
    w.targets.clear(); // no pickups, despite permanent full throttle
    let ev = run(&mut w, (151.0 / DT) as usize, |_| vec![]);
    let cs = w.challenge.as_ref().unwrap();
    assert!(cs.done && cs.progress == 0);
    assert_eq!(cs.medal, 0);
    assert!(close(cs.time, 150.0, 0.05));
    assert!(ev.contains(&Event::ChallengeDone { medal: 0 }));
    assert_eq!(w.phase, RacePhase::Finished);
}

#[test]
fn egg_hunt_rows_are_laid_again_on_the_next_crossing() {
    let mut w = World::new(RaceConfig::challenge(ChallengeId::EggHunt, 0, 1));
    skip_countdown(&mut w);
    w.targets[0].alive = false;
    w.targets[0].respawn_t = w.tun.target_respawn;
    for _ in 0..(13.8 / DT) as usize {
        w.step(&[]);
    }
    assert!(w.targets[0].alive);
}

#[test]
fn elimination_removes_one_racer_every_25_s_and_ends_with_one_left() {
    for seed in [2u64, 3] {
        let mut w = bot_world(ChallengeId::Elimination, BotLevel::Normal, seed);
        let mut outs: Vec<(f32, u8, u8, [f32; 3])> = Vec::new();
        let mut last_ev = Vec::new();
        while w.phase != RacePhase::Finished && w.tick < 60 * 400 {
            // This test covers the full survivor ranking, not the player's early exit.
            let lead = w.racers.iter().map(|r| r.progress).fold(0.0_f32, f32::max);
            w.racers[0].lap = 20;
            w.racers[0].progress = lead + 100.0;
            w.step(&[]);
            let t = w.race_time();
            for e in w.drain_events() {
                if let Event::Eliminated { racer, place, pos } = e {
                    outs.push((t, racer, place, pos));
                    // Exactly one racer fewer in the field right now.
                    let cs = w.challenge.as_ref().unwrap();
                    assert_eq!(cs.eliminated.len(), outs.len());
                    assert_eq!(cs.eliminated.last(), Some(&(racer as usize)));
                }
                last_ev.push(e);
            }
        }
        assert_eq!(outs.len(), 7, "seed {seed}: {outs:?}");
        for (k, (t, _, place, pos)) in outs.iter().enumerate() {
            assert!(
                close(*t, 25.0 * (k + 1) as f32, 0.05),
                "seed {seed}: elimination {k} at {t}"
            );
            assert_eq!(*place as usize, 8 - k);
            assert!(pos.iter().all(|c| c.is_finite()) && pos[1] > -100.0);
        }
        let cs = w.challenge.as_ref().unwrap();
        assert!(cs.done);
        assert_eq!(cs.eliminated.len(), 7);
        assert_eq!(w.phase, RacePhase::Finished);
        assert!(last_ev.contains(&Event::RaceOver));
        let mut all: Vec<usize> = cs.eliminated.clone();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 7, "nobody eliminated twice");
        let winner = (0..8).find(|i| !cs.eliminated.contains(i)).unwrap();
        // Standings: the survivor first, then the eliminated in reverse order.
        let order = w.standings();
        assert_eq!(order[0], winner);
        let mut rev = cs.eliminated.clone();
        rev.reverse();
        assert_eq!(order[1..], rev[..]);
        assert_eq!(w.racers[winner].place, 1);
        for (k, &i) in cs.eliminated.iter().enumerate() {
            assert!(w.is_eliminated(i));
            assert_eq!(w.racers[i].place as usize, 8 - k);
            assert!(w.racers[i].pos.y < -100.0, "parked out of sight");
        }
        assert!(!w.is_eliminated(winner));
        // The player's score is their place; the medal follows it.
        let me_place = w.racers[0].place as f32;
        assert_eq!(cs.score, me_place);
        assert_eq!(cs.medal, ChallengeId::Elimination.medal_for(me_place));
        assert!(w.racers[winner].pos.y > -100.0);
    }
}

#[test]
fn elimination_takes_the_last_placed_and_removed_racers_stay_out() {
    let mut w = bot_world(ChallengeId::Elimination, BotLevel::Normal, 5);
    let mut gone: Option<(usize, f32)> = None;
    let mut after = Vec::new();
    while w.race_time() < 60.0 && w.phase != RacePhase::Finished {
        let order_before = w.standings();
        w.step(&[]);
        for e in w.drain_events() {
            match e {
                Event::Eliminated { racer, .. } if gone.is_none() => {
                    // The standings a tick earlier ended with this racer.
                    assert_eq!(*order_before.last().unwrap(), racer as usize);
                    gone = Some((racer as usize, w.race_time()));
                }
                Event::Hit { victim, .. } if gone.map_or(false, |g| g.0 == victim as usize) => {
                    after.push(e)
                }
                Event::CoinPicked { racer, .. } | Event::Footstep { racer }
                    if gone.map_or(false, |g| g.0 == racer as usize) =>
                {
                    after.push(e)
                }
                _ => {}
            }
        }
    }
    assert!(gone.is_some());
    assert!(
        after.is_empty(),
        "an eliminated racer did something: {after:?}"
    );
}

#[test]
fn elimination_ends_when_the_player_is_out() {
    // An idle human sits on the grid at the back: out at the first elimination.
    let mut w = World::new(RaceConfig::challenge(ChallengeId::Elimination, 0, 3));
    skip_countdown(&mut w);
    let ev = run(&mut w, (27.0 / DT) as usize, |_| vec![]);
    let cs = w.challenge.as_ref().unwrap();
    assert!(cs.done);
    assert_eq!(cs.eliminated, vec![0]);
    assert_eq!(cs.score, 8.0);
    assert_eq!(cs.medal, 0);
    assert!(w.phase == RacePhase::Finished);
    assert!(ev.contains(&Event::ChallengeDone { medal: 0 }));
    assert!(w.is_eliminated(0));
    // Medals by place: 3rd bronze, 2nd silver, 1st gold.
    assert_eq!(ChallengeId::Elimination.medal_for(3.0), 1);
    assert_eq!(ChallengeId::Elimination.medal_for(2.0), 2);
    assert_eq!(ChallengeId::Elimination.medal_for(1.0), 3);
    assert_eq!(ChallengeId::Elimination.medal_for(4.0), 0);
}

#[test]
fn elimination_time_left_counts_to_the_next_one() {
    let mut w = bot_world(ChallengeId::Elimination, BotLevel::Normal, 2);
    skip_countdown(&mut w);
    for _ in 0..(10.0 / DT) as usize {
        w.step(&[]);
    }
    let cs = w.challenge.as_ref().unwrap();
    assert!(close(cs.time_left, 15.0, 0.1), "{}", cs.time_left);
    assert!(cs.score >= 1.0 && cs.score <= 8.0);
}

#[test]
fn drift_king_scores_charge_times_tier_on_release() {
    let mut w = bot_world(ChallengeId::DriftKing, BotLevel::Normal, 1);
    let ev = play(&mut w, 200.0);
    let t = Tunables::default();
    let mut sum = 0.0;
    let mut pairs = 0;
    for (k, e) in ev.iter().enumerate() {
        if let Event::DriftPoints { racer: 0, points } = e {
            sum += points;
            // Every scored release comes with its Boost event just before.
            let Some(Event::Boost { tier, .. }) = ev[..k]
                .iter()
                .rev()
                .find(|e| matches!(e, Event::Boost { racer: 0, .. }))
            else {
                panic!("DriftPoints without a Boost")
            };
            let tier = *tier;
            let tier_charge = t.drift_tiers[tier as usize - 1];
            assert!(
                *points >= tier_charge * tier as f32 - 1e-3,
                "{points} points for tier {tier}"
            );
            assert!(*points <= 6.0 * tier as f32, "{points}");
            pairs += 1;
        }
    }
    let cs = w.challenge.as_ref().unwrap();
    assert!(pairs >= 3, "the bot should score some drifts: {pairs}");
    assert!(close(cs.score, sum, 1e-3));
    assert_eq!(cs.progress, pairs, "progress counts scored drifts");
    assert!(cs.done);
    assert_eq!(w.lap_times[0].len(), 2, "two laps");
    assert_eq!(cs.medal, ChallengeId::DriftKing.medal_for(cs.score));
    assert_eq!(w.phase, RacePhase::Finished);
    assert!(ev.contains(&Event::ChallengeDone { medal: cs.medal }));
}

#[test]
fn drift_king_medals() {
    let id = ChallengeId::DriftKing;
    let [b, s, g] = id.medal_thresholds();
    assert_eq!(
        (
            id.medal_for(b - 1.0),
            id.medal_for(b),
            id.medal_for(s),
            id.medal_for(g + 50.0)
        ),
        (0, 1, 2, 3)
    );
}

#[test]
fn retired_bash_challenge_does_not_enable_legacy_attacks() {
    let mut w = World::new(RaceConfig::challenge(ChallengeId::BashBrawl, 0, 1));
    skip_countdown(&mut w);
    place(&mut w, 0, 200.0, -2.0, 20.0);
    place(&mut w, 1, 200.0, 1.0, 20.0);
    let mut inp = idle(6);
    inp[0].bash = true;
    w.step(&inp);
    assert!(!w.drain_events().iter().any(|e| matches!(e, Event::Bash { .. } | Event::BashWhiff { .. })));
    assert_eq!(w.challenge.as_ref().unwrap().score, 0.0);
}

#[test]
fn bash_brawl_ends_at_60_s_with_the_right_medal() {
    let mut w = bot_world(ChallengeId::BashBrawl, BotLevel::Hard, 1);
    let ev = play(&mut w, 100.0);
    let bashes: f32 = ev
        .iter()
        .map(|e| match e {
            Event::Bash {
                attacker: 0, stole, ..
            } => {
                if stole.is_some() {
                    2.0
                } else {
                    1.0
                }
            }
            _ => 0.0,
        })
        .sum();
    let cs = w.challenge.as_ref().unwrap();
    assert!(cs.done && close(cs.time, 60.0, 0.05));
    assert_eq!(cs.score, bashes);
    assert_eq!(bashes, 0.0, "bots cannot bash in legacy challenge configs");
    assert_eq!(cs.medal, ChallengeId::BashBrawl.medal_for(bashes));
    assert_eq!(w.phase, RacePhase::Finished);
}

#[test]
fn turbo_time_attack_starts_with_three_feathers_and_scores_the_time() {
    let w = World::new(RaceConfig::challenge(ChallengeId::TurboTimeAttack, 0, 1));
    assert_eq!(w.racers.len(), 1);
    assert_eq!(w.racers[0].item, Some(Item::Feather));
    assert_eq!(w.racers[0].item_uses, 3);
    let mut w = bot_world(ChallengeId::TurboTimeAttack, BotLevel::Hard, 1);
    assert_eq!(w.racers[0].item_uses, 3);
    let ev = play(&mut w, 300.0);
    let cs = w.challenge.as_ref().unwrap();
    let fin = ev
        .iter()
        .find_map(|e| match e {
            Event::Finished { racer: 0, time, .. } => Some(*time),
            _ => None,
        })
        .expect("finished");
    assert!(cs.done);
    assert_eq!(cs.score, fin);
    assert_eq!(w.lap_times[0].len(), 3);
    assert_eq!(cs.progress, 3, "laps completed");
    assert_eq!(cs.medal, ChallengeId::TurboTimeAttack.medal_for(fin));
    assert!(
        ev.iter()
            .filter(|e| matches!(
                e,
                Event::ItemUsed {
                    racer: 0,
                    item: Item::Feather
                }
            ))
            .count()
            >= 3
    );
    assert_eq!(w.phase, RacePhase::Finished);
    assert!(ev.contains(&Event::ChallengeDone { medal: cs.medal }));
}

#[test]
fn time_score_runs_with_the_clock_until_done() {
    let mut w = World::new(RaceConfig::challenge(ChallengeId::TurboTimeAttack, 0, 1));
    skip_countdown(&mut w);
    for _ in 0..300 {
        w.step(&[Input::go()]);
    }
    let cs = w.challenge.as_ref().unwrap();
    assert!(!cs.done && close(cs.score, 5.0, 0.05) && cs.medal == 0);
}

#[test]
fn challenges_do_not_disturb_ordinary_races() {
    let mut w = World::new(RaceConfig::all_bots(
        TrackId::Savanna,
        4,
        BotLevel::Normal,
        1,
    ));
    assert!(w.challenge.is_none() && w.lesson.is_none());
    for _ in 0..600 {
        w.step(&[]);
    }
    assert!((0..4).all(|i| !w.is_eliminated(i)));
}

// ─── The four added in spec 17 ─────────────────────────────────────────────

#[test]
fn laser_sniper_scores_time_plus_two_seconds_a_miss() {
    // A bot clears 15 eggs; its score is its time plus penalties.
    let mut w = bot_world(ChallengeId::LaserSniper, BotLevel::Normal, 1);
    skip_countdown(&mut w);
    w.cfg.racers[0].kind = RacerKind::Human;
    let mut ev = Vec::new();
    for _ in 0..15 {
        place(&mut w, 0, 10.0, 0.0, 0.0);
        place(&mut w, 1, 25.0, 0.0, 0.0);
        w.racers[1].effects.burn = 0.0;
        w.racers[1].dazzle_imm = [0; 8];
        w.racers[0].laser.heat = 0.0;
        w.racers[0].laser.overheated = false;
        ev.extend(run(&mut w, 10, |_| vec![Input { laser: true, ..Default::default() }]));
    }
    let cs = w.challenge.clone().unwrap();
    let eggs = ev
        .iter()
        .filter(|e| matches!(e, Event::LaserZap { by: 0, .. }))
        .count();
    assert!(cs.done && w.phase == RacePhase::Finished);
    assert_eq!(cs.progress, 15);
    assert_eq!(eggs, 15);
    assert!(close(cs.score, cs.time + cs.penalty, 1e-3), "{cs:?}");
    assert!(close(cs.penalty, cs.misses as f32 * SNIPER_MISS_PENALTY, 1e-4));
    assert_eq!(cs.medal, ChallengeId::LaserSniper.medal_for(cs.score));
    assert!(ev.contains(&Event::ChallengeDone { medal: cs.medal }));
    assert!(!ev.iter().any(|e| matches!(e, Event::ChallengeScore { .. })), "time challenge");

    // Lasering nothing: one miss per 0.6 s of firing.
    let mut w = World::new(RaceConfig::challenge(ChallengeId::LaserSniper, 0, 1));
    skip_countdown(&mut w);
    let fire = Input {
        laser: true,
        ..Input::default()
    };
    // 0.65 s firing (one miss), let it cool, then 0.6 s more: the firing time
    // without a hit adds up across taps (a second miss).
    for _ in 0..39 {
        w.step(&[fire]);
    }
    assert_eq!(w.challenge.as_ref().unwrap().misses, 1);
    for _ in 0..60 {
        w.step(&[Input::default()]);
    }
    for _ in 0..36 {
        w.step(&[fire]);
    }
    let cs = w.challenge.clone().unwrap();
    assert_eq!(cs.misses, 2, "{cs:?}");
    assert!(close(cs.penalty, 4.0, 1e-4));
    assert!(close(cs.score, cs.time + 4.0, 1e-3));
    assert!(!cs.done);
    // Medals: lower is better.
    let m = ChallengeId::LaserSniper.medal_thresholds();
    assert!(m[0] > m[1] && m[1] > m[2]);
}

#[test]
fn laser_sniper_gives_up_at_150_s_with_no_medal() {
    let mut w = World::new(RaceConfig::challenge(ChallengeId::LaserSniper, 0, 1));
    play(&mut w, 160.0);
    let cs = w.challenge.as_ref().unwrap();
    assert!(cs.done);
    assert!(cs.score >= 150.0);
    assert_eq!(cs.medal, 0);
}

#[test]
fn ramp_rider_counts_tricks_and_chains_into_the_next_ramp() {
    // A Normal bot lands its tricks; chains only happen when it carries a boost.
    let mut w = bot_world(ChallengeId::RampRider, BotLevel::Normal, 3);
    let ev = play(&mut w, 200.0);
    let cs = w.challenge.clone().unwrap();
    let landed = ev
        .iter()
        .filter(|e| matches!(e, Event::TrickBoost { racer: 0 }))
        .count();
    assert!(cs.done && w.racers[0].finished_at.is_some());
    assert_eq!(cs.progress as usize, landed);
    assert!(landed >= 4, "the bot tricks off the ramps: {landed}");
    assert_eq!(cs.score, landed as f32 + 2.0 * cs.misses as f32);
    assert_eq!(cs.medal, ChallengeId::RampRider.medal_for(cs.score));

    // Keep the player boosting the whole way: every trick after a landed one chains.
    let mut w = bot_world(ChallengeId::RampRider, BotLevel::Normal, 3);
    let mut ev = Vec::new();
    while w.phase != RacePhase::Finished && w.race_time() < 200.0 {
        if w.phase == RacePhase::Racing {
            w.racers[0].boost_t = w.racers[0].boost_t.max(0.3);
        }
        w.step(&[]);
        ev.extend(w.drain_events());
    }
    let cs = w.challenge.clone().unwrap();
    let tricks = ev.iter().filter(|e| matches!(e, Event::Trick { racer: 0 })).count();
    let landed = ev
        .iter()
        .filter(|e| matches!(e, Event::TrickBoost { racer: 0 }))
        .count();
    assert!(cs.misses >= 3, "chains: {cs:?}");
    assert!(cs.misses as usize <= tricks.saturating_sub(1));
    assert_eq!(cs.score, landed as f32 + 2.0 * cs.misses as f32);
    assert!(cs.score <= 16.0);
}

#[test]
fn night_coin_rush_coins_only_come_back_on_a_boost_pad() {
    let mut w = bot_world(ChallengeId::NightCoinRush, BotLevel::Normal, 2);
    let mut picked = 0;
    let mut revived_on_pad = 0;
    let mut alive: Vec<bool> = w.coins.iter().map(|c| c.alive).collect();
    while w.phase != RacePhase::Finished && w.race_time() < 150.0 {
        w.step(&[]);
        let ev = w.drain_events();
        picked += ev
            .iter()
            .filter(|e| matches!(e, Event::CoinPicked { racer: 0, .. }))
            .count();
        let pad = ev.iter().any(|e| matches!(e, Event::BoostPad { racer: 0 }));
        for (k, c) in w.coins.iter().enumerate() {
            if c.alive && !alive[k] {
                assert!(pad, "coin {k} came back without a boost pad at {:.1} s", w.race_time());
                revived_on_pad += 1;
            }
            alive[k] = c.alive;
        }
    }
    let cs = w.challenge.clone().unwrap();
    assert!(cs.done);
    assert!(close(cs.time, 120.0, 0.05), "{}", cs.time);
    assert_eq!(cs.score, picked as f32);
    assert!(revived_on_pad > 0, "pads brought lines back");
    assert!(cs.score > 60.0, "{}", cs.score);
    assert_eq!(cs.medal, ChallengeId::NightCoinRush.medal_for(cs.score));
}

#[test]
fn last_feather_starts_on_one_feather_and_scores_the_place() {
    let w = World::new(RaceConfig::challenge(ChallengeId::LastFeather, 2, 1));
    let b = w.battle.as_ref().expect("a battle");
    assert_eq!(b.feathers, vec![1, 3, 3, 3, 3, 3]);
    assert_eq!(w.cfg.track, TrackId::DustBowl);
    assert!(w.cfg.racers[1..].iter().all(|r| r.kind == RacerKind::Bot(BotLevel::Normal)));

    // The player goes out at once: 6th, no medal; the race ends after ChallengeDone.
    let mut w = World::new(RaceConfig::challenge(ChallengeId::LastFeather, 2, 1));
    skip_countdown(&mut w);
    let r = &w.racers[0];
    w.projectiles.push(Projectile {
        kind: ProjectileKind::RollingEgg,
        pos: r.body(),
        vel: Vec3::ZERO,
        owner: 3,
        target: None,
        age: 1.0,
        bounces: 0,
        hint: r.track_idx,
        s: r.s,
        lateral: r.lateral,
        id: 1,
    });
    let ev = run(&mut w, 2, |w| idle(w.racers.len()));
    let cs = w.challenge.clone().unwrap();
    assert!(cs.done);
    assert_eq!(cs.score, 6.0);
    assert_eq!(cs.medal, 0);
    let order: Vec<&Event> = ev
        .iter()
        .filter(|e| matches!(e, Event::Out { .. } | Event::BattleOver | Event::ChallengeDone { .. } | Event::RaceOver))
        .collect();
    assert!(matches!(order[0], Event::Out { racer: 0, place: 6, .. }), "{order:?}");
    assert_eq!(*order[1], Event::BattleOver);
    assert_eq!(*order[2], Event::ChallengeDone { medal: 0 });
    assert_eq!(*order[3], Event::RaceOver);
    assert!(w.battle.as_ref().unwrap().done);

    // Bots in every seat: it ends, the score is the player's place, the medal follows it.
    for seed in 1..=3 {
        let mut w = bot_world(ChallengeId::LastFeather, BotLevel::Normal, seed);
        let ev = play(&mut w, 200.0);
        let cs = w.challenge.clone().unwrap();
        let b = w.battle.as_ref().unwrap();
        assert!(cs.done && b.done, "seed {seed}");
        assert_eq!(cs.score, b.place_of(0) as f32);
        assert_eq!(cs.medal, ChallengeId::LastFeather.medal_for(cs.score));
        assert!(cs.time <= 180.0 + DT);
        assert_eq!(cs.progress as usize, b.out_order.iter().filter(|&&i| i != 0).count());
        assert!(ev.contains(&Event::ChallengeDone { medal: cs.medal }));
        if cs.score == 1.0 {
            assert_eq!(cs.medal, 3, "gold = win");
        }
    }
}

/// A human-seat challenge world, countdown over, with racer 0 `back` metres
/// before target `k` (in line with it), facing along the track.
fn before_egg(id: ChallengeId, k: usize, back: f32) -> World {
    let mut w = World::new(RaceConfig::challenge(id, 0, 1));
    skip_countdown(&mut w);
    let pr = w.track.project_global(w.targets[k].pos);
    place(&mut w, 0, pr.s - back, pr.lateral, 0.0);
    w
}

#[test]
fn laser_sniper_ignores_eggs_and_counts_forward_rival_hits() {
    let mut w = before_egg(ChallengeId::LaserSniper, 0, 20.0);
    w.targets[0].alive = true;
    w.tun.accel = 0.0;
    w.give_item(0, Item::MegaLaser, 1);
    let mut ev = run(&mut w, 1, |_| vec![Input { item: true, ..Default::default() }]);
    ev.extend(run(&mut w, 60, |_| idle(1)));
    assert!(w.targets[0].alive);
    assert!(!ev.iter().any(|e| matches!(e, Event::TargetDestroyed { .. })));
    assert_eq!(w.challenge.as_ref().unwrap().progress, 0);
}

#[test]
fn night_coin_rush_does_not_pay_for_lasered_eggs() {
    let mut w = before_egg(ChallengeId::NightCoinRush, 0, 14.0);
    w.give_item(0, Item::Shield, 1); // holding an item: an egg would pay a coin elsewhere
    let fire = Input {
        laser: true,
        ..Input::default()
    };
    let ev = run(&mut w, 180, |_| vec![fire]);
    assert!(
        ev.iter().any(|e| matches!(e, Event::TargetDestroyed { racer: 0, .. })),
        "the player lasered an egg"
    );
    assert!(!ev.iter().any(|e| matches!(e, Event::CoinPicked { racer: 0, .. })));
    let cs = w.challenge.as_ref().unwrap();
    assert_eq!((cs.score, cs.progress), (0.0, 0));
}
