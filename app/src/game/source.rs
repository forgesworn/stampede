//! Where a race's state comes from. The race screen only talks to
//! [`RaceSource`]: fixed 60 Hz stepping fed by the local player's input, a
//! render-ready [`RaceView`], and the events since the last frame. [`LocalRace`]
//! runs a `race_core::World` in-process (quick race, training, time trial,
//! attract). A client session for online play implements the same trait
//! (prediction + interpolation inside), without the screen changing.

use crate::ui::screens::{Ctx, Transition};
use race_core::math::DT;
use race_core::*;
use std::sync::Arc;

/// Most 60 Hz ticks run in one rendered frame (a slow frame drops the rest).
pub const MAX_STEPS: u32 = 5;

/// Everything the screen reads, borrowed from the source.
pub struct RaceView<'a> {
    pub cfg: &'a RaceConfig,
    pub track: &'a Arc<Track>,
    pub tun: &'a Tunables,
    /// State after the latest tick.
    pub racers: &'a [Racer],
    /// State one tick earlier (interpolation).
    pub prev: &'a [Racer],
    /// 0..1 between `prev` and `racers`.
    pub alpha: f32,
    pub projectiles: &'a [Projectile],
    pub hazards: &'a [Hazard],
    pub coins: &'a [CoinSpot],
    pub loose_coins: &'a [LooseCoin],
    pub targets: &'a [Target],
    pub play: &'a race_core::playful::PlayState,
    pub phase: RacePhase,
    pub race_time: f32,
    pub countdown_left: f32,
    pub lesson: Option<&'a LessonState>,
    /// Set in `RaceMode::Challenge` (score, clock, eliminations).
    pub challenge: Option<&'a ChallengeState>,
    /// Set in a battle (and the Last Feather challenge): feathers, clock, who is out.
    pub battle: Option<&'a BattleState>,
    pub lap_times: &'a [Vec<f32>],
    pub hurdles_active: bool,
}

pub trait RaceSource {
    fn view(&self) -> RaceView<'_>;
    /// The open-ground drill's state (SPEC 18.2), when this race is a drill.
    fn drill(&self) -> Option<&race_core::DrillState> {
        None
    }
    /// Seat of the local player (None for an all-bot attract race).
    fn local_seat(&self) -> Option<usize>;
    /// Advance `dt` seconds of wall time; `input` is asked once per 60 Hz tick
    /// for the local player. Returns the number of ticks run.
    fn advance(&mut self, dt: f32, input: &mut dyn FnMut() -> Input) -> u32;
    /// Events since the last call (all racers).
    fn drain_events(&mut self) -> Vec<Event>;
    /// Let the bot AI drive the local seat (shots, `--screen ..._auto`).
    fn set_autopilot(&mut self, _level: Option<BotLevel>, _forced: Forced) {}
    /// Hand the local player an item (dev/shots).
    fn give_local_item(&mut self, _item: Item, _uses: u8) {}
    /// Local cosmetic selection; online selections arrive from the host.
    fn set_attire(&mut self, _attire: [u8; 3]) {}
    /// Time Trial: record the local racer's run as a ghost (call before the first `advance`).
    fn record_ghost(&mut self, _on: bool) {}
    /// The local racer's finished run, once (None unless recording and finished).
    fn take_ghost(&mut self) -> Option<Ghost> {
        None
    }

    // ---- online hooks (all optional; offline sources keep the defaults) ----

    /// Connection state for the HUD dot (None: offline).
    fn net_status(&self) -> Option<NetStatus> {
        None
    }
    /// Does opening the pause menu stop the sim? Online it must not: the
    /// room keeps racing, so the source keeps being advanced (idle controls).
    fn pauses_sim(&self) -> bool {
        true
    }
    /// The race screen is paused: online the room races on, so the source
    /// lets a bot steer instead of freezing the player's ostrich.
    fn set_hands_off(&mut self, _on: bool) {}
    /// May the screen leave now (the race is over for everyone)? Online, a
    /// finished racer waits here for the host's results.
    fn ready_to_finish(&self) -> bool {
        true
    }
    /// Where leaving the race goes, if not the usual `after_race`: online it
    /// hands the session to the results screen, or back to the menu.
    fn exit_transition(&mut self, _ctx: &mut Ctx, _quit: bool) -> Option<Transition> {
        None
    }
}

/// What the HUD shows about an online race.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetStatus {
    /// Round trip (host: its slowest guest).
    pub rtt_ms: Option<u32>,
    /// 0 good, 1 ok, 2 poor.
    pub quality: u8,
    /// The link is gone for good: why. The race screen leaves with a message.
    pub lost: Option<String>,
    /// You finished and are waiting for the others.
    pub waiting: bool,
    pub host: bool,
    /// "<name> left" while a player who left the race is being announced.
    pub notice: Option<String>,
}

/// Buttons an autopilot holds down on top of the bot's own choices (shots).
#[derive(Clone, Copy, Debug, Default)]
pub struct Forced {
    pub look_back: bool,
    pub laser: bool,
    pub drift: bool,
    /// Fire the held item once the race clock passes this many seconds (the bot never fires the seat's item).
    pub item_at: Option<f32>,
}

/// The in-process race.
pub struct LocalRace {
    world: World,
    prev: Vec<Racer>,
    acc: f32,
    seat: Option<usize>,
    events: Vec<Event>,
    autopilot: Option<BotLevel>,
    forced: Forced,
    recorder: Option<GhostRecorder>,
    ghost: Option<Ghost>,
}

impl LocalRace {
    pub fn new(cfg: RaceConfig, seat: Option<usize>) -> LocalRace {
        let world = World::new(cfg);
        let prev = world.racers.clone();
        LocalRace { world, prev, acc: 0.0, seat, events: Vec::new(), autopilot: None, forced: Forced::default(), recorder: None, ghost: None }
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    /// Step exactly one tick with `input` for the local seat.
    fn tick(&mut self, input: Input) {
        self.prev.clone_from(&self.world.racers);
        let n = self.world.racers.len();
        let mut inputs = vec![Input::default(); n];
        if let Some(s) = self.seat.filter(|&s| s < n) {
            inputs[s] = match self.autopilot {
                Some(level) => {
                    let mut i = race_core::bot_input(&self.world, s, level);
                    // Like a player: rev inside the start-boost window, not before.
                    if self.world.phase == RacePhase::Countdown {
                        i.throttle = if self.world.countdown_left() <= self.world.tun.start_boost_window * 0.8 { 1.0 } else { 0.0 };
                    }
                    i.look_back |= self.forced.look_back;
                    i.laser |= self.forced.laser;
                    i.item |= self.forced.item_at.is_some_and(|t| self.world.phase != RacePhase::Countdown && self.world.race_time() > t);
                    i.drift |= self.forced.drift && i.steer.abs() > 0.35;
                    i
                }
                None => input,
            };
        }
        self.world.step(&inputs);
        self.events.extend(self.world.drain_events());
        if let (Some(rec), Some(s)) = (self.recorder.as_mut(), self.seat) {
            if self.ghost.is_none() {
                rec.record(&self.world);
                if self.world.racers.get(s).is_some_and(|r| r.finished_at.is_some()) {
                    self.ghost = self.recorder.take().and_then(|r| r.finish_world(&self.world));
                }
            }
        }
    }
}

impl RaceSource for LocalRace {
    fn set_attire(&mut self, attire: [u8; 3]) {
        if let Some(i) = self.seat {
            if let Some(r) = self.world.racers.get_mut(i) { r.attire = attire; }
            if let Some(r) = self.prev.get_mut(i) { r.attire = attire; }
        }
    }
    fn view(&self) -> RaceView<'_> {
        let w = &self.world;
        RaceView {
            cfg: &w.cfg,
            track: &w.track,
            tun: &w.tun,
            racers: &w.racers,
            prev: &self.prev,
            alpha: (self.acc / DT).clamp(0.0, 1.0),
            projectiles: &w.projectiles,
            hazards: &w.hazards,
            coins: &w.coins,
            loose_coins: &w.loose_coins,
            targets: &w.targets,
            play: &w.play,
            phase: w.phase,
            race_time: w.race_time(),
            countdown_left: w.countdown_left(),
            lesson: w.lesson.as_ref(),
            challenge: w.challenge.as_ref(),
            battle: w.battle.as_ref(),
            lap_times: &w.lap_times,
            hurdles_active: w.hurdles_active(),
        }
    }

    fn drill(&self) -> Option<&race_core::DrillState> {
        self.world.drill.as_ref()
    }

    fn local_seat(&self) -> Option<usize> {
        self.seat
    }

    fn advance(&mut self, dt: f32, input: &mut dyn FnMut() -> Input) -> u32 {
        self.acc += dt.clamp(0.0, 0.25);
        let mut steps = 0;
        while self.acc >= DT && steps < MAX_STEPS {
            let i = input();
            self.tick(i);
            self.acc -= DT;
            steps += 1;
        }
        if self.acc > DT {
            // Fell behind: drop the backlog instead of spiralling.
            self.acc %= DT;
        }
        steps
    }

    fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn set_autopilot(&mut self, level: Option<BotLevel>, forced: Forced) {
        self.autopilot = level;
        self.forced = forced;
    }

    fn give_local_item(&mut self, item: Item, uses: u8) {
        if let Some(s) = self.seat {
            self.world.give_item(s, item, uses);
        }
    }

    fn record_ghost(&mut self, on: bool) {
        self.recorder = match (on, self.seat) {
            (true, Some(s)) => Some(GhostRecorder::new(&self.world, s)),
            _ => None,
        };
    }

    fn take_ghost(&mut self) -> Option<Ghost> {
        self.ghost.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_steps_and_alpha() {
        let cfg = RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 7);
        let mut r = LocalRace::new(cfg, Some(0));
        let mut n = 0;
        // 10 frames of 25 ms = 250 ms = 15 ticks.
        for _ in 0..10 {
            n += r.advance(0.025, &mut Input::default);
        }
        assert!((14..=15).contains(&n), "{n}");
        let a = r.view().alpha;
        assert!((0.0..1.0).contains(&a), "{a}");
        assert_eq!(r.world().tick as u32, n);
    }

    #[test]
    fn a_long_frame_is_capped() {
        let cfg = RaceConfig::quick(TrackId::Savanna, 1, BotLevel::Easy, 7);
        let mut r = LocalRace::new(cfg, Some(0));
        assert_eq!(r.advance(0.2, &mut Input::default), MAX_STEPS);
    }
}
