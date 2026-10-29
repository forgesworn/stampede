//! The platform-independent frame: input state, the router, egui and the
//! services screens use. The windowed app (`app.rs`) and the headless
//! screenshot mode (`shot.rs`) both drive an [`Engine`] and then render its
//! output, so a screen behaves the same in both.

use crate::cli::Options;
use crate::haptics::Haptics;
use crate::online::OnlineSlot;
use crate::platform::data_dir::Profile;
use crate::platform::os::Platform;
use crate::platform::viewport::ui_ppp;
use crate::platform::{Insets, Keys, KeyCode, TouchPhase, Touches, Viewport};
use crate::ui::screens::{Applied, Ctx, Router, Screen, Toast};
use crate::ui::theme;
use egui::{pos2, Pos2};
use std::path::PathBuf;

/// Where egui's pointer lets go when a screen change drops a finger (no widget there).
const OFFSCREEN: Pos2 = Pos2::new(-10_000.0, -10_000.0);

/// The services a screen reaches through [`Ctx`]. Separate from the router so
/// both can be borrowed at once.
pub struct Services {
    pub touches: Touches,
    pub keys: Keys,
    pub haptics: Box<dyn Haptics>,
    pub platform: Box<dyn Platform>,
    pub tilt: crate::platform::tilt::Steering,
    pub profile: Profile,
    pub data_dir: PathBuf,
    /// A room or Keep to join, from a launch link, waiting for a screen to take it.
    pub join_code: Option<crate::keeps::JoinTarget>,
    /// The Keep the player agreed to leave a room for (see `Ctx::join_confirmed`).
    pub join_confirmed: Option<[u8; 32]>,
    /// Backgrounded or unfocused: sound off (race-audio reads this via `Ctx::muted`).
    pub muted: bool,
    pub opts: Options,
    pub toast: Option<Toast>,
    /// The app's one online session (see `crate::online`).
    pub online: OnlineSlot,
    /// In-app updates and the install path More games shares (see `crate::update`).
    pub updates: crate::update::Updater,
    pub account: Option<crate::account::Account>,
    /// What screens see while a fade-out holds input back.
    no_touches: Touches,
}

impl Services {
    fn ctx(&mut self, dt: f32, time: f64, viewport: Viewport, input_blocked: bool) -> Ctx<'_> {
        Ctx {
            dt,
            time,
            viewport,
            touches: if input_blocked { &self.no_touches } else { &self.touches },
            keys: &self.keys,
            haptics: self.haptics.as_mut(),
            platform: self.platform.as_mut(),
            tilt: &mut self.tilt,
            profile: &mut self.profile,
            data_dir: &self.data_dir,
            join_code: &mut self.join_code,
            join_confirmed: &mut self.join_confirmed,
            input_blocked,
            muted: self.muted,
            opts: &self.opts,
            toast: &mut self.toast,
            online: &self.online,
            updates: &mut self.updates,
            account: &mut self.account,
        }
    }
}

/// What a frame produced (render it, then present).
pub struct FrameOut {
    pub egui: egui::FullOutput,
    /// The colour to clear the frame to (sRGB 0..1, rgba).
    pub clear: [f32; 4],
    pub viewport: Viewport,
    /// The event loop must end.
    pub quit: bool,
}

pub struct Engine {
    pub router: Router,
    pub svc: Services,
    pub egui: egui::Context,
    /// Safe-area insets in UI points (set by the platform before each frame).
    pub insets: Insets,
    /// Engine time in seconds.
    pub time: f64,
    pub frames: u64,
    /// CPU time of the last frame's logic and UI, milliseconds.
    pub last_ms: f32,
    back_requested: bool,
    keep_on: bool,
    orientation: Option<crate::platform::os::Orientation>,
    backgrounded: bool,
    /// The platform's room service (keeps the process in use) is on.
    room_service: bool,
    /// UI pixels per point of the current surface (see `platform::viewport::ui_ppp`).
    ppp: f32,
    /// egui pointer events since the last frame.
    egui_events: Vec<egui::Event>,
    /// The touch (or mouse button) that drives egui's single pointer.
    egui_primary: Option<u64>,
    cursor: Pos2,
}

impl Engine {
    pub fn new(first: Box<dyn Screen>, profile: Profile, data_dir: PathBuf, opts: Options, platform: Box<dyn Platform>, haptics: Box<dyn Haptics>) -> Engine {
        let egui = egui::Context::default();
        theme::install(&egui);
        let insets = opts.insets;
        // Screenshots keep the state in memory only (their staged offers are throwaway-signed).
        let mut updates = if opts.shot.is_some() { crate::update::Updater::in_memory() } else { crate::update::Updater::open(&data_dir) };
        updates.set_user_relays(profile.relays.clone());
        // Staged screenshots and unit harnesses have no persisted accounts or sockets.
        let account = if opts.shot.is_some() || cfg!(test) { None } else { Some(crate::account::Account::open(&data_dir)) };
        let first = match &account {
            Some(a) if !a.signed_in() => Box::new(crate::ui::screens::account::AccountScreen::login(first)) as Box<dyn Screen>,
            Some(a) if matches!(first.name(), "host" | "join" | "nearby" | "lobby") && !a.online_allowed() => Box::new(crate::ui::screens::account::AccountScreen::online(Some(first))),
            _ => first,
        };
        Engine {
            router: Router::new(first),
            svc: Services {
                touches: Touches::new(),
                keys: Keys::default(),
                haptics,
                platform,
                tilt: Default::default(),
                profile,
                data_dir,
                join_code: None,
                join_confirmed: None,
                muted: false,
                opts,
                toast: None,
                online: OnlineSlot::new(),
                updates,
                account,
                no_touches: Touches::new(),
            },
            egui,
            insets,
            time: 0.0,
            frames: 0,
            last_ms: 0.0,
            back_requested: false,
            keep_on: false,
            orientation: None,
            backgrounded: false,
            room_service: false,
            ppp: 1.0,
            egui_events: Vec::new(),
            egui_primary: None,
            cursor: Pos2::ZERO,
        }
    }

    pub fn screen_name(&self) -> &'static str {
        self.router.top().name()
    }

    // ── surface ─────────────────────────────────────────────────────────────

    /// The surface is `size_px` big at OS scale factor `native_ppp`. Input
    /// positions (physical pixels) are divided by the UI scale this sets.
    pub fn set_surface(&mut self, size_px: (u32, u32), native_ppp: f32) {
        self.ppp = ui_ppp(size_px, native_ppp);
    }

    /// UI pixels per point right now.
    pub fn ppp(&self) -> f32 {
        self.ppp
    }

    // ── input (positions in physical pixels) ────────────────────────────────

    fn to_pt(&self, px: (f64, f64)) -> Pos2 {
        pos2(px.0 as f32 / self.ppp, px.1 as f32 / self.ppp)
    }

    /// A finger: raw to `Ctx::touches`, and the newest one down also drives egui's pointer.
    pub fn touch_event(&mut self, id: u64, phase: TouchPhase, px: (f64, f64)) {
        let pos = self.to_pt(px);
        self.svc.touches.touch(id, phase, pos);
        self.egui_pointer(id, phase, pos);
    }

    /// The mouse cursor moved (desktop).
    pub fn cursor_moved(&mut self, px: (f64, f64)) {
        let pos = self.to_pt(px);
        self.cursor = pos;
        self.svc.touches.cursor_moved(pos);
        if self.egui_primary.is_none() || self.egui_primary == Some(crate::platform::touch::MOUSE_LEFT) {
            self.egui_events.push(egui::Event::PointerMoved(pos));
        }
    }

    /// The cursor left the window (desktop).
    pub fn cursor_left(&mut self) {
        if self.egui_primary.is_none() {
            self.egui_events.push(egui::Event::PointerGone);
        }
    }

    /// A mouse button (desktop): left = touch [`crate::platform::touch::MOUSE_LEFT`], right = second touch.
    pub fn mouse_button(&mut self, left: bool, pressed: bool) {
        self.svc.touches.mouse_button(left, pressed);
        if left {
            let phase = if pressed { TouchPhase::Started } else { TouchPhase::Ended };
            let pos = self.cursor;
            self.egui_pointer(crate::platform::touch::MOUSE_LEFT, phase, pos);
            if !pressed {
                // Back to hovering.
                self.egui_events.push(egui::Event::PointerMoved(pos));
            }
        }
    }

    fn egui_pointer(&mut self, id: u64, phase: TouchPhase, pos: Pos2) {
        let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        match phase {
            // The newest finger down drives egui's pointer: a steering thumb that
            // stays on the glass must not stop taps on pause / results buttons.
            TouchPhase::Started => {
                self.egui_primary = Some(id);
                self.egui_events.push(egui::Event::PointerMoved(pos));
                self.egui_events.push(button(true));
            }
            TouchPhase::Moved if self.egui_primary == Some(id) => self.egui_events.push(egui::Event::PointerMoved(pos)),
            TouchPhase::Ended if self.egui_primary == Some(id) => {
                self.egui_primary = None;
                self.egui_events.push(egui::Event::PointerMoved(pos));
                self.egui_events.push(button(false));
                if id != crate::platform::touch::MOUSE_LEFT {
                    self.egui_events.push(egui::Event::PointerGone);
                }
            }
            _ => {}
        }
    }

    /// Drop every finger (screen change): raw touches and egui's pointer.
    pub(crate) fn clear_touches(&mut self) {
        self.svc.touches.clear();
        if let Some(id) = self.egui_primary.take() {
            // Let go off-screen, so no widget sees a click, then forget the pointer.
            self.egui_events.push(egui::Event::PointerMoved(OFFSCREEN));
            self.egui_events.push(egui::Event::PointerButton { pos: OFFSCREEN, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
            if id == crate::platform::touch::MOUSE_LEFT {
                self.egui_events.push(egui::Event::PointerMoved(self.cursor));
            } else {
                self.egui_events.push(egui::Event::PointerGone);
            }
        }
    }

    /// A physical key (desktop).
    pub fn key_event(&mut self, key: KeyCode, down: bool) {
        self.svc.keys.key(key, down);
    }

    /// System Back (Android Back key, Esc on desktop). Acted on at the start
    /// of the next frame: the current screen's `Screen::back`.
    pub fn request_back(&mut self) {
        self.back_requested = true;
    }

    // ── lifecycle ───────────────────────────────────────────────────────────

    /// The app went to the background or lost focus: tell the screen, drop
    /// fingers and keys, mute, release the wake lock, save the profile.
    pub fn on_background(&mut self) {
        crate::sound::set_suspended(true);
        if std::mem::replace(&mut self.backgrounded, true) {
            return;
        }
        self.svc.touches.clear();
        self.svc.keys.clear();
        self.egui_primary = None;
        self.egui_events.clear();
        self.svc.muted = true;
        // The room goes on without you: a bot steers while the app is away.
        self.svc.online.with(|s| s.set_away(true));
        let viewport = self.viewport_now();
        let mut ctx = self.svc.ctx(0.0, self.time, viewport, false);
        self.router.top_mut().on_background(&mut ctx);
        self.sync_tilt(0.0);
        self.sync_wake_lock();
        self.svc.profile.save(&self.svc.data_dir);
    }

    /// Back in the foreground: sound on again.
    pub fn on_foreground(&mut self) {
        let was_away = std::mem::replace(&mut self.backgrounded, false);
        if let Some(account) = &self.svc.account { account.resume(); }
        self.sync_orientation();
        self.svc.muted = false;
        crate::sound::set_suspended(false);
        if was_away {
            // Back: if the room survived a long absence, say what happened.
            let back = self.svc.online.with(|s| {
                let away = s.set_away(false)?;
                (away >= 2500 && matches!(s.me().phase(), race_session::ClientPhase::Racing)).then_some(away)
            });
            if let Some(Some(_)) = back {
                self.svc.toast = Some(Toast { text: "Welcome back. A bot kept your ostrich going. Resume to take over.".into(), left: crate::ui::screens::TOAST_SECS + 1.5 });
            }
        }
    }

    /// Is an online session alive that needs driving even with no frames (the
    /// app in the background)? The platform loop calls [`Engine::background_tick`]
    /// every few tens of milliseconds while this is true.
    pub fn wants_background_tick(&self) -> bool {
        self.svc.online.is_some()
            || self.svc.online.has_background()
            || self.svc.account.as_ref().is_some_and(|a| a.busy())
    }

    /// Drive the online session without a frame: keeps a backgrounded player's
    /// seat alive (a bot steers) and the host's room running.
    pub fn background_tick(&mut self) {
        self.account_upkeep();
        self.svc.online.with(|s| {
            s.pump_auto();
            s.end_frame();
        });
        self.save_keep_passes();
        self.svc.online.pump_background();
        self.sync_room_service();
    }

    /// A Keep that admitted this player sent a pass: save the Keep to My Keeps
    /// exactly as a completed invite join would, and join nothing. The same pass
    /// again changes nothing (`record_join` replaces by host key).
    fn save_keep_passes(&mut self) {
        let passes = self.svc.online.with(|s| s.take_keep_passes()).unwrap_or_default();
        for (pass, host_name) in passes {
            match crate::keeps::record_join(&self.svc.data_dir, &pass.host_key, &pass.secret, host_name.as_deref(), crate::keeps::unix_now()) {
                Ok(true) => log::info!("keeps: saved {} from the Keep's pass", crate::keeps::describe(&crate::keeps::JoinTarget::Invite { host_key: pass.host_key, secret: pass.secret })),
                Ok(false) => {}
                Err(e) => log::warn!("keeps: could not save the Keep's pass: {e}"),
            }
        }
    }

    /// Keep the platform's room service on exactly while there is an online session.
    fn sync_room_service(&mut self) {
        let want = self.svc.online.is_some()
            || self.svc.online.has_background()
            || self.svc.account.as_ref().is_some_and(|a| a.busy());
        if want != self.room_service {
            self.room_service = want;
            self.svc.platform.set_room_service(want);
        }
    }

    /// Keep account work alive and enforce sign-in before any screen input.
    fn account_upkeep(&mut self) {
        let mut needs_login = false;
        if let Some(account) = &mut self.svc.account {
            account.tick(self.svc.platform.as_mut());
            needs_login = !account.signed_in();
            if let Some(name) = account.name() {
                self.svc.profile.name = race_session::proto::sanitize_name(&name, "Rider");
            }
            if self.svc.online.is_some() && !account.online_allowed() {
                self.svc.online.leave();
                self.svc.toast = Some(Toast { text: "Online approval ended. Ask My Signet again.".into(), left: crate::ui::screens::TOAST_SECS });
            }
        }
        if needs_login && self.router.top().name() != "signin" {
            self.svc.online.leave();
            self.clear_touches();
            self.router = Router::new(Box::new(crate::ui::screens::account::AccountScreen::login(
                Box::new(crate::ui::screens::home::HomeScreen::new()))));
        }
    }

    /// Per frame, after the screens ran: drive the session if no screen did
    /// (the Garage over the lobby), pull the player back to the online screen
    /// when something needs it (a race started, the link died) and end a
    /// session no screen shows any more.
    fn online_upkeep(&mut self) -> bool {
        use crate::ui::screens::Transition;
        // A standby whose player left runs on in the background (no screen).
        self.svc.online.pump_background();
        if !self.svc.online.is_some() {
            return false;
        }
        self.svc.online.with(|s| s.pump_auto());
        self.save_keep_passes();
        // (Not while a fade-out is pending: the screen that shows the session may be on its way.)
        if !self.router.input_blocked() && !self.router.any_uses_online() {
            log::info!("online: no screen shows the session any more: leaving the room");
            self.svc.online.leave();
            return false;
        }
        let attention = self.svc.online.with(|s| s.dead_reason().is_some() || (matches!(s.me().phase(), race_session::ClientPhase::Racing) && s.me().view().is_some()) || s.migration_state() == crate::online::MigrationState::ConsentRequested).unwrap_or(false);
        let mut quit = false;
        if attention && !self.router.input_blocked() && !self.router.top().uses_online() {
            if let Some(name) = self.router.nearest_online() {
                log::info!("online: back to {name} (the room needs it)");
                self.clear_touches();
                quit = self.apply(Transition::PopTo(name));
            }
        }
        self.svc.online.with(|s| s.end_frame());
        quit
    }

    /// The loop is ending (Android: the process ends right after).
    pub fn on_exit(&mut self) {
        self.svc.platform.set_keep_screen_on(false);
        self.svc.profile.save(&self.svc.data_dir);
    }

    fn viewport_now(&self) -> Viewport {
        // Only used outside frames (background hook): a nominal landscape size.
        Viewport::new(egui::vec2(890.0, 411.0), self.ppp, self.insets)
    }

    fn sync_orientation(&mut self) {
        if self.backgrounded { return; }
        let want = self.router.top().orientation();
        if self.orientation != Some(want) {
            self.orientation = Some(want);
            self.svc.platform.set_orientation(want);
        }
    }

    fn sync_wake_lock(&mut self) {
        let want = !self.backgrounded && self.router.top().keep_awake();
        if want != self.keep_on {
            self.keep_on = want;
            self.svc.platform.set_keep_screen_on(want);
        }
    }

    fn sync_tilt(&mut self, dt: f32) {
        let want = !self.backgrounded && self.svc.profile.steer_mode == crate::platform::data_dir::SteerMode::Tilt && self.router.top().uses_tilt();
        self.svc.platform.set_tilt_enabled(want);
        if !want { self.svc.tilt.reset(); }
        else if dt > 0.0 {
            let reading = self.svc.platform.tilt_reading();
            let before = self.svc.tilt.status();
            self.svc.tilt.update(reading, dt, self.svc.profile.tilt_sensitivity);
            if crate::game::stage::diag_build() && before != self.svc.tilt.status() {
                log::info!("tilt: {:?}, {reading:?}", self.svc.tilt.status());
            }
        }
    }

    /// Apply a transition; true when the event loop must end. A quit first
    /// asks the platform to leave the app its own way: on Android that sends
    /// the game to the background and the loop goes on.
    fn apply(&mut self, t: crate::ui::screens::Transition) -> bool {
        let t = match t {
            crate::ui::screens::Transition::Push(next) if matches!(next.name(), "join" | "nearby" | "lobby")
                && self.svc.account.as_ref().is_some_and(|a| !a.online_allowed()) => {
                crate::ui::screens::Transition::Push(Box::new(crate::ui::screens::account::AccountScreen::online(Some(next))))
            }
            crate::ui::screens::Transition::Cut(next) if matches!(next.name(), "host" | "join" | "nearby" | "lobby")
                && self.svc.account.as_ref().is_some_and(|a| !a.online_allowed()) => {
                crate::ui::screens::Transition::Cut(Box::new(crate::ui::screens::account::AccountScreen::online(Some(next))))
            }
            crate::ui::screens::Transition::Replace(next) if matches!(next.name(), "host" | "join" | "nearby" | "lobby")
                && self.svc.account.as_ref().is_some_and(|a| !a.online_allowed()) => {
                crate::ui::screens::Transition::Replace(Box::new(crate::ui::screens::account::AccountScreen::online(Some(next))))
            }
            other => other,
        };
        match self.router.apply(t) {
            Applied::Stay => false,
            Applied::Quit => !self.svc.platform.leave_app(),
        }
    }

    // ── the frame ───────────────────────────────────────────────────────────

    /// Run one frame of `dt` seconds on a surface of `size_px` at OS scale
    /// `native_ppp`. `raw` is egui's input for this frame (from egui-winit,
    /// or `RawInput::default()` headless).
    pub fn frame(&mut self, dt: f32, size_px: (u32, u32), native_ppp: f32, mut raw: egui::RawInput) -> FrameOut {
        use crate::ui::screens::Transition;
        let t0 = std::time::Instant::now();
        let dt = dt.clamp(0.0, 0.1);
        self.set_surface(size_px, native_ppp);
        let ppp = self.ppp;
        self.time += dt as f64;
        self.frames += 1;
        self.svc.touches.time = self.time;
        let viewport = Viewport::new(egui::vec2(size_px.0 as f32 / ppp, size_px.1 as f32 / ppp), ppp, self.insets);
        let time = self.time;
        let mut quit = false;

        // Updates: the saved verdict, the launch check, and any download being followed.
        self.svc.updates.tick(dt, self.svc.platform.as_mut());
        self.account_upkeep();

        // Launch links (cold start or while running).
        while let Some(link) = self.svc.platform.take_launch_link() {
            match crate::keeps::parse_launch_link(&link) {
                Some(target) => {
                    log::info!("launch link: join {}", crate::keeps::describe(&target));
                    self.svc.join_code = Some(target);
                }
                None => log::info!("launch link ignored ({} chars)", link.len()),
            }
        }
        if let Some(t) = self.svc.toast.as_mut() {
            t.left -= dt;
        }
        if self.svc.toast.as_ref().is_some_and(|t| t.left <= 0.0) {
            self.svc.toast = None;
        }

        // System Back, before the screen's own logic.
        if std::mem::take(&mut self.back_requested) && !self.router.input_blocked() {
            let mut ctx = self.svc.ctx(dt, time, viewport, false);
            let t = self.router.top_mut().back(&mut ctx);
            if !t.is_none() {
                self.clear_touches();
            }
            quit |= self.apply(t);
        }

        // Logic.
        self.sync_tilt(dt);
        {
            let blocked = self.router.input_blocked();
            let mut ctx = self.svc.ctx(dt, time, viewport, blocked);
            let t = self.router.top_mut().update(&mut ctx);
            if !t.is_none() {
                self.clear_touches();
            }
            quit |= self.apply(t);
        }
        quit |= self.online_upkeep();
        self.router.tick(dt);

        // UI.
        raw.screen_rect = Some(egui::Rect::from_min_size(Pos2::ZERO, viewport.size()));
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
        if self.router.input_blocked() {
            // A fade-out holds input back from egui too (the old screen is still
            // drawn and live): only let go of anything held, off-screen.
            for e in self.egui_events.drain(..) {
                match e {
                    egui::Event::PointerButton { pressed: false, button, modifiers, .. } => raw.events.push(egui::Event::PointerButton { pos: OFFSCREEN, button, pressed: false, modifiers }),
                    egui::Event::PointerGone => raw.events.push(e),
                    _ => {}
                }
            }
        } else {
            raw.events.append(&mut self.egui_events);
        }
        // egui's clock is ours, not egui-winit's: that one restarts at 0 when
        // the window (and its egui_winit::State) is rebuilt on an Android
        // resume, and with time running backwards every Area's fade-in sat at
        // opacity 0 until the clock caught up again (Twang Thang, device
        // test 1: buttons gone after pause/resume).
        raw.time = Some(time);
        self.egui.begin_pass(raw);
        let clear;
        let t = {
            let blocked = self.router.input_blocked();
            let mut ctx = self.svc.ctx(dt, time, viewport, blocked);
            clear = self.router.top().clear_color(&ctx);
            self.router.top_mut().draw(&mut ctx, &self.egui)
        };
        if let Some(toast) = &self.svc.toast {
            draw_toast(&self.egui, &viewport, toast);
        }
        let alpha = self.router.fade_alpha();
        if alpha > 0.0 {
            let p = self.egui.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("stampede-fade")));
            p.rect_filled(viewport.screen, 0.0, theme::BG.gamma_multiply(alpha));
        }
        let out = self.egui.end_pass();
        if !matches!(t, Transition::None) {
            self.clear_touches();
        }
        quit |= self.apply(t);

        self.sync_wake_lock();
        self.sync_room_service();
        self.sync_orientation();
        self.sync_tilt(0.0);
        self.svc.touches.end_frame(dt);
        self.svc.keys.end_frame();
        self.last_ms = t0.elapsed().as_secs_f32() * 1000.0;
        FrameOut { egui: out, clear, viewport, quit }
    }
}

/// A rounded message near the bottom centre.
fn draw_toast(ctx: &egui::Context, vp: &Viewport, toast: &Toast) {
    let a = ((toast.left / 0.3).min(1.0)).clamp(0.0, 1.0);
    let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("stampede-toast")));
    let galley = p.layout_no_wrap(toast.text.clone(), theme::ui_bold(15.0), theme::TEXT.gamma_multiply(a));
    let size = galley.size() + egui::vec2(28.0, 16.0);
    let rect = egui::Rect::from_center_size(pos2(vp.safe.center().x, vp.safe.bottom() - 36.0), size);
    p.rect_filled(rect, size.y / 2.0, theme::BG.gamma_multiply(0.92 * a));
    p.rect_stroke(rect, size.y / 2.0, egui::Stroke::new(1.5_f32, theme::ACCENT.gamma_multiply(a)), egui::StrokeKind::Inside);
    p.galley(rect.center() - galley.size() / 2.0, galley, theme::TEXT);
}

#[cfg(test)]
mod account_tests {
    use super::*;
    use crate::platform::os::RecordingPlatform;
    use crate::haptics::NoHaptics;
    use crate::ui::screens::{account::AccountScreen, settings::SettingsScreen, Transition};

    #[test]
    fn signed_out_account_cannot_back_through_a_pending_fade_into_local_play() {
        let dir = std::env::temp_dir().join(format!("stampede-account-back-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::new(Box::new(SettingsScreen::new()), Profile::default(), dir.clone(), Options::default(),
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        e.router.apply(Transition::Push(Box::new(AccountScreen::manage())));
        for _ in 0..30 { e.router.tick(1.0 / 60.0); }
        assert_eq!(e.router.names(), ["settings", "play_online"]);
        e.router.apply(Transition::Pop);
        assert!(e.router.input_blocked());
        e.svc.account = Some(crate::account::Account::open(&dir));
        e.request_back();
        e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        assert_eq!(e.router.names(), ["signin"]);
        for _ in 0..30 { e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default()); }
        assert_eq!(e.router.names(), ["signin"]);
        assert!(!e.svc.online.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tilt_tests {
    use super::*;
    use crate::platform::{data_dir::SteerMode, os::RecordingPlatform, tilt::{Reading, Status}};
    use crate::haptics::NoHaptics;
    use crate::ui::screens::{Screen, Transition};
    struct Driving;
    impl Screen for Driving {
        fn name(&self) -> &'static str { "tilt-test" }
        fn uses_tilt(&self) -> bool { true }
    }
    #[test]
    fn screen_off_silences_audio_without_frames_or_a_screen_background_hook() {
        let dir = std::env::temp_dir().join(format!("stampede-screen-off-{}", std::process::id()));
        let profile = Profile { music_volume: 0.37, effects_volume: 0.64, ..Default::default() };
        let mut e = Engine::new(Box::new(Driving), profile, dir.clone(),
            Options { shot: Some("test".into()), ..Default::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        crate::sound::init(true);
        e.on_foreground();
        assert!(!crate::sound::with(|a| a.is_suspended()));
        for _ in 0..2 { // Focus loss followed by surface suspension is idempotent.
            e.on_background();
            assert!(e.svc.muted);
            assert!(crate::sound::with(|a| a.is_suspended()));
        }
        crate::sound::with(|a| a.resume_all());
        assert!(crate::sound::with(|a| a.is_suspended()));
        e.on_foreground();
        assert!(!e.svc.muted);
        assert!(!crate::sound::with(|a| a.is_suspended()));
        assert_eq!(e.svc.profile.music_volume, 0.37);
        assert_eq!(e.svc.profile.effects_volume, 0.64);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn sensor_input_runs_only_on_an_active_tilt_screen_and_resets_on_return() {
        let platform = RecordingPlatform { tilt: Reading::Sample { gravity: [0.0, 0.0, 9.81], rotation: 1, generation: 1, age: 0.01 }, ..Default::default() };
        let dir = std::env::temp_dir().join(format!("stampede-tilt-{}", std::process::id()));
        let mut e = Engine::new(Box::new(Driving), Profile { steer_mode: SteerMode::Tilt, ..Default::default() }, dir.clone(),
            Options { shot: Some("test".into()), ..Default::default() }, Box::new(platform), Box::new(NoHaptics::default()));
        for _ in 0..30 { e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default()); }
        assert!(e.svc.platform.recorded().unwrap().tilt_enabled); assert_eq!(e.svc.tilt.status(), Status::Ready);
        e.on_background(); assert!(!e.svc.platform.recorded().unwrap().tilt_enabled); assert_eq!(e.svc.tilt.steer(), None);
        e.on_foreground(); e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        assert_eq!(e.svc.tilt.status(), Status::Centring);
        e.router.apply(Transition::Cut(Box::new(crate::ui::screens::home::HomeScreen::new())));
        e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        assert!(!e.svc.platform.recorded().unwrap().tilt_enabled);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod trophy_tests {
    use super::*;
    use crate::flow::{RaceKind, RaceLaunch};
    use crate::haptics::NoHaptics;
    use crate::platform::os::RecordingPlatform;
    use crate::ui::screens::after_race::{sample_outcome, GpScreen, SampleKind};
    use race_core::TrackId;

    /// The last round of a cup that `local_points` before it leaves the local seat
    /// (the others have 10-70) on top or far behind.
    fn final_round(tracks: &[TrackId], local_points: u32) -> GpScreen {
        let mut o = sample_outcome(SampleKind::Gp(3));
        let n = o.launch.config.racers.len();
        let mut points: Vec<u32> = (0..n as u32).map(|i| 70 - i * 8).collect();
        points[0] = local_points;
        o.launch.kind = RaceKind::GrandPrix { tracks: tracks.to_vec(), round: tracks.len() - 1, points };
        o.launch.config.track = *tracks.last().unwrap();
        GpScreen::new(o)
    }

    fn engine(dir: &std::path::Path, first: GpScreen) -> Engine {
        Engine::new(Box::new(first), Profile::default(), dir.to_path_buf(), Options::default(),
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()))
    }

    fn run_frames(e: &mut Engine, n: usize) {
        for _ in 0..n {
            e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        }
    }

    #[test]
    fn a_won_grand_prix_is_on_disk_after_the_standings_first_frame_and_counted_once() {
        for (tracks, bit) in [(TrackId::RACE.to_vec(), 0b01u8), (TrackId::CUP2.to_vec(), 0b10)] {
            let dir = std::env::temp_dir().join(format!("stampede-trophy-{}-{bit}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let mut e = engine(&dir, final_round(&tracks, 400));
            run_frames(&mut e, 1);
            // Straight from disk, as the next launch would read it.
            let saved = Profile::load(&dir);
            assert_eq!(saved.gp_wins, 1, "the win is saved on the first frame");
            assert_eq!(saved.progress.cups_won, bit, "so is the cup");
            assert!(saved.progress.achievements != 0, "the champion achievement is saved too");
            // Staying on the page, going to the trophy page and backgrounding add nothing.
            run_frames(&mut e, 120);
            e.on_background();
            e.on_exit();
            let again = Profile::load(&dir);
            assert_eq!(again.gp_wins, 1);
            assert_eq!(again.progress.cups_won, bit);
            assert_eq!(again.progress.sats, saved.progress.sats, "the prize is paid once");
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    // ── the real race screen, driven by the autopilot ───────────────────────

    use crate::game::race::{new as race_new, Staging};
    use crate::ui::screens::{after_race::CUP_DECISIONS, Screen, Transition};
    use race_core::BotLevel;

    /// The screen under the race, so that leaving it has somewhere to go.
    struct Setup;
    impl Screen for Setup {
        fn name(&self) -> &'static str { "setup-stub" }
    }

    /// The last round of a one-lap cup; the local seat starts with `local_points`
    /// (the others have 10-70), so 400 wins whatever the race does and 0 cannot.
    fn final_launch(local_points: u32) -> RaceLaunch {
        let profile = Profile::default();
        let mut l = crate::ui::screens::common::grand_prix_launch(&profile, crate::ui::screens::common::Cup::One, 1, BotLevel::Normal);
        let tracks = TrackId::RACE.to_vec();
        let n = l.config.racers.len();
        let mut points: Vec<u32> = (0..n as u32).map(|i| 70 - i * 8).collect();
        points[0] = local_points;
        l.config.track = *tracks.last().unwrap();
        l.kind = RaceKind::GrandPrix { tracks: tracks.clone(), round: tracks.len() - 1, points };
        l
    }

    /// An engine with a real race screen, driven by the autopilot, over a stub.
    fn race_engine(dir: &std::path::Path, launch: RaceLaunch) -> Engine {
        CUP_DECISIONS.with(|c| c.set(0));
        let mut e = Engine::new(Box::new(Setup), Profile::default(), dir.to_path_buf(),
            Options { shot: Some("test".into()), ..Default::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        let race = race_new(launch, Staging { autopilot: Some(BotLevel::Normal), ..Default::default() });
        e.router.apply(Transition::Push(Box::new(race)));
        e
    }

    /// Runs frames until `done` or the budget is spent; returns the frames used.
    fn run_until(e: &mut Engine, budget: usize, mut done: impl FnMut(&Engine) -> bool) -> usize {
        for k in 0..budget {
            if done(e) {
                return k;
            }
            e.frame(1.0 / 60.0, (2400, 1080), 3.0, egui::RawInput::default());
        }
        panic!("not reached in {budget} frames; screens {:?}", e.router.names());
    }

    fn race_is_over() -> bool {
        CUP_DECISIONS.with(|c| c.get()) > 0
    }

    fn frame(e: &mut Engine, raw: egui::RawInput) -> FrameOut {
        e.frame(1.0 / 60.0, (2400, 1080), 3.0, raw)
    }

    /// Is the race's pause menu on screen?
    fn pause_open(e: &Engine) -> bool {
        let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("race-pause"));
        e.egui.memory(|m| m.areas().visible_last_frame(&layer))
    }

    /// The pause menu's height in points: 322 with RESTART, 273 without.
    fn pause_height(e: &Engine) -> f32 {
        e.egui.memory(|m| m.area_rect(egui::Id::new("race-pause"))).map_or(0.0, |r| r.height())
    }

    /// Opens the pause menu with System Back and lets it settle.
    fn open_pause(e: &mut Engine) -> egui::Rect {
        e.request_back();
        let mut out = None;
        for _ in 0..30 {
            out = Some(frame(e, egui::RawInput::default()));
        }
        assert!(pause_open(e), "the pause menu opened");
        out.unwrap().viewport.safe
    }

    /// Taps the pause menu's `slot`-th visible button (counting from 0), the way a player does.
    fn tap_slot(e: &mut Engine, safe: egui::Rect, slot: usize) {
        let h = pause_height(e);
        let panel = egui::Rect::from_center_size(safe.center(), egui::vec2(248.0, h));
        let at = egui::pos2(panel.center().x, panel.top() + 62.0 + slot as f32 * 49.0 + 20.5);
        let ev = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        frame(e, egui::RawInput { events: vec![egui::Event::PointerMoved(at)], ..Default::default() });
        frame(e, egui::RawInput { events: vec![ev(true)], ..Default::default() });
        frame(e, egui::RawInput { events: vec![ev(false)], ..Default::default() });
    }

    /// Opens the pause menu and taps QUIT (the last button).
    fn tap_quit(e: &mut Engine) {
        let safe = open_pause(e);
        let last = if pause_height(e) < 300.0 { 3 } else { 4 };
        tap_slot(e, safe, last);
    }

    #[test]
    fn a_won_cup_is_saved_the_moment_the_race_ends_and_counted_once_after_the_standings() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = race_engine(&dir, final_launch(400));
        assert_eq!(Profile::load(&dir).gp_wins, 0);
        run_until(&mut e, 12_000, |_| race_is_over());
        // The race has ended, the race screen is still up for its wait: the win is on disk.
        assert_eq!(e.router.names(), ["setup-stub", "race"]);
        let saved = Profile::load(&dir);
        assert_eq!(saved.gp_wins, 1, "saved when the race ended");
        // A suspend and a restore in the wait add nothing, and lose nothing.
        e.on_background();
        e.on_exit();
        e.on_foreground();
        assert_eq!(Profile::load(&dir).gp_wins, 1);
        run_until(&mut e, 1_200, |e| e.router.names().last() == Some(&"gp_standings"));
        run_frames(&mut e, 120);
        let after = Profile::load(&dir);
        assert_eq!(after.gp_wins, 1, "counted once");
        assert_eq!(after.progress.cups_won, 0b01, "the cup is in the book once the standings are up");
        assert!(after.progress.achievements != 0, "the champion achievement too");
        assert!(after.progress.sats > 0, "and the prize is paid");
        let sats = after.progress.sats;
        e.on_background();
        e.on_exit();
        assert_eq!(Profile::load(&dir).gp_wins, 1);
        assert_eq!(Profile::load(&dir).progress.sats, sats, "paid once");
        assert_eq!(CUP_DECISIONS.with(|c| c.get()), 1, "decided once");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quitting_after_crossing_the_line_still_records_the_cup_once_and_shows_the_standings() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-quit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = race_engine(&dir, final_launch(400));
        run_until(&mut e, 12_000, |_| race_is_over());
        assert_eq!(e.router.names(), ["setup-stub", "race"], "still in the wait for the results");
        let safe = open_pause(&mut e);
        assert!(pause_height(&e) < 300.0, "the menu is up, without RESTART");
        tap_slot(&mut e, safe, 3);
        run_until(&mut e, 600, |e| e.router.names().last() == Some(&"gp_standings"));
        assert_eq!(e.router.names(), ["setup-stub", "gp_standings"], "Quit after the line is not a quit");
        run_frames(&mut e, 120);
        let saved = Profile::load(&dir);
        assert_eq!(saved.gp_wins, 1, "recorded exactly once");
        assert_eq!(saved.progress.cups_won, 0b01);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quitting_before_finishing_leaves_without_recording_anything() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-early-quit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = race_engine(&dir, final_launch(400));
        run_frames(&mut e, 300);
        assert_eq!(e.router.names(), ["setup-stub", "race"]);
        tap_quit(&mut e);
        run_frames(&mut e, 90);
        assert_eq!(e.router.names(), ["setup-stub"], "back to where the race began");
        let saved = Profile::load(&dir);
        assert_eq!((saved.gp_wins, saved.progress.cups_won, saved.progress.achievements), (0, 0, 0));
        assert_eq!(CUP_DECISIONS.with(|c| c.get()), 0, "no cup was decided");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cup_not_won_is_decided_at_the_end_and_records_nothing() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = race_engine(&dir, final_launch(0));
        run_until(&mut e, 12_000, |_| race_is_over());
        assert_eq!(CUP_DECISIONS.with(|c| c.get()), 1, "the end-of-race code ran and decided");
        assert_eq!(Profile::load(&dir).gp_wins, 0);
        run_until(&mut e, 1_200, |e| e.router.names().last() == Some(&"gp_standings"));
        run_frames(&mut e, 120);
        let saved = Profile::load(&dir);
        assert_eq!((saved.gp_wins, saved.progress.cups_won), (0, 0));
        assert_eq!(CUP_DECISIONS.with(|c| c.get()), 1, "and the standings did not decide it again");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mid_cup_round_records_nothing_at_the_end_of_the_race() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-mid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut l = final_launch(400);
        let tracks = TrackId::RACE.to_vec();
        l.config.track = tracks[1];
        l.kind = RaceKind::GrandPrix { tracks, round: 1, points: vec![400, 30, 20, 10, 5, 4, 3, 2] };
        let mut e = race_engine(&dir, l);
        run_until(&mut e, 12_000, |e| e.router.names().last() == Some(&"gp_standings"));
        run_frames(&mut e, 120);
        let saved = Profile::load(&dir);
        assert_eq!((saved.gp_wins, saved.progress.cups_won), (0, 0));
        assert_eq!(CUP_DECISIONS.with(|c| c.get()), 0, "nothing is decided before the last round");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn restart_is_gone_from_the_pause_menu_once_the_cup_race_is_won_and_works_before() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = race_engine(&dir, final_launch(400));
        // Before the line: RESTART is offered and works, and records nothing.
        run_frames(&mut e, 300);
        let safe = open_pause(&mut e);
        assert!(pause_height(&e) > 300.0, "RESTART is on the menu before the line");
        tap_slot(&mut e, safe, 1);
        run_frames(&mut e, 30);
        assert!(!pause_open(&e), "a fresh race replaced the paused one");
        assert_eq!(e.router.names(), ["setup-stub", "race"]);
        assert_eq!((Profile::load(&dir).gp_wins, CUP_DECISIONS.with(|c| c.get())), (0, 0));
        // After the line: no RESTART, and nothing a tap can reach replays the round.
        run_until(&mut e, 12_000, |_| race_is_over());
        let safe = open_pause(&mut e);
        assert!(pause_height(&e) < 300.0, "no RESTART once across the line");
        tap_slot(&mut e, safe, 1); // where RESTART was: now CAMERA
        run_frames(&mut e, 30);
        assert!(pause_open(&e), "still the same paused race");
        assert_eq!(Profile::load(&dir).gp_wins, 1);
        assert_eq!(CUP_DECISIONS.with(|c| c.get()), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cup_win_and_its_cup_bit_are_saved_together_when_the_race_ends() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-bit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = race_engine(&dir, final_launch(400));
        run_until(&mut e, 12_000, |_| race_is_over());
        // As a killed game would find it: before the standings, the achievement or the prize.
        let saved = Profile::load(&dir);
        assert_eq!((saved.gp_wins, saved.progress.cups_won), (1, 0b01));
        assert_eq!(saved.progress.achievements, 0);
        assert_eq!(saved.progress.sats, 0);
        // The standings still award them, and the bit stays one bit.
        run_until(&mut e, 1_200, |e| e.router.names().last() == Some(&"gp_standings"));
        run_frames(&mut e, 60);
        let after = Profile::load(&dir);
        assert_eq!((after.gp_wins, after.progress.cups_won), (1, 0b01));
        assert!(after.progress.achievements != 0 && after.progress.sats > 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A stand-in for an online source: a local race that reports a connection and
    /// notes what `exit_transition` is told.
    struct FakeNet {
        inner: crate::game::source::LocalRace,
        left: std::rc::Rc<std::cell::Cell<Option<bool>>>,
        /// The local racer has crossed the line.
        over: std::rc::Rc<std::cell::Cell<bool>>,
    }
    impl crate::game::source::RaceSource for FakeNet {
        fn view(&self) -> crate::game::source::RaceView<'_> { self.inner.view() }
        fn local_seat(&self) -> Option<usize> { self.inner.local_seat() }
        fn advance(&mut self, dt: f32, input: &mut dyn FnMut() -> race_core::Input) -> u32 {
            let n = self.inner.advance(dt, input);
            if self.inner.view().racers[0].finished_at.is_some() {
                self.over.set(true);
            }
            n
        }
        fn drain_events(&mut self) -> Vec<race_core::Event> { self.inner.drain_events() }
        fn set_autopilot(&mut self, level: Option<BotLevel>, forced: crate::game::source::Forced) { self.inner.set_autopilot(level, forced) }
        fn net_status(&self) -> Option<crate::game::source::NetStatus> { Some(Default::default()) }
        fn exit_transition(&mut self, _ctx: &mut crate::ui::screens::Ctx, quit: bool) -> Option<Transition> {
            self.left.set(Some(quit));
            Some(Transition::Pop)
        }
    }

    #[test]
    fn online_a_finished_racer_who_taps_quit_still_quits() {
        let dir = std::env::temp_dir().join(format!("stampede-trophy-online-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        CUP_DECISIONS.with(|c| c.set(0));
        let launch = final_launch(400);
        let left = std::rc::Rc::new(std::cell::Cell::new(None));
        let mut src = crate::game::source::LocalRace::new(launch.config.clone(), Some(0));
        crate::game::source::RaceSource::set_autopilot(&mut src, Some(BotLevel::Normal), Default::default());
        let over = std::rc::Rc::new(std::cell::Cell::new(false));
        let net = FakeNet { inner: src, left: left.clone(), over: over.clone() };
        let race = crate::game::race::with_source(launch, Staging { autopilot: Some(BotLevel::Normal), ..Default::default() }, Box::new(net));
        let mut e = Engine::new(Box::new(Setup), Profile::default(), dir.clone(),
            Options { shot: Some("test".into()), ..Default::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        e.router.apply(Transition::Push(Box::new(race)));
        // Across the line, in the 3.4 s wait: QUIT must reach the source as a quit
        // (the online source leaves the room on it), not be read as "not quit".
        run_until(&mut e, 12_000, |_| over.get());
        assert_eq!(left.get(), None, "still waiting");
        tap_quit(&mut e);
        assert_eq!(left.get(), Some(true), "the source was told the player quit");
        assert_eq!((Profile::load(&dir).gp_wins, CUP_DECISIONS.with(|c| c.get())), (0, 0), "online records no cup");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod orientation_tests {
    use super::*;
    use crate::platform::os::{Orientation, RecordingPlatform};
    use crate::haptics::NoHaptics;
    use crate::ui::screens::{self, Transition};

    #[test]
    fn rotation_follows_the_top_screen_through_fades_and_resume() {
        let mut e = Engine::new(Box::new(screens::menu::MenuScreen::new()), Profile::default(),
            std::env::temp_dir().join("stampede-orientation"),
            Options { shot: Some("test".into()), ..Default::default() },
            Box::new(RecordingPlatform::default()), Box::new(NoHaptics::default()));
        e.sync_orientation();
        assert_eq!(e.orientation, Some(Orientation::Free));
        e.router.apply(Transition::Push(screens::by_name("race_countdown").unwrap()));
        e.sync_orientation();
        assert_eq!(e.orientation, Some(Orientation::Free), "outgoing menu still owns the frame");
        e.router.tick(1.0);
        e.sync_orientation();
        assert_eq!(e.orientation, Some(Orientation::Landscape));
        e.on_background();
        e.on_foreground();
        assert_eq!(e.orientation, Some(Orientation::Landscape));
        e.router.tick(1.0);
        e.router.apply(Transition::Pop);
        e.router.tick(1.0);
        e.sync_orientation();
        assert_eq!(e.orientation, Some(Orientation::Free));
        for name in ["run_track", "place_items", "designer", "track_build", "drill_slalom"] {
            assert_eq!(screens::by_name(name).unwrap().orientation(), Orientation::Landscape, "{name}");
        }
    }
}
