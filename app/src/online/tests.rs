//! Online play through the real engine, router and screens, on the loopback
//! transport and a manual clock: a host phone and guest phones, each an
//! [`Engine`] with its own screen stack, linked by a [`LoopbackHub`].
//!
//! These cover what the review of the online glue found missing: leaving from
//! every screen, rematches, joining during a race, a guest in the Garage while
//! the race starts, a screen change the router has to drop mid-fade, the
//! join-by-code retry and a backgrounded player.

use super::*;
use crate::cli::Options;
use crate::engine::Engine;
use crate::haptics::NoHaptics;
use crate::platform::data_dir::Profile;
use crate::platform::os::RecordingPlatform;
use crate::platform::TouchPhase;
use crate::ui::screens::garage::GarageScreen;
use crate::ui::screens::join::JoinScreen;
use crate::ui::screens::lobby::LobbyScreen;
use crate::ui::screens::{Ctx, Screen, Transition};
use race_core::{BotLevel, TrackId};
use race_session::{HostConfig, HostPhase};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

const SIZE: (u32, u32) = (1280, 720);
const PPP: f32 = 720.0 / 412.0;
const STEP_MS: u64 = 16;

/// The screen the multiplayer menu would be: what leaving a room returns to.
struct Root;
impl Screen for Root {
    fn name(&self) -> &'static str {
        "multiplayer"
    }
    fn update(&mut self, _ctx: &mut Ctx) -> Transition {
        Transition::None
    }
}

/// Removes a phone's data directory once its engine is gone (fields drop in order).
struct DirGuard(std::path::PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Phone {
    engine: Engine,
    _dir: DirGuard,
}

impl Phone {
    fn new(name: &str) -> Phone {
        // One directory per phone, never shared: tests run in parallel and several
        // wipe and rewrite `keeps.json` in their phone's directory.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("stampede-online-test-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::new(Box::new(Root), Profile::default(), dir.clone(), Options::default(), Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        Phone { engine, _dir: DirGuard(dir) }
    }

    fn slot(&self) -> OnlineSlot {
        self.engine.svc.online.clone()
    }

    fn top(&self) -> &'static str {
        self.engine.screen_name()
    }

    fn stack(&self) -> Vec<&'static str> {
        self.engine.router.names()
    }

    fn frame(&mut self) {
        self.engine.frame(1.0 / 60.0, SIZE, PPP, egui::RawInput::default());
    }

    /// Put `s` in the app's slot and open the lobby, as Host / Join do.
    fn open_lobby(&mut self, s: OnlineSession) {
        self.engine.svc.online.put(s);
        self.engine.router.apply(Transition::Push(Box::new(LobbyScreen::new())));
    }

    /// A tap on the screen at UI point `(x, y)`: press, a frame, release, a frame.
    fn tap(&mut self, x: f32, y: f32) {
        let px = ((x * PPP) as f64, (y * PPP) as f64);
        self.engine.touch_event(7, TouchPhase::Started, px);
        self.frame();
        self.engine.touch_event(7, TouchPhase::Ended, px);
        self.frame();
    }
}

struct Rig {
    time: Arc<AtomicU64>,
    clock: LoopClock,
    hub: LoopbackHub,
    host: Phone,
    guests: Vec<Phone>,
}

impl Rig {
    /// A host phone with its room open on the lobby screen (`bots` bots, `laps`
    /// laps on Savanna, the race starting by itself when everyone is ready).
    fn new(laps: u8, bots: u8) -> Rig {
        let (clock, time) = LoopClock::manual();
        time.store(10_000, Ordering::Relaxed);
        let hub = LoopbackHub::new(clock.clone(), 5);
        let cfg = HostConfig { track: TrackId::Savanna, laps, bots, bot_level: BotLevel::Normal, seed: 11, auto_start: true };
        let authority = HostAuthority::new(Box::new(hub.host()), cfg);
        let me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Host", 0);
        let mut host = Phone::new("host");
        host.open_lobby(OnlineSession::from_parts(me, Some(authority), clock.clone()));
        Rig { time, clock, hub, host, guests: Vec::new() }
    }

    /// A guest phone that joins the room, on the lobby screen.
    fn add_guest(&mut self, name: &str, character: u8) -> usize {
        let mut g = Phone::new(name);
        let me = ClientSession::new(Box::new(self.hub.connect(LoopbackConfig::PERFECT)), name, character);
        g.open_lobby(OnlineSession::from_parts(me, None, self.clock.clone()));
        self.guests.push(g);
        self.guests.len() - 1
    }

    fn step(&mut self) {
        self.time.fetch_add(STEP_MS, Ordering::Relaxed);
        self.host.frame();
        for g in &mut self.guests {
            g.frame();
        }
    }

    fn steps(&mut self, n: usize) {
        for _ in 0..n {
            self.step();
        }
    }

    /// Run until `cond` holds, at most `secs` of game time.
    fn until(&mut self, secs: u64, what: &str, mut cond: impl FnMut(&Rig) -> bool) {
        for _ in 0..(secs * 1000 / STEP_MS) {
            if cond(self) {
                return;
            }
            self.step();
        }
        panic!("timed out ({secs} s) waiting for: {what}\nhost screens {:?}, guests {:?}", self.host.stack(), self.guests.iter().map(|g| g.stack()).collect::<Vec<_>>());
    }

    fn phones(&self) -> impl Iterator<Item = &Phone> {
        std::iter::once(&self.host).chain(self.guests.iter())
    }

    fn host_phase(&self) -> HostPhase {
        self.host.slot().with(|s| s.host.as_ref().map(|h| h.phase())).flatten().expect("host session")
    }

    fn humans(&self) -> usize {
        self.host.slot().with(|s| s.host.as_ref().map(|h| h.human_count())).flatten().unwrap_or(0)
    }

    /// Everyone seated in a lobby.
    fn seat_everyone(&mut self) {
        self.until(15, "everyone seated, on the lobby screen", |r| {
            r.phones().all(|p| p.top() == "lobby" && p.slot().with(|s| s.me.lobby().is_some_and(|l| l.humans() > r.guests.len())).unwrap_or(false))
        });
    }

    /// Everyone ready: with `auto_start` the race begins.
    fn ready_everyone(&mut self) {
        for p in self.phones() {
            p.slot().with(|s| s.me.set_ready(true));
        }
    }

    /// Start the race and run until every phone shows the race screen.
    fn to_race(&mut self) {
        self.seat_everyone();
        self.ready_everyone();
        self.until(20, "everyone on the race screen", |r| r.phones().all(|p| p.top() == "race"));
    }

    /// Let the bot AI steer everyone, and run until every phone shows the results.
    fn to_results(&mut self) {
        for p in self.phones() {
            p.slot().with(|s| s.autopilot = true);
        }
        self.until(400, "everyone on the results screen", |r| r.phones().all(|p| p.top() == "online_results"));
    }
}

fn phase_of(p: &Phone) -> Option<ClientPhase> {
    p.slot().with(|s| s.me.phase().clone())
}

/// Where the pause menu's QUIT button sits (UI points) for an online race.
fn quit_button(p: &Phone) -> (f32, f32) {
    let _ = p;
    let v = crate::platform::Viewport::new(egui::vec2(SIZE.0 as f32 / PPP, SIZE.1 as f32 / PPP), PPP, crate::platform::Insets::ZERO);
    // Panel 248x273 centred in the safe rect; QUIT is the 4th of 4 online buttons.
    let c = v.safe.center();
    (c.x, c.y - 136.5 + 62.0 + 3.0 * 49.0 + 20.5)
}

/// Pause, then QUIT (a host confirms once more).
fn quit_race(p: &mut Phone, host: bool) {
    p.engine.request_back();
    for _ in 0..30 {
        p.frame();
    }
    let (x, y) = quit_button(p);
    p.tap(x, y);
    if host {
        for _ in 0..10 {
            p.frame();
        }
        p.tap(x, y);
    }
}

// ─── leaving ────────────────────────────────────────────────────────────────

#[test]
fn a_guest_leaves_from_the_lobby_and_the_host_keeps_the_room() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    assert_eq!(r.humans(), 2);
    r.guests[0].engine.request_back();
    r.until(5, "guest back on the menu", |r| r.guests[0].top() == "multiplayer");
    assert!(!r.guests[0].slot().is_some(), "the guest's slot is empty");
    r.steps(30);
    assert_eq!(r.humans(), 1, "the host sees the guest go");
    assert_eq!(r.host.top(), "lobby", "the host stays in the room");
}

#[test]
fn the_host_leaves_from_the_lobby_and_the_guest_is_told() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    r.host.engine.request_back();
    r.until(5, "both back on the menu", |r| r.phones().all(|p| p.top() == "multiplayer"));
    assert!(r.phones().all(|p| !p.slot().is_some()));
    assert!(r.guests[0].engine.svc.toast.as_ref().is_some_and(|t| t.text.contains("host left")), "the guest is told why: {:?}", r.guests[0].engine.svc.toast);
}

#[test]
fn a_guest_quits_a_race_and_the_race_goes_on_for_the_host() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    quit_race(&mut r.guests[0], false);
    r.until(5, "guest back on the menu", |r| r.guests[0].top() == "multiplayer");
    assert!(!r.guests[0].slot().is_some());
    r.steps(60);
    assert_eq!(r.host.top(), "race");
    assert_eq!(r.host_phase(), HostPhase::Racing, "the race goes on");
    assert_eq!(r.humans(), 1, "the guest's seat went to a bot");
}

#[test]
fn the_host_quits_a_race_after_confirming_and_the_guest_is_told() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    quit_race(&mut r.host, true);
    r.until(8, "both back on the menu", |r| r.phones().all(|p| p.top() == "multiplayer"));
    assert!(r.phones().all(|p| !p.slot().is_some()));
    assert!(r.guests[0].engine.svc.toast.as_ref().is_some_and(|t| t.text.contains("host left")));
}

#[test]
fn the_hosts_quit_asks_first() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    r.host.engine.request_back();
    r.steps(30);
    let (x, y) = quit_button(&r.host);
    r.host.tap(x, y);
    r.steps(30);
    assert_eq!(r.host.top(), "race", "one tap on QUIT does not end everyone's race");
    assert!(r.host.slot().is_some());
    assert_eq!(r.host_phase(), HostPhase::Racing);
}

#[test]
fn guest_and_host_leave_from_the_results() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    r.to_results();
    r.guests[0].engine.request_back();
    r.until(5, "guest back on the menu", |r| r.guests[0].top() == "multiplayer");
    r.steps(30);
    assert_eq!(r.humans(), 1);
    assert_eq!(r.host.top(), "online_results");
    r.host.engine.request_back();
    r.until(5, "host back on the menu", |r| r.host.top() == "multiplayer");
    assert!(r.phones().all(|p| !p.slot().is_some()));
}

#[test]
fn the_host_leaving_the_results_tells_the_guest() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    r.to_results();
    r.host.engine.request_back();
    r.until(8, "both back on the menu", |r| r.phones().all(|p| p.top() == "multiplayer"));
    assert!(r.guests[0].engine.svc.toast.as_ref().is_some_and(|t| t.text.contains("host left")));
}

// ─── rematch ────────────────────────────────────────────────────────────────

#[test]
fn rematch_twice_keeps_everyone_in_the_room() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    r.to_results();
    for round in 1..=2 {
        for p in r.phones() {
            p.slot().with(|s| s.me.set_rematch_ready(true));
        }
        r.until(15, &format!("rematch {round}: everyone on the race screen"), |r| r.phones().all(|p| p.top() == "race"));
        assert_eq!(r.humans(), 2);
        r.to_results();
        let rows = r.guests[0].slot().with(|s| s.me.results().map(|x| x.rows.len())).flatten();
        assert_eq!(rows, Some(4), "two people and two bots, round {round}");
    }
    assert!(r.phones().all(|p| matches!(phase_of(p), Some(ClientPhase::Results))));
}

#[test]
fn back_to_the_lobby_after_a_race_takes_everyone_there() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    r.to_results();
    r.host.slot().with(|s| s.host.as_mut().map(|h| h.to_lobby()));
    r.until(10, "everyone in the lobby", |r| r.phones().all(|p| p.top() == "lobby"));
    assert_eq!(r.humans(), 2);
}

// ─── joining while a race runs ──────────────────────────────────────────────

#[test]
fn joining_during_the_countdown_and_the_race_is_refused_nicely() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    // A third phone dials in during the countdown, then again mid-race.
    for wait_steps in [0usize, 400] {
        r.steps(wait_steps);
        let hub = r.hub.clone();
        let clock = r.clock.clone();
        let mut late = Phone::new("late");
        late.engine.router.apply(Transition::Push(Box::new(JoinScreen::with_code("KWTQPM").with_connector(move |_| {
            let me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Late", 5);
            Ok(OnlineSession::from_parts(me, None, clock.clone()))
        }))));
        for _ in 0..120 {
            r.time.fetch_add(STEP_MS, Ordering::Relaxed);
            r.host.frame();
            r.guests[0].frame();
            late.frame();
        }
        assert_eq!(late.top(), "join", "the join screen stays, showing why");
        assert!(!late.slot().is_some(), "no half-joined session is left behind");
        assert_eq!(r.humans(), 2, "the room is untouched");
        assert_eq!(r.host.top(), "race");
    }
}

#[test]
fn a_guest_admitted_in_the_same_frame_the_race_starts_goes_straight_to_the_race() {
    let mut r = Rig::new(1, 2);
    // The guest's phone sends its hello, then stalls (as a phone does) ...
    let g = r.add_guest("Guest", 3);
    for _ in 0..3 {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.guests[g].frame();
    }
    // ... while the host admits it and starts at once.
    for _ in 0..6 {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
    }
    let now = r.host.slot().with(|s| s.now_ms()).unwrap();
    assert!(r.host.slot().with(|s| s.host.as_mut().map(|h| h.start_race(now))).flatten().unwrap_or(false), "the host starts the race");
    r.time.fetch_add(STEP_MS, Ordering::Relaxed);
    r.host.frame();
    // The guest wakes up to welcome, lobby and race start together.
    r.until(10, "the guest on the race screen", |r| r.guests[g].top() == "race");
    assert_eq!(r.humans(), 2);
}

// ─── the Garage over the lobby, and screen changes the router drops ─────────

#[test]
fn a_guest_in_the_garage_stays_seated_and_is_pulled_into_the_race() {
    let mut r = Rig::new(1, 2);
    let g = r.add_guest("Guest", 3);
    r.seat_everyone();
    // The guest opens the Garage and stays there longer than the host's silence limit.
    r.guests[g].engine.router.apply(Transition::Push(Box::new(GarageScreen::new())));
    r.until(3, "the Garage open", |r| r.guests[g].top() == "garage");
    r.steps(8 * 60);
    assert_eq!(r.guests[g].top(), "garage");
    assert_eq!(r.humans(), 2, "still seated after 8 s in the Garage");
    assert!(matches!(phase_of(&r.guests[g]), Some(ClientPhase::Lobby)), "{:?}", phase_of(&r.guests[g]));
    // The host (and the guest, from the Garage) ready up: the race starts.
    r.host.slot().with(|s| s.me.set_ready(true));
    r.guests[g].slot().with(|s| s.me.set_ready(true));
    r.until(20, "both on the race screen", |r| r.phones().all(|p| p.top() == "race"));
    assert_eq!(r.humans(), 2);
    assert!(!r.guests[g].stack().contains(&"garage"), "the Garage was closed for the race: {:?}", r.guests[g].stack());
    // And it keeps its seat through the first seconds of the race.
    r.steps(10 * 60);
    assert_eq!(r.humans(), 2);
    assert!(matches!(phase_of(&r.guests[g]), Some(ClientPhase::Racing)));
}

#[test]
fn a_garage_visit_that_lasts_through_the_whole_race_start_does_not_drop_the_seat() {
    let mut r = Rig::new(1, 2);
    let g = r.add_guest("Guest", 3);
    r.seat_everyone();
    r.guests[g].engine.router.apply(Transition::Push(Box::new(GarageScreen::new())));
    r.until(3, "the Garage open", |r| r.guests[g].top() == "garage");
    let now = r.host.slot().with(|s| s.now_ms()).unwrap();
    r.host.slot().with(|s| s.host.as_mut().map(|h| h.start_race(now)));
    r.until(20, "the guest pulled onto the race screen", |r| r.guests[g].top() == "race");
    r.steps(8 * 60);
    assert_eq!(r.humans(), 2, "the host never dropped the guest");
}

#[test]
fn a_transition_asked_for_during_a_fade_cannot_lose_the_session() {
    let mut r = Rig::new(1, 2);
    let g = r.add_guest("Guest", 3);
    r.seat_everyone();
    // The guest taps "change" (the Garage push starts a fade) ...
    r.guests[g].engine.router.apply(Transition::Push(Box::new(GarageScreen::new())));
    assert!(r.guests[g].engine.router.input_blocked());
    // ... and the race starts inside that fade: the lobby's Replace(race) is refused.
    r.ready_everyone();
    for _ in 0..4 {
        r.step();
    }
    assert!(r.guests[g].slot().is_some(), "the guest still holds its room");
    assert!(r.humans() >= 2);
    r.until(20, "both on the race screen", |r| r.phones().all(|p| p.top() == "race"));
    assert_eq!(r.humans(), 2);
    assert!(matches!(phase_of(&r.guests[g]), Some(ClientPhase::Racing)));
    // The room is still the host's: nobody ended it.
    assert_eq!(r.host_phase(), HostPhase::Racing);
}

#[test]
fn the_host_stays_in_its_room_when_a_screen_change_is_dropped_mid_fade() {
    let mut r = Rig::new(1, 2);
    let g = r.add_guest("Guest", 3);
    r.seat_everyone();
    // The host taps "change", and the last guest readies inside the fade.
    r.host.engine.router.apply(Transition::Push(Box::new(GarageScreen::new())));
    r.guests[g].slot().with(|s| s.me.set_ready(true));
    r.host.slot().with(|s| s.me.set_ready(true));
    r.steps(6);
    assert!(r.host.slot().is_some());
    r.until(20, "both on the race screen", |r| r.phones().all(|p| p.top() == "race"));
    assert_eq!(r.humans(), 2, "the host did not end everyone's race");
    r.steps(5 * 60);
    assert_eq!(r.host_phase(), HostPhase::Racing);
    assert!(matches!(phase_of(&r.guests[g]), Some(ClientPhase::Racing)));
}

#[test]
fn a_room_no_screen_shows_is_left() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    // Something replaced the whole stack: the engine does not keep a room nobody can see.
    r.guests[0].engine.router.apply(Transition::Cut(Box::new(Root)));
    r.steps(5);
    assert!(!r.guests[0].slot().is_some());
    r.steps(30);
    assert_eq!(r.humans(), 1);
}

// ─── a join link while in a room ────────────────────────────────────────────

#[test]
fn a_join_link_in_a_room_asks_before_leaving_it() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    r.guests[0].engine.svc.join_code = Some(crate::keeps::JoinTarget::Code("KWTQPM".into()));
    r.steps(20);
    assert!(r.guests[0].slot().is_some(), "nothing happens until the player answers");
    assert!(r.guests[0].engine.svc.join_code.is_none(), "the lobby took the link");
    // Stay: the room goes on.
    let v = crate::platform::Viewport::new(egui::vec2(SIZE.0 as f32 / PPP, SIZE.1 as f32 / PPP), PPP, crate::platform::Insets::ZERO);
    let c = v.safe.center();
    let (stay, go) = ((c.x - 100.0, c.y + 55.0), (c.x + 106.0, c.y + 55.0));
    r.guests[0].tap(stay.0, stay.1);
    r.steps(20);
    assert!(r.guests[0].slot().is_some());
    assert_eq!(r.guests[0].top(), "lobby");
    // A second link, this time "Leave and join": the room is left and the code waits for the join screen.
    r.guests[0].engine.svc.join_code = Some(crate::keeps::JoinTarget::Code("HDXNZB".into()));
    r.steps(5);
    r.guests[0].tap(go.0, go.1);
    r.until(5, "guest left the room", |r| !r.guests[0].slot().is_some());
    assert_eq!(r.guests[0].engine.svc.join_code, Some(crate::keeps::JoinTarget::Code("HDXNZB".into())), "the code is handed on to be joined");
    r.until(5, "back on the menu", |r| r.guests[0].top() == "multiplayer");
    r.steps(30);
    assert_eq!(r.humans(), 1);
}

// ─── a join by code that has to wait for the host's relays ──────────────────

/// A link that fails the way the QUIC join does when no host answers.
struct NoAnswer {
    clock: LoopClock,
    until: u64,
    sent: bool,
}

impl ClientTransport for NoAnswer {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        if !self.sent && self.clock.now_ms() >= self.until {
            self.sent = true;
            return Some(ClientNetEvent::Failed("Code not found (check the code, or the host may have started). [FindingHost]".into()));
        }
        None
    }
    fn send_reliable(&mut self, _: Vec<u8>) {}
    fn send_lossy(&mut self, _: Vec<u8>) {}
    fn rtt_ms(&self) -> Option<u32> {
        None
    }
    fn close(&mut self) {}
}

#[test]
fn a_join_by_code_is_retried_until_the_host_answers() {
    let mut r = Rig::new(1, 2);
    let hub = r.hub.clone();
    let clock = r.clock.clone();
    let tries = Rc::new(Cell::new(0u32));
    let t2 = tries.clone();
    let mut joiner = Phone::new("joiner");
    joiner.engine.router.apply(Transition::Push(Box::new(JoinScreen::with_code("KWTQPM").with_connector(move |_| {
        t2.set(t2.get() + 1);
        let n = t2.get();
        let link: Box<dyn ClientTransport> = if n < 3 {
            // The first two offers are lost: "Code not found" after 10 s each.
            let now = clock.now_ms();
            Box::new(NoAnswer { clock: clock.clone(), until: now + 10_000, sent: false })
        } else {
            Box::new(hub.connect(LoopbackConfig::PERFECT))
        };
        Ok(OnlineSession::from_parts(ClientSession::new(link, "Joiner", 2), None, clock.clone()))
    }))));
    let mut seated = false;
    for _ in 0..(60 * 1000 / STEP_MS) {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
        joiner.frame();
        if joiner.top() == "lobby" {
            seated = true;
            break;
        }
        assert_ne!(joiner.stack().last().copied(), Some("join").filter(|_| tries.get() >= 3 && !joiner.slot().is_some()), "the join gave up");
    }
    assert!(seated, "the third try gets in (tries so far {}), screens {:?}", tries.get(), joiner.stack());
    assert_eq!(tries.get(), 3);
    assert_eq!(r.humans(), 2);
}

#[test]
fn a_join_by_code_stops_retrying_and_says_so() {
    let mut r = Rig::new(1, 2);
    let clock = r.clock.clone();
    let tries = Rc::new(Cell::new(0u32));
    let t2 = tries.clone();
    let mut joiner = Phone::new("joiner");
    joiner.engine.router.apply(Transition::Push(Box::new(JoinScreen::with_code("KWTQPM").with_connector(move |_| {
        t2.set(t2.get() + 1);
        let link = NoAnswer { clock: clock.clone(), until: clock.now_ms() + 10_000, sent: false };
        Ok(OnlineSession::from_parts(ClientSession::new(Box::new(link), "Joiner", 2), None, clock.clone()))
    }))));
    for _ in 0..(80 * 1000 / STEP_MS) {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
        joiner.frame();
    }
    assert!((3..=5).contains(&tries.get()), "about 35 s of tries, got {}", tries.get());
    assert_eq!(joiner.top(), "join");
    assert!(!joiner.slot().is_some(), "the failed attempt is gone");
}

// ─── joining a Keep by invite ───────────────────────────────────────────────

/// Answer the "Join a Keep?" card: `true` taps Join, `false` taps Not now.
fn answer_keep_confirm(p: &mut Phone, join: bool) {
    use crate::ui::screens::common::{prompt_buttons, KEEP_CONFIRM_H};
    let (not_now, go) = prompt_buttons(ui_screen(), KEEP_CONFIRM_H);
    tap_rect(p, if join { go } else { not_now });
}

#[test]
fn a_keep_joined_by_invite_is_saved_and_a_new_secret_replaces_it() {
    let mut r = Rig::new(1, 2);
    let (hub, clock) = (r.hub.clone(), r.clock.clone());
    let mut joiner = Phone::new("keep-joiner");
    let dir = joiner.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    for secret in [[7u8; 32], [8u8; 32]] {
        let (hub, clock) = (hub.clone(), clock.clone());
        let seen = Rc::new(Cell::new(None));
        let seen2 = seen.clone();
        joiner.engine.router.apply(Transition::Push(Box::new(JoinScreen::with_target(crate::keeps::JoinTarget::Invite { host_key: [9; 32], secret }).with_invite_connector(move |host, secret| {
            seen2.set(Some((*host, *secret)));
            let me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Joiner", 2);
            Ok(OnlineSession::from_parts(me, None, clock.clone()))
        }))));
        // A Keep not in My Keeps is asked about first (the second round is a saved one).
        for _ in 0..60 {
            joiner.frame();
        }
        if crate::keeps::load(&dir).is_empty() {
            assert!(seen.get().is_none(), "nothing is dialled before the player agrees");
            answer_keep_confirm(&mut joiner, true);
        }
        for _ in 0..600 {
            r.time.fetch_add(STEP_MS, Ordering::Relaxed);
            r.host.frame();
            joiner.frame();
            if joiner.top() == "lobby" {
                break;
            }
        }
        assert_eq!(joiner.top(), "lobby", "seated through the invite");
        assert_eq!(seen.get(), Some(([9; 32], secret)), "the connector got the link's two halves");
        joiner.slot().leave();
        joiner.engine.router.apply(Transition::PopTo("multiplayer"));
        for _ in 0..120 {
            joiner.frame();
        }
        assert_eq!(joiner.top(), "multiplayer");
    }
    let saved = crate::keeps::load(&dir);
    assert_eq!(saved.len(), 1, "same host key, one entry");
    assert_eq!(saved[0].secret, "08".repeat(32));
    assert_eq!(saved[0].host_key, "09".repeat(32));
    assert_eq!(saved[0].name, "Keep 090909", "a loopback host proves no name");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_keep_is_saved_once_per_join_not_once_per_frame_of_the_fade() {
    let mut r = Rig::new(1, 2);
    let (hub, clock) = (r.hub.clone(), r.clock.clone());
    let mut joiner = Phone::new("keep-save-once");
    let dir = joiner.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    joiner.engine.router.apply(Transition::Push(Box::new(JoinScreen::with_target(crate::keeps::JoinTarget::Invite { host_key: [9; 32], secret: [7; 32] }).with_invite_connector(move |_, _| {
        let me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Joiner", 2);
        Ok(OnlineSession::from_parts(me, None, clock.clone()))
    }))));
    for _ in 0..60 {
        joiner.frame();
    }
    answer_keep_confirm(&mut joiner, true);
    // Run until the Keep is saved; the join screen then fades out for several frames.
    let file = dir.join("keeps.json");
    for _ in 0..600 {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
        joiner.frame();
        if file.exists() {
            break;
        }
    }
    assert!(file.exists(), "saved on the way in");
    // Take it away: any later frame of the same join that saved again would bring it back.
    std::fs::remove_file(&file).unwrap();
    for _ in 0..600 {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
        joiner.frame();
        if joiner.top() == "lobby" {
            break;
        }
    }
    assert_eq!(joiner.top(), "lobby");
    for _ in 0..30 {
        joiner.frame();
    }
    assert!(!file.exists(), "one save per join");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_player_a_keep_removed_is_told_so_and_a_room_guest_keeps_the_room_words() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Keeper", 3);
    r.add_guest("Roomer", 4);
    r.seat_everyone();
    r.guests[0].slot().with(|s| s.keep_join = true);
    for g in 0..2 {
        let seat = r.guests[g].slot().with(|s| s.me.my_seat()).flatten().unwrap();
        r.host.slot().with(|s| s.host.as_mut().unwrap().kick(seat));
    }
    r.until(5, "both guests out of the room", |r| r.guests.iter().all(|g| !g.slot().is_some()));
    let toast = |g: &Phone| g.engine.svc.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default();
    assert_eq!(toast(&r.guests[0]), "You were removed from this Keep.");
    assert_eq!(toast(&r.guests[1]), "The host removed you from the game.");
}

/// The Keep (in a Rig, the host) sends `guest` a pass for the Keep `host_key`.
fn send_pass(r: &mut Rig, guest: usize, host_key: [u8; 32], secret: [u8; 32]) {
    let seat = r.guests[guest].slot().with(|s| s.me.my_seat()).flatten().expect("seated");
    let sent = r.host.slot().with(|s| {
        let host = s.host.as_mut().expect("the host's room");
        let peer = host.peer_of_seat(seat).expect("a peer");
        host.send_keep_pass(peer, race_session::KeepPass { host_key, secret })
    });
    assert_eq!(sent, Some(true));
}

#[test]
fn a_keep_pass_from_the_host_is_saved_once_to_my_keeps_and_joins_nothing() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Passed", 3);
    r.seat_everyone();
    let dir = r.guests[0].engine.svc.data_dir.clone();
    let file = dir.join("keeps.json");
    assert!(crate::keeps::load(&dir).is_empty() && !file.exists(), "a player who joined a room by code has no Keeps");
    send_pass(&mut r, 0, [0x5a; 32], [0xa5; 32]);
    r.until(5, "the pass saved", |_| file.exists());
    let saved = crate::keeps::load(&dir);
    assert_eq!(saved.len(), 1);
    assert_eq!((saved[0].host_key.as_str(), saved[0].secret.as_str()), ("5a".repeat(32).as_str(), "a5".repeat(32).as_str()));
    assert_eq!(saved[0].name, "Keep 5a5a5a", "a loopback host proves no name");
    assert_eq!(r.guests[0].top(), "lobby", "a pass joins nothing and leaves the player where they are");
    assert_eq!(r.humans(), 2);
    // Saved once: the file is not written again for a frame, and a second copy adds no entry.
    std::fs::remove_file(&file).unwrap();
    r.steps(120);
    assert!(!file.exists(), "no save without a new pass");
    send_pass(&mut r, 0, [0x5a; 32], [0xa5; 32]);
    r.until(5, "the second copy handled", |_| file.exists());
    assert_eq!(crate::keeps::load(&dir).len(), 1, "a second copy of the same pass does not duplicate");
    // A pass with a new secret for the same Keep replaces it; another Keep is a second entry.
    send_pass(&mut r, 0, [0x5a; 32], [0xb6; 32]);
    r.until(5, "the new secret saved", |r| crate::keeps::load(&r.guests[0].engine.svc.data_dir).first().is_some_and(|k| k.secret == "b6".repeat(32)));
    assert_eq!(crate::keeps::load(&dir).len(), 1);
    send_pass(&mut r, 0, [0x11; 32], [0x22; 32]);
    r.until(5, "another Keep saved", |r| crate::keeps::load(&r.guests[0].engine.svc.data_dir).len() == 2);
}

/// A join screen for `target` whose connectors count their calls.
fn counted_join(target: crate::keeps::JoinTarget) -> (JoinScreen, Rc<Cell<u32>>) {
    let calls = Rc::new(Cell::new(0u32));
    let (c1, c2) = (calls.clone(), calls.clone());
    let screen = JoinScreen::with_target(target)
        .with_connector(move |_| {
            c1.set(c1.get() + 1);
            Err("test".into())
        })
        .with_invite_connector(move |_, _| {
            c2.set(c2.get() + 1);
            Err("test".into())
        });
    (screen, calls)
}

#[test]
fn a_link_to_an_unseen_keep_asks_before_dialling() {
    let mut joiner = Phone::new("keep-unseen");
    let dir = joiner.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    let (screen, calls) = counted_join(crate::keeps::JoinTarget::Invite { host_key: [5; 32], secret: [6; 32] });
    joiner.engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..120 {
        joiner.frame();
    }
    assert_eq!((joiner.top(), calls.get()), ("join", 0), "the card waits");
    assert!(!joiner.slot().is_some());
    // Not now: back out, nothing dialled, nothing saved.
    answer_keep_confirm(&mut joiner, false);
    for _ in 0..30 {
        joiner.frame();
    }
    assert_eq!((joiner.top(), calls.get()), ("multiplayer", 0));
    assert!(crate::keeps::load(&dir).is_empty());
    // Join: now it dials (and fails here, with the card for it).
    let (screen, calls) = counted_join(crate::keeps::JoinTarget::Invite { host_key: [5; 32], secret: [6; 32] });
    joiner.engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..60 {
        joiner.frame();
    }
    answer_keep_confirm(&mut joiner, true);
    for _ in 0..30 {
        joiner.frame();
    }
    assert_eq!((joiner.top(), calls.get()), ("join", 1));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_link_to_a_saved_keep_joins_at_once() {
    let mut joiner = Phone::new("keep-saved");
    let dir = joiner.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::keeps::record_join(&dir, &[5; 32], &[1; 32], Some("Cousin's Keep"), crate::keeps::unix_now()).unwrap();
    let (screen, calls) = counted_join(crate::keeps::JoinTarget::Invite { host_key: [5; 32], secret: [6; 32] });
    joiner.engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..30 {
        joiner.frame();
    }
    assert_eq!(calls.get(), 1, "a Keep already in My Keeps is not asked about");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_room_link_that_is_not_a_code_opens_the_empty_entry_not_a_join() {
    for raw in ["ABCDEFGH12", "O0O0O0", "KWTQP", "KWTQPMX", ""] {
        let mut joiner = Phone::new("bad-code-link");
        let (screen, calls) = counted_join(crate::keeps::JoinTarget::Code(raw.into()));
        joiner.engine.router.apply(Transition::Push(Box::new(screen)));
        for _ in 0..30 {
            joiner.frame();
        }
        assert_eq!((joiner.top(), calls.get()), ("join", 0), "{raw:?} is not dialled");
        assert!(!joiner.slot().is_some(), "{raw:?}");
    }
    // A real code, in any case, still goes straight out.
    let mut joiner = Phone::new("good-code-link");
    let (screen, calls) = counted_join(crate::keeps::JoinTarget::Code("kwtqpm".into()));
    joiner.engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..30 {
        joiner.frame();
    }
    assert_eq!(calls.get(), 1);
}

#[test]
fn my_keeps_is_read_again_when_the_multiplayer_screen_comes_back() {
    use crate::ui::screens::multiplayer::{MultiplayerScreen, LAST_READ};
    let mut p = Phone::new("keeps-refresh");
    let dir = p.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    p.engine.router.apply(Transition::Push(Box::new(MultiplayerScreen::new())));
    for _ in 0..10 {
        p.frame();
    }
    assert_eq!(LAST_READ.with(|c| c.get()), Some(0));
    // A lobby (or Nearby, or a race) covers it; a Keep is saved meanwhile.
    p.engine.router.apply(Transition::Push(Box::new(JoinScreen::new())));
    for _ in 0..30 {
        p.frame();
    }
    crate::keeps::record_join(&dir, &[5; 32], &[1; 32], Some("Cousin's Keep"), crate::keeps::unix_now()).unwrap();
    p.engine.router.apply(Transition::Pop);
    for _ in 0..10 {
        p.frame();
    }
    assert_eq!(p.top(), "multiplayer");
    assert_eq!(LAST_READ.with(|c| c.get()), Some(1), "the list is read again on the way back");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn my_keeps_is_not_read_while_it_fades_out_under_the_screen_it_pushed() {
    use crate::ui::screens::multiplayer::{MultiplayerScreen, LAST_READ, READS};
    let mut p = Phone::new("keeps-fade");
    let dir = p.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    p.engine.router.apply(Transition::Push(Box::new(MultiplayerScreen::new())));
    for _ in 0..30 {
        p.frame();
    }
    let reads = READS.with(|c| c.get());
    assert_eq!(p.top(), "multiplayer");
    // A link makes it push the join screen; it keeps updating through the 0.14 s fade-out,
    // and must not read a file the pushed join has not yet written.
    p.engine.svc.join_code = Some(crate::keeps::JoinTarget::Invite { host_key: [5; 32], secret: [6; 32] });
    for _ in 0..60 {
        p.frame();
        assert_eq!(READS.with(|c| c.get()), reads, "no read on {} while this screen is covered or fading", p.top());
        if p.top() == "join" {
            break;
        }
    }
    assert_eq!(p.top(), "join");
    for _ in 0..30 {
        p.frame();
    }
    crate::keeps::record_join(&dir, &[5; 32], &[6; 32], Some("Cousin's Keep"), crate::keeps::unix_now()).unwrap();
    p.engine.router.apply(Transition::Pop);
    for _ in 0..30 {
        p.frame();
    }
    assert_eq!(p.top(), "multiplayer");
    assert_eq!(LAST_READ.with(|c| c.get()), Some(1), "read once the screen is on top again");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_second_link_swaps_the_keep_card_and_a_tap_for_the_first_does_not_answer_it() {
    let mut joiner = Phone::new("keep-swap");
    let dir = joiner.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    let seen = Rc::new(Cell::new(None));
    let seen2 = seen.clone();
    let screen = JoinScreen::with_target(crate::keeps::JoinTarget::Invite { host_key: [5; 32], secret: [6; 32] }).with_invite_connector(move |host, _| {
        seen2.set(Some(host[0]));
        Err("test".into())
    });
    joiner.engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..60 {
        joiner.frame();
    }
    // Link B replaces the card, and the player's tap (meant for A's card) lands at once.
    joiner.engine.svc.join_code = Some(crate::keeps::JoinTarget::Invite { host_key: [7; 32], secret: [8; 32] });
    joiner.frame();
    answer_keep_confirm(&mut joiner, true);
    assert_eq!(seen.get(), None, "nothing is dialled by a tap that was meant for the other card");
    assert_eq!(joiner.top(), "join");
    // Once the card has been up for a moment, Join answers it, for B.
    for _ in 0..40 {
        joiner.frame();
    }
    answer_keep_confirm(&mut joiner, true);
    assert_eq!(seen.get(), Some(7), "the join is for the new card's Keep");
    let _ = std::fs::remove_dir_all(dir);
}

/// A guest in a lobby gets a link to `target`; returns once the card is up.
fn lobby_with_link(r: &mut Rig, target: crate::keeps::JoinTarget) {
    r.guests[0].engine.svc.join_code = Some(target);
    r.steps(40);
}

#[test]
fn an_unknown_keep_link_in_a_lobby_is_one_card_and_the_join_screen_does_not_ask_again() {
    use crate::ui::screens::common::{prompt_buttons, KEEP_CONFIRM_H};
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    let dir = r.guests[0].engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    let link = crate::keeps::JoinTarget::Invite { host_key: [5; 32], secret: [6; 32] };
    let (not_now, join) = prompt_buttons(ui_screen(), KEEP_CONFIRM_H);
    // Not now: the player stays where they were, and nothing is agreed.
    lobby_with_link(&mut r, link.clone());
    tap_rect(&mut r.guests[0], not_now);
    r.steps(20);
    assert!(r.guests[0].slot().is_some());
    assert_eq!(r.guests[0].top(), "lobby");
    assert_eq!(r.guests[0].engine.svc.join_confirmed, None);
    assert!(r.guests[0].engine.svc.join_code.is_none());
    // Join: the room is left, the agreement lives in memory for that Keep only.
    lobby_with_link(&mut r, link.clone());
    tap_rect(&mut r.guests[0], join);
    r.until(5, "guest left the room", |r| !r.guests[0].slot().is_some());
    assert_eq!(r.guests[0].engine.svc.join_confirmed, Some([5; 32]));
    assert_eq!(r.guests[0].engine.svc.join_code, Some(link.clone()));
    // The join screen it hands over to dials once, with no second card.
    r.until(5, "back on the menu", |r| r.guests[0].top() == "multiplayer");
    let target = r.guests[0].engine.svc.join_code.take().unwrap();
    let (screen, calls) = counted_join(target);
    r.guests[0].engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..30 {
        r.guests[0].frame();
    }
    assert_eq!(calls.get(), 1, "dialled once");
    assert_eq!(r.guests[0].engine.svc.join_confirmed, None, "the agreement is used up");
    // A link that comes later for the same Keep asks again.
    let (screen, calls) = counted_join(link);
    r.guests[0].engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..30 {
        r.guests[0].frame();
    }
    assert_eq!(calls.get(), 0, "no agreement left: the join screen asks");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_agreement_for_one_keep_does_not_cover_another() {
    let mut joiner = Phone::new("keep-agree-other");
    let dir = joiner.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    joiner.engine.svc.join_confirmed = Some([5; 32]);
    let (screen, calls) = counted_join(crate::keeps::JoinTarget::Invite { host_key: [9; 32], secret: [6; 32] });
    joiner.engine.router.apply(Transition::Push(Box::new(screen)));
    for _ in 0..30 {
        joiner.frame();
    }
    assert_eq!(calls.get(), 0);
    assert_eq!(joiner.engine.svc.join_confirmed, None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_known_keep_link_in_a_lobby_keeps_the_leave_and_join_card() {
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    let dir = r.guests[0].engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::keeps::record_join(&dir, &[5; 32], &[1; 32], Some("Cousin's Keep"), crate::keeps::unix_now()).unwrap();
    lobby_with_link(&mut r, crate::keeps::JoinTarget::Invite { host_key: [5; 32], secret: [6; 32] });
    let v = crate::platform::Viewport::new(egui::vec2(SIZE.0 as f32 / PPP, SIZE.1 as f32 / PPP), PPP, crate::platform::Insets::ZERO);
    let c = v.safe.center();
    r.guests[0].tap(c.x + 106.0, c.y + 55.0);
    r.until(5, "guest left the room", |r| !r.guests[0].slot().is_some());
    assert_eq!(r.guests[0].engine.svc.join_confirmed, None, "a saved Keep needs no agreement");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_keep_that_does_not_answer_is_not_saved() {
    let mut r = Rig::new(1, 2);
    let clock = r.clock.clone();
    let mut joiner = Phone::new("keep-nobody");
    let dir = joiner.engine.svc.data_dir.clone();
    let _ = std::fs::remove_dir_all(&dir);
    joiner.engine.router.apply(Transition::Push(Box::new(JoinScreen::with_target(crate::keeps::JoinTarget::Invite { host_key: [0; 32], secret: [0; 32] }).with_invite_connector(move |_, _| {
        let link = NoAnswer { clock: clock.clone(), until: clock.now_ms() + 10_000, sent: false };
        Ok(OnlineSession::from_parts(ClientSession::new(Box::new(link), "Joiner", 2), None, clock.clone()))
    }))));
    for _ in 0..30 {
        joiner.frame();
    }
    answer_keep_confirm(&mut joiner, true);
    for _ in 0..(80 * 1000 / STEP_MS) {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
        joiner.frame();
    }
    assert_eq!(joiner.top(), "join", "the failure stays on screen");
    assert!(!joiner.slot().is_some());
    assert!(crate::keeps::load(&dir).is_empty());
    assert!(!dir.join("keeps.json").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_full_or_started_room_is_not_retried() {
    for why in ["Room full.", "That game has already started.", "The host is running a different version of the app.", "That isn't a valid room code."] {
        assert!(!crate::ui::screens::join::is_transient(why), "{why}");
    }
    let invite = format!("{} [FindingHost]", race_session::INVITE_NO_ANSWER);
    for why in [invite.as_str(), "Code not found (check the code, or the host may have started). [FindingHost]", "Couldn't reach the internet meeting points (relays). [Preparing]", "Found the host but couldn't connect to it."] {
        assert!(crate::ui::screens::join::is_transient(why), "{why}");
    }
}

// ─── backgrounding ──────────────────────────────────────────────────────────

#[test]
fn a_guest_backgrounded_mid_race_keeps_its_seat_while_the_app_ticks() {
    let mut r = Rig::new(2, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    r.steps(5 * 60);
    r.guests[0].engine.on_background();
    // 15 s away: no frames, only the background tick (what the Android loop does).
    for _ in 0..(15_000 / STEP_MS) {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
        assert!(r.guests[0].engine.wants_background_tick());
        r.guests[0].engine.background_tick();
    }
    assert_eq!(r.humans(), 2, "the host never dropped the seat");
    r.guests[0].engine.on_foreground();
    r.steps(60);
    assert_eq!(r.guests[0].top(), "race");
    assert!(matches!(phase_of(&r.guests[0]), Some(ClientPhase::Racing)));
    assert!(r.guests[0].engine.svc.toast.is_some(), "welcomed back");
}

#[test]
fn a_frozen_guest_is_told_it_was_away_not_that_the_connection_broke() {
    let mut r = Rig::new(2, 2);
    r.add_guest("Guest", 3);
    r.to_race();
    r.steps(5 * 60);
    r.guests[0].engine.on_background();
    // The process is frozen: nothing ticks the guest at all.
    for _ in 0..(15_000 / STEP_MS) {
        r.time.fetch_add(STEP_MS, Ordering::Relaxed);
        r.host.frame();
    }
    assert_eq!(r.humans(), 1, "the host moved the silent seat to a bot");
    r.guests[0].engine.on_foreground();
    r.until(5, "the guest back on the menu", |r| r.guests[0].top() == "multiplayer");
    let toast = r.guests[0].engine.svc.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default();
    assert!(toast.contains("away"), "honest message, got {toast:?}");
    assert!(!toast.contains("Lost the connection"), "{toast:?}");
}

#[test]
fn a_dropped_link_without_an_absence_is_still_a_lost_connection() {
    assert_eq!(dead_text(&ClientPhase::Disconnected("idle".into())).as_deref(), Some("Lost the connection to the host."));
}

// ─── the room code is offered only when a join can work ─────────────────────

#[test]
fn leaving_twice_is_harmless() {
    let slot = OnlineSlot::new();
    slot.put(OnlineSession::offline_host("Ann", 1, HostConfig::default(), "KWTQPM"));
    slot.leave();
    slot.leave();
    assert!(!slot.is_some());
}

#[test]
fn a_dropped_screen_handle_does_not_end_the_session() {
    let slot = OnlineSlot::new();
    slot.put(OnlineSession::offline_host("Ann", 1, HostConfig::default(), "KWTQPM"));
    drop(slot.clone());
    assert!(slot.with(|s| s.pump(Input::default())).is_some());
    assert!(slot.is_some());
}

#[test]
fn an_offline_room_has_a_code_at_once() {
    let s = OnlineSession::offline_host("Ann", 1, HostConfig::default(), "KWTQPM");
    assert!(s.code_ready());
}

#[test]
fn a_downloaded_track_syncs_app_metadata_and_guest_save_repairs_a_corrupt_file() {
    let raw: Vec<_> = (0..=160).map(|i| {
        let a = i as f32 / 160.0 * std::f32::consts::TAU;
        race_core::Vec2::new(a.cos() * 95.0, a.sin() * 95.0)
    }).collect();
    let shaped = race_core::shape(&raw, false, 18.0).unwrap();
    let track = race_core::custom_track_from(&shaped, "Guest save loop", TrackId::Oasis, 7, 180, vec![], 1);
    let mut r = Rig::new(1, 1);
    let g = r.add_guest("GuestCustomSave", 3);
    r.seat_everyone();
    r.host.slot().with(|s| s.select_custom_track(&track).unwrap());
    r.until(5, "both phones have the checked custom track", |r| r.phones().all(|p| {
        p.slot().with(|s| s.me.custom_track() == Some(&track) && s.active_custom == Some(track.hash())).unwrap_or(false)
    }));
    let dir = r.guests[g].engine.svc.data_dir.clone();
    std::fs::create_dir_all(dir.join("tracks")).unwrap();
    std::fs::write(crate::custom_tracks::path(&dir, track.hash()), b"corrupt").unwrap();
    // Give the guest a fresh lobby so its saved-status cache reads this file.
    r.guests[g].engine.router.apply(Transition::Replace(Box::new(LobbyScreen::new())));
    r.steps(70);
    use crate::ui::widgets::S4;
    let width = SIZE.0 as f32 / PPP;
    let height = SIZE.1 as f32 / PPP;
    let body_width = width - 2.0 * S4;
    let side_width = (body_width * 0.43).clamp(250.0, 340.0);
    let x = width - S4 - side_width * 0.5;
    let y = height - S4 - 12.0 - 17.0;
    r.guests[g].tap(x, y);
    assert_eq!(crate::custom_tracks::load(&dir, track.hash()), Some(track.clone()));
    assert!(r.guests[g].engine.svc.toast.as_ref().is_some_and(|t| t.text == "Saved in My Tracks"));
    // The same received track can be used immediately for racing.
    r.ready_everyone();
    r.until(10, "everyone racing on the received geometry", |r| r.phones().all(|p| p.top() == "race"));
    for p in r.phones() {
        assert_eq!(p.slot().with(|s| s.me.view().unwrap().world.track.name.clone()), Some(track.name.clone()));
        assert_eq!(p.slot().with(|s| s.me.view().unwrap().world.cfg.laps), Some(7));
    }
}

// ─── armed rooms (host migration), on real QUIC loopback and real time ──────

use decented_net::{Identity, NetConfig, RoomInfo};
use race_session::migration::ArmingState;
use race_session::QuicClient;
use std::time::{Duration, Instant};

/// A runtime identity with a My Signet approval for Stampede.
fn approved() -> Identity {
    let persona = decented_net::Keys::generate();
    let runtime = Identity::ephemeral();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let event = decented_signet::attestation::unsigned_attestation(
        race_session::APP_TAG,
        &persona.public_key(),
        &runtime.pubkey(),
        now,
    )
    .sign_with_keys(&persona)
    .unwrap();
    runtime.with_attestation(serde_json::to_string(&event).unwrap())
}

fn lan_cfg() -> NetConfig {
    NetConfig::new(
        std::env::temp_dir().join(format!("stampede-app-armed-{}", std::process::id())),
        race_session::APP_TAG,
    )
    .lan_only()
}

fn armed_room_config() -> HostConfig {
    HostConfig {
        bots: 2,
        auto_start: false,
        ..HostConfig::default()
    }
}

/// A host phone's session on `handle`, pumped until its room is open.
fn open_room(cfg: &NetConfig, id: Identity, handle: decented_net::HostHandle) -> OnlineSession {
    let mut host = OnlineSession::network_host(
        cfg.clone(),
        id,
        QuicHost::new(handle),
        "Host",
        0,
        armed_room_config(),
    );
    let t = Instant::now();
    while !host.room_ready() {
        host.pump(Input::default());
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "the room never opened: {:?}",
            host.status()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    host
}

/// Where Nearby players dial the room (loopback), its LAN token and code.
fn dial_info(host: &OnlineSession) -> (std::net::SocketAddr, String, String) {
    host.link
        .as_ref()
        .unwrap()
        .with(|q| {
            let h = q.handle().unwrap();
            (
                std::net::SocketAddr::from(([127, 0, 0, 1], h.local_addr().unwrap().port())),
                h.lan_token(),
                q.code().unwrap().to_string(),
            )
        })
        .unwrap()
}

/// Pump the phones that are alive (`alive[i]`), one frame each, until `cond`.
pub(crate) fn run_until(
    phones: &mut [OnlineSession],
    alive: &[bool],
    secs: u64,
    what: &str,
    mut cond: impl FnMut(&[OnlineSession]) -> bool,
) {
    let t = Instant::now();
    while !cond(phones) {
        for (p, on) in phones.iter_mut().zip(alive) {
            if *on {
                let input = p.bot_drive();
                p.pump(input);
                // A standby whose player left runs on in the background.
                p.pump_background();
            }
        }
        assert!(
            t.elapsed() < Duration::from_secs(secs),
            "timed out ({secs} s) waiting for: {what}; states {:?}; dead {:?}",
            phones
                .iter()
                .map(|p| p.migration_state())
                .collect::<Vec<_>>(),
            phones.iter().map(|p| p.dead_reason()).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Each state a phone shows, in order, without repeats.
#[derive(Default)]
pub(crate) struct Seen(Vec<Vec<MigrationState>>);
impl Seen {
    fn note(&mut self, phones: &[OnlineSession]) {
        self.0.resize(phones.len(), Vec::new());
        for (seen, p) in self.0.iter_mut().zip(phones) {
            let s = p.migration_state();
            if seen.last() != Some(&s) {
                seen.push(s);
            }
        }
    }
    /// `want` appears in phone `i`'s states in this order (others may sit between).
    fn in_order(&self, i: usize, want: &[fn(&MigrationState) -> bool]) -> bool {
        let mut it = self.0[i].iter();
        want.iter().all(|w| it.any(w))
    }
}

fn is_armed(s: &MigrationState) -> bool {
    matches!(s, MigrationState::Armed { unprotected } if unprotected.is_empty())
}
fn after_takeover(s: &MigrationState) -> bool {
    matches!(
        s,
        MigrationState::Resumed | MigrationState::UnprotectedAfterTakeover
    )
}

/// Three phones on real QUIC loopback, armed over the room's messages: the
/// phone host (0), the standby phone (1) and a client (2), every one
/// `Armed`, and the dial details of the room. The states each phone showed
/// are in `seen`.
pub(crate) fn armed_room(
    seen: &mut Seen,
) -> (
    Vec<OnlineSession>,
    NetConfig,
    std::net::SocketAddr,
    String,
    String,
) {
    let cfg = lan_cfg();
    let id = approved();
    let room = decented_net::host(
        &cfg,
        &id,
        RoomInfo {
            name: "Armed".into(),
            max_players: 8,
        },
    );
    let host = open_room(&cfg, id, room);
    let (addr, token, code) = dial_info(&host);
    let join = |name: &str, character: u8| {
        let id = approved();
        let link = QuicClient::new(decented_net::join_addr(&cfg, &id, addr, &token));
        OnlineSession::network_client(
            cfg.clone(),
            id,
            link,
            &code,
            Some((addr, token.clone())),
            name,
            character,
        )
    };
    let mut phones = vec![host, join("Backup", 1), join("Other", 2)];
    let all = [true, true, true];
    run_until(&mut phones, &all, 15, "everyone seated", |p| {
        p[0].authority().is_some_and(|a| a.human_count() == 3)
            && p[1..].iter().all(|c| c.me().lobby().is_some())
    });
    let seat = phones[0]
        .me()
        .lobby()
        .unwrap()
        .seats
        .iter()
        .find(|s| s.name == "Backup")
        .unwrap()
        .id;

    // Arming: the host chooses the backup, its person says yes.
    phones[0].choose_backup(seat);
    assert_eq!(phones[0].migration_state(), MigrationState::Asking);
    run_until(&mut phones, &all, 10, "the backup is asked", |p| {
        p[1].migration_state() == MigrationState::ConsentRequested
    });
    seen.note(&phones);
    phones[1].answer_consent(true);
    run_until(&mut phones, &all, 25, "every phone armed", |p| {
        seen.note(p);
        p.iter().all(|x| is_armed(&x.migration_state()))
    });
    assert!(
        phones[0].is_host() && phones[0].authority().is_some(),
        "the armed host keeps its room"
    );
    assert_eq!(phones[0].code(), Some(code.as_str()));
    assert!(phones[0].room_ready(), "the room status survives the pin");

    (phones, cfg, addr, token, code)
}

/// The app-level takeover: a phone host, the standby phone and a client
/// arm over the room's messages, race, and the host is killed (never pumped
/// again, nothing closed). The standby promotes on its watch, the client
/// recovers and resumes on the same room code, and the promoted phone keeps
/// hosting with its own seat. While armed, the room's door stays shut to a
/// new joiner. Then the new host dies too: the client's session ends with
/// the documented reason.
#[test]
fn an_app_host_standby_and_client_arm_race_and_survive_a_killed_host() {
    let mut seen = Seen::default();
    let (mut phones, cfg, addr, token, code) = armed_room(&mut seen);
    let all = [true, true, true];
    // The door: while armed, a new ordinary joiner is refused, never seated.
    let stranger = approved();
    let mut door = QuicClient::new(decented_net::join_addr(&cfg, &stranger, addr, &token));
    let mut refused = false;
    run_until(&mut phones, &all, 10, "the stranger refused", |_| {
        while let Some(ev) = door.poll() {
            refused |= matches!(
                ev,
                ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
            );
        }
        refused
    });
    assert_eq!(
        phones[0].authority().unwrap().human_count(),
        3,
        "nobody seated outside the armed roster"
    );

    // The race starts through the armed commits.
    assert!(phones[0].start_race(), "queued for the next batch");
    run_until(&mut phones, &all, 15, "everyone racing", |p| {
        p.iter()
            .all(|x| matches!(x.me().phase(), ClientPhase::Racing) && x.me().view().is_some())
    });
    let t = Instant::now();
    run_until(&mut phones, &all, 10, "a moment of racing", |p| {
        t.elapsed() > Duration::from_millis(1500)
            && p.iter().all(|x| x.me().clock_offset_ms().is_some())
    });
    assert!(
        phones.iter().all(|p| is_armed(&p.migration_state())),
        "{:?}",
        phones
            .iter()
            .map(|p| p.migration_state())
            .collect::<Vec<_>>()
    );

    // Kill the host: it is never pumped again and closes nothing.
    let killed = Instant::now();
    let alive = [false, true, true];
    run_until(&mut phones, &alive, 15, "both survivors resumed", |p| {
        seen.note(p);
        after_takeover(&p[1].migration_state()) && after_takeover(&p[2].migration_state())
    });
    let resumed_ms = killed.elapsed().as_millis();
    assert!(phones[1].is_host(), "the promoted phone hosts");
    assert_eq!(phones[1].code(), Some(code.as_str()), "the same room code");
    assert!(phones[1..].iter().all(|p| p.dead_reason().is_none()));
    // The successor commits on: game time and the race move on both phones.
    let tick = |p: &OnlineSession| p.me().view().map(|v| v.world.tick);
    let (a0, b0) = (
        phones[1].authority().unwrap().world().map(|w| w.tick),
        tick(&phones[2]),
    );
    let t = Instant::now();
    run_until(&mut phones, &alive, 10, "play goes on", |_| {
        t.elapsed() > Duration::from_secs(1)
    });
    assert!(
        phones[1].authority().unwrap().world().map(|w| w.tick) > a0,
        "the successor's world moves"
    );
    assert!(tick(&phones[2]) > b0, "the client's race moves");
    assert!(matches!(phones[2].me().phase(), ClientPhase::Racing));
    assert_eq!(
        phones[1].authority().unwrap().human_count(),
        2,
        "the killed host's player departed"
    );
    assert!(
        seen.in_order(
            0,
            &[
                |s| *s == MigrationState::Asking,
                |s| *s == MigrationState::Arming,
                is_armed
            ]
        ),
        "host {:?}",
        seen.0[0]
    );
    assert!(
        seen.in_order(
            1,
            &[
                |s| *s == MigrationState::ConsentRequested,
                |s| *s == MigrationState::Arming,
                is_armed,
                |s| *s == MigrationState::Recovering,
                after_takeover
            ]
        ),
        "standby {:?}",
        seen.0[1]
    );
    assert!(
        seen.in_order(
            2,
            &[
                |s| *s == MigrationState::Arming,
                is_armed,
                |s| *s == MigrationState::Recovering,
                after_takeover
            ]
        ),
        "client {:?}",
        seen.0[2]
    );
    println!(
        "APP TAKEOVER resumed_after_kill_ms={resumed_ms} states={:?}",
        seen.0
    );

    // A second host failure is not taken over: the session ends.
    let alive = [false, false, true];
    run_until(&mut phones, &alive, 15, "the client's session ends", |p| {
        p[2].dead_reason().is_some()
    });
    assert_eq!(phones[2].dead_reason().as_deref(), Some(SECOND_FAILURE));
}

#[test]
fn arming_states_and_reasons_read_plainly() {
    use decented_net::migration::Error;
    assert_eq!(paused_reason(&Error::Unauthorized), BACKUP_LOST);
    assert_eq!(BACKUP_LOST, "Backup lost — the room is paused");
    assert!(paused_reason(&Error::Invalid("game clock skew over 250 ms")).contains("paused"));
    assert_eq!(
        ended_reason(&Error::Invalid("session lost: the successor stopped")),
        SECOND_FAILURE
    );
    assert_eq!(
        arming_state(&ArmingState::Asking, true),
        MigrationState::ConsentRequested
    );
    assert_eq!(
        arming_state(&ArmingState::Asking, false),
        MigrationState::Asking
    );
    for s in [
        ArmingState::AwaitingConsent,
        ArmingState::Activating,
        ArmingState::Armed,
        ArmingState::Switching,
    ] {
        assert_eq!(arming_state(&s, false), MigrationState::Arming);
    }
    let reason = "No capability ACK from Other within 5 s".to_string();
    assert_eq!(
        arming_state(
            &ArmingState::Unprotected {
                reason: reason.clone()
            },
            false
        ),
        MigrationState::Unprotected {
            reason: reason.clone()
        }
    );
    assert_eq!(
        arming_state(
            &ArmingState::Refused {
                reason: reason.clone()
            },
            false
        ),
        MigrationState::Refused { reason }
    );
    // A room with no network refuses with a reason and stays unarmed.
    let mut s = OnlineSession::offline_host("Ann", 1, HostConfig::default(), "KWTQPM");
    s.choose_backup(0);
    assert!(
        matches!(s.migration_state(), MigrationState::Refused { reason } if reason.contains("network"))
    );
    assert!(s.is_host() && s.set_laps(2), "the room plays on unarmed");
}

#[test]
fn an_online_code_room_refuses_arming_with_a_visible_reason() {
    let cfg = lan_cfg();
    let id = approved();
    let mut online = cfg.clone();
    online.relays = vec!["wss://relay.invalid".into()];
    let handle = decented_net::host_with_relay(
        &online,
        &id,
        RoomInfo {
            name: "Online".into(),
            max_players: 8,
        },
        Some(Box::new(decented_net::relay::FakeRelayHub::new().client())),
    );
    let mut host = open_room(&cfg, id, handle);
    host.choose_backup(0);
    assert!(
        matches!(host.migration_state(), MigrationState::Refused { reason } if reason.contains("online code")),
        "{:?}",
        host.migration_state()
    );
    assert!(
        host.authority().is_some_and(|a| !a.migration_frozen()),
        "the lobby is not held"
    );
}

#[test]
fn a_room_with_a_bluetooth_player_refuses_arming_with_a_visible_reason() {
    let mut cfg = lan_cfg();
    // A virtual host withholds its attestation by default; Stampede joins need it.
    cfg.withhold_host_attestation = Some(false);
    let id = approved();
    let (radio, rx) = decented_net::virtual_link_queue(16);
    let room = decented_net::host_virtual(
        &cfg,
        &id,
        RoomInfo {
            name: "BLE".into(),
            max_players: 8,
        },
        rx,
    );
    let mut host = open_room(&cfg, id, room);
    let (joiner, host_end) = decented_net::virtual_pair("BLE", "host", 256);
    radio.try_send(host_end).expect("the host takes the link");
    let guest_id = approved();
    let link = QuicClient::new(decented_net::join_virtual(&cfg, &guest_id, joiner));
    let mut phones = vec![
        host,
        OnlineSession::network_client(cfg.clone(), guest_id, link, "BLE", None, "Radio", 3),
    ];
    run_until(
        &mut phones,
        &[true, true],
        15,
        "the Bluetooth player seated",
        |p| p[0].authority().is_some_and(|a| a.human_count() == 2),
    );
    host = phones.remove(0);
    let seat = host
        .me()
        .lobby()
        .unwrap()
        .seats
        .iter()
        .find(|s| s.name == "Radio")
        .unwrap()
        .id;
    host.choose_backup(seat);
    assert!(
        matches!(host.migration_state(), MigrationState::Refused { reason } if reason.contains("Bluetooth")),
        "{:?}",
        host.migration_state()
    );
    drop(radio);
}

/// The standby phone leaves an armed room: its seat's `Leave` goes through
/// its route, its runtime goes with its slot, and the armed host pauses with
/// "Backup lost — the room is paused". The client is told it lost its backup
/// and stays in the room.
/// A standby device that goes away (its session dropped: its transports
/// close, nothing pumps it) pauses the armed room as backup lost, and the
/// client loses its backup but stays.
#[test]
fn a_standby_device_that_goes_pauses_the_armed_room_as_backup_lost() {
    let (mut phones, ..) = armed_room(&mut Seen::default());
    drop(phones.remove(1));
    let alive = [true, true];
    run_until(&mut phones, &alive, 10, "the host paused", |p| {
        p[0].migration_state()
            == MigrationState::Paused {
                reason: BACKUP_LOST.into(),
            }
    });
    run_until(&mut phones, &alive, 5, "the client unprotected", |p| {
        p[1].migration_state()
            == MigrationState::Unprotected {
                reason: BACKUP_LOST.into(),
            }
    });
    assert!(
        phones[0].dead_reason().is_none() && phones[1].dead_reason().is_none(),
        "nobody is thrown out"
    );
    assert!(phones[0].is_host());
}

/// The backup phone stalls (not pumped, nothing closed) for 300 ms
/// mid-race, so the batch on the wire gets its receipt 300 ms late: past
/// the 250 ms game-clock budget. The host shows a soft pause (backup lost,
/// or behind once its session time holds at the budget), never a latched
/// one, and plays on armed by itself once the receipts come back; the
/// backup, whose own stall the watch grace covers, does not promote.
#[test]
fn a_backup_stall_past_the_skew_budget_soft_pauses_the_host_and_play_goes_on() {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    let all = [true, true, true];
    assert!(phones[0].start_race(), "queued for the next batch");
    run_until(&mut phones, &all, 15, "everyone racing", |p| {
        p.iter()
            .all(|x| matches!(x.me().phase(), ClientPhase::Racing) && x.me().view().is_some())
    });
    let t = Instant::now();
    run_until(&mut phones, &all, 5, "a moment of racing", |_| {
        t.elapsed() > Duration::from_millis(500)
    });
    let tick = |p: &OnlineSession| p.authority().and_then(|a| a.world().map(|w| w.tick));
    let before = tick(&phones[0]);
    let mut host = Vec::new();
    let mut behind = false;
    let stalled = [true, false, true];
    let t = Instant::now();
    run_until(&mut phones, &stalled, 5, "the backup's stall", |p| {
        host.push(p[0].migration_state());
        behind |= p[0].role.host_behind();
        t.elapsed() > Duration::from_millis(300)
    });
    run_until(&mut phones, &all, 10, "the host armed again", |p| {
        host.push(p[0].migration_state());
        behind |= p[0].role.host_behind();
        is_armed(&p[0].migration_state())
    });
    assert!(behind, "the host never showed the behind pause");
    let t = Instant::now();
    run_until(&mut phones, &all, 10, "a moment of racing", |p| {
        host.push(p[0].migration_state());
        t.elapsed() > Duration::from_millis(500)
    });
    assert!(
        host.iter().any(|s| matches!(s, MigrationState::Paused { .. })),
        "the stall never paused the host: {host:?}"
    );
    for state in &host {
        assert!(
            !matches!(state, MigrationState::Paused { reason } if reason != BACKUP_LOST && reason != BEHIND),
            "the host paused for another reason: {state:?}"
        );
    }
    assert!(
        is_armed(&phones[0].migration_state()),
        "the host plays on armed: {:?} after {host:?}",
        phones[0].migration_state()
    );
    assert!(
        is_armed(&phones[1].migration_state()),
        "the backup promoted on its own stall"
    );
    assert!(tick(&phones[0]) > before, "the race moves on");
    assert!(phones.iter().all(|p| p.dead_reason().is_none()));
}

/// The armed lobby ticking at 33 ms, as the app does with no window
/// (`BACKGROUND_TICK`): every phone pumps once per 33 ms, the backup half a
/// frame after the host. The host sends each batch from `stage`, in the
/// frame that staged it, so the backup's next frame answers and the host's
/// next frame commits: about one commit per host frame (two frames per
/// commit if the batch waited for the next poll), each up to 50 ms of game
/// time. So when the frames hold their rate the lobby's game time keeps up
/// with real time and the host never shows the behind pause. A loaded
/// machine that cannot hold the frame rate checks only the commits per
/// frame (some receipts then miss the next frame, so the bound has slack).
#[test]
fn an_armed_lobby_at_33_ms_frames_commits_every_frame_and_keeps_real_time() {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    /// Every phone for `span` of real time, one frame each per `frame`,
    /// the host first and the others half a frame later (two phones' frames
    /// fall at independent phases). `each` sees the phones after every host
    /// frame.
    fn paced(
        phones: &mut [OnlineSession],
        frame: Duration,
        span: Duration,
        mut each: impl FnMut(&[OnlineSession]),
    ) {
        let start = Instant::now();
        let mut next = start;
        while start.elapsed() < span {
            let input = phones[0].bot_drive();
            phones[0].pump(input);
            phones[0].pump_background();
            each(phones);
            std::thread::sleep((next + frame / 2).saturating_duration_since(Instant::now()));
            for p in &mut phones[1..] {
                let input = p.bot_drive();
                p.pump(input);
                p.pump_background();
            }
            // A late frame delays the next one; frames never bunch up.
            next = (next + frame).max(Instant::now());
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    }
    let frame = Duration::from_millis(33);
    // A second to settle into the paced cycle, then four measured.
    paced(&mut phones, frame, Duration::from_secs(1), |_| {});
    let committed = |p: &OnlineSession| {
        p.role
            .host_committed()
            .expect("the host left the armed room")
    };
    let (index, logical) = committed(&phones[0]);
    let (mut frames, mut behind_frames) = (0u64, 0u64);
    let t = Instant::now();
    paced(&mut phones, frame, Duration::from_secs(4), |p| {
        frames += 1;
        behind_frames += u64::from(p[0].role.host_behind());
    });
    let wall = t.elapsed().as_millis() as u64;
    let (last, now) = committed(&phones[0]);
    let (commits, game) = (last - index, now - logical);
    let held = frames * 40 >= wall;
    println!(
        "STAMPEDE ARMED LOBBY 33 MS wall_ms={wall} game_ms={game} host_frames={frames} commits={commits} behind_frames={behind_frames} frame_rate_held={held}"
    );
    assert!(
        !matches!(phones[0].migration_state(), MigrationState::Paused { reason } if reason != BEHIND),
        "the host paused: {:?}",
        phones[0].migration_state()
    );
    // Waiting for the next poll caps it at one commit per two frames.
    assert!(
        commits * 100 >= frames * 60,
        "{commits} commits in {frames} host frames"
    );
    if held {
        assert_eq!(
            behind_frames, 0,
            "the host showed the behind pause in {behind_frames} of {frames} frames"
        );
        assert!(
            game * 100 >= wall * 90,
            "game time ran at {}% of real time ({game} ms in {wall} ms)",
            game * 100 / wall
        );
    }
}

/// The takeover correction bound after a soft pause. The backup phone stalls
/// 450 ms mid-race, so the host's paced clock holds for about 200 ms. Right
/// after the host plays on armed again it is killed. Every survivor (the
/// client, and the standby's own player) must still resume: each one's clock
/// correction at the cut is within the 250 ms cap, so none is refused.
#[test]
fn a_host_killed_right_after_a_soft_pause_still_resumes_every_survivor() {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    let all = [true, true, true];
    assert!(phones[0].start_race(), "queued for the next batch");
    run_until(&mut phones, &all, 15, "everyone racing", |p| {
        p.iter()
            .all(|x| matches!(x.me().phase(), ClientPhase::Racing) && x.me().view().is_some())
    });
    let t = Instant::now();
    run_until(&mut phones, &all, 5, "clocks synced", |p| {
        t.elapsed() > Duration::from_millis(500)
            && p.iter().all(|x| x.me().clock_offset_ms().is_some())
    });
    let offsets = |p: &[OnlineSession]| {
        p.iter()
            .map(|x| x.me().clock_offset_ms())
            .collect::<Vec<_>>()
    };
    let before = offsets(&phones);
    let stalled = [true, false, true];
    let t = Instant::now();
    run_until(&mut phones, &stalled, 5, "the backup's stall", |_| {
        t.elapsed() > Duration::from_millis(450)
    });
    run_until(&mut phones, &all, 10, "the host armed again", |p| {
        is_armed(&p[0].migration_state())
    });
    let after = offsets(&phones);
    // Killed at once: never pumped again, nothing closed.
    let alive = [false, true, true];
    run_until(&mut phones, &alive, 15, "both survivors resumed", |p| {
        after_takeover(&p[1].migration_state()) && after_takeover(&p[2].migration_state())
    });
    assert!(phones[1..].iter().all(|p| p.dead_reason().is_none()));
    println!("APP SOFT PAUSE TAKEOVER offsets_before={before:?} offsets_at_kill={after:?}");
}

/// The takeover correction bound while play resumes after a soft pause. The
/// backup phone stalls 450 ms mid-race; once the host plays on armed again,
/// the room catches up to the host's held clock, and the host is killed
/// 100-250 ms later (one fresh room per offset). Both survivors (the client
/// and the standby's own seat) resume, each with a correction within the
/// 250 ms cap in either sign.
#[test]
fn a_host_killed_while_play_resumes_after_a_soft_pause_still_resumes_every_survivor() {
    for kill_after in [100, 150, 200, 250] {
        soft_pause_takeover(false, Some(450), kill_after);
    }
}

/// A takeover from the armed lobby (term 0, where no client freezes): the
/// host is killed in the lobby and both survivors resume within the cap.
#[test]
fn a_host_killed_in_the_armed_lobby_resumes_every_survivor() {
    soft_pause_takeover(true, None, 0);
}

/// As above, after the backup stalled 450 ms in the lobby (the host's
/// clock holds there too): killed at once, and while play resumes.
#[test]
fn a_host_killed_in_the_lobby_after_a_soft_pause_resumes_every_survivor() {
    for kill_after in [0, 150, 250] {
        soft_pause_takeover(true, Some(450), kill_after);
    }
}

/// An armed room (racing unless `lobby`), the backup stalled for `stall` ms
/// (if any), then the host killed `kill_after` ms after it plays on armed
/// again. Every survivor must resume, and each cut's clock correction must
/// be accepted and within 250 ms.
fn soft_pause_takeover(lobby: bool, stall: Option<u64>, kill_after: u64) {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    let all = [true, true, true];
    if !lobby {
        assert!(phones[0].start_race(), "queued for the next batch");
        run_until(&mut phones, &all, 15, "everyone racing", |p| {
            p.iter()
                .all(|x| matches!(x.me().phase(), ClientPhase::Racing) && x.me().view().is_some())
        });
    }
    let t = Instant::now();
    run_until(&mut phones, &all, 5, "clocks synced", |p| {
        t.elapsed() > Duration::from_millis(500)
            && p.iter().all(|x| x.me().clock_offset_ms().is_some())
    });
    if let Some(stall) = stall {
        let stalled = [true, false, true];
        let t = Instant::now();
        run_until(&mut phones, &stalled, 5, "the backup's stall", |_| {
            t.elapsed() > Duration::from_millis(stall)
        });
        run_until(&mut phones, &all, 10, "the host armed again", |p| {
            is_armed(&p[0].migration_state())
        });
    }
    let t = Instant::now();
    run_until(&mut phones, &all, 5, "play resumed", |_| {
        t.elapsed() >= Duration::from_millis(kill_after)
    });
    armed::CUTS.with(|c| c.borrow_mut().clear());
    // Killed: never pumped again, nothing closed.
    let alive = [false, true, true];
    run_until(&mut phones, &alive, 15, "both survivors resumed", |p| {
        let dead = p[1..].iter().map(|x| x.dead_reason()).collect::<Vec<_>>();
        assert!(
            dead.iter().all(Option::is_none),
            "a survivor ended (lobby {lobby}, stall {stall:?}, killed {kill_after} ms after play resumed): {dead:?}; cuts {:?}",
            armed::CUTS.with(|c| c.borrow().clone())
        );
        after_takeover(&p[1].migration_state()) && after_takeover(&p[2].migration_state())
    });
    let cuts = armed::CUTS.with(|c| c.borrow().clone());
    println!(
        "APP TAKEOVER CUT lobby={lobby} stall_ms={stall:?} kill_after_ms={kill_after} cuts={cuts:?}"
    );
    assert!(phones[1..].iter().all(|p| p.dead_reason().is_none()));
    for who in ["client", "own seat"] {
        assert!(
            cuts.iter()
                .any(|(w, d, ok)| *w == who && *ok && d.unsigned_abs() <= 250),
            "no accepted {who} cut within 250 ms: {cuts:?}"
        );
    }
}

/// The standby promotes while the original still runs: the host phone
/// stalls (not pumped, nothing closed) past the standby's 200 ms watch, on
/// a still-live pair link, and the standby's promotion takes real time (as
/// on a phone). The signed `Promote` must still reach the original: once it
/// runs again it pauses as superseded instead of hosting on unaware.
#[test]
fn a_standby_promoting_on_a_live_original_tells_it_and_it_stands_down() {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    let all = [true, true, true];
    assert!(phones[0].start_race(), "queued for the next batch");
    run_until(&mut phones, &all, 15, "everyone racing", |p| {
        p.iter()
            .all(|x| matches!(x.me().phase(), ClientPhase::Racing) && x.me().view().is_some())
    });
    armed::PROMOTE_DELAY_MS.with(|d| d.set(3));
    let stalled = [false, true, true];
    run_until(&mut phones, &stalled, 10, "the standby promoted", |p| {
        matches!(p[1].role, Role::Takeover(_) | Role::Successor(_))
    });
    armed::PROMOTE_DELAY_MS.with(|d| d.set(0));
    run_until(&mut phones, &all, 5, "the original superseded", |p| {
        p[0].migration_state()
            == MigrationState::Paused {
                reason: armed::SUPERSEDED.into(),
            }
    });
    assert!(
        phones[0].dead_reason().is_none(),
        "the original is not thrown out"
    );
}

/// Item 2.2: the standby device's player leaves mid-race. Its seat departs
/// like any human's, the device stays the standby in the background, and
/// the room plays on armed (never paused). Then the host is killed: the
/// background standby promotes without a seat and the client resumes under
/// it, racing on.
#[test]
fn the_standby_players_leave_keeps_the_standby_and_a_seatless_takeover_resumes_survivors() {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    race_armed(&mut phones);
    phones[1].leave();
    assert!(phones[1].in_background(), "the standby runs on");
    let all = [true, true, true];
    run_until(&mut phones, &all, 10, "the standby's seat departed", |p| {
        p[0].authority().is_some_and(|a| a.human_count() == 2)
    });
    let t = Instant::now();
    run_until(&mut phones, &all, 10, "play goes on", |p| {
        assert!(
            is_armed(&p[0].migration_state()) && is_armed(&p[2].migration_state()),
            "the room stopped playing armed: {:?} / {:?}",
            p[0].migration_state(),
            p[2].migration_state()
        );
        t.elapsed() > Duration::from_millis(1500)
    });
    assert!(phones[1].in_background(), "the standby still runs");
    assert!(matches!(phones[2].me().phase(), ClientPhase::Racing));

    // Kill the host: never pumped again, nothing closed.
    let killed = Instant::now();
    let alive = [false, true, true];
    run_until(&mut phones, &alive, 15, "the client resumed", |p| {
        seen.note(p);
        after_takeover(&p[2].migration_state())
    });
    let resumed_ms = killed.elapsed().as_millis();
    assert!(phones[1].is_host(), "the background standby hosts");
    assert!(phones[1].in_background(), "still no screen, still running");
    assert_eq!(
        phones[1].authority().unwrap().human_count(),
        1,
        "only the client remains seated"
    );
    let tick = |p: &OnlineSession| p.me().view().map(|v| v.world.tick);
    let before = tick(&phones[2]);
    let t = Instant::now();
    run_until(&mut phones, &alive, 10, "the race goes on", |_| {
        t.elapsed() > Duration::from_secs(1)
    });
    assert!(tick(&phones[2]) > before, "the client's race moves");
    assert!(matches!(phones[2].me().phase(), ClientPhase::Racing));
    assert!(phones[2].dead_reason().is_none());
    assert!(
        seen.in_order(
            2,
            &[
                is_armed,
                |s| *s == MigrationState::Recovering,
                after_takeover
            ]
        ),
        "client {:?}",
        seen.0[2]
    );
    println!("APP SEATLESS TAKEOVER resumed_after_kill_ms={resumed_ms}");
}

/// The standby phone's own session-bound re-dial fails after the host pinned
/// (its network drops for the whole switch), while its pair link stays up.
/// The standby role is fixed once armed: the phone keeps its runtime with no
/// player, the host reports that player unprotected and the room plays on
/// armed, never paused. Then the host is killed inside the 30 s parked
/// departure, while that player is still a member at C: the backup takes over
/// (retaining the seat with fresh journals, then departing it), and the
/// client resumes under it and races on.
#[test]
fn a_standby_whose_own_redial_fails_after_the_pin_keeps_the_room_armed_and_takes_over() {
    let cfg = lan_cfg();
    let id = approved();
    let room = decented_net::host(
        &cfg,
        &id,
        RoomInfo {
            name: "Redial".into(),
            max_players: 8,
        },
    );
    let host = open_room(&cfg, id, room);
    let (addr, token, code) = dial_info(&host);
    let join = |name: &str, character: u8| {
        let id = approved();
        let link = QuicClient::new(decented_net::join_addr(&cfg, &id, addr, &token));
        OnlineSession::network_client(
            cfg.clone(),
            id,
            link,
            &code,
            Some((addr, token.clone())),
            name,
            character,
        )
    };
    let mut phones = vec![host, join("Backup", 1), join("Other", 2)];
    let all = [true, true, true];
    run_until(&mut phones, &all, 15, "everyone seated", |p| {
        p[0].authority().is_some_and(|a| a.human_count() == 3)
            && p[1..].iter().all(|c| c.me().lobby().is_some())
    });
    let seat = phones[0]
        .me()
        .lobby()
        .unwrap()
        .seats
        .iter()
        .find(|s| s.name == "Backup")
        .unwrap()
        .id;
    phones[0].choose_backup(seat);
    run_until(&mut phones, &all, 10, "the backup is asked", |p| {
        p[1].migration_state() == MigrationState::ConsentRequested
    });
    phones[1].answer_consent(true);
    run_until(&mut phones, &all, 25, "the backup switching", |p| {
        matches!(p[1].role, Role::ClientSwitch(_))
    });
    phones[1]
        .role
        .switch_mut()
        .expect("the backup's switch")
        .fail_join_for_test();
    let switched = Instant::now();
    run_until(&mut phones, &all, 30, "the backup's re-dial failed", |p| {
        !matches!(p[1].role, Role::ClientSwitch(_))
            && matches!(p[0].role, Role::HostArmed(_))
            && is_armed(&p[2].migration_state())
    });
    let failed_ms = switched.elapsed().as_millis();
    // The room plays on armed: the host lists the backup's player as
    // unprotected and never pauses.
    let t = Instant::now();
    run_until(&mut phones, &all, 10, "a moment armed", |p| {
        let host = p[0].migration_state();
        assert!(
            matches!(&host, MigrationState::Armed { unprotected } if unprotected.len() == 1),
            "the host stopped playing armed: {host:?}"
        );
        assert!(
            is_armed(&p[2].migration_state()),
            "the client left armed play"
        );
        t.elapsed() > Duration::from_millis(1500)
    });
    assert!(
        phones[1].role.standby_mut().is_some(),
        "the backup dropped its standby runtime"
    );
    assert_eq!(
        phones[0].authority().unwrap().human_count(),
        3,
        "the backup's player is still a member (parked)"
    );
    assert!(phones[0].start_race(), "queued for the next batch");
    run_until(&mut phones, &all, 15, "the client racing", |p| {
        matches!(p[2].me().phase(), ClientPhase::Racing) && p[2].me().view().is_some()
    });

    // Kill the host inside the parked departure: never pumped again.
    let killed = Instant::now();
    let alive = [false, true, true];
    run_until(&mut phones, &alive, 15, "the client resumed", |p| {
        after_takeover(&p[2].migration_state())
    });
    let resumed_ms = killed.elapsed().as_millis();
    assert!(phones[1].is_host(), "the backup hosts");
    assert_eq!(phones[1].code(), Some(code.as_str()), "the same room code");
    run_until(
        &mut phones,
        &alive,
        10,
        "the backup's player departed",
        |p| p[1].authority().is_some_and(|a| a.human_count() == 1),
    );
    let tick = |p: &OnlineSession| p.me().view().map(|v| v.world.tick);
    let before = tick(&phones[2]);
    let t = Instant::now();
    run_until(&mut phones, &alive, 10, "the race goes on", |_| {
        t.elapsed() > Duration::from_secs(1)
    });
    assert!(tick(&phones[2]) > before, "the client's race moves");
    assert!(matches!(phones[2].me().phase(), ClientPhase::Racing));
    assert!(phones[1..].iter().all(|p| p.dead_reason().is_none()));
    println!(
        "APP STANDBY REDIAL FAILED failed_after_ms={failed_ms} resumed_after_kill_ms={resumed_ms}"
    );
}

/// The standby phone's re-dial fails after the host pinned, and at the
/// switch's end its pair link carries nothing for a moment (its network
/// drops from 220 ms before the switch gives up, a grace second past its
/// budget, until just after), so the watch is stale at that moment. A stale
/// watch there means unknown, not a dead host: the pair link is still up
/// (its 6 s idle timeout has not passed). The phone neither promotes nor
/// drops its standby; once its network is back the watch is fresh again and
/// the standby runs on without a player. The host's pause while it heard no answers clears:
/// it plays on armed with that player unprotected, instead of staying paused
/// as backup lost.
#[test]
fn a_standby_whose_watch_is_stale_when_its_redial_fails_holds_and_is_kept() {
    let cfg = lan_cfg();
    let id = approved();
    let room = decented_net::host(
        &cfg,
        &id,
        RoomInfo {
            name: "Redial".into(),
            max_players: 8,
        },
    );
    let host = open_room(&cfg, id, room);
    let (addr, token, code) = dial_info(&host);
    let join = |name: &str, character: u8| {
        let id = approved();
        let link = QuicClient::new(decented_net::join_addr(&cfg, &id, addr, &token));
        OnlineSession::network_client(
            cfg.clone(),
            id,
            link,
            &code,
            Some((addr, token.clone())),
            name,
            character,
        )
    };
    let mut phones = vec![host, join("Backup", 1), join("Other", 2)];
    let all = [true, true, true];
    run_until(&mut phones, &all, 15, "everyone seated", |p| {
        p[0].authority().is_some_and(|a| a.human_count() == 3)
            && p[1..].iter().all(|c| c.me().lobby().is_some())
    });
    let seat = phones[0]
        .me()
        .lobby()
        .unwrap()
        .seats
        .iter()
        .find(|s| s.name == "Backup")
        .unwrap()
        .id;
    phones[0].choose_backup(seat);
    run_until(&mut phones, &all, 10, "the backup is asked", |p| {
        p[1].migration_state() == MigrationState::ConsentRequested
    });
    phones[1].answer_consent(true);
    run_until(&mut phones, &all, 25, "the backup switching", |p| {
        matches!(p[1].role, Role::ClientSwitch(_))
    });
    let switch = phones[1].role.switch_mut().expect("the backup's switch");
    switch.fail_join_for_test();
    // The failed re-dial gives up a grace second past the switch's budget.
    let gate =
        switch.deadline() + Duration::from_millis(race_session::migration::SWITCH_REJOIN_GRACE_MS);
    run_until(
        &mut phones,
        &all,
        15,
        "just before the switch gives up",
        |_| Instant::now() + Duration::from_millis(220) >= gate,
    );
    assert!(
        matches!(phones[1].role, Role::ClientSwitch(_)),
        "the backup's switch ended early"
    );
    // The backup's network drops across that moment, for longer than its
    // 200 ms watch; every phone runs on.
    let offline = |p: &mut OnlineSession, offline: bool| {
        p.role
            .standby_mut()
            .expect("the backup's standby")
            .offline_for_test(offline)
    };
    offline(&mut phones[1], true);
    let mut host_states = Vec::new();
    run_until(&mut phones, &all, 5, "the switch gave up", |p| {
        host_states.push(p[0].migration_state());
        p[1].migration_state() != MigrationState::Arming
    });
    assert!(
        !matches!(phones[1].role, Role::Plain),
        "the backup dropped its standby on a stale watch"
    );
    // Its network is back: the backup keeps its standby without a player,
    // and the host plays on armed with that player unprotected.
    offline(&mut phones[1], false);
    run_until(
        &mut phones,
        &all,
        5,
        "the backup kept as the standby",
        |p| {
            host_states.push(p[0].migration_state());
            matches!(p[1].role, Role::Standby(_))
        },
    );
    run_until(&mut phones, &all, 10, "the host armed again", |p| {
        host_states.push(p[0].migration_state());
        matches!(&p[0].migration_state(), MigrationState::Armed { unprotected } if unprotected.len() == 1)
    });
    // Its watch of the backup went stale too: the soft pause showed, and
    // cleared by itself.
    assert!(
        host_states.contains(&MigrationState::Paused {
            reason: BACKUP_LOST.into()
        }),
        "the host never paused"
    );
    // The outage can also leave the host's game clock held at its budget, so
    // the soft behind pause may show as it clears; no other reason may.
    for state in &host_states {
        assert!(
            !matches!(state, MigrationState::Paused { reason } if reason != BACKUP_LOST && reason != BEHIND),
            "the host paused for another reason: {state:?}"
        );
    }
    let t = Instant::now();
    run_until(&mut phones, &all, 10, "a moment armed", |p| {
        let host = p[0].migration_state();
        assert!(
            matches!(&host, MigrationState::Armed { unprotected } if unprotected.len() == 1),
            "the host stopped playing armed: {host:?}"
        );
        assert!(
            matches!(p[1].role, Role::Standby(_)),
            "the backup left the standby role"
        );
        t.elapsed() > Duration::from_millis(1500)
    });
    assert!(phones.iter().all(|p| p.dead_reason().is_none()));
}

/// Latch the standby phone's runtime: its pair listener moves out, leaving a
/// closed shell, so its next guard fails `Closed` and latches. The listener
/// is returned and kept by the caller, so the original's end of the pair
/// link is not closed by this.
fn latch_standby(phone: &mut OnlineSession) -> QuicHost {
    let live = phone.role.standby_mut().expect("the armed standby");
    assert!(live.failure().is_none(), "already latched");
    live.pair_network_mut().0.take_listener()
}

/// The spec exception for an own-seat `Leave` in flight at C, through the
/// app wiring: the standby phone's player leaves mid-race and the host dies
/// in the same step (never pumped again, nothing closed), so the `Leave`
/// never commits under the original. The background standby promotes with
/// its seat still seated at C, drives that seat (`OwnSeat` from the route's
/// parts), the `Leave` drains once as a departure, and the seat retires: the
/// promoted phone hosts on in the background without a seat, only the
/// client stays seated, and the client resumes and races on.
#[test]
fn a_standby_players_leave_in_flight_at_the_takeover_drains_and_its_seat_departs() {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    race_armed(&mut phones);
    let humans_before = phones[0].authority().unwrap().human_count();
    // The Leave goes out on this call; the host is never pumped again.
    phones[1].leave();
    assert!(phones[1].in_background(), "the standby runs on");
    let killed = Instant::now();
    let alive = [false, true, true];
    run_until(&mut phones, &alive, 15, "the client resumed", |p| {
        seen.note(p);
        after_takeover(&p[2].migration_state())
    });
    let resumed_ms = killed.elapsed().as_millis();
    assert_eq!(humans_before, 3);
    assert!(phones[1].is_host(), "the background standby hosts");
    assert!(phones[1].in_background(), "still no screen, still running");
    assert_eq!(
        phones[1].ended, None,
        "the promoted phone's armed session ended"
    );
    assert!(matches!(phones[1].role, Role::Successor(_)), "no successor");
    assert!(
        phones[1].role.client().is_none(),
        "the successor kept a seat"
    );
    assert_eq!(
        phones[1].authority().unwrap().human_count(),
        1,
        "the standby's seat departed with the host's: only the client remains"
    );
    let tick = |p: &OnlineSession| p.me().view().map(|v| v.world.tick);
    let before = tick(&phones[2]);
    let t = Instant::now();
    run_until(&mut phones, &alive, 10, "the race goes on", |_| {
        t.elapsed() > Duration::from_secs(1)
    });
    assert!(tick(&phones[2]) > before, "the client's race moves");
    assert!(matches!(phones[2].me().phase(), ClientPhase::Racing));
    assert!(phones[2].dead_reason().is_none());
    println!("APP IN-FLIGHT OWN LEAVE TAKEOVER resumed_after_kill_ms={resumed_ms}");
}

/// A standby that latches a fault can never promote (`promote` hands it back
/// every frame), so the standby phone drops it at once and plays on as an
/// ordinary unprotected client. When the original then leaves (its link
/// `Left`), the seated standby phone reaches the ordinary loss path and its
/// session ends, instead of showing "Waiting for the host" forever.
#[test]
fn a_latched_standby_stops_and_the_seated_phone_ends_when_the_host_leaves() {
    let mut seen = Seen::default();
    let (mut phones, ..) = armed_room(&mut seen);
    race_armed(&mut phones);
    let _listener = latch_standby(&mut phones[1]);
    let all = [true, true, true];
    let t = Instant::now();
    run_until(&mut phones, &all, 5, "a few frames after the latch", |p| {
        seen.note(p);
        t.elapsed() > Duration::from_millis(300)
    });
    phones[0].leave();
    let left = Instant::now();
    let bound = race_session::migration::SESSION_LOST_AFTER_MS / 1000 + 3;
    run_until(
        &mut phones,
        &[false, true, true],
        bound,
        "the standby phone's session ended",
        |p| {
            seen.note(p);
            p[1].dead_reason().is_some()
        },
    );
    let reason = phones[1].dead_reason().unwrap();
    assert_eq!(reason, "Lost the connection to the host.");
    println!(
        "APP LATCHED STANDBY seated ended_after_leave_ms={} reason={reason:?} states={:?}",
        left.elapsed().as_millis(),
        seen.0[1]
    );
}

/// The same latch on a standby phone whose player already left: the
/// background standby is torn down at once (nothing is left to pump).
#[test]
fn a_latched_background_standby_is_torn_down() {
    let (mut phones, ..) = armed_room(&mut Seen::default());
    race_armed(&mut phones);
    phones[1].leave();
    assert!(phones[1].in_background(), "the standby runs on");
    let all = [true, true, true];
    run_until(&mut phones, &all, 10, "the standby's seat departed", |p| {
        p[0].authority().is_some_and(|a| a.human_count() == 2)
    });
    let _listener = latch_standby(&mut phones[1]);
    run_until(
        &mut phones,
        &all,
        2,
        "the background standby stopped",
        |p| !p[1].in_background(),
    );
    assert_eq!(
        phones[1].dead_reason().as_deref(),
        Some("The background standby stopped.")
    );
    assert!(
        phones[0].dead_reason().is_none() && phones[2].dead_reason().is_none(),
        "the room goes on"
    );
}

/// Item 2.3: the host's pin is refused until its deadline, so every client's
/// session-bound re-dial fails. After its budget (`SWITCH_JOIN_MS` plus
/// `SWITCH_REJOIN_GRACE_MS`) the client falls back to an ordinary rejoin and
/// is seated again in the thawed, unarmed lobby, showing `Unprotected`
/// ("Rejoining the lobby unarmed"). As documented, it rejoined on the
/// switch's own link and has no arming link: it cannot arm again in this
/// session.
#[test]
fn a_client_whose_redial_fails_rejoins_the_unarmed_lobby_and_cannot_arm_again() {
    let cfg = lan_cfg();
    let id = approved();
    let room = decented_net::host(
        &cfg,
        &id,
        RoomInfo {
            name: "Redial".into(),
            max_players: 8,
        },
    );
    let host = open_room(&cfg, id, room);
    let (addr, token, code) = dial_info(&host);
    let join = |name: &str, character: u8| {
        let id = approved();
        let link = QuicClient::new(decented_net::join_addr(&cfg, &id, addr, &token));
        OnlineSession::network_client(
            cfg.clone(),
            id,
            link,
            &code,
            Some((addr, token.clone())),
            name,
            character,
        )
    };
    let mut phones = vec![host, join("Backup", 1), join("Other", 2)];
    let all = [true, true, true];
    run_until(&mut phones, &all, 15, "everyone seated", |p| {
        p[0].authority().is_some_and(|a| a.human_count() == 3)
            && p[1..].iter().all(|c| c.me().lobby().is_some())
    });
    let seat = phones[0]
        .me()
        .lobby()
        .unwrap()
        .seats
        .iter()
        .find(|s| s.name == "Backup")
        .unwrap()
        .id;
    phones[0].choose_backup(seat);
    run_until(&mut phones, &all, 10, "the backup is asked", |p| {
        p[1].migration_state() == MigrationState::ConsentRequested
    });
    phones[1].answer_consent(true);
    run_until(&mut phones, &all, 25, "the host switching", |p| {
        matches!(p[0].role, Role::HostSwitch(_))
    });
    let Role::HostSwitch(switch) = &mut phones[0].role else {
        unreachable!()
    };
    switch.refuse_pins_for_test();
    let switched = Instant::now();
    run_until(&mut phones, &all, 30, "the client rejoined unarmed", |p| {
        let rejoining = |s: &MigrationState| {
            matches!(s, MigrationState::Unprotected { reason }
                if reason.contains("Rejoining the lobby unarmed"))
        };
        rejoining(&p[2].migration_state())
            && matches!(p[2].role, Role::Plain)
            && p[2].me().lobby().is_some()
            && p[0].authority().is_some_and(|a| a.human_count() == 3)
    });
    let fallback = switched.elapsed();
    assert!(
        fallback
            >= Duration::from_millis(
                race_session::migration::SWITCH_JOIN_MS
                    + race_session::migration::SWITCH_REJOIN_GRACE_MS
            ),
        "fell back after {fallback:?}, before its budget"
    );
    assert!(phones[2].dead_reason().is_none());
    assert!(
        matches!(phones[0].role, Role::Plain),
        "the host plays unarmed"
    );
    assert!(
        phones[2].mig.as_ref().is_some_and(|m| m.client.is_none()),
        "the rejoined client keeps no arming link"
    );
    println!(
        "APP REDIAL FALLBACK fallback_ms={} client_state={:?}",
        fallback.as_millis(),
        phones[2].migration_state()
    );
}

/// Racing phones on an armed room: start the race through the armed commits
/// and race for a moment, every phone `Armed`.
fn race_armed(phones: &mut [OnlineSession]) {
    let all = vec![true; phones.len()];
    assert!(phones[0].start_race(), "queued for the next batch");
    run_until(phones, &all, 15, "everyone racing", |p| {
        p.iter()
            .all(|x| matches!(x.me().phase(), ClientPhase::Racing) && x.me().view().is_some())
    });
    let t = Instant::now();
    run_until(phones, &all, 10, "a moment of racing", |_| {
        t.elapsed() > Duration::from_millis(500)
    });
    assert!(phones.iter().all(|p| is_armed(&p.migration_state())));
}

/// Item 2.1: a standby frame stall alone cannot make a live original look
/// silent. The standby phone is not pumped for about 300 ms while the host
/// and the client are; its next frame reads and installs everything queued
/// on the pair link before it looks at its watch, so it does not promote on
/// that frame. (Only that frame is the property. The host's standby watch
/// convicts a standby that answers nothing for 200 ms, so the host shows the
/// soft backup-lost pause during the stall; it keeps polling and probing,
/// so it does not fall silent, and the pause clears once the standby
/// answers again.)
#[test]
fn a_standby_whose_pump_stalls_does_not_promote() {
    let (mut phones, ..) = armed_room(&mut Seen::default());
    race_armed(&mut phones);
    let stalled = Instant::now();
    let others = [true, false, true];
    let mut host_states = Vec::new();
    while stalled.elapsed() < Duration::from_millis(300) {
        for (p, on) in phones.iter_mut().zip(others) {
            if on {
                let input = p.bot_drive();
                p.pump(input);
            }
        }
        let s = phones[0].migration_state();
        if host_states.last() != Some(&s) {
            host_states.push(s);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let input = phones[1].bot_drive();
    phones[1].pump(input);
    let after = phones[1].migration_state();
    println!(
        "APP STANDBY STALL stalled_ms={} standby_after={after:?} host_states={host_states:?}",
        stalled.elapsed().as_millis()
    );
    assert!(!phones[1].is_host(), "the stalled standby promoted");
    assert_ne!(
        after,
        MigrationState::Recovering,
        "the stalled standby promoted"
    );
}

// ─── the migration screens (7b) ─────────────────────────────────────────────

fn ui_screen() -> egui::Rect {
    egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(SIZE.0 as f32 / PPP, SIZE.1 as f32 / PPP),
    )
}

fn tap_rect(p: &mut Phone, r: egui::Rect) {
    p.tap(r.center().x, r.center().y);
}

#[test]
fn the_hosts_protect_button_and_pick_call_choose_backup() {
    use crate::ui::screens::migration;
    let _protect = crate::dev::force_show_protect(true);
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    r.steps(60);
    let screen = ui_screen();
    assert_eq!(
        r.host.slot().with(|s| s.migration_state()),
        Some(MigrationState::Idle)
    );
    // "Protect this room" opens the picker; nothing is asked yet.
    tap_rect(&mut r.host, migration::protect_button(screen));
    r.steps(5);
    assert_eq!(
        r.host.slot().with(|s| s.migration_state()),
        Some(MigrationState::Idle)
    );
    // The one seated guest is the one row; choosing them calls `choose_backup`
    // (this room has no network of its own, so it refuses, with its reason).
    tap_rect(&mut r.host, migration::pick_button(screen, 0));
    r.steps(5);
    let state = r.host.slot().with(|s| s.migration_state());
    assert!(
        matches!(&state, Some(MigrationState::Refused { reason }) if reason.contains("network")),
        "the pick reached choose_backup: {state:?}"
    );
    assert_eq!(r.host.top(), "lobby", "the picker closed on the pick");
}

/// A host, a backup phone behind the real lobby screen and another player
/// on real QUIC loopback: the host asks the backup, whose person answers on
/// the consent prompt. Returns the states once they settle.
fn consent_through_the_prompt(yes: bool) -> (MigrationState, MigrationState, MigrationState) {
    consent_from(yes, false)
}

/// `in_garage`: the backup's person is in the Garage over the lobby when asked.
fn consent_from(yes: bool, in_garage: bool) -> (MigrationState, MigrationState, MigrationState) {
    use crate::ui::screens::migration;
    let cfg = lan_cfg();
    let id = approved();
    let room = decented_net::host(
        &cfg,
        &id,
        RoomInfo {
            name: "Consent".into(),
            max_players: 8,
        },
    );
    let host = open_room(&cfg, id, room);
    let (addr, token, code) = dial_info(&host);
    let join = |name: &str, character: u8| {
        let id = approved();
        let link = QuicClient::new(decented_net::join_addr(&cfg, &id, addr, &token));
        OnlineSession::network_client(
            cfg.clone(),
            id,
            link,
            &code,
            Some((addr, token.clone())),
            name,
            character,
        )
    };
    let mut others = vec![host, join("Other", 2)];
    let mut backup = Phone::new(if yes { "consent-yes" } else { "consent-no" });
    backup.open_lobby(join("Backup", 1));
    let t = Instant::now();
    let drive = |others: &mut Vec<OnlineSession>,
                 backup: &mut Phone,
                 what: &str,
                 cond: &dyn Fn(&[OnlineSession], &Phone) -> bool| {
        while !cond(others, backup) {
            backup.frame();
            for p in others.iter_mut() {
                let input = p.bot_drive();
                p.pump(input);
            }
            assert!(t.elapsed() < Duration::from_secs(60), "timed out: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let backup_state = |b: &Phone| b.slot().with(|s| s.migration_state()).unwrap();
    drive(&mut others, &mut backup, "everyone seated", &|o, b| {
        o[0].authority().is_some_and(|a| a.human_count() == 3)
            && b.top() == "lobby"
            && b.slot().with(|s| s.me().lobby().is_some()) == Some(true)
    });
    let seat = others[0]
        .me()
        .lobby()
        .unwrap()
        .seats
        .iter()
        .find(|s| s.name == "Backup")
        .unwrap()
        .id;
    if in_garage {
        backup
            .engine
            .router
            .apply(Transition::Push(Box::new(GarageScreen::new())));
        drive(&mut others, &mut backup, "the Garage open", &|_, b| {
            b.top() == "garage"
        });
    }
    others[0].choose_backup(seat);
    drive(&mut others, &mut backup, "the backup is asked", &|_, b| {
        backup_state(b) == MigrationState::ConsentRequested
    });
    // Asked from the Garage, the lobby comes forward with the prompt.
    drive(&mut others, &mut backup, "the lobby in front", &|_, b| {
        b.top() == "lobby"
    });
    // Let the prompt draw, then answer on it.
    for _ in 0..10 {
        backup.frame();
    }
    let prompt = migration::prompt_layout(ui_screen());
    tap_rect(&mut backup, if yes { prompt.yes } else { prompt.no });
    if yes {
        drive(&mut others, &mut backup, "every phone armed", &|o, b| {
            is_armed(&backup_state(b))
                && is_armed(&o[0].migration_state())
                && is_armed(&o[1].migration_state())
        });
    } else {
        drive(&mut others, &mut backup, "the host is told no", &|o, b| {
            backup_state(b) != MigrationState::ConsentRequested
                && matches!(
                    o[0].migration_state(),
                    MigrationState::Refused { .. } | MigrationState::Unprotected { .. }
                )
        });
    }
    (
        others[0].migration_state(),
        backup_state(&backup),
        others[1].migration_state(),
    )
}

#[test]
fn yes_on_the_consent_prompt_arms_the_room() {
    let (host, backup, other) = consent_through_the_prompt(true);
    assert!(is_armed(&host) && is_armed(&backup) && is_armed(&other));
}

#[test]
fn no_thanks_on_the_consent_prompt_declines() {
    let (host, backup, other) = consent_through_the_prompt(false);
    assert!(!is_armed(&host) && !is_armed(&backup) && !is_armed(&other));
    assert_ne!(backup, MigrationState::ConsentRequested);
}

#[test]
fn a_backup_in_the_garage_is_brought_to_the_consent_prompt_and_arms() {
    let (host, backup, other) = consent_from(true, true);
    assert!(is_armed(&host) && is_armed(&backup) && is_armed(&other));
}

/// Every text a frame of `p` draws.
fn frame_texts(p: &mut Phone) -> Vec<String> {
    fn walk(shape: &egui::epaint::Shape, out: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
            egui::epaint::Shape::Text(t) => out.push(t.galley.text().to_string()),
            _ => {}
        }
    }
    let out = p.engine.frame(1.0 / 60.0, SIZE, PPP, egui::RawInput::default());
    let mut v = Vec::new();
    for cs in &out.egui.shapes {
        walk(&cs.shape, &mut v);
    }
    v
}

/// The test setting is off by default: the host's lobby (a room with seated
/// guests, nothing armed) has no "Protect this room" in the top right, and a
/// tap where it would be does nothing. With the setting on it is there.
#[test]
fn the_hosts_lobby_has_no_protect_button_until_the_test_setting_is_on() {
    use crate::ui::screens::migration;
    let screen = ui_screen();
    let mut r = Rig::new(1, 2);
    r.add_guest("Guest", 3);
    r.seat_everyone();
    r.steps(60);
    assert!(!crate::dev::show_protect(), "off by default");
    assert!(
        !frame_texts(&mut r.host).iter().any(|t| t.contains("Protect this room")),
        "no Protect button for a normal player"
    );
    tap_rect(&mut r.host, migration::protect_button(screen));
    r.steps(5);
    assert_eq!(r.host.slot().with(|s| s.migration_state()), Some(MigrationState::Idle));
    assert!(!protect_sheet_up(&r.host) && r.host.top() == "lobby", "nothing opened");
    {
        let _protect = crate::dev::force_show_protect(true);
        r.steps(2);
        assert!(
            frame_texts(&mut r.host).iter().any(|t| t.contains("Protect this room")),
            "with the test setting on the button is there"
        );
    }
}

fn protect_sheet_up(p: &Phone) -> bool {
    use crate::ui::screens::migration;
    let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new(migration::SHEET_ID));
    p.engine.egui.memory(|m| m.areas().visible_last_frame(&layer))
}

/// The room Host a game opens has an online code, and only a room on one
/// Wi-Fi can have a backup. Protect asks first; Not now changes nothing;
/// Protect reopens the room nearby-only (its bots and laps kept, the room
/// code, Copy and Share replaced by how to join) and opens the picker. A
/// phone that joins from Nearby is chosen, says yes, and the room is armed.
#[test]
fn protect_on_an_online_code_room_reopens_it_nearby_only_and_arms_it() {
    use crate::ui::screens::migration;
    let _protect = crate::dev::force_show_protect(true);
    let cfg = lan_cfg();
    let id = approved();
    let mut online = cfg.clone();
    online.relays = vec!["wss://relay.invalid".into()];
    let handle = decented_net::host_with_relay(
        &online,
        &id,
        RoomInfo {
            name: "Host's race".into(),
            max_players: 8,
        },
        Some(Box::new(decented_net::relay::FakeRelayHub::new().client())),
    );
    let mut room = open_room(&online, id, handle);
    assert!(room.set_laps(2));
    let mut host = Phone::new("protect-online");
    host.open_lobby(room);
    for _ in 0..60 {
        host.frame();
    }
    let screen = ui_screen();
    let kind = |p: &Phone| p.slot().with(|s| (s.has_online_code(), s.nearby_only())).unwrap();
    assert_eq!(kind(&host), (true, false), "the room has an online code");
    assert!(frame_texts(&mut host).iter().any(|t| t == "Copy code"));
    // Protect asks first.
    tap_rect(&mut host, migration::protect_button(screen));
    for _ in 0..10 {
        host.frame();
    }
    assert!(protect_sheet_up(&host), "Protect asks before the room changes");
    let sheet = migration::sheet_layout(screen);
    // Not now changes nothing.
    tap_rect(&mut host, sheet.no);
    host.frame();
    assert!(!protect_sheet_up(&host));
    assert_eq!(kind(&host), (true, false), "Not now keeps the room as it was");
    assert_eq!(host.slot().with(|s| s.migration_state()), Some(MigrationState::Idle));
    // Protect: the room reopens nearby-only.
    tap_rect(&mut host, migration::protect_button(screen));
    for _ in 0..10 {
        host.frame();
    }
    tap_rect(&mut host, sheet.yes);
    host.frame();
    assert_eq!(host.top(), "lobby");
    assert_eq!(kind(&host), (false, true), "the room is nearby-only now");
    let t = Instant::now();
    while !host.slot().with(|s| s.room_ready() && s.me().lobby().is_some()).unwrap_or(false) {
        host.frame();
        assert!(t.elapsed() < Duration::from_secs(10), "the nearby room never opened");
        std::thread::sleep(Duration::from_millis(2));
    }
    let (bots, laps) = host.slot().with(|s| s.me().lobby().map(|l| (l.bots, l.laps))).flatten().unwrap();
    assert_eq!((bots, laps), (2, 2), "the room kept its bots and laps");
    // A phone on the Wi-Fi joins the new room from Nearby.
    let (addr, token, code) = host.slot().with(|s| dial_info(s)).unwrap();
    let gid = approved();
    let link = QuicClient::new(decented_net::join_addr(&cfg, &gid, addr, &token));
    let mut backup = OnlineSession::network_client(cfg.clone(), gid, link, &code, Some((addr, token.clone())), "Backup", 1);
    let t = Instant::now();
    let drive = |host: &mut Phone, backup: &mut OnlineSession, what: &str, cond: &dyn Fn(&Phone, &OnlineSession) -> bool| {
        while !cond(host, backup) {
            host.frame();
            let input = backup.bot_drive();
            backup.pump(input);
            assert!(t.elapsed() < Duration::from_secs(60), "timed out: {what}; host {:?}, backup {:?}", host.slot().with(|s| s.migration_state()), backup.migration_state());
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    drive(&mut host, &mut backup, "the backup seated", &|h, b| {
        h.slot().with(|s| s.authority().is_some_and(|a| a.human_count() == 2)).unwrap_or(false) && b.me().lobby().is_some()
    });
    // The picker is open: the one row is the backup; Choose asks it.
    tap_rect(&mut host, migration::pick_button(screen, 0));
    drive(&mut host, &mut backup, "the backup is asked", &|_, b| b.migration_state() == MigrationState::ConsentRequested);
    backup.answer_consent(true);
    drive(&mut host, &mut backup, "the room armed", &|h, b| {
        is_armed(&b.migration_state()) && h.slot().with(|s| is_armed(&s.migration_state())).unwrap_or(false)
    });    // Back in the lobby, the room shows how to join instead of a code
    // nobody can use there.
    assert_eq!(host.top(), "lobby");
    let texts = frame_texts(&mut host);
    assert!(texts.iter().any(|t| t == migration::NEARBY_JOIN), "{texts:?}");
    for gone in ["ROOM CODE", "Copy code", "Share link"] {
        assert!(!texts.iter().any(|t| t == gone), "{gone} is still shown: {texts:?}");
    }
}
