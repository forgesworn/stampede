//! Bots (spec 8): pure-pursuit steering on the racing line with a curvature
//! speed plan, drifting, ramp tricks, hurdle jumps, lasers and items.
//!
//! [`bot_input`] is a pure function of the world: its randomness comes from a
//! `Pcg32` derived from (seed, tick, racer), and the little memory it needs
//! (unstick timer, item hold time) lives in `World::bot_mem`, updated by
//! `World::step`.

use rand::Rng;
use rand_pcg::Pcg32;
use serde::{Deserialize, Serialize};

use crate::challenge::ChallengeId;
use crate::input::Input;
use crate::items::{Item, ProjectileKind};
use crate::math::{flat, lerp, mix64, wrap_angle, yaw_of, DT};
use crate::racer::Racer;
use crate::track::{Feature, Surface};
use crate::world::{RaceMode, RacePhase, World};

/// Bot skill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BotLevel {
    /// Slower, noisy, rarely drifts.
    Easy,
    /// The default opponent.
    Normal,
    /// Full speed, purple drifts.
    Hard,
    /// Training dummy: slow, never attacks.
    Dummy,
}

/// Per-bot memory, updated by `World::step` (not by [`bot_input`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BotMem {
    /// Time spent stopped.
    pub stuck_t: f32,
    /// Reversing out of a jam: time left.
    pub reverse_t: f32,
    /// Time the current item has been held.
    pub hold_t: f32,
    /// Time since the last drift ended (drift cooldown).
    pub calm_t: f32,
}

/// Updates a bot's memory from its racer (called once per tick by `World::step`).
pub fn update_mem(m: &mut BotMem, r: &Racer, racing: bool) {
    m.reverse_t = (m.reverse_t - DT).max(0.0);
    if racing
        && r.respawn_t <= 0.0
        && !r.disabled()
        && r.on_ground
        && r.speed.abs() < 2.0
        && m.reverse_t <= 0.0
    {
        m.stuck_t += DT;
    } else if m.reverse_t <= 0.0 {
        m.stuck_t = 0.0;
    }
    if m.stuck_t > 0.8 {
        m.reverse_t = 0.7;
        m.stuck_t = 0.0;
    }
    if r.drift.active || r.drift.hop {
        m.calm_t = 0.0;
    } else {
        m.calm_t += DT;
    }
    if r.item.is_some() {
        m.hold_t += DT;
    } else {
        m.hold_t = 0.0;
    }
}

/// Seconds of travel the bots start a drift earlier by (part of the ease-in).
const DRIFT_LEAD_TIME: f32 = 0.3;

fn turn_rate(w: &World, r: &Racer, speed: f32) -> f32 {
    let t = &w.tun;
    let cls = r.class.mods();
    lerp(
        t.turn_rate_low,
        t.turn_rate_high,
        (speed.abs() / (t.top_speed * cls.top)).clamp(0.0, 1.0),
    ) * cls.turn
}

/// Max speed the bot can carry around curvature `k` (yaw-rate limited).
fn corner_speed(w: &World, r: &Racer, k: f32, factor: f32) -> f32 {
    let t = &w.tun;
    let cls = r.class.mods();
    let base_top = t.top_speed * cls.top;
    let lo = t.turn_rate_low * cls.turn * factor;
    let slope = (t.turn_rate_high - t.turn_rate_low) * cls.turn * factor / base_top; // negative
                                                                                     // v * k = lo + slope * v  ->  v = lo / (k - slope)
    let k = k.abs().max(1e-4);
    lo / (k - slope)
}

/// Integrated signed turn (rad, + left) of the racing line over `[s0, s0 + len]`.
fn turn_over(w: &World, s0: f32, len: f32) -> (f32, f32) {
    let tr = &w.track;
    let mut sum = 0.0;
    let mut kmax = 0.0f32;
    let mut x = 0.0;
    while x < len {
        let k = tr.racing_line[tr.idx_at(s0 + x)].curvature;
        sum += k * 2.0;
        kmax = kmax.max(k.abs());
        x += 2.0;
    }
    (sum, kmax)
}

/// Inputs for bot `idx` this tick. Pure (see the module docs).
pub fn bot_input(world: &World, idx: usize, level: BotLevel) -> Input {
    let w = world;
    let r = &w.racers[idx];
    if r.effects.dismount > 0.0 {
        return Input {
            jump: r.chase_offset().length() < 2.0 && !r.prev_input.jump,
            ..Input::default()
        };
    }
    let tr = &*w.track;
    let t = &w.tun;
    let mem = w.bot_mem.get(idx).copied().unwrap_or_default();
    let mut rng = w.bot_rng(idx);
    let mut inp = Input::default();

    if w.phase == RacePhase::Countdown {
        let press_at = match level {
            BotLevel::Easy => {
                let h = mix64(w.cfg.seed ^ (idx as u64 + 77));
                0.15 + (h >> 40) as f32 / (1u64 << 24) as f32 * 0.9
            }
            BotLevel::Normal => 0.32,
            BotLevel::Hard => 0.28,
            BotLevel::Dummy => 0.0,
        };
        if w.countdown_left() <= press_at {
            inp.throttle = 1.0;
        }
        return inp;
    }
    if r.respawn_t > 0.0 {
        return inp;
    }
    if w.battle.is_some() && level != BotLevel::Dummy {
        return battle_input(w, idx, level, mem, &mut rng);
    }

    let speed = r.speed.max(0.0);
    // Keep collecting race Bitcoin even while carrying a power-up.
    let coin_rush = matches!(
        w.cfg.mode,
        RaceMode::Challenge(ChallengeId::CoinRush | ChallengeId::NightCoinRush)
    );
    let easy = level == BotLevel::Easy;
    let dummy = level == BotLevel::Dummy;

    // ---- where to aim ----
    let la = (6.0 + speed * 0.45) * if easy { 0.85 } else { 1.0 };
    let s_t = r.s + la;
    let line = &tr.racing_line[tr.idx_at(s_t)];
    let sm_t = tr.sample_at(s_t);
    let hw = 0.5 * sm_t.width;
    let mut lat_t = line.offset;
    // Stay central approaching ramps and gaps.
    let mut ramp_ahead: Option<f32> = None; // distance to the lip
    let mut hurdle_ahead: Option<f32> = None;
    for f in &tr.features {
        match *f {
            Feature::Ramp { s, len, .. } => {
                let d = tr.s_delta(r.s, s + len);
                if d > -2.0 && d < 60.0 {
                    lat_t *= 0.3;
                    if d > -1.0 {
                        ramp_ahead = Some(ramp_ahead.map_or(d, |x: f32| x.min(d)));
                    }
                }
            }
            Feature::Gap { s, .. } => {
                let d = tr.s_delta(r.s, s);
                if d > -15.0 && d < 90.0 {
                    lat_t *= 0.2;
                }
            }
            Feature::Hurdle { s } if w.hurdles_active() => {
                let d = tr.s_delta(r.s, s);
                if d > 0.0 && d < 40.0 {
                    hurdle_ahead = Some(hurdle_ahead.map_or(d, |x: f32| x.min(d)));
                }
            }
            Feature::CoinLine {
                s,
                lateral,
                count,
                spacing,
            } if !dummy && (r.coins < t.coin_max as u32 || coin_rush) => {
                let d = tr.s_delta(r.s, s);
                let end = count as f32 * spacing;
                if d > -end && d < la + 25.0 && (lateral - line.offset).abs() < 6.0 {
                    lat_t = lateral;
                }
            }
            _ => {}
        }
    }
    // Contact eggs need a deliberate line through a slot. Avoid the bad egg
    // when it is readable, including the whisk's global inversion.
    if !dummy && !matches!(w.cfg.mode, RaceMode::Training(id) if id != crate::LessonId::CoinsItems) {
        let mut best = f32::INFINITY;
        for egg in &w.targets {
            if !egg.alive || (egg.bad ^ (w.play.scramble > 0.0)) { continue; }
            let pr = tr.project(egg.pos, r.track_idx as usize);
            let ahead = tr.s_delta(r.s, pr.s);
            if ahead > 0.0 && ahead < la + 25.0 {
                let score = ahead + (pr.lateral - r.lateral).abs() * 2.0;
                if score < best { best = score; lat_t = pr.lateral; }
            }
        }
    }
    // Dodge cacti.
    for h in &w.hazards {
        let d = h.pos - r.pos;
        let along = flat(d).dot(r.forward());
        if along > 2.0 && along < 30.0 {
            let pr_lat = flat(d).dot(r.right()) + r.lateral;
            if (pr_lat - lat_t).abs() < 2.5 {
                lat_t = if pr_lat > 0.0 {
                    pr_lat - 3.5
                } else {
                    pr_lat + 3.5
                };
            }
        }
    }
    if easy {
        lat_t += rng.gen_range(-1.0f32..1.0);
    }
    lat_t = lat_t.clamp(-(hw - 1.5), hw - 1.5);
    let target = sm_t.pos + sm_t.right * lat_t;

    // ---- pure pursuit ----
    let to = flat(target - r.pos);
    let err = wrap_angle(yaw_of(to) - r.yaw); // + = target to the left
    let ld = to.length().max(3.0);
    let kappa = 2.0 * crate::math::sin(err) / ld;
    let omega_left = speed.max(5.0) * kappa;
    let tb = turn_rate(w, r, speed).max(0.3);
    let gain = if easy { 0.85 } else { 1.15 };
    let mut drift_q = 1.0;
    if r.drift.active {
        let d = r.drift.dir as f32;
        let q = -omega_left * gain * d / tb;
        drift_q = q;
        // The drift turn is proportional to the steer into it; wanting less than
        // none, counter-steer (it only firms the grip up).
        let full = crate::racer::Drift::full_turn(r.drift.blend(), t);
        inp.steer = d * (q / full).clamp(-1.0, 1.0);
    } else {
        inp.steer = (-omega_left * gain / tb).clamp(-1.0, 1.0);
    }
    if easy {
        inp.steer = (inp.steer + rng.gen_range(-0.12f32..0.12)).clamp(-1.0, 1.0);
    }

    // ---- speed plan ----
    let can_drift = matches!(level, BotLevel::Normal | BotLevel::Hard);
    let factor = if r.drift.active {
        1.05
    } else if can_drift {
        0.98
    } else {
        0.9
    };
    let decel = t.brake * 0.5;
    let look = (speed * speed / (2.0 * decel) + 12.0).min(110.0);
    let mut allowed = f32::INFINITY;
    let mut x = 0.0;
    while x < look {
        let k = tr.racing_line[tr.idx_at(r.s + x)].curvature;
        let vt = corner_speed(w, r, k, factor);
        allowed = allowed.min((vt * vt + 2.0 * decel * x).sqrt());
        x += 2.0;
    }
    inp.throttle = 1.0;
    if speed > allowed + 2.0 && speed > 10.0 {
        inp.brake = true;
        inp.throttle = 0.0;
    } else if speed > allowed {
        inp.throttle = 0.0;
    }

    // ---- drift ----
    if can_drift || (easy && r.speed > 20.0) {
        let goal_tier = match level {
            BotLevel::Hard => 2,
            BotLevel::Normal => 1,
            _ => 1,
        };
        if r.drift.active {
            let d = r.drift.dir as f32;
            let (rem, _) = turn_over(w, r.s + 2.0, 22.0);
            let rem = -rem * d; // positive while the corner keeps turning our way
                                // Release at the goal tier, when the corner straightens, or when the
                                // drift turns us more than the line wants even steering out of it.
            let over_rotating = drift_q < -0.15 && r.drift.charge > 0.35;
            let learning = matches!(w.cfg.mode, RaceMode::Training(crate::LessonId::DriftBoost) | RaceMode::Challenge(ChallengeId::DriftKing));
            let release = r.drift.tier >= goal_tier || (!learning && (rem < 0.12 || over_rotating));
            inp.drift = !release;
        } else if r.drift.hop {
            let d = r.drift.dir as f32;
            // The arm lasts as long as the button is held: on the ground with no
            // side to pick, let go rather than hang on to it.
            inp.drift = d != 0.0 || !r.on_ground || inp.steer.abs() > 0.25;
            if d != 0.0 {
                inp.steer = d * inp.steer.abs().max(0.6);
            }
        } else if r.on_ground
            && speed > t.drift_min_speed + 4.0
            && !r.prev_input.drift
            && r.stun_t <= 0.0
            && mem.calm_t > 0.6
        {
            // Looks a little further ahead: the slide eases in rather than snapping round.
            let (turn, kmax) = turn_over(w, r.s + 4.0 + DRIFT_LEAD_TIME * speed, 30.0);
            // On the wide training oval (R 65 m) lesson bots still take the corner in a drift.
            let lesson = matches!(w.cfg.mode, RaceMode::Training(_));
            let kth = if easy { 1.0 / 28.0 } else if lesson { 1.0 / 70.0 } else { 1.0 / 44.0 };
            let amt = if easy { 1.2 } else if lesson { 0.4 } else { 0.7 };
            if turn.abs() > amt && kmax > kth && ramp_ahead.map_or(true, |d| d > 40.0) {
                let dir = -turn.signum();
                inp.drift = true;
                inp.steer = dir * inp.steer.abs().max(0.6);
            }
        }
    }

    // Rugs are readable road hazards, so bots jump them like hurdles.
    for rug in &w.play.rugs {
        if rug.pull > 0 || rug.owner == idx as u8 {
            continue;
        }
        let d = tr.s_delta(r.s, rug.s);
        if d > 0.0 && d < 40.0 {
            hurdle_ahead = Some(hurdle_ahead.map_or(d, |x| x.min(d)));
        }
    }

    // ---- jumps: ramp tricks, hurdles and rugs ----
    if !r.prev_input.jump && r.on_ground && r.stun_t <= 0.0 {
        if let Some(d) = ramp_ahead {
            let on_ramp = tr.sample_at(r.s).surface == Surface::Ramp;
            if !easy && !dummy && on_ramp && d < speed * 0.12 + 0.5 {
                inp.jump = true;
            }
        }
        if let Some(d) = hurdle_ahead {
            let lo = speed * 0.10 + 1.0;
            let hi = speed * 0.22 + 2.0;
            if !dummy && d > lo && d < hi {
                inp.jump = true;
            }
        }
    }

    if dummy {
        inp.drift = false;
        return finish_unstick(w, r, mem, err, inp);
    }

    // Lasers are straight rays; eggs are collected by running into them.
    if t.practice_lasers && !r.laser.overheated && r.mega_t <= 0.0 {
        inp.laser = r.laser.heat < 0.8 && w.racers.iter().enumerate().any(|(j, o)| {
            let v = o.body() - r.eye();
            let along = v.dot(r.forward());
            j != idx && o.respawn_t <= 0.0 && along > 1.0 && along < t.laser_range
                && (flat(v) - r.forward() * along).length() < 0.9
        });
    }

    // ---- items ----
    if let Some(item) = r.item {
        if !r.prev_input.item && r.stun_t <= 0.0 {
            let hold = mem.hold_t;
            let (turn40, _) = turn_over(w, r.s, 40.0);
            let straight = turn40.abs() < 0.25;
            let ahead = w.racers.iter().enumerate().any(|(j, o)| {
                if j == idx || o.respawn_t > 0.0 {
                    return false;
                }
                let d = flat(o.pos - r.pos);
                let dist = d.length();
                dist < 40.0
                    && dist > 2.0
                    && d.normalize().dot(r.forward()) > crate::math::cos(12f32.to_radians())
            });
            let behind_close = w.racers.iter().enumerate().any(|(j, o)| {
                j != idx && {
                    let gap = r.progress - o.progress;
                    gap > 0.0 && gap < 15.0
                }
            });
            let hunted = w.projectiles.iter().any(|p| {
                p.kind == ProjectileKind::HunterEgg
                    && p.target == Some(idx as u8)
                    && (p.pos - r.pos).length() < 60.0
            });
            let coins_near = w
                .coins
                .iter()
                .any(|c| c.alive && (c.pos - r.pos).length() < 30.0);
            let use_it = match item {
                Item::Laptop
                | Item::OrangePill
                | Item::EggScrambler
                | Item::MoneyPrinter | Item::RugPull => hold > 0.5,
                // Cake is worth it only when there is enough good to take (or
                // anything at all, once it has been held a while), and, like the
                // Rose, not once finished.
                Item::SharedCake => {
                    let take = good_things_to_take(w, idx);
                    hold > 0.5
                        && r.finished_at.is_none()
                        && (take >= CAKE_MIN_TAKE || (hold > 15.0 && take >= 1))
                }
                // Not once finished: the seller is for those still racing.
                Item::Rose => {
                    hold > 0.3
                        && r.finished_at.is_none()
                        && w.racers.iter().enumerate().any(|(j, o)| {
                            j != idx && o.finished_at.is_none() && o.respawn_t <= 0.0
                        })
                }
                Item::Feather => r.boost_t < 0.2 && (straight || hold > 8.0),
                Item::RollingEgg => ahead || hold > 12.0,
                Item::HunterEgg | Item::Lightning => true,
                Item::MegaLaser => straight || hold > 4.0,
                Item::Cactus => behind_close || hold > 12.0,
                Item::Shield => hunted || r.place == 1 || hold > 15.0,
                Item::Magnet => coins_near || hold > 10.0,
                Item::LaserTrump => hold > 0.3,
            };
            inp.item = use_it && (!easy || rng.gen::<f32>() < 0.2);
        }
    }

    finish_unstick(w, r, mem, err, inp)
}

/// How many good power-ups the other racers still racing hold, queue or have
/// running: what a Shared Cake would take. Racers already eating, down or
/// finished are left alone by a cake, so they do not count.
fn good_things_to_take(w: &World, idx: usize) -> usize {
    w.racers
        .iter()
        .enumerate()
        .filter(|(j, o)| {
            *j != idx && o.finished_at.is_none() && o.respawn_t <= 0.0 && o.effects.cake_eat <= 0.0
        })
        .map(|(_, o)| {
            usize::from(o.item.is_some_and(Item::is_good))
                + o.queue.iter().filter(|q| q.item.is_good()).count()
                + usize::from(o.effects.feather > 0.0 && o.boost_t > 0.0)
                + usize::from(o.shield_t > 0.0)
                + usize::from(o.magnet_t > 0.0)
                + usize::from(o.effects.pill > 0.0)
                + usize::from(o.effects.trump > 0.0)
        })
        .sum()
}

/// A bot keeps its Shared Cake until at least this many good power-ups are
/// there to take, in all, across the others.
const CAKE_MIN_TAKE: usize = 2;

fn finish_unstick(w: &World, r: &Racer, mem: BotMem, err: f32, mut inp: Input) -> Input {
    let _ = w;
    if mem.reverse_t > 0.0 {
        inp.brake = false;
        inp.throttle = 1.0;
        inp.drift = false;
        inp.steer = if err.abs() > 0.05 { -err.signum() } else { 0.0 };
    } else if err.abs() > 1.9 && r.speed.abs() < 8.0 {
        // Facing backwards after a spin: full lock toward the line.
        inp.steer = -err.signum();
        inp.throttle = 1.0;
        inp.brake = false;
        inp.drift = false;
    }
    inp
}

/// Battle bots (spec 17): no racing line. Empty-handed they go for the nearest
/// live egg ahead and collect it; holding an item they hunt the nearest
/// opponent ahead along the loop and fire when lined up (eggs forward or, looking
/// back, backward), drop cacti on a chaser, dodge
/// cacti, and (Hard, on the last feather, empty-handed) swerve away from a close
/// opponent. They always drive round the loop the way it runs and steer to a
/// point on the band ahead, so the island never gets in the way.
fn battle_input(w: &World, idx: usize, level: BotLevel, mem: BotMem, rng: &mut Pcg32) -> Input {
    let b = w.battle.as_ref().expect("battle bots need a battle");
    let r = &w.racers[idx];
    let tr = &*w.track;
    let t = &w.tun;
    let l = tr.length;
    let mut inp = Input::default();
    let speed = r.speed.max(0.0);
    let easy = level == BotLevel::Easy;
    let hard = level == BotLevel::Hard;
    let (fwd, rt) = (r.forward(), r.right());
    let ahead = |s: f32| (s - r.s).rem_euclid(l);
    let opps: Vec<usize> = (0..w.racers.len())
        .filter(|&j| j != idx && !b.is_out(j) && w.racers[j].respawn_t <= 0.0)
        .collect();
    let hittable = |j: usize| !b.flashing(j);
    // Opponent `j` in my frame: (along, lateral, distance).
    let rel = |j: usize| {
        let d = flat(w.racers[j].pos - r.pos);
        (d.dot(fwd), d.dot(rt), d.length())
    };
    let track_dist = |j: usize| {
        let g = ahead(w.racers[j].s);
        g.min(l - g)
    };
    let hw = 0.5 * tr.sample_at(r.s).width;
    let lane = hw - 2.5;

    // ---- what to go for ----
    let mut goal: Option<(f32, f32)> = None; // (metres ahead along the loop, lateral)
    let threat = opps
        .iter()
        .copied()
        .filter(|&j| track_dist(j) < 25.0)
        .min_by(|&a, &c| track_dist(a).total_cmp(&track_dist(c)));
    let fleeing = hard && b.feathers[idx] <= 1 && r.item.is_none() && threat.is_some();
    if let (true, Some(j)) = (fleeing, threat) {
        let o = &w.racers[j];
        let side = if o.lateral > r.lateral { -1.0 } else { 1.0 };
        goal = Some((20.0, side * lane));
    } else if r.item.is_some() {
        let prey = opps
            .iter()
            .copied()
            .filter(|&j| hittable(j))
            .min_by(|&a, &c| ahead(w.racers[a].s).total_cmp(&ahead(w.racers[c].s)))
            .or_else(|| {
                opps.iter()
                    .copied()
                    .min_by(|&a, &c| ahead(w.racers[a].s).total_cmp(&ahead(w.racers[c].s)))
            });
        if let Some(j) = prey {
            let o = &w.racers[j];
            goal = Some((ahead(o.s), o.lateral));
        }
    } else {
        // The nearest live Power Egg ahead.
        let mut row_s = Vec::new();
        for f in &tr.features {
            if let Feature::TargetRow { s, .. } = *f {
                row_s.push(s);
            }
        }
        let mut best: Option<(f32, f32)> = None;
        for tg in w.targets.iter().filter(|tg| tg.alive && !(tg.bad ^ (w.play.scramble > 0.0))) {
            let Some(&s) = row_s.get(tg.row as usize) else {
                continue;
            };
            let g = ahead(s);
            if g < 6.0 {
                continue;
            }
            let sm = tr.sample_at(s);
            let lat = (tg.pos - sm.pos).dot(sm.right);
            if best.map_or(true, |(bg, _)| g < bg) {
                best = Some((g, lat));
            }
        }
        // An opponent close ahead is worth a bash on the way.
        let bash_prey = opps
            .iter()
            .copied()
            .filter(|&j| hittable(j) && ahead(w.racers[j].s) < 18.0)
            .min_by(|&a, &c| ahead(w.racers[a].s).total_cmp(&ahead(w.racers[c].s)));
        goal = match (best, bash_prey) {
            (_, Some(j)) if !easy => Some((ahead(w.racers[j].s), w.racers[j].lateral)),
            (Some(e), _) => Some(e),
            (None, Some(j)) => Some((ahead(w.racers[j].s), w.racers[j].lateral)),
            (None, None) => None,
        };
    }

    // ---- aim point on the band ahead ----
    let la = 7.0 + speed * 0.4;
    let (mut s_aim, mut lat_t) = (r.s + la, r.lateral.clamp(-lane * 0.6, lane * 0.6));
    if let Some((g, lat)) = goal {
        if g < la && g > 2.0 {
            s_aim = r.s + g.max(4.0);
            lat_t = lat;
        } else {
            // Ease toward the goal's lateral as it comes closer.
            let k = (1.0 - (g - la) / 60.0).clamp(0.35, 1.0);
            lat_t = lat_t + (lat - lat_t) * k;
        }
    }
    // Dodge cacti.
    for h in &w.hazards {
        let d = flat(h.pos - r.pos);
        let along = d.dot(fwd);
        if along > 1.5 && along < 28.0 {
            let pr_lat = d.dot(rt) + r.lateral;
            if (pr_lat - lat_t).abs() < 2.8 {
                lat_t = if pr_lat > lat_t { pr_lat - 3.8 } else { pr_lat + 3.8 };
            }
        }
    }
    if easy {
        lat_t += rng.gen_range(-1.0f32..1.0);
    }
    let lat_t = lat_t.clamp(-lane, lane);
    let sm = tr.sample_at(s_aim);
    let target = sm.pos + sm.right * lat_t;

    // ---- pursuit ----
    let to = flat(target - r.pos);
    let err = wrap_angle(yaw_of(to) - r.yaw);
    let ld = to.length().max(3.0);
    let kappa = 2.0 * crate::math::sin(err) / ld;
    let tb = turn_rate(w, r, speed).max(0.3);
    let gain = if easy { 0.85 } else { 1.15 };
    inp.steer = (-speed.max(5.0) * kappa * gain / tb).clamp(-1.0, 1.0);
    if easy {
        inp.steer = (inp.steer + rng.gen_range(-0.12f32..0.12)).clamp(-1.0, 1.0);
    }

    // ---- speed: corner limit at our lateral, slow down to swing round ----
    let decel = t.brake * 0.5;
    let mut allowed = f32::INFINITY;
    let mut x = 0.0;
    while x < 36.0 {
        let k = tr.samples[tr.idx_at(r.s + x)].curvature;
        let k_eff = k / (1.0 + lat_t * k).max(0.2);
        let vt = corner_speed(w, r, k_eff, 0.92);
        allowed = allowed.min((vt * vt + 2.0 * decel * x).sqrt());
        x += 3.0;
    }
    inp.throttle = 1.0;
    if (speed > allowed + 2.0 && speed > 10.0) || (err.abs() > 1.2 && speed > 16.0) {
        inp.brake = true;
        inp.throttle = 0.0;
    } else if speed > allowed || (err.abs() > 0.7 && speed > 20.0) {
        inp.throttle = 0.0;
    }

    // ---- laser: only opponents on the forward ray ----
    if t.practice_lasers && !r.laser.overheated && r.mega_t <= 0.0 {
        let hot_ok = r.laser.heat < 0.8 && (r.prev_input.laser || r.laser.heat < 0.45);
        inp.laser = hot_ok && opps.iter().any(|&j| {
            let (along, lateral, _) = rel(j);
            along > 1.0 && along < t.laser_range && lateral.abs() < 0.9
        });
    }

    // ---- items ----
    if let Some(item) = r.item {
        if !r.prev_input.item && r.stun_t <= 0.0 {
            let hold = mem.hold_t;
            let lined_up = |lo: f32, hi: f32, tol: f32| {
                opps.iter().any(|&j| {
                    let (a, lat, _) = rel(j);
                    hittable(j) && a > lo && a < hi && lat.abs() < tol + a.abs() * 0.04
                })
            };
            let behind = opps.iter().any(|&j| {
                let (a, lat, _) = rel(j);
                hittable(j) && a < -3.0 && a > -30.0 && lat.abs() < 1.6
            });
            let chaser = opps.iter().any(|&j| {
                let (a, lat, _) = rel(j);
                a < -2.0 && a > -20.0 && lat.abs() < 4.0
            });
            let hunted = w.projectiles.iter().any(|p| {
                p.owner as usize != idx
                    && (p.target == Some(idx as u8) || p.kind == ProjectileKind::RollingEgg)
                    && (p.pos - r.pos).length() < 25.0
            });
            let prey_ahead = opps
                .iter()
                .any(|&j| hittable(j) && ahead(w.racers[j].s) < 90.0);
            let mut look_back = false;
            let use_it = match item {
                Item::RollingEgg => {
                    if lined_up(3.0, 45.0, 1.3) || hold > 10.0 {
                        true
                    } else if behind {
                        look_back = true;
                        r.look_back > 0.5
                    } else {
                        false
                    }
                }
                Item::HunterEgg => prey_ahead || hold > 4.0,
                Item::Cactus => chaser || hold > 8.0,
                Item::Shield => hunted || b.feathers[idx] <= 1 || hold > 3.0,
                Item::Lightning => opps.iter().any(|&j| hittable(j)) || hold > 2.0,
                Item::MegaLaser => lined_up(3.0, 55.0, 2.5) || hold > 6.0,
                Item::Feather => {
                    r.boost_t < 0.2 && ((hold > 1.5 && prey_ahead) || hold > 6.0 || fleeing)
                }
                Item::Magnet
                | Item::Laptop
                | Item::OrangePill
                | Item::EggScrambler
                | Item::SharedCake
                | Item::MoneyPrinter | Item::RugPull => true,
                // Never rolled in a battle.
                Item::LaserTrump | Item::Rose => false,

            };
            inp.look_back = look_back;
            inp.item = use_it && (!easy || rng.gen::<f32>() < 0.2);
        }
    }

    finish_unstick(w, r, mem, err, inp)
}
