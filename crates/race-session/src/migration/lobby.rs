//! Arming over the room's own links (slices 6.5 and 6.6): from the host's
//! choice of standby to an armed room, with the two-phase consent, the pair
//! link, activation and the capability ACKs carried as real
//! `ToClient::Migration` / `ToHost::Migration` messages.
//!
//! - [`HostArming`] runs on the host. Phase 1 (`Ask` → `Willing`/`Decline`)
//!   reads the answer through the ordinary [`HostAuthority::update`] path.
//!   Phase 2 freezes the authority (its `update` then reads and changes
//!   nothing), prepares the offer and reads `ToHost::Migration` straight from
//!   the room's [`QuicHost`], beside the frozen authority. Every other event is
//!   held, in order, for the authority until the boundary ends.
//! - [`ClientArming`] runs on every joined client. The selected device asks
//!   its person once (phase 1), then consents to the offer automatically
//!   (phase 2). Every client signs its capability ACK automatically.
//!
//! Both are poll-driven: the app calls `poll` from its frame loop after the
//! session's own update, and reads [`ArmingState`].

use super::switch::{verify_bundle, ClientSwitch, VerifiedSwitch};
use super::wire::{chunk, MigrationEvent, Reassembler};
use super::{
    checkpoint, consent_lobby_offer, ArmedLobbyStandby, ConsentedLobby, LiveStandby,
    LobbyActivation, LobbyOffer, LobbyStandby, PrimaryBootstrap, StandbyBootstrap, SCHEMA,
};
use crate::host::{HostAuthority, HostPhase};
use crate::net::{QuicClient, QuicHost};
use crate::proto::{
    decode, encode, ToClient, ToClientMigration, ToHost, ToHostMigration, MAX_RELIABLE_LEN,
};
use crate::transport::{
    ClientNetEvent, ClientTransport, HostNetEvent, HostTransport, LinkPath, PeerKey,
    LOCAL_PEER_BASE,
};
use crate::ClientSession;
use decented_net::migration::{
    room_code_hash, CapabilityAck, ConnectionBinding, Descriptor, LinkRole, MAX_BATCH,
    MAX_JOURNAL_BYTES, MAX_JOURNAL_RECORDS,
};
use decented_net::{Identity, NetConfig};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};

/// How long phase 2 may wait for the selected device's consent and the pair
/// link, from the freeze. Activation then has Fray's own 5 s window.
pub const CONSENT_WINDOW_MS: u64 = 5000;
/// The activation window (Fray's `ActivationSource` clock).
pub const ACTIVATION_WINDOW_MS: u64 = 5000;
/// How long phase 1 may wait for the selected person's answer (host), or for
/// the person's answer and then the offer (client).
pub const ANSWER_WINDOW_MS: u64 = 60_000;
/// Per-peer budget at the frozen boundary: the events held for the authority
/// plus the migration parts the peer sends meanwhile. A peer over it is
/// disconnected (no ban); its `Left` is still held for the authority.
pub const HELD_PER_PEER_RECORDS: usize = 64;
pub const HELD_PER_PEER_BYTES: usize = 32 * 1024;

/// Where an arming attempt stands, on the host or on a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArmingState {
    /// Nothing in progress.
    Idle,
    /// Phase 1. Host: `Ask` sent, waiting for the person's answer. Client:
    /// the consent request is up; answer with [`ClientArming::answer`].
    Asking,
    /// Host: frozen, `Offer` sent, waiting for the signed consent. Client:
    /// said yes, waiting for the offer.
    AwaitingConsent,
    /// The pair link, activation and the capability ACKs.
    Activating,
    /// Host: activation finished with the standby's signed Armed. Standby:
    /// its bootstrap is ready. (Ordinary clients learn this in the next slice.)
    Armed,
    /// Client: the host's `Switch` arrived and the armed bundle verified;
    /// the ordinary link is closed. Take the switch with
    /// [`ClientArming::take_switch`] and drive it.
    Switching,
    /// Activation began and failed: the room plays unprotected.
    Unprotected { reason: String },
    /// Arming never began, or the consent or pair link failed before
    /// activation: the room stays unarmed.
    Refused { reason: String },
}

impl ArmingState {
    /// Armed, Unprotected or Refused: nothing more happens in this attempt.
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            ArmingState::Armed
                | ArmingState::Switching
                | ArmingState::Unprotected { .. }
                | ArmingState::Refused { .. }
        )
    }
}

enum Fail {
    Refused(String),
    Unprotected(String),
}

/// The activation window closed with these ACKs still due.
fn ack_timeout(pending: &BTreeMap<PeerKey, (u64, String)>) -> Fail {
    let missing: Vec<&str> = pending.values().map(|(_, n)| n.as_str()).collect();
    Fail::Unprotected(format!(
        "No capability ACK from {} within 5 s.",
        missing.join(", ")
    ))
}

pub(super) fn send_to_client(network: &mut QuicHost, peer: PeerKey, payload: &ToClientMigration) {
    match chunk(payload) {
        Ok(parts) => {
            for part in parts {
                network.send_reliable(peer, encode(&ToClient::Migration(part)));
            }
        }
        Err(e) => log::error!("[arming] setup message not sent: {e:?}"),
    }
}

fn send_to_host(network: &mut QuicClient, payload: &ToHostMigration) {
    match chunk(payload) {
        Ok(parts) => {
            for part in parts {
                network.send_reliable(encode(&ToHost::Migration(part)));
            }
        }
        Err(e) => log::error!("[arming] setup message not sent: {e:?}"),
    }
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn unhex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A private-network address: v0.1 arming is LAN-only, so the host never
/// dials a standby listener anywhere else.
pub(super) fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

/// Why the host may not arm this room now, if it may not.
fn arming_refusal(host: &HostAuthority, network: &QuicHost, selected: PeerKey) -> Option<String> {
    if host.phase != HostPhase::Lobby {
        return Some("Arming is only possible in the lobby.".into());
    }
    if host.migration_frozen() {
        return Some("Arming is already in progress.".into());
    }
    let Some(handle) = network.handle().filter(|_| network.is_ready()) else {
        return Some("The room is not open.".into());
    };
    if handle.relays_connected().1 > 0 {
        return Some("Arming is LAN-only: this room was opened with an online code.".into());
    }
    let remote = host
        .humans
        .iter()
        .filter(|h| h.peer < LOCAL_PEER_BASE)
        .collect::<Vec<_>>();
    if let Some(h) = remote
        .iter()
        .find(|h| network.peer_path(h.peer) == Some(LinkPath::Bluetooth))
    {
        return Some(format!(
            "Arming is refused while Bluetooth players are in the room ({}).",
            h.name
        ));
    }
    if let Some(h) = remote
        .iter()
        .find(|h| network.peer_path(h.peer) != Some(LinkPath::Lan))
    {
        return Some(format!(
            "Arming is LAN-only: {} did not join over the local network.",
            h.name
        ));
    }
    if !remote.iter().any(|h| h.peer == selected) {
        return Some("The chosen backup is not a player in this room.".into());
    }
    None
}

fn seated_name(host: &HostAuthority, peer: PeerKey) -> Option<String> {
    let id = host.by_peer.get(&peer)?;
    host.humans
        .iter()
        .find(|h| h.id == *id)
        .map(|h| h.name.clone())
}

/// What the frozen boundary holds for the authority, in arrival order.
struct Frozen {
    started_ms: u64,
    /// The authority checkpoint at the freeze; it must be unchanged at Armed.
    image: Vec<u8>,
    held: VecDeque<HostNetEvent>,
    held_bytes: usize,
    /// Records and bytes charged to each peer since the freeze: its held
    /// events and its migration parts.
    used: HashMap<PeerKey, (usize, usize)>,
    /// Peers disconnected for overflowing their budget: later events other
    /// than `Left` are dropped.
    closed: BTreeSet<PeerKey>,
    rx: HashMap<PeerKey, Reassembler>,
}

impl Frozen {
    fn new(started_ms: u64, image: Vec<u8>) -> Self {
        Self {
            started_ms,
            image,
            held: VecDeque::new(),
            held_bytes: 0,
            used: HashMap::new(),
            closed: BTreeSet::new(),
            rx: HashMap::new(),
        }
    }
    fn peer_of(event: &HostNetEvent) -> PeerKey {
        match event {
            HostNetEvent::Joined { peer, .. }
            | HostNetEvent::Left { peer }
            | HostNetEvent::Reliable { peer, .. }
            | HostNetEvent::Lossy { peer, .. } => *peer,
        }
    }
    fn size(event: &HostNetEvent) -> usize {
        match event {
            HostNetEvent::Reliable { bytes, .. } | HostNetEvent::Lossy { bytes, .. } => bytes.len(),
            _ => 0,
        }
    }

    /// Charge one record of `bytes` to `peer`'s budget. `Some(peer)` when
    /// this puts it over: its held events (but a `Left`) are dropped, it is
    /// closed, and the caller disconnects it.
    fn charge(&mut self, peer: PeerKey, bytes: usize) -> Option<PeerKey> {
        let used = self.used.entry(peer).or_default();
        used.0 += 1;
        used.1 += bytes;
        if used.0 <= HELD_PER_PEER_RECORDS && used.1 <= HELD_PER_PEER_BYTES {
            return None;
        }
        log::warn!("[arming] peer {peer} overflowed the frozen lobby; disconnecting");
        let mut freed = 0;
        self.held.retain(|e| {
            let keep = Self::peer_of(e) != peer || matches!(e, HostNetEvent::Left { .. });
            if !keep {
                freed += Self::size(e);
            }
            keep
        });
        self.held_bytes -= freed;
        self.used.remove(&peer);
        self.closed.insert(peer);
        Some(peer)
    }

    /// Hold `event` for the authority. A `Left` is always held, outside
    /// every bound, so a departure is never lost. `Ok(Some(peer))`: `peer`
    /// went over its budget; disconnect it. `Err`: the boundary as a whole
    /// is over its bounds. The event is still held, so nothing is lost when
    /// the caller ends arming and hands the held events back.
    fn hold(&mut self, event: HostNetEvent) -> Result<Option<PeerKey>, Fail> {
        let peer = Self::peer_of(&event);
        if matches!(event, HostNetEvent::Left { .. }) {
            self.closed.remove(&peer);
            self.used.remove(&peer);
            self.held.push_back(event);
            return Ok(None);
        }
        if self.closed.contains(&peer) {
            return Ok(None);
        }
        let size = Self::size(&event);
        if let Some(over) = self.charge(peer, size) {
            return Ok(Some(over));
        }
        let full =
            self.held.len() >= MAX_JOURNAL_RECORDS || self.held_bytes + size > MAX_JOURNAL_BYTES;
        self.held_bytes += size;
        self.held.push_back(event);
        if full {
            return Err(Fail::Unprotected(
                "Too much traffic while the lobby was held for arming.".into(),
            ));
        }
        Ok(None)
    }

    /// Someone joined while the lobby was held: the armed roster no longer
    /// describes the room.
    fn joined(&self) -> bool {
        self.held
            .iter()
            .any(|e| matches!(e, HostNetEvent::Joined { .. }))
    }
}

/// What the frozen boundary read in one poll.
#[derive(Default)]
struct FrozenRead {
    messages: Vec<(PeerKey, ToHostMigration)>,
    left: Vec<PeerKey>,
}

/// The finished host side, handed to the switch slice. The authority is still
/// frozen: its image equals the armed initial image, and `held` are the
/// events that arrived meanwhile, in order. [`thaw_lobby`] gives them to the
/// authority natively.
pub struct ArmedHost {
    pub bootstrap: PrimaryBootstrap,
    pub descriptor: Descriptor,
    /// The exact capability ACK bytes the host signed for its own seat (a
    /// playing host), kept for its own endpoint.
    pub own_ack: Option<Vec<u8>>,
    pub held: VecDeque<HostNetEvent>,
    /// The standby's listener, as its signed `Consent` named it: the pair
    /// listener is also the passive control listener the roster's
    /// `ControlPlayer` links reach (one `QuicHost`), so the switch forwards
    /// this address and token unchanged in `Standby`.
    pub standby: SocketAddr,
    pub standby_token: [u8; 32],
}

/// End the frozen boundary natively: `held` reach the authority at its next
/// [`HostAuthority::update`], in order and before anything newer.
pub fn thaw_lobby(host: &mut HostAuthority, held: impl IntoIterator<Item = HostNetEvent>) {
    host.migration_thaw(held);
}

struct Activating {
    activation: LobbyActivation,
    descriptor: Descriptor,
    started_ms: u64,
    /// Remote humans whose ACK is still due: peer -> (player, name).
    pending: BTreeMap<PeerKey, (u64, String)>,
    own_ack: Option<Vec<u8>>,
}

enum HostStage {
    Asking {
        willing: bool,
    },
    Consent {
        offer: LobbyOffer,
    },
    Pairing {
        consented: ConsentedLobby,
        link: QuicClient,
    },
    Activating(Box<Activating>),
    /// `finish` is running its last fallible step; for `fail` this is still
    /// activation (every client was sent `Setup`).
    Finishing,
    Armed(Box<ArmedHost>),
    Done,
}

/// The host side of arming, one attempt for one selected device.
///
/// Drive it every frame with [`HostArming::poll`]: in phase 1 after
/// `host.update(now)`, as usual; from phase 2 `host.update` is a no-op and
/// `poll` reads the room's transport itself. `network` is the room's own
/// [`QuicHost`] (the one inside the authority's transport). Never call `poll`
/// while that transport is borrowed elsewhere.
pub struct HostArming {
    cfg: NetConfig,
    identity: Identity,
    selected: PeerKey,
    name: String,
    state: ArmingState,
    stage: HostStage,
    frozen: Option<Frozen>,
    /// When phase 1 began (the first `poll`), for [`ANSWER_WINDOW_MS`].
    asked_ms: Option<u64>,
    /// The standby listener the consent named (address, LAN token).
    standby_at: Option<(SocketAddr, [u8; 32])>,
    ignored: u64,
}

impl HostArming {
    /// The explicit app action "make this player the backup". Refuses with a
    /// reason (and sends nothing) when the room is not a LAN-only lobby, has
    /// Bluetooth players, or `selected` is not a seated remote player.
    /// Otherwise sends `Ask` to `selected`.
    ///
    /// Attempt identity: the phase-1 messages carry no attempt id, so an
    /// answer is bound to this attempt by ordering. Migration messages the
    /// authority queued before this `Ask` are discarded (counted as ignored):
    /// only a `Willing` that arrives after this attempt's `Ask` counts.
    pub fn start(
        host: &mut HostAuthority,
        network: &mut QuicHost,
        cfg: &NetConfig,
        identity: &Identity,
        selected: PeerKey,
    ) -> Self {
        let name = seated_name(host, selected).unwrap_or_else(|| "The chosen backup".into());
        let mut arming = Self {
            cfg: cfg.clone(),
            identity: identity.clone(),
            selected,
            name,
            state: ArmingState::Asking,
            stage: HostStage::Asking { willing: false },
            frozen: None,
            asked_ms: None,
            standby_at: None,
            ignored: 0,
        };
        // Anything already queued predates this attempt's Ask.
        arming.ignored += host.take_migration_events().len() as u64;
        if let Some(reason) = arming_refusal(host, network, selected) {
            arming.state = ArmingState::Refused { reason };
            arming.stage = HostStage::Done;
        } else {
            send_to_client(network, selected, &ToClientMigration::Ask);
        }
        arming
    }

    pub fn state(&self) -> &ArmingState {
        &self.state
    }

    pub fn selected(&self) -> PeerKey {
        self.selected
    }

    /// Migration messages ignored so far: from a peer other than the one each
    /// step expects, malformed, or out of turn. None of them fails arming.
    pub fn ignored(&self) -> u64 {
        self.ignored
    }

    /// The armed result, once [`ArmingState::Armed`]. The authority stays
    /// frozen until the caller ends the boundary.
    pub fn take_armed(&mut self) -> Option<ArmedHost> {
        match std::mem::replace(&mut self.stage, HostStage::Done) {
            HostStage::Armed(armed) => {
                self.frozen = None;
                Some(*armed)
            }
            stage => {
                self.stage = stage;
                None
            }
        }
    }

    /// Abandon the attempt (the app's cancel): the room stays unarmed with
    /// `reason`, a frozen lobby is released, and every client that was sent
    /// `Setup` is sent `Withdrawn` (otherwise only the selected device).
    ///
    /// Once the attempt is finished this does nothing. In particular, after
    /// [`ArmingState::Armed`] the frozen boundary belongs to the caller:
    /// [`HostArming::take_armed`], then [`thaw_lobby`] with its `held` events
    /// (the next slice's switch, or the app abandoning the armed room).
    pub fn cancel(&mut self, host: &mut HostAuthority, network: &mut QuicHost, reason: &str) {
        if !self.state.is_finished() {
            self.fail(host, network, Fail::Refused(reason.into()));
        }
    }

    pub fn poll(
        &mut self,
        host: &mut HostAuthority,
        network: &mut QuicHost,
        now_ms: u64,
    ) -> &ArmingState {
        if let Err(fail) = self.step(host, network, now_ms) {
            self.fail(host, network, fail);
        }
        &self.state
    }

    /// End the attempt: release the frozen lobby and tell the clients. Once
    /// `Setup` went to every remote human (activation, including a failing
    /// `finish`), every one of them is told, so each resets its coordinator
    /// and a later attempt starts clean; before that only the selected device
    /// has any state.
    fn fail(&mut self, host: &mut HostAuthority, network: &mut QuicHost, fail: Fail) {
        let activating = matches!(self.stage, HostStage::Activating(_) | HostStage::Finishing);
        self.stage = HostStage::Done;
        if let Some(frozen) = self.frozen.take() {
            host.migration_thaw(frozen.held);
        }
        let peers: Vec<PeerKey> = if activating {
            host.humans
                .iter()
                .filter(|h| h.peer < LOCAL_PEER_BASE)
                .map(|h| h.peer)
                .collect()
        } else {
            vec![self.selected]
        };
        let (message, state) = match fail {
            Fail::Refused(reason) => {
                log::info!("[arming] refused: {reason}");
                let short = reason.chars().take(200).collect();
                (
                    ToClientMigration::Withdrawn { reason: short },
                    ArmingState::Refused { reason },
                )
            }
            Fail::Unprotected(reason) => {
                log::info!("[arming] unprotected: {reason}");
                let short = reason.chars().take(200).collect();
                (
                    ToClientMigration::Unprotected { reason: short },
                    ArmingState::Unprotected { reason },
                )
            }
        };
        for peer in peers {
            send_to_client(network, peer, &message);
        }
        self.state = state;
    }

    fn step(
        &mut self,
        host: &mut HostAuthority,
        network: &mut QuicHost,
        now_ms: u64,
    ) -> Result<(), Fail> {
        match &mut self.stage {
            HostStage::Asking { willing } => {
                if seated_name(host, self.selected).is_none() {
                    return Err(Fail::Refused(format!("{} left the room.", self.name)));
                }
                for (peer, event) in host.take_migration_events() {
                    match event {
                        MigrationEvent::Received(ToHostMigration::Willing)
                            if peer == self.selected =>
                        {
                            *willing = true
                        }
                        MigrationEvent::Received(ToHostMigration::Decline)
                            if peer == self.selected =>
                        {
                            return Err(Fail::Refused(format!(
                                "{} declined to be the backup.",
                                self.name
                            )));
                        }
                        _ => self.ignored += 1,
                    }
                }
                let asked = *self.asked_ms.get_or_insert(now_ms);
                if *willing && !host.lobby_dirty {
                    self.enter_phase_two(host, network, now_ms)?;
                } else if now_ms.saturating_sub(asked) >= ANSWER_WINDOW_MS {
                    return Err(Fail::Refused(format!(
                        "The backup did not answer within 60 s ({}).",
                        self.name
                    )));
                }
                Ok(())
            }
            HostStage::Consent { .. } => {
                if now_ms.saturating_sub(self.frozen_started()) >= CONSENT_WINDOW_MS {
                    return Err(Fail::Refused(format!(
                        "{} did not consent within 5 s.",
                        self.name
                    )));
                }
                let read = self.read_frozen(host, network)?;
                self.check_left(host, &read.left, false)?;
                for (peer, message) in read.messages {
                    if peer != self.selected {
                        log::warn!("[arming] {message:?} from unselected peer {peer} ignored");
                        self.ignored += 1;
                        continue;
                    }
                    match message {
                        ToHostMigration::Consent {
                            descriptor,
                            addr,
                            token,
                        } => {
                            return self.on_consent(host, network, peer, &descriptor, &addr, token)
                        }
                        ToHostMigration::Decline => {
                            return Err(Fail::Refused(format!(
                                "{}'s device could not prepare as the backup.",
                                self.name
                            )))
                        }
                        _ => self.ignored += 1,
                    }
                }
                Ok(())
            }
            HostStage::Pairing { .. } => {
                if now_ms.saturating_sub(self.frozen_started()) >= CONSENT_WINDOW_MS {
                    return Err(Fail::Refused(format!(
                        "The pair link to {} did not connect within 5 s.",
                        self.name
                    )));
                }
                let read = self.read_frozen(host, network)?;
                self.check_left(host, &read.left, false)?;
                self.ignored += read.messages.len() as u64;
                self.poll_pair_link(host, network, now_ms)
            }
            HostStage::Activating(_) => {
                let read = self.read_frozen(host, network)?;
                self.check_left(host, &read.left, true)?;
                let HostStage::Activating(act) = &mut self.stage else {
                    unreachable!()
                };
                for (peer, message) in read.messages {
                    let ToHostMigration::Ack { ack } = message else {
                        self.ignored += 1;
                        continue;
                    };
                    let Some((player, name)) = act.pending.get(&peer).cloned() else {
                        self.ignored += 1;
                        continue;
                    };
                    if now_ms.saturating_sub(act.started_ms) >= ACTIVATION_WINDOW_MS {
                        // Too late: a timeout naming who was late, never "invalid".
                        return Err(ack_timeout(&act.pending));
                    }
                    let root = &act.descriptor.root;
                    let ack = CapabilityAck::decode(root, &ack).map_err(|e| {
                        Fail::Unprotected(format!("{name}'s capability ACK was invalid ({e})."))
                    })?;
                    if ack.player != player {
                        return Err(Fail::Unprotected(format!(
                            "{name} sent a capability ACK for another player."
                        )));
                    }
                    act.activation.acknowledge(ack, now_ms).map_err(|e| {
                        Fail::Unprotected(format!("{name}'s capability ACK was invalid ({e})."))
                    })?;
                    act.pending.remove(&peer);
                }
                match act.activation.poll(now_ms) {
                    Ok(false) => Ok(()),
                    Ok(true) => self.finish(host, now_ms),
                    Err(e) => {
                        if now_ms.saturating_sub(act.started_ms) >= ACTIVATION_WINDOW_MS {
                            Err(if act.pending.is_empty() {
                                Fail::Unprotected(format!("{} did not arm within 5 s.", self.name))
                            } else {
                                ack_timeout(&act.pending)
                            })
                        } else {
                            Err(Fail::Unprotected(format!(
                                "The pair link to {} failed ({e}).",
                                self.name
                            )))
                        }
                    }
                }
            }
            HostStage::Armed(_) | HostStage::Finishing | HostStage::Done => Ok(()),
        }
    }

    fn frozen_started(&self) -> u64 {
        self.frozen.as_ref().map_or(0, |f| f.started_ms)
    }

    /// Phase 2: freeze, prepare and send the offer.
    fn enter_phase_two(
        &mut self,
        host: &mut HostAuthority,
        network: &mut QuicHost,
        now_ms: u64,
    ) -> Result<(), Fail> {
        if let Some(reason) = arming_refusal(host, network, self.selected) {
            return Err(Fail::Refused(reason));
        }
        let image = checkpoint(host)
            .map_err(|e| Fail::Refused(format!("The lobby could not be checkpointed ({e}).")))?
            .1;
        host.migration_freeze();
        self.frozen = Some(Frozen::new(now_ms, image));
        let offer = LobbyOffer::prepare(host, network, self.identity.keys(), self.selected)
            .map_err(|e| Fail::Refused(format!("The backup offer could not be prepared ({e}).")))?;
        let descriptor = offer
            .descriptor()
            .encode()
            .map_err(|e| Fail::Refused(format!("The backup offer could not be encoded ({e}).")))?;
        let code = network.code().unwrap_or_default().to_owned();
        send_to_client(
            network,
            self.selected,
            &ToClientMigration::Offer { code, descriptor },
        );
        self.stage = HostStage::Consent { offer };
        self.state = ArmingState::AwaitingConsent;
        Ok(())
    }

    /// Read the room's transport beside the frozen authority: seated players'
    /// `ToHost::Migration` parts come here, charged to the sender's budget;
    /// every other event is held.
    fn read_frozen(
        &mut self,
        host: &HostAuthority,
        network: &mut QuicHost,
    ) -> Result<FrozenRead, Fail> {
        let frozen = self.frozen.as_mut().expect("phase 2 is frozen");
        let mut read = FrozenRead::default();
        for _ in 0..MAX_BATCH {
            let Some(event) = network.poll() else { break };
            match &event {
                HostNetEvent::Reliable { peer, bytes }
                    if bytes.len() <= MAX_RELIABLE_LEN && seated_name(host, *peer).is_some() =>
                {
                    if let Some(ToHost::Migration(part)) = decode::<ToHost>(bytes) {
                        if frozen.closed.contains(peer) {
                            continue;
                        }
                        if let Some(over) = frozen.charge(*peer, bytes.len()) {
                            frozen.rx.remove(&over);
                            network.disconnect(over);
                            continue;
                        }
                        match frozen
                            .rx
                            .entry(*peer)
                            .or_default()
                            .push::<ToHostMigration>(part)
                        {
                            Ok(Some(message)) => read.messages.push((*peer, message)),
                            Ok(None) => {}
                            Err(e) => {
                                log::debug!("[arming] record from {peer} dropped: {e:?}");
                                self.ignored += 1;
                            }
                        }
                        continue;
                    }
                }
                HostNetEvent::Left { peer } => {
                    frozen.rx.remove(peer);
                    read.left.push(*peer);
                }
                _ => {}
            }
            if let Some(over) = frozen.hold(event)? {
                frozen.rx.remove(&over);
                network.disconnect(over);
            }
        }
        Ok(read)
    }

    /// A roster player leaving ends the attempt: the frozen roster no longer
    /// matches the room.
    fn check_left(
        &self,
        host: &HostAuthority,
        left: &[PeerKey],
        activating: bool,
    ) -> Result<(), Fail> {
        for peer in left {
            if let Some(name) = seated_name(host, *peer) {
                let reason = format!("{name} left during arming.");
                return Err(if activating {
                    Fail::Unprotected(reason)
                } else {
                    Fail::Refused(reason)
                });
            }
        }
        Ok(())
    }

    fn on_consent(
        &mut self,
        host: &HostAuthority,
        network: &mut QuicHost,
        peer: PeerKey,
        descriptor: &[u8],
        addr: &str,
        token: [u8; 32],
    ) -> Result<(), Fail> {
        let name = self.name.clone();
        let HostStage::Consent { offer } = std::mem::replace(&mut self.stage, HostStage::Done)
        else {
            unreachable!()
        };
        let consent = Descriptor::decode(descriptor)
            .map_err(|e| Fail::Refused(format!("{name} sent an unreadable consent ({e}).")))?;
        let addr: SocketAddr = addr
            .parse()
            .ok()
            .filter(|a: &SocketAddr| is_lan_ip(a.ip()) && a.port() != 0)
            .ok_or_else(|| {
                Fail::Refused(format!(
                    "{name} offered a standby address outside the local network."
                ))
            })?;
        let consented = offer.accept(host, network, peer, consent).map_err(|e| {
            Fail::Refused(format!("{name}'s consent did not match the offer ({e})."))
        })?;
        let root = &consented.descriptor().root;
        let pair_refused =
            |e: String| Fail::Refused(format!("The pair link to {name} could not start ({e})."));
        let binding = ConnectionBinding {
            session: root.session,
            descriptor: root.digest().map_err(|e| pair_refused(e.to_string()))?,
            term: 0,
            client: LinkRole::Original,
            server: LinkRole::Standby,
        };
        let standby = decented_net::PublicKey::from_slice(&root.standby)
            .map_err(|e| pair_refused(e.to_string()))?;
        let handle = decented_net::join_session_addr(
            &self.cfg,
            &self.identity,
            addr,
            &hex(&token),
            binding,
            standby,
        )
        .map_err(pair_refused)?;
        self.standby_at = Some((addr, token));
        self.stage = HostStage::Pairing {
            consented,
            link: QuicClient::new(handle),
        };
        self.state = ArmingState::Activating;
        Ok(())
    }

    /// Connect the pair link; only once it is admitted is the activation
    /// source created (consent precedes activation). Then `Setup` goes to
    /// every remote human, and a playing host signs its own ACK.
    fn poll_pair_link(
        &mut self,
        host: &HostAuthority,
        network: &mut QuicHost,
        now_ms: u64,
    ) -> Result<(), Fail> {
        let name = self.name.clone();
        let HostStage::Pairing { link, .. } = &mut self.stage else {
            unreachable!()
        };
        for _ in 0..MAX_BATCH {
            match link.poll() {
                None => break,
                Some(ClientNetEvent::Progress(_)) | Some(ClientNetEvent::Connected { .. }) => {}
                Some(ClientNetEvent::Failed(reason))
                | Some(ClientNetEvent::Disconnected { reason, .. }) => {
                    return Err(Fail::Refused(format!(
                        "The pair link to {name} failed ({reason})."
                    )))
                }
                Some(_) => {
                    return Err(Fail::Refused(format!(
                        "The pair link to {name} sent data before activation."
                    )))
                }
            }
            if link.migration_keys().is_ok() {
                break;
            }
        }
        if link.migration_keys().is_err() {
            return Ok(());
        }
        let HostStage::Pairing { consented, link } =
            std::mem::replace(&mut self.stage, HostStage::Done)
        else {
            unreachable!()
        };
        let descriptor = consented.descriptor().clone();
        let initial = consented
            .initial_hash()
            .map_err(|e| Fail::Refused(format!("The armed image could not be hashed ({e}).")))?;
        let encoded = descriptor.encode().map_err(|e| {
            Fail::Refused(format!("The armed descriptor could not be encoded ({e})."))
        })?;
        let mut activation = consented.into_activation(link, now_ms).map_err(|e| {
            Fail::Refused(format!("The pair link to {name} was not admitted ({e})."))
        })?;
        let mut pending = BTreeMap::new();
        let mut own_ack = None;
        let root = &descriptor.root;
        for human in &host.humans {
            let player = u64::from(human.id) + 1;
            if human.peer < LOCAL_PEER_BASE {
                pending.insert(human.peer, (player, human.name.clone()));
                continue;
            }
            // The playing host's own seat: the original signs for itself.
            let ack = CapabilityAck::sign(root, player, initial, 1, self.identity.keys())
                .and_then(|ack| Ok((ack.encode(root)?, ack)))
                .map_err(|e| Fail::Unprotected(format!("The host's own ACK failed ({e}).")))?;
            activation
                .acknowledge(ack.1, now_ms)
                .map_err(|e| Fail::Unprotected(format!("The host's own ACK failed ({e}).")))?;
            own_ack = Some(ack.0);
        }
        for peer in pending.keys() {
            send_to_client(
                network,
                *peer,
                &ToClientMigration::Setup {
                    descriptor: encoded.clone(),
                    initial,
                },
            );
        }
        self.stage = HostStage::Activating(Box::new(Activating {
            activation,
            descriptor,
            started_ms: now_ms,
            pending,
            own_ack,
        }));
        Ok(())
    }

    /// The stage stays activation until every fallible step has passed, so
    /// a failure here tells every client (see [`HostArming::fail`]).
    fn finish(&mut self, host: &HostAuthority, now_ms: u64) -> Result<(), Fail> {
        let frozen = self.frozen.as_ref().expect("phase 2 is frozen");
        let image = checkpoint(host).map(|(_, image)| image).ok();
        if image.as_ref() != Some(&frozen.image) {
            return Err(Fail::Unprotected(
                "The lobby changed while it was held for arming.".into(),
            ));
        }
        if frozen.joined() {
            // The newcomer is seated natively once the lobby is released.
            return Err(Fail::Unprotected("A player joined while arming.".into()));
        }
        let HostStage::Activating(act) = std::mem::replace(&mut self.stage, HostStage::Finishing)
        else {
            unreachable!()
        };
        let act = *act;
        let bootstrap = act
            .activation
            .finish(now_ms)
            .map_err(|e| Fail::Unprotected(format!("Activation could not finish ({e}).")))?;
        let (standby, standby_token) = self.standby_at.ok_or(Fail::Unprotected(
            "The standby's listener is unknown.".into(),
        ))?;
        let frozen = self.frozen.as_mut().expect("phase 2 is frozen");
        let held = std::mem::take(&mut frozen.held);
        self.stage = HostStage::Armed(Box::new(ArmedHost {
            bootstrap,
            descriptor: act.descriptor,
            own_ack: act.own_ack,
            held,
            standby,
            standby_token,
        }));
        self.state = ArmingState::Armed;
        Ok(())
    }
}

enum ClientStage {
    Idle,
    /// The consent request is up.
    Asked,
    /// The person said yes; waiting for the offer.
    Willing,
    /// Consented; the standby pair listener is starting.
    Listening {
        descriptor: Descriptor,
        listener: QuicHost,
        since_ms: u64,
    },
    /// `Consent` sent; waiting for the original's pair link.
    Pairing {
        descriptor: Descriptor,
        listener: QuicHost,
        since_ms: u64,
    },
    Sink(Box<LobbyStandby>, Descriptor),
    ArmedSink(Box<ArmedLobbyStandby>),
    Standby(Box<StandbyBootstrap>),
}

/// The client side of arming. Every joined client runs one: the selected
/// device answers `Ask` (its person decides, once) and consents to the offer
/// automatically; every client signs its capability ACK automatically.
///
/// Drive it every frame with [`ClientArming::poll`] after `session.update`.
/// `network` is the session's own [`QuicClient`]; never call `poll` while
/// that transport is borrowed elsewhere.
pub struct ClientArming {
    cfg: NetConfig,
    identity: Identity,
    code: String,
    advertise: Option<IpAddr>,
    state: ArmingState,
    stage: ClientStage,
    /// When the current `Ask` arrived, for [`ANSWER_WINDOW_MS`].
    asked_ms: u64,
    /// The Setup this device acknowledged and the exact ACK bytes it sent.
    acked: Option<(Vec<u8>, [u8; 32], Vec<u8>)>,
    /// The original's LAN address and token this device joined with. The
    /// switch re-dials (and, after a failed re-dial, rejoins) that address
    /// with the token from `Switch`.
    rejoin: Option<(SocketAddr, String)>,
    /// The armed bundle as received: `Armed{armed, cert0}`, then `Standby`.
    bundle: Option<(Vec<u8>, Vec<u8>)>,
    bundle_standby: Option<(String, [u8; 32])>,
    /// The verified switch, until [`ClientArming::take_switch`].
    switch: Option<Box<VerifiedSwitch>>,
    ignored: u64,
}

impl ClientArming {
    /// `code` is the room code this device joined (the beacon's or the typed
    /// one); the offer must name the same room.
    pub fn new(cfg: &NetConfig, identity: &Identity, code: &str) -> Self {
        Self {
            cfg: cfg.clone(),
            identity: identity.clone(),
            code: code.to_owned(),
            advertise: None,
            state: ArmingState::Idle,
            stage: ClientStage::Idle,
            asked_ms: 0,
            acked: None,
            rejoin: None,
            bundle: None,
            bundle_standby: None,
            switch: None,
            ignored: 0,
        }
    }

    /// The original's LAN address and admission token this device joined
    /// with (the beacon's, or `join_addr`'s). The switch re-dials them
    /// session-bound; without them this device cannot switch.
    pub fn rejoin(mut self, addr: SocketAddr, token: &str) -> Self {
        self.rejoin = Some((addr, token.to_owned()));
        self
    }

    /// The verified switch, once [`ArmingState::Switching`]: it takes the
    /// session (the route owns it from here). The standby device passes its
    /// running [`LiveStandby`]: its own seat shares the standby runtime's
    /// sender. Without a verified switch the session is handed back.
    #[allow(clippy::result_large_err)] // the session itself, handed back
    pub fn take_switch(
        &mut self,
        session: ClientSession,
        standby: Option<&LiveStandby>,
    ) -> Result<ClientSwitch, ClientSession> {
        let Some(verified) = self.switch.take() else {
            return Err(session);
        };
        // `on_switch` verified a switch only with a rejoin address.
        let Some(original) = self.rejoin.clone() else {
            return Err(session);
        };
        self.stage = ClientStage::Idle;
        Ok(ClientSwitch::start(
            &self.cfg,
            &self.identity,
            *verified,
            original,
            session,
            standby,
        ))
    }

    /// Advertise the standby listener on `ip` instead of this device's LAN
    /// address (a device on several LANs). The port is always the listener's.
    pub fn advertise(mut self, ip: IpAddr) -> Self {
        self.advertise = Some(ip);
        self
    }

    pub fn state(&self) -> &ArmingState {
        &self.state
    }

    /// The host asked this device to be the backup; show the consent screen
    /// and pass the person's answer to [`ClientArming::answer`].
    pub fn consent_requested(&self) -> bool {
        matches!(self.stage, ClientStage::Asked)
    }

    /// The exact capability ACK bytes this device signed, kept for its later
    /// endpoint.
    pub fn capability_ack(&self) -> Option<&[u8]> {
        self.acked.as_ref().map(|(_, _, ack)| ack.as_slice())
    }

    /// Messages ignored so far (out of turn, malformed, or unverified).
    pub fn ignored(&self) -> u64 {
        self.ignored
    }

    /// The armed descriptor this device verified and acknowledged (the
    /// standby device builds its `LiveStandby` with it).
    pub fn descriptor(&self) -> Option<Descriptor> {
        let (bytes, _, _) = self.acked.as_ref()?;
        Descriptor::decode(bytes).ok()
    }

    /// The standby's finished bootstrap, once [`ArmingState::Armed`].
    pub fn take_standby(&mut self) -> Option<StandbyBootstrap> {
        match std::mem::replace(&mut self.stage, ClientStage::Idle) {
            ClientStage::Standby(bootstrap) => Some(*bootstrap),
            stage => {
                self.stage = stage;
                None
            }
        }
    }

    /// The person's answer to the consent request (phase 1). Only after a
    /// yes will this device consent to an offer.
    pub fn answer(&mut self, network: &mut QuicClient, yes: bool) {
        if !matches!(self.stage, ClientStage::Asked) {
            return;
        }
        if yes {
            send_to_host(network, &ToHostMigration::Willing);
            self.stage = ClientStage::Willing;
            self.state = ArmingState::AwaitingConsent;
        } else {
            send_to_host(network, &ToHostMigration::Decline);
            self.stage = ClientStage::Idle;
            self.state = ArmingState::Refused {
                reason: "You declined to be the backup.".into(),
            };
        }
    }

    pub fn poll(
        &mut self,
        session: &mut ClientSession,
        network: &mut QuicClient,
        now_ms: u64,
    ) -> &ArmingState {
        for event in session.take_migration_events() {
            match event {
                MigrationEvent::Received(message) => self.on_message(network, message, now_ms),
                MigrationEvent::Rejected(_) => self.ignored += 1,
            }
        }
        if let Err(fail) = self.step(network, now_ms) {
            self.fail(network, fail);
        }
        &self.state
    }

    fn fail(&mut self, network: &mut QuicClient, fail: Fail) {
        if matches!(
            self.stage,
            ClientStage::Asked
                | ClientStage::Willing
                | ClientStage::Listening { .. }
                | ClientStage::Pairing { .. }
        ) {
            // Tell the host at once rather than leave it to its timeout.
            send_to_host(network, &ToHostMigration::Decline);
        }
        self.stage = ClientStage::Idle;
        self.state = match fail {
            Fail::Refused(reason) => ArmingState::Refused { reason },
            Fail::Unprotected(reason) => ArmingState::Unprotected { reason },
        };
    }

    fn on_message(&mut self, network: &mut QuicClient, message: ToClientMigration, now_ms: u64) {
        match message {
            ToClientMigration::Ask => {
                // A new Ask begins a new attempt: a yes given to an earlier
                // one does not carry over (attempts are bound by ordering on
                // this link, as the payloads carry no attempt id).
                let fresh = match self.stage {
                    ClientStage::Idle => !self.busy(),
                    ClientStage::Asked | ClientStage::Willing => true,
                    _ => false,
                };
                if fresh {
                    self.stage = ClientStage::Asked;
                    self.state = ArmingState::Asking;
                    self.asked_ms = now_ms;
                } else {
                    self.ignored += 1;
                }
            }
            ToClientMigration::Offer { code, descriptor } => {
                if !matches!(self.stage, ClientStage::Willing) {
                    // Never consent without this person's yes.
                    self.ignored += 1;
                    return;
                }
                if let Err(fail) = self.consent(network, &code, &descriptor, now_ms) {
                    send_to_host(network, &ToHostMigration::Decline);
                    self.stage = ClientStage::Idle;
                    self.state = match fail {
                        Fail::Refused(reason) | Fail::Unprotected(reason) => {
                            ArmingState::Refused { reason }
                        }
                    };
                }
            }
            ToClientMigration::Setup {
                descriptor,
                initial,
            } => {
                if let Err(reason) = self.on_setup(network, descriptor, initial) {
                    log::warn!("[arming] setup refused: {reason}");
                    self.ignored += 1;
                    if !matches!(self.stage, ClientStage::Idle) {
                        self.stage = ClientStage::Idle;
                    }
                    self.state = ArmingState::Refused { reason };
                }
            }
            ToClientMigration::Withdrawn { reason } => {
                self.stage = ClientStage::Idle;
                self.acked = None;
                self.forget_bundle();
                self.state = ArmingState::Refused { reason };
            }
            ToClientMigration::Unprotected { reason } => {
                self.stage = ClientStage::Idle;
                self.acked = None;
                self.forget_bundle();
                self.state = ArmingState::Unprotected { reason };
            }
            // The armed bundle (C2, C6): kept until `Switch`, and only by a
            // device that acknowledged a setup.
            ToClientMigration::Armed { armed, certificate }
                if self.acked.is_some() && self.bundle.is_none() =>
            {
                self.bundle = Some((armed, certificate));
            }
            ToClientMigration::Standby { addr, token }
                if self.bundle.is_some() && self.bundle_standby.is_none() =>
            {
                self.bundle_standby = Some((addr, token));
            }
            ToClientMigration::Switch { token }
                if self.bundle.is_some() && self.switch.is_none() =>
            {
                self.on_switch(network, token)
            }
            _ => self.ignored += 1,
        }
    }

    fn forget_bundle(&mut self) {
        self.bundle = None;
        self.bundle_standby = None;
        self.switch = None;
    }

    /// C7: verify the bundle against the acknowledged setup, and only then
    /// close the ordinary link. A refused bundle leaves the link open: the
    /// host closes it as a straggler and reports this device unprotected.
    fn on_switch(&mut self, network: &mut QuicClient, token: [u8; 32]) {
        let verified = match (&self.acked, &self.bundle, network.migration_keys()) {
            (Some(acked), Some((armed, certificate)), Ok((local, _))) => verify_bundle(
                acked,
                local,
                armed,
                certificate,
                self.bundle_standby.as_ref(),
                token,
            ),
            (_, _, Err(e)) => Err(format!("The host link is not admitted ({e}).")),
            _ => Err("The switch came without the armed bundle.".into()),
        };
        // Without the room's address this device could not re-dial: it stays
        // on its ordinary link rather than close it for nothing.
        let verified = verified.and_then(|v| match self.rejoin {
            Some(_) => Ok(v),
            None => Err("This device does not know the room's address to rejoin.".into()),
        });
        match verified {
            Ok(verified) => {
                network.close();
                self.switch = Some(Box::new(verified));
                self.state = ArmingState::Switching;
            }
            Err(reason) => {
                log::warn!("[arming] switch refused: {reason}");
                self.ignored += 1;
                self.forget_bundle();
                self.state = ArmingState::Unprotected { reason };
            }
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self.state,
            ArmingState::AwaitingConsent | ArmingState::Activating
        )
    }

    /// Phase 2 on the selected device, without asking the person again.
    fn consent(
        &mut self,
        network: &mut QuicClient,
        code: &str,
        descriptor: &[u8],
        now_ms: u64,
    ) -> Result<(), Fail> {
        match network.link_path() {
            Some(LinkPath::Lan) => {}
            Some(LinkPath::Bluetooth) => {
                return Err(Fail::Refused(
                    "Arming is refused over a Bluetooth link.".into(),
                ))
            }
            _ => {
                return Err(Fail::Refused(
                    "Arming is LAN-only: this device did not join over the local network.".into(),
                ))
            }
        }
        if code != self.code {
            return Err(Fail::Refused("The offer names a different room.".into()));
        }
        if let Some(reason) = Descriptor::decode(descriptor)
            .ok()
            .and_then(|d| super::version_refusal(&d.root))
        {
            return Err(Fail::Refused(reason.into()));
        }
        let consented = consent_lobby_offer(network, &self.identity, &self.code, descriptor)
            .map_err(|e| Fail::Refused(format!("The offer did not verify ({e}).")))?;
        let handle =
            decented_net::host_standby(&self.cfg, &self.identity, &self.code, consented.clone())
                .map_err(|e| Fail::Refused(format!("The standby could not listen ({e}).")))?;
        self.stage = ClientStage::Listening {
            descriptor: consented,
            listener: QuicHost::new(handle),
            since_ms: now_ms,
        };
        self.state = ArmingState::Activating;
        Ok(())
    }

    /// Every client: verify the host's setup and sign the capability ACK once.
    fn on_setup(
        &mut self,
        network: &mut QuicClient,
        descriptor: Vec<u8>,
        initial: [u8; 32],
    ) -> Result<(), String> {
        if let Some((seen, seen_initial, ack)) = &self.acked {
            if *seen == descriptor && *seen_initial == initial {
                // A repeated Setup gets the same bytes, never a new signature.
                send_to_host(network, &ToHostMigration::Ack { ack: ack.clone() });
                return Ok(());
            }
            return Err("A second, different arming setup was refused.".into());
        }
        let decoded = Descriptor::decode(&descriptor)
            .map_err(|e| format!("The arming setup was unreadable ({e})."))?;
        decoded
            .verify()
            .map_err(|e| format!("The arming setup did not verify ({e})."))?;
        let (local, host_key) = network
            .migration_keys()
            .map_err(|e| format!("The host link is not admitted ({e})."))?;
        let root = &decoded.root;
        if let Some(reason) = super::version_refusal(root) {
            return Err(reason.into());
        }
        let code = room_code_hash(&self.code).map_err(|e| e.to_string())?;
        if self.identity.pubkey().to_bytes() != local
            || root.host != host_key
            || root.controller != host_key
            || root.app != crate::net::APP_TAG
            || root.game_version != crate::proto::PROTO_VERSION
            || root.checkpoint_version != SCHEMA
            || root.code != code
        {
            return Err("The arming setup is not for this room and host.".into());
        }
        if root.standby == local {
            let consented = match &self.stage {
                ClientStage::Pairing { descriptor, .. } | ClientStage::Sink(_, descriptor) => {
                    Some(descriptor)
                }
                _ => None,
            };
            if consented != Some(&decoded) {
                return Err(
                    "The arming setup names this device as a backup it did not consent to be."
                        .into(),
                );
            }
        }
        let player = root
            .members
            .iter()
            .find(|m| m.key == Some(local))
            .map(|m| m.player)
            .ok_or("This device is not in the arming roster.")?;
        let ack = CapabilityAck::sign(root, player, initial, 1, self.identity.keys())
            .and_then(|ack| ack.encode(root))
            .map_err(|e| format!("The capability ACK could not be signed ({e})."))?;
        send_to_host(network, &ToHostMigration::Ack { ack: ack.clone() });
        self.acked = Some((descriptor, initial, ack));
        if matches!(self.state, ArmingState::Idle) || self.state.is_finished() {
            self.state = ArmingState::Activating;
        }
        Ok(())
    }

    fn step(&mut self, network: &mut QuicClient, now_ms: u64) -> Result<(), Fail> {
        match &mut self.stage {
            ClientStage::Listening {
                listener, since_ms, ..
            } => {
                if now_ms.saturating_sub(*since_ms) >= CONSENT_WINDOW_MS {
                    return Err(Fail::Refused("The standby listener did not start.".into()));
                }
                if listener.poll().is_some() {
                    return Err(Fail::Refused(
                        "The standby listener had a peer before consent.".into(),
                    ));
                }
                if let Some(reason) = listener.failure() {
                    return Err(Fail::Refused(format!(
                        "The standby could not listen ({reason})."
                    )));
                }
                if !listener.is_ready() {
                    return Ok(());
                }
                let handle = listener
                    .handle()
                    .ok_or(Fail::Refused("The standby listener closed.".into()))?;
                let addr = standby_addr(handle, self.advertise).ok_or(Fail::Refused(
                    "This device has no local network address for the standby.".into(),
                ))?;
                let token = unhex32(&handle.lan_token()).ok_or(Fail::Refused(
                    "The standby listener has no admission token.".into(),
                ))?;
                let ClientStage::Listening {
                    descriptor,
                    listener,
                    ..
                } = std::mem::replace(&mut self.stage, ClientStage::Idle)
                else {
                    unreachable!()
                };
                let encoded = descriptor.encode().map_err(|e| {
                    Fail::Refused(format!("The consent could not be encoded ({e})."))
                })?;
                send_to_host(
                    network,
                    &ToHostMigration::Consent {
                        descriptor: encoded,
                        addr: addr.to_string(),
                        token,
                    },
                );
                self.stage = ClientStage::Pairing {
                    descriptor,
                    listener,
                    since_ms: now_ms,
                };
                Ok(())
            }
            ClientStage::Pairing {
                listener, since_ms, ..
            } => {
                if now_ms.saturating_sub(*since_ms) >= CONSENT_WINDOW_MS {
                    return Err(Fail::Refused(
                        "The host did not connect the pair link within 5 s.".into(),
                    ));
                }
                let peer = match listener.poll() {
                    None => return Ok(()),
                    Some(HostNetEvent::Joined { peer, .. }) => peer,
                    Some(_) => {
                        return Err(Fail::Refused(
                            "The pair link sent data before it was admitted.".into(),
                        ))
                    }
                };
                let ClientStage::Pairing {
                    descriptor,
                    listener,
                    ..
                } = std::mem::replace(&mut self.stage, ClientStage::Idle)
                else {
                    unreachable!()
                };
                let sink = LobbyStandby::new(descriptor.clone(), listener, peer, now_ms)
                    .map_err(|e| Fail::Refused(format!("The pair link was not admitted ({e}).")))?;
                self.stage = ClientStage::Sink(Box::new(sink), descriptor);
                Ok(())
            }
            ClientStage::Sink(sink, _) => {
                let ready = sink
                    .poll(now_ms)
                    .map_err(|e| Fail::Unprotected(format!("Arming failed ({e}).")))?;
                if ready {
                    let ClientStage::Sink(sink, _) =
                        std::mem::replace(&mut self.stage, ClientStage::Idle)
                    else {
                        unreachable!()
                    };
                    let armed = sink
                        .arm(self.identity.keys(), now_ms)
                        .map_err(|e| Fail::Unprotected(format!("Arming failed ({e}).")))?;
                    self.stage = ClientStage::ArmedSink(Box::new(armed));
                }
                Ok(())
            }
            ClientStage::ArmedSink(armed) => {
                let sent = armed
                    .poll(now_ms)
                    .map_err(|e| Fail::Unprotected(format!("Arming failed ({e}).")))?;
                if sent {
                    let ClientStage::ArmedSink(armed) =
                        std::mem::replace(&mut self.stage, ClientStage::Idle)
                    else {
                        unreachable!()
                    };
                    let bootstrap = armed
                        .finish(now_ms)
                        .map_err(|e| Fail::Unprotected(format!("Arming failed ({e}).")))?;
                    self.stage = ClientStage::Standby(Box::new(bootstrap));
                    self.state = ArmingState::Armed;
                }
                Ok(())
            }
            ClientStage::Asked if now_ms.saturating_sub(self.asked_ms) >= ANSWER_WINDOW_MS => Err(
                Fail::Refused("The backup did not answer within 60 s.".into()),
            ),
            ClientStage::Willing if now_ms.saturating_sub(self.asked_ms) >= ANSWER_WINDOW_MS => {
                Err(Fail::Refused(
                    "The host did not send the backup offer within 60 s.".into(),
                ))
            }
            ClientStage::Idle
            | ClientStage::Asked
            | ClientStage::Willing
            | ClientStage::Standby(_) => Ok(()),
        }
    }
}

/// The standby listener's LAN address, composed as Fray's LAN beacon does:
/// the listener's own address when it is bound to one, else this device's
/// LAN IPv4 with the listener's port.
fn standby_addr(
    handle: &decented_net::HostHandle,
    advertise: Option<IpAddr>,
) -> Option<SocketAddr> {
    let local = handle.local_addr()?;
    let ip = match advertise {
        Some(ip) => ip,
        None if !local.ip().is_unspecified() => local.ip(),
        None => IpAddr::V4(decented_net::nat::candidates::local_outbound_v4()?),
    };
    Some(SocketAddr::new(ip, local.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_through_hex_and_bad_text_is_refused() {
        let token = [0xA7u8; 32];
        assert_eq!(unhex32(&hex(&token)), Some(token));
        assert_eq!(unhex32("00"), None);
        assert_eq!(unhex32(&"zz".repeat(32)), None);
    }

    #[test]
    fn only_private_network_addresses_are_dialled() {
        for ok in [
            "192.168.1.20",
            "10.0.0.2",
            "172.16.4.4",
            "127.0.0.1",
            "fe80::1",
            "fd00::2",
        ] {
            assert!(is_lan_ip(ok.parse().unwrap()), "{ok}");
        }
        for bad in ["8.8.8.8", "100.85.0.1", "2001:db8::1", "0.0.0.0"] {
            assert!(!is_lan_ip(bad.parse().unwrap()), "{bad}");
        }
    }

    fn lossy(peer: PeerKey) -> HostNetEvent {
        HostNetEvent::Lossy {
            peer,
            bytes: vec![0xEE],
        }
    }

    /// A host with one seated loopback player, for replaying held events.
    fn seated_host() -> (HostAuthority, ClientSession, PeerKey) {
        use crate::transport::{LoopClock, LoopbackConfig, LoopbackHub};
        use crate::HostConfig;
        let hub = LoopbackHub::new(LoopClock::real(), 7);
        let mut host = HostAuthority::new_headless(
            Box::new(hub.host()),
            HostConfig {
                auto_start: false,
                ..HostConfig::default()
            },
        );
        let mut client = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "A", 0);
        for now in 0..2000 {
            host.update(now);
            client.update(now, race_core::Input::default());
            if host.human_count() == 1 {
                let peer = host.humans[0].peer;
                return (host, client, peer);
            }
        }
        panic!("the loopback player never sat down");
    }

    /// Item 4: with the boundary's global cap full, a `Left` is still held
    /// (once), and replays exactly once when the boundary ends; an ordinary
    /// event over the cap fails arming but is held, not dropped.
    #[test]
    fn a_left_is_held_outside_the_full_cap_and_replays_once_at_the_thaw() {
        let (mut host, _client, seated) = seated_host();
        host.migration_freeze();
        let mut frozen = Frozen::new(0, Vec::new());
        // Fill the cap from many peers, each within its own budget.
        let mut peer = 10_000;
        while frozen.held.len() < MAX_JOURNAL_RECORDS {
            for _ in 0..HELD_PER_PEER_RECORDS {
                if frozen.held.len() == MAX_JOURNAL_RECORDS {
                    break;
                }
                assert!(matches!(frozen.hold(lossy(peer)), Ok(None)));
            }
            peer += 1;
        }
        assert!(matches!(
            frozen.hold(HostNetEvent::Left { peer: seated }),
            Ok(None)
        ));
        let lefts = |f: &Frozen| {
            f.held
                .iter()
                .filter(|e| matches!(e, HostNetEvent::Left { peer } if *peer == seated))
                .count()
        };
        assert_eq!(lefts(&frozen), 1);
        // An ordinary event over the cap: arming fails, nothing is dropped.
        let before = frozen.held.len();
        assert!(matches!(
            frozen.hold(lossy(peer + 1)),
            Err(Fail::Unprotected(reason)) if reason.contains("Too much traffic")
        ));
        assert_eq!(frozen.held.len(), before + 1);
        assert_eq!(lefts(&frozen), 1);
        host.migration_thaw(frozen.held);
        assert_eq!(host.human_count(), 1, "nothing is read before the update");
        host.update(5000);
        assert_eq!(host.human_count(), 0, "the held Left replayed");
        assert!(host.migration_held.is_empty());
    }

    /// Item 5: migration parts count against the sender's own budget; one
    /// peer over it is closed (its held events dropped, never its `Left`)
    /// without touching another peer's.
    #[test]
    fn a_peer_over_its_budget_is_closed_without_touching_another() {
        let mut frozen = Frozen::new(0, Vec::new());
        assert!(matches!(frozen.hold(lossy(1)), Ok(None)));
        assert!(matches!(frozen.hold(lossy(2)), Ok(None)));
        for _ in 1..HELD_PER_PEER_RECORDS {
            assert_eq!(frozen.charge(2, 10), None);
        }
        assert_eq!(frozen.charge(2, 10), Some(2));
        assert!(frozen.closed.contains(&2));
        assert!(matches!(frozen.hold(lossy(2)), Ok(None)));
        assert!(frozen.held.iter().all(|e| Frozen::peer_of(e) == 1));
        assert!(matches!(
            frozen.hold(HostNetEvent::Left { peer: 2 }),
            Ok(None)
        ));
        assert_eq!(frozen.held.len(), 2);
        // Peer 1 still has its whole budget.
        for _ in 1..HELD_PER_PEER_RECORDS {
            assert_eq!(frozen.charge(1, 10), None);
        }
    }

    /// Item 3: a peer joining while held (here over Bluetooth) is seen at
    /// `finish`, whatever its path.
    #[test]
    fn a_held_join_on_any_path_is_seen() {
        let mut frozen = Frozen::new(0, Vec::new());
        assert!(!frozen.joined());
        assert!(matches!(
            frozen.hold(HostNetEvent::Joined {
                peer: 9,
                path: LinkPath::Bluetooth
            }),
            Ok(None)
        ));
        assert!(frozen.joined());
    }
}
