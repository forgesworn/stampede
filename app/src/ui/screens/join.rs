//! Join by room code (and by Keep invite): six slots, an in-game QWERTY keypad (letters of the room
//! code alphabet only), Paste, and honest connecting stages. Also the
//! "connecting" page Nearby uses once you tap a room.

use super::common;
use super::lobby::LobbyScreen;
use super::{Ctx, Screen, Transition};
use crate::keeps::{self, JoinTarget};
use crate::online::{self, OnlineSession};
use crate::ui::art::{self, rgba};
use crate::ui::icons::Icon;
use crate::ui::widgets::{self, Btn, S2, S3};
use crate::ui::{feedback, theme};
use decented_net::room::{self, ALPHABET, CODE_LEN};
use egui::{pos2, vec2, Align2, Color32, Rect, Stroke};
use race_session::ClientPhase;

/// How long one attempt may take before we say so (the transport has its own timeouts).
const GIVE_UP_SECS: f32 = 40.0;
/// A join by code that fails for a reason that may pass (the host's relays are
/// still coming up, a lost offer) is tried again until this many seconds have
/// gone by in all, then reported. The transport itself gives up after about 10 s.
pub const RETRY_BUDGET_SECS: f32 = 35.0;

/// Would trying again soon plausibly work? (Not for a full or started room, a
/// wrong version, or an unverified host.)
pub fn is_transient(why: &str) -> bool {
    let base = online::friendly_failure(why);
    [race_session::INVITE_NO_ANSWER, "Code not found", "Couldn't reach the internet meeting points", "Found the host but couldn't connect", "didn't answer over the link", "Network problem"].iter().any(|m| base.contains(m))
}

/// What Paste says when the clipboard holds no room code.
pub const NO_CODE_ON_CLIPBOARD: &str = "No room code on the clipboard.";

enum Stage {
    Entry,
    /// The session being joined is in the app's online slot.
    Connecting { age: f32 },
    Failed(String),
    /// An invite for a Keep not in My Keeps: asked about before anything is dialled.
    Confirm(Invite),
}

/// Starts a join by code; replaced in tests.
type Connector = Box<dyn FnMut(&str) -> Result<OnlineSession, String>>;
/// Starts a join by invite (host key, secret); replaced in tests.
type InviteConnector = Box<dyn FnMut(&[u8; 32], &[u8; 32]) -> Result<OnlineSession, String>>;

/// The Keep being joined: the two halves of an invite link.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Invite {
    host_key: [u8; 32],
    secret: [u8; 32],
}

pub struct JoinScreen {
    code: String,
    stage: Stage,
    /// What we are joining, for the connecting page.
    label: String,
    t: f32,
    shake: f32,
    hint: Option<(String, f32)>,
    /// Join this on the first frame (a link opened the app).
    auto: Option<JoinTarget>,
    /// The Keep being joined, when this is an invite join (else a room code).
    invite: Option<Invite>,
    /// This join came from a My Keeps row: a retry dials the secret My Keeps holds then.
    saved: bool,
    /// This join's Keep has been saved to My Keeps (once per join, not once per frame).
    recorded: bool,
    /// Seconds the "Join a Keep?" card has been asking about its current Keep.
    confirm_age: f32,
    /// The failure on screen is of a Keep that is saved in My Keeps: offer Forget.
    can_forget: bool,
    /// Nearby handed over a session already connecting.
    pending: Option<Box<OnlineSession>>,
    /// Attempt number (a join by code is retried while the host's relays come up).
    attempt: u32,
    /// Seconds since the first attempt of this join.
    total: f32,
    /// Retry transient failures (joins by code; not a Nearby room).
    retry: bool,
    connector: Option<Connector>,
    invite_connector: Option<InviteConnector>,
}

impl Default for JoinScreen {
    fn default() -> Self {
        Self::new()
    }
}

/// The room code in clipboard text: a bare code, `KWT QPM`, or a `stampede://join#CODE` link.
pub fn code_in(text: &str) -> Option<String> {
    let t = text.trim();
    if let Some(c) = room::normalize_code(t) {
        return Some(c);
    }
    if let Some((_, tail)) = t.rsplit_once('#') {
        if let Some(c) = room::normalize_code(tail) {
            return Some(c);
        }
    }
    t.split(|c: char| !c.is_ascii_alphabetic()).find_map(|w| (w.len() == CODE_LEN).then(|| room::normalize_code(w)).flatten())
}

impl JoinScreen {
    pub fn new() -> JoinScreen {
        JoinScreen { code: String::new(), stage: Stage::Entry, label: String::new(), t: 0.0, shake: 0.0, hint: None, auto: None, invite: None, saved: false, recorded: false, confirm_age: 0.0, can_forget: false, pending: None, attempt: 0, total: 0.0, retry: true, connector: None, invite_connector: None }
    }

    /// Straight into joining (a `stampede://join#CODE` link).
    pub fn with_code(code: &str) -> JoinScreen {
        match room::normalize_code(code) {
            Some(c) => JoinScreen::with_target(JoinTarget::Code(c)),
            None => JoinScreen::new(),
        }
    }

    /// Straight into joining a room or a Keep (a launch link, a saved Keep, a pasted invite).
    pub fn with_target(target: JoinTarget) -> JoinScreen {
        JoinScreen { auto: Some(target), ..JoinScreen::new() }
    }

    /// Straight into joining a Keep from My Keeps (its row's two halves).
    pub fn rejoining(host_key: [u8; 32], secret: [u8; 32]) -> JoinScreen {
        JoinScreen { saved: true, ..JoinScreen::with_target(JoinTarget::Invite { host_key, secret }) }
    }

    /// The connecting page for a session already dialling (Nearby).
    pub fn connecting(s: OnlineSession, label: &str) -> JoinScreen {
        JoinScreen { label: label.to_string(), pending: Some(Box::new(s)), retry: false, ..JoinScreen::new() }
    }

    /// A failed attempt at a Keep (screenshots).
    pub fn failed_keep_sample() -> JoinScreen {
        JoinScreen { stage: Stage::Failed(keeps::UNREACHABLE.into()), can_forget: true, invite: Some(Invite { host_key: [0xab; 32], secret: [1; 32] }), ..JoinScreen::new() }
    }

    /// A failed attempt (screenshots).
    pub fn failed_sample() -> JoinScreen {
        JoinScreen { code: "KWTQPM".into(), stage: Stage::Failed("Code not found (check the code, or the host may have started).".into()), ..JoinScreen::new() }
    }

    /// Six letters typed (screenshots).
    pub fn sample() -> JoinScreen {
        JoinScreen { code: "KWTQ".into(), ..JoinScreen::new() }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.hint = Some((text.into(), 2.8));
    }

    /// Join tests drive the screen without a network.
    #[cfg(test)]
    pub fn with_connector(mut self, c: impl FnMut(&str) -> Result<OnlineSession, String> + 'static) -> JoinScreen {
        self.connector = Some(Box::new(c));
        self
    }

    /// Join tests drive an invite join without a network.
    #[cfg(test)]
    pub fn with_invite_connector(mut self, c: impl FnMut(&[u8; 32], &[u8; 32]) -> Result<OnlineSession, String> + 'static) -> JoinScreen {
        self.invite_connector = Some(Box::new(c));
        self
    }

    /// Start joining `code` from scratch (a new attempt count).
    fn begin(&mut self, ctx: &mut Ctx, code: &str) {
        self.attempt = 0;
        self.recorded = false;
        self.total = 0.0;
        self.retry = true;
        self.invite = None;
        self.can_forget = false;
        self.attempt_join(ctx, code);
    }

    /// Start joining a room or a Keep from scratch.
    fn begin_target(&mut self, ctx: &mut Ctx, target: JoinTarget) {
        match target {
            // A room link says what it likes: only a real code is dialled.
            JoinTarget::Code(code) => match room::normalize_code(&code) {
                Some(code) => {
                    self.code = code.clone();
                    self.begin(ctx, &code);
                }
                None => {
                    self.code.clear();
                    self.invite = None;
                    self.can_forget = false;
                    self.stage = Stage::Entry;
                }
            },
            JoinTarget::Invite { host_key, secret } => {
                self.code.clear();
                let invite = Invite { host_key, secret };
                if keeps::contains(&keeps::load(ctx.data_dir), &race_session::invite::hex_encode(&host_key)) {
                    self.begin_invite(ctx, invite);
                } else {
                    // Someone else's Keep: nothing is sent until the player says so (or already
                    // has, in the card that took them out of a room).
                    if ctx.take_join_confirmed(&host_key) {
                        self.begin_invite(ctx, invite);
                    } else {
                        self.invite = None;
                        self.can_forget = false;
                        self.confirm_age = 0.0;
                        self.stage = Stage::Confirm(invite);
                    }
                }
            }
            JoinTarget::BadInvite => {
                self.code.clear();
                self.invite = None;
                self.can_forget = false;
                self.stage = Stage::Failed(keeps::NOT_AN_INVITE.into());
            }
        }
    }

    fn begin_invite(&mut self, ctx: &mut Ctx, invite: Invite) {
        self.attempt = 0;
        self.recorded = false;
        self.total = 0.0;
        self.retry = true;
        self.invite = Some(invite);
        let host_hex = race_session::invite::hex_encode(&invite.host_key);
        self.label = keeps::load(ctx.data_dir).iter().find(|k| k.host_key == host_hex).map(|k| k.name.clone()).unwrap_or_else(|| keeps::display_name(None, &host_hex));
        self.attempt_invite(ctx, invite);
    }

    /// `invite`, with the secret My Keeps holds now when this join came from a My Keeps row.
    fn current_invite(&mut self, data_dir: &std::path::Path, invite: Invite) -> Invite {
        if !self.saved {
            return invite;
        }
        let host_hex = race_session::invite::hex_encode(&invite.host_key);
        match keeps::load(data_dir).iter().find(|k| k.host_key == host_hex).and_then(keeps::Keep::keys) {
            Some((host_key, secret)) => {
                let fresh = Invite { host_key, secret };
                self.invite = Some(fresh);
                fresh
            }
            None => invite,
        }
    }

    fn attempt_invite(&mut self, ctx: &mut Ctx, invite: Invite) {
        let invite = self.current_invite(ctx.data_dir, invite);
        self.attempt += 1;
        let r = match self.invite_connector.as_mut() {
            Some(c) => c(&invite.host_key, &invite.secret),
            None => OnlineSession::join_invite(ctx.data_dir, invite.host_key, invite.secret, &ctx.profile.name, ctx.profile.character),
        };
        match r {
            Ok(s) => {
                ctx.online.put(s);
                self.stage = Stage::Connecting { age: 0.0 };
            }
            Err(e) => self.stage = Stage::Failed(e),
        }
    }

    /// The failure to show, once the join is given up on.
    fn failure_text(&self, why: String) -> String {
        if self.invite.is_some() {
            keeps::invite_failure_text(&why)
        } else {
            why
        }
    }

    fn attempt_join(&mut self, ctx: &mut Ctx, code: &str) {
        self.attempt += 1;
        let r = match self.connector.as_mut() {
            Some(c) => c(code),
            None => OnlineSession::join_code(ctx.data_dir, code, &ctx.profile.name, ctx.profile.character),
        };
        match r {
            Ok(s) => {
                self.label = format!("Room {}", online::spaced_code(code));
                ctx.online.put(s);
                self.stage = Stage::Connecting { age: 0.0 };
            }
            Err(e) => self.stage = Stage::Failed(e),
        }
    }

    fn fail(&mut self, ctx: &mut Ctx, why: String) {
        ctx.online.leave();
        // Forget only when the saved entry is the one just tried (a stray link must not offer to delete a working Keep).
        self.can_forget = self.invite.is_some_and(|i| keeps::load(ctx.data_dir).iter().any(|k| k.keys() == Some((i.host_key, i.secret))));
        self.stage = Stage::Failed(self.failure_text(why));
    }

    fn push_letter(&mut self, c: char) {
        if self.code.chars().count() < CODE_LEN {
            self.code.push(c);
        }
    }
}

enum Step {
    /// In the lobby (or beyond); the host's own name, if the link proved one.
    Seated(Option<String>),
    Dead(String),
    Waiting,
    Gone,
}

/// Keyboard letter -> char.
fn typed_letters(ctx: &Ctx) -> Vec<char> {
    use winit::keyboard::KeyCode as K;
    const KEYS: [(K, char); 26] = [
        (K::KeyA, 'A'),
        (K::KeyB, 'B'),
        (K::KeyC, 'C'),
        (K::KeyD, 'D'),
        (K::KeyE, 'E'),
        (K::KeyF, 'F'),
        (K::KeyG, 'G'),
        (K::KeyH, 'H'),
        (K::KeyI, 'I'),
        (K::KeyJ, 'J'),
        (K::KeyK, 'K'),
        (K::KeyL, 'L'),
        (K::KeyM, 'M'),
        (K::KeyN, 'N'),
        (K::KeyO, 'O'),
        (K::KeyP, 'P'),
        (K::KeyQ, 'Q'),
        (K::KeyR, 'R'),
        (K::KeyS, 'S'),
        (K::KeyT, 'T'),
        (K::KeyU, 'U'),
        (K::KeyV, 'V'),
        (K::KeyW, 'W'),
        (K::KeyX, 'X'),
        (K::KeyY, 'Y'),
        (K::KeyZ, 'Z'),
    ];
    KEYS.iter().filter(|(k, _)| ctx.keys.pressed(*k)).map(|(_, c)| *c).collect()
}

impl Screen for JoinScreen {
    fn name(&self) -> &'static str {
        "join"
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn uses_online(&self) -> bool {
        true
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        self.shake = (self.shake - ctx.dt * 3.0).max(0.0);
        if let Some((_, left)) = self.hint.as_mut() {
            *left -= ctx.dt;
            if *left <= 0.0 {
                self.hint = None;
            }
        }
        if let Some(s) = self.pending.take() {
            ctx.online.put(*s);
            self.attempt = 1;
            self.stage = Stage::Connecting { age: 0.0 };
        }
        // A join link while here: join that room (or Keep) instead.
        if let Some(target) = ctx.take_join_code() {
            let already = match (&target, self.invite) {
                (JoinTarget::Code(c), None) => self.code == *c,
                (JoinTarget::Invite { host_key, secret }, Some(i)) => i.host_key == *host_key && i.secret == *secret,
                _ => false,
            };
            if !already || !matches!(self.stage, Stage::Connecting { .. }) {
                ctx.online.leave();
                self.auto = Some(target);
            }
        }
        if let Some(target) = self.auto.take() {
            self.begin_target(ctx, target);
        }
        match &mut self.stage {
            Stage::Entry => {
                let letters = typed_letters(ctx);
                for c in letters {
                    if ALPHABET.contains(c) {
                        self.push_letter(c);
                    } else {
                        self.shake = 1.0;
                        self.say(room::rejected_hint(c));
                    }
                }
                if ctx.keys.pressed(winit::keyboard::KeyCode::Backspace) {
                    self.code.pop();
                }
                if ctx.keys.pressed(winit::keyboard::KeyCode::Enter) && self.code.chars().count() == CODE_LEN {
                    let c = self.code.clone();
                    self.begin(ctx, &c);
                }
            }
            Stage::Connecting { age } => {
                *age += ctx.dt;
                let age = *age;
                self.total += ctx.dt;
                // Seated in the lobby (or the host's race already began in the
                // same frame as our welcome): hand over to the lobby screen,
                // which goes on to the race if there is one.
                let step = ctx
                    .online
                    .with(|s| {
                        s.pump(race_core::Input::default());
                        if let Some(why) = s.dead_reason() {
                            return Step::Dead(why);
                        }
                        let seated = match s.me().phase() {
                            ClientPhase::Lobby => s.me().lobby().is_some(),
                            ClientPhase::Racing => s.me().view().is_some(),
                            ClientPhase::Results => true,
                            _ => false,
                        };
                        if seated {
                            Step::Seated(s.me().host_identity().map(|h| h.name.clone()))
                        } else {
                            Step::Waiting
                        }
                    })
                    .unwrap_or(Step::Gone);
                match step {
                    Step::Seated(host_name) => {
                        // A Keep got us in: remember it for My Keeps. The screen keeps running
                        // while it fades out, so this step comes round every frame: save once.
                        if let (Some(i), false) = (self.invite, self.recorded) {
                            self.recorded = true;
                            match keeps::record_join(ctx.data_dir, &i.host_key, &i.secret, host_name.as_deref(), keeps::unix_now()) {
                                Ok(true) => log::info!("join: saved {} in My Keeps", keeps::describe(&JoinTarget::Invite { host_key: i.host_key, secret: i.secret })),
                                Ok(false) => {}
                                Err(e) => log::warn!("join: could not save the Keep: {e}"),
                            }
                        }
                        return Transition::Replace(Box::new(LobbyScreen::new()));
                    }
                    Step::Dead(why) => {
                        log::info!("join: attempt {} failed after {:.0}s: {why}", self.attempt, self.total);
                        let can_ask_again = self.invite.is_some() || self.code.chars().count() == CODE_LEN;
                        if self.retry && can_ask_again && is_transient(&why) && self.total < RETRY_BUDGET_SECS {
                            // The host's relays may still be coming up (or our offer was lost): ask again.
                            ctx.online.leave();
                            match self.invite {
                                Some(i) => self.attempt_invite(ctx, i),
                                None => {
                                    let c = self.code.clone();
                                    self.attempt_join(ctx, &c);
                                }
                            }
                        } else {
                            self.fail(ctx, why);
                        }
                    }
                    Step::Gone => self.stage = Stage::Failed("The connection was cancelled.".into()),
                    Step::Waiting if age > GIVE_UP_SECS => {
                        log::info!("join: gave up after {age:.0}s");
                        self.fail(ctx, "That's taking too long. Check you are online and the host is still waiting, then try again.".into());
                    }
                    Step::Waiting => {}
                }
            }
            Stage::Confirm(_) => self.confirm_age += ctx.dt,
            Stage::Failed(_) => {}
        }
        Transition::None
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        // Cancelling a typed-code attempt returns to the keypad; anything else leaves.
        if matches!(self.stage, Stage::Connecting { .. }) {
            ctx.online.leave();
            if self.code.chars().count() == CODE_LEN {
                self.stage = Stage::Entry;
                return Transition::None;
            }
        }
        Transition::Pop
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut out = Transition::None;
        let entry = matches!(self.stage, Stage::Entry);
        let mut typed: Vec<char> = Vec::new();
        let (mut backspace, mut paste, mut go, mut retry, mut leave, mut forget) = (false, false, false, false, false, false);
        let invite = self.invite.is_some();
        let can_forget = self.can_forget;
        let code = self.code.clone();
        let shake = self.shake;
        let hint = self.hint.clone();
        let label = self.label.clone();
        let stage_text = match &self.stage {
            Stage::Connecting { .. } => ctx
                .online
                .with(|s| match s.me().phase() {
                    ClientPhase::Connecting(text) => text.clone(),
                    _ => "Joining...".into(),
                })
                .unwrap_or_else(|| "Joining...".into()),
            _ => String::new(),
        };
        let progress = if self.attempt > 1 && invite {
            format!("Still looking for the Keep... (try {})", self.attempt)
        } else if self.attempt > 1 {
            format!("Still looking... the host may still be getting ready (try {})", self.attempt)
        } else {
            "This can take a few seconds.".to_string()
        };
        let failed = match &self.stage {
            Stage::Failed(m) => Some(m.clone()),
            _ => None,
        };
        let confirm = match &self.stage {
            Stage::Confirm(i) => Some(*i),
            _ => None,
        };
        let (mut confirm_yes, mut confirm_no) = (false, false);
        common::page(ctx.viewport.screen, tm, ui, 0.30, |ui, _brand| {
            let hd = widgets::header(ui, safe, if entry { "Join by code" } else if failed.is_some() { "Couldn't join" } else { "Joining" }, widgets::enter(t, 0));
            if hd.back.clicked() {
                leave = true;
            }
            let body = hd.body;
            if entry {
                // ── the slots and the buttons ──
                let portrait = common::Layout::new(safe).portrait;
                let slot_w = 48.0_f32.min((body.width() - 8.0 - 35.0) / CODE_LEN as f32);
                let slot_h = 60.0;
                let gap = 7.0;
                let slots_w = CODE_LEN as f32 * slot_w + (CODE_LEN as f32 - 1.0) * gap;
                let top = body.top() + 2.0;
                let p = ui.painter().clone();
                let sx = (shake * 40.0).sin() * shake * 6.0;
                let n = code.chars().count();
                for i in 0..CODE_LEN {
                    let r = Rect::from_min_size(pos2(body.left() + 4.0 + sx + i as f32 * (slot_w + gap), top), vec2(slot_w, slot_h));
                    let e = widgets::enter(t, 1 + i / 3);
                    let mut q = p.clone();
                    q.set_opacity(e);
                    widgets::well(&q, r, 12.0);
                    let cur = i == n;
                    if cur {
                        q.rect_stroke(r, 12.0, Stroke::new(2.0_f32, rgba(0xFF, 0xC8, 0x3D, 0.6 + 0.4 * (tm * 6.0).sin().abs())), egui::StrokeKind::Inside);
                    }
                    if let Some(c) = code.chars().nth(i) {
                        art::outlined(&q, r.center(), Align2::CENTER_CENTER, &c.to_string(), theme::title(36.0), theme::TEXT, theme::OUTLINE, 2.5, 0.0);
                    }
                }
                let bx = if portrait { body.left() } else { body.left() + 4.0 + slots_w + S3 * 2.0 };
                let bw = if portrait { (body.width() - S2) * 0.5 } else { body.right() - bx };
                let ph = if portrait { 44.0 } else { (slot_h - S2) / 2.0 };
                let by = top + if portrait { slot_h + 12.0 } else { 0.0 };
                if widgets::button(ui, Rect::from_min_size(pos2(bx, by), vec2(bw, ph)), "join-paste", &Btn::new("Paste code").size(15.0)).clicked() {
                    paste = true;
                }
                let ready = n == CODE_LEN;
                if widgets::button(ui, Rect::from_min_size(pos2(if portrait { bx + bw + S2 } else { bx }, if portrait { by } else { by + ph + S2 }), vec2(bw, ph)), "join-go", &Btn::primary("Join").enabled(ready).size(18.0)).clicked() {
                    go = true;
                }
                // the line under the slots
                let hint_y = top + slot_h + if portrait { 76.0 } else { 14.0 };
                match &hint {
                    Some((h, _)) => {
                        p.text(pos2(body.left() + 4.0, hint_y), Align2::LEFT_CENTER, h, theme::ui_bold(12.0), Color32::from_rgb(0xFF, 0xB0, 0x8A));
                    }
                    None => {
                        p.text(pos2(body.left() + 4.0, hint_y), Align2::LEFT_CENTER, "Ask the host for their six-letter room code.", theme::ui_medium(12.0), theme::TEXT_DIM);
                    }
                }
                // ── the keypad ──
                let rows = ["QWERTYUP", "ASDFGHJKL", "ZXCVBNM"];
                let kgap = 6.0;
                let kw = ((body.width() - kgap * 8.0) / 9.0).min(62.0);
                let kh = ((body.bottom() - (hint_y + 14.0)) - kgap * 2.0) / 3.0;
                let kh = kh.clamp(34.0, 52.0);
                let ky = body.bottom() - (kh * 3.0 + kgap * 2.0);
                for (ri, keys) in rows.iter().enumerate() {
                    let n_keys = keys.chars().count() + usize::from(ri == 2);
                    let total = n_keys as f32 * kw + (n_keys as f32 - 1.0) * kgap + if ri == 2 { kw * 0.5 } else { 0.0 };
                    let x0 = body.left() + (body.width() - total) / 2.0;
                    let y = ky + ri as f32 * (kh + kgap);
                    for (ci, c) in keys.chars().enumerate() {
                        let r = Rect::from_min_size(pos2(x0 + ci as f32 * (kw + kgap), y), vec2(kw, kh));
                        let e = widgets::enter(t, 3 + ri);
                        let clicked = widgets::staged(ui, e, vec2(0.0, 20.0), |ui, off| widgets::button(ui, r.translate(off), &format!("join-key-{c}"), &Btn::new(&c.to_string()).size(22.0)).clicked());
                        if clicked {
                            typed.push(c);
                        }
                    }
                    if ri == 2 {
                        let r = Rect::from_min_size(pos2(x0 + keys.chars().count() as f32 * (kw + kgap), y), vec2(kw * 1.5, kh));
                        if widgets::button(ui, r, "join-bs", &Btn::new("").icon(Icon::Backspace)).clicked() {
                            backspace = true;
                        }
                    }
                }
            } else if let Some(msg) = &failed {
                let card = Rect::from_center_size(body.center(), vec2(body.width().min(560.0), body.height().min(240.0)));
                let p = ui.painter().clone();
                widgets::panel(&p, card, 24.0);
                p.text(pos2(card.center().x, card.top() + 38.0), Align2::CENTER_CENTER, "Couldn't join", theme::title(26.0), Color32::from_rgb(0xFF, 0x8A, 0x7A));
                let g = p.layout(msg.clone(), theme::ui_medium(14.0), theme::TEXT, card.width() - 56.0);
                p.galley(pos2(card.left() + 28.0, card.top() + 66.0), g, theme::TEXT);
                let can_retry = code.chars().count() == CODE_LEN || invite;
                let buttons = 1 + usize::from(can_forget) + usize::from(can_retry);
                let bw = (card.width() - 56.0 - S3 * (buttons as f32 - 1.0)) / buttons as f32;
                let by = card.bottom() - 22.0 - 46.0;
                let mut bx = card.left() + 28.0;
                if widgets::button(ui, Rect::from_min_size(pos2(bx, by), vec2(bw, 46.0)), "join-fail-back", &Btn::new("Back")).clicked() {
                    leave = true;
                }
                bx += bw + S3;
                if can_forget {
                    if widgets::button(ui, Rect::from_min_size(pos2(bx, by), vec2(bw, 46.0)), "join-forget", &Btn::new("Forget")).clicked() {
                        forget = true;
                    }
                    bx += bw + S3;
                }
                if can_retry && widgets::button(ui, Rect::from_min_size(pos2(bx, by), vec2(bw, 46.0)), "join-retry", &Btn::primary("Try again")).clicked() {
                    retry = true;
                }
            } else if confirm.is_some() {
                // the question is drawn over the page, below
            } else {
                // connecting
                let card = Rect::from_center_size(body.center(), vec2(body.width().min(520.0), body.height().min(220.0)));
                let p = ui.painter().clone();
                widgets::panel(&p, card, 24.0);
                let c = pos2(card.left() + 78.0, card.center().y - 10.0);
                p.circle_stroke(c, 30.0, Stroke::new(5.0_f32, rgba(255, 255, 255, 0.12)));
                let a0 = tm * 5.0;
                let pts: Vec<egui::Pos2> = (0..=18).map(|i| c + vec2((a0 + i as f32 * 0.07).cos(), (a0 + i as f32 * 0.07).sin()) * 30.0).collect();
                p.add(egui::Shape::line(pts, Stroke::new(5.0_f32, theme::ACCENT)));
                let tx = card.left() + 140.0;
                p.text(pos2(tx, card.top() + 54.0), Align2::LEFT_CENTER, &stage_text, theme::title(26.0), theme::TEXT);
                p.text(pos2(tx, card.top() + 86.0), Align2::LEFT_CENTER, &label, theme::ui_bold(14.0), theme::SAND);
                p.text(pos2(tx, card.top() + 110.0), Align2::LEFT_CENTER, &progress, theme::ui_medium(12.0), theme::TEXT_DIM);
                if widgets::button(ui, Rect::from_min_size(pos2(tx, card.bottom() - 22.0 - 42.0), vec2(card.right() - 28.0 - tx, 42.0)), "join-cancel", &Btn::new("Cancel")).clicked() {
                    leave = true;
                }
            }
        });
        if let Some(i) = confirm {
            match common::keep_confirm_prompt(ui, ctx.viewport.screen, &race_session::invite::hex_encode(&i.host_key)) {
                Some(true) => confirm_yes = true,
                Some(false) => confirm_no = true,
                None => {}
            }
        }
        // apply
        // A second link may have swapped the card in this very frame: a tap that landed on
        // the old one must not answer the new one.
        if confirm_yes && self.confirm_age >= common::CONFIRM_GUARD_SECS {
            if let Some(i) = confirm {
                feedback::select(ctx);
                self.begin_invite(ctx, i);
            }
        }
        if confirm_no {
            feedback::back(ctx);
            return Transition::Pop;
        }
        for c in typed {
            feedback::tap(ctx);
            self.push_letter(c);
        }
        if backspace {
            feedback::tap(ctx);
            self.code.pop();
        }
        if paste {
            feedback::tap(ctx);
            match ctx.platform.read_clipboard().as_deref().and_then(code_in) {
                Some(c) => {
                    self.code = c;
                    self.hint = None;
                }
                None => self.say(NO_CODE_ON_CLIPBOARD),
            }
        }
        if go && self.code.chars().count() == CODE_LEN {
            feedback::select(ctx);
            let c = self.code.clone();
            self.begin(ctx, &c);
        }
        if retry {
            feedback::select(ctx);
            self.stage = Stage::Entry;
            if let Some(i) = self.invite {
                self.begin_invite(ctx, i);
            } else if self.code.chars().count() == CODE_LEN {
                let c = self.code.clone();
                self.begin(ctx, &c);
            }
        }
        if forget {
            feedback::select(ctx);
            if let Some(i) = self.invite {
                let host_hex = race_session::invite::hex_encode(&i.host_key);
                match keeps::forget_saved(ctx.data_dir, &host_hex) {
                    Ok(()) => ctx.say(format!("Forgot {}.", self.label)),
                    Err(e) => ctx.say(format!("Couldn't forget that Keep: {e}")),
                }
            }
            self.can_forget = false;
            return Transition::Pop;
        }
        if leave {
            out = self.back(ctx);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_found_in_pasted_text() {
        assert_eq!(code_in("kwtqpm").as_deref(), Some("KWTQPM"));
        assert_eq!(code_in(" KWT QPM ").as_deref(), Some("KWTQPM"));
        assert_eq!(code_in("stampede://join#KWTQPM").as_deref(), Some("KWTQPM"));
        assert_eq!(code_in("Join my race: KWTQPM tonight").as_deref(), Some("KWTQPM"));
        assert_eq!(code_in("hello"), None);
        assert_eq!(code_in("KWTQPI"), None, "I is not in the alphabet");
    }

    #[test]
    fn a_retry_of_a_my_keeps_join_dials_the_secret_in_the_file() {
        let d = std::env::temp_dir().join(format!("stampede-join-test-{}-retry", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        keeps::record_join(&d, &[7; 32], &[1; 32], None, keeps::unix_now()).unwrap();
        let tried = Invite { host_key: [7; 32], secret: [1; 32] };
        // The same Keep is joined again by a newer invite while the failure card is up.
        keeps::record_join(&d, &[7; 32], &[2; 32], None, keeps::unix_now()).unwrap();
        let mut from_row = JoinScreen::rejoining([7; 32], [1; 32]);
        assert_eq!(from_row.current_invite(&d, tried).secret, [2; 32]);
        assert_eq!(from_row.invite.map(|i| i.secret), Some([2; 32]));
        // A pasted or tapped link brings its own secret and keeps it.
        let mut from_link = JoinScreen::with_target(JoinTarget::Invite { host_key: [7; 32], secret: [1; 32] });
        assert_eq!(from_link.current_invite(&d, tried).secret, [1; 32]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_keypad_has_each_code_letter_once() {
        let mut keys: Vec<char> = "QWERTYUPASDFGHJKLZXCVBNM".chars().collect();
        keys.sort_unstable();
        let mut alpha: Vec<char> = ALPHABET.chars().collect();
        alpha.sort_unstable();
        assert_eq!(keys, alpha);
    }
}
