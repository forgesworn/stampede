//! The Settings cards below the two fixed panels: RELAYS (the ordered nostr relay
//! list with health dots), UPDATES (in-app updates) and MORE GAMES (the entry to
//! the catalog). Ported from Twang Thang's Settings (spec 20.1, 20.2), redrawn in
//! Stampede's style. Each card is drawn into a rectangle the screen lays out, and
//! reports what was tapped as a [`CardAction`]; the screen acts on it.
//!
//! The screen scrolls, so a row that does not lie wholly inside the visible
//! `view` is neither drawn nor tappable (no half rows under the header).

use crate::ui::theme;
use crate::ui::widgets::{self, Btn};
use crate::update::{UpdateTone, UpdateView};
use egui::{pos2, vec2, Align2, Color32, Rect, Sense, Shape, Stroke, Ui};

/// Height of one relay row.
pub const RELAY_ROW_H: f32 = 40.0;
/// Height of a button row in a card.
const BTN_H: f32 = 42.0;
/// Card padding.
const PAD: f32 = 14.0;
/// The label line ("RELAYS") at the top of a card.
const LABEL_H: f32 = 22.0;
/// The extra line while the defaults stand in for the player's list.
const NOTICE_H: f32 = 24.0;

/// What a card asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum CardAction {
    /// Move relay `i` up (`true`) or down.
    MoveRelay(usize, bool),
    RemoveRelay(usize),
    /// Tap on relay `i`'s health dot.
    ShowRelayHealth(usize),
    /// Open the keypad for the "Add a relay" field.
    TypeRelay,
    PasteRelay,
    AddRelay,
    RestoreRelays,
    CheckUpdates,
    InstallUpdate,
    CancelUpdate,
    MoreGames,
}

/// A relay's dot colour: green working, amber slow or flaky, red failing, grey untried.
pub fn relay_dot_color(dot: decented_relays::Dot) -> Color32 {
    match dot {
        decented_relays::Dot::Green => Color32::from_rgb(0x34, 0xD3, 0x99),
        decented_relays::Dot::Amber => theme::GOLD,
        decented_relays::Dot::Red => theme::LASER,
        decented_relays::Dot::Unknown => theme::TEXT_DIM.gamma_multiply(0.6),
    }
}

/// What tapping a relay's dot says: its last error, or how it is doing.
pub fn relay_health_line(url: &str, v: &decented_relays::RelayView) -> String {
    use decented_relays::Dot;
    let host = url.trim_start_matches("wss://").trim_start_matches("ws://");
    let err: String = v.last_error.as_deref().unwrap_or("no reason given").chars().take(90).collect();
    match v.dot {
        Dot::Unknown => format!("{host}: not tried yet"),
        Dot::Green => match v.latency {
            Some(l) => format!("{host}: working ({} ms)", l.as_millis()),
            None => format!("{host}: working"),
        },
        Dot::Amber => match (&v.last_error, v.latency) {
            (Some(_), _) => format!("{host}: flaky. Last error: {err}"),
            (None, Some(l)) => format!("{host}: slow ({} ms)", l.as_millis()),
            (None, None) => format!("{host}: slow"),
        },
        Dot::Red => match v.retry_in {
            Some(t) => format!("{host}: {err}. Trying again in {} s", t.as_secs() + 1),
            None => format!("{host}: {err}"),
        },
    }
}

/// Where one relay row's parts sit: health dot, address, then up, down and
/// remove (each a 32 x 34 target) along the right edge.
#[derive(Clone, Copy, Debug)]
pub struct RelayRow {
    pub dot: Rect,
    pub text: Rect,
    pub up: Rect,
    pub down: Rect,
    pub remove: Rect,
}

impl RelayRow {
    pub fn at(card: Rect, cy: f32) -> RelayRow {
        let btn = |cx: f32| Rect::from_center_size(pos2(cx, cy), vec2(32.0, 34.0));
        let remove = btn(card.max.x - 20.0);
        let down = btn(card.max.x - 53.0);
        let up = btn(card.max.x - 86.0);
        let dot = Rect::from_center_size(pos2(card.min.x + 22.0, cy), vec2(34.0, 34.0));
        let x0 = dot.max.x + 4.0;
        let text = Rect::from_min_max(pos2(x0, cy - 17.0), pos2(up.min.x - 4.0, cy + 17.0));
        RelayRow { dot, text, up, down, remove }
    }
}

/// The RELAYS card's height for `n` relays.
pub fn relays_height(n: usize, notice: bool) -> f32 {
    PAD + LABEL_H + if notice { NOTICE_H } else { 0.0 } + n as f32 * RELAY_ROW_H + 8.0 + BTN_H + 8.0 + BTN_H + PAD
}

/// Is `r` wholly inside the visible area?
fn shown(view: Rect, r: Rect) -> bool {
    view.expand(0.5).contains_rect(r)
}

fn card_frame(ui: &Ui, card: Rect, label: &str, view: Rect, hot: bool) {
    let p = ui.painter();
    widgets::panel(p, card, 20.0);
    if hot {
        p.rect_stroke(card, egui::CornerRadius::same(20), Stroke::new(2.0_f32, theme::ACCENT), egui::StrokeKind::Inside);
    }
    let lr = Rect::from_min_size(card.min + vec2(PAD + 2.0, PAD - 2.0), vec2(card.width() - 2.0 * PAD, LABEL_H - 4.0));
    if shown(view, lr) {
        p.text(pos2(lr.left(), lr.center().y), Align2::LEFT_CENTER, label, theme::ui_bold(11.5), theme::SAND);
    }
}

/// The text of `s` fitted to `width`, cut from the LEFT with an ellipsis (the end of an address is what tells relays apart).
fn tail_fit(p: &egui::Painter, s: &str, font: &egui::FontId, width: f32) -> String {
    let w = |t: &str| p.layout_no_wrap(t.to_string(), font.clone(), Color32::WHITE).size().x;
    if w(s) <= width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    for cut in 1..chars.len() {
        let t: String = std::iter::once('\u{2026}').chain(chars[cut..].iter().copied()).collect();
        if w(&t) <= width {
            return t;
        }
    }
    "\u{2026}".to_string()
}

/// The RELAYS card. `input` is what is typed in the "Add a relay" field so far.
#[allow(clippy::too_many_arguments)]
pub fn draw_relays(ui: &mut Ui, view: Rect, card: Rect, relays: &[String], health: &[decented_relays::RelayView], notice: Option<&str>, input: &str, defaults: &[String], act: &mut Option<CardAction>) {
    card_frame(ui, card, "RELAYS", view, false);
    let p = ui.painter().clone();
    let hdr = if notice.is_some() { NOTICE_H } else { 0.0 };
    let top = card.min.y + PAD + LABEL_H;
    if let Some(n) = notice {
        let r = Rect::from_min_size(pos2(card.min.x + PAD + 2.0, top), vec2(card.width() - 2.0 * PAD, NOTICE_H));
        if shown(view, r) {
            let shown_text = tail_fit(&p, n, &theme::ui_medium(12.0), r.width());
            p.text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, shown_text, theme::ui_medium(12.0), theme::GOLD);
        }
    }
    for (i, url) in relays.iter().enumerate() {
        let cy = top + hdr + i as f32 * RELAY_ROW_H + RELAY_ROW_H / 2.0;
        let row = RelayRow::at(card, cy);
        let whole = Rect::from_min_max(pos2(card.min.x + 6.0, cy - RELAY_ROW_H / 2.0), pos2(card.max.x - 6.0, cy + RELAY_ROW_H / 2.0));
        if !shown(view, whole) {
            continue;
        }
        let view_i = health.get(i);
        // The health dot (tap: say what went wrong).
        let dot_resp = ui.interact(row.dot, egui::Id::new(("stampede-relay-dot", i)), Sense::click());
        let col = relay_dot_color(view_i.map_or(decented_relays::Dot::Unknown, |v| v.dot));
        p.circle_filled(row.dot.center(), 5.5, col);
        if dot_resp.clicked() {
            *act = Some(CardAction::ShowRelayHealth(i));
        }
        let txt = url.trim_start_matches("wss://");
        let shown_text = tail_fit(&p, txt, &theme::ui_medium(14.0), row.text.width());
        p.text(pos2(row.text.min.x, cy), Align2::LEFT_CENTER, shown_text, theme::ui_medium(14.0), theme::TEXT);
        // Up / down: the list is the priority order, the top is tried first.
        for (rect, up) in [(row.up, true), (row.down, false)] {
            let can = if up { i > 0 } else { i + 1 < relays.len() };
            let resp = ui.interact(rect, egui::Id::new(("stampede-relay-move", i, up)), if can { Sense::click() } else { Sense::hover() });
            let c = if !can { theme::TEXT_DIM.gamma_multiply(0.3) } else if resp.hovered() { theme::ACCENT } else { theme::SAND };
            let (cx, k) = (rect.center(), 5.5);
            let tri = if up { vec![cx + vec2(-k, k * 0.6), cx + vec2(k, k * 0.6), cx + vec2(0.0, -k * 0.8)] } else { vec![cx + vec2(-k, -k * 0.6), cx + vec2(k, -k * 0.6), cx + vec2(0.0, k * 0.8)] };
            p.add(Shape::convex_polygon(tri, c, Stroke::NONE));
            if can && resp.clicked() {
                *act = Some(CardAction::MoveRelay(i, up));
            }
        }
        let resp = ui.interact(row.remove, egui::Id::new(("stampede-relay-remove", i)), Sense::click());
        let c = if resp.hovered() { theme::LASER } else { theme::TEXT_DIM };
        let (cx, k) = (row.remove.center(), 5.0);
        p.line_segment([cx + vec2(-k, -k), cx + vec2(k, k)], Stroke::new(2.0_f32, c));
        p.line_segment([cx + vec2(-k, k), cx + vec2(k, -k)], Stroke::new(2.0_f32, c));
        if resp.clicked() {
            *act = Some(CardAction::RemoveRelay(i));
        }
        if i + 1 < relays.len() {
            p.hline((card.min.x + PAD)..=(card.max.x - PAD), cy + RELAY_ROW_H / 2.0, Stroke::new(1.0_f32, Color32::from_white_alpha(14)));
        }
    }
    // Add a relay: tap the field to type it on the keypad, or paste it; Add stores it.
    let ay = top + hdr + relays.len() as f32 * RELAY_ROW_H + 8.0;
    let (add_w, paste_w) = (58.0, 68.0);
    let field = Rect::from_min_size(pos2(card.min.x + PAD, ay), vec2(card.width() - 2.0 * PAD - add_w - paste_w - 12.0, BTN_H));
    let paste = Rect::from_min_size(pos2(field.max.x + 6.0, ay), vec2(paste_w, BTN_H));
    let add = Rect::from_min_size(pos2(paste.max.x + 6.0, ay), vec2(add_w, BTN_H));
    if shown(view, Rect::from_min_max(field.min, add.max)) {
        let resp = ui.interact(field, egui::Id::new("stampede-relay-field"), Sense::click());
        widgets::well(&p, field, 12.0);
        p.rect_stroke(field, egui::CornerRadius::same(12), Stroke::new(1.0_f32, if resp.hovered() { theme::ACCENT } else { Color32::from_white_alpha(30) }), egui::StrokeKind::Inside);
        if input.is_empty() {
            p.text(pos2(field.min.x + 10.0, field.center().y), Align2::LEFT_CENTER, "Add a relay: wss://\u{2026}", theme::ui_medium(13.0), theme::TEXT_DIM.gamma_multiply(0.75));
        } else {
            let t = tail_fit(&p, input, &theme::ui_medium(13.5), field.width() - 20.0);
            p.text(pos2(field.min.x + 10.0, field.center().y), Align2::LEFT_CENTER, t, theme::ui_medium(13.5), theme::TEXT);
        }
        if resp.clicked() {
            *act = Some(CardAction::TypeRelay);
        }
        if widgets::button(ui, paste, "relay-paste", &Btn::ghost("Paste").size(14.0)).clicked() {
            *act = Some(CardAction::PasteRelay);
        }
        if widgets::button(ui, add, "relay-add", &Btn::new("Add").size(15.0).enabled(!input.trim().is_empty())).clicked() {
            *act = Some(CardAction::AddRelay);
        }
    }
    let by = ay + BTN_H + 8.0;
    let reset = Rect::from_min_size(pos2(card.min.x + PAD, by), vec2((card.width() - 2.0 * PAD).min(220.0), BTN_H));
    if shown(view, reset) && widgets::button(ui, reset, "relays-reset", &Btn::ghost("Restore defaults").size(14.0).enabled(relays != defaults)).clicked() {
        *act = Some(CardAction::RestoreRelays);
    }
}

fn tone_color(t: UpdateTone) -> Color32 {
    match t {
        UpdateTone::Dim => theme::TEXT_DIM,
        UpdateTone::Accent => theme::ACCENT,
        UpdateTone::Gold => theme::GOLD,
        UpdateTone::Danger => theme::LASER,
    }
}

/// The UPDATES card's height (the status and notes wrap, so it depends on `v`).
pub fn updates_height(p: &egui::Painter, width: f32, v: &UpdateView) -> f32 {
    let inner = width - 2.0 * PAD - 4.0;
    let status = p.layout(v.status.clone(), theme::ui_medium(13.0), Color32::WHITE, inner).size().y;
    let notes = v.offer.as_ref().filter(|o| !o.notes.is_empty()).map_or(0.0, |o| p.layout(o.notes.clone(), theme::ui_medium(12.0), Color32::WHITE, inner).size().y + 6.0);
    PAD + LABEL_H + 22.0 + 4.0 + status + 8.0 + notes + if v.progress.is_some() { 14.0 } else { 0.0 } + if v.offer.is_some() { BTN_H + 8.0 } else { 0.0 } + BTN_H + PAD
}

/// The UPDATES card: what is installed, where the check stands, the release
/// notes when one is on offer, download progress, and the buttons.
pub fn draw_updates(ui: &mut Ui, view: Rect, card: Rect, v: &UpdateView, t: f32, act: &mut Option<CardAction>) {
    card_frame(ui, card, "UPDATES", view, v.offer.is_some());
    let p = ui.painter().clone();
    let tone = tone_color(v.tone);
    let x = card.min.x + PAD + 2.0;
    let inner = card.width() - 2.0 * PAD - 4.0;
    let mut y = card.min.y + PAD + LABEL_H;
    let title = Rect::from_min_size(pos2(x, y), vec2(inner, 22.0));
    if shown(view, title) {
        p.text(pos2(x, y + 11.0), Align2::LEFT_CENTER, format!("Installed {}", v.installed), theme::ui_bold(15.0), theme::TEXT);
        if v.checking {
            widgets::dots(&p, pos2(card.max.x - 36.0, y + 11.0), 3, t * 3.0, theme::ACCENT);
        }
    }
    y += 22.0 + 4.0;
    let sg = p.layout(v.status.clone(), theme::ui_medium(13.0), tone, inner);
    let sh = sg.size().y;
    if shown(view, Rect::from_min_size(pos2(x, y), vec2(inner, sh))) {
        p.galley(pos2(x, y), sg, tone);
    }
    y += sh + 8.0;
    if let Some(o) = v.offer.as_ref().filter(|o| !o.notes.is_empty()) {
        let ng = p.layout(o.notes.clone(), theme::ui_medium(12.0), theme::TEXT_DIM, inner);
        let nh = ng.size().y;
        if shown(view, Rect::from_min_size(pos2(x, y - 6.0), vec2(inner, nh))) {
            p.galley(pos2(x, y - 6.0), ng, theme::TEXT_DIM);
        }
        y += nh;
    }
    if let Some(pr) = v.progress {
        let track = Rect::from_min_size(pos2(x, y - 2.0), vec2(inner, 6.0));
        if shown(view, track) {
            p.rect_filled(track, 3.0, Color32::from_white_alpha(28));
            p.rect_filled(Rect::from_min_size(track.min, vec2(track.width() * pr, 6.0)), 3.0, theme::ACCENT);
        }
        y += 14.0;
    }
    if let Some(o) = &v.offer {
        let b = Rect::from_min_size(pos2(x, y), vec2(inner, BTN_H));
        if shown(view, b) {
            if v.downloading {
                if widgets::button(ui, b, "update-cancel", &Btn::ghost("Cancel download").size(16.0)).clicked() {
                    *act = Some(CardAction::CancelUpdate);
                }
            } else if widgets::button(ui, b, "update-install", &Btn::primary(&format!("Update to v{}", o.version)).size(17.0).enabled(v.can_install)).clicked() {
                *act = Some(CardAction::InstallUpdate);
            }
        }
        y += BTN_H + 8.0;
    }
    let b = Rect::from_min_size(pos2(x, y), vec2(inner, BTN_H));
    if shown(view, b) && widgets::button(ui, b, "update-check", &Btn::ghost("Check for updates").size(15.0).enabled(v.can_check)).clicked() {
        *act = Some(CardAction::CheckUpdates);
    }
}

/// The MORE GAMES card's height.
pub fn more_games_height() -> f32 {
    PAD + LABEL_H + BTN_H + PAD
}

/// The MORE GAMES card: one button to the catalog.
pub fn draw_more_games(ui: &mut Ui, view: Rect, card: Rect, act: &mut Option<CardAction>) {
    card_frame(ui, card, "MORE GAMES", view, false);
    let b = Rect::from_min_size(pos2(card.min.x + PAD + 2.0, card.min.y + PAD + LABEL_H), vec2(card.width() - 2.0 * PAD - 4.0, BTN_H));
    if shown(view, b) && widgets::button(ui, b, "more-games", &Btn::new("More games").sub("Other games by the same people").size(16.0)).clicked() {
        *act = Some(CardAction::MoreGames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decented_relays::{Dot, RelayView};
    use egui::Pos2;
    use std::time::Duration;

    fn view(dot: Dot, err: Option<&str>, lat: Option<u64>, retry: Option<u64>) -> RelayView {
        RelayView { dot, last_error: err.map(String::from), successes: 1, failures: 1, latency: lat.map(Duration::from_millis), retry_in: retry.map(Duration::from_secs) }
    }

    #[test]
    fn a_dot_tap_says_what_is_wrong() {
        let u = "wss://relay.example.com";
        assert_eq!(relay_health_line(u, &view(Dot::Unknown, None, None, None)), "relay.example.com: not tried yet");
        assert_eq!(relay_health_line(u, &view(Dot::Green, None, Some(120), None)), "relay.example.com: working (120 ms)");
        assert!(relay_health_line(u, &view(Dot::Amber, Some("timed out"), None, None)).contains("flaky. Last error: timed out"));
        assert!(relay_health_line(u, &view(Dot::Amber, None, Some(2000), None)).contains("slow (2000 ms)"));
        let red = relay_health_line(u, &view(Dot::Red, Some("connection refused"), None, Some(14)));
        assert!(red.contains("connection refused") && red.contains("Trying again in 15 s"), "{red}");
    }

    #[test]
    fn dots_are_green_amber_red_and_grey() {
        let cs = [Dot::Green, Dot::Amber, Dot::Red, Dot::Unknown].map(relay_dot_color);
        for i in 0..4 {
            for j in i + 1..4 {
                assert_ne!(cs[i], cs[j]);
            }
        }
    }

    #[test]
    fn a_row_has_room_for_its_parts_in_a_narrow_card() {
        let card = Rect::from_min_size(pos2(0.0, 0.0), vec2(330.0, 200.0));
        let r = RelayRow::at(card, 20.0);
        assert!(r.text.width() > 100.0, "{r:?}");
        assert!(r.dot.max.x <= r.text.min.x && r.text.max.x <= r.up.min.x && r.up.max.x <= r.down.min.x && r.down.max.x <= r.remove.min.x && r.remove.max.x <= card.max.x);
    }

    /// Draw `f` for two frames with a click (press, then release) at `pos`; what the second frame reported.
    fn click_at(pos: Pos2, f: impl Fn(&mut Ui, &mut Option<CardAction>)) -> Option<CardAction> {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let raw = |events: Vec<egui::Event>, time: f64| egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
            events,
            time: Some(time),
            ..Default::default()
        };
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let mut out = None;
        for (i, events) in [vec![egui::Event::PointerMoved(pos)], vec![button(true)], vec![button(false)], vec![]].into_iter().enumerate() {
            let mut act = None;
            #[allow(deprecated)]
            let _ = ctx.run(raw(events, i as f64 * 0.05), |c| {
                #[allow(deprecated)]
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(c, |ui| f(ui, &mut act));
            });
            if act.is_some() {
                out = act;
            }
        }
        out
    }

    fn relays3() -> Vec<String> {
        crate::platform::data_dir::default_relays()
    }

    const CARD_W: f32 = 400.0;

    fn card() -> Rect {
        Rect::from_min_size(pos2(20.0, 20.0), vec2(CARD_W, relays_height(3, false)))
    }

    fn relay_click(pos: Pos2, relays: &[String], input: &str) -> Option<CardAction> {
        let healths: Vec<RelayView> = relays.iter().map(|_| view(Dot::Green, None, Some(80), None)).collect();
        let defaults = relays3();
        let (c, rel, inp) = (card(), relays.to_vec(), input.to_string());
        click_at(pos, move |ui, act| draw_relays(ui, c.expand(20.0), c, &rel, &healths, None, &inp, &defaults, act))
    }

    fn row_y(i: usize) -> f32 {
        card().min.y + PAD + LABEL_H + i as f32 * RELAY_ROW_H + RELAY_ROW_H / 2.0
    }

    #[test]
    fn the_relay_card_reports_taps_on_move_remove_and_dot() {
        let rel = relays3();
        let r = |i: usize| RelayRow::at(card(), row_y(i));
        assert_eq!(relay_click(r(0).down.center(), &rel, ""), Some(CardAction::MoveRelay(0, false)));
        assert_eq!(relay_click(r(2).up.center(), &rel, ""), Some(CardAction::MoveRelay(2, true)));
        assert_eq!(relay_click(r(1).remove.center(), &rel, ""), Some(CardAction::RemoveRelay(1)));
        assert_eq!(relay_click(r(2).dot.center(), &rel, ""), Some(CardAction::ShowRelayHealth(2)));
        // The ends cannot move past themselves.
        assert_eq!(relay_click(r(0).up.center(), &rel, ""), None);
        assert_eq!(relay_click(r(2).down.center(), &rel, ""), None);
    }

    #[test]
    fn the_add_row_types_pastes_adds_and_restores() {
        let c = card();
        let ay = c.min.y + PAD + LABEL_H + 3.0 * RELAY_ROW_H + 8.0;
        let field = pos2(c.min.x + 60.0, ay + BTN_H / 2.0);
        let paste = pos2(c.max.x - PAD - 58.0 - 6.0 - 34.0, ay + BTN_H / 2.0);
        let add = pos2(c.max.x - PAD - 29.0, ay + BTN_H / 2.0);
        let rel = relays3();
        assert_eq!(relay_click(field, &rel, ""), Some(CardAction::TypeRelay));
        assert_eq!(relay_click(paste, &rel, ""), Some(CardAction::PasteRelay));
        assert_eq!(relay_click(add, &rel, ""), None, "nothing typed, nothing to add");
        assert_eq!(relay_click(add, &rel, "relay.example.com"), Some(CardAction::AddRelay));
        // Restore defaults is live only when the list differs from them.
        let reset = pos2(c.min.x + PAD + 40.0, ay + BTN_H + 8.0 + BTN_H / 2.0);
        assert_eq!(relay_click(reset, &rel, ""), None);
        let mut changed = rel.clone();
        changed.push("wss://relay.example.com".into());
        // (the card is one row taller now: the click lands a row lower)
        let reset2 = pos2(reset.x, reset.y + RELAY_ROW_H);
        let healths: Vec<RelayView> = changed.iter().map(|_| view(Dot::Green, None, None, None)).collect();
        let c2 = Rect::from_min_size(c.min, vec2(CARD_W, relays_height(4, false)));
        let defaults = relays3();
        let got = click_at(reset2, move |ui, act| draw_relays(ui, c2.expand(20.0), c2, &changed, &healths, None, "", &defaults, act));
        assert_eq!(got, Some(CardAction::RestoreRelays));
    }

    #[test]
    fn rows_outside_the_visible_area_are_neither_drawn_nor_tappable() {
        let rel = relays3();
        let healths: Vec<RelayView> = rel.iter().map(|_| view(Dot::Green, None, None, None)).collect();
        let defaults = relays3();
        let c = card();
        // Only the top row is in view.
        let view_rect = Rect::from_min_max(c.min, pos2(c.max.x, row_y(0) + RELAY_ROW_H / 2.0 + 1.0));
        let r1 = RelayRow::at(c, row_y(1));
        let got = click_at(r1.remove.center(), move |ui, act| draw_relays(ui, view_rect, c, &rel, &healths, None, "", &defaults, act));
        assert_eq!(got, None);
    }

    #[test]
    fn the_updates_card_offers_what_the_view_allows() {
        use crate::update::OfferView;
        let c = Rect::from_min_size(pos2(20.0, 20.0), vec2(CARD_W, 260.0));
        let mut v = UpdateView { installed: "v0.2.0".into(), status: "Version 0.3.0 is available (9.8 MB)".into(), tone: UpdateTone::Accent, offer: Some(OfferView { version: "0.3.0".into(), size: "9.8 MB".into(), notes: String::new() }), checking: false, progress: None, can_check: true, can_install: true, downloading: false };
        let (x, inner) = (c.min.x + PAD + 2.0, c.width() - 2.0 * PAD - 4.0);
        let _ = inner;
        // Layout: label, installed line, status (one line here), then the buttons.
        let probe = |v: &UpdateView, y: f32| {
            let v = v.clone();
            click_at(pos2(x + 60.0, y), move |ui, act| draw_updates(ui, c.expand(300.0), c, &v, 0.0, act))
        };
        let top = c.min.y + PAD + LABEL_H + 22.0 + 4.0;
        let status_h = 17.0; // one line of 13 pt text
        let by = top + status_h + 8.0;
        let install_y = by + BTN_H / 2.0;
        let got = probe(&v, install_y);
        assert!(matches!(got, Some(CardAction::InstallUpdate) | Some(CardAction::CheckUpdates)), "{got:?}");
        // While bytes arrive the same button cancels.
        v.downloading = true;
        v.can_install = false;
        v.progress = Some(0.3);
        let got = probe(&v, install_y + 14.0);
        assert_eq!(got, Some(CardAction::CancelUpdate));
    }

    #[test]
    fn card_heights_grow_with_the_list() {
        assert!(relays_height(4, false) - relays_height(3, false) == RELAY_ROW_H);
        assert!(relays_height(3, true) > relays_height(3, false));
        assert!(more_games_height() > 60.0);
    }
}
