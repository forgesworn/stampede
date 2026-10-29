//! Host migration in the app: the armed room's roles, owned by the online
//! slot's [`OnlineSession`] (see `docs/host-migration.md`, "App wiring").
//!
//! One [`Role`] at a time holds every migration object of this device: the
//! arming coordinator, the switch, the armed runtime (`LivePrimary` and
//! `PlayerHost` on the host, `RemotePlayer` on a client, `LiveStandby` beside
//! the standby device's own seat), the takeover and the successor. Every
//! mutation goes through the session: screens read [`OnlineSession::me`],
//! [`OnlineSession::authority`] and [`OnlineSession::migration_state`], and
//! change things only through [`OnlineSession::client_command`], the host
//! controls and the three arming actions. So no screen can bypass the commit
//! barrier, the journals or the frozen lobby.
//!
//! [`OnlineSession::pump`] drives the role every frame, whichever screen is
//! on top (the engine's catch-all pump included), in the documented order:
//! - host: authority and arming, then the switch, then the own seat,
//!   `PlayerHost`, `LivePrimary` (publish on `Committed`, certify on
//!   `Membership`), one stage, flush;
//! - standby device: `LiveStandby` (everything queued on the pair link is
//!   read and installed first, then promote once its watch expired or the
//!   original left), then its own seat; after promotion the takeover
//!   (`SurvivorLinks`, `OwnSeat`, barrier, `Resume`), then the successor;
//! - client: `ClientArming`, then `ClientSwitch`, then `RemotePlayer`
//!   (`watch_standby` → `recover` → resume).

use super::{OnlineSession, RoomStatus};
use decented_net::migration::{
    DeliveryJournal, Descriptor, Error, IntentJournal, Recovery, RecoveryBarrier, RecoveryPhase,
    SessionImage,
};
use decented_net::{Identity, Keys, NetConfig};
use race_core::{BotLevel, CustomTrack, Input, TrackId};
use race_session::migration::{
    self, ArmingState, ClientArming, ClientSwitch, HostArming, HostSwitch, LivePrimary,
    LiveStandby, LiveSuccessor, OriginalLink, OwnSeat, PlayerHost, PromoteRefused, PromotedStandby,
    RecoveringPlayer, RemotePlayer, RuntimeEvent, SurvivorLinks, SwitchState,
};
use race_session::proto::BattleSetup;
use race_session::{
    ClientNetEvent, ClientSession, ClientTransport, HostAuthority, PeerKey, QuicClient,
};
use std::sync::{Arc, Mutex};

/// How long after a takeover's `Resume` the state reads
/// [`MigrationState::Resumed`] before it settles on
/// [`MigrationState::UnprotectedAfterTakeover`] (presentation only; 7b may
/// change it).
pub const RESUMED_NOTICE_MS: u64 = 5000;

/// What the screens show about host migration on this device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MigrationState {
    /// Nothing armed and nothing in progress.
    #[default]
    Idle,
    /// Host: the chosen backup was asked; waiting for its person.
    Asking,
    /// This device was asked to be the backup: answer with
    /// [`OnlineSession::answer_consent`].
    ConsentRequested,
    /// Consent given: the lobby is held while the room arms and switches.
    Arming,
    /// The room plays protected. Host: `unprotected` names the players the
    /// switch left out (and why); a client's list is empty.
    Armed { unprotected: Vec<String> },
    /// This device (or the room) plays on without protection.
    Unprotected { reason: String },
    /// Arming never began or stopped before activation.
    Refused { reason: String },
    /// The armed room stopped committing.
    Paused { reason: String },
    /// The original is gone: this device is resuming on the backup.
    Recovering,
    /// Just resumed after a takeover (for [`RESUMED_NOTICE_MS`]).
    Resumed,
    /// Playing on after a takeover: no backup, no second takeover.
    UnprotectedAfterTakeover,
}

/// Shown when the backup is lost and the armed room pauses: the original's
/// runtime reports the soft pause `LivePrimary::backup_lost` (its standby
/// watch reads `Paused`; it clears by itself), or latched `Unauthorized`/
/// `Closed` (the pair link's keys or connection are gone).
pub const BACKUP_LOST: &str = "Backup lost — the room is paused";
/// Shown while the armed room's commits are late (the backup's receipts are
/// slow, or this phone stalled): the original's soft pause
/// `LivePrimary::behind`. Session time holds at 250 ms past the last commit
/// meanwhile, and the room plays on by itself once the commits catch up.
pub const BEHIND: &str = "Waiting for the backup — the room is paused";
/// Shown on an original whose standby took over while it still ran.
pub const SUPERSEDED: &str = "The backup took over the room — this phone no longer hosts it";
/// The documented end of a session whose successor stopped (a second host
/// failure is never taken over).
pub const SECOND_FAILURE: &str = "The new host stopped responding, so the game has ended.";

#[cfg(test)]
thread_local! {
    /// Test only: real time spent between a standby frame's clock sample and
    /// its `promote()` (a slow intake or restore, as on a phone).
    pub(super) static PROMOTE_DELAY_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Test only: each survivor's takeover clock correction at its cut, in
    /// order: (which survivor, correction in ms, whether it was accepted).
    pub(super) static CUTS: std::cell::RefCell<Vec<(&'static str, i64, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A survivor's takeover clock correction at its cut (refused over 250 ms
/// in either sign).
fn note_cut(who: &'static str, delta: i64, accepted: bool) {
    log::info!("online: takeover cut ({who}): clock correction {delta} ms, accepted {accepted}");
    #[cfg(test)]
    CUTS.with(|c| c.borrow_mut().push((who, delta, accepted)));
}

/// A paused armed room's reason, in words.
pub fn paused_reason(error: &Error) -> String {
    match error {
        Error::Unauthorized | Error::Closed => BACKUP_LOST.into(),
        Error::Invalid(why) if why.contains("skew") => {
            "The game fell too far behind — the room is paused".into()
        }
        other => format!("The room is paused ({other:?})"),
    }
}

/// Why this device's armed session ended, in words.
pub fn ended_reason(error: &Error) -> String {
    match error {
        Error::Invalid(why) if why.contains("successor stopped") => SECOND_FAILURE.into(),
        Error::Invalid(why) if why.contains("recovery deadline") => {
            "The takeover did not finish in time, so the game has ended.".into()
        }
        Error::Invalid(why) if why.contains("correction") => {
            "This phone's clock was too far out to resume, so the game has ended.".into()
        }
        Error::Closed => "Lost the connection to the host.".into(),
        other => format!("The game has ended ({other:?})."),
    }
}

/// The arming coordinator's state as the screens show it.
pub fn arming_state(state: &ArmingState, consent_requested: bool) -> MigrationState {
    match state {
        ArmingState::Idle => MigrationState::Idle,
        ArmingState::Asking if consent_requested => MigrationState::ConsentRequested,
        ArmingState::Asking => MigrationState::Asking,
        ArmingState::AwaitingConsent
        | ArmingState::Activating
        | ArmingState::Armed
        | ArmingState::Switching => MigrationState::Arming,
        ArmingState::Unprotected { reason } => MigrationState::Unprotected {
            reason: reason.clone(),
        },
        ArmingState::Refused { reason } => MigrationState::Refused {
            reason: reason.clone(),
        },
    }
}

/// What this device needs to take part in arming: its network config and
/// identity, and the room code it hosts or joined (a LAN joiner's re-dial
/// address is in its `ClientArming`).
pub(super) struct MigrationNet {
    pub cfg: NetConfig,
    pub identity: Identity,
    pub code: String,
    /// A joiner's own ordinary link, shared with its `ClientSession`, for the
    /// arming coordinator.
    pub client: Option<Arc<Mutex<QuicClient>>>,
}

/// A host lobby control in an armed room: run inside the next staged batch
/// (operations may run twice on fresh private authorities, so they carry
/// data only).
#[derive(Clone, Debug)]
pub(super) enum HostOp {
    StartRace,
    ToLobby,
    Track(TrackId),
    Custom(Box<CustomTrack>),
    Laps(u8),
    Bots(u8),
    BotLevel(BotLevel),
    Battle(Option<BattleSetup>),
    Kick(u8),
}

fn apply_ops(ops: &[HostOp], h: &mut HostAuthority, now: u64) {
    for op in ops {
        let _ = match op {
            HostOp::StartRace => h.start_race(now),
            HostOp::ToLobby => h.to_lobby(),
            HostOp::Track(t) => h.set_track(*t),
            HostOp::Custom(t) => h.set_custom_track(t).is_ok(),
            HostOp::Laps(n) => h.set_laps(*n),
            HostOp::Bots(n) => h.set_bots(*n),
            HostOp::BotLevel(l) => h.set_bot_level(*l),
            HostOp::Battle(b) => h.set_battle(*b),
            HostOp::Kick(seat) => h.kick(*seat),
        };
    }
}

/// The standby runtime and the descriptor it consented to.
pub(super) struct Standby {
    live: LiveStandby,
    descriptor: Descriptor,
}

pub(super) struct ClientSide {
    arming: ClientArming,
    standby: Option<Standby>,
}

pub(super) struct SwitchSide {
    switch: ClientSwitch,
    standby: Option<Standby>,
}

pub(super) struct RemoteSide {
    player: RemotePlayer,
    /// The standby device: its runtime beside its own seat.
    standby: Option<Standby>,
    /// The link to the original closed (session time); only the standby
    /// link, or the standby runtime, can carry on.
    link_lost_at: Option<u64>,
    /// Why this device is unprotected while it plays armed.
    unprotected: Option<String>,
    /// The standby device's player left the room: its `Leave` went out
    /// through its endpoint and its seat departs like any human, while the
    /// standby runtime keeps running in the background until the room ends
    /// ([`OnlineSession::in_background`]). Its route stays to build the
    /// `OwnSeat` if that seat is still a member at a takeover's C, seated or
    /// kept seatless by its `Leave` batch: its client (`Closed("left")`) and
    /// journals are what the takeover retains and seals for it.
    seat_left: bool,
}

/// The standby device whose own player route never attached: its re-dial
/// failed after the host pinned, while the pair link stayed up. Once armed
/// the standby role is fixed, so the runtime runs on without a player, and
/// that player is out of this match, as if departed. `seat` is its session
/// as it stood at the switch: the original parks that player and departs it
/// after its parked departure, and a takeover while it is still a member at
/// C retains it (with fresh journals: the route never had any) and departs
/// it in the successor's first batch.
pub(super) struct StandbySide {
    standby: Standby,
    seat: ClientSession,
}

/// The standby device's own player at a takeover.
enum OwnRoute {
    /// Its armed route, as it played (or left).
    Player(RemotePlayer),
    /// Its route never attached ([`StandbySide`]).
    Unattached(ClientSession),
}

/// Shown on the standby device whose own player could not rejoin.
const STANDBY_ONLY: &str =
    "This phone could not rejoin the armed room. It stays the room's backup.";

pub(super) struct HostRoom {
    live: LivePrimary,
    players: PlayerHost,
    /// The playing phone host's own seat, in process.
    own: Option<RemotePlayer>,
    ops: Vec<HostOp>,
    /// The armed initial (frozen) game time.
    base: u64,
    unprotected: Vec<String>,
    paused: Option<String>,
}

enum TakeoverStage {
    Driving(Recovery),
    Barrier(RecoveryBarrier),
}

/// Where the standby device's own seat is cut.
#[derive(Clone, Copy, Debug)]
enum OwnCut {
    /// The host time its last stepped frame recorded: a sync round that
    /// completed since does not move it.
    Host(u64),
    /// Its host-time estimate at this local time, through its clock as it
    /// stands at the cut.
    Local(u64),
}

pub(super) struct Takeover {
    links: SurvivorLinks,
    stage: TakeoverStage,
    /// The standby device's own seat; `None` for a seatless takeover (the
    /// seat was retired before C).
    own: Option<OwnSeat>,
    /// The own seat retired (its `Leave` drained): no cut, no `Resume`.
    retired: bool,
    /// The own seat's native id, when its route never attached: the
    /// successor's first batch departs it.
    depart_own: Option<u8>,
    own_frozen: OwnCut,
    resume_host: Option<race_session::QuicHost>,
    keys: Keys,
}

pub(super) struct SuccessorRoom {
    live: LiveSuccessor,
    /// C, the frozen time, and the session time the room resumed at.
    c: u64,
    resumed_local: u64,
    ops: Vec<HostOp>,
}

/// This device's part in host migration. See the module docs.
#[derive(Default)]
pub(super) enum Role {
    /// Ordinary play (also a host before `choose_backup`).
    #[default]
    Plain,
    ClientArming(Box<ClientSide>),
    ClientSwitch(Box<SwitchSide>),
    Remote(Box<RemoteSide>),
    Recovering(Box<RecoveringPlayer>),
    HostArming(Box<HostArming>),
    HostSwitch(Box<HostSwitch>),
    HostArmed(Box<HostRoom>),
    Takeover(Box<Takeover>),
    Successor(Box<SuccessorRoom>),
    /// The standby device without its own player ([`StandbySide`]).
    Standby(Box<StandbySide>),
    /// The armed session ended ([`OnlineSession::dead_reason`] says why).
    Ended,
}

impl Role {
    pub(super) fn client_side(arming: ClientArming) -> Role {
        Role::ClientArming(Box::new(ClientSide {
            arming,
            standby: None,
        }))
    }

    /// The armed standby device's runtime (hostile tests only).
    #[cfg(test)]
    pub(super) fn standby_mut(&mut self) -> Option<&mut LiveStandby> {
        match self {
            Role::Remote(r) => r.standby.as_mut().map(|s| &mut s.live),
            Role::Standby(s) => Some(&mut s.standby.live),
            Role::ClientSwitch(s) => s.standby.as_mut().map(|s| &mut s.live),
            _ => None,
        }
    }

    /// A client's switch in flight (hostile tests only).
    #[cfg(test)]
    pub(super) fn switch_mut(&mut self) -> Option<&mut ClientSwitch> {
        match self {
            Role::ClientSwitch(s) => Some(&mut s.switch),
            _ => None,
        }
    }

    /// The room's authority lives in this role (not in the plain host).
    pub(super) fn hosts(&self) -> bool {
        matches!(
            self,
            Role::HostSwitch(_) | Role::HostArmed(_) | Role::Takeover(_) | Role::Successor(_)
        )
    }

    /// The client session the screens read, when the role owns it.
    pub(super) fn client(&self) -> Option<&ClientSession> {
        match self {
            Role::Remote(r) => Some(r.player.client()),
            Role::Recovering(r) => Some(r.client()),
            Role::HostArmed(r) => r.own.as_ref().map(|o| o.client()),
            Role::Takeover(t) => t.own.as_ref().map(|o| o.client()),
            Role::Successor(s) => s.live.own_client(),
            _ => None,
        }
    }

    pub(super) fn authority(&self) -> Option<&HostAuthority> {
        match self {
            Role::HostArmed(r) => Some(r.live.authority()),
            Role::Successor(s) => Some(s.live.authority()),
            _ => None,
        }
    }

    /// Whether local UI commands wait for the next armed frame (the client
    /// lives in a journaled endpoint, or is in flight between two).
    pub(super) fn defers_commands(&self) -> bool {
        matches!(
            self,
            Role::ClientSwitch(_)
                | Role::Remote(_)
                | Role::Recovering(_)
                | Role::Takeover(_)
                | Role::Successor(_)
        ) || matches!(self, Role::HostArmed(r) if r.own.is_some())
    }

    /// Queue a host control (armed host or successor). `None`: not armed here.
    pub(super) fn host_op(&mut self, op: HostOp) -> Option<bool> {
        match self {
            Role::HostArmed(r) if r.paused.is_none() => {
                r.ops.push(op);
                Some(true)
            }
            Role::Successor(s) => {
                s.ops.push(op);
                Some(true)
            }
            Role::HostArmed(_) | Role::Takeover(_) | Role::HostSwitch(_) => Some(false),
            _ => None,
        }
    }

    /// Test only: the armed original shows the soft behind pause.
    #[cfg(test)]
    pub(super) fn host_behind(&self) -> bool {
        matches!(self, Role::HostArmed(r) if r.live.behind())
    }

    /// Test only: the armed original's last commit: its index and game time.
    #[cfg(test)]
    pub(super) fn host_committed(&self) -> Option<(u64, u64)> {
        match self {
            Role::HostArmed(r) => {
                let receipt = r.live.primary().receipt();
                Some((receipt.index, receipt.logical_ms))
            }
            _ => None,
        }
    }

    /// The room's status once the armed room owns its listener.
    pub(super) fn room_status(&self) -> Option<RoomStatus> {
        match self {
            Role::HostArmed(r) => Some(super::read_status(r.players.network())),
            _ => None,
        }
    }
}

/// A transport that carries nothing: the screens' stand-in client while the
/// real one is inside the switch (it is never dead, so no screen leaves).
struct NullLink;
impl ClientTransport for NullLink {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        None
    }
    fn send_reliable(&mut self, _: Vec<u8>) {}
    fn send_lossy(&mut self, _: Vec<u8>) {}
    fn rtt_ms(&self) -> Option<u32> {
        None
    }
    fn close(&mut self) {}
}

pub(super) fn placeholder() -> ClientSession {
    ClientSession::new(Box::new(NullLink), "Rider", 0)
}

/// The network peer of a seated human (no public accessor yet: read from the
/// authority's own serialized seats; follow-up: `HostAuthority::peer_of`).
pub(super) fn peer_of_seat(host: &HostAuthority, seat: u8) -> Option<PeerKey> {
    let v = serde_json::to_value(host).ok()?;
    let human = v
        .get("humans")?
        .as_array()?
        .iter()
        .find(|h| h.get("id").and_then(|i| i.as_u64()) == Some(u64::from(seat)))?;
    human
        .get("peer")?
        .as_u64()
        .and_then(|p| PeerKey::try_from(p).ok())
}

/// Drop migration objects (QUIC handles that may block on close) off the
/// UI thread.
fn drop_detached<T: Send + 'static>(value: T) {
    let _ = std::thread::Builder::new()
        .name("stampede-armed-drop".into())
        .spawn(move || drop(value));
}

impl OnlineSession {
    /// The session's game-facing time: the clock, held across arming and the
    /// switch (the armed room resumes at the frozen lobby's time), and
    /// jumping forward if the attempt falls back to the unarmed lobby.
    pub(super) fn session_now(&self) -> u64 {
        let now = self.clock.now_ms();
        self.hold_from.unwrap_or(now).saturating_sub(self.shift)
    }

    fn hold(&mut self) {
        if self.hold_from.is_none() {
            self.hold_from = Some(self.clock.now_ms());
        }
    }

    /// End the hold: `keep` (the armed room started) keeps time continuous;
    /// otherwise time catches up with the unarmed host's clock.
    fn release(&mut self, keep: bool) {
        if let Some(h) = self.hold_from.take() {
            if keep {
                self.shift += self.clock.now_ms().saturating_sub(h);
            }
        }
    }

    fn end(&mut self, reason: String) -> Role {
        log::warn!("online: armed session ended: {reason}");
        self.ended = Some(reason);
        Role::Ended
    }

    /// The commands queued by the screens, as one command for the next armed
    /// client frame.
    fn take_commands(&mut self) -> impl FnOnce(&mut ClientSession) {
        let commands = std::mem::take(&mut self.pending);
        move |c: &mut ClientSession| {
            for command in commands {
                command(c);
            }
        }
    }

    fn after_takeover(&self, now: u64) -> MigrationState {
        match self.resumed_at {
            Some(at) if now.saturating_sub(at) < RESUMED_NOTICE_MS => MigrationState::Resumed,
            _ => MigrationState::UnprotectedAfterTakeover,
        }
    }

    // ─── the three arming actions ───────────────────────────────────────

    /// Host: make the player in `seat` the backup (asks that device's
    /// person). A refusal shows as [`MigrationState::Refused`] with a reason.
    pub fn choose_backup(&mut self, seat: u8) {
        let refused = |reason: &str| MigrationState::Refused {
            reason: reason.into(),
        };
        if !matches!(self.role, Role::Plain) {
            self.mstate = refused("Arming is already in progress.");
            return;
        }
        let (Some(host), Some(link), Some(net)) =
            (self.host.as_mut(), self.link.clone(), self.mig.as_ref())
        else {
            self.mstate = refused("Arming needs a room opened on this phone's network.");
            return;
        };
        let Some(peer) = peer_of_seat(host, seat) else {
            self.mstate = refused("That player is not seated.");
            return;
        };
        let Some(arming) = link.with(|q| HostArming::start(host, q, &net.cfg, &net.identity, peer))
        else {
            self.mstate = refused("The room is not open.");
            return;
        };
        self.mstate = arming_state(arming.state(), false);
        if !arming.state().is_finished() {
            self.role = Role::HostArming(Box::new(arming));
        }
    }

    /// The person's answer when asked to be the backup.
    pub fn answer_consent(&mut self, yes: bool) {
        let link = self.mig.as_ref().and_then(|m| m.client.clone());
        if let (Role::ClientArming(side), Some(link)) = (&mut self.role, link) {
            if let Ok(mut q) = link.lock() {
                side.arming.answer(&mut q, yes);
            }
            self.mstate = arming_state(side.arming.state(), side.arming.consent_requested());
        }
    }

    /// Host: abandon arming before the room is armed (the lobby is released).
    /// A device asked to be the backup declines.
    pub fn cancel_arming(&mut self) {
        match &mut self.role {
            Role::HostArming(arming) => {
                if let (Some(host), Some(link)) = (self.host.as_mut(), self.link.clone()) {
                    link.with(|q| arming.cancel(host, q, "The host cancelled arming."));
                }
            }
            Role::ClientArming(side) if side.arming.consent_requested() => {
                self.answer_consent(false)
            }
            _ => {}
        }
    }

    // ─── the frame ──────────────────────────────────────────────────────

    /// Drive this device's role one frame at session time `now`.
    pub(super) fn drive(&mut self, input: Input, now: u64) {
        let wall = self.runner.now_ms();
        let role = std::mem::take(&mut self.role);
        self.role = match role {
            Role::Plain => {
                if let Some(h) = self.host.as_mut() {
                    h.update(now);
                }
                self.me.update(now, input);
                Role::Plain
            }
            Role::ClientArming(side) => self.drive_client_arming(side, input, now, wall),
            Role::ClientSwitch(side) => self.drive_client_switch(side, wall),
            Role::Remote(side) => self.drive_remote(side, input, now, wall),
            Role::Recovering(player) => self.drive_recovering(player, now, wall),
            Role::HostArming(arming) => self.drive_host_arming(arming, input, now, wall),
            Role::HostSwitch(switch) => self.drive_host_switch(switch, input, now, wall),
            Role::HostArmed(room) => self.drive_host_armed(room, input, now, wall),
            Role::Takeover(t) => self.drive_takeover(t, now, wall),
            Role::Successor(room) => self.drive_successor(room, input, now, wall),
            Role::Standby(side) => self.drive_standby(side, now),
            Role::Ended => Role::Ended,
        };
    }

    fn drive_client_arming(
        &mut self,
        mut side: Box<ClientSide>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Role {
        self.me.update(now, input);
        let Some(link) = self.mig.as_ref().and_then(|m| m.client.clone()) else {
            return Role::ClientArming(side);
        };
        if let Ok(mut q) = link.lock() {
            side.arming.poll(&mut self.me, &mut q, wall);
        }
        if let Some(sb) = side.standby.as_mut() {
            if let Err(e) = sb.live.poll() {
                log::warn!("online: standby fault while arming: {e}");
            }
        }
        let state = side.arming.state().clone();
        self.mstate = arming_state(&state, side.arming.consent_requested());
        match state {
            ArmingState::AwaitingConsent | ArmingState::Activating => self.hold(),
            ArmingState::Armed => {
                self.hold();
                if side.standby.is_none() {
                    if let (Some(boot), Some(descriptor), Some(net)) = (
                        side.arming.take_standby(),
                        side.arming.descriptor(),
                        self.mig.as_ref(),
                    ) {
                        match LiveStandby::new(
                            descriptor.clone(),
                            boot,
                            net.identity.clone(),
                            self.runner,
                        ) {
                            // Its pulse activity is derived from the
                            // checkpoint it installs; there is nothing to set.
                            Ok(live) => side.standby = Some(Standby { live, descriptor }),
                            Err(e) => {
                                log::warn!("online: the standby runtime could not start: {e}")
                            }
                        }
                    }
                }
            }
            ArmingState::Switching => {
                self.hold();
                let session = std::mem::replace(&mut self.me, placeholder());
                match side
                    .arming
                    .take_switch(session, side.standby.as_ref().map(|s| &s.live))
                {
                    Ok(switch) => {
                        return Role::ClientSwitch(Box::new(SwitchSide {
                            switch,
                            standby: side.standby.take(),
                        }))
                    }
                    Err(session) => self.me = session,
                }
            }
            ArmingState::Unprotected { .. } | ArmingState::Refused { .. } => {
                self.release(false);
                side.standby = None;
            }
            ArmingState::Idle | ArmingState::Asking => {}
        }
        Role::ClientArming(side)
    }

    fn drive_client_switch(&mut self, mut side: Box<SwitchSide>, wall: u64) -> Role {
        let _ = wall;
        if !side.switch.state().is_finished() {
            side.switch.poll();
        }
        if let Some(sb) = side.standby.as_mut() {
            if let Err(e) = sb.live.poll() {
                log::warn!("online: standby fault while switching: {e}");
            }
        }
        match side.switch.state().clone() {
            SwitchState::Switching => {
                self.mstate = MigrationState::Arming;
                Role::ClientSwitch(side)
            }
            SwitchState::Protected { .. } => {
                self.release(true);
                let Some(player) = side.switch.take_player() else {
                    return self.end("The switch finished without a route.".into());
                };
                self.mstate = MigrationState::Armed {
                    unprotected: Vec::new(),
                };
                Role::Remote(Box::new(RemoteSide {
                    player,
                    standby: side.standby.take(),
                    link_lost_at: None,
                    unprotected: None,
                    seat_left: false,
                }))
            }
            SwitchState::Unprotected { reason } => {
                self.mstate = MigrationState::Unprotected {
                    reason: reason.clone(),
                };
                if let Some(player) = side.switch.take_player() {
                    // It has its route but no standby link: it plays armed,
                    // unprotected.
                    self.release(true);
                    return Role::Remote(Box::new(RemoteSide {
                        player,
                        standby: side.standby.take(),
                        link_lost_at: None,
                        unprotected: Some(reason),
                        seat_left: false,
                    }));
                }
                // The standby device's re-dial failed. A pair link still live
                // on a fresh watch means the host pinned: a host whose switch
                // failed thawed and dropped its runtime at the same budget,
                // a grace second before this. The role stays, without a
                // player (its ordinary rejoin would be refused by the pinned
                // listener anyway). A live link on a stale watch is unknown,
                // not a dead host: the device holds here, neither promoting
                // nor dropping, until the link goes (`Left`, or its idle
                // timeout closes it) or the watch is fresh again. The
                // fallback rejoin stays in the switch meanwhile.
                if let Some(mut standby) = side.standby.take() {
                    let linked = standby.live.failure().is_none()
                        && matches!(standby.live.original(), OriginalLink::Live);
                    let expired = standby.live.expired();
                    if linked && expired == Ok(true) {
                        side.standby = Some(standby);
                        return Role::ClientSwitch(side);
                    }
                    let armed = linked && expired == Ok(false);
                    match side.switch.take_seat() {
                        Some(seat) if armed => {
                            log::warn!(
                                "online: the standby's own player is out; the standby runs on"
                            );
                            self.release(true);
                            // The fallback rejoin is not needed; its link closes.
                            drop(side.switch.take_client());
                            self.mstate = MigrationState::Unprotected {
                                reason: STANDBY_ONLY.into(),
                            };
                            return Role::Standby(Box::new(StandbySide { standby, seat }));
                        }
                        _ => drop(standby),
                    }
                }
                // The re-dial failed: a fresh session rejoins the ordinary
                // lobby (its link is the switch's own; no arming from here).
                self.release(false);
                if let Some(client) = side.switch.take_client() {
                    self.me = client;
                }
                if let Some(net) = self.mig.as_mut() {
                    net.client = None;
                }
                Role::Plain
            }
        }
    }

    fn drive_remote(
        &mut self,
        mut side: Box<RemoteSide>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Role {
        // The app read the last pump's events. A pump which runs no frame
        // (the link was lost, the player left) leaves none to read again.
        side.player.discard_view_events();
        // The standby device: its runtime first, and promotion once due.
        if let Some(sb) = side.standby.as_mut() {
            standby_intake(&mut sb.live);
            // A latched standby can never promote (`promote` would hand it
            // back every frame), so it stops here: this device is then an
            // ordinary unprotected client, and the ordinary loss path (the
            // session-lost timeout, or the background standby stopping)
            // applies. The latch can come from the intake or from the watch
            // read itself.
            let expired = sb.live.expired();
            if let Some(error) = sb.live.failure().cloned() {
                log::warn!("online: the standby latched a fault and stops: {error}");
                side.standby = None;
                side.unprotected = Some("This phone can no longer take over.".to_owned());
            } else if matches!(sb.live.original(), OriginalLink::Left) || expired == Ok(true) {
                log::info!(
                    "online: the standby promotes (original link {:?})",
                    sb.live.original()
                );
                let Standby { live, descriptor } = side.standby.take().expect("standby");
                #[cfg(test)]
                std::thread::sleep(std::time::Duration::from_millis(
                    PROMOTE_DELAY_MS.with(|d| d.get()),
                ));
                match live.promote() {
                    Ok(promoted) => {
                        return self.begin_takeover(
                            promoted,
                            descriptor,
                            OwnRoute::Player(side.player),
                            side.seat_left,
                            now,
                        )
                    }
                    Err(PromoteRefused {
                        standby: Some(back),
                        ..
                    }) => {
                        side.standby = Some(Standby {
                            live: *back,
                            descriptor,
                        })
                    }
                    Err(PromoteRefused {
                        error,
                        standby: None,
                    }) => {
                        log::warn!("online: the standby could not take over: {error:?}");
                        side.unprotected = Some("This phone can no longer take over.".to_owned());
                    }
                }
            }
        }
        if side.seat_left {
            // The player left: only the standby runtime matters now. The
            // route is framed (its errors ignored) until its link is gone,
            // so the `Leave` it journaled is delivered.
            if side.standby.is_none() {
                return self.end("The background standby stopped.".into());
            }
            if side.link_lost_at.is_none() && side.player.frame(now, wall, input).is_err() {
                side.link_lost_at = Some(now);
            }
            return Role::Remote(side);
        }
        if side.link_lost_at.is_none() {
            let result = if side.player.frozen_at().is_none() {
                let commands = self.take_commands();
                side.player.frame_with(now, wall, input, commands)
            } else {
                side.player.frame(now, wall, input)
            };
            if let Err(e) = result {
                if side.player.protected() || side.standby.is_some() {
                    // A closed original is expected before the watch fires:
                    // keep watching the standby link (or let the standby
                    // runtime promote).
                    side.link_lost_at = Some(now);
                } else if side.player.under_successor() {
                    return self.end(ended_reason(&e));
                } else {
                    return self.end(dead_link_reason(&e));
                }
            }
        }
        if side.player.protected() && side.unprotected.is_none() {
            match side.player.watch_standby(wall) {
                Ok(Some(promotion)) => {
                    log::info!("online: a promotion arrived; recovering");
                    self.mstate = MigrationState::Recovering;
                    let player = side.player;
                    return match player.recover(promotion, wall) {
                        Ok(recovering) => Role::Recovering(Box::new(recovering)),
                        Err(e) => self.end(ended_reason(&e)),
                    };
                }
                Ok(None) => {}
                Err(e) if side.link_lost_at.is_some() => return self.end(dead_link_reason(&e)),
                Err(_) => side.unprotected = Some(BACKUP_LOST.into()),
            }
        }
        if let Some(at) = side.link_lost_at {
            if side.standby.is_none() && now.saturating_sub(at) > migration::SESSION_LOST_AFTER_MS {
                return self.end("Lost the connection to the host.".into());
            }
        }
        self.mstate = if side.player.under_successor() {
            if side.player.frozen_at().is_some() {
                MigrationState::Paused {
                    reason: "Waiting for the new host".into(),
                }
            } else {
                self.after_takeover(now)
            }
        } else if side.player.frozen_at().is_some() || side.link_lost_at.is_some() {
            MigrationState::Paused {
                reason: "Waiting for the host".into(),
            }
        } else if let Some(reason) = &side.unprotected {
            MigrationState::Unprotected {
                reason: reason.clone(),
            }
        } else {
            MigrationState::Armed {
                unprotected: Vec::new(),
            }
        };
        Role::Remote(side)
    }

    fn drive_recovering(&mut self, mut player: Box<RecoveringPlayer>, now: u64, wall: u64) -> Role {
        self.mstate = MigrationState::Recovering;
        match player.step(now, wall) {
            Ok(frame) => {
                if let Some(delta) = frame.clock_delta {
                    note_cut("client", delta, true);
                }
            }
            Err(e) => {
                if player.clock_delta().is_none() {
                    // Any recovery failure before the cut is confirmed: the
                    // correction it would have had, with the error.
                    let delta = player.cut_delta();
                    log::warn!("online: recovery failed before its cut was confirmed: {e}");
                    note_cut("client", delta, false);
                }
                return self.end(ended_reason(&e));
            }
        }
        if !player.resumed() {
            return Role::Recovering(player);
        }
        match player.into_resumed() {
            Ok(player) => {
                self.resumed_at = Some(now);
                log::info!("online: resumed under the new host");
                self.mstate = MigrationState::Resumed;
                Role::Remote(Box::new(RemoteSide {
                    player,
                    standby: None,
                    link_lost_at: None,
                    unprotected: None,
                    seat_left: false,
                }))
            }
            Err(e) => self.end(ended_reason(&e)),
        }
    }

    /// The standby device without its own player: only its runtime runs,
    /// and it promotes as [`Self::drive_remote`]'s standby does.
    fn drive_standby(&mut self, mut side: Box<StandbySide>, now: u64) -> Role {
        standby_intake(&mut side.standby.live);
        let expired = side.standby.live.expired();
        if let Some(error) = side.standby.live.failure().cloned() {
            log::warn!("online: the standby latched a fault and stops: {error}");
            return self.end("The background standby stopped.".into());
        }
        if matches!(side.standby.live.original(), OriginalLink::Left) || expired == Ok(true) {
            log::info!(
                "online: the standby promotes without its player (original link {:?})",
                side.standby.live.original()
            );
            let StandbySide {
                standby: Standby { live, descriptor },
                seat,
            } = *side;
            match live.promote() {
                Ok(promoted) => {
                    return self.begin_takeover(
                        promoted,
                        descriptor,
                        OwnRoute::Unattached(seat),
                        false,
                        now,
                    )
                }
                Err(PromoteRefused {
                    standby: Some(back),
                    ..
                }) => {
                    side = Box::new(StandbySide {
                        standby: Standby {
                            live: *back,
                            descriptor,
                        },
                        seat,
                    })
                }
                Err(PromoteRefused {
                    error,
                    standby: None,
                }) => {
                    log::warn!("online: the standby could not take over: {error:?}");
                    return self.end("The background standby stopped.".into());
                }
            }
        }
        self.mstate = MigrationState::Unprotected {
            reason: STANDBY_ONLY.into(),
        };
        Role::Standby(side)
    }

    /// `seat_left`: the standby device's player left before the promotion.
    /// Whether it left or not, its seat is driven while it is still a member
    /// at C: seated (a journaled `Leave` drains as a departure) or kept
    /// seatless by its departure batch (the successor's first batch retires
    /// it). Only a seat retired before C takes over seatless.
    fn begin_takeover(
        &mut self,
        mut promoted: PromotedStandby,
        descriptor: Descriptor,
        player: OwnRoute,
        seat_left: bool,
        now: u64,
    ) -> Role {
        // Release the signed `Promote` to an original that is still live (a
        // promotion on an expired watch), so it stops publishing. The clock
        // is sampled now, after `promote()` took its own sample: the frame's
        // earlier one is refused as a backwards clock and nothing is sent.
        if let Err(e) = promoted.poll(self.runner.now_ms()) {
            log::warn!("online: the promoted standby could not tell the original: {e}");
        }
        self.mstate = MigrationState::Recovering;
        let client = match &player {
            OwnRoute::Player(player) => player.client(),
            OwnRoute::Unattached(seat) => seat,
        };
        let room = client
            .lobby()
            .and_then(|l| {
                l.seats
                    .iter()
                    .find(|s| s.host)
                    .map(|s| format!("{}'s race", s.name))
            })
            .unwrap_or_else(|| "Race".into());
        let (links, recovery) = match SurvivorLinks::open(promoted.into_link()) {
            Ok(x) => x,
            Err(e) => return self.end(ended_reason(&e)),
        };
        let own_frozen = match &player {
            // Its last stepped frame, as a remote survivor's cut reads it:
            // the seat's game stopped there, at the host time that frame
            // recorded, bounded to the commits it saw (`MAX_LEAD_MS`). This
            // frame's promotion runs before the seat's frame, so `now` can be
            // a long pump later.
            OwnRoute::Player(player) => match player.last_frame_host() {
                Some(host) => OwnCut::Host(host),
                None => OwnCut::Local(player.last_frame().unwrap_or(now)),
            },
            // A seat that never played armed saw no freeze: its frozen point
            // is where its host clock reads C, so its cut corrects nothing.
            OwnRoute::Unattached(seat) => {
                let c = recovery.promotion().final_receipt.logical_ms;
                OwnCut::Local(
                    seat.host_now(now)
                        .map_or(now, |host| (now + c).saturating_sub(host)),
                )
            }
        };
        // While the standby's player is still a member at C, seated or kept
        // seatless by its departure batch, its seat is local here (the
        // background standby kept its client and journals after `Leave`) and
        // must be driven: `SurvivorLinks::drive` refuses a takeover without
        // it. Only a seat retired before C takes over seatless.
        let root = &descriptor.root;
        let member_at_c = match SessionImage::decode(root, recovery.sequencer().image()) {
            Ok(image) => image.members.iter().any(|m| m.key == Some(root.standby)),
            Err(e) => return self.end(ended_reason(&e)),
        };
        let depart_own = match &player {
            OwnRoute::Unattached(seat) if member_at_c => seat.my_seat(),
            _ => None,
        };
        let own = if member_at_c {
            let parts = match player {
                OwnRoute::Player(player) => player.into_parts(),
                OwnRoute::Unattached(seat) => {
                    log::info!("online: takeover retaining the standby's unattached seat");
                    Ok((seat, DeliveryJournal::default(), IntentJournal::default()))
                }
            };
            match parts.and_then(|(client, delivery, intents)| {
                OwnSeat::new(
                    root,
                    recovery.sequencer().image(),
                    client,
                    delivery,
                    intents,
                )
            }) {
                Ok(own) => Some(own),
                Err(e) => return self.end(ended_reason(&e)),
            }
        } else {
            log::info!(
                "online: seatless takeover (the standby's seat retired before C, left={seat_left})"
            );
            // Its client and journals are not needed; the route closes.
            if let OwnRoute::Player(player) = player {
                let _ = player.into_parts();
            }
            None
        };
        let Some(net) = self.mig.as_ref() else {
            return self.end("No network for the takeover.".into());
        };
        let resume_host = match migration::start_resume_host(
            &net.cfg,
            &net.identity,
            &room,
            &net.code,
            descriptor,
            recovery.promotion().clone(),
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("online: the resume host could not open: {e}");
                None
            }
        };
        let keys = net.identity.keys().clone();
        self.code = Some(net.code.clone());
        log::info!("online: promoted to host of room {}", net.code);
        Role::Takeover(Box::new(Takeover {
            links,
            stage: TakeoverStage::Driving(recovery),
            own,
            retired: false,
            depart_own,
            own_frozen,
            resume_host,
            keys,
        }))
    }

    fn drive_takeover(&mut self, t: Box<Takeover>, now: u64, wall: u64) -> Role {
        self.mstate = MigrationState::Recovering;
        let Takeover {
            mut links,
            stage,
            mut own,
            mut retired,
            depart_own,
            own_frozen,
            resume_host,
            keys,
        } = *t;
        let stage = match stage {
            TakeoverStage::Driving(mut recovery) => {
                match links.drive(&mut recovery, own.as_mut(), &keys, now, wall) {
                    Ok(RecoveryPhase::Ready) => {
                        retired = own.as_ref().is_none_or(|o| o.retired(&recovery));
                        let mut barrier = match links.barrier(recovery, &keys, wall) {
                            Ok(b) => b,
                            Err(e) => return self.end(ended_reason(&e)),
                        };
                        if let Some(own) = own.as_mut().filter(|_| !retired) {
                            let c = barrier.promotion().final_receipt.logical_ms;
                            let (delta, result) = match own_frozen {
                                OwnCut::Host(host) => (
                                    (host as i64).saturating_sub(c as i64),
                                    own.ready_at_host(&mut barrier, host, now, wall),
                                ),
                                OwnCut::Local(at) => (
                                    own.client().migration_clock_delta(at, c),
                                    own.ready(&mut barrier, at, now, wall),
                                ),
                            };
                            note_cut("own seat", delta, result.is_ok());
                            if let Err(e) = result {
                                return self.end(ended_reason(&e));
                            }
                        }
                        TakeoverStage::Barrier(barrier)
                    }
                    Ok(_) => TakeoverStage::Driving(recovery),
                    Err(e) => return self.end(ended_reason(&e)),
                }
            }
            TakeoverStage::Barrier(mut barrier) => match links.drive_barrier(&mut barrier, wall) {
                Ok(true) => {
                    let actor = match links.resume(barrier, &keys, wall) {
                        Ok((actor, _resume)) => actor,
                        Err(e) => return self.end(ended_reason(&e)),
                    };
                    if let Some(own) = own.as_mut().filter(|_| !retired) {
                        if let Err(e) = own.resumed(now) {
                            return self.end(ended_reason(&e));
                        }
                    }
                    let live = match links.into_successor(
                        actor,
                        own.filter(|_| !retired),
                        resume_host,
                        keys,
                    ) {
                        Ok(live) => live,
                        Err(e) => return self.end(ended_reason(&e)),
                    };
                    let c = live.receipt().logical_ms;
                    self.resumed_at = Some(now);
                    log::info!("online: the successor resumed the room");
                    self.mstate = MigrationState::Resumed;
                    return Role::Successor(Box::new(SuccessorRoom {
                        live,
                        c,
                        resumed_local: now,
                        // An unattached own seat departs at once: this
                        // device's player is out of the match.
                        ops: depart_own
                            .filter(|_| !retired)
                            .map(HostOp::Kick)
                            .into_iter()
                            .collect(),
                    }));
                }
                Ok(false) => TakeoverStage::Barrier(barrier),
                Err(e) => return self.end(ended_reason(&e)),
            },
        };
        Role::Takeover(Box::new(Takeover {
            links,
            stage,
            own,
            retired,
            depart_own,
            own_frozen,
            resume_host,
            keys,
        }))
    }

    fn drive_successor(
        &mut self,
        mut room: Box<SuccessorRoom>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Role {
        if let Err(e) = room.live.poll(wall) {
            return self.end(ended_reason(&e));
        }
        if room.live.own_client().is_some() {
            let commands = self.take_commands();
            if let Err(e) = room.live.own_frame_with(now, input, commands) {
                log::warn!("online: own seat frame refused: {e}");
            }
        }
        // At least one batch per frame, each at most 50 ms of game time:
        // after a slow frame, stage again after each commit until caught up.
        let game = room.c + now.saturating_sub(room.resumed_local);
        for _ in 0..8 {
            let last = room.live.receipt().logical_ms;
            if last >= game {
                break;
            }
            let at = game.min(last + migration::MAX_BATCH_GAME_MS);
            let ops = std::mem::take(&mut room.ops);
            match room.live.stage(game, |h| {
                apply_ops(&ops, h, at);
                Ok(())
            }) {
                Ok(true) => {}
                Ok(false) => {
                    room.ops = ops;
                    break;
                }
                Err(e) => return self.end(ended_reason(&e)),
            }
            if let Err(e) = room.live.commit(wall) {
                return self.end(ended_reason(&e));
            }
        }
        room.live.flush(wall);
        self.mstate = self.after_takeover(now);
        Role::Successor(room)
    }

    fn drive_host_arming(
        &mut self,
        mut arming: Box<HostArming>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Role {
        let (Some(link), Some(_)) = (self.link.clone(), self.host.as_ref()) else {
            return Role::Plain;
        };
        let host = self.host.as_mut().expect("host");
        host.update(now);
        let state = link
            .with(|q| arming.poll(host, q, wall).clone())
            .unwrap_or(ArmingState::Idle);
        let frozen = host.migration_frozen();
        self.me.update(now, input);
        if frozen {
            self.hold();
        }
        self.mstate = arming_state(&state, false);
        match state {
            ArmingState::Armed => {
                let (Some(armed), Some(net)) = (arming.take_armed(), self.mig.as_ref()) else {
                    return self.end("Arming finished without an armed room.".into());
                };
                let identity = net.identity.clone();
                let host = self.host.as_mut().expect("host");
                let runner = self.runner;
                match link.with(|q| HostSwitch::start(host, q, armed, &identity, runner, wall)) {
                    Some(switch) => Role::HostSwitch(Box::new(switch)),
                    None => self.end("The room's transport is gone.".into()),
                }
            }
            ArmingState::Refused { .. } | ArmingState::Unprotected { .. } => {
                self.release(false);
                Role::Plain
            }
            _ => Role::HostArming(arming),
        }
    }

    fn drive_host_switch(
        &mut self,
        mut switch: Box<HostSwitch>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Role {
        let Some(link) = self.link.clone() else {
            return self.end("The room's transport is gone.".into());
        };
        if let Some(host) = self.host.as_mut() {
            host.update(now);
            link.with(|q| switch.poll(host, q, wall));
        }
        self.me.update(now, input);
        match switch.state().clone() {
            SwitchState::Switching => {
                self.mstate = MigrationState::Arming;
                Role::HostSwitch(switch)
            }
            SwitchState::Protected { unprotected } => {
                self.release(true);
                // The phone host's own seat joins in process before the
                // first armed commit.
                let keys = self.mig.as_ref().map(|n| n.identity.keys().clone());
                let own = keys.and_then(|keys| {
                    let client = std::mem::replace(&mut self.me, placeholder());
                    switch
                        .attach_own(client, keys)
                        .map_err(|e| log::warn!("online: the host's own seat was refused: {e}"))
                        .ok()
                });
                let Some((live, players)) = switch.take_room() else {
                    return self.end("The armed room was not handed over.".into());
                };
                let base = SessionImage::decode(
                    &switch.descriptor().root,
                    live.primary().committed_image(),
                )
                .map(|i| i.logical_ms)
                .unwrap_or(now);
                // The lobby authority is superseded (and stays frozen).
                if let Some(old) = self.host.take() {
                    drop(old);
                }
                let unprotected: Vec<String> = unprotected
                    .iter()
                    .map(|u| format!("{}: {}", u.name, u.reason))
                    .collect();
                self.mstate = MigrationState::Armed {
                    unprotected: unprotected.clone(),
                };
                log::info!("online: room armed at game time {base}; unprotected {unprotected:?}");
                Role::HostArmed(Box::new(HostRoom {
                    live,
                    players,
                    own,
                    ops: Vec::new(),
                    base,
                    unprotected,
                    paused: None,
                }))
            }
            SwitchState::Unprotected { reason } => {
                // Before the pin: the lobby is thawed and plays on unarmed.
                self.release(false);
                self.mstate = MigrationState::Unprotected { reason };
                Role::Plain
            }
        }
    }

    fn drive_host_armed(
        &mut self,
        mut room: Box<HostRoom>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Role {
        // Session time is paced to the commits first (the soft behind
        // pause): while they are late it holds at 250 ms past the last one,
        // and the shift keeps it continuous, so neither the own seat nor the
        // batches jump when it runs on.
        let (now, game) = if room.paused.is_none() {
            let raw = now.max(room.base);
            let game = room.live.pace(raw);
            self.shift += raw - game;
            (if game < raw { game } else { now }, game)
        } else {
            (now, now.max(room.base))
        };
        // The own seat first: its intents and views join this batch.
        if let Some(own) = room.own.as_mut() {
            let result = if own.frozen_at().is_none() {
                let commands = self.take_commands();
                own.frame_with(now, wall, input, commands)
            } else {
                own.frame(now, wall, input)
            };
            if let Err(e) = result {
                log::warn!("online: the host's own seat frame failed: {e}");
            }
        }
        if room.paused.is_none() {
            let HostRoom {
                live,
                players,
                ops,
                paused,
                ..
            } = &mut *room;
            if let Err(e) = players.poll(live, wall) {
                pause(paused, e);
            }
            // The driver probes every frame, before the poll that sends it
            // (`LivePrimary::probe`); the standby probes inside its own poll.
            if let Err(e) = live.probe() {
                pause(paused, e);
            }
            match live.poll() {
                Ok(events) => {
                    for event in events {
                        match event {
                            RuntimeEvent::Committed {
                                outputs,
                                selected_views,
                                ..
                            } => {
                                if let Err(e) =
                                    players.publish(live, &outputs, &selected_views, wall)
                                {
                                    pause(paused, e);
                                }
                            }
                            RuntimeEvent::Membership(_) => {
                                if let Err(e) = players.certify(live, wall) {
                                    pause(paused, e);
                                }
                            }
                            RuntimeEvent::Promoted(_) => {
                                // The standby took over: this original is superseded.
                                paused.get_or_insert_with(|| SUPERSEDED.into());
                            }
                        }
                    }
                }
                Err(e) => pause(paused, e),
            }
            if paused.is_none() && !live.has_pending() {
                let at =
                    game.min(live.primary().receipt().logical_ms + migration::MAX_BATCH_GAME_MS);
                let staged = std::mem::take(ops);
                match players.stage(live, game, |h| {
                    apply_ops(&staged, h, at);
                    Ok(())
                }) {
                    Ok(true) => {}
                    Ok(false) => *ops = staged,
                    Err(e) => {
                        *ops = staged;
                        pause(paused, e);
                    }
                }
            }
            if let Some(e) = live.failure().cloned() {
                pause(paused, e);
            }
            if let Some(why) = paused.as_ref() {
                log::warn!("online: the armed room paused: {why}");
            }
        }
        room.players.flush(wall);
        // A genuine fault latched in `paused` is the reason first. The soft
        // backup-lost and behind pauses are read afresh every frame and clear
        // by themselves once the standby answers again and the commits catch
        // up; the room keeps polling and staging meanwhile.
        self.mstate = match &room.paused {
            Some(reason) => MigrationState::Paused {
                reason: reason.clone(),
            },
            None if room.live.backup_lost() => MigrationState::Paused {
                reason: BACKUP_LOST.into(),
            },
            None if room.live.behind() => MigrationState::Paused {
                reason: BEHIND.into(),
            },
            None => MigrationState::Armed {
                unprotected: room.unprotected.clone(),
            },
        };
        Role::HostArmed(room)
    }

    /// Leave an armed role. A client's `Leave` is journaled and sent through
    /// its endpoint first (`ClientSession::leave` closes only the client's
    /// old, already detached transport, never a pair or standby link). The
    /// rest is dropped: an armed original that goes is taken over by the
    /// standby. The standby device's player leaving is different: its seat
    /// departs like any human, and the role stays, running the standby in
    /// the background ([`OnlineSession::in_background`]) until the room ends.
    /// A standby device that goes (its session dropped, or never pumped
    /// again) pauses the armed room as backup lost.
    pub(super) fn leave_role(&mut self) {
        let now = self.session_now();
        let wall = self.runner.now_ms();
        match std::mem::replace(&mut self.role, Role::Ended) {
            Role::Remote(side) if side.seat_left => self.role = Role::Remote(side),
            // Its player is already out: the standby runs on in the
            // background.
            Role::Standby(side) => self.role = Role::Standby(side),
            Role::Remote(mut side) => {
                let _ = side
                    .player
                    .frame_with(now, wall, Input::default(), |c| c.leave());
                if side.standby.is_some() {
                    // The standby device stays the standby in the background
                    // until the room ends; only its seat departs.
                    log::info!("online: the standby's player left; the standby runs on");
                    side.seat_left = true;
                    self.role = Role::Remote(side);
                } else {
                    drop_detached(side);
                }
            }
            Role::HostArmed(mut room) => {
                if let Some(own) = room.own.as_mut() {
                    let _ = own.frame_with(now, wall, Input::default(), |c| c.leave());
                }
                drop_detached(room);
            }
            Role::Successor(mut room) => {
                if room.live.own_client().is_some() {
                    let _ = room
                        .live
                        .own_frame_with(now, Input::default(), |c| c.leave());
                }
                drop_detached(room);
            }
            Role::ClientArming(side) => drop_detached(side),
            Role::ClientSwitch(side) => drop(side),
            Role::Recovering(player) => drop_detached(player),
            Role::Takeover(t) => drop_detached(t),
            Role::HostArming(arming) => drop_detached(arming),
            Role::HostSwitch(switch) => drop_detached(switch),
            Role::Plain | Role::Ended => {}
        }
    }
}

/// Bound on the standby's catch-up polls in one frame ([`standby_intake`]).
const STANDBY_INTAKE_POLLS: usize = 8;

/// Read and process everything queued on the standby's pair link before its
/// watch is looked at, in the same frame: each poll moves what the network
/// holds into the ingress and installs what is complete, and it runs again
/// while it still installs or frames stay queued (a poll takes at most
/// `MAX_BATCH` frames). A standby frame stall alone then cannot make a live
/// original look silent: what the original sent during the stall counts
/// before `expired()`/`promote()` sample the watch.
fn standby_intake(live: &mut LiveStandby) {
    for _ in 0..STANDBY_INTAKE_POLLS {
        match live.poll() {
            Ok(installed) if installed.is_empty() && live.queued_bytes() == 0 => break,
            Ok(_) => {}
            Err(e) => {
                log::warn!("online: standby fault: {e}");
                break;
            }
        }
    }
}

impl OnlineSession {
    /// The player has left, but this device still runs the room's standby
    /// in the background (or, after the host failed, its seatless takeover
    /// and successor, while any human remains). The app keeps pumping it with
    /// [`Self::pump_background`] until the room ends.
    pub fn in_background(&self) -> bool {
        self.left
            && match &self.role {
                Role::Remote(side) => side.seat_left && side.standby.is_some(),
                Role::Standby(_) | Role::Takeover(_) => true,
                Role::Successor(room) => room.live.authority().human_count() > 0,
                _ => false,
            }
    }

    /// Drive a background standby ([`Self::in_background`]) one frame; a
    /// no-op otherwise.
    pub fn pump_background(&mut self) {
        if !self.in_background() {
            return;
        }
        let now = self.session_now();
        self.drive(Input::default(), now);
    }

    /// Drop whatever role remains, off the UI thread.
    pub(super) fn drop_role(&mut self) {
        match std::mem::replace(&mut self.role, Role::Ended) {
            Role::Remote(side) => drop_detached(side),
            Role::Takeover(t) => drop_detached(t),
            Role::Successor(room) => drop_detached(room),
            Role::Recovering(player) => drop_detached(player),
            Role::ClientArming(side) => drop_detached(side),
            Role::HostArming(arming) => drop_detached(arming),
            Role::HostSwitch(switch) => drop_detached(switch),
            Role::HostArmed(room) => drop_detached(room),
            Role::ClientSwitch(side) => drop(side),
            Role::Standby(side) => drop_detached(side),
            Role::Plain | Role::Ended => {}
        }
    }
}

fn pause(paused: &mut Option<String>, error: Error) {
    paused.get_or_insert_with(|| paused_reason(&error));
}

fn dead_link_reason(error: &Error) -> String {
    match error {
        Error::Closed => "Lost the connection to the host.".into(),
        other => ended_reason(other),
    }
}
