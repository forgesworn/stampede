//! Building the `RaceOutcome` handed to `after_race`, and the profile records.

use super::source::RaceView;
use crate::flow::{BattleRow, BattleSummary, ChallengeResult, LessonResult, RaceLaunch, RaceOutcome, RacerResult};
use race_core::{BotLevel, RacerKind};

/// Results sorted by place; the training dummies are left out.
pub fn build(launch: &RaceLaunch, v: &RaceView, quit: bool) -> RaceOutcome {
    let mut results: Vec<RacerResult> = v
        .racers
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(v.cfg.racers.get(*i).map(|s| s.kind), Some(RacerKind::Bot(BotLevel::Dummy))))
        .map(|(i, r)| {
            let setup = v.cfg.racers.get(i);
            let best = v.lap_times.get(i).and_then(|l| l.iter().copied().fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.min(x)))));
            RacerResult {
                name: setup.map(|s| s.name.clone()).unwrap_or_else(|| format!("Racer {}", i + 1)),
                character: r.character,
                place: r.place,
                time_s: r.finished_at,
                best_lap_s: best,
                coins: r.coins,
                is_local: i == launch.local_seat,
                is_bot: matches!(setup.map(|s| s.kind), Some(RacerKind::Bot(_))),
                battle: v.battle.map(|b| BattleRow {
                    feathers: b.feathers.get(i).copied().unwrap_or(0),
                    hits: b.hits.get(i).copied().unwrap_or(0),
                    taken: b.taken.get(i).copied().unwrap_or(0),
                    out_order: b.out_order.iter().position(|&o| o == i).map(|k| k as u8 + 1),
                }),
            }
        })
        .collect();
    results.sort_by_key(|r| r.place);
    // Places are 1-based and unique; renumber after dropping dummies.
    for (k, r) in results.iter_mut().enumerate() {
        r.place = k as u8 + 1;
    }
    let lesson = v.lesson.map(|l| LessonResult { lesson: l.id, done: l.done, stars: if l.done { l.stars } else { 0 }, time_s: l.time });
    let challenge = v.challenge.map(|c| ChallengeResult { id: c.id, score: c.score, medal: c.medal, done: c.done, progress: c.progress });
    let battle = v.battle.map(|b| BattleSummary { duration: v.race_time, time_up: b.time_left <= 0.0 });
    RaceOutcome { launch: launch.clone(), results, lesson, challenge, quit, ghost: None, battle, stats: Default::default(), gp_recorded: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::RaceKind;
    use crate::game::source::{LocalRace, RaceSource};
    use race_core::{Input, LessonId, RaceConfig, TrackId};

    fn launch(cfg: RaceConfig, kind: RaceKind) -> RaceLaunch {
        RaceLaunch { config: cfg, kind, local_seat: 0 }
    }

    #[test]
    fn results_are_sorted_with_local_flag_and_bots() {
        let cfg = RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 1);
        let l = launch(cfg.clone(), RaceKind::Quick);
        let mut r = LocalRace::new(cfg, Some(0));
        for _ in 0..600 {
            r.advance(1.0 / 60.0, &mut Input::go);
        }
        let o = build(&l, &r.view(), false);
        assert_eq!(o.results.len(), 4);
        assert!(o.results.windows(2).all(|w| w[0].place < w[1].place));
        assert_eq!(o.results.iter().filter(|x| x.is_local).count(), 1);
        assert_eq!(o.results.iter().filter(|x| x.is_bot).count(), 3);
        assert!(o.lesson.is_none() && !o.quit);
    }

    #[test]
    fn lesson_and_challenge_outcomes_carry_their_results() {
        let cfg = RaceConfig::training(LessonId::Jump, RacerKind::Human, 1);
        let l = launch(cfg.clone(), RaceKind::Training(LessonId::Jump));
        let r = LocalRace::new(cfg, Some(0));
        let o = build(&l, &r.view(), false);
        assert!(o.lesson.is_some() && o.challenge.is_none());

        let id = race_core::ChallengeId::CoinRush;
        let cfg = RaceConfig::challenge(id, 0, 3);
        let l = launch(cfg.clone(), RaceKind::Challenge(id));
        let mut r = LocalRace::new(cfg, Some(0));
        for _ in 0..400 {
            r.advance(1.0 / 60.0, &mut Input::go);
        }
        let o = build(&l, &r.view(), false);
        let c = o.challenge.expect("a challenge race reports its result");
        assert_eq!(c.id, id);
        assert!(o.lesson.is_none() && !c.done, "90 s have not passed");
    }

    #[test]
    fn a_battle_outcome_has_feathers_hits_and_out_order_and_no_times() {
        use race_core::{BattleRules, RacerSetup};
        let mut cfg = RaceConfig::battle_bots(TrackId::DustBowl, 6, BotLevel::Hard, BattleRules { time_limit: 60, ..BattleRules::default() }, 5);
        cfg.racers[0] = RacerSetup { name: "Me".into(), character: 0, kind: RacerKind::Human };
        let l = RaceLaunch { config: cfg.clone(), kind: RaceKind::Battle, local_seat: 0 };
        let mut r = LocalRace::new(cfg, Some(0));
        r.set_autopilot(Some(BotLevel::Normal), Default::default());
        for _ in 0..(60 * 70) {
            r.advance(1.0 / 60.0, &mut Input::default);
        }
        let v = r.view();
        let b = v.battle.expect("a battle has its state");
        assert!(b.done, "60 s on the clock, over after 70 s");
        let o = build(&l, &v, false);
        assert_eq!(o.results.len(), 6);
        assert!(o.results.iter().all(|x| x.time_s.is_none() && x.best_lap_s.is_none()), "battles have no times");
        assert!(o.results.iter().all(|x| x.battle.is_some()));
        assert!(o.results.windows(2).all(|w| w[0].place < w[1].place));
        // the table is the battle ranking: survivors first (most feathers), out racers after
        let rows: Vec<_> = o.results.iter().map(|x| x.battle.unwrap()).collect();
        let first_out = rows.iter().position(|x| x.out_order.is_some()).unwrap_or(rows.len());
        assert!(rows[..first_out].iter().all(|x| x.feathers > 0 && x.out_order.is_none()));
        assert!(rows[first_out..].iter().all(|x| x.feathers == 0 && x.out_order.is_some()));
        assert!(rows[..first_out].windows(2).all(|w| w[0].feathers >= w[1].feathers));
        // out racers: latest out first
        assert!(rows[first_out..].windows(2).all(|w| w[0].out_order > w[1].out_order));
        // feather and hit books balance: every hit popped (or stole) one feather from somebody
        let hits: u32 = rows.iter().map(|x| x.hits as u32).sum();
        let taken: u32 = rows.iter().map(|x| x.taken as u32).sum();
        assert!(hits <= taken + 2 && taken > 0, "{hits} hits, {taken} taken");
        assert_eq!(o.battle.unwrap().time_up, b.time_left <= 0.0);
        assert!(o.results.iter().filter(|x| x.is_local).count() == 1);
    }
}
