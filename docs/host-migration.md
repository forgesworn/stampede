# Host migration progress (Stampede)

Stampede follows Fray's step-3 host-migration protocol. This note records what
the Stampede consumer has, and what is still missing. It is not a design
document; the protocol lives with Fray.

## In place

- **Complete authority checkpoints** (`race-session/src/migration.rs`): World,
  bots, items, custom track, seats, input queues (`inq`, starved ticks, late
  presses) and pending results, with stable Fray seat bindings. Host-only
  simulation state that `race_core` keeps out of its serde form (each racer's
  Laser Trump victim mask, each frog's paused path time) travels beside the
  authority in the same image and is written back on restore.
- **Lobby bootstrap**: `LobbyOffer` / `ConsentedLobby` / `LobbyActivation` on
  the original, `LobbyStandby` / `ArmedLobbyStandby` on the standby, over the
  admitted QUIC pair link. A headless host has no player seat.
- **Owned original runtime** (`race-session/src/migration/live.rs`),
  `LivePrimary`, the counterpart of Twang's:
  - consumes the `PrimaryBootstrap` and owns the native authority, Fray's
    `PrimaryActor`, the pair link and its ingress;
  - stages each batch on a fresh private authority from verified player
    queues, and adopts it (and releases outputs and transport controls) only
    after the standby's matching commit receipt;
  - checks actual-UTC admission before every native attempt and operation:
    its own My Signet approval, the standby's approval on the pair link, and
    shared replay state;
  - a native callback error keeps its provenance (`Bounds` from a callback
    no longer shrinks the input prefix), gets one fresh retry from the same
    checkpoint and then pauses the runtime;
  - after the signed `Promote` it drops the pending batch, refuses staging,
    and counts term-0 frames as obsolete (`ObsoleteIngress`) instead of
    queuing them for recovery;
  - pair-link robustness in `poll`, in this order: a frame Fray refuses as
    `Stale` or `Gap` (an older certificate, a late Pulse, an expired partial
    transfer) is dropped and counted (`refused_frames()`), never latched;
    every other refusal latches, and so does a failed replay session. While a
    batch waits for its receipt the retry is owed by time: it is resent at
    most once per `RETRY_MIN_GAP_MS` (150 ms, under the 200 ms standby
    watch, so a lost receipt is retried before the room shows backup lost)
    of one frontier, counted from the stage, on any poll, refused frame or
    not (`retries()`). **Probing the standby is the driver's job**: it calls
    `LivePrimary::probe` every frame in every phase, before `poll` (whose
    send loop carries it; paced to one per 50 ms). The app's host and
    `HostSwitch` do. Fray defers its verdict after this runner's own stall
    only if a probe went out within 200 ms before the gap. A driver that
    forgets costs only a false soft pause, never a takeover. Then, after the
    frames read this poll,
    Fray's standby-progress watch (`PrimaryActor::status`) is read on every
    poll: while it reports `Paused` the standby is lost and `backup_lost()`
    holds ("Backup lost — the room is paused"). That is a **soft pause**, not
    a latch: it clears by itself at the first poll whose watch reads `Live`,
    and polling, probing, retrying and staging go on throughout, which is
    safe because nothing commits without the standby's receipt. Hard latches
    stay for genuine faults. The verdict is Fray's as it is, with no
    game-side tolerance and no "only while a batch waits" gate: telling an
    own stall from standby silence is Fray's watch's job ("a local stall is
    not peer silence"). Only a runtime already latched skips the watch (Fray
    reads `Paused` for a failed actor, its own fault), so the real reason is
    never overwritten. A missed Promotion shows as a pause that persists.
    Once `Promote` closed term 0 the pause is never reported: recovery owns
    the state (`backup_lost_is_never_reported_after_promote`).
  - **activity**, derived, never declared (there is no setter). The rule
    (decided): any phase of the committed authority whose world steps or
    whose timer runs counts as `Advancing`. So `activity()` is `Advancing`
    in `Racing` (the grid countdown and the race) and in `Results`, where
    the host still steps the world every tick and the rematch 3-2-1 runs.
    `LiveStandby::activity()` follows the same rule (`activity_of`).
    It is `Idle` in the lobby (no timer: an automatic start begins the race
    at once) and the switch window before the first armed commit (receipt
    index 0). It is
    handed to Fray (`PrimaryActor::set_activity`) on a change, after the
    adoption that changed it; Fray starts a fresh no-progress window on
    Idle→Advancing, so the first racing batch is not convicted. Idle on both
    sides lets the standby's idle pulses count as progress, so an idle lobby
    with probes never pauses; during play a pulse cannot mask a stalled
    commit.
  - **stage before send**: `stage` sends the staged candidate itself, so it
    is on the wire in the frame that staged it, not one host frame later at
    the next `poll`.
- **Owned standby runtime** (`race-session/src/migration/standby.rs`),
  `LiveStandby`, the counterpart of Twang's:
  - **probes inside its own `poll`** (paced to one per 50 ms, a `Live`
    original only), so a driver that forgets `probe()` cannot cause a false
    takeover: the original's answers keep the standby's watch fresh while
    nothing commits (`a_standby_whose_driver_never_probes_stays_live_and_does_not_promote`).
    An answer read after its probe's 200 ms window (an own stall on either
    side) is counted and ignored by Fray (`late_answers()`; Fray's
    uncommitted watch change), not refused as `Stale`, which the standby
    would take as a protocol rejection, marking the original `Rejected` and
    stopping replication
    (`late_probe_answers_after_an_original_stall_do_not_reject_the_original`).
    The standby's own `Stale` handling is unchanged: a candidate refused
    `Stale` (wrong index or parent: a faulty original) still marks it
    `Rejected`, so takeover stays possible.
  - signs its pulses with an activity derived from the checkpoint it
    installed last (`activity()`, the same rule as the original's); there
    is no setter.
  - consumes the `StandbyBootstrap` and the standby runtime identity, and owns
    Fray's `StandbyActor` (and so the sequencer), the armed certificate, the
    pair listener and channel, deferred ingress and bounded early roster
    events. Construction checks both runtimes' actual-UTC admission and
    restores the installed image once;
  - ACKs a replicated commit only after `validate_candidate` has freshly
    restored the complete native authority from it and Fray has installed the
    exact image;
  - classifies faults by origin. Only three original-caused conditions are
    non-latching, recorded in `OriginalLink`: transport loss (`Left`), lapsed
    admission (`Unauthorized`) and a protocol rejection of the original's
    frame (`Rejected`: a lossy pair frame or Fray's `StandbyFault::Frame`).
    Every standby-side fault latches and forbids promotion. Partial-transfer
    expiry stays non-latching, as Fray classifies it;
  - checks the original's admission before every listener poll, frame and
    send, never inside the restore callback. Stampede's listener kicks an
    expired peer and reports `Left`, so the check runs before each poll, and
    a `Left` read after UTC has moved past the second of that check is
    recorded as `Unauthorized`, since it may be an expiry kick;
  - `promote` needs the standby's own admission and either an expired watch
    or a `Left` original (an unauthorized original does not count). It never
    drains the network, hands the standby back on any refusal before the
    actor is consumed, and classifies queued frames with `ObsoleteIngress`;
  - `PromotedStandby` releases `Promote` only to a `Live`, admitted original.
    `StandbyRecoveryLink` carries early roster events in order. The
    original's `Left` is kept in link state only, never in game events.
- **Own seat on the shared sender.** The standby device's own human seat signs
  its player controls with the standby runtime key, the key of the pair
  link's receipts. `LiveStandby::sender_sequence` hands out the pair link's
  sender sequence, and the seat's endpoint is built with
  `PlayerEndpoint::new_with_sender` on it (Fray refuses the standby key in
  `PlayerEndpoint::new`). Its controls then interleave with the pair link's
  records at the original instead of reusing their sequence numbers, which
  the original's replay session refuses as a permanent `Conflict`.
- **Own-seat recovery** (`OwnSeat`, the counterpart of Twang's reviewed one).
  It owns the seat's actual `ClientSession` and its endpoint's delivery and
  intent journals, and checks that the client is the native seat the standby's
  player holds at C. `retain` reports the seat's real cut. `replay` applies
  its reliable replay records, after the same frontier check as Twang's
  (`check_recovery_record`), through the client's frozen
  `migration_recovery_step`, up to `MAX_BATCH` per transaction, then
  acknowledges them; the delivery journal skips duplicates and refuses gaps
  and conflicts. `seal` acknowledges the committed handled prefix, seals the
  whole intent tail and supplies every unhandled intent, so each drains once
  at frozen C. Lossy traffic has no recovery path. Input views in flight at C
  are never applied by recovery. The authority stays at C, so the client's
  predictions over those inputs are uncommitted and may change after play
  resumes.
- **Frozen client recovery step** (`ClientSession::migration_recovery_step`,
  in parity with Twang's). It takes the device's real local time and the
  frozen logical time C, and no input or operation. It stages, journals and
  adopts exactly as `migration_step` does, but runs no frame: no input
  generation, prediction, view or countdown advance, no clock-sync pings and
  no other periodic traffic, so it produces no datagram. Replayed reliable
  outputs apply as certified effects, and their reactive reliable followups
  are not suppressed: a replayed `TrackData` that completes a lobby download
  journals `TrackReady` in the same transaction. Such an intent extends the
  tail past the cut retained at C; Fray's `seal_inputs` accepts a grown tail
  in the next input round, and it drains like any other (not exercised in a
  lobby takeover yet). Replayed lossy events are ignored. Host events are
  stamped at C. A closed client stays closed: a replayed `RaceStart`,
  `Results` or `Closed` cannot revive `Closed("left")` or replace its
  reason. A replayed `Pong` is not a clock sample: its receipt includes the
  outage, so it touches neither the open round's samples nor the offset or
  RTT (conservative default). A clock-sync round left open
  finishes on its 2 s round timeout at a frame after recovery, with
  the samples it had, so ignoring the sample leaves the round state sound.
  Stampede's client has no security clock; the local time only keeps the
  step monotonic and anchors a race start replayed during recovery.
- **Own-seat `Leave` in flight at C** (decision of 2026-10-16). It is
  honoured: recovery replays it exactly once as a normal seat departure (a
  bot takes over the racer), and the promoted device keeps hosting without a
  player seat. The seat is not reclaimed. `ClientSession::leave` journals
  `ToHost::Leave` inside the client transaction and then closes the client
  (`Closed("left")`), so the `Leave` is the seat's last intent, as Fray
  requires of an input that removes its own seat. After it drains, Fray
  retires the seat's watermark and drops it from the survivors and
  histories; a duplicate or new supply for the seat is refused
  (`Unauthorized`). `OwnSeat::retired` then reports true, and
  `OwnSeat::replay` and `seal` become no-op `Ok`s (the replay reports
  `retired`), so a driver that keeps calling them while Fray returns to
  `Replay` after other departures does not wedge; the caller stops driving
  the seat.
- **Output to a player departing in the same batch** (decision of
  2026-10-17). It is kept. When a batch removes a seat (an ordinary `Leave`)
  and also addresses that player an output (a `Pong` for a `Ping` handled
  just before the `Leave`), the leaver stays a seatless Fray member for that
  one batch: its watermark is not retired, the membership revision does not
  move, and the output enters the replicated outbox like any other. The
  very next batch, whatever it carries, addresses it nothing, so it removes
  the member and retires its watermark (the recipient-retirement tombstone);
  Fray then prunes its outbox records and the certificate moves one
  revision. Nothing is dropped at staging and the room does not pause.
  Fray is unchanged. During the window nothing the leaver queued is fed (a
  `Hello` would re-admit it), the original refuses its reconnect challenge,
  a `Closed("left")` client offers no resume proof, and the standby refuses
  a seatless member that was not seated in the previous image or is not
  addressed by the batch, in either term; only a recovery's frozen drain
  records may carry it over, still seatless. A hostile original cannot seat
  the leaver again and undo its departure: the seat-transition check
  already refuses that (`Conflict`: a seat row for an old player the
  previous image did not seat). An explicit rule now states it as well,
  as defence in depth: every member seatless in the previous image must be
  gone from the candidate, seated or not (`Invalid("race seatless member
  re-seated")`, the drain records included). Stampede reads the seatless set
  from the images' own seat rows, which the validator checks against both
  restored authorities anyway, so the rule costs no extra restore and needs
  no cache (Twang caches it because its set comes from a restored runner).
  Test: `the_standby_refuses_a_candidate_that_seats_a_seatless_member_again`. A takeover inside the window never challenges
  the seatless member (the promoted standby closes its prepared link, even
  for a modified client that skips the library's guard): survivor drain
  records carry it over and the departure commit retires it with no native
  event. A batch that removes a seat and
  addresses the leaver nothing retires it at once, as before.
- **A departure changes the membership on the listener and the routes.**
  The commit that retires a player closes its queue at `publish` (Fray's
  `refresh` returns `Closed`): its route reads and frames nothing more, but
  its link is not closed there. On the certificate for that receipt
  (`RuntimeEvent::Membership`, revision + 1, one batch after a same-batch
  output), `PlayerHost::certify` installs it on the pinned listener with
  `update_session_membership`, which detaches the retired key's lingering
  link and refuses its later connections, and then removes every route
  whose player the certificate no longer lists under its key. That is the
  route-side counterpart of Fray's `PlayerInbox::update_membership`, which
  `PlayerQueue` does not expose. A certificate that changes nothing leaves
  the listener alone. Test:
  `a_membership_changing_departure_kicks_the_lingering_link_and_closes_the_route`.
- **A term-0 link drop parks the player; the silence rule decides**
  (decision of 2026-10-17). A dropped route parks for a reconnect, and the
  native authority is never fed its unarmed `Left` (which would remove the
  human at once in every phase). The native rule that turns a parked player
  into a departure while racing is Stampede's silence rule, which every
  armed batch runs: `drop_silent` removes a human that sent nothing for 6 s
  (`SILENCE_MS`) of game time while racing, and its racer becomes a bot.
  That departure commits like any other: the member retires, the revision
  moves, `certify` installs the certificate on the listener and the route
  is removed. It wins well before the 30 s parked departure below. Test:
  `a_parked_racer_silent_for_six_seconds_departs_by_the_native_silence_rule`
  (it also checks that the runner stamped the park and that the racer left
  the membership after about 1 s of runner time, well inside 30 s).
- **A player parked for longer than 30 s is departed by the host**
  (owner decision: "A player parked for longer than 30 s in an armed room
  is departed by the host, as an explicit host-side departure like a kick,
  in both games. This retires the player and its outbox before the outbox
  bound can pause the room."). At the end of each `PlayerHost::poll`, every
  network route whose queue no proven connection carries is reported to
  `LivePrimary::park`: one with no connection, or with a replacement
  connection that has not proved its journals (it may take the route in
  the same poll as the old link's `Left`, or after a host-side detach, so
  the route is never seen without a connection). The park is stamped on
  the runner's own `RunnerClock` (never a client's time) for a keyed member
  of the committed image; a repeated report keeps the first stamp. Only an
  attached connection (journals proved, `Resume::Attached`) cancels it
  (`LivePrimary::unpark`). The next `LivePrimary::stage` after the player
  has been parked for longer than `PARKED_DEPARTURE_MS` (30 000 ms), in any
  phase, kicks its native seat (`HostAuthority::kick`, the host's own
  explicit-removal path) before the caller's operation, and does not feed
  its queue (a queued `Hello` read after the kick would re-admit it and the
  new-admission gate would pause the room). The player is departed only
  when the kick will remove its seat (the committed authority holds it and
  is not frozen; the staged authority is restored from it); otherwise it
  stays parked, its queue is fed, and the next stage tries again. The kick addresses the player
  its `Closed("kicked")`, so the batch keeps it a seatless member, and the
  next batch retires it (revision + 1, outbox pruned, route removed by
  `certify`). A park ends with the membership. The original's own seat
  never parks (its route is local). The standby device's own seat is a
  remote human in term 0: parked for longer than 30 s, it departs like any
  other player (intended), and the device stays the standby. Test:
  `a_player_parked_past_thirty_seconds_departs_and_a_reconnect_before_cancels_it`
  (about 31 s of real time; no clock setter): B parks, then A, whose drop
  and replacement connection land in one poll and whose replacement never
  proves its journals; B resumes after under 1 s; A stays seated while
  parked for at most 30 s and
  departs in the first batch after (30 012 ms in the recorded run); B,
  parked earlier, is still seated and attached.
- **The successor applies the same 30 s rule.** After `Resume` a survivor
  whose link drops (it left, or was closed) and does not prove its journals
  again within `PARKED_DEPARTURE_MS` is kicked natively in the next batch
  `LiveSuccessor::stage` builds, before the caller's operation, with its
  queue unfed; the kick keeps it seatless for that batch and the next
  retires it and its outbox. Before this the native silence rule (racing
  only) was the successor's only departure for a lost link, so a ghost seat
  on the results screen or in the lobby blocked the rematch and the start,
  for good on a headless successor. The successor counts the 30 s in game
  time from the committed game time of the drop: after `Resume` its game
  clock is the device's own local clock. Test:
  `a_survivor_lost_in_the_results_departs_after_thirty_seconds_and_the_rematch_starts`
  (fast-forwarded game time).
- **A host kick in an armed batch commits as a departure.** In the lobby
  and while racing, `kick` in a batch's operation removes the seat; any
  native removal of a member's seat in a batch is a departure in Stampede's
  staging (the member is kept only if seated or addressed), so no fix was
  needed (Twang's returned `Conflict`). The kick's `Closed("kicked")` keeps
  the player seatless for that batch and the next retires it. Test:
  `a_host_kick_in_an_armed_batch_commits_as_a_departure`.
- **The hosting authority's own seat, checked** (session decision for
  Twang: its native removal returns `Conflict` and the room pauses, as a
  host cannot depart its own seat while it hosts). Stampede has no such
  gate: the phone host's kick of its own player in a term-0 armed batch
  commits like any departure (kept seatless for the batch, retired by the
  next; the in-process route is removed by `certify`), and the original
  keeps hosting with no seat, as the promoted standby does after its own
  seat's `Leave`. Open: whether Stampede should refuse it like Twang. The
  30 s parked departure cannot reach this seat. Test:
  `a_kick_of_the_hosts_own_seat_in_an_armed_batch_commits_and_the_original_hosts_on`.
- `committed_authority` restores the native authority from a committed image,
  for byte comparison.
- **Every lossy view.** Fray's player queue keeps a bounded, number-ordered
  list of signed lossy views per player (`MAX_LOSSY_VIEWS`, oldest dropped on
  overflow) and selection delivers each as its own `Lossy` event, so a burst of
  datagrams in one batch reaches the race exactly as on the plain host. The
  consumer needed no change.

- **Arming over the room's links** (slices 6.5 and 6.6,
  `race-session/src/migration/lobby.rs`). `HostArming` (host) and
  `ClientArming` (every client) carry the arming handshake as real
  `ToClient::Migration` / `ToHost::Migration` messages, from the host's choice
  of standby to an armed room. Both are poll-driven and report an
  `ArmingState`: `Idle`, `Asking`, `AwaitingConsent`, `Activating`, `Armed`,
  `Switching` (a client whose `Switch` verified), `Unprotected { reason }`,
  `Refused { reason }`.
  - **Refusals before anything is sent.** `HostArming::start` refuses with a
    visible reason when the room is not in the lobby, was opened with an
    online code (relays configured: v0.1 arming is LAN-only), has a Bluetooth
    player, has a player whose link is not `Lan` (`QuicHost::peer_path`), or
    the chosen backup is not a seated remote player. Stampede joins have a
    real path (Fray's `PathKind::Virtual` maps to `LinkPath::Bluetooth`), so
    the Bluetooth check is a real one, not a constant.
  - **Phase 1, lobby running.** The host sends `Ask` to the chosen device;
    the answer (`Willing` or `Decline`) arrives through the ordinary
    `HostAuthority::update` path (`take_migration_events`). The client
    surfaces `consent_requested()`; its person answers once with
    `answer(yes)`. A device that has not said yes ignores any offer.
    Phase 1 waits at most 60 s (`ANSWER_WINDOW_MS`): the host then ends
    `Refused` ("The backup did not answer within 60 s"), and the device
    gives up too (unanswered: "The backup did not answer within 60 s", and
    it declines; said yes but no offer came: "The host did not send the
    backup offer within 60 s").
  - **Attempt identity, by ordering.** `Ask` and `Willing` carry no attempt
    id (the wire is shared with Twang), so an answer is bound to its attempt
    by ordering on the link. `HostArming::start` discards every migration
    message the authority queued before its `Ask` (counted in `ignored()`):
    only a `Willing` that arrives after this attempt's `Ask` counts. On the
    device a new `Ask` starts over: a yes given to an earlier attempt does not
    carry over to the next offer. Residual: an answer still in flight when
    the next `Ask` goes out is read as the new attempt's. A stale `Willing`
    then gets an offer the device ignores (it has not said yes to this
    attempt), and phase 2 ends `Refused` at its 5 s window; a stale `Decline`
    refuses the new attempt. Neither arms without a yes.
  - **Phase 2, frozen.** On `Willing` the host freezes the authority
    (`HostAuthority::update` then reads, ticks and sends nothing, for the
    network and the host's own loopback player alike), runs
    `LobbyOffer::prepare` and sends `Offer { code, descriptor }`. While frozen
    the coordinator reads the room's `QuicHost` itself: seated players'
    `ToHost::Migration` parts go to its own reassemblers; every other event is
    held in arrival order. The authority checkpoint is byte-identical from
    the freeze to Armed.
  - **Freeze rules.**
    - Every host lobby control (`set_track`, `set_custom_track`, `set_laps`,
      `set_le_mans_start`, `set_bots`, `set_bot_level`, `set_battle`,
      `kick`, `start_race`, `to_lobby`) is refused while frozen: it returns
      `false` (`set_custom_track`: `Err`), changes nothing and sends
      nothing. `kick` and `to_lobby` now return `bool`.
    - Per-peer bound (64 records, 32 KiB): a peer's held events plus the
      migration parts it sends while frozen. A peer over it is disconnected
      (no ban) and its held events dropped; its `Left` is still held.
    - Global bound (`MAX_JOURNAL_RECORDS` / `MAX_JOURNAL_BYTES`): an event
      past it is still held and arming ends `Unprotected` ("Too much traffic
      while the lobby was held for arming"), so nothing is dropped silently.
    - A `Left` is always held, outside both bounds, and replays exactly once
      when the boundary ends.
    - A `Joined` held while frozen (any path, Bluetooth or not) ends arming
      at `finish`, `Unprotected` ("A player joined while arming"); the
      newcomer is seated natively once the lobby is released. The room stays
      open (`set_open(false)` would rotate the LAN token).
    - `cancel` releases a frozen lobby before Armed. After `Armed` it does
      nothing: the boundary is the caller's, ended by `take_armed` then
      `thaw_lobby` with its held events.
  - **Consent.** The willing device checks its own link is `Lan` and the
    offer's room code is its own, runs `consent_lobby_offer`, starts
    `host_standby` and replies `Consent { descriptor, addr, token }` without
    asking again. `addr` is the listener's LAN address, composed as Fray's
    LAN beacon does (the device's LAN IPv4 and the listener's port).
  - **Pair link, then activation.** Only the selected peer's `Consent`
    counts; any other peer's is ignored and counted, never fatal. The host
    checks the address is a private-network one, `accept`s the consent,
    dials the standby with `join_session_addr` and, once the pair link is
    admitted, creates the activation source (consent precedes activation).
  - **Capability ACKs.** The host sends `Setup { descriptor, initial }` to
    every remote human; a playing host signs its own seat's ACK itself. Each
    client verifies the descriptor (both signatures, its host, room code,
    app and versions, its own membership; the standby: the exact descriptor
    it consented to), signs `CapabilityAck` once, keeps the exact bytes and
    sends `Ack`. The host checks each ACK is for the sender's own player and
    passes it to `acknowledge`.
  - **Outcomes.** A bad ACK leaves the room `Unprotected` with a reason that
    names the client; an ACK read at or after the 5 s mark is a timeout
    naming the client ("No capability ACK from … within 5 s"), never
    "invalid". The 5 s consent/pair window, a decline, a failed pair link or
    a leaving player end in `Refused` (before activation) or `Unprotected`
    (after), and the boundary is released: the held events reach the
    authority at its next `update`, in order and before anything newer.
  - **Failure reasons** (the `reason` shown; `Refused` before activation,
    `Unprotected` from activation on):
    - before anything is sent: not in the lobby, already arming, room not
      open, online code, a Bluetooth player, a player not on the LAN, the
      backup not seated;
    - phase 1: the backup declined; the backup left; no answer within 60 s;
    - phase 2 before activation: no consent within 5 s; the device "could not
      prepare as the backup" (its `Decline`); an unreadable consent, an
      address outside the LAN, a consent that does not match the offer; the
      pair link could not start, failed, did not connect within 5 s, sent data
      early or was not admitted; a player left; the lobby could not be
      checkpointed or the offer prepared;
    - from activation: an invalid ACK or one for another player (naming the
      client); no ACK within 5 s (naming the clients); the pair link failed;
      the host's own ACK failed; a player left; too much traffic while held;
      the lobby changed while held; a player joined while arming; activation
      could not finish;
    - the app's `cancel`, with its own reason.
  - **Telling the clients.** Before `Setup` only the selected device has
    state, and only it is told (`Withdrawn` for `Refused`, `Unprotected`).
    From `Setup` on (including a failure inside `finish`, which keeps the
    stage until its last fallible step has passed) every remote human is
    told, and each resets its coordinator (drops its ACK), so a later attempt
    starts clean. `Armed` ends this slice: the host holds the `PrimaryBootstrap`
    (`take_armed`, with the held events and the host's own ACK bytes) and the
    lobby stays frozen for the switch slice; the standby holds its
    `StandbyBootstrap` (`take_standby`).

- **The armed bundle and the switch** (slices 6.7 and 6.8,
  `race-session/src/migration/switch.rs`). `HostSwitch` (host) and
  `ClientSwitch` (every client, from `ClientArming::take_switch`) take an
  armed room to protected play over the room's own messages. Both report a
  `SwitchState`: `Switching`, `Protected { unprotected }`,
  `Unprotected { reason }`.
  - **One standby listener.** The standby's control listener is the pair
    listener: `host_standby`'s `QuicHost` goes through `LobbyStandby`,
    `ArmedLobbyStandby` and `StandbyBootstrap` into `LiveStandby` (whose
    `listener()` is that handle; capacity 9, the pair link plus 8 controls).
    `ArmedHost` now keeps the address and token from the signed `Consent`
    (`standby`, `standby_token`) and the switch forwards them unchanged.
  - **Host.** `HostSwitch::start(host, network, armed, identity, clock, now)`
    builds the original's `LivePrimary` from the frozen lobby image (a
    restored copy with no transport: the armed initial image). It waits for
    cert0 on the pair link (the standby's index-0 membership certificate,
    `LivePrimary::membership_certificate`, at most 5 s), then sends every
    remote roster human `Armed { armed, certificate }`, `Standby { addr,
    token }` and `Switch { token }`, in that order on its ordered link. The
    `Switch` token is the ordinary listener's LAN token at the moment it is
    sent (`lan_token()`), which is not the one the clients joined with if the
    room was closed or reopened since: a lobby that fills to eight humans
    closes its listener natively, and each `set_open` change rotates the
    token. Each payload is checked to fit before anything is sent.
  - **Leaves, stragglers, pin.** The host reads the listener itself. It waits
    up to 5 s (`SWITCH_LEAVE_MS`) for the roster's ordinary links to leave.
    A straggler is sent `Unprotected { reason }` while its link exists, then
    closed. Any other peer that joins between `Armed` and the pin is closed
    as it arrives and never seated. Then `PlayerHost::new` pins the listener
    (`QuicHost::take_listener` moves it out; a closed shell stays behind). A
    refused pin is retried: Fray refuses while an ordinary link is still
    seated, and it reports that as `Unauthorized`. A handshake still pending
    on the unpinned listener (an early re-dial, or a bare QUIC connection from
    any LAN host) does not hold the pin off: Fray's pin closes it as a closed
    room and frees its slot.
    The room is not closed before the pin: `set_open(false)` would rotate
    the LAN token `Switch` just gave the clients to re-dial with.
  - **Routes and the missed-switch report (C7).** After the pin the host
    attaches routes as the clients re-dial, for up to 5 s
    (`SWITCH_ROUTE_MS`), or until every non-straggler is attached. It then
    reports `Protected { unprotected }`, which lists every roster human with
    no attached route: a straggler ("did not switch to the armed room within
    5 s") or a client that left but never reconnected. `take_room` hands over
    the `LivePrimary` and the `PlayerHost`, and the room proceeds.
  - **Parked at the pin (accepted).** When the pin succeeds, every remote
    roster human is parked (`LivePrimary::park`, stamped on the
    `RunnerClock` at pin time); none has a route yet. Attaching a route
    cancels the park, as a proved reconnect does. One that never attaches (a
    straggler, or a client that left the lobby link and never re-dialled) is
    departed by the existing 30 s rule (`PARKED_DEPARTURE_MS`): the native kick, then
    retirement in the next batch. The room never pauses for it.
  - **The host's own seat.** A playing phone host's own player is not a
    roster human: it gets no bundle and is never listed or parked. Once the
    switch is `Protected`, and before `take_room` (so before the first armed
    commit), `HostSwitch::attach_own(client, keys)` attaches it in process
    (`PlayerHost::attach_own`, with the ACK the host signed at arming and
    the armed initial record). At a takeover it dies with the device and its
    player departs.
  - **Client.** `ClientArming` keeps `Armed` and `Standby` (only after it
    acknowledged a setup). On `Switch` it verifies the bundle against its
    acknowledged setup before it closes anything. The checks: the descriptor
    it verified; its exact ACK (and that ACK's initial hash); `Armed` decoded
    under the root's context (standby-signed) and `verify_initial` against
    that hash; cert0 verified, at the same receipt, listing the armed roster
    exactly and this device's player and key. The standby address must be a
    private-network one. Only then does it close the ordinary link and report
    `ArmingState::Switching`. A refused bundle leaves the link open and the
    device `Unprotected`. The host then closes it as a straggler and lists
    it.
  - **Re-dial.** `take_switch(session, standby)` starts `ClientSwitch`. It
    re-dials the original's LAN address from its own original join
    (`ClientArming::rejoin(addr, token)`), unchanged, never an address the
    host names (no redirection). The admission token is the one from
    `Switch`, which it received over its admitted host link; the ordinary
    rejoin below tries it first, then the join's own token. A wrong `Switch`
    token
    makes the re-dial fail (an admission refusal is final), and the client
    then falls back as below; it never dials anywhere else. The re-dial uses
    the session-bound player role
    (`net::join_player_route`). The dial is retried by `net::PinnedJoin`,
    modelled on Twang's. The switch variant `PinnedJoin::until` also retries
    a handshake-stage refusal while the pin races: `Network` (an unpinned
    listener refuses a session-bound join with `BAD_HELLO`, which shows as
    `Network`) and `RoomClosed` (the pin closing a handshake it preempts). It is
    time-bounded: one deadline instead of six attempts, with the backoff
    capped at 400 ms. Identity, admission and version refusals stay final.
    `QuicClient::failed_join` exposes the failed stage and reason.
  - **The client's re-dial budget (accepted).** A client cannot see the pin,
    so its whole switch (the re-dial and the standby link) is bounded by
    `SWITCH_JOIN_MS`, 10 s from `Switch`: the host's leave window
    (`SWITCH_LEAVE_MS`, 5 s) plus its route window (`SWITCH_ROUTE_MS`, 5 s).
    Each connection attempt has its own 2 s deadline inside that budget.
  - **Route and standby link.** The route is `RemotePlayer::connect` with
    the exact ACK the device signed. The standby device uses
    `connect_with_sender` with its running `LiveStandby`'s sender (and the
    same armed record) and is protected by its role, without dialling a
    standby link. Every other client then prepares its `ControlPlayer` link
    (`net::join_standby_route`, `prepare_standby`). Any failed step reports
    `Unprotected { reason }`; `take_player` returns the route if one was
    built, and `take_client` returns the session if not.
  - **Before `Switch`, nothing is lost.** If the runtime cannot be built or
    cert0 does not arrive, the lobby is thawed with its held events, every
    remote roster human is told `Unprotected`, and the room plays on
    unarmed.
  - **After `Switch` and before the pin, a failure falls back to the
    unarmed lobby.** The held
    events are kept until the switch succeeds (only then are they discarded
    and counted by `HostSwitch::superseded`). While it switches, the host
    also holds each roster `Left` it reads and the roster's reliable traffic
    (within the boundary's bounds). If the switch fails after `Switch` went
    out and before the pin succeeded (the pin is still refused at the 10 s
    budget, the pair link drops, any terminal error), the host drops the
    armed runtime, thaws the lobby with
    every held event replayed in order, so the humans whose ordinary links
    left are departed (no ghost seats), and tells every roster human whose
    link is still up. The host reports `Unprotected { reason }` and the room
    plays on unarmed. A client whose re-dial fails or never completes waits
    `SWITCH_REJOIN_GRACE_MS` (1 s) past its 10 s budget, so a failed host
    has thawed, then rejoins the ordinary lobby at its own join address with
    the token from `Switch` (the listener's current one, so a token that
    rotated since the join, as in a full lobby, still admits it) and, if that
    join fails before it connects, once more at the same address with the
    token it joined with, so a wrong `Switch` token cannot strand it. A room
    that was closed at the switch (a full lobby) is reopened by the thaw's
    replayed `Left`s. For `SWITCH_TOKEN_HOLD_MS` (the 10 s budget plus the
    1 s grace) from the thaw, that reopen, and the close when the last
    rejoin fills the room again, use Fray's `set_open_keeping_token`, so the
    `Switch` token still admits every rejoin. The hold applies only in the
    lobby phase: the close when a race starts, and every open or close after
    the window, rotates the token again, as natively. Admission and the deny
    list still apply to every rejoin. It
    never dials any other address:
    `take_client` is then a fresh session that says `Hello` and is
    seated natively. Its state is `Unprotected` with "Rejoining the lobby
    unarmed".
  - **After the pin, nothing returns to the lobby.** Once `PlayerHost::new`
    has pinned the listener the armed room exists, so the switch always
    finishes `Protected` and `take_room` hands it over. A failure in the
    route window is the armed runtime's: a lost standby latches in
    `LivePrimary` (`failure()`), which pauses the armed room with that
    reason (standby loss pauses the armed room); `HostSwitch::route_failure`
    reports the first one. Roster humans not attached by the window's end
    are listed unprotected and parked, as always. The lobby authority stays
    frozen (superseded) and is dropped.
  - **Known gaps (v0.1).**
    - The host lists only missing routes, not prepared standby links. A
      client whose standby link failed knows it and reports itself
      unprotected, but the host lists it as protected. Closing this needs a
      new pair-link record from the standby (a client's prepared-link
      report), which v0.1 does not have.
    - A client whose re-dial failed while the host did pin falls back to an
      ordinary rejoin that the pinned listener refuses (`BAD_HELLO`); that
      device rejoins when the match ends.
    - The standby device is the exception: its role is fixed once armed.
      If its own re-dial fails while its pair link is still live on a fresh
      watch (the host pinned: a host whose switch failed has thawed and
      dropped its runtime a grace second earlier), the app keeps the
      standby running with no player (`Role::Standby`, shown as
      unprotected) and drops only the fallback rejoin. The host lists that
      player unprotected, parks it and departs it after 30 s; the room
      never pauses. If the original dies, the standby promotes as usual.
      A pair link still live on a stale watch at that moment is unknown,
      not a dead host: the device holds, neither promoting nor dropping,
      until the link goes (`Left`, or the 6 s idle timeout closes it), which
      drops the standby as above, or the watch is fresh again, which keeps
      it. Test:
      `a_standby_whose_watch_is_stale_when_its_redial_fails_holds_and_is_kept`.
      An outage of the backup over about 250 ms soft-pauses the host
      (behind, below; decided 2026-10-19), and the room plays on once its
      receipts come back.
      While its player is still a member at C, the takeover retains that
      seat from the session as it stood at the switch, with fresh journals
      (`ClientSwitch::take_seat`; its route never had any), at no clock
      correction, and the successor's first batch kicks it: the device
      hosts seatless. A standby whose original died before the switch
      finished is not covered: at the switch's end a dead host and a thawed
      one look alike, so that standby is dropped as before. Test:
      `a_standby_whose_own_redial_fails_after_the_pin_keeps_the_room_armed_and_takes_over`.
    - A client whose ordinary link dropped before the switch has no route.
      It rejoins when the match ends.
    - The ordinary events held at the frozen boundary are discarded at a
      successful switch (`HostSwitch::superseded` counts them): the armed
      initial image is the frozen one, and those events came on the links
      the switch replaces. So any lobby change made during the freeze is
      dropped, for example a `SetCharacter` or a ready toggle; the player
      makes it again in the armed room. (A roster `Left` during consent,
      pairing or activation ends arming instead, so none is held at a
      successful switch; test (g).) The frozen lobby authority is superseded too
      and stays frozen, so its `update` does nothing; drop it.
    - Game time pauses from the freeze to the first armed commit: the armed
      room resumes at the frozen image's (the armed initial) time. The app
      must resume its game clock at that armed initial time, not at the
      wall-clock time since the freeze: the clients' clock offsets, synced
      in the lobby, are stale after the freeze until their next clock round
      (for item 7).
    - A device that was not given `ClientArming::rejoin` refuses `Switch`
      and stays on its ordinary link (then a straggler).

## Tests (`race-session/tests/migration.rs`, actual QUIC pair link)

- `owned_primary_commits_race_ticks_like_the_plain_host_and_refuses_the_closed_term`:
  over 130+ commits from the lobby through the countdown into the race, every
  commit is published only after its receipt. The adopted authority equals
  both the standby's committed image and a plain (non-migration) host fed the
  same Fray-delivered datagrams (every lossy view, in number order), byte for
  byte. The plain host's outputs also equal the
  certified outputs. Covered: overlapping input windows, two datagrams in one
  batch, replayed and stale Fray lossy numbers, a starved stall followed by a
  burst of six datagrams in one batch whose oldest carries a late jump press
  outside the newest five-input window (the press mounts the Le Mans jockey
  only on the real stream, and every burst view is selected in order), and one
  transient native fault. Then the closed term: pending C+1 dropped,
  staging refused with no native call, and injected term-0 candidate and
  receipt frames refused.
- `owned_primary_native_fault_restores_once_then_pauses`
- `owned_primary_own_approval_expiry_suppresses_the_next_native_attempt`

- `owned_standby_replicates_the_race_and_refuses_the_closed_term`: both owned
  runtimes over the pair link, from the lobby through the countdown into 30
  racing batches (two frozen humans, six bots, a headless original with no
  seat). Each receipt is observed only after the standby installed it, and
  the standby's restored authority equals the original's at every commit.
  Then the closed term: the standby only queues the C+1 candidate, promotes,
  and refuses it with no restore. A delayed term-0 receipt at the original and
  a genuinely signed term-0 activation vote at the promoted standby have no
  effect, and `retained_bytes()` is 0. The early roster event is carried; the
  original's `Left` is in link state only.
- `standby_side_fault_latches_but_original_frame_rejection_does_not`: an
  undecodable control record from the original marks it `Rejected` without a
  latch, and promotion succeeds. A member's activation vote re-sent with a
  different hash poisons the replay session (an original-delivered replay
  conflict, an open question); the standby latches and promotion is refused
  with the standby handed back.
- `standby_own_approval_expiry_latches_and_refuses_promotion`: the standby's
  own approval lapses after four commits. It latches, never restores C+1, and
  refuses promotion even after the watch, handing itself back. The original,
  whose C+1 goes unanswered, soft-pauses as backup lost (`backup_lost()`,
  no latch) and never adopts it.
- `original_approval_expiry_stops_replication_but_standby_still_promotes`:
  the original's pair-link approval expires after four commits. The standby
  marks it `Unauthorized`, never restores its C+1, and does not latch. A
  promotion right after the lapse is refused with `Stale` (the standby kept
  probing, so the watch was fresh) and the standby handed back; it promotes
  after the watch. `Promote` is withheld from the expired original. The
  original may pause as backup lost meanwhile (its C+1 goes unanswered).
- `stale_pair_link_frames_are_dropped_and_the_original_still_commits`: on
  the real pair link, a late certificate for receipt 1, re-framed, is
  refused `Stale` and dropped (`refused_frames() == 1`), no latch; a
  two-chunk transfer left half-sent across a 600 ms poll hitch expires, and
  batches 3 and 4 still commit with their certificates. The late
  certificate uses transfer id 511, not one near `u64::MAX`: a completed id
  raises the receiver's high-water mark, and every live frame 512 or more
  below it would be ignored. (With Fray's `expire` fix the expiry refuses
  no other frame, so only the certificate is counted.)
- `a_receipt_dropped_with_no_later_refusal_is_retried`: batch 3's receipt
  and certificate are swallowed before the original's actor reads them; no
  frame is refused. The original resends batch 3 at 150 ms, the standby
  answers again, and it commits within about 200 ms of the stage with no
  soft pause (`retries() == 1`).
- `a_missed_promotion_pauses_the_original_as_backup_lost`: the standby
  promotes on its watch while the original is silent with batch 3 staged,
  and its Promotion is never sent. The original resumes and soft-pauses as
  backup lost about 200 ms later (Fray's local-stall deferral: its stall
  began within 200 ms of a probe), with no latch, and the pause persists
  over a further second of polls while the retries go on.
- `a_standby_hiccup_soft_pauses_and_the_room_resumes_by_itself`: racing,
  batch 3 staged, the standby not run for 300 ms. The original soft-pauses;
  when the standby runs again it installs batch 3, the receipt commits it,
  the pause clears by itself and batch 4 commits. Mutation: latching on the
  watch instead fails it.
- `an_idle_lobby_with_probes_never_pauses`: both runtimes `Idle` at index 0
  and after lobby commits; a second without commits, the original probing
  and the standby answering idle pulses, never pauses. Mutation: `Advancing`
  in the lobby fails it.
- `an_idle_to_race_transition_never_convicts_the_first_batch`: an idle
  lobby for 400 ms, then the race-start batch (both sides turn `Advancing`),
  then the first racing batch held 100 ms before the standby runs: no pause
  at any poll.
- `a_staged_candidate_is_on_the_wire_in_the_same_frame`: after `stage`,
  with no further original poll, the standby installs the batch. Mutation:
  leaving the send to the next `poll` fails it.

- `owned_standby_drains_own_human_seat_inflight_intent_once`: the standby
  device's own seat plays from the lobby into the race through its real
  client and a real `PlayerEndpoint` on the shared sender. Its committed
  outputs reach it over the player channel two behind; its input views,
  clock pings and `Applied` ACKs are signed controls, and every one is
  accepted (87 in the observed run, 4 of them ACKs). As on its real route, a
  frame that applied outputs sends one cumulative `Applied`, so each applied
  output is acknowledged once; each ACK commits with the next batch and
  prunes the outbox, and the committed watermark, the queued ACK and the
  delivery journal agree. Once its inputs have been committed for a while its intents
  are withheld; C is the first commit after which it holds two withheld clock
  pings, and the original has not answered them. Its next input view is in
  flight in the refused C+1. After promotion the seat recovers locally
  through the frozen step while the device's local time runs on 2.86 s past
  its last frame (more than a clock-sync period; the recovery deadline runs on
  a separate synthetic clock). Replayed `Pong`s reach the client, yet it sends
  no datagram and, in this racing takeover, journals no intent (checked after
  every replay). Its whole clock-sync state (offset, RTT and the open round's
  samples), prediction and view stay exactly as at its last frame before
  recovery, and the sealed tail
  is the cut retained at C. A genuine delivery gap pauses; then every replay is delivered twice and every
  intent supplied twice, with no effect the second time. The tracked ping
  drains once with its original bytes, and the native host answers it with
  exactly one `Pong`, stamped at C. Every drained ping has exactly one `Pong`.
  The world tick and the own racer are exactly as committed at C (the
  in-flight input view was not applied), and game time never moves. The
  absent other human departs and a bot takes over its racer.
- `owned_standby_missing_own_seat_intent_stays_paused_past_recovery_deadline`:
  the same takeover, but the seat's first unhandled intent is sealed and never
  supplied. Until the 5000 ms recovery deadline every step is refused with
  `Gap`, no later intent drains, no departure runs, both seats stay and
  committed state is byte-identical. At the deadline recovery fails with
  `Invalid("recovery deadline")` and refuses everything after, including a
  late supply of the missing intent.
- `owned_standby_honours_own_seat_leave_in_flight_at_takeover`: the same
  takeover, but after C+1 the own player quits through the client's real
  `leave`, inside a client transaction: `ToHost::Leave` is journaled, never
  sent, and unhandled at C. The other human resumes as a remote survivor
  (signed resume proof and input-round cuts over a synthetic bound
  connection). Recovery drains the seat's pings, each answered once, then its
  `Leave` exactly once: the native host runs its departure, a bot takes over
  the own racer, `human_count` drops from 2 to 1, the racer count is
  unchanged and the seat's watermark is retired. A duplicate supply and a
  new intent of the seat are refused with `Unauthorized` and change nothing.
  The retired seat's later `seal` and `replay` are no-op `Ok`s. At `Ready`
  the seat is gone from the membership and is not reclaimed, the other human
  stays, the own client reports `Closed("left")`, and one native tick runs on
  the restored host, still racing.
- `owned_standby_own_seat_leave_with_absent_human_reenters_replay_and_empties_room`:
  the same Leave with the other human absent. After the Leave drains, the
  other human's departure sends Fray back to `Replay`; the driver keeps
  calling the retired seat, gets no-op `Ok`s, and recovery reaches `Ready`.
  No human is left. The promoted host is headless (restored from the image),
  so its room keeps racing with bots through a native tick.
- `standby_own_seat_controls_share_the_pair_link_sender`: the regression for
  the earlier `Conflict`. The own seat's views through the shared-sender
  endpoint are all accepted while both runtimes commit. A control numbered
  from a separate per-player counter reuses a sequence the original holds from
  the pair link, is refused with `Conflict`, and fails the original's replay
  session.

The player channels in the tests above are synthetic bound channels. In the
owned-primary tests the standby is a test-driven `StandbyActor`. The watch
expiry is a synthetic sample.

### Real player transport

Game protocol 11 (was 10) marks armed rooms. `PlayerHost` and `RemotePlayer`
(`race-session/src/migration/players.rs`) carry remote humans over their real
QUIC connections:

- **Original.** After arming, every ordinary lobby connection is closed and
  the room's own listener is pinned to the session (`pin_session`) with the
  armed membership certificate. Remote humans reconnect with the session-bound
  `Player`/`Original` role. The unchanged persona check in `QuicHost` admits
  the connection; `QuicHost::migration_channel` gives its bound channel and
  `LivePrimary::attach_player` its `PlayerQueue` (one initial route per
  player). Physical frames pass a per-player `ReceiveQueue`, the channel and
  the queue's signature and replay checks. A refused frame is dropped and
  counted, never queued. Raw datagrams are ignored. Staging uses every
  attached queue and real RTTs under the native logical peers. Outputs are
  framed to their recipients only from `RuntimeEvent::Committed`, as
  certified outputs, never from `HostAuthority`. `publish` refuses, before
  anything is sent, any output whose receipt is in another term or ahead of
  the receipt this original installed.
- **What pauses the room.** Malformed, unsigned, wrong-session, oversized and
  replayed frames never do. A validly signed original that reuses a retained
  intent ID with different bytes does: Fray faults the session
  (`PlayerQueue::receive` → `Conflict` → `replay.fail()`), as the spec
  requires ("conflicting bytes for a retained ID fault the session"). That
  connection is also banned. So one admitted member can halt the room by
  signing a conflicting original; this is open for the design session.
- **Bans and disconnects.** `kick` (which bans the runtime key for the room's
  life) is used only for that signed `Conflict`. Every other refusal uses
  `disconnect`, which does not ban: a refused or second connection, a send
  backpressure failure (`Channel::enqueue` or `poll_send`), a closed queue, a
  retired member, and a failed resume. The queue stays attached, so the
  player can reconnect while armed.
- **Second connection.** A second connection with a routed player's key never
  evicts the live one. Fray's pinned listener refuses a concurrent connection
  with the same key; if one were admitted, `PlayerHost` disconnects the
  newcomer without a ban.
- **Reconnect.** When a player's connection has left, a new admitted
  connection with the same key is challenged: `LivePrimary::begin_player_reconnect`
  (Fray `PlayerQueue::begin_reconnect`, signed by the original) sends a
  `ReconnectChallenge` bound to the new TLS exporter and to the standby's
  membership certificate at the installed receipt. The caller supplies that
  certificate after each commit with `PlayerHost::certify` (the test takes
  it from the owned standby; how the product original gets it is open).
  `certify` also sends that certificate to every attached player the
  commit framed no output to (the countdown, a quiet spell), as committed
  progress for its loss detection (see Clients below).
  `RemotePlayer::reconnect` reads the challenge and calls
  `PlayerEndpoint::reconnect`, which proves the client's retained journals.
  `PlayerQueue::finish_reconnect` accepts the proof; the original then
  re-frames every retained reliable output above the player's ACK. Pending
  intents above the committed watermark are sent again under their original
  IDs. Nothing is framed to the new connection before the proof.
- **Capacity.** A verified control that the full queue refuses (`Bounds`:
  512 pending originals or 256 KiB) has already been consumed by the channel,
  so it cannot be read again. Fray's contract says such a refusal consumes
  neither the original ID nor the control cursor, so `PlayerHost` parks the
  control on its route and stops reading that route. The parked control is
  retried first on every poll, and accepted once a commit frees capacity.
  Parking keeps intake ordered on the same connection, where a disconnect
  would cost a reconnect round trip. The cost: that player's later frames
  (views and ACKs too) wait in its bounded receive queue until the next
  commit.
- **Admission on every frame and output.** `QuicHost` closes a connection
  whose My Signet approval has expired before it reads that connection's
  frames, so none reaches a queue. `publish` re-checks the recipient's
  approval for every output and frames nothing to an expired one. The
  original's and the standby's runtime keys never get a remote player route:
  the standby device's seat rides the pair link's shared sender.
- **Rate limits and backpressure.** A frame refused for the ingress rate
  limit stays queued (Fray `ReceiveQueue::poll`) and is read on a later poll,
  on both sides, so a backlog delivered at once after a stall is delayed, not
  lost. If a route's receive queue is full, the connection is closed without
  a ban and the player resumes. The client holds a wire its full receive
  queue refused, and stops reading its transport until the wire fits. When
  `publish` cannot queue an output, nothing later is framed to that player
  (so the connection has no gap) and the connection is closed without a ban.
  Reliable outputs stay in the outbox and are re-framed on resume.
- **Client.** `RemotePlayer` runs `PlayerEndpoint` on the client's admitted
  `QuicClient`. Received certified outputs go through the client's
  transactional `migration_step` (duplicates are no-ops); each frame journals
  its reliable intents before sending them, sends its input datagrams as
  signed views, and ACKs newly applied outputs with `Applied`. A frame runs
  one client step per received output and then its own, and each step's
  view replaces the last one's events, so the frame collects the events of
  every step and leaves them all, in order, on the view the app reads (none
  on a frame that ran no step, nor while recovering). The successor's own
  seat does the same. Before this an armed client's host events (lap
  banners, its own `Finished`, `RaceOver`) were overwritten before the app
  read them, so its race screen never left for the results. Tests:
  `an_armed_client_surfaces_its_finish_and_the_race_over_once` (term 0) and
  `remote_survivors_resume_on_the_promoted_standby_at_frozen_time` (after
  `Resume`, survivors and the own seat). A frame which fails part-way
  leaves the events of the steps it did commit on the view (a committed
  output is not replayed, so they would never come back), and a frame which
  fails at once on an earlier latched failure leaves none. The app reads the
  view once per pump, so a pump which runs no frame for the seat (its link was
  lost and the standby is watched, or the player left) first drops the events
  the last pump read (`RemotePlayer::discard_view_events`). Every frame's
  events are then read by at most one pump; a recovering client's step drops
  them at its entry as well. Tests:
  `a_frame_failing_part_way_keeps_the_events_of_its_committed_steps`,
  `a_frame_after_a_latched_failure_leaves_no_events_to_read_again` and
  `a_pump_that_runs_no_frame_leaves_no_events_to_read_again`.

**Armed player frames ride the reliable stream.** Fray's migration `Channel`
has no datagram path (its framing carries session, term, sequence and
fragment, with no lossy delivery class), so input views and snapshot outputs
travel as reliable frames. Native semantics survive: each view still carries
the 5-input window, the queue keeps up to `MAX_LOSSY_VIEWS` views, and late
presses are recovered within 32 ticks. The independent review measured the
costs against plain Stampede:

- Head-of-line delay under loss. One lost QUIC packet holds back every later
  view, intent and ACK on that player's stream until it is retransmitted
  (about 1 to 1.5 RTT, or a PTO for a tail loss). At 80 ms RTT and 2% loss
  the host starves about 6 to 12 ticks per loss event and feeds the last
  input, then the stalled views drain in a burst. A jump press is lost only
  if the stall exceeds 32 ticks (a PTO or backoff on a link of 150 ms or
  slower), where plain Stampede would have kept it.
- Queue overflow when the host stalls. `QuicHost` holds 256 reliable
  messages per peer and disconnects the peer on the next one; an armed client
  sends about 60 view controls a second, so a host stall of about 2.5 s
  disconnects every remote player (plain Stampede drops datagrams instead).
  The progress watch normally pre-empts this by failing over to the standby
  first. The disconnect no longer bans, so the player can reconnect and
  resume. On the client side, 1024 queued messages are about 10 to 15 s of
  snapshots.

The options for the design session are a lossy framing in Fray for views and
snapshots, or coalescing several views into one control.

Tests (`tests/migration.rs`, real localhost QUIC):

- `remote_humans_play_an_armed_race_over_real_player_links`: two remote humans
  (with the frozen standby seat and four bots) play from the lobby into the
  race for 240 commits through the owned `LivePrimary` and `LiveStandby`.
  Native state equals the standby's at every commit. Each remote human's
  views drive its own native racer: the racer's last input has that client's
  steer and full throttle, the racer moves, and a jump press reaches it; the
  frozen standby seat gets no throttle. Every journaled intent (clock pings)
  is handled once, with exactly one native `Pong` each; every certified
  reliable output is applied once; the clients' `Applied` ACKs are committed
  and prune the original's outbox. An already ACKed output framed again is a
  no-op, and a physical intent frame sent again on the same connection is
  consumed by the channel's replay guard with no effect.
- `hostile_and_unadmitted_player_peers_never_reach_the_queue_or_pause_the_room`:
  an admitted member's connection sends another session's frame, a control
  transfer over the 4096-byte cap, an unsigned control on its own bound
  channel, a message over the 1024-byte cap (dropped by QUIC) and a raw
  datagram. A stranger with a valid approval tries a session-bound and an
  ordinary join. Nothing reaches a queue, both strangers are refused, and
  the room keeps committing unpaused.
- `old_version_client_is_refused_and_a_plain_match_runs_at_the_new_version`:
  a `Hello` with each older protocol (10, 9, 8, 7, 6 and 5) gets
  `Closed("version mismatch (host 11, you N)")` and no seat; a plain room
  still races at protocol 11 over QUIC.
- `publish_refuses_an_output_whose_receipt_is_not_installed`: an output whose
  proof verifies under a receipt signed with the standby key, one index ahead
  of the installed receipt or in term 1, is refused with `Stale`, also when
  framed next to an installed output; nothing reaches the client.
- `a_dropped_player_resumes_and_a_second_connection_evicts_nobody`: a second
  session-bound connection with a routed player's key ends without
  disturbing the live connection, whose views keep driving its racer, and
  bans nothing. That player's connection then drops, the room keeps
  committing, the same key reconnects, is challenged and resumes. Every
  journaled intent is handled once and the committed ACK equals the
  client's.
- `a_full_player_queue_parks_the_control_and_loses_no_intent`: 552 signed
  originals arrive with no commit. The queue holds 512, the 513th is parked
  (not refused), and later commits apply all 552 with no gap, no refused
  frame and no disconnect.
- `a_burst_over_the_ingress_rate_limit_is_delayed_not_lost`: 200 wires
  (about 180 KiB) sent at once hit the original's rate limit and are all
  applied, and a client that stalled for 150 commits reads its backlog over
  several rate-limited frames. Nothing is lost and no connection closes.
- `a_send_overflow_closes_the_connection_visibly_and_the_player_resumes`:
  6000 outputs framed to one player in one publish. About half are queued,
  the connection closes without a ban, and the player resumes with its
  intents and ACKs intact.
- `an_expired_approval_gets_no_output_and_no_frame_reaches_the_queue`: after
  a member's approval expires, `publish` frames nothing to it, and its later
  original never reaches a queue.
- `a_view_for_another_race_never_drives_the_native_racer`: a signed view for
  another race number never steers the member's racer; the same view for
  the current race does.
- `the_standby_runtime_key_never_gets_a_remote_player_route`: a session-bound
  connection with the standby runtime's key is refused, and the room keeps
  its three routes.

### Remote survivors resume on the promoted standby

Every remote human prepares a `ControlPlayer` link to the standby's passive
listener while the room is armed (`RemotePlayer::prepare_standby`). It stays
idle. After promotion, `SurvivorLinks::open` takes the promoted standby's
link apart, opens each carried roster link and announces the signed
`Promote` on it. The client reads it (`watch_standby`) and moves its same
`PlayerEndpoint` and the prepared link into Fray's `PlayerRecovery`
(`RemotePlayer::recover`). Fray only runs player recovery over a link bound
to the standby, so survivors do not recover over a fresh join.

`SurvivorLinks::drive` runs takeover steps 2 to 5 at frozen C. Its own seat
is an `Option<&mut OwnSeat>`. While the standby's player is still a member at
C, seated or seatless, the seat is local to the device (it keeps its client
and journals after its own `Leave`; the app's background standby keeps them
for it), so `drive` without its `OwnSeat` is refused before recovery moves
(`Invalid("own seat is a member at C …")`): Fray's `Recovery::seal` needs a
cut for every unretired member keyed to the standby. A seat C kept seatless
(its departure batch addressed it an output) is retained, replayed and sealed
like any survivor (`OwnSeat::new` accepts it with no native seat), and the
successor's first live batch feeds nothing from it, retires it and drops the
seat; the device hosts on seatless. A seatless takeover (`None`) is allowed
only when the seat was retired before C (Twang's rule). Tests:
`a_standby_whose_player_left_in_its_window_promotes_with_its_seat_and_the_successor_retires_it`
and `a_standby_whose_seat_departed_before_c_takes_over_seatless_and_survivors_resume`.
The steps:
- a resume challenge on each link, bound to that connection's id and TLS
  exporter; a proof is offered only for the connection it arrived on;
- the own seat's cut, cohort seal (all answered, or 250 ms);
- the security boundary, replay of committed reliable outputs (the client
  applies each once through its frozen step), `Applied` ACKs;
- sealed input rounds and drains in Fray's order; absent humans depart (a bot
  takes over) only after the survivors drain.

Then `barrier` sends each survivor its final cut. On it the client computes
its correction (its host-time estimate at its last stepped frame, as that
frame recorded it, minus C), confirms it, and rebases: host time at its current local time reads C, the open
clock-sync round is aborted and the race frame clock restarts (no catch-up).
`OwnSeat::ready` does the same for the standby's own seat
(`ready_at_host` with the host time its last stepped frame recorded). A correction over
250 ms is refused by Fray before the client is touched. `resume` signs and
sends `Resume`. Each survivor (and `OwnSeat::resumed`) re-anchors host time C
to the local time it receives `Resume`, since the barrier may have waited for
other survivors after its cut: it continues from C, with no burst of future
inputs. The same code is served again by `start_resume_host`; that listener
stays closed (ordinary joins and strangers refused).

No single link can fail recovery: a frame a link's ordered ingress refuses
(empty, oversized or over its bounds) closes that link without a ban, as
`PlayerHost` does. A sealed survivor whose link closes fails at the deadline.

Tests: `remote_survivors_resume_on_the_promoted_standby_at_frozen_time`,
`a_lagging_ready_does_not_run_the_other_survivors_clock_past_c` (B's ready
1.5 s after A's; a late absent client's empty frame mid-recovery) and
`a_survivor_missing_its_sealed_record_keeps_recovery_paused_until_the_deadline`.
The original stops without closing anything; the standby promotes on its real
200 ms watch, and each survivor frames on until `Promote` or its own loss
detection freezes it (below). With the 100 ms client freeze (decided
2026-10-17) the measured corrections are 66-136 ms against the 250 ms cap (the
smallest margin is 114 ms), because each client freezes before the standby's
200 ms watch promotes.

### The successor plays on, unprotected

`SurvivorLinks::barrier` keeps a `PlayerInbox` per accepted link at its final
cut; after `Resume`, `SurvivorLinks::into_successor` attaches each link with
Fray's one-use `SuccessorActor::attach_warm_player` (bound to that cut's TLS
exporter) and returns `LiveSuccessor`, the single authority with no standby:
- `stage` stages a private batch over the survivors' queues and the own seat
  and prepares it; nothing is visible or sent.
- `commit` runs Fray's `commit` (fresh native restore validation through
  `validate_candidate`, then the signed install), adopts exactly that
  authority, and only then publishes: each certified output (its receipt the
  installed one) is framed to its survivor's recovered link or queued for the
  own seat. `Applied` ACKs advance the watermarks and prune the outbox.
- The own seat (the promoted phone keeps its seat) is local: its journaled
  intents and input views feed the batch game-side (`OwnInputs`; Fray's
  `LocalInputs` covers only keyless members), recorded in its committed
  history; its delivery watermark advances from the outputs its client
  applied through its delivery journal. Its first frame after `Resume`
  resends its unconfirmed input window, like a remote survivor's; an inbox
  record leaves the inbox only after the client step that applied it
  succeeded, and its views are bounded like Fray's queue (`MAX_LOSSY_VIEWS`).
  This route is a candidate Fray `LocalInputs` extension (the standby-keyed
  member on `SuccessorActor`, with a local delivery-ACK hook).
- `poll` refuses every fresh connection on the standby listener (no ban).
  On the resume host only a survivor whose link dropped, on the
  session-bound `Player` role, is challenged: Fray's `begin_reconnect`
  with the successor's membership certificate, then `finish_reconnect` on
  its proof of the retained journals, then the unACKed outbox is replayed.
  A second connection never evicts a live one; strangers, the absent
  roster key and plain joins stay refused. Only a link that sends a refused
  frame is dropped.
- Idle liveness: a survivor that a commit sends no output (an idle lobby or
  results screen) gets the successor's signed membership certificate at the
  new receipt. Fray's term-1 pulse responder is not reachable from
  `SuccessorActor` (no `Pulse::respond` path), so this is committed-progress
  evidence rather than a challenge echo: a client counts it only for a
  receipt newer than any it saw whose game time advanced, so neither a
  replay nor a stalled game clock refreshes anything. The successor driver
  must commit at least every 50 ms (`MAX_BATCH_GAME_MS`, half of the 100 ms
  `FREEZE_AFTER_MS`), and each batch spans at most 50 ms of game time (the
  spec's batch cadence, checked when staging). An idle lobby therefore
  commits every frame at a 30 Hz driver (33 ms); a 100 ms idle cadence would
  need batches longer than 50 ms, which the spec forbids, and would freeze
  every client.
- Rematch follows the native flow (the spec keeps ready/rematch permissions
  and the session armed across rematches): when every human readies on the
  results screen the 3-2-1 starts the session's next match with the same
  seats, still unprotected.
- `protected()` is false. There is no promotion API and no second takeover.

Clients:
- `FREEZE_AFTER_MS` (100, decided 2026-10-17; deliberately shorter than the
  standby's 200 ms term-close timeout, so a survivor's lead past C stays
  inside the 250 ms correction cap): a client in a race or on its results that sees no
  fresh committed progress for that long runs no client step (no
  simulation, prediction, input or timers) while network work continues;
  progress unfreezes it. Fresh committed progress is the spec's rule, as in
  Fray's `ProgressWatch`: a receipt newer than any seen whose game time
  advanced, carried by an output or, on a commit with no output to this
  player, by the signed membership certificate (the standby's in term 0,
  the successor's after `Resume`). A newer receipt at the same game time is
  not progress. Under term 0 a lobby never freezes (the standby covers it);
  under the successor every state is watched.
- `MAX_LEAD_MS` (200, decided 2026-10-19): under term 0 a client bounds
  its host-time estimate to the commits. Before every frame it steps
  (lobby, race and results alike) it keeps that estimate within
  `[newest, newest + 200 ms]`, where newest is the game time of the newest
  progress it saw, by moving its clock offset. The first pull-down of an
  episode restarts its clock-sync round (an open round is aborted, since
  its samples may straddle a held host clock); the round then runs in the
  background and the client never freezes for it. The episode ends on a
  stepped frame inside the bound with no round open, so a bound that flaps
  never aborts the round it asked for. A cut is the last stepped frame,
  for remote survivors and for the standby device's own seat alike, at the
  host time that frame recorded (`last_frame_host`): a sync round that
  completes later, while the client is frozen (its pongs ride commits at a
  stalled game time), does not move it. C
  is at least the newest progress it saw, so every takeover correction is
  at most 200 ms whatever the original's clock did. While commits flow the
  client steps on, so its cut moves on with C and the correction stays
  above minus the commits still in flight to it. Normal play leads by the
  commit round trip plus one way, well under the bound. A held host
  (the soft behind pause) moves the clients' clocks with it: their game
  time stands while commits do not move, then follows the commits.
  An earlier version froze a client while it led by 200 ms or more until
  its resync completed; an independent review found that a cut taken in
  that freeze stood still while C moved on, so a takeover 100-250 ms after
  play resumed corrected by about -300 ms and was refused. In a term-0
  lobby (never frozen) a host death or hold let the clients run on past
  the cap (about +550 ms after a 450 ms stall); the bound covers that too.
  A steady commit cycle over 50 ms leaves clients at the bound: such a room
  runs slow, but every takeover stays inside the cap.
- After `Resume` the player is unprotected: `prepare_standby` is refused
  (`SECOND_TAKEOVER_REFUSED`), and a client frozen `SESSION_LOST_AFTER_MS`
  (5000, the spec's recovery timeout) under the successor ends the session.
- The first frame after `Resume` resends the unconfirmed input window
  (`migration_resend_inputs`: ring entries above the newest acknowledged
  number, at most 16 packets); the successor treats numbers it already
  applied as late copies.

Tests: `remote_survivors_resume_on_the_promoted_standby_at_frozen_time` now
plays on to the race results (the humans driven by the native bot as an
autopilot), then one rematch, then an idle lobby, and checks exactly-once
delivery, drained intents, a pruned outbox, the results each client saw
against the world, the same code and session (the rematch is the next
match), stable seats, no starvation on the successor after `Resume` (per
seat), a commit gate (no client applies an output whose receipt is past the
last returned commit), an own-seat step failure that loses no record, a
survivor reconnect after `Resume`, fresh joins after `Resume`, an idle
lobby that does not freeze until the successor hangs there, and no false
freeze in any state of that healthy play. The standby device's own seat
plays in term 0 through the original (its views and intents committed over
the own-seat endpoint) and frames on until its freeze, so it holds
unconfirmed inputs at C; its resend after `Resume` leaves it unstarved.
`remote_humans_play_an_armed_race_over_real_player_links` checks no false
freeze across an armed lobby, the countdown and the race in term 0, and
`a_stalled_game_clock_freezes_clients_although_commits_arrive` shows that
commits at a stalled game time (outputs and certificates still arriving)
freeze clients at the constant.
`a_second_host_failure_is_not_taken_over_and_ends_the_session` stops the
successor mid-race: no `Promote`, clients freeze at the constant and end
after the timeout, and a second standby link is refused.

### Roster traffic never latches the armed standby

Listener events from peers other than the pair link are held with a per-peer
budget (`PEER_PENDING_RECORDS`, `PEER_PENDING_BYTES`). Overflow drops that
peer's held events and closes its link (no ban); it never fails the standby,
so the original keeps committing. A `Left` drops a peer's held events with its
held `Joined`, so a flapping link holds nothing. Test:
`a_flood_on_one_prepared_link_closes_only_that_link`.

### Local seats on real player paths, and slow frames

- Item A: the standby device's own player is admitted on an ordinary
  `Player`→`Original` route, as Twang does. Its endpoint signs from the
  standby's pair-link sender (`RemotePlayer::connect_with_sender`, Fray's
  `PlayerEndpoint::new_with_sender`); a standby-keyed endpoint on a separate
  counter is still refused, and the host key still gets no network route.
  At takeover `OwnSeat` takes its journals from that real endpoint
  (`RemotePlayer::into_parts`).
- Item B: a playing phone host's own player runs in process through Fray's
  real player path: `PlayerHost::attach_own` builds the endpoint on the
  original's own sender plus `LivePrimary::attach_player`, over an
  in-memory channel. It is accepted only for the original's key; its intents
  and views are journaled and committed like a remote player's, its outputs
  apply once and its `Applied` ACKs prune its outbox.
- `both_own_seats_play_an_armed_match_on_real_routes_through_slow_frames`:
  both own seats, two remote humans and four bots play an armed race;
  exactly-once intents (each ping one pong) and outputs, outbox pruning, and
  native state equal to the standby's at every commit.
- Slow frames: a batch spans at most 50 ms of game time. `LivePrimary::stage`
  and `LiveSuccessor::stage` stage `min(now, last commit + 50)`, so a slow
  frame's elapsed time is split into back-to-back batches, each a full stage
  and commit, staged by the driver after each commit until the receipt
  reaches its clock. Under the 250 ms skew budget nothing is clamped.
  Before, one 80-150 ms hitch latched `Bounds` and paused the room for good.
- **The skew budget is a soft pause on the original (decided 2026-10-19).**
  The original commits at most 50 ms of game time per candidate-to-receipt
  round trip (one batch in flight), so one round trip over about 250 ms (a
  Wi-Fi stall on the pair link, or a local stall) used to latch `game clock
  skew over 250 ms` and end the room; on phone Wi-Fi that is routine.
  Now the driver paces its game clock every frame, before anything uses it:
  `LivePrimary::pace(now)` returns `now` capped at 250 ms past the last
  commit, and the driver adds what it held back to its session-time shift
  (as its hold across the switch does). Game time therefore stops at the
  budget while commits are late, moves only as commits move the cap, and
  runs on continuously once they catch up: it never runs more than 250 ms
  ahead of the commits and never jumps. A capped clock whose game time has
  also stopped (no commit moved it on for `FREEZE_AFTER_MS`, the clients'
  own freeze rule) is the soft pause `LivePrimary::behind()`, modelled on
  `backup_lost()`: no latch; polling, probing, retrying and staging go on
  (each batch at most 50 ms, nothing commits without the standby's
  receipt). It clears by itself once the lag is down to 200 ms, or once
  commits have moved game time on without such a stop for
  `BEHIND_CLEAR_MS` (250 ms). A steady commit cycle over 50 ms keeps the
  clock at the budget for good while the game runs on at 50/cycle of real
  speed; that is a slow room, not a paused one, so the pause clears
  (`a_steady_60_ms_commit_cycle_clears_the_behind_pause`). Making such a
  room run at real speed needs more than 50 ms of game time per round trip
  (pipelined or larger batches), which is not built. `stage` still bounds
  the clock itself: one over 300 ms past the commit can only come from a
  driver that does not pace, and latches as skew. Genuine faults still
  latch. The app shows "Waiting for the backup — the room is paused" (after
  a latched reason and backup lost); the headless host reports `behind` and
  `caught_up` lines. The successor, whose commits are local, keeps the
  latched skew diagnostic.
  **The clients do not follow the hold through their offset.** A remote
  client's host time is its own clock plus an offset fixed at its last
  sync round (every 2.5 s), so after a hold of H it reads H ahead of the
  host until the next round. An independent review found that this broke
  the takeover correction cap: a takeover in that window gave a correction
  of about 100 + H, which Fray refuses (for the standby's own seat as
  `Unauthorized`, which ends the takeover). The bound now lives in the
  client (`MAX_LEAD_MS`, below): every survivor's correction is at most
  200 ms whatever the host's clock did. A client pulls its host-time
  estimate down to the commits at once and restarts its clock sync in the
  background, so it follows the held clock without freezing.
  Tests: `a_receipt_held_past_the_skew_budget_soft_pauses_and_the_room_commits_on`
  (one receipt held 300 ms on the pair link) and, in the app,
  `a_backup_stall_past_the_skew_budget_soft_pauses_the_host_and_play_goes_on`
  and `a_host_killed_right_after_a_soft_pause_still_resumes_every_survivor`
  (the backup stalls 450 ms, the host is killed right after it plays on
  armed again, every survivor resumes).
  The cut bound: `a_held_host_clock_keeps_every_racing_cut_within_the_correction_cap`
  and `..._lobby_cut_...` (no commit for 450 ms, then catch-up; on every
  frame a takeover's correction would be within 250 ms),
  `a_sync_round_completing_while_frozen_does_not_move_the_takeover_cut`
  (the cut is the recorded host time although a round completed while
  frozen) and, in the app,
  `a_host_killed_while_play_resumes_after_a_soft_pause_still_resumes_every_survivor`
  (killed 100, 150, 200 and 250 ms after play resumes),
  `a_host_killed_in_the_armed_lobby_resumes_every_survivor` and
  `a_host_killed_in_the_lobby_after_a_soft_pause_resumes_every_survivor`;
  each checks both survivors' accepted corrections.
  The 50 ms-per-round-trip cap and the standby's 200 ms watch are unchanged,
  so a pair-link stall over 200 ms can still promote the standby on a live
  link; it then stands the original down with `Promote`.
- A 70 ms and a 150 ms hitch mid-race, in term 0 (the test above) and in
  term 1 (`a_slow_frame_under_the_successor_is_split_into_50_ms_batches`),
  commit as 2 and 3 batches. The room carries on, the standby stays in sync
  (term 0), and the native world (ticks, racers, results, starvation) equals
  a plain host's single update over the same elapsed time. The full native
  state equals a plain host updated at the batch times: only the 30 Hz view
  schedule differs from one long update (views are released per batch).
  Removing the split fails both tests on `Err(Bounds)`.
- The takeover from that state:
  `a_takeover_from_both_own_seats_drains_the_standby_seat_and_departs_the_host_player`.
  The standby's own seat drains its in-flight intents (a clock-sync ping
  among them, answered by exactly one pong) from its real endpoint's
  journals. The host player departs by the native leave rule: its racer
  becomes a bot, its watermark retires and nothing stays in the outbox for
  it. Both own seats' `prepare_standby` is refused (`Unauthorized`).
- The host key's gate is tested where only it can refuse: a network dial on
  the original's key before `attach_own`, and again after the host's own
  player leaves and its local route detaches
  (`the_phone_hosts_own_player_runs_on_the_originals_key_and_sender`).
  Removing the gate fails the first dial, and the second when the first is
  skipped.
- The host's own in-process pipe is bounded (4096 frames each way). Its
  route moves at most 16 KiB into its ingress at once; later frames wait in
  order in the pipe and the route is never detached for capacity
  (`the_host_players_pipe_holds_a_burst_while_its_ingress_is_full_and_recovers`).
  When the host's own player leaves, its route detaches once every frame it
  sent has been read.

### Arming over the room's links

- `arming_runs_from_ask_to_armed_over_room_messages_with_the_lobby_frozen`
  and `a_playing_host_signs_its_own_seat_and_arms_over_room_messages`: Ask to
  Armed over real QUIC with the production standby address; the authority
  image is byte-identical across phase 2; a `SetCharacter` sent during the
  freeze is held and applied only after the boundary ends.
- `a_declined_ask_leaves_the_room_unarmed_with_a_reason`,
  `an_online_code_room_refuses_arming_with_a_visible_reason`.
- `a_bad_capability_ack_leaves_the_room_unprotected_naming_the_client`: a
  properly signed ACK over the wrong initial image.
- `a_client_that_never_acks_times_out_unprotected_and_releases_the_lobby`.
- `a_forged_consent_from_another_seated_peer_is_refused`: a well-formed,
  standby-signed consent sent by another seated peer before the real one is
  ignored, and arming completes.
- `a_client_refuses_to_ack_a_setup_without_the_standby_consent`.
- `a_failed_finish_tells_every_client_and_a_later_attempt_arms`: the image
  check fails inside `finish` (the app released the boundary and changed the
  lobby, the only way left to move it), every client is told and drops its
  ACK, and a second attempt with the same coordinators arms.
- `every_lobby_control_is_refused_while_the_lobby_is_frozen`.
- `a_player_joining_while_arming_leaves_the_room_unarmed_and_is_seated_after`:
  a LAN joiner (one Fray host has one path, so a Bluetooth peer cannot reach a
  LAN room; the unit test `a_held_join_on_any_path_is_seen` holds a Bluetooth
  `Joined`).
- `an_unselected_peer_flooding_migration_parts_is_cut_off_at_its_hold_bound`:
  its held `Left` replays once at the thaw. Unit tests in `lobby.rs`:
  `a_left_is_held_outside_the_full_cap_and_replays_once_at_the_thaw`,
  `a_peer_over_its_budget_is_closed_without_touching_another`.
- `a_willing_from_before_the_ask_is_ignored`,
  `phase_one_gives_up_after_60_s_without_an_answer`,
  `a_capability_ack_after_the_window_is_a_timeout_naming_the_client`.
- `bluetooth_refuses_arming_on_the_host_and_on_the_device`: an all-virtual
  (Bluetooth) room; the host refuses before sending, and a device asked and
  offered anyway refuses and declines.
- `a_decline_in_phase_two_refuses_and_releases_the_lobby`,
  `a_pair_link_failure_in_phase_two_refuses_and_releases_the_lobby`,
  `cancel_releases_the_lobby_and_after_armed_does_nothing`.
- Mutations, each failing a test: reading migration parts through the frozen
  authority (no consent arrives); skipping the client's descriptor
  verification (an unverified setup is signed); accepting a consent from an
  unselected peer (the arming is refused); marking `finish` done before its
  fallible steps (a client is never told); one control running while frozen;
  `finish` passing with a held `Joined`; dropping a `Left` at the cap;
  accepting a `Willing` queued before the `Ask`.

### The armed bundle and the switch

The network-built rooms use four humans (the standby device and three
remote humans) and four bots; (f) adds a playing phone host. Arming, the bundle and the switch all run over
the room's messages, with each device on one runner clock. The devices'
sessions are held at the frozen time across the switch (see the game-time
gap above).

- `a_network_built_room_switches_protected_and_every_survivor_resumes_at_takeover`
  (a): every device ends `Protected`; the three remote humans hold prepared
  standby links and the standby device holds its own-seat route. Armed
  commits run until the race runs and every clock is synced. The original
  then hangs (never polled again, nothing closed), and the standby promotes
  on its watch. The three survivors and the own seat recover, pass the
  barrier and resume in term 1. In one run the switch took 72 ms, the watch
  197 ms, and each survivor resumed 318 ms after the drop frame (local time;
  own seat 317 ms).
- `a_client_that_ignores_the_switch_is_reported_unprotected_and_departs_at_takeover`
  (b): the straggler's link is closed at 5 s after it was told
  `Unprotected`. The host lists only it, the others switch, and at the
  takeover it departs after the drain while both survivors resume.
- `a_client_whose_standby_link_fails_reports_unprotected_and_the_room_proceeds`
  (c): the standby refuses the forwarded token. Each remote client reports
  `Unprotected` and keeps its route, the host lists nobody (the known gap),
  and ten armed commits land.
- `a_peer_joining_between_armed_and_the_pin_is_not_seated_armed` (d): a
  stranger that joined the ordinary listener after `Armed` is closed. The
  armed roster stays the four humans and four routes attach.
- `a_tampered_armed_or_certificate_is_refused_by_the_client` (e): a bundle
  with a flipped `Armed` signature, and one with a flipped cert0 signature,
  are refused. Neither device closes its link or begins a switch, and the
  host's real bundle then switches both.
- `a_playing_phone_host_switches_armed_and_its_player_departs_when_it_dies`
  (f): the room of (a) with a playing phone host (five humans, three bots).
  The host's own seat is attached with `HostSwitch::attach_own` after the
  switch and plays the armed commits. Then the phone host hangs, its seat
  with it. The standby promotes, the three remote survivors and the standby's
  own seat resume in term 1, and the host's player departs (not a member,
  its watermark retired, nothing for it in the outbox). In one run the
  switch took 104 ms, the watch 197 ms, and each survivor resumed 500 ms
  after the drop frame (local time; own seat 499 ms).
- `a_client_gone_during_the_freeze_is_parked_at_the_pin_and_departs_after_thirty_seconds`
  (g): remote human 3's ordinary link drops after `Armed`, while the lobby
  is still frozen and before the switch starts. A roster `Left` during
  consent, pairing or activation ends arming instead ("left during
  arming"), so this is the window in which a departure reaches a successful
  switch. The client never re-dials. The host lists it after the 5 s route
  window ("left the lobby link but did not reconnect"), and it was parked at
  the pin. The room plays on armed without pausing; the departure batch
  lands just over 30 s after the pin (30006 ms in one run) and keeps it a
  seatless member, and the next batch retires it (revision + 1). Real time:
  about 36 s.
- (h) `a_switch_failing_after_switch_thaws_the_lobby_and_every_client_rejoins_unarmed`:
  four humans, one a straggler that sets ready during the freeze (held at
  the frozen boundary, ahead of its ACK). The standby's pair link drops 1 s
  after the three others closed their ordinary links, inside the leave
  window, so the host has read their `Left`s. The host reports
  `Unprotected` ("The pair link failed during the switch"), its lobby is no
  longer frozen, and `take_room` is `None`. The three re-dials never
  complete; each client rejoins the ordinary lobby about 11 s after
  `Switch`. The thawed lobby then seats exactly the four humans, once each,
  with the straggler ready, and an unarmed race starts on all four
  devices. Mutations: discarding the held events at `Switch` again (the
  straggler's ready is lost: "the thawed lobby's humans"); not holding the
  roster's `Left`s (seven humans: "stalled"). Real time: about 12 s.
- (h2) `a_pin_refused_until_the_deadline_thaws_the_lobby_and_every_client_rejoins_unarmed`:
  the same fallback from a forced pin failure (`HostSwitch::refuse_pins_for_test`,
  a hidden test hook). The host gives up at the 10 s budget ("could not be
  pinned"); the straggler, closed at the leave deadline, is departed by the
  thaw, so the three rejoined humans are seated once each and race unarmed.
- (h2b) `a_full_lobby_failed_switch_keeps_the_token_for_every_fallback_rejoin`:
  eight humans behind a dedicated host fill the room, which closed when the
  eighth sat (rotating the token; closed at the switch, asserted). The pin
  is refused until the deadline, so the thaw's replayed `Left`s reopen the
  room. In the hold window the token stays, so all eight fallback rejoins
  are admitted and the room is closed again with the `Switch` token
  (asserted), still after the window. One human then leaves: the room
  reopens and the token rotates (asserted), and an unarmed race starts on
  the seven. Mutation: rotating in the window (plain `set_open`) refuses
  every rejoin ("stalled", humans 0). Real time: about 21 s.
- (h3) `a_switch_failing_after_a_token_rotation_still_rejoins_every_client_unarmed`:
  as (h), but the listener is closed and reopened after the room armed,
  just before the switch, so the token every device joined with no longer
  admits it (asserted). Each switched device's fallback rejoin, at its own
  join address with the `Switch` token, is admitted: the thawed lobby seats
  the four humans once each and an unarmed race starts. Mutation: the
  fallback with the join-time token again is refused ("stalled": the
  devices never reach the lobby).
- (h4) `a_wrong_switch_token_before_a_failed_pin_still_rejoins_with_the_join_token`:
  as (h), but one device first gets a genuine bundle and a `Switch` with a
  wrong token. Its re-dial is refused; its fallback rejoin is refused with
  the `Switch` token and admitted with its join token, at its own join
  address, and the thawed lobby seats the four humans once each. Mutation:
  the fallback with the `Switch` token alone strands it ("stalled").
- (i) `a_standby_lost_after_the_pin_pauses_the_armed_room_and_nothing_is_thawed`:
  the standby's pair link drops right after the pin. The host still reports
  `Protected`, `take_room` hands over the armed room, its `LivePrimary`
  pauses (`failure()` is `Unauthorized`: the pair link's keys are gone, a
  genuine fault that latches), and
  the lobby authority stays frozen with all four humans. Mutation: treating
  a route-window failure as terminal (thaw) fails it (the host is
  `Unprotected`).
- (j) `a_full_lobby_of_eight_humans_switches_protected_although_its_token_rotated`:
  eight humans and no bot behind a dedicated host. The native host closed
  the room when the eighth joined, so the LAN token every device joined with
  has rotated (asserted). All eight re-dial their own join address with the
  `Switch` token: the host lists nobody, every device is `Protected`, and
  eight routes attach.
- (k) `a_token_rotated_after_the_join_still_re_dials_protected`: four humans;
  the listener is closed and reopened after the room armed, just before the
  switch (the token rotated, asserted). Everyone switches protected.
- (l) `a_tampered_switch_token_fails_the_re_dial_and_the_device_falls_back`:
  one device gets a genuine bundle and a `Switch` with a wrong token. It
  verifies, closes, re-dials its own join address and is refused at
  admission ("could not be rejoined", then "Rejoining the lobby unarmed"),
  hands back a session for the ordinary rejoin (its own join address only,
  with the `Switch` token and then the join token; the pinned listener
  refuses both) and never gets an armed route. The host lists it; the other three switch protected.
- Mutation: re-dialling with the original join's token again fails (j) and
  (k) (the host lists the humans).
- Mutations, each failing a test: the client acting on `Switch` without a
  verified bundle (e: "a refused bundle closed the link"); the host pinning
  at once without closing stragglers (b: "The room's listener could not be
  pinned"); the missed-switch listing disabled (b: the straggler is not
  listed); no park at the pin (g: "the never-attached client was not parked
  at the pin"); `HostSwitch::attach_own` not attaching (f: "the host's own
  seat was refused").

## App wiring (item 7a, `app/src/online/armed.rs`)

- **One owner per role.** The app's one `OnlineSession` (in the online slot)
  holds this device's migration role: `HostArming`, `HostSwitch`, the armed
  host (`LivePrimary`, `PlayerHost`, the phone host's own `RemotePlayer`),
  `ClientArming` (with `LiveStandby` once the standby device is armed),
  `ClientSwitch`, the armed client (`RemotePlayer`, plus `LiveStandby` on the
  standby device), `RecoveringPlayer`, the takeover (`SurvivorLinks`,
  `OwnSeat`, the resume host) and `LiveSuccessor`. Screens read the active
  client through `OnlineSession::me()` (the ordinary session, or the one
  inside the route, recovery or successor seat), the authority through
  `authority()`, and the state through `migration_state()`. A screen command
  (`client_command`) runs inside the next armed frame's client transaction,
  so its intent is journaled before it is sent; a host control
  (`set_track`, `start_race`, `kick`, ...) is queued into the next staged
  batch; while arming or switching it is refused (by the frozen lobby, or
  by the switch). Screen commands issued during `ClientArming`,
  `HostArming` or `HostSwitch` are not deferred: they reach the frozen
  lobby, and are dropped if the switch succeeds. No screen holds a
  migration object.
- **Always pumped.** `OnlineSession::pump` drives the role every frame (the
  engine's catch-all pump covers frames no screen drove, and the background
  tick). Host: `update` and `HostArming::poll`, then `HostSwitch::poll`, then
  at `Protected` `attach_own` and `take_room`; then each frame the own seat,
  `PlayerHost::poll`, `LivePrimary::poll` (`publish` on `Committed`,
  `certify` on `Membership`), one `stage` when nothing is pending, `flush`.
  Client: `update` and `ClientArming::poll`, `take_switch`,
  `ClientSwitch::poll`, then `RemotePlayer::frame` and `watch_standby`; a
  `Promotion` moves the player into `recover`, then `step` until `Resume`
  and `into_resumed`. Standby device: `LiveStandby::poll` every frame from
  its bootstrap on (also during the switch); in the armed room it first
  reads and installs everything queued on the pair link (`poll` again, up
  to 8 times, while it installs or frames stay queued), and only then looks
  at its watch: promotion once `expired()` or the original `Left`, then
  `PromotedStandby::poll` on a clock sampled after `promote()` (the signed
  `Promote` reaches a still-live original, which pauses as superseded; the
  frame's earlier sample was refused as a backwards clock, so nothing was
  sent: `a_standby_promoting_on_a_live_original_tells_it_and_it_stands_down`),
  then
  `SurvivorLinks::open`, `OwnSeat` from the route's
  parts (none if the standby's player left and its seat is gone at C),
  `start_resume_host` on the same code, `drive` to `Ready`, the
  barrier (`OwnSeat::ready`), `drive_barrier`, `resume`, `OwnSeat::resumed`
  and `into_successor`; the successor polls, frames its own seat, and stages
  and commits until its receipt reaches game time (at most 50 ms a batch).
- **Clock.** Session time is held from the host's freeze (a client: from its
  consent or its `Setup`) until its switch finishes, then continues: the
  armed room resumes at the armed initial time. A fallback to the unarmed
  lobby drops the hold, so the session catches up with the thawed host.
  The armed host stages at `max(session time, armed initial time)`; the
  successor at `C + (session time - Resume)`; survivors rebase at their cut
  and `Resume` inside `RecoveringPlayer`.
- **Failures.** A closed link to the original is not fatal while the client
  holds a prepared standby link (or is the standby device): it keeps
  watching for `Promote`; with neither, it ends after
  `SESSION_LOST_AFTER_MS`. A standby runtime that latches a fault can never
  promote, so the standby device drops it at once and is then an ordinary
  unprotected client ("This phone can no longer take over (…)."): the
  ordinary loss path applies, and a background standby ends. A latched `LivePrimary` failure pauses the armed
  room; `Unauthorized`/`Closed` (the pair link gone) reads "Backup lost — the
  room is paused". So does the soft pause `LivePrimary::backup_lost()` (the
  standby watch reads `Paused`), read afresh every frame without touching
  the room's latched reason: it clears by itself and the room plays on. The
  soft behind pause `LivePrimary::behind()` reads "Waiting for the backup —
  the room is paused" the same way, after backup lost. The
  standby's pulse activity is derived from the checkpoint it installs, so
  the app sets none. The successor's session-lost error ends the session with "The
  new host stopped responding, so the game has ended." (no second takeover).
- **State for the screens** (`MigrationState`): `Idle`, `Asking`,
  `ConsentRequested`, `Arming`, `Armed { unprotected }` (host: the names and
  reasons the switch left out), `Unprotected { reason }`, `Refused {
  reason }` (the coordinators' own reasons, so an online-code room and a
  Bluetooth player read as such), `Paused { reason }` (a host's latched
  failure or its soft backup-lost pause; a client frozen by loss detection:
  "Waiting for the host"),
  `Recovering`, `Resumed` (for `RESUMED_NOTICE_MS`, 3 s, after `Resume`),
  `UnprotectedAfterTakeover`. Actions: `choose_backup(seat)`,
  `answer_consent(yes)`, `cancel_arming()`. A client the switch left out is
  `Unprotected`, and its dead-link text carries the reason.
- **Leaving.** A client's `Leave` goes through its endpoint's frame
  (`ClientSession::leave` closes only the client's old, already detached
  transport, never the pair or standby link); the rest is dropped off the UI
  thread. An armed original that leaves is taken over (its link is `Left`).
  The standby device's player leaving keeps the standby: its `Leave` goes
  out and its seat departs like any human's, but the role stays
  (`OnlineSession::in_background`), and `OnlineSlot::leave` (or `put`) moves
  the session to the slot's background list, which the engine drives every
  frame and in the background tick (`OnlineSlot::pump_background`) until
  the room ends. If the host then fails, the standby promotes and takes over.
  Its seat is still driven while it is a member at C: seated (a `Leave` in
  flight drains as a departure) or kept seatless by its `Leave` batch (the
  successor's first batch retires it); only a seat retired before C takes
  over seatless. The successor hosts on in the background while any human
  remains. A standby device that goes (its
  session dropped, or never pumped again) pauses the armed room as backup
  lost: the original's standby watch convicts it.
- **Bluetooth.** The Stampede app has no separate Bluetooth listener: a
  room's only listener is its Fray host, arming refuses a room with a
  Bluetooth player, and once armed the pinned listener refuses every new
  ordinary joiner.
- **Watch under load (open).** The tests pump three phones in one thread.
  Under CPU load a pump of over 100 ms has twice starved the standby's
  200 ms watch at the race start, and it promoted while the original was
  alive (the original then paused as superseded, the survivors resumed on
  the standby). The standby now reads and installs everything queued before
  its watch, and `promote` re-checks the watch itself, so a standby frame
  stall alone does not promote on the frame after it
  (`a_standby_whose_pump_stalls_does_not_promote`). That ordering held
  before too (`poll` ran before `expired()`), and race-start images are
  about 8 KB, far below the 256-frame poll cap and the 64 KiB ingress burst,
  so the reported promotions are not explained by it. A likelier cause was
  on the original: `stage` ran after `poll`'s send loop, so each candidate
  left one host frame after its stage, doubling the silence the standby saw
  when host frames were slow. Fixed: `stage` now sends the candidate itself
  (`a_staged_candidate_is_on_the_wire_in_the_same_frame`). Whether that
  explains the reported promotions is still open.
- **Standby stalls soft-pause the room.** The original convicts a standby
  that answers nothing for 200 ms. A standby pump stall of about 200 ms or
  more therefore shows "Backup lost — the room is paused" on the host, as a
  soft pause: the host keeps polling and probing (so it does not fall
  silent and the standby that stalled has no reason to promote), and the
  pause clears by itself once the standby answers or commits again
  (`a_standby_hiccup_soft_pauses_and_the_room_resumes_by_itself`).
  **Catch-up after a soft pause (decided).** Game time is not held during
  the pause while the commits are under the 250 ms skew budget behind it.
  When the standby returns, the room catches up at up to 50 ms of game time
  per frame (one batch of at most `MAX_BATCH_GAME_MS` per stage and commit),
  about three times normal speed at a 60 Hz driver, until the receipt
  reaches the driver's clock. This is the existing hitch catch-up rule (see
  "Slow frames" above). At the budget the driver's clock holds (the soft
  behind pause, decided 2026-10-19, above), so the catch-up is at most
  250 ms of game time; the clients then follow the held clock through
  their bound on the commits (`MAX_LEAD_MS`) and a background sync round.
- **Retry versus the standby watch.** The owed retry fires 150 ms after the
  stage, before the standby watch convicts at 200 ms, so a lost receipt is
  retried and committed with no soft pause
  (`a_receipt_dropped_with_no_later_refusal_is_retried`). Since Fray's
  `expire` fix a receipt is lost only across an own stall of 500 ms or
  more, and there the retry is due at once on the first poll after it. The
  cost: a standby slower than 150 ms to answer gets identical resends,
  which it answers with the same receipt; the original drops the late
  copies as `Stale`.
- **The standby watch without commits.** Fray's watch counts standby
  progress only from an advancing frontier or an idle/idle pulse. Both
  runtimes now derive their activity from the committed checkpoint (`Idle`
  in the lobby and at index 0 through the switch), the original's drivers
  (the app and `HostSwitch`) probe every frame, and a `Paused` watch is a
  soft pause, so
  `HostSwitch` polling `LivePrimary` for seconds through its leave, pin and
  route windows, and the idle windows of the tests, no longer fail arming
  or latch the room. The tests this kept red pass with their assertions
  unchanged
  (`a_client_gone_during_the_freeze_is_parked_at_the_pin_and_departs_after_thirty_seconds`,
  `a_client_that_ignores_the_switch_is_reported_unprotected_and_departs_at_takeover`,
  `a_pin_refused_until_the_deadline_thaws_the_lobby_and_every_client_rejoins_unarmed`,
  `a_leave_followed_at_once_by_the_drop_is_committed`,
  `an_expired_approval_gets_no_output_and_no_frame_reaches_the_queue`,
  `original_approval_expiry_stops_replication_but_standby_still_promotes`,
  `per_frame_probes_neither_latch_the_standby_nor_pause_the_original`, and
  the `ArmedRoom` and owned-standby tests that failed through them). Two
  tests asserted the old latch itself and now assert the soft pause:
  `a_missed_promotion_pauses_the_original_as_backup_lost` and
  `standby_own_approval_expiry_latches_and_refuses_promotion`. Their
  `standby.set_activity(Advancing)` setup lines went with the setter.
  `owned_standby_replicates_the_race_and_refuses_the_closed_term` waits out
  the standby's 50 ms probe pacing before its explicit probe, and polls the
  original 50 ms longer after the stage (the candidate now leaves from
  `stage`), so the probe's term-0 answer still reaches the standby.
- **Known gaps.** The seat-to-peer lookup for `choose_backup` reads the
  authority's serialized seats (no public `HostAuthority` accessor yet).
  During a client's switch (normally ~100 ms) the screens see a stand-in
  client with no lobby. A client whose re-dial failed rejoins on the switch's
  own link and cannot arm again in that session. A standby that leaves
  during arming or the switch (before the armed room) is dropped, not kept.
- Tests (`app/src/online/tests.rs`, real QUIC loopback):
  `an_app_host_standby_and_client_arm_race_and_survive_a_killed_host` (a
  phone host, the standby phone and a client arm over the room's messages,
  a stranger's join is refused while armed, they race, the host is never
  pumped again; both survivors resume on the same code, about 500 ms after
  the kill, the promoted phone hosts with its seat and the host's player
  departs; then the new host stops and the client ends with the documented
  reason), `arming_states_and_reasons_read_plainly`,
  `an_online_code_room_refuses_arming_with_a_visible_reason`,
  `a_room_with_a_bluetooth_player_refuses_arming_with_a_visible_reason`,
  `a_standby_device_that_goes_pauses_the_armed_room_as_backup_lost` (the
  standby's session is dropped; the host reads "Backup lost — the room is
  paused", the client loses its backup and stays),
  `the_standby_players_leave_keeps_the_standby_and_a_seatless_takeover_resumes_survivors`
  (the standby's player leaves mid-race; its seat departs, the room plays on
  armed, the host is killed, the background standby promotes seatless and
  the client resumes about 500 ms after the kill and races on),
  `a_standby_players_leave_in_flight_at_the_takeover_drains_and_its_seat_departs`
  (the standby's player leaves and the host dies in the same step; the
  background standby drives its still-seated seat, the `Leave` drains as a
  departure, the promoted phone hosts seatless and the client races on),
  `a_latched_standby_stops_and_the_seated_phone_ends_when_the_host_leaves`
  and `a_latched_background_standby_is_torn_down` (a standby that latches a
  fault is dropped at once, so the phone is an ordinary unprotected client:
  seated, it ends with "Lost the connection to the host." when the host
  leaves; in the background, it ends with "The background standby
  stopped."),
  `a_standby_whose_pump_stalls_does_not_promote` (the standby phone is not
  pumped for 300 ms; on its next frame it does not promote),
  `a_client_whose_redial_fails_rejoins_the_unarmed_lobby_and_cannot_arm_again`
  (the host's pin is refused until its deadline; the client falls back after
  about 11.4 s, its budget plus grace, to an ordinary rejoin, is seated in
  the thawed lobby as `Unprotected` "… Rejoining the lobby unarmed.", and
  keeps no arming link);
  `game/source_net/tests.rs`:
  `an_armed_clients_race_source_races_on_through_a_takeover`.

## Not yet

- A non-headless promoted room left with no humans. The fixture's original
  is headless, and its empty room keeps racing with bots; a non-headless one
  would reset to the lobby (`after_departure`). Untested.
- A lobby takeover in which recovery's replayed `TrackData` journals
  `TrackReady` past the cut at C, then seals and drains it.
- The switch's known gaps (see "The armed bundle and the switch"):
  prepared standby links that the host cannot see, held lobby events
  superseded at a successful switch, game time paused across it, and no
  ordinary rejoin for a client whose re-dial failed while the host pinned.
- Player transport beyond reconnect: lossy views
  and snapshots on QUIC datagrams. No takeover after a silence-rule
  departure is tested. The own-seat tests still use
  synthetic bound channels (their signed `Applied` ACKs commit and prune the
  outbox over them).
- Idle liveness under the successor uses certified idle commits, not the
  spec's challenge/response pulses: Fray does not expose a term-1 pulse
  responder on `SuccessorActor`.
- Process-kill trials, phone recovery and one-second recovery.
- App wiring gaps (see "App wiring"): no LAN-only host option in the UI (a
  room arms only when the player's relay list is empty), no consent or
  arming screens yet (7b), and the standby device's leave drops its runtime.
