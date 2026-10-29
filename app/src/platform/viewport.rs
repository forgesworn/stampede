//! Screen layout: UI scale, safe-area insets and the safe rectangle.
//!
//! All sizes are in UI points. Menus rotate; driving stays landscape.
//! Layouts share a short side of [`MIN_SHORT`]..[`MAX_SHORT`] points
//! (Android dp: a Pixel 8 is 411). [`ui_ppp`] picks the pixels-per-point that
//! gets any window or screen into that band, so a 1600x720 desktop window and
//! a 2400x1080 phone both show the same layout.

use egui::{pos2, vec2, Rect, Vec2};

/// Shortest short side (points) the UI is laid out for.
pub const MIN_SHORT: f32 = 360.0;
/// Longest short side (points) the UI is laid out for.
pub const MAX_SHORT: f32 = 412.0;

/// UI pixels-per-point for a surface of `size_px` whose OS scale factor is
/// `native_ppp`. Equal to `native_ppp` when the short side is already
/// 360..412 points (every phone); a desktop window or a tablet is scaled so
/// its short side becomes 412 (or 360 if it is smaller than that).
pub fn ui_ppp(size_px: (u32, u32), native_ppp: f32) -> f32 {
    let native = if native_ppp.is_finite() && native_ppp > 0.0 { native_ppp } else { 1.0 };
    let short_px = size_px.0.min(size_px.1).max(1) as f32;
    let short_pt = short_px / native;
    let target = short_pt.clamp(MIN_SHORT, MAX_SHORT);
    short_px / target
}

/// Safe-area padding in points: the camera cutout and anything else the
/// system keeps clear. The cutout can be on any edge after a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

impl Insets {
    pub const ZERO: Insets = Insets { top: 0.0, bottom: 0.0, left: 0.0, right: 0.0 };

    /// Parse `left,top,right,bottom` (points), as `--insets` takes it.
    pub fn parse(s: &str) -> Result<Insets, String> {
        let v: Result<Vec<f32>, _> = s.split(',').map(|p| p.trim().parse::<f32>()).collect();
        match v.as_deref() {
            Ok([l, t, r, b]) if [l, t, r, b].iter().all(|x| x.is_finite() && **x >= 0.0) => Ok(Insets { left: *l, top: *t, right: *r, bottom: *b }),
            _ => Err(format!("--insets wants LEFT,TOP,RIGHT,BOTTOM in points, got {s:?}")),
        }
    }
}

/// Where everything goes on screen this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// The full screen in UI points (origin at the top-left).
    pub screen: Rect,
    /// UI pixels per point (see [`ui_ppp`]).
    pub ppp: f32,
    pub insets: Insets,
    /// The screen minus the insets: keep controls and text inside it.
    pub safe: Rect,
}

impl Viewport {
    /// Lay out a screen of `size` points at `ppp`.
    pub fn new(size: Vec2, ppp: f32, insets: Insets) -> Viewport {
        let size = size.max(vec2(1.0, 1.0));
        let screen = Rect::from_min_size(pos2(0.0, 0.0), size);
        let safe = Rect::from_min_max(
            pos2(insets.left.min(size.x * 0.4), insets.top.min(size.y * 0.4)),
            pos2(size.x - insets.right.min(size.x * 0.4), size.y - insets.bottom.min(size.y * 0.4)),
        );
        Viewport { screen, ppp, insets, safe }
    }

    pub fn size(&self) -> Vec2 {
        self.screen.size()
    }

    /// Physical pixel size of the screen.
    pub fn physical(&self) -> (u32, u32) {
        ((self.screen.width() * self.ppp).round().max(1.0) as u32, (self.screen.height() * self.ppp).round().max(1.0) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phones_keep_their_native_scale() {
        // Pixel 8 held sideways: 2400x1080 at 2.625 -> 411 dp short side.
        let p = ui_ppp((2400, 1080), 2.625);
        assert!((p - 2.625).abs() < 0.01, "{p}");
    }

    #[test]
    fn desktop_and_tablets_are_scaled_into_the_band() {
        let p = ui_ppp((1600, 720), 1.0);
        assert!((720.0 / p - MAX_SHORT).abs() < 0.01);
        let p = ui_ppp((2560, 1600), 2.0); // 800 dp short side
        assert!((1600.0 / p - MAX_SHORT).abs() < 0.01);
        // A small short side is enlarged up to 360 points.
        let p = ui_ppp((1280, 600), 2.0); // 300 dp
        assert!((600.0 / p - MIN_SHORT).abs() < 0.01);
    }

    #[test]
    fn safe_rect_pads_left_and_right_for_a_landscape_cutout() {
        let v = Viewport::new(vec2(890.0, 411.0), 2.625, Insets { left: 48.0, right: 0.0, top: 0.0, bottom: 0.0 });
        assert_eq!(v.safe.min, pos2(48.0, 0.0));
        assert_eq!(v.safe.max, pos2(890.0, 411.0));
        assert_eq!(v.physical(), (2336, 1079));
    }

    #[test]
    fn insets_parse() {
        assert_eq!(Insets::parse("48,0,24,10").unwrap(), Insets { left: 48.0, top: 0.0, right: 24.0, bottom: 10.0 });
        assert!(Insets::parse("1,2,3").is_err());
    }
}
