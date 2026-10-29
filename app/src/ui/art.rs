//! Brand art and painted scenery: the four brand textures, gradients, the
//! sunset-desert backdrop every menu sits on, chunky outlined display text and
//! the gradient wordmark. Everything is a pure function of time, so a `--shot`
//! frame is exactly what a phone shows at that moment.

use crate::ui::theme;
use egui::epaint::{Mesh, Vertex};
use egui::{pos2, vec2, Align2, Color32, ColorImage, Context, FontId, Id, Painter, Pos2, Rect, Shape, Stroke, TextureHandle, TextureId, TextureOptions};
use std::sync::Arc;

// ── brand textures ──────────────────────────────────────────────────────────

static OSTRICH_RUN: &[u8] = include_bytes!("../../assets/brand/ostrich_run.png");
static HEAD_PROFILE: &[u8] = include_bytes!("../../assets/brand/head_profile.png");
static DUST_PUFF: &[u8] = include_bytes!("../../assets/brand/dust_puff.png");
static BTC_COIN: &[u8] = include_bytes!("../../assets/brand/btc_coin.png");

/// The brand images as egui textures (loaded once per egui context).
pub struct Brand {
    /// White running-ostrich silhouette (tint it), 1024x700, faces right.
    pub run: TextureHandle,
    /// Ostrich head in profile with laser eyes, 1024x1024, faces right.
    pub head: TextureHandle,
    /// Soft dust cloud (tint it), 512x512.
    pub dust: TextureHandle,
    /// Bitcoin coin, 256x256.
    pub coin: TextureHandle,
}

/// A game's icon from the catalog (a PNG bundled in the app) as a texture, loaded once per key.
pub fn game_icon(ctx: &Context, key: &str, png: &'static [u8]) -> TextureHandle {
    let id = Id::new(("stampede-game-icon", key));
    if let Some(t) = ctx.data(|d| d.get_temp::<TextureHandle>(id)) {
        return t;
    }
    let t = ctx.load_texture(format!("game-icon-{key}"), decode(png), TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    t
}

fn decode(bytes: &[u8]) -> ColorImage {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let blank = || ColorImage::filled([4, 4], Color32::TRANSPARENT);
    let Ok(mut r) = dec.read_info() else { return blank() };
    let Some(n) = r.output_buffer_size() else { return blank() };
    let mut buf = vec![0u8; n];
    let Ok(info) = r.next_frame(&mut buf) else { return blank() };
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..w * h * 4].to_vec(),
        png::ColorType::Rgb => buf[..w * h * 3].chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf[..w * h * 2].chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        _ => buf[..w * h].iter().flat_map(|&g| [g, g, g, 255]).collect(),
    };
    ColorImage::from_rgba_unmultiplied([w, h], &rgba)
}

/// The brand textures, loading them on first use. Cheap afterwards.
pub fn brand(ctx: &Context) -> Arc<Brand> {
    let id = Id::new("stampede-brand-textures");
    if let Some(b) = ctx.data(|d| d.get_temp::<Arc<Brand>>(id)) {
        return b;
    }
    let opts = TextureOptions::LINEAR;
    let b = Arc::new(Brand {
        run: ctx.load_texture("brand-run", decode(OSTRICH_RUN), opts),
        head: ctx.load_texture("brand-head", decode(HEAD_PROFILE), opts),
        dust: ctx.load_texture("brand-dust", decode(DUST_PUFF), opts),
        coin: ctx.load_texture("brand-coin", decode(BTC_COIN), opts),
    });
    ctx.data_mut(|d| d.insert_temp(id, b.clone()));
    b
}

// ── small helpers ───────────────────────────────────────────────────────────

/// 0..1 hash of an integer (stable, no state).
pub fn hash01(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297A_2D39);
    x ^= x >> 15;
    (x & 0xFFFF) as f32 / 65535.0
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = 1.0 - t.clamp(0.0, 1.0);
    1.0 - t * t * t
}

pub fn ease_out_back(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0) - 1.0;
    1.0 + 2.70158 * t * t * t + 1.70158 * t * t
}

/// Progress of an animation that starts at `delay` and runs `dur` seconds.
pub fn phase(t: f32, delay: f32, dur: f32) -> f32 {
    ((t - delay) / dur).clamp(0.0, 1.0)
}

/// A colour with its alpha scaled (premultiplied-safe).
pub fn fade(c: Color32, a: f32) -> Color32 {
    c.gamma_multiply(a.clamp(0.0, 1.0))
}

pub fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Opaque colour of a 0..1 float triple (race-core palettes).
pub fn from_f3(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}

fn push_quad(m: &mut Mesh, corners: [Pos2; 4], uvs: [Pos2; 4], cols: [Color32; 4]) {
    let i = m.vertices.len() as u32;
    for k in 0..4 {
        m.vertices.push(Vertex { pos: corners[k], uv: uvs[k], color: cols[k] });
    }
    m.indices.extend([i, i + 1, i + 2, i, i + 2, i + 3]);
}

/// A vertical multi-stop gradient filling `r`. Stops are `(0..1 down the rect, colour)`.
pub fn vgradient(p: &Painter, r: Rect, stops: &[(f32, Color32)]) {
    if stops.len() < 2 || r.height() <= 0.0 {
        return;
    }
    let mut m = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    for w in stops.windows(2) {
        let (y0, y1) = (r.top() + w[0].0 * r.height(), r.top() + w[1].0 * r.height());
        push_quad(&mut m, [pos2(r.left(), y0), pos2(r.right(), y0), pos2(r.right(), y1), pos2(r.left(), y1)], [pos2(uv.x, uv.y); 4], [w[0].1, w[0].1, w[1].1, w[1].1]);
    }
    p.add(Shape::mesh(m));
}

/// A horizontal gradient filling `r`.
pub fn hgradient(p: &Painter, r: Rect, left: Color32, right: Color32) {
    let mut m = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    push_quad(&mut m, [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()], [pos2(uv.x, uv.y); 4], [left, right, right, left]);
    p.add(Shape::mesh(m));
}

/// A soft round glow: `color` at the centre fading to nothing at `radius`.
pub fn glow(p: &Painter, c: Pos2, radius: f32, color: Color32) {
    let mut m = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    m.vertices.push(Vertex { pos: c, uv, color });
    let n = 40;
    for i in 0..n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        m.vertices.push(Vertex { pos: c + vec2(a.cos(), a.sin()) * radius, uv, color: Color32::TRANSPARENT });
    }
    for i in 0..n as u32 {
        m.indices.extend([0, 1 + i, 1 + (i + 1) % n as u32]);
    }
    p.add(Shape::mesh(m));
}

/// A glow squashed to an ellipse (horizon haze).
pub fn glow_ellipse(p: &Painter, c: Pos2, rx: f32, ry: f32, color: Color32) {
    let mut m = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    m.vertices.push(Vertex { pos: c, uv, color });
    let n = 40;
    for i in 0..n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        m.vertices.push(Vertex { pos: c + vec2(a.cos() * rx, a.sin() * ry), uv, color: Color32::TRANSPARENT });
    }
    for i in 0..n as u32 {
        m.indices.extend([0, 1 + i, 1 + (i + 1) % n as u32]);
    }
    p.add(Shape::mesh(m));
}

/// A textured quad centred on `c`, rotated by `angle` radians, optionally mirrored.
pub fn sprite(p: &Painter, tex: TextureId, c: Pos2, size: egui::Vec2, angle: f32, flip_x: bool, tint: Color32) {
    let (s, co) = angle.sin_cos();
    let rot = |v: egui::Vec2| vec2(v.x * co - v.y * s, v.x * s + v.y * co);
    let h = size / 2.0;
    let corners = [c + rot(vec2(-h.x, -h.y)), c + rot(vec2(h.x, -h.y)), c + rot(vec2(h.x, h.y)), c + rot(vec2(-h.x, h.y))];
    let (u0, u1) = if flip_x { (1.0, 0.0) } else { (0.0, 1.0) };
    let uvs = [pos2(u0, 0.0), pos2(u1, 0.0), pos2(u1, 1.0), pos2(u0, 1.0)];
    let mut m = Mesh::with_texture(tex);
    push_quad(&mut m, corners, uvs, [tint; 4]);
    p.add(Shape::mesh(m));
}

/// A bitcoin coin of height `h` (a spinning coin when `spin` is 0..1).
pub fn coin(p: &Painter, b: &Brand, c: Pos2, h: f32, spin: Option<f32>, alpha: f32) {
    let w = match spin {
        Some(s) => (s * std::f32::consts::TAU).cos().abs().max(0.12) * h,
        None => h,
    };
    sprite(p, b.coin.id(), c, vec2(w, h), 0.0, false, fade(Color32::WHITE, alpha));
}

// ── text treatments ─────────────────────────────────────────────────────────

/// Text with a coloured outline and an optional soft drop shadow, centred (or
/// aligned) on `pos`. Returns the text rect.
#[allow(clippy::too_many_arguments)]
pub fn outlined(p: &Painter, pos: Pos2, anchor: Align2, text: &str, font: FontId, fill: Color32, outline: Color32, width: f32, shadow: f32) -> Rect {
    let galley = p.layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let rect = anchor.anchor_size(pos, galley.size());
    let a = fill.a() as f32 / 255.0;
    if shadow > 0.0 && a > 0.0 {
        let sc = rgba(0, 0, 0, 0.35 * a);
        p.galley_with_override_text_color(rect.min + vec2(0.0, shadow), galley.clone(), sc);
    }
    if width > 0.0 {
        let n = if width > 2.5 { 16 } else { 10 };
        let oc = fade(outline, a);
        for i in 0..n {
            let ang = i as f32 / n as f32 * std::f32::consts::TAU;
            p.galley_with_override_text_color(rect.min + vec2(ang.cos(), ang.sin()) * width, galley.clone(), oc);
        }
    }
    p.galley_with_override_text_color(rect.min, galley, fill);
    rect
}

/// Display text with a vertical gradient fill, a thick plum outline and a drop
/// shadow: the wordmark treatment. `pos` is the centre.
#[allow(clippy::too_many_arguments)]
pub fn gradient_text(p: &Painter, pos: Pos2, text: &str, font: FontId, top: Color32, bottom: Color32, outline: Color32, width: f32, alpha: f32) -> Rect {
    let galley = p.layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let rect = Align2::CENTER_CENTER.anchor_size(pos, galley.size());
    // soft shadow
    p.galley_with_override_text_color(rect.min + vec2(0.0, width * 1.6), galley.clone(), rgba(0, 0, 0, 0.30 * alpha));
    p.galley_with_override_text_color(rect.min + vec2(0.0, width * 1.1), galley.clone(), rgba(0, 0, 0, 0.30 * alpha));
    let n = 18;
    for i in 0..n {
        let ang = i as f32 / n as f32 * std::f32::consts::TAU;
        p.galley_with_override_text_color(rect.min + vec2(ang.cos(), ang.sin()) * width, galley.clone(), fade(outline, alpha));
    }
    // gradient fill: recolour the glyph mesh by height
    let (y0, y1) = (rect.top() + galley.size().y * 0.12, rect.bottom() - galley.size().y * 0.14);
    let [tw, th] = p.fonts(|f| f.font_image_size());
    for row in &galley.rows {
        let mut m = row.row.visuals.mesh.clone();
        let off = rect.min.to_vec2() + row.pos.to_vec2();
        for v in &mut m.vertices {
            v.uv = pos2(v.uv.x / tw as f32, v.uv.y / th as f32);
            v.pos += off;
            let k = ((v.pos.y - y0) / (y1 - y0)).clamp(0.0, 1.0);
            v.color = fade(theme::mix(top, bottom, k), alpha);
        }
        p.add(Shape::mesh(m));
    }
    // a thin highlight along the top edge of the letters
    let hi = galley.clone();
    p.with_clip_rect(Rect::from_min_max(rect.min - vec2(8.0, 8.0), pos2(rect.max.x + 8.0, rect.top() + galley.size().y * 0.30)))
        .galley_with_override_text_color(rect.min + vec2(0.0, -width * 0.18), hi, rgba(255, 244, 200, 0.16 * alpha));
    rect
}

/// The largest font size (at most `size`) at which `text` fits `max_w` in `family`.
pub fn fit_size(p: &Painter, text: &str, mk: fn(f32) -> FontId, size: f32, max_w: f32) -> f32 {
    let mut s = size;
    while s > 8.0 {
        let w = p.layout_no_wrap(text.to_string(), mk(s), Color32::WHITE).size().x;
        if w <= max_w {
            break;
        }
        s -= 0.5;
    }
    s
}

// ── the backdrop ────────────────────────────────────────────────────────────

/// How the painted sky is set (sunset by default).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scene {
    /// Seconds (drives the drift).
    pub t: f32,
    /// 0..1: how high the sun sits (0 = on the horizon, 1 = well up).
    pub sun: f32,
    /// 0..1 dark veil over everything but the sky, for quiet screens.
    pub veil: f32,
    /// Shift the palette towards night (stars, cooler sky).
    pub night: f32,
    /// Draw the sun + mesas (false: just the sky, for modal backdrops).
    pub horizon: bool,
}

impl Scene {
    pub fn dusk(t: f32) -> Scene {
        Scene { t, sun: 0.0, veil: 0.0, night: 0.0, horizon: true }
    }
    pub fn quiet(t: f32) -> Scene {
        Scene { t, sun: 0.0, veil: 0.42, night: 0.0, horizon: true }
    }
}

const SKY_TOP: Color32 = Color32::from_rgb(0x14, 0x08, 0x2E);
const SKY_MID: Color32 = Color32::from_rgb(0x3A, 0x13, 0x6E);
const SKY_PLUM: Color32 = Color32::from_rgb(0x7B, 0x22, 0x6E);
const SKY_ROSE: Color32 = Color32::from_rgb(0xB8, 0x3A, 0x4E);
const SKY_HORIZON: Color32 = Color32::from_rgb(0xE2, 0x6A, 0x24);

fn sky_stops(night: f32) -> [(f32, Color32); 5] {
    let n = |c: Color32| theme::mix(c, theme::BG, night * 0.55);
    [(0.0, n(SKY_TOP)), (0.28, n(SKY_MID)), (0.55, n(SKY_PLUM)), (0.80, n(SKY_ROSE)), (1.0, n(SKY_HORIZON))]
}

fn sky_at(stops: &[(f32, Color32); 5], k: f32) -> Color32 {
    let k = k.clamp(0.0, 1.0);
    for w in stops.windows(2) {
        if k <= w[1].0 {
            let t = (k - w[0].0) / (w[1].0 - w[0].0);
            return theme::mix(w[0].1, w[1].1, t);
        }
    }
    stops[4].1
}

/// A ridge silhouette: periodic height field so it can scroll for ever.
fn ridge(p: &Painter, rect: Rect, base_y: f32, amp: f32, period: f32, scroll: f32, seed: f32, color: Color32, sharp: bool) {
    let mut m = Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    let n = ((rect.width() / 14.0).ceil() as usize).max(8);
    for i in 0..=n {
        let x = rect.left() + rect.width() * i as f32 / n as f32;
        let u = (x + scroll) / period * std::f32::consts::TAU;
        let mut h = (u + seed).sin() * 0.55 + (u * 2.3 + seed * 1.7).sin() * 0.30 + (u * 5.1 + seed * 3.1).sin() * 0.15;
        if sharp {
            // mesas: clip the tops flat
            h = h.min(0.35) + (h - 0.35).max(0.0) * 0.2;
        }
        let y = base_y - amp * (0.5 + 0.5 * h);
        let i0 = m.vertices.len() as u32;
        m.vertices.push(Vertex { pos: pos2(x, y), uv, color });
        m.vertices.push(Vertex { pos: pos2(x, rect.bottom()), uv, color });
        if i > 0 {
            m.indices.extend([i0 - 2, i0 - 1, i0, i0 - 1, i0 + 1, i0]);
        }
    }
    p.add(Shape::mesh(m));
}

/// The sunset-desert scene filling `rect`: sky, stars, striped sun, mesas,
/// ground, drifting dust and a vignette.
pub fn backdrop(p: &Painter, b: &Brand, rect: Rect, sc: Scene) {
    let t = sc.t;
    let p = &p.with_clip_rect(rect);
    let horizon_y = rect.top() + rect.height() * 0.66;
    let sky_rect = Rect::from_min_max(rect.min, pos2(rect.right(), horizon_y + 1.0));
    let stops = sky_stops(sc.night);
    vgradient(p, sky_rect, &stops);

    // stars
    for i in 0..46u32 {
        let x = rect.left() + hash01(i * 3 + 1) * rect.width();
        let y = rect.top() + hash01(i * 3 + 2) * (horizon_y - rect.top()) * 0.55;
        let tw = 0.5 + 0.5 * (t * (0.8 + hash01(i * 7)) + hash01(i * 5) * 6.28).sin();
        let a = (0.25 + 0.55 * tw) * (1.0 - (y - rect.top()) / (horizon_y - rect.top()) * 1.5).max(0.0);
        p.circle_filled(pos2(x, y), 0.6 + hash01(i * 11) * 0.9, rgba(255, 238, 220, a));
    }

    let h = rect.height();
    if sc.horizon {
        // sun: a glow, the disc and retro stripe cuts
        let sun_r = h * 0.27;
        let sun_c = pos2(rect.center().x + rect.width() * 0.0, horizon_y - sun_r * (0.30 + 0.45 * sc.sun) + (t * 0.35).sin() * 1.5);
        glow(p, sun_c, sun_r * 2.6, rgba(0xF7, 0x93, 0x1A, 0.34));
        glow_ellipse(p, pos2(sun_c.x, horizon_y), rect.width() * 0.55, h * 0.16, rgba(0xFF, 0x8A, 0x3A, 0.30));
        let mut m = Mesh::default();
        let uv = egui::epaint::WHITE_UV;
        let rows = 24;
        for i in 0..=rows {
            let k = i as f32 / rows as f32;
            let y = sun_c.y - sun_r + 2.0 * sun_r * k;
            let hw = (sun_r * sun_r - (y - sun_c.y).powi(2)).max(0.0).sqrt();
            let col = if k < 0.5 { theme::mix(Color32::from_rgb(0xFF, 0xD0, 0x5A), theme::ACCENT, k * 2.0) } else { theme::mix(theme::ACCENT, Color32::from_rgb(0xE0, 0x3E, 0x3A), (k - 0.5) * 2.0) };
            let i0 = m.vertices.len() as u32;
            m.vertices.push(Vertex { pos: pos2(sun_c.x - hw, y), uv, color: col });
            m.vertices.push(Vertex { pos: pos2(sun_c.x + hw, y), uv, color: col });
            if i > 0 {
                m.indices.extend([i0 - 2, i0 - 1, i0, i0 - 1, i0 + 1, i0]);
            }
        }
        p.add(Shape::mesh(m));
        // stripes eat into the lower half of the disc, thicker towards the bottom
        for i in 0..5 {
            let k = 0.52 + i as f32 * 0.10;
            let thick = sun_r * (0.018 + 0.017 * i as f32);
            let y = sun_c.y - sun_r + 2.0 * sun_r * k;
            let hw = (sun_r * sun_r - (y - sun_c.y).powi(2)).max(0.0).sqrt() + 1.0;
            let sky = sky_at(&stops, (y - rect.top()) / (horizon_y - rect.top()));
            p.rect_filled(Rect::from_min_max(pos2(sun_c.x - hw, y), pos2(sun_c.x + hw, y + thick)), 0.0, sky);
        }

        // mesas and dunes (far to near), slowly scrolling
        let far = theme::mix(SKY_ROSE, SKY_PLUM, 0.55);
        let far = theme::mix(far, theme::BG, 0.30 + sc.night * 0.3);
        ridge(p, rect, horizon_y + 2.0, h * 0.13, 620.0, t * 2.0, 0.6, far, true);
        ridge(p, rect, horizon_y + 5.0, h * 0.09, 380.0, t * 4.5, 2.4, theme::mix(Color32::from_rgb(0x55, 0x1C, 0x5A), theme::BG, 0.35), false);
    }

    // ground
    let ground = Rect::from_min_max(pos2(rect.left(), horizon_y), rect.max);
    vgradient(
        p,
        ground,
        &[(0.0, theme::mix(Color32::from_rgb(0x8A, 0x2C, 0x2A), theme::BG, 0.25 + sc.night * 0.3)), (0.35, theme::mix(Color32::from_rgb(0x4A, 0x16, 0x34), theme::BG, 0.2)), (1.0, Color32::from_rgb(0x12, 0x08, 0x1C))],
    );
    // receding speed lines on the ground
    for i in 0..9 {
        let k = ((i as f32 + (t * 0.55) % 1.0) / 9.0).powf(1.9);
        let y = horizon_y + ground.height() * k;
        let a = 0.05 + 0.20 * k;
        p.hline(rect.x_range(), y, Stroke::new(0.5 + 1.4 * k, rgba(0xF7, 0xB0, 0x60, a * (1.0 - sc.veil * 0.5))));
    }

    // dust drifting across the lower half
    let dust = b.dust.id();
    for i in 0..22u32 {
        let sp = 8.0 + hash01(i * 13) * 38.0;
        let size = 50.0 + hash01(i * 17) * 150.0;
        let span = rect.width() + size * 2.0;
        let x = rect.left() - size + (hash01(i * 19) * span + t * sp) % span;
        let y = horizon_y + (hash01(i * 23) - 0.15) * (rect.bottom() - horizon_y) * 0.95;
        let a = 0.05 + 0.10 * hash01(i * 29);
        let tint = theme::mix(theme::SAND, theme::ACCENT, hash01(i * 31) * 0.5);
        sprite(p, dust, pos2(x, y), vec2(size * 1.5, size), (hash01(i * 37) - 0.5) * 0.8 + t * 0.02, hash01(i * 41) > 0.5, fade(tint, a));
    }

    // veil (quiet screens) and vignette
    if sc.veil > 0.0 {
        p.rect_filled(rect, 0.0, rgba(0x0C, 0x05, 0x16, sc.veil));
    }
    vignette(p, rect, 0.55);
}

/// Darkened edges.
pub fn vignette(p: &Painter, rect: Rect, strength: f32) {
    let d = Color32::from_rgba_unmultiplied(8, 3, 16, (strength * 255.0) as u8);
    let w = rect.width() * 0.22;
    let hh = rect.height() * 0.30;
    hgradient(p, Rect::from_min_size(rect.min, vec2(w, rect.height())), d, Color32::TRANSPARENT);
    hgradient(p, Rect::from_min_max(pos2(rect.right() - w, rect.top()), rect.max), Color32::TRANSPARENT, d);
    vgradient(p, Rect::from_min_size(rect.min, vec2(rect.width(), hh)), &[(0.0, d), (1.0, Color32::TRANSPARENT)]);
    vgradient(p, Rect::from_min_max(pos2(rect.left(), rect.bottom() - hh), rect.max), &[(0.0, Color32::TRANSPARENT), (1.0, d)]);
}

/// Over a live 3D backdrop: only a light vignette and a bottom shade so the
/// controls stay readable.
pub fn backdrop_over_3d(p: &Painter, rect: Rect, left_shade: f32) {
    vignette(p, rect, 0.45);
    if left_shade > 0.0 {
        hgradient(p, Rect::from_min_size(rect.min, vec2(rect.width() * 0.6, rect.height())), rgba(0x0C, 0x05, 0x16, left_shade), Color32::TRANSPARENT);
    }
}

/// The running-ostrich silhouette, `x` across the rect (0..1 of the rect width,
/// can be outside), feet on `ground_y`, `height` tall; with a bounce and a dust trail.
pub fn runner(p: &Painter, b: &Brand, x: f32, ground_y: f32, height: f32, t: f32, color: Color32, trail: f32) {
    let w = height * 1024.0 / 700.0;
    let bob = (t * 15.0).sin() * height * 0.012;
    let tilt = (t * 15.0 + 1.0).sin() * 0.012;
    let c = pos2(x, ground_y - height * 0.5 - bob.abs());
    // dust behind the feet
    for i in 0..9u32 {
        let k = i as f32 / 9.0;
        let back = (k * 1.4 + (t * 3.0 + hash01(i * 7)) % 1.0 * 0.5) * height * 1.1;
        let sz = height * (0.22 + 0.34 * k);
        let a = (0.30 - 0.27 * k) * trail;
        sprite(p, b.dust.id(), pos2(x - w * 0.30 - back, ground_y - sz * 0.30 - k * height * 0.05), vec2(sz * 1.5, sz), t + i as f32, i % 2 == 0, fade(theme::mix(theme::SAND, theme::ACCENT, 0.3), a));
    }
    sprite(p, b.run.id(), c, vec2(w, height), tilt, false, color);
}

/// Laser-eye beams from `from` going right, length `len`, with a hot core.
pub fn laser_beam(p: &Painter, from: Pos2, dir: egui::Vec2, len: f32, width: f32, alpha: f32) {
    let d = dir.normalized();
    let n = vec2(-d.y, d.x);
    let end = from + d * len;
    for (w, a, col) in [(width * 4.0, 0.16, theme::LASER), (width * 2.2, 0.35, theme::LASER), (width, 0.95, Color32::from_rgb(255, 220, 215))] {
        let mut m = Mesh::default();
        let uv = egui::epaint::WHITE_UV;
        let c0 = fade(col, a * alpha);
        // tapered: thick at the eye, thin at the end
        push_quad(&mut m, [from - n * w * 0.5, end - n * w * 0.12, end + n * w * 0.12, from + n * w * 0.5], [pos2(uv.x, uv.y); 4], [c0, fade(col, 0.0), fade(col, 0.0), c0]);
        p.add(Shape::mesh(m));
    }
    glow(p, from, width * 5.0, fade(theme::LASER, 0.7 * alpha));
}

/// A big shaded trophy cup centred on `c`, `s` points across. `base` is the metal colour.
pub fn trophy(p: &Painter, c: Pos2, s: f32, base: Color32) {
    let u = |x: f32, y: f32| c + vec2(x, y) * s;
    let dark = theme::mix(base, Color32::from_rgb(0x5A, 0x24, 0x08), 0.45);
    let mid = theme::mix(base, Color32::from_rgb(0x5A, 0x24, 0x08), 0.20);
    let light = theme::mix(base, Color32::WHITE, 0.55);
    let outline = theme::OUTLINE;
    let w = s * 0.075;
    // handles first, behind the cup
    for side in [-1.0_f32, 1.0] {
        let (a0, a1) = if side < 0.0 { (std::f32::consts::FRAC_PI_2, std::f32::consts::PI * 1.5) } else { (-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2) };
        let pts: Vec<Pos2> = (0..=14).map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / 14.0;
            u(side * 0.31, -0.22) + vec2(a.cos() * 0.15 * s, a.sin() * 0.16 * s)
        }).collect();
        p.add(Shape::line(pts.clone(), Stroke::new(w + 4.0, outline)));
        p.add(Shape::line(pts, Stroke::new(w, mid)));
    }
    let cup = [(0.30, -0.40), (0.27, -0.12), (0.17, 0.06), (0.07, 0.12), (-0.07, 0.12), (-0.17, 0.06), (-0.27, -0.12), (-0.30, -0.40)];
    let pts: Vec<Pos2> = cup.iter().map(|&(x, y)| u(x, y)).collect();
    p.add(Shape::closed_line(pts.clone(), Stroke::new(5.0_f32, outline)));
    crate::ui::icons::fan(p, u(0.0, -0.14), &pts, base);
    let shade: Vec<Pos2> = [(0.12, -0.40), (0.30, -0.40), (0.27, -0.12), (0.17, 0.06), (0.07, 0.12), (0.11, -0.10)].iter().map(|&(x, y)| u(x, y)).collect();
    crate::ui::icons::fan(p, u(0.2, -0.16), &shade, dark);
    let hi: Vec<Pos2> = [(-0.23, -0.36), (-0.14, -0.36), (-0.17, -0.14), (-0.12, 0.0), (-0.19, -0.08)].iter().map(|&(x, y)| u(x, y)).collect();
    crate::ui::icons::fan(p, u(-0.17, -0.2), &hi, light);
    crate::ui::icons::star(p, u(0.0, -0.20), 0.12 * s, dark, Stroke::new(1.5_f32, outline));
    // rim, stem and base
    p.rect_filled(Rect::from_center_size(u(0.0, -0.40), vec2(0.62 * s, 0.05 * s)), s * 0.02, light);
    for (rect, col) in [
        (Rect::from_center_size(u(0.0, 0.19), vec2(0.12 * s, 0.14 * s)), mid),
        (Rect::from_center_size(u(0.0, 0.30), vec2(0.46 * s, 0.07 * s)), base),
        (Rect::from_center_size(u(0.0, 0.375), vec2(0.62 * s, 0.09 * s)), dark),
    ] {
        p.rect_stroke(rect.expand(1.5), egui::CornerRadius::same(3), Stroke::new(2.5_f32, outline), egui::StrokeKind::Outside);
        p.rect_filled(rect, 3.0, col);
    }
}
