//! Sim state -> renderer data: interpolated [`OstrichPose`]s and the per-frame
//! [`FrameScene`] (coins, targets, eggs, cacti, beams, shields, emitters, bursts).

use super::source::RaceView;
use glam::Vec3;
use race_core as core;
use race_core::math::{forward, wrap_angle, DT};
use race_core::{CharClass, Event, HitCause, LaserHit, Racer};
use race_render as rr;
use race_render::{Accessory, Beam, Burst, BurstKind, Camera, CoinInst, Emitter, FrameScene, Hazard, MirrorView, OstrichPose, Projectile, ProjectileKind, Shield, TargetInst};

/// Smoothed presentation state per racer (lean, pitch and tuck ease at the render rate).
#[derive(Clone, Copy, Default)]
struct RacerFx {
    lean: f32,
    pitch: f32,
    tuck: f32,
    /// 0..1 Sonic leg wheel, eased (in over 0.12 s, out over 0.25 s).
    wheel: f32,
    ready: bool,
    unseated: bool,
    remount: f32,
    last_offset: Vec3,
    /// Which way the ostrich is bowled (+1 right), fixed when the rider comes off
    /// so the roll cannot flip as the loose ostrich walks back to the road centre.
    knock_side: f32,
}

/// Seconds coins take to fly from a rugged racer to the rugger, from the first
/// coin leaving to the last arriving.
pub const COIN_FLIGHT_SECS: f32 = 0.8;
/// Most coins drawn in flight at once (a bigger purse is thinned out).
const MAX_FLYING_COINS: usize = 24;
/// The stagger between the first and last coin leaving.
const COIN_STAGGER: f32 = 0.25;

/// One coin in the air from a rugged racer to the rugger.
#[derive(Clone, Copy)]
struct FlyingCoin {
    from: Vec3,
    to: usize,
    start: f32,
    delay: f32,
    arrived: bool,
}

/// The render style of each cause of a knock-off. One style per cause.
pub fn knock_style(k: core::KnockKind) -> rr::KnockStyle {
    match k {
        core::KnockKind::None => rr::KnockStyle::None,
        core::KnockKind::Printer => rr::KnockStyle::Printer,
        core::KnockKind::Rug => rr::KnockStyle::Rug,
        core::KnockKind::Pepe => rr::KnockStyle::Pepe,
        core::KnockKind::Trump => rr::KnockStyle::Trump,
    }
}

#[derive(Default)]
pub struct Presenter {
    fx: Vec<RacerFx>,
    coin_flights: Vec<FlyingCoin>,
    /// Monotonic animation clock (seconds).
    pub time: f32,
    /// The local seat and the colour variant it wears (None: everyone in their own colours).
    pub look: Option<(usize, u8)>,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp_angle(a: f32, b: f32, t: f32) -> f32 {
    a + wrap_angle(b - a) * t
}

fn ease_to(cur: f32, target: f32, rate: f32, dt: f32) -> f32 {
    cur + (target - cur) * (1.0 - (-rate * dt).exp())
}

fn accessory(a: core::Accessory) -> Accessory {
    match a {
        core::Accessory::Scarf => Accessory::Scarf,
        core::Accessory::Goggles => Accessory::Goggles,
        core::Accessory::Crown => Accessory::Crown,
        core::Accessory::StarHeadband => Accessory::StarHeadband,
        core::Accessory::VikingHelmet => Accessory::VikingHelmet,
        core::Accessory::Mohawk => Accessory::Mohawk,
        core::Accessory::GoldChainShades => Accessory::ChainShades,
        core::Accessory::Cap => Accessory::Cap,
    }
}

pub fn class_scale(c: CharClass) -> f32 {
    match c {
        CharClass::Light => 0.95,
        CharClass::Medium => 1.0,
        CharClass::Heavy => 1.1,
    }
}

/// A pose that only carries a character's look (colours, accessory, scale).
pub fn character_pose(id: u8) -> OstrichPose {
    let c = core::Character::get(id);
    OstrichPose {
        body: c.body,
        neck: c.neck,
        legs: c.legs,
        beak: c.beak,
        accent: c.accent,
        accessory: accessory(c.accessory),
        scale: class_scale(c.class),
        ..Default::default()
    }
}

/// Recolour `pose` with the bought colour variant `variant` of `character` (0 leaves it as it is).
pub fn apply_variant(pose: &mut OstrichPose, character: u8, variant: u8) {
    if let Some(p) = crate::ui::progress::palette(character, variant) {
        pose.body = p.body;
        pose.neck = p.neck;
        pose.legs = p.legs;
        pose.beak = p.beak;
        pose.accent = p.accent;
    }
}

/// True while a racer is out of sight (fell, waiting to respawn).
pub fn hidden(r: &Racer) -> bool {
    r.respawn_t > 0.0
}

impl Presenter {
    pub fn new() -> Presenter {
        Presenter::default()
    }

    /// Interpolated renderer poses for every racer. `hide_head` is the seat in first person.
    pub fn poses(&mut self, v: &RaceView, dt: f32, hide_head: Option<usize>) -> Vec<OstrichPose> {
        self.time += dt;
        let n = v.racers.len();
        self.fx.resize(n, RacerFx::default());
        let a = v.alpha;
        let mut out = Vec::with_capacity(n);
        for (i, r) in v.racers.iter().enumerate() {
            let p = v.prev.get(i).unwrap_or(r);
            let jump = (r.pos - p.pos).length() > 6.0 || hidden(r) != hidden(p);
            let a = if jump { 1.0 } else { a };
            let pos = p.pos.lerp(r.pos, a);
            let yaw = lerp_angle(p.yaw, r.yaw, a);
            let off0 = wrap_angle(p.visual_yaw - p.yaw);
            let off1 = wrap_angle(r.visual_yaw - r.yaw);
            let gait = {
                let mut d = r.gait_phase - p.gait_phase;
                if d < -0.5 {
                    d += 1.0;
                }
                if d > 0.5 {
                    d -= 1.0;
                }
                (p.gait_phase + d * a).rem_euclid(1.0)
            };
            let vel_h = Vec3::new(r.vel.x, 0.0, r.vel.z).length().max(0.5);
            let spun = r.stun_t > 0.0;
            let lean_t = if spun {
                0.0
            } else {
                (-r.yaw_rate * 0.10 * (r.speed / 20.0).clamp(0.0, 1.0)).clamp(-0.28, 0.28) + if r.drift.active { r.drift.dir as f32 * 0.22 } else { 0.0 }
            };
            let pitch_t = (-(r.vel.y.atan2(vel_h))).clamp(-0.5, 0.5) * 0.8;
            let tuck_t = if r.on_ground { 0.0 } else { 1.0 };
            let f = &mut self.fx[i];
            if !f.ready {
                *f = RacerFx {
                    lean: lean_t,
                    pitch: pitch_t,
                    tuck: tuck_t,
                    ready: true,
                    ..Default::default()
                };
            }
            f.lean = ease_to(f.lean, lean_t, 10.0, dt);
            f.pitch = ease_to(f.pitch, pitch_t, 8.0, dt);
            f.tuck = ease_to(f.tuck, tuck_t, 14.0, dt);
            // Legs blur into a wheel while drifting and while trumping.
            let wheel_t = if r.trumping() || r.drift.active { 1.0 } else { 0.0 };
            let step = dt / if wheel_t > f.wheel { 0.12 } else { 0.25 };
            f.wheel = if wheel_t > f.wheel { (f.wheel + step).min(wheel_t) } else { (f.wheel - step).max(wheel_t) };
            if r.trumping() {
                f.wheel = 1.0;
            }
            let mut pose = character_pose(r.character);
            apply_variant(&mut pose, r.character, r.attire[0]);
            pose.rider = r.character.wrapping_add(r.idx);
            pose.attire = [r.attire[1], r.attire[2]];
            pose.laptop = r.effects.laptop > 0.0;
            pose.cake = r.effects.cake_eat > 0.0;
            pose.shells = r.effects.splat > 0.0;
            pose.burning = r.effects.burn > 0.0;
            pose.printing = r.effects.printer > 0.0;
            pose.dismounted = r.effects.dismount > 0.0;
            pose.chase_yaw = r.effects.chase_yaw;
            pose.grid_start = r.effects.le_mans;
            if pose.dismounted {
                let offset = r.chase_offset();
                let ground = v
                    .track
                    .project(r.pos + offset, r.track_idx as usize)
                    .ground_y;
                pose.chase_offset = Vec3::new(offset.x, ground - r.pos.y, offset.z);
                pose.chase_age = if v.phase == core::RacePhase::Countdown {
                    -1.0
                } else {
                    (core::mischief::DISMOUNT_TIME - r.effects.dismount).max(0.0)
                };
                f.last_offset = pose.chase_offset;
            }
            if pose.dismounted {
                if !f.unseated {
                    // Bowled away from the side the jockey is thrown to.
                    let aside = r.chase_offset().dot(core::math::right(r.yaw));
                    f.knock_side = if aside >= 0.0 { -1.0 } else { 1.0 };
                }
                pose.knock_age = pose.chase_age.max(0.0);
            } else if r.stun_t > 0.0 {
                pose.knock_age = (v.tun.egg_spin - r.stun_t).max(0.0);
            }
            pose.knock = if pose.dismounted || r.stun_t > 0.0 { knock_style(r.effects.knock) } else { rr::KnockStyle::None };
            pose.knock_side = if f.knock_side < 0.0 { -1.0 } else { 1.0 };
            if f.unseated && !pose.dismounted {
                f.remount = 0.25;
            }
            f.unseated = pose.dismounted;
            f.remount = (f.remount - dt).max(0.0);
            if f.remount > 0.0 {
                pose.boarding = (1.0 - f.remount / 0.25).clamp(0.001, 0.999);
                pose.boarding_from = f.last_offset;
            }
            if r.effects.tumble > 0.0 {
                pose.tumble = (1.0 - r.effects.tumble / v.tun.playful.tumble_time).clamp(0.001, 0.999);
            }
            match r.attire[1] {
                1 => pose.accessory = Accessory::Scarf,
                2 => pose.accessory = Accessory::Crown,
                3 => pose.accessory = Accessory::Goggles,
                _ => {}
            }
            if let Some((_, v)) = self.look.filter(|l| l.0 == i) {
                apply_variant(&mut pose, r.character, v);
            }
            pose.pos = pos;
            pose.yaw = yaw;
            pose.visual_yaw = lerp(off0, off1, a);
            pose.lean = f.lean;
            pose.pitch = f.pitch;
            pose.wheel = f.wheel;
            pose.drift_tier = if r.drift.active { r.drift.tier } else { 0 };
            pose.trumping = r.trumping();
            pose.gait_phase = gait;
            pose.speed = lerp(p.speed, r.speed, a);
            pose.airborne = !r.on_ground;
            pose.tuck = f.tuck;
            pose.bash = lerp(p.bash_anim, r.bash_anim, a);
            pose.look_back = if r.laser.firing || r.mega_t > 0.0 { 0.0 } else { lerp(p.look_back, r.look_back, a) };
            pose.laser_on = r.laser.firing && r.stun_t <= 0.0;
            pose.trick_flip = if r.trick_t > 0.0 { (1.0 - r.trick_t / 0.6).clamp(0.0, 1.0) } else { 0.0 };
            pose.hide_head = hide_head == Some(i);
            if hidden(r) {
                pose.scale = 0.001;
            }
            out.push(pose);
        }
        super::battle_fx::adjust_poses(v, &mut out, self.time);
        out
    }

    /// Everything but the ostriches' own motion: the scene to render.
    #[allow(clippy::too_many_arguments)]
    pub fn scene(&mut self, v: &RaceView, local: usize, poses: Vec<OstrichPose>, events: &[Event], camera: Camera, shake: Vec3, mirror: Option<MirrorView>, fog_scale: f32) -> FrameScene {
        let t = self.time;
        let near = |p: Vec3, r: f32| (p - camera.eye).length_squared() < r * r;
        let theme = &v.track.theme;
        let mut sc = FrameScene { camera, time: t, ostriches: poses, mirror, shake, fog_scale, ..Default::default() };
        // Start gantry: five red lights come on one by one through the countdown, green at GO, then dark.
        sc.start_lights = match v.phase {
            core::RacePhase::Countdown => (5.0 * (1.0 - v.countdown_left / v.tun.countdown.max(0.5))).ceil().clamp(0.0, 5.0),
            core::RacePhase::Racing if v.race_time < 1.6 => 6.0,
            _ => 0.0,
        };

        // A rival passing right beside the lens would fill the view with its neck: shrink it to the
        // ground as it comes within ~2.6 m of the camera (gone at 1.3 m).
        // (First person: the eye is the player's own head, so only a rival practically inside it goes.)
        let (gone, full) = if sc.ostriches.get(local).is_some_and(|p| p.hide_head) { (0.6, 1.4) } else { (1.3, 2.6) };
        for (i, p) in sc.ostriches.iter_mut().enumerate() {
            if i != local {
                let d = Vec3::new(p.pos.x - camera.eye.x, 0.0, p.pos.z - camera.eye.z).length();
                let k = ((d - gone) / (full - gone)).clamp(0.0, 1.0);
                p.scale = (p.scale * k * k * (3.0 - 2.0 * k)).max(0.001);
            }
        }

        // Pickups.
        for (i, c) in v.coins.iter().enumerate() {
            if c.alive && near(c.pos, 170.0) {
                sc.coins.push(CoinInst { pos: c.pos, spin: t * 3.0 + i as f32 * 0.4, visible: true });
            }
        }
        for (i, c) in v.loose_coins.iter().enumerate() {
            sc.coins.push(CoinInst { pos: c.pos, spin: t * 4.0 + i as f32 * 0.9, visible: true });
        }
        for tg in v.targets.iter().filter(|t| near(t.pos, 220.0)) {
            sc.targets.push(TargetInst { pos: tg.pos, alive: tg.alive, hit_flash: 0.0 });
        }
        // Eggs (extrapolated back from the latest tick so they move smoothly) and cacti.
        for p in v.projectiles {
            let pos = p.pos - p.vel * (1.0 - v.alpha) * DT;
            let yaw = if p.vel.x.abs() + p.vel.z.abs() > 0.1 { p.vel.x.atan2(p.vel.z) } else { 0.0 };
            let kind = match p.kind {
                core::ProjectileKind::RollingEgg => ProjectileKind::Egg,
                core::ProjectileKind::HunterEgg => ProjectileKind::HunterEgg,
                core::ProjectileKind::DollarBill => ProjectileKind::DollarBill,
            };
            sc.projectiles.push(Projectile { kind, pos, yaw });
        }
        for h in v.hazards {
            sc.hazards.push(Hazard { pos: h.pos });
        }

        // Per-racer effects.
        let dust_col = theme.shoulder;
        for (i, r) in v.racers.iter().enumerate() {
            let pose = sc.ostriches[i];
            if hidden(r) {
                sc.emitters.push(Emitter { racer: i, dust: 0.0, dust_color: dust_col, drift_tier: 0, drifting: false, wheel: 0.0, offroad: false, boost: false, burning: false });
                continue;
            }
            // Generous dust: the sim's value, lifted a little at speed.
            // No dust storms when pinned against a wall or standing still.
            let moving = ((r.speed.abs() - 2.0) / 8.0).clamp(0.0, 1.0);
            // Far-away racers kick up less (particle budget: the GPU is fill-rate bound on phones).
            let d = (pose.pos - camera.eye).length();
            let far = if i == local { 1.0 } else { (1.0 - ((d - 14.0) / 32.0).clamp(0.0, 1.0)) * 0.55 };
            let dust = (r.dust * 1.25 + (r.speed.abs() / 36.0).clamp(0.0, 1.0) * 0.12).min(1.6) * (0.2 + 0.8 * moving) * far;
            sc.emitters.push(Emitter {
                racer: i,
                dust,
                // The wheel spray is lifted at night, where the lit dust would sink into the dark road.
                dust_color: if r.offroad { theme.ground } else if theme.night && pose.wheel > 0.05 { dust_col.map(|c| c * 1.35) } else { dust_col },
                drift_tier: if r.drift.active { r.drift.tier } else { 0 },
                drifting: r.drift.active,
                wheel: pose.wheel,
                offroad: r.offroad,
                boost: r.boost_t > 0.0,
                burning: r.effects.burn > 0.0,
            });
            let eyes = rr::eye_positions(&pose);
            let mid = (eyes[0] + eyes[1]) * 0.5;
            if r.mega_t > 0.0 {
                let dir = forward(r.yaw);
                let pulse = 1.0 + 0.15 * (t * 40.0).sin();
                // First person: the beam starts well ahead of the lens and is slimmer, so it does not white out the view.
                let (lead, slim) = if pose.hide_head { (5.0, 0.4) } else { (0.0, 1.0) };
                sc.beams.push(Beam { from: mid + dir * lead, to: mid + dir * (r.laser.hit - r.laser.origin).length(), width: v.tun.mega_width * 0.55 * slim, color: [1.0, 0.25, 0.2], intensity: 1.5 * pulse });
            } else if r.laser.firing && r.stun_t <= 0.0 {
                let to = r.laser.hit;
                let pulse = 1.0 + 0.2 * (t * 55.0 + i as f32).sin();
                for e in eyes {
                    if (to - e).length() > 0.6 && to.is_finite() {
                        // First person: start the beam a few metres ahead of the lens.
                        let (from, width) = if pose.hide_head && (to - e).length() > 4.0 { (e + (to - e).normalize() * 2.5, 0.05) } else { (e, 0.09) };
                        sc.beams.push(Beam { from, to, width, color: [1.0, 0.1, 0.08], intensity: pulse });
                    }
                }
                let _ = LaserHit::None;
            }
            if r.shield_t > 0.0 || r.effects.pill > 0.0 {
                let aura = r.effects.pill > 0.0;
                let left = if aura { r.effects.pill } else { r.shield_t };
                let flicker = if left < 1.5 { 0.5 + 0.5 * (t * 14.0).sin().abs() } else { 1.0 };
                let s = pose.scale;
                sc.shields.push(Shield { pos: pose.pos + Vec3::Y * 1.6 * s, radius: 2.0 * s, alpha: (if aura { 0.7 } else { 0.45 }) * flicker, aura });
            }
        }

        // One-shot bursts from events.
        for e in events {
            match *e {
                Event::TargetDestroyed { target, .. } => {
                    if let Some(tg) = v.targets.get(target as usize) {
                        sc.bursts.push(Burst { kind: BurstKind::TargetShatter, pos: tg.pos, count: 26 });
                    }
                }
                Event::CoinPicked { racer, .. } => {
                    if let Some(p) = sc.ostriches.get(racer as usize) {
                        sc.bursts.push(Burst { kind: BurstKind::CoinSparkle, pos: p.pos + Vec3::Y * 1.2, count: 10 });
                    }
                }
                Event::Landed { racer, hard } => {
                    if let Some(p) = sc.ostriches.get(racer as usize) {
                        sc.bursts.push(Burst { kind: BurstKind::LandingDust, pos: p.pos, count: if hard { 30 } else { 14 } });
                    }
                }
                Event::Hit { victim, cause, .. } => {
                    if let Some(p) = sc.ostriches.get(victim as usize) {
                        let kind = if cause == HitCause::Lightning { BurstKind::Lightning } else { BurstKind::HitPuff };
                        sc.bursts.push(Burst { kind, pos: p.pos + Vec3::Y * 1.3, count: 22 });
                        if cause == HitCause::RugPull {
                            // The rug is yanked out from under it: a puff of dust at the feet.
                            sc.bursts.push(Burst { kind: BurstKind::LandingDust, pos: p.pos, count: 36 });
                        }
                    }
                }
                Event::CoinsTaken { from, to, count } => {
                    if let Some(p) = sc.ostriches.get(from as usize) {
                        let n = (count as usize).min(MAX_FLYING_COINS);
                        for k in 0..n {
                            self.coin_flights.push(FlyingCoin {
                                from: p.pos + Vec3::Y * 1.2,
                                to: to as usize,
                                start: t,
                                delay: COIN_STAGGER * k as f32 / n.max(1) as f32,
                                arrived: false,
                            });
                        }
                    }
                }
                Event::Bash { victim, .. } => {
                    if let Some(p) = sc.ostriches.get(victim as usize) {
                        sc.bursts.push(Burst { kind: BurstKind::HitPuff, pos: p.pos + Vec3::Y * 1.8, count: 18 });
                    }
                }
                Event::Eliminated { pos, .. } => {
                    sc.bursts.push(Burst { kind: BurstKind::HitPuff, pos: Vec3::from_array(pos) + Vec3::Y * 1.4, count: 40 });
                }
                Event::ShieldBlock { racer, .. } => {
                    if let Some(p) = sc.ostriches.get(racer as usize) {
                        sc.bursts.push(Burst { kind: BurstKind::HitPuff, pos: p.pos + Vec3::Y * 1.4, count: 14 });
                    }
                }
                _ => {}
            }
        }
        // Coins flying from a rugged racer to the rugger, homing on where it is now.
        let flight = COIN_FLIGHT_SECS - COIN_STAGGER;
        self.coin_flights.retain(|c| t - c.start - c.delay < flight);
        for (k, c) in self.coin_flights.iter_mut().enumerate() {
            let age = t - c.start - c.delay;
            if age < 0.0 {
                continue;
            }
            let Some(dest) = sc.ostriches.get(c.to).map(|p| p.pos + Vec3::Y * 1.4) else {
                continue;
            };
            let u = (age / flight).clamp(0.0, 1.0);
            let ease = u * u * (3.0 - 2.0 * u);
            let pos = c.from.lerp(dest, ease) + Vec3::Y * (4.0 * u * (1.0 - u) * 1.8);
            sc.coins.push(CoinInst { pos, spin: t * 12.0 + k as f32, visible: true });
            if u > 0.92 && !c.arrived {
                c.arrived = true;
                sc.bursts.push(Burst { kind: BurstKind::CoinSparkle, pos: dest, count: 4 });
            }
        }
        // The egg layer is presentation only: no racer slot or collider.
        let mut row = 0u16;
        for feature in &v.track.features {
            if let core::Feature::TargetRow { s, .. } = feature {
                if let Some(phase) = core::playful::laying_phase(v.race_time, row) {
                    let sample = v.track.sample_at(*s);
                    let lat = (phase * 2.0 - 1.0) * (sample.width * 0.5 + 2.0);
                    let pos = v.track.point_at(*s, lat);
                    if near(pos, 180.0) {
                        let mut npc = character_pose(3);
                        npc.jockey = false;
                        npc.pos = pos;
                        npc.yaw = core::math::yaw_of(sample.right);
                        npc.speed = (sample.width + 4.0) / 1.6;
                        npc.gait_phase = (v.race_time * 4.0).fract();
                        npc.scale = 0.9;
                        sc.ostriches.push(npc);
                    }
                }
                row += 1;
            }
        }
        sc.rugs = v
            .play
            .rugs
            .iter()
            .enumerate()
            .filter_map(|(k, rug)| {
                let (pos, yaw, width) = core::mischief::rug_site(v.track, rug);
                let sample = v.track.sample_at(rug.s);
                near(pos, 180.0).then_some(rr::RugPose {
                    pos,
                    yaw,
                    up: sample.up,
                    slope: sample.tangent.y,
                    width,
                    extension: core::mischief::rug_extension(v.race_time, k, rug),
                    pulling: rug.pull > 0,
                    side: if k % 2 == 0 { 1.0 } else { -1.0 },
                })
            })
            .collect();
        sc.roses = v
            .play
            .roses
            .iter()
            .filter(|r| near(r.pos, 180.0))
            .map(|r| rr::RosePose {
                pos: r.pos,
                yaw: r.yaw,
                // The seller runs at `side` of its customer, so the customer is
                // the other way: local +x is the seller's left (-right).
                toward: r.side as f32,
                selling: r.selling(),
                leave: if r.leaving() {
                    (1.0 - r.t / core::playful::ROSE_LEAVE).clamp(0.0, 1.0)
                } else {
                    0.0
                },
            })
            .collect();
        sc.frogs = v
            .play
            .frogs

            .iter()
            .filter(|f| near(f.pos, 180.0))
            .map(|f| rr::FrogPose {
                pos: f.pos,
                yaw: f.yaw,
                tumble: if f.tumble > 0.0 {
                    (1.0 - f.tumble / v.tun.playful.tumble_time).clamp(0.001, 0.999)
                } else {
                    0.0
                },
            })
            .collect();
        sc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::source::{LocalRace, RaceSource};
    use race_core::{BotLevel, Input, RaceConfig, TrackId};

    #[test]
    fn every_knock_off_cause_has_its_own_pose() {
        let causes = [HitCause::MoneyPrinter, HitCause::RugPull, HitCause::Pepe, HitCause::LaserTrump];
        let styles: Vec<_> = causes.iter().map(|c| knock_style(core::KnockKind::from_cause(*c))).collect();
        for (i, a) in styles.iter().enumerate() {
            assert_ne!(*a, rr::KnockStyle::None, "{:?} must pose", causes[i]);
            for (j, b) in styles.iter().enumerate().skip(i + 1) {
                assert_ne!(a, b, "{:?} and {:?} share a pose", causes[i], causes[j]);
            }
        }
        assert_eq!(styles.len(), rr::KnockStyle::ALL.len());
        for k in core::KnockKind::ALL {
            assert!(rr::KnockStyle::ALL.contains(&knock_style(k)));
        }
        for c in [HitCause::RollingEgg, HitCause::HunterEgg, HitCause::Cactus, HitCause::Lightning, HitCause::MegaLaser, HitCause::Bash, HitCause::Laser, HitCause::Hurdle] {
            assert_eq!(knock_style(core::KnockKind::from_cause(c)), rr::KnockStyle::None, "{c:?} keeps its own treatment");
        }
    }

    #[test]
    fn a_knocked_off_racer_is_posed_by_cause_and_a_trump_smash_by_its_spin() {
        let mut race = LocalRace::new(RaceConfig::quick(TrackId::Dune, 7, BotLevel::Normal, 3), Some(0));
        race.advance(1.0 / 60.0, &mut Input::go);
        let mut racers = race.view().racers.to_vec();
        racers[2].effects.dismount = core::mischief::DISMOUNT_TIME - 0.1;
        racers[2].effects.knock = core::KnockKind::Rug;
        racers[2].effects.chase_x = -3.0;
        racers[3].effects.knock = core::KnockKind::Trump;
        racers[3].stun_t = 0.6;
        racers[4].effects.knock = core::KnockKind::Pepe; // seated and not spun: no pose
        let base = race.view();
        let v = RaceView { racers: &racers, prev: &racers, phase: core::RacePhase::Racing, ..base };
        let poses = Presenter::new().poses(&v, 1.0 / 60.0, None);
        assert_eq!(poses[2].knock, rr::KnockStyle::Rug);
        assert!((poses[2].knock_age - 0.1).abs() < 1e-3);
        assert_eq!(poses[3].knock, rr::KnockStyle::Trump);
        assert!((poses[3].knock_age - 0.4).abs() < 1e-3);
        assert_eq!(poses[4].knock, rr::KnockStyle::None);
        assert_eq!(poses[0].knock, rr::KnockStyle::None);
    }

    #[test]
    fn the_throw_lasts_as_long_in_the_renderer_as_in_the_sim() {
        assert_eq!(rr::THROW_SECS, core::mischief::DISMOUNT_FALL);
        assert_eq!(rr::SMASH_SECS, core::Tunables::default().egg_spin);
    }

    #[test]
    fn a_rugged_racers_coins_fly_to_the_rugger_over_about_a_second_and_then_stop() {
        let mut race = LocalRace::new(RaceConfig::quick(TrackId::Dune, 7, BotLevel::Normal, 3), Some(0));
        race.advance(1.0 / 60.0, &mut Input::go);
        let mut pr = Presenter::new();
        let frame = |pr: &mut Presenter, events: &[Event]| {
            let v = race.view();
            let poses = pr.poses(&v, 1.0 / 60.0, None);
            let sc = pr.scene(&v, 0, poses, events, Camera::default(), Vec3::ZERO, None, 1.0);
            (sc.coins.len(), sc.ostriches[0].pos, sc.bursts.iter().filter(|b| b.kind == BurstKind::CoinSparkle).count())
        };
        let (base, _, _) = frame(&mut pr, &[]);
        let (first, _, _) = frame(&mut pr, &[Event::CoinsTaken { from: 1, to: 0, count: 6 }]);
        assert!(first > base, "coins appear at once ({first} vs {base})");
        let mut flying = 0;
        let mut sparkles = 0;
        let mut mid_seen = false;
        for k in 0..70 {
            let (n, _, s) = frame(&mut pr, &[]);
            sparkles += s;
            if k == 20 {
                mid_seen = n > base;
            }
            flying = n.saturating_sub(base);
        }
        assert!(mid_seen, "still in the air after a third of a second");
        assert_eq!(flying, 0, "all landed within about a second (1.2 s later)");
        assert!(sparkles > 0, "a sparkle where they arrive");
    }

    #[test]
    fn poses_cover_every_racer_and_stay_finite() {
        let mut race = LocalRace::new(RaceConfig::quick(TrackId::Dune, 7, BotLevel::Normal, 3), Some(0));
        let mut pr = Presenter::new();
        for _ in 0..240 {
            race.advance(1.0 / 60.0, &mut Input::go);
            let ev = race.drain_events();
            let v = race.view();
            let poses = pr.poses(&v, 1.0 / 60.0, None);
            assert_eq!(poses.len(), 8);
            let sc = pr.scene(&v, 0, poses, &ev, Camera::default(), Vec3::ZERO, None, 1.0);
            assert_eq!(sc.emitters.len(), 8);
            assert!(sc.ostriches.iter().all(|p| p.pos.is_finite() && p.yaw.is_finite() && p.lean.is_finite()));
        }
    }

    #[test]
    fn drifting_racer_legs_ease_into_the_wheel_and_back() {
        let mut race = LocalRace::new(RaceConfig::quick(TrackId::Dune, 7, BotLevel::Normal, 3), Some(0));
        race.advance(1.0 / 60.0, &mut Input::go);
        let mut drifting = race.view().racers.to_vec();
        drifting[2].drift.active = true;
        drifting[2].drift.tier = 2;
        let mut trumping = drifting.clone();
        trumping[3].effects.trump = 2.0;
        let mut pr = Presenter::new();
        let mut run = |racers: &[Racer], frames: usize| {
            let base = race.view();
            let v = RaceView { racers, prev: racers, ..base };
            let mut last = Vec::new();
            for _ in 0..frames {
                last = pr.poses(&v, 1.0 / 60.0, None);
            }
            last
        };
        // Starts at 0, is part-way after a couple of frames, and full after 0.12 s.
        let idle = run(&race.view().racers.to_vec(), 1);
        assert_eq!(idle[2].wheel, 0.0);
        let early = run(&drifting, 3);
        assert!(early[2].wheel > 0.2 && early[2].wheel < 0.9, "{}", early[2].wheel);
        let full = run(&drifting, 10);
        assert_eq!(full[2].wheel, 1.0);
        assert_eq!(full[2].drift_tier, 2);
        assert_eq!(full[0].wheel, 0.0);
        // Eases back over a quarter of a second.
        let out = run(&race.view().racers.to_vec(), 5);
        assert!(out[2].wheel > 0.3 && out[2].wheel < 1.0, "{}", out[2].wheel);
        assert_eq!(run(&race.view().racers.to_vec(), 20)[2].wheel, 0.0);
        assert_eq!(run(&race.view().racers.to_vec(), 1)[2].drift_tier, 0);
        // Trumping is a full wheel at once, with the pose flag and the emitter.
        let t = run(&trumping, 1);
        assert!(t[3].trumping && t[3].wheel == 1.0 && !t[0].trumping);
    }

    #[test]
    fn visual_yaw_is_an_offset() {
        let mut race = LocalRace::new(RaceConfig::quick(TrackId::Dune, 1, BotLevel::Normal, 3), Some(0));
        race.advance(1.0 / 60.0, &mut Input::go);
        let v = race.view();
        let poses = Presenter::new().poses(&v, 1.0 / 60.0, None);
        assert!(poses[0].visual_yaw.abs() < 0.5, "{}", poses[0].visual_yaw);
    }
}
