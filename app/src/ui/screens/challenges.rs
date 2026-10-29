//! Challenges: ten scored runs as cards (track, what it asks, your medal, the
//! medal thresholds). A tap starts the race; the result screen is in
//! `after_race`.

use super::common::{self, medal_name};
use crate::flow::{ChallengeResult, RaceKind, RaceLaunch, RaceOutcome};
use crate::ui::widgets::{Btn, S2, S4};
use super::tracks;
use super::{Ctx, Screen, Transition};
use crate::ui::art::{self, fade, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, S3};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind};
use race_core::ChallengeId;

pub struct ChallengesScreen {
    t: f32,
    /// Screenshots: a profile with some medals.
    demo: bool,
}

impl Default for ChallengesScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl ChallengesScreen {
    pub fn new() -> ChallengesScreen {
        ChallengesScreen { t: 0.0, demo: false }
    }

    /// Staged for screenshots: gold, silver, bronze and a few open.
    pub fn sample() -> ChallengesScreen {
        ChallengesScreen { t: 0.0, demo: true }
    }
}

/// Bronze / silver / gold as (top, bottom, rim).
pub fn medal_colors(level: u8) -> (Color32, Color32, Color32) {
    match level {
        1 => (Color32::from_rgb(0xEC, 0xA8, 0x6C), Color32::from_rgb(0x9A, 0x52, 0x24), Color32::from_rgb(0x5E, 0x2C, 0x0E)),
        2 => (Color32::from_rgb(0xF2, 0xF0, 0xFA), Color32::from_rgb(0x8E, 0x86, 0xA8), Color32::from_rgb(0x4A, 0x44, 0x62)),
        _ => (Color32::from_rgb(0xFF, 0xDC, 0x6E), Color32::from_rgb(0xD8, 0x84, 0x14), Color32::from_rgb(0x8A, 0x4A, 0x06)),
    }
}

/// A medal disc of radius `r` (`level` 1-3). `level` 0 draws an empty socket.
/// `shine` 0..1 sweeps a highlight across it.
pub fn medal(p: &egui::Painter, c: egui::Pos2, r: f32, level: u8, shine: f32) {
    if level == 0 {
        p.circle_filled(c, r, rgba(10, 3, 22, 0.45));
        p.circle_stroke(c, r, Stroke::new((r * 0.12).max(1.0), rgba(255, 255, 255, 0.2)));
        return;
    }
    let (top, bot, rim) = medal_colors(level);
    p.circle_filled(c + vec2(0.0, r * 0.12), r + r * 0.05, rgba(0, 0, 0, 0.35));
    p.circle_filled(c, r, rim);
    p.circle_filled(c, r * 0.9, bot);
    // upper half lighter: a gradient-ish cap
    let n = 5;
    for i in 0..n {
        let k = i as f32 / n as f32;
        p.circle_filled(c - vec2(0.0, r * 0.06 * (1.0 - k)), r * (0.86 - 0.12 * k), theme::mix(top, bot, k * 0.9));
    }
    p.circle_stroke(c, r * 0.66, Stroke::new((r * 0.07).max(0.8), fade(rim, 0.55)));
    icons::star(p, c + vec2(0.0, r * 0.03), r * 0.46, fade(Color32::WHITE, 0.92), Stroke::new((r * 0.07).max(0.8), rim));
    if shine > 0.0 && shine < 1.0 {
        let x = art::lerp(-1.2, 1.2, shine) * r;
        let a = (1.0 - (shine * 2.0 - 1.0).abs()).clamp(0.0, 1.0);
        p.add(egui::Shape::convex_polygon(vec![c + vec2(x - r * 0.18, -r * 0.8), c + vec2(x + r * 0.10, -r * 0.8), c + vec2(x + r * 0.30, r * 0.8), c + vec2(x + r * 0.02, r * 0.8)], rgba(255, 255, 255, 0.35 * a), Stroke::NONE));
    }
}

/// A threshold as the card shows it: `70`, `3rd`, `1:20`.
pub fn fmt_threshold(id: ChallengeId, v: f32) -> String {
    match id {
        ChallengeId::EggHunt | ChallengeId::TurboTimeAttack | ChallengeId::LaserSniper => format!("{}:{:02}", (v / 60.0).floor() as u32, (v % 60.0).round() as u32),
        ChallengeId::Elimination | ChallengeId::LastFeather => common::ordinal(v.round() as u8),
        _ => format!("{}", v.round() as i64),
    }
}

impl Screen for ChallengesScreen {
    fn name(&self) -> &'static str {
        "challenges"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if std::mem::take(&mut self.demo) {
            // Coin Rush gold, Egg Hunt silver, Elimination bronze, Bash Brawl bronze,
            // Ramp Rider bronze, Last Feather silver; the rest open.
            ctx.profile.challenge_medals = [3, 2, 1, 0, 1, 0, 0, 1, 0, 2];
            ctx.profile.challenge_best = [138.0, 55.4, 3.0, 0.0, 6.0, 0.0, 0.0, 7.0, 0.0, 2.0];
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
        let medals = ctx.profile.challenge_medals;
        let bests = ctx.profile.challenge_best;
        common::page(ctx.viewport.screen, tm, ui, 0.36, |ui, _brand| {
            let hd = widgets::header(ui, safe, "Challenges", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let p = ui.painter().clone();
            // medal count, top right (like Training's progress pill)
            let got = medals.iter().enumerate().filter(|(i, m)| *i != 4 && **m > 0).count();
            let gold = medals.iter().enumerate().filter(|(i, m)| *i != 4 && **m == 3).count();
            let portrait = common::Layout::new(safe).portrait;
            let pc = pos2(safe.right() - 16.0 - 105.0, if portrait { hd.body.top() + 18.0 } else { safe.top() + 34.0 });
            {
                let mut pp = p.clone();
                pp.set_opacity(widgets::enter(t, 1));
                let r = Rect::from_center_size(pc, vec2(210.0, 34.0));
                widgets::well(&pp, r, 17.0);
                pp.text(pos2(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, format!("{got} of {} medals", (ChallengeId::ALL.len() - 1)), theme::ui_bold(13.0), theme::TEXT);
                medal(&pp, pos2(r.right() - 52.0, r.center().y), 9.0, 3, 0.0);
                pp.text(pos2(r.right() - 38.0, r.center().y), Align2::LEFT_CENTER, format!("x {gold}"), theme::ui_bold(13.0), theme::SAND);
            }

            // ten cards, two rows of five: each is still ~130 x 130 pt, well over a thumb
            let mut body = hd.body;
            if portrait { body.min.y += 46.0; }
            common::scroll(ui, body, "challenges", if portrait { 840.0 } else { body.height() }, |ui, body| {
            let gap = S2 + 2.0;
            let cols = if portrait { 2 } else { 5 };
            let rows = (ChallengeId::ALL.len() - 1).div_ceil(cols);
            let (cw, ch) = ((body.width() - gap * (cols - 1) as f32) / cols as f32, (body.height() - gap * (rows - 1) as f32) / rows as f32);
            for (col, (i, id)) in ChallengeId::ALL.iter().enumerate().filter(|(_, id)| **id != ChallengeId::BashBrawl).enumerate() {
                let rect = Rect::from_min_size(pos2(body.left() + (col % cols) as f32 * (cw + gap), body.top() + (col / cols) as f32 * (ch + gap)), vec2(cw, ch));
                let e = widgets::enter(t, i + 1);
                let best = (bests[i] > 0.0).then_some(bests[i]);
                let clicked = widgets::staged(ui, e, vec2(0.0, 36.0), |ui, off| card(ui, rect.translate(off), *id, medals[i], best, tm));
                if clicked {
                    feedback::select(ctx);
                    out = Transition::Push(crate::game::race_screen(common::challenge_launch(ctx.profile, *id)));
                }
            }
            });
        });
        out
    }
}

/// The one-or-two-line blurb a card carries (the full `description()` is too long
/// for ten cards on one screen).
fn blurb(id: ChallengeId) -> &'static str {
    match id {
        ChallengeId::CoinRush => "Grab bitcoins for 90s. They grow back.",
        ChallengeId::EggHunt => "Collect 20 eggs, fast.",
        ChallengeId::Elimination => "Last place is out every 25s.",
        ChallengeId::DriftKing => "Two laps solo. Long drifts score most.",
        ChallengeId::BashBrawl => "60s of head-bashing. Steals count double.",
        ChallengeId::TurboTimeAttack => "Three laps, three Turbo Feathers.",
        ChallengeId::LaserSniper => "Burn 15 rivals. Each miss costs 2s.",
        ChallengeId::RampRider => "Land ramp tricks. Chain them for double.",
        ChallengeId::NightCoinRush => "120s of night coins. Boost pads refill.",
        ChallengeId::LastFeather => "One feather against five rivals' three.",
    }
}

fn card(ui: &mut egui::Ui, rect: Rect, id: ChallengeId, medal_level: u8, best: Option<f32>, tm: f32) -> bool {
    let wid = Id::new(("stampede-challenge", id.number()));
    let resp = ui.interact(rect, wid, Sense::click());
    let (scale, _) = widgets::press_fx(ui, wid, &resp);
    let rect = Rect::from_center_size(rect.center(), rect.size() * scale);
    let p = ui.painter().clone();
    let r = 14.0;
    let info = tracks::info(id.track());
    widgets::soft_shadow(&p, rect, r, 4.0, 12.0, 0.5);
    // the track's own sky, so each card reads as its place
    let top = theme::mix(info.sky_top, theme::INDIGO, if info.night { 0.0 } else { 0.5 });
    let hor = theme::mix(info.sky_horizon, theme::BURNT, if info.night { 0.2 } else { 0.22 });
    widgets::rrect_gradient(&p, rect, r, top, theme::mix(hor, info.ground, 0.5));
    // a dark body under the head so the text always reads
    let head_h = 40.0;
    let bodyr = Rect::from_min_max(pos2(rect.left(), rect.top() + head_h), rect.max);
    p.rect_filled(bodyr, CornerRadius { nw: 0, ne: 0, sw: r as u8, se: r as u8 }, rgba(14, 5, 30, 0.82));
    p.hline(rect.x_range(), bodyr.top(), Stroke::new(1.0_f32, rgba(255, 255, 255, 0.12)));

    // number badge, title (up to two lines), medal
    let bc = pos2(rect.left() + 17.0, rect.top() + 20.0);
    p.circle_filled(bc + vec2(0.0, 1.5), 10.5, rgba(0, 0, 0, 0.4));
    p.circle_filled(bc, 10.5, theme::OUTLINE);
    p.circle_filled(bc, 8.8, if medal_level > 0 { theme::MINT } else { theme::ACCENT });
    if medal_level > 0 {
        icons::draw(&p, Icon::Check, bc, 12.0, theme::OUTLINE);
    } else {
        p.text(bc + vec2(0.0, 0.5), Align2::CENTER_CENTER, format!("{}", id.number()), theme::title(12.0), Color32::WHITE);
    }
    let medal_r = 11.5;
    let tx = rect.left() + 32.0;
    let tw = rect.right() - medal_r * 2.0 - 8.0 - 3.0 - tx;
    let title = id.title();
    let one = art::fit_size(&p, title, theme::title, 15.0, tw);
    let out = |text: &str, size: f32, y: f32| {
        art::outlined(&p, pos2(tx, y), Align2::LEFT_CENTER, text, theme::title(size), theme::TEXT, theme::OUTLINE, (size * 0.07).max(1.0), 1.0);
    };
    match title.match_indices(' ').min_by_key(|(i, _)| (*i as i32 * 2 - title.len() as i32).abs()) {
        Some((i, _)) if one < 13.0 => {
            let (l1, l2) = (&title[..i], &title[i + 1..]);
            let sz = art::fit_size(&p, l1, theme::title, 14.0, tw).min(art::fit_size(&p, l2, theme::title, 14.0, tw));
            out(l1, sz, rect.top() + 20.0 - sz * 0.55);
            out(l2, sz, rect.top() + 20.0 + sz * 0.55);
        }
        _ => out(title, one, rect.top() + 20.0),
    }
    let shine = if medal_level > 0 { ((tm * 0.35 + id.number() as f32 * 0.37) % 4.0) / 1.2 } else { 2.0 };
    medal(&p, pos2(rect.right() - medal_r - 6.0, rect.top() + 20.0), medal_r, medal_level, shine);

    // track name
    let lx = rect.left() + 10.0;
    let ty = bodyr.top() + 10.0;
    icons::draw(&p, Icon::Flag, pos2(lx + 4.0, ty), 10.0, theme::SAND);
    let nsize = art::fit_size(&p, &info.name, theme::ui_bold, 10.5, rect.width() - 20.0 - 12.0);
    p.text(pos2(lx + 12.0, ty), Align2::LEFT_CENTER, &info.name, theme::ui_bold(nsize), theme::SAND);

    // what it asks: a short blurb, two lines at most
    let desc_w = rect.width() - 20.0;
    let mut size = 10.2;
    let mut g = p.layout(blurb(id).to_string(), theme::ui_medium(size), theme::TEXT, desc_w);
    while g.size().y > 26.0 && size > 8.4 {
        size -= 0.4;
        g = p.layout(blurb(id).to_string(), theme::ui_medium(size), theme::TEXT, desc_w);
    }
    p.galley(pos2(lx, ty + 9.0), g, theme::TEXT);

    // thresholds: bronze, silver, gold
    let fy = rect.bottom() - 12.0;
    let sepy = fy - 11.0;
    p.hline((rect.left() + 10.0)..=(rect.right() - 10.0), sepy, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)));
    // best, right above the thresholds (or a hint when open)
    let by = sepy - 8.0;
    match best {
        Some(b) => {
            p.text(pos2(lx, by), Align2::LEFT_CENTER, "Best", theme::ui_medium(10.0), theme::TEXT_DIM);
            p.text(pos2(rect.right() - 10.0, by), Align2::RIGHT_CENTER, common::fmt_score(id, b), theme::ui_bold(11.0), theme::GOLD);
        }
        None => {
            p.text(pos2(lx, by), Align2::LEFT_CENTER, "Not yet played", theme::ui_medium(10.0), theme::TEXT_DIM);
        }
    }
    let th = id.medal_thresholds();
    let slot = (rect.width() - 20.0) / 3.0;
    for (k, v) in th.iter().enumerate() {
        let cx = rect.left() + 10.0 + slot * (k as f32 + 0.5);
        let earned = medal_level as usize > k;
        medal(&p, pos2(cx - 11.0, fy), 6.0, (k + 1) as u8, 2.0);
        p.text(pos2(cx - 3.0, fy), Align2::LEFT_CENTER, fmt_threshold(id, *v), theme::ui_bold(10.5), if earned { theme::TEXT } else { theme::TEXT_DIM });
    }

    // rim
    let rim = if medal_level == 3 { theme::GOLD } else if medal_level > 0 { rgba(0x6B, 0xE0, 0xA8, 0.55) } else { theme::OUTLINE };
    p.rect_stroke(rect, CornerRadius::same(r as u8), Stroke::new(2.0_f32, rim), StrokeKind::Inside);
    resp.clicked()
}


// ── results ─────────────────────────────────────────────────────────────────

/// After a challenge: the score counting up, then the medal landing.
pub struct ChallengeResultScreen {
    out: RaceOutcome,
    res: ChallengeResult,
    t: f32,
    applied: bool,
    new_best: bool,
    prev_medal: u8,
    payout: Option<crate::ui::progress_ui::Payout>,
}

impl ChallengeResultScreen {
    pub fn new(out: RaceOutcome) -> ChallengeResultScreen {
        let res = out.challenge.clone().unwrap_or(ChallengeResult { id: ChallengeId::CoinRush, score: 0.0, medal: 0, done: false, progress: 0 });
        ChallengeResultScreen { out, res, t: 0.0, applied: false, new_best: false, prev_medal: 0, payout: None }
    }

    /// A fake finished challenge (screenshots, tests).
    pub fn sample(id: ChallengeId, score: f32, progress: u32) -> ChallengeResultScreen {
        let profile = crate::platform::data_dir::Profile { name: "Rider".into(), ..Default::default() };
        let launch = common::challenge_launch(&profile, id);
        let res = ChallengeResult { id, score, medal: id.medal_for(score), done: true, progress };
        let mut s = ChallengeResultScreen::new(RaceOutcome { launch, results: Vec::new(), lesson: None, challenge: Some(res), quit: false, ghost: None, battle: None, stats: Default::default(), gp_recorded: None });
        s.applied = true;
        s.new_best = true;
        s
    }

    fn launch(&self) -> &RaceLaunch {
        &self.out.launch
    }
}

/// Elimination: `place` is the player's final place among 8. Everyone below them was
/// knocked out before them (the sim's own counter also counts the player's exit).
fn elimination_detail(place: f32) -> String {
    let place = place.round().clamp(1.0, 8.0) as u32;
    if place <= 1 {
        "Last one standing!".to_string()
    } else {
        format!("{} of 7 rivals knocked out first", 8 - place)
    }
}

/// Last Feather: `score` is the final place among six; `progress` the rivals knocked out.
fn last_feather_detail(score: f32, progress: u32) -> String {
    if score.round() <= 1.0 {
        "Last ostrich standing!".to_string()
    } else {
        format!("{} of 5 rivals knocked out", progress.min(5))
    }
}

/// The line under the score: what happened besides the number.
fn detail(r: &ChallengeResult) -> Option<String> {
    match r.id {
        ChallengeId::Elimination => Some(elimination_detail(r.score)),
        ChallengeId::EggHunt if !common::challenge_completed(r) => Some(format!("{} of 20 eggs before time ran out", r.progress)),
        ChallengeId::DriftKing => Some(format!("{} scoring drifts", r.progress)),
        ChallengeId::LaserSniper if !common::challenge_completed(r) => Some(format!("{} of 15 eggs before time ran out", r.progress)),
        ChallengeId::RampRider => Some(format!("{} tricks landed", r.progress)),
        ChallengeId::LastFeather => Some(last_feather_detail(r.score, r.progress)),
        _ => None,
    }
}

impl Screen for ChallengeResultScreen {
    fn name(&self) -> &'static str {
        "challenge_result"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !self.applied {
            self.applied = true;
            let i = ChallengeId::ALL.iter().position(|c| *c == self.res.id).unwrap_or(0);
            self.prev_medal = ctx.profile.challenge_medals[i];
            self.new_best = common::record_challenge(ctx.profile, self.res.id, self.res.score, self.res.medal, common::challenge_completed(&self.res));
            ctx.save_profile();
            use crate::ui::progress::{Earn, Played};
            let report = crate::ui::progress_ui::Report::settle(ctx, &Played::of_outcome(&self.out), Earn::of_outcome(&self.out, 0, self.prev_medal));
            self.payout = Some(crate::ui::progress_ui::Payout::new(report));
        }
        if let Some(po) = self.payout.as_mut() {
            po.update(ctx);
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
        let res = self.res.clone();
        let id = res.id;
        let launch = self.launch().clone();
        let new_best = self.new_best;
        let prev_medal = self.prev_medal;
        let info = tracks::info(id.track());
        let payout = self.payout.as_ref();
        let tally_h = if payout.is_some_and(|p| p.report.earn.total() > 0) { 74.0 } else { 0.0 };
        common::page(ctx.viewport.screen, tm, ui, 0.30, |ui, brand| {
            let p = ui.painter().clone();
            let area = Rect::from_min_max(safe.min + vec2(S4, S3), safe.max - vec2(S4, S3));
            let gap = S3;
            let lw = (area.width() * 0.40).min(300.0);
            let (left, right) = common::Layout::new(safe).split(area, area.width() - lw - gap, 320.0, gap);

            // ── the medal ──
            let count = art::ease_out_cubic(art::phase(t, 0.35, 1.1));
            let land_t = 1.55;
            let k = art::ease_out_back(art::phase(t, land_t, 0.5));
            let mc = pos2(left.center().x, left.top() + (left.height() - tally_h) / 2.0 - 14.0);
            let big = (left.width().min(left.height() - tally_h) * 0.30).min(78.0);
            if res.medal > 0 {
                let (top, _, _) = medal_colors(res.medal);
                // rays and glow behind, once it has landed
                let ray = art::phase(t, land_t, 0.8);
                if ray > 0.0 {
                    for i in 0..14 {
                        let a = tm * 0.22 + i as f32 / 14.0 * std::f32::consts::TAU;
                        let (r0, r1, w) = (big * 0.9, big * (1.2 + 1.5 * ray), 0.10);
                        let pts = vec![mc + vec2(a.cos(), a.sin()) * r0, mc + vec2((a - w).cos(), (a - w).sin()) * r1, mc + vec2((a + w).cos(), (a + w).sin()) * r1];
                        p.add(egui::Shape::convex_polygon(pts, fade(top, 0.10 + 0.04 * res.medal as f32), Stroke::NONE));
                    }
                    art::glow(&p, mc, big * 2.4 * ray, fade(top, 0.28));
                }
                // the drop: from above, overshoot; the ribbon hangs from the top with it
                let y = mc.y - (1.0 - k.min(1.2)) * 90.0;
                let alpha = art::phase(t, land_t, 0.12);
                if alpha > 0.0 {
                    let (_, _, rim) = medal_colors(res.medal);
                    let red = Color32::from_rgb(0xC8, 0x2A, 0x3A);
                    let kk = k.clamp(0.0, 1.0);
                    let mut mp = p.clone();
                    mp.set_opacity(alpha);
                    for sgn in [-1.0f32, 1.0] {
                        let q = |x: f32, yy: f32| pos2(mc.x + sgn * x * big, y + yy * big);
                        mp.add(egui::Shape::convex_polygon(vec![q(0.62, -1.75), q(0.22, -1.75), q(-0.12, -0.45), q(0.28, -0.45)], theme::mix(red, Color32::BLACK, if sgn < 0.0 { 0.0 } else { 0.18 }), Stroke::new(1.5_f32, fade(rim, kk))));
                    }
                    medal(&mp, pos2(mc.x, y), big * (0.6 + 0.4 * k.clamp(0.0, 1.2)), res.medal, ((t - land_t - 0.5) / 0.9).clamp(0.0, 1.0));
                }
                if t > land_t + 0.15 && t < land_t + 0.3 {
                    // landing ring
                    let rr = (t - land_t - 0.15) / 0.15;
                    p.circle_stroke(mc, big * (1.0 + 0.8 * rr), Stroke::new(3.0_f32 * (1.0 - rr), fade(Color32::WHITE, 1.0 - rr)));
                }
                let name = medal_name(res.medal);
                let ne = art::ease_out_cubic(art::phase(t, land_t + 0.25, 0.4));
                let mut tp = p.clone();
                tp.set_opacity(ne);
                widgets::heading(&tp, pos2(mc.x, mc.y + big + 22.0 - (1.0 - ne) * 8.0), Align2::CENTER_CENTER, &format!("{} medal!", name), 26.0, medal_colors(res.medal).0);
                if res.medal >= 2 || new_best {
                    widgets::confetti(&p, brand, Rect::from_min_max(safe.min, pos2(safe.right(), safe.bottom())), (t - land_t).max(0.0), if res.medal == 3 { 40 } else { 18 });
                }
            } else {
                let ne = art::ease_out_cubic(art::phase(t, 1.2, 0.4));
                medal(&p, mc, big, 0, 2.0);
                let mut tp = p.clone();
                tp.set_opacity(ne);
                widgets::heading(&tp, pos2(mc.x, mc.y + big + 22.0), Align2::CENTER_CENTER, "No medal this time", 22.0, theme::TEXT);
                let need = id.medal_thresholds()[0];
                tp.text(pos2(mc.x, mc.y + big + 44.0), Align2::CENTER_CENTER, format!("Bronze needs {}", fmt_threshold(id, need)), theme::ui_bold(12.5), theme::SAND);
            }

            // ── the numbers ──
            let e = widgets::enter(t, 0);
            widgets::staged(ui, e, vec2(40.0, 0.0), |ui, off| {
                let panel = right.translate(off);
                let p = ui.painter().clone();
                widgets::panel(&p, panel, 20.0);
                let inner = panel.shrink2(vec2(16.0, 12.0));
                widgets::heading(&p, pos2(inner.left(), inner.top() + 17.0), Align2::LEFT_CENTER, id.title(), 30.0, theme::TEXT);
                p.text(pos2(inner.right(), inner.top() + 9.0), Align2::RIGHT_CENTER, &info.name, theme::ui_bold(12.5), theme::SAND);
                p.text(pos2(inner.right(), inner.top() + 25.0), Align2::RIGHT_CENTER, format!("Challenge {}", id.number()), theme::ui_medium(11.0), theme::TEXT_DIM);
                p.hline(inner.x_range(), inner.top() + 42.0, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)));

                // the score, counting up
                let sc = pos2(inner.left(), inner.top() + 72.0);
                p.text(sc - vec2(0.0, 22.0), Align2::LEFT_CENTER, id.score_label().to_uppercase(), theme::ui_bold(11.0), theme::TEXT_DIM);
                let shown = if id.higher_is_better() || matches!(id, ChallengeId::EggHunt | ChallengeId::TurboTimeAttack) { res.score * count } else { res.score };
                let txt = if count < 1.0 && matches!(id, ChallengeId::EggHunt | ChallengeId::TurboTimeAttack) { common::fmt_time(shown) } else { common::fmt_score(id, shown) };
                let r = art::outlined(&p, sc + vec2(0.0, 6.0), Align2::LEFT_CENTER, &txt, theme::numbers(50.0), if res.medal == 3 { theme::GOLD } else { Color32::WHITE }, theme::OUTLINE, 3.0, 2.0);
                if new_best && t > land_t {
                    let kb = art::ease_out_back(art::phase(t, land_t + 0.1, 0.4));
                    let mut bp = p.clone();
                    bp.set_opacity(kb.clamp(0.0, 1.0));
                    widgets::pill(&bp, pos2(r.right() + 62.0, sc.y + 6.0), "NEW BEST", theme::GOLD, theme::OUTLINE, 12.0 * kb.max(0.5));
                }
                if let Some(d) = detail(&res) {
                    p.text(sc + vec2(0.0, 40.0), Align2::LEFT_CENTER, d, theme::ui_bold(12.5), theme::SAND);
                }

                // the three medals and what they need
                let ty = inner.top() + 168.0;
                let th = id.medal_thresholds();
                let slot = inner.width() / 3.0;
                for (kx, v) in th.iter().enumerate() {
                    let cx = inner.left() + slot * (kx as f32 + 0.5);
                    let lvl = (kx + 1) as u8;
                    let got = res.medal >= lvl;
                    let we = art::phase(t, 0.5 + 0.14 * kx as f32, 0.3);
                    let mut mp = p.clone();
                    mp.set_opacity(we);
                    if got {
                        mp.rect_filled(Rect::from_center_size(pos2(cx, ty + 4.0), vec2(slot - 10.0, 56.0)), 12.0, rgba(0xF7, 0x93, 0x1A, 0.16));
                    }
                    medal(&mp, pos2(cx - 24.0, ty), 13.0, lvl, 2.0);
                    mp.text(pos2(cx - 6.0, ty - 6.0), Align2::LEFT_CENTER, fmt_threshold(id, *v), theme::numbers(17.0), if got { Color32::WHITE } else { theme::TEXT_DIM });
                    mp.text(pos2(cx - 6.0, ty + 12.0), Align2::LEFT_CENTER, medal_name(lvl), theme::ui_medium(10.5), if got { theme::SAND } else { theme::TEXT_DIM });
                    if got && prev_medal < lvl && t > land_t {
                        icons::draw(&mp, Icon::Check, pos2(cx + 40.0, ty - 6.0), 14.0, theme::MINT);
                    } else if got {
                        icons::draw(&mp, Icon::Check, pos2(cx + 40.0, ty - 6.0), 14.0, theme::MINT.gamma_multiply(0.7));
                    }
                }

                // buttons
                let bh = 48.0;
                let by = inner.bottom() - bh;
                let labels = [("chres-retry", "retry", Btn::primary("Retry")), ("chres-next", "next", Btn::new("Next challenge")), ("chres-list", "list", Btn::new("Challenges"))];
                let ws = [inner.width() * 0.28, inner.width() * 0.40, inner.width() * 0.32 - 2.0 * S2];
                let mut x = inner.left();
                for (i, (salt, tag, btn)) in labels.iter().enumerate() {
                    let r = Rect::from_min_size(pos2(x, by), vec2(ws[i], bh)).translate(vec2(0.0, 0.0));
                    x += ws[i] + S2;
                    let eb = widgets::enter(t, 3 + i);
                    let clicked = widgets::staged(ui, eb, vec2(0.0, 30.0), |ui, off| widgets::button(ui, r.translate(off), salt, btn).clicked());
                    if clicked {
                        feedback::select(ctx);
                        out = match *tag {
                            "retry" => Transition::Replace(crate::game::race_screen(common::retry(&launch))),
                            "next" => {
                                let n = ChallengeId::ALL.iter().position(|c| *c == id).unwrap_or(0);
                                let mut next = (n + 1) % ChallengeId::ALL.len();
                                if ChallengeId::ALL[next] == ChallengeId::BashBrawl { next += 1; }
                                let nx = ChallengeId::ALL[next];
                                Transition::Replace(crate::game::race_screen(common::challenge_launch(ctx.profile, nx)))
                            }
                            _ => Transition::Pop,
                        };
                    }
                }
            });
            if let Some(po) = payout {
                let w = left.width().min(262.0);
                po.draw_tally(&p, brand, Rect::from_min_size(pos2(left.left(), left.bottom() - 66.0), vec2(w, 66.0)), tm);
                po.draw_toasts(&p, brand, Rect::from_min_max(pos2(left.left(), safe.top()), pos2(left.right(), safe.bottom())));
            }
        });
        let _ = matches!(launch_kind(&self.out), RaceKind::Challenge(_));
        out
    }
}

fn launch_kind(o: &RaceOutcome) -> &RaceKind {
    &o.launch.kind
}

#[cfg(test)]
mod tests {
    use super::elimination_detail;

    #[test]
    fn elimination_counts_only_rivals() {
        assert_eq!(elimination_detail(1.0), "Last one standing!");
        assert_eq!(elimination_detail(2.0), "6 of 7 rivals knocked out first");
        assert_eq!(elimination_detail(4.0), "4 of 7 rivals knocked out first");
        assert_eq!(elimination_detail(8.0), "0 of 7 rivals knocked out first");
    }
}
