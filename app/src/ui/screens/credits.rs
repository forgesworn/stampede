//! Credits: decented, the fonts' licences (SIL OFL 1.1), made with Rust.

use super::common::{self, wordmark};
use super::{Ctx, Screen, Transition};
use crate::ui::art::{self, ease_out_cubic, fade, phase, rgba};
use crate::ui::widgets::{self, S3, S4};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Rect};

pub struct CreditsScreen {
    t: f32,
}

impl Default for CreditsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl CreditsScreen {
    pub fn new() -> CreditsScreen {
        CreditsScreen { t: 0.0 }
    }
}

impl Screen for CreditsScreen {
    fn name(&self) -> &'static str {
        "credits"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        Transition::Pop
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        common::page(ctx.viewport.screen, tm, ui, 0.30, |ui, brand| {
            let hd = widgets::header(ui, safe, "Credits", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let body = hd.body;
            let e = ease_out_cubic(phase(t, 0.12, 0.45));
            let portrait = common::Layout::new(safe).portrait;
            let panel_h = body.height().min(if portrait { 560.0 } else { 262.0 });
            let card = Rect::from_center_size(pos2(body.center().x, body.center().y + (1.0 - e) * 20.0 - 4.0), vec2(body.width().min(640.0), panel_h));
            widgets::staged(ui, e, vec2(0.0, 0.0), |ui, _| {
                let p = ui.painter().clone();
                widgets::panel(&p, card, 24.0);
                // left: the maker
                let lx = if portrait { card.center().x } else { card.left() + 150.0 };
                wordmark(&p, pos2(lx, card.top() + 54.0), 40.0, 1.0);
                p.text(pos2(lx, card.top() + 100.0), Align2::CENTER_CENTER, "a game by", theme::ui_medium(12.0), theme::TEXT_DIM);
                widgets::heading(&p, pos2(lx, card.top() + 126.0), Align2::CENTER_CENTER, "decented", 30.0, theme::SAND);
                art::coin(&p, brand, pos2(lx, card.top() + 176.0), 30.0, Some(tm * 0.5), 1.0);
                p.text(pos2(lx, card.top() + if portrait { 230.0 } else { card.height() - 26.0 }), Align2::CENTER_CENTER, crate::version_label(), theme::ui_medium(11.0), fade(theme::TEXT_DIM, 0.9));
                if !portrait { p.vline(card.left() + 300.0, (card.top() + 24.0)..=(card.bottom() - 24.0), egui::Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10))); }
                // right: the small print
                let rx = card.left() + if portrait { 24.0 } else { 324.0 };
                let rw = card.right() - S4 - rx;
                let mut y = card.top() + if portrait { 280.0 } else { 36.0 };
                let block = |head: &str, lines: &[&str], y: &mut f32| {
                    p.text(pos2(rx, *y), Align2::LEFT_CENTER, head, theme::title(16.0), theme::ACCENT);
                    *y += 19.0;
                    for l in lines {
                        let g = p.layout(l.to_string(), theme::ui_medium(11.8), theme::TEXT, rw);
                        let h = g.size().y;
                        p.galley(pos2(rx, *y - 1.0), g, theme::TEXT);
                        *y += h + 2.0;
                    }
                    *y += S3 - 2.0;
                };
                block("Made with Rust", &["Native, no game engine: wgpu, winit, egui and rodio."], &mut y);
                block(
                    "Fonts",
                    &["Lilita One, copyright 2011 Juan Montoreano.", "Exo 2, copyright 2013 the Exo 2 Project Authors.", "Both under the SIL Open Font License 1.1."],
                    &mut y,
                );
                block("Sound", &["Every sound and both music loops are synthesised in the game, from scratch."], &mut y);
            });
        });
        out
    }
}
