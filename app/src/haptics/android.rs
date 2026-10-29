//! Android haptics through JNI (`jni` + `ndk-context`, see
//! `platform::android::with_env`):
//!
//! 1. The Application context's `getSystemService("vibrator")` →
//!    `android.os.Vibrator` (cached as a global ref; `hasVibrator()` checked).
//! 2. API ≥ 26: `VibrationEffect.createOneShot(ms, amplitude)` +
//!    `vibrator.vibrate(effect)`; older: `vibrator.vibrate(ms)`.
//! 3. On any JNI error `failed` is set and haptics stay off for the session
//!    (silently). Needs the `VIBRATE` permission (manifest).

use super::Haptics;
use crate::platform::android::{system_service, with_env};
use jni::objects::{GlobalRef, JValue};

#[derive(Debug, Default)]
pub struct AndroidHaptics {
    on: bool,
    failed: bool,
    vibrator: Option<GlobalRef>,
}

impl AndroidHaptics {
    pub fn new() -> Self {
        Self::default()
    }

    fn vibrator(&mut self) -> Option<GlobalRef> {
        if self.vibrator.is_none() {
            let v = with_env("get vibrator", |env| {
                let v = system_service(env, "vibrator")?;
                if !env.call_method(&v, "hasVibrator", "()Z", &[])?.z()? {
                    return Ok(None);
                }
                env.new_global_ref(v).map(Some)
            })
            .flatten();
            if v.is_none() {
                log::info!("haptics: no vibrator; haptics off");
                self.failed = true;
            }
            self.vibrator = v;
        }
        self.vibrator.clone()
    }
}

impl Haptics for AndroidHaptics {
    fn vibrate(&mut self, ms: u32, amplitude: u8) {
        if self.failed || ms == 0 {
            return;
        }
        let Some(vib) = self.vibrator() else { return };
        let modern = android_activity::AndroidApp::sdk_version() >= 26;
        // 1..=255; 0 would mean "default amplitude" (-1), which we never want.
        let amp = i32::from(amplitude.max(1));
        let ok = with_env("vibrate", |env| {
            if modern {
                let effect = env
                    .call_static_method(
                        "android/os/VibrationEffect",
                        "createOneShot",
                        "(JI)Landroid/os/VibrationEffect;",
                        &[JValue::Long(ms as i64), JValue::Int(amp)],
                    )?
                    .l()?;
                env.call_method(vib.as_obj(), "vibrate", "(Landroid/os/VibrationEffect;)V", &[JValue::Object(&effect)])?;
            } else {
                env.call_method(vib.as_obj(), "vibrate", "(J)V", &[JValue::Long(ms as i64)])?;
            }
            Ok(())
        });
        if ok.is_none() {
            log::info!("haptics: vibrate failed; haptics off");
            self.failed = true;
        }
    }
    fn set_enabled(&mut self, on: bool) {
        self.on = on;
    }
    fn enabled(&self) -> bool {
        self.on && !self.failed
    }
}
