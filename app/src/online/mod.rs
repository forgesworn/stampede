//! Online play for the app: the glue between the screens and `race-session`.
//!
//! One [`OnlineSession`] is a whole multiplayer game on this device: a joiner
//! (just a `ClientSession`) or a host (a `HostAuthority` plus the host's own
//! `ClientSession` on a zero-latency loopback). It lives in the app's one
//! [`OnlineSlot`] (`Services::online`, reached through `Ctx::online`), not in a
//! screen and never inside a [`Transition`](crate::ui::screens::Transition): the
//! lobby, race, results and join screens only borrow it, so a screen change
//! the router drops (or a screen pushed on top, like the Garage) can neither
//! lose the room nor stop it. The engine pumps it every frame no matter which
//! screen is on top, and from the Android event loop while the app is in the
//! background ([`OnlineSession::pump_auto`]). Leaving the room is explicit
//! ([`OnlineSlot::leave`]); a slot that outlives its screens is cleaned up by
//! the engine.
//!
//! No threads here (except the detached network shutdowns): `race-session` is
//! driven with an explicit clock, and `decented-net` runs its own runtime
//! behind the transports.
//!
//! Host migration (an armed room) lives in [`armed`]: the session's one role
//! owns every arming, switch, armed and takeover object, and the screens read
//! the active client through [`OnlineSession::me`] only.

mod armed;
pub use armed::{
    arming_state, ended_reason, paused_reason, MigrationState, BACKUP_LOST, BEHIND,
    RESUMED_NOTICE_MS, SECOND_FAILURE,
};
use armed::{HostOp, MigrationNet, Role};

use decented_net::LanRoom;
use race_core::{bot_input, BotLevel, CustomTrack, Input, RacePhase, TrackId};
use race_session::migration::{ClientArming, RunnerClock};
use race_session::{
    host_with_local_player, ClientNetEvent, ClientPhase, ClientSession, ClientTransport, HostAuthority, HostConfig, HostNetEvent, HostTransport, LoopClock,
    LoopbackConfig, LoopbackHub, PeerKey, QuicClient, QuicHost,
};
use std::cell::RefCell;
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// `stampede://join#CODE`, what Share link puts on the clipboard.
pub fn join_link(code: &str) -> String {
    format!("stampede://join#{code}")
}

/// The code as shown: `KWT QPM`.
pub fn spaced_code(code: &str) -> String {
    let mut out = String::new();
    for (i, c) in code.chars().enumerate() {
        if i == 3 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// A hosted room's QUIC transport, shared so the app can still read the code
/// and the reachability after the host authority took the transport.
#[derive(Clone)]
pub struct SharedHost(pub Arc<Mutex<QuicHost>>);

impl SharedHost {
    fn with<R>(&self, f: impl FnOnce(&mut QuicHost) -> R) -> Option<R> {
        self.0.lock().ok().map(|mut g| f(&mut g))
    }
}

impl HostTransport for SharedHost {
    fn poll(&mut self) -> Option<HostNetEvent> {
        self.with(|h| h.poll()).flatten()
    }
    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.with(|h| h.send_reliable(peer, bytes));
    }
    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.with(|h| h.send_lossy(peer, bytes));
    }
    fn kick(&mut self, peer: PeerKey) {
        self.with(|h| h.kick(peer));
    }
    fn disconnect(&mut self, peer: PeerKey) {
        self.with(|h| h.disconnect(peer));
    }
    fn confirm_timeout(&mut self, peer: PeerKey) {
        self.with(|h| h.confirm_timeout(peer));
    }
    fn set_open(&mut self, open: bool) {
        self.with(|h| h.set_open(open));
    }
    fn set_open_keeping_token(&mut self, open: bool) {
        self.with(|h| h.set_open_keeping_token(open));
    }
    fn rtt_ms(&self, peer: PeerKey) -> Option<u32> {
        self.with(|h| h.rtt_ms(peer)).flatten()
    }
    fn shutdown(&mut self) {
        // The QUIC host blocks up to a second flushing its close to the peers:
        // never on the UI thread.
        let link = self.clone();
        let spawned = std::thread::Builder::new().name("stampede-host-shutdown".into()).spawn(move || {
            link.with(|h| h.shutdown());
        });
        if spawned.is_err() {
            self.with(|h| h.shutdown());
        }
    }
}

/// A joiner's QUIC link shared between its `ClientSession` and the arming
/// coordinator (which reads and writes the same connection).
/// Its blocking close (up to 300 ms) and drop (100 ms) run on a short-lived
/// thread, so leaving never stalls a frame.
struct SharedClient(Option<Arc<Mutex<QuicClient>>>);

impl SharedClient {
    fn with<R>(&self, f: impl FnOnce(&mut QuicClient) -> R) -> Option<R> {
        self.0.as_ref()?.lock().ok().map(|mut q| f(&mut q))
    }
}

/// Drop (and with `close`, close) a shared link off the UI thread.
fn detach_link(link: Arc<Mutex<QuicClient>>, close: bool) {
    // If no thread can be made the closure drops with its link right here.
    let _ = std::thread::Builder::new()
        .name("stampede-client-close".into())
        .spawn(move || {
            if close {
                if let Ok(mut q) = link.lock() {
                    q.close();
                }
            }
            drop(link);
        });
}

impl ClientTransport for SharedClient {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        self.with(|q| q.poll()).flatten()
    }
    fn send_reliable(&mut self, bytes: Vec<u8>) {
        self.with(|q| q.send_reliable(bytes));
    }
    fn send_lossy(&mut self, bytes: Vec<u8>) {
        self.with(|q| q.send_lossy(bytes));
    }
    fn rtt_ms(&self) -> Option<u32> {
        self.with(|q| q.rtt_ms()).flatten()
    }
    fn close(&mut self) {
        if let Some(link) = self.0.take() {
            detach_link(link, true);
        }
    }
}

impl Drop for SharedClient {
    fn drop(&mut self) {
        if let Some(link) = self.0.take() {
            detach_link(link, false);
        }
    }
}

/// The room status lines from the room's listener.
fn read_status(q: &QuicHost) -> RoomStatus {
    let h = q.handle();
    RoomStatus {
        ready: q.is_ready(),
        failure: q.failure().map(str::to_string),
        relays: h.map(|h| h.relays_connected()).unwrap_or((0, 0)),
        lan: h.and_then(|h| h.local_addr()),
        online: h.and_then(|h| h.online_addr()),
    }
}

/// How the room is doing, for the lobby's status lines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoomStatus {
    /// The code is live.
    pub ready: bool,
    pub failure: Option<String>,
    /// Relays up / configured.
    pub relays: (usize, usize),
    /// Where Nearby players dial (LAN address and port).
    pub lan: Option<SocketAddr>,
    /// The address online players reach (STUN / port mapping), if learned.
    pub online: Option<SocketAddr>,
}

/// A game on this device. See the module docs.
pub struct OnlineSession {
    /// The local player (the host's own player when hosting).
    me: ClientSession,
    /// Hosting: the authority.
    host: Option<HostAuthority>,
    link: Option<SharedHost>,
    /// The room code to show (hosting). Samples set it directly.
    code: Option<String>,
    /// Other in-process players (screenshot samples and tests).
    pub extras: Vec<ClientSession>,
    clock: LoopClock,
    left: bool,
    /// Last room status read.
    status: RoomStatus,
    /// Which `extras` should be ready (samples).
    extras_ready: Vec<bool>,
    /// Dev/test hook: the bot AI drives your racer (`STAMPEDE_AUTOPILOT=1`, or the
    /// Android property; developer builds only, see `crate::dev`), so a two-device test needs no thumbs.
    pub autopilot: bool,
    /// `pump` ran since the engine last called [`OnlineSession::end_frame`]
    /// (the engine's catch-all pump skips a session its screen already drove).
    pumped: bool,
    /// The app went to the background at this clock time; a bot drives meanwhile.
    away_since: Option<u64>,
    /// The last away period: (ended at, lasted), clock ms.
    last_away: Option<(u64, u64)>,
    /// The first relay was seen up at this clock time (the code works from then on).
    relay_up_since: Option<u64>,
    /// Custom track last copied into the app's active-track metadata.
    active_custom: Option<u64>,
    /// Host migration: this device's role (see [`armed`]).
    role: Role,
    /// Network config and identity for arming (rooms opened on the network).
    mig: Option<MigrationNet>,
    /// The device's one runner clock for every migration object.
    runner: RunnerClock,
    /// Session time held across arming and the switch.
    shift: u64,
    hold_from: Option<u64>,
    /// Screen commands waiting for the next armed client frame.
    pending: Vec<Box<dyn FnOnce(&mut ClientSession)>>,
    mstate: MigrationState,
    /// The armed session ended, and why.
    ended: Option<String>,
    /// This session joined a Keep by invite (not a room by code).
    keep_join: bool,
    /// Session time this device resumed after a takeover.
    resumed_at: Option<u64>,
}

/// An away period this long counts as "you left the game" in messages.
const LONG_AWAY_MS: u64 = 2500;
/// ...and still explains a lost connection this long after the player came back.
const AWAY_EXPLAINS_MS: u64 = 20_000;
/// A relay must have been up this long before the room code is offered (its
/// subscription is live a moment after it connects).
const CODE_GRACE_MS: u64 = 1200;

/// What a player who lost the link while away is told.
pub const AWAY_TEXT: &str = "You were away too long, so the game carried on without you.";

impl OnlineSession {
    fn wrap(me: ClientSession, host: Option<HostAuthority>, link: Option<SharedHost>) -> OnlineSession {
        OnlineSession {
            me,
            host,
            link,
            code: None,
            extras: Vec::new(),
            clock: LoopClock::real(),
            left: false,
            status: RoomStatus::default(),
            extras_ready: Vec::new(),
            autopilot: false,
            pumped: false,
            away_since: None,
            last_away: None,
            relay_up_since: None,
            active_custom: None,
            role: Role::Plain,
            mig: None,
            runner: RunnerClock::start(),
            shift: 0,
            hold_from: None,
            pending: Vec::new(),
            mstate: MigrationState::Idle,
            ended: None,
            keep_join: false,
            resumed_at: None,
        }
    }

    fn dev_autopilot(data_dir: &Path) -> bool {
        let _ = data_dir;
        // Developer builds only; read once at start (`crate::dev`).
        crate::dev::autopilot()
    }

    /// Open a room: the code online and the beacon on the LAN at once.
    pub fn host(data_dir: &Path, name: &str, character: u8, cfg: HostConfig) -> Result<OnlineSession, String> {
        let net = crate::platform::net_config(data_dir);
        let id = crate::account::identity_for_online(&net.data_dir).map_err(|e| format!("Couldn't create this phone's network key: {e}"))?;
        let quic = race_session::host_room(&net, &id, &format!("{name}'s race"));
        let mut s = Self::network_host(net, id, quic, name, character, cfg);
        s.autopilot = Self::dev_autopilot(data_dir);
        Ok(s)
    }

    /// A room on `quic` (opened with `net` and `id`), with the host's own
    /// player. Arming is possible on a LAN-only room (no relays).
    pub fn network_host(
        net: decented_net::NetConfig,
        id: decented_net::Identity,
        quic: QuicHost,
        name: &str,
        character: u8,
        cfg: HostConfig,
    ) -> OnlineSession {
        let code = quic.code().map(str::to_string);
        let link = SharedHost(Arc::new(Mutex::new(quic)));
        let (mut host, me) =
            host_with_local_player(Some(Box::new(link.clone())), cfg, name, character);
        host.set_le_mans_start(true);
        log::info!("online: hosting room {}", code.as_deref().unwrap_or("?"));
        let mut s = OnlineSession::wrap(me, Some(host), Some(link));
        s.mig = Some(MigrationNet {
            cfg: net,
            identity: id,
            code: code.clone().unwrap_or_default(),
            client: None,
        });
        s.code = code;
        s
    }

    /// A joiner on `link`, the room `code` it dialled; `rejoin` is the
    /// original's LAN address and token (a Nearby join), which the armed
    /// switch re-dials. Every joiner runs the arming coordinator.
    pub fn network_client(
        net: decented_net::NetConfig,
        id: decented_net::Identity,
        link: QuicClient,
        code: &str,
        rejoin: Option<(SocketAddr, String)>,
        name: &str,
        character: u8,
    ) -> OnlineSession {
        let shared = Arc::new(Mutex::new(link));
        let me = ClientSession::new(
            Box::new(SharedClient(Some(shared.clone()))),
            name,
            character,
        );
        let mut s = OnlineSession::wrap(me, None, None);
        let mut arming = ClientArming::new(&net, &id, code);
        if let Some((addr, token)) = &rejoin {
            arming = arming.rejoin(*addr, token);
        }
        s.role = Role::client_side(arming);
        s.mig = Some(MigrationNet {
            cfg: net,
            identity: id,
            code: code.to_string(),
            client: Some(shared),
        });
        s
    }

    /// A host with no network at all (samples, tests, practice).
    pub fn offline_host(name: &str, character: u8, cfg: HostConfig, code: &str) -> OnlineSession {
        let (mut host, me) = host_with_local_player(None, cfg, name, character);
        host.set_le_mans_start(true);
        let mut s = OnlineSession::wrap(me, Some(host), None);
        s.code = Some(code.to_string());
        s
    }

    pub fn host_custom(data_dir: &Path, name: &str, character: u8, track: &CustomTrack) -> Result<OnlineSession, String> {
        race_core::custom::to_def(track).map_err(|e| e.to_string())?;
        let mut s = Self::host(data_dir, name, character, HostConfig::default())?;
        s.select_custom_track(track)?;
        Ok(s)
    }

    pub fn select_custom_track(&mut self, track: &CustomTrack) -> Result<(), String> {
        match self.role.host_op(HostOp::Custom(Box::new(track.clone()))) {
            Some(true) => Ok(()),
            Some(false) => Err("The room can't change track right now.".into()),
            None => self
                .host
                .as_mut()
                .ok_or("Only the host can choose a track.")?
                .set_custom_track(track),
        }
    }

    /// A session from parts on a clock of the caller's (tests drive virtual time).
    pub fn from_parts(me: ClientSession, host: Option<HostAuthority>, clock: LoopClock) -> OnlineSession {
        let mut s = OnlineSession::wrap(me, host, None);
        s.clock = clock;
        s
    }

    /// A host with in-process guests `(name, character, ready)` and no network:
    /// the lobby screenshots. Nothing starts by itself.
    pub fn sample_lobby(guests: &[(&str, u8, bool)], bots: u8, code: &str) -> OnlineSession {
        let hub = LoopbackHub::new(LoopClock::real(), 7);
        let cfg = HostConfig { track: TrackId::Dune, laps: 3, bots, bot_level: BotLevel::Normal, seed: 7, auto_start: false };
        let host = HostAuthority::new(Box::new(hub.host()), cfg);
        let me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Rider", 0);
        let mut s = OnlineSession::wrap(me, Some(host), None);
        s.code = Some(code.to_string());
        s.status = RoomStatus { ready: true, failure: None, relays: (3, 3), lan: Some(SocketAddr::from(([192, 168, 1, 24], 41234))), online: None };
        for (name, ch, ready) in guests {
            s.extras.push(ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), name, *ch));
            s.extras_ready.push(*ready);
        }
        s
    }

    /// Samples: ready the in-process guests once they are seated.
    pub fn ready_guests(&mut self) {
        for (x, want) in self.extras.iter_mut().zip(&self.extras_ready) {
            if *want && matches!(x.phase(), ClientPhase::Lobby) && x.my_lobby_seat().is_some_and(|m| !m.ready) {
                x.set_ready(true);
            }
        }
    }

    /// Join by room code.
    pub fn join_code(data_dir: &Path, code: &str, name: &str, character: u8) -> Result<OnlineSession, String> {
        let net = crate::platform::net_config(data_dir);
        let id = crate::account::identity_for_online(&net.data_dir).map_err(|e| format!("Couldn't create this phone's network key: {e}"))?;
        log::info!("online: joining room {code}");
        let link = race_session::join_code(&net, &id, code);
        let mut s = Self::network_client(net, id, link, code, None, name, character);
        s.autopilot = Self::dev_autopilot(data_dir);
        Ok(s)
    }

    /// Join a Keep by invite: the two halves of an invite link, same flow as a
    /// code join (this phone's signed-in identity, the player's relays), and the
    /// joiner refuses a host whose key is not `host_key`.
    ///
    /// There is no room code, so the armed-room (host migration) coordinator is
    /// given an empty one: no offer names it, and a Keep does not migrate. The
    /// secret is never logged.
    pub fn join_invite(data_dir: &Path, host_key: [u8; 32], secret: [u8; 32], name: &str, character: u8) -> Result<OnlineSession, String> {
        let net = crate::platform::net_config(data_dir);
        let id = crate::account::identity_for_online(&net.data_dir).map_err(|e| format!("Couldn't create this phone's network key: {e}"))?;
        log::info!("online: joining {}", crate::keeps::describe(&crate::keeps::JoinTarget::Invite { host_key, secret }));
        let link = race_session::join_invite(&net, &id, &secret, host_key);
        let mut s = Self::network_client(net, id, link, "", None, name, character);
        s.keep_join = true;
        s.autopilot = Self::dev_autopilot(data_dir);
        Ok(s)
    }

    /// Join a room heard on the LAN.
    pub fn join_nearby(data_dir: &Path, room: &LanRoom, name: &str, character: u8) -> Result<OnlineSession, String> {
        let net = crate::platform::net_config(data_dir);
        let id = crate::account::identity_for_online(&net.data_dir).map_err(|e| format!("Couldn't create this phone's network key: {e}"))?;
        log::info!("online: joining nearby room {} ({}) at {}", room.name, room.code, room.addr);
        let link = race_session::join_nearby(&net, &id, room);
        let mut s = Self::network_client(
            net,
            id,
            link,
            &room.code,
            Some((room.addr, room.token.clone())),
            name,
            character,
        );
        s.autopilot = Self::dev_autopilot(data_dir);
        Ok(s)
    }

    /// Milliseconds on this session's clock (held across arming and the
    /// armed switch, like the game time it drives).
    pub fn now_ms(&self) -> u64 {
        self.session_now()
    }

    /// Drive the session one frame: the host's world and network first, then
    /// every in-process player (yours gets `input`).
    pub fn pump(&mut self, input: Input) {
        if self.left {
            return;
        }
        self.pumped = true;
        let now = self.session_now();
        // The role first: plain play, or every armed object in its order.
        self.drive(input, now);
        let active = self.active_custom;
        let activated = self
            .me()
            .custom_track()
            .map(|t| t.hash())
            .filter(|hash| active != Some(*hash))
            .and_then(|hash| {
                let t = self.me().custom_track()?;
                crate::custom_tracks::activate(t).is_ok().then_some(hash)
            });
        if activated.is_some() {
            self.active_custom = activated;
        }
        // An armed host may have paced session time back this frame
        // (`LivePrimary::pace`); the extra players follow it.
        let now = now.min(self.session_now());
        for x in &mut self.extras {
            x.update(now, Input::default());
        }
        let st = match self.role.room_status() {
            Some(st) => Some(st),
            // After the pin the shared link is a closed shell: keep the last
            // status until the armed room reports again.
            None if self.role.hosts() => None,
            None => self.link.as_ref().and_then(|l| l.with(|q| read_status(q))),
        };
        {
            if let Some(st) = st {
                if st != self.status {
                    log::info!("online: room {:?} status {:?}", self.code, st);
                }
                if st.relays.0 > 0 {
                    self.relay_up_since.get_or_insert(self.clock.now_ms());
                } else {
                    self.relay_up_since = None;
                }
                self.status = st;
            }
        }
    }

    /// The engine's catch-all: drive the session if no screen did this frame
    /// (a screen pushed over the lobby, a background tick). A bot steers your
    /// racer while the app is away (and in the dev autopilot).
    pub fn pump_auto(&mut self) {
        if self.pumped || self.left {
            return;
        }
        let input = if self.away_since.is_some() || self.autopilot { self.bot_drive() } else { Input::default() };
        self.pump(input);
    }

    /// Close the frame: the next `pump_auto` will drive the session again.
    pub fn end_frame(&mut self) {
        self.pumped = false;
    }

    /// The bot AI's input for your racer (idle outside a race and on the grid
    /// before the start window).
    pub fn bot_drive(&self) -> Input {
        match self.me().view() {
            Some(v) if matches!(self.me().phase(), ClientPhase::Racing) => {
                if v.world.phase == RacePhase::Countdown && v.hud.countdown > 0.25 {
                    Input::default()
                } else {
                    bot_input(&v.world, v.you, BotLevel::Normal)
                }
            }
            _ => Input::default(),
        }
    }

    /// The app left (`true`) or came back to (`false`) the foreground. Coming
    /// back returns how long the player was away.
    pub fn set_away(&mut self, away: bool) -> Option<u64> {
        let now = self.clock.now_ms();
        match (away, self.away_since) {
            (true, None) => self.away_since = Some(now),
            (false, Some(t)) => {
                self.away_since = None;
                let len = now.saturating_sub(t);
                self.last_away = Some((now, len));
                return Some(len);
            }
            _ => {}
        }
        None
    }

    pub fn is_away(&self) -> bool {
        self.away_since.is_some()
    }

    fn away_explains_loss(&self) -> bool {
        let now = self.clock.now_ms();
        self.away_since.is_some_and(|t| now.saturating_sub(t) >= LONG_AWAY_MS) || self.last_away.is_some_and(|(end, len)| len >= LONG_AWAY_MS && now.saturating_sub(end) <= AWAY_EXPLAINS_MS)
    }

    /// The player's client as the screens read it: the ordinary session, or
    /// the one inside the armed route, recovery or successor seat. Read-only:
    /// changes go through [`Self::client_command`].
    pub fn me(&self) -> &ClientSession {
        self.role.client().unwrap_or(&self.me)
    }

    /// The room's authority (hosting): the lobby's, or the armed room's.
    pub fn authority(&self) -> Option<&HostAuthority> {
        self.role.authority().or(self.host.as_ref())
    }

    /// Where host migration stands on this device.
    pub fn migration_state(&self) -> MigrationState {
        self.mstate.clone()
    }

    /// Screen-driven local changes enter the runner through one boundary. In
    /// an armed room the command runs inside the next client frame's
    /// transaction (its intent is journaled before it is sent), and this
    /// returns `R::default()`.
    pub fn client_command<R: Default>(
        &mut self,
        command: impl FnOnce(&mut ClientSession) -> R + 'static,
    ) -> R {
        if self.role.defers_commands() {
            self.pending.push(Box::new(move |c| {
                let _ = command(c);
            }));
            return R::default();
        }
        command(&mut self.me)
    }

    /// Keep passes the host sent since the last call, each with the host's
    /// proven name when the link proved one. A Keep sends one after admitting a
    /// player by code (or on the local network); the engine saves it to My Keeps.
    pub fn take_keep_passes(&mut self) -> Vec<(race_session::KeepPass, Option<String>)> {
        let name = self.me.host_identity().map(|h| h.name.clone());
        self.me.take_keep_passes().into_iter().map(|p| (p, name.clone())).collect()
    }

    pub fn can_start(&self) -> bool {
        self.authority().is_none_or(|host| host.can_start())
    }

    /// A host control: queued for the next batch in an armed room, applied
    /// to the lobby authority otherwise (which refuses it while frozen).
    fn host_op(&mut self, op: HostOp, plain: impl FnOnce(&mut HostAuthority) -> bool) -> bool {
        match self.role.host_op(op) {
            Some(queued) => queued,
            None => self.host.as_mut().is_some_and(plain),
        }
    }

    pub fn start_race(&mut self) -> bool {
        let now = self.session_now();
        self.host_op(HostOp::StartRace, |host| host.start_race(now))
    }

    pub fn to_lobby(&mut self) {
        self.host_op(HostOp::ToLobby, |host| host.to_lobby());
    }

    pub fn set_track(&mut self, track: TrackId) -> bool {
        self.host_op(HostOp::Track(track), |host| host.set_track(track))
    }

    pub fn set_laps(&mut self, laps: u8) -> bool {
        self.host_op(HostOp::Laps(laps), |host| host.set_laps(laps))
    }

    pub fn set_bots(&mut self, bots: u8) -> bool {
        self.host_op(HostOp::Bots(bots), |host| host.set_bots(bots))
    }

    pub fn set_bot_level(&mut self, level: BotLevel) -> bool {
        self.host_op(HostOp::BotLevel(level), |host| host.set_bot_level(level))
    }

    pub fn set_battle(&mut self, battle: Option<race_session::proto::BattleSetup>) -> bool {
        self.host_op(HostOp::Battle(battle), |host| host.set_battle(battle))
    }

    pub fn kick(&mut self, seat: u8) {
        self.host_op(HostOp::Kick(seat), |host| host.kick(seat));
    }

    pub fn is_host(&self) -> bool {
        self.host.is_some() || self.role.hosts()
    }

    /// The room has an online code (relays): a backup can't protect it
    /// (arming is LAN-only).
    pub fn has_online_code(&self) -> bool {
        self.link
            .as_ref()
            .and_then(|l| l.with(|q| q.handle().map(|h| h.relays_connected().1 > 0)))
            .flatten()
            .unwrap_or(false)
    }

    /// A room this phone hosts on its Wi-Fi only, with no online code: its
    /// players join from Nearby.
    pub fn nearby_only(&self) -> bool {
        self.is_host() && self.mig.is_some() && !self.has_online_code()
    }

    /// This room again, on this Wi-Fi only: a new room with no relays (so no
    /// online code; only such a room can have a backup), the same host, race
    /// settings and bots. A live room can't leave its relays, so the caller
    /// puts the new room in the slot in place of this one, which closes this
    /// one: its players join the new one from Nearby.
    pub fn reopen_nearby(&self) -> Result<OnlineSession, String> {
        let (Some(mig), Some(_)) = (self.mig.as_ref(), self.host.as_ref()) else {
            return Err("Only the phone that opened the room can protect it.".into());
        };
        let lobby = self.me.lobby().ok_or("The room isn't open yet.")?;
        let me = lobby.seats.iter().find(|s| s.host).ok_or("The room isn't open yet.")?;
        let custom = lobby.custom.and(self.me.custom_track().cloned());
        let cfg = HostConfig {
            track: if custom.is_some() { HostConfig::default().track } else { lobby.track },
            laps: lobby.laps,
            bots: lobby.bots,
            bot_level: lobby.bot_level,
            ..HostConfig::default()
        };
        let net = mig.cfg.clone().lan_only();
        let quic = race_session::host_room(&net, &mig.identity, &format!("{}'s race", me.name));
        let mut s = Self::network_host(net, mig.identity.clone(), quic, &me.name, me.character, cfg);
        if let Some(track) = &custom {
            s.select_custom_track(track)?;
            s.set_laps(lobby.laps);
        }
        if lobby.battle.is_some() {
            s.set_battle(lobby.battle);
        }
        s.autopilot = self.autopilot;
        log::info!("online: the room reopened nearby-only to be protected ({} bots)", lobby.bots);
        Ok(s)
    }

    /// The room code (hosting).
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    pub fn status(&self) -> &RoomStatus {
        &self.status
    }

    /// Has the room link been opened (or is there no link to open)? Nearby
    /// players can find the room from here on.
    pub fn room_ready(&self) -> bool {
        self.link.is_none() || (self.status.ready && self.status.failure.is_none())
    }

    /// Is the room code safe to hand out? A joiner's offer is lost unless a
    /// relay already carries the room, so the code is offered only once a relay
    /// has been up for a moment.
    pub fn code_ready(&self) -> bool {
        self.link.is_none() || (self.room_ready() && self.relay_up_since.is_some_and(|t| self.clock.now_ms().saturating_sub(t) >= CODE_GRACE_MS))
    }

    /// The link is gone for good (the host left, we were removed, no network).
    /// A player who was away when it dropped is told so, not "lost connection".
    pub fn dead_reason(&self) -> Option<String> {
        if let Some(why) = &self.ended {
            return Some(why.clone());
        }
        if matches!(self.me().phase(), ClientPhase::Disconnected(_)) && self.away_explains_loss() {
            return Some(AWAY_TEXT.into());
        }
        let text = match self.me().phase() {
            // A Keep removes a player when their invite is revoked; say so plainly.
            ClientPhase::Closed(why) if self.keep_join && why == "kicked" => KEEP_REMOVED_TEXT.into(),
            phase => dead_text(phase)?,
        };
        // A client the switch left out was told why before its link closed.
        Some(match &self.mstate {
            MigrationState::Unprotected { reason } => format!("{text} ({reason})"),
            _ => text,
        })
    }

    /// Worst round trip to anyone (host: its slowest guest; guest: to the host).
    pub fn rtt_ms(&self) -> Option<u32> {
        match &self.host {
            Some(h) => {
                let lobby = self.me.lobby()?;
                lobby.seats.iter().filter(|s| !s.bot && !s.host).filter_map(|s| h.rtt_ms(s.id)).max()
            }
            None => self.me().rtt_ms(),
        }
    }

    /// Leave the room (the host tells everyone first). Idempotent. On the
    /// armed room's standby device only the seat leaves: the standby runs on
    /// in the background ([`Self::in_background`]).
    pub fn leave(&mut self) {
        if self.left {
            return;
        }
        self.left = true;
        for x in &mut self.extras {
            x.leave();
        }
        self.leave_role();
        self.me.leave();
        if let Some(link) = self.mig.as_mut().and_then(|m| m.client.take()) {
            detach_link(link, false);
        }
        if let Some(h) = self.host.as_mut() {
            h.shutdown();
        }
    }
}

/// The app's one online session, shared by the screens that show it (lobby,
/// race, results, join). Cloning the handle shares the slot. Dropping a handle
/// (a screen the router discarded) never touches the session; only
/// [`OnlineSlot::leave`], replacing it with [`OnlineSlot::put`], or the engine's
/// clean-up ends it.
///
/// A session whose player left while this device is the armed room's standby
/// is kept in the slot's background list instead
/// ([`OnlineSession::in_background`]): no screen shows it, and the engine
/// drives it with [`OnlineSlot::pump_background`] until the room ends.
#[derive(Clone, Default)]
pub struct OnlineSlot(
    Rc<RefCell<Option<OnlineSession>>>,
    Rc<RefCell<Vec<OnlineSession>>>,
);

impl OnlineSlot {
    pub fn new() -> OnlineSlot {
        OnlineSlot::default()
    }

    /// Make `s` the app's session; a session already there leaves its room.
    pub fn put(&self, s: OnlineSession) {
        let old = self.0.borrow_mut().replace(s);
        if let Some(old) = old {
            self.retire(old);
        }
    }

    /// A session that left: kept in the background while it still runs the
    /// room's standby, dropped otherwise.
    fn retire(&self, mut s: OnlineSession) {
        s.leave();
        if s.in_background() {
            if let Ok(mut background) = self.1.try_borrow_mut() {
                background.push(s);
            }
        }
    }

    /// Drive every background standby one frame and drop the ones whose room
    /// ended.
    pub fn pump_background(&self) {
        let Ok(mut background) = self.1.try_borrow_mut() else {
            return;
        };
        for s in background.iter_mut() {
            s.pump_background();
        }
        background.retain(|s| s.in_background());
    }

    /// Is a background standby running ([`Self::pump_background`])?
    pub fn has_background(&self) -> bool {
        self.1.try_borrow().map(|b| !b.is_empty()).unwrap_or(true)
    }

    pub fn is_some(&self) -> bool {
        self.0.try_borrow().map(|g| g.is_some()).unwrap_or(true)
    }

    /// Run `f` on the session, if there is one (and it is not already borrowed).
    pub fn with<R>(&self, f: impl FnOnce(&mut OnlineSession) -> R) -> Option<R> {
        let mut g = self.0.try_borrow_mut().ok()?;
        g.as_mut().map(f)
    }

    /// Leave the room and empty the slot. Idempotent. A standby keeps
    /// running in the background ([`Self::pump_background`]).
    pub fn leave(&self) {
        let s = self.0.try_borrow_mut().ok().and_then(|mut g| g.take());
        if let Some(s) = s {
            self.retire(s);
        }
    }

    /// Take the session out (tests).
    pub fn take(&self) -> Option<OnlineSession> {
        self.0.try_borrow_mut().ok().and_then(|mut g| g.take())
    }
}

impl Drop for OnlineSession {
    fn drop(&mut self) {
        self.leave();
        // A background standby dropped here (not kept by the slot) goes too.
        self.drop_role();
    }
}

/// What a player removed from a Keep (their invite revoked) is told.
pub const KEEP_REMOVED_TEXT: &str = "You were removed from this Keep.";

/// Why the player's link is gone, in words (None while it is up).
pub fn dead_text(phase: &ClientPhase) -> Option<String> {
    Some(match phase {
        ClientPhase::Failed(why) => friendly_failure(why),
        ClientPhase::Closed(why) => closed_text(why),
        ClientPhase::HostLeft => "The host left the game.".into(),
        ClientPhase::Disconnected(_) => "Lost the connection to the host.".into(),
        _ => return None,
    })
}

/// Strip the transport's " [Stage]" suffix from a failure text.
pub fn friendly_failure(why: &str) -> String {
    let base = why.split(" [").next().unwrap_or(why).trim();
    if base.is_empty() {
        "Couldn't join that game.".into()
    } else {
        base.to_string()
    }
}

/// What the host's close reasons mean to a player.
pub fn closed_text(why: &str) -> String {
    match why {
        "lobby full" => "Room full. Ask the host to make space, or start your own game.".into(),
        "kicked" => "The host removed you from the game.".into(),
        w if w.starts_with("version mismatch") => "The host is running a different version of the game. Both of you need the latest version.".into(),
        "race in progress" => "That game has already started. Wait for the next round.".into(),
        // The host chose these words (a Keep may be a stranger's): plain text, short.
        w => {
            let w: String = w.chars().filter(|c| !c.is_control() && !crate::keeps::is_invisible(*c)).take(60).collect();
            format!("The host closed the game ({w}).")
        }
    }
}

/// Ping quality for the dot: 0 good, 1 ok, 2 poor.
pub fn quality(rtt_ms: Option<u32>) -> u8 {
    match rtt_ms {
        Some(r) if r <= 70 => 0,
        Some(r) if r <= 140 => 1,
        Some(_) => 2,
        None => 0,
    }
}

#[cfg(test)]
pub(crate) mod tests;
