# App shell (`app/`, crate `stampede`)

The platform layer: window, GPU surface, egui, input, lifecycle, Android glue,
the screen router and headless screenshots. Later agents add screens and
gameplay; they should not need to touch `platform/`, `app.rs` or `render/`.

## Module map (`app/src`)

| Module | What |
|---|---|
| `lib.rs` | `run_desktop`, `android_main` (logger tag `stampede`, panic hook, one game loop per process), `version_label()` |
| `cli.rs` | desktop options (`--screen`, `--shot`, `--out`, `--size`, `--ppp`, `--t`, `--insets`, `--join`, `--data-dir`, `--quit-after`) |
| `app.rs` | winit `ApplicationHandler`: surface create/drop/rebuild, lifecycle, input routing, redraw, frame-time log |
| `engine.rs` | `Engine::frame(dt, size_px, native_ppp, raw)`: input, router, egui, toast, fade, wake lock. Same code for window and `--shot` |
| `shot.rs` | headless screenshot mode |
| `render/` | `Gpu`, `EguiPainter`, `capture` (offscreen + PNG), `perf::FrameStats`, `ScenePass`, `encode_frame` |
| `platform/` | `lifecycle`, `viewport` (UI scale, `Insets`, `Viewport`), `touch` (`Touches`, `Keys`), `tilt` (motion steering), `os` (`Platform` trait), `data_dir` (`Profile`), `android` (JNI) |
| `haptics/` | `Haptics`, `Pulse`; Android vibrator over JNI |
| `ui/theme.rs` | palette (SPEC 13), fonts, `install` |
| `ui/screens/` | `Screen`, `Transition`, `Router`, `Ctx`, registry `SCREENS`; `splash`, `home` (placeholders) |

Frame: `engine.frame` -> `render::encode_frame` = clear to `Screen::clear_color`,
`Screen::paint_scene` (your 3D hook), then egui on top.

Screen-off, suspension and focus loss mute the shared audio immediately from
`Engine::on_background`, independently of screen hooks and further frames.
Foreground focus restores audio at the saved volumes; the race's pause state
still applies. Surface recreation alone does not unmute an unfocused window.

## The Screen trait (`ui/screens/mod.rs`)

```rust
pub trait Screen {
    fn name(&self) -> &'static str;                                   // required; key for --shot/--screen/PopTo
    fn update(&mut self, ctx: &mut Ctx) -> Transition;                // logic, once per frame
    fn draw(&mut self, ctx: &mut Ctx, ui: &egui::Context) -> Transition; // egui UI
    fn back(&mut self, ctx: &mut Ctx) -> Transition;                  // system Back / Esc; default Pop
    fn clear_color(&self, ctx: &Ctx) -> [f32; 4];                     // sRGB rgba; default theme::BG
    fn paint_scene(&mut self, pass: &mut ScenePass);                  // 3D renderer hook (device, queue, encoder, view, size_px, format)
    fn orientation(&self) -> platform::os::Orientation;              // Free by default; Landscape for driving and designer
    fn keep_awake(&self) -> bool;                                     // wake lock while true (a race running)
    fn on_background(&mut self, ctx: &mut Ctx);                       // app backgrounded / unfocused
}
pub enum Transition { None, Push(Box<dyn Screen>), Replace(Box<dyn Screen>), Cut(Box<dyn Screen>), Pop, PopTo(&'static str), Quit }
```

Push/Replace/Pop/PopTo fade through the background colour (0.14 s out, 0.22 s
in); input is held back (`ctx.touches` empty) during the fade-out. `Cut` swaps
at once. `Pop` on the first screen quits: on Android that is `moveTaskToBack`
(the loop keeps running), on desktop the window closes.

`Ctx` (built by the engine each call): `dt`, `time`, `viewport`
(`screen`, `safe`, `insets`, `ppp` in UI points), `touches`, `keys`, `haptics`,
`platform`, `profile`, `data_dir`, `join_code`, `muted`, `opts`, plus
`ctx.say("toast")`, `ctx.take_join_code()`, `ctx.save_profile()`.

### Add a screen

1. New file `ui/screens/garage.rs` with `pub struct GarageScreen` implementing `Screen`.
2. `pub mod garage;` in `ui/screens/mod.rs` and one line in `SCREENS`:
   `("garage", || Box::new(garage::GarageScreen::new()))`.
3. Open it from another screen: `Transition::Push(Box::new(GarageScreen::new()))`.
4. Check it without a display: `cargo run -p stampede -- --shot garage --out /tmp/g.png`.

Lay out inside `ctx.viewport.safe` (the cutout is on the left/right in
landscape). Design for a short side of 360-412 points.

## UI scale

The UI uses a short side of 360-412 points in either orientation
(`viewport::ui_ppp`). On phones that is the native scale (Pixel 8: 914x411 pt
at 2.625; OnePlus 8T: 800x360 at 3.0). A desktop window or tablet is scaled so
its short side is 412 points (1600x720 window -> 916x412 pt, ppp 1.748). All
positions in `Ctx` are UI points. egui gets the same ppp and screen rect; egui's
pointer is fed by the engine, not egui-winit, so hit-testing matches.

Menus follow the phone's rotation setting (`FULL_USER` on Android). Racing,
countdowns, training runs, Run a Track, Drive & place and the designer request
`SENSOR_LANDSCAPE`. The engine changes the request only when the top screen's
policy changes, including after a fade or background session transition.
The manifest starts in `fullUser`; a surface resize clears held touches and
asks for fresh cutout insets immediately.

Menu layouts use `screens::common::Layout::new(safe).portrait` and `split`
to put columns below each other. `common::scroll` provides a clipped body
for tall painted pages. The garage and podium widen and shift their camera
view to leave room for portrait panels.

## Raw touch API (`ctx.touches: &Touches`)

egui sees one pointer; the race needs every thumb. Each finger:

```rust
Touch { id: u64, pos, start: Pos2, start_time: f64, vel: Vec2 /* pt/s, smoothed */, order: u64 }
// .travel(), .delta(), .age(now), .is_mouse()
touches.active() -> &[Touch]      // down now, oldest first
touches.began()   -> &[Touch]     // went down since last frame (a tap lands in began AND ended)
touches.ended()   -> &[Touch]     // lifted/cancelled since last frame, at last position
touches.get(id), touches.count(), touches.in_rect(rect)
```

Desktop: left mouse = touch `MOUSE_LEFT`, right mouse = a second touch
`MOUSE_RIGHT` (press both to try two thumbs). Ids are stable for a finger's
life; lifting one never moves another. `ctx.keys`: `held(KeyCode)`,
`pressed`, `released`, `any_held(&[..])` (physical keys, so WASD is layout
independent). Esc and Android Back call `Screen::back` (not in `keys`).
Raw touches are not gated by egui: if a screen has buttons and a thumb pad,
test `ctx.touches` against your own rects.

After finishing online, the race screen follows an unfinished racer while the
host gathers results. The on-screen previous/next buttons and desktop arrow
keys choose the racer; a finished target is replaced by another still racing.
Driving controls and tilt sensing stop while following, but Pause and Camera
remain available. The original local seat and result settlement do not change.

## Haptics

`ctx.haptics.pulse(Pulse::Bash)`; kinds `Tap, Coin, Landing, Boost, Hit, Bash`
(length + amplitude each, `Pulse::millis/amplitude`). No-op on desktop and
when `profile.haptics` is off (set via `set_enabled`; the engine does not sync
it, call `ctx.haptics.set_enabled(ctx.profile.haptics)` after Settings changes).

## Wake lock, multicast lock, clipboard, links

- Wake lock: return `true` from `Screen::keep_awake` while a race runs; the
  engine toggles `FLAG_KEEP_SCREEN_ON` (via the UI thread, never
  `AndroidApp::set_window_flags`, which deadlocks) and drops it on background.
- Multicast lock (Nearby play): `ctx.platform.set_multicast_lock(true/false)`.
- Clipboard: `ctx.platform.copy_text(code)`, `ctx.platform.read_clipboard()`
  (desktop needs xclip, wl-copy or xsel).
- Join links: `stampede://join#CODE` (adb: `am start -a android.intent.action.VIEW
  -d "stampede://join#KWTQPM" org.decented.stampede`; desktop: `--join KWTQPM`)
  arrive as `ctx.take_join_code()` -> `Some("KWTQPM")` (upper-cased). Whichever
  screen is on top may take it; Home logs and toasts it for now.
- `ctx.muted` is true while backgrounded or unfocused (race-audio should read it).

## Data dir and profile

Android: the activity's internal data path. Desktop: `--data-dir`, else
`$XDG_DATA_HOME/stampede` or `~/.local/share/stampede`. One file,
`profile.json` (`Profile`: name, music/effects volume, haptics,
auto_accelerate (legacy; races are full throttle), steer_mode, tilt_sensitivity, quality, left_handed; every field defaulted, so
old files load; corrupt file -> `profile.json.bad` + defaults). Mutate
`ctx.profile`; it is saved when the app is backgrounded and on exit
(`ctx.save_profile()` forces it). Put new settings in `Profile`.

## Shot mode

```
cargo run -p stampede -- --shot home --out docs/local/shots/x/home.png
    [--size 2400x1080] [--ppp 2.625] [--t 1.5] [--insets 48,0,0,0] [--join CODE]
```

No window: the screen runs on a virtual 60 Hz clock for `--t` seconds (default
1) with no input, the last frame is rendered offscreen (Vulkan or GL, lavapipe
is fine) and written as PNG. `--size` is output pixels (default 1600x720);
`--size 2400x1080 --ppp 2.625` reproduces a phone. Unknown names list the
registered screens. A screen that needs fingers/keys for its state needs a
constructor that stages it (add a registry entry with that state).

## Frame-time log

Every 5 s one line (logcat tag `stampede`, desktop stderr):
`frames: 301 in 5.0s = 60.1 fps | interval avg 16.63 ms p95 18.86 ms | cpu avg 16.19 ms p95 18.42 ms`.
`interval` = time between presented frames; `cpu` = start of redraw to present
returned (includes waiting in Fifo `present`, so it tracks the interval).
`STAMPEDE_NO_VSYNC=1` selects Immediate on desktop: on the dev laptop (Intel
Xe, X11) Fifo surface acquire blocks ~1 s per frame (Twang Thang does the
same), so use it for desktop runs.

## Lifecycle notes (ported, do not undo)

- Surface dropped in `suspended`, rebuilt in `resumed`; the GPU device and
  egui's textures (`TextureMirror`) survive. `Lost`/`Outdated` reconfigure.
- Unfocused on Android: no drawing, `ControlFlow::Wait`.
- One `EventLoop` per process: the loop ends when the activity is destroyed
  and the process exits with it (`platform/lifecycle.rs`).
- egui's clock is the engine clock (`raw.time`), not egui-winit's.
- Insets are re-read every second (a 180-degree flip moves the cutout).

## Battle, ghosts and the ten challenges (app side)

- Race menu has four cards; Battle opens `battle_setup` (arena from `TrackId::ARENAS`, bots 1-7, level). Launch: `common::battle_launch`, `RaceKind::Battle`, results `battle_results` (feathers, hits, lost, out order). Online: the host's lobby has Mode (Race / Battle) and Arena; guests see it; `OnlineResultsScreen` shows feathers and hits from `view.world.battle`.
- Battle HUD (`game/battle_hud.rs`, also drawn in Last Feather) reads `RaceView::battle`; out racers fade and invulnerable ones blink (`game/battle_fx.rs`, one call in `Presenter::poses`). Knocked out offline: the camera follows the leader and the sim runs 4x until the battle ends.
- Item button (`game/hud.rs`, `Frame::item_active` from `Racer::active_item`): while a timed item the racer fired runs, a radial timer (dark wedge growing clockwise from 12 o'clock, `icons::radial_timer`, plus a bright rim arc for the time left) sits over the button. The held item (the next egg's) shows dimmed under it and does nothing when pressed; with nothing held, the running item's icon shows. `playful_hud` no longer prints seconds for these items (only "INVINCIBLE", "CAKE FOR EVERYONE", "EGGS SCRAMBLED" labels remain, for racers the effect reaches).
- Time Trial records every run (`RaceSource::record_ghost`) and keeps the best per track in `<data_dir>/ghosts/<Track>.bin` (3 laps) or `<Track>_<n>lap.bin`; written through a temp file, replaced only by a faster run. `Profile::show_ghost` (Time Trial setup, "Ghost On/Off") turns the racing of it off.
- `Profile::challenge_medals` / `challenge_best` are `[_; 10]` (`CHALLENGES`); shorter lists in an old `profile.json` are padded.

## Progression: sats wallet, Garage variants, achievements

- `ui/progress.rs` is the logic (pure, tested), `ui/progress_ui.rs` the drawing (wallet pill, `Payout` count-up + unlock toasts, `AchievementsScreen`). Data: `Profile::progress` (`platform/data_dir.rs::Progress`, all defaulted, so old profiles load; the wallet is a `u64`).
- Earning (`Earn::of_outcome`): coins collected + place bonus 1st 50 / 2nd 30 / 3rd 20 / other 10 (a solo Time Trial pays 10); Battle pays the place bonus only; a lesson pays 20 per NEW star; a challenge pays 30 / 60 / 120 for each NEW medal tier (first bronze, silver, gold). Online races pay the same from `ResultsView`. Quitting pays nothing. Each result screen settles once in its first `update` (`Report::settle`), then shows `Payout::draw_tally` and `draw_toasts`.
- Race counters come from `RaceStats` (`flow.rs`), filled by `RaceScreen` from the sim events (`absorb`) and put in `RaceOutcome::stats`; online and Training hand them over with `progress::stash_stats` / `take_stats`. "No wall hit" only counts offline (online events may be thinner) and means no `WallHit` event (strength over 0.2).
- Grand Prix cup result: the final round's win (`gp_wins`) is decided and saved when the race ends (`RaceScreen::record_cup` -> `after_race::record_final_round`; the points and ranks come from `after_race::gp_rows`, shared with the standings). The outcome carries `RaceOutcome::gp_recorded` (`flow::GpRecorded { won }`), so `GpScreen` neither counts the win again nor drops the cup from the payout. The same step sets the cup bit (`cups_won`, idempotent), so a game killed in the wait keeps `gp_wins` and the cup; the achievement, prize and coin payout still come from `Report::settle` on the standings. A local racer who has crossed the line is never treated as having quit (`RaceScreen::finish`; online, Quit still leaves the room), and the pause menu drops RESTART once a Grand Prix racer has finished. Local only; mid-cup rounds record nothing.
- Variants: 0 original, 1 (1,000 sats) and 2 (2,500 sats) per ostrich, 3 = Laser Gold (free once every challenge is gold). `progress::buy` checks and deducts in one step; the Garage saves the profile straight after. The worn variant is applied by `game::present::apply_variant` to the local seat only (`Presenter::look`) and in the Garage stage (`stage::show_garage_look`). It is NOT sent online: race-session carries only the character id, so other players see the default colours (local-only for now).
- Screens: `achievements` (trophy button under the name on the main menu), shots `achievements_sample`, `garage_locked_sample`, `garage_buy_confirm_sample`, `results_sats_sample`, `menu_wallet_sample`.

## Open-ground training drills

- `TrainingScreen` offers five Paddock drills and short lessons on the Oval or Figure Eight. Quick Race and Time Trial also offer the two training circuits.
- `RaceKind::Drill(DrillId)` keeps results inside `RaceScreen`: Retry starts a fresh seeded course, Next opens the next drill, and Back returns to Training. Earned stars and the first Master payout settle once, including when leaving a run.
- `Profile::drill_best`, `drill_stars` and `drill_master` follow `DrillId::ALL`; old profiles default to empty progress. Drill payouts are 15 sats per new star and 50 for the first Master medal.
- `RaceSource::drill()` exposes optional drill state. `game::drill_props::instances` maps its current, next and fallen props into renderer instances using the sim's physical dimensions; Power Eggs continue through the target renderer.
- Silent headless captures include `training_menu`, `drill_slalom`, `drill_ring`, `drill_hurdles` and `drill_over` (use `--t 4` for the results card).

## Custom track screens

- `DesignerScreen::new()` opens a blank plan; `edit(CustomTrack)` keeps the
  saved geometry, settings and features; `from_run(CustomTrack)` opens the
  2.5-second build lobby. The plan uses metres mapped to screen `(-x, -z)`,
  one-finger drawing and two-finger pan/zoom. Start moves or reverses the line;
  feature anchors follow the new start. Placement and every launch/save use
  race-core's custom-track checks. Undo retains the full draft.
- `MyTracksScreen::new()` opens the saved list and track tools;
  `picker(trial, bots, BotLevel)` retains the Quick Race / Time Trial setup
  choices. The list reloads on return from drawing, recording or editing;
  tracks retain their own laps. Removal has a confirmation and keeps bests.
- `custom_tracks::{list, save, replace, load, remove}` use `<data_dir>/tracks/` with
  hash-named JSON files (50 tracks). Edited saves replace the old slot after
  writing the new track; a failed write keeps the original. `common::custom_launch` validates and
  activates the chosen track before building the race. Custom bests use the
  track hash in `Profile.custom_bests`.
- Shots: `designer`, `designer_sample`, `designer_hills`, `designer_error`, `track_build`
  (use `--t 3` for the finished lobby), `my_tracks_sample`, `my_tracks_empty`.


### My Signet account

`Services::account` owns the shared `decented-signet` instance and asynchronous
sign-in, online approval and sign-out handles. The engine pumps it both during
frames and background ticks, reconnects on resume, and runs a silent room
service while an approval is waiting. Unsigned accounts are sent to `signin`
before screen input, including Back during an existing fade. Headless shots
and unit harnesses stage screens without persisted accounts; real network
entry points always reload and check approval independently.

`AccountScreen` stages required login, online approval or Settings account
controls. Deferred join/Nearby screens wait behind approval; designer Invite
retains the current draft below it. `identity_for_online` supplies the same
runtime key to QUIC with its verified signed attestation. `QuicHost` checks
peer admission and expiry; `QuicClient` verifies its host before reporting a
connection. Local loopbacks need no network identity.

Designer `Move` drags a short section using the gesture's original line as a
baseline, reshapes it, preserves feature positions along the lap and commits
one Undo entry. Invalid geometry or feature placement restores the baseline;
starting a two-finger gesture does the same. Existing tracks open in Move.

## Tilt steering

`Platform::set_tilt_enabled(bool)` controls sensor acquisition;
`tilt_reading()` returns unavailable, waiting, or a timestamp-aged gravity sample
with display rotation and registration generation. Android `TiltInput.java`
prefers the gravity sensor, falling back to a filtered accelerometer at about
60 Hz, and unregisters on pause. No new permission is needed at this rate.
Axis mapping follows [Android sensor coordinates](https://developer.android.com/develop/sensors-and-location/sensors/sensors_overview#sensors-coords).

The engine owns `tilt::Steering`, exposed through `Ctx::tilt`, and enables it
only for foreground `Screen::uses_tilt()` screens when the saved mode is Tilt.
It centres a still 250 ms grip, rejects samples older than 250 ms and invalid
magnitudes, applies a 2° dead zone and 35 ms smoothing, and recentres on display
rotation / sensor generation changes. `tilt_sensitivity` defaults to 1.0 and is
clamped to 0.5–2.0. Tilt is not mirrored by the left-handed button layout.

`Controller::set_tilt(Option<f32>)` feeds the normal race input and prediction
path. Keys and buttons take priority, then the touch pad, then tilt. The Centre
HUD button is a UI action; it cannot use an item or trigger a countdown rev.
The centre and sensitivity controls apply live. Tilt calibration is per sensor
session; settings and progress from older profiles remain readable.
