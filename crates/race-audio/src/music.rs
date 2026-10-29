//! The two music loops, rendered once into buffers. Every note is written with
//! wrap-around ([`add_wrapped`]) and every effect is circular, so the loop point
//! is seamless by construction: the buffer is one period of a periodic signal.

use crate::dsp::*;
use crate::MusicTrack;
use std::f32::consts::TAU;

pub const RACE_BPM: f32 = 150.0;
pub const MENU_BPM: f32 = 90.0;
pub const BARS: usize = 8;
const STEPS_PER_BAR: usize = 16;

/// Render a music loop.
pub fn render_music(track: MusicTrack) -> Vec<f32> {
    match track {
        MusicTrack::Menu => menu(),
        MusicTrack::Race => race(),
    }
}

/// Length in samples of one loop of `track`.
pub fn loop_len(track: MusicTrack) -> usize {
    let bpm = match track {
        MusicTrack::Menu => MENU_BPM,
        MusicTrack::Race => RACE_BPM,
    };
    step_len(bpm) * STEPS_PER_BAR * BARS
}

fn step_len(bpm: f32) -> usize {
    (SR * 60.0 / bpm / 4.0).round() as usize
}

/// A note on the 16th-note grid: `(start step within the bar, length in steps, midi)`.
type Note = (usize, f32, f32);

// ---- drums (rendered once, copied into place) ----------------------------------

fn kick(punch: f32) -> Vec<f32> {
    let mut rng = Rng::new(1);
    let mut ph = 0.0f32;
    gen(0.34, |t, _| {
        let f = 46.0 + 125.0 * (-t * 42.0).fe();
        ph = (ph + f / SR).fract();
        let body = softclip((TAU * ph).fs() * (1.0 + punch)) * (-t * 8.5).fe() * (1.0 - (-t * 1200.0).fe());
        let click = rng.signed() * (-t * 900.0).fe() * (1.0 - (-t * 5000.0).fe()) * 0.35 * punch;
        body + click
    })
}

fn clap(seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut v = gen(0.28, |t, _| {
        // Three quick smacks then a tail.
        let mut e = (-(t * 28.0)).fe() * 0.35;
        for k in 0..3 {
            let tt = t - k as f32 * 0.011;
            if tt >= 0.0 {
                e += (-tt * 300.0).fe() * 0.8;
            }
        }
        rng.signed() * e
    });
    highpass(&mut v, 700.0);
    lowpass(&mut v, 6200.0);
    // A body tone under the noise.
    for (i, x) in v.iter_mut().enumerate() {
        let t = i as f32 / SR;
        *x += (TAU * (190.0 - 60.0 * t) * t).fs() * (-t * 38.0).fe() * 0.5;
    }
    v
}

fn hat(secs: f32, decay: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut v = gen(secs, |t, _| rng.signed() * (-t * decay).fe() * (1.0 - (-t * 2500.0).fe()));
    highpass(&mut v, 7000.0);
    v
}

fn crash() -> Vec<f32> {
    let mut rng = Rng::new(77);
    let mut v = gen(1.4, |t, _| rng.signed() * (-t * 3.2).fe() * (1.0 - (-t * 800.0).fe()));
    highpass(&mut v, 5200.0);
    v
}

/// Sidechain "pump": dips right after each kick and swells back.
fn duck_curve(len: usize, kicks: &[usize], depth: f32, tau: f32) -> Vec<f32> {
    let mut d = vec![1.0f32; len];
    let span = n(tau * 5.0);
    for &k in kicks {
        for j in 0..span {
            let t = j as f32 / SR;
            let g = 1.0 - depth * (1.0 - (-t / 0.003).fe()) * (-t / tau).fe();
            let idx = (k + j) % len;
            if g < d[idx] {
                d[idx] = g;
            }
        }
    }
    d
}

// ---- race ----------------------------------------------------------------------

fn bass_note(freq: f32, gate: f32) -> Vec<f32> {
    let dt = freq / SR;
    let mut p = 0.0f32;
    let mut lp = 0.0f32;
    let a = 1.0 - (-TAU * (freq * 7.0).min(2600.0) / SR).fe();
    gen(gate + 0.03, |t, i| {
        p = (p + dt).fract();
        let x = 0.7 * saw(p, dt) + 0.9 * fsin(i as f32 * dt) + 0.5 * fsin(i as f32 * dt * 0.5);
        let x = softclip(x * 1.8);
        lp += a * (x - lp);
        lp * adsr(t, gate, 0.003, 0.18, 0.55, 0.03)
    })
}

fn stab_chord(notes: &[f32], gate: f32) -> Vec<f32> {
    let len = n(gate + 0.05);
    let mut buf = vec![0.0f32; len];
    for &m in notes {
        let f = mtof(m);
        let (dt1, dt2) = (f / SR, f * 1.0065 / SR);
        let (mut p1, mut p2) = (0.0f32, 0.41f32);
        for (i, b) in buf.iter_mut().enumerate() {
            let t = i as f32 / SR;
            p1 = (p1 + dt1).fract();
            p2 = (p2 + dt2).fract();
            *b += (saw(p1, dt1) * 0.5 + pulse(p2, dt2, 0.35) * 0.5) * adsr(t, gate, 0.002, 0.05, 0.25, 0.05);
        }
    }
    lowpass(&mut buf, 3600.0);
    buf
}

fn lead_note(freq: f32, gate: f32, bright: f32) -> Vec<f32> {
    let release = 0.07;
    let len = n(gate + release);
    let (mut p1, mut p2) = (0.0f32, 0.2f32);
    let mut lp = 0.0f32;
    let a = 1.0 - (-TAU * (2600.0 + 3600.0 * bright) / SR).fe();
    (0..len)
        .map(|i| {
            let t = i as f32 / SR;
            let vib = 1.0 + 0.006 * fsin(5.6 * t) * ((t - 0.1) * 8.0).clamp(0.0, 1.0);
            let f = freq * vib;
            p1 = (p1 + f * 0.9975 / SR).fract();
            p2 = (p2 + f * 1.0025 / SR).fract();
            let x = 0.5 * saw(p1, f / SR) + 0.35 * pulse(p2, f / SR, 0.5) + 0.3 * fsin(f * t);
            lp += a * (x - lp);
            lp * adsr(t, gate, 0.004, 0.1, 0.72, release)
        })
        .collect()
}

fn race() -> Vec<f32> {
    let step = step_len(RACE_BPM);
    let total = step * STEPS_PER_BAR * BARS;
    let bar = step * STEPS_PER_BAR;
    let step_s = step as f32 / SR;

    // Chords per bar (A minor: Am F G E): bass root and stab voicing.
    let roots = [45.0f32, 41.0, 43.0, 40.0];
    let stabs: [[f32; 3]; 4] = [[57.0, 60.0, 64.0], [57.0, 60.0, 65.0], [55.0, 59.0, 62.0], [56.0, 59.0, 64.0]];

    let (kick_s, clap_s, hat_c, hat_o, crash_s) = (kick(1.0), clap(5), hat(0.05, 90.0, 6), hat(0.24, 16.0, 7), crash());

    // Drums.
    let mut drums = vec![0.0f32; total];
    let mut kicks = Vec::new();
    for b in 0..BARS {
        let base = b * bar;
        for s in 0..STEPS_PER_BAR {
            let at = base + s * step;
            let is_fill = (b % 4 == 3) && s >= 12;
            if s % 4 == 0 {
                add_wrapped(&mut drums, at, &kick_s, 0.95);
                kicks.push(at);
            } else if (s == 14 && b % 2 == 1) || (s == 10 && b % 2 == 0) {
                add_wrapped(&mut drums, at, &kick_s, 0.7);
                kicks.push(at);
            }
            if (s == 4 || s == 12) && !is_fill {
                add_wrapped(&mut drums, at, &clap_s, 0.62);
            }
            if is_fill {
                // Snare roll into the next section, getting louder.
                add_wrapped(&mut drums, at, &clap_s, 0.35 + 0.1 * (s - 12) as f32);
            } else if s % 4 == 2 {
                add_wrapped(&mut drums, at, &hat_o, 0.30);
            } else if s % 2 == 0 {
                add_wrapped(&mut drums, at, &hat_c, 0.22);
            } else {
                add_wrapped(&mut drums, at, &hat_c, 0.12);
            }
        }
        if b % 4 == 0 {
            add_wrapped(&mut drums, base, &crash_s, 0.22);
        }
    }
    let duck = duck_curve(total, &kicks, 0.62, 0.11);

    // Bass: a galloping 3+3+2 figure with octave jumps.
    let pattern: [(usize, f32, f32); 7] = [(0, 3.0, 0.0), (3, 3.0, 0.0), (6, 2.0, 12.0), (8, 3.0, 0.0), (11, 3.0, 0.0), (14, 1.0, 7.0), (15, 1.0, 10.0)];
    let mut bass = vec![0.0f32; total];
    for b in 0..BARS {
        let root = roots[b % 4];
        for &(s, len, off) in &pattern {
            let note = bass_note(mtof(root + off), len * step_s * 0.92);
            add_wrapped(&mut bass, b * bar + s * step, &note, 0.42);
        }
    }

    // Off-beat chord stabs.
    let mut stab = vec![0.0f32; total];
    for b in 0..BARS {
        let chord = bar_voicing(&stabs[b % 4], b >= 4);
        let hit = stab_chord(&chord, step_s * 1.6);
        for s in [2usize, 6, 10, 14] {
            let g = if s == 10 && b % 2 == 1 { 0.0 } else { 1.0 };
            add_wrapped(&mut stab, b * bar + s * step, &hit, 0.27 * g);
        }
    }

    // Lead melody.
    let section_a: [&[Note]; 4] = [
        &[(0, 3.0, 69.0), (3, 1.0, 72.0), (4, 2.0, 76.0), (6, 2.0, 72.0), (8, 3.0, 69.0), (11, 1.0, 72.0), (12, 4.0, 76.0)],
        &[(0, 3.0, 65.0), (3, 1.0, 69.0), (4, 2.0, 72.0), (6, 2.0, 69.0), (8, 3.0, 65.0), (11, 1.0, 69.0), (12, 4.0, 72.0)],
        &[(0, 3.0, 67.0), (3, 1.0, 71.0), (4, 2.0, 74.0), (6, 2.0, 71.0), (8, 3.0, 67.0), (11, 1.0, 71.0), (12, 4.0, 74.0)],
        &[(0, 3.0, 68.0), (3, 1.0, 71.0), (4, 2.0, 76.0), (6, 2.0, 74.0), (8, 2.0, 71.0), (10, 2.0, 68.0), (12, 4.0, 71.0)],
    ];
    let section_b: [&[Note]; 4] = [
        &[(0, 2.0, 81.0), (2, 2.0, 79.0), (4, 2.0, 76.0), (6, 2.0, 79.0), (8, 4.0, 81.0), (12, 2.0, 83.0), (14, 2.0, 84.0)],
        &[(0, 2.0, 84.0), (2, 2.0, 81.0), (4, 2.0, 77.0), (6, 2.0, 81.0), (8, 4.0, 84.0), (12, 2.0, 81.0), (14, 2.0, 77.0)],
        &[(0, 2.0, 83.0), (2, 2.0, 79.0), (4, 2.0, 74.0), (6, 2.0, 79.0), (8, 4.0, 83.0), (12, 2.0, 81.0), (14, 2.0, 79.0)],
        &[(0, 2.0, 80.0), (2, 2.0, 76.0), (4, 2.0, 80.0), (6, 2.0, 83.0), (8, 2.0, 80.0), (10, 2.0, 76.0), (12, 4.0, 83.0)],
    ];
    let mut lead = vec![0.0f32; total];
    for b in 0..BARS {
        let notes = if b < 4 { section_a[b] } else { section_b[b - 4] };
        for &(s, len, m) in notes {
            let note = lead_note(mtof(m), len * step_s * 0.9, if b < 4 { 0.0 } else { 0.5 });
            add_wrapped(&mut lead, b * bar + s * step, &note, 0.40);
        }
    }
    // Dotted-eighth echo (3 steps), wrapping round the loop.
    echo_circular(&mut lead, step * 3, 0.5, 3, 0.4);

    // Mix: the bass and stabs duck under the kick.
    let mut out = vec![0.0f32; total];
    for i in 0..total {
        let x = drums[i] * 0.85 + (bass[i] * 0.9 + stab[i]) * duck[i] + lead[i] * (0.55 + 0.45 * duck[i]);
        out[i] = softclip(x * 1.15);
    }
    normalize(&mut out, 0.85);
    out
}

/// Spread a stab chord: second half of the tune sits an octave higher.
fn bar_voicing(base: &[f32; 3], high: bool) -> Vec<f32> {
    base.iter().map(|m| if high { m + 12.0 } else { *m }).collect()
}

// ---- menu ------------------------------------------------------------------------

fn epiano(freq: f32, secs: f32, vel: f32) -> Vec<f32> {
    // Running exponentials (one multiply per sample) instead of exp() calls.
    let len = n(secs);
    // libm exp here: a per-sample decay factor needs its full precision.
    let (k_body, k_idx, k_tine) = ((-2.4 / SR).exp(), (-3.5 / SR).exp(), (-38.0 / SR).exp());
    let (mut e_body, mut e_idx, mut e_tine) = (1.0f32, vel * 1.1, 0.35f32);
    let tine_len = n(0.16);
    let step = freq / SR;
    let mut c = 0.0f32;
    (0..len)
        .map(|i| {
            // Tine: a bright FM bell that dies fast, over a mellow body.
            let tine = if i < tine_len { fsin(c * 14.0) * e_tine } else { 0.0 };
            let body = fsin(c + (0.2 + e_idx) * fsin(c)) * e_body;
            let att = (i as f32 * (1.0 / 80.0)).min(1.0);
            e_body *= k_body;
            e_idx *= k_idx;
            e_tine *= k_tine;
            c += step;
            if c >= 1.0 {
                c -= 1.0;
            }
            (body + tine) * att
        })
        .collect()
}

fn pad_envelope(secs: f32) -> Vec<f32> {
    (0..n(secs)).map(|i| adsr(i as f32 / SR, secs * 0.8, 0.5, 1.0, 1.0, secs * 0.2)).collect()
}

fn pad_chord(notes: &[f32], env: &[f32]) -> Vec<f32> {
    let mut buf = vec![0.0f32; env.len()];
    for &m in notes {
        let f = mtof(m);
        let (d1, d2) = (f * 0.997 / SR, f * 1.004 / SR);
        let (mut p1, mut p2) = (0.1f32, 0.6f32);
        for b in buf.iter_mut() {
            p1 += d1;
            if p1 >= 1.0 {
                p1 -= 1.0;
            }
            p2 += d2;
            if p2 >= 1.0 {
                p2 -= 1.0;
            }
            *b += (p1 * 2.0 - 1.0) + (p2 * 2.0 - 1.0);
        }
    }
    lowpass(&mut buf, 780.0);
    lowpass(&mut buf, 1500.0);
    for (b, e) in buf.iter_mut().zip(env) {
        *b *= e;
    }
    buf
}

fn pluck_bass(freq: f32, gate: f32) -> Vec<f32> {
    gen(gate + 0.1, |t, i| {
        let c = i as f32 * freq / SR;
        let x = fsin(c) + 0.6 * fsin(2.0 * c) * (-t * 3.0).fe() + 0.2 * fsin(3.0 * c) * (-t * 5.0).fe();
        x * (-t * 2.8).fe() * (1.0 - (-t * 900.0).fe()) * (1.0 - ((t - gate) / 0.1).clamp(0.0, 1.0))
    })
}

fn soft_lead(freq: f32, gate: f32) -> Vec<f32> {
    let release = 0.25;
    gen(gate + release, |t, _| {
        let vib = 1.0 + 0.008 * fsin(5.0 * t) * ((t - 0.25) * 4.0).clamp(0.0, 1.0);
        let c = freq * vib * t;
        let x = fsin(c) + 0.28 * fsin(2.0 * c) + 0.1 * fsin(3.0 * c) + 0.3 * fsin(c * 1.004) ;
        x * adsr(t, gate, 0.03, 0.2, 0.75, release)
    })
}

fn rim() -> Vec<f32> {
    let mut rng = Rng::new(31);
    let mut v = gen(0.12, |t, _| {
        let tone = fsin(t * 410.0) * (-t * 55.0).fe() + fsin(t * 1210.0) * (-t * 90.0).fe() * 0.5;
        tone + rng.signed() * (-t * 260.0).fe() * 0.35
    });
    highpass(&mut v, 200.0);
    v
}

fn shaker(seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let mut v = gen(0.1, |t, _| rng.signed() * (-t * 38.0).fe() * (1.0 - (-t * 400.0).fe()));
    highpass(&mut v, 5500.0);
    lowpass(&mut v, 11000.0);
    v
}

fn menu() -> Vec<f32> {
    let step = step_len(MENU_BPM);
    let total = step * STEPS_PER_BAR * BARS;
    let bar = step * STEPS_PER_BAR;
    let step_s = step as f32 / SR;
    // 8th-note swing: off-beat 8ths land a little late.
    let swing = |s: usize| -> usize {
        if s % 4 == 2 {
            (step as f32 * 0.22) as usize
        } else {
            0
        }
    };

    // Dm9 | Bbmaj7 | Gm7 | A7 (twice).
    let keys: [[f32; 4]; 4] = [[53.0, 57.0, 60.0, 64.0], [50.0, 53.0, 57.0, 62.0], [50.0, 53.0, 58.0, 62.0], [49.0, 52.0, 55.0, 57.0]];
    let pads: [[f32; 4]; 4] = [[50.0, 57.0, 60.0, 65.0], [46.0, 57.0, 60.0, 65.0], [43.0, 58.0, 62.0, 65.0], [45.0, 55.0, 61.0, 64.0]];
    let bass_roots = [38.0f32, 46.0, 43.0, 45.0];

    // Keys: a long chord on the downbeat and a lazy answer.
    let mut keys_bus = vec![0.0f32; total];
    for b in 0..BARS {
        let chord = &keys[b % 4];
        for (k, &m) in chord.iter().enumerate() {
            let strum = (k as f32 * 0.012 * SR) as usize; // a hair of strum
            let long = epiano(mtof(m), 1.25, 0.8);
            add_wrapped(&mut keys_bus, b * bar + strum, &long, 0.16);
            let short = epiano(mtof(m + if k == 3 { 12.0 } else { 0.0 }), 0.8, 0.5);
            add_wrapped(&mut keys_bus, b * bar + 10 * step + swing(10) + strum, &short, 0.1);
        }
    }

    // Pad underneath each bar, overlapping the next.
    let mut pad_bus = vec![0.0f32; total];
    let pad_env = pad_envelope(step_s * 18.0);
    for b in 0..BARS {
        let p = pad_chord(&pads[b % 4], &pad_env);
        add_wrapped(&mut pad_bus, b * bar, &p, 0.032);
    }

    // Bass: root, a pickup and a fifth.
    let mut bass_bus = vec![0.0f32; total];
    for b in 0..BARS {
        let r = bass_roots[b % 4];
        for (s, len, off, g) in [(0usize, 5.0f32, 0.0f32, 1.0f32), (6, 3.0, 0.0, 0.7), (10, 4.0, 7.0, 0.8), (14, 2.0, 5.0, 0.5)] {
            let note = pluck_bass(mtof(r + off), len * step_s);
            add_wrapped(&mut bass_bus, b * bar + s * step + swing(s), &note, 0.5 * g);
        }
    }

    // Soft drums: kick on 1 and the "and" of 3, rim on 3, swung shaker.
    let (kick_s, rim_s, sh_a, sh_b) = (kick(0.2), rim(), shaker(41), shaker(42));
    let mut drums = vec![0.0f32; total];
    for b in 0..BARS {
        let base = b * bar;
        add_wrapped(&mut drums, base, &kick_s, 0.55);
        add_wrapped(&mut drums, base + 10 * step + swing(10), &kick_s, 0.4);
        add_wrapped(&mut drums, base + 8 * step, &rim_s, 0.5);
        if b % 4 == 3 {
            add_wrapped(&mut drums, base + 15 * step, &rim_s, 0.2);
        }
        for s in (0..16).step_by(2) {
            let g = if s % 4 == 0 { 0.1 } else { 0.07 };
            add_wrapped(&mut drums, base + s * step + swing(s), if s % 8 == 0 { &sh_a } else { &sh_b }, g);
        }
    }

    // Melody (D minor pentatonic with the A7's C sharp).
    let tune: [&[Note]; 8] = [
        &[(2, 3.0, 74.0), (6, 2.0, 77.0), (8, 6.0, 81.0)],
        &[(0, 4.0, 74.0), (6, 2.0, 72.0), (8, 6.0, 70.0)],
        &[(2, 3.0, 70.0), (6, 2.0, 72.0), (8, 4.0, 74.0), (12, 4.0, 77.0)],
        &[(0, 6.0, 76.0), (8, 2.0, 73.0), (10, 6.0, 69.0)],
        &[(2, 3.0, 77.0), (6, 2.0, 81.0), (8, 4.0, 84.0), (12, 4.0, 81.0)],
        &[(2, 3.0, 81.0), (6, 2.0, 77.0), (8, 6.0, 74.0)],
        &[(0, 4.0, 79.0), (4, 2.0, 77.0), (6, 2.0, 74.0), (8, 4.0, 72.0), (12, 4.0, 70.0)],
        &[(0, 3.0, 69.0), (4, 2.0, 73.0), (6, 2.0, 76.0), (8, 8.0, 74.0)],
    ];
    let mut lead_bus = vec![0.0f32; total];
    for (b, notes) in tune.iter().enumerate() {
        for &(s, len, m) in notes.iter() {
            let note = soft_lead(mtof(m), len * step_s * 0.95);
            add_wrapped(&mut lead_bus, b * bar + s * step + swing(s), &note, 0.16);
        }
    }
    echo_circular(&mut lead_bus, step * 3, 0.55, 4, 0.45);

    // A touch of room on the harmonic bus.
    let mut room_in = vec![0.0f32; total];
    for i in 0..total {
        room_in[i] = keys_bus[i] * 0.8 + pad_bus[i] * 1.2 + lead_bus[i];
    }
    let wet = reverb_circular(&room_in);

    let mut out = vec![0.0f32; total];
    for i in 0..total {
        let x = drums[i] + bass_bus[i] * 0.9 + keys_bus[i] + pad_bus[i] + lead_bus[i] + wet[i] * 0.45;
        out[i] = softclip(x * 1.2);
    }
    normalize(&mut out, 0.8);
    out
}
