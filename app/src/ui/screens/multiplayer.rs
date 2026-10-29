//! Multiplayer: Host (opens a room and its lobby), Join by code and Nearby, Join
//! with invite (a pasted Keep link) and My Keeps (Keeps joined before, one tap to rejoin).

use super::common;
use super::join::JoinScreen;
use super::lobby::LobbyScreen;
use super::menu::big_card;
use super::nearby::NearbyScreen;
use super::{Ctx, Screen, Transition};
use crate::keeps::{self, JoinTarget, Keep};
use crate::online::OnlineSession;
use crate::ui::art::{self, rgba};
use crate::ui::icons::Icon;
use crate::ui::widgets::{self, Btn, S2, S3};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Rect};
use race_session::HostConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnlineKind {
    Host,
    Join,
    Nearby,
}

impl OnlineKind {
    fn title(self) -> &'static str {
        match self {
            OnlineKind::Host => "Host a game",
            OnlineKind::Join => "Join by code",
            OnlineKind::Nearby => "Nearby games",
        }
    }
    fn icon(self) -> Icon {
        match self {
            OnlineKind::Host => Icon::Host,
            OnlineKind::Join => Icon::Join,
            OnlineKind::Nearby => Icon::Nearby,
        }
    }
    fn blurb(self) -> &'static str {
        match self {
            OnlineKind::Host => "Open a room, share the six-letter code and fill the empty seats with bots.",
            OnlineKind::Join => "Type a friend's room code and drop straight onto their grid.",
            OnlineKind::Nearby => "Find games on the same Wi-Fi, no code needed.",
        }
    }
}

/// How tall the Keeps list's rows are.
const KEEP_ROW_H: f32 = 54.0;
/// The Join with invite strip.
const INVITE_BAR_H: f32 = 56.0;
/// The three cards when My Keeps shares the page.
const MINI_CARD_H: f32 = 72.0;

pub struct MultiplayerScreen {
    t: f32,
    /// Open the join screen for this room or Keep on the first frame (a join link
    /// from Home or the menu), so Back from it lands here.
    join: Option<JoinTarget>,
    /// My Keeps, newest play first; `None` until read from `keeps.json`.
    keeps: Option<Vec<Keep>>,
    /// Read `keeps.json` again on the next frame (another screen may have changed it).
    reload: bool,
    /// Pixels the Keeps list is scrolled up.
    scroll: f32,
    /// The Keep (host key) whose Forget is waiting for a second tap.
    confirm: Option<String>,
    /// Screenshots: show `keeps` as given and never read the file.
    sample: bool,
    /// `ctx.time` of the last frame this screen was on top: a long gap means another
    /// screen (a lobby, a race, Nearby) covered it and may have changed My Keeps.
    last_seen: f64,
}

/// A gap this long between two frames of the screen means it was covered (seconds).
const COVERED_GAP: f64 = 0.25;

#[cfg(test)]
thread_local! {
    /// Tests: how many Keeps the screen last read from `keeps.json`.
    pub static LAST_READ: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Tests: how many times the screen has read `keeps.json`.
    pub static READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Default for MultiplayerScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiplayerScreen {
    pub fn new() -> MultiplayerScreen {
        MultiplayerScreen { t: 0.0, join: None, keeps: None, reload: false, scroll: 0.0, confirm: None, sample: false, last_seen: f64::NEG_INFINITY }
    }

    /// Multiplayer with the join screen for `target` straight on top.
    pub fn joining(target: JoinTarget) -> MultiplayerScreen {
        MultiplayerScreen { join: Some(target), ..MultiplayerScreen::new() }
    }

    /// Three saved Keeps (screenshots).
    pub fn keeps_sample() -> MultiplayerScreen {
        let now = keeps::unix_now();
        let list = vec![
            Keep::new(&[0x11; 32], &[1; 32], Some("Cousin's Keep"), now - 2 * 3600),
            Keep::new(&[0x22; 32], &[2; 32], None, now - 3 * 86_400),
            Keep::new(&[0x33; 32], &[3; 32], Some("The Friday Gang"), now - 40 * 86_400),
        ];
        MultiplayerScreen { keeps: Some(list), sample: true, ..MultiplayerScreen::new() }
    }

    /// Many saved Keeps, the list scrolled (screenshots).
    pub fn many_keeps_sample() -> MultiplayerScreen {
        let now = keeps::unix_now();
        let list = (0..20u8).map(|i| Keep::new(&[i + 1; 32], &[i; 32], Some(&format!("Keep number {}", i + 1)), now - u64::from(i) * 5000)).collect();
        MultiplayerScreen { keeps: Some(list), sample: true, scroll: 80.0, confirm: Some("02".repeat(32)), ..MultiplayerScreen::new() }
    }

    /// Push `screen`, and read My Keeps again when we are back.
    fn push(&mut self, screen: Box<dyn Screen>) -> Transition {
        self.reload = true;
        Transition::Push(screen)
    }
}

impl Screen for MultiplayerScreen {
    fn name(&self) -> &'static str {
        "multiplayer"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        // Back from a screen that covered this one: My Keeps may have changed meanwhile.
        if ctx.time - self.last_seen > COVERED_GAP {
            self.reload = true;
        }
        self.last_seen = ctx.time;
        // Not while this screen fades out under a screen it just pushed: that join has not
        // happened yet, so a read now would be one join old. The first frame it is on top again
        // (or the Join tap itself, which reads the file afresh) picks up the change.
        if !self.sample && (self.keeps.is_none() || (self.reload && !ctx.input_blocked)) {
            self.reload = false;
            self.keeps = Some(keeps::load(ctx.data_dir));
            #[cfg(test)]
            {
                LAST_READ.with(|c| c.set(self.keeps.as_ref().map(Vec::len)));
                READS.with(|c| c.set(c.get() + 1));
            }
            self.scroll = 0.0;
            self.confirm = None;
        }
        // A join link opened the app (or arrived while here): straight into joining.
        if let Some(target) = self.join.take() {
            return self.push(Box::new(JoinScreen::with_target(target)));
        }
        if let Some(target) = ctx.take_join_code() {
            log::info!("multiplayer: join link for {}", keeps::describe(&target));
            return self.push(Box::new(JoinScreen::with_target(target)));
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
        let list = self.keeps.clone().unwrap_or_default();
        let now = keeps::unix_now();
        let mut scroll = self.scroll;
        let mut confirm = self.confirm.clone();
        let mut rejoin: Option<Keep> = None;
        let mut forget: Option<String> = None;
        let mut paste = false;
        common::page(ctx.viewport.screen, tm, ui, 0.18, |ui, _| {
            let hd = widgets::header(ui, safe, "Multiplayer", widgets::enter(t, 0));
            if hd.back.clicked() {
                feedback::back(ctx);
                out = Transition::Pop;
            }
            let body = hd.body;
            let gap = S3;
            let portrait = common::Layout::new(safe).portrait;
            let w = if portrait { body.width() } else { (body.width() - gap * 2.0) / 3.0 };
            let has_keeps = !list.is_empty();
            // Without saved Keeps the three cards keep their full size and the invite strip sits
            // under them; with some, the cards shrink to make room for the list.
            let card_h = if portrait || has_keeps { MINI_CARD_H } else { (body.height() - INVITE_BAR_H - gap - 8.0).min(212.0) };
            let cards_h = if portrait { 3.0 * card_h + 2.0 * gap } else { card_h };
            let invite_h = if portrait { 114.0 } else { INVITE_BAR_H };
            let block_h = if has_keeps { body.height() } else { cards_h + gap + invite_h };
            let y = body.top() + if has_keeps { 0.0 } else { (body.height() - block_h) / 2.0 - 4.0 };
            for (i, kind) in [OnlineKind::Host, OnlineKind::Join, OnlineKind::Nearby].into_iter().enumerate() {
                let rect = Rect::from_min_size(pos2(body.left() + if portrait { 0.0 } else { i as f32 * (w + gap) }, y + if portrait { i as f32 * (card_h + gap) } else { 0.0 }), vec2(w, card_h));
                let foot = match kind {
                    OnlineKind::Host => "Up to 8 racers",
                    OnlineKind::Join => "6-letter code",
                    OnlineKind::Nearby => "Same Wi-Fi",
                };
                let e = widgets::enter(t, i + 1);
                let clicked = widgets::staged(ui, e, vec2(0.0, 40.0), |ui, off| {
                    if portrait || has_keeps {
                        mini_card(ui, rect.translate(off), &format!("mp-{i}"), kind.title(), foot, kind.icon(), i == 0, tm + i as f32)
                    } else {
                        big_card(ui, rect.translate(off), &format!("mp-{i}"), kind.title(), kind.blurb(), foot, kind.icon(), i == 0, tm + i as f32)
                    }
                });
                if clicked {
                    feedback::select(ctx);
                    out = open(ctx, kind);
                }
            }
            // ── Join with invite ──
            let bar = Rect::from_min_size(pos2(body.left(), y + cards_h + gap), vec2(body.width(), invite_h));
            let e = widgets::enter(t, 4);
            widgets::staged(ui, e, vec2(0.0, 24.0), |ui, off| {
                let bar = bar.translate(off);
                let p = ui.painter().clone();
                widgets::well(&p, bar, 14.0);
                p.text(bar.left_top() + vec2(16.0, 20.0), Align2::LEFT_CENTER, "Join with invite", theme::title(20.0), theme::TEXT);
                let g = p.layout("Tap a stampede://keep link in a message, or copy it and paste it here.".into(), theme::ui_medium(11.5), theme::TEXT_DIM, bar.width() - if portrait { 32.0 } else { 200.0 });
                p.galley(bar.min + vec2(16.0, 32.0), g, theme::TEXT_DIM);
                let btn = Rect::from_min_size(pos2(bar.right() - 160.0 - 8.0, bar.top() + if portrait { 68.0 } else { 8.0 }), vec2(160.0, 40.0));
                if widgets::button(ui, btn, "mp-paste-invite", &Btn::primary("Paste invite").size(16.0)).clicked() {
                    paste = true;
                }
            });
            // ── My Keeps ──
            if has_keeps {
                let head_y = bar.bottom() + gap + 8.0;
                let p = ui.painter().clone();
                p.text(pos2(body.left() + 4.0, head_y), Align2::LEFT_CENTER, "My Keeps", theme::title(20.0), theme::SAND);
                let area = Rect::from_min_max(pos2(body.left(), head_y + 16.0), body.right_bottom());
                if area.height() < KEEP_ROW_H * 0.6 {
                    return;
                }
                let keep_h = if portrait { 96.0 } else { KEEP_ROW_H };
                let step = keep_h + S2;
                let max_scroll = (list.len() as f32 * step - S2 - area.height()).max(0.0);
                let bg = ui.interact(area, egui::Id::new("mp-keeps-scroll"), egui::Sense::drag());
                if bg.dragged() {
                    scroll -= bg.drag_delta().y;
                }
                scroll = (scroll - ui.input(|i| i.smooth_scroll_delta.y)).clamp(0.0, max_scroll);
                let clip = ui.painter().with_clip_rect(area);
                let old_clip = ui.clip_rect();
                ui.set_clip_rect(old_clip.intersect(area));
                for (i, keep) in list.iter().enumerate() {
                    let row = Rect::from_min_size(pos2(area.left(), area.top() + i as f32 * step - scroll), vec2(area.width(), keep_h));
                    if row.bottom() < area.top() || row.top() > area.bottom() {
                        continue;
                    }
                    widgets::well(&clip, row, 12.0);
                    let asking = confirm.as_deref() == Some(keep.host_key.as_str());
                    let btn_h = 40.0;
                    let by = row.top() + if portrait { 50.0 } else { (KEEP_ROW_H - btn_h) / 2.0 };
                    let (bw, right) = (96.0, row.right() - 8.0);
                    // Show controls only when the whole button fits inside the list.
                    let buttons_visible = by >= area.top() && by + btn_h <= area.bottom();
                    if asking {
                        clip.text(row.left_top() + vec2(16.0, 18.0), Align2::LEFT_CENTER, &keep.name, theme::title(20.0), theme::TEXT);
                        clip.text(row.left_top() + vec2(16.0, 38.0), Align2::LEFT_CENTER, "Forget this Keep? You will need a new invite to return.", theme::ui_medium(11.5), Color32::from_rgb(0xFF, 0xB0, 0x8A));
                        if buttons_visible {
                            if widgets::button(ui, Rect::from_min_size(pos2(right - bw, by), vec2(bw, btn_h)), &format!("mp-keep-sure-{i}"), &Btn::primary("Forget").size(15.0)).clicked() {
                                forget = Some(keep.host_key.clone());
                            }
                            if widgets::button(ui, Rect::from_min_size(pos2(right - bw * 2.0 - S2, by), vec2(bw, btn_h)), &format!("mp-keep-keep-{i}"), &Btn::new("Cancel").size(15.0)).clicked() {
                                confirm = None;
                            }
                        }
                        continue;
                    }
                    let main = Rect::from_min_max(row.min, pos2(if portrait { row.right() } else { right - bw * 2.0 - S2 - S2 }, if portrait { row.top() + 48.0 } else { row.bottom() })).intersect(area);
                    let hit = ui.interact(main, egui::Id::new(("mp-keep-row", i)), egui::Sense::click_and_drag());
                    if hit.dragged() {
                        scroll = (scroll - hit.drag_delta().y).clamp(0.0, max_scroll);
                    }
                    if hit.clicked() {
                        rejoin = Some(keep.clone());
                    }
                    let name = art::fit_size(&clip, &keep.name, theme::title, 20.0, main.width() - 24.0);
                    clip.text(row.left_top() + vec2(16.0, 18.0), Align2::LEFT_CENTER, &keep.name, theme::title(name), theme::TEXT);
                    clip.text(row.left_top() + vec2(16.0, 38.0), Align2::LEFT_CENTER, format!("last played {} \u{b7} {}", keeps::ago(keep.last_joined, now), keeps::keep_id(&keep.host_key)), theme::ui_medium(11.5), theme::TEXT_DIM);
                    if buttons_visible {
                        if widgets::button(ui, Rect::from_min_size(pos2(right - bw, by), vec2(bw, btn_h)), &format!("mp-keep-join-{i}"), &Btn::primary("Join").size(16.0)).clicked() {
                            rejoin = Some(keep.clone());
                        }
                        if widgets::button(ui, Rect::from_min_size(pos2(right - bw * 2.0 - S2, by), vec2(bw, btn_h)), &format!("mp-keep-forget-{i}"), &Btn::ghost("Forget").size(15.0)).clicked() {
                            confirm = Some(keep.host_key.clone());
                        }
                    }
                }
                ui.set_clip_rect(old_clip);
                if max_scroll > 0.0 {
                    // a thin scroll bar at the right edge
                    let track = Rect::from_min_max(pos2(body.right() + 3.0, area.top()), pos2(body.right() + 6.0, area.bottom()));
                    let h = (track.height() * area.height() / (area.height() + max_scroll)).max(24.0);
                    let ty = track.top() + (track.height() - h) * (scroll / max_scroll);
                    ui.painter().rect_filled(Rect::from_min_size(pos2(track.left(), ty), vec2(track.width(), h)), 2.0, rgba(255, 255, 255, 0.28));
                }
            }
        });
        self.scroll = scroll;
        self.confirm = confirm;
        if let Some(host_key) = forget {
            feedback::select(ctx);
            self.confirm = None;
            if let Err(e) = keeps::forget_saved(ctx.data_dir, &host_key) {
                ctx.say(format!("Couldn't forget that Keep: {e}"));
            }
            if self.sample {
                if let Some(l) = self.keeps.as_mut() {
                    keeps::forget(l, &host_key);
                }
            } else {
                self.reload = true;
            }
        }
        if let Some(keep) = rejoin {
            feedback::select(ctx);
            // The row is only for drawing: the secret to dial is read from the file at this
            // moment, so a Keep joined by a newer invite is never dialled with the old one.
            let keep = self.current(ctx.data_dir, keep);
            out = match (super::account::gate(ctx), keep.as_ref().and_then(Keep::keys)) {
                (Some(gate), _) => gate,
                (None, Some((host_key, secret))) => self.push(Box::new(JoinScreen::rejoining(host_key, secret))),
                (None, None) if keep.is_none() => {
                    ctx.say("That Keep is no longer in My Keeps.");
                    Transition::None
                }
                (None, None) => {
                    ctx.say(keeps::NOT_AN_INVITE);
                    Transition::None
                }
            };
        }
        if paste {
            feedback::select(ctx);
            out = self.paste_invite(ctx);
        }
        out
    }
}

impl MultiplayerScreen {
    /// `row` as My Keeps holds it now (`None`: it was forgotten meanwhile). The sample list
    /// of the screenshots has no file behind it.
    fn current(&mut self, data_dir: &std::path::Path, row: Keep) -> Option<Keep> {
        if self.sample {
            return Some(row);
        }
        let fresh = keeps::load(data_dir).into_iter().find(|k| k.host_key.eq_ignore_ascii_case(&row.host_key));
        if fresh.as_ref().map(|k| &k.secret) != Some(&row.secret) {
            // The list on screen is out of date: draw the file's next frame.
            self.reload = true;
        }
        fresh
    }

    /// Paste invite: read the clipboard and join the Keep it names.
    fn paste_invite(&mut self, ctx: &mut Ctx) -> Transition {
        match ctx.platform.read_clipboard().as_deref().and_then(keeps::invite_in_text) {
            None => {
                ctx.say(keeps::NO_INVITE_ON_CLIPBOARD);
                Transition::None
            }
            Some(JoinTarget::BadInvite) => {
                ctx.say(keeps::NOT_AN_INVITE);
                Transition::None
            }
            Some(target) => {
                if let Some(gate) = super::account::gate(ctx) {
                    return gate;
                }
                self.push(Box::new(JoinScreen::with_target(target)))
            }
        }
    }
}

/// A card in the compact row above My Keeps: the same look as the big cards
/// without the blurb.
#[allow(clippy::too_many_arguments)]
fn mini_card(ui: &mut egui::Ui, rect: Rect, salt: &str, title: &str, foot: &str, icon: Icon, hero: bool, tm: f32) -> bool {
    let id = egui::Id::new(("stampede-minicard", salt));
    let resp = ui.interact(rect, id, egui::Sense::click());
    let (scale, sink) = widgets::press_fx(ui, id, &resp);
    let rect = Rect::from_center_size(rect.center() + vec2(0.0, sink * 2.0), rect.size() * scale);
    let p = ui.painter().clone();
    let r = 18.0;
    widgets::soft_shadow(&p, rect, r, 6.0, 16.0, 0.5);
    let (top, bot) = if hero { (Color32::from_rgb(0xFF, 0xB8, 0x44), Color32::from_rgb(0xD8, 0x5A, 0x16)) } else { (Color32::from_rgb(0x7E, 0x47, 0xAC), Color32::from_rgb(0x44, 0x1C, 0x6E)) };
    widgets::rrect_gradient(&p, rect, r, top, bot);
    p.rect_stroke(rect, egui::CornerRadius::same(r as u8), egui::Stroke::new(2.5_f32, theme::OUTLINE), egui::StrokeKind::Inside);
    p.hline((rect.left() + r)..=(rect.right() - r), rect.top() + 3.0, egui::Stroke::new(1.6_f32, rgba(255, 255, 255, 0.35)));
    let mc = pos2(rect.left() + 14.0 + 22.0, rect.center().y);
    p.circle_filled(mc, 22.0, rgba(10, 3, 22, 0.30));
    p.circle_stroke(mc, 22.0, egui::Stroke::new(2.0_f32, rgba(255, 255, 255, 0.22)));
    crate::ui::icons::draw(&p, icon, mc + vec2(0.0, (tm * 1.8).sin() * 1.0), 34.0, if hero { Color32::WHITE } else { theme::SAND });
    let tx = mc.x + 22.0 + 12.0;
    let size = art::fit_size(&p, title, theme::title, 22.0, rect.right() - 10.0 - tx);
    art::outlined(&p, pos2(tx, rect.center().y - 9.0), Align2::LEFT_CENTER, title, theme::title(size), Color32::WHITE, theme::OUTLINE, (size * 0.085).max(1.0), 1.5);
    p.text(pos2(tx, rect.center().y + 14.0), Align2::LEFT_CENTER, foot, theme::ui_medium(11.5), if hero { Color32::from_rgb(0x4A, 0x1A, 0x08) } else { theme::SAND });
    resp.clicked()
}

/// Open the screen behind a card. Hosting opens the room first.
fn open(ctx: &mut Ctx, kind: OnlineKind) -> Transition {
    if let Some(gate) = super::account::gate(ctx) { return gate; }
    match kind {
        OnlineKind::Host => match OnlineSession::host(ctx.data_dir, &ctx.profile.name, ctx.profile.character, HostConfig::default()) {
            Ok(s) => {
                ctx.online.put(s);
                Transition::Push(Box::new(LobbyScreen::new()))
            }
            Err(e) => {
                ctx.say(e);
                Transition::None
            }
        },
        OnlineKind::Join => Transition::Push(Box::new(JoinScreen::new())),
        OnlineKind::Nearby => Transition::Push(Box::new(NearbyScreen::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("stampede-mp-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_join_tap_dials_the_secret_in_the_file_not_the_one_on_the_screen() {
        let d = dir("fresh");
        keeps::record_join(&d, &[7; 32], &[1; 32], Some("Fray test host"), keeps::unix_now()).unwrap();
        let mut screen = MultiplayerScreen::new();
        let stale = keeps::load(&d).remove(0);
        // Another screen joins the same Keep again by a newer invite while this list is on screen.
        keeps::record_join(&d, &[7; 32], &[2; 32], Some("Fray test host"), keeps::unix_now()).unwrap();
        let fresh = screen.current(&d, stale.clone()).expect("still saved");
        assert_eq!(fresh.keys(), Some(([7; 32], [2; 32])));
        assert_ne!(fresh.secret, stale.secret);
        assert!(screen.reload, "the list is drawn again from the file");
        // Forgotten meanwhile: nothing to dial.
        keeps::forget_saved(&d, &stale.host_key).unwrap();
        assert!(screen.current(&d, stale).is_none());
        let _ = std::fs::remove_dir_all(d);
    }
}
