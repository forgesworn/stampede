//! Headless showcase: builds a synthetic closed track (hill, ramp, boost pads,
//! heightfield, scenery for a day savanna and a night neon palette), poses
//! four ostriches and writes PNGs from several cameras.
//!
//! cargo run -p race-render --example showcase --release -- <out_dir> [only_shot_substring]
//! Env: SHOT_W / SHOT_H (default 1280x576), WGPU_BACKEND=gl to force GLES.

use glam::{Mat4, Vec2, Vec3};
use race_render::capture::{Offscreen, SHOT_FORMAT};
use race_render::*;
use std::f32::consts::{PI, TAU};
use std::path::Path;

// ------------------------------------------------------------ the track ----

struct Track {
    samples: Vec<RoadSample>,
}

fn track_point(t: f32) -> Vec2 {
    // A closed, non-self-intersecting loop with a long straight on the +Z side.
    let a = 150.0 + 28.0 * (2.0 * t + 0.7).cos() + 10.0 * (3.0 * t).sin();
    let b = 95.0 + 22.0 * (3.0 * t + 1.9).cos();
    Vec2::new(a * t.cos(), b * t.sin())
}

fn build_track(night: bool) -> Track {
    // Dense polyline, then resample every metre.
    let n = 6000;
    let pts: Vec<Vec2> = (0..n).map(|i| track_point(TAU * i as f32 / n as f32)).collect();
    let mut cum = vec![0.0f32];
    for i in 1..=n {
        cum.push(cum[i - 1] + (pts[i % n] - pts[i - 1]).length());
    }
    let total = cum[n];
    let count = total.floor() as usize;
    let step = total / count as f32;
    let mut xy = Vec::with_capacity(count);
    let mut j = 0;
    for k in 0..count {
        let d = k as f32 * step;
        while cum[j + 1] < d {
            j += 1;
        }
        let f = (d - cum[j]) / (cum[j + 1] - cum[j]).max(1e-6);
        xy.push(pts[j].lerp(pts[(j + 1) % n], f));
    }
    // Heights: a hill and a ramp.
    let hill_c = count as f32 * 0.30;
    let ramp_s = count as f32 * 0.52;
    let height = |s: f32| -> f32 {
        let mut y = 0.0;
        let d = (s - hill_c).abs();
        if d < 80.0 {
            y += 5.5 * (0.5 + 0.5 * (PI * d / 80.0).cos());
        }
        let r = s - ramp_s;
        if (0.0..14.0).contains(&r) {
            y += 1.9 * r / 14.0;
        }
        y
    };
    let ys: Vec<f32> = (0..count).map(|k| height(k as f32 * step)).collect();
    let mut samples = Vec::with_capacity(count);
    for k in 0..count {
        let p = Vec3::new(xy[k].x, ys[k], xy[k].y);
        let pn = Vec3::new(xy[(k + 1) % count].x, ys[(k + 1) % count], xy[(k + 1) % count].y);
        let pp = Vec3::new(xy[(k + count - 1) % count].x, ys[(k + count - 1) % count], xy[(k + count - 1) % count].y);
        let fwd = (pn - pp).normalize();
        let right = fwd.cross(Vec3::Y).normalize();
        let up = right.cross(fwd).normalize();
        let mut flags = 0;
        // Curbs on the tighter corners.
        let curv = {
            let a = (pn - p).normalize();
            let b = (p - pp).normalize();
            a.cross(b).y
        };
        if curv.abs() > 0.012 {
            flags |= road_flags::CURB;
        }
        if night && (k as f32 - ramp_s - 20.0).abs() < 7.0 {
            flags |= road_flags::GAP;
        }
        let width = if night { 15.0 } else { 18.0 };
        samples.push(RoadSample { pos: p, right, up, width, shoulder_l: 5.0, shoulder_r: 5.0, flags });
    }
    Track { samples }
}

fn track_yaw(s: &RoadSample) -> f32 {
    let f = s.up.cross(s.right);
    f.x.atan2(f.z)
}

fn nearest_road(track: &Track, x: f32, z: f32) -> (f32, usize) {
    let mut best = (f32::MAX, 0);
    for (i, s) in track.samples.iter().enumerate().step_by(2) {
        let d = (s.pos.x - x).powi(2) + (s.pos.z - z).powi(2);
        if d < best.0 {
            best = (d, i);
        }
    }
    (best.0.sqrt(), best.1)
}

fn hash(a: i32, b: i32) -> f32 {
    let mut h = (a as u32).wrapping_mul(374761393) ^ (b as u32).wrapping_mul(668265263);
    h = (h ^ (h >> 13)).wrapping_mul(1274126177);
    ((h ^ (h >> 16)) & 0xffffff) as f32 / 16777216.0
}

fn vnoise(x: f32, z: f32) -> f32 {
    let (xi, zi) = (x.floor(), z.floor());
    let (fx, fz) = (x - xi, z - zi);
    let (sx, sz) = (fx * fx * (3.0 - 2.0 * fx), fz * fz * (3.0 - 2.0 * fz));
    let h = |dx: i32, dz: i32| hash(xi as i32 + dx, zi as i32 + dz);
    let a = h(0, 0) + (h(1, 0) - h(0, 0)) * sx;
    let b = h(0, 1) + (h(1, 1) - h(0, 1)) * sx;
    a + (b - a) * sz
}

fn terrain_height(track: &Track, x: f32, z: f32, night: bool) -> f32 {
    let (d, i) = nearest_road(track, x, z);
    let road_y = track.samples[i].pos.y;
    let amp = if night { 2.5 } else { 7.0 };
    let n = (vnoise(x * 0.018, z * 0.018) - 0.5) * 2.0 * amp + (vnoise(x * 0.07, z * 0.07) - 0.5) * 1.2;
    let rise = 3.5 + 0.015 * (d - 30.0).max(0.0) * if night { 0.2 } else { 1.0 };
    let w = ((d - 24.0) / 50.0).clamp(0.0, 1.0);
    let w = w * w * (3.0 - 2.0 * w);
    let mut h = road_y + (n + rise * 0.0) * w + w * 0.0;
    // Trench under the gap so the jump reads.
    if night && track.samples[i].flags & road_flags::GAP != 0 && d < 12.0 {
        h = road_y - 8.0 + d * 0.3;
    }
    h
}

fn build_world(night: bool, wall: WallStyle, palette: Palette) -> WorldGeometry {
    let track = build_track(night);
    let (cell, w, h) = (6.0, 108, 84);
    let origin = Vec2::new(-324.0, -252.0);
    let mut heights = Vec::with_capacity(w * h);
    for zi in 0..h {
        for xi in 0..w {
            heights.push(terrain_height(&track, origin.x + xi as f32 * cell, origin.y + zi as f32 * cell, night));
        }
    }
    // Scenery.
    let mut scenery = Vec::new();
    let mut k = 0;
    let mut tries = 0;
    while k < 420 && tries < 4000 {
        tries += 1;
        let x = origin.x + hash(tries, 1) * (w as f32 - 2.0) * cell + cell;
        let z = origin.y + hash(tries, 2) * (h as f32 - 2.0) * cell + cell;
        let (d, _) = nearest_road(&track, x, z);
        let kind_r = hash(tries, 3);
        let y = terrain_height(&track, x, z, night);
        let (kind, min_d, scale) = if night {
            match kind_r {
                r if r < 0.35 => (SceneryKind::Rock, 22.0, 0.8 + hash(tries, 4)),
                r if r < 0.55 => (SceneryKind::Cactus, 22.0, 0.8 + 0.6 * hash(tries, 4)),
                r if r < 0.62 => (SceneryKind::Mesa, 95.0, 0.9 + 0.6 * hash(tries, 4)),
                r if r < 0.85 => (SceneryKind::GrassTuft, 22.0, 0.9),
                _ => (SceneryKind::Bush, 22.0, 0.9),
            }
        } else {
            match kind_r {
                r if r < 0.22 => (SceneryKind::Acacia, 24.0, 0.8 + 0.7 * hash(tries, 4)),
                r if r < 0.38 => (SceneryKind::Rock, 22.0, 0.7 + hash(tries, 4)),
                r if r < 0.50 => (SceneryKind::Bush, 22.0, 0.8 + 0.5 * hash(tries, 4)),
                r if r < 0.56 => (SceneryKind::Cactus, 24.0, 0.8 + 0.5 * hash(tries, 4)),
                r if r < 0.60 => (SceneryKind::Mesa, 110.0, 0.8 + 0.7 * hash(tries, 4)),
                r if r < 0.64 => (SceneryKind::Dune, 60.0, 0.7 + 0.5 * hash(tries, 4)),
                _ => (SceneryKind::GrassTuft, 21.0, 0.9 + 0.5 * hash(tries, 4)),
            }
        };
        if d < min_d {
            continue;
        }
        k += 1;
        scenery.push(Scenery { kind, pos: Vec3::new(x, y, z), yaw: hash(tries, 5) * TAU, scale });
    }
    if night {
        // Neon pylons along both sides.
        for i in (0..track.samples.len()).step_by(26) {
            let s = &track.samples[i];
            if s.flags & road_flags::GAP != 0 {
                continue;
            }
            for side in [-1.0f32, 1.0] {
                let p = s.pos + s.right * side * (s.width * 0.5 + s.shoulder_l + 5.0);
                scenery.push(Scenery { kind: SceneryKind::NeonPylon, pos: Vec3::new(p.x, s.pos.y, p.z), yaw: 0.0, scale: 1.0 });
            }
        }
    } else {
        // A fence line on the far side of one straight.
        for i in (0..track.samples.len()).step_by(12).take(60) {
            let s = &track.samples[i];
            let p = s.pos + s.right * (s.width * 0.5 + s.shoulder_l + 14.0);
            scenery.push(Scenery { kind: SceneryKind::Fence, pos: Vec3::new(p.x, terrain_height(&track, p.x, p.z, false), p.z), yaw: track_yaw(s) + PI * 0.5, scale: 1.0 });
        }
    }
    let n = track.samples.len();
    let pad_at = |i: usize| {
        let s = &track.samples[i % n];
        BoostPad { pos: s.pos, yaw: track_yaw(s), length: 9.0, width: 6.0 }
    };
    let pads = vec![pad_at(n / 10), pad_at(n * 62 / 100), pad_at(n * 9 / 10)];
    let s0 = &track.samples[0];
    let start = Some(StartLine { pos: s0.pos, right: s0.right, width: s0.width });
    WorldGeometry {
        road: track.samples,
        closed: true,
        terrain: Some(Terrain { origin, cell, w, h, heights }),
        wall,
        palette,
        scenery,
        pads,
        start,
        fx: WorldFx::default(),
        bridges: Vec::new(),
        field: None,
    }
}

// --------------------------------------------------------------- racers ----

struct Chars;
impl Chars {
    fn pose(i: usize) -> OstrichPose {
        let base = OstrichPose::default();
        match i % 8 {
            0 => OstrichPose { body: [0.82, 0.66, 0.42], neck: [0.93, 0.68, 0.62], legs: [0.93, 0.68, 0.62], beak: [0.98, 0.72, 0.25], accent: [0.86, 0.12, 0.1], accessory: Accessory::Scarf, ..base },
            1 => OstrichPose { body: [0.10, 0.10, 0.13], neck: [0.96, 0.42, 0.08], legs: [0.9, 0.55, 0.3], beak: [1.0, 0.78, 0.2], accent: [1.0, 0.5, 0.1], accessory: Accessory::Goggles, ..base },
            2 => OstrichPose { body: [0.97, 0.58, 0.10], neck: [0.98, 0.9, 0.82], legs: [0.98, 0.9, 0.82], beak: [0.95, 0.6, 0.2], accent: [1.0, 0.98, 0.95], accessory: Accessory::Crown, ..base },
            3 => OstrichPose { body: [0.85, 0.18, 0.62], neck: [0.98, 0.7, 0.8], legs: [0.98, 0.7, 0.8], beak: [1.0, 0.8, 0.3], accent: [1.0, 0.85, 0.2], accessory: Accessory::StarHeadband, ..base },
            4 => OstrichPose { body: [0.34, 0.40, 0.50], neck: [0.75, 0.62, 0.6], legs: [0.75, 0.62, 0.6], beak: [0.9, 0.75, 0.4], accent: [0.8, 0.82, 0.88], accessory: Accessory::VikingHelmet, scale: 1.12, ..base },
            5 => OstrichPose { body: [0.08, 0.68, 0.62], neck: [0.9, 0.75, 0.7], legs: [0.9, 0.75, 0.7], beak: [1.0, 0.7, 0.2], accent: [1.0, 0.25, 0.6], accessory: Accessory::Mohawk, scale: 0.94, ..base },
            6 => OstrichPose { body: [0.30, 0.14, 0.50], neck: [0.8, 0.6, 0.7], legs: [0.8, 0.6, 0.7], beak: [1.0, 0.8, 0.25], accent: [1.0, 0.8, 0.2], accessory: Accessory::ChainShades, scale: 1.1, ..base },
            _ => OstrichPose { body: [0.96, 0.92, 0.80], neck: [0.98, 0.78, 0.74], legs: [0.98, 0.78, 0.74], beak: [0.98, 0.72, 0.28], accent: [0.55, 0.78, 1.0], accessory: Accessory::Cap, ..base },
        }
    }
}

fn at_s(track_samples: &[RoadSample], s: f32, lateral: f32) -> (Vec3, f32, &RoadSample) {
    let n = track_samples.len();
    let i = (s.rem_euclid(n as f32)) as usize % n;
    let smp = &track_samples[i];
    (smp.pos + smp.right * lateral, track_yaw(smp), smp)
}

#[derive(Clone, Copy)]
enum Role {
    Run,
    Drift,
    Jump,
    Laser,
    Trump,
}

struct Racer {
    ch: usize,
    s0: f32,
    lateral: f32,
    speed: f32,
    role: Role,
}

struct Sim {
    samples: Vec<RoadSample>,
    racers: Vec<Racer>,
    t: f32,
}

impl Sim {
    fn poses(&self) -> (Vec<OstrichPose>, Vec<Emitter>) {
        let mut poses = Vec::new();
        let mut ems = Vec::new();
        for (i, r) in self.racers.iter().enumerate() {
            let s = r.s0 + r.speed * self.t;
            let (mut pos, yaw, smp) = at_s(&self.samples, s, r.lateral);
            let mut p = Chars::pose(r.ch);
            p.speed = r.speed;
            p.gait_phase = s / stride_len(r.speed);
            p.yaw = yaw;
            let mut em = Emitter { racer: i, dust: (r.speed / 32.0) * 0.3, dust_color: [0.85, 0.74, 0.55], drift_tier: 0, drifting: false, wheel: 0.0, offroad: false, boost: false, burning: false };
            match r.role {
                Role::Run => {
                    p.lean = 0.05;
                }
                Role::Drift => {
                    p.visual_yaw = 0.42;
                    p.lean = 0.35;
                    em.dust = 1.2;
                    em.drifting = true;
                    em.drift_tier = 2;
                    em.wheel = 1.0;
                    p.wheel = 1.0;
                    p.drift_tier = 2;
                    pos += smp.right * -1.5;
                }
                Role::Jump => {
                    p.airborne = true;
                    p.tuck = 0.9;
                    pos.y += 1.6;
                    p.pitch = -0.1;
                    em.dust = 0.0;
                }
                Role::Trump => {
                    p.trumping = true;
                    p.wheel = 1.0;
                    em.wheel = 1.0;
                    em.dust = 1.0;
                }
                Role::Laser => {
                    p.laser_on = true;
                    em.boost = true;
                    em.dust = 0.5;
                }
            }
            p.pos = pos;
            poses.push(p);
            ems.push(em);
        }
        (poses, ems)
    }
}

/// Metres of ground per gait cycle (two steps): ~1.8 m at a jog up to ~6 m at top speed,
/// the way the sim will advance `gait_phase`.
fn stride_len(speed: f32) -> f32 {
    let t = ((speed - 5.0) / 27.0).clamp(0.0, 1.0);
    1.8 + (6.0 - 1.8) * t
}

fn base_scene(sim: &Sim, time: f32) -> FrameScene {
    let (poses, ems) = sim.poses();
    let mut sc = FrameScene { time, ostriches: poses, emitters: ems, ..Default::default() };
    // Everything is placed relative to the player so it stays in view.
    let sref = sim.racers[0].s0 + sim.racers[0].speed * sim.t;
    for k in 0..30 {
        let s = sref + 22.0 + (k % 15) as f32 * 3.2;
        let lat = if k < 15 { -6.8 } else { 6.8 };
        let (p, _, smp) = at_s(&sim.samples, s, lat);
        sc.coins.push(CoinInst { pos: p + smp.up * 1.0, spin: time * 3.0 + k as f32 * 0.4, visible: true });
    }
    let ts = sref + 84.0;
    for (k, lat) in [-5.0f32, 0.0, 5.0].iter().enumerate() {
        let (p, _, smp) = at_s(&sim.samples, ts, *lat);
        sc.targets.push(TargetInst { pos: p + smp.up * 2.0, alive: k != 2, hit_flash: if k == 1 { 0.7 } else { 0.0 } });
        if k == 2 {
            sc.bursts.push(Burst { kind: BurstKind::TargetShatter, pos: p + smp.up * 2.0, count: 24 });
        }
    }
    let laser_idx = sim.racers.iter().position(|r| matches!(r.role, Role::Laser));
    if let Some(i) = laser_idx {
        let eyes = eye_positions(&sc.ostriches[i]);
        let tgt = sc.targets[1].pos;
        for e in eyes {
            sc.beams.push(Beam { from: e, to: tgt, width: 0.1, color: [1.0, 0.1, 0.08], intensity: 1.2 });
        }
    }
    let (p, yaw, smp) = at_s(&sim.samples, sref + 36.0, 6.0);
    sc.projectiles.push(Projectile { kind: ProjectileKind::Egg, pos: p + smp.up * 0.6, yaw });
    let (p2, _, _) = at_s(&sim.samples, sref + 26.0, -5.5);
    sc.hazards.push(Hazard { pos: p2 });
    let (p3, yaw3, smp3) = at_s(&sim.samples, sref + 44.0, -6.0);
    sc.projectiles.push(Projectile { kind: ProjectileKind::HunterEgg, pos: p3 + smp3.up * 0.6, yaw: yaw3 });
    sc
}

fn chase_cam(p: &OstrichPose, back: f32, up: f32, ahead: f32, fov: f32) -> Camera {
    let f = Vec3::new(p.yaw.sin(), 0.0, p.yaw.cos());
    Camera { eye: p.pos - f * back + Vec3::Y * up, target: p.pos + f * ahead + Vec3::Y * 1.0, up: Vec3::Y, fov_y: fov.to_radians(), near: 0.3, far: 700.0 }
}

// ----------------------------------------------------------------- gpu ----

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn headless() -> Gpu {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::all()),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("no adapter");
    let info = adapter.get_info();
    eprintln!("adapter: {} ({:?})", info.name, info.backend);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("showcase"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
        ..Default::default()
    }))
    .expect("device");
    Gpu { device, queue }
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().unwrap();
    wr.write_image_data(rgba).unwrap();
    wr.finish().unwrap();
}

struct Ctx {
    gpu: Gpu,
    r: Renderer,
    off: Offscreen,
    out: std::path::PathBuf,
    only: Option<String>,
    log: Vec<String>,
}

impl Ctx {
    fn frame(&mut self, scene: &FrameScene, dt: f32) {
        let mut enc = self.gpu.device.create_command_encoder(&Default::default());
        self.r.render(&self.gpu.device, &self.gpu.queue, &mut enc, &self.off.view, scene, dt);
        self.gpu.queue.submit(Some(enc.finish()));
    }

    fn shot(&mut self, name: &str, scene: &FrameScene) {
        if let Some(o) = &self.only {
            if !o.split(',').any(|k| name.contains(k)) {
                return;
            }
        }
        self.frame(scene, 1.0 / 60.0);
        let st = self.r.stats();
        let rgba = self.off.read_rgba(&self.gpu.device, &self.gpu.queue).expect("read");
        write_png(&self.out.join(format!("{name}.png")), self.off.size.0, self.off.size.1, &rgba);
        let line = format!("{name:<22} draw_calls={:<4} triangles={:<7} particles={}", st.draw_calls, st.triangles, st.particles);
        println!("{line}");
        self.log.push(line);
    }

    /// Run the particle system forward with the sim moving (no drawing).
    fn warm(&mut self, sim: &mut Sim, secs: f32) {
        let steps = (secs * 60.0) as usize;
        for _ in 0..steps {
            sim.t += 1.0 / 60.0;
            let sc = base_scene(sim, sim.t);
            self.r.step_particles(&sc, 1.0 / 60.0);
        }
    }
}

fn studio_shots(cx: &mut Ctx, day: &WorldGeometry) {
    let w = cx.off.size.0;
    let _ = w;
    // Character lineup on a straight, facing the camera.
    let mut lu = FrameScene { time: 1.0, ..Default::default() };
    let (c0, y0, s0) = at_s(&day.road, 40.0, 0.0);
    let _ = (y0, c0);
    let fwd = s0.up.cross(s0.right);
    for i in 0..8 {
        let mut p = Chars::pose(i);
        let lat = -7.0 + 2.0 * i as f32;
        p.pos = s0.pos + s0.right * lat;
        p.yaw = track_yaw(s0) - 0.45;
        p.gait_phase = 0.1 * i as f32;
        p.speed = 0.0;
        lu.ostriches.push(p);
    }
    lu.camera = Camera { eye: s0.pos + fwd * 17.0 + Vec3::Y * 3.0 + s0.right * 0.0, target: s0.pos + Vec3::Y * 1.6 - fwd * 0.0, up: Vec3::Y, fov_y: 40f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("day_lineup", &lu);
    // Close-ups of heads (accessories).
    for (name, idx) in [("heads_a", [0usize, 1, 2, 3]), ("heads_b", [4, 5, 6, 7])] {
        let mut hs = FrameScene { time: 1.0, ..Default::default() };
        for (k, ci) in idx.iter().enumerate() {
            let mut p = Chars::pose(*ci);
            p.pos = s0.pos + s0.right * (-3.6 + 2.4 * k as f32);
            p.yaw = track_yaw(s0) - 0.6;
            hs.ostriches.push(p);
        }
        hs.camera = Camera { eye: s0.pos + fwd * 8.0 + Vec3::Y * 2.5, target: s0.pos + Vec3::Y * 2.35, up: Vec3::Y, fov_y: 28f32.to_radians(), near: 0.3, far: 700.0 };
        cx.shot(name, &hs);
    }

    // Run cycle sheets, side view: jog 5, sprint 18, top speed 30 and turbo 42 m/s.
    // `cycle` at gait phase 0.05, `cycle_b` a third of a cycle later.
    let face = f32::atan2(s0.right.x, s0.right.z);
    for (name, phase) in [("cycle", 0.05f32), ("cycle_b", 0.38)] {
        let mut cyc = FrameScene { time: 1.0, ..Default::default() };
        for (i, speed) in [5.0f32, 18.0, 30.0, 42.0].into_iter().enumerate() {
            let mut p = Chars::pose(0);
            p.pos = s0.pos + s0.right * (-5.4 + 3.6 * i as f32);
            p.yaw = face;
            p.speed = speed;
            p.gait_phase = phase;
            cyc.ostriches.push(p);
        }
        cyc.camera = Camera { eye: s0.pos + fwd * 15.0 + Vec3::Y * 1.5, target: s0.pos + Vec3::Y * 0.95, up: Vec3::Y, fov_y: 34f32.to_radians(), near: 0.3, far: 700.0 };
        cx.shot(name, &cyc);
    }
    // Neck/head poses: look back, bash left/right, tuck, flip, spin.
    let mut po = FrameScene { time: 1.0, ..Default::default() };
    let variants: [Box<dyn Fn(&mut OstrichPose)>; 8] = [
        Box::new(|p| p.look_back = 0.5),
        Box::new(|p| p.look_back = 1.0),
        Box::new(|p| p.bash = 1.0),
        Box::new(|p| p.bash = -1.0),
        Box::new(|p| {
            p.airborne = true;
            p.tuck = 1.0;
            p.pos.y += 1.5;
        }),
        Box::new(|p| {
            p.trick_flip = 0.25;
            p.airborne = true;
            p.pos.y += 2.0;
        }),
        Box::new(|p| p.spin = 1.0),
        Box::new(|p| {
            p.laser_on = true;
            p.lean = 0.3;
        }),
    ];
    for (i, v) in variants.iter().enumerate() {
        let mut p = Chars::pose(i);
        p.pos = s0.pos + s0.right * (-8.4 + 2.4 * i as f32);
        p.yaw = track_yaw(s0) - 0.55;
        p.speed = 20.0;
        p.gait_phase = 0.15;
        v(&mut p);
        po.ostriches.push(p);
    }
    po.camera = Camera { eye: s0.pos + fwd * 17.0 + Vec3::Y * 3.0, target: s0.pos + Vec3::Y * 1.6, up: Vec3::Y, fov_y: 40f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("poses", &po);

}

fn drill_prop_shots(cx: &mut Ctx, day: &WorldGeometry) {
    let mut ground = day.clone();
    ground.road.clear();
    ground.wall = WallStyle::None;
    ground.scenery.clear();
    ground.start = None;
    ground.pads.clear();
    ground.fx = WorldFx::default();
    ground.terrain = Some(Terrain { origin: Vec2::splat(-50.0), cell: 10.0, w: 11, h: 11, heights: vec![0.0; 121] });
    cx.r.load_world(&cx.gpu.device, &cx.gpu.queue, &ground);
    let mut sc = FrameScene { fog_scale: 0.3, ..Default::default() };
    for (kind, pos, size) in [
        (DrillPropKind::Cone, Vec3::new(-7.0, 0.0, 0.0), Vec3::new(1.2, 2.6, 1.2)),
        (DrillPropKind::Pylon, Vec3::new(-3.0, 0.0, 0.0), Vec3::new(1.6, 4.0, 1.6)),
        (DrillPropKind::RingCone, Vec3::new(1.0, 0.0, 0.0), Vec3::new(0.7, 1.2, 0.7)),
        (DrillPropKind::Flag, Vec3::new(5.0, 0.0, 0.0), Vec3::new(1.4, 3.0, 1.0)),
        (DrillPropKind::Log, Vec3::new(0.0, 0.0, 5.0), Vec3::new(12.0, 0.8, 0.8)),
    ] {
        sc.drill_props.push(DrillPropInst { kind, pos, size, yaw: 0.0, knocked: 0.0 });
    }
    sc.camera = Camera { eye: Vec3::new(13.0, 8.0, 20.0), target: Vec3::new(0.0, 1.2, 2.0), up: Vec3::Y, fov_y: 48f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("drill_props", &sc);
    sc.camera.eye = Vec3::new(-13.0, 8.0, -20.0);
    cx.shot("drill_props_reverse", &sc);
    sc.drill_props.clear();
    for (x, knocked) in [(-5.0, 0.0), (-1.5, 0.45), (2.0, 1.0), (5.5, 1.0)] {
        sc.drill_props.push(DrillPropInst { kind: DrillPropKind::Cone, pos: Vec3::new(x, 0.0, 0.0), size: Vec3::new(1.2, 2.6, 1.2), yaw: 0.0, knocked });
    }
    sc.camera = Camera { eye: Vec3::new(10.0, 5.0, 16.0), target: Vec3::new(0.0, 0.8, 1.0), up: Vec3::Y, fov_y: 44f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("drill_cones_down", &sc);
    sc.time = 40.0;
    cx.shot("drill_cones_down_later", &sc);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "docs/local/shots/render".to_string());
    let only = args.next();
    let (w, h) = (
        std::env::var("SHOT_W").ok().and_then(|v| v.parse().ok()).unwrap_or(1280u32),
        std::env::var("SHOT_H").ok().and_then(|v| v.parse().ok()).unwrap_or(576u32),
    );
    let gpu = headless();
    // SHOT_LINEAR=1 renders into a non-sRGB target (the renderer then encodes itself).
    let fmt = if std::env::var("SHOT_LINEAR").is_ok() { wgpu::TextureFormat::Rgba8Unorm } else { SHOT_FORMAT };
    let mut r = Renderer::new(&gpu.device, &gpu.queue, fmt, Quality::Max);
    r.resize(w, h);
    let off = Offscreen::with_format(&gpu.device, w, h, fmt);
    let mut cx = Ctx { gpu, r, off, out: out.into(), only, log: Vec::new() };

    // ================= DAY SAVANNA =================
    let day = build_world(false, WallStyle::FencePosts, Palette::savanna());
    drill_prop_shots(&mut cx, &day);
    cx.r.load_world(&cx.gpu.device, &cx.gpu.queue, &day);
    let n = day.road.len();
    let racers = vec![
        Racer { ch: 0, s0: n as f32 * 0.50 - 2.0, lateral: 2.0, speed: 28.0, role: Role::Run },
        Racer { ch: 1, s0: n as f32 * 0.50 + 14.0, lateral: 4.0, speed: 26.0, role: Role::Drift },
        Racer { ch: 2, s0: n as f32 * 0.50 + 8.0, lateral: -3.5, speed: 30.0, role: Role::Jump },
        Racer { ch: 3, s0: n as f32 * 0.50 + 28.0, lateral: 0.0, speed: 29.0, role: Role::Laser },
        Racer { ch: 4, s0: n as f32 * 0.50 - 55.0, lateral: -6.0, speed: 50.0, role: Role::Trump },
    ];
    let mut sim = Sim { samples: day.road.clone(), racers, t: 0.0 };
    studio_shots(&mut cx, &day);
    cx.warm(&mut sim, 2.5);
    let mut sc = base_scene(&sim, sim.t);
    sc.shields.push(Shield { pos: sc.ostriches[2].pos + Vec3::Y * 1.1, radius: 1.9, alpha: 0.8, aura: false });
    sc.camera = chase_cam(&sc.ostriches[0], 6.5, 2.6, 2.0, 74.0);
    cx.shot("day_chase", &sc);

    // Turbo (42 m/s): the legs become a wheel. Close chase shot and a side view.
    let mut tb = sc.clone();
    tb.ostriches[0].speed = 42.0;
    tb.ostriches[0].gait_phase = 0.13;
    tb.emitters[0].boost = true;
    tb.emitters[0].dust = 1.0;
    let tp = tb.ostriches[0];
    let tf = Vec3::new(tp.yaw.sin(), 0.0, tp.yaw.cos());
    let trt = Vec3::new(-tp.yaw.cos(), 0.0, tp.yaw.sin());
    tb.camera = Camera { eye: tp.pos - tf * 4.4 + trt * 2.4 + Vec3::Y * 1.3, target: tp.pos + tf * 0.6 + Vec3::Y * 0.8, up: Vec3::Y, fov_y: 58f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("turbo", &tb);
    let mut tbs = tb.clone();
    tbs.camera = Camera { eye: tp.pos + trt * 5.2 + Vec3::Y * 1.0, target: tp.pos + Vec3::Y * 0.9, up: Vec3::Y, fov_y: 40f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("turbo_side", &tbs);

    // First person from racer 0.
    let mut fp = sc.clone();
    fp.ostriches[0].hide_head = true;
    let e = first_person_eye(&fp.ostriches[0]);
    fp.camera = Camera { eye: e.eye, target: e.eye + e.forward * 10.0, up: e.up, fov_y: 80f32.to_radians(), near: 0.08, far: 700.0 };
    cx.shot("day_firstperson", &fp);

    // Overview from above.
    let mut ov = sc.clone();
    ov.fog_scale = 0.15;
    ov.camera = Camera { eye: Vec3::new(-140.0, 190.0, -260.0), target: Vec3::new(0.0, 0.0, 20.0), up: Vec3::Y, fov_y: 55f32.to_radians(), near: 1.0, far: 1500.0 };
    cx.shot("day_overview", &ov);

    // Drift close-up.
    let mut dr = sc.clone();
    let dp = dr.ostriches[1];
    let f = Vec3::new(dp.yaw.sin(), 0.0, dp.yaw.cos());
    let rt = Vec3::new(-dp.yaw.cos(), 0.0, dp.yaw.sin());
    dr.camera = Camera { eye: dp.pos - f * 5.0 + rt * 3.2 + Vec3::Y * 1.6, target: dp.pos + Vec3::Y * 1.2, up: Vec3::Y, fov_y: 62f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("day_drift", &dr);
    // Drift wheel, side-on, and the Laser Trump from behind and the side.
    let mut dw = dr.clone();
    dw.camera = Camera { eye: dp.pos + rt * 5.0 + Vec3::Y * 1.0, target: dp.pos + Vec3::Y * 0.9, up: Vec3::Y, fov_y: 45f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("drift_wheel_side", &dw);
    let tpose = dr.ostriches[4];
    let tfw = Vec3::new(tpose.yaw.sin(), 0.0, tpose.yaw.cos());
    let trt = Vec3::new(-tpose.yaw.cos(), 0.0, tpose.yaw.sin());
    let mut tr = dr.clone();
    tr.camera = Camera { eye: tpose.pos - tfw * 5.0 + trt * 7.0 + Vec3::Y * 2.2, target: tpose.pos - tfw * 3.0 + Vec3::Y * 1.2, up: Vec3::Y, fov_y: 62f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("trump", &tr);
    let mut trs = dr.clone();
    trs.camera = Camera { eye: tpose.pos + trt * -7.0 + tfw * 2.0 + Vec3::Y * 1.5, target: tpose.pos - tfw * 3.0 + Vec3::Y * 1.2, up: Vec3::Y, fov_y: 60f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("trump_side", &trs);

    // Looking back + mirror: the player (racer 0) turns its head round.
    let mut mir = sc.clone();
    mir.ostriches[0].look_back = 1.0;
    mir.camera = chase_cam(&mir.ostriches[0], 6.5, 2.6, 2.0, 74.0);
    let head = head_transform(&mir.ostriches[0]);
    let eye = head.transform_point3(Vec3::new(0.0, 0.15, 0.0));
    let back = -Vec3::new(mir.ostriches[0].yaw.sin(), 0.0, mir.ostriches[0].yaw.cos());
    mir.mirror = Some(MirrorView {
        camera: Camera { eye, target: eye + back * 20.0, up: Vec3::Y, fov_y: 60f32.to_radians(), near: 0.2, far: 300.0 },
        rect_px: [w as f32 * 0.5 - 220.0, 14.0, 440.0, 184.0],
        scale: 1.0,
    });
    // Make something follow the player: racer 3's ahead; add a chaser behind via a duplicate.
    let mut chaser = Chars::pose(5);
    let (cp, cyaw, _) = at_s(&sim.samples, sim.racers[0].s0 + sim.racers[0].speed * sim.t - 14.0, -3.0);
    chaser.pos = cp;
    chaser.yaw = cyaw;
    chaser.speed = 30.0;
    chaser.gait_phase = 0.3;
    mir.ostriches.push(chaser);
    cx.shot("day_mirror", &mir);
    let mut mir_half = mir.clone();
    mir_half.mirror.as_mut().unwrap().scale = 0.45;
    cx.shot("day_mirror_opening", &mir_half);

    // Battery quality (1x path, upscale blit).
    cx.r.set_quality(Quality::Battery);
    cx.shot("day_chase_battery", &sc);
    let mut mir_b = mir.clone();
    mir_b.camera = mir.camera;
    cx.shot("day_mirror_battery", &mir_b);
    cx.r.set_quality(Quality::Balanced);
    cx.shot("day_chase_balanced", &sc);
    cx.r.set_quality(Quality::Max);

    // ================= NIGHT NEON =================
    let night = build_world(true, WallStyle::NeonRail, Palette::neon_night());
    cx.r.load_world(&cx.gpu.device, &cx.gpu.queue, &night);
    let n = night.road.len();
    let racers = vec![
        Racer { ch: 2, s0: n as f32 * 0.50 - 2.0, lateral: 2.0, speed: 30.0, role: Role::Run },
        Racer { ch: 6, s0: n as f32 * 0.50 + 14.0, lateral: 4.0, speed: 26.0, role: Role::Drift },
        Racer { ch: 3, s0: n as f32 * 0.50 + 8.0, lateral: -3.5, speed: 30.0, role: Role::Jump },
        Racer { ch: 1, s0: n as f32 * 0.50 + 28.0, lateral: 0.0, speed: 29.0, role: Role::Laser },
    ];
    let mut sim = Sim { samples: night.road.clone(), racers, t: 0.0 };
    cx.r.step_particles(&FrameScene::default(), 0.0);
    cx.warm(&mut sim, 2.5);
    let mut nsc = base_scene(&sim, sim.t);
    nsc.camera = chase_cam(&nsc.ostriches[0], 6.5, 2.6, 2.0, 74.0);
    // A Mega Laser from the lasering racer.
    let p3 = nsc.ostriches[3];
    let f3 = Vec3::new(p3.yaw.sin(), 0.0, p3.yaw.cos());
    nsc.beams.push(Beam { from: p3.pos + Vec3::Y * 2.2, to: p3.pos + Vec3::Y * 2.2 + f3 * 60.0, width: 1.1, color: [1.0, 0.55, 0.1], intensity: 1.5 });
    nsc.bursts.push(Burst { kind: BurstKind::Lightning, pos: nsc.ostriches[1].pos, count: 20 });
    cx.shot("night_chase", &nsc);
    let mut nov = nsc.clone();
    nov.fog_scale = 0.15;
    ov.camera = Camera { eye: Vec3::new(-140.0, 190.0, -260.0), target: Vec3::new(0.0, 0.0, 20.0), up: Vec3::Y, fov_y: 55f32.to_radians(), near: 1.0, far: 1500.0 };
    cx.shot("night_overview", &nov);
    // Jump over the gap: camera by the ramp.
    let mut ng = nsc.clone();
    let rs = &night.road[(n as f32 * 0.52) as usize];
    let rf = rs.up.cross(rs.right);
    ng.camera = Camera { eye: rs.pos - rf * 12.0 + rs.right * 7.0 + Vec3::Y * 3.5, target: rs.pos + rf * 14.0, up: Vec3::Y, fov_y: 60f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("night_gap", &ng);
    let mut ngt = nsc.clone();
    ngt.fog_scale = 0.1;
    ngt.camera = Camera { eye: rs.pos + Vec3::Y * 45.0 + rs.right * 3.0 - rf * 4.0, target: rs.pos + rf * 12.0, up: rf, fov_y: 55f32.to_radians(), near: 0.3, far: 700.0 };
    cx.shot("night_gap_top", &ngt);

    // Budget stress: 8 racers, everything emitting, mirror up, at night.
    let n = night.road.len();
    let racers: Vec<Racer> = (0..8)
        .map(|i| Racer {
            ch: i,
            s0: n as f32 * 0.30 + 6.0 * (i as f32 / 2.0).floor() - 3.0 * (i % 2) as f32,
            lateral: if i % 2 == 0 { -3.5 } else { 3.5 },
            speed: 27.0 + i as f32,
            role: match i % 4 {
                0 => Role::Run,
                1 => Role::Drift,
                2 => Role::Laser,
                _ => Role::Run,
            },
        })
        .collect();
    let mut sim = Sim { samples: night.road.clone(), racers, t: 0.0 };
    cx.warm(&mut sim, 3.0);
    let mut sc8 = base_scene(&sim, sim.t);
    for em in sc8.emitters.iter_mut() {
        em.dust = em.dust.max(0.9);
        em.offroad = true;
        em.boost = true;
        em.drift_tier = 3;
    }
    sc8.camera = chase_cam(&sc8.ostriches[0], 6.5, 2.6, 2.0, 74.0);
    let h = head_transform(&sc8.ostriches[0]).transform_point3(Vec3::new(0.0, 0.15, 0.0));
    let b = -Vec3::new(sc8.ostriches[0].yaw.sin(), 0.0, sc8.ostriches[0].yaw.cos());
    sc8.mirror = Some(MirrorView { camera: Camera { eye: h, target: h + b * 20.0, ..Default::default() }, rect_px: [w as f32 * 0.5 - 220.0, 14.0, 440.0, 184.0], scale: 1.0 });
    sc8.ostriches[0].look_back = 1.0;
    cx.shot("stress_8_mirror", &sc8);
    // The same budget with all eight in turbo (leg wheels, ghosts, swirls).
    let mut sct = sc8.clone();
    for o in sct.ostriches.iter_mut() {
        o.speed = 42.0;
    }
    cx.shot("stress_8_turbo", &sct);

    println!("---");
    for l in &cx.log {
        println!("{l}");
    }
    let _ = Mat4::IDENTITY;
}
