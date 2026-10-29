//! Owned standby replication after lobby activation, its promotion trigger and
//! the warm link it hands to recovery, the Stampede counterpart of Twang's
//! `LiveStandby`.
//!
//! Frame classification after the standby has signed the promotion uses the
//! same [`ObsoleteIngress`] filter as the original:
//! - A decoded frame for this pair session in term 0 is obsolete. Candidate and
//!   delta frames are refused after close: they are never restored, installed
//!   or acknowledged. Receipts, liveness and capability records from term 0
//!   have no effect. Both are counted, never applied.
//! - Every other frame (term 1, another session, or bytes which do not decode)
//!   stays queued in order for the recovery link, whose `Channel` still applies
//!   its normal session/term/role checks. Nothing here can reopen term 0.
//!
//! The standby device's own human seat signs its player controls with the
//! standby runtime key, so it shares the pair link's sender sequence
//! ([`LiveStandby::sender_sequence`]); a separate counter would collide with
//! the pair link's replay sequence at the original. After promotion, that seat
//! recovers as a local survivor through [`OwnSeat`], which drains its in-flight
//! reliable intents exactly once.
use super::live::{own_approval, unix_now};
use super::*;
use crate::transport::{HostNetEvent, HostTransport};
use decented_net::migration::{
    Activity, Armed, CertifiedOutput, Channel, Delivery, DeliveryJournal, Descriptor,
    IntentJournal, MembershipCertificate, Receipt, ReceiveQueue, Recovery, RecoveryBarrier,
    SenderSequence, SessionId, SessionImage, StandbyActor, StandbyFault, MAX_BATCH,
    MAX_OUTBOX_RECORDS,
};

/// Fray rejects a second pulse probe within this many milliseconds of the last
/// one as `Stale` (`decented-net` `migration.rs`, `ProgressWatch`/`Watch`
/// `probe_with_sequence`: `now_ms - last < 50`). The value is private there, so
/// it is mirrored here; the wrappers coalesce instead of latching on it.
const PROBE_MIN_GAP_MS: u64 = 50;

/// The standby's own side of the pair link: its own My Signet approval on
/// actual UTC and its own listener key. Failures here are local faults and
/// latch the standby.
fn own_link(
    network: &crate::net::QuicHost,
    descriptor: &Descriptor,
    approved_until: u64,
) -> Result<()> {
    if approved_until <= unix_now() {
        return Err(Error::Unauthorized);
    }
    if network
        .handle()
        .ok_or(Error::Closed)?
        .runtime_key()
        .to_bytes()
        != descriptor.root.standby
    {
        return Err(Error::Unauthorized);
    }
    Ok(())
}
/// The original's side: its actual-UTC admission on the authenticated link.
fn original_admitted(
    network: &crate::net::QuicHost,
    descriptor: &Descriptor,
    peer: PeerKey,
) -> bool {
    network
        .migration_peer_key(peer)
        .is_ok_and(|key| key == descriptor.root.host)
}

/// Classify one pair-listener event, the same way before and after
/// promotion. The original's reliable frames are returned for the caller's
/// ingress. Its lossy frames are rejected traffic and are not kept.
///
/// The pair link and the game are kept apart. The original's own `Left` is a
/// pair-link event: it is recorded only as [`OriginalLink::Left`] and never
/// enters the game event queue, so a game consumer cannot mistake the
/// pair-link peer for a departing game peer. Every other event (early roster
/// events and other peers' traffic) is kept in arrival order in the bounded
/// `pending` queue.
fn route(
    event: HostNetEvent,
    original_peer: PeerKey,
    original: &mut OriginalLink,
    pending: &mut BootstrapEvents,
    network: &mut crate::net::QuicHost,
) -> Result<Option<Vec<u8>>> {
    match event {
        HostNetEvent::Reliable { peer, bytes } if peer == original_peer => return Ok(Some(bytes)),
        HostNetEvent::Lossy { peer, .. } if peer == original_peer => {
            if *original == OriginalLink::Live {
                *original = OriginalLink::Rejected;
            }
            return Ok(None);
        }
        HostNetEvent::Left { peer } if peer == original_peer => {
            *original = OriginalLink::Left;
            return Ok(None);
        }
        _ => {}
    }
    // Roster traffic never fails the standby: a peer over its own budget is
    // closed (no ban) and only its held events are dropped.
    if let Some(peer) = pending.push(event) {
        network.disconnect(peer);
    }
    Ok(None)
}

/// What the standby knows about the original's side of the pair link. Only
/// `Live` replicates. The other states stop replication without latching a
/// standby fault, so a healthy standby can still promote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginalLink {
    Live,
    /// The original's transport left. This is the immediate promotion trigger.
    Left,
    /// The original's actual-UTC admission failed (e.g. its signed approval
    /// expired). No later frame from it is processed; promotion still needs
    /// the normal expired watch.
    Unauthorized,
    /// The original sent traffic the pair protocol rejects (not a restore
    /// fault). Replication stops; promotion still needs the expired watch.
    Rejected,
}

/// Promotion was refused. `standby` is returned whenever the refusal happened
/// before the actor was consumed, so the caller can retry later.
pub struct PromoteRefused {
    pub error: Error,
    pub standby: Option<Box<LiveStandby>>,
}
impl std::fmt::Debug for PromoteRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromoteRefused")
            .field("error", &self.error)
            .field("standby_retained", &self.standby.is_some())
            .finish()
    }
}

/// Owns the standby's sequencer (inside Fray's `StandbyActor`), armed
/// certificate, pair channel, listener, deferred ingress and early roster
/// events. A replicated commit is ACKed only after the complete native race
/// authority has been freshly restored from it and Fray has installed the
/// exact image. The actor itself makes one fresh retry and then fails.
/// Read accessors expose no mutable actor, sequencer or transport handle.
///
/// Fault kinds. Only three named original-caused conditions are non-latching:
/// the original's transport loss (`Left`), its lapsed actual-UTC admission
/// (`Unauthorized`), and a protocol rejection of its frame (`Rejected`: a
/// lossy frame on the pair link, or Fray's [`StandbyFault::Frame`]). They
/// only move [`OriginalLink`] out of `Live`, which stops replication. Every
/// other error latches `failure` and forbids promotion: the standby's own
/// approval and listener, its clock, resource bounds, restore faults, and
/// every [`StandbyFault::Local`] (a failed or poisoned replay session, the
/// watch clock, a full receipt queue, post-install replay commit and watch
/// bookkeeping).
pub struct LiveStandby {
    descriptor: Descriptor,
    actor: StandbyActor,
    armed: Armed,
    network: crate::net::QuicHost,
    peer: PeerKey,
    ingress: ReceiveQueue,
    pending: BootstrapEvents,
    keys: decented_net::Keys,
    approved_until: u64,
    activity: Activity,
    original: OriginalLink,
    restores: u64,
    failure: Option<Error>,
    clock: RunnerClock,
    last_ms: u64,
    last_probe_ms: Option<u64>,
    sender: SenderSequence,
    /// Test hook: the runtime neither reads nor sends on the pair link.
    offline: bool,
}
impl LiveStandby {
    /// `identity` is the standby runtime's own identity, with its My Signet
    /// approval. The installed image is restored once here, so an image the
    /// native race cannot restore fails before any traffic. `clock` is the
    /// device's [`RunnerClock`]: every watch time this runner uses is sampled
    /// from it, never supplied by the caller.
    pub fn new(
        descriptor: Descriptor,
        bootstrap: StandbyBootstrap,
        identity: decented_net::Identity,
        clock: RunnerClock,
    ) -> Result<Self> {
        let now_ms = clock.now_ms();
        descriptor.verify()?;
        let approved_until = own_approval(&identity)?;
        let keys = identity.keys().clone();
        let root = &descriptor.root;
        if keys.public_key().to_bytes() != root.standby {
            return Err(Error::Unauthorized);
        }
        own_link(&bootstrap.network, &descriptor, approved_until)?;
        check_standby_admission(
            &bootstrap.network,
            &descriptor,
            bootstrap.original_peer,
            false,
        )?;
        if bootstrap.armed.receipt != *bootstrap.sequencer.receipt() {
            return Err(Error::Conflict);
        }
        let activity = activity_of(
            bootstrap.sequencer.receipt().index,
            committed_authority(root, bootstrap.sequencer.image())?.phase(),
        );
        let mut pending = BootstrapEvents::default();
        let mut network = bootstrap.network;
        for event in bootstrap.pending {
            if let Some(peer) = pending.push(event) {
                network.disconnect(peer);
            }
        }
        // The pair link's sender: the standby device's own player controls
        // allocate from this same counter (cloning shares the allocation).
        let sender = bootstrap.sequencer.sender_sequence();
        let actor = StandbyActor::new(
            descriptor.clone(),
            bootstrap.sequencer,
            bootstrap.channel,
            now_ms,
        )?;
        Ok(Self {
            descriptor,
            actor,
            armed: bootstrap.armed,
            network,
            peer: bootstrap.original_peer,
            ingress: bootstrap.ingress,
            pending,
            keys,
            approved_until,
            activity,
            original: OriginalLink::Live,
            restores: 0,
            failure: None,
            clock,
            last_ms: now_ms,
            last_probe_ms: None,
            offline: false,
            sender,
        })
    }
    pub fn root(&self) -> &decented_net::migration::Root {
        &self.descriptor.root
    }
    /// Where the standby's passive control listener accepts session-bound
    /// links: its local socket address and LAN token. Roster clients prepare
    /// their `ControlPlayer` paths to it while armed. Only the address is
    /// exposed: the listener itself (its ordered ingress, sends and admission)
    /// stays inside the runtime.
    pub fn listener(&self) -> Option<(std::net::SocketAddr, String)> {
        let handle = self.network.handle()?;
        Some((handle.local_addr()?, handle.lan_token()))
    }
    pub fn armed(&self) -> &Armed {
        &self.armed
    }
    /// The pair link's shared sender sequence. The standby device's own human
    /// seat signs with the standby runtime key, so its player endpoint must
    /// allocate control sequences from this counter
    /// (`PlayerEndpoint::new_with_sender`); Fray refuses the standby key in
    /// `PlayerEndpoint::new`. A separate counter would reuse sequence numbers
    /// the original has already recorded for this key from the pair link,
    /// which its replay session treats as a permanent `Conflict`. The clone
    /// also shares the replay session's fault latch.
    pub fn sender_sequence(&self) -> SenderSequence {
        self.sender.clone()
    }
    /// The standby's signed membership certificate at its installed receipt,
    /// for the own seat's player endpoint and the original's player queue.
    pub fn membership_certificate(&self) -> Result<MembershipCertificate> {
        self.actor.membership_certificate(&self.keys)
    }
    pub fn receipt(&self) -> &Receipt {
        self.actor.receipt()
    }
    pub fn image(&self) -> &[u8] {
        self.actor.image()
    }
    pub fn failure(&self) -> Option<&Error> {
        self.failure.as_ref().or(self.actor.failure())
    }
    /// Native fresh-restore attempts made for replicated candidates.
    /// The original's answers to this standby's probes read after their
    /// 200 ms window, which Fray counts and ignores
    /// (`StandbyActor::late_answers`) instead of refusing them as `Stale`:
    /// with [`Self::poll`] probing every 50 ms, an own stall on either side
    /// leaves some.
    pub fn late_answers(&self) -> u64 {
        self.actor.late_answers()
    }
    pub fn restores(&self) -> u64 {
        self.restores
    }
    pub fn original(&self) -> OriginalLink {
        self.original
    }
    /// Physical bytes received from the original and not yet processed.
    pub fn queued_bytes(&self) -> usize {
        self.ingress.queued_bytes()
    }
    pub fn pending_events(&self) -> usize {
        self.pending.events.len()
    }
    /// Raw access to the pair link's transport and the original's peer, for
    /// hostile tests only (like `ClientPlayerRoute::network_mut`). Anything
    /// sent here bypasses the actor.
    #[doc(hidden)]
    pub fn pair_network_mut(&mut self) -> (&mut crate::net::QuicHost, PeerKey) {
        (&mut self.network, self.peer)
    }
    /// Test hook: while set, the runtime neither reads from the pair
    /// listener nor sends on it (as on a device whose network drops): the
    /// original's frames stay queued on the listener and this runtime's own
    /// (its probes) in its send queue, in order, and both drain once it is
    /// cleared.
    #[doc(hidden)]
    pub fn offline_for_test(&mut self, offline: bool) {
        self.offline = offline;
    }
    /// Roster peers whose held-traffic budget overflowed while armed; each
    /// one's link was closed without a ban. The standby is unaffected.
    pub fn overflowed_peers(&self) -> usize {
        self.pending.overflowed
    }
    /// Fresh complete native authority restored from the installed image.
    pub fn committed_authority(&self) -> Result<HostAuthority> {
        committed_authority(self.root(), self.actor.image())
    }
    /// Latching checks: the standby's own fault state, approval and listener.
    fn guard(&self) -> Result<()> {
        if self.failure.is_some() || self.actor.failure().is_some() {
            return Err(Error::Closed);
        }
        own_link(&self.network, &self.descriptor, self.approved_until)
    }
    /// Non-latching: true only while the original is still `Live` and its
    /// actual-UTC admission holds now. Otherwise it records `Unauthorized`.
    fn original_live(&mut self) -> bool {
        if self.original != OriginalLink::Live {
            return false;
        }
        if !original_admitted(&self.network, &self.descriptor, self.peer) {
            self.original = OriginalLink::Unauthorized;
            return false;
        }
        true
    }
    fn latch<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            if self.failure.is_none() {
                self.failure = Some(error.clone());
            }
        }
        result
    }
    fn advance(&mut self, now_ms: u64) -> Result<()> {
        if now_ms < self.last_ms {
            return Err(Error::Invalid("backwards standby runtime clock"));
        }
        self.last_ms = now_ms;
        Ok(())
    }
    /// A fresh sample of the runner clock for one actor call. It is recorded
    /// before use, so no later call can hand the actor's watch an earlier time.
    fn actor_clock(&mut self) -> Result<u64> {
        let current = self.clock.now_ms();
        self.advance(current)?;
        Ok(current)
    }
    /// Stampede's listener kicks a peer whose approval has expired and reports
    /// that as `Left`, the same event as a real transport loss. Both paths drop
    /// the peer's approval, so admission cannot be re-checked after a `Left`.
    /// Instead the original's admission is checked before every listener
    /// poll, with the UTC second recorded just before that check:
    /// - A lapsed approval found by the check is `Unauthorized`.
    /// - A `Left` read while UTC is still in the recorded second cannot be an
    ///   expiry kick: the check passed in that second, so the approval ends in
    ///   a later second, and the listener kicks only once it has ended. It is
    ///   transport loss, the immediate promotion trigger.
    /// - A `Left` read after UTC has moved to a later second may be an expiry
    ///   kick. It is recorded conservatively as `Unauthorized`, so promotion
    ///   still waits for the watch.
    fn drain_network(&mut self) -> Result<()> {
        if self.offline {
            return Ok(());
        }
        for _ in 0..MAX_BATCH {
            let checked = unix_now();
            if !self.original_live() {
                break;
            }
            let Some(event) = self.network.poll() else {
                break;
            };
            // Transport loss is the promotion trigger, not a fault. Already
            // queued frames stay for promotion's classifier.
            let frame = route(
                event,
                self.peer,
                &mut self.original,
                &mut self.pending,
                &mut self.network,
            )?;
            if self.original == OriginalLink::Left && unix_now() != checked {
                self.original = OriginalLink::Unauthorized;
            }
            if let Some(bytes) = frame {
                self.ingress.enqueue(bytes)?;
            }
        }
        Ok(())
    }
    /// The activity this standby signs into its pulses, derived from the
    /// checkpoint it installed last, by the same rule as the original's:
    /// `Advancing` in any phase whose world steps or whose timer runs (the
    /// countdown, the race and the results), `Idle` in the lobby and at
    /// index 0. Fray's
    /// `Pulse::respond`: the runner derives it from the installed checkpoint
    /// and never accepts a declaration, so there is no setter.
    pub fn activity(&self) -> Activity {
        self.activity
    }
    fn probe_coalesced(&self, now_ms: u64) -> bool {
        self.last_probe_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < PROBE_MIN_GAP_MS)
    }
    /// Probes only a `Live` original; otherwise this is a no-op. A probe within
    /// `PROBE_MIN_GAP_MS` of the last one that reached the actor is a no-op
    /// success, so a per-frame caller cannot latch a healthy standby.
    /// [`Self::poll`] already probes on every call, so a driver that never
    /// calls this cannot cause a false takeover.
    pub fn probe(&mut self) -> Result<()> {
        let result = (|| {
            let now_ms = self.actor_clock()?;
            self.guard()?;
            if self.original_live() && !self.probe_coalesced(now_ms) {
                self.actor.probe(now_ms)?;
                self.last_probe_ms = Some(now_ms);
            }
            Ok(())
        })();
        self.latch(result)
    }
    /// Whether the 200 ms watch has expired at a fresh runner-clock sample.
    pub fn expired(&mut self) -> Result<bool> {
        let result = (|| {
            let now_ms = self.actor_clock()?;
            self.guard()?;
            self.actor.expired(now_ms)
        })();
        self.latch(result)
    }
    /// Move physical frames from the listener into the ordered ingress queue
    /// without running the actor, restore or sending anything.
    pub fn intake(&mut self) -> Result<()> {
        let result = (|| {
            self.actor_clock()?;
            self.guard()?;
            self.drain_network()
        })();
        self.latch(result)
    }
    /// Returns the receipts newly installed in this call, in order. Fray's
    /// reply to an identical retry of the batch already installed (the
    /// original resends a waiting batch) answers the original with that
    /// receipt again, but it is not an install and is not returned, so a
    /// consumer never processes one receipt twice. The standby's own
    /// approval is checked before and after every native restore attempt; the
    /// original's admission before every frame and every send. A receipt is
    /// released only after its complete image is installed.
    ///
    /// The original's admission is not re-checked inside the restore callback:
    /// a failure there would be indistinguishable from a restore fault to the
    /// actor and would permanently fail it. A candidate admitted at intake may
    /// therefore install if the original's approval lapses during its restore;
    /// its receipt is not sent, and the original's own runtime refuses
    /// adoption after its approval lapses.
    ///
    /// After the frames, it probes a `Live` original itself (paced to one per
    /// `PROBE_MIN_GAP_MS`), before the send loop that carries the probe: the
    /// original's answers keep this standby's watch fresh while nothing
    /// commits, and Fray defers its verdict after this runner's own stall
    /// only if a probe went out within 200 ms before the gap. So a driver
    /// that never calls [`Self::probe`] cannot cause a false takeover.
    pub fn poll(&mut self) -> Result<Vec<Receipt>> {
        let result = (|| {
            self.actor_clock()?;
            self.guard()?;
            if !self.original_live() {
                return Ok(vec![]);
            }
            self.drain_network()?;
            let mut installed = Vec::new();
            for _ in 0..MAX_BATCH {
                self.guard()?;
                if !self.original_live() {
                    return Ok(installed);
                }
                let current = self.actor_clock()?;
                // Fray answers an identical retry of the installed batch with
                // that receipt again; only a higher index is a new install.
                let installed_index = self.actor.receipt().index;
                let Self {
                    descriptor,
                    actor,
                    network,
                    ingress,
                    keys,
                    approved_until,
                    activity,
                    restores,
                    ..
                } = self;
                let root = &descriptor.root;
                // The phase of the candidate restored for validation; it
                // becomes the pulse activity only once that candidate installs.
                let mut phase = None;
                let received = ingress.poll(|wire| {
                    let result = actor.receive_classified(
                        wire,
                        current,
                        keys,
                        *activity,
                        // Fray passes (candidate, new image, previous image).
                        |candidate, _, previous| {
                            own_link(network, descriptor, *approved_until)?;
                            *restores += 1;
                            let (validated, restored) =
                                validate_candidate_phase(root, previous, candidate, false)?;
                            own_link(network, descriptor, *approved_until)?;
                            phase = Some((candidate.index, restored));
                            Ok(validated)
                        },
                    );
                    match result {
                        // Not consumed: the frame stays queued for a later poll.
                        Err(StandbyFault::Frame(Error::RateLimited)) => Err(Error::RateLimited),
                        result => Ok(result),
                    }
                })?;
                let receipt = match received {
                    None => break,
                    Some(Ok(receipt)) => receipt,
                    // The only non-latching receive error: Fray's named
                    // protocol rejection of the original's frame, provided
                    // the standby itself is still healthy.
                    Some(Err(StandbyFault::Frame(_))) if self.guard().is_ok() => {
                        self.original = OriginalLink::Rejected;
                        return Ok(installed);
                    }
                    // Every standby-side fault latches.
                    Some(Err(fault)) => return Err(fault.into_error()),
                };
                self.guard()?;
                if let (Some(receipt), Some((index, restored))) = (&receipt, phase) {
                    if receipt.index == index {
                        self.activity = activity_of(index, restored);
                    }
                }
                // A re-answer to a retried batch is not an install: it is
                // never reported, so no consumer processes a receipt twice.
                installed.extend(receipt.filter(|r| r.index > installed_index));
            }
            let current = self.actor_clock()?;
            if self.original_live() && !self.probe_coalesced(current) {
                self.actor.probe(current)?;
                self.last_probe_ms = Some(current);
            }
            for _ in 0..MAX_BATCH {
                self.guard()?;
                if !self.original_live() || self.offline {
                    break;
                }
                let current = self.actor_clock()?;
                let Some(wire) = self.actor.poll_send(current)? else {
                    break;
                };
                self.network.send_reliable(self.peer, wire);
            }
            self.actor_clock()?;
            Ok(installed)
        })();
        self.latch(result)
    }
    /// Close term 0 at the last installed commit and sign `Promote`. Requires
    /// an expired watch or a `Left` original transport, plus the standby's own
    /// current admission. A latched standby cannot promote. Refusals before
    /// the actor is consumed return the standby unchanged. Frames already
    /// queued are classified here; nothing more is drained from the network
    /// (the promoted poll classifies later frames identically).
    ///
    /// The watch time is one fresh runner-clock sample, taken here; the
    /// promotion uses it throughout and reports it as
    /// [`PromotedStandby::promoted_at_ms`].
    pub fn promote(mut self) -> std::result::Result<PromotedStandby, PromoteRefused> {
        let now_ms = self.clock.now_ms();
        let checked = (|| {
            if self.failure.is_some() || self.actor.failure().is_some() {
                return Err(Error::Closed);
            }
            if now_ms < self.last_ms {
                return Err(Error::Invalid("backwards standby runtime clock"));
            }
            self.guard()?;
            let left = self.original == OriginalLink::Left;
            if !left && !self.actor.expired(now_ms)? {
                return Err(Error::Stale);
            }
            Ok(left)
        })();
        let left = match checked {
            Ok(left) => left,
            Err(error) => {
                return Err(PromoteRefused {
                    error,
                    standby: Some(Box::new(self)),
                })
            }
        };
        self.last_ms = now_ms;
        let refused = |error| PromoteRefused {
            error,
            standby: None,
        };
        let (recovery, channel) = self
            .actor
            .begin_recovery(&self.keys, now_ms, left)
            .map_err(refused)?;
        own_link(&self.network, &self.descriptor, self.approved_until).map_err(refused)?;
        let session = channel.binding().session;
        let mut promoted = PromotedStandby {
            recovery,
            descriptor: self.descriptor,
            channel,
            session,
            network: self.network,
            peer: self.peer,
            ingress: self.ingress,
            pending: self.pending,
            approved_until: self.approved_until,
            obsolete: ObsoleteIngress::default(),
            original: self.original,
            clock: self.clock,
            promoted_at_ms: now_ms,
            last_ms: now_ms,
        };
        promoted
            .obsolete
            .retain(promoted.session, &mut promoted.ingress)
            .map_err(refused)?;
        Ok(promoted)
    }
}

/// The promoted standby: Fray's owned `Recovery` at the closed frontier plus
/// the warm original link now in term 1. Its poll delivers the signed
/// `Promote` to a still-admitted original and filters term-0 traffic; remaining
/// term-1 frames are retained for the original's retained-client recovery.
/// Recovery state is advanced with the `recover_*` adapters against `recovery`.
pub struct PromotedStandby {
    pub recovery: Recovery,
    descriptor: Descriptor,
    channel: Channel,
    session: SessionId,
    network: crate::net::QuicHost,
    peer: PeerKey,
    ingress: ReceiveQueue,
    pending: BootstrapEvents,
    approved_until: u64,
    obsolete: ObsoleteIngress,
    original: OriginalLink,
    clock: RunnerClock,
    promoted_at_ms: u64,
    last_ms: u64,
}
/// The promoted standby's warm link for the retained-client path. `ingress`
/// holds only frames which passed the obsolete filter; use `intake` for more.
/// The original's departure is visible only through `original`
/// ([`OriginalLink::Left`]), never as a game event. Every other listener event
/// (early roster events from before promotion and later ones) stays in
/// arrival order in a bounded queue.
pub struct StandbyRecoveryLink {
    pub recovery: Recovery,
    pub network: crate::net::QuicHost,
    pub peer: PeerKey,
    pub channel: Channel,
    pub ingress: ReceiveQueue,
    pub obsolete: ObsoleteIngress,
    pub original: OriginalLink,
    pending: BootstrapEvents,
}
impl StandbyRecoveryLink {
    /// Move later frames from the original into `ingress` through the filter,
    /// record its departure in `original`, and move every other event into
    /// the ordered event queue.
    pub fn intake(&mut self) -> Result<()> {
        let session = self.channel.binding().session;
        for _ in 0..MAX_BATCH {
            let Some(event) = self.network.poll() else {
                break;
            };
            if let Some(bytes) = route(
                event,
                self.peer,
                &mut self.original,
                &mut self.pending,
                &mut self.network,
            )? {
                if self.obsolete.keep(session, &bytes) {
                    self.ingress.enqueue(bytes)?;
                }
            }
        }
        Ok(())
    }
    /// Listener events carried across promotion, oldest first.
    pub fn events(&self) -> impl Iterator<Item = &HostNetEvent> {
        self.pending.events.iter()
    }
    /// Hand the carried events to their consumer, oldest first.
    pub fn take_events(&mut self) -> Vec<HostNetEvent> {
        std::mem::take(&mut self.pending).events.into()
    }
    /// For remote-survivor recovery: the owned `Recovery`, the standby's
    /// listener (which carries the roster's prepared control links), the
    /// original's peer, and the carried listener events, oldest first. The
    /// original's own warm link is dropped here: it has no survivor role.
    pub fn into_survivor_parts(
        mut self,
    ) -> (Recovery, crate::net::QuicHost, PeerKey, Vec<HostNetEvent>) {
        let events = self.take_events();
        (self.recovery, self.network, self.peer, events)
    }
}
impl PromotedStandby {
    /// The runner-clock time the promotion used: its watch sample and the
    /// time Fray's recovery began at.
    pub fn promoted_at_ms(&self) -> u64 {
        self.promoted_at_ms
    }
    /// The device's runner clock, for downstream recovery times on the same
    /// timebase as [`Self::promoted_at_ms`].
    pub fn clock(&self) -> RunnerClock {
        self.clock
    }
    pub fn obsolete(&self) -> ObsoleteIngress {
        self.obsolete
    }
    /// Term-1 frames retained, in order, for the retained-client path.
    pub fn retained_bytes(&self) -> usize {
        self.ingress.queued_bytes()
    }
    pub fn original(&self) -> OriginalLink {
        self.original
    }
    pub fn pending_events(&self) -> usize {
        self.pending.events.len()
    }
    pub fn tls(&self) -> [u8; 32] {
        self.channel.tls()
    }
    /// Hand the warm link to the retained-client recovery owner.
    pub fn into_link(self) -> StandbyRecoveryLink {
        StandbyRecoveryLink {
            recovery: self.recovery,
            network: self.network,
            peer: self.peer,
            channel: self.channel,
            ingress: self.ingress,
            obsolete: self.obsolete,
            original: self.original,
            pending: self.pending,
        }
    }
    /// Intake and classify warm-link frames, then release queued term-1 sends
    /// (initially the signed `Promote`) only to an original that is still
    /// `Live` and admitted. A lost or unauthorized original receives nothing.
    pub fn poll(&mut self, now_ms: u64) -> Result<()> {
        if now_ms < self.last_ms {
            return Err(Error::Invalid("backwards promoted standby clock"));
        }
        self.last_ms = now_ms;
        own_link(&self.network, &self.descriptor, self.approved_until)?;
        for _ in 0..MAX_BATCH {
            let Some(event) = self.network.poll() else {
                break;
            };
            if let Some(bytes) = route(
                event,
                self.peer,
                &mut self.original,
                &mut self.pending,
                &mut self.network,
            )? {
                if self.obsolete.keep(self.session, &bytes) {
                    self.ingress.enqueue(bytes)?;
                }
            }
        }
        if self.original != OriginalLink::Live {
            return Ok(());
        }
        if !original_admitted(&self.network, &self.descriptor, self.peer) {
            self.original = OriginalLink::Unauthorized;
            return Ok(());
        }
        for _ in 0..MAX_BATCH {
            let Some(wire) = self.channel.poll_send(now_ms)? else {
                break;
            };
            self.network.send_reliable(self.peer, wire);
        }
        Ok(())
    }
}

/// The standby device's own human seat as a local recovery survivor, the
/// Stampede counterpart of Twang's reviewed `OwnSeat`. It owns that seat's
/// actual `ClientSession` and the delivery and intent journals its player
/// endpoint kept from arming. Recovery uses them in three steps, all at
/// frozen authority time C:
/// 1. [`Self::retain`] reports the seat's actual cut while collecting.
/// 2. [`Self::replay`] applies the seat's committed reliable replay through
///    the client's frozen `migration_recovery_step`, exactly once per output
///    sequence (the delivery journal skips duplicates and refuses gaps and
///    conflicts), then acknowledges it with `acknowledge_outputs` and
///    `commit_delivery_acks`.
/// 3. [`Self::seal`] acknowledges the committed handled prefix, seals the
///    seat's whole intent tail and supplies every unhandled intent, so Fray
///    drains each one once.
///
/// Lossy traffic has no recovery path. The seat's lossy outputs are not
/// replayed, and the frozen step runs no frame, so the client produces no
/// input datagram while recovering (any would be discarded and counted in
/// [`OwnSeatReplay::lossy_discarded`]). Input views that were in flight at C
/// are never applied by recovery: the authority stays at C, and the client's
/// predictions over those inputs are uncommitted.
///
/// A `Leave` from this seat that is in flight at C is honoured: it drains
/// once as a normal seat departure (a bot takes over the racer) and must be
/// the last intent of the sealed tail, as Fray requires of an input that
/// removes its own seat. Fray then retires the seat's watermark and drops it
/// from the survivors. From then on [`Self::retired`] is true and
/// [`Self::replay`] and [`Self::seal`] are no-ops that return `Ok` (the
/// replay reports `retired`), so a driver loop that keeps calling them while
/// Fray finishes other departures does not wedge; the caller stops driving
/// the seat. The promoted device keeps hosting without a player seat; the
/// seat is not reclaimed.
pub struct OwnSeat {
    player: u64,
    client: crate::client::ClientSession,
    delivery: DeliveryJournal,
    intents: IntentJournal,
    /// The frozen authority time C, once the seat confirmed its final cut.
    cut_logical: Option<u64>,
    /// [`Self::resumed`] re-anchored the client at `Resume` (exactly once).
    resumed: bool,
}
/// What one [`OwnSeat::replay`] call changed.
#[derive(Debug, Default)]
pub struct OwnSeatReplay {
    /// Reliable outputs newly applied by the seat's client (duplicates skipped).
    pub applied: usize,
    /// Input datagrams the client produced in those steps. The frozen step
    /// runs no frame, so this stays 0; recovery has no lossy path, and any
    /// would be dropped, never sent.
    pub lossy_discarded: usize,
    /// The committed delivery watermark, when it moved.
    pub receipt: Option<Receipt>,
    /// The seat has left (its own `Leave` drained): nothing was applied.
    pub retired: bool,
}
impl OwnSeat {
    /// `delivery` and `intents` are the seat's actual journals since arming.
    /// `committed` is the closed frontier's session image, where the
    /// standby's player must still be a member. While it is seated there, the
    /// client must be the native seat it holds. A seatless member (its
    /// departure batch, its own `Leave` say, addressed it an output) has no
    /// native seat left, so its client (normally `Closed("left")`) needs only
    /// to have held one; it is still local to this device and is retained
    /// and sealed at C, and the successor's first live batch retires it. A
    /// player retired before C is no member: `Unauthorized`.
    pub fn new(
        root: &decented_net::migration::Root,
        committed: &[u8],
        client: crate::client::ClientSession,
        delivery: DeliveryJournal,
        intents: IntentJournal,
    ) -> Result<Self> {
        let member = root
            .members
            .iter()
            .find(|m| m.key == Some(root.standby))
            .ok_or(Error::Unauthorized)?;
        let session = SessionImage::decode(root, committed)?;
        if !session
            .members
            .iter()
            .any(|m| m.player == member.player && m.key == Some(root.standby))
        {
            return Err(Error::Unauthorized);
        }
        let saved: BatchImage = options()
            .deserialize(&session.game)
            .map_err(|_| Error::Invalid("race recovery image"))?;
        let native = match saved.seats.iter().find(|seat| seat.player == member.player) {
            Some(seat) => Some(seat.native_id),
            // A seatless member: any native seat the client held.
            None => client.my_seat(),
        };
        if native.is_none() || client.my_seat() != native {
            return Err(Error::Unauthorized);
        }
        delivery.checkpoint()?;
        intents.checkpoint()?;
        Ok(Self {
            player: member.player,
            client,
            delivery,
            intents,
            cut_logical: None,
            resumed: false,
        })
    }
    pub fn player(&self) -> u64 {
        self.player
    }
    pub fn client(&self) -> &crate::client::ClientSession {
        &self.client
    }
    pub fn journals(&self) -> (&DeliveryJournal, &IntentJournal) {
        (&self.delivery, &self.intents)
    }
    /// Whether this seat has left: the committed receipt retired its
    /// watermark, because its own `Leave` drained or recovery departed it as
    /// absent. A retired seat is never reclaimed.
    pub fn retired(&self, recovery: &Recovery) -> bool {
        recovery
            .sequencer()
            .receipt()
            .watermarks
            .iter()
            .any(|m| m.player == self.player && m.retired)
    }
    /// While collecting: retain this seat's actual applied output and intent
    /// tail, never a watermark assumed on its behalf.
    pub fn retain(&self, recovery: &mut Recovery, now_ms: u64) -> Result<()> {
        recovery.retain_local(
            self.player,
            self.delivery.applied(),
            self.intents.tail(),
            now_ms,
        )
    }
    /// Apply this seat's records from `records` (normally `recovery.replay()`;
    /// other recipients are ignored) and commit the seat's delivery
    /// watermark. Delivering the same records again applies nothing.
    /// `local_ms` is the device's real local time (monotonic, in the client's
    /// own clock domain); game time stays at C (`recovery.logical_ms()`). The
    /// client's frozen recovery step runs no frame, whatever `local_ms` is, so
    /// it generates no input, ping, prediction or periodic traffic. A replayed
    /// `Pong` is not a clock sample. Reactive reliable followups to the
    /// replayed outputs (a `TrackReady` after a replayed `TrackData` completes
    /// a lobby download) are journaled in the same transaction; they extend
    /// the tail past the cut retained at C and are sealed and drained in the
    /// next input round. A retired seat applies nothing and returns `Ok`.
    ///
    /// Each chunk of at most `MAX_BATCH` outputs is one client transaction:
    /// its journals change only with the client. A failure leaves the seat at
    /// its last whole chunk and is safe to retry.
    pub fn replay(
        &mut self,
        recovery: &mut Recovery,
        keys: &decented_net::Keys,
        local_ms: u64,
        records: &[CertifiedOutput],
    ) -> Result<OwnSeatReplay> {
        if records.len() > MAX_OUTBOX_RECORDS {
            return Err(Error::Bounds);
        }
        if self.retired(recovery) {
            return Ok(OwnSeatReplay {
                retired: true,
                ..OwnSeatReplay::default()
            });
        }
        let context = recovery.root().context()?;
        let promotion = recovery.promotion().clone();
        let receipt = recovery.sequencer().receipt().clone();
        let mut own = Vec::new();
        for record in records {
            if record.output.recipient != self.player {
                continue;
            }
            check_recovery_record(&context, &promotion, &receipt, record)?;
            if record.output.delivery == Delivery::Reliable {
                own.push(record);
            }
        }
        let logical_ms = recovery.logical_ms();
        let mut result = OwnSeatReplay::default();
        for chunk in own.chunks(MAX_BATCH) {
            let mut delivery = DeliveryJournal::restore(&self.delivery.checkpoint()?)?;
            let mut intents = IntentJournal::restore(&self.intents.checkpoint()?)?;
            let mut incoming = Vec::new();
            for record in chunk {
                delivery.apply_certified(&context, self.player, record, |bytes| {
                    incoming.push(crate::transport::ClientNetEvent::Reliable(bytes.to_vec()));
                    Ok(())
                })?;
            }
            if incoming.is_empty() {
                continue;
            }
            let batch = self.client.migration_recovery_step(
                &mut intents,
                local_ms,
                logical_ms,
                &incoming,
                None,
            )?;
            self.delivery = delivery;
            self.intents = intents;
            result.applied += incoming.len();
            result.lossy_discarded += batch.lossy.len();
        }
        recovery.acknowledge_outputs(self.player, self.delivery.applied())?;
        result.receipt = recovery.commit_delivery_acks(keys)?;
        Ok(result)
    }
    /// At the recovery barrier: confirm this seat's final journals and its
    /// bounded clock correction (its host-time estimate at `frozen_local`,
    /// its last frame before the freeze, minus C), then rebase its client so
    /// host time at `local_ms` reads C and no clock round stays open. Fray
    /// refuses a correction over 250 ms before the client is touched; the
    /// session then stays paused. Returns the correction. A retired seat is
    /// not in the barrier's membership and is not called.
    pub fn ready(
        &mut self,
        barrier: &mut RecoveryBarrier,
        frozen_local: u64,
        local_ms: u64,
        now_ms: u64,
    ) -> Result<i64> {
        let logical_ms = barrier.promotion().final_receipt.logical_ms;
        let delta = self.client.migration_clock_delta(frozen_local, logical_ms);
        self.confirm_cut(barrier, delta, local_ms, now_ms)
    }
    /// As [`Self::ready`], with the seat's host-time estimate at its last
    /// stepped frame as that frame recorded it
    /// ([`super::RemotePlayer::last_frame_host`]): the correction is that
    /// minus C, whatever a sync round completing since did to the clock.
    pub fn ready_at_host(
        &mut self,
        barrier: &mut RecoveryBarrier,
        frozen_host: u64,
        local_ms: u64,
        now_ms: u64,
    ) -> Result<i64> {
        let logical_ms = barrier.promotion().final_receipt.logical_ms;
        let delta = (frozen_host as i64).saturating_sub(logical_ms as i64);
        self.confirm_cut(barrier, delta, local_ms, now_ms)
    }
    fn confirm_cut(
        &mut self,
        barrier: &mut RecoveryBarrier,
        delta: i64,
        local_ms: u64,
        now_ms: u64,
    ) -> Result<i64> {
        let logical_ms = barrier.promotion().final_receipt.logical_ms;
        barrier.retain_local(self.player, &self.delivery, &self.intents, delta, now_ms)?;
        self.client
            .migration_rebase_clock(local_ms, logical_ms, delta)
            .map_err(Error::Invalid)?;
        self.cut_logical = Some(logical_ms);
        Ok(delta)
    }
    /// At `Resume`: re-anchor host time C to `local_ms`, the local time the
    /// standby issued `Resume`. The barrier may have waited for remote
    /// survivors since this seat's cut; the seat continues from C, not from C
    /// plus that wait. Refused before [`Self::ready`] and a second time (a
    /// later re-anchor would move the seat's clock back).
    pub fn resumed(&mut self, local_ms: u64) -> Result<()> {
        let logical_ms = self
            .cut_logical
            .ok_or(Error::Invalid("own seat not ready"))?;
        if self.resumed {
            return Err(Error::Invalid("own seat already resumed"));
        }
        self.client
            .migration_rebase_clock(local_ms, logical_ms, 0)
            .map_err(Error::Invalid)?;
        self.resumed = true;
        Ok(())
    }
    /// After `Resume`: the seat's client and journals, for the successor's
    /// local seat. Refused before [`Self::resumed`], so the seat's first
    /// successor frame runs on a clock re-anchored at `Resume`.
    pub(super) fn into_parts(
        self,
    ) -> Result<(
        u64,
        crate::client::ClientSession,
        DeliveryJournal,
        IntentJournal,
    )> {
        self.cut_logical
            .ok_or(Error::Invalid("own seat not ready"))?;
        if !self.resumed {
            return Err(Error::Invalid("own seat not resumed"));
        }
        Ok((self.player, self.client, self.delivery, self.intents))
    }
    /// For an input round: release intents the committed history already
    /// handled, seal the seat's whole tail and supply every unhandled intent.
    /// Supplying the same tail again is a no-op in Fray. A retired seat has
    /// nothing to seal: a no-op `Ok`.
    pub fn seal(&mut self, recovery: &mut Recovery) -> Result<()> {
        if self.retired(recovery) {
            return Ok(());
        }
        let handled = recovery
            .histories()
            .get(&self.player)
            .ok_or(Error::Unauthorized)?
            .handled();
        if handled > self.intents.acknowledged() {
            self.intents.acknowledge(handled)?;
        }
        recovery.seal_inputs(self.player, self.intents.tail())?;
        for (sequence, bytes) in self.intents.pending() {
            recovery.supply(self.player, sequence, bytes.to_vec())?;
        }
        Ok(())
    }
}
