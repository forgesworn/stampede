//! Keeps a player has joined by invite, and the text around an invite link.
//!
//! A Keep is a host that stays up (see `docs/api/race-session.md`, "Invites").
//! A player joins one with a link, `stampede://keep/<host key>/<secret>`, and
//! afterwards rejoins it from the Multiplayer screen's My Keeps list with no
//! link and no room code. The list lives in `keeps.json` in the app's data
//! directory: the secrets are the way in, so the file is private to the app
//! (mode 0600 where the platform has modes, and the app's own directory on
//! Android) and is replaced atomically.
//!
//! Everything here is pure or touches only `keeps.json`; the screens do the
//! rest.

use fray_keep::Keeps;
use race_session::invite::{INVITE_LINK_PREFIX};
use std::path::{Path, PathBuf};

// The saved-Keeps rules (the file, the list, the names) are Fray's; the game
// keeps its own names for them.
pub use fray_keep::keeps::{contains, forget, remember};
pub use fray_keep::{ago, display_name_hex as display_name, is_invisible, keep_id_hex as keep_id, Keep, MAX_KEEPS};

/// The file in the data directory.
pub const FILE: &str = "keeps.json";

/// What an invite join says when the Keep cannot be reached. The link may have
/// been used up, expired or revoked (a Keep answers none of those), or the
/// Keep may be off: the player cannot tell which, so the text names them all.
pub const UNREACHABLE: &str = "Couldn't reach that Keep. The invite may have been used, expired or revoked, or the Keep is offline.";
/// A `stampede://keep/` link that is not a whole, well-formed invite.
pub const NOT_AN_INVITE: &str = "That isn't a Stampede invite link.";
/// Paste invite, and the clipboard holds no invite link at all.
pub const NO_INVITE_ON_CLIPBOARD: &str = "No invite link on the clipboard. Copy the link your friend sent, then tap Paste invite.";

/// Longest name kept, as Fray's crate cuts it: the tests check it.
#[cfg(test)]
const MAX_NAME: usize = 40;

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Read `keeps.json`. The second value is where Fray's crate moved the file when it was there
/// but unreadable (the first free `keeps.json.bad`, `.bad.2`, ...), so the caller can say so in
/// the log.
fn open(dir: &Path) -> (std::io::Result<Keeps>, Option<PathBuf>) {
    let keeps = Keeps::load(path(dir));
    let set_aside = keeps.as_ref().ok().and_then(|k| k.set_aside()).map(Path::to_path_buf);
    (keeps, set_aside)
}

/// [`open`], and a log line when the file was set aside. The line names the file, never what is in it.
fn open_logged(dir: &Path) -> std::io::Result<Keeps> {
    let (keeps, set_aside) = open(dir);
    if let Some(moved) = set_aside {
        log::warn!("keeps: {} is unreadable; kept as {}, starting an empty list", path(dir).display(), moved.display());
    }
    keeps
}

/// The saved Keeps, newest play first. A missing file is an empty list. An
/// unreadable one is kept as `keeps.json.bad`, or the next free `.bad.N` (never lost), logged, and reads as empty;
/// an entry whose keys are not hex is dropped.
pub fn load(dir: &Path) -> Vec<Keep> {
    match open_logged(dir) {
        Ok(keeps) => keeps.list().to_vec(),
        Err(e) => {
            log::warn!("keeps: {} is unreadable ({e}); starting an empty list", path(dir).display());
            Vec::new()
        }
    }
}

/// Note a successful join: load, [`remember`], save. `Ok(true)` when the file was
/// written, `Ok(false)` when the list came out as it already was (nothing to
/// write). The error is for the log; a Keep that could not be saved does not stop
/// the game.
pub fn record_join(dir: &Path, host_key: &[u8; 32], secret: &[u8; 32], host_name: Option<&str>, now: u64) -> Result<bool, String> {
    let mut keeps = open_logged(dir).map_err(|e| e.to_string())?;
    keeps.record_join(host_key, secret, host_name, now).map_err(|e| e.to_string())
}

/// Take a Keep out of `keeps.json`.
pub fn forget_saved(dir: &Path, host_key: &str) -> Result<(), String> {
    // A key that is not hex names no saved Keep: nothing to take out.
    let Some(key) = fray_keep::parse_hex32(host_key) else { return Ok(()) };
    let mut keeps = open_logged(dir).map_err(|e| e.to_string())?;
    keeps.forget(&key).map(|_| ()).map_err(|e| e.to_string())
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    crate::update::unix_now()
}

// ─── What a link or a paste asks for ────────────────────────────────────────

/// What a launch link or a pasted line asks the app to join. `Debug` never
/// prints an invite's secret.
#[derive(Clone, PartialEq, Eq)]
pub enum JoinTarget {
    /// A room code (`stampede://join#CODE`, or typed).
    Code(String),
    /// A Keep: the two halves of an invite link.
    Invite { host_key: [u8; 32], secret: [u8; 32] },
    /// A `stampede://keep/` link that does not parse: the join screen says so.
    BadInvite,
}

impl std::fmt::Debug for JoinTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JoinTarget({})", describe(self))
    }
}

impl JoinTarget {
    /// For "Leave this room and join ...?" and the race screen's note.
    pub fn label(&self) -> String {
        match self {
            JoinTarget::Code(c) => crate::online::spaced_code(c),
            JoinTarget::Invite { host_key, .. } => format!("Keep {}", fray_keep::keep_id(host_key)),
            JoinTarget::BadInvite => "that Keep".into(),
        }
    }
}

/// A `stampede://keep...` token as an invite (its prefix may be in any case).
fn invite_from(token: &str) -> JoinTarget {
    match fray_keep::parse_invite_link(INVITE_LINK_PREFIX, token) {
        Some(invite) => JoinTarget::Invite { host_key: invite.host_key, secret: invite.secret },
        None => JoinTarget::BadInvite,
    }
}

/// What a launch link (the OS handed the app a `stampede:` URL) asks for:
/// `stampede://join#CODE`, `stampede://keep/<host>/<secret>` (a bad one is
/// [`JoinTarget::BadInvite`]), or nothing.
pub fn parse_launch_link(link: &str) -> Option<JoinTarget> {
    if let Some(code) = crate::platform::parse_join_link(link) {
        return Some(JoinTarget::Code(code));
    }
    let t = link.trim();
    fray_keep::is_link_word(INVITE_LINK_PREFIX, t).then(|| invite_from(t))
}

/// The invite link in pasted text: the first word that starts with
/// `stampede://keep`, so a message like "Join my Keep: stampede://keep/..." works.
/// `None` when there is none; a word that is not a whole invite is
/// [`JoinTarget::BadInvite`].
pub fn invite_in_text(text: &str) -> Option<JoinTarget> {
    fray_keep::invite_in_text(INVITE_LINK_PREFIX, text).map(invite_from)
}

/// The link text of a target, for the log (never the secret).
pub fn describe(target: &JoinTarget) -> String {
    match target {
        JoinTarget::Code(c) => format!("room {c}"),
        JoinTarget::Invite { host_key, .. } => format!("Keep {}", fray_keep::describe(host_key)),
        JoinTarget::BadInvite => "a bad invite link".into(),
    }
}

/// How an invite join failed, in the player's words. Before the player is
/// seated, a Keep that is not found, never answers, turns the joiner away
/// (the host seems to leave) or drops the link all look alike from here, so
/// they share [`UNREACHABLE`]; anything else (no sign-in, full, wrong version)
/// keeps its own text. `why` is the join screen's failure text.
pub fn invite_failure_text(why: &str) -> String {
    const LIKE_NOT_FOUND: [&str; 7] = [
        race_session::INVITE_NO_ANSWER,
        "Code not found",
        "didn't let us in",
        "didn't answer over the link",
        "That's taking too long",
        "The host left the game.",
        "Lost the connection to the host.",
    ];
    if LIKE_NOT_FOUND.iter().any(|m| why.contains(m)) {
        UNREACHABLE.into()
    } else {
        why.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use race_session::format_invite_link;

    const HOST: [u8; 32] = [0xab; 32];
    const SECRET: [u8; 32] = [0x01; 32];

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stampede-keeps-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn keep(n: u8, at: u64) -> Keep {
        Keep::new(&[n; 32], &[n.wrapping_add(1); 32], Some(&format!("Keep number {n}")), at)
    }

    /// Save a Keep the way the game does: read the file, add it, write.
    fn join(d: &Path, n: u8, at: u64) {
        record_join(d, &[n; 32], &[n.wrapping_add(1); 32], Some(&format!("Keep number {n}")), at).unwrap();
    }

    #[test]
    fn a_keep_without_a_name_is_named_after_its_key() {
        assert_eq!(Keep::new(&HOST, &SECRET, None, 5).name, "Keep ababab");
        assert_eq!(Keep::new(&HOST, &SECRET, Some("  \n"), 5).name, "Keep ababab");
        assert_eq!(Keep::new(&HOST, &SECRET, Some(" Cousin's \u{7}Keep "), 5).name, "Cousin's Keep");
        assert_eq!(Keep::new(&HOST, &SECRET, Some(&"x".repeat(200)), 5).name.chars().count(), MAX_NAME);
        let k = Keep::new(&HOST, &SECRET, Some("Cousin"), 9);
        assert_eq!(k.host_key, "ab".repeat(32));
        assert_eq!(k.secret, "01".repeat(32));
        assert_eq!(k.keys(), Some((HOST, SECRET)));
    }

    #[test]
    fn saved_keeps_load_back_and_a_missing_file_is_empty() {
        let d = dir("roundtrip");
        assert!(load(&d).is_empty());
        join(&d, 1, 100);
        join(&d, 2, 200);
        assert_eq!(load(&d), vec![keep(2, 200), keep(1, 100)]);
        assert!(!d.join("keeps.json.tmp").exists(), "the temp file is renamed away");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join(FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the file holds secrets");
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_same_keep_with_a_new_secret_replaces_the_old_entry() {
        let mut list = vec![keep(1, 100), keep(2, 200)];
        let again = Keep::new(&[1; 32], &[9; 32], Some("Renamed"), 300);
        remember(&mut list, again.clone());
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], again, "newest first");
        assert_eq!(list[1], keep(2, 200));
        assert_eq!(list.iter().filter(|k| k.host_key == "01".repeat(32)).count(), 1);
    }

    #[test]
    fn the_list_holds_twenty_and_drops_the_oldest() {
        let mut list = Vec::new();
        for i in 0..MAX_KEEPS as u8 {
            remember(&mut list, keep(i, 1000 + u64::from(i)));
        }
        assert_eq!(list.len(), MAX_KEEPS);
        remember(&mut list, keep(100, 5000));
        assert_eq!(list.len(), MAX_KEEPS);
        assert_eq!(list[0], keep(100, 5000));
        assert!(!contains(&list, &"00".repeat(32)), "the oldest, played at 1000, is gone");
        assert!(contains(&list, &"01".repeat(32)));
        // A new Keep older than all twenty does not push a newer one out.
        remember(&mut list, keep(101, 1));
        assert_eq!(list.len(), MAX_KEEPS);
        assert!(!contains(&list, &"65".repeat(32)));
    }

    #[test]
    fn forgetting_removes_one_keep_and_saves() {
        let d = dir("forget");
        record_join(&d, &[1; 32], &[2; 32], Some("One"), 10).unwrap();
        record_join(&d, &[3; 32], &[4; 32], None, 20).unwrap();
        assert_eq!(load(&d).len(), 2);
        forget_saved(&d, &"01".repeat(32)).unwrap();
        let left = load(&d);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, "Keep 030303");
        forget_saved(&d, &"01".repeat(32)).unwrap();
        assert_eq!(load(&d), left, "forgetting an absent Keep changes nothing");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_join_that_changes_nothing_does_not_rewrite_the_file() {
        let d = dir("rewrite");
        assert_eq!(record_join(&d, &[1; 32], &[2; 32], Some("One"), 10), Ok(true));
        let file = d.join(FILE);
        let first = std::fs::read(&file).unwrap();
        // Same Keep, same secret, same second: the list is as it was.
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, &first).unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(record_join(&d, &[1; 32], &[2; 32], Some("One"), 10), Ok(false));
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), mtime);
        assert_eq!(std::fs::read(&file).unwrap(), first);
        // A later play, or a new secret, is a change.
        assert_eq!(record_join(&d, &[1; 32], &[2; 32], Some("One"), 11), Ok(true));
        assert_eq!(record_join(&d, &[1; 32], &[3; 32], Some("One"), 11), Ok(true));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_file_set_aside_is_reported_and_one_that_is_not_is_not() {
        let d = dir("setaside");
        std::fs::create_dir_all(&d).unwrap();
        assert!(open(&d).1.is_none(), "no file, nothing set aside");
        join(&d, 1, 5);
        let (keeps, set_aside) = open(&d);
        assert!(keeps.is_ok() && set_aside.is_none(), "a good file is read in place");
        std::fs::write(d.join(FILE), "[{\"host_key\":1}]").unwrap();
        assert!(open(&d).1.is_none(), "a list whose entries are dropped is not set aside");
        std::fs::write(d.join(FILE), "not json").unwrap();
        let (keeps, set_aside) = open(&d);
        assert!(keeps.unwrap().list().is_empty());
        assert_eq!(set_aside, Some(d.join("keeps.json.bad")));
        assert!(d.join("keeps.json.bad").exists() && !d.join(FILE).exists());
        // A second one is set aside under the next name, and says so.
        std::fs::write(d.join(FILE), "not json either").unwrap();
        let (keeps, set_aside) = open(&d);
        assert!(keeps.unwrap().list().is_empty());
        assert_eq!(set_aside, Some(d.join("keeps.json.bad.2")));
        assert_eq!(std::fs::read_to_string(d.join("keeps.json.bad")).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_broken_file_is_set_aside_and_bad_entries_are_dropped() {
        let d = dir("broken");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(FILE), "not json").unwrap();
        assert!(load(&d).is_empty());
        assert!(d.join("keeps.json.bad").exists());
        let good = keep(1, 5);
        let mut bad = keep(2, 6);
        bad.secret = "zz".into();
        std::fs::write(d.join(FILE), serde_json::to_vec(&vec![bad, good.clone()]).unwrap()).unwrap();
        assert_eq!(load(&d), vec![good]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn times_read_in_plain_words() {
        let now = 10_000_000;
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(now - 600, now), "10 min ago");
        assert_eq!(ago(now - 3 * 3600, now), "3 h ago");
        assert_eq!(ago(now - 86_400, now), "yesterday");
        assert_eq!(ago(now - 4 * 86_400, now), "4 d ago");
        assert_eq!(ago(now - 90 * 86_400, now), "3 mo ago");
        assert_eq!(ago(now + 50, now), "just now", "a clock that went back");
    }

    fn link() -> String {
        format_invite_link(&HOST, &SECRET)
    }

    #[test]
    fn launch_links_dispatch_to_a_room_or_a_keep() {
        assert_eq!(parse_launch_link("stampede://join#kwtqpm"), Some(JoinTarget::Code("KWTQPM".into())));
        assert_eq!(parse_launch_link(&link()), Some(JoinTarget::Invite { host_key: HOST, secret: SECRET }));
        assert_eq!(parse_launch_link(&format!("  {}\n", link())), Some(JoinTarget::Invite { host_key: HOST, secret: SECRET }));
        assert_eq!(parse_launch_link(&link().replacen("stampede://keep", "STAMPEDE://KEEP", 1)), Some(JoinTarget::Invite { host_key: HOST, secret: SECRET }));
        let zero = format!("stampede://keep/{}/{}", "0".repeat(64), "0".repeat(64));
        assert_eq!(parse_launch_link(&zero), Some(JoinTarget::Invite { host_key: [0; 32], secret: [0; 32] }));
        for bad in ["stampede://keep", "stampede://keep/", "stampede://keep/abc/def", &format!("{}/extra", link()), &link()[..link().len() - 1]] {
            assert_eq!(parse_launch_link(bad), Some(JoinTarget::BadInvite), "{bad}");
        }
        for other in ["stampede://keeper/x", "stampede://other#ABC", "https://example.com/keep", "twangthang://join#ABC", ""] {
            assert_eq!(parse_launch_link(other), None, "{other}");
        }
    }

    #[test]
    fn an_invite_is_found_in_pasted_text() {
        let ok = Some(JoinTarget::Invite { host_key: HOST, secret: SECRET });
        assert_eq!(invite_in_text(&link()), ok);
        assert_eq!(invite_in_text(&format!("\n {}  \n", link())), ok);
        assert_eq!(invite_in_text(&format!("Join my Keep: {}.", link())), ok);
        assert_eq!(invite_in_text(&format!("<{}>", link())), ok);
        assert_eq!(invite_in_text(&format!("(see {})", link())), ok);
        assert_eq!(invite_in_text(&format!("try stampede://keep/short and then {}", link())), Some(JoinTarget::BadInvite), "the first Keep link is the one read");
        assert_eq!(invite_in_text("stampede://keep/nope"), Some(JoinTarget::BadInvite));
        assert_eq!(invite_in_text("KWTQPM"), None);
        assert_eq!(invite_in_text("stampede://join#KWTQPM"), None);
        assert_eq!(invite_in_text("https://example.com/keeps"), None);
        assert_eq!(invite_in_text(""), None);
    }

    #[test]
    fn a_host_name_loses_bidi_and_zero_width_characters() {
        assert_eq!(display_name(Some("Co\u{202e}us\u{200b}in\u{feff}"), "abcdef0123"), "Cousin");
        assert_eq!(display_name(Some("\u{202e}\u{200b}"), "abcdef0123"), "Keep abcdef");
        assert_eq!(keep_id("abcdef0123"), "ABCDEF");
    }

    #[test]
    fn targets_name_themselves_without_the_secret() {
        assert_eq!(JoinTarget::Code("KWTQPM".into()).label(), "KWT QPM");
        let t = JoinTarget::Invite { host_key: HOST, secret: SECRET };
        assert_eq!(t.label(), "Keep ABABAB");
        assert_eq!(JoinTarget::BadInvite.label(), "that Keep");
        assert_eq!(describe(&t), "Keep ababab");
        assert!(!describe(&t).contains("0101"));
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let t = JoinTarget::Invite { host_key: HOST, secret: SECRET };
        assert_eq!(format!("{t:?}"), "JoinTarget(Keep ababab)");
        assert!(!format!("{t:?}").contains("0101"));
        assert_eq!(format!("{:?}", JoinTarget::Code("KWTQPM".into())), "JoinTarget(room KWTQPM)");
        assert_eq!(format!("{:?}", JoinTarget::BadInvite), "JoinTarget(a bad invite link)");
    }

    #[test]
    fn an_unreachable_keep_is_never_called_a_missing_code() {
        for why in [
            "Code not found (check the code, or the host may have started).",
            race_session::INVITE_NO_ANSWER,
            race_session::INVITE_NOT_ADMITTED,
            "The host didn't let us in (the code may be out of date).",
            "The host's phone didn't answer over the link.",
            "That's taking too long. Check you are online and the host is still waiting, then try again.",
            "The host left the game.",
            "Lost the connection to the host.",
        ] {
            let text = invite_failure_text(why);
            assert_eq!(text, UNREACHABLE, "{why}");
            assert!(!text.contains("Code not found"));
        }
        assert_eq!(invite_failure_text("Sign in with My Signet to play."), "Sign in with My Signet to play.");
        assert_eq!(invite_failure_text("Room full. Ask the host to make space, or start your own game."), "Room full. Ask the host to make space, or start your own game.");
    }

    #[test]
    fn a_file_written_before_the_move_loads_the_same() {
        // The exact bytes the game's own save wrote before it used Fray's crate:
        // pretty JSON, one entry hand-edited to upper-case hex and a name with a
        // control character and a bidi override (the old loader cleaned both).
        let a = "AB".repeat(32);
        let b = "0f".repeat(32);
        let file = format!(
            "[\n  {{\n    \"host_key\": \"{a}\",\n    \"secret\": \"{}\",\n    \"name\": \"Co\\u0007usin\\u202e's Keep\",\n    \"last_joined\": 100\n  }},\n  {{\n    \"host_key\": \"{b}\",\n    \"secret\": \"{}\",\n    \"name\": \"Keep 0f0f0f\",\n    \"last_joined\": 200\n  }}\n]",
            "CD".repeat(32),
            "01".repeat(32)
        );
        let d = dir("oldformat");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(FILE), file).unwrap();
        let list = load(&d);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], Keep { host_key: b.clone(), secret: "01".repeat(32), name: "Keep 0f0f0f".into(), last_joined: 200 });
        assert_eq!(list[1], Keep { host_key: "ab".repeat(32), secret: "cd".repeat(32), name: "Cousin's Keep".into(), last_joined: 100 });
        assert!(contains(&list, &a), "a host key is matched in either case");
        assert!(!d.join("keeps.json.bad").exists());
        let _ = std::fs::remove_dir_all(d);
    }
}
