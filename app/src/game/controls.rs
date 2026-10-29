//! Touch and keyboard controls (SPEC 12) as pure logic: the button [`Layout`],
//! and a [`Controller`] that turns `Touches` + `Keys` into race-core `Input`
//! plus UI taps. No drawing here (see `hud.rs`), so it is all unit-tested.

use crate::platform::data_dir::SteerMode;
use crate::platform::{KeyCode, Keys, Touch, Touches};
use egui::{pos2, vec2, Pos2, Rect};
use race_core::Input;

/// Horizontal drag, in points, that is full lock.
pub const PAD_FULL: f32 = 70.0;
const PAD_DEAD: f32 = 5.0;
/// Extra reach around a button that still counts as a hit (points).
pub const SLOP: f32 = 8.0;
/// Jump is a big thumb target, nearly Drift's size.
pub const JUMP_R: f32 = 44.0;
/// Two empty-space fingers on the free half look back after this long.
const AUX_HOLD: f32 = 0.15;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Btn {
    Drift,
    Jump,
    Laser,
    Item,
    Bash,
    Brake,
    Go,
    Pause,
    Camera,
    LookBack,
    Left,
    Right,
    Centre,
}

impl Btn {
    pub const ALL: [Btn; 13] = [Btn::Drift, Btn::Jump, Btn::Laser, Btn::Item, Btn::Bash, Btn::Brake, Btn::Go, Btn::Pause, Btn::Camera, Btn::LookBack, Btn::Left, Btn::Right, Btn::Centre];

    fn idx(self) -> usize {
        Btn::ALL.iter().position(|&b| b == self).unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Circle {
    pub c: Pos2,
    pub r: f32,
}

/// Where everything is, in UI points.
#[derive(Clone, Debug)]
pub struct Layout {
    pub safe: Rect,
    pub left_handed: bool,
    /// Floating steer pad: touches that go down here steer.
    pub pad_zone: Rect,
    /// Where the pad's resting hint is drawn.
    pub pad_rest: Pos2,
    /// The other half: two empty fingers here = look back.
    pub aux_zone: Rect,
    buttons: Vec<(Btn, Circle)>,
}

impl Layout {
    pub fn new(safe: Rect, left_handed: bool, steer: SteerMode, _auto_accel: bool) -> Layout {
        let (l, r, t, b) = (safe.left(), safe.right(), safe.top(), safe.bottom());
        let w = safe.width();
        let mx = |x: f32| if left_handed { l + r - x } else { x };
        let mut buttons: Vec<(Btn, Circle)> = Vec::new();
        let mut add = |btn: Btn, x: f32, y: f32, rad: f32, mirrored: bool| {
            buttons.push((btn, Circle { c: pos2(if mirrored { mx(x) } else { x }, y), r: rad }));
        };
        // Right thumb cluster (mirrors when left handed).
        add(Btn::Drift, r - 62.0, b - 66.0, 46.0, true);
        add(Btn::Jump, r - 195.0, b - 46.0, JUMP_R, true);
        add(Btn::Item, r - 144.0, b - 123.0, 31.0, true);
        add(Btn::Laser, r - 79.0, b - 164.0, 33.0, true);
        if steer == SteerMode::Buttons {
            add(Btn::Left, l + 70.0, b - 62.0, 44.0, true);
            add(Btn::Right, l + 182.0, b - 62.0, 44.0, true);
        }
        if steer == SteerMode::Tilt { add(Btn::Centre, l + 132.0, b - 151.0, 24.0, true); }
        // Top bar (not mirrored).
        add(Btn::Pause, r - 26.0, t + 26.0, 19.0, false);
        add(Btn::Camera, r - 74.0, t + 26.0, 19.0, false);
        add(Btn::LookBack, r - 122.0, t + 26.0, 19.0, false);
        let pad_zone = if left_handed { Rect::from_min_max(pos2(r - w * 0.4, t), pos2(r, b)) } else { Rect::from_min_max(pos2(l, t), pos2(l + w * 0.4, b)) };
        let aux_zone = if left_handed { Rect::from_min_max(pos2(l, t), pos2(l + w * 0.5, b)) } else { Rect::from_min_max(pos2(l + w * 0.5, t), pos2(r, b)) };
        Layout { safe, left_handed, pad_zone, pad_rest: pos2(mx(l + 132.0), b - 70.0), aux_zone, buttons }
    }

    pub fn circle(&self, b: Btn) -> Option<Circle> {
        self.buttons.iter().find(|(x, _)| *x == b).map(|(_, c)| *c)
    }

    /// Ordinary races get laser eyes from an item, using the Item button.
    pub fn without_laser(mut self) -> Self {
        self.buttons.retain(|(b, _)| *b != Btn::Laser);
        self
    }

    /// Steering and a large Jump button while the jockey is on foot.
    pub fn running(mut self) -> Self {
        let jump = self.circle(Btn::Drift);
        self.buttons.retain(|(b, _)| {
            matches!(
                b,
                Btn::Jump | Btn::Pause | Btn::Camera | Btn::Left | Btn::Right | Btn::Centre
            )
        });
        if let Some(c) = jump {
            if let Some((_, circle)) = self.buttons.iter_mut().find(|(b, _)| *b == Btn::Jump) {
                *circle = c;
            }
        }
        self
    }

    pub fn buttons(&self) -> impl Iterator<Item = (Btn, Circle)> + '_ {
        self.buttons.iter().copied()
    }

    /// The button under a finger (nearest centre relative to radius, with slop).
    pub fn hit(&self, p: Pos2) -> Option<Btn> {
        let mut best: Option<(f32, Btn)> = None;
        for (b, c) in &self.buttons {
            let d = (p - c.c).length();
            if d <= c.r + SLOP {
                let score = d / c.r;
                if best.is_none_or(|(s, _)| score < s) {
                    best = Some((score, *b));
                }
            }
        }
        best.map(|(_, b)| b)
    }
}

/// Nearby opponents, retained for control API compatibility.
#[derive(Clone, Copy, Debug, Default)]
pub struct Adjacent {
    pub left: bool,
    pub right: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Claim {
    Pad,
    Btn(Btn),
    Aux,
}

/// The settings that shape the mapping.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub steer_mode: SteerMode,
    pub auto_accel: bool,
}

/// What the controls want this frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Out {
    /// Held state (buttons as levels). Edge buttons also get one-tick pulses via [`Controller::next_tick_input`].
    pub held: Input,
    pub pause: bool,
    pub camera: bool,
    pub centre: bool,
    /// A press this frame (any button) for UI feedback sounds.
    pub flick_bash: bool,
}

pub struct Controller {
    claims: Vec<(u64, Claim)>,
    pad_center: Option<Pos2>,
    steer: f32,
    tilt: Option<f32>,
    pad_knob: Option<Pos2>,
    aux_t: f32,
    latch_jump: bool,
    latch_item: bool,
    latch_bash: bool,
    rev: bool,
    /// After `release_all`: the next update re-adopts fingers that are already
    /// down (a thumb resting on the pad steers again without being re-pressed).
    reclaim: bool,
    /// The finger that tapped Pause is not re-adopted.
    reclaim_skip: Vec<u64>,
    /// Smoothed 0..1 press animation per button (drawing).
    pub press: [f32; 13],
    held: Input,
}

impl Default for Controller {
    fn default() -> Self {
        Controller {
            claims: Vec::new(),
            pad_center: None,
            steer: 0.0,
            tilt: None,
            pad_knob: None,
            aux_t: 0.0,
            latch_jump: false,
            latch_item: false,
            latch_bash: false,
            rev: false,
            reclaim: false,
            reclaim_skip: Vec::new(),
            press: [0.0; 13],
            held: Input::default(),
        }
    }
}

/// Pad drag in points -> steer -1..1: dead zone, full lock at [`PAD_FULL`], a mild expo.
pub fn pad_curve(dx: f32) -> f32 {
    let a = ((dx.abs() - PAD_DEAD) / (PAD_FULL - PAD_DEAD)).clamp(0.0, 1.0);
    dx.signum() * a * (0.6 + 0.4 * a)
}

fn approach(cur: f32, target: f32, step: f32) -> f32 {
    if (target - cur).abs() <= step {
        target
    } else {
        cur + step * (target - cur).signum()
    }
}

impl Controller {
    pub fn new() -> Controller {
        Controller::default()
    }

    /// Drop all fingers (pause, screen change).
    pub fn release_all(&mut self) {
        self.reclaim_skip = self.claims.iter().filter(|(_, c)| *c == Claim::Btn(Btn::Pause)).map(|(id, _)| *id).collect();
        self.reclaim = true;
        self.latch_jump = false;
        self.latch_item = false;
        self.latch_bash = false;
        self.rev = false;
        self.claims.clear();
        self.pad_center = None;
        self.pad_knob = None;
        self.aux_t = 0.0;
        self.held = Input::default();
        self.steer = 0.0;
        self.tilt = None;
    }

    /// Where the pad is anchored and where the thumb is (drawing).
    pub fn pad_state(&self) -> Option<(Pos2, Pos2)> {
        Some((self.pad_center?, self.pad_knob?))
    }

    /// The player pressed inside the start-boost window (auto-accelerate rev).
    pub fn revved(&self) -> bool {
        self.rev
    }

    /// Smoothed steer, -1..1.
    pub fn steer(&self) -> f32 {
        self.steer
    }

    pub fn set_tilt(&mut self, steer: Option<f32>) { self.tilt = steer.filter(|s| s.is_finite()).map(|s| s.clamp(-1.0, 1.0)); }
    pub fn tilt_ready(&self) -> bool { self.tilt.is_some() }

    /// Is `b` held right now (drawing).
    pub fn is_held(&self, b: Btn) -> bool {
        self.claims.iter().any(|(_, c)| *c == Claim::Btn(b))
    }

    /// `countdown_left`: Some(seconds) while the grid countdown runs.
    #[allow(clippy::too_many_arguments)]
    pub fn update(&mut self, dt: f32, _now: f64, touches: &Touches, keys: &Keys, layout: &Layout, cfg: Config, _adj: Adjacent, countdown_left: Option<f32>, start_window: f32) -> Out {
        let mut out = Out::default();
        // Claims of fingers that lifted go away.
        self.claims.retain(|(id, _)| touches.get(*id).is_some());
        if self.pad_center.is_some() && !self.claims.iter().any(|(_, c)| *c == Claim::Pad) {
            self.pad_center = None;
            self.pad_knob = None;
            }
        let mut any_press = false;
        // Fingers already down after a pause: claim them (no tap effects).
        let resting: Vec<Touch> = if std::mem::take(&mut self.reclaim) {
            touches.active().iter().filter(|t| !self.reclaim_skip.contains(&t.id) && !touches.began().iter().any(|b| b.id == t.id)).cloned().collect()
        } else {
            Vec::new()
        };
        self.reclaim_skip.clear();
        for (is_rest, t) in resting.iter().map(|t| (true, t)).chain(touches.began().iter().map(|t| (false, t))) {
            let hit = layout.hit(t.pos);
            if !is_rest && !matches!(hit, Some(Btn::Pause | Btn::Camera | Btn::LookBack | Btn::Centre)) {
                any_press = true;
            }
            let active = touches.get(t.id).is_some();
            if let Some(b) = hit {
                // A finger resting on a button needs a fresh press (no surprise jump / item on resume).
                if is_rest {
                    continue;
                }
                if active {
                    self.claims.push((t.id, Claim::Btn(b)));
                }
                match b {
                    Btn::Jump => self.latch_jump = true,
                    Btn::Item => self.latch_item = true,
                    Btn::Bash => {},
                    Btn::Pause => out.pause = true,
                    Btn::Camera => out.camera = true,
                    Btn::Centre => out.centre = true,
                    _ => {}
                }
            } else if active && cfg.steer_mode != SteerMode::Buttons && layout.pad_zone.contains(t.pos) && !self.claims.iter().any(|(_, c)| *c == Claim::Pad) {
                self.claims.push((t.id, Claim::Pad));
                self.pad_center = Some(t.pos);
                self.pad_knob = Some(t.pos);
                    } else if active && layout.aux_zone.contains(t.pos) {
                self.claims.push((t.id, Claim::Aux));
            }
        }
        // Pad: floating centre that follows an over-drag, so reversing is instant.
        let mut pad_raw = 0.0;
        if let (Some(id), Some(centre)) = (self.claims.iter().find(|(_, c)| *c == Claim::Pad).map(|(i, _)| *i), self.pad_center) {
            if let Some(t) = touches.get(id) {
                let mut c = centre;
                let dx = t.pos.x - c.x;
                if dx.abs() > PAD_FULL {
                    c.x = t.pos.x - dx.signum() * PAD_FULL;
                    self.pad_center = Some(c);
                }
                self.pad_knob = Some(t.pos);
                pad_raw = pad_curve(t.pos.x - c.x);

            }
        }
        // Held buttons.
        let held_btn = |this: &Controller, b: Btn| this.claims.iter().any(|(_, c)| *c == Claim::Btn(b));
        let key = |ks: &[KeyCode]| keys.any_held(ks);
        let kp = |ks: &[KeyCode]| ks.iter().any(|&k| keys.pressed(k));
        if kp(&[KeyCode::Space]) {
            self.latch_jump = true;
        }
        if kp(&[KeyCode::KeyK]) {
            self.latch_item = true;
        }
        if kp(&[KeyCode::KeyC]) {
            out.camera = true;
        }
        if kp(&[KeyCode::KeyP]) {
            out.pause = true;
        }
        any_press |= keys_any_pressed(keys);

        // Steering target from buttons, keys or pad.
        let kb = (key(&[KeyCode::ArrowRight, KeyCode::KeyD]) as i32 - key(&[KeyCode::ArrowLeft, KeyCode::KeyA]) as i32) as f32;
        let bt = (held_btn(self, Btn::Right) as i32 - held_btn(self, Btn::Left) as i32) as f32;
        let step_rate = |cur: f32, target: f32| if target == 0.0 || target.signum() != cur.signum() && cur != 0.0 { 14.0 } else { 10.0 };
        if kb != 0.0 || bt != 0.0 {
            let target = if kb != 0.0 { kb } else { bt };
            self.steer = approach(self.steer, target, step_rate(self.steer, target) * dt);
        } else if self.pad_center.is_some() {
            // Smoothed but responsive: ~22 ms time constant.
            self.steer += (pad_raw - self.steer) * (1.0 - (-dt / 0.022).exp());
        } else if cfg.steer_mode == SteerMode::Tilt && self.tilt.is_some() {
            self.steer = self.tilt.unwrap_or(0.0);
        } else {
            self.steer = approach(self.steer, 0.0, 14.0 * dt);
        }
        if out.centre { self.steer = 0.0; }

        // Look back: button, key, or two empty fingers on the free half.
        let aux = self.claims.iter().filter(|(_, c)| *c == Claim::Aux).count();
        self.aux_t = if aux >= 2 { self.aux_t + dt } else { 0.0 };
        let look_back = held_btn(self, Btn::LookBack) || key(&[KeyCode::KeyB]) || self.aux_t >= AUX_HOLD;

        // A countdown tap can still earn a start boost. Racing is always full throttle.
        let throttle = if let Some(left) = countdown_left {
            if any_press && left <= start_window { self.rev = true; }
            if self.rev { 1.0 } else { 0.0 }
        } else { self.rev = false; 1.0 };

        self.held = Input {
            steer: self.steer.clamp(-1.0, 1.0),
            throttle,
            brake: false,
            drift: held_btn(self, Btn::Drift) || key(&[KeyCode::ShiftLeft, KeyCode::ShiftRight]),
            jump: held_btn(self, Btn::Jump) || key(&[KeyCode::Space]),
            laser: held_btn(self, Btn::Laser) || key(&[KeyCode::KeyJ]),
            item: held_btn(self, Btn::Item) || key(&[KeyCode::KeyK]),
            bash: false,
            look_back,
        };
        out.held = self.held;

        // Press animations (for drawing): quick down, a bit slower up.
        for b in Btn::ALL {
            let on = held_btn(self, b) || match b {
                Btn::Drift => self.held.drift,
                Btn::Laser => self.held.laser,
                Btn::LookBack => look_back,
                Btn::Brake => self.held.brake,
                Btn::Jump => self.held.jump,
                Btn::Item => self.held.item,
                Btn::Bash => self.held.bash,
                _ => false,
            };
            let p = &mut self.press[b.idx()];
            *p = approach(*p, if on { 1.0 } else { 0.0 }, dt * if on { 14.0 } else { 7.0 });
        }
        out
    }

    /// The input for one 60 Hz tick. Taps (jump, item, bash) are delivered as
    /// a pulse on the first tick after they happened, even if the finger is
    /// already up.
    pub fn next_tick_input(&mut self) -> Input {
        let mut i = self.held;
        if std::mem::take(&mut self.latch_jump) {
            i.jump = true;
        }
        if std::mem::take(&mut self.latch_item) {
            i.item = true;
        }
        self.latch_bash = false;
        i.bash = false;
        i
    }

    /// Press animation of a button (0 up, 1 fully pressed).
    pub fn pressed_amount(&self, b: Btn) -> f32 {
        self.press[b.idx()]
    }
}

fn keys_any_pressed(keys: &Keys) -> bool {
    use KeyCode::*;
    [ArrowUp, ArrowDown, ArrowLeft, ArrowRight, KeyW, KeyA, KeyS, KeyD, Space, ShiftLeft, ShiftRight, KeyJ, KeyK].iter().any(|&k| keys.pressed(k))
}

/// Notices a racer pinned against a wall and shows a steering hint.
#[derive(Default, Debug)]
pub struct Unstick { stuck: f32 }

/// Seconds stopped before showing the steering hint.
pub const STUCK_HINT: f32 = 1.0;

impl Unstick {
    /// Ignore countdown, deliberate pauses and gameplay recovery animations.
    pub fn update(&mut self, dt: f32, speed: f32, moving_ok: bool, _auto_accel: bool) {
        if moving_ok && speed.abs() < 1.5 { self.stuck += dt; }
        else { self.stuck = 0.0; }
    }
    /// Show the steering hint after a sustained stop.
    pub fn hint(&self) -> bool { self.stuck >= STUCK_HINT }
}

/// Is an opponent beside the racer, on which side? `others`: (lateral offset in
/// the racer's frame, + = right; longitudinal offset, + = ahead), metres.
pub fn adjacent_sides(others: impl IntoIterator<Item = (f32, f32)>) -> Adjacent {
    let mut a = Adjacent::default();
    for (lat, long) in others {
        if long.abs() < 3.0 && lat.abs() > 0.5 && lat.abs() < 4.5 {
            if lat > 0.0 {
                a.right = true;
            } else {
                a.left = true;
            }
        }
    }
    a
}

#[allow(unused)]
fn _sz() -> egui::Vec2 {
    vec2(0.0, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::TouchPhase;

    const DT: f32 = 1.0 / 60.0;

    fn safe() -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(800.0, 360.0))
    }

    fn cfg() -> Config {
        Config { steer_mode: SteerMode::Pad, auto_accel: true }
    }

    #[test]
    fn tilt_keeps_its_direction_when_buttons_are_mirrored_and_pad_takes_priority() {
        for mirrored in [false, true] {
            let mut c = Controller::new();
            let l = Layout::new(safe(), mirrored, SteerMode::Tilt, true);
            let cfg = Config { steer_mode: SteerMode::Tilt, auto_accel: true };
            let mut touches = Touches::new();
            let keys = Keys::default();
            c.set_tilt(Some(0.8));
            assert_eq!(c.update(DT, 0.0, &touches, &keys, &l, cfg, Adjacent::default(), None, 0.4).held.steer, 0.8);
            let p = l.pad_rest;
            touches.touch(1, TouchPhase::Started, p);
            c.update(DT, 0.0, &touches, &keys, &l, cfg, Adjacent::default(), None, 0.4);
            touches.end_frame(DT);
            touches.touch(1, TouchPhase::Moved, p - vec2(70.0, 0.0));
            for _ in 0..20 { c.update(DT, 0.1, &touches, &keys, &l, cfg, Adjacent::default(), None, 0.4); }
            assert!(c.next_tick_input().steer < -0.99);
            touches.touch(1, TouchPhase::Ended, p);
            assert_eq!(c.update(DT, 1.0, &touches, &keys, &l, cfg, Adjacent::default(), None, 0.4).held.steer, 0.8);
        }
    }

    #[test]
    fn centre_is_a_ui_tap_and_cannot_spend_an_item_or_rev_the_start() {
        let mut c = Controller::new();
        let l = Layout::new(safe(), false, SteerMode::Tilt, true);
        let mut touches = Touches::new();
        touches.touch(1, TouchPhase::Started, l.circle(Btn::Centre).unwrap().c);
        c.set_tilt(Some(1.0));
        let out = c.update(DT, 0.0, &touches, &Keys::default(), &l, Config { steer_mode: SteerMode::Tilt, auto_accel: true }, Adjacent::default(), Some(0.2), 0.4);
        assert!(out.centre); assert_eq!(out.held.steer, 0.0); assert!(!c.revved());
        let tick = c.next_tick_input(); assert!(!tick.jump && !tick.item && !tick.bash);
        c.release_all(); assert_eq!(c.next_tick_input().steer, 0.0);
    }

    struct Rig {
        c: Controller,
        t: Touches,
        k: Keys,
        l: Layout,
        now: f64,
        adj: Adjacent,
    }

    impl Rig {
        fn new(left_handed: bool) -> Rig {
            Rig { c: Controller::new(), t: Touches::new(), k: Keys::default(), l: Layout::new(safe(), left_handed, SteerMode::Pad, true), now: 0.0, adj: Adjacent::default() }
        }
        fn frame(&mut self) -> Out {
            self.t.time = self.now;
            let o = self.c.update(DT, self.now, &self.t, &self.k, &self.l, cfg(), self.adj, None, 0.4);
            self.t.end_frame(DT);
            self.k.end_frame();
            self.now += DT as f64;
            o
        }
    }

    #[test]
    fn pad_steers_with_a_floating_centre() {
        let mut r = Rig::new(false);
        r.t.touch(1, TouchPhase::Started, pos2(100.0, 200.0));
        r.frame();
        r.t.touch(1, TouchPhase::Moved, pos2(170.0, 200.0));
        let mut o = r.frame();
        for _ in 0..20 {
            o = r.frame();
        }
        assert!(o.held.steer > 0.98, "full lock right, got {}", o.held.steer);
        // Drag further: the centre follows, so coming back 70 pt is full lock the other way.
        r.t.touch(1, TouchPhase::Moved, pos2(260.0, 200.0));
        r.frame();
        r.t.touch(1, TouchPhase::Moved, pos2(190.0, 200.0));
        for _ in 0..20 {
            o = r.frame();
        }
        assert!(o.held.steer < 0.05, "centre followed the over-drag, got {}", o.held.steer);
        r.t.touch(1, TouchPhase::Ended, pos2(190.0, 200.0));
        for _ in 0..30 {
            o = r.frame();
        }
        assert!(o.held.steer.abs() < 1e-3);
    }

    #[test]
    fn a_resting_thumb_steers_again_after_release_all() {
        let mut r = Rig::new(false);
        r.t.touch(1, TouchPhase::Started, pos2(100.0, 200.0));
        r.frame();
        r.t.touch(1, TouchPhase::Moved, pos2(170.0, 200.0));
        let mut o = r.frame();
        for _ in 0..20 {
            o = r.frame();
        }
        assert!(o.held.steer > 0.9);
        // Pause and resume with the thumb still down.
        r.c.release_all();
        r.frame();
        for _ in 0..30 {
            o = r.frame();
        }
        assert!(o.held.steer.abs() < 0.2, "re-anchored at the resting spot, got {}", o.held.steer);
        r.t.touch(1, TouchPhase::Moved, pos2(240.0, 200.0));
        for _ in 0..20 {
            o = r.frame();
        }
        assert!(o.held.steer > 0.9, "the resting thumb steers without a re-press, got {}", o.held.steer);
    }

    #[test]
    fn release_all_drops_pending_taps_and_the_pause_finger_is_not_reclaimed() {
        let mut r = Rig::new(false);
        let item = r.l.buttons.iter().find(|b| b.0 == Btn::Item).map(|b| b.1.c).unwrap();
        let pause = r.l.buttons.iter().find(|b| b.0 == Btn::Pause).map(|b| b.1.c).unwrap();
        r.t.touch(1, TouchPhase::Started, item);
        r.t.touch(2, TouchPhase::Started, pause);
        let o = r.frame();
        assert!(o.pause);
        r.c.release_all();
        // Finger 2 (the Pause tap) stays down; nothing may fire after the resume.
        r.frame();
        let i = r.c.next_tick_input();
        assert!(!i.item, "latched item tap must not survive a pause");
        assert!(!r.c.is_held(Btn::Pause));
    }

    #[test]
    fn pause_camera_taps_do_not_rev_the_start() {
        let mut r = Rig::new(false);
        let pause = r.l.buttons.iter().find(|b| b.0 == Btn::Pause).map(|b| b.1.c).unwrap();
        r.t.touch(1, TouchPhase::Started, pause);
        r.t.time = r.now;
        let _ = r.c.update(DT, r.now, &r.t, &r.k, &r.l, cfg(), r.adj, Some(0.2), 0.4);
        assert!(!r.c.revved());
    }

    #[test]
    fn pad_has_a_dead_zone_and_is_symmetric() {
        assert_eq!(pad_curve(3.0), 0.0);
        assert!((pad_curve(70.0) - 1.0).abs() < 1e-6);
        assert!((pad_curve(-70.0) + 1.0).abs() < 1e-6);
        assert!(pad_curve(35.0) > 0.2 && pad_curve(35.0) < 0.5);
    }

    #[test]
    fn right_side_touches_never_steer() {
        let mut r = Rig::new(false);
        r.t.touch(1, TouchPhase::Started, pos2(500.0, 100.0));
        r.frame();
        r.t.touch(1, TouchPhase::Moved, pos2(570.0, 100.0));
        let o = r.frame();
        assert_eq!(o.held.steer, 0.0);
    }

    #[test]
    fn flicks_only_steer_even_with_a_neighbour() {
        let mut r = Rig::new(false);
        r.adj = Adjacent { left: false, right: true };
        r.t.touch(1, TouchPhase::Started, pos2(100.0, 200.0));
        r.frame();
        // 60 pt in 3 frames = 1200 pt/s to the right.
        for i in 1..=3 {
            r.t.touch(1, TouchPhase::Moved, pos2(100.0 + 20.0 * i as f32, 200.0));
            r.frame();
        }
        assert!(!r.c.next_tick_input().bash, "flicks never bash");
        // The same flick to the left, no one there.
        let mut r = Rig::new(false);
        r.adj = Adjacent { left: false, right: true };
        r.t.touch(1, TouchPhase::Started, pos2(200.0, 200.0));
        r.frame();
        for i in 1..=3 {
            r.t.touch(1, TouchPhase::Moved, pos2(200.0 - 20.0 * i as f32, 200.0));
            r.frame();
        }
        assert!(!r.c.next_tick_input().bash);
        // A slow drag never bashes.
        let mut r = Rig::new(false);
        r.adj = Adjacent { left: true, right: true };
        r.t.touch(1, TouchPhase::Started, pos2(100.0, 200.0));
        r.frame();
        for i in 1..=30 {
            r.t.touch(1, TouchPhase::Moved, pos2(100.0 + 2.0 * i as f32, 200.0));
            r.frame();
        }
        assert!(!r.c.next_tick_input().bash);
    }

    #[test]
    fn left_handed_mirrors_the_cluster_but_not_the_top_bar() {
        let a = Layout::new(safe(), false, SteerMode::Pad, true);
        let b = Layout::new(safe(), true, SteerMode::Pad, true);
        let (da, db) = (a.circle(Btn::Drift).unwrap(), b.circle(Btn::Drift).unwrap());
        assert!((da.c.x + db.c.x - 800.0).abs() < 1e-3 && da.c.y == db.c.y);
        assert!(da.c.x > 400.0 && db.c.x < 400.0);
        assert_eq!(a.circle(Btn::Pause), b.circle(Btn::Pause));
        assert!(a.pad_zone.right() <= 321.0 && b.pad_zone.left() >= 479.0);
        assert!(b.pad_rest.x > 400.0);
    }

    #[test]
    fn left_handed_steering_pad_is_on_the_right() {
        let mut r = Rig::new(true);
        r.t.touch(1, TouchPhase::Started, pos2(700.0, 200.0));
        r.frame();
        r.t.touch(1, TouchPhase::Moved, pos2(630.0, 200.0));
        let mut o = r.frame();
        for _ in 0..20 {
            o = r.frame();
        }
        assert!(o.held.steer < -0.95);
    }

    #[test]
    fn button_hit_testing_prefers_the_closest_and_has_slop() {
        let l = Layout::new(safe(), false, SteerMode::Pad, true);
        let d = l.circle(Btn::Drift).unwrap();
        assert_eq!(l.hit(d.c), Some(Btn::Drift));
        assert_eq!(l.hit(d.c + vec2(d.r + 4.0, 0.0)), Some(Btn::Drift), "inside the slop");
        assert_eq!(l.hit(pos2(400.0, 100.0)), None, "the road is not a button");
        // Every button is reachable at its centre and none overlap.
        let all: Vec<_> = l.buttons().collect();
        for (b, c) in &all {
            assert_eq!(l.hit(c.c), Some(*b));
        }
        for (i, (_, a)) in all.iter().enumerate() {
            for (_, b) in &all[i + 1..] {
                assert!((a.c - b.c).length() > a.r + b.r, "{a:?} overlaps {b:?}");
            }
        }
        assert!(l.circle(Btn::Bash).is_none(), "bash has no touch control");
        assert!(l.circle(Btn::Go).is_none(), "no Go pedal with auto-accelerate");
        let manual = Layout::new(safe(), false, SteerMode::Pad, false);
        assert!(manual.circle(Btn::Go).is_none());
        assert!(manual.circle(Btn::Brake).is_none(), "no throttle or brake pedals in any saved control mode");
    }

    #[test]
    fn jump_is_big_and_clear_of_every_other_button_in_all_modes() {
        for (w, h) in [(800.0, 360.0), (914.0, 411.0)] {
            let s = Rect::from_min_max(pos2(0.0, 0.0), pos2(w, h));
            for left in [false, true] {
                for steer in [SteerMode::Pad, SteerMode::Buttons, SteerMode::Tilt] {
                    for auto in [false, true] {
                        let l = Layout::new(s, left, steer, auto);
                        let j = l.circle(Btn::Jump).unwrap();
                        assert!(j.r >= 42.0, "jump radius {} too small", j.r);
                        assert!(j.c.x - j.r >= 0.0 && j.c.x + j.r <= w && j.c.y - j.r >= 0.0 && j.c.y + j.r <= h, "{j:?} off a {w}x{h} screen");
                        for (b, c) in l.buttons().filter(|(b, _)| *b != Btn::Jump) {
                            // Clear even counting each button's touch slop on one side.
                            let gap = (j.c - c.c).length() - j.r - c.r;
                            assert!(gap > 2.0 * SLOP, "jump too close to {b:?} ({gap}) left={left} {steer:?} auto={auto}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn layout_fits_the_safe_area_on_both_phones() {
        for (w, h) in [(800.0, 360.0), (914.0, 411.0)] {
            for left in [false, true] {
                let s = Rect::from_min_max(pos2(0.0, 0.0), pos2(w, h));
                let l = Layout::new(s, left, SteerMode::Buttons, false);
                for (b, c) in l.buttons() {
                    assert!(c.c.x - c.r >= 0.0 && c.c.x + c.r <= w && c.c.y - c.r >= 0.0 && c.c.y + c.r <= h, "{b:?} {c:?} off a {w}x{h} screen");
                }
            }
        }
    }

    #[test]
    fn a_tap_on_jump_between_ticks_is_not_lost() {
        let mut r = Rig::new(false);
        let j = r.l.circle(Btn::Jump).unwrap().c;
        r.t.touch(5, TouchPhase::Started, j);
        r.t.touch(5, TouchPhase::Ended, j);
        let o = r.frame();
        assert!(!o.held.jump, "the finger is already up");
        assert!(r.c.next_tick_input().jump, "but the tick sees a pulse");
        assert!(!r.c.next_tick_input().jump, "once");
    }

    #[test]
    fn holding_drift_and_laser_with_two_thumbs() {
        let mut r = Rig::new(false);
        let (d, l) = (r.l.circle(Btn::Drift).unwrap().c, r.l.circle(Btn::Laser).unwrap().c);
        r.t.touch(1, TouchPhase::Started, d);
        r.t.touch(2, TouchPhase::Started, l);
        r.t.touch(3, TouchPhase::Started, pos2(100.0, 250.0));
        let o = r.frame();
        assert!(o.held.drift && o.held.laser);
        r.t.touch(1, TouchPhase::Ended, d);
        let o = r.frame();
        assert!(!o.held.drift && o.held.laser, "lifting one finger leaves the others");
    }

    #[test]
    fn a_finger_that_slides_onto_a_button_does_not_press_it() {
        let mut r = Rig::new(false);
        r.t.touch(1, TouchPhase::Started, pos2(450.0, 200.0));
        r.frame();
        r.t.touch(1, TouchPhase::Moved, r.l.circle(Btn::Drift).unwrap().c);
        assert!(!r.frame().held.drift);
    }

    #[test]
    fn two_fingers_on_the_free_half_look_back() {
        let mut r = Rig::new(false);
        r.t.touch(1, TouchPhase::Started, pos2(450.0, 120.0));
        r.t.touch(2, TouchPhase::Started, pos2(500.0, 140.0));
        let mut o = r.frame();
        assert!(!o.held.look_back, "needs a short hold");
        for _ in 0..12 {
            o = r.frame();
        }
        assert!(o.held.look_back);
        r.t.touch(2, TouchPhase::Ended, pos2(500.0, 140.0));
        assert!(!r.frame().held.look_back);
    }

    #[test]
    fn keyboard_steer_ramps_and_keys_map() {
        let mut r = Rig::new(false);
        r.k.key(KeyCode::ArrowRight, true);
        let o = r.frame();
        assert!(o.held.steer > 0.0 && o.held.steer < 0.3, "ramps, not a snap: {}", o.held.steer);
        for _ in 0..20 {
            r.frame();
        }
        assert!((r.c.steer() - 1.0).abs() < 1e-3);
        r.k.key(KeyCode::ArrowRight, false);
        for _ in 0..20 {
            r.frame();
        }
        assert_eq!(r.c.steer(), 0.0);
        r.k.key(KeyCode::ShiftLeft, true);
        r.k.key(KeyCode::KeyJ, true);
        r.k.key(KeyCode::KeyB, true);
        r.k.key(KeyCode::ArrowDown, true);
        let o = r.frame();
        assert!(o.held.drift && o.held.laser && o.held.look_back && !o.held.brake);
        r.k.key(KeyCode::Space, true);
        r.frame();
        assert!(r.c.next_tick_input().jump);
        r.k.key(KeyCode::KeyC, true);
        assert!(r.frame().camera);
    }

    #[test]
    fn pause_and_camera_buttons_fire_on_press() {
        let mut r = Rig::new(false);
        let p = r.l.circle(Btn::Pause).unwrap().c;
        r.t.touch(1, TouchPhase::Started, p);
        assert!(r.frame().pause);
        let c = r.l.circle(Btn::Camera).unwrap().c;
        r.t.touch(2, TouchPhase::Started, c);
        assert!(r.frame().camera);
    }

    #[test]
    fn start_boost_press_inside_the_window_only() {
        let mut c = Controller::new();
        let (t, k, l) = (Touches::new(), Keys::default(), Layout::new(safe(), false, SteerMode::Pad, true));
        let o = c.update(DT, 0.0, &t, &k, &l, cfg(), Adjacent::default(), Some(2.0), 0.38);
        assert_eq!(o.held.throttle, 0.0, "auto-accelerate waits for the start");
        let mut t2 = Touches::new();
        t2.touch(1, TouchPhase::Started, pos2(450.0, 200.0));
        let o = c.update(DT, 0.0, &t2, &k, &l, cfg(), Adjacent::default(), Some(0.3), 0.38);
        assert_eq!(o.held.throttle, 1.0, "a press inside the window revs");
        let mut c = Controller::new();
        let mut t3 = Touches::new();
        t3.touch(1, TouchPhase::Started, pos2(450.0, 200.0));
        let o = c.update(DT, 0.0, &t3, &k, &l, cfg(), Adjacent::default(), Some(1.5), 0.38);
        assert_eq!(o.held.throttle, 0.0, "too early does nothing");
        let o = c.update(DT, 0.0, &t, &k, &l, cfg(), Adjacent::default(), None, 0.38);
        assert_eq!(o.held.throttle, 1.0, "after GO the auto throttle is on");
    }

    #[test]
    fn unstick_shows_a_steering_hint_without_a_brake_control() {
        let mut u = Unstick::default();
        for _ in 0..64 {
            u.update(DT, 0.2, true, true);
        }
        assert!(u.hint());
        for _ in 0..60 {
            u.update(DT, 0.2, true, true);
        }
        assert!(u.hint(), "a sustained stop keeps the steering hint visible");
        for _ in 0..90 {
            u.update(DT, -3.0, true, true);
        }
        assert!(!u.hint(), "moving releases it");
        // Moving or not racing never triggers.
        let mut u = Unstick::default();
        for _ in 0..300 {
            u.update(DT, 20.0, true, true);
            u.update(DT, 0.0, false, true);
        }
        assert!(!u.hint());
    }

    #[test]
    fn adjacent_rule() {
        let a = adjacent_sides([(2.0, 1.0), (-8.0, 0.0), (1.0, 20.0)]);
        assert!(a.right && !a.left);
    }
}
