//! Settings: name (an in-game keypad, no OS text field), auto-accelerate, steer
//! mode, camera, graphics, volumes, haptics, left-handed. Changes apply live
//! and are saved when the finger lifts. Below the two panels the page scrolls on
//! to the RELAYS, UPDATES and MORE GAMES cards (`settings_cards.rs`, spec 20).

use super::common;
use super::settings_cards::{self, CardAction};
use super::{Ctx, Screen, Transition};
use crate::platform::data_dir::{CameraView, Quality, SteerMode};
use crate::ui::art::{self, ease_out_back, ease_out_cubic, phase, rgba};
use crate::ui::icons::Icon;
use crate::ui::widgets::{self, Btn, Seg, S3, S4};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Rect, Stroke};

/// Longest name.
pub const NAME_MAX: usize = 12;

/// Longest relay address the keypad takes.
pub const RELAY_INPUT_MAX: usize = 200;
/// Gap between the panels and the cards below them.
const GAP: f32 = 12.0;

/// What a staged screenshot shows in the UPDATES card.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Demo {
    None,
    /// A newer release is on offer.
    Offer,
    /// ...and its download is half way.
    Downloading,
}

pub struct SettingsScreen {
    t: f32,
    keypad: Option<Keypad>,
    dirty: bool,
    /// Pixels the page is scrolled up.
    scroll: f32,
    /// Scroll to the cards on the first frame (the Home update pill, screenshots).
    to_cards: bool,
    /// What is typed in the "Add a relay" field.
    relay_input: String,
    demo: Demo,
    /// The first frame ran (staged state is set up once).
    started: bool,
}

struct Keypad {
    buf: String,
    shift: bool,
    /// Seconds open.
    age: f32,
    /// Typing a relay address: lower case, no shift, `. - : /` and Paste instead of space.
    url: bool,
}

impl Keypad {
    fn new(name: &str) -> Keypad {
        Keypad { buf: name.to_string(), shift: name.is_empty(), age: 0.0, url: false }
    }

    fn new_url(text: &str) -> Keypad {
        Keypad { buf: text.to_string(), shift: false, age: 0.0, url: true }
    }

    fn max(&self) -> usize {
        if self.url {
            RELAY_INPUT_MAX
        } else {
            NAME_MAX
        }
    }

    fn press(&mut self, c: char) {
        if self.buf.chars().count() < self.max() {
            if self.url {
                self.buf.push(c.to_ascii_lowercase());
                return;
            }
            self.buf.push(if self.shift { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() });
            self.shift = self.buf.ends_with(' ');
        }
    }

    fn backspace(&mut self) {
        self.buf.pop();
        if !self.url && (self.buf.is_empty() || self.buf.ends_with(' ')) {
            self.shift = true;
        }
    }
}

impl Default for SettingsScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsScreen {
    pub fn new() -> SettingsScreen {
        SettingsScreen { t: 0.0, keypad: None, dirty: false, scroll: 0.0, to_cards: false, relay_input: String::new(), demo: Demo::None, started: false }
    }

    /// Settings scrolled to the cards (the Home update pill opens it; screenshots).
    pub fn at_cards() -> SettingsScreen {
        SettingsScreen { to_cards: true, ..SettingsScreen::new() }
    }

    /// The name keypad open (screenshots).
    pub fn keypad_sample() -> SettingsScreen {
        let mut s = SettingsScreen::new();
        s.keypad = Some(Keypad { buf: "Rider".into(), shift: false, age: 1.0, url: false });
        s
    }

    /// The relay keypad open with an address half typed (screenshots).
    pub fn relay_keypad_sample() -> SettingsScreen {
        let mut s = SettingsScreen::at_cards();
        s.keypad = Some(Keypad { age: 1.0, ..Keypad::new_url("relay.example.com") });
        s
    }

    /// A newer release on offer in the UPDATES card (screenshots).
    pub fn updates_sample(downloading: bool) -> SettingsScreen {
        SettingsScreen { to_cards: true, demo: if downloading { Demo::Downloading } else { Demo::Offer }, ..SettingsScreen::new() }
    }

    /// Type into the relay field (tests).
    pub fn set_relay_input(&mut self, text: &str) {
        self.relay_input = text.to_string();
    }
}

impl SettingsScreen {
    /// The relay list changed: tell everything that uses it, and save.
    fn relays_changed(&mut self, ctx: &mut Ctx) {
        crate::platform::set_relays(ctx.profile.relays.clone());
        ctx.updates.set_user_relays(ctx.profile.relays.clone());
        self.dirty = true;
    }

    /// Act on a tap in one of the cards. Returns a screen change, if any.
    fn card_action(&mut self, ctx: &mut Ctx, a: CardAction) -> Option<Transition> {
        use crate::platform::data_dir::{add_relay, default_relays, move_relay};
        match a {
            CardAction::MoveRelay(i, up) => {
                if move_relay(&mut ctx.profile.relays, i, up) {
                    feedback::tap(ctx);
                    log::info!("relays: moved #{} {} ({} now first)", i + 1, if up { "up" } else { "down" }, ctx.profile.relays[0]);
                    self.relays_changed(ctx);
                }
            }
            CardAction::RemoveRelay(i) => {
                if ctx.profile.relays.len() <= 1 {
                    ctx.say("Keep at least one relay.");
                } else if i < ctx.profile.relays.len() {
                    feedback::tap(ctx);
                    ctx.profile.relays.remove(i);
                    self.relays_changed(ctx);
                }
            }
            CardAction::ShowRelayHealth(i) => {
                if let Some(url) = ctx.profile.relays.get(i) {
                    let line = settings_cards::relay_health_line(url, &decented_relays::RelayPool::global().view(url));
                    feedback::tap(ctx);
                    ctx.say(line);
                }
            }
            CardAction::TypeRelay => {
                feedback::tap(ctx);
                self.keypad = Some(Keypad::new_url(&self.relay_input));
            }
            CardAction::PasteRelay => match ctx.platform.read_clipboard() {
                Some(t) if !t.trim().is_empty() => {
                    feedback::tap(ctx);
                    self.relay_input = t.trim().chars().take(RELAY_INPUT_MAX).collect();
                }
                _ => ctx.say("Nothing to paste."),
            },
            CardAction::AddRelay => match add_relay(&mut ctx.profile.relays, &self.relay_input) {
                Ok(url) => {
                    feedback::select(ctx);
                    self.relays_changed(ctx);
                    self.relay_input.clear();
                    log::info!("relays: added {url} ({} now)", ctx.profile.relays.len());
                    ctx.say("Relay added.");
                }
                Err(msg) => ctx.say(msg),
            },
            CardAction::RestoreRelays => {
                feedback::tap(ctx);
                ctx.profile.relays = default_relays();
                self.relays_changed(ctx);
                ctx.say("Default relays restored.");
            }
            CardAction::CheckUpdates => {
                feedback::tap(ctx);
                ctx.updates.check_now();
            }
            CardAction::InstallUpdate => {
                feedback::select(ctx);
                if let Err(notice) = ctx.updates.install(&mut *ctx.platform) {
                    ctx.say(&notice);
                }
            }
            CardAction::CancelUpdate => {
                feedback::tap(ctx);
                ctx.updates.cancel_install(&mut *ctx.platform);
            }
            CardAction::MoreGames => {
                feedback::tap(ctx);
                ctx.save_profile();
                return Some(Transition::Push(Box::new(super::games::GamesScreen::new())));
            }
        }
        None
    }
}

impl Screen for SettingsScreen {
    fn uses_tilt(&self) -> bool { true }
    fn name(&self) -> &'static str {
        "settings"
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        if !std::mem::replace(&mut self.started, true) && self.demo != Demo::None && ctx.opts.shot.is_some() {
            // Staged screenshots only: a throwaway-signed offer, never on a real run.
            ctx.updates.demo_offer("0.3.0", "Faster starts, a new track and fixes.");
            if self.demo == Demo::Downloading {
                ctx.updates.demo_install_state(crate::platform::os::ApkStatus::Downloading { done: 12_400_000, total: 27_700_000 });
            }
        }
        if let Some(k) = self.keypad.as_mut() {
            k.age += ctx.dt;
        }
        // save once the finger lifts after a change
        if self.dirty && ctx.touches.active().is_empty() {
            self.dirty = false;
            ctx.save_profile();
        }
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        if self.keypad.take().is_some() {
            return Transition::None;
        }
        ctx.save_profile();
        Transition::Pop
    }

    fn on_background(&mut self, ctx: &mut Ctx) {
        ctx.save_profile();
        feedback::on_background();
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        let mut dirty = false;
        let mut keypad = self.keypad.take();
        let keypad_open = keypad.is_some();
        let mut scroll = self.scroll;
        let mut to_cards = std::mem::take(&mut self.to_cards);
        let mut relay_input = std::mem::take(&mut self.relay_input);
        let mut card_act: Option<CardAction> = None;
        let tilt_available = ctx.platform.tilt_reading() != crate::platform::tilt::Reading::Unavailable;
        let tilt_selected = ctx.profile.steer_mode == SteerMode::Tilt;
        // Relays: the list, how each is doing (the shared pool), and the defaults notice.
        let relays = ctx.profile.relays.clone();
        let pool = decented_relays::RelayPool::global();
        pool.refresh_fallback(&relays);
        let relay_notice = pool.notice();
        let relay_views: Vec<decented_relays::RelayView> = relays.iter().map(|u| pool.view(u)).collect();
        let defaults = crate::platform::data_dir::default_relays();
        let upd = ctx.updates.view(crate::update::unix_now());
        let screen_rect = ctx.viewport.screen;
        common::page(ctx.viewport.screen, tm, ui, 0.55, |ui, _| {
            let hd = widgets::header(ui, safe, "Settings", widgets::enter(t, 0));
            if hd.back.clicked() && !keypad_open {
                feedback::back(ctx);
                ctx.save_profile();
                out = Transition::Pop;
            }
            let body = hd.body;
            let gap = S3;
            let portrait = common::Layout::new(safe).portrait;
            let cw = if portrait { body.width() } else { (body.width() - gap) / 2.0 };
            // The two panels keep their natural height; the cards follow below, and the page scrolls.
            let panel_h = body.height().min(262.0);
            let relays_h = settings_cards::relays_height(relays.len(), relay_notice.is_some());
            let upd_h = settings_cards::updates_height(ui.painter(), cw, &upd);
            let cards_h = relays_h.max(upd_h + GAP + settings_cards::more_games_height());
            let panels_h = if portrait { panel_h * 2.0 + GAP } else { panel_h };
            let content_h = panels_h + GAP + if portrait { relays_h + GAP + upd_h + GAP + settings_cards::more_games_height() } else { cards_h } + S4;
            let max_scroll = (content_h - body.height()).max(0.0);
            if to_cards {
                scroll = panels_h + GAP;
                to_cards = false;
            }
            if !keypad_open {
                // Drag the page (the controls above this take their own touches), or the mouse wheel.
                let drag = ui.interact(body, egui::Id::new("settings-scroll"), egui::Sense::drag());
                if drag.dragged() {
                    scroll -= drag.drag_delta().y;
                }
                if ui.rect_contains_pointer(body) {
                    scroll -= ui.input(|i| i.smooth_scroll_delta.y);
                }
            }
            scroll = scroll.clamp(0.0, max_scroll);
            // Everything below is clipped to the page area (under the header).
            ui.set_clip_rect(body.expand2(vec2(10.0, 0.0)));
            let sy = vec2(0.0, -scroll);
            let left = Rect::from_min_size(body.min + sy, vec2(cw, panel_h));
            let right = Rect::from_min_size((if portrait { pos2(body.left(), body.top() + panel_h + GAP) } else { pos2(body.left() + cw + gap, body.top()) }) + sy, vec2(cw, panel_h));
            let rh = 44.0;
            let label_w = 118.0;

            // a row's label and hairline; returns the control rect
            let row = |p: &egui::Painter, panel: Rect, i: usize, label: &str, sub: Option<&str>| -> Rect {
                let inner = panel.shrink2(vec2(16.0, 10.0));
                let r = Rect::from_min_size(pos2(inner.left(), inner.top() + i as f32 * (rh + 4.0)), vec2(inner.width(), rh));
                if sub.is_some() {
                    p.text(pos2(r.left(), r.center().y - 7.0), Align2::LEFT_CENTER, label, theme::ui_bold(14.0), theme::TEXT);
                    p.text(pos2(r.left(), r.center().y + 9.0), Align2::LEFT_CENTER, sub.unwrap_or(""), theme::ui_medium(10.5), theme::TEXT_DIM);
                } else {
                    p.text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, label, theme::ui_bold(14.0), theme::TEXT);
                }
                if i > 0 {
                    p.hline((r.left())..=(r.right()), r.top() - 2.0, Stroke::new(1.0_f32, rgba(255, 255, 255, 0.07)));
                }
                Rect::from_min_max(pos2(r.left() + label_w, r.top() + 4.0), pos2(r.right(), r.bottom() - 4.0))
            };

            // The panels are not drawn at all once scrolled wholly out of view.
            let panels_visible = left.bottom() > body.top();
            // ── left panel ──
            if panels_visible {
                widgets::staged(ui, widgets::enter(t, 1), vec2(-40.0, 0.0), |ui, off| {
                    let panel = left.translate(off);
                    let p = ui.painter().clone();
                    widgets::panel(&p, panel, 20.0);
                    // name
                    let c = row(&p, panel, 0, if ctx.account.is_some() { "My Signet" } else { "Name" }, None);
                    let name = ctx.profile.name.clone();
                    let b = widgets::button(ui, Rect::from_min_max(c.min, c.max).shrink2(vec2(0.0, -2.0)), "set-name", &Btn::new(&name).size(17.0));
                    p.text(pos2(c.right() - 14.0, c.center().y), Align2::RIGHT_CENTER, if ctx.account.is_some() { "account" } else { "edit" }, theme::ui_bold(10.5), theme::TEXT_DIM);
                    if b.clicked() && !keypad_open {
                        feedback::tap(ctx);
                        if ctx.account.is_some() {
                            out = Transition::Push(Box::new(super::account::AccountScreen::manage()));
                        } else { keypad = Some(Keypad::new(&ctx.profile.name)); }
                    }
                    if tilt_selected {
                        let status = match ctx.tilt.status() {
                            crate::platform::tilt::Status::Ready => "Tilt ready",
                            crate::platform::tilt::Status::Unavailable => "Use the pad",
                            _ => "Hold still to centre",
                        };
                        let c = row(&p, panel, 1, "Centre tilt", Some(status));
                        let meter = Rect::from_center_size(pos2(c.left() + 38.0, c.center().y), vec2(64.0, 6.0));
                        p.rect_filled(meter, 3.0, theme::TEXT_DIM.gamma_multiply(0.3));
                        p.circle_filled(meter.center() + vec2(ctx.tilt.steer().unwrap_or(0.0) * 30.0, 0.0), 5.0, theme::MINT);
                        let button = Rect::from_min_max(pos2(c.left() + 82.0, c.top()), c.max);
                        if widgets::button(ui, button, "set-tilt-centre", &Btn::new("Centre").size(13.0)).clicked() && !keypad_open {
                            if ctx.tilt.centre() { ctx.say("Steering centred"); }
                        }
                    } else {
                        let c = row(&p, panel, 1, "Full throttle", Some("Speed changes come from the race"));
                        p.text(c.center(), Align2::CENTER_CENTER, "Always on", theme::ui_bold(13.0), theme::MINT);
                    }
                    // steer mode
                    let c = row(&p, panel, 2, "Steering", None);
                    let segs = [Seg::new("Pad"), Seg::new("Buttons"), Seg { label: "Tilt", enabled: tilt_available, tag: (!tilt_available).then_some("sensor") }];
                    let sel = match ctx.profile.steer_mode {
                        SteerMode::Pad => 0,
                        SteerMode::Buttons => 1,
                        SteerMode::Tilt => 2,
                    };
                    if let Some(i) = widgets::segmented(ui, c, "set-steer", &segs, sel) {
                        if !keypad_open {
                            feedback::tap(ctx);
                            ctx.profile.steer_mode = [SteerMode::Pad, SteerMode::Buttons, SteerMode::Tilt][i];
                            if i == 2 { ctx.say("Hold your phone comfortably and still to centre steering. The pad still works."); }
                            dirty = true;
                        }
                    }
                    // camera
                    let c = row(&p, panel, 3, "Camera", None);
                    let segs = [Seg::new("Chase"), Seg::new("First person"), Seg::new("Far")];
                    let sel = match ctx.profile.camera {
                        CameraView::Chase => 0,
                        CameraView::FirstPerson => 1,
                        CameraView::Far => 2,
                    };
                    if let Some(i) = widgets::segmented(ui, c, "set-camera", &segs, sel) {
                        if !keypad_open {
                            feedback::tap(ctx);
                            ctx.profile.camera = [CameraView::Chase, CameraView::FirstPerson, CameraView::Far][i];
                            dirty = true;
                        }
                    }
                    // graphics
                    let c = row(&p, panel, 4, "Graphics", None);
                    let segs = [Seg::new("Low"), Seg::new("Balanced"), Seg::new("High")];
                    let sel = match ctx.profile.quality {
                        Quality::Low => 0,
                        Quality::Balanced => 1,
                        Quality::High => 2,
                    };
                    if let Some(i) = widgets::segmented(ui, c, "set-quality", &segs, sel) {
                        if !keypad_open {
                            feedback::tap(ctx);
                            ctx.profile.quality = [Quality::Low, Quality::Balanced, Quality::High][i];
                            dirty = true;
                        }
                    }
                });
            }

            // ── right panel ──
            if panels_visible {
                widgets::staged(ui, widgets::enter(t, 2), vec2(40.0, 0.0), |ui, off| {
                    let panel = right.translate(off);
                    let p = ui.painter().clone();
                    widgets::panel(&p, panel, 20.0);
                    for (i, (label, salt)) in [("Music", "set-music"), ("Effects", "set-fx")].into_iter().enumerate() {
                        let c = row(&p, panel, i, label, None);
                        let mut v = if i == 0 { ctx.profile.music_volume } else { ctx.profile.effects_volume };
                        let sl = Rect::from_min_max(pos2(c.left() - 6.0, c.top() - 4.0), pos2(c.right() - 44.0, c.bottom() + 4.0));
                        let resp = widgets::slider(ui, sl, salt, &mut v);
                        p.text(pos2(c.right(), c.center().y), Align2::RIGHT_CENTER, format!("{}%", (v * 100.0).round() as i32), theme::numbers(15.0), theme::SAND);
                        if resp.changed() && !keypad_open {
                            if i == 0 {
                                ctx.profile.music_volume = v;
                            } else {
                                ctx.profile.effects_volume = v;
                            }
                            dirty = true;
                        }
                        if resp.drag_stopped() && i == 1 && !keypad_open {
                            feedback::tap(ctx);
                        }
                    }
                    let c = row(&p, panel, 2, "Haptics", Some("Vibration on taps and bumps"));
                    let mut v = ctx.profile.haptics;
                    let r = Rect::from_min_max(pos2(panel.left() + 16.0, c.top() - 4.0), pos2(panel.right() - 16.0, c.bottom() + 4.0));
                    if widgets::toggle(ui, r, "set-haptics", &mut v).changed() && !keypad_open {
                        ctx.profile.haptics = v;
                        ctx.haptics.set_enabled(v);
                        feedback::tap(ctx);
                        dirty = true;
                    }
                    let c = row(&p, panel, 3, "Left-handed", Some("Mirror the thumb layout"));
                    let mut v = ctx.profile.left_handed;
                    let r = Rect::from_min_max(pos2(panel.left() + 16.0, c.top() - 4.0), pos2(panel.right() - 16.0, c.bottom() + 4.0));
                    if widgets::toggle(ui, r, "set-left", &mut v).changed() && !keypad_open {
                        feedback::tap(ctx);
                        ctx.profile.left_handed = v;
                        dirty = true;
                    }
                    if tilt_selected {
                        let c = row(&p, panel, 4, "Tilt sensitivity", None);
                        let mut v = (ctx.profile.tilt_sensitivity - 0.5) / 1.5;
                        let sl = Rect::from_min_max(pos2(c.left() - 6.0, c.top() - 4.0), pos2(c.right() - 44.0, c.bottom() + 4.0));
                        if widgets::slider(ui, sl, "set-tilt-sensitivity", &mut v).changed() && !keypad_open {
                            ctx.profile.tilt_sensitivity = 0.5 + v * 1.5;
                            dirty = true;
                        }
                        p.text(pos2(c.right(), c.center().y), Align2::RIGHT_CENTER, format!("{:.1}×", ctx.profile.tilt_sensitivity), theme::numbers(15.0), theme::SAND);
                    }
                });
            }

            // ── the cards: RELAYS on the left, UPDATES and MORE GAMES on the right ──
            let top = body.top() + panels_h + GAP - scroll;
            let relays_card = Rect::from_min_size(pos2(body.left(), top), vec2(cw, relays_h));
            let upd_card = Rect::from_min_size(if portrait { pos2(body.left(), top + relays_h + GAP) } else { pos2(body.left() + cw + gap, top) }, vec2(cw, upd_h));
            let more_card = Rect::from_min_size(pos2(upd_card.left(), upd_card.bottom() + GAP), vec2(cw, settings_cards::more_games_height()));
            if !keypad_open {
                settings_cards::draw_relays(ui, body, relays_card, &relays, &relay_views, relay_notice, &relay_input, &defaults, &mut card_act);
                settings_cards::draw_updates(ui, body, upd_card, &upd, tm, &mut card_act);
                settings_cards::draw_more_games(ui, body, more_card, &mut card_act);
            } else {
                // Same look, no taps while the keypad is up.
                let mut ignored = None;
                settings_cards::draw_relays(ui, body, relays_card, &relays, &relay_views, relay_notice, &relay_input, &defaults, &mut ignored);
                settings_cards::draw_updates(ui, body, upd_card, &upd, tm, &mut ignored);
                settings_cards::draw_more_games(ui, body, more_card, &mut ignored);
            }
            if max_scroll > 0.0 {
                // a thin scroll bar at the right edge
                let track = Rect::from_min_max(pos2(body.right() + 4.0, body.top() + 4.0), pos2(body.right() + 7.0, body.bottom() - 4.0));
                let h = (track.height() * body.height() / content_h).max(24.0);
                let y = track.top() + (track.height() - h) * (scroll / max_scroll);
                ui.painter().rect_filled(Rect::from_min_size(pos2(track.left(), y), vec2(track.width(), h)), 1.5, rgba(255, 255, 255, 0.28));
            }

            // ── the keypad (name or relay address) ──
            ui.set_clip_rect(screen_rect);
            if let Some(k) = keypad.as_mut() {
                let mut close = None;
                draw_keypad(ctx, ui, safe, k, &mut close);
                match close {
                    Some(true) if k.url => {
                        relay_input = k.buf.trim().to_string();
                        feedback::select(ctx);
                        keypad = None;
                    }
                    Some(true) => {
                        let n = k.buf.trim().to_string();
                        ctx.profile.name = if n.is_empty() { ctx.profile.fallback_name() } else { n };
                        dirty = true;
                        feedback::select(ctx);
                        keypad = None;
                    }
                    Some(false) => {
                        feedback::back(ctx);
                        keypad = None;
                    }
                    None => {}
                }
            }
        });
        self.keypad = keypad;
        self.scroll = scroll;
        self.relay_input = relay_input;
        self.dirty |= dirty;
        if let Some(a) = card_act {
            if let Some(t) = self.card_action(ctx, a) {
                return t;
            }
        }
        out
    }
}

/// The modal keypad: the name field, digits, QWERTY, shift, backspace, space, Done.
fn draw_keypad(ctx: &mut Ctx, ui: &mut egui::Ui, safe: Rect, k: &mut Keypad, close: &mut Option<bool>) {
    let e = ease_out_back(phase(k.age, 0.0, 0.32)).min(1.04);
    let p0 = ui.painter().clone();
    p0.rect_filled(ctx.viewport.screen, 0.0, rgba(6, 2, 14, 0.82 * ease_out_cubic(phase(k.age, 0.0, 0.2))));
    widgets::staged(ui, phase(k.age, 0.0, 0.2), vec2(0.0, 0.0), |ui, _| {
        let w = (safe.width() - 24.0).min(640.0);
        let h = (safe.height() - 16.0).min(340.0);
        let card = Rect::from_center_size(pos2(safe.center().x, safe.center().y + (1.0 - e.min(1.0)) * 30.0), vec2(w, h));
        let p = ui.painter().clone();
        widgets::panel_solid(&p, card, 22.0);
        let inner = card.shrink(12.0);
        // the field
        let field_h = 48.0;
        let field = Rect::from_min_size(inner.min, vec2(inner.width() - 96.0, field_h));
        widgets::well(&p, field, 14.0);
        let caret_on = (ctx.time * 2.0).fract() < 0.6;
        let tx = field.left() + 14.0;
        let max = k.max();
        if k.url {
            // An address: plain type, and the END of it when it is longer than the field.
            let font = theme::ui_medium(19.0);
            let room = field.width() - 28.0 - 56.0;
            let chars: Vec<char> = k.buf.chars().collect();
            let width_of = |t: &str| p.layout_no_wrap(t.to_string(), font.clone(), theme::TEXT).size().x;
            let mut shown: String = k.buf.clone();
            let mut cut = 0;
            while width_of(&shown) > room && cut < chars.len() {
                cut += 1;
                shown = std::iter::once('\u{2026}').chain(chars[cut..].iter().copied()).collect();
            }
            if shown.is_empty() {
                p.text(pos2(tx, field.center().y), Align2::LEFT_CENTER, "wss://relay.example.com", font.clone(), theme::TEXT_DIM.gamma_multiply(0.6));
            } else {
                p.text(pos2(tx, field.center().y), Align2::LEFT_CENTER, &shown, font.clone(), theme::TEXT);
            }
            if caret_on && chars.len() < max {
                let cx = tx + if shown.is_empty() { 0.0 } else { width_of(&shown) } + 3.0;
                p.rect_filled(Rect::from_center_size(pos2(cx, field.center().y), vec2(2.5, 24.0)), 1.2, theme::ACCENT);
            }
        } else {
            let name_size = art::fit_size(&p, &k.buf, theme::title, 28.0, field.width() - 66.0);
            let g = p.layout_no_wrap(k.buf.clone(), theme::title(name_size), theme::TEXT);
            art::outlined(&p, pos2(tx, field.center().y), Align2::LEFT_CENTER, &k.buf, theme::title(name_size), theme::TEXT, theme::OUTLINE, 2.0, 0.0);
            if caret_on && k.buf.chars().count() < max {
                let cx = tx + g.size().x + 3.0;
                p.rect_filled(Rect::from_center_size(pos2(cx, field.center().y), vec2(3.0, 28.0)), 1.5, theme::ACCENT);
            }
        }
        p.text(pos2(field.right() - 10.0, field.center().y), Align2::RIGHT_CENTER, format!("{}/{}", k.buf.chars().count(), max), theme::ui_bold(11.5), theme::TEXT_DIM);
        // cancel
        let cancel = Rect::from_min_size(pos2(inner.right() - 88.0, inner.top()), vec2(88.0, field_h));
        if widgets::button(ui, cancel, "kp-cancel", &Btn::ghost("Cancel").size(15.0)).clicked() {
            *close = Some(false);
        }
        // keys
        let rows = ["1234567890", "QWERTYUIOP", "ASDFGHJKL", "ZXCVBNM"];
        let gap = if common::Layout::new(safe).portrait { 3.0 } else { 6.0 };
        let top = inner.top() + field_h + 10.0;
        let kh = ((inner.bottom() - top) - gap * 4.0) / 5.0;
        let kw = (inner.width() - gap * 9.0) / 10.0;
        for (ri, keys) in rows.iter().enumerate() {
            let n = keys.chars().count();
            let mut x0 = inner.left() + (inner.width() - (n as f32 * kw + (n as f32 - 1.0) * gap)) / 2.0;
            if ri == 3 {
                // shift and backspace flank the last row
                let side = kw * 1.5;
                let total = side * 2.0 + n as f32 * kw + (n as f32 + 1.0) * gap;
                x0 = inner.left() + (inner.width() - total) / 2.0;
                let y = top + ri as f32 * (kh + gap);
                let sh = Rect::from_min_size(pos2(x0, y), vec2(side, kh));
                if !k.url && widgets::button(ui, sh, "kp-shift", &Btn::new("").icon(Icon::Jump).selected(k.shift)).clicked() {
                    feedback::tap(ctx);
                    k.shift = !k.shift;
                }
                x0 += side + gap;
                let bs = Rect::from_min_size(pos2(x0 + n as f32 * (kw + gap), y), vec2(side, kh));
                if widgets::button(ui, bs, "kp-bs", &Btn::new("").icon(Icon::Backspace)).clicked() {
                    feedback::tap(ctx);
                    k.backspace();
                }
            }
            for (ci, c) in keys.chars().enumerate() {
                let r = Rect::from_min_size(pos2(x0 + ci as f32 * (kw + gap), top + ri as f32 * (kh + gap)), vec2(kw, kh));
                let shown = if c.is_ascii_digit() || k.shift { c } else { c.to_ascii_lowercase() };
                if widgets::button(ui, r, &format!("kp-{c}"), &Btn::new(&shown.to_string()).size(21.0)).clicked() {
                    feedback::tap(ctx);
                    k.press(c);
                }
            }
        }
        // space and done
        let y = top + 4.0 * (kh + gap);
        let portrait = common::Layout::new(safe).portrait;
        let done_w = if portrait { inner.width() * 0.30 } else { 190.0 };
        let space_x = inner.left() + if portrait { 0.0 } else { kw * 1.5 + gap };
        let space_w = inner.width() - done_w - gap - if portrait { 0.0 } else { 2.0 * (kw * 1.5 + gap) };
        let space = Rect::from_min_size(pos2(space_x, y), vec2(space_w, kh));
        if k.url {
            // . - : / and Paste where the space bar is
            let keys = [".", "-", ":", "/"];
            let paste_w = space.width() * 0.34;
            let kw2 = (space.width() - paste_w - gap * keys.len() as f32) / keys.len() as f32;
            for (i, c) in keys.iter().enumerate() {
                let r = Rect::from_min_size(pos2(space.left() + i as f32 * (kw2 + gap), y), vec2(kw2, kh));
                if widgets::button(ui, r, &format!("kp-sym-{i}"), &Btn::new(c).size(21.0)).clicked() {
                    feedback::tap(ctx);
                    k.press(c.chars().next().unwrap_or('.'));
                }
            }
            let r = Rect::from_min_size(pos2(space.right() - paste_w, y), vec2(paste_w, kh));
            if widgets::button(ui, r, "kp-paste", &Btn::ghost("Paste").size(15.0)).clicked() {
                feedback::tap(ctx);
                match ctx.platform.read_clipboard() {
                    Some(t) if !t.trim().is_empty() => k.buf = t.trim().chars().take(RELAY_INPUT_MAX).collect(),
                    _ => ctx.say("Nothing to paste."),
                }
            }
        } else if widgets::button(ui, space, "kp-space", &Btn::new("space").size(15.0)).clicked() {
            feedback::tap(ctx);
            if !k.buf.is_empty() && !k.buf.ends_with(' ') && k.buf.chars().count() < NAME_MAX {
                k.buf.push(' ');
                k.shift = true;
            }
        }
        let done = Rect::from_min_size(pos2(space.right() + gap, y), vec2(done_w, kh));
        if widgets::button(ui, done, "kp-done", &Btn::primary("Done").icon(Icon::Check).size(22.0)).clicked() {
            *close = Some(true);
        }
    });
}
