//! Battle presentation that does not belong in `present.rs`: out racers fade
//! (shrink and spin away), a racer that just lost a feather blinks, and the
//! one-shot puffs for outs, hits and steals. `present::Presenter::poses` calls
//! [`adjust_poses`] once at its end; the race screen calls [`scene_bursts`].

use super::present::class_scale;
use super::source::RaceView;
use glam::Vec3;
use race_core::{Character, Event};
use race_render::{Burst, BurstKind, FrameScene, OstrichPose};

/// Blink rate of an invulnerable racer (full on/off cycles per second).
pub const BLINK_HZ: f32 = 5.0;

/// Is a blinking racer visible at animation time `t`? (60 % on, so it reads as a flash, not a vanish.)
pub fn blink_visible(t: f32) -> bool {
    (t * BLINK_HZ).fract() < 0.6
}

/// The scale an out racer is drawn at: full while it has not faded, shrinking to nothing as `fade` runs 1 -> 0.
pub fn out_scale(base: f32, fade: f32) -> f32 {
    let f = fade.clamp(0.0, 1.0);
    if f <= 0.0 {
        0.001
    } else {
        // Ease so the ostrich lingers, then collapses.
        base * (f * f * (3.0 - 2.0 * f)).max(0.001)
    }
}

/// Apply the battle look to `poses` (index = racer): out racers fade away spinning, invulnerable ones blink.
pub fn adjust_poses(v: &RaceView, poses: &mut [OstrichPose], time: f32) {
    let Some(b) = v.battle else { return };
    for (i, p) in poses.iter_mut().enumerate() {
        let Some(r) = v.racers.get(i) else { continue };
        if b.is_out(i) {
            let f = b.fade(i);
            let base = class_scale(Character::get(r.character).class);
            p.scale = out_scale(base, f);
            p.spin += (1.0 - f) * std::f32::consts::TAU * 1.5;
            p.laser_on = false;
        } else if b.flashing(i) && !blink_visible(time + i as f32 * 0.13) {
            p.scale = 0.001;
        }
    }
}

/// One-shot puffs for this frame's battle events.
pub fn scene_bursts(events: &[Event], v: &RaceView, scene: &mut FrameScene) {
    if v.battle.is_none() {
        return;
    }
    for e in events {
        match *e {
            Event::Out { pos, .. } => {
                let at = Vec3::from_array(pos) + Vec3::Y * 1.4;
                scene.bursts.push(Burst { kind: BurstKind::HitPuff, pos: at, count: 48 });
                scene.bursts.push(Burst { kind: BurstKind::CoinSparkle, pos: at + Vec3::Y * 0.6, count: 24 });
            }
            Event::FeatherLost { racer, .. } => {
                if let Some(p) = scene.ostriches.get(racer as usize) {
                    let at = p.pos + Vec3::Y * 2.0;
                    scene.bursts.push(Burst { kind: BurstKind::CoinSparkle, pos: at, count: 12 });
                }
            }
            Event::FeatherStolen { racer, .. } => {
                if let Some(p) = scene.ostriches.get(racer as usize) {
                    scene.bursts.push(Burst { kind: BurstKind::CoinSparkle, pos: p.pos + Vec3::Y * 2.2, count: 16 });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_racers_shrink_to_nothing() {
        assert_eq!(out_scale(1.0, 1.0), 1.0);
        assert!(out_scale(1.0, 0.5) < 1.0 && out_scale(1.0, 0.5) > 0.2);
        assert_eq!(out_scale(1.1, 0.0), 0.001);
        assert!(out_scale(1.0, 0.1) < out_scale(1.0, 0.2));
    }

    #[test]
    fn blink_is_mostly_on_and_flips() {
        let on = (0..100).filter(|k| blink_visible(*k as f32 * 0.01)).count();
        assert!((50..=70).contains(&on), "{on}");
        assert!(blink_visible(0.0) && !blink_visible(0.15));
    }
}
