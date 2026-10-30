//! Local real-game qualification. No listener, owner keys or WASI linker.
pub(crate) use fray_session::authority as n;
pub(crate) use fray_session::authority::Authority;
pub(crate) use race_session::authority::{AuthorityConfig, Control, PeerInfo, RaceAuthority};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

wasmtime::component::bindgen!({
    path: "../../../forgesworn/fray/crates/fray-session/wit",
    world: "game-authority",
});
pub(crate) use exports::fray::authority::authority as w;

#[allow(dead_code)]
#[path = "../../../race-session/examples/authority_baseline.rs"]
pub(crate) mod legacy;

const FUEL: u64 = 10_000_000;
const SETUP_FUEL: u64 = 1_000_000_000;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Qualification,
    Measurement,
}
impl Profile {
    fn fuel(self, operation: Operation) -> u64 {
        if self == Self::Measurement
            || matches!(
                operation,
                Operation::Init | Operation::Restore | Operation::Control
            )
        {
            SETUP_FUEL
        } else {
            FUEL
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Qualification => "stampede-preparation-v1",
            Self::Measurement => "setup-measurement",
        }
    }
    fn deadline_epochs(self, operation: Operation) -> u64 {
        if self.fuel(operation) == SETUP_FUEL {
            10
        } else {
            2
        }
    }
}

struct EpochTicker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl EpochTicker {
    fn start(engine: Engine) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                std::thread::park_timeout(std::time::Duration::from_millis(50));
                if !flag.load(Ordering::Acquire) {
                    engine.increment_epoch();
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            thread.join().unwrap();
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Operation {
    Init,
    Restore,
    Event,
    Control,
    Tick,
    Checkpoint,
    StateHash,
}
impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Restore => "restore",
            Self::Event => "on-event",
            Self::Control => "control",
            Self::Tick => "tick",
            Self::Checkpoint => "checkpoint",
            Self::StateHash => "state-hash",
        }
    }
}
const IMAGE_LIMIT: usize = 1024 * 1024;
const MODULE_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) struct Harness {
    engine: Engine,
    component: Component,
    component_bytes: usize,
    profile: Profile,
    samples: Arc<Mutex<BTreeMap<String, Vec<Sample>>>>,
    _ticker: EpochTicker,
}

impl Harness {
    pub(crate) fn get() -> &'static Self {
        static HARNESS: OnceLock<Harness> = OnceLock::new();
        HARNESS.get_or_init(|| Self::load(Profile::Qualification))
    }
    pub(crate) fn measurement() -> Self {
        Self::load(Profile::Measurement)
    }
    pub(crate) fn measurement_component(path: &std::path::Path) -> Self {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .unwrap()
            .take(MODULE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        Self::load_component(Profile::Measurement, bytes)
    }
    fn load(profile: Profile) -> Self {
        let path = std::env::var_os("RACE_AUTHORITY_WASM")
            .expect("build the guest and set RACE_AUTHORITY_WASM to its core .wasm file");
        let file = std::fs::File::open(&path).unwrap();
        use std::io::Read;
        let mut core = Vec::new();
        file.take(MODULE_LIMIT as u64 + 1)
            .read_to_end(&mut core)
            .unwrap();
        assert!(core.len() <= MODULE_LIMIT);
        let bytes = wit_component::ComponentEncoder::default()
            .module(&core)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap();
        Self::load_component(profile, bytes)
    }
    fn load_component(profile: Profile, bytes: Vec<u8>) -> Self {
        assert!(bytes.len() <= MODULE_LIMIT);
        let mut modules = 0;
        for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
            match payload.unwrap() {
                wasmparser::Payload::ModuleSection { .. } => {
                    modules += 1;
                    assert!(modules <= 2);
                }
                wasmparser::Payload::TypeSection(s) => assert!(s.count() <= 4096),
                wasmparser::Payload::FunctionSection(s) => assert!(s.count() <= 4096),
                wasmparser::Payload::CodeSectionEntry(body) => {
                    assert!(body.as_bytes().len() <= IMAGE_LIMIT)
                }
                _ => {}
            }
        }
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true)
            .cranelift_nan_canonicalization(true)
            .wasm_simd(false)
            .wasm_relaxed_simd(false)
            .wasm_memory64(false)
            .wasm_multi_memory(false)
            .max_wasm_stack(256 * 1024)
            .memory_reservation(64 * 1024 * 1024)
            .memory_reservation_for_growth(0)
            .memory_guard_size(64 * 1024)
            .memory_init_cow(false)
            .wasm_backtrace_max_frames(None);
        if std::env::var_os("RACE_AUTHORITY_DIAGNOSTICS").is_some() {
            config.wasm_backtrace_max_frames(std::num::NonZeroUsize::new(32));
        }
        let engine = Engine::new(&config).unwrap();
        let component = Component::new(&engine, &bytes).unwrap();
        assert_eq!(component.component_type().imports(&engine).count(), 0);
        if let Some(path) = std::env::var_os("RACE_AUTHORITY_COMPONENT") {
            std::fs::write(path, &bytes).unwrap();
        }
        let ticker = EpochTicker::start(engine.clone());
        Harness {
            engine,
            component,
            component_bytes: bytes.len(),
            profile,
            samples: Arc::new(Mutex::new(BTreeMap::new())),
            _ticker: ticker,
        }
    }
    pub(crate) fn clear_costs(&self) {
        self.samples.lock().unwrap().clear();
    }
    pub(crate) fn costs(&self) -> serde_json::Value {
        let samples = self.samples.lock().unwrap();
        let mut result = serde_json::Map::new();
        for (operation, values) in samples.iter() {
            let mut elapsed: Vec<_> = values.iter().map(|sample| sample.elapsed_ns).collect();
            elapsed.sort_unstable();
            let worst = values.iter().max_by_key(|sample| sample.fuel).unwrap();
            result.insert(
                operation.clone(),
                serde_json::json!({
                    "calls":values.len(), "max_fuel":worst.fuel, "max_fuel_tick":worst.tick,
                    "p50_ns":elapsed[(elapsed.len()-1)/2],
                    "p99_ns":elapsed[(elapsed.len()*99).div_ceil(100)-1],
                    "max_ns":elapsed.last().unwrap(),
                }),
            );
        }
        serde_json::Value::Object(result)
    }
    pub(crate) fn fresh(&self) -> Guest {
        let limits = StoreLimitsBuilder::new()
            .memory_size(64 * 1024 * 1024)
            .table_elements(1024)
            .instances(2)
            .memories(1)
            .tables(1)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(&self.engine, limits);
        store.limiter(|limits| limits);
        store.set_hostcall_fuel(IMAGE_LIMIT);
        store.set_max_component_handles(0);
        store.set_fuel(FUEL).unwrap();
        store.set_epoch_deadline(2);
        let bindings =
            GameAuthority::instantiate(&mut store, &self.component, &Linker::new(&self.engine))
                .unwrap();
        Guest {
            store,
            bindings,
            max_fuel: BTreeMap::new(),
            current_tick: 0,
            profile: self.profile,
            samples: (self.profile == Profile::Measurement).then(|| self.samples.clone()),
        }
    }
    pub(crate) fn init(&self, config: &[u8]) -> Guest {
        let mut guest = self.fresh();
        guest
            .call(Operation::Init, |b, s| {
                b.fray_authority_authority().call_init(s, config, 42)
            })
            .unwrap();
        guest
    }
    pub(crate) fn restore(&self, config: &[u8], checkpoint: &[u8]) -> Guest {
        let mut guest = self.fresh();
        guest
            .call(Operation::Restore, |b, s| {
                b.fray_authority_authority()
                    .call_restore(s, config, checkpoint)
            })
            .unwrap();
        guest
    }
}

struct Sample {
    tick: u64,
    fuel: u64,
    elapsed_ns: u64,
}

pub(crate) struct Guest {
    store: Store<StoreLimits>,
    bindings: GameAuthority,
    max_fuel: BTreeMap<String, u64>,
    current_tick: u64,
    profile: Profile,
    samples: Option<Arc<Mutex<BTreeMap<String, Vec<Sample>>>>>,
}
impl Guest {
    pub(crate) fn call<T>(
        &mut self,
        operation: Operation,
        call: impl FnOnce(&GameAuthority, &mut Store<StoreLimits>) -> wasmtime::Result<T>,
    ) -> T {
        let budget = self.profile.fuel(operation);
        let name = operation.name();
        self.store.set_fuel(budget).unwrap();
        self.store.set_hostcall_fuel(IMAGE_LIMIT);
        self.store
            .set_epoch_deadline(self.profile.deadline_epochs(operation));
        let started = Instant::now();
        let result = call(&self.bindings, &mut self.store);
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).unwrap();
        let used = budget - self.store.get_fuel().unwrap();
        if let Some(samples) = &self.samples {
            let mut samples = samples.lock().unwrap();
            let values = samples.entry(name.to_owned()).or_default();
            assert!(values.len() < 100_000, "measurement sample bound");
            values.push(Sample {
                tick: self.current_tick,
                fuel: used,
                elapsed_ns,
            });
        }
        let result = result.unwrap_or_else(|error| {
            panic!("component {name} at tick {} trapped (fuel remaining {:?}, prior maxima {:?}): {error:#}", self.current_tick, self.store.get_fuel(), self.max_fuel)
        });
        self.max_fuel
            .entry(name.to_owned())
            .and_modify(|value| *value = (*value).max(used))
            .or_insert(used);
        result
    }
    pub(crate) fn checkpoint(&mut self) -> Vec<u8> {
        self.call(Operation::Checkpoint, |b, s| {
            b.fray_authority_authority().call_checkpoint(s)
        })
        .unwrap()
    }
    pub(crate) fn hash(&mut self) -> Vec<u8> {
        self.call(Operation::StateHash, |b, s| {
            b.fray_authority_authority().call_state_hash(s)
        })
        .unwrap()
    }
    pub(crate) fn event(&mut self, time: n::Time, event: n::Event) -> Vec<n::Command> {
        self.current_tick = time.tick;
        let operation = if matches!(&event, n::Event::Control(_)) {
            Operation::Control
        } else {
            Operation::Event
        };
        let time = wire_time(time);
        let event = wire_event(event);
        commands(
            self.call(operation, |b, s| {
                b.fray_authority_authority().call_on_event(s, time, &event)
            })
            .unwrap(),
        )
    }
    pub(crate) fn tick(&mut self, time: n::Time) -> Vec<n::Command> {
        self.current_tick = time.tick;
        let time = wire_time(time);
        commands(
            self.call(Operation::Tick, |b, s| {
                b.fray_authority_authority().call_tick(s, time)
            })
            .unwrap(),
        )
    }
}
pub(crate) fn wire_time(time: n::Time) -> w::Time {
    w::Time {
        tick: time.tick,
        elapsed_us: time.elapsed_us,
    }
}
pub(crate) fn wire_event(event: n::Event) -> w::Event {
    match event {
        n::Event::Joined(player) => w::Event::Joined(w::Player {
            id: player.id.0,
            identity: player.identity.map(|bytes| bytes.to_vec()),
            metadata: player.metadata,
        }),
        n::Event::Left { player, reason } => w::Event::Left(w::Leave {
            player: player.0,
            reason,
        }),
        n::Event::Message {
            player,
            delivery,
            bytes,
        } => w::Event::Message(w::Message {
            player: player.0,
            delivery: match delivery {
                n::Delivery::Reliable => w::Delivery::Reliable,
                n::Delivery::Lossy => w::Delivery::Lossy,
            },
            bytes,
        }),
        n::Event::Control(bytes) => w::Event::Control(bytes),
    }
}
pub(crate) fn commands(commands: Vec<w::Command>) -> Vec<n::Command> {
    commands
        .into_iter()
        .map(|command| match command {
            w::Command::Send(send) => n::Command::Send {
                target: match send.target {
                    w::Target::Player(id) => n::Target::Player(n::PlayerId(id)),
                    w::Target::Broadcast => n::Target::Broadcast,
                },
                delivery: match send.delivery {
                    w::Delivery::Reliable => n::Delivery::Reliable,
                    w::Delivery::Lossy => n::Delivery::Lossy,
                },
                bytes: send.bytes,
            },
            w::Command::Disconnect(leave) => n::Command::Disconnect {
                player: n::PlayerId(leave.player),
                reason: leave.reason,
            },
            w::Command::SetOpen(open) => n::Command::SetOpen(open),
        })
        .collect()
}

pub(crate) fn assert_image(actual: &[u8], expected: &[u8], context: &str, tick: u64) {
    if actual != expected {
        let offset = actual
            .iter()
            .zip(expected)
            .position(|(a, b)| a != b)
            .unwrap_or(actual.len().min(expected.len()));
        if let Some(directory) = std::env::var_os("RACE_AUTHORITY_EVIDENCE") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let name: String = context
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            std::fs::write(directory.join(format!("{name}-{tick}-guest.bin")), actual).unwrap();
            std::fs::write(
                directory.join(format!("{name}-{tick}-native.bin")),
                expected,
            )
            .unwrap();
        }
        panic!("{context} at tick {tick}: first differing byte {offset}, guest {} bytes, native {} bytes", actual.len(), expected.len());
    }
}

pub(crate) fn compare_race(harness: &Harness) -> serde_json::Value {
    use race_core::{BotLevel, TrackId};
    use race_session::{HostNetEvent, LinkPath};
    let config = AuthorityConfig {
        track: TrackId::Custom,
        laps: 1,
        bots: 3,
        bot_level: BotLevel::Hard,
        auto_start: false,
        custom: legacy::track().to_bytes(),
        ..AuthorityConfig::default()
    }
    .to_bytes()
    .unwrap();
    let mut native = RaceAuthority::init(&config, 42).unwrap();
    let mut guest = harness.init(&config);
    let mut restored = Vec::<Guest>::new();
    let mut max_checkpoint = 0;
    let mut final_tick = 0;
    let root = legacy::run(
        |tick, actions, events, _, _| {
            final_tick = tick;
            let now = n::Time::at_tick(tick, 60).unwrap();
            let mut inputs = Vec::new();
            for action in actions {
                assert_eq!(*action, "start");
                inputs.push(n::Event::Control(Control::StartRace.to_bytes().unwrap()));
            }
            inputs.extend(events.iter().map(|event| {
                match event {
                    HostNetEvent::Joined { peer, path } => n::Event::Joined(n::Player {
                        id: n::PlayerId(*peer as u64),
                        identity: None,
                        metadata: PeerInfo {
                            local: *path == LinkPath::Local,
                            rtt_ms: Some(40),
                        }
                        .to_bytes(),
                    }),
                    HostNetEvent::Left { peer } => n::Event::Left {
                        player: n::PlayerId(*peer as u64),
                        reason: String::new(),
                    },
                    HostNetEvent::Reliable { peer, bytes } => n::Event::Message {
                        player: n::PlayerId(*peer as u64),
                        delivery: n::Delivery::Reliable,
                        bytes: bytes.clone(),
                    },
                    HostNetEvent::Lossy { peer, bytes } => n::Event::Message {
                        player: n::PlayerId(*peer as u64),
                        delivery: n::Delivery::Lossy,
                        bytes: bytes.clone(),
                    },
                }
            }));
            for event in inputs {
                let expected = native.on_event(now, event.clone()).unwrap();
                assert_eq!(guest.event(now, event.clone()), expected, "event at {tick}");
                for copy in &mut restored {
                    assert_eq!(
                        copy.event(now, event.clone()),
                        expected,
                        "restored event at {tick}"
                    );
                }
            }
            let expected = native.tick(now).unwrap();
            assert_eq!(guest.tick(now), expected, "tick commands at {tick}");
            let checkpoint = native.checkpoint().unwrap();
            max_checkpoint = max_checkpoint.max(checkpoint.len());
            assert_image(&guest.checkpoint(), &checkpoint, "checkpoint", tick);
            assert_eq!(guest.hash(), native.state_hash().unwrap(), "hash at {tick}");
            for copy in &mut restored {
                assert_eq!(copy.tick(now), expected, "restored tick at {tick}");
                assert_image(&copy.checkpoint(), &checkpoint, "restored checkpoint", tick);
                assert_eq!(copy.hash(), guest.hash(), "restored hash at {tick}");
            }
            if [1, 50, 299, 330, 500, 1424, 1604].contains(&tick) {
                let mut copy = harness.restore(&config, &checkpoint);
                assert_image(&copy.checkpoint(), &checkpoint, "fresh restore", tick);
                assert_eq!(copy.hash(), guest.hash());
                restored.push(copy);
            }
        },
        false,
    );
    assert_eq!(final_tick, 1904);
    assert_eq!(
        root.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        "d3be4cb48203b16d436cfb460f3acdb16cdb3f4c9430cae356e649848c3c0b6a"
    );
    assert_eq!(restored.len(), 7);
    for copy in restored {
        for (operation, fuel) in copy.max_fuel {
            guest
                .max_fuel
                .entry(operation)
                .and_modify(|value| *value = (*value).max(fuel))
                .or_insert(fuel);
        }
    }
    let report = serde_json::json!({"engine":"wasmtime", "engine_version":"50.0.1", "arch":std::env::consts::ARCH, "os":std::env::consts::OS, "profile":harness.profile.name(), "fuel_limits":{"ordinary":FUEL,"preparation":SETUP_FUEL}, "ticks":final_tick, "restore_continuations":7, "component_bytes":harness.component_bytes, "max_checkpoint_bytes":max_checkpoint, "max_fuel":guest.max_fuel});
    report
}
