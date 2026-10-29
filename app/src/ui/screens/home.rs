//! Title: the wordmark over the attract race (or the painted sunset), the
//! laser-eyed head, "tap to start". The first time (`!profile.seen_intro`) a tap
//! opens a friendly nudge to try Training before the main menu.

use super::common::{self, wordmark, StagePaint};
use super::menu::MenuScreen;
use super::{Ctx, Screen, Transition};
use crate::render::ScenePass;
use crate::ui::art::{self, ease_out_back, ease_out_cubic, fade, phase, rgba};
use crate::ui::widgets::{self, Btn, S2, S3, S4};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect, Stroke};
use race_core::TrackId;

pub struct HomeScreen {
    t: f32,
    /// The first-run nudge is showing (seconds since it opened).
    nudge: Option<f32>,
    /// Which attract track this visit shows.
    track: TrackId,
    stage: StagePaint,
    /// A join code that arrived by link, until the lobby takes it.
    join: Option<crate::keeps::JoinTarget>,
    /// Where the "Update to vX" pill was drawn last frame (a tap on it opens Settings, not the menu).
    pill_rect: Option<Rect>,
    /// Screenshots: show an update on offer.
    demo_update: bool,
}

impl Default for HomeScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl HomeScreen {
    pub fn new() -> HomeScreen {
        HomeScreen { t: 0.0, nudge: None, track: common::menu_track(), stage: StagePaint::default(), join: None, pill_rect: None, demo_update: false }
    }

    /// The Title with an update on offer (screenshots).
    pub fn update_sample() -> HomeScreen {
        HomeScreen { demo_update: true, ..HomeScreen::new() }
    }

    /// The Title with the one-time Training nudge already open (screenshots).
    pub fn first_run_sample() -> HomeScreen {
        let mut h = HomeScreen::new();
        h.nudge = Some(1.0);
        h
    }
}

impl Screen for HomeScreen {
    fn name(&self) -> &'static str {
        "home"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        crate::game::stage::show_attract(self.track);
        if std::mem::take(&mut self.demo_update) && ctx.opts.shot.is_some() {
            ctx.updates.demo_offer("0.3.0", "");
        }
        if let Some(target) = ctx.take_join_code() {
            log::info!("home: join link for {}", crate::keeps::describe(&target));
            self.join = Some(target.clone());
            // A link goes straight to joining that room.
            return Transition::Push(Box::new(super::multiplayer::MultiplayerScreen::joining(target)));
        }
        if let Some(n) = self.nudge.as_mut() {
            *n += ctx.dt;
            return Transition::None;
        }
        // A tap on the update pill opens Settings at the UPDATES card; a tap anywhere else starts.
        if let Some(r) = self.pill_rect.filter(|_| ctx.updates.pill_live()) {
            if ctx.touches.began().iter().any(|tc| r.expand(10.0).contains(tc.pos)) && self.t > 0.45 {
                feedback::select(ctx);
                return Transition::Push(Box::new(super::settings::SettingsScreen::at_cards()));
            }
        }
        let on_pill = self.pill_rect.is_some_and(|r| ctx.touches.began().iter().any(|tc| r.expand(10.0).contains(tc.pos)));
        let tapped = !ctx.touches.began().is_empty() && self.t > 0.45 && !on_pill;
        let key = ctx.keys.any_held(&[crate::platform::KeyCode::Space, crate::platform::KeyCode::Enter]) && self.t > 0.45;
        if tapped || key {
            feedback::select(ctx);
            if ctx.profile.seen_intro {
                return Transition::Push(Box::new(MenuScreen::new()));
            }
            self.nudge = Some(0.0);
        }
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        if self.nudge.take().is_some() {
            feedback::back(ctx);
            return Transition::None;
        }
        Transition::Pop
    }

    fn paint_scene(&mut self, pass: &mut ScenePass) {
        self.stage.paint(pass);
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let screen = ctx.viewport.screen;
        let t = self.t;
        let tm = ctx.time as f32;
        let over3d = crate::game::stage::is_drawing();
        let mut out = Transition::None;
        let nudge = self.nudge;
        let join = self.join.clone();
        let pill = ctx.updates.pill();
        let mut pill_rect = None;
        common::page(screen, tm, ui, 0.0, |ui, brand| {
            let p = ui.painter().clone();
            let h = safe.height();
            // the head, bottom left (continues the splash)
            let a_in = ease_out_cubic(phase(t, 0.05, 0.7));
            let portrait = common::Layout::new(safe).portrait;
            let head_size = if portrait { safe.width() * 1.05 } else { screen.height() * 0.86 };
            let head_c = pos2(if portrait { safe.center().x } else { safe.left() + head_size * 0.40 }, screen.bottom() - head_size * 0.5 + (1.0 - a_in) * 40.0 + (tm * 1.3).sin() * 2.0);
            art::sprite(&p, brand.head.id(), head_c, vec2(head_size, head_size), 0.0, false, fade(Color32::WHITE, a_in));

            // the sprinter crosses the horizon now and then
            if !over3d {
                let cycle = (tm % 11.0) / 11.0;
                if cycle < 0.5 {
                    let k = cycle / 0.5;
                    art::runner(&p, brand, art::lerp(-0.2, 1.25, k) * screen.width(), screen.top() + screen.height() * 0.80, screen.height() * 0.30, tm, Color32::from_rgb(0x1A, 0x08, 0x2E), 0.9);
                }
            }

            // wordmark: drops in, then floats
            let size = (safe.width() * 0.62 / 4.45).min(h * 0.31);
            let e = ease_out_back(phase(t, 0.10, 0.55));
            let bob = (tm * 1.6).sin() * 2.0;
            let centre = pos2(safe.center().x + safe.width() * 0.05, safe.top() + h * 0.37 + bob);
            let rect = wordmark(&p, centre - vec2(0.0, (1.0 - e) * 60.0), size * (0.8 + 0.2 * e), phase(t, 0.1, 0.25));
            let tk = phase(t, 0.55, 0.45);
            p.text(pos2(centre.x, rect.bottom() + size * 0.30), Align2::CENTER_CENTER, "laser-eyed ostrich racing", theme::ui_bold((size * 0.21).clamp(12.0, 19.0)), fade(theme::SAND, tk));

            // a laser pulse from the head to the wordmark every few seconds
            let eye = pos2(head_c.x - head_size * 0.5 + head_size * 0.49, head_c.y - head_size * 0.5 + head_size * 0.385);
            let lc = (tm % 6.0) / 6.0;
            if lc > 0.72 && lc < 0.86 && t > 0.8 && nudge.is_none() {
                let k = (lc - 0.72) / 0.14;
                let pulse = (k * std::f32::consts::PI).sin();
                let target = pos2(rect.left() + rect.width() * 0.22, rect.center().y);
                let v = target - eye;
                art::laser_beam(&p, eye, v, v.length(), 4.5, pulse);
                art::glow(&p, target, size * 0.7, rgba(255, 150, 100, 0.45 * pulse));
            }

            // tap to start
            let ps = phase(t, 0.9, 0.5);
            if ps > 0.0 && nudge.is_none() {
                let pulse = 0.5 + 0.5 * (tm * 3.2).sin();
                let c = pos2(safe.center().x + safe.width() * 0.05, safe.bottom() - 40.0);
                let label = "TAP TO START";
                let g = p.layout_no_wrap(label.to_string(), theme::title(21.0), Color32::WHITE);
                let pw = g.size().x + 44.0;
                let pr = Rect::from_center_size(c, vec2(pw * (1.0 + 0.03 * pulse), 40.0 * (1.0 + 0.03 * pulse)));
                let mut pp = p.clone();
                pp.set_opacity(ps);
                art::glow(&pp, c, pw * 0.8, rgba(0xF7, 0x93, 0x1A, 0.18 + 0.12 * pulse));
                widgets::soft_shadow(&pp, pr, 20.0, 4.0, 12.0, 0.5);
                widgets::rrect_gradient(&pp, pr, 20.0, rgba(0x2C, 0x0D, 0x44, 0.82), rgba(0x1B, 0x0B, 0x33, 0.9));
                pp.rect_stroke(pr, egui::CornerRadius::same(20), Stroke::new(2.0_f32, fade(theme::ACCENT, 0.55 + 0.45 * pulse)), egui::StrokeKind::Inside);
                art::outlined(&pp, c, Align2::CENTER_CENTER, label, theme::title(21.0), theme::TEXT, theme::OUTLINE, 1.6, 0.0);
            }

            // build and credit lines
            p.text(pos2(safe.right() - S4, safe.bottom() - S2), Align2::RIGHT_BOTTOM, format!("a game by decented  {}", crate::version_label()), theme::ui_medium(10.5), fade(theme::TEXT_DIM, 0.8));
            if let Some(label) = &pill {
                let live = ctx.updates.pill_live();
                let c = pos2(safe.right() - S4 - 80.0, safe.top() + 24.0);
                let pulse = if live { 0.5 + 0.5 * (tm * 3.0).sin() } else { 0.0 };
                art::glow(&p, c, 70.0, rgba(0xF7, 0x93, 0x1A, 0.10 + 0.10 * pulse));
                let r = widgets::pill(&p, c, label, if live { theme::ACCENT } else { rgba(0x2C, 0x0D, 0x44, 0.9) }, if live { Color32::from_rgb(0x2C, 0x0D, 0x44) } else { theme::TEXT }, 14.0);
                pill_rect = Some(r);
            }
            if let Some(target) = &join {
                let what = match target {
                    crate::keeps::JoinTarget::Code(code) => format!("join room {code}"),
                    _ => "join that Keep".to_string(),
                };
                widgets::pill(&p, pos2(safe.center().x, safe.top() + 22.0), &what, rgba(0x2C, 0x0D, 0x44, 0.9), theme::TEXT, 13.0);
            }

            // the one-time nudge
            if let Some(nt) = nudge {
                let k = ease_out_cubic(phase(nt, 0.0, 0.3));
                p.rect_filled(screen, 0.0, rgba(8, 3, 18, 0.62 * k));
                let (w, hh) = (safe.width().min(420.0), 168.0);
                let card = Rect::from_center_size(safe.center() + vec2(0.0, (1.0 - k) * 24.0), vec2(w, hh));
                widgets::staged(ui, k, vec2(0.0, 0.0), |ui, _| {
                    let p = ui.painter().clone();
                    widgets::panel_solid(&p, card, 22.0);
                    widgets::heading(&p, pos2(card.center().x, card.top() + 30.0), Align2::CENTER_CENTER, "New here?", 28.0, theme::SAND);
                    let g = p.layout(
                        "Try drills on the Paddock or short track lessons: steering, drifting, jumping, laser eyes and items. Start before your first race?".to_string(),
                        theme::ui_medium(13.5),
                        theme::TEXT,
                        w - 48.0,
                    );
                    p.galley(pos2(card.left() + 24.0, card.top() + 54.0), g, theme::TEXT);
                    let by = card.bottom() - 20.0 - 42.0;
                    let bw = (w - 24.0 * 2.0 - S3) * 0.5;
                    let go = widgets::button(ui, Rect::from_min_size(pos2(card.left() + 24.0, by), vec2(bw, 42.0)), "nudge-train", &Btn::primary("Start training"));
                    let skip = widgets::button(ui, Rect::from_min_size(pos2(card.left() + 24.0 + bw + S3, by), vec2(bw, 42.0)), "nudge-skip", &Btn::new("Not now"));
                    if go.clicked() {
                        feedback::select(ctx);
                        ctx.profile.seen_intro = true;
                        ctx.save_profile();
                        out = Transition::Push(Box::new(MenuScreen::then_training()));
                    } else if skip.clicked() {
                        feedback::tap(ctx);
                        ctx.profile.seen_intro = true;
                        ctx.save_profile();
                        out = Transition::Push(Box::new(MenuScreen::new()));
                    }
                });
            }
        });
        self.pill_rect = pill_rect;
        if !out.is_none() {
            self.nudge = None;
        }
        out
    }
}
