//! The render-ready race view and the one trait the app drives: online
//! ([`crate::ClientSession`]) and offline ([`crate::LocalRace`]) races look the
//! same to it.

use race_core::{
    Event, Input, Item, LessonState, RacePhase, RaceConfig, Racer, World, GP_POINTS,
};

use crate::proto::{ResultRow, ResultsView};

/// Everything the HUD shows, read off the own racer and the clock.
#[derive(Clone, Debug, Default)]
pub struct Hud {
    pub place: u8,
    pub racers: u8,
    /// Current lap, 1-based, at most `laps`.
    pub lap: u8,
    pub laps: u8,
    pub final_lap: bool,
    /// Race seconds (0 until GO).
    pub race_time: f32,
    /// Seconds to GO while the grid countdown runs, else 0.
    pub countdown: f32,
    /// m/s along the heading.
    pub speed: f32,
    pub coins: u32,
    pub item: Option<Item>,
    pub item_uses: u8,
    /// Laser heat 0..1.
    pub heat: f32,
    pub overheated: bool,
    /// Drift sparks tier 0..3.
    pub drift_tier: u8,
    pub boosting: bool,
    pub wrong_way: bool,
    pub finished: bool,
    pub finish_time: Option<f32>,
    /// Round trip to the host (online only).
    pub ping_ms: Option<u32>,
    pub lesson: Option<LessonState>,
}

impl Hud {
    pub fn from_world(w: &World, you: usize, ping_ms: Option<u32>) -> Hud {
        let r: &Racer = &w.racers[you.min(w.racers.len().saturating_sub(1))];
        let laps = w.cfg.laps;
        let lap = r.lap.clamp(1, laps.max(1));
        Hud {
            place: r.place.max(1),
            racers: w.racers.len() as u8,
            lap,
            laps,
            final_lap: lap == laps && laps > 0,
            race_time: if r.finished_at.is_some() { r.finished_at.unwrap_or(0.0) } else { w.race_time() },
            countdown: if w.phase == RacePhase::Countdown { w.countdown_left() } else { 0.0 },
            speed: r.speed,
            coins: r.coins,
            item: r.item,
            item_uses: r.item_uses,
            heat: r.laser.heat,
            overheated: r.laser.overheated,
            drift_tier: r.drift.tier,
            boosting: r.boost_t > 0.0,
            wrong_way: r.wrong_way,
            finished: r.finished_at.is_some(),
            finish_time: r.finished_at,
            ping_ms,
            lesson: w.lesson.clone(),
        }
    }
}

/// One frame of a race, ready to draw. Rebuilt by `update`; borrow it with
/// `view()`.
///
/// `world` is a full `race_core::World` whose dynamic state is current for
/// display: for a local race it is the real sim; for an online client it is a
/// shadow that is never stepped: `racers` (own predicted + correction offset,
/// others interpolated), `projectiles`, `hazards`, `coins[i].alive` and
/// `targets[i].alive` (coins sit at their home), `loose_coins`, `phase`, `tick`
/// (so `race_time()` / `countdown_left()` work) and `cfg`. Static data (track,
/// coin and target positions) is the same as the host's.
pub struct RaceView {
    pub world: World,
    /// Your racer index.
    pub you: usize,
    pub hud: Hud,
    /// Racer indices, 1st first.
    pub standings: Vec<usize>,
    /// Events since the previous `update`: sounds and FX. Online, footsteps
    /// and the 3-2-1-GO beeps are derived locally; events of your own motion
    /// come from the prediction, not the host.
    pub events: Vec<Event>,
    /// The visual correction currently added to your racer's position
    /// (online; zero offline). Already applied in `world.racers[you]`.
    pub correction: race_core::Vec3,
    /// The race is over (results exist).
    pub over: bool,
}

impl RaceView {
    pub fn new(cfg: RaceConfig, you: usize) -> RaceView {
        Self::from_world(World::new(cfg), you)
    }

    pub(crate) fn from_world(world: World, you: usize) -> RaceView {
        let mut v = RaceView {
            world,
            you,
            hud: Hud::default(),
            standings: Vec::new(),
            events: Vec::new(),
            correction: race_core::Vec3::ZERO,
            over: false,
        };
        v.refresh(None);
        v
    }

    pub fn own(&self) -> &Racer {
        &self.world.racers[self.you]
    }

    pub fn others(&self) -> impl Iterator<Item = &Racer> {
        let you = self.you;
        self.world.racers.iter().enumerate().filter(move |(i, _)| *i != you).map(|(_, r)| r)
    }

    /// Once the race is over the results are the truth: places and standings
    /// follow them (racers that keep driving after the end must not reshuffle).
    pub(crate) fn freeze_places(&mut self, res: &ResultsView) {
        for row in &res.rows {
            if let Some(r) = self.world.racers.get_mut(row.racer as usize) {
                r.place = row.place;
            }
        }
        self.standings = res.standings();
    }

    /// Recompute `hud` and `standings` from `world`.
    pub(crate) fn refresh(&mut self, ping_ms: Option<u32>) {
        self.hud = Hud::from_world(&self.world, self.you, ping_ms);
        let mut order: Vec<usize> = (0..self.world.racers.len()).collect();
        order.sort_by_key(|&i| (self.world.racers[i].place.max(1), i));
        self.standings = order;
    }
}

/// Results of a finished (or over) world, shared by the host and local races.
pub fn results_from_world(w: &World) -> ResultsView {
    let order = w.standings();
    let rows = order
        .iter()
        .enumerate()
        .map(|(p, &i)| {
            let r = &w.racers[i];
            let setup = &w.cfg.racers[i.min(w.cfg.racers.len().saturating_sub(1))];
            ResultRow {
                racer: i as u8,
                name: setup.name.clone(),
                character: setup.character,
                place: p as u8 + 1,
                bot: matches!(w.kind(i), race_core::RacerKind::Bot(_)),
                time: r.finished_at,
                best_lap: w.lap_times.get(i).and_then(|l| l.iter().copied().reduce(f32::min)),
                coins: r.coins,
                points: GP_POINTS.get(p).copied().unwrap_or(0),
            }
        })
        .collect();
    ResultsView { track: w.cfg.track, laps: w.cfg.laps, rows }
}

/// Move the rows of racers whose player left (`left`) after everyone else,
/// renumber the places and points, and take the time, best lap and points
/// off the leavers' rows: the bot that drove their racer on earned them,
/// not the player.
pub fn rank_leavers_last(res: &mut ResultsView, left: &[u8]) {
    if left.is_empty() {
        return;
    }
    let (mut rows, gone): (Vec<ResultRow>, Vec<ResultRow>) =
        std::mem::take(&mut res.rows).into_iter().partition(|r| !left.contains(&r.racer));
    let first_gone = rows.len();
    rows.extend(gone);
    for (p, r) in rows.iter_mut().enumerate() {
        r.place = p as u8 + 1;
        if p < first_gone {
            r.points = GP_POINTS.get(p).copied().unwrap_or(0);
        } else {
            r.bot = false;
            r.time = None;
            r.best_lap = None;
            r.points = 0;
        }
    }
    res.rows = rows;
}

/// What the app's race screen drives, online or offline.
///
/// Call `update` once per rendered frame with the frame's time in
/// milliseconds (any monotonic clock) and the player's current controls, then
/// read `view()`.
pub trait RaceDriver {
    fn update(&mut self, now_ms: u64, input: Input);
    /// The current frame; `None` before a race exists.
    fn view(&self) -> Option<&RaceView>;
    /// Set once the race is over.
    fn results(&self) -> Option<&ResultsView>;
}
