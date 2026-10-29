//! Android glue for a NativeActivity APK whose only Java is one tiny class
//! (`StampedeActivity`, `app/android/java`): every OS service is a JNI call
//! from Rust (`jni` 0.21 + `ndk-context`). Ported from Twang Thang.
//!
//! Rules every call here follows:
//! * It goes through [`with_env`]: a fresh local-reference frame, any Java
//!   exception is described to logcat and cleared, any Rust panic is caught.
//!   A failure is logged and becomes a no-op / friendly error, never a crash.
//! * Never call `AndroidApp` methods that take its inner lock for writing
//!   (`set_window_flags`, `enable_motion_axis`, ...) from the game thread:
//!   winit runs us inside `poll_events`, which holds that lock for reading,
//!   so they deadlock. Use JNI on the UI thread instead.
//! * Window and view calls (immersive mode, insets, keep-screen-on) run on the
//!   Java UI thread via `AndroidApp::run_on_java_main_thread`; binder-only
//!   calls (clipboard, intents, Wi-Fi, vibrator) run on the calling thread.
//! * System services come from the Application context (`ndk_context`);
//!   intents and the window come from the Activity (`AndroidApp`).
//!
//! Launch links: the activity is `singleTask` and `StampedeActivity` overrides
//! `onNewIntent` to call `setIntent`, so `getIntent()` always holds the newest
//! intent (stock NativeActivity drops it). We read `getIntent()` over JNI at
//! start-up, on resume and at most every [`LINK_POLL`] while frames run (winit
//! does not surface Android's `Resume`, and a link that reaches a foreground
//! activity pauses and resumes it without recreating the window). A link is
//! queued, then consumed with `intent.setData(null)` so it is handled once.

use super::lan::{choose_local_network, LocalNetwork, Seen};
use super::os::Platform;
use super::Insets;
use android_activity::AndroidApp;
use jni::objects::{GlobalRef, JFloatArray, JObject, JObjectArray, JString, JValue};
use jni::{JNIEnv, JavaVM};
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
/// `WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON`.
const FLAG_KEEP_SCREEN_ON: i32 = 0x80;
/// `WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE` (API 30).
const BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE: i32 = 2;
/// API < 30: LAYOUT_STABLE | LAYOUT_HIDE_NAVIGATION | LAYOUT_FULLSCREEN |
/// HIDE_NAVIGATION | FULLSCREEN | IMMERSIVE_STICKY.
const LEGACY_IMMERSIVE: i32 = 0x0100 | 0x0200 | 0x0400 | 0x0002 | 0x0004 | 0x1000;
/// How many launch links we keep waiting at most.
const MAX_LINKS: usize = 4;
/// How often the frame loop re-reads `getIntent()` (one cheap JNI call).
const LINK_POLL: Duration = Duration::from_millis(250);
/// How often the cutout insets are re-read: a 180-degree flip of a
/// `sensorLandscape` phone moves the cutout to the other side without
/// recreating the activity.
const INSETS_POLL: Duration = Duration::from_millis(1000);

/// State shared between the game thread and UI-thread callbacks.
struct Shared {
    app: Option<AndroidApp>,
    links: VecDeque<String>,
    /// A link whose `setData(null)` failed: don't queue it again.
    stuck_link: Option<String>,
    /// When `getIntent()` was last read.
    last_poll: Option<Instant>,
    /// Display-cutout safe insets in px: top, bottom, left, right.
    insets_px: Option<[i32; 4]>,
    /// When an inset read was last queued on the UI thread.
    insets_asked: Option<Instant>,
    /// Root window insets can arrive a few frames after a resize.
    insets_settling: Option<Instant>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { app: None, links: VecDeque::new(), stuck_link: None, last_poll: None, insets_px: None, insets_asked: None, insets_settling: None });

fn shared() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

fn app() -> Option<AndroidApp> {
    shared().app.clone()
}

pub(crate) fn sdk() -> i32 {
    AndroidApp::sdk_version()
}

/// Called from `android_main` before anything else touches the platform.
/// Once per process: a second `android_main` ends the process instead
/// (`platform::lifecycle`).
pub fn init(app: &AndroidApp) {
    {
        let mut s = shared();
        s.app = Some(app.clone());
        s.links.clear();
        s.stuck_link = None;
        s.last_poll = None;
        s.insets_px = None;
        s.insets_asked = None;
    }
    log::info!("android: API {}", sdk());
    poll_launch_links();
    apply_immersive();
}

/// The window came back (winit `resumed`): re-apply immersive mode, re-read
/// the insets and pick up a link the app was brought forward with.
pub fn on_resume() {
    on_resize();
    apply_immersive();
    poll_launch_links();
}

/// Surface dimensions changed: ask the UI thread for the new cutout immediately.
pub fn on_resize() {
    {
        let mut s = shared();
        s.insets_asked = None;
        s.insets_settling = Some(Instant::now());
    }
    request_insets();
}

/// Safe-area insets for a UI at `ppp` px per point: the display cutout on all
/// four edges (in landscape it is on the left or right). The first frames are
/// zero until the UI thread has answered; re-read every [`INSETS_POLL`].
pub fn insets(ppp: f32) -> Insets {
    let (px, need_read) = {
        let mut s = shared();
        let interval = if s.insets_settling.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) { Duration::from_millis(50) } else { INSETS_POLL };
        let need = s.app.is_some() && s.insets_asked.is_none_or(|t| t.elapsed() >= interval);
        if need {
            s.insets_asked = Some(Instant::now());
        }
        (s.insets_px, need)
    };
    if need_read {
        request_insets();
    }
    let Some([t, b, l, r]) = px else { return Insets::ZERO };
    let k = if ppp > 0.0 { 1.0 / ppp } else { 1.0 };
    Insets { top: t as f32 * k, bottom: b as f32 * k, left: l as f32 * k, right: r as f32 * k }
}

/// The activity this loop runs for is destroyed (`Activity.isDestroyed()`,
/// already true while its `onDestroy` runs, which waits for `android_main` to
/// return). One JNI call; false when it can't be asked.
pub fn activity_destroyed() -> bool {
    let Some(app) = app() else { return false };
    with_env("isDestroyed", |env| env.call_method(activity_of(&app), "isDestroyed", "()Z", &[])?.z()).unwrap_or(false)
}

// ─── JNI plumbing ───────────────────────────────────────────────────────────

fn vm() -> Option<JavaVM> {
    let ctx = std::panic::catch_unwind(ndk_context::android_context).ok()?;
    if ctx.vm().is_null() {
        return None;
    }
    // SAFETY: the pointer is the process JavaVM, set up by android-activity.
    unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()
}

/// Run `f` with an attached `JNIEnv` inside a local-reference frame. Java
/// exceptions are logged and cleared, panics caught; any failure is `None`.
pub(crate) fn with_env<T>(what: &str, f: impl FnOnce(&mut JNIEnv) -> jni::errors::Result<T>) -> Option<T> {
    let run = std::panic::AssertUnwindSafe(|| -> Option<T> {
        let vm = vm()?;
        let mut env = match vm.attach_current_thread() {
            Ok(e) => e,
            Err(e) => {
                log::warn!("android: {what}: no JNI env ({e})");
                return None;
            }
        };
        match env.with_local_frame(32, |env| f(env)) {
            Ok(v) => Some(v),
            Err(e) => {
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_describe();
                    let _ = env.exception_clear();
                }
                log::warn!("android: {what} failed: {e}");
                None
            }
        }
    });
    std::panic::catch_unwind(run).unwrap_or_else(|_| {
        log::error!("android: {what} panicked");
        None
    })
}

/// The Application context (system services). Not owned: never deleted.
fn app_context<'a>() -> jni::errors::Result<JObject<'a>> {
    let ctx = std::panic::catch_unwind(ndk_context::android_context).map_err(|_| jni::errors::Error::NullPtr("android context"))?;
    if ctx.context().is_null() {
        return Err(jni::errors::Error::NullPtr("android context"));
    }
    // SAFETY: a global reference held for the life of the process.
    Ok(unsafe { JObject::from_raw(ctx.context().cast()) })
}

/// The current Activity. Not owned: never deleted. Valid while `app` lives.
fn activity_of<'a>(app: &AndroidApp) -> JObject<'a> {
    // SAFETY: android-activity keeps this reference alive while the AndroidApp exists.
    unsafe { JObject::from_raw(app.activity_as_ptr().cast()) }
}

/// `context.getSystemService(name)`, or an error if it is null.
pub(crate) fn system_service<'a>(env: &mut JNIEnv<'a>, name: &str) -> jni::errors::Result<JObject<'a>> {
    let ctx = app_context()?;
    let n = env.new_string(name)?;
    let svc = env.call_method(&ctx, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&n)])?.l()?;
    if svc.is_null() {
        return Err(jni::errors::Error::NullPtr("system service"));
    }
    Ok(svc)
}

/// A Java `String` (or null) as a Rust string.
fn rust_string(env: &mut JNIEnv, obj: JObject) -> jni::errors::Result<Option<String>> {
    if obj.is_null() {
        return Ok(None);
    }
    let s = JString::from(obj);
    let out: String = env.get_string(&s)?.into();
    Ok(Some(out))
}

/// The class name of the pending exception, clearing it (for messages).
fn take_exception_class(env: &mut JNIEnv) -> Option<String> {
    if !env.exception_check().unwrap_or(false) {
        return None;
    }
    let ex = env.exception_occurred().ok()?;
    let _ = env.exception_clear();
    let cls = env.get_object_class(&ex).ok()?;
    let name = env.call_method(&cls, "getName", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
    let _ = env.exception_clear();
    rust_string(env, name).ok().flatten()
}

fn start_activity(env: &mut JNIEnv, intent: &JObject) -> Result<(), String> {
    let app = app().ok_or_else(|| "The app isn't ready yet.".to_string())?;
    let act = activity_of(&app);
    match env.call_method(&act, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(intent)]) {
        Ok(_) => Ok(()),
        Err(e) => {
            let class = take_exception_class(env);
            log::warn!("android: startActivity failed: {e} ({class:?})");
            if class.as_deref() == Some("android.content.ActivityNotFoundException") {
                Err("No app on this phone can open that.".into())
            } else {
                Err("Couldn't open that on this phone.".into())
            }
        }
    }
}

// ─── Launch links ───────────────────────────────────────────────────────────

/// Read `getIntent()`'s data and consume it (`setData(null)`), so the same link
/// is not handled twice. Returns the link and whether it was consumed.
fn take_intent_link(app: &AndroidApp) -> Option<Option<(String, bool)>> {
    with_env("read launch intent", |env| {
        let act = activity_of(app);
        let intent = env.call_method(&act, "getIntent", "()Landroid/content/Intent;", &[])?.l()?;
        if intent.is_null() {
            return Ok(None);
        }
        let d = env.call_method(&intent, "getDataString", "()Ljava/lang/String;", &[])?.l()?;
        let Some(link) = rust_string(env, d)? else { return Ok(None) };
        let consumed = match env.call_method(&intent, "setData", "(Landroid/net/Uri;)Landroid/content/Intent;", &[JValue::Object(&JObject::null())]) {
            Ok(_) => true,
            Err(e) => {
                let _ = take_exception_class(env);
                log::warn!("android: couldn't consume the launch intent: {e}");
                false
            }
        };
        Ok(Some((link, consumed)))
    })
}

fn is_our_link(s: &str) -> bool {
    s.starts_with("stampede:")
}

/// Queue the link in `getIntent()`, if any, and consume it.
fn poll_launch_links() {
    let Some(app) = app() else { return };
    shared().last_poll = Some(Instant::now());
    let Some(found) = take_intent_link(&app) else { return };
    let mut s = shared();
    let Some((link, consumed)) = found else {
        s.stuck_link = None;
        return;
    };
    if !consumed {
        if s.stuck_link.as_ref() == Some(&link) {
            return;
        }
        s.stuck_link = Some(link.clone());
    }
    if is_our_link(&link) {
        log::info!("android: launch link ({} chars)", link.len());
        s.links.retain(|x| x != &link);
        s.links.push_back(link);
        while s.links.len() > MAX_LINKS {
            s.links.pop_front();
        }
    }
}

// ─── Window: immersive mode and insets (UI thread) ──────────────────────────

fn apply_immersive() {
    let Some(app) = app() else { return };
    let a = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        with_env("immersive mode", |env| {
            let act = activity_of(&a);
            let window = env.call_method(&act, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
            if window.is_null() {
                return Ok(());
            }
            if sdk() >= 30 {
                env.call_method(&window, "setDecorFitsSystemWindows", "(Z)V", &[JValue::Bool(0)])?;
                let ctl = env.call_method(&window, "getInsetsController", "()Landroid/view/WindowInsetsController;", &[])?.l()?;
                if !ctl.is_null() {
                    let bars = env.call_static_method("android/view/WindowInsets$Type", "systemBars", "()I", &[])?.i()?;
                    env.call_method(&ctl, "setSystemBarsBehavior", "(I)V", &[JValue::Int(BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE)])?;
                    env.call_method(&ctl, "hide", "(I)V", &[JValue::Int(bars)])?;
                }
            } else {
                let decor = env.call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?.l()?;
                if !decor.is_null() {
                    env.call_method(&decor, "setSystemUiVisibility", "(I)V", &[JValue::Int(LEGACY_IMMERSIVE)])?;
                }
            }
            Ok(())
        });
    }));
}

fn request_insets() {
    let Some(app) = app() else { return };
    let a = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        let px = with_env("read insets", |env| read_cutout_px(env, &a)).flatten();
        let mut s = shared();
        if let Some(px) = px {
            if s.insets_px != Some(px) {
                log::info!("android: cutout insets px (top, bottom, left, right) {px:?}");
            }
            s.insets_px = Some(px);
        }
    }));
}

/// Display-cutout safe insets (px), or `None` if the view isn't attached yet.
fn read_cutout_px(env: &mut JNIEnv, app: &AndroidApp) -> jni::errors::Result<Option<[i32; 4]>> {
    if sdk() < 28 {
        return Ok(Some([0; 4]));
    }
    let act = activity_of(app);
    let window = env.call_method(&act, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
    if window.is_null() {
        return Ok(None);
    }
    let decor = env.call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?.l()?;
    if decor.is_null() {
        return Ok(None);
    }
    let wi = env.call_method(&decor, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])?.l()?;
    if wi.is_null() {
        return Ok(None);
    }
    if sdk() >= 30 {
        let cut = env.call_static_method("android/view/WindowInsets$Type", "displayCutout", "()I", &[])?.i()?;
        let ins = env.call_method(&wi, "getInsets", "(I)Landroid/graphics/Insets;", &[JValue::Int(cut)])?.l()?;
        let mut v = [0; 4];
        for (i, f) in ["top", "bottom", "left", "right"].iter().enumerate() {
            v[i] = env.get_field(&ins, *f, "I")?.i()?;
        }
        Ok(Some(v))
    } else {
        let dc = env.call_method(&wi, "getDisplayCutout", "()Landroid/view/DisplayCutout;", &[])?.l()?;
        if dc.is_null() {
            return Ok(Some([0; 4]));
        }
        let mut v = [0; 4];
        for (i, m) in ["getSafeInsetTop", "getSafeInsetBottom", "getSafeInsetLeft", "getSafeInsetRight"].iter().enumerate() {
            v[i] = env.call_method(&dc, *m, "()I", &[])?.i()?;
        }
        Ok(Some(v))
    }
}

// ─── The Platform ───────────────────────────────────────────────────────────

/// Android: JNI calls into the framework, all best effort.
#[derive(Default)]
pub struct AndroidPlatform {
    /// `WifiManager.MulticastLock` (not reference counted), created lazily.
    multicast: Option<GlobalRef>,
    multicast_on: bool,
    keep_on: bool,
    room_service: bool,
    tilt_on: bool,
}

impl AndroidPlatform {
    fn multicast_lock(&mut self) -> Option<GlobalRef> {
        if self.multicast.is_none() {
            self.multicast = with_env("create multicast lock", |env| {
                let wifi = system_service(env, "wifi")?;
                let tag = env.new_string("stampede-lan")?;
                let lock = env
                    .call_method(&wifi, "createMulticastLock", "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;", &[JValue::Object(&tag)])?
                    .l()?;
                env.call_method(&lock, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])?;
                env.new_global_ref(lock)
            });
        }
        self.multicast.clone()
    }
}

impl Drop for AndroidPlatform {
    fn drop(&mut self) {
        self.set_tilt_enabled(false);
        if self.multicast_on {
            self.set_multicast_lock(false);
        }
    }
}

impl Platform for AndroidPlatform {
    fn set_orientation(&mut self, orientation: super::os::Orientation) {
        let Some(app) = app() else { return };
        let a = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            with_env("orientation", |env| {
                // ActivityInfo: FULL_USER respects rotation lock; SENSOR_LANDSCAPE permits either side.
                let value = match orientation { super::os::Orientation::Free => 13, super::os::Orientation::Landscape => 6 };
                env.call_method(activity_of(&a), "setRequestedOrientation", "(I)V", &[JValue::Int(value)])?;
                log::info!("android: orientation {orientation:?}");
                Ok(())
            });
        }));
    }

    fn name(&self) -> &'static str {
        "android"
    }

    fn set_tilt_enabled(&mut self, on: bool) {
        if self.tilt_on == on { return; }
        let Some(app) = app() else { return };
        if with_env("tilt sensors", |env| env.call_method(activity_of(&app), "setTiltEnabled", "(Z)V", &[JValue::Bool(on as u8)]).map(|_| ())).is_some() {
            self.tilt_on = on;
        }
    }

    fn tilt_reading(&mut self) -> super::tilt::Reading {
        use super::tilt::Reading;
        let Some(app) = app() else { return Reading::Unavailable };
        with_env("tilt sample", |env| {
            let a = JFloatArray::from(env.call_method(activity_of(&app), "readTilt", "()[F", &[])?.l()?);
            if a.is_null() || env.get_array_length(&a)? != 7 { return Ok(Reading::Unavailable); }
            let mut v = [0.0; 7];
            env.get_float_array_region(&a, 0, &mut v)?;
            Ok(match v[6] as i32 {
                1 => Reading::Waiting,
                2 => Reading::Sample { gravity: [v[0], v[1], v[2]], rotation: v[3] as u8, age: v[4], generation: v[5] as u32 },
                _ => Reading::Unavailable,
            })
        }).unwrap_or(Reading::Unavailable)
    }

    fn open_url(&mut self, url: &str) -> Result<(), String> {
        with_env("open url", |env| {
            let s = env.new_string(url)?;
            let uri = env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&s)])?.l()?;
            let action = env.new_string("android.intent.action.VIEW")?;
            let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;Landroid/net/Uri;)V", &[JValue::Object(&action), JValue::Object(&uri)])?;
            env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(FLAG_ACTIVITY_NEW_TASK)])?;
            Ok(start_activity(env, &intent))
        })
        .unwrap_or_else(|| Err("Couldn't open the link.".into()))
    }

    fn copy_text(&mut self, text: &str) -> Result<(), String> {
        with_env("copy text", |env| {
            let cm = system_service(env, "clipboard")?;
            let label = env.new_string("Stampede")?;
            let t = env.new_string(text)?;
            let clip = env
                .call_static_method(
                    "android/content/ClipData",
                    "newPlainText",
                    "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                    &[JValue::Object(&label), JValue::Object(&t)],
                )?
                .l()?;
            env.call_method(&cm, "setPrimaryClip", "(Landroid/content/ClipData;)V", &[JValue::Object(&clip)])?;
            Ok(())
        })
        .ok_or_else(|| "Couldn't copy.".to_string())
    }

    fn read_clipboard(&mut self) -> Option<String> {
        // Android 10+ answers null unless the app has input focus.
        with_env("read clipboard", |env| {
            let cm = system_service(env, "clipboard")?;
            let clip = env.call_method(&cm, "getPrimaryClip", "()Landroid/content/ClipData;", &[])?.l()?;
            if clip.is_null() || env.call_method(&clip, "getItemCount", "()I", &[])?.i()? < 1 {
                return Ok(None);
            }
            let item = env.call_method(&clip, "getItemAt", "(I)Landroid/content/ClipData$Item;", &[JValue::Int(0)])?.l()?;
            if item.is_null() {
                return Ok(None);
            }
            let cs = env.call_method(&item, "getText", "()Ljava/lang/CharSequence;", &[])?.l()?;
            if cs.is_null() {
                return Ok(None);
            }
            let s = env.call_method(&cs, "toString", "()Ljava/lang/String;", &[])?.l()?;
            rust_string(env, s)
        })
        .flatten()
        .filter(|s| !s.trim().is_empty())
    }

    fn take_launch_link(&mut self) -> Option<String> {
        // Called every frame: re-read getIntent() now and then (see the module docs).
        let due = shared().last_poll.is_none_or(|t| t.elapsed() >= LINK_POLL);
        if due {
            poll_launch_links();
        }
        shared().links.pop_front()
    }

    fn leave_app(&mut self) -> bool {
        // `moveTaskToBack(true)`: the game goes to the background like any
        // other game and the native loop keeps running (it pauses on focus
        // loss). Ending `android_main` instead tears the activity glue down
        // and a relaunch in the same process aborts (Twang Thang device test 2).
        let Some(app) = app() else { return false };
        let moved = with_env("move task to back", |env| {
            let act = activity_of(&app);
            Ok(env.call_method(&act, "moveTaskToBack", "(Z)Z", &[JValue::Bool(1)])?.z()?)
        });
        log::info!("android: leave app, moveTaskToBack -> {moved:?}");
        moved == Some(true)
    }

    fn can_install_apk(&self) -> bool {
        true
    }

    fn own_cert_sha256(&mut self) -> Option<String> {
        super::updater_android::cert_sha256()
    }

    fn apk_start(&mut self, job: &super::os::ApkJob) -> Result<(), String> {
        super::updater_android::start(job)
    }

    fn apk_status(&mut self) -> super::os::ApkStatus {
        super::updater_android::status()
    }

    fn apk_cancel(&mut self) {
        super::updater_android::cancel();
    }

    fn apk_active_package(&mut self) -> Option<String> {
        super::updater_android::active_package()
    }

    fn apk_open_install_settings(&mut self) {
        super::updater_android::open_install_settings();
    }

    fn installed_version_code(&mut self, package: &str) -> Option<u32> {
        super::updater_android::installed_version_code(package)
    }

    fn open_package(&mut self, package: &str) -> bool {
        super::updater_android::launch(package).is_ok()
    }

    fn set_multicast_lock(&mut self, on: bool) {
        if on == self.multicast_on {
            return;
        }
        let Some(lock) = self.multicast_lock() else { return };
        let done = with_env("multicast lock", |env| {
            if on {
                env.call_method(lock.as_obj(), "acquire", "()V", &[])?;
            } else if env.call_method(lock.as_obj(), "isHeld", "()Z", &[])?.z()? {
                env.call_method(lock.as_obj(), "release", "()V", &[])?;
            }
            Ok(())
        });
        if done.is_some() {
            self.multicast_on = on;
            log::info!("android: multicast lock {}", if on { "held" } else { "released" });
        }
    }

    fn set_room_service(&mut self, on: bool) {
        if on == self.room_service {
            return;
        }
        let done = with_env("room service", |env| {
            let ctx = app_context()?;
            let intent = env.new_object("android/content/Intent", "()V", &[])?;
            let pkg = env.new_string("org.decented.stampede")?;
            let cls = env.new_string("org.decented.stampede.RoomService")?;
            env.call_method(&intent, "setClassName", "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&pkg), JValue::Object(&cls)])?;
            if on {
                let m = if sdk() >= 26 { "startForegroundService" } else { "startService" };
                env.call_method(&ctx, m, "(Landroid/content/Intent;)Landroid/content/ComponentName;", &[JValue::Object(&intent)])?;
            } else {
                env.call_method(&ctx, "stopService", "(Landroid/content/Intent;)Z", &[JValue::Object(&intent)])?;
            }
            Ok(())
        });
        if done.is_some() {
            self.room_service = on;
            log::info!("android: room service {}", if on { "started" } else { "stopped" });
        }
    }

    fn set_keep_screen_on(&mut self, on: bool) {
        if on == self.keep_on {
            return;
        }
        self.keep_on = on;
        // Not `AndroidApp::set_window_flags`: it takes android-activity's
        // inner RwLock for writing, while winit's `poll_events` holds the same
        // lock for reading around every callback we run in, so it deadlocked
        // the game thread on the first frame of every match (Twang Thang
        // device test 1, bug 1). `Window.addFlags` on the UI thread instead.
        let Some(app) = app() else { return };
        let a = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            with_env("keep screen on", |env| {
                let act = activity_of(&a);
                let window = env.call_method(&act, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
                if !window.is_null() {
                    let m = if on { "addFlags" } else { "clearFlags" };
                    env.call_method(&window, m, "(I)V", &[JValue::Int(FLAG_KEEP_SCREEN_ON)])?;
                }
                Ok(())
            });
            log::info!("android: keep screen on {}", if on { "set" } else { "cleared" });
        }));
    }
}

// ─── Local play beside a VPN ────────────────────────────────────────────────

// `android_setsocknetwork` (API 23+, libandroid; minSdk is 24): bind a socket to
// one `Network`, so its traffic uses that network's routes even when a VPN owns
// the default route. The handle is `Network.getNetworkHandle()`.
#[link(name = "android")]
extern "C" {
    fn android_setsocknetwork(network: u64, fd: i32) -> i32;
}

/// `NetworkCapabilities` transports and capability used below.
const TRANSPORT_WIFI: i32 = 1;
const TRANSPORT_ETHERNET: i32 = 3;
const TRANSPORT_VPN: i32 = 4;
const NET_CAPABILITY_NOT_VPN: i32 = 15;

/// Ask `ConnectivityManager` for the local network. `None` if JNI fails.
fn local_network() -> Option<LocalNetwork> {
    with_env("local network", |env| {
        let cm = system_service(env, "connectivity")?;
        let arr = env.call_method(&cm, "getAllNetworks", "()[Landroid/net/Network;", &[])?.l()?;
        let arr = JObjectArray::from(arr);
        let n = env.get_array_length(&arr)?;
        let mut seen = Vec::new();
        for i in 0..n {
            let net = env.get_object_array_element(&arr, i)?;
            let caps = env
                .call_method(&cm, "getNetworkCapabilities", "(Landroid/net/Network;)Landroid/net/NetworkCapabilities;", &[JValue::Object(&net)])?
                .l()?;
            if caps.is_null() {
                continue;
            }
            let has = |env: &mut JNIEnv, m: &str, v: i32| -> jni::errors::Result<bool> { env.call_method(&caps, m, "(I)Z", &[JValue::Int(v)])?.z() };
            let wifi = has(env, "hasTransport", TRANSPORT_WIFI)?;
            let eth = has(env, "hasTransport", TRANSPORT_ETHERNET)?;
            let vpn = has(env, "hasTransport", TRANSPORT_VPN)? || !has(env, "hasCapability", NET_CAPABILITY_NOT_VPN)?;
            let handle = env.call_method(&net, "getNetworkHandle", "()J", &[])?.j()?;
            let mut blocked = false;
            if (wifi || eth) && !vpn {
                // NetworkInfo is deprecated but still answers BLOCKED for a lockdown.
                let info = env.call_method(&cm, "getNetworkInfo", "(Landroid/net/Network;)Landroid/net/NetworkInfo;", &[JValue::Object(&net)])?.l()?;
                if !info.is_null() {
                    let st = env.call_method(&info, "getDetailedState", "()Landroid/net/NetworkInfo$DetailedState;", &[])?.l()?;
                    let name = env.call_method(&st, "name", "()Ljava/lang/String;", &[])?.l()?;
                    blocked = rust_string(env, name)?.is_some_and(|s| s == "BLOCKED");
                }
            }
            seen.push(Seen { handle, wifi, ethernet: eth, vpn, blocked });
        }
        Ok(choose_local_network(&seen))
    })
}

/// A VPN network is up: the same check the Nearby notice uses (`local_network`).
/// If the connectivity service cannot be asked, treat it as a VPN (UPnP stays
/// off): the safe answer when unsure.
pub fn vpn_active() -> bool {
    local_network().is_none_or(|n| n.vpn)
}

/// The hook `decented-net` runs on every socket that talks to local peers: bind
/// it to the Wi-Fi (or Ethernet) network so the VPN's default route does not
/// swallow it. See `decented_net::netbind` for what each answer means.
pub fn lan_socket_hook() -> decented_net::LanSocketHook {
    decented_net::LanSocketHook::new(bind_to_local_network)
}

fn bind_to_local_network(fd: i32) -> Result<(), decented_net::LanBindError> {
    use decented_net::LanBindError::{Blocked, Failed};
    let Some(net) = local_network() else { return Err(Failed("could not ask the connectivity service".into())) };
    let Some(handle) = net.handle else { return Err(Failed("no Wi-Fi or Ethernet network (a hotspot?)".into())) };
    if net.blocked && net.vpn {
        return Err(Blocked("the local network is blocked while the VPN is on".into()));
    }
    // SAFETY: plain FFI call on a descriptor the caller owns; it only changes the socket's network.
    let rc = unsafe { android_setsocknetwork(handle as u64, fd) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if net.vpn && decented_net::netbind::is_lockdown_error(&err) {
        Err(Blocked(format!("binding to the Wi-Fi network was refused: {err}")))
    } else {
        Err(Failed(format!("android_setsocknetwork: {err}")))
    }
}
