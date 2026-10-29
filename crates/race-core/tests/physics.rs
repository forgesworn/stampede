//! The driving model (spec 4.3), through the pure `Racer::integrate`.

mod common;
use common::*;
use race_core::math::{flat, DT};
use race_core::*;

#[test]
fn zero_to_top_speed_in_about_three_seconds() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 20.0, 0.0, 0); // Medium
    let mut ticks = 0;
    while r.speed < t.top_speed * 0.99 && ticks < 600 {
        r.integrate(&Input::go(), &tr, &t);
        ticks += 1;
    }
    let time = secs(ticks);
    assert!((2.5..=3.5).contains(&time), "0 -> top took {time:.2} s");
    // And it holds there, not above.
    drive(&mut r, &tr, &t, Input::go(), 120);
    assert!(close(r.speed, t.top_speed, 0.05), "speed {}", r.speed);
}

#[test]
fn classes_change_acceleration_and_top_speed() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut light = racer_at(&tr, 20.0, -4.0, 1);
    let mut heavy = racer_at(&tr, 20.0, 4.0, 4);
    drive(&mut light, &tr, &t, Input::go(), 60);
    drive(&mut heavy, &tr, &t, Input::go(), 60);
    assert!(
        light.speed > heavy.speed,
        "light {} heavy {}",
        light.speed,
        heavy.speed
    );
    drive(&mut light, &tr, &t, Input::go(), 600);
    drive(&mut heavy, &tr, &t, Input::go(), 600);
    assert!(close(light.speed, t.top_speed * 0.97, 0.1));
    assert!(close(heavy.speed, t.top_speed * 1.03, 0.1));
}

#[test]
fn held_bitcoin_adds_top_speed() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    r.coins = 10;
    drive(&mut r, &tr, &t, Input::go(), 600);
    let want = t.top_speed * (1.0 + 10.0 * t.coin_bonus);
    assert!(close(r.speed, want, 0.1), "speed {} want {want}", r.speed);
    // Max 10 counts.
    r.coins = 14;
    drive(&mut r, &tr, &t, Input::go(), 300);
    assert!(close(r.speed, want, 0.1));
}

#[test]
fn acceleration_is_permanent_even_with_brake_or_zero_throttle() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    drive(&mut r, &tr, &t, Input { brake: true, ..Default::default() }, 240);
    assert!(close(r.speed, t.top_speed, 0.05));
    set_speed(&mut r, 20.0);
    drive(&mut r, &tr, &t, Input::default(), 60);
    assert!(r.speed > 20.0);
}

#[test]
fn steering_right_decreases_yaw_and_needs_speed() {
    let tr = arena();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 0.0, 0.0, 0);
    let y0 = r.yaw;
    drive(
        &mut r,
        &tr,
        &t,
        Input {
            steer: 1.0,
            ..Default::default()
        },
        30,
    );
    assert!(r.speed > 0.0 && math::wrap_angle(r.yaw - y0) < -0.2, "accelerates and steers immediately");
    set_speed(&mut r, 20.0);
    let y0 = r.yaw;
    drive(
        &mut r,
        &tr,
        &t,
        Input {
            steer: 1.0,
            throttle: 1.0,
            ..Default::default()
        },
        30,
    );
    assert!(
        math::wrap_angle(r.yaw - y0) < -0.5,
        "right turn lowers yaw: {}",
        r.yaw - y0
    );
}

/// Presses drift with a steer on the ground and holds it until `drift.active`.
fn start_drift(r: &mut Racer, tr: &Track, t: &Tunables, steer: f32) -> Input {
    let hold = Input {
        steer,
        throttle: 1.0,
        drift: true,
        ..Default::default()
    };
    let fx = r.integrate(&hold, tr, t);
    assert!(fx.drift_start, "drift starts on the press");
    assert!(r.on_ground, "no hop");
    hold
}

fn slip_deg(r: &Racer) -> f32 {
    let v = flat(r.vel);
    v.dot(r.right()).abs().atan2(v.dot(r.forward())).to_degrees()
}

#[test]
fn drift_reaches_every_tier_and_releases_its_boost() {
    let t = Tunables::default();
    let tr = arena();
    for tier in 1..=3u8 {
        let mut r = racer_at(&tr, 0.0, 60.0, 0);
        set_speed(&mut r, 28.0);
        let hold = start_drift(&mut r, &tr, &t, -0.4);
        drive(&mut r, &tr, &t, hold, 29);
        assert!(r.drift.active && r.drift.dir == -1);
        let mut n = 0;
        while r.drift.tier < tier && n < 600 {
            r.integrate(&hold, &tr, &t);
            n += 1;
        }
        assert_eq!(r.drift.tier, tier, "reached tier {tier}");
        // Release; the boost fires as the slip comes down (within the hold time).
        let release = Input {
            steer: -0.4,
            throttle: 1.0,
            ..Default::default()
        };
        let mut fired = None;
        for k in 0..((t.drift_boost_hold + t.drift_ease_out) / DT) as usize + 5 {
            let fx = r.integrate(&release, &tr, &t);
            if fx.drift_boost > 0 {
                fired = Some((k, fx));
                break;
            }
        }
        let (_, fx) = fired.unwrap_or_else(|| panic!("tier {tier} never fired"));
        assert_eq!(fx.drift_boost, tier);
        
        assert!(!r.drift.active);
        assert_eq!(r.drift.charge, 0.0, "release resets the charge");
        let want = t.drift_boost_time[tier as usize - 1];
        assert!(close(r.boost_t, want, 0.02), "tier {tier} boost {} want {want}", r.boost_t);
        assert!(close(r.boost_power(&t), t.drift_boost_power[tier as usize - 1], 1e-4));
        // The boost pushes above top speed, harder for the higher tiers.
        let mut peak = r.speed;
        for _ in 0..(want / DT) as usize - 2 {
            r.integrate(&Input::go(), &tr, &t);
            peak = peak.max(r.speed);
        }
        assert!(peak > t.top_speed, "boosted speed {peak}");
    }
}

#[test]
fn drift_charge_times_follow_the_spec() {
    // Charge is elapsed drift time: tiers at 0.8, 1.6 and 2.4 s.
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = Input {
        steer: 0.4,
        throttle: 1.0,
        drift: true,
        ..Default::default()
    };
    let mut ticks = 0;
    while !r.drift.active && ticks < 60 {
        r.integrate(&hold, &tr, &t);
        ticks += 1;
    }
    let mut n = 0;
    let mut at = [0usize; 3];
    while r.drift.tier < 3 && n < 600 {
        r.integrate(&hold, &tr, &t);
        n += 1;
        if r.drift.tier > 0 && at[r.drift.tier as usize - 1] == 0 {
            at[r.drift.tier as usize - 1] = n;
        }
    }
    for (i, want) in [0.8, 1.6, 2.4].into_iter().enumerate() {
        assert!(close(secs(at[i]), want, 0.04), "tier {} at {}", i + 1, secs(at[i]));
    }
}

#[test]
fn a_short_drift_earns_nothing() {
    let t = Tunables::default();
    let tr = arena();
    // 0.7 s is under the first tier; also check the edge just below it.
    for hold_ticks in [20usize, 40, 46] {
        let mut r = racer_at(&tr, 0.0, 60.0, 0);
        set_speed(&mut r, 28.0);
        let hold = start_drift(&mut r, &tr, &t, 0.6);
        drive(&mut r, &tr, &t, hold, hold_ticks);
        assert_eq!(r.drift.tier, 0, "{hold_ticks} ticks is under 0.8 s");
        let release = Input { steer: 0.6, throttle: 1.0, ..Default::default() };
        let fx = drive(&mut r, &tr, &t, release, 180);
        assert!(fx.iter().all(|f| f.drift_boost == 0), "a short drift earns no boost");
        assert_eq!(r.boost_t, 0.0);
        assert_eq!(r.drift.pending, 0);
    }
}

#[test]
fn the_exit_boost_waits_for_the_slip_to_come_down_and_can_lapse() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    drive(&mut r, &tr, &t, hold, 66);
    assert!(r.drift.tier >= 1 && slip_deg(&r) > t.drift_boost_slip.to_degrees() + 10.0, "sideways at {}", slip_deg(&r));
    let release = Input { steer: 1.0, throttle: 1.0, ..Default::default() };
    let first = r.integrate(&release, &tr, &t);
    assert_eq!(first.drift_boost, 0, "still sideways: held, not fired");
    assert_eq!(r.boost_t, 0.0);
    assert!(r.drift.pending >= 1);
    // It fires on the tick the slip crosses 15 degrees, inside the hold time.
    let mut ticks = 1;
    loop {
        let before = slip_deg(&r);
        let fx = r.integrate(&release, &tr, &t);
        ticks += 1;
        if fx.drift_boost > 0 {
            assert!(slip_deg(&r) < t.drift_boost_slip.to_degrees() + 0.5, "fired at {}", slip_deg(&r));
            assert!(before >= t.drift_boost_slip.to_degrees() - 3.0);
            break;
        }
        assert!(ticks <= (t.drift_boost_hold / DT) as usize + 2, "never fired");
    }
    assert!(secs(ticks) <= t.drift_boost_hold + 0.02);

    // If the slip never comes down the boost lapses after the hold time.
    let mut stubborn = t.clone();
    stubborn.drift_boost_slip = 0.001;
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &stubborn, 1.0);
    drive(&mut r, &tr, &stubborn, hold, 66);
    let fx = drive(&mut r, &tr, &stubborn, release, 90);
    assert!(fx.iter().all(|f| f.drift_boost == 0));
    assert_eq!(r.drift.pending, 0, "lapsed");
    assert_eq!(r.boost_t, 0.0);
}

#[test]
fn drifting_works_under_the_orange_pill_and_still_pays_out() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    r.effects.pill = 60.0;
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    drive(&mut r, &tr, &t, hold, 66);
    assert!(r.drift.active && r.drift.tier >= 1, "tier {} under the pill", r.drift.tier);
    assert!(r.drift.blend() > 0.9, "the slide is fully in: {}", r.drift.blend());
    assert!(slip_deg(&r) > 12.0, "slip {}", slip_deg(&r));
    // Letting go eases out and pays the exit boost, pill or not.
    let release = Input { steer: 1.0, throttle: 1.0, ..Default::default() };
    let fx = drive(&mut r, &tr, &t, release, 90);
    assert!(!r.drift.active);
    assert!(fx.iter().any(|f| f.drift_boost > 0), "the exit boost fires");
    assert!(r.effects.invincible(), "still on the pill");
}

#[test]
fn the_slide_eases_in_over_the_ease_in_time() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    let ease_ticks = (t.drift_ease_in / DT).round() as usize;
    let mut blends = vec![];
    let mut slips = vec![];
    for _ in 0..ease_ticks + 6 {
        r.integrate(&hold, &tr, &t);
        blends.push(r.drift.blend());
        slips.push(slip_deg(&r));
    }
    assert!(blends.windows(2).all(|w| w[1] >= w[0]), "blend only rises");
    assert!(blends[2] < 0.25, "not an instant slide: {}", blends[2]);
    assert!(close(blends[ease_ticks / 2 - 1], 0.5, 0.06), "halfway at half the time: {}", blends[ease_ticks / 2 - 1]);
    assert!(blends[ease_ticks - 1] > 0.97, "fully in by {} s", t.drift_ease_in);
    // The slip builds, it does not jump: little in the first 0.1 s, most by 0.4 s.
    assert!(slips[5] < 18.0, "slip after 0.1 s: {}", slips[5]);
    assert!(slips[ease_ticks - 1] > slips[5] * 1.5);
    assert!(slips.windows(2).all(|w| w[1] - w[0] < 4.0), "no tick jumps the slip by more than 4 degrees");
}

#[test]
fn letting_go_eases_out_instead_of_snapping_straight() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    drive(&mut r, &tr, &t, hold, 40);
    let slip0 = slip_deg(&r);
    let rate0 = r.yaw_rate;
    assert!(r.drift.blend() > 0.99 && slip0 > 25.0);
    // Release, steering still into the turn.
    let release = Input { steer: 1.0, throttle: 1.0, ..Default::default() };
    r.integrate(&release, &tr, &t);
    assert!(!r.drift.active);
    assert!(r.drift.blend() > 0.9, "blend drops over the ease-out, not at once: {}", r.drift.blend());
    assert!(r.yaw_rate.abs() > rate0.abs() * 0.7, "the turn eases: {} then {}", rate0, r.yaw_rate);
    let out_ticks = (t.drift_ease_out / DT).round() as usize;
    let mut mid = 0.0;
    for k in 0..out_ticks + 2 {
        r.integrate(&release, &tr, &t);
        if k == out_ticks / 3 {
            mid = slip_deg(&r);
        }
    }
    assert!(mid > slip0 * 0.5, "slip is still there a third of the way out: {mid} of {slip0}");
    assert_eq!(r.drift.blend_q, 0, "fully out after {} s", t.drift_ease_out);
    drive(&mut r, &tr, &t, release, 30);
    assert!(slip_deg(&r) < 12.0, "back to normal grip: {}", slip_deg(&r));
}

#[test]
fn centred_steering_unwinds_the_slide_smoothly_and_counter_steering_quicker() {
    let t = Tunables::default();
    let tr = arena();
    let run = |steer_after: f32, ticks: usize| {
        let mut r = racer_at(&tr, 0.0, 60.0, 0);
        set_speed(&mut r, 28.0);
        let hold = start_drift(&mut r, &tr, &t, 1.0);
        drive(&mut r, &tr, &t, hold, 60);
        let (slip0, rate0, carry0) = (slip_deg(&r), r.yaw_rate, r.drift.carry());
        let y0 = r.yaw;
        let hold = Input { steer: steer_after, ..hold };
        drive(&mut r, &tr, &t, hold, ticks);
        (r, slip0, rate0, carry0, y0)
    };
    // Centred: no snap, the turn unwinds over `drift_carry_time` (about 1/e
    // after one time constant) while the racer is still sliding and arcing.
    let (r, _, rate0, ..) = run(0.0, 1);
    assert!(r.yaw_rate.abs() > rate0.abs() * 0.9, "no snap: {} then {}", rate0, r.yaw_rate);
    let (r, slip0, rate0, carry0, y0) = run(0.0, (t.drift_carry_time / DT) as usize);
    let ratio = r.drift.carry() / carry0;
    assert!((0.25..0.5).contains(&ratio), "carry after one time constant: {ratio}");
    assert!(r.yaw_rate.abs() < rate0.abs() * 0.6 && r.yaw_rate.abs() > rate0.abs() * 0.2);
    assert!(r.drift.active && slip_deg(&r) > 8.0 && slip_deg(&r) < slip0, "still sliding, easing: {} of {slip0}", slip_deg(&r));
    assert!(math::wrap_angle(r.yaw - y0).abs() > 0.2, "it keeps arcing");
    // Counter-steering takes the slip down quicker than centring.
    let (counter, ..) = run(-1.0, 15);
    let (centred, ..) = run(0.0, 15);
    eprintln!("after 0.25 s: counter {} centred {}", slip_deg(&counter), slip_deg(&centred));
    assert!(slip_deg(&counter) < slip_deg(&centred) * 0.6, "counter {} centred {}", slip_deg(&counter), slip_deg(&centred));
    let (counter, ..) = run(-1.0, 30);
    assert!(slip_deg(&counter) < 15.0);
}

#[test]
fn holding_drift_can_never_spin_you_out() {
    let t = Tunables::default();
    let tr = arena();
    let limit = t.drift_spin_slip.to_degrees();
    for v in [14.0, 18.0, 22.0, 28.0, 32.0] {
        for steer in [1.0, 0.5, 0.0, -1.0] {
            let mut r = racer_at(&tr, 0.0, 0.0, 0);
            set_speed(&mut r, v);
            let hold = start_drift(&mut r, &tr, &t, 1.0);
            let hold = Input { steer, ..hold };
            let mut worst = 0.0f32;
            for _ in 0..240 {
                // (A wall is a different story: stop at the first one.)
                if r.integrate(&hold, &tr, &t).wall_hit > 0.0 {
                    break;
                }
                worst = worst.max(slip_deg(&r));
                assert!(r.stun_t <= 0.0, "spun out at {v} m/s with steer {steer}");
            }
            assert!(worst < limit - 3.0, "{v} m/s steer {steer}: slip peaked at {worst}");
        }
    }
    // Centred from a deep slide: the slip only ever falls.
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    drive(&mut r, &tr, &t, hold, 60);
    let start = slip_deg(&r);
    assert!(start > 20.0, "a deep slide to begin with: {start}");
    let mut prev = start;
    for k in 0..300 {
        r.integrate(&Input { steer: 0.0, ..hold }, &tr, &t);
        let now = slip_deg(&r);
        // The slide's momentum may carry it a touch further for a tick or two.
        assert!(now <= start + 0.5, "slip rose at tick {k}: {start} then {now}");
        assert!(k < 3 || now <= prev + 0.05, "slip rose at tick {k}: {prev} then {now}");
        prev = now;
    }
    assert!(prev < start, "and it ended lower: {prev} of {start}");
}

#[test]
fn only_reckless_driving_spins_a_drift_out() {
    let t = Tunables::default();
    let tr = arena();
    let sideways = |r: &mut Racer, deg: f32| {
        // A shove (a bash, a bump): the same speed, swung round to `deg` of slip.
        let sp = flat(r.vel).length();
        let a = deg.to_radians();
        // A right-hand drift slides out to the left of the heading.
        r.vel = r.forward() * (sp * a.cos()) - r.right() * (sp * a.sin());
    };
    let mk = || {
        let mut r = racer_at(&tr, 0.0, 60.0, 0);
        set_speed(&mut r, 28.0);
        let hold = start_drift(&mut r, &tr, &t, 1.0);
        drive(&mut r, &tr, &t, hold, 60);
        (r, hold)
    };
    // A nudge just over the line recovers by itself before the hold time is up.
    let (mut r, hold) = mk();
    sideways(&mut r, 68.0);
    let limit = t.drift_spin_slip.to_degrees();
    let mut over_ticks = 0;
    for _ in 0..40 {
        r.integrate(&hold, &tr, &t);
        assert!(r.stun_t <= 0.0, "a brief nudge past the threshold is forgiven");
        if slip_deg(&r) > limit {
            over_ticks += 1;
        }
    }
    assert!(over_ticks >= 1, "the nudge really did put the slip over the threshold");
    assert!(secs(over_ticks) < t.drift_spin_hold, "but not for as long as the hold time: {over_ticks} ticks");
    assert!(slip_deg(&r) < limit, "it recovered by itself: {}", slip_deg(&r));
    assert!(r.drift.active);
    // Well past it, and staying there, it is a spin-out.
    let (mut r, hold) = mk();
    sideways(&mut r, 100.0);
    let mut spun_at = None;
    for k in 0..60 {
        r.integrate(&hold, &tr, &t);
        if r.stun_t > 0.0 {
            spun_at = Some(k);
            break;
        }
    }
    let k = spun_at.expect("sliding backwards spins out");
    assert!(secs(k) >= t.drift_spin_hold - 0.05 && !r.drift.active);
    assert!(close(r.stun_t, t.drift_spin_time, 0.05));
    // Not while merely driving: the same shove without the drift held does nothing special.
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    sideways(&mut r, 100.0);
    drive(&mut r, &tr, &t, Input::go(), 60);
    assert!(r.stun_t <= 0.0);
}

#[test]
fn drift_slides_more_than_grip() {
    let t = Tunables::default();
    let tr = arena();
    let mut a = racer_at(&tr, 0.0, 60.0, 0);
    let mut b = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut a, 28.0);
    set_speed(&mut b, 28.0);
    drive(
        &mut a,
        &tr,
        &t,
        Input {
            steer: 1.0,
            throttle: 1.0,
            drift: true,
            ..Default::default()
        },
        90,
    );
    drive(
        &mut b,
        &tr,
        &t,
        Input {
            steer: 1.0,
            throttle: 1.0,
            ..Default::default()
        },
        90,
    );
    let slip = |r: &Racer| r.vel.dot(r.right()).abs();
    assert!(
        slip(&a) > slip(&b) * 1.5,
        "drift slip {} grip slip {}",
        slip(&a),
        slip(&b)
    );
    assert!(a.dust > 0.9, "drifting kicks up lots of dust: {}", a.dust);
    let mut c = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut c, 28.0);
    drive(&mut c, &tr, &t, Input::go(), 60);
    assert!(
        c.dust < 0.4,
        "straight running on road: light dust {}",
        c.dust
    );
    assert!(
        math::wrap_angle(a.visual_yaw - a.yaw).abs() > 0.3,
        "body angle while drifting {} active {} speed {}",
        math::wrap_angle(a.visual_yaw - a.yaw),
        a.drift.active,
        a.speed
    );
}

#[test]
fn drift_with_no_steer_stays_grounded_and_waits_for_a_side_while_held() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 0.0, 0);
    set_speed(&mut r, 28.0);
    let pull = Input {
        throttle: 1.0,
        drift: true,
        ..Default::default()
    };
    let fx = drive(&mut r, &tr, &t, pull, 6);
    assert!(fx.iter().all(|f| !f.jumped && !f.drift_start));
    assert!(r.on_ground && !r.drift.active && r.drift.hop, "armed, on the ground");
    // A steer picks the side.
    let fx = r.integrate(&Input { steer: 1.0, ..pull }, &tr, &t);
    assert!(fx.drift_start && r.drift.dir == 1);

    // It stays armed as long as the button is held, however long.
    let mut r = racer_at(&tr, 0.0, 0.0, 0);
    set_speed(&mut r, 28.0);
    let fx = drive(&mut r, &tr, &t, pull, 40);
    assert!(fx.iter().all(|f| !f.drift_start));
    assert!(r.on_ground && !r.drift.active && r.drift.hop);
    let fx = r.integrate(&Input { steer: -1.0, ..pull }, &tr, &t);
    assert!(fx.drift_start && r.drift.dir == -1);

    // Letting go disarms it.
    let mut r = racer_at(&tr, 0.0, 0.0, 0);
    set_speed(&mut r, 28.0);
    drive(&mut r, &tr, &t, pull, 10);
    let fx = r.integrate(&Input { steer: 1.0, throttle: 1.0, ..Default::default() }, &tr, &t);
    assert!(!fx.drift_start && !r.drift.hop && !r.drift.active);
}

#[test]
fn pressing_drift_eases_the_turn_in_rather_than_snapping() {
    let t = Tunables::default();
    let tr = arena();
    for v in [16.0, t.top_speed] {
        let mut plain = racer_at(&tr, 0.0, 60.0, 0);
        set_speed(&mut plain, v);
        let mut drift = plain.clone();
        let steer = Input { steer: 1.0, throttle: 1.0, ..Default::default() };
        plain.integrate(&steer, &tr, &t);
        let base = plain.yaw_rate.abs();
        let hold = start_drift(&mut drift, &tr, &t, 1.0);
        let mut rates = vec![drift.yaw_rate.abs()];
        rates.extend((0..60).map(|_| {
            drift.integrate(&hold, &tr, &t);
            drift.yaw_rate.abs()
        }));
        let early = rates[..15].iter().fold(0.0f32, |a, &b| a.max(b));
        let later = rates[30..].iter().fold(0.0f32, |a, &b| a.max(b));
        eprintln!("at {v} m/s: plain {base:.3}, first 0.25 s peak {:.3}x, by 1 s {:.3}x", early / base, later / base);
        assert!(rates[0] < base * 1.02, "at {v} m/s the press turns no harder than steering: {} vs {base}", rates[0]);
        assert!(early <= base * 1.25, "at {v} m/s no snap: {early} vs {base}");
        assert!(rates.windows(2).all(|w| w[1] - w[0] < base * 0.05), "at {v} m/s the turn builds smoothly");
        assert!(later > early * 1.05, "at {v} m/s the turn keeps building: {later} vs {early}");
        // The turn rate grows with speed lost to the slide; allow for that.
        assert!(later <= base * t.drift_turn_max * 1.15, "at {v} m/s at most the full drift turn: {later} vs {base}");
        assert!(drift.drift.active && slip_deg(&drift) > 15.0, "at {v} m/s it slides: {}", slip_deg(&drift));
    }
}

#[test]
fn drift_held_going_straight_does_not_turn() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, t.top_speed);
    let y0 = r.yaw;
    let pull = Input { throttle: 1.0, drift: true, ..Default::default() };
    let fx = drive(&mut r, &tr, &t, pull, 60);
    assert!(fx.iter().all(|f| !f.drift_start) && !r.drift.active);
    let turned = math::wrap_angle(r.yaw - y0).abs().to_degrees();
    eprintln!("straight with drift held for 1 s: heading moved {turned:.3} degrees");
    assert!(turned < 3.0, "heading moved {turned} degrees");
    // A light wobble does not start it either.
    let fx = drive(&mut r, &tr, &t, Input { steer: 0.2, ..pull }, 30);
    assert!(fx.iter().all(|f| !f.drift_start) && !r.drift.active);
}

#[test]
fn the_drift_turn_follows_how_far_you_steer() {
    let t = Tunables::default();
    let tr = arena();
    let settled = |steer: f32| {
        let mut r = racer_at(&tr, 0.0, 60.0, 0);
        set_speed(&mut r, 28.0);
        let hold = start_drift(&mut r, &tr, &t, 0.3);
        let hold = Input { steer, ..hold };
        drive(&mut r, &tr, &t, hold, 42);
        let rates: Vec<f32> = (0..18).map(|_| { r.integrate(&hold, &tr, &t); r.yaw_rate.abs() }).collect();
        assert!(r.drift.active || steer.abs() < 0.15, "still drifting at steer {steer}");
        rates.iter().sum::<f32>() / rates.len() as f32
    };
    let (base, quarter, half, full) = (settled(0.0), settled(0.25), settled(0.5), settled(1.0));
    let ratio = (half - base) / (full - base);
    eprintln!("settled yaw rates: 0 {base:.3}, 0.25 {quarter:.3}, 0.5 {half:.3}, 1.0 {full:.3}; half/full {ratio:.3}");
    assert!(base < quarter && quarter < half && half < full, "{base} {quarter} {half} {full}");
    assert!((0.35..=0.65).contains(&ratio), "half steer gives about half the turn: {ratio}");
}

#[test]
fn straightening_up_with_drift_held_fires_the_exit_boost() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    let mut n = 0;
    while r.drift.tier < 1 && n < 120 {
        r.integrate(&hold, &tr, &t);
        n += 1;
    }
    assert_eq!(r.drift.tier, 1);
    let slip0 = slip_deg(&r);
    let straight = Input { steer: 0.0, ..hold };
    let mut settled_at = None;
    let mut fired_at = None;
    for k in 1..=60usize {
        let fx = r.integrate(&straight, &tr, &t);
        if settled_at.is_none() && slip_deg(&r) < t.drift_boost_slip.to_degrees() {
            settled_at = Some(k);
        }
        if fx.drift_boost > 0 {
            assert_eq!(fx.drift_boost, 1);
            assert!(fx.drift_points > 0.0);
            fired_at = Some(k);
            break;
        }
        assert!(r.drift.active, "the drift runs on until the payoff");
    }
    let settled_at = settled_at.expect("the slip comes down");
    let fired_at = fired_at.expect("the exit boost fires with the button held");
    eprintln!("from {slip0:.1} degrees: slip under {:.1} after {:.3} s, boost after {:.3} s", t.drift_boost_slip.to_degrees(), secs(settled_at), secs(fired_at));
    assert!(secs(settled_at) <= 0.8 && secs(fired_at) <= 0.8 + t.drift_straight_hold);
    assert!(fired_at > settled_at, "a moment straight first, not a twitch");
    assert!(!r.drift.active && !r.drift.hop && r.boost_t > 0.0);
    // Still held: no new drift without a fresh press, and no second boost.
    let fx = drive(&mut r, &tr, &t, Input { steer: 1.0, ..hold }, 30);
    assert!(fx.iter().all(|f| !f.drift_start && f.drift_boost == 0));

    // A quick wiggle through the centre does not fire it.
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    drive(&mut r, &tr, &t, hold, 60);
    assert!(r.drift.tier >= 1);
    let fx = drive(&mut r, &tr, &t, straight, 3);
    let fx2 = drive(&mut r, &tr, &t, hold, 30);
    assert!(fx.iter().chain(&fx2).all(|f| f.drift_boost == 0) && r.drift.active);
}

/// A jump at top speed on the flat with the button released after take-off:
/// (airtime s, apex m, distance m).
fn measure_jump(t: &Tunables) -> (f32, f32, f32) {
    let tr = proving_ground();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, t.top_speed);
    let start = r.pos;
    let fx = r.integrate(&Input { throttle: 1.0, jump: true, ..Default::default() }, &tr, t);
    assert!(fx.jumped && !r.on_ground);
    let mut ticks = 1;
    let mut peak = 0.0f32;
    loop {
        let fx = r.integrate(&Input::go(), &tr, t);
        ticks += 1;
        peak = peak.max(r.air_h);
        if fx.landed || ticks > 400 {
            break;
        }
    }
    (secs(ticks), peak, flat(r.pos - start).length())
}

#[test]
fn jump_air_time_is_about_0_9_s() {
    let tr = proving_ground();
    let t = Tunables::default();
    let (air, peak, _) = measure_jump(&t);
    assert!(close(air, 2.0 * t.jump_vy / t.gravity, 0.04), "air {air}");
    assert!(
        close(peak, t.jump_vy * t.jump_vy / (2.0 * t.gravity), 0.1),
        "peak {peak}"
    );
    // Holding jump does not re-jump (edge detected).
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 25.0);
    let hold = Input { throttle: 1.0, jump: true, ..Default::default() };
    drive(&mut r, &tr, &t, hold, 1);
    drive(&mut r, &tr, &t, Input::go(), 80);
    assert!(r.on_ground);
    let fx = drive(&mut r, &tr, &t, hold, 1);
    assert!(fx[0].jumped, "a fresh press jumps");
    let fx = drive(&mut r, &tr, &t, hold, 100);
    assert!(fx.iter().all(|f| !f.jumped));
}

#[test]
fn the_jump_is_about_twice_as_high_and_1_4_times_as_long_as_it_was() {
    let old = Tunables { jump_vy: 7.8, ..Tunables::default() };
    let new = Tunables::default();
    let (air0, peak0, dist0) = measure_jump(&old);
    let (air1, peak1, dist1) = measure_jump(&new);
    eprintln!("old: air {air0:.3} s apex {peak0:.3} m dist {dist0:.1} m");
    eprintln!("new: air {air1:.3} s apex {peak1:.3} m dist {dist1:.1} m");
    assert!((1.7..=2.0).contains(&(peak1 / peak0)), "apex x{}", peak1 / peak0);
    assert!((1.3..=1.5).contains(&(air1 / air0)), "airtime x{}", air1 / air0);
    assert!((1.3..=1.5).contains(&(dist1 / dist0)), "distance x{}", dist1 / dist0);
    // A full jump is not a hard landing; a real drop still is.
    assert!(peak1 < new.hard_landing);
}

/// Take-off at top speed on the flat; `hold(k, r)` gives jump for each tick `k`.
/// Returns the per-tick (y, vy, horizontal speed, on_ground) and the landing tick.
fn flight(t: &Tunables, mut hold: impl FnMut(usize, &Racer) -> bool) -> (Vec<(f32, f32, f32)>, usize) {
    let tr = proving_ground();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, t.top_speed);
    let mut log = Vec::new();
    for k in 0..600 {
        let inp = Input { throttle: 1.0, jump: k == 0 || hold(k, &r), ..Default::default() };
        let fx = r.integrate(&inp, &tr, t);
        log.push((r.air_h, r.vel.y, flat(r.vel).length()));
        if k > 0 && fx.landed {
            assert!(r.on_ground && r.vel.y == 0.0, "landing resets cleanly");
            return (log, k);
        }
    }
    panic!("never landed");
}

#[test]
fn holding_jump_after_the_apex_glides_down_slowly() {
    let t = Tunables::default();
    let (free, free_land) = flight(&t, |_, _| false);
    let (glide, glide_land) = flight(&t, |_, _| true);
    eprintln!("landing: released {:.3} s, held {:.3} s", secs(free_land + 1), secs(glide_land + 1));
    // Take-off frame and the rise are identical, up to and including the apex tick.
    let apex = free.iter().position(|f| f.1 <= 0.0).unwrap();
    assert!(apex > 10);
    for k in 0..=apex {
        assert_eq!(free[k], glide[k], "tick {k} differs before the apex");
    }
    assert!(glide_land > free_land + 20, "glide {glide_land} vs fall {free_land}");
    // Never faster than the cap, and no free speed.
    for (k, f) in glide.iter().enumerate().take(glide_land) {
        assert!(f.1 >= playful::GLIDE_MAX_FALL - 1e-3, "tick {k}: vy {}", f.1);
        assert!(close(f.2, t.top_speed, 0.05), "tick {k}: horizontal {}", f.2);
    }
    for f in free.iter().take(free_land) {
        assert!(close(f.2, t.top_speed, 0.05));
    }
    // Light gravity before the cap bites: a quarter of the usual pull.
    let k = apex + 3;
    assert!(close(glide[k + 1].1 - glide[k].1, -t.gravity * playful::GLIDE_GRAVITY * DT, 1e-4));
}

#[test]
fn letting_go_mid_glide_falls_normally_from_the_current_speed() {
    let t = Tunables::default();
    let (free, free_land) = flight(&t, |_, _| false);
    let apex = free.iter().position(|f| f.1 <= 0.0).unwrap();
    let release = apex + 25;
    let (mixed, mixed_land) = flight(&t, |k, _| k < release);
    let (glide, glide_land) = flight(&t, |_, _| true);
    // Same as the full glide up to the release, then ordinary gravity at once.
    assert_eq!(mixed[release - 1], glide[release - 1]);
    let vy0 = mixed[release - 1].1;
    assert!(vy0 < -1.0 && vy0 > -3.01, "gliding at {vy0}");
    for (n, f) in mixed[release..].iter().enumerate().take(5) {
        assert!(close(f.1, vy0 - t.gravity * DT * (n + 1) as f32, 1e-3), "n {n}: vy {}", f.1);
    }
    assert!(free_land < mixed_land && mixed_land < glide_land, "{free_land} < {mixed_land} < {glide_land}");
    // Pressing again in the air does not jump or re-glide before the apex: a rise is untouched.
    let (rise, _) = flight(&t, |k, _| k % 2 == 0);
    assert_eq!(rise[3], free[3]);
}

#[test]
fn a_fast_fall_with_jump_held_is_eased_to_the_glide_speed() {
    let t = Tunables::default();
    let tr = proving_ground();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 25.0);
    r.pos.y += 40.0;
    r.on_ground = false;
    r.vel.y = -15.0;
    let hold = Input { throttle: 1.0, jump: true, ..Default::default() };
    let mut last = r.vel.y;
    for _ in 0..30 {
        r.integrate(&hold, &tr, &t);
        assert!(r.vel.y >= last && r.vel.y <= -2.9, "vy {}", r.vel.y);
        last = r.vel.y;
    }
    assert!(close(last, playful::GLIDE_MAX_FALL, 0.05), "settled at {last}");
}

#[test]
fn no_glide_while_stunned_or_mid_trick() {
    let t = Tunables::default();
    let tr = proving_ground();
    let hold = Input { throttle: 1.0, jump: true, ..Default::default() };
    // Spun out in the air.
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 25.0);
    r.pos.y += 20.0;
    r.on_ground = false;
    r.vel.y = -5.0;
    r.spin_out(1.0);
    r.integrate(&hold, &tr, &t);
    assert!(close(r.vel.y, -5.0 - t.gravity * DT, 1e-4), "stunned vy {}", r.vel.y);
    // A trick in the air.
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 25.0);
    r.pos.y += 20.0;
    r.on_ground = false;
    r.vel.y = -5.0;
    r.trick_pending = true;
    r.integrate(&hold, &tr, &t);
    assert!(close(r.vel.y, -5.0 - t.gravity * DT, 1e-4), "trick vy {}", r.vel.y);
}

/// Cake: eating blocks every button but not steering.
#[test]
fn eating_cake_blocks_jump_drift_and_laser_but_not_steering() {
    let tr = proving_ground();
    let t = Tunables { practice_lasers: true, ..Tunables::default() };
    let all = Input {
        throttle: 1.0,
        steer: 1.0,
        drift: true,
        jump: true,
        laser: true,
        item: true,
        bash: true,
        ..Default::default()
    };
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 25.0);
    r.effects.cake_eat = 0.5;
    let yaw0 = r.yaw;
    let mut eating_ticks = 0;
    while r.effects.cake_eat > 0.0 {
        let fx = r.integrate(&all, &tr, &t);
        eating_ticks += 1;
        assert!(!fx.jumped && r.on_ground, "jumped while eating (tick {eating_ticks})");
        assert!(!r.drift.hop && !r.drift.active && fx.drift_start == false, "drifted while eating");
        assert!(!r.laser.firing && r.laser.heat == 0.0, "lasered while eating");
        assert!(eating_ticks < 100);
    }
    assert!(eating_ticks >= 29, "{eating_ticks} ticks of cake");
    assert!(r.yaw != yaw0, "steering still turns the bird");
    // Everything was held through the end of the cake: nothing fires on the last bite.
    for _ in 0..3 {
        let fx = r.integrate(&all, &tr, &t);
        assert!(!fx.jumped && !r.drift.hop && !r.drift.active, "phantom edge after the cake");
    }
    // Laser is a held level, so it works again at once.
    assert!(r.laser.firing || r.laser.overheated);
    // Release, then press afresh: both work.
    drive(&mut r, &tr, &t, Input::go(), 2);
    let fx = r.integrate(&Input { jump: true, drift: true, ..Input::go() }, &tr, &t);
    assert!(fx.jumped, "a fresh jump press works after the cake");
    assert!(r.drift.hop, "a fresh drift press works after the cake");
}

#[test]
fn a_press_made_during_cake_and_held_past_it_does_not_fire() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 25.0);
    r.effects.cake_eat = 0.2;
    // Released first, pressed mid-cake, held on.
    r.integrate(&Input::go(), &tr, &t);
    let press = Input { jump: true, drift: true, ..Input::go() };
    let fx = drive(&mut r, &tr, &t, press, 40);
    assert!(r.effects.cake_eat == 0.0);
    assert!(fx.iter().all(|f| !f.jumped), "no jump from a press made while eating");
    assert!(!r.drift.hop && !r.drift.active);
}

#[test]
fn a_drift_running_when_the_cake_lands_ends_through_the_normal_release() {
    let tr = arena();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    drive(&mut r, &tr, &t, hold, 90);
    assert!(r.drift.active && r.drift.tier > 0);
    r.effects.cake_eat = 0.3;
    let fx = drive(&mut r, &tr, &t, hold, 40);
    assert!(!r.drift.active, "the drift ends under the cake");
    assert!(fx.iter().any(|f| f.drift_boost > 0) || r.drift.pending > 0 || r.boost_t > 0.0, "release keeps its exit boost");
}

#[test]
fn full_lock_into_a_wall_stays_in_the_band() {
    let tr = proving_ground();
    let t = Tunables::default();
    for steer in [-1.0f32, 1.0] {
        let mut r = racer_at(&tr, 50.0, 0.0, 4);
        set_speed(&mut r, 32.0);
        let mut hit = false;
        for _ in 0..600 {
            let fx = r.integrate(
                &Input {
                    steer,
                    throttle: 1.0,
                    ..Default::default()
                },
                &tr,
                &t,
            );
            hit |= fx.wall_hit > 0.0;
            let hb = tr.half_band(r.s, r.lateral);
            assert!(r.lateral.abs() <= hb, "lateral {} band {hb}", r.lateral);
        }
        assert!(hit, "should have hit the wall");
        // Same with a drift held into the wall.
        let mut r = racer_at(&tr, 50.0, 0.0, 1);
        set_speed(&mut r, 32.0);
        for _ in 0..600 {
            r.integrate(
                &Input {
                    steer,
                    throttle: 1.0,
                    drift: true,
                    ..Default::default()
                },
                &tr,
                &t,
            );
            assert!(r.lateral.abs() <= tr.half_band(r.s, r.lateral));
        }
    }
}

#[test]
fn ramps_launch_and_tricks_boost() {
    let tr = proving_ground();
    let t = Tunables::default();
    let ramp_s = pg_back_straight() + 200.0;
    for trick in [false, true] {
        let mut r = racer_at(&tr, ramp_s - 80.0, 0.0, 0);
        set_speed(&mut r, t.top_speed);
        let mut launched_at = None;
        let mut landed = false;
        let mut trick_seen = false;
        let mut trick_boost = false;
        let mut peak = 0.0f32;
        for k in 0..400 {
            // Press jump on the ramp face, just before the lip.
            let on_face = r.s > ramp_s + 6.0 && r.s < ramp_s + 12.0;
            let inp = Input {
                throttle: 1.0,
                jump: trick && on_face,
                ..Default::default()
            };
            let fx = r.integrate(&inp, &tr, &t);
            trick_seen |= fx.trick;
            trick_boost |= fx.trick_boost;
            if !r.on_ground && launched_at.is_none() && r.s > ramp_s + 5.0 {
                launched_at = Some(k);
            }
            peak = peak.max(r.air_h);
            if launched_at.is_some() && fx.landed {
                landed = true;
                let air = secs(k - launched_at.unwrap());
                assert!(air > 0.35, "air time off the ramp {air}");
                break;
            }
        }
        assert!(launched_at.is_some(), "ramp should launch");
        assert!(landed);
        assert!(peak > 1.0, "airborne above the ground: {peak}");
        assert_eq!(
            trick_seen, trick,
            "trick only when jump is pressed at the lip"
        );
        assert_eq!(trick_boost, trick);
        if trick {
            assert!(r.boost_t > 0.5);
        }
    }
}

#[test]
fn boost_pad_boosts() {
    let tr = proving_ground();
    let t = Tunables::default();
    let pad = pg_back_straight() + 420.0;
    let mut r = racer_at(&tr, pad - 30.0, 0.0, 0);
    set_speed(&mut r, 25.0);
    let fx = drive(&mut r, &tr, &t, Input::go(), 90);
    assert!(fx.iter().any(|f| f.boost_pad));
    assert!(r.speed > t.top_speed, "{}", r.speed);
}

#[test]
fn offroad_is_slow() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 20.0, 11.5, 0); // on the right shoulder
    set_speed(&mut r, 30.0);
    drive(&mut r, &tr, &t, Input::go(), 240);
    assert!(r.offroad);
    assert!(
        close(r.speed, t.top_speed * t.offroad_top, 0.3),
        "offroad speed {}",
        r.speed
    );
    assert!(r.dust > 0.4, "off-road dust {}", r.dust);
}

#[test]
fn spin_out_turns_twice_and_slows() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 30.0);
    let y0 = r.yaw;
    r.spin_out(1.0);
    let mut total = 0.0;
    for _ in 0..60 {
        let before = r.yaw;
        r.integrate(&Input::go(), &tr, &t);
        total += math::wrap_angle(r.yaw - before);
    }
    assert!(
        close(total, 2.0 * std::f32::consts::TAU, 0.1),
        "spun {total}"
    );
    assert!(close(math::wrap_angle(r.yaw - y0), 0.0, 0.1));
    assert!(r.speed < 30.0 - 15.0, "spin decays speed: {}", r.speed);
}

#[test]
fn gait_footsteps_and_look_back() {
    let tr = proving_ground();
    let t = Tunables::default();
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    set_speed(&mut r, 26.0);
    let fx = drive(
        &mut r,
        &tr,
        &t,
        Input {
            throttle: 1.0,
            look_back: true,
            ..Default::default()
        },
        60,
    );
    let steps = fx.iter().filter(|f| f.footstep).count();
    // ~26-32 m/s over 1 s with a ~5-6 m stride at speed = ~5 cycles = ~10 footsteps.
    assert!((7..=14).contains(&steps), "steps {steps}");
    assert!(
        close(r.look_back, 1.0, 1e-3),
        "look back eased to 1 within 1 s"
    );
    let mut r2 = r.clone();
    r2.integrate(&Input::go(), &tr, &t);
    assert!(close(r2.look_back, 1.0 - t.look_back_rate * DT, 1e-3));
}

#[test]
fn laser_heat_overheats_and_locks() {
    let tr = proving_ground();
    let t = Tunables { practice_lasers: true, ..Default::default() };
    let mut r = racer_at(&tr, 20.0, 0.0, 0);
    let fire = Input {
        laser: true,
        ..Default::default()
    };
    let mut n = 0;
    let mut over = false;
    while n < 200 {
        let fx = r.integrate(&fire, &tr, &t);
        n += 1;
        if fx.overheated {
            over = true;
            break;
        }
    }
    assert!(over);
    assert!(
        close(secs(n), 1.0 / t.laser_heat_rate, 0.03),
        "overheat after {}",
        secs(n)
    );
    // Locked for 1.4 s even while holding.
    let fx = drive(
        &mut r,
        &tr,
        &t,
        fire,
        (t.laser_overheat_lock / DT) as usize - 2,
    );
    assert!(fx.iter().all(|_| true) && r.laser.overheated && !r.laser.firing);
    drive(&mut r, &tr, &t, fire, 6);
    assert!(!r.laser.overheated && r.laser.firing);
}

#[test]
fn integrate_is_pure() {
    let tr = TrackId::Canyon.track();
    let t = Tunables::default();
    let mut a = racer_at(&tr, 300.0, 0.0, 2);
    let mut b = a.clone();
    let mut seq = 12345u32;
    for _ in 0..2000 {
        seq = seq.wrapping_mul(1664525).wrapping_add(1013904223);
        let inp = Input::unpack(seq.to_le_bytes());
        let inp = Input {
            throttle: 1.0,
            ..inp
        };
        let fa = a.integrate(&inp, &tr, &t);
        let fb = b.integrate(&inp, &tr, &t);
        assert_eq!(fa, fb);
    }
    assert_eq!(a, b);
}

#[test]
fn input_pack_roundtrip() {
    let i = Input {
        steer: -0.5,
        throttle: 1.0,
        brake: true,
        drift: false,
        jump: true,
        laser: true,
        item: false,
        bash: true,
        look_back: true,
    };
    let u = Input::unpack(i.pack());
    assert!(close(u.steer, -0.5, 0.01));
    assert_eq!(u.throttle, 1.0);
    assert_eq!(
        (
            u.brake,
            u.drift,
            u.jump,
            u.laser,
            u.item,
            u.bash,
            u.look_back
        ),
        (true, false, true, true, false, true, true)
    );
    assert_eq!(Input::unpack(Input::default().pack()), Input::default());
}

#[test]
fn falling_into_the_canyon_gap_respawns_before_it() {
    let tr = TrackId::Canyon.track();
    let t = Tunables::default();
    let gap = tr
        .features
        .iter()
        .find_map(|f| {
            if let Feature::Gap { s, len } = *f {
                Some((s, len))
            } else {
                None
            }
        })
        .expect("canyon has a gap");
    // Crawl into the gap.
    let mut r = racer_at(&tr, gap.0 - 2.0, 0.0, 0);
    set_speed(&mut r, 6.0);
    let mut fell = false;
    let mut respawned = false;
    for _ in 0..600 {
        let fx = r.integrate(&Input::go(), &tr, &t);
        fell |= fx.fell;
        if fx.respawned {
            respawned = true;
            break;
        }
    }
    assert!(
        fell && respawned,
        "fell {fell} respawned {respawned} y {} s {}",
        r.pos.y,
        r.s
    );
    let back = tr.s_delta(r.s, gap.0);
    assert!(back >= 60.0, "respawned {back} m before the gap");
    assert!(r.on_ground && r.speed > 0.3 * t.top_speed);
    // From there, full throttle clears the gap (boost pad + ramp).
    let mut cleared = false;
    for _ in 0..600 {
        let fx = r.integrate(&Input::go(), &tr, &t);
        assert!(!fx.fell, "fell into the gap again");
        if tr.s_delta(gap.0 + gap.1 + 10.0, r.s) > 0.0 && tr.s_delta(gap.0, r.s) < 100.0 {
            cleared = true;
            break;
        }
    }
    assert!(cleared);
}

/// A racer at 20 m/s 1.5 m inside the right-hand wall, heading `deg` degrees off
/// the track direction towards it.
fn at_right_wall(tr: &Track, deg: f32) -> Racer {
    let s0 = 100.0;
    let mut r = racer_at(tr, s0, 0.0, 0);
    let sm = tr.sample_at(s0);
    let hb = tr.half_band(s0, 0.0);
    r.pos = sm.pos + sm.right * (hb - 1.5);
    r.pos.y = tr.project(r.pos, tr.idx_at(s0)).ground_y;
    // yaw 0 faces +Z and `right = (-cos yaw, 0, sin yaw)`, so turning towards the
    // right-hand side is a negative yaw change.
    r.yaw = math::wrap_angle(math::yaw_of(sm.tangent) - deg.to_radians());
    set_speed(&mut r, 20.0);
    r.lateral = hb - 1.5;
    r
}

#[test]
fn walls_slide_and_steering_away_recovers_speed() {
    let tr = proving_ground();
    let t = Tunables::default();
    for deg in [30.0f32, 80.0] {
        let mut r = at_right_wall(&tr, deg);
        let s_start = r.s;
        // Hold full throttle into the wall for 3 s with no steering.
        let fx = drive(&mut r, &tr, &t, Input::go(), 180);
        assert!(fx.iter().any(|f| f.wall_hit > 0.0), "{deg} deg: should reach the wall");
        assert!(r.lateral.abs() <= tr.half_band(r.s, r.lateral), "{deg} deg: stays in the band");
        let slid = r.s - s_start;
        assert!(
            slid > 10.0,
            "{deg} deg: pressed against the wall for 3 s the racer must keep sliding along it; moved {slid:.1} m, speed {:.1}",
            r.speed
        );
        // Steer away from the wall and recover within 2 s.
        let mut best = 0.0f32;
        for _ in 0..120 {
            r.integrate(&Input { steer: -1.0, throttle: 1.0, ..Default::default() }, &tr, &t);
            best = best.max(r.speed);
        }
        assert!(r.speed > 15.0, "{deg} deg: recovered only to {:.1} m/s (best {best:.1})", r.speed);
    }
}

/// The steering floor alone (no glancing assist) is enough to turn away from a wall
/// you have been pushed into, and it is only a floor near walls: no spinning on the
/// spot in open road.
#[test]
fn steering_floor_turns_away_from_a_wall_without_the_assist() {
    let tr = proving_ground();
    let t = Tunables { wall_glance: 0.0, ..Default::default() };
    for deg in [30.0f32, 80.0] {
        let mut r = at_right_wall(&tr, deg);
        drive(&mut r, &tr, &t, Input::go(), 90);
        let mut best = 0.0f32;
        for _ in 0..120 {
            r.integrate(&Input { steer: -1.0, throttle: 1.0, ..Default::default() }, &tr, &t);
            best = best.max(r.speed);
        }
        assert!(r.speed > 15.0, "{deg} deg: recovered only to {:.1} m/s (best {best:.1})", r.speed);
    }
    // Open road, standing still: steering does not spin the racer on the spot.
    let mut r = racer_at(&tr, 100.0, 0.0, 0);
    let yaw0 = r.yaw;
    drive(&mut r, &tr, &t, Input { steer: 1.0, ..Default::default() }, 60);
    assert!(r.speed > 1.0 && math::wrap_angle(r.yaw - yaw0) < -0.2, "full throttle turns while running");
}

/// A weak tier-1 drift-exit boost with plenty of time left.
fn weak_exit_boost(r: &mut Racer, t: &Tunables) {
    r.boost_t = 5.0;
    r.boost_tier = 1;
    assert!(r.boost_power(t) < 0.9, "set-up: the exit boost is the weak one");
}

#[test]
fn a_coin_or_trump_exit_boost_runs_at_full_power_over_a_weak_exit_boost() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 0.0, 0);
    weak_exit_boost(&mut r, &t);
    r.pick_coin(&t);
    assert_eq!(r.boost_tier, 0);
    assert!(close(r.boost_power(&t), 1.0, 1e-6));
    // The Laser Trump's exit boost: through the real motion step.
    let mut r = racer_at(&tr, 0.0, 0.0, 0);
    set_speed(&mut r, 20.0);
    weak_exit_boost(&mut r, &t);
    r.effects.trump = 0.001; // the rocket runs out on this tick
    r.integrate(&Input::go(), &tr, &t);
    assert_eq!(r.boost_tier, 0, "the trump exit boost is full power");
}

#[test]
fn a_trick_boost_runs_at_full_power_over_a_weak_exit_boost() {
    let tr = proving_ground();
    let t = Tunables::default();
    let ramp_s = pg_back_straight() + 200.0;
    let mut r = racer_at(&tr, ramp_s - 80.0, 0.0, 0);
    set_speed(&mut r, t.top_speed);
    weak_exit_boost(&mut r, &t);
    let mut trick_boost = false;
    for _ in 0..400 {
        let on_face = r.s > ramp_s + 6.0 && r.s < ramp_s + 12.0;
        let inp = Input { throttle: 1.0, jump: on_face, ..Default::default() };
        let fx = r.integrate(&inp, &tr, &t);
        if fx.trick_boost {
            trick_boost = true;
            break;
        }
        assert_eq!(r.boost_tier, 1, "the weak boost is untouched until the trick lands");
    }
    assert!(trick_boost, "the trick landed");
    assert_eq!(r.boost_tier, 0);
    assert!(close(r.boost_power(&t), 1.0, 1e-6));
}

#[test]
fn a_drift_exit_boost_never_weakens_a_stronger_running_boost() {
    let t = Tunables::default();
    let tr = arena();
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    // Hold a drift to tier 1 only, then let go while a pad boost is running.
    drive(&mut r, &tr, &t, Input { steer: 0.3, ..hold }, ((t.drift_tiers[0] + 0.1) / DT) as usize);
    assert!(r.drift.tier >= 1 && r.drift.tier < 3);
    r.grant_boost(t.boost_pad_time);
    assert_eq!(r.boost_tier, 0);
    let release = Input { steer: -1.0, throttle: 1.0, ..Default::default() };
    let mut fired = false;
    for _ in 0..90 {
        let fx = r.integrate(&release, &tr, &t);
        fired |= fx.drift_boost > 0;
        assert_eq!(r.boost_tier, 0, "full power kept");
        assert!(close(r.boost_power(&t), 1.0, 1e-6));
    }
    assert!(fired, "the exit boost still fired");
}

#[test]
fn a_long_jump_with_drift_held_and_full_steering_never_spins_out() {
    let t = Tunables::default();
    let tr = arena();
    for vy in [t.jump_vy, 10.0, 14.0, 18.0] {
        for steer in [1.0, -1.0] {
            let mut r = racer_at(&tr, 0.0, 60.0, 0);
            set_speed(&mut r, 28.0);
            let hold = start_drift(&mut r, &tr, &t, steer);
            drive(&mut r, &tr, &t, hold, 20);
            r.vel.y = vy;
            r.on_ground = false;
            let mut worst = 0.0f32;
            let mut airborne = 0;
            for _ in 0..400 {
                let fx = r.integrate(&hold, &tr, &t);
                if fx.wall_hit > 0.0 {
                    break;
                }
                assert!(r.stun_t <= 0.0, "spun out: vy {vy} steer {steer}");
                if !r.on_ground {
                    airborne += 1;
                }
                worst = worst.max(slip_deg(&r));
                if fx.landed {
                    // Hold on a little after landing too.
                    drive(&mut r, &tr, &t, hold, 40);
                    assert!(r.stun_t <= 0.0, "spun out on landing: vy {vy} steer {steer}");
                    break;
                }
            }
            assert!(airborne as f32 * DT > 0.5, "it really was a jump: {airborne} ticks");
            assert!(worst < t.drift_spin_slip.to_degrees() - 3.0, "vy {vy} steer {steer}: slip peaked at {worst}");
        }
    }
    // A deliberate shove in the air still can: well past the threshold, staying there.
    let mut r = racer_at(&tr, 0.0, 60.0, 0);
    set_speed(&mut r, 28.0);
    let hold = start_drift(&mut r, &tr, &t, 1.0);
    drive(&mut r, &tr, &t, hold, 20);
    r.vel.y = 14.0;
    r.on_ground = false;
    let sp = flat(r.vel).length();
    let a = 100.0f32.to_radians();
    let y = r.vel.y;
    r.vel = r.forward() * (sp * a.cos()) - r.right() * (sp * a.sin());
    r.vel.y = y;
    let mut spun = false;
    for _ in 0..200 {
        r.integrate(&Input { steer: 0.0, ..hold }, &tr, &t);
        spun |= r.stun_t > 0.0;
    }
    assert!(spun, "a shove past the threshold still spins the racer out");
}
