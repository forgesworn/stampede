//! The multiplayer lobby: eight seats (people and bots), your ostrich, Ready,
//! and for the host the room code (Copy / Share link), the room's reachability
//! and the race settings. The same screen serves the host and the guests.

use super::common;
use super::garage::GarageScreen;
use super::migration;
use super::tracks;
use super::{Ctx, Screen, Transition};
use crate::online::{self, MigrationState, OnlineSession};
use crate::ui::art::{self, from_f3, rgba};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, Btn, Seg, S2, S3};
use crate::ui::{feedback, theme};
use egui::{pos2, vec2, Align2, Color32, Painter, Rect};
use race_core::{BotLevel, Character, CustomTrack, Input, TrackId};
use race_session::BattleSetup;
use race_session::{ClientPhase, SeatView};

pub struct LobbyScreen {
    /// A session this screen puts in the app's slot on its first frame
    /// (screenshot samples, tests); real flows put it there before pushing.
    staged: Option<OnlineSession>,
    t: f32,
    /// Staged for screenshots: guests are readied by the screen.
    sample: bool,
    /// Seconds since the last Copy / Link tap (the button says "Copied").
    copied: f32,
    copied_link: bool,
    /// Open a real room on the first frame (`--screen host`, dev).
    open_host: bool,
    /// Holds back the card for a short wait for the backup.
    mig_delay: migration::BehindDelay,
    /// The slot held a session at some point (it is gone now: leave).
    had_session: bool,
    /// The character last sent to the host (not resent every frame).
    sent_char: Option<u8>,
    /// A join link arrived while in this room: ask before leaving.
    join_prompt: Option<common::LeaveAsk>,
    /// The player chose to leave for another room: stop reading the link we just put back.
    leaving: bool,
    /// Screenshots: the host has picked Battle on this arena.
    sample_battle: Option<TrackId>,
    /// Screenshots: draw the guest panel even though the session hosts.
    guest_view: bool,
    picker: Option<Vec<CustomTrack>>,
    picker_scroll: f32,
    /// The backup picker is open (its scroll).
    protect: Option<f32>,
    /// "Protect this room?" is up: the room has an online code, and only a
    /// room on one Wi-Fi can have a backup.
    protect_sheet: bool,
    saved_custom: Option<(u64, bool)>,
    sample_downloading: bool,
}

impl Default for LobbyScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl LobbyScreen {
    /// Host migration as the lobby draws it at `now_ms` on the wall clock (a
    /// short wait for the backup held back), and whether the host can ask a
    /// backup: while nothing is armed or being armed, a question of the real
    /// state, not of what is drawn. Off unless the test setting is on
    /// (`dev::show_protect`).
    fn migration_at(&mut self, real: &MigrationState, is_host: bool, has_lobby: bool, now_ms: u64) -> (MigrationState, bool) {
        let can_protect = is_host && has_lobby && crate::dev::show_protect() && matches!(real, MigrationState::Idle | MigrationState::Refused { .. } | MigrationState::Unprotected { .. });
        (self.mig_delay.state(real, now_ms), can_protect)
    }

    /// The lobby of the session in the app's slot.
    pub fn new() -> LobbyScreen {
        LobbyScreen { staged: None, t: 0.0, sample: false, copied: 99.0, copied_link: false, open_host: false, mig_delay: Default::default(), had_session: false, sent_char: None, join_prompt: None, leaving: false, sample_battle: None, guest_view: false, picker: None, picker_scroll: 0.0, protect: None, protect_sheet: false, saved_custom: None, sample_downloading: false }
    }

    /// The lobby of `s`, which it puts in the slot on its first frame.
    pub fn with_session(s: OnlineSession) -> LobbyScreen {
        LobbyScreen { staged: Some(s), ..LobbyScreen::new() }
    }

    /// Hosts a real room as soon as it opens (`--screen host`, desktop dev). Draws an empty
    /// lobby in `--shot` runs.
    pub fn open_host() -> LobbyScreen {
        LobbyScreen { open_host: true, ..LobbyScreen::new() }
    }

    /// Designer / build lobby: open a real room on this checked track.
    pub fn invite_custom(ctx: &mut Ctx, track: &CustomTrack) -> Transition {
        if let Some(gate) = super::account::gate(ctx) { return gate; }
        match OnlineSession::host_custom(ctx.data_dir, &ctx.profile.name, ctx.profile.character, track) {
            Ok(s) => { ctx.online.put(s); Transition::Push(Box::new(Self::new())) }
            Err(e) => { ctx.say(e); Transition::None }
        }
    }

    /// A host with two people and three bots (screenshots).
    pub fn lobby_sample() -> LobbyScreen {
        LobbyScreen { sample: true, ..LobbyScreen::with_session(OnlineSession::sample_lobby(&[("Mo", 5, true)], 3, "KWTQPM")) }
    }

    /// A host with a guest and bots, Battle on Mempool Plaza picked (screenshots).
    pub fn battle_sample() -> LobbyScreen {
        LobbyScreen { sample_battle: Some(TrackId::MempoolPlaza), ..LobbyScreen::lobby_sample() }
    }

    /// The same room as a guest sees it (screenshots).
    pub fn battle_guest_sample() -> LobbyScreen {
        LobbyScreen { sample_battle: Some(TrackId::DustBowl), guest_view: true, ..LobbyScreen::lobby_sample() }
    }

    /// A host who just opened a room and waits alone (screenshots).
    pub fn host_sample() -> LobbyScreen {
        LobbyScreen { sample: true, ..LobbyScreen::with_session(OnlineSession::sample_lobby(&[], 5, "KWTQPM")) }
    }

    pub fn custom_host_sample() -> LobbyScreen {
        let raw: Vec<_> = (0..=160).map(|i| {
            let a = i as f32 / 160.0 * std::f32::consts::TAU;
            race_core::Vec2::new(a.cos() * 95.0, a.sin() * 95.0)
        }).collect();
        let shaped = race_core::shape(&raw, false, 18.0).expect("sample loop");
        let track = race_core::custom_track_from(&shaped, "Sunset Loop", TrackId::Oasis, 7, 180, vec![], 1);
        let mut s = OnlineSession::sample_lobby(&[("Mo", 5, true)], 3, "KWTQPM");
        s.select_custom_track(&track).expect("sample custom track");
        LobbyScreen { sample: true, ..LobbyScreen::with_session(s) }
    }

    pub fn custom_guest_sample() -> LobbyScreen {
        LobbyScreen { guest_view: true, ..Self::custom_host_sample() }
    }

    pub fn custom_downloading_sample() -> LobbyScreen {
        LobbyScreen { sample_downloading: true, ..Self::custom_guest_sample() }
    }

    fn leave(&mut self, ctx: &mut Ctx, say: Option<&str>) -> Transition {
        ctx.online.leave();
        if let Some(t) = say {
            ctx.say(t);
        }
        Transition::PopTo("multiplayer")
    }

    /// The host picks the backup: who can be asked, and what a backup gets.
    fn draw_protect(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let who = ctx.online.with(|s| s.me().lobby().map(|l| migration::candidates(&l.seats))).flatten().unwrap_or_default();
        let mut scroll = self.protect.unwrap_or(0.0);
        let pick = migration::backup_picker(ctx.time as f32, ctx.viewport.screen, ctx.viewport.safe, ui, &who, &mut scroll);
        self.protect = Some(scroll);
        match pick {
            Some(migration::Pick::Back) => { feedback::back(ctx); self.protect = None; }
            Some(migration::Pick::Seat(seat)) => {
                feedback::select(ctx);
                self.protect = None;
                ctx.online.with(|s| s.choose_backup(seat));
            }
            None => {}
        }
        Transition::None
    }

    fn draw_picker(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        let tracks = self.picker.as_deref().unwrap_or_default();
        let mut picked = None;
        let mut back = false;
        let mut scroll = self.picker_scroll;
        common::page(ctx.viewport.screen, ctx.time as f32, ui, 0.26, |ui, _| {
            let hd = widgets::header(ui, ctx.viewport.safe, "My Tracks", 1.0);
            back = hd.back.clicked();
            let list = hd.body;
            if tracks.is_empty() {
                ui.painter().text(list.center(), Align2::CENTER_CENTER, "Save a track in Track Designer first.", theme::ui_medium(16.0), theme::TEXT_DIM);
                return;
            }
            let rh = 54.0;
            let max_scroll = (tracks.len() as f32 * (rh + S2) - list.height()).max(0.0);
            let drag = ui.interact(list, egui::Id::new("lobby-my-tracks-scroll"), egui::Sense::drag());
            if drag.dragged() { scroll = (scroll - drag.drag_delta().y).clamp(0.0, max_scroll); }
            let p = ui.painter().with_clip_rect(list);
            for (i, track) in tracks.iter().enumerate() {
                let row = Rect::from_min_size(pos2(list.left(), list.top() + i as f32 * (rh + S2) - scroll), vec2(list.width(), rh));
                if !list.contains_rect(row) { continue; }
                widgets::well(&p, row, 12.0);
                p.text(row.left_center() + vec2(16.0, -8.0), Align2::LEFT_CENTER, &track.name, theme::title(20.0), theme::TEXT);
                p.text(row.left_center() + vec2(16.0, 12.0), Align2::LEFT_CENTER, format!("{} laps", track.laps), theme::ui_medium(11.5), theme::TEXT_DIM);
                let button = Rect::from_min_size(pos2(row.right() - 116.0, row.top() + 7.0), vec2(108.0, 40.0));
                if widgets::button(ui, button, &format!("lobby-choose-track-{i}"), &Btn::primary("Choose").size(16.0)).clicked() { picked = Some(i); }
            }
        });
        self.picker_scroll = scroll;
        let track = picked.and_then(|i| tracks.get(i)).cloned();
        if back { feedback::back(ctx); self.picker = None; }
        if let Some(track) = track {
            let result = ctx.online.with(|s| s.select_custom_track(&track));
            if let Some(Err(e)) = result { ctx.say(e); }
            else { feedback::select(ctx); self.picker = None; }
        }
        Transition::None
    }
}

/// Dev: `STAMPEDE_LAPS` / `STAMPEDE_BOTS` shorten a desktop test race.
fn dev_host_config() -> race_session::HostConfig {
    let mut c = race_session::HostConfig::default();
    if let Some(n) = crate::dev::var("STAMPEDE_LAPS").and_then(|v| v.parse().ok()) {
        c.laps = n;
    }
    if let Some(n) = crate::dev::var("STAMPEDE_BOTS").and_then(|v| v.parse().ok()) {
        c.bots = n;
    }
    c
}

/// A character's body colour as a chip.
pub fn character_chip(p: &Painter, c: egui::Pos2, r: f32, character: u8) {
    widgets::chip(p, c, r, from_f3(Character::get(character.min(7)).body));
}

/// One seat: chip, name, ostrich, and the seat's badges.
pub fn seat_cell(p: &Painter, rect: Rect, seat: Option<&SeatView>, me: Option<u8>, e: f32) {
    let mut p = p.clone();
    p.set_opacity(e.clamp(0.0, 1.0));
    widgets::well(&p, rect, 12.0);
    let Some(s) = seat else {
        p.text(rect.center(), Align2::CENTER_CENTER, "open seat", theme::ui_medium(11.5), rgba(0xB8, 0xA6, 0xC9, 0.5));
        return;
    };
    let mine = !s.bot && Some(s.id) == me;
    if mine {
        p.rect_stroke(rect, 12.0, egui::Stroke::new(2.0_f32, theme::GOLD), egui::StrokeKind::Inside);
    }
    let ch = Character::get(s.character.min(7));
    character_chip(&p, pos2(rect.left() + 22.0, rect.center().y), 12.5, s.character);
    let right = rect.right() - 10.0;
    let tx = rect.left() + 42.0;
    let name_w = (right - tx - 34.0).max(40.0);
    let size = art::fit_size(&p, &s.name, theme::ui_bold, 13.5, name_w);
    p.text(pos2(tx, rect.center().y - 8.0), Align2::LEFT_CENTER, &s.name, theme::ui_bold(size), theme::TEXT);
    p.text(pos2(tx, rect.center().y + 8.5), Align2::LEFT_CENTER, ch.name, theme::ui_medium(10.5), theme::TEXT_DIM);
    // badges, right to left
    let mut x = right;
    if s.bot {
        let r = widgets::pill(&p, pos2(x - 14.0, rect.center().y), "BOT", rgba(0x6A, 0x36, 0x96, 0.9), theme::TEXT, 9.5);
        x = r.left() - 4.0;
    } else if s.ready {
        icons::draw(&p, Icon::Check, pos2(x - 8.0, rect.center().y), 17.0, theme::MINT);
        x -= 22.0;
    } else {
        p.circle_stroke(pos2(x - 8.0, rect.center().y), 6.0, egui::Stroke::new(1.6_f32, rgba(0xB8, 0xA6, 0xC9, 0.55)));
        x -= 22.0;
    }
    if s.host {
        let _ = widgets::pill(&p, pos2(x - 17.0, rect.center().y - 17.0 + 8.0), "HOST", theme::GOLD, theme::OUTLINE, 8.5);
    }
}

fn status_lines(s: &OnlineSession) -> String {
    let st = s.status();
    if let Some(f) = &st.failure {
        return format!("Couldn't open the room: {f}");
    }
    if !st.ready {
        return "Opening the room...".into();
    }
    let lan = match st.lan {
        Some(a) if a.ip().is_unspecified() => "Nearby: on".to_string(),
        Some(a) => format!("Nearby: on ({})", a.ip()),
        None => "Nearby: starting...".into(),
    };
    let (up, total) = st.relays;
    let net = if total == 0 {
        "Online: off".to_string()
    } else if up == 0 {
        "Online: reaching relays... the code works once one is up".to_string()
    } else if !s.code_ready() {
        format!("Online: {up} of {total} relays, publishing the room...")
    } else {
        format!("Online: {up} of {total} relays")
    };
    format!("{lan}  \u{b7}  {net}")
}

enum Act {
    /// Race (false) or Battle (true).
    Battle(bool),
    Arena(i32),
    Back,
    Ready(bool),
    StartNow,
    Ostrich,
    Copy,
    Link,
    Track(i32),
    MyTracks,
    SaveTrack,
    Laps(u8),
    Bots(i32),
    Level(BotLevel),
    /// "Protect this room": the backup picker, or first (a room with an
    /// online code) "Protect this room?".
    Protect,
    /// "Protect this room?": Protect. The room reopens nearby-only (no
    /// online code; everyone rejoins from Nearby) and the picker opens.
    ProtectNearby,
    /// "Protect this room?": Not now.
    CloseProtectSheet,
    CancelArming,
}

/// What the lobby draws, copied out of the session for the frame.
struct Snap {
    is_host: bool,
    lobby: Option<race_session::LobbyView>,
    my_seat: Option<u8>,
    me_ready: bool,
    code: Option<String>,
    room_status: String,
    /// The room code works (a relay carries the room).
    code_ok: bool,
    rtt: Option<u32>,
    custom: Option<CustomTrack>,
    downloading: bool,
    track_error: Option<String>,
    migration: MigrationState,
    /// The host's name (the backup's consent names the room).
    host_name: String,
    /// The room has an online code (a backup can't protect it as it is).
    online_code: bool,
    /// The room is on this Wi-Fi only: no code to share.
    nearby_only: bool,
}

enum Next {
    Dead(String),
    Race,
    Results,
    Stay,
}

impl Screen for LobbyScreen {
    fn name(&self) -> &'static str {
        "lobby"
    }

    fn uses_online(&self) -> bool {
        true
    }

    fn on_background(&mut self, _ctx: &mut Ctx) {
        feedback::on_background();
    }

    fn update(&mut self, ctx: &mut Ctx) -> Transition {
        common::begin(ctx, &mut self.t);
        self.copied += ctx.dt;
        let sample = self.sample;
        if let Some(s) = self.staged.take() {
            ctx.online.put(s);
        }
        if let Some(arena) = self.sample_battle.take() {
            ctx.online.with(|s| s.set_battle(Some(BattleSetup::new(arena))));
        }
        if self.open_host && !ctx.online.is_some() && ctx.opts.shot.is_none() {
            self.open_host = false;
            match OnlineSession::host(ctx.data_dir, &ctx.profile.name, ctx.profile.character, dev_host_config()) {
                Ok(s) => ctx.online.put(s),
                Err(e) => ctx.say(e),
            }
        }
        // A join link while in a room: ask before leaving it.
        if self.leaving {
            return Transition::None;
        }
        match ctx.take_join_code() {
            // A link that isn't an invite is no reason to leave the room.
            Some(crate::keeps::JoinTarget::BadInvite) => ctx.say(crate::keeps::NOT_AN_INVITE),
            Some(code) => self.join_prompt = Some(common::LeaveAsk::new(ctx.data_dir, code)),
            None => {}
        }
        if !ctx.online.is_some() {
            if self.had_session {
                // The room is gone (left elsewhere): nothing to show.
                self.had_session = false;
                return Transition::PopTo("multiplayer");
            }
            return Transition::None;
        }
        self.had_session = true;
        let attire = crate::ui::progress::attire(ctx.profile, ctx.profile.character);
        let (want_char, sent) = (ctx.profile.character.min(7), self.sent_char);
        let autoready = crate::dev::var_set("STAMPEDE_AUTOREADY") && self.t > 1.0;
        let mut now_sent = sent;
        let next = ctx
            .online
            .with(|s| {
                s.pump(Input::default());
                s.client_command(move |client| client.set_attire(attire));
                if sample {
                    s.ready_guests();
                }
                if let Some(why) = s.dead_reason() {
                    return Next::Dead(why);
                }
                // Changing the ostrich in the Garage comes back here.
                if let Some(seat) = s.me().my_lobby_seat() {
                    if !sample && seat.character != want_char && sent != Some(want_char) {
                        s.client_command(move |client| client.set_character(want_char));
                        now_sent = Some(want_char);
                    } else if seat.character == want_char {
                        now_sent = None;
                    }
                }
                // Dev: STAMPEDE_AUTOREADY readies up once seated (two desktop instances).
                if autoready && s.me().lobby().is_some_and(|l| l.humans() >= 2) && s.me().my_lobby_seat().is_some_and(|m| !m.ready) {
                    s.client_command(move |client| client.set_ready(true));
                }
                // The race may already be on (it can start in the frame we are seated),
                // or over.
                match s.me().phase() {
                    ClientPhase::Racing if s.me().view().is_some() => Next::Race,
                    ClientPhase::Results => Next::Results,
                    _ => Next::Stay,
                }
            })
            .unwrap_or(Next::Stay);
        self.sent_char = now_sent;
        match next {
            Next::Dead(why) => self.leave(ctx, Some(&why)),
            Next::Race => match crate::game::source_net::race_screen(ctx.online) {
                Some(screen) => Transition::Replace(screen),
                None => Transition::None,
            },
            Next::Results => Transition::Replace(Box::new(super::online_results::OnlineResultsScreen::new())),
            Next::Stay => Transition::None,
        }
    }

    fn back(&mut self, ctx: &mut Ctx) -> Transition {
        feedback::back(ctx);
        if self.picker.take().is_some() { return Transition::None; }
        if self.protect.take().is_some() { return Transition::None; }
        if std::mem::take(&mut self.protect_sheet) { return Transition::None; }
        // Back on the backup's question is "No thanks".
        if ctx.online.with(|s| s.migration_state() == MigrationState::ConsentRequested).unwrap_or(false) {
            ctx.online.with(|s| s.answer_consent(false));
            return Transition::None;
        }
        if self.join_prompt.take().is_some() {
            return Transition::None;
        }
        let host = ctx.online.with(|s| s.is_host()).unwrap_or(false);
        self.leave(ctx, if host { Some("Room closed") } else { None })
    }

    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition {
        if self.picker.is_some() { return self.draw_picker(ctx, ui); }
        if self.protect.is_some() { return self.draw_protect(ctx, ui); }
        let safe = ctx.viewport.safe;
        let (t, tm) = (self.t, ctx.time as f32);
        let mut acts: Vec<Act> = Vec::new();
        let copied = (self.copied < 1.6).then_some(self.copied_link);
        let my_char = ctx.profile.character.min(7);
        let Some(snap) = ctx.online.with(|s| Snap {
            is_host: s.is_host() && !self.guest_view,
            lobby: s.me().lobby().cloned(),
            my_seat: s.me().my_seat(),
            me_ready: s.me().my_lobby_seat().is_some_and(|m| m.ready),
            code: s.code().map(str::to_string),
            room_status: status_lines(s),
            code_ok: s.code_ready(),
            rtt: s.rtt_ms(),
            custom: s.me().custom_track().cloned(),
            downloading: self.sample_downloading || s.me().downloading_track(),
            track_error: s.me().track_error().map(str::to_string),
            migration: s.migration_state(),
            host_name: s.me().lobby().and_then(|l| l.seats.iter().find(|x| x.host)).map_or_else(|| "The host".to_string(), |h| h.name.clone()),
            online_code: s.has_online_code(),
            nearby_only: s.nearby_only(),
        }) else {
            // No session yet (a room being opened on the first frame): just the page.
            common::page(ctx.viewport.screen, tm, ui, 0.26, |ui, _| {
                widgets::header(ui, safe, "Your room", widgets::enter(t, 0));
            });
            return Transition::None;
        };
        let Snap { is_host, lobby, my_seat, me_ready, code, room_status, code_ok: room_ok, rtt, custom, downloading, track_error, migration: mig, host_name, online_code, nearby_only } = snap;
        let (mig, can_protect) = self.migration_at(&mig, is_host, lobby.is_some(), decented_signet::sharing::now_ms());
        if !can_protect {
            self.protect_sheet = false;
        }
        let saved = custom.as_ref().is_some_and(|t| {
            let hash = t.hash();
            if self.saved_custom.is_none_or(|(h, _)| h != hash) {
                self.saved_custom = Some((hash, crate::custom_tracks::load(ctx.data_dir, hash).as_ref() == Some(t)));
            }
            self.saved_custom.is_some_and(|(_, saved)| saved)
        });
        let humans = lobby.as_ref().map_or(0, |l| l.humans());
        // A battle needs two ostriches (bots count); the host refuses otherwise.
        let host_can_start = ctx.online.with(|s| s.can_start()).unwrap_or(true);
        let all_ready = lobby.as_ref().is_some_and(|l| l.seats.iter().filter(|x| !x.bot).all(|x| x.ready));
        let connecting = lobby.is_none();
        let guest_view = self.guest_view;
        let title = if is_host { "Your room".to_string() } else { lobby.as_ref().and_then(|l| l.seats.iter().find(|x| x.host)).map(|h| format!("{}'s room", h.name)).unwrap_or_else(|| "Lobby".into()) };
        common::page(ctx.viewport.screen, tm, ui, 0.26, |ui, _brand| {
            let hd = widgets::header(ui, safe, &title, widgets::enter(t, 0));
            if hd.back.clicked() {
                acts.push(Act::Back);
            }
            // Protection: the host's "Protect this room", or how the room stands.
            let title_right = safe.left() + widgets::S4 + widgets::TAP_MIN + S3 + ui.painter().layout_no_wrap(title.clone(), theme::title(30.0), Color32::WHITE).size().x;
            match migration::cluster(ui, migration::band(safe, title_right + S3), &mig, is_host, can_protect) {
                Some(migration::Cluster::Protect) => acts.push(Act::Protect),
                Some(migration::Cluster::Cancel) => acts.push(Act::CancelArming),
                None => {}
            }
            let body = hd.body;
            let gap = S3;
            let rw = (body.width() * 0.43).clamp(250.0, 340.0);
            let (left, right) = common::Layout::new(safe).split(body, rw, 360.0, gap);

            // ── seats ──
            let bar_h = 46.0;
            let grid = Rect::from_min_max(left.min, pos2(left.right(), left.bottom() - bar_h - S2));
            let (cw, ch) = ((grid.width() - S2) / 2.0, (grid.height() - S2 * 3.0) / 4.0);
            let seats: Vec<&SeatView> = lobby.as_ref().map(|l| l.seats.iter().collect()).unwrap_or_default();
            for i in 0..8usize {
                let (col, row) = (i / 4, i % 4);
                let r = Rect::from_min_size(pos2(grid.left() + col as f32 * (cw + S2), grid.top() + row as f32 * (ch + S2)), vec2(cw, ch));
                let e = widgets::enter(t, 1 + i / 2);
                widgets::staged(ui, e, vec2(0.0, 24.0), |ui, off| seat_cell(ui.painter(), r.translate(off), seats.get(i).copied(), my_seat, e));
            }

            // ── the action bar ──
            let by = left.bottom() - bar_h;
            let ch_def = Character::get(my_char);
            let ow = (left.width() * 0.36).min(160.0);
            let orect = Rect::from_min_size(pos2(left.left(), by), vec2(ow, bar_h));
            widgets::staged(ui, widgets::enter(t, 5), vec2(0.0, 20.0), |ui, off| {
                let r = orect.translate(off);
                if widgets::button(ui, r, "lobby-ostrich", &Btn::new(ch_def.name).size(16.0)).clicked() {
                    acts.push(Act::Ostrich);
                }
                widgets::chip(ui.painter(), pos2(r.left() + 18.0, r.center().y), 8.0, from_f3(ch_def.body));
                ui.painter().text(pos2(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, "change", theme::ui_bold(10.0), theme::TEXT_DIM);
            });
            let start_w = if is_host { 104.0 } else { 0.0 };
            let rrect = Rect::from_min_max(pos2(orect.right() + S2, by), pos2(left.right() - if is_host { start_w + S2 } else { 0.0 }, by + bar_h));
            widgets::staged(ui, widgets::enter(t, 6), vec2(0.0, 20.0), |ui, off| {
                let r = rrect.translate(off);
                let label = if me_ready { "Ready!" } else { "Ready" };
                let b = if me_ready { Btn::new(label).icon(Icon::Check).selected(true) } else { Btn::primary(label) };
                if widgets::button(ui, r, "lobby-ready", &b.enabled(!connecting && !downloading).size(22.0)).clicked() {
                    acts.push(Act::Ready(!me_ready));
                }
            });
            if is_host {
                let r = Rect::from_min_max(pos2(left.right() - start_w, by), pos2(left.right(), by + bar_h));
                widgets::staged(ui, widgets::enter(t, 6), vec2(0.0, 20.0), |ui, off| {
                    let enabled = host_can_start && (humans > 0 && !all_ready || (humans > 1 && all_ready));
                    let label = if all_ready { "Go!" } else { "Start" };
                    if widgets::button(ui, r.translate(off), "lobby-start", &Btn::new(label).sub("now").enabled(enabled)).clicked() {
                        acts.push(Act::StartNow);
                    }
                });
            }

            // ── the side panel ──
            widgets::staged(ui, widgets::enter(t, 2), vec2(50.0, 0.0), |ui, off| {
                let col = right.translate(off);
                let p = ui.painter().clone();
                widgets::panel(&p, col, 20.0);
                let inner = col.shrink2(vec2(14.0, 12.0));
                if is_host && !guest_view {
                    host_panel(ui, &p, inner, &code, &room_status, room_ok, nearby_only, copied, lobby.as_ref(), &mut acts);
                } else {
                    guest_panel(ui, &p, inner, lobby.as_ref(), rtt, connecting, downloading, track_error.as_deref(), saved, &mut acts);
                }
            });
        });
        // Apply.
        // A join link arrived: the question takes the screen.
        if let Some(mut ask) = self.join_prompt.take() {
            acts.clear();
            ask.tick(ctx.dt);
            let answer = common::leave_prompt(ctx, ui, &ask);
            match answer {
                Some(true) => {
                    feedback::select(ctx);
                    ctx.online.leave();
                    self.leaving = true;
                    // Multiplayer (or Home / Menu) takes the code and opens the join.
                    *ctx.join_code = Some(ask.target);
                    return Transition::PopTo("multiplayer");
                }
                Some(false) => feedback::back(ctx),
                None => self.join_prompt = Some(ask),
            }
        }
        // "Protect this room?": a room with an online code moves to this Wi-Fi first.
        if self.protect_sheet {
            acts.clear();
            let mut lines = vec![migration::NEARBY_ONLY.to_string()];
            if humans > 1 {
                lines.push(migration::REJOIN.to_string());
            }
            match migration::nearby_sheet(ui, ctx.viewport.screen, &lines) {
                Some(true) => acts.push(Act::ProtectNearby),
                Some(false) => acts.push(Act::CloseProtectSheet),
                None => {}
            }
        }
        // The room stopped (paused, reconnecting): a card over everything.
        migration::card(ui, ctx.viewport.screen, &migration::view(&mig, is_host));
        // Asked to be the backup: the question takes the screen.
        if mig == MigrationState::ConsentRequested {
            acts.clear();
            if let Some(yes) = migration::consent_prompt(ui, ctx.viewport.screen, &host_name) {
                if yes { feedback::select(ctx) } else { feedback::back(ctx) }
                ctx.online.with(|s| s.answer_consent(yes));
            }
        }
        let mut out = Transition::None;
        for a in acts {
            match a {
                Act::Back => {
                    feedback::back(ctx);
                    return self.leave(ctx, if is_host { Some("Room closed") } else { None });
                }
                Act::Ready(r) => {
                    feedback::tap(ctx);
                    ctx.online.with(|s| s.client_command(move |client| client.set_ready(r)));
                }
                Act::StartNow => {
                    feedback::select(ctx);
                    ctx.online.with(|s| {
                        s.start_race();
                    });
                }
                Act::Ostrich => {
                    feedback::tap(ctx);
                    out = Transition::Push(Box::new(GarageScreen::new()));
                }
                Act::Copy => {
                    feedback::tap(ctx);
                    if let Some(c) = code.as_ref().filter(|_| room_ok) {
                        match ctx.platform.copy_text(c) {
                            Ok(()) => ctx.say("Room code copied"),
                            Err(e) => ctx.say(format!("Couldn't copy: {e}")),
                        }
                        self.copied = 0.0;
                        self.copied_link = false;
                    }
                }
                Act::Link => {
                    feedback::tap(ctx);
                    if let Some(c) = code.as_ref().filter(|_| room_ok) {
                        match ctx.platform.copy_text(&online::join_link(c)) {
                            Ok(()) => ctx.say("Join link copied. Paste it in a message."),
                            Err(e) => ctx.say(format!("Couldn't copy: {e}")),
                        }
                        self.copied = 0.0;
                        self.copied_link = true;
                    }
                }
                Act::Track(d) => {
                    feedback::tap(ctx);
                    if let Some(l) = lobby.as_ref() {
                        let all = TrackId::RACE_ALL;
                        let i = all.iter().position(|&x| x == l.track).unwrap_or(0) as i32;
                        let next = all[(i + d).rem_euclid(all.len() as i32) as usize];
                        ctx.online.with(|s| s.set_track(next));
                    }
                }
                Act::MyTracks => {
                    feedback::tap(ctx);
                    self.picker = Some(crate::custom_tracks::list(ctx.data_dir));
                    self.picker_scroll = 0.0;
                }
                Act::SaveTrack => {
                    if let Some(t) = &custom {
                        match crate::custom_tracks::save(ctx.data_dir, t) {
                            Ok(hash) => { self.saved_custom = Some((hash, true)); feedback::select(ctx); ctx.say("Saved in My Tracks"); }
                            Err(e) => ctx.say(e),
                        }
                    }
                }
                Act::Battle(on) => {
                    feedback::tap(ctx);
                    let arena = lobby.as_ref().and_then(|l| l.battle.map(|b| b.arena)).unwrap_or(TrackId::ARENAS[0]);
                    ctx.online.with(|s| s.set_battle(on.then(|| BattleSetup::new(arena))));
                }
                Act::Arena(d) => {
                    feedback::tap(ctx);
                    if let Some(b) = lobby.as_ref().and_then(|l| l.battle) {
                        let arenas = TrackId::ARENAS;
                        let i = arenas.iter().position(|&x| x == b.arena).unwrap_or(0) as i32;
                        let next = arenas[(i + d).rem_euclid(arenas.len() as i32) as usize];
                        ctx.online.with(|s| s.set_battle(Some(BattleSetup { arena: next, rules: b.rules })));
                    }
                }
                Act::Laps(n) => {
                    feedback::tap(ctx);
                    ctx.online.with(|s| s.set_laps(n));
                }
                Act::Bots(d) => {
                    feedback::tap(ctx);
                    if let Some(l) = lobby.as_ref() {
                        ctx.online.with(|s| s.set_bots((l.bots as i32 + d).clamp(0, 7) as u8));
                    }
                }
                Act::Level(lv) => {
                    feedback::tap(ctx);
                    ctx.online.with(|s| s.set_bot_level(lv));
                }
                Act::Protect => {
                    feedback::tap(ctx);
                    if online_code {
                        self.protect_sheet = true;
                    } else {
                        self.protect = Some(0.0);
                    }
                }
                Act::ProtectNearby => {
                    feedback::select(ctx);
                    self.protect_sheet = false;
                    // A live room can't leave its relays: a new room on this
                    // Wi-Fi takes its place (the old one closes as it goes).
                    match ctx.online.with(|s| s.reopen_nearby()) {
                        Some(Ok(s)) => {
                            ctx.online.put(s);
                            self.sent_char = None;
                            self.saved_custom = None;
                            self.copied = 99.0;
                            self.protect = Some(0.0);
                        }
                        Some(Err(e)) => ctx.say(e),
                        None => {}
                    }
                }
                Act::CloseProtectSheet => {
                    feedback::back(ctx);
                    self.protect_sheet = false;
                }
                Act::CancelArming => {
                    feedback::back(ctx);
                    ctx.online.with(|s| s.cancel_arming());
                }
            }
        }
        out
    }
}

fn level_index(l: BotLevel) -> usize {
    common::LEVELS.iter().position(|(_, x)| *x == l).unwrap_or(1)
}

#[allow(clippy::too_many_arguments)]
fn host_panel(ui: &mut egui::Ui, p: &Painter, inner: Rect, code: &Option<String>, status: &str, room_ok: bool, nearby_only: bool, copied: Option<bool>, lobby: Option<&race_session::LobbyView>, acts: &mut Vec<Act>) {
    if nearby_only {
        // No code works here: say how friends join instead of the code, Copy and Share.
        let area = Rect::from_min_max(pos2(inner.left(), inner.top() + 2.0), pos2(inner.right(), inner.top() + 62.0));
        let g = migration::wrapped(p, migration::NEARBY_JOIN, theme::title, 18.0, 12.0, theme::TEXT, area.width(), 2);
        p.galley(pos2(area.left(), area.center().y - g.size().y / 2.0), g, theme::TEXT);
    } else {
        room_code(ui, p, inner, code, room_ok, copied, acts);
    }
    let g = p.layout(status.to_string(), theme::ui_medium(10.2), if status.starts_with("Couldn't") { Color32::from_rgb(0xFF, 0x8A, 0x7A) } else { theme::TEXT_DIM }, inner.width());
    p.galley(pos2(inner.left(), inner.top() + 66.0), g, theme::TEXT_DIM);
    settings_rows(ui, p, inner, lobby, acts);
}

/// The room code with Copy code and Share link (a room with an online code).
fn room_code(ui: &mut egui::Ui, p: &Painter, inner: Rect, code: &Option<String>, room_ok: bool, copied: Option<bool>, acts: &mut Vec<Act>) {
    p.text(pos2(inner.left(), inner.top() + 6.0), Align2::LEFT_CENTER, "ROOM CODE", theme::ui_bold(10.5), theme::SAND);
    let bw = 92.0;
    let code_area = Rect::from_min_max(pos2(inner.left(), inner.top() + 14.0), pos2(inner.right() - bw - S2, inner.top() + 62.0));
    match code {
        Some(c) if room_ok => {
            let txt = online::spaced_code(c);
            let size = art::fit_size(p, &txt, theme::title, 40.0, code_area.width());
            art::outlined(p, code_area.left_center() + vec2(0.0, 1.0), Align2::LEFT_CENTER, &txt, theme::title(size), theme::TEXT, theme::OUTLINE, (size * 0.075).max(1.5), 0.0);
        }
        _ => {
            p.text(code_area.left_center(), Align2::LEFT_CENTER, "Getting ready...", theme::title(22.0), theme::TEXT_DIM);
        }
    }
    let b1 = Rect::from_min_size(pos2(inner.right() - bw, inner.top() + 14.0), vec2(bw, 22.0));
    let b2 = Rect::from_min_size(pos2(inner.right() - bw, inner.top() + 40.0), vec2(bw, 22.0));
    let ok = code.is_some() && room_ok;
    let l1 = if copied == Some(false) { "Copied" } else { "Copy code" };
    let l2 = if copied == Some(true) { "Copied" } else { "Share link" };
    if widgets::button(ui, b1, "lobby-copy", &Btn::new(l1).enabled(ok).size(12.0)).clicked() {
        acts.push(Act::Copy);
    }
    if widgets::button(ui, b2, "lobby-link", &Btn::new(l2).enabled(ok).size(12.0)).clicked() {
        acts.push(Act::Link);
    }
}

/// The race settings under the room's status.
fn settings_rows(ui: &mut egui::Ui, p: &Painter, inner: Rect, lobby: Option<&race_session::LobbyView>, acts: &mut Vec<Act>) {
    let Some(l) = lobby else { return };
    let top = inner.top() + 90.0;
    let rh = 29.0;
    let lw = 46.0;
    let battle = l.battle;
    let mut nrow = 0usize;
    let mut row = || {
        let r = Rect::from_min_size(pos2(inner.left(), top + nrow as f32 * (rh + 5.0)), vec2(inner.width(), rh));
        nrow += 1;
        r
    };
    let label = |r: Rect, text: &str| p.text(pos2(r.left(), r.center().y), Align2::LEFT_CENTER, text, theme::ui_bold(11.5), theme::SAND);
    // mode: Race or Battle
    let r = row();
    label(r, "Mode");
    let segs = [Seg::new("Race"), Seg::new("Battle")];
    if let Some(i) = widgets::segmented(ui, Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max), "lobby-mode", &segs, battle.is_some() as usize) {
        acts.push(Act::Battle(i == 1));
    }
    // track or arena
    let r = row();
    label(r, if battle.is_some() { "Arena" } else { "Track" });
    let tr = Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max);
    let aw = 34.0;
    let (prev, next) = if battle.is_some() { ("lobby-arena-prev", "lobby-arena-next") } else { ("lobby-track-prev", "lobby-track-next") };
    let step = |d: i32| if battle.is_some() { Act::Arena(d) } else { Act::Track(d) };
    if widgets::button(ui, Rect::from_min_size(tr.left_top(), vec2(aw, rh)), prev, &Btn::new("").icon(Icon::ChevronLeft)).clicked() {
        acts.push(step(-1));
    }
    if widgets::button(ui, Rect::from_min_size(pos2(tr.right() - aw, tr.top()), vec2(aw, rh)), next, &Btn::new("").icon(Icon::ChevronRight)).clicked() {
        acts.push(step(1));
    }
    let info = tracks::info(battle.map_or(l.track, |b| b.arena));
    let mid = Rect::from_min_max(pos2(tr.left() + aw + 4.0, tr.top()), pos2(tr.right() - aw - 4.0, tr.bottom()));
    let size = art::fit_size(p, &info.name, theme::title, 16.0, mid.width());
    if battle.is_none() {
        if widgets::button(ui, mid, "lobby-my-tracks", &Btn::new(&info.name).sub("My Tracks").size(size)).clicked() { acts.push(Act::MyTracks); }
    } else {
        p.text(mid.center() - vec2(0.0, 4.0), Align2::CENTER_CENTER, &info.name, theme::title(size), theme::TEXT);
        p.text(mid.center() + vec2(0.0, 9.0), Align2::CENTER_CENTER, "Feather Frenzy", theme::ui_bold(9.5), theme::TEXT_DIM);
    }
    // laps (races only)
    if battle.is_none() {
        let r = row();
        label(r, "Laps");
        let ctl = Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max);
        if l.custom.is_some() {
            if widgets::button(ui, Rect::from_min_size(ctl.min, vec2(40.0, rh)), "lobby-laps-minus", &Btn::new("-").enabled(l.laps > 1)).clicked() { acts.push(Act::Laps(l.laps.saturating_sub(1))); }
            if widgets::button(ui, Rect::from_min_size(pos2(ctl.right() - 40.0, ctl.top()), vec2(40.0, rh)), "lobby-laps-plus", &Btn::new("+").enabled(l.laps < 9)).clicked() { acts.push(Act::Laps(l.laps.saturating_add(1))); }
            p.text(ctl.center(), Align2::CENTER_CENTER, l.laps.to_string(), theme::numbers(20.0), theme::TEXT);
        } else {
            let segs: Vec<Seg> = ["1", "2", "3", "4", "5"].iter().map(|x| Seg::new(x)).collect();
            if let Some(i) = widgets::segmented(ui, ctl, "lobby-laps", &segs, (l.laps as usize).clamp(1, 5) - 1) {
                acts.push(Act::Laps(i as u8 + 1));
            }
        }
    }
    // bots
    let r = row();
    label(r, "Bots");
    let ctl = Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max);
    let bw = 40.0;
    if widgets::button(ui, Rect::from_min_size(ctl.left_top(), vec2(bw, rh)), "lobby-bots-minus", &Btn::new("-").enabled(l.bots > 0).size(20.0)).clicked() {
        acts.push(Act::Bots(-1));
    }
    if widgets::button(ui, Rect::from_min_size(pos2(ctl.right() - bw, ctl.top()), vec2(bw, rh)), "lobby-bots-plus", &Btn::new("+").enabled(l.bots < 7).size(20.0)).clicked() {
        acts.push(Act::Bots(1));
    }
    art::outlined(p, ctl.center(), Align2::CENTER_CENTER, &format!("{}", l.bots), theme::numbers(20.0), theme::TEXT, theme::OUTLINE, 2.0, 0.0);
    // level
    let r = row();
    label(r, "Level");
    let segs: Vec<Seg> = common::LEVELS.iter().map(|(n, _)| Seg::new(n)).collect();
    if let Some(i) = widgets::segmented(ui, Rect::from_min_max(pos2(r.left() + lw, r.top()), r.max), "lobby-level", &segs, level_index(l.bot_level)) {
        acts.push(Act::Level(common::LEVELS[i].1));
    }
}

#[allow(clippy::too_many_arguments)]
fn guest_panel(ui: &mut egui::Ui, p: &Painter, inner: Rect, lobby: Option<&race_session::LobbyView>, rtt: Option<u32>, connecting: bool, downloading: bool, track_error: Option<&str>, saved: bool, acts: &mut Vec<Act>) {
    let _ = ui;
    let Some(l) = lobby else {
        p.text(inner.center(), Align2::CENTER_CENTER, "Joining...", theme::title(22.0), theme::TEXT_DIM);
        let _ = connecting;
        return;
    };
    let card = Rect::from_min_size(inner.min, vec2(inner.width(), 124.0));
    if downloading {
        widgets::well(p, card, 14.0);
        let text = track_error.map(|e| format!("Couldn't download the track: {e}")).unwrap_or_else(|| "Downloading track…".into());
        let g = p.layout(text, theme::ui_medium(16.0), theme::TEXT_DIM, card.width() - 24.0);
        let pos = card.center() - g.size() * 0.5;
        p.galley(pos, g, theme::TEXT_DIM);
    } else {
        let info = tracks::info(l.battle.map_or(l.track, |b| b.arena));
        tracks::card(ui, card, "lobby-track", &info, false, None, None, false, 1.0);
    }
    let mut y = card.bottom() + 14.0;
    let line = |y: f32, a: &str, b: &str| {
        p.text(pos2(inner.left() + 2.0, y), Align2::LEFT_CENTER, a, theme::ui_bold(12.0), theme::SAND);
        p.text(pos2(inner.right() - 2.0, y), Align2::RIGHT_CENTER, b, theme::ui_bold(12.5), theme::TEXT);
    };
    match l.battle {
        Some(b) => {
            line(y, "Mode", "Battle (Feather Frenzy)");
            y += 22.0;
            line(y, "Rules", &format!("{} feathers, {} min", b.rules.feathers, b.rules.time_limit / 60))
        }
        None => line(y, "Laps", &format!("{}", l.laps)),
    }
    y += 22.0;
    let level = common::LEVELS[level_index(l.bot_level)].0;
    line(y, "Bots", &format!("{} ({level})", l.bots));
    y += 22.0;
    let q = online::quality(rtt);
    let col = [theme::MINT, theme::GOLD, Color32::from_rgb(0xFF, 0x5A, 0x4A)][q as usize];
    p.circle_filled(pos2(inner.left() + 7.0, y), 4.5, col);
    p.text(pos2(inner.left() + 18.0, y), Align2::LEFT_CENTER, "Connection", theme::ui_bold(12.0), theme::SAND);
    p.text(pos2(inner.right() - 2.0, y), Align2::RIGHT_CENTER, rtt.map(|r| format!("{r} ms")).unwrap_or_else(|| "--".into()), theme::ui_bold(12.5), theme::TEXT);
    y += 26.0;
    p.text(pos2(inner.center().x, y), Align2::CENTER_CENTER, if l.battle.is_some() { "The battle starts when everyone is ready." } else { "The race starts when everyone is ready." }, theme::ui_medium(10.5), theme::TEXT_DIM);
    if l.custom.is_some() {
        let save = Rect::from_min_size(pos2(inner.left(), inner.bottom() - 34.0), vec2(inner.width(), 34.0));
        if widgets::button(ui, save, "lobby-save-track", &Btn::new(if saved { "Saved in My Tracks" } else { "Save to My Tracks" }).enabled(!downloading && !saved).size(14.0)).clicked() { acts.push(Act::SaveTrack); }
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::online::BEHIND;
    use migration::{view, View};

    /// The lobby draws host migration through its held-back wait: a wait
    /// for the backup 400 ms long shows no card, one that lasts 1.2 s shows
    /// it. Protect stays a question of the real state.
    #[test]
    fn the_lobby_shows_the_wait_for_the_backup_only_once_it_lasts() {
        let card = |s: &MigrationState| matches!(view(s, true), View::Card { .. });
        let armed = MigrationState::Armed { unprotected: vec![] };
        let behind = MigrationState::Paused { reason: BEHIND.into() };
        let mut l = LobbyScreen::new();
        assert_eq!(l.migration_at(&armed, true, true, 9_000), (armed.clone(), false));
        for now in [10_000, 10_400] {
            let (shown, can_protect) = l.migration_at(&behind, true, true, now);
            assert!(!card(&shown), "{now}: {shown:?}");
            assert!(!can_protect);
        }
        let (shown, _) = l.migration_at(&behind, true, true, 11_200);
        assert_eq!(shown, behind);
        assert!(card(&shown), "1.2 s: the card");
    }
}
