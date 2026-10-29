//! Drawing for the progression layer: the wallet pill, the "+N ₿" payout and the
//! achievement toasts on the result screens, and the Achievements screen.
//! The numbers and rules are in `progress`.
//!
//! The fonts have no ₿ glyph, so ₿ is always the game's coin sprite next to a number.

use super::art::{self, ease_out_back, ease_out_cubic, fade, phase, rgba, Brand};
use super::icons::{self, Icon};
use super::progress::{self, grouped, Ach, Earn, Pic, ACHIEVEMENTS};
use super::screens::{common, Ctx, Screen, Transition};
use super::widgets::{self, S2};
use super::{feedback, theme};
use crate::platform::data_dir::Profile;
use egui::{pos2, vec2, Align2, Color32, Id, Painter, Pos2, Rect, Sense, Stroke, Ui};

// ── small pieces ────────────────────────────────────────────────────────────

/// An achievement's picture (an icon or the coin) centred on `c`.
pub fn draw_pic(p: &Painter, brand: &Brand, pic: Pic, c: Pos2, size: f32, col: Color32) {
    match pic {
        Pic::Icon(i) => icons::draw(p, i, c, size, col),
        Pic::Coin => art::coin(p, brand, c, size * 0.95, None, col.a() as f32 / 255.0),
    }
}

/// A round achievement badge of radius `r`: gold when unlocked, a dim socket when not.
pub fn badge(p: &Painter, brand: &Brand, c: Pos2, r: f32, a: Ach, unlocked: bool) {
    if unlocked {
        art::glow(p, c, r * 1.9, rgba(0xFF, 0xC8, 0x3D, 0.22));
        p.circle_filled(c + vec2(0.0, r * 0.10), r + 1.0, rgba(0, 0, 0, 0.35));
        p.circle_filled(c, r, Color32::from_rgb(0x8A, 0x4A, 0x06));
        p.circle_filled(c, r * 0.9, Color32::from_rgb(0xFF, 0xB8, 0x30));
        p.circle_filled(c - vec2(0.0, r * 0.18), r * 0.64, rgba(255, 240, 170, 0.35));
        p.circle_stroke(c, r, Stroke::new(1.6_f32, theme::OUTLINE));
        draw_pic(p, brand, a.pic(), c, r * 1.15, Color32::from_rgb(0x4A, 0x1A, 0x08));
    } else {
        p.circle_filled(c, r, rgba(10, 3, 22, 0.55));
        p.circle_stroke(c, r, Stroke::new(1.5_f32, rgba(255, 255, 255, 0.16)));
        draw_pic(p, brand, a.pic(), c, r * 1.1, rgba(0xB8, 0xA6, 0xC9, 0.55));
    }
}

/// A coin with its centre at `pos` and the number to its right, in `col`.
pub fn amount(p: &Painter, brand: &Brand, pos: Pos2, n: u64, size: f32, col: Color32) {
    art::coin(p, brand, pos + vec2(size * 0.5, 0.0), size * 1.15, None, 1.0);
    p.text(pos + vec2(size * 1.3, 0.0), Align2::LEFT_CENTER, grouped(n), theme::numbers(size), col);
}

/// Width of an [`amount`] drawn at `size`.
pub fn amount_width(p: &Painter, n: u64, size: f32) -> f32 {
    p.layout_no_wrap(grouped(n), theme::numbers(size), Color32::WHITE).size().x + size * 1.3
}

/// The wallet in a glass pill (coin and total). Tapping it answers `clicked()`.
/// `left` is the pill's left edge at the vertical centre `y`.
pub fn wallet_pill(ui: &mut Ui, brand: &Brand, left: f32, y: f32, h: f32, sats: u64, salt: &str, tm: f32) -> egui::Response {
    let p = ui.painter().clone();
    let w = amount_width(&p, sats, h * 0.52) + h * 0.62;
    let rect = Rect::from_min_size(pos2(left, y - h / 2.0), vec2(w, h));
    let id = Id::new(("stampede-wallet", salt));
    let resp = ui.interact(rect, id, Sense::click());
    let (scale, sink) = widgets::press_fx(ui, id, &resp);
    let rect = Rect::from_center_size(rect.center() + vec2(0.0, sink), rect.size() * scale);
    widgets::soft_shadow(&p, rect, h / 2.0, 3.0, 8.0, 0.4);
    widgets::rrect_gradient(&p, rect, h / 2.0, rgba(0x2A, 0x10, 0x4A, 0.92), rgba(0x14, 0x06, 0x28, 0.92));
    p.rect_stroke(rect, egui::CornerRadius::same((h / 2.0) as u8), Stroke::new(1.6_f32, rgba(0xFF, 0xC8, 0x3D, 0.75)), egui::StrokeKind::Inside);
    let size = h * 0.52;
    let spin = ((tm * 0.35) % 1.0 > 0.85).then(|| ((tm * 0.35) % 1.0 - 0.85) / 0.15);
    art::coin(&p, brand, pos2(rect.left() + h * 0.50, rect.center().y), h * 0.66, spin, 1.0);
    p.text(pos2(rect.left() + h * 0.50 + h * 0.46, rect.center().y), Align2::LEFT_CENTER, grouped(sats), theme::numbers(size), theme::GOLD);
    resp
}

/// [`wallet_pill`] with its right edge at `right`, not clickable.
pub fn wallet_pill_right(ui: &mut Ui, brand: &Brand, right: f32, y: f32, h: f32, sats: u64, salt: &str, tm: f32) {
    let w = amount_width(ui.painter(), sats, h * 0.52) + h * 0.62;
    wallet_pill(ui, brand, right - w, y, h, sats, salt, tm);
}

// ── the payout on a results screen ──────────────────────────────────────────

/// What a finished race did for the player: the earnings and the achievements it unlocked.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub earn: Earn,
    pub unlocked: Vec<Ach>,
    /// The wallet after the payout.
    pub wallet: u64,
}

impl Report {
    /// Pay `earn` into the profile and unlock whatever it completed (the profile is saved).
    pub fn settle(ctx: &mut Ctx, played: &progress::Played, earn: Earn) -> Report {
        let unlocked = progress::settle(ctx.profile, played, &earn);
        ctx.save_profile();
        Report { earn, unlocked, wallet: ctx.profile.progress.sats }
    }
}

const TALLY_DELAY: f32 = 0.9;
const TOAST_SLOT: f32 = 2.9;

/// The count-up of "+N ₿" and the unlock toasts that follow it.
pub struct Payout {
    pub report: Report,
    age: f32,
    ticked: u32,
    done_ding: bool,
    chimed: usize,
}

impl Payout {
    pub fn new(report: Report) -> Payout {
        Payout { report, age: 0.0, ticked: 0, done_ding: false, chimed: 0 }
    }

    /// Nothing to show (no ₿ earned, nothing unlocked).
    pub fn is_empty(&self) -> bool {
        self.report.earn.total() == 0 && self.report.unlocked.is_empty()
    }

    fn total(&self) -> u32 {
        self.report.earn.total()
    }

    fn count_secs(&self) -> f32 {
        (0.45 + self.total() as f32 / 70.0).min(1.5)
    }

    fn count(&self) -> f32 {
        ease_out_cubic(phase(self.age, TALLY_DELAY, self.count_secs()))
    }

    fn toast_start(&self, i: usize) -> f32 {
        TALLY_DELAY + self.count_secs() + 0.35 + i as f32 * TOAST_SLOT
    }

    /// Advance the clock and play the coin ticks and chimes.
    pub fn update(&mut self, ctx: &mut Ctx) {
        self.age += ctx.dt;
        let total = self.total();
        if total > 0 {
            let n = (total / 6).clamp(5, 16);
            let want = (self.count() * n as f32) as u32;
            let mut fired = 0;
            while self.ticked < want && fired < 2 {
                feedback::coin(self.ticked.min(7));
                self.ticked += 1;
                fired += 1;
            }
            if !self.done_ding && self.age >= TALLY_DELAY + self.count_secs() {
                self.done_ding = true;
                feedback::coin(7);
            }
        }
        while self.chimed < self.report.unlocked.len() && self.age >= self.toast_start(self.chimed) {
            feedback::unlock(ctx);
            self.chimed += 1;
        }
    }

    /// Paint the "+N" panel in `rect` (about 250 x 66 pt).
    pub fn draw_tally(&self, p: &Painter, brand: &Brand, rect: Rect, tm: f32) {
        let total = self.total();
        if total == 0 {
            return;
        }
        let e = ease_out_back(phase(self.age, 0.35, 0.45)).clamp(0.0, 1.05);
        if e <= 0.0 {
            return;
        }
        let rect = rect.translate(vec2(0.0, (1.0 - e.min(1.0)) * 18.0));
        let mut p = p.clone();
        p.set_opacity(e.clamp(0.0, 1.0));
        widgets::panel(&p, rect, 16.0);
        let counting = self.age < TALLY_DELAY + self.count_secs();
        let shown = (total as f32 * self.count()).round() as u32;
        // coin, wobbling while it counts
        let cc = pos2(rect.left() + 28.0, rect.center().y);
        if counting {
            art::glow(&p, cc, 30.0, rgba(0xFF, 0xC8, 0x3D, 0.25));
        }
        let spin = counting.then(|| (tm * 1.6) % 1.0);
        art::coin(&p, brand, cc, 32.0, spin, 1.0);
        // the number
        let txt = format!("+{}", grouped(shown as u64));
        let num_w = rect.width() * 0.46 - 52.0;
        let size = art::fit_size(&p, &format!("+{}", grouped(total as u64)), theme::numbers, 34.0, num_w.max(40.0));
        let pop = if counting { 1.0 } else { 1.0 + 0.10 * (1.0 - phase(self.age, TALLY_DELAY + self.count_secs(), 0.35)) };
        art::outlined(&p, pos2(rect.left() + 50.0, rect.center().y), Align2::LEFT_CENTER, &txt, theme::numbers(size * pop), theme::GOLD, theme::OUTLINE, 3.0, 1.5);
        // the breakdown
        let x0 = rect.left() + rect.width() * 0.52;
        let x1 = rect.right() - 12.0;
        let ea = self.report.earn;
        let place_label = if ea.place_n == 0 { "Place".to_string() } else { format!("{} place", common::ordinal(ea.place_n)) };
        let mut lines: Vec<(String, String, Color32)> = Vec::new();
        if ea.coins > 0 {
            lines.push(("Coins".into(), format!("{}", ea.coins), theme::SAND));
        }
        if ea.place > 0 {
            lines.push((place_label, format!("{}", ea.place), theme::SAND));
        }
        if ea.bonus > 0 {
            lines.push((match ea.bonus_kind { 1 => "New stars", 3 => "Drill rewards", _ => "New medals" }.into(), format!("{}", ea.bonus), theme::SAND));
        }
        let wallet_shown = self.report.wallet.saturating_sub(total as u64) + shown as u64;
        lines.push(("Wallet".into(), grouped(wallet_shown), theme::GOLD));
        let lh = 13.5;
        let top = rect.center().y - lines.len() as f32 * lh / 2.0 + lh / 2.0;
        for (i, (l, v, col)) in lines.iter().enumerate() {
            let a = ease_out_cubic(phase(self.age, 0.55 + 0.12 * i as f32, 0.3));
            let y = top + i as f32 * lh;
            let last = i + 1 == lines.len();
            p.text(pos2(x0 + (1.0 - a) * 10.0, y), Align2::LEFT_CENTER, l, theme::ui_medium(10.5), fade(theme::TEXT_DIM, a));
            p.text(pos2(x1, y), Align2::RIGHT_CENTER, v, theme::ui_bold(if last { 11.5 } else { 11.0 }), fade(*col, a));
        }
    }

    /// A compact "+N" pill with its left edge at `left_centre` (the training banner).
    pub fn draw_chip(&self, p: &Painter, brand: &Brand, left_centre: Pos2, h: f32, tm: f32) {
        let total = self.total();
        if total == 0 {
            return;
        }
        let counting = self.age < TALLY_DELAY + self.count_secs();
        let shown = (total as f32 * self.count()).round() as u64;
        let txt = format!("+{}", grouped(shown));
        let fs = h * 0.53;
        let g = p.layout_no_wrap(format!("+{}", grouped(total as u64)), theme::numbers(fs), theme::GOLD);
        let r = Rect::from_min_size(left_centre - vec2(0.0, h / 2.0), vec2(g.size().x + h * 1.5, h));
        widgets::panel(p, r, h / 2.0);
        art::coin(p, brand, pos2(r.left() + h * 0.6, r.center().y), h * 0.65, counting.then(|| (tm * 1.6) % 1.0), 1.0);
        p.text(pos2(r.left() + h * 1.12, r.center().y), Align2::LEFT_CENTER, txt, theme::numbers(fs), theme::GOLD);
    }

    /// Paint the unlock toasts, one after another, sliding in from the top of `safe`.
    pub fn draw_toasts(&self, p: &Painter, brand: &Brand, safe: Rect) {
        for (i, a) in self.report.unlocked.iter().enumerate() {
            let age = self.age - self.toast_start(i);
            if !(0.0..TOAST_SLOT - 0.2).contains(&age) {
                continue;
            }
            let k_in = ease_out_back(phase(age, 0.0, 0.45));
            let k_out = ease_out_cubic(phase(age, TOAST_SLOT - 0.7, 0.4));
            let w = 262.0_f32.min(safe.width() - 24.0);
            let rect = Rect::from_center_size(pos2(safe.center().x, safe.top() + 34.0 - (1.0 - k_in) * 70.0 - k_out * 60.0), vec2(w, 50.0));
            widgets::panel_solid(p, rect, 16.0);
            p.rect_stroke(rect, egui::CornerRadius::same(16), Stroke::new(2.0_f32, theme::GOLD), egui::StrokeKind::Inside);
            let shine = phase(age, 0.3, 0.7);
            badge(p, brand, pos2(rect.left() + 28.0, rect.center().y), 17.0, *a, true);
            if shine > 0.0 && shine < 1.0 {
                p.circle_stroke(pos2(rect.left() + 28.0, rect.center().y), 17.0 + 14.0 * shine, Stroke::new(2.0_f32 * (1.0 - shine), fade(theme::GOLD, 1.0 - shine)));
            }
            p.text(pos2(rect.left() + 54.0, rect.center().y - 11.0), Align2::LEFT_CENTER, "ACHIEVEMENT UNLOCKED", theme::ui_bold(9.5), theme::GOLD);
            let size = art::fit_size(p, a.title(), theme::title, 19.0, rect.width() - 66.0);
            p.text(pos2(rect.left() + 54.0, rect.center().y + 7.0), Align2::LEFT_CENTER, a.title(), theme::title(size), theme::TEXT);
        }
    }

    /// The tally panel rect for a column `side` (bottom, above the buttons at `above`).
    pub fn tally_rect(side: Rect, above: f32) -> Rect {
        let w = side.width().min(280.0);
        Rect::from_min_size(pos2(side.left(), above - 66.0), vec2(w, 66.0))
    }
}

// ── the Achievements screen ─────────────────────────────────────────────────

pub struct AchievementsScreen {
    t: f32,
    /// Screenshots: a profile with some progress.
    demo: bool,
}

impl Default for AchievementsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl AchievementsScreen {
    pub fn new() -> AchievementsScreen {
        AchievementsScreen { t: 0.0, demo: false }
    }

    /// Some unlocked, some part way (screenshots).
    pub fn sample() -> AchievementsScreen {
        AchievementsScreen { t: 0.0, demo: true }
    }
}

/// A profile with some progress, for screenshots.
pub fn demo_progress(p: &mut Profile) {
    p.progress.wins = 4;
    p.progress.track_wins = 0b0000_0111;
    p.progress.cups_won = 1;
    p.progress.coins = 640;
    p.progress.bashes = 31;
    p.progress.eggs = 100;
    p.progress.purple_turbos = 9;
    p.progress.best_tricks = 6;
    p.progress.clean_races = 2;
    p.progress.sats = 1_840;
    p.lesson_stars = [3, 3, 3, 3, 2, 1];
    p.challenge_medals = [3, 2, 1, 0, 3, 0, 0, 0, 0, 0];
    progress::unlock_new(p);
}

impl Screen for AchievementsScreen {
    fn name(&self) -> &'static str {
        "achievements"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if std::mem::take(&mut self.demo) {
            demo_progress(ctx.profile);
        }
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
        let profile = ctx.profile.clone();
        common::page(ctx.viewport.screen, tm, ui, 0.72, |ui, brand| {
            let hd = widgets::header(ui, safe, "Achievements", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let p = ui.painter().clone();
            // the count, top right
            let n = progress::unlocked_count(&profile);
            let e0 = widgets::enter(t, 1);
            let mut hp = p.clone();
            hp.set_opacity(e0);
            let cy = if common::Layout::new(safe).portrait { hd.body.top() + 14.0 } else { safe.top() + S2 + 2.0 + widgets::TAP_MIN / 2.0 };
            let label = format!("{n} of {}", (ACHIEVEMENTS.len() - 1));
            let g = hp.layout_no_wrap(label.clone(), theme::ui_bold(14.0), theme::TEXT);
            let wallet_w = amount_width(&hp, profile.progress.sats, 15.0) + 22.0;
            let right = safe.right() - widgets::S4;
            let pill = Rect::from_center_size(pos2(right - wallet_w - 10.0 - (g.size().x + 26.0) / 2.0, cy), vec2(g.size().x + 26.0, 28.0));
            widgets::pill(&hp, pill.center(), &label, rgba(0xF7, 0x93, 0x1A, 0.9), theme::OUTLINE, 14.0);
            let wp = Rect::from_min_size(pos2(right - wallet_w, cy - 14.0), vec2(wallet_w, 28.0));
            widgets::well(&hp, wp, 14.0);
            amount(&hp, brand, pos2(wp.left() + 8.0, wp.center().y), profile.progress.sats, 14.0, theme::GOLD);

            // the grid: four columns, five rows (room for twenty)
            let mut body = hd.body;
            body.min.y -= 6.0;
            let portrait = common::Layout::new(safe).portrait;
            if portrait { body.min.y += 38.0; }
            let (cols, rows) = if portrait { (2usize, 10usize) } else { (4usize, 5usize) };
            let gap = 6.0;
            let cw = (body.width() - gap * (cols as f32 - 1.0)) / cols as f32;
            let ch = ((body.height() - gap * (rows as f32 - 1.0)) / rows as f32).min(58.0);
            for (i, a) in ACHIEVEMENTS.iter().filter(|a| **a != Ach::Bash50).enumerate() {
                let (col, row) = (i % cols, i / cols);
                let r = Rect::from_min_size(pos2(body.left() + col as f32 * (cw + gap), body.top() + row as f32 * (ch + gap)), vec2(cw, ch));
                let e = widgets::enter(t, 2 + i / 2);
                let (cur, goal) = a.progress(&profile);
                let done = a.unlocked(&profile);
                widgets::staged(ui, e, vec2(0.0, 24.0), |ui, off| {
                    let r = r.translate(off);
                    let p = ui.painter().clone();
                    if done {
                        widgets::rrect_gradient(&p, r, 12.0, rgba(0x6A, 0x3A, 0x10, 0.85), rgba(0x3A, 0x18, 0x40, 0.85));
                        p.rect_stroke(r, egui::CornerRadius::same(12), Stroke::new(1.6_f32, rgba(0xFF, 0xC8, 0x3D, 0.9)), egui::StrokeKind::Inside);
                    } else {
                        p.rect_filled(r, 12.0, rgba(10, 4, 24, 0.62));
                        p.rect_stroke(r, egui::CornerRadius::same(12), Stroke::new(1.0_f32, rgba(255, 255, 255, 0.08)), egui::StrokeKind::Inside);
                    }
                    let br = (ch * 0.30).clamp(11.0, 17.0);
                    badge(&p, brand, pos2(r.left() + br + 9.0, r.center().y), br, *a, done);
                    let x0 = r.left() + br * 2.0 + 18.0;
                    let x1 = r.right() - 9.0;
                    let ts = art::fit_size(&p, a.title(), theme::ui_bold, 12.0, x1 - x0 - 36.0);
                    p.text(pos2(x0, r.top() + ch * 0.26), Align2::LEFT_CENTER, a.title(), theme::ui_bold(ts), if done { theme::TEXT } else { theme::SAND });
                    let ds = art::fit_size(&p, a.desc(), theme::ui_medium, 9.5, x1 - x0);
                    p.text(pos2(x0, r.top() + ch * 0.56), Align2::LEFT_CENTER, a.desc(), theme::ui_medium(ds), theme::TEXT_DIM);
                    if done {
                        icons::draw(&p, Icon::Check, pos2(x1 - 6.0, r.top() + ch * 0.26), 13.0, theme::MINT);
                        // a full bar
                        bar(&p, Rect::from_min_max(pos2(x0, r.bottom() - 9.0), pos2(x1, r.bottom() - 5.0)), 1.0, true);
                    } else {
                        p.text(pos2(x1, r.top() + ch * 0.26), Align2::RIGHT_CENTER, format!("{}/{}", grouped(cur as u64), grouped(goal as u64)), theme::ui_bold(10.5), theme::TEXT_DIM);
                        let k = ease_out_cubic(phase(t, 0.35 + 0.04 * i as f32, 0.6));
                        bar(&p, Rect::from_min_max(pos2(x0, r.bottom() - 9.0), pos2(x1, r.bottom() - 5.0)), cur as f32 / goal.max(1) as f32 * k, false);
                    }
                });
            }
        });
        out
    }
}

/// A thin progress bar: orange fill on a dark track, gold when complete.
fn bar(p: &Painter, r: Rect, frac: f32, done: bool) {
    p.rect_filled(r, r.height() / 2.0, rgba(10, 3, 22, 0.6));
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let fill = Rect::from_min_size(r.min, vec2((r.width() * f).max(r.height()), r.height()));
        widgets::rrect_gradient(p, fill, r.height() / 2.0, if done { Color32::from_rgb(0xFF, 0xDC, 0x6E) } else { Color32::from_rgb(0xFF, 0xB8, 0x44) }, if done { Color32::from_rgb(0xD8, 0x84, 0x14) } else { Color32::from_rgb(0xF0, 0x6A, 0x1A) });
    }
}
