//! Repeatable drill snapshots for `--shot`: the real race screen and HUD over
//! a staged World. The pilot stays at the rep while its running pose animates.

use super::race::{self, Staging};
use super::source::{RaceSource, RaceView};
use crate::flow::{RaceKind, RaceLaunch};
use crate::ui::screens::{training::TrainingScreen, Screen};
use race_core::drill::RepInfo;
use race_core::{DrillId, DrillState, Event, Input, RaceConfig, RacePhase, World};

pub fn training_menu() -> Box<dyn Screen> {
    Box::new(TrainingScreen::sample())
}

pub fn drill_slalom() -> Box<dyn Screen> {
    staged("drill_slalom", DrillId::Slalom, false)
}

pub fn drill_ring() -> Box<dyn Screen> {
    staged("drill_ring", DrillId::DriftRing, false)
}

pub fn drill_hurdles() -> Box<dyn Screen> {
    staged("drill_hurdles", DrillId::Hurdles, false)
}

pub fn drill_over() -> Box<dyn Screen> {
    staged("drill_over", DrillId::Slalom, true)
}

fn staged(name: &'static str, id: DrillId, over: bool) -> Box<dyn Screen> {
    let config = RaceConfig::drill(id, race_core::TrackId::Field, 7);
    let mut world = World::new(config.clone());
    world.phase = if over {
        RacePhase::Finished
    } else {
        RacePhase::Racing
    };
    let ds = world.drill.as_mut().expect("drill config");
    ds.level = 7;
    ds.best_level = 7;
    ds.reps_in_level = 2;
    ds.clean_total = 26;
    ds.stars = 2;
    ds.strikes = if over { 3 } else { 1 };
    ds.over = over;
    ds.rep_limit = Some(10.0);
    ds.rep_t = 3.4;
    let pilot = &mut world.racers[0];
    match ds.current_rep() {
        Some(RepInfo::Gate { centre, yaw, .. }) => {
            pilot.pos = centre - race_core::math::forward(yaw) * 14.0;
            pilot.yaw = yaw;
        }
        Some(RepInfo::Ring { centre, radius, .. }) => {
            pilot.pos = centre + glam::Vec3::X * radius;
            pilot.yaw = std::f32::consts::PI;
            pilot.drift.active = true;
            pilot.drift.dir = 1;
            pilot.drift.tier = 3;
            pilot.drift.charge = 3.0;
            ds.ring_swept_deg = 235.0;
        }
        Some(RepInfo::Logs { remaining, yaw }) => {
            pilot.pos = remaining[0] - race_core::math::forward(yaw) * 14.0;
            pilot.yaw = yaw;
        }
        _ => {}
    }
    pilot.pos.y = world.track.terrain_height(pilot.pos.x, pilot.pos.z);
    pilot.ground_y = pilot.pos.y;
    pilot.visual_yaw = pilot.yaw;
    pilot.speed = if over { 0.0 } else { 12.0 };
    pilot.vel = pilot.forward() * pilot.speed;
    let launch = RaceLaunch {
        config,
        kind: RaceKind::Drill(id),
        local_seat: 0,
    };
    let events = if over {
        vec![Event::Drill(race_core::DrillEvent::Over), Event::RaceOver]
    } else {
        Vec::new()
    };
    Box::new(
        race::with_source(
            launch,
            Staging::default(),
            Box::new(DrillShot { world, events }),
        )
        .named(name),
    )
}

struct DrillShot {
    world: World,
    events: Vec<Event>,
}

impl RaceSource for DrillShot {
    fn view(&self) -> RaceView<'_> {
        let w = &self.world;
        RaceView {
            cfg: &w.cfg,
            track: &w.track,
            tun: &w.tun,
            racers: &w.racers,
            prev: &w.racers,
            alpha: 1.0,
            projectiles: &w.projectiles,
            hazards: &w.hazards,
            coins: &w.coins,
            loose_coins: &w.loose_coins,
            targets: &w.targets,
            play: &w.play,
            phase: w.phase,
            race_time: 32.0,
            countdown_left: 0.0,
            lesson: None,
            challenge: None,
            battle: None,
            lap_times: &w.lap_times,
            hurdles_active: false,
        }
    }

    fn drill(&self) -> Option<&DrillState> {
        self.world.drill.as_ref()
    }

    fn local_seat(&self) -> Option<usize> {
        Some(0)
    }

    fn advance(&mut self, dt: f32, _input: &mut dyn FnMut() -> Input) -> u32 {
        let pilot = &mut self.world.racers[0];
        pilot.gait_phase += dt * pilot.speed;
        1
    }

    fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
}
