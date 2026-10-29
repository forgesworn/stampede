//! Gameplay: the race screen, sim -> renderer mapping, cameras, HUD, touch
//! controls, audio and haptics wiring, and the shared 3D stage.
//! Owned by the gameplay agent. Entry point for the menus: [`race_screen`].

pub mod battle_fx;
pub mod battle_hud;
pub mod camera;
pub mod challenge_hud;
pub mod controls;
pub mod drill_hud;
pub mod drill_props;
pub mod fx;
pub mod geometry;
pub mod ghost_hud;
pub mod ghosts;
pub mod hud;
pub mod icons;
pub mod outcome;
pub mod placing;
pub mod present;
mod playful_hud;
pub mod race;
pub mod recording;
pub mod shots;
pub mod shots_training;
pub mod shots_playful;
pub mod source;
pub mod source_net;
mod spectate;
pub mod stage;

use crate::flow::RaceLaunch;
use crate::ui::screens::Screen;

/// The screen that runs a race (solo, training, time trial; online later).
pub fn race_screen(launch: RaceLaunch) -> Box<dyn Screen> {
    Box::new(race::new(launch, race::Staging::default()))
}
