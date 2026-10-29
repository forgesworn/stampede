//! CPU particle system: dust kicked from the feet (and a plume from the drift
//! wheel), drift sparks, boost flames, Laser Trump gas and one-shot bursts. Advanced by `dt`, bounded by a fixed pool, drawn as
//! instanced billboards (dust alpha-blended, sparks and flames additive).

use crate::color::srgb3_to_linear;
use crate::ostrich::{foot_positions, tail_point};
use crate::types::*;
use bytemuck::{Pod, Zeroable};
use glam::Vec3;

pub const KIND_DUST: f32 = 0.0;
pub const KIND_GLOW: f32 = 1.0;
pub const KIND_FLAME: f32 = 2.0;
pub const KIND_STAR: f32 = 3.0;
pub const KIND_AURA: f32 = 4.0;

/// GPU instance: `a` = pos.xyz + size, `b` = rotation, seed, kind, life fraction,
/// `c` = linear rgba.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct ParticleInst {
    pub a: [f32; 4],
    pub b: [f32; 4],
    pub c: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
struct Particle {
    pos: Vec3,
    vel: Vec3,
    age: f32,
    life: f32,
    size0: f32,
    size1: f32,
    /// Linear rgb and peak alpha.
    color: [f32; 4],
    additive: bool,
    kind: f32,
    drag: f32,
    gravity: f32,
    buoy: f32,
    rot: f32,
    rot_vel: f32,
    seed: f32,
    /// Keep above the ground.
    on_ground: bool,
}

#[derive(Clone, Debug)]
struct Bolt {
    pts: Vec<Vec3>,
    age: f32,
    life: f32,
}

/// A zig-zag lightning segment to draw as a beam.
#[derive(Clone, Copy, Debug)]
pub struct BoltSeg {
    pub from: Vec3,
    pub to: Vec3,
    pub width: f32,
    pub intensity: f32,
}

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x >> 8) as f32 / 16_777_216.0
    }
    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.next()
    }
    fn signed(&mut self) -> f32 {
        self.next() * 2.0 - 1.0
    }
    fn dir(&mut self) -> Vec3 {
        loop {
            let v = Vec3::new(self.signed(), self.signed(), self.signed());
            let l = v.length_squared();
            if l > 0.01 && l <= 1.0 {
                return v / l.sqrt();
            }
        }
    }
}

pub struct ParticleSystem {
    parts: Vec<Particle>,
    cap: usize,
    cursor: usize,
    scale: f32,
    acc_dust: Vec<f32>,
    acc_spark: Vec<f32>,
    acc_flame: Vec<f32>,
    acc_wheel: Vec<f32>,
    acc_trump: Vec<f32>,
    rng: Rng,
    bolts: Vec<Bolt>,
    /// Colour for bursts that carry none (sRGB).
    pub ambient_dust: Rgb,
}

pub const POOL: usize = 3600;

/// The dust puff noise as a tileable `n` x `n` R8 tile: the two value-noise octaves the
/// fragment shader used to compute per pixel (about 3 and 8 cells across the puff),
/// baked once so the shader does a single fetch.
pub fn dust_noise(n: usize) -> Vec<u8> {
    fn hash(x: i32, y: i32, period: i32, salt: f32) -> f32 {
        let (x, y) = (x.rem_euclid(period) as f32, y.rem_euclid(period) as f32);
        let v = ((x + salt * 3.7) * 12.9898 + (y + salt * 1.3) * 78.233).sin() * 43758.547;
        v - v.floor()
    }
    fn vnoise(u: f32, v: f32, period: i32, salt: f32) -> f32 {
        let (x, y) = (u * period as f32, v * period as f32);
        let (ix, iy) = (x.floor() as i32, y.floor() as i32);
        let (fx, fy) = (x - ix as f32, y - iy as f32);
        let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let a = hash(ix, iy, period, salt);
        let b = hash(ix + 1, iy, period, salt);
        let c = hash(ix, iy + 1, period, salt);
        let d = hash(ix + 1, iy + 1, period, salt);
        let top = a + (b - a) * sx;
        let bot = c + (d - c) * sx;
        top + (bot - top) * sy
    }
    let mut out = Vec::with_capacity(n * n);
    for j in 0..n {
        for i in 0..n {
            let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
            let nz = vnoise(u, v, 3, 1.0) * 0.6 + vnoise(u, v, 8, 7.0) * 0.4;
            out.push((nz.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
    }
    out
}

impl ParticleSystem {
    pub fn new(scale: f32) -> ParticleSystem {
        let mut s = ParticleSystem {
            parts: Vec::with_capacity(POOL),
            cap: POOL,
            cursor: 0,
            scale: 1.0,
            acc_dust: Vec::new(),
            acc_spark: Vec::new(),
            acc_flame: Vec::new(),
            acc_wheel: Vec::new(),
            acc_trump: Vec::new(),
            rng: Rng(0x1234_5678),
            bolts: Vec::new(),
            ambient_dust: [0.8, 0.7, 0.55],
        };
        s.set_scale(scale);
        s
    }

    /// Particle quality: scales both the spawn rates and the pool.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale.clamp(0.1, 1.0);
        self.cap = ((POOL as f32) * self.scale) as usize;
        if self.parts.len() > self.cap {
            self.parts.truncate(self.cap);
        }
    }

    pub fn len(&self) -> usize {
        self.parts.len()
    }
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.cap
    }
    pub fn clear(&mut self) {
        self.parts.clear();
        self.bolts.clear();
    }

    fn push(&mut self, p: Particle) {
        if self.parts.len() < self.cap {
            self.parts.push(p);
        } else if self.cap > 0 {
            // Pool full: recycle round-robin (oldest-ish first).
            self.cursor = (self.cursor + 1) % self.cap;
            self.parts[self.cursor] = p;
        }
    }

    /// Advance existing particles.
    pub fn step(&mut self, dt: f32, ground: &dyn Fn(f32, f32, f32) -> f32) {
        let dt = dt.clamp(0.0, 0.1);
        for p in &mut self.parts {
            p.age += dt;
            p.vel.y += (p.buoy - p.gravity) * dt;
            p.vel *= (-p.drag * dt).exp();
            p.pos += p.vel * dt;
            p.rot += p.rot_vel * dt;
            if p.on_ground {
                // Dust puffs are billboards: keep their centre high enough that the
                // lower half does not slice through the road (no hard clip line).
                let u = (p.age / p.life).clamp(0.0, 1.0);
                let size = p.size0 + (p.size1 - p.size0) * (1.0 - (1.0 - u) * (1.0 - u));
                let lift = if p.kind == KIND_DUST { 0.4 * size } else { 0.0 };
                let g = ground(p.pos.x, p.pos.y, p.pos.z) + 0.06 + lift;
                if p.pos.y < g {
                    p.pos.y = g;
                    if p.vel.y < 0.0 {
                        p.vel.y = 0.0;
                    }
                }
            }
        }
        self.parts.retain(|p| p.age < p.life);
        if self.cursor >= self.parts.len() {
            self.cursor = 0;
        }
        for b in &mut self.bolts {
            b.age += dt;
        }
        self.bolts.retain(|b| b.age < b.life);
    }

    /// Spawn from the frame's emitters and bursts, then step. `cam` drives a
    /// cheap level of detail for distant racers.
    pub fn update(&mut self, dt: f32, scene: &FrameScene, cam: Vec3, ground: &dyn Fn(f32, f32, f32) -> f32) {
        let dt = dt.clamp(0.0, 0.1);
        let n = scene.ostriches.len();
        self.acc_dust.resize(n, 0.0);
        self.acc_spark.resize(n, 0.0);
        self.acc_flame.resize(n, 0.0);
        self.acc_wheel.resize(n, 0.0);
        self.acc_trump.resize(n, 0.0);
        for e in &scene.emitters {
            if e.racer >= n {
                continue;
            }
            let pose = scene.ostriches[e.racer];
            let dist = (pose.pos - cam).length();
            let lod = if dist > 90.0 { 0.35 } else if dist > 45.0 { 0.65 } else { 1.0 };
            self.emit_dust(e, &pose, dt, lod);
            self.emit_wheel_dust(e, &pose, dt, lod, cam);
            self.emit_sparks(e, &pose, dt, lod);
            self.emit_flames(e, &pose, dt, lod);
        }
        for (i, pose) in scene.ostriches.iter().enumerate() {
            let lod = {
                let dist = (pose.pos - cam).length();
                if dist > 90.0 {
                    0.35
                } else if dist > 45.0 {
                    0.65
                } else {
                    1.0
                }
            };
            self.emit_trump(i, pose, dt, lod);
        }
        for b in &scene.bursts {
            self.burst(b);
        }
        self.step(dt, ground);
    }

    /// The Sonic wheel digs in: a rooster-tail of dirt thrown low, back and out of
    /// the turn, about three times the dust of a plain drift in total. It hugs the
    /// ground and dies quickly near the camera, so it never veils the chase view.
    fn emit_wheel_dust(&mut self, e: &Emitter, pose: &OstrichPose, dt: f32, lod: f32, cam: Vec3) {
        let wheel = e.wheel.clamp(0.0, 1.0);
        let d = e.dust.clamp(0.0, 1.6);
        if wheel < 0.05 || d < 0.03 {
            self.acc_wheel[e.racer] = 0.0;
            return;
        }
        let drift_rate = (30.0 + 135.0 * d) * (1.5 + 0.7 * e.offroad as u32 as f32);
        self.acc_wheel[e.racer] += 2.0 * drift_rate * wheel * self.scale * lod * dt;
        let fwd = Vec3::new(pose.yaw.sin(), 0.0, pose.yaw.cos());
        let right = Vec3::new(-pose.yaw.cos(), 0.0, pose.yaw.sin());
        // Leaning right is turning right, so the dust flies out to the left.
        let turn = if pose.lean.abs() > 0.03 {
            Some(pose.lean.signum())
        } else {
            None
        };
        let feet = foot_positions(pose);
        // Lifted above the plain dust so the spray reads against the road.
        let base = srgb3_to_linear(e.dust_color).map(|c| c * 1.7);
        // Near the camera (the watched racer) the puffs are short-lived.
        let near = ((pose.pos - cam).length() / 14.0).clamp(0.5, 1.0);
        while self.acc_wheel[e.racer] >= 1.0 {
            self.acc_wheel[e.racer] -= 1.0;
            let foot = if self.rng.next() < 0.5 { feet[0] } else { feet[1] };
            let pos = Vec3::new(foot.x, pose.pos.y + 0.1, foot.z)
                + Vec3::new(self.rng.signed() * 0.2, 0.0, self.rng.signed() * 0.2);
            let side = turn.unwrap_or(if self.rng.next() < 0.5 { 1.0 } else { -1.0 });
            // The cloud keeps most of the racer's own speed, so it trails the feet slowly enough
            // for a chase camera to see it, and is flung hard out of the turn.
            let along = fwd * pose.speed * 0.62 * self.rng.range(0.7, 1.0);
            let shade = self.rng.range(0.85, 1.12);
            // Some of it is flung as chunks: small, bright, thrown up and out, and falling back.
            let chunk = self.rng.next() < 0.35;
            let (vel, life, size0, size1, alpha, drag, gravity) = if chunk {
                (
                    along - right * side * self.rng.range(4.0, 10.0) + Vec3::Y * self.rng.range(3.5, 6.5),
                    self.rng.range(0.55, 0.9) * near,
                    self.rng.range(0.5, 0.8),
                    self.rng.range(1.3, 2.0),
                    0.85,
                    0.7,
                    11.0,
                )
            } else {
                (
                    along - right * side * self.rng.range(5.0, 12.0) + Vec3::Y * self.rng.range(0.6, 2.0),
                    self.rng.range(0.9, 1.5) * near,
                    self.rng.range(1.0, 1.6),
                    self.rng.range(3.0, 4.6),
                    0.7,
                    1.3,
                    0.0,
                )
            };
            let pp = Particle {
                pos,
                vel,
                age: 0.0,
                life,
                size0,
                size1,
                color: [base[0] * shade, base[1] * shade, base[2] * shade, alpha * self.rng.range(0.8, 1.1)],
                additive: false,
                kind: KIND_DUST,
                drag,
                gravity,
                buoy: 0.0,
                rot: self.rng.range(0.0, 6.28),
                rot_vel: self.rng.signed() * 0.6,
                seed: self.rng.next() * 100.0,
                on_ground: true,
            };
            self.push(pp);
        }
    }

    /// Laser Trump: gassy green puffs streaming back from the tail, with a few
    /// bright motes shot down the beam.
    fn emit_trump(&mut self, i: usize, pose: &OstrichPose, dt: f32, lod: f32) {
        if !pose.trumping {
            self.acc_trump[i] = 0.0;
            return;
        }
        self.acc_trump[i] += 110.0 * self.scale * lod * dt;
        let fwd = Vec3::new(pose.yaw.sin(), 0.0, pose.yaw.cos());
        let right = Vec3::new(-pose.yaw.cos(), 0.0, pose.yaw.sin());
        let src = tail_point(pose);
        let sc = pose.scale.max(0.05);
        while self.acc_trump[i] >= 1.0 {
            self.acc_trump[i] -= 1.0;
            let pos = src
                + right * self.rng.signed() * 0.2 * sc
                + Vec3::Y * self.rng.signed() * 0.15 * sc;
            if self.rng.next() < 0.3 {
                // A bright mote riding the beam.
                let c = srgb3_to_linear([0.45, 1.0, 0.5]);
                let pp = Particle {
                    pos,
                    vel: -fwd * (pose.speed * 0.5 + self.rng.range(8.0, 22.0))
                        + right * self.rng.signed() * 0.6,
                    age: 0.0,
                    life: self.rng.range(0.2, 0.4),
                    size0: self.rng.range(0.35, 0.6) * sc,
                    size1: 0.05,
                    color: [c[0], c[1], c[2], 0.9],
                    additive: true,
                    kind: KIND_GLOW,
                    drag: 0.5,
                    gravity: 0.0,
                    buoy: 0.0,
                    rot: 0.0,
                    rot_vel: 0.0,
                    seed: self.rng.next() * 100.0,
                    on_ground: false,
                };
                self.push(pp);
                continue;
            }
            let g = self.rng.range(0.85, 1.1);
            let c = srgb3_to_linear([0.42 * g, 0.85 * g, 0.26 * g]);
            let pp = Particle {
                pos,
                // Left hanging in the air behind the racer: a trail, not a jet.
                vel: fwd * (pose.speed * 0.12 - self.rng.range(1.0, 4.0)) + right * self.rng.signed() * 1.8 + Vec3::Y * self.rng.range(0.4, 2.0),
                age: 0.0,
                life: self.rng.range(0.9, 1.6),
                size0: self.rng.range(1.0, 1.5) * sc,
                size1: self.rng.range(2.6, 4.2) * sc,
                color: [c[0], c[1], c[2], 0.62 * self.rng.range(0.8, 1.1)],
                additive: false,
                kind: KIND_DUST,
                drag: 1.8,
                gravity: 0.0,
                buoy: 0.5,
                rot: self.rng.range(0.0, 6.28),
                rot_vel: self.rng.signed() * 0.8,
                seed: self.rng.next() * 100.0,
                on_ground: false,
            };
            self.push(pp);
        }
    }

    fn emit_dust(&mut self, e: &Emitter, pose: &OstrichPose, dt: f32, lod: f32) {
        let d = e.dust.clamp(0.0, 1.6);
        if d < 0.03 {
            self.acc_dust[e.racer] = 0.0;
            return;
        }
        let mult = 1.0 + 0.5 * e.drifting as u32 as f32 + 0.7 * e.offroad as u32 as f32;
        let rate = (30.0 + 135.0 * d) * mult * self.scale * lod;
        self.acc_dust[e.racer] += rate * dt;
        let big = 1.0 + 0.45 * e.drifting as u32 as f32 + 0.6 * e.offroad as u32 as f32;
        let fwd = Vec3::new(pose.yaw.sin(), 0.0, pose.yaw.cos());
        let right = Vec3::new(-pose.yaw.cos(), 0.0, pose.yaw.sin());
        let feet = foot_positions(pose);
        let base = srgb3_to_linear(e.dust_color);
        while self.acc_dust[e.racer] >= 1.0 {
            self.acc_dust[e.racer] -= 1.0;
            let foot = if self.rng.next() < 0.5 { feet[0] } else { feet[1] };
            let pos = foot + Vec3::new(self.rng.signed() * 0.2, 0.08, self.rng.signed() * 0.2);
            let spread = 0.9 + 1.4 * d.min(1.0) + 1.2 * (e.drifting as u32 as f32);
            let mut vel = -fwd * (0.6 + pose.speed * 0.20 * self.rng.range(0.4, 1.0))
                + right * self.rng.signed() * spread * big
                + Vec3::Y * self.rng.range(0.8, 2.6 + 1.2 * d.min(1.0));
            vel += fwd * pose.speed * 0.12;
            let shade = self.rng.range(0.85, 1.12);
            let life = self.rng.range(1.1, 2.1) * (1.0 + 0.35 * e.offroad as u32 as f32);
            let pp = Particle {
                pos,
                vel,
                age: 0.0,
                life,
                size0: self.rng.range(0.7, 1.1) * big,
                size1: self.rng.range(2.4, 4.0) * big * (0.7 + 0.5 * d.min(1.0)),
                color: [base[0] * shade, base[1] * shade, base[2] * shade, (0.30 + 0.20 * d.min(1.0) + 0.08 * (e.drifting as u32 as f32 + e.offroad as u32 as f32)) * self.rng.range(0.8, 1.1)],
                additive: false,
                kind: KIND_DUST,
                drag: 1.4,
                gravity: 0.0,
                buoy: 0.45,
                rot: self.rng.range(0.0, 6.28),
                rot_vel: self.rng.signed() * 0.5,
                seed: self.rng.next() * 100.0,
                on_ground: true,
            };
self.push(pp);
        }
    }

    fn emit_sparks(&mut self, e: &Emitter, pose: &OstrichPose, dt: f32, lod: f32) {
        if e.drift_tier == 0 {
            self.acc_spark[e.racer] = 0.0;
            return;
        }
        let tier = e.drift_tier.min(3);
        let col = crate::color::drift_tier_rgb(tier).unwrap_or([1.0; 3]);
        let lin = srgb3_to_linear(col);
        let rate = (80.0 + 40.0 * tier as f32) * self.scale * lod;
        self.acc_spark[e.racer] += rate * dt;
        let fwd = Vec3::new(pose.yaw.sin(), 0.0, pose.yaw.cos());
        let right = Vec3::new(-pose.yaw.cos(), 0.0, pose.yaw.sin());
        while self.acc_spark[e.racer] >= 1.0 {
            self.acc_spark[e.racer] -= 1.0;
            let side = if self.rng.next() < 0.5 { 1.0 } else { -1.0 };
            let pos = pose.pos + right * side * 0.5 - fwd * 0.75 + Vec3::Y * 0.12;
            let vel = -fwd * self.rng.range(1.0, 5.0) + right * side * self.rng.range(0.5, 3.5) + Vec3::Y * self.rng.range(1.5, 4.5);
            let big = tier == 3 && self.rng.next() < 0.15;
            let pp = Particle {
                pos,
                vel,
                age: 0.0,
                life: self.rng.range(0.25, 0.55),
                size0: if big { 0.9 } else { self.rng.range(0.22, 0.4) },
                size1: 0.03,
                color: [lin[0], lin[1], lin[2], 1.0],
                additive: true,
                kind: if big { KIND_STAR } else { KIND_GLOW },
                drag: 0.6,
                gravity: 9.0,
                buoy: 0.0,
                rot: self.rng.range(0.0, 6.28),
                rot_vel: self.rng.signed() * 5.0,
                seed: self.rng.next() * 100.0,
                on_ground: true,
            };
self.push(pp);
        }
    }

    fn emit_flames(&mut self, e: &Emitter, pose: &OstrichPose, dt: f32, lod: f32) {
        if !e.boost && !e.burning {
            self.acc_flame[e.racer] = 0.0;
            return;
        }
        let rate = (if e.burning { 85.0 } else { 150.0 }) * self.scale * lod;
        self.acc_flame[e.racer] += rate * dt;
        let fwd = Vec3::new(pose.yaw.sin(), 0.0, pose.yaw.cos());
        let right = Vec3::new(-pose.yaw.cos(), 0.0, pose.yaw.sin());
        let sc = pose.scale;
        while self.acc_flame[e.racer] >= 1.0 {
            self.acc_flame[e.racer] -= 1.0;
            let pos = pose.pos - fwd * 1.1 * sc + Vec3::Y * 1.05 * sc + right * self.rng.signed() * 0.25 * sc;
            let vel = if e.burning { -fwd * pose.speed * 0.12 + Vec3::Y * self.rng.range(1.8, 3.5) + right * self.rng.signed() * 0.6 } else { -fwd * (pose.speed * 0.25 + self.rng.range(4.0, 8.0)) + Vec3::Y * self.rng.range(-0.3, 1.2) + right * self.rng.signed() * 0.8 };
            let hot = self.rng.next();
            let smoke = e.burning && hot < 0.4;
            let c = if smoke { srgb3_to_linear([0.25, 0.25, 0.28]) } else if hot > 0.5 { srgb3_to_linear([1.0, 0.75, 0.25]) } else { srgb3_to_linear([1.0, 0.35, 0.08]) };
            let pp = Particle {
                pos,
                vel,
                age: 0.0,
                life: if smoke { self.rng.range(0.8, 1.35) } else { self.rng.range(0.3, 0.55) },
                size0: self.rng.range(0.38, 0.6) * sc,
                size1: if smoke { 1.8 } else { 0.08 },
                color: [c[0], c[1], c[2], if smoke { 0.65 } else { 0.95 }],
                additive: !smoke,
                kind: if smoke { KIND_DUST } else { KIND_FLAME },
                drag: 2.0,
                gravity: 0.0,
                buoy: 1.0,
                rot: self.rng.range(0.0, 6.28),
                rot_vel: self.rng.signed() * 3.0,
                seed: self.rng.next() * 100.0,
                on_ground: false,
            };
self.push(pp);
        }
    }

    /// Spawn one burst now.
    pub fn burst(&mut self, b: &Burst) {
        let n = b.count.clamp(1, 80);
        let dust_lin = srgb3_to_linear(self.ambient_dust);
        match b.kind {
            BurstKind::TargetShatter => {
                let gold = srgb3_to_linear([1.0, 0.78, 0.2]);
                let orange = srgb3_to_linear([1.0, 0.5, 0.08]);
                for i in 0..n {
                    let v = self.rng.dir() * self.rng.range(4.0, 11.0);
                    let c = if i % 3 == 0 { orange } else { gold };
                    let pp = Particle {
                        pos: b.pos,
                        vel: v,
                        age: 0.0,
                        life: self.rng.range(0.5, 1.0),
                        size0: self.rng.range(0.2, 0.34),
                        size1: 0.02,
                        color: [c[0], c[1], c[2], 1.0],
                        additive: true,
                        kind: if i % 4 == 0 { KIND_STAR } else { KIND_GLOW },
                        drag: 1.0,
                        gravity: 11.0,
                        buoy: 0.0,
                        rot: self.rng.range(0.0, 6.28),
                        rot_vel: self.rng.signed() * 6.0,
                        seed: self.rng.next() * 100.0,
                        on_ground: true,
                    };
self.push(pp);
                }
                self.flash(b.pos, 4.5, [1.0, 0.8, 0.35], 0.22);
                for _ in 0..(n / 4).max(3) {
                    let v = self.rng.dir() * self.rng.range(1.0, 3.5);
                    self.puff(b.pos, v, [0.95, 0.85, 0.6], 0.5, 1.3, 0.8, 0.45);
                }
            }
            BurstKind::CoinSparkle => {
                let gold = srgb3_to_linear([1.0, 0.85, 0.3]);
                for _ in 0..n.min(14) {
                    let v = self.rng.dir() * self.rng.range(1.0, 3.2) + Vec3::Y * 1.2;
                    let pp = Particle {
                        pos: b.pos,
                        vel: v,
                        age: 0.0,
                        life: self.rng.range(0.4, 0.75),
                        size0: self.rng.range(0.22, 0.38),
                        size1: 0.0,
                        color: [gold[0], gold[1], gold[2], 1.0],
                        additive: true,
                        kind: KIND_STAR,
                        drag: 2.0,
                        gravity: 2.0,
                        buoy: 0.0,
                        rot: self.rng.range(0.0, 6.28),
                        rot_vel: self.rng.signed() * 4.0,
                        seed: self.rng.next() * 100.0,
                        on_ground: false,
                    };
self.push(pp);
                }
                self.flash(b.pos, 1.6, [1.0, 0.85, 0.35], 0.18);
            }
            BurstKind::HitPuff => {
                for _ in 0..n.min(24) {
                    let v = self.rng.dir() * self.rng.range(2.0, 6.0);
                    self.puff(b.pos, v, [0.92, 0.9, 0.86], 0.5, 2.0, 0.8, 0.5);
                }
                for _ in 0..(n / 2).clamp(4, 14) {
                    let v = self.rng.dir() * self.rng.range(4.0, 9.0);
                    let pp = Particle {
                        pos: b.pos,
                        vel: v,
                        age: 0.0,
                        life: self.rng.range(0.3, 0.6),
                        size0: self.rng.range(0.22, 0.4),
                        size1: 0.0,
                        color: [1.0, 0.95, 0.6, 1.0],
                        additive: true,
                        kind: KIND_STAR,
                        drag: 2.5,
                        gravity: 4.0,
                        buoy: 0.0,
                        rot: self.rng.range(0.0, 6.28),
                        rot_vel: self.rng.signed() * 8.0,
                        seed: self.rng.next() * 100.0,
                        on_ground: false,
                    };
self.push(pp);
                }
                self.flash(b.pos, 3.0, [1.0, 0.95, 0.7], 0.15);
            }
            BurstKind::LandingDust => {
                for i in 0..n.min(48) {
                    let a = std::f32::consts::TAU * i as f32 / n.min(48) as f32 + self.rng.signed() * 0.2;
                    let sp = self.rng.range(2.5, 6.5);
                    let v = Vec3::new(a.cos() * sp, self.rng.range(0.2, 1.2), a.sin() * sp);
                    let col = [dust_lin[0], dust_lin[1], dust_lin[2]];
                    let pp = Particle {
                        pos: b.pos + Vec3::new(a.cos() * 0.4, 0.1, a.sin() * 0.4),
                        vel: v,
                        age: 0.0,
                        life: self.rng.range(0.8, 1.5),
                        size0: self.rng.range(0.5, 0.8),
                        size1: self.rng.range(2.0, 3.2),
                        color: [col[0], col[1], col[2], 0.55],
                        additive: false,
                        kind: KIND_DUST,
                        drag: 2.2,
                        gravity: 0.0,
                        buoy: 0.35,
                        rot: self.rng.range(0.0, 6.28),
                        rot_vel: self.rng.signed() * 0.5,
                        seed: self.rng.next() * 100.0,
                        on_ground: true,
                    };
self.push(pp);
                }
            }
            BurstKind::Lightning => {
                let top = b.pos + Vec3::Y * 36.0;
                let mut pts = vec![top];
                let segs = 12;
                for i in 1..segs {
                    let t = i as f32 / segs as f32;
                    let p = top.lerp(b.pos, t) + Vec3::new(self.rng.signed(), 0.0, self.rng.signed()) * 1.6 * (1.0 - 0.3 * t);
                    pts.push(p);
                }
                pts.push(b.pos);
                self.bolts.push(Bolt { pts, age: 0.0, life: 0.32 });
                self.flash(b.pos + Vec3::Y * 0.5, 7.0, [0.7, 0.85, 1.0], 0.3);
                let c = srgb3_to_linear([0.65, 0.85, 1.0]);
                for _ in 0..n.min(30) {
                    let v = self.rng.dir() * self.rng.range(3.0, 9.0) + Vec3::Y * 3.0;
                    let pp = Particle {
                        pos: b.pos + Vec3::Y * 0.3,
                        vel: v,
                        age: 0.0,
                        life: self.rng.range(0.3, 0.7),
                        size0: self.rng.range(0.14, 0.26),
                        size1: 0.02,
                        color: [c[0], c[1], c[2], 1.0],
                        additive: true,
                        kind: KIND_GLOW,
                        drag: 1.2,
                        gravity: 10.0,
                        buoy: 0.0,
                        rot: 0.0,
                        rot_vel: 0.0,
                        seed: self.rng.next() * 100.0,
                        on_ground: true,
                    };
self.push(pp);
                }
            }
        }
    }

    fn flash(&mut self, pos: Vec3, size: f32, col: Rgb, life: f32) {
        let c = srgb3_to_linear(col);
        let pp = Particle {
            pos,
            vel: Vec3::ZERO,
            age: 0.0,
            life,
            size0: size * 0.6,
            size1: size,
            color: [c[0], c[1], c[2], 0.9],
            additive: true,
            kind: KIND_GLOW,
            drag: 0.0,
            gravity: 0.0,
            buoy: 0.0,
            rot: 0.0,
            rot_vel: 0.0,
            seed: 0.0,
            on_ground: false,
        };
self.push(pp);
    }

    #[allow(clippy::too_many_arguments)]
    fn puff(&mut self, pos: Vec3, vel: Vec3, col: Rgb, s0: f32, s1: f32, alpha: f32, life: f32) {
        let c = srgb3_to_linear(col);
        let pp = Particle {
            pos,
            vel,
            age: 0.0,
            life: life * self.rng.range(0.8, 1.3),
            size0: s0,
            size1: s1,
            color: [c[0], c[1], c[2], alpha],
            additive: false,
            kind: KIND_DUST,
            drag: 3.0,
            gravity: 0.0,
            buoy: 0.3,
            rot: self.rng.range(0.0, 6.28),
            rot_vel: self.rng.signed(),
            seed: self.rng.next() * 100.0,
            on_ground: false,
        };
self.push(pp);
    }

    /// GPU instances: `alpha` sorted far to near, `additive` unsorted.
    pub fn instances(&self, cam: Vec3, alpha: &mut Vec<ParticleInst>, additive: &mut Vec<ParticleInst>) {
        alpha.clear();
        additive.clear();
        let mut tmp: Vec<(f32, ParticleInst)> = Vec::new();
        for p in &self.parts {
            let u = (p.age / p.life).clamp(0.0, 1.0);
            let size = p.size0 + (p.size1 - p.size0) * (1.0 - (1.0 - u) * (1.0 - u));
            let fade = if p.kind == KIND_DUST {
                smooth01(u / 0.12) * (1.0 - u).powf(1.4)
            } else {
                (1.0 - u).powf(1.2)
            };
            // Dust right against the lens would fill the screen: thin it out close in.
            let fade = if p.kind == KIND_DUST { fade * smooth01(((p.pos - cam).length() - 1.5) / 3.5) } else { fade };
            let inst = ParticleInst {
                a: [p.pos.x, p.pos.y, p.pos.z, size],
                b: [p.rot, p.seed, p.kind, u],
                c: [p.color[0], p.color[1], p.color[2], p.color[3] * fade],
            };
            if p.additive {
                additive.push(inst);
            } else {
                tmp.push(((p.pos - cam).length_squared(), inst));
            }
        }
        tmp.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        alpha.extend(tmp.into_iter().map(|t| t.1));
    }

    /// Live lightning bolts as beam segments.
    pub fn bolt_segments(&self, out: &mut Vec<BoltSeg>) {
        for b in &self.bolts {
            let u = (b.age / b.life).clamp(0.0, 1.0);
            let flick = if (b.age * 40.0) as i32 % 2 == 0 { 1.0 } else { 0.6 };
            let inten = (1.0 - u) * flick * 1.6;
            for w in b.pts.windows(2) {
                out.push(BoltSeg { from: w[0], to: w[1], width: 0.35 * (1.0 - 0.5 * u), intensity: inten });
            }
        }
    }
}

fn smooth01(x: f32) -> f32 {
    let t = x.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene_full() -> FrameScene {
        let mut s = FrameScene::default();
        for i in 0..8 {
            s.ostriches.push(OstrichPose { pos: Vec3::new(i as f32 * 3.0, 0.0, 0.0), speed: 40.0, ..Default::default() });
            s.emitters.push(Emitter {
                racer: i,
                dust: 1.5,
                dust_color: [0.8, 0.7, 0.5],
                drift_tier: 3,
                drifting: true,
                wheel: 0.0,
                offroad: true,
                boost: true,
                burning: false,
            });
        }
        s
    }

    #[test]
    fn pool_stays_bounded() {
        let mut ps = ParticleSystem::new(1.0);
        let scene = scene_full();
        for i in 0..2000 {
            let mut sc = scene.clone();
            if i % 10 == 0 {
                sc.bursts.push(Burst { kind: BurstKind::TargetShatter, pos: Vec3::ZERO, count: 80 });
                sc.bursts.push(Burst { kind: BurstKind::Lightning, pos: Vec3::ZERO, count: 80 });
            }
            ps.update(1.0 / 60.0, &sc, Vec3::new(0.0, 3.0, -8.0), &|_, _, _| 0.0);
            assert!(ps.len() <= ps.capacity());
        }
        assert!(ps.len() > 500, "expected a lot of particles at max rates, got {}", ps.len());
        let mut a = Vec::new();
        let mut b = Vec::new();
        ps.instances(Vec3::ZERO, &mut a, &mut b);
        assert_eq!(a.len() + b.len(), ps.len());
        for i in a.iter().chain(b.iter()) {
            assert!(i.a.iter().chain(i.b.iter()).chain(i.c.iter()).all(|v| v.is_finite()));
        }
    }

    #[test]
    fn battery_halves_pool_and_rate() {
        let scene = scene_full();
        let mut full = ParticleSystem::new(1.0);
        let mut half = ParticleSystem::new(0.5);
        assert_eq!(half.capacity() * 2, full.capacity());
        for _ in 0..30 {
            full.update(1.0 / 60.0, &scene, Vec3::ZERO, &|_, _, _| 0.0);
            half.update(1.0 / 60.0, &scene, Vec3::ZERO, &|_, _, _| 0.0);
        }
        assert!((half.len() as f32) < full.len() as f32 * 0.7);
    }

    #[test]
    fn spawn_rate_is_dt_independent() {
        let mut scene = FrameScene::default();
        scene.ostriches.push(OstrichPose {
            speed: 20.0,
            ..Default::default()
        });
        scene.emitters.push(Emitter {
            racer: 0,
            dust: 1.0,
            dust_color: [0.8; 3],
            drift_tier: 0,
            drifting: false,
            wheel: 0.0,
            offroad: false,
            boost: false,
            burning: false,
        });
        let mut a = ParticleSystem::new(1.0);
        let mut b = ParticleSystem::new(1.0);
        for _ in 0..30 {
            a.update(1.0 / 60.0, &scene, Vec3::ZERO, &|_, _, _| 0.0);
        }
        for _ in 0..15 {
            b.update(1.0 / 30.0, &scene, Vec3::ZERO, &|_, _, _| 0.0);
        }
        let (la, lb) = (a.len() as i32, b.len() as i32);
        assert!((la - lb).abs() <= 3, "{la} vs {lb}");
    }

    #[test]
    fn dust_noise_tile_is_full_range_and_wraps() {
        let n = 64;
        let t = dust_noise(n);
        assert_eq!(t.len(), n * n);
        let mean = t.iter().map(|&v| v as f32).sum::<f32>() / t.len() as f32 / 255.0;
        assert!((0.35..0.65).contains(&mean), "mean {mean}");
        let (lo, hi) = (*t.iter().min().unwrap(), *t.iter().max().unwrap());
        assert!(hi as i32 - lo as i32 > 90, "needs contrast: {lo}..{hi}");
        // Tileable: the step across the seam is no bigger than a typical step inside.
        let step = |a: u8, b: u8| (a as i32 - b as i32).abs();
        let seam = (0..n).map(|j| step(t[j * n + n - 1], t[j * n])).max().unwrap();
        assert!(seam < 40, "seam step {seam}");
    }

    fn run(scene: &FrameScene, frames: usize) -> ParticleSystem {
        let mut ps = ParticleSystem::new(1.0);
        for _ in 0..frames {
            ps.update(1.0 / 60.0, scene, Vec3::ZERO, &|_, _, _| 0.0);
        }
        ps
    }

    fn drift_scene(wheel: f32, lean: f32) -> FrameScene {
        let mut s = FrameScene::default();
        s.ostriches.push(OstrichPose {
            speed: 25.0,
            lean,
            wheel,
            ..Default::default()
        });
        s.emitters.push(Emitter {
            racer: 0,
            dust: 1.0,
            dust_color: [0.8; 3],
            drift_tier: 0,
            drifting: true,
            wheel,
            offroad: false,
            boost: false,
            burning: false,
        });
        s
    }

    #[test]
    fn wheel_throws_a_big_plume_of_dust() {
        let plain = run(&drift_scene(0.0, 0.2), 30);
        let wheel = run(&drift_scene(1.0, 0.2), 30);
        assert!(
            wheel.len() as f32 > plain.len() as f32 * 2.5,
            "{} vs {}",
            wheel.len(),
            plain.len()
        );
        // Part of the spray is thrown as small falling chunks.
        assert!(wheel.parts.iter().any(|p| p.gravity > 5.0 && p.size1 < 2.5));
        // Half a wheel gives half the extra.
        let half = run(&drift_scene(0.5, 0.2), 30);
        assert!(half.len() > plain.len() && half.len() < wheel.len());
        // No dust, no plume (airborne).
        let mut s = drift_scene(1.0, 0.2);
        s.emitters[0].dust = 0.0;
        assert_eq!(run(&s, 30).len(), 0);
    }

    #[test]
    fn wheel_plume_flies_back_and_out_of_the_turn() {
        // Facing +Z, the racer's right is -X; leaning right means turning right.
        for (lean, out_sign) in [(0.25f32, 1.0f32), (-0.25, -1.0)] {
            let only_wheel = run(&drift_scene(1.0, lean), 20);
            let (mut x, mut z, mut up) = (0.0, 0.0, 0.0);
            for p in &only_wheel.parts {
                x += p.vel.x;
                z += p.vel.z;
                up += p.vel.y;
            }
            let n = only_wheel.parts.len() as f32;
            assert!(
                x / n * out_sign > 0.3,
                "lean {lean}: mean lateral velocity {}",
                x / n
            );
            // It falls behind the racer (25 m/s) and is thrown upward a little.
            assert!(z / n < 25.0 * 0.8 && up / n > 0.0);
        }
    }

    #[test]
    fn trumping_streams_green_gas_from_the_tail() {
        let mut s = FrameScene::default();
        s.ostriches.push(OstrichPose {
            speed: 50.0,
            trumping: true,
            ..Default::default()
        });
        // No emitter needed.
        let ps = run(&s, 60);
        assert!(ps.len() > 40, "{}", ps.len());
        let gas: Vec<&Particle> = ps.parts.iter().filter(|p| !p.additive).collect();
        assert!(gas.len() > 20);
        for p in &gas {
            // Green-tinted.
            assert!(p.color[1] > p.color[0] && p.color[1] > p.color[2]);
            // Left hanging behind the racer (who faces +Z at 50 m/s), not carried with it.
            assert!(p.vel.z < 0.3 * 50.0);
        }
        let mut quiet = FrameScene::default();
        quiet.ostriches.push(OstrichPose {
            speed: 50.0,
            ..Default::default()
        });
        assert_eq!(run(&quiet, 60).len(), 0);
    }

    #[test]
    fn trump_and_wheel_respect_the_pool() {
        let mut s = scene_full();
        for (i, o) in s.ostriches.iter_mut().enumerate() {
            o.trumping = i % 2 == 0;
            o.wheel = 1.0;
            o.lean = 0.2;
            s.emitters[i].wheel = 1.0;
        }
        let mut ps = ParticleSystem::new(1.0);
        for _ in 0..600 {
            ps.update(1.0 / 60.0, &s, Vec3::new(0.0, 3.0, -8.0), &|_, _, _| 0.0);
            assert!(ps.len() <= ps.capacity());
        }
        let mut half = ParticleSystem::new(0.5);
        for _ in 0..600 {
            half.update(1.0 / 60.0, &s, Vec3::new(0.0, 3.0, -8.0), &|_, _, _| 0.0);
            assert!(half.len() <= half.capacity());
        }
    }

    #[test]
    fn particles_die() {
        let mut ps = ParticleSystem::new(1.0);
        ps.burst(&Burst { kind: BurstKind::LandingDust, pos: Vec3::ZERO, count: 40 });
        assert!(ps.len() > 0);
        for _ in 0..400 {
            ps.step(1.0 / 60.0, &|_, _, _| 0.0);
        }
        assert_eq!(ps.len(), 0);
    }
}
