# race-audio API

Synthesised sound bank and music on rodio 0.20 (`default-features = false`, no decoders;
same device path as Twang Thang: cpal / AAudio on Android). Mono, 44.1 kHz. Nothing is
loaded from disk.

```rust
use race_audio::{Audio, Sfx, LoopId, MusicTrack, coin_pitch};
```

## Audio

| Call | Meaning |
|---|---|
| `Audio::new() -> Audio` | Open the default output device, synthesise SFX + loops (synchronously, ~60-90 ms desktop CPU) and start rendering both music loops on two background threads. No device or stream error: returns a silent `Audio` (every call is a no-op). Not `Send` (holds the stream): keep it on the main thread. |
| `Audio::silent() -> Audio` | Explicit no-op instance (tests, screenshots, headless). |
| `is_live() -> bool` | A device is open. |
| `music_ready(MusicTrack) -> bool` | That loop has finished rendering (poll from the splash if you want music exactly on the first beat). |
| `play(sfx: Sfx, gain: f32, pitch: f32)` | One-shot. `gain` multiplies the effect's built-in level (1.0 = designed; clamped 0..4). `pitch` is a playback-rate ratio (1.0 = designed, 2.0 = octave up and twice as fast; clamped 0.25..4). Up to 32 voices; the oldest is dropped beyond that. |
| `set_loop(id: LoopId, gain: f32, pitch: f32)` | Live parameters of a looped voice. Call every frame; the mixer smooths (~20 ms gain, ~30 ms pitch). `gain` 0 = silent (clamped 0..2). `pitch` = playback-rate ratio (clamped 0.25..4). |
| `stop_loops()` | All loop gains to 0 (e.g. on leaving a race). |
| `set_music(Option<MusicTrack>)` | Start or switch music with a 1.2 s equal-power crossfade; `None` fades out. Setting the current track is a no-op. If the loop is still rendering, music starts when it is ready. |
| `set_volumes(master, music, sfx)` | Each 0..1, smoothed. |
| `pause_all()` / `resume_all()` | Pause: loops and gameplay one-shots go silent and freeze, music ducks by 60 %, UI sounds (`MenuTap/Back/Select`) still play. Resume undoes it. |
| `set_suspended(bool)` / `is_suspended()` | Platform lifecycle mute: immediately silences every output, including music and UI. Independent of pause and volumes; freezes music/loops and discards one-shots until foregrounded. |

`dsp`, `sfx` and `music` are public: `sfx::render_sfx(Sfx) -> Vec<f32>`,
`sfx::render_loop(LoopId) -> Vec<f32>`, `music::render_music(MusicTrack) -> Vec<f32>`
(buffers are peak-normalised below 1.0) and `coin_pitch(chain: u32) -> f32`.

## Sfx (29)

`Jump`, `Land`, `Boost`, `SparkTier1`, `SparkTier2`, `SparkTier3`, `LaserZap` (hit),
`TargetShatter`, `ItemTick`, `ItemGet`, `EggThrow`, `EggHit`, `Cactus`, `ShieldUp`,
`ShieldBlock`, `Lightning`, `MegaLaser`, `Bash` (thump + squawk), `Coin`, `CountdownBeep`,
`CountdownGo`, `Lap`, `FinalLap`, `Finish`, `MenuTap`, `MenuBack`, `MenuSelect`, `Laugh` (a short frog, jockey and ostrich chorus), `Trump` (Laser Trump: a comic raspberry rolling into a rising laser zap, about 1.2 s).
Frog laughter plays once on entering the sprawled pause; nearby racers hear it
with distance attenuation. Snapshot corrections do not retrigger it.
`Sfx::ALL` lists them; `Sfx::name()` gives a file-friendly name.

**Coin chains:** `audio.play(Sfx::Coin, 1.0, coin_pitch(chain))` where `chain` is the number
of coins collected in the current quick run (0 for the first). The ratio climbs a major
pentatonic (0, 2, 4, 7, 9, 12, 14, 16 semitones) and holds at the top. Reset `chain` to 0
after about 1 s without a coin.

## LoopId (4)

`Footsteps`, `Wind`, `Skid`, `LaserHum`. `LoopId::ALL`.

## MusicTrack (2)

`Menu` (laid-back, 90 bpm, D minor, 8 bars = 21.3 s) and `Race` (driving, 150 bpm, A minor,
8 bars = 12.8 s, kick-sidechained bass and stabs). Both are seamless by construction.

## Driving the loops from speed

Compute `s = speed / top_speed` (0..1+) once per frame and call `set_loop` for each voice.
Starting points, to be tuned by ear:

```rust
let s = (speed / top_speed).clamp(0.0, 1.2);
let grounded = !airborne;
audio.set_loop(LoopId::Footsteps, if grounded { (s * 3.0).min(1.0) } else { 0.0 }, 0.6 + 1.5 * s);
//   pitch is also the step rate: 1.0 = 5 steps/s, so 0.6 = a jog (3/s), 2.1 = a sprint (10.5/s).
audio.set_loop(LoopId::Wind, s * s * 0.9, 0.8 + 0.6 * s);
audio.set_loop(LoopId::Skid, if drifting { 0.5 + 0.5 * drift_slip } else { 0.0 }, 0.9 + 0.3 * s);
audio.set_loop(LoopId::LaserHum, if laser_charging { 0.6 + 0.4 * heat } else { 0.0 }, 0.8 + 0.8 * heat);
```

Footsteps: rate and pitch follow speed; silent in the air and when stopped.
Wind: quiet until about 40 % speed (quadratic), then rising. Skid: while the drift button is
held and slipping. LaserHum: while the laser is charging or firing; pitch climbs with heat.
Silence all four with gain 0 (or `stop_loops()`) on the results screen.

## Menus and race flow

- Splash/menus: `set_music(Some(Menu))`; tap `MenuTap`, back `MenuBack`, confirm `MenuSelect`.
- Countdown: `CountdownBeep` x3 then `CountdownGo`; start `set_music(Some(Race))` on Go (or
  at the first beep for a build-up).
- Lap line: `Lap`; the last lap line: `FinalLap`; the finish: `Finish`, then
  `set_music(Some(Menu))` for the results screen.
- Pause menu: `pause_all()`; Resume: `resume_all()`.
- Settings sliders: `set_volumes(master, music, sfx)` (music 0 silences music only).

## Listening and timing

`cargo run -p race-audio --example render_wav --release -- <dir>` writes every effect, each
loop (4 s), 20 s of each music loop, a `coin_chain.wav` and a `footsteps_ramp.wav`.
`--example profile_synth` prints CPU time per sound (Linux only). Build with
`CARGO_TARGET_DIR=~/.cache/twangthang-target`.
