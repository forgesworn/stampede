//! Owned original-authority IO and receipt-gated adoption after lobby
//! activation, the Stampede counterpart of Twang's `LivePrimary`.
//!
//! The runtime owns the native race authority (World, bots, items, custom
//! track, seats, input queues and pending results), Fray's `PrimaryActor`,
//! the pair link and its ingress. Player endpoints, the app lifecycle and the
//! standby side remain separate owners.
use super::*;
use crate::transport::{ClientNetEvent, ClientTransport};
use decented_net::migration::{
    Activity, CertifiedOutput, Channel, Descriptor, Frame, MembershipCertificate, PacketKind,
    PlayerQueue, Primary, PrimaryActor, PrimaryEvent, PrimaryUpdate, Promotion, ReceiveQueue,
    ReconnectChallenge, SenderSequence, SessionId, WatchStatus, MAX_BATCH,
};

/// Fray rejects a second pulse probe within this many milliseconds of the last
/// one as `Stale` (`decented-net` `migration.rs`, `ProgressWatch`/`Watch`
/// `probe_with_sequence`: `now_ms - last < 50`). The value is private there, so
/// it is mirrored here; the wrappers coalesce instead of latching on it.
const PROBE_MIN_GAP_MS: u64 = 50;

/// While a batch waits for its receipt, [`LivePrimary::poll`] resends it at
/// most once per this many milliseconds of one committed frontier, counted
/// from the stage or the previous resend, whether or not a frame was refused.
/// It is shorter than Fray's 200 ms standby watch ([`STANDBY_WATCH_MS`]), so
/// a lost receipt is retried before the room shows backup lost.
pub const RETRY_MIN_GAP_MS: u64 = 150;

/// Fray's standby-progress watch (`ProgressWatch::status`, read through
/// `PrimaryActor::status`) reports `Paused` after this many milliseconds
/// without standby progress. The value is private there and mirrored here
/// for tests and docs; [`LivePrimary::poll`] soft-pauses on Fray's verdict as
/// it is ([`LivePrimary::backup_lost`]), with no tolerance of its own.
pub const STANDBY_WATCH_MS: u64 = 200;

/// A player whose term-0 link parked for longer than this (on the original's
/// [`RunnerClock`], counted from the park) is departed by the host in the
/// next batch staged, in every phase: an explicit host-side departure, the
/// native kick. This retires the player and its outbox before the outbox
/// bound (`MAX_OUTBOX_RECORDS` / `MAX_OUTBOX_BYTES`) can pause the room.
/// While racing, the native silence rule (`drop_silent`, 6 s of game time)
/// usually departs a parked racer first.
pub const PARKED_DEPARTURE_MS: u64 = 30_000;

/// Term-0 physical frames ignored after the promotion certificate was learned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObsoleteIngress {
    /// Candidate/delta frames refused after close. No restore, install or ACK.
    pub candidate_frames: usize,
    /// Receipts, liveness, capability and other control frames. No effect.
    pub other_frames: usize,
}
impl ObsoleteIngress {
    pub fn total(&self) -> usize {
        self.candidate_frames + self.other_frames
    }
    /// The single obsolete-ingress filter. Returns true when the frame must be
    /// kept for the recovery link and false when it was counted as obsolete.
    /// Term-1 frames, other sessions and undecodable bytes stay queued in
    /// order; the recovery `Channel` still applies its own checks to them.
    pub(super) fn keep(&mut self, session: SessionId, wire: &[u8]) -> bool {
        let Ok(frame) = Frame::decode(wire) else {
            return true;
        };
        if frame.session != session || frame.term != 0 {
            return true;
        }
        match frame.kind {
            PacketKind::Candidate | PacketKind::Delta | PacketKind::InitialImage => {
                self.candidate_frames += 1
            }
            _ => self.other_frames += 1,
        }
        false
    }
    /// Re-queue only frames which may cross into recovery, preserving order.
    pub(super) fn retain(&mut self, session: SessionId, ingress: &mut ReceiveQueue) -> Result<()> {
        let mut kept = ReceiveQueue::default();
        while let Some(wire) = ingress.poll(|wire| Ok(wire.to_vec()))? {
            if self.keep(session, &wire) {
                kept.enqueue(wire)?;
            }
        }
        *ingress = kept;
        Ok(())
    }
}

pub enum RuntimeEvent {
    /// The complete native authority has already been adopted. Every game
    /// output carries its commit proof; transport controls may now be applied.
    Committed {
        outputs: Vec<CertifiedOutput>,
        controls: Vec<Effect>,
        /// Refresh the caller's verified queues against `primary()` and then
        /// discard only these selected replaceable views.
        selected_views: Vec<(u64, u64)>,
    },
    /// The old publication capability is closed. Keep the human endpoints and
    /// journals, then consume this owner for its exact warm recovery link.
    Promoted(Promotion),
    /// The standby's signed membership certificate arrived over the pair
    /// link, already verified by Fray against the installed receipt and its
    /// image; [`LivePrimary::membership_certificate`] now returns it. It
    /// follows the `Committed` for the same receipt on a later frame, so act
    /// on membership (a player host's `certify`, a reconnect challenge) here,
    /// never on `Committed`. It is the last event of its poll, so when the
    /// poll returns it is still the certificate for the installed receipt.
    /// An index-0 certificate that arrived before the
    /// activation handoff produces no event; read it from the accessor.
    Membership(MembershipCertificate),
}

/// The closed original's warm link. `ingress` holds only frames which may
/// cross into recovery: term-0 traffic read after `Promote` was counted in
/// `obsolete`, never applied.
pub struct OriginalRecoveryLink {
    pub network: crate::net::QuicClient,
    pub channel: Channel,
    pub ingress: ReceiveQueue,
    pub obsolete: ObsoleteIngress,
}
impl OriginalRecoveryLink {
    /// Move later physical frames into `ingress` through the same filter.
    pub fn intake(&mut self) -> Result<()> {
        let session = self.channel.binding().session;
        for _ in 0..MAX_BATCH {
            let Some(event) = self.network.poll() else {
                break;
            };
            match event {
                ClientNetEvent::Reliable(bytes) => {
                    if self.obsolete.keep(session, &bytes) {
                        self.ingress.enqueue(bytes)?
                    }
                }
                ClientNetEvent::Progress(_) => {}
                _ => return Err(Error::Closed),
            }
        }
        Ok(())
    }
}

pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_secs())
        .unwrap_or(0)
}

/// Actual-UTC expiry of this runtime's own My Signet approval, verified with
/// the unchanged game verifier. There is no caller-supplied UTC.
pub(super) fn own_approval(identity: &decented_net::Identity) -> Result<u64> {
    let json = identity.attestation().ok_or(Error::Unauthorized)?;
    let until = decented_signet::verify_attestation(
        crate::net::APP_TAG,
        json.as_bytes(),
        &identity.pubkey(),
        unix_now(),
    )
    .map_err(|_| Error::Unauthorized)?
    .valid_until;
    if until <= unix_now() {
        return Err(Error::Unauthorized);
    }
    Ok(until)
}

/// Owns every original native authority mutation and receipt-gated adoption.
/// Its read accessors expose no mutable authority, actor or transport handle.
pub struct LivePrimary {
    descriptor: Descriptor,
    actor: PrimaryActor,
    network: crate::net::QuicClient,
    ingress: ReceiveQueue,
    keys: decented_net::Keys,
    approved_until: u64,
    authority: HostAuthority,
    pending: Option<QueuedBatch>,
    failure: Option<Error>,
    promoted: bool,
    obsolete: ObsoleteIngress,
    clock: RunnerClock,
    last_ms: u64,
    last_probe_ms: Option<u64>,
    /// Pair-link frames refused as `Stale` or `Gap` and dropped ([`Self::poll`]).
    refused: u64,
    /// Resends of a waiting batch ([`Self::poll`]).
    retries: u64,
    /// The committed frontier index and runner time of the pending batch's
    /// stage or its last resend.
    last_retry: Option<(u64, u64)>,
    /// Members whose player link parked, with the runner-clock time of the
    /// park ([`Self::park`]).
    parked: BTreeMap<u64, u64>,
    /// The activity handed to the actor's standby watch, derived from the
    /// committed authority ([`Self::activity`]).
    activity: Activity,
    /// Fray's standby watch read `Paused` at the last poll: the soft
    /// backup-lost pause ([`Self::backup_lost`]).
    backup_lost: bool,
    /// The driver's game clock is at the skew budget and stopped: the soft
    /// behind pause ([`Self::behind`], [`Self::pace`]).
    behind: bool,
    /// The game time and runner time of the last commit that moved game
    /// time on.
    advanced: (u64, u64),
    /// While behind: since when commits have moved game time on with no
    /// stop of [`FREEZE_AFTER_MS`].
    flowing_since: Option<u64>,
}
impl LivePrimary {
    /// `clock` is the device's [`RunnerClock`]: every watch and framing time
    /// this runner hands its actor is sampled from it, never supplied by the
    /// caller.
    pub fn new(
        descriptor: Descriptor,
        bootstrap: PrimaryBootstrap,
        authority: HostAuthority,
        identity: decented_net::Identity,
        clock: RunnerClock,
    ) -> Result<Self> {
        let now_ms = clock.now_ms();
        descriptor.verify()?;
        let approved_until = own_approval(&identity)?;
        let keys = identity.keys().clone();
        let root = &descriptor.root;
        if keys.public_key().to_bytes() != root.host
            || bootstrap.network.migration_keys()? != (root.host, root.standby)
        {
            return Err(Error::Unauthorized);
        }
        // A headless original never holds a player seat of its own.
        if authority.headless && root.members.iter().any(|m| m.key == Some(root.host)) {
            return Err(Error::Conflict);
        }
        let initial = decented_net::migration::SessionImage::decode(
            root,
            bootstrap.primary.committed_image(),
        )?;
        bootstrap.armed.verify_initial(
            root,
            state_hash(&initial.encode(root)?)?,
            initial.logical_ms,
        )?;
        if checkpoint_batch(&authority)?.1 != initial.game {
            return Err(Error::Conflict);
        }
        let actor = PrimaryActor::new(
            descriptor.clone(),
            bootstrap.primary,
            bootstrap.channel,
            Activity::Idle,
            now_ms,
        )?;
        Ok(Self {
            descriptor,
            actor,
            network: bootstrap.network,
            ingress: bootstrap.ingress,
            keys,
            approved_until,
            authority,
            pending: None,
            failure: None,
            promoted: false,
            obsolete: Default::default(),
            clock,
            last_ms: now_ms,
            last_probe_ms: None,
            refused: 0,
            retries: 0,
            last_retry: None,
            parked: BTreeMap::new(),
            activity: Activity::Idle,
            backup_lost: false,
            behind: false,
            advanced: (initial.logical_ms, now_ms),
            flowing_since: None,
        })
    }
    pub fn root(&self) -> &decented_net::migration::Root {
        &self.descriptor.root
    }
    /// The last adopted (committed) authority. A staged batch is never visible.
    pub fn authority(&self) -> &HostAuthority {
        &self.authority
    }
    pub fn primary(&self) -> &Primary {
        self.actor.primary()
    }
    /// The newest standby-signed membership certificate learned over the pair
    /// link (the index-0 certificate carried over from activation, or a later
    /// one reported as [`RuntimeEvent::Membership`]). It may trail
    /// `primary().receipt()` until the certificate for that receipt arrives;
    /// pair a receipt only with the certificate for that same receipt.
    pub fn membership_certificate(&self) -> Option<&MembershipCertificate> {
        self.actor.membership_certificate()
    }
    pub fn sender_sequence(&self) -> SenderSequence {
        self.actor.primary().sender_sequence()
    }
    pub fn failure(&self) -> Option<&Error> {
        self.failure.as_ref()
    }
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
    /// Pair-link frames [`Self::poll`] dropped as refused (`Stale`/`Gap`)
    /// instead of latching.
    pub fn refused_frames(&self) -> u64 {
        self.refused
    }
    /// Times [`Self::poll`] resent a batch that was waiting for its receipt.
    pub fn retries(&self) -> u64 {
        self.retries
    }
    /// The activity this original's standby watch runs with, derived from
    /// the committed authority: `Advancing` in every phase whose timer runs
    /// (`Racing`, and `Results`, where the world still steps and the rematch
    /// 3-2-1 runs), `Idle` in the lobby and the switch window before the
    /// first armed commit. There is no setter.
    pub fn activity(&self) -> Activity {
        self.activity
    }
    /// The soft backup-lost pause: at the last [`Self::poll`], Fray's
    /// standby-progress watch reported `Paused`. Show "Backup lost — the
    /// room is paused" while this holds. It is not a latch: it clears by
    /// itself at the first poll whose watch reads `Live` again, and
    /// meanwhile polling, probing, retrying and staging go on (nothing
    /// commits without the standby's receipt). False once a genuine fault
    /// latched ([`Self::failure`] is the reason then), and never reported
    /// after `Promote` closed term 0: recovery owns the state then.
    pub fn backup_lost(&self) -> bool {
        self.backup_lost
            && self.failure.is_none()
            && !self.promoted
            && !self.actor.primary().is_closed()
    }
    /// Pace the driver's game clock `now` to the skew budget, once per frame
    /// before anything uses it (the own seat's frame, [`Self::stage`]), in
    /// every phase and whether or not a batch waits. Returns the game time
    /// to drive this frame: `now`, capped at [`MAX_GAME_GAP_MS`] past the
    /// last commit. The driver adds the difference to its clock's shift
    /// (as its hold across the switch does), so its game-facing clock stops
    /// at the cap, then moves only as commits move the cap, and runs on
    /// continuously once they catch up: game time never runs more than
    /// 250 ms ahead of the commits and never jumps.
    ///
    /// A capped clock that also stopped is the soft behind pause
    /// ([`Self::behind`]). Remote clients do not follow the hold through
    /// their clock offset; they keep their own host-time estimate within
    /// `MAX_LEAD_MS` of the commits instead, and resync in the background.
    pub fn pace(&mut self, now: u64) -> u64 {
        let committed = self.actor.primary().receipt().logical_ms;
        let lag = now.saturating_sub(committed);
        let wall = self.clock.now_ms();
        let stopped = wall.saturating_sub(self.advanced.1) >= FREEZE_AFTER_MS;
        if lag > MAX_GAME_GAP_MS && stopped {
            self.behind = true;
            self.flowing_since = None;
        } else if self.behind {
            if lag <= MAX_GAME_GAP_MS - MAX_BATCH_GAME_MS {
                self.behind = false;
            } else if stopped {
                self.flowing_since = None;
            } else if wall.saturating_sub(*self.flowing_since.get_or_insert(wall))
                >= BEHIND_CLEAR_MS
            {
                self.behind = false;
            }
            if !self.behind {
                self.flowing_since = None;
            }
        }
        now.min(committed + MAX_GAME_GAP_MS)
    }
    /// The soft behind pause: the driver's game clock is at the skew budget
    /// past the last commit and game time stopped: no commit moved it on
    /// for [`FREEZE_AFTER_MS`] (the clients' own freeze rule). Show the room
    /// paused while this holds. Like [`Self::backup_lost`] it is not a
    /// latch: polling, probing, retrying and staging go on (each batch at
    /// most 50 ms of game time, and nothing commits without the standby's
    /// receipt). It clears by itself once commits bring the lag down to
    /// `MAX_GAME_GAP_MS - MAX_BATCH_GAME_MS`, or once they have moved game
    /// time on with no such stop for [`BEHIND_CLEAR_MS`]: a commit cycle
    /// over 50 ms keeps the clock at the budget for good, but the game runs
    /// on (slower than real time), so that is not a pause. Read through
    /// [`Self::pace`], once per frame. False once a genuine fault latched,
    /// and never reported after `Promote` closed term 0.
    pub fn behind(&self) -> bool {
        self.behind && self.failure.is_none() && !self.promoted && !self.actor.primary().is_closed()
    }
    /// Raw access to the pair link's transport, for hostile tests only (like
    /// `LiveStandby::pair_network_mut`). Frames read here never reach the
    /// actor; anything sent bypasses it.
    #[doc(hidden)]
    pub fn pair_network_mut(&mut self) -> &mut crate::net::QuicClient {
        &mut self.network
    }
    /// Term-0 frames ignored after this original read the signed `Promote`.
    pub fn obsolete(&self) -> ObsoleteIngress {
        self.obsolete
    }
    /// Frames retained for the recovery link after promotion.
    pub fn queued_bytes(&self) -> usize {
        self.ingress.queued_bytes()
    }
    fn guard(&self) -> Result<()> {
        if self.failure.is_some() {
            return Err(Error::Closed);
        }
        self.guard_connection()
    }
    /// Shared replay state, this runtime's own approval and the standby's
    /// admission on the pair link, all on actual UTC.
    fn guard_connection(&self) -> Result<()> {
        self.actor.primary().sender_sequence().check_live()?;
        if self.approved_until <= unix_now() {
            return Err(Error::Unauthorized);
        }
        let root = self.root();
        if self.network.migration_keys()? != (root.host, root.standby) {
            return Err(Error::Unauthorized);
        }
        Ok(())
    }
    /// A closed original never stages, probes or reports activity again.
    fn open(&self) -> Result<()> {
        if self.promoted || self.actor.primary().is_closed() {
            return Err(Error::Closed);
        }
        Ok(())
    }
    fn latch<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            if self.failure.is_none() {
                self.failure = Some(error.clone());
                self.actor.pause(error.clone());
            }
        }
        result
    }
    fn advance(&mut self, now_ms: u64) -> Result<()> {
        if now_ms < self.last_ms {
            return Err(Error::Invalid("backwards original runtime clock"));
        }
        self.last_ms = now_ms;
        Ok(())
    }
    /// A fresh sample of the runner clock for one actor call. It is recorded
    /// before use, so no later call can hand the actor an earlier time.
    fn actor_clock(&mut self) -> Result<u64> {
        let current = self.clock.now_ms();
        self.advance(current)?;
        Ok(current)
    }
    /// Attach one admitted player's verified input queue. A refused attach
    /// changes no native state and does not pause the runtime.
    pub fn attach_player(
        &mut self,
        channel: &Channel,
        membership: MembershipCertificate,
    ) -> Result<PlayerQueue> {
        self.guard()?;
        self.open()?;
        self.actor.attach_player(channel, membership)
    }
    /// Challenge a replacement connection for an attached player's queue,
    /// signed by this original (term 0). `membership` is the standby's
    /// certificate at the installed receipt, as learned over the pair link
    /// ([`Self::membership_certificate`]). The queue keeps its original
    /// IDs, bytes and delivery cursor; nothing native changes, and a refused
    /// challenge does not pause the runtime. A seatless member (it left in a
    /// batch that addressed it an output, and the next batch retires it) is
    /// refused: it left explicitly.
    pub fn begin_player_reconnect(
        &self,
        queue: &mut PlayerQueue,
        channel: &Channel,
        membership: MembershipCertificate,
        now_ms: u64,
    ) -> Result<ReconnectChallenge> {
        self.guard()?;
        self.open()?;
        if seatless_players(self.root(), self.actor.primary().committed_image())?
            .contains(&queue.player())
        {
            return Err(Error::Unauthorized);
        }
        queue.begin_reconnect(
            channel,
            membership,
            self.actor.primary(),
            now_ms,
            &self.keys,
        )
    }
    /// A member's term-0 player link parked: its connection left (or was
    /// closed) and its queue waits for a reconnect. `PlayerHost::poll` calls
    /// this for every parked network route. The park is stamped on this
    /// runner's [`RunnerClock`], never a client's; a repeated call keeps the
    /// first stamp. A player that is not a keyed member of the committed
    /// image, and the original's own seat (its route is local; the hosting
    /// authority never departs its own seat), are ignored. Once the player
    /// has been parked for longer than [`PARKED_DEPARTURE_MS`], the next
    /// [`Self::stage`] departs it. [`Self::unpark`] cancels it.
    pub fn park(&mut self, player: u64) -> Result<()> {
        if self.parked.contains_key(&player) {
            return Ok(());
        }
        let image = decented_net::migration::SessionImage::decode(
            self.root(),
            self.actor.primary().committed_image(),
        )?;
        let host = self.root().host;
        if image
            .members
            .iter()
            .any(|m| m.player == player && m.key.is_some() && m.key != Some(host))
        {
            self.parked.insert(player, self.clock.now_ms());
        }
        Ok(())
    }
    /// The parked player reconnected: its replacement connection proved its
    /// journals and carries the queue again. Its pending departure is
    /// cancelled; a later drop parks it afresh.
    pub fn unpark(&mut self, player: u64) {
        self.parked.remove(&player);
    }
    /// When `player`'s link parked, on this runner's clock, until it
    /// reconnects or leaves the membership.
    pub fn parked_at(&self, player: u64) -> Option<u64> {
        self.parked.get(&player).copied()
    }
    /// The members parked for longer than [`PARKED_DEPARTURE_MS`] at
    /// `now_ms` whose native seat the kick will remove, with its native id.
    /// The staged authority is restored from [`Self::authority`] (staging
    /// checks the checkpoint bytes), so `kick` succeeds there exactly when
    /// the committed authority holds the seat and is not frozen. A player it
    /// would not remove stays parked, its queue fed, and is retried on the
    /// next stage.
    fn due_departures(&self, now_ms: u64) -> Result<Vec<(u64, u8)>> {
        let due: Vec<u64> = self
            .parked
            .iter()
            .filter(|(_, at)| now_ms.saturating_sub(**at) > PARKED_DEPARTURE_MS)
            .map(|(player, _)| *player)
            .collect();
        if due.is_empty() || self.authority.migration_frozen() {
            return Ok(Vec::new());
        }
        let image = decented_net::migration::SessionImage::decode(
            self.root(),
            self.actor.primary().committed_image(),
        )?;
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
    /// Return false while the previous batch waits for a receipt. Operations
    /// run on fresh private authorities and may run twice; capture command
    /// data, never side effects outside that authority.
    ///
    /// Every member parked for longer than [`PARKED_DEPARTURE_MS`] (on the
    /// runner clock, sampled once per call) is departed in this batch by the
    /// native kick (`HostAuthority::kick`), before `operation` runs, and its
    /// queue is not fed: what it delivered before the drop is dropped with
    /// it (a queued `Hello` read after the kick would re-admit it). Like any
    /// native removal of a seat in a batch, it commits as a departure. The
    /// kick addresses the leaver its `Closed("kicked")`, so it stays a
    /// seatless member for this batch (keep-then-retire), and the next batch
    /// retires it: its watermark retires, the membership revision rises and
    /// `PlayerHost::certify` installs the standby's certificate on the
    /// listener.
    ///
    /// `now` is the driver's game clock, paced by [`Self::pace`]. A batch
    /// spans at most 50 ms of game time, so after a slow frame this stages
    /// the first 50 ms and the driver stages again after each commit until
    /// the committed receipt reaches `now` (see `batch_time`). A paced clock
    /// is at most 250 ms past the last commit (`pace` caps it at the commit
    /// before this frame's poll). One more than 300 ms past it can only come
    /// from a driver that does not pace, and is refused as skew.
    pub fn stage(
        &mut self,
        now: u64,
        queues: &[&PlayerQueue],
        rtts: &BTreeMap<PeerKey, Option<u32>>,
        mut operation: impl FnMut(&mut HostAuthority) -> Result<()>,
    ) -> Result<bool> {
        let result = (|| {
            self.guard()?;
            self.open()?;
            let committed = self.actor.primary().receipt().logical_ms;
            if now.saturating_sub(committed) > MAX_GAME_GAP_MS + MAX_BATCH_GAME_MS {
                return Err(Error::Invalid("game clock skew over 250 ms (unpaced)"));
            }
            if self.pending.is_some() {
                return Ok(false);
            }
            let now = batch_time(self.actor.primary().receipt().logical_ms, now);
            let departing = self.due_departures(self.clock.now_ms())?;
            let queues = queues
                .iter()
                .copied()
                .filter(|q| !departing.iter().any(|(player, _)| *player == q.player()))
                .collect::<Vec<_>>();
            let staged = try_stage_queued_batch(
                &self.authority,
                self.actor.primary(),
                self.root(),
                now,
                &queues,
                None,
                rtts,
                || self.guard(),
                |host| {
                    self.guard()?;
                    // `due_departures` kept only seats this kick removes. Were
                    // one refused, the player would stay seated and parked,
                    // its unfed queue intact (its watermark does not move),
                    // and the next stage would try again.
                    for (_, native_id) in &departing {
                        host.kick(*native_id);
                    }
                    operation(host)?;
                    self.guard()
                },
            )?;
            self.guard()?;
            staged.prepare_actor(&mut self.actor)?;
            self.pending = Some(staged);
            let staged_at = self.clock.now_ms();
            self.last_retry = Some((self.actor.primary().receipt().index, staged_at));
            // The candidate leaves in this frame, not at the next poll.
            self.send_queued()?;
            Ok(true)
        })();
        self.latch(result)
    }
    pub fn status(&mut self) -> Result<WatchStatus> {
        let result = (|| {
            let now_ms = self.actor_clock()?;
            self.guard()?;
            self.actor.status(now_ms)
        })();
        self.latch(result)
    }
    /// Probe the standby. The driver calls this every frame, in every phase,
    /// before [`Self::poll`] (whose send loop carries it): an idle standby
    /// answers with an idle pulse, which keeps the standby watch live while
    /// nothing commits, and Fray defers its verdict after this runner's own
    /// stall only if a probe went out within 200 ms before the gap. A driver
    /// that forgets costs only a false soft pause ([`Self::backup_lost`]),
    /// never a takeover. A probe within `PROBE_MIN_GAP_MS` of the last one is
    /// a no-op success, and so is a probe once `Promote` closed term 0 (a
    /// closed original never probes; a per-frame driver must not latch it).
    pub fn probe(&mut self) -> Result<()> {
        let result = (|| {
            let now_ms = self.actor_clock()?;
            self.guard()?;
            if self.open().is_err() {
                return Ok(());
            }
            if self
                .last_probe_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < PROBE_MIN_GAP_MS)
            {
                return Ok(());
            }
            self.actor.probe(now_ms)?;
            self.last_probe_ms = Some(now_ms);
            Ok(())
        })();
        self.latch(result)
    }
    /// Hand the actor the activity of the committed authority, on a change
    /// only. Called after every adoption: the receipt index and phase move
    /// only there.
    fn sync_activity(&mut self) -> Result<()> {
        let activity = activity_of(self.actor.primary().receipt().index, self.authority.phase());
        if activity != self.activity {
            self.actor.set_activity(activity)?;
            self.activity = activity;
        }
        Ok(())
    }
    /// Send everything the actor has queued for the pair link.
    fn send_queued(&mut self) -> Result<()> {
        for _ in 0..MAX_BATCH {
            self.guard_connection()?;
            let current = self.actor_clock()?;
            let Some(wire) = self.actor.poll_send(current)? else {
                break;
            };
            self.network.send_reliable(wire);
        }
        Ok(())
    }
    /// Actual UTC admission is checked before every ingest, release and native
    /// adoption. Each actor call gets a fresh runner-clock sample, so native
    /// wall cost also advances framing/watch time this call.
    ///
    /// Pair-link robustness, in this order on every poll:
    /// - A pair-link frame that Fray refuses as `Stale` or `Gap` is dropped and
    ///   counted ([`Self::refused_frames`]), never latched: such a refusal
    ///   records nothing and leaves the actor usable. It covers an older or
    ///   not-yet-installed certificate, a Pulse answering no probe inside the
    ///   watch's window, and an expired partial transfer (Fray discards a
    ///   transfer whose chunks are read 500 ms or more apart, so a poll hitch
    ///   between the two chunks of a certificate or receipt refuses the
    ///   second). Every other refusal latches, and so does a replay session
    ///   that the frame failed (`guard_connection` runs after every frame).
    /// - The receipt of the waiting batch may have been such a frame, or lost
    ///   without any later refusal, so the retry is owed by time: while a
    ///   batch waits, it is resent at most once per [`RETRY_MIN_GAP_MS`] of
    ///   one frontier, counted from its stage ([`Self::retries`]). The standby
    ///   answers an identical retry with the receipt and certificate again.
    /// - Probing is the driver's job ([`Self::probe`], every frame before
    ///   this poll); this poll's send loop carries the probe.
    /// - Then, after the frames read this poll, Fray's standby-progress watch
    ///   (`status()`) is read. While it reports `Paused` the standby is lost:
    ///   [`Self::backup_lost`] holds and the app shows "Backup lost — the room
    ///   is paused". This is a soft pause, not a latch: it clears by itself
    ///   when the watch reads `Live` again, and polling, probing and retrying
    ///   go on throughout. A missed Promotion shows as this pause for good.
    ///   The verdict is Fray's as it is: no game-side tolerance and no gate
    ///   on a waiting batch. Its activity is derived from the committed
    ///   authority ([`Self::activity`]).
    ///
    /// The send loop runs last, so the driver's probe and this poll's retry
    /// leave now; a batch staged after this poll leaves from [`Self::stage`]
    /// itself.
    pub fn poll(&mut self) -> Result<Vec<RuntimeEvent>> {
        let result = (|| {
            self.actor_clock()?;
            self.guard_connection()?;
            let session = self.descriptor.root.session;
            for _ in 0..MAX_BATCH {
                let Some(event) = self.network.poll() else {
                    break;
                };
                match event {
                    ClientNetEvent::Reliable(bytes) => {
                        if !self.promoted || self.obsolete.keep(session, &bytes) {
                            self.ingress.enqueue(bytes)?
                        }
                    }
                    ClientNetEvent::Progress(_) => {}
                    _ => return Err(Error::Closed),
                }
            }
            let mut events = Vec::new();
            if self.promoted {
                // The closed actor never sees later frames; they wait for
                // the recovery link (term 1) or were counted above (term 0).
                return Ok(events);
            }
            for _ in 0..MAX_BATCH {
                self.guard_connection()?;
                let current = self.actor_clock()?;
                let Some(received) = self.ingress.poll(|wire| {
                    match self.actor.receive_update(wire, current, &self.keys) {
                        // Not consumed: the frame stays queued for a later poll.
                        Err(Error::RateLimited) => Err(Error::RateLimited),
                        result => Ok(result),
                    }
                })?
                else {
                    break;
                };
                self.guard_connection()?;
                let event = match received {
                    Ok(event) => event,
                    Err(Error::Stale | Error::Gap) => {
                        self.refused += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                match event {
                    Some(PrimaryUpdate::Event(PrimaryEvent::Committed { outputs, .. })) => {
                        let staged = self.pending.take().ok_or(Error::Conflict)?;
                        let selected_views = staged.selected_views().to_vec();
                        let (authority, certified, controls) =
                            staged.adopt_actor(&mut self.actor, &outputs)?;
                        self.guard_connection()?;
                        self.authority = authority;
                        let logical = self.actor.primary().receipt().logical_ms;
                        if logical > self.advanced.0 {
                            self.advanced = (logical, self.clock.now_ms());
                        }
                        // A parked player that left the membership (any
                        // departure) is no longer tracked.
                        let image = decented_net::migration::SessionImage::decode(
                            &self.descriptor.root,
                            self.actor.primary().committed_image(),
                        )?;
                        self.parked
                            .retain(|player, _| image.members.iter().any(|m| m.player == *player));
                        self.sync_activity()?;
                        events.push(RuntimeEvent::Committed {
                            outputs: certified,
                            controls,
                            selected_views,
                        });
                    }
                    Some(PrimaryUpdate::Event(PrimaryEvent::Promoted(promotion))) => {
                        // Close dropped the pending candidate (or validated
                        // it into the final receipt); neither is adopted here.
                        self.pending = None;
                        self.promoted = true;
                        self.obsolete.retain(session, &mut self.ingress)?;
                        events.push(RuntimeEvent::Promoted(promotion));
                        return Ok(events);
                    }
                    Some(PrimaryUpdate::Membership(certificate)) => {
                        events.push(RuntimeEvent::Membership(certificate));
                        // A certificate that trails its receipt can share a
                        // drain with the next receipt. The caller certifies
                        // after this poll returns, so the next receipt waits
                        // for the next poll.
                        break;
                    }
                    None => {}
                }
            }
            self.retry_owed()?;
            self.watch_standby()?;
            self.send_queued()?;
            self.actor_clock()?;
            Ok(events)
        })();
        self.latch(result)
    }
    /// Resend the waiting batch once [`RETRY_MIN_GAP_MS`] have passed since
    /// its stage or last resend at this frontier ([`Self::poll`]).
    fn retry_owed(&mut self) -> Result<()> {
        if self.pending.is_none() {
            return Ok(());
        }
        let current = self.actor_clock()?;
        let frontier = self.actor.primary().receipt().index;
        let due = match self.last_retry {
            Some((index, at)) if index == frontier => {
                current.saturating_sub(at) >= RETRY_MIN_GAP_MS
            }
            _ => true,
        };
        if due {
            self.guard_connection()?;
            self.actor.retry()?;
            self.retries += 1;
            self.last_retry = Some((frontier, current));
        }
        Ok(())
    }
    /// Read Fray's standby-progress watch ([`Self::poll`]): `Paused` is a
    /// lost standby, the soft pause [`Self::backup_lost`]; `Live` clears it.
    /// A runtime already latched is skipped: Fray's `status` reads `Paused`
    /// for a failed actor, which is its own fault, not the standby's, and the
    /// first failure stays the reason.
    fn watch_standby(&mut self) -> Result<()> {
        if self.failure.is_some() {
            return Ok(());
        }
        let current = self.actor_clock()?;
        self.backup_lost = self.actor.status(current)? == WatchStatus::Paused;
        Ok(())
    }
    pub fn into_recovery(self) -> Result<OriginalRecoveryLink> {
        // A real signed close is the sole authority for this handoff. A local
        // pause cannot manufacture a promotion or reset framing/journals.
        let channel = self.actor.into_recovery_channel()?;
        let mut link = OriginalRecoveryLink {
            network: self.network,
            channel,
            ingress: self.ingress,
            obsolete: self.obsolete,
        };
        // A fence learned outside poll still classifies queued frames.
        if !self.promoted {
            let session = link.channel.binding().session;
            link.obsolete.retain(session, &mut link.ingress)?;
        }
        Ok(link)
    }
}
