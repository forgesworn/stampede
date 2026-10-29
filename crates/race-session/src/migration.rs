//! Complete Stampede authority checkpoints, separate from recipient snapshots.
//!
//! No transport, key or callback is serialized. Custom geometry is rebuilt from
//! its canonical content, independent of whichever track the process cache holds.

use super::{HostAuthority, HostTransport, PeerKey};
pub use super::checkpoint::{checkpoint, restore, Binding};
#[cfg(test)]
use super::checkpoint::{binding, HostOnly, Snapshot, MAGIC};
use super::checkpoint::{options, null_transport, SCHEMA};
use crate::track_transfer::SelectedTrack;
use bincode::Options;
use decented_net::migration::{state_hash, Error, Hash, Result, MAX_IMAGE};
use race_core::CustomTrack;
#[cfg(test)]
use race_core::TrackId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[path = "migration/live.rs"]
mod live;
pub use live::{
    LivePrimary, ObsoleteIngress, OriginalRecoveryLink, RuntimeEvent, PARKED_DEPARTURE_MS,
    RETRY_MIN_GAP_MS, STANDBY_WATCH_MS,
};
#[path = "migration/players.rs"]
mod players;
pub use players::{
    PlayerHost, PlayerHostRefused, PlayerIngress, RecoveringPlayer, RecoveryFrame, RemoteFrame,
    RemotePlayer, FREEZE_AFTER_MS, SECOND_TAKEOVER_REFUSED, SESSION_LOST_AFTER_MS,
};
#[path = "migration/successor.rs"]
mod successor;
pub use successor::{
    start_resume_host, LiveSuccessor, OwnFrame, SuccessorCommit, SuccessorIngress, SurvivorLinks,
};
#[path = "migration/standby.rs"]
mod standby;
pub use standby::{
    LiveStandby, OriginalLink, OwnSeat, OwnSeatReplay, PromoteRefused, PromotedStandby,
    StandbyRecoveryLink,
};
#[path = "migration/lobby.rs"]
mod lobby;
pub use super::migration_wire as wire;
pub use lobby::{
    thaw_lobby, ArmedHost, ArmingState, ClientArming, HostArming, ACTIVATION_WINDOW_MS,
    ANSWER_WINDOW_MS, CONSENT_WINDOW_MS, HELD_PER_PEER_BYTES, HELD_PER_PEER_RECORDS,
};
#[path = "migration/switch.rs"]
mod switch;
pub use switch::{
    ClientSwitch, HostSwitch, SwitchState, UnprotectedPlayer, CERTIFICATE_WINDOW_MS,
    SWITCH_JOIN_MS, SWITCH_LEAVE_MS, SWITCH_REJOIN_GRACE_MS, SWITCH_ROUTE_MS, SWITCH_TOKEN_HOLD_MS,
};

/// The device's monotonic clock for the migration runners' 200 ms watch.
///
/// Start one per device and hand every runner on that device a copy
/// ([`LivePrimary`], [`LiveStandby`]); the runners sample it themselves, so no
/// promotion can be driven by a caller-supplied watch time. It counts
/// milliseconds since [`RunnerClock::start`] and cannot be set or stepped, by
/// design: there is no constructor from a number.
#[derive(Clone, Copy, Debug)]
pub struct RunnerClock {
    origin: std::time::Instant,
}
impl RunnerClock {
    /// The only constructor: milliseconds count from this instant.
    pub fn start() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
    /// Monotonic milliseconds since [`Self::start`].
    pub fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

/// Every transport side effect is quarantined with the post-state. The room
/// runner maps data effects to Fray's certified recipient streams; control
/// effects are executed only after the enclosing receipt is accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Reliable { peer: PeerKey, bytes: Vec<u8> },
    Lossy { peer: PeerKey, bytes: Vec<u8> },
    InstallCustomTrack(Vec<u8>),
    Kick(PeerKey),
    Disconnect(PeerKey),
    ConfirmTimeout(PeerKey),
    Open(bool),
    Shutdown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameSeat {
    pub player: u64,
    pub seat: u8,
    pub native_id: u8,
    pub logical_peer: PeerKey,
}
#[derive(Serialize, Deserialize)]
struct BatchImage {
    schema: u32,
    next_player: u64,
    #[serde(deserialize_with = "decented_net::migration::bounded_vec::<_,_,8>")]
    seats: Vec<GameSeat>,
    match_id: u64,
    logical_ms: u64,
    binding: Binding,
    #[serde(deserialize_with = "decented_net::migration::bounded_vec::<_,_,1048576>")]
    authority: Vec<u8>,
    #[serde(deserialize_with = "decented_net::migration::bounded_vec::<_,_,256>")]
    effects: Vec<Effect>,
}
struct Capture {
    events: std::collections::VecDeque<crate::transport::HostNetEvent>,
    rtts: BTreeMap<PeerKey, Option<u32>>,
    effects: Vec<Effect>,
    bytes: usize,
    overflow: bool,
}
impl Capture {
    fn push(&mut self, effect: Effect) {
        let (size, cap) = match &effect {
            Effect::Reliable { bytes, .. } | Effect::Lossy { bytes, .. } => {
                (bytes.len(), decented_net::MAX_RELIABLE_LEN)
            }
            Effect::InstallCustomTrack(bytes) => (bytes.len(), crate::track_transfer::MAX_BYTES),
            _ => (0, 0),
        };
        if self.overflow
            || size > cap
            || self.effects.len() >= decented_net::migration::MAX_BATCH
            || self
                .bytes
                .checked_add(size)
                .is_none_or(|n| n > decented_net::migration::MAX_OUTBOX_BYTES)
        {
            self.overflow = true;
            return;
        }
        self.bytes += size;
        self.effects.push(effect);
    }
}
struct CaptureTransport(std::sync::Arc<std::sync::Mutex<Capture>>);
impl HostTransport for CaptureTransport {
    fn poll(&mut self) -> Option<crate::transport::HostNetEvent> {
        self.0.lock().unwrap().events.pop_front()
    }
    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0
            .lock()
            .unwrap()
            .push(Effect::Reliable { peer, bytes });
    }
    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0.lock().unwrap().push(Effect::Lossy { peer, bytes });
    }
    fn kick(&mut self, peer: PeerKey) {
        self.0.lock().unwrap().push(Effect::Kick(peer));
    }
    fn disconnect(&mut self, peer: PeerKey) {
        self.0.lock().unwrap().push(Effect::Disconnect(peer));
    }
    fn confirm_timeout(&mut self, peer: PeerKey) {
        self.0.lock().unwrap().push(Effect::ConfirmTimeout(peer));
    }
    fn set_open(&mut self, open: bool) {
        self.0.lock().unwrap().push(Effect::Open(open));
    }
    fn set_open_keeping_token(&mut self, open: bool) {
        self.set_open(open);
    }
    fn rtt_ms(&self, peer: PeerKey) -> Option<u32> {
        self.0.lock().unwrap().rtts.get(&peer).copied().flatten()
    }
    fn shutdown(&mut self) {
        self.0.lock().unwrap().push(Effect::Shutdown);
    }
}
/// A speculative authority is deliberately private: UI queries continue to
/// read the previous committed authority while this transaction awaits ACK.
pub struct StagedBatch {
    image: BatchImage,
    encoded: Vec<u8>,
    base: decented_net::migration::Receipt,
}
impl StagedBatch {
    pub fn bytes(&self) -> &[u8] {
        &self.encoded
    }
    pub fn binding(&self) -> &Binding {
        &self.image.binding
    }
    pub fn match_id(&self) -> u64 {
        self.image.match_id
    }
    pub fn effects(&self) -> &[Effect] {
        &self.image.effects
    }
    pub fn seat_bindings(&self) -> &[GameSeat] {
        &self.image.seats
    }
    pub fn next_player(&self) -> u64 {
        self.image.next_player
    }
    /// Fill the game-owned bindings and send through the shared actor. The
    /// caller retains this staged transaction when queue pressure rejects it.
    /// Membership and handled-intent metadata come from the session runner.
    pub fn prepare_actor(
        &self,
        actor: &mut impl decented_net::migration::PublicationActor,
        mut session: decented_net::migration::SessionImage,
        watermarks: Vec<decented_net::migration::Watermark>,
    ) -> Result<()> {
        if actor.primary().receipt() != &self.base {
            return Err(Error::Stale);
        }
        session.game = self.encoded.clone();
        session.logical_ms = self.image.logical_ms;
        session.match_id = self.image.match_id;
        session.config = self.image.binding.config;
        session.content = self.image.binding.content;
        validate_seats(
            &restore(&self.image.authority, &self.image.binding, null_transport())?,
            &self.image.seats,
            self.image.next_player,
            Some(&session),
        )?;
        let outputs = self.outputs(&session.members, actor.primary())?;
        actor.prepare(session, watermarks, outputs)
    }
    /// Release the exact staged authority and controls through the actor's
    /// owned one-use publication gate after a Committed event.
    pub fn adopt_actor(
        self,
        actor: &mut impl decented_net::migration::PublicationActor,
        certified: &[decented_net::migration::CertifiedOutput],
    ) -> Result<(
        HostAuthority,
        Vec<decented_net::migration::CertifiedOutput>,
        Vec<Effect>,
    )> {
        let root = actor.root().clone();
        actor.adopt(|primary| self.adopt(primary, &root, certified))
    }
    pub fn outputs(
        &self,
        members: &[decented_net::migration::SessionMember],
        primary: &decented_net::migration::Primary,
    ) -> Result<Vec<decented_net::migration::Output>> {
        use decented_net::migration::{Delivery, Output};
        let mut outputs = Vec::new();
        let mut allocated = BTreeMap::new();
        for effect in &self.image.effects {
            let (peer, bytes, delivery) = match effect {
                Effect::Reliable { peer, bytes } => (*peer, bytes, Delivery::Reliable),
                Effect::Lossy { peer, bytes } => (*peer, bytes, Delivery::Lossy),
                _ => continue,
            };
            let recipient = members
                .iter()
                .find(|m| m.logical_peer == peer)
                .ok_or(Error::Unauthorized)?
                .player;
            let ceiling = allocated
                .entry((recipient, u8::from(delivery == Delivery::Lossy)))
                .or_insert_with(|| primary.output_ceiling(recipient, delivery));
            *ceiling = ceiling.checked_add(1).ok_or(Error::Bounds)?;
            outputs.push(Output {
                recipient,
                delivery,
                sequence: *ceiling,
                bytes: bytes.clone(),
            });
        }
        Ok(outputs)
    }
    /// Only an exact image already accepted by Primary can unlock the new UI
    /// state and transport controls. Consuming self prevents a second release.
    pub fn adopt(
        self,
        primary: &mut decented_net::migration::Primary,
        root: &decented_net::migration::Root,
        certified: &[decented_net::migration::CertifiedOutput],
    ) -> Result<(
        HostAuthority,
        Vec<decented_net::migration::CertifiedOutput>,
        Vec<Effect>,
    )> {
        primary.receipt().verify(&root.context()?)?;
        if primary.is_closed()
            || primary.receipt().index != self.base.index.checked_add(1).ok_or(Error::Bounds)?
            || primary.receipt().parent != self.base.digest(&root.context()?)?
        {
            return Err(Error::Stale);
        }
        let image = decented_net::migration::SessionImage::decode(root, primary.committed_image())?;
        if image.game != self.encoded
            || image.config != self.image.binding.config
            || image.content != self.image.binding.content
            || image.logical_ms != primary.receipt().logical_ms
        {
            return Err(Error::Conflict);
        }
        let authority = restore(&self.image.authority, &self.image.binding, null_transport())?;
        validate_seats(
            &authority,
            &self.image.seats,
            self.image.next_player,
            Some(&image),
        )?;
        if authority.now != image.logical_ms || authority.races != image.match_id {
            return Err(Error::Conflict);
        }
        if primary.receipt().outputs_count as usize != certified.len() {
            return Err(Error::Conflict);
        }
        for (ordinal, record) in certified.iter().enumerate() {
            record.verify(&root.context()?)?;
            if record.receipt != *primary.receipt() || record.ordinal as usize != ordinal {
                return Err(Error::Conflict);
            }
        }
        validate_outputs(
            &self.image.effects,
            &image.members,
            &certified
                .iter()
                .map(|c| c.output.clone())
                .collect::<Vec<_>>(),
        )?;
        let mut custom = Vec::new();
        let mut effects = Vec::new();
        for effect in self.image.effects {
            match effect {
                Effect::InstallCustomTrack(bytes) => {
                    let track = CustomTrack::from_bytes(&bytes)
                        .map_err(|_| Error::Invalid("deferred custom content"))?;
                    let selected = SelectedTrack::new(track)
                        .map_err(|_| Error::Invalid("deferred custom geometry"))?;
                    if selected.bytes != bytes {
                        return Err(Error::Invalid("deferred noncanonical track"));
                    }
                    custom.push(selected.def);
                }
                Effect::Reliable { .. } | Effect::Lossy { .. } => {}
                other => effects.push(other),
            }
        }
        primary.claim_adoption(&primary.receipt().clone())?;
        for def in custom {
            race_core::custom::install(def);
        }
        Ok((authority, certified.to_vec(), effects))
    }
}
fn encode_batch(image: &BatchImage) -> Result<Vec<u8>> {
    let size = options()
        .serialized_size(image)
        .map_err(|_| Error::Bounds)?;
    if size > MAX_IMAGE as u64 {
        return Err(Error::Bounds);
    }
    options()
        .serialize(image)
        .map_err(|_| Error::Invalid("race batch encoding"))
}
pub fn checkpoint_batch(host: &HostAuthority) -> Result<(Binding, Vec<u8>)> {
    let (binding, authority) = checkpoint(host)?;
    // No Fray IDs exist before this frozen arming allocation. Persist it;
    // native IDs and vector positions are never used to reconstruct it later.
    let mut seats: Vec<_> = host
        .humans
        .iter()
        .enumerate()
        .map(|(i, h)| GameSeat {
            player: u64::from(h.id) + 1,
            seat: i as u8,
            native_id: h.id,
            logical_peer: h.peer,
        })
        .collect();
    seats.sort_by_key(|s| s.player);
    let next_player = seats
        .last()
        .map_or(Some(1), |s| s.player.checked_add(1))
        .ok_or(Error::Bounds)?;
    validate_seats(host, &seats, next_player, None)?;
    let image = BatchImage {
        schema: SCHEMA,
        next_player,
        seats,
        match_id: host.races,
        logical_ms: host.now,
        binding: binding.clone(),
        authority,
        effects: vec![],
    };
    Ok((binding, encode_batch(&image)?))
}

/// Public consent metadata plus a private, frozen initial checkpoint. The app
/// must stop ordinary host/client updates and drain ordered loopback effects
/// before preparation. No image accessor exists before verified consent.
pub struct LobbyOffer {
    descriptor: decented_net::migration::Descriptor,
    image: decented_net::migration::SessionImage,
}

/// Exact selected-backup consent has been checked against a still-live,
/// admitted connection. Activation still requires every human's capability
/// ACK, native restore equivalence and the standby's signed Armed certificate.
pub struct ConsentedLobby {
    descriptor: decented_net::migration::Descriptor,
    image: decented_net::migration::SessionImage,
}

impl LobbyOffer {
    /// Selection is an explicit app action. Joining never calls this method.
    pub fn prepare(
        host: &HostAuthority,
        network: &crate::net::QuicHost,
        keys: &decented_net::Keys,
        selected_peer: PeerKey,
    ) -> Result<Self> {
        use decented_net::migration::*;
        let handle = network.handle().ok_or(Error::Closed)?;
        let original = keys.public_key().to_bytes();
        if handle.runtime_key().to_bytes() != original
            || host.phase != super::HostPhase::Lobby
            || host.lobby_dirty
        {
            return Err(Error::Unauthorized);
        }
        let standby = network.migration_peer_key(selected_peer)?;
        if standby == original {
            return Err(Error::Unauthorized);
        }
        let local_count = host.humans.iter().filter(|h| h.host).count();
        if local_count > 1 || (host.headless && local_count != 0) {
            return Err(Error::Invalid("migration local player mapping"));
        }
        let (binding, game) = checkpoint_batch(host)?;
        let mut members = Vec::with_capacity(host.humans.len());
        let mut routes = Vec::with_capacity(host.humans.len());
        for (seat, human) in host.humans.iter().enumerate() {
            let player = u64::from(human.id) + 1;
            let key = if human.host {
                if human.peer < crate::transport::LOCAL_PEER_BASE {
                    return Err(Error::Conflict);
                }
                original
            } else {
                if human.peer >= crate::transport::LOCAL_PEER_BASE {
                    return Err(Error::Conflict);
                }
                network.migration_peer_key(human.peer)?
            };
            members.push(Member {
                player,
                seat: seat as u8,
                key: Some(key),
            });
            routes.push((player, human.peer));
        }
        members.sort_by_key(|member| member.player);
        let created_day = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Invalid("UTC before epoch"))?
            .as_secs()
            / 86400;
        let root = Root {
            session: fresh_session()?,
            app: crate::net::APP_TAG.into(),
            transport_version: 7,
            game_version: crate::proto::PROTO_VERSION,
            checkpoint_version: SCHEMA,
            host: original,
            standby,
            controller: original,
            code: room_code_hash(handle.code())?,
            created_day,
            roster_revision: 1,
            members,
            config: binding.config,
            content: binding.content,
            limits: ResourceLimits::default(),
        };
        let image = SessionImage::initial(&root, &routes, game, host.races, host.now)?;
        validate_initial(&root, &image)?;
        Ok(Self {
            descriptor: Descriptor::offer(root, keys)?,
            image,
        })
    }

    pub fn descriptor(&self) -> &decented_net::migration::Descriptor {
        &self.descriptor
    }

    /// Only public hashes/roster travel before consent, never authority bytes.
    pub fn initial_hash(&self) -> Result<Hash> {
        state_hash(&self.image.encode(&self.descriptor.root)?)
    }

    pub fn accept(
        self,
        host: &HostAuthority,
        network: &crate::net::QuicHost,
        peer: PeerKey,
        consent: decented_net::migration::Descriptor,
    ) -> Result<ConsentedLobby> {
        let handle = network.handle().ok_or(Error::Closed)?;
        if handle.runtime_key().to_bytes() != self.descriptor.root.host
            || decented_net::migration::room_code_hash(handle.code())? != self.descriptor.root.code
            || network.migration_peer_key(peer)? != self.descriptor.root.standby
            || consent.root != self.descriptor.root
            || consent.host_signature != self.descriptor.host_signature
        {
            return Err(Error::Unauthorized);
        }
        consent.verify()?;
        // A roster, config, clock or accepted-input change invalidates setup.
        // The caller cannot attach an earlier consent to a newer checkpoint.
        let (binding, game) = checkpoint_batch(host)?;
        if host.phase != super::HostPhase::Lobby
            || binding.config != self.image.config
            || binding.content != self.image.content
            || game != self.image.game
            || host.now != self.image.logical_ms
            || host.races != self.image.match_id
        {
            return Err(Error::Conflict);
        }
        for member in &self.image.members {
            if member.logical_peer < crate::transport::LOCAL_PEER_BASE
                && Some(network.migration_peer_key(member.logical_peer)?) != member.key
            {
                return Err(Error::Unauthorized);
            }
        }
        Ok(ConsentedLobby {
            descriptor: consent,
            image: self.image,
        })
    }
}

impl ConsentedLobby {
    pub fn descriptor(&self) -> &decented_net::migration::Descriptor {
        &self.descriptor
    }
    pub fn initial_hash(&self) -> Result<Hash> {
        state_hash(&self.image.encode(&self.descriptor.root)?)
    }
    pub fn logical_ms(&self) -> u64 {
        self.image.logical_ms
    }

    pub fn into_activation(
        self,
        network: crate::net::QuicClient,
        now_ms: u64,
    ) -> Result<LobbyActivation> {
        let (local, remote) = network.migration_keys()?;
        if local != self.descriptor.root.host || remote != self.descriptor.root.standby {
            return Err(Error::Unauthorized);
        }
        let policy = decented_net::migration::EndpointPolicy {
            descriptor: self.descriptor.clone(),
            role: decented_net::migration::LinkRole::Standby,
            promotion: None,
            membership: None,
        };
        let channel = network.migration_channel(&policy)?;
        let source = decented_net::migration::ActivationSource::new(
            self.descriptor,
            self.image,
            channel,
            now_ms,
        )?;
        Ok(LobbyActivation {
            source,
            network,
            ingress: Default::default(),
            local,
            remote,
            ready: false,
            failed: false,
        })
    }
}

/// Owns the admitted pair link during initial disclosure. No queued frame or
/// raw source escapes before admission is rechecked. Expiry/disconnect latches
/// failure, so a later UI call cannot resume an expired activation.
pub struct LobbyActivation {
    source: decented_net::migration::ActivationSource,
    network: crate::net::QuicClient,
    ingress: decented_net::migration::ReceiveQueue,
    local: [u8; 32],
    remote: [u8; 32],
    ready: bool,
    failed: bool,
}
pub struct PrimaryBootstrap {
    pub primary: decented_net::migration::Primary,
    pub armed: decented_net::migration::Armed,
    pub channel: decented_net::migration::Channel,
    pub network: crate::net::QuicClient,
    pub ingress: decented_net::migration::ReceiveQueue,
}
impl LobbyActivation {
    fn check_admission(&self) -> Result<()> {
        if self.failed {
            return Err(Error::Closed);
        }
        if self.network.migration_keys()? != (self.local, self.remote) {
            return Err(Error::Unauthorized);
        }
        Ok(())
    }
    pub fn initial_hash(&self) -> Result<Hash> {
        self.source.initial_hash()
    }
    pub fn acknowledge(
        &mut self,
        ack: decented_net::migration::CapabilityAck,
        now_ms: u64,
    ) -> Result<()> {
        let result = self
            .check_admission()
            .and_then(|()| self.source.acknowledge(ack, now_ms));
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn poll(&mut self, now_ms: u64) -> Result<bool> {
        use crate::transport::{ClientNetEvent, ClientTransport};
        let result = (|| {
            self.check_admission()?;
            for _ in 0..decented_net::migration::MAX_BATCH {
                let Some(event) = self.network.poll() else {
                    break;
                };
                match event {
                    ClientNetEvent::Reliable(bytes) => self.ingress.enqueue(bytes)?,
                    ClientNetEvent::Progress(_) => {}
                    _ => return Err(Error::Closed),
                }
            }
            for _ in 0..decented_net::migration::MAX_BATCH {
                // Once `Armed` is in, later frames (the standby's first
                // probe) stay queued for the armed primary, which owns them.
                if self.ready {
                    break;
                }
                let Some(ready) = self
                    .ingress
                    .poll(|bytes| self.source.receive(bytes, now_ms))?
                else {
                    break;
                };
                self.ready |= ready;
            }
            for _ in 0..decented_net::migration::MAX_BATCH {
                // Actual UTC is re-read before every release, including after
                // parsing/validation. No caller-supplied UTC authorizes data.
                self.check_admission()?;
                let Some(bytes) = self.source.poll_send(now_ms)? else {
                    break;
                };
                self.network.send_reliable(bytes);
            }
            Ok(self.ready)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn finish(self, now_ms: u64) -> Result<PrimaryBootstrap> {
        self.check_admission()?;
        if !self.ready {
            return Err(Error::Gap);
        }
        let armed = self.source.armed().ok_or(Error::Gap)?.clone();
        let (primary, channel) = self.source.finish(now_ms)?;
        Ok(PrimaryBootstrap {
            primary,
            armed,
            channel,
            network: self.network,
            ingress: self.ingress,
        })
    }
}

fn check_standby_admission(
    network: &crate::net::QuicHost,
    descriptor: &decented_net::migration::Descriptor,
    peer: PeerKey,
    failed: bool,
) -> Result<()> {
    if failed {
        return Err(Error::Closed);
    }
    if network
        .handle()
        .ok_or(Error::Closed)?
        .runtime_key()
        .to_bytes()
        != descriptor.root.standby
        || network.migration_peer_key(peer)? != descriptor.root.host
    {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

/// Per-peer budget for roster (non-pair) traffic held while armed. A
/// prepared control link carries nothing before `Promote`, so this is slack
/// for at most a few early frames.
pub const PEER_PENDING_RECORDS: usize = 32;
pub const PEER_PENDING_BYTES: usize = 16 * 1024;

/// The spec's batch cadence: one batch spans at most this much game time.
pub const MAX_BATCH_GAME_MS: u64 = 50;
/// The spec's clock budget: a driver clock never runs further than this past
/// the last committed batch. The armed original's driver is paced to it
/// (`LivePrimary::pace`, a soft, self-clearing pause); the successor's
/// driver past it is skew, which ends the session with a diagnostic.
pub const MAX_GAME_GAP_MS: u64 = 250;
/// The armed original's soft behind pause clears once commits have moved
/// game time on for this long with no stop of `FREEZE_AFTER_MS`
/// (`LivePrimary::behind`).
pub const BEHIND_CLEAR_MS: u64 = 250;

/// The game time of the next batch for a driver clock at `now`, after the
/// last committed batch at `committed`: at most [`MAX_BATCH_GAME_MS`] past
/// it. A slow frame's elapsed time is split into back-to-back batches (each
/// a full stage and commit; the rest is staged by the following calls, never
/// dropped). A clock behind the commit is left to the batch bounds.
fn batch_time(committed: u64, now: u64) -> u64 {
    now.min(committed + MAX_BATCH_GAME_MS)
}

/// As [`batch_time`], refusing a gap over [`MAX_GAME_GAP_MS`] as skew (the
/// successor, whose commits are local).
fn next_batch_time(committed: u64, now: u64) -> Result<u64> {
    if now.saturating_sub(committed) > MAX_GAME_GAP_MS {
        return Err(Error::Invalid("game clock skew over 250 ms"));
    }
    Ok(batch_time(committed, now))
}

/// Listener events from peers other than the pair link (roster control
/// links), held in arrival order until a consumer takes them. Each peer has
/// its own bounded budget: overflow drops that peer's held events and asks
/// the caller to close its link (no ban). It never fails the holder, so one
/// roster member cannot latch the standby or stall the original's commits.
/// A peer's `Left` drops its held events with it when its `Joined` is still
/// held, so a flapping link holds nothing.
#[derive(Default)]
struct BootstrapEvents {
    events: std::collections::VecDeque<crate::transport::HostNetEvent>,
    bytes: usize,
    /// Links closed for overflowing their budget.
    overflowed: usize,
    /// Peers closed for overflow whose `Left` has not arrived: their later
    /// frames are dropped, not held. Bounded by the listener's own links.
    closed: std::collections::BTreeSet<PeerKey>,
}
impl BootstrapEvents {
    fn peer_of(event: &crate::transport::HostNetEvent) -> PeerKey {
        use crate::transport::HostNetEvent;
        match event {
            HostNetEvent::Joined { peer, .. }
            | HostNetEvent::Left { peer }
            | HostNetEvent::Reliable { peer, .. }
            | HostNetEvent::Lossy { peer, .. } => *peer,
        }
    }
    fn size(event: &crate::transport::HostNetEvent) -> usize {
        use crate::transport::HostNetEvent;
        match event {
            HostNetEvent::Reliable { bytes, .. } | HostNetEvent::Lossy { bytes, .. } => bytes.len(),
            _ => 0,
        }
    }
    fn drop_peer(&mut self, peer: PeerKey) {
        let mut bytes = self.bytes;
        self.events.retain(|e| {
            let keep = Self::peer_of(e) != peer;
            if !keep {
                bytes -= Self::size(e);
            }
            keep
        });
        self.bytes = bytes;
    }
    /// Hold `event`. Returns a peer whose link the caller must close (no
    /// ban) because its budget overflowed.
    fn push(&mut self, event: crate::transport::HostNetEvent) -> Option<PeerKey> {
        use crate::transport::HostNetEvent;
        let peer = Self::peer_of(&event);
        if self.closed.contains(&peer) {
            if let HostNetEvent::Left { .. } = event {
                self.closed.remove(&peer);
            }
            return None;
        }
        if let HostNetEvent::Left { .. } = event {
            let joined = self
                .events
                .iter()
                .any(|e| matches!(e, HostNetEvent::Joined { peer: p, .. } if *p == peer));
            if joined {
                self.drop_peer(peer);
                return None;
            }
        }
        let (mut records, mut bytes) = (1usize, Self::size(&event));
        for held in self.events.iter().filter(|e| Self::peer_of(e) == peer) {
            records += 1;
            bytes += Self::size(held);
        }
        if records > PEER_PENDING_RECORDS
            || bytes > PEER_PENDING_BYTES
            || self.events.len() >= decented_net::migration::MAX_JOURNAL_RECORDS
            || self.bytes + Self::size(&event) > decented_net::migration::MAX_JOURNAL_BYTES
        {
            self.drop_peer(peer);
            self.overflowed += 1;
            if self.closed.len() >= decented_net::migration::MAX_PLAYERS * 4 {
                self.closed.pop_first();
            }
            self.closed.insert(peer);
            return Some(peer);
        }
        self.bytes += Self::size(&event);
        self.events.push_back(event);
        None
    }
}

/// Receiving half of the same explicit consent handoff. It owns the admitted
/// listener, so expired or lost pair admission cannot enter native restore.
/// Early roster-control events stay bounded and ordered for the armed runner.
pub struct LobbyStandby {
    descriptor: decented_net::migration::Descriptor,
    network: crate::net::QuicHost,
    peer: PeerKey,
    sink: decented_net::migration::ActivationSink,
    ingress: decented_net::migration::ReceiveQueue,
    pending: BootstrapEvents,
    started_ms: u64,
    failed: bool,
}
impl LobbyStandby {
    /// Retained physical bytes, useful for local setup diagnostics.
    pub fn buffered_bytes(&self) -> usize {
        self.ingress.queued_bytes()
    }
    pub fn new(
        descriptor: decented_net::migration::Descriptor,
        network: crate::net::QuicHost,
        peer: PeerKey,
        now_ms: u64,
    ) -> Result<Self> {
        descriptor.verify()?;
        check_standby_admission(&network, &descriptor, peer, false)?;
        let channel = network.migration_channel(peer)?;
        let sink =
            decented_net::migration::ActivationSink::new(descriptor.clone(), channel, now_ms)?;
        Ok(Self {
            descriptor,
            network,
            peer,
            sink,
            ingress: Default::default(),
            pending: Default::default(),
            started_ms: now_ms,
            failed: false,
        })
    }
    pub fn poll(&mut self, now_ms: u64) -> Result<bool> {
        use crate::transport::{HostNetEvent, HostTransport};
        let result = (|| {
            check_standby_admission(&self.network, &self.descriptor, self.peer, self.failed)?;
            self.sink.poll(now_ms)?;
            for _ in 0..decented_net::migration::MAX_BATCH {
                let Some(event) = self.network.poll() else {
                    break;
                };
                match event {
                    HostNetEvent::Reliable { peer, bytes } if peer == self.peer => {
                        self.ingress.enqueue(bytes)?
                    }
                    HostNetEvent::Left { peer } if peer == self.peer => return Err(Error::Closed),
                    HostNetEvent::Lossy { peer, .. } if peer == self.peer => {
                        return Err(Error::Unauthorized)
                    }
                    event => {
                        if let Some(peer) = self.pending.push(event) {
                            self.network.disconnect(peer);
                        }
                    }
                }
            }
            for _ in 0..decented_net::migration::MAX_BATCH {
                check_standby_admission(&self.network, &self.descriptor, self.peer, false)?;
                let Some(_) = self
                    .ingress
                    .poll(|bytes| self.sink.receive(bytes, now_ms))?
                else {
                    break;
                };
            }
            Ok(self.sink.ready())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn arm(self, keys: &decented_net::Keys, now_ms: u64) -> Result<ArmedLobbyStandby> {
        check_standby_admission(&self.network, &self.descriptor, self.peer, self.failed)?;
        let root = &self.descriptor.root;
        let (sequencer, armed, channel) = self.sink.arm(keys, now_ms, |bytes| {
            check_standby_admission(&self.network, &self.descriptor, self.peer, false)?;
            let image = decented_net::migration::SessionImage::decode(root, bytes)?;
            let validated = validate_initial(root, &image)?;
            check_standby_admission(&self.network, &self.descriptor, self.peer, false)?;
            Ok(validated)
        })?;
        // Native validation may take time. A private signature produced during
        // it is discarded if the existing actual-UTC admission has expired.
        check_standby_admission(&self.network, &self.descriptor, self.peer, false)?;
        Ok(ArmedLobbyStandby {
            descriptor: self.descriptor,
            network: self.network,
            peer: self.peer,
            sequencer,
            armed,
            channel,
            ingress: self.ingress,
            pending: self.pending,
            started_ms: self.started_ms,
            last_ms: now_ms,
            failed: false,
        })
    }
}

/// The initial certificate stays behind the same admission gate until its
/// framed bytes have been released. Handoff preserves the listener, channel
/// counters and any earlier roster-control traffic.
pub struct ArmedLobbyStandby {
    descriptor: decented_net::migration::Descriptor,
    network: crate::net::QuicHost,
    peer: PeerKey,
    sequencer: decented_net::migration::Sequencer,
    armed: decented_net::migration::Armed,
    channel: decented_net::migration::Channel,
    ingress: decented_net::migration::ReceiveQueue,
    pending: BootstrapEvents,
    started_ms: u64,
    last_ms: u64,
    failed: bool,
}
pub struct StandbyBootstrap {
    pub original_peer: PeerKey,
    pub sequencer: decented_net::migration::Sequencer,
    pub armed: decented_net::migration::Armed,
    pub channel: decented_net::migration::Channel,
    pub network: crate::net::QuicHost,
    pub ingress: decented_net::migration::ReceiveQueue,
    pub pending: std::collections::VecDeque<crate::transport::HostNetEvent>,
}
impl ArmedLobbyStandby {
    pub fn poll(&mut self, now_ms: u64) -> Result<bool> {
        use crate::transport::{HostNetEvent, HostTransport};
        let result = (|| {
            check_standby_admission(&self.network, &self.descriptor, self.peer, self.failed)?;
            self.check_clock(now_ms)?;
            self.last_ms = now_ms;
            self.channel.expire(now_ms)?;
            for _ in 0..decented_net::migration::MAX_BATCH {
                let Some(event) = self.network.poll() else {
                    break;
                };
                match event {
                    HostNetEvent::Reliable { peer, bytes } if peer == self.peer => {
                        self.ingress.enqueue(bytes)?
                    }
                    event => {
                        if let Some(peer) = self.pending.push(event) {
                            self.network.disconnect(peer);
                        }
                    }
                }
            }
            for _ in 0..decented_net::migration::MAX_BATCH {
                check_standby_admission(&self.network, &self.descriptor, self.peer, false)?;
                let Some(bytes) = self.channel.poll_send(now_ms)? else {
                    break;
                };
                self.network.send_reliable(self.peer, bytes);
            }
            Ok(self.channel.queued_bytes() == 0)
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn check_clock(&self, now_ms: u64) -> Result<()> {
        if now_ms < self.last_ms || now_ms - self.started_ms >= 5000 {
            return Err(Error::Closed);
        }
        Ok(())
    }
    pub fn finish(mut self, now_ms: u64) -> Result<StandbyBootstrap> {
        check_standby_admission(&self.network, &self.descriptor, self.peer, self.failed)?;
        self.check_clock(now_ms)?;
        self.channel.expire(now_ms)?;
        if self.channel.queued_bytes() != 0 {
            return Err(Error::Gap);
        }
        Ok(StandbyBootstrap {
            original_peer: self.peer,
            sequencer: self.sequencer,
            armed: self.armed,
            channel: self.channel,
            network: self.network,
            ingress: self.ingress,
            pending: self.pending.events,
        })
    }
}

/// Why a room cannot be backed up when the host's app and this device's app
/// differ: the checkpoint layout (and the game protocol) must match exactly,
/// or the standby could not restore the room at takeover. `None` if they match.
/// Arming is refused up front on either mismatch, with this text.
pub fn version_refusal(root: &decented_net::migration::Root) -> Option<&'static str> {
    (root.game_version != crate::proto::PROTO_VERSION || root.checkpoint_version != SCHEMA).then_some(
        "This device and the host run different versions of the app, so the room cannot be backed up. Update both to the same version.",
    )
}

/// Called only after the selected device's explicit UI consent. This checks
/// the admitted live link as well as the host signature and current game/code.
/// It grants access to this checkpoint profile, never persona/private keys.
pub fn consent_lobby_offer(
    network: &crate::net::QuicClient,
    identity: &decented_net::Identity,
    code: &str,
    bytes: &[u8],
) -> Result<decented_net::migration::Descriptor> {
    let (local, original) = network.migration_keys()?;
    let mut descriptor = decented_net::migration::Descriptor::decode(bytes)?;
    let root = &descriptor.root;
    if identity.pubkey().to_bytes() != local
        || root.standby != local
        || root.host != original
        || root.controller != original
        || root.app != crate::net::APP_TAG
        || root.game_version != crate::proto::PROTO_VERSION
        || root.checkpoint_version != SCHEMA
        || root.code != decented_net::migration::room_code_hash(code)?
        || descriptor.standby_signature != [0; 64]
    {
        return Err(Error::Unauthorized);
    }
    descriptor.consent(identity.keys())?;
    Ok(descriptor)
}

fn validate_seats(
    host: &HostAuthority,
    seats: &[GameSeat],
    next_player: u64,
    session: Option<&decented_net::migration::SessionImage>,
) -> Result<()> {
    if next_player == 0 || seats.len() > crate::proto::MAX_SEATS || seats.len() != host.humans.len()
    {
        return Err(Error::Bounds);
    }
    let mut native = HashSet::new();
    let mut slots = HashSet::new();
    let mut peers = HashSet::new();
    let mut previous = None;
    for row in seats {
        if row.player >= next_player
            || previous.is_some_and(|p| p >= row.player)
            || row.seat as usize >= crate::proto::MAX_SEATS
            || row.native_id >= super::BOT_ID_BASE
            || !native.insert(row.native_id)
            || !slots.insert(row.seat)
            || !peers.insert(row.logical_peer)
        {
            return Err(Error::Conflict);
        }
        let human = host
            .humans
            .iter()
            .find(|h| h.id == row.native_id)
            .ok_or(Error::Gap)?;
        if human.peer != row.logical_peer
            || host.by_peer.get(&row.logical_peer) != Some(&row.native_id)
        {
            return Err(Error::Conflict);
        }
        previous = Some(row.player);
    }
    if let Some(session) = session {
        if session.next_player != next_player || session.members.len() < seats.len() {
            return Err(Error::Conflict);
        }
        // Every seat row is bound to its member by player. A member with no
        // seat row is a seatless member (a leaver kept for one batch because
        // its departure batch addressed it an output): it has no native
        // human and no native peer route.
        for row in seats {
            let member = session
                .members
                .iter()
                .find(|m| m.player == row.player)
                .ok_or(Error::Conflict)?;
            if row.seat != member.seat
                || row.logical_peer != member.logical_peer
                || member.key.is_none()
            {
                return Err(Error::Conflict);
            }
        }
        for member in &session.members {
            if seats.iter().any(|row| row.player == member.player) {
                continue;
            }
            if member.key.is_none()
                || host.by_peer.contains_key(&member.logical_peer)
                || host.humans.iter().any(|h| h.peer == member.logical_peer)
            {
                return Err(Error::Conflict);
            }
        }
    }
    Ok(())
}
/// Members of `session` with no seat row in `seats`: leavers kept for
/// exactly one batch after a departure batch that addressed them an output.
fn seatless<'a>(
    session: &'a decented_net::migration::SessionImage,
    seats: &[GameSeat],
) -> impl Iterator<Item = u64> + 'a {
    let seats = seats.iter().map(|row| row.player).collect::<Vec<_>>();
    session
        .members
        .iter()
        .map(|m| m.player)
        .filter(move |player| !seats.contains(player))
}
/// The seatless members of an encoded committed session image.
pub(super) fn seatless_players(
    root: &decented_net::migration::Root,
    image: &[u8],
) -> Result<Vec<u64>> {
    let session = decented_net::migration::SessionImage::decode(root, image)?;
    let saved: BatchImage = options()
        .deserialize(&session.game)
        .map_err(|_| Error::Invalid("race batch encoding"))?;
    Ok(seatless(&session, &saved.seats).collect())
}
fn validate_seat_transition(previous: &BatchImage, next: &BatchImage) -> Result<()> {
    if next.next_player < previous.next_player {
        return Err(Error::Conflict);
    }
    for row in &next.seats {
        match previous.seats.iter().find(|old| old.player == row.player) {
            Some(old) if old != row => return Err(Error::Conflict),
            None if row.player < previous.next_player => return Err(Error::Conflict),
            _ => {}
        }
    }
    Ok(())
}
fn successor_seats(host: &HostAuthority, previous: &BatchImage) -> Result<(Vec<GameSeat>, u64)> {
    let mut seats: Vec<_> = previous
        .seats
        .iter()
        .filter(|row| {
            host.humans
                .iter()
                .any(|h| h.id == row.native_id && h.peer == row.logical_peer)
        })
        .cloned()
        .collect();
    let mut next = previous.next_player;
    for human in &host.humans {
        if seats
            .iter()
            .any(|row| row.native_id == human.id && row.logical_peer == human.peer)
        {
            continue;
        }
        let seat = (0..crate::proto::MAX_SEATS as u8)
            .find(|seat| seats.iter().all(|row| row.seat != *seat))
            .ok_or(Error::Bounds)?;
        let player = next;
        next = next.checked_add(1).ok_or(Error::Bounds)?;
        seats.push(GameSeat {
            player,
            seat,
            native_id: human.id,
            logical_peer: human.peer,
        });
    }
    seats.sort_by_key(|row| row.player);
    validate_seats(host, &seats, next, None)?;
    Ok((seats, next))
}
/// Inputs have already passed Fray's contiguous-intent and persona gates.
/// Operations must modify only the supplied fresh authority. Each retry starts
/// from the exact same checkpoint, event bytes, RTT readings and logical time.
/// A complete native batch with verified human input hashes and delivery ACKs.
/// Neither the queued originals nor the visible game are consumed by staging.
pub struct QueuedBatch {
    batch: StagedBatch,
    session: decented_net::migration::SessionImage,
    marks: Vec<decented_net::migration::Watermark>,
    selected_views: Vec<(u64, u64)>,
}
impl QueuedBatch {
    pub fn selected_views(&self) -> &[(u64, u64)] {
        &self.selected_views
    }
    pub fn batch(&self) -> &StagedBatch {
        &self.batch
    }
    pub fn session(&self) -> &decented_net::migration::SessionImage {
        &self.session
    }
    pub fn prepare_actor(
        &self,
        actor: &mut impl decented_net::migration::PublicationActor,
    ) -> Result<()> {
        self.batch
            .prepare_actor(actor, self.session.clone(), self.marks.clone())
    }
    pub fn adopt_actor(
        self,
        actor: &mut impl decented_net::migration::PublicationActor,
        certified: &[decented_net::migration::CertifiedOutput],
    ) -> Result<(
        HostAuthority,
        Vec<decented_net::migration::CertifiedOutput>,
        Vec<Effect>,
    )> {
        self.batch.adopt_actor(actor, certified)
    }
}
pub fn stage_queued_batch(
    host: &HostAuthority,
    primary: &decented_net::migration::Primary,
    root: &decented_net::migration::Root,
    now: u64,
    queues: &[&decented_net::migration::PlayerQueue],
    rtts: &BTreeMap<PeerKey, Option<u32>>,
    operation: impl FnMut(&mut HostAuthority) -> Result<()>,
) -> Result<QueuedBatch> {
    try_stage_queued_batch(
        host,
        primary,
        root,
        now,
        queues,
        None,
        rtts,
        || Ok(()),
        operation,
    )
}
/// The promoted device's own seat as a local input source of the successor
/// after `Resume`. It has no player link: its reliable intents (contiguous
/// numbers from its own retained journal) and its input views are fed to the
/// native authority from the device itself, recorded in the seat's committed
/// input history, and its delivery watermark advances from the outputs its
/// own client applied. Fray's `LocalInputs` covers only keyless members, so
/// this route is game-side.
pub(super) struct OwnInputs {
    pub player: u64,
    pub originals: Vec<(u64, Vec<u8>)>,
    pub views: Vec<(u64, Vec<u8>)>,
    pub output_ack: u64,
}
/// `guard` runs before every fresh native attempt. A failing guard returns
/// at once, so no native attempt follows it.
#[allow(clippy::too_many_arguments)]
fn try_stage_queued_batch(
    host: &HostAuthority,
    primary: &decented_net::migration::Primary,
    root: &decented_net::migration::Root,
    now: u64,
    queues: &[&decented_net::migration::PlayerQueue],
    own: Option<&OwnInputs>,
    rtts: &BTreeMap<PeerKey, Option<u32>>,
    mut guard: impl FnMut() -> Result<()>,
    mut operation: impl FnMut(&mut HostAuthority) -> Result<()>,
) -> Result<QueuedBatch> {
    let mut limit = decented_net::migration::MAX_BATCH;
    loop {
        match stage_queued_batch_limited(
            host,
            primary,
            root,
            now,
            queues,
            own,
            rtts,
            &mut guard,
            &mut operation,
            limit,
        ) {
            Ok(batch) => return Ok(batch),
            Err(Error::Bounds) if limit > 0 => limit /= 2,
            Err(error) => return Err(error),
        }
    }
}
fn stage_queued_batch_limited(
    host: &HostAuthority,
    primary: &decented_net::migration::Primary,
    root: &decented_net::migration::Root,
    now: u64,
    queues: &[&decented_net::migration::PlayerQueue],
    own: Option<&OwnInputs>,
    rtts: &BTreeMap<PeerKey, Option<u32>>,
    guard: impl FnMut() -> Result<()>,
    operation: impl FnMut(&mut HostAuthority) -> Result<()>,
    limit: usize,
) -> Result<QueuedBatch> {
    use decented_net::migration::{Delivery, SessionImage, MAX_PLAYERS};
    if queues.len() > MAX_PLAYERS {
        return Err(Error::Bounds);
    }
    let base = SessionImage::decode(root, primary.committed_image())?;
    let saved: BatchImage = options()
        .deserialize(&base.game)
        .map_err(|_| Error::Invalid("race batch encoding"))?;
    let mut session = base.clone();
    let mut marks = primary.receipt().watermarks.clone();
    let mut ordered = BTreeMap::new();
    for queue in queues {
        queue.validate_for(root, primary)?;
        if ordered.insert(queue.player(), *queue).is_some() {
            return Err(Error::Conflict);
        }
    }
    // A seatless member left explicitly: nothing it queued after its Leave
    // is fed (a `Hello` would re-admit it), and its watermark stays as
    // committed until this batch retires it.
    let seated = |player: u64| saved.seats.iter().any(|row| row.player == player);
    ordered.retain(|player, _| seated(*player));
    let queues = queues
        .iter()
        .copied()
        .filter(|queue| seated(queue.player()))
        .collect::<Vec<_>>();
    let queues = queues.as_slice();
    let own = own.filter(|own| seated(own.player));
    let mut events = Vec::new();
    let mut selected_views = Vec::new();
    let (mut own_records, mut own_bytes) = (0usize, 0usize);
    if let Some(own) = own {
        if ordered.contains_key(&own.player) {
            return Err(Error::Conflict);
        }
        let member = base
            .members
            .iter()
            .find(|m| m.player == own.player && m.key == Some(root.standby))
            .ok_or(Error::Unauthorized)?;
        // A fair share: the seat takes at most one player's part of the
        // record and byte budgets, originals before views.
        let share = (limit / (queues.len() + 1)).max(1).min(limit);
        let byte_share = decented_net::migration::MAX_JOURNAL_BYTES / (queues.len() + 1);
        let history = session.inputs.get_mut(&own.player).ok_or(Error::Gap)?;
        for (number, bytes) in &own.originals {
            if own_records == share || own_bytes + bytes.len() > byte_share {
                break;
            }
            if history.record(*number, bytes)? {
                own_records += 1;
                own_bytes += bytes.len();
                events.push(crate::transport::HostNetEvent::Reliable {
                    peer: member.logical_peer,
                    bytes: bytes.clone(),
                });
            }
        }
        for (number, bytes) in &own.views {
            if own_records == share || own_bytes + bytes.len() > byte_share {
                break;
            }
            own_records += 1;
            own_bytes += bytes.len();
            selected_views.push((own.player, *number));
            events.push(crate::transport::HostNetEvent::Lossy {
                peer: member.logical_peer,
                bytes: bytes.clone(),
            });
        }
        let handled = history.handled();
        let mark = marks
            .iter_mut()
            .find(|m| m.player == own.player && !m.retired)
            .ok_or(Error::Unauthorized)?;
        if own.output_ack < mark.output_ack
            || own.output_ack > primary.output_ceiling(own.player, Delivery::Reliable)
        {
            return Err(Error::Invalid("own seat output ACK"));
        }
        mark.input = handled;
        mark.output_ack = own.output_ack;
    }
    for input in decented_net::migration::select_player_inputs(
        root,
        primary,
        queues,
        limit - own_records,
        decented_net::migration::MAX_JOURNAL_BYTES - own_bytes,
    )? {
        match input {
            decented_net::migration::SelectedPlayerInput::Reliable {
                player,
                number,
                bytes,
            } => {
                let member = base
                    .members
                    .iter()
                    .find(|m| m.player == player)
                    .ok_or(Error::Unauthorized)?;
                if session
                    .inputs
                    .get_mut(&player)
                    .ok_or(Error::Gap)?
                    .record(number, &bytes)?
                {
                    events.push(crate::transport::HostNetEvent::Reliable {
                        peer: member.logical_peer,
                        bytes,
                    });
                }
            }
            decented_net::migration::SelectedPlayerInput::Lossy {
                player,
                number,
                bytes,
            } => {
                let member = base
                    .members
                    .iter()
                    .find(|m| m.player == player)
                    .ok_or(Error::Unauthorized)?;
                selected_views.push((player, number));
                events.push(crate::transport::HostNetEvent::Lossy {
                    peer: member.logical_peer,
                    bytes,
                });
            }
        }
    }
    for (player, queue) in ordered {
        let history = session.inputs.get(&player).ok_or(Error::Gap)?;
        let mark = marks
            .iter_mut()
            .find(|m| m.player == player && !m.retired)
            .ok_or(Error::Unauthorized)?;
        let ack = queue.output_ack();
        if ack < mark.output_ack || ack > primary.output_ceiling(player, Delivery::Reliable) {
            return Err(Error::Invalid("human output ACK"));
        }
        mark.input = history.handled();
        mark.output_ack = ack;
    }
    let batch = stage_batch_guarded(host, primary, root, now, &events, rtts, guard, operation)?;
    // Ordinary authenticated inputs may remove their own seat. New admission
    // requires its separate membership/capability transaction.
    if batch.image.seats.iter().any(|seat| {
        !base.members.iter().any(|m| {
            m.player == seat.player && m.logical_peer == seat.logical_peer && m.seat == seat.seat
        })
    }) {
        return Err(Error::Unauthorized);
    }
    // An output to a player who departs in this batch is kept: the leaver
    // stays a seatless member (its watermark unretired) through this batch,
    // so the output enters the replicated outbox. The next batch addresses
    // it nothing and retires it here (the recipient-retirement tombstone).
    session.members.retain(|m| {
        batch.image.seats.iter().any(|seat| seat.player == m.player)
            || batch.image.effects.iter().any(|effect| {
                matches!(effect, Effect::Reliable { peer, .. } | Effect::Lossy { peer, .. }
                    if *peer == m.logical_peer)
            })
    });
    session
        .inputs
        .retain(|player, _| session.members.iter().any(|m| m.player == *player));
    for mark in &mut marks {
        if !session.members.iter().any(|m| m.player == mark.player) {
            mark.retired = true;
        }
    }
    if session.members != base.members {
        session.membership_revision = session
            .membership_revision
            .checked_add(1)
            .ok_or(Error::Bounds)?;
    }
    session.game = batch.encoded.clone();
    session.logical_ms = batch.image.logical_ms;
    session.match_id = batch.image.match_id;
    session.config = batch.image.binding.config;
    session.content = batch.image.binding.content;
    session.next_player = batch.image.next_player;
    session.validate_successor(root, &base, &marks, now)?;
    session.encode(root)?;
    Ok(QueuedBatch {
        batch,
        session,
        marks,
        selected_views,
    })
}

pub fn stage_batch(
    host: &HostAuthority,
    primary: &decented_net::migration::Primary,
    root: &decented_net::migration::Root,
    now: u64,
    events: &[crate::transport::HostNetEvent],
    rtts: &BTreeMap<PeerKey, Option<u32>>,
    operation: impl FnMut(&mut HostAuthority) -> Result<()>,
) -> Result<StagedBatch> {
    stage_batch_guarded(host, primary, root, now, events, rtts, || Ok(()), operation)
}
// A callback's Bounds is an authority/service fault, not evidence that a
// smaller input prefix can fit. Preserve the one-retry rule for those faults.
fn native_callback_error(error: Error) -> Error {
    if error == Error::Bounds {
        Error::Invalid("native callback bounds fault")
    } else {
        error
    }
}
#[allow(clippy::too_many_arguments)]
fn stage_batch_guarded(
    host: &HostAuthority,
    primary: &decented_net::migration::Primary,
    root: &decented_net::migration::Root,
    now: u64,
    events: &[crate::transport::HostNetEvent],
    rtts: &BTreeMap<PeerKey, Option<u32>>,
    mut guard: impl FnMut() -> Result<()>,
    mut operation: impl FnMut(&mut HostAuthority) -> Result<()>,
) -> Result<StagedBatch> {
    primary.receipt().verify(&root.context()?)?;
    if primary.is_closed() || primary.pending().is_some() {
        return Err(Error::Closed);
    }
    if now < host.now
        || now - host.now > 50
        || events.len() > decented_net::migration::MAX_JOURNAL_RECORDS
        || rtts.len() > crate::proto::MAX_SEATS
    {
        return Err(Error::Bounds);
    }
    let mut input_bytes = 0usize;
    for event in events {
        if let crate::transport::HostNetEvent::Reliable { bytes, .. }
        | crate::transport::HostNetEvent::Lossy { bytes, .. } = event
        {
            input_bytes = input_bytes.checked_add(bytes.len()).ok_or(Error::Bounds)?;
            if bytes.len() > decented_net::MAX_RELIABLE_LEN
                || input_bytes > decented_net::migration::MAX_JOURNAL_BYTES
            {
                return Err(Error::Bounds);
            }
        }
    }
    let (binding, base_bytes) = checkpoint(host)?;
    let base = decented_net::migration::SessionImage::decode(root, primary.committed_image())?;
    validate_batch(&base.game, &binding, host.now)?;
    let saved: BatchImage = options()
        .deserialize(&base.game)
        .map_err(|_| Error::Invalid("race batch encoding"))?;
    validate_seats(host, &saved.seats, saved.next_player, Some(&base))?;
    if base.config != binding.config
        || base.content != binding.content
        || base.logical_ms != host.now
        || base.match_id != host.races
        || primary.receipt().logical_ms != host.now
        || saved.authority != base_bytes
    {
        return Err(Error::Conflict);
    }
    for attempt in 0..2 {
        // Shared replay faults and the caller's admission are rechecked before
        // every fresh native attempt, including the retry.
        primary.sender_sequence().check_live()?;
        guard().map_err(native_callback_error)?;
        let capture = std::sync::Arc::new(std::sync::Mutex::new(Capture {
            events: events.iter().cloned().collect(),
            rtts: rtts.clone(),
            effects: Vec::new(),
            bytes: 0,
            overflow: false,
        }));
        let mut staged = restore(
            &base_bytes,
            &binding,
            Box::new(CaptureTransport(capture.clone())),
        )?;
        staged.migration_custom = Some(vec![]);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            operation(&mut staged).map_err(native_callback_error)?;
            staged.update(now);
            Ok::<_, Error>(())
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) if attempt == 1 => return Err(error),
            Err(_) if attempt == 1 => return Err(Error::Invalid("race authority batch panic")),
            _ => continue,
        }
        let mut capture = capture.lock().map_err(|_| Error::Closed)?;
        for bytes in staged.migration_custom.take().unwrap_or_default() {
            capture.push(Effect::InstallCustomTrack(bytes));
        }
        if capture.overflow {
            return Err(Error::Bounds);
        }
        if !capture.events.is_empty() {
            return Err(Error::Gap);
        }
        let (binding, authority) = checkpoint(&staged)?;
        let (seats, next_player) = successor_seats(&staged, &saved)?;
        let image = BatchImage {
            schema: SCHEMA,
            next_player,
            seats,
            match_id: staged.races,
            logical_ms: staged.now,
            binding,
            authority,
            effects: capture.effects.clone(),
        };
        validate_seat_transition(&saved, &image)?;
        let encoded = encode_batch(&image)?;
        return Ok(StagedBatch {
            image,
            encoded,
            base: primary.receipt().clone(),
        });
    }
    Err(Error::Invalid("race authority batch fault"))
}
/// Drain one original reliable input through Fray's causal recovery engine.
/// Fray owns the two-attempt fault policy; each callback restores exactly once.
/// Stampede has no inherited monotonic persona/presence cache. Its credential
/// checks remain on actual UTC in the transport. Prove exact native restore
/// before the mandatory Fray security boundary, without a game frame/effect.
pub fn recover_security(
    recovery: &mut decented_net::migration::Recovery,
    keys: &decented_net::Keys,
    now_ms: u64,
) -> Result<decented_net::migration::Receipt> {
    let root = recovery.root().clone();
    recovery.revalidate_security(keys, now_ms, |bytes, _| {
        let session = decented_net::migration::SessionImage::decode(&root, bytes)?;
        let saved: BatchImage = options()
            .deserialize(&session.game)
            .map_err(|_| Error::Invalid("race batch encoding"))?;
        validate_batch(&session.game, &saved.binding, session.logical_ms)?;
        Ok(bytes.to_vec())
    })
}

pub fn recover_next(
    recovery: &mut decented_net::migration::Recovery,
    keys: &decented_net::Keys,
    now_ms: u64,
) -> Result<decented_net::migration::Receipt> {
    use decented_net::migration::SessionImage;
    recovery.poll(now_ms)?;
    let root = recovery.root().clone();
    let previous = SessionImage::decode(&root, recovery.sequencer().image())?;
    let receipt = recovery.sequencer().receipt().clone();
    let marks = recovery.reconciled_watermarks();
    let ceilings = recovery_ceilings(recovery);
    recovery.apply_next(keys, |player, _, bytes, histories| {
        let member = previous
            .members
            .iter()
            .find(|m| m.player == player)
            .ok_or(Error::Unauthorized)?;
        frozen_candidate(
            &root,
            &previous,
            &receipt,
            &marks,
            &ceilings,
            &[crate::transport::HostNetEvent::Reliable {
                peer: member.logical_peer,
                bytes: bytes.to_vec(),
            }],
            &[],
            histories,
        )
    })
}
/// Missing peers leave only after Fray has drained all surviving input tails.
pub fn recover_departures(
    recovery: &mut decented_net::migration::Recovery,
    keys: &decented_net::Keys,
    now_ms: u64,
) -> Result<decented_net::migration::Receipt> {
    use decented_net::migration::SessionImage;
    recovery.poll(now_ms)?;
    let root = recovery.root().clone();
    let previous = SessionImage::decode(&root, recovery.sequencer().image())?;
    let receipt = recovery.sequencer().receipt().clone();
    let marks = recovery.reconciled_watermarks();
    let ceilings = recovery_ceilings(recovery);
    let saved: BatchImage = options()
        .deserialize(&previous.game)
        .map_err(|_| Error::Invalid("race recovery image"))?;
    recovery.commit_departures(keys, |absent, _, histories| {
        // An absent seatless member has no native seat to leave: it departs
        // through the same removal and retirement, with no native event.
        let (seated, departing): (Vec<u64>, Vec<u64>) = absent
            .iter()
            .partition(|player| saved.seats.iter().any(|row| row.player == **player));
        let events = seated
            .iter()
            .map(|player| {
                let member = previous
                    .members
                    .iter()
                    .find(|m| m.player == *player)
                    .ok_or(Error::Unauthorized)?;
                Ok(crate::transport::HostNetEvent::Left {
                    peer: member.logical_peer,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        frozen_candidate(
            &root, &previous, &receipt, &marks, &ceilings, &events, &departing, histories,
        )
    })
}
/// A recovery replay record for a local survivor must verify, and must be
/// certified at or before the closed frontier (term 0) or by this recovery
/// (term 1 after the final receipt), never ahead of the current committed
/// receipt. The same check as Twang's local-survivor replay.
fn check_recovery_record(
    context: &decented_net::migration::Context,
    promotion: &decented_net::migration::Promotion,
    receipt: &decented_net::migration::Receipt,
    record: &decented_net::migration::CertifiedOutput,
) -> Result<()> {
    record.verify(context)?;
    if record.receipt.term == 1 && record.receipt.index <= promotion.final_receipt.index
        || record.receipt.index > receipt.index
        || record.receipt.term > receipt.term
        || record.receipt.logical_ms > receipt.logical_ms
        || (record.receipt.index == receipt.index
            && record.receipt.digest(context)? != receipt.digest(context)?)
    {
        return Err(Error::Conflict);
    }
    Ok(())
}
fn recovery_ceilings(recovery: &decented_net::migration::Recovery) -> BTreeMap<(u64, u8), u64> {
    use decented_net::migration::Delivery;
    recovery
        .sequencer()
        .receipt()
        .watermarks
        .iter()
        .flat_map(|m| {
            [Delivery::Reliable, Delivery::Lossy]
                .into_iter()
                .map(move |delivery| {
                    (
                        (m.player, u8::from(delivery == Delivery::Lossy)),
                        recovery.sequencer().stream_ceiling(m.player, delivery),
                    )
                })
        })
        .collect()
}
#[allow(clippy::too_many_arguments)]
fn frozen_candidate(
    root: &decented_net::migration::Root,
    previous: &decented_net::migration::SessionImage,
    receipt: &decented_net::migration::Receipt,
    marks: &[decented_net::migration::Watermark],
    ceilings: &BTreeMap<(u64, u8), u64>,
    events: &[crate::transport::HostNetEvent],
    departing: &[u64],
    histories: &BTreeMap<u64, decented_net::migration::HandledInputs>,
) -> Result<(Vec<u8>, Vec<decented_net::migration::Output>)> {
    use decented_net::migration::{Candidate, Delivery, Output};
    let saved: BatchImage = options()
        .deserialize(&previous.game)
        .map_err(|_| Error::Invalid("race recovery image"))?;
    validate_batch(&previous.game, &saved.binding, previous.logical_ms)?;
    let capture = std::sync::Arc::new(std::sync::Mutex::new(Capture {
        events: Default::default(),
        rtts: Default::default(),
        effects: vec![],
        bytes: 0,
        overflow: false,
    }));
    let mut host = restore(
        &saved.authority,
        &saved.binding,
        Box::new(CaptureTransport(capture.clone())),
    )?;
    validate_seats(&host, &saved.seats, saved.next_player, Some(previous))?;
    host.migration_custom = Some(vec![]);
    host.migration_recover_events(events)?;
    let mut captured = capture.lock().map_err(|_| Error::Closed)?;
    for bytes in host.migration_custom.take().unwrap_or_default() {
        captured.push(Effect::InstallCustomTrack(bytes));
    }
    if captured.overflow {
        return Err(Error::Bounds);
    }
    let (binding, authority) = checkpoint(&host)?;
    let (seats, next_player) = successor_seats(&host, &saved)?;
    if next_player != saved.next_player
        || host.now != previous.logical_ms
        || host.races != previous.match_id
    {
        return Err(Error::Conflict);
    }
    let image = BatchImage {
        schema: SCHEMA,
        match_id: host.races,
        logical_ms: host.now,
        binding: binding.clone(),
        authority,
        seats,
        next_player,
        effects: std::mem::take(&mut captured.effects),
    };
    // A seatless member (kept by its departure batch for an output) stays
    // through the drain records; only the departure commit, which lists it
    // in `departing`, removes and retires it.
    let carried = seatless(previous, &saved.seats).collect::<Vec<_>>();
    let mut session = previous.clone();
    session.members.retain(|m| {
        image.seats.iter().any(|s| s.player == m.player)
            || (carried.contains(&m.player) && !departing.contains(&m.player))
            || image.effects.iter().any(|effect| {
                matches!(effect, Effect::Reliable { peer, .. } | Effect::Lossy { peer, .. }
                    if *peer == m.logical_peer)
            })
    });
    session.inputs = histories
        .iter()
        .filter(|(p, _)| session.members.iter().any(|m| m.player == **p))
        .map(|(p, h)| (*p, h.clone()))
        .collect();
    session.membership_revision = previous
        .membership_revision
        .checked_add(u64::from(session.members != previous.members))
        .ok_or(Error::Bounds)?;
    session.game = encode_batch(&image)?;
    let mut marks = marks.to_vec();
    for mark in &mut marks {
        if let Some(history) = histories.get(&mark.player) {
            mark.input = history.handled();
        }
        if !session.members.iter().any(|m| m.player == mark.player) {
            mark.retired = true;
        }
    }
    let mut allocated = ceilings.clone();
    let mut outputs = vec![];
    for effect in &image.effects {
        let (peer, bytes, delivery) = match effect {
            Effect::Reliable { peer, bytes } => (*peer, bytes, Delivery::Reliable),
            Effect::Lossy { peer, bytes } => (*peer, bytes, Delivery::Lossy),
            _ => continue,
        };
        let recipient = session
            .members
            .iter()
            .find(|m| m.logical_peer == peer)
            .ok_or(Error::Unauthorized)?
            .player;
        let sequence = allocated
            .entry((recipient, u8::from(delivery == Delivery::Lossy)))
            .or_default();
        *sequence = sequence.checked_add(1).ok_or(Error::Bounds)?;
        outputs.push(Output {
            recipient,
            delivery,
            sequence: *sequence,
            bytes: bytes.clone(),
        });
    }
    session.validate_successor(root, previous, &marks, receipt.logical_ms)?;
    let candidate = Candidate {
        // Validation-only envelope; the owning actor allocates the wire ID.
        sequence: 1,
        term: 1,
        index: receipt.index.checked_add(1).ok_or(Error::Bounds)?,
        parent: receipt.digest(&root.context()?)?,
        logical_ms: receipt.logical_ms,
        image: session.encode(root)?,
        watermarks: marks,
        outputs,
    };
    validate_candidate_in(root, previous, &candidate, true)?;
    Ok((candidate.image, candidate.outputs))
}

/// The standby validates actual fresh game restoration as well as canonical
/// runner queues before it installs the enclosing Fray session candidate.
pub fn validate_batch(bytes: &[u8], expected: &Binding, logical_ms: u64) -> Result<Vec<u8>> {
    if bytes.len() > MAX_IMAGE {
        return Err(Error::Bounds);
    }
    let image: BatchImage = options()
        .deserialize(bytes)
        .map_err(|_| Error::Invalid("race batch encoding"))?;
    if image.schema != SCHEMA || image.binding != *expected || encode_batch(&image)? != bytes {
        return Err(Error::Invalid("race batch binding/canonical bytes"));
    }
    let mut budget = Capture {
        events: Default::default(),
        rtts: Default::default(),
        effects: vec![],
        bytes: 0,
        overflow: false,
    };
    for effect in &image.effects {
        budget.push(effect.clone());
        if let Effect::InstallCustomTrack(bytes) = effect {
            let track = CustomTrack::from_bytes(bytes)
                .map_err(|_| Error::Invalid("deferred custom content"))?;
            let selected = SelectedTrack::new(track)
                .map_err(|_| Error::Invalid("deferred custom geometry"))?;
            if selected.bytes != *bytes {
                return Err(Error::Invalid("deferred noncanonical track"));
            }
        }
    }
    if budget.overflow {
        return Err(Error::Bounds);
    }
    let host = restore(&image.authority, expected, null_transport())?;
    validate_seats(&host, &image.seats, image.next_player, None)?;
    if host.now != logical_ms || image.logical_ms != logical_ms || host.races != image.match_id {
        return Err(Error::Invalid("race batch logical time/match"));
    }
    Ok(bytes.to_vec())
}
fn validate_outputs(
    effects: &[Effect],
    members: &[decented_net::migration::SessionMember],
    outputs: &[decented_net::migration::Output],
) -> Result<()> {
    use decented_net::migration::Delivery;
    let mut outputs = outputs.iter();
    for effect in effects {
        let (peer, bytes, delivery) = match effect {
            Effect::Reliable { peer, bytes } => (*peer, bytes, Delivery::Reliable),
            Effect::Lossy { peer, bytes } => (*peer, bytes, Delivery::Lossy),
            _ => continue,
        };
        let player = members
            .iter()
            .find(|m| m.logical_peer == peer)
            .ok_or(Error::Unauthorized)?
            .player;
        let output = outputs.next().ok_or(Error::Gap)?;
        if output.recipient != player || output.delivery != delivery || output.bytes != *bytes {
            return Err(Error::Conflict);
        }
    }
    if outputs.next().is_some() {
        return Err(Error::Conflict);
    }
    Ok(())
}
pub fn validate_candidate(
    root: &decented_net::migration::Root,
    previous: &decented_net::migration::SessionImage,
    candidate: &decented_net::migration::Candidate,
) -> Result<Vec<u8>> {
    validate_candidate_in(root, previous, candidate, false)
}
/// As [`validate_candidate`]. `recovering` is set only for a frozen recovery
/// drain record ([`frozen_candidate`]), which may carry a seatless member
/// over until the departure commit retires it.
fn validate_candidate_in(
    root: &decented_net::migration::Root,
    previous: &decented_net::migration::SessionImage,
    candidate: &decented_net::migration::Candidate,
    recovering: bool,
) -> Result<Vec<u8>> {
    validate_candidate_phase(root, previous, candidate, recovering).map(|(image, _)| image)
}
/// Whether game time advances in a committed authority, for Fray's failure
/// watches (`Activity`). Every phase whose timer runs advances: `Racing`
/// (the grid countdown and the race) and `Results`, where the host still
/// steps the world every tick (`HostAuthority::update` -> `step_world`) and
/// the rematch 3-2-1 runs. The lobby has no timer (an automatic start begins
/// the race at once) and is idle, and so is the armed room before its first
/// armed commit (receipt index 0, the switch window).
pub(super) fn activity_of(
    index: u64,
    phase: super::HostPhase,
) -> decented_net::migration::Activity {
    use decented_net::migration::Activity;
    if index > 0 && phase != super::HostPhase::Lobby {
        Activity::Advancing
    } else {
        Activity::Idle
    }
}
/// As [`validate_candidate_in`], also returning the candidate authority's
/// phase from the restore validation already does, so the standby derives
/// its activity from the checkpoint it installs without a second restore.
pub(super) fn validate_candidate_phase(
    root: &decented_net::migration::Root,
    previous: &decented_net::migration::SessionImage,
    candidate: &decented_net::migration::Candidate,
    recovering: bool,
) -> Result<(Vec<u8>, super::HostPhase)> {
    let image = decented_net::migration::SessionImage::decode(root, &candidate.image)?;
    let binding = Binding {
        config: image.config,
        content: image.content,
    };
    validate_batch(&image.game, &binding, candidate.logical_ms)?;
    if image.logical_ms != candidate.logical_ms {
        return Err(Error::Conflict);
    }
    let saved: BatchImage = options()
        .deserialize(&image.game)
        .map_err(|_| Error::Invalid("race batch encoding"))?;
    let old: BatchImage = options()
        .deserialize(&previous.game)
        .map_err(|_| Error::Invalid("race previous batch encoding"))?;
    previous.validate(root)?;
    validate_seats(
        &restore(&old.authority, &old.binding, null_transport())?,
        &old.seats,
        old.next_player,
        Some(previous),
    )?;
    let restored = restore(&saved.authority, &saved.binding, null_transport())?;
    validate_seats(&restored, &saved.seats, saved.next_player, Some(&image))?;
    validate_seatless(previous, &old, &image, &saved, recovering)?;
    validate_seat_transition(&old, &saved)?;
    if saved.match_id != image.match_id {
        return Err(Error::Invalid("race session match binding"));
    }
    validate_outputs(&saved.effects, &image.members, &candidate.outputs)?;
    Ok((candidate.image.clone(), restored.phase()))
}
/// A seatless member lives exactly one ordinary batch: it was seated in the
/// previous image and this batch addresses it an output (its departure
/// batch); the next batch removes and retires it. Only a recovery's frozen
/// drain records carry a seatless member over, still seatless, until the
/// departure commit retires it; live batches of either term hold the
/// one-batch bound.
///
/// So every member seatless in `previous` is absent from the candidate,
/// seated or not, except in that carry-over: a hostile original cannot seat
/// it again and undo its departure. The seatless set is read from the
/// images' own seat rows (members with no row), which the caller has
/// already checked against both restored authorities: no extra restore and
/// no cache.
fn validate_seatless(
    previous: &decented_net::migration::SessionImage,
    old: &BatchImage,
    image: &decented_net::migration::SessionImage,
    saved: &BatchImage,
    recovering: bool,
) -> Result<()> {
    let carried = seatless(previous, &old.seats).collect::<Vec<_>>();
    // Seated again: refused in every case, the carry-over included. Still
    // seatless outside a recovery drain record: refused below.
    if carried.iter().any(|player| {
        image.members.iter().any(|m| m.player == *player)
            && saved.seats.iter().any(|row| row.player == *player)
    }) {
        return Err(Error::Invalid("race seatless member re-seated"));
    }
    for player in seatless(image, &saved.seats) {
        if recovering && carried.contains(&player) {
            continue;
        }
        let peer = image
            .members
            .iter()
            .find(|m| m.player == player)
            .ok_or(Error::Gap)?
            .logical_peer;
        let addressed = saved.effects.iter().any(|effect| {
            matches!(effect, Effect::Reliable { peer: to, .. } | Effect::Lossy { peer: to, .. }
                if *to == peer)
        });
        if !old.seats.iter().any(|row| row.player == player) || !addressed {
            return Err(Error::Invalid("race seatless member"));
        }
    }
    Ok(())
}
pub fn validate_initial(
    root: &decented_net::migration::Root,
    image: &decented_net::migration::SessionImage,
) -> Result<Vec<u8>> {
    image.validate_initial(root)?;
    let saved: BatchImage = options()
        .deserialize(&image.game)
        .map_err(|_| Error::Invalid("race initial batch encoding"))?;
    if !saved.effects.is_empty() {
        return Err(Error::Conflict);
    }
    let candidate = decented_net::migration::Candidate {
        // Validation-only envelope; the owning actor allocates the wire ID.
        sequence: 1,
        term: 0,
        index: 0,
        parent: [0; 32],
        logical_ms: image.logical_ms,
        image: image.encode(root)?,
        watermarks: image
            .members
            .iter()
            .map(|m| decented_net::migration::Watermark {
                player: m.player,
                input: 0,
                output_ack: 0,
                retired: false,
            })
            .collect(),
        outputs: vec![],
    };
    validate_candidate(root, image, &candidate)
}

/// Restore the complete native authority from a committed session image,
/// without a transport. Used to compare a runtime with its committed image.
pub fn committed_authority(
    root: &decented_net::migration::Root,
    image: &[u8],
) -> Result<HostAuthority> {
    let session = decented_net::migration::SessionImage::decode(root, image)?;
    let binding = Binding {
        config: session.config,
        content: session.content,
    };
    validate_batch(&session.game, &binding, session.logical_ms)?;
    let saved: BatchImage = options()
        .deserialize(&session.game)
        .map_err(|_| Error::Invalid("race batch encoding"))?;
    let authority = restore(&saved.authority, &saved.binding, null_transport())?;
    validate_seats(&authority, &saved.seats, saved.next_player, Some(&session))?;
    Ok(authority)
}

#[cfg(test)]
mod tests {
    use super::super::Human;
    use super::*;
    use crate::{HostConfig, NullHostTransport};
    use std::collections::VecDeque;

    /// Every phase whose timer runs is `Advancing`: the race (with its grid
    /// countdown) and the results, where the world still steps and the
    /// rematch 3-2-1 runs. Only the lobby, and index 0 (the switch window),
    /// are `Idle`.
    #[test]
    fn activity_follows_every_running_timer() {
        use super::super::HostPhase;
        use decented_net::migration::Activity;
        assert_eq!(activity_of(3, HostPhase::Lobby), Activity::Idle);
        assert_eq!(activity_of(3, HostPhase::Racing), Activity::Advancing);
        assert_eq!(activity_of(3, HostPhase::Results), Activity::Advancing);
        for phase in [HostPhase::Lobby, HostPhase::Racing, HostPhase::Results] {
            assert_eq!(activity_of(0, phase), Activity::Idle);
        }
    }

    fn host() -> HostAuthority {
        let cfg = HostConfig {
            bots: 8,
            laps: 1,
            seed: 42,
            auto_start: false,
            ..HostConfig::default()
        };
        HostAuthority::new_headless(Box::new(NullHostTransport), cfg)
    }
    fn add_human(host: &mut HostAuthority, id: u8, peer: PeerKey) {
        host.humans.push(Human {
            id,
            peer,
            name: format!("Player {id}"),
            character: 0,
            attire: [0; 3],
            ready: false,
            custom_ready: None,
            host: false,
            last_heard: 0,
            racer: None,
            inq: BTreeMap::new(),
            last_fed: [0; 4],
            applied_seq: 0,
            starved: VecDeque::new(),
            pending_press: 0,
        });
        host.by_peer.insert(peer, id);
        host.next_id = (id + 1) % super::super::BOT_ID_BASE;
    }
    fn gated(
        host: &HostAuthority,
    ) -> (
        decented_net::migration::Root,
        decented_net::migration::SessionImage,
        decented_net::migration::Primary,
        decented_net::migration::Sequencer,
        decented_net::Keys,
    ) {
        use decented_net::migration::*;
        let original = decented_net::Keys::generate();
        let key = decented_net::Keys::generate();
        let (binding, game) = checkpoint_batch(host).unwrap();
        let mut root = Root {
            session: fresh_session().unwrap(),
            app: "stampede-batch-test".into(),
            transport_version: 7,
            game_version: crate::proto::PROTO_VERSION,
            checkpoint_version: 1,
            host: original.public_key().to_bytes(),
            standby: key.public_key().to_bytes(),
            controller: original.public_key().to_bytes(),
            code: room_code_hash("ABCDEF").unwrap(),
            created_day: 20741,
            roster_revision: 1,
            members: host
                .humans
                .iter()
                .enumerate()
                .map(|(i, h)| Member {
                    player: u64::from(h.id) + 1,
                    seat: i as u8,
                    key: Some(if i == 0 {
                        original.public_key().to_bytes()
                    } else {
                        decented_net::Keys::generate().public_key().to_bytes()
                    }),
                })
                .collect(),
            config: binding.config,
            content: binding.content,
            limits: ResourceLimits::default(),
        };
        root.members.sort_by_key(|m| m.player);
        let routes: Vec<_> = host
            .humans
            .iter()
            .map(|h| (u64::from(h.id) + 1, h.peer))
            .collect();
        let image = SessionImage::initial(&root, &routes, game, 0, host.now).unwrap();
        validate_initial(&root, &image).unwrap();
        let sequencer = Sequencer::arm_session(root.clone(), image.clone(), &key).unwrap();
        let primary = Primary::armed(
            root.context().unwrap(),
            image.encode(&root).unwrap(),
            sequencer.receipt().clone(),
        )
        .unwrap();
        (root, image, primary, sequencer, key)
    }
    fn commit_staged(
        staged: StagedBatch,
        root: &decented_net::migration::Root,
        image: &mut decented_net::migration::SessionImage,
        primary: &mut decented_net::migration::Primary,
        seq: &mut decented_net::migration::Sequencer,
        key: &decented_net::Keys,
        marks: Vec<decented_net::migration::Watermark>,
    ) -> HostAuthority {
        let previous =
            decented_net::migration::SessionImage::decode(root, primary.committed_image()).unwrap();
        image.game = staged.bytes().to_vec();
        image.match_id = staged.match_id();
        image.logical_ms = staged.image.logical_ms;
        image.config = staged.binding().config;
        image.content = staged.binding().content;
        image.next_player = staged.next_player();
        let outputs = staged.outputs(&image.members, primary).unwrap();
        let candidate = primary
            .prepare(
                image.encode(root).unwrap(),
                image.logical_ms,
                marks,
                outputs,
            )
            .unwrap()
            .clone();
        validate_candidate(root, &previous, &candidate).unwrap();
        let token = seq.stage(candidate).unwrap();
        let receipt = seq.install(token, key).unwrap();
        let certified = primary.acknowledge(receipt).unwrap();
        staged.adopt(primary, root, &certified).unwrap().0
    }
    #[test]
    fn verified_queued_inputs_commit_a_prefix_and_keep_the_remaining_originals() {
        use decented_net::migration::*;
        for padded in [false, true] {
            let mut visible = host();
            add_human(&mut visible, 7, 70);
            let (mut root, mut image, _, _, key) = gated(&visible);
            let player = decented_net::Keys::generate();
            root.host = player.public_key().to_bytes();
            root.controller = root.host;
            root.members[0].key = Some(root.host);
            image.members[0].key = Some(root.host);
            let mut seq = Sequencer::arm_session(root.clone(), image.clone(), &key).unwrap();
            let mut primary = Primary::armed(
                root.context().unwrap(),
                image.encode(&root).unwrap(),
                seq.receipt().clone(),
            )
            .unwrap();
            let mut descriptor = Descriptor::offer(root.clone(), &player).unwrap();
            descriptor.consent(&key).unwrap();
            let cert = seq.membership_certificate(&key).unwrap();
            let policy = EndpointPolicy {
                descriptor,
                role: LinkRole::Original,
                promotion: None,
                membership: Some(cert.clone()),
            };
            let channel = Channel::new(
                &policy,
                ConnectionBinding {
                    session: root.session,
                    descriptor: root.digest().unwrap(),
                    term: 0,
                    client: LinkRole::Player,
                    server: LinkRole::Original,
                },
                ChannelSide::Server,
                root.host,
                [9; 32],
            )
            .unwrap();
            let mut queue = primary.attach_player(&channel, cert.clone()).unwrap();
            for number in 1..=256 {
                let mut bytes = crate::proto::encode(&crate::proto::ToHost::Ping {
                    id: number as u32,
                    client_t: 0,
                });
                if padded {
                    bytes.resize(1024, 0);
                }
                let wire = PlayerControl::sign(
                    &root,
                    &cert,
                    0,
                    number,
                    8,
                    [9; 32],
                    PlayerAction::Intent { number, bytes },
                    &player,
                )
                .unwrap()
                .encode(&root)
                .unwrap();
                queue.receive(&wire, &primary).unwrap();
            }
            let before = checkpoint(&visible).unwrap().1;
            let queued = stage_queued_batch(
                &visible,
                &primary,
                &root,
                16,
                &[&queue],
                &BTreeMap::new(),
                |h| {
                    assert!(h.start_race(0));
                    Ok(())
                },
            )
            .unwrap();
            let handled = queued.session.inputs[&8].handled();
            assert!(handled > 0 && handled <= 256);
            if !padded {
                assert!(
                    handled < 256,
                    "valid Pongs and race start require a smaller prefix"
                );
            }
            assert_eq!(queue.pending().count(), 256);
            assert_eq!(checkpoint(&visible).unwrap().1, before);
            let outputs = queued
                .batch
                .outputs(&queued.session.members, &primary)
                .unwrap();
            let candidate = primary
                .prepare(
                    queued.session.encode(&root).unwrap(),
                    16,
                    queued.marks,
                    outputs,
                )
                .unwrap()
                .clone();
            validate_candidate(&root, &image, &candidate).unwrap();
            let token = seq.stage(candidate).unwrap();
            let receipt = seq.install(token, &key).unwrap();
            assert_eq!(queue.pending().count(), 256);
            let records = primary.acknowledge(receipt).unwrap();
            visible = queued.batch.adopt(&mut primary, &root, &records).unwrap().0;
            queue.refresh(&primary).unwrap();
            assert_eq!(queue.pending().count(), 256 - handled as usize);
            assert!(visible.run.is_some());
            let ack = primary.output_ceiling(8, Delivery::Reliable);
            let wire = PlayerControl::sign(
                &root,
                &cert,
                0,
                257,
                8,
                [9; 32],
                PlayerAction::Applied {
                    output: ack,
                    tail: 256,
                    cut: None,
                },
                &player,
            )
            .unwrap()
            .encode(&root)
            .unwrap();
            queue.receive(&wire, &primary).unwrap();
            let queued = stage_queued_batch(
                &visible,
                &primary,
                &root,
                32,
                &[&queue],
                &BTreeMap::new(),
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(
                queued
                    .marks
                    .iter()
                    .find(|m| m.player == 8)
                    .unwrap()
                    .output_ack,
                ack
            );
            assert_eq!(queued.session.inputs[&8].handled(), 256);
            let old = primary.receipt().index;
            assert!(primary.outbox().any(|r| r.receipt.index == old));
            let outputs = queued
                .batch
                .outputs(&queued.session.members, &primary)
                .unwrap();
            let candidate = primary
                .prepare(
                    queued.session.encode(&root).unwrap(),
                    32,
                    queued.marks,
                    outputs,
                )
                .unwrap()
                .clone();
            let token = seq.stage(candidate).unwrap();
            let receipt = seq.install(token, &key).unwrap();
            primary.acknowledge(receipt).unwrap();
            queue.refresh(&primary).unwrap();
            assert_eq!(queue.pending().count(), 0);
            assert!(primary.outbox().all(|r| r.receipt.index > old));
        }
    }

    #[test]
    fn causal_recovery_keeps_world_frozen_then_hands_the_same_match_to_successor() {
        use decented_net::migration::*;
        let original_key = decented_net::Keys::generate();
        let player_key = decented_net::Keys::generate();
        let mut visible = host();
        visible.cfg.bots = 6;
        add_human(&mut visible, 7, 70);
        add_human(&mut visible, 42, 420);
        let (mut root, mut image, _, _, key) = gated(&visible);
        root.host = original_key.public_key().to_bytes();
        root.controller = root.host;
        root.members[0].key = Some(root.host);
        root.members[1].key = Some(player_key.public_key().to_bytes());
        image.members[0].key = root.members[0].key;
        image.members[1].key = root.members[1].key;
        let mut descriptor = Descriptor::offer(root.clone(), &original_key).unwrap();
        descriptor.consent(&key).unwrap();
        let mut seq = Sequencer::arm_session(root.clone(), image.clone(), &key).unwrap();
        let mut primary = Primary::armed(
            root.context().unwrap(),
            image.encode(&root).unwrap(),
            seq.receipt().clone(),
        )
        .unwrap();
        for step in 1..=80 {
            let staged = stage_batch(
                &visible,
                &primary,
                &root,
                step * 50,
                &[],
                &BTreeMap::new(),
                |h| {
                    if step == 1 {
                        assert!(h.start_race(0));
                    }
                    Ok(())
                },
            )
            .unwrap();
            let marks = seq.receipt().watermarks.clone();
            visible = commit_staged(
                staged,
                &root,
                &mut image,
                &mut primary,
                &mut seq,
                &key,
                marks,
            );
        }
        let before = checkpoint(&visible).unwrap().1;
        let world_before = options()
            .serialize(&visible.run.as_ref().unwrap().world)
            .unwrap();
        let frozen_tick = visible.run.as_ref().unwrap().world.tick;
        assert!(frozen_tick > 0);
        assert_eq!(visible.run.as_ref().unwrap().world.racers.len(), 8);
        let mut delivery = DeliveryJournal::default();
        for output in seq.outbox().filter(|r| r.output.recipient == 43) {
            delivery
                .apply_certified(&root.context().unwrap(), 43, output, |_| Ok(()))
                .unwrap();
        }
        let mut intents = IntentJournal::default();
        let ping = crate::proto::encode(&crate::proto::ToHost::Ping {
            id: 9,
            client_t: 4000,
        });
        assert_eq!(intents.append_batch(&[ping.clone()]).unwrap(), vec![1]);
        let mut recovery = Recovery::begin_session(descriptor, seq, &key, 0).unwrap();
        let tls = [4; 32];
        let challenge = recovery
            .challenge(1, tls, 43, player_key.public_key().to_bytes(), &key, 0)
            .unwrap();
        let proof = ResumeProof::sign(
            &root,
            &challenge,
            tls,
            delivery.applied(),
            intents.tail(),
            1,
            &player_key,
        )
        .unwrap();
        recovery.accept(1, tls, &proof, 0).unwrap();
        // Sample every frame: a clock jump would count as a local stall.
        for now in (50..249).step_by(50) {
            recovery.poll(now).unwrap();
        }
        assert!(!recovery.seal(249).unwrap());
        recovery.seal(250).unwrap();
        recover_security(&mut recovery, &key, 250).unwrap();
        recovery.commit_delivery_acks(&key).unwrap();
        recovery.begin_round().unwrap();
        let round = recovery.issue_input_round(43, tls, &key, 250).unwrap();
        let cut = PlayerControl::seal_round(
            &root,
            recovery.promotion(),
            &round,
            tls,
            &delivery,
            &intents,
            2,
            &player_key,
        )
        .unwrap();
        recovery.accept_input_cut(&cut, tls, 250).unwrap();
        recovery.supply(43, 1, ping.clone()).unwrap();
        recover_next(&mut recovery, &key, 260).unwrap();
        let post = SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
        let batch: BatchImage = options().deserialize(&post.game).unwrap();
        let frozen = restore(&batch.authority, &batch.binding, null_transport()).unwrap();
        assert_eq!(
            options()
                .serialize(&frozen.run.as_ref().unwrap().world)
                .unwrap(),
            world_before
        );
        assert_eq!(frozen.now, 4000);
        assert_eq!(frozen.races, visible.races);
        assert_eq!(checkpoint(&visible).unwrap().1, before);
        // A lost input receipt resends the exact old ID, without another Pong.
        let committed = recovery.sequencer().receipt().clone();
        recovery.supply(43, 1, ping).unwrap();
        assert_eq!(recovery.sequencer().receipt(), &committed);
        for record in recovery.replay().unwrap() {
            delivery
                .apply_certified(&root.context().unwrap(), 43, &record, |_| Ok(()))
                .unwrap();
        }
        recovery
            .acknowledge_outputs(43, delivery.applied())
            .unwrap();
        recovery.commit_delivery_acks(&key).unwrap();
        recovery.begin_round().unwrap();
        let round = recovery.issue_input_round(43, tls, &key, 260).unwrap();
        let cut = PlayerControl::seal_round(
            &root,
            recovery.promotion(),
            &round,
            tls,
            &delivery,
            &intents,
            3,
            &player_key,
        )
        .unwrap();
        recovery.accept_input_cut(&cut, tls, 260).unwrap();
        assert_eq!(recovery.absent().unwrap(), vec![8]);
        recover_departures(&mut recovery, &key, 270).unwrap();
        let post = SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
        assert_eq!(
            post.members.iter().map(|m| m.player).collect::<Vec<_>>(),
            vec![43]
        );
        assert_eq!(post.members[0].seat, 1);
        let batch: BatchImage = options().deserialize(&post.game).unwrap();
        let frozen = restore(&batch.authority, &batch.binding, null_transport()).unwrap();
        assert_eq!(frozen.run.as_ref().unwrap().world.tick, frozen_tick);
        assert_eq!(frozen.humans[0].id, 42);
        assert_eq!(frozen.run.as_ref().unwrap().world.racers.len(), 8);
        for record in recovery.replay().unwrap() {
            delivery
                .apply_certified(&root.context().unwrap(), 43, &record, |_| Ok(()))
                .unwrap();
        }
        recovery
            .acknowledge_outputs(43, delivery.applied())
            .unwrap();
        recovery.commit_delivery_acks(&key).unwrap();
        recovery.begin_round().unwrap();
        let round = recovery.issue_input_round(43, tls, &key, 270).unwrap();
        let cut = PlayerControl::seal_round(
            &root,
            recovery.promotion(),
            &round,
            tls,
            &delivery,
            &intents,
            4,
            &player_key,
        )
        .unwrap();
        recovery.accept_input_cut(&cut, tls, 270).unwrap();
        assert_eq!(recovery.phase(), RecoveryPhase::Ready);
        let promotion = recovery.promotion().clone();
        let mut barrier = RecoveryBarrier::new(recovery, &key, 280).unwrap();
        let cut = barrier.issue(43, tls, &key, 280).unwrap();
        let ready = RecoveryReady::sign(
            &root,
            &promotion,
            &cut,
            tls,
            &delivery,
            &intents,
            0,
            5,
            &player_key,
        )
        .unwrap();
        barrier.accept(&ready, tls, 280).unwrap();
        let (mut successor, _, resume) = barrier.finish(&key, 290).unwrap();
        assert_eq!(resume.membership.receipt.logical_ms, 4000);
        let staged = stage_batch(
            &frozen,
            successor.primary(),
            &root,
            4016,
            &[],
            &BTreeMap::new(),
            |_| Ok(()),
        )
        .unwrap();
        let session = SessionImage::decode(&root, successor.image()).unwrap();
        let marks = successor.receipt().watermarks.clone();
        staged
            .prepare_actor(&mut successor, session, marks)
            .unwrap();
        let PrimaryEvent::Committed { outputs, .. } = successor
            .commit(&key, |candidate, _, previous| {
                validate_candidate(&root, previous, candidate)
            })
            .unwrap()
        else {
            panic!("single successor");
        };
        let (resumed, _, _) = staged.adopt_actor(&mut successor, &outputs).unwrap();
        assert_eq!(resumed.races, visible.races);
        assert!(resumed.run.as_ref().unwrap().world.tick > frozen_tick);
        assert_eq!(resumed.humans[0].id, 42);
    }

    /// A lobby of three humans: the original's own seat (player 8), a
    /// survivor (43) and a leaver (51), each with a signing key.
    struct DepartureRoom {
        visible: HostAuthority,
        root: decented_net::migration::Root,
        descriptor: decented_net::migration::Descriptor,
        primary: decented_net::migration::Primary,
        seq: decented_net::migration::Sequencer,
        key: decented_net::Keys,
        survivor: decented_net::Keys,
        leaver: decented_net::Keys,
        cert: decented_net::migration::MembershipCertificate,
        queue: decented_net::migration::PlayerQueue,
        control: u64,
    }
    const LEAVER: u64 = 51;
    const LEAVER_PEER: PeerKey = 500;
    const TLS: [u8; 32] = [9; 32];
    impl DepartureRoom {
        fn new() -> Self {
            use decented_net::migration::*;
            let original = decented_net::Keys::generate();
            let survivor = decented_net::Keys::generate();
            let leaver = decented_net::Keys::generate();
            let mut visible = host();
            visible.cfg.bots = 5;
            add_human(&mut visible, 7, 70);
            add_human(&mut visible, 42, 420);
            add_human(&mut visible, 50, LEAVER_PEER);
            let (mut root, mut image, _, _, key) = gated(&visible);
            root.host = original.public_key().to_bytes();
            root.controller = root.host;
            for (i, k) in [&original, &survivor, &leaver].into_iter().enumerate() {
                root.members[i].key = Some(k.public_key().to_bytes());
                image.members[i].key = root.members[i].key;
            }
            let mut descriptor = Descriptor::offer(root.clone(), &original).unwrap();
            descriptor.consent(&key).unwrap();
            let seq = Sequencer::arm_session(root.clone(), image.clone(), &key).unwrap();
            let mut primary = Primary::armed(
                root.context().unwrap(),
                image.encode(&root).unwrap(),
                seq.receipt().clone(),
            )
            .unwrap();
            let cert = seq.membership_certificate(&key).unwrap();
            let policy = EndpointPolicy {
                descriptor: descriptor.clone(),
                role: LinkRole::Original,
                promotion: None,
                membership: Some(cert.clone()),
            };
            let channel = Channel::new(
                &policy,
                ConnectionBinding {
                    session: root.session,
                    descriptor: root.digest().unwrap(),
                    term: 0,
                    client: LinkRole::Player,
                    server: LinkRole::Original,
                },
                ChannelSide::Server,
                leaver.public_key().to_bytes(),
                TLS,
            )
            .unwrap();
            let queue = primary.attach_player(&channel, cert.clone()).unwrap();
            Self {
                visible,
                root,
                descriptor,
                primary,
                seq,
                key,
                survivor,
                leaver,
                cert,
                queue,
                control: 0,
            }
        }
        /// The leaver's signed intent `number` into its queue.
        fn intent(&mut self, number: u64, message: &crate::proto::ToHost) {
            use decented_net::migration::*;
            self.control += 1;
            let wire = PlayerControl::sign(
                &self.root,
                &self.cert,
                0,
                self.control,
                LEAVER,
                TLS,
                PlayerAction::Intent {
                    number,
                    bytes: crate::proto::encode(message),
                },
                &self.leaver,
            )
            .unwrap()
            .encode(&self.root)
            .unwrap();
            self.queue.receive(&wire, &self.primary).unwrap();
        }
        fn stage(&self, now: u64) -> Result<QueuedBatch> {
            stage_queued_batch(
                &self.visible,
                &self.primary,
                &self.root,
                now,
                &[&self.queue],
                &BTreeMap::new(),
                |_| Ok(()),
            )
        }
        /// The standby's validation, install and receipt, then adoption.
        fn commit(&mut self, queued: QueuedBatch) -> decented_net::migration::Candidate {
            let previous = decented_net::migration::SessionImage::decode(
                &self.root,
                self.primary.committed_image(),
            )
            .unwrap();
            let outputs = queued
                .batch
                .outputs(&queued.session.members, &self.primary)
                .unwrap();
            let candidate = self
                .primary
                .prepare(
                    queued.session.encode(&self.root).unwrap(),
                    queued.session.logical_ms,
                    queued.marks.clone(),
                    outputs,
                )
                .unwrap()
                .clone();
            validate_candidate(&self.root, &previous, &candidate).unwrap();
            let token = self.seq.stage(candidate.clone()).unwrap();
            let receipt = self.seq.install(token, &self.key).unwrap();
            let certified = self.primary.acknowledge(receipt).unwrap();
            self.visible = queued
                .batch
                .adopt(&mut self.primary, &self.root, &certified)
                .unwrap()
                .0;
            // A retired player's queue is closed (its route is removed).
            match self.queue.refresh(&self.primary) {
                Err(Error::Closed) => assert!(self.mark().retired),
                other => other.unwrap(),
            }
            candidate
        }
        fn image(&self) -> decented_net::migration::SessionImage {
            decented_net::migration::SessionImage::decode(
                &self.root,
                self.primary.committed_image(),
            )
            .unwrap()
        }
        fn mark(&self) -> decented_net::migration::Watermark {
            self.primary
                .receipt()
                .watermarks
                .iter()
                .find(|m| m.player == LEAVER)
                .cloned()
                .unwrap()
        }
        /// Batch N: the leaver's Ping and Leave in one batch. The Pong is
        /// kept; the leaver stays a seatless member with an unretired mark.
        fn departure(&mut self) {
            use crate::proto::ToHost;
            self.intent(1, &ToHost::Ping { id: 1, client_t: 0 });
            self.intent(2, &ToHost::Leave);
            let queued = self.stage(16).unwrap();
            let pongs = queued
                .batch
                .effects()
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        Effect::Reliable {
                            peer: LEAVER_PEER,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(pongs, 1, "the departure batch addresses the leaver");
            self.commit(queued);
            let image = self.image();
            assert!(image.members.iter().any(|m| m.player == LEAVER));
            assert_eq!(
                seatless_players(&self.root, self.primary.committed_image()).unwrap(),
                vec![LEAVER]
            );
            assert!(!self.mark().retired);
            assert_eq!(self.mark().input, 2);
            assert!(self.primary.outbox().any(|r| r.output.recipient == LEAVER));
            assert_eq!(self.visible.human_count(), 2);
        }
    }

    // T4: what the seatless member queued after its Leave (a Hello, which
    // would re-admit it natively) is not fed at N+1. The batch retires the
    // leaver with its mark as committed, prunes its outbox and commits.
    #[test]
    fn a_seatless_members_queue_is_not_fed_and_the_next_batch_retires_it() {
        use crate::proto::ToHost;
        let mut room = DepartureRoom::new();
        room.departure();
        let revision = room.image().membership_revision;
        room.intent(
            3,
            &ToHost::Hello {
                proto: crate::proto::PROTO_VERSION,
                name: "back".into(),
                character: 0,
            },
        );
        let queued = room.stage(32).unwrap();
        assert!(queued.selected_views().is_empty());
        assert!(!queued.session().members.iter().any(|m| m.player == LEAVER));
        assert!(!queued.session().inputs.contains_key(&LEAVER));
        assert!(!queued.batch().effects().iter().any(|e| matches!(
            e,
            Effect::Reliable {
                peer: LEAVER_PEER,
                ..
            }
        )));
        room.commit(queued);
        let mark = room.mark();
        assert!(mark.retired, "the next batch did not retire the leaver");
        assert_eq!(mark.input, 2, "the post-Leave Hello was fed");
        assert_eq!(room.image().membership_revision, revision + 1);
        assert!(room.primary.outbox().all(|r| r.output.recipient != LEAVER));
        assert_eq!(room.visible.human_count(), 2, "the leaver was re-admitted");
        assert!(seatless_players(&room.root, room.primary.committed_image())
            .unwrap()
            .is_empty());
        for now in [48, 64] {
            let queued = stage_queued_batch(
                &room.visible,
                &room.primary,
                &room.root,
                now,
                &[],
                &BTreeMap::new(),
                |_| Ok(()),
            )
            .unwrap();
            room.commit(queued);
        }
    }

    // T3: the standby bounds a seatless member to one ordinary batch: it
    // must have been seated in the previous image and be addressed by this
    // batch. A two-batch seatless member, one never seated before and one
    // not addressed are refused; a live successor batch
    // holds the same bound; only a recovery drain record carries one over.
    #[test]
    fn the_standby_refuses_a_seatless_member_outside_its_departure_batch() {
        use decented_net::migration::*;
        let mut room = DepartureRoom::new();
        let initial = room.image();
        room.departure();
        let departed = room.image();
        let marks = room.primary.receipt().watermarks.clone();
        // N+1 as staged, with the leaver kept seatless a second batch.
        let queued = stage_queued_batch(
            &room.visible,
            &room.primary,
            &room.root,
            32,
            &[],
            &BTreeMap::new(),
            |_| Ok(()),
        )
        .unwrap();
        let candidate = |image: &SessionImage, term: u64| Candidate {
            sequence: 1,
            term,
            index: 2,
            parent: [0; 32],
            logical_ms: image.logical_ms,
            image: image.encode(&room.root).unwrap(),
            watermarks: marks.clone(),
            outputs: vec![],
        };
        let mut kept = queued.session().clone();
        kept.members = departed.members.clone();
        kept.inputs = departed.inputs.clone();
        kept.membership_revision = departed.membership_revision;
        let refused = Err(Error::Invalid("race seatless member"));
        assert_eq!(
            validate_candidate(&room.root, &departed, &candidate(&kept, 0)),
            refused,
            "a two-batch seatless member"
        );
        // A live successor batch (term 1) holds the same bound.
        assert_eq!(
            validate_candidate(&room.root, &departed, &candidate(&kept, 1)),
            refused,
            "a live successor batch carried a seatless member over"
        );
        // Only a recovery's frozen drain record carries it over.
        validate_candidate_in(&room.root, &departed, &candidate(&kept, 1), true).unwrap();
        // Seated before N, but N+1 addresses it nothing.
        assert_eq!(
            validate_candidate(&room.root, &initial, &candidate(&kept, 0)),
            refused,
            "an unaddressed seatless member"
        );
        // A seatless member that was never a member before.
        let mut injected = queued.session().clone();
        let mut saved: BatchImage = options().deserialize(&injected.game).unwrap();
        let player = saved.next_player;
        saved.next_player += 1;
        injected.game = encode_batch(&saved).unwrap();
        injected.next_player = saved.next_player;
        injected.members.push(SessionMember {
            player,
            seat: 7,
            key: Some(decented_net::Keys::generate().public_key().to_bytes()),
            logical_peer: 999,
        });
        injected.inputs.insert(player, HandledInputs::default());
        assert_eq!(
            validate_candidate(&room.root, &departed, &candidate(&injected, 0)),
            refused,
            "a seatless member never seated before"
        );
        // The staged N+1 itself (the leaver removed) is valid.
        validate_candidate(&room.root, &departed, &candidate(queued.session(), 0)).unwrap();
    }

    // A member seatless in the previous image must be gone from the
    // candidate, seated or not: a hostile original cannot seat the leaver
    // again and undo its departure. The candidate is N+1 as staged, at a
    // later time, with the leaver's native human, its old seat row, its
    // membership and its history put back. The same seat over an image
    // where the leaver was seated is accepted, so only the departure
    // refuses it.
    #[test]
    fn the_standby_refuses_a_candidate_that_seats_a_seatless_member_again() {
        use decented_net::migration::*;
        let mut room = DepartureRoom::new();
        let initial = room.image();
        let seated: BatchImage = options().deserialize(&initial.game).unwrap();
        let row = seated
            .seats
            .iter()
            .find(|row| row.player == LEAVER)
            .cloned()
            .unwrap();
        room.departure();
        let departed = room.image();
        let queued = stage_queued_batch(
            &room.visible,
            &room.primary,
            &room.root,
            32,
            &[],
            &BTreeMap::new(),
            |_| Ok(()),
        )
        .unwrap();
        let mut reseated = queued.session().clone();
        assert!(reseated.logical_ms > departed.logical_ms);
        let mut saved: BatchImage = options().deserialize(&reseated.game).unwrap();
        let mut host = restore(&saved.authority, &saved.binding, null_transport()).unwrap();
        add_human(&mut host, row.native_id, row.logical_peer);
        let (binding, authority) = checkpoint(&host).unwrap();
        saved.binding = binding;
        saved.authority = authority;
        saved.seats.push(row);
        saved.seats.sort_by_key(|row| row.player);
        reseated.game = encode_batch(&saved).unwrap();
        reseated.members = departed.members.clone();
        reseated.inputs = departed.inputs.clone();
        reseated.membership_revision = departed.membership_revision;
        let candidate = || Candidate {
            sequence: 1,
            term: 0,
            index: 2,
            parent: [0; 32],
            logical_ms: reseated.logical_ms,
            image: reseated.encode(&room.root).unwrap(),
            watermarks: room.primary.receipt().watermarks.clone(),
            outputs: vec![],
        };
        // Over an image where the leaver held this seat, it is valid.
        validate_candidate(&room.root, &initial, &candidate()).unwrap();
        // Over the departure batch, where the leaver is seatless, it is not.
        assert_eq!(
            validate_candidate(&room.root, &departed, &candidate()),
            Err(Error::Invalid("race seatless member re-seated")),
            "a seatless member came back seated"
        );
        // Nor may a live successor batch, or a recovery drain record, seat it.
        for recovering in [false, true] {
            let mut later = candidate();
            later.term = 1;
            assert_eq!(
                validate_candidate_in(&room.root, &departed, &later, recovering),
                Err(Error::Invalid("race seatless member re-seated")),
                "re-seated in term 1 (recovering={recovering})"
            );
        }
    }

    // T2: a takeover between N and N+1. The seatless leaver is absent (never
    // challenged); a survivor's drain record carries it over, and the
    // departure commit removes and retires it with no native event and
    // prunes its outbox. Recovery reaches Ready.
    #[test]
    fn a_takeover_between_the_departure_batch_and_the_next_retires_the_seatless_leaver() {
        use decented_net::migration::*;
        for drain in [false, true] {
            let mut room = DepartureRoom::new();
            room.departure();
            let context = room.root.context().unwrap();
            let mut delivery = DeliveryJournal::default();
            for output in room.seq.outbox().filter(|r| r.output.recipient == 43) {
                delivery
                    .apply_certified(&context, 43, output, |_| Ok(()))
                    .unwrap();
            }
            let mut intents = IntentJournal::default();
            let ping = crate::proto::encode(&crate::proto::ToHost::Ping {
                id: 9,
                client_t: 16,
            });
            if drain {
                intents.append_batch(std::slice::from_ref(&ping)).unwrap();
            }
            let DepartureRoom {
                root,
                descriptor,
                seq,
                key,
                survivor,
                ..
            } = room;
            let mut recovery = Recovery::begin_session(descriptor, seq, &key, 0).unwrap();
            let tls = [4; 32];
            let challenge = recovery
                .challenge(1, tls, 43, survivor.public_key().to_bytes(), &key, 0)
                .unwrap();
            let proof = ResumeProof::sign(
                &root,
                &challenge,
                tls,
                delivery.applied(),
                intents.tail(),
                1,
                &survivor,
            )
            .unwrap();
            recovery.accept(1, tls, &proof, 0).unwrap();
            // Sample every frame: a clock jump would count as a local stall.
            for now in (50..250).step_by(50) {
                recovery.poll(now).unwrap();
            }
            recovery.seal(250).unwrap();
            recover_security(&mut recovery, &key, 250).unwrap();
            let mut control = 2;
            let mut now = 250;
            let mut departed = false;
            while recovery.phase() != RecoveryPhase::Ready {
                assert!(control < 10, "recovery stalled in {:?}", recovery.phase());
                for record in recovery.replay().unwrap() {
                    if record.output.recipient == 43 {
                        delivery
                            .apply_certified(&context, 43, &record, |_| Ok(()))
                            .unwrap();
                    }
                }
                recovery
                    .acknowledge_outputs(43, delivery.applied())
                    .unwrap();
                recovery.commit_delivery_acks(&key).unwrap();
                recovery.begin_round().unwrap();
                let round = recovery.issue_input_round(43, tls, &key, now).unwrap();
                let cut = PlayerControl::seal_round(
                    &root,
                    recovery.promotion(),
                    &round,
                    tls,
                    &delivery,
                    &intents,
                    control,
                    &survivor,
                )
                .unwrap();
                recovery.accept_input_cut(&cut, tls, now).unwrap();
                control += 1;
                now += 10;
                match recovery.phase() {
                    RecoveryPhase::Draining => {
                        recovery.supply(43, 1, ping.clone()).unwrap();
                        recover_next(&mut recovery, &key, now).unwrap();
                        // The drain record carries the seatless leaver.
                        let post =
                            SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
                        assert!(post.members.iter().any(|m| m.player == LEAVER));
                    }
                    RecoveryPhase::Departures => {
                        let mut absent = recovery.absent().unwrap();
                        absent.sort();
                        assert_eq!(absent, vec![8, LEAVER]);
                        recover_departures(&mut recovery, &key, now).unwrap();
                        departed = true;
                        let post =
                            SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
                        assert_eq!(
                            post.members.iter().map(|m| m.player).collect::<Vec<_>>(),
                            vec![43]
                        );
                        let receipt = recovery.sequencer().receipt();
                        assert!(receipt
                            .watermarks
                            .iter()
                            .any(|m| m.player == LEAVER && m.retired));
                        assert!(recovery
                            .sequencer()
                            .outbox()
                            .all(|r| r.output.recipient != LEAVER));
                    }
                    _ => {}
                }
            }
            assert!(departed, "the departure commit never ran (drain={drain})");
            let post = SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
            assert_eq!(post.inputs[&43].handled(), u64::from(drain));
        }
    }

    #[test]
    fn sparse_native_seats_survive_deletion_and_reuse_with_fresh_fray_ids() {
        use decented_net::migration::{HandledInputs, SessionMember, Watermark};
        let mut original = host();
        add_human(&mut original, 7, 70);
        add_human(&mut original, 42, 420);
        let (root, mut image, mut primary, mut seq, key) = gated(&original);
        assert_eq!(
            image
                .members
                .iter()
                .map(|m| (m.player, m.seat))
                .collect::<Vec<_>>(),
            vec![(8, 0), (43, 1)]
        );
        let stage = stage_batch(
            &original,
            &primary,
            &root,
            16,
            &[crate::transport::HostNetEvent::Left { peer: 70 }],
            &BTreeMap::new(),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            stage.seat_bindings(),
            &[GameSeat {
                player: 43,
                seat: 1,
                native_id: 42,
                logical_peer: 420
            }]
        );
        image.members.retain(|m| m.player != 8);
        image.inputs.remove(&8);
        image.membership_revision += 1;
        let mut marks = seq.receipt().watermarks.clone();
        marks.iter_mut().find(|m| m.player == 8).unwrap().retired = true;
        let committed = commit_staged(
            stage,
            &root,
            &mut image,
            &mut primary,
            &mut seq,
            &key,
            marks,
        );
        assert_eq!(committed.humans[0].id, 42);
        let hello = crate::proto::encode(&crate::proto::ToHost::Hello {
            proto: crate::proto::PROTO_VERSION,
            name: "New player".into(),
            character: 0,
        });
        let events = [
            crate::transport::HostNetEvent::Joined {
                peer: 700,
                path: crate::transport::LinkPath::Direct,
            },
            crate::transport::HostNetEvent::Reliable {
                peer: 700,
                bytes: hello,
            },
        ];
        let stage = stage_batch(
            &committed,
            &primary,
            &root,
            32,
            &events,
            &BTreeMap::new(),
            |h| {
                h.next_id = 7;
                Ok(())
            },
        )
        .unwrap();
        let new = stage
            .seat_bindings()
            .iter()
            .find(|m| m.native_id == 7)
            .unwrap();
        assert_eq!((new.player, new.seat, new.logical_peer), (44, 0, 700));
        image.members.push(SessionMember {
            player: 44,
            seat: 0,
            key: Some(decented_net::Keys::generate().public_key().to_bytes()),
            logical_peer: 700,
        });
        image.inputs.insert(44, HandledInputs::default());
        image.membership_revision += 1;
        let mut marks = seq.receipt().watermarks.clone();
        marks.push(Watermark {
            player: 44,
            input: 0,
            output_ack: 0,
            retired: false,
        });
        let committed = commit_staged(
            stage,
            &root,
            &mut image,
            &mut primary,
            &mut seq,
            &key,
            marks,
        );
        assert_eq!(committed.by_peer.get(&700), Some(&7));
        assert_eq!(image.next_player, 45);
        let saved: BatchImage = options().deserialize(&image.game).unwrap();
        assert_eq!(
            saved
                .seats
                .iter()
                .map(|s| (s.player, s.native_id))
                .collect::<Vec<_>>(),
            vec![(43, 42), (44, 7)]
        );
    }
    #[test]
    fn swapped_routes_and_existing_native_remaps_fail_before_standby_install() {
        use decented_net::migration::{Candidate, SessionImage};
        let mut original = host();
        add_human(&mut original, 7, 70);
        add_human(&mut original, 42, 420);
        let (root, initial, primary, _, _) = gated(&original);
        let mut swapped = initial.clone();
        swapped.members[0].logical_peer = 420;
        swapped.members[1].logical_peer = 70;
        swapped.validate_initial(&root).unwrap();
        assert!(validate_initial(&root, &swapped).is_err());
        let staged = stage_batch(
            &original,
            &primary,
            &root,
            16,
            &[],
            &BTreeMap::new(),
            |_| Ok(()),
        )
        .unwrap();
        let mut image = initial.clone();
        image.logical_ms = 16;
        image.game = staged.bytes().to_vec();
        let mut saved: BatchImage = options().deserialize(&image.game).unwrap();
        let mut host = restore(&saved.authority, &saved.binding, null_transport()).unwrap();
        host.humans[0].id = 42;
        host.humans[1].id = 7;
        host.by_peer.insert(70, 42);
        host.by_peer.insert(420, 7);
        saved.authority = checkpoint(&host).unwrap().1;
        saved.seats[0].native_id = 42;
        saved.seats[1].native_id = 7;
        image.game = encode_batch(&saved).unwrap();
        validate_batch(&image.game, &saved.binding, 16).unwrap();
        let candidate = Candidate {
            // Validation-only envelope; the owning actor allocates the wire ID.
            sequence: 1,
            term: 0,
            index: 1,
            parent: primary.receipt().digest(&root.context().unwrap()).unwrap(),
            logical_ms: 16,
            image: image.encode(&root).unwrap(),
            watermarks: primary.receipt().watermarks.clone(),
            outputs: staged.outputs(&image.members, &primary).unwrap(),
        };
        assert!(matches!(
            validate_candidate(&root, &initial, &candidate),
            Err(Error::Conflict)
        ));
        assert_eq!(
            SessionImage::decode(&root, primary.committed_image()).unwrap(),
            initial
        );
    }
    #[test]
    fn speculative_world_and_transport_controls_unlock_only_after_the_exact_receipt() {
        let original = host();
        let original_bytes = checkpoint(&original).unwrap().1;
        let (root, mut image, mut primary, mut sequencer, key) = gated(&original);
        let stage = |h: &mut HostAuthority| {
            assert!(h.start_race(0));
            Ok(())
        };
        let early =
            stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), stage).unwrap();
        assert!(early.adopt(&mut primary, &root, &[]).is_err());
        assert_eq!(checkpoint(&original).unwrap().1, original_bytes);
        let staged =
            stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), stage).unwrap();
        assert!(staged
            .effects()
            .iter()
            .any(|e| matches!(e, Effect::Open(false))));
        assert_eq!(
            validate_batch(staged.bytes(), staged.binding(), 16).unwrap(),
            staged.bytes()
        );
        assert!(validate_batch(staged.bytes(), staged.binding(), 17).is_err());
        image.game = staged.bytes().to_vec();
        image.match_id = staged.match_id();
        image.logical_ms = 16;
        image.config = staged.binding().config;
        image.content = staged.binding().content;
        let candidate = primary
            .prepare(image.encode(&root).unwrap(), 16, vec![], vec![])
            .unwrap()
            .clone();
        assert_eq!(checkpoint(&original).unwrap().1, original_bytes);
        let token = sequencer.stage(candidate).unwrap();
        let receipt = sequencer.install(token, &key).unwrap();
        let certified = primary.acknowledge(receipt).unwrap();
        let (committed, _, effects) = staged.adopt(&mut primary, &root, &certified).unwrap();
        assert_eq!(committed.now, 16);
        assert!(committed.run.is_some());
        assert!(effects.iter().any(|e| matches!(e, Effect::Open(false))));
        assert_eq!(original.phase, super::super::HostPhase::Lobby);
    }
    #[test]
    fn shared_actors_drive_real_race_batches_and_gate_every_visible_world_update() {
        use decented_net::migration::*;
        let mut visible = host();
        visible.cfg.bots = 7;
        let host_key = decented_net::Keys::generate();
        let key = decented_net::Keys::generate();
        visible.humans.push(Human {
            id: 0,
            peer: 10,
            name: "Player".into(),
            character: 0,
            attire: [0; 3],
            ready: false,
            custom_ready: None,
            host: true,
            last_heard: 0,
            racer: None,
            inq: BTreeMap::new(),
            last_fed: [0; 4],
            applied_seq: 0,
            starved: VecDeque::new(),
            pending_press: 0,
        });
        visible.by_peer.insert(10, 0);
        visible.next_id = 1;
        let (mut root, mut image, _, _, _) = gated(&visible);
        root.host = host_key.public_key().to_bytes();
        root.controller = root.host;
        root.standby = key.public_key().to_bytes();
        root.members[0].key = Some(root.host);
        let mut descriptor = Descriptor::offer(root.clone(), &host_key).unwrap();
        descriptor.consent(&key).unwrap();
        image.members[0].key = Some(root.host);
        let seq = Sequencer::arm_session(root.clone(), image.clone(), &key).unwrap();
        let primary = Primary::armed(
            root.context().unwrap(),
            image.encode(&root).unwrap(),
            seq.receipt().clone(),
        )
        .unwrap();
        let policy = EndpointPolicy {
            descriptor: descriptor.clone(),
            role: LinkRole::Standby,
            promotion: None,
            membership: None,
        };
        let binding = ConnectionBinding {
            session: root.session,
            descriptor: root.digest().unwrap(),
            term: 0,
            client: LinkRole::Original,
            server: LinkRole::Standby,
        };
        let client = Channel::new(
            &policy,
            binding.clone(),
            ChannelSide::Client,
            root.standby,
            [7; 32],
        )
        .unwrap();
        let server =
            Channel::new(&policy, binding, ChannelSide::Server, root.host, [7; 32]).unwrap();
        let mut actor =
            PrimaryActor::new(descriptor.clone(), primary, client, Activity::Advancing, 0).unwrap();
        let mut standby = StandbyActor::new(descriptor, seq, server, 0).unwrap();
        let mut wire_ms = 0;
        let mut views = 0;
        for tick in 1..=300u64 {
            let now = (tick * 1000).div_ceil(60);
            let before = checkpoint(&visible).unwrap().1;
            let staged = stage_batch(
                &visible,
                actor.primary(),
                &root,
                now,
                &[],
                &BTreeMap::new(),
                |host| {
                    if tick == 1 {
                        assert!(host.start_race(0));
                    }
                    Ok(())
                },
            )
            .unwrap();
            let old_image = SessionImage::decode(&root, actor.primary().committed_image()).unwrap();
            staged
                .prepare_actor(&mut actor, old_image, standby.receipt().watermarks.clone())
                .unwrap();
            assert_eq!(checkpoint(&visible).unwrap().1, before);
            while standby.receipt().index < tick {
                while let Some(wire) = actor.poll_send(wire_ms).unwrap() {
                    standby
                        .receive(
                            &wire,
                            wire_ms,
                            &key,
                            Activity::Advancing,
                            |candidate, _, previous| validate_candidate(&root, previous, candidate),
                        )
                        .unwrap();
                }
                wire_ms += 1;
                assert!(wire_ms < 5000);
            }
            // The standby has the complete next World; UI still reads C.
            assert_eq!(checkpoint(&visible).unwrap().1, before);
            let mut committed = None;
            while let Some(wire) = standby.poll_send(wire_ms).unwrap() {
                if let Some(PrimaryEvent::Committed { outputs, .. }) =
                    actor.receive(&wire, wire_ms, &host_key).unwrap()
                {
                    committed = Some(outputs);
                }
            }
            let certified = committed.unwrap();
            views += certified.len();
            let (next, published, controls) = staged.adopt_actor(&mut actor, &certified).unwrap();
            assert_eq!(published, certified);
            assert!(controls
                .iter()
                .all(|e| !matches!(e, Effect::Reliable { .. } | Effect::Lossy { .. })));
            visible = next;
            assert_eq!(visible.now, now);
            let installed = SessionImage::decode(&root, standby.image()).unwrap();
            let saved: BatchImage = options().deserialize(&installed.game).unwrap();
            assert_eq!(checkpoint(&visible).unwrap().1, saved.authority);
        }
        assert!(views > 0);
        assert_eq!(visible.world().unwrap().racers.len(), 8);
        assert_eq!(actor.primary().receipt().index, 300);
        let world = visible.world().unwrap();
        assert!(world.tick > world.go_tick + 60);
    }
    #[test]
    fn failed_or_oversized_staged_batches_cannot_mutate_the_visible_authority() {
        let original = host();
        let original_bytes = checkpoint(&original).unwrap().1;
        let (root, _, primary, _, _) = gated(&original);
        let mut attempts = 0;
        let batch = stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), |h| {
            attempts += 1;
            assert_eq!(h.phase, super::super::HostPhase::Lobby);
            assert_eq!(h.now, 0);
            h.start_race(0);
            if attempts == 1 {
                Err(Error::Invalid("test fault"))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempts, 2);
        assert!(!batch.effects().is_empty());
        let mut attempts = 0;
        assert!(
            stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), |h| {
                attempts += 1;
                h.start_race(0);
                Err(Error::Invalid("persistent test fault"))
            })
            .is_err()
        );
        assert_eq!(attempts, 2);
        assert!(
            stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), |h| {
                for _ in 0..=decented_net::migration::MAX_BATCH {
                    h.transport.set_open(false);
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(checkpoint(&original).unwrap().1, original_bytes);
        assert!(primary.pending().is_none());
    }
    #[test]
    fn data_effects_require_complete_certified_streams_and_one_receipt_has_one_adoption() {
        let mut original = host();
        original.cfg.bots = 2;
        original.humans.push(Human {
            id: 0,
            peer: 10,
            name: "Player".into(),
            character: 0,
            attire: [0; 3],
            ready: false,
            custom_ready: None,
            host: true,
            last_heard: 0,
            racer: None,
            inq: BTreeMap::new(),
            last_fed: [0; 4],
            applied_seq: 0,
            starved: VecDeque::new(),
            pending_press: 0,
        });
        original.by_peer.insert(10, 0);
        original.next_id = 1;
        let (root, mut image, mut primary, mut sequencer, key) = gated(&original);
        let op = |h: &mut HostAuthority| {
            assert!(h.start_race(0));
            Ok(())
        };
        let a = stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), op).unwrap();
        let b = stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), op).unwrap();
        let missing =
            stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), op).unwrap();
        assert_eq!(a.bytes(), b.bytes());
        assert!(a.outputs(&[], &primary).is_err());
        let outputs = a.outputs(&image.members, &primary).unwrap();
        assert!(!outputs.is_empty());
        assert_eq!(
            a.outputs(&image.members, &primary).unwrap(),
            outputs,
            "mapping never consumes live stream IDs"
        );
        image.game = a.bytes().to_vec();
        image.match_id = a.match_id();
        image.logical_ms = 16;
        image.config = a.binding().config;
        image.content = a.binding().content;
        let candidate = primary
            .prepare(
                image.encode(&root).unwrap(),
                16,
                sequencer.receipt().watermarks.clone(),
                outputs,
            )
            .unwrap()
            .clone();
        let mut bad = candidate.clone();
        bad.outputs.clear();
        assert!(validate_candidate(
            &root,
            &decented_net::migration::SessionImage::decode(&root, primary.committed_image())
                .unwrap(),
            &bad
        )
        .is_err());
        let mut bad = candidate.clone();
        let mut wrong = image.clone();
        wrong.match_id += 1;
        bad.image = wrong.encode(&root).unwrap();
        assert!(validate_candidate(
            &root,
            &decented_net::migration::SessionImage::decode(&root, primary.committed_image())
                .unwrap(),
            &bad
        )
        .is_err());
        let mut bad = candidate.clone();
        bad.outputs[0].bytes[0] ^= 1;
        assert!(validate_candidate(
            &root,
            &decented_net::migration::SessionImage::decode(&root, primary.committed_image())
                .unwrap(),
            &bad
        )
        .is_err());
        validate_candidate(
            &root,
            &decented_net::migration::SessionImage::decode(&root, primary.committed_image())
                .unwrap(),
            &candidate,
        )
        .unwrap();
        let token = sequencer.stage(candidate).unwrap();
        let receipt = sequencer.install(token, &key).unwrap();
        let certified = primary.acknowledge(receipt).unwrap();
        assert!(missing.adopt(&mut primary, &root, &[]).is_err());
        let (_, publication, controls) = a.adopt(&mut primary, &root, &certified).unwrap();
        assert_eq!(publication, certified);
        assert!(controls
            .iter()
            .all(|e| !matches!(e, Effect::Reliable { .. } | Effect::Lossy { .. })));
        assert!(b.adopt(&mut primary, &root, &certified).is_err());
    }
    #[test]
    fn custom_track_registry_changes_only_after_commit_and_never_during_a_failed_stage() {
        let unrelated = crate::track_transfer::sample_track("Other room", 180.0);
        race_core::custom::install(race_core::custom::to_def(&unrelated).unwrap());
        let generation = race_core::custom::generation();
        let selected = crate::track_transfer::sample_track("Staged room", 95.0);
        let original = host();
        let (root, mut image, mut primary, mut sequencer, key) = gated(&original);
        assert!(
            stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), |h| {
                h.set_custom_track(&selected).unwrap();
                Err(Error::Invalid("test track fault"))
            })
            .is_err()
        );
        assert_eq!(race_core::custom::generation(), generation);
        assert_eq!(race_core::custom::current().unwrap().1.name, unrelated.name);
        let staged = stage_batch(&original, &primary, &root, 16, &[], &BTreeMap::new(), |h| {
            h.set_custom_track(&selected).unwrap();
            assert!(h.start_race(0));
            Ok(())
        })
        .unwrap();
        assert_eq!(race_core::custom::generation(), generation);
        image.game = staged.bytes().to_vec();
        image.match_id = staged.match_id();
        image.logical_ms = 16;
        image.config = staged.binding().config;
        image.content = staged.binding().content;
        let candidate = primary
            .prepare(image.encode(&root).unwrap(), 16, vec![], vec![])
            .unwrap()
            .clone();
        validate_candidate(
            &root,
            &decented_net::migration::SessionImage::decode(&root, primary.committed_image())
                .unwrap(),
            &candidate,
        )
        .unwrap();
        assert_eq!(race_core::custom::generation(), generation);
        let token = sequencer.stage(candidate).unwrap();
        let receipt = sequencer.install(token, &key).unwrap();
        let certified = primary.acknowledge(receipt).unwrap();
        let (committed, _, _) = staged.adopt(&mut primary, &root, &certified).unwrap();
        assert_eq!(race_core::custom::generation(), generation + 1);
        assert_eq!(race_core::custom::current().unwrap().1.name, selected.name);
        assert_eq!(committed.world().unwrap().track.name, selected.name);
    }
    #[test]
    fn le_mans_rule_is_bound_and_retained_across_lobby_and_running_restore() {
        let mut original = host();
        assert!(original.le_mans);
        let enabled = checkpoint(&original).unwrap().0;
        assert!(original.set_le_mans_start(false));
        let (disabled, bytes) = checkpoint(&original).unwrap();
        assert_ne!(enabled.config, disabled.config);
        assert!(restore(&bytes, &enabled, null_transport()).is_err());
        let restored = restore(&bytes, &disabled, null_transport()).unwrap();
        assert!(!restored.le_mans);
        for on in [false, true] {
            let mut original = host();
            assert!(original.set_le_mans_start(on));
            assert!(original.start_race(0));
            original.update(2400);
            let (binding, bytes) = checkpoint(&original).unwrap();
            let mut restored = restore(&bytes, &binding, null_transport()).unwrap();
            assert_eq!(restored.le_mans, on);
            assert_eq!(restored.run.as_ref().unwrap().world.cfg.le_mans, on);
            for now in [2416, 2432, 2500] {
                original.update(now);
                restored.update(now);
                assert_eq!(
                    checkpoint(&original).unwrap(),
                    checkpoint(&restored).unwrap()
                );
            }
        }
    }
    #[test]
    fn full_headless_world_restores_and_continues_with_identical_future_state() {
        let mut uninterrupted = host();
        assert!(uninterrupted.start_race(0));
        for tick in 1..=120 {
            uninterrupted.update((tick * 1000u64).div_ceil(60));
        }
        let (binding, bytes) = checkpoint(&uninterrupted).unwrap();
        let mut restored = restore(&bytes, &binding, Box::new(NullHostTransport)).unwrap();
        assert_eq!(checkpoint(&restored).unwrap().1, bytes);
        for tick in 121..=360 {
            let now = (tick * 1000u64).div_ceil(60);
            uninterrupted.update(now);
            restored.update(now);
            assert_eq!(
                checkpoint(&uninterrupted).unwrap().1,
                checkpoint(&restored).unwrap().1,
                "tick {tick}"
            );
        }
    }
    #[test]
    fn custom_track_restore_uses_checkpoint_content_not_another_rooms_registry() {
        let mut original = host();
        let track = crate::track_transfer::sample_track("Migration loop", 95.0);
        original.set_custom_track(&track).unwrap();
        assert!(original.start_race(0));
        original.update(1500);
        let (binding, bytes) = checkpoint(&original).unwrap();
        let unrelated = crate::track_transfer::sample_track("Other room", 180.0);
        race_core::custom::install(race_core::custom::to_def(&unrelated).unwrap());
        let restored = restore(&bytes, &binding, Box::new(NullHostTransport)).unwrap();
        assert_eq!(restored.world().unwrap().track.name, track.name);
        assert_eq!(restored.custom_track(), Some(&track));
        assert_eq!(checkpoint(&restored).unwrap().1, bytes);
        assert_eq!(race_core::custom::current().unwrap().1.name, unrelated.name);
    }
    #[test]
    fn input_queue_starvation_late_press_and_peer_rebinding_survive() {
        let mut original = host();
        original.cfg.bots = 2;
        original.humans.push(Human {
            id: 0,
            peer: 10,
            name: "Test player".into(),
            character: 0,
            attire: [0; 3],
            ready: false,
            custom_ready: None,
            host: false,
            last_heard: 0,
            racer: None,
            inq: BTreeMap::new(),
            last_fed: [0; 4],
            applied_seq: 0,
            starved: VecDeque::new(),
            pending_press: 0,
        });
        original.by_peer.insert(10, 0);
        original.next_id = 1;
        assert!(original.start_race(0));
        original.update(1000);
        original.humans[0].inq.insert(24, [1, 2, 3, 4]);
        original.humans[0].starved.push_back((20, 0));
        original.humans[0].pending_press = 1 << 2;
        let (binding, bytes) = checkpoint(&original).unwrap();
        let mut restored = restore(&bytes, &binding, Box::new(NullHostTransport)).unwrap();
        assert_eq!(restored.humans[0].pending_press, 1 << 2);
        assert_eq!(restored.humans[0].inq, original.humans[0].inq);
        assert_eq!(restored.humans[0].starved, original.humans[0].starved);
        for tick in 61..=80 {
            let now = (tick * 1000u64).div_ceil(60);
            original.update(now);
            restored.update(now);
            assert_eq!(
                checkpoint(&original).unwrap().1,
                checkpoint(&restored).unwrap().1
            );
        }
    }
    /// `race_core` keeps the Laser Trump victim masks and the frogs' paused
    /// path time out of its serde form (host-only, never on the wire). The
    /// checkpoint carries them beside the authority, so a restored host
    /// continues the same race; out-of-range values are refused.
    #[test]
    fn host_only_trump_masks_and_frog_pauses_survive_restore() {
        let mut original = host();
        assert!(original.start_race(0));
        original.update(1500);
        {
            let world = &mut original.run.as_mut().unwrap().world;
            let racers = world.racers.len() as u32;
            for (i, racer) in world.racers.iter_mut().enumerate() {
                racer.trump_hit = (i as u32 * 37 + 5) & ((1 << racers) - 1);
                racer.effects.feather = 0.5 + i as f32 * 0.1;
            }
            if world.play.frogs.is_empty() {
                world.play.frogs.push(race_core::playful::Frog::default());
            }
            for (i, frog) in world.play.frogs.iter_mut().enumerate() {
                frog.paused = 0.25 + i as f32;
            }
        }
        let (binding, bytes) = checkpoint(&original).unwrap();
        let mut restored = restore(&bytes, &binding, Box::new(NullHostTransport)).unwrap();
        let (a, b) = (
            &original.run.as_ref().unwrap().world,
            &restored.run.as_ref().unwrap().world,
        );
        assert!(a.racers.iter().any(|r| r.trump_hit != 0));
        assert_eq!(
            a.racers.iter().map(|r| r.trump_hit).collect::<Vec<_>>(),
            b.racers.iter().map(|r| r.trump_hit).collect::<Vec<_>>()
        );
        assert_eq!(
            a.play.frogs.iter().map(|f| f.paused).collect::<Vec<_>>(),
            b.play.frogs.iter().map(|f| f.paused).collect::<Vec<_>>()
        );
        assert_eq!(
            a.racers.iter().map(|r| r.effects.feather).collect::<Vec<_>>(),
            b.racers.iter().map(|r| r.effects.feather).collect::<Vec<_>>()
        );

        for tick in 91..=150 {
            let now = (tick * 1000u64).div_ceil(60);
            original.update(now);
            restored.update(now);
            assert_eq!(
                checkpoint(&original).unwrap().1,
                checkpoint(&restored).unwrap().1,
                "tick {tick}"
            );
        }
        let encode = |host_only: HostOnly| {
            let view = Snapshot {
                schema: SCHEMA,
                game_protocol: crate::proto::PROTO_VERSION,
                binding: &binding,
                custom: None,
                authority: &original,
                host_only,
            };
            let mut bytes = MAGIC.to_vec();
            bytes.extend(options().serialize(&view).unwrap());
            bytes
        };
        let binding = checkpoint(&original).unwrap().0;
        let good = HostOnly::capture(&original);
        let racers = good.trump_hit.len();
        let mut wide = good.clone();
        wide.trump_hit[0] = 1 << racers;
        let mut short = good.clone();
        short.trump_hit.pop();
        let mut negative = good.clone();
        negative.frog_paused[0] = -1.0;
        let mut infinite = good.clone();
        infinite.frog_paused[0] = f32::INFINITY;
        let mut feather = good.clone();
        feather.feather[0] = f32::NAN;
        assert!(restore(&encode(good), &binding, null_transport()).is_ok());
        for bad in [wide, short, negative, infinite, feather] {
            assert!(restore(&encode(bad), &binding, null_transport()).is_err());
        }
    }
    #[test]
    fn battle_arena_restores_and_continues_independently_of_selected_race_track() {
        let mut original = host();
        let arena = TrackId::ARENAS[0];
        assert_ne!(original.cfg.track, arena);
        assert!(original.set_battle(Some(crate::proto::BattleSetup::new(arena))));
        assert!(original.start_race(0));
        original.update(1500);
        let (binding, bytes) = checkpoint(&original).unwrap();
        let mut restored = restore(&bytes, &binding, Box::new(NullHostTransport)).unwrap();
        for tick in 91..=330 {
            let now = (tick * 1000u64).div_ceil(60);
            original.update(now);
            restored.update(now);
            assert_eq!(
                checkpoint(&original).unwrap().1,
                checkpoint(&restored).unwrap().1,
                "battle tick {tick}"
            );
        }
    }
    // Encode a structurally canonical but semantically invalid authority,
    // bypassing the production encoder's validation as a malicious sender can.
    fn unchecked_image(host: &HostAuthority, binding: &Binding) -> Vec<u8> {
        let view = Snapshot {
            schema: SCHEMA,
            game_protocol: crate::proto::PROTO_VERSION,
            binding,
            custom: host.custom.as_ref().map(|t| t.bytes.as_slice()),
            authority: host,
            host_only: HostOnly::capture(host),
        };
        let mut bytes = MAGIC.to_vec();
        bytes.extend(options().serialize(&view).unwrap());
        bytes
    }
    #[test]
    fn canonical_images_with_invalid_parallel_arrays_references_or_numbers_fail_restore() {
        for defect in 0..9 {
            let mut bad = host();
            bad.start_race(0);
            bad.update(1500);
            let binding = binding(&bad).unwrap();
            let world = &mut bad.run.as_mut().unwrap().world;
            match defect {
                0 => world.lap_start.clear(),
                1 => world.bot_mem.clear(),
                2 => world.cfg.racers.pop().map(|_| ()).unwrap(),
                3 => world.racers[0].idx = 3,
                4 => world.racers[0].pos.x = f32::NAN,
                5 => world.racers[0].laser.on_racer = Some(99),
                6 => world.lap_times.clear(),
                7 => world.racers[0].laser.heat = f32::INFINITY,
                8 => world.max_stuck.clear(),
                _ => unreachable!(),
            }
            let bytes = unchecked_image(&bad, &binding);
            assert!(
                restore(&bytes, &binding, Box::new(NullHostTransport)).is_err(),
                "defect {defect}"
            );
        }
    }
    #[test]
    fn unsupported_corrupt_and_wrong_bindings_are_rejected() {
        let original = host();
        let (binding, bytes) = checkpoint(&original).unwrap();
        for len in [0, 1, MAGIC.len(), bytes.len() - 1] {
            assert!(restore(&bytes[..len], &binding, Box::new(NullHostTransport)).is_err());
        }
        let mut wrong = binding.clone();
        wrong.config[0] ^= 1;
        assert!(restore(&bytes, &wrong, Box::new(NullHostTransport)).is_err());
        let mut version = bytes.clone();
        version[MAGIC.len() + 3] ^= 1;
        assert!(restore(&version, &binding, Box::new(NullHostTransport)).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(restore(&trailing, &binding, Box::new(NullHostTransport)).is_err());
    }

    fn migration_bytes(part: crate::proto::MigrationPart) -> Vec<u8> {
        crate::proto::encode(&crate::proto::ToHost::Migration(part))
    }

    #[test]
    fn a_migration_record_from_a_seated_peer_leaves_the_checkpoint_unchanged() {
        use crate::proto::ToHostMigration;
        let mut host = host();
        add_human(&mut host, 0, 7);
        host.now = 5_000;
        let before = checkpoint(&host).unwrap().1;
        let ack = ToHostMigration::Ack { ack: vec![3; 1200] };
        let parts = wire::chunk(&ack).unwrap();
        assert_eq!(parts.len(), 2);
        for part in parts {
            host.on_reliable(7, &migration_bytes(part));
        }
        assert_eq!(checkpoint(&host).unwrap().1, before);
        assert_eq!(
            host.take_migration_events(),
            vec![(7, wire::MigrationEvent::Received(ack))]
        );
        // Any native message is stamped: the comparison above can see it.
        let ping = crate::proto::ToHost::Ping { id: 1, client_t: 1 };
        host.on_reliable(7, &crate::proto::encode(&ping));
        assert_ne!(checkpoint(&host).unwrap().1, before);
    }

    #[test]
    fn one_flooding_peer_cannot_crowd_out_another_peers_ack() {
        use crate::proto::ToHostMigration;
        let mut host = host();
        add_human(&mut host, 0, 7);
        add_human(&mut host, 1, 8);
        let decline = wire::chunk(&ToHostMigration::Decline).unwrap().remove(0);
        for _ in 0..10 {
            host.on_reliable(7, &migration_bytes(decline.clone()));
        }
        let ack = ToHostMigration::Ack { ack: vec![9; 64] };
        host.on_reliable(8, &migration_bytes(wire::chunk(&ack).unwrap().remove(0)));
        let events = host.take_migration_events();
        assert_eq!(
            events.iter().filter(|(p, _)| *p == 7).count(),
            wire::MAX_PENDING_PER_PEER
        );
        assert!(events.contains(&(8, wire::MigrationEvent::Received(ack))));
        assert_eq!(host.migration_dropped(), 6);
        // A departing peer's queued events go with it; the other peer's stay.
        host.on_reliable(7, &migration_bytes(decline.clone()));
        host.on_reliable(8, &migration_bytes(decline.clone()));
        host.on_left(7);
        assert_eq!(
            host.take_migration_events(),
            vec![(8, wire::MigrationEvent::Received(ToHostMigration::Decline))]
        );
    }
}
