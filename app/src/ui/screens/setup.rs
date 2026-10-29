//! Race setup: Quick Race (track, laps, bots, difficulty), Grand Prix (the four
//! cup tracks in order, difficulty) and Time Trial (track, laps, your best).

use super::common::{self, fmt_time, Cup, LEVELS};
use super::garage::GarageScreen;
use super::tracks;
use super::{Ctx, Screen, Transition};
use crate::ui::art::{self};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Btn, Seg};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Rect};
use race_core::{Character, TrackId};

/// The "Training" group in the Quick Race and Time Trial pickers: the Oval and the Figure Eight (never the Paddock).
const TRAINING_GROUP: [TrackId; 2] = [TrackId::Training, TrackId::Eight];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Quick,
    GrandPrix,
    TimeTrial,
}

pub struct SetupScreen {
    mode: Mode,
    t: f32,
    /// Which cup's four tracks are shown (Quick Race / Time Trial pick from both; a Grand Prix runs one).
    cup: Cup,
    /// The "Training" group (the Oval and the Figure Eight) is showing instead of a cup (Quick Race and Time Trial only).
    training: bool,
    /// Index into the group's tracks.
    track: usize,
    laps: u8,
    bots: usize,
    level: usize,
    /// Screenshots: put some best times into the profile.
    demo: bool,
}

impl SetupScreen {
    pub fn new(mode: Mode) -> SetupScreen {
        SetupScreen { mode, t: 0.0, cup: Cup::One, training: false, track: 0, laps: 3, bots: 7, level: 1, demo: false }
    }

    /// Staged for screenshots: another track picked, fewer bots, Hard.
    pub fn sample(mode: Mode) -> SetupScreen {
        SetupScreen { track: 2, bots: 5, level: 2, laps: 4, demo: true, ..SetupScreen::new(mode) }
    }

    /// Staged for screenshots: Cup 2 showing, Volcano Vault picked.
    pub fn sample_cup2(mode: Mode) -> SetupScreen {
        SetupScreen { cup: Cup::Two, track: 1, ..SetupScreen::sample(mode) }
    }

    /// Staged for screenshots: the Training group showing, the Figure Eight picked.
    pub fn sample_training(mode: Mode) -> SetupScreen {
        SetupScreen { training: true, track: 1, ..SetupScreen::sample(mode) }
    }
}

fn row_label(p: &egui::Painter, rect: Rect, text: &str) {
    p.text(pos2(rect.left(), rect.center().y), Align2::LEFT_CENTER, text, theme::ui_bold(12.5), theme::SAND);
}

impl Screen for SetupScreen {
    fn name(&self) -> &'static str {
        match self.mode {
            Mode::Quick => "quick_race",
            Mode::GrandPrix => "grand_prix",
            Mode::TimeTrial => "time_trial",
        }
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if std::mem::take(&mut self.demo) {
            for (id, secs, lap) in [(TrackId::Savanna, 84.37, 27.91), (TrackId::Canyon, 131.02, 43.18), (TrackId::Oasis, 92.4, 29.8), (TrackId::Volcano, 128.9, 41.6)] {
                ctx.profile.best_times.insert(common::best_key(id), secs);
                ctx.profile.best_laps.insert(common::best_key(id), lap);
            }
            ctx.profile.gp_wins = 2;
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
        let mode = self.mode;
        let title = match mode {
            Mode::Quick => "Quick Race",
            Mode::GrandPrix => "Grand Prix",
            Mode::TimeTrial => "Time Trial",
        };
        let (mut cup, mut training, mut track, mut laps, mut bots, mut level) = (self.cup, self.training && self.mode != Mode::GrandPrix, self.track, self.laps, self.bots, self.level);
        common::page(ctx.viewport.screen, tm, ui, 0.22, |ui, _brand| {
            let hd = widgets::header(ui, safe, title, widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let body = hd.body;
            let rw = 268.0_f32.min(body.width() * 0.36);
            let gap = 14.0;
            let layout = common::Layout::new(safe);
            let (left, right) = layout.split(body, rw, (body.height() * 0.51).min(body.height() - 300.0), gap);

            // The tracks in the showing group: a cup's four, or the two training venues (never the Paddock).
            let group: Vec<TrackId> = if training { TRAINING_GROUP.to_vec() } else { cup.tracks().to_vec() };
            // ── cup switcher, then the track cards, 2 x 2 ──
            let sw_h = 32.0;
            let sw = Rect::from_min_size(left.min, vec2(left.width().min(if mode == Mode::GrandPrix { 240.0 } else { 360.0 }), sw_h));
            {
                let e = widgets::enter(t, 1);
                let mut segs: Vec<widgets::Seg> = Cup::ALL.iter().map(|c| widgets::Seg::new(c.name())).collect();
                if mode != Mode::GrandPrix {
                    segs.push(widgets::Seg::new("Training"));
                    segs.push(widgets::Seg::new("My Tracks"));
                }
                let sel = if training { 2 } else if cup == Cup::One { 0 } else { 1 };
                let picked = widgets::staged(ui, e, vec2(0.0, -20.0), |ui, off| widgets::segmented(ui, sw.translate(off), &format!("{mode:?}-cup"), &segs, sel));
                if let Some(i) = picked {
                    feedback::tap(ctx);
                    if i == 3 {
                        out = Transition::Push(Box::new(super::my_tracks::MyTracksScreen::picker(mode == Mode::TimeTrial, bots, LEVELS[level].1)));
                    } else {
                    training = i == 2;
                    if !training {
                        cup = Cup::ALL[i];
                    }
                    track = track.min(if training { 1 } else { 3 });
                    }
                }
                let note = match (mode, training) {
                    (Mode::GrandPrix, _) => "Four rounds, points for every place",
                    (_, true) => "The Oval and the Figure Eight",
                    _ => "Pick any of the eight tracks",
                };
                if !layout.portrait { ui.painter().text(pos2(sw.right() + 10.0, sw.center().y), Align2::LEFT_CENTER, note, theme::ui_medium(9.5), theme::TEXT_DIM); }
            }
            let grid = Rect::from_min_max(pos2(left.left(), left.top() + sw_h + 8.0), left.max);
            let cg = 10.0;
            // two training venues fill the whole grid height side by side
            let rows = if training { 1.0 } else { 2.0 };
            let (cw, ch) = ((grid.width() - cg) / 2.0, (grid.height() - cg * (rows - 1.0)) / rows);
            for (i, id) in group.iter().enumerate() {
                let rect = Rect::from_min_size(pos2(grid.left() + (i % 2) as f32 * (cw + cg), grid.top() + (i / 2) as f32 * (ch + cg)), vec2(cw, ch));
                let e = widgets::enter(t, i + 1);
                let info = tracks::info(*id);
                let best = common::best_time(ctx.profile, *id);
                // the key includes the cup so a switch re-animates the minimaps
                let clicked = widgets::staged(ui, e, vec2(0.0, 36.0), |ui, off| {
                    tracks::card(ui, rect.translate(off), &format!("{mode:?}-{cup:?}-{training}-{i}"), &info, mode != Mode::GrandPrix && track == i, best, if mode == Mode::GrandPrix { Some(i + 1) } else { None }, mode != Mode::GrandPrix, (t - 0.2 - 0.07 * i as f32) / 0.7).clicked()
                });
                if clicked && mode != Mode::GrandPrix && track != i {
                    feedback::tap(ctx);
                    track = i;
                }
            }

            // ── the settings panel ──
            let e = widgets::enter(t, 5);
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
                let b = widgets::button(ui, br, &format!("{mode:?}-ostrich"), &Btn::new(ch.name).size(17.0));
                widgets::chip(&p, pos2(br.left() + 17.0, br.center().y), 8.5, art::from_f3(ch.body));
                p.text(pos2(br.right() - 14.0, br.center().y), Align2::RIGHT_CENTER, "change", theme::ui_bold(10.5), theme::TEXT_DIM);
                if b.clicked() {
                    feedback::tap(ctx);
                    out = Transition::Push(Box::new(GarageScreen::new()));
                }

                // laps
                let r = next_row(rh);
                row_label(&p, r, "Laps");
                let labels = ["1", "2", "3", "4", "5"];
                let segs: Vec<Seg> = labels.iter().map(|l| Seg::new(l)).collect();
                if let Some(i) = widgets::segmented(ui, Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max), &format!("{mode:?}-laps"), &segs, (laps as usize).clamp(1, 5) - 1) {
                    feedback::tap(ctx);
                    laps = i as u8 + 1;
                }

                if mode == Mode::TimeTrial {
                    // race your saved best run as a ghost
                    let r = next_row(rh);
                    row_label(&p, r, "Ghost");
                    let segs = [Seg::new("On"), Seg::new("Off")];
                    if let Some(i) = widgets::segmented(ui, Rect::from_min_max(pos2(r.left() + lw + 6.0, r.top()), r.max), "tt-ghost", &segs, if ctx.profile.show_ghost { 0 } else { 1 }) {
                        feedback::tap(ctx);
                        ctx.profile.show_ghost = i == 0;
                        ctx.save_profile();
                    }
                }

                if mode == Mode::Quick {
                    let r = next_row(rh);
                    row_label(&p, r, "Bots");
                    let ctl = Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max);
                    let bw = rh;
                    let minus = Rect::from_min_size(ctl.left_top(), vec2(bw + 6.0, rh));
                    let plus = Rect::from_min_size(pos2(ctl.right() - bw - 6.0, ctl.top()), vec2(bw + 6.0, rh));
                    if widgets::button(ui, minus, "bots-minus", &Btn::new("-").enabled(bots > 0).size(24.0)).clicked() {
                        feedback::tap(ctx);
                        bots -= 1;
                    }
                    if widgets::button(ui, plus, "bots-plus", &Btn::new("+").enabled(bots < 7).size(24.0)).clicked() {
                        feedback::tap(ctx);
                        bots += 1;
                    }
                    art::outlined(&p, pos2(ctl.center().x, ctl.center().y), Align2::CENTER_CENTER, &format!("{bots}"), theme::numbers(26.0), theme::TEXT, theme::OUTLINE, 2.0, 0.0);
                }

                if mode != Mode::TimeTrial {
                    let r = next_row(rh);
                    row_label(&p, r, "Bots are");
                    let segs: Vec<Seg> = LEVELS.iter().map(|(n, _)| Seg::new(n)).collect();
                    if let Some(i) = widgets::segmented(ui, Rect::from_min_max(pos2(r.left() + lw + 6.0, r.top()), r.max), &format!("{mode:?}-level"), &segs, level) {
                        feedback::tap(ctx);
                        level = i;
                    }
                }

                // the middle: what you have done here
                let go_h = 54.0;
                let mid = Rect::from_min_max(pos2(inner.left(), y - 1.0), pos2(inner.right(), inner.bottom() - go_h - 9.0));
                if mid.height() > 26.0 {
                    match mode {
                        Mode::TimeTrial => {
                            let id = group[track.min(group.len() - 1)];
                            widgets::well(&p, mid, 12.0);
                            icons::draw(&p, Icon::Stopwatch, pos2(mid.left() + 22.0, mid.center().y), 24.0, theme::GOLD);
                            let (bt, bl) = (common::best_time(ctx.profile, id), common::best_lap(ctx.profile, id));
                            let tx = mid.left() + 42.0;
                            p.text(pos2(tx, mid.center().y - 9.0), Align2::LEFT_CENTER, "Best (3 laps)", theme::ui_medium(11.0), theme::TEXT_DIM);
                            p.text(pos2(tx, mid.center().y + 9.0), Align2::LEFT_CENTER, "Best lap", theme::ui_medium(11.0), theme::TEXT_DIM);
                            p.text(pos2(mid.right() - 10.0, mid.center().y - 9.0), Align2::RIGHT_CENTER, bt.map(fmt_time).unwrap_or("--".into()), theme::numbers(16.0), if bt.is_some() { theme::GOLD } else { theme::TEXT_DIM });
                            p.text(pos2(mid.right() - 10.0, mid.center().y + 9.0), Align2::RIGHT_CENTER, bl.map(fmt_time).unwrap_or("--".into()), theme::numbers(16.0), if bl.is_some() { theme::GOLD } else { theme::TEXT_DIM });
                        }
                        Mode::GrandPrix => {
                            widgets::well(&p, mid, 12.0);
                            icons::draw(&p, Icon::Trophy, pos2(mid.left() + 22.0, mid.center().y), 26.0, theme::GOLD);
                            p.text(pos2(mid.left() + 42.0, mid.center().y - 8.0), Align2::LEFT_CENTER, "Cups won", theme::ui_medium(11.0), theme::TEXT_DIM);
                            p.text(pos2(mid.left() + 42.0, mid.center().y + 9.0), Align2::LEFT_CENTER, if cup == Cup::One { "Cup 1, 8 racers" } else { "Cup 2, 8 racers" }, theme::ui_medium(11.0), theme::TEXT_DIM);
                            art::outlined(&p, pos2(mid.right() - 18.0, mid.center().y), Align2::CENTER_CENTER, &format!("{}", ctx.profile.gp_wins), theme::numbers(24.0), theme::GOLD, theme::OUTLINE, 2.0, 0.0);
                        }
                        Mode::Quick => {}
                    }
                }

                // go
                let go = Rect::from_min_max(pos2(inner.left(), inner.bottom() - go_h), inner.max);
                let label = match mode {
                    Mode::Quick => "Go!",
                    Mode::GrandPrix => "Start cup",
                    Mode::TimeTrial => "Start",
                };
                if widgets::button(ui, go, &format!("{mode:?}-go"), &Btn::primary(label).size(30.0)).clicked() {
                    feedback::select(ctx);
                    let level = LEVELS[level].1;
                    let launch = match mode {
                        Mode::Quick => common::quick_launch(ctx.profile, group[track.min(group.len() - 1)], laps, bots, level),
                        Mode::GrandPrix => common::grand_prix_launch(ctx.profile, cup, laps, level),
                        Mode::TimeTrial => common::time_trial_launch(ctx.profile, group[track.min(group.len() - 1)], laps),
                    };
                    out = Transition::Push(crate::game::race_screen(launch));
                }
            });
        });
        self.cup = cup;
        self.training = training;
        self.track = track;
        self.laps = laps;
        self.bots = bots;
        self.level = level;
        out
    }
}
