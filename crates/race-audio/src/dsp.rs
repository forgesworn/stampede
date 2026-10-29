//! Small DSP toolbox shared by the SFX and music synthesisers. Pure functions on
//! mono `f32` buffers at [`SAMPLE_RATE`]; no IO, no allocation beyond the buffers
//! they return.

use std::f32::consts::{PI, TAU};

pub const SAMPLE_RATE: u32 = 44_100;
pub const SR: f32 = SAMPLE_RATE as f32;

/// xorshift64*: tiny and deterministic, so every run synthesises the same bytes.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    pub fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Uniform in [-1, 1).
    pub fn signed(&mut self) -> f32 {
        self.f32() * 2.0 - 1.0
    }
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.f32()
    }
}

/// Seconds to a sample count (at least one).
pub fn n(secs: f32) -> usize {
    (secs * SR).round().max(1.0) as usize
}

/// A buffer of `secs` seconds filled by `f(t_seconds, index)`.
pub fn gen(secs: f32, mut f: impl FnMut(f32, usize) -> f32) -> Vec<f32> {
    (0..n(secs)).map(|i| f(i as f32 / SR, i)).collect()
}

/// Scale so the peak is `peak`.
pub fn normalize(buf: &mut [f32], peak: f32) {
    let m = buf.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    if m > 1e-9 {
        let k = peak / m;
        buf.iter_mut().for_each(|x| *x *= k);
    }
}

/// Short linear fades at both ends (no clicks).
pub fn declick(buf: &mut [f32], fade_in: f32, fade_out: f32) {
    let len = buf.len();
    let fi = n(fade_in).min(len);
    let fo = n(fade_out).min(len);
    for (i, x) in buf.iter_mut().enumerate().take(fi) {
        *x *= i as f32 / fi as f32;
    }
    for i in 0..fo {
        buf[len - 1 - i] *= i as f32 / fo as f32;
    }
}

/// One-pole low-pass in place.
pub fn lowpass(buf: &mut [f32], cutoff: f32) {
    let a = 1.0 - (-TAU * cutoff / SR).exp();
    let mut y = 0.0;
    for x in buf.iter_mut() {
        y += a * (*x - y);
        *x = y;
    }
}

/// One-pole high-pass in place.
pub fn highpass(buf: &mut [f32], cutoff: f32) {
    let a = (-TAU * cutoff / SR).exp();
    let (mut px, mut py) = (0.0, 0.0);
    for x in buf.iter_mut() {
        let y = a * (py + *x - px);
        px = *x;
        py = y;
        *x = y;
    }
}

/// Smooth, odd saturator (a cheap tanh); output within -1..1.
#[inline]
pub fn softclip(x: f32) -> f32 {
    if x.abs() >= 3.0 {
        x.signum()
    } else {
        (x * (27.0 + x * x) / (27.0 + 9.0 * x * x)).clamp(-1.0, 1.0)
    }
}

/// Fast sine of a phase in cycles (parabolic approximation, error < 0.1%).
#[inline]
pub fn fsin(cycles: f32) -> f32 {
    // Round to the nearest whole cycle without calling libm (cycles stays well inside
    // +-16384 for everything the synthesiser asks of it).
    let k = (cycles + 16384.5) as i32 as f32 - 16384.0;
    let x = (cycles - k) * TAU; // -pi..pi
    let y = 1.273_239_5 * x - 0.405_284_73 * x * x.abs();
    0.225 * (y * y.abs() - y) + y
}

/// Fast `exp` (relative error about 1e-4): plenty for envelopes, several times cheaper than
/// libm. Not for per-sample decay factors close to 1 (use `f32::exp` there).
#[inline]
pub fn fexp(x: f32) -> f32 {
    let x = (x * std::f32::consts::LOG2_E).clamp(-126.0, 126.0);
    let t = x as i32;
    let i = t - ((t as f32) > x) as i32; // floor
    let f = x - i as f32;
    let p = 1.0 + f * (std::f32::consts::LN_2 + f * (0.240_226_5 + f * (0.055_504_11 + f * (0.009_618_13 + f * 0.001_333_36))));
    f32::from_bits(((i + 127) as u32) << 23) * p
}

/// Method forms of the fast functions, so synthesis code reads like the maths:
/// `x.fe()` is `exp(x)`, `x.fs()` is `sin(x)` for x in radians.
pub trait Fast {
    fn fe(self) -> f32;
    fn fs(self) -> f32;
}

impl Fast for f32 {
    #[inline]
    fn fe(self) -> f32 {
        fexp(self)
    }
    #[inline]
    fn fs(self) -> f32 {
        fsin(self * (1.0 / TAU))
    }
}

/// MIDI note number to frequency.
pub fn mtof(m: f32) -> f32 {
    440.0 * 2f32.powf((m - 69.0) / 12.0)
}

/// Chamberlin state-variable filter. Stable for `fc` below about SR/6.
#[derive(Clone, Copy, Debug, Default)]
pub struct Svf {
    low: f32,
    band: f32,
    f: f32,
    q: f32,
}

impl Svf {
    pub fn new(fc: f32, quality: f32) -> Svf {
        let mut s = Svf::default();
        s.set(fc, quality);
        s
    }
    pub fn set(&mut self, fc: f32, quality: f32) {
        self.f = 2.0 * (PI * fc.clamp(10.0, 7000.0) / SR).sin();
        self.q = 1.0 / quality.max(0.5);
    }
    /// Returns `(low, band, high)`.
    #[inline]
    pub fn tick(&mut self, x: f32) -> (f32, f32, f32) {
        self.low += self.f * self.band;
        let high = x - self.low - self.q * self.band;
        self.band += self.f * high;
        (self.low, self.band, high)
    }
}

#[inline]
fn polyblep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// Band-limited saw, phase in 0..1, `dt` = frequency / sample rate.
#[inline]
pub fn saw(phase: f32, dt: f32) -> f32 {
    2.0 * phase - 1.0 - polyblep(phase, dt)
}

/// Band-limited pulse with width `pw`.
#[inline]
pub fn pulse(phase: f32, dt: f32, pw: f32) -> f32 {
    let mut y = if phase < pw { 1.0 } else { -1.0 };
    y += polyblep(phase, dt);
    y -= polyblep((phase + 1.0 - pw).fract(), dt);
    y
}

/// Attack / decay / sustain / release amplitude at time `t`, gate open for `gate`
/// seconds (release runs after that). Attack and decay are exponential-ish.
#[inline]
pub fn adsr(t: f32, gate: f32, a: f32, d: f32, s: f32, r: f32) -> f32 {
    if t < 0.0 {
        return 0.0;
    }
    let held = if t < a { t / a } else { s + (1.0 - s) * (-(t - a) / d.max(1e-4)).fe() };
    if t <= gate {
        held
    } else {
        let tg = gate;
        let at_gate = if tg < a { tg / a } else { s + (1.0 - s) * (-(tg - a) / d.max(1e-4)).fe() };
        at_gate * (1.0 - (t - gate) / r).max(0.0)
    }
}

/// Add `src * gain` into `dst` starting at `start`, wrapping round the end: notes
/// that ring past the loop point fold back to the beginning, so a looped buffer is
/// seamless by construction.
pub fn add_wrapped(dst: &mut [f32], start: usize, src: &[f32], gain: f32) {
    let len = dst.len();
    let mut idx = start % len;
    for &s in src {
        dst[idx] += s * gain;
        idx += 1;
        if idx == len {
            idx = 0;
        }
    }
}

/// Circular feedback echo: `taps` repeats, `delay` samples apart, each `fb` times
/// the last, wrapping round the buffer (for loops).
pub fn echo_circular(buf: &mut [f32], delay: usize, fb: f32, taps: usize, wet: f32) {
    let len = buf.len();
    let dry = buf.to_vec();
    let mut g = wet;
    for k in 1..=taps {
        let off = (delay * k) % len;
        // buf[i] += dry[i - off], wrapping: two straight runs, no per-sample modulo.
        for i in off..len {
            buf[i] += dry[i - off] * g;
        }
        for i in 0..off {
            buf[i] += dry[len - off + i] * g;
        }
        g *= fb;
    }
}

/// Circular comb filter with a damped feedback path (a building block of the loop
/// reverb). Two passes so the wrap-around is settled.
fn comb_circular(src: &[f32], delay: usize, g: f32, damp: f32) -> Vec<f32> {
    let len = src.len();
    let delay = delay % len;
    let mut out = src.to_vec();
    for _ in 0..2 {
        let mut lp = out[len - delay];
        for i in 0..len {
            let j = if i >= delay { i - delay } else { i + len - delay };
            lp += damp * (out[j] - lp);
            out[i] = src[i] + g * lp;
        }
    }
    out
}

/// A cheap lush reverb for loops: four damped circular combs averaged. Returns the
/// wet signal only.
pub fn reverb_circular(src: &[f32]) -> Vec<f32> {
    let delays = [1557usize, 1617, 1491, 1422, 1277, 1356];
    let mut wet = vec![0.0f32; src.len()];
    for (k, d) in delays.iter().enumerate() {
        let c = comb_circular(src, *d * 2 + k * 37, 0.84, 0.35);
        for (w, x) in wet.iter_mut().zip(c) {
            *w += x / delays.len() as f32;
        }
    }
    wet
}

/// Linear-interpolation resampler: `rate` 2.0 plays an octave up in half the time.
pub fn resample(buf: &[f32], rate: f32) -> Vec<f32> {
    let mut out = Vec::new();
    let mut pos = 0.0f32;
    while (pos as usize) + 1 < buf.len() {
        let i = pos as usize;
        let f = pos - i as f32;
        out.push(buf[i] * (1.0 - f) + buf[i + 1] * f);
        pos += rate;
    }
    out
}

/// Turn `raw` (at least `len + x` samples of stationary noise-like material) into
/// a seamless loop of `len` samples by cross-fading the tail into the head with
/// equal-power weights: sample `len - 1` and sample 0 are then neighbours in `raw`.
pub fn make_loop(raw: &[f32], len: usize) -> Vec<f32> {
    let x = raw.len() - len;
    let mut out = raw[..len].to_vec();
    for i in 0..x {
        let w = i as f32 / x as f32;
        out[i] = raw[i] * w.sqrt() + raw[len + i] * (1.0 - w).sqrt();
    }
    out
}
