//! Item 9 (automated part): a real subprocess kill. A separate
//! `stampede-headless --migration-host` process hosts the room headless (no
//! player seat) over localhost QUIC. A full lobby of eight humans, no bot:
//! this process drives them as the app does (the standby device, which
//! consents and keeps its own seat, and seven remote humans), each device's
//! driver loop on its own thread as on its own phone. It waits for
//! the host's `armed` and `racing` barrier lines, then SIGKILLs the host
//! mid-race. Every surviving client must receive the successor's
//! authoritative `Resume` within 1000 ms of the kill, in the same room code,
//! session and match, on the same seats, and the race must then finish under
//! the successor.
//!
//! The full lobby is deliberate: when the eighth human joins, the host closes
//! its listener, and closing rotates the LAN admission token (`decented_net`
//! `set_open`), so the token every player joined with no longer admits it.
//! The host's `Switch` carries its current token, and every player re-dials
//! its own join address with that. The host's `armed` line reports
//! `lan_token_rotated`, and this test requires it to be true (the rotated
//! path is the one exercised) with every human protected.
//!
//! Acceptance runs use a release build, as phones run (the default test
//! build's per-batch signing and hashing can stall the host's frames past
//! the 250 ms skew budget):
//! `cargo test --release --locked -p race-session --test migration sigkilled -- --ignored --nocapture`

use super::*;
use decented_net::Keys;
use race_core::{bot_input, BotLevel, RaceConfig};
use race_session::migration::{
    ArmingState, ClientArming, ClientSwitch, LiveStandby, LiveSuccessor, OriginalLink, OwnSeat,
    PromoteRefused, PromotedStandby, RecoveringPlayer, RemotePlayer, RunnerClock, SurvivorLinks,
    SwitchState,
};
use race_session::HostPhase;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

/// This file's own errors: a reason to fail the trial with.
type Res<T> = std::result::Result<T, String>;

/// Seats: the humans this process drives, and the host's bots.
const HUMANS: usize = 8;
const BOTS: usize = 0;
const SEATS: usize = HUMANS + BOTS;
const TRIALS: usize = 10;
const RESUME_LIMIT_MS: u64 = 1000;

// ─── the host process ──────────────────────────────────────────────────────

/// The host subprocess, its stdout lines (drained on a thread for its whole
/// life, so a full pipe can never stall it) and its stderr.
struct HostProcess {
    child: Child,
    lines: mpsc::Receiver<serde_json::Value>,
    stderr: Arc<Mutex<String>>,
    /// Every stdout line, for a failure report.
    stdout: Arc<Mutex<Vec<String>>>,
    reaped: bool,
}

impl HostProcess {
    fn spawn(data_dir: &std::path::Path, standby: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_stampede-headless"))
            .args(["--migration-host", "--test-identity", "--players"])
            .arg(HUMANS.to_string())
            .arg("--bots")
            .arg(BOTS.to_string())
            .args(["--laps", "1", "--seconds", "300"])
            .arg("--data-dir")
            .arg(data_dir)
            .args(["--standby", standby])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stampede-headless");
        let (tx, lines) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let log = stdout.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                log.lock().unwrap().push(line.clone());
                let value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| serde_json::json!({"event":"text","line":line}));
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut err = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Self {
            child,
            lines,
            stderr,
            stdout,
            reaped: false,
        }
    }
    /// The process exited on its own: a failure, with its stderr.
    fn check_alive(&mut self) -> Res<()> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.reaped = true;
                Err(format!(
                    "the host exited early ({status}): {}",
                    self.stderr.lock().unwrap()
                ))
            }
            _ => Ok(()),
        }
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ─── one device, driven as the app drives it ──────────────────────────────

/// The device's game-facing time, as the app keeps it: held through arming
/// and the switch, then continuous.
struct SessionTime {
    hold_from: Option<u64>,
    shift: u64,
}
impl SessionTime {
    fn now(&self, clock: RunnerClock) -> u64 {
        let now = clock.now_ms();
        self.hold_from.unwrap_or(now).saturating_sub(self.shift)
    }
    fn hold(&mut self, clock: RunnerClock) {
        self.hold_from.get_or_insert(clock.now_ms());
    }
    fn release(&mut self, clock: RunnerClock) {
        if let Some(held) = self.hold_from.take() {
            self.shift += clock.now_ms().saturating_sub(held);
        }
    }
}

#[allow(clippy::large_enum_variant)] // as the app's takeover stage; it lives in a box
enum Stage {
    Driving(Recovery),
    Barrier(RecoveryBarrier),
}

struct Takeover {
    links: SurvivorLinks,
    stage: Stage,
    own: Option<OwnSeat>,
    retired: bool,
    own_frozen: u64,
    resume_host: Option<QuicHost>,
    keys: Keys,
    descriptor: Descriptor,
}

struct Successor {
    live: LiveSuccessor,
    c: u64,
    resumed_local: u64,
    descriptor: Descriptor,
    /// The resume listener served the original room code.
    resume_code: Option<String>,
}

enum Role {
    Arming(Box<ClientArming>, Box<ClientSession>),
    Switch(Box<ClientSwitch>),
    Remote(Box<RemotePlayer>),
    Recovering(Box<RecoveringPlayer>),
    Takeover(Box<Takeover>),
    Successor(Box<Successor>),
    Moving,
}

#[derive(Clone)]
struct Ctx {
    cfg: NetConfig,
    code: String,
    clock: RunnerClock,
}

struct Device {
    name: String,
    identity: Identity,
    link: Arc<Mutex<QuicClient>>,
    role: Role,
    /// The standby device's runtime and the descriptor it consented to.
    standby: Option<(LiveStandby, Descriptor)>,
    /// Kept from the consent for the final checks.
    standby_session: Option<SessionId>,
    time: SessionTime,
    link_lost: bool,
    /// Monotonic time this device's client took the successor's `Resume`.
    resumed: Option<Instant>,
    /// The device loop's timing, for a failure report.
    frames: Frames,
}

/// Every seat races itself to the line: the bot driver on its own view.
fn autopilot(client: &ClientSession) -> Input {
    client
        .view()
        .filter(|v| !v.over)
        .map_or(Input::default(), |v| {
            bot_input(&v.world, v.you, BotLevel::Hard)
        })
}

impl Device {
    fn describe(&self) -> String {
        let role = match &self.role {
            Role::Arming(a, me) => format!("arming {:?} {:?}", a.state(), me.phase()),
            Role::Switch(s) => format!("switch {:?}", s.state()),
            Role::Remote(p) => format!("remote protected={}", p.protected()),
            Role::Recovering(_) => "recovering".into(),
            Role::Takeover(_) => "takeover".into(),
            Role::Successor(_) => "successor".into(),
            Role::Moving => "moving".into(),
        };
        format!("{}: {role}", self.name)
    }

    fn client(&self) -> Option<&ClientSession> {
        match &self.role {
            Role::Arming(_, me) => Some(me),
            Role::Remote(p) => Some(p.client()),
            Role::Recovering(p) => Some(p.client()),
            Role::Takeover(t) => t.own.as_ref().map(|o| o.client()),
            Role::Successor(s) => s.live.own_client(),
            _ => None,
        }
    }

    fn drive(&mut self, ctx: &Ctx) -> Res<()> {
        let now = self.time.now(ctx.clock);
        let wall = ctx.clock.now_ms();
        let input = self.client().map_or(Input::default(), autopilot);
        let role = std::mem::replace(&mut self.role, Role::Moving);
        self.role = match role {
            Role::Arming(arming, me) => self.drive_arming(ctx, arming, me, input, now, wall)?,
            Role::Switch(switch) => self.drive_switch(ctx, switch)?,
            Role::Remote(player) => self.drive_remote(ctx, player, input, now, wall)?,
            Role::Recovering(mut player) => {
                player
                    .step(now, wall)
                    .map_err(|e| format!("{}: recovery: {e:?}", self.name))?;
                if player.resumed() {
                    self.resumed.get_or_insert_with(Instant::now);
                    let player = player
                        .into_resumed()
                        .map_err(|e| format!("{}: resumed: {e:?}", self.name))?;
                    Role::Remote(Box::new(player))
                } else {
                    Role::Recovering(player)
                }
            }
            Role::Takeover(t) => self.drive_takeover(t, now, wall)?,
            Role::Successor(mut room) => {
                room.live
                    .poll(wall)
                    .map_err(|e| format!("successor poll: {e:?}"))?;
                if room.live.own_client().is_some() {
                    let own = room.live.own_client().map_or(Input::default(), autopilot);
                    room.live
                        .own_frame(now, own)
                        .map_err(|e| format!("successor own seat: {e:?}"))?;
                }
                let game = room.c + now.saturating_sub(room.resumed_local);
                for _ in 0..8 {
                    if room.live.receipt().logical_ms >= game {
                        break;
                    }
                    if !room
                        .live
                        .stage(game, |_| Ok(()))
                        .map_err(|e| format!("successor stage: {e:?}"))?
                    {
                        break;
                    }
                    room.live
                        .commit(wall)
                        .map_err(|e| format!("successor commit: {e:?}"))?;
                }
                room.live.flush(wall);
                Role::Successor(room)
            }
            Role::Moving => unreachable!(),
        };
        Ok(())
    }

    fn drive_arming(
        &mut self,
        ctx: &Ctx,
        mut arming: Box<ClientArming>,
        mut me: Box<ClientSession>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Res<Role> {
        me.update(now, input);
        {
            let mut link = self.link.lock().unwrap();
            arming.poll(&mut me, &mut link, wall);
            if arming.consent_requested() {
                arming.answer(&mut link, true);
            }
        }
        if let Some((standby, _)) = self.standby.as_mut() {
            standby
                .poll()
                .map_err(|e| format!("standby while arming: {e:?}"))?;
        }
        Ok(match arming.state().clone() {
            ArmingState::AwaitingConsent | ArmingState::Activating => {
                self.time.hold(ctx.clock);
                Role::Arming(arming, me)
            }
            ArmingState::Armed => {
                self.time.hold(ctx.clock);
                if self.standby.is_none() {
                    if let (Some(boot), Some(descriptor)) =
                        (arming.take_standby(), arming.descriptor())
                    {
                        let live = LiveStandby::new(
                            descriptor.clone(),
                            boot,
                            self.identity.clone(),
                            ctx.clock,
                        )
                        .map_err(|e| format!("standby runtime: {e:?}"))?;
                        self.standby_session = Some(descriptor.root.session);
                        self.standby = Some((live, descriptor));
                    }
                }
                Role::Arming(arming, me)
            }
            ArmingState::Switching => {
                self.time.hold(ctx.clock);
                match arming.take_switch(*me, self.standby.as_ref().map(|s| &s.0)) {
                    Ok(switch) => Role::Switch(Box::new(switch)),
                    Err(me) => Role::Arming(arming, Box::new(me)),
                }
            }
            state @ (ArmingState::Unprotected { .. } | ArmingState::Refused { .. }) => {
                return Err(format!("{}: arming ended {state:?}", self.name))
            }
            ArmingState::Idle | ArmingState::Asking => Role::Arming(arming, me),
        })
    }

    fn drive_switch(&mut self, ctx: &Ctx, mut switch: Box<ClientSwitch>) -> Res<Role> {
        if !switch.state().is_finished() {
            switch.poll();
        }
        if let Some((standby, _)) = self.standby.as_mut() {
            standby
                .poll()
                .map_err(|e| format!("standby while switching: {e:?}"))?;
        }
        Ok(match switch.state().clone() {
            SwitchState::Switching => Role::Switch(switch),
            SwitchState::Protected { .. } => {
                self.time.release(ctx.clock);
                let player = switch
                    .take_player()
                    .ok_or_else(|| format!("{}: switched without a route", self.name))?;
                Role::Remote(Box::new(player))
            }
            SwitchState::Unprotected { reason } => {
                return Err(format!("{}: switch unprotected: {reason}", self.name))
            }
        })
    }

    fn drive_remote(
        &mut self,
        ctx: &Ctx,
        mut player: Box<RemotePlayer>,
        input: Input,
        now: u64,
        wall: u64,
    ) -> Res<Role> {
        // The standby device: everything queued is installed before its
        // watch is read, then it promotes once the watch expired.
        if let Some((standby, _)) = self.standby.as_mut() {
            for _ in 0..8 {
                match standby.poll() {
                    Ok(installed) if installed.is_empty() && standby.queued_bytes() == 0 => break,
                    Ok(_) => {}
                    Err(e) => return Err(format!("standby fault: {e:?}")),
                }
            }
            if let Some(error) = standby.failure() {
                return Err(format!("the standby latched {error:?}"));
            }
            let expired = standby.expired();
            if matches!(standby.original(), OriginalLink::Left) || expired == Ok(true) {
                let (live, descriptor) = self.standby.take().unwrap();
                match live.promote() {
                    Ok(promoted) => {
                        return self.begin_takeover(ctx, promoted, descriptor, *player, now, wall)
                    }
                    Err(PromoteRefused {
                        standby: Some(back),
                        ..
                    }) => self.standby = Some((*back, descriptor)),
                    Err(PromoteRefused { error, .. }) => {
                        return Err(format!("the standby could not take over: {error:?}"))
                    }
                }
            }
        }
        if !self.link_lost {
            if let Err(e) = player.frame(now, wall, input) {
                if player.protected() || self.standby.is_some() {
                    self.link_lost = true;
                } else {
                    return Err(format!("{}: frame failed: {e:?}", self.name));
                }
            }
        }
        if player.protected() {
            match player.watch_standby(wall) {
                Ok(Some(promotion)) => {
                    let recovering = player
                        .recover(promotion, wall)
                        .map_err(|e| format!("{}: recover: {e:?}", self.name))?;
                    return Ok(Role::Recovering(Box::new(recovering)));
                }
                Ok(None) => {}
                Err(e) => return Err(format!("{}: standby watch: {e:?}", self.name)),
            }
        }
        Ok(Role::Remote(player))
    }

    fn begin_takeover(
        &mut self,
        ctx: &Ctx,
        mut promoted: PromotedStandby,
        descriptor: Descriptor,
        player: RemotePlayer,
        now: u64,
        wall: u64,
    ) -> Res<Role> {
        let _ = promoted.poll(wall);
        let own_frozen = player.frozen_at().unwrap_or(now);
        let (links, recovery) = SurvivorLinks::open(promoted.into_link())
            .map_err(|e| format!("survivor links: {e:?}"))?;
        let root = &descriptor.root;
        let image = SessionImage::decode(root, recovery.sequencer().image())
            .map_err(|e| format!("image at C: {e:?}"))?;
        if !image.members.iter().any(|m| m.key == Some(root.standby)) {
            return Err("the standby's seat was not a member at C".into());
        }
        let (client, delivery, intents) = player
            .into_parts()
            .map_err(|e| format!("own seat parts: {e:?}"))?;
        let own = OwnSeat::new(
            root,
            recovery.sequencer().image(),
            client,
            delivery,
            intents,
        )
        .map_err(|e| format!("own seat: {e:?}"))?;
        let resume_host = migration::start_resume_host(
            &ctx.cfg,
            &self.identity,
            "Resumed",
            &ctx.code,
            descriptor.clone(),
            recovery.promotion().clone(),
        )
        .map_err(|e| format!("resume host: {e:?}"))?;
        Ok(Role::Takeover(Box::new(Takeover {
            links,
            stage: Stage::Driving(recovery),
            own: Some(own),
            retired: false,
            own_frozen,
            resume_host: Some(resume_host),
            keys: self.identity.keys().clone(),
            descriptor,
        })))
    }

    fn drive_takeover(&mut self, t: Box<Takeover>, now: u64, wall: u64) -> Res<Role> {
        let Takeover {
            mut links,
            stage,
            mut own,
            mut retired,
            own_frozen,
            resume_host,
            keys,
            descriptor,
        } = *t;
        let stage = match stage {
            Stage::Driving(mut recovery) => {
                match links
                    .drive(&mut recovery, own.as_mut(), &keys, now, wall)
                    .map_err(|e| format!("takeover drive: {e:?}"))?
                {
                    RecoveryPhase::Ready => {
                        retired = own.as_ref().is_none_or(|o| o.retired(&recovery));
                        let mut barrier = links
                            .barrier(recovery, &keys, wall)
                            .map_err(|e| format!("barrier: {e:?}"))?;
                        if let Some(own) = own.as_mut().filter(|_| !retired) {
                            own.ready(&mut barrier, own_frozen, now, wall)
                                .map_err(|e| format!("own seat cut: {e:?}"))?;
                        }
                        Stage::Barrier(barrier)
                    }
                    _ => Stage::Driving(recovery),
                }
            }
            Stage::Barrier(mut barrier) => {
                if links
                    .drive_barrier(&mut barrier, wall)
                    .map_err(|e| format!("barrier drive: {e:?}"))?
                {
                    let (actor, _resume) = links
                        .resume(barrier, &keys, wall)
                        .map_err(|e| format!("resume: {e:?}"))?;
                    if let Some(own) = own.as_mut().filter(|_| !retired) {
                        own.resumed(now)
                            .map_err(|e| format!("own seat resume: {e:?}"))?;
                        self.resumed.get_or_insert_with(Instant::now);
                    }
                    let resume_code = resume_host
                        .as_ref()
                        .and_then(|h| h.code().map(str::to_owned));
                    let live = links
                        .into_successor(actor, own.filter(|_| !retired), resume_host, keys)
                        .map_err(|e| format!("successor: {e:?}"))?;
                    let c = live.receipt().logical_ms;
                    return Ok(Role::Successor(Box::new(Successor {
                        live,
                        c,
                        resumed_local: now,
                        descriptor,
                        resume_code,
                    })));
                }
                Stage::Barrier(barrier)
            }
        };
        Ok(Role::Takeover(Box::new(Takeover {
            links,
            stage,
            own,
            retired,
            own_frozen,
            resume_host,
            keys,
            descriptor,
        })))
    }
}

// ─── one device per thread ─────────────────────────────────────────────────

/// A job the test runs on a device between its frames.
type Job = Box<dyn FnOnce(&mut Device) + Send>;

/// One device's driver loop on its own thread, as on its own phone: the
/// standby device's replies (and, once it takes over, its commits) never wait
/// behind another device's frame. The device is built on its thread (its
/// switch dialler is not `Send`), and the test reads it only through jobs run
/// between its frames. A device error is latched and the device stops
/// driving; the test fails on it.
struct DeviceThread {
    name: String,
    jobs: Option<mpsc::Sender<Job>>,
    error: Arc<Mutex<Option<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DeviceThread {
    fn spawn(ctx: Ctx, name: String, make: impl FnOnce() -> Device + Send + 'static) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        let error = Arc::new(Mutex::new(None));
        let latch = error.clone();
        let handle = std::thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                let mut device = make();
                loop {
                    loop {
                        match rx.try_recv() {
                            Ok(job) => job(&mut device),
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }
                    if latch.lock().unwrap().is_none() {
                        if let Err(e) = device.frame(&ctx) {
                            *latch.lock().unwrap() = Some(e);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
            .expect("spawn a device thread");
        Self {
            name,
            jobs: Some(jobs),
            error,
            handle: Some(handle),
        }
    }

    fn ask<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Device) -> R + Send + 'static,
    ) -> mpsc::Receiver<R> {
        let (tx, rx) = mpsc::channel();
        let job: Job = Box::new(move |d| {
            let _ = tx.send(f(d));
        });
        let _ = self.jobs.as_ref().unwrap().send(job);
        rx
    }

    fn error(&self) -> Option<String> {
        if let Some(e) = self.error.lock().unwrap().clone() {
            return Some(e);
        }
        self.handle
            .as_ref()
            .is_some_and(|h| h.is_finished())
            .then(|| format!("{}: the device thread ended", self.name))
    }
}

impl Drop for DeviceThread {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Run `f` on every device (all at once) and collect the answers in order.
fn query<R: Send + 'static>(
    trial: usize,
    host: &HostProcess,
    threads: &[DeviceThread],
    f: impl FnOnce(&mut Device) -> R + Send + Clone + 'static,
) -> Vec<R> {
    let replies: Vec<_> = threads.iter().map(|t| t.ask(f.clone())).collect();
    replies
        .into_iter()
        .zip(threads)
        .map(|(reply, t)| {
            reply
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| {
                    let why = t.error().unwrap_or_else(|| "no answer".into());
                    fail(trial, host, format!("{}: {why}", t.name))
                })
        })
        .collect()
}

/// As [`query`], on one device.
fn query_one<R: Send + 'static>(
    trial: usize,
    host: &HostProcess,
    thread: &DeviceThread,
    f: impl FnOnce(&mut Device) -> R + Send + Clone + 'static,
) -> R {
    query(trial, host, std::slice::from_ref(thread), f)
        .pop()
        .unwrap()
}

/// Fail on the first device error.
fn check_devices(trial: usize, host: &HostProcess, threads: &[DeviceThread]) {
    if let Some(e) = threads.iter().find_map(DeviceThread::error) {
        fail(trial, host, e);
    }
}

/// Each device's longest loop gap and longest frame, for a failure report.
fn timing(trial: usize, host: &HostProcess, threads: &[DeviceThread]) -> String {
    query(trial, host, threads, |d| {
        format!(
            "{}: gap {} ms, frame {} ms ({})",
            d.name, d.frames.max_gap_ms, d.frames.max_step_ms, d.frames.slowest
        )
    })
    .join("; ")
}

// ─── one trial ─────────────────────────────────────────────────────────────

/// What a client showed just before the kill.
struct Before {
    seat: Option<u8>,
    you: usize,
    cfg: RaceConfig,
    tick: u64,
}

struct TrialResult {
    /// Kill to `Resume`, per human: the standby device's own seat first.
    per_client_ms: Vec<u64>,
    racing_commits: u64,
    /// The longest device-loop gap from the kill to the finish.
    loop_gap_ms: u64,
    signal: Option<i32>,
    same_code: bool,
    same_session: bool,
    same_match: bool,
    stable_seats: bool,
    completed: bool,
    no_phantom_seat: bool,
    invariants: Vec<String>,
}

fn fail(trial: usize, host: &HostProcess, what: impl std::fmt::Display) -> ! {
    let stdout = host.stdout.lock().map(|l| l.join("\n")).unwrap_or_default();
    let stderr = host.stderr.lock().map(|e| e.clone()).unwrap_or_default();
    panic!("trial {trial}: {what}\n--- host stdout ---\n{stdout}\n--- host stderr ---\n{stderr}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn seat_ids(seats: &[race_session::proto::SeatView]) -> Vec<(u8, String)> {
    seats.iter().map(|s| (s.id, s.name.clone())).collect()
}

/// A device's longest gap between two starts of its driver loop, its longest
/// frame, and what ran then: the successor refuses a driver clock over 250 ms
/// past its last commit as skew, so a stall here is a stall of the new host.
#[derive(Default)]
struct Frames {
    last: Option<Instant>,
    max_gap_ms: u64,
    max_step_ms: u64,
    slowest: String,
}

impl Device {
    /// One loop of the device's driver, timed.
    fn frame(&mut self, ctx: &Ctx) -> Res<()> {
        let start = Instant::now();
        if let Some(last) = self.frames.last {
            let gap = (start - last).as_millis() as u64;
            self.frames.max_gap_ms = self.frames.max_gap_ms.max(gap);
        }
        self.frames.last = Some(start);
        let result = self.drive(ctx);
        let took = start.elapsed().as_millis() as u64;
        self.frames.max_step_ms = self.frames.max_step_ms.max(took);
        if took > 20 {
            self.frames.slowest = format!("{}: {took} ms", self.describe());
        }
        result.map_err(|e| {
            format!(
                "{e} (longest device-loop gap {} ms; slowest drive {})",
                self.frames.max_gap_ms, self.frames.slowest
            )
        })
    }
}

/// The device's seat and racer index.
fn seat_of(d: &mut Device) -> (Option<u8>, usize) {
    let client = d.client().unwrap();
    (
        client.my_seat(),
        client.view().map_or(usize::MAX, |v| v.you),
    )
}

/// The successor's state at the finish, read on the standby device.
struct SuccessorCheck {
    same_code: bool,
    session: String,
    standby_session: Option<String>,
    same_match: bool,
    lobby_seats: Vec<(u8, String)>,
    members_ok: bool,
    completed: bool,
    invariants: Vec<String>,
}

/// A trial's temporary directory, removed when the trial ends (also when it
/// fails).
struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn kill_trial(trial: usize) -> TrialResult {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = TempDir(std::env::temp_dir().join(format!(
        "stampede-kill-{}-{trial}-{nanos}",
        std::process::id()
    )));
    let (host_dir, client_dir) = (dir.0.join("host"), dir.0.join("clients"));
    std::fs::create_dir_all(&host_dir).unwrap();
    std::fs::create_dir_all(&client_dir).unwrap();
    let cfg = NetConfig::new(&client_dir, race_session::APP_TAG).lan_only();
    // Synthetic identities: ephemeral keys with a test persona's approval.
    let identities: Vec<Identity> = (0..HUMANS).map(|_| approved()).collect();
    let mut host = HostProcess::spawn(&host_dir, &identities[0].pubkey().to_hex());
    let started = Instant::now();
    let ready = loop {
        if let Ok(line) = host.lines.recv_timeout(Duration::from_millis(20)) {
            if line["event"] == "migration_ready" {
                break line;
            }
        }
        if let Err(e) = host.check_alive() {
            fail(trial, &host, e);
        }
        if started.elapsed() > Duration::from_secs(15) {
            fail(trial, &host, "no migration_ready line");
        }
    };
    let addr: std::net::SocketAddr = ready["addr"].as_str().unwrap().parse().unwrap();
    let token = ready["token"].as_str().unwrap().to_owned();
    let code = ready["room_code"].as_str().unwrap().to_owned();
    let ctx = Ctx {
        cfg: cfg.clone(),
        code: code.clone(),
        clock: RunnerClock::start(),
    };
    // Each device on its own thread, as on its own phone.
    let threads: Vec<DeviceThread> = identities
        .iter()
        .enumerate()
        .map(|(i, identity)| {
            let name = if i == 0 {
                "Backup".to_owned()
            } else {
                format!("Rider {}", i + 1)
            };
            let (cfg, identity, token, code) =
                (cfg.clone(), identity.clone(), token.clone(), code.clone());
            let device_name = name.clone();
            DeviceThread::spawn(ctx.clone(), name, move || {
                let link = Arc::new(Mutex::new(QuicClient::new(decented_net::join_addr(
                    &cfg, &identity, addr, &token,
                ))));
                let me =
                    ClientSession::new(Box::new(ClientLink(link.clone())), &device_name, i as u8);
                let arming = ClientArming::new(&cfg, &identity, &code).rejoin(addr, &token);
                Device {
                    name: device_name,
                    identity,
                    link,
                    role: Role::Arming(Box::new(arming), Box::new(me)),
                    standby: None,
                    standby_session: None,
                    time: SessionTime {
                        hold_from: None,
                        shift: 0,
                    },
                    link_lost: false,
                    resumed: None,
                    frames: Frames::default(),
                }
            })
        })
        .collect();

    // The barrier: the host's `armed` and `racing` lines, and every client
    // racing on its armed route with its clock synced.
    let (mut armed, mut racing) = (None, None);
    loop {
        check_devices(trial, &host, &threads);
        while let Ok(line) = host.lines.try_recv() {
            match line["event"].as_str() {
                Some("armed") => armed = Some(line),
                Some("racing") => racing = Some(line),
                Some("paused") => fail(
                    trial,
                    &host,
                    format!(
                        "host paused: {line}; device timing: {}",
                        timing(trial, &host, &threads)
                    ),
                ),
                _ => {}
            }
        }
        if let Err(e) = host.check_alive() {
            let states = query(trial, &host, &threads, |d| d.describe());
            fail(trial, &host, format!("{e}\ndevices: {states:#?}"));
        }
        let ready = query(trial, &host, &threads, |d| {
            let racing = match &d.role {
                Role::Remote(p) => {
                    p.client().clock_offset_ms().is_some()
                        && p.client()
                            .view()
                            .is_some_and(|v| v.world.phase == RacePhase::Racing)
                }
                _ => false,
            };
            (racing, d.standby.is_some())
        });
        let all_racing = ready.iter().all(|r| r.0);
        if armed.is_some() && racing.is_some() && all_racing && ready[0].1 {
            break;
        }
        if started.elapsed() > Duration::from_secs(60) {
            fail(trial, &host, "the armed race never reached the kill point");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let armed = armed.unwrap();
    let racing = racing.unwrap();
    let before: Vec<Before> = query(trial, &host, &threads, |d| {
        let client = d.client().unwrap();
        let view = client.view().unwrap();
        Before {
            seat: client.my_seat(),
            you: view.you,
            cfg: view.world.cfg.clone(),
            tick: view.world.tick,
        }
    });
    let (lobby_before, world_before, logical_before) = query_one(trial, &host, &threads[0], |d| {
        let client = d.client().unwrap();
        (
            client.lobby().unwrap().clone(),
            client.view().unwrap().world.clone(),
            d.standby.as_ref().unwrap().0.receipt().logical_ms,
        )
    });

    // No device may have resumed before the kill: a stale Resume would read
    // as 0 ms and pass the bound.
    let early = query(trial, &host, &threads, |d| d.resumed.is_some());
    if early.iter().any(|&r| r) {
        fail(
            trial,
            &host,
            format!("a device resumed before the kill: {early:?}"),
        );
    }
    // The kill: SIGKILL, timed on the monotonic clock.
    query(trial, &host, &threads, |d| d.frames.max_gap_ms = 0);
    let killed_at = Instant::now();
    host.child.kill().expect("SIGKILL the host");
    let resumed = loop {
        check_devices(trial, &host, &threads);
        let resumed = query(trial, &host, &threads, |d| d.resumed);
        if resumed.iter().all(Option::is_some) {
            break resumed;
        }
        if killed_at.elapsed() > Duration::from_secs(10) {
            let waiting: Vec<&str> = threads
                .iter()
                .zip(&resumed)
                .filter(|(_, r)| r.is_none())
                .map(|(t, _)| t.name.as_str())
                .collect();
            fail(trial, &host, format!("no Resume for {waiting:?}"));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    if let Some(i) = resumed.iter().position(|r| r.unwrap() < killed_at) {
        fail(trial, &host, format!("device {i} resumed before the kill"));
    }
    let per_client_ms: Vec<u64> = resumed
        .iter()
        .map(|r| r.unwrap().duration_since(killed_at).as_millis() as u64)
        .collect();
    let status = host.child.wait().expect("reap the host");
    host.reaped = true;
    let seats_resumed = query(trial, &host, &threads, seat_of);

    // The same race finishes under the successor.
    let done = Instant::now();
    loop {
        check_devices(trial, &host, &threads);
        let results = query_one(trial, &host, &threads[0], |d| match &d.role {
            Role::Successor(room) => Some(room.live.authority().phase() == HostPhase::Results),
            _ => None,
        });
        let Some(results) = results else {
            // A device error leaves its role mid-move: report the error.
            check_devices(trial, &host, &threads);
            fail(trial, &host, "the standby is not the successor")
        };
        let seen = query(trial, &host, &threads, |d| {
            d.client()
                .is_some_and(|c| c.view().is_some_and(|v| v.over) || c.results().is_some())
        });
        if results && seen.iter().all(|s| *s) {
            break;
        }
        if done.elapsed() > Duration::from_secs(150) {
            fail(trial, &host, "the race never finished under the successor");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let final_seats = query(trial, &host, &threads, seat_of);
    let dead = query(trial, &host, &threads, |d| {
        d.client()
            .is_some_and(|c| c.phase().is_dead())
            .then(|| d.name.clone())
    });
    let loop_gap_ms = query(trial, &host, &threads, |d| d.frames.max_gap_ms)
        .into_iter()
        .max()
        .unwrap_or(0);
    let keys: BTreeSet<[u8; 32]> = identities.iter().map(|i| i.pubkey().to_bytes()).collect();
    let marks: Vec<(RaceConfig, u64)> = before.iter().map(|b| (b.cfg.clone(), b.tick)).collect();
    let resume_code = code.clone();
    let successor = query_one(trial, &host, &threads[0], move |d| {
        let Role::Successor(room) = &d.role else {
            unreachable!()
        };
        let authority = room.live.authority();
        let world = authority.world().unwrap();
        let root = &room.descriptor.root;
        let image = SessionImage::decode(root, room.live.image()).unwrap();
        let mut invariants = Vec::new();
        if room.live.receipt().term != 1 {
            invariants.push(format!("term {}", room.live.receipt().term));
        }
        if room.live.failure().is_some() {
            invariants.push(format!("successor fault {:?}", room.live.failure()));
        }
        if authority.human_count() != HUMANS {
            invariants.push(format!("{} humans", authority.human_count()));
        }
        SuccessorCheck {
            same_code: room.resume_code.as_deref() == Some(resume_code.as_str())
                && root.code == room_code_hash(&resume_code).unwrap(),
            session: hex(&root.session),
            standby_session: d.standby_session.map(|s| hex(&s)),
            same_match: marks
                .iter()
                .all(|(cfg, tick)| *cfg == world.cfg && world.tick > *tick)
                && room.live.receipt().logical_ms > logical_before
                && room.c >= logical_before,
            lobby_seats: seat_ids(&authority.lobby_view().seats),
            members_ok: image.members.len() == HUMANS
                && image
                    .members
                    .iter()
                    .all(|m| m.key.is_some_and(|k| keys.contains(&k) && k != root.host)),
            completed: authority.phase() == HostPhase::Results
                && world.phase == RacePhase::Finished
                && world.racers.len() == SEATS
                && (0..SEATS)
                    .filter(|&i| world.kind(i) == RacerKind::Human)
                    .all(|i| world.racers[i].finished_at.is_some()),
            invariants,
        }
    });

    let same_code = armed["room_code"] == code.as_str() && successor.same_code;
    let same_session = armed["session"] == successor.session.as_str()
        && successor.standby_session.as_deref() == Some(successor.session.as_str());
    let same_match = successor.same_match;
    let stable_seats = before
        .iter()
        .zip(&seats_resumed)
        .zip(&final_seats)
        .all(|((b, r), f)| (b.seat, b.you) == *r && r == f && b.seat.is_some())
        && successor.lobby_seats == seat_ids(&lobby_before.seats);
    let no_phantom_seat = armed["host_seats"] == 0
        && armed["host_member"] == false
        && lobby_before.seats.len() == SEATS
        && lobby_before.seats.iter().all(|s| !s.host)
        && lobby_before.seats.iter().filter(|s| !s.bot).count() == HUMANS
        && world_before.racers.len() == SEATS
        && (0..SEATS)
            .filter(|&i| world_before.kind(i) == RacerKind::Human)
            .count()
            == HUMANS
        && successor.members_ok;
    let completed = successor.completed;
    let mut invariants = successor.invariants;
    if racing["racers"] != SEATS {
        invariants.push(format!("host raced {}", racing["racers"]));
    }
    if armed["all_protected"] != true
        || armed["members"] != HUMANS
        || armed["lan_token_rotated"] != true
    {
        invariants.push(format!("armed line {armed}"));
    }
    if let Some(dead) = dead.into_iter().flatten().next() {
        invariants.push(format!("{dead} is dead"));
    }
    drop(threads);
    drop(host);
    drop(dir);
    TrialResult {
        per_client_ms,
        racing_commits: racing["racing_commits"].as_u64().unwrap_or(0),
        loop_gap_ms,
        signal: status.signal(),
        same_code,
        same_session,
        same_match,
        stable_seats,
        completed,
        no_phantom_seat,
        invariants,
    }
}

/// Ten independent trials, each with fresh synthetic identities in fresh
/// temporary directories and a fresh host process: eight humans (the standby
/// device's own seat and seven remote humans), no bot, behind a headless host
/// with no seat. Each trial SIGKILLs the host about a second into the armed
/// race; all eight human survivors must take the successor's `Resume` within
/// 1000 ms, and the race must then finish under the successor.
#[test]
#[ignore = "ten real-time races (several minutes): run with --ignored"]
fn a_sigkilled_headless_host_process_is_taken_over_within_a_second_in_ten_trials() {
    let mut results = Vec::new();
    for trial in 1..=TRIALS {
        let t = Instant::now();
        let r = kill_trial(trial);
        let max = *r.per_client_ms.iter().max().unwrap();
        println!(
            "STAMPEDE KILL TRIAL {trial}/{TRIALS} signal={:?} racing_commits_before_kill={} resume_ms={max} per_client_ms={:?} same_code={} same_session={} same_match={} stable_seats={} completed={} no_phantom_seat={} invariants={:?} max_loop_gap_ms={} wall_s={:.1}",
            r.signal,
            r.racing_commits,
            r.per_client_ms,
            r.same_code,
            r.same_session,
            r.same_match,
            r.stable_seats,
            r.completed,
            r.no_phantom_seat,
            r.invariants,
            r.loop_gap_ms,
            t.elapsed().as_secs_f64()
        );
        results.push(r);
    }
    let max_resume = results
        .iter()
        .flat_map(|r| r.per_client_ms.iter().copied())
        .max()
        .unwrap();
    let per_client: Vec<u64> = (0..HUMANS)
        .map(|i| results.iter().map(|r| r.per_client_ms[i]).max().unwrap())
        .collect();
    let all = |f: fn(&TrialResult) -> bool| results.iter().all(f);
    let completed = results.iter().filter(|r| r.completed).count();
    let invariants_ok = results.iter().all(|r| r.invariants.is_empty());
    println!(
        "STAMPEDE SUBPROCESS KILL trials={} max_resume_ms={max_resume} per_client_ms={per_client:?} same_code={} same_session={} same_match={} stable_seats={} completed={completed} no_phantom_seat={} sigkill={} invariants={}",
        results.len(),
        all(|r| r.same_code),
        all(|r| r.same_session),
        all(|r| r.same_match),
        all(|r| r.stable_seats),
        all(|r| r.no_phantom_seat),
        all(|r| r.signal == Some(9)),
        if invariants_ok { "ok" } else { "FAILED" }
    );
    assert_eq!(results.len(), TRIALS);
    assert!(
        max_resume <= RESUME_LIMIT_MS,
        "a resume took {max_resume} ms"
    );
    assert!(all(|r| r.signal == Some(9)), "the host was not SIGKILLed");
    assert!(all(|r| r.same_code), "room code changed");
    assert!(all(|r| r.same_session), "session changed");
    assert!(all(|r| r.same_match), "match changed");
    assert!(all(|r| r.stable_seats), "seats moved");
    assert!(all(|r| r.no_phantom_seat), "phantom seat");
    assert_eq!(completed, TRIALS, "a race did not complete");
    assert!(invariants_ok, "invariants failed");
}
