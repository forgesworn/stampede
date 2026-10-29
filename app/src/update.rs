//! In-app updates over nostr (spec 20), ported from Twang Thang's `update.rs`.
//!
//! A release is a signed kind-30063 event from one pinned release key
//! ([`RELEASE_PUBKEY_HEX`]), found on three public relays. The event names the
//! APK by SHA-256 and size, says which certificate signed it, and lists where
//! to download it (canonical Blossom URLs only). The app:
//!
//! 1. reads the events from the player's relays (in their order) and the
//!    release relays (relay.trotters.cc, relay.damus.io, nos.lol), all at
//!    once, 8 s, at launch if the last check is 6 h old, or when the
//!    player taps "Check for updates" (never while [`RELEASE_PUBKEY_HEX`] is
//!    `None`: no key pinned, no check, no network);
//! 2. keeps only events with a valid id and signature from the pinned key and
//!    well-formed tags ([`parse_release`]), picks the highest `version_code`
//!    ([`pick_latest`]) and offers it only if that is above the installed
//!    versionCode and the `cert` is this app's own ([`judge`]);
//! 3. on a tap, has the platform download it (streaming SHA-256, abort past
//!    `size` + 1 MiB, refuse on any mismatch) and hand it to Android's
//!    installer. The install is always a tap in Android's own prompt.
//!
//! The real trust chain: the pinned key signs `x`; the bytes must hash to `x`;
//! Android itself refuses an update signed with a different certificate.
//! Nothing here installs silently. `tools/release/publish-release.mjs` is the
//! publishing half.
//!
//! The same download, verify and install path also installs ANOTHER package from
//! the More games catalog ([`Updater::install_game`], `games.rs`): the Java side
//! then checks the downloaded file's package and signing certificate against
//! the catalog before it commits the installer session.

use crate::platform::os::{held_by_another, ApkJob, ApkStatus, Platform, ANOTHER_INSTALL};
use nostr::{Event, Filter, Kind, PublicKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Zapstore's "software release" kind number, used with our own tag set.
pub const RELEASE_KIND: u16 = 30063;
/// The `d` tag of this app's Android channel.
pub const RELEASE_CHANNEL: &str = "stampede-android-apk";
/// Read for the release event; also what `publish-release.mjs` writes to.
pub const RELEASE_RELAYS: [&str; 3] = ["wss://relay.trotters.cc", "wss://relay.damus.io", "wss://nos.lol"];
/// The release key's public half (npub1ka2v5p8ansyaz7uwzgu5aan4g3r4qfdug34vpe2jvavgcgcj3z5qqa984j),
/// Stampede's own (one release key per game). `None` would disable update checks.
/// Rotating it later means shipping a new APK by hand: there is no in-band
/// rotation. `publish-release.mjs` refuses to sign with any other key.
pub const RELEASE_PUBKEY_HEX: Option<&str> = Some("b754ca04fd9c09d17b8e12394ef67544475025bc446ac0e55267588c231288a8");
/// The largest APK an announcement may name (a signed `size` above this is refused
/// whoever signed it: it would fill the cache partition). `publish-release.mjs`
/// holds the same cap.
pub const MAX_APK_BYTES: u64 = 256 * 1024 * 1024;
/// Shown in Settings while [`RELEASE_PUBKEY_HEX`] is `None`.
pub const NOT_SET_UP: &str = "Updates aren't set up for this build yet";
/// How long the relays get to answer.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// The launch-time check runs at most this often.
pub const CHECK_INTERVAL_SECS: u64 = 6 * 3600;
/// How often the platform's download status is read while an install is under way.
const STATUS_POLL_SECS: f32 = 0.25;
/// The file next to `profile.json`: last check time and the best verified release event.
const FILE: &str = "update.json";

// ─── Versions ───────────────────────────────────────────────────────────────

/// Android versionCode of a `major.minor.patch` version, `major * 1e6 +
/// minor * 1e3 + patch`, exactly what `tools/build-apk.sh` stamps into the APK. `None` when
/// a part is missing, not a number, or overflows the scheme.
pub fn version_code_of(version: &str) -> Option<u32> {
    let mut it = version.split('.');
    let major: u32 = it.next()?.parse().ok()?;
    let minor: u32 = it.next().unwrap_or("0").parse().ok()?;
    let patch: u32 = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() || minor >= 1000 || patch >= 1000 || major > 2000 {
        return None;
    }
    Some(major * 1_000_000 + minor * 1_000 + patch)
}

/// This build's versionCode (the workspace version, as `build-apk.sh` derives it).
pub fn installed_version_code() -> u32 {
    version_code_of(env!("CARGO_PKG_VERSION")).unwrap_or(0)
}

// ─── The release event ──────────────────────────────────────────────────────

/// A verified release announcement.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub version_code: u32,
    /// APK SHA-256, lowercase hex.
    pub sha256: String,
    pub size: u64,
    /// SHA-256 of the APK's signing certificate, lowercase hex.
    pub cert: String,
    pub urls: Vec<String>,
    pub notes: String,
    pub created_at: u64,
    event: Event,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_decimal(s: &str) -> bool {
    !s.is_empty() && s.len() <= 18 && s.bytes().all(|b| b.is_ascii_digit())
}

/// The value of the first tag called `name`.
fn tag_value<'a>(ev: &'a Event, name: &str) -> Option<&'a str> {
    ev.tags.iter().find_map(|t| {
        let s = t.as_slice();
        (s.len() >= 2 && s[0] == name).then(|| s[1].as_str())
    })
}

/// Is `url` the blob's canonical Blossom address for `sha256`:
/// `https://host/<sha>` or `https://host/<sha>.<ext>` (BUD-01), with no query,
/// fragment or credentials? That is the one address a Blossom server promises
/// to keep serving; a CDN redirect target is not. Enforced here and in
/// `release-helpers.mjs`.
pub fn is_canonical_blossom_url(url: &str, sha256: &str) -> bool {
    if !is_hex64(sha256) {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => return false,
    };
    // Host (and an optional :port): letters, digits, dots, hyphens. No '@' (credentials), no spaces.
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
        return false;
    }
    if port.is_some_and(|p| p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    // Path is exactly "/<sha>" or "/<sha>.<1-8 alphanumerics>": no query, fragment or extra segments.
    let Some(name) = path.strip_prefix('/') else { return false };
    let (hash, ext) = match name.split_once('.') {
        Some((h, e)) => (h, Some(e)),
        None => (name, None),
    };
    hash == sha256 && ext.is_none_or(|e| (1..=8).contains(&e.len()) && e.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Verify and read one event, in this order: kind, `d` tag, id and signature,
/// signer == the pinned key, then strict tag shapes. Anything off is `None`
/// (the update check treats that as "no news", never as "behind").
pub fn parse_release(ev: &Event, pinned: &PublicKey, channel: &str) -> Option<Release> {
    if ev.kind != Kind::Custom(RELEASE_KIND) || tag_value(ev, "d") != Some(channel) {
        return None;
    }
    ev.verify().ok()?;
    if ev.pubkey != *pinned {
        return None;
    }
    let version = tag_value(ev, "version")?;
    if version.is_empty() || version.len() > 32 || version.chars().any(char::is_control) {
        return None;
    }
    let code = tag_value(ev, "version_code")?;
    if !is_decimal(code) {
        return None;
    }
    let version_code: u32 = code.parse().ok().filter(|c| *c > 0)?;
    let sha256 = tag_value(ev, "x")?;
    if !is_hex64(sha256) {
        return None;
    }
    let size = tag_value(ev, "size")?;
    if !is_decimal(size) {
        return None;
    }
    let size: u64 = size.parse().ok().filter(|s| (1..=MAX_APK_BYTES).contains(s))?;
    let cert = tag_value(ev, "cert")?;
    if !is_hex64(cert) {
        return None;
    }
    let urls: Vec<String> = ev
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            (s.len() >= 2 && s[0] == "url").then(|| s[1].clone())
        })
        .collect();
    if urls.is_empty() || urls.iter().any(|u| !is_canonical_blossom_url(u, sha256)) {
        return None;
    }
    Some(Release {
        version: version.to_string(),
        version_code,
        sha256: sha256.to_string(),
        size,
        cert: cert.to_string(),
        urls,
        notes: ev.content.clone(),
        created_at: ev.created_at.as_secs(),
        event: ev.clone(),
    })
}

/// The best verified announcement among `events`: the highest `version_code`
/// (a later `created_at` breaks a tie).
pub fn pick_latest(events: &[Event], pinned: &PublicKey, channel: &str) -> Option<Release> {
    events.iter().filter_map(|e| parse_release(e, pinned, channel)).max_by_key(|r| (r.version_code, r.created_at))
}

/// What the newest verified release means for this install.
#[derive(Clone, Debug)]
pub enum Verdict {
    /// Nothing verified to compare against (never checked, or no news).
    Unknown,
    /// The newest release is not above what is installed.
    UpToDate,
    /// A newer release, signed with this app's certificate: offer it.
    Offer(Release),
    /// A newer release, but signed with a different certificate: Android would
    /// refuse it, so it is not offered.
    DifferentKey(Release),
}

/// Decide on `latest`: offered only if its `version_code` is above
/// `installed` and its `cert` equals `own_cert`. An unknown own cert (no
/// platform support) skips the early check; Android's install-time signature
/// check is the one that counts.
pub fn judge(latest: Option<Release>, installed: u32, own_cert: Option<&str>) -> Verdict {
    match latest {
        None => Verdict::Unknown,
        Some(r) if r.version_code <= installed => Verdict::UpToDate,
        Some(r) if own_cert.is_some_and(|c| c != r.cert) => Verdict::DifferentKey(r),
        Some(r) => Verdict::Offer(r),
    }
}

/// The relay query for the channel's events from the pinned key.
pub fn release_filter(pinned: &PublicKey) -> Filter {
    Filter::new().kind(Kind::Custom(RELEASE_KIND)).author(*pinned).identifier(RELEASE_CHANNEL).limit(10)
}

/// The relay query for several channels at once, each from its own key (More
/// games): authors = every key, `#d` = every channel. A relay may return an
/// event of key A on channel B; [`parse_release`] with the entry's own key and
/// channel is what decides.
pub fn catalog_filter(keys: &[PublicKey], channels: &[&str]) -> Filter {
    Filter::new().kind(Kind::Custom(RELEASE_KIND)).authors(keys.iter().copied()).identifiers(channels.iter().copied()).limit(50)
}

// ─── Saved state ────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    /// Unix seconds of the last check that heard from a relay.
    last_check: u64,
    /// The best verified release event of that check. Kept as the signed
    /// event and verified again on load, so a file edit buys nothing.
    event: Option<Event>,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ─── The updater ────────────────────────────────────────────────────────────

/// How bad a line of update status is (the screens colour it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateTone {
    Dim,
    Accent,
    Gold,
    Danger,
}

/// Everything a screen shows about updates.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateView {
    /// "v0.1.0".
    pub installed: String,
    /// One line: where things stand.
    pub status: String,
    pub tone: UpdateTone,
    /// A release is on offer (or its download is running): "Update to v0.2.0".
    pub offer: Option<OfferView>,
    /// A relay check is running.
    pub checking: bool,
    /// Download progress 0..=1 while bytes arrive.
    pub progress: Option<f32>,
    /// The Check button is live.
    pub can_check: bool,
    /// The Update button is live (offered, and no download under way).
    pub can_install: bool,
    /// Bytes are arriving: the button reads "Cancel download" instead.
    pub downloading: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferView {
    pub version: String,
    /// "27.7 MB".
    pub size: String,
    /// The release notes, one paragraph, trimmed.
    pub notes: String,
}

/// Screenshots only: a release as the parser would return it, announced by a
/// throwaway key that nothing else trusts.
pub fn demo_release(channel: &str, version: &str, size: u64, cert: &str) -> Release {
    let keys = nostr::Keys::generate();
    let sha = "d".repeat(64);
    let tag = |parts: &[&str]| nostr::Tag::parse(parts.iter().copied()).expect("valid tag");
    let tags = vec![
        tag(&["d", channel]),
        tag(&["version", version]),
        tag(&["version_code", "2000"]),
        tag(&["x", &sha]),
        tag(&["size", &size.to_string()]),
        tag(&["cert", cert]),
        tag(&["url", &format!("https://nostr.download/{sha}.apk")]),
    ];
    let ev = nostr::EventBuilder::new(Kind::Custom(RELEASE_KIND), "").tags(tags).sign_with_keys(&keys).expect("signs");
    parse_release(&ev, &keys.public_key(), channel).expect("a well-formed demo release")
}

/// What the download / install in progress (or the last one) is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An update of this app.
    SelfApp,
    /// Another game from the More games catalog, by package name.
    Game(String),
}

pub struct Updater {
    path: Option<PathBuf>,
    stored: Stored,
    /// The pinned release key; `None` while Stampede has no release key
    /// ([`RELEASE_PUBKEY_HEX`]): nothing is checked then.
    pinned: Option<PublicKey>,
    /// The versionCode to compare with (tests set it).
    pub installed: u32,
    /// Allowed to touch the network (off for fake networks and screenshots).
    pub enabled: bool,
    started: bool,
    own_cert: Option<String>,
    verdict: Verdict,
    task: Option<crate::nostr_fetch::Task<crate::nostr_fetch::FetchReport>>,
    /// The last finished check saw nothing valid.
    no_news: bool,
    /// ...and no relay answered at all (as opposed to answering with nothing).
    unreached: bool,
    install: ApkStatus,
    target: Target,
    /// Screenshots: the staged install state is not polled away.
    frozen: bool,
    poll_left: f32,
    /// The player's own relays (Settings), asked on top of [`RELEASE_RELAYS`].
    user_relays: Vec<String>,
}

/// Where to look for a release: the player's own relays in their order, then the
/// pinned release relays they did not list, each once (by normalised address). A
/// relay can only carry the event, never forge it: the pinned release key signs it.
pub fn release_relays(user: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for u in user.iter().cloned().chain(RELEASE_RELAYS.iter().map(|s| s.to_string())) {
        let key = decented_relays::normalize_relay_url(&u);
        if !seen.contains(&key) {
            seen.push(key);
            out.push(u);
        }
    }
    out
}

impl Updater {
    /// Load the saved state from `dir` (a missing or corrupt file is a fresh start).
    pub fn open(dir: &Path) -> Updater {
        let path = dir.join(FILE);
        let stored = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Stored>(&b).ok()).unwrap_or_default();
        let mut u = Updater::in_memory();
        u.path = Some(path);
        u.stored = stored;
        u
    }

    /// No file, no network (tests, screenshots).
    pub fn in_memory() -> Updater {
        Updater {
            path: None,
            stored: Stored::default(),
            pinned: RELEASE_PUBKEY_HEX.map(|h| PublicKey::from_hex(h).expect("RELEASE_PUBKEY_HEX is a valid x-only key")),
            installed: installed_version_code(),
            enabled: false,
            started: false,
            own_cert: None,
            verdict: Verdict::Unknown,
            task: None,
            no_news: false,
            unreached: false,
            install: ApkStatus::Idle,
            target: Target::SelfApp,
            frozen: false,
            poll_left: 0.0,
            user_relays: Vec::new(),
        }
    }

    /// Is a release key pinned (so that checks can run)?
    pub fn configured(&self) -> bool {
        self.pinned.is_some()
    }

    /// The player's relay list changed (Settings): later checks ask these too.
    pub fn set_user_relays(&mut self, relays: Vec<String>) {
        self.user_relays = relays;
    }

    /// The relays the next check asks.
    pub fn check_relays(&self) -> Vec<String> {
        release_relays(&self.user_relays)
    }

    /// Tests: trust another key.
    #[cfg(test)]
    pub fn trust(&mut self, key: PublicKey) {
        self.pinned = Some(key);
    }

    /// Screenshots and demos: a newer release is out, announced by a throwaway
    /// key that is trusted for this run only. It refuses a file-backed
    /// `Updater`: a throwaway-signed event must never reach `update.json`.
    pub fn demo_offer(&mut self, version: &str, notes: &str) {
        if self.path.is_some() {
            log::warn!("demo_offer ignored on a file-backed Updater");
            return;
        }
        let keys = nostr::Keys::generate();
        self.pinned = Some(keys.public_key());
        let sha = "d".repeat(64);
        let tag = |parts: &[&str]| nostr::Tag::parse(parts.iter().copied()).expect("valid tag");
        let tags = vec![
            tag(&["d", RELEASE_CHANNEL]),
            tag(&["version", version]),
            tag(&["version_code", &(self.installed + 1000).to_string()]),
            tag(&["x", &sha]),
            tag(&["size", "27700000"]),
            tag(&["cert", &"e".repeat(64)]),
            tag(&["url", &format!("https://nostr.download/{sha}.apk")]),
        ];
        let ev = nostr::EventBuilder::new(Kind::Custom(RELEASE_KIND), notes).tags(tags).sign_with_keys(&keys).expect("signs");
        self.apply_events(&[ev], unix_now());
    }

    /// Screenshots: pretend the running download is for `package` and in this state.
    pub fn demo_game_state(&mut self, package: &str, st: ApkStatus) {
        if self.path.is_none() {
            self.install = st;
            self.target = Target::Game(package.to_string());
            self.frozen = true;
        }
    }

    /// Screenshots: pretend the install of the offer is in this state.
    pub fn demo_install_state(&mut self, st: ApkStatus) {
        if self.path.is_none() {
            self.install = st;
            self.target = Target::SelfApp;
            self.frozen = true;
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let r = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| serde_json::to_vec(&self.stored).map_err(std::io::Error::other))
            .and_then(|b| std::fs::write(path, b));
        if let Err(e) = r {
            log::warn!("could not save update state: {e}");
        }
    }

    /// Is the launch-time check due at `now`?
    pub fn due(&self, now: u64) -> bool {
        // A last_check in the future (the clock was wrong when it was taken) counts as due.
        self.stored.last_check == 0 || now < self.stored.last_check || now - self.stored.last_check >= CHECK_INTERVAL_SECS
    }

    fn rejudge(&mut self) {
        let latest = self.pinned.as_ref().and_then(|k| self.stored.event.as_ref().and_then(|e| parse_release(e, k, RELEASE_CHANNEL)));
        self.verdict = judge(latest, self.installed, self.own_cert.as_deref());
    }

    /// Frame hook: the first call settles the saved verdict and, on a platform
    /// that can install and when the last check is 6 h old, starts the launch
    /// check; later calls collect a finished check and follow an install.
    pub fn tick(&mut self, dt: f32, platform: &mut dyn Platform) {
        if !self.started {
            self.started = true;
            self.own_cert = platform.own_cert_sha256();
            self.rejudge();
            if self.enabled && self.configured() && platform.can_install_apk() && self.due(unix_now()) {
                self.begin_check();
            }
        }
        if let Some(report) = self.task.as_ref().and_then(|t| t.poll()) {
            self.task = None;
            self.apply_report(&report, unix_now());
        }
        if self.install.active() && !self.frozen {
            self.poll_left -= dt;
            if self.poll_left <= 0.0 {
                self.poll_left = STATUS_POLL_SECS;
                if held_by_another(platform, &self.target_package()) {
                    // The one installer is on another package's attempt: its progress and
                    // prompt are not this one's to show (and polling could launch its prompt).
                    self.install = ApkStatus::Failed(ANOTHER_INSTALL.into());
                } else {
                    let st = platform.apk_status();
                    self.follow(st, platform);
                }
            }
        }
    }

    /// Check now ("Check for updates"): unthrottled. Does nothing while no key is pinned.
    pub fn check_now(&mut self) {
        self.begin_check();
    }

    fn begin_check(&mut self) {
        let Some(pinned) = self.pinned else { return };
        if self.task.is_some() {
            return;
        }
        let urls = self.check_relays();
        self.task = Some(crate::nostr_fetch::fetch_live(urls, vec![release_filter(&pinned)], FETCH_TIMEOUT));
        self.no_news = false;
        self.unreached = false;
    }

    /// Take a finished check: the events, and whether any relay answered at
    /// all, so "no update published yet" and "couldn't reach the relays" stay
    /// different.
    pub fn apply_report(&mut self, report: &crate::nostr_fetch::FetchReport, now: u64) {
        self.apply_events(&report.events, now);
        self.unreached = self.no_news && !report.reached();
        log::info!("update check: {} of {} relays answered, {} event(s)", report.answered, report.total, report.events.len());
    }

    /// Take the events of a finished check: pick the newest verified release,
    /// remember it and when we looked.
    pub fn apply_events(&mut self, events: &[Event], now: u64) {
        let Some(pinned) = self.pinned else { return };
        match pick_latest(events, &pinned, RELEASE_CHANNEL) {
            Some(best) => {
                self.stored.last_check = now;
                // Never step back: a relay still serving an old retained event must not
                // displace a newer one we already know (the saved one is verified again).
                let known = self.stored.event.as_ref().and_then(|e| parse_release(e, &pinned, RELEASE_CHANNEL));
                if known.is_none_or(|k| (best.version_code, best.created_at) >= (k.version_code, k.created_at)) {
                    self.stored.event = Some(best.event);
                }
                self.no_news = false;
                self.unreached = false;
                self.save();
            }
            None => {
                // Heard nothing valid: keep what we had; try again next launch.
                self.no_news = true;
            }
        }
        self.rejudge();
    }

    /// The package the running (or last) attempt is for.
    fn target_package(&self) -> String {
        match &self.target {
            Target::SelfApp => crate::games::OWN_PACKAGE.to_string(),
            Target::Game(p) => p.clone(),
        }
    }

    /// Start the download and install of the offered release. `Err(notice)`
    /// when a download for ANOTHER package is already running: nothing was
    /// started, and the caller says so.
    pub fn install(&mut self, platform: &mut dyn Platform) -> Result<(), String> {
        let Verdict::Offer(r) = &self.verdict else { return Ok(()) };
        if self.install.active() {
            return if self.target == Target::SelfApp { Ok(()) } else { Err(ANOTHER_INSTALL.into()) };
        }
        // An empty package means "this app": Java installs over itself.
        let job = ApkJob { urls: r.urls.clone(), sha256: r.sha256.clone(), size: r.size, package: String::new(), cert: r.cert.clone(), release_cert: r.cert.clone() };
        let size = r.size;
        self.target = Target::SelfApp;
        match platform.apk_start(&job) {
            Ok(()) => {
                self.install = ApkStatus::Downloading { done: 0, total: size };
                self.poll_left = 0.0;
            }
            Err(e) => self.install = ApkStatus::Failed(e),
        }
        Ok(())
    }

    /// Download and install ANOTHER package from the More games catalog.
    /// `release` was parsed with the catalog entry's own key and channel;
    /// `cert` is the entry's pinned signing certificate. The Java side checks
    /// the downloaded file's package and certificate against both before the
    /// installer sees it. `Err(notice)` while a download for another package
    /// runs: nothing was started, and the caller says so. The same game tapped
    /// again while it downloads is a no-op.
    pub fn install_game(&mut self, platform: &mut dyn Platform, package: &str, release: &Release, cert: &str) -> Result<(), String> {
        if self.install.active() {
            return if self.target == Target::Game(package.to_string()) { Ok(()) } else { Err(ANOTHER_INSTALL.into()) };
        }
        self.target = Target::Game(package.to_string());
        if release.cert != cert {
            self.install = ApkStatus::Failed("That release is signed with a different key than this game's, so it was not installed.".into());
            return Ok(());
        }
        let job = ApkJob { urls: release.urls.clone(), sha256: release.sha256.clone(), size: release.size, package: package.to_string(), cert: cert.to_string(), release_cert: release.cert.clone() };
        match platform.apk_start(&job) {
            Ok(()) => {
                self.install = ApkStatus::Downloading { done: 0, total: release.size };
                self.poll_left = 0.0;
            }
            Err(e) => self.install = ApkStatus::Failed(e),
        }
        Ok(())
    }

    /// "Cancel download": stop the running download (the platform then reports
    /// a failure, which the screen shows; Update works again).
    /// Only this follower's own attempt is cancelled: the platform's one installer
    /// may be on another package's (the next poll then says so).
    pub fn cancel_install(&mut self, platform: &mut dyn Platform) {
        if matches!(self.install, ApkStatus::Downloading { .. }) && !held_by_another(platform, &self.target_package()) {
            platform.apk_cancel();
            self.poll_left = 0.0;
        }
    }

    fn follow(&mut self, st: ApkStatus, platform: &mut dyn Platform) {
        if st == ApkStatus::NeedsPermission && self.install != ApkStatus::NeedsPermission {
            platform.apk_open_install_settings();
        }
        // The platform forgot (a restart of the Java side): don't spin on it.
        self.install = if st == ApkStatus::Idle { ApkStatus::Failed("The install stopped. Tap to try again.".into()) } else { st };
    }

    /// The verdict on the newest known release.
    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// Where the running (or last) download / install stands, whatever it is for.
    pub fn install_status(&self) -> &ApkStatus {
        &self.install
    }

    /// What the running (or last) download / install is for.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The install state of a game's download, `None` if the last job was for something else.
    pub fn game_status(&self, package: &str) -> Option<&ApkStatus> {
        matches!(&self.target, Target::Game(p) if p == package).then_some(&self.install)
    }

    /// A download or install (of anything) is under way.
    pub fn busy(&self) -> bool {
        self.install.active()
    }

    /// The install state as it concerns an update of this app.
    fn self_install(&self) -> &ApkStatus {
        const IDLE: ApkStatus = ApkStatus::Idle;
        if self.target == Target::SelfApp {
            &self.install
        } else {
            &IDLE
        }
    }

    /// An update is on offer, or its download is being followed: Home shows the pill.
    pub fn pill(&self) -> Option<String> {
        let Verdict::Offer(r) = &self.verdict else { return None };
        Some(match self.self_install() {
            ApkStatus::Idle | ApkStatus::Installed => format!("Update to v{}", r.version),
            ApkStatus::Downloading { done, total } => format!("Downloading {}%", percent(*done, *total)),
            ApkStatus::Installing => "Installing…".into(),
            ApkStatus::WaitingForUser => "Confirm in Android's prompt".into(),
            ApkStatus::NeedsPermission => "Allow installs, then tap".into(),
            ApkStatus::Failed(_) => "Update failed. Tap to retry".into(),
        })
    }

    /// The Home pill can be tapped (not mid-download).
    pub fn pill_live(&self) -> bool {
        matches!(self.verdict, Verdict::Offer(_)) && !self.install.active()
    }

    pub fn view(&self, now: u64) -> UpdateView {
        let installed = crate::version_label();
        if !self.configured() {
            return UpdateView { installed, status: NOT_SET_UP.to_string(), tone: UpdateTone::Dim, offer: None, checking: false, progress: None, can_check: false, can_install: false, downloading: false };
        }
        let checking = self.task.is_some();
        let mut offer = None;
        let (mut status, mut tone) = match &self.verdict {
            Verdict::Offer(r) => {
                offer = Some(OfferView { version: r.version.clone(), size: size_text(r.size), notes: notes_text(&r.notes) });
                (format!("Version {} is available ({})", r.version, size_text(r.size)), UpdateTone::Accent)
            }
            Verdict::DifferentKey(r) => (format!("Version {} is out, but it is signed with a different key. Reinstall from the official page.", r.version), UpdateTone::Gold),
            Verdict::UpToDate => (format!("You're up to date{}", ago_suffix(self.stored.last_check, now)), UpdateTone::Dim),
            Verdict::Unknown if self.no_news && self.unreached => ("Couldn't reach the relays".to_string(), UpdateTone::Danger),
            Verdict::Unknown if self.no_news => ("No update published yet".to_string(), UpdateTone::Dim),
            Verdict::Unknown => ("Not checked yet".to_string(), UpdateTone::Dim),
        };
        if checking {
            (status, tone) = ("Checking…".into(), UpdateTone::Dim);
        }
        let mut progress = None;
        if offer.is_some() {
            match self.self_install() {
                ApkStatus::Downloading { done, total } => {
                    status = format!("Downloading… {}%", percent(*done, *total));
                    tone = UpdateTone::Accent;
                    progress = Some((*done as f32 / (*total).max(1) as f32).clamp(0.0, 1.0));
                }
                ApkStatus::Installing => (status, tone) = ("Preparing the install…".into(), UpdateTone::Accent),
                ApkStatus::WaitingForUser => (status, tone) = ("Confirm the update in Android's prompt".into(), UpdateTone::Accent),
                ApkStatus::NeedsPermission => (status, tone) = ("Allow installs from Stampede in the settings page that opened, then tap Update again".into(), UpdateTone::Gold),
                ApkStatus::Failed(m) => (status, tone) = (m.clone(), UpdateTone::Danger),
                ApkStatus::Idle | ApkStatus::Installed => {}
            }
        }
        UpdateView {
            installed,
            status,
            tone,
            can_check: !checking && !self.install.active(),
            can_install: offer.is_some() && !self.install.active() && !checking,
            checking,
            progress,
            offer,
            downloading: matches!(self.self_install(), ApkStatus::Downloading { .. }),
        }
    }
}

fn percent(done: u64, total: u64) -> u64 {
    done.saturating_mul(100).checked_div(total).map_or(0, |p| p.min(100))
}

fn size_text(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// Release notes as one short paragraph.
fn notes_text(notes: &str) -> String {
    let one: String = notes.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 160 {
        let cut: String = one.chars().take(159).collect();
        format!("{cut}…")
    } else {
        one
    }
}

fn ago_suffix(last: u64, now: u64) -> String {
    if last == 0 {
        return String::new();
    }
    let s = now.saturating_sub(last);
    let t = match s {
        0..=89 => "just now".to_string(),
        90..=5399 => format!("{} min ago", (s + 30) / 60),
        5400..=129_599 => format!("{} h ago", (s + 1800) / 3600),
        _ => format!("{} d ago", (s + 43_200) / 86_400),
    };
    format!(", checked {t}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Tag};

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CERT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn tag(parts: &[&str]) -> Tag {
        Tag::parse(parts.iter().copied()).unwrap()
    }

    fn good_tags(code: u32) -> Vec<Tag> {
        vec![
            tag(&["d", RELEASE_CHANNEL]),
            tag(&["version", "0.2.0"]),
            tag(&["version_code", &code.to_string()]),
            tag(&["x", SHA]),
            tag(&["size", "27700000"]),
            tag(&["cert", CERT]),
            tag(&["url", &format!("https://nostr.download/{SHA}.apk")]),
        ]
    }

    fn sign(keys: &Keys, tags: Vec<Tag>) -> Event {
        EventBuilder::new(Kind::Custom(RELEASE_KIND), "Faster bands.").tags(tags).sign_with_keys(keys).unwrap()
    }

    /// `good_tags` with every tag called `name` replaced by `with` (or removed).
    fn changed(code: u32, name: &str, with: Option<&[&str]>) -> Vec<Tag> {
        let mut t: Vec<Tag> = good_tags(code).into_iter().filter(|t| t.as_slice()[0] != name).collect();
        if let Some(w) = with {
            t.push(tag(w));
        }
        t
    }

    fn parse(keys: &Keys, tags: Vec<Tag>) -> Option<Release> {
        parse_release(&sign(keys, tags), &keys.public_key(), RELEASE_CHANNEL)
    }

    #[test]
    fn version_codes_follow_build_apk() {
        assert_eq!(version_code_of("0.1.0"), Some(1000));
        assert_eq!(version_code_of("0.2.0"), Some(2000));
        assert_eq!(version_code_of("1.2.3"), Some(1_002_003));
        assert_eq!(version_code_of("0.1"), Some(1000));
        assert_eq!(version_code_of("0.1000.0"), None);
        assert_eq!(version_code_of("0.1.0.4"), None);
        assert_eq!(version_code_of("x.1.0"), None);
        assert_eq!(installed_version_code(), version_code_of(env!("CARGO_PKG_VERSION")).unwrap());
        assert!(installed_version_code() > 0);
        // Versions compare as their codes do.
        assert!(version_code_of("0.1.1").unwrap() > version_code_of("0.1.0").unwrap());
        assert!(version_code_of("0.2.0").unwrap() > version_code_of("0.1.999").unwrap());
        assert!(version_code_of("1.0.0").unwrap() > version_code_of("0.999.999").unwrap());
    }

    #[test]
    fn the_pinned_key_is_a_real_key() {
        // `None` until Stampede's own release key is pinned; once pinned it must be a real x-only key.
        if let Some(h) = RELEASE_PUBKEY_HEX {
            assert!(PublicKey::from_hex(h).is_ok());
            assert!(h.bytes().any(|b| b != b'0'));
        }
    }

    #[test]
    fn a_good_event_parses() {
        let keys = Keys::generate();
        let r = parse(&keys, good_tags(2000)).expect("valid");
        assert_eq!((r.version.as_str(), r.version_code, r.size), ("0.2.0", 2000, 27_700_000));
        assert_eq!((r.sha256.as_str(), r.cert.as_str()), (SHA, CERT));
        assert_eq!(r.urls, vec![format!("https://nostr.download/{SHA}.apk")]);
        assert_eq!(r.notes, "Faster bands.");
    }

    #[test]
    fn another_signer_is_ignored() {
        let (pinned, other) = (Keys::generate(), Keys::generate());
        let ev = sign(&other, good_tags(2000));
        assert!(parse_release(&ev, &pinned.public_key(), RELEASE_CHANNEL).is_none());
    }

    #[test]
    fn a_bad_signature_or_id_is_ignored() {
        let keys = Keys::generate();
        let pinned = keys.public_key();
        let a = sign(&keys, good_tags(2000));
        let b = sign(&keys, good_tags(2001));
        // A genuine signature, but over a different event.
        let mut swapped = a.clone();
        swapped.sig = b.sig;
        assert!(parse_release(&a, &pinned, RELEASE_CHANNEL).is_some());
        assert!(parse_release(&swapped, &pinned, RELEASE_CHANNEL).is_none());
        // Content changed after signing: the id no longer matches.
        let mut edited = a.clone();
        edited.content = "Free money inside.".into();
        assert!(parse_release(&edited, &pinned, RELEASE_CHANNEL).is_none());
    }

    #[test]
    fn wrong_kind_or_channel_is_ignored() {
        let keys = Keys::generate();
        let other_kind = EventBuilder::new(Kind::Custom(30064), "").tags(good_tags(2000)).sign_with_keys(&keys).unwrap();
        assert!(parse_release(&other_kind, &keys.public_key(), RELEASE_CHANNEL).is_none());
        assert!(parse(&keys, changed(2000, "d", Some(&["d", "skative-android-apk"]))).is_none());
        assert!(parse(&keys, changed(2000, "d", None)).is_none());
    }

    #[test]
    fn only_canonical_blossom_urls_pass() {
        let ok = |u: &str| is_canonical_blossom_url(u, SHA);
        assert!(ok(&format!("https://nostr.download/{SHA}")));
        assert!(ok(&format!("https://nostr.download/{SHA}.apk")));
        assert!(ok(&format!("https://blossom.example.com:8443/{SHA}.apk")));
        for bad in [
            format!("http://nostr.download/{SHA}.apk"),
            format!("https://nostr.download/{SHA}.apk?x=1"),
            format!("https://nostr.download/{SHA}.apk#frag"),
            format!("https://user@nostr.download/{SHA}.apk"),
            format!("https://user:pw@nostr.download/{SHA}.apk"),
            format!("https://nostr.download/uploads/{SHA}.apk"),
            format!("https://nostr.download/{SHA}.apk/"),
            format!("https://nostr.download/{}.apk", "c".repeat(64)),
            format!("https://nostr.download/{SHA}.apk.exe"),
            format!("https://nostr.download/{SHA}."),
            format!("https://nostr.download/{}", &SHA[..63]),
            format!("https:///{SHA}.apk"),
            format!("https://nostr.download{SHA}"),
            "https://nostr.download".to_string(),
        ] {
            assert!(!ok(&bad), "{bad}");
        }
        assert!(!is_canonical_blossom_url(&format!("https://h/{}", "A".repeat(64)), &"A".repeat(64)), "uppercase hash");
    }

    #[test]
    fn a_non_canonical_url_rejects_the_whole_event() {
        let keys = Keys::generate();
        let mut tags = good_tags(2000);
        tags.push(tag(&["url", &format!("https://cdn.example.com/dl/{SHA}.apk?sig=1")]));
        assert!(parse(&keys, tags).is_none());
        // ...and so does having none.
        assert!(parse(&keys, changed(2000, "url", None)).is_none());
    }

    #[test]
    fn malformed_tags_reject_the_event() {
        let keys = Keys::generate();
        let upper = SHA.to_uppercase();
        let upper_x = ["x", upper.as_str()];
        let cases: Vec<(&str, Option<&[&str]>)> = vec![
            ("version", None),
            ("version", Some(&["version", ""])),
            ("version_code", None),
            ("version_code", Some(&["version_code", "0"])),
            ("version_code", Some(&["version_code", "+5"])),
            ("version_code", Some(&["version_code", "1.5"])),
            ("version_code", Some(&["version_code", "99999999999"])),
            ("x", Some(&["x", "short"])),
            ("x", Some(&upper_x)),
            ("size", Some(&["size", "0"])),
            ("size", Some(&["size", "-1"])),
            ("size", None),
            ("size", Some(&["size", "268435457"])),
            ("size", Some(&["size", "999999999999999999"])),
            ("cert", None),
            ("cert", Some(&["cert", "nope"])),
        ];
        for (name, with) in cases {
            assert!(parse(&keys, changed(2000, name, with)).is_none(), "{name} -> {with:?}");
        }
    }

    #[test]
    fn the_highest_version_code_wins_and_invalid_events_never_do() {
        let keys = Keys::generate();
        let impostor = Keys::generate();
        let events = vec![
            sign(&keys, good_tags(2000)),
            sign(&keys, good_tags(3000)),
            sign(&keys, good_tags(2500)),
            // Higher, but not ours / not valid.
            sign(&impostor, good_tags(9000)),
            sign(&keys, changed(8000, "cert", Some(&["cert", "zz"]))),
        ];
        let best = pick_latest(&events, &keys.public_key(), RELEASE_CHANNEL).unwrap();
        assert_eq!(best.version_code, 3000);
        assert!(pick_latest(&[], &keys.public_key(), RELEASE_CHANNEL).is_none());
        assert!(pick_latest(&events[3..], &keys.public_key(), RELEASE_CHANNEL).is_none());
    }

    #[test]
    fn judge_offers_only_a_newer_release_signed_with_our_cert() {
        let keys = Keys::generate();
        let release = |code| parse(&keys, good_tags(code)).unwrap();
        assert!(matches!(judge(Some(release(2000)), 1000, Some(CERT)), Verdict::Offer(r) if r.version_code == 2000));
        // Equal and older are not offered.
        assert!(matches!(judge(Some(release(1000)), 1000, Some(CERT)), Verdict::UpToDate));
        assert!(matches!(judge(Some(release(999)), 1000, Some(CERT)), Verdict::UpToDate));
        // Another certificate is never offered.
        assert!(matches!(judge(Some(release(2000)), 1000, Some(&"c".repeat(64))), Verdict::DifferentKey(_)));
        // An app that cannot read its own cert skips that early check (Android still enforces it).
        assert!(matches!(judge(Some(release(2000)), 1000, None), Verdict::Offer(_)));
        assert!(matches!(judge(None, 1000, Some(CERT)), Verdict::Unknown));
    }

    fn updater_for(keys: &Keys, installed: u32) -> Updater {
        let mut u = Updater::in_memory();
        u.trust(keys.public_key());
        u.installed = installed;
        u
    }

    #[test]
    fn an_offer_installs_through_the_platform_and_follows_it() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        assert!(u.pill().is_none());
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        assert_eq!(u.pill().as_deref(), Some("Update to v0.2.0"));
        let v = u.view(5_000_100);
        assert!(v.can_install && v.can_check);
        assert_eq!(v.offer.as_ref().unwrap().size, "27.7 MB");

        u.install(&mut p).unwrap();
        // A self-update names no package (Java means "this app") and carries the announced certificate.
        assert_eq!(p.apk_started, vec![ApkJob { urls: vec![format!("https://nostr.download/{SHA}.apk")], sha256: SHA.to_string(), size: 27_700_000, package: String::new(), cert: CERT.to_string(), release_cert: CERT.to_string() }]);
        assert_eq!(u.pill().as_deref(), Some("Downloading 0%"));
        // A second tap while it runs does nothing.
        u.install(&mut p).unwrap();
        assert_eq!(p.apk_started.len(), 1);

        p.apk = Some(ApkStatus::Downloading { done: 13_850_000, total: 27_700_000 });
        u.tick(0.3, &mut p);
        let v = u.view(5_000_200);
        assert_eq!(v.progress, Some(0.5));
        assert!(v.status.contains("50%"), "{}", v.status);
        assert!(!v.can_install && !v.can_check);

        p.apk = Some(ApkStatus::WaitingForUser);
        u.tick(0.3, &mut p);
        assert!(u.view(5_000_300).status.contains("prompt"));
    }

    #[test]
    fn the_largest_announced_size_is_capped() {
        let keys = Keys::generate();
        assert_eq!(parse(&keys, changed(2000, "size", Some(&["size", "268435456"]))).map(|r| r.size), Some(MAX_APK_BYTES));
        assert!(parse(&keys, changed(2000, "size", Some(&["size", "268435457"]))).is_none());
    }

    #[test]
    fn a_start_while_the_installer_is_on_another_package_is_refused_visibly() {
        use crate::platform::os::RecordingPlatform;
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);

        // Java is busy with a game while this follower is idle (e.g. after a transient failure).
        p.apk = Some(ApkStatus::Downloading { done: 1, total: 2 });
        p.apk_active = Some("org.example.game".into());
        u.install(&mut p).unwrap();
        assert!(p.apk_started.is_empty(), "nothing was started");
        assert_eq!(*u.install_status(), ApkStatus::Failed(ANOTHER_INSTALL.into()));
        assert_eq!(u.view(5_000_100).status, ANOTHER_INSTALL);
        assert!(u.view(5_000_100).can_install, "and Update works again");

        // The other direction: Java is on our own update, a game is asked for.
        let mut u = updater_for(&keys, 1000);
        let mut p = RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        p.apk = Some(ApkStatus::WaitingForUser);
        p.apk_active = Some(crate::games::OWN_PACKAGE.into());
        let rel = parse_release(&sign(&keys, good_tags(2000)), &keys.public_key(), RELEASE_CHANNEL).unwrap();
        u.install_game(&mut p, "org.example.game", &rel, CERT).unwrap();
        assert!(p.apk_started.is_empty());
        assert_eq!(u.game_status("org.example.game"), Some(&ApkStatus::Failed(ANOTHER_INSTALL.into())));
    }

    #[test]
    fn a_start_while_this_follower_runs_another_package_is_refused_and_changes_nothing() {
        use crate::platform::os::RecordingPlatform;
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        u.install(&mut p).unwrap();
        assert_eq!(u.target(), &Target::SelfApp);
        let rel = parse_release(&sign(&keys, good_tags(2000)), &keys.public_key(), RELEASE_CHANNEL).unwrap();
        assert_eq!(u.install_game(&mut p, "org.example.game", &rel, CERT), Err(ANOTHER_INSTALL.to_string()));
        assert_eq!(p.apk_started.len(), 1);
        assert_eq!(u.target(), &Target::SelfApp, "still following our own");
        assert!(matches!(u.install_status(), ApkStatus::Downloading { .. }));
        // The same package tapped again is a quiet no-op, not a refusal.
        assert_eq!(u.install(&mut p), Ok(()));
        assert_eq!(p.apk_started.len(), 1);
    }

    #[test]
    fn a_follower_never_shows_or_cancels_another_packages_attempt() {
        use crate::platform::os::RecordingPlatform;
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        u.install(&mut p).unwrap();
        // Java's current attempt turns out to be a game's: ours must not show its progress.
        p.apk = Some(ApkStatus::Downloading { done: 1, total: 2 });
        p.apk_active = Some("org.example.game".into());
        u.cancel_install(&mut p);
        assert_eq!(p.apk_cancels, 0, "not ours to cancel");
        u.tick(0.3, &mut p);
        assert_eq!(*u.install_status(), ApkStatus::Failed(ANOTHER_INSTALL.into()));
    }

    #[test]
    fn missing_install_permission_opens_the_settings_page_once() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        u.install(&mut p).unwrap();
        p.apk = Some(ApkStatus::NeedsPermission);
        u.tick(0.3, &mut p);
        assert_eq!(p.install_settings_opened, 1);
        assert_eq!(*u.install_status(), ApkStatus::NeedsPermission);
        // Not polled again while parked; tapping Update retries the whole thing.
        u.tick(0.3, &mut p);
        assert_eq!(p.install_settings_opened, 1);
        assert!(u.view(5_000_100).can_install);
        p.apk = Some(ApkStatus::Downloading { done: 1, total: 2 });
        u.install(&mut p).unwrap();
        assert_eq!(p.apk_started.len(), 2);
    }

    #[test]
    fn a_failure_is_shown_and_can_be_retried() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        u.install(&mut p).unwrap();
        p.apk = Some(ApkStatus::Failed("The download didn't match what was announced.".into()));
        u.tick(0.3, &mut p);
        let v = u.view(5_000_100);
        assert_eq!(v.status, "The download didn't match what was announced.");
        assert_eq!(v.tone, UpdateTone::Danger);
        assert!(v.can_install);
        assert_eq!(u.pill().as_deref(), Some("Update failed. Tap to retry"));
    }

    #[test]
    fn the_update_check_asks_the_release_relays_and_the_players_own_once_each() {
        let mut u = Updater::in_memory();
        let base: Vec<String> = RELEASE_RELAYS.iter().map(|s| s.to_string()).collect();
        assert_eq!(u.check_relays(), base, "no own relays: the release relays only");
        // Own relays are added; one that is a release relay in another spelling is not repeated.
        u.set_user_relays(vec!["wss://relay.damus.io".into(), "wss://NOS.LOL:443/".into(), "wss://mine.example.com".into(), "wss://relay.primal.net".into(), "wss://mine.example.com/".into()]);
        let got = u.check_relays();
        // Order: the player's own, in their order; then the release relays they did not list.
        assert_eq!(got, vec!["wss://relay.damus.io".to_string(), "wss://NOS.LOL:443/".to_string(), "wss://mine.example.com".to_string(), "wss://relay.primal.net".to_string(), "wss://relay.trotters.cc".to_string()]);
        // Clearing the list goes back to the release relays.
        u.set_user_relays(Vec::new());
        assert_eq!(u.check_relays(), base);
    }

    #[test]
    fn desktop_cannot_install_and_says_so() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        // The default Platform has no installer.
        struct NoInstaller;
        impl Platform for NoInstaller {
            fn name(&self) -> &'static str {
                "none"
            }
            fn open_url(&mut self, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn copy_text(&mut self, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn read_clipboard(&mut self) -> Option<String> {
                None
            }
            fn take_launch_link(&mut self) -> Option<String> {
                None
            }
            fn set_multicast_lock(&mut self, _: bool) {}
            fn set_keep_screen_on(&mut self, _: bool) {}
        }
        let mut p = NoInstaller;
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        u.install(&mut p).unwrap();
        assert!(matches!(u.install_status(), ApkStatus::Failed(m) if m.contains("Android")));
    }

    #[test]
    fn the_saved_release_is_checked_again_on_load() {
        let dir = std::env::temp_dir().join(format!("stampede-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let keys = Keys::generate();
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };

        let mut u = Updater::open(&dir);
        u.trust(keys.public_key());
        u.installed = 1000;
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        assert!(dir.join(FILE).exists());

        // A fresh start offers it straight away, without waiting for the relays.
        let mut again = Updater::open(&dir);
        again.trust(keys.public_key());
        again.installed = 1000;
        again.tick(0.0, &mut p);
        assert_eq!(again.pill().as_deref(), Some("Update to v0.2.0"));
        assert!(!again.due(5_000_000 + 3600));
        assert!(again.due(5_000_000 + CHECK_INTERVAL_SECS));

        // Once the new version is installed the saved event is just history...
        let mut after = Updater::open(&dir);
        after.trust(keys.public_key());
        after.installed = 2000;
        after.tick(0.0, &mut p);
        assert!(after.pill().is_none());
        assert!(matches!(after.verdict(), Verdict::UpToDate));

        // ...a different signer's genuine event is not ours...
        let mut forged = Updater::open(&dir);
        forged.trust(Keys::generate().public_key());
        forged.installed = 1000;
        forged.tick(0.0, &mut p);
        assert!(matches!(forged.verdict(), Verdict::Unknown));

        // ...an edited file (a bigger version_code under the old id and signature) buys nothing...
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(text.contains("\"2000\""), "{text}");
        std::fs::write(dir.join(FILE), text.replace("\"2000\"", "\"9000\"")).unwrap();
        let mut edited = Updater::open(&dir);
        edited.trust(keys.public_key());
        edited.installed = 1000;
        edited.tick(0.0, &mut p);
        assert!(matches!(edited.verdict(), Verdict::Unknown), "an edited event must fail id/signature");
        std::fs::write(dir.join(FILE), text).unwrap();

        // ...and garbage in the file is a fresh start.
        std::fs::write(dir.join(FILE), b"{not json").unwrap();
        let fresh = Updater::open(&dir);
        assert!(fresh.due(1) && matches!(fresh.verdict(), Verdict::Unknown));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_check_that_hears_nothing_keeps_what_we_knew() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        assert_eq!(u.view(10).status, "Not checked yet");
        // Relays that answered with nothing: no update has been published.
        u.apply_events(&[], 10);
        assert_eq!(u.view(10).status, "No update published yet");
        assert_eq!(u.view(10).tone, UpdateTone::Dim);
        assert!(u.due(10), "an empty answer doesn't reset the clock");
        // Nobody answered: that is a connection problem, said as one.
        let none = crate::nostr_fetch::FetchReport { events: vec![], answered: 0, total: 3 };
        u.apply_report(&none, 11);
        assert_eq!(u.view(11).status, "Couldn't reach the relays");
        assert_eq!(u.view(11).tone, UpdateTone::Danger);
        // Two of three answered, with nothing: reached.
        let empty = crate::nostr_fetch::FetchReport { events: vec![], answered: 2, total: 3 };
        u.apply_report(&empty, 12);
        assert_eq!(u.view(12).status, "No update published yet");
        u.apply_events(&[sign(&keys, good_tags(2000))], 20);
        u.apply_events(&[], 30);
        assert_eq!(u.pill().as_deref(), Some("Update to v0.2.0"), "a later empty answer keeps the known release");
    }

    #[test]
    fn an_older_retained_event_never_displaces_a_newer_known_one() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(3000))], 10);
        u.apply_events(&[sign(&keys, good_tags(2000))], 20);
        assert_eq!(u.verdict_code(), Some(3000), "a relay serving only the older event keeps the newer offer");
        u.apply_events(&[sign(&keys, good_tags(4000))], 30);
        assert_eq!(u.verdict_code(), Some(4000));
    }

    #[test]
    fn a_last_check_in_the_future_is_due() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        u.apply_events(&[sign(&keys, good_tags(2000))], 9_000_000);
        assert!(!u.due(9_000_100));
        assert!(u.due(1_000_000), "the clock was wrong when the check was taken");
    }

    #[test]
    fn cancel_asks_the_platform_only_while_downloading() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5_000_000);
        u.cancel_install(&mut p);
        assert_eq!(p.apk_cancels, 0);
        u.install(&mut p).unwrap();
        assert!(u.view(5_000_100).downloading);
        u.cancel_install(&mut p);
        assert_eq!(p.apk_cancels, 1);
        p.apk = Some(ApkStatus::Failed("The download was cancelled.".into()));
        u.tick(0.3, &mut p);
        let v = u.view(5_000_200);
        assert!(!v.downloading && v.can_install && v.can_check && v.status.contains("cancelled"));
    }

    #[test]
    fn a_demo_offer_never_reaches_the_saved_file() {
        let dir = std::env::temp_dir().join(format!("stampede-update-demo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut u = Updater::open(&dir);
        u.demo_offer("9.9.9", "demo");
        assert!(!dir.join(FILE).exists() && matches!(u.verdict(), Verdict::Unknown));
        let mut m = Updater::in_memory();
        m.demo_offer("9.9.9", "demo");
        assert!(matches!(m.verdict(), Verdict::Offer(_)));
    }

    #[test]
    fn the_status_line_for_a_current_install_says_when_it_looked() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 2000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 1_000_000);
        assert_eq!(u.view(1_000_000).status, "You're up to date, checked just now");
        assert_eq!(u.view(1_000_000 + 7200).status, "You're up to date, checked 2 h ago");
        assert!(u.view(1_000_000).offer.is_none() && u.pill().is_none());
    }

    #[test]
    fn a_release_signed_with_another_certificate_is_explained_not_offered() {
        let keys = Keys::generate();
        let mut u = updater_for(&keys, 1000);
        let mut p = crate::platform::os::RecordingPlatform { cert: Some("c".repeat(64)), ..Default::default() };
        u.tick(0.0, &mut p);
        u.apply_events(&[sign(&keys, good_tags(2000))], 5);
        assert!(u.pill().is_none());
        let v = u.view(5);
        assert!(v.status.contains("different key") && !v.can_install && v.offer.is_none());
    }

    #[test]
    fn an_event_from_the_node_publisher_verifies_here() {
        // The publisher's own code path (`buildReleaseEvent` + nostr-tools `finalizeEvent`, as
        // `publish-release.mjs` runs it) signed this with a THROWAWAY key whose secret was
        // discarded. Never commit an event signed by the real release key: it would be a
        // publishable announcement (the publisher's --dry-run signs with a throwaway key too).
        const TEST_PUBKEY: &str = "8835941fd71d7197381916c90e3ee71bcba254259ef28f7d95828fe26e9a05f2";
        let json = r#"{"kind":30063,"created_at":1791886220,"tags":[["d","stampede-android-apk"],["version","0.1.0"],["version_code","1000"],["x","3ff9a9900fdf6c4e4227f9cd7874b97fc78a4f7c42796a32199fa994eec87b21"],["size","10021738"],["cert","eacd24505a50630e2a115226fe592518bc16cc2421cfc019372b58baeaf08aa9"],["url","https://nostr.download/3ff9a9900fdf6c4e4227f9cd7874b97fc78a4f7c42796a32199fa994eec87b21.apk"]],"content":"First in-app update release.","pubkey":"8835941fd71d7197381916c90e3ee71bcba254259ef28f7d95828fe26e9a05f2","id":"e06bd01082aa35fd96ac9af607c305f736377c206706800fb9981fa7249cb18f","sig":"69e1f0c0d0a004417bb070c38be377ed3d093f2fe60dd37972c4b43a7856ac8187ff3b42d9370417b9d2af7a9b80435902868d0abe4a8755d7e7146b7eb22608"}"#;
        let ev: Event = serde_json::from_str(json).unwrap();
        let pinned = PublicKey::from_hex(TEST_PUBKEY).unwrap();
        let r = parse_release(&ev, &pinned, RELEASE_CHANNEL).expect("the app accepts what the publisher signs");
        assert_eq!((r.version.as_str(), r.version_code, r.size), ("0.1.0", 1000, 10_021_738));
        assert_eq!(r.notes, "First in-app update release.");
        // The production key does not trust it.
        assert!(RELEASE_PUBKEY_HEX.is_none_or(|h| parse_release(&ev, &PublicKey::from_hex(h).unwrap(), RELEASE_CHANNEL).is_none()));
        // The same event is not offered to an install that already has it.
        assert!(matches!(judge(Some(r), 1000, Some(&r_cert())), Verdict::UpToDate));
    }

    #[test]
    fn the_url_grammar_matches_the_publishers_shared_vectors() {
        // tools/release/url-vectors.json is read by release-helpers.test.mjs too: one grammar, two implementations.
        let list: Vec<serde_json::Value> = serde_json::from_str(include_str!("../../tools/release/url-vectors.json")).unwrap();
        assert!(list.len() > 20);
        for v in list {
            let url = v["url"].as_str().unwrap().replace("<sha>", SHA).replace("<SHAUP>", &SHA.to_uppercase());
            assert_eq!(is_canonical_blossom_url(&url, SHA), v["ok"].as_bool().unwrap(), "{url:?}");
        }
    }

    impl Updater {
        fn verdict_code(&self) -> Option<u32> {
            match &self.verdict {
                Verdict::Offer(r) | Verdict::DifferentKey(r) => Some(r.version_code),
                _ => None,
            }
        }
    }

    fn r_cert() -> String {
        "eacd24505a50630e2a115226fe592518bc16cc2421cfc019372b58baeaf08aa9".into()
    }

    #[test]
    fn with_no_release_key_pinned_nothing_is_checked_and_the_card_says_so() {
        let mut u = Updater::in_memory();
        u.pinned = None;
        u.enabled = true;
        assert!(!u.configured());
        let mut p = crate::platform::os::RecordingPlatform { cert: Some(CERT.into()), ..Default::default() };
        // The launch check would run on a platform that can install, were a key pinned.
        u.tick(0.0, &mut p);
        assert!(u.task.is_none());
        u.check_now();
        assert!(u.task.is_none(), "no key, no network");
        let v = u.view(100);
        assert_eq!(v.status, NOT_SET_UP);
        assert_eq!(v.tone, UpdateTone::Dim);
        assert!(!v.can_check && !v.can_install && !v.checking && v.offer.is_none());
        // Even a validly signed event is not news without a pinned key.
        let keys = Keys::generate();
        u.apply_events(&[sign(&keys, good_tags(2000))], 5);
        assert!(u.pill().is_none() && matches!(u.verdict(), Verdict::Unknown));
        assert!(u.due(5), "nothing was recorded");
    }

    #[test]
    fn the_shipped_constants_are_stampedes_own() {
        assert_eq!(RELEASE_CHANNEL, "stampede-android-apk");
        assert_eq!(RELEASE_RELAYS, ["wss://relay.trotters.cc", "wss://relay.damus.io", "wss://nos.lol"]);
        // Twang Thang's release key is not Stampede's: it must never be pinned here.
        assert_ne!(RELEASE_PUBKEY_HEX, Some("2cc65a54774d9c1d6fde7e98255dbe1064940facb10dbc3e49f63300a27178c7"));
    }

    #[test]
    fn notes_are_one_short_paragraph() {
        assert_eq!(notes_text("  Faster\n\nbands.  "), "Faster bands.");
        let long = notes_text(&"word ".repeat(100));
        assert!(long.chars().count() <= 160 && long.ends_with('…'));
    }
}
