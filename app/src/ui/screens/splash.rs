//! Splash (~2.9 s, tap to skip): black, a dust storm sweeps across, the running
//! ostrich silhouette sprints through, the STAMPEDE wordmark slams in (scale
//! overshoot, screen shake, shock ring) and the laser-eyed head fires across it,
//! then "decented" and a fade to the title.

use super::common::{self, wordmark};
use super::{home::HomeScreen, tracks, Ctx, Screen, Transition};
use crate::ui::art::{self, ease_out_back, ease_out_cubic, fade, hash01, phase, rgba, Scene};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect, Stroke};

pub const SPLASH_SECS: f32 = 3.0;

/// When the wordmark lands.
const SLAM: f32 = 1.12;
/// When the head and its lasers come in.
const LASER: f32 = 1.55;

pub struct SplashScreen {
    t: f32,
    started: bool,
    warmed: bool,
}

impl Default for SplashScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl SplashScreen {
    pub fn new() -> SplashScreen {
        SplashScreen { t: 0.0, started: false, warmed: false }
    }
}

impl Screen for SplashScreen {
    fn name(&self) -> &'static str {
        "splash"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !self.started {
            self.started = true;
            crate::game::stage::warm_up();
            tracks::warm_in_background();
        }
        let skip = !ctx.touches.began().is_empty() || ctx.keys.any_held(&[crate::platform::KeyCode::Space, crate::platform::KeyCode::Enter]);
        if self.t >= SPLASH_SECS || (skip && self.t > 0.3) {
            return Transition::Replace(Box::new(HomeScreen::new()));
        }
        Transition::None
    }

    fn paint_scene(&mut self, pass: &mut crate::render::ScenePass) {
        // Create the shared renderer (pipeline compile, 0.2-0.6 s on a phone) and load the title's
        // attract world while the splash plays, not at the first race's countdown.
        if self.t > 0.45 && !self.warmed {
            self.warmed = true;
            crate::game::stage::warm_renderer(pass, Some(common::menu_track()));
        }
    }

    /// Back skips to the title (it must not quit the app).
    fn back(&mut self, _ctx: &mut Ctx) -> Transition {
        Transition::Replace(Box::new(HomeScreen::new()))
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let brand = art::brand(ui);
        let screen = ctx.viewport.screen;
        let safe = ctx.viewport.safe;
        let t = self.t;
        let p = ui.layer_painter(egui::LayerId::background());

        // shake, from the slam
        let k = t - SLAM;
        let shake = if (0.0..0.5).contains(&k) { vec2((k * 95.0).sin(), (k * 71.0).cos()) * 9.0 * (-k * 9.0).exp() } else { vec2(0.0, 0.0) };
        let area = screen.expand(14.0).translate(shake);

        // the sun climbs while it plays
        let rise = ease_out_cubic(phase(t, 0.0, 2.2));
        art::backdrop(&p, &brand, area, Scene { t: ctx.time as f32, sun: -0.65 + 0.65 * rise, veil: 0.0, night: 0.0, horizon: true });

        let h = screen.height();
        let w = screen.width();
        let ground_y = screen.top() + h * 0.84;

        // the laser head, bottom left, rising at LASER
        let head_in = ease_out_back(phase(t, LASER - 0.15, 0.40));
        let portrait = common::Layout::new(safe).portrait;
        let head_size = if portrait { safe.width() * 1.05 } else { h * 0.86 };
        let head_x = if portrait { safe.center().x } else { safe.left() + head_size * 0.40 };
        let head_rect = Rect::from_min_size(pos2(head_x - head_size / 2.0, screen.bottom() - head_size * (0.10 + 0.9 * head_in) + shake.y), vec2(head_size, head_size));
        if head_in > 0.0 {
            let mut hp = p.clone();
            hp.set_opacity(phase(t, LASER - 0.15, 0.18));
            art::sprite(&hp, brand.head.id(), head_rect.center() + shake, head_rect.size(), 0.0, false, Color32::WHITE);
        }

        // dust storm: big soft clouds and streaks sweeping left to right
        let storm = ((t - 0.10) / 1.55).clamp(0.0, 1.0);
        let env = (storm * std::f32::consts::PI).sin().powf(0.7);
        if env > 0.0 {
            for i in 0..30u32 {
                let sz = h * (0.35 + 0.8 * hash01(i * 7));
                let sp = w * (0.9 + 1.3 * hash01(i * 11));
                let start = -sz - hash01(i * 13) * w * 0.5;
                let x = start + (t - 0.1) * sp;
                let y = screen.top() + h * (0.30 + 0.75 * hash01(i * 17));
                let tint = theme::mix(theme::SAND, theme::BURNT, hash01(i * 19) * 0.7);
                art::sprite(&p, brand.dust.id(), pos2(x, y) + shake, vec2(sz * 1.7, sz), hash01(i * 23) * 3.0, hash01(i * 29) > 0.5, fade(tint, 0.30 * env));
            }
        }
        // the sprinting silhouette
        let run_k = phase(t, 0.42, 1.25);
        if run_k > 0.0 && run_k < 1.0 {
            let x = art::lerp(-0.22 * w, 1.28 * w, run_k) + shake.x;
            art::runner(&p, &brand, x, ground_y + shake.y, h * 0.52, t, Color32::from_rgb(0x12, 0x05, 0x24), 1.0);
        }

        if env > 0.0 {
            for i in 0..26u32 {
                let y = screen.top() + h * hash01(i * 31);
                let len = w * (0.12 + 0.30 * hash01(i * 37));
                let x = -len + ((t - 0.1) * w * (2.2 + 1.8 * hash01(i * 41)) + hash01(i * 43) * w) % (w + len * 2.0);
                p.line_segment([pos2(x, y), pos2(x + len, y)], Stroke::new(1.0 + 1.6 * hash01(i * 47), rgba(255, 220, 170, 0.26 * env)));
            }
        }

        // the wordmark slams in
        let size = (safe.width() * 0.64 / 4.45).min(safe.height() * 0.33);
        let centre = pos2(safe.center().x, safe.top() + safe.height() * 0.40) + shake;
        if t >= SLAM {
            let e = ease_out_back(phase(t, SLAM, 0.30));
            let sc = art::lerp(2.8, 1.0, e);
            let a = phase(t, SLAM, 0.08);
            let rect = wordmark(&p, centre, size * sc, a);
            // shock ring and impact flash
            if k < 0.6 {
                let rk = k / 0.6;
                p.circle_stroke(centre, size * (0.5 + 3.0 * ease_out_cubic(rk)), Stroke::new(5.0 * (1.0 - rk) + 0.5, rgba(255, 214, 140, 0.55 * (1.0 - rk))));
                p.rect_filled(screen, 0.0, rgba(255, 236, 200, 0.5 * (1.0 - phase(k, 0.0, 0.16))));
            }
            // dust kicked out under the word
            if k < 1.0 {
                for i in 0..10u32 {
                    let dir = if i % 2 == 0 { -1.0 } else { 1.0 };
                    let spread = (0.15 + 0.85 * hash01(i * 3)) * size * 3.0 * ease_out_cubic(phase(k, 0.0, 0.8));
                    let q = pos2(centre.x + dir * spread, rect.bottom() + size * 0.05 - hash01(i * 5) * size * 0.2);
                    let s = size * (0.45 + 0.4 * hash01(i * 7));
                    art::sprite(&p, brand.dust.id(), q, vec2(s * 1.5, s), hash01(i * 9) * 3.0, i % 3 == 0, fade(theme::SAND, 0.5 * (1.0 - phase(k, 0.1, 0.9))));
                }
            }
            // the head's lasers
            let eye = head_rect.min + vec2(head_rect.width() * 0.49, head_rect.height() * 0.385) + shake;
            let lk = phase(t, LASER, 0.9);
            if lk > 0.0 && lk < 1.0 {
                let pulse = (lk * std::f32::consts::PI).sin().powf(0.6);
                let target = pos2(rect.left() + rect.width() * (0.18 + 0.6 * ease_out_cubic(lk)), rect.center().y + rect.height() * 0.1);
                let v = target - eye;
                art::laser_beam(&p, eye, v, v.length(), 5.0, pulse);
                art::glow(&p, target, size * 0.9, rgba(255, 120, 90, 0.5 * pulse));
                art::glow(&p, target, size * 0.35, rgba(255, 240, 220, 0.8 * pulse));
                // a glint sweeps the letters where the beam lands
                let band = Rect::from_min_max(pos2(target.x - size * 0.16, rect.top() - 6.0), pos2(target.x + size * 0.16, rect.bottom() + 6.0));
                art::gradient_text(&p.with_clip_rect(band), centre, "STAMPEDE", theme::title(size), Color32::from_rgb(0xFF, 0xF4, 0xD8), Color32::from_rgb(0xFF, 0xB0, 0x5A), Color32::TRANSPARENT, 0.0, 0.55 * pulse);
            }
            // tagline
            let tk = phase(t, 1.75, 0.35);
            if tk > 0.0 {
                p.text(pos2(centre.x, rect.bottom() + size * 0.34 + (1.0 - ease_out_cubic(tk)) * 8.0), Align2::CENTER_CENTER, "laser-eyed ostrich racing", theme::ui_bold((size * 0.20).clamp(12.0, 18.0)), fade(theme::SAND, tk));
            }
        }

        // credit
        let ck = phase(t, 2.0, 0.45);
        if ck > 0.0 {
            p.text(pos2(safe.center().x, safe.bottom() - 18.0), Align2::CENTER_CENTER, "a game by decented", theme::ui_bold(13.0), fade(theme::TEXT_DIM, ck * 0.9));
        }

        // up from black at the start, and out at the end
        let black = (1.0 - phase(t, 0.0, 0.45)).max(phase(t, SPLASH_SECS - 0.25, 0.25));
        if black > 0.0 {
            p.rect_filled(screen, 0.0, theme::BG.gamma_multiply(black));
        }
        Transition::None
    }
}
