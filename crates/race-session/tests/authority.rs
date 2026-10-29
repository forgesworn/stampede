#![cfg(feature = "authority")]
use bincode::Options;
use fray_session::authority::{
    Authority, Command, Delivery, ErrorCode, Event, Player, PlayerId, Target, Time,
};
use race_core::{BotLevel, TrackId};
use race_session::authority::{AuthorityConfig, Control, PeerInfo, RaceAuthority};
use race_session::proto::{encode, ToHost, PROTO_VERSION};
use serde::{Deserialize, Serialize};

fn time(tick: u64) -> Time {
    Time::at_tick(tick, 60).unwrap()
}
fn joined(id: u64) -> Event {
    Event::Joined(Player {
        id: PlayerId(id),
        identity: None,
        metadata: PeerInfo {
            local: false,
            rtt_ms: Some(40),
        }
        .to_bytes(),
    })
}
fn hello(id: u64) -> Event {
    Event::Message {
        player: PlayerId(id),
        delivery: Delivery::Reliable,
        bytes: encode(&ToHost::Hello {
            proto: PROTO_VERSION,
            name: format!("Test racer {id}"),
            character: id as u8,
        }),
    }
}
fn step(
    authority: &mut RaceAuthority,
    tick: u64,
    events: Vec<Event>,
) -> Result<Vec<Command>, fray_session::authority::AuthorityError> {
    let mut commands = Vec::new();
    for event in events {
        commands.extend(authority.on_event(time(tick), event)?);
    }
    commands.extend(authority.tick(time(tick))?);
    Ok(commands)
}
#[derive(Deserialize, Serialize)]
struct Member {
    _player: u64,
    _logical: u32,
    _identity: Option<[u8; 32]>,
    _info: PeerInfo,
}
#[derive(Deserialize, Serialize)]
struct Binding {
    _config: [u8; 32],
    _content: [u8; 32],
}
#[derive(Deserialize, Serialize)]
struct Image {
    _schema: u32,
    _seed: u64,
    _tick: u64,
    _next: u32,
    _config: Vec<u8>,
    _members: Vec<Member>,
    _binding: Binding,
    game: Vec<u8>,
}
fn game_image(bytes: &[u8]) -> Vec<u8> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_big_endian()
        .reject_trailing_bytes()
        .deserialize::<Image>(&bytes[b"stampede/fray-authority/v1\0".len()..])
        .unwrap()
        .game
}

#[test]
fn restore_continues_membership_rtt_and_commands_without_transport_services() {
    let config = AuthorityConfig::default().to_bytes().unwrap();
    let mut original = RaceAuthority::init(&config, 42).unwrap();
    step(&mut original, 1, vec![joined(901), hello(901)]).unwrap();
    step(
        &mut original,
        2,
        vec![Event::Control(
            Control::PeerRtt {
                player: 901,
                rtt_ms: Some(130),
            }
            .to_bytes()
            .unwrap(),
        )],
    )
    .unwrap();
    let bytes = original.checkpoint().unwrap();
    let mut restored = RaceAuthority::restore(&config, &bytes).unwrap();
    assert_eq!(restored.checkpoint().unwrap(), bytes);
    assert_eq!(
        restored.state_hash().unwrap(),
        original.state_hash().unwrap()
    );
    for tick in 3..120 {
        let events = if tick == 5 {
            vec![joined(712), hello(712)]
        } else {
            vec![]
        };
        assert_eq!(
            step(&mut original, tick, events.clone()).unwrap(),
            step(&mut restored, tick, events).unwrap()
        );
        assert_eq!(
            restored.checkpoint().unwrap(),
            original.checkpoint().unwrap()
        );
    }
}

#[test]
fn config_schema_magic_and_binding_are_checked() {
    let config = AuthorityConfig::default();
    let bytes = config.to_bytes().unwrap();
    let original = RaceAuthority::init(&bytes, 42).unwrap();
    let saved = original.checkpoint().unwrap();
    let mut other = config.clone();
    other.bots = 1;
    assert!(RaceAuthority::restore(&other.to_bytes().unwrap(), &saved).is_err());
    let mut wrong = saved.clone();
    wrong[0] ^= 1;
    assert!(RaceAuthority::restore(&bytes, &wrong).is_err());
    assert!(RaceAuthority::restore(&bytes, &saved[..saved.len() - 1]).is_err());
    let mut wrong_clock = saved.clone();
    let tick_offset = b"stampede/fray-authority/v1\0".len() + 4 + 8;
    wrong_clock[tick_offset..tick_offset + 8].copy_from_slice(&1u64.to_be_bytes());
    assert!(RaceAuthority::restore(&bytes, &wrong_clock).is_err());
    let mut future = config;
    future.schema = 99;
    assert_eq!(
        RaceAuthority::init(&future.to_bytes().unwrap(), 42)
            .err()
            .unwrap()
            .code,
        ErrorCode::UnsupportedVersion
    );
    assert!(RaceAuthority::init(&vec![0; 17 * 1024], 42).is_err());
}

#[test]
fn restore_rejects_a_canonical_image_relabelled_with_another_immutable_config() {
    let original = AuthorityConfig::default();
    let config = original.to_bytes().unwrap();
    let mut other = original;
    other.auto_start = !other.auto_start;
    let alternate = RaceAuthority::init(&other.to_bytes().unwrap(), 42)
        .unwrap()
        .checkpoint()
        .unwrap();
    let magic = b"stampede/fray-authority/v1\0";
    let opts = || {
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_big_endian()
            .reject_trailing_bytes()
    };
    let mut image: Image = opts().deserialize(&alternate[magic.len()..]).unwrap();
    image._config = config.clone();
    let mut relabelled = magic.to_vec();
    relabelled.extend(opts().serialize(&image).unwrap());
    assert_eq!(
        RaceAuthority::restore(&config, &relabelled)
            .err()
            .unwrap()
            .code,
        ErrorCode::InvalidCheckpoint
    );
}

#[test]
fn stale_controller_selections_do_not_end_the_room() {
    let config = AuthorityConfig::default().to_bytes().unwrap();
    let mut authority = RaceAuthority::init(&config, 42).unwrap();
    step(
        &mut authority,
        1,
        vec![
            Event::Control(Control::Kick(99).to_bytes().unwrap()),
            Event::Control(
                Control::PeerRtt {
                    player: 99,
                    rtt_ms: Some(100),
                }
                .to_bytes()
                .unwrap(),
            ),
        ],
    )
    .unwrap();
    step(&mut authority, 2, vec![joined(1), hello(1)]).unwrap();
}

#[test]
fn ordering_faults_and_invalid_host_control_are_terminal() {
    let config = AuthorityConfig::default().to_bytes().unwrap();
    let mut bad = RaceAuthority::init(&config, 42).unwrap();
    assert!(bad.tick(time(2)).is_err());
    assert!(bad.tick(time(1)).is_err());
    let mut unfinished = RaceAuthority::init(&config, 42).unwrap();
    unfinished.on_event(time(1), joined(70)).unwrap();
    assert!(unfinished.checkpoint().is_err());
    assert!(unfinished.tick(time(1)).is_err());
    let mut control = RaceAuthority::init(&config, 42).unwrap();
    assert!(control
        .on_event(time(1), Event::Control(vec![255; 8]))
        .is_err());
    assert!(control.tick(time(1)).is_err());
}

#[test]
fn malformed_player_messages_are_expected_input_and_step_outputs_are_bounded() {
    let config = AuthorityConfig::default().to_bytes().unwrap();
    let mut authority = RaceAuthority::init(&config, 42).unwrap();
    step(
        &mut authority,
        1,
        vec![
            joined(1),
            hello(1),
            Event::Message {
                player: PlayerId(1),
                delivery: Delivery::Reliable,
                bytes: vec![255; 7],
            },
        ],
    )
    .unwrap();
    assert!(step(&mut authority, 2, vec![]).is_ok());
    let pings = (0..300)
        .map(|id| Event::Message {
            player: PlayerId(1),
            delivery: Delivery::Reliable,
            bytes: encode(&ToHost::Ping { id, client_t: 0 }),
        })
        .collect();
    assert!(step(&mut authority, 3, pings).is_err());
    assert!(authority.tick(time(3)).is_err());
}

#[cfg(feature = "native")]
#[allow(dead_code)]
#[path = "../examples/authority_baseline.rs"]
mod legacy;

#[cfg(feature = "native")]
#[test]
fn a_valid_custom_selection_after_start_is_a_harmless_refusal() {
    let config = AuthorityConfig {
        auto_start: false,
        ..AuthorityConfig::default()
    }
    .to_bytes()
    .unwrap();
    let mut actual = RaceAuthority::init(&config, 42).unwrap();
    let mut expected = RaceAuthority::init(&config, 42).unwrap();
    for authority in [&mut actual, &mut expected] {
        step(authority, 1, vec![]).unwrap();
        step(
            authority,
            2,
            vec![Event::Control(Control::StartRace.to_bytes().unwrap())],
        )
        .unwrap();
    }
    let stale = Event::Control(
        Control::SetCustomTrack(legacy::track().to_bytes())
            .to_bytes()
            .unwrap(),
    );
    assert_eq!(
        step(&mut actual, 3, vec![stale]).unwrap(),
        step(&mut expected, 3, vec![]).unwrap()
    );
    assert_eq!(actual.checkpoint().unwrap(), expected.checkpoint().unwrap());
    assert_eq!(
        step(&mut actual, 4, vec![]).unwrap(),
        step(&mut expected, 4, vec![]).unwrap()
    );
}

#[cfg(feature = "native")]
#[test]
fn adapter_matches_the_pre_extraction_native_trace_and_restores_through_the_race() {
    use race_session::{HostNetEvent, LinkPath};
    let track = legacy::track();
    let config = AuthorityConfig {
        schema: 1,
        track: TrackId::Custom,
        laps: 1,
        bots: 3,
        bot_level: BotLevel::Hard,
        auto_start: false,
        headless: true,
        le_mans: true,
        custom: track.to_bytes(),
        battle: None,
    }
    .to_bytes()
    .unwrap();
    let mut authority = RaceAuthority::init(&config, 42).unwrap();
    let mut restored = Vec::<RaceAuthority>::new();
    let root = legacy::run(
        |tick, actions, events, effects, expected_image| {
            let mut inputs = Vec::new();
            for action in actions {
                assert_eq!(*action, "start");
                inputs.push(Event::Control(Control::StartRace.to_bytes().unwrap()));
            }
            inputs.extend(events.iter().map(|event| {
                match event {
                    HostNetEvent::Joined { peer, path } => Event::Joined(Player {
                        id: PlayerId(*peer as u64),
                        identity: None,
                        metadata: PeerInfo {
                            local: *path == LinkPath::Local,
                            rtt_ms: Some(40),
                        }
                        .to_bytes(),
                    }),
                    HostNetEvent::Left { peer } => Event::Left {
                        player: PlayerId(*peer as u64),
                        reason: String::new(),
                    },
                    HostNetEvent::Reliable { peer, bytes } => Event::Message {
                        player: PlayerId(*peer as u64),
                        delivery: Delivery::Reliable,
                        bytes: bytes.clone(),
                    },
                    HostNetEvent::Lossy { peer, bytes } => Event::Message {
                        player: PlayerId(*peer as u64),
                        delivery: Delivery::Lossy,
                        bytes: bytes.clone(),
                    },
                }
            }));
            let expected: Vec<_> = effects
                .iter()
                .map(|effect| match effect {
                    legacy::Effect::Reliable(p, b) => Command::Send {
                        target: Target::Player(PlayerId(*p as u64)),
                        delivery: Delivery::Reliable,
                        bytes: b.clone(),
                    },
                    legacy::Effect::Lossy(p, b) => Command::Send {
                        target: Target::Player(PlayerId(*p as u64)),
                        delivery: Delivery::Lossy,
                        bytes: b.clone(),
                    },
                    legacy::Effect::Kick(p) => Command::Disconnect {
                        player: PlayerId(*p as u64),
                        reason: "kicked".into(),
                    },
                    legacy::Effect::Disconnect(p) => Command::Disconnect {
                        player: PlayerId(*p as u64),
                        reason: "disconnected".into(),
                    },
                    legacy::Effect::ConfirmTimeout(p) => Command::Disconnect {
                        player: PlayerId(*p as u64),
                        reason: "confirmation timeout".into(),
                    },
                    legacy::Effect::Open(b) | legacy::Effect::OpenKeepingToken(b) => {
                        Command::SetOpen(*b)
                    }
                    legacy::Effect::Shutdown => panic!("the fixture does not shut down its host"),
                })
                .collect();
            let actual = step(&mut authority, tick, inputs.clone()).unwrap();
            assert_eq!(actual, expected, "commands at {tick}");
            let checkpoint = authority.checkpoint().unwrap();
            assert_eq!(
                game_image(&checkpoint),
                expected_image,
                "native game bytes at {tick}"
            );
            for copy in &mut restored {
                assert_eq!(
                    step(copy, tick, inputs.clone()).unwrap(),
                    actual,
                    "restored commands at {tick}"
                );
                assert_eq!(
                    copy.checkpoint().unwrap(),
                    checkpoint,
                    "restored bytes at {tick}"
                );
                assert_eq!(copy.state_hash().unwrap(), authority.state_hash().unwrap());
            }
            if [1, 50, 299, 330, 500, 1424, 1604].contains(&tick) {
                restored.push(RaceAuthority::restore(&config, &checkpoint).unwrap());
            }
        },
        false,
    );
    assert_eq!(
        root.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        "d3be4cb48203b16d436cfb460f3acdb16cdb3f4c9430cae356e649848c3c0b6a"
    );
    assert_eq!(restored.len(), 7);
}
