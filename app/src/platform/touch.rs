//! Raw multi-touch and keyboard state.
//!
//! The race needs simultaneous thumbs, and egui has a single pointer, so every
//! finger is tracked here, by id (the pattern from Axe'n'Stax's
//! `touch_input.rs`: lifting one finger never moves another). Screens read
//! [`Touches`] from `Ctx` and decide what to do with each finger; egui
//! separately sees one primary pointer (see `App::pointer_event`).
//!
//! Positions are in UI points (what `Ctx::viewport` and egui use), y down.
//! On desktop the left mouse button is a touch with id [`MOUSE_LEFT`] and the
//! right button a second one, [`MOUSE_RIGHT`], so two-thumb screens can be
//! tried with a mouse.

use egui::{Pos2, Vec2};
use std::collections::HashSet;
use winit::keyboard::KeyCode;

/// Id of the left mouse button's touch (desktop).
pub const MOUSE_LEFT: u64 = u64::MAX;
/// Id of the right mouse button's touch (desktop).
pub const MOUSE_RIGHT: u64 = u64::MAX - 1;

/// Touch lifecycle, mirrored from winit so tests need no winit types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
}

/// One finger (or mouse button) on the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Touch {
    /// Stable for the life of the finger.
    pub id: u64,
    /// Where it is now (UI points).
    pub pos: Pos2,
    /// Where it went down.
    pub start: Pos2,
    /// [`Touches::time`] when it went down (seconds).
    pub start_time: f64,
    /// Smoothed velocity in points per second, updated once per frame.
    pub vel: Vec2,
    /// Press order, larger is more recent.
    pub order: u64,
    last_frame_pos: Pos2,
}

impl Touch {
    /// Distance from where the finger went down (points).
    pub fn travel(&self) -> f32 {
        (self.pos - self.start).length()
    }

    /// Offset from where the finger went down.
    pub fn delta(&self) -> Vec2 {
        self.pos - self.start
    }

    /// Seconds the finger has been down.
    pub fn age(&self, now: f64) -> f32 {
        (now - self.start_time).max(0.0) as f32
    }

    pub fn is_mouse(&self) -> bool {
        self.id >= MOUSE_RIGHT
    }
}

/// Every finger down right now, plus what began and ended since the last frame.
#[derive(Clone, Debug, Default)]
pub struct Touches {
    active: Vec<Touch>,
    began: Vec<Touch>,
    ended: Vec<Touch>,
    cursor: Pos2,
    counter: u64,
    /// Engine time of the last frame, seconds.
    pub time: f64,
}

impl Touches {
    pub fn new() -> Touches {
        Touches::default()
    }

    /// Fingers down now, oldest first.
    pub fn active(&self) -> &[Touch] {
        &self.active
    }

    pub fn count(&self) -> usize {
        self.active.len()
    }

    pub fn get(&self, id: u64) -> Option<&Touch> {
        self.active.iter().find(|t| t.id == id)
    }

    /// Fingers that went down since the last frame (a tap that also ended
    /// is here and in [`Touches::ended`], and not in `active`).
    pub fn began(&self) -> &[Touch] {
        &self.began
    }

    /// Fingers that lifted (or were cancelled) since the last frame, at their last position.
    pub fn ended(&self) -> &[Touch] {
        &self.ended
    }

    /// Fingers inside `rect` (by current position).
    pub fn in_rect(&self, rect: egui::Rect) -> impl Iterator<Item = &Touch> {
        self.active.iter().filter(move |t| rect.contains(t.pos))
    }

    /// A touch event (winit `WindowEvent::Touch`, converted to UI points).
    /// `Cancelled` is reported as `Ended`.
    pub fn touch(&mut self, id: u64, phase: TouchPhase, pos: Pos2) {
        let found = self.active.iter().position(|t| t.id == id);
        match phase {
            TouchPhase::Started => {
                if found.is_some() {
                    return;
                }
                self.counter += 1;
                let t = Touch { id, pos, start: pos, start_time: self.time, vel: Vec2::ZERO, order: self.counter, last_frame_pos: pos };
                self.active.push(t);
                self.began.push(t);
            }
            TouchPhase::Moved => {
                if let Some(i) = found {
                    self.active[i].pos = pos;
                }
            }
            TouchPhase::Ended => {
                if let Some(i) = found {
                    let mut t = self.active.remove(i);
                    t.pos = pos;
                    self.ended.push(t);
                }
            }
        }
    }

    /// The mouse cursor moved (desktop). Only the most recently pressed held
    /// mouse button follows it.
    pub fn cursor_moved(&mut self, pos: Pos2) {
        self.cursor = pos;
        if let Some(t) = self.active.iter_mut().filter(|t| t.is_mouse()).max_by_key(|t| t.order) {
            t.pos = pos;
        }
    }

    /// A mouse button changed (desktop): left is [`MOUSE_LEFT`], right is [`MOUSE_RIGHT`].
    pub fn mouse_button(&mut self, left: bool, pressed: bool) {
        let id = if left { MOUSE_LEFT } else { MOUSE_RIGHT };
        let phase = if pressed { TouchPhase::Started } else { TouchPhase::Ended };
        self.touch(id, phase, self.cursor);
    }

    /// Drop every finger (focus lost, a screen change): they end where they were.
    pub fn clear(&mut self) {
        let gone: Vec<Touch> = self.active.drain(..).collect();
        self.ended.extend(gone);
    }

    /// Finish a frame: forget what began/ended, update velocities.
    pub fn end_frame(&mut self, dt: f32) {
        self.began.clear();
        self.ended.clear();
        if dt <= 0.0 {
            return;
        }
        // Exponential smoothing with a ~50 ms time constant.
        let k = 1.0 - (-dt / 0.05).exp();
        for t in &mut self.active {
            let inst = (t.pos - t.last_frame_pos) / dt;
            t.vel += (inst - t.vel) * k;
            t.last_frame_pos = t.pos;
        }
    }
}

/// Keyboard state (desktop; physical keys, so WASD works on any layout).
#[derive(Clone, Debug, Default)]
pub struct Keys {
    held: HashSet<KeyCode>,
    pressed: HashSet<KeyCode>,
    released: HashSet<KeyCode>,
}

impl Keys {
    pub fn held(&self, k: KeyCode) -> bool {
        self.held.contains(&k)
    }

    /// Went down since the last frame.
    pub fn pressed(&self, k: KeyCode) -> bool {
        self.pressed.contains(&k)
    }

    /// Went up since the last frame.
    pub fn released(&self, k: KeyCode) -> bool {
        self.released.contains(&k)
    }

    /// Any of `ks` held.
    pub fn any_held(&self, ks: &[KeyCode]) -> bool {
        ks.iter().any(|k| self.held.contains(k))
    }

    pub fn key(&mut self, k: KeyCode, down: bool) {
        if down {
            if self.held.insert(k) {
                self.pressed.insert(k);
            }
        } else if self.held.remove(&k) {
            self.released.insert(k);
        }
    }

    pub fn clear(&mut self) {
        let gone: Vec<KeyCode> = self.held.drain().collect();
        self.released.extend(gone);
    }

    pub fn end_frame(&mut self) {
        self.pressed.clear();
        self.released.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    #[test]
    fn two_fingers_keep_their_ids() {
        let mut t = Touches::new();
        t.touch(7, TouchPhase::Started, pos2(10.0, 10.0));
        t.touch(3, TouchPhase::Started, pos2(300.0, 200.0));
        assert_eq!(t.count(), 2);
        assert_eq!(t.began().len(), 2);
        t.touch(7, TouchPhase::Moved, pos2(40.0, 10.0));
        t.touch(7, TouchPhase::Ended, pos2(40.0, 10.0));
        assert_eq!(t.count(), 1);
        assert_eq!(t.get(3).unwrap().pos, pos2(300.0, 200.0), "lifting one finger never moves the other");
        assert_eq!(t.ended()[0].id, 7);
        t.end_frame(1.0 / 60.0);
        assert!(t.began().is_empty() && t.ended().is_empty());
    }

    #[test]
    fn a_tap_between_frames_is_seen_began_and_ended() {
        let mut t = Touches::new();
        t.touch(1, TouchPhase::Started, pos2(5.0, 5.0));
        t.touch(1, TouchPhase::Ended, pos2(5.0, 5.0));
        assert_eq!((t.count(), t.began().len(), t.ended().len()), (0, 1, 1));
    }

    #[test]
    fn velocity_follows_motion() {
        let mut t = Touches::new();
        t.touch(1, TouchPhase::Started, pos2(0.0, 0.0));
        for i in 1..=30 {
            t.touch(1, TouchPhase::Moved, pos2(i as f32 * 200.0 / 60.0, 0.0));
            t.end_frame(1.0 / 60.0);
        }
        // 200 / 60 = 3.33 pt per frame = 200 pt/s.
        let v = t.get(1).unwrap().vel.x;
        assert!((v - 200.0).abs() < 20.0, "{v}");
    }

    #[test]
    fn mouse_buttons_are_touches_and_the_latest_follows() {
        let mut t = Touches::new();
        t.cursor_moved(pos2(1.0, 1.0));
        t.mouse_button(true, true);
        t.cursor_moved(pos2(50.0, 50.0));
        t.mouse_button(false, true);
        t.cursor_moved(pos2(90.0, 90.0));
        assert_eq!(t.get(MOUSE_LEFT).unwrap().pos, pos2(50.0, 50.0));
        assert_eq!(t.get(MOUSE_RIGHT).unwrap().pos, pos2(90.0, 90.0));
        t.mouse_button(false, false);
        t.mouse_button(true, false);
        assert_eq!(t.count(), 0);
    }

    #[test]
    fn keys_edges() {
        let mut k = Keys::default();
        k.key(KeyCode::Space, true);
        k.key(KeyCode::Space, true);
        assert!(k.held(KeyCode::Space) && k.pressed(KeyCode::Space));
        k.end_frame();
        assert!(k.held(KeyCode::Space) && !k.pressed(KeyCode::Space));
        k.key(KeyCode::Space, false);
        assert!(!k.held(KeyCode::Space) && k.released(KeyCode::Space));
    }
}
