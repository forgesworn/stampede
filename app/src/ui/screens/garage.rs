//! Garage: browse the eight ostriches (arrows, thumbnail strip, drag to turn the
//! turntable), see their class stat bars and colours, and pick one.
//! The 3D model comes from `game::stage::show_garage`; without it the screen
//! paints a spotlit turntable with the brand head.
//!
//! Each ostrich also has colour variants: tap a swatch to preview it on the turntable, buy the
//! locked ones with the ₿ wallet (after a confirm), and the one worn is the one raced in.

use super::common::{self, StagePaint};
use super::{Ctx, Screen, Transition};
use crate::render::ScenePass;
use crate::ui::art::{self, ease_out_cubic, fade, from_f3, phase, rgba};
use crate::ui::progress::{self, grouped, BuyError, LASER_GOLD, VARIANTS, VARIANT_PRICE};
use crate::ui::progress_ui;
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Btn, S3};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Id, Rect, Sense, Stroke};
use race_core::{Accessory, CharClass, Character, CHARACTERS};

pub struct GarageScreen {
    t: f32,
    cur: usize,
    yaw: f32,
    spin: f32,
    /// Seconds since the shown ostrich changed.
    swap: f32,
    /// Direction of the last change (slides the panel).
    dir: f32,
    stage: StagePaint,
    inited: bool,
    /// The colour variant on show (0 = original).
    var: u8,
    /// Asking "buy this?".
    confirm: bool,
    /// Seconds since a purchase (the celebration).
    bought_t: f32,
    /// Seconds since a purchase was refused (shakes the wallet).
    refused_t: f32,
    /// Screenshots: seed the profile with a wallet (and owned variants).
    seed: Option<u8>,
}

impl Default for GarageScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl GarageScreen {
    pub fn new() -> GarageScreen {
        GarageScreen { t: 0.0, cur: 0, yaw: 0.5, spin: 0.0, swap: 1.0, dir: 1.0, stage: StagePaint::default(), inited: false, var: 0, confirm: false, bought_t: 9.0, refused_t: 9.0, seed: None }
    }

    /// Browsing a different ostrich (screenshots).
    pub fn sample(cur: usize) -> GarageScreen {
        GarageScreen { cur, inited: true, ..GarageScreen::new() }
    }

    /// A locked variant on show with too little ₿ (screenshots).
    pub fn locked_sample() -> GarageScreen {
        GarageScreen { cur: 4, var: 2, seed: Some(1), inited: true, ..GarageScreen::new() }
    }

    /// "Buy Midnight?" (screenshots).
    pub fn buy_confirm_sample() -> GarageScreen {
        GarageScreen { cur: 0, var: 1, seed: Some(2), confirm: true, inited: true, ..GarageScreen::new() }
    }

    fn go(&mut self, to: usize, dir: f32, profile: &crate::platform::data_dir::Profile) {
        self.cur = to % 8;
        self.dir = dir;
        self.swap = 0.0;
        self.var = progress::equipped(profile, self.cur as u8);
        self.confirm = false;
    }
}

fn accessory_name(a: Accessory) -> &'static str {
    match a {
        Accessory::Scarf => "Red scarf",
        Accessory::Goggles => "Goggles",
        Accessory::Crown => "Crown",
        Accessory::StarHeadband => "Star headband",
        Accessory::VikingHelmet => "Viking helmet",
        Accessory::Mohawk => "Mohawk",
        Accessory::GoldChainShades => "Gold chain and shades",
        Accessory::Cap => "Cap",
    }
}

fn class_info(c: CharClass) -> (&'static str, &'static str) {
    match c {
        CharClass::Light => ("Light", "Quick off the line and nimble. Gets shoved around."),
        CharClass::Medium => ("Medium", "Balanced all round. A safe pick."),
        CharClass::Heavy => ("Heavy", "Hard to stop. Slow to turn."),
    }
}

/// (label, value for a class, [lo, hi] across classes) for the four bars.
fn stat_rows(c: CharClass) -> [(&'static str, f32, f32, f32); 4] {
    let all = [CharClass::Light, CharClass::Medium, CharClass::Heavy].map(|k| k.mods());
    let m = c.mods();
    let range = |f: fn(&race_core::ClassMods) -> f32| {
        let v: Vec<f32> = all.iter().map(f).collect();
        (v.iter().cloned().fold(f32::MAX, f32::min), v.iter().cloned().fold(f32::MIN, f32::max))
    };
    let (sl, sh) = range(|m| m.top);
    let (al, ah) = range(|m| m.accel);
    let (tl, th) = range(|m| m.turn);
    let (bl, bh) = range(|m| m.mass);
    [("Speed", m.top, sl, sh), ("Accel", m.accel, al, ah), ("Handling", m.turn, tl, th), ("Mass", m.mass, bl, bh)]
}

impl Screen for GarageScreen {
    fn name(&self) -> &'static str {
        "garage"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !self.inited {
            self.inited = true;
            self.cur = ctx.profile.character.min(7) as usize;
            self.var = progress::equipped(ctx.profile, self.cur as u8);
        }
        if let Some(k) = self.seed.take() {
            ctx.profile.progress.sats = 1_840;
            if k == 1 {
                ctx.profile.progress.variants_owned[4] = 0b010;
            }
        }
        self.swap += ctx.dt;
        self.bought_t += ctx.dt;
        self.refused_t += ctx.dt;
        // the turntable idles slowly, a flick keeps it spinning for a moment
        self.yaw += (0.35 + self.spin) * ctx.dt;
        self.spin *= (1.0 - 2.2 * ctx.dt).max(0.0);
        let mut attire = progress::attire(ctx.profile, self.cur as u8);
        attire[0] = self.var;
        crate::game::stage::show_garage_attire(self.cur as u8, attire, self.yaw);
        // the stats panel covers the right 40%: the ostrich stands in the free part
        crate::game::stage::set_focus(if common::Layout::new(ctx.viewport.safe).portrait { 0.5 } else { 0.29 });
        if common::Layout::new(ctx.viewport.safe).portrait { crate::game::stage::set_portrait_focus(0.29, 2.1); }
        if ctx.keys.pressed(crate::platform::KeyCode::ArrowRight) {
            feedback::tap(ctx);
            self.go(self.cur + 1, 1.0, ctx.profile);
        }
        if ctx.keys.pressed(crate::platform::KeyCode::ArrowLeft) {
            feedback::tap(ctx);
            self.go(self.cur + 7, -1.0, ctx.profile);
        }
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        if std::mem::take(&mut self.confirm) {
            return Transition::None;
        }
        Transition::Pop
    }

    fn paint_scene(&mut self, pass: &mut ScenePass) {
        self.stage.paint(pass);
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        let over3d = crate::game::stage::is_drawing();
        let (cur, mut yaw, mut spin) = (self.cur, self.yaw, self.spin);
        let swap = self.swap;
        let dir = self.dir;
        let mut change: Option<(usize, f32)> = None;
        let (var, confirm, bought_t, refused_t) = (self.var, self.confirm, self.bought_t, self.refused_t);
        let ch_id = cur as u8;
        let owned: [bool; VARIANTS] = std::array::from_fn(|v| progress::owns(ctx.profile, ch_id, v as u8));
        let worn = progress::equipped(ctx.profile, ch_id);
        let (sats, selected_char) = (ctx.profile.progress.sats, ctx.profile.character);
        let golds = ctx.profile.challenge_medals.iter().filter(|m| **m >= 3).count();
        let worn_all: [u8; 8] = std::array::from_fn(|i| progress::equipped(ctx.profile, i as u8));
        let (mut new_var, mut want_buy, mut want_select, mut do_buy, mut cancel_buy) = (var, false, false, false, false);
        common::page(ctx.viewport.screen, tm, ui, 0.42, |ui, brand| {
            let hd = widgets::header(ui, safe, "Garage", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let portrait = common::Layout::new(safe).portrait;
            let wardrobe = Rect::from_center_size(pos2(if portrait { safe.left() + 74.0 } else { safe.center().x }, if portrait { hd.body.top() + 20.0 } else { safe.top() + 30.0 }), vec2(112.0, 38.0));
            if widgets::button(ui, wardrobe, "garage-wardrobe", &Btn::new("Attire").size(15.0)).clicked() {
                feedback::tap(ctx);
                out = Transition::Push(Box::new(super::wardrobe::WardrobeScreen::new()));
            }
            let mut body = hd.body;
            if portrait { body.min.y += 48.0; }
            {
                let hy = safe.top() + widgets::S2 + 2.0 + widgets::TAP_MIN / 2.0;
                let shake = if refused_t < 0.45 { (refused_t * 60.0).sin() * 5.0 * (1.0 - refused_t / 0.45) } else { 0.0 };
                progress_ui::wallet_pill_right(ui, brand, safe.right() - widgets::S4 + shake, hy, 30.0, sats, "garage", tm);
            }
            let pw = 292.0_f32.min(body.width() * 0.40);
            let (stage, stats) = common::Layout::new(safe).split(body, pw, body.height() * 0.48, S3);
            let ch: &Character = &CHARACTERS[cur];
            let accent = from_f3(ch.accent);
            let pal = progress::palette(ch_id, var);
            let neck = from_f3(pal.map_or(ch.neck, |p| p.neck));

            // ── the stage (drag to turn) ──
            let drag = ui.interact(stage, Id::new("garage-turntable"), Sense::drag());
            if drag.dragged() {
                yaw += drag.drag_delta().x * 0.014;
                spin = (drag.drag_delta().x * 0.014 / ctx.dt.max(0.005)).clamp(-9.0, 9.0) * 0.5;
            }
            let e_stage = widgets::enter(t, 1);
            let p = ui.painter().clone();
            {
                let mut sp = p.clone();
                sp.set_opacity(e_stage);
                let floor_c = pos2(stage.center().x, stage.bottom() - stage.height() * 0.17);
                if !over3d {
                    // spotlight, glow, turntable
                    let beam = vec![pos2(floor_c.x - 28.0, stage.top() - 6.0), pos2(floor_c.x + 28.0, stage.top() - 6.0), pos2(floor_c.x + stage.width() * 0.34, floor_c.y), pos2(floor_c.x - stage.width() * 0.34, floor_c.y)];
                    let mut m = egui::epaint::Mesh::default();
                    let uv = egui::epaint::WHITE_UV;
                    let cols = [rgba(255, 240, 220, 0.14), rgba(255, 240, 220, 0.14), Color32::TRANSPARENT, Color32::TRANSPARENT];
                    for (q, c) in beam.iter().zip(cols) {
                        m.vertices.push(egui::epaint::Vertex { pos: *q, uv, color: c });
                    }
                    m.indices.extend([0, 1, 2, 0, 2, 3]);
                    sp.add(egui::Shape::mesh(m));
                    art::glow(&sp, floor_c - vec2(0.0, stage.height() * 0.33), stage.height() * 0.62, fade(accent, 0.20));
                    let (rx, ry) = (stage.width() * 0.36, stage.height() * 0.085);
                    art::glow_ellipse(&sp, floor_c + vec2(0.0, 6.0), rx * 1.5, ry * 2.4, rgba(0, 0, 0, 0.38));
                    for (k, (a, col)) in [(1.0, rgba(0x2A, 0x0F, 0x48, 0.95)), (0.86, rgba(0x4A, 0x20, 0x78, 0.95)), (0.78, rgba(0x20, 0x0B, 0x38, 0.95))].iter().enumerate() {
                        let pts: Vec<egui::Pos2> = (0..48).map(|i| {
                            let ang = i as f32 / 48.0 * std::f32::consts::TAU;
                            floor_c + vec2(ang.cos() * rx * a, ang.sin() * ry * a + (2 - k as i32) as f32 * 1.5)
                        }).collect();
                        sp.add(egui::Shape::convex_polygon(pts, *col, Stroke::NONE));
                    }
                    // ticks that turn with the yaw
                    for i in 0..24 {
                        let ang = yaw * 1.0 + i as f32 / 24.0 * std::f32::consts::TAU;
                        let (c, s) = (ang.cos(), ang.sin());
                        if s > 0.0 {
                            let a = pos2(floor_c.x + c * rx * 0.97, floor_c.y + s * ry * 0.97);
                            let b = pos2(floor_c.x + c * rx * 0.88, floor_c.y + s * ry * 0.88);
                            sp.line_segment([a, b], Stroke::new(1.6_f32, fade(accent, 0.8 * s)));
                        }
                    }
                    // the ostrich: the brand head, turned by squeezing it
                    let hs = stage.height() * 0.92;
                    let turn = yaw.cos();
                    let wdt = hs * (0.28 + 0.72 * turn.abs());
                    let tint = theme::mix(Color32::WHITE, neck, 0.55);
                    let bob = (tm * 1.7).sin() * 2.0;
                    art::sprite(&sp, brand.head.id(), pos2(floor_c.x + (1.0 - turn.abs()) * 6.0 * turn.signum(), floor_c.y - hs * 0.42 + bob), vec2(wdt, hs), 0.0, turn < 0.0, tint);
                }
                // drag hint
                let hint_a = (1.0 - phase(t, 2.5, 1.0)).max(0.0) * phase(t, 0.8, 0.4);
                if hint_a > 0.0 && !drag.dragged() {
                    sp.text(pos2(stage.center().x, stage.bottom() - 6.0), Align2::CENTER_CENTER, "drag to turn", theme::ui_bold(11.5), fade(theme::SAND, 0.8 * hint_a));
                }
            }

            // arrows
            for (k, side) in [(-1.0_f32, "garage-prev"), (1.0, "garage-next")] {
                let c = pos2(if k < 0.0 { stage.left() + 38.0 } else { stage.right() - 30.0 }, stage.center().y);
                let r = Rect::from_center_size(c, vec2(46.0, 56.0));
                let resp = widgets::staged(ui, widgets::enter(t, 2), vec2(0.0, 0.0), |ui, _| widgets::button(ui, r, side, &Btn::new("").icon(if k < 0.0 { Icon::ChevronLeft } else { Icon::ChevronRight })));
                if resp.clicked() {
                    feedback::tap(ctx);
                    change = Some(((cur + if k < 0.0 { 7 } else { 1 }) % 8, k));
                }
            }

            // thumbnail strip
            let strip_y = stage.bottom() - 6.0 - 14.0;
            let sx0 = stage.center().x - 8.0 * 17.0;
            for i in 0..8usize {
                let c = pos2(sx0 + 17.0 + i as f32 * 34.0, strip_y - 12.0);
                let hit = ui.interact(Rect::from_center_size(c, vec2(34.0, 34.0)), Id::new(("garage-thumb", i)), Sense::click());
                let lit = ui.ctx().animate_bool_with_time(Id::new(("garage-thumb-lit", i)), i == cur, 0.14);
                let pp = ui.painter().clone();
                if lit > 0.01 {
                    pp.circle_stroke(c, 14.0, Stroke::new(2.5 * lit, fade(theme::GOLD, lit)));
                }
                widgets::chip(&pp, c, 9.0 + 1.5 * lit, from_f3(progress::palette(i as u8, worn_all[i]).map_or(CHARACTERS[i].body, |p| p.body)));
                if CHARACTERS[i].id == ctx.profile.character {
                    icons::draw(&pp, Icon::Check, c + vec2(8.0, -8.0), 10.0, theme::MINT);
                }
                if hit.clicked() && i != cur {
                    feedback::tap(ctx);
                    change = Some((i, if i > cur { 1.0 } else { -1.0 }));
                }
            }

            // ── the info panel ──
            let e = widgets::enter(t, 3);
            let slide = (1.0 - ease_out_cubic(phase(swap, 0.0, 0.28))) * 26.0 * dir;
            widgets::staged(ui, e, vec2(60.0, 0.0), |ui, off| {
                let col = stats.translate(off);
                let p = ui.painter().clone();
                widgets::panel(&p, col, 20.0);
                let inner = col.shrink2(vec2(14.0, 12.0));
                let a = ease_out_cubic(phase(swap, 0.0, 0.25));
                let mut ip = p.clone();
                ip.set_opacity(a.max(0.0));
                // name
                let size = art::fit_size(&ip, ch.name, theme::title, 34.0, inner.width() - 4.0);
                widgets::heading(&ip, pos2(inner.left() + slide, inner.top() + 17.0), Align2::LEFT_CENTER, ch.name, size, theme::TEXT);
                if ch.id == ctx.profile.character {
                    widgets::pill(&ip, pos2(inner.right() - 30.0, inner.top() + 15.0), "In use", rgba(0x6B, 0xE0, 0xA8, 0.9), theme::OUTLINE, 11.5);
                }
                // class
                let (cname, cdesc) = class_info(ch.class);
                let cy = inner.top() + 50.0;
                widgets::pill(&ip, pos2(inner.left() + 30.0 + slide, cy), &format!("{cname} class"), accent.gamma_multiply(0.9), if accent.r() as u32 + accent.g() as u32 + accent.b() as u32 > 380 { theme::OUTLINE } else { Color32::WHITE }, 11.5);
                let g = ip.layout(cdesc.to_string(), theme::ui_medium(11.2), theme::TEXT_DIM, inner.width());
                ip.galley(pos2(inner.left() + slide, cy + 12.0), g, theme::TEXT_DIM);
                // accessory, on the class line
                ip.text(pos2(inner.right(), cy), Align2::RIGHT_CENTER, accessory_name(ch.accessory), theme::ui_medium(10.5), theme::TEXT_DIM);
                // bars
                let mut y = inner.top() + 86.0;
                let grow = ease_out_cubic(phase(swap, 0.05, 0.5));
                for (label, v, lo, hi) in stat_rows(ch.class) {
                    ip.text(pos2(inner.left(), y + 7.0), Align2::LEFT_CENTER, label, theme::ui_bold(11.5), theme::SAND);
                    let norm = if hi > lo { 0.16 + 0.84 * (v - lo) / (hi - lo) } else { 0.5 };
                    widgets::stat_bar(&ip, Rect::from_min_size(pos2(inner.left() + 66.0, y + 1.0), vec2(inner.width() - 66.0 - 46.0, 12.0)), norm, grow, 5, Color32::from_rgb(0xFF, 0xB8, 0x44), Color32::from_rgb(0xFF, 0x5A, 0x2A));
                    let pct = ((v - 1.0) * 100.0).round() as i32;
                    let txt = if pct == 0 { "even".to_string() } else { format!("{pct:+}%") };
                    ip.text(pos2(inner.right(), y + 7.0), Align2::RIGHT_CENTER, txt, theme::ui_bold(11.0), if pct > 0 { theme::MINT } else if pct < 0 { Color32::from_rgb(0xFF, 0x8A, 0x7A) } else { theme::TEXT_DIM });
                    y += 18.5;
                }
                // colour variants: four swatches (original, two to buy, Laser Gold)
                y += 6.0;
                let gap = 6.0;
                let sw = (inner.width() - gap * (VARIANTS as f32 - 1.0)) / VARIANTS as f32;
                let sh = 30.0;
                for v in 0..VARIANTS {
                    let r = Rect::from_min_size(pos2(inner.left() + v as f32 * (sw + gap), y), vec2(sw, sh));
                    let id = Id::new(("garage-variant", v));
                    let resp = ui.interact(r, id, Sense::click());
                    let (scale, sink) = widgets::press_fx(ui, id, &resp);
                    let r = Rect::from_center_size(r.center() + vec2(0.0, sink), r.size() * scale);
                    let sel = v as u8 == var;
                    let lit = ui.ctx().animate_bool_with_time(id.with("lit"), sel, 0.12);
                    let pal_v = progress::palette(ch_id, v as u8);
                    let cols = pal_v.map_or([ch.body, ch.neck, ch.accent], |p| [p.body, p.neck, p.accent]);
                    ip.rect_filled(r, 10.0, rgba(10, 4, 24, 0.55));
                    ip.rect_stroke(r, egui::CornerRadius::same(10), Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)), egui::StrokeKind::Inside);
                    let dim = if owned[v] { 1.0 } else { 0.9 };
                    for (k, c) in cols.iter().enumerate() {
                        widgets::chip(&ip, pos2(r.center().x + (k as f32 - 1.0) * 11.0, r.center().y + 1.0), 8.0 - k as f32 * 0.4, fade(from_f3(*c), dim));
                    }
                    if lit > 0.01 {
                        ip.rect_stroke(r.expand(1.5), egui::CornerRadius::same(11), Stroke::new(2.2_f32 * lit, fade(theme::GOLD, lit)), egui::StrokeKind::Outside);
                    }
                    if !owned[v] {
                        let lc = pos2(r.right() - 7.5, r.top() + 7.5);
                        ip.circle_filled(lc, 7.0, rgba(10, 3, 22, 0.85));
                        icons::draw(&ip, Icon::Lock, lc, 10.0, theme::SAND);
                    } else if worn == v as u8 {
                        let lc = pos2(r.right() - 7.5, r.top() + 7.5);
                        ip.circle_filled(lc, 7.0, theme::MINT);
                        icons::draw(&ip, Icon::Check, lc, 9.5, theme::OUTLINE);
                    }
                    if resp.clicked() && !confirm {
                        feedback::tap(ctx);
                        new_var = v as u8;
                    }
                }
                // what is on show: its name, and owned / the price
                y += sh + 12.0;
                let vname = progress::variant_name(ch_id, var);
                ip.text(pos2(inner.left(), y), Align2::LEFT_CENTER, vname, theme::ui_bold(12.5), theme::TEXT);
                let vi = var as usize;
                if owned[vi] {
                    let (txt, col) = if worn == var { ("Wearing", theme::MINT) } else { ("Owned", theme::SAND) };
                    ip.text(pos2(inner.right(), y), Align2::RIGHT_CENTER, txt, theme::ui_bold(11.5), col);
                } else if var == LASER_GOLD {
                    ip.text(pos2(inner.right(), y), Align2::RIGHT_CENTER, format!("{golds} of 10 golds"), theme::ui_bold(11.5), theme::TEXT_DIM);
                } else {
                    let price = VARIANT_PRICE[vi];
                    let w = progress_ui::amount_width(&ip, price, 12.0);
                    progress_ui::amount(&ip, brand, pos2(inner.right() - w, y), price, 12.0, if sats >= price { theme::GOLD } else { Color32::from_rgb(0xFF, 0x8A, 0x7A) });
                }
                // the action
                let by = inner.bottom() - 40.0;
                let br = Rect::from_min_max(pos2(inner.left(), by), inner.max);
                if owned[vi] {
                    let chosen = ch.id == selected_char && worn == var;
                    let b = Btn { label: if chosen { "Selected" } else { "Select" }, ..Btn::primary("") }.enabled(!chosen).icon(if chosen { Icon::Check } else { Icon::Ostrich });
                    if widgets::button(ui, br, "garage-select", &b).clicked() && !confirm {
                        want_select = true;
                    }
                } else if var == LASER_GOLD {
                    widgets::button(ui, br, "garage-gold", &Btn::new("Gold in every challenge").enabled(false).icon(Icon::Lock));
                } else {
                    let price = VARIANT_PRICE[vi];
                    if sats >= price {
                        let label = format!("Buy {}", grouped(price));
                        if widgets::button(ui, br, "garage-buy", &Btn::primary(&label)).clicked() && !confirm {
                            want_buy = true;
                        }
                        art::coin(&ip, brand, pos2(br.right() - 24.0, br.center().y - 1.0), 24.0, None, 1.0);
                    } else {
                        let label = format!("Need {} more", grouped(price - sats));
                        widgets::button(ui, br, "garage-buy", &Btn::new(&label).enabled(false).icon(Icon::Lock));
                    }
                }
            });
            // ── bought! sparkles over the stage ──
            if bought_t < 2.2 {
                let k = phase(bought_t, 0.0, 0.3);
                let mut bp = p.clone();
                bp.set_opacity(k * (1.0 - phase(bought_t, 1.6, 0.6)));
                widgets::pill(&bp, pos2(stage.left() + 70.0, stage.top() + 16.0), &format!("{} unlocked!", progress::variant_name(ch_id, worn)), theme::GOLD, theme::OUTLINE, 13.0 + 2.0 * (1.0 - k));
                widgets::confetti(&p, brand, stage, bought_t, 28);
            } else if !owned[var as usize] && !confirm {
                let mut bp = p.clone();
                bp.set_opacity(widgets::enter(t, 2));
                widgets::pill(&bp, pos2(stage.left() + 70.0, stage.top() + 16.0), "Preview: locked", rgba(10, 3, 22, 0.75), theme::SAND, 11.5);
            }
            // ── "buy this?" ──
            if confirm {
                let screen = ctx.viewport.screen;
                let dim = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Middle, Id::new("garage-dim")));
                dim.rect_filled(screen, 0.0, Color32::from_black_alpha(165));
                let card = Rect::from_center_size(screen.center(), vec2(screen.width().min(360.0), 176.0));
                let price = VARIANT_PRICE[(var as usize).min(2)];
                egui::Area::new(Id::new("garage-confirm")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ui.ctx(), |ui| {
                    ui.set_min_size(screen.size());
                    // swallow taps outside the card so nothing underneath is pressed
                    ui.interact(screen, Id::new("garage-confirm-block"), Sense::click_and_drag());
                    let p = ui.painter().clone();
                    widgets::panel_solid(&p, card, 22.0);
                    let name = progress::variant_name(ch_id, var);
                    let size = art::fit_size(&p, &format!("Unlock {name}?"), theme::title, 26.0, card.width() - 40.0);
                    widgets::heading(&p, pos2(card.center().x, card.top() + 28.0), Align2::CENTER_CENTER, &format!("Unlock {name}?"), size, theme::TEXT);
                    // the three colours, then who it is for
                    if let Some(pv) = progress::palette(ch_id, var) {
                        for (k, c) in [pv.body, pv.neck, pv.accent].iter().enumerate() {
                            widgets::chip(&p, pos2(card.left() + 34.0 + k as f32 * 22.0, card.top() + 66.0), 9.0, from_f3(*c));
                        }
                    }
                    p.text(pos2(card.left() + 100.0, card.top() + 66.0), Align2::LEFT_CENTER, format!("{} colours", ch.name), theme::ui_bold(13.0), theme::SAND);
                    let w = progress_ui::amount_width(&p, price, 16.0);
                    progress_ui::amount(&p, brand, pos2(card.right() - 22.0 - w, card.top() + 66.0), price, 16.0, theme::GOLD);
                    p.text(pos2(card.center().x, card.top() + 92.0), Align2::CENTER_CENTER, format!("You have {}, {} left after", grouped(sats), grouped(sats.saturating_sub(price))), theme::ui_medium(11.5), theme::TEXT_DIM);
                    let bw = (card.width() - 48.0 - widgets::S3) / 2.0;
                    let by = card.bottom() - 16.0 - 44.0;
                    if widgets::button(ui, Rect::from_min_size(pos2(card.left() + 24.0, by), vec2(bw, 44.0)), "garage-confirm-no", &Btn::new("Not now")).clicked() {
                        cancel_buy = true;
                    }
                    if widgets::button(ui, Rect::from_min_size(pos2(card.left() + 24.0 + bw + widgets::S3, by), vec2(bw, 44.0)), "garage-confirm-yes", &Btn::primary("Buy")).clicked() {
                        do_buy = true;
                    }
                });
            }
        });
        self.yaw = yaw;
        self.spin = spin;
        if let Some((to, d)) = change {
            self.go(to, d, ctx.profile);
        } else {
            self.cur = cur;
            self.var = new_var;
        }
        if want_buy {
            feedback::select(ctx);
            self.confirm = true;
        }
        if cancel_buy {
            feedback::back(ctx);
            self.confirm = false;
        }
        if want_select {
            feedback::select(ctx);
            ctx.profile.character = ch_id;
            progress::equip(ctx.profile, ch_id, self.var);
            ctx.save_profile();
        }
        if do_buy {
            self.confirm = false;
            // checks the price and deducts it in one step; the wallet and the unlock reach the disk together
            match progress::buy(ctx.profile, ch_id, self.var) {
                Ok(_) => {
                    progress::equip(ctx.profile, ch_id, self.var);
                    ctx.save_profile();
                    feedback::bought(ctx);
                    self.bought_t = 0.0;
                }
                Err(BuyError::Short(n)) => {
                    ctx.say(format!("Need {} more", grouped(n)));
                    self.refused_t = 0.0;
                }
                Err(_) => {}
            }
        }
        out
    }
}
