//! Local measurement of first-party setup work; never qualification evidence.
#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod support;
use support::*;

fn decode_checkpoint(input: &std::ffi::OsStr, output: &std::ffi::OsStr) {
    use bincode::Options;
    use serde::{Deserialize, Serialize};
    #[derive(Deserialize)]
    struct Member {
        _player: u64,
        _logical: u32,
        _identity: Option<[u8; 32]>,
        _info: PeerInfo,
    }
    #[derive(Deserialize, Serialize)]
    struct Binding {
        config: [u8; 32],
        content: [u8; 32],
    }
    #[derive(Deserialize)]
    struct Image {
        _schema: u32,
        _seed: u64,
        _completed_tick: u64,
        _next: u32,
        _config: Vec<u8>,
        _members: Vec<Member>,
        _binding: Binding,
        game: Vec<u8>,
    }
    #[derive(Deserialize, Serialize)]
    struct HostOnly {
        trump_hit: Vec<u32>,
        frog_paused: Vec<f32>,
        feather: Vec<f32>,
    }
    #[derive(Deserialize, Serialize)]
    struct Game {
        schema: u32,
        game_protocol: u32,
        binding: Binding,
        custom: Option<Vec<u8>>,
        authority: race_session::HostAuthority,
        host_only: HostOnly,
    }
    let options = || {
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_big_endian()
            .with_limit(1024 * 1024)
            .reject_trailing_bytes()
    };
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(input)
        .unwrap()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= 1024 * 1024);
    let magic = b"stampede/fray-authority/v1\0";
    assert!(bytes.starts_with(magic));
    let image: Image = options().deserialize(&bytes[magic.len()..]).unwrap();
    let magic = b"stampede/authority-checkpoint/v1\0";
    assert!(image.game.starts_with(magic));
    let game: Game = options().deserialize(&image.game[magic.len()..]).unwrap();
    std::fs::write(output, serde_json::to_vec_pretty(&game).unwrap()).unwrap();
}

fn built_in(harness: &Harness, track: race_core::TrackId) -> serde_json::Value {
    use race_session::proto::{encode, ToHost, PROTO_VERSION};
    let config = AuthorityConfig {
        track,
        bots: 3,
        ..AuthorityConfig::default()
    }
    .to_bytes()
    .unwrap();
    let mut native = RaceAuthority::init(&config, 42).unwrap();
    let mut guest = harness.init(&config);
    let id = n::PlayerId(u64::MAX);
    let mut first_mismatch = None;
    let mut max_checkpoint = 0;
    for tick in 1..=600 {
        let now = n::Time::at_tick(tick, 60).unwrap();
        let events = if tick == 1 {
            vec![
                n::Event::Joined(n::Player {
                    id,
                    identity: Some([17; 32]),
                    metadata: PeerInfo {
                        local: false,
                        rtt_ms: Some(40),
                    }
                    .to_bytes(),
                }),
                n::Event::Message {
                    player: id,
                    delivery: n::Delivery::Reliable,
                    bytes: encode(&ToHost::Hello {
                        proto: PROTO_VERSION,
                        name: "Racer".into(),
                        character: 0,
                    }),
                },
                n::Event::Message {
                    player: id,
                    delivery: n::Delivery::Reliable,
                    bytes: encode(&ToHost::SetReady(true)),
                },
            ]
        } else if tick == 500 {
            vec![n::Event::Left {
                player: id,
                reason: "leaving".into(),
            }]
        } else {
            vec![]
        };
        for event in events {
            if guest.event(now, event.clone()) != native.on_event(now, event).unwrap()
                && first_mismatch.is_none()
            {
                first_mismatch = Some(serde_json::json!({"operation":"on-event","tick":tick}));
            }
        }
        if guest.tick(now) != native.tick(now).unwrap() && first_mismatch.is_none() {
            first_mismatch = Some(serde_json::json!({"operation":"tick","tick":tick}));
        }
        let actual = guest.checkpoint();
        let expected = native.checkpoint().unwrap();
        max_checkpoint = max_checkpoint.max(actual.len());
        if actual != expected && first_mismatch.is_none() {
            let offset = actual
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(actual.len().min(expected.len()));
            first_mismatch =
                Some(serde_json::json!({"operation":"checkpoint","tick":tick,"offset":offset}));
            if let Some(path) = std::env::var_os("RACE_AUTHORITY_EVIDENCE") {
                let path = std::path::PathBuf::from(path);
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join(format!("{track:?}-{tick}-guest.bin")), &actual).unwrap();
                std::fs::write(path.join(format!("{track:?}-{tick}-native.bin")), &expected)
                    .unwrap();
            }
        }
        if guest.hash() != native.state_hash().unwrap() && first_mismatch.is_none() {
            first_mismatch = Some(serde_json::json!({"operation":"state-hash","tick":tick}));
        }
        if [1, 299, 500].contains(&tick) {
            let mut restored = harness.restore(&config, &expected);
            let bytes = restored.checkpoint();
            if bytes != expected && first_mismatch.is_none() {
                first_mismatch = Some(serde_json::json!({"operation":"restore","tick":tick}));
            }
        }
    }
    serde_json::json!({"ticks":600,"native_equivalent":first_mismatch.is_none(),"first_mismatch":first_mismatch,"max_checkpoint_bytes":max_checkpoint})
}

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--decode") {
        assert_eq!(
            args.len(),
            3,
            "usage: profile_setup --decode IMAGE.bin OUTPUT.json"
        );
        decode_checkpoint(&args[1], &args[2]);
        return;
    }
    assert_eq!(args.len(), 1, "usage: profile_setup REPORT.json");
    let output = &args[0];
    std::panic::set_hook(Box::new(|_| {}));
    let harness = Harness::measurement();
    let mut scenarios = Vec::new();
    for name in ["savanna", "oasis", "custom-race"] {
        harness.clear_costs();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match name {
            "savanna" => built_in(&harness, race_core::TrackId::Savanna),
            "oasis" => built_in(&harness, race_core::TrackId::Oasis),
            _ => compare_race(&harness),
        }));
        let result = match result {
            Ok(value) => value,
            Err(payload) => {
                let error = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "non-string panic".into());
                serde_json::json!({"failure":error.chars().take(2048).collect::<String>()})
            }
        };
        scenarios
            .push(serde_json::json!({"scenario":name,"result":result,"costs":harness.costs()}));
        let report = serde_json::json!({"qualification":false,"profile":"setup-measurement","fuel_per_call":1_000_000_000u64,"engine":"wasmtime","engine_version":"50.0.0","arch":std::env::consts::ARCH,"os":std::env::consts::OS,"scenarios":scenarios});
        std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
