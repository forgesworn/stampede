//! From an armed room to protected play (slices 6.7 and 6.8): the armed
//! bundle, the switch to session-bound links and the missed-switch report,
//! carried as real `ToClient::Migration` messages over the ordinary links.
//!
//! - [`HostSwitch`] runs on the host once [`super::HostArming`] is `Armed`.
//!   It builds the original's [`LivePrimary`] from the frozen lobby image,
//!   waits for the standby's index-0 membership certificate (cert0) on the
//!   pair link, then sends every remote roster human `Armed{armed, cert0}`,
//!   `Standby{addr, token}` and `Switch{token}` (the ordinary listener's LAN
//!   token at that moment, which may have rotated since the clients joined).
//!   It waits up to
//!   [`SWITCH_LEAVE_MS`] for the ordinary links to leave, closes the
//!   stragglers, pins the listener ([`PlayerHost::new`]) and attaches routes
//!   as the clients re-dial, for up to [`SWITCH_ROUTE_MS`] after the pin.
//!   Roster humans with no attached route then are reported unprotected, and
//!   the room proceeds. Every roster human is parked at the pin, so one that
//!   never attaches departs after [`super::PARKED_DEPARTURE_MS`].
//!   The lobby's held events are kept until the switch succeeds. A switch
//!   that fails after `Switch` but before the pin (the pin still refused at
//!   the deadline, the pair link, any terminal error) thaws the lobby with
//!   them replayed, followed by the `Left` of every roster human whose
//!   ordinary link left, so it keeps no ghost seats, and the room proceeds
//!   unarmed with the reason in [`SwitchState::Unprotected`]. Once the pin
//!   succeeded the armed room exists and the listener is pinned, so nothing
//!   is thawed: a failure in the route window (the standby lost) is the armed
//!   runtime's, which pauses with its reason ([`LivePrimary::failure`]), and
//!   the switch still finishes `Protected` and hands the room over.
//! - [`ClientArming`](super::ClientArming) keeps the bundle and, on `Switch`,
//!   verifies it against its acknowledged setup before it closes the ordinary
//!   link. [`ClientSwitch`] then re-dials the original session-bound
//!   ([`PinnedJoin::until`]) at the address it joined, with the token from
//!   `Switch`, builds the route ([`RemotePlayer::connect`],
//!   with the exact capability ACK the device signed) and prepares the
//!   standby link ([`RemotePlayer::prepare_standby`]). If the re-dial fails
//!   or never completes, the client rejoins the ordinary lobby at the
//!   address it joined, with the token from `Switch` (the listener's current
//!   one, so a token that rotated since the join still admits it) and, if
//!   that join fails, with the token it joined with, after
//!   [`SWITCH_REJOIN_GRACE_MS`] past its switch budget, so it lands in the
//!   thawed, unarmed lobby. A room closed at the switch (a full lobby) is
//!   reopened by the thaw's replayed `Left`s; for [`SWITCH_TOKEN_HOLD_MS`]
//!   from the thaw that reopen (and the close when the last rejoin fills it
//!   again) keeps the token `Switch` carried, so the rejoins are admitted.
//!   After that window, or once a race starts, each open or close rotates
//!   the token again, as natively.
//!
//! Known gaps (v0.1):
//! - The host lists only missing routes, not prepared standby links. A
//!   client whose `prepare_standby` failed knows it and reports itself
//!   unprotected, but the host lists it as protected. Closing this needs a
//!   new pair-link record from the standby, which v0.1 does not have.
//! - A client whose re-dial failed while the host pinned rejoins a pinned
//!   listener, which refuses its ordinary join: that device rejoins when the
//!   match ends.

use super::lobby::{hex, is_lan_ip, send_to_client, unhex32, ArmedHost};
use super::wire::chunk;
use super::{
    checkpoint, null_transport, restore, thaw_lobby, LivePrimary, LiveStandby, PlayerHost,
    PlayerHostRefused, RemotePlayer, RunnerClock,
};
use crate::host::HostAuthority;
use crate::net::{
    join_player_route, join_standby_route, PinnedJoin, PinnedJoinPoll, QuicClient, QuicHost,
};
use crate::proto::ToClientMigration;
use crate::transport::{
    ClientNetEvent, ClientTransport, HostNetEvent, HostTransport, PeerKey, LOCAL_PEER_BASE,
};
use crate::ClientSession;
use decented_net::migration::{
    Armed, CapabilityAck, Descriptor, Error, MembershipCertificate, SenderSequence, MAX_BATCH,
    MAX_JOURNAL_BYTES, MAX_JOURNAL_RECORDS,
};
use decented_net::{Identity, NetConfig};
use std::collections::{BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// How long the host waits, from `Switch`, for the ordinary links to leave
/// before it closes the stragglers and pins.
pub const SWITCH_LEAVE_MS: u64 = 5000;
/// How long the host waits, from the pin, for the clients' session-bound
/// routes; then it reports the missing ones unprotected.
pub const SWITCH_ROUTE_MS: u64 = 5000;
/// How long the host waits for cert0 on the pair link before the bundle.
/// Nothing has switched yet, so a timeout leaves the room unarmed.
pub const CERTIFICATE_WINDOW_MS: u64 = 5000;
/// A client's whole switch, from `Switch`: it cannot see the pin, so its
/// re-dial is retried for the host's leave window plus its route window.
pub const SWITCH_JOIN_MS: u64 = SWITCH_LEAVE_MS + SWITCH_ROUTE_MS;
/// One session-bound connection attempt's own deadline.
const ATTEMPT_DEADLINE: Duration = Duration::from_secs(2);
/// A client whose re-dial failed waits this long past its switch budget
/// ([`SWITCH_JOIN_MS`]) before it rejoins the ordinary lobby: the host gives
/// up pinning at the same budget from its own `Switch`, so by then a failed
/// host has thawed its lobby instead of closing the join as a newcomer.
pub const SWITCH_REJOIN_GRACE_MS: u64 = 1000;
/// After a switch that failed before the pin, how long the thawed lobby's
/// opens and closes keep the LAN token `Switch` carried
/// (`HostAuthority::hold_lan_token_for`), timed from the thaw: a client's
/// whole switch budget plus its rejoin grace. A switched client starts its
/// fallback rejoin within that long of its `Switch`, which the host sent
/// before it could thaw.
pub const SWITCH_TOKEN_HOLD_MS: u64 = SWITCH_JOIN_MS + SWITCH_REJOIN_GRACE_MS;

/// A roster human the switch left unprotected, as the host reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnprotectedPlayer {
    pub player: u64,
    pub name: String,
    pub reason: String,
}

/// Where the switch stands, on the host or on a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchState {
    /// Host: waiting for cert0, the leaves, the pin or the routes. Client:
    /// re-dialling the original or preparing the standby link.
    Switching,
    /// The room plays armed. Host: `unprotected` lists the roster humans with
    /// no route at the deadline (a client whose standby link failed is not
    /// listed: the host cannot see it). Client: this device has its route and
    /// its prepared standby link (the standby device: its route); the list is
    /// empty.
    Protected { unprotected: Vec<UnprotectedPlayer> },
    /// Host: the room could not be armed before the pin, and `reason` says
    /// why. The lobby is thawed with its held events (and, after `Switch`,
    /// the `Left` of every roster human whose ordinary link left) and plays
    /// on unarmed. After the pin the host never reaches this state.
    /// Client: this device is not protected. If its re-dial failed,
    /// [`ClientSwitch::take_client`] is a fresh session rejoining the
    /// ordinary lobby.
    Unprotected { reason: String },
}

impl SwitchState {
    pub fn is_finished(&self) -> bool {
        !matches!(self, SwitchState::Switching)
    }
}

/// A remote roster human: its player, ordinary connection and name.
struct RosterHuman {
    player: u64,
    peer: PeerKey,
    name: String,
    left: bool,
    /// Closed at the leave deadline: it never left by itself.
    straggler: bool,
}

enum HostStage {
    /// Waiting for cert0; the lobby is still frozen.
    Certifying {
        since: u64,
    },
    /// The bundle and `Switch` went out at `sent`.
    Leaving {
        sent: u64,
    },
    /// Every ordinary link has left or was closed; pin, retried.
    Pinning {
        sent: u64,
    },
    Routing {
        pinned: u64,
    },
    Done,
}

/// The host side of the switch, after [`super::HostArming`] reached `Armed`.
///
/// Drive it every frame with [`HostSwitch::poll`], with the same lobby
/// authority and room [`QuicHost`] arming used; the authority stays frozen
/// (its `update` is a no-op) and the switch reads the transport itself.
/// The pin moves the listener out of `network` (a closed shell remains), and
/// from then on the armed room is [`HostSwitch::take_room`]'s: the frozen
/// lobby authority is superseded and should be dropped.
pub struct HostSwitch {
    descriptor: Descriptor,
    /// The standby-signed initial Armed record (the players' pin).
    armed: Armed,
    standby: (SocketAddr, [u8; 32]),
    own_ack: Option<Vec<u8>>,
    live: Option<LivePrimary>,
    players: Option<PlayerHost>,
    roster: Vec<RosterHuman>,
    /// The lobby events held at the frozen boundary, then the roster's
    /// events the switch read (bounded), until the switch succeeds: a failed
    /// switch replays them at the thaw.
    held: VecDeque<HostNetEvent>,
    held_bytes: usize,
    stage: HostStage,
    state: SwitchState,
    superseded: usize,
    refused_joins: usize,
    pin_refusals: Vec<Error>,
    /// The first armed-room failure in the route window, logged once.
    route_failure: Option<String>,
    /// Test hook: every pin attempt is refused.
    refuse_pins: bool,
}

impl HostSwitch {
    /// Start from [`super::HostArming::take_armed`]: the original's runtime
    /// is built from the frozen lobby image (the armed initial image), on the
    /// device's `clock`. Nothing is sent until cert0 has arrived.
    ///
    /// If the runtime cannot be built, the lobby is thawed with its held
    /// events, every remote roster human is told it is unprotected, and the
    /// state is `Unprotected`.
    pub fn start(
        host: &mut HostAuthority,
        network: &mut QuicHost,
        armed: ArmedHost,
        identity: &Identity,
        clock: RunnerClock,
        now_ms: u64,
    ) -> Self {
        let ArmedHost {
            bootstrap,
            descriptor,
            own_ack,
            held,
            standby,
            standby_token,
        } = armed;
        let roster = host
            .humans
            .iter()
            .filter(|h| h.peer < LOCAL_PEER_BASE)
            .map(|h| RosterHuman {
                player: u64::from(h.id) + 1,
                peer: h.peer,
                name: h.name.clone(),
                left: false,
                straggler: false,
            })
            .collect();
        let mut switch = Self {
            descriptor: descriptor.clone(),
            armed: bootstrap.armed.clone(),
            standby: (standby, standby_token),
            own_ack,
            live: None,
            players: None,
            roster,
            held_bytes: held
                .iter()
                .map(|e| match e {
                    HostNetEvent::Reliable { bytes, .. } => bytes.len(),
                    _ => 0,
                })
                .sum(),
            held,
            stage: HostStage::Done,
            state: SwitchState::Switching,
            superseded: 0,
            refused_joins: 0,
            pin_refusals: Vec::new(),
            route_failure: None,
            refuse_pins: false,
        };
        // The runtime's authority is a copy of the frozen lobby at the armed
        // initial image, not frozen and without the lobby's transport.
        let live = checkpoint(host)
            .and_then(|(binding, bytes)| restore(&bytes, &binding, null_transport()))
            .and_then(|authority| {
                LivePrimary::new(descriptor, bootstrap, authority, identity.clone(), clock)
            });
        match live {
            Ok(live) => {
                switch.live = Some(live);
                switch.stage = HostStage::Certifying { since: now_ms };
            }
            Err(e) => switch.unarmed(
                host,
                network,
                format!("The armed room could not start ({e})."),
            ),
        }
        switch
    }

    pub fn state(&self) -> &SwitchState {
        &self.state
    }

    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// The original's runtime, until [`HostSwitch::take_room`].
    pub fn live(&self) -> Option<&LivePrimary> {
        self.live.as_ref()
    }

    /// The pinned player host, once pinned and until `take_room`.
    pub fn players(&self) -> Option<&PlayerHost> {
        self.players.as_ref()
    }

    /// A playing host's own capability ACK bytes, as it signed them at
    /// arming ([`HostSwitch::attach_own`] uses them).
    pub fn own_ack(&self) -> Option<&[u8]> {
        self.own_ack.as_deref()
    }

    /// A playing phone host's own seat in the armed room: its in-process
    /// route ([`PlayerHost::attach_own`]) with the ACK it signed at arming
    /// and the armed initial record. Call it once `Protected` and before
    /// [`HostSwitch::take_room`], so before the first armed commit. `client`
    /// is the host's own lobby session (its loopback client) and `keys` the
    /// original's. The seat is not a roster human: it gets no bundle, is
    /// never listed or parked, and departs with the device at a takeover.
    pub fn attach_own(
        &mut self,
        client: ClientSession,
        keys: decented_net::Keys,
    ) -> Result<RemotePlayer, Error> {
        if !matches!(self.state, SwitchState::Protected { .. }) {
            return Err(Error::Stale);
        }
        let ack = self.own_ack.as_deref().ok_or(Error::Unauthorized)?;
        let capability = CapabilityAck::decode(&self.descriptor.root, ack)?;
        let (Some(live), Some(players)) = (self.live.as_mut(), self.players.as_mut()) else {
            return Err(Error::Closed);
        };
        players.attach_own(live, &self.armed, &capability, client, keys)
    }

    /// Ordinary events held at the frozen boundary (and read during the
    /// switch) that a successful switch superseded: lobby traffic on links
    /// the armed room replaces. Zero until `Protected`.
    pub fn superseded(&self) -> usize {
        self.superseded
    }

    /// Ordinary joins closed between `Armed` and the pin (never seated).
    pub fn refused_joins(&self) -> usize {
        self.refused_joins
    }

    /// Test hook: refuse every pin attempt (as Fray refusing it), so the
    /// switch falls back to the unarmed lobby at the pin deadline.
    #[doc(hidden)]
    pub fn refuse_pins_for_test(&mut self) {
        self.refuse_pins = true;
    }

    /// The first failure of the armed room in the route window (the pair
    /// link lost, a route error), if any. The room is still handed over;
    /// [`LivePrimary::failure`] holds the runtime's own pause reason.
    pub fn route_failure(&self) -> Option<&str> {
        self.route_failure.as_deref()
    }

    /// Why each refused pin attempt was refused, in order.
    pub fn pin_refusals(&self) -> &[Error] {
        &self.pin_refusals
    }

    /// The armed room, once `Protected`: the original's runtime and its
    /// pinned player host. The app drives them from here on.
    pub fn take_room(&mut self) -> Option<(LivePrimary, PlayerHost)> {
        if !matches!(self.state, SwitchState::Protected { .. }) {
            return None;
        }
        Some((self.live.take()?, self.players.take()?))
    }

    pub fn poll(
        &mut self,
        host: &mut HostAuthority,
        network: &mut QuicHost,
        now_ms: u64,
    ) -> &SwitchState {
        if let Err(reason) = self.step(host, network, now_ms) {
            self.unarmed(host, network, reason);
        }
        &self.state
    }

    /// Before the pin: give the lobby back natively, as a failed activation
    /// does. The armed runtime is dropped, the lobby is thawed with every
    /// held event in order (after `Switch`, the roster's `Left`s among them,
    /// so the humans whose ordinary links left are not ghost seats), and
    /// every remote roster human whose link is still up is told why. The
    /// room plays on unarmed; the clients that switched rejoin the lobby (see
    /// [`ClientSwitch`]). The listener is still in `network`, unpinned.
    fn unarmed(&mut self, host: &mut HostAuthority, network: &mut QuicHost, reason: String) {
        log::warn!("[switch] unarmed: {reason}");
        debug_assert!(self.players.is_none(), "a pinned room is never thawed");
        self.stage = HostStage::Done;
        self.live = None;
        // The clients rejoin with the token `Switch` carried: the thaw's
        // replay (a full lobby's reopen) must not rotate it in that window.
        host.hold_lan_token_for(SWITCH_TOKEN_HOLD_MS);
        thaw_lobby(host, std::mem::take(&mut self.held));
        self.held_bytes = 0;
        let short: String = reason.chars().take(200).collect();
        for human in &self.roster {
            send_to_client(
                network,
                human.peer,
                &ToClientMigration::Unprotected {
                    reason: short.clone(),
                },
            );
        }
        self.state = SwitchState::Unprotected { reason };
    }

    fn step(
        &mut self,
        host: &mut HostAuthority,
        network: &mut QuicHost,
        now_ms: u64,
    ) -> Result<(), String> {
        let _ = host;
        match &self.stage {
            HostStage::Certifying { since, .. } => {
                let since = *since;
                let live = self.live.as_mut().ok_or("The armed room is gone.")?;
                // The driver's probe, every step (`LivePrimary::probe`): the
                // idle standby's answers keep its watch live meanwhile.
                live.probe()
                    .and_then(|()| live.poll())
                    .map_err(|e| format!("The pair link failed before the switch ({e})."))?;
                let cert = live
                    .membership_certificate()
                    .filter(|c| c.receipt == *live.primary().receipt())
                    .cloned();
                let Some(cert) = cert else {
                    if now_ms.saturating_sub(since) >= CERTIFICATE_WINDOW_MS {
                        return Err(
                            "The standby's armed certificate did not arrive within 5 s.".into()
                        );
                    }
                    return Ok(());
                };
                self.send_bundle(network, &cert)?;
                // The room is not closed here: closing rotates the LAN token
                // `Switch` just gave the clients to re-dial with. A join from
                // now to the pin is closed as it arrives (`drain`), and the
                // pin closes the room.
                // The held events stay until the switch succeeds: a failure
                // from here on thaws the lobby with them.
                self.stage = HostStage::Leaving { sent: now_ms };
                Ok(())
            }
            HostStage::Leaving { sent } => {
                let sent = *sent;
                self.keep_live()?;
                self.drain(network);
                let pending: Vec<usize> = (0..self.roster.len())
                    .filter(|&i| !self.roster[i].left)
                    .collect();
                if pending.is_empty() {
                    self.stage = HostStage::Pinning { sent };
                    return self.pin(network, now_ms, sent);
                }
                if now_ms.saturating_sub(sent) >= SWITCH_LEAVE_MS {
                    for i in pending {
                        let human = &mut self.roster[i];
                        human.straggler = true;
                        // C7: it missed the switch. Tell it while its link
                        // still exists, then close it so the pin can run.
                        send_to_client(
                            network,
                            human.peer,
                            &ToClientMigration::Unprotected {
                                reason: format!(
                                    "{} did not switch to the armed room within 5 s.",
                                    human.name
                                )
                                .chars()
                                .take(200)
                                .collect(),
                            },
                        );
                        network.disconnect(human.peer);
                    }
                    self.stage = HostStage::Pinning { sent };
                }
                Ok(())
            }
            HostStage::Pinning { sent } => {
                let sent = *sent;
                self.keep_live()?;
                self.drain(network);
                self.pin(network, now_ms, sent)
            }
            HostStage::Routing { pinned } => {
                let pinned = *pinned;
                // The armed room exists and the listener is pinned: nothing
                // here returns to the lobby. A pair-link failure latches in
                // the runtime, which pauses (the standby lost); the window
                // still runs and the room is handed over.
                let (Some(live), Some(players)) = (self.live.as_mut(), self.players.as_mut())
                else {
                    unreachable!("Routing holds the armed room")
                };
                let failed = live
                    .probe()
                    .and_then(|()| live.poll())
                    .err()
                    .map(|e| format!("The pair link failed during the switch ({e})."))
                    .or_else(|| {
                        players
                            .poll(live, now_ms)
                            .err()
                            .map(|e| format!("The armed room failed during the switch ({e})."))
                    });
                if let Some(why) = failed {
                    if self.route_failure.is_none() {
                        log::warn!("[switch] {why} The armed room proceeds paused.");
                        self.route_failure = Some(why);
                    }
                }
                let missing: Vec<&RosterHuman> = self
                    .roster
                    .iter()
                    .filter(|h| !players.attached(h.player))
                    .collect();
                // A straggler never switched and will not re-dial: only the
                // others are waited for.
                let waiting = missing.iter().any(|h| !h.straggler);
                if waiting && now_ms.saturating_sub(pinned) < SWITCH_ROUTE_MS {
                    return Ok(());
                }
                let unprotected = missing
                    .into_iter()
                    .map(|h| UnprotectedPlayer {
                        player: h.player,
                        name: h.name.clone(),
                        reason: if h.straggler {
                            format!("{} did not switch to the armed room within 5 s.", h.name)
                        } else if let Some(why) = &self.route_failure {
                            // Routes are not polled once the armed room has
                            // failed, so a missing route says nothing about
                            // the client.
                            format!("{} could not join the armed room: {why}", h.name)
                        } else {
                            format!(
                                "{} left the lobby link but did not reconnect within 5 s.",
                                h.name
                            )
                        },
                    })
                    .collect::<Vec<_>>();
                for h in &unprotected {
                    log::info!("[switch] {}", h.reason);
                }
                // The ordinary links these events came on are replaced; the
                // armed room starts from the frozen image.
                self.superseded = std::mem::take(&mut self.held).len();
                self.held_bytes = 0;
                self.stage = HostStage::Done;
                self.state = SwitchState::Protected { unprotected };
                Ok(())
            }
            HostStage::Done => Ok(()),
        }
    }

    /// `Armed{armed, cert0}`, `Standby{addr, token}` and `Switch{token}` to
    /// every remote roster human, in that order on its ordered link. Every
    /// payload is checked to fit before anything is sent.
    fn send_bundle(
        &mut self,
        network: &mut QuicHost,
        cert: &MembershipCertificate,
    ) -> Result<(), String> {
        let live = self.live.as_ref().ok_or("The armed room is gone.")?;
        let root = live.root();
        let context = root
            .context()
            .map_err(|e| format!("The armed bundle could not be built ({e})."))?;
        let armed = self
            .armed
            .encode(&context)
            .map_err(|e| format!("The armed bundle could not be built ({e})."))?;
        let certificate = cert
            .encode(root)
            .map_err(|e| format!("The armed bundle could not be built ({e})."))?;
        // The listener's LAN token now, not the one each client joined with:
        // a lobby that filled up (or was otherwise closed and reopened)
        // rotated it since. Nothing changes the room's open state from here
        // to the pin (the lobby is frozen and the switch does not close it),
        // so this token still admits the re-dials then.
        let token = network
            .handle()
            .and_then(|h| unhex32(&h.lan_token()))
            .ok_or("The armed bundle could not be built (the listener has no LAN token).")?;
        let bundle = [
            ToClientMigration::Armed { armed, certificate },
            ToClientMigration::Standby {
                addr: self.standby.0.to_string(),
                token: self.standby.1,
            },
            ToClientMigration::Switch { token },
        ];
        for message in &bundle {
            chunk(message).map_err(|e| format!("The armed bundle does not fit ({e:?})."))?;
        }
        for human in &self.roster {
            for message in &bundle {
                send_to_client(network, human.peer, message);
            }
        }
        Ok(())
    }

    /// Keep the pair link probed and read while the ordinary links switch.
    fn keep_live(&mut self) -> Result<(), String> {
        let live = self.live.as_mut().ok_or("The armed room is gone.")?;
        live.probe()
            .and_then(|()| live.poll())
            .map_err(|e| format!("The pair link failed during the switch ({e})."))?;
        Ok(())
    }

    /// Read the ordinary listener until the pin: roster leaves are marked,
    /// anyone else who joined is closed (never seated in the armed room),
    /// and lossy traffic is dropped. A roster `Left` and roster reliable
    /// traffic are held with the lobby's events (reliable within the
    /// boundary's bounds; a `Left` always), so a failed switch replays them.
    fn drain(&mut self, network: &mut QuicHost) {
        let mut closed = BTreeSet::new();
        for _ in 0..MAX_BATCH {
            let Some(event) = network.poll() else { break };
            match &event {
                HostNetEvent::Left { peer } => {
                    if let Some(human) = self.roster.iter_mut().find(|h| h.peer == *peer) {
                        human.left = true;
                        self.held.push_back(event);
                    }
                }
                HostNetEvent::Joined { peer, .. } => {
                    let peer = *peer;
                    if !self.roster.iter().any(|h| h.peer == peer) && closed.insert(peer) {
                        log::info!("[switch] peer {peer} joined after Armed; closed");
                        self.refused_joins += 1;
                        network.disconnect(peer);
                    }
                }
                HostNetEvent::Reliable { peer, bytes } => {
                    let roster = self.roster.iter().any(|h| h.peer == *peer);
                    let size = bytes.len();
                    if roster
                        && self.held.len() < MAX_JOURNAL_RECORDS
                        && self.held_bytes + size <= MAX_JOURNAL_BYTES
                    {
                        self.held_bytes += size;
                        self.held.push_back(event);
                    }
                }
                HostNetEvent::Lossy { .. } => {}
            }
        }
    }

    /// Pin the listener, retried: Fray refuses while an ordinary link is
    /// still seated (a pending ordinary handshake does not hold it off: the
    /// pin closes it as a closed room), and until cert0 is the installed
    /// receipt's. A refusal hands the listener back.
    fn pin(&mut self, network: &mut QuicHost, now_ms: u64, sent: u64) -> Result<(), String> {
        if self.roster.iter().any(|h| !h.left) {
            // A closed straggler's `Left` is still on its way.
            return self.pin_deadline(now_ms, sent);
        }
        if self.refuse_pins {
            if self.pin_refusals.last() != Some(&Error::Unauthorized) {
                self.pin_refusals.push(Error::Unauthorized);
            }
            return self.pin_deadline(now_ms, sent);
        }
        let live = self.live.as_mut().ok_or("The armed room is gone.")?;
        match PlayerHost::new(network.take_listener(), &self.descriptor, live) {
            Ok(players) => {
                // Pinned: from here the armed room is handed over whatever
                // happens, so no error below returns to the lobby.
                self.players = Some(players);
                self.stage = HostStage::Routing { pinned: now_ms };
                // No roster human has a route yet: park each on the runner
                // clock now. `PARKED_DEPARTURE_MS` then departs one that
                // never attaches (a straggler, or a client that left the
                // lobby link and never re-dialled); attaching cancels it, as
                // a proved reconnect does.
                for human in &self.roster {
                    if let Err(e) = live.park(human.player) {
                        let why = format!("The armed room could not park a player ({e}).");
                        log::warn!("[switch] {why}");
                        self.route_failure.get_or_insert(why);
                    }
                }
                Ok(())
            }
            Err(PlayerHostRefused {
                error,
                network: back,
            }) => {
                *network = back;
                if self.pin_refusals.last() != Some(&error) {
                    self.pin_refusals.push(error);
                }
                self.pin_deadline(now_ms, sent)
            }
        }
    }

    fn pin_deadline(&self, now_ms: u64, sent: u64) -> Result<(), String> {
        if now_ms.saturating_sub(sent) >= SWITCH_LEAVE_MS + SWITCH_ROUTE_MS {
            return Err(format!(
                "The room's listener could not be pinned to the armed session ({:?}).",
                self.pin_refusals.last()
            ));
        }
        Ok(())
    }
}

/// The bundle a client verified on `Switch`, ready to re-dial.
pub(super) struct VerifiedSwitch {
    pub(super) descriptor: Descriptor,
    pub(super) armed: Armed,
    pub(super) membership: MembershipCertificate,
    pub(super) capability: CapabilityAck,
    pub(super) standby: Option<(SocketAddr, [u8; 32])>,
    /// The host listener's LAN token from `Switch`, for the re-dial (the
    /// address is always the one this device joined).
    pub(super) token: [u8; 32],
}

/// Verify the armed bundle against this device's acknowledged setup: the
/// descriptor it verified, the initial hash and the exact ACK it signed.
/// Nothing is closed or dialled before this passes.
pub(super) fn verify_bundle(
    acked: &(Vec<u8>, [u8; 32], Vec<u8>),
    local: [u8; 32],
    armed: &[u8],
    certificate: &[u8],
    standby: Option<&(String, [u8; 32])>,
    token: [u8; 32],
) -> Result<VerifiedSwitch, String> {
    let (descriptor, initial, ack) = acked;
    let bad = |what: &str, e: Error| format!("The armed bundle was refused: {what} ({e}).");
    let descriptor = Descriptor::decode(descriptor).map_err(|e| bad("descriptor", e))?;
    descriptor.verify().map_err(|e| bad("descriptor", e))?;
    let root = &descriptor.root;
    let context = root.context().map_err(|e| bad("descriptor", e))?;
    let capability = CapabilityAck::decode(root, ack).map_err(|e| bad("capability ACK", e))?;
    if capability.initial != *initial {
        return Err("The armed bundle was refused: the ACK names another image.".into());
    }
    let armed = Armed::decode(&context, armed).map_err(|e| bad("Armed", e))?;
    // No independent initial logical time exists on this device: `Setup`
    // carries only the initial hash, and the lobby session never learns the
    // frozen image's exact time. So the record's own `logical_ms` is passed
    // and that one comparison proves nothing; the receipt's state is still
    // bound to the hash this device acknowledged, and that image encodes its
    // own logical time.
    armed
        .verify_initial(root, *initial, armed.receipt.logical_ms)
        .map_err(|e| bad("Armed", e))?;
    let membership =
        MembershipCertificate::decode(root, certificate).map_err(|e| bad("certificate", e))?;
    membership.verify(root).map_err(|e| bad("certificate", e))?;
    let same_receipt = membership
        .receipt
        .digest(&context)
        .and_then(|m| Ok(m == armed.receipt.digest(&context)?))
        .map_err(|e| bad("certificate", e))?;
    let same_roster = membership.members.len() == root.members.len()
        && membership.members.iter().all(|m| {
            root.members
                .iter()
                .any(|r| r.player == m.player && r.seat == m.seat && r.key == m.key)
        });
    let mine = membership
        .members
        .iter()
        .any(|m| m.player == capability.player && m.key == Some(local));
    if !same_receipt || !same_roster || !mine {
        return Err(
            "The armed bundle was refused: the certificate is not the armed roster's.".into(),
        );
    }
    // LAN-only in v0.1: only a private-network standby is ever dialled.
    let standby = standby.and_then(|(addr, token)| {
        addr.parse::<SocketAddr>()
            .ok()
            .filter(|a| is_lan_ip(a.ip()) && a.port() != 0)
            .map(|a| (a, *token))
    });
    Ok(VerifiedSwitch {
        descriptor,
        armed,
        membership,
        capability,
        standby,
        token,
    })
}

enum ClientStage {
    /// Re-dialling the original; the session waits for its route.
    Joining {
        join: PinnedJoin,
        client: Box<ClientSession>,
    },
    /// The route is up; preparing the standby link.
    Preparing {
        join: PinnedJoin,
        player: Box<RemotePlayer>,
    },
    /// The re-dial failed: rejoin the ordinary lobby at `at`.
    Rejoining {
        client: Box<ClientSession>,
        at: Instant,
        reason: String,
    },
    Done,
}

/// A client's switch, from [`super::ClientArming::take_switch`]: the
/// ordinary link is already closed.
///
/// Drive it every frame with [`ClientSwitch::poll`] until it is finished,
/// then take the route with [`ClientSwitch::take_player`] (it exists even
/// when the standby link could not be prepared), or, if the re-dial failed
/// or never completed, a session with [`ClientSwitch::take_client`]. That is
/// a fresh session rejoining the ordinary lobby at the same address, with the
/// token from `Switch` and then, if that join fails, the join's own token,
/// dialled [`SWITCH_REJOIN_GRACE_MS`] after the switch budget: a host
/// whose switch failed has thawed its lobby by then, so the player is back
/// in the unarmed lobby (and if the host did pin, the pinned listener
/// refuses the rejoin and the session fails; that device rejoins when the
/// match ends).
pub struct ClientSwitch {
    cfg: NetConfig,
    identity: Identity,
    verified: VerifiedSwitch,
    /// The standby device's own seat shares the standby runtime's sender.
    sender: Option<SenderSequence>,
    deadline: Instant,
    /// The original's LAN address and token this device joined with, for
    /// the ordinary rejoin (which tries the token from `Switch` first).
    original: (SocketAddr, String),
    stage: ClientStage,
    state: SwitchState,
    player: Option<RemotePlayer>,
    client: Option<ClientSession>,
    /// The standby device's own session as it stood at the switch, kept
    /// after its re-dial failed (see [`Self::take_seat`]).
    seat: Option<ClientSession>,
    /// Test hook: the re-dial fails on its next poll.
    fail_join: bool,
}

/// The ordinary rejoin after a failed re-dial, as a [`ClientTransport`]: a
/// join at the device's own join address with the token from `Switch`; if
/// that join fails before it connects, one more join at the same address
/// with the token the device joined with (skipped when the two are equal).
/// It never dials any other address.
struct FallbackRejoin {
    link: QuicClient,
    /// The second join, until the first connects or fails.
    next: Option<(NetConfig, Identity, SocketAddr, String)>,
}

impl FallbackRejoin {
    fn start(
        cfg: &NetConfig,
        identity: &Identity,
        addr: SocketAddr,
        switch_token: String,
        join_token: String,
    ) -> Self {
        let link = QuicClient::new(decented_net::join_addr(cfg, identity, addr, &switch_token));
        let next =
            (join_token != switch_token).then(|| (cfg.clone(), identity.clone(), addr, join_token));
        Self { link, next }
    }
}

impl ClientTransport for FallbackRejoin {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        match self.link.poll()? {
            ClientNetEvent::Connected { host } => {
                self.next = None;
                Some(ClientNetEvent::Connected { host })
            }
            ClientNetEvent::Failed(why) => match self.next.take() {
                Some((cfg, identity, addr, token)) => {
                    log::info!(
                        "[switch] the rejoin with the switch token failed ({why}); \
                         retrying with the join token"
                    );
                    self.link.close();
                    self.link =
                        QuicClient::new(decented_net::join_addr(&cfg, &identity, addr, &token));
                    Some(ClientNetEvent::Progress("Rejoining the lobby".into()))
                }
                None => Some(ClientNetEvent::Failed(why)),
            },
            event => Some(event),
        }
    }

    fn send_reliable(&mut self, bytes: Vec<u8>) {
        self.link.send_reliable(bytes)
    }

    fn send_lossy(&mut self, bytes: Vec<u8>) {
        self.link.send_lossy(bytes)
    }

    fn rtt_ms(&self) -> Option<u32> {
        self.link.rtt_ms()
    }

    fn close(&mut self) {
        self.next = None;
        self.link.close()
    }
}

impl ClientSwitch {
    pub(super) fn start(
        cfg: &NetConfig,
        identity: &Identity,
        verified: VerifiedSwitch,
        original: (SocketAddr, String),
        client: ClientSession,
        standby: Option<&LiveStandby>,
    ) -> Self {
        let deadline = Instant::now() + Duration::from_millis(SWITCH_JOIN_MS);
        let is_standby = verified.descriptor.root.standby == identity.pubkey().to_bytes();
        let mut switch = Self {
            cfg: cfg.clone(),
            identity: identity.clone(),
            sender: None,
            deadline,
            original: original.clone(),
            stage: ClientStage::Done,
            state: SwitchState::Switching,
            player: None,
            client: None,
            seat: None,
            fail_join: false,
            verified,
        };
        if is_standby {
            // Its seat is the standby runtime's: the same armed record, and
            // its sender shared with the pair link.
            match standby {
                Some(live) if live.armed() == &switch.verified.armed => {
                    switch.sender = Some(live.sender_sequence())
                }
                _ => {
                    switch.client = Some(client);
                    switch.state = SwitchState::Unprotected {
                        reason: "The standby's own seat needs the running standby.".into(),
                    };
                    return switch;
                }
            }
        }
        let (cfg, id, descriptor) = (
            switch.cfg.clone(),
            switch.identity.clone(),
            switch.verified.descriptor.clone(),
        );
        // The address this device joined, never one the host names; the
        // token is the host's current one from `Switch`, received over the
        // admitted host link.
        let addr = original.0;
        let token = hex(&switch.verified.token);
        let join = PinnedJoin::until(deadline, ATTEMPT_DEADLINE, move || {
            join_player_route(&cfg, &id, addr, &token, &descriptor)
        });
        switch.stage = ClientStage::Joining {
            join,
            client: Box::new(client),
        };
        switch
    }

    pub fn state(&self) -> &SwitchState {
        &self.state
    }

    /// The device's armed route, once finished with a route.
    pub fn take_player(&mut self) -> Option<RemotePlayer> {
        self.player.take()
    }

    /// The session when no route was built: after a failed re-dial, a fresh
    /// session rejoining the ordinary lobby.
    pub fn take_client(&mut self) -> Option<ClientSession> {
        self.client.take()
    }

    /// The end of this switch's re-dial budget ([`SWITCH_JOIN_MS`] from its
    /// start). A failed re-dial gives up [`SWITCH_REJOIN_GRACE_MS`] after it.
    #[doc(hidden)]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The standby device only, after its re-dial failed: its own session as
    /// it stood at the switch, still holding its native seat. Its route never
    /// attached, so it has no journals. A host that pinned keeps that seat a
    /// member until its parked departure, and a takeover in that time must
    /// retain it (see `OwnSeat::new`).
    pub fn take_seat(&mut self) -> Option<ClientSession> {
        self.seat.take()
    }

    /// Test hook: fail the session-bound re-dial on its next poll (as a
    /// device whose network drops for the whole switch would).
    #[doc(hidden)]
    pub fn fail_join_for_test(&mut self) {
        self.fail_join = true;
    }

    pub fn poll(&mut self) -> &SwitchState {
        if let Err(reason) = self.step() {
            log::warn!("[switch] this device is unprotected: {reason}");
            match std::mem::replace(&mut self.stage, ClientStage::Done) {
                ClientStage::Joining { client, .. } | ClientStage::Rejoining { client, .. } => {
                    self.client = Some(*client)
                }
                ClientStage::Preparing { player, .. } => self.player = Some(*player),
                ClientStage::Done => {}
            }
            self.state = SwitchState::Unprotected { reason };
        }
        &self.state
    }

    fn step(&mut self) -> Result<(), String> {
        match &mut self.stage {
            ClientStage::Joining { join, .. } => {
                let polled = if self.fail_join {
                    PinnedJoinPoll::Failed("test".into())
                } else {
                    join.poll()
                };
                let link = match polled {
                    PinnedJoinPoll::Pending => return Ok(()),
                    PinnedJoinPoll::Failed(why) => {
                        let reason = format!("The armed room could not be rejoined ({why}).");
                        log::warn!("[switch] {reason} Rejoining the lobby unarmed.");
                        let ClientStage::Joining { client, .. } =
                            std::mem::replace(&mut self.stage, ClientStage::Done)
                        else {
                            unreachable!()
                        };
                        // Not before the host can have given up: it thaws
                        // its lobby at the same budget from its `Switch`.
                        let at = Instant::now().max(self.deadline)
                            + Duration::from_millis(SWITCH_REJOIN_GRACE_MS);
                        self.stage = ClientStage::Rejoining { client, at, reason };
                        return Ok(());
                    }
                    PinnedJoinPoll::Connected(link) => link,
                };
                let ClientStage::Joining { client, .. } =
                    std::mem::replace(&mut self.stage, ClientStage::Done)
                else {
                    unreachable!()
                };
                let v = &self.verified;
                let keys = self.identity.keys().clone();
                let player = RemotePlayer::connect_with_sender(
                    v.descriptor.clone(),
                    &v.armed,
                    &v.capability,
                    v.membership.clone(),
                    link,
                    *client,
                    keys,
                    self.sender.clone(),
                )
                .map_err(|e| format!("The armed route was refused ({e})."))?;
                if self.sender.is_some() {
                    // The standby device is protected by its role.
                    self.player = Some(player);
                    self.state = SwitchState::Protected {
                        unprotected: Vec::new(),
                    };
                    return Ok(());
                }
                let Some((addr, token)) = v.standby else {
                    self.player = Some(player);
                    return Err("The host sent no standby address on the local network.".into());
                };
                let (cfg, id, descriptor) = (
                    self.cfg.clone(),
                    self.identity.clone(),
                    v.descriptor.clone(),
                );
                let token = hex(&token);
                let join = PinnedJoin::until(self.deadline, ATTEMPT_DEADLINE, move || {
                    join_standby_route(&cfg, &id, addr, &token, &descriptor)
                });
                self.stage = ClientStage::Preparing {
                    join,
                    player: Box::new(player),
                };
                Ok(())
            }
            ClientStage::Preparing { join, .. } => {
                let link = match join.poll() {
                    PinnedJoinPoll::Pending => return Ok(()),
                    PinnedJoinPoll::Failed(why) => {
                        return Err(format!("The standby link could not be prepared ({why})."))
                    }
                    PinnedJoinPoll::Connected(link) => link,
                };
                let ClientStage::Preparing { mut player, .. } =
                    std::mem::replace(&mut self.stage, ClientStage::Done)
                else {
                    unreachable!()
                };
                let prepared = player.prepare_standby(link);
                self.player = Some(*player);
                prepared.map_err(|e| format!("The standby link could not be prepared ({e})."))?;
                self.state = SwitchState::Protected {
                    unprotected: Vec::new(),
                };
                Ok(())
            }
            ClientStage::Rejoining { at, .. } => {
                if Instant::now() < *at {
                    return Ok(());
                }
                let ClientStage::Rejoining { client, reason, .. } =
                    std::mem::replace(&mut self.stage, ClientStage::Done)
                else {
                    unreachable!()
                };
                // An ordinary join, as the lobby join was: the fresh
                // session says Hello on `Connected` and is seated natively.
                // The listener's token from `Switch` first (the lobby may
                // have rotated it since the join, as a full lobby does), then
                // the join's own token: a wrong `Switch` token cannot strand
                // the device. Both dial only the address it joined. (A full
                // lobby's thaw reopens the room but keeps the `Switch` token
                // in the rejoin window; see the module docs.)
                let (addr, join_token) = self.original.clone();
                let rejoin = FallbackRejoin::start(
                    &self.cfg,
                    &self.identity,
                    addr,
                    hex(&self.verified.token),
                    join_token,
                );
                self.client = Some(client.rejoin(Box::new(rejoin)));
                if self.sender.is_some() {
                    self.seat = Some(*client);
                }
                self.state = SwitchState::Unprotected {
                    reason: format!("{reason} Rejoining the lobby unarmed."),
                };
                Ok(())
            }
            ClientStage::Done => Ok(()),
        }
    }
}
