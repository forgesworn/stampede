//! Training: the five open-ground drills on the Paddock (best level, stars, the
//! Master medal) and the six track lessons, which can be run on the Oval or on
//! the Figure Eight. Lessons stay locked until the previous one has a star.
//! After a lesson the race screen hands over to [`TrainingScreen::returning`],
//! which records the stars and shows what you earned; a drill saves itself in
//! the race screen (its own results card) and this list just reads the profile.

use super::common;
use super::{Ctx, Screen, Transition};
use crate::flow::{LessonResult, RaceKind, RaceLaunch};
use crate::ui::art::{self, ease_out_back, fade, phase, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Seg, S2, S3};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind};
use race_core::{DrillId, HintControl, LessonId, RaceConfig, RacerKind, TrackId};

thread_local! {
    /// A finished lesson's result, left by [`hand_back`] for the list screen under the race.
    static PENDING: std::cell::RefCell<Option<LessonResult>> = const { std::cell::RefCell::new(None) };
}

/// The lesson race is over: leave its result for the training list, which is
/// still on the stack under the race (the race then pops back to it).
pub fn hand_back(result: LessonResult) {
    PENDING.with(|p| *p.borrow_mut() = Some(result));
}

/// Where the lessons run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Venue {
    Oval,
    Eight,
}

impl Venue {
    pub const ALL: [Venue; 2] = [Venue::Oval, Venue::Eight];

    pub fn track(self) -> TrackId {
        match self {
            Venue::Oval => TrackId::Training,
            Venue::Eight => TrackId::Eight,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Venue::Oval => "Oval",
            Venue::Eight => "Figure Eight",
        }
    }
}

pub struct TrainingScreen {
    t: f32,
    /// A lesson just finished: its result, until it is recorded.
    result: Option<LessonResult>,
    /// The recorded result, shown as a banner (lesson, stars, done, newly unlocked, seconds since).
    banner: Option<Banner>,
    returning: bool,
    /// Screenshots: stars to put into the profile first.
    demo_stars: Option<[u8; 6]>,
    /// Screenshots: (best level, stars, master) per drill to put into the profile first.
    demo_drills: Option<([u8; 5], [u8; 5], [bool; 5])>,
    /// The ₿ earned for the lesson (new stars pay once).
    payout: Option<crate::ui::progress_ui::Payout>,
    /// Where the lessons run.
    venue: Venue,
}

struct Banner {
    lesson: LessonId,
    stars: u8,
    done: bool,
    unlocked: Option<LessonId>,
    age: f32,
}

impl Default for TrainingScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl TrainingScreen {
    pub fn new() -> TrainingScreen {
        TrainingScreen { t: 0.0, result: None, banner: None, returning: false, demo_stars: None, demo_drills: None, payout: None, venue: Venue::Oval }
    }

    /// Three lessons done, the fourth open, two drills played (screenshots).
    pub fn sample() -> TrainingScreen {
        TrainingScreen { demo_stars: Some([3, 2, 1, 0, 0, 0]), demo_drills: Some(([7, 4, 10, 2, 0], [2, 1, 3, 0, 0], [false, false, true, false, false])), ..TrainingScreen::new() }
    }

    /// A fresh profile: nothing done, Slalom is the first suggestion, the Figure Eight picked (screenshots).
    pub fn fresh_sample() -> TrainingScreen {
        TrainingScreen { demo_stars: Some([0; 6]), demo_drills: Some(([0; 5], [0; 5], [false; 5])), venue: Venue::Eight, ..TrainingScreen::new() }
    }

    /// Back from lesson 3 with two stars (screenshots).
    pub fn result_sample() -> TrainingScreen {
        TrainingScreen { demo_stars: Some([3, 2, 0, 0, 0, 0]), ..TrainingScreen::returning(LessonResult { lesson: LessonId::Jump, done: true, stars: 2, time_s: 31.0 }) }
    }

    /// The list after a lesson: records `result` and celebrates it.
    pub fn returning(result: LessonResult) -> TrainingScreen {
        TrainingScreen { t: 0.0, result: Some(result), banner: None, returning: true, demo_stars: None, demo_drills: None, payout: None, venue: Venue::Oval }
    }
}

fn icon_for(c: HintControl) -> Icon {
    match c {
        HintControl::Steer => Icon::Steer,
        HintControl::Drift => Icon::Drift,
        HintControl::Jump => Icon::Jump,
        HintControl::Laser => Icon::Laser,
        HintControl::Bash => Icon::Bash,
        HintControl::Item => Icon::Item,
    }
}

/// What a drill teaches, for its card.
pub fn teaches(d: DrillId) -> &'static str {
    match d {
        DrillId::Slalom => "Steering",
        DrillId::DriftRing => "Drift & boost",
        DrillId::Hurdles => "Jumping",
        DrillId::LaserRange => "Laser eyes",
        DrillId::BashBuddy => "Head-bash",
    }
}

fn lesson_number(lesson: LessonId) -> u8 {
    lesson.number() - u8::from(lesson == LessonId::CoinsItems)
}

/// Is lesson `i` open? The first always; the rest once the one before has a star.
pub fn unlocked(stars: &[u8; 6], i: usize) -> bool {
    i == 0 || stars[if i == 5 { 3 } else { i - 1 }] >= 1
}

/// A lesson launch for `lesson` on the Oval.
pub fn launch_for(profile: &crate::platform::data_dir::Profile, lesson: LessonId) -> RaceLaunch {
    launch_for_on(profile, lesson, TrackId::Training)
}

/// A lesson launch on `venue` (the Oval, `TrackId::Training`, or the Figure Eight, `TrackId::Eight`).
pub fn launch_for_on(profile: &crate::platform::data_dir::Profile, lesson: LessonId, venue: TrackId) -> RaceLaunch {
    let mut config = RaceConfig::training(lesson, RacerKind::Human, common::fresh_seed());
    config.track = venue;
    common::seat_player(&mut config, profile);
    RaceLaunch { config, kind: RaceKind::Training(lesson), local_seat: 0 }
}

/// A drill launch on the Paddock with a fresh seed.
pub fn drill_launch(profile: &crate::platform::data_dir::Profile, id: DrillId) -> RaceLaunch {
    let mut config = RaceConfig::drill(id, TrackId::Field, common::fresh_seed());
    common::seat_player(&mut config, profile);
    if let Some(buddy) = config.racers.get_mut(1) {
        buddy.name = "Buddy".into();
    }
    RaceLaunch { config, kind: RaceKind::Drill(id), local_seat: 0 }
}

impl Screen for TrainingScreen {
    fn name(&self) -> &'static str {
        "training"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if let Some(s) = self.demo_stars.take() {
            ctx.profile.lesson_stars = s;
        }
        if let Some((best, stars, master)) = self.demo_drills.take() {
            ctx.profile.drill_best = best;
            ctx.profile.drill_stars = stars;
            ctx.profile.drill_master = master;
        }
        if let Some(r) = PENDING.with(|p| p.borrow_mut().take()) {
            self.result = Some(r);
        }
        if let Some(r) = self.result.take() {
            let i = LessonId::ALL.iter().position(|l| *l == r.lesson).unwrap_or(0);
            let next = (i + 1..6).find(|&j| j != 4);
            let was_open_next = next.map(|j| unlocked(&ctx.profile.lesson_stars, j)).unwrap_or(true);
            let stars = if r.done { r.stars.clamp(1, 3) } else { 0 };
            let prev_stars = ctx.profile.lesson_stars[i];
            if stars > ctx.profile.lesson_stars[i] {
                ctx.profile.lesson_stars[i] = stars;
            }
            ctx.save_profile();
            let unlocked_now = if r.done && !was_open_next { next.map(|j| LessonId::ALL[j]) } else { None };
            self.banner = Some(Banner { lesson: r.lesson, stars, done: r.done, unlocked: unlocked_now, age: 0.0 });
            use crate::ui::progress::{self, Earn, Kind, Played};
            let earn = Earn { bonus: progress::STAR_PAY * stars.saturating_sub(prev_stars) as u32, bonus_kind: 1, ..Earn::default() };
            let played = Played { kind: Kind::Training, track: None, place: 0, racers: 0, finished: r.done, coins: 0, stats: progress::take_stats(), cup_won: None };
            let report = crate::ui::progress_ui::Report::settle(ctx, &played, earn);
            self.payout = Some(crate::ui::progress_ui::Payout::new(report));
        }
        if let Some(po) = self.payout.as_mut() {
            po.update(ctx);
        }
        if let Some(b) = self.banner.as_mut() {
            b.age += ctx.dt;
        }
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        if self.returning {
            Transition::PopTo("menu")
        } else {
            Transition::Pop
        }
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        let stars = ctx.profile.lesson_stars;
        let (d_best, d_stars, d_master) = (ctx.profile.drill_best, ctx.profile.drill_stars, ctx.profile.drill_master);
        let banner = self.banner.as_ref().map(|b| (b.lesson, b.stars, b.done, b.unlocked, b.age));
        let returning = self.returning;
        let payout = self.payout.as_ref();
        let mut venue = self.venue;
        common::page(ctx.viewport.screen, tm, ui, 0.40, |ui, brand| {
            let hd = widgets::header(ui, safe, "Training", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = if returning { Transition::PopTo("menu") } else { Transition::Pop };
            }
            let p = ui.painter().clone();
            // progress, top right
            let done = stars.iter().enumerate().filter(|(i, s)| *i != 4 && **s >= 1).count();
            let total_stars: u32 = stars.iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, s)| *s as u32).chain(d_stars[..4].iter().map(|s| *s as u32)).sum();
            let max_stars = 27;
            let portrait = common::Layout::new(safe).portrait;
            let pc = pos2(safe.right() - 16.0 - 109.0, if portrait { hd.body.top() + 18.0 } else { safe.top() + 34.0 });
            let e0 = widgets::enter(t, 1);
            {
                let mut pp = p.clone();
                pp.set_opacity(e0);
                let r = Rect::from_center_size(pc, vec2(218.0, 34.0));
                widgets::well(&pp, r, 17.0);
                pp.text(pos2(r.left() + 14.0, r.center().y), Align2::LEFT_CENTER, format!("{done} of 5 lessons"), theme::ui_bold(13.0), theme::TEXT);
                icons::star(&pp, pos2(r.right() - 64.0, r.center().y), 8.5, theme::GOLD, Stroke::new(1.0_f32, Color32::from_rgb(0xB8, 0x6A, 0x10)));
                pp.text(pos2(r.right() - 50.0, r.center().y), Align2::LEFT_CENTER, format!("{total_stars}/{max_stars}"), theme::ui_bold(13.0), theme::SAND);
            }

            let mut body = hd.body;
            if portrait { body.min.y += 46.0; }
            common::scroll(ui, body, "training", if portrait { 990.0 } else { body.height() }, |ui, body| {
            let p = ui.painter().clone();
            let gap = S3;
            let label_h = 26.0;
            let avail = body.height() - 2.0 * label_h - 2.0 * S2;
            let drills_h = if portrait { 180.0 } else { (avail * 0.60).clamp(112.0, 176.0).min(avail - 84.0) };
            let lessons_h = if portrait { 160.0 } else { (avail - drills_h).max(72.0) };

            // ── section 1: the drills ──
            let y0 = body.top();
            let e = widgets::enter(t, 1);
            {
                let mut pp = p.clone();
                pp.set_opacity(e);
                section_label(&pp, pos2(body.left() + 2.0, y0 + label_h / 2.0), "DRILLS ON THE PADDOCK", if portrait { "" } else { "Open ground. Four clean reps level you up; three strikes end the run." });
            }
            let drill_cols = if portrait { 2 } else { 4 };
            let dw = (body.width() - gap * (drill_cols - 1) as f32) / drill_cols as f32;
            let next_drill = (0..4).find(|&i| d_best[i] == 0);
            for (i, id) in DrillId::ALL.iter().enumerate().filter(|(_, id)| **id != DrillId::BashBuddy) {
                let rect = Rect::from_min_size(pos2(body.left() + (i % drill_cols) as f32 * (dw + gap), y0 + label_h + S2 * 0.5 + (i / drill_cols) as f32 * (drills_h + gap)), vec2(dw, drills_h));
                let e = widgets::enter(t, i + 1);
                let clicked = widgets::staged(ui, e, vec2(0.0, 36.0), |ui, off| drill_card(ui, rect.translate(off), i, *id, d_best[i], d_stars[i], d_master[i], next_drill == Some(i), tm));
                if clicked {
                    feedback::select(ctx);
                    out = Transition::Push(crate::game::race_screen(drill_launch(ctx.profile, *id)));
                }
            }

            // ── section 2: the lessons, on the Oval or the Figure Eight ──
            let y1 = y0 + label_h + S2 * 0.5 + if portrait { 2.0 * drills_h + gap } else { drills_h } + S2;
            let e = widgets::enter(t, 3);
            {
                let mut pp = p.clone();
                pp.set_opacity(e);
                section_label(&pp, pos2(body.left() + 2.0, y1 + label_h / 2.0), "TRACK LESSONS", if portrait { "" } else { "Pick where to run them." });
            }
            {
                let sw = Rect::from_min_size(pos2(if portrait { body.left() } else { body.right() - 236.0 }, y1 + if portrait { label_h + 3.0 } else { (label_h - 28.0) / 2.0 - 1.0 }), vec2(236.0, 28.0));
                let segs: Vec<Seg> = Venue::ALL.iter().map(|v| Seg::new(v.name())).collect();
                let picked = widgets::staged(ui, e, vec2(0.0, -12.0), |ui, off| widgets::segmented(ui, sw.translate(off), "training-venue", &segs, Venue::ALL.iter().position(|v| *v == venue).unwrap_or(0)));
                if let Some(i) = picked {
                    feedback::tap(ctx);
                    venue = Venue::ALL[i];
                }
            }
            let lesson_cols = if portrait { 2 } else { 5 };
            let cw = (body.width() - gap * (lesson_cols - 1) as f32) / lesson_cols as f32;
            let next_up = (0..6).filter(|&i| i != 4).find(|&i| unlocked(&stars, i) && stars[i] == 0);
            for (col, (i, lesson)) in LessonId::ALL.iter().enumerate().filter(|(_, id)| **id != LessonId::HeadBash).enumerate() {
                let rect = Rect::from_min_size(pos2(body.left() + (col % lesson_cols) as f32 * (cw + gap), y1 + label_h + S2 * 0.5 + if portrait { 36.0 + (col / lesson_cols) as f32 * (lessons_h + gap) } else { 0.0 }), vec2(cw, lessons_h));
                let e = widgets::enter(t, i + 3);
                let open = unlocked(&stars, i);
                let just = banner.filter(|b| b.0 == *lesson).map(|b| b.4);
                let fresh_unlock = banner.and_then(|b| b.3).filter(|l| l == lesson).map(|_| banner.map(|b| b.4).unwrap_or(0.0));
                let clicked = widgets::staged(ui, e, vec2(0.0, 36.0), |ui, off| lesson_card(ui, rect.translate(off), i, *lesson, stars[i], open, next_up == Some(i), just, fresh_unlock, tm));
                if clicked && open {
                    feedback::select(ctx);
                    out = Transition::Push(crate::game::race_screen(launch_for_on(ctx.profile, *lesson, venue.track())));
                } else if clicked {
                    feedback::back(ctx);
                    ctx.say(format!("Finish lesson {} first", lesson_number(*lesson) - 1));
                }
            }

            });

            // the result banner
            if let Some((lesson, st, done, unlocked_l, age)) = banner {
                let k = ease_out_back(phase(age, 0.15, 0.45)) * (1.0 - phase(age, 4.2, 0.4));
                if k > 0.0 {
                    let w = 330.0;
                    let r = Rect::from_center_size(pos2(safe.center().x, safe.top() + 30.0 - (1.0 - k.min(1.0)) * 80.0), vec2(w, 52.0));
                    let mut bp = p.clone();
                    bp.set_opacity(k.min(1.0));
                    widgets::panel(&bp, r, 26.0);
                    let msg = if done { format!("Lesson {} done!", lesson_number(lesson)) } else { "Not quite. Try again!".to_string() };
                    widgets::heading(&bp, pos2(r.left() + 20.0, r.center().y - 7.0), Align2::LEFT_CENTER, &msg, 19.0, if done { theme::GOLD } else { theme::TEXT });
                    if let Some(u) = unlocked_l {
                        bp.text(pos2(r.left() + 20.0, r.center().y + 14.0), Align2::LEFT_CENTER, format!("Lesson {} unlocked: {}", lesson_number(u), u.title()), theme::ui_bold(11.5), theme::SAND);
                    } else if !done {
                        bp.text(pos2(r.left() + 20.0, r.center().y + 14.0), Align2::LEFT_CENTER, "Retake it whenever you like", theme::ui_bold(11.5), theme::TEXT_DIM);
                    }
                    if done {
                        widgets::stars(&bp, pos2(r.right() - 52.0, r.center().y), 3, st, 22.0, (age - 0.5).max(0.0));
                    }
                    if let Some(po) = payout {
                        po.draw_chip(&bp, brand, if portrait { pos2(safe.left() + 20.0, pc.y) } else { pos2(r.right() + 10.0, r.center().y) }, 34.0, tm);
                    }
                }
            }
            if let Some(po) = payout {
                // below the banner, over the cards, for a moment
                po.draw_toasts(&p, brand, Rect::from_min_max(pos2(safe.left(), safe.top() + 58.0), safe.max));
            }
        });
        self.venue = venue;
        out
    }
}

/// "DRILLS ON THE PADDOCK" with a dim note after it.
fn section_label(p: &egui::Painter, left_centre: egui::Pos2, title: &str, note: &str) {
    let r = p.text(left_centre, Align2::LEFT_CENTER, title, theme::ui_bold(12.5), theme::SAND);
    p.text(pos2(r.right() + 12.0, left_centre.y), Align2::LEFT_CENTER, note, theme::ui_medium(11.0), theme::TEXT_DIM);
}

/// `text` wrapped to `width`, cut with an ellipsis after `rows` lines.
fn clipped(p: &egui::Painter, text: &str, size: f32, col: Color32, width: f32, rows: usize) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), theme::ui_medium(size), col, width);
    job.wrap.max_rows = rows;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('\u{2026}');
    p.layout_job(job)
}

#[allow(clippy::too_many_arguments)]
fn drill_card(ui: &mut egui::Ui, rect: Rect, i: usize, drill: DrillId, best: u8, stars: u8, master: bool, next_up: bool, tm: f32) -> bool {
    let id = Id::new(("stampede-drill", i));
    let resp = ui.interact(rect, id, Sense::click());
    let (scale, _) = widgets::press_fx(ui, id, &resp);
    let rect = Rect::from_center_size(rect.center(), rect.size() * scale);
    let p = ui.painter().clone();
    let r = 18.0;
    widgets::soft_shadow(&p, rect, r, 5.0, 14.0, 0.5);
    let played = best > 0;
    let (top, bot) = if master { (Color32::from_rgb(0x6A, 0x3E, 0x78), Color32::from_rgb(0x38, 0x16, 0x4C)) } else if played { (Color32::from_rgb(0x4B, 0x2A, 0x78), Color32::from_rgb(0x2A, 0x12, 0x4C)) } else { (Color32::from_rgb(0x5A, 0x2C, 0x86), Color32::from_rgb(0x30, 0x14, 0x56)) };
    widgets::rrect_gradient(&p, rect, r, fade(top, 0.94), fade(bot, 0.94));
    p.hline((rect.left() + r * 0.8)..=(rect.right() - r * 0.8), rect.top() + 1.5, Stroke::new(1.2_f32, rgba(255, 255, 255, 0.18)));
    let rim = if next_up { theme::mix(theme::ACCENT, theme::GOLD, 0.5 + 0.5 * (tm * 3.0).sin()) } else if master { theme::GOLD } else if played { rgba(0x6B, 0xE0, 0xA8, 0.55) } else { theme::OUTLINE };
    p.rect_stroke(rect, CornerRadius::same(r as u8), Stroke::new(if next_up || master { 3.0_f32 } else { 2.0 }, rim), StrokeKind::Inside);
    let compact = rect.height() < 138.0;

    // control medallion and title
    let mc = pos2(rect.left() + 28.0, rect.top() + 29.0);
    p.circle_filled(mc + vec2(0.0, 1.5), 19.0, rgba(0, 0, 0, 0.4));
    p.circle_filled(mc, 19.0, theme::OUTLINE);
    p.circle_filled(mc, 17.0, if next_up { theme::ACCENT } else if played { theme::MINT } else { Color32::from_rgb(0x8A, 0x5A, 0xB0) });
    icons::draw(&p, icon_for(drill.control()), mc, 25.0, theme::OUTLINE);
    let tx = rect.left() + 54.0;
    let title = drill.title();
    let ts = art::fit_size(&p, title, theme::title, 19.0, rect.right() - tx - 10.0);
    art::outlined(&p, pos2(tx, rect.top() + 22.0), Align2::LEFT_CENTER, title, theme::title(ts), theme::TEXT, theme::OUTLINE, (ts * 0.07).max(1.0), 1.0);
    let teaching = format!("Teaches {}", teaches(drill).to_lowercase());
    let teaching_size = art::fit_size(&p, &teaching, theme::ui_bold, 10.0, rect.right() - tx - 10.0);
    p.text(pos2(tx, rect.top() + 41.0), Align2::LEFT_CENTER, teaching, theme::ui_bold(teaching_size), theme::ACCENT);

    // instruction
    if !compact {
        let rows = ((rect.height() - 126.0) / 13.0).floor().clamp(1.0, 3.0) as usize;
        let g = clipped(&p, drill.instruction(), 11.0, fade(theme::TEXT, 0.92), rect.width() - 24.0, rows);
        p.galley(pos2(rect.left() + 12.0, rect.top() + 56.0), g, theme::TEXT);
    }

    // best level: ten pips, the star levels (3, 6, 9) marked
    let py = rect.bottom() - 44.0;
    let pw = (rect.width() - 24.0 - 9.0 * 2.0) / 10.0;
    for k in 0..10u8 {
        let x = rect.left() + 12.0 + k as f32 * (pw + 2.0);
        let r = Rect::from_min_size(pos2(x, py), vec2(pw, 6.0));
        let lit = k < best;
        let star_level = matches!(k + 1, 3 | 6 | 9);
        if lit {
            widgets::rrect_gradient(&p, r, 3.0, if star_level { theme::GOLD } else { Color32::from_rgb(0xFF, 0xB8, 0x44) }, if star_level { Color32::from_rgb(0xD8, 0x84, 0x14) } else { Color32::from_rgb(0xF0, 0x6A, 0x1A) });
        } else {
            p.rect_filled(r, 3.0, rgba(10, 3, 22, 0.55));
        }
        if star_level {
            p.circle_filled(pos2(r.center().x, r.top() - 5.0), 1.8, if lit { theme::GOLD } else { rgba(255, 255, 255, 0.25) });
        }
    }

    // The best level sits above its pips; stars and the medal have their own row.
    let fy = rect.bottom() - 18.0;
    p.hline((rect.left() + 12.0)..=(rect.right() - 12.0), fy - 13.0, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.08)));
    let label = if best == 0 { if next_up { "Start here".to_string() } else { "Try it".to_string() } } else { format!("Best L{best}") };
    let lc = if best == 0 { if next_up { theme::ACCENT } else { theme::SAND } } else { theme::SAND };
    p.text(pos2(rect.left() + 12.0, rect.bottom() - 61.0), Align2::LEFT_CENTER, label, theme::title(14.0), lc);
    let medal_c = pos2(rect.right() - 18.0, fy);
    if master {
        art::glow(&p, medal_c, 16.0, rgba(0xFF, 0xC8, 0x3D, 0.30));
        icons::draw(&p, Icon::Medal, medal_c, 24.0, theme::GOLD);
    } else {
        icons::draw(&p, Icon::Medal, medal_c, 22.0, rgba(255, 255, 255, 0.20));
    }
    widgets::stars(&p, pos2(rect.center().x - 10.0, fy), 3, stars, 17.0, 9.0);
    resp.clicked()
}

#[allow(clippy::too_many_arguments)]
fn lesson_card(ui: &mut egui::Ui, rect: Rect, i: usize, lesson: LessonId, stars: u8, open: bool, next_up: bool, just: Option<f32>, fresh_unlock: Option<f32>, _tm: f32) -> bool {
    let id = Id::new(("stampede-lesson", i));
    let resp = ui.interact(rect, id, Sense::click());
    let (scale, _) = if open { widgets::press_fx(ui, id, &resp) } else { (1.0, 0.0) };
    let pulse = if let Some(a) = fresh_unlock { (1.0 - phase(a, 0.3, 2.5)) * (0.5 + 0.5 * (a * 7.0).sin()) } else { 0.0 };
    let rect = Rect::from_center_size(rect.center(), rect.size() * (scale + 0.01 * pulse));
    let p = ui.painter().clone();
    let r = 16.0;
    let dim = if open { 1.0 } else { 0.55 };
    widgets::soft_shadow(&p, rect, r, 4.0, 12.0, 0.5);
    let done = stars >= 1;
    let (top, bot) = if done { (Color32::from_rgb(0x4B, 0x2A, 0x78), Color32::from_rgb(0x2A, 0x12, 0x4C)) } else { (Color32::from_rgb(0x5A, 0x2C, 0x86), Color32::from_rgb(0x30, 0x14, 0x56)) };
    widgets::rrect_gradient(&p, rect, r, fade(top, 0.94), fade(bot, 0.94));
    p.hline((rect.left() + r * 0.8)..=(rect.right() - r * 0.8), rect.top() + 1.5, Stroke::new(1.2_f32, rgba(255, 255, 255, 0.18 * dim)));
    let rim = if next_up { theme::ACCENT } else if done { rgba(0x6B, 0xE0, 0xA8, 0.55) } else { theme::OUTLINE };
    p.rect_stroke(rect, CornerRadius::same(r as u8), Stroke::new(if next_up { 2.5_f32 } else { 2.0 }, rim), StrokeKind::Inside);
    if pulse > 0.0 {
        p.rect_stroke(rect.expand(2.0), CornerRadius::same(r as u8 + 2), Stroke::new(3.0_f32, fade(theme::GOLD, pulse)), StrokeKind::Outside);
    }
    let tall = rect.height() >= 96.0;

    // number badge, title and the control medallion
    let bc = pos2(rect.left() + 19.0, rect.top() + 21.0);
    p.circle_filled(bc + vec2(0.0, 1.5), 12.5, rgba(0, 0, 0, 0.4));
    p.circle_filled(bc, 12.5, theme::OUTLINE);
    p.circle_filled(bc, 10.5, if done { theme::MINT } else if open { theme::ACCENT } else { Color32::from_rgb(0x6A, 0x58, 0x80) });
    if done {
        icons::draw(&p, Icon::Check, bc, 14.0, theme::OUTLINE);
    } else {
        p.text(bc + vec2(0.0, 0.5), Align2::CENTER_CENTER, format!("{}", lesson_number(lesson)), theme::title(14.0), Color32::WHITE);
    }
    let title = lesson.title();
    let ts = art::fit_size(&p, title, theme::title, 15.5, rect.width() - 36.0 - 30.0);
    art::outlined(&p, pos2(rect.left() + 36.0, rect.top() + 21.0), Align2::LEFT_CENTER, title, theme::title(ts), fade(theme::TEXT, dim), theme::OUTLINE, (ts * 0.07).max(1.0), 1.0);
    let mc = pos2(rect.right() - 17.0, rect.top() + 21.0);
    icons::draw(&p, icon_for(lesson.control()), mc, 20.0, fade(theme::SAND, dim));

    // instruction
    if tall {
        let g = clipped(&p, lesson.instruction(), 10.2, fade(theme::TEXT, 0.9 * dim), rect.width() - 20.0, 2);
        p.galley(pos2(rect.left() + 10.0, rect.top() + 38.0), g, theme::TEXT);
    }

    // footer: Start / Replay, stars (or the lock)
    let fy = rect.bottom() - 15.0;
    p.hline((rect.left() + 10.0)..=(rect.right() - 10.0), fy - 12.0, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.08)));
    if open {
        let anim = just.map(|a| (a - 0.5).max(0.0)).unwrap_or(9.0);
        widgets::stars(&p, pos2(rect.right() - 34.0, fy), 3, stars, 15.0, anim);
        let label = if done { "Replay" } else if next_up { "Start" } else { "Try it" };
        p.text(pos2(rect.left() + 10.0, fy), Align2::LEFT_CENTER, label, theme::title(13.0), if next_up { theme::ACCENT } else { theme::SAND });
    } else {
        p.rect_filled(rect, CornerRadius::same(r as u8), rgba(10, 3, 22, 0.40));
        icons::draw(&p, Icon::Lock, pos2(rect.left() + 18.0, fy), 15.0, theme::TEXT_DIM);
        let label = format!("Finish lesson {}", lesson_number(lesson) - 1);
        let size = art::fit_size(&p, &label, theme::ui_bold, 10.0, rect.width() - 41.0);
        p.text(pos2(rect.left() + 31.0, fy), Align2::LEFT_CENTER, label, theme::ui_bold(size), theme::TEXT_DIM);
    }
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::data_dir::Profile;
    use crate::game::source::{LocalRace, RaceSource};

    #[test]
    fn drill_launches_are_on_the_paddock_with_a_dummy_for_bash_and_laser_practice() {
        let p = Profile::default();
        for id in DrillId::ALL {
            let l = drill_launch(&p, id);
            assert_eq!(l.config.track, TrackId::Field);
            assert_eq!(l.config.drill, Some(id));
            assert!(matches!(l.kind, RaceKind::Drill(d) if d == id));
            assert_eq!(l.config.racers.len(), 1);
            assert!(l.config.racers[0].kind == RacerKind::Human);
            let r = LocalRace::new(l.config, Some(0));
            assert_eq!(r.view().racers.len(), if matches!(id, DrillId::BashBuddy | DrillId::LaserRange) { 2 } else { 1 });
        }
    }

    #[test]
    fn every_lesson_runs_on_both_venues_with_its_state_and_something_to_do() {
        let p = Profile::default();
        for venue in Venue::ALL {
            for lesson in LessonId::ALL {
                let l = launch_for_on(&p, lesson, venue.track());
                assert_eq!(l.config.track, venue.track());
                let r = LocalRace::new(l.config.clone(), Some(0));
                let v = r.view();
                let ls = v.lesson.unwrap_or_else(|| panic!("{lesson:?} on {venue:?} has no lesson state"));
                assert!(ls.goal > 0, "{lesson:?} on {venue:?}");
                // Each venue supplies enough props to finish the lesson.
                let count = match lesson {
                    LessonId::RunSteer => v.track.features.iter().filter(|f| matches!(f, race_core::Feature::Gate { .. })).count(),
                    LessonId::Jump => v.track.features.iter().filter(|f| matches!(f, race_core::Feature::Hurdle { .. })).count(),
                    LessonId::LaserEyes => v.racers.len().saturating_sub(1) * 3,
                    LessonId::HeadBash => v.racers.len().saturating_sub(1) * 3,
                    LessonId::CoinsItems => v.coins.len(),
                    LessonId::DriftBoost => continue,
                };
                let needed = if lesson == LessonId::CoinsItems { 10 } else { ls.goal as usize };
                assert!(count >= needed, "{lesson:?} on {venue:?}: {count} props for {needed}");
            }
        }
        assert_eq!(launch_for(&p, LessonId::Jump).config.track, TrackId::Training);
    }

    #[test]
    fn lessons_unlock_in_order() {
        let mut s = [0u8; 6];
        assert!(unlocked(&s, 0) && !unlocked(&s, 1));
        s[0] = 1;
        assert!(unlocked(&s, 1) && !unlocked(&s, 2));
    }
}
