//! Remote human players of an armed room over their real QUIC connections.
//!
//! [`PlayerHost`] is the original's side: it pins the room's `QuicHost` to the
//! armed session, gives every admitted remote human a bound player channel
//! (`QuicHost::migration_channel`, after the unchanged persona admission)
//! and feeds Fray's `PlayerInbox`/`PlayerQueue` for [`LivePrimary`]. It frames
//! only certified (committed) outputs back to their recipients. A player whose
//! connection dropped resumes on a new one through Fray's signed reconnect
//! challenge; a second connection while the player's route is live is
//! disconnected and never evicts it.
//!
//! [`RemotePlayer`] is a remote client's side: it runs Fray's `PlayerEndpoint`
//! on the client's admitted `QuicClient`, journals every reliable intent
//! through the client's transactional step, applies each certified output
//! once and sends `Applied` ACKs. The plain (non-armed) path is unchanged.
//!
//! The standby device's own player is an ordinary remote player on the
//! original's listener whose endpoint shares the standby runtime's sender
//! counter ([`RemotePlayer::connect_with_sender`]); a separate counter for
//! that key would conflict in the original's replay ledger. The original
//! phone host's own player runs in process through the same Fray path
//! ([`PlayerHost::attach_own`]): a bound in-memory channel pair, an endpoint
//! on the original's own sender and a queue attached to [`LivePrimary`].
//!
//! Every armed player frame travels on the reliable stream: Fray's migration
//! `Channel` has no datagram path, its framing splits payloads, and a lost
//! datagram would leave a partial transfer.
use super::*;
use crate::client::ClientSession;
use crate::net::{QuicClient, QuicHost};
use crate::transport::{ClientNetEvent, ClientTransport, HostNetEvent};
use decented_net::migration::{
    Armed, CapabilityAck, CertifiedOutput, Channel, ChannelSide, ConnectionBinding, Delivery,
    DeliveryJournal, Descriptor, EndpointPolicy, IntentJournal, LinkRole, MembershipCertificate,
    PacketKind, PlayerEndpoint, PlayerEvent, PlayerQueue, PlayerRecovery, Promotion, ReceiveQueue,
    ReconnectChallenge, RecoveryPlayerEvent, SenderSequence, SessionImage, MAX_BATCH,
};
use race_core::{Event, Input};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The spec's client loss detection: a client in a race (or on its results)
/// that sees no fresh committed progress for this long freezes simulation,
/// prediction, input generation and game timers. The decided value is 100 ms
/// (2026-10-17), deliberately shorter than the standby's 200 ms term-close
/// timeout so that a survivor's lead past C stays inside the 250 ms
/// clock-correction cap.
pub const FREEZE_AFTER_MS: u64 = 100;
/// The clients' bound on their own lead over the commits. Under term 0 an
/// armed client keeps its host-time estimate, before every frame it steps
/// (lobby, race and results alike), within this much past the game time of
/// the newest progress it saw, and never below it, by moving its clock
/// offset (`RemotePlayer::bound_lead`). The armed original holds its game
/// clock at the skew budget while commits are late (`LivePrimary::pace`),
/// and nothing else moves a client's offset before its next sync round, so
/// without the bound a client would read ahead of the held host. The first
/// pull-down of an episode asks a fresh sync round, which runs in the
/// background; the client never freezes for it. A takeover cuts a client at
/// its last stepped frame, at the host time that frame recorded (a round that
/// completes later, while frozen, does not move it), and C is at least the
/// newest progress it saw, so
/// its correction is at most this, whatever the host's clock did, and while
/// commits flow the cut moves on with them. Below the 250 ms correction cap
/// with a margin; above the lead of normal play (the commit round trip plus
/// one way).
pub const MAX_LEAD_MS: u64 = 200;
/// After a takeover the room is unprotected: no standby, no second takeover.
/// A client frozen this long under the successor ends the session (the
/// spec's "a second host failure ends the session"), using the spec's
/// overall recovery timeout as the bound.
pub const SESSION_LOST_AFTER_MS: u64 = 5000;

/// Why a resumed player refuses a new standby link.
pub const SECOND_TAKEOVER_REFUSED: &str = "unprotected: one takeover per session";

/// Physical player ingress the original refused before any queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayerIngress {
    /// Admitted connections refused a player route (stranger key, a second
    /// connection while the player's route is live, refused attach or
    /// resume). Each is disconnected without a ban.
    pub refused_peers: usize,
    /// Frames refused by framing, signature, replay or queue checks. Each
    /// is dropped. Only a validly signed conflicting original pauses the
    /// room (Fray faults the session, as the spec requires).
    pub refused_frames: usize,
    /// Raw datagrams from routed players. Armed players use the reliable stream.
    pub ignored_datagrams: usize,
    /// Physical reliable frames read from routed players.
    pub received_frames: usize,
    /// Physical frames whose control was accepted by a player queue.
    pub accepted_controls: usize,
    /// Verified controls a full queue refused for capacity. Each is parked
    /// on its route and retried after a commit frees capacity; none is lost.
    pub parked_controls: usize,
    /// Replacement connections which proved the player's retained journals.
    pub resumed: usize,
    /// Routed connections disconnected (no ban) after a send, framing or
    /// resume failure. The player may reconnect.
    pub disconnected: usize,
    /// Connections banned for a signed conflicting control.
    pub banned: usize,
    /// Polls that stopped at a route's ingress rate limit with frames still
    /// queued. Those frames stay queued and are read on a later poll.
    pub rate_limited: usize,
    /// Polls that left frames from the phone host's own player in its
    /// in-process pipe because its route's ingress was full. They stay in
    /// order and are read on a later poll; the route stays attached.
    pub held_local: usize,
    /// Committed outputs not framed because the recipient's approval is no
    /// longer current. Its connection is closed by the next poll.
    pub unadmitted_outputs: usize,
}

/// Where a route's current connection is in Fray's reconnect handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resume {
    /// The connection carries the queue's own inbox.
    Attached,
    /// A replacement connection waits for a membership certificate at the
    /// installed receipt (see [`PlayerHost::certify`]).
    Waiting,
    /// The signed challenge went out; the next control must be the proof.
    Challenged,
}

struct Route {
    /// The physical connection; `None` after it left.
    peer: Option<PeerKey>,
    /// The ordinary-join peer the native authority and its image route by.
    logical: PeerKey,
    channel: Channel,
    queue: PlayerQueue,
    ingress: ReceiveQueue,
    resume: Resume,
    /// A verified control the queue refused for capacity. The channel has
    /// consumed its frame, so it is retried before any later frame.
    parked: Option<Vec<u8>>,
    /// The original phone host's own player: its in-process wire. `peer` is
    /// then the logical peer, never a network connection.
    local: Option<LocalPipe>,
    /// The runtime key the route's player is a member under.
    key: [u8; 32],
    /// A committed retirement closed the queue (`refresh` returned
    /// `Closed`): nothing more is read or framed. The route waits for the
    /// membership certificate that no longer lists the player
    /// ([`PlayerHost::certify`]); installing it on the listener kicks any
    /// lingering link, and the route is then removed.
    closed: bool,
}

/// Frames [`LocalPipe`] holds in each direction (Fray's own ingress frame
/// bound). The host's own client cannot fill it towards the host: it sends
/// nothing new once it sees no committed progress for [`FREEZE_AFTER_MS`],
/// and its journal holds a bounded number of unacknowledged intents. If it
/// is ever exceeded the pipe closes rather than drop a frame (a gap).
/// Towards the client, the original stops releasing frames while the pipe
/// is full; they wait in the channel's own bounded send queue.
const LOCAL_PIPE_FRAMES: usize = 4096;
/// The bytes the host's own route moves from its pipe into its ingress at
/// once. Later frames wait in order in the pipe (backpressure) and are
/// moved on a later poll; the route is never detached for capacity.
const LOCAL_INGRESS_BYTES: usize = 16 * 1024;

#[derive(Default)]
struct PipeState {
    to_host: VecDeque<Vec<u8>>,
    to_client: VecDeque<Vec<u8>>,
    closed: bool,
    /// The client overflowed [`LOCAL_PIPE_FRAMES`] towards the host.
    overflowed: bool,
}
/// The in-memory wire between the original's queue and its own player's
/// endpoint, in one process. Frames keep their order; both directions are
/// bounded by [`LOCAL_PIPE_FRAMES`], and an overflow closes the pipe rather
/// than leave a gap.
#[derive(Clone, Default)]
struct LocalPipe(Arc<Mutex<PipeState>>);
impl LocalPipe {
    fn lock(&self) -> std::sync::MutexGuard<'_, PipeState> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
/// The host player's client end of [`LocalPipe`].
struct LocalClient(LocalPipe);
impl ClientTransport for LocalClient {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        let mut state = self.0.lock();
        match state.to_client.pop_front() {
            Some(bytes) => Some(ClientNetEvent::Reliable(bytes)),
            None if state.overflowed => Some(ClientNetEvent::Disconnected {
                host_left: false,
                reason: "local player pipe overflowed".into(),
            }),
            None if state.closed => Some(ClientNetEvent::Disconnected {
                host_left: true,
                reason: "local player route closed".into(),
            }),
            None => None,
        }
    }
    fn send_reliable(&mut self, bytes: Vec<u8>) {
        let mut state = self.0.lock();
        if state.closed {
            return;
        }
        if state.to_host.len() >= LOCAL_PIPE_FRAMES {
            state.overflowed = true;
            state.closed = true;
            return;
        }
        state.to_host.push_back(bytes);
    }
    fn send_lossy(&mut self, _: Vec<u8>) {}
    fn rtt_ms(&self) -> Option<u32> {
        Some(0)
    }
    fn close(&mut self) {
        self.0.lock().closed = true;
    }
}

/// A refused [`PlayerHost::new`]: why, and the listener, handed back so the
/// caller can retry (on `Gap` or `Stale`, after `RuntimeEvent::Membership`).
pub struct PlayerHostRefused {
    pub error: Error,
    pub network: QuicHost,
}
impl std::fmt::Debug for PlayerHostRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayerHostRefused")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// The original's player transport for an armed room.
pub struct PlayerHost {
    network: QuicHost,
    descriptor: Descriptor,
    root: decented_net::migration::Root,
    membership: MembershipCertificate,
    routes: BTreeMap<u64, Route>,
    peers: BTreeMap<PeerKey, u64>,
    ingress: PlayerIngress,
    /// The receipt index of the last `publish`, and the players it framed
    /// an output to: [`Self::certify`] sends the others the certificate.
    reached: (u64, std::collections::BTreeSet<u64>),
}
impl PlayerHost {
    /// Pin the room's listener to the armed session. Every ordinary
    /// connection must already have left (Fray refuses otherwise); players
    /// reconnect with the session-bound player role. The membership is the
    /// standby-signed certificate `live` learned over the pair link
    /// ([`LivePrimary::membership_certificate`]) at its installed receipt:
    /// the armed (index 0) certificate the players' endpoints also hold.
    /// `Gap` until it has arrived; `Stale` if it trails the receipt.
    ///
    /// A refusal hands the listener back in [`PlayerHostRefused`], so the
    /// caller keeps it and retries, for example after the next
    /// `RuntimeEvent::Membership`. Every certificate check runs before the
    /// listener is touched; only a refused pin or membership update (after
    /// those checks) can return it changed.
    pub fn new(
        network: QuicHost,
        descriptor: &Descriptor,
        live: &LivePrimary,
    ) -> std::result::Result<Self, PlayerHostRefused> {
        let membership = match Self::pin(&network, descriptor, live) {
            Ok(membership) => membership,
            Err(error) => return Err(PlayerHostRefused { error, network }),
        };
        let root = &descriptor.root;
        Ok(Self {
            network,
            descriptor: descriptor.clone(),
            root: root.clone(),
            membership,
            routes: BTreeMap::new(),
            peers: BTreeMap::new(),
            ingress: PlayerIngress::default(),
            reached: (0, Default::default()),
        })
    }
    /// The checks and the pin of [`Self::new`], on a borrowed listener.
    fn pin(
        network: &QuicHost,
        descriptor: &Descriptor,
        live: &LivePrimary,
    ) -> Result<MembershipCertificate> {
        descriptor.verify()?;
        let root = &descriptor.root;
        if root != live.root() {
            return Err(Error::Unauthorized);
        }
        let membership = live.membership_certificate().ok_or(Error::Gap)?.clone();
        membership.verify(root)?;
        let context = root.context()?;
        if membership.receipt.digest(&context)? != live.primary().receipt().digest(&context)? {
            return Err(Error::Stale);
        }
        let handle = network.handle().ok_or(Error::Closed)?;
        handle
            .pin_session(descriptor.clone())
            .map_err(|_| Error::Unauthorized)?;
        handle
            .update_session_membership(membership.clone())
            .map_err(|_| Error::Unauthorized)?;
        Ok(membership)
    }
    pub fn network(&self) -> &QuicHost {
        &self.network
    }
    pub fn ingress(&self) -> PlayerIngress {
        self.ingress
    }
    /// Players with an attached queue, connected or not.
    pub fn players(&self) -> impl Iterator<Item = u64> + '_ {
        self.routes.keys().copied()
    }
    pub fn queue(&self, player: u64) -> Option<&PlayerQueue> {
        self.routes.get(&player).map(|r| &r.queue)
    }
    /// The physical connection currently carrying `player`, if any.
    pub fn peer(&self, player: u64) -> Option<PeerKey> {
        self.routes.get(&player).and_then(|r| r.peer)
    }
    /// Whether `player`'s current connection has proved its journals (or is
    /// the original attach), so outputs are framed to it.
    pub fn attached(&self, player: u64) -> bool {
        self.routes
            .get(&player)
            .is_some_and(|r| r.peer.is_some() && r.resume == Resume::Attached)
    }
    /// Whether `player` holds a parked control refused for capacity.
    pub fn parked(&self, player: u64) -> bool {
        self.routes.get(&player).is_some_and(|r| r.parked.is_some())
    }
    /// Install the standby's signed membership certificate that `live`
    /// learned over the pair link ([`LivePrimary::membership_certificate`]),
    /// once it is the certificate for the primary's installed receipt: call
    /// this on `RuntimeEvent::Membership`, not on `Committed`, which it
    /// trails. A trailing certificate is `Stale` and changes nothing; none
    /// at all is `Gap`. A reconnect challenge must carry the certificate
    /// for the queue's exact committed base; a waiting replacement
    /// connection is challenged here.
    ///
    /// A certificate that changes the membership (a committed departure
    /// retired a player) is installed on the pinned listener first
    /// (`update_session_membership`): Fray detaches every link whose key it
    /// no longer lists and refuses that key's later connections. Then each
    /// route takes it as Fray's `PlayerInbox::update_membership` would
    /// (`PlayerQueue` does not expose its inbox): a route whose player the
    /// certificate no longer lists under its key is closed and removed. A
    /// refused listener update changes nothing here.
    pub fn certify(&mut self, live: &LivePrimary, now_ms: u64) -> Result<usize> {
        let membership = live.membership_certificate().ok_or(Error::Gap)?.clone();
        let context = self.root.context()?;
        membership.follows(&self.root, &self.membership)?;
        if membership.receipt.digest(&context)? != live.primary().receipt().digest(&context)? {
            return Err(Error::Stale);
        }
        if membership.revision != self.membership.revision
            || membership.members != self.membership.members
        {
            self.network
                .handle()
                .ok_or(Error::Closed)?
                .update_session_membership(membership.clone())
                .map_err(|_| Error::Unauthorized)?;
        }
        self.membership = membership;
        let retired = self
            .routes
            .iter()
            .filter(|(&player, r)| {
                !self
                    .membership
                    .members
                    .iter()
                    .any(|m| m.player == player && m.key == Some(r.key))
            })
            .map(|(&player, _)| player)
            .collect::<Vec<_>>();
        for player in retired {
            self.retire(player);
        }
        let waiting = self
            .routes
            .iter()
            .filter(|(_, r)| r.peer.is_some() && !r.closed && r.resume == Resume::Waiting)
            .map(|(&player, _)| player)
            .collect::<Vec<_>>();
        for player in waiting {
            self.challenge(player, live, now_ms);
        }
        // Committed progress for every attached player this commit framed no
        // output to (the countdown before snapshots, a quiet spell, the
        // results screen): the standby's signed certificate at the new
        // receipt. A client counts it only when that receipt is newer and its
        // game time advanced, so a replay or a stalled game clock refreshes
        // nothing. Same rule as the successor's idle certificates.
        let index = live.primary().receipt().index;
        let idle = self
            .routes
            .iter()
            .filter(|(player, r)| {
                r.peer.is_some()
                    && !r.closed
                    && r.resume == Resume::Attached
                    && !(self.reached.0 == index && self.reached.1.contains(*player))
            })
            .map(|(&player, _)| player)
            .collect::<Vec<_>>();
        let mut certified = 0;
        if !idle.is_empty() {
            let certificate = self.membership.encode(&self.root)?;
            let mut failed = Vec::new();
            for player in idle {
                let route = self.routes.get_mut(&player).ok_or(Error::Closed)?;
                match route.channel.enqueue(PacketKind::Control, &certificate) {
                    Ok(_) => certified += 1,
                    Err(_) => failed.push(player),
                }
            }
            for player in failed {
                self.detach(player, false);
            }
        }
        self.flush(now_ms);
        Ok(certified)
    }
    /// Remove a retired player's route. Its network link, if one lingers,
    /// was detached by the listener's membership update (reported later as
    /// that peer's `Left`, which no route claims any more). The host's own
    /// route has no listener link: its pipe is closed instead.
    fn retire(&mut self, player: u64) {
        if let Some(route) = self.routes.remove(&player) {
            if let Some(pipe) = &route.local {
                pipe.lock().closed = true;
            } else if let Some(peer) = route.peer {
                self.peers.remove(&peer);
            }
        }
    }
    /// Refuse an admitted connection without banning its key: a refused
    /// replacement or a second connection must never lock the player out.
    fn refuse_peer(&mut self, peer: PeerKey) {
        self.ingress.refused_peers += 1;
        self.network.disconnect(peer);
    }
    fn attach(&mut self, peer: PeerKey, live: &mut LivePrimary, now_ms: u64) {
        // QuicHost hands out framing only for a currently approved peer.
        let Ok(channel) = self.network.migration_channel(peer) else {
            return self.refuse_peer(peer);
        };
        let key = channel.remote_key();
        // The original runtime's key never holds a network player route:
        // the phone host's own player runs in process (`attach_own`). The
        // standby device's seat is an ordinary remote human here; its
        // endpoint must share the standby runtime's sender (Fray refuses a
        // separate counter for that key, and a conflicting reuse faults).
        if key == self.root.host {
            return self.refuse_peer(peer);
        }
        let member = SessionImage::decode(&self.root, live.primary().committed_image())
            .ok()
            .and_then(|image| image.members.into_iter().find(|m| m.key == Some(key)));
        let Some(member) = member else {
            return self.refuse_peer(peer);
        };
        if let Some(route) = self.routes.get_mut(&member.player) {
            // A second connection never evicts the live one, and a retired
            // player's closed route never resumes.
            if route.peer.is_some() || route.closed {
                return self.refuse_peer(peer);
            }
            // The player's connection dropped: this one must prove the
            // retained journals before it carries the queue.
            route.peer = Some(peer);
            route.channel = channel;
            route.ingress = ReceiveQueue::default();
            route.parked = None;
            route.resume = Resume::Waiting;
            self.peers.insert(peer, member.player);
            self.challenge(member.player, live, now_ms);
            return;
        }
        let Ok(queue) = live.attach_player(&channel, self.membership.clone()) else {
            return self.refuse_peer(peer);
        };
        self.peers.insert(peer, member.player);
        self.routes.insert(
            member.player,
            Route {
                peer: Some(peer),
                logical: member.logical_peer,
                channel,
                queue,
                ingress: ReceiveQueue::default(),
                resume: Resume::Attached,
                parked: None,
                local: None,
                key,
                closed: false,
            },
        );
    }
    /// Run the original phone host's own player through Fray's real player
    /// path, in process: a bound `{0, Player, Original}` channel pair over an
    /// in-memory wire, the client endpoint on this original's own sender
    /// (`PlayerEndpoint::new_with_sender` on [`LivePrimary::sender_sequence`])
    /// and the server end attached with [`LivePrimary::attach_player`]. Its
    /// intents and views are journaled and committed like a remote player's,
    /// its outputs are framed only after their receipt and apply once, and
    /// its `Applied` ACKs prune its outbox. Refused unless `keys` are the
    /// original runtime's own and the root seats that key. Call it at arming,
    /// before the first commit, with the armed (index 0) certificate.
    pub fn attach_own(
        &mut self,
        live: &mut LivePrimary,
        armed: &Armed,
        capability: &CapabilityAck,
        client: ClientSession,
        keys: decented_net::Keys,
    ) -> Result<RemotePlayer> {
        let key = keys.public_key().to_bytes();
        if key != self.root.host || live.root() != &self.root {
            return Err(Error::Unauthorized);
        }
        let member = SessionImage::decode(&self.root, live.primary().committed_image())?
            .members
            .into_iter()
            .find(|m| m.key == Some(key))
            .ok_or(Error::Unauthorized)?;
        if self.routes.contains_key(&member.player) {
            return Err(Error::Conflict);
        }
        let descriptor = self.descriptor.clone();
        let policy = EndpointPolicy {
            descriptor: descriptor.clone(),
            role: LinkRole::Original,
            promotion: None,
            membership: Some(self.membership.clone()),
        };
        let binding = ConnectionBinding {
            session: self.root.session,
            descriptor: self.root.digest()?,
            term: 0,
            client: LinkRole::Player,
            server: LinkRole::Original,
        };
        let mut tls = [0u8; 32];
        getrandom::fill(&mut tls).map_err(|_| Error::Invalid("local route entropy"))?;
        let near =
            Channel::new_bound(&policy, binding.clone(), ChannelSide::Client, key, key, tls)?;
        let far = Channel::new_bound(&policy, binding, ChannelSide::Server, key, key, tls)?;
        if far.remote_key() != self.root.host {
            return Err(Error::Unauthorized);
        }
        let endpoint = PlayerEndpoint::new_with_sender(
            descriptor,
            armed,
            capability,
            self.membership.clone(),
            near,
            keys,
            live.sender_sequence(),
        )?;
        let queue = live.attach_player(&far, self.membership.clone())?;
        let pipe = LocalPipe::default();
        self.routes.insert(
            member.player,
            Route {
                peer: Some(member.logical_peer),
                logical: member.logical_peer,
                channel: far,
                queue,
                ingress: ReceiveQueue::default(),
                resume: Resume::Attached,
                parked: None,
                local: Some(pipe.clone()),
                key,
                closed: false,
            },
        );
        Ok(RemotePlayer::from_endpoint(
            endpoint,
            policy,
            client,
            Box::new(LocalClient(pipe)),
        ))
    }
    /// Send a waiting replacement connection the original's signed
    /// challenge. A certificate behind the installed receipt waits for
    /// [`Self::certify`]; any other refusal disconnects the connection.
    fn challenge(&mut self, player: u64, live: &LivePrimary, now_ms: u64) {
        let Some(route) = self.routes.get_mut(&player) else {
            return;
        };
        if route.peer.is_none() || route.resume != Resume::Waiting {
            return;
        }
        let request = live.begin_player_reconnect(
            &mut route.queue,
            &route.channel,
            self.membership.clone(),
            now_ms,
        );
        let sent = request.and_then(|request| {
            route
                .channel
                .enqueue(PacketKind::Control, &request.encode(&self.root)?)
        });
        match sent {
            Ok(_) => route.resume = Resume::Challenged,
            Err(Error::Stale) => {}
            Err(_) => {
                self.ingress.refused_peers += 1;
                self.detach(player, false);
            }
        }
    }
    /// Drop `player`'s connection and keep its queue for a reconnect. Only a
    /// signed conflicting control (`ban`) refuses the key for the room.
    fn detach(&mut self, player: u64, ban: bool) {
        if let Some(route) = self.routes.get_mut(&player) {
            route.parked = None;
            if let Some(peer) = route.peer.take() {
                if let Some(pipe) = &route.local {
                    // The host's own route has no reconnect: closing it
                    // leaves its reliable outputs in the outbox.
                    pipe.lock().closed = true;
                    self.ingress.disconnected += 1;
                    return;
                }
                self.peers.remove(&peer);
                if ban {
                    self.ingress.banned += 1;
                    self.network.kick(peer);
                } else {
                    self.ingress.disconnected += 1;
                    self.network.disconnect(peer);
                }
            }
        }
    }
    /// After a resume, frame every retained reliable output the player has
    /// not ACKed. Lossy outputs missed meanwhile are not replayed.
    fn replay(&mut self, player: u64, live: &LivePrimary) -> Result<()> {
        let context = self.root.context()?;
        let route = self.routes.get_mut(&player).ok_or(Error::Closed)?;
        let ack = route.queue.output_ack();
        for output in live.primary().outbox() {
            if output.output.recipient == player && output.output.sequence > ack {
                route
                    .channel
                    .enqueue(PacketKind::Output, &output.encode(&context)?)?;
            }
        }
        Ok(())
    }
    /// Read physical events and feed verified controls into the queues.
    /// A refused peer or frame is dropped and counted. Only Fray itself
    /// pauses the room: a validly signed conflicting original faults the
    /// session (the spec requires it), and that connection is banned.
    pub fn poll(&mut self, live: &mut LivePrimary, now_ms: u64) -> Result<()> {
        for _ in 0..MAX_BATCH {
            let Some(event) = self.network.poll() else {
                break;
            };
            match event {
                HostNetEvent::Joined { peer, .. } => self.attach(peer, live, now_ms),
                HostNetEvent::Left { peer } => {
                    let Some(&player) = self.peers.get(&peer) else {
                        continue;
                    };
                    // A signed intent that reached the host is never dropped:
                    // read every frame this connection already delivered (a
                    // `Leave` just before it closed) into the queue before
                    // the route is parked. What capacity or the rate limit
                    // holds back stays on the route and is read later.
                    let route = self.routes.get_mut(&player).ok_or(Error::Closed)?;
                    // A retired player's closed route reads nothing more.
                    if !route.closed {
                        let read = read_route(route, &mut self.ingress, live.primary(), now_ms);
                        if let Some(ban) = read.dropped {
                            self.detach(player, ban);
                        }
                    }
                    self.peers.remove(&peer);
                    if let Some(route) = self.routes.get_mut(&player) {
                        route.peer = None;
                    }
                }
                HostNetEvent::Reliable { peer, bytes } => {
                    let Some(&player) = self.peers.get(&peer) else {
                        self.ingress.refused_frames += 1;
                        continue;
                    };
                    let route = self.routes.get_mut(&player).ok_or(Error::Closed)?;
                    if route.ingress.enqueue(bytes).is_ok() {
                        self.ingress.received_frames += 1;
                    } else {
                        // A dropped frame would leave a gap on this
                        // connection: close it without a ban and let the
                        // player resume, which re-sends from its journals.
                        self.ingress.refused_frames += 1;
                        self.detach(player, false);
                    }
                }
                HostNetEvent::Lossy { .. } => self.ingress.ignored_datagrams += 1,
            }
        }
        let mut dropped = Vec::new();
        let mut resumed = Vec::new();
        for (&player, route) in &mut self.routes {
            // A parked route keeps reading what its closed connection had
            // already delivered. A retired player's closed route reads
            // nothing: its lingering link waits for the membership update.
            if route.closed
                || (route.peer.is_none()
                    && route.parked.is_none()
                    && route.ingress.queued_bytes() == 0)
            {
                continue;
            }
            if let Some(pipe) = route.local.as_ref().filter(|_| route.peer.is_some()) {
                // Backpressure, never a detach for capacity: a frame the
                // ingress cannot take now stays at the front of the pipe and
                // is moved, in order, on a later poll.
                let mut state = pipe.lock();
                while route.ingress.queued_bytes() < LOCAL_INGRESS_BYTES {
                    let Some(bytes) = state.to_host.pop_front() else {
                        break;
                    };
                    if bytes.is_empty() || bytes.len() > decented_net::MAX_RELIABLE_LEN {
                        // Fray's own framing never emits this: a fault.
                        self.ingress.refused_frames += 1;
                        dropped.push((player, false));
                        break;
                    }
                    if route.ingress.enqueue(bytes.clone()).is_ok() {
                        self.ingress.received_frames += 1;
                    } else {
                        state.to_host.push_front(bytes);
                        break;
                    }
                }
                if !state.to_host.is_empty() {
                    self.ingress.held_local += 1;
                } else if state.closed {
                    // The host's own player left (or overflowed the pipe):
                    // detach its route once every frame it sent is read.
                    dropped.push((player, false));
                }
            }
            let read = read_route(route, &mut self.ingress, live.primary(), now_ms);
            if let Some(ban) = read.dropped {
                dropped.push((player, ban));
            }
            if read.resumed && route.peer.is_some() {
                resumed.push(player);
            }
        }
        for (player, ban) in dropped {
            self.detach(player, ban);
        }
        for player in resumed {
            if self.replay(player, live).is_err() {
                self.detach(player, false);
            }
        }
        self.flush(now_ms);
        // A network route whose queue no proven connection carries is
        // parked: it has no connection, or a replacement connection that has
        // not proved its journals yet (it may have taken the route in the
        // same poll as the old link's `Left`, or after a host-side detach,
        // so the route is never seen without a connection). The runner
        // stamps the park on its own clock and departs the player once it
        // has been parked for longer than `PARKED_DEPARTURE_MS`. Only an
        // attached connection (journals proved) cancels it. The host's own
        // (local) route never parks, and a retired player's closed route
        // waits for its certificate.
        for (&player, route) in &self.routes {
            if route.closed || route.local.is_some() {
                continue;
            }
            if route.peer.is_some() && route.resume == Resume::Attached {
                live.unpark(player);
            } else {
                live.park(player)?;
            }
        }
        Ok(())
    }
    /// Stage one batch over every attached queue, with each connected
    /// player's real RTT under its logical peer.
    pub fn stage(
        &mut self,
        live: &mut LivePrimary,
        now: u64,
        operation: impl FnMut(&mut HostAuthority) -> Result<()>,
    ) -> Result<bool> {
        self.stage_with(live, now, &[], operation)
    }
    /// As [`Self::stage`], plus queues this host holds no route for (test
    /// fixtures that attach a queue directly).
    pub fn stage_with(
        &mut self,
        live: &mut LivePrimary,
        now: u64,
        extra: &[&PlayerQueue],
        operation: impl FnMut(&mut HostAuthority) -> Result<()>,
    ) -> Result<bool> {
        let rtts = self
            .routes
            .values()
            .filter(|r| !r.closed)
            .map(|r| match (&r.local, r.peer) {
                (Some(_), Some(_)) => (r.logical, Some(0)),
                (_, peer) => (r.logical, peer.and_then(|p| self.network.rtt_ms(p))),
            })
            .collect::<BTreeMap<_, _>>();
        let mut queues = self
            .routes
            .values()
            .filter(|r| !r.closed)
            .map(|r| &r.queue)
            .collect::<Vec<_>>();
        queues.extend_from_slice(extra);
        live.stage(now, &queues, &rtts, operation)
    }
    /// After `RuntimeEvent::Committed`: refresh every queue to the new
    /// receipt, discard the selected views, then frame each committed output
    /// to its connected recipient. An output not proven by an installed
    /// receipt of this primary is refused before anything is sent. A queue
    /// the commit retired closes its route here; the route is removed, and
    /// any lingering link kicked, by the membership certificate that follows
    /// ([`Self::certify`]).
    pub fn publish(
        &mut self,
        live: &LivePrimary,
        outputs: &[CertifiedOutput],
        selected_views: &[(u64, u64)],
        now_ms: u64,
    ) -> Result<usize> {
        let context = self.root.context()?;
        let receipt = live.primary().receipt();
        for output in outputs {
            output.verify(&context)?;
            if output.receipt.term != receipt.term || output.receipt.index > receipt.index {
                return Err(Error::Stale);
            }
        }
        for (&player, route) in &mut self.routes {
            if route.closed {
                continue;
            }
            match route.queue.refresh(live.primary()) {
                Ok(()) => {}
                Err(Error::Closed) => {
                    route.closed = true;
                    route.parked = None;
                    continue;
                }
                Err(error) => return Err(error),
            }
            for (owner, number) in selected_views {
                if *owner == player {
                    route.queue.discard_lossy(*number);
                }
            }
        }
        let mut framed = 0;
        let mut failed = Vec::new();
        if self.reached.0 != receipt.index {
            self.reached = (receipt.index, Default::default());
        }
        for output in outputs {
            let player = output.output.recipient;
            let Some(route) = self.routes.get_mut(&player) else {
                continue;
            };
            // A replacement connection gets the retained outbox once it
            // proves its journals; until then nothing is framed to it.
            let Some(peer) = route.peer else {
                continue;
            };
            if route.closed || route.resume != Resume::Attached || failed.contains(&player) {
                continue;
            }
            // Admission is re-checked per output: an expired approval gets
            // nothing more, and the next poll closes its connection.
            if route.local.is_none() && self.network.migration_peer_key(peer).is_err() {
                self.ingress.unadmitted_outputs += 1;
                continue;
            }
            match route
                .channel
                .enqueue(PacketKind::Output, &output.encode(&context)?)
            {
                Ok(_) => {
                    framed += 1;
                    self.reached.1.insert(player);
                }
                // Backpressure: nothing later is framed to this player (no
                // gap on the connection), which is closed without a ban.
                // Reliable outputs stay in the Primary outbox and are
                // re-framed when the player resumes.
                Err(_) => failed.push(player),
            }
        }
        for player in failed {
            self.detach(player, false);
        }
        self.flush(now_ms);
        Ok(framed)
    }
    /// Release framed bytes paced by each channel. A send failure
    /// disconnects that player without a ban; its queue waits for a resume.
    pub fn flush(&mut self, now_ms: u64) {
        let mut failed = Vec::new();
        for (&player, route) in &mut self.routes {
            let Some(peer) = route.peer else {
                continue;
            };
            loop {
                // The host's own pipe is bounded: while it is full (or its
                // client left), frames wait in the channel's send queue.
                if let Some(pipe) = &route.local {
                    let state = pipe.lock();
                    if state.closed || state.to_client.len() >= LOCAL_PIPE_FRAMES {
                        break;
                    }
                }
                match route.channel.poll_send(now_ms) {
                    Ok(Some(wire)) => match &route.local {
                        Some(pipe) => pipe.lock().to_client.push_back(wire),
                        None => self.network.send_reliable(peer, wire),
                    },
                    Ok(None) => break,
                    Err(_) => {
                        failed.push(player);
                        break;
                    }
                }
            }
        }
        for player in failed {
            self.detach(player, false);
        }
    }
}

/// What one remote client frame did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteFrame {
    /// Certified reliable outputs applied for the first time.
    pub reliable_applied: usize,
    /// Certified lossy outputs (snapshots) applied.
    pub lossy_applied: usize,
    /// Certified outputs already applied: no game effect.
    pub duplicates: usize,
    /// Reliable original intents (number, game bytes) queued to the original.
    pub intents: Vec<(u64, Vec<u8>)>,
    /// The newest commit index proving an output received this frame.
    pub newest_receipt: Option<u64>,
    /// Lossy views (input datagrams) queued to the original.
    pub views_sent: usize,
    /// The `Applied` output watermark sent this frame, if any.
    pub acked: Option<u64>,
    /// Reading stopped at the ingress rate limit with frames still queued;
    /// they are read on a later frame.
    pub rate_limited: bool,
    /// Loss detection froze this frame: no client step ran (no simulation,
    /// prediction, input or timers); network work continued.
    pub frozen: bool,
    /// Input packets resent on the first frame after `Resume`.
    pub resent: usize,
    /// Commits that framed this player no output, seen as the signed
    /// certificate at a newer receipt with advanced game time (fresh
    /// progress): the standby's in term 0, the successor's after `Resume`.
    pub idle_progress: usize,
    /// Signed membership certificates received, whether or not they were
    /// fresh progress.
    pub certificates: usize,
}

/// A remote client's armed player link.
pub struct RemotePlayer {
    network: Box<dyn ClientTransport>,
    endpoint: PlayerEndpoint,
    client: ClientSession,
    ingress: ReceiveQueue,
    policy: EndpointPolicy,
    /// A replacement connection's framing, until the original's challenge.
    resuming: Option<Channel>,
    /// A received wire the full ingress refused. It is enqueued first on the
    /// next frame, and the transport is not read until it fits.
    held: Option<Vec<u8>>,
    acked: u64,
    sent: u64,
    /// The prepared control path to the standby, idle until a takeover.
    warm: Option<WarmLink>,
    /// Local time of the last frame that ran a client step (the freeze
    /// point for clock correction).
    last_frame: Option<u64>,
    /// The client's host-time estimate as that frame left it: a takeover
    /// cuts here, although a sync round may complete while frozen.
    last_host: Option<u64>,
    /// Local time of the last fresh committed progress (a received output or
    /// certificate at a newer receipt whose game time advanced), and that
    /// receipt.
    last_progress: Option<u64>,
    newest: Option<(u64, u64)>,
    /// The game time of the newest receipt that counted as progress.
    newest_logical: Option<u64>,
    /// Local time loss detection froze this client, while frozen.
    frozen_at: Option<u64>,
    /// The lead bound ([`MAX_LEAD_MS`]) pulled the clock down in this
    /// episode and asked a clock resync ([`Self::bound_lead`]).
    lead_resync: bool,
    /// Resumed under the takeover's successor: unprotected, and the
    /// unconfirmed input window is resent on the first frame.
    successor: bool,
    resent: bool,
}
/// A prepared `ControlPlayer` to `Standby` link: its channel, transport and
/// ordered ingress. The standby's signed `Promote` is its first frame.
struct WarmLink {
    channel: Channel,
    network: Box<dyn ClientTransport>,
    ingress: ReceiveQueue,
}
impl WarmLink {
    fn read(&mut self) -> Result<()> {
        for _ in 0..MAX_BATCH {
            match self.network.poll() {
                None => break,
                Some(ClientNetEvent::Reliable(bytes)) => self.ingress.enqueue(bytes)?,
                Some(ClientNetEvent::Failed(_)) | Some(ClientNetEvent::Disconnected { .. }) => {
                    return Err(Error::Closed)
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}
impl RemotePlayer {
    /// Attach to the client's admitted, session-bound connection. Its signed
    /// capability ACK and the armed certificate pin the session and roster.
    pub fn connect(
        descriptor: Descriptor,
        armed: &Armed,
        capability: &CapabilityAck,
        membership: MembershipCertificate,
        network: QuicClient,
        client: ClientSession,
        keys: decented_net::Keys,
    ) -> Result<Self> {
        let policy = EndpointPolicy {
            descriptor: descriptor.clone(),
            role: LinkRole::Original,
            promotion: None,
            membership: Some(membership.clone()),
        };
        let channel = network.migration_channel(&policy)?;
        Self::with_transport(
            descriptor,
            armed,
            capability,
            membership,
            channel,
            Box::new(network),
            client,
            keys,
        )
    }
    /// As [`Self::connect`], for a caller which already took `channel` from
    /// the same admitted `QuicClient` that `network` carries (for example a
    /// shared handle). The original refuses frames from any other connection.
    #[allow(clippy::too_many_arguments)]
    pub fn with_transport(
        descriptor: Descriptor,
        armed: &Armed,
        capability: &CapabilityAck,
        membership: MembershipCertificate,
        channel: Channel,
        network: Box<dyn ClientTransport>,
        client: ClientSession,
        keys: decented_net::Keys,
    ) -> Result<Self> {
        Self::with_transport_and_sender(
            descriptor, armed, capability, membership, channel, network, client, keys, None,
        )
    }
    /// As [`Self::connect`]; `sender` is the standby runtime's
    /// [`LiveStandby::sender_sequence`] when this is the standby device's own
    /// player (Fray refuses that key without it). Every other player passes
    /// `None` and gets its own counter from its capability ACK.
    #[allow(clippy::too_many_arguments)]
    pub fn connect_with_sender(
        descriptor: Descriptor,
        armed: &Armed,
        capability: &CapabilityAck,
        membership: MembershipCertificate,
        network: QuicClient,
        client: ClientSession,
        keys: decented_net::Keys,
        sender: Option<SenderSequence>,
    ) -> Result<Self> {
        let policy = EndpointPolicy {
            descriptor: descriptor.clone(),
            role: LinkRole::Original,
            promotion: None,
            membership: Some(membership.clone()),
        };
        let channel = network.migration_channel(&policy)?;
        Self::with_transport_and_sender(
            descriptor,
            armed,
            capability,
            membership,
            channel,
            Box::new(network),
            client,
            keys,
            sender,
        )
    }
    /// As [`Self::with_transport`], with the shared-sender option of
    /// [`Self::connect_with_sender`].
    #[allow(clippy::too_many_arguments)]
    pub fn with_transport_and_sender(
        descriptor: Descriptor,
        armed: &Armed,
        capability: &CapabilityAck,
        membership: MembershipCertificate,
        channel: Channel,
        network: Box<dyn ClientTransport>,
        client: ClientSession,
        keys: decented_net::Keys,
        sender: Option<SenderSequence>,
    ) -> Result<Self> {
        let policy = EndpointPolicy {
            descriptor: descriptor.clone(),
            role: LinkRole::Original,
            promotion: None,
            membership: Some(membership.clone()),
        };
        let endpoint = match sender {
            Some(sender) => PlayerEndpoint::new_with_sender(
                descriptor, armed, capability, membership, channel, keys, sender,
            )?,
            None => PlayerEndpoint::new(descriptor, armed, capability, membership, channel, keys)?,
        };
        Ok(Self::from_endpoint(endpoint, policy, client, network))
    }
    fn from_endpoint(
        endpoint: PlayerEndpoint,
        policy: EndpointPolicy,
        client: ClientSession,
        network: Box<dyn ClientTransport>,
    ) -> Self {
        Self {
            network,
            endpoint,
            client,
            ingress: ReceiveQueue::default(),
            policy,
            resuming: None,
            held: None,
            acked: 0,
            sent: 0,
            warm: None,
            last_frame: None,
            last_host: None,
            last_progress: None,
            newest: None,
            newest_logical: None,
            frozen_at: None,
            lead_resync: false,
            successor: false,
            resent: false,
        }
    }
    /// At a takeover, the standby device's own player becomes the promoted
    /// device's local seat: its actual client and the delivery and intent
    /// journals its real endpoint kept since arming (for [`OwnSeat::new`]).
    /// The link to the original is closed.
    pub fn into_parts(mut self) -> Result<(ClientSession, DeliveryJournal, IntentJournal)> {
        let (delivery, intents) = self.endpoint.journals();
        let delivery = DeliveryJournal::restore(&delivery.checkpoint()?)?;
        let intents = IntentJournal::restore(&intents.checkpoint()?)?;
        self.network.close();
        // The app read the last frame's events; recovery runs no frame.
        self.client.migration_take_view_events();
        Ok((self.client, delivery, intents))
    }
    /// Whether a prepared standby link protects this player (term 0 only).
    pub fn protected(&self) -> bool {
        self.warm.is_some()
    }
    /// Whether this player resumed under the takeover's successor. The room
    /// is then unprotected: there is no standby and no second takeover.
    pub fn under_successor(&self) -> bool {
        self.successor
    }
    /// Local time loss detection froze this client, while it is frozen.
    pub fn frozen_at(&self) -> Option<u64> {
        self.frozen_at
    }
    /// Local time of the last fresh committed progress.
    pub fn last_progress(&self) -> Option<u64> {
        self.last_progress
    }
    /// The newest receipt (term, index) this client has seen, whether or
    /// not it was fresh progress.
    pub fn newest_receipt(&self) -> Option<(u64, u64)> {
        self.newest
    }
    /// Fresh committed progress, the spec's rule (as Fray's
    /// `ProgressWatch`): a receipt newer than any seen whose game time
    /// advanced. A newer receipt at the same game time (an ACK-only or drain
    /// commit, or a stalled game clock) is not progress, so neither an
    /// output nor a certificate can mask a stalled commit frontier.
    fn observe_receipt(&mut self, receipt: &decented_net::migration::Receipt, now: u64) -> bool {
        if self
            .newest
            .is_some_and(|n| (receipt.term, receipt.index) <= n)
        {
            return false;
        }
        self.newest = Some((receipt.term, receipt.index));
        if self.newest_logical.is_some_and(|l| receipt.logical_ms <= l) {
            return false;
        }
        self.newest_logical = Some(receipt.logical_ms);
        self.last_progress = Some(now);
        self.frozen_at = None;
        true
    }
    /// Local time of the last frame that ran a client step.
    pub fn last_frame(&self) -> Option<u64> {
        self.last_frame
    }
    /// The client's host-time estimate at [`Self::last_frame`], as that
    /// frame left it (bounded to the commits under term 0). A takeover's cut
    /// is this value, not the frame's local time read through the clock as
    /// it stands at the cut. `None` while unsynced then, or before a stepped
    /// frame.
    pub fn last_frame_host(&self) -> Option<u64> {
        self.last_host
    }
    /// Under term 0, before a stepped frame at `now`: keep the client's
    /// host-time estimate within `[newest, newest + MAX_LEAD_MS]` of the
    /// newest progress it saw, by moving its clock offset ([`MAX_LEAD_MS`]).
    /// The first time in an episode that the estimate is pulled down (the
    /// host held its game clock), a fresh clock-sync round is asked, which
    /// runs in the background; the episode ends on a stepped frame inside
    /// the bound with no round open, so a flapping bound never aborts the
    /// round it asked for.
    fn bound_lead(&mut self, now: u64) {
        let Some(newest) = self.newest_logical else {
            return;
        };
        let pulled = self
            .client
            .migration_bound_clock(now, newest, newest + MAX_LEAD_MS)
            < 0;
        if pulled && !self.lead_resync {
            self.client.migration_resync_clock();
            self.lead_resync = true;
        } else if !pulled && !self.client.clock_sync().round_open() {
            self.lead_resync = false;
        }
    }
    /// Prepare this player's control path to the standby on `network`, an
    /// admitted session-bound `ControlPlayer` connection to the standby's
    /// passive listener. It carries nothing until the standby promotes.
    pub fn prepare_standby(&mut self, network: QuicClient) -> Result<()> {
        // One automatic takeover per session: a resumed player never
        // prepares another standby link.
        if self.successor {
            return Err(Error::Invalid(SECOND_TAKEOVER_REFUSED));
        }
        // An authority runtime's own player never dials the standby: the
        // standby's seat is local to it and the original's leaves with it.
        let root = &self.policy.descriptor.root;
        let own = root.member(self.endpoint.player())?.key;
        if own == Some(root.host) || own == Some(root.standby) {
            return Err(Error::Unauthorized);
        }
        let policy = EndpointPolicy {
            role: LinkRole::Standby,
            ..self.policy.clone()
        };
        let channel = network.migration_channel(&policy)?;
        if channel.remote_key() != policy.descriptor.root.standby {
            return Err(Error::Unauthorized);
        }
        self.warm = Some(WarmLink {
            channel,
            network: Box::new(network),
            ingress: ReceiveQueue::default(),
        });
        Ok(())
    }
    /// Read the prepared standby link for the standby's signed `Promote`
    /// (verified against the pinned root). Later frames stay queued, in
    /// order, for recovery. Any other first frame is refused.
    pub fn watch_standby(&mut self, wall: u64) -> Result<Option<Promotion>> {
        let warm = self.warm.as_mut().ok_or(Error::Closed)?;
        warm.read()?;
        let context = self.policy.descriptor.root.context()?;
        loop {
            let channel = &mut warm.channel;
            let Some(received) = warm.ingress.poll(|wire| channel.receive(wire, wall))? else {
                return Ok(None);
            };
            match received {
                None => continue,
                Some((PacketKind::Control, _, bytes)) => {
                    return Ok(Some(Promotion::decode(&context, &bytes)?))
                }
                Some(_) => return Err(Error::Unauthorized),
            }
        }
    }
    /// Freeze this client and move its same endpoint (journals, sender
    /// namespace) and its prepared standby link into Fray's player recovery
    /// at the promotion's frozen time C. The link to the original is closed.
    pub fn recover(mut self, promotion: Promotion, wall: u64) -> Result<RecoveringPlayer> {
        self.not_left()?;
        let warm = self.warm.take().ok_or(Error::Closed)?;
        let logical_ms = promotion.final_receipt.logical_ms;
        // After `Resume` a dropped link reconnects to the successor (term 1,
        // the promotion pinned); the roster comes from the descriptor until
        // the successor's challenge carries its membership certificate.
        let policy = EndpointPolicy {
            descriptor: self.policy.descriptor.clone(),
            role: LinkRole::Successor,
            promotion: Some(promotion.clone()),
            membership: None,
        };
        let recovery = self.endpoint.into_recovery(warm.channel, promotion, wall)?;
        self.network.close();
        Ok(RecoveringPlayer {
            recovery,
            client: self.client,
            network: warm.network,
            ingress: warm.ingress,
            policy,
            logical_ms,
            frozen_local: self.last_frame.unwrap_or(0),
            frozen_host: self.last_host,
            clock_delta: None,
            round_open_at_cut: None,
            resumed: false,
            resumed_local: None,
        })
    }
    /// Resume on a new admitted, session-bound connection after the previous
    /// one dropped. The original challenges it; the endpoint proves the
    /// journals it retained (Fray `PlayerEndpoint::reconnect`). Pending
    /// intents above the committed watermark are sent again, with their
    /// original IDs.
    pub fn reconnect(&mut self, network: QuicClient) -> Result<()> {
        self.not_left()?;
        let channel = network.migration_channel(&self.policy)?;
        self.resume_with_transport(channel, Box::new(network))
    }
    /// As [`Self::reconnect`], for a caller which already took `channel`
    /// from the same new connection that `network` carries.
    pub fn resume_with_transport(
        &mut self,
        channel: Channel,
        network: Box<dyn ClientTransport>,
    ) -> Result<()> {
        self.not_left()?;
        if let Some(error) = self.endpoint.failure() {
            return Err(error.clone());
        }
        self.network.close();
        self.network = network;
        self.ingress = ReceiveQueue::default();
        self.held = None;
        self.resuming = Some(channel);
        Ok(())
    }
    /// A client that left (`Closed("left")`) never offers a resume proof:
    /// neither a reconnect to the original nor a takeover resume.
    fn not_left(&self) -> Result<()> {
        match self.client.phase() {
            crate::client::ClientPhase::Closed(reason) if reason == "left" => Err(Error::Closed),
            _ => Ok(()),
        }
    }
    /// Whether a replacement connection still waits for its challenge.
    pub fn resuming(&self) -> bool {
        self.resuming.is_some()
    }
    /// Read the original's challenge on the replacement connection and
    /// switch the endpoint to it. False while the challenge has not arrived.
    fn resume(&mut self, wall: u64) -> Result<bool> {
        for _ in 0..MAX_BATCH {
            let Some(channel) = self.resuming.as_mut() else {
                return Ok(true);
            };
            let Some(received) = self.ingress.poll(|wire| channel.receive(wire, wall))? else {
                return Ok(false);
            };
            let Some((kind, _, bytes)) = received else {
                continue;
            };
            if kind != PacketKind::Control {
                return Err(Error::Unauthorized);
            }
            let request = ReconnectChallenge::decode(&self.policy.descriptor.root, &bytes)?;
            let channel = self.resuming.take().ok_or(Error::Closed)?;
            self.endpoint.reconnect(channel, &request)?;
            self.policy.membership = Some(request.membership);
            return Ok(true);
        }
        Ok(false)
    }
    pub fn client(&self) -> &ClientSession {
        &self.client
    }
    pub fn endpoint(&self) -> &PlayerEndpoint {
        &self.endpoint
    }
    /// Drop the events the app already read from the view. A pump which runs
    /// no frame for this player (its link was lost and only the standby is
    /// watched) calls this, so no frame's events are read more than once.
    pub fn discard_view_events(&mut self) {
        self.client.migration_take_view_events();
    }
    /// The newest output watermark sent in an `Applied` ACK.
    pub fn acked(&self) -> u64 {
        self.acked
    }
    /// One client frame at local time `now` (`wall` paces framing): apply
    /// received certified outputs through the client's transaction, run the
    /// frame, journal and send its reliable intents and input views, and
    /// ACK newly applied outputs. The view's events are this frame's: those
    /// of every step it ran, in order (none when it ran no step).
    pub fn frame(&mut self, now: u64, wall: u64, input: Input) -> Result<RemoteFrame> {
        self.frame_with(now, wall, input, |_| {})
    }
    /// As [`Self::frame`], with a local UI command (`command`, for example
    /// `set_character`) run inside the frame's own client transaction, so
    /// its reliable intent is journaled before it is sent.
    pub fn frame_with(
        &mut self,
        now: u64,
        wall: u64,
        input: Input,
        command: impl FnOnce(&mut ClientSession),
    ) -> Result<RemoteFrame> {
        // The app read the previous frame's events. Each step replaces the
        // view's events with its own, so they are collected per step and
        // left on the view whatever the frame returns: a step which committed
        // is not replayed, so its events would never come back.
        self.client.migration_take_view_events();
        let mut surfaced = Vec::new();
        let result = self.frame_steps(now, wall, input, command, &mut surfaced);
        self.client.migration_set_view_events(surfaced);
        result
    }
    /// The body of [`Self::frame_with`]; the events of every step which
    /// committed are pushed to `surfaced`, also when the frame then fails.
    fn frame_steps(
        &mut self,
        now: u64,
        wall: u64,
        input: Input,
        command: impl FnOnce(&mut ClientSession),
        surfaced: &mut Vec<Event>,
    ) -> Result<RemoteFrame> {
        if let Some(error) = self.endpoint.failure() {
            return Err(error.clone());
        }
        let mut frame = RemoteFrame::default();
        if let Some(wire) = self.held.take() {
            if let Err(error) = self.ingress.enqueue(wire.clone()) {
                if wire.is_empty() || wire.len() > decented_net::MAX_RELIABLE_LEN {
                    return Err(error);
                }
                self.held = Some(wire);
            }
        }
        for _ in 0..MAX_BATCH {
            if self.held.is_some() {
                break;
            }
            match self.network.poll() {
                None => break,
                Some(ClientNetEvent::Reliable(bytes)) => {
                    if let Err(error) = self.ingress.enqueue(bytes.clone()) {
                        if bytes.is_empty() || bytes.len() > decented_net::MAX_RELIABLE_LEN {
                            return Err(error);
                        }
                        self.held = Some(bytes);
                    }
                }
                Some(ClientNetEvent::Lossy(_))
                | Some(ClientNetEvent::Progress(_))
                | Some(ClientNetEvent::Connected { .. }) => {}
                Some(ClientNetEvent::Failed(_)) | Some(ClientNetEvent::Disconnected { .. }) => {
                    return Err(Error::Closed)
                }
            }
        }
        if !self.resume(wall)? {
            return Ok(frame);
        }
        let rtt = self.network.rtt_ms();
        let mut views = Vec::new();
        for _ in 0..MAX_BATCH {
            let (endpoint, client) = (&mut self.endpoint, &mut self.client);
            let received = self.ingress.poll(|wire| {
                endpoint.receive(wire, wall, |delivery, bytes, intents| {
                    let event = match delivery {
                        Delivery::Reliable => ClientNetEvent::Reliable(bytes.to_vec()),
                        Delivery::Lossy => ClientNetEvent::Lossy(bytes.to_vec()),
                    };
                    let batch =
                        client.migration_step(intents, now, input, &[event], rtt, |_| Ok(()))?;
                    surfaced.extend(client.migration_take_view_events());
                    views.extend(batch.lossy);
                    Ok(())
                })
            })?;
            match received {
                None => {
                    frame.rate_limited |= self.ingress.queued_bytes() > 0;
                    break;
                }
                Some(Some(PlayerEvent::Output {
                    receipt,
                    delivery,
                    applied,
                    ..
                })) => {
                    frame.newest_receipt = frame.newest_receipt.max(Some(receipt.index));
                    self.observe_receipt(&receipt, now);
                    match (delivery, applied) {
                        (_, false) => frame.duplicates += 1,
                        (Delivery::Reliable, true) => frame.reliable_applied += 1,
                        (Delivery::Lossy, true) => frame.lossy_applied += 1,
                    }
                }
                // A commit that framed this player no output carries the
                // signed membership certificate at its receipt instead (the
                // standby's in term 0, the successor's after `Resume`).
                Some(Some(PlayerEvent::Membership(certificate))) => {
                    frame.certificates += 1;
                    if self.observe_receipt(&certificate.receipt, now) {
                        frame.idle_progress += 1;
                    }
                }
                Some(_) => {}
            }
        }
        // Loss detection while a race (or its results) runs. Under term 0 a
        // lobby is idle and the standby covers it; under the successor every
        // state is watched, idle ones through the certified idle commits.
        let watched = self.successor || self.client.next_input().is_some();
        let silent = self
            .last_progress
            .is_some_and(|p| now.saturating_sub(p) >= FREEZE_AFTER_MS);
        let stale = watched && silent;
        if !stale && !self.successor {
            self.bound_lead(now);
        }
        if stale {
            let at = *self.frozen_at.get_or_insert(now);
            frame.frozen = true;
            if self.successor && now.saturating_sub(at) >= SESSION_LOST_AFTER_MS {
                let error = Error::Invalid("session lost: the successor stopped");
                self.endpoint.pause(error.clone());
                return Err(error);
            }
        } else {
            let resend = self.successor && !self.resent;
            let mut resent = 0;
            let batch = self.client.migration_step(
                self.endpoint.intents_mut()?,
                now,
                input,
                &[],
                rtt,
                |client| {
                    if resend {
                        resent = client.migration_resend_inputs();
                    }
                    command(client);
                    Ok(())
                },
            )?;
            self.resent |= resend;
            frame.resent = resent;
            surfaced.extend(self.client.migration_take_view_events());
            views.extend(batch.lossy);
            self.last_frame = Some(now);
            self.last_host = self.client.host_now(now);
            self.frozen_at = None;
        }
        for view in views {
            self.endpoint.queue_view(view)?;
            frame.views_sent += 1;
        }
        let fresh = self
            .endpoint
            .journals()
            .1
            .pending()
            .filter(|(number, _)| *number > self.sent)
            .map(|(number, bytes)| (number, bytes.to_vec()))
            .collect::<Vec<_>>();
        let mut queued = 0;
        loop {
            let more = self.endpoint.queue_pending(MAX_BATCH)?;
            if more == 0 {
                break;
            }
            queued += more;
        }
        frame.intents = fresh.into_iter().take(queued).collect();
        if let Some((number, _)) = frame.intents.last() {
            self.sent = *number;
        }
        let applied = self.endpoint.journals().0.applied();
        if applied > self.acked {
            self.endpoint.queue_applied()?;
            self.acked = applied;
            frame.acked = Some(applied);
        }
        while let Some(wire) = self.endpoint.poll_send(wall)? {
            self.network.send_reliable(wire);
        }
        Ok(frame)
    }
}

/// What one recovering client step did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryFrame {
    /// Replayed reliable outputs applied for the first time, at C.
    pub applied: usize,
    /// Replayed outputs this client had already applied: no game effect.
    pub duplicates: usize,
    /// Input datagrams produced while frozen (none: no frame runs). They
    /// would be dropped, never sent.
    pub views_discarded: usize,
    /// The clock correction confirmed at the final cut, in this step.
    pub clock_delta: Option<i64>,
    /// The signed `Resume` arrived in this step.
    pub resumed: bool,
}

/// A surviving remote client frozen at C during takeover. It keeps its
/// complete `ClientSession` and answers the promoted standby over the
/// prepared link: the resume proof bound to that connection, replayed
/// outputs (applied once, through the frozen step), input rounds, the final
/// cut with its clock correction, then `Resume`.
pub struct RecoveringPlayer {
    recovery: PlayerRecovery,
    client: ClientSession,
    network: Box<dyn ClientTransport>,
    ingress: ReceiveQueue,
    policy: EndpointPolicy,
    logical_ms: u64,
    frozen_local: u64,
    /// The host-time estimate the last stepped frame recorded
    /// ([`RemotePlayer::last_frame_host`]).
    frozen_host: Option<u64>,
    clock_delta: Option<i64>,
    round_open_at_cut: Option<bool>,
    resumed: bool,
    resumed_local: Option<u64>,
}
/// A cut's bounded clock correction: the host-time estimate recorded at the
/// last stepped frame minus C. Without a recorded estimate, the client's
/// estimate at `frozen_local` through its clock as it stands.
fn cut_delta(
    client: &ClientSession,
    frozen_host: Option<u64>,
    frozen_local: u64,
    logical_ms: u64,
) -> i64 {
    match frozen_host {
        Some(host) => (host as i64).saturating_sub(logical_ms as i64),
        None => client.migration_clock_delta(frozen_local, logical_ms),
    }
}
impl RecoveringPlayer {
    pub fn client(&self) -> &ClientSession {
        &self.client
    }
    pub fn recovery(&self) -> &PlayerRecovery {
        &self.recovery
    }
    /// The frozen authority time C.
    pub fn logical_ms(&self) -> u64 {
        self.logical_ms
    }
    /// The confirmed clock correction, once the final cut arrived.
    pub fn clock_delta(&self) -> Option<i64> {
        self.clock_delta
    }
    /// Whether a clock-sync round was open when the final cut arrived (the
    /// rebase aborts it).
    pub fn round_open_at_cut(&self) -> Option<bool> {
        self.round_open_at_cut
    }
    /// Local time of this client's last frame before the freeze.
    pub fn frozen_local(&self) -> u64 {
        self.frozen_local
    }
    /// This client's host-time estimate at its last stepped frame, as that
    /// frame recorded it.
    pub fn frozen_host(&self) -> Option<u64> {
        self.frozen_host
    }
    /// The bounded correction this client's cut confirms at C: its host-time
    /// estimate at its last stepped frame, as recorded then, minus C.
    pub fn cut_delta(&self) -> i64 {
        cut_delta(&self.client, self.frozen_host, self.frozen_local, self.logical_ms)
    }
    pub fn resumed(&self) -> bool {
        self.resumed
    }
    /// One step at real local time `local_ms` (`wall` drives Fray's recovery
    /// clock and pacing). Replayed outputs go through the client's frozen
    /// `migration_recovery_step`: no frame, no input, no clock sample. At the
    /// final cut the client confirms its bounded correction and rebases its
    /// clock to C; a correction over 250 ms pauses it (Fray refuses it before
    /// the client is touched). At `Resume` it re-anchors host time C to the
    /// local time of receipt: the barrier may have waited for other survivors
    /// since this client's cut, and the successor continues from C, not from
    /// C plus that wait.
    pub fn step(&mut self, local_ms: u64, wall: u64) -> Result<RecoveryFrame> {
        // No frame runs while recovering: the app read the last frame's
        // events, so none stay on the view to be read again.
        self.client.migration_take_view_events();
        let mut frame = RecoveryFrame::default();
        for _ in 0..MAX_BATCH {
            match self.network.poll() {
                None => break,
                Some(ClientNetEvent::Reliable(bytes)) => self.ingress.enqueue(bytes)?,
                Some(ClientNetEvent::Failed(_)) | Some(ClientNetEvent::Disconnected { .. }) => {
                    return Err(Error::Closed)
                }
                Some(_) => {}
            }
        }
        let logical_ms = self.logical_ms;
        while !self.resumed {
            let (recovery, client) = (&mut self.recovery, &mut self.client);
            let mut discarded = 0;
            let received = self.ingress.poll(|wire| {
                recovery.receive(wire, wall, |delivery, bytes, intents| {
                    if delivery != Delivery::Reliable {
                        return Err(Error::Unauthorized);
                    }
                    let event = ClientNetEvent::Reliable(bytes.to_vec());
                    let batch = client.migration_recovery_step(
                        intents,
                        local_ms,
                        logical_ms,
                        &[event],
                        None,
                    )?;
                    discarded += batch.lossy.len();
                    Ok(())
                })
            })?;
            frame.views_discarded += discarded;
            match received {
                None => break,
                Some(None) => {}
                Some(Some(RecoveryPlayerEvent::Output(PlayerEvent::Output {
                    applied, ..
                }))) => {
                    if applied {
                        frame.applied += 1;
                    } else {
                        frame.duplicates += 1;
                    }
                }
                Some(Some(RecoveryPlayerEvent::Output(_))) => {}
                Some(Some(RecoveryPlayerEvent::ClockCut { logical_ms })) => {
                    let delta = cut_delta(client, self.frozen_host, self.frozen_local, logical_ms);
                    let round_open = client.clock_sync().round_open();
                    recovery.confirm_clock(delta, wall, |c, _| {
                        client
                            .migration_rebase_clock(local_ms, c, delta)
                            .map_err(Error::Invalid)
                    })?;
                    self.clock_delta = Some(delta);
                    self.round_open_at_cut = Some(round_open);
                    frame.clock_delta = Some(delta);
                }
                Some(Some(RecoveryPlayerEvent::Resumed)) => {
                    client
                        .migration_rebase_clock(local_ms, logical_ms, 0)
                        .map_err(Error::Invalid)?;
                    self.resumed = true;
                    self.resumed_local = Some(local_ms);
                    frame.resumed = true;
                }
            }
        }
        while let Some(wire) = self.recovery.poll_send(wall)? {
            self.network.send_reliable(wire);
        }
        Ok(frame)
    }
    /// After `Resume`: the same endpoint (warm framing, journals, sender IDs)
    /// and client, on the link to the successor, which now commits alone.
    /// The player is unprotected (no standby, no second takeover). Loss
    /// detection restarts from the local time `Resume` arrived; the first
    /// frame resends the unconfirmed input window.
    pub fn into_resumed(self) -> Result<RemotePlayer> {
        let resumed_local = self.resumed_local.ok_or(Error::Invalid("not resumed"))?;
        let endpoint = self.recovery.into_endpoint()?;
        let (applied, tail) = (
            endpoint.journals().0.applied(),
            endpoint.journals().1.tail(),
        );
        Ok(RemotePlayer {
            network: self.network,
            endpoint,
            client: self.client,
            ingress: self.ingress,
            policy: self.policy,
            resuming: None,
            held: None,
            acked: applied,
            sent: tail,
            warm: None,
            last_frame: None,
            last_host: None,
            last_progress: Some(resumed_local),
            newest: None,
            newest_logical: Some(self.logical_ms),
            frozen_at: None,
            lead_resync: false,
            successor: true,
            resent: false,
        })
    }
}

/// What [`read_route`] did to a route.
#[derive(Default)]
struct RouteRead {
    /// Detach the route; `Some(true)` also bans the key.
    dropped: Option<bool>,
    /// The replacement connection proved the player's journals.
    resumed: bool,
}

/// Feed one route's parked control and queued frames into its queue, in
/// order, until the queue is full, the ingress rate limit holds the rest or
/// nothing is left. Used for every polled route and, on `Left`, for the
/// closing connection's frames before the route is parked.
fn read_route(
    route: &mut Route,
    stats: &mut PlayerIngress,
    primary: &decented_net::migration::Primary,
    now_ms: u64,
) -> RouteRead {
    let mut read = RouteRead::default();
    // A parked control goes first; while the queue is still full, later
    // frames wait in this route's bounded receive queue.
    if let Some(control) = route.parked.take() {
        match route.queue.receive(&control, primary) {
            Ok(_) => stats.accepted_controls += 1,
            Err(Error::Bounds) => {
                route.parked = Some(control);
                return read;
            }
            Err(error) => {
                stats.refused_frames += 1;
                if matches!(error, Error::Conflict | Error::Closed) {
                    read.dropped = Some(error == Error::Conflict);
                    return read;
                }
            }
        }
    }
    for _ in 0..MAX_BATCH {
        let channel = &mut route.channel;
        let frame = match route.ingress.poll(|wire| channel.receive(wire, now_ms)) {
            Ok(None) => {
                if route.ingress.queued_bytes() > 0 {
                    stats.rate_limited += 1;
                }
                break;
            }
            Ok(Some(frame)) => frame,
            Err(_) => {
                stats.refused_frames += 1;
                continue;
            }
        };
        let Some((kind, _, bytes)) = frame else {
            continue;
        };
        if kind != PacketKind::Control || route.resume == Resume::Waiting {
            stats.refused_frames += 1;
            continue;
        }
        if route.resume == Resume::Challenged {
            match route.queue.finish_reconnect(&bytes, primary, now_ms) {
                Ok(()) => {
                    route.resume = Resume::Attached;
                    stats.resumed += 1;
                    read.resumed = true;
                    continue;
                }
                Err(error) => {
                    stats.refused_frames += 1;
                    read.dropped = Some(error == Error::Conflict);
                    break;
                }
            }
        }
        match route.queue.receive(&bytes, primary) {
            Ok(_) => stats.accepted_controls += 1,
            // Capacity: Fray consumed neither the original ID nor the
            // control cursor, but the channel consumed the frame. Park the
            // control and stop reading this route.
            Err(Error::Bounds) => {
                stats.parked_controls += 1;
                route.parked = Some(bytes);
                break;
            }
            Err(error) => {
                stats.refused_frames += 1;
                if matches!(error, Error::Conflict | Error::Closed) {
                    read.dropped = Some(error == Error::Conflict);
                    break;
                }
            }
        }
    }
    read
}
