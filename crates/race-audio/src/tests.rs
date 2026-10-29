use super::*;

fn peak(b: &[f32]) -> f32 {
    b.iter().fold(0.0f32, |m, x| m.max(x.abs()))
}

/// RMS of adjacent-sample differences: a yardstick for "how jumpy is this signal".
fn step_rms(b: &[f32]) -> f32 {
    let s: f32 = b.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum();
    (s / (b.len() - 1) as f32).sqrt()
}

/// The wrap from the last sample to the first is no bigger than an ordinary step.
fn assert_seamless(name: &str, b: &[f32]) {
    let seam = (b[0] - b[b.len() - 1]).abs();
    // No bigger than an ordinary step: 4x the RMS step, or (for signals with sharp
    // natural edges such as a saw wrap) the 99.9th-percentile step.
    let mut diffs: Vec<f32> = b.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p999 = diffs[(diffs.len() as f32 * 0.999) as usize];
    let limit = (4.0 * step_rms(b)).max(1.25 * p999) + 0.02;
    assert!(seam <= limit, "{name}: seam jump {seam} exceeds {limit}");
}

fn assert_clean(name: &str, b: &[f32]) {
    assert!(!b.is_empty(), "{name}: empty");
    assert!(b.iter().all(|x| x.is_finite()), "{name}: non-finite sample");
    assert!(peak(b) <= 1.0, "{name}: peak {}", peak(b));
    assert!(peak(b) > 0.05, "{name}: silent");
}

#[test]
fn sfx_order_matches_discriminants() {
    for (i, s) in Sfx::ALL.iter().enumerate() {
        assert_eq!(*s as usize, i, "{:?}", s);
    }
    for (i, l) in LoopId::ALL.iter().enumerate() {
        assert_eq!(*l as usize, i);
    }
}

#[test]
fn every_sfx_is_clean_and_starts_and_ends_quietly() {
    for s in Sfx::ALL {
        let b = sfx::render_sfx(s);
        assert_clean(s.name(), &b);
        assert!(b[0].abs() < 0.05, "{}: starts with a click ({})", s.name(), b[0]);
        assert!(b[b.len() - 1].abs() < 0.02, "{}: ends with a click", s.name());
        assert!(b.len() < 3 * SAMPLE_RATE as usize, "{}: unreasonably long", s.name());
    }
}

#[test]
fn every_loop_is_clean_and_seamless() {
    for l in LoopId::ALL {
        let b = sfx::render_loop(l);
        assert_clean(l.name(), &b);
        assert_seamless(l.name(), &b);
    }
}

#[test]
fn music_loops_are_clean_seamless_and_the_right_length() {
    for t in MusicTrack::ALL {
        let b = music::render_music(t);
        assert_clean(t.name(), &b);
        assert_seamless(t.name(), &b);
        assert_eq!(b.len(), music::loop_len(t));
        // Lively: not just a quiet bed.
        let rms = (b.iter().map(|x| x * x).sum::<f32>() / b.len() as f32).sqrt();
        assert!(rms > 0.07, "{}: rms {rms}", t.name());
    }
    // 8 bars at 150 bpm is 12.8 s; at 90 bpm it is 21.33 s.
    assert_eq!(music::loop_len(MusicTrack::Race), 564_480);
    assert_eq!(music::loop_len(MusicTrack::Menu), 940_800);
}

#[test]
fn coin_pitch_climbs_then_caps() {
    assert_eq!(coin_pitch(0), 1.0);
    let mut last = 0.0;
    for c in 0..8 {
        assert!(coin_pitch(c) > last);
        last = coin_pitch(c);
    }
    assert_eq!(coin_pitch(7), coin_pitch(100));
}

fn test_mixer() -> (Mixer, Sender<Voice>, Arc<Shared>, Arc<MusicBank>) {
    let shared = Arc::new(Shared::new());
    let music_bank = Arc::new(MusicBank::default());
    let bank = Bank {
        sfx: Sfx::ALL.iter().map(|&s| sfx::render_sfx(s).into()).collect(),
        loops: LoopId::ALL.iter().map(|&l| sfx::render_loop(l).into()).collect(),
    };
    let (tx, rx) = channel();
    let mixer = Mixer::new(rx, Arc::clone(&shared), &bank, Arc::clone(&music_bank));
    (mixer, tx, shared, music_bank)
}

#[test]
fn mixer_mixes_voices_and_stays_bounded() {
    let (mut m, tx, shared, _) = test_mixer();
    let loud: Arc<[f32]> = vec![0.9; 4000].into();
    for _ in 0..60 {
        tx.send(Voice { buf: Arc::clone(&loud), pos: 0.0, rate: 1.0, gain: 3.0, ui: false }).unwrap();
    }
    for id in LoopId::ALL {
        store(&shared.loop_gain[id as usize], 2.0);
    }
    let out: Vec<f32> = (&mut m).take(SAMPLE_RATE as usize / 2).collect();
    assert!(out.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    assert!(peak(&out) > 0.3);
    assert!(m.voices.len() <= MAX_VOICES);
}

#[test]
fn loops_follow_gain_and_go_quiet() {
    let (mut m, _tx, shared, _) = test_mixer();
    let quiet: Vec<f32> = (&mut m).take(2000).collect();
    assert_eq!(peak(&quiet), 0.0);
    store(&shared.loop_gain[LoopId::Wind as usize], 1.0);
    let _ = (&mut m).take(8000).count();
    let loud: Vec<f32> = (&mut m).take(8000).collect();
    assert!(peak(&loud) > 0.1);
    store(&shared.loop_gain[LoopId::Wind as usize], 0.0);
    let _ = (&mut m).take(SAMPLE_RATE as usize).count();
    let after: Vec<f32> = (&mut m).take(4000).collect();
    assert!(peak(&after) < 0.01);
}

#[test]
fn pause_silences_gameplay_but_not_ui() {
    let (mut m, tx, shared, _) = test_mixer();
    let tone: Arc<[f32]> = (0..30000).map(|i| (i as f32 * 0.05).sin() * 0.5).collect::<Vec<_>>().into();
    shared.paused.store(true, Ordering::Relaxed);
    let _ = (&mut m).take(SAMPLE_RATE as usize / 2).count(); // let the pause ramp settle
    tx.send(Voice { buf: Arc::clone(&tone), pos: 0.0, rate: 1.0, gain: 1.0, ui: false }).unwrap();
    let game: Vec<f32> = (&mut m).take(3000).collect();
    assert!(peak(&game) < 1e-3, "gameplay voice leaked while paused");
    tx.send(Voice { buf: Arc::clone(&tone), pos: 0.0, rate: 1.0, gain: 1.0, ui: true }).unwrap();
    let ui: Vec<f32> = (&mut m).take(3000).collect();
    assert!(peak(&ui) > 0.1, "UI voice should still play");
    // Resume: the frozen gameplay voice carries on.
    shared.paused.store(false, Ordering::Relaxed);
    let _ = (&mut m).take(SAMPLE_RATE as usize / 2).count();
    assert!(m.voices.iter().all(|v| v.pos > 0.0) || m.voices.is_empty());
}

#[test]
fn suspension_silences_music_loops_and_ui_even_if_game_pause_is_resumed() {
    let (mut mixer, tx, shared, bank) = test_mixer();
    let tone: Arc<[f32]> = vec![0.5; SAMPLE_RATE as usize * 2].into();
    bank.menu.set(Arc::clone(&tone)).ok();
    shared.track.store(MusicTrack::Menu.id(), Ordering::Relaxed);
    store(&shared.loop_gain[LoopId::Wind as usize], 1.0);
    let warm: Vec<f32> = (&mut mixer).take(8000).collect();
    assert!(peak(&warm) > 0.1);
    let music_pos = mixer.cur.pos;
    let mut audio = Audio::silent();
    audio.shared = Arc::clone(&shared);
    audio.set_suspended(true);
    audio.pause_all();
    audio.resume_all(); // A screen callback cannot undo platform silence.
    audio.set_volumes(1.0, 1.0, 1.0);
    for ui in [false, true] {
        tx.send(Voice { buf: Arc::clone(&tone), pos: 0.0, rate: 1.0, gain: 1.0, ui }).unwrap();
    }
    let quiet: Vec<f32> = (&mut mixer).take(8000).collect();
    assert_eq!(peak(&quiet), 0.0, "all output stops without a fade or another game frame");
    assert_eq!(mixer.cur.pos, music_pos, "music stays in place while the screen is off");
    assert!(mixer.voices.is_empty(), "background one-shots must not replay on return");
    audio.set_suspended(false);
    let resumed: Vec<f32> = (&mut mixer).take(8000).collect();
    assert!(peak(&resumed) > 0.1);
}

#[test]
fn music_crossfades_between_tracks() {
    let (mut m, _tx, shared, bank) = test_mixer();
    let a: Arc<[f32]> = vec![0.5; 100_000].into();
    let b: Arc<[f32]> = vec![-0.5; 100_000].into();
    bank.menu.set(a).ok();
    bank.race.set(b).ok();
    shared.track.store(1, Ordering::Relaxed);
    let first: Vec<f32> = (&mut m).take(SAMPLE_RATE as usize * 2).collect();
    assert!(*first.last().unwrap() > 0.1, "menu music should be playing");
    shared.track.store(2, Ordering::Relaxed);
    let fade: Vec<f32> = (&mut m).take(SAMPLE_RATE as usize * 3).collect();
    assert!(*fade.last().unwrap() < -0.1, "race music should have taken over");
    // No jump bigger than the smoothed fade allows.
    let max_step = fade.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
    assert!(max_step < 0.01, "crossfade stepped by {max_step}");
    shared.track.store(0, Ordering::Relaxed);
    let _ = (&mut m).take(SAMPLE_RATE as usize * 2).count();
    let off: Vec<f32> = (&mut m).take(2000).collect();
    assert!(peak(&off) < 1e-3, "music should fade out on None");
}

#[test]
fn music_waits_for_its_render() {
    let (mut m, _tx, shared, bank) = test_mixer();
    shared.track.store(2, Ordering::Relaxed);
    let before: Vec<f32> = (&mut m).take(5000).collect();
    assert_eq!(peak(&before), 0.0);
    bank.race.set(vec![0.4; 100_000].into()).ok();
    let after: Vec<f32> = (&mut m).take(SAMPLE_RATE as usize).collect();
    assert!(peak(&after) > 0.05);
}

#[test]
fn silent_audio_accepts_every_call() {
    let a = Audio::silent();
    assert!(!a.is_live());
    for s in Sfx::ALL {
        a.play(s, 1.0, 1.0);
    }
    for l in LoopId::ALL {
        a.set_loop(l, 1.0, 1.0);
    }
    a.set_music(Some(MusicTrack::Race));
    a.set_music(None);
    a.set_volumes(0.5, 0.5, 0.5);
    a.pause_all();
    a.resume_all();
    a.stop_loops();
    assert!(!a.music_ready(MusicTrack::Menu));
}

#[test]
fn startup_synthesis_time() {
    let t0 = std::time::Instant::now();
    let _ = Bank::synthesise();
    let sfx_ms = t0.elapsed().as_millis();
    let t1 = std::time::Instant::now();
    let _ = music::render_music(MusicTrack::Race);
    let race_ms = t1.elapsed().as_millis();
    let t2 = std::time::Instant::now();
    let _ = music::render_music(MusicTrack::Menu);
    let menu_ms = t2.elapsed().as_millis();
    println!("synthesis: sfx+loops {sfx_ms} ms, race {race_ms} ms, menu {menu_ms} ms (profile-dependent)");
}

#[test]
fn fast_exp_and_sin_are_accurate() {
    use crate::dsp::{fexp, fsin};
    let mut worst = 0.0f32;
    let mut x = -20.0f32;
    while x < 5.0 {
        worst = worst.max((fexp(x) / x.exp() - 1.0).abs());
        x += 0.0137;
    }
    assert!(worst < 3e-4, "fexp relative error {worst}");
    // Never grows for a negative argument (so envelopes cannot blow up).
    assert!(fexp(-1e-6) <= 1.0 && fexp(-3e-5) <= 1.0);
    let mut worst = 0.0f32;
    let mut c = -3.0f32;
    while c < 3.0 {
        worst = worst.max((fsin(c) - (c * std::f32::consts::TAU).sin()).abs());
        c += 0.00731;
    }
    assert!(worst < 2e-3, "fsin abs error {worst}");
}

#[test]
fn trump_is_a_raspberry_then_a_rising_zap() {
    let b = sfx::render_sfx(Sfx::Trump);
    assert_clean("trump", &b);
    let secs = b.len() as f32 / SAMPLE_RATE as f32;
    assert!((1.0..1.4).contains(&secs), "trump lasts {secs} s");
    // Both halves carry sound: the raspberry first, the zap after it.
    let sr = SAMPLE_RATE as usize;
    assert!(peak(&b[..sr / 2]) > 0.2, "raspberry too quiet");
    assert!(peak(&b[sr * 3 / 4..sr]) > 0.1, "zap too quiet");
    // The zap is brighter than the raspberry: more zero crossings per second.
    let crossings = |s: &[f32]| {
        s.windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count()
    };
    assert!(
        crossings(&b[sr * 9 / 10..sr * 11 / 10]) > 2 * crossings(&b[sr / 10..sr / 10 + sr / 5])
    );
}
