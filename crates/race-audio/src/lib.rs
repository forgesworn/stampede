//! Stampede's synthesised sound bank and music on rodio (no decoders).
//!
//! Everything is generated at start-up: sound effects and looped voices
//! synchronously (a few tens of milliseconds), the two music loops on background
//! threads. One custom rodio source mixes one-shots, four live-parameter loops and
//! a cross-fading music player. With no output device [`Audio`] is a silent no-op.
//!
//! ```no_run
//! use race_audio::*;
//! let audio = Audio::new();
//! audio.set_music(Some(MusicTrack::Menu));
//! audio.play(Sfx::MenuTap, 1.0, 1.0);
//! audio.play(Sfx::Coin, 1.0, coin_pitch(3)); // fourth coin in a quick chain
//! audio.set_loop(LoopId::Wind, 0.5, 1.2);    // call every frame
//! ```

pub mod dsp;
pub mod music;
pub mod sfx;

pub use dsp::SAMPLE_RATE;
pub use sfx::coin_pitch;

use rodio::Source;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// One-shot sound effects (spec section 14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sfx {
    Jump,
    Land,
    Boost,
    /// Drift spark tier dings (tier 1, 2, 3: climbing and brighter).
    SparkTier1,
    SparkTier2,
    SparkTier3,
    /// Laser hit: zap and crackle.
    LaserZap,
    TargetShatter,
    ItemTick,
    ItemGet,
    EggThrow,
    EggHit,
    Cactus,
    ShieldUp,
    ShieldBlock,
    Lightning,
    MegaLaser,
    /// Ostrich bash: thump plus squawk.
    Bash,
    /// Bitcoin ding. Pass `coin_pitch(chain)` as the pitch for quick chains.
    Coin,
    CountdownBeep,
    CountdownGo,
    Lap,
    FinalLap,
    Finish,
    MenuTap,
    MenuBack,
    MenuSelect,
    /// Frog, ostrich and jockey laughing together after a tumble.
    Laugh,
    /// Laser Trump: a comic raspberry rolling into a rising laser zap.
    Trump,
}

impl Sfx {
    /// Every effect, in discriminant order.
    pub const ALL: [Sfx; 29] = [
        Sfx::Jump,
        Sfx::Land,
        Sfx::Boost,
        Sfx::SparkTier1,
        Sfx::SparkTier2,
        Sfx::SparkTier3,
        Sfx::LaserZap,
        Sfx::TargetShatter,
        Sfx::ItemTick,
        Sfx::ItemGet,
        Sfx::EggThrow,
        Sfx::EggHit,
        Sfx::Cactus,
        Sfx::ShieldUp,
        Sfx::ShieldBlock,
        Sfx::Lightning,
        Sfx::MegaLaser,
        Sfx::Bash,
        Sfx::Coin,
        Sfx::CountdownBeep,
        Sfx::CountdownGo,
        Sfx::Lap,
        Sfx::FinalLap,
        Sfx::Finish,
        Sfx::MenuTap,
        Sfx::MenuBack,
        Sfx::MenuSelect,
        Sfx::Laugh,
        Sfx::Trump,
    ];

    /// File-name-friendly name (used by the WAV renderer).
    pub fn name(self) -> &'static str {
        match self {
            Sfx::Jump => "jump",
            Sfx::Land => "land",
            Sfx::Boost => "boost",
            Sfx::SparkTier1 => "spark_tier1",
            Sfx::SparkTier2 => "spark_tier2",
            Sfx::SparkTier3 => "spark_tier3",
            Sfx::LaserZap => "laser_zap",
            Sfx::TargetShatter => "target_shatter",
            Sfx::ItemTick => "item_tick",
            Sfx::ItemGet => "item_get",
            Sfx::EggThrow => "egg_throw",
            Sfx::EggHit => "egg_hit",
            Sfx::Cactus => "cactus",
            Sfx::ShieldUp => "shield_up",
            Sfx::ShieldBlock => "shield_block",
            Sfx::Lightning => "lightning",
            Sfx::MegaLaser => "mega_laser",
            Sfx::Bash => "bash",
            Sfx::Coin => "coin",
            Sfx::CountdownBeep => "countdown_beep",
            Sfx::CountdownGo => "countdown_go",
            Sfx::Lap => "lap",
            Sfx::FinalLap => "final_lap",
            Sfx::Finish => "finish",
            Sfx::MenuTap => "menu_tap",
            Sfx::MenuBack => "menu_back",
            Sfx::MenuSelect => "menu_select",
            Sfx::Laugh => "laugh",
            Sfx::Trump => "trump",
        }
    }

    /// Mix level relative to the others (buffers are all normalised alike).
    fn base_gain(self) -> f32 {
        match self {
            Sfx::Jump => 0.55,
            Sfx::Land => 0.8,
            Sfx::Boost => 0.7,
            Sfx::SparkTier1 | Sfx::SparkTier2 | Sfx::SparkTier3 => 0.5,
            Sfx::LaserZap => 0.7,
            Sfx::TargetShatter => 0.7,
            Sfx::ItemTick => 0.45,
            Sfx::ItemGet => 0.65,
            Sfx::EggThrow => 0.6,
            Sfx::EggHit => 0.85,
            Sfx::Cactus => 0.8,
            Sfx::ShieldUp => 0.6,
            Sfx::ShieldBlock => 0.75,
            Sfx::Lightning => 0.95,
            Sfx::MegaLaser => 0.85,
            Sfx::Bash => 0.95,
            Sfx::Coin => 0.6,
            Sfx::CountdownBeep => 0.7,
            Sfx::CountdownGo => 0.8,
            Sfx::Lap => 0.65,
            Sfx::FinalLap => 0.75,
            Sfx::Finish => 0.85,
            Sfx::MenuTap | Sfx::MenuBack | Sfx::MenuSelect => 0.55,
            Sfx::Laugh => 0.65,
            Sfx::Trump => 0.85,
        }
    }

    /// UI sounds keep playing while the game is paused.
    fn is_ui(self) -> bool {
        matches!(self, Sfx::MenuTap | Sfx::MenuBack | Sfx::MenuSelect)
    }
}

/// Looped voices with live parameters; drive them every frame with
/// [`Audio::set_loop`]. Gain 0 silences a loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoopId {
    /// Ostrich footfalls. Pitch is also the step rate: 1.0 is 5 steps/s.
    Footsteps,
    /// Rushing wind. Pitch shifts its tone; gain follows speed.
    Wind,
    /// Drift skid scrape.
    Skid,
    /// Laser charge/aim hum.
    LaserHum,
}

impl LoopId {
    pub const ALL: [LoopId; 4] = [LoopId::Footsteps, LoopId::Wind, LoopId::Skid, LoopId::LaserHum];
    const COUNT: usize = 4;

    pub fn name(self) -> &'static str {
        match self {
            LoopId::Footsteps => "footsteps",
            LoopId::Wind => "wind",
            LoopId::Skid => "skid",
            LoopId::LaserHum => "laser_hum",
        }
    }

    fn base_gain(self) -> f32 {
        match self {
            LoopId::Footsteps => 0.9,
            LoopId::Wind => 0.55,
            LoopId::Skid => 0.55,
            LoopId::LaserHum => 0.4,
        }
    }
}

/// The two music loops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MusicTrack {
    /// Laid-back menu groove, 90 bpm.
    Menu,
    /// Driving race theme, 150 bpm.
    Race,
}

impl MusicTrack {
    pub const ALL: [MusicTrack; 2] = [MusicTrack::Menu, MusicTrack::Race];

    pub fn name(self) -> &'static str {
        match self {
            MusicTrack::Menu => "menu",
            MusicTrack::Race => "race",
        }
    }

    fn id(self) -> u32 {
        match self {
            MusicTrack::Menu => 1,
            MusicTrack::Race => 2,
        }
    }
}

// ---- shared state ------------------------------------------------------------

fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

fn store(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

/// Parameters shared with the mixer (f32 bits in atomics).
struct Shared {
    master: AtomicU32,
    music: AtomicU32,
    sfx: AtomicU32,
    paused: AtomicBool,
    suspended: AtomicBool,
    track: AtomicU32,
    loop_gain: [AtomicU32; LoopId::COUNT],
    loop_pitch: [AtomicU32; LoopId::COUNT],
}

impl Shared {
    fn new() -> Shared {
        let z = || AtomicU32::new(0f32.to_bits());
        let one = || AtomicU32::new(1f32.to_bits());
        Shared {
            master: one(),
            music: one(),
            sfx: one(),
            paused: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            track: AtomicU32::new(0),
            loop_gain: [z(), z(), z(), z()],
            loop_pitch: [one(), one(), one(), one()],
        }
    }
}

/// Synthesised effects and loop voices.
struct Bank {
    sfx: Vec<Arc<[f32]>>,
    loops: Vec<Arc<[f32]>>,
}

impl Bank {
    fn synthesise() -> Bank {
        Bank {
            sfx: Sfx::ALL.iter().map(|&s| sfx::render_sfx(s).into()).collect(),
            loops: LoopId::ALL.iter().map(|&l| sfx::render_loop(l).into()).collect(),
        }
    }
}

/// The music buffers, filled by background threads.
#[derive(Default)]
struct MusicBank {
    menu: OnceLock<Arc<[f32]>>,
    race: OnceLock<Arc<[f32]>>,
}

impl MusicBank {
    fn slot(&self, id: u32) -> Option<&OnceLock<Arc<[f32]>>> {
        match id {
            1 => Some(&self.menu),
            2 => Some(&self.race),
            _ => None,
        }
    }
    fn get(&self, id: u32) -> Option<Arc<[f32]>> {
        self.slot(id)?.get().cloned()
    }
}

// ---- the mixer -----------------------------------------------------------------

struct Voice {
    buf: Arc<[f32]>,
    pos: f32,
    rate: f32,
    gain: f32,
    ui: bool,
}

struct LoopVoice {
    buf: Arc<[f32]>,
    pos: f64,
    gain: f32,
    pitch: f32,
}

#[derive(Default)]
struct Slot {
    id: u32,
    buf: Option<Arc<[f32]>>,
    pos: usize,
    gain: f32,
}

impl Slot {
    fn read(&mut self) -> f32 {
        match &self.buf {
            Some(b) if !b.is_empty() => {
                let s = b[self.pos];
                self.pos += 1;
                if self.pos >= b.len() {
                    self.pos = 0;
                }
                s
            }
            _ => 0.0,
        }
    }
}

const MAX_VOICES: usize = 32;
const MUSIC_FADE_SECS: f32 = 1.2;

/// The single rodio source.
pub struct Mixer {
    rx: Receiver<Voice>,
    voices: Vec<Voice>,
    loops: Vec<LoopVoice>,
    shared: Arc<Shared>,
    music_bank: Arc<MusicBank>,
    cur: Slot,
    prev: Slot,
    master: f32,
    music: f32,
    sfx: f32,
    pause: f32,
    tick: u32,
}

impl Mixer {
    fn new(rx: Receiver<Voice>, shared: Arc<Shared>, bank: &Bank, music_bank: Arc<MusicBank>) -> Mixer {
        Mixer {
            rx,
            voices: Vec::new(),
            loops: bank.loops.iter().map(|b| LoopVoice { buf: Arc::clone(b), pos: 0.0, gain: 0.0, pitch: 1.0 }).collect(),
            music_bank,
            cur: Slot::default(),
            prev: Slot::default(),
            master: load(&shared.master),
            music: load(&shared.music),
            sfx: load(&shared.sfx),
            pause: 0.0,
            tick: 0,
            shared,
        }
    }

    /// Housekeeping done every 64 samples.
    fn control(&mut self) {
        while let Ok(v) = self.rx.try_recv() {
            if self.voices.len() >= MAX_VOICES {
                self.voices.remove(0);
            }
            self.voices.push(v);
        }
        let target = self.shared.track.load(Ordering::Relaxed);
        if target != self.cur.id {
            // The outgoing track fades from wherever it had got to.
            self.prev = std::mem::take(&mut self.cur);
            self.cur = Slot { id: target, buf: None, pos: 0, gain: 0.0 };
        }
        if self.cur.id != 0 && self.cur.buf.is_none() {
            self.cur.buf = self.music_bank.get(self.cur.id);
        }
    }

    fn sample(&mut self) -> f32 {
        if self.tick & 63 == 0 {
            self.control();
        }
        self.tick = self.tick.wrapping_add(1);

        // Screen-off/background silence overrides pause, music and UI sounds.
        if self.shared.suspended.load(Ordering::Relaxed) {
            self.voices.clear();
            return 0.0;
        }

        // Parameter smoothing (~20 ms).
        let k = 0.0011;
        self.master += (load(&self.shared.master) - self.master) * k;
        self.music += (load(&self.shared.music) - self.music) * k;
        self.sfx += (load(&self.shared.sfx) - self.sfx) * k;
        let pause_target = if self.shared.paused.load(Ordering::Relaxed) { 1.0 } else { 0.0 };
        self.pause += (pause_target - self.pause) * 0.0015;
        let live = 1.0 - self.pause;
        let advancing = self.pause < 0.995;

        // One-shots.
        let pause = self.pause;
        let mut fx = 0.0f32;
        self.voices.retain_mut(|v| {
            let frozen = pause > 0.995 && !v.ui;
            if frozen {
                return true;
            }
            let i = v.pos as usize;
            if i + 1 >= v.buf.len() {
                return false;
            }
            let f = v.pos - i as f32;
            let s = (v.buf[i] * (1.0 - f) + v.buf[i + 1] * f) * v.gain;
            fx += if v.ui { s } else { s * (1.0 - pause) };
            v.pos += v.rate;
            true
        });

        // Looped voices.
        for (idx, lv) in self.loops.iter_mut().enumerate() {
            let tg = load(&self.shared.loop_gain[idx]);
            let tp = load(&self.shared.loop_pitch[idx]);
            lv.gain += (tg - lv.gain) * 0.0012;
            lv.pitch += (tp - lv.pitch) * 0.0008;
            if lv.gain < 1e-4 && tg < 1e-4 {
                continue;
            }
            if !advancing {
                continue;
            }
            let len = lv.buf.len();
            let i = lv.pos as usize;
            let f = (lv.pos - i as f64) as f32;
            let a = lv.buf[i];
            let b = lv.buf[if i + 1 == len { 0 } else { i + 1 }];
            fx += (a * (1.0 - f) + b * f) * lv.gain * live;
            lv.pos += lv.pitch as f64;
            if lv.pos >= len as f64 {
                lv.pos -= len as f64;
            }
        }

        // Music with an equal-power crossfade.
        let step = 1.0 / (MUSIC_FADE_SECS * dsp::SR);
        let mut mus = 0.0;
        if self.prev.id != 0 {
            self.prev.gain -= step;
            if self.prev.gain <= 0.0 {
                self.prev = Slot::default();
            } else {
                mus += self.prev.read() * self.prev.gain.sqrt();
            }
        }
        if self.cur.id != 0 && self.cur.buf.is_some() {
            self.cur.gain = (self.cur.gain + step).min(1.0);
            mus += self.cur.read() * self.cur.gain.sqrt();
        }
        // Pausing ducks the music rather than stopping it.
        let mus = mus * (1.0 - 0.6 * self.pause);

        let x = (fx * self.sfx * 0.8 + mus * self.music * 0.6) * self.master;
        dsp::softclip(x)
    }
}

impl Iterator for Mixer {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        Some(self.sample())
    }
}

impl Source for Mixer {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

// ---- the facade ---------------------------------------------------------------------

/// The app's audio facade. Cheap to call every frame; silent if there is no
/// output device. Holds the device open, so keep it on the thread that made it.
pub struct Audio {
    tx: Option<Sender<Voice>>,
    shared: Arc<Shared>,
    bank: Option<Arc<Bank>>,
    music_bank: Arc<MusicBank>,
    // Keeps the device open; not Send, so Audio lives on the main thread.
    _stream: Option<rodio::OutputStream>,
}

impl Default for Audio {
    fn default() -> Audio {
        Audio::new()
    }
}

impl Audio {
    /// Open the default output device and synthesise the sound bank. Falls back
    /// to silence (every call a no-op) on any error. Music loops are rendered on
    /// background threads; until a loop is ready, `set_music` waits for it.
    pub fn new() -> Audio {
        let shared = Arc::new(Shared::new());
        let music_bank = Arc::new(MusicBank::default());
        let (stream, handle) = match rodio::OutputStream::try_default() {
            Ok(s) => s,
            Err(e) => {
                log::warn!("audio unavailable ({e}); running silently");
                return Audio::silent();
            }
        };
        let t0 = std::time::Instant::now();
        let bank = Arc::new(Bank::synthesise());
        log::info!("synthesised sfx and loops in {} ms", t0.elapsed().as_millis());
        spawn_music(&music_bank);
        let (tx, rx) = channel();
        let mixer = Mixer::new(rx, Arc::clone(&shared), &bank, Arc::clone(&music_bank));
        if let Err(e) = handle.play_raw(mixer) {
            log::warn!("audio stream failed ({e}); running silently");
            return Audio::silent();
        }
        Audio { tx: Some(tx), shared, bank: Some(bank), music_bank, _stream: Some(stream) }
    }

    /// No device, no synthesis (tests, screenshots, headless runs).
    pub fn silent() -> Audio {
        Audio { tx: None, shared: Arc::new(Shared::new()), bank: None, music_bank: Arc::new(MusicBank::default()), _stream: None }
    }

    /// True if a device is open and sounds will actually play.
    pub fn is_live(&self) -> bool {
        self.tx.is_some()
    }

    /// True once `track`'s loop has been rendered (always false when silent).
    pub fn music_ready(&self, track: MusicTrack) -> bool {
        self.is_live() && self.music_bank.get(track.id()).is_some()
    }

    /// Play a one-shot. `gain` multiplies the effect's own level (1.0 = normal);
    /// `pitch` is a playback-rate ratio (1.0 = as designed, 2.0 = an octave up and
    /// twice as fast), clamped to 0.25..4.
    pub fn play(&self, sfx: Sfx, gain: f32, pitch: f32) {
        if self.is_suspended() { return; }
        let (Some(tx), Some(bank)) = (&self.tx, &self.bank) else { return };
        let _ = tx.send(Voice {
            buf: Arc::clone(&bank.sfx[sfx as usize]),
            pos: 0.0,
            rate: pitch.clamp(0.25, 4.0),
            gain: gain.clamp(0.0, 4.0) * sfx.base_gain(),
            ui: sfx.is_ui(),
        });
    }

    /// Set a looped voice's live parameters. Call every frame (cheap); the mixer
    /// smooths changes. `gain` 0 silences it; `pitch` is a playback-rate ratio,
    /// clamped to 0.25..4.
    pub fn set_loop(&self, id: LoopId, gain: f32, pitch: f32) {
        store(&self.shared.loop_gain[id as usize], gain.clamp(0.0, 2.0) * id.base_gain());
        store(&self.shared.loop_pitch[id as usize], pitch.clamp(0.25, 4.0));
    }

    /// Silence every looped voice.
    pub fn stop_loops(&self) {
        for id in LoopId::ALL {
            store(&self.shared.loop_gain[id as usize], 0.0);
        }
    }

    /// Start (cross-fading from the current track) or stop (`None`, fades out) the
    /// music. Setting the track that is already playing does nothing.
    pub fn set_music(&self, track: Option<MusicTrack>) {
        self.shared.track.store(track.map_or(0, MusicTrack::id), Ordering::Relaxed);
    }

    /// Master, music and sound-effect volumes, each 0..1.
    pub fn set_volumes(&self, master: f32, music: f32, sfx: f32) {
        store(&self.shared.master, master.clamp(0.0, 1.0));
        store(&self.shared.music, music.clamp(0.0, 1.0));
        store(&self.shared.sfx, sfx.clamp(0.0, 1.0));
    }

    /// Pause: loops and gameplay one-shots are silenced and frozen, the music ducks;
    /// UI sounds (menu tap/back/select) still play.
    pub fn pause_all(&self) {
        self.shared.paused.store(true, Ordering::Relaxed);
    }

    /// Undo [`Audio::pause_all`].
    pub fn resume_all(&self) {
        self.shared.paused.store(false, Ordering::Relaxed);
    }

    /// Silence all output immediately while backgrounded, including music and UI.
    /// Independent of game pause and volumes; resuming a race cannot undo it.
    pub fn set_suspended(&self, suspended: bool) {
        self.shared.suspended.store(suspended, Ordering::Relaxed);
    }

    /// Whether platform lifecycle currently silences all output.
    pub fn is_suspended(&self) -> bool {
        self.shared.suspended.load(Ordering::Relaxed)
    }
}

fn render_into(track: MusicTrack, bank: &MusicBank) {
    let t0 = std::time::Instant::now();
    let buf: Arc<[f32]> = music::render_music(track).into();
    log::info!("synthesised {} music in {} ms", track.name(), t0.elapsed().as_millis());
    if let Some(slot) = bank.slot(track.id()) {
        let _ = slot.set(buf);
    }
}

fn spawn_music(bank: &Arc<MusicBank>) {
    for track in MusicTrack::ALL {
        let b = Arc::clone(bank);
        let spawned = std::thread::Builder::new().name(format!("stampede-music-{}", track.name())).spawn(move || render_into(track, &b));
        // If a thread cannot be spawned, render inline rather than go without.
        if spawned.is_err() {
            render_into(track, bank);
        }
    }
}

#[cfg(test)]
mod tests;
