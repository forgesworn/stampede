//! Repeatable close-ups of the real gameplay presentation, without a payout.

use super::{
    geometry,
    present::Presenter,
    source::RaceView,
    stage::{self, WorldKey},
};
use crate::render::ScenePass;
use crate::ui::screens::{Ctx, Screen, Transition};
use glam::Vec3;
use race_core::{BotLevel, Feature, RaceConfig, RacePhase, TrackId, World};
use race_render::{Camera, FrameScene};

pub fn fire() -> Box<dyn Screen> {
    sample(0)
}
pub fn laptop() -> Box<dyn Screen> {
    sample(1)
}
pub fn cake() -> Box<dyn Screen> {
    sample(2)
}
pub fn splat() -> Box<dyn Screen> {
    sample(3)
}
pub fn frog() -> Box<dyn Screen> {
    sample(4)
}
pub fn eggs() -> Box<dyn Screen> {
    sample(5)
}
pub fn pill() -> Box<dyn Screen> {
    sample(6)
}
pub fn printer() -> Box<dyn Screen> {
    sample(7)
}
pub fn chase() -> Box<dyn Screen> {
    sample(8)
}
pub fn rug() -> Box<dyn Screen> {
    sample(9)
}
/// A rose seller leaning on the player, with the seller's line on screen.
pub fn rose() -> Box<dyn Screen> {
    sample(10)
}

fn sample(kind: u8) -> Box<dyn Screen> {
    let mut w = World::new(RaceConfig::quick(TrackId::Savanna, 2, BotLevel::Normal, 19));
    w.phase = RacePhase::Racing;
    let mut s = w
        .track
        .features
        .iter()
        .find_map(|f| {
            if let Feature::TargetRow { s, .. } = f {
                Some(*s)
            } else {
                None
            }
        })
        .unwrap_or(60.0);
    if kind == 9 {
        s = w.track.length * 0.31;
        w.play.rugs = vec![race_core::mischief::Rug { s, rest: 45, owner: 1, ..Default::default() }];
    }
    for (i, r) in w.racers.iter_mut().enumerate() {
        let at = s - 5.0 + i as f32 * 2.0;
        r.pos = w.track.point_at(at, (i as f32 - 1.0) * 3.5);
        r.yaw = race_core::math::yaw_of(w.track.sample_at(at).tangent);
        r.visual_yaw = r.yaw;
        r.speed = 18.0;
        r.attire = [0, i as u8 + 1, i as u8 + 1];
    }
    match kind {
        0 => {
            for r in &mut w.racers {
                r.effects.burn = 3.5;
            }
        }
        1 => w.racers[0].effects.laptop = 1.5,
        2 => {
            for r in &mut w.racers {
                r.effects.cake_eat = 1.8;
            }
        }
        3 => w.racers[0].effects.splat = 2.7,
        6 => w.racers[0].effects.pill = 5.0,
        7 => {
            w.racers[0].effects.printer = 4.0;
            let r = &w.racers[0];
            for k in 0..4 {
                w.projectiles.push(race_core::Projectile {
                    kind: race_core::ProjectileKind::DollarBill,
                    pos: r.body() + Vec3::Y * 0.4 + r.forward() * (k as f32 + 1.0) * 1.5,
                    vel: r.forward() * 52.0,
                    owner: 0,
                    target: Some(1),
                    age: 0.0,
                    bounces: 0,
                    hint: r.track_idx,
                    s: r.s,
                    lateral: r.lateral,
                    id: k,
                });
            }
        }
        8 => {
            w.racers[0].effects.dismount = 5.0;
            w.racers[0].effects.chase_yaw = w.racers[0].yaw;
            let offset = -w.racers[0].forward() * 3.0 + w.racers[0].right() * 2.0;
            w.racers[0].effects.chase_x = offset.x;
            w.racers[0].effects.chase_z = offset.z;
        }
        9 => {}
        10 => {
            // The others well up the road, so the pair is in the clear.
            for i in 1..w.racers.len() {
                let at = s + 30.0 + i as f32 * 6.0;
                w.racers[i].pos = w.track.point_at(at, (i as f32 - 1.5) * 3.0);
            }
            w.racers[0].pos = w.track.point_at(s - 5.0, -1.0);
            w.racers[0].effects.rose = 3.2;
            let at = w.track.project_global(w.racers[0].pos);
            let (s, lateral) = (at.s, at.lateral + race_core::playful::ROSE_SIDE);
            w.play.roses.push(race_core::playful::RoseSeller {
                pos: w.track.point_at(s, lateral),
                yaw: w.racers[0].yaw,
                s,
                lateral,
                owner: 1,
                victim: 0,
                tried: 0,
                again: 0,
                side: 1,
                t: 3.2,
            });
        }
        4 => {
            w.racers[0].effects.tumble = 1.5;
            w.racers[0].speed = 0.0;
            w.play.frogs.push(race_core::playful::Frog {
                pos: w.racers[0].pos + Vec3::X * 1.5,
                yaw: w.racers[0].yaw,
                tumble: 1.5,
                victim: 0,
                paused: 0.0,
            });
        }
        _ => {
            for (k, egg) in w.targets.iter_mut().enumerate() {
                egg.alive = k < 5;
            }
        }
    }
    Box::new(PlayfulShot {
        world: w,
        presenter: Presenter::new(),
        scene: None,
        kind,
    })
}

struct PlayfulShot {
    world: World,
    presenter: Presenter,
    scene: Option<FrameScene>,
    kind: u8,
}

impl Screen for PlayfulShot {
    fn orientation(&self) -> crate::platform::os::Orientation {
        crate::platform::os::Orientation::Landscape
    }

    fn name(&self) -> &'static str {
        match self.kind {
            0 => "playful_fire",
            1 => "playful_laptop",
            2 => "playful_cake",
            3 => "playful_splat",
            4 => "playful_frog",
            6 => "playful_pill",
            7 => "playful_printer",
            8 => "playful_chase",
            9 => "playful_rug",
            10 => "playful_rose",
            _ => "playful_eggs",
        }
    }
    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        stage::set_quality(ctx.profile.quality);
        for (i, r) in self.world.racers.iter_mut().enumerate() {
            r.gait_phase = (ctx.time as f32 * 2.5 + i as f32 * 0.23).fract();
        }
        let w = &self.world;
        let v = RaceView {
            cfg: &w.cfg,
            track: &w.track,
            tun: &w.tun,
            racers: &w.racers,
            prev: &w.racers,
            alpha: 1.0,
            projectiles: &w.projectiles,
            hazards: &w.hazards,
            coins: &[],
            loose_coins: &[],
            targets: &w.targets,
            play: &w.play,
            phase: w.phase,
            race_time: 3.0,
            countdown_left: 0.0,
            lesson: None,
            challenge: None,
            battle: None,
            lap_times: &w.lap_times,
            hurdles_active: false,
        };
        let poses = self.presenter.poses(&v, ctx.dt, None);
        let r = &w.racers[0];
        let eye = if self.kind == 9 {
            r.pos - r.forward() * 8.0 + r.right() * 4.0 + Vec3::Y * 6.0
        } else if self.kind == 10 {
            // From the seller's side, a little ahead, so the rose faces the lens.
            r.pos + r.forward() * 5.5 + r.right() * 6.0 + Vec3::Y * 2.6
        } else {
            r.pos + r.forward() * 5.0 + r.right() * -5.5 + Vec3::Y * 3.6
        };
        let target = r.pos + Vec3::Y * 1.5 + r.right() * 1.2;
        let camera = Camera {
            eye,
            target,
            up: Vec3::Y,
            fov_y: 56f32.to_radians(),
            near: 0.2,
            far: 500.0,
        };
        let scene = self
            .presenter
            .scene(&v, 0, poses, &[], camera, Vec3::ZERO, None, 0.7);
        if stage::needs_history() {
            stage::push_history(&scene);
        }
        self.scene = Some(scene);
        Transition::None
    }
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        if self.kind == 3 || self.kind == 10 {

            let p = ui.layer_painter(egui::LayerId::background());
            super::playful_hud::draw(
                &p,
                ctx.viewport.safe,
                &self.world.racers[0],
                0.0,
                self.world.tun.playful.splat_time,
                self.world.phase,
                ctx.time as f32,
            );
        }
        Transition::None
    }
    fn paint_scene(&mut self, pass: &mut ScenePass) {
        if let Some(scene) = &self.scene {
            stage::draw(
                pass,
                WorldKey::Track(
                    TrackId::Savanna,
                    geometry::Course::of(TrackId::Savanna, self.world.cfg.mode),
                ),
                || geometry::cached(TrackId::Savanna),
                scene,
                1.0 / 60.0,
            );
        }
    }
}
