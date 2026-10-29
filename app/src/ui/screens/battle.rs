//! Battle ("Feather Frenzy", SPEC 17.1): the setup screen (arena, bots, level)
//! and the results after a battle against bots. The race itself is the normal
//! race screen (`game::race`); online battles use the lobby and
//! `online_results`.

use super::common::{self, fmt_time, ordinal, StagePaint, LEVELS};
use super::garage::GarageScreen;
use super::tracks;
use super::{Ctx, Screen, Transition};
use crate::flow::{BattleRow, BattleSummary, RaceOutcome, RacerResult};
use crate::game::battle_hud::feather;
use crate::render::ScenePass;
use crate::ui::art::{self, ease_out_back, ease_out_cubic, from_f3, phase, rgba};
use crate::ui::widgets::{self, Btn, Seg, S2, S3, S4};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind};
use race_core::{BotLevel, Character, TrackId};

/// One line about each arena, for its card.
pub fn arena_blurb(id: TrackId) -> &'static str {
    match id {
        TrackId::DustBowl => "Day desert round a raised mesa. Two kickers, sand mounds, plenty of eggs.",
        TrackId::MempoolPlaza => "Neon night plaza ringed by pylons. Eight boost pads, fast and loud.",
        _ => "",
    }
}

/// Rules line shown on the setup (the defaults the launch uses).
pub fn rules_line() -> String {
    let r = race_core::BattleRules::default();
    format!("{} feathers each, {} minutes. Power-ups pop feathers. Last ostrich in wins.", r.feathers, r.time_limit / 60)
}

// ── setup ───────────────────────────────────────────────────────────────────

pub struct BattleSetupScreen {
    t: f32,
    /// Index into `TrackId::ARENAS`.
    arena: usize,
    bots: usize,
    level: usize,
}

impl Default for BattleSetupScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl BattleSetupScreen {
    pub fn new() -> BattleSetupScreen {
        BattleSetupScreen { t: 0.0, arena: 0, bots: 5, level: 1 }
    }

    /// Staged for screenshots: Mempool Plaza, seven Hard bots.
    pub fn sample() -> BattleSetupScreen {
        BattleSetupScreen { t: 0.0, arena: 1, bots: 7, level: 2 }
    }
}

fn row_label(p: &egui::Painter, rect: Rect, text: &str) {
    p.text(pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, text, theme::ui_bold(12.5), theme::SAND);
}

/// An arena card: the track's own sky, its outline as the minimap, name and blurb.
fn arena_card(ui: &mut egui::Ui, rect: Rect, id: TrackId, selected: bool, anim: f32) -> bool {
    let wid = Id::new(("stampede-arena", id.index()));
    let resp = ui.interact(rect, wid, Sense::click());
    let (scale, _) = widgets::press_fx(ui, wid, &resp);
    let sel = ui.ctx().animate_bool_with_time(wid.with("sel"), selected, 0.14);
    let rect = Rect::from_center_size(rect.center(), rect.size() * (scale * (1.0 + 0.008 * sel)));
    let p = ui.painter().clone();
    let info = tracks::info(id);
    let r = 18.0;
    widgets::soft_shadow(&p, rect, r, 6.0, 16.0, 0.55);
    let top = theme::mix(info.sky_top, theme::INDIGO, if info.night { 0.0 } else { 0.28 });
    let hor = theme::mix(info.sky_horizon, theme::BURNT, if info.night { 0.2 } else { 0.22 });
    widgets::rrect_gradient(&p, rect, r, top, theme::mix(hor, info.ground, 0.5));
    if info.night {
        for i in 0..18u32 {
            let q = pos2(rect.left() + art::hash01(i * 3) * rect.width(), rect.top() + art::hash01(i * 3 + 1) * rect.height() * 0.4);
            p.circle_filled(q, 0.8, rgba(255, 240, 220, 0.5));
        }
    }
    // the minimap fills the upper part
    let strip_h = (rect.height() * 0.40).min(104.0);
    let map = Rect::from_min_max(rect.min + vec2(18.0, 14.0), pos2(rect.right() - 18.0, rect.bottom() - strip_h - 8.0));
    tracks::minimap(&p, map, &info, theme::mix(info.road, Color32::WHITE, 0.72), ease_out_cubic(anim));
    // text strip
    let strip = Rect::from_min_max(pos2(rect.left(), rect.bottom() - strip_h), rect.max);
    p.rect_filled(strip, CornerRadius { nw: 0, ne: 0, sw: r as u8, se: r as u8 }, rgba(14, 5, 30, 0.82));
    p.hline(strip.x_range(), strip.top(), Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)));
    let size = art::fit_size(&p, &info.name, theme::title, 20.0, strip.width() - 24.0);
    p.text(pos2(strip.left() + 12.0, strip.top() + 17.0), Align2::LEFT_CENTER, &info.name, theme::title(size), theme::TEXT);
    let g = p.layout(arena_blurb(id).to_string(), theme::ui_medium(10.8), theme::TEXT_DIM, strip.width() - 24.0);
    let gh = g.size().y;
    p.galley(pos2(strip.left() + 12.0, strip.top() + 31.0), g, theme::TEXT_DIM);
    p.text(pos2(strip.left() + 12.0, strip.top() + 31.0 + gh + 8.0), Align2::LEFT_CENTER, format!("{:.0} m loop", info.length), theme::ui_bold(10.8), theme::SAND);
    p.rect_stroke(rect, CornerRadius::same(r as u8), Stroke::new(2.0_f32, theme::mix_rgba(theme::OUTLINE, theme::GOLD, sel)), StrokeKind::Inside);
    if sel > 0.01 {
        p.rect_stroke(rect.expand(1.5), CornerRadius::same(r as u8 + 1), Stroke::new(2.5 * sel, art::fade(theme::GOLD, sel)), StrokeKind::Outside);
        art::glow(&p, rect.center(), rect.width() * 0.8, rgba(0xFF, 0xC8, 0x3D, 0.06 * sel));
    }
    resp.clicked()
}

impl Screen for BattleSetupScreen {
    fn name(&self) -> &'static str {
        "battle_setup"
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
        let (mut arena, mut bots, mut level) = (self.arena, self.bots, self.level);
        common::page(ctx.viewport.screen, tm, ui, 0.22, |ui, _brand| {
            let hd = widgets::header(ui, safe, "Battle", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let body = hd.body;
            let rw = 268.0_f32.min(body.width() * 0.36);
            let gap = 14.0;
            let (left, right) = common::Layout::new(safe).split(body, rw, body.height() * 0.48, gap);

            // ── the two arenas ──
            let cg = 10.0;
            let cw = (left.width() - cg) / 2.0;
            for (i, id) in TrackId::ARENAS.iter().enumerate() {
                let rect = Rect::from_min_size(pos2(left.left() + i as f32 * (cw + cg), left.top()), vec2(cw, left.height()));
                let e = widgets::enter(t, i + 1);
                let clicked = widgets::staged(ui, e, vec2(0.0, 36.0), |ui, off| arena_card(ui, rect.translate(off), *id, arena == i, (t - 0.2 - 0.1 * i as f32) / 0.7));
                if clicked && arena != i {
                    feedback::tap(ctx);
                    arena = i;
                }
            }

            // ── the settings panel ──
            let e = widgets::enter(t, 4);
            widgets::staged(ui, e, vec2(50.0, 0.0), |ui, off| {
                let col = right.translate(off);
                let p = ui.painter().clone();
                widgets::panel(&p, col, 20.0);
                let inner = col.shrink(12.0);
                let rh = 38.0;
                let lw = 58.0;
                let mut y = inner.top();
                let mut next_row = |h: f32| {
                    let r = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), h));
                    y += h + 11.0;
                    r
                };
                // your ostrich
                let r = next_row(rh);
                row_label(&p, r, "Ostrich");
                let ch = Character::get(ctx.profile.character.min(7));
                let br = Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max);
                let b = widgets::button(ui, br, "battle-ostrich", &Btn::new(ch.name).size(17.0));
                widgets::chip(&p, pos2(br.left() + 17.0, br.center().y), 8.5, from_f3(ch.body));
                p.text(pos2(br.right() - 14.0, br.center().y), Align2::RIGHT_CENTER, "change", theme::ui_bold(10.5), theme::TEXT_DIM);
                if b.clicked() {
                    feedback::tap(ctx);
                    out = Transition::Push(Box::new(GarageScreen::new()));
                }
                // bots 1-7
                let r = next_row(rh);
                row_label(&p, r, "Bots");
                let ctl = Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max);
                let bw = rh;
                let minus = Rect::from_min_size(ctl.left_top(), vec2(bw + 6.0, rh));
                let plus = Rect::from_min_size(pos2(ctl.right() - bw - 6.0, ctl.top()), vec2(bw + 6.0, rh));
                if widgets::button(ui, minus, "battle-bots-minus", &Btn::new("-").enabled(bots > 1).size(24.0)).clicked() {
                    feedback::tap(ctx);
                    bots -= 1;
                }
                if widgets::button(ui, plus, "battle-bots-plus", &Btn::new("+").enabled(bots < 7).size(24.0)).clicked() {
                    feedback::tap(ctx);
                    bots += 1;
                }
                art::outlined(&p, ctl.center(), Align2::CENTER_CENTER, &format!("{bots}"), theme::numbers(26.0), theme::TEXT, theme::OUTLINE, 2.0, 0.0);
                // bot level
                let r = next_row(rh);
                row_label(&p, r, "Bots are");
                let segs: Vec<Seg> = LEVELS.iter().map(|(n, _)| Seg::new(n)).collect();
                if let Some(i) = widgets::segmented(ui, Rect::from_min_max(pos2(r.left() + lw + 6.0, r.top()), r.max), "battle-level", &segs, level) {
                    feedback::tap(ctx);
                    level = i;
                }
                // the rules, in the middle
                let go_h = 54.0;
                let mid = Rect::from_min_max(pos2(inner.left(), y - 1.0), pos2(inner.right(), inner.bottom() - go_h - 9.0));
                if mid.height() > 24.0 {
                    widgets::well(&p, mid, 12.0);
                    let c = pos2(mid.left() + 20.0, mid.center().y);
                    feather(&p, c, 26.0, 0.0, 1.0, true);
                    let g = p.layout(rules_line(), theme::ui_medium(10.4), theme::TEXT_DIM, mid.width() - 46.0);
                    let gy = mid.center().y - g.size().y / 2.0;
                    p.galley(pos2(mid.left() + 38.0, gy), g, theme::TEXT_DIM);
                }
                // go
                let go = Rect::from_min_max(pos2(inner.left(), inner.bottom() - go_h), inner.max);
                if widgets::button(ui, go, "battle-go", &Btn::primary("Battle!").size(30.0)).clicked() {
                    feedback::select(ctx);
                    let launch = common::battle_launch(ctx.profile, TrackId::ARENAS[arena], bots, LEVELS[level].1);
                    out = Transition::Push(crate::game::race_screen(launch));
                }
            });
        });
        self.arena = arena;
        self.bots = bots;
        self.level = level;
        out
    }
}

// ── results ─────────────────────────────────────────────────────────────────

pub struct BattleResultsScreen {
    out: RaceOutcome,
    t: f32,
    stage: StagePaint,
    applied: bool,
    payout: Option<crate::ui::progress_ui::Payout>,
}

impl BattleResultsScreen {
    pub fn new(out: RaceOutcome) -> BattleResultsScreen {
        BattleResultsScreen { out, t: 0.0, stage: StagePaint::default(), applied: false, payout: None }
    }

    /// A finished 8-racer battle on Dust Bowl, you 3rd (screenshots).
    pub fn sample() -> BattleResultsScreen {
        BattleResultsScreen::new(sample_outcome())
    }

    fn local(&self) -> Option<&RacerResult> {
        self.out.results.iter().find(|r| r.is_local)
    }
}

/// The characters on the first three podium steps.
fn top3(results: &[RacerResult]) -> [Option<u8>; 3] {
    let mut v: Vec<&RacerResult> = results.iter().collect();
    v.sort_by_key(|r| r.place);
    let get = |i: usize| v.get(i).map(|r| r.character);
    [get(0), get(1), get(2)]
}

/// "WINNER", "OUT 3rd", or "TIME" for a survivor when the clock ended it.
pub fn status_text(r: &BattleRow, place: u8, time_up: bool) -> String {
    match r.out_order {
        Some(_) => "OUT".to_string(),
        None if place == 1 && !time_up => "WINNER".to_string(),
        None => "IN".to_string(),
    }
}

impl Screen for BattleResultsScreen {
    fn name(&self) -> &'static str {
        "battle_results"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !std::mem::replace(&mut self.applied, true) {
            use crate::ui::progress::{Earn, Played};
            let report = crate::ui::progress_ui::Report::settle(ctx, &Played::of_outcome(&self.out), Earn::of_outcome(&self.out, 0, 0));
            self.payout = Some(crate::ui::progress_ui::Payout::new(report));
        }
        if let Some(po) = self.payout.as_mut() {
            po.update(ctx);
        }
        crate::game::stage::show_podium(top3(&self.out.results), self.out.launch.config.track);
        crate::game::stage::set_focus(if common::Layout::new(ctx.viewport.safe).portrait { 0.5 } else { 0.80 });
        if common::Layout::new(ctx.viewport.safe).portrait { crate::game::stage::set_portrait_focus(0.65, 3.2); }
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
        let launch = self.out.launch.clone();
        let results = self.out.results.clone();
        let summary = self.out.battle.unwrap_or(BattleSummary { duration: 0.0, time_up: false });
        let me = self.local().cloned();
        let over3d = crate::game::stage::is_drawing();
        let won = me.as_ref().is_some_and(|m| m.place == 1);
        let payout = self.payout.as_ref();
        common::page(ctx.viewport.screen, tm, ui, 0.10, |ui, brand| {
            let area = Rect::from_min_max(safe.min + vec2(S4, S3), safe.max - vec2(S4, S3));
            let gap = S3;
            let tw = (area.width() * 0.585).min(520.0);
            let (table, side) = common::Layout::new(safe).split(area, area.width() - tw - gap, 374.0, gap);
            let p = ui.painter().clone();

            // ── the table ──
            widgets::staged(ui, widgets::enter(t, 0), vec2(-30.0, 0.0), |ui, off| {
                let panel = table.translate(off);
                let p = ui.painter().clone();
                widgets::panel(&p, panel, 20.0);
                let inner = panel.shrink2(vec2(14.0, 10.0));
                let place = me.as_ref().map(|m| m.place).unwrap_or(0);
                let headline = if place == 1 { "You won!".to_string() } else if place > 0 { format!("{} place", ordinal(place)) } else { "Results".to_string() };
                widgets::heading(&p, pos2(inner.left(), inner.top() + 17.0), Align2::LEFT_CENTER, &headline, 30.0, if place == 1 { theme::GOLD } else { theme::TEXT });
                let info = tracks::info(launch.config.track);
                p.text(pos2(inner.right(), inner.top() + 9.0), Align2::RIGHT_CENTER, &info.name, theme::ui_bold(12.5), theme::SAND);
                let how = if summary.time_up { "time ran out" } else { "last one standing" };
                p.text(pos2(inner.right(), inner.top() + 25.0), Align2::RIGHT_CENTER, format!("Battle, {} ({how})", fmt_time(summary.duration)), theme::ui_medium(11.0), theme::TEXT_DIM);
                // column heads
                let hy = inner.top() + 48.0;
                let (c_out, c_taken, c_hits, c_feath) = (inner.right(), inner.right() - 62.0, inner.right() - 62.0 - 52.0, inner.right() - 62.0 - 52.0 - 62.0);
                let head = |x: f32, a: Align2, s: &str| p.text(pos2(x, hy), a, s, theme::ui_bold(10.5), theme::TEXT_DIM);
                head(inner.left() + 4.0, Align2::LEFT_CENTER, "Place");
                head(inner.left() + 66.0, Align2::LEFT_CENTER, "Racer");
                head(c_feath, Align2::CENTER_CENTER, "Feathers");
                head(c_hits, Align2::CENTER_CENTER, "Hits");
                head(c_taken, Align2::CENTER_CENTER, "Lost");
                head(c_out, Align2::RIGHT_CENTER, "");
                p.hline(inner.x_range(), hy + 11.0, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)));
                // rows
                let n = results.len().max(1);
                let top = hy + 16.0;
                let rh = ((inner.bottom() - top) / n as f32).min(34.0);
                for (i, r) in results.iter().enumerate() {
                    let re = ease_out_cubic(phase(t, 0.45 + 0.10 * i as f32, 0.35));
                    if re <= 0.0 {
                        continue;
                    }
                    let y = top + i as f32 * rh;
                    let row = Rect::from_min_size(pos2(inner.left() - 6.0 + (1.0 - re) * 36.0, y), vec2(inner.width() + 12.0, rh - 3.0));
                    let mut rp = p.clone();
                    rp.set_opacity(re);
                    if r.is_local {
                        rp.rect_filled(row, 10.0, rgba(0xF7, 0x93, 0x1A, 0.22));
                        rp.rect_stroke(row, CornerRadius::same(10), Stroke::new(1.5_f32, rgba(0xF7, 0x93, 0x1A, 0.8)), StrokeKind::Inside);
                    } else if i % 2 == 0 {
                        rp.rect_filled(row, 10.0, rgba(255, 255, 255, 0.035));
                    }
                    let cy = row.center().y;
                    let dx = row.left() + 6.0;
                    let pc = match r.place {
                        1 => theme::GOLD,
                        2 => Color32::from_rgb(0xD8, 0xD4, 0xE8),
                        3 => Color32::from_rgb(0xE2, 0x9A, 0x5E),
                        _ => theme::TEXT_DIM,
                    };
                    art::outlined(&rp, pos2(dx + 22.0, cy), Align2::CENTER_CENTER, &format!("{}", r.place), theme::title(19.0), pc, theme::OUTLINE, 1.5, 0.0);
                    widgets::chip(&rp, pos2(dx + 58.0, cy), 8.5, from_f3(Character::get(r.character.min(7)).body));
                    let nm = art::fit_size(&rp, &r.name, theme::ui_bold, 14.0, c_feath - 38.0 - (dx + 72.0));
                    rp.text(pos2(dx + 74.0, cy), Align2::LEFT_CENTER, &r.name, theme::ui_bold(nm), if r.is_local { Color32::WHITE } else { theme::TEXT });
                    if let Some(b) = r.battle {
                        // feathers left: icons (or a count past five), dim "-" when out
                        if b.feathers == 0 {
                            rp.text(pos2(c_feath, cy), Align2::CENTER_CENTER, "out", theme::ui_bold(12.0), Color32::from_rgb(255, 120, 110));
                        } else if b.feathers <= 5 {
                            for k in 0..b.feathers {
                                let fx = c_feath + (k as f32 - (b.feathers as f32 - 1.0) / 2.0) * 11.0;
                                feather(&rp, pos2(fx, cy), 15.0, 0.0, 1.0, true);
                            }
                        } else {
                            feather(&rp, pos2(c_feath - 12.0, cy), 15.0, 0.0, 1.0, true);
                            rp.text(pos2(c_feath + 2.0, cy), Align2::LEFT_CENTER, format!("x{}", b.feathers), theme::numbers(13.0), theme::TEXT);
                        }
                        rp.text(pos2(c_hits, cy), Align2::CENTER_CENTER, format!("{}", b.hits), theme::numbers(15.0), if b.hits > 0 { theme::MINT } else { theme::TEXT_DIM });
                        rp.text(pos2(c_taken, cy), Align2::CENTER_CENTER, format!("{}", b.taken), theme::numbers(15.0), if b.taken > 0 { Color32::from_rgb(255, 140, 120) } else { theme::TEXT_DIM });
                        let st = status_text(&b, r.place, summary.time_up);
                        let col = if st == "WINNER" { theme::GOLD } else if st == "OUT" { theme::TEXT_DIM } else { theme::MINT };
                        rp.text(pos2(c_out + 2.0, cy), Align2::RIGHT_CENTER, st, theme::ui_bold(10.5), col);
                    }
                }
            });

            // ── the side: podium (painted fallback) and buttons ──
            if !over3d {
                let pod = Rect::from_min_max(pos2(side.left() + 6.0, side.top() + 14.0), pos2(side.right() - 6.0, side.bottom() - 74.0));
                let mut pp = p.clone();
                pp.set_opacity(widgets::enter(t, 1));
                let names: Vec<(u8, String)> = {
                    let mut v: Vec<&RacerResult> = results.iter().collect();
                    v.sort_by_key(|r| r.place);
                    v.iter().take(3).map(|r| (r.character, r.name.clone())).collect()
                };
                let who = |i: usize| names.get(i).map(|(c, n)| (*c, n.as_str()));
                paint_podium(&pp, brand, pod, [who(0), who(1), who(2)], t, tm);
            }
            if won {
                widgets::confetti(&p, brand, Rect::from_min_max(side.min, pos2(side.right(), safe.bottom())), tm, 26);
            }
            let bh = 50.0;
            let by = side.bottom() - bh;
            let labels: Vec<(&str, &str, Btn)> = vec![("bres-retry", "retry", Btn::new("Again")), ("bres-next", "next", Btn::primary("Other arena")), ("bres-menu", "menu", Btn::new("Menu"))];
            let n = labels.len() as f32;
            let bw = (side.width() - S2 * (n - 1.0)) / n;
            for (i, (salt, tag, btn)) in labels.iter().enumerate() {
                let r = Rect::from_min_size(pos2(side.left() + i as f32 * (bw + S2), by), vec2(bw, bh));
                let eb = widgets::enter(t, 3 + i);
                let clicked = widgets::staged(ui, eb, vec2(0.0, 30.0), |ui, off| widgets::button(ui, r.translate(off), salt, btn).clicked());
                if clicked {
                    feedback::select(ctx);
                    out = match *tag {
                        "retry" => Transition::Replace(crate::game::race_screen(common::retry(&launch))),
                        "next" => {
                            let mut l = common::retry(&launch);
                            l.config.track = common::next_arena(l.config.track);
                            Transition::Replace(crate::game::race_screen(l))
                        }
                        _ => Transition::PopTo("menu"),
                    };
                }
            }
            if let Some(po) = payout {
                po.draw_tally(&p, brand, crate::ui::progress_ui::Payout::tally_rect(side, by - S2), tm);
                po.draw_toasts(&p, brand, Rect::from_min_max(pos2(side.left(), safe.top()), safe.max));
            }
        });
        out
    }
}

/// Three podium blocks (2nd, 1st, 3rd) with the ostrich heads on top: the painted fallback while the 3D stage is not drawing.
fn paint_podium(p: &egui::Painter, brand: &art::Brand, rect: Rect, who: [Option<(u8, &str)>; 3], t: f32, tm: f32) {
    let order = [(1usize, 0.62), (0, 1.0), (2, 0.42)];
    let bw = (rect.width() - 16.0) / 3.0;
    let base = rect.bottom();
    for (slot, (idx, hk)) in order.iter().enumerate() {
        let e = ease_out_back(phase(t, 0.25 + 0.18 * (2 - *idx) as f32, 0.45));
        let bh = rect.height() * 0.44 * hk * e.max(0.0);
        let x = rect.left() + slot as f32 * (bw + 8.0);
        let block = Rect::from_min_size(pos2(x, base - bh), vec2(bw, bh));
        widgets::soft_shadow(p, block, 8.0, 4.0, 12.0, 0.4);
        let (top, bot) = match idx {
            0 => (Color32::from_rgb(0xFF, 0xD2, 0x5E), Color32::from_rgb(0xD8, 0x84, 0x14)),
            1 => (Color32::from_rgb(0xE6, 0xE2, 0xF0), Color32::from_rgb(0x8E, 0x86, 0xA8)),
            _ => (Color32::from_rgb(0xE2, 0x9A, 0x5E), Color32::from_rgb(0x9A, 0x52, 0x24)),
        };
        widgets::rrect_gradient(p, block, 8.0, top, bot);
        p.rect_stroke(block, CornerRadius::same(8), Stroke::new(2.0_f32, theme::OUTLINE), StrokeKind::Inside);
        if bh > 24.0 {
            art::outlined(p, block.center_top() + vec2(0.0, 22.0), Align2::CENTER_CENTER, &format!("{}", idx + 1), theme::title(26.0), Color32::WHITE, theme::OUTLINE, 2.0, 1.0);
        }
        let Some((char_id, name)) = who[*idx] else { continue };
        let ch = Character::get(char_id);
        let hs = rect.height() * 0.46 * (0.82 + 0.18 * hk);
        let tint = theme::mix(Color32::WHITE, from_f3(ch.neck), 0.55);
        let hop = if *idx == 0 { ((tm * 4.0).sin().abs()) * 6.0 } else { 0.0 };
        let cx = block.center().x;
        art::sprite(p, brand.head.id(), pos2(cx + hs * 0.04, block.top() - hs * 0.42 - hop), vec2(hs, hs), 0.0, false, art::fade(tint, e.clamp(0.0, 1.0)));
        if e > 0.9 {
            p.text(pos2(cx, base + 9.0), Align2::CENTER_CENTER, name, theme::ui_bold(art::fit_size(p, name, theme::ui_bold, 11.0, bw)), theme::SAND);
        }
    }
}

/// A fake finished battle on Dust Bowl with eight racers (screenshots).
pub fn sample_outcome() -> RaceOutcome {
    let profile = crate::platform::data_dir::Profile { name: "Rider".into(), character: 2, ..Default::default() };
    let launch = common::battle_launch(&profile, TrackId::DustBowl, 7, BotLevel::Normal);
    let n = launch.config.racers.len();
    // you finish 3rd: (seat, feathers, hits, taken); out racers in the order they went out
    let order: [usize; 8] = [4, 6, 0, 2, 7, 1, 5, 3];
    let feathers = [2u8, 1, 0, 0, 3, 0, 1, 0];
    let hits = [4u16, 3, 2, 3, 6, 1, 2, 0];
    let taken = [2u16, 2, 3, 3, 1, 3, 2, 3];
    let out_k = [None, None, Some(2u8), Some(5), None, Some(4), None, Some(3)];
    let _ = n;
    let results = order
        .iter()
        .enumerate()
        .map(|(pos, &seat)| {
            let s = &launch.config.racers[seat];
            RacerResult {
                name: s.name.clone(),
                character: s.character,
                place: pos as u8 + 1,
                time_s: None,
                best_lap_s: None,
                coins: 0,
                is_local: seat == 0,
                is_bot: seat != 0,
                battle: Some(BattleRow { feathers: feathers[seat], hits: hits[seat], taken: taken[seat], out_order: out_k[seat] }),
            }
        })
        .collect();
    RaceOutcome { launch, results, lesson: None, challenge: None, quit: false, ghost: None, battle: Some(BattleSummary { duration: 101.4, time_up: true }), stats: Default::default(), gp_recorded: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::RaceKind;

    #[test]
    fn setup_offers_exactly_the_arenas() {
        assert_eq!(TrackId::ARENAS.len(), 2);
        for id in TrackId::ARENAS {
            assert!(id.is_arena());
            assert!(!arena_blurb(id).is_empty());
        }
        let l = common::battle_launch(&crate::platform::data_dir::Profile::default(), TrackId::Dune, 3, BotLevel::Easy);
        assert!(l.config.track.is_arena(), "a race track is never launched as a battle");
    }

    #[test]
    fn launch_has_you_plus_the_bots() {
        let profile = crate::platform::data_dir::Profile { name: "Ada".into(), character: 3, ..Default::default() };
        for bots in [1usize, 5, 7, 12] {
            let l = common::battle_launch(&profile, TrackId::MempoolPlaza, bots, BotLevel::Hard);
            assert_eq!(l.config.racers.len(), bots.clamp(1, 7) + 1);
            assert!(matches!(l.config.mode, race_core::RaceMode::Battle(_)));
            assert!(matches!(l.kind, RaceKind::Battle));
            assert_eq!((l.config.racers[0].name.as_str(), l.config.racers[0].character), ("Ada", 3));
            assert!(matches!(l.config.racers[0].kind, race_core::RacerKind::Human));
            assert!(l.config.racers[1..].iter().all(|r| matches!(r.kind, race_core::RacerKind::Bot(BotLevel::Hard)) && r.character != 3));
        }
    }

    #[test]
    fn next_arena_swaps() {
        assert_eq!(common::next_arena(TrackId::DustBowl), TrackId::MempoolPlaza);
        assert_eq!(common::next_arena(TrackId::MempoolPlaza), TrackId::DustBowl);
    }

    #[test]
    fn sample_has_one_winner_row_status_text() {
        let o = sample_outcome();
        assert_eq!(o.results.len(), 8);
        let st = |k: usize| status_text(&o.results[k].battle.unwrap(), o.results[k].place, false);
        assert_eq!(st(0), "WINNER");
        assert_eq!(st(7), "OUT");
    }
}
