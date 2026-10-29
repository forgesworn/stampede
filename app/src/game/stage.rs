//! The shared 3D stage: one `race_render::Renderer` for the whole app, so the
//! menus can show a live 3D backdrop without owning a renderer, and the race
//! screen draws through the same one.
//!
//! Menus call, from `Screen::update`, one of [`show_attract`], [`show_garage`],
//! [`show_podium`], then from `Screen::paint_scene` call [`paint`]. The stage
//! simulation follows the engine clock (`ScenePass::time`), not the `dt` the
//! caller passes. [`set_quality`] maps the Settings choice onto the renderer.

use super::geometry::{self, PODIUM_H};
use super::present::{character_pose, Presenter};
use super::source::{LocalRace, RaceSource};
use crate::platform::data_dir::Quality;
use crate::render::ScenePass;
use glam::Vec3;
use race_core::math::{forward, yaw_of};
use race_core::{BotLevel, Input, RaceConfig, RaceMode, TrackId};
use race_render::{Burst, BurstKind, Camera, FrameScene, OstrichPose, Renderer, WorldGeometry};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::Arc;

/// What the loaded world was built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorldKey {
    Track(TrackId, geometry::Course),
    Podium(TrackId),
    Studio,
}

struct Gfx {
    renderer: Renderer,
    format: wgpu::TextureFormat,
    device_id: u64,
    size: (u32, u32),
    world: Option<WorldKey>,
    custom_generation: u64,
    quality: race_render::Quality,
}

thread_local! {
    // ManuallyDrop: wgpu resources must not be dropped from a thread-local
    // destructor after the device is gone (it panics on exit); the OS reclaims them.
    static GFX: RefCell<Option<ManuallyDrop<Gfx>>> = const { RefCell::new(None) };
    static QUALITY: Cell<race_render::Quality> = const { Cell::new(race_render::Quality::Balanced) };
    /// Scenes drawn before the renderer exists (headless shots only render the
    /// last frame): replayed through the particle system so dust is already there.
    static HISTORY: RefCell<VecDeque<FrameScene>> = const { RefCell::new(VecDeque::new()) };
    static STAGE: RefCell<StageState> = RefCell::new(StageState::default());
}

const HISTORY_FRAMES: usize = 150;

pub fn render_quality(q: Quality) -> race_render::Quality {
    match q {
        Quality::Low => race_render::Quality::Battery,
        Quality::Balanced => race_render::Quality::Balanced,
        Quality::High => race_render::Quality::Max,
    }
}

/// Graphics quality for everything the stage draws (Settings: Low/Balanced/High).
pub fn set_quality(q: Quality) {
    let rq = render_quality(q);
    QUALITY.with(|c| {
        if c.get() != rq {
            // The player chose: forget any automatic step-down.
            GOV.with(|g| g.borrow_mut().reset(rq));
        }
        c.set(rq)
    });
}

/// Forget any automatic step-down (a new race starts from the chosen quality).
pub fn reset_governor() {
    GOV.with(|g| g.borrow_mut().reset(race_render::Quality::Balanced));
}

fn level(q: race_render::Quality) -> u8 {
    match q {
        race_render::Quality::Battery => 0,
        race_render::Quality::Balanced => 1,
        race_render::Quality::Max => 2,
    }
}

fn from_level(l: u8) -> race_render::Quality {
    match l {
        0 => race_render::Quality::Battery,
        1 => race_render::Quality::Balanced,
        _ => race_render::Quality::Max,
    }
}

/// Frame-time governor: if the GPU cannot hold ~60 fps at the chosen quality
/// (the OnePlus 8T's Adreno 650 at Balanced/High in a crowded scene) it steps
/// the renderer down one level, once per measuring window. The choice in
/// Settings is untouched; changing it resets the governor.
struct Gov {
    cap: u8,
    acc: f32,
    n: u32,
    skip: u32,
    slow_windows: u8,
    last_t: Option<f32>,
}

impl Gov {
    const fn new() -> Gov {
        Gov { cap: 2, acc: 0.0, n: 0, skip: 120, slow_windows: 0, last_t: None }
    }
    fn reset(&mut self, _q: race_render::Quality) {
        *self = Gov::new();
    }
    /// Feed one painted frame at engine time `t`; returns the new cap if it dropped.
    fn frame(&mut self, t: f32, wanted: u8) -> Option<u8> {
        let dt = self.last_t.map(|l| t - l).unwrap_or(0.0);
        self.last_t = Some(t);
        if !(0.001..0.1).contains(&dt) {
            return None;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return None;
        }
        self.acc += dt;
        self.n += 1;
        if self.n >= 90 {
            let avg = self.acc / self.n as f32;
            self.acc = 0.0;
            self.n = 0;
            // Two slow windows in a row (3 s): one hitch must not cost the player quality.
            if avg > 0.0192 {
                self.slow_windows += 1;
            } else {
                self.slow_windows = 0;
            }
            if self.slow_windows >= 2 && wanted.min(self.cap) > 0 {
                self.cap = wanted.min(self.cap) - 1;
                self.skip = 120;
                self.slow_windows = 0;
                return Some(self.cap);
            }
        }
        None
    }
}

thread_local! {
    static GOV: RefCell<Gov> = const { RefCell::new(Gov::new()) };
}

// -------------------------------------------------------------- DIAG knobs ----
// Device measurement (DIAG builds only: `tools/build-apk.sh` with DIAG=1 sets
// STAMPEDE_DIAG). `<data dir>/diag.txt` holds words like `screen=race_dune tier=1
// gov=0 scale=0.8 msaa=1 dust=0 repeat=3` (see `race_render::Diag`); it is re-read
// every second, so passes can be toggled live with `adb shell run-as ... sh -c 'echo ... > files/diag.txt'`.
static DIAG_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn diag_build() -> bool {
    option_env!("STAMPEDE_DIAG") == Some("1")
}

/// Remember the data dir for the DIAG knob file (a no-op outside DIAG builds).
pub fn diag_init(dir: &std::path::Path) {
    if diag_build() {
        let _ = DIAG_DIR.set(dir.to_path_buf());
    }
}

/// The text of `diag.txt` (DIAG builds), else empty.
pub fn diag_text() -> String {
    DIAG_DIR.get().and_then(|d| std::fs::read_to_string(d.join("diag.txt")).ok()).unwrap_or_default()
}

/// The `screen=NAME` knob, for starting straight in a race.
pub fn diag_screen() -> Option<String> {
    diag_text().split_whitespace().find_map(|w| w.strip_prefix("screen=").map(String::from))
}

#[derive(Default)]
struct DiagState {
    last_poll: Option<std::time::Instant>,
    text: String,
    tier: Option<u8>,
    gov_off: bool,
}

thread_local! {
    static DIAG: RefCell<DiagState> = RefCell::new(DiagState::default());
}

/// Re-read the knob file once a second; returns (renderer knobs, tier override, governor off).
fn diag_poll() -> Option<(race_render::Diag, Option<u8>, bool)> {
    if !diag_build() {
        return None;
    }
    DIAG.with(|d| {
        let mut d = d.borrow_mut();
        let due = d.last_poll.is_none_or(|t| t.elapsed().as_secs_f32() > 1.0);
        if due {
            d.last_poll = Some(std::time::Instant::now());
            let text = diag_text();
            if text != d.text {
                log::info!("stage: diag knobs: {}", text.trim());
                d.tier = text.split_whitespace().find_map(|w| w.strip_prefix("tier=")?.parse().ok());
                d.gov_off = text.split_whitespace().any(|w| w == "gov=0");
                d.text = text;
            }
        }
        Some((race_render::Diag::parse(&d.text), d.tier, d.gov_off))
    })
}

/// True when no renderer exists yet (so a screen may record frames for replay).
pub fn needs_history() -> bool {
    GFX.with(|g| g.borrow().is_none())
}

/// Record a frame for replay once the renderer exists (see [`needs_history`]).
pub fn push_history(scene: &FrameScene) {
    HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        if h.len() >= HISTORY_FRAMES {
            h.pop_front();
        }
        h.push_back(scene.clone());
    });
}

/// Renderer stats of the last frame, if any.
pub fn stats() -> Option<race_render::RenderStats> {
    GFX.with(|g| g.borrow().as_ref().map(|g| g.renderer.stats()))
}

/// Render `scene` into the pass with the shared renderer, loading the world for
/// `key` first if it is not the one loaded. `geom` builds it on demand.
pub fn draw(pass: &mut ScenePass, key: WorldKey, geom: impl FnOnce() -> Arc<WorldGeometry>, scene: &FrameScene, dt: f32) {
    GFX.with(|cell| {
        let mut slot = cell.borrow_mut();
        let diag = diag_poll();
        let wanted = match diag.and_then(|d| d.1) {
            Some(t) => from_level(t),
            None => QUALITY.with(|c| c.get()),
        };
        let gov_off = diag.is_some_and(|d| d.2);
        let quality = GOV.with(|g| {
            let mut g = g.borrow_mut();
            if gov_off {
                return wanted;
            }
            if let Some(cap) = g.frame(pass.time, level(wanted)) {
                log::warn!("stage: frame time over budget at {:?}, stepping the renderer down to {:?}", wanted, from_level(cap));
            }
            from_level(level(wanted).min(g.cap))
        });
        if slot.as_ref().is_some_and(|g| g.format != pass.format || g.device_id != pass.device_id) {
            log::info!("stage: output format or GPU device changed, rebuilding the renderer");
            if let Some(old) = slot.take() {
                drop(ManuallyDrop::into_inner(old));
            }
        }
        let g = slot.get_or_insert_with(|| {
            let t0 = std::time::Instant::now();
            let renderer = Renderer::new(pass.device, pass.queue, pass.format, quality);
            log::info!("stage: renderer created in {:.1} ms ({:?}, {:?})", t0.elapsed().as_secs_f32() * 1000.0, pass.format, quality);
            ManuallyDrop::new(Gfx { renderer, format: pass.format, device_id: pass.device_id, size: (0, 0), world: None, custom_generation: 0, quality })
        });
        if g.quality != quality {
            g.renderer.set_quality(quality);
            g.quality = quality;
        }
        if let Some((d, _, _)) = diag {
            g.renderer.set_diag(d);
        }
        if g.size != pass.size_px {
            g.renderer.resize(pass.size_px.0, pass.size_px.1);
            g.size = pass.size_px;
        }
        let generation = match key {
            WorldKey::Track(TrackId::Custom, _) | WorldKey::Podium(TrackId::Custom) => race_core::custom::generation(),
            _ => 0,
        };
        if g.world != Some(key) || g.custom_generation != generation {
            let t0 = std::time::Instant::now();
            let w = geom();
            g.renderer.load_world(pass.device, pass.queue, &w);
            g.world = Some(key);
            g.custom_generation = generation;
            log::info!("stage: world {:?} loaded in {:.1} ms", key, t0.elapsed().as_secs_f32() * 1000.0);
            // Replay what was recorded before the renderer existed.
            let frames: Vec<FrameScene> = HISTORY.with(|h| h.borrow_mut().drain(..).collect());
            for f in &frames {
                g.renderer.step_particles(f, 1.0 / 60.0);
            }
        }
        g.renderer.render(pass.device, pass.queue, pass.encoder, pass.view, scene, dt);
    });
}

// ------------------------------------------------------------ the stage ----

#[derive(Clone, Copy, Debug)]
enum Mode {
    Attract(TrackId),
    Garage { character: u8, attire: [u8; 3], yaw: f32 },
    Podium { top3: [Option<u8>; 3], track: TrackId },
}

#[derive(Default)]
struct StageState {
    request: Option<Mode>,
    drawing: bool,
    last_time: Option<f32>,
    ff: f32,
    /// Where the subject should sit across the screen (0..1), for the next paint only.
    focus: Option<f32>,
    portrait_focus: Option<(f32, f32)>,
    clock: f32,
    attract: Option<Attract>,
    last_burst: i64,
}

struct Attract {
    track: TrackId,
    race: LocalRace,
    pres: Presenter,
    cut: u32,
    cut_t: f32,
}

/// Title screen: an all-bot attract race on `track` with a slow orbiting camera.
pub fn show_attract(track: TrackId) {
    STAGE.with(|s| s.borrow_mut().request = Some(Mode::Attract(track)));
}

/// Garage: one ostrich (`character` id) on a turntable at `yaw` radians,
/// in a neutral studio set.
pub fn show_garage(character: u8, yaw: f32) {
    show_garage_look(character, 0, yaw);
}

/// [`show_garage`] in colour variant `variant` (0 = the original colours).
pub fn show_garage_look(character: u8, variant: u8, yaw: f32) {
    show_garage_attire(character, [variant, 0, 0], yaw);
}

/// Turntable with the selected ostrich and jockey clothing.
pub fn show_garage_attire(character: u8, attire: [u8; 3], yaw: f32) {
    STAGE.with(|s| s.borrow_mut().request = Some(Mode::Garage { character, attire, yaw }));
}

/// Results: the top three ostriches (character ids, 1st first) on a podium,
/// celebrating, on `track`'s scenery.
/// `top3[k]` is the character on step k (1st, 2nd, 3rd), or `None` when fewer than three raced.
pub fn show_podium(top3: [Option<u8>; 3], track: TrackId) {
    STAGE.with(|s| s.borrow_mut().request = Some(Mode::Podium { top3, track }));
}

/// Put the 3D subject at `x_frac` of the screen width (0 = left edge, 0.5 = centre, the
/// default) for the next `paint`, so it sits in the free area beside the menu panels.
/// Call it from `Screen::update` next to `show_*`; it is forgotten after each paint.
pub fn set_focus(x_frac: f32) {
    STAGE.with(|s| s.borrow_mut().focus = Some(x_frac.clamp(0.05, 0.95)));
}

/// Centre a subject in a portrait panel and widen the view to leave room for the UI.
pub fn set_portrait_focus(y_frac: f32, zoom_out: f32) {
    STAGE.with(|s| s.borrow_mut().portrait_focus = Some((y_frac, zoom_out)));
}

fn frame_portrait(cam: &mut Camera, framing: Option<(f32, f32)>) {
    let Some((y, zoom_out)) = framing else { return };
    // Keep the eye in front of the podium: moving it back can put track scenery between us and the racers.
    cam.fov_y = 2.0 * ((cam.fov_y * 0.5).tan() * zoom_out).atan();
    let dir = cam.target - cam.eye;
    let len = dir.length();
    let forward = dir / len;
    let up = forward.cross(cam.up).normalize().cross(forward);
    let alpha = ((1.0 - 2.0 * y) * (cam.fov_y * 0.5).tan()).atan();
    cam.target = cam.eye + (forward * alpha.cos() - up * alpha.sin()) * len;
}

/// Turn the camera about its eye so what it looks at lands at `x_frac` across a
/// `size_px` frame (a lens shift without touching the projection).
pub fn shift_focus(cam: &mut Camera, x_frac: f32, size_px: (u32, u32)) {
    let ndc = (x_frac - 0.5) * 2.0;
    if ndc.abs() < 1e-3 || size_px.1 == 0 {
        return;
    }
    let aspect = size_px.0 as f32 / size_px.1 as f32;
    let alpha = (ndc * (cam.fov_y * 0.5).tan() * aspect).atan();
    let dir = cam.target - cam.eye;
    let len = dir.length();
    if len < 1e-3 {
        return;
    }
    let dir = dir / len;
    let right = dir.cross(cam.up).normalize_or_zero();
    // Look to the left of the subject (for ndc > 0) so it appears on the right.
    let nd = (dir * alpha.cos() - right * alpha.sin()).normalize();
    cam.target = cam.eye + nd * len;
}

/// Create the shared renderer (and load `track`'s world) ahead of the first race, from a
/// splash frame, so the countdown does not stutter on pipeline compilation. Draws nothing.
pub fn warm_renderer(pass: &mut ScenePass, track: Option<TrackId>) {
    GFX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().is_some_and(|g| g.format != pass.format || g.device_id != pass.device_id) {
            if let Some(old) = slot.take() {
                drop(ManuallyDrop::into_inner(old));
            }
        }
        let quality = QUALITY.with(|c| c.get());
        let g = slot.get_or_insert_with(|| {
            let t0 = std::time::Instant::now();
            let renderer = Renderer::new(pass.device, pass.queue, pass.format, quality);
            log::info!("stage: renderer warmed in {:.1} ms", t0.elapsed().as_secs_f32() * 1000.0);
            ManuallyDrop::new(Gfx { renderer, format: pass.format, device_id: pass.device_id, size: (0, 0), world: None, custom_generation: 0, quality })
        });
        if g.size != pass.size_px {
            g.renderer.resize(pass.size_px.0, pass.size_px.1);
            g.size = pass.size_px;
        }
        if let Some(id) = track {
            if g.world.is_none() {
                let t0 = std::time::Instant::now();
                let course = geometry::Course::of(id, race_core::RaceMode::Race);
                g.renderer.load_world(pass.device, pass.queue, &geometry::cached_course(id, course));
                g.world = Some(WorldKey::Track(id, course));
                log::info!("stage: world {:?} warmed in {:.1} ms", id, t0.elapsed().as_secs_f32() * 1000.0);
            }
        }
    });
}

/// Run the stage clock (and an attract race) ahead by `secs` before the next
/// paint. For headless shots, where only the last frame is ever painted.
pub fn fast_forward(secs: f32) {
    STAGE.with(|s| s.borrow_mut().ff += secs);
}

/// True while the stage draws a 3D backdrop this frame (menus then keep their
/// own background translucent). A mode has to be requested for the frame.
pub fn is_drawing() -> bool {
    STAGE.with(|s| s.borrow().request.is_some())
}

/// Build every track's geometry ahead of time (race-core builds terrain on
/// first use). Call once from the splash; cheap when already warm.
pub fn warm_up() {
    geometry::warm_all();
}

/// Advance and draw the requested stage into the pass. Call from
/// `Screen::paint_scene`. Draws nothing if nothing was requested. (`dt` is
/// ignored: the stage follows `pass.time`.)
pub fn paint(pass: &mut ScenePass, _dt: f32) {
    STAGE.with(|cell| {
        let mut st = cell.borrow_mut();
        let Some(mode) = st.request.take() else {
            st.drawing = false;
            st.last_time = None;
            return;
        };
        st.drawing = true;
        let focus = st.focus.take().unwrap_or(0.5);
        let now = pass.time;
        let dt = match st.last_time {
            Some(t) => (now - t).clamp(0.0, 0.1),
            None => 1.0 / 60.0,
        };
        st.last_time = Some(now);
        let ff = std::mem::take(&mut st.ff);
        st.clock += dt + ff;
        let portrait_focus = st.portrait_focus.take();
        match mode {
            Mode::Attract(track) => paint_attract(&mut st, pass, track, dt, ff, focus),
            Mode::Garage { character, attire, yaw } => paint_garage(&mut st, pass, character, attire, yaw, dt, focus, portrait_focus),
            Mode::Podium { top3, track } => paint_podium(&mut st, pass, top3, track, dt, focus, portrait_focus),
        }
    });
}

fn paint_attract(st: &mut StageState, pass: &mut ScenePass, track: TrackId, dt: f32, ff: f32, focus_x: f32) {
    if st.attract.as_ref().is_none_or(|a| a.track != track) {
        let mut cfg = RaceConfig::all_bots(track, 8, BotLevel::Normal, 11 + track.index() as u64);
        cfg.mode = RaceMode::Attract;
        let mut race = LocalRace::new(cfg, None);
        // Skip the grid countdown and the first seconds of bunched-up start.
        for _ in 0..(6 * 60) {
            race.advance(1.0 / 60.0, &mut Input::default);
        }
        race.drain_events();
        st.attract = Some(Attract { track, race, pres: Presenter::new(), cut: 0, cut_t: 0.0 });
    }
    let a = st.attract.as_mut().expect("just made");
    // Fast-forward in whole frames.
    let mut ff = ff;
    while ff > 0.0 {
        let d = ff.min(1.0 / 30.0);
        a.race.advance(d, &mut Input::default);
        a.race.drain_events();
        a.cut_t += d;
        ff -= d;
    }
    a.race.advance(dt, &mut Input::default);
    let events = a.race.drain_events();
    a.cut_t += dt;
    if a.cut_t > 5.5 {
        a.cut_t = 0.0;
        a.cut += 1;
    }
    let v = a.race.view();
    let poses = a.pres.poses(&v, dt, None);
    let order: Vec<usize> = {
        let mut o: Vec<usize> = (0..v.racers.len()).collect();
        o.sort_by_key(|&i| v.racers[i].place);
        o
    };
    const PICK: [usize; 8] = [0, 3, 1, 5, 2, 6, 4, 7];
    let focus = order[PICK[a.cut as usize % 8] % order.len()];
    let p = poses[focus];
    let tr = v.track;
    let style = a.cut % 4;
    let t = st.clock;
    let fwd = forward(p.yaw);
    let body = p.pos + Vec3::Y * 1.3;
    let (eye, target, fov, fog): (Vec3, Vec3, f32, f32) = match style {
        0 => {
            let ang = t * 0.35;
            (body + Vec3::new(ang.sin() * 14.0, 4.2, ang.cos() * 14.0), body, 56.0, 0.8)
        }
        1 => {
            let r = &v.racers[focus];
            let s = r.s + 38.0;
            let sm = tr.sample_at(s);
            let side = if a.cut % 8 < 4 { 1.0 } else { -1.0 };
            let lat = side * (sm.width * 0.5 + 5.0);
            let mut e = tr.point_at(s, lat) + Vec3::Y * 2.2;
            e.y = e.y.max(tr.terrain_height(e.x, e.z) + 1.5);
            (e, body, 42.0, 0.7)
        }
        2 => (body - fwd * 9.5 + Vec3::Y * 3.4, body + fwd * 3.0, 64.0, 1.0),
        _ => (body + fwd * 10.5 + Vec3::Y * 0.4 + Vec3::new(fwd.z, 0.0, -fwd.x) * 1.4, body, 52.0, 0.9),
    };
    let mut cam = Camera { eye, target, up: Vec3::Y, fov_y: fov.to_radians(), near: 0.3, far: 700.0 };
    shift_focus(&mut cam, focus_x, pass.size_px);
    let scene = a.pres.scene(&v, usize::MAX, poses, &events, cam, Vec3::ZERO, None, fog);
    let id = track;
    let course = geometry::Course::of(id, race_core::RaceMode::Attract);
    draw(pass, WorldKey::Track(id, course), || geometry::cached_course(id, course), &scene, dt);
}

/// Periodic head swings / look-backs / laser flashes so a standing ostrich is alive.
fn idle(p: &mut OstrichPose, t: f32, phase: f32) {
    let c = (t + phase).rem_euclid(7.0);
    let smooth = |x: f32| (x.clamp(0.0, 1.0) * std::f32::consts::PI).sin();
    if (1.5..3.0).contains(&c) {
        p.look_back = smooth((c - 1.5) / 1.5).sqrt();
    }
    if (4.2..4.7).contains(&c) {
        p.bash = smooth((c - 4.2) / 0.5) * if phase > 1.0 { -1.0 } else { 1.0 };
    }
    if (5.6..6.2).contains(&c) {
        p.laser_on = true;
    }
}

fn paint_garage(st: &mut StageState, pass: &mut ScenePass, character: u8, attire: [u8; 3], yaw: f32, dt: f32, focus: f32, portrait_focus: Option<(f32, f32)>) {
    let t = st.clock;
    let mut p = character_pose(character);
    super::present::apply_variant(&mut p, character, attire[0]);
    p.rider = character;
    p.attire = [attire[1], attire[2]];
    p.accessory = match attire[1] { 1 => race_render::Accessory::Scarf,
        2 => race_render::Accessory::Crown, 3 => race_render::Accessory::Goggles, _ => p.accessory };
    p.yaw = yaw;
    p.pos = Vec3::new(0.0, 0.0, 0.0);
    p.speed = 0.0;
    p.pitch = 0.0;
    idle(&mut p, t, 0.0);
    let mut cam = Camera { eye: Vec3::new(0.0, 2.2, 9.2), target: Vec3::new(0.0, 1.6, 0.0), up: Vec3::Y, fov_y: 34f32.to_radians(), near: 0.3, far: 400.0 };
    frame_portrait(&mut cam, portrait_focus);
    shift_focus(&mut cam, focus, pass.size_px);
    let mut scene = FrameScene { camera: cam, time: t, ostriches: vec![p], fog_scale: 0.6, ..Default::default() };
    if p.laser_on {
        for e in race_render::eye_positions(&p) {
            scene.beams.push(race_render::Beam { from: e, to: e + forward(p.yaw) * 14.0 + Vec3::Y * 0.3, width: 0.09, color: [1.0, 0.1, 0.08], intensity: 1.0 });
        }
    }
    draw(pass, WorldKey::Studio, || Arc::new(geometry::studio()), &scene, dt);
}

fn paint_podium(st: &mut StageState, pass: &mut ScenePass, top3: [Option<u8>; 3], track: TrackId, dt: f32, focus: f32, portrait_focus: Option<(f32, f32)>) {
    let t = st.clock;
    let spot = geometry::podium_spot(track);
    let geom = move || Arc::new(geometry::podium(track).0);
    let fwd = forward(spot.yaw);
    let mut poses = Vec::new();
    let offs = [0.0f32, 2.7, -2.7];
    let mut scene = FrameScene { time: t, fog_scale: 0.25, ..Default::default() };
    for k in 0..3 {
        let Some(character) = top3[k] else { continue };
        let mut p = character_pose(character);
        let base = spot.pos + spot.right * offs[k];
        p.pos = Vec3::new(base.x, base.y + PODIUM_H[k], base.z);
        p.yaw = yaw_of(-fwd) + (t * 0.9 + k as f32).sin() * 0.12;
        p.speed = 0.0;
        // Celebrate: 1st hops and fires its eyes, the others swing their heads.
        let hop = ((t * 3.4 + k as f32 * 1.7).sin()).max(0.0);
        let lift = hop * [0.55, 0.28, 0.2][k];
        p.pos.y += lift;
        p.airborne = lift > 0.05;
        p.tuck = if p.airborne { 0.8 } else { 0.0 };
        if k == 0 {
            p.laser_on = ((t * 2.0) as i32) % 3 == 0;
        } else {
            p.bash = (t * 5.0 + k as f32).sin() * 0.9;
        }
        poses.push(p);
    }
    for p in &poses {
        if p.laser_on {
            for e in race_render::eye_positions(p) {
                scene.beams.push(race_render::Beam { from: e, to: e + forward(p.yaw) * 20.0 + Vec3::Y * 6.0, width: 0.1, color: [1.0, 0.1, 0.08], intensity: 1.1 });
            }
        }
    }
    // Confetti: golden sparkles above each step, a burst per beat.
    let beat = (t * 1.6) as i64;
    if beat != st.last_burst {
        st.last_burst = beat;
        for p in &poses {
            scene.bursts.push(Burst { kind: BurstKind::CoinSparkle, pos: p.pos + Vec3::Y * 2.4, count: 10 });
        }
    }
    scene.ostriches = poses;
    let ang = (t * 0.25).sin() * 0.45;
    let centre = spot.pos + Vec3::Y * (PODIUM_H[0] + 0.9);
    // A narrow free strip (focus near an edge) needs the camera further back to fit all three steps.
    let dist = 11.0 * (0.56 / (2.0 * focus.min(1.0 - focus))).max(1.0);
    let eye = spot.pos + fwd * (dist * ang.cos()) + spot.right * (dist * ang.sin()) + Vec3::Y * 3.0;
    scene.camera = Camera { eye, target: centre, up: Vec3::Y, fov_y: 42f32.to_radians(), near: 0.3, far: 700.0 };
    frame_portrait(&mut scene.camera, portrait_focus);
    shift_focus(&mut scene.camera, focus, pass.size_px);
    draw(pass, WorldKey::Podium(track), geom, &scene, dt);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_shift_moves_the_subject_across_the_screen() {
        let base = Camera { eye: Vec3::new(0.0, 2.0, 8.0), target: Vec3::new(0.0, 1.5, 0.0), up: Vec3::Y, fov_y: 34f32.to_radians(), near: 0.3, far: 400.0 };
        // Project the old target through the shifted camera: it must land at x_frac.
        for frac in [0.25f32, 0.5, 0.78] {
            let mut cam = base;
            shift_focus(&mut cam, frac, (2400, 1080));
            let f = (cam.target - cam.eye).normalize();
            let r = f.cross(cam.up).normalize();
            let u = r.cross(f);
            let d = base.target - cam.eye;
            let (x, z) = (d.dot(r), d.dot(f));
            let aspect = 2400.0 / 1080.0;
            let ndc = x / (z * (cam.fov_y * 0.5).tan() * aspect);
            let _ = u;
            assert!(((ndc * 0.5 + 0.5) - frac).abs() < 0.01, "frac {frac}: landed at {}", ndc * 0.5 + 0.5);
        }
    }

    #[test]
    fn portrait_framing_keeps_the_eye_and_projects_the_subject_into_its_panel() {
        let base = Camera { eye: Vec3::new(0.0, 2.0, 8.0), target: Vec3::new(0.0, 1.5, 0.0), up: Vec3::Y, fov_y: 34f32.to_radians(), near: 0.3, far: 400.0 };
        for y in [0.23, 0.30, 0.65] {
            let mut cam = base;
            frame_portrait(&mut cam, Some((y, 2.1)));
            assert_eq!(cam.eye, base.eye);
            let f = (cam.target - cam.eye).normalize();
            let r = f.cross(cam.up).normalize();
            let u = r.cross(f);
            let d = base.target - cam.eye;
            let ndc = d.dot(u) / (d.dot(f) * (cam.fov_y * 0.5).tan());
            assert!((0.5 - ndc * 0.5 - y).abs() < 0.01);
        }
    }

    fn run(g: &mut Gov, dt: f32, frames: u32, wanted: u8) -> Option<u8> {
        let mut t = 100.0;
        let mut out = None;
        for _ in 0..frames {
            t += dt;
            if let Some(c) = g.frame(t, wanted) {
                out = Some(c);
            }
        }
        out
    }

    #[test]
    fn governor_steps_down_only_when_slow() {
        let mut g = Gov::new();
        assert_eq!(run(&mut g, 1.0 / 60.0, 600, 1), None, "60 fps holds the quality");
        let mut g = Gov::new();
        assert_eq!(run(&mut g, 0.022, 420, 1), Some(0), "46 fps at Balanced drops to Battery");
        // At the bottom there is nowhere to go.
        assert_eq!(run(&mut g, 0.030, 600, 1), None);
        // High steps down one level at a time.
        let mut g = Gov::new();
        assert_eq!(run(&mut g, 0.025, 320, 2), Some(1));
    }

    #[test]
    fn hitches_do_not_count() {
        let mut g = Gov::new();
        let mut t = 0.0;
        for i in 0..400 {
            t += if i % 50 == 0 { 0.6 } else { 1.0 / 60.0 };
            assert_eq!(g.frame(t, 1), None);
        }
    }

    #[test]
    fn quality_maps_to_the_renderer() {
        assert_eq!(render_quality(Quality::Low), race_render::Quality::Battery);
        assert_eq!(render_quality(Quality::Balanced), race_render::Quality::Balanced);
        assert_eq!(render_quality(Quality::High), race_render::Quality::Max);
    }
}
