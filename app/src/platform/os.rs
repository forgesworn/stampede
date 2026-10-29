//! OS services the game needs beyond a window: the clipboard (room codes),
//! launch links (`stampede://join#CODE`), the screen wake lock, the Wi-Fi
//! multicast lock (Nearby play) and leaving the app. Best effort everywhere:
//! a failure is a message or a no-op, never a crash.

use std::collections::{HashMap, VecDeque};

/// How an APK download and install (in-app updates, `update.rs`; More games,
/// `games.rs`) stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApkStatus {
    /// Nothing started (or the last attempt was reset).
    Idle,
    /// Bytes arriving: `total` is the size the release event announced.
    Downloading { done: u64, total: u64 },
    /// Verified; handing the file to the system installer.
    Installing,
    /// Android's own install prompt is up: the player must tap it.
    WaitingForUser,
    /// "Install unknown apps" is off for this app; the settings page was opened.
    NeedsPermission,
    /// Refused or failed, with a message for the player.
    Failed(String),
    /// The system reported success (an updated app is normally replaced before this is seen).
    Installed,
}

impl ApkStatus {
    /// A download or install is under way (poll again soon).
    pub fn active(&self) -> bool {
        matches!(self, ApkStatus::Downloading { .. } | ApkStatus::Installing | ApkStatus::WaitingForUser)
    }
}

/// What a start (or the follower) is told while the one installer is busy with another package's attempt.
pub const ANOTHER_INSTALL: &str = "Another install is in progress. Try again when it is done.";

/// The platform's current install attempt belongs to a package other than `mine`: its status,
/// progress and Android prompt are not `mine`'s to show, and polling `apk_status` could even
/// launch that other attempt's parked prompt.
pub fn held_by_another(platform: &mut dyn Platform, mine: &str) -> bool {
    platform.apk_active_package().is_some_and(|p| p != mine)
}

/// One APK to download, verify and install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApkJob {
    /// Canonical Blossom addresses, tried in order.
    pub urls: Vec<String>,
    /// SHA-256 of the file (lower-case hex) and its exact size, from the signed release event.
    pub sha256: String,
    pub size: u64,
    /// The package the file must be. Empty = this app (an update).
    pub package: String,
    /// The SHA-256 (lower-case hex) of the signing certificate the file must carry:
    /// the one pinned in the catalog (or this app's own, for an update).
    pub cert: String,
    /// The certificate the signed release event names. Java checks the file against
    /// this one as well, independently of `cert` (a game needs both equal to the file's).
    pub release_cert: String,
}

/// The orientations allowed by the current screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    /// Follow the user's rotation setting, including portrait.
    Free,
    /// Driving and the designer use either landscape orientation.
    Landscape,
}

/// OS services. One instance, owned by the engine (`Ctx::platform`).
pub trait Platform {
    fn name(&self) -> &'static str;

    fn set_orientation(&mut self, _orientation: Orientation) {}

    /// Motion sensors are active only on the foreground race / tilt setup screen.
    fn set_tilt_enabled(&mut self, _on: bool) {}
    fn tilt_reading(&mut self) -> super::tilt::Reading { super::tilt::Reading::Unavailable }

    /// Open a link in the system browser.
    fn open_url(&mut self, url: &str) -> Result<(), String>;

    /// Put `text` on the clipboard (a room code). Android: `setPrimaryClip`.
    fn copy_text(&mut self, text: &str) -> Result<(), String>;

    /// The clipboard's text, if any (paste a room code). Android 10+ answers
    /// nothing unless the app has input focus.
    fn read_clipboard(&mut self) -> Option<String>;

    /// A launch link the OS handed us (a `stampede:` URL), at most one per call.
    /// The engine turns `stampede://join#CODE` into `Ctx::join_code`.
    fn take_launch_link(&mut self) -> Option<String>;

    /// Hold the Wi-Fi multicast lock while Nearby play listens (Android only).
    fn set_multicast_lock(&mut self, on: bool);

    /// Keep the screen on. The engine drives this from `Screen::keep_awake`.
    fn set_keep_screen_on(&mut self, on: bool);

    /// Keep the process in use while the player is in an online room (Android:
    /// a silent foreground service, so the network survives a switch to another
    /// app). The engine turns it on when a session exists and off when it ends.
    fn set_room_service(&mut self, _on: bool) {}

    /// This platform can install an APK itself (Android), so the launch-time
    /// update check is worth making. The Settings button works anywhere.
    fn can_install_apk(&self) -> bool {
        false
    }

    /// SHA-256 (lowercase hex) of the certificate this app is signed with, for
    /// the update's `cert` check. Android: `PackageManager` signing info.
    fn own_cert_sha256(&mut self) -> Option<String> {
        None
    }

    /// Download the APK (the first URL that works), check it is exactly `size`
    /// bytes with SHA-256 `sha256`, check its package and signing certificate
    /// against the job, and hand it to the system installer. Returns at once;
    /// follow it with [`Platform::apk_status`]. Android: `Updater.start` /
    /// `Updater.startGame`. `Err` when it could not start, in particular
    /// [`ANOTHER_INSTALL`] when an attempt for another package is in flight (the
    /// platform has ONE installer; nothing was changed).
    fn apk_start(&mut self, _job: &ApkJob) -> Result<(), String> {
        Err("Installs run on Android only.".into())
    }

    /// Where the download / install stands now.
    fn apk_status(&mut self) -> ApkStatus {
        ApkStatus::Idle
    }

    /// Stop the current install attempt (the status then turns to Failed): a running
    /// download, the copy into the installer, or an attempt parked at Android's prompt
    /// (its session is abandoned, so no prompt appears later). Android: `Updater.cancel`.
    /// It stops whatever attempt is current, whoever started it: check
    /// [`Platform::apk_active_package`] first.
    fn apk_cancel(&mut self) {}

    /// The package the current (or most recent) install attempt is for (Stampede's own,
    /// or a catalog game's); `None` before any. The platform runs ONE installer, so the
    /// follower believes [`Platform::apk_status`] only while this is its own package.
    /// Android: `Updater.activePkg`.
    fn apk_active_package(&mut self) -> Option<String> {
        None
    }

    /// Open the "Install unknown apps" page for this app (Android).
    fn apk_open_install_settings(&mut self) {}

    /// The versionCode of an installed package, `None` if it is not installed
    /// (or cannot be asked). Android: `PackageManager.getPackageInfo`; the
    /// manifest lists each catalog package in `<queries>`.
    fn installed_version_code(&mut self, _package: &str) -> Option<u32> {
        None
    }

    /// Open an installed package's launcher activity. False if it cannot be opened.
    fn open_package(&mut self, _package: &str) -> bool {
        false
    }

    /// Back on the first screen: leave the app the platform's own way.
    /// True if the app was sent away and the loop should go on (Android:
    /// to the background); false if the loop should end (desktop).
    fn leave_app(&mut self) -> bool {
        false
    }

    /// Records calls (tests and `--shot`); `None` on real platforms.
    fn recorded(&self) -> Option<&RecordingPlatform> {
        None
    }
}

/// The platform for this build. `launch_link` is the desktop stand-in for an
/// Android intent (`--join CODE`).
pub fn platform(launch_link: Option<String>) -> Box<dyn Platform> {
    #[cfg(target_os = "android")]
    {
        let _ = launch_link;
        Box::new(super::android::AndroidPlatform::default())
    }
    #[cfg(not(target_os = "android"))]
    {
        Box::new(DesktopPlatform { launch_link })
    }
}

// ─── Desktop ────────────────────────────────────────────────────────────────

/// Linux desktop: external tools, never a hard dependency.
#[cfg(not(target_os = "android"))]
pub struct DesktopPlatform {
    launch_link: Option<String>,
}

#[cfg(not(target_os = "android"))]
fn pipe_to(cmd: &str, args: &[&str], text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| format!("{cmd}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).map_err(|e| format!("{cmd}: {e}"))?;
    }
    // wl-copy / xclip fork to keep serving the selection; don't wait on them.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(not(target_os = "android"))]
impl Platform for DesktopPlatform {
    fn name(&self) -> &'static str {
        "desktop"
    }

    fn open_url(&mut self, url: &str) -> Result<(), String> {
        let mut child = std::process::Command::new("xdg-open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("couldn't open a browser ({e})"))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn copy_text(&mut self, text: &str) -> Result<(), String> {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let tries: [(&str, &[&str]); 3] = if wayland {
            [("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
        } else {
            [("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"]), ("wl-copy", &[])]
        };
        let mut last = String::from("no clipboard tool");
        for (cmd, args) in tries {
            match pipe_to(cmd, args, text) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn read_clipboard(&mut self) -> Option<String> {
        let tries: [(&str, &[&str]); 3] = [("wl-paste", &["--no-newline"]), ("xclip", &["-selection", "clipboard", "-o"]), ("xsel", &["--clipboard", "--output"])];
        for (cmd, args) in tries {
            if let Ok(out) = std::process::Command::new(cmd).args(args).stderr(std::process::Stdio::null()).output() {
                if out.status.success() {
                    return String::from_utf8(out.stdout).ok().filter(|s| !s.trim().is_empty());
                }
            }
        }
        None
    }

    fn take_launch_link(&mut self) -> Option<String> {
        self.launch_link.take()
    }

    fn set_multicast_lock(&mut self, _on: bool) {}

    fn set_keep_screen_on(&mut self, _on: bool) {}
}

// ─── Tests and screenshots ──────────────────────────────────────────────────

/// Records every call (tests, headless screenshots). Never touches the OS.
#[derive(Debug, Default)]
pub struct RecordingPlatform {
    pub tilt: super::tilt::Reading,
    pub tilt_enabled: bool,
    pub orientations: Vec<Orientation>,
    pub opened: Vec<String>,
    pub copied: Vec<String>,
    pub clipboard: Option<String>,
    pub launch: VecDeque<String>,
    pub multicast: bool,
    pub keep_on: bool,
    /// How many times the app was asked to leave (Back on the first screen).
    pub left: u32,
    /// What `leave_app` answers.
    pub leave_goes_on: bool,
    /// The certificate `own_cert_sha256` answers with.
    pub cert: Option<String>,
    /// What `apk_start` was asked.
    pub apk_started: Vec<ApkJob>,
    /// What `apk_status` answers (`None` = Idle).
    pub apk: Option<ApkStatus>,
    /// How many times the install-settings page was opened.
    pub install_settings_opened: u32,
    /// How many times a download cancel was asked for.
    pub apk_cancels: u32,
    /// The package of the current (or most recent) attempt: what `apk_active_package` answers.
    /// Set by `apk_start`, which refuses (as Java does) while `apk` is active for another package.
    pub apk_active: Option<String>,
    /// Installed packages and their versionCodes.
    pub installed_packages: HashMap<String, u32>,
    /// Packages `open_package` was asked to open.
    pub opened_packages: Vec<String>,
}

impl Platform for RecordingPlatform {
    fn set_orientation(&mut self, orientation: Orientation) { self.orientations.push(orientation); }
    fn set_tilt_enabled(&mut self, on: bool) { self.tilt_enabled = on; }
    fn tilt_reading(&mut self) -> super::tilt::Reading { self.tilt }
    fn name(&self) -> &'static str {
        "recording"
    }

    fn open_url(&mut self, url: &str) -> Result<(), String> {
        self.opened.push(url.to_string());
        Ok(())
    }

    fn copy_text(&mut self, text: &str) -> Result<(), String> {
        self.copied.push(text.to_string());
        self.clipboard = Some(text.to_string());
        Ok(())
    }

    fn read_clipboard(&mut self) -> Option<String> {
        self.clipboard.clone()
    }

    fn take_launch_link(&mut self) -> Option<String> {
        self.launch.pop_front()
    }

    fn set_multicast_lock(&mut self, on: bool) {
        self.multicast = on;
    }

    fn set_keep_screen_on(&mut self, on: bool) {
        self.keep_on = on;
    }

    fn leave_app(&mut self) -> bool {
        self.left += 1;
        self.leave_goes_on
    }

    fn can_install_apk(&self) -> bool {
        true
    }

    fn own_cert_sha256(&mut self) -> Option<String> {
        self.cert.clone()
    }

    fn apk_start(&mut self, job: &ApkJob) -> Result<(), String> {
        // What Java does at the top of a start: refuse while an attempt for another
        // package is in flight, otherwise this package becomes the current attempt.
        let pkg = if job.package.is_empty() { crate::games::OWN_PACKAGE } else { job.package.as_str() };
        let in_flight = self.apk.as_ref().is_some_and(ApkStatus::active);
        if in_flight && self.apk_active.as_deref() != Some(pkg) {
            return Err(ANOTHER_INSTALL.into());
        }
        self.apk_active = Some(pkg.to_string());
        self.apk_started.push(job.clone());
        Ok(())
    }

    fn apk_active_package(&mut self) -> Option<String> {
        self.apk_active.clone()
    }

    fn apk_status(&mut self) -> ApkStatus {
        self.apk.clone().unwrap_or(ApkStatus::Idle)
    }

    fn apk_open_install_settings(&mut self) {
        self.install_settings_opened += 1;
    }

    fn apk_cancel(&mut self) {
        self.apk_cancels += 1;
    }

    fn installed_version_code(&mut self, package: &str) -> Option<u32> {
        self.installed_packages.get(package).copied()
    }

    fn open_package(&mut self, package: &str) -> bool {
        self.opened_packages.push(package.to_string());
        self.installed_packages.contains_key(package)
    }

    fn recorded(&self) -> Option<&RecordingPlatform> {
        Some(self)
    }
}
