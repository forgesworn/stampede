//! Deterministic native-host trace used before extracting the authority adapter.
use race_core::{BotLevel, TrackId};
use race_session::proto::{encode, LossyToHost, ToHost, PROTO_VERSION};
use race_session::{
    migration, HostAuthority, HostConfig, HostNetEvent, HostPhase, HostTransport, LinkPath,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

type Peer = race_session::PeerKey;
#[derive(Clone, Debug, Serialize)]
pub enum Effect {
    Reliable(Peer, Vec<u8>),
    Lossy(Peer, Vec<u8>),
    Kick(Peer),
    Disconnect(Peer),
    ConfirmTimeout(Peer),
    Open(bool),
    OpenKeepingToken(bool),
    Shutdown,
}
#[derive(Default)]
struct Capture {
    events: VecDeque<HostNetEvent>,
    effects: Vec<Effect>,
}
struct Link(Arc<Mutex<Capture>>);
impl HostTransport for Link {
    fn poll(&mut self) -> Option<HostNetEvent> {
        self.0.lock().unwrap().events.pop_front()
    }
    fn send_reliable(&mut self, p: Peer, b: Vec<u8>) {
        self.0.lock().unwrap().effects.push(Effect::Reliable(p, b));
    }
    fn send_lossy(&mut self, p: Peer, b: Vec<u8>) {
        self.0.lock().unwrap().effects.push(Effect::Lossy(p, b));
    }
    fn kick(&mut self, p: Peer) {
        self.0.lock().unwrap().effects.push(Effect::Kick(p));
    }
    fn disconnect(&mut self, p: Peer) {
        self.0.lock().unwrap().effects.push(Effect::Disconnect(p));
    }
    fn confirm_timeout(&mut self, p: Peer) {
        self.0
            .lock()
            .unwrap()
            .effects
            .push(Effect::ConfirmTimeout(p));
    }
    fn set_open(&mut self, b: bool) {
        self.0.lock().unwrap().effects.push(Effect::Open(b));
    }
    fn set_open_keeping_token(&mut self, b: bool) {
        self.0
            .lock()
            .unwrap()
            .effects
            .push(Effect::OpenKeepingToken(b));
    }
    fn rtt_ms(&self, _: Peer) -> Option<u32> {
        Some(40)
    }
    fn shutdown(&mut self) {
        self.0.lock().unwrap().effects.push(Effect::Shutdown);
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn reliable(peer: Peer, msg: ToHost) -> HostNetEvent {
    HostNetEvent::Reliable {
        peer,
        bytes: encode(&msg),
    }
}
pub fn run(
    mut observe: impl FnMut(u64, &[&str], &[HostNetEvent], &[Effect], &[u8]),
    emit: bool,
) -> [u8; 32] {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let config = HostConfig {
        track: TrackId::Savanna,
        laps: 1,
        bots: 3,
        bot_level: BotLevel::Hard,
        seed: 42,
        auto_start: false,
    };
    let mut host = HostAuthority::new_headless(Box::new(Link(capture.clone())), config);
    let track = track();
    host.set_custom_track(&track).unwrap();
    let mut root = [0u8; 32];
    let mut first_results = None;
    let mut rematch_tick = None;
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    for tick in 1..=36_000u64 {
        let now = tick * 1_000_000 / 60 / 1000;
        let mut events = Vec::new();
        let mut actions = Vec::new();
        if tick == 1 {
            for peer in [1, 2] {
                events.push(HostNetEvent::Joined {
                    peer,
                    path: LinkPath::Direct,
                });
                events.push(reliable(
                    peer,
                    ToHost::Hello {
                        proto: PROTO_VERSION,
                        name: format!("Test racer {peer}"),
                        character: peer as u8,
                    },
                ));
                events.push(reliable(peer, ToHost::TrackReady { hash: track.hash() }));
            }
        }
        if tick == 2 {
            assert!(host.start_race(now));
            actions.push("start");
        }
        if tick == 500 {
            events.push(HostNetEvent::Left { peer: 2 });
        }
        if host.phase() == HostPhase::Racing {
            if let (Some(index), Some(world)) = (host.racer_of(0), host.world()) {
                if !(300..330).contains(&tick) {
                    let mut input = race_core::bot_input(world, index, BotLevel::Hard);
                    let mut seq = world.tick as u32 + 1;
                    if tick == 330 {
                        seq = seq.saturating_sub(10);
                        input.jump = true;
                        input.item = true;
                    }
                    let race = if rematch_tick.is_some() { 2 } else { 1 };
                    events.push(HostNetEvent::Lossy {
                        peer: 1,
                        bytes: encode(&LossyToHost::Inputs {
                            race,
                            seq,
                            inputs: vec![input.pack()],
                        }),
                    });
                }
            }
        }
        if host.phase() == HostPhase::Results && first_results.is_none() {
            first_results = Some(tick);
            events.push(reliable(1, ToHost::RematchReady(true)));
        }
        let event_json: Vec<_> = events
            .iter()
            .map(|event| match event {
                HostNetEvent::Joined { peer, .. } => serde_json::json!({"joined":peer,"rtt_ms":40}),
                HostNetEvent::Left { peer } => serde_json::json!({"left":peer}),
                HostNetEvent::Reliable { peer, bytes } => {
                    serde_json::json!({"peer":peer,"reliable":hex(bytes)})
                }
                HostNetEvent::Lossy { peer, bytes } => {
                    serde_json::json!({"peer":peer,"lossy":hex(bytes)})
                }
            })
            .collect();
        let observed_events = events.clone();
        capture.lock().unwrap().events.extend(events);
        host.update(now);
        if first_results.is_some() && host.phase() == HostPhase::Racing && rematch_tick.is_none() {
            rematch_tick = Some(tick);
        }
        let effects = std::mem::take(&mut capture.lock().unwrap().effects);
        let checkpoint = migration::checkpoint(&host).unwrap().1;
        let checkpoint_hash = decented_net::migration::state_hash(&checkpoint).unwrap();
        let effect_bytes = bincode::serialize(&effects).unwrap();
        let command_hash = decented_net::migration::state_hash(&effect_bytes).unwrap();
        let mut row = root.to_vec();
        row.extend(tick.to_be_bytes());
        row.extend(checkpoint_hash);
        row.extend(command_hash);
        root = decented_net::migration::state_hash(&row).unwrap();
        observe(tick, &actions, &observed_events, &effects, &checkpoint);
        if emit {
            writeln!(output,"{}",serde_json::json!({"tick":tick,"now_ms":now,"actions":actions,"events":event_json,"commands":effects,"checkpoint_sha256":hex(&checkpoint_hash),"checkpoint_bytes":checkpoint.len(),"root":hex(&root)})).unwrap();
        }
        if rematch_tick.is_some_and(|started| tick >= started + 300) {
            break;
        }
    }
    assert!(first_results.is_some(), "native race must reach results");
    assert!(
        rematch_tick.is_some(),
        "native ready vote must start the rematch"
    );
    if emit {
        eprintln!(
            "native fixture: results={first_results:?} rematch={rematch_tick:?} root={}",
            hex(&root)
        );
    }
    root
}

fn main() {
    run(|_, _, _, _, _| {}, true);
}

pub fn track() -> race_core::CustomTrack {
    let raw: Vec<_> = (0..=500)
        .map(|i| {
            let a = i as f32 / 500.0 * std::f32::consts::TAU;
            race_core::Vec2::new(a.cos() * 95.0, a.sin() * 95.0)
        })
        .collect();
    let shaped = race_core::shape(&raw, false, 18.0).unwrap();

    race_core::custom_track_from(&shaped, "Test circuit", TrackId::Oasis, 1, 180, vec![], 9)
}
