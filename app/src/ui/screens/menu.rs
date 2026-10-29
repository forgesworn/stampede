//! The main menu and the Race sub-menu (Quick Race, Grand Prix, Time Trial).

use super::common::{self, wordmark, StagePaint};
use crate::render::ScenePass;
use super::challenges::ChallengesScreen;
use super::credits::CreditsScreen;
use super::garage::GarageScreen;
use super::multiplayer::MultiplayerScreen;
use super::settings::SettingsScreen;
use super::setup::{Mode, SetupScreen};
use super::training::TrainingScreen;
use super::{Ctx, Screen, Transition};
use crate::ui::art::{self, fade, phase, rgba};
use crate::ui::icons::Icon;
use crate::ui::widgets::{self, Btn, S2, S3, S4};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect};
use race_core::Character;

pub struct MenuScreen {
    t: f32,
    stage: StagePaint,
    /// Open Training straight away (the first-run nudge said yes).
    then_training: bool,
    /// Screenshots: a profile with some progress.
    demo: bool,
}

impl Default for MenuScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MenuScreen {
    pub fn new() -> MenuScreen {
        MenuScreen { t: 0.0, stage: StagePaint::default(), then_training: false, demo: false }
    }

    pub fn then_training() -> MenuScreen {
        MenuScreen { t: 0.0, stage: StagePaint::default(), then_training: true, demo: false }
    }

    /// With a wallet and some achievements (screenshots).
    pub fn demo() -> MenuScreen {
        MenuScreen { demo: true, ..MenuScreen::new() }
    }
}

/// The left column shared by the main menu: wordmark and the ostrich head.
fn left_art(p: &egui::Painter, brand: &art::Brand, ctx_screen: Rect, safe: Rect, t: f32, tm: f32) {
    let a_in = art::ease_out_cubic(phase(t, 0.0, 0.6));
    let head = ctx_screen.height() * 0.86;
    let c = pos2(safe.left() + head * 0.40 - (1.0 - a_in) * 30.0, ctx_screen.bottom() - head * 0.5 + (tm * 1.3).sin() * 2.0);
    art::sprite(p, brand.head.id(), c, vec2(head, head), 0.0, false, fade(Color32::WHITE, a_in));
}

impl Screen for MenuScreen {
    fn name(&self) -> &'static str {
        "menu"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if std::mem::take(&mut self.demo) {
            crate::ui::progress_ui::demo_progress(ctx.profile);
        }
        if let Some(target) = ctx.take_join_code() {
            return Transition::Push(Box::new(super::multiplayer::MultiplayerScreen::joining(target)));
        }
        crate::game::stage::show_attract(common::menu_track());
        if self.then_training && self.t > 0.3 {
            self.then_training = false;
            return Transition::Push(Box::new(TrainingScreen::new()));
        }
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
        let safe = ctx.viewport.safe;
        let screen = ctx.viewport.screen;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        common::page(screen, tm, ui, 0.0, |ui, brand| {
            let p = ui.painter().clone();
            let portrait = common::Layout::new(safe).portrait;
            if !portrait { left_art(&p, brand, screen, safe, t, tm); }

            // wordmark, top left
            let a = phase(t, 0.0, 0.3);
            let wsize = (safe.width() * if portrait { 0.78 } else { 0.30 } / 4.45).min(38.0);
            let wc = pos2(safe.left() + S4 + wsize * 2.3, safe.top() + S3 + wsize * 0.62);
            wordmark(&p, wc, wsize, a);

            // the player's ostrich chip under it
            let ch = Character::get(ctx.profile.character.min(7));
            let chip_e = widgets::enter(t, 5);
            let chip_rect = Rect::from_min_size(pos2(safe.left() + S4, safe.top() + S3 + wsize * 1.45), vec2(wsize * 4.6, 34.0));
            widgets::staged(ui, chip_e, vec2(-24.0, 0.0), |ui, off| {
                let r = chip_rect.translate(off);
                let name = ctx.profile.name.clone();
                let resp = widgets::button(ui, r, "menu-profile", &Btn::ghost(&name).size(15.0));
                let p = ui.painter().clone();
                widgets::chip(&p, pos2(r.right() - 17.0, r.center().y), 9.0, art::from_f3(ch.body));
                if resp.clicked() {
                    feedback::tap(ctx);
                    out = Transition::Push(Box::new(SettingsScreen::new()));
                }
            });

            // the wallet (tap: spend it in the Garage) and the achievements trophy, under the name
            let row2_y = chip_rect.bottom() + 8.0 + 15.0;
            let wallet_e = widgets::enter(t, 6);
            widgets::staged(ui, wallet_e, vec2(-24.0, 0.0), |ui, off| {
                let sats = ctx.profile.progress.sats;
                let resp = crate::ui::progress_ui::wallet_pill(ui, brand, chip_rect.left() + off.x, row2_y, 30.0, sats, "menu", tm);
                if resp.clicked() {
                    feedback::tap(ctx);
                    out = Transition::Push(Box::new(GarageScreen::new()));
                }
                let tr = Rect::from_center_size(pos2(chip_rect.right() - 17.0 + off.x, row2_y), vec2(34.0, 34.0));
                if widgets::button(ui, tr, "menu-trophy", &Btn::new("").icon(Icon::Trophy)).clicked() {
                    feedback::tap(ctx);
                    out = Transition::Push(Box::new(crate::ui::progress_ui::AchievementsScreen::new()));
                }
            });

            let credits_rect = Rect::from_min_size(pos2(safe.left() + S4, safe.bottom() - 33.0), vec2(100.0, 28.0));
            if widgets::button(ui, credits_rect, "menu-credits", &Btn::ghost("Credits").icon(Icon::Info).size(13.0)).clicked() {
                out = Transition::Push(Box::new(CreditsScreen::new()));
            }
            let exit_rect = Rect::from_min_size(pos2(credits_rect.right() + 10.0, safe.bottom() - 36.0), vec2(100.0, 32.0));
            if widgets::button(ui, exit_rect, "menu-exit", &Btn::ghost("Exit").icon(Icon::Close).size(14.0)).clicked() {
                ctx.save_profile();
                feedback::on_background();
                out = Transition::Quit;
            }
            // the buttons, right column
            let x0 = safe.left() + if portrait { S4 } else { safe.width() * 0.40 };
            let w = safe.right() - S4 - x0;
            let (hero_h, row_h, gap) = (70.0, if portrait { 66.0 } else { 54.0 }, 9.0);
            let total = hero_h + gap + 3.0 * (row_h + gap) - gap;
            let y0 = if portrait { (row2_y + 44.0).max(safe.top() + (safe.height() - total) * 0.5) } else { safe.top() + ((safe.height() - total) / 2.0).max(S2) };
            let col = (w - gap) / 2.0;
            let row = |i: usize, c: usize, h: f32| {
                let y = y0 + hero_h + gap + i as f32 * (row_h + gap);
                Rect::from_min_size(pos2(x0 + c as f32 * (col + gap), y), vec2(col, h))
            };
            let medals = ctx.profile.challenge_medals.iter().filter(|m| **m > 0).count();
            let chal_sub = if medals > 0 { format!("{medals} of {} medals", race_core::ChallengeId::ALL.len()) } else { "Ten scored runs".to_string() };
            let slots = [
                Rect::from_min_size(pos2(x0, y0), vec2(w, hero_h)),
                row(0, 0, row_h),
                row(0, 1, row_h),
                row(1, 0, row_h),
                row(1, 1, row_h),
                row(2, 0, row_h),
                row(2, 1, row_h),
            ];
            let items: [(&str, Btn, &str); 7] = [
                ("menu-race", Btn::primary("Race").icon(Icon::Flag).sub("Quick Race, Cups, Time Trial, Battle").size(32.0), "race"),
                ("menu-chal", Btn::new("Challenges").icon(Icon::Medal).sub(&chal_sub), "chal"),
                ("menu-multi", Btn::new("Multiplayer").icon(Icon::Globe).sub("Host, join or nearby"), "multi"),
                ("menu-train", Btn::new("Training").icon(Icon::Wheel).sub("Paddock drills & track lessons"), "train"),
                ("menu-garage", Btn::new("Garage").icon(Icon::Ostrich).sub("Pick your ostrich"), "garage"),
                ("menu-settings", Btn::new("Settings").icon(Icon::Gear).sub("Steering, camera & sound"), "settings"),
                ("menu-tracks", Btn::new("My Tracks").icon(Icon::Flag).sub("Draw, run or drive your own"), "tracks"),
            ];
            for (i, (salt, btn, tag)) in items.iter().enumerate() {
                let e = widgets::enter(t, i);
                let clicked = widgets::staged(ui, e, vec2(70.0, 0.0), |ui, off| widgets::button(ui, slots[i].translate(off), salt, btn).clicked());
                if clicked {
                    if *tag == "race" {
                        feedback::select(ctx);
                    } else {
                        feedback::tap(ctx);
                    }
                    out = Transition::Push(match *tag {
                        "race" => Box::new(RaceMenuScreen::new()),
                        "chal" => Box::new(ChallengesScreen::new()),
                        "multi" => Box::new(MultiplayerScreen::new()),
                        "train" => Box::new(TrainingScreen::new()),
                        "garage" => Box::new(GarageScreen::new()),
                        "settings" => Box::new(SettingsScreen::new()),
                        "tracks" => Box::new(super::my_tracks::MyTracksScreen::new()),
                        _ => Box::new(CreditsScreen::new()),
                    });
                }
            }
        });
        out
    }
}

/// Race: Quick Race, Grand Prix, Time Trial.
pub struct RaceMenuScreen {
    t: f32,
    stage: StagePaint,
}

impl Default for RaceMenuScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl RaceMenuScreen {
    pub fn new() -> RaceMenuScreen {
        RaceMenuScreen { t: 0.0, stage: StagePaint::default() }
    }
}

impl Screen for RaceMenuScreen {
    fn name(&self) -> &'static str {
        "race_menu"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        crate::game::stage::show_attract(common::menu_track());
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
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        common::page(ctx.viewport.screen, tm, ui, 0.18, |ui, _brand| {
            let hd = widgets::header(ui, safe, "Race", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let body = hd.body;
            let gap = S3;
            let portrait = common::Layout::new(safe).portrait;
            let cols = if portrait { 2 } else { 4 };
            let w = (body.width() - gap * (cols - 1) as f32) / cols as f32;
            let h = if portrait { ((body.height() - gap) * 0.5).min(260.0) } else { (body.height() - S2).min(212.0) };
            let y = body.top() + (body.height() - if portrait { h * 2.0 + gap } else { h }) / 2.0;
            let wins = ctx.profile.gp_wins;
            let best_any = race_core::TrackId::RACE_ALL.iter().filter_map(|id| common::best_time(ctx.profile, *id)).count();
            let cards: [(&str, &str, &str, Icon, String, Option<Mode>); 4] = [
                ("Quick Race", "Pick a track, set the laps and take on up to seven bots.", "rc-quick", Icon::Flag, "Up to 8 ostriches".into(), Some(Mode::Quick)),
                ("Grand Prix", "Pick Cup 1 or Cup 2: four tracks, points for every place.", "rc-gp", Icon::Trophy, if wins > 0 { format!("Cups won: {wins}") } else { "No cups yet".into() }, Some(Mode::GrandPrix)),
                ("Time Trial", "Just you and the clock. Beat your best lap.", "rc-tt", Icon::Stopwatch, if best_any > 0 { format!("{best_any} of 8 tracks timed") } else { "No times set".into() }, Some(Mode::TimeTrial)),
                ("Battle", "Feather Frenzy: use power-ups and be the last ostrich in.", "rc-battle", Icon::Egg, "Vs bots, 3 minutes".into(), None),
            ];
            for (i, (title, text, salt, icon, foot, mode)) in cards.into_iter().enumerate() {
                let e = widgets::enter(t, i + 1);
                let rect = Rect::from_min_size(pos2(body.left() + (i % cols) as f32 * (w + gap), y + (i / cols) as f32 * (h + gap)), vec2(w, h));
                let clicked = widgets::staged(ui, e, vec2(0.0, 40.0), |ui, off| big_card(ui, rect.translate(off), salt, title, text, &foot, icon, i == 0, tm + i as f32));
                if clicked {
                    feedback::select(ctx);
                    out = Transition::Push(match mode {
                        Some(m) => Box::new(SetupScreen::new(m)),
                        None => Box::new(super::battle::BattleSetupScreen::new()),
                    });
                }
            }
        });
        out
    }
}

/// A tall card button with an icon medallion, a title, a blurb and a footer.
#[allow(clippy::too_many_arguments)]
pub(super) fn big_card(ui: &mut egui::Ui, rect: Rect, salt: &str, title: &str, text: &str, foot: &str, icon: Icon, hero: bool, tm: f32) -> bool {
    let id = egui::Id::new(("stampede-bigcard", salt));
    let resp = ui.interact(rect, id, egui::Sense::click());
    let (scale, sink) = widgets::press_fx(ui, id, &resp);
    let rect = Rect::from_center_size(rect.center() + vec2(0.0, sink * 2.0), rect.size() * scale);
    let p = ui.painter().clone();
    let r = 22.0;
    widgets::soft_shadow(&p, rect, r, 8.0, 20.0, 0.55);
    let (top, bot) = if hero { (Color32::from_rgb(0xFF, 0xB8, 0x44), Color32::from_rgb(0xD8, 0x5A, 0x16)) } else { (Color32::from_rgb(0x7E, 0x47, 0xAC), Color32::from_rgb(0x44, 0x1C, 0x6E)) };
    widgets::rrect_gradient(&p, rect, r, top, bot);
    p.rect_stroke(rect, egui::CornerRadius::same(r as u8), egui::Stroke::new(2.5_f32, theme::OUTLINE), egui::StrokeKind::Inside);
    p.hline((rect.left() + r)..=(rect.right() - r), rect.top() + 3.0, egui::Stroke::new(1.6_f32, rgba(255, 255, 255, 0.35)));
    // medallion
    let mc = pos2(rect.center().x, rect.top() + rect.height() * 0.28);
    let md = (rect.height() * 0.30).min(52.0);
    art::glow(&p, mc, md * 1.5, rgba(255, 255, 255, if hero { 0.20 } else { 0.10 }));
    p.circle_filled(mc + vec2(0.0, 2.0), md * 0.78, rgba(10, 3, 22, 0.35));
    p.circle_filled(mc, md * 0.78, rgba(10, 3, 22, 0.28));
    p.circle_stroke(mc, md * 0.78, egui::Stroke::new(2.0_f32, rgba(255, 255, 255, 0.22)));
    let bob = (tm * 1.8).sin() * 1.2;
    crate::ui::icons::draw(&p, icon, mc + vec2(0.0, bob), md * 1.05, if hero { Color32::WHITE } else { theme::SAND });
    // text
    let ty = rect.top() + rect.height() * 0.52;
    let size = art::fit_size(&p, title, theme::title, 24.0, rect.width() - 20.0);
    art::outlined(&p, pos2(rect.center().x, ty), Align2::CENTER_CENTER, title, theme::title(size), Color32::WHITE, theme::OUTLINE, (size * 0.085).max(1.0), 1.5);
    let g = p.layout(text.to_string(), theme::ui_medium(12.0), if hero { Color32::from_rgb(0x4A, 0x1A, 0x08) } else { theme::TEXT }, rect.width() - 28.0);
    let gx = rect.center().x - g.size().x / 2.0;
    p.galley(pos2(gx, ty + 18.0), g, theme::TEXT);
    // footer pill
    let fc = pos2(rect.center().x, rect.bottom() - 20.0);
    widgets::pill(&p, fc, foot, rgba(10, 3, 22, 0.40), if hero { Color32::WHITE } else { theme::SAND }, 11.0);
    resp.clicked()
}
