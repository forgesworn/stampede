//! Host migration on screen (item 7b): choosing a backup, the backup's consent,
//! and the status of a protected room. Everything here reads a
//! [`MigrationState`] (or the lobby's seats) and reports what the person
//! pressed; the screens call the session's `choose_backup`, `answer_consent`
//! and `cancel_arming` with it. No migration object is held here.

use super::common;
use super::lobby;
use crate::online::MigrationState;
use crate::ui::art;
use crate::ui::widgets::{self, Btn, S2, S3, S4, TAP_MIN};
use crate::ui::{icons, theme};
use egui::epaint::text::LayoutJob;
use egui::{pos2, vec2, Align2, Color32, Painter, Rect};
use race_session::SeatView;
use std::sync::Arc;

/// Just after a takeover, on the phones that kept racing.
pub const TAKEOVER: &str = "The host's phone dropped out — the backup phone took over. Race on!";
/// The same, on the phone that took over.
pub const TAKEOVER_HERE: &str = "The host's phone dropped out — your phone took over. Race on!";
/// Either, in one short row (the results header).
pub const TAKEOVER_SHORT: &str = "Host's phone dropped out — race on!";
/// After that notice: the room can't take another host loss.
pub const NO_BACKUP: &str = "No backup now";

/// What a backup gets, said to the host before it picks.
pub const DISCLOSURE: &str = "The backup phone receives the full game state, including hidden information, so it can take over if your phone drops out.";

/// The same, said to the person asked (whose phone would take over the host's).
pub const DISCLOSURE_ASKED: &str = "The backup phone receives the full game state, including hidden information, so it can take over if the host's phone drops out.";

/// "Protect this room?" on a room with an online code: what switching it to
/// this Wi-Fi means (a backup can only protect a room on one Wi-Fi).
pub const NEARBY_ONLY: &str = "Only players on this Wi-Fi can join a protected room. The online code will stop working.";

/// The same sheet when people are in the room: it reopens, so they join again.
pub const REJOIN: &str = "Everyone here now will need to join again: on their phone, open Nearby and join your race.";

/// A nearby-only room's host panel, in place of the room code, Copy and Share.
pub const NEARBY_JOIN: &str = "Same Wi-Fi only — friends join from Nearby";

/// The egui area of "Protect this room?" (tests look for it).
pub const SHEET_ID: &str = "migration-nearby-sheet";

/// How a status reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Info,
}

/// What a state looks like on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// Nothing to say.
    Hidden,
    /// A small status chip. `detail` holds the reasons, or (a protected
    /// room's host) who the switch left out.
    Chip {
        tone: Tone,
        head: String,
        detail: Vec<String>,
    },
    /// The room stopped: an overlay with the reason.
    Card { title: String, body: String },
}

/// The words for `state` on this device. `is_host`: this phone opened the room.
pub fn view(state: &MigrationState, is_host: bool) -> View {
    let chip = |tone, head: &str, detail: Vec<String>| View::Chip {
        tone,
        head: head.into(),
        detail,
    };
    match state {
        MigrationState::Idle | MigrationState::ConsentRequested => View::Hidden,
        MigrationState::Asking | MigrationState::Arming => chip(
            Tone::Info,
            if is_host {
                progress(state)
            } else {
                "Setting up protection…"
            },
            vec![],
        ),
        MigrationState::Armed { unprotected } => chip(
            Tone::Good,
            "Protected",
            if is_host {
                unprotected.iter().map(|u| plain_line(u)).collect()
            } else {
                vec![]
            },
        ),
        MigrationState::Unprotected { reason } => {
            chip(Tone::Warn, "Not protected", vec![plain_reason(reason)])
        }
        MigrationState::Refused { reason } => {
            chip(Tone::Warn, "Can't protect this room", vec![plain_reason(reason)])
        }
        MigrationState::Paused { reason } => View::Card {
            title: "Room paused".into(),
            body: plain_reason(reason),
        },
        MigrationState::Recovering => View::Card {
            title: "Host lost — reconnecting…".into(),
            body: String::new(),
        },
        // The phone that took over hosts now; the others don't know its
        // name here (the state carries none), so it is "the backup phone".
        MigrationState::Resumed => chip(
            Tone::Good,
            if is_host { TAKEOVER_HERE } else { TAKEOVER },
            vec![],
        ),
        // One takeover per session: a second host loss ends the race.
        MigrationState::UnprotectedAfterTakeover => chip(Tone::Warn, NO_BACKUP, vec![]),
    }
}

/// A "Name: reason" line of who a protected room left out, in plain words.
fn plain_line(line: &str) -> String {
    match line.split_once(": ") {
        Some((name, reason)) => format!("{name}: {}", plain_reason(reason)),
        None => plain_reason(line),
    }
}

/// A reason from the session, in the words a player reads. The session's
/// own wording ("arming", "LAN", "standby"...) stays in the state and the
/// logs; only what is drawn changes. Unknown reasons with technical words
/// read as a plain failure; plain ones are kept.
pub fn plain_reason(reason: &str) -> String {
    let reason = reason.trim();
    // A switch that failed and fell back to the lobby: the cause, then where it went.
    if let Some(cause) = reason.strip_suffix(" Rejoining the lobby unarmed.") {
        return format!("{} Back in the lobby.", plain_reason(cause));
    }
    if reason == "Rejoining the lobby unarmed" {
        return "Back in the lobby".into();
    }
    let wifi = "A backup needs everyone on the same Wi-Fi.";
    let before = |marker: &str| reason.split_once(marker).map(|(name, _)| name.trim().to_string());
    if reason.contains("opened with an online code") {
        return "This room has an online code. Only a room on one Wi-Fi can have a backup.".into();
    }
    if let Some(rest) = reason.strip_prefix("Arming is refused while Bluetooth players are in the room (") {
        let name = rest.trim_end_matches(").").trim_end_matches(')');
        return if name == "a joiner" {
            format!("Someone is joining by Bluetooth. {wifi}")
        } else {
            format!("{name} joined by Bluetooth. {wifi}")
        };
    }
    if reason.contains("over a Bluetooth link") {
        return format!("This phone joined by Bluetooth. {wifi}");
    }
    if reason.contains("this device did not join over the local network") {
        return "This phone isn't on the host's Wi-Fi.".into();
    }
    if let Some(rest) = reason.strip_prefix("Arming is LAN-only: ") {
        if let Some(name) = rest.strip_suffix(" did not join over the local network.") {
            return format!("{name} isn't on the same Wi-Fi.");
        }
        return wifi.into();
    }
    match reason {
        "Arming is only possible in the lobby." => return "A room can only be protected in the lobby.".into(),
        "Arming is already in progress." => return "The backup phone is already being set up.".into(),
        "This room is already armed." => return "This room is already protected.".into(),
        "Arming needs a room opened on this phone's network." => {
            return "Only a room this phone hosts on its Wi-Fi can be protected.".into()
        }
        "Arming needs a room that checks every player's persona." => {
            return "Only a room that checks who everyone is can be protected.".into()
        }
        "The room is not open." => return "The room isn't open yet.".into(),
        "The chosen backup is not a player in this room." => return "That player isn't in the room any more.".into(),
        "The host cancelled arming." => return "The host cancelled.".into(),
        "A player joined while arming." => return "A player joined while the backup phone was being set up.".into(),
        "The armed room is gone." => return "The protected room closed.".into(),
        "This phone could not rejoin the armed room. It stays the room's backup." => {
            return "This phone couldn't rejoin the room. It stays the backup.".into()
        }
        _ => {}
    }
    if reason.contains("did not settle for arming") || reason.contains("held for arming") {
        return "The lobby was busy. Try again.".into();
    }
    if let Some(name) = reason.strip_prefix("No capability ACK from ").and_then(|r| r.split_once(" within ").map(|x| x.0)) {
        return format!("{name}'s phone didn't answer in time.");
    }
    for marker in [" did not arm within", " did not consent within"] {
        if let Some(name) = before(marker) {
            return format!("{name}'s phone didn't answer in time.");
        }
    }
    for marker in [" did not switch to the armed room", " left the lobby link but did not reconnect"] {
        if let Some(name) = before(marker) {
            return format!("{name}'s phone didn't reconnect in time.");
        }
    }
    if let Some(name) = before(" could not join the armed room") {
        return format!("{name}'s phone couldn't reconnect.");
    }
    if let Some(name) = reason.strip_suffix(" left during arming.") {
        return format!("{name} left while the backup phone was being set up.");
    }
    if reason.starts_with("This phone can no longer take over") {
        return "This phone can no longer take over.".into();
    }
    if reason.starts_with("The room is paused (") {
        return "The room is paused".into();
    }
    const TECHNICAL: [&str; 22] = [
        "arming", "Arming", "armed", "Armed", "standby", "Standby", "promot", "ACK", "descriptor", "pair link",
        "bundle", "certificate", "checkpoint", "Activation", "capability", "LAN", "consent", "offer", "image",
        "listener", "route", "switch",
    ];
    if TECHNICAL.iter().any(|w| reason.contains(w)) {
        return "The backup phone couldn't be set up.".into();
    }
    reason.to_string()
}

/// The host's progress line while the backup is asked and the room is set up.
pub fn progress(state: &MigrationState) -> &'static str {
    match state {
        MigrationState::Asking => "Waiting for the backup to answer…",
        _ => "Setting up the backup…",
    }
}

/// A chip's lines: the heading and the lines under it. A protected room's
/// host sees who is left out (`expanded`), or just how many.
pub fn chip_lines(
    tone: Tone,
    head: &str,
    detail: &[String],
    expanded: bool,
) -> (String, Vec<String>) {
    if tone == Tone::Good && !detail.is_empty() {
        if !expanded {
            return (format!("{head} · {} not covered", detail.len()), vec![]);
        }
        let mut lines = vec!["Not protected:".to_string()];
        if detail.len() <= 2 {
            lines.extend(detail.iter().cloned());
        } else {
            lines.push(detail[0].clone());
            lines.push(format!("and {} more", detail.len() - 1));
        }
        return (head.to_string(), lines);
    }
    (head.to_string(), detail.to_vec())
}

fn tone_color(t: Tone) -> Color32 {
    match t {
        Tone::Good => theme::MINT,
        Tone::Warn => theme::GOLD,
        Tone::Info => theme::TEXT_DIM,
    }
}

/// Text in `width` that keeps to `rows` rows: the font shrinks to `min`, then the
/// last row is cut with an ellipsis.
pub(crate) fn wrapped(
    p: &Painter,
    text: &str,
    mk: fn(f32) -> egui::FontId,
    size: f32,
    min: f32,
    color: Color32,
    width: f32,
    rows: usize,
) -> Arc<egui::Galley> {
    let mut s = size;
    loop {
        let g = p.layout(text.to_string(), mk(s), color, width);
        if g.rows.len() <= rows || s <= min {
            if g.rows.len() <= rows {
                return g;
            }
            let mut job = LayoutJob::simple(text.to_string(), mk(s), color, width);
            job.wrap.max_rows = rows;
            return p.layout_job(job);
        }
        s -= 0.5;
    }
}

// ── the header band ─────────────────────────────────────────────────────────

/// The header row's free right-hand side, from `left` to the safe edge (the title
/// sits at the other end).
pub fn band(safe: Rect, left: f32) -> Rect {
    let top = safe.top() + S2 + 2.0;
    Rect::from_min_max(
        pos2(left.min(safe.right() - S4 - 140.0), top),
        pos2(safe.right() - S4, top + TAP_MIN),
    )
}

const PROTECT_W: f32 = 150.0;
const CANCEL_W: f32 = 84.0;
const BTN_H: f32 = 34.0;

fn action_button(band: Rect, w: f32) -> Rect {
    Rect::from_min_size(
        pos2(band.right() - w, band.center().y - BTN_H / 2.0),
        vec2(w, BTN_H),
    )
}

/// The lobby's "Protect this room" button (tests tap it here).
pub fn protect_button(safe: Rect) -> Rect {
    action_button(band(safe, 0.0), PROTECT_W)
}

/// A status chip, right-aligned in `band` and growing downward. `expanded`: room
/// for the unprotected list; `rows`: lines a reason may take.
/// The heading takes up to two rows (`rows`, at most 2). Where it was drawn.
pub fn chip(p: &Painter, band: Rect, view: &View, expanded: bool, rows: usize) -> Option<Rect> {
    let View::Chip { tone, head, detail } = view else {
        return None;
    };
    let (head, lines) = chip_lines(*tone, head, detail, expanded);
    let max_w = band.width().min(340.0);
    let inner = max_w - 2.0 * S3 - 12.0;
    let hg = wrapped(p, &head, theme::ui_bold, 12.5, 9.5, theme::TEXT, inner, rows.clamp(1, 2));
    let line_rows = if *tone == Tone::Good { 1 } else { rows };
    let gs: Vec<_> = lines
        .iter()
        .map(|l| {
            wrapped(
                p,
                l,
                theme::ui_medium,
                10.5,
                9.0,
                theme::TEXT_DIM,
                inner,
                line_rows,
            )
        })
        .collect();
    let w =
        (gs.iter().map(|g| g.size().x).fold(hg.size().x, f32::max) + 2.0 * S3 + 12.0).min(max_w);
    let h = 16.0 + hg.size().y + gs.iter().map(|g| g.size().y + 2.0).sum::<f32>();
    let rect = Rect::from_min_size(pos2(band.right() - w, band.top()), vec2(w, h));
    p.rect_filled(rect, 12.0, theme::BG.gamma_multiply(0.92));
    p.rect_stroke(
        rect,
        12.0,
        egui::Stroke::new(1.5_f32, tone_color(*tone)),
        egui::StrokeKind::Inside,
    );
    p.circle_filled(
        pos2(rect.left() + S3 + 3.0, rect.top() + 8.0 + hg.size().y / 2.0),
        3.5,
        tone_color(*tone),
    );
    let mut y = rect.top() + 8.0;
    let x = rect.left() + S3 + 12.0;
    let hh = hg.size().y;
    p.galley(pos2(x, y), hg, theme::TEXT);
    y += hh + 2.0;
    for g in gs {
        let gh = g.size().y;
        p.galley(pos2(x, y), g, theme::TEXT_DIM);
        y += gh + 2.0;
    }
    Some(rect)
}

/// What the header cluster reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cluster {
    /// Open the backup picker.
    Protect,
    /// Abandon arming.
    Cancel,
}

/// The lobby's migration cluster in the header band. The host's "Protect this
/// room" (`can_protect`: nothing armed and the lobby is open), or progress with
/// Cancel while arming; otherwise the state's status chip.
pub fn cluster(
    ui: &mut egui::Ui,
    band: Rect,
    state: &MigrationState,
    is_host: bool,
    can_protect: bool,
) -> Option<Cluster> {
    let p = ui.painter().clone();
    let mut out = None;
    let arming = matches!(state, MigrationState::Asking | MigrationState::Arming);
    if is_host && arming {
        let cancel = action_button(band, CANCEL_W);
        if widgets::button(
            ui,
            cancel,
            "migration-cancel",
            &Btn::new("Cancel").size(13.0),
        )
        .clicked()
        {
            out = Some(Cluster::Cancel);
        }
        let text = Rect::from_min_max(band.min, pos2(cancel.left() - S2, band.bottom()));
        let g = wrapped(
            &p,
            progress(state),
            theme::ui_bold,
            12.5,
            9.0,
            theme::TEXT,
            text.width(),
            2,
        );
        p.galley(
            pos2(
                text.right() - g.size().x,
                text.center().y - g.size().y / 2.0,
            ),
            g,
            theme::TEXT,
        );
        return out;
    }
    if is_host && can_protect {
        let b = action_button(band, PROTECT_W);
        if widgets::button(
            ui,
            b,
            "migration-protect",
            &Btn::primary("Protect this room")
                .icon(icons::Icon::Check)
                .size(13.0),
        )
        .clicked()
        {
            out = Some(Cluster::Protect);
        }
        // A failed attempt says why, beside the button.
        if let View::Chip { detail, .. } = view(state, true) {
            if let Some(reason) = detail.first() {
                let text = Rect::from_min_max(band.min, pos2(b.left() - S2, band.bottom()));
                let g = wrapped(
                    &p,
                    reason,
                    theme::ui_medium,
                    11.0,
                    9.0,
                    theme::GOLD,
                    text.width(),
                    2,
                );
                p.galley(
                    pos2(
                        text.right() - g.size().x,
                        text.center().y - g.size().y / 2.0,
                    ),
                    g,
                    theme::GOLD,
                );
            }
        }
        return out;
    }
    let _ = chip(&p, band, &view(state, is_host), true, 2);
    out
}

// ── overlays ────────────────────────────────────────────────────────────────

/// The room stopped (paused, or reconnecting): a card over a dimmed screen.
/// Not interactive: leaving still works underneath.
pub fn card(ui: &egui::Context, screen: Rect, view: &View) {
    let View::Card { title, body } = view else {
        return;
    };
    let p = ui.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("migration-card"),
    ));
    p.rect_filled(screen, 0.0, Color32::from_black_alpha(150));
    let w = (screen.width() - 2.0 * S4).min(420.0);
    let inner = w - 2.0 * S4 - 8.0;
    let ts = art::fit_size(&p, title, theme::title, 26.0, inner);
    let tg = p.layout_no_wrap(title.clone(), theme::title(ts), theme::TEXT);
    let bg = (!body.is_empty()).then(|| {
        wrapped(
            &p,
            body,
            theme::ui_medium,
            15.0,
            11.0,
            theme::TEXT_DIM,
            inner,
            4,
        )
    });
    let h = 2.0 * S4 + 8.0 + tg.size().y + bg.as_ref().map_or(0.0, |g| g.size().y + S3);
    let rect = Rect::from_center_size(screen.center(), vec2(w, h));
    widgets::panel_solid(&p, rect, 22.0);
    p.circle_filled(
        pos2(rect.center().x, rect.top() + S4 + 2.0),
        3.5,
        theme::GOLD,
    );
    let mut y = rect.top() + S4 + 8.0;
    p.galley(
        pos2(rect.center().x - tg.size().x / 2.0, y),
        tg.clone(),
        theme::TEXT,
    );
    y += tg.size().y + S3;
    if let Some(g) = bg {
        p.galley(
            pos2(rect.center().x - g.size().x / 2.0, y),
            g,
            theme::TEXT_DIM,
        );
    }
}

/// Where the consent prompt's parts sit (tests tap the buttons here).
pub struct Prompt {
    pub card: Rect,
    pub no: Rect,
    pub yes: Rect,
}

pub fn prompt_layout(screen: Rect) -> Prompt {
    let card = Rect::from_center_size(screen.center(), vec2(screen.width().min(520.0), 250.0));
    let bw = (card.width() - 48.0 - S3) / 2.0;
    let by = card.bottom() - 18.0 - 44.0;
    Prompt {
        card,
        no: Rect::from_min_size(pos2(card.left() + 24.0, by), vec2(bw, 44.0)),
        yes: Rect::from_min_size(pos2(card.left() + 24.0 + bw + S3, by), vec2(bw, 44.0)),
    }
}

/// "Be the backup?": the whole screen, for the device the host asked. `Some(true)`
/// = yes, `Some(false)` = no thanks, `None` = still asking.
pub fn consent_prompt(ui: &egui::Context, screen: Rect, host: &str) -> Option<bool> {
    let l = prompt_layout(screen);
    let id = egui::Id::new("migration-consent");
    let mut answer = None;
    egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ui, |ui| {
            ui.set_min_size(screen.size());
            // Nothing underneath takes a tap while the question is up.
            ui.interact(screen, id.with("block"), egui::Sense::click_and_drag());
            let p = ui.painter().clone();
            p.rect_filled(screen, 0.0, theme::BG);
            widgets::panel_solid(&p, l.card, 22.0);
            let c = l.card;
            p.text(
                pos2(c.center().x, c.top() + 34.0),
                Align2::CENTER_CENTER,
                "Be the backup?",
                theme::title(26.0),
                theme::TEXT,
            );
            let who = format!("{host}'s room");
            let ws = art::fit_size(&p, &who, theme::ui_bold, 14.0, c.width() - 48.0);
            p.text(
                pos2(c.center().x, c.top() + 62.0),
                Align2::CENTER_CENTER,
                who,
                theme::ui_bold(ws),
                theme::SAND,
            );
            let g = wrapped(
                &p,
                DISCLOSURE_ASKED,
                theme::ui_medium,
                14.0,
                11.0,
                theme::TEXT_DIM,
                c.width() - 48.0,
                5,
            );
            p.galley(pos2(c.left() + 24.0, c.top() + 82.0), g, theme::TEXT_DIM);
            if widgets::button(ui, l.no, "migration-consent-no", &Btn::new("No thanks")).clicked() {
                answer = Some(false);
            }
            if widgets::button(
                ui,
                l.yes,
                "migration-consent-yes",
                &Btn::primary("Be the backup"),
            )
            .clicked()
            {
                answer = Some(true);
            }
        });
    answer
}

/// A chip as the race and the results show it: the heading only (a protected
/// room's host still sees how many are left out). `short`: the takeover in
/// one short row.
fn headline(v: &View, short: bool) -> View {
    let View::Chip { tone, head, detail } = v else {
        return v.clone();
    };
    let head = if short && (head == TAKEOVER || head == TAKEOVER_HERE) { TAKEOVER_SHORT.to_string() } else { head.clone() };
    let detail = if *tone == Tone::Good { detail.clone() } else { Vec::new() };
    View::Chip { tone: *tone, head, detail }
}

/// The race's chip: the heading, in up to two rows.
pub fn race_view(v: &View) -> View {
    headline(v, false)
}

/// Where the race's chip sits: right of the minimap and the place, left of
/// the Pause / Camera / Look-back buttons, above the lap and time column.
pub fn race_note_area(safe: Rect) -> Rect {
    Rect::from_min_max(pos2(safe.left() + 250.0, safe.top() + 6.0), pos2(safe.right() - 150.0, safe.top() + 60.0))
}

/// How long the race shows a status chip after it changes: then the road
/// is clear again (the lobby and the results keep saying it).
pub const GAME_NOTE_SECS: f32 = 5.0;

/// The race's status chip: shown for [`GAME_NOTE_SECS`] each time it changes.
#[derive(Clone, Debug, Default)]
pub struct GameNote {
    shown: Option<View>,
    age: f32,
}

impl GameNote {
    /// One frame `dt` long: whether `v`'s chip is drawn now.
    pub fn step(&mut self, v: &View, dt: f32) -> bool {
        let chip = matches!(v, View::Chip { .. }).then(|| v.clone());
        if chip != self.shown {
            self.shown = chip;
            self.age = 0.0;
        } else {
            self.age += dt;
        }
        self.shown.is_some() && self.age < GAME_NOTE_SECS
    }
}

/// How long the host's wait for the backup's receipts must last before the
/// card says so: the short waits phones see (about half a second, again and
/// again) would only flicker it.
pub const BEHIND_CARD_DELAY_MS: u64 = 1000;

/// Holds back the card for a short wait for the backup ([`BEHIND_CARD_DELAY_MS`]).
/// A screen keeps one and reads its state through [`BehindDelay::state`] each
/// frame; only what is drawn changes, the session's state does not.
#[derive(Clone, Debug, Default)]
pub struct BehindDelay {
    /// When the wait now going on was first seen.
    since_ms: Option<u64>,
    /// The protected state the wait began from, said again while it is held
    /// back. Kept through a lost backup (the same arming) and dropped once
    /// the room leaves the armed states, so a new arming starts without one.
    armed: Option<MigrationState>,
}

impl BehindDelay {
    /// `state` as the screen should draw it at `now_ms` (the wall clock): a
    /// wait for the backup that has lasted less than [`BEHIND_CARD_DELAY_MS`]
    /// reads as the protected room; everything else is `state` itself.
    pub fn state(&mut self, state: &MigrationState, now_ms: u64) -> MigrationState {
        match state {
            MigrationState::Paused { reason } if reason == crate::online::BEHIND => {
                let since = *self.since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) < BEHIND_CARD_DELAY_MS {
                    // A wait seen before any protected state (a screen opened
                    // mid-wait, as the race opens at the start) has no
                    // left-out players to say: the pause carries none. It
                    // reads as protected for nobody left out until the card or
                    // the first protected frame; showing the card at once
                    // would bring back the flicker at the start of the race.
                    return self.armed.clone().unwrap_or(MigrationState::Armed { unprotected: vec![] });
                }
            }
            MigrationState::Armed { .. } => {
                self.since_ms = None;
                self.armed = Some(state.clone());
            }
            MigrationState::Paused { reason } if reason == crate::online::BACKUP_LOST => self.since_ms = None,
            _ => {
                self.since_ms = None;
                self.armed = None;
            }
        }
        state.clone()
    }
}

/// The race: the card, or the chip for a few seconds after it changes.
#[allow(clippy::too_many_arguments)]
pub fn race_status(ui: &egui::Context, p: &Painter, screen: Rect, safe: Rect, state: &MigrationState, is_host: bool, note: &mut GameNote, dt: f32) {
    let v = view(state, is_host);
    if let View::Card { .. } = v {
        card(ui, screen, &v);
    }
    if note.step(&v, dt) {
        let _ = chip(p, race_note_area(safe), &race_view(&v), false, 2);
    }
}

/// The results: the card, or a one-row chip in the header right of the title.
/// Where the chip was drawn.
pub fn results_status(ui: &egui::Context, p: &Painter, screen: Rect, safe: Rect, state: &MigrationState, is_host: bool) -> Option<Rect> {
    let v = view(state, is_host);
    match &v {
        View::Card { .. } => {
            card(ui, screen, &v);
            None
        }
        View::Chip { .. } => chip(p, band(safe, safe.left() + 260.0), &headline(&v, true), false, 1),
        View::Hidden => None,
    }
}

/// Where "Protect this room?" puts its parts (tests tap the buttons here):
/// the consent prompt's card.
pub fn sheet_layout(screen: Rect) -> Prompt {
    prompt_layout(screen)
}

/// "Protect this room?": the room has an online code, and only a room on one
/// Wi-Fi can have a backup. `lines`: [`NEARBY_ONLY`], then what happens to the
/// people in the room. `Some(true)` = Protect (reopen the room nearby-only),
/// `Some(false)` = Not now.
pub fn nearby_sheet(ui: &egui::Context, screen: Rect, lines: &[String]) -> Option<bool> {
    let l = sheet_layout(screen);
    let id = egui::Id::new(SHEET_ID);
    let mut answer = None;
    egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ui, |ui| {
            ui.set_min_size(screen.size());
            // Nothing underneath takes a tap while the question is up.
            ui.interact(screen, id.with("block"), egui::Sense::click_and_drag());
            let p = ui.painter().clone();
            p.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
            widgets::panel_solid(&p, l.card, 22.0);
            let c = l.card;
            p.text(pos2(c.center().x, c.top() + 34.0), Align2::CENTER_CENTER, "Protect this room?", theme::title(26.0), theme::TEXT);
            let mut y = c.top() + 62.0;
            for (i, line) in lines.iter().enumerate() {
                let col = if i == 0 { theme::TEXT } else { theme::TEXT_DIM };
                let g = wrapped(&p, line, theme::ui_medium, 14.0, 11.0, col, c.width() - 48.0, 3);
                let h = g.size().y;
                if y + h > l.no.top() - S2 {
                    break;
                }
                p.galley(pos2(c.left() + 24.0, y), g, col);
                y += h + S2;
            }
            if widgets::button(ui, l.no, "migration-nearby-no", &Btn::new("Not now")).clicked() {
                answer = Some(false);
            }
            if widgets::button(ui, l.yes, "migration-nearby-yes", &Btn::primary("Protect")).clicked() {
                answer = Some(true);
            }
        });
    answer
}

// ── the backup picker ───────────────────────────────────────────────────────

/// A seated person the host can ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub seat: u8,
    pub name: String,
    pub character: u8,
}

/// The people who can be the backup: seated humans other than the host.
pub fn candidates(seats: &[SeatView]) -> Vec<Candidate> {
    seats
        .iter()
        .filter(|s| !s.bot && !s.host)
        .map(|s| Candidate {
            seat: s.id,
            name: s.name.clone(),
            character: s.character,
        })
        .collect()
}

/// What the picker reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Back,
    Seat(u8),
}

/// The picker with nobody to ask yet (it updates as people join).
pub const PICKER_EMPTY: &str = "Nobody else is here yet. On the other phone, open Nearby and join your race.";

const ROW_H: f32 = 48.0;
const LIST_TOP: f32 = 58.0;

fn pick_row(list: Rect, i: usize, scroll: f32) -> Rect {
    Rect::from_min_size(
        pos2(list.left(), list.top() + i as f32 * (ROW_H + S2) - scroll),
        vec2(list.width(), ROW_H),
    )
}

fn pick_choose(row: Rect) -> Rect {
    Rect::from_min_size(
        pos2(row.right() - 116.0, row.top() + 6.0),
        vec2(108.0, ROW_H - 12.0),
    )
}

/// Where row `i`'s Choose button sits unscrolled (tests tap it here).
pub fn pick_button(safe: Rect, i: usize) -> Rect {
    let body = Rect::from_min_max(
        pos2(safe.left() + S4, safe.top() + S2 + 2.0 + TAP_MIN + S3),
        pos2(safe.right() - S4, safe.bottom() - S4),
    );
    pick_choose(pick_row(
        Rect::from_min_max(pos2(body.left(), body.top() + LIST_TOP), body.max),
        i,
        0.0,
    ))
}

/// "Protect this room": what a backup gets, and the people to ask.
pub fn backup_picker(
    time: f32,
    screen: Rect,
    safe: Rect,
    ui: &egui::Context,
    who: &[Candidate],
    scroll: &mut f32,
) -> Option<Pick> {
    let mut out = None;
    common::page(screen, time, ui, 0.26, |ui, _| {
        let hd = widgets::header(ui, safe, "Protect this room", 1.0);
        if hd.back.clicked() {
            out = Some(Pick::Back);
        }
        let p = ui.painter().clone();
        let g = wrapped(
            &p,
            DISCLOSURE,
            theme::ui_medium,
            13.5,
            10.5,
            theme::TEXT_DIM,
            hd.body.width(),
            3,
        );
        p.galley(hd.body.min, g, theme::TEXT_DIM);
        let list = Rect::from_min_max(pos2(hd.body.left(), hd.body.top() + LIST_TOP), hd.body.max);
        if who.is_empty() {
            let g = wrapped(&p, PICKER_EMPTY, theme::ui_medium, 16.0, 11.0, theme::TEXT_DIM, list.width(), 3);
            p.galley(list.center() - g.size() * 0.5, g, theme::TEXT_DIM);
            return;
        }
        let max_scroll = (who.len() as f32 * (ROW_H + S2) - list.height()).max(0.0);
        let drag = ui.interact(
            list,
            egui::Id::new("migration-picker-scroll"),
            egui::Sense::drag(),
        );
        if drag.dragged() {
            *scroll = (*scroll - drag.drag_delta().y).clamp(0.0, max_scroll);
        }
        let clip = p.with_clip_rect(list);
        for (i, c) in who.iter().enumerate() {
            let row = pick_row(list, i, *scroll);
            if !list.contains_rect(row) {
                continue;
            }
            widgets::well(&clip, row, 12.0);
            lobby::character_chip(
                &clip,
                pos2(row.left() + 26.0, row.center().y),
                13.0,
                c.character,
            );
            let choose = pick_choose(row);
            let size = art::fit_size(
                &clip,
                &c.name,
                theme::title,
                20.0,
                choose.left() - row.left() - 60.0,
            );
            clip.text(
                pos2(row.left() + 50.0, row.center().y),
                Align2::LEFT_CENTER,
                &c.name,
                theme::title(size),
                theme::TEXT,
            );
            if widgets::button(
                ui,
                choose,
                &format!("migration-pick-{i}"),
                &Btn::primary("Choose").size(16.0),
            )
            .clicked()
            {
                out = Some(Pick::Seat(c.seat));
            }
        }
    });
    out
}

/// Staged migration overlays for the desktop capture and layout checks.
pub struct SampleScreen { kind: u8, scroll: f32 }
impl SampleScreen {
    pub fn new(kind: u8) -> Self { Self { kind, scroll: 0.0 } }
}
impl super::Screen for SampleScreen {
    fn name(&self) -> &'static str { "migration" }
    fn draw(&mut self, ctx: &mut super::Ctx, ui: &egui::Context) -> super::Transition {
        let safe = ctx.viewport.safe;
        match self.kind {
            0 => { consent_prompt(ui, safe, "Mo"); }
            1 => { backup_picker(ctx.time as f32, ctx.viewport.screen, safe, ui,
                &[Candidate { seat: 1, name: "Mo".into(), character: 1 },
                  Candidate { seat: 2, name: "Nova".into(), character: 2 }], &mut self.scroll); }
            2 => { nearby_sheet(ui, safe, &[NEARBY_ONLY.into(), REJOIN.into()]); }
            _ => card(ui, safe, &View::Card { title: "Room stopped".into(), body: "The host's phone dropped out. Open Nearby to join another game.".into() }),
        }
        super::Transition::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Insets;
    use egui::epaint::Shape;
    use egui::Event;

    const PHONES: [(f32, f32, f32); 2] = [(2400.0, 1080.0, 3.0), (2400.0, 1080.0, 2.625)];

    fn unprotected(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("Player{i}: did not join over the local network"))
            .collect()
    }

    fn states() -> Vec<MigrationState> {
        let long = "Arming is LAN-only: this room was opened with an online code, so no phone can take it over.".to_string();
        vec![
            MigrationState::Idle,
            MigrationState::Asking,
            MigrationState::ConsentRequested,
            MigrationState::Arming,
            MigrationState::Armed {
                unprotected: vec![],
            },
            MigrationState::Armed {
                unprotected: unprotected(1),
            },
            MigrationState::Armed {
                unprotected: unprotected(4),
            },
            MigrationState::Unprotected {
                reason: long.clone(),
            },
            MigrationState::Refused { reason: long },
            MigrationState::Paused {
                reason: "Backup lost — the room is paused".into(),
            },
            MigrationState::Recovering,
            MigrationState::Resumed,
            MigrationState::UnprotectedAfterTakeover,
        ]
    }

    #[test]
    fn states_read_plainly() {
        let chip = |tone, head: &str, detail: &[&str]| View::Chip {
            tone,
            head: head.into(),
            detail: detail.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(view(&MigrationState::Idle, true), View::Hidden);
        assert_eq!(view(&MigrationState::ConsentRequested, false), View::Hidden);
        assert_eq!(
            view(
                &MigrationState::Armed {
                    unprotected: vec![]
                },
                false
            ),
            chip(Tone::Good, "Protected", &[])
        );
        // The host is told who is left out; a client never is.
        let left_out = MigrationState::Armed {
            unprotected: vec!["Mo: late".into()],
        };
        assert_eq!(
            view(&left_out, true),
            chip(Tone::Good, "Protected", &["Mo: late"])
        );
        assert_eq!(view(&left_out, false), chip(Tone::Good, "Protected", &[]));
        let reason = "Arming is LAN-only: this room was opened with an online code.";
        for s in [
            MigrationState::Unprotected {
                reason: reason.into(),
            },
            MigrationState::Refused {
                reason: reason.into(),
            },
        ] {
            for host in [true, false] {
                assert!(
                    matches!(view(&s, host), View::Chip { tone: Tone::Warn, detail, .. } if detail == [plain_reason(reason)])
                );
            }
        }
        assert_eq!(
            view(
                &MigrationState::Paused {
                    reason: "Backup lost — the room is paused".into()
                },
                true
            ),
            View::Card {
                title: "Room paused".into(),
                body: "Backup lost — the room is paused".into()
            }
        );
        assert_eq!(
            view(&MigrationState::Recovering, false),
            View::Card {
                title: "Host lost — reconnecting…".into(),
                body: String::new()
            }
        );
        assert_eq!(
            view(&MigrationState::Resumed, false),
            chip(Tone::Good, TAKEOVER, &[])
        );
        assert_eq!(
            view(&MigrationState::UnprotectedAfterTakeover, false),
            chip(Tone::Warn, NO_BACKUP, &[])
        );
        assert_eq!(
            view(&MigrationState::Arming, true),
            chip(Tone::Info, "Setting up the backup…", &[])
        );
        assert_eq!(
            view(&MigrationState::Asking, true),
            chip(Tone::Info, "Waiting for the backup to answer…", &[])
        );
        assert_eq!(
            view(&MigrationState::Arming, false),
            chip(Tone::Info, "Setting up protection…", &[])
        );
    }

    /// After a takeover: who took over and "race on" for about 5 s, then a
    /// small "No backup now".
    #[test]
    fn a_takeover_reads_as_who_took_over_then_no_backup() {
        let head = |s: &MigrationState, host| match view(s, host) {
            View::Chip { tone, head, detail } if detail.is_empty() => (tone, head),
            v => panic!("{v:?}"),
        };
        assert_eq!(
            head(&MigrationState::Resumed, false),
            (Tone::Good, "The host's phone dropped out — the backup phone took over. Race on!".into())
        );
        assert_eq!(
            head(&MigrationState::Resumed, true),
            (Tone::Good, "The host's phone dropped out — your phone took over. Race on!".into())
        );
        for host in [true, false] {
            assert_eq!(head(&MigrationState::UnprotectedAfterTakeover, host), (Tone::Warn, "No backup now".into()));
        }
        assert_eq!(crate::online::RESUMED_NOTICE_MS, 5000, "the notice stays about 5 s");
    }

    /// What the person reads: no "arming", "LAN", "standby", "promote" or
    /// "ACK", on the host or on a joiner. The reasons stay as the session
    /// wrote them (and as the logs say them); only the screen words change.
    #[test]
    fn reasons_read_in_plain_words() {
        let shown = |reason: &str| {
            let mut out = Vec::new();
            for host in [true, false] {
                for state in [
                    MigrationState::Refused { reason: reason.into() },
                    MigrationState::Unprotected { reason: reason.into() },
                ] {
                    match view(&state, host) {
                        View::Chip { detail, .. } => out.push(detail.join(" | ")),
                        v => panic!("{v:?}"),
                    }
                }
            }
            assert!(out.iter().all(|t| *t == out[0]), "{out:?}");
            out.remove(0)
        };
        for (raw, plain) in [
            ("Arming is LAN-only: this room was opened with an online code.", "This room has an online code. Only a room on one Wi-Fi can have a backup."),
            ("Arming is refused while Bluetooth players are in the room (Kim).", "Kim joined by Bluetooth. A backup needs everyone on the same Wi-Fi."),
            ("Arming is refused while Bluetooth players are in the room (a joiner).", "Someone is joining by Bluetooth. A backup needs everyone on the same Wi-Fi."),
            ("Arming is refused over a Bluetooth link.", "This phone joined by Bluetooth. A backup needs everyone on the same Wi-Fi."),
            ("Arming is LAN-only: Kim did not join over the local network.", "Kim isn't on the same Wi-Fi."),
            ("Arming is LAN-only: this device did not join over the local network.", "This phone isn't on the host's Wi-Fi."),
            ("Arming needs a room opened on this phone's network.", "Only a room this phone hosts on its Wi-Fi can be protected."),
            ("Arming is only possible in the lobby.", "A room can only be protected in the lobby."),
            ("Arming is already in progress.", "The backup phone is already being set up."),
            ("This room is already armed.", "This room is already protected."),
            ("The room is not open.", "The room isn't open yet."),
            ("The chosen backup is not a player in this room.", "That player isn't in the room any more."),
            ("Kim did not arm within 5 s.", "Kim's phone didn't answer in time."),
            ("Kim did not consent within 5 s.", "Kim's phone didn't answer in time."),
            ("No capability ACK from Kim, Mo within 5 s.", "Kim, Mo's phone didn't answer in time."),
            ("Kim did not switch to the armed room within 5 s.", "Kim's phone didn't reconnect in time."),
            ("Kim left the lobby link but did not reconnect within 5 s.", "Kim's phone didn't reconnect in time."),
            ("Kim could not join the armed room: it closed", "Kim's phone couldn't reconnect."),
            ("Kim left during arming.", "Kim left while the backup phone was being set up."),
            ("The host cancelled arming.", "The host cancelled."),
            ("A player joined while arming.", "A player joined while the backup phone was being set up."),
            ("The lobby changed while it was held for arming.", "The lobby was busy. Try again."),
            ("Too much traffic while the lobby was held for arming.", "The lobby was busy. Try again."),
            ("The armed bundle does not fit (Size).", "The backup phone couldn't be set up."),
            ("The standby's armed certificate did not arrive within 5 s.", "The backup phone couldn't be set up."),
            ("Kim's consent did not match the offer (Bad).", "The backup phone couldn't be set up."),
            ("This phone could not rejoin the armed room. It stays the room's backup.", "This phone couldn't rejoin the room. It stays the backup."),
            ("Kim did not switch to the armed room within 5 s. Rejoining the lobby unarmed.", "Kim's phone didn't reconnect in time. Back in the lobby."),
            // Plain already: kept as they are.
            ("You declined to be the backup.", "You declined to be the backup."),
            ("Kim left the room.", "Kim left the room."),
        ] {
            let got = shown(raw);
            assert_eq!(got, plain, "{raw}");
            for word in ["arm", "LAN", "standby", "promot", "ACK", "local network"] {
                assert!(!got.contains(word), "{raw} reads {got}");
            }
        }
        // Who a protected room left out, on the host.
        let left_out = MigrationState::Armed {
            unprotected: vec!["Kim: Kim did not switch to the armed room within 5 s.".into()],
        };
        let View::Chip { detail, .. } = view(&left_out, true) else { panic!() };
        let (_, lines) = chip_lines(Tone::Good, "Protected", &detail, true);
        assert_eq!(lines, vec!["Not protected:".to_string(), "Kim: Kim's phone didn't reconnect in time.".to_string()]);
        // The paused card: plain reasons kept, a raw error said simply.
        assert_eq!(
            view(&MigrationState::Paused { reason: "The room is paused (Invalid(\"x\"))".into() }, true),
            View::Card { title: "Room paused".into(), body: "The room is paused".into() }
        );
        assert_eq!(
            view(&MigrationState::Paused { reason: "Backup lost — the room is paused".into() }, true),
            View::Card { title: "Room paused".into(), body: "Backup lost — the room is paused".into() }
        );
    }

    /// The host's short waits for the backup show nothing; one that lasts
    /// [`BEHIND_CARD_DELAY_MS`] shows the card, and every other pause at once.
    #[test]
    fn a_short_wait_for_the_backup_shows_no_card() {
        use crate::online::{BACKUP_LOST, BEHIND};
        let paused = |reason: &str| MigrationState::Paused { reason: reason.into() };
        let armed = MigrationState::Armed { unprotected: vec![] };
        let shown = |d: &mut BehindDelay, s: &MigrationState, now| view(&d.state(s, now), true);
        let protected = view(&armed, true);

        // 400 ms of waiting, then it catches up: nothing was drawn.
        let mut d = BehindDelay::default();
        assert_eq!(shown(&mut d, &armed, 0), protected);
        for now in [10_000, 10_100, 10_399] {
            assert_eq!(shown(&mut d, &paused(BEHIND), now), protected, "{now}");
        }
        assert_eq!(shown(&mut d, &armed, 10_400), protected);

        // A wait that reaches 1 s shows the card, as it reads today.
        let card = view(&paused(BEHIND), true);
        assert!(matches!(card, View::Card { .. }));
        assert_eq!(shown(&mut d, &paused(BEHIND), 20_000), protected);
        assert_eq!(shown(&mut d, &paused(BEHIND), 20_999), protected);
        assert_eq!(shown(&mut d, &paused(BEHIND), 20_000 + BEHIND_CARD_DELAY_MS), card);
        assert_eq!(shown(&mut d, &paused(BEHIND), 21_200), card);
        assert_eq!(shown(&mut d, &armed, 21_300), protected, "caught up: the card goes");

        // A new wait restarts the timer: 900 ms + 900 ms with a catch-up between is no card.
        assert_eq!(shown(&mut d, &paused(BEHIND), 30_000), protected);
        assert_eq!(shown(&mut d, &paused(BEHIND), 30_900), protected);
        assert_eq!(shown(&mut d, &armed, 30_950), protected);
        assert_eq!(shown(&mut d, &paused(BEHIND), 31_000), protected);
        assert_eq!(shown(&mut d, &paused(BEHIND), 31_900), protected);
        assert_eq!(shown(&mut d, &paused(BEHIND), 32_000), card);

        // Every other pause, and every other state, shows at once and unchanged.
        let lost = paused(BACKUP_LOST);
        let mut d = BehindDelay::default();
        assert_eq!(shown(&mut d, &lost, 50_000), view(&lost, true));
        assert_eq!(shown(&mut d, &paused(BEHIND), 50_100), protected);
        assert_eq!(shown(&mut d, &lost, 50_200), view(&lost, true), "a lost backup in the middle of a wait");
        assert_eq!(shown(&mut d, &paused(BEHIND), 50_300), protected, "and the wait after it starts over");
        for s in states() {
            assert_eq!(shown(&mut d, &s, 60_000), view(&s, true), "{s:?}");
        }
        // The wait hides as the protected state it came from: the same line, the same chip.
        let left_out = MigrationState::Armed { unprotected: vec!["Kim: late".into()] };
        let mut d = BehindDelay::default();
        d.state(&left_out, 70_000);
        assert_eq!(shown(&mut d, &paused(BEHIND), 70_100), view(&left_out, true));
    }

    /// The protected state a wait hides as belongs to the arming going on:
    /// it lasts through a lost backup, and goes once the room leaves the
    /// armed states, so a room armed again never shows the earlier
    /// arming's left-out players.
    #[test]
    fn a_short_wait_never_shows_an_earlier_armings_left_out_players() {
        use crate::online::{BACKUP_LOST, BEHIND};
        let paused = |reason: &str| MigrationState::Paused { reason: reason.into() };
        let left_out = MigrationState::Armed { unprotected: vec!["Kim: late".into()] };
        let nobody = MigrationState::Armed { unprotected: vec![] };

        let mut d = BehindDelay::default();
        d.state(&left_out, 0);
        assert_eq!(d.state(&paused(BACKUP_LOST), 100), paused(BACKUP_LOST));
        assert_eq!(d.state(&paused(BEHIND), 200), left_out, "the same arming, after a lost backup");

        let left = [
            MigrationState::Idle,
            MigrationState::Asking,
            MigrationState::Arming,
            MigrationState::Unprotected { reason: "x".into() },
            MigrationState::Refused { reason: "x".into() },
            paused("The backup took over the room"),
            MigrationState::Recovering,
            MigrationState::UnprotectedAfterTakeover,
        ];
        for s in left {
            let mut d = BehindDelay::default();
            d.state(&left_out, 0);
            d.state(&s, 100);
            assert_eq!(d.state(&paused(BEHIND), 200), nobody, "after {s:?}");
        }
    }

    /// The person asked is told it is the host's phone their phone would
    /// take over, not their own.
    #[test]
    fn the_backup_is_told_it_takes_over_the_hosts_phone() {
        let s = screen();
        let ctx = context();
        // A new area shows from its second frame.
        let mut out = None;
        for i in 0..2 {
            out = Some(ctx.run_ui(raw(s, vec![], 1.0 + i as f64), |ui| {
                consent_prompt(ui.ctx(), s, "Mo");
            }));
        }
        let all = all_text(&out.unwrap());
        assert!(all.contains("if the host's phone drops out"), "{all}");
        assert!(!all.contains("your phone drops out"), "{all}");
    }

    /// The safe rects the race and results tests use: both phones, with and
    /// without a left cutout.
    fn safes() -> Vec<Rect> {
        let mut out = Vec::new();
        for (w, h, ppp) in PHONES.into_iter().chain([(1280.0, 720.0, 720.0 / 412.0)]) {
            for left in [0.0, 34.0] {
                out.push(Rect::from_min_max(pos2(left, 0.0), pos2(w / ppp, h / ppp)));
            }
        }
        out
    }

    /// In the race the chip sits between the place and the Pause / Camera /
    /// Look-back buttons, above the lap and time column: never on them. It
    /// goes after about 5 s and comes back only when it changes.
    #[test]
    fn the_race_chip_keeps_off_the_hud_and_goes() {
        use crate::game::controls::{Btn, Layout};
        use crate::platform::data_dir::SteerMode;
        for safe in safes() {
            let area = race_note_area(safe);
            let ctx = context();
            let mut drawn = Vec::new();
            let _ = ctx.run_ui(raw(Rect::from_min_size(egui::Pos2::ZERO, safe.max.to_vec2()), vec![], 1.0), |ui| {
                let p = ui.painter().clone();
                for s in [MigrationState::Resumed, MigrationState::UnprotectedAfterTakeover, MigrationState::Refused { reason: "Arming is LAN-only: this room was opened with an online code.".into() }, MigrationState::Armed { unprotected: unprotected(3) }] {
                    for host in [true, false] {
                        drawn.push(chip(&p, area, &race_view(&view(&s, host)), false, 2));
                    }
                }
            });
            let (l, t, r) = (safe.left(), safe.top(), safe.right());
            // The minimap and the place (hud.rs), the lap / time column.
            let mut keep_off = vec![
                Rect::from_min_size(pos2(l + 8.0, t + 8.0), vec2(108.0, 86.0)),
                Rect::from_min_max(pos2(l + 116.0, t + 4.0), pos2(l + 240.0, t + 80.0)),
                Rect::from_min_max(pos2(r - 100.0, t + 52.0), pos2(r, t + 140.0)),
            ];
            for left_handed in [false, true] {
                let layout = Layout::new(safe, left_handed, SteerMode::Pad, false);
                for b in [Btn::Pause, Btn::Camera, Btn::LookBack] {
                    let c = layout.circle(b).unwrap();
                    keep_off.push(Rect::from_center_size(c.c, vec2(2.0 * c.r, 2.0 * c.r)));
                }
            }
            for d in drawn.into_iter().flatten() {
                assert!(area.expand(0.5).contains_rect(d), "{safe:?}: the chip {d:?} leaves its area {area:?}");
                for k in &keep_off {
                    assert!(!d.intersects(*k), "{safe:?}: the chip {d:?} covers {k:?}");
                }
            }
        }
        let mut note = GameNote::default();
        let takeover = view(&MigrationState::Resumed, false);
        let mut shown = 0;
        for _ in 0..(60 * 8) {
            shown += note.step(&takeover, 1.0 / 60.0) as usize;
        }
        assert!((290..=301).contains(&shown), "about 5 s: {shown} frames");
        assert!(note.step(&view(&MigrationState::UnprotectedAfterTakeover, false), 1.0 / 60.0), "a new state shows again");
        assert!(!note.step(&View::Hidden, 1.0 / 60.0));
    }

    /// The results chip is one row in the header, right of the title: it
    /// never reaches the table or the side panel below.
    #[test]
    fn the_results_chip_stays_in_the_header_row() {
        for safe in safes() {
            let body_top = safe.top() + S2 + 2.0 + TAP_MIN + S3;
            let title_right = safe.left() + S4 + TAP_MIN + S3 + 140.0;
            let ctx = context();
            let mut drawn = Vec::new();
            let _ = ctx.run_ui(raw(Rect::from_min_size(egui::Pos2::ZERO, safe.max.to_vec2()), vec![], 1.0), |ui| {
                let p = ui.painter().clone();
                for s in states() {
                    for host in [true, false] {
                        drawn.push(results_status(ui.ctx(), &p, safe, safe, &s, host));
                    }
                }
            });
            for d in drawn.into_iter().flatten() {
                assert!(d.bottom() <= body_top, "{safe:?}: the chip {d:?} reaches the body at {body_top}");
                assert!(d.left() >= title_right && d.right() <= safe.right(), "{safe:?}: the chip {d:?} is on the title");
            }
        }
    }

    #[test]
    fn the_protect_sheet_answers_protect_and_not_now() {
        let s = screen();
        let l = sheet_layout(s);
        let lines = [NEARBY_ONLY.to_string(), REJOIN.to_string()];
        assert_eq!(click_at(s, l.yes.center(), |ui| nearby_sheet(ui.ctx(), s, &lines)), Some(true));
        assert_eq!(click_at(s, l.no.center(), |ui| nearby_sheet(ui.ctx(), s, &lines)), Some(false));
        assert_eq!(click_at(s, l.card.center() - vec2(0.0, 40.0), |ui| nearby_sheet(ui.ctx(), s, &lines)), None);
        assert_eq!(NEARBY_ONLY, "Only players on this Wi-Fi can join a protected room. The online code will stop working.");
    }

    fn texts_of(shape: &Shape, out: &mut Vec<String>) {
        match shape {
            Shape::Vec(v) => v.iter().for_each(|s| texts_of(s, out)),
            Shape::Text(t) => out.push(t.galley.text().to_string()),
            _ => {}
        }
    }

    /// Every text drawn, joined.
    fn all_text(out: &egui::FullOutput) -> String {
        let mut v = Vec::new();
        for cs in &out.shapes {
            texts_of(&cs.shape, &mut v);
        }
        v.join(" ")
    }

    #[test]
    fn the_unprotected_list_names_players_and_shortens_when_long() {
        let (h, l) = chip_lines(Tone::Good, "Protected", &unprotected(2), true);
        assert_eq!((h.as_str(), l.len()), ("Protected", 3));
        assert!(l[1].starts_with("Player0:") && l[2].starts_with("Player1:"));
        let (_, l) = chip_lines(Tone::Good, "Protected", &unprotected(4), true);
        assert_eq!((l.len(), l[2].as_str()), (3, "and 3 more"));
        let (h, l) = chip_lines(Tone::Good, "Protected", &unprotected(2), false);
        assert_eq!((h.as_str(), l.len()), ("Protected · 2 not covered", 0));
    }

    #[test]
    fn the_disclosure_and_pick_rules_are_the_decided_ones() {
        assert_eq!(DISCLOSURE, "The backup phone receives the full game state, including hidden information, so it can take over if your phone drops out.");
        assert_eq!(DISCLOSURE_ASKED, "The backup phone receives the full game state, including hidden information, so it can take over if the host's phone drops out.");
        let seat = |id, bot, host| race_session::SeatView {
            id,
            name: format!("p{id}"),
            character: 0,
            ready: false,
            bot,
            host,
        };
        let seats = [
            seat(0, false, true),
            seat(1, false, false),
            seat(100, true, false),
            seat(2, false, false),
        ];
        let ids: Vec<u8> = candidates(&seats).iter().map(|c| c.seat).collect();
        assert_eq!(ids, [1, 2], "seated humans other than the host; no bots");
    }

    // ── drawing and taps, headless ──

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        ctx
    }

    fn raw(screen: Rect, events: Vec<Event>, time: f64) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(screen),
            events,
            time: Some(time),
            ..Default::default()
        }
    }

    /// Draw `f` over a click (move, press, release, settle) at `pos`: what it returned on any frame.
    fn click_at<R: Clone>(
        screen: Rect,
        pos: egui::Pos2,
        f: impl Fn(&mut egui::Ui) -> Option<R>,
    ) -> Option<R> {
        let ctx = context();
        let button = |pressed| Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut out = None;
        for (i, events) in [
            vec![],
            vec![Event::PointerMoved(pos)],
            vec![button(true)],
            vec![button(false)],
            vec![],
        ]
        .into_iter()
        .enumerate()
        {
            let mut got = None;
            let _ = ctx.run_ui(raw(screen, events, i as f64 * 0.05), |ui| got = f(ui));
            out = out.or(got);
        }
        out
    }

    fn screen() -> Rect {
        Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 360.0))
    }

    #[test]
    fn the_consent_buttons_answer_yes_and_no() {
        let s = screen();
        let l = prompt_layout(s);
        assert_eq!(
            click_at(s, l.yes.center(), |ui| consent_prompt(ui.ctx(), s, "Mo")),
            Some(true)
        );
        assert_eq!(
            click_at(s, l.no.center(), |ui| consent_prompt(ui.ctx(), s, "Mo")),
            Some(false)
        );
        // A tap elsewhere on the card answers nothing.
        assert_eq!(
            click_at(s, l.card.center() - vec2(0.0, 40.0), |ui| consent_prompt(
                ui.ctx(),
                s,
                "Mo"
            )),
            None
        );
    }

    #[test]
    fn the_header_cluster_protects_or_cancels() {
        let s = screen();
        let band = band(s, 300.0);
        let run = |state: MigrationState, host: bool, can: bool, at: egui::Pos2| {
            click_at(s, at, move |ui| cluster(ui, band, &state, host, can))
        };
        let at = protect_button(s).center();
        assert_eq!(
            run(MigrationState::Idle, true, true, at),
            Some(Cluster::Protect)
        );
        let refused = MigrationState::Refused {
            reason: "Arming is LAN-only: this room was opened with an online code.".into(),
        };
        assert_eq!(
            run(refused, true, true, at),
            Some(Cluster::Protect),
            "a refused host may try again"
        );
        assert_eq!(
            run(MigrationState::Idle, false, false, at),
            None,
            "a guest has no such control"
        );
        assert_eq!(
            run(
                MigrationState::Armed {
                    unprotected: vec![]
                },
                true,
                false,
                at
            ),
            None
        );
        let cancel = action_button(band, CANCEL_W).center();
        assert_eq!(
            run(MigrationState::Asking, true, false, cancel),
            Some(Cluster::Cancel)
        );
        assert_eq!(
            run(MigrationState::Arming, true, false, cancel),
            Some(Cluster::Cancel)
        );
    }

    #[test]
    fn the_picker_lists_people_and_chooses_one() {
        let s = screen();
        let who = vec![
            Candidate {
                seat: 3,
                name: "Mo".into(),
                character: 1,
            },
            Candidate {
                seat: 5,
                name: "Jo".into(),
                character: 2,
            },
        ];
        let run = |who: Vec<Candidate>, at: egui::Pos2| {
            click_at(s, at, move |ui| {
                let mut scroll = 0.0;
                backup_picker(1.0, s, s, ui.ctx(), &who, &mut scroll)
            })
        };
        assert_eq!(
            run(who.clone(), pick_button(s, 0).center()),
            Some(Pick::Seat(3))
        );
        assert_eq!(
            run(who.clone(), pick_button(s, 1).center()),
            Some(Pick::Seat(5))
        );
        assert_eq!(
            run(vec![], pick_button(s, 0).center()),
            None,
            "nobody to choose"
        );
    }

    fn text_rects(shape: &Shape, out: &mut Vec<Rect>) {
        match shape {
            Shape::Vec(v) => v.iter().for_each(|s| text_rects(s, out)),
            Shape::Text(t) => {
                if t.galley.rows.iter().any(|r| !r.row.glyphs.is_empty()) && t.opacity_factor > 0.05
                {
                    let r = t.galley.rect;
                    out.push(Rect::from_min_size(t.pos + r.min.to_vec2(), r.size()));
                }
            }
            _ => {}
        }
    }

    /// Every state, drawn every way, keeps its text inside the safe area at both phone sizes.
    #[test]
    fn every_state_keeps_its_text_inside_the_safe_area() {
        let who = vec![
            Candidate {
                seat: 3,
                name: "A very long player name indeed".into(),
                character: 1
            };
            7
        ];
        let mut texts = 0;
        for (w, h, ppp) in PHONES {
            for insets in [
                Insets::ZERO,
                Insets {
                    left: 34.0,
                    top: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                },
            ] {
                let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(w / ppp, h / ppp));
                let safe = Rect::from_min_max(
                    screen.min + vec2(insets.left, insets.top),
                    screen.max - vec2(insets.right, insets.bottom),
                );
                for state in states() {
                    for host in [true, false] {
                        for can in [true, false] {
                            let ctx = context();
                            let out = ctx.run_ui(raw(screen, vec![], 1.0), |ui| {
                                cluster(ui, band(safe, safe.left() + 260.0), &state, host, can);
                                let c = ui.ctx().clone();
                                let p = c.layer_painter(egui::LayerId::new(
                                    egui::Order::Middle,
                                    egui::Id::new("t"),
                                ));
                                let _ = results_status(&c, &p, screen, safe, &state, host);
                                race_status(&c, &p, screen, safe, &state, host, &mut GameNote::default(), 0.0);
                            });
                            texts += check(
                                &out,
                                safe,
                                &format!("{state:?} host={host} can={can} {w}x{h}@{ppp}"),
                            );
                        }
                    }
                }
                let ctx = context();
                let out = ctx.run_ui(raw(screen, vec![], 1.0), |ui| {
                    consent_prompt(ui.ctx(), safe, "A host with a rather long name");
                });
                texts += check(&out, safe, "consent prompt");
                let ctx = context();
                let out = ctx.run_ui(raw(screen, vec![], 1.0), |ui| {
                    backup_picker(1.0, screen, safe, ui.ctx(), &who, &mut 0.0);
                });
                texts += check(&out, safe, "picker");
                let ctx = context();
                let out = ctx.run_ui(raw(screen, vec![], 1.0), |ui| {
                    backup_picker(1.0, screen, safe, ui.ctx(), &[], &mut 0.0);
                });
                assert!(all_text(&out).contains(PICKER_EMPTY), "the empty picker says how to join");
                texts += check(&out, safe, "empty picker");
                let ctx = context();
                let out = ctx.run_ui(raw(screen, vec![], 1.0), |ui| {
                    nearby_sheet(ui.ctx(), safe, &[NEARBY_ONLY.to_string(), REJOIN.to_string()]);
                });
                texts += check(&out, safe, "protect sheet");
            }
        }
        assert!(
            texts > 1000,
            "the test saw {texts} texts: it is not drawing"
        );
    }

    /// Fails on text outside `safe`; how many texts it saw.
    fn check(out: &egui::FullOutput, safe: Rect, what: &str) -> usize {
        let mut bad = Vec::new();
        let mut seen = 0;
        for cs in &out.shapes {
            let mut rects = Vec::new();
            text_rects(&cs.shape, &mut rects);
            seen += rects.len();
            for r in rects {
                let r = r.intersect(cs.clip_rect);
                if r.is_negative() || r.width() <= 0.0 || r.height() <= 0.0 {
                    continue;
                }
                if !safe.expand(6.0).contains_rect(r) {
                    bad.push(r);
                }
            }
        }
        assert!(bad.is_empty(), "{what}: text outside {safe:?}: {bad:?}");
        seen
    }
}
