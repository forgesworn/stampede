//! Track data for the menus (name, length, theme colours, a normalised outline
//! for the minimap) and the track card widget.

use crate::ui::art::{self, fade, from_f3, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::{theme, widgets};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, Id, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};
use race_core::TrackId;
use std::cell::RefCell;
use std::rc::Rc;

pub struct TrackInfo {
    pub id: TrackId,
    pub name: String,
    pub length: f32,
    /// The centreline seen from above, in a box from -0.5 to 0.5 (aspect kept), ~100 points.
    pub outline: Vec<Vec2>,
    pub sky_top: Color32,
    pub sky_horizon: Color32,
    pub ground: Color32,
    pub road: Color32,
    pub night: bool,
}

thread_local! {
    static CACHE: RefCell<Vec<Option<Rc<TrackInfo>>>> = const { RefCell::new(Vec::new()) };
}

fn build(id: TrackId) -> TrackInfo {
    let track = id.track();
    let step = (track.samples.len() / 110).max(1);
    let pts: Vec<(f32, f32)> = track.samples.iter().step_by(step).map(|s| (-s.pos.x, -s.pos.z)).collect();
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for &(x, y) in &pts {
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    let ext = (x1 - x0).max(y1 - y0).max(1.0);
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let outline = pts.iter().map(|&(x, y)| vec2((x - cx) / ext, (y - cy) / ext)).collect();
    let th = &track.theme;
    TrackInfo { id, name: track.name.clone(), length: track.length, outline, sky_top: from_f3(th.sky_top), sky_horizon: from_f3(th.sky_horizon), ground: from_f3(th.ground), road: from_f3(th.road), night: th.night }
}

/// The (cached) info for a track. The first call per track builds its terrain.
pub fn info(id: TrackId) -> Rc<TrackInfo> {
    if id == TrackId::Custom {
        return Rc::new(build(id));
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_empty() {
            c.resize(TrackId::ALL.len(), None);
        }
        c[id.index()].get_or_insert_with(|| Rc::new(build(id))).clone()
    })
}

/// Build every race track's info on a worker thread's worth of time: called from the splash.
pub fn warm_in_background() {
    std::thread::spawn(|| {
        for id in TrackId::ALL {
            let _ = id.track();
        }
    });
}

/// `1.10 km`
pub fn km(length: f32) -> String {
    format!("{:.2} km", length / 1000.0)
}

/// Draw a track's outline into `area` (a minimap). `draw` 0..1 reveals the line.
pub fn minimap(p: &egui::Painter, area: Rect, info: &TrackInfo, line: Color32, draw: f32) {
    let side = area.width().min(area.height());
    let c = area.center();
    let pts: Vec<egui::Pos2> = info.outline.iter().map(|v| pos2(c.x + v.x * side, c.y + v.y * side)).collect();
    let n = ((pts.len() as f32 * draw.clamp(0.0, 1.0)) as usize).max(2).min(pts.len());
    let part: Vec<egui::Pos2> = pts[..n].to_vec();
    let closed = n == pts.len();
    let w = (side * 0.05).clamp(2.5, 6.0);
    let mk = |pts: Vec<egui::Pos2>, st: Stroke| if closed { Shape::closed_line(pts, st) } else { Shape::line(pts, st) };
    p.add(mk(part.iter().map(|q| *q + vec2(0.0, 2.5)).collect(), Stroke::new(w + 4.0, rgba(0, 0, 0, 0.30))));
    p.add(mk(part.clone(), Stroke::new(w + 3.0, theme::OUTLINE)));
    p.add(mk(part.clone(), Stroke::new(w, line)));
    if let Some(&s) = pts.first() {
        p.circle_filled(s, w * 0.95 + 1.5, Color32::WHITE);
        p.circle_filled(s, w * 0.95 - 0.5, theme::ACCENT);
    }
}

/// A track card. `badge`: a round number (Grand Prix order). Returns the response.
#[allow(clippy::too_many_arguments)]
pub fn card(ui: &mut Ui, rect: Rect, salt: &str, info: &TrackInfo, selected: bool, best: Option<f32>, badge: Option<usize>, clickable: bool, anim: f32) -> Response {
    let id = Id::new(("stampede-track-card", salt));
    let resp = ui.interact(rect, id, if clickable { Sense::click() } else { Sense::hover() });
    let (scale, _) = if clickable { widgets::press_fx(ui, id, &resp) } else { (1.0, 0.0) };
    let sel = ui.ctx().animate_bool_with_time(id.with("sel"), selected, 0.14);
    let rect = Rect::from_center_size(rect.center(), rect.size() * (scale * (1.0 + 0.008 * sel)));
    let p = ui.painter().clone();
    let r = 16.0;
    widgets::soft_shadow(&p, rect, r, 6.0, 16.0, 0.55);
    // scenery: the track's own sky into its ground
    let sky_h = rect.height() * 0.70;
    let top = theme::mix(info.sky_top, theme::INDIGO, if info.night { 0.0 } else { 0.28 });
    let hor = theme::mix(info.sky_horizon, theme::BURNT, if info.night { 0.2 } else { 0.22 });
    widgets::rrect_gradient(&p, rect, r, top, theme::mix(hor, info.ground, 0.5));
    if info.night {
        for i in 0..14u32 {
            let q = pos2(rect.left() + art::hash01(i * 3) * rect.width(), rect.top() + art::hash01(i * 3 + 1) * sky_h * 0.55);
            p.circle_filled(q, 0.8, rgba(255, 240, 220, 0.5));
        }
    }
    // minimap
    let map_area = Rect::from_min_max(rect.min + vec2(14.0, 10.0), pos2(rect.right() - 14.0, rect.top() + rect.height() * 0.64));
    minimap(&p, map_area, info, theme::mix(info.road, Color32::WHITE, 0.72), art::ease_out_cubic(anim));
    // text strip
    let strip = Rect::from_min_max(pos2(rect.left(), rect.bottom() - rect.height() * 0.34), rect.max);
    p.rect_filled(strip, CornerRadius { nw: 0, ne: 0, sw: r as u8, se: r as u8 }, rgba(14, 5, 30, 0.80));
    p.hline(strip.x_range(), strip.top(), Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)));
    let size = art::fit_size(&p, &info.name, theme::title, 17.0, strip.width() - 20.0);
    p.text(pos2(strip.left() + 11.0, strip.top() + strip.height() * 0.34), Align2::LEFT_CENTER, &info.name, theme::title(size), theme::TEXT);
    let y2 = strip.top() + strip.height() * 0.76;
    let len = if info.id.is_arena() { format!("{:.0} m loop", info.length) } else { km(info.length) };
    p.text(pos2(strip.left() + 11.0, y2), Align2::LEFT_CENTER, len, theme::ui_bold(11.5), theme::SAND);
    let (txt, col) = match best {
        None if info.id.is_arena() => (String::new(), theme::TEXT_DIM),
        Some(t) => (super::common::fmt_time(t), theme::GOLD),
        None => ("no time yet".to_string(), theme::TEXT_DIM),
    };
    let g = p.layout_no_wrap(txt, theme::ui_bold(11.5), col);
    let gx = strip.right() - 11.0 - g.size().x;
    p.galley(pos2(gx, y2 - g.size().y / 2.0), g, col);
    if best.is_some() {
        icons::draw(&p, Icon::Stopwatch, pos2(gx - 9.0, y2), 13.0, theme::GOLD);
    }
    // rim
    p.rect_stroke(rect, CornerRadius::same(r as u8), Stroke::new(2.0_f32, theme::mix_rgba(theme::OUTLINE, theme::GOLD, sel)), StrokeKind::Inside);
    if sel > 0.01 {
        p.rect_stroke(rect.expand(1.5), CornerRadius::same(r as u8 + 1), Stroke::new(2.5 * sel, fade(theme::GOLD, sel)), StrokeKind::Outside);
        art::glow(&p, rect.center(), rect.width() * 0.8, rgba(0xFF, 0xC8, 0x3D, 0.06 * sel));
    }
    if let Some(n) = badge {
        let c = rect.left_top() + vec2(20.0, 20.0);
        p.circle_filled(c + vec2(0.0, 1.5), 14.0, rgba(0, 0, 0, 0.4));
        p.circle_filled(c, 14.0, theme::OUTLINE);
        p.circle_filled(c, 12.0, theme::ACCENT);
        p.text(c + vec2(0.0, 0.5), Align2::CENTER_CENTER, format!("{}", n), theme::title(16.0), Color32::WHITE);
    }
    resp
}
