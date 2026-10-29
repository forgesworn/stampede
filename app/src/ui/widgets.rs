//! The menu widget kit: big buttons that squash and bounce, glass panels,
//! toggles, sliders, segmented controls, stat bars, the back button and the
//! screen header. Immediate mode and rect-based: every widget is given its
//! rectangle (all layout is done by the screen, inside `ctx.viewport.safe`).
//! Colours and fonts come from [`theme`]; opacity comes from the `Ui` (wrap a
//! group in [`staged`] for the entry animation).

use crate::ui::art::{self, ease_out_back, fade, hash01, outlined, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::theme;
use egui::epaint::{Mesh, Shadow, Vertex};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Id, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};
use std::cell::Cell;

// Spacing scale.
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;
pub const S5: f32 = 24.0;

/// Height of the header strip (back button + title).
pub const HEADER_H: f32 = 56.0;
/// Smallest tap target.
pub const TAP_MIN: f32 = 44.0;

thread_local! {
    static OVER_3D: Cell<bool> = const { Cell::new(false) };
}

/// Tell the kit a live 3D scene is behind the UI: panels turn more see-through.
pub fn set_over_3d(on: bool) {
    OVER_3D.with(|c| c.set(on));
}

fn glass() -> f32 {
    if OVER_3D.with(|c| c.get()) {
        0.58
    } else {
        0.88
    }
}

// ── shapes ──────────────────────────────────────────────────────────────────

fn rr(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}

/// Points of a rounded rectangle, clockwise from the top-left corner's end.
fn rrect_points(rect: Rect, r: f32) -> Vec<Pos2> {
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let seg = ((r / 3.0).ceil() as usize).clamp(2, 10);
    let mut pts = Vec::with_capacity(seg * 4 + 4);
    let corners = [(rect.right_top() + vec2(-r, r), -std::f32::consts::FRAC_PI_2), (rect.right_bottom() + vec2(-r, -r), 0.0), (rect.left_bottom() + vec2(r, -r), std::f32::consts::FRAC_PI_2), (rect.left_top() + vec2(r, r), std::f32::consts::PI)];
    for (c, a0) in corners {
        for i in 0..=seg {
            let a = a0 + std::f32::consts::FRAC_PI_2 * i as f32 / seg as f32;
            pts.push(c + vec2(a.cos(), a.sin()) * r);
        }
    }
    pts
}

/// A rounded rectangle filled with a vertical gradient (`top` to `bottom`).
pub fn rrect_gradient(p: &Painter, rect: Rect, r: f32, top: Color32, bottom: Color32) {
    let pts = rrect_points(rect, r);
    let mut m = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    let col = |y: f32| theme::mix_rgba(top, bottom, (y - rect.top()) / rect.height().max(1.0));
    m.vertices.push(Vertex { pos: rect.center(), uv, color: col(rect.center().y) });
    for q in &pts {
        m.vertices.push(Vertex { pos: *q, uv, color: col(q.y) });
    }
    let n = pts.len() as u32;
    for i in 0..n {
        m.indices.extend([0, 1 + i, 1 + (i + 1) % n]);
    }
    p.add(Shape::mesh(m));
}

/// A soft drop shadow under a rounded rectangle.
pub fn soft_shadow(p: &Painter, rect: Rect, r: f32, dy: f32, blur: f32, alpha: f32) {
    let sh = Shadow { offset: [0, dy as i8], blur: blur.clamp(0.0, 40.0) as u8, spread: 0, color: rgba(8, 2, 20, alpha) };
    p.add(sh.as_shape(rect, rr(r)));
}

/// A glass panel: soft shadow, deep-plum body, a bright top edge.
pub fn panel(p: &Painter, rect: Rect, r: f32) {
    panel_alpha(p, rect, r, glass());
}

/// A fully opaque panel (modals).
pub fn panel_solid(p: &Painter, rect: Rect, r: f32) {
    panel_alpha(p, rect, r, 1.0);
}

fn panel_alpha(p: &Painter, rect: Rect, r: f32, g: f32) {
    soft_shadow(p, rect, r, 6.0, 18.0, 0.45);
    rrect_gradient(p, rect, r, rgba(0x3A, 0x1A, 0x62, g), rgba(0x1B, 0x0B, 0x33, g));
    p.rect_stroke(rect, rr(r), Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)), StrokeKind::Inside);
    p.rect_stroke(rect.shrink(0.5), rr(r - 0.5), Stroke::new(1.0_f32, rgba(0, 0, 0, 0.35)), StrokeKind::Outside);
    // lit top edge
    let y = rect.top() + 1.0;
    p.hline((rect.left() + r * 0.8)..=(rect.right() - r * 0.8), y, Stroke::new(1.0_f32, rgba(255, 220, 250, 0.22)));
}

/// A darker recessed well inside a panel (rows, tracks).
pub fn well(p: &Painter, rect: Rect, r: f32) {
    p.rect_filled(rect, rr(r), rgba(10, 4, 24, 0.42));
    p.rect_stroke(rect, rr(r), Stroke::new(1.0_f32, rgba(255, 255, 255, 0.07)), StrokeKind::Inside);
}

/// A small coloured pill with text ("NEW BEST", "LOCKED").
pub fn pill(p: &Painter, centre: Pos2, text: &str, bg: Color32, fg: Color32, size: f32) -> Rect {
    let g = p.layout_no_wrap(text.to_string(), theme::ui_bold(size), fg);
    let rect = Rect::from_center_size(centre, g.size() + vec2(size * 1.0, size * 0.55));
    p.rect_filled(rect, rr(rect.height() / 2.0), bg);
    p.galley(rect.center() - g.size() / 2.0, g, fg);
    rect
}

/// Text with the house outline: chunky display type.
pub fn heading(p: &Painter, pos: Pos2, anchor: Align2, text: &str, size: f32, fill: Color32) -> Rect {
    outlined(p, pos, anchor, text, theme::title(size), fill, theme::OUTLINE, (size * 0.075).max(1.0), (size * 0.07).max(1.0))
}

// ── entry animation ─────────────────────────────────────────────────────────

/// 0..1 for the `i`-th item of a staggered entry, `t` seconds after the screen opened.
pub fn enter(t: f32, i: usize) -> f32 {
    art::ease_out_cubic(art::phase(t, 0.10 + 0.06 * i as f32, 0.38))
}

/// Run `f` with `ui` faded to `e` (0..1) and hand it the slide offset to add to
/// the rects it draws (`slide` at e = 0, none at e = 1).
pub fn staged<R>(ui: &mut Ui, e: f32, slide: Vec2, f: impl FnOnce(&mut Ui, Vec2) -> R) -> R {
    ui.scope(|ui| {
        ui.set_opacity(e.clamp(0.0, 1.0));
        f(ui, slide * (1.0 - e))
    })
    .inner
}

// ── buttons ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Bitcoin orange: the one main action on a screen.
    Primary,
    /// Plum.
    Secondary,
    /// Outline only.
    Ghost,
}

pub struct Btn<'a> {
    pub label: &'a str,
    pub sub: Option<&'a str>,
    pub icon: Option<Icon>,
    pub kind: Kind,
    pub enabled: bool,
    /// Draw as the current choice (a lit rim).
    pub selected: bool,
    /// Font size override (default from the button height).
    pub size: Option<f32>,
}

impl<'a> Btn<'a> {
    pub fn new(label: &'a str) -> Btn<'a> {
        Btn { label, sub: None, icon: None, kind: Kind::Secondary, enabled: true, selected: false, size: None }
    }
    pub fn primary(label: &'a str) -> Btn<'a> {
        Btn { kind: Kind::Primary, ..Btn::new(label) }
    }
    pub fn ghost(label: &'a str) -> Btn<'a> {
        Btn { kind: Kind::Ghost, ..Btn::new(label) }
    }
    pub fn sub(mut self, s: &'a str) -> Self {
        self.sub = Some(s);
        self
    }
    pub fn icon(mut self, i: Icon) -> Self {
        self.icon = Some(i);
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    pub fn selected(mut self, on: bool) -> Self {
        self.selected = on;
        self
    }
    pub fn size(mut self, s: f32) -> Self {
        self.size = Some(s);
        self
    }
}

/// Press squash and release bounce for a clickable thing: the draw scale (about
/// its centre) and how far the face sinks (0..1). Call every frame.
pub fn press_fx(ui: &Ui, id: Id, resp: &Response) -> (f32, f32) {
    let ctx = ui.ctx();
    let now = ui.input(|i| i.time);
    let down = resp.is_pointer_button_down_on();
    let sink = ctx.animate_value_with_time(id.with("sink"), if down { 1.0 } else { 0.0 }, 0.06);
    if resp.clicked() {
        ctx.data_mut(|d| d.insert_temp(id.with("click"), now));
    }
    let since = ctx.data(|d| d.get_temp::<f64>(id.with("click"))).map(|t| (now - t) as f32).unwrap_or(9.0);
    let bounce = if since < 0.5 { (-since * 10.0).exp() * (since * 26.0).sin() * 0.055 } else { 0.0 };
    ((1.0 - 0.055 * sink + bounce).max(0.5), sink)
}

#[cfg(test)]
thread_local! {
    pub(crate) static BUTTON_RECTS: std::cell::RefCell<Option<Vec<(String, Rect, Rect)>>> = const { std::cell::RefCell::new(None) };
}

/// A big game button in `rect`. Returns its response (`clicked()` to act).
pub fn button(ui: &mut Ui, rect: Rect, salt: &str, b: &Btn) -> Response {
    #[cfg(test)]
    BUTTON_RECTS.with(|v| { if let Some(v) = v.borrow_mut().as_mut() { v.push((salt.to_string(), rect, ui.clip_rect())); } });
    let id = Id::new(("stampede-btn", salt));
    let resp = ui.interact(rect, id, if b.enabled { Sense::click() } else { Sense::hover() });
    let (scale, sink) = if b.enabled { press_fx(ui, id, &resp) } else { (1.0, 0.0) };
    let p = ui.painter().clone();
    draw_button(&p, rect, b, scale, sink);
    resp
}

fn draw_button(p: &Painter, rect: Rect, b: &Btn, scale: f32, sink: f32) {
    let rect = Rect::from_center_size(rect.center(), rect.size() * scale);
    let h = rect.height();
    let r = (h * 0.28).clamp(8.0, 22.0);
    let lip = (h * 0.07).clamp(2.0, 5.0);
    let dim = if b.enabled { 1.0 } else { 0.45 };
    let (top, bottom, lip_c, text_c, rim) = match b.kind {
        Kind::Primary => (Color32::from_rgb(0xFF, 0xBC, 0x4A), Color32::from_rgb(0xF0, 0x78, 0x1A), Color32::from_rgb(0xA8, 0x44, 0x10), Color32::WHITE, theme::OUTLINE),
        Kind::Secondary => (Color32::from_rgb(0x84, 0x4C, 0xB2), Color32::from_rgb(0x56, 0x27, 0x82), Color32::from_rgb(0x2A, 0x0F, 0x48), theme::TEXT, theme::OUTLINE),
        Kind::Ghost => (rgba(255, 255, 255, 0.07), rgba(255, 255, 255, 0.03), Color32::TRANSPARENT, theme::SAND, rgba(0xE8, 0xC6, 0x8A, 0.45)),
    };
    let face = rect.translate(vec2(0.0, lip * 0.8 * sink)).shrink2(vec2(0.0, 0.0));
    let face = Rect::from_min_max(face.min, pos2(face.max.x, face.max.y - lip));
    if b.kind != Kind::Ghost {
        soft_shadow(p, rect, r, 5.0 * (1.0 - 0.6 * sink), 12.0, 0.5 * dim);
        // the lip (button thickness)
        let lip_r = Rect::from_min_max(pos2(face.left(), face.top() + lip * 0.6), pos2(face.right(), face.bottom() + lip));
        p.rect_filled(lip_r, rr(r), fade(lip_c, dim));
        p.rect_stroke(lip_r, rr(r), Stroke::new(2.0_f32, fade(rim, dim)), StrokeKind::Inside);
    }
    rrect_gradient(p, face, r, fade(top, dim), fade(bottom, dim));
    if b.kind != Kind::Ghost {
        // gloss
        p.hline((face.left() + r * 0.9)..=(face.right() - r * 0.9), face.top() + 2.5, Stroke::new(1.6_f32, rgba(255, 255, 255, 0.42 * dim)));
        let gloss = Rect::from_min_max(face.min + vec2(3.0, 3.0), pos2(face.max.x - 3.0, face.center().y));
        p.rect_filled(gloss, CornerRadius { nw: (r - 2.0) as u8, ne: (r - 2.0) as u8, sw: 2, se: 2 }, rgba(255, 255, 255, 0.07 * dim));
    }
    p.rect_stroke(face, rr(r), Stroke::new(if b.kind == Kind::Ghost { 1.5_f32 } else { 2.0 }, fade(rim, dim)), StrokeKind::Inside);
    if b.selected {
        p.rect_stroke(face.expand(2.5), rr(r + 2.5), Stroke::new(2.5_f32, theme::GOLD), StrokeKind::Outside);
    }

    // icon-only buttons centre their icon
    if b.label.is_empty() {
        if let Some(ic) = b.icon {
            icons::draw(p, ic, face.center(), (face.height() * 0.58).min(30.0), fade(if b.kind == Kind::Primary { Color32::WHITE } else { theme::SAND }, dim));
        }
        return;
    }
    // contents
    let pad = h * 0.30;
    let mut text_left = face.left() + pad;
    let mut avail = face.width() - 2.0 * pad;
    let mut centre_x = face.center().x;
    if let Some(ic) = b.icon {
        let d = (face.height() * 0.52).min(34.0);
        let c = pos2(face.left() + pad + d / 2.0, face.center().y);
        p.circle_filled(c, d * 0.72, rgba(20, 6, 40, 0.30 * dim));
        icons::draw(p, ic, c, d, fade(if b.kind == Kind::Primary { Color32::WHITE } else { theme::SAND }, dim));
        text_left = c.x + d * 0.72 + pad * 0.7;
        avail = face.right() - pad - text_left;
        centre_x = text_left + avail / 2.0;
    }
    let base = b.size.unwrap_or((face.height() * 0.40).clamp(13.0, 30.0));
    let size = art::fit_size(p, b.label, theme::title, base, avail.max(20.0));
    let ly = if b.sub.is_some() { face.center().y - size * 0.30 } else { face.center().y };
    let (anchor, x) = if b.icon.is_some() { (Align2::LEFT_CENTER, text_left) } else { (Align2::CENTER_CENTER, centre_x) };
    let text_col = fade(text_c, dim);
    outlined(p, pos2(x, ly - sink * 0.0), anchor, b.label, theme::title(size), text_col, if b.kind == Kind::Ghost { Color32::TRANSPARENT } else { fade(rim, dim) }, if b.kind == Kind::Ghost { 0.0 } else { (size * 0.085).max(1.0) }, if b.kind == Kind::Ghost { 0.0 } else { 1.5 });
    if let Some(sub) = b.sub {
        let ss = art::fit_size(p, sub, theme::ui_medium, (size * 0.52).clamp(10.5, 14.0), avail.max(20.0));
        p.text(pos2(x, face.center().y + size * 0.62), if b.icon.is_some() { Align2::LEFT_CENTER } else { Align2::CENTER_CENTER }, sub, theme::ui_medium(ss), fade(if b.kind == Kind::Primary { Color32::from_rgb(0x5A, 0x22, 0x08) } else { theme::TEXT_DIM }, dim));
    }
}

/// The round back button at the header's left. Returns its response.
pub fn back_button(ui: &mut Ui, safe: Rect) -> Response {
    let rect = Rect::from_min_size(pos2(safe.left() + S4, safe.top() + S2 + 2.0), vec2(TAP_MIN, TAP_MIN));
    let id = Id::new("stampede-back");
    let resp = ui.interact(rect.expand(4.0), id, Sense::click());
    let (scale, sink) = press_fx(ui, id, &resp);
    let p = ui.painter().clone();
    let c = rect.center();
    let r = TAP_MIN / 2.0 * scale;
    soft_shadow(&p, Rect::from_center_size(c, vec2(r * 2.0, r * 2.0)), r, 4.0, 10.0, 0.5);
    p.circle_filled(c + vec2(0.0, 2.5 - sink * 1.5), r, Color32::from_rgb(0x2A, 0x0F, 0x48));
    p.circle_filled(c + vec2(0.0, sink * 1.5), r - 1.0, Color32::from_rgb(0x6A, 0x36, 0x96));
    p.circle_filled(c + vec2(0.0, sink * 1.5 - r * 0.2), r * 0.72, rgba(255, 255, 255, 0.07));
    p.circle_stroke(c + vec2(0.0, sink * 1.5), r - 1.0, Stroke::new(2.0_f32, theme::OUTLINE));
    icons::draw(&p, Icon::ChevronLeft, c + vec2(-1.0, sink * 1.5), TAP_MIN * 0.62, theme::TEXT);
    resp
}

/// The screen header: back button and title. `body` is the area below it.
pub struct Header {
    pub back: Response,
    pub body: Rect,
}

pub fn header(ui: &mut Ui, safe: Rect, title: &str, e: f32) -> Header {
    let back = staged(ui, e, vec2(-20.0, 0.0), |ui, off| {
        let mut s = safe;
        s.min += off;
        back_button(ui, s)
    });
    let p = ui.painter().clone();
    let mut p2 = p.clone();
    p2.set_opacity(e.clamp(0.0, 1.0));
    let size = art::fit_size(&p2, title, theme::title, 30.0, safe.width() - 2.0 * S4 - TAP_MIN - S3);
    heading(&p2, pos2(safe.left() + S4 + TAP_MIN + S3 + (1.0 - e) * 14.0, safe.top() + S2 + 2.0 + TAP_MIN / 2.0), Align2::LEFT_CENTER, title, size, theme::TEXT);
    let body = Rect::from_min_max(pos2(safe.left() + S4, safe.top() + S2 + 2.0 + TAP_MIN + S3), pos2(safe.right() - S4, safe.bottom() - S4));
    Header { back, body }
}

// ── controls ────────────────────────────────────────────────────────────────

/// A switch. `rect` is the whole row (tap anywhere); the switch sits at its right end.
pub fn toggle(ui: &mut Ui, rect: Rect, salt: &str, on: &mut bool) -> Response {
    let id = Id::new(("stampede-toggle", salt));
    let mut resp = ui.interact(rect, id, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let (_, sink) = press_fx(ui, id, &resp);
    let k = ui.ctx().animate_bool_with_time(id.with("k"), *on, 0.16);
    let p = ui.painter().clone();
    let sw = vec2(56.0, 30.0);
    let sr = Rect::from_center_size(pos2(rect.right() - sw.x / 2.0, rect.center().y), sw);
    let r = sw.y / 2.0;
    well(&p, sr.expand(1.0), r + 1.0);
    let off_c = (Color32::from_rgb(0x3A, 0x22, 0x5A), Color32::from_rgb(0x2A, 0x16, 0x44));
    let on_c = (Color32::from_rgb(0xFF, 0xB0, 0x3A), Color32::from_rgb(0xF0, 0x78, 0x1A));
    rrect_gradient(&p, sr, r, theme::mix(off_c.0, on_c.0, k), theme::mix(off_c.1, on_c.1, k));
    p.rect_stroke(sr, rr(r), Stroke::new(2.0_f32, theme::OUTLINE), StrokeKind::Inside);
    let kr = r - 3.5 + sink * 1.0;
    let kx = art::lerp(sr.left() + r, sr.right() - r, k);
    p.circle_filled(pos2(kx, sr.center().y + 1.5), kr, rgba(0, 0, 0, 0.30));
    p.circle_filled(pos2(kx, sr.center().y), kr, Color32::from_rgb(0xFF, 0xF4, 0xE0));
    p.circle_filled(pos2(kx, sr.center().y - kr * 0.35), kr * 0.55, rgba(255, 255, 255, 0.5));
    resp
}

/// A 0..1 slider across `rect`. Returns the response (`changed()` while dragging).
pub fn slider(ui: &mut Ui, rect: Rect, salt: &str, v: &mut f32) -> Response {
    let id = Id::new(("stampede-slider", salt));
    let mut resp = ui.interact(rect, id, Sense::click_and_drag());
    let pad = 14.0;
    let (x0, x1) = (rect.left() + pad, rect.right() - pad);
    if resp.dragged() || resp.drag_started() || resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let nv = ((pos.x - x0) / (x1 - x0)).clamp(0.0, 1.0);
            if (nv - *v).abs() > 1e-4 {
                *v = nv;
                resp.mark_changed();
            }
        }
    }
    let down = resp.is_pointer_button_down_on();
    let grow = ui.ctx().animate_value_with_time(id.with("grow"), if down { 1.0 } else { 0.0 }, 0.08);
    let p = ui.painter().clone();
    let cy = rect.center().y;
    let track = Rect::from_min_max(pos2(x0 - 4.0, cy - 6.0), pos2(x1 + 4.0, cy + 6.0));
    well(&p, track, 6.0);
    let fx = art::lerp(x0, x1, *v);
    if fx > track.left() + 2.0 {
        let fill = Rect::from_min_max(track.min + vec2(1.5, 1.5), pos2(fx, track.max.y - 1.5));
        rrect_gradient(&p, fill, 5.0, Color32::from_rgb(0xFF, 0xC0, 0x4A), Color32::from_rgb(0xF0, 0x78, 0x1A));
    }
    let tr = 13.0 + 2.5 * grow;
    p.circle_filled(pos2(fx, cy + 2.5), tr, rgba(0, 0, 0, 0.35));
    p.circle_filled(pos2(fx, cy), tr, theme::OUTLINE);
    p.circle_filled(pos2(fx, cy), tr - 2.5, Color32::from_rgb(0xFF, 0xF0, 0xD6));
    p.circle_filled(pos2(fx, cy), tr - 6.0, theme::ACCENT);
    p.circle_filled(pos2(fx - 1.5, cy - 2.5), 2.5, rgba(255, 255, 255, 0.6));
    resp
}

pub struct Seg<'a> {
    pub label: &'a str,
    pub enabled: bool,
    /// A small tag under a disabled option ("soon").
    pub tag: Option<&'a str>,
}

impl<'a> Seg<'a> {
    pub fn new(label: &'a str) -> Seg<'a> {
        Seg { label, enabled: true, tag: None }
    }
    pub fn soon(label: &'a str) -> Seg<'a> {
        Seg { label, enabled: false, tag: Some("soon") }
    }
}

/// A segmented control with a sliding highlight. Returns the index the user
/// tapped this frame, if any (the caller stores it).
pub fn segmented(ui: &mut Ui, rect: Rect, salt: &str, segs: &[Seg], selected: usize) -> Option<usize> {
    let n = segs.len().max(1);
    let p = ui.painter().clone();
    let r = (rect.height() / 2.0).min(18.0);
    well(&p, rect, r);
    let sw = rect.width() / n as f32;
    let id = Id::new(("stampede-seg", salt));
    let sel_x = ui.ctx().animate_value_with_time(id.with("x"), selected as f32, 0.16);
    let hi = Rect::from_min_size(pos2(rect.left() + sel_x * sw, rect.top()), vec2(sw, rect.height())).shrink(2.5);
    soft_shadow(&p, hi, r - 2.5, 3.0, 8.0, 0.4);
    rrect_gradient(&p, hi, r - 2.5, Color32::from_rgb(0xFF, 0xB8, 0x44), Color32::from_rgb(0xF0, 0x78, 0x1A));
    p.rect_stroke(hi, rr(r - 2.5), Stroke::new(1.6_f32, theme::OUTLINE), StrokeKind::Inside);
    p.hline((hi.left() + r * 0.9)..=(hi.right() - r * 0.9), hi.top() + 2.0, Stroke::new(1.2_f32, rgba(255, 255, 255, 0.4)));
    let mut tapped = None;
    for (i, s) in segs.iter().enumerate() {
        let cell = Rect::from_min_size(pos2(rect.left() + i as f32 * sw, rect.top()), vec2(sw, rect.height()));
        let resp = ui.interact(cell, id.with(i), if s.enabled { Sense::click() } else { Sense::hover() });
        if resp.clicked() && i != selected {
            tapped = Some(i);
        }
        let lit = 1.0 - (sel_x - i as f32).abs().min(1.0);
        let col = if !s.enabled { rgba(0xB8, 0xA6, 0xC9, 0.45) } else { theme::mix(theme::TEXT_DIM, Color32::WHITE, lit) };
        let size = art::fit_size(&p, s.label, theme::title, (rect.height() * 0.46).clamp(12.0, 20.0), sw - 12.0);
        let cy = if s.tag.is_some() { cell.center().y - 3.5 } else { cell.center().y };
        if s.enabled && lit > 0.5 {
            outlined(&p, pos2(cell.center().x, cy), Align2::CENTER_CENTER, s.label, theme::title(size), col, theme::OUTLINE, 1.2, 0.0);
        } else {
            p.text(pos2(cell.center().x, cy), Align2::CENTER_CENTER, s.label, theme::title(size), col);
        }
        if let Some(tag) = s.tag {
            p.text(pos2(cell.center().x, cell.bottom() - 6.5), Align2::CENTER_CENTER, tag, theme::ui_bold(9.0), rgba(0xE8, 0xC6, 0x8A, 0.7));
        }
    }
    tapped
}

/// A segmented stat bar: `norm` 0..1 lights segments left to right (animated by `grow` 0..1).
pub fn stat_bar(p: &Painter, rect: Rect, norm: f32, grow: f32, segs: usize, col_a: Color32, col_b: Color32) {
    let gap = 3.0;
    let w = (rect.width() - gap * (segs as f32 - 1.0)) / segs as f32;
    let lit = norm.clamp(0.0, 1.0) * segs as f32 * grow.clamp(0.0, 1.0);
    for i in 0..segs {
        let seg = Rect::from_min_size(pos2(rect.left() + i as f32 * (w + gap), rect.top()), vec2(w, rect.height()));
        well(p, seg, 3.0);
        let k = (lit - i as f32).clamp(0.0, 1.0);
        if k > 0.0 {
            let mut f = seg.shrink(1.2);
            f.max.x = f.min.x + f.width() * k;
            let c = theme::mix(col_a, col_b, i as f32 / (segs.max(2) - 1) as f32);
            rrect_gradient(p, f, 2.5, theme::mix(c, Color32::WHITE, 0.25), c);
        }
    }
}

/// A row of `total` stars, the first `filled` lit; stars pop in one after another
/// as `anim` (seconds since the row appeared) grows.
pub fn stars(p: &Painter, centre: Pos2, total: u8, filled: u8, size: f32, anim: f32) {
    let gap = size * 1.12;
    let x0 = centre.x - gap * (total as f32 - 1.0) / 2.0;
    for i in 0..total {
        let c = pos2(x0 + gap * i as f32, centre.y + if i == 1 && total == 3 { -size * 0.08 } else { 0.0 });
        let on = i < filled;
        if on {
            let k = ease_out_back(art::phase(anim, 0.10 * i as f32, 0.30));
            if k <= 0.0 {
                icons::star(p, c, size * 0.5, rgba(10, 4, 24, 0.5), Stroke::NONE);
                continue;
            }
            art::glow(p, c, size * 0.95 * k, rgba(0xFF, 0xC8, 0x3D, 0.28));
            icons::star(p, c + vec2(0.0, 1.5), size * 0.56 * k, rgba(0x2C, 0x0D, 0x44, 0.9), Stroke::NONE);
            icons::star(p, c, size * 0.5 * k, theme::GOLD, Stroke::new(1.5_f32, Color32::from_rgb(0xB8, 0x6A, 0x10)));
            icons::star(p, c - vec2(0.0, size * 0.04), size * 0.26 * k, rgba(255, 245, 200, 0.55), Stroke::NONE);
        } else {
            icons::star(p, c, size * 0.5, rgba(10, 4, 24, 0.55), Stroke::new(1.2_f32, rgba(255, 255, 255, 0.16)));
        }
    }
}

/// A colour chip: a round swatch with a bright rim.
pub fn chip(p: &Painter, c: Pos2, r: f32, col: Color32) {
    p.circle_filled(c + vec2(0.0, 1.2), r + 1.0, rgba(0, 0, 0, 0.35));
    p.circle_filled(c, r + 1.0, rgba(255, 244, 224, 0.9));
    p.circle_filled(c, r - 0.8, col);
    p.circle_filled(c - vec2(r * 0.2, r * 0.3), r * 0.28, rgba(255, 255, 255, 0.28));
}

/// Dots under a pager: `n` dots, `sel` lit.
pub fn dots(p: &Painter, centre: Pos2, n: usize, sel: f32, col: Color32) {
    let gap = 14.0;
    let x0 = centre.x - gap * (n as f32 - 1.0) / 2.0;
    for i in 0..n {
        let k = 1.0 - (sel - i as f32).abs().min(1.0);
        p.circle_filled(pos2(x0 + gap * i as f32, centre.y), 3.0 + 2.2 * k, theme::mix_rgba(rgba(255, 255, 255, 0.22), col, k));
    }
}

/// Sparkle / confetti coins falling across `rect` (a celebration).
pub fn confetti(p: &Painter, b: &art::Brand, rect: Rect, t: f32, amount: usize) {
    for i in 0..amount as u32 {
        let x = rect.left() + hash01(i * 3 + 1) * rect.width();
        let speed = 70.0 + hash01(i * 5 + 2) * 110.0;
        let fall = rect.height() + 60.0;
        let y = rect.top() - 30.0 + (hash01(i * 7 + 3) * fall + t * speed) % fall;
        let sway = (t * (1.0 + hash01(i * 11)) + hash01(i * 13) * 6.0).sin() * 14.0;
        let sz = 12.0 + hash01(i * 17) * 12.0;
        if i % 3 == 0 {
            art::coin(p, b, pos2(x + sway, y), sz, Some(t * (0.6 + hash01(i * 19)) + hash01(i * 23)), 0.95);
        } else {
            let cols = [theme::ACCENT, theme::GOLD, theme::LASER, theme::SAND, Color32::from_rgb(0xB8, 0x7A, 0xFF)];
            let c = cols[(i % 5) as usize];
            let a = t * (2.0 + hash01(i * 29) * 3.0) + hash01(i * 31) * 6.0;
            let w = sz * 0.34;
            let d = vec2(a.cos(), a.sin()) * w;
            p.add(Shape::convex_polygon(vec![pos2(x + sway, y) + d, pos2(x + sway, y) + vec2(-d.y, d.x) * 0.4, pos2(x + sway, y) - d, pos2(x + sway, y) - vec2(-d.y, d.x) * 0.4], c, Stroke::NONE));
        }
    }
}

/// `FontId` for the small body text used in cards.
pub fn body(size: f32) -> FontId {
    theme::ui_medium(size)
}
