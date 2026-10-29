//! The race cameras (SPEC 11): chase, first person, far chase. The chase camera
//! follows the ostrich rigidly in the ground plane and springs everything else
//! (heading, height, distance, FOV), so jumps, drifts and spin-outs stay readable.

use crate::platform::data_dir::CameraView;
use glam::{Quat, Vec3};
use race_core::math::{forward, wrap_angle};
use race_core::Track;
use race_render as rr;
use race_render::{Camera, OstrichPose};

/// Chase rig numbers (metres): behind, up, aim point ahead of and above the ostrich's feet.
/// Look down over the jockey so the ostrich's head and the road ahead stay visible.
/// The feet still sit near the bottom of the screen (about 87% down at speed).
const CHASE_BACK: f32 = 6.5;
const CHASE_UP: f32 = 7.0;
const CHASE_AHEAD: f32 = 10.0;
const CHASE_AIM: f32 = 2.0;
const FAR_BACK: f32 = 11.5;
const FAR_AHEAD: f32 = 14.0;
const FAR_AIM: f32 = 3.8;
/// First person: eye lift above the eye line (m) and upward tilt off the beak axis (degrees).
const FP_EYE_UP: f32 = 0.2;
const FP_PITCH_UP: f32 = 4.0;
/// First person: the view never pitches above this (a ramp jump would show only sky) or below FP_MIN_PITCH.
const FP_MAX_PITCH: f32 = 12.0;
const FP_MIN_PITCH: f32 = -35.0;
/// Bridges: the chase eye stays this far under a deck's underside (m); the deck's
/// slab is this deep below its road surface (matches the renderer's bridge slab);
/// the dip eases down at `DIP_DOWN` per second and back up, slower, at `DIP_UP`.
const DECK_MARGIN: f32 = 0.8;
const DECK_DEPTH: f32 = 1.4;
const DIP_DOWN: f32 = 7.0;
const DIP_UP: f32 = 2.5;
/// Decks closer than this height above the racer's feet are not "overhead" (m).
const DECK_MIN_ABOVE: f32 = 2.5;

fn ease_to(cur: f32, target: f32, rate: f32, dt: f32) -> f32 {
    cur + (target - cur) * (1.0 - (-rate * dt).exp())
}

pub fn next_view(v: CameraView) -> CameraView {
    match v {
        CameraView::Chase => CameraView::FirstPerson,
        CameraView::FirstPerson => CameraView::Far,
        CameraView::Far => CameraView::Chase,
    }
}

pub fn view_name(v: CameraView) -> &'static str {
    match v {
        CameraView::Chase => "CHASE",
        CameraView::FirstPerson => "FIRST PERSON",
        CameraView::Far => "FAR",
    }
}

/// What the rig needs to know about the followed ostrich this frame.
pub struct Follow<'a> {
    pub pose: &'a OstrichPose,
    /// Speed as a fraction of base top speed (0..1.4).
    pub speed_frac: f32,
    pub boosting: bool,
    pub drifting: bool,
    pub spun_out: bool,
    /// Racer is off the map waiting to respawn: freeze the camera.
    pub hold: bool,
    /// Lowest the eye may go (terrain under it plus a margin).
    pub floor_y: f32,
    /// 1 on the grid, easing to 0 after GO: the intro framing.
    pub intro: f32,
    /// Lateral offset on the road (+ right): the grid intro slides the camera
    /// toward the centre line so it is not inside the row behind.
    pub lateral: f32,
    /// Underside of the lowest bridge deck over the racer's road ahead of and
    /// behind it (world y), or `None`: the chase eye dips to stay beneath it.
    /// See [`deck_ceiling`].
    pub ceiling: Option<f32>,
}

/// The lowest deck underside above the stretch of ground from the chase camera's
/// place behind the racer to a little way ahead of it (so the dip starts before the
/// racer is under the deck and lasts until the camera is out from beneath it).
/// `track_idx` is the racer's road sample, `speed` its speed in m/s. Only decks
/// above the racer count: a racer on the deck itself, or climbing onto it, gets
/// `None`.
pub fn deck_ceiling(track: &Track, pose: &OstrichPose, track_idx: usize, speed: f32, far: bool) -> Option<f32> {
    if track.bridges.is_empty() {
        return None;
    }
    let back = if far { FAR_BACK + 2.0 } else { CHASE_BACK + 2.0 };
    let ahead = 8.0 + 0.5 * speed.max(0.0);
    // The racer's own road within this arc is never a ceiling.
    let own_reach = back + ahead + 6.0;
    let own_s = track.samples[track_idx.min(track.n() - 1)].s;
    let fwd = forward(pose.yaw);
    let min_y = pose.pos.y + DECK_MIN_ABOVE;
    let steps = ((back + ahead) / 2.0).ceil() as usize;
    (0..=steps)
        .filter_map(|k| {
            let d = -back + (back + ahead) * k as f32 / steps as f32;
            let p = pose.pos + fwd * d;
            track.deck_surface_above(p.x, p.z, min_y, own_s, own_reach)
        })
        .reduce(f32::min)
        .map(|y| y - DECK_DEPTH)
}

pub struct CameraRig {
    yaw: f32,
    anchor_y: f32,
    back: f32,
    fov: f32,
    fp_fwd: Vec3,
    shake: f32,
    /// How far the chase eye is held below its usual height by a deck overhead (m).
    dip: Option<f32>,
    last: Camera,
    ready: bool,
    view: CameraView,
}

impl Default for CameraRig {
    fn default() -> Self {
        CameraRig { yaw: 0.0, anchor_y: 0.0, back: CHASE_BACK, fov: 70.0, fp_fwd: Vec3::Z, shake: 0.0, dip: None, last: Camera::default(), ready: false, view: CameraView::Chase }
    }
}

impl CameraRig {
    pub fn new() -> CameraRig {
        CameraRig::default()
    }

    /// Forget the spring state (respawn, view change): the next update cuts.
    pub fn snap(&mut self) {
        self.ready = false;
        self.dip = None;
    }

    pub fn add_shake(&mut self, amp: f32) {
        self.shake = (self.shake + amp).min(0.9);
    }

    /// The world-space shake offset for this instant.
    pub fn shake_vec(&self, t: f32) -> Vec3 {
        let a = self.shake;
        Vec3::new((t * 57.0).sin(), (t * 43.0 + 1.3).sin() * 0.8, (t * 49.0 + 2.1).sin()) * a * 0.5
    }

    pub fn update(&mut self, dt: f32, view: CameraView, f: &Follow) -> Camera {
        if view != self.view {
            self.view = view;
            self.ready = false;
            self.dip = None;
        }
        self.shake = ease_to(self.shake, 0.0, 7.0, dt);
        if f.hold && self.ready {
            return self.last;
        }
        let p = f.pose;
        if !self.ready {
            self.yaw = p.yaw;
            self.anchor_y = p.pos.y;
            self.back = if self.view == CameraView::Far { FAR_BACK } else { CHASE_BACK };
            self.fov = 70.0;
            self.fp_fwd = forward(p.yaw);
            self.ready = true;
        }
        let cam = match view {
            CameraView::FirstPerson => self.first_person(dt, f),
            CameraView::Chase | CameraView::Far => self.chase(dt, f, view == CameraView::Far),
        };
        self.last = cam;
        cam
    }

    fn chase(&mut self, dt: f32, f: &Follow, far: bool) -> Camera {
        let p = f.pose;
        // Heading: lazy in a drift (you see the ostrich slide), frozen in a spin-out.
        if !f.spun_out {
            let rate = if f.drifting { 4.2 } else { 9.0 };
            let d = wrap_angle(p.yaw - self.yaw);
            self.yaw = wrap_angle(self.yaw + d * (1.0 - (-rate * dt).exp()));
        }
        // Height springs: up fast enough to follow slopes, softly through a jump.
        let k = if p.pos.y > self.anchor_y { 7.0 } else { 5.0 };
        self.anchor_y = ease_to(self.anchor_y, p.pos.y, k, dt);
        let intro = f.intro.clamp(0.0, 1.0);
        let base_back = if far { FAR_BACK } else { CHASE_BACK };
        let want_back = base_back + f.speed_frac.min(1.0) * if far { 0.8 } else { 0.3 } + if f.boosting { if far { 1.1 } else { 0.7 } } else { 0.0 };
        self.back = ease_to(self.back, want_back, 4.0, dt);
        let fwd = forward(self.yaw);
        let up_h = if far { 5.2 } else { CHASE_UP };
        let ahead = if far { FAR_AHEAD } else { CHASE_AHEAD };
        let anchor = Vec3::new(p.pos.x, self.anchor_y, p.pos.z);
        let side = -f.lateral.clamp(-3.0, 3.0) * intro;
        let rgt = race_core::math::right(self.yaw);
        let intro_back = if far { -0.6 } else { 0.9 };
        let intro_up = if far { 2.2 } else { 0.8 };
        let mut eye = anchor - fwd * (self.back + intro_back * intro) + rgt * side + Vec3::Y * (up_h + intro_up * intro);
        // Under a bridge: duck beneath the deck (quickly going in, slowly coming out).
        let want_dip = f.ceiling.map_or(0.0, |c| (eye.y - (c - DECK_MARGIN)).max(0.0));
        let dip = self.dip.unwrap_or(want_dip);
        let dip = ease_to(dip, want_dip, if want_dip > dip { DIP_DOWN } else { DIP_UP }, dt);
        self.dip = Some(dip);
        eye.y = (eye.y - dip).max(f.floor_y);
        let target = Vec3::new(p.pos.x, 0.0, p.pos.z) + rgt * (side * 0.8) + fwd * ahead + Vec3::Y * (self.anchor_y * 0.6 + p.pos.y * 0.4 + if far { FAR_AIM } else { CHASE_AIM });
        let want_fov = (if far { 62.0 } else { 70.0 }) + 7.0 * f.speed_frac.clamp(0.0, 1.2) + if f.boosting { 5.0 } else { 0.0 };
        self.fov = ease_to(self.fov, want_fov, 4.5, dt);
        Camera { eye, target, up: Vec3::Y, fov_y: self.fov.to_radians(), near: 0.3, far: 700.0 }
    }

    fn first_person(&mut self, dt: f32, f: &Follow) -> Camera {
        // The camera ignores the look-back and bash head turns (the mirror shows
        // the first; the second only swings the visible beak): it looks where the
        // body points.
        let mut pose = *f.pose;
        pose.look_back = 0.0;
        pose.bash = 0.0;
        pose.hide_head = true;
        let fp = rr::first_person_eye(&pose);
        let eyes = rr::eye_positions(&pose);
        // At the eyes (the skull is hidden), a touch above so the beak is below the frame's lower edge.
        // A little higher when slow (the field of view is narrower, so the beak looms larger).
        let lift = FP_EYE_UP + 0.07 * (1.0 - f.speed_frac.clamp(0.0, 1.0));
        let eye = (eyes[0] + eyes[1]) * 0.5 + fp.up * lift * pose.scale;
        self.fp_fwd = self.fp_fwd.lerp(fp.forward, 1.0 - (-20.0 * dt).exp()).normalize_or_zero();
        if self.fp_fwd == Vec3::ZERO {
            self.fp_fwd = fp.forward;
        }
        let up = (fp.up * 0.4 + Vec3::Y * 0.6).normalize();
        // Look a little above the beak axis: only the tip stays in view, at the bottom centre.
        let pitch = FP_PITCH_UP.to_radians();
        let mut look = (self.fp_fwd * pitch.cos() + up * pitch.sin()).normalize();
        let a = look.y.clamp(-1.0, 1.0).asin().clamp(FP_MIN_PITCH.to_radians(), FP_MAX_PITCH.to_radians());
        let flat = Vec3::new(look.x, 0.0, look.z).normalize_or_zero();
        if flat != Vec3::ZERO {
            look = flat * a.cos() + Vec3::Y * a.sin();
        }
        let want_fov = 76.0 + 9.0 * f.speed_frac.clamp(0.0, 1.2) + if f.boosting { 6.0 } else { 0.0 };
        self.fov = ease_to(self.fov, want_fov, 4.5, dt);
        Camera { eye, target: eye + look * 30.0, up, fov_y: self.fov.to_radians(), near: 0.08, far: 700.0 }
    }
}

/// Tilt steering: counter-roll the view about its axis so the horizon stays level
/// with the real ground while the phone rolls like a wheel. `roll` is the phone's
/// roll (radians, + = right edge down); the image turns the opposite way. Rolling
/// `up` by +roll about the view direction (right-hand rule) drops the camera's
/// right-hand side, which lifts the right of the horizon on screen. `None` or a
/// zero roll returns the camera untouched.
pub fn with_roll(mut c: Camera, roll: Option<f32>) -> Camera {
    let Some(roll) = roll.filter(|r| r.is_finite() && *r != 0.0) else { return c };
    let axis = (c.target - c.eye).normalize_or_zero();
    if axis != Vec3::ZERO {
        c.up = Quat::from_axis_angle(axis, roll) * c.up;
    }
    c
}

/// The rear-view mirror camera: at the head, looking back along the road.
pub fn mirror_camera(pose: &OstrichPose) -> Camera {
    let head = rr::head_transform(pose).w_axis.truncate();
    let eye = head + Vec3::Y * 0.15;
    let back = -forward(pose.yaw);
    Camera { eye, target: eye + back * 20.0 + Vec3::Y * 0.4, up: Vec3::Y, fov_y: 60f32.to_radians(), near: 0.2, far: 300.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn follow(pose: &OstrichPose) -> Follow<'_> {
        Follow { pose, speed_frac: 1.0, boosting: false, drifting: false, spun_out: false, hold: false, floor_y: -100.0, intro: 0.0, lateral: 0.0, ceiling: None }
    }

    #[test]
    fn chase_sits_behind_and_above() {
        let mut rig = CameraRig::new();
        let pose = OstrichPose { pos: Vec3::new(10.0, 2.0, 5.0), yaw: 0.0, ..Default::default() };
        let mut c = rig.update(1.0 / 60.0, CameraView::Chase, &follow(&pose));
        for _ in 0..240 {
            c = rig.update(1.0 / 60.0, CameraView::Chase, &follow(&pose));
        }
        assert!(c.eye.z < pose.pos.z - 4.5 && c.eye.y > pose.pos.y + 2.0, "{:?}", c.eye);
        assert!(c.fov_y.to_degrees() > 70.0);
    }

    use glam::Mat4;

    /// Screen height fraction from the top (0..1) of a world point.
    fn screen_y(c: &Camera, p: Vec3) -> f32 {
        let aspect = 20.0 / 9.0;
        let m = Mat4::perspective_rh(c.fov_y, aspect, c.near, c.far) * Mat4::look_at_rh(c.eye, c.target, c.up);
        let q = m * p.extend(1.0);
        (1.0 - q.y / q.w) * 0.5
    }

    #[test]
    fn chase_puts_the_feet_low_and_shows_road_over_the_head() {
        let mut rig = CameraRig::new();
        let pose = OstrichPose { pos: Vec3::new(0.0, 0.0, 0.0), yaw: 0.0, ..Default::default() };
        let mut c = Camera::default();
        for _ in 0..240 {
            c = rig.update(1.0 / 60.0, CameraView::Chase, &follow(&pose));
        }
        let fwd = forward(0.0);
        let feet = screen_y(&c, pose.pos);
        let head = screen_y(&c, pose.pos + Vec3::Y * 2.2);
        let horizon = screen_y(&c, c.eye + fwd * 600.0);
        assert!((0.82..0.92).contains(&feet), "feet at {feet}");
        assert!(head > horizon + 0.05, "head {head} must sit below the horizon {horizon}");
        assert!((0.24..0.35).contains(&horizon), "horizon at {horizon}");
        assert!(c.eye.y - c.target.y > 4.5, "must look down over the jockey");
    }

    #[test]
    fn spin_out_keeps_the_camera_steady() {
        let mut rig = CameraRig::new();
        let mut pose = OstrichPose { yaw: 0.5, ..Default::default() };
        for _ in 0..60 {
            rig.update(1.0 / 60.0, CameraView::Chase, &follow(&pose));
        }
        let before = rig.yaw;
        for i in 0..60 {
            pose.yaw = wrap_angle(0.5 + i as f32 * 0.2);
            let mut f = follow(&pose);
            f.spun_out = true;
            rig.update(1.0 / 60.0, CameraView::Chase, &f);
        }
        assert!((rig.yaw - before).abs() < 1e-4);
    }

    #[test]
    fn drift_lags_the_heading() {
        let mut calm = CameraRig::new();
        let mut drift = CameraRig::new();
        let mut pose = OstrichPose::default();
        for i in 0..60 {
            pose.yaw = wrap_angle(-(i as f32) * 0.03);
            calm.update(1.0 / 60.0, CameraView::Chase, &follow(&pose));
            let mut f = follow(&pose);
            f.drifting = true;
            drift.update(1.0 / 60.0, CameraView::Chase, &f);
        }
        assert!(wrap_angle(pose.yaw - drift.yaw).abs() > wrap_angle(pose.yaw - calm.yaw).abs());
    }

    #[test]
    fn first_person_ignores_bash_and_look_back() {
        let mut a = CameraRig::new();
        let mut b = CameraRig::new();
        let calm = OstrichPose { pos: Vec3::new(1.0, 0.0, 2.0), yaw: 0.3, ..Default::default() };
        let busy = OstrichPose { bash: 1.0, look_back: 1.0, ..calm };
        let (mut ca, mut cb) = (Camera::default(), Camera::default());
        for _ in 0..30 {
            ca = a.update(1.0 / 60.0, CameraView::FirstPerson, &follow(&calm));
            cb = b.update(1.0 / 60.0, CameraView::FirstPerson, &follow(&busy));
        }
        assert!((ca.eye - cb.eye).length() < 1e-4 && (ca.target - cb.target).length() < 1e-3);
    }

    /// Screen position of a world point in pixel-shaped units (x right, y up).
    fn screen_xy(c: &Camera, p: Vec3) -> (f32, f32) {
        let aspect = 20.0 / 9.0;
        let m = Mat4::perspective_rh(c.fov_y, aspect, c.near, c.far) * Mat4::look_at_rh(c.eye, c.target, c.up);
        let q = m * p.extend(1.0);
        (q.x / q.w * aspect, q.y / q.w)
    }

    /// Angle (radians, anticlockwise on screen) of the world horizon seen from `c`.
    fn horizon_angle(c: &Camera, level_dir: Vec3) -> f32 {
        let right = level_dir.cross(Vec3::Y).normalize();
        let mid = c.eye + level_dir * 600.0;
        let (lx, ly) = screen_xy(c, mid - right * 20.0);
        let (rx, ry) = screen_xy(c, mid + right * 20.0);
        (ry - ly).atan2(rx - lx)
    }

    #[test]
    fn roll_keeps_the_horizon_level_with_the_ground() {
        let mut rig = CameraRig::new();
        let pose = OstrichPose::default();
        for view in [CameraView::Chase, CameraView::Far, CameraView::FirstPerson] {
            let mut base = Camera::default();
            for _ in 0..240 {
                base = rig.update(1.0 / 60.0, view, &follow(&pose));
            }
            let dir = Vec3::Z;
            assert!(horizon_angle(&base, dir).abs() < 1e-3);
            for deg in [20.0f32, -35.0, 60.0] {
                // Phone rolled right edge down by `deg`: the world horizon must
                // appear turned the other way on the screen (right end higher),
                // so it is level against the real ground.
                let c = with_roll(base, Some(deg.to_radians()));
                let seen = horizon_angle(&c, dir).to_degrees();
                assert!((seen - deg).abs() < 0.1, "{view:?}: phone {deg}, horizon {seen}");
                // Right-down phone: a point on the screen's right (looking along +Z that is -X) rises.
                let side = Vec3::X;
                let (_, y_left) = screen_xy(&c, c.eye + dir * 100.0 + side * 10.0);
                let (_, y_right) = screen_xy(&c, c.eye + dir * 100.0 - side * 10.0);
                assert_eq!(y_right > y_left, deg > 0.0);
                // Eye, target and field of view are untouched; up stays unit and orthogonal-ish.
                assert_eq!((c.eye, c.target, c.fov_y), (base.eye, base.target, base.fov_y));
                assert!((c.up.length() - 1.0).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn no_roll_leaves_the_camera_identical() {
        let c = Camera { eye: Vec3::new(1.0, 5.0, -3.0), target: Vec3::new(2.0, 1.0, 20.0), ..Camera::default() };
        for r in [None, Some(0.0), Some(f32::NAN)] {
            let o = with_roll(c, r);
            assert_eq!((o.eye, o.target, o.up), (c.eye, c.target, Vec3::Y));
        }
    }

    /// The designer's figure of eight: one bridge, the upper road over the lower.
    fn eight() -> (Track, race_core::Crossing) {
        use race_core::custom;
        let raw: Vec<_> = (0..=160).map(|i| { let a = i as f32 / 160.0 * std::f32::consts::TAU; race_core::Vec2::new(a.cos() * 140.0, a.sin() * a.cos() * 190.0) }).collect();
        let shaped = custom::shape(&raw, false, 18.0).expect("bridge loop");
        let ct = custom::custom_track_from(&shaped, "Figure of Eight", race_core::TrackId::Savanna, 3, 180, custom::auto_fill(&shaped, 7), 7);
        let track = Track::build(&ct.to_def().expect("def"));
        let c = track.crossings[0];
        (track, c)
    }

    /// One step of a drive along `s` of the track at 28 m/s: (racer pose, camera, ceiling).
    fn drive(track: &Track, rig: &mut CameraRig, s: f32, far: bool) -> (OstrichPose, Camera, Option<f32>) {
        let sm = track.sample_at(s);
        let t = sm.tangent;
        let pose = OstrichPose { pos: sm.pos, yaw: race_core::math::yaw_of(Vec3::new(t.x, 0.0, t.z)), ..Default::default() };
        let ceiling = deck_ceiling(track, &pose, track.idx_at(s), 28.0, far);
        let f = Follow { ceiling, ..follow(&pose) };
        let view = if far { CameraView::Far } else { CameraView::Chase };
        let c = rig.update(1.0 / 60.0, view, &f);
        (pose, c, ceiling)
    }

    #[test]
    fn the_chase_camera_ducks_under_a_bridge_and_recovers() {
        let (track, c) = eight();
        let underside = c.y_high - DECK_DEPTH;
        for far in [false, true] {
            let mut rig = CameraRig::new();
            let step = 28.0 / 60.0;
            let start = c.s_low - 140.0;
            let mut s = start;
            let normal = {
                // The unobstructed height, well before the bridge.
                let mut h = 0.0;
                while s < start + 40.0 {
                    h = drive(&track, &mut rig, s, far).1.eye.y;
                    s += step;
                }
                h
            };
            let (mut first_dip, mut racer_under, mut cam_under, mut racer_out, mut recovered) = (None, None, None, None, None);
            let mut lowest = f32::MAX;
            let mut steps = 0;
            while s < c.s_low + 140.0 {
                let (pose, cam, ceiling) = drive(&track, &mut rig, s, far);
                steps += 1;
                let rel = s - c.s_low;
                let own = track.sample_at(s).s;
                let over = |p: Vec3| track.deck_surface_above(p.x, p.z, 2.5, own, 60.0).is_some();
                if first_dip.is_none() && cam.eye.y < normal - 0.1 {
                    first_dip = Some(rel);
                }
                if racer_under.is_none() && over(pose.pos) {
                    racer_under = Some(rel);
                    assert!(cam.eye.y < normal - 0.3, "far={far}: not yet ducking as the racer reaches the deck: {} vs {normal}", cam.eye.y);
                }
                if over(pose.pos) {
                    racer_out = Some(rel);
                }
                if over(cam.eye) {
                    cam_under.get_or_insert(rel);
                    assert!(cam.eye.y < underside - 0.5, "far={far}: eye {} is not under the deck at {underside} (rel {rel})", cam.eye.y);
                }
                if ceiling.is_some() {
                    lowest = lowest.min(cam.eye.y);
                }
                if racer_out.is_some_and(|o| rel > o + 40.0) && recovered.is_none() && (cam.eye.y - normal).abs() < 0.2 {
                    recovered = Some(rel);
                }
                s += step;
            }
            assert!(steps > 100);
            let (fd, ru, cu) = (first_dip.expect("dips"), racer_under.expect("racer under"), cam_under.expect("camera under"));
            assert!(fd < ru - 5.0, "far={far}: starts ducking at {fd}, racer under at {ru}");
            assert!(cu > ru, "far={far}: camera goes under after the racer");
            assert!(lowest < underside - 0.7, "far={far}: lowest eye {lowest}");
            assert!(recovered.expect("recovers") < racer_out.unwrap() + 100.0);
        }
    }

    #[test]
    fn the_chase_camera_does_not_duck_on_the_deck() {
        let (track, c) = eight();
        let mut rig = CameraRig::new();
        let mut s = c.s_high - 150.0;
        let mut ys = Vec::new();
        while s < c.s_high + 150.0 {
            let (pose, cam, ceiling) = drive(&track, &mut rig, s, false);
            assert_eq!(ceiling, None, "deck at s={s}");
            ys.push(cam.eye.y - pose.pos.y);
            s += 28.0 / 60.0;
        }
        // Eye height above the road never drops below its usual level (it rises on the climb).
        assert!(ys.iter().skip(120).all(|&h| h > 5.5), "{:?}", ys.iter().cloned().fold(f32::MAX, f32::min));
    }

    #[test]
    fn view_cycle() {
        assert_eq!(next_view(next_view(next_view(CameraView::Chase))), CameraView::Chase);
    }
}
