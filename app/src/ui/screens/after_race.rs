//! What follows a race: results (Quick Race, Time Trial, Online), Grand Prix
//! standings after every round and the trophy at the end, and the return to the
//! lesson list after Training. Owned by the menus agent; gameplay only calls
//! [`after_race`].
//!
//! Stack shape: the setup screen pushed the race; the race returns
//! `after_race(outcome)`, a `Replace` that swaps the race for the screen built
//! here. Menu buttons then `PopTo("menu")`; Back goes to the setup screen.
//! Profile writes (best times, cups, lesson stars) happen in the new screen's
//! first `update`, where a `Ctx` exists.

use super::common::{self, fmt_time, ordinal, StagePaint};
use super::tracks;
use super::{Ctx, Screen, Transition};
use crate::flow::{GpRecorded, LessonResult, RaceKind, RaceLaunch, RaceOutcome, RacerResult};
use crate::render::ScenePass;
use crate::ui::art::{self, ease_out_back, ease_out_cubic, fade, from_f3, phase, rgba};
use crate::ui::icons;
use crate::ui::progress::{self, Earn, Played};
use crate::ui::progress_ui::{Payout, Report};
use crate::ui::widgets::{self, Btn, S2, S3, S4};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect, Stroke};
use race_core::{Character, TrackId, GP_POINTS};

/// The transition the race screen returns when its race is over.
pub fn after_race(outcome: RaceOutcome) -> Transition {
    if outcome.quit {
        // Back to wherever the race was started from.
        return Transition::Pop;
    }
    match outcome.launch.kind.clone() {
        RaceKind::Quick | RaceKind::TimeTrial => Transition::Replace(Box::new(ResultsScreen::new(outcome))),
        RaceKind::Battle => Transition::Replace(Box::new(super::battle::BattleResultsScreen::new(outcome))),
        // Online results belong to the session (`OnlineResultsScreen`, via `RaceSource::exit_transition`);
        // reaching here means the session is already gone: back to the multiplayer menu.
        RaceKind::Online => Transition::PopTo("multiplayer"),
        RaceKind::GrandPrix { .. } => Transition::Replace(Box::new(GpScreen::new(outcome))),
        RaceKind::Challenge(_) => Transition::Replace(Box::new(super::challenges::ChallengeResultScreen::new(outcome))),
        // A drill shows its own results card inside the race screen; leaving goes back to the list.
        RaceKind::Drill(_) => Transition::Pop,
        RaceKind::Training(lesson) => {
            let result = outcome.lesson.unwrap_or(LessonResult { lesson, done: false, stars: 0, time_s: 0.0 });
            // The lesson list is still under the race: hand it the result and pop back to it.
            crate::ui::progress::stash_stats(outcome.stats);
            super::training::hand_back(result);
            Transition::Pop
        }
    }
}

/// The characters on the first three podium steps; `None` where nobody finished that high.
fn top3(results: &[RacerResult]) -> [Option<u8>; 3] {
    let mut v: Vec<&RacerResult> = results.iter().collect();
    v.sort_by_key(|r| r.place);
    let get = |i: usize| v.get(i).map(|r| r.character);
    [get(0), get(1), get(2)]
}

// ── the painted podium (used while the 3D stage is not drawing) ─────────────

/// Three podium blocks (2nd, 1st, 3rd) with the ostrich heads on top.
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
        p.rect_stroke(block, egui::CornerRadius::same(8), Stroke::new(2.0_f32, theme::OUTLINE), egui::StrokeKind::Inside);
        if bh > 24.0 {
            art::outlined(p, block.center_top() + vec2(0.0, 22.0), Align2::CENTER_CENTER, &format!("{}", idx + 1), theme::title(26.0), Color32::WHITE, theme::OUTLINE, 2.0, 1.0);
        }
        // the ostrich (nobody on this step with fewer than three racers)
        let Some((char_id, name)) = who[*idx] else { continue };
        let ch = Character::get(char_id);
        let hs = rect.height() * 0.46 * (0.82 + 0.18 * hk);
        let tint = theme::mix(Color32::WHITE, from_f3(ch.neck), 0.55);
        let hop = if *idx == 0 { ((tm * 4.0).sin().abs()) * 6.0 } else { 0.0 };
        let cx = block.center().x;
        art::sprite(p, brand.head.id(), pos2(cx + hs * 0.04, block.top() - hs * 0.42 - hop), vec2(hs, hs), 0.0, false, fade(tint, e.clamp(0.0, 1.0)));
        if e > 0.9 {
            p.text(pos2(cx, base + 9.0), Align2::CENTER_CENTER, name, theme::ui_bold(art::fit_size(p, name, theme::ui_bold, 11.0, bw)), theme::SAND);
        }
    }
}

// ── results ─────────────────────────────────────────────────────────────────

pub struct ResultsScreen {
    out: RaceOutcome,
    t: f32,
    applied: bool,
    new_time: bool,
    new_lap: bool,
    stage: StagePaint,
    /// The ₿ earned and the achievements unlocked.
    payout: Option<Payout>,
    /// Screenshots: a wallet to start from.
    seed_sats: Option<u64>,
}

impl ResultsScreen {
    pub fn new(out: RaceOutcome) -> ResultsScreen {
        ResultsScreen { out, t: 0.0, applied: false, new_time: false, new_lap: false, stage: StagePaint::default(), payout: None, seed_sats: None }
    }

    /// A won race with a payout on the way and two achievements to unlock (screenshots).
    pub fn sats_sample() -> ResultsScreen {
        let mut o = sample_outcome(SampleKind::QuickWin);
        if let Some(me) = o.results.iter_mut().find(|r| r.is_local) {
            me.coins = 9;
        }
        o.stats.bashes = 4;
        ResultsScreen { seed_sats: Some(1_240), ..ResultsScreen::new(o) }
    }

    fn local(&self) -> Option<&RacerResult> {
        self.out.results.iter().find(|r| r.is_local)
    }

    /// Record bests (once, here and nowhere else) and remember whether this race set them.
    fn apply(&mut self, ctx: &mut Ctx) {
        self.applied = true;
        if !matches!(self.out.launch.kind, RaceKind::Quick | RaceKind::TimeTrial) {
            return;
        }
        let Some(me) = self.local().cloned() else { return };
        if let Some(n) = self.seed_sats {
            ctx.profile.progress.sats = n;
        }
        let b = common::record_bests(ctx.profile, self.out.launch.config.track, self.out.launch.config.laps, &me);
        self.new_time = b.new_time;
        self.new_lap = b.new_lap;
        ctx.save_profile();
        let report = Report::settle(ctx, &Played::of_outcome(&self.out), Earn::of_outcome(&self.out, 0, 0));
        self.payout = Some(Payout::new(report));
    }
}

impl Screen for ResultsScreen {
    fn name(&self) -> &'static str {
        "results"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !self.applied {
            self.apply(ctx);
        }
        if let Some(po) = self.payout.as_mut() {
            po.update(ctx);
        }
        crate::game::stage::show_podium(top3(&self.out.results), self.out.launch.config.track);
        // the table covers the left 58%: the podium sits in the strip on the right
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
        let (new_time, new_lap) = (self.new_time, self.new_lap);
        let me = self.local().cloned();
        let ghost_res = self.out.ghost;
        let over3d = crate::game::stage::is_drawing();
        let won = me.as_ref().map(|m| m.place == 1).unwrap_or(false);
        let payout = self.payout.as_ref();
        common::page(ctx.viewport.screen, tm, ui, 0.10, |ui, brand| {
            let p = ui.painter().clone();
            let area = Rect::from_min_max(safe.min + vec2(S4, S3), safe.max - vec2(S4, S3));
            let gap = S3;
            let tw = (area.width() * 0.585).min(520.0);
            let (table, side) = common::Layout::new(safe).split(area, area.width() - tw - gap, 374.0, gap);

            // ── the table ──
            let e = widgets::enter(t, 0);
            widgets::staged(ui, e, vec2(-30.0, 0.0), |ui, off| {
                let panel = table.translate(off);
                let p = ui.painter().clone();
                widgets::panel(&p, panel, 20.0);
                let inner = panel.shrink2(vec2(14.0, 10.0));
                let compact = common::Layout::new(safe).portrait;
                // headline
                let place = me.as_ref().map(|m| m.place).unwrap_or(0);
                let headline = if place == 1 { "You won!".to_string() } else if place > 0 { format!("{} place", ordinal(place)) } else { "Results".to_string() };
                widgets::heading(&p, pos2(inner.left(), inner.top() + 17.0), Align2::LEFT_CENTER, &headline, 30.0, if place == 1 { theme::GOLD } else { theme::TEXT });
                let info = tracks::info(launch.config.track);
                let mode = match launch.kind {
                    RaceKind::TimeTrial => "Time Trial",
                    _ => "Quick Race",
                };
                p.text(pos2(inner.right(), inner.top() + if compact { 40.0 } else { 9.0 }), Align2::RIGHT_CENTER, &info.name, theme::ui_bold(12.5), theme::SAND);
                p.text(pos2(inner.right(), inner.top() + if compact { 55.0 } else { 25.0 }), Align2::RIGHT_CENTER, format!("{mode}, {} lap{}", launch.config.laps, if launch.config.laps == 1 { "" } else { "s" }), theme::ui_medium(11.0), theme::TEXT_DIM);
                // column heads
                let hy = inner.top() + if compact { 77.0 } else { 48.0 };
                let (c_coin, c_lap, c_time) = if compact { (inner.right(), inner.right() - 48.0, inner.right() - 48.0) } else { (inner.right(), inner.right() - 56.0, inner.right() - 56.0 - 84.0) };
                let head = |x: f32, a: Align2, s: &str| p.text(pos2(x, hy), a, s, theme::ui_bold(10.5), theme::TEXT_DIM);
                head(inner.left() + 4.0, Align2::LEFT_CENTER, "Place");
                head(inner.left() + 66.0, Align2::LEFT_CENTER, "Racer");
                head(c_time, Align2::RIGHT_CENTER, if compact { "Time / lap" } else { "Time" });
                if !compact { head(c_lap, Align2::RIGHT_CENTER, "Best lap"); }
                art::coin(&p, brand, pos2(c_coin - 8.0, hy), 13.0, None, 0.8);
                p.hline(inner.x_range(), hy + 11.0, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)));
                // rows
                let n = results.len().max(1);
                let top = hy + 16.0;
                let rh = ((inner.bottom() - top) / n as f32).min(34.0);
                let best_lap_all = results.iter().filter_map(|r| r.best_lap_s).fold(f32::MAX, f32::min);
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
                        rp.rect_stroke(row, egui::CornerRadius::same(10), Stroke::new(1.5_f32, rgba(0xF7, 0x93, 0x1A, 0.8)), egui::StrokeKind::Inside);
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
                    let nm = art::fit_size(&rp, &r.name, theme::ui_bold, 14.0, c_time - if compact { 72.0 } else { 92.0 } - (dx + 72.0));
                    rp.text(pos2(dx + 74.0, cy), Align2::LEFT_CENTER, &r.name, theme::ui_bold(nm), if r.is_local { Color32::WHITE } else { theme::TEXT });
                    let tt = r.time_s.map(fmt_time).unwrap_or_else(|| "DNF".to_string());
                    let is_new_t = r.is_local && new_time;
                    rp.text(pos2(c_time + 4.0, cy - if compact { 6.5 } else { 0.0 }), Align2::RIGHT_CENTER, &tt, theme::numbers(if compact { 13.0 } else { 15.0 }), if is_new_t { theme::GOLD } else if r.time_s.is_some() { theme::TEXT } else { theme::TEXT_DIM });
                    if is_new_t && !compact {
                        icons::star(&rp, pos2(c_time - 62.0, cy), 6.5, theme::GOLD, Stroke::new(1.0_f32, Color32::from_rgb(0xB8, 0x6A, 0x10)));
                    }
                    let lap = r.best_lap_s.map(fmt_time).unwrap_or_else(|| "--".to_string());
                    let is_new_l = r.is_local && new_lap;
                    let fastest = r.best_lap_s.map(|l| l <= best_lap_all + 1e-4).unwrap_or(false);
                    rp.text(pos2(c_lap + 2.0, cy + if compact { 8.0 } else { 0.0 }), Align2::RIGHT_CENTER, &lap, theme::numbers(if compact { 10.5 } else { 14.0 }), if is_new_l { theme::GOLD } else if fastest { theme::MINT } else { theme::TEXT_DIM });
                    if is_new_l && !compact {
                        icons::star(&rp, pos2(c_lap - 52.0, cy), 6.0, theme::GOLD, Stroke::new(1.0_f32, Color32::from_rgb(0xB8, 0x6A, 0x10)));
                    }
                    art::coin(&rp, brand, pos2(c_coin - 30.0, cy), 13.0, None, 0.95);
                    rp.text(pos2(c_coin, cy), Align2::RIGHT_CENTER, format!("{}", r.coins), theme::numbers(if compact { 13.0 } else { 15.0 }), theme::SAND);
                }
                // Time Trial ghost: what happened to it
                if let Some(g) = ghost_res {
                    let gy = top + n as f32 * rh + 10.0;
                    let box_r = Rect::from_min_size(pos2(inner.left(), gy), vec2(inner.width(), 44.0));
                    if box_r.bottom() < inner.bottom() + 4.0 {
                        let mut gp = p.clone();
                        gp.set_opacity(ease_out_cubic(phase(t, 0.9, 0.4)));
                        widgets::well(&gp, box_r, 12.0);
                        let blue = crate::game::ghost_hud::GHOST_BLUE;
                        gp.circle_filled(pos2(box_r.left() + 22.0, box_r.center().y), 8.0, blue);
                        let (l1, l2, col) = match (g.saved, g.raced) {
                            _ if g.failed => ("Could not save the ghost".to_string(), "Check the free space on this device".to_string(), Color32::from_rgb(255, 140, 120)),
                            (true, Some(r)) => ("Ghost beaten! Your run is the new ghost".to_string(), format!("Old ghost {}   You {}  ({})", fmt_time(r), fmt_time(g.time), crate::game::ghosts::fmt_split(g.time - r)), theme::MINT),
                            (true, None) => ("Ghost saved".to_string(), "Race it next time in Time Trial".to_string(), theme::MINT),
                            (false, Some(r)) => ("The ghost wins this one".to_string(), format!("Ghost {}   You {}  ({})", fmt_time(r), fmt_time(g.time), crate::game::ghosts::fmt_split(g.time - r)), theme::TEXT),
                            (false, None) => ("Your saved ghost is faster".to_string(), "Turn the ghost on to race it".to_string(), theme::TEXT),
                        };
                        gp.text(pos2(box_r.left() + 40.0, box_r.center().y - 8.0), Align2::LEFT_CENTER, l1, theme::ui_bold(12.5), col);
                        gp.text(pos2(box_r.left() + 40.0, box_r.center().y + 9.0), Align2::LEFT_CENTER, l2, theme::ui_medium(11.0), theme::TEXT_DIM);
                    }
                }
                // new-best badge
                if (new_time || new_lap) && t > 0.9 {
                    let k = ease_out_back(phase(t, 0.9, 0.4));
                    let c = pos2(inner.left() + if compact { 160.0 } else { 190.0 }, inner.top() + 17.0);
                    let mut bp = p.clone();
                    bp.set_opacity(k.clamp(0.0, 1.0));
                    let label = if compact { "NEW BEST" } else if new_time && new_lap { "NEW BEST TIME + LAP" } else if new_time { "NEW BEST TIME" } else { "NEW BEST LAP" };
                    widgets::pill(&bp, c + vec2(54.0, 0.0), label, theme::GOLD, theme::OUTLINE, 11.5 * k.max(0.5));
                }
            });

            // ── the side: podium (painted fallback) and buttons ──
            let e = widgets::enter(t, 1);
            if !over3d {
                let pod = Rect::from_min_max(pos2(side.left() + 6.0, side.top() + 14.0), pos2(side.right() - 6.0, side.bottom() - 74.0));
                let mut pp = p.clone();
                pp.set_opacity(e);
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
            let labels: Vec<(&str, &str, Btn)> = vec![("res-retry", "retry", Btn::new("Retry")), ("res-next", "next", Btn::primary("Next")), ("res-menu", "menu", Btn::new("Menu"))];
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
                            l.config.track = common::next_track(l.config.track);
                            Transition::Replace(crate::game::race_screen(l))
                        }
                        _ => Transition::PopTo("menu"),
                    };
                }
            }
            if let Some(po) = payout {
                po.draw_tally(&p, brand, Payout::tally_rect(side, by - S2), tm);
                po.draw_toasts(&p, brand, Rect::from_min_max(pos2(side.left(), safe.top()), safe.max));
            }
        });
        out
    }
}

// ── Grand Prix ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct GpRow {
    name: String,
    character: u8,
    is_local: bool,
    round_place: u8,
    gained: u32,
    prev: u32,
    total: u32,
    old_rank: usize,
    new_rank: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GpPhase {
    Standings,
    Trophy,
}

pub struct GpScreen {
    launch: RaceLaunch,
    rows: Vec<GpRow>,
    round: usize,
    rounds: usize,
    next: Option<RaceLaunch>,
    t: f32,
    phase_t: f32,
    phase: GpPhase,
    recorded: bool,
    /// This round's bests are in the profile.
    bests_done: bool,
    me: Option<RacerResult>,
    stage: StagePaint,
    played: Played,
    earn: Earn,
    payout: Option<Payout>,
}

/// The standings after this round of a Grand Prix: each racer's points and rank
/// before and after. The one place the points are worked out: the standings
/// screen shows them and the race screen decides the cup from them.
fn gp_rows(launch: &RaceLaunch, results: &[RacerResult]) -> Vec<GpRow> {
    let RaceKind::GrandPrix { points, .. } = &launch.kind else { return Vec::new() };
    let n = launch.config.racers.len();
    let mut taken = vec![false; n];
    let mut rows: Vec<GpRow> = (0..n)
        .map(|i| {
            let setup = &launch.config.racers[i];
            GpRow { name: setup.name.clone(), character: setup.character, is_local: i == launch.local_seat, round_place: n as u8, gained: 0, prev: points.get(i).copied().unwrap_or(0), total: 0, old_rank: 0, new_rank: 0 }
        })
        .collect();
    for r in results {
        let seat = common::seat_of(&launch.config, &r.name, r.character, &taken).or_else(|| (0..n).find(|i| !taken[*i]));
        if let Some(i) = seat {
            taken[i] = true;
            rows[i].round_place = r.place;
            rows[i].gained = GP_POINTS.get(r.place.saturating_sub(1) as usize).copied().unwrap_or(0) as u32;
        }
    }
    for r in rows.iter_mut() {
        r.total = r.prev + r.gained;
    }
    let mut old: Vec<usize> = (0..n).collect();
    old.sort_by(|a, b| rows[*b].prev.cmp(&rows[*a].prev).then(a.cmp(b)));
    let mut new: Vec<usize> = (0..n).collect();
    new.sort_by(|a, b| rows[*b].total.cmp(&rows[*a].total).then(rows[*a].round_place.cmp(&rows[*b].round_place)));
    for (rank, i) in old.iter().enumerate() {
        rows[*i].old_rank = rank;
    }
    for (rank, i) in new.iter().enumerate() {
        rows[*i].new_rank = rank;
    }
    rows
}

#[cfg(test)]
thread_local! {
    /// Tests: how many times a final round's cup result was decided (won or not).
    pub(crate) static CUP_DECISIONS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Decides, and saves, the cup result of a Grand Prix as its final race ends.
/// The placings are final from the moment the race is over, so a game killed
/// during the wait for the standings keeps the win count (`gp_wins`) and the cup
/// bit (`cups_won`). It still loses what only `Report::settle` writes on the
/// standings: the champion achievement, the prize and the race's coin payout.
/// `None` for every other race and for a mid-cup round. The standings screen is told (`RaceOutcome::gp_recorded`)
/// so it never writes the win a second time.
pub fn record_final_round(ctx: &mut Ctx, launch: &RaceLaunch, results: &[RacerResult]) -> Option<GpRecorded> {
    let RaceKind::GrandPrix { tracks, round, .. } = &launch.kind else { return None };
    if round + 1 < tracks.len() {
        return None;
    }
    let won = gp_rows(launch, results).iter().any(|r| r.is_local && r.new_rank == 0);
    #[cfg(test)]
    CUP_DECISIONS.with(|c| c.set(c.get() + 1));
    if won {
        ctx.profile.gp_wins += 1;
        // The cup bit is idempotent (`Report::settle` sets it again, harmlessly), and
        // saving it with the win means the two never disagree on disk.
        ctx.profile.progress.cups_won |= 1 << progress::cup_of(tracks).min(1);
        ctx.save_profile();
    }
    Some(GpRecorded { won })
}

impl GpScreen {
    pub fn new(out: RaceOutcome) -> GpScreen {
        let RaceKind::GrandPrix { tracks, round, .. } = out.launch.kind.clone() else { unreachable!("GpScreen needs a Grand Prix outcome") };
        let rows = gp_rows(&out.launch, &out.results);
        let totals: Vec<u32> = rows.iter().map(|r| r.total).collect();
        let rounds = tracks.len();
        let next = common::next_round(&out.launch, totals);
        let me = out.results.iter().find(|r| r.is_local).cloned();
        let (mut played, earn) = (Played::of_outcome(&out), Earn::of_outcome(&out, 0, 0));
        // The race screen already saved the cup result: carry it, do not count it again.
        let recorded = out.gp_recorded.is_some();
        if out.gp_recorded.is_some_and(|g| g.won) {
            played.cup_won = Some(progress::cup_of(&tracks));
        }
        GpScreen { played, earn, payout: None, me, bests_done: false, launch: out.launch, rows, round, rounds, next, t: 0.0, phase_t: 0.0, phase: GpPhase::Standings, recorded, stage: StagePaint::default() }
    }

    /// Standings after a mid-cup round (screenshots).
    pub fn sample(round: usize) -> GpScreen {
        GpScreen::new(sample_outcome(SampleKind::Gp(round)))
    }

    /// Straight to the trophy screen (screenshots).
    pub fn trophy_sample(win: bool) -> GpScreen {
        let mut g = GpScreen::new(sample_outcome(SampleKind::Gp(3)));
        if win {
            let n = g.rows.len();
            // make the local player the champion
            for r in g.rows.iter_mut() {
                r.total = if r.is_local { 300 } else { r.total.min(280) };
            }
            let mut idx: Vec<usize> = (0..n).collect();
            idx.sort_by(|a, b| g.rows[*b].total.cmp(&g.rows[*a].total));
            for (rank, i) in idx.iter().enumerate() {
                g.rows[*i].new_rank = rank;
            }
        }
        g.phase = GpPhase::Trophy;
        g.t = 3.0;
        g.recorded = true;
        g.bests_done = true;
        g
    }

    fn is_final(&self) -> bool {
        self.round + 1 >= self.rounds
    }

    fn sorted(&self) -> Vec<&GpRow> {
        let mut v: Vec<&GpRow> = self.rows.iter().collect();
        v.sort_by_key(|r| r.new_rank);
        v
    }

    fn local_rank(&self) -> usize {
        self.rows.iter().find(|r| r.is_local).map(|r| r.new_rank).unwrap_or(7)
    }
}

impl Screen for GpScreen {
    fn name(&self) -> &'static str {
        match self.phase {
            GpPhase::Standings => "gp_standings",
            GpPhase::Trophy => "gp_trophy",
        }
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        self.phase_t += ctx.dt;
        if !self.bests_done {
            self.bests_done = true;
            if let Some(me) = &self.me {
                common::record_bests(ctx.profile, self.launch.config.track, self.launch.config.laps, me);
                ctx.save_profile();
            }
        }
        if !self.recorded && self.is_final() {
            self.recorded = true;
            if self.local_rank() == 0 {
                ctx.profile.gp_wins += 1;
                ctx.save_profile();
                if let RaceKind::GrandPrix { tracks, .. } = &self.launch.kind {
                    self.played.cup_won = Some(progress::cup_of(tracks));
                }
            }
        }
        if self.payout.is_none() && self.bests_done && (self.recorded || !self.is_final()) {
            let report = Report::settle(ctx, &self.played, self.earn);
            self.payout = Some(Payout::new(report));
        }
        if let Some(po) = self.payout.as_mut() {
            po.update(ctx);
        }
        if self.is_final() {
            let s = self.sorted();
            let g = |i: usize| s.get(i).map(|r| r.character);
            crate::game::stage::show_podium([g(0), g(1), g(2)], self.launch.config.track);
            // standings table on the left, the trophy page's text on the right
            if common::Layout::new(ctx.viewport.safe).portrait {
                crate::game::stage::set_focus(0.5);
                crate::game::stage::set_portrait_focus(if self.phase == GpPhase::Trophy { 0.23 } else { 0.65 }, 3.2);
            } else {
                crate::game::stage::set_focus(if self.phase == GpPhase::Trophy { 0.27 } else { 0.79 });
            }
        }
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        if self.phase == GpPhase::Trophy {
            self.phase = GpPhase::Standings;
            self.phase_t = 0.0;
            self.t = 3.0;
            return Transition::None;
        }
        Transition::PopTo("menu")
    }

    fn paint_scene(&mut self, pass: &mut ScenePass) {
        self.stage.paint(pass);
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        let phase_now = self.phase;
        let phase_t = self.phase_t;
        let rows = self.rows.clone();
        let (round, rounds, is_final) = (self.round, self.rounds, self.is_final());
        let next = self.next.clone();
        let launch = self.launch.clone();
        let won = self.local_rank() == 0;
        let my_rank = self.local_rank();
        let wins = ctx.profile.gp_wins;
        let over3d = crate::game::stage::is_drawing();
        let mut to_trophy = false;
        let payout = self.payout.as_ref();
        common::page(ctx.viewport.screen, tm, ui, 0.10, |ui, brand| {
            let p = ui.painter().clone();
            let area = Rect::from_min_max(safe.min + vec2(S4, S3), safe.max - vec2(S4, S3));
            if phase_now == GpPhase::Standings {
                let gap = S3;
                let tw = (area.width() * 0.62).min(540.0);
                let (table, side) = common::Layout::new(safe).split(area, area.width() - tw - gap, 374.0, gap);
                widgets::staged(ui, widgets::enter(t, 0), vec2(-30.0, 0.0), |ui, off| {
                    let panel = table.translate(off);
                    let p = ui.painter().clone();
                    widgets::panel(&p, panel, 20.0);
                    let inner = panel.shrink2(vec2(14.0, 10.0));
                    widgets::heading(&p, pos2(inner.left(), inner.top() + 17.0), Align2::LEFT_CENTER, if is_final { "Final standings" } else { "Standings" }, 28.0, theme::TEXT);
                    let info = tracks::info(launch.config.track);
                    p.text(pos2(inner.right(), inner.top() + 9.0), Align2::RIGHT_CENTER, format!("Round {} of {}", round + 1, rounds), theme::ui_bold(12.5), theme::SAND);
                    p.text(pos2(inner.right(), inner.top() + 25.0), Align2::RIGHT_CENTER, &info.name, theme::ui_medium(11.0), theme::TEXT_DIM);
                    let hy = inner.top() + 48.0;
                    let (c_tot, c_gain, c_pl) = (inner.right(), inner.right() - 56.0, inner.right() - 56.0 - 62.0);
                    let head = |x: f32, a: Align2, s: &str| p.text(pos2(x, hy), a, s, theme::ui_bold(10.5), theme::TEXT_DIM);
                    head(inner.left() + 4.0, Align2::LEFT_CENTER, "Rank");
                    head(inner.left() + 66.0, Align2::LEFT_CENTER, "Racer");
                    head(c_pl, Align2::RIGHT_CENTER, "This round");
                    head(c_tot, Align2::RIGHT_CENTER, "Points");
                    p.hline(inner.x_range(), hy + 11.0, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.10)));
                    let n = rows.len().max(1);
                    let top = hy + 16.0;
                    let rh = ((inner.bottom() - top) / n as f32).min(34.0);
                    let slide = ease_out_cubic(phase(t, 1.25, 0.9));
                    let count = ease_out_cubic(phase(t, 0.7, 1.1));
                    // draw back to front so the rising rows overlap the ones they pass
                    let mut order: Vec<&GpRow> = rows.iter().collect();
                    order.sort_by(|a, b| (a.new_rank as f32).partial_cmp(&(b.new_rank as f32)).unwrap().reverse());
                    for r in order {
                        let re = ease_out_cubic(phase(t, 0.35 + 0.07 * r.old_rank as f32, 0.35));
                        if re <= 0.0 {
                            continue;
                        }
                        let rank_f = art::lerp(r.old_rank as f32, r.new_rank as f32, slide);
                        let y = top + rank_f * rh;
                        let moving = (r.old_rank != r.new_rank) as i32 as f32 * (slide * (1.0 - slide) * 4.0);
                        let row = Rect::from_min_size(pos2(inner.left() - 6.0 + (1.0 - re) * 36.0, y), vec2(inner.width() + 12.0, rh - 3.0)).expand(moving * 1.5);
                        let mut rp = p.clone();
                        rp.set_opacity(re);
                        if r.is_local {
                            rp.rect_filled(row, 10.0, rgba(0xF7, 0x93, 0x1A, 0.24));
                            rp.rect_stroke(row, egui::CornerRadius::same(10), Stroke::new(1.5_f32, rgba(0xF7, 0x93, 0x1A, 0.85)), egui::StrokeKind::Inside);
                        } else {
                            rp.rect_filled(row, 10.0, rgba(28, 12, 52, 0.55));
                        }
                        let cy = row.center().y;
                        let dx = row.left() + 6.0;
                        let shown_rank = if slide > 0.5 { r.new_rank } else { r.old_rank };
                        let pc = match shown_rank {
                            0 => theme::GOLD,
                            1 => Color32::from_rgb(0xD8, 0xD4, 0xE8),
                            2 => Color32::from_rgb(0xE2, 0x9A, 0x5E),
                            _ => theme::TEXT_DIM,
                        };
                        art::outlined(&rp, pos2(dx + 22.0, cy), Align2::CENTER_CENTER, &format!("{}", shown_rank + 1), theme::title(19.0), pc, theme::OUTLINE, 1.5, 0.0);
                        widgets::chip(&rp, pos2(dx + 58.0, cy), 8.5, from_f3(Character::get(r.character.min(7)).body));
                        let nm = art::fit_size(&rp, &r.name, theme::ui_bold, 14.0, c_pl - 80.0 - (dx + 72.0));
                        rp.text(pos2(dx + 74.0, cy), Align2::LEFT_CENTER, &r.name, theme::ui_bold(nm), if r.is_local { Color32::WHITE } else { theme::TEXT });
                        rp.text(pos2(c_pl + 4.0, cy), Align2::RIGHT_CENTER, ordinal(r.round_place), theme::ui_bold(13.0), theme::TEXT_DIM);
                        // points gained, popping in then folding into the total
                        let shown = (r.prev as f32 + (r.gained as f32) * count).round() as u32;
                        let pop = ease_out_back(phase(t, 0.55 + 0.05 * r.round_place as f32, 0.35));
                        if r.gained > 0 && pop > 0.0 && count < 0.98 {
                            let mut gp = rp.clone();
                            gp.set_opacity((1.0 - phase(count, 0.75, 0.25)).clamp(0.0, 1.0) * pop.clamp(0.0, 1.0));
                            gp.text(pos2(c_gain - (1.0 - pop) * 10.0, cy), Align2::RIGHT_CENTER, format!("+{}", r.gained), theme::numbers(15.0), theme::MINT);
                        }
                        rp.text(pos2(c_tot, cy), Align2::RIGHT_CENTER, format!("{shown}"), theme::numbers(17.0), if r.is_local { theme::GOLD } else { theme::TEXT });
                    }
                });
                // side: next track or the podium, and the buttons
                let bh = 50.0;
                if is_final {
                    if !over3d {
                        let pod = Rect::from_min_max(pos2(side.left() + 6.0, side.top() + 14.0), pos2(side.right() - 6.0, side.bottom() - 74.0));
                        let mut pp = p.clone();
                        pp.set_opacity(widgets::enter(t, 2));
                        let mut v: Vec<&GpRow> = rows.iter().collect();
                        v.sort_by_key(|r| r.new_rank);
                        let who = |i: usize| v.get(i).map(|r| (r.character, r.name.as_str()));
                        paint_podium(&pp, brand, pod, [who(0), who(1), who(2)], (t - 1.8).max(0.0), tm);
                    }
                } else if let Some(nx) = &next {
                    let info = tracks::info(nx.config.track);
                    let tally_h = if payout.is_some_and(|p| p.report.earn.total() > 0) { 66.0 + S2 } else { 0.0 };
                    let card = Rect::from_min_max(pos2(side.left(), side.top()), pos2(side.right(), side.bottom() - bh - S3 - tally_h));
                    let ecard = widgets::enter(t, 2);
                    widgets::staged(ui, ecard, vec2(40.0, 0.0), |ui, off| {
                        tracks::card(ui, card.translate(off), "gp-next", &info, false, None, Some(round + 2), false, (t - 0.6) / 0.7);
                    });
                }
                if won && is_final {
                    widgets::confetti(&p, brand, Rect::from_min_max(side.min, pos2(side.right(), safe.bottom())), tm, 22);
                }
                let by = side.bottom() - bh;
                let eb = widgets::enter(t, 3);
                let (label, sub): (&str, Option<String>) = if is_final { ("Trophy", None) } else { ("Next race", next.as_ref().map(|n| tracks::info(n.config.track).name.clone())) };
                let mut btn = Btn::primary(label).size(21.0);
                if let Some(s) = sub.as_deref() {
                    btn = btn.sub(s);
                }
                let wmenu = 78.0;
                let go = Rect::from_min_size(pos2(side.left(), by), vec2(side.width() - wmenu - S2, bh));
                let mn = Rect::from_min_size(pos2(side.right() - wmenu, by), vec2(wmenu, bh));
                let (go_c, mn_c) = widgets::staged(ui, eb, vec2(0.0, 30.0), |ui, off| (widgets::button(ui, go.translate(off), "gp-go", &btn).clicked(), widgets::button(ui, mn.translate(off), "gp-menu", &Btn::new("Menu")).clicked()));
                if go_c {
                    feedback::select(ctx);
                    if is_final {
                        to_trophy = true;
                    } else if let Some(nx) = &next {
                        out = Transition::Replace(crate::game::race_screen(nx.clone()));
                    }
                }
                if mn_c {
                    feedback::tap(ctx);
                    out = Transition::PopTo("menu");
                }
                if let Some(po) = payout {
                    po.draw_tally(&p, brand, Payout::tally_rect(side, by - S2), tm);
                }
            } else {
                trophy_page(ui, ctx, brand, area, safe, &rows, won, my_rank, wins, phase_t, tm, over3d, &mut out);
            }
            if let Some(po) = payout {
                let w = if common::Layout::new(safe).portrait { area.width() } else { (area.width() * 0.34).min(300.0) };
                po.draw_toasts(&p, brand, Rect::from_min_max(pos2(area.right() - w, safe.top()), pos2(area.right(), safe.bottom())));
            }
        });
        if to_trophy {
            self.phase = GpPhase::Trophy;
            self.phase_t = 0.0;
        }
        out
    }
}

/// The big finish: a trophy, your final place and the top three.
#[allow(clippy::too_many_arguments)]
fn trophy_page(ui: &mut egui::Ui, ctx: &mut Ctx, brand: &art::Brand, area: Rect, safe: Rect, rows: &[GpRow], won: bool, my_rank: usize, wins: u32, t: f32, tm: f32, over3d: bool, out: &mut Transition) {
    let p = ui.painter().clone();
    let e = ease_out_cubic(phase(t, 0.0, 0.5));
    let portrait = common::Layout::new(safe).portrait;
    let c = if portrait { pos2(area.center().x, area.top() + 180.0) } else { pos2(area.left() + area.width() * 0.30, area.center().y - 8.0) };
    if !over3d {
        // sun-ray fan behind the cup
        let rays = 14;
        for i in 0..rays {
            let a = tm * 0.18 + i as f32 / rays as f32 * std::f32::consts::TAU;
            let (r0, r1, w) = (30.0, 210.0 * e, 0.11);
            let pts = vec![c + vec2(a.cos(), a.sin()) * r0, c + vec2((a - w).cos(), (a - w).sin()) * r1, c + vec2((a + w).cos(), (a + w).sin()) * r1];
            p.add(egui::Shape::convex_polygon(pts, rgba(0xFF, 0xC8, 0x3D, if won { 0.16 } else { 0.07 }), Stroke::NONE));
        }
        art::glow(&p, c, 190.0 * e, rgba(0xFF, 0xB0, 0x30, if won { 0.30 } else { 0.14 }));
        let k = ease_out_back(phase(t, 0.15, 0.6));
        let bob = (tm * 1.6).sin() * 3.0;
        let cs = 190.0 * k.max(0.0);
        art::glow_ellipse(&p, c + vec2(0.0, 0.40 * cs + 4.0), cs * 0.45, cs * 0.06, rgba(0, 0, 0, 0.35));
        art::trophy(&p, c + vec2(0.0, bob), cs, if won { theme::GOLD } else { Color32::from_rgb(0xC8, 0xC4, 0xD8) });
        // sparkles
        for i in 0..8u32 {
            let ph = (tm * 0.9 + art::hash01(i * 3) * 4.0) % 2.0;
            if ph < 0.6 && won {
                let q = c + vec2((art::hash01(i * 5) - 0.5) * 140.0, (art::hash01(i * 7) - 0.5) * 140.0 + bob);
                let s = (ph / 0.6 * std::f32::consts::PI).sin();
                icons::star(&p, q, 7.0 * s, Color32::WHITE, Stroke::NONE);
            }
        }
    }
    if won {
        widgets::confetti(&p, brand, Rect::from_min_max(safe.min, safe.max), tm, 36);
    }
    // text and the top three
    let area = if portrait { Rect::from_min_max(pos2(area.left(), area.top() + 370.0), area.max) } else { area };
    let tx = area.left() + if portrait { 0.0 } else { area.width() * 0.60 };
    let tw = area.right() - tx;
    let head = if won { "Champion!".to_string() } else { format!("{} overall", ordinal(my_rank as u8 + 1)) };
    let es = widgets::enter(t, 1);
    widgets::staged(ui, es, vec2(40.0, 0.0), |ui, off| {
        let p = ui.painter().clone();
        let o = off;
        widgets::heading(&p, pos2(tx, area.top() + 36.0) + o, Align2::LEFT_CENTER, &head, 40.0, if won { theme::GOLD } else { theme::TEXT });
        p.text(pos2(tx, area.top() + 68.0) + o, Align2::LEFT_CENTER, if won { "You took the Grand Prix." } else { "Grand Prix complete." }, theme::ui_bold(13.5), theme::SAND);
        let mut v: Vec<&GpRow> = rows.iter().collect();
        v.sort_by_key(|r| r.new_rank);
        let panel = Rect::from_min_size(pos2(tx, area.top() + 86.0) + o, vec2(tw, 108.0));
        widgets::panel(&p, panel, 16.0);
        for (i, r) in v.iter().take(3).enumerate() {
            let y = panel.top() + 20.0 + i as f32 * 32.0;
            let col = [theme::GOLD, Color32::from_rgb(0xD8, 0xD4, 0xE8), Color32::from_rgb(0xE2, 0x9A, 0x5E)][i];
            art::outlined(&p, pos2(panel.left() + 22.0, y), Align2::CENTER_CENTER, &format!("{}", i + 1), theme::title(19.0), col, theme::OUTLINE, 1.5, 0.0);
            widgets::chip(&p, pos2(panel.left() + 52.0, y), 8.5, from_f3(Character::get(r.character.min(7)).body));
            p.text(pos2(panel.left() + 70.0, y), Align2::LEFT_CENTER, &r.name, theme::ui_bold(14.0), if r.is_local { Color32::WHITE } else { theme::TEXT });
            p.text(pos2(panel.right() - 14.0, y), Align2::RIGHT_CENTER, format!("{} pts", r.total), theme::numbers(15.0), theme::SAND);
        }
        p.text(pos2(tx, panel.bottom() + 18.0), Align2::LEFT_CENTER, format!("Cups won: {wins}"), theme::ui_bold(13.0), theme::GOLD);
        let bw = (tw - S2) / 2.0;
        let by = area.bottom() - 50.0;
        let (a, b) = (Rect::from_min_size(pos2(tx, by), vec2(bw, 50.0)).translate(o), Rect::from_min_size(pos2(tx + bw + S2, by), vec2(bw, 50.0)).translate(o));
        if widgets::button(ui, a, "tr-menu", &Btn::new("Menu")).clicked() {
            feedback::tap(ctx);
            *out = Transition::PopTo("menu");
        }
        if widgets::button(ui, b, "tr-again", &Btn::primary("New cup")).clicked() {
            feedback::select(ctx);
            // back to the Grand Prix setup this cup was started from
            *out = Transition::Pop;
        }
    });
}

// ── staged outcomes for screenshots and tests ───────────────────────────────

pub enum SampleKind {
    /// Quick Race, finished 2nd, with new bests.
    Quick,
    /// Quick Race, won.
    QuickWin,
    /// Grand Prix round `n` (0-based).
    Gp(usize),
}

/// A fake finished race (never runs a race; for `--shot` and layout tests).
/// A finished Time Trial that beat its ghost (screenshots).
pub fn ghost_sample_outcome() -> RaceOutcome {
    let mut o = sample_outcome(SampleKind::Quick);
    o.launch.kind = RaceKind::TimeTrial;
    o.launch.config.racers.truncate(1);
    o.results.retain(|r| r.is_local);
    if let Some(r) = o.results.first_mut() {
        r.place = 1;
        r.time_s = Some(84.37);
        r.best_lap_s = Some(27.9);
    }
    o.ghost = Some(crate::flow::GhostResult { time: 84.37, raced: Some(85.12), saved: true, failed: false });
    o
}

pub fn sample_outcome(kind: SampleKind) -> RaceOutcome {
    let profile = crate::platform::data_dir::Profile { name: "Rider".into(), ..Default::default() };
    let (track, local_place, gp) = match kind {
        SampleKind::Quick => (TrackId::Dune, 2u8, None),
        SampleKind::QuickWin => (TrackId::Savanna, 1u8, None),
        SampleKind::Gp(r) => (TrackId::RACE[r.min(3)], 3u8, Some(r.min(3))),
    };
    let mut launch = match gp {
        Some(r) => {
            let mut l = common::grand_prix_launch(&profile, common::Cup::One, 3, race_core::BotLevel::Normal);
            l.config.track = TrackId::RACE[r];
            // a mid-cup points table
            let pts: Vec<u32> = (0..8u32).map(|i| match r { 0 => 0, 1 => [15, 12, 10, 8, 6, 4, 2, 1][((i + 2) % 8) as usize], _ => [38, 34, 29, 24, 20, 14, 9, 5][((i + 3) % 8) as usize] }).collect();
            l.kind = RaceKind::GrandPrix { tracks: TrackId::RACE.to_vec(), round: r, points: pts };
            l
        }
        None => common::quick_launch(&profile, track, 3, 7, race_core::BotLevel::Normal),
    };
    launch.local_seat = 0;
    let n = launch.config.racers.len();
    let mut order: Vec<usize> = (1..n).collect();
    order.insert((local_place as usize - 1).min(order.len()), 0);
    let results = order
        .iter()
        .enumerate()
        .map(|(pos, &seat)| {
            let s = &launch.config.racers[seat];
            let base = 96.0 + pos as f32 * 2.3 + (seat as f32 * 0.37) % 1.1;
            RacerResult {
                name: s.name.clone(),
                character: s.character,
                place: pos as u8 + 1,
                time_s: if pos == 7 { None } else { Some(base) },
                best_lap_s: if pos == 7 { None } else { Some(base / 3.0 - 0.4 + (seat as f32 * 0.21) % 0.8) },
                coins: ((12 - pos as i32 * 2 + seat as i32 % 3).max(0)) as u32,
                is_local: seat == 0,
                is_bot: seat != 0,
                battle: None,
            }
        })
        .collect();
    RaceOutcome { launch, results, lesson: None, challenge: None, quit: false, ghost: None, battle: None, stats: Default::default(), gp_recorded: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grand_prix_points_follow_the_places() {
        let g = GpScreen::sample(1);
        assert_eq!(g.rows.len(), 8);
        assert_eq!(g.rows.iter().map(|r| r.gained).sum::<u32>(), GP_POINTS.iter().map(|p| *p as u32).sum::<u32>(), "every place scores exactly once");
        for r in &g.rows {
            assert_eq!(r.total, r.prev + r.gained);
            assert_eq!(r.gained, GP_POINTS[r.round_place as usize - 1] as u32);
        }
        // ranks are a permutation, best total first
        let mut ranks: Vec<usize> = g.rows.iter().map(|r| r.new_rank).collect();
        ranks.sort();
        assert_eq!(ranks, (0..8).collect::<Vec<_>>());
        let s = g.sorted();
        assert!(s.windows(2).all(|w| w[0].total >= w[1].total));
        // the next round carries the totals and the next track
        let next = g.next.as_ref().expect("round 2 of 4 has a next round");
        let RaceKind::GrandPrix { round, points, .. } = &next.kind else { panic!("not a GP launch") };
        assert_eq!(*round, 2);
        assert_eq!(points, &g.rows.iter().map(|r| r.total).collect::<Vec<_>>());
        assert_eq!(next.config.track, TrackId::RACE[2]);
        // the last round has none and is the final
        let last = GpScreen::sample(3);
        assert!(last.next.is_none() && last.is_final());
    }

    #[test]
    fn results_record_bests_once() {
        let r = ResultsScreen::new(sample_outcome(SampleKind::Quick));
        let me = r.local().expect("a local racer");
        assert_eq!(me.place, 2);
        assert!(r.out.results.windows(2).all(|w| w[0].place < w[1].place));
        assert_eq!(top3(&r.out.results), [Some(r.out.results[0].character), Some(r.out.results[1].character), Some(r.out.results[2].character)]);
        assert_eq!(top3(&r.out.results[..1]), [Some(r.out.results[0].character), None, None]);
    }

    #[test]
    fn launches_give_every_racer_its_own_ostrich() {
        let profile = crate::platform::data_dir::Profile { character: 3, name: "Rider".into(), ..Default::default() };
        let l = common::quick_launch(&profile, TrackId::Dune, 2, 7, race_core::BotLevel::Hard);
        let mut ids: Vec<u8> = l.config.racers.iter().map(|r| r.character).collect();
        assert_eq!(l.config.racers[0].character, 3);
        assert_eq!(l.config.racers[0].name, "Rider");
        assert_eq!(l.config.laps, 2);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 8);
        let tt = common::time_trial_launch(&profile, TrackId::Savanna, 3);
        assert_eq!(tt.config.racers.len(), 1);
        assert_eq!(tt.config.mode, race_core::RaceMode::TimeTrial);
    }
}
