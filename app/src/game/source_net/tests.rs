//! A host and a client, both through `NetRace`, run a short race over the
//! loopback hub (virtual clock, 40 ms each way) and agree on the results.

use super::*;
use crate::online::{OnlineSession, OnlineSlot};
use race_core::{bot_input, BotLevel, Input, RacePhase, TrackId};
use race_session::{ClientPhase, ClientSession, HostAuthority, HostConfig, LoopClock, LoopbackConfig, LoopbackHub};
use std::sync::atomic::Ordering;

fn pilot(s: &OnlineSession) -> Input {
    match s.me().view() {
        Some(v) if !(v.world.phase == RacePhase::Countdown && v.hud.countdown > 0.25) => bot_input(&v.world, v.you, BotLevel::Normal),
        _ => Input::default(),
    }
}

fn seated(s: &OnlineSlot) -> bool {
    s.with(|s| matches!(s.me().phase(), ClientPhase::Lobby) && s.me().lobby().is_some()).unwrap_or(false)
}

#[test]
fn host_and_client_complete_a_one_lap_race_with_two_bots_and_agree() {
    let (clock, t) = LoopClock::manual();
    t.store(10_000, Ordering::Relaxed);
    let hub = LoopbackHub::new(clock.clone(), 5);
    let cfg = HostConfig {
        track: TrackId::Savanna,
        laps: 1,
        bots: 2,
        bot_level: BotLevel::Normal,
        seed: 11,
        auto_start: true,
    };
    let mut host = HostAuthority::new(Box::new(hub.host()), cfg);
    assert!(host.set_le_mans_start(true));
    let me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Host", 0);
    let guest = ClientSession::new(Box::new(hub.connect(LoopbackConfig { latency_ms: 40, jitter_ms: 4, lossy_loss: 0.02 })), "Guest", 3);
    let (a, b) = (OnlineSlot::new(), OnlineSlot::new());
    a.put(OnlineSession::from_parts(me, Some(host), clock.clone()));
    b.put(OnlineSession::from_parts(guest, None, clock.clone()));
    let (mut ra, mut rb): (Option<NetRace>, Option<NetRace>) = (None, None);
    let mut readied = false;
    let mut watched_while_waiting = false;
    for _ in 0..(5 * 60 * 60) {
        t.fetch_add(16, Ordering::Relaxed);
        for (s, r) in [(&a, &mut ra), (&b, &mut rb)] {
            if let Some(src) = r.as_mut() {
                let inp = s.with(|s| pilot(s)).unwrap();
                src.advance(0.016, &mut || inp);
                let _ = src.drain_events();
                if src.net_status().is_some_and(|n| n.waiting) && !src.ready_to_finish() {
                    let v = src.view();
                    let seats = crate::game::spectate::candidates(&v, src.you, true);
                    if let Some(watch) = crate::game::spectate::target(&seats, src.you) {
                        assert_ne!(watch, src.you);
                        assert!(v.racers[watch].finished_at.is_none());
                        watched_while_waiting = true;
                    }
                }
            } else {
                s.with(|s| s.pump(Input::default()));
            }
        }
        if !readied && seated(&a) && seated(&b) {
            for s in [&a, &b] {
                s.with(|s| s.client_command(|client|client.set_ready(true)));
            }
            readied = true;
        }
        for (s, r) in [(&a, &mut ra), (&b, &mut rb)] {
            if r.is_none() && s.with(|s| matches!(s.me().phase(), ClientPhase::Racing) && s.me().view().is_some()).unwrap_or(false) {
                *r = Some(NetRace::new(s).expect("race view"));
            }
        }
        if let (Some(x), Some(y)) = (&ra, &rb) {
            if x.ready_to_finish() && y.ready_to_finish() {
                break;
            }
        }
    }
    let (ra, rb) = (
        ra.expect("host race started"),
        rb.expect("guest race started"),
    );
    assert!(
        ra.ready_to_finish() && rb.ready_to_finish(),
        "both races reach their results"
    );
    assert!(
        ra.view().cfg.le_mans && rb.view().cfg.le_mans,
        "the shared start rule reaches both clients"
    );
    let res_a = a
        .with(|s| s.me().results().cloned())
        .flatten()
        .expect("host results");
    let res_b = b
        .with(|s| s.me().results().cloned())
        .flatten()
        .expect("guest results");

    assert_eq!(res_a, res_b, "both sides show the same results");
    assert_eq!(res_a.rows.len(), 4, "two people and two bots");
    assert!(res_a.rows.iter().filter(|r| !r.bot).all(|r| r.time.is_some()), "both people finished");
    assert!(watched_while_waiting, "a finished player can follow a racer before the shared results arrive");
    // The adapter maps the shadow world as a race view.
    assert_eq!(rb.view().racers.len(), 4);
    assert_eq!(rb.local_seat(), Some(rb.you));
    assert!(rb.net_status().is_some_and(|n| !n.host));
    assert!(ra.net_status().is_some_and(|n| n.host));
}

/// One frame of the standby phone (pumped from its slot) and of the client's
/// race source.
fn armed_frame(standby: &OnlineSlot, client: &OnlineSlot, race: &mut NetRace) {
    standby.with(|s| {
        let input = s.bot_drive();
        s.pump(input);
    });
    let input = client.with(|s| pilot(s)).unwrap();
    race.advance(0.016, &mut || input);
    let _ = race.drain_events();
    std::thread::sleep(std::time::Duration::from_millis(1));
}

/// The race screen's source runs unchanged on an armed client (real QUIC):
/// it reads the armed route's race through the slot, and keeps racing when
/// the host is killed and the standby phone takes over.
#[test]
fn an_armed_clients_race_source_races_on_through_a_takeover() {
    use crate::online::tests::{armed_room, run_until, Seen};
    use crate::online::MigrationState;
    use std::time::{Duration, Instant};
    let (mut phones, ..) = armed_room(&mut Seen::default());
    assert!(phones[0].start_race());
    run_until(&mut phones, &[true; 3], 15, "everyone racing", |p| {
        p.iter()
            .all(|x| matches!(x.me().phase(), ClientPhase::Racing) && x.me().view().is_some())
    });
    let (client, standby) = (phones.pop().unwrap(), phones.pop().unwrap());
    let (b, c) = (OnlineSlot::new(), OnlineSlot::new());
    b.put(standby);
    c.put(client);
    let mut race = NetRace::new(&c).expect("the armed client's race view");
    // The killed host stays in `phones`, never pumped again.
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(1) {
        phones[0].pump(Input::default());
        armed_frame(&b, &c, &mut race);
    }
    assert!(race
        .net_status()
        .is_some_and(|n| !n.host && n.lost.is_none()));
    let t = Instant::now();
    while !c
        .with(|s| {
            matches!(
                s.migration_state(),
                MigrationState::Resumed | MigrationState::UnprotectedAfterTakeover
            )
        })
        .unwrap()
    {
        armed_frame(&b, &c, &mut race);
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "no takeover: {:?}",
            c.with(|s| (s.migration_state(), s.dead_reason()))
        );
    }
    let tick = race.shadow.world.tick;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(1) {
        armed_frame(&b, &c, &mut race);
    }
    assert!(
        race.shadow.world.tick > tick,
        "the race moves on after the takeover"
    );
    assert!(!race.ready_to_finish(), "still racing");
    assert!(race.net_status().is_some_and(|n| n.lost.is_none()));
    assert!(b.with(|s| s.is_host()).unwrap(), "the standby phone hosts");
}
