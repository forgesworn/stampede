//! egui look (SPEC 13): sunset desert. Deep indigo to plum to burnt orange,
//! bitcoin orange `#F7931A` as the accent, sand `#E8C68A`, off-white text.
//! Bundled OFL fonts: Lilita One (chunky display) for titles and numbers, Exo 2
//! for the UI. Every title goes through [`title`], every number through [`numbers`].

use egui::epaint::text::VariationCoords;
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, FontTweak};
use std::sync::Arc;

static LILITA: &[u8] = include_bytes!("../../assets/fonts/lilita-one/LilitaOne-Regular.ttf");
static EXO2: &[u8] = include_bytes!("../../assets/fonts/exo2/Exo2-VariableFont_wght.ttf");

/// Font family names.
pub const LILITA_ONE: &str = "lilita-one";
/// Legacy names (the display font used to be Orbitron); both are Lilita One now.
pub const ORBITRON_BOLD: &str = "orbitron-bold";
pub const ORBITRON_BLACK: &str = "orbitron-black";
pub const EXO_BOLD: &str = "exo-bold";
pub const EXO_MEDIUM: &str = "exo-medium";
pub const EXO_HEAVY: &str = "exo-heavy";

// Palette (sRGB).
/// The window background: also the Android window colour, so no white flash.
pub const BG: Color32 = Color32::from_rgb(0x12, 0x0A, 0x1F);
pub const INDIGO: Color32 = Color32::from_rgb(0x2B, 0x14, 0x5E);
pub const PLUM: Color32 = Color32::from_rgb(0x6A, 0x1F, 0x6B);
pub const BURNT: Color32 = Color32::from_rgb(0xC8, 0x4B, 0x1A);
pub const ACCENT: Color32 = Color32::from_rgb(0xF7, 0x93, 0x1A);
pub const SAND: Color32 = Color32::from_rgb(0xE8, 0xC6, 0x8A);
pub const TEXT: Color32 = Color32::from_rgb(0xFB, 0xF3, 0xE6);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xB8, 0xA6, 0xC9);
pub const LASER: Color32 = Color32::from_rgb(0xFF, 0x2D, 0x3F);
/// Dark plum used for outlines around chunky type and buttons.
pub const OUTLINE: Color32 = Color32::from_rgb(0x2C, 0x0D, 0x44);
/// Deep plum for panel bodies.
pub const DEEP: Color32 = Color32::from_rgb(0x1B, 0x0B, 0x33);
pub const GOLD: Color32 = Color32::from_rgb(0xFF, 0xC8, 0x3D);
pub const MINT: Color32 = Color32::from_rgb(0x6B, 0xE0, 0xA8);
pub const PANEL: Color32 = Color32::from_rgba_premultiplied(24, 12, 44, 220);

/// A colour as the 0..1 sRGB floats `Screen::clear_color` returns.
pub fn rgb(c: Color32) -> [f32; 3] {
    [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0]
}

/// The window background as a clear colour.
pub fn bg_clear() -> [f32; 4] {
    let [r, g, b] = rgb(BG);
    [r, g, b, 1.0]
}

/// Linear blend of two colours (`t` 0..1).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// Blend two colours including alpha (on the premultiplied channels, so it is a true blend).
pub fn mix_rgba(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()), f(a.a(), b.a()))
}

fn weighted(bytes: &'static [u8], wght: f32) -> Arc<FontData> {
    Arc::new(FontData::from_static(bytes).tweak(FontTweak { coords: VariationCoords::new([(b"wght", wght)]), ..FontTweak::default() }))
}

/// Font definitions with the bundled fonts.
pub fn fonts() -> FontDefinitions {
    let mut f = FontDefinitions::default();
    f.font_data.insert("lilita".into(), Arc::new(FontData::from_static(LILITA)));
    f.font_data.insert("exo-500".into(), weighted(EXO2, 500.0));
    f.font_data.insert("exo-700".into(), weighted(EXO2, 700.0));
    f.font_data.insert("exo-600".into(), weighted(EXO2, 600.0));
    f.font_data.insert("exo-800".into(), weighted(EXO2, 800.0));
    let fallback: Vec<String> = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let fam = |first: &str| -> Vec<String> {
        let mut v = vec![first.to_string()];
        v.extend(fallback.iter().cloned());
        v
    };
    f.families.insert(FontFamily::Proportional, fam("exo-500"));
    f.families.insert(FontFamily::Name(LILITA_ONE.into()), fam("lilita"));
    f.families.insert(FontFamily::Name(ORBITRON_BOLD.into()), fam("lilita"));
    f.families.insert(FontFamily::Name(ORBITRON_BLACK.into()), fam("lilita"));
    f.families.insert(FontFamily::Name(EXO_BOLD.into()), fam("exo-700"));
    f.families.insert(FontFamily::Name(EXO_MEDIUM.into()), fam("exo-600"));
    f.families.insert(FontFamily::Name(EXO_HEAVY.into()), fam("exo-800"));
    f
}

/// Chunky display type for titles.
pub fn title(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(LILITA_ONE.into()))
}

/// Numbers (timer, place, speed): the display face too.
pub fn numbers(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(LILITA_ONE.into()))
}

pub fn ui_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(EXO_BOLD.into()))
}

pub fn ui_medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(EXO_MEDIUM.into()))
}

pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Install fonts and style on a context.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    let mut style = egui::Style { visuals: egui::Visuals::dark(), ..Default::default() };
    style.visuals.panel_fill = Color32::TRANSPARENT;
    style.visuals.window_fill = PANEL;
    style.visuals.override_text_color = Some(TEXT);
    style.spacing.interact_size.y = 40.0;
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.interaction.selectable_labels = false;
    ctx.set_global_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_load_and_lay_out_text() {
        let ctx = egui::Context::default();
        install(&ctx);
        ctx.begin_pass(egui::RawInput::default());
        let g = ctx.fonts_mut(|f| f.layout_no_wrap("STAMPEDE".into(), title(40.0), TEXT));
        let width = g.size().x;
        let _ = ctx.end_pass();
        assert!(width > 100.0, "title laid out at {width}");
    }

    #[test]
    fn window_background_matches_the_android_theme() {
        assert_eq!((BG.r(), BG.g(), BG.b()), (0x12, 0x0A, 0x1F));
    }
}
