//! Native test host and CPU benchmark. No graphics, local player or real Signet keys.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use decented_net::migration::SessionImage;
use decented_net::{Identity, NetConfig};
use nostr::Keys;
use race_core::{Input, RacePhase};
use race_session::migration::{
    ArmingState, HostArming, HostSwitch, RunnerClock, RuntimeEvent, SwitchState,
};
use race_session::net::AdmissionGate;
use race_session::{
    migration, Admission, InviteId, ClientPhase, ClientSession, HostAuthority, HostConfig, HostNetEvent, HostPhase,
    HostTransport, NullHostTransport, PeerKey, QuicClient, QuicHost,
};
use serde_json::json;

#[path = "stampede-headless/invites.rs"]
mod invites;
#[path = "stampede-headless/keep.rs"]
mod keep;
#[path = "stampede-headless/owner.rs"]
mod owner;
#[path = "stampede-headless/status.rs"]
mod status;
#[path = "stampede-headless/wire.rs"]
mod wire;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Bench,
    Smoke,
    Lan,
    Migration,
}

struct Args {
    mode: Mode,
    bots: u8,
    players: usize,
    rooms: usize,
    seconds: f64,
    seed: u64,
    laps: u8,
    name: String,
    lan_port: u16,
    lan_port_set: bool,
    test_identity: bool,
    bots_set: bool,
    data_dir: Option<PathBuf>,
    standby: Option<String>,
    lobby_seconds: u64,
    /// `--online`: host the room through the relays (and STUN) as well as nearby.
    online: bool,
    /// `--relay`, in order; empty = the default relay list.
    relays: Vec<String>,
    quic_port: u16,
    advertise: Vec<SocketAddr>,
    advertise_only: bool,
    /// `--status`: where the status page listens.
    status: Option<SocketAddr>,
    /// `--status-host`: a name (besides IP addresses and `localhost`) the status page answers to.
    status_host: Option<String>,
    players_set: bool,
    help: bool,
}

const HELP: &str = "stampede-headless --bench [--bots 8] [--rooms N] [--seconds 60] [--seed 42] [--laps 3]
stampede-headless --smoke [--players 8]
stampede-headless --lan --test-identity [--name NAME] [--players N] [--bots N] [--seconds 60] [--lan-port PORT]
stampede-headless --online --test-identity [--data-dir DIR] [--relay wss://URL]... [--quic-port PORT] [--advertise IP:PORT]... [--advertise-only] [--status ADDR:PORT [--status-host NAME]] [--name NAME] [--players N] [--bots N] [--seconds 0] [--lan-port PORT]
stampede-headless --migration-host --test-identity --data-dir DIR --standby PUBKEY_HEX|any [--lobby-seconds 30] [--lan-port PORT] [--players 8] [--bots 0] [--laps 3] [--seconds 60]

--online is a --lan room that is also open to phones anywhere by its 6-letter room code, a Keep.
  The room stays open between races: 30 s after the results, or as soon as everyone has left,
  it returns to the lobby so the code can be joined again.
  --relay URL        a wss:// relay for the room code (repeatable; default: the app's relay list)
  --quic-port PORT   the UDP port the room listens on (0 = any); set it to a forwarded port
  --advertise IP:PORT  also announce this address, e.g. a router or VPN port forward (repeatable)
  --advertise-only   announce only the --advertise addresses (no LAN, STUN, UPnP or IPv6 discovery,
                     no nearby beacon)
                     STAMPEDE_ADVERTISE=IP:PORT[,IP:PORT] in the environment adds to --advertise
  --players N        start a race once N humans are in and every human is ready (bots fill the
                     grid); default 1 with --online, 8 with --lan
  --seconds 0        run until stopped (SIGTERM or SIGINT stop it cleanly)
  --status ADDR:PORT serve a status page and /status.json there (off without it). The page shows
                     the room code to anyone who can reach it, and a button that replaces the code,
                     so bind it to a home-network address, never a public one. It answers only
                     requests addressed to an IP address or localhost (a web page cannot reach it
                     under another name), and to --status-host NAME if given.
  --status-host NAME a name the status page may be reached by, e.g. keep.home.arpa
  --data-dir DIR     keep the Keep's identity and its invites in DIR (files private to the owner).
                     The identity then survives restarts, which invite links need: they name the
                     Keep by its key. Without it the identity is new each start and there are no
                     invites. With it and --online the status page offers \"New guest pass\" (for
                     the first person who uses it, 24 h) and \"New personal invite\" (one person's npub, 1-365 days, default
                     30), each a link of the form stampede://keep/<Keep key>/<secret> to send
                     (the page shows it after you make it; /status.json lists the live ones), and
                     Revoke. A guest pass works for the first person who uses it, until it expires.
                     With --online and a data dir the Keep can also be claimed once from My Signet:
                     while it has no owner it prints a pairing link (once, on stderr, at start) and
                     shows it on the status page to requests from this machine. The owner then makes
                     and cancels invites in My Signet (gift-wrapped messages over the --relay list,
                     at most three relays) and the page makes none. Reset the owner by deleting
                     owner.json in the data dir.
                     A data dir whose invites.json or key files cannot be read does not stop the
                     Keep: invites are off for that run (the page says why) and the files are left
                     as they are. The room code keeps working too.";

fn args() -> Result<Args, String> {
    let result = parse_args(std::env::args().skip(1), std::env::var(ADVERTISE_ENV).ok())?;
    if result.help {
        println!("{HELP}");
        std::process::exit(0);
    }
    Ok(result)
}

/// The most relays `--relay` takes (the app's own cap).
const MAX_RELAYS: usize = 32;

/// A `--relay` value: `wss://host[:port][/path]`, nothing else. Plain `ws://`
/// would send room codes in the clear. Mirrors the app's relay list rules
/// (`clean_relays` in app/src/platform/data_dir.rs), but refuses what the app
/// would drop quietly.
fn parse_relay(value: &str) -> Result<String, String> {
    let bad = || format!("Invalid value for --relay: {value} (use wss://relay.example.com)");
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad());
    }
    let (scheme, rest) = value.split_once("://").ok_or_else(bad)?;
    if !scheme.eq_ignore_ascii_case("wss") {
        return Err(format!("Relays must use wss:// (encrypted): {value}"));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.starts_with(':') || host.len() > 253 {
        return Err(bad());
    }
    Ok(value.to_string())
}

/// Extra advertised addresses, comma-separated IP:PORT, added to `--advertise`. It lets a
/// node's app manifest fill in the host's address at install time, which plain arguments can't.
const ADVERTISE_ENV: &str = "STAMPEDE_ADVERTISE";

fn parse_args(
    input: impl IntoIterator<Item = String>,
    advertise_env: Option<String>,
) -> Result<Args, String> {
    let mut result = Args {
        mode: Mode::Bench,
        bots: 8,
        players: 8,
        rooms: 1,
        seconds: 60.0,
        seed: 42,
        laps: 3,
        name: "Fray test host".into(),
        lan_port: decented_net::DEFAULT_LAN_PORT,
        lan_port_set: false,
        test_identity: false,
        bots_set: false,
        data_dir: None,
        standby: None,
        lobby_seconds: 30,
        online: false,
        relays: Vec::new(),
        quic_port: 0,
        advertise: Vec::new(),
        advertise_only: false,
        status: None,
        status_host: None,
        players_set: false,
        help: false,
    };
    let mut mode = None;
    let mut input = input.into_iter();
    while let Some(flag) = input.next() {
        let value = match flag.as_str() {
            "--bench" | "--smoke" | "--lan" | "--migration-host" => {
                if mode.is_some() {
                    return Err("Choose one of --bench, --smoke, --lan or --migration-host".into());
                }
                mode = Some(match flag.as_str() {
                    "--bench" => Mode::Bench,
                    "--smoke" => Mode::Smoke,
                    "--lan" => Mode::Lan,
                    _ => Mode::Migration,
                });
                continue;
            }
            "--test-identity" => {
                result.test_identity = true;
                continue;
            }
            "--online" => {
                result.online = true;
                continue;
            }
            "--advertise-only" => {
                result.advertise_only = true;
                continue;
            }
            "--help" | "-h" => {
                result.help = true;
                return Ok(result);
            }
            "--bots" | "--players" | "--rooms" | "--seconds" | "--seed" | "--laps" | "--name"
            | "--lan-port" | "--data-dir" | "--standby" | "--lobby-seconds" | "--relay"
            | "--quic-port" | "--advertise" | "--status" | "--status-host" => input
                .next()
                .ok_or_else(|| format!("Missing value for {flag}"))?,
            _ => return Err(format!("Unknown argument: {flag}")),
        };
        let invalid = || format!("Invalid value for {flag}");
        match flag.as_str() {
            "--bots" => {
                result.bots = value.parse().map_err(|_| invalid())?;
                result.bots_set = true;
            }
            "--players" => {
                result.players = value.parse().map_err(|_| invalid())?;
                result.players_set = true;
            }
            "--rooms" => result.rooms = value.parse().map_err(|_| invalid())?,
            "--seconds" => result.seconds = value.parse().map_err(|_| invalid())?,
            "--seed" => result.seed = value.parse().map_err(|_| invalid())?,
            "--laps" => result.laps = value.parse().map_err(|_| invalid())?,
            "--name" => result.name = value,
            "--lan-port" => {
                result.lan_port = value.parse().map_err(|_| invalid())?;
                result.lan_port_set = true;
            }
            "--data-dir" => result.data_dir = Some(value.into()),
            "--standby" => result.standby = Some(value),
            "--lobby-seconds" => result.lobby_seconds = value.parse().map_err(|_| invalid())?,
            "--relay" => {
                let relay = parse_relay(&value)?;
                if !result.relays.contains(&relay) {
                    if result.relays.len() == MAX_RELAYS {
                        return Err(format!("Use at most {MAX_RELAYS} relays"));
                    }
                    result.relays.push(relay);
                }
            }
            "--quic-port" => result.quic_port = value.parse().map_err(|_| invalid())?,
            "--advertise" => {
                let address: SocketAddr = value.parse().map_err(|_| invalid())?;
                if !result.advertise.contains(&address) {
                    result.advertise.push(address);
                }
            }
            "--status" => result.status = Some(value.parse().map_err(|_| invalid())?),
            "--status-host" => {
                let name = value.to_ascii_lowercase();
                let valid = !name.is_empty()
                    && name.len() <= 253
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
                if !valid {
                    return Err(invalid());
                }
                result.status_host = Some(name);
            }
            _ => unreachable!(),
        }
    }
    for value in advertise_env.iter().flat_map(|v| v.split(',')).map(str::trim) {
        if value.is_empty() {
            continue;
        }
        let address: SocketAddr =
            value.parse().map_err(|_| format!("Invalid value in {ADVERTISE_ENV}: {value}"))?;
        if !result.advertise.contains(&address) {
            result.advertise.push(address);
        }
    }
    result.mode = match (mode, result.online) {
        (None, true) | (Some(Mode::Lan), true) => Mode::Lan,
        (Some(_), true) => return Err("--online hosts a room: use it alone or with --lan".into()),
        (Some(mode), false) => mode,
        (None, false) => {
            return Err(
                "Choose --bench, --smoke, --lan, --online or --migration-host (see --help)".into(),
            )
        }
    };
    if !result.online
        && (!result.relays.is_empty() || !result.advertise.is_empty() || result.advertise_only)
    {
        return Err("--relay, --advertise and --advertise-only need --online".into());
    }
    if result.status_host.is_some() && result.status.is_none() {
        return Err("--status-host needs --status".into());
    }
    // An online room is for phones that come and go: one ready human is enough.
    if result.online && !result.players_set {
        result.players = 1;
    }
    if result.advertise_only && result.advertise.is_empty() {
        return Err("--advertise-only needs at least one --advertise".into());
    }
    if result.mode != Mode::Lan && (result.quic_port != 0 || result.status.is_some()) {
        return Err("--quic-port and --status need --lan or --online".into());
    }
    if result.mode == Mode::Migration && !result.bots_set {
        result.bots = 0;
    }
    if result.bots > 8
        || !(1..=8).contains(&result.players)
        || !(1..=64).contains(&result.rooms)
        || !(1..=5).contains(&result.laps)
        || !result.seconds.is_finite()
        || result.seconds < 0.0
        || (result.seconds == 0.0 && result.mode != Mode::Lan)
        || result.seconds > 3600.0
    {
        return Err("Use bots 0–8, players 1–8, rooms 1–64, laps 1–5 and seconds greater than 0 and at most 3600 (0 = until stopped, with --lan or --online)".into());
    }
    if result.mode == Mode::Bench && result.bots == 0 {
        return Err("The benchmark needs at least one bot".into());
    }
    if result.mode == Mode::Lan && !result.test_identity {
        return Err(
            "LAN and online test hosting require --test-identity; this is not a production Keep service"
                .into(),
        );
    }
    if result.mode == Mode::Migration {
        if !result.test_identity || result.data_dir.is_none() || result.standby.is_none() {
            return Err(
                "Migration test hosting requires --test-identity, --data-dir and --standby".into(),
            );
        }
        if result.players < 2 || result.players + result.bots as usize > 8 {
            return Err("Migration test hosting needs 2–8 players and at most 8 seats".into());
        }
    }
    Ok(result)
}

fn config(args: &Args, room: usize) -> HostConfig {
    HostConfig {
        bots: args.bots,
        laps: args.laps,
        seed: args.seed.wrapping_add(room as u64),
        auto_start: false,
        ..HostConfig::default()
    }
}

fn memory_bytes(key: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name != key {
                return None;
            }
            value
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|kib| kib * 1024)
        })
}

#[derive(Default)]
struct Samples {
    seen: u64,
    ns: Vec<u64>,
}

impl Samples {
    fn record(&mut self, ns: u64) {
        self.seen += 1;
        const LIMIT: usize = 16384;
        if self.ns.len() < LIMIT {
            self.ns.push(ns);
        } else {
            let index = race_core::math::mix64(self.seen) % self.seen;
            if index < LIMIT as u64 {
                self.ns[index as usize] = ns;
            }
        }
    }
    fn percentile_ms(&mut self, percent: usize) -> f64 {
        self.ns.sort_unstable();
        let index = (self.ns.len() * percent).div_ceil(100).saturating_sub(1);
        self.ns.get(index).copied().unwrap_or(0) as f64 / 1_000_000.0
    }
}

fn bench(args: &Args) -> Result<(), String> {
    let rss_before = memory_bytes("VmRSS");
    let mut hosts: Vec<_> = (0..args.rooms)
        .map(|room| HostAuthority::new_headless(Box::new(NullHostTransport), config(args, room)))
        .collect();
    for host in &mut hosts {
        if !host.start_race(0) {
            return Err("Could not start bot race".into());
        }
    }
    let mut samples = Samples::default();
    let mut bursts = Samples::default();
    let mut races = vec![0u64; args.rooms];
    let mut stopped = vec![false; args.rooms];
    let mut race_start = vec![0u64; args.rooms];
    let mut finished_racers = 0usize;
    let mut step = 0u64;
    let mut ticks = 0u64;
    let mut warmup_ticks = 0u64;
    let start = Instant::now();
    let warmup = Duration::from_millis(250);
    let target = Duration::from_secs_f64(args.seconds);
    while stopped.iter().any(|done| !done) {
        step += 1;
        let now = (step * 1000).div_ceil(race_core::math::TICK_HZ as u64);
        let measured = start.elapsed() >= warmup;
        let stop_after_race = start.elapsed() >= target + warmup;
        let burst_start = Instant::now();
        let mut advanced = false;
        for (room, host) in hosts.iter_mut().enumerate() {
            if stopped[room] {
                continue;
            }
            if now.saturating_sub(race_start[room]) > 600_000 {
                return Err(format!(
                    "Room {room}: bot race exceeded ten minutes of game time"
                ));
            }
            let before = host.stats().ticks;
            let tick_start = Instant::now();
            host.update(now);
            let elapsed = tick_start.elapsed();
            let count = host.stats().ticks - before;
            ticks += count;
            if count != 0 {
                if measured {
                    samples.record((elapsed.as_nanos() / count as u128) as u64);
                    advanced = true;
                } else {
                    warmup_ticks += count;
                }
            }
            if host.phase() == HostPhase::Results {
                let world = host.world().ok_or("Missing completed world")?;
                if world.phase != RacePhase::Finished
                    || world.racers.len() != args.bots as usize
                    || host.human_count() != 0
                    || world.racers.iter().any(|r| r.finished_at.is_none())
                {
                    return Err("Bot race did not complete with the expected seats".into());
                }
                finished_racers = world.racers.len();
                races[room] += 1;
                if stop_after_race {
                    stopped[room] = true;
                } else {
                    host.to_lobby();
                    if !host.start_race(now) {
                        return Err("Could not restart bot race".into());
                    }
                    race_start[room] = now;
                }
            }
        }
        if measured && advanced {
            bursts.record(burst_start.elapsed().as_nanos() as u64);
        }
    }
    let rss_after = memory_bytes("VmRSS");
    println!(
        "{}",
        json!({"event":"bench", "completed":true, "rooms":args.rooms,
        "racers_per_room":finished_racers, "human_players":0, "completed_races":races,
        "ticks":ticks, "warmup_ticks":warmup_ticks, "wall_seconds":start.elapsed().as_secs_f64(),
        "tick_cost_p50_ms":samples.percentile_ms(50), "tick_cost_p99_ms":samples.percentile_ms(99),
        "room_burst_p50_ms":bursts.percentile_ms(50), "room_burst_p99_ms":bursts.percentile_ms(99),
        "rss_bytes":rss_after, "peak_rss_bytes":memory_bytes("VmHWM"),
        "rss_growth_bytes":rss_before.zip(rss_after).map(|(before,after)|after.saturating_sub(before)),
        "tick_budget_ms":1000.0 / race_core::math::TICK_HZ as f64,
        "measurement":"native host update cost per advanced tick; virtual clock; no network clients; bounded reservoir samples"})
    );
    Ok(())
}

fn signed_test_identity() -> Result<Identity, String> {
    keep::sign_identity(Identity::ephemeral(), &Keys::generate())
}

/// The room's listener, shared by the authority and the migration
/// coordinators, and the peers that ever joined it.
#[derive(Clone)]
struct NetworkHost(Arc<Mutex<QuicHost>>, Arc<Mutex<BTreeSet<PeerKey>>>);

impl NetworkHost {
    fn new(host: QuicHost) -> Self {
        Self(Arc::new(Mutex::new(host)), Arc::default())
    }
}

impl HostTransport for NetworkHost {
    fn poll(&mut self) -> Option<HostNetEvent> {
        let event = self.0.lock().unwrap().poll()?;
        if let HostNetEvent::Joined { peer, .. } = &event {
            self.1.lock().unwrap().insert(*peer);
        }
        Some(event)
    }
    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_reliable(peer, bytes);
    }
    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_lossy(peer, bytes);
    }
    fn kick(&mut self, peer: PeerKey) {
        self.0.lock().unwrap().kick(peer);
    }
    fn disconnect(&mut self, peer: PeerKey) {
        self.0.lock().unwrap().disconnect(peer);
    }
    fn confirm_timeout(&mut self, peer: PeerKey) {
        self.0.lock().unwrap().confirm_timeout(peer);
    }
    fn set_open(&mut self, open: bool) {
        self.0.lock().unwrap().set_open(open);
    }
    fn set_open_keeping_token(&mut self, open: bool) {
        self.0.lock().unwrap().set_open_keeping_token(open);
    }
    fn rtt_ms(&self, peer: PeerKey) -> Option<u32> {
        self.0.lock().unwrap().rtt_ms(peer)
    }
    fn shutdown(&mut self) {
        self.0.lock().unwrap().shutdown();
    }
}

/// The room's network settings: nearby only (no relays, no STUN) unless
/// `--online`, then the relays and STUN as the app has them, with the
/// `--relay`, `--quic-port` and `--advertise` choices.
fn room_net(args: &Args) -> NetConfig {
    let mut net = race_session::net_config(std::env::temp_dir());
    if args.online {
        if !args.relays.is_empty() {
            // The operator's list is the whole list: no quiet fallback to the app's.
            net.relays = args.relays.clone();
            net.relay_fallback = false;
        }
    } else {
        net = net.lan_only();
    }
    net.quic_port = args.quic_port;
    net.advertise = args.advertise.clone();
    net.advertise_only = args.advertise_only;
    net.host_has_player = false;
    net
}

/// Set by SIGTERM or SIGINT (`--lan` and `--online` rooms): the room says
/// goodbye to its players and the process exits 0.
static STOP: AtomicBool = AtomicBool::new(false);

/// Set when the room's loop ends (a stop signal or `--seconds`), to stop the owner's loop.
static OWNER_STOP: AtomicBool = AtomicBool::new(false);

fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// The owner's loop. However `keep` is left (an early return, an error, the
/// normal end) its thread is told to stop and waited for, so the wrap cursor
/// is saved.
struct OwnerLoop(Option<std::thread::JoinHandle<()>>);

impl OwnerLoop {
    fn finish(&mut self) {
        OWNER_STOP.store(true, Ordering::Relaxed);
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for OwnerLoop {
    fn drop(&mut self) {
        self.finish();
    }
}

/// One hosted room: the listener, the authority over it and what a joiner
/// dials.
struct Room {
    link: NetworkHost,
    host: HostAuthority,
    code: String,
    address: SocketAddr,
    token: String,
}

impl Room {
    /// `(connected, configured)` relays.
    fn relays(&self) -> (usize, usize) {
        let network = self.link.0.lock().unwrap();
        network.handle().map_or((0, 0), |h| h.relays_connected())
    }
}

/// Host a room again after the old one shut down, as the status page's New
/// code asks. A fixed `--quic-port` may take a while to come free (the old
/// router mapping is removed first), so this keeps trying, with a growing
/// pause, and tells the status page why; only a stop signal ends it.
fn reopen_room(
    args: &Args,
    net: &NetConfig,
    identity: &Identity,
    now: &dyn Fn() -> u64,
    shared: &status::Shared,
    invites: Option<&Arc<invites::Invites>>,
) -> Option<Room> {
    keep::retry(
        || open_room(args, net, identity, now, invites),
        |reason, attempt| {
            emit(json!({"event":"reopen_failed","attempt":attempt,"reason":reason}));
            let mut trying = shared.snapshot();
            trying.restarting = true;
            trying.error = Some(reason.to_string());
            shared.set(trying);
        },
        |pause| {
            let end = Instant::now() + pause;
            while Instant::now() < end && !stopped() {
                std::thread::sleep(Duration::from_millis(50));
            }
        },
        stopped,
    )
}

/// What the status page shows of a room.
fn snapshot(
    args: &Args,
    room: &Room,
    code_ready: bool,
    relays: (usize, usize),
    races: u64,
) -> status::Snapshot {
    let host = &room.host;
    let mut addresses = keep::announced();
    if addresses.is_empty() {
        addresses = args.advertise.iter().map(|a| ("advertise".to_string(), a.to_string())).collect();
    }
    status::Snapshot {
        code: Some(room.code.clone()),
        online: args.online,
        code_ready: args.online && code_ready,
        restarting: false,
        error: None,
        phase: format!("{:?}", host.phase()).to_lowercase(),
        open: room.link.0.lock().unwrap().handle().is_some_and(|h| h.is_open()),
        players: host.lobby_view().seats.into_iter().filter(|s| !s.bot).map(|s| s.name).collect(),
        races,
        relays,
        addresses,
        invites: None,
        invites_problem: None,
        now: 0,
        ..status::Snapshot::default()
    }
}

/// Host a room and wait until it listens.
fn open_room(
    args: &Args,
    net: &NetConfig,
    identity: &Identity,
    now: &dyn Fn() -> u64,
    invites: Option<&Arc<invites::Invites>>,
) -> Result<Room, String> {
    keep::forget_announced();
    let link = NetworkHost::new(race_session::host_room(net, identity, &args.name));
    if let Some(invites) = invites {
        link.0.lock().unwrap().set_admission_gate(invite_gate(invites.clone()));
    }
    let mut host = HostAuthority::new_headless(Box::new(link.clone()), config(args, 0));
    let (address, token, code) = listening(&mut host, &link, now)?;
    Ok(Room {
        link,
        host,
        code,
        address,
        token,
    })
}

/// How often the Keep looks at its invites even if nothing changed (expiry).
const INVITE_SWEEP: Duration = Duration::from_secs(30);

/// Applies the Keep's invite rules to a joiner Fray admitted by invite; every
/// other way in (the room code, the LAN token) passes. A refused peer is disconnected
/// by the host before the game sees it. Logs a JSON line either way.
fn invite_gate(invites: Arc<invites::Invites>) -> AdmissionGate {
    Box::new(move |join| {
        let Admission::Invite(id) = join.admitted_by else {
            return Ok(());
        };
        let persona = hex(&join.persona);
        match invites.admit(id, &join.persona, keep::unix_now()) {
            Ok(admitted) => {
                emit(json!({"event":"invite_admitted","invite":id.to_string(),"kind":admitted.kind.as_str(),
                    "label":admitted.label,"persona":persona,"uses":admitted.uses,"exhausted":admitted.exhausted}));
                Ok(())
            }
            Err(why) => {
                emit(json!({"event":"invite_refused","invite":id.to_string(),"reason":why.code(),"persona":persona}));
                Err(why.reason().to_string())
            }
        }
    })
}

/// Make the room's host hold exactly the live invites (adding the new ones,
/// including all of them on a fresh room, removing used-up, expired and revoked
/// ones), then drop the players a revoked invite let in.
fn sync_invites(room: &mut Room, invites: &invites::Invites, held: &mut BTreeSet<InviteId>) {
    let (add, remove) = invites::plan(held, &invites.live(keep::unix_now()));
    let revoked = invites.take_revoked();
    {
        let network = room.link.0.lock().unwrap();
        let Some(handle) = network.handle() else { return };
        for id in remove {
            handle.remove_invite(id);
            held.remove(&id);
        }
        for (id, secret) in add {
            match handle.add_invite(secret) {
                Ok(_) => {
                    held.insert(id);
                }
                Err(reason) => emit(json!({"event":"invite_add_failed","invite":id.to_string(),"reason":reason})),
            }
        }
    }
    for id in revoked {
        let peers = room.link.0.lock().unwrap().peers_admitted_by(id);
        for peer in peers {
            emit(json!({"event":"invite_kick","invite":id.to_string(),"reason":"revoked"}));
            // A seated player is told why; one not seated yet (or while the lobby is
            // frozen) just loses the link.
            if !room.host.remove_peer(peer) {
                room.link.clone().disconnect(peer);
            }
        }
    }
}

fn network(args: &Args) -> Result<(), String> {
    let mut net = room_net(args);
    net.lan_port = if args.mode == Mode::Smoke {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| e.to_string())?
            .local_addr()
            .map_err(|e| e.to_string())?
            .port()
    } else {
        args.lan_port
    };
    // A Keep with a data dir keeps its identity between runs, so the invite
    // links that pin its key keep working. If a key file there is damaged the
    // Keep still runs, with a temporary key and no invites: room-code play does
    // not depend on the files, and the damaged one is never overwritten.
    let (identity, data_problem) = match (&args.data_dir, args.mode) {
        (Some(dir), Mode::Lan) => {
            keep::ensure_private_dir(dir)?;
            match keep::persistent_test_identity(dir) {
                Ok(identity) => (identity, None),
                Err(problem) => (
                    signed_test_identity()?,
                    Some(format!("{problem} (this run uses a temporary key)")),
                ),
            }
        }
        _ => (signed_test_identity()?, None),
    };
    if args.mode == Mode::Smoke {
        let expected_host = identity.pubkey().to_hex();
        let start = Instant::now();
        let mut room = open_room(args, &net, &identity, &|| start.elapsed().as_millis() as u64, None)?;
        let result = smoke(
            args,
            &net,
            room.address,
            &room.token,
            &expected_host,
            &mut room.host,
        );
        room.host.shutdown();
        return result;
    }
    keep(args, &net, &identity, data_problem)
}

/// The invite table for a Keep with `--data-dir`, or why there is none: a
/// problem with the data dir's key files (`data_problem`) or with
/// `invites.json` turns invites off for this run, with the reason to show.
fn open_invites(
    data_dir: Option<&std::path::Path>,
    online: bool,
    host_key: [u8; 32],
    data_problem: Option<String>,
) -> (Option<Arc<invites::Invites>>, Option<String>) {
    let Some(dir) = data_dir else { return (None, None) };
    if let Some(problem) = data_problem {
        return (None, Some(problem));
    }
    if !online {
        return (None, None);
    }
    match invites::Invites::open(dir, host_key) {
        Ok(table) => (Some(Arc::new(table)), None),
        Err(problem) => (None, Some(problem)),
    }
}

/// `--lan` and `--online`: a room that starts a race when `--players` humans
/// are ready, and stays open between races, until `--seconds` or a stop signal.
fn keep(args: &Args, net: &NetConfig, identity: &Identity, data_problem: Option<String>) -> Result<(), String> {
    ctrlc::set_handler(|| STOP.store(true, Ordering::Relaxed))
        .map_err(|e| format!("Cannot watch for SIGTERM and SIGINT: {e}"))?;
    keep::capture_announced();
    // Invites need the relays and somewhere to keep the table.
    let (invites, invites_problem) = open_invites(
        args.data_dir.as_deref(),
        args.online,
        identity.pubkey().to_bytes(),
        data_problem,
    );
    if let Some(problem) = &invites_problem {
        log::warn!("[keep] invites are off: {problem}");
        emit(json!({"event":"invites_off","reason":problem}));
    }
    // The owner: a one-time claim from My Signet, then its invites and revokes.
    let mut owner = None;
    let mut owner_problem = None;
    if let (Some(invites), Some(dir)) = (&invites, &args.data_dir) {
        let opened = keep::host_keys(dir, identity)
            .map_err(owner::OpenError::Other)
            .and_then(|keys| owner::Owner::open(dir, keys, invites.clone(), &args.name, &net.relays));
        match opened {
            Ok(opened) => owner = Some(Arc::new(opened)),
            Err(problem) => {
                let reason = problem.message().to_string();
                log::warn!("[keep] owner messages are off: {reason}");
                emit(json!({"event":"owner_off","reason":reason}));
                // Owner files that cannot be read, or a Keep that has an owner,
                // shut the page's invites; a Keep nobody has claimed is not
                // shut by a missing relay or key.
                if problem.fails_closed(dir) {
                    owner_problem = Some(reason);
                }
            }
        }
    }
    let mut owner_loop = OwnerLoop(owner.as_ref().and_then(|owner| {
        match owner.pairing_link() {
            Some(link) => {
                emit(json!({"event":"owner","state":"unclaimed"}));
                keep::print_pairing_link(&link);
            }
            None => emit(json!({"event":"owner","state":"claimed"})),
        }
        // One connection to each relay of the pairing link: the Keep listens on
        // all of them and answers on every one, not only the healthiest.
        let relays = owner.connect_relays();
        let owner = owner.clone();
        std::thread::Builder::new()
            .name("owner".into())
            .spawn(move || owner.run(relays, || stopped() || OWNER_STOP.load(Ordering::Relaxed)))
            .ok()
    }));
    // The page is up while the room starts and while it waits for a relay.
    let mut page = status::Shared::new(status::Snapshot {
        online: args.online,
        restarting: true,
        phase: "lobby".into(),
        ..status::Snapshot::default()
    }, args.status_host.as_deref());
    if let Some(invites) = &invites {
        page = page.with_invites(invites.clone());
    }
    if let Some(problem) = invites_problem {
        page = page.with_invites_problem(problem);
    }
    if let Some(owner) = &owner {
        page = page.with_owner(owner.clone());
    }
    if let Some(problem) = owner_problem {
        page = page.with_owner_problem(problem);
    }
    let shared = Arc::new(page);
    if let Some(address) = args.status {
        let listener = std::net::TcpListener::bind(address)
            .map_err(|e| format!("Cannot serve the status page on {address}: {e}"))?;
        let bound = listener.local_addr().map_err(|e| e.to_string())?;
        status::spawn(listener, shared.clone())
            .map_err(|e| format!("Cannot serve the status page on {address}: {e}"))?;
        emit(json!({"event":"status_page","addr":bound.to_string()}));
    }
    let start = Instant::now();
    let clock = || start.elapsed().as_millis() as u64;
    let mut room = match open_room(args, net, identity, &clock, invites.as_ref()) {
        Ok(room) => room,
        Err(_) if stopped() => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut grace = keep::RelayGrace::default();
    let mut back = keep::LobbyReturn::default();
    let mut ready_sent = false;
    let mut completed = 0u64;
    let mut last_phase = room.host.phase();
    let mut last_status = Instant::now();
    let mut last_snapshot = Instant::now() - Duration::from_secs(1);
    // The invites Fray holds for this room, and the seats already named.
    let mut held: BTreeSet<InviteId> = BTreeSet::new();
    let mut seated: BTreeSet<u8> = BTreeSet::new();
    let mut last_sweep = Instant::now();
    while !stopped() && (args.seconds == 0.0 || start.elapsed().as_secs_f64() < args.seconds) {
        if shared.take_new_code() {
            // A new code is a new room: the old one says goodbye first.
            emit(new_code_event(args, &room.code, room.host.human_count()));
            room.host.shutdown();
            room = match reopen_room(args, net, identity, &clock, &shared, invites.as_ref()) {
                Some(room) => room,
                None => {
                    emit(json!({"event":"stopped","completed_races":completed}));
                    return Ok(());
                }
            };
            grace = keep::RelayGrace::default();
            back = keep::LobbyReturn::default();
            ready_sent = false;
            last_phase = room.host.phase();
            last_snapshot = Instant::now() - Duration::from_secs(1);
            // The new room holds no invites yet: add them all again.
            held.clear();
            seated.clear();
            if let Some(invites) = &invites {
                invites.mark_dirty();
            }
        }
        let now = clock();
        room.host.update(now);
        if let Some(invites) = &invites {
            // The expiry sweep rides on a 30 s timer; any change is seen at once.
            if invites.take_dirty() || last_sweep.elapsed() >= INVITE_SWEEP {
                last_sweep = Instant::now();
                sync_invites(&mut room, invites, &mut held);
            }
            let seats: Vec<(u8, String)> = room
                .host
                .lobby_view()
                .seats
                .into_iter()
                .filter(|s| !s.bot)
                .map(|s| (s.id, s.name))
                .collect();
            for (id, name) in &seats {
                if !seated.contains(id) {
                    invites.claim_name(name);
                }
            }
            if seats.len() != seated.len() || seats.iter().any(|(id, _)| !seated.contains(id)) {
                seated = seats.iter().map(|(id, _)| *id).collect();
            }
        }
        let (relays_up, relays_total) = room.relays();
        // A LAN room is ready as soon as it listens; an online one once its code can be joined.
        let code_ready = !args.online || grace.update(relays_up, now);
        if !ready_sent && code_ready {
            ready_sent = true;
            emit(ready_event(args, identity, &room, (relays_up, relays_total)));
        }
        let host = &mut room.host;
        let human_ready: Vec<bool> = host
            .lobby_view()
            .seats
            .iter()
            .filter(|seat| !seat.bot)
            .map(|seat| seat.ready)
            .collect();
        if host.phase() == HostPhase::Lobby
            && keep::may_start(host.human_count(), &human_ready, args.players)
        {
            host.start_race(now);
        } else if host.phase() == HostPhase::Results && last_phase != HostPhase::Results {
            completed += 1;
            emit(
                json!({"event":"race_completed","race":completed,"humans":host.human_count(),"ticks":host.stats().ticks}),
            );
        }
        // The room only takes joiners in the lobby: bring it back.
        let mut reason = "game";
        if let Some(why) = back.update(host.phase(), host.human_count(), now) {
            if host.to_lobby() {
                reason = why;
            }
        }
        if host.phase() != last_phase {
            emit(
                json!({"event":"phase","phase":format!("{:?}",host.phase()),"from":format!("{last_phase:?}"),
                "reason":reason,"humans":host.human_count(),"races":completed}),
            );
            last_phase = host.phase();
        }
        if last_status.elapsed() >= Duration::from_secs(5) {
            last_status = Instant::now();
            let mut line = json!({"event":"status","humans":host.human_count(),
                "phase":format!("{:?}",host.phase()),"ticks":host.stats().ticks,
                "snapshots_sent":host.stats().snapshots_sent,"snapshot_bytes_max":host.stats().snapshot_bytes_max,
                "starved_ticks":host.stats().starved_ticks});
            if args.online {
                line["relays_connected"] = relays_up.into();
                line["relays_total"] = relays_total.into();
            }
            emit(line);
        }
        if last_snapshot.elapsed() >= Duration::from_millis(200) {
            last_snapshot = Instant::now();
            shared.set(snapshot(args, &room, code_ready, (relays_up, relays_total), completed));
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    room.host.shutdown();
    owner_loop.finish();
    emit(
        json!({"event":"stopped","completed_races":completed,"ticks":room.host.stats().ticks,"snapshots_sent":room.host.stats().snapshots_sent,"snapshot_bytes_max":room.host.stats().snapshot_bytes_max}),
    );
    Ok(())
}

/// Whether this run is a Keep: online with a status page, where the owner
/// reads the room code. Its stdout (what `docker logs` shows) never carries
/// the code; a run without a status page keeps it in its events, because
/// test harnesses and phone scripts read it there.
fn code_stays_on_status_page(args: &Args) -> bool {
    args.online && args.status.is_some()
}

/// The `new_code` line: the old room says goodbye, naming its code unless
/// this is a Keep.
fn new_code_event(args: &Args, previous: &str, humans: usize) -> serde_json::Value {
    let mut event = json!({"event":"new_code","humans":humans});
    if !code_stays_on_status_page(args) {
        event["previous"] = previous.into();
    }
    event
}

/// The `ready` line: the room listens and, online, its code can be joined.
fn ready_event(
    args: &Args,
    identity: &Identity,
    room: &Room,
    relays: (usize, usize),
) -> serde_json::Value {
    ready_json(args, &identity.pubkey().to_hex(), &room.code, relays)
}

/// The `ready` line's content. A Keep says `room_code_on_status_page`
/// instead of giving the code.
fn ready_json(args: &Args, host_pubkey: &str, code: &str, relays: (usize, usize)) -> serde_json::Value {
    let mut event = json!({"event":"ready","room_name":args.name,
        "host_pubkey":host_pubkey,"nearby":!args.advertise_only,"online_code":args.online,
        "local_players":0,"max_players":8,"synthetic_identity":true,"seconds":args.seconds});
    if code_stays_on_status_page(args) {
        event["room_code_on_status_page"] = true.into();
    } else {
        event["room_code"] = code.into();
    }
    if args.online {
        let mut candidates = keep::announced();
        if candidates.is_empty() {
            // The listening line was not read: say what was configured.
            candidates = args.advertise.iter().map(|a| ("advertise".to_string(), a.to_string())).collect();
        }
        event["relays_connected"] = relays.0.into();
        event["relays_total"] = relays.1.into();
        event["candidates"] = candidates
            .into_iter()
            .map(|(kind, addr)| json!({"kind":kind,"addr":addr}))
            .collect();
    }
    event
}

/// Wait until the room's listener is ready: its loopback address, LAN token
/// and room code. A stop signal ends the wait.
fn listening(
    host: &mut HostAuthority,
    link: &NetworkHost,
    now: impl Fn() -> u64,
) -> Result<(SocketAddr, String, String), String> {
    let start = Instant::now();
    loop {
        host.update(now());
        {
            let network = link.0.lock().unwrap();
            if let Some(error) = network.failure() {
                return Err(error.to_string());
            }
            if network.is_ready() {
                let handle = network.handle().ok_or("Missing host handle")?;
                let bound = handle.local_addr().ok_or("Missing listening address")?;
                return Ok((
                    SocketAddr::new(Ipv4Addr::LOCALHOST.into(), bound.port()),
                    handle.lan_token(),
                    handle.code().to_string(),
                ));
            }
        }
        if stopped() {
            host.shutdown();
            return Err("Stopped before the room was ready".into());
        }
        if start.elapsed() > Duration::from_secs(10) {
            host.shutdown();
            return Err("Host readiness timed out".into());
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

fn smoke(
    args: &Args,
    net: &NetConfig,
    address: SocketAddr,
    token: &str,
    expected_host: &str,
    host: &mut HostAuthority,
) -> Result<(), String> {
    let mut clients = Vec::new();
    for player in 0..args.players {
        let identity = signed_test_identity()?;
        let handle = decented_net::join_addr(net, &identity, address, token);
        clients.push(ClientSession::new(
            Box::new(QuicClient::new(handle)),
            &format!("Test player {}", player + 1),
            player as u8,
        ));
    }
    let start = Instant::now();
    loop {
        let now = start.elapsed().as_millis() as u64;
        host.update(now);
        for client in &mut clients {
            client.update(now, Input::default());
            if let ClientPhase::Failed(error) = client.phase() {
                return Err(format!("Smoke client failed: {error}"));
            }
            if let Some(identity) = client.host_identity() {
                if identity.pubkey != expected_host {
                    return Err("Smoke client reached the wrong host".into());
                }
            }
        }
        if host.human_count() == args.players
            && clients.iter().all(|client| {
                *client.phase() == ClientPhase::Lobby
                    && client
                        .lobby()
                        .is_some_and(|lobby| lobby.humans() == args.players)
            })
        {
            if host.lobby_view().seats.iter().any(|seat| seat.host) {
                return Err("Headless host occupies a player seat".into());
            }
            println!(
                "{}",
                json!({"event":"smoke","completed":true,"authenticated_clients":clients.len(),
                "human_players":host.human_count(),"host_player_seats":0,"transport":"localhost QUIC",
                "synthetic_identities":true,"wall_seconds":start.elapsed().as_secs_f64()})
            );
            for client in &mut clients {
                client.leave();
            }
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(15) {
            return Err(format!(
                "Smoke timed out with {} clients seated",
                host.human_count()
            ));
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

// ─── host migration test host ──────────────────────────────────────────────

/// One machine-readable stdout line, flushed at once: a test process reads
/// these lines as its process-control barrier.
fn emit(value: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The device's game-facing time, as the app keeps it: held from the frozen
/// lobby through arming and the switch, then continuous (the armed room
/// resumes at the frozen lobby's time).
struct SessionTime {
    runner: RunnerClock,
    hold_from: Option<u64>,
    shift: u64,
}

impl SessionTime {
    fn now(&self) -> u64 {
        let now = self.runner.now_ms();
        self.hold_from.unwrap_or(now).saturating_sub(self.shift)
    }
    fn hold(&mut self) {
        self.hold_from.get_or_insert(self.runner.now_ms());
    }
    fn release(&mut self) {
        if let Some(held) = self.hold_from.take() {
            self.shift += self.runner.now_ms().saturating_sub(held);
        }
    }
    /// Hold back `ms` of game time (what `LivePrimary::pace` capped): the
    /// clock reads that much less from now on and stays continuous.
    fn absorb(&mut self, ms: u64) {
        self.shift += ms;
    }
}

/// `--migration-host`: a headless host (no player seat) that arms the room
/// with the chosen standby, switches every human to protected play, starts
/// the race in the first armed batch and hosts it as the armed original
/// (`LivePrimary` + `PlayerHost`, probing every poll), until it is killed or
/// `--seconds` pass. Its stdout lines are the test's barrier:
/// `migration_ready` (address, token, room code), `armed` (session, every
/// human protected, no host seat), `racing` (the armed race is a second past
/// GO). Synthetic identity; network state under `--data-dir`.
fn migration_host(args: &Args) -> Result<(), String> {
    let dir = args.data_dir.as_ref().ok_or("Missing --data-dir")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // `any`: the first human in the full lobby (phones, whose keys are not known in advance).
    let standby_key = match args.standby.as_deref() {
        Some("any") => None,
        key => Some(
            nostr::PublicKey::from_hex(key.unwrap_or_default())
                .map_err(|_| "Invalid --standby public key")?
                .to_bytes(),
        ),
    };
    let mut net = race_session::net_config(dir).lan_only();
    net.host_has_player = false;
    // An explicit `--lan-port` (the default discovery port) lets phones find the room.
    net.lan_port = if args.lan_port_set {
        args.lan_port
    } else {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| e.to_string())?
            .local_addr()
            .map_err(|e| e.to_string())?
            .port()
    };
    let identity = signed_test_identity()?;
    let link = NetworkHost::new(race_session::host_room(&net, &identity, &args.name));
    let mut host = HostAuthority::new_headless(Box::new(link.clone()), config(args, 0));
    let runner = RunnerClock::start();
    let mut time = SessionTime {
        runner,
        hold_from: None,
        shift: 0,
    };
    let (address, token, code) = listening(&mut host, &link, || runner.now_ms())?;
    emit(
        json!({"event":"migration_ready","addr":address.to_string(),"token":token,
        "room_code":code,"host_pubkey":identity.pubkey().to_hex(),"local_players":0,
        "synthetic_identity":true,"pid":std::process::id()}),
    );
    let deadline = Duration::from_secs(args.lobby_seconds);
    let wait = Instant::now();
    // The lobby: every expected human seated, the standby among them.
    let selected = loop {
        host.update(time.now());
        if host.human_count() == args.players {
            let network = link.0.lock().unwrap();
            let found = link.1.lock().unwrap().iter().copied().find(|&peer| {
                standby_key.is_none_or(|standby| {
                    network
                        .migration_peer_key(peer)
                        .is_ok_and(|key| key == standby)
                })
            });
            if let Some(peer) = found {
                break peer;
            }
        }
        if wait.elapsed() > deadline {
            return Err(format!(
                "Lobby timed out with {} of {} players",
                host.human_count(),
                args.players
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let host_seats = host.lobby_view().seats.iter().filter(|s| s.host).count();
    if host_seats != 0 {
        return Err("Headless host occupies a player seat".into());
    }
    // Arming over the room's own messages, then the switch.
    let mut arming = HostArming::start(
        &mut host,
        &mut link.0.lock().unwrap(),
        &net,
        &identity,
        selected,
    );
    let armed = loop {
        let now = time.now();
        host.update(now);
        let state = arming
            .poll(&mut host, &mut link.0.lock().unwrap(), runner.now_ms())
            .clone();
        if host.migration_frozen() {
            time.hold();
        }
        match state {
            ArmingState::Armed => break arming.take_armed().ok_or("Armed without a room")?,
            ArmingState::Refused { reason } | ArmingState::Unprotected { reason } => {
                return Err(format!("Arming failed: {reason}"))
            }
            _ => {}
        }
        if wait.elapsed() > deadline {
            return Err(format!("Arming timed out in {state:?}"));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let mut switch = HostSwitch::start(
        &mut host,
        &mut link.0.lock().unwrap(),
        armed,
        &identity,
        runner,
        runner.now_ms(),
    );
    let unprotected = loop {
        host.update(time.now());
        switch.poll(&mut host, &mut link.0.lock().unwrap(), runner.now_ms());
        match switch.state().clone() {
            SwitchState::Switching => {}
            SwitchState::Protected { unprotected } => break unprotected,
            SwitchState::Unprotected { reason } => return Err(format!("Switch failed: {reason}")),
        }
        if wait.elapsed() > deadline {
            return Err("Switch timed out".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    time.release();
    let (mut live, mut players) = switch.take_room().ok_or("No armed room")?;
    let root = switch.descriptor().root.clone();
    drop(host);
    let base = SessionImage::decode(&root, live.primary().committed_image())
        .map_err(|e| format!("Armed image: {e:?}"))?
        .logical_ms;
    let names: Vec<String> = unprotected
        .iter()
        .map(|u| format!("{}: {}", u.name, u.reason))
        .collect();
    let host_member = root.members.iter().any(|m| m.key == Some(root.host));
    // A lobby that filled up closed its listener, which rotates the LAN
    // admission token: the token the players joined with no longer admits
    // them, so their re-dial at the switch uses the one `Switch` carried.
    let token_rotated = players
        .network()
        .handle()
        .is_none_or(|h| h.lan_token() != token);
    emit(
        json!({"event":"armed","room_code":code,"session":hex(&root.session),
        "standby":hex(&root.standby),"members":root.members.len(),
        "humans":live.authority().human_count(),"host_seats":host_seats,
        "host_member":host_member,"lan_token_rotated":token_rotated,"all_protected":names.is_empty(),"unprotected":names,
        "base_ms":base}),
    );
    if !names.is_empty() {
        return Err("Not every human is protected".into());
    }
    // The armed original, as the app's host frame drives it.
    let mut start_pending = true;
    let mut racing_commits = 0u64;
    let mut racing_sent = false;
    let mut completed_sent = false;
    let mut paused: Option<String> = None;
    // The soft behind pause (`LivePrimary::behind`): since when, and how
    // many times it was entered.
    let (mut behind_since, mut behind_pauses) = (None::<u64>, 0u64);
    // The longest gap between two armed frames of this process: a stall over
    // the 250 ms game-clock budget soft-pauses the room until commits catch up.
    let (mut last_frame, mut max_gap_ms) = (runner.now_ms(), 0u64);
    // The longest time each step of the frame took: player poll, probe,
    // runtime poll (with publish and certify), stage, flush and sleep.
    let mut step_ms = [0u64; 5];
    let mut lap = Instant::now();
    let mut step = |i: usize, step_ms: &mut [u64; 5]| {
        step_ms[i] = step_ms[i].max(lap.elapsed().as_millis() as u64);
        lap = Instant::now();
    };
    while runner.now_ms() < (args.seconds * 1000.0) as u64 {
        let wall = runner.now_ms();
        max_gap_ms = max_gap_ms.max(wall - last_frame);
        last_frame = wall;
        // Paced to the commits: while they are late the game clock holds at
        // 250 ms past the last one, then runs on from there (no jump).
        let raw = time.now().max(base);
        let game = live.pace(raw);
        time.absorb(raw - game);
        let mut superseded = false;
        step(4, &mut step_ms);
        let mut fault = |e: decented_net::migration::Error| {
            paused.get_or_insert_with(|| format!("{e:?}"));
        };
        if let Err(e) = players.poll(&mut live, wall) {
            fault(e);
        }
        step(0, &mut step_ms);
        if let Err(e) = live.probe() {
            fault(e);
        }
        step(1, &mut step_ms);
        match live.poll() {
            Ok(events) => {
                for event in events {
                    match event {
                        RuntimeEvent::Committed {
                            outputs,
                            selected_views,
                            ..
                        } => {
                            if let Err(e) = players.publish(&live, &outputs, &selected_views, wall)
                            {
                                fault(e);
                            }
                            if live
                                .authority()
                                .world()
                                .is_some_and(|w| w.phase == RacePhase::Racing)
                            {
                                racing_commits += 1;
                            }
                        }
                        RuntimeEvent::Membership(_) => {
                            if let Err(e) = players.certify(&live, wall) {
                                fault(e);
                            }
                        }
                        RuntimeEvent::Promoted(_) => superseded = true,
                    }
                }
            }
            Err(e) => fault(e),
        }
        step(2, &mut step_ms);
        if !live.has_pending() {
            let at = game.min(live.primary().receipt().logical_ms + migration::MAX_BATCH_GAME_MS);
            let start = start_pending;
            match players.stage(&mut live, game, |h| {
                if start {
                    h.start_race(at);
                }
                Ok(())
            }) {
                Ok(true) => start_pending = false,
                Ok(false) => {}
                Err(e) => fault(e),
            }
        }
        if let Some(e) = live.failure().cloned() {
            fault(e);
        }
        let committed = live.primary().receipt().logical_ms;
        match (live.behind(), behind_since) {
            (true, None) => {
                behind_since = Some(wall);
                behind_pauses += 1;
                let pending = live.has_pending();
                emit(
                    json!({"event":"behind","committed_ms":committed,"game_ms":game,
                    "pending":pending}),
                );
            }
            (false, Some(since)) => {
                behind_since = None;
                emit(
                    json!({"event":"caught_up","committed_ms":committed,"game_ms":game,
                    "behind_ms":wall - since}),
                );
            }
            _ => {}
        }
        step(3, &mut step_ms);
        players.flush(wall);
        if superseded {
            paused.get_or_insert_with(|| "superseded: the standby took over".into());
        }
        if let Some(reason) = paused {
            emit(
                json!({"event":"paused","reason":reason,"max_frame_gap_ms":max_gap_ms,
                "committed_ms":live.primary().receipt().logical_ms,"game_ms":game,
                "step_max_ms":step_ms,"behind_pauses":behind_pauses}),
            );
            return Err(format!("The armed room paused: {reason}"));
        }
        let authority = live.authority();
        if let Some(world) = authority.world() {
            if !racing_sent
                && world.phase == RacePhase::Racing
                && world.tick >= world.go_tick + u64::from(race_core::math::TICK_HZ)
            {
                racing_sent = true;
                emit(json!({"event":"racing","racing_commits":racing_commits,
                    "tick":world.tick,"racers":world.racers.len(),"max_frame_gap_ms":max_gap_ms,"step_max_ms":step_ms,
                    "logical_ms":live.primary().receipt().logical_ms}));
            }
        }
        if !completed_sent && authority.phase() == HostPhase::Results {
            completed_sent = true;
            emit(json!({"event":"race_completed","humans":authority.human_count()}));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    emit(json!({"event":"stopped","racing_commits":racing_commits,"behind_pauses":behind_pauses}));
    Ok(())
}

fn main() {
    let result = args().and_then(|args| match args.mode {
        Mode::Bench => bench(&args),
        Mode::Migration => migration_host(&args),
        _ => network(&args),
    });
    if let Err(error) = result {
        eprintln!("stampede-headless: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Args, String> {
        parse_args(line.split_whitespace().map(String::from), None)
    }

    fn refused(line: &str) -> String {
        parse(line).err().unwrap_or_else(|| panic!("accepted: {line}"))
    }

    #[test]
    fn dropping_the_owner_loop_stops_and_waits_for_its_thread() {
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let done = done.clone();
            std::thread::spawn(move || {
                while !OWNER_STOP.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // The cursor would be saved here.
                std::thread::sleep(Duration::from_millis(50));
                done.store(true, Ordering::Relaxed);
            })
        };
        // An early return drops it: the thread has finished when the drop returns.
        drop(OwnerLoop(Some(thread)));
        assert!(done.load(Ordering::Relaxed));
        OWNER_STOP.store(false, Ordering::Relaxed);
    }

    #[test]
    fn lan_mode_is_unchanged() {
        let args = parse("--lan --test-identity --players 2 --seconds 300").unwrap();
        assert_eq!(args.mode, Mode::Lan);
        assert!(!args.online && args.relays.is_empty() && args.advertise.is_empty());
        assert_eq!((args.quic_port, args.status), (0, None));
        let net = room_net(&args);
        assert!(net.relays.is_empty() && net.stun.is_empty() && !net.relay_fallback);
        assert!(!net.host_has_player && net.quic_port == 0 && net.advertise.is_empty());
        assert!(refused("--lan").contains("--test-identity"));
        assert!(refused("--bench --seconds 0").contains("seconds"));
        assert!(refused("--lan --test-identity --seconds 3601").contains("seconds"));
        assert!(refused("--lan --test-identity --seconds -1").contains("seconds"));
        assert!(refused("--test-identity").contains("Choose"));
        assert!(parse("--bench").is_ok() && parse("--smoke").is_ok());
        // A plain LAN room still waits for eight humans unless told otherwise.
        assert_eq!(args.players, 2);
        assert_eq!(parse("--lan --test-identity").unwrap().players, 8);
    }

    #[test]
    fn an_online_room_starts_with_one_ready_human_unless_told_otherwise() {
        assert_eq!(parse("--online --test-identity").unwrap().players, 1);
        assert_eq!(parse("--lan --online --test-identity").unwrap().players, 1);
        assert_eq!(parse("--online --test-identity --players 3").unwrap().players, 3);
        assert_eq!(parse("--online --test-identity --players 8").unwrap().players, 8);
        assert!(refused("--online --test-identity --players 0").contains("players"));
        assert!(HELP.contains("default 1 with --online"));
    }

    #[test]
    fn a_status_host_name_needs_a_status_page() {
        let args = parse("--online --test-identity --status 0.0.0.0:8780 --status-host Keep.Home.Arpa").unwrap();
        assert_eq!(args.status_host.as_deref(), Some("keep.home.arpa"));
        assert!(refused("--online --test-identity --status-host keep.home.arpa").contains("--status"));
        for bad in ["", "a b", "evil/x", "a:80", "ünï"] {
            let line = ["--online", "--test-identity", "--status", "127.0.0.1:1", "--status-host", bad];
            assert!(parse_args(line.map(String::from), None).is_err(), "accepted {bad:?}");
        }
        assert!(HELP.contains("--status-host"));
    }

    #[test]
    fn online_mode_keeps_the_default_relays_and_stun() {
        let args = parse("--online --test-identity").unwrap();
        assert_eq!(args.mode, Mode::Lan);
        assert!(args.online);
        let net = room_net(&args);
        let defaults = race_session::net_config(std::env::temp_dir());
        assert!(!net.relays.is_empty());
        assert_eq!(net.relays, defaults.relays);
        assert_eq!(net.stun, defaults.stun);
        assert!(net.relay_fallback && !net.host_has_player);
        assert!(parse("--online --lan --test-identity").is_ok());
        assert!(refused("--online").contains("--test-identity"));
        assert!(refused("--online --bench").contains("--online"));
        assert!(refused("--online --smoke --test-identity").contains("--online"));
        assert!(refused("--online --migration-host --test-identity").contains("--online"));
    }

    #[test]
    fn relays_are_wss_only_and_replace_the_defaults() {
        let args = parse(
            "--online --test-identity --relay wss://a.example --relay wss://b.example:8443/path --relay wss://a.example",
        )
        .unwrap();
        assert_eq!(args.relays, ["wss://a.example", "wss://b.example:8443/path"]);
        let net = room_net(&args);
        assert_eq!(net.relays, args.relays);
        assert!(!net.relay_fallback);
        for bad in ["ws://a.example", "https://a.example", "a.example", "wss://", "wss:///x", "wss://:443", "wss://a b"] {
            let line = ["--online", "--test-identity", "--relay", bad];
            assert!(parse_args(line.map(String::from), None).is_err(), "accepted {bad}");
        }
        assert!(refused("--lan --test-identity --relay wss://a.example").contains("--online"));
        assert!(refused("--online --test-identity --relay").contains("Missing"));
    }

    #[test]
    fn port_advertise_and_status_flags() {
        let args = parse(
            "--online --test-identity --quic-port 47800 --advertise 203.0.113.9:47800 --advertise [2001:db8::1]:47800 --advertise 203.0.113.9:47800 --status 127.0.0.1:8780",
        )
        .unwrap();
        assert_eq!(args.quic_port, 47800);
        assert_eq!(args.advertise.len(), 2);
        assert_eq!(args.status, Some("127.0.0.1:8780".parse().unwrap()));
        let net = room_net(&args);
        assert_eq!(net.quic_port, 47800);
        assert_eq!(net.advertise, args.advertise);
        assert!(!net.advertise_only);
        let only = parse("--online --test-identity --advertise 192.168.1.5:47800 --advertise-only").unwrap();
        assert!(room_net(&only).advertise_only);
        assert!(refused("--online --test-identity --advertise-only").contains("--advertise"));
        assert!(refused("--lan --test-identity --advertise 192.168.1.5:47800").contains("--online"));
        assert!(refused("--lan --test-identity --advertise-only").contains("--online"));
        assert!(refused("--online --test-identity --advertise 1.2.3.4").contains("--advertise"));
        assert!(refused("--online --test-identity --quic-port 70000").contains("--quic-port"));
        assert!(refused("--online --test-identity --status nonsense").contains("--status"));
        assert!(refused("--smoke --quic-port 5000").contains("--lan or --online"));
        assert!(refused("--bench --status 127.0.0.1:1").contains("--lan or --online"));
        // STAMPEDE_ADVERTISE adds to --advertise and satisfies --advertise-only.
        let env = |line: &str, value: &str| {
            parse_args(line.split_whitespace().map(String::from), Some(value.to_string()))
        };
        let only_env = "--online --test-identity --advertise-only";
        let from_env = env(only_env, "192.0.2.10:47800").unwrap();
        assert_eq!(from_env.advertise, vec!["192.0.2.10:47800".parse().unwrap()]);
        assert!(room_net(&from_env).advertise_only);
        let both = env(
            "--online --test-identity --advertise 192.0.2.10:47800",
            " 192.0.2.10:47800 , [2001:db8::1]:47800 ,",
        )
        .unwrap();
        assert_eq!(both.advertise.len(), 2);
        assert!(env(only_env, "").err().unwrap().contains("--advertise"));
        assert!(env(only_env, ":47800").err().unwrap().contains(ADVERTISE_ENV));
        assert!(env("--lan --test-identity", "192.0.2.10:47800").err().unwrap().contains("--online"));
        // A fixed port and a status page also work on a plain LAN room.
        assert!(parse("--lan --test-identity --quic-port 47800 --status 127.0.0.1:8780").is_ok());
    }

    #[test]
    fn seconds_zero_runs_until_stopped_in_a_room_only() {
        assert_eq!(parse("--online --test-identity --seconds 0").unwrap().seconds, 0.0);
        assert_eq!(parse("--lan --test-identity --seconds 0").unwrap().seconds, 0.0);
        assert!(parse("--smoke --seconds 0").is_err());
        assert!(parse("--migration-host --test-identity --data-dir d --standby any --seconds 0").is_err());
        assert!(parse("--online --test-identity --seconds nan").is_err());
    }

    #[test]
    fn help_stops_parsing_and_lists_every_flag() {
        assert!(parse("--bench --help").unwrap().help);
        assert!(parse("-h").unwrap().help);
        assert!(parse("--bogus --help").is_err());
        for flag in [
            "--online", "--relay", "--quic-port", "--advertise ", "--advertise-only", "STAMPEDE_ADVERTISE", "--seconds 0", "--status",
            "--data-dir DIR", "New guest pass", "New personal invite", "stampede://keep/<Keep key>/<secret>",
            "A guest pass works for the first person who uses it, until it expires",
        ] {
            assert!(HELP.contains(flag), "help lacks {flag}");
        }
        assert!(HELP.contains("room code to anyone who can reach it"));
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("headless-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_damaged_invites_file_turns_invites_off_and_is_left_alone() {
        let dir = scratch_dir("open-invites");
        let key = [7u8; 32];
        // No data dir, or a healthy one: nothing to report.
        assert!(matches!(open_invites(None, true, key, None), (None, None)));
        assert!(matches!(open_invites(Some(&dir), false, key, None), (None, None)));
        let (table, problem) = open_invites(Some(&dir), true, key, None);
        assert!(table.is_some() && problem.is_none());
        drop(table);
        // A damaged file: no table, the reason names the file, the file is as it was.
        let damaged = dir.join(invites::FILE);
        std::fs::write(&damaged, "{ not json").unwrap();
        let (table, problem) = open_invites(Some(&dir), true, key, None);
        assert!(table.is_none());
        let problem = problem.unwrap();
        assert!(problem.starts_with("invites.json couldn't be read: "), "{problem}");
        assert_eq!(std::fs::read_to_string(&damaged).unwrap(), "{ not json");
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from(invites::FILE)], "nothing was written beside it");
        // A problem with the key files (the identity is temporary) also turns them off,
        // and the invites file is not even opened.
        let (table, problem) = open_invites(Some(&dir), true, key, Some("persona.key couldn't be read: x".into()));
        assert!(table.is_none());
        assert_eq!(problem.as_deref(), Some("persona.key couldn't be read: x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_keeps_stdout_events_carry_no_room_code() {
        let keep = parse("--online --test-identity --status 127.0.0.1:8780").unwrap();
        let ready = ready_json(&keep, "ab", "KWTQPM", (1, 2));
        assert!(ready.get("room_code").is_none(), "{ready}");
        assert_eq!(ready["room_code_on_status_page"], true);
        assert_eq!(ready["relays_connected"], 1);
        let rolled = new_code_event(&keep, "KWTQPM", 3);
        assert!(!rolled.to_string().contains("KWTQPM") && rolled["humans"] == 3, "{rolled}");
        // Without a status page nobody could read the code there, so it stays in the event.
        for line in ["--online --test-identity", "--lan --test-identity --status 127.0.0.1:8780", "--lan --test-identity"] {
            let args = parse(line).unwrap();
            let ready = ready_json(&args, "ab", "KWTQPM", (0, 0));
            assert_eq!(ready["room_code"], "KWTQPM", "{line}");
            assert!(ready.get("room_code_on_status_page").is_none(), "{line}");
            assert_eq!(new_code_event(&args, "KWTQPM", 0)["previous"], "KWTQPM", "{line}");
        }
    }

    #[test]
    fn a_data_dir_goes_with_an_online_keep() {
        let args = parse("--online --test-identity --data-dir /var/keep --status 127.0.0.1:8780 --seconds 0").unwrap();
        assert_eq!(args.data_dir, Some(PathBuf::from("/var/keep")));
        assert!(args.online && args.mode == Mode::Lan);
        assert!(parse("--online --test-identity --data-dir").is_err());
        assert!(parse("--online --test-identity").unwrap().data_dir.is_none());
    }
}
