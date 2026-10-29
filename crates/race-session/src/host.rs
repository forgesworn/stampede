//! [`HostAuthority`]: the lobby and the race, host side (spec 10).
//!
//! The host runs `World`; bots live in it. The host's own player is an
//! ordinary [`crate::ClientSession`] on a zero-latency loopback link (see
//! [`crate::MuxHostTransport`]). Driven as `update(now_ms)` with any monotonic
//! millisecond clock; the clients sync to it.
//!
//! Timeline: world tick `t` happens at host time `start_host_ms + t * 1000 / 60`
//! and consumes input number `t + 1` of every human seat. Clients send inputs
//! ahead of that by about half their RTT plus the 2-tick jitter buffer, so an
//! input normally sits in the queue for 2 ticks before it is used. A tick
//! whose input never arrived repeats the seat's last applied input as it was
//! (buttons are held levels the sim edge-detects, so a repeat can never fire a
//! press; clearing them would make the next real input fire it again). A late
//! input for a starved tick is otherwise dropped, but a jump/item/bash press
//! in it that the repeat missed fires on the next tick, once.

use std::collections::{BTreeMap, HashMap, VecDeque};
use serde::{Serialize, Deserialize};

#[cfg(feature = "authority")]
#[path = "authority.rs"]
pub mod authority;
#[path = "checkpoint.rs"]
pub(crate) mod checkpoint;
#[path = "migration/wire.rs"]
pub mod migration_wire;
#[cfg(feature = "native")]
#[path = "migration.rs"]
pub mod migration;

use race_core::math::{mix64, TICK_HZ};
use race_core::{
    custom, BotLevel, CustomTrack, Event, Input, RaceConfig, RaceMode, RacePhase, RacerKind, RacerSetup, TrackId, World, CHARACTERS,
};

use crate::proto::{
    self, chunk_events, decode, encode, is_wire_event, pack_bits, sanitize_name, BattleNet, BattleSetup, HazNet,
    KeepPass, LobbyPhase, LobbyView,
    LossyToHost, ProjNet, RematchMsg, SeatView, Snapshot, ToClient, ToHost, MAX_LOSSY_LEN,
    MAX_RELIABLE_LEN, MAX_SEATS, PROTO_VERSION, SNAPSHOT_EVERY_TICKS, SNAP_OVER,
};
use crate::view::{rank_leavers_last, results_from_world};
use crate::transport::{HostNetEvent, HostTransport, PeerKey, LOCAL_PEER_BASE};
use crate::proto::ResultsView;
use crate::track_transfer::{SelectedTrack, CHUNK_BYTES};
use crate::proto::{ToClientMigration, ToHostMigration};
use migration_wire::{MigrationEvent, Reassembler, WireError};

/// Host-clock lead between `RaceStart` and tick 0, so every client has the
/// message before the grid countdown begins.
pub const START_LEAD_MS: u64 = 700;
/// The rematch 3-2-1.
pub const REMATCH_MS: u64 = 3000;
/// A seat that sends nothing for this long mid-race is dropped to a bot.
pub const SILENCE_MS: u64 = 6000;
/// Most world ticks stepped in one `update` (a stalled app catches up gradually).
const MAX_CATCH_UP: u32 = 12;
/// Inputs queued per seat at most (anything older than the next tick is dropped too).
const MAX_QUEUED: usize = 120;
/// WallHit events per racer are sent at most this often.
const WALL_HIT_GAP_MS: u64 = 250;
/// Starved ticks remembered per seat (for presses that arrive late).
const STARVED_MEMORY: u32 = 32;
/// The edge-detected buttons in `Input::pack`'s byte 2: jump, item, bash.
const EDGE_BITS: u8 = (1 << 2) | (1 << 4) | (1 << 5);
/// Human seat ids are below this; bot seats are `BOT_ID_BASE + racer index`.
pub const BOT_ID_BASE: u8 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostPhase {
    Lobby,
    /// From `RaceStart` to the results: the grid countdown and the race.
    Racing,
    Results,
}

/// Lobby settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostConfig {
    pub track: TrackId,
    /// 1..=5 (custom tracks allow 1..=9).
    pub laps: u8,
    /// Bots wanted, 0..=7. Fewer sit when humans need the seats.
    pub bots: u8,
    pub bot_level: BotLevel,
    /// Base seed; each race derives its own.
    pub seed: u64,
    /// Start the race when every human is ready (and again for rematches).
    pub auto_start: bool,
}

#[cfg(feature = "native")]
impl Default for HostConfig {
    fn default() -> Self {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        HostConfig {
            track: TrackId::Savanna,
            laps: 3,
            bots: 5,
            bot_level: BotLevel::Normal,
            seed: u64::from_le_bytes(b),
            auto_start: true,
        }
    }
}

/// Counters for tests and debug overlays.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostStats {
    pub snapshots_sent: u64,
    pub snapshot_bytes_max: usize,
    /// Eggs/cacti/coins dropped to keep a snapshot inside the budget.
    pub entities_dropped: u64,
    /// Ticks where a seat's input had not arrived (its last input was repeated).
    pub starved_ticks: u64,
    pub ticks: u64,
}

#[derive(Serialize, Deserialize)]
struct Human {
    id: u8,
    peer: PeerKey,
    name: String,
    character: u8,
    attire: [u8; 3],
    ready: bool,
    custom_ready: Option<u64>,
    host: bool,
    last_heard: u64,
    /// Racer index in the current race.
    racer: Option<usize>,
    inq: BTreeMap<u32, [u8; 4]>,
    /// The packed input the world last used for this seat (real, repeated, or
    /// with a late press added).
    last_fed: [u8; 4],
    applied_seq: u32,
    /// Recent starved ticks: (input number, edge buttons the world was fed).
    starved: std::collections::VecDeque<(u32, u8)>,
    /// Presses found in late inputs, fed on the next tick.
    pending_press: u8,
}

impl Human {
    fn reset_race_input(&mut self) {
        self.inq.clear();
        self.applied_seq = 0;
        self.last_fed = Input::default().pack();
        self.starved.clear();
        self.pending_press = 0;
    }

    /// The packed input for input number `want` (world tick `want - 1`) and
    /// whether it was missing.
    fn feed(&mut self, want: u32) -> ([u8; 4], bool) {
        let (mut fed, starved) = match self.inq.remove(&want) {
            Some(p) => (p, false),
            None => (self.last_fed, true),
        };
        fed[2] |= std::mem::take(&mut self.pending_press) & EDGE_BITS;
        // Whatever is held now covers presses still missing from earlier gaps.
        for e in self.starved.iter_mut() {
            e.1 |= fed[2] & EDGE_BITS;
        }
        if starved {
            self.starved.push_back((want, fed[2] & EDGE_BITS));
        }
        while self.starved.front().is_some_and(|e| e.0 + STARVED_MEMORY < want) {
            self.starved.pop_front();
        }
        // Late arrivals for ticks already gone.
        while let Some(&k) = self.inq.keys().next() {
            if k <= want {
                self.inq.remove(&k);
            } else {
                break;
            }
        }
        self.applied_seq = want;
        self.last_fed = fed;
        (fed, starved)
    }

    /// A late copy of input `n` (already applied): a press the world did not
    /// see then fires on the next tick, at most once.
    fn late_input(&mut self, n: u32, p: [u8; 4]) {
        if let Some(e) = self.starved.iter_mut().find(|e| e.0 == n) {
            self.pending_press |= p[2] & !e.1 & EDGE_BITS;
            e.1 |= p[2] & EDGE_BITS;
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Run {
    world: World,
    start_ms: u64,
    events: Vec<Event>,
    wall_last: [u64; MAX_SEATS],
    results: Option<ResultsView>,
    results_pending: bool,
    last_snap_tick: u64,
    /// Racers whose player left before finishing, in the order they left.
    left: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct HostAuthority {
    #[serde(skip)]
    migration_custom:Option<Vec<Vec<u8>>>,
    headless: bool,
    le_mans: bool,
    #[serde(skip, default = "checkpoint::null_transport")]
    transport: Box<dyn HostTransport>,
    cfg: HostConfig,
    phase: HostPhase,
    #[serde(deserialize_with="fray_session::checkpoint::bounded_vec::<_,_,8>")]
    humans: Vec<Human>,
    #[serde(with = "checkpoint::peer_map")]
    by_peer: HashMap<PeerKey, u8>,
    next_id: u8,
    now: u64,
    races: u64,
    run: Option<Run>,
    rematch_at: Option<u64>,
    lobby_dirty: bool,
    rematch_dirty: bool,
    open: Option<bool>,
    stats: HostStats,
    /// `Some`: the next race is a battle (lobby option; kept for rematches).
    battle: Option<BattleSetup>,
    #[serde(skip)]
    custom: Option<SelectedTrack>,
    /// Host-migration setup reassembly, per seated peer. Link state, not
    /// authority state: never checkpointed.
    #[serde(skip)]
    migration_rx: HashMap<PeerKey, Reassembler>,
    #[serde(skip)]
    migration_events: VecDeque<(PeerKey, MigrationEvent<ToHostMigration>)>,
    #[serde(skip)]
    migration_rejected: u64,
    #[serde(skip)]
    migration_dropped: u64,
    /// The frozen lobby boundary of host-migration arming: while set,
    /// [`HostAuthority::update`] reads nothing and changes nothing. The arming
    /// coordinator reads the transport beside the authority and hands the
    /// held events back when the boundary ends. Link state: never checkpointed.
    #[serde(skip)]
    migration_frozen: bool,
    /// Events held at the frozen boundary, handed back when it ended; the
    /// next `update` reads them before anything newer from the transport.
    #[serde(skip)]
    migration_held: VecDeque<HostNetEvent>,
    /// Until this time (the authority's `now`), opening or closing the lobby
    /// keeps the listener's LAN token instead of rotating it: set by a
    /// host-migration switch that failed before the pin, whose `Switch`
    /// handed the clients the current token for their fallback rejoin. Link
    /// state: in no checkpoint.
    #[serde(skip)]
    lan_token_held_until: Option<u64>,
    /// A hold asked for (its length, ms) and not started yet: it starts at the
    /// next `update`, on the clock that `update` is given.
    #[serde(skip)]
    lan_token_hold_ms: Option<u64>,
}

impl HostAuthority {
    pub fn new(transport: Box<dyn HostTransport>, mut cfg: HostConfig) -> HostAuthority {
        cfg.laps = cfg.laps.clamp(1, 5);
        cfg.bots = cfg.bots.min(7);
        // A custom track must be selected through the validating setter.
        if cfg.track == TrackId::Custom { cfg.track = TrackId::Savanna; }
        HostAuthority {
            migration_custom:None,
            headless: false,
            le_mans: false,
            transport,
            cfg,
            phase: HostPhase::Lobby,
            humans: Vec::new(),
            by_peer: HashMap::new(),
            next_id: 0,
            now: 0,
            races: 0,
            run: None,
            rematch_at: None,
            lobby_dirty: false,
            rematch_dirty: false,
            open: None,
            stats: HostStats::default(),
            battle: None,
            custom: None,
            migration_rx: HashMap::new(),
            migration_events: VecDeque::new(),
            migration_rejected: 0,
            migration_dropped: 0,
            migration_frozen: false,
            migration_held: VecDeque::new(),
            lan_token_held_until: None,
            lan_token_hold_ms: None,
        }
    }

    /// A dedicated authority with no local player. Permits eight bots and an
    /// explicitly started bot-only race. Automatic starts and rematches still
    /// wait for at least one human; leaving humans become bots mid-race.
    /// Pair this with `NetConfig::host_has_player = false` for a network host.
    pub fn new_headless(transport: Box<dyn HostTransport>, cfg: HostConfig) -> HostAuthority {
        let bots = cfg.bots.min(MAX_SEATS as u8);
        let mut host = Self::new(transport, cfg);
        host.headless = true;
        host.le_mans = true;
        host.cfg.bots = bots;
        host
    }

    // ─── Queries ─────────────────────────────────────────────────────────

    pub fn phase(&self) -> HostPhase {
        self.phase
    }

    pub fn config(&self) -> &HostConfig {
        &self.cfg
    }

    /// The battle the lobby has chosen (`None`: a race on `config().track`).
    pub fn battle(&self) -> Option<BattleSetup> {
        self.battle
    }

    pub fn stats(&self) -> &HostStats {
        &self.stats
    }

    /// The authoritative world while a race (or its results) exists.
    pub fn world(&self) -> Option<&World> {
        self.run.as_ref().map(|r| &r.world)
    }

    /// Test hook: the authoritative world, to hand a racer an item.
    #[cfg(test)]
    pub(crate) fn world_mut(&mut self) -> Option<&mut World> {
        self.run.as_mut().map(|r| &mut r.world)
    }

    /// The transport peer behind a seat (what [`HostAuthority::send_keep_pass`]
    /// and [`HostAuthority::remove_peer`] take).
    pub fn peer_of_seat(&self, seat: u8) -> Option<PeerKey> {
        self.humans.iter().find(|h| h.id == seat).map(|h| h.peer)
    }

    pub fn results(&self) -> Option<&ResultsView> {
        self.run.as_ref().and_then(|r| r.results.as_ref())
    }

    /// Racers whose player left the current race before finishing (listed
    /// as Left in its results).
    pub fn left_racers(&self) -> &[u8] {
        self.run.as_ref().map_or(&[], |r| r.left.as_slice())
    }

    /// Humans seated.
    pub fn human_count(&self) -> usize {
        self.humans.len()
    }

    /// How many of a human seat's input numbers above `after` the world ran
    /// starved (its last held input repeated), from the seat's recent
    /// starvation memory (the last 32 ticks). Read-only, for diagnostics.
    pub fn starved_since(&self, seat: u8, after: u32) -> Option<usize> {
        let h = self.humans.iter().find(|h| h.id == seat)?;
        Some(h.starved.iter().filter(|e| e.0 > after).count())
    }

    /// The racer index of a human seat (by [`SeatView::id`]) in the current race.
    pub fn racer_of(&self, seat: u8) -> Option<usize> {
        self.humans.iter().find(|h| h.id == seat).and_then(|h| h.racer)
    }

    /// Host-clock time the current race's tick 0 happens.
    pub fn start_host_ms(&self) -> Option<u64> {
        self.run.as_ref().map(|r| r.start_ms)
    }

    /// Round trip to a human seat, if the transport knows.
    pub fn rtt_ms(&self, seat: u8) -> Option<u32> {
        let h = self.humans.iter().find(|h| h.id == seat)?;
        self.transport.rtt_ms(h.peer)
    }

    /// When the rematch 3-2-1 ends (host clock), while one runs.
    pub fn rematch_ends_at(&self) -> Option<u64> {
        self.rematch_at
    }

    /// Bots that sit now: wanted, but humans come first.
    pub fn effective_bots(&self) -> usize {
        (self.cfg.bots as usize).min(MAX_SEATS.saturating_sub(self.humans.len()))
    }

    /// Whether a race could start now: a human (or bots on a dedicated host),
    /// and for a battle at least two
    /// participants (humans plus bots), since a lone ostrich "wins" at GO. The
    /// lobby should disable Start while this is false.
    pub fn can_start(&self) -> bool {
        (!self.humans.is_empty() || (self.headless && self.effective_bots() > 0))
            && (self.battle.is_none() || self.humans.len() + self.effective_bots() >= 2)
            && (self.cfg.track != TrackId::Custom || self.custom.as_ref().is_some_and(|t| {
                self.humans.iter().all(|h| h.custom_ready == Some(t.hash))
            }))
    }

    pub fn custom_track(&self) -> Option<&CustomTrack> {
        self.custom.as_ref().map(|t| &t.track)
    }

    /// Host-migration setup messages from seated peers since the last call,
    /// in arrival order: complete payloads, and records dropped as malformed.
    /// The host acts on none of them. At most 4 per peer are held between
    /// calls; a peer's queued events go when it leaves.
    pub fn take_migration_events(&mut self) -> Vec<(PeerKey, MigrationEvent<ToHostMigration>)> {
        self.migration_events.drain(..).collect()
    }

    /// Malformed host-migration setup records dropped so far, all peers.
    pub fn migration_rejected(&self) -> u64 {
        self.migration_rejected
    }

    /// Complete host-migration setup records dropped because their peer
    /// already had 4 events pending.
    pub fn migration_dropped(&self) -> u64 {
        self.migration_dropped
    }

    /// Send a host-migration setup payload to one peer, chunked into
    /// `ToClient::Migration` parts. Refuses a payload over four parts.
    #[cfg_attr(not(test), allow(dead_code))] // Until the lobby flow (slices 6.5-6.8) sends.
    pub(crate) fn send_migration(
        &mut self,
        peer: PeerKey,
        payload: &ToClientMigration,
    ) -> Result<(), WireError> {
        for part in migration_wire::chunk(payload)? {
            self.send(peer, &ToClient::Migration(part));
        }
        Ok(())
    }

    pub fn lobby_view(&self) -> LobbyView {
        let mut seats: Vec<SeatView> = self
            .humans
            .iter()
            .map(|h| SeatView {
                id: h.id,
                name: h.name.clone(),
                character: h.character,
                ready: h.ready,
                bot: false,
                host: h.host,
            })
            .collect();
        if self.phase == HostPhase::Lobby {
            // Bots take the racer indices after the humans when the race starts.
            let first = self.humans.len();
            for (i, (name, character)) in self.bot_identities().into_iter().enumerate() {
                let id = BOT_ID_BASE + (first + i) as u8;
                seats.push(SeatView { id, name, character, ready: true, bot: true, host: false });
            }
        } else if let Some(run) = &self.run {
            // In a race / results the roster is the race's: bots as they sit.
            for (i, s) in run.world.cfg.racers.iter().enumerate() {
                let human_here = self.humans.iter().any(|h| h.racer == Some(i));
                if !human_here {
                    seats.push(SeatView {
                        id: BOT_ID_BASE + i as u8,
                        name: s.name.clone(),
                        character: s.character,
                        ready: true,
                        bot: true,
                        host: false,
                    });
                }
            }
        }
        LobbyView {
            seats,
            track: self.cfg.track,
            custom: self.custom.as_ref().map(|t| t.hash),
            laps: self.cfg.laps,
            bots: self.cfg.bots,
            bot_level: self.cfg.bot_level,
            phase: match self.phase {
                HostPhase::Lobby => LobbyPhase::Lobby,
                HostPhase::Racing => LobbyPhase::Racing,
                HostPhase::Results => LobbyPhase::Results,
            },
            battle: self.battle,
        }
    }

    /// Names and characters of the bots that would sit now (distinct from the
    /// humans' characters where possible).
    fn bot_identities(&self) -> Vec<(String, u8)> {
        let mut used: Vec<u8> = self.humans.iter().map(|h| h.character).collect();
        let mut out = Vec::new();
        for _ in 0..self.effective_bots() {
            let c = (0..CHARACTERS.len() as u8)
                .find(|c| !used.contains(c))
                .unwrap_or((used.len() % CHARACTERS.len()) as u8);
            used.push(c);
            out.push((CHARACTERS[c as usize].name.to_string(), c));
        }
        out
    }

    // ─── Lobby controls (the host's UI) ──────────────────────────────────
    //
    // While host-migration arming holds the lobby frozen
    // ([`HostAuthority::migration_frozen`]), every control below is refused:
    // it returns `false` (or `Err`), changes nothing and sends nothing, so the
    // authority image stays the one the standby is armed with.

    /// The race track (not the Training Ground, not a battle arena).
    pub fn set_track(&mut self, track: TrackId) -> bool {
        if self.migration_frozen || self.phase != HostPhase::Lobby || track == TrackId::Training || track == TrackId::Custom || track.is_arena() {
            return false;
        }
        if self.custom.take().is_some() {
            for h in &mut self.humans { h.ready = false; h.custom_ready = None; }
        }
        self.cfg.track = track;
        self.cfg.laps = self.cfg.laps.clamp(1, 5);
        self.lobby_dirty = true;
        true
    }

    /// Validate and select a My Track, then transfer it to every human.
    /// No lobby settings change if validation fails.
    pub fn set_custom_track(&mut self, track: &CustomTrack) -> Result<(), String> {
        if self.migration_frozen {
            return Err("The lobby is held while the backup is set up; try again shortly.".into());
        }
        if self.phase != HostPhase::Lobby { return Err("Return to the lobby to change tracks.".into()); }
        let selected = SelectedTrack::new(track.clone())?;
        if self.custom.as_ref().is_some_and(|t| t.hash == selected.hash) { return Ok(()); }
        if let Some(pending)=self.migration_custom.as_mut() {
            if pending.len()>=fray_session::checkpoint::MAX_BATCH{return Err("Too many track changes in one update.".into());}
            pending.push(selected.bytes.clone());
        }else{custom::install(selected.def.clone());}
        self.cfg.track = TrackId::Custom;
        self.cfg.laps = track.laps;
        self.battle = None;
        self.custom = Some(selected);
        for h in &mut self.humans { h.ready = false; h.custom_ready = None; }
        // Reliable ordering: announce the hash before its first chunk.
        self.broadcast(&ToClient::Lobby(self.lobby_view()));
        let peers: Vec<_> = self.humans.iter().map(|h| h.peer).collect();
        for peer in peers { self.send_track(peer); }
        self.lobby_dirty = true;
        Ok(())
    }

    pub fn set_laps(&mut self, laps: u8) -> bool {
        if self.migration_frozen || self.phase != HostPhase::Lobby {
            return false;
        }
        self.cfg.laps = laps.clamp(1, if self.cfg.track == TrackId::Custom { 9 } else { 5 });
        self.lobby_dirty = true;
        true
    }

    /// Enable the playable run-and-jump start for subsequent races.
    pub fn set_le_mans_start(&mut self, on: bool) -> bool {
        if self.migration_frozen || self.phase != HostPhase::Lobby {
            return false;
        }
        self.le_mans = on;
        true
    }

    pub fn set_bots(&mut self, bots: u8) -> bool {
        if self.migration_frozen || self.phase != HostPhase::Lobby {
            return false;
        }
        self.cfg.bots = bots.min(if self.headless { MAX_SEATS as u8 } else { 7 });
        self.lobby_dirty = true;
        true
    }

    pub fn set_bot_level(&mut self, level: BotLevel) -> bool {
        if self.migration_frozen || self.phase != HostPhase::Lobby || level == BotLevel::Dummy {
            return false;
        }
        self.cfg.bot_level = level;
        self.lobby_dirty = true;
        true
    }

    /// Battle or race: `Some(setup)` makes the next race (and its rematches) a
    /// battle on `setup.arena` (one of `TrackId::ARENAS`) with `setup.rules`
    /// (clamped to sane values); `None` goes back to racing on the lobby's
    /// track. Lobby phase only.
    pub fn set_battle(&mut self, battle: Option<BattleSetup>) -> bool {
        if self.migration_frozen || self.phase != HostPhase::Lobby {
            return false;
        }
        if let Some(mut b) = battle {
            if !b.arena.is_arena() {
                return false;
            }
            let r = &mut b.rules;
            r.feathers = r.feathers.clamp(1, 9);
            r.max_feathers = r.max_feathers.clamp(r.feathers, 15);
            r.time_limit = r.time_limit.clamp(30, 900);
            // The snapshot carries invulnerability as u8 centiseconds (2.55 s).
            r.invuln_ds = r.invuln_ds.min(25);
            r.egg_respawn_ds = r.egg_respawn_ds.clamp(5, 200);
            if self.custom.take().is_some() {
                self.cfg.track = TrackId::Savanna;
                self.cfg.laps = self.cfg.laps.min(5);
                for h in &mut self.humans { h.ready = false; h.custom_ready = None; }
            }
            self.battle = Some(b);
        } else {
            self.battle = None;
        }
        self.lobby_dirty = true;
        true
    }

    /// Remove a human seat (`SeatView::id`). `false`: no such seat, or the
    /// lobby is frozen.
    pub fn kick(&mut self, seat: u8) -> bool {
        if self.migration_frozen {
            return false;
        }
        if let Some(i) = self.humans.iter().position(|h| h.id == seat) {
            let h = self.humans.remove(i);
            self.by_peer.remove(&h.peer);
            self.send(h.peer, &ToClient::Closed("kicked".into()));
            self.transport.kick(h.peer);
            self.after_departure(h.racer);
            return true;
        }
        false
    }

    /// Remove the human on `peer` without banning them (a Keep whose invite was
    /// revoked: a new invite lets them back). The player is told `Closed("kicked")`
    /// before the link drops, so they learn they were removed instead of seeing the
    /// connection end some other way. `false`: no such player, or the lobby is frozen.
    pub fn remove_peer(&mut self, peer: PeerKey) -> bool {
        if self.migration_frozen {
            return false;
        }
        let Some(id) = self.by_peer.remove(&peer) else { return false };
        self.migration_rx.remove(&peer);
        self.migration_events.retain(|(p, _)| *p != peer);
        self.send(peer, &ToClient::Closed("kicked".into()));
        self.transport.disconnect(peer);
        if let Some(i) = self.humans.iter().position(|h| h.id == id) {
            let h = self.humans.remove(i);
            self.after_departure(h.racer);
        }
        true
    }

    /// Send `pass` (the Keep's host key and an invite secret) to the seated human
    /// on `peer`, once after admission. An older client drops it unread; see
    /// [`ToClient::KeepPass`]. The Keep decides when and how often to call this.
    /// `false`: `peer` holds no seat here, or the lobby is frozen.
    pub fn send_keep_pass(&mut self, peer: PeerKey, pass: KeepPass) -> bool {
        if self.migration_frozen || !self.by_peer.contains_key(&peer) {
            return false;
        }
        self.send(peer, &ToClient::KeepPass(pass));
        true
    }

    /// Start the race now (the host's "go" when `auto_start` is off, or to
    /// ignore stragglers). Needs the lobby phase and a human, or bots on a dedicated host.
    pub fn start_race(&mut self, now_ms: u64) -> bool {
        if self.migration_frozen || self.phase != HostPhase::Lobby || !self.can_start() {
            return false;
        }
        self.now = self.now.max(now_ms);
        self.begin_race();
        true
    }

    /// From the results, back to the lobby (to change settings); every ready is cleared.
    /// `false`: already in the lobby, or the lobby is frozen.
    pub fn to_lobby(&mut self) -> bool {
        if self.migration_frozen || self.phase == HostPhase::Lobby {
            return false;
        }
        self.phase = HostPhase::Lobby;
        self.run = None;
        self.rematch_at = None;
        for h in &mut self.humans {
            h.ready = false;
            h.racer = None;
            h.reset_race_input();
        }
        self.lobby_dirty = true;
        true
    }

    /// Tell everyone the host is leaving and drop the links.
    pub fn shutdown(&mut self) {
        let peers: Vec<PeerKey> = self.humans.iter().map(|h| h.peer).collect();
        for p in peers {
            self.send(p, &ToClient::Closed("host left".into()));
        }
        self.transport.shutdown();
        self.humans.clear();
        self.by_peer.clear();
        self.migration_rx.clear();
        self.migration_events.clear();
        self.migration_held.clear();
    }

    // ─── The frame ───────────────────────────────────────────────────────

    /// One host frame at host-clock `now_ms`: read the network, step the world
    /// for every tick that is due, and send what changed.
    /// Drain reliable causes on a private restored authority at frozen C.
    /// Ordinary update also ticks World and evaluates automatic transitions;
    /// neither is permitted during recovery.
    #[cfg(feature = "native")]
    pub(crate) fn migration_recover_events(
        &mut self,
        events: &[HostNetEvent],
    ) -> decented_net::migration::Result<()> {
        use decented_net::migration::Error;
        if events.len() > decented_net::migration::MAX_JOURNAL_RECORDS {
            return Err(Error::Bounds);
        }
        let bytes = events.iter().try_fold(0usize, |n, event| {
            let len = match event {
                HostNetEvent::Reliable { bytes, .. } => bytes.len(),
                _ => 0,
            };
            n.checked_add(len).ok_or(Error::Bounds)
        })?;
        if bytes > decented_net::migration::MAX_JOURNAL_BYTES { return Err(Error::Bounds); }
        if events.iter().any(|event| !matches!(event,
            HostNetEvent::Reliable { peer, bytes } if self.by_peer.contains_key(peer)
                && bytes.len() <= MAX_RELIABLE_LEN
        ) && !matches!(event, HostNetEvent::Left { peer } if self.by_peer.contains_key(peer))) {
            return Err(Error::Unauthorized);
        }
        for event in events {
            match event {
                HostNetEvent::Reliable { peer, bytes } => self.on_reliable(*peer, bytes),
                HostNetEvent::Left { peer } => self.on_left(*peer),
                _ => unreachable!(),
            }
        }
        self.flush();
        Ok(())
    }

    pub fn update(&mut self, now_ms: u64) {
        if !self.begin_update(now_ms) { return; }
        while let Some(ev) = self.migration_held.pop_front() {
            self.on_net_event(ev);
        }
        while let Some(ev) = self.transport.poll() {
            self.on_net_event(ev);
        }

        self.finish_update();
    }

    pub(crate) fn begin_update(&mut self, now_ms: u64) -> bool {
        if self.migration_frozen {
            // Host-migration arming holds the lobby still (see
            // `migration::HostArming`): nothing is read, ticked or sent.
            return false;
        }
        self.now = self.now.max(now_ms);
        let now = self.now;
        if let Some(window) = self.lan_token_hold_ms.take() {
            self.lan_token_held_until = Some(now.saturating_add(window));
        }
        true
    }

    pub(crate) fn finish_update(&mut self) {
        let now = self.now;
        match self.phase {
            HostPhase::Lobby => {
                if self.cfg.auto_start && !self.humans.is_empty() && self.can_start() && self.humans.iter().all(|h| h.ready) {
                    self.begin_race();
                }
            }
            HostPhase::Racing | HostPhase::Results => {
                self.step_world(now);
                self.drop_silent(now);
            }
        }
        if self.phase == HostPhase::Results {
            self.eval_rematch();
            if let Some(t) = self.rematch_at {
                if now >= t {
                    self.rematch_at = None;
                    // A battle that lost its opponents (guests left, no bots)
                    // stays on the results screen.
                    if self.can_start() {
                        self.begin_race();
                    }
                }
            }
        }
        self.flush();
    }

    pub(crate) fn on_net_event(&mut self, ev: HostNetEvent) {
        match ev {
            HostNetEvent::Joined { peer, path } => {
                log::debug!("[host] peer {peer} linked via {path:?}");
                let _ = path;
            }
            HostNetEvent::Left { peer } => self.on_left(peer),
            HostNetEvent::Reliable { peer, bytes } => self.on_reliable(peer, &bytes),
            HostNetEvent::Lossy { peer, bytes } => self.on_lossy(peer, &bytes),
        }
    }

    /// Whether host-migration arming holds this lobby frozen.
    pub fn migration_frozen(&self) -> bool {
        self.migration_frozen
    }

    /// Enter the frozen lobby boundary (host-migration arming).
    pub(crate) fn migration_freeze(&mut self) {
        self.migration_frozen = true;
    }

    /// End the frozen lobby boundary. The events held while frozen reach the
    /// authority at the next `update`, in order and before anything newer, as
    /// `update` would have read them. Nothing is read or sent here, so this is
    /// safe while the caller holds the room's transport.
    pub(crate) fn migration_thaw(&mut self, held: impl IntoIterator<Item = HostNetEvent>) {
        self.migration_frozen = false;
        self.migration_held.extend(held);
    }

    /// A host-migration switch failed before the pin: for `window_ms` from
    /// the next `update` (on the clock `update` is given, which the thaw
    /// resumes), the lobby's opens and closes keep the LAN token its `Switch`
    /// handed the clients, so their fallback rejoin is admitted although the
    /// thaw's replayed `Left`s reopen a room that closed when it filled (and
    /// the last rejoin closes it again). Only in the lobby: the close when a
    /// race starts, and anything after the window, rotates the token as
    /// natively. Admission and the deny list still apply to every rejoin.
    pub(crate) fn hold_lan_token_for(&mut self, window_ms: u64) {
        self.lan_token_hold_ms = Some(window_ms);
    }

    // ─── Messages ────────────────────────────────────────────────────────

    fn send(&mut self, peer: PeerKey, m: &ToClient) {
        let bytes = encode(m);
        if bytes.is_empty() || bytes.len() > MAX_RELIABLE_LEN {
            log::error!("[host] reliable message of {} bytes not sent", bytes.len());
            return;
        }
        self.transport.send_reliable(peer, bytes);
    }

    fn broadcast(&mut self, m: &ToClient) {
        let bytes = encode(m);
        if bytes.is_empty() || bytes.len() > MAX_RELIABLE_LEN {
            log::error!("[host] broadcast of {} bytes not sent", bytes.len());
            return;
        }
        let peers: Vec<PeerKey> = self.humans.iter().map(|h| h.peer).collect();
        for p in peers {
            self.transport.send_reliable(p, bytes.clone());
        }
    }

    fn send_track(&mut self, peer: PeerKey) {
        let Some(t) = self.custom.as_ref() else { return };
        let total = t.bytes.len().div_ceil(CHUNK_BYTES) as u8;
        let messages: Vec<_> = t.bytes.chunks(CHUNK_BYTES).enumerate().map(|(idx, bytes)| {
            ToClient::TrackData { hash: t.hash, idx: idx as u8, total, bytes: bytes.to_vec() }
        }).collect();
        for m in messages { self.send(peer, &m); }
    }

    fn on_reliable(&mut self, peer: PeerKey, bytes: &[u8]) {
        if bytes.len() > MAX_RELIABLE_LEN { return; }
        let Some(msg) = decode::<ToHost>(bytes) else {
            log::debug!("[host] undecodable reliable message from {peer}");
            return;
        };
        let now = self.now;
        if let ToHost::Hello { proto, name, character } = msg {
            if !self.by_peer.contains_key(&peer) {
                self.on_hello(peer, proto, &name, character);
            }
            return;
        }
        let Some(&id) = self.by_peer.get(&peer) else { return };
        // Migration parts come before any liveness stamp: they never touch the
        // authority image and never keep a silent peer seated.
        if let ToHost::Migration(part) = msg {
            if self.humans.iter().any(|h| h.id == id) {
                self.on_migration_part(peer, part);
            }
            return;
        }
        let Some(h) = self.humans.iter_mut().find(|h| h.id == id) else { return };
        h.last_heard = now;
        match msg {
            ToHost::Hello { .. } => {}
            ToHost::SetAttire(attire) => {
                if self.phase == HostPhase::Lobby { h.attire = attire.map(|v| v.min(3)); }
            }
            ToHost::SetCharacter(c) => {
                if self.phase == HostPhase::Lobby && (c as usize) < CHARACTERS.len() && h.character != c {
                    h.character = c;
                    self.lobby_dirty = true;
                }
            }
            ToHost::SetReady(r) => {
                let track_ready = self.cfg.track != TrackId::Custom
                    || self.custom.as_ref().is_some_and(|t| h.custom_ready == Some(t.hash));
                if self.phase == HostPhase::Lobby && h.ready != r && (!r || track_ready) {
                    h.ready = r;
                    self.lobby_dirty = true;
                }
            }
            ToHost::TrackReady { hash } => {
                if self.custom.as_ref().is_some_and(|t| t.hash == hash) && h.custom_ready != Some(hash) {
                    h.custom_ready = Some(hash);
                    self.lobby_dirty = true;
                    self.rematch_dirty = true;
                }
            }
            ToHost::RematchReady(r) => {
                if self.phase == HostPhase::Results && h.ready != r {
                    h.ready = r;
                    self.lobby_dirty = true;
                    self.rematch_dirty = true;
                }
            }
            ToHost::Leave => {
                self.on_left(peer);
                self.transport.disconnect(peer);
            }
            ToHost::Ping { id, client_t } => {
                self.send(peer, &ToClient::Pong { id, client_t, host_t: now });
            }
            ToHost::Migration(_) => {} // Handled above, before the stamp.
        }
    }

    /// Surface a seated peer's migration part; never a protocol violation and
    /// never acted on here.
    fn on_migration_part(&mut self, peer: PeerKey, part: proto::MigrationPart) {
        let event = match self
            .migration_rx
            .entry(peer)
            .or_default()
            .push::<ToHostMigration>(part)
        {
            Ok(None) => return,
            Ok(Some(payload)) => MigrationEvent::Received(payload),
            Err(e) => {
                self.migration_rejected += 1;
                log::debug!("[host] migration record from {peer} dropped: {e:?}");
                MigrationEvent::Rejected(e)
            }
        };
        if !migration_wire::queue_event(&mut self.migration_events, peer, event) {
            self.migration_dropped += 1;
        }
    }

    fn on_hello(&mut self, peer: PeerKey, proto: u32, name: &str, character: u8) {
        if proto != PROTO_VERSION {
            self.send(peer, &ToClient::Closed(format!("version mismatch (host {PROTO_VERSION}, you {proto})")));
            self.transport.disconnect(peer);
            return;
        }
        if self.phase != HostPhase::Lobby {
            self.send(peer, &ToClient::Closed("race in progress".into()));
            self.transport.disconnect(peer);
            return;
        }
        if self.humans.len() >= MAX_SEATS {
            self.send(peer, &ToClient::Closed("lobby full".into()));
            self.transport.disconnect(peer);
            return;
        }
        // The next id below BOT_ID_BASE not in use; ids are not reused at once,
        // so a UI keyed on them does not mistake a newcomer for who just left.
        let Some(id) = (0..BOT_ID_BASE)
            .map(|k| (self.next_id + k) % BOT_ID_BASE)
            .find(|id| !self.humans.iter().any(|h| h.id == *id))
        else {
            return;
        };
        self.next_id = (id + 1) % BOT_ID_BASE;
        let host = peer >= LOCAL_PEER_BASE;
        self.humans.push(Human {
            id,
            peer,
            name: sanitize_name(name, &format!("Player {}", id as u32 + 1)),
            character: if (character as usize) < CHARACTERS.len() { character } else { 0 },
            attire: [0; 3],
            ready: false,
            custom_ready: None,
            host,
            last_heard: self.now,
            racer: None,
            inq: BTreeMap::new(),
            last_fed: Input::default().pack(),
            applied_seq: 0,
            starved: std::collections::VecDeque::new(),
            pending_press: 0,
        });
        self.by_peer.insert(peer, id);
        self.send(peer, &ToClient::Welcome { proto: PROTO_VERSION, seat: id });
        self.send(peer, &ToClient::Lobby(self.lobby_view()));
        self.send_track(peer);
        self.lobby_dirty = true;
    }

    fn on_lossy(&mut self, peer: PeerKey, bytes: &[u8]) {
        let Some(LossyToHost::Inputs { race, seq, inputs }) = decode::<LossyToHost>(bytes) else { return };
        let now = self.now;
        let Some(&id) = self.by_peer.get(&peer) else { return };
        let Some(h) = self.humans.iter_mut().find(|h| h.id == id) else { return };
        h.last_heard = now;
        if self.phase == HostPhase::Lobby || race != (self.races & 0xFF) as u8 || inputs.is_empty() || inputs.len() > proto::INPUTS_PER_PACKET {
            return;
        }
        // `seq` numbers the last input; input numbers start at 1.
        let Some(first) = seq.checked_sub(inputs.len() as u32 - 1).filter(|&f| f >= 1) else { return };
        for (i, p) in inputs.iter().enumerate() {
            let n = first + i as u32;
            if n > h.applied_seq {
                h.inq.entry(n).or_insert(*p);
            } else {
                h.late_input(n, *p);
            }
        }
        while h.inq.len() > MAX_QUEUED {
            // A client far ahead of itself (bad clock): keep the oldest due.
            let last = *h.inq.keys().next_back().expect("non-empty");
            h.inq.remove(&last);
        }
    }

    fn on_left(&mut self, peer: PeerKey) {
        self.migration_rx.remove(&peer);
        self.migration_events.retain(|(p, _)| *p != peer);
        let Some(id) = self.by_peer.remove(&peer) else { return };
        if let Some(i) = self.humans.iter().position(|h| h.id == id) {
            let h = self.humans.remove(i);
            self.after_departure(h.racer);
        }
    }

    /// A human is gone: in a race its racer becomes a bot; the roster changes.
    fn after_departure(&mut self, racer: Option<usize>) {
        self.lobby_dirty = true;
        self.rematch_dirty = true;
        if self.phase == HostPhase::Racing {
            let mut notice = None;
            if let (Some(r), Some(run)) = (racer, self.run.as_mut()) {
                // Finished, out of a battle or eliminated: the place is theirs.
                let finished = run.world.racers.get(r).is_some_and(|x| x.finished_at.is_some())
                    || run.world.is_out(r)
                    || run.world.is_eliminated(r);
                if let Some(s) = run.world.cfg.racers.get_mut(r) {
                    s.kind = RacerKind::Bot(self.cfg.bot_level);
                    log::info!("[host] seat {r} ({}) left mid-race: now a bot{}", s.name, if finished { "" } else { ", listed as Left" });
                    if !finished && !run.left.contains(&(r as u8)) {
                        run.left.push(r as u8);
                    }
                    notice = Some(ToClient::Left { racer: r as u8, finished });
                }
            }
            if let Some(m) = notice {
                self.broadcast(&m);
            }
        }
        if !self.headless && self.phase != HostPhase::Lobby && self.humans.is_empty() {
            // Nobody left to watch: reset for the next people.
            self.to_lobby();
        }
    }

    fn drop_silent(&mut self, now: u64) {
        if self.phase != HostPhase::Racing {
            return;
        }
        let start = self.run.as_ref().map_or(0, |r| r.start_ms);
        if now < start + SILENCE_MS {
            return;
        }
        let silent: Vec<PeerKey> =
            self.humans.iter().filter(|h| now.saturating_sub(h.last_heard) > SILENCE_MS).map(|h| h.peer).collect();
        for p in silent {
            log::info!("[host] peer {p} silent for {SILENCE_MS} ms: dropped");
            self.on_left(p);
            self.transport.disconnect(p);
        }
    }

    // ─── Starting a race ────────────────────────────────────────────────

    fn begin_race(&mut self) {
        let mut racers: Vec<RacerSetup> = Vec::new();
        for (i, h) in self.humans.iter_mut().enumerate() {
            racers.push(RacerSetup { name: h.name.clone(), character: h.character, kind: RacerKind::Human });
            h.racer = Some(i);
            h.ready = false;
            h.last_heard = self.now;
            h.reset_race_input();
        }
        let level = self.cfg.bot_level;
        for (name, character) in self.bot_identities() {
            racers.push(RacerSetup { name, character, kind: RacerKind::Bot(level) });
        }
        self.races += 1;
        let seed = mix64(self.cfg.seed ^ mix64(self.races));
        let mut cfg = match self.battle {
            Some(b) => RaceConfig::battle(b.arena, racers, b.rules, seed),
            None => RaceConfig {
                track: self.cfg.track,
                laps: self.cfg.laps,
                seed,
                mode: RaceMode::Race,
                racers,
                drill: None,
                attire: [[0; 3]; 8],
                le_mans: false,
            },
        };
        cfg.le_mans = self.le_mans;
        for (i, h) in self.humans.iter().enumerate().take(8) {
            cfg.attire[i] = h.attire;
        }
        let start_ms = self.now + START_LEAD_MS;
        self.run = Some(Run {
            world: match &self.custom {
                Some(t) => World::with_track(cfg.clone(), t.built.clone()),
                None => World::new(cfg.clone()),
            },
            start_ms,
            events: Vec::new(),
            wall_last: [0; MAX_SEATS],
            results: None,
            results_pending: false,
            last_snap_tick: 0,
            left: Vec::new(),
        });
        self.phase = HostPhase::Racing;
        self.rematch_at = None;
        let starts: Vec<(PeerKey, u8)> =
            self.humans.iter().map(|h| (h.peer, h.racer.expect("just seated") as u8)).collect();
        for (peer, you) in starts {
            let race = (self.races & 0xFF) as u8;
            self.send(peer, &ToClient::RaceStart { config: cfg.clone(), custom: self.custom.as_ref().map(|t| t.hash), start_host_ms: start_ms, you, race });
        }
        self.lobby_dirty = true;
    }

    // ─── The world ──────────────────────────────────────────────────────

    fn step_world(&mut self, now: u64) {
        let Some(run) = self.run.as_mut() else { return };
        let mut steps = 0;
        while steps < MAX_CATCH_UP {
            let t = run.world.tick;
            if run.start_ms + t * 1000 / TICK_HZ as u64 > now {
                break;
            }
            let n = run.world.racers.len();
            let mut inputs = vec![Input::default(); n];
            let want = t as u32 + 1;
            for h in &mut self.humans {
                let Some(r) = h.racer.filter(|&r| r < n) else { continue };
                let (fed, starved) = h.feed(want);
                if starved && run.world.phase == RacePhase::Racing && run.world.racers[r].finished_at.is_none() {
                    self.stats.starved_ticks += 1;
                }
                inputs[r] = Input::unpack(fed);
            }
            run.world.step(&inputs);
            self.stats.ticks += 1;
            steps += 1;
            for e in run.world.drain_events() {
                if !is_wire_event(&e) || self.phase == HostPhase::Results {
                    continue;
                }
                if let Event::WallHit { racer, .. } = e {
                    let slot = &mut run.wall_last[(racer as usize).min(MAX_SEATS - 1)];
                    if *slot != 0 && now.saturating_sub(*slot) < WALL_HIT_GAP_MS {
                        continue;
                    }
                    *slot = now.max(1);
                }
                run.events.push(e);
            }
            if run.world.phase == RacePhase::Finished && run.results.is_none() {
                let mut res = results_from_world(&run.world);
                rank_leavers_last(&mut res, &run.left);
                run.results = Some(res);
                run.results_pending = true;
                self.phase = HostPhase::Results;
                self.lobby_dirty = true;
                for h in &mut self.humans {
                    h.ready = false;
                }
            }
        }
    }

    // ─── Rematch ────────────────────────────────────────────────────────

    /// Everyone ready on the results screen: start the 3-2-1; anyone not
    /// ready (or gone): cancel it.
    fn eval_rematch(&mut self) {
        if !std::mem::take(&mut self.rematch_dirty) {
            return;
        }
        let all = self.phase == HostPhase::Results && !self.humans.is_empty() && self.can_start() && self.humans.iter().all(|h| h.ready);
        match (all, self.rematch_at) {
            (true, None) => {
                let at = self.now + REMATCH_MS;
                self.rematch_at = Some(at);
                self.broadcast(&ToClient::Rematch(RematchMsg::Countdown { ends_at_host_ms: at }));
            }
            (false, Some(_)) => {
                self.rematch_at = None;
                self.broadcast(&ToClient::Rematch(RematchMsg::Cancelled));
            }
            _ => {}
        }
    }

    // ─── Output ─────────────────────────────────────────────────────────

    fn flush(&mut self) {
        if std::mem::take(&mut self.lobby_dirty) {
            let v = self.lobby_view();
            self.broadcast(&ToClient::Lobby(v));
        }
        // Events, then the results (so the finish events precede them).
        let mut reliable: Vec<Vec<u8>> = Vec::new();
        let mut results: Option<ToClient> = None;
        if let Some(run) = self.run.as_mut() {
            if !run.events.is_empty() {
                let evs = std::mem::take(&mut run.events);
                reliable = chunk_events(&evs);
            }
            if std::mem::take(&mut run.results_pending) {
                results = run.results.clone().map(ToClient::Results);
            }
        }
        for bytes in reliable {
            self.broadcast_bytes(&bytes);
        }
        if let Some(r) = results {
            self.broadcast(&r);
        }
        self.send_snapshots();
        let open = self.phase == HostPhase::Lobby && self.humans.len() < MAX_SEATS;
        if self.open != Some(open) {
            self.open = Some(open);
            // In a failed switch's rejoin window the lobby keeps the token
            // (see `hold_lan_token_for`); otherwise each change rotates it.
            if self.phase == HostPhase::Lobby
                && self
                    .lan_token_held_until
                    .is_some_and(|until| self.now < until)
            {
                self.transport.set_open_keeping_token(open);
            } else {
                self.lan_token_held_until = None;
                self.transport.set_open(open);
            }
        }
    }

    fn broadcast_bytes(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_RELIABLE_LEN {
            log::error!("[host] broadcast of {} bytes not sent", bytes.len());
            return;
        }
        let peers: Vec<PeerKey> = self.humans.iter().map(|h| h.peer).collect();
        for p in peers {
            self.transport.send_reliable(p, bytes.to_vec());
        }
    }

    fn send_snapshots(&mut self) {
        let Some(run) = self.run.as_mut() else { return };
        let w = &run.world;
        if w.tick == 0 || w.tick / SNAPSHOT_EVERY_TICKS == run.last_snap_tick / SNAPSHOT_EVERY_TICKS {
            return;
        }
        run.last_snap_tick = w.tick;
        let over = w.phase == RacePhase::Finished;
        let nets: Vec<race_core::RacerNet> = w.racers.iter().map(|r| r.to_net()).collect();
        let projectiles: Vec<ProjNet> = w.projectiles.iter().map(ProjNet::from_projectile).collect();
        let hazards: Vec<HazNet> = w
            .hazards
            .iter()
            .map(|h| HazNet { id: h.id, owner: h.owner, pos: h.pos.to_array() })
            .collect();
        let loose: Vec<[f32; 3]> = w.loose_coins.iter().map(|c| c.pos.to_array()).collect();
        let coins_alive = pack_bits(w.coins.iter().map(|c| c.alive));
        let battle = w.battle.as_ref().map(BattleNet::from_state);
        let targets_alive = pack_bits(w.targets.iter().map(|t| t.alive));
        let mut out: Vec<(PeerKey, Vec<u8>)> = Vec::new();
        for h in &self.humans {
            let Some(r) = h.racer.filter(|&r| r < w.racers.len()) else { continue };
            let own = w.racers[r].clone();
            let from = own.pos;
            let mut snap = Snapshot {
                race: (self.races & 0xFF) as u8,
                tick: w.tick as u32,
                ack_seq: h.applied_seq,
                flags: if over { SNAP_OVER } else { 0 },
                own,
                others: nets.iter().enumerate().filter(|(i, _)| *i != r).map(|(_, n)| *n).collect(),
                projectiles: projectiles.clone(),
                hazards: hazards.clone(),
                loose_coins: loose.clone(),
                targets_alive: targets_alive.clone(),
                play: w.play.clone(),
                coins_alive: coins_alive.clone(),
                battle: battle.clone(),
            };
            self.stats.entities_dropped += snap.budget(from) as u64;
            let bytes = snap.encode();
            if bytes.len() > MAX_LOSSY_LEN {
                log::error!("[host] snapshot of {} bytes over budget, not sent", bytes.len());
                continue;
            }
            self.stats.snapshot_bytes_max = self.stats.snapshot_bytes_max.max(bytes.len());
            self.stats.snapshots_sent += 1;
            out.push((h.peer, bytes));
        }
        for (peer, bytes) in out {
            self.transport.send_lossy(peer, bytes);
        }
    }
}
