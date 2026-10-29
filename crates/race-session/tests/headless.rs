use race_core::RacePhase;
use race_session::{HostAuthority, HostConfig, HostPhase, NullHostTransport};

fn config() -> HostConfig {
    HostConfig {
        bots: 8,
        laps: 1,
        seed: 42,
        auto_start: false,
        ..HostConfig::default()
    }
}

#[test]
fn eight_bots_complete_without_a_local_player() {
    let mut host = HostAuthority::new_headless(Box::new(NullHostTransport), config());
    assert_eq!(host.human_count(), 0);
    assert_eq!(host.effective_bots(), 8);
    assert!(host
        .lobby_view()
        .seats
        .iter()
        .all(|seat| seat.bot && !seat.host));
    assert!(host.start_race(0));
    let mut completed_at = None;
    for tick in 1..=36_000u64 {
        let now = (tick * 1000).div_ceil(60);
        host.update(now);
        if host.phase() == HostPhase::Results {
            completed_at = Some(now);
            break;
        }
    }
    let completed_at = completed_at.expect("eight-bot race completed within ten game minutes");
    let world = host.world().unwrap();
    assert_eq!(world.phase, RacePhase::Finished);
    assert_eq!(world.racers.len(), 8);
    assert!(world.racers.iter().all(|racer| racer.finished_at.is_some()));
    assert!(host
        .results()
        .unwrap()
        .rows
        .iter()
        .all(|row| row.bot && row.time.is_some()));
    assert_eq!(host.human_count(), 0);
    for offset in 1..=600u64 {
        host.update(completed_at + (offset * 1000).div_ceil(60));
    }
    assert_eq!(
        host.phase(),
        HostPhase::Results,
        "empty readiness cannot start a rematch"
    );
}

#[test]
fn ordinary_constructor_keeps_the_existing_human_requirement_and_bot_limit() {
    let mut host = HostAuthority::new(Box::new(NullHostTransport), config());
    assert_eq!(host.effective_bots(), 7);
    assert!(host.set_bots(8));
    assert_eq!(host.effective_bots(), 7);
    assert!(!host.can_start());
    assert!(!host.start_race(0));
}

#[test]
fn the_last_departing_player_becomes_a_bot_and_the_dedicated_race_completes() {
    use race_core::{Input, RacerKind};
    use race_session::{ClientSession, LinkPath, LoopClock, LoopbackConfig, LoopbackHub};
    use std::sync::atomic::Ordering;

    let (clock, time) = LoopClock::manual();
    let hub = LoopbackHub::new(clock, 42);
    let mut host = HostAuthority::new_headless(Box::new(hub.host()), config());
    assert!(host.set_bots(8));
    let mut client = ClientSession::new(
        Box::new(hub.connect_via(LoopbackConfig::PERFECT, LinkPath::Direct)),
        "Test player",
        0,
    );
    for now in 0..100 {
        time.store(now, Ordering::Relaxed);
        client.update(now, Input::default());
        host.update(now);
    }
    assert_eq!(host.human_count(), 1);
    assert!(host.start_race(100));
    client.leave();
    time.store(101, Ordering::Relaxed);
    host.update(101);
    assert_eq!(host.human_count(), 0);
    assert_eq!(host.phase(), HostPhase::Racing);
    assert!(matches!(host.world().unwrap().kind(0), RacerKind::Bot(_)));
    for tick in 1..=36_000u64 {
        let now = 101 + (tick * 1000).div_ceil(60);
        time.store(now, Ordering::Relaxed);
        host.update(now);
        if host.phase() == HostPhase::Results {
            break;
        }
    }
    assert_eq!(host.phase(), HostPhase::Results);
    assert!(host
        .world()
        .unwrap()
        .racers
        .iter()
        .all(|r| r.finished_at.is_some()));
}

#[test]
fn dedicated_server_waits_for_players_even_when_auto_start_is_enabled() {
    let mut cfg = config();
    cfg.auto_start = true;
    let mut host = HostAuthority::new_headless(Box::new(NullHostTransport), cfg);
    host.update(10_000);
    assert_eq!(host.phase(), HostPhase::Lobby);
    assert!(host.world().is_none());
}

#[test]
fn eight_authenticated_clients_join_the_headless_quic_host() {
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_stampede-headless"))
        .args(["--smoke", "--players", "8"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(summary["completed"], true);
    assert_eq!(summary["authenticated_clients"], 8);
    assert_eq!(summary["host_player_seats"], 0);
    assert_eq!(summary["transport"], "localhost QUIC");
}
