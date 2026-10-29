//! More games (spec 20.3): a catalog of other games, built into the app, that
//! can be installed from here. There is no remote list. Each entry pins the
//! game's package, its release channel, the key that signs its release events
//! and the certificate its APK is signed with.
//!
//! The screen opens one relay pass ([`Games::begin`]) and judges every entry
//! with the same rules as the self-update ([`crate::update::parse_release`]),
//! using THAT entry's key and channel. Installing goes through
//! [`crate::update::Updater::install_game`]; the Java side then reads the
//! downloaded file's package and signing certificate and refuses unless they
//! match the catalog ([`judge_archive`], mirrored in `ApkCheck.java` and tested
//! against the same vectors).

use crate::nostr_fetch::{FetchReport, Task};
use crate::platform::os::Platform;
use crate::update::{self, Release};
use nostr::{Event, PublicKey};
use std::time::Duration;

/// One game in the catalog.
pub struct GameEntry {
    pub name: &'static str,
    /// One line.
    pub blurb: &'static str,
    /// The Android package.
    pub package: &'static str,
    /// The release channel: the `d` tag of its release events.
    pub channel: &'static str,
    /// The key that signs its releases (hex). `None` = not published yet.
    pub release_pubkey: Option<&'static str>,
    /// SHA-256 of the certificate its APK is signed with (hex). `None` = not known yet.
    pub cert: Option<&'static str>,
    /// A PNG, bundled in the app.
    pub icon: &'static [u8],
}

impl GameEntry {
    /// Both pins are known: the entry can be fetched and installed. Otherwise "Coming soon".
    pub fn ready(&self) -> bool {
        self.release_pubkey.is_some_and(|k| PublicKey::from_hex(k).is_ok()) && self.cert.is_some_and(is_hex64)
    }

    fn key(&self) -> Option<PublicKey> {
        self.release_pubkey.and_then(|k| PublicKey::from_hex(k).ok())
    }
}

/// This app's own package: the one installer's "owner" while Stampede updates itself.
pub const OWN_PACKAGE: &str = "org.decented.stampede";

/// Stampede's catalog. It omits Stampede itself.
pub const CATALOG: &[GameEntry] = &[GameEntry {
    name: "Twang Thang",
    blurb: "A neon energy-band party game.",
    package: "org.decented.twangthang",
    channel: "twangthang-android-apk",
    release_pubkey: Some("2cc65a54774d9c1d6fde7e98255dbe1064940facb10dbc3e49f63300a27178c7"),
    cert: Some("ad02f430148557fb6501c97b16a9ef60681d12d533f7579035e6d62a83dec640"),
    icon: include_bytes!("../assets/games/twangthang.png"),
}];

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// How long the relays get to answer.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// How often the installed versions are read again while the screen is open.
const INSTALLED_POLL_SECS: f32 = 1.0;

/// The newest release of `entry`: signed by the entry's own key, on the
/// entry's own channel, well formed with canonical URLs ([`update::parse_release`]),
/// and carrying the entry's pinned certificate. Anything else is `Err`/`Ok(None)`.
pub fn release_for(entry: &GameEntry, events: &[Event]) -> Result<Option<Release>, CertMismatch> {
    let (Some(key), Some(cert)) = (entry.key(), entry.cert) else { return Ok(None) };
    match update::pick_latest(events, &key, entry.channel) {
        Some(r) if r.cert != cert => Err(CertMismatch { version: r.version }),
        other => Ok(other),
    }
}

/// The newest verified release is signed with a different certificate than the catalog pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertMismatch {
    pub version: String,
}

/// What a card shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameStatus {
    /// No key or certificate pinned yet.
    ComingSoon,
    /// The relay pass is running.
    Checking,
    /// No relay answered.
    CantReach,
    /// Relays answered, no release of this game is published.
    NoRelease,
    /// A release exists but is signed with another key than the catalog's: not offered.
    WrongKey { version: String },
    /// Not installed, a release can be installed.
    Get { version: String, size: u64 },
    /// Installed, nothing newer.
    Open,
    /// Installed, a newer release exists.
    Update { version: String, size: u64 },
}

/// Pure: what a card shows, from what is known.
pub fn status_of(entry: &GameEntry, fetch: &Fetch, release: &Result<Option<Release>, CertMismatch>, installed: Option<u32>) -> GameStatus {
    if !entry.ready() {
        return GameStatus::ComingSoon;
    }
    match (release, installed) {
        (Ok(Some(r)), Some(code)) if r.version_code > code => GameStatus::Update { version: r.version.clone(), size: r.size },
        (Ok(Some(r)), None) => GameStatus::Get { version: r.version.clone(), size: r.size },
        (_, Some(_)) => GameStatus::Open,
        (Err(m), None) => GameStatus::WrongKey { version: m.version.clone() },
        (Ok(None), None) => match fetch {
            Fetch::Idle | Fetch::Running => GameStatus::Checking,
            Fetch::Unreached => GameStatus::CantReach,
            Fetch::Answered => GameStatus::NoRelease,
        },
    }
}

/// How the relay pass stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    Idle,
    Running,
    /// Relays answered (possibly with nothing).
    Answered,
    /// No relay answered at all.
    Unreached,
}

/// The More games screen's data: the relay pass and the installed versions.
pub struct Games {
    task: Option<Task<FetchReport>>,
    fetch: Fetch,
    releases: Vec<Result<Option<Release>, CertMismatch>>,
    installed: Vec<Option<u32>>,
    poll_left: f32,
}

impl Default for Games {
    fn default() -> Self {
        Self::new()
    }
}

impl Games {
    pub fn new() -> Games {
        Games { task: None, fetch: Fetch::Idle, releases: CATALOG.iter().map(|_| Ok(None)).collect(), installed: CATALOG.iter().map(|_| None).collect(), poll_left: 0.0 }
    }

    /// The one filter for the whole catalog: every ready entry's key and channel.
    pub fn filter() -> Option<nostr::Filter> {
        let ready: Vec<&GameEntry> = CATALOG.iter().filter(|e| e.ready()).collect();
        if ready.is_empty() {
            return None;
        }
        let keys: Vec<PublicKey> = ready.iter().filter_map(|e| e.key()).collect();
        let channels: Vec<&str> = ready.iter().map(|e| e.channel).collect();
        Some(update::catalog_filter(&keys, &channels))
    }

    /// The screen opened: one pass over `relays` (the player's, then the release relays). Never
    /// touches the network when no entry is ready.
    pub fn begin(&mut self, user_relays: &[String]) {
        let Some(filter) = Self::filter() else { return };
        self.fetch = Fetch::Running;
        self.task = Some(crate::nostr_fetch::fetch_live(update::release_relays(user_relays), vec![filter], FETCH_TIMEOUT));
    }

    /// Frame hook: collect the relay pass; read the installed versions now and then.
    pub fn tick(&mut self, dt: f32, platform: &mut dyn Platform) {
        if let Some(report) = self.task.as_ref().and_then(|t| t.poll()) {
            self.task = None;
            self.apply_report(&report);
        }
        self.poll_left -= dt;
        if self.poll_left <= 0.0 {
            self.poll_left = INSTALLED_POLL_SECS;
            for (i, e) in CATALOG.iter().enumerate() {
                self.installed[i] = platform.installed_version_code(e.package);
            }
        }
    }

    /// Take a finished relay pass.
    pub fn apply_report(&mut self, report: &FetchReport) {
        for (i, e) in CATALOG.iter().enumerate() {
            self.releases[i] = release_for(e, &report.events);
        }
        self.fetch = if report.reached() { Fetch::Answered } else { Fetch::Unreached };
        log::info!("more games: {} of {} relays answered, {} event(s)", report.answered, report.total, report.events.len());
    }

    pub fn fetch(&self) -> Fetch {
        self.fetch
    }

    pub fn status(&self, i: usize) -> GameStatus {
        match (CATALOG.get(i), self.releases.get(i), self.installed.get(i)) {
            (Some(e), Some(release), Some(installed)) => status_of(e, &self.fetch, release, *installed),
            _ => GameStatus::ComingSoon,
        }
    }

    /// Get / Update on entry `i`: download, verify and install through the shared updater.
    /// `Err(notice)` when another package's install is running (nothing started): show it.
    pub fn install(&self, i: usize, updates: &mut update::Updater, platform: &mut dyn Platform) -> Result<(), String> {
        let Some(e) = CATALOG.get(i) else { return Ok(()) };
        let Some(Ok(Some(r))) = self.releases.get(i) else { return Ok(()) };
        let Some(cert) = e.cert else { return Ok(()) };
        updates.install_game(platform, e.package, r, cert)
    }

    /// Open the installed game.
    pub fn open(&self, i: usize, platform: &mut dyn Platform) -> bool {
        CATALOG.get(i).is_some_and(|e| platform.open_package(e.package))
    }

    /// Screenshots and tests: set what is known.
    pub fn demo(&mut self, fetch: Fetch, installed: Vec<Option<u32>>, releases: Vec<Result<Option<Release>, CertMismatch>>) {
        self.fetch = fetch;
        self.installed = installed;
        self.releases = releases;
    }
}

// ─── Checking the downloaded file ───────────────────────────────────────────

/// What the player is told when the downloaded file is not the expected app from the
/// expected signer (`Updater.DIDNT_CHECK_OUT` in the Java source).
pub const DIDNT_CHECK_OUT: &str = "This download didn't check out, so it wasn't installed.";

/// The decision made on the downloaded file before the installer session is
/// committed: its package (`got_pkg`) and its one signing certificate (`got_cert`, SHA-256
/// hex; empty = unreadable) against what it must be. `strict` (a game from the
/// catalog): the package, the catalog's pinned certificate AND the release event's
/// certificate must all be present and equal what the file says. Loose (this app's
/// own update): the package must match, and a certificate that was read must equal
/// each one that was given. This is `ApkCheck.verify` in the Java source; both are
/// tested against `app/android/test/apk-check-vectors.tsv`.
pub fn archive_ok(got_pkg: &str, got_cert: &str, want_pkg: &str, pinned_cert: &str, release_cert: &str, strict: bool) -> bool {
    if want_pkg.is_empty() || got_pkg != want_pkg {
        return false;
    }
    if strict {
        return is_hex64(got_cert) && is_hex64(pinned_cert) && is_hex64(release_cert) && got_cert == pinned_cert && got_cert == release_cert;
    }
    if got_cert.is_empty() {
        return true;
    }
    (pinned_cert.is_empty() || got_cert == pinned_cert) && (release_cert.is_empty() || got_cert == release_cert)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::os::{ApkJob, ApkStatus, RecordingPlatform};
    use crate::update::{RELEASE_KIND, Updater};
    use nostr::util::JsonUtil;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CERT: &str = "ad02f430148557fb6501c97b16a9ef60681d12d533f7579035e6d62a83dec640";
    const OTHER_CERT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CHANNEL: &str = "twangthang-android-apk";

    fn tag(parts: &[&str]) -> Tag {
        Tag::parse(parts.iter().copied()).unwrap()
    }

    fn event(keys: &Keys, channel: &str, code: u32, cert: &str) -> Event {
        let tags = vec![
            tag(&["d", channel]),
            tag(&["version", "1.2.3"]),
            tag(&["version_code", &code.to_string()]),
            tag(&["x", SHA]),
            tag(&["size", "9000000"]),
            tag(&["cert", cert]),
            tag(&["url", &format!("https://nostr.download/{SHA}.apk")]),
        ];
        EventBuilder::new(Kind::Custom(RELEASE_KIND), "notes").tags(tags).sign_with_keys(keys).unwrap()
    }

    /// A catalog entry for tests, pinned to `keys`.
    fn entry_for(keys: &Keys) -> GameEntry {
        // The test key's hex has to live as long as the entry: leak it (tests only).
        let hex: &'static str = Box::leak(keys.public_key().to_hex().into_boxed_str());
        GameEntry { name: "Test", blurb: "", package: "org.example.test", channel: CHANNEL, release_pubkey: Some(hex), cert: Some(CERT), icon: &[] }
    }

    #[test]
    fn the_catalog_pins_are_well_formed() {
        for e in CATALOG {
            assert!(e.ready(), "{} should be fetchable", e.name);
            assert!(!e.name.is_empty() && !e.blurb.is_empty() && e.blurb.len() < 60 && !e.icon.is_empty());
            assert_ne!(e.package, OWN_PACKAGE, "the catalog omits Stampede itself");
        }
        assert_eq!(CATALOG[0].package, "org.decented.twangthang");
        assert_eq!(CATALOG[0].channel, "twangthang-android-apk");
    }

    #[test]
    fn an_entry_without_a_key_or_cert_is_coming_soon_and_never_fetched() {
        let mut e = entry_for(&Keys::generate());
        e.release_pubkey = None;
        assert!(!e.ready());
        assert_eq!(status_of(&e, &Fetch::Answered, &Ok(None), None), GameStatus::ComingSoon);
        let mut e2 = entry_for(&Keys::generate());
        e2.cert = None;
        assert_eq!(status_of(&e2, &Fetch::Answered, &Ok(None), Some(5)), GameStatus::ComingSoon);
        // Nothing to fetch: no filter, so no network.
        assert!(release_for(&e, &[]).unwrap().is_none());
    }

    #[test]
    fn an_event_counts_only_from_that_entrys_key_and_channel() {
        let (key, other) = (Keys::generate(), Keys::generate());
        let e = entry_for(&key);
        // Right key and channel.
        assert_eq!(release_for(&e, &[event(&key, CHANNEL, 7, CERT)]).unwrap().unwrap().version_code, 7);
        // Another key on the same channel, and the right key on another channel (e.g. Stampede's own).
        assert!(release_for(&e, &[event(&other, CHANNEL, 9, CERT)]).unwrap().is_none());
        assert!(release_for(&e, &[event(&key, "stampede-android-apk", 9, CERT)]).unwrap().is_none());
        // The highest valid one wins among several.
        let evs = [event(&key, CHANNEL, 5, CERT), event(&key, CHANNEL, 8, CERT), event(&other, CHANNEL, 99, CERT)];
        assert_eq!(release_for(&e, &evs).unwrap().unwrap().version_code, 8);
        // A release signed with another certificate than the catalog pins is not offered.
        assert_eq!(release_for(&e, &[event(&key, CHANNEL, 7, OTHER_CERT)]), Err(CertMismatch { version: "1.2.3".into() }));
    }

    #[test]
    fn statuses_follow_what_is_installed_and_published() {
        let key = Keys::generate();
        let e = entry_for(&key);
        let rel = |code| release_for(&e, &[event(&key, CHANNEL, code, CERT)]);
        let got = |f: Fetch, r: &Result<Option<Release>, CertMismatch>, i: Option<u32>| status_of(&e, &f, r, i);
        let get = GameStatus::Get { version: "1.2.3".into(), size: 9_000_000 };
        let upd = GameStatus::Update { version: "1.2.3".into(), size: 9_000_000 };
        assert_eq!(got(Fetch::Answered, &rel(7), None), get);
        assert_eq!(got(Fetch::Answered, &rel(7), Some(7)), GameStatus::Open);
        assert_eq!(got(Fetch::Answered, &rel(7), Some(9)), GameStatus::Open);
        assert_eq!(got(Fetch::Answered, &rel(7), Some(6)), upd);
        // Nothing published: honest wording, and an installed game is still just openable.
        assert_eq!(got(Fetch::Answered, &Ok(None), None), GameStatus::NoRelease);
        assert_eq!(got(Fetch::Unreached, &Ok(None), None), GameStatus::CantReach);
        assert_eq!(got(Fetch::Running, &Ok(None), None), GameStatus::Checking);
        assert_eq!(got(Fetch::Unreached, &Ok(None), Some(3)), GameStatus::Open);
        let bad = release_for(&e, &[event(&key, CHANNEL, 7, OTHER_CERT)]);
        assert_eq!(got(Fetch::Answered, &bad, None), GameStatus::WrongKey { version: "1.2.3".into() });
    }

    #[test]
    fn the_filter_names_every_ready_key_and_channel() {
        let f = Games::filter().expect("the catalog has a ready entry");
        let json = f.try_as_json().unwrap();
        assert!(json.contains("2cc65a54774d9c1d6fde7e98255dbe1064940facb10dbc3e49f63300a27178c7"), "{json}");
        assert!(json.contains("30063") && json.contains("twangthang-android-apk"), "{json}");
    }

    #[test]
    fn a_report_is_judged_per_entry_and_unreached_is_not_no_release() {
        // The real catalog entry never accepts an event from a throwaway key.
        let mut g = Games::new();
        let impostor = Keys::generate();
        g.apply_report(&FetchReport { events: vec![event(&impostor, CHANNEL, 7, CERT)], answered: 2, total: 3 });
        assert_eq!(g.status(0), GameStatus::NoRelease);
        g.apply_report(&FetchReport { events: vec![], answered: 0, total: 3 });
        assert_eq!(g.status(0), GameStatus::CantReach);
    }

    #[test]
    fn the_installed_version_is_read_from_the_platform() {
        let mut g = Games::new();
        let mut p = RecordingPlatform::default();
        g.tick(0.0, &mut p);
        assert!(matches!(g.status(0), GameStatus::Checking | GameStatus::NoRelease));
        p.installed_packages.insert("org.decented.twangthang".into(), 1000);
        g.tick(1.5, &mut p);
        assert_eq!(g.status(0), GameStatus::Open);
        assert!(g.open(0, &mut p));
        assert_eq!(p.opened_packages, vec!["org.decented.twangthang".to_string()]);
    }

    #[test]
    fn installing_a_game_names_its_package_and_pinned_cert_and_leaves_the_self_update_alone() {
        let key = Keys::generate();
        let e = entry_for(&key);
        let r = release_for(&e, &[event(&key, CHANNEL, 7, CERT)]).unwrap().unwrap();
        let mut u = Updater::in_memory();
        let mut p = RecordingPlatform { cert: Some("c".repeat(64)), ..Default::default() };
        u.tick(0.0, &mut p);
        u.install_game(&mut p, e.package, &r, CERT).unwrap();
        assert_eq!(p.apk_started, vec![ApkJob { urls: r.urls.clone(), sha256: SHA.into(), size: 9_000_000, package: "org.example.test".into(), cert: CERT.into(), release_cert: CERT.into() }]);
        assert_eq!(*u.install_status(), ApkStatus::Downloading { done: 0, total: 9_000_000 });
        assert_eq!(u.game_status("org.example.test"), Some(&ApkStatus::Downloading { done: 0, total: 9_000_000 }));
        assert_eq!(u.game_status("org.other"), None);
        // The self-update screens see nothing of it, but cannot start a second download.
        assert!(u.pill().is_none());
        let v = u.view(0);
        assert!(!v.downloading && v.progress.is_none());
        u.install_game(&mut p, e.package, &r, CERT).unwrap();
        assert_eq!(p.apk_started.len(), 1, "one download at a time");
        // Progress is followed like any download, and a failure is kept for the card.
        p.apk = Some(ApkStatus::Failed(DIDNT_CHECK_OUT.into()));
        u.tick(0.3, &mut p);
        assert_eq!(u.game_status("org.example.test"), Some(&ApkStatus::Failed(DIDNT_CHECK_OUT.into())));
    }

    #[test]
    fn a_release_whose_cert_differs_from_the_pin_is_not_started() {
        let key = Keys::generate();
        let e = entry_for(&key);
        let r = match release_for(&e, &[event(&key, CHANNEL, 7, OTHER_CERT)]) {
            Err(_) => {
                // release_for refuses it, so build the Release the way the parser would, with the wrong cert.
                crate::update::parse_release(&event(&key, CHANNEL, 7, OTHER_CERT), &key.public_key(), CHANNEL).unwrap()
            }
            Ok(_) => panic!("must be refused"),
        };
        let mut u = Updater::in_memory();
        let mut p = RecordingPlatform::default();
        u.install_game(&mut p, e.package, &r, CERT).unwrap();
        assert!(p.apk_started.is_empty());
        assert!(matches!(u.install_status(), ApkStatus::Failed(m) if m.contains("different key")));
    }

    #[test]
    fn the_archive_vectors_shared_with_the_java_check() {
        // app/android/test/apk-check-vectors.tsv is read by ApkCheckTest.java too: one decision, two implementations.
        let text = include_str!("../android/test/apk-check-vectors.tsv");
        let mut n = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
            let f: Vec<&str> = line.split('\t').map(|x| if x == "-" { "" } else { x }).collect();
            assert_eq!(f.len(), 7, "{line:?}");
            let got = archive_ok(f[0], f[1], f[2], f[3], f[4], f[5] == "strict");
            assert_eq!(got, f[6] == "ok", "{line}");
            n += 1;
        }
        assert!(n >= 10, "{n} vectors");
    }

    /// Twang Thang's own publisher signed this with a THROWAWAY key whose secret was discarded:
    /// it is what a real Twang release event looks like to the More games parse path.
    #[test]
    fn a_twang_release_event_parses_for_an_entry_pinned_to_its_key() {
        const PUB: &str = "44e49e42cde28b3a1cb5d2b43fe7f3d3a2fd36bb977d1319c7d05a34f497c878";
        let json = r#"{"kind":30063,"created_at":1791721775,"tags":[["d","twangthang-android-apk"],["version","0.1.0"],["version_code","1000"],["x","2a6f9fe9a8233340827bb1851aeb02fee659c7060fa97d27a7352aa16763016a"],["size","9796461"],["cert","ad02f430148557fb6501c97b16a9ef60681d12d533f7579035e6d62a83dec640"],["url","https://nostr.download/2a6f9fe9a8233340827bb1851aeb02fee659c7060fa97d27a7352aa16763016a.apk"]],"content":"First in-app update release.","pubkey":"44e49e42cde28b3a1cb5d2b43fe7f3d3a2fd36bb977d1319c7d05a34f497c878","id":"e8867c4aa8ced84f3c676f4dea1636eaac658ee1682c160e82c02fb6ecdebb50","sig":"f9d4e55bac1bf7ef6258be3df0dbb4a67fdbea7f50cf3fbacd4ea2eec6defd0a39694c0c9568aeb7a9dd76d70bd72ff93ddd68c5ee6278483f859e4c4e0c453a"}"#;
        let ev: Event = serde_json::from_str(json).unwrap();
        let entry = GameEntry { release_pubkey: Some(PUB), ..entry_for(&Keys::generate()) };
        let entry = GameEntry { cert: CATALOG[0].cert, ..entry };
        let r = release_for(&entry, std::slice::from_ref(&ev)).unwrap().expect("accepted for the key it was signed by");
        assert_eq!((r.version.as_str(), r.version_code, r.size), ("0.1.0", 1000, 9_796_461));
        // The real catalog entry (a different key) does not accept it.
        assert!(release_for(&CATALOG[0], std::slice::from_ref(&ev)).unwrap().is_none());
    }
}
