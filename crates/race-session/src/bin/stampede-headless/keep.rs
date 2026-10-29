//! What keeps a headless online room open: when its code is ready to share,
//! when it returns to the lobby, and the addresses it announced.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use decented_net::Identity;
use nostr::Keys;
use race_session::{HostPhase, APP_TAG};

/// The file in `--data-dir` that holds the Keep's synthetic persona key.
/// (The runtime key lives beside it in `identity.key`, written by Fray.)
pub const PERSONA_FILE: &str = "persona.key";
/// The runtime key's file, written by Fray beside it.
pub const IDENTITY_FILE: &str = "identity.key";

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Write `bytes` to `path` so a reader sees the old file or the whole new one,
/// never half: a temp file in the same directory (mode 0600, synced), then a
/// rename, then a sync of the directory so the rename itself survives a power
/// cut.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or("Bad file name")?;
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Cannot write {}: {e}", path.display()));
    }
    // The file is in place; the directory sync only makes that survive a
    // power cut. Some filesystems refuse it, so that is not a failed write.
    if let Err(e) = sync_parent(path) {
        log::warn!("[keep] Cannot sync the directory of {}: {e}", path.display());
    }
    Ok(())
}

/// Flush the directory entry of `path` to disk (unix; elsewhere a no-op).
#[cfg(unix)]
fn sync_parent(path: &Path) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Make `dir` (private to the owner) if it is not there yet.
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Cannot protect {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// A runtime identity vouched for by a synthetic `persona`, signed now.
pub fn sign_identity(runtime: Identity, persona: &Keys) -> Result<Identity, String> {
    let event = decented_signet::attestation::unsigned_attestation(
        APP_TAG,
        &persona.public_key(),
        &runtime.pubkey(),
        unix_now(),
    )
    .sign_with_keys(persona)
    .map_err(|e| e.to_string())?;
    Ok(runtime.with_attestation(serde_json::to_string(&event).map_err(|e| e.to_string())?))
}

/// The Keep's test identity kept in `dir`: the runtime key (invite links pin
/// it) and the synthetic persona behind it, both created on first use with
/// mode 0600. The attestation is signed again each start.
pub fn persistent_test_identity(dir: &Path) -> Result<Identity, String> {
    ensure_private_dir(dir)?;
    let runtime = race_session::load_identity(dir)
        .map_err(|e| {
            let reason = e.strip_prefix("identity.key is ").or_else(|| e.strip_prefix("identity.key "));
            format!("{IDENTITY_FILE} couldn't be read: {}", reason.unwrap_or(&e))
        })?;
    let file = dir.join(PERSONA_FILE);
    let persona = match std::fs::read_to_string(&file) {
        Ok(text) => Keys::parse(text.trim())
            .map_err(|e| format!("{PERSONA_FILE} couldn't be read: not a valid key ({e})"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let keys = Keys::generate();
            write_private_atomic(&file, keys.secret_key().to_secret_hex().as_bytes())?;
            keys
        }
        Err(e) => return Err(format!("{PERSONA_FILE} couldn't be read: {e}")),
    };
    sign_identity(runtime, &persona)
}

/// The Keep's host secret key, from the `identity.key` Fray keeps in `dir`. The
/// Keep reads and answers owner messages (NIP-44) as this key, the one in its
/// `stampede://keep/<key>/...` links. Refuses a file that is not the identity's key.
pub fn host_keys(dir: &Path, identity: &Identity) -> Result<Keys, String> {
    let text = std::fs::read_to_string(dir.join(IDENTITY_FILE))
        .map_err(|e| format!("{IDENTITY_FILE} couldn't be read: {e}"))?;
    let keys = Keys::parse(text.trim()).map_err(|_| format!("{IDENTITY_FILE} couldn't be read: not a valid key"))?;
    if keys.public_key() != identity.pubkey() {
        return Err(format!("{IDENTITY_FILE} is not the key this Keep runs with"));
    }
    Ok(keys)
}

/// Print the pairing link on stderr, once, while the Keep has no owner.
///
/// THE ONE DELIBERATE EXCEPTION to the log rule that nothing secret is printed:
/// the link carries a one-time code that lets whoever holds it make themselves
/// the Keep's owner, and the log on the Keep's own box is the one place that
/// reaches only the person who runs it (under the Proton compose the status
/// page's requests come from the docker bridge, not loopback). It is written
/// straight to stderr, not through [`redact`] (which would cut the 64-digit
/// key), and it is the only line that is. The code is gone once the Keep is
/// claimed and is new at every start. Nothing else prints it: not stdout, not
/// `/status.json`, not the page for anyone but loopback.
pub fn print_pairing_link(link: &str) {
    eprintln!("{}", pairing_line(link));
}

/// The line [`print_pairing_link`] prints.
fn pairing_line(link: &str) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{} INFO  stampede_headless::owner Make this your Keep: open My Signet, Settings, My Keep, Add a Keep, and paste {link}", clock_stamp(now))
}

/// How long the relay must have been up before the code is shown: the app's
/// `CODE_GRACE_MS` (app/src/online/mod.rs), so a code is not offered a moment
/// before the relay can carry its first offer.
pub const CODE_GRACE_MS: u64 = 1200;

/// How long the results stay up when nobody starts a rematch.
pub const RESULTS_HOLD_MS: u64 = 30_000;

/// Tracks the relay connection: the code is ready once at least one relay has
/// been connected for [`CODE_GRACE_MS`] without a break.
#[derive(Default)]
pub struct RelayGrace {
    up_since: Option<u64>,
}

impl RelayGrace {
    /// Feed the connected relay count at `now_ms`; true while the code is ready.
    pub fn update(&mut self, relays_up: usize, now_ms: u64) -> bool {
        if relays_up == 0 {
            self.up_since = None;
            return false;
        }
        let since = *self.up_since.get_or_insert(now_ms);
        now_ms.saturating_sub(since) >= CODE_GRACE_MS
    }
}

/// Whether a lobby starts its race: at least `players` humans are in and every
/// human's seat is ready (bots need no ready).
pub fn may_start(humans: usize, human_ready: &[bool], players: usize) -> bool {
    humans > 0 && humans >= players && human_ready.iter().all(|ready| *ready)
}

/// The pause before the next try to host again, after `attempt` failures:
/// 0.3 s, doubling, at most 5 s.
pub fn retry_delay(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(5);
    Duration::from_millis(300u64 << doublings).min(Duration::from_secs(5))
}

/// Try until `attempt` works. After each failure `failed` hears the reason and
/// the number of failures, then `wait` pauses for [`retry_delay`]. `None` only
/// once `stop` says so: a room that cannot reopen is retried, not given up on.
pub fn retry<T>(
    mut attempt: impl FnMut() -> Result<T, String>,
    mut failed: impl FnMut(&str, u32),
    mut wait: impl FnMut(Duration),
    stop: impl Fn() -> bool,
) -> Option<T> {
    let mut failures = 0u32;
    loop {
        match attempt() {
            Ok(value) => return Some(value),
            Err(_) if stop() => return None,
            Err(reason) => {
                failures = failures.saturating_add(1);
                failed(&reason, failures);
                wait(retry_delay(failures));
                if stop() {
                    return None;
                }
            }
        }
    }
}

/// Decides when a room that is not in the lobby goes back to it, so its code
/// is joinable again (a room only takes joiners in the lobby).
#[derive(Default)]
pub struct LobbyReturn {
    results_since: Option<u64>,
}

impl LobbyReturn {
    /// Feed the room's state at `now_ms`; `Some(reason)` when it should go
    /// back to the lobby now: `room_empty` (everyone left) or
    /// `results_timeout` (no rematch within [`RESULTS_HOLD_MS`] of the results).
    pub fn update(&mut self, phase: HostPhase, humans: usize, now_ms: u64) -> Option<&'static str> {
        if phase != HostPhase::Results {
            self.results_since = None;
        }
        if phase == HostPhase::Lobby {
            return None;
        }
        if humans == 0 {
            self.results_since = None;
            return Some("room_empty");
        }
        if phase != HostPhase::Results {
            return None;
        }
        let since = *self.results_since.get_or_insert(now_ms);
        (now_ms.saturating_sub(since) >= RESULTS_HOLD_MS).then_some("results_timeout")
    }
}

// ─── announced addresses ───────────────────────────────────────────────────

/// The line Fray logs when a host starts listening, with every candidate it
/// announces. The host handle has no accessor for them, so this reads that line.
const LISTENING: &str = "[host] listening on";

static LISTENING_LINE: Mutex<Option<String>> = Mutex::new(None);

/// Crates whose log lines are shown by default: Fray (`fray_net`, `fray_relays`), the
/// race protocol and this binary. Other crates' lines (the Signet client and the like)
/// only appear when `RUST_LOG` names them.
const DEFAULT_TARGETS: [&str; 4] = ["fray_net", "fray_relays", "race_session", "stampede_headless"];

/// How many info (or finer) lines a second may print before the rest are counted instead.
/// Warnings and errors are never held back.
const LINES_PER_SECOND: u32 = 40;

/// Which log lines to print: `RUST_LOG` as `level` or `target=level`, comma separated
/// (`off`, `error`, `warn`, `info`, `debug`, `trace`). Without it, info for
/// [`DEFAULT_TARGETS`].
#[derive(Debug, PartialEq)]
struct Filter {
    /// A bare level: applies to every target (else only [`DEFAULT_TARGETS`], at info).
    all: Option<log::LevelFilter>,
    /// `target=level`; the longest matching prefix wins.
    targets: Vec<(String, log::LevelFilter)>,
}

impl Filter {
    fn parse(spec: Option<&str>) -> Filter {
        let mut filter = Filter { all: None, targets: Vec::new() };
        for part in spec.unwrap_or("").split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.split_once('=') {
                Some((target, level)) => {
                    if let Ok(level) = level.trim().parse() {
                        filter.targets.push((target.trim().replace('-', "_"), level));
                    }
                }
                None => {
                    if let Ok(level) = part.parse() {
                        filter.all = Some(level);
                    }
                }
            }
        }
        filter
    }

    fn level_for(&self, target: &str) -> log::LevelFilter {
        let under = |t: &str| target == t || target.strip_prefix(t).is_some_and(|rest| rest.starts_with("::"));
        if let Some((_, level)) = self.targets.iter().filter(|(t, _)| under(t)).max_by_key(|(t, _)| t.len()) {
            return *level;
        }
        match self.all {
            Some(level) => level,
            None if DEFAULT_TARGETS.iter().any(|t| under(t)) => log::LevelFilter::Info,
            None => log::LevelFilter::Off,
        }
    }

    /// The finest level any line may have, for `log::set_max_level`.
    fn finest(&self) -> log::LevelFilter {
        self.targets.iter().map(|(_, l)| *l).chain(self.all).chain([log::LevelFilter::Info]).max().unwrap_or(log::LevelFilter::Info)
    }
}

/// Hide what a log line must never carry: the room code a host announces, and any run of
/// 48 or more hex digits (an invite secret is 64), which is cut to its first eight.
/// Invite ids (32 hex digits) and Keep IDs are safe to print and stay whole.
fn redact(message: &str) -> String {
    const CODE: &str = ", code ";
    let mut text = message.to_string();
    if let Some(at) = text.find(CODE) {
        let start = at + CODE.len();
        let end = text[start..].find(|c: char| c == ',' || c.is_whitespace()).map_or(text.len(), |n| start + n);
        text.replace_range(start..end, "[hidden]");
    }
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 48 {
            out.push_str(&run[..8]);
            out.push('~');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// `12:03:41.123Z`, the time of day in UTC.
fn clock_stamp(now: Duration) -> String {
    let secs = now.as_secs() % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}Z", secs / 3600, secs % 3600 / 60, secs % 60, now.subsec_millis())
}

/// One second's count of printed lines, so a noisy peer cannot fill the log.
struct Budget {
    second: u64,
    printed: u32,
    held_back: u32,
}

struct Stderr {
    filter: Filter,
    budget: Mutex<Budget>,
}

impl log::Log for Stderr {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // The listening line is always read, whatever is printed.
        metadata.level() <= self.filter.level_for(metadata.target()) || (metadata.level() <= log::Level::Info && metadata.target().starts_with("fray_net"))
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        if message.starts_with(LISTENING) {
            if let Ok(mut line) = LISTENING_LINE.lock() {
                *line = Some(message.clone());
            }
        }
        if record.level() > self.filter.level_for(record.target()) {
            return;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let mut held = 0;
        if record.level() > log::Level::Warn {
            let Ok(mut budget) = self.budget.lock() else { return };
            if budget.second != now.as_secs() {
                held = std::mem::take(&mut budget.held_back);
                budget.second = now.as_secs();
                budget.printed = 0;
            }
            if budget.printed >= LINES_PER_SECOND {
                budget.held_back += 1;
                return;
            }
            budget.printed += 1;
        }
        if held > 0 {
            eprintln!("{} INFO  stampede_headless [log] {held} lines held back in the last second", clock_stamp(now));
        }
        eprintln!("{} {:5} {} {}", clock_stamp(now), record.level(), record.target(), redact(&message));
    }
    fn flush(&self) {}
}

/// Print Fray's and the race protocol's log lines on stderr (info by default, or as
/// `RUST_LOG` says), and read the host's listening line. Once per process; a second
/// call (another logger already set) changes nothing.
pub fn capture_announced() {
    let filter = Filter::parse(std::env::var("RUST_LOG").ok().as_deref());
    let finest = filter.finest();
    let logger = Box::leak(Box::new(Stderr { filter, budget: Mutex::new(Budget { second: 0, printed: 0, held_back: 0 }) }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(finest);
    }
}

/// Forget the last room's line, before the next room starts.
pub fn forget_announced() {
    if let Ok(mut line) = LISTENING_LINE.lock() {
        *line = None;
    }
}

/// The `(kind, address)` pairs of the last listening line; empty if it has
/// not been logged (or Fray words it differently).
pub fn announced() -> Vec<(String, String)> {
    LISTENING_LINE
        .lock()
        .ok()
        .and_then(|line| line.as_deref().map(parse_candidates))
        .unwrap_or_default()
}

/// The candidates in a listening line, which prints them as
/// `Candidate { kind: "stun", addr: "203.0.113.9:47800" }`.
pub fn parse_candidates(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("kind: \"") {
        rest = &rest[at + 7..];
        let Some(end) = rest.find('"') else { break };
        let kind = &rest[..end];
        rest = &rest[end..];
        let Some(at) = rest.find("addr: \"") else { break };
        rest = &rest[at + 7..];
        let Some(end) = rest.find('"') else { break };
        out.push((kind.to_string(), rest[..end].to_string()));
        rest = &rest[end..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("keep-{name}-{}-{}", std::process::id(), unix_now()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_test_identity_survives_a_restart_and_is_private() {
        let dir = scratch("identity");
        let first = persistent_test_identity(&dir).unwrap();
        let again = persistent_test_identity(&dir).unwrap();
        assert_eq!(first.pubkey(), again.pubkey());
        // The same persona signs both, and the attestation is for this key.
        let persona = |id: &Identity| {
            decented_signet::verify_attestation(APP_TAG, id.attestation().unwrap().as_bytes(), &id.pubkey(), unix_now())
                .unwrap()
                .persona
        };
        assert_eq!(persona(&first), persona(&again));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in ["identity.key", PERSONA_FILE] {
                let mode = std::fs::metadata(dir.join(file)).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{file}");
            }
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let other = persistent_test_identity(&scratch("other")).unwrap();
        assert_ne!(other.pubkey(), first.pubkey());
        std::fs::write(dir.join(PERSONA_FILE), "not a key").unwrap();
        assert!(persistent_test_identity(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_key_file_is_named_and_left_alone() {
        for (file, text) in [(PERSONA_FILE, "not a key"), (IDENTITY_FILE, "zz-not-hex")] {
            let dir = scratch(&format!("damaged-{file}"));
            persistent_test_identity(&dir).unwrap();
            std::fs::write(dir.join(file), text).unwrap();
            let error = persistent_test_identity(&dir).err().expect(file);
            assert!(error.starts_with(&format!("{file} couldn't be read: ")), "{error}");
            assert!(!error.contains(dir.to_str().unwrap()), "{error}");
            assert_eq!(std::fs::read_to_string(dir.join(file)).unwrap(), text, "never overwritten");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_is_synced_after_a_write() {
        let dir = scratch("syncdir");
        ensure_private_dir(&dir).unwrap();
        assert!(sync_parent(&dir.join("table.json")).is_ok());
        assert!(sync_parent(Path::new("table.json")).is_ok(), "a bare name syncs the current directory");
        assert!(sync_parent(&dir.join("gone").join("table.json")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_no_temp() {
        let dir = scratch("atomic");
        ensure_private_dir(&dir).unwrap();
        let file = dir.join("table.json");
        write_private_atomic(&file, b"one").unwrap();
        write_private_atomic(&file, b"two, longer").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two, longer");
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("table.json")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // A directory that is not there is an error, not a panic.
        assert!(write_private_atomic(&dir.join("gone").join("x"), b"x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_code_waits_a_grace_after_the_first_relay() {
        let mut grace = RelayGrace::default();
        assert!(!grace.update(0, 0));
        assert!(!grace.update(1, 100));
        assert!(!grace.update(1, 1299));
        assert!(grace.update(1, 1300));
        assert!(grace.update(3, 5000));
    }

    #[test]
    fn losing_every_relay_restarts_the_grace() {
        let mut grace = RelayGrace::default();
        assert!(!grace.update(1, 0));
        assert!(grace.update(1, 1200));
        assert!(!grace.update(0, 1300));
        assert!(!grace.update(2, 1400));
        assert!(!grace.update(2, 2599));
        assert!(grace.update(2, 2600));
    }

    #[test]
    fn results_return_to_the_lobby_after_the_hold() {
        let mut back = LobbyReturn::default();
        assert_eq!(back.update(HostPhase::Lobby, 2, 0), None);
        assert_eq!(back.update(HostPhase::Racing, 2, 1_000), None);
        assert_eq!(back.update(HostPhase::Results, 2, 50_000), None);
        assert_eq!(back.update(HostPhase::Results, 2, 79_999), None);
        assert_eq!(back.update(HostPhase::Results, 2, 80_000), Some("results_timeout"));
        // Still asked while the room has not gone back yet.
        assert_eq!(back.update(HostPhase::Results, 2, 80_004), Some("results_timeout"));
        assert_eq!(back.update(HostPhase::Lobby, 2, 80_008), None);
    }

    #[test]
    fn a_rematch_resets_the_hold() {
        let mut back = LobbyReturn::default();
        assert_eq!(back.update(HostPhase::Results, 2, 0), None);
        assert_eq!(back.update(HostPhase::Results, 2, 29_999), None);
        assert_eq!(back.update(HostPhase::Racing, 2, 30_500), None);
        assert_eq!(back.update(HostPhase::Results, 2, 100_000), None);
        assert_eq!(back.update(HostPhase::Results, 2, 129_999), None);
        assert_eq!(back.update(HostPhase::Results, 2, 130_000), Some("results_timeout"));
    }

    #[test]
    fn an_empty_room_goes_back_at_once() {
        let mut back = LobbyReturn::default();
        assert_eq!(back.update(HostPhase::Results, 0, 10), Some("room_empty"));
        assert_eq!(back.update(HostPhase::Racing, 0, 20), Some("room_empty"));
        assert_eq!(back.update(HostPhase::Lobby, 0, 30), None);
        // An empty lobby is where it should be.
        assert_eq!(back.update(HostPhase::Lobby, 0, 40_000), None);
        // The hold counts from the results, not from an earlier race.
        assert_eq!(back.update(HostPhase::Racing, 1, 50_000), None);
        assert_eq!(back.update(HostPhase::Results, 1, 51_000), None);
        assert_eq!(back.update(HostPhase::Results, 1, 80_999), None);
        assert_eq!(back.update(HostPhase::Results, 1, 81_000), Some("results_timeout"));
    }

    #[test]
    fn candidates_come_out_of_the_listening_line() {
        let line = "[host] listening on Some([::]:47800) (online None), code KWTQPM, candidates [Candidate { kind: \"lan\", addr: \"192.168.1.5:47800\" }, Candidate { kind: \"stun\", addr: \"203.0.113.9:47800\" }]";
        assert_eq!(
            parse_candidates(line),
            [
                ("lan".to_string(), "192.168.1.5:47800".to_string()),
                ("stun".to_string(), "203.0.113.9:47800".to_string())
            ]
        );
        assert!(parse_candidates("candidates []").is_empty());
        assert!(parse_candidates("kind: \"lan\" and no address").is_empty());
        assert!(parse_candidates("kind: \"lan").is_empty());
    }

    #[test]
    fn a_race_starts_when_enough_humans_are_all_ready() {
        // Online default: one ready human.
        assert!(may_start(1, &[true], 1));
        assert!(may_start(2, &[true, true], 1));
        // Everyone must be ready.
        assert!(!may_start(2, &[true, false], 1));
        assert!(!may_start(1, &[false], 1));
        // Not before `players` humans are in.
        assert!(!may_start(1, &[true], 2));
        assert!(may_start(2, &[true, true], 2));
        assert!(!may_start(7, &[true; 7], 8));
        assert!(may_start(8, &[true; 8], 8));
        // An empty lobby never starts.
        assert!(!may_start(0, &[], 1));
    }

    #[test]
    fn hosting_again_retries_with_a_growing_pause_until_it_works() {
        assert_eq!(retry_delay(1), Duration::from_millis(300));
        assert_eq!(retry_delay(2), Duration::from_millis(600));
        assert_eq!(retry_delay(3), Duration::from_millis(1200));
        assert_eq!(retry_delay(6), Duration::from_millis(5000));
        assert_eq!(retry_delay(1000), Duration::from_secs(5));
        let (mut tries, mut told, mut waited) = (0, Vec::new(), Vec::new());
        let room = retry(
            || {
                tries += 1;
                if tries <= 25 { Err(format!("port busy {tries}")) } else { Ok("room") }
            },
            |reason, n| told.push((reason.to_string(), n)),
            |pause| waited.push(pause),
            || false,
        );
        // Far past a minute of waiting, and still it goes on.
        assert_eq!(room, Some("room"));
        assert_eq!((told.len(), waited.len()), (25, 25));
        assert_eq!(told[0], ("port busy 1".to_string(), 1));
        assert_eq!(told[24].1, 25);
        assert!(waited.iter().sum::<Duration>() > Duration::from_secs(60));
        assert!(waited.iter().all(|pause| *pause <= Duration::from_secs(5)));
    }

    #[test]
    fn a_stop_ends_the_retries() {
        let stopped = std::cell::Cell::new(false);
        let told = std::cell::Cell::new(0);
        let room: Option<()> = retry(
            || Err("busy".to_string()),
            |_, _| told.set(told.get() + 1),
            |_| {
                if told.get() == 3 {
                    stopped.set(true);
                }
            },
            || stopped.get(),
        );
        assert_eq!((room, told.get()), (None, 3));
        // A stop while an attempt fails does not report it as a failure.
        let mut told = 0;
        let room: Option<()> = retry(|| Err("x".into()), |_, _| told += 1, |_| {}, || true);
        assert_eq!((room, told), (None, 0));
    }

    #[test]
    fn rust_log_picks_what_is_printed() {
        use log::LevelFilter::*;
        let none = Filter::parse(None);
        assert_eq!(none.level_for("fray_net::host"), Info);
        assert_eq!(none.level_for("race_session::net"), Info);
        assert_eq!(none.level_for("decented_signet::signet"), Off, "other crates stay quiet");
        assert_eq!(none.finest(), Info);
        let some = Filter::parse(Some("warn, fray_net::host=trace,garbage=loud,fray-relays=off"));
        assert_eq!(some.level_for("fray_net::host"), Trace);
        assert_eq!(some.level_for("fray_net::quic"), Warn);
        assert_eq!(some.level_for("fray_relays"), Off);
        assert_eq!(some.level_for("decented_signet"), Warn);
        assert_eq!(some.finest(), Trace);
        assert_eq!(Filter::parse(Some("off")).level_for("fray_net"), Off);
    }

    #[test]
    fn log_lines_never_carry_a_room_code_or_a_secret() {
        let secret = "ab".repeat(32);
        let id = "cd".repeat(16);
        let line = format!("[host] listening on Some(0.0.0.0:4433) (online None), code KWTQPM, candidates [x] secret {secret} id {id}");
        let shown = redact(&line);
        assert!(!shown.contains("KWTQPM") && !shown.contains(&secret), "{shown}");
        assert!(shown.contains("code [hidden],") && shown.contains("abababab~") && shown.contains(&id), "{shown}");
        assert_eq!(redact("[host] listening on virtual links, code ABCDEF"), "[host] listening on virtual links, code [hidden]");
        assert_eq!(redact("[join] LAN connect to 10.0.0.2:4433: timed out"), "[join] LAN connect to 10.0.0.2:4433: timed out");
        // A long hex run is cut anywhere in a line, as is one in capitals.
        assert_eq!(redact(&"A".repeat(64)), "AAAAAAAA~");
        // The one named exception to "no secret in the log": the pairing link of an
        // unclaimed Keep (`print_pairing_link`), printed straight to stderr without
        // `redact`. Everything that does go through the logger is redacted, so the
        // link, which `redact` would cut, never comes out of it.
        let link = format!("signet-keep:pair?k={}&c={}", "ab".repeat(32), "cd".repeat(16));
        assert!(!redact(&link).contains(&"ab".repeat(32)));
    }

    #[test]
    fn owner_message_lines_carry_no_secret_code_link_or_whole_key() {
        use crate::owner::{ack_sent_line, message_line, Ack, Source, Subject, KIND_ACCEPT, KIND_CLAIM, KIND_INVITE, KIND_REVOKE, KIND_UPDATE};
        use nostr::{EventBuilder, Kind, Tag, Timestamp};
        let sender = Keys::parse(&format!("{}2", "0".repeat(63))).unwrap().public_key();
        let (secret, code, grant) = ("ab".repeat(32), "cd".repeat(16), "ef".repeat(16));
        let link = format!("signet-keep:pair?k={}&c={code}&n=Home&r=wss%3A%2F%2Fr.example", "12".repeat(32));
        let tag = |parts: &[&str]| Tag::parse(parts.iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap();
        let acks = [
            None,
            Some(Ack { to: sender, status: "paired", subject: Subject::Claim, reason: None }),
            Some(Ack { to: sender, status: "refused", subject: Subject::Grant(grant.clone()), reason: Some("conflict") }),
            Some(Ack { to: sender, status: "refused", subject: Subject::Rev(4), reason: Some("stale") }),
        ];
        for kind in [KIND_CLAIM, KIND_INVITE, KIND_REVOKE, KIND_UPDATE, KIND_ACCEPT] {
            // Every tag a message can carry holds something that must not be printed.
            let tags = vec![
                tag(&["code", &code]),
                tag(&["secret", &secret]),
                tag(&["grant", &grant]),
                tag(&["name", &link]),
                tag(&["rev", "4"]),
            ];
            let rumor = EventBuilder::new(Kind::Custom(kind), &link).tags(tags).custom_created_at(Timestamp::from_secs(1)).build(sender);
            for ack in &acks {
                let line = message_line(&rumor, &sender, Source::Live, ack.as_ref()).expect("a line");
                assert!(line.contains(&sender.to_hex()[..6]) && !line.contains(&sender.to_hex()), "{line}");
                for hidden in [&secret, &code, &grant, &link] {
                    assert!(!line.contains(hidden.as_str()), "{line}");
                }
                // What the logger prints is what was built: nothing in it for `redact` to cut.
                assert_eq!(redact(&line), line);
            }
        }
        let reply = EventBuilder::new(Kind::GiftWrap, "").tag(tag(&["p", &sender.to_hex()])).sign_with_keys(&Keys::generate()).unwrap();
        let line = ack_sent_line(&reply, 3);
        assert!(line.contains(&sender.to_hex()[..6]) && !line.contains(&sender.to_hex()) && redact(&line) == line, "{line}");
    }

    #[test]
    fn the_pairing_link_is_the_one_line_that_is_not_redacted() {
        // The exception to the rule above, on purpose: the link carries the Keep's
        // 64-digit key and a one-time code, and the log on the Keep's box is how the
        // person who runs it gets them. `redact` would cut the key, so it is printed
        // without it; nothing else is.
        let link = "signet-keep:pair?k=".to_string() + &"ab".repeat(32) + "&c=" + &"cd".repeat(16) + "&n=Home&r=wss%3A%2F%2Fr.example";
        let line = pairing_line(&link);
        assert!(line.contains(&link), "{line}");
        assert_ne!(redact(&line), line, "redact would hide the key");
        assert!(!pairing_line("x").contains('\n') && line.matches("signet-keep:pair").count() == 1);
    }

    #[test]
    fn the_host_key_is_read_from_the_identity_file_and_must_match() {
        let dir = scratch("hostkey");
        let identity = persistent_test_identity(&dir).unwrap();
        let keys = host_keys(&dir, &identity).unwrap();
        assert_eq!(keys.public_key(), identity.pubkey());
        let other = persistent_test_identity(&scratch("hostkey-other")).unwrap();
        assert!(host_keys(&dir, &other).unwrap_err().contains("not the key"));
        std::fs::write(dir.join(IDENTITY_FILE), "zz").unwrap();
        assert!(host_keys(&dir, &identity).unwrap_err().contains("not a valid key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stamp_is_a_utc_time_of_day() {
        assert_eq!(clock_stamp(Duration::from_millis(86_400_000 + 3_723_004)), "01:02:03.004Z");
    }
}
