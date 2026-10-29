//! Events -> sounds, haptics and UI reactions (banners, camera shake), and the
//! continuous loops driven from the player's speed.

use super::source::RaceView;
use crate::haptics::{Haptics, Pulse};
use crate::sound;
use glam::Vec3;
use race_audio::{coin_pitch, LoopId, MusicTrack, Sfx};
use race_core::{Event, HitCause, Item, Racer, RacePhase};

/// What the screen should do in response to this frame's events.
#[derive(Default, Debug)]
pub struct Reactions {
    pub shake: f32,
    pub snap_camera: bool,
    pub go: bool,
    pub perfect_start: bool,
    pub too_early: bool,
    pub final_lap: bool,
    pub lap: Option<u8>,
    pub finished: Option<(u8, f32)>,
    pub lesson_done: Option<u8>,
    pub lesson_failed: bool,
    pub item_got: Option<(Item, u8)>,
    pub race_over: bool,
    pub respawned: bool,
    /// Drift King: points for the local racer's releases this frame.
    pub drift_points: Vec<f32>,
    /// Elimination: (racer, place) knocked out this frame.
    pub eliminated: Vec<(u8, u8)>,
    /// The challenge ended with this medal (0-3).
    pub challenge_done: Option<u8>,
    /// Battle: (racer, place) that went out this frame.
    pub battle_out: Vec<(u8, u8)>,
    /// Battle: (victim, attacker) feathers popped this frame.
    pub feather_lost: Vec<(u8, Option<u8>)>,
    /// Battle: (thief, from) feathers stolen this frame.
    pub feather_stolen: Vec<(u8, u8)>,
    /// The battle ended this frame.
    pub battle_over: bool,
}

#[derive(Default)]
pub struct Fx {
    chain: u32,
    chain_t: f32,
    muted: bool,
    last_vol: Option<(f32, f32, f32)>,
    paused: bool,
    laughed: [bool; 8],
    frog_laughed: [bool; 3],
}

fn play(sfx: Sfx, gain: f32, pitch: f32) {
    sound::with(|a| a.play(sfx, gain, pitch));
}

// Latch across snapshot corrections so laughter plays once, when the tumble
// reaches its sprawled pause. A late snapshot can enter that phase directly.
fn starts_laughing(heard: &mut bool, left: f32, duration: f32) -> bool {
    if left <= 0.0 { *heard = false; return false; }
    if !*heard && left <= duration * 0.75 && left > duration * 0.22 {
        *heard = true;
        return true;
    }
    false
}

impl Fx {
    pub fn new() -> Fx {
        Fx::default()
    }

    /// Start the race music and push the volume settings.
    pub fn start(&mut self, music: f32, effects: f32, muted: bool) {
        self.apply_volumes(music, effects, muted);
        sound::with(|a| {
            a.resume_all();
            a.set_music(Some(MusicTrack::Race));
        });
    }

    pub fn apply_volumes(&mut self, music: f32, effects: f32, muted: bool) {
        self.muted = muted;
        let v = (if muted { 0.0 } else { 1.0 }, music, effects);
        if self.last_vol != Some(v) {
            self.last_vol = Some(v);
            sound::with(|a| a.set_volumes(v.0, v.1, v.2));
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        if paused != self.paused {
            self.paused = paused;
            sound::with(|a| if paused { a.pause_all() } else { a.resume_all() });
        }
    }

    /// Leaving the race: silence the loops, hand the music back to the menus.
    pub fn stop(&mut self) {
        sound::with(|a| {
            a.stop_loops();
            a.resume_all();
            a.set_music(Some(MusicTrack::Menu));
        });
        self.paused = false;
    }

    /// Restarting: silence the loops but keep the race music going.
    pub fn stop_loops(&mut self) {
        sound::with(|a| {
            a.stop_loops();
            a.resume_all();
        });
        self.paused = false;
    }

    /// The speed-driven loops (footsteps, wind, skid, laser hum).
    pub fn loops(&mut self, r: &Racer, top: f32, dt: f32, racing: bool) {
        self.chain_t -= dt;
        if self.chain_t <= 0.0 {
            self.chain = 0;
        }
        let s = (r.speed.max(0.0) / top).clamp(0.0, 1.2);
        let grounded = r.on_ground && racing;
        let slip = (r.vel.x * r.yaw.cos() - r.vel.z * r.yaw.sin()).abs() / 8.0;
        sound::with(|a| {
            a.set_loop(LoopId::Footsteps, if grounded { (s * 3.0).min(1.0) } else { 0.0 }, 0.6 + 1.5 * s);
            a.set_loop(LoopId::Wind, s * s * 0.9, 0.8 + 0.6 * s);
            a.set_loop(LoopId::Skid, if r.drift.active && grounded { 0.5 + 0.5 * slip.clamp(0.0, 1.0) } else { 0.0 }, 0.9 + 0.3 * s);
            a.set_loop(LoopId::LaserHum, if r.laser.firing || r.mega_t > 0.0 { 0.6 + 0.4 * r.laser.heat } else { 0.0 }, 0.8 + 0.8 * r.laser.heat);
        });
    }

    /// Roulette tick sounds and the settle chime.
    pub fn roulette_tick(&self) {
        if !self.muted {
            play(Sfx::ItemTick, 0.6, 1.0);
        }
    }

    pub fn roulette_done(&self) {
        if !self.muted {
            play(Sfx::ItemGet, 1.0, 1.0);
        }
    }

    pub fn handle(&mut self, events: &[Event], v: &RaceView, seat: Option<usize>, hap: &mut dyn Haptics) -> Reactions {
        let mut re = Reactions::default();
        let local_pos = seat.and_then(|s| v.racers.get(s)).map(|r| r.pos);
        let is_me = |r: u8| seat == Some(r as usize);
        // Gain for something that happened to another racer.
        let near = |r: u8| -> f32 {
            if is_me(r) {
                return 1.0;
            }
            match (local_pos, v.racers.get(r as usize)) {
                (Some(me), Some(o)) => ((1.0 - (o.pos - me).length() / 60.0).clamp(0.0, 1.0)) * 0.55,
                (None, _) => 0.0,
                _ => 0.0,
            }
        };
        let snd = |sfx: Sfx, gain: f32, pitch: f32, muted: bool| {
            if !muted && gain > 0.04 {
                play(sfx, gain, pitch);
            }
        };
        let m = self.muted;
        for (i, r) in v.racers.iter().take(self.laughed.len()).enumerate() {
            if starts_laughing(&mut self.laughed[i], r.effects.tumble, v.tun.playful.tumble_time) {
                snd(Sfx::Laugh, near(i as u8), 1.0, m);
            }
        }
        // Pepe giggles on the ground where he fell; heard by whoever he bowled over.
        for (k, f) in v.play.frogs.iter().take(self.frog_laughed.len()).enumerate() {
            if starts_laughing(&mut self.frog_laughed[k], f.tumble, v.tun.playful.tumble_time) && f.victim != 255 {
                snd(Sfx::Laugh, near(f.victim), 1.0, m);
            }
        }
        for e in events {
            match *e {
                Event::Countdown { n } => snd(Sfx::CountdownBeep, 1.0, 1.0 + (3 - n.min(3)) as f32 * 0.0, m),
                Event::Go => {
                    snd(Sfx::CountdownGo, 1.0, 1.0, m);
                    re.go = true;
                }
                Event::StartBoost { racer } if is_me(racer) => {
                    re.perfect_start = true;
                    hap.pulse(Pulse::Boost);
                    snd(Sfx::Boost, 1.0, 1.0, m);
                }
                Event::Stumble { racer } if is_me(racer) && v.race_time < 1.0 && v.phase != RacePhase::Countdown => re.too_early = true,
                Event::Jump { racer } => snd(Sfx::Jump, near(racer), 1.0, m),
                Event::DriftHop { racer } => snd(Sfx::Jump, near(racer) * 0.5, 1.3, m),
                Event::DriftTier { racer, tier } if is_me(racer) => {
                    let s = match tier {
                        1 => Sfx::SparkTier1,
                        2 => Sfx::SparkTier2,
                        _ => Sfx::SparkTier3,
                    };
                    snd(s, 1.0, 1.0, m);
                }
                Event::Boost { racer, .. } | Event::BoostPad { racer } | Event::TrickBoost { racer } => {
                    snd(Sfx::Boost, near(racer), 1.0, m);
                    if is_me(racer) {
                        hap.pulse(Pulse::Boost);
                    }
                }
                Event::Trick { racer } => snd(Sfx::Jump, near(racer), 1.35, m),
                Event::Landed { racer, hard } => {
                    snd(Sfx::Land, near(racer) * if hard { 1.0 } else { 0.55 }, if hard { 0.9 } else { 1.15 }, m);
                    if is_me(racer) && hard {
                        hap.pulse(Pulse::Landing);
                        re.shake += 0.28;
                    }
                }
                Event::WallHit { racer, strength } if is_me(racer) && strength > 0.3 => {
                    snd(Sfx::Land, 0.6 * strength, 0.7, m);
                    re.shake += 0.14 * strength;
                    hap.pulse(Pulse::Landing);
                }
                Event::Respawned { racer } if is_me(racer) => {
                    re.snap_camera = true;
                    re.respawned = true;
                }
                Event::CoinPicked { racer, .. } if is_me(racer) => {
                    snd(Sfx::Coin, 0.9, coin_pitch(self.chain), m);
                    self.chain += 1;
                    self.chain_t = 1.0;
                    hap.pulse(Pulse::Coin);
                }
                Event::TargetDestroyed { racer, item, uses, .. } => {
                    snd(Sfx::TargetShatter, near(racer), 1.0, m);
                    if is_me(racer) {
                        if let Some(it) = item {
                            re.item_got = Some((it, uses));
                        }
                    }
                }
                Event::ItemUsed { racer, item } => {
                    let g = near(racer);
                    match item {
                        Item::Feather => snd(Sfx::Boost, g, 1.1, m),
                        Item::RollingEgg | Item::HunterEgg => snd(Sfx::EggThrow, g, 1.0, m),
                        Item::Cactus => snd(Sfx::Cactus, g * 0.7, 1.2, m),
                        Item::Laptop
                        | Item::OrangePill
                        | Item::EggScrambler
                        | Item::SharedCake
                        | Item::MoneyPrinter
                        | Item::RugPull
                        | Item::Rose => snd(Sfx::ItemGet, g * 0.7, 1.2, m),
                        Item::Magnet => snd(Sfx::ItemGet, g * 0.7, 1.5, m),
                        Item::LaserTrump => snd(Sfx::Trump, g, 1.0, m),
                        Item::Shield | Item::Lightning | Item::MegaLaser => {}
                    }
                    if is_me(racer) && item == Item::Feather {
                        hap.pulse(Pulse::Boost);
                    }
                }
                Event::Hit { victim, cause, .. } => {
                    let g = near(victim);
                    let s = match cause {
                        HitCause::RollingEgg | HitCause::HunterEgg => Some(Sfx::EggHit),
                        HitCause::Cactus => Some(Sfx::Cactus),
                        HitCause::Lightning => Some(Sfx::Lightning),
                        HitCause::MegaLaser => Some(Sfx::EggHit),
                        HitCause::Laser => Some(Sfx::LaserZap),
                        HitCause::Hurdle => Some(Sfx::Land),
                        HitCause::Bash => None,
                        HitCause::MoneyPrinter => Some(Sfx::EggHit),
                        HitCause::RugPull => Some(Sfx::Land),
                        HitCause::LaserTrump => Some(Sfx::Bash),
                        HitCause::Pepe => Some(Sfx::Land),
                    };
                    if let Some(s) = s {
                        snd(s, g, 1.0, m);
                    }
                    if is_me(victim) {
                        hap.pulse(Pulse::Hit);
                        re.shake += 0.45;
                    }
                }
                Event::Bash { attacker, victim, .. } => {
                    snd(Sfx::Bash, near(attacker).max(near(victim)), 1.0, m);
                    if is_me(attacker) {
                        hap.pulse(Pulse::Bash);
                        re.shake += 0.2;
                    }
                    if is_me(victim) {
                        hap.pulse(Pulse::Hit);
                        re.shake += 0.4;
                    }
                }
                Event::BashWhiff { racer } => snd(Sfx::Bash, near(racer) * 0.35, 1.5, m),
                Event::LaserZap { by, victim } if is_me(by) || is_me(victim) => snd(Sfx::LaserZap, 0.9, 1.0, m),
                Event::Overheated { racer } if is_me(racer) => snd(Sfx::ShieldBlock, 0.8, 0.6, m),
                Event::ShieldUp { racer } => snd(Sfx::ShieldUp, near(racer), 1.0, m),
                Event::ShieldBlock { racer, .. } => {
                    snd(Sfx::ShieldBlock, near(racer), 1.0, m);
                    if is_me(racer) {
                        hap.pulse(Pulse::Hit);
                    }
                }
                Event::Lightning { .. } => snd(Sfx::Lightning, 0.9, 1.0, m),
                Event::MegaLaser { racer } => snd(Sfx::MegaLaser, near(racer).max(0.4), 1.0, m),
                Event::HurdleCleared { racer } if is_me(racer) => snd(Sfx::ItemGet, 0.5, 1.4, m),
                Event::Gate { racer } if is_me(racer) => snd(Sfx::ItemGet, 0.6, 1.5, m),
                Event::Lap { racer, lap } if is_me(racer) => {
                    snd(Sfx::Lap, 1.0, 1.0, m);
                    re.lap = Some(lap);
                }
                Event::FinalLap { racer } if is_me(racer) => {
                    snd(Sfx::FinalLap, 1.0, 1.0, m);
                    re.final_lap = true;
                }
                Event::Finished { racer, place, time } if is_me(racer) => {
                    snd(Sfx::Finish, 1.0, 1.0, m);
                    re.finished = Some((place, time));
                }
                Event::LessonDone { stars } => {
                    snd(Sfx::Finish, 1.0, 1.0, m);
                    re.lesson_done = Some(stars);
                }
                Event::LessonFailed => {
                    snd(Sfx::MenuBack, 1.0, 0.8, m);
                    re.lesson_failed = true;
                }
                Event::DriftPoints { racer, points } if is_me(racer) => {
                    snd(Sfx::ItemGet, 0.7, 1.2 + (points / 20.0).min(0.6), m);
                    re.drift_points.push(points);
                }
                Event::Eliminated { racer, place, .. } => {
                    snd(Sfx::TargetShatter, if is_me(racer) { 1.0 } else { 0.8 }, 0.7, m);
                    if is_me(racer) {
                        re.shake += 0.3;
                    }
                    re.eliminated.push((racer, place));
                }
                Event::FeatherLost { racer, by } => {
                    if is_me(racer) {
                        hap.pulse(Pulse::Hit);
                        re.shake += 0.25;
                    }
                    snd(Sfx::EggHit, near(racer) * 0.6, 1.4, m);
                    re.feather_lost.push((racer, by));
                }
                Event::FeatherStolen { racer, from } => {
                    snd(Sfx::ItemGet, near(racer).max(near(from)), 1.6, m);
                    re.feather_stolen.push((racer, from));
                }
                Event::Out { racer, place, .. } => {
                    snd(Sfx::TargetShatter, if is_me(racer) { 1.0 } else { 0.8 }, 0.7, m);
                    if is_me(racer) {
                        re.shake += 0.3;
                        hap.pulse(Pulse::Bash);
                    }
                    re.battle_out.push((racer, place));
                }
                Event::BattleOver => {
                    snd(Sfx::Finish, 1.0, 1.0, m);
                    re.battle_over = true;
                }
                Event::ChallengeDone { medal } => {
                    snd(Sfx::Finish, 1.0, 1.0, m);
                    re.challenge_done = Some(medal);
                }
                Event::RaceOver => re.race_over = true,
                _ => {}
            }
        }
        let _ = Vec3::ZERO;
        re
    }
}

#[cfg(test)]
mod tests {
    use super::starts_laughing;

    #[test]
    fn laughter_waits_for_the_pause_and_does_not_repeat_on_snapshot_corrections() {
        let mut heard = false;
        assert!(!starts_laughing(&mut heard, 2.8, 2.8));
        assert!(!starts_laughing(&mut heard, 2.4, 2.8));
        assert!(starts_laughing(&mut heard, 2.0, 2.8));
        for left in [1.9, 2.1, 2.0, 1.8, 0.2] {
            assert!(!starts_laughing(&mut heard, left, 2.8));
        }
        assert!(!starts_laughing(&mut heard, 0.0, 2.8));
        assert!(starts_laughing(&mut heard, 1.5, 2.8), "late remote snapshot");
        assert!(!starts_laughing(&mut heard, 0.0, 2.8));
        assert!(!starts_laughing(&mut heard, 0.3, 2.8), "already getting back up");
    }
}
