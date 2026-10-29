//! Ostrich accessories and jockey attire, bought with banked race Bitcoin.

use super::{common, Ctx, Screen, Transition};
use crate::render::ScenePass;
use crate::ui::{feedback, progress, theme, widgets};
use egui::{pos2, vec2, Align2, Rect};

pub struct WardrobeScreen {
    t: f32,
    yaw: f32,
    stage: common::StagePaint,
}

impl WardrobeScreen {
    pub fn new() -> Self {
        Self {
            t: 0.0,
            yaw: 0.5,
            stage: Default::default(),
        }
    }
}

impl Screen for WardrobeScreen {
    fn name(&self) -> &'static str {
        "wardrobe"
    }
    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        self.yaw += ctx.dt * 0.3;
        let c = ctx.profile.character;
        let attire = progress::attire(ctx.profile, c);
        crate::game::stage::show_garage_attire(c, attire, self.yaw);
        crate::game::stage::set_focus(if common::Layout::new(ctx.viewport.safe).portrait { 0.5 } else { 0.22 });
        if common::Layout::new(ctx.viewport.safe).portrait { crate::game::stage::set_portrait_focus(0.3, 2.1); }
        Transition::None
    }
    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        Transition::Pop
    }
    fn paint_scene(&mut self, pass: &mut ScenePass) {
        self.stage.paint(pass);
    }
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let mut out = Transition::None;
        common::page(ctx.viewport.screen, ctx.time as f32, ui, 0.35, |ui, _| {
            let hd = widgets::header(ui, ctx.viewport.safe, "Wardrobe", widgets::enter(self.t, 0));
            if hd.back.clicked() {
                out = self.back(ctx);
            }
            let mut body = hd.body;
            let portrait = common::Layout::new(ctx.viewport.safe).portrait;
            if portrait { body.min.y += body.height() * 0.46; }
            let p = ui.painter().clone();
            let character = ctx.profile.character;
            let left = body.left() + if portrait { 0.0 } else { body.width() * 0.43 };
            let width = (body.right() - left - 12.0) * 0.5;
            let y = body.top() + 49.0;
            let height = ((body.height() - 83.0) / 4.0).clamp(36.0, 52.0);
            p.text(
                pos2(left, body.top() + 10.0),
                Align2::LEFT_CENTER,
                format!(
                    "Banked Bitcoin: {}",
                    progress::grouped(ctx.profile.progress.sats)
                ),
                theme::ui_bold(15.0),
                theme::GOLD,
            );
            for slot in 0..2 {
                let x = left + slot as f32 * (width + 12.0);
                p.text(
                    pos2(x, y - 20.0),
                    Align2::LEFT_CENTER,
                    if slot == 0 { "OSTRICH" } else { "JOCKEY" },
                    theme::ui_bold(12.0),
                    theme::TEXT,
                );
                for id in 0..4u8 {
                    let r = Rect::from_min_size(
                        pos2(x, y + id as f32 * height),
                        vec2(width, height - 5.0),
                    );
                    let owned = progress::owns_attire(ctx.profile, slot, id);
                    let worn = progress::attire(ctx.profile, character)[slot + 1] == id;
                    let name = progress::ATTIRE_NAMES[slot][id as usize];
                    let label = if worn {
                        format!("{name} — wearing")
                    } else if owned {
                        format!("Wear {name}")
                    } else {
                        format!("Buy {name} · {}", progress::ATTIRE_PRICES[id as usize])
                    };
                    let btn = widgets::Btn::new(&label).size(12.0).enabled(!worn);
                    if widgets::button(ui, r, &format!("wardrobe-{slot}-{id}"), &btn).clicked() {
                        match progress::wear_attire(ctx.profile, character, slot, id) {
                            Ok(_) => {
                                feedback::tap(ctx);
                                ctx.save_profile();
                            }
                            Err(progress::BuyError::Short(n)) => ctx.say(format!(
                                "Earn {} more Bitcoin in races",
                                progress::grouped(n)
                            )),
                            Err(_) => {}
                        }
                    }
                }
            }
            p.text(
                pos2(left, body.bottom() - 8.0),
                Align2::LEFT_BOTTOM,
                if portrait { "Cosmetic only. Racing stays the same." } else { "Purely cosmetic. Same speed, turning and power-ups." },
                theme::ui_medium(11.0),
                theme::TEXT_DIM,
            );
        });
        out
    }
}
