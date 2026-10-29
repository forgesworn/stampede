//! Loopback-hub tests: lobby, full races under latency and loss, prediction
//! accuracy, disconnects, rematch. Virtual clock throughout; no sockets.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use race_core::{bot_input, BotLevel, Input, RacePhase, RacerKind, TrackId, Vec3};

use crate::client::{ClientPhase, ClientSession};
use crate::host::{HostAuthority, HostConfig, HostPhase};
use crate::local::LocalRace;
use crate::proto::LobbyPhase;
use crate::transport::{LinkPath, LoopClock, LoopbackConfig, LoopbackHub};
use crate::view::RaceDriver;

mod jitter;

/// Who drives a test client's controls.
#[derive(Clone, Copy)]
enum Pilot {
    Idle,
    Const(Input),
    /// The sim's bot, run on the client's own view of the world.
    Bot(BotLevel),
    /// Full throttle with a slow steering sweep (exercises replay).
    Weave,
}

struct TClient {
    s: ClientSession,
    /// Added to the global time: every client has its own clock.
    skew: u64,
    pilot: Pilot,
}

struct Net {
    t: Arc<AtomicU64>,
    hub: LoopbackHub,
    host: HostAuthority,
    clients: Vec<TClient>,
    /// (world tick, pos) of each racer after every host tick, for comparisons.
    history: HashMap<(usize, u64), Vec3>,
    last_tick: u64,
}

fn link(latency_ms: u32, loss: f32) -> LoopbackConfig {
    LoopbackConfig { latency_ms, jitter_ms: 0, lossy_loss: loss }
}

impl Net {
    fn new(cfg: HostConfig) -> Net {
        let (clock, t) = LoopClock::manual();
        t.store(10_000, Ordering::Relaxed);
        let hub = LoopbackHub::new(clock, 42);
        let host = HostAuthority::new(Box::new(hub.host()), cfg);
        Net { t, hub, host, clients: Vec::new(), history: HashMap::new(), last_tick: 0 }
    }

    fn cfg(bots: u8, laps: u8) -> HostConfig {
        HostConfig { track: TrackId::Savanna, laps, bots, bot_level: BotLevel::Normal, seed: 99, auto_start: true }
    }

    fn join(&mut self, name: &str, character: u8, link: LoopbackConfig, pilot: Pilot) -> usize {
        let c = self.hub.connect_via(link, LinkPath::Direct);
        let skew = 1_000_000 + 77_777 * self.clients.len() as u64;
        self.clients.push(TClient { s: ClientSession::new(Box::new(c), name, character), skew, pilot });
        self.clients.len() - 1
    }

    fn now(&self) -> u64 {
        self.t.load(Ordering::Relaxed)
    }

    fn step(&mut self, ms: u64) {
        let now = self.t.fetch_add(ms, Ordering::Relaxed) + ms;
        self.host.update(now);
        if let Some(w) = self.host.world() {
            if w.tick != self.last_tick {
                for r in &w.racers {
                    self.history.insert((r.idx as usize, w.tick), r.pos);
                }
                self.last_tick = w.tick;
            }
        }
        for c in &mut self.clients {
            let input = match c.pilot {
                Pilot::Idle => Input::default(),
                Pilot::Const(i) => i,
                Pilot::Bot(level) => c.s.view().map_or(Input::default(), |v| {
                    if v.world.phase == RacePhase::Countdown && v.hud.countdown > 0.25 {
                        Input::default()
                    } else {
                        bot_input(&v.world, v.you, level)
                    }
                }),
                Pilot::Weave => {
                    let t = (now as f32) / 1000.0;
                    Input { throttle: 1.0, steer: (t * 0.9).sin() * 0.35, ..Input::default() }
                }
            };
            c.s.update(now + c.skew, input);
        }
    }

    /// Step `ms` at a time until `cond`, at most `max_ms` of virtual time.
    fn run_until(&mut self, step_ms: u64, max_ms: u64, mut cond: impl FnMut(&Net) -> bool) -> bool {
        let end = self.now() + max_ms;
        while self.now() < end {
            self.step(step_ms);
            if cond(self) {
                return true;
            }
        }
        false
    }

    fn run_for(&mut self, step_ms: u64, ms: u64) {
        self.run_until(step_ms, ms, |_| false);
    }
}

// ─── (a) lobby ──────────────────────────────────────────────────────────────

#[test]
fn custom_selection_transfers_on_join_and_changes_and_keeps_session_geometry() {
    use crate::track_transfer::sample_track;
    let mut net = Net::new(HostConfig { auto_start: false, ..Net::cfg(1, 3) });
    let t = sample_track("First loop", 180.0);
    net.host.set_custom_track(&t).unwrap();
    assert_eq!(net.host.lobby_view().custom, Some(t.hash()));
    assert_eq!(net.host.config().laps, 7);
    let a = net.join("Ann", 0, link(70, 0.0), Pilot::Bot(BotLevel::Hard));
    assert!(!net.host.can_start());
    assert!(net.run_until(5, 2500, |n| n.host.can_start()));
    assert_eq!(net.clients[a].s.custom_track(), Some(&t));
    let b = net.join("Bob", 1, link(100, 0.0), Pilot::Bot(BotLevel::Hard));
    assert!(net.run_until(5, 1000, |n| n.host.human_count() == 2));
    assert!(!net.host.can_start(), "joining guest has not acknowledged this hash");
    assert!(!net.host.start_race(net.now()), "Start cannot bypass the transfer");
    assert!(net.run_until(5, 2500, |n| n.host.can_start()));
    assert_eq!(net.clients[b].s.custom_track(), Some(&t));
    for c in &mut net.clients { c.s.set_ready(true); }
    net.run_for(5, 500);
    let t2 = sample_track("Another loop", 95.0);
    net.host.set_custom_track(&t2).unwrap();
    assert!(!net.host.can_start());
    assert!(net.host.lobby_view().seats.iter().filter(|s| !s.bot).all(|s| !s.ready));
    assert!(net.run_until(5, 2500, |n| n.host.can_start()));
    for c in &net.clients { assert_eq!(c.s.custom_track(), Some(&t2)); }
    let old = net.host.lobby_view();
    let mut bad = t2.clone();
    bad.points.swap(1, 2);
    assert!(net.host.set_custom_track(&bad).is_err());
    assert_eq!(net.host.lobby_view(), old);
    net.host.set_laps(9);
    race_core::custom::install(race_core::custom::to_def(&t).unwrap());
    assert!(net.host.start_race(net.now()));
    net.run_for(5, 1500);
    assert_eq!(net.host.world().unwrap().track.name, t2.name);
    assert_eq!(net.host.world().unwrap().cfg.laps, 9);
    for c in &net.clients {
        assert_eq!(c.s.view().unwrap().world.track.name, t2.name);
        assert_eq!(c.s.view().unwrap().world.cfg.laps, 9);
    }
    net.host.to_lobby();
    assert!(net.host.set_track(TrackId::Dune));
    assert_eq!(net.host.lobby_view().custom, None);
    assert_eq!(net.host.config().laps, 5);
    net.run_for(5, 500);
    assert!(net.clients.iter().all(|c| c.s.custom_track().is_none()));
    assert!(!net.host.set_track(TrackId::Custom), "bare Custom never uses some other room's registry");
}

#[test]
fn custom_readiness_requires_every_current_hash_including_the_host_loopback() {
    use crate::proto::{encode, ToHost, PROTO_VERSION};
    use crate::track_transfer::sample_track;
    use crate::transport::ClientTransport;
    let t = sample_track("Ready loop", 95.0);
    let (mut host, mut me) = crate::host_with_local_player(None, HostConfig { auto_start: false, ..HostConfig::default() }, "Host", 0);
    host.set_custom_track(&t).unwrap();
    me.update(10_000, Input::default());
    host.update(10_000);
    assert!(!host.can_start(), "own host seat must also validate and acknowledge");
    me.update(10_000, Input::default());
    host.update(10_000);
    assert!(host.can_start());
    assert_eq!(me.custom_track(), Some(&t));

    let mut net = Net::new(HostConfig { auto_start: false, ..Net::cfg(0, 1) });
    net.host.set_custom_track(&t).unwrap();
    net.join("Ann", 0, LoopbackConfig::PERFECT, Pilot::Idle);
    let mut raw = net.hub.connect(LoopbackConfig::PERFECT);
    raw.send_reliable(encode(&ToHost::Hello { proto: PROTO_VERSION, name: "Raw".into(), character: 1 }));
    net.run_for(5, 500);
    assert_eq!(net.host.human_count(), 2);
    assert!(!net.host.can_start());
    raw.send_reliable(encode(&ToHost::TrackReady { hash: t.hash() ^ 1 }));
    raw.send_reliable(encode(&ToHost::SetReady(true)));
    net.run_for(5, 100);
    assert!(!net.host.can_start());
    assert!(!net.host.lobby_view().seats.iter().find(|s| s.name == "Raw").unwrap().ready);
    raw.send_reliable(encode(&ToHost::TrackReady { hash: t.hash() }));
    net.run_for(5, 100);
    assert!(net.host.can_start());
    let other = sample_track("New hash", 96.0);
    net.host.set_custom_track(&other).unwrap();
    raw.send_reliable(encode(&ToHost::TrackReady { hash: t.hash() }));
    net.run_for(5, 500);
    assert!(!net.host.can_start(), "old ready cannot acknowledge a new selection");
}

#[test]
fn custom_rematch_keeps_the_checked_track_and_back_to_lobby_can_select_again() {
    let t = crate::track_transfer::sample_track("Rematch loop", 60.0);
    let mut net = Net::new(Net::cfg(1, 1));
    net.host.set_custom_track(&t).unwrap();
    net.host.set_laps(1);
    net.join("Ann", 0, link(30, 0.05), Pilot::Bot(BotLevel::Hard));
    net.join("Bob", 1, link(50, 0.05), Pilot::Bot(BotLevel::Hard));
    assert!(net.run_until(5, 2500, |n| n.host.can_start()));
    for c in &mut net.clients { c.s.set_ready(true); }
    assert!(net.run_until(5, 6 * 60_000, |n| n.host.phase() == HostPhase::Results));
    net.run_for(5, 500);
    let seed = net.host.world().unwrap().cfg.seed;
    for c in &mut net.clients { c.s.set_rematch_ready(true); }
    assert!(net.run_until(5, 5000, |n| n.host.phase() == HostPhase::Racing));
    net.run_for(5, 1000);
    assert_ne!(net.host.world().unwrap().cfg.seed, seed);
    assert_eq!(net.host.world().unwrap().track.name, t.name);
    for c in &net.clients {
        assert_eq!(c.s.custom_track(), Some(&t));
        assert_eq!(c.s.view().unwrap().world.track.name, t.name);
    }
    net.host.to_lobby();
    net.run_for(5, 500);
    net.host.set_custom_track(&crate::track_transfer::sample_track("Next loop", 95.0)).unwrap();
    assert!(!net.host.can_start());
    assert!(net.run_until(5, 2500, |n| n.host.can_start()));
}

#[test]
fn custom_guest_ignores_old_chunks_and_refuses_missing_or_wrong_start_hashes() {
    use crate::proto::{decode, encode, ToClient, ToHost, PROTO_VERSION};
    use crate::track_transfer::{sample_track, CHUNK_BYTES};
    use crate::transport::{HostNetEvent, HostTransport, NullHostTransport};
    let (clock, _) = LoopClock::manual();
    let hub = LoopbackHub::new(clock, 13);
    let mut wire = hub.host();
    let mut guest = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Guest", 0);
    guest.update(0, Input::default());
    let mut peer = None;
    while let Some(ev) = wire.poll() { if let HostNetEvent::Joined { peer: p, .. } = ev { peer = Some(p); } }
    let peer = peer.unwrap();
    wire.send_reliable(peer, encode(&ToClient::Welcome { proto: PROTO_VERSION, seat: 0 }));
    let host = HostAuthority::new(Box::new(NullHostTransport), HostConfig::default());
    let mut lobby = host.lobby_view();
    lobby.track = TrackId::Custom;
    let first = sample_track("Old loop", 180.0);
    lobby.custom = Some(first.hash());
    wire.send_reliable(peer, encode(&ToClient::Lobby(lobby.clone())));
    let old = first.to_bytes();
    let old_total = old.len().div_ceil(CHUNK_BYTES) as u8;
    wire.send_reliable(peer, encode(&ToClient::TrackData { hash: first.hash(), idx: 0, total: old_total, bytes: old[..CHUNK_BYTES].to_vec() }));
    guest.update(0, Input::default());
    assert!(guest.downloading_track());
    assert!(guest.custom_track().is_none());
    let current = sample_track("Current loop", 95.0);
    lobby.custom = Some(current.hash());
    wire.send_reliable(peer, encode(&ToClient::Lobby(lobby.clone())));
    for (idx, bytes) in old.chunks(CHUNK_BYTES).enumerate() {
        wire.send_reliable(peer, encode(&ToClient::TrackData { hash: first.hash(), idx: idx as u8, total: old_total, bytes: bytes.to_vec() }));
    }
    let config = race_core::RaceConfig::quick(TrackId::Custom, 0, BotLevel::Normal, 1);
    wire.send_reliable(peer, encode(&ToClient::RaceStart { config: config.clone(), custom: Some(current.hash()), start_host_ms: 0, you: 0, race: 1 }));
    guest.update(0, Input::default());
    assert_eq!(*guest.phase(), ClientPhase::Lobby, "race cannot start before validation");
    assert!(guest.custom_track().is_none(), "old chunks cannot install a stale track");
    wire.send_reliable(peer, encode(&ToClient::TrackData { hash: current.hash(), idx: 255, total: 1, bytes: vec![1] }));
    guest.update(0, Input::default());
    assert!(guest.track_error().is_some());
    let bytes = current.to_bytes();
    let total = bytes.len().div_ceil(CHUNK_BYTES) as u8;
    for (idx, part) in bytes.chunks(CHUNK_BYTES).enumerate().rev() {
        wire.send_reliable(peer, encode(&ToClient::TrackData { hash: current.hash(), idx: idx as u8, total, bytes: part.to_vec() }));
    }
    guest.update(0, Input::default());
    assert_eq!(guest.custom_track(), Some(&current));
    assert!(guest.track_error().is_none());
    let mut ack = false;
    while let Some(ev) = wire.poll() {
        if let HostNetEvent::Reliable { bytes, .. } = ev {
            if decode::<ToHost>(&bytes) == Some(ToHost::TrackReady { hash: current.hash() }) { ack = true; }
        }
    }
    assert!(ack, "only a checked track is acknowledged");
    for custom in [None, Some(first.hash())] {
        wire.send_reliable(peer, encode(&ToClient::RaceStart { config: config.clone(), custom, start_host_ms: 0, you: 0, race: 1 }));
        guest.update(0, Input::default());
        assert_eq!(*guest.phase(), ClientPhase::Lobby);
    }
    wire.send_reliable(peer, encode(&ToClient::RaceStart { config, custom: Some(current.hash()), start_host_ms: 0, you: 0, race: 1 }));
    guest.update(0, Input::default());
    assert_eq!(*guest.phase(), ClientPhase::Racing);
    assert_eq!(guest.view().unwrap().world.track.name, current.name);
}

#[test]
fn lobby_join_choose_ready_start_and_a_third_joiner_takes_a_bot_seat() {
    let mut net = Net::new(Net::cfg(6, 3));
    let a = net.join("Ann", 2, link(20, 0.0), Pilot::Idle);
    let b = net.join("Bob", 0, link(20, 0.0), Pilot::Idle);
    assert!(net.run_until(5, 2000, |n| n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Lobby)));
    assert_eq!(net.host.phase(), HostPhase::Lobby);
    // 2 humans + 6 bots fill the room.
    let v = net.clients[a].s.lobby().expect("lobby").clone();
    assert_eq!(v.seats.len(), 8);
    assert_eq!(v.humans(), 2);
    assert_eq!(v.seats.iter().filter(|s| s.bot).count(), 6);
    assert!(v.seats.iter().filter(|s| s.bot).all(|s| s.ready));
    assert_eq!((v.track, v.laps, v.bots, v.bot_level), (TrackId::Savanna, 3, 6, BotLevel::Normal));
    // Bots do not share a character with the humans.
    for s in v.seats.iter().filter(|s| s.bot) {
        assert!(s.character != 2 && s.character != 0, "{s:?}");
    }

    // Character choice reaches everyone.
    net.clients[a].s.set_character(5);
    net.run_for(5, 300);
    assert_eq!(net.clients[b].s.lobby().unwrap().seat(net.clients[a].s.my_seat().unwrap()).unwrap().character, 5);
    assert_eq!(net.clients[a].s.my_lobby_seat().unwrap().character, 5);

    // A third person joins the full lobby and takes a bot's seat.
    let c = net.join("Cat", 7, link(20, 0.0), Pilot::Idle);
    assert!(net.run_until(5, 2000, |n| n.clients[c].s.lobby().is_some_and(|l| l.humans() == 3)));
    net.run_for(5, 100);
    let v = net.clients[a].s.lobby().unwrap().clone();
    assert_eq!(v.humans(), 3);
    assert_eq!(v.seats.iter().filter(|s| s.bot).count(), 5);
    assert_eq!(v.seats.len(), 8);
    assert_eq!(net.host.config().bots, 6, "the wish stays; the seats give way");

    // Ready: not enough until everyone is.
    net.clients[a].s.set_ready(true);
    net.clients[b].s.set_ready(true);
    net.run_for(5, 300);
    assert_eq!(net.host.phase(), HostPhase::Lobby);
    assert!(net.clients[a].s.my_lobby_seat().unwrap().ready);
    net.clients[c].s.set_ready(true);
    assert!(net.run_until(5, 2000, |n| n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Racing)));
    assert_eq!(net.host.phase(), HostPhase::Racing);
    let w = net.host.world().unwrap();
    assert_eq!(w.racers.len(), 8);
    assert_eq!(w.cfg.racers.iter().filter(|r| r.kind == RacerKind::Human).count(), 3);
    assert_eq!(w.cfg.racers[0].name, "Ann");
    assert_eq!(w.cfg.racers[0].character, 5);
    // Each client was told its own racer.
    for (i, c) in net.clients.iter().enumerate() {
        assert_eq!(c.s.view().unwrap().you, i);
    }
    // Late joiners are turned away while racing.
    let late = net.join("Late", 1, link(20, 0.0), Pilot::Idle);
    net.run_for(5, 500);
    // The host closed the room when the race began.
    assert!(matches!(net.clients[late].s.phase(), ClientPhase::Failed(_)), "{:?}", net.clients[late].s.phase());
}

#[test]
fn a_ninth_person_is_refused_and_the_version_is_checked() {
    let mut net = Net::new(Net::cfg(0, 1));
    // Nine connect at once, before the host closes the room at eight.
    for i in 0..9 {
        net.join(&format!("P{i}"), (i % 8) as u8, link(5, 0.0), Pilot::Idle);
    }
    net.run_for(5, 600);
    assert_eq!(net.host.human_count(), 8);
    assert!(matches!(net.clients[8].s.phase(), ClientPhase::Closed(r) if r.contains("full")), "{:?}", net.clients[8].s.phase());
    // The room is shut to a later one at the door.
    let extra = net.join("Ten", 0, link(5, 0.0), Pilot::Idle);
    net.run_for(5, 300);
    assert!(matches!(net.clients[extra].s.phase(), ClientPhase::Failed(_)), "{:?}", net.clients[extra].s.phase());
    assert_eq!(net.host.human_count(), 8);

    // A client from the future.
    let mut net = Net::new(Net::cfg(1, 1));
    let c = net.hub.connect_via(link(5, 0.0), LinkPath::Direct);
    let mut raw: Box<dyn crate::ClientTransport> = Box::new(c);
    net.run_for(5, 50);
    while raw.poll().is_some() {}
    raw.send_reliable(crate::proto::encode(&crate::proto::ToHost::Hello { proto: 999, name: "x".into(), character: 0 }));
    net.run_for(5, 100);
    let mut closed = false;
    while let Some(ev) = raw.poll() {
        if let crate::ClientNetEvent::Reliable(b) = ev {
            if let Some(crate::proto::ToClient::Closed(r)) = crate::proto::decode(&b) {
                closed = r.contains("version");
            }
        }
    }
    assert!(closed);
    assert_eq!(net.host.human_count(), 0);
}

// ─── (b) a full race under latency and loss ─────────────────────────────────

#[test]
fn full_three_lap_race_150ms_5pct_loss_completes_and_standings_match() {
    let mut net = Net::new(Net::cfg(5, 3));
    let lossy = link(150, 0.05);
    let a = net.join("Ann", 1, lossy, Pilot::Bot(BotLevel::Normal));
    let b = net.join("Bob", 3, lossy, Pilot::Bot(BotLevel::Normal));
    assert!(net.run_until(5, 5000, |n| n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Lobby)));
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    assert_eq!(net.host.world().unwrap().racers.len(), 7);

    let done = net.run_until(5, 8 * 60_000, |n| n.host.phase() == HostPhase::Results);
    assert!(done, "race did not finish; tick {}", net.host.world().unwrap().tick);
    // Let the results and the last snapshots arrive.
    net.run_for(5, 3000);

    let host_res = net.host.results().expect("host results").clone();
    assert_eq!(host_res.rows.len(), 7);
    for (i, c) in net.clients.iter().enumerate() {
        assert_eq!(*c.s.phase(), ClientPhase::Results, "client {i}");
        let res = c.s.results().expect("client results");
        assert_eq!(res, &host_res, "client {i} results");
        let view = c.s.view().expect("view");
        assert!(view.over);
        assert_eq!(view.standings, host_res.standings(), "client {i} view standings");
        // Their own finish is in the results with a time.
        let row = host_res.rows.iter().find(|r| r.racer as usize == view.you).unwrap();
        assert!(row.time.is_some(), "client {i} finished: {row:?}");
    }
    // The race ran as hosted: three laps, everyone's lap count sane.
    let w = net.host.world().unwrap();
    assert_eq!(w.phase, RacePhase::Finished);
    assert!(w.racers[a].finished_at.is_some() && w.racers[b].finished_at.is_some());

    let hs = net.host.stats();
    let (sent, dropped) = net.hub.lossy_counts();
    eprintln!(
        "race: {} ticks, {} snapshots (max {} B, {} entities dropped), starved ticks {}; lossy {sent} sent / {dropped} dropped",
        hs.ticks, hs.snapshots_sent, hs.snapshot_bytes_max, hs.entities_dropped, hs.starved_ticks
    );
    for (i, c) in net.clients.iter().enumerate() {
        let st = c.s.stats();
        eprintln!(
            "client {i}: {} snapshots, {} input packets, correction last {:.3} m max {:.3} m, snaps > 4 m: {}",
            st.snapshots, st.inputs_sent, st.last_correction_m, st.max_correction_m, st.hard_snaps
        );
    }
    assert!(hs.snapshot_bytes_max <= crate::proto::MAX_LOSSY_LEN);
    assert!(dropped > 0, "loss was injected");
}

// ─── (b1) a battle needs two ───────────────────────────────────────────────

#[test]
fn a_battle_with_one_participant_does_not_start() {
    use crate::proto::BattleSetup;
    let mut net = Net::new(Net::cfg(0, 3));
    let mut setup = BattleSetup::new(TrackId::DustBowl);
    setup.rules.invuln_ds = 50;
    assert!(net.host.set_battle(Some(setup)));
    let shown = net.host.lobby_view().battle.expect("battle").rules;
    assert_eq!(shown.invuln_ds, 25, "invulnerability fits the snapshot's u8 centiseconds");
    net.join("Ann", 1, link(20, 0.0), Pilot::Idle);
    assert!(net.run_until(5, 5000, |n| *n.clients[0].s.phase() == ClientPhase::Lobby));
    assert!(!net.host.can_start(), "one human, no bots");
    net.clients[0].s.set_ready(true);
    net.run_for(5, 3000);
    assert_eq!(net.host.phase(), HostPhase::Lobby, "auto start refuses a lone battle");
    let now = net.now();
    assert!(!net.host.start_race(now), "Start now refuses too");
    // A bot makes it a fight; a plain race never needed one.
    assert!(net.host.set_bots(1));
    assert!(net.host.can_start());
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    assert_eq!(net.host.world().unwrap().racers.len(), 2);

    let mut net = Net::new(Net::cfg(0, 3));
    net.join("Ann", 1, link(20, 0.0), Pilot::Idle);
    assert!(net.run_until(5, 5000, |n| *n.clients[0].s.phase() == ClientPhase::Lobby));
    assert!(net.host.can_start(), "a race alone is fine");
}

// ─── (b2) a battle under latency and loss ────────────────────────────────────

#[test]
fn battle_150ms_5pct_loss_completes_and_every_client_agrees_on_the_ranking() {
    use crate::proto::BattleSetup;
    let mut net = Net::new(Net::cfg(4, 3));
    assert!(!net.host.set_track(TrackId::DustBowl), "arenas are not race tracks");
    assert!(!net.host.set_battle(Some(BattleSetup::new(TrackId::Savanna))), "battles need an arena");
    assert!(net.host.set_battle(Some(BattleSetup::new(TrackId::DustBowl))));
    let lossy = link(150, 0.05);
    net.join("Ann", 1, lossy, Pilot::Bot(BotLevel::Normal));
    net.join("Bob", 3, lossy, Pilot::Bot(BotLevel::Normal));
    assert!(net.run_until(5, 5000, |n| n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Lobby)));
    assert!(net.run_until(5, 2000, |n| n
        .clients
        .iter()
        .all(|c| c.s.lobby().is_some_and(|l| l.battle == Some(BattleSetup::new(TrackId::DustBowl))))));
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    {
        let w = net.host.world().unwrap();
        assert_eq!(w.racers.len(), 6);
        assert_eq!(w.cfg.track, TrackId::DustBowl);
        assert!(matches!(w.cfg.mode, race_core::RaceMode::Battle(_)));
        assert!(w.battle.is_some());
    }
    assert!(!net.host.set_battle(None), "lobby only");

    let done = net.run_until(5, 5 * 60_000, |n| n.host.phase() == HostPhase::Results);
    assert!(done, "battle did not finish; tick {}", net.host.world().unwrap().tick);
    net.run_for(5, 3000);

    let host_res = net.host.results().expect("host results").clone();
    let w = net.host.world().unwrap();
    let hb = w.battle.clone().expect("battle");
    assert!(hb.done);
    assert!(hb.alive().len() == 1 || hb.time_left <= 0.0);
    assert_eq!(host_res.rows.len(), 6);
    assert_eq!(host_res.track, TrackId::DustBowl);
    assert_eq!(host_res.standings(), hb.ranking(), "results follow the battle ranking");
    for (i, c) in net.clients.iter().enumerate() {
        assert_eq!(*c.s.phase(), ClientPhase::Results, "client {i}");
        assert_eq!(c.s.results(), Some(&host_res), "client {i} results");
        let view = c.s.view().expect("view");
        assert!(view.over);
        assert_eq!(view.standings, host_res.standings(), "client {i} view standings");
        let cb = view.world.battle.as_ref().expect("client battle state");
        assert_eq!(cb.feathers, hb.feathers, "client {i} feathers");
        assert_eq!(cb.out_order, hb.out_order, "client {i} out order");
        assert_eq!(cb.hits, hb.hits, "client {i} hits");
        assert!(cb.done);
        assert_eq!(cb.ranking(), hb.ranking(), "client {i} ranking");
    }
    let hs = net.host.stats();
    let (sent, dropped) = net.hub.lossy_counts();
    eprintln!(
        "battle: {:.1} s, out {:?}, feathers {:?}, hits {:?}; {} snapshots (max {} B, {} entities dropped); lossy {sent} sent / {dropped} dropped",
        w.race_time(),
        hb.out_order,
        hb.feathers,
        hb.hits,
        hs.snapshots_sent,
        hs.snapshot_bytes_max,
        hs.entities_dropped
    );
    assert!(hs.snapshot_bytes_max <= crate::proto::MAX_LOSSY_LEN);
    assert!(dropped > 0, "loss was injected");
    assert!(hb.hits.iter().map(|&h| h as u32).sum::<u32>() >= 5, "they fought");
}

/// The frame a guest's `Out` / `BattleOver` event arrives in, its battle state
/// already shows it: the right winner and the right "N left" (spec 17.1).
#[test]
fn guest_battle_state_is_never_behind_its_out_and_over_events() {
    use crate::proto::BattleSetup;
    use race_core::{BattleRules, Event};
    let mut checked = 0;
    for seed in 0..3u64 {
        let mut cfg = Net::cfg(6, 3);
        cfg.seed = 99 + seed;
        let mut net = Net::new(cfg);
        let mut setup = BattleSetup::new(TrackId::DustBowl);
        setup.rules = BattleRules { feathers: 1, ..BattleRules::default() };
        assert!(net.host.set_battle(Some(setup)));
        let lag = link(150, 0.05);
        net.join("Ann", 1, lag, Pilot::Bot(BotLevel::Hard));
        net.join("Bob", 3, lag, Pilot::Bot(BotLevel::Hard));
        assert!(net.run_until(5, 5000, |n| n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Lobby)));
        for c in &mut net.clients {
            c.s.set_ready(true);
        }
        let mut seen_over = [false; 2];
        let mut bad = Vec::new();
        let mut host_out: Vec<usize> = Vec::new();
        let finished = net.run_until(5, 5 * 60_000, |n| {
            for (ci, c) in n.clients.iter().enumerate() {
                let Some(v) = c.s.view() else { continue };
                let Some(b) = v.world.battle.as_ref() else { continue };
                for e in &v.events {
                    match e {
                        Event::Out { racer, place, .. } => {
                            let left = b.alive().len();
                            // Several eliminations can arrive in one frame. State may be
                            // ahead of an earlier event, but its recorded place must agree.
                            let out_place = b.out_order.iter().position(|&i| i == *racer as usize)
                                .map(|i| b.feathers.len() - i);
                            if !b.is_out(*racer as usize) || left + 1 > *place as usize
                                || out_place != Some(*place as usize) || b.feathers[*racer as usize] != 0 {
                                bad.push(format!("client {ci}: Out {racer} place {place}: out {:?} feathers {:?}", b.out_order, b.feathers));
                            }
                            host_out.push(*racer as usize);
                        }
                        Event::BattleOver => {
                            seen_over[ci] = true;
                            // The banner reads the ranking in this frame.
                            let winner = b.ranking()[0];
                            if !b.done || b.is_out(winner) {
                                bad.push(format!("client {ci}: BattleOver, winner {winner}, out {:?}, done {}", b.out_order, b.done));
                            }
                        }
                        _ => {}
                    }
                }
            }
            n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Results)
        });
        assert!(finished, "seed {seed}: battle did not finish");
        assert!(bad.is_empty(), "seed {seed}: {bad:?}");
        let hb = net.host.world().unwrap().battle.clone().unwrap();
        if hb.alive().len() == 1 {
            assert!(seen_over.iter().all(|&s| s), "seed {seed}: both guests saw BattleOver");
            let winner = hb.ranking()[0];
            for c in &net.clients {
                assert_eq!(c.s.view().unwrap().world.battle.as_ref().unwrap().ranking()[0], winner);
            }
            checked += 1;
        }
    }
    assert!(checked >= 1, "at least one battle ended on a knock-out");
}

#[test]
fn local_race_records_the_same_ghost_as_every_tick_whatever_the_frame_rate() {
    let mut cfg = race_core::RaceConfig::all_bots(TrackId::Savanna, 1, BotLevel::Hard, 4);
    cfg.mode = race_core::RaceMode::TimeTrial;
    cfg.laps = 1;
    // Reference: a recorder fed after every World::step.
    let mut w = race_core::World::new(cfg.clone());
    let mut rec = race_core::GhostRecorder::new(&w, 0);
    while w.racers[0].finished_at.is_none() {
        w.step(&[]);
        rec.record(&w);
    }
    let want = rec.finish_world(&w).unwrap();
    // LocalRace at ~30 fps with a stall (several ticks per update).
    let mut race = LocalRace::new(cfg, 0);
    race.record_ghost(true);
    let mut now = 5_000u64;
    let mut k = 0;
    while race.ghost().is_none() && now < 5_000 + 200_000 {
        now += if k % 50 == 49 { 140 } else { 33 };
        k += 1;
        race.update(now, race_core::Input::default());
    }
    let got = race.take_ghost().expect("ghost after the finish");
    assert_eq!(got.frames, want.frames);
    assert_eq!(got.time, want.time);
    assert_eq!(got.lap_times, want.lap_times);
    assert!(race.ghost().is_none());
}

// ─── (c) prediction accuracy ────────────────────────────────────────────────

fn prediction_error(link_cfg: LoopbackConfig, pilot: Pilot, seconds_after_go: u64) -> (f32, f32, usize, Net) {
    prediction_error_with_bots(0, link_cfg, pilot, seconds_after_go)
}

fn prediction_error_with_bots(bots: u8, link_cfg: LoopbackConfig, pilot: Pilot, seconds_after_go: u64) -> (f32, f32, usize, Net) {
    let mut net = Net::new(Net::cfg(bots, 3));
    let a = net.join("Ann", 0, link_cfg, pilot);
    assert!(net.run_until(5, 3000, |n| *n.clients[a].s.phase() == ClientPhase::Lobby));
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let racer = net.host.racer_of(net.clients[a].s.my_seat().unwrap()).unwrap();
    let go_tick = net.host.world().unwrap().go_tick;
    let end = net.now() + crate::host::START_LEAD_MS + 3000 + seconds_after_go * 1000;
    // Every input number is compared once the host has applied it: the
    // prediction first made for `seq` against the host's racer after tick
    // `seq - 1` (that is, world.tick == seq).
    let (mut worst, mut sum, mut n, mut next) = (0.0f32, 0.0f32, 0usize, go_tick + 1);
    while net.now() < end {
        net.step(4);
        let Some(ack) = net.clients[a].s.last_ack() else { continue };
        while next <= ack as u64 {
            if let (Some(p), Some(h)) = (net.clients[a].s.live_predicted_pos(next as u32), net.history.get(&(racer, next))) {
                let e = p.distance(*h);
                worst = worst.max(e);
                sum += e;
                n += 1;
            }
            next += 1;
        }
    }
    assert!(n > 100, "compared only {n} ticks");
    (worst, sum / n as f32, n, net)
}

#[test]
fn prediction_stays_within_half_a_metre_at_100ms_latency_constant_input() {
    let (worst, mean, n, net) = prediction_error(link(100, 0.0), Pilot::Const(Input::go()), 6);
    let v = net.clients[0].s.view().unwrap();
    eprintln!(
        "prediction, 100 ms each way, constant input: {n} ticks compared, worst {worst:.4} m, mean {mean:.4} m; speed {:.1} m/s, snapshots {}, max correction {:.4} m, starved {}",
        v.own().speed,
        net.clients[0].s.stats().snapshots,
        net.clients[0].s.stats().max_correction_m,
        net.host.stats().starved_ticks
    );
    assert!(v.own().speed > 15.0, "the racer actually ran");
    assert!(worst < 0.5, "worst {worst}");
}

#[test]
fn prediction_with_steering_loss_and_a_correction_offset_that_eases() {
    let (worst, mean, n, net) = prediction_error(
        LoopbackConfig { latency_ms: 100, jitter_ms: 10, lossy_loss: 0.05 },
        Pilot::Weave,
        10,
    );
    eprintln!(
        "prediction, 100 ms +-10, 5% loss, weaving: {n} ticks, worst {worst:.4} m, mean {mean:.4} m, max correction {:.4} m, starved {}",
        net.clients[0].s.stats().max_correction_m,
        net.host.stats().starved_ticks
    );
    assert!(worst < 0.5, "worst {worst}");
    assert!(net.clients[0].s.view().unwrap().correction.length() < 0.5);
}

#[test]
fn a_pestered_racer_is_predicted_through_the_sale_and_the_seller_drawn_beside_it() {
    use race_core::playful::{RoseSeller, ROSE_SALE, ROSE_SIDE};
    let mut net = Net::new(Net::cfg(0, 3));
    let a = net.join("Ann", 0, link(100, 0.0), Pilot::Weave);
    assert!(net.run_until(5, 3000, |n| *n.clients[a].s.phase() == ClientPhase::Lobby));
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let racer = net.host.racer_of(net.clients[a].s.my_seat().unwrap()).unwrap();
    let go_ms = net.host.start_host_ms().unwrap() + 3000;
    while net.now() < go_ms + 1500 {
        net.step(4);
    }
    // Someone else's seller arrives beside Ann on the host's next tick.
    let w = net.host.world_mut().unwrap();
    let r = &w.racers[racer];
    let lateral = r.lateral + ROSE_SIDE;
    let seller = RoseSeller {
        pos: w.track.point_at(r.s, lateral),
        yaw: r.yaw,
        s: r.s,
        lateral,
        owner: 1,
        victim: racer as u8,
        tried: 0,
        again: 0,
        side: 1,
        t: 0.0,
    };
    w.play.roses.push(seller);
    let start = w.tick + 1;
    // Compared from once the client can know (a round trip and the lead later).
    let (mut next, mut worst, mut n, mut beside) = (start + 30, 0.0f32, 0, 0.0f32);
    let end = start + (ROSE_SALE * 60.0) as u64;
    while next < end {
        net.step(4);
        if let Some(v) = net.clients[a].s.view() {
            if let Some(r) = v.world.play.roses.first().filter(|r| r.selling()) {
                beside = beside.max((r.pos.distance(v.own().pos) - ROSE_SIDE).abs());
            }
        }
        let Some(ack) = net.clients[a].s.last_ack() else { continue };
        while next <= (ack as u64).min(end - 1) {
            if let (Some(p), Some(h)) = (net.clients[a].s.live_predicted_pos(next as u32), net.history.get(&(racer, next))) {
                worst = worst.max(p.distance(*h));
                n += 1;
            }
            next += 1;
        }
    }
    let w = net.host.world().unwrap();
    assert!(w.racers[racer].effects.rose > 0.0 || w.tick >= end, "the sale ran");
    eprintln!("pestered prediction: {n} ticks, worst {worst:.4} m; seller drawn {beside:.3} m off");
    assert!(n > 200, "compared only {n} ticks");
    assert!(worst < 0.01, "worst {worst}");
    assert!(beside < 0.3, "seller drawn {beside} m off beside the racer");
}

#[test]
fn prediction_among_bumping_bots_stays_bounded() {
    // Bumps, frogs and printer falls reach prediction in the next snapshot. At 100 ms
    // each way a sudden stop briefly leaves the historical prediction several
    // metres ahead; the displayed correction must still ease without a snap.
    let (worst, mean, n, net) = prediction_error_with_bots(7, link(100, 0.0), Pilot::Weave, 25);
    let st = net.clients[0].s.stats();
    eprintln!(
        "prediction with 7 bots around: {n} ticks, worst {worst:.3} m, mean {mean:.4} m, max correction {:.3} m, hard snaps {}",
        st.max_correction_m, st.hard_snaps
    );
    let stop_bound = race_core::Tunables::default().top_speed * 0.2 + 1.0;
    assert!(worst < stop_bound, "worst {worst}, two-way stop bound {stop_bound}");
    assert!(mean < 0.1, "mean {mean}");
    assert_eq!(st.hard_snaps, 0);
}

#[test]
fn prediction_is_exact_for_the_start_boost() {
    // Throttle held only for the last 0.3 s before GO earns the start boost
    // on the host; the client must have predicted it (no lurch at GO).
    let mut net = Net::new(Net::cfg(0, 3));
    let a = net.join("Ann", 0, link(60, 0.0), Pilot::Idle);
    assert!(net.run_until(5, 3000, |n| *n.clients[a].s.phase() == ClientPhase::Lobby));
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let start = net.host.start_host_ms().unwrap();
    let go_ms = start + 3000;
    // Idle until 0.3 s before GO, then full throttle.
    while net.now() < go_ms - 300 {
        net.step(4);
    }
    net.clients[a].pilot = Pilot::Const(Input::go());
    while net.now() < go_ms + 2500 {
        net.step(4);
    }
    let racer = net.host.racer_of(net.clients[a].s.my_seat().unwrap()).unwrap();
    let w = net.host.world().unwrap();
    assert!(w.racers[racer].s > 5.0, "moving");
    let go_tick = w.go_tick;
    let ack = net.clients[a].s.last_ack().unwrap() as u64;
    let mut worst = 0.0f32;
    for seq in (go_tick + 1)..=ack {
        if let (Some(p), Some(h)) = (net.clients[a].s.live_predicted_pos(seq as u32), net.history.get(&(racer, seq))) {
            worst = worst.max(p.distance(*h));
        }
    }
    eprintln!("start-boost prediction worst error {worst:.4} m");
    assert!(worst < 0.5);
}

// ─── (d) disconnect mid-race ────────────────────────────────────────────────

#[test]
fn disconnect_mid_race_turns_the_seat_into_a_bot_and_the_race_completes() {
    // A race in which Bob's bot finishes ahead of Ann, the last human (once
    // she is over the line the race ends). Picked again for the Rose.
    let mut net = Net::new(HostConfig { seed: 104, ..Net::cfg(3, 2) });
    let a = net.join("Ann", 0, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    let b = net.join("Bob", 1, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    net.run_for(5, 500);
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let bob_seat = net.clients[b].s.my_seat().unwrap();
    let bob_racer = net.host.racer_of(bob_seat).unwrap();
    net.run_for(5, 12_000);
    assert_eq!(net.host.world().unwrap().kind(bob_racer), RacerKind::Human);

    // Bob's connection drops (the client object is simply gone).
    let bob = net.clients.remove(b);
    drop(bob);
    assert_eq!(net.clients[0].s.notice(), None);
    net.run_for(5, 500);
    assert_eq!(net.host.world().unwrap().kind(bob_racer), RacerKind::Bot(BotLevel::Normal));
    assert_eq!(net.host.human_count(), 1);
    assert_eq!(net.host.left_racers(), &[bob_racer as u8]);
    // Everyone else sees Bob as a bot seat now, and is told he left.
    let lobby = net.clients[0].s.lobby().unwrap();
    assert_eq!(lobby.humans(), 1);
    assert_eq!(lobby.phase, LobbyPhase::Racing);
    assert_eq!(net.clients[0].s.notice().as_deref(), Some("Bob left"));
    assert_eq!(net.clients[0].s.left_racers(), &[bob_racer as u8]);
    // For a few seconds.
    net.run_for(5, crate::client::NOTICE_MS);
    assert_eq!(net.clients[0].s.notice(), None);

    // The bot drives Bob's racer on (the race is not emptied)...
    assert!(net.run_until(5, 8 * 60_000, |n| n.host.phase() == HostPhase::Results), "race must complete");
    net.run_for(5, 2000);
    let w = net.host.world().unwrap();
    assert!(w.racers[a].finished_at.is_some());
    assert!(w.racers[bob_racer].finished_at.is_some(), "the bot finished Bob's race");
    // ...but its result is not Bob's: Bob is Left, last, with no time or
    // points, and everyone who was behind the bot moves up a place.
    let res = net.host.results().unwrap();
    let last = res.rows.last().unwrap();
    assert_eq!(last.racer as usize, bob_racer);
    assert_eq!((last.name.as_str(), last.bot, last.time, last.best_lap, last.points), ("Bob", false, None, None, 0));
    let mut sim = crate::view::results_from_world(w);
    let bot_place = sim.rows.iter().find(|r| r.racer as usize == bob_racer).unwrap().place;
    assert!(bot_place < res.rows.len() as u8, "the bot did not finish last: {sim:?}");
    for row in &res.rows[..res.rows.len() - 1] {
        let before = sim.rows.iter().find(|r| r.racer == row.racer).unwrap();
        let up = if before.place > bot_place { 1 } else { 0 };
        assert_eq!(row.place, before.place - up, "{row:?}");
        assert_eq!(row.time, before.time);
        assert_eq!(row.points, race_core::GP_POINTS.get(row.place as usize - 1).copied().unwrap_or(0));
    }
    assert_eq!(res.rows.iter().map(|r| r.place).collect::<Vec<_>>(), (1..=res.rows.len() as u8).collect::<Vec<_>>());
    assert_eq!(net.clients[0].s.results().unwrap(), res);
    assert_eq!(net.clients[0].s.left_racers(), &[bob_racer as u8]);
    // The same ranking rule, applied by hand.
    crate::view::rank_leavers_last(&mut sim, &[bob_racer as u8]);
    assert_eq!(&sim, res);
}

#[test]
fn a_player_who_finished_and_then_left_keeps_the_place() {
    let mut net = Net::new(Net::cfg(0, 1));
    let a = net.join("Ann", 0, link(20, 0.0), Pilot::Bot(BotLevel::Hard));
    let b = net.join("Bob", 1, link(20, 0.0), Pilot::Idle);
    net.run_for(5, 500);
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let ann = net.host.racer_of(net.clients[a].s.my_seat().unwrap()).unwrap();
    assert!(net.run_until(5, 4 * 60_000, |n| n.host.world().unwrap().racers[ann].finished_at.is_some()), "Ann finishes");
    assert_eq!(net.host.phase(), HostPhase::Racing, "Bob is still out there");
    // Ann leaves after her finish: Bob is told, but her finish stays hers.
    let ann_client = net.clients.remove(a);
    drop(ann_client);
    net.run_for(5, 500);
    let bob = &net.clients[b - 1].s;
    assert_eq!(bob.notice().as_deref(), Some("Ann left"));
    assert!(bob.left_racers().is_empty());
    assert!(net.host.left_racers().is_empty());
}

/// A battle with Ann and Bob (both flown by bots) and four bots, racing.
fn battle_with_two_players(seed: u64) -> (Net, usize, usize) {
    use crate::proto::BattleSetup;
    let mut cfg = Net::cfg(4, 3);
    cfg.seed = seed;
    let mut net = Net::new(cfg);
    assert!(net.host.set_battle(Some(BattleSetup::new(TrackId::DustBowl))));
    let a = net.join("Ann", 1, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    let b = net.join("Bob", 3, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    assert!(net.run_until(5, 5000, |n| n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Lobby)));
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    assert!(net.host.world().unwrap().battle.is_some());
    (net, a, b)
}

#[test]
fn a_battle_player_who_leaves_while_still_in_is_listed_left_and_last() {
    let (mut net, a, b) = battle_with_two_players(99);
    let bob = net.host.racer_of(net.clients[b].s.my_seat().unwrap()).unwrap();
    net.run_for(5, 3000);
    assert_eq!(net.host.phase(), HostPhase::Racing);
    assert!(!net.host.world().unwrap().is_out(bob), "Bob is still in the battle when he leaves");
    drop(net.clients.remove(b));
    net.run_for(5, 500);
    assert_eq!(net.host.left_racers(), &[bob as u8]);
    assert_eq!(net.clients[a].s.notice().as_deref(), Some("Bob left"));
    assert_eq!(net.clients[a].s.left_racers(), &[bob as u8]);

    // The bot goes on fighting for Bob's racer; the battle ends.
    assert!(net.run_until(5, 5 * 60_000, |n| n.host.phase() == HostPhase::Results), "battle must end");
    let res = net.host.results().unwrap().clone();
    // The ranking rule applied by hand to the world as it ended (before the world moves on).
    let mut sim = crate::view::results_from_world(net.host.world().unwrap());
    net.run_for(5, 2000);
    let last = res.rows.last().unwrap();
    assert_eq!(last.racer as usize, bob);
    assert_eq!((last.name.as_str(), last.bot, last.time, last.best_lap, last.points), ("Bob", false, None, None, 0));
    assert_eq!(res.rows.iter().map(|r| r.place).collect::<Vec<_>>(), (1..=res.rows.len() as u8).collect::<Vec<_>>());
    assert!(res.rows[..res.rows.len() - 1].iter().all(|r| r.racer as usize != bob));
    // Everyone else keeps the battle's order, closed up over Bob's old place.
    let before: Vec<u8> = sim.rows.iter().map(|r| r.racer).filter(|&r| r as usize != bob).collect();
    crate::view::rank_leavers_last(&mut sim, &[bob as u8]);
    assert_eq!(&sim, &res);
    assert_eq!(res.rows[..res.rows.len() - 1].iter().map(|r| r.racer).collect::<Vec<_>>(), before);
    assert_eq!(net.clients[a].s.results(), Some(&res));
    assert_eq!(net.clients[a].s.left_racers(), &[bob as u8]);
}

#[test]
fn a_battle_player_who_is_already_out_and_then_leaves_keeps_the_place() {
    let (mut net, a, b) = battle_with_two_players(99);
    let bob = net.host.racer_of(net.clients[b].s.my_seat().unwrap()).unwrap();
    net.run_for(5, 3000);
    // Knock Bob out of the battle (what losing the last feather does).
    net.host.world_mut().unwrap().battle.as_mut().unwrap().out_order.push(bob);
    assert!(net.host.world().unwrap().is_out(bob));
    drop(net.clients.remove(b));
    net.run_for(5, 500);
    assert_eq!(net.clients[a].s.notice().as_deref(), Some("Bob left"));
    assert!(net.host.left_racers().is_empty(), "the out place is Bob's");
    assert!(net.clients[a].s.left_racers().is_empty());
}

#[test]
fn an_eliminated_player_who_leaves_keeps_the_place() {
    use race_core::{ChallengeId, ChallengeState};
    let mut net = Net::new(Net::cfg(3, 2));
    net.join("Ann", 0, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    let b = net.join("Bob", 1, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    net.run_for(5, 500);
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let bob = net.host.racer_of(net.clients[b].s.my_seat().unwrap()).unwrap();
    net.run_for(5, 2000);
    // Online races are plain races or battles, so put Bob in the Elimination
    // challenge's eliminated list by hand (the rule must not depend on the mode).
    let mut cs = ChallengeState::new(ChallengeId::Elimination);
    cs.eliminated.push(bob);
    cs.done = true;
    net.host.world_mut().unwrap().challenge = Some(cs);
    assert!(net.host.world().unwrap().is_eliminated(bob));
    drop(net.clients.remove(b));
    net.run_for(5, 500);
    assert_eq!(net.clients[0].s.notice().as_deref(), Some("Bob left"));
    assert!(net.host.left_racers().is_empty(), "the eliminated place is Bob's");
    assert!(net.clients[0].s.left_racers().is_empty());
}

/// Who has left travels with the room: a host rebuilt from its checkpoint
/// (what a promoted standby does) still lists the leaver as Left, a player who
/// had finished is still not listed, and a checkpoint of another schema is
/// refused rather than misread.
#[test]
fn the_leavers_survive_a_checkpoint_and_another_schema_is_refused() {
    use crate::host::migration::{checkpoint, restore};
    use crate::transport::NullHostTransport;
    let mut net = Net::new(Net::cfg(3, 2));
    net.join("Ann", 0, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    let b = net.join("Bob", 1, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    net.run_for(5, 500);
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let bob = net.host.racer_of(net.clients[b].s.my_seat().unwrap()).unwrap();
    net.run_for(5, 3000);
    drop(net.clients.remove(b));
    net.run_for(5, 500);
    assert_eq!(net.host.left_racers(), &[bob as u8]);

    let (binding, bytes) = checkpoint(&net.host).unwrap();
    let back = restore(&bytes, &binding, Box::new(NullHostTransport)).unwrap();
    assert_eq!(back.left_racers(), &[bob as u8], "the list of leavers is carried");
    // (The schema is the first field after the magic: u32, big-endian.)
    let at = b"stampede/authority-checkpoint/v1\0".len();
    assert_eq!(&bytes[at..at + 4], &[0, 0, 0, 2], "schema two carries the leavers");
    let mut old = bytes.clone();
    old[at + 3] = 1;
    assert!(restore(&old, &binding, Box::new(NullHostTransport)).is_err(), "schema one is refused");
}

#[test]
fn ranking_leavers_last_renumbers_places_and_points() {
    use crate::proto::{ResultRow, ResultsView};
    let row = |racer: u8, place: u8| ResultRow {
        racer,
        name: format!("r{racer}"),
        character: 0,
        place,
        bot: racer == 2,
        time: Some(60.0 + place as f32),
        best_lap: Some(20.0),
        coins: 1,
        points: race_core::GP_POINTS[place as usize - 1],
    };
    let mut res = ResultsView { track: TrackId::Savanna, laps: 3, rows: vec![row(2, 1), row(0, 2), row(3, 3), row(1, 4)] };
    let untouched = res.clone();
    crate::view::rank_leavers_last(&mut res, &[]);
    assert_eq!(res, untouched);
    crate::view::rank_leavers_last(&mut res, &[2]);
    let got: Vec<(u8, u8, bool, Option<f32>, u8)> = res.rows.iter().map(|r| (r.racer, r.place, r.bot, r.time, r.points)).collect();
    let gp = race_core::GP_POINTS;
    assert_eq!(got, vec![(0, 1, false, Some(62.0), gp[0]), (3, 2, false, Some(63.0), gp[1]), (1, 3, false, Some(64.0), gp[2]), (2, 4, false, None, 0)]);
    assert_eq!(res.rows[3].best_lap, None);
}

#[test]
fn a_player_removed_by_the_host_is_told_it_was_removed_and_the_seat_becomes_a_bot() {
    let mut net = Net::new(Net::cfg(3, 2));
    let _a = net.join("Ann", 0, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    let b = net.join("Bob", 1, link(40, 0.0), Pilot::Bot(BotLevel::Normal));
    net.run_for(5, 500);
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let seat = net.clients[b].s.my_seat().unwrap();
    let racer = net.host.racer_of(seat).unwrap();
    let peer = net.host.peer_of_seat(seat).unwrap();
    assert!(net.host.remove_peer(peer));
    assert!(!net.host.remove_peer(peer), "a second removal finds nobody");
    net.run_for(5, 500);
    assert_eq!(net.clients[b].s.phase(), &ClientPhase::Closed("kicked".into()), "told it was removed, not that the host left");
    assert_eq!(net.host.world().unwrap().kind(racer), RacerKind::Bot(BotLevel::Normal));
    assert_eq!(net.host.human_count(), 1);
    assert_eq!(net.clients[0].s.lobby().unwrap().humans(), 1);
}

#[test]
fn a_silent_seat_is_dropped_to_a_bot() {
    let mut net = Net::new(Net::cfg(2, 1));
    let a = net.join("Ann", 0, link(20, 0.0), Pilot::Idle);
    let b = net.join("Bob", 1, link(20, 0.0), Pilot::Idle);
    net.run_for(5, 300);
    net.clients[a].s.set_ready(true);
    net.clients[b].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let racer = net.host.racer_of(net.clients[a].s.my_seat().unwrap()).unwrap();
    // Ann freezes (stops updating) but her link stays up; Bob plays on.
    let end = net.now() + crate::host::SILENCE_MS + 4000;
    while net.now() < end {
        let now = net.t.fetch_add(5, Ordering::Relaxed) + 5;
        net.host.update(now);
        let skew = net.clients[b].skew;
        net.clients[b].s.update(now + skew, Input::go());
    }
    assert_eq!(net.host.world().unwrap().kind(racer), RacerKind::Bot(BotLevel::Normal));
    assert_eq!(net.host.human_count(), 1);
}

#[test]
fn the_last_human_leaving_resets_the_room_to_the_lobby() {
    let mut net = Net::new(Net::cfg(2, 1));
    let a = net.join("Ann", 0, link(20, 0.0), Pilot::Idle);
    net.run_for(5, 300);
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    net.clients[a].s.leave();
    net.run_for(5, 300);
    assert_eq!(net.host.phase(), HostPhase::Lobby);
    assert!(net.host.world().is_none());
    assert_eq!(net.host.human_count(), 0);
}

#[test]
fn a_starved_seat_repeats_its_last_input_as_held() {
    let mut net = Net::new(Net::cfg(0, 3));
    let steer = Input { throttle: 1.0, steer: 0.05, jump: true, ..Input::default() };
    let a = net.join("Ann", 0, link(30, 0.0), Pilot::Const(steer));
    net.run_for(5, 300);
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let seat = net.clients[a].s.my_seat().unwrap();
    let racer = net.host.racer_of(seat).unwrap();
    net.run_until(4, 8000, |n| n.host.world().is_some_and(|w| w.phase == RacePhase::Racing && w.race_time() > 1.0));
    let before = net.host.world().unwrap().racers[racer].clone();
    // The client goes quiet for a while (still connected): the host repeats.
    let starved0 = net.host.stats().starved_ticks;
    let end = net.now() + 1500;
    while net.now() < end {
        let now = net.t.fetch_add(4, Ordering::Relaxed) + 4;
        net.host.update(now);
    }
    let w = net.host.world().unwrap();
    assert!(net.host.stats().starved_ticks > starved0 + 50);
    let after = &w.racers[racer];
    assert!(after.speed > before.speed + 5.0 && after.speed > 15.0, "kept running: speed {} -> {}", before.speed, after.speed);
    // Held, not re-pressed: a repeat can never fire a press (the next real
    // input then sees no edge either), and the client's replay agrees.
    assert!(after.prev_input.jump, "held level kept");
    assert!((after.prev_input.steer - 0.05).abs() < 0.02, "steer kept: {}", after.prev_input.steer);
}

// ─── (f) rematch ────────────────────────────────────────────────────────────

#[test]
fn rematch_everyone_ready_cancellable_3_2_1_then_a_new_race() {
    let mut net = Net::new(Net::cfg(2, 1));
    let a = net.join("Ann", 0, link(30, 0.0), Pilot::Bot(BotLevel::Hard));
    let b = net.join("Bob", 1, link(30, 0.0), Pilot::Bot(BotLevel::Hard));
    net.run_for(5, 400);
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    let seed1 = net.host.world().unwrap().cfg.seed;
    assert!(net.run_until(5, 6 * 60_000, |n| n.host.phase() == HostPhase::Results));
    net.run_for(5, 1000);
    assert!(net.clients.iter().all(|c| *c.s.phase() == ClientPhase::Results));
    assert!(net.clients.iter().all(|c| !c.s.my_lobby_seat().unwrap().ready), "ready cleared for the rematch");

    // One ready is not enough.
    net.clients[a].s.set_rematch_ready(true);
    net.run_for(5, 500);
    assert!(net.host.rematch_ends_at().is_none());
    assert!(net.clients[b].s.rematch_ms_left(net.now() + net.clients[b].skew).is_none());
    // Both ready: the 3-2-1 starts and both clients see it.
    net.clients[b].s.set_rematch_ready(true);
    net.run_for(5, 400);
    let ends = net.host.rematch_ends_at().expect("countdown");
    for c in &net.clients {
        let left = c.s.rematch_ms_left(net.now() + c.skew).expect("client sees the countdown");
        assert!((2000..=3000).contains(&left), "{left}");
    }
    // Bob changes his mind: cancelled for everyone.
    net.clients[b].s.set_rematch_ready(false);
    net.run_for(5, 400);
    assert!(net.host.rematch_ends_at().is_none());
    assert!(net.clients.iter().all(|c| c.s.rematch_ms_left(net.now() + c.skew).is_none()));
    assert_eq!(net.host.phase(), HostPhase::Results, "no race started");
    // Ready again, let it run out: a new race, new seed, tick 0.
    net.clients[b].s.set_rematch_ready(true);
    net.run_for(5, 200);
    let ends2 = net.host.rematch_ends_at().expect("restarted from 3");
    assert!(ends2 > ends);
    assert!(net.run_until(5, 5000, |n| n.host.phase() == HostPhase::Racing));
    assert_ne!(net.host.world().unwrap().cfg.seed, seed1);
    assert!(net.host.world().unwrap().tick < 120);
    net.run_for(5, 1500);
    for c in &net.clients {
        assert_eq!(*c.s.phase(), ClientPhase::Racing);
        assert!(c.s.results().is_none());
        assert!(!c.s.view().unwrap().over);
        assert_ne!(c.s.view().unwrap().world.cfg.seed, seed1);
    }
    // And that race completes too.
    assert!(net.run_until(5, 6 * 60_000, |n| n.host.phase() == HostPhase::Results));
}

#[test]
fn host_can_return_everyone_to_the_lobby_from_the_results() {
    let mut net = Net::new(Net::cfg(3, 1));
    let a = net.join("Ann", 0, link(10, 0.0), Pilot::Bot(BotLevel::Hard));
    net.run_for(5, 300);
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    assert!(net.run_until(5, 6 * 60_000, |n| n.host.phase() == HostPhase::Results));
    net.run_for(5, 500);
    assert!(!net.host.set_track(TrackId::Dune), "settings are for the lobby");
    net.host.to_lobby();
    net.run_for(5, 300);
    assert_eq!(*net.clients[a].s.phase(), ClientPhase::Lobby);
    assert!(net.clients[a].s.view().is_none());
    assert!(net.host.set_track(TrackId::Dune));
    assert!(net.host.set_bots(1));
    assert!(net.host.set_laps(2));
    net.run_for(5, 300);
    let v = net.clients[a].s.lobby().unwrap();
    assert_eq!((v.track, v.laps, v.bots), (TrackId::Dune, 2, 1));
    assert_eq!(v.seats.len(), 2);
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    assert_eq!(net.host.world().unwrap().cfg.track, TrackId::Dune);
}

// ─── (e) sizes in a live race ───────────────────────────────────────────────

#[test]
fn live_snapshots_and_events_stay_inside_the_datagram_limits() {
    // Eight on the grid with every power-up in play: whatever the sim emits.
    let mut net = Net::new(Net::cfg(7, 1));
    let a = net.join("Ann", 0, link(15, 0.0), Pilot::Bot(BotLevel::Hard));
    net.run_for(5, 300);
    net.clients[a].s.set_ready(true);
    assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
    net.run_until(5, 3 * 60_000, |n| n.host.phase() == HostPhase::Results);
    let st = net.host.stats();
    eprintln!("8 racers: max snapshot {} B over {} snapshots", st.snapshot_bytes_max, st.snapshots_sent);
    assert!(st.snapshot_bytes_max > 400 && st.snapshot_bytes_max <= 1024);
    assert_eq!(st.entities_dropped, 0, "a real race stays well inside the budget");
}

// ─── Offline: same view API ─────────────────────────────────────────────────

#[test]
fn local_race_runs_to_results_through_the_same_view_api() {
    use race_core::RaceConfig;
    let cfg = RaceConfig::quick(TrackId::Savanna, 3, BotLevel::Normal, 5);
    let mut cfg = cfg;
    cfg.laps = 1;
    let mut race = LocalRace::new(cfg, 0);
    let mut t = 0u64;
    let mut counted = (0, 0);
    while race.results().is_none() && t < 5 * 60_000 {
        t += 16;
        let input = race.view().map_or(Input::default(), |v| {
            if v.world.phase == RacePhase::Countdown && v.hud.countdown > 0.25 {
                Input::default()
            } else {
                bot_input(&v.world, v.you, BotLevel::Normal)
            }
        });
        race.update(t, input);
        for e in &race.view().unwrap().events {
            match e {
                race_core::Event::Countdown { .. } => counted.0 += 1,
                race_core::Event::Go => counted.1 += 1,
                _ => {}
            }
        }
    }
    let res = race.results().expect("race ends");
    assert_eq!(counted, (3, 1));
    assert_eq!(res.rows.len(), 4);
    assert!(race.is_over());
    let v = race.view().unwrap();
    assert!(v.over);
    assert_eq!(v.standings, res.standings());
    assert!(v.hud.finished && v.hud.finish_time.is_some());
    assert_eq!(v.hud.laps, 1);
    // Pause freezes the sim.
    let mut r2 = LocalRace::new(RaceConfig::quick(TrackId::Savanna, 1, BotLevel::Easy, 1), 0);
    for i in 0..30 {
        r2.update(i * 16, Input::go());
    }
    let tick = r2.world().tick;
    assert!(tick > 20);
    r2.set_paused(true);
    for i in 30..60 {
        r2.update(i * 16, Input::go());
    }
    assert_eq!(r2.world().tick, tick);
    r2.set_paused(false);
    for i in 60..90 {
        r2.update(i * 16, Input::go());
    }
    assert!(r2.world().tick > tick);
}

#[test]
fn local_race_trains_a_lesson() {
    use race_core::{LessonId, RaceConfig};
    let cfg = RaceConfig::training(LessonId::RunSteer, RacerKind::Human, 3);
    let mut race = LocalRace::new(cfg, 0);
    let mut t = 0;
    while t < 120_000 && race.results().is_none() {
        t += 16;
        let input = race.view().map_or(Input::default(), |v| bot_input(&v.world, 0, BotLevel::Hard));
        let input = if race.world().phase == RacePhase::Countdown { Input::default() } else { Input { throttle: 1.0, ..input } };
        race.update(t, input);
    }
    let lesson = race.view().unwrap().hud.lesson.clone().expect("lesson state in the HUD");
    assert!(lesson.progress > 0, "{lesson:?}");
}

// ─── The host's own player through the mux ──────────────────────────────────

#[test]
fn host_plays_through_the_mux_beside_a_remote_peer() {
    use crate::transport::MuxHostTransport;
    let (clock, t) = LoopClock::manual();
    t.store(5_000, Ordering::Relaxed);
    let hub = LoopbackHub::new(clock.clone(), 1);
    let remote_hub = LoopbackHub::new(clock, 2);
    let mux = MuxHostTransport::new(hub.host(), Box::new(remote_hub.host()));
    let mut host = HostAuthority::new(Box::new(mux), Net::cfg(2, 1));
    let mut me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Me", 4);
    let mut friend = ClientSession::new(Box::new(remote_hub.connect_via(link(25, 0.0), LinkPath::Direct)), "Friend", 6);
    let mut now = 5_000;
    let mut tick = |host: &mut HostAuthority, me: &mut ClientSession, friend: &mut ClientSession, ms: u64| {
        now += ms;
        t.store(now, Ordering::Relaxed);
        host.update(now);
        me.update(now, Input::go());
        friend.update(now + 999, Input::go());
    };
    for _ in 0..80 {
        tick(&mut host, &mut me, &mut friend, 5);
    }
    me.set_ready(true);
    friend.set_ready(true);
    for _ in 0..200 {
        tick(&mut host, &mut me, &mut friend, 5);
    }
    assert_eq!(host.phase(), HostPhase::Racing);
    let v = me.lobby().unwrap();
    assert!(v.seat(me.my_seat().unwrap()).unwrap().host, "the local seat is the host's");
    assert!(!v.seat(friend.my_seat().unwrap()).unwrap().host);
    assert_eq!(me.view().unwrap().you, 0);
    assert_eq!(friend.view().unwrap().you, 1);
    // Zero-latency player: the host leaves with no lead beyond the buffer.
    for _ in 0..1200 {
        tick(&mut host, &mut me, &mut friend, 5);
    }
    assert!(me.view().unwrap().own().s > 10.0);
    assert!(host.stats().starved_ticks < 40, "starved {}", host.stats().starved_ticks);
}

#[test]
fn client_clock_sync_recovers_the_host_clock_across_skew() {
    let mut net = Net::new(Net::cfg(1, 1));
    let a = net.join("Ann", 0, link(80, 0.0), Pilot::Idle);
    net.run_for(5, 1500);
    let c = &net.clients[a];
    let host_now = c.s.host_now(net.now() + c.skew).expect("synced");
    let err = host_now as i64 - net.now() as i64;
    assert!(err.abs() <= 6, "clock error {err} ms");
    assert!((155..=170).contains(&c.s.rtt_ms().unwrap()), "{:?}", c.s.rtt_ms());
}


// ─── Review fixes: input edges, seat ids, hostile peers, own launch FX ──────

mod review {
    use super::*;
    use crate::client::{correction_left, correction_ms, SNAP_DISTANCE};
    use crate::proto::{decode, encode, LossyToHost, Snapshot, ToClient, ToHost, PROTO_VERSION};
    use crate::transport::{ClientNetEvent, ClientTransport, HostNetEvent, HostTransport, LoopbackClient};
    use race_core::{Event, RaceConfig, RaceMode, RacerSetup, World};

    /// A hand-driven client: inputs with chosen numbers at chosen ticks.
    struct Raw {
        t: Arc<AtomicU64>,
        host: HostAuthority,
        c: LoopbackClient,
        race: u8,
        racer: u8,
        events: Vec<Event>,
    }

    impl Raw {
        fn new() -> Raw {
            let (clock, t) = LoopClock::manual();
            t.store(10_000, Ordering::Relaxed);
            let hub = LoopbackHub::new(clock, 3);
            let mut host = HostAuthority::new(Box::new(hub.host()), Net::cfg(0, 3));
            let mut c = hub.connect(LoopbackConfig::PERFECT);
            c.send_reliable(encode(&ToHost::Hello { proto: PROTO_VERSION, name: "Raw".into(), character: 0 }));
            c.send_reliable(encode(&ToHost::SetReady(true)));
            host.update(10_000);
            let mut start = None;
            while let Some(ev) = c.poll() {
                if let ClientNetEvent::Reliable(b) = ev {
                    if let Some(ToClient::RaceStart { race, you, .. }) = decode(&b) {
                        start = Some((race, you));
                    }
                }
            }
            let (race, racer) = start.expect("race started");
            Raw { t, host, c, race, racer, events: Vec::new() }
        }

        fn tick(&self) -> u64 {
            self.host.world().unwrap().tick
        }

        fn send(&mut self, seq: u32, inputs: &[Input]) {
            let inputs = inputs.iter().map(|i| i.pack()).collect();
            self.c.send_lossy(encode(&LossyToHost::Inputs { race: self.race, seq, inputs }));
        }

        /// Step exactly one world tick.
        fn step(&mut self) {
            let start = self.host.start_host_ms().unwrap();
            let now = start + self.tick() * 1000 / 60;
            self.t.store(now, Ordering::Relaxed);
            let before = self.tick();
            self.host.update(now);
            assert_eq!(self.tick(), before + 1);
            while let Some(ev) = self.c.poll() {
                if let ClientNetEvent::Reliable(b) = ev {
                    if let Some(ToClient::Events(v)) = decode(&b) {
                        self.events.extend(v);
                    }
                }
            }
        }

        fn jumps(&self) -> usize {
            self.events.iter().filter(|e| matches!(e, Event::Jump { racer } if *racer == self.racer)).count()
        }

        fn on_ground(&self) -> bool {
            self.host.world().unwrap().racers[self.racer as usize].on_ground
        }
    }

    #[test]
    fn a_held_jump_does_not_fire_twice_across_a_starved_tick() {
        let mut r = Raw::new();
        let go = r.host.world().unwrap().go_tick;
        let hold = Input { throttle: 1.0, jump: true, ..Input::default() };
        // Holding the button glides now, so the bird is down about 1.5 s after the press.
        let gap = go + 170;
        while r.tick() < go + 260 {
            let t = r.tick();
            let input = if t >= go + 60 { hold } else { Input::go() };
            if t == gap {
                assert!(r.on_ground(), "landed from the first jump, still holding");
            } else {
                // One input per datagram, so skipping one really starves the tick.
                r.send(t as u32 + 1, &[input]);
            }
            r.step();
        }
        assert!(r.host.stats().starved_ticks >= 1);
        assert_eq!(r.jumps(), 1, "one press, one jump: {:?}", r.events.iter().filter(|e| matches!(e, Event::Jump { .. })).collect::<Vec<_>>());
    }

    #[test]
    fn the_host_rejects_an_item_use_while_a_timed_item_is_active() {
        use race_core::Item;
        let mut r = Raw::new();
        let go = r.host.world().unwrap().go_tick;
        let me = r.racer as usize;
        let go_in = Input::go();
        let press = Input { item: true, ..Input::go() };
        // Drive to a few ticks after GO, then hand over a pill and press for it.
        while r.tick() < go + 30 {
            let t = r.tick();
            r.send(t as u32 + 1, &[go_in]);
            r.step();
        }
        r.host.world_mut().unwrap().give_item(me, Item::OrangePill, 1);
        let used = |r: &Raw, item: Item| r.events.iter().filter(|e| matches!(e, Event::ItemUsed { racer, item: i } if *racer == me as u8 && *i == item)).count();
        let t = r.tick();
        r.send(t as u32 + 1, &[press]);
        r.step();
        assert_eq!(used(&r, Item::OrangePill), 1, "the pill fires");
        assert!(r.host.world().unwrap().racers[me].item.is_none(), "the slot empties at once");
        // A feather is collected while the pill runs; pressing for it is refused.
        r.host.world_mut().unwrap().give_item(me, Item::Feather, 1);
        for _ in 0..3 {
            let t = r.tick();
            r.send(t as u32 + 1, &[go_in]);
            r.step();
            let t = r.tick();
            r.send(t as u32 + 1, &[press]);
            r.step();
        }
        assert_eq!(used(&r, Item::Feather), 0, "refused while the pill runs");
        assert_eq!(r.host.world().unwrap().racers[me].item, Some(Item::Feather), "and not wasted");
        // The pill ends; the very next press goes through.
        r.host.world_mut().unwrap().racers[me].effects.pill = 0.0;
        let t = r.tick();
        r.send(t as u32 + 1, &[go_in]);
        r.step();
        let t = r.tick();
        r.send(t as u32 + 1, &[press]);
        r.step();
        assert_eq!(used(&r, Item::Feather), 1, "usable the moment the pill ends");
    }

    #[test]
    fn a_press_that_arrives_late_fires_once() {
        let mut r = Raw::new();
        let go = r.host.world().unwrap().go_tick;
        let tap = Input { throttle: 1.0, jump: true, ..Input::default() };
        let press = go + 60;
        while r.tick() < go + 200 {
            let t = r.tick();
            if t == press {
                // The datagram carrying the tap is lost; the next one (with the
                // tap as its redundant copy) comes after the tick was used.
            } else if t == press + 1 {
                r.send(t as u32 + 1, &[tap, Input::go()]);
            } else if t == press + 100 {
                // A stray duplicate of the old datagram, long after.
                r.send(press as u32 + 2, &[tap, Input::go()]);
                r.send(t as u32 + 1, &[Input::go()]);
            } else {
                r.send(t as u32 + 1, &[Input::go()]);
            }
            r.step();
        }
        assert_eq!(r.jumps(), 1);
    }

    #[test]
    fn seat_ids_stay_below_the_bot_range_and_never_wrap() {
        let (clock, t) = LoopClock::manual();
        t.store(1, Ordering::Relaxed);
        let hub = LoopbackHub::new(clock, 1);
        let mut host = HostAuthority::new(Box::new(hub.host()), Net::cfg(7, 1));
        let hello = |c: &mut LoopbackClient| {
            c.send_reliable(encode(&ToHost::Hello { proto: PROTO_VERSION, name: "x".into(), character: 1 }))
        };
        let mut stay = hub.connect(LoopbackConfig::PERFECT);
        hello(&mut stay);
        host.update(1);
        for i in 0..300u64 {
            let mut c = hub.connect(LoopbackConfig::PERFECT);
            hello(&mut c);
            host.update(2 + i);
            let v = host.lobby_view();
            assert_eq!(v.humans(), 2);
            let mut ids: Vec<u8> = v.seats.iter().map(|s| s.id).collect();
            assert!(v.seats.iter().all(|s| (s.id < 100) != s.bot), "{ids:?}");
            ids.dedup();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), v.seats.len(), "unique ids");
            c.close();
            host.update(2 + i);
        }
        assert_eq!(host.human_count(), 1);
    }

    #[test]
    fn bot_seat_ids_are_the_same_in_the_lobby_and_the_race() {
        let mut net = Net::new(Net::cfg(3, 1));
        let a = net.join("Ann", 0, link(10, 0.0), Pilot::Idle);
        let b = net.join("Bob", 1, link(10, 0.0), Pilot::Idle);
        net.run_for(5, 300);
        let bots = |v: &crate::proto::LobbyView| v.seats.iter().filter(|s| s.bot).map(|s| s.id).collect::<Vec<_>>();
        let lobby_ids = bots(net.clients[a].s.lobby().unwrap());
        assert_eq!(lobby_ids, vec![102, 103, 104]);
        net.clients[a].s.set_ready(true);
        net.clients[b].s.set_ready(true);
        assert!(net.run_until(5, 3000, |n| n.clients.iter().all(|c| *c.s.phase() == ClientPhase::Racing)));
        net.run_for(5, 100);
        let v = net.clients[a].s.lobby().unwrap().clone();
        assert_eq!(v.phase, LobbyPhase::Racing);
        assert_eq!(bots(&v), lobby_ids, "same bot ids once racing");
        let w = net.host.world().unwrap();
        for s in v.seats.iter().filter(|s| s.bot) {
            let r = s.racer().expect("bot racer");
            assert!(matches!(w.kind(r), RacerKind::Bot(_)));
            assert_eq!(w.cfg.racers[r].name, s.name);
        }
    }

    #[test]
    fn a_hostile_host_cannot_crash_the_client() {
        let (clock, t) = LoopClock::manual();
        t.store(100_000, Ordering::Relaxed);
        let hub = LoopbackHub::new(clock, 1);
        let mut h = hub.host();
        let mut s = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Me", 0);
        let mut now = 100_000;
        let mut peer = None;
        let mut frame = |h: &mut crate::transport::LoopbackHost, s: &mut ClientSession, now: u64, input: Input| {
            t.store(now, Ordering::Relaxed);
            s.update(now, input);
            while let Some(ev) = h.poll() {
                match ev {
                    HostNetEvent::Reliable { peer: p, bytes } => {
                        peer = Some(p);
                        match decode::<ToHost>(&bytes) {
                            Some(ToHost::Hello { .. }) => {
                                h.send_reliable(p, encode(&ToClient::Welcome { proto: PROTO_VERSION, seat: 0 }))
                            }
                            Some(ToHost::Ping { id, client_t }) => {
                                h.send_reliable(p, encode(&ToClient::Pong { id, client_t, host_t: now }))
                            }
                            _ => {}
                        }
                    }
                    HostNetEvent::Joined { peer: p, .. } => peer = Some(p),
                    _ => {}
                }
            }
            peer
        };
        let mut p = None;
        for _ in 0..60 {
            now += 10;
            p = frame(&mut h, &mut s, now, Input::default()).or(p);
        }
        let p = p.expect("linked");
        assert_eq!(*s.phase(), ClientPhase::Lobby);
        assert!(s.clock_offset_ms().is_some());

        // Ten racers (the world keeps eight) and `you` = 9.
        let mut cfg = RaceConfig::all_bots(TrackId::Savanna, 8, BotLevel::Normal, 1);
        cfg.racers.push(cfg.racers[0].clone());
        cfg.racers.push(cfg.racers[0].clone());
        h.send_reliable(p, encode(&ToClient::RaceStart { config: cfg, custom: None, start_host_ms: now, you: 9, race: 1 }));
        now += 10;
        frame(&mut h, &mut s, now, Input::default());
        assert_eq!(*s.phase(), ClientPhase::Lobby, "refused");

        // A real race whose GO is long past, then a poisoned snapshot.
        let cfg = RaceConfig {
            track: TrackId::Savanna,
            laps: 1,
            seed: 1,
            mode: RaceMode::Race,
            racers: vec![
                RacerSetup { name: "Me".into(), character: 0, kind: RacerKind::Human },
                RacerSetup { name: "Bot".into(), character: 1, kind: RacerKind::Bot(BotLevel::Easy) },
            ],
            drill: None,
            attire: [[0; 3]; 8],
            le_mans: false,
        };
        let w = World::new(cfg.clone());
        h.send_reliable(p, encode(&ToClient::RaceStart { config: cfg, custom: None, start_host_ms: now - 5000, you: 0, race: 2 }));
        let drive = Input { throttle: 1.0, ..Input::default() };
        for _ in 0..3 {
            now += 16;
            frame(&mut h, &mut s, now, drive);
        }
        assert_eq!(*s.phase(), ClientPhase::Racing);
        let mut own = w.racers[0].clone();
        own.drift.active = true;
        own.drift.tier = 9;
        own.coins = 255;
        let snap = Snapshot {
            race: 2,
            tick: 290,
            ack_seq: 0,
            flags: 0,
            own,
            others: vec![w.racers[1].to_net()],
            projectiles: vec![],
            hazards: vec![],
            loose_coins: vec![],
            targets_alive: vec![],
            play: Default::default(),
            coins_alive: crate::proto::pack_bits(w.coins.iter().map(|_| true)),
            battle: Some(crate::proto::BattleNet {
                feathers: vec![9; 40],
                out_order: vec![250, 0],
                hits: vec![1; 3],
                ..Default::default()
            }),
        };
        h.send_lossy(p, snap.encode());
        h.send_reliable(
            p,
            encode(&ToClient::Events(vec![
                Event::Hit { victim: 200, by: Some(1), cause: race_core::HitCause::Bash },
                Event::FeatherLost { racer: 200, by: None },
                Event::FeatherStolen { racer: 1, from: 99 },
                Event::Out { racer: 77, place: 1, pos: [0.0; 3] },
                Event::Jump { racer: 9 },
                Event::TargetDestroyed { racer: 1, target: 60_000, item: None, uses: 0 },
                Event::CoinPicked { racer: 1, coins: 3 },
                Event::CoinsTaken { from: 1, to: 99, count: 5 },
                Event::CoinsTaken { from: 120, to: 1, count: 5 },
            ])),
        );
        let mut seen = Vec::new();
        for _ in 0..10 {
            now += 16;
            frame(&mut h, &mut s, now, drive);
            seen.extend(s.view().unwrap().events.iter().cloned());
        }
        assert!(seen.contains(&Event::CoinPicked { racer: 1, coins: 3 }), "{seen:?}");
        assert!(!seen.iter().any(|e| matches!(e, Event::Hit { .. } | Event::TargetDestroyed { .. } | Event::Jump { racer: 9 } | Event::CoinsTaken { .. })));
        assert!(!seen.iter().any(|e| matches!(e, Event::FeatherLost { .. } | Event::FeatherStolen { .. } | Event::Out { .. })));
        assert!(s.view().unwrap().world.battle.is_none(), "a race ignores battle data");

        // Results naming a racer that does not exist are ignored.
        let row = |racer: u8, place: u8| crate::proto::ResultRow {
            racer,
            name: "x".into(),
            character: 0,
            place,
            bot: false,
            time: None,
            best_lap: None,
            coins: 0,
            points: 0,
        };
        let bad = crate::proto::ResultsView { track: TrackId::Savanna, laps: 1, rows: vec![row(0, 1), row(200, 2)] };
        h.send_reliable(p, encode(&ToClient::Results(bad)));
        now += 16;
        frame(&mut h, &mut s, now, drive);
        assert!(s.results().is_none());
        assert_eq!(*s.phase(), ClientPhase::Racing);
        assert!(s.view().unwrap().standings.iter().all(|&i| i < 2));

        // A start time at the end of time, then a launch event.
        h.send_reliable(p, encode(&ToClient::RaceStart {
            custom: None,
            config: RaceConfig::all_bots(TrackId::Savanna, 2, BotLevel::Easy, 3),
            start_host_ms: u64::MAX,
            you: 0,
            race: 3,
        }));
        h.send_reliable(p, encode(&ToClient::Events(vec![Event::StartBoost { racer: 0 }, Event::Stumble { racer: 0 }])));
        for _ in 0..5 {
            now += 16;
            frame(&mut h, &mut s, now, drive);
        }
        assert_eq!(*s.phase(), ClientPhase::Racing);
        // And the other end of time.
        h.send_reliable(p, encode(&ToClient::RaceStart {
            custom: None,
            config: RaceConfig::all_bots(TrackId::Savanna, 2, BotLevel::Easy, 3),
            start_host_ms: 1 << 63,
            you: 0,
            race: 4,
        }));
        for _ in 0..5 {
            now += 16;
            frame(&mut h, &mut s, now, drive);
        }
        assert_eq!(*s.phase(), ClientPhase::Racing);
    }

    #[test]
    fn own_start_boost_sounds_at_go_not_a_round_trip_later() {
        let mut net = Net::new(Net::cfg(0, 3));
        let a = net.join("Ann", 0, link(150, 0.0), Pilot::Idle);
        assert!(net.run_until(5, 3000, |n| *n.clients[a].s.phase() == ClientPhase::Lobby));
        net.clients[a].s.set_ready(true);
        assert!(net.run_until(5, 3000, |n| n.host.phase() == HostPhase::Racing));
        let go_ms = net.host.start_host_ms().unwrap() + 3000;
        let mut heard = Vec::new();
        while net.now() < go_ms + 1500 {
            if net.now() >= go_ms - 600 {
                net.clients[a].pilot = Pilot::Const(Input::go());
            }
            net.step(4);
            let c = &net.clients[a];
            let Some(host_now) = c.s.host_now(net.now() + c.skew) else { continue };
            for e in &c.s.view().unwrap().events {
                if matches!(e, Event::StartBoost { racer: 0 } | Event::Stumble { racer: 0 }) {
                    heard.push((e.clone(), host_now));
                }
            }
        }
        let w = net.host.world().unwrap();
        assert!(w.racers[0].boost_t > 0.0 || w.racers[0].s > 0.0);
        assert_eq!(heard.len(), 1, "once: {heard:?}");
        assert_eq!(heard[0].0, Event::StartBoost { racer: 0 });
        assert!(heard[0].1 <= go_ms + 20, "at GO, not {} ms after", heard[0].1 - go_ms);
    }

    #[test]
    fn corrections_ease_longer_when_bigger_and_snap_only_past_ten_metres() {
        assert_eq!(SNAP_DISTANCE, 10.0);
        assert_eq!(correction_ms(0.3), 150.0);
        assert!(correction_ms(3.0) > 150.0 && correction_ms(3.0) < 250.0);
        assert_eq!(correction_ms(9.9), 250.0);
        assert_eq!(correction_left(0.0, 150.0), 1.0);
        assert_eq!(correction_left(150.0, 150.0), 0.0);
        assert!(correction_left(75.0, 150.0) > 0.4 && correction_left(75.0, 150.0) < 0.6);
    }
}

// ─── Host-migration setup envelope ──────────────────────────────────────────

#[test]
fn migration_setup_payloads_in_two_parts_are_surfaced_both_ways_and_native_play_is_unchanged() {
    use crate::host::migration::wire::MigrationEvent;
    use crate::proto::{ToClientMigration, ToHostMigration};

    let mut net = Net::new(Net::cfg(2, 1));
    let a = net.join("Ann", 2, link(20, 0.0), Pilot::Idle);
    let b = net.join("Bob", 0, link(20, 0.0), Pilot::Idle);
    assert!(net.run_until(5, 2000, |n| n
        .clients
        .iter()
        .all(|c| *c.s.phase() == ClientPhase::Lobby)));
    let lobby = net.clients[a].s.lobby().cloned();

    // Client -> host: a consent over 900 bytes arrives in two parts.
    let consent = ToHostMigration::Consent {
        descriptor: vec![7; 1000],
        addr: "192.168.1.20:40000".into(),
        token: [8; 32],
    };
    assert_eq!(
        crate::host::migration::wire::chunk(&consent).unwrap().len(),
        2
    );
    net.clients[a].s.send_migration(&consent).unwrap();
    net.run_for(5, 200);
    let got = net.host.take_migration_events();
    assert_eq!(got.len(), 1);
    let (peer, event) = got.into_iter().next().unwrap();
    assert_eq!(event, MigrationEvent::Received(consent));
    assert!(net.host.take_migration_events().is_empty());

    // Host -> that client only: cert0 for eight humans, two parts.
    let armed = ToClientMigration::Armed {
        armed: vec![1; 200],
        certificate: vec![2; 1004],
    };
    assert_eq!(
        crate::host::migration::wire::chunk(&armed).unwrap().len(),
        2
    );
    net.host.send_migration(peer, &armed).unwrap();
    net.run_for(5, 200);
    assert_eq!(
        net.clients[a].s.take_migration_events(),
        vec![MigrationEvent::Received(armed)]
    );
    assert!(net.clients[b].s.take_migration_events().is_empty());

    // Nothing was malformed, and the native game did not move.
    assert_eq!(
        (
            net.host.migration_rejected(),
            net.clients[a].s.migration_rejected()
        ),
        (0, 0)
    );
    assert_eq!(net.clients[a].s.lobby().cloned(), lobby);
    assert_eq!(net.host.phase(), HostPhase::Lobby);
    assert_eq!(net.host.human_count(), 2);
    for c in &mut net.clients {
        c.s.set_ready(true);
    }
    assert!(net.run_until(5, 2000, |n| n
        .clients
        .iter()
        .all(|c| *c.s.phase() == ClientPhase::Racing)));
}

mod keep_pass {
    use super::*;
    use crate::proto::{decode, encode, KeepPass, ToClient, PROTO_VERSION};
    use crate::transport::{HostNetEvent, HostTransport};

    const PASS: KeepPass = KeepPass { host_key: [0x5a; 32], secret: [0xa5; 32] };

    #[test]
    fn the_host_sends_a_pass_to_one_seated_peer_and_only_that_client_gets_it() {
        let mut net = Net::new(Net::cfg(0, 1));
        let a = net.join("Ann", 0, link(20, 0.0), Pilot::Idle);
        let b = net.join("Bob", 1, link(20, 0.0), Pilot::Idle);
        net.run_for(5, 500);
        let seat = net.clients[a].s.my_seat().expect("seated");
        let peer = net.host.peer_of_seat(seat).expect("a peer");
        assert!(net.host.send_keep_pass(peer, PASS));
        net.run_for(5, 300);
        assert_eq!(net.clients[a].s.take_keep_passes(), vec![PASS], "the host key and the secret arrive as sent");
        assert!(net.clients[a].s.take_keep_passes().is_empty(), "taken once");
        assert!(net.clients[b].s.take_keep_passes().is_empty(), "the other player gets none");
        assert_eq!(net.clients[a].s.phase(), &ClientPhase::Lobby, "a pass changes nothing else");
        // Nobody on that peer: nothing is sent.
        assert!(net.host.remove_peer(peer));
        assert!(!net.host.send_keep_pass(peer, PASS));
    }

    /// The `ToClient` variants an app built before `KeepPass` knew (the first
    /// ten, payloads kept the same shape), decoded the way it decodes.
    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    enum OldToClient {
        Welcome { proto: u32, seat: u8 },
        Lobby(crate::proto::LobbyView),
        RaceStart { config: race_core::RaceConfig, custom: Option<u64>, start_host_ms: u64, you: u8, race: u8 },
        TrackData { hash: u64, idx: u8, total: u8, bytes: Vec<u8> },
        Events(Vec<race_core::Event>),
        Results(crate::proto::ResultsView),
        Rematch(crate::proto::RematchMsg),
        Pong { id: u32, client_t: u64, host_t: u64 },
        Closed(String),
        Migration(crate::proto::MigrationPart),
    }

    #[test]
    fn an_older_app_cannot_read_a_pass_and_drops_it_so_the_protocol_version_stays_put() {
        // KeepPass itself needed no bump (12 is the Rose and the cake queue).
        assert_eq!(PROTO_VERSION, 12, "a bump would make every older app refuse the host; KeepPass is an appended variant instead");
        let bytes = encode(&ToClient::KeepPass(PASS));
        assert_eq!(bytes[0], 10);
        assert!(decode::<OldToClient>(&bytes).is_none(), "an older app's decode fails, and it ignores a message that fails to decode");
        // The messages it does know still decode the same way.
        let welcome = encode(&ToClient::Welcome { proto: PROTO_VERSION, seat: 3 });
        assert!(matches!(decode::<OldToClient>(&welcome), Some(OldToClient::Welcome { seat: 3, .. })));
    }

    #[test]
    fn an_older_app_cannot_read_a_left_notice_and_drops_it() {
        let bytes = encode(&ToClient::Left { racer: 3, finished: true });
        assert_eq!(bytes[0], 11);
        assert!(decode::<OldToClient>(&bytes).is_none(), "an older app ignores a message that fails to decode");
    }

    #[test]
    fn a_session_that_cannot_decode_a_message_stays_in_the_room_and_reads_the_next_one() {
        // What an older app does with a message it does not know. The index is one past
        // the last variant (`Left`, the newest), so it is unknown to this build too; the
        // assert below proves it rather than trusting a number.
        let unknown = encode(&ToClient::Left { racer: 0, finished: false })[0] + 1;
        assert!(decode::<ToClient>(&[unknown, 1, 2, 3]).is_none(), "index {unknown} must be beyond the last variant");
        let (clock, t) = LoopClock::manual();
        t.store(100_000, Ordering::Relaxed);
        let hub = LoopbackHub::new(clock, 1);
        let mut h = hub.host();
        let mut s = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Me", 0);
        let mut peer = None;
        for step in 0..40u64 {
            let now = 100_000 + step * 20;
            t.store(now, Ordering::Relaxed);
            s.update(now, Input::default());
            while let Some(ev) = h.poll() {
                match ev {
                    HostNetEvent::Joined { peer: p, .. } => peer = Some(p),
                    HostNetEvent::Reliable { peer: p, bytes } => {
                        if matches!(decode::<crate::proto::ToHost>(&bytes), Some(crate::proto::ToHost::Hello { .. })) {
                            h.send_reliable(p, encode(&ToClient::Welcome { proto: PROTO_VERSION, seat: 0 }));
                            h.send_reliable(p, vec![unknown, 1, 2, 3]);
                            h.send_reliable(p, encode(&ToClient::Closed("later message read".into())));
                        }
                    }
                    _ => {}
                }
            }
        }
        assert!(peer.is_some());
        assert_eq!(s.phase(), &ClientPhase::Closed("later message read".into()), "the unknown message did not break the link or the ones after it");
    }
}
