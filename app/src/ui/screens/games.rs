//! More games (spec 20.3): the built-in catalog of other games, each with what
//! the player can do about it: Get, Update or Open. The relays are asked once, when
//! the screen opens (`games::Games`); installs go through the shared updater, so a
//! download goes on if the player leaves this screen.

use super::common;
use super::{Ctx, Screen, Transition};
use crate::games::{GameStatus, Games, CATALOG};
use crate::platform::os::ApkStatus;
use crate::ui::art::{self, rgba};
use crate::ui::widgets::{self, Btn, S2, S3};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect, Sense, Stroke};

/// Height of one game's card.
const CARD_H: f32 = 108.0;

/// A staged state for screenshots.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sample {
    Get,
    NoRelease,
    Downloading,
    Open,
}

pub struct GamesScreen {
    t: f32,
    games: Games,
    started: bool,
    sample: Option<Sample>,
    scroll: f32,
}

impl Default for GamesScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl GamesScreen {
    pub fn new() -> GamesScreen {
        GamesScreen { t: 0.0, games: Games::new(), started: false, sample: None, scroll: 0.0 }
    }

    fn staged(s: Sample) -> GamesScreen {
        GamesScreen { sample: Some(s), ..GamesScreen::new() }
    }

    pub fn sample_get() -> GamesScreen {
        Self::staged(Sample::Get)
    }

    pub fn sample_none() -> GamesScreen {
        Self::staged(Sample::NoRelease)
    }

    pub fn sample_downloading() -> GamesScreen {
        Self::staged(Sample::Downloading)
    }

    pub fn sample_open() -> GamesScreen {
        Self::staged(Sample::Open)
    }

    /// Set up the staged state (once, screenshots only).
    fn stage(&mut self, ctx: &mut Ctx, s: Sample) {
        use crate::games::Fetch;
        let e = &CATALOG[0];
        let release = crate::update::demo_release(e.channel, "0.4.1", 9_800_000, e.cert.unwrap_or(""));
        match s {
            Sample::Get => self.games.demo(Fetch::Answered, vec![None], vec![Ok(Some(release))]),
            Sample::NoRelease => self.games.demo(Fetch::Answered, vec![None], vec![Ok(None)]),
            Sample::Open => self.games.demo(Fetch::Answered, vec![Some(4001)], vec![Ok(Some(release))]),
            Sample::Downloading => {
                self.games.demo(Fetch::Answered, vec![None], vec![Ok(Some(release))]);
                ctx.updates.demo_game_state(e.package, ApkStatus::Downloading { done: 3_900_000, total: 9_800_000 });
            }
        }
    }
}

/// What a card says and which button it offers.
struct CardView {
    line: String,
    tone: Color32,
    progress: Option<f32>,
    /// (label, primary, enabled)
    button: (&'static str, bool, bool),
    /// A download is running: the button cancels it.
    cancel: bool,
}

fn size_text(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

fn card_view(status: &GameStatus, install: Option<&ApkStatus>) -> CardView {
    let dim = theme::TEXT_DIM;
    let mut v = match status {
        GameStatus::ComingSoon => CardView { line: "Coming soon".into(), tone: dim, progress: None, button: ("Coming soon", false, false), cancel: false },
        GameStatus::Checking => CardView { line: "Looking for the latest release\u{2026}".into(), tone: dim, progress: None, button: ("Get", true, false), cancel: false },
        GameStatus::CantReach => CardView { line: "Couldn't reach the relays".into(), tone: theme::LASER, progress: None, button: ("Get", true, false), cancel: false },
        GameStatus::NoRelease => CardView { line: "No release published yet".into(), tone: dim, progress: None, button: ("Get", true, false), cancel: false },
        GameStatus::WrongKey { version } => CardView { line: format!("Version {version} is signed with a different key, so it isn't offered."), tone: theme::GOLD, progress: None, button: ("Get", true, false), cancel: false },
        GameStatus::Get { version, size } => CardView { line: format!("Version {version} \u{b7} {}", size_text(*size)), tone: theme::SAND, progress: None, button: ("Get", true, true), cancel: false },
        GameStatus::Open => CardView { line: "Installed".into(), tone: theme::MINT, progress: None, button: ("Open", false, true), cancel: false },
        GameStatus::Update { version, size } => CardView { line: format!("Update to {version} \u{b7} {}", size_text(*size)), tone: theme::ACCENT, progress: None, button: ("Update", true, true), cancel: false },
    };
    // A download for this game overrides what the card says.
    if matches!(status, GameStatus::Get { .. } | GameStatus::Update { .. } | GameStatus::Open) {
        match install {
            Some(ApkStatus::Downloading { done, total }) => {
                let pct = done.saturating_mul(100).checked_div(*total).map_or(0, |p| p.min(100));
                v.line = format!("Downloading\u{2026} {pct}%");
                v.tone = theme::ACCENT;
                v.progress = Some((*done as f32 / (*total).max(1) as f32).clamp(0.0, 1.0));
                v.button = ("Cancel", false, true);
                v.cancel = true;
            }
            Some(ApkStatus::Installing) => {
                v.line = "Preparing the install\u{2026}".into();
                v.tone = theme::ACCENT;
                v.button.2 = false;
            }
            Some(ApkStatus::WaitingForUser) => {
                v.line = "Confirm in Android's prompt".into();
                v.tone = theme::ACCENT;
                v.button.2 = false;
            }
            Some(ApkStatus::NeedsPermission) => {
                v.line = "Allow installs from Stampede in the settings page that opened, then tap again".into();
                v.tone = theme::GOLD;
            }
            Some(ApkStatus::Failed(m)) => {
                v.line = m.clone();
                v.tone = theme::LASER;
            }
            Some(ApkStatus::Idle | ApkStatus::Installed) | None => {}
        }
    }
    v
}

impl Screen for GamesScreen {
    fn name(&self) -> &'static str {
        "more_games"
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !std::mem::replace(&mut self.started, true) {
            match self.sample {
                Some(s) if ctx.opts.shot.is_some() => self.stage(ctx, s),
                // One relay pass when the screen opens, and not a shot or a test.
                Some(_) => {}
                None => self.games.begin(&ctx.profile.relays),
            }
        }
        if self.sample.is_none() {
            self.games.tick(ctx.dt, ctx.platform);
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
        let mut scroll = self.scroll;
        let mut act: Option<(usize, bool)> = None; // (entry, cancel?)
        let mut open: Option<usize> = None;
        let statuses: Vec<GameStatus> = (0..CATALOG.len()).map(|i| self.games.status(i)).collect();
        let installs: Vec<Option<ApkStatus>> = CATALOG.iter().map(|e| ctx.updates.game_status(e.package).cloned()).collect();
        common::page(ctx.viewport.screen, tm, ui, 0.5, |ui, _| {
            let hd = widgets::header(ui, safe, "More games", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let body = hd.body;
            let portrait = common::Layout::new(safe).portrait;
            let card_h = if portrait { 210.0 } else { CARD_H };
            let total = CATALOG.len() as f32 * (card_h + S2);
            let max_scroll = (total - body.height()).max(0.0);
            let drag = ui.interact(body, egui::Id::new("games-scroll"), Sense::drag());
            if drag.dragged() {
                scroll -= drag.drag_delta().y;
            }
            scroll = scroll.clamp(0.0, max_scroll);
            ui.set_clip_rect(body.expand2(vec2(8.0, 0.0)));
            let w = body.width().min(720.0);
            let x0 = body.center().x - w / 2.0;
            for (i, e) in CATALOG.iter().enumerate() {
                let r = Rect::from_min_size(pos2(x0, body.top() + i as f32 * (card_h + S2) - scroll), vec2(w, card_h));
                if !body.expand(0.5).contains_rect(r) {
                    continue;
                }
                let cv = card_view(&statuses[i], installs[i].as_ref());
                widgets::staged(ui, widgets::enter(t, 1 + i), vec2(0.0, 28.0), |ui, off| {
                    let r = r.translate(off);
                    let p = ui.painter().clone();
                    widgets::panel(&p, r, 20.0);
                    // icon
                    let ic = Rect::from_center_size(pos2(r.left() + 16.0 + 34.0, r.top() + if portrait { 48.0 } else { r.height() * 0.5 }), vec2(68.0, 68.0));
                    let tex = art::game_icon(ui.ctx(), e.package, e.icon);
                    widgets::soft_shadow(&p, ic, 16.0, 4.0, 10.0, 0.5);
                    art::sprite(&p, tex.id(), ic.center(), ic.size(), 0.0, false, Color32::WHITE);
                    p.rect_stroke(ic, egui::CornerRadius::same(14), Stroke::new(1.5_f32, rgba(255, 255, 255, 0.18)), egui::StrokeKind::Inside);
                    // texts
                    let bw = 132.0;
                    let tx = ic.right() + 14.0;
                    let room = r.right() - if portrait { 14.0 } else { bw + 28.0 } - tx;
                    let nsize = art::fit_size(&p, e.name, theme::title, 24.0, room);
                    p.text(pos2(tx, r.top() + 26.0), Align2::LEFT_CENTER, e.name, theme::title(nsize), theme::TEXT);
                    let blurb = p.layout(e.blurb.into(), theme::ui_medium(12.0), theme::TEXT_DIM, room);
                    let line_top = r.top() + 52.0 + blurb.size().y;
                    p.galley(pos2(tx, r.top() + 42.0), blurb, theme::TEXT_DIM);
                    let g = p.layout(cv.line.clone(), theme::ui_bold(12.5), cv.tone, room);
                    p.galley(pos2(tx, if portrait { line_top } else { r.top() + 62.0 }), g, cv.tone);
                    if let Some(pr) = cv.progress {
                        let track = Rect::from_min_size(pos2(tx, r.bottom() - 14.0), vec2(room, 5.0));
                        p.rect_filled(track, 2.5, Color32::from_white_alpha(28));
                        p.rect_filled(Rect::from_min_size(track.min, vec2(track.width() * pr, 5.0)), 2.5, theme::ACCENT);
                    }
                    // the button
                    let b = Rect::from_center_size(pos2(r.right() - 14.0 - bw / 2.0, if portrait { r.bottom() - 38.0 } else { r.center().y }), vec2(bw, 46.0));
                    let btn = if cv.button.1 { Btn::primary(cv.button.0) } else { Btn::new(cv.button.0) };
                    if widgets::button(ui, b, &format!("game-{i}"), &btn.size(18.0).enabled(cv.button.2)).clicked() {
                        if cv.cancel {
                            act = Some((i, true));
                        } else if matches!(statuses[i], GameStatus::Open) {
                            open = Some(i);
                        } else {
                            act = Some((i, false));
                        }
                    }
                });
            }
            let _ = S3;
        });
        self.scroll = scroll;
        if let Some(i) = open {
            feedback::select(ctx);
            if !self.games.open(i, ctx.platform) {
                ctx.say("Couldn't open it. Try from your app list.");
            }
        }
        if let Some((i, cancel)) = act {
            if cancel {
                feedback::tap(ctx);
                ctx.updates.cancel_install(&mut *ctx.platform);
            } else {
                feedback::select(ctx);
                if let Err(notice) = self.games.install(i, ctx.updates, &mut *ctx.platform) {
                    ctx.say(&notice);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_says_what_is_going_on() {
        let get = GameStatus::Get { version: "1.0.0".into(), size: 9_800_000 };
        let v = card_view(&get, None);
        assert_eq!((v.line.as_str(), v.button), ("Version 1.0.0 \u{b7} 9.8 MB", ("Get", true, true)));
        let v = card_view(&get, Some(&ApkStatus::Downloading { done: 50, total: 100 }));
        assert_eq!((v.line.as_str(), v.cancel, v.progress), ("Downloading\u{2026} 50%", true, Some(0.5)));
        assert_eq!(card_view(&get, Some(&ApkStatus::Failed("nope".into()))).line, "nope");
        assert_eq!(card_view(&GameStatus::NoRelease, None).line, "No release published yet");
        assert!(!card_view(&GameStatus::NoRelease, None).button.2, "nothing to get");
        assert_eq!(card_view(&GameStatus::CantReach, None).line, "Couldn't reach the relays");
        assert_eq!(card_view(&GameStatus::ComingSoon, None).line, "Coming soon");
        assert_eq!(card_view(&GameStatus::Open, None).button, ("Open", false, true));
        // A leftover download state never changes a game that has nothing to get.
        assert_eq!(card_view(&GameStatus::NoRelease, Some(&ApkStatus::Downloading { done: 1, total: 2 })).line, "No release published yet");
    }
}
