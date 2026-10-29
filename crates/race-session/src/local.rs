//! [`LocalRace`]: the offline path (solo races against bots, training) with
//! the same view API as an online [`crate::ClientSession`], so the app has one
//! race screen.

use race_core::math::TICK_HZ;
use race_core::{Ghost, GhostRecorder, Input, RaceConfig, World};

use crate::proto::ResultsView;
use crate::view::{results_from_world, RaceDriver, RaceView};

/// Most world ticks stepped in one `update` (a stalled frame must not spiral).
const MAX_CATCH_UP: u32 = 12;
/// Longest frame step counted, ms.
const MAX_FRAME_MS: u64 = 100;

/// A race with no network: wraps `World` and steps it at 60 Hz from the
/// frame times.
pub struct LocalRace {
    view: RaceView,
    sim_ms: u64,
    last_now: Option<u64>,
    paused: bool,
    results: Option<ResultsView>,
    ghost_rec: Option<GhostRecorder>,
    ghost: Option<Ghost>,
}

impl LocalRace {
    /// `you` is the racer index the controls drive (usually 0, a `Human`).
    /// The grid countdown starts on the first `update`.
    pub fn new(cfg: RaceConfig, you: usize) -> LocalRace {
        let you = you.min(cfg.racers.len().saturating_sub(1));
        LocalRace {
            view: RaceView::new(cfg, you),
            sim_ms: 0,
            last_now: None,
            paused: false,
            results: None,
            ghost_rec: None,
            ghost: None,
        }
    }

    /// Record a Time Trial ghost of your racer: every world tick is sampled
    /// inside `update` (exactly as `GhostRecorder::record` after each
    /// `World::step`), however many ticks a frame steps. Turn it on before
    /// the first `update`.
    pub fn record_ghost(&mut self, on: bool) {
        self.ghost_rec = on.then(|| GhostRecorder::new(&self.view.world, self.view.you));
        self.ghost = None;
    }

    /// The ghost of your run, once your racer has finished (with
    /// `record_ghost(true)`). Store it if it beats the best (`Ghost::time`).
    pub fn ghost(&self) -> Option<&Ghost> {
        self.ghost.as_ref()
    }

    /// Takes the finished ghost.
    pub fn take_ghost(&mut self) -> Option<Ghost> {
        self.ghost.take()
    }

    /// The real sim (training lesson state, tunables, lap times).
    pub fn world(&self) -> &World {
        &self.view.world
    }

    /// Freeze or resume the sim (frame times keep flowing).
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// The world's end condition has fired (race over, lesson done or failed).
    pub fn is_over(&self) -> bool {
        self.results.is_some()
    }
}

impl RaceDriver for LocalRace {
    fn update(&mut self, now_ms: u64, input: Input) {
        let dt = self.last_now.map_or(0, |l| now_ms.saturating_sub(l)).min(MAX_FRAME_MS);
        self.last_now = Some(now_ms);
        if !self.paused {
            self.sim_ms += dt;
        }
        let you = self.view.you;
        let w = &mut self.view.world;
        let mut events = Vec::new();
        let mut steps = 0;
        while steps < MAX_CATCH_UP && !self.paused && w.tick * 1000 / TICK_HZ as u64 <= self.sim_ms {
            let mut inputs = vec![Input::default(); w.racers.len()];
            inputs[you] = input;
            w.step(&inputs);
            events.extend(w.drain_events());
            steps += 1;
            if let Some(rec) = self.ghost_rec.as_mut() {
                rec.record(w);
                if w.racers[you].finished_at.is_some() {
                    self.ghost = self.ghost_rec.take().and_then(|r| r.finish_world(w));
                }
            }
        }
        self.view.events = events;
        self.view.refresh(None);
        if self.results.is_none() && self.view.world.phase == race_core::RacePhase::Finished {
            self.results = Some(results_from_world(&self.view.world));
        }
        self.view.over = self.results.is_some();
        if let Some(res) = &self.results {
            self.view.freeze_places(res);
            self.view.hud.place = self.view.world.racers[self.view.you].place;
        }
    }

    fn view(&self) -> Option<&RaceView> {
        Some(&self.view)
    }

    fn results(&self) -> Option<&ResultsView> {
        self.results.as_ref()
    }
}
