//! Screen-relative tilt steering. Sensor acquisition stays in the platform;
//! calibration, dead zone and filtering are shared with the test harness.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Reading {
    #[default]
    Unavailable,
    Waiting,
    Sample {
        gravity: [f32; 3],
        rotation: u8,
        generation: u32,
        age: f32,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Off,
    Unavailable,
    Waiting,
    Centring,
    Ready,
}

/// Roll smoothing time constant (s) and the most the horizon is ever rolled (degrees).
const ROLL_TAU: f32 = 0.04;
const ROLL_LIMIT: f32 = 60.0;

#[derive(Default)]
pub struct Steering {
    status: Status,
    session: Option<(u8, u32)>,
    angle: Option<f32>,
    /// Lightly smoothed `angle`, for levelling the 3D horizon.
    roll: Option<f32>,
    neutral: f32,
    settle: f32,
    value: f32,
}

impl Steering {
    pub fn status(&self) -> Status {
        self.status
    }

    pub fn steer(&self) -> Option<f32> {
        (self.status == Status::Ready).then_some(self.value)
    }

    /// The phone's roll against gravity in the current screen rotation (radians,
    /// + = right edge down), smoothed and clamped to +-60 degrees. Unlike the
    /// steering value it ignores the calibrated neutral: the horizon follows the
    /// real ground. `None` unless the sensor is centring or ready.
    pub fn roll(&self) -> Option<f32> {
        matches!(self.status, Status::Centring | Status::Ready).then_some(self.roll).flatten()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn centre(&mut self) -> bool {
        let Some(angle) = self.angle else { return false };
        self.neutral = angle;
        self.settle = 0.25;
        self.value = 0.0;
        self.status = Status::Ready;
        true
    }

    pub fn update(&mut self, reading: Reading, dt: f32, sensitivity: f32) {
        let Reading::Sample { gravity: [x, y, z], rotation, generation, age } = reading else {
            self.reset();
            self.status = if reading == Reading::Unavailable { Status::Unavailable } else { Status::Waiting };
            return;
        };
        let norm = (x * x + y * y + z * z).sqrt();
        if !norm.is_finite() || !(6.0..=14.0).contains(&norm) || !age.is_finite() || !(0.0..=0.25).contains(&age) || rotation > 3 {
            self.update(Reading::Waiting, dt, sensitivity);
            return;
        }
        // Android sensor axes stay in the device's natural orientation.
        // Positive steering lowers the right-hand edge of the current screen.
        let screen_x = match rotation {
            0 => x,
            1 => -y,
            2 => -x,
            _ => y,
        };
        let angle = (-screen_x / norm).clamp(-1.0, 1.0).asin();
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.1) } else { 0.0 };
        if self.session != Some((rotation, generation)) {
            self.reset();
            self.session = Some((rotation, generation));
            self.neutral = angle;
        }
        self.angle = Some(angle);
        let limit = ROLL_LIMIT.to_radians();
        let roll = self.roll.map_or(angle, |r| r + (angle - r) * (1.0 - (-dt / ROLL_TAU).exp()));
        self.roll = Some(roll.clamp(-limit, limit));
        if self.settle < 0.25 {
            // Average a short, still hold; moving the phone restarts centring.
            if (angle - self.neutral).abs() > 3.0f32.to_radians() {
                self.settle = 0.0;
                self.neutral = angle;
            }
            let span = self.settle + dt;
            self.neutral += (angle - self.neutral) * dt / span.max(0.001);
            self.settle = span;
            self.value = 0.0;
            self.status = Status::Centring;
            return;
        }
        self.status = Status::Ready;
        let gain = if sensitivity.is_finite() { sensitivity.clamp(0.5, 2.0) } else { 1.0 };
        let delta = (angle - self.neutral).to_degrees();
        let target = delta.signum() * ((delta.abs() - 2.0) / (25.0 / gain - 2.0)).clamp(0.0, 1.0);
        self.value += (target - self.value) * (1.0 - (-dt / 0.035).exp());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(deg: f32, rotation: u8, generation: u32) -> Reading {
        let a = deg.to_radians();
        let (x, y) = (-a.sin() * 9.81, a.cos() * 9.81);
        let gravity = match rotation {
            0 => [x, y, 0.0],
            1 => [y, -x, 0.0],
            2 => [-x, -y, 0.0],
            _ => [-y, x, 0.0],
        };
        Reading::Sample { gravity, rotation, generation, age: 0.01 }
    }
    fn hold(s: &mut Steering, deg: f32, rotation: u8, generation: u32, gain: f32) {
        for _ in 0..30 {
            s.update(sample(deg, rotation, generation), 1.0 / 60.0, gain);
        }
    }
    #[test]
    fn steering_direction_is_the_same_in_every_display_rotation() {
        for r in 0..4 {
            let mut s = Steering::default();
            hold(&mut s, 0.0, r, 1, 1.0);
            hold(&mut s, 25.0, r, 1, 1.0);
            assert!(s.steer().unwrap() > 0.99);
            hold(&mut s, -25.0, r, 1, 1.0);
            assert!(s.steer().unwrap() < -0.99);
        }
    }
    #[test]
    fn neutral_grip_dead_zone_and_manual_centring() {
        let mut s = Steering::default();
        hold(&mut s, 10.0, 1, 1, 1.0);
        hold(&mut s, 11.5, 1, 1, 1.0);
        assert_eq!(s.steer(), Some(0.0));
        hold(&mut s, 28.0, 1, 1, 1.0);
        assert!(s.steer().unwrap() > 0.6);
        assert!(s.centre());
        assert_eq!(s.steer(), Some(0.0));
        hold(&mut s, 28.0, 1, 1, 1.0);
        assert_eq!(s.steer(), Some(0.0));
    }
    #[test]
    fn rotation_and_resume_recentre_without_a_steering_kick() {
        let mut s = Steering::default();
        hold(&mut s, 0.0, 1, 1, 1.0);
        hold(&mut s, 25.0, 1, 1, 1.0);
        s.update(sample(-20.0, 3, 1), 0.016, 1.0);
        assert_eq!(s.steer(), None);
        hold(&mut s, -20.0, 3, 1, 1.0);
        assert_eq!(s.steer(), Some(0.0));
        s.update(sample(15.0, 3, 2), 0.016, 1.0);
        assert_eq!(s.steer(), None);
    }
    #[test]
    fn missing_stale_and_invalid_samples_release_steering() {
        let mut s = Steering::default();
        hold(&mut s, 0.0, 1, 1, 1.0);
        for reading in [
            Reading::Unavailable,
            Reading::Waiting,
            Reading::Sample { gravity: [0.0, 9.81, 0.0], rotation: 0, generation: 1, age: 1.0 },
            Reading::Sample { gravity: [f32::NAN, 0.0, 0.0], rotation: 0, generation: 1, age: 0.0 },
            Reading::Sample { gravity: [0.0; 3], rotation: 0, generation: 1, age: 0.0 },
        ] {
            s.update(reading, 0.016, 1.0);
            assert_eq!(s.steer(), None);
            assert!(!s.centre());
        }
    }
    #[test]
    fn roll_is_raw_gravity_angle_smoothed_and_clamped() {
        let mut s = Steering::default();
        assert_eq!(s.roll(), None);
        // The neutral grip is 10 degrees: steering ignores it, the roll must not.
        hold(&mut s, 10.0, 1, 1, 1.0);
        assert!((s.roll().unwrap().to_degrees() - 10.0).abs() < 0.01);
        // One 60 Hz frame moves only part of the way (smoothing)...
        s.update(sample(30.0, 1, 1), 1.0 / 60.0, 1.0);
        let r = s.roll().unwrap().to_degrees();
        assert!(r > 10.0 && r < 30.0, "{r}");
        // ...and it settles on the angle, whatever the screen rotation.
        for rot in 0..4 {
            let mut s = Steering::default();
            hold(&mut s, 0.0, rot, 1, 1.0);
            hold(&mut s, 20.0, rot, 1, 1.0);
            assert!((s.roll().unwrap().to_degrees() - 20.0).abs() < 0.01);
            hold(&mut s, -20.0, rot, 1, 1.0);
            assert!((s.roll().unwrap().to_degrees() + 20.0).abs() < 0.01);
        }
        hold(&mut s, 80.0, 1, 1, 1.0);
        assert!((s.roll().unwrap().to_degrees() - 60.0).abs() < 0.01);
        // Gone with the sensor.
        s.update(Reading::Waiting, 0.016, 1.0);
        assert_eq!(s.roll(), None);
        hold(&mut s, 5.0, 1, 1, 1.0);
        s.reset();
        assert_eq!(s.roll(), None);
    }
    #[test]
    fn higher_sensitivity_reaches_full_lock_sooner_and_filter_is_frame_independent() {
        let mut slow = Steering::default();
        let mut fast = Steering::default();
        hold(&mut slow, 0.0, 0, 1, 0.5);
        hold(&mut fast, 0.0, 0, 1, 2.0);
        hold(&mut slow, 15.0, 0, 1, 0.5);
        hold(&mut fast, 15.0, 0, 1, 2.0);
        assert!(slow.steer().unwrap() < 0.3);
        assert!(fast.steer().unwrap() > 0.99);
        let mut a = Steering::default();
        hold(&mut a, 0.0, 0, 1, 1.0);
        let mut b = Steering::default();
        hold(&mut b, 0.0, 0, 1, 1.0);
        for _ in 0..6 {
            a.update(sample(20.0, 0, 1), 1.0 / 60.0, 1.0);
        }
        for _ in 0..3 {
            b.update(sample(20.0, 0, 1), 1.0 / 30.0, 1.0);
        }
        assert!((a.steer().unwrap() - b.steer().unwrap()).abs() < 1e-5);
    }
}
