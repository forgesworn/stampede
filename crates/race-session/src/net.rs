//! [`HostTransport`] / [`ClientTransport`] over `decented-net` (QUIC), plus the
//! three ways in: host a room (online code and Nearby beacon at once), join by
//! room code, join a room found on the LAN.
//!
//! ```ignore
//! let net = race_session::net_config(data_dir);          // app tag "stampede"
//! let id = race_session::load_identity(&net.data_dir)?;
//! let host = race_session::net::host_room(&net, &id, "Friday race"); // code via host.code()
//! let client = race_session::net::join_code(&net, &id, "ABCDEF");
//! let mut browser = race_session::net::browse_nearby(&net);            // browser.poll() -> Vec<LanRoom>
//! let client = race_session::net::join_nearby(&net, &id, &room);
//! ```

use std::path::Path;

pub use decented_net::{Admission, InviteId};
use decented_net::{
    Bytes, ClientEvent, ClientHandle, DisconnectReason, FailReason, HostEvent, HostHandle,
    Identity, JoinStage, LanBrowser, LanRoom, NetConfig, PathKind, PeerId, RoomInfo,
};

use crate::transport::{ClientNetEvent, ClientTransport, HostIdentity, HostNetEvent, HostTransport, LinkPath, PeerKey};

/// The app tag that namespaces Stampede's room codes, QUIC ALPN and LAN beacon.
pub const APP_TAG: &str = "stampede";

/// Network settings for Stampede with the data dir where the identity key lives.
pub fn net_config(data_dir: impl AsRef<Path>) -> NetConfig {
    NetConfig::new(data_dir.as_ref(), APP_TAG)
}

/// This install's network identity (created on first use, stored in `data_dir`).
pub fn load_identity(data_dir: impl AsRef<Path>) -> Result<Identity, String> {
    Identity::load_or_create(data_dir.as_ref())
}

/// Start hosting a room. The same room is reachable by its room code online
/// and shows up to Nearby browsers on the LAN. Read the code from
/// [`QuicHost::code`] once `is_ready()`.
pub fn host_room(cfg: &NetConfig, id: &Identity, room_name: &str) -> QuicHost {
    let room = RoomInfo { name: room_name.chars().take(24).collect(), max_players: crate::proto::MAX_SEATS as u8 };
    QuicHost::new(decented_net::host(cfg, id, room))
}

/// Join by room code (online, through the relays).
pub fn join_code(cfg: &NetConfig, id: &Identity, code: &str) -> QuicClient {
    QuicClient::new(decented_net::join(cfg, id, code))
}

/// Join a room found by [`browse_nearby`].
pub fn join_nearby(cfg: &NetConfig, id: &Identity, room: &LanRoom) -> QuicClient {
    QuicClient::new(decented_net::join_lan(cfg, id, room))
}

/// Join by invite (online, through the relays): the Keep's runtime public key
/// and the invite's secret are the two halves of an invite link (see
/// [`crate::parse_invite_link`]). Only a host whose key is `host_key` is joined.
pub fn join_invite(cfg: &NetConfig, id: &Identity, secret: &[u8; 32], host_key: [u8; 32]) -> QuicClient {
    QuicClient { invite: true, ..QuicClient::new(decented_net::join_invite(cfg, id, secret, host_key)) }
}

/// What a Keep that does not answer an invite join, or turns it away, means
/// to the player. Fray words those for a code join ("Code not found"); an
/// invite has no code, and a revoked invite gets no answer at all.
pub const INVITE_NO_ANSWER: &str = "The Keep didn't answer. The invite may have been revoked, or the Keep is offline.";
pub const INVITE_NOT_ADMITTED: &str = "The Keep didn't let us in. The invite may have been revoked.";

/// A failed join's text: the reason, then the stage in brackets.
pub fn join_failure_text(reason: &FailReason, stage: JoinStage, invite: bool) -> String {
    match reason {
        FailReason::NoAnswer if invite => format!("{INVITE_NO_ANSWER} [{stage:?}]"),
        FailReason::NotAdmitted if invite => format!("{INVITE_NOT_ADMITTED} [{stage:?}]"),
        _ => format!("{reason} [{stage:?}]"),
    }
}

/// Listen for Nearby rooms; `poll()` returns the current list.
pub fn browse_nearby(cfg: &NetConfig) -> LanBrowser {
    decented_net::lan_browse(cfg)
}

/// A joiner that passed the persona check, as an [`AdmissionGate`] sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinInfo {
    pub peer: PeerKey,
    /// Which admission token the joiner proved.
    pub admitted_by: Admission,
    /// The joiner's persona public key, from the attestation just verified.
    pub persona: [u8; 32],
    pub path: LinkPath,
}

/// Decides, once the persona check has passed, whether a joiner is let in
/// (`Err` carries why, for logs). A refused peer is disconnected (not banned: it may come back with
/// a valid invite) and never reaches the host as `Joined`. A host with no gate (the phone app) admits as before.
pub type AdmissionGate = Box<dyn FnMut(&JoinInfo) -> Result<(), String> + Send>;

/// A hosted room as a [`HostTransport`]. Room-level events (`Ready`, `Failed`)
/// are kept for the app to read via [`QuicHost::code`] / [`QuicHost::failure`].
pub struct QuicHost {
    handle: Option<HostHandle>,
    ready: bool,
    failure: Option<String>,
    approved: std::collections::HashMap<PeerKey, u64>,
    /// How each approved peer reached this host (Fray's join path).
    paths: std::collections::HashMap<PeerKey, LinkPath>,
    /// Which admission token each approved peer proved.
    admitted: std::collections::HashMap<PeerKey, Admission>,
    gate: Option<AdmissionGate>,
}

impl QuicHost {
    pub fn new(handle: HostHandle) -> Self {
        QuicHost {
            handle: Some(handle),
            ready: false,
            failure: None,
            approved: Default::default(),
            paths: Default::default(),
            admitted: Default::default(),
            gate: None,
        }
    }

    /// Put a gate in front of admission (see [`AdmissionGate`]). The Keep uses
    /// it to apply its invite rules; the app does not set one.
    pub fn set_admission_gate(&mut self, gate: AdmissionGate) {
        self.gate = Some(gate);
    }

    /// The connected, approved peers that came in by this invite.
    pub fn peers_admitted_by(&self, invite: InviteId) -> Vec<PeerKey> {
        let mut peers: Vec<PeerKey> = self
            .admitted
            .iter()
            .filter(|(_, by)| **by == Admission::Invite(invite))
            .map(|(&peer, _)| peer)
            .collect();
        peers.sort_unstable();
        peers
    }

    /// How an approved peer reached this host, while it is connected.
    pub fn peer_path(&self, peer: PeerKey) -> Option<LinkPath> {
        self.paths.get(&peer).copied()
    }

    /// The room code to show (valid once `is_ready`).
    pub fn code(&self) -> Option<&str> {
        self.handle.as_ref().map(|h| h.code())
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn handle(&self) -> Option<&HostHandle> {
        self.handle.as_ref()
    }

    /// The listener handle, for calls that need `&mut` (a Keep rotating the
    /// room's code with [`HostHandle::rotate_code`] while its players stay).
    pub fn handle_mut(&mut self) -> Option<&mut HostHandle> {
        self.handle.as_mut()
    }

    /// Move the listener out, leaving a closed shell here: the armed switch
    /// takes the room's listener to pin it, while the lobby authority's own
    /// transport still points at this one. The shell polls and sends nothing.
    pub fn take_listener(&mut self) -> QuicHost {
        std::mem::replace(
            self,
            QuicHost {
                handle: None,
                ready: false,
                failure: None,
                approved: Default::default(),
                paths: Default::default(),
                admitted: Default::default(),
                gate: None,
            },
        )
    }

    /// A currently admitted, live runtime key for migration setup. Never infer
    /// this from a seat name or a previously received raw attestation.
    pub fn migration_peer_key(&self, peer: PeerKey) -> decented_net::migration::Result<[u8; 32]> {
        use decented_net::migration::Error;
        let until = self.approved.get(&peer).ok_or(Error::Unauthorized)?;
        if *until <= unix_now() { return Err(Error::Unauthorized); }
        self.handle.as_ref().and_then(|h| h.peer_key(PeerId(peer)))
            .map(|key| key.to_bytes()).ok_or(Error::Unauthorized)
    }
    /// Attach framing only after the unchanged game admission check.
    pub fn migration_channel(&self, peer: PeerKey) -> decented_net::migration::Result<decented_net::migration::Channel> {
        self.migration_peer_key(peer)?;
        self.handle.as_ref().ok_or(decented_net::migration::Error::Closed)?
            .migration_channel(PeerId(peer)).map_err(|_| decented_net::migration::Error::Unauthorized)
    }
}

impl HostTransport for QuicHost {
    fn poll(&mut self) -> Option<HostNetEvent> {
        let h = self.handle.as_ref()?;
        let now = unix_now();
        if let Some(peer) = self.approved.iter().find_map(|(&peer, &until)| (until <= now).then_some(peer)) {
            self.approved.remove(&peer);
            self.paths.remove(&peer);
            self.admitted.remove(&peer);
            h.kick(PeerId(peer));
            return Some(HostNetEvent::Left { peer });
        }
        loop {
            match h.poll()? {
                HostEvent::Ready { .. } => self.ready = true,
                HostEvent::Failed { reason } => self.failure = Some(reason),
                HostEvent::PeerJoined { peer, path, pubkey, attestation, admitted_by, .. } => {
                    let Ok(info) = verify_persona(attestation.as_deref(), &pubkey, now) else {
                        h.kick(peer);
                        continue;
                    };
                    let path = match path {
                        PathKind::Lan => LinkPath::Lan,
                        PathKind::Direct => LinkPath::Direct,
                        PathKind::Virtual => LinkPath::Bluetooth,
                    };
                    if let Some(gate) = self.gate.as_mut() {
                        let join = JoinInfo { peer: peer.0, admitted_by, persona: info.persona.to_bytes(), path };
                        if let Err(why) = gate(&join) {
                            log::info!("[net] {peer} refused: {why}");
                            // Not a ban: the person may come back with the right invite.
                            h.disconnect(peer);
                            continue;
                        }
                    }
                    self.approved.insert(peer.0, info.valid_until);
                    self.admitted.insert(peer.0, admitted_by);
                    self.paths.insert(peer.0, path);
                    return Some(HostNetEvent::Joined { peer: peer.0, path });
                }
                HostEvent::PeerLeft { peer, reason } => {
                    self.paths.remove(&peer.0);
                    self.admitted.remove(&peer.0);
                    if self.approved.remove(&peer.0).is_none() { continue; }
                    log::info!("[net] {peer} left: {reason:?}");
                    return Some(HostNetEvent::Left { peer: peer.0 });
                }
                HostEvent::Reliable { peer, bytes } => {
                    if !self.approved.contains_key(&peer.0) { continue; }
                    return Some(HostNetEvent::Reliable { peer: peer.0, bytes: bytes.to_vec() })
                }
                HostEvent::Lossy { peer, bytes } => {
                    if !self.approved.contains_key(&peer.0) { continue; }
                    return Some(HostNetEvent::Lossy { peer: peer.0, bytes: bytes.to_vec() });
                }
            }
        }
    }

    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        if let Some(h) = &self.handle {
            h.send_reliable(PeerId(peer), Bytes::from(bytes));
        }
    }

    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        if let Some(h) = &self.handle {
            h.send_lossy(PeerId(peer), Bytes::from(bytes));
        }
    }

    fn kick(&mut self, peer: PeerKey) {
        if let Some(h) = &self.handle {
            h.kick(PeerId(peer));
        }
    }

    fn disconnect(&mut self, peer: PeerKey) {
        if let Some(h) = &self.handle {
            h.disconnect(PeerId(peer));
        }
    }

    fn confirm_timeout(&mut self, peer: PeerKey) {
        if let Some(h) = &self.handle {
            h.confirm_timeout(PeerId(peer));
        }
    }

    fn set_open(&mut self, open: bool) {
        if let Some(h) = &self.handle {
            if h.is_open() != open {
                h.set_open(open);
            }
        }
    }

    fn set_open_keeping_token(&mut self, open: bool) {
        if let Some(h) = &self.handle {
            if h.is_open() != open {
                h.set_open_keeping_token(open);
            }
        }
    }

    fn rtt_ms(&self, peer: PeerKey) -> Option<u32> {
        self.handle.as_ref()?.rtt_ms(PeerId(peer))
    }

    fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

/// A join (by code or LAN) as a [`ClientTransport`].
pub struct QuicClient {
    handle: Option<ClientHandle>,
    allowed_until: Option<u64>,
    path: Option<LinkPath>,
    failed: Option<(JoinStage, FailReason)>,
    /// Joined by invite, so a failure is worded for a Keep, not a code.
    invite: bool,
}

impl QuicClient {
    pub fn new(handle: ClientHandle) -> Self {
        QuicClient {
            handle: Some(handle),
            allowed_until: None,
            path: None,
            failed: None,
            invite: false,
        }
    }

    /// Where and why the join failed, once its `Failed` event was polled.
    pub fn failed_join(&self) -> Option<&(JoinStage, FailReason)> {
        self.failed.as_ref()
    }

    /// How this join reached the host, once connected.
    pub fn link_path(&self) -> Option<LinkPath> {
        self.path
    }

    /// Both sides of the current admitted connection, for explicit lobby
    /// consent. A stored host name or expired approval cannot authorize it.
    pub fn migration_keys(&self) -> decented_net::migration::Result<([u8; 32], [u8; 32])> {
        use decented_net::migration::Error;
        if self.allowed_until.is_none_or(|until| until <= unix_now()) {
            return Err(Error::Unauthorized);
        }
        let handle = self.handle.as_ref().ok_or(Error::Closed)?;
        Ok((handle.runtime_key(), handle.host_key().ok_or(Error::Unauthorized)?))
    }

    /// Attach framing to an admitted host without replacing its UTC expiry.
    pub fn migration_channel(&self, policy: &decented_net::migration::EndpointPolicy) -> decented_net::migration::Result<decented_net::migration::Channel> {
        self.migration_keys()?;
        self.handle.as_ref().ok_or(decented_net::migration::Error::Closed)?
            .migration_channel(policy).map_err(|_| decented_net::migration::Error::Unauthorized)
    }

    fn reject(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = std::thread::Builder::new().name("stampede-persona-close".into()).spawn(move || handle.shutdown());
        }
    }
}

/// Join an armed room as a human player on its term-0 Fray player route.
/// The original host's key (`root.host`) is pinned before Identify is sent.
pub fn join_player_route(
    cfg: &NetConfig,
    id: &Identity,
    addr: std::net::SocketAddr,
    token: &str,
    descriptor: &decented_net::migration::Descriptor,
) -> Result<QuicClient, String> {
    use decented_net::migration::LinkRole;
    session_join(
        cfg,
        id,
        addr,
        token,
        descriptor,
        LinkRole::Player,
        LinkRole::Original,
        descriptor.root.host,
    )
}

/// Prepare a roster player's `ControlPlayer` link to the armed standby's
/// passive listener. The standby's key (`root.standby`) is pinned before
/// Identify is sent.
pub fn join_standby_route(
    cfg: &NetConfig,
    id: &Identity,
    addr: std::net::SocketAddr,
    token: &str,
    descriptor: &decented_net::migration::Descriptor,
) -> Result<QuicClient, String> {
    use decented_net::migration::LinkRole;
    session_join(
        cfg,
        id,
        addr,
        token,
        descriptor,
        LinkRole::ControlPlayer,
        LinkRole::Standby,
        descriptor.root.standby,
    )
}

#[allow(clippy::too_many_arguments)]
fn session_join(
    cfg: &NetConfig,
    id: &Identity,
    addr: std::net::SocketAddr,
    token: &str,
    descriptor: &decented_net::migration::Descriptor,
    client: decented_net::migration::LinkRole,
    server: decented_net::migration::LinkRole,
    key: [u8; 32],
) -> Result<QuicClient, String> {
    let root = &descriptor.root;
    let binding = decented_net::migration::ConnectionBinding {
        session: root.session,
        descriptor: root.digest().map_err(|e| e.to_string())?,
        term: 0,
        client,
        server,
    };
    let key = decented_net::PublicKey::from_slice(&key).map_err(|e| e.to_string())?;
    decented_net::join_session_addr(cfg, id, addr, token, binding, key).map(QuicClient::new)
}

/// What one [`PinnedJoin::poll`] produced.
pub enum PinnedJoinPoll {
    /// Still connecting, or waiting to retry.
    Pending,
    /// The transport is up (its `Connected` event was consumed here).
    Connected(QuicClient),
    /// Refused after the transport was up, or out of attempts. Final.
    Failed(String),
}

/// A session-bound join to a pinned listener (a player route to the original,
/// or a prepared control link to the standby), retried with a bounded,
/// doubling backoff, as Twang's. A pinned Fray listener runs at most two
/// unauthenticated handshakes at once and refuses the rest at the QUIC layer,
/// so concurrent joins can be refused for concurrency alone. A failure while
/// still connecting, or an attempt that has not connected by its deadline, is
/// retried on a fresh connection. A refusal once the transport is up
/// (admission, host identity, a closed room) is final, and so is running out
/// of attempts.
///
/// [`PinnedJoin::until`] is the armed switch's variant: the client re-dials
/// as soon as it closed its ordinary link, while the original may not have
/// pinned its listener yet (it waits for every ordinary link to leave). An
/// unpinned listener refuses a session-bound join with `BAD_HELLO` (a session
/// binding it does not expect), which the joiner sees as a handshake-stage
/// `FailReason::Network`, not `RoomClosed`. So that refusal is retried too, as
/// is `RoomClosed` (`CLOSED`: the pin closing a handshake still pending on the
/// unpinned listener), and the join is bounded by one overall deadline
/// instead of an attempt count.
pub struct PinnedJoin {
    dial: Box<dyn FnMut() -> Result<QuicClient, String>>,
    link: Option<QuicClient>,
    attempt_started: std::time::Instant,
    attempt_deadline: std::time::Duration,
    retry_at: Option<std::time::Instant>,
    attempts: u32,
    failed: Option<String>,
    /// [`PinnedJoin::until`]: the overall deadline.
    until: Option<std::time::Instant>,
}

impl PinnedJoin {
    /// Attempts, the first one included ([`PinnedJoin::new`]).
    pub const MAX_ATTEMPTS: u32 = 6;
    /// The wait before the second attempt; it doubles for each later one
    /// (plus up to 15 ms of jitter), so six attempts back off for under 1 s.
    pub const FIRST_BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);
    /// The longest wait between two attempts of [`PinnedJoin::until`].
    pub const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_millis(400);

    /// `dial` opens one fresh connection (for example
    /// [`join_player_route`]). The first attempt starts on the first poll.
    /// `attempt_deadline` must be finite (a few seconds at most): it is the
    /// only bound on an attempt whose handshake hangs with no event, so the
    /// whole join is bounded by six deadlines plus the backoff.
    pub fn new(
        attempt_deadline: std::time::Duration,
        dial: impl FnMut() -> Result<QuicClient, String> + 'static,
    ) -> Self {
        PinnedJoin {
            dial: Box::new(dial),
            link: None,
            attempt_started: std::time::Instant::now(),
            attempt_deadline,
            retry_at: None,
            attempts: 0,
            failed: None,
            until: None,
        }
    }

    /// As [`PinnedJoin::new`], retried (a closed-room refusal and a transient
    /// handshake failure included) until `deadline`, with the backoff capped
    /// at [`PinnedJoin::MAX_BACKOFF`]. An identity, admission or version
    /// refusal stays final.
    pub fn until(
        deadline: std::time::Instant,
        attempt_deadline: std::time::Duration,
        dial: impl FnMut() -> Result<QuicClient, String> + 'static,
    ) -> Self {
        PinnedJoin {
            until: Some(deadline),
            ..Self::new(attempt_deadline, dial)
        }
    }

    /// Connections opened so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn poll(&mut self) -> PinnedJoinPoll {
        if let Some(why) = &self.failed {
            return PinnedJoinPoll::Failed(why.clone());
        }
        let now = std::time::Instant::now();
        if self.link.is_none() {
            if self.retry_at.is_some_and(|at| now < at) {
                return PinnedJoinPoll::Pending;
            }
            if self.until.is_some_and(|until| now >= until) {
                let why = format!(
                    "no connection before the deadline (after {} attempts)",
                    self.attempts
                );
                self.failed = Some(why.clone());
                return PinnedJoinPoll::Failed(why);
            }
            self.attempts += 1;
            self.attempt_started = now;
            self.retry_at = None;
            match (self.dial)() {
                Ok(link) => self.link = Some(link),
                Err(why) => return self.retry(why),
            }
        }
        let Some(link) = self.link.as_mut() else {
            return PinnedJoinPoll::Pending;
        };
        while let Some(event) = link.poll() {
            match event {
                ClientNetEvent::Connected { .. } => {
                    return self
                        .link
                        .take()
                        .map_or(PinnedJoinPoll::Pending, PinnedJoinPoll::Connected)
                }
                ClientNetEvent::Failed(why)
                    if Self::retryable(link.failed_join(), self.until.is_some()) =>
                {
                    return self.retry(why)
                }
                ClientNetEvent::Failed(why) | ClientNetEvent::Disconnected { reason: why, .. } => {
                    if let Some(mut link) = self.link.take() {
                        link.close();
                    }
                    self.failed = Some(why.clone());
                    return PinnedJoinPoll::Failed(why);
                }
                _ => {}
            }
        }
        if now.duration_since(self.attempt_started) >= self.attempt_deadline {
            return self.retry("no connection before the attempt deadline".into());
        }
        PinnedJoinPoll::Pending
    }

    /// While switching, a handshake-stage `Network` is what an unpinned
    /// listener's `BAD_HELLO` looks like, and `RoomClosed` what the pin gives
    /// a handshake it preempts: both mean "not pinned yet", so both retry.
    fn retryable(failed: Option<&(JoinStage, FailReason)>, switching: bool) -> bool {
        match failed {
            Some((JoinStage::Connecting, _)) => true,
            Some((JoinStage::Handshake, FailReason::RoomClosed | FailReason::Network(_))) => {
                switching
            }
            _ => false,
        }
    }

    fn retry(&mut self, why: String) -> PinnedJoinPoll {
        if let Some(mut link) = self.link.take() {
            link.close();
        }
        let now = std::time::Instant::now();
        let out = match self.until {
            Some(until) => now >= until,
            None => self.attempts >= Self::MAX_ATTEMPTS,
        };
        if out {
            let why = format!("{why} (after {} attempts)", self.attempts);
            self.failed = Some(why.clone());
            return PinnedJoinPoll::Failed(why);
        }
        let jitter = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos() as u64 % 16);
        let mut backoff = Self::FIRST_BACKOFF * (1u32 << (self.attempts - 1).min(8));
        if self.until.is_some() {
            backoff = backoff.min(Self::MAX_BACKOFF);
        }
        self.retry_at = Some(now + backoff + std::time::Duration::from_millis(jitter));
        PinnedJoinPoll::Pending
    }
}

fn stage_text(stage: JoinStage) -> &'static str {
    match stage {
        JoinStage::Preparing => "Getting ready...",
        JoinStage::FindingHost => "Finding host...",
        JoinStage::Connecting => "Connecting...",
        JoinStage::Handshake => "Joining...",
    }
}

impl ClientTransport for QuicClient {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        let h = self.handle.as_ref()?;
        if self.allowed_until.is_some_and(|until| until <= unix_now()) {
            self.reject();
            return Some(ClientNetEvent::Disconnected { host_left: false, reason: "The host's My Signet approval expired.".into() });
        }
        Some(match h.poll()? {
            ClientEvent::Progress { stage } => ClientNetEvent::Progress(stage_text(stage).into()),
            ClientEvent::Connected { path, host_name, host_pubkey, attestation, .. } => {
                self.path = Some(match path {
                    PathKind::Lan => LinkPath::Lan,
                    PathKind::Direct => LinkPath::Direct,
                    PathKind::Virtual => LinkPath::Bluetooth,
                });
                match check_persona(attestation.as_deref(), &host_pubkey, unix_now()) {
                    Ok(until) => self.allowed_until = Some(until),
                    Err(_) => {
                        self.reject();
                        return Some(ClientNetEvent::Failed("Couldn't confirm the host's My Signet approval.".into()));
                    }
                }
                ClientNetEvent::Connected {
                host: Some(HostIdentity {
                    name: host_name,
                    pubkey: host_pubkey.to_hex(),
                    npub: decented_net::npub_of(&host_pubkey),
                }),
            }},
            ClientEvent::Failed { stage, reason } => {
                log::info!("[net] join failed at {stage:?}: {reason:?}");
                let text = join_failure_text(&reason, stage, self.invite);
                self.failed = Some((stage, reason));
                ClientNetEvent::Failed(text)
            }
            ClientEvent::Reliable(b) => ClientNetEvent::Reliable(b.to_vec()),
            ClientEvent::Lossy(b) => ClientNetEvent::Lossy(b.to_vec()),
            ClientEvent::Disconnected { reason } => ClientNetEvent::Disconnected {
                host_left: reason == DisconnectReason::HostLeft,
                reason: format!("{reason:?}"),
            },
        })
    }

    fn send_reliable(&mut self, bytes: Vec<u8>) {
        if let Some(h) = &self.handle {
            h.send_reliable(Bytes::from(bytes));
        }
    }

    fn send_lossy(&mut self, bytes: Vec<u8>) {
        if let Some(h) = &self.handle {
            h.send_lossy(Bytes::from(bytes));
        }
    }

    fn rtt_ms(&self) -> Option<u32> {
        self.handle.as_ref()?.rtt_ms()
    }

    fn close(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_secs()).unwrap_or(0)
}

fn verify_persona(
    attestation: Option<&str>,
    runtime: &nostr::PublicKey,
    now: u64,
) -> Result<decented_signet::attestation::PersonaInfo, String> {
    let json = attestation.ok_or("Missing My Signet approval")?;
    decented_signet::verify_attestation(APP_TAG, json.as_bytes(), runtime, now).map_err(|e| e.to_string())
}

fn check_persona(attestation: Option<&str>, runtime: &nostr::PublicKey, now: u64) -> Result<u64, String> {
    verify_persona(attestation, runtime, now).map(|info| info.valid_until)
}

#[cfg(test)]
mod failure_text_tests {
    use super::*;

    #[test]
    fn an_invite_join_that_gets_no_answer_never_says_code_not_found() {
        let no_answer = join_failure_text(&FailReason::NoAnswer, JoinStage::FindingHost, true);
        assert_eq!(no_answer, format!("{INVITE_NO_ANSWER} [FindingHost]"));
        assert!(!no_answer.contains("Code"));
        let refused = join_failure_text(&FailReason::NotAdmitted, JoinStage::Connecting, true);
        assert_eq!(refused, format!("{INVITE_NOT_ADMITTED} [Connecting]"));
        assert!(!refused.contains("code"));
        // Other reasons, and every code join, keep Fray's words.
        assert_eq!(join_failure_text(&FailReason::RoomFull, JoinStage::Connecting, true), "Room full. [Connecting]");
        assert_eq!(
            join_failure_text(&FailReason::NoAnswer, JoinStage::FindingHost, false),
            "Code not found (check the code, or the host may have started). [FindingHost]"
        );
        assert_eq!(
            join_failure_text(&FailReason::NotAdmitted, JoinStage::Connecting, false),
            format!("{} [Connecting]", FailReason::NotAdmitted)
        );
    }
}

#[cfg(test)]
mod persona_tests {
    use super::*;
    use nostr::Keys;
    #[test]
    fn network_approval_is_signed_for_this_game_and_the_proven_runtime_key() {
        let persona = Keys::generate();
        let runtime = Keys::generate();
        let now = unix_now();
        let attest = |app, key: &nostr::PublicKey, at| {
            let event = decented_signet::attestation::unsigned_attestation(app, &persona.public_key(), key, at)
                .sign_with_keys(&persona).unwrap();
            serde_json::to_string(&event).unwrap()
        };
        let valid = attest(APP_TAG, &runtime.public_key(), now);
        assert!(check_persona(Some(&valid), &runtime.public_key(), now).is_ok());
        assert!(check_persona(None, &runtime.public_key(), now).is_err());
        assert!(check_persona(Some("{}"), &runtime.public_key(), now).is_err());
        assert!(check_persona(Some(&valid), &Keys::generate().public_key(), now).is_err());
        assert!(check_persona(Some(&attest("twangthang", &runtime.public_key(), now)), &runtime.public_key(), now).is_err());
        assert!(check_persona(Some(&valid), &runtime.public_key(), now + 100 * 86400).is_err());
        let tampered = valid.replace("stampede", "stampede-edited");
        assert!(check_persona(Some(&tampered), &runtime.public_key(), now).is_err());
    }
}
