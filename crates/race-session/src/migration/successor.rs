//! The promoted standby's side of remote-survivor recovery (takeover steps
//! 2 to 6). Every roster client keeps a prepared `ControlPlayer` link to the
//! standby's passive listener while the room is armed. On promotion the
//! standby announces the signed `Promote` on each such link (its last
//! old-term frame) and runs Fray's `Recovery` over them at frozen time C:
//! a resume challenge bound to that connection's TLS exporter, the client's
//! retained-journal proof, committed replay, sealed input rounds, absent
//! departures after the drain, then the final `RecoveryBarrier` cut with each
//! survivor's bounded clock correction, and `Resume`.
//!
//! The same room code is served again by [`start_resume_host`]. That listener
//! stays closed: an ordinary join and a stranger are refused, and a roster
//! key only authenticates. Survivors recover over their prepared links, not
//! over a fresh join. After `Resume` a survivor whose link dropped may
//! reconnect there on the `Player` role through Fray's reconnect challenge.
use super::standby::OwnSeat;
use super::*;
use crate::transport::{HostNetEvent, HostTransport};
use decented_net::migration::{
    CertifiedOutput, DeliveryJournal, IntentJournal, PlayerInbox, PlayerQueue, PrimaryEvent,
    SessionImage,
};
use decented_net::migration::{
    Channel, Delivery, Descriptor, LinkRole, PacketKind, PlayerControl, Promotion, ReceiveQueue,
    Recovery, RecoveryBarrier, RecoveryCut, RecoveryPhase, RecoveryReady, Resume, ResumeChallenge,
    ResumeProof, Root, SuccessorActor, MAX_BATCH,
};

/// Serve the room's existing code as the certified successor: Fray's
/// `resume_host` with the pinned descriptor and the signed promotion. Normal
/// admission stays closed mid-match.
pub fn start_resume_host(
    cfg: &decented_net::NetConfig,
    identity: &decented_net::Identity,
    room: &str,
    code: &str,
    descriptor: Descriptor,
    promotion: Promotion,
) -> Result<crate::net::QuicHost> {
    if identity.pubkey().to_bytes() != descriptor.root.standby {
        return Err(Error::Unauthorized);
    }
    let handle = decented_net::resume_host(
        cfg,
        identity,
        decented_net::RoomInfo {
            name: room.into(),
            max_players: decented_net::MAX_PLAYERS,
        },
        code,
        descriptor,
        promotion,
    )
    .map_err(|_| Error::Invalid("resume host"))?;
    Ok(crate::net::QuicHost::new(handle))
}

/// One survivor's prepared link, now in term 1.
struct Route {
    peer: PeerKey,
    channel: Channel,
    ingress: ReceiveQueue,
    challenged: bool,
    accepted: bool,
    round: bool,
    /// Highest replayed reliable output sequence framed to this link.
    framed: u64,
    cut: Option<RecoveryCut>,
    /// The link's control inbox, made at its final cut, for the one-use
    /// warm attachment after `Resume`.
    inbox: Option<PlayerInbox>,
}

/// The promoted standby's roster links during recovery. Each link is bound to
/// one roster player by the key its connection authenticated; a resume proof
/// is accepted only for the challenge issued on the connection it arrived on.
pub struct SurvivorLinks {
    root: Root,
    promotion: Promotion,
    network: crate::net::QuicHost,
    original: PeerKey,
    routes: BTreeMap<u64, Route>,
    challenges: BTreeMap<u64, ResumeChallenge>,
    proofs: BTreeMap<u64, Vec<u8>>,
    own_retained: bool,
    own_sealed: bool,
    /// Whether the standby's own player is a member at C, seated or
    /// seatless (cached on the first [`Self::drive`]).
    own_seated: Option<bool>,
    /// Proofs, controls and links refused without harming the room.
    pub refused: usize,
    /// Frames and events from peers with no survivor link.
    pub unrouted: usize,
}
impl SurvivorLinks {
    /// Take the promoted standby's link apart and open every carried prepared
    /// roster link, announcing the signed `Promote` on each.
    pub fn open(link: super::StandbyRecoveryLink) -> Result<(Self, Recovery)> {
        let (recovery, network, original, events) = link.into_survivor_parts();
        let mut links = Self {
            root: recovery.root().clone(),
            promotion: recovery.promotion().clone(),
            network,
            original,
            routes: BTreeMap::new(),
            challenges: BTreeMap::new(),
            proofs: BTreeMap::new(),
            own_retained: false,
            own_sealed: false,
            own_seated: None,
            refused: 0,
            unrouted: 0,
        };
        for event in events {
            links.route(event);
        }
        Ok((links, recovery))
    }
    /// Players with a prepared link.
    pub fn players(&self) -> impl Iterator<Item = u64> + '_ {
        self.routes.keys().copied()
    }
    /// Whether this player's resume proof was accepted on its own link.
    pub fn accepted(&self, player: u64) -> bool {
        self.routes.get(&player).is_some_and(|r| r.accepted)
    }
    /// The challenge issued on this player's link.
    pub fn challenge_of(&self, player: u64) -> Option<&ResumeChallenge> {
        self.challenges.get(&player)
    }
    /// The exact proof bytes accepted for this player.
    pub fn proof_of(&self, player: u64) -> Option<&[u8]> {
        self.proofs.get(&player).map(Vec::as_slice)
    }
    /// This player's link: (connection id, TLS exporter, authenticated key).
    pub fn link_of(&self, player: u64) -> Option<(u64, [u8; 32], [u8; 32])> {
        self.routes
            .get(&player)
            .map(|r| (u64::from(r.peer), r.channel.tls(), r.channel.remote_key()))
    }
    /// One listener event. No single link can fail recovery: a frame the
    /// link's ordered ingress refuses (empty, oversized, or over its queue
    /// bounds) closes that link without a ban, as [`super::PlayerHost`]
    /// does. A sealed survivor whose link closes then fails safely at the
    /// recovery deadline; anyone else is simply no longer routed.
    fn route(&mut self, event: HostNetEvent) {
        match event {
            HostNetEvent::Joined { peer, .. } => self.join(peer),
            HostNetEvent::Left { peer } => self.routes.retain(|_, r| r.peer != peer),
            HostNetEvent::Reliable { peer, bytes } => {
                let routed = self.routes.iter_mut().find(|(_, r)| r.peer == peer);
                match routed {
                    Some((&player, route)) => {
                        if route.ingress.enqueue(bytes).is_err() {
                            self.close_route(player);
                        }
                    }
                    None => self.unrouted += 1,
                }
            }
            HostNetEvent::Lossy { .. } => self.unrouted += 1,
        }
    }
    /// Close a routed link without a ban and count it refused.
    fn close_route(&mut self, player: u64) {
        if let Some(route) = self.routes.remove(&player) {
            self.refused += 1;
            self.network.disconnect(route.peer);
        }
    }
    /// A prepared roster link: an admitted `ControlPlayer` connection whose
    /// authenticated key is a roster member other than the pair. A second
    /// link for a routed player never evicts the first.
    fn join(&mut self, peer: PeerKey) {
        if peer == self.original {
            self.unrouted += 1;
            return;
        }
        let refuse = |links: &mut Self| {
            links.unrouted += 1;
            links.network.disconnect(peer);
        };
        let Ok(mut channel) = self.network.migration_channel(peer) else {
            return refuse(self);
        };
        let key = channel.remote_key();
        let player = self
            .root
            .members
            .iter()
            .find(|m| m.key == Some(key))
            .map(|m| m.player);
        let Some(player) = player else {
            return refuse(self);
        };
        if channel.binding().client != LinkRole::ControlPlayer
            || channel.binding().term != 0
            || key == self.root.host
            || key == self.root.standby
            || self.routes.contains_key(&player)
        {
            return refuse(self);
        }
        if channel.promote_and_announce(&self.promotion).is_err() {
            return refuse(self);
        }
        self.routes.insert(
            player,
            Route {
                peer,
                channel,
                ingress: ReceiveQueue::default(),
                challenged: false,
                accepted: false,
                round: false,
                framed: 0,
                cut: None,
                inbox: None,
            },
        );
    }
    fn intake(&mut self) {
        for _ in 0..MAX_BATCH {
            let Some(event) = self.network.poll() else {
                break;
            };
            self.route(event);
        }
    }
    /// Send every link's queued frames.
    pub fn flush(&mut self, now_ms: u64) -> Result<()> {
        for route in self.routes.values_mut() {
            for _ in 0..MAX_BATCH {
                let Some(wire) = route.channel.poll_send(now_ms)? else {
                    break;
                };
                self.network.send_reliable(route.peer, wire);
            }
        }
        Ok(())
    }
    /// Reassembled control records, per player, in arrival order. A link
    /// that sends an invalid frame is closed (no ban) and counted.
    fn controls(&mut self, now_ms: u64) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut closed = Vec::new();
        for (&player, route) in &mut self.routes {
            for _ in 0..MAX_BATCH {
                let channel = &mut route.channel;
                match route.ingress.poll(|wire| channel.receive(wire, now_ms)) {
                    Ok(None) => break,
                    Ok(Some(None)) => {}
                    Ok(Some(Some((PacketKind::Control, _, bytes)))) => out.push((player, bytes)),
                    Ok(Some(Some(_))) | Err(_) => {
                        closed.push(player);
                        break;
                    }
                }
            }
        }
        for player in closed {
            self.close_route(player);
        }
        out
    }
    /// Count a refusal; a refusal that failed recovery itself propagates.
    fn refuse(&mut self, recovery: &Recovery, error: Error) -> Result<()> {
        if recovery.phase() == RecoveryPhase::Failed {
            return Err(error);
        }
        self.refused += 1;
        Ok(())
    }
    /// Offer a resume proof that arrived on `arrived_on`'s link. It is checked
    /// against the challenge issued on that connection and its TLS exporter.
    pub fn offer_proof(
        &mut self,
        recovery: &mut Recovery,
        arrived_on: u64,
        bytes: &[u8],
        keys: &decented_net::Keys,
        now_ms: u64,
    ) -> Result<bool> {
        let Some(route) = self.routes.get(&arrived_on) else {
            self.refused += 1;
            return Ok(false);
        };
        let (connection, tls) = (u64::from(route.peer), route.channel.tls());
        let membership = recovery.sequencer().membership_certificate(keys)?;
        let accepted = ResumeProof::decode_with_certificate(&self.root, &membership, bytes)
            .and_then(|proof| {
                recovery.accept(connection, tls, &proof, now_ms)?;
                Ok(proof.player)
            });
        match accepted {
            Ok(player) if player == arrived_on => {
                self.routes.get_mut(&player).ok_or(Error::Closed)?.accepted = true;
                self.proofs.insert(player, bytes.to_vec());
                Ok(true)
            }
            Ok(_) => Err(Error::Unauthorized),
            Err(error) => {
                self.refuse(recovery, error)?;
                Ok(false)
            }
        }
    }
    fn deliver_controls(
        &mut self,
        recovery: &mut Recovery,
        keys: &decented_net::Keys,
        now_ms: u64,
    ) -> Result<()> {
        for (player, bytes) in self.controls(now_ms) {
            if recovery.phase() == RecoveryPhase::Collecting {
                self.offer_proof(recovery, player, &bytes, keys, now_ms)?;
                continue;
            }
            let Some(route) = self.routes.get(&player) else {
                continue;
            };
            let tls = route.channel.tls();
            let result = PlayerControl::decode(&self.root, &bytes).and_then(|record| {
                if record.player != player {
                    return Err(Error::Unauthorized);
                }
                recovery.accept_player_control(&record, tls, now_ms)
            });
            if let Err(error) = result {
                self.refuse(recovery, error)?;
            }
        }
        Ok(())
    }
    /// Frame each survivor's committed reliable replay above what was framed.
    fn replay(&mut self, recovery: &Recovery) -> Result<()> {
        let context = self.root.context()?;
        for record in recovery.replay()? {
            if record.output.delivery != Delivery::Reliable {
                continue;
            }
            let Some(route) = self.routes.get_mut(&record.output.recipient) else {
                continue;
            };
            if !route.accepted || record.output.sequence <= route.framed {
                continue;
            }
            route
                .channel
                .enqueue(PacketKind::Output, &record.encode(&context)?)?;
            route.framed = record.output.sequence;
        }
        Ok(())
    }
    fn begin_round(&mut self, recovery: &mut Recovery) -> Result<bool> {
        match recovery.begin_round() {
            Ok(()) => {
                for route in self.routes.values_mut() {
                    route.round = false;
                }
                self.own_sealed = false;
                Ok(true)
            }
            Err(Error::Gap) => Ok(false),
            Err(error) => Err(error),
        }
    }
    /// One step of frozen recovery at C, in the spec's order: collect proofs
    /// on prepared links and the own seat's cut (sealing the cohort after all
    /// answer or 250 ms), commit the security boundary, replay and ACK
    /// committed outputs, seal each survivor's input round and drain it, and
    /// only then depart absent players. Returns the phase reached.
    ///
    /// `own` is the standby device's own seat. While the standby's player is
    /// still a member of the closed frontier's image, seated or seatless, the
    /// seat is local to this device (it kept its client and journals, even
    /// after its own `Leave`), so it is retained, replayed and sealed like any
    /// survivor: `drive` without its `OwnSeat` is refused before recovery is
    /// touched (Fray would fail the seal for its missing local state). A seat
    /// C kept seatless (its departure batch addressed it an output) is driven
    /// too; the successor's first live batch then retires it and drops the
    /// seat, and the device hosts seatless. A seatless takeover (`None`) is
    /// allowed only when the seat was retired before C (no member there); an
    /// `OwnSeat` passed for it is not driven.
    pub fn drive(
        &mut self,
        recovery: &mut Recovery,
        own: Option<&mut OwnSeat>,
        keys: &decented_net::Keys,
        local_ms: u64,
        now_ms: u64,
    ) -> Result<RecoveryPhase> {
        let seated = match self.own_seated {
            Some(seated) => seated,
            None => {
                let session = SessionImage::decode(&self.root, recovery.sequencer().image())?;
                let member = session
                    .members
                    .iter()
                    .any(|m| m.key == Some(self.root.standby));
                self.own_seated = Some(member);
                member
            }
        };
        if seated && own.is_none() {
            return Err(Error::Invalid(
                "own seat is a member at C: pass its OwnSeat",
            ));
        }
        let mut own = own.filter(|_| seated);
        self.intake();
        recovery.poll(now_ms)?;
        match recovery.phase() {
            RecoveryPhase::Collecting => {
                // A seatless member left explicitly (its departure batch
                // addressed it an output): it is never challenged, so it is
                // absent and the departure commit retires it.
                let seatless = seatless_players(&self.root, recovery.sequencer().image())?;
                let refused = self
                    .routes
                    .keys()
                    .copied()
                    .filter(|player| seatless.contains(player))
                    .collect::<Vec<_>>();
                for player in refused {
                    self.close_route(player);
                }
                for (&player, route) in &mut self.routes {
                    if route.challenged {
                        continue;
                    }
                    route.challenged = true;
                    match recovery.challenge(
                        u64::from(route.peer),
                        route.channel.tls(),
                        player,
                        route.channel.remote_key(),
                        keys,
                        now_ms,
                    ) {
                        Ok(challenge) => {
                            route
                                .channel
                                .enqueue(PacketKind::Control, &challenge.encode(&self.root)?)?;
                            self.challenges.insert(player, challenge);
                        }
                        Err(_) => self.refused += 1,
                    }
                }
                if !self.own_retained {
                    if let Some(own) = own.as_deref() {
                        own.retain(recovery, now_ms)?;
                    }
                    self.own_retained = true;
                }
                self.deliver_controls(recovery, keys, now_ms)?;
                recovery.seal(now_ms)?;
            }
            RecoveryPhase::Security => {
                recover_security(recovery, keys, now_ms)?;
            }
            RecoveryPhase::Replay | RecoveryPhase::Draining => {
                self.deliver_controls(recovery, keys, now_ms)?;
                let records = recovery.replay()?;
                if let Some(own) = own.as_deref_mut() {
                    own.replay(recovery, keys, local_ms, &records)?;
                }
                self.replay(recovery)?;
                recovery.commit_delivery_acks(keys)?;
                let next = if recovery.phase() == RecoveryPhase::Draining {
                    match recovery.next_input() {
                        Ok(next) => next.is_some(),
                        Err(Error::Gap) => return self.finish_step(recovery, now_ms),
                        Err(error) => return Err(error),
                    }
                } else {
                    false
                };
                if next {
                    recover_next(recovery, keys, now_ms)?;
                } else {
                    self.begin_round(recovery)?;
                }
            }
            RecoveryPhase::AwaitCuts => {
                for (&player, route) in &mut self.routes {
                    if !route.accepted || route.round {
                        continue;
                    }
                    let request =
                        recovery.issue_input_round(player, route.channel.tls(), keys, now_ms)?;
                    route.channel.enqueue(
                        PacketKind::Control,
                        &request.encode(&self.root, &self.promotion)?,
                    )?;
                    route.round = true;
                }
                if !self.own_sealed {
                    if let Some(own) = own.as_deref_mut() {
                        own.seal(recovery)?;
                    }
                    self.own_sealed = true;
                }
                self.deliver_controls(recovery, keys, now_ms)?;
            }
            RecoveryPhase::Departures => {
                recover_departures(recovery, keys, now_ms)?;
            }
            RecoveryPhase::Ready | RecoveryPhase::Failed => {}
        }
        self.finish_step(recovery, now_ms)
    }
    fn finish_step(&mut self, recovery: &Recovery, now_ms: u64) -> Result<RecoveryPhase> {
        self.flush(now_ms)?;
        Ok(recovery.phase())
    }
    /// Enter the final barrier (causal recovery is `Ready`) and send each
    /// accepted survivor its signed final cut on its own link.
    pub fn barrier(
        &mut self,
        recovery: Recovery,
        keys: &decented_net::Keys,
        now_ms: u64,
    ) -> Result<RecoveryBarrier> {
        let mut barrier = RecoveryBarrier::new(recovery, keys, now_ms)?;
        for (&player, route) in &mut self.routes {
            if !route.accepted {
                continue;
            }
            let cut = barrier.issue(player, route.channel.tls(), keys, now_ms)?;
            route.channel.enqueue(
                PacketKind::Control,
                &cut.encode(&self.root, &self.promotion)?,
            )?;
            route.inbox = Some(PlayerInbox::new(&route.channel, cut.membership.clone())?);
            route.cut = Some(cut);
        }
        self.flush(now_ms)?;
        Ok(barrier)
    }
    /// Accept each survivor's `RecoveryReady` (its final journals and clock
    /// correction) on its own link. True once every member is confirmed.
    pub fn drive_barrier(&mut self, barrier: &mut RecoveryBarrier, now_ms: u64) -> Result<bool> {
        self.intake();
        for (player, bytes) in self.controls(now_ms) {
            let Some(route) = self.routes.get(&player) else {
                continue;
            };
            let Some(cut) = route.cut.as_ref() else {
                self.refused += 1;
                continue;
            };
            let tls = route.channel.tls();
            let result = RecoveryReady::decode(&self.root, &self.promotion, cut, &bytes)
                .and_then(|ready| barrier.accept(&ready, tls, now_ms));
            if result.is_err() {
                self.refused += 1;
            }
        }
        self.flush(now_ms)?;
        Ok(barrier.is_ready())
    }
    /// Commit nothing new: sign `Resume` over the confirmed cuts and send it
    /// on every survivor link. [`Self::into_successor`] then runs the match.
    pub fn resume(
        &mut self,
        barrier: RecoveryBarrier,
        keys: &decented_net::Keys,
        now_ms: u64,
    ) -> Result<(SuccessorActor, Resume)> {
        let (successor, _, resume) = barrier.finish(keys, now_ms)?;
        let bytes = resume.encode(&self.root, &self.promotion)?;
        for route in self.routes.values_mut() {
            if route.accepted {
                route.channel.enqueue(PacketKind::Control, &bytes)?;
            }
        }
        self.flush(now_ms)?;
        Ok((successor, resume))
    }
}

/// One survivor's recovered link under the successor.
struct SuccessorRoute {
    peer: Option<PeerKey>,
    /// The peer is on the resume host (a reconnect after `Resume`), not on
    /// the standby listener that carried the prepared link.
    resumed_link: bool,
    /// A reconnect challenge went out; the next control must be the proof.
    challenged: bool,
    channel: Channel,
    ingress: ReceiveQueue,
    queue: PlayerQueue,
    parked: Option<Vec<u8>>,
    /// The committed game time at which the link dropped, until the
    /// survivor proves its journals again ([`LiveSuccessor::stage`] departs
    /// it after [`super::PARKED_DEPARTURE_MS`]).
    gone_since: Option<u64>,
}

/// The promoted device's own seat, now local to the authority.
struct OwnLocal {
    player: u64,
    client: crate::client::ClientSession,
    delivery: DeliveryJournal,
    intents: IntentJournal,
    views: Vec<(u64, Vec<u8>)>,
    next_view: u64,
    inbox: std::collections::VecDeque<CertifiedOutput>,
    /// The unconfirmed input window was resent (the first frame after
    /// `Resume`, as every remote survivor does).
    resent: bool,
}

/// What one own-seat frame did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnFrame {
    pub applied: usize,
    pub views: usize,
    pub intents: u64,
    /// Input packets resent on the seat's first frame after `Resume`.
    pub resent: usize,
}

/// What one successor commit published.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuccessorCommit {
    pub index: u64,
    /// Certified outputs framed to survivors' recovered links.
    pub framed: usize,
    /// Certified outputs queued for the own seat's client.
    pub own: usize,
    /// Every certified output of this commit.
    pub outputs: usize,
    /// Survivors with no output in this commit that got the successor's
    /// signed membership certificate at the new receipt (idle liveness).
    pub certified: usize,
}

/// Successor ingress refused without harming the room.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SuccessorIngress {
    /// New connections refused on the standby listener or the resume host:
    /// the room is closed mid-match. Only a survivor whose link dropped may
    /// reconnect (see [`LiveSuccessor::poll`]).
    pub refused_joins: usize,
    /// Survivors that reconnected after `Resume` with a fresh proof of
    /// their retained journals.
    pub reconnected: usize,
    pub refused_frames: usize,
    pub accepted_controls: usize,
    pub parked_controls: usize,
    pub dropped_routes: usize,
}

/// The single unprotected authority after `Resume`, the Stampede counterpart
/// of [`super::LivePrimary`] with no standby: Fray's `SuccessorActor` is its
/// own sequencer. Every batch is staged privately, committed (fresh native
/// restore validation, then the signed install) and only then adopted; its
/// certified outputs then go to the survivors' recovered links and to the
/// own seat's client. There is no standby and no second takeover: if this
/// device stops, clients freeze and the session ends.
pub struct LiveSuccessor {
    root: Root,
    actor: SuccessorActor,
    keys: decented_net::Keys,
    network: crate::net::QuicHost,
    resume_host: Option<crate::net::QuicHost>,
    authority: HostAuthority,
    routes: BTreeMap<u64, SuccessorRoute>,
    own: Option<OwnLocal>,
    pending: Option<QueuedBatch>,
    failure: Option<Error>,
    ingress: SuccessorIngress,
}
impl SurvivorLinks {
    /// After [`Self::resume`]: attach each survivor's recovered link to the
    /// successor (Fray's one-use warm attachment, bound to the TLS exporter
    /// of its accepted final cut) and own the match. `own` is the promoted
    /// device's seat after its `resumed`; `resume_host` serves the same code.
    pub fn into_successor(
        self,
        mut actor: SuccessorActor,
        own: Option<OwnSeat>,
        resume_host: Option<crate::net::QuicHost>,
        keys: decented_net::Keys,
    ) -> Result<LiveSuccessor> {
        if keys.public_key().to_bytes() != self.root.standby {
            return Err(Error::Unauthorized);
        }
        let authority = committed_authority(&self.root, actor.image())?;
        let mut routes = BTreeMap::new();
        let mut network = self.network;
        for (player, mut route) in self.routes {
            if !route.accepted || route.cut.is_none() {
                network.disconnect(route.peer);
                continue;
            }
            let queue = actor.attach_warm_player(&route.channel, &mut route.inbox, &keys)?;
            routes.insert(
                player,
                SuccessorRoute {
                    peer: Some(route.peer),
                    resumed_link: false,
                    challenged: false,
                    channel: route.channel,
                    ingress: route.ingress,
                    queue,
                    parked: None,
                    gone_since: None,
                },
            );
        }
        let own = match own {
            Some(seat) => {
                let (player, client, delivery, intents) = seat.into_parts()?;
                Some(OwnLocal {
                    player,
                    client,
                    delivery,
                    intents,
                    views: Vec::new(),
                    next_view: 0,
                    inbox: Default::default(),
                    resent: false,
                })
            }
            None => None,
        };
        Ok(LiveSuccessor {
            root: self.root,
            actor,
            keys,
            network,
            resume_host,
            authority,
            routes,
            own,
            pending: None,
            failure: None,
            ingress: SuccessorIngress::default(),
        })
    }
}
impl LiveSuccessor {
    /// Always false: after the one takeover there is no standby.
    pub fn protected(&self) -> bool {
        false
    }
    /// The last adopted (committed) authority.
    pub fn authority(&self) -> &HostAuthority {
        &self.authority
    }
    pub fn primary(&self) -> &decented_net::migration::Primary {
        self.actor.primary()
    }
    pub fn receipt(&self) -> &decented_net::migration::Receipt {
        self.actor.receipt()
    }
    pub fn image(&self) -> &[u8] {
        self.actor.image()
    }
    pub fn failure(&self) -> Option<&Error> {
        self.failure.as_ref().or(self.actor.failure())
    }
    pub fn ingress(&self) -> SuccessorIngress {
        self.ingress
    }
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn players(&self) -> impl Iterator<Item = u64> + '_ {
        self.routes.keys().copied()
    }
    pub fn queue(&self, player: u64) -> Option<&PlayerQueue> {
        self.routes.get(&player).map(|r| &r.queue)
    }
    pub fn own_client(&self) -> Option<&crate::client::ClientSession> {
        self.own.as_ref().map(|o| &o.client)
    }
    pub fn own_journals(&self) -> Option<(&DeliveryJournal, &IntentJournal)> {
        self.own.as_ref().map(|o| (&o.delivery, &o.intents))
    }
    fn latch<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            self.failure.get_or_insert(error.clone());
        }
        result
    }
    fn guard(&self) -> Result<()> {
        if self.failure().is_some() {
            return Err(Error::Closed);
        }
        Ok(())
    }
    fn refuse_join(&mut self, peer: PeerKey, resume_host: bool) {
        self.ingress.refused_joins += 1;
        match (resume_host, self.resume_host.as_mut()) {
            (true, Some(host)) => host.disconnect(peer),
            _ => self.network.disconnect(peer),
        }
    }
    fn drop_route(&mut self, player: u64) {
        let at = self.actor.receipt().logical_ms;
        if let Some(route) = self.routes.get_mut(&player) {
            route.parked = None;
            route.challenged = false;
            if let Some(peer) = route.peer.take() {
                route.gone_since.get_or_insert(at);
                self.ingress.dropped_routes += 1;
                match (route.resumed_link, self.resume_host.as_mut()) {
                    (true, Some(host)) => host.disconnect(peer),
                    _ => self.network.disconnect(peer),
                }
            }
        }
    }
    /// Close a survivor's link without a ban (for example after the app saw
    /// its transport fault). Its queue and journals stay; the survivor may
    /// reconnect on the resume host.
    pub fn disconnect(&mut self, player: u64) {
        self.drop_route(player);
    }
    /// A new connection on the resume host. Only a roster survivor whose
    /// link dropped, on the session-bound `Player` role, is challenged
    /// (Fray's reconnect: a fresh TLS-bound challenge signed by this
    /// successor, answered by a proof of the retained journals). A second
    /// connection never evicts a live one; anyone else is refused (no ban).
    fn reconnect_peer(&mut self, peer: PeerKey, now_ms: u64) {
        let Some(host) = self.resume_host.as_ref() else {
            return self.refuse_join(peer, true);
        };
        let Ok(mut channel) = host.migration_channel(peer) else {
            return self.refuse_join(peer, true);
        };
        if channel.binding().client != LinkRole::Player {
            return self.refuse_join(peer, true);
        }
        let key = channel.remote_key();
        let player = SessionImage::decode(&self.root, self.actor.image())
            .ok()
            .and_then(|image| image.members.into_iter().find(|m| m.key == Some(key)))
            .map(|m| m.player);
        // A seatless member left explicitly: its reconnect is refused.
        let seatless = player.is_some_and(|p| {
            seatless_players(&self.root, self.actor.image()).map_or(true, |s| s.contains(&p))
        });
        if seatless {
            return self.refuse_join(peer, true);
        }
        let Some(route) = player.and_then(|p| self.routes.get_mut(&p)) else {
            return self.refuse_join(peer, true);
        };
        if route.peer.is_some() {
            return self.refuse_join(peer, true);
        }
        let sent = self
            .actor
            .membership_certificate(&self.keys)
            .and_then(|membership| {
                let request = route.queue.begin_reconnect(
                    &channel,
                    membership,
                    self.actor.primary(),
                    now_ms,
                    &self.keys,
                )?;
                channel.enqueue(PacketKind::Control, &request.encode(&self.root)?)
            });
        if sent.is_err() {
            return self.refuse_join(peer, true);
        }
        route.peer = Some(peer);
        route.resumed_link = true;
        route.challenged = true;
        route.channel = channel;
        route.ingress = ReceiveQueue::default();
        route.parked = None;
    }
    /// After a reconnect: frame every retained reliable output the survivor
    /// has not ACKed. Lossy outputs missed meanwhile are not replayed.
    fn replay(&mut self, player: u64) -> Result<()> {
        let context = self.root.context()?;
        let route = self.routes.get_mut(&player).ok_or(Error::Closed)?;
        let ack = route.queue.output_ack();
        for output in self.actor.outbox() {
            if output.output.recipient == player && output.output.sequence > ack {
                route
                    .channel
                    .enqueue(PacketKind::Output, &output.encode(&context)?)?;
            }
        }
        Ok(())
    }
    /// Read both listeners and feed verified controls into the survivors'
    /// queues. The room is closed mid-match: a fresh connection on the
    /// standby listener is refused (no ban), and on the resume host only a
    /// survivor whose link dropped is challenged to reconnect. A refused
    /// frame drops only that link.
    pub fn poll(&mut self, now_ms: u64) -> Result<()> {
        self.guard()?;
        for _ in 0..MAX_BATCH {
            let Some(event) = self.network.poll() else {
                break;
            };
            match event {
                HostNetEvent::Joined { peer, .. } => self.refuse_join(peer, false),
                HostNetEvent::Left { peer } => self.left(peer, false),
                HostNetEvent::Reliable { peer, bytes } => {
                    let Some((&player, route)) = self
                        .routes
                        .iter_mut()
                        .find(|(_, r)| r.peer == Some(peer) && !r.resumed_link)
                    else {
                        self.ingress.refused_frames += 1;
                        continue;
                    };
                    if route.ingress.enqueue(bytes).is_err() {
                        self.ingress.refused_frames += 1;
                        self.drop_route(player);
                    }
                }
                HostNetEvent::Lossy { .. } => self.ingress.refused_frames += 1,
            }
        }
        for _ in 0..MAX_BATCH {
            let Some(event) = self.resume_host.as_mut().and_then(|h| h.poll()) else {
                break;
            };
            match event {
                HostNetEvent::Joined { peer, .. } => self.reconnect_peer(peer, now_ms),
                HostNetEvent::Left { peer } => self.left(peer, true),
                HostNetEvent::Reliable { peer, bytes } => {
                    let Some((&player, route)) = self
                        .routes
                        .iter_mut()
                        .find(|(_, r)| r.peer == Some(peer) && r.resumed_link)
                    else {
                        self.ingress.refused_frames += 1;
                        continue;
                    };
                    if route.ingress.enqueue(bytes).is_err() {
                        self.ingress.refused_frames += 1;
                        self.drop_route(player);
                    }
                }
                HostNetEvent::Lossy { .. } => self.ingress.refused_frames += 1,
            }
        }
        let mut dropped = Vec::new();
        let mut reconnected = Vec::new();
        for (&player, route) in &mut self.routes {
            if route.peer.is_none() {
                continue;
            }
            if let Some(control) = route.parked.take() {
                match route.queue.receive(&control, self.actor.primary()) {
                    Ok(_) => self.ingress.accepted_controls += 1,
                    Err(Error::Bounds) => {
                        route.parked = Some(control);
                        continue;
                    }
                    Err(_) => {
                        self.ingress.refused_frames += 1;
                        dropped.push(player);
                        continue;
                    }
                }
            }
            for _ in 0..MAX_BATCH {
                let channel = &mut route.channel;
                let frame = match route.ingress.poll(|wire| channel.receive(wire, now_ms)) {
                    Ok(None) => break,
                    Ok(Some(frame)) => frame,
                    Err(_) => {
                        self.ingress.refused_frames += 1;
                        continue;
                    }
                };
                let Some((kind, _, bytes)) = frame else {
                    continue;
                };
                if kind != PacketKind::Control {
                    self.ingress.refused_frames += 1;
                    continue;
                }
                if route.challenged {
                    match route
                        .queue
                        .finish_reconnect(&bytes, self.actor.primary(), now_ms)
                    {
                        Ok(()) => {
                            route.challenged = false;
                            route.gone_since = None;
                            reconnected.push(player);
                            continue;
                        }
                        Err(_) => {
                            self.ingress.refused_frames += 1;
                            dropped.push(player);
                            break;
                        }
                    }
                }
                match route.queue.receive(&bytes, self.actor.primary()) {
                    Ok(_) => self.ingress.accepted_controls += 1,
                    Err(Error::Bounds) => {
                        self.ingress.parked_controls += 1;
                        route.parked = Some(bytes);
                        break;
                    }
                    Err(error) => {
                        self.ingress.refused_frames += 1;
                        if matches!(error, Error::Conflict | Error::Closed) {
                            dropped.push(player);
                            break;
                        }
                    }
                }
            }
        }
        for player in dropped {
            self.drop_route(player);
        }
        for player in reconnected {
            self.ingress.reconnected += 1;
            if self.replay(player).is_err() {
                self.drop_route(player);
            }
        }
        self.flush(now_ms);
        Ok(())
    }
    fn left(&mut self, peer: PeerKey, resume_host: bool) {
        let at = self.actor.receipt().logical_ms;
        for route in self.routes.values_mut() {
            if route.peer == Some(peer) && route.resumed_link == resume_host {
                route.gone_since.get_or_insert(at);
                route.peer = None;
                route.parked = None;
                route.challenged = false;
            }
        }
    }
    /// One own-seat frame at local time `local_ms`: apply its certified
    /// outputs once (delivery journal, then the client's transaction), run
    /// the frame with `input`, and keep its journaled intents and input views
    /// for the next batch. The view's events are this frame's: those of every
    /// step it ran, in order. A no-op without an own seat.
    pub fn own_frame(&mut self, local_ms: u64, input: race_core::Input) -> Result<OwnFrame> {
        self.own_frame_with(local_ms, input, |_| {})
    }
    /// As [`Self::own_frame`], with a local UI command (for example
    /// `set_rematch_ready`) run inside the frame's own client transaction,
    /// so its reliable intent is journaled before the next batch takes it.
    pub fn own_frame_with(
        &mut self,
        local_ms: u64,
        input: race_core::Input,
        command: impl FnOnce(&mut crate::client::ClientSession),
    ) -> Result<OwnFrame> {
        // The app read the previous frame's events; each step replaces the
        // view's, so a frame of several steps collects them per step.
        if let Some(own) = self.own.as_mut() {
            own.client.migration_take_view_events();
        }
        self.guard()?;
        let context = self.root.context()?;
        let Some(own) = self.own.as_mut() else {
            return Ok(OwnFrame::default());
        };
        // Left on the view whatever the frame returns: a step which committed
        // is not replayed, so its events would never come back.
        let mut surfaced = Vec::new();
        let result = Self::own_steps(own, &context, local_ms, input, command, &mut surfaced);
        own.client.migration_set_view_events(surfaced);
        result
    }
    /// The body of [`Self::own_frame_with`]; the events of every step which
    /// committed are pushed to `surfaced`, also when the frame then fails.
    fn own_steps(
        own: &mut OwnLocal,
        context: &decented_net::migration::Context,
        local_ms: u64,
        input: race_core::Input,
        command: impl FnOnce(&mut crate::client::ClientSession),
        surfaced: &mut Vec<race_core::Event>,
    ) -> Result<OwnFrame> {
        let mut command = Some(command);
        let mut frame = OwnFrame::default();
        let tail = own.intents.tail();
        loop {
            let mut delivery = DeliveryJournal::restore(&own.delivery.checkpoint()?)?;
            let mut intents = IntentJournal::restore(&own.intents.checkpoint()?)?;
            let mut incoming = Vec::new();
            // Records are only read here; they leave the inbox once the
            // client's step has committed them, so a failed step loses none.
            for record in own.inbox.iter().take(MAX_BATCH) {
                if record.output.delivery == Delivery::Reliable {
                    // Once per sequence: duplicates skip, gaps are refused.
                    delivery.apply_certified(context, own.player, record, |bytes| {
                        incoming.push(crate::transport::ClientNetEvent::Reliable(bytes.to_vec()));
                        Ok(())
                    })?;
                } else {
                    // Lossy views are not journaled; each is certified too.
                    record.verify(context)?;
                    if record.output.recipient != own.player {
                        return Err(Error::Unauthorized);
                    }
                    incoming.push(crate::transport::ClientNetEvent::Lossy(
                        record.output.bytes.clone(),
                    ));
                }
            }
            let taken = own.inbox.len().min(MAX_BATCH);
            // N4 for the own seat: its client predicted past C before the
            // freeze, and those inputs went to the dead original. Resend the
            // unconfirmed window once, inside the step, like a remote
            // survivor's first frame after `Resume`.
            let resend = !own.resent;
            let mut resent = 0;
            let batch = own.client.migration_step(
                &mut intents,
                local_ms,
                input,
                &incoming,
                None,
                |client| {
                    if resend {
                        resent = client.migration_resend_inputs();
                    }
                    if let Some(command) = command.take() {
                        command(client);
                    }
                    Ok(())
                },
            )?;
            surfaced.extend(own.client.migration_take_view_events());
            own.inbox.drain(..taken);
            own.delivery = delivery;
            own.intents = intents;
            own.resent = true;
            frame.resent += resent;
            frame.applied += incoming.len();
            for view in batch.lossy {
                own.next_view += 1;
                own.views.push((own.next_view, view));
                frame.views += 1;
            }
            // Bounded like Fray's player queue: the oldest view goes first.
            let excess = own
                .views
                .len()
                .saturating_sub(decented_net::migration::MAX_LOSSY_VIEWS);
            own.views.drain(..excess);
            if own.inbox.is_empty() {
                break;
            }
        }
        frame.intents = own.intents.tail() - tail;
        Ok(frame)
    }
    /// The survivors whose link has been gone for longer than
    /// [`super::PARKED_DEPARTURE_MS`] at game time `now` and whose native
    /// seat the kick will remove, with its native id. As on the original
    /// ([`super::LivePrimary::stage`]): a seat the kick would not remove
    /// stays, and is retried on the next stage.
    fn due_departures(&self, now: u64) -> Result<Vec<(u64, u8)>> {
        let due: Vec<u64> = self
            .routes
            .iter()
            .filter(|(_, r)| {
                r.gone_since
                    .is_some_and(|at| now.saturating_sub(at) > PARKED_DEPARTURE_MS)
            })
            .map(|(player, _)| *player)
            .collect();
        if due.is_empty() || self.authority.migration_frozen() {
            return Ok(Vec::new());
        }
        let image = SessionImage::decode(&self.root, self.actor.image())?;
        let saved: BatchImage = options()
            .deserialize(&image.game)
            .map_err(|_| Error::Invalid("race batch encoding"))?;
        Ok(saved
            .seats
            .iter()
            .filter(|row| due.contains(&row.player))
            .filter(|row| self.authority.humans.iter().any(|h| h.id == row.native_id))
            .map(|row| (row.player, row.native_id))
            .collect())
    }
    /// Stage one batch over the survivors' queues and the own seat, and
    /// prepare it on the successor. False while one is already staged.
    /// Nothing is visible or sent until [`Self::commit`].
    ///
    /// The original's parked-departure rule holds here too: a survivor whose
    /// link dropped (it left, or was closed) and that has not proved its
    /// journals again within [`super::PARKED_DEPARTURE_MS`] is departed in
    /// this batch by the native kick, before `operation` runs, and its queue
    /// is not fed. The kick keeps it a seatless member for this batch; the
    /// next batch retires it and its outbox. The time is the batch's game
    /// time, counted from the committed game time of the drop: after
    /// `Resume` the successor's game clock is the device's own local clock
    /// (there is no pause), and it is what the native silence rule counts.
    pub fn stage(
        &mut self,
        now: u64,
        mut operation: impl FnMut(&mut HostAuthority) -> Result<()>,
    ) -> Result<bool> {
        self.guard()?;
        if self.pending.is_some() {
            return Ok(false);
        }
        let result = (|| {
            // At most 50 ms of game time per batch: a slow frame's elapsed
            // time is staged over back-to-back batches (the driver stages
            // again after each commit); over 250 ms behind pauses as skew.
            let now = next_batch_time(self.actor.primary().receipt().logical_ms, now)?;
            let departing = self.due_departures(now)?;
            let rtts = self
                .routes
                .iter()
                .map(|(player, r)| {
                    let logical = SessionImage::decode(&self.root, self.actor.image())
                        .ok()
                        .and_then(|i| i.members.into_iter().find(|m| m.player == *player))
                        .map(|m| m.logical_peer)
                        .ok_or(Error::Gap)?;
                    Ok((logical, r.peer.and_then(|p| self.network.rtt_ms(p))))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let queues = self
                .routes
                .iter()
                .filter(|(player, _)| !departing.iter().any(|(p, _)| p == *player))
                .map(|(_, r)| &r.queue)
                .collect::<Vec<_>>();
            let own = self.own.as_ref().map(|o| OwnInputs {
                player: o.player,
                originals: o.intents.pending().map(|(n, b)| (n, b.to_vec())).collect(),
                views: o.views.clone(),
                output_ack: o.delivery.applied(),
            });
            let staged = try_stage_queued_batch(
                &self.authority,
                self.actor.primary(),
                &self.root,
                now,
                &queues,
                own.as_ref(),
                &rtts,
                || Ok(()),
                |host| {
                    for (_, native_id) in &departing {
                        host.kick(*native_id);
                    }
                    operation(host)
                },
            )?;
            staged.prepare_actor(&mut self.actor)?;
            Ok(staged)
        })();
        let staged = self.latch(result)?;
        self.pending = Some(staged);
        Ok(true)
    }
    /// Commit the staged batch: Fray's fresh restore validation and signed
    /// install, then adoption of exactly that authority, then publication of
    /// its certified outputs. No output is framed or applied before the
    /// install, and none whose receipt is not installed.
    pub fn commit(&mut self, now_ms: u64) -> Result<SuccessorCommit> {
        self.guard()?;
        let result = (|| {
            let staged = self.pending.take().ok_or(Error::Stale)?;
            let root = self.root.clone();
            let event = self.actor.commit(&self.keys, |candidate, _, previous| {
                validate_candidate(&root, previous, candidate)
            })?;
            let PrimaryEvent::Committed { outputs, .. } = event else {
                return Err(Error::Conflict);
            };
            let selected = staged.selected_views().to_vec();
            let (authority, certified, _controls) =
                staged.adopt_actor(&mut self.actor, &outputs)?;
            self.authority = authority;
            self.publish(&certified, &selected, now_ms)
        })();
        self.latch(result)
    }
    fn publish(
        &mut self,
        outputs: &[CertifiedOutput],
        selected: &[(u64, u64)],
        now_ms: u64,
    ) -> Result<SuccessorCommit> {
        let context = self.root.context()?;
        let receipt = self.actor.receipt().clone();
        for output in outputs {
            output.verify(&context)?;
            if output.receipt != receipt {
                return Err(Error::Stale);
            }
        }
        let image = SessionImage::decode(&self.root, self.actor.image())?;
        let mut retired = Vec::new();
        for (&player, route) in &mut self.routes {
            match route.queue.refresh(self.actor.primary()) {
                Ok(()) => {}
                Err(Error::Closed) => retired.push(player),
                Err(error) => return Err(error),
            }
            for (owner, number) in selected {
                if *owner == player {
                    route.queue.discard_lossy(*number);
                }
            }
        }
        for player in retired {
            self.drop_route(player);
            self.routes.remove(&player);
        }
        if let Some(own) = self.own.as_mut() {
            let newest = selected
                .iter()
                .filter(|(p, _)| *p == own.player)
                .map(|(_, n)| *n)
                .max();
            if let Some(newest) = newest {
                own.views.retain(|(n, _)| *n > newest);
            }
            match image.inputs.get(&own.player) {
                Some(history) if history.handled() > own.intents.acknowledged() => {
                    own.intents.acknowledge(history.handled())?
                }
                Some(_) => {}
                // The seat left (its own Leave), or C kept it seatless and
                // this batch retired it: the device keeps hosting, seatless.
                None => self.own = None,
            }
        }
        let mut commit = SuccessorCommit {
            index: receipt.index,
            outputs: outputs.len(),
            ..Default::default()
        };
        let mut failed = Vec::new();
        let mut reached = std::collections::BTreeSet::new();
        for output in outputs {
            let player = output.output.recipient;
            if let Some(own) = self.own.as_mut().filter(|o| o.player == player) {
                own.inbox.push_back(output.clone());
                commit.own += 1;
                continue;
            }
            let Some(route) = self.routes.get_mut(&player) else {
                continue;
            };
            // A challenged reconnect gets the outbox replayed once it proves
            // its journals.
            if route.peer.is_none() || route.challenged || failed.contains(&player) {
                continue;
            }
            match route
                .channel
                .enqueue(PacketKind::Output, &output.encode(&context)?)
            {
                Ok(_) => {
                    commit.framed += 1;
                    reached.insert(player);
                }
                Err(_) => failed.push(player),
            }
        }
        // Idle liveness: a survivor this commit sent no output (an idle
        // lobby or results screen) gets the successor's signed membership
        // certificate at the installed receipt instead. It is fresh committed
        // progress: a client counts it only for a receipt newer than any it
        // saw, so a replay cannot refresh liveness, and it exists only per
        // commit, so it cannot mask stalled commits during play.
        let idle = self
            .routes
            .iter()
            .filter(|(p, r)| {
                r.peer.is_some() && !r.challenged && !reached.contains(*p) && !failed.contains(*p)
            })
            .map(|(p, _)| *p)
            .collect::<Vec<_>>();
        if !idle.is_empty() {
            let certificate = self
                .actor
                .membership_certificate(&self.keys)?
                .encode(&self.root)?;
            for player in idle {
                if let Some(route) = self.routes.get_mut(&player) {
                    match route.channel.enqueue(PacketKind::Control, &certificate) {
                        Ok(_) => commit.certified += 1,
                        Err(_) => failed.push(player),
                    }
                }
            }
        }
        for player in failed {
            self.drop_route(player);
        }
        self.flush(now_ms);
        Ok(commit)
    }
    /// Release paced frames on every recovered link.
    pub fn flush(&mut self, now_ms: u64) {
        let mut failed = Vec::new();
        for (&player, route) in &mut self.routes {
            let Some(peer) = route.peer else {
                continue;
            };
            loop {
                match route.channel.poll_send(now_ms) {
                    Ok(Some(wire)) => match (route.resumed_link, self.resume_host.as_mut()) {
                        (true, Some(host)) => host.send_reliable(peer, wire),
                        _ => self.network.send_reliable(peer, wire),
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
            self.drop_route(player);
        }
    }
}
