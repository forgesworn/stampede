//! Local real-game qualification. No listener, owner keys or WASI linker.
#[allow(dead_code)]
mod support;
use support::*;

#[test]
fn real_race_matches_native_and_seven_restore_continuations() {
    let report = compare_race(Harness::get());
    if let Some(path) = std::env::var_os("RACE_AUTHORITY_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    println!("{report}");
}

#[test]
fn long_built_in_races_match_native_through_bank_heights_and_restores() {
    use race_session::proto::{encode, ToHost, PROTO_VERSION};
    for track in [race_core::TrackId::Savanna, race_core::TrackId::Oasis] {
        let config = AuthorityConfig {
            laps: 1,
            bots: 6,
            auto_start: false,
            ..AuthorityConfig::default()
        }
        .to_bytes()
        .unwrap();
        let mut native = RaceAuthority::init(&config, 42).unwrap();
        let harness = Harness::get();
        let mut guest = harness.init(&config);
        for tick in 1..=6000 {
            let time = n::Time::at_tick(tick, 60).unwrap();
            let mut events = Vec::new();
            match tick {
                1 => {
                    for (id, name) in [(17, "Racer A"), (29, "Racer B")] {
                        events.push(n::Event::Joined(n::Player {
                            id: n::PlayerId(id),
                            identity: Some([id as u8; 32]),
                            metadata: PeerInfo {
                                local: false,
                                rtt_ms: Some(40),
                            }
                            .to_bytes(),
                        }));
                        for message in [
                            ToHost::Hello {
                                proto: PROTO_VERSION,
                                name: name.into(),
                                character: 0,
                            },
                            ToHost::SetReady(true),
                        ] {
                            events.push(n::Event::Message {
                                player: n::PlayerId(id),
                                delivery: n::Delivery::Reliable,
                                bytes: encode(&message),
                            });
                        }
                    }
                }
                3 => events.push(n::Event::Control(
                    Control::SetTrack(track).to_bytes().unwrap(),
                )),
                5 | 5201 => events.push(n::Event::Control(Control::StartRace.to_bytes().unwrap())),
                1000 => events.push(n::Event::Control(
                    Control::PeerRtt {
                        player: 17,
                        rtt_ms: Some(90),
                    }
                    .to_bytes()
                    .unwrap(),
                )),
                1200 => events.push(n::Event::Left {
                    player: n::PlayerId(29),
                    reason: "left".into(),
                }),
                1400 => events.push(n::Event::Message {
                    player: n::PlayerId(17),
                    delivery: n::Delivery::Reliable,
                    bytes: vec![255],
                }),
                5200 => events.push(n::Event::Control(Control::ToLobby.to_bytes().unwrap())),
                _ => {}
            }
            for event in events {
                assert_eq!(
                    guest.event(time, event.clone()),
                    native.on_event(time, event).unwrap(),
                    "event {track:?} tick {tick}"
                );
            }
            assert_eq!(
                guest.tick(time),
                native.tick(time).unwrap(),
                "commands {track:?} tick {tick}"
            );
            let image = native.checkpoint().unwrap();
            assert_image(
                &guest.checkpoint(),
                &image,
                &format!("long checkpoint {track:?}"),
                tick,
            );
            assert_eq!(
                guest.hash(),
                native.state_hash().unwrap(),
                "hash {track:?} tick {tick}"
            );
            if tick == 300 {
                guest = harness.restore(&config, &image);
                native = RaceAuthority::restore(&config, &image).unwrap();
                assert_image(&guest.checkpoint(), &image, "fresh long restore", tick);
            }
        }
    }
}

fn assert_terminal(guest: &mut Guest, config: &[u8]) {
    assert!(guest
        .call(Operation::Checkpoint, |b, s| b
            .fray_authority_authority()
            .call_checkpoint(s))
        .is_err());
    assert!(guest
        .call(Operation::StateHash, |b, s| b
            .fray_authority_authority()
            .call_state_hash(s))
        .is_err());
    assert!(guest
        .call(Operation::Init, |b, s| b
            .fray_authority_authority()
            .call_init(s, config, 42))
        .is_err());
    let saved = RaceAuthority::init(config, 42)
        .unwrap()
        .checkpoint()
        .unwrap();
    assert!(guest
        .call(Operation::Restore, |b, s| b
            .fray_authority_authority()
            .call_restore(s, config, &saved))
        .is_err());
    let time = wire_time(n::Time::at_tick(1, 60).unwrap());
    assert!(guest
        .call(Operation::Tick, |b, s| b
            .fray_authority_authority()
            .call_tick(s, time))
        .is_err());
    let event = w::Event::Control(Control::StartRace.to_bytes().unwrap());
    assert!(guest
        .call(Operation::Event, |b, s| b
            .fray_authority_authority()
            .call_on_event(s, time, &event))
        .is_err());
}

#[test]
fn component_lifecycle_and_conversion_failures_are_terminal() {
    let harness = Harness::get();
    let config = AuthorityConfig::default().to_bytes().unwrap();
    let mut before_init = harness.fresh();
    assert_terminal(&mut before_init, &config);
    let mut bad_init = harness.fresh();
    assert_eq!(
        bad_init
            .call(Operation::Init, |b, s| b
                .fray_authority_authority()
                .call_init(s, &[], 42))
            .unwrap_err()
            .code,
        w::ErrorCode::InvalidConfig
    );
    assert_terminal(&mut bad_init, &config);
    let future = AuthorityConfig {
        schema: 99,
        ..AuthorityConfig::default()
    }
    .to_bytes()
    .unwrap();
    for (bytes, code) in [
        (future, w::ErrorCode::UnsupportedVersion),
        (vec![0; 17 * 1024], w::ErrorCode::InvalidConfig),
    ] {
        let mut guest = harness.fresh();
        assert_eq!(
            guest
                .call(Operation::Init, |b, s| b
                    .fray_authority_authority()
                    .call_init(s, &bytes, 42))
                .unwrap_err()
                .code,
            code
        );
        assert_terminal(&mut guest, &config);
    }
    let mut repeated = harness.init(&config);
    assert!(repeated
        .call(Operation::Init, |b, s| b
            .fray_authority_authority()
            .call_init(s, &config, 42))
        .is_err());
    assert_terminal(&mut repeated, &config);
    for length in [0, 31, 33] {
        let mut guest = harness.init(&config);
        let event = w::Event::Joined(w::Player {
            id: 1,
            identity: Some(vec![0; length]),
            metadata: PeerInfo {
                local: false,
                rtt_ms: None,
            }
            .to_bytes(),
        });
        assert!(guest
            .call(Operation::Event, |b, s| b
                .fray_authority_authority()
                .call_on_event(
                    s,
                    wire_time(n::Time::at_tick(1, 60).unwrap()),
                    &event
                ))
            .is_err());
        assert_terminal(&mut guest, &config);
    }
    let mut order = harness.init(&config);
    assert!(order
        .call(Operation::Tick, |b, s| b
            .fray_authority_authority()
            .call_tick(s, wire_time(n::Time::at_tick(2, 60).unwrap())))
        .is_err());
    assert_terminal(&mut order, &config);
}

#[test]
fn malformed_restore_and_config_mismatch_are_terminal() {
    let harness = Harness::get();
    let config = AuthorityConfig::default().to_bytes().unwrap();
    let mut good = harness.init(&config);
    good.tick(n::Time::at_tick(1, 60).unwrap());
    let saved = good.checkpoint();
    let other = AuthorityConfig {
        bots: 1,
        ..AuthorityConfig::default()
    }
    .to_bytes()
    .unwrap();
    let mut wrong_magic = saved.clone();
    wrong_magic[0] ^= 1;
    for (cfg, checkpoint) in [
        (&config, &saved[..saved.len() - 1]),
        (&config, wrong_magic.as_slice()),
        (&other, saved.as_slice()),
    ] {
        let mut guest = harness.fresh();
        assert_eq!(
            guest
                .call(Operation::Restore, |b, s| b
                    .fray_authority_authority()
                    .call_restore(s, cfg, checkpoint))
                .unwrap_err()
                .code,
            w::ErrorCode::InvalidCheckpoint
        );
        assert_terminal(&mut guest, &config);
    }
    let mut live = harness.init(&config);
    assert!(live
        .call(Operation::Restore, |b, s| b
            .fray_authority_authority()
            .call_restore(s, &config, &saved))
        .is_err());
    assert_terminal(&mut live, &config);
}

#[test]
fn built_in_tracks_identity_and_malformed_messages_match_native() {
    use race_session::proto::{encode, ToHost, PROTO_VERSION};
    for track in [race_core::TrackId::Savanna, race_core::TrackId::Oasis] {
        let config = AuthorityConfig {
            track,
            bots: 3,
            ..AuthorityConfig::default()
        }
        .to_bytes()
        .unwrap();
        let mut native = RaceAuthority::init(&config, 42).unwrap();
        let mut guest = Harness::get().init(&config);
        let id = n::PlayerId(u64::MAX);
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
                    n::Event::Message {
                        player: id,
                        delivery: n::Delivery::Reliable,
                        bytes: vec![255; 7],
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
                assert_eq!(
                    guest.event(now, event.clone()),
                    native.on_event(now, event).unwrap(),
                    "event {track:?} tick {tick}"
                );
            }
            assert_eq!(
                guest.tick(now),
                native.tick(now).unwrap(),
                "commands {track:?} tick {tick}"
            );
            assert_image(
                &guest.checkpoint(),
                &native.checkpoint().unwrap(),
                &format!("checkpoint {track:?}"),
                tick,
            );
            assert_eq!(
                guest.hash(),
                native.state_hash().unwrap(),
                "hash {track:?} tick {tick}"
            );
        }
    }
}

#[test]
fn a_cold_selection_is_prepared_before_play_and_after_lobby_restore() {
    use race_core::TrackId;
    use race_session::proto::{encode, ToHost, PROTO_VERSION};
    for custom in [false, true] {
        let config = AuthorityConfig {
            track: if custom {
                TrackId::Custom
            } else {
                TrackId::Savanna
            },
            custom: if custom {
                legacy::track().to_bytes()
            } else {
                vec![]
            },
            laps: 1,
            bots: 3,
            auto_start: false,
            ..AuthorityConfig::default()
        }
        .to_bytes()
        .unwrap();
        let mut native = RaceAuthority::init(&config, 42).unwrap();
        let mut guests = vec![Harness::get().init(&config)];
        let player = n::PlayerId(17);
        for tick in 1..=360 {
            let now = n::Time::at_tick(tick, 60).unwrap();
            let events = match tick {
                1 => vec![
                    n::Event::Joined(n::Player {
                        id: player,
                        identity: None,
                        metadata: PeerInfo {
                            local: false,
                            rtt_ms: Some(40),
                        }
                        .to_bytes(),
                    }),
                    n::Event::Message {
                        player,
                        delivery: n::Delivery::Reliable,
                        bytes: encode(&ToHost::Hello {
                            proto: PROTO_VERSION,
                            name: "Racer".into(),
                            character: 0,
                        }),
                    },
                ],
                2 => vec![n::Event::Control(
                    Control::SetTrack(TrackId::Oasis).to_bytes().unwrap(),
                )],
                3 => vec![n::Event::Control(Control::StartRace.to_bytes().unwrap())],
                _ => vec![],
            };
            for event in events {
                let expected = native.on_event(now, event.clone()).unwrap();
                for guest in &mut guests {
                    assert_eq!(
                        guest.event(now, event.clone()),
                        expected,
                        "selection event {tick}"
                    );
                }
            }
            let expected = native.tick(now).unwrap();
            let checkpoint = native.checkpoint().unwrap();
            for guest in &mut guests {
                assert_eq!(guest.tick(now), expected, "selection tick {tick}");
                assert_image(
                    &guest.checkpoint(),
                    &checkpoint,
                    "selection checkpoint",
                    tick,
                );
                assert_eq!(guest.hash(), native.state_hash().unwrap());
            }
            if tick == 2 {
                let mut restored = Harness::get().restore(&config, &checkpoint);
                assert_image(&restored.checkpoint(), &checkpoint, "lobby restore", tick);
                guests.push(restored);
            }
        }
    }
}
