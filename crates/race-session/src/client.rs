//! [`ClientSession`]: one player's side of the lobby and the race (spec 10).
//!
//! During the race the client
//! - sends `Inputs` at 60 Hz, numbered, the last 5 in every datagram, running
//!   ahead of the host's world by about half an RTT plus the 2-tick jitter
//!   buffer (input `n` drives world tick `n - 1`);
//! - predicts its own racer with `Racer::integrate` and, on every snapshot,
//!   resets to the host's `own` and replays the inputs after `ack_seq`, hiding
//!   the jump in a visual offset that eases out over 150-250 ms (longer for
//!   bigger errors; snapping only when it is over 10 m);
//! - interpolates everyone else (racers, eggs, cacti, frogs, rose sellers) at least 100 ms
//!   in the past on the host's timeline: further back when snapshots arrive
//!   late, by as much as the recent ones were late (see [`interp_target_ms`]).
//!   That point in host time is a clock of its own, which runs a little slower
//!   or faster than real time to follow its target and never jumps on a late
//!   packet, a burst or a clock-sync round.
//!
//! Driven as `update(now_ms, local_input)`: `now_ms` is any monotonic local
//! clock (the host clock is estimated by ping/pong).

use std::collections::VecDeque;
use std::sync::Arc;

use race_core::math::{flat, forward, lerp, wrap_angle, DT, TICK_HZ};
use race_core::playful::RoseSeller;
use race_core::{custom, BattleState, CustomTrack, Event, Input, Launch, MotionFx, RaceMode, RacePhase, Racer, RacerNet, Track, TrackId, Tunables, Vec3, World};

use crate::clock::ClockSync;
use crate::proto::{
    self, bit_at, decode, encode, LobbyPhase, LobbyView, LossyToClient, LossyToHost, ResultsView, SeatView, Snapshot,
    KeepPass, ToClient, ToHost, INPUTS_PER_PACKET, PROTO_VERSION,
};
use crate::transport::{ClientNetEvent, ClientTransport, HostIdentity};
use crate::view::{RaceDriver, RaceView};
use crate::track_transfer::{Download, SelectedTrack};
use crate::host::migration::wire::{MigrationEvent, Reassembler, WireError};
use crate::proto::{ToClientMigration, ToHostMigration};

/// Others are drawn at least this far in the past (host time).
pub const INTERP_DELAY_MS: f64 = 100.0;
/// ...and at most this far, however late the snapshots are.
pub const INTERP_DELAY_MAX_MS: f64 = 350.0;
/// The time between two snapshots (ms): drawing needs the one after the
/// drawn time to have arrived.
const SNAPSHOT_GAP_MS: f64 = proto::SNAPSHOT_EVERY_TICKS as f64 * 1000.0 / TICK_HZ as f64;
/// Spare time on top of the snapshots' lateness (ms).
const INTERP_MARGIN_MS: f64 = 8.0;
/// Snapshot lateness is judged over this long (local ms).
const LATENESS_WINDOW_MS: u64 = 4000;
/// The share of recent snapshots the delay waits for; the rest are
/// extrapolated (briefly) instead.
const LATENESS_QUANTILE: f64 = 0.97;
/// A shrinking delay comes down at most this fast (ms per second).
const DELAY_FALL_PER_S: f64 = 25.0;
/// The others' clock runs between these rates of real time to follow its
/// target (slower when the delay grows, faster when it shrinks).
const RENDER_RATE_MIN: f64 = 0.8;
const RENDER_RATE_MAX: f64 = 1.08;
/// The rate changes by 1/this per ms of distance from the target.
const RENDER_STEER_MS: f64 = 250.0;
/// The others' clock changes its rate by at most this much per second.
const RENDER_RATE_CHANGE_PER_S: f64 = 3.0;
/// Running out of snapshots (a burst held them back), the others' clock slows
/// down to this rate of real time once it is this close to the newest one (ms).
const RENDER_RATE_STARVED: f64 = 0.5;
const RENDER_HEADROOM_MS: f64 = 50.0;
/// Further from its target than this (a stall, a rebase), the others' clock jumps.
const RENDER_SNAP_MS: f64 = 500.0;
/// The longest frame the others' clock advances by (a stalled frame must not run it on).
const RENDER_MAX_FRAME_MS: f64 = 250.0;
/// A visual correction eases out over at least this long...
pub const CORRECTION_MS: f32 = 150.0;
/// ...and at most this long (bigger errors ease longer, see [`correction_ms`]).
pub const CORRECTION_MAX_MS: f32 = 250.0;
/// The largest clock correction a migration rebase may apply (the spec's
/// recovery budget); a larger skew leaves the session paused.
pub const MIGRATION_MAX_CORRECTION_MS: u64 = 250;
/// A correction bigger than this snaps instead of easing (a respawn, a
/// teleport). Hits and bashes arriving half an RTT late slide.
pub const SNAP_DISTANCE: f32 = 10.0;
/// Host start-boost / stumble events for your racer are matched against the
/// local prediction's until this long after GO (host clock).
const LAUNCH_DEDUPE_MS: u64 = 5000;
/// The host's jitter buffer, in ticks (the client's lead covers it).
pub const JITTER_TICKS: u64 = 2;
const INPUT_RING: usize = 480;
/// The most input packets resent after a takeover's `Resume`.
pub const RESEND_PACKETS: usize = 16;
const SNAP_BUFFER: usize = 30;
/// The others are extrapolated at most this many ticks past the newest snapshot.
const EXTRAPOLATE_TICKS: f64 = 6.0;
/// Longest frame step used for easing (a stalled frame must not jump).
const MAX_FRAME_S: f32 = 0.1;
/// More input ticks than this due at once (a stall): skip ahead instead.
const MAX_CATCH_UP_TICKS: u32 = 120;

/// Where this player is in the flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientPhase {
    /// Joining; the text is the current stage.
    Connecting(String),
    Lobby,
    /// From `RaceStart`: the grid countdown, then the race.
    Racing,
    Results,
    /// The join failed (friendly text).
    Failed(String),
    /// The host refused or removed us (full, version, kicked...).
    Closed(String),
    HostLeft,
    Disconnected(String),
}

impl ClientPhase {
    /// The link is gone for good.
    pub fn is_dead(&self) -> bool {
        matches!(
            self,
            ClientPhase::Failed(_) | ClientPhase::Closed(_) | ClientPhase::HostLeft | ClientPhase::Disconnected(_)
        )
    }
}

/// Counters for tests and a debug overlay.
#[derive(Clone, Debug, Default)]
pub struct ClientStats {
    pub snapshots: u64,
    pub inputs_sent: u64,
    /// Size of the latest visual correction (before easing), metres.
    pub last_correction_m: f32,
    pub max_correction_m: f32,
    /// Corrections over [`SNAP_DISTANCE`] (shown as a jump, not eased).
    pub hard_snaps: u64,
    /// Race frames that drew the others (from the first snapshot on).
    pub interp_frames: u64,
    /// ...of which drew them past the newest snapshot (extrapolating: the
    /// snapshot they needed was late).
    pub interp_starved_frames: u64,
    /// ...of which were so far past it that the others stood still.
    pub interp_frozen_frames: u64,
    /// How far in the past the others were drawn in the latest frame (host ms).
    pub interp_delay_ms: f32,
}

/// How long a visual correction of `metres` eases out: [`CORRECTION_MS`] up
/// to 1 m, then 20 ms more per metre, at most [`CORRECTION_MAX_MS`].
pub fn correction_ms(metres: f32) -> f32 {
    (CORRECTION_MS + (metres - 1.0).max(0.0) * 20.0).min(CORRECTION_MAX_MS)
}

/// How far in the past to draw the others (host ms), given how late recent
/// snapshots were (ms each): late enough that the snapshot after the drawn
/// time has nearly always arrived, at least [`INTERP_DELAY_MS`] and at most
/// [`INTERP_DELAY_MAX_MS`].
pub fn interp_target_ms(lateness_ms: &[f64]) -> f64 {
    if lateness_ms.is_empty() {
        return INTERP_DELAY_MS;
    }
    let mut v = lateness_ms.to_vec();
    v.sort_by(f64::total_cmp);
    let q = v[((v.len() - 1) as f64 * LATENESS_QUANTILE).round() as usize];
    (q + SNAPSHOT_GAP_MS + INTERP_MARGIN_MS).clamp(INTERP_DELAY_MS, INTERP_DELAY_MAX_MS)
}

/// The share of a correction still shown `age_ms` into an ease of
/// `duration_ms` (1 -> 0, smoothstep).
pub fn correction_left(age_ms: f32, duration_ms: f32) -> f32 {
    let t = (age_ms / duration_ms.max(1.0)).clamp(0.0, 1.0);
    1.0 - t * t * (3.0 - 2.0 * t)
}

#[derive(Clone)]
struct Race {
    race_no: u8,
    you: usize,
    start_ms: u64,
    go_tick: u64,
    track: Arc<Track>,
    tun: Tunables,
    pred: Racer,
    predicting: bool,
    over: bool,
    /// Next input number to produce (1-based).
    next_seq: u32,
    ring: VecDeque<(u32, [u8; 4])>,
    pred_log: VecDeque<(u32, Vec3)>,
    /// The first prediction made for each input number (never rewritten).
    live_log: VecDeque<(u32, Vec3)>,
    /// Coin homes, which coins the newest snapshot says are alive, and which
    /// the prediction has already picked since (reset on every reconcile).
    coin_homes: Vec<Vec3>,
    coin_bits: Vec<u8>,
    coin_taken: Vec<bool>,
    snaps: VecDeque<Snapshot>,
    newest_tick: u32,
    corr_from: Vec3,
    corr_age: f32,
    corr_ms: f32,
    last_now: u64,
    /// The start the prediction raised locally (its host copy is dropped).
    launch_local: Option<Launch>,
    events: Vec<Event>,
    cd_last: Option<i64>,
    gait_prev: Vec<Option<f32>>,
    finish_times: Vec<Option<f32>>,
    /// A battle (snapshots carry `BattleNet`).
    battle: bool,
    /// Battle state as the reliable events tell it (feathers, hits, taken, out
    /// order, done). Snapshots are lossy and often arrive after the events of
    /// the same tick, so the view merges this over the newest snapshot: the
    /// event a banner reacts to is never ahead of the state it reads.
    battle_ev: Option<BattleState>,
    /// The host tick the others were last drawn at.
    render_tick: f64,
    /// Recent snapshots: (local ms at arrival, how late they were in ms: the
    /// estimated host time at arrival minus their tick's host time).
    lateness: VecDeque<(u64, f64)>,
    /// How far in the past the others are drawn (host ms), and the host time
    /// they are drawn at (`None` until the first frame).
    delay_ms: f64,
    render_ms: Option<f64>,
    /// The rate the others' clock ran at in the latest frame.
    render_rate: f64,
}

/// One player's session.
pub struct ClientSession {
    migration_custom:Option<Vec<race_core::TrackDef>>,
    transport: Box<dyn ClientTransport>,
    name: String,
    character: u8,
    attire: [u8; 3],
    phase: ClientPhase,
    host: Option<HostIdentity>,
    my_seat: Option<u8>,
    lobby: Option<LobbyView>,
    results: Option<ResultsView>,
    rematch_ends: Option<u64>,
    clock: ClockSync,
    race: Option<Race>,
    view: Option<RaceView>,
    stats: ClientStats,
    now: u64,
    custom: Option<SelectedTrack>,
    download: Option<Download>,
    track_error: Option<String>,
    /// Host-migration setup reassembly on this session's link.
    migration_rx: Reassembler,
    migration_events: VecDeque<((), MigrationEvent<ToClientMigration>)>,
    migration_dropped: u64,
    /// Keep passes the host sent, not yet taken by the app.
    keep_passes: Vec<KeepPass>,
    /// Who left the current race.
    departures: Departures,
}

/// How long a "<name> left" notice shows (local ms).
pub const NOTICE_MS: u64 = 4000;
/// Notices shown at once (the oldest goes first).
const MAX_NOTICES: usize = 3;

/// Players who left the current race, as the host told us.
#[derive(Clone, Debug, Default)]
struct Departures {
    /// Racers listed as Left in the results (they left before finishing).
    left: Vec<u8>,
    /// "<name> left" and the local ms it arrived.
    notices: VecDeque<(String, u64)>,
}

/// Passes held between [`ClientSession::take_keep_passes`] calls.
const MAX_KEEP_PASSES: usize = 4;

fn input_tick_ms(t: i64) -> i64 {
    t * 1000 / TICK_HZ as i64
}

impl ClientSession {
    fn migration_clone(&self,transport:Box<dyn ClientTransport>)->Self {
        Self {
            migration_custom:Some(vec![]),transport,name:self.name.clone(),character:self.character,attire:self.attire,phase:self.phase.clone(),host:self.host.clone(),my_seat:self.my_seat,lobby:self.lobby.clone(),results:self.results.clone(),rematch_ends:self.rematch_ends,clock:self.clock.clone(),race:self.race.clone(),stats:self.stats.clone(),now:self.now,custom:self.custom.clone(),download:self.download.clone(),track_error:self.track_error.clone(),migration_rx:self.migration_rx.clone(),migration_events:self.migration_events.clone(),migration_dropped:self.migration_dropped,departures:self.departures.clone(),keep_passes:self.keep_passes.clone(),
            view:self.view.as_ref().map(|v|RaceView{world:v.world.clone(),you:v.you,hud:v.hud.clone(),standings:v.standings.clone(),events:v.events.clone(),correction:v.correction,over:v.over}),
        }
    }
    /// Stage a complete prediction/input frame privately, including late input
    /// history, view events and custom content. Journal all reliable intents
    /// before adopting any local effect or publishing network/UI work.
    pub fn migration_step(&mut self,journal:&mut decented_net::migration::IntentJournal,now:u64,input:Input,incoming:&[ClientNetEvent],rtt:Option<u32>,operation:impl FnOnce(&mut Self)->decented_net::migration::Result<()>)->decented_net::migration::Result<MigrationClientBatch> {
        self.migration_step_inner(journal,now,None,input,incoming,rtt,operation)
    }
    /// Apply certified recovery effects (replayed reliable outputs) at frozen
    /// authority time C, `logical_ms`. `now` is the device's real local time;
    /// Stampede's client keeps no security clock, so it only stays monotonic.
    /// No frame runs: no input generation, prediction or view advance, no
    /// clock-sync pings and no other periodic traffic. A `Pong` applied here
    /// changes neither the clock offset nor the RTT (its receipt includes the
    /// outage), and host events are stamped at C. Reactive reliable followups
    /// to the certified outputs are not suppressed: a replayed `TrackData`
    /// that completes a lobby download sends `TrackReady`, which is journaled
    /// in the same transaction (then sealed and drained like any intent).
    /// Everything is staged exactly as in [`Self::migration_step`].
    pub fn migration_recovery_step(&mut self,journal:&mut decented_net::migration::IntentJournal,now:u64,logical_ms:u64,incoming:&[ClientNetEvent],rtt:Option<u32>)->decented_net::migration::Result<MigrationClientBatch> {
        self.migration_step_inner(journal,now,Some(logical_ms),Input::default(),incoming,rtt,|_|Ok(()))
    }
    #[allow(clippy::too_many_arguments)]
    fn migration_step_inner(&mut self,journal:&mut decented_net::migration::IntentJournal,now:u64,frozen:Option<u64>,input:Input,incoming:&[ClientNetEvent],rtt:Option<u32>,operation:impl FnOnce(&mut Self)->decented_net::migration::Result<()>)->decented_net::migration::Result<MigrationClientBatch> {
        use decented_net::migration::{Error,MAX_BATCH,MAX_JOURNAL_BYTES};
        if now<self.now || self.migration_custom.is_some() || incoming.len()>MAX_BATCH{return Err(Error::Bounds);}
        let mut bytes=0usize;
        for event in incoming {
            let (ClientNetEvent::Reliable(payload)|ClientNetEvent::Lossy(payload))=event else{return Err(Error::Invalid("physical connection event in armed game batch"));};
            bytes=bytes.checked_add(payload.len()).ok_or(Error::Bounds)?;
            if payload.len()>decented_net::MAX_RELIABLE_LEN || bytes>MAX_JOURNAL_BYTES{return Err(Error::Bounds);}
        }
        let capture=std::sync::Arc::new(std::sync::Mutex::new(MigrationCapture{incoming:incoming.iter().cloned().collect(),reliable:vec![],lossy:vec![],bytes:0,rtt,closed:false,overflow:false}));
        let mut staged=self.migration_clone(Box::new(MigrationClientTransport(capture.clone())));
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{operation(&mut staged)?;match frozen{None=>staged.update(now,input),Some(c)=>staged.recover_frozen(now,c)}Ok::<_,Error>(())})).map_err(|_|Error::Invalid("race client transaction panic"))??;
        if let Some(race)=&staged.race {decented_net::migration::validate_finite(&race.pred)?;}
        if let Some(view)=&staged.view {decented_net::migration::validate_finite(&view.world)?;}
        let capture=capture.lock().map_err(|_|Error::Closed)?;
        if capture.overflow{return Err(Error::Bounds);}
        if !capture.incoming.is_empty(){return Err(Error::Gap);}
        let custom=staged.migration_custom.take().ok_or(Error::Invalid("client staging services replaced"))?;
        if custom.len()>MAX_BATCH{return Err(Error::Bounds);}
        let ids=journal.append_batch(&capture.reliable)?;
        let batch=MigrationClientBatch{reliable:ids.into_iter().zip(capture.reliable.iter().cloned()).collect(),lossy:capture.lossy.clone(),closed:capture.closed};
        std::mem::swap(&mut staged.transport,&mut self.transport);
        *self=staged;
        for def in custom {custom::install(def);}Ok(batch)
    }
    /// A fresh session joining again over `transport` with this one's name
    /// and character (a failed armed switch rejoins the ordinary lobby).
    pub(crate) fn rejoin(&self, transport: Box<dyn ClientTransport>) -> ClientSession {
        ClientSession::new(transport, &self.name, self.character)
    }

    pub fn new(transport: Box<dyn ClientTransport>, name: &str, character: u8) -> ClientSession {
        ClientSession {
            migration_custom:None,
            transport,
            name: proto::sanitize_name(name, "Player"),
            character,
            attire: [0; 3],
            phase: ClientPhase::Connecting("Connecting".into()),
            host: None,
            my_seat: None,
            lobby: None,
            results: None,
            rematch_ends: None,
            clock: ClockSync::default(),
            race: None,
            view: None,
            stats: ClientStats::default(),
            now: 0,
            custom: None,
            download: None,
            track_error: None,
            migration_rx: Reassembler::default(),
            migration_events: VecDeque::new(),
            migration_dropped: 0,
            keep_passes: Vec::new(),
            departures: Departures::default(),
        }
    }

    // ─── Queries ─────────────────────────────────────────────────────────

    pub fn phase(&self) -> &ClientPhase {
        &self.phase
    }

    /// Who the QUIC link proved the host to be (None on loopback).
    pub fn host_identity(&self) -> Option<&HostIdentity> {
        self.host.as_ref()
    }

    pub fn lobby(&self) -> Option<&LobbyView> {
        self.lobby.as_ref()
    }

    /// The current lobby's checked custom track, ready to save in My Tracks.
    pub fn custom_track(&self) -> Option<&CustomTrack> {
        let hash = self.lobby.as_ref()?.custom?;
        self.custom.as_ref().filter(|t| t.hash == hash).map(|t| &t.track)
    }

    pub fn downloading_track(&self) -> bool {
        self.lobby.as_ref().is_some_and(|l| l.custom.is_some()) && self.custom_track().is_none()
    }

    /// Keep passes the host sent since the last call, in arrival order, each
    /// once (a repeat of one already held is dropped). A Keep sends one after
    /// admitting a player; the app saves it to My Keeps. A pass naming a
    /// different Keep than the one the link proved is dropped when the
    /// transport proved a host (QUIC); loopback proves none and accepts all.
    /// At most 4 are held between calls.
    pub fn take_keep_passes(&mut self) -> Vec<KeepPass> {
        std::mem::take(&mut self.keep_passes)
    }

    /// Racers whose player left the current race before finishing: the
    /// results list them last, without a time, and should show them as Left.
    pub fn left_racers(&self) -> &[u8] {
        &self.departures.left
    }

    /// "<name> left" for each player who left the race in the last
    /// [`NOTICE_MS`] (as of the latest `update`), newest last, joined by " · ".
    pub fn notice(&self) -> Option<String> {
        let shown: Vec<&str> = self
            .departures
            .notices
            .iter()
            .filter(|(_, at)| self.now < at.saturating_add(NOTICE_MS))
            .map(|(text, _)| text.as_str())
            .collect();
        (!shown.is_empty()).then(|| shown.join(" · "))
    }

    /// Host-migration setup messages from the host since the last call, in
    /// arrival order: complete payloads, and records dropped as malformed.
    /// The session acts on none of them. At most 4 are held between calls.
    pub fn take_migration_events(&mut self) -> Vec<MigrationEvent<ToClientMigration>> {
        self.migration_events.drain(..).map(|(_, e)| e).collect()
    }

    /// Complete host-migration setup records dropped because 4 events were
    /// already pending.
    pub fn migration_dropped(&self) -> u64 {
        self.migration_dropped
    }

    /// Malformed host-migration setup records dropped so far.
    pub fn migration_rejected(&self) -> u64 {
        self.migration_rx.rejected()
    }

    /// Send a host-migration setup payload to the host, chunked into
    /// `ToHost::Migration` parts. Refuses a payload over four parts.
    #[cfg_attr(not(test), allow(dead_code))] // Until the lobby flow (slices 6.5-6.8) sends.
    pub(crate) fn send_migration(&mut self, payload: &ToHostMigration) -> Result<(), WireError> {
        for part in crate::host::migration::wire::chunk(payload)? {
            self.send(&ToHost::Migration(part));
        }
        Ok(())
    }

    pub fn track_error(&self) -> Option<&str> {
        self.track_error.as_deref()
    }

    /// Your stable player id in the lobby (`SeatView::id`).
    pub fn my_seat(&self) -> Option<u8> {
        self.my_seat
    }

    /// Your own row of the roster.
    pub fn my_lobby_seat(&self) -> Option<&SeatView> {
        let id = self.my_seat?;
        self.lobby.as_ref()?.seat(id)
    }

    pub fn results(&self) -> Option<&ResultsView> {
        self.results.as_ref()
    }

    pub fn view(&self) -> Option<&RaceView> {
        self.view.as_ref()
    }

    pub fn stats(&self) -> &ClientStats {
        &self.stats
    }

    /// Round trip to the host (clock-sync best sample, else the transport's).
    pub fn rtt_ms(&self) -> Option<u32> {
        self.clock.rtt_ms().map(|r| r as u32).or_else(|| self.transport.rtt_ms())
    }

    /// Host clock minus local clock, once synced.
    pub fn clock_offset_ms(&self) -> Option<i64> {
        self.clock.offset_ms()
    }

    /// The bounded recovery clock correction: this client's estimate of host
    /// time at `frozen_local` (its last frame before the freeze) minus the
    /// frozen authority time C. Wall downtime is not part of it. Zero while
    /// the clock is unsynced.
    pub fn migration_clock_delta(&self, frozen_local: u64, logical_ms: u64) -> i64 {
        self.clock.host_now(frozen_local).map_or(0, |host| (host as i64).saturating_sub(logical_ms as i64))
    }
    /// Retained-session clock rebase at the recovery barrier: host time at
    /// `local_now` reads C (`logical_ms`), the open clock-sync round is
    /// aborted (a replayed `Pong` is not a sample), and the race frame clock
    /// restarts at `local_now`, so no physics catch-up runs for the pause.
    /// World, prediction, journals and input numbers are kept; the client
    /// continues from the next tick. `delta` is the bounded correction this
    /// rebase applies ([`Self::migration_clock_delta`] at the cut, 0 for the
    /// `Resume` re-anchor): over [`MIGRATION_MAX_CORRECTION_MS`] it is
    /// refused before anything changes, and the session stays paused. The
    /// client keeps no security clock, so nothing here can extend one.
    pub fn migration_rebase_clock(&mut self, local_now: u64, logical_ms: u64, delta: i64) -> Result<(), &'static str> {
        if delta.unsigned_abs() > MIGRATION_MAX_CORRECTION_MS {
            return Err("migration clock correction exceeds 250 ms");
        }
        self.clock.rebase(local_now, logical_ms);
        if let Some(race) = self.race.as_mut() { race.last_now = race.last_now.max(local_now); }
        self.now = self.now.max(local_now);
        Ok(())
    }
    /// Re-measure the host clock now: the next frame starts a fresh
    /// clock-sync round (an open one is aborted); the offset holds until it
    /// completes. An armed client whose host-time estimate ran too far ahead
    /// of the commits (the host held its game clock) calls this.
    pub fn migration_resync_clock(&mut self) {
        self.clock.restart();
    }
    /// Keep this client's host-time estimate at `local_now` within
    /// `[low, high]` by moving its clock offset (an open sync round is
    /// kept). An armed client bounds its estimate to the commits it saw this
    /// way before each stepped frame. Returns the change in ms (negative:
    /// the estimate was above `high`); 0 while unsynced.
    pub fn migration_bound_clock(&mut self, local_now: u64, low: u64, high: u64) -> i64 {
        self.clock.bound(local_now, low, high)
    }
    /// Take the view's events. Each client step's view replaces the last
    /// one's events, so a driver that runs several steps in one app frame
    /// takes them after each step and sets the frame's whole list
    /// ([`Self::migration_set_view_events`]) before the app reads the view.
    pub(crate) fn migration_take_view_events(&mut self) -> Vec<Event> {
        self.view.as_mut().map(|v| std::mem::take(&mut v.events)).unwrap_or_default()
    }
    /// Set the view's events to an app frame's collected list, in order.
    pub(crate) fn migration_set_view_events(&mut self, events: Vec<Event>) {
        if let Some(view) = self.view.as_mut() {
            view.events = events;
        }
    }
    /// After a takeover's `Resume`: send this race's unconfirmed input window
    /// again (ring entries above the newest snapshot's acknowledged number,
    /// at most `RESEND_PACKETS` packets of the newest), oldest first, in packets of at most `INPUTS_PER_PACKET`
    /// consecutive numbers. The dead host never committed them; the
    /// successor treats numbers it already applied as late copies (a press
    /// is fed once at most) and the rest as ordinary inputs, so the client
    /// does not go quiet for its correction. Returns the packets sent.
    pub fn migration_resend_inputs(&mut self) -> usize {
        let Some(race) = self.race.as_ref().filter(|r| !r.over) else { return 0 };
        let mut packets = Vec::new();
        let acked = race.snaps.back().map_or(0, |s| s.ack_seq);
        let mut ring: Vec<(u32, [u8; 4])> = race.ring.iter().copied().filter(|(s, _)| *s > acked).collect();
        ring.drain(..ring.len().saturating_sub(RESEND_PACKETS * INPUTS_PER_PACKET));
        let mut start = 0;
        while start < ring.len() {
            let mut end = start + 1;
            while end < ring.len() && end - start < INPUTS_PER_PACKET && ring[end].0 == ring[end - 1].0 + 1 { end += 1; }
            let seq = ring[end - 1].0;
            let inputs = ring[start..end].iter().map(|(_, p)| *p).collect();
            packets.push(encode(&LossyToHost::Inputs { race: race.race_no, seq, inputs }));
            start = end;
        }
        for bytes in &packets { self.stats.inputs_sent += 1; self.transport.send_lossy(bytes.clone()); }
        packets.len()
    }
    /// The clock-sync state (round, samples, offset), read-only.
    pub fn clock_sync(&self) -> &ClockSync {
        &self.clock
    }

    /// Estimated host clock.
    pub fn host_now(&self, local_now: u64) -> Option<u64> {
        self.clock.host_now(local_now)
    }

    /// Milliseconds left of the rematch 3-2-1 (results screen), if it runs.
    pub fn rematch_ms_left(&self, local_now: u64) -> Option<u64> {
        let ends = self.rematch_ends?;
        let host = self.clock.host_now(local_now)?;
        Some(ends.saturating_sub(host))
    }

    /// The predicted position for input number `seq`, while it is still in
    /// the log (tests, debug).
    pub fn predicted_pos(&self, seq: u32) -> Option<Vec3> {
        let race = self.race.as_ref()?;
        race.pred_log.iter().find(|(s, _)| *s == seq).map(|(_, p)| *p)
    }

    /// The prediction first made for input number `seq` (before any later
    /// snapshot revised it): what the player saw, minus the easing offset.
    pub fn live_predicted_pos(&self, seq: u32) -> Option<Vec3> {
        let race = self.race.as_ref()?;
        race.live_log.iter().find(|(s, _)| *s == seq).map(|(_, p)| *p)
    }

    /// The next input number this client's race frame will produce (tests,
    /// debug).
    pub fn next_input(&self) -> Option<u32> {
        self.race.as_ref().map(|r| r.next_seq)
    }

    /// The latest input number the host has applied (from the newest snapshot).
    pub fn last_ack(&self) -> Option<u32> {
        self.race.as_ref()?.snaps.back().map(|s| s.ack_seq)
    }

    // ─── Commands ────────────────────────────────────────────────────────

    pub fn set_character(&mut self, c: u8) {
        self.character = c;
        self.send(&ToHost::SetCharacter(c));
    }

    /// Change cosmetic selections while seated in a lobby.
    pub fn set_attire(&mut self, attire: [u8; 3]) {
        let attire = attire.map(|v| v.min(3));
        if self.attire != attire && matches!(self.phase, ClientPhase::Lobby) {
            self.attire = attire;
            self.send(&ToHost::SetAttire(attire));
        }
    }

    pub fn set_ready(&mut self, ready: bool) {
        if ready && self.downloading_track() { return; }
        self.send(&ToHost::SetReady(ready));
    }

    /// Results screen: ready for a rematch (all ready starts the 3-2-1; un-ready cancels it).
    pub fn set_rematch_ready(&mut self, ready: bool) {
        if ready && self.downloading_track() { return; }
        self.send(&ToHost::RematchReady(ready));
    }

    /// Leave the room.
    pub fn leave(&mut self) {
        self.send(&ToHost::Leave);
        self.transport.close();
        self.phase = ClientPhase::Closed("left".into());
    }

    fn send(&mut self, m: &ToHost) {
        let b = encode(m);
        if !b.is_empty() && b.len() <= proto::MAX_RELIABLE_LEN {
            self.transport.send_reliable(b);
        }
    }

    // ─── The frame ───────────────────────────────────────────────────────

    /// One frame. `local_input` is the player's controls now (ignored outside a race).
    pub fn update(&mut self, now_ms: u64, local_input: Input) {
        self.now = now_ms;
        while let Some(ev) = self.transport.poll() {
            self.on_net(ev, now_ms, None);
        }
        if matches!(self.phase, ClientPhase::Lobby | ClientPhase::Racing | ClientPhase::Results) {
            for p in self.clock.tick(now_ms) {
                self.send(&p);
            }
        }
        if matches!(self.phase, ClientPhase::Racing | ClientPhase::Results) {
            self.race_frame(now_ms, local_input);
        }
    }

    /// Recovery at frozen authority time C: apply the delivered events only.
    /// `now` stays the real local time (the monotonic check and a replayed
    /// race start's frame anchor); the clock, prediction and race frame do not
    /// move.
    fn recover_frozen(&mut self, now_ms: u64, logical_ms: u64) {
        self.now = now_ms;
        while let Some(ev) = self.transport.poll() {
            self.on_net(ev, now_ms, Some(logical_ms));
        }
    }

    fn on_net(&mut self, ev: ClientNetEvent, now: u64, frozen: Option<u64>) {
        match ev {
            ClientNetEvent::Progress(s) => {
                if matches!(self.phase, ClientPhase::Connecting(_)) {
                    self.phase = ClientPhase::Connecting(s);
                }
            }
            ClientNetEvent::Connected { host } => {
                self.host = host;
                self.phase = ClientPhase::Connecting("Joining".into());
                let hello = ToHost::Hello { proto: PROTO_VERSION, name: self.name.clone(), character: self.character };
                self.send(&hello);
            }
            ClientNetEvent::Failed(s) => self.phase = ClientPhase::Failed(s),
            ClientNetEvent::Reliable(b) => {
                if b.len() > proto::MAX_RELIABLE_LEN { return; }
                if let Some(m) = decode::<ToClient>(&b) {
                    self.on_reliable(m, now, frozen);
                }
            }
            ClientNetEvent::Lossy(b) => {
                if frozen.is_some() {
                    return; // recovery has no lossy path
                }
                if let Some(LossyToClient::Snapshot(s)) = decode::<LossyToClient>(&b) {
                    self.on_snapshot(s, now);
                }
            }
            ClientNetEvent::Disconnected { host_left, reason } => {
                if self.phase.is_dead() {
                    return; // keep the more specific reason (Closed("lobby full")...)
                }
                self.phase = if host_left { ClientPhase::HostLeft } else { ClientPhase::Disconnected(reason) };
            }
        }
    }

    fn on_reliable(&mut self, m: ToClient, now: u64, frozen: Option<u64>) {
        // A closed client stays closed: a late or replayed output (recovery
        // keeps replaying to a seat that has left) cannot revive it or replace
        // its reason.
        if self.phase.is_dead()
            && matches!(m, ToClient::RaceStart { .. } | ToClient::Results(_) | ToClient::Closed(_))
        {
            return;
        }
        match m {
            ToClient::Welcome { proto, seat } => {
                if proto != PROTO_VERSION {
                    self.phase = ClientPhase::Closed("version mismatch".into());
                    return;
                }
                self.my_seat = Some(seat);
                if matches!(self.phase, ClientPhase::Connecting(_)) {
                    self.phase = ClientPhase::Lobby;
                }
            }
            ToClient::Lobby(v) => {
                if (v.track == TrackId::Custom) != v.custom.is_some() || (v.battle.is_some() && v.custom.is_some()) {
                    self.track_error = Some("The host sent an inconsistent track selection.".into());
                    return;
                }
                if self.lobby.as_ref().and_then(|l| l.custom) != v.custom {
                    self.download = None;
                    self.track_error = None;
                }
                let back_to_lobby = v.phase == LobbyPhase::Lobby;
                self.lobby = Some(v);
                if back_to_lobby && matches!(self.phase, ClientPhase::Results | ClientPhase::Racing) {
                    self.phase = ClientPhase::Lobby;
                    self.race = None;
                    self.view = None;
                    self.results = None;
                    self.rematch_ends = None;
                }
            }
            ToClient::TrackData { hash, idx, total, bytes } => {
                self.on_track_data(hash, idx, total, bytes);
            }
            ToClient::RaceStart { config, custom, start_host_ms, you, race } => {
                self.on_race_start(config, custom, start_host_ms, you as usize, race)
            }
            ToClient::Events(v) => {
                let host_now = frozen.or_else(|| self.clock.host_now(now));
                if let (Some(race), Some(view)) = (self.race.as_mut(), self.view.as_ref()) {
                    let (racers, targets) = (view.world.racers.len(), view.world.targets.len());
                    for e in v {
                        // Indices come from the host: never hand the app one it cannot index.
                        if event_in_range(&e, racers, targets) {
                            race.on_host_event(e, host_now);
                        }
                    }
                }
            }
            ToClient::Results(r) => {
                // Racer indices and places come from the host: check them.
                let n = self.view.as_ref().map_or(proto::MAX_SEATS, |v| v.world.racers.len());
                let sane = r.rows.len() <= n
                    && r.rows.iter().all(|row| (row.racer as usize) < n && (1..=n).contains(&(row.place as usize)));
                if !sane {
                    log::warn!("[client] malformed results ignored");
                    return;
                }
                self.results = Some(r);
                self.phase = ClientPhase::Results;
                self.rematch_ends = None;
                if let Some(race) = self.race.as_mut() {
                    race.over = true;
                    race.predicting = false;
                }
            }
            ToClient::Rematch(proto::RematchMsg::Countdown { ends_at_host_ms }) => self.rematch_ends = Some(ends_at_host_ms),
            ToClient::Rematch(proto::RematchMsg::Cancelled) => self.rematch_ends = None,
            // In recovery the sample's receipt includes the outage: not a clock
            // sample. An open round finishes on its timeout after recovery.
            ToClient::Pong { id, client_t, host_t } => {
                if frozen.is_none() {
                    self.clock.on_pong(id, client_t, host_t, now);
                }
            }
            ToClient::Closed(why) => {
                self.phase = if why == "host left" { ClientPhase::HostLeft } else { ClientPhase::Closed(why) };
            }
            ToClient::Left { racer, finished } => {
                let Some(view) = self.view.as_ref() else { return };
                let Some(setup) = view.world.cfg.racers.get(racer as usize) else { return };
                if racer as usize == view.you {
                    return;
                }
                let name = setup.name.clone();
                let d = &mut self.departures;
                if !finished && !d.left.contains(&racer) {
                    d.left.push(racer);
                }
                d.notices.push_back((format!("{name} left"), now));
                while d.notices.len() > MAX_NOTICES {
                    d.notices.pop_front();
                }
            }
            ToClient::KeepPass(pass) => {
                // A host may only hand out a way back into itself: otherwise any
                // room could fill My Keeps with Keeps it does not run.
                let proven = self.host.as_ref().map(|h| h.pubkey.eq_ignore_ascii_case(&crate::invite::hex_encode(&pass.host_key)));
                if proven == Some(false) {
                    log::warn!("[client] a keep pass for another host was ignored");
                } else if self.keep_passes.len() < MAX_KEEP_PASSES && !self.keep_passes.contains(&pass) {
                    self.keep_passes.push(pass);
                }
            }
            // Surfaced only; the session acts on nothing here.
            ToClient::Migration(part) => {
                let event = match self.migration_rx.push::<ToClientMigration>(part) {
                    Ok(None) => return,
                    Ok(Some(payload)) => MigrationEvent::Received(payload),
                    Err(e) => {
                        log::debug!("[client] migration record dropped: {e:?}");
                        MigrationEvent::Rejected(e)
                    }
                };
                let pending = &mut self.migration_events;
                let queued = crate::host::migration::wire::queue_event(pending, (), event);
                if !queued {
                    self.migration_dropped += 1;
                }
            }
        }
    }

    fn on_track_data(&mut self, hash: u64, idx: u8, total: u8, bytes: Vec<u8>) {
        if self.lobby.as_ref().and_then(|l| l.custom) != Some(hash) || !matches!(self.phase, ClientPhase::Lobby) {
            return; // A delayed chunk from another selection cannot replace this one.
        }
        if self.custom.as_ref().is_some_and(|t| t.hash == hash) {
            self.send(&ToHost::TrackReady { hash });
            return;
        }
        let result = (|| {
            if self.download.is_none() { self.download = Some(Download::new(hash, total)?); }
            self.download.as_mut().expect("created").push(hash, idx, total, bytes)
        })();
        match result {
            Ok(Some(t)) => {
                if let Some(pending)=self.migration_custom.as_mut(){pending.push(t.def.clone());}else{custom::install(t.def.clone());}
                self.custom = Some(t);
                self.download = None;
                self.track_error = None;
                self.send(&ToHost::TrackReady { hash });
            }
            Ok(None) => {}
            Err(e) => { self.track_error = Some(e); self.download = None; }
        }
    }

    fn on_race_start(&mut self, config: race_core::RaceConfig, custom: Option<u64>, start_host_ms: u64, you: usize, race_no: u8) {
        // `World::new` keeps at most 8 racers: check against the world built.
        if you >= config.racers.len().min(proto::MAX_SEATS) {
            return;
        }
        let world = if config.track == TrackId::Custom {
            let selected = custom.and_then(|hash| self.custom.as_ref().filter(|t| t.hash == hash));
            let Some(t) = selected.filter(|t| self.lobby.as_ref().and_then(|l| l.custom) == Some(t.hash)) else { return };
            if config.mode != RaceMode::Race || config.drill.is_some() || !(1..=9).contains(&config.laps) { return; }
            World::with_track(config, t.built.clone())
        } else {
            if custom.is_some() { return; }
            World::new(config)
        };
        let view = RaceView::from_world(world, you);
        let w = &view.world;
        if you >= w.racers.len() {
            return;
        }
        let n = w.racers.len();
        self.race = Some(Race {
            race_no,
            you,
            start_ms: start_host_ms,
            go_tick: w.go_tick,
            track: w.track.clone(),
            tun: w.tun.clone(),
            pred: w.racers[you].clone(),
            predicting: true,
            over: false,
            next_seq: 1,
            ring: VecDeque::new(),
            pred_log: VecDeque::new(),
            live_log: VecDeque::new(),
            coin_homes: w.coins.iter().map(|c| c.home).collect(),
            coin_bits: proto::pack_bits(w.coins.iter().map(|_| true)),
            coin_taken: vec![false; w.coins.len()],
            snaps: VecDeque::new(),
            newest_tick: 0,
            corr_from: Vec3::ZERO,
            corr_age: 1.0,
            corr_ms: CORRECTION_MS,
            last_now: self.now,
            launch_local: None,
            events: Vec::new(),
            cd_last: None,
            gait_prev: vec![None; n],
            finish_times: vec![None; n],
            battle: w.battle.is_some(),
            battle_ev: w.battle.clone(),
            render_tick: 0.0,
            lateness: VecDeque::new(),
            delay_ms: INTERP_DELAY_MS,
            render_ms: None,
            render_rate: 1.0,
        });
        self.view = Some(view);
        self.results = None;
        self.rematch_ends = None;
        self.departures = Departures::default();
        self.phase = ClientPhase::Racing;
    }

    fn on_snapshot(&mut self, s: Snapshot, now: u64) {
        let Some(race) = self.race.as_mut() else { return };
        if s.race != race.race_no {
            return; // a straggler from the previous race
        }
        self.stats.snapshots += 1;
        if let Some(host_now) = self.clock.host_now(now) {
            race.note_lateness(s.tick, host_now, now);
        }
        if let Some((d, snapped)) = race.on_snapshot(s) {
            self.stats.last_correction_m = d;
            self.stats.max_correction_m = self.stats.max_correction_m.max(d);
            if snapped {
                self.stats.hard_snaps += 1;
            }
        }
    }

    fn lead_ms(&self) -> i64 {
        let one_way = self.clock.rtt_ms().unwrap_or(0).div_ceil(2) as i64;
        one_way + input_tick_ms(JITTER_TICKS as i64) + 1
    }

    /// Race frame: input production and prediction, then the view.
    fn race_frame(&mut self, now: u64, input: Input) {
        let Some(host_now) = self.clock.host_now(now) else { return };
        let lead = self.lead_ms();
        let Some(race) = self.race.as_mut() else { return };
        let frame_ms = now.saturating_sub(race.last_now) as f64;
        let dt = (frame_ms as f32 / 1000.0).min(MAX_FRAME_S);
        race.last_now = now;
        race.advance_render(host_now, frame_ms);
        race.corr_age += dt;

        if !race.over {
            // Input numbers due: input n is needed by the host at
            // start + (n-1)/60 s; we are `lead` early.
            // i128 and a cap: `start_ms` is the host's to choose.
            let t_ms = host_now as i128 + lead as i128 - race.start_ms as i128;
            let due: u32 = if t_ms < 0 { 0 } else { (t_ms * TICK_HZ as i128 / 1000 + 1).min(u32::MAX as i128 / 2) as u32 };
            if due.saturating_sub(race.next_seq) > MAX_CATCH_UP_TICKS {
                race.next_seq = due - 2;
            }
            let mut sent_any = false;
            while race.next_seq <= due {
                race.produce(input);
                sent_any = true;
            }
            if sent_any {
                // The newest inputs with consecutive numbers (a skip-ahead leaves a gap).
                let seq = race.next_seq - 1;
                let n = race
                    .ring
                    .iter()
                    .rev()
                    .take(INPUTS_PER_PACKET)
                    .enumerate()
                    .take_while(|(k, (s, _))| *s == seq - *k as u32)
                    .count();
                let inputs: Vec<[u8; 4]> = race.ring.iter().skip(race.ring.len() - n).map(|(_, p)| *p).collect();
                let bytes = encode(&LossyToHost::Inputs { race: race.race_no, seq, inputs });
                self.stats.inputs_sent += 1;
                self.transport.send_lossy(bytes);
            }
        }
        let ping = self.rtt_ms();
        if let (Some(race), Some(view)) = (self.race.as_mut(), self.view.as_mut()) {
            race.build_view(view, host_now, ping);
            self.stats.interp_delay_ms = race.render_ms.map_or(race.delay_ms, |r| host_now as f64 - r) as f32;
            if !race.over && !race.snaps.is_empty() {
                self.stats.interp_frames += 1;
                let ahead = race.render_tick - race.newest_tick as f64;
                if ahead > 0.0 {
                    self.stats.interp_starved_frames += 1;
                }
                if ahead > EXTRAPOLATE_TICKS {
                    self.stats.interp_frozen_frames += 1;
                }
            }
            if let Some(res) = &self.results {
                view.freeze_places(res);
                view.hud.place = view.world.racers[view.you].place;
            }
        }
    }
}

pub struct MigrationClientBatch {
    pub reliable:Vec<(u64,Vec<u8>)>,
    pub lossy:Vec<Vec<u8>>,
    pub closed:bool,
}
struct MigrationCapture {
    incoming:VecDeque<ClientNetEvent>,reliable:Vec<Vec<u8>>,lossy:Vec<Vec<u8>>,bytes:usize,rtt:Option<u32>,closed:bool,overflow:bool,
}
impl MigrationCapture {
    fn send(&mut self,bytes:Vec<u8>,reliable:bool) {
        use decented_net::migration::{MAX_JOURNAL_BYTES,MAX_JOURNAL_RECORDS};
        if self.overflow || bytes.len()>decented_net::MAX_RELIABLE_LEN || self.reliable.len()+self.lossy.len()>=MAX_JOURNAL_RECORDS || self.bytes.checked_add(bytes.len()).is_none_or(|n|n>MAX_JOURNAL_BYTES){self.overflow=true;return;}
        self.bytes+=bytes.len();if reliable{self.reliable.push(bytes);}else{self.lossy.push(bytes);}
    }
}
struct MigrationClientTransport(std::sync::Arc<std::sync::Mutex<MigrationCapture>>);
impl ClientTransport for MigrationClientTransport {
    fn poll(&mut self)->Option<ClientNetEvent>{self.0.lock().unwrap().incoming.pop_front()}
    fn send_reliable(&mut self,bytes:Vec<u8>){self.0.lock().unwrap().send(bytes,true);}
    fn send_lossy(&mut self,bytes:Vec<u8>){self.0.lock().unwrap().send(bytes,false);}
    fn rtt_ms(&self)->Option<u32>{self.0.lock().unwrap().rtt}
    fn close(&mut self){self.0.lock().unwrap().closed=true;}
}

impl RaceDriver for ClientSession {
    fn update(&mut self, now_ms: u64, input: Input) {
        ClientSession::update(self, now_ms, input)
    }

    fn view(&self) -> Option<&RaceView> {
        ClientSession::view(self)
    }

    fn results(&self) -> Option<&ResultsView> {
        ClientSession::results(self)
    }
}

// ─── Prediction ─────────────────────────────────────────────────────────────

/// What one predicted tick did.
enum Step {
    /// A countdown tick (and the launch, on the last one).
    Grid(Option<Launch>),
    /// A racing tick.
    Motion(MotionFx),
}

/// One predicted tick: what the world does to a human racer's motion, with
/// race-core's own per-racer steps (`Racer::countdown_step` before GO,
/// `Racer::integrate` after).
fn predict_step(pred: &mut Racer, tick: u64, go_tick: u64, input: &Input, track: &Track, tun: &Tunables) -> Step {
    if tick < go_tick {
        Step::Grid(pred.countdown_step(input, tick + 1 >= go_tick, tun))
    } else {
        Step::Motion(pred.integrate(input, track, tun))
    }
}

/// Whether every racer / target index in a host event is in range.
fn event_in_range(e: &Event, racers: usize, targets: usize) -> bool {
    let ok = |i: u8| (i as usize) < racers;
    let opt = |i: Option<u8>| i.is_none_or(ok);
    match e {
        Event::Countdown { .. } | Event::Go | Event::RaceOver => true,
        Event::LessonProgress { .. } | Event::LessonDone { .. } | Event::LessonFailed => true,
        Event::ChallengeScore { .. } | Event::ChallengeDone { .. } => true,
        Event::DriftPoints { racer, .. } | Event::Eliminated { racer, .. } => ok(*racer),
        Event::StartBoost { racer }
        | Event::Stumble { racer }
        | Event::Footstep { racer }
        | Event::Jump { racer }
        | Event::DriftHop { racer }
        | Event::DriftStart { racer, .. }
        | Event::DriftTier { racer, .. }
        | Event::Boost { racer, .. }
        | Event::BoostPad { racer }
        | Event::Trick { racer }
        | Event::TrickBoost { racer }
        | Event::Landed { racer, .. }
        | Event::WallHit { racer, .. }
        | Event::Fell { racer }
        | Event::Respawned { racer }
        | Event::CoinPicked { racer, .. }
        | Event::CoinsDropped { racer, .. }
        | Event::ItemUsed { racer, .. }
        | Event::BashWhiff { racer }
        | Event::Overheated { racer }
        | Event::ShieldUp { racer }
        | Event::MegaLaser { racer }
        | Event::HurdleCleared { racer }
        | Event::Gate { racer }
        | Event::Lap { racer, .. }
        | Event::FinalLap { racer }
        | Event::Finished { racer, .. }
        | Event::WrongWay { racer, .. } => ok(*racer),
        Event::TargetDestroyed { racer, target, .. } => ok(*racer) && (*target as usize) < targets,
        Event::Hit { victim, by, .. } => ok(*victim) && opt(*by),
        Event::Bash { attacker, victim, .. } => ok(*attacker) && ok(*victim),
        Event::LaserZap { by, victim } => ok(*by) && ok(*victim),
        Event::ShieldBlock { racer, by } => ok(*racer) && opt(*by),
        Event::Lightning { by } => ok(*by),
        Event::FeatherLost { racer, by } => ok(*racer) && opt(*by),
        Event::FeatherStolen { racer, from } => ok(*racer) && ok(*from),
        Event::Out { racer, .. } => ok(*racer),
        Event::CoinsTaken { from, to, .. } => ok(*from) && ok(*to),
        Event::BattleOver | Event::Drill(_) => true,
    }
}

/// Rose sellers a fraction `t` of the way from snapshot `a` to `b`, matched by
/// owner. One only in `a` (gone) shows for the first half, one only in `b` (new)
/// for the second; one that jumped far snaps across at the middle.
fn lerp_roses(a: &[RoseSeller], b: &[RoseSeller], t: f32) -> Vec<RoseSeller> {
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    for rb in b {
        match a.iter().find(|r| r.owner == rb.owner) {
            Some(ra) if ra.pos.distance(rb.pos) <= 20.0 => out.push(RoseSeller {
                pos: ra.pos.lerp(rb.pos, t),
                yaw: wrap_angle(ra.yaw + wrap_angle(rb.yaw - ra.yaw) * t),
                lateral: lerp(ra.lateral, rb.lateral, t),
                ..if t < 0.5 { *ra } else { *rb }
            }),
            Some(ra) => out.push(if t < 0.5 { *ra } else { *rb }),
            None if t >= 0.5 => out.push(*rb),
            None => {}
        }
    }
    if t < 0.5 {
        out.extend(a.iter().filter(|ra| !b.iter().any(|r| r.owner == ra.owner)));
    }
    out
}

#[cfg(test)]
mod rose_view_tests {
    use super::*;

    fn seller(owner: u8, pos: Vec3) -> RoseSeller {
        RoseSeller { pos, yaw: 0.0, s: pos.z, lateral: pos.x, owner, victim: 0, tried: 0, again: 0, side: 1, t: 2.0 }
    }

    #[test]
    fn sellers_move_smoothly_between_snapshots_whatever_order_they_come_in() {
        let a = [seller(2, Vec3::ZERO), seller(5, Vec3::new(10.0, 0.0, 0.0)), seller(6, Vec3::ZERO)];
        // Nearest first, so another order; 2 moved on, 6 is gone, 7 is new, and
        // 5 jumped (a new victim far away).
        let b = [
            seller(7, Vec3::ONE),
            seller(5, Vec3::new(10.0, 0.0, 100.0)),
            seller(2, Vec3::new(0.0, 0.0, 4.0)),
        ];
        let at = |t: f32, owner: u8| lerp_roses(&a, &b, t).into_iter().find(|r| r.owner == owner);
        assert_eq!(at(0.25, 2).unwrap().pos, Vec3::new(0.0, 0.0, 1.0));
        assert_eq!(at(0.75, 2).unwrap().pos, Vec3::new(0.0, 0.0, 3.0));
        assert_eq!(at(0.25, 5).unwrap().pos, a[1].pos, "a jump snaps across at the middle");
        assert_eq!(at(0.75, 5).unwrap().pos, b[1].pos);
        assert!(at(0.25, 6).is_some() && at(0.75, 6).is_none(), "gone");
        assert!(at(0.25, 7).is_none() && at(0.75, 7).is_some(), "new");
        assert!(lerp_roses(&a, &b, 0.5).len() <= race_core::playful::MAX_ROSES);
    }
}

/// Clamp the parts of a host-sent racer that the prediction indexes with
/// (a hostile host must not be able to crash the client).
fn sanitize_own(r: &mut Racer) {
    r.drift.tier = r.drift.tier.min(3);
}

#[cfg(test)]
mod effect_replay_tests {
    use super::*;

    #[test]
    fn snapshot_effect_starts_transitions_and_expiry_replay_the_host_motion() {
        use race_core::{playful::Effects, BotLevel, RaceConfig, World};
        let cases = [
            Effects {
                burn: 4.0,
                ..Default::default()
            },
            Effects {
                cake_eat: 2.5,
                ..Default::default()
            },
            Effects {
                pill: 7.0,
                ..Default::default()
            },
            Effects {
                tumble: 2.8,
                ..Default::default()
            },
            Effects {
                splat: 3.0,
                laptop: 2.0,
                ..Default::default()
            },
            Effects {
                dismount: 8.0,
                chase_x: -5.0,
                chase_z: 2.0,
                ..Default::default()
            },
            Effects {
                printer: 5.0,
                ..Default::default()
            },
            Effects {
                rose: 5.0,
                rose_side: -1,
                ..Default::default()
            },
            Effects {
                dismount: 8.0,
                chase_x: -8.0,
                chase_z: -1.5,
                le_mans: true,
                ..Default::default()
            },
        ];
        for effects in cases {
            let mut cfg = RaceConfig::quick(race_core::TrackId::Field, 0, BotLevel::Normal, 8);
            cfg.mode = RaceMode::TimeTrial;
            let mut world = World::new(cfg);
            world.phase = RacePhase::Racing;
            world.tick = world.go_tick;
            world.racers[0].effects = effects;
            // This is the actual snapshot codec and full-own state used to
            // restart prediction; no guessed local item timers are involved.
            let mut predicted: Racer = proto::decode(&proto::encode(&world.racers[0])).unwrap();
            for k in 0..600 {
                let input = if world.racers[0].effects.dismount > 0.0 {
                    race_core::bot_input(&world, 0, BotLevel::Normal)
                } else {
                    Input { steer: (k as f32 * 0.013).sin() * 0.1, ..Input::default() }
                };
                let tick = world.tick;
                world.step(&[input]);
                predict_step(&mut predicted, tick, world.go_tick, &input, &world.track, &world.tun);
                assert!(predicted.pos.distance(world.racers[0].pos) < 0.00001, "{effects:?} tick {k}");
                assert_eq!(predicted.effects, world.racers[0].effects, "tick {k}");
            }
            assert_eq!(predicted.effects, Effects::default());
        }
    }

    #[test]
    fn a_rose_sellers_shove_replays_the_host_motion_for_the_whole_sale() {
        use race_core::playful::{Effects, RoseSeller, ROSE_SALE, ROSE_SIDE};
        use race_core::{BotLevel, RaceConfig, World};
        let mut cfg = RaceConfig::quick(race_core::TrackId::Field, 0, BotLevel::Normal, 8);
        cfg.mode = RaceMode::TimeTrial;
        let mut world = World::new(cfg);
        world.phase = RacePhase::Racing;
        world.tick = world.go_tick;
        // A seller sent by someone else arrives beside racer 0 on the next tick.
        let r = &world.racers[0];
        world.play.roses.push(RoseSeller {
            pos: world.track.point_at(r.s, r.lateral + ROSE_SIDE),
            yaw: r.yaw,
            s: r.s,
            lateral: r.lateral + ROSE_SIDE,
            owner: 1,
            victim: 0,
            tried: 0,
            again: 0,
            side: 1,
            t: 0.0,
        });
        world.step(&[Input::default()]);
        assert_eq!(world.racers[0].effects.rose, ROSE_SALE, "the sale started");
        assert_eq!(world.racers[0].effects.rose_side, 1);
        let mut predicted: Racer = proto::decode(&proto::encode(&world.racers[0])).unwrap();
        let mut calm = predicted.clone();
        calm.effects = Effects::default();
        let mut pushed = 0.0f32;
        let mut sold = 0;
        for k in 0..((ROSE_SALE + 1.0) / DT) as usize {
            let input = Input { steer: (k as f32 * 0.013).sin() * 0.1, ..Input::default() };
            let tick = world.tick;
            world.step(&[input]);
            predict_step(&mut predicted, tick, world.go_tick, &input, &world.track, &world.tun);
            predict_step(&mut calm, tick, world.go_tick, &input, &world.track, &world.tun);
            assert!(predicted.pos.distance(world.racers[0].pos) < 0.00001, "tick {k}");
            assert_eq!(predicted.vel, world.racers[0].vel, "tick {k}");
            assert_eq!(predicted.effects, world.racers[0].effects, "tick {k}");
            if world.racers[0].effects.rose > 0.0 {
                sold += 1;
            }
            pushed = pushed.max(calm.lateral - predicted.lateral);
        }
        assert!(sold as f32 * DT > ROSE_SALE - 0.1, "sold for {sold} ticks");
        assert!(pushed > 0.5, "leaned on, away from the seller: {pushed} m");
        assert_eq!(predicted.effects, Effects::default());
    }

    #[test]
    fn laser_trump_rocket_and_its_exit_boost_replay_the_host_motion() {
        use race_core::{playful::Effects, BotLevel, RaceConfig, World};
        let mut cfg = RaceConfig::quick(race_core::TrackId::Field, 2, BotLevel::Normal, 8);
        cfg.mode = RaceMode::Race;
        let mut world = World::new(cfg);
        world.phase = RacePhase::Racing;
        world.tick = world.go_tick;
        // The bots lead, so the host's top-two wind-down never touches the rocket.
        world.racers[1].lap = 3;
        world.racers[2].lap = 3;
        world.racers[0].effects.trump = 3.0;
        let mut predicted: Racer = proto::decode(&proto::encode(&world.racers[0])).unwrap();
        let mut fastest = 0.0f32;
        for k in 0..600 {
            // Player steering and a drift press are ignored by the rocket.
            let input = Input {
                steer: (k as f32 * 0.05).sin(),
                drift: k % 50 < 25,
                ..Input::default()
            };
            let tick = world.tick;
            world.step(&[input]);
            predict_step(
                &mut predicted,
                tick,
                world.go_tick,
                &input,
                &world.track,
                &world.tun,
            );
            assert!(
                predicted.pos.distance(world.racers[0].pos) < 0.00001,
                "tick {k}"
            );
            assert_eq!(predicted.effects, world.racers[0].effects, "tick {k}");
            assert_eq!(predicted.boost_t, world.racers[0].boost_t, "tick {k}");
            fastest = fastest.max(predicted.speed);
        }
        assert!(
            fastest > world.tun.top_speed * 1.5,
            "rocket speed {fastest}"
        );
        assert_eq!(predicted.effects, Effects::default());
    }
}

/// The events a predicted tick would have raised (host copies of these for
/// our own racer are dropped).
fn fx_events(you: u8, fx: &MotionFx, dir: i8, out: &mut Vec<Event>) {
    let racer = you;
    if fx.footstep {
        out.push(Event::Footstep { racer });
    }
    if fx.jumped {
        out.push(Event::Jump { racer });
    }
    if fx.drift_start {
        out.push(Event::DriftStart { racer, dir });
    }
    if fx.drift_tier > 0 {
        out.push(Event::DriftTier { racer, tier: fx.drift_tier });
    }
    if fx.drift_boost > 0 {
        out.push(Event::Boost { racer, tier: fx.drift_boost });
    }
    if fx.boost_pad {
        out.push(Event::BoostPad { racer });
    }
    if fx.trick {
        out.push(Event::Trick { racer });
    }
    if fx.trick_boost {
        out.push(Event::TrickBoost { racer });
    }
    if fx.landed {
        out.push(Event::Landed { racer, hard: fx.hard_landing });
    }
    if fx.wall_hit > 0.2 {
        out.push(Event::WallHit { racer, strength: fx.wall_hit });
    }
    if fx.fell {
        out.push(Event::Fell { racer });
    }
    if fx.respawned {
        out.push(Event::Respawned { racer });
    }
    if fx.overheated {
        out.push(Event::Overheated { racer });
    }
}

/// Whether a host event is one the prediction already raised locally.
/// Folds a reliable battle event into the event-derived battle state.
fn note_battle_event(b: &mut BattleState, e: &Event) {
    let n = b.feathers.len();
    match *e {
        Event::FeatherLost { racer, by } if (racer as usize) < n => {
            let r = racer as usize;
            b.feathers[r] = b.feathers[r].saturating_sub(1);
            b.taken[r] = b.taken[r].saturating_add(1);
            if let Some(a) = by.map(usize::from).filter(|&a| a < n && a != r) {
                b.hits[a] = b.hits[a].saturating_add(1);
            }
        }
        Event::FeatherStolen { racer, .. } if (racer as usize) < n => {
            let r = racer as usize;
            b.feathers[r] = b.feathers[r].saturating_add(1);
        }
        Event::Out { racer, .. } if (racer as usize) < n => {
            let r = racer as usize;
            if !b.is_out(r) {
                b.out_order.push(r);
            }
            b.feathers[r] = 0;
        }
        Event::BattleOver => b.done = true,
        _ => {}
    }
}

/// Puts the event-derived battle state over the one the newest snapshot wrote,
/// wherever the snapshot is behind it (never where it is ahead).
fn merge_battle_events(ev: &BattleState, b: &mut BattleState) {
    let n = b.feathers.len().min(ev.feathers.len());
    for i in 0..n {
        let behind = ev.hits[i] > b.hits[i] || ev.taken[i] > b.taken[i];
        b.hits[i] = b.hits[i].max(ev.hits[i]);
        b.taken[i] = b.taken[i].max(ev.taken[i]);
        if behind {
            b.feathers[i] = ev.feathers[i];
        }
    }
    for &i in &ev.out_order {
        if i < n && !b.is_out(i) {
            b.out_order.push(i);
        }
    }
    for i in 0..n {
        if b.is_out(i) {
            b.feathers[i] = 0;
        }
    }
    b.done |= ev.done;
}

fn is_own_motion_event(e: &Event, you: u8) -> bool {
    match e {
        Event::Jump { racer }
        | Event::DriftHop { racer }
        | Event::DriftStart { racer, .. }
        | Event::DriftTier { racer, .. }
        | Event::Boost { racer, .. }
        | Event::BoostPad { racer }
        | Event::Trick { racer }
        | Event::TrickBoost { racer }
        | Event::Landed { racer, .. }
        | Event::WallHit { racer, .. }
        | Event::Fell { racer }
        | Event::Respawned { racer }
        | Event::Overheated { racer } => *racer == you,
        _ => false,
    }
}

impl Race {
    fn on_host_event(&mut self, e: Event, host_now: Option<u64>) {
        if let Some(b) = self.battle_ev.as_mut() {
            note_battle_event(b, &e);
        }
        if is_own_motion_event(&e, self.you as u8) {
            return;
        }
        // The start boost / stumble the prediction already raised.
        let launch = match e {
            Event::StartBoost { racer } if racer as usize == self.you => Some(Launch::StartBoost),
            Event::Stumble { racer } if racer as usize == self.you => Some(Launch::Stumble),
            _ => None,
        };
        if launch.is_some() {
            let go_ms = self.start_ms.saturating_add(self.go_tick * 1000 / TICK_HZ as u64);
            let in_window = host_now.is_none_or(|t| t <= go_ms.saturating_add(LAUNCH_DEDUPE_MS));
            // A host verdict the prediction did not reach is still played.
            if in_window && self.launch_local == launch {
                self.launch_local = None;
                return;
            }
        }
        if let Event::Finished { racer, time, .. } = &e {
            if let Some(slot) = self.finish_times.get_mut(*racer as usize) {
                *slot = Some(*time);
            }
        }
        self.events.push(e);
    }

    fn log_pred(&mut self, seq: u32) {
        let pos = self.pred.pos;
        match self.pred_log.back() {
            Some(&(last, _)) if seq <= last => {
                if let Some(e) = self.pred_log.iter_mut().find(|(s, _)| *s == seq) {
                    e.1 = pos;
                }
            }
            _ => {
                self.pred_log.push_back((seq, pos));
                while self.pred_log.len() > INPUT_RING {
                    self.pred_log.pop_front();
                }
            }
        }
    }

    /// Take the next input number: record it and step the prediction.
    fn produce(&mut self, input: Input) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let packed = input.pack();
        self.ring.push_back((seq, packed));
        while self.ring.len() > INPUT_RING {
            self.ring.pop_front();
        }
        if self.predicting {
            let q = Input::unpack(packed);
            let racer = self.you as u8;
            match predict_step(&mut self.pred, seq as u64 - 1, self.go_tick, &q, &self.track, &self.tun) {
                Step::Motion(fx) => {
                    fx_events(racer, &fx, self.pred.drift.dir, &mut self.events);
                    self.predict_coins();
                }
                Step::Grid(Some(l)) => {
                    // Raised the instant it happens; the host's copy is dropped.
                    self.events.push(match l {
                        Launch::StartBoost => Event::StartBoost { racer },
                        Launch::Stumble => Event::Stumble { racer },
                    });
                    self.launch_local = Some(l);
                }
                Step::Grid(None) => {}
            }
            self.log_pred(seq);
            self.live_log.push_back((seq, self.pred.pos));
            while self.live_log.len() > INPUT_RING {
                self.live_log.pop_front();
            }
        }
    }

    /// A snapshot arrived. Returns the reconciliation error `(metres, snapped)`
    /// when it was the newest and the racer is predicted.
    fn on_snapshot(&mut self, s: Snapshot) -> Option<(f32, bool)> {
        let newest = s.tick > self.newest_tick || self.snaps.is_empty();
        // Keep the buffer sorted by tick.
        if self.snaps.iter().any(|x| x.tick == s.tick) {
            return None;
        }
        let pos = self.snaps.iter().position(|x| x.tick > s.tick).unwrap_or(self.snaps.len());
        // Out of a battle: the host froze us; stop predicting and show its state.
        let own_out = self.battle && s.battle.as_ref().is_some_and(|b| b.is_out(self.you));
        let snap_over = s.is_over() || own_out;
        let mut own = s.own.clone();
        sanitize_own(&mut own);
        let ack = s.ack_seq;
        self.snaps.insert(pos, s);
        while self.snaps.len() > SNAP_BUFFER {
            self.snaps.pop_front();
        }
        if !newest {
            return None;
        }
        self.newest_tick = self.snaps.back().map_or(0, |x| x.tick);

        if snap_over || own.finished_at.is_some() {
            self.predicting = false;
        }
        if !self.predicting {
            self.pred = own;
            self.corr_from = Vec3::ZERO;
            return None;
        }

        let old_pos = self.pred.pos;
        self.pred = own;
        if let Some(newest) = self.snaps.back() {
            self.coin_bits = newest.coins_alive.clone();
        }
        self.coin_taken.iter_mut().for_each(|t| *t = false);
        while self.ring.front().is_some_and(|(s, _)| *s <= ack) {
            self.ring.pop_front();
        }
        let replay: Vec<(u32, [u8; 4])> = self.ring.iter().copied().collect();
        for (seq, p) in replay {
            let q = Input::unpack(p);
            // Replays raise no events: they were raised when first predicted.
            if let Step::Motion(_) = predict_step(&mut self.pred, seq as u64 - 1, self.go_tick, &q, &self.track, &self.tun) {
                self.predict_coins();
            }
            self.log_pred(seq);
        }
        let delta = old_pos - self.pred.pos;
        let total = self.current_correction() + delta;
        let snapped = !total.is_finite() || total.length() > SNAP_DISTANCE;
        self.corr_from = if snapped { Vec3::ZERO } else { total };
        self.corr_ms = correction_ms(self.corr_from.length());
        self.corr_age = 0.0;
        Some((delta.length(), snapped))
    }

    /// The world's coin pickup, replayed on the prediction: a coin the newest
    /// snapshot shows alive and within reach gives the same count and
    /// micro-boost the host will give (a few cm of motion otherwise).
    fn predict_coins(&mut self) {
        for i in 0..self.coin_homes.len() {
            if self.coin_taken[i] || !bit_at(&self.coin_bits, i) || !self.pred.reaches_coin(self.coin_homes[i], &self.tun) {
                continue;
            }
            self.coin_taken[i] = true;
            self.pred.pick_coin(&self.tun);
        }
    }

    /// A snapshot of `tick` arrived at `local` ms, the host clock reading
    /// `host_now` (estimated).
    fn note_lateness(&mut self, tick: u32, host_now: u64, local: u64) {
        let sent = self.start_ms as f64 + tick as f64 * 1000.0 / TICK_HZ as f64;
        self.lateness.push_back((local, host_now as f64 - sent));
        while self.lateness.front().is_some_and(|(t, _)| t + LATENESS_WINDOW_MS < local) {
            self.lateness.pop_front();
        }
        while self.lateness.len() > 4 * SNAP_BUFFER * LATENESS_WINDOW_MS as usize / 1000 {
            self.lateness.pop_front();
        }
    }

    /// Move the others' clock on by a frame of `frame_ms`: toward
    /// `host_now - delay`, at between [`RENDER_RATE_MIN`] and
    /// [`RENDER_RATE_MAX`] of real time.
    fn advance_render(&mut self, host_now: u64, frame_ms: f64) {
        let dt_ms = frame_ms.clamp(0.0, RENDER_MAX_FRAME_MS);
        let late: Vec<f64> = self.lateness.iter().map(|(_, l)| *l).collect();
        let target = interp_target_ms(&late);
        // Grow at once (the clock slows to it); shrink slowly.
        self.delay_ms = target.max(self.delay_ms - DELAY_FALL_PER_S * dt_ms / 1000.0);
        let goal = host_now as f64 - self.delay_ms;
        self.render_ms = Some(match self.render_ms {
            Some(r) if (goal - r).abs() <= RENDER_SNAP_MS => {
                let r = r + dt_ms;
                let mut rate = (1.0 + (goal - r) / RENDER_STEER_MS).clamp(RENDER_RATE_MIN, RENDER_RATE_MAX);
                if let Some(newest) = self.snaps.back() {
                    // Close to the newest snapshot: slow down rather than run
                    // past it and jump when the late ones come in.
                    let newest_ms = self.start_ms as f64 + newest.tick as f64 * 1000.0 / TICK_HZ as f64;
                    let room = ((newest_ms - r) / RENDER_HEADROOM_MS).clamp(0.0, 1.0);
                    if room < 1.0 {
                        rate = rate.min(RENDER_RATE_STARVED + (1.0 - RENDER_RATE_STARVED) * room);
                    }
                }
                // Change speed gradually (a sudden change reads as a jolt).
                let step = RENDER_RATE_CHANGE_PER_S * dt_ms / 1000.0;
                let rate = rate.clamp(self.render_rate - step, self.render_rate + step);
                self.render_rate = rate;
                // `r` already ran a full frame: take back what the rate does not allow.
                r - dt_ms * (1.0 - rate)
            }
            _ => {
                self.render_rate = 1.0;
                goal
            }
        });
    }

    /// The frogs drawn at `render_tick`, between the two snapshots around it
    /// (they come in a fixed order).
    fn interpolate_frogs(&self, frogs: &mut [race_core::playful::Frog], render_tick: f64) {
        let Some((a, b, t)) = self.bracket(render_tick) else { return };
        if std::ptr::eq(a, b) {
            // Past the newest snapshot: carry on along the last two (briefly).
            let n = self.snaps.len();
            if n < 2 || !std::ptr::eq(a, &self.snaps[n - 1]) {
                return;
            }
            let prev = &self.snaps[n - 2];
            let gap = (a.tick - prev.tick).max(1) as f32;
            let ahead = (render_tick - a.tick as f64).clamp(0.0, EXTRAPOLATE_TICKS) as f32;
            for (k, f) in frogs.iter_mut().enumerate() {
                let (Some(pa), Some(pp)) = (a.play.frogs.get(k), prev.play.frogs.get(k)) else { continue };
                if pa.pos.distance(pp.pos) <= 20.0 && pa.tumble <= 0.0 {
                    f.pos = pa.pos + (pa.pos - pp.pos) * (ahead / gap);
                }
            }
            return;
        }
        let t = t as f32;
        let (fa, fb) = (&a.play.frogs, &b.play.frogs);
        for (k, f) in frogs.iter_mut().enumerate() {
            let (Some(pa), Some(pb)) = (fa.get(k), fb.get(k)) else { continue };
            *f = if pa.pos.distance(pb.pos) > 20.0 {
                if t < 0.5 { *pa } else { *pb }
            } else {
                race_core::playful::Frog {
                    pos: pa.pos.lerp(pb.pos, t),
                    yaw: wrap_angle(pa.yaw + wrap_angle(pb.yaw - pa.yaw) * t),
                    tumble: lerp(pa.tumble, pb.tumble, t),
                    ..if t < 0.5 { *pa } else { *pb }
                }
            };
        }
    }

    /// The rose sellers drawn at `render_tick`, between the two snapshots around
    /// it. Matched by owner (one seller each): a snapshot sends the nearest first.
    fn interpolate_roses(&self, roses: &mut Vec<RoseSeller>, render_tick: f64) {
        let Some((a, b, t)) = self.bracket(render_tick) else { return };
        if !std::ptr::eq(a, b) {
            *roses = lerp_roses(&a.play.roses, &b.play.roses, t as f32);
            return;
        }
        // Past the newest snapshot: carry on along the last two (briefly).
        let n = self.snaps.len();
        if n < 2 || !std::ptr::eq(a, &self.snaps[n - 1]) {
            return;
        }
        let prev = &self.snaps[n - 2];
        let gap = (a.tick - prev.tick).max(1) as f32;
        let ahead = (render_tick - a.tick as f64).clamp(0.0, EXTRAPOLATE_TICKS) as f32;
        for r in roses.iter_mut() {
            if let Some(p) = prev.play.roses.iter().find(|p| p.owner == r.owner) {
                if r.pos.distance(p.pos) <= 20.0 {
                    r.pos += (r.pos - p.pos) * (ahead / gap);
                }
            }
        }
    }

    /// The visual offset right now: starts at `corr_from`, eases to 0 over `corr_ms`.
    fn current_correction(&self) -> Vec3 {
        self.corr_from * correction_left(self.corr_age * 1000.0, self.corr_ms)
    }

    // ─── The view ────────────────────────────────────────────────────────

    fn build_view(&mut self, view: &mut RaceView, host_now: u64, ping: Option<u32>) {
        let you = self.you;
        let tick_f = (host_now as f64 - self.start_ms as f64) * TICK_HZ as f64 / 1000.0;
        let tick_now = tick_f.floor() as i64;
        let w = &mut view.world;
        w.tick = tick_now.max(0) as u64;
        w.phase = if self.over {
            RacePhase::Finished
        } else if w.tick >= self.go_tick {
            RacePhase::Racing
        } else {
            RacePhase::Countdown
        };

        // 3-2-1-GO from the synced clock.
        let from = self.cd_last.map_or(tick_now - 1, |c| c + 1).max(tick_now - 30);
        for t in from..=tick_now {
            if t < 0 {
                continue;
            }
            let t = t as u64;
            if t % TICK_HZ as u64 == 0 && t < self.go_tick {
                self.events.push(Event::Countdown { n: ((self.go_tick - t) / TICK_HZ as u64) as u8 });
            }
            if t == self.go_tick {
                self.events.push(Event::Go);
            }
        }
        self.cd_last = Some(tick_now);

        // Racers.
        let render_ms = self.render_ms.unwrap_or(host_now as f64 - self.delay_ms);
        let render_tick = (render_ms - self.start_ms as f64) * TICK_HZ as f64 / 1000.0;
        self.render_tick = render_tick;
        let track = self.track.clone();
        let corr = self.current_correction();
        for i in 0..w.racers.len() {
            if i == you && self.predicting {
                let mut r = self.pred.clone();
                r.pos += corr;
                w.racers[i] = r;
                continue;
            }
            if i == you {
                w.racers[i] = self.pred.clone();
            }
            if let Some(net) = self.interpolate(i, render_tick) {
                let r = &mut w.racers[i];
                r.apply_net(&net.net);
                r.pos = net.pos;
                r.vel = net.vel;
                r.yaw = net.yaw;
                r.visual_yaw = wrap_angle(net.yaw + net.vis_off);
                r.speed = flat(net.vel).dot(forward(net.yaw));
                let pr = track.project(r.pos, r.track_idx as usize);
                r.track_idx = pr.idx as u32;
                r.s = pr.s;
                r.lateral = pr.lateral;
                r.ground_y = pr.ground_y;
                r.progress = r.lap.saturating_sub(1) as f32 * track.length + pr.s;
                if r.finished_at.is_some() {
                    if let Some(Some(t)) = self.finish_times.get(i) {
                        r.finished_at = Some(*t);
                    }
                }
                // Footsteps from the gait phase (the host does not send them).
                let cur = r.gait_phase;
                if let Some(prev) = self.gait_prev[i] {
                    if r.on_ground && r.speed.abs() > 1.0 && ((cur * 2.0).floor() != (prev * 2.0).floor()) {
                        self.events.push(Event::Footstep { racer: i as u8 });
                    }
                }
                self.gait_prev[i] = Some(cur);
            }
        }

        // Eggs, cacti, coins, targets.
        if let Some(newest) = self.snaps.back() {
            for (i, c) in w.coins.iter_mut().enumerate() {
                c.alive = bit_at(&newest.coins_alive, i);
            }
            for (i, t) in w.targets.iter_mut().enumerate() {
                t.alive = bit_at(&newest.targets_alive, i);
            }
            w.play = newest.play.clone();
            w.play.frogs.truncate(3);
            self.interpolate_frogs(&mut w.play.frogs, render_tick);
            self.interpolate_roses(&mut w.play.roses, render_tick);
            if self.predicting {
                // A seller pestering us runs beside where we are drawn, not
                // where the host had us.
                let me = &self.pred;
                for r in w.play.roses.iter_mut().filter(|r| r.selling() && r.victim as usize == you) {
                    r.s = me.s;
                    r.lateral = me.lateral + r.side as f32 * race_core::playful::ROSE_SIDE;
                    r.pos = track.point_at(r.s, r.lateral) + corr;
                }
            }
            if let (Some(bn), Some(b)) = (&newest.battle, w.battle.as_mut()) {
                bn.apply(b);
            }
            w.loose_coins = newest
                .loose_coins
                .iter()
                .map(|p| race_core::LooseCoin {
                    pos: Vec3::from_array(*p),
                    vel: Vec3::ZERO,
                    ttl: 1.0,
                    dropped_by: 0,
                    age: 0.0,
                    hint: 0,
                })
                .collect();
        }
        if let (Some(ev), Some(b)) = (&self.battle_ev, w.battle.as_mut()) {
            merge_battle_events(ev, b);
        }
        self.interpolate_entities(w, render_tick);

        view.you = you;
        view.over = self.over;
        view.correction = if self.predicting { corr } else { Vec3::ZERO };
        view.events = std::mem::take(&mut self.events);
        view.refresh(ping);
        // Continuous clock for the HUD (the world's is tick-quantised).
        if !self.over && !view.hud.finished {
            let t = (tick_f / TICK_HZ as f64) - self.go_tick as f64 / TICK_HZ as f64;
            view.hud.race_time = t.max(0.0) as f32;
            view.hud.countdown = (-t).max(0.0) as f32;
        }
    }

    /// The net state of racer `idx` at `render_tick` on the host's timeline.
    fn interpolate(&self, idx: usize, render_tick: f64) -> Option<Interp> {
        let net_of = |s: &Snapshot| -> Option<RacerNet> {
            if idx == self.you {
                Some(s.own.to_net())
            } else {
                s.others.iter().find(|n| n.idx as usize == idx).copied()
            }
        };
        let (a, b, t) = self.bracket(render_tick)?;
        let na = net_of(a)?;
        let nb = net_of(b).unwrap_or(na);
        if std::ptr::eq(a, b) {
            // Before the oldest or after the newest snapshot: hold, with a
            // short extrapolation past the newest (not across a respawn).
            let ahead = (render_tick - a.tick as f64).clamp(0.0, EXTRAPOLATE_TICKS) as f32 * DT;
            let ahead = if na.flags & (1 << 9) == 0 { ahead } else { 0.0 };
            let vel = net_vel(&na);
            return Some(Interp {
                pos: Vec3::from_array(na.pos) + vel * ahead,
                vel,
                yaw: net_yaw(&na),
                vis_off: na.vis_yaw as f32 / 100.0,
                net: na,
            });
        }
        let t = t as f32;
        let pa = Vec3::from_array(na.pos);
        let pb = Vec3::from_array(nb.pos);
        let teleport = pa.distance(pb) > 20.0;
        let (pos, vel) = if teleport {
            if t < 0.5 {
                (pa, net_vel(&na))
            } else {
                (pb, net_vel(&nb))
            }
        } else {
            (pa.lerp(pb, t), net_vel(&na).lerp(net_vel(&nb), t))
        };
        let (ya, yb) = (net_yaw(&na), net_yaw(&nb));
        let yaw = wrap_angle(ya + wrap_angle(yb - ya) * t);
        let vis_off = lerp(na.vis_yaw as f32 / 100.0, nb.vis_yaw as f32 / 100.0, t);
        Some(Interp { pos, vel, yaw, vis_off, net: if t < 0.5 { na } else { nb } })
    }

    /// The two snapshots around `render_tick` and the fraction between them.
    /// Equal references mean "hold this one".
    fn bracket(&self, render_tick: f64) -> Option<(&Snapshot, &Snapshot, f64)> {
        let first = self.snaps.front()?;
        let last = self.snaps.back()?;
        if render_tick <= first.tick as f64 {
            return Some((first, first, 0.0));
        }
        if render_tick >= last.tick as f64 {
            return Some((last, last, 0.0));
        }
        let j = self.snaps.iter().position(|s| s.tick as f64 > render_tick)?;
        let (a, b) = (&self.snaps[j - 1], &self.snaps[j]);
        let t = (render_tick - a.tick as f64) / (b.tick - a.tick).max(1) as f64;
        Some((a, b, t))
    }

    fn interpolate_entities(&self, w: &mut race_core::World, render_tick: f64) {
        let Some((a, b, t)) = self.bracket(render_tick) else { return };
        let t = t as f32;
        let later = t >= 0.5;
        let mut projs = Vec::with_capacity(b.projectiles.len() + 2);
        for pb in &b.projectiles {
            match a.projectiles.iter().find(|p| p.id == pb.id) {
                Some(pa) => {
                    let mut p = pb.to_projectile();
                    p.pos = Vec3::from_array(pa.pos).lerp(Vec3::from_array(pb.pos), t);
                    projs.push(p);
                }
                None if later => projs.push(pb.to_projectile()),
                None => {}
            }
        }
        if !later {
            for pa in &a.projectiles {
                if !b.projectiles.iter().any(|p| p.id == pa.id) {
                    projs.push(pa.to_projectile());
                }
            }
        }
        w.projectiles = projs;
        let hz = if later { b } else { a };
        w.hazards = hz
            .hazards
            .iter()
            .map(|h| race_core::Hazard {
                kind: race_core::HazardKind::Cactus,
                pos: Vec3::from_array(h.pos),
                owner: h.owner,
                age: 0.0,
                id: h.id,
            })
            .collect();
    }
}

struct Interp {
    pos: Vec3,
    vel: Vec3,
    yaw: f32,
    vis_off: f32,
    net: RacerNet,
}

fn net_vel(n: &RacerNet) -> Vec3 {
    Vec3::new(n.vel[0] as f32, n.vel[1] as f32, n.vel[2] as f32) / 100.0
}

/// As `Racer::apply_net` decodes it.
fn net_yaw(n: &RacerNet) -> f32 {
    (n.yaw as f32 / 65535.0 - 0.5) * std::f32::consts::TAU
}

#[cfg(test)]
mod migration_transaction_tests {
    use super::*;
    use decented_net::migration::{Error,IntentJournal,MAX_JOURNAL_RECORDS};
    fn client()->(ClientSession,Arc<std::sync::Mutex<MigrationCapture>>) {
        let capture=Arc::new(std::sync::Mutex::new(MigrationCapture{incoming:Default::default(),reliable:vec![],lossy:vec![],bytes:0,rtt:None,closed:false,overflow:false}));
        let mut client=ClientSession::new(Box::new(MigrationClientTransport(capture.clone())),"Player",0);
        client.my_seat=Some(0);client.clock.tick(0);client.clock.on_pong(0,0,0,0);client.clock.tick(2000);
        client.on_race_start(race_core::RaceConfig::quick(TrackId::Savanna,2,race_core::BotLevel::Normal,42),None,0,0,1);
        (client,capture)
    }
    fn state(c:&ClientSession)->Vec<u8> {
        bincode::serialize(&(&c.name,c.character,c.attire,format!("{:?}",c.phase),c.my_seat,&c.lobby,&c.results,c.now,
            c.race.as_ref().map(|r|(&r.pred,r.next_seq,&r.ring,&r.pred_log,&r.live_log,r.last_now,&r.events)),
            c.view.as_ref().map(|v|(&v.world,&v.events)),c.custom.as_ref().map(|t|t.hash),c.download.is_some())).unwrap()
    }
    #[test]
    fn full_journal_rolls_back_prediction_and_view_then_success_preserves_live_transport() {
        let (mut c,wire)=client();let before=state(&c);let mut full=IntentJournal::default();
        full.append_batch(&vec![vec![];MAX_JOURNAL_RECORDS]).unwrap();
        assert!(matches!(c.migration_step(&mut full,16,Input::default(),&[],None,|c|{c.set_ready(true);Ok(())}),Err(Error::Bounds)));
        assert_eq!(state(&c),before);assert_eq!(full.tail(),MAX_JOURNAL_RECORDS as u64);assert!(wire.lock().unwrap().reliable.is_empty());assert!(wire.lock().unwrap().lossy.is_empty());
        let mut journal=IntentJournal::default();let batch=c.migration_step(&mut journal,16,Input::default(),&[],None,|c|{c.set_ready(true);Ok(())}).unwrap();
        assert_eq!(batch.reliable.len(),1);assert_eq!(decode::<ToHost>(&batch.reliable[0].1),Some(ToHost::SetReady(true)));assert_eq!(journal.pending().next().unwrap(),(1,batch.reliable[0].1.as_slice()));
        assert_ne!(state(&c),before);assert!(wire.lock().unwrap().reliable.is_empty());assert!(wire.lock().unwrap().lossy.is_empty());
        c.set_ready(false);assert_eq!(wire.lock().unwrap().reliable.len(),1);
    }
    #[test]
    fn client_action_error_panic_or_capture_overflow_cannot_publish_or_change_prediction() {
        let (mut c,wire)=client();let before=state(&c);let mut journal=IntentJournal::default();
        assert!(c.migration_step(&mut journal,16,Input::default(),&[],None,|c|{c.leave();Err(Error::Invalid("action fault"))}).is_err());
        assert_eq!(state(&c),before);assert!(!wire.lock().unwrap().closed);
        assert!(c.migration_step(&mut journal,16,Input::default(),&[],None,|c|{c.leave();panic!("action panic")}).is_err());
        assert_eq!(state(&c),before);assert!(!wire.lock().unwrap().closed);
        assert!(c.migration_step(&mut journal,16,Input::default(),&[],None,|c|{for _ in 0..=MAX_JOURNAL_RECORDS{c.set_ready(true);}Ok(())}).is_err());
        assert_eq!(state(&c),before);assert_eq!(journal.tail(),0);assert!(wire.lock().unwrap().reliable.is_empty());
    }
    #[test]
    fn completed_custom_download_and_track_ready_intent_adopt_together_without_cache_leaks() {
        let old=crate::track_transfer::sample_track("Other room",180.0);custom::install(custom::to_def(&old).unwrap());let generation=custom::generation();
        let track=crate::track_transfer::sample_track("Received track",95.0);let bytes=track.to_bytes();let chunks:Vec<_>=bytes.chunks(crate::track_transfer::CHUNK_BYTES).collect();let total=chunks.len() as u8;
        let (mut c,wire)=client();c.phase=ClientPhase::Lobby;c.race=None;c.view=None;
        c.lobby=Some(LobbyView{seats:vec![],track:TrackId::Custom,custom:Some(track.hash()),laps:track.laps,bots:0,bot_level:race_core::BotLevel::Normal,phase:LobbyPhase::Lobby,battle:None});
        let incoming:Vec<_>=chunks.iter().enumerate().map(|(i,bytes)|ClientNetEvent::Reliable(encode(&ToClient::TrackData{hash:track.hash(),idx:i as u8,total,bytes:bytes.to_vec()}))).collect();
        let before=state(&c);let mut full=IntentJournal::default();full.append_batch(&vec![vec![];MAX_JOURNAL_RECORDS]).unwrap();
        assert!(c.migration_step(&mut full,16,Input::default(),&incoming,None,|_|Ok(())).is_err());
        assert_eq!(state(&c),before);assert_eq!(custom::generation(),generation);assert_eq!(custom::current().unwrap().1.name,old.name);
        let mut journal=IntentJournal::default();let batch=c.migration_step(&mut journal,16,Input::default(),&incoming,None,|_|Ok(())).unwrap();
        assert_eq!(custom::generation(),generation+1);assert_eq!(c.custom_track(),Some(&track));assert_eq!(batch.reliable.len(),1);
        assert_eq!(decode::<ToHost>(&batch.reliable[0].1),Some(ToHost::TrackReady{hash:track.hash()}));assert!(wire.lock().unwrap().reliable.is_empty());
    }
    // The recovery barrier rebase validates before it mutates: a correction
    // of 250 ms is applied, 251 (either sign) leaves the whole client,
    // clock-sync round included, exactly as it was.
    #[test]
    fn migration_rebase_accepts_250_and_refuses_251_without_mutation() {
        let (mut c,wire)=client();let pings=c.clock.tick(4600);assert_eq!(pings.len(),1);assert!(c.clock_sync().round_open());
        let before=(state(&c),format!("{:?}",c.clock_sync()));
        for delta in [251,-251,i64::MIN] {
            assert_eq!(c.migration_rebase_clock(5000,1000,delta),Err("migration clock correction exceeds 250 ms"));
            assert_eq!((state(&c),format!("{:?}",c.clock_sync())),before,"client touched at {delta}");
        }
        assert!(c.migration_rebase_clock(5000,1000,-250).is_ok());
        assert_eq!(c.host_now(5000),Some(1000));assert!(!c.clock_sync().round_open());
        assert_eq!(c.race.as_ref().unwrap().last_now,5000);assert_eq!(c.now,5000);
        assert!(c.migration_rebase_clock(5100,1000,250).is_ok());assert_eq!(c.host_now(5100),Some(1000));
        // No traffic: the client keeps no security clock for a rebase to touch.
        assert!(wire.lock().unwrap().reliable.is_empty()&&wire.lock().unwrap().lossy.is_empty());
        // An unsynced clock stays unsynced.
        let mut fresh=ClientSession::new(Box::new(MigrationClientTransport(wire.clone())),"Fresh",0);
        assert!(fresh.migration_rebase_clock(5000,1000,0).is_ok());assert_eq!(fresh.clock_offset_ms(),None);
    }
    // The rebase aborts an open round: pongs of the old term (ids below the
    // next round's first id), replayed late, complete nothing, so neither
    // the offset nor the RTT moves, before or after a fresh round opens.
    #[test]
    fn migration_rebase_aborts_the_open_round_and_old_term_pongs_complete_nothing() {
        let (mut c,_wire)=client();let rtt=c.clock_sync().rtt_ms();
        let mut old=Vec::new();
        for t in (4600..=4760).step_by(40) {for p in c.clock.tick(t) {if let ToHost::Ping{id,client_t}=p {old.push((id,client_t));}}}
        assert_eq!(old.len(),crate::clock::SYNC_PINGS as usize);assert!(c.clock_sync().round_open());
        let pongs:Vec<_>=old.iter().map(|&(id,client_t)|ClientNetEvent::Reliable(encode(&ToClient::Pong{id,client_t,host_t:client_t+60_000}))).collect();
        c.migration_rebase_clock(5000,1000,0).unwrap();
        assert!(!c.clock_sync().round_open());let offset=c.clock_offset_ms();assert_eq!(offset,Some(1000-5000));
        let mut journal=IntentJournal::default();
        // Late, at the first frame after recovery: the frame opens a fresh round.
        c.migration_step(&mut journal,5010,Input::default(),&pongs,None,|_|Ok(())).unwrap();
        assert!(c.clock_sync().round_first_id().unwrap()>old.last().unwrap().0);
        assert_eq!((c.clock_offset_ms(),c.clock_sync().rtt_ms()),(offset,rtt));
        // Replayed again, then the fresh round times out with no sample.
        c.migration_step(&mut journal,5020,Input::default(),&pongs,None,|_|Ok(())).unwrap();
        c.migration_step(&mut journal,7100,Input::default(),&[],None,|_|Ok(())).unwrap();
        assert!(!c.clock_sync().round_open());assert_eq!((c.clock_offset_ms(),c.clock_sync().rtt_ms()),(offset,rtt));
    }
    // Recovery keeps replaying certified outputs to a seat whose player left:
    // a Results, RaceStart or Closed in flight cannot revive `Closed("left")`.
    #[test]
    fn frozen_replay_after_leave_keeps_the_client_closed() {
        let (mut c,_wire)=client();let mut journal=IntentJournal::default();
        let left=c.migration_step(&mut journal,16,Input::default(),&[],None,|c|{c.leave();Ok(())}).unwrap();
        assert!(left.closed);assert_eq!(decode::<ToHost>(&left.reliable[0].1),Some(ToHost::Leave));assert_eq!(c.phase,ClientPhase::Closed("left".into()));
        let results=ToClient::Results(ResultsView{track:TrackId::Savanna,laps:2,rows:vec![]});
        let start=ToClient::RaceStart{config:race_core::RaceConfig::quick(TrackId::Savanna,2,race_core::BotLevel::Normal,42),custom:None,start_host_ms:0,you:0,race:2};
        let incoming:Vec<_>=[results,start,ToClient::Closed("kicked".into())].iter().map(|m|ClientNetEvent::Reliable(encode(m))).collect();
        let before=state(&c);
        let batch=c.migration_recovery_step(&mut journal,5000,16,&incoming,None).unwrap();
        assert_eq!(c.phase,ClientPhase::Closed("left".into()));assert!(batch.reliable.is_empty()&&batch.lossy.is_empty());
        assert_eq!(journal.tail(),1);
        // Only the real local time moved (it stays monotonic).
        assert_eq!(c.now,5000);c.now=16;assert_eq!(state(&c),before);c.now=5000;
        c.update(6000,Input::default());assert_eq!(c.phase,ClientPhase::Closed("left".into()));
    }
    // The frozen step runs no frame, but a reactive reliable followup to a
    // certified output is journaled: a replayed TrackData that completes a
    // lobby download journals TrackReady in the same transaction.
    #[test]
    fn frozen_replay_journals_reactive_track_ready_and_no_periodic_traffic() {
        let track=crate::track_transfer::sample_track("Frozen track",95.0);let bytes=track.to_bytes();let chunks:Vec<_>=bytes.chunks(crate::track_transfer::CHUNK_BYTES).collect();let total=chunks.len() as u8;
        let (mut c,_wire)=client();c.phase=ClientPhase::Lobby;c.race=None;c.view=None;
        c.lobby=Some(LobbyView{seats:vec![],track:TrackId::Custom,custom:Some(track.hash()),laps:track.laps,bots:0,bot_level:race_core::BotLevel::Normal,phase:LobbyPhase::Lobby,battle:None});
        let incoming:Vec<_>=chunks.iter().enumerate().map(|(i,bytes)|ClientNetEvent::Reliable(encode(&ToClient::TrackData{hash:track.hash(),idx:i as u8,total,bytes:bytes.to_vec()}))).collect();
        let clock=(c.clock_offset_ms(),c.rtt_ms());let mut journal=IntentJournal::default();
        // Local time is well past a clock-sync period: a frame would ping.
        let batch=c.migration_recovery_step(&mut journal,9000,16,&incoming,None).unwrap();
        assert_eq!(batch.reliable.len(),1);assert_eq!(decode::<ToHost>(&batch.reliable[0].1),Some(ToHost::TrackReady{hash:track.hash()}));
        assert_eq!(journal.pending().next().unwrap(),(1,batch.reliable[0].1.as_slice()));assert!(batch.lossy.is_empty());
        assert_eq!(c.custom_track(),Some(&track));assert_eq!((c.clock_offset_ms(),c.rtt_ms()),clock);
    }
}


#[cfg(test)]
mod keep_pass_tests {
    use super::*;
    use crate::transport::{LoopClock, LoopbackConfig, LoopbackHub};

    fn session() -> ClientSession {
        let (clock, _t) = LoopClock::manual();
        let hub = LoopbackHub::new(clock, 5);
        ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Me", 0)
    }

    fn proven(key: &[u8; 32]) -> HostIdentity {
        HostIdentity { name: "Keep".into(), pubkey: crate::invite::hex_encode(key), npub: String::new() }
    }

    fn deliver(c: &mut ClientSession, pass: &KeepPass) {
        c.on_net(ClientNetEvent::Reliable(encode(&ToClient::KeepPass(pass.clone()))), 0, None);
    }

    #[test]
    fn a_pass_for_the_host_the_link_proved_is_surfaced_once_and_taken_once() {
        let mut c = session();
        let pass = KeepPass { host_key: [7; 32], secret: [8; 32] };
        c.on_net(ClientNetEvent::Connected { host: Some(proven(&[7; 32])) }, 0, None);
        deliver(&mut c, &pass);
        deliver(&mut c, &pass);
        assert_eq!(c.take_keep_passes(), vec![pass]);
        assert!(c.take_keep_passes().is_empty());
    }

    #[test]
    fn a_pass_for_another_host_than_the_proven_one_is_dropped() {
        let mut c = session();
        c.on_net(ClientNetEvent::Connected { host: Some(proven(&[7; 32])) }, 0, None);
        deliver(&mut c, &KeepPass { host_key: [9; 32], secret: [8; 32] });
        assert!(c.take_keep_passes().is_empty());
    }

    #[test]
    fn held_passes_are_bounded_and_the_secret_stays_out_of_debug_output() {
        let mut c = session();
        for i in 0..10u8 {
            deliver(&mut c, &KeepPass { host_key: [1; 32], secret: [i; 32] });
        }
        assert_eq!(c.take_keep_passes().len(), MAX_KEEP_PASSES);
        let shown = format!("{:?}", ToClient::KeepPass(KeepPass { host_key: [1; 32], secret: [0xab; 32] }));
        assert!(!shown.contains("abab") && shown.contains("hidden"), "{shown}");
    }
}
