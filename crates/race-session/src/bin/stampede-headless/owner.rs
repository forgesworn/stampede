//! The Keep's owner: a one-time claim and owner-signed invites and revokes,
//! sent from My Signet as NIP-59 gift wraps over Nostr relays (the K1d
//! contract, `plans/2026-10-22-k1d-contract.md`).
//!
//! [`Engine`] is the rules, with no clock, relay or thread of its own: a rumor
//! in, an acknowledgement out. [`Owner`] wraps it with a lock (the status page
//! asks whether the Keep is claimed) and, in [`Owner::run`], the relay loop.
//!
//! K1e adds owner personas with revisions (`plans/2026-10-22-k1e-contract.md`):
//! an owners update (27664) and an owner accept (27665), and the gate for
//! invite, revoke and update is "an active owner persona".
//!
//! Files in `--data-dir`, all private and fsynced like the invite table:
//! `owner.json` (the owner personas, their revision and the claim's scope;
//! delete it to reset) and `owner-state.json` (the
//! grants and the wrap cursor).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use decented_signet::relay::{auth_signer, AuthConnector, Connector, Relay, RelayMsg, SubStatus, Subscription};
use nostr::{Event, EventId, Filter, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};
use race_session::invite::{hex_encode, parse_hex32};
use race_session::InviteId;
use serde::{Deserialize, Serialize};

use crate::invites::{self, AddOwnerError, Invites};
use crate::keep::{unix_now, write_private_atomic};
use crate::wire::{self, count_tags, has_tag, one_tag};

pub const OWNER_FILE: &str = "owner.json";
pub const STATE_FILE: &str = "owner-state.json";

pub const KIND_CLAIM: u16 = 27660;
pub const KIND_INVITE: u16 = 27661;
pub const KIND_REVOKE: u16 = 27662;
pub const KIND_ACK: u16 = 27663;
pub const KIND_UPDATE: u16 = 27664;
pub const KIND_ACCEPT: u16 = 27665;

/// The most owner personas a Keep lists.
pub const MAX_PERSONAS: usize = 16;
/// The most accepts held for personas an update has not listed yet.
pub const MAX_PENDING: usize = 32;
/// The largest revision (exact in JavaScript); a higher one is malformed.
pub const MAX_REV: u64 = 1 << 53;
/// How long an accept that came before its update is held.
pub const PENDING_TTL_SECS: u64 = 7 * 86_400;

/// The most an owner invite may run ahead.
pub const MAX_EXPIRY_SECS: u64 = 365 * 86_400;
/// An hour for clocks that disagree (a phone a few minutes slow, with the full
/// jitter, would otherwise stamp a wrap before the window).
pub const CLOCK_SLACK_SECS: u64 = 3_600;
/// How far back the subscription reaches before the cursor (NIP-59 jitter,
/// and [`CLOCK_SLACK_SECS`]). Repeats are harmless.
pub const CURSOR_MARGIN_SECS: u64 = 2 * 86_400 + CLOCK_SLACK_SECS;
/// The first run reads this far back.
pub const FIRST_RUN_SECS: u64 = 3 * 86_400;
/// Grants remembered. A grant is forgotten once its invite's expiration is
/// [`PRUNE_MARGIN_SECS`] past: a replay of that invite is refused `expired`
/// by the expiry rule anyway.
pub const MAX_GRANTS: usize = 4096;
/// How long past an invite's expiration its grant is still remembered.
pub const PRUNE_MARGIN_SECS: u64 = 86_400;
/// The cursor is written to disk when it has moved this far past the saved one.
const CURSOR_SAVE_STEP: u64 = 60;
/// Wraps remembered (by id) so a relay replay after a reconnect is not read
/// twice: enough for a whole start-up read from three relays.
const SEEN_CAP: usize = 3 * (BACKLOG_CAP + 3 * STRANGER_CAP);
/// Wraps asked for in one page of the read at start. Well under the
/// subscription queue (1024), which drops what it cannot hold.
pub const PAGE_LIMIT: usize = 250;
/// The most pages read from one relay at a time (a bigger flood is left).
const MAX_PAGES: usize = 400;
/// How long a page of the read may take to arrive.
const PAGE_WAIT: Duration = if cfg!(test) { Duration::from_millis(800) } else { Duration::from_secs(15) };
/// The most opened messages from listed personas (and the claim with the
/// right code) a start-up read holds. Nobody else can sign as a listed
/// persona, so a flood from strangers cannot push these out; the cap is only a
/// ceiling for a listed key gone bad.
const BACKLOG_CAP: usize = 20_000;
/// The most opened messages from anyone else that a start-up read holds from
/// one relay. When it is full the relay is still read (within the page
/// limits), but no more of these are held.
pub const STRANGER_CAP: usize = 4_000;
/// A message held at start has at most this many tags (an owners update has
/// `keep`, `rev` and up to 16 `persona`) ...
const MAX_RUMOR_TAGS: usize = 24;
/// ... of which at most this many are `persona` (the contract's 16) ...
const MAX_PERSONA_TAGS: usize = MAX_PERSONAS;
/// ... and at most this many are any other kind the Keep reads.
const MAX_OTHER_TAGS: usize = 8;
/// ... each with at most this many parts (the name, the value, and room for a hint) ...
const MAX_TAG_PARTS: usize = 3;
/// ... of at most this many bytes (a key or secret in hex is 64) ...
const MAX_TAG_PART_BYTES: usize = 64;
/// ... except `name` (a label of 40 characters is up to 160 bytes) ...
const MAX_NAME_BYTES: usize = 160;
/// ... and `persona`, whose parts after the key are section names.
const MAX_PERSONA_PARTS: usize = 6;
const MAX_SECTION_BYTES: usize = 32;
/// The tags the Keep reads; the others are not kept in a held message.
const KNOWN_TAGS: [&str; 11] = ["keep", "rev", "persona", "grant", "secret", "expiration", "guest", "p", "name", "code", "scope"];
/// The pairing link carries at most this many relays.
pub const MAX_LINK_RELAYS: usize = 3;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum GrantState {
    Active,
    Revoked,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Grant {
    /// The owner's grant id, 32 hex.
    grant: String,
    /// The Fray invite id (of the secret), 32 hex; empty for a revoke that came first.
    invite: String,
    state: GrantState,
    /// What the invite said, to tell a repeat from a different invite under the
    /// same grant. Empty once revoked.
    #[serde(default)]
    tags: String,
    /// When the invite expires (kept after a revoke). A grant without it
    /// (from an older file) is never pruned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    /// When a revoke that came before its invite was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recorded_at: Option<u64>,
}

impl Grant {
    /// Whether a replay of this grant's invite would be refused `expired`
    /// anyway, so there is no need to remember it. A revoke that came before
    /// its invite is kept for the longest an invite may run, and a day more.
    fn forgettable(&self, now: u64) -> bool {
        match (self.expires_at, self.recorded_at) {
            (Some(expires), _) => expires.saturating_add(PRUNE_MARGIN_SECS) <= now,
            (None, Some(recorded)) => recorded.saturating_add(MAX_EXPIRY_SECS + PRUNE_MARGIN_SECS) <= now,
            (None, None) => false,
        }
    }
}

/// One owner persona as the Keep holds it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Persona {
    /// Hex, lower case.
    persona: String,
    /// The sections switched on for it (unknown names are kept).
    #[serde(default)]
    sections: Vec<String>,
    /// The revision that most recently added it (0 for the claimer).
    added_rev: u64,
    /// The highest accept seen from it; none if it has not accepted.
    #[serde(default)]
    accepted_rev: Option<u64>,
}

impl Persona {
    /// Listed, and has accepted the listing: it may invite, revoke and update.
    fn active(&self) -> bool {
        self.accepted_rev.is_some_and(|accepted| accepted >= self.added_rev)
    }
}

/// An accept that came before the update that lists its persona. Kept in
/// memory only: anyone can send one, so none is written to disk or answered
/// (the owner's My Signet sends it again after a restart).
#[derive(Clone, Debug, PartialEq)]
struct Pending {
    persona: String,
    rev: u64,
    received_at: u64,
}

/// `owner.json`, version 2.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct OwnerFile {
    version: u32,
    claimed_at: u64,
    /// `all` or `one`, as the claim said (for display; My Signet applies it).
    scope: String,
    /// The current revision (0 after a claim).
    rev: u64,
    /// Empty only while the Keep is unclaimed.
    owners: Vec<Persona>,
    /// Who sent the update that set `rev` (hex), so a repeat of it is told
    /// `updated` again, not `stale`. None at revision 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rev_by: Option<String>,
}

impl OwnerFile {
    fn unclaimed() -> OwnerFile {
        OwnerFile { version: 2, claimed_at: 0, scope: "one".into(), rev: 0, owners: Vec::new(), rev_by: None }
    }
}

/// `owner.json` as the K1d Keep wrote it: a list of personas (one, after a claim).
#[derive(Deserialize)]
struct OwnerFileV1 {
    owners: Vec<String>,
    claimed_at: u64,
}

fn normal_hex(text: &str) -> Option<String> {
    PublicKey::from_hex(text).ok().map(|p| p.to_hex())
}

/// Read `owner.json`. The second value is `true` when it was a K1d file,
/// read as a K1e one in memory (the caller rewrites it). A file that cannot be
/// read is an error: the Keep must not guess whether it is claimed.
fn read_owner_file(dir: &Path) -> Result<(OwnerFile, bool), String> {
    let bad = |why: String| format!("{OWNER_FILE} couldn't be read: {why}");
    let text = match std::fs::read_to_string(dir.join(OWNER_FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((OwnerFile::unclaimed(), false)),
        Err(e) => return Err(bad(e.to_string())),
    };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
    match value.get("version").and_then(|v| v.as_u64()) {
        Some(1) => {
            let old: OwnerFileV1 = serde_json::from_value(value).map_err(|e| bad(e.to_string()))?;
            let mut file = OwnerFile::unclaimed();
            file.claimed_at = old.claimed_at;
            for owner in &old.owners {
                let persona = normal_hex(owner).ok_or_else(|| bad("an owner is bad".into()))?;
                if file.owners.iter().any(|o| o.persona == persona) {
                    return Err(bad("an owner is listed twice".into()));
                }
                // The claimer: listed at revision 0, its claim is its acceptance.
                file.owners.push(Persona { persona, sections: Vec::new(), added_rev: 0, accepted_rev: Some(0) });
            }
            Ok((file, !old.owners.is_empty()))
        }
        Some(2) => {
            let mut file: OwnerFile = serde_json::from_value(value).map_err(|e| bad(e.to_string()))?;
            if file.owners.is_empty() || file.owners.len() > MAX_PERSONAS {
                return Err(bad("the owner list is the wrong size".into()));
            }
            let mut seen = HashSet::new();
            for owner in &mut file.owners {
                owner.persona = normal_hex(&owner.persona).ok_or_else(|| bad("an owner is bad".into()))?;
                if !seen.insert(owner.persona.clone()) {
                    return Err(bad("an owner is listed twice".into()));
                }
            }
            if let Some(by) = &mut file.rev_by {
                *by = normal_hex(by).ok_or_else(|| bad("the sender of the revision is bad".into()))?;
            }
            if !file.owners.iter().any(Persona::active) {
                return Err(bad("no owner persona is active".into()));
            }
            // Display only; anything but `all` is `one`.
            if file.scope != "all" {
                file.scope = "one".into();
            }
            Ok((file, false))
        }
        Some(other) => Err(bad(format!("version {other} is not understood"))),
        None => Err(bad("no version".into())),
    }
}

#[derive(Serialize, Deserialize, Default)]
struct StateFile {
    version: u32,
    #[serde(default)]
    cursor: Option<u64>,
    #[serde(default)]
    grants: Vec<Grant>,
}

/// What an ack is about.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Subject {
    /// A claim: `["claim"]`.
    Claim,
    /// An invite or revoke: `["grant", <32 hex>]`.
    Grant(String),
    /// An owners update or accept: `["rev", <decimal>]`.
    Rev(u64),
}

/// What the Keep says back.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Ack {
    pub to: PublicKey,
    pub status: &'static str,
    pub subject: Subject,
    pub reason: Option<&'static str>,
}

impl Ack {
    fn refused(to: PublicKey, grant: &str, reason: &'static str) -> Ack {
        Ack { to, status: "refused", subject: Subject::Grant(grant.to_string()), reason: Some(reason) }
    }
    fn grant(to: PublicKey, grant: &str, status: &'static str) -> Ack {
        Ack { to, status, subject: Subject::Grant(grant.to_string()), reason: None }
    }
    /// `updated` or `pending`, with a revision.
    fn rev(to: PublicKey, status: &'static str, rev: u64) -> Ack {
        Ack { to, status, subject: Subject::Rev(rev), reason: None }
    }
    /// A refusal of an update or accept, carrying the Keep's current revision.
    fn refused_rev(to: PublicKey, rev: u64, reason: &'static str) -> Ack {
        Ack { to, status: "refused", subject: Subject::Rev(rev), reason: Some(reason) }
    }
}

/// A 32-hex-digit value (16 bytes), as the contract's `grant` and `code`.
fn parse_hex16(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in text.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Equal without stopping at the first difference.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A revision: decimal digits, a `u64`.
fn parse_rev(rumor: &UnsignedEvent) -> Option<u64> {
    let text = one_tag(rumor, "rev").ok()??;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|rev| *rev <= MAX_REV)
}

/// Why an owners update's persona list was refused.
enum ListError {
    Malformed,
    TooMany,
}

/// The `persona` tags of an owners update: persona hex (lower case) and its
/// sections (no repeats).
fn parse_personas(rumor: &UnsignedEvent) -> Result<Vec<(String, Vec<String>)>, ListError> {
    let tags: Vec<&[String]> = rumor.tags.iter().map(|t| t.as_slice()).filter(|t| t.first().map(String::as_str) == Some("persona")).collect();
    if tags.is_empty() {
        return Err(ListError::Malformed);
    }
    if tags.len() > MAX_PERSONAS {
        return Err(ListError::TooMany);
    }
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for tag in tags {
        let persona = tag.get(1).and_then(|p| normal_hex(p)).ok_or(ListError::Malformed)?;
        if out.iter().any(|(p, _)| *p == persona) {
            return Err(ListError::Malformed);
        }
        let mut sections: Vec<String> = Vec::new();
        for section in &tag[2..] {
            if !section.is_empty() && !sections.contains(section) {
                sections.push(section.clone());
            }
        }
        out.push((persona, sections));
    }
    Ok(out)
}

/// A fresh invite, as parsed from its tags.
struct Wanted {
    secret: [u8; 32],
    expiration: u64,
    holder: Option<String>,
    label: Option<String>,
}

impl Wanted {
    /// What the invite said, in one string. The secret is there as its invite
    /// id (a hash), so the state file holds no secret.
    fn fingerprint(&self) -> String {
        let who = self.holder.as_ref().map_or("guest".to_string(), |p| format!("p:{p}"));
        format!("{}|{}|{who}|{}", InviteId::of(&self.secret), self.expiration, self.label.as_deref().unwrap_or(""))
    }
}

/// Read an invite's tags (everything but `keep` and `grant`). `Err` = malformed.
fn parse_invite(rumor: &UnsignedEvent) -> Result<Wanted, ()> {
    let secret = parse_hex32(one_tag(rumor, "secret")?.ok_or(())?).map_err(|_| ())?;
    let expiration = one_tag(rumor, "expiration")?.ok_or(())?;
    if expiration.is_empty() || !expiration.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    let expiration = expiration.parse::<u64>().map_err(|_| ())?;
    let guest = has_tag(rumor, "guest");
    let persona = one_tag(rumor, "p")?;
    let holder = match (persona, guest) {
        (Some(p), false) => Some(PublicKey::from_hex(p).map_err(|_| ())?.to_hex()),
        (None, true) if count_tags(rumor, "guest") == 1 => None,
        _ => return Err(()),
    };
    let label = match one_tag(rumor, "name")? {
        None => None,
        Some(name) if name.chars().count() > invites::MAX_LABEL => return Err(()),
        Some(name) => invites::clean_label(name),
    };
    Ok(Wanted { secret, expiration, holder, label })
}

/// Where a message came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Arrived on the standing subscription.
    Live,
    /// Found in a start-up (or comeback) read. `revoke_ok`: for a revoke, its
    /// signer was an active owner persona at its place in the order, so it
    /// counts even if the signer has been removed since.
    Backlog { revoke_ok: bool },
}

/// The owner rules and their files.
pub struct Engine {
    keep: Keys,
    dir: PathBuf,
    invites: Arc<Invites>,
    /// The owner personas, their revision and the scope.
    data: OwnerFile,
    /// Accepts that came before the update that lists their persona (memory only).
    pending: Vec<Pending>,
    /// `owner.json` in memory is ahead of the file (an update the disk could
    /// not take: it holds for this run). Any later write clears it; so does
    /// the retry made at the start of each message.
    owners_unsaved: bool,
    /// Who sent the update that was not saved, and its revision: a resend of
    /// it is acknowledged `updated` once saved, not refused `stale`.
    unsaved_update: Option<(PublicKey, u64)>,
    code: Option<[u8; 16]>,
    grants: Vec<Grant>,
    cursor: Option<u64>,
    cursor_saved: Option<u64>,
    /// The window is being read newest page first: the cursor is not saved
    /// until it has all been read.
    paging: bool,
    /// The grants in memory are ahead of `owner-state.json` (a revoke the disk
    /// could not take). The next successful save clears it; a repeat of the
    /// revoke tries again before it is acknowledged.
    state_unsaved: bool,
}

impl Engine {
    /// Open the owner files in `dir`. With no owner yet the Keep makes a code
    /// (`new_code`, 16 random bytes at each start). A file that cannot be
    /// read is an error: the Keep must not guess whether it is claimed.
    pub fn open(dir: &Path, keep: Keys, invites: Arc<Invites>, new_code: [u8; 16]) -> Result<Engine, String> {
        let (data, migrated) = read_owner_file(dir)?;
        let state = match std::fs::read_to_string(dir.join(STATE_FILE)) {
            Ok(text) => {
                let file: StateFile = serde_json::from_str(&text).map_err(|e| format!("{STATE_FILE} couldn't be read: {e}"))?;
                if file.version != 1 {
                    return Err(format!("{STATE_FILE} couldn't be read: version {} is not understood", file.version));
                }
                file
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StateFile { version: 1, ..StateFile::default() },
            Err(e) => return Err(format!("{STATE_FILE} couldn't be read: {e}")),
        };
        let code = data.owners.is_empty().then_some(new_code);
        let mut engine = Engine {
            keep,
            dir: dir.to_path_buf(),
            invites,
            data,
            pending: Vec::new(),
            owners_unsaved: false,
            unsaved_update: None,
            code,
            grants: state.grants,
            cursor: state.cursor,
            cursor_saved: state.cursor,
            paging: false,
            state_unsaved: false,
        };
        if engine.migrate_grants() {
            if let Err(e) = engine.save_state() {
                log::warn!("[keep] cannot save the owner state: {e}");
            }
        }
        // A K1d owner file becomes a K1e one in place. If the disk will not
        // take it the Keep runs on what it read, and tries again at the next write.
        if migrated {
            match engine.write_owners(&engine.data) {
                Ok(()) => log::info!("[keep] {OWNER_FILE} was rewritten in the K1e form"),
                Err(e) => {
                    log::warn!("[keep] cannot rewrite {OWNER_FILE} in the K1e form (it is read as one meanwhile): {e}");
                    engine.owners_unsaved = true;
                }
            }
        }
        engine.reconcile(unix_now());
        Ok(engine)
    }

    /// An older state file kept the invite's secret in a grant's fingerprint
    /// and no expiry: swap the secret for its invite id and read the expiry
    /// out of it. `true` if anything changed.
    fn migrate_grants(&mut self) -> bool {
        let mut changed = false;
        for grant in &mut self.grants {
            let mut parts = grant.tags.splitn(3, '|');
            let (Some(first), Some(second), Some(rest)) = (parts.next(), parts.next(), parts.next()) else { continue };
            if grant.expires_at.is_none() {
                grant.expires_at = second.parse().ok();
                changed |= grant.expires_at.is_some();
            }
            if first.len() == 64 {
                if let Ok(secret) = parse_hex32(first) {
                    grant.tags = format!("{}|{second}|{rest}", InviteId::of(&secret));
                    changed = true;
                }
            }
        }
        changed
    }

    /// Forget the grants whose invites have run out for good.
    fn prune(&mut self, now: u64) {
        self.grants.retain(|g| !g.forgettable(now));
    }

    pub fn claimed(&self) -> bool {
        !self.data.owners.is_empty()
    }

    /// Write `data` as `owner.json` (private, fsynced).
    fn write_owners(&self, data: &OwnerFile) -> Result<(), String> {
        let text = serde_json::to_string_pretty(data).map_err(|e| e.to_string())? + "\n";
        write_private_atomic(&self.dir.join(OWNER_FILE), text.as_bytes())
    }

    /// Replace the owner data with `next` once it is on disk. `Err` = nothing changed.
    fn store_owners(&mut self, next: OwnerFile) -> Result<(), String> {
        self.write_owners(&next)?;
        self.data = next;
        self.owners_unsaved = false;
        Ok(())
    }

    /// Try again to write an `owner.json` that is ahead of the disk.
    fn retry_owners(&mut self) {
        if self.owners_unsaved && self.write_owners(&self.data).is_ok() {
            self.owners_unsaved = false;
        }
    }

    pub fn keep_hex(&self) -> String {
        self.keep.public_key().to_hex()
    }

    /// The `since` of the subscription: the cursor less the jitter margin, or
    /// three days back on a first run.
    pub fn since(&self, now: u64) -> u64 {
        match self.cursor {
            Some(cursor) => cursor.saturating_sub(CURSOR_MARGIN_SECS),
            None => now.saturating_sub(FIRST_RUN_SECS),
        }
    }

    fn save_state(&mut self) -> Result<(), String> {
        // While the window is being read newest page first, a save for another
        // reason (a grant) keeps the cursor that was saved.
        let cursor = if self.paging { self.cursor_saved } else { self.cursor };
        let file = StateFile { version: 1, cursor, grants: self.grants.clone() };
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())? + "\n";
        write_private_atomic(&self.dir.join(STATE_FILE), text.as_bytes())?;
        self.cursor_saved = cursor;
        self.state_unsaved = false;
        Ok(())
    }

    /// Note a processed wrap's `created_at`. Never past `now`: a wrap dated in
    /// the future must not move the cursor ahead of real messages. Written
    /// out when it has moved a minute past what is saved.
    pub fn note_wrap(&mut self, created_at: u64, now: u64) {
        let at = created_at.min(now);
        if self.cursor.is_none_or(|c| at > c) {
            self.cursor = Some(at);
        }
        if self.paging {
            return;
        }
        if self.cursor.unwrap_or(0) >= self.cursor_saved.unwrap_or(0).saturating_add(CURSOR_SAVE_STEP) || self.cursor_saved.is_none() {
            if let Err(e) = self.save_state() {
                log::warn!("[keep] cannot save the wrap cursor: {e}");
            }
        }
    }

    /// While `on`, the cursor moves in memory only: pages of a long window
    /// arrive newest first, and the saved cursor must not jump past the pages
    /// not yet read. Turning it off saves the cursor; setting it as it is
    /// changes nothing.
    pub fn set_paging(&mut self, on: bool) {
        if self.paging == on {
            return;
        }
        self.paging = on;
        if !on {
            self.flush_cursor();
        }
    }

    /// Write the cursor if it has moved at all (on shutdown).
    pub fn flush_cursor(&mut self) {
        if self.paging {
            return;
        }
        if self.cursor != self.cursor_saved {
            if let Err(e) = self.save_state() {
                log::warn!("[keep] cannot save the wrap cursor: {e}");
            }
        }
    }

    /// Read a rumor from `sender` (the seal's signer) at `now`. `None` = drop
    /// it silently, or nothing to say back (a write that failed is logged and
    /// not acknowledged: the owner's retry finds the Keep as it was).
    pub fn handle(&mut self, sender: PublicKey, rumor: &UnsignedEvent, now: u64, source: Source) -> Option<Ack> {
        if rumor.pubkey != sender {
            return None;
        }
        let Kind::Custom(kind) = rumor.kind else { return None };
        if !matches!(kind, KIND_CLAIM | KIND_INVITE | KIND_REVOKE | KIND_UPDATE | KIND_ACCEPT) {
            return None;
        }
        // The exchange must be about this Keep.
        if one_tag(rumor, "keep").ok()??.to_ascii_lowercase() != self.keep_hex() {
            return None;
        }
        self.retry_owners();
        match kind {
            KIND_CLAIM => self.claim(sender, rumor, now),
            KIND_INVITE if self.is_active(&sender) => self.invite(sender, rumor, now),
            KIND_REVOKE if self.is_active(&sender) || source == (Source::Backlog { revoke_ok: true }) => self.revoke(sender, rumor, now),
            KIND_UPDATE => self.update(sender, rumor, now, source),
            KIND_ACCEPT => self.accept(sender, rumor, now),
            _ => None,
        }
    }

    /// Listed, whether or not it has accepted.
    fn is_listed(&self, who: &PublicKey) -> bool {
        let hex = who.to_hex();
        self.data.owners.iter().any(|o| o.persona == hex)
    }

    /// Listed and accepted: may invite, revoke and send an owners update.
    fn is_active(&self, who: &PublicKey) -> bool {
        let hex = who.to_hex();
        self.data.owners.iter().any(|o| o.persona == hex && o.active())
    }

    /// Forget the early accepts that have waited out their week.
    fn expire_pending(&mut self, now: u64) {
        self.pending.retain(|p| p.received_at.saturating_add(PENDING_TTL_SECS) > now);
    }

    fn claim(&mut self, sender: PublicKey, rumor: &UnsignedEvent, now: u64) -> Option<Ack> {
        let paired = Ack { to: sender, status: "paired", subject: Subject::Claim, reason: None };
        // A lost ack is recovered by claiming again (by an owner persona that may still act).
        if self.claimed() {
            return self.is_active(&sender).then_some(paired);
        }
        let code = parse_hex16(one_tag(rumor, "code").ok()??)?;
        if !same_bytes(&code, self.code.as_ref()?) {
            return None;
        }
        // Missing = `one`. Anything but `all` or `one` is not a claim.
        let scope = match one_tag(rumor, "scope").ok()? {
            None | Some("one") => "one",
            Some("all") => "all",
            Some(_) => return None,
        };
        let file = OwnerFile {
            version: 2,
            claimed_at: now,
            scope: scope.into(),
            rev: 0,
            // The claim is the claimer's acceptance.
            owners: vec![Persona { persona: sender.to_hex(), sections: Vec::new(), added_rev: 0, accepted_rev: Some(0) }],
            rev_by: None,
        };
        if let Err(e) = self.store_owners(file) {
            log::warn!("[keep] cannot save the owner: {e}");
            return None;
        }
        self.code = None;
        Some(paired)
    }

    /// An owners update (27664) from an owner persona that may act.
    fn update(&mut self, sender: PublicKey, rumor: &UnsignedEvent, now: u64, source: Source) -> Option<Ack> {
        // A resend of an update whose save failed: acknowledge it once it is saved.
        if let (Some((who, rev)), Some(sent)) = (self.unsaved_update, parse_rev(rumor)) {
            if who == sender && rev == sent && rev == self.data.rev {
                if self.owners_unsaved {
                    let data = self.data.clone();
                    if let Err(e) = self.store_owners(data) {
                        log::warn!("[keep] cannot save the owners update (it holds for this run): {e}");
                        return None;
                    }
                }
                self.unsaved_update = None;
                return Some(Ack::rev(sender, "updated", rev));
            }
        }
        // A repeat of the update that set the current revision (a lost `updated`
        // ack, or a relay handing it over again) is told `updated` again.
        if self.is_repeat_of_current(&sender, rumor) {
            return Some(Ack::rev(sender, "updated", self.data.rev));
        }
        if !self.is_active(&sender) {
            return None;
        }
        self.expire_pending(now);
        let current = self.data.rev;
        let Some(rev) = parse_rev(rumor) else { return Some(Ack::refused_rev(sender, current, "malformed")) };
        if rev <= current {
            // A replay found in the start-up read is old news, not a mistake
            // to tell the owner about; only a live message is refused.
            return match source {
                Source::Live => Some(Ack::refused_rev(sender, current, "stale")),
                Source::Backlog { .. } => None,
            };
        }
        let listed = match parse_personas(rumor) {
            Ok(listed) => listed,
            Err(ListError::TooMany) => return Some(Ack::refused_rev(sender, current, "too_many")),
            Err(ListError::Malformed) => return Some(Ack::refused_rev(sender, current, "malformed")),
        };
        let mut next = self.data.clone();
        next.rev = rev;
        next.rev_by = Some(sender.to_hex());
        next.owners = Vec::new();
        for (persona, sections) in listed {
            // Listed already: it keeps its place and its accept. New (or
            // added again after a removal, whose old record is gone): added at
            // this revision, active at once only if its accept came early.
            let kept = self.data.owners.iter().find(|o| o.persona == persona).cloned();
            let owner = match kept {
                Some(old) => Persona { sections, ..old },
                None => {
                    let early = self.pending.iter().find(|p| p.persona == persona && p.rev >= rev).map(|p| p.rev);
                    Persona { persona: persona.clone(), sections, added_rev: rev, accepted_rev: early }
                }
            };
            next.owners.push(owner);
        }
        // An early accept is used up by the update that lists it; one that
        // could never count (older than the listing) goes with it.
        let newly: Vec<String> = next.owners.iter().filter(|o| o.added_rev == rev).map(|o| o.persona.clone()).collect();
        // The list may drop its own signer, never its last active persona.
        if !next.owners.iter().any(Persona::active) {
            return Some(Ack::refused_rev(sender, current, "malformed"));
        }
        // Applied in memory first: a removal ends that persona's rights at
        // once, even if the disk is full. Then it is acknowledged only once saved.
        self.data = next;
        self.pending.retain(|p| !newly.contains(&p.persona));
        self.owners_unsaved = true;
        self.unsaved_update = Some((sender, rev));
        let data = self.data.clone();
        match self.store_owners(data) {
            Ok(()) => {
                self.unsaved_update = None;
                Some(Ack::rev(sender, "updated", rev))
            }
            Err(e) => {
                log::warn!("[keep] cannot save the owners update (it holds for this run, and is acknowledged when its resend can be saved): {e}");
                None
            }
        }
    }

    /// Whether `rumor` is the update `sender` sent that set the current
    /// revision: same signer, same revision, same list.
    fn is_repeat_of_current(&self, sender: &PublicKey, rumor: &UnsignedEvent) -> bool {
        if self.data.rev == 0 || self.data.rev_by.as_deref() != Some(sender.to_hex().as_str()) || parse_rev(rumor) != Some(self.data.rev) {
            return false;
        }
        let Ok(listed) = parse_personas(rumor) else { return false };
        listed.len() == self.data.owners.len() && listed.iter().zip(&self.data.owners).all(|((persona, sections), owner)| *persona == owner.persona && *sections == owner.sections)
    }

    /// An owner accept (27665) from the persona being added.
    ///
    /// Only a persona an update lists is answered. An accept from anyone else
    /// is held in memory for the update that may list it, with no write and no
    /// ack: anyone can address one to the Keep.
    fn accept(&mut self, sender: PublicKey, rumor: &UnsignedEvent, now: u64) -> Option<Ack> {
        if !self.claimed() {
            return None;
        }
        self.expire_pending(now);
        let current = self.data.rev;
        let me = sender.to_hex();
        let listed = self.is_listed(&sender);
        let Some(rev) = parse_rev(rumor) else { return listed.then(|| Ack::refused_rev(sender, current, "malformed")) };
        if let Some(owner) = self.data.owners.iter().find(|o| o.persona == me) {
            if rev >= owner.added_rev {
                if owner.accepted_rev.is_none_or(|accepted| rev > accepted) {
                    let mut next = self.data.clone();
                    if let Some(owner) = next.owners.iter_mut().find(|o| o.persona == me) {
                        owner.accepted_rev = Some(rev);
                    }
                    if let Err(e) = self.store_owners(next) {
                        log::warn!("[keep] cannot save an owner accept: {e}");
                        return None;
                    }
                }
                return Some(Ack::rev(sender, "updated", current));
            }
        }
        // Not listed (yet), or an accept of an older listing: held for the
        // update that lists it. An accept never adds a persona by itself.
        match self.pending.iter_mut().find(|p| p.persona == me) {
            Some(held) if rev > held.rev => {
                held.rev = rev;
                held.received_at = now;
            }
            Some(_) => {}
            None => {
                // Full: the oldest goes (a stranger can push a real one out;
                // the owner's resend recovers it).
                if self.pending.len() >= MAX_PENDING {
                    if let Some(oldest) = (0..self.pending.len()).min_by_key(|i| self.pending[*i].received_at) {
                        self.pending.remove(oldest);
                    }
                }
                self.pending.push(Pending { persona: me, rev, received_at: now });
            }
        }
        listed.then(|| Ack::rev(sender, "pending", rev))
    }

    fn grant_of(&self, rumor: &UnsignedEvent) -> Option<String> {
        let grant = one_tag(rumor, "grant").ok()??;
        parse_hex16(grant)?;
        Some(grant.to_ascii_lowercase())
    }

    fn invite(&mut self, owner: PublicKey, rumor: &UnsignedEvent, now: u64) -> Option<Ack> {
        let grant = self.grant_of(rumor)?;
        let Ok(wanted) = parse_invite(rumor) else {
            return Some(Ack::refused(owner, &grant, "malformed"));
        };
        let fingerprint = wanted.fingerprint();
        self.prune(now);
        if let Some(known) = self.grants.iter().find(|g| g.grant == grant) {
            return match known.state {
                GrantState::Revoked => Some(Ack::refused(owner, &grant, "revoked")),
                GrantState::Active if known.tags != fingerprint => Some(Ack::refused(owner, &grant, "conflict")),
                // The same invite again. It should be in the table; if a write
                // between the two files was lost, put it back.
                GrantState::Active => {
                    // Answer from the table's real row: the page may have
                    // cancelled the invite (while the Keep had no owner), and
                    // a cancelled invite is not `active`.
                    let id = InviteId::of(&wanted.secret);
                    match self.invites.status_of(id, now) {
                        Some(invites::Status::Revoked) => {
                            self.mark_revoked(&grant);
                            Some(Ack::refused(owner, &grant, "revoked"))
                        }
                        Some(_) => Some(Ack::grant(owner, &grant, "active")),
                        // Gone from the table and past its expiry: nothing to put back.
                        None if wanted.expiration <= now => Some(Ack::refused(owner, &grant, "expired")),
                        None => match self.invites.add_owner(wanted.secret, wanted.holder, wanted.label, wanted.expiration, now) {
                            Ok(_) => Some(Ack::grant(owner, &grant, "active")),
                            Err(AddOwnerError::Full) => Some(Ack::refused(owner, &grant, "full")),
                            Err(AddOwnerError::Write(e)) => {
                                log::warn!("[keep] cannot save an owner invite: {e}");
                                None
                            }
                        },
                    }
                }
            };
        }
        if wanted.expiration <= now {
            return Some(Ack::refused(owner, &grant, "expired"));
        }
        if wanted.expiration > now.saturating_add(MAX_EXPIRY_SECS) {
            return Some(Ack::refused(owner, &grant, "expiry_too_long"));
        }
        let invite_id = InviteId::of(&wanted.secret);
        // Another grant, or the page, already made this secret.
        if self.invites.contains(invite_id) || self.grants.iter().any(|g| g.invite == invite_id.to_string()) {
            return Some(Ack::refused(owner, &grant, "conflict"));
        }
        if self.grants.len() >= MAX_GRANTS {
            return Some(Ack::refused(owner, &grant, "full"));
        }
        self.grants.push(Grant {
            grant: grant.clone(),
            invite: invite_id.to_string(),
            state: GrantState::Active,
            tags: fingerprint,
            expires_at: Some(wanted.expiration),
            recorded_at: None,
        });
        if let Err(e) = self.save_state() {
            self.grants.pop();
            log::warn!("[keep] cannot save an owner grant: {e}");
            return None;
        }
        match self.invites.add_owner(wanted.secret, wanted.holder, wanted.label, wanted.expiration, now) {
            Ok(_) => Some(Ack::grant(owner, &grant, "active")),
            Err(why) => {
                // Not held: forget the grant so a later try can succeed.
                self.grants.pop();
                if let Err(e) = self.save_state() {
                    log::warn!("[keep] cannot save an owner grant: {e}");
                }
                match why {
                    AddOwnerError::Full => Some(Ack::refused(owner, &grant, "full")),
                    AddOwnerError::Write(e) => {
                        log::warn!("[keep] cannot save an owner invite: {e}");
                        None
                    }
                }
            }
        }
    }

    /// Record that the grant's invite is revoked (the table already says so).
    fn mark_revoked(&mut self, grant: &str) {
        let Some(at) = self.grants.iter().position(|g| g.grant == grant) else { return };
        if self.grants[at].state == GrantState::Revoked {
            return;
        }
        self.grants[at].state = GrantState::Revoked;
        self.grants[at].tags.clear();
        if let Err(e) = self.save_state() {
            log::warn!("[keep] cannot save an owner revoke: {e}");
        }
    }

    /// Make the table agree with the grants after a start: a revoke that the
    /// owner state has but the invite table lacks (a crash between the two
    /// writes, or a disk that was full) is applied, and a grant whose invite
    /// the page cancelled becomes revoked.
    fn reconcile(&mut self, now: u64) {
        let known: Vec<(String, String, GrantState)> = self.grants.iter().map(|g| (g.grant.clone(), g.invite.clone(), g.state)).collect();
        for (grant, invite, state) in known {
            let Some(id) = invites::parse_id(&invite) else { continue };
            match (state, self.invites.status_of(id, now)) {
                (GrantState::Revoked, Some(status)) if status != invites::Status::Revoked => {
                    if let Err(e) = self.invites.revoke_fail_safe(&invite) {
                        log::warn!("[keep] cannot save an owner revoke in the invites (it holds for this run): {e}");
                    }
                }
                (GrantState::Active, Some(invites::Status::Revoked)) => self.mark_revoked(&grant),
                _ => {}
            }
        }
    }

    fn revoke(&mut self, owner: PublicKey, rumor: &UnsignedEvent, now: u64) -> Option<Ack> {
        let grant = self.grant_of(rumor)?;
        self.prune(now);
        // A revoke must hold even when the disk is full: the grant stays
        // revoked in memory when `owner-state.json` cannot be written, the
        // invite is still cancelled (and its players dropped), and nothing is
        // acknowledged until both files are saved (the owner's resend finishes it).
        let mut saved = true;
        let at = self.grants.iter().position(|g| g.grant == grant);
        let invite = match at {
            Some(at) => {
                if self.grants[at].state == GrantState::Active {
                    self.grants[at].state = GrantState::Revoked;
                    self.grants[at].tags.clear();
                    self.state_unsaved = true;
                }
                if self.state_unsaved {
                    if let Err(e) = self.save_state() {
                        log::warn!("[keep] cannot save an owner revoke (it holds for this run): {e}");
                        saved = false;
                    }
                }
                self.grants[at].invite.clone()
            }
            None => {
                // Revoked for good, though the invite was never seen: a late
                // invite under this grant is refused.
                if self.grants.len() >= MAX_GRANTS {
                    log::warn!("[keep] too many grants to remember another revoke");
                    return None;
                }
                self.grants.push(Grant {
                    grant: grant.clone(),
                    invite: String::new(),
                    state: GrantState::Revoked,
                    tags: String::new(),
                    expires_at: None,
                    recorded_at: Some(now),
                });
                self.state_unsaved = true;
                if let Err(e) = self.save_state() {
                    log::warn!("[keep] cannot save an owner revoke (it holds for this run): {e}");
                    saved = false;
                }
                String::new()
            }
        };
        // Also for a repeat: if the file was written but the table was not, this finishes it.
        if !invite.is_empty() {
            if let Err(e) = self.invites.revoke_fail_safe(&invite) {
                log::warn!("[keep] cannot save an owner revoke in the invites (it holds for this run, and is saved by the next write): {e}");
                return None;
            }
        }
        saved.then(|| Ack::grant(owner, &grant, "revoked"))
    }

    /// Make the signed wrap for an ack.
    pub fn ack_wrap(&self, ack: &Ack, now: u64) -> Result<Event, String> {
        let tag = |parts: &[&str]| Tag::parse(parts.iter().map(|p| p.to_string()).collect::<Vec<_>>()).map_err(|e| e.to_string());
        let mut tags = vec![tag(&["keep", &self.keep_hex()])?, tag(&["status", ack.status])?];
        match &ack.subject {
            Subject::Grant(grant) => tags.push(tag(&["grant", grant])?),
            Subject::Claim => tags.push(tag(&["claim"])?),
            Subject::Rev(rev) => tags.push(tag(&["rev", &rev.to_string()])?),
        }
        if let Some(reason) = ack.reason {
            tags.push(tag(&["reason", reason])?);
        }
        wire::gift_wrap(&self.keep, &ack.to, KIND_ACK, tags, now)
    }

    /// Open one wrap from the relays and answer it: the ack wrap to send, if any.
    pub fn process(&mut self, wrap: &Event, now: u64) -> Option<Event> {
        // `wire::unwrap` takes the cheap checks first (kind, size, whom it is
        // for) and only then the signature and the decryption, so a flood of
        // junk costs little. The relay layer has verified the wrap already;
        // that is not relied on here, but it is not repeated either.
        let (sender, rumor) = wire::unwrap(&self.keep, wrap).ok()?;
        self.process_opened(wrap.created_at.as_secs(), sender, &rumor, now, Source::Live)
    }

    /// Answer a rumor from an opened wrap made at `wrap_at`.
    pub fn process_opened(&mut self, wrap_at: u64, sender: PublicKey, rumor: &UnsignedEvent, now: u64, source: Source) -> Option<Event> {
        // Only a message from the owner, or a claim that worked, moves the
        // cursor (and never past `now`). Anyone can post a wrap to the Keep's
        // key with any date; junk, a wrap that will not open, a claim with the
        // wrong code or a message from someone else must not push the cursor
        // ahead and hide the owner's older revokes at the next start.
        //
        // An owner persona that may act moves the cursor, and so does an
        // accept from a persona an update lists (waiting for that accept). An
        // accept that is only held for an update to come does not: anyone
        // can send one.
        let counts = self.is_active(&sender) || (rumor.kind == Kind::Custom(KIND_ACCEPT) && self.is_listed(&sender));
        let ack = self.handle(sender, rumor, now, source);
        if counts || self.is_active(&sender) {
            self.note_wrap(wrap_at, now);
        }
        // One line for each owner message that got an answer, or came from a
        // persona the Keep lists (the owner must be able to see that a message
        // arrived even when nothing is said back). Nobody else is logged: a
        // stranger can send as many as they like.
        if ack.is_some() || self.is_listed(&sender) {
            if let Some(line) = message_line(rumor, &sender, source, ack.as_ref()) {
                log::info!("{line}");
            }
        }
        let ack = ack?;
        match self.ack_wrap(&ack, now) {
            Ok(event) => Some(event),
            Err(e) => {
                log::warn!("[keep] cannot make an ack: {e}");
                None
            }
        }
    }
}

/// The first six hex digits of a key: enough to tell personas apart in a log.
pub fn short_key(who: &PublicKey) -> String {
    who.to_hex().chars().take(6).collect()
}

/// The log line for an owner message that was handled: its kind, who sent it
/// (a short key prefix), where it was read, the revision it names (an owners
/// update or accept) and what the Keep answered or that it said nothing.
/// Built only from the ack (a static status and reason, a revision) and the
/// message's kind and revision, so no code, secret or link can get into it.
/// `None` for a kind the Keep does not act on.
pub fn message_line(rumor: &UnsignedEvent, sender: &PublicKey, source: Source, ack: Option<&Ack>) -> Option<String> {
    let Kind::Custom(kind) = rumor.kind else { return None };
    let name = match kind {
        KIND_CLAIM => "claim",
        KIND_INVITE => "invite",
        KIND_REVOKE => "revoke",
        KIND_UPDATE => "update",
        KIND_ACCEPT => "accept",
        _ => return None,
    };
    let read = match source {
        Source::Live => "live",
        Source::Backlog { .. } => "backlog",
    };
    let mut line = format!("[keep] owner {name} from {} ({read})", short_key(sender));
    if matches!(kind, KIND_UPDATE | KIND_ACCEPT) {
        match parse_rev(rumor) {
            Some(rev) => line.push_str(&format!(" rev {rev}")),
            None => line.push_str(" rev ?"),
        }
    }
    match ack {
        Some(ack) => {
            line.push_str(&format!(": {}", ack.status));
            if let Some(reason) = ack.reason {
                line.push_str(&format!(" ({reason})"));
            }
            // A refusal carries the revision the Keep is at, which is not the one sent.
            if let (Subject::Rev(rev), "refused") = (&ack.subject, ack.status) {
                line.push_str(&format!(", the Keep is at rev {rev}"));
            }
        }
        None => line.push_str(": no ack"),
    }
    Some(line)
}

/// The log line for an ack handed to the relays: who it is for and how many relays.
pub fn ack_sent_line(ack: &Event, relays: usize) -> String {
    let to = ack.tags.iter().find_map(|t| match t.as_slice() {
        [name, who, ..] if name == "p" => Some(who.chars().take(6).collect::<String>()),
        _ => None,
    });
    format!("[keep] ack to {} sent to {relays} relays", to.as_deref().unwrap_or("?"))
}

/// `signet-keep:pair?k=...&c=...&n=...&r=...`, the link My Signet pairs with.
pub fn pairing_link(keep_hex: &str, code: &[u8; 16], name: &str, relays: &[String]) -> String {
    let mut link = format!("signet-keep:pair?k={keep_hex}&c={}&n={}", hex_encode(code), percent_encode(name));
    for relay in relays.iter().take(MAX_LINK_RELAYS) {
        link.push_str("&r=");
        link.push_str(&percent_encode(relay));
    }
    link
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The owner, shared between the status page's threads and the relay loop.
pub struct Owner {
    engine: Mutex<Engine>,
    name: String,
    relays: Vec<String>,
    pacing: Pacing,
}

/// How hard the Keep may lean on a public relay. A relay answers too many
/// requests from one address with a ban, and a ban lasts far longer than the
/// burst that earned it.
#[derive(Clone, Copy, Debug)]
struct Pacing {
    /// Wait before each page of a read after the first. A window with a flood in
    /// it is read at two pages a second rather than as fast as the relay answers.
    page_gap: Duration,
    /// The shortest wait before a relay is read again after it came back (or
    /// refused a read). Each read of the same relay within [`Pacing::calm`] of the
    /// last doubles the wait, up to [`Pacing::redial_max`].
    redial_base: Duration,
    redial_max: Duration,
    /// A relay left alone this long starts over with the shortest wait.
    calm: Duration,
}

impl Pacing {
    #[cfg_attr(test, allow(dead_code))]
    const STANDARD: Pacing = Pacing {
        page_gap: Duration::from_millis(500),
        redial_base: Duration::from_secs(15),
        redial_max: Duration::from_secs(600),
        // Longer than the longest wait, or a relay that keeps failing would start over each time.
        calm: Duration::from_secs(1200),
    };
    /// No waits: the tests that are not about pacing.
    #[cfg(test)]
    const NONE: Pacing = Pacing { page_gap: Duration::ZERO, redial_base: Duration::ZERO, redial_max: Duration::ZERO, calm: Duration::ZERO };
}

/// When one relay may next be read from the start: the waits grow while the
/// relay keeps coming back (or refusing) and shrink back after a calm spell.
#[derive(Clone, Copy, Debug)]
struct Redial {
    last: Option<Instant>,
    level: u32,
    next: Option<Instant>,
}

impl Redial {
    fn new() -> Redial {
        Redial { last: None, level: 0, next: None }
    }

    /// May the relay be read now?
    fn ready(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }

    /// The read that began at [`Redial::started`] is over at `now`: the next one is
    /// at least a short wait after this, however long this one took (a read of a
    /// flood can outlast the wait it began with).
    fn finished(&mut self, now: Instant, pacing: &Pacing) {
        let after = now + pacing.redial_base;
        self.next = Some(self.next.map_or(after, |next| next.max(after)));
    }

    /// The relay is being read at `now`: when it may be read again.
    fn started(&mut self, now: Instant, pacing: &Pacing) {
        self.level = match self.last {
            Some(last) if now.saturating_duration_since(last) < pacing.calm => (self.level + 1).min(16),
            _ => 0,
        };
        self.last = Some(now);
        let wait = pacing.redial_base.saturating_mul(1u32 << self.level).min(pacing.redial_max);
        self.next = Some(now + wait);
    }
}

/// A relay's window still to be read, whole or in part: from `since`, with
/// what earlier reads of it found, held unapplied. The saved cursor stays put
/// while any relay has one of these.
///
/// A wrap sent after a read began can be dated up to [`CURSOR_MARGIN_SECS`]
/// before that (NIP-59 jitter and clock skew), so a read that was cut short is
/// read again from the top: only `done`, the wraps dated strictly between its
/// two ends, is passed over. Those were read, and are dated more than the
/// margin before the read that got them, so nothing sent since can be among
/// them.
struct Resume {
    since: u64,
    done: Option<(u64, u64)>,
    held: Vec<Backlog>,
}

impl Resume {
    fn fresh(since: u64) -> Resume {
        Resume { since, done: None, held: Vec::new() }
    }

    /// The read that began at `started` stopped before the page up to `until`
    /// (none: before its first page): everything above `until` was read, so
    /// what lies more than the margin before `started` is done. A read stopped
    /// above the band that was already done leaves that band as it was.
    fn cut(self, until: Option<u64>, started: u64, held: Vec<Backlog>) -> Read {
        let floor = started.saturating_sub(CURSOR_MARGIN_SECS);
        let done = match (until, self.done) {
            (Some(until), Some((lo, hi))) if until > lo => Some((lo, hi)),
            (Some(until), _) => (until < floor).then_some((until, floor)),
            (None, done) => done,
        };
        Read::Cut(Resume { since: self.since, done, held })
    }
}

/// What a read of a relay's window got.
enum Read {
    Whole(Vec<Backlog>),
    Cut(Resume),
}

/// A relay's own words for a log line: printable characters only, and short,
/// so a relay cannot forge a line or flood one.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(60).collect()
}

/// Sleep for `gap` in short steps. `false` if `stop` said so meanwhile.
fn pause(gap: Duration, stop: &dyn Fn() -> bool) -> bool {
    let end = Instant::now() + gap;
    loop {
        if stop() {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// Why [`Owner::open`] could not make an owner.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// An owner file could not be read. The Keep must not guess whether it is
    /// claimed, so it counts as claimed.
    Files(String),
    /// Anything else (no usable relay, no randomness, no key): owner messages
    /// are off, and the Keep is as claimed as its `owner.json` says.
    Other(String),
}

impl OpenError {
    pub fn message(&self) -> &str {
        match self {
            OpenError::Files(m) | OpenError::Other(m) => m,
        }
    }

    /// Whether the page must treat the Keep as claimed (make no invites). Only
    /// for owner files that cannot be read, or an `owner.json` that exists:
    /// a Keep nobody has claimed is not shut because a relay is missing.
    pub fn fails_closed(&self, dir: &Path) -> bool {
        match self {
            OpenError::Files(_) => true,
            OpenError::Other(_) => !matches!(std::fs::metadata(dir.join(OWNER_FILE)), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
        }
    }
}

impl Owner {
    /// `relays` are the Keep's own list: the pairing link carries the first
    /// [`MAX_LINK_RELAYS`], and the Keep listens and answers on exactly those.
    pub fn open(dir: &Path, keep: Keys, invites: Arc<Invites>, name: &str, relays: &[String]) -> Result<Owner, OpenError> {
        let mut code = [0u8; 16];
        getrandom::fill(&mut code).map_err(|e| OpenError::Other(format!("No randomness for a pairing code: {e}")))?;
        // `wss://` in any case, as the relay list accepts it; the link carries it in lower case.
        let relays: Vec<String> = relays
            .iter()
            .filter(|r| r.get(..6).is_some_and(|scheme| scheme.eq_ignore_ascii_case("wss://")))
            .map(|r| format!("wss://{}", &r[6..]))
            .take(MAX_LINK_RELAYS)
            .collect();
        if relays.is_empty() {
            return Err(OpenError::Other("owner pairing needs at least one wss:// relay".into()));
        }
        let engine = Engine::open(dir, keep, invites, code).map_err(OpenError::Files)?;
        #[cfg(test)]
        let pacing = Pacing::NONE;
        #[cfg(not(test))]
        let pacing = Pacing::STANDARD;
        Ok(Owner { engine: Mutex::new(engine), name: name.to_string(), relays, pacing })
    }

    #[cfg(test)]
    fn with_pacing(mut self, pacing: Pacing) -> Owner {
        self.pacing = pacing;
        self
    }

    fn lock(&self) -> MutexGuard<'_, Engine> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn claimed(&self) -> bool {
        self.lock().claimed()
    }

    /// Read and answer one wrap from the relays: the ack to send, if any.
    pub fn process(&self, wrap: &Event, now: u64) -> Option<Event> {
        self.lock().process(wrap, now)
    }

    #[cfg(test)]
    fn relays(&self) -> &[String] {
        &self.relays
    }

    /// One connection to each of the Keep's relays, for [`Owner::run`]. A relay
    /// that challenges for NIP-42 authentication before it serves a read of the
    /// Keep's wraps (relay.damus.io does) is answered with an event signed by
    /// the Keep's key, and the request it closed for want of that is asked again
    /// at once. Nothing else goes to a relay for it.
    pub fn connect_relays(&self) -> Vec<Arc<dyn Relay>> {
        let connector = AuthConnector::new(auth_signer(self.lock().keep.clone()));
        self.relays.iter().map(|url| connector.connect(std::slice::from_ref(url))).collect()
    }

    /// The pairing link, only while the Keep is unclaimed.
    pub fn pairing_link(&self) -> Option<String> {
        let engine = self.lock();
        let code = engine.code.as_ref()?;
        Some(pairing_link(&engine.keep_hex(), code, &self.name, &self.relays))
    }

    /// Listen for owner messages on `relays` (one connection each) and answer
    /// them, on every one of them, until `stop` says so. Blocks: run it on its
    /// own thread.
    ///
    /// At the start, and again whenever a relay comes back after being down,
    /// each relay's window is read a page at a time, newest first, so a flood
    /// of newer junk cannot hide an older message behind the relay's own
    /// limit. The standing subscription is opened first, so nothing that
    /// arrives meanwhile is missed.
    pub fn run(&self, relays: Vec<Arc<dyn Relay>>, stop: impl Fn() -> bool) {
        self.run_with(&relays, PAGE_LIMIT, STRANGER_CAP, &stop);
    }

    fn run_with(&self, relays: &[Arc<dyn Relay>], limit: usize, strangers: usize, stop: &dyn Fn() -> bool) {
        let mut seen = Seen::default();
        let since = self.lock().since(unix_now());
        let mut subs = Vec::new();
        let mut down = vec![false; relays.len()];
        let mut redial = vec![Redial::new(); relays.len()];
        // A refusal of the standing request, already noted (so it is noted once).
        let mut refused = vec![false; relays.len()];
        // A relay whose window is still to be read: it came back, refused a
        // request, or was cut short part way (what it read is held unapplied).
        let mut unread: Vec<Option<Resume>> = (0..relays.len()).map(|_| None).collect();
        // Every window is unread until the start-up read has been applied.
        self.lock().set_paging(true);
        let mut backlog = Vec::new();
        for (at, relay) in relays.iter().enumerate() {
            redial[at].started(Instant::now(), &self.pacing);
            let (sub, read) = self.catch_up(relay.as_ref(), Resume::fresh(since), limit, strangers, &mut seen, stop);
            subs.push(sub);
            match read {
                Read::Whole(found) => backlog.extend(found),
                Read::Cut(rest) => unread[at] = Some(rest),
            }
            redial[at].finished(Instant::now(), &self.pacing);
        }
        // All of it is read before any of it is applied, oldest first, so an
        // invite is not judged before the update and accept that make its
        // sender an owner (pages arrive newest first). A relay that was cut
        // short is applied when the rest of its window is read, as one batch.
        // A batch a stop cut short leaves the saved cursor where it was (paging
        // stays on), so the next start reads what was not applied.
        if !self.apply_backlog(backlog, &mut seen, relays, stop) {
            return;
        }
        self.hold_cursor(&unread);
        'run: while !stop() {
            let mut idle = true;
            for (at, relay) in relays.iter().enumerate() {
                // A relay that was down and is back may have kept things for
                // us; its standing request would send the whole window again,
                // so start over from the cursor instead. A relay that answered
                // a request with CLOSED (a rate limit, say) is not heard from
                // again on that request, so that is asked afresh too. Both wait
                // their turn, longer each time the same relay comes round again
                // (see [`Redial`]), so a relay that takes the connection and then
                // drops or refuses it is not asked over and over.
                let status = subs[at].status();
                let mut owed = came_back(&mut down[at], &status);
                if let SubStatus::Closed(why) = &status {
                    if !refused[at] {
                        refused[at] = true;
                        owed = true;
                        log::warn!("[keep] a relay closed the request for owner messages ({}); it is asked again after a wait", clean(why));
                    }
                } else {
                    refused[at] = false;
                }
                if owed && unread[at].is_none() {
                    unread[at] = Some(Resume::fresh(self.lock().since(unix_now())));
                    self.hold_cursor(&unread);
                }
                // While its window is unread a relay's old request is not
                // listened to: its replay would be applied as it comes, out of
                // order and before the ordered read. The read covers all of it.
                let waiting = unread[at].is_some();
                while let Some(message) = subs[at].try_recv() {
                    idle = false;
                    let RelayMsg::Event(wrap) = message else { continue };
                    if !waiting && seen.first(wrap.id) {
                        self.answer(&wrap, relays);
                    }
                }
                if !(matches!(status, SubStatus::Up | SubStatus::Closed(_)) && redial[at].ready(Instant::now())) {
                    continue;
                }
                let Some(rest) = unread[at].take() else { continue };
                refused[at] = false;
                redial[at].started(Instant::now(), &self.pacing);
                idle = false;
                let (sub, read) = self.catch_up(relay.as_ref(), rest, limit, strangers, &mut seen, stop);
                subs[at] = sub;
                down[at] = false;
                match read {
                    Read::Whole(found) => {
                        if !self.apply_backlog(found, &mut seen, relays, stop) {
                            break 'run;
                        }
                    }
                    Read::Cut(rest) => unread[at] = Some(rest),
                }
                redial[at].finished(Instant::now(), &self.pacing);
                self.hold_cursor(&unread);
            }
            if idle {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.lock().flush_cursor();
    }

    /// The saved cursor stays where it was while any relay's window is unread,
    /// so a stop or a crash cannot lose that relay's older pages for good: the
    /// next start reads from the old cursor.
    ///
    /// A relay that stays down, or keeps refusing, holds the saved cursor back
    /// for as long as it does (the whole run, if it never comes back). That is
    /// the safe way round: the next start reads further back than it needs to,
    /// and loses nothing.
    fn hold_cursor(&self, unread: &[Option<Resume>]) {
        self.lock().set_paging(unread.iter().any(Option::is_some));
    }

    /// Open the standing subscription on `relay`, then read what it holds of
    /// the window `rest` describes, page by page. Returns the subscription and
    /// what was read, not yet applied.
    fn catch_up(&self, relay: &dyn Relay, rest: Resume, limit: usize, strangers: usize, seen: &mut Seen, stop: &dyn Fn() -> bool) -> (Subscription, Read) {
        let keep_pub = self.lock().keep.public_key();
        let filter = Filter::new().kind(Kind::GiftWrap).pubkey(keep_pub).since(Timestamp::from_secs(rest.since));
        let sub = relay.subscribe(vec![filter]);
        let read = self.read_window(relay, rest, limit, strangers, seen, stop);
        (sub, read)
    }

    /// [`Owner::read_window`] of a whole window from `since`: `false` for the
    /// first value if it was cut short; the second is what was held.
    #[cfg(test)]
    fn read_pages(&self, relay: &dyn Relay, since: u64, limit: usize, strangers: usize, seen: &mut Seen, stop: &dyn Fn() -> bool) -> (bool, Vec<Backlog>) {
        match self.read_window(relay, Resume::fresh(since), limit, strangers, seen, stop) {
            Read::Whole(found) => (true, found),
            Read::Cut(rest) => (false, rest.held),
        }
    }

    /// Read the stored wraps for this Keep from `rest.since`, `limit` at a
    /// time, newest page first, passing over the band `rest.done`, and open
    /// each (the cheap checks, then the signature and the decryption, in
    /// [`wire::unwrap`]). A relay cuts a read short at its own limit, so each
    /// next page asks for what is older than the last. What is worth applying
    /// is held, with what `rest` held already; a relay that refuses a page or
    /// cannot be reached (or a stop) cuts the read short.
    ///
    /// A wrap is marked seen (for every relay and the live feed) only when it
    /// is applied, or when no relay's copy of it could be: one that does not
    /// open, or is not a message the Keep acts on. One held here may still be
    /// taken first from another relay's read or the live feed.
    fn read_window(&self, relay: &dyn Relay, rest: Resume, limit: usize, strangers: usize, seen: &mut Seen, stop: &dyn Fn() -> bool) -> Read {
        let started = unix_now();
        let Resume { since, done, held: mut found } = rest;
        let (keep, keep_pub, listed, code) = {
            let engine = self.lock();
            // Who is listed when the read starts: nobody else can sign as them.
            let listed: HashSet<String> = engine.data.owners.iter().map(|o| o.persona.clone()).collect();
            (engine.keep.clone(), engine.keep.public_key(), listed, engine.code)
        };
        let keep_hex = keep_pub.to_hex();
        // Wraps met in this read (a page starts on the second the last one ended on).
        let mut met: HashSet<EventId> = found.iter().map(|b| b.wrap_id).collect();
        // Listed personas' messages (and the right claim) count apart from everyone else's.
        let counts_apart = |item: &Backlog| {
            listed.contains(&item.sender.to_hex()) || (item.kind == KIND_CLAIM && code.is_some_and(|code| item.tag("code").and_then(parse_hex16).is_some_and(|sent| same_bytes(&sent, &code))))
        };
        let mut kept = found.iter().filter(|b| counts_apart(b)).count();
        let mut others = found.len() - kept;
        let rest = Resume { since, done, held: Vec::new() };
        let mut warned = false;
        let mut warned_others = false;
        let mut until: Option<u64> = None;
        for page_no in 0..MAX_PAGES {
            if stop() || (page_no > 0 && !pause(self.pacing.page_gap, stop)) {
                return rest.cut(until, started, found);
            }
            let mut filter = Filter::new().kind(Kind::GiftWrap).pubkey(keep_pub).since(Timestamp::from_secs(since)).limit(limit);
            if let Some(until) = until {
                filter = filter.until(Timestamp::from_secs(until));
            }
            let mut sub = relay.subscribe(vec![filter]);
            let want = sub.relays();
            let (mut eose, mut page) = (0, Vec::new());
            let deadline = Instant::now() + PAGE_WAIT;
            while eose < want && Instant::now() < deadline && !stop() {
                match sub.try_recv() {
                    Some(RelayMsg::Event(wrap)) => page.push(*wrap),
                    Some(RelayMsg::Eose) => eose += 1,
                    None => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            // A page that did not finish in time (or a stop) was not read: it is not the
            // end of the window, and the cursor must not move past what it would have held.
            let unfinished = eose < want;
            let unreachable = sub.unreachable() > 0;
            let closed = match sub.status() {
                SubStatus::Closed(why) => Some(why),
                _ => None,
            };
            drop(sub);
            if unreachable || unfinished {
                if unfinished && !unreachable && !stop() {
                    log::warn!("[keep] a relay did not finish a page of the owner messages in time; the rest of its window is read again later");
                }
                return rest.cut(until, started, found);
            }
            // A relay that closes the request answers it with an empty page, which
            // is not the end of the window: what is older than this has not been read.
            if let Some(why) = closed {
                log::warn!("[keep] a relay refused a read of the owner messages ({}); the rest of its window is read again later", clean(&why));
                return rest.cut(until, started, found);
            }
            let Some(oldest) = page.iter().map(|w| w.created_at.as_secs()).min() else { return Read::Whole(found) };
            let full = page.len() >= limit;
            for wrap in page {
                if !met.insert(wrap.id) || seen.has(&wrap.id) {
                    continue;
                }
                // Only what the Keep acts on is held, and only if it has the
                // shape of one (a compact record of it, not the parsed rumor).
                let item = wire::unwrap(&keep, &wrap).ok().and_then(|(sender, rumor)| Backlog::hold(wrap.id, wrap.created_at.as_secs(), sender, &rumor, &keep_hex));
                let Some(item) = item else {
                    seen.first(wrap.id);
                    continue;
                };
                if counts_apart(&item) {
                    if kept >= BACKLOG_CAP {
                        if !warned {
                            log::warn!("[keep] the backlog holds more than {BACKLOG_CAP} messages from owner personas; the rest were left");
                            warned = true;
                        }
                        continue;
                    }
                    kept += 1;
                } else {
                    if others >= strangers {
                        continue;
                    }
                    others += 1;
                }
                found.push(item);
            }
            if others >= strangers && !warned_others {
                // No more from strangers is held, but the relay is read on: an
                // owner persona's older message must not be buried by them.
                log::warn!("[keep] a relay holds {strangers} or more messages for this Keep from keys that are not owner personas; no more of those are held");
                warned_others = true;
            }
            // `until` is inclusive, so the same second again is either the end of
            // the window (only what was just read is left) or a page full of
            // wraps made at one instant: either way step over that second
            // rather than read the same page for ever.
            let next = match until {
                Some(until) if oldest >= until => {
                    if full {
                        log::warn!("[keep] a relay holds {limit} or more wraps for this Keep made in the same second; reading past them");
                    }
                    until.saturating_sub(1)
                }
                _ => oldest,
            };
            if next < since {
                return Read::Whole(found);
            }
            // What an earlier read of this window has done for good is not read again.
            until = Some(match done {
                Some((lo, hi)) if next > lo && next < hi => lo,
                _ => next,
            });
        }
        log::warn!("[keep] a relay holds more wraps for this Keep than were read at start; the oldest were left");
        Read::Whole(found)
    }

    /// Apply what a start-up (or comeback) read found, oldest rumor first, in
    /// two passes (see below). The rumor's `created_at` is the sender's own
    /// date; messages made in the same second go claim, update, accept, then
    /// invites and revokes, and otherwise in the order read. `false` if a stop
    /// cut it short: the in-memory cursor may then be past messages not
    /// applied, so the caller must not save it.
    #[must_use]
    fn apply_backlog(&self, mut backlog: Vec<Backlog>, seen: &mut Seen, relays: &[Arc<dyn Relay>], stop: &dyn Fn() -> bool) -> bool {
        // One wrap that several relays sent is applied once, and one already
        // taken from another relay or the live feed not again. (A stop part way
        // leaves the rest marked but not applied; the Keep is stopping, and its
        // caller leaves the saved cursor where it was, so the next start reads them.)
        backlog.retain(|b| seen.first(b.wrap_id));
        backlog.sort_by_key(|b| (b.created_at, b.rank()));
        // Pass 1: claims, updates and accepts, which decide who the owner
        // personas are. Invites wait, and so does a revoke (but whether its
        // signer was active at its place in the order is noted now).
        let mut later: Vec<(Backlog, bool)> = Vec::new();
        for item in backlog {
            if stop() {
                return false;
            }
            match item.kind {
                KIND_INVITE => later.push((item, false)),
                KIND_REVOKE => {
                    let was_active = self.lock().is_active(&item.sender);
                    later.push((item, was_active));
                }
                _ => self.apply_one(&item, Source::Backlog { revoke_ok: false }, relays),
            }
        }
        // Pass 2: invites, judged against the owner personas as pass 1 left
        // them, so a persona removed anywhere in the backlog cannot have
        // back-dated one in. A revoke only takes access away: it counts if
        // its signer was active at its place or is at the end.
        for (item, was_active) in later {
            if stop() {
                return false;
            }
            self.apply_one(&item, Source::Backlog { revoke_ok: was_active }, relays);
        }
        true
    }

    fn apply_one(&self, item: &Backlog, source: Source, relays: &[Arc<dyn Relay>]) {
        let Some(rumor) = item.rumor() else { return };
        let before = self.claimed();
        let reply = self.lock().process_opened(item.wrap_at, item.sender, &rumor, unix_now(), source);
        self.finish(before, reply, relays);
    }

    /// Read one wrap and send the answer, if any, to every relay.
    fn answer(&self, wrap: &Event, relays: &[Arc<dyn Relay>]) {
        let before = self.claimed();
        let reply = self.process(wrap, unix_now());
        self.finish(before, reply, relays);
    }

    fn finish(&self, before: bool, reply: Option<Event>, relays: &[Arc<dyn Relay>]) {
        let Some(reply) = reply else { return };
        for relay in relays {
            relay.publish(&reply);
        }
        log::info!("{}", ack_sent_line(&reply, relays.len()));
        if !before && self.claimed() {
            log::info!("[keep] this Keep now has an owner; pairing is closed");
            super::emit(serde_json::json!({"event":"owner_paired"}));
        }
    }
}

/// A message from a start-up read, opened and waiting to be applied in order.
/// A compact record, not the parsed rumor: only the tags the Keep reads, as
/// plain strings.
struct Backlog {
    wrap_id: EventId,
    wrap_at: u64,
    /// The rumor's own date (the sender's).
    created_at: u64,
    sender: PublicKey,
    kind: u16,
    tags: Vec<Vec<String>>,
}

impl Backlog {
    /// Hold `rumor` (from the wrap `wrap_id`, made at `wrap_at`) if it is a
    /// message the Keep acts on for this Keep and has the shape of one: no
    /// content, few and short tags. `None` = leave it.
    fn hold(wrap_id: EventId, wrap_at: u64, sender: PublicKey, rumor: &UnsignedEvent, keep_hex: &str) -> Option<Backlog> {
        let Kind::Custom(kind) = rumor.kind else { return None };
        if !matches!(kind, KIND_CLAIM | KIND_INVITE | KIND_REVOKE | KIND_UPDATE | KIND_ACCEPT) || !rumor.content.is_empty() || rumor.tags.len() > MAX_RUMOR_TAGS {
            return None;
        }
        if one_tag(rumor, "keep").ok().flatten().map(str::to_ascii_lowercase).as_deref() != Some(keep_hex) {
            return None;
        }
        let mut tags: Vec<Vec<String>> = Vec::new();
        let mut personas = 0;
        for tag in rumor.tags.iter() {
            let parts = tag.as_slice();
            let fits = match parts.first().map(String::as_str) {
                Some("persona") => parts.len() <= MAX_PERSONA_PARTS && parts.iter().enumerate().all(|(i, part)| part.len() <= if i < 2 { MAX_TAG_PART_BYTES } else { MAX_SECTION_BYTES }),
                Some("name") => parts.len() <= MAX_TAG_PARTS && parts.iter().all(|part| part.len() <= MAX_NAME_BYTES),
                _ => parts.len() <= MAX_TAG_PARTS && parts.iter().all(|part| part.len() <= MAX_TAG_PART_BYTES),
            };
            if !fits {
                return None;
            }
            if parts.first().is_some_and(|name| KNOWN_TAGS.contains(&name.as_str())) {
                personas += usize::from(parts[0] == "persona");
                tags.push(parts.to_vec());
                if personas > MAX_PERSONA_TAGS || tags.len() - personas > MAX_OTHER_TAGS {
                    return None;
                }
            }
        }
        Some(Backlog { wrap_id, wrap_at, created_at: rumor.created_at.as_secs(), sender, kind, tags })
    }

    /// The value of the first tag called `name`.
    fn tag(&self, name: &str) -> Option<&str> {
        self.tags.iter().find(|t| t.first().map(String::as_str) == Some(name)).and_then(|t| t.get(1)).map(String::as_str)
    }

    /// The order messages made in one second are applied in: what the others depend on first.
    fn rank(&self) -> u8 {
        match self.kind {
            KIND_CLAIM => 0,
            KIND_UPDATE => 1,
            KIND_ACCEPT => 2,
            _ => 3,
        }
    }

    /// The rumor again, for the rules.
    fn rumor(&self) -> Option<UnsignedEvent> {
        let tags: Vec<Tag> = self.tags.iter().filter_map(|t| Tag::parse(t).ok()).collect();
        Some(UnsignedEvent::new(self.sender, Timestamp::from_secs(self.created_at), Kind::Custom(self.kind), tags, ""))
    }
}

/// Wraps remembered by id, so one that several relays send, or one sent again,
/// is read once.
#[derive(Default)]
struct Seen {
    ids: HashSet<EventId>,
    order: VecDeque<EventId>,
}

impl Seen {
    fn has(&self, id: &EventId) -> bool {
        self.ids.contains(id)
    }

    /// `true` the first time `id` is seen.
    fn first(&mut self, id: EventId) -> bool {
        if !self.ids.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > SEEN_CAP {
            if let Some(old) = self.order.pop_front() {
                self.ids.remove(&old);
            }
        }
        true
    }
}

/// Track a relay's status: `true` the moment it is up again after being seen down.
fn came_back(was_down: &mut bool, status: &SubStatus) -> bool {
    match status {
        SubStatus::Down(_) => {
            *was_down = true;
            false
        }
        SubStatus::Up if *was_down => {
            *was_down = false;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decented_signet::relay::FakeNetwork;
    use std::collections::HashMap;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;
    const CODE: [u8; 16] = [0x5a; 16];
    const GRANT: &str = "11111111111111111111111111111111";

    fn keys(n: u8) -> Keys {
        Keys::parse(&format!("{}{n:x}", "0".repeat(63))).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("owner-{name}-{}-{NOW}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tag(parts: &[&str]) -> Tag {
        Tag::parse(parts.iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn engine_in(dir: &Path) -> Engine {
        let invites = Arc::new(Invites::open(dir, keys(1).public_key().to_bytes()).unwrap());
        Engine::open(dir, keys(1), invites, CODE).unwrap()
    }

    fn engine(name: &str) -> (Engine, PathBuf) {
        let dir = scratch(name);
        (engine_in(&dir), dir)
    }

    /// What a reply says: tags by name.
    type Reply = HashMap<String, String>;

    /// Send a message through the whole path (wrap, process, unwrap the ack).
    fn send(engine: &mut Engine, from: &Keys, kind: u16, tags: Vec<Tag>, now: u64) -> Option<Reply> {
        let wrap = wire::gift_wrap(from, &keys(1).public_key(), kind, tags, now).unwrap();
        let ack = engine.process(&wrap, now)?;
        let (sender, rumor) = wire::unwrap(from, &ack).expect("an ack opens for the sender");
        assert_eq!(sender, keys(1).public_key(), "an ack is sealed by the Keep");
        assert_eq!(rumor.kind, Kind::Custom(KIND_ACK));
        assert_eq!(rumor.content, "");
        let mut reply = Reply::new();
        for t in rumor.tags.iter() {
            let t = t.as_slice();
            reply.insert(t[0].clone(), t.get(1).cloned().unwrap_or_default());
        }
        assert_eq!(reply["keep"], keys(1).public_key().to_hex());
        Some(reply)
    }

    fn keep_tag() -> Tag {
        tag(&["keep", &keys(1).public_key().to_hex()])
    }

    fn claim(engine: &mut Engine, from: &Keys, code: &[u8; 16]) -> Option<Reply> {
        send(engine, from, KIND_CLAIM, vec![keep_tag(), tag(&["code", &hex_encode(code)])], NOW)
    }

    fn claimed(name: &str) -> (Engine, PathBuf) {
        let (mut engine, dir) = engine(name);
        assert_eq!(claim(&mut engine, &keys(2), &CODE).unwrap()["status"], "paired");
        (engine, dir)
    }

    fn secret(n: u8) -> String {
        hex_encode(&[n; 32])
    }

    fn invite_tags(grant: &str, secret_n: u8, expiration: u64, who: &[&str]) -> Vec<Tag> {
        let mut tags = vec![keep_tag(), tag(&["grant", grant]), tag(&["secret", &secret(secret_n)]), tag(&["expiration", &expiration.to_string()])];
        tags.push(tag(who));
        tags
    }

    fn invite(engine: &mut Engine, grant: &str, secret_n: u8, expiration: u64, who: &[&str]) -> Option<Reply> {
        send(engine, &keys(2), KIND_INVITE, invite_tags(grant, secret_n, expiration, who), NOW)
    }

    fn revoke(engine: &mut Engine, grant: &str) -> Option<Reply> {
        send(engine, &keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", grant])], NOW)
    }

    fn refused(reply: Option<Reply>, reason: &str) {
        let reply = reply.expect("an ack");
        assert_eq!((reply["status"].as_str(), reply["reason"].as_str()), ("refused", reason));
    }

    fn live_secrets(engine: &Engine) -> Vec<[u8; 32]> {
        engine.invites.live(NOW).into_iter().map(|(_, s)| s).collect()
    }

    // ─── claim ──────────────────────────────────────────────────────────────

    #[test]
    fn the_right_code_claims_the_keep_once_and_closes_pairing() {
        let (mut engine, dir) = engine("claim");
        assert!(!engine.claimed());
        let reply = claim(&mut engine, &keys(2), &CODE).unwrap();
        assert_eq!((reply["status"].as_str(), reply.contains_key("claim"), reply.contains_key("grant")), ("paired", true, false));
        assert!(engine.claimed() && engine.code.is_none());
        let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(OWNER_FILE)).unwrap()).unwrap();
        assert_eq!(file["version"], 2);
        assert_eq!(file["owners"], serde_json::json!([{"persona": keys(2).public_key().to_hex(), "sections": [], "added_rev": 0, "accepted_rev": 0}]));
        assert_eq!((file["claimed_at"].as_u64(), file["rev"].as_u64(), file["scope"].as_str()), (Some(NOW), Some(0), Some("one")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join(OWNER_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // The next start knows the owner and makes no code.
        let again = engine_in(&dir);
        assert!(again.claimed() && again.code.is_none());
    }

    #[test]
    fn a_wrong_or_broken_code_gets_no_answer_and_changes_nothing() {
        let (mut engine, dir) = engine("wrongcode");
        assert!(claim(&mut engine, &keys(2), &[0x5b; 16]).is_none());
        let mut tags = vec![keep_tag(), tag(&["code", "zz"])];
        assert!(send(&mut engine, &keys(2), KIND_CLAIM, tags.clone(), NOW).is_none());
        tags.truncate(1);
        assert!(send(&mut engine, &keys(2), KIND_CLAIM, tags, NOW).is_none());
        // The right code for another Keep is not for this one.
        let other = vec![tag(&["keep", &keys(5).public_key().to_hex()]), tag(&["code", &hex_encode(&CODE)])];
        assert!(send(&mut engine, &keys(2), KIND_CLAIM, other, NOW).is_none());
        assert!(!engine.claimed() && !dir.join(OWNER_FILE).exists());
        // And the right one still works afterwards.
        assert_eq!(claim(&mut engine, &keys(2), &CODE).unwrap()["status"], "paired");
    }

    #[test]
    fn the_owner_claiming_again_is_told_paired_and_anyone_else_is_not() {
        let (mut engine, dir) = claimed("reclaim");
        let before = std::fs::read(dir.join(OWNER_FILE)).unwrap();
        // The owner again, with any code at all: the ack is repeated.
        assert_eq!(claim(&mut engine, &keys(2), &[0; 16]).unwrap()["status"], "paired");
        assert_eq!(std::fs::read(dir.join(OWNER_FILE)).unwrap(), before, "no change");
        // Someone else, even with the code the Keep once had: nothing.
        assert!(claim(&mut engine, &keys(3), &CODE).is_none());
        assert!(engine.is_active(&keys(2).public_key()) && !engine.is_active(&keys(3).public_key()));
        assert_eq!(engine.data.owners.len(), 1);
    }

    // ─── invite ─────────────────────────────────────────────────────────────

    #[test]
    fn only_the_owner_may_invite_or_revoke() {
        let (mut engine, _) = engine("nonowner-unclaimed");
        // Nobody owns the Keep yet.
        assert!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).is_none());
        let (mut engine, _) = claimed("nonowner");
        let intruder = keys(3);
        let wanted = invite_tags(GRANT, 1, NOW + DAY, &["guest"]);
        assert!(send(&mut engine, &intruder, KIND_INVITE, wanted, NOW).is_none());
        assert!(send(&mut engine, &intruder, KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], NOW).is_none());
        assert!(live_secrets(&engine).is_empty());
        // And the revoke did not leave a tombstone either: the owner's invite still works.
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
    }

    #[test]
    fn an_invite_for_another_keep_is_dropped() {
        let (mut engine, _) = claimed("otherkeep");
        let mut tags = invite_tags(GRANT, 1, NOW + DAY, &["guest"]);
        tags[0] = tag(&["keep", &keys(5).public_key().to_hex()]);
        assert!(send(&mut engine, &keys(2), KIND_INVITE, tags, NOW).is_none());
        let mut no_keep = invite_tags(GRANT, 1, NOW + DAY, &["guest"]);
        no_keep.remove(0);
        assert!(send(&mut engine, &keys(2), KIND_INVITE, no_keep, NOW).is_none());
        assert!(live_secrets(&engine).is_empty());
    }

    #[test]
    fn a_personal_invite_and_a_guest_pass_go_into_the_table_as_the_owners() {
        let (mut engine, dir) = claimed("invite");
        let friend = keys(7);
        let mut tags = invite_tags(GRANT, 1, NOW + 30 * DAY, &["p", &friend.public_key().to_hex()]);
        tags.push(tag(&["name", "Ada"]));
        tags.push(tag(&["unknown", "ignored"]));
        let reply = send(&mut engine, &keys(2), KIND_INVITE, tags, NOW).unwrap();
        assert_eq!((reply["status"].as_str(), reply["grant"].as_str(), reply.contains_key("reason")), ("active", GRANT, false));
        let guest = "22222222222222222222222222222222";
        assert_eq!(invite(&mut engine, guest, 2, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        let views = engine.invites.views(NOW);
        assert_eq!(views.len(), 2);
        let personal = views.iter().find(|v| v.kind == "personal").unwrap();
        assert_eq!((personal.source, personal.label.as_deref(), personal.holder.as_deref()), ("owner", Some("Ada"), Some(friend.public_key().to_hex().as_str())));
        assert_eq!((personal.expires_at, personal.status), (NOW + 30 * DAY, "live"));
        let pass = views.iter().find(|v| v.kind == "guest").unwrap();
        assert_eq!((pass.source, pass.holder.clone(), pass.expires_at), ("owner", None, NOW + DAY));
        assert!(personal.link.is_none() && pass.link.is_none(), "the page never shows an owner's link");
        // Fray is to hold both secrets, and the table is on disk with the source.
        let mut live = live_secrets(&engine);
        live.sort();
        assert_eq!(live, vec![[1u8; 32], [2u8; 32]]);
        assert!(engine.invites.take_dirty());
        let text = std::fs::read_to_string(dir.join("invites.json")).unwrap();
        assert!(text.contains("\"source\": \"owner\""), "{text}");
        // A restart reads it back.
        let again = Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap();
        assert_eq!(again.live(NOW).len(), 2);
    }

    #[test]
    fn the_same_invite_again_is_acknowledged_again_and_made_once() {
        let (mut engine, _) = claimed("dup");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        assert_eq!(engine.invites.views(NOW).len(), 1);
        assert_eq!(engine.grants.len(), 1);
    }

    #[test]
    fn the_state_file_holds_no_invite_secret() {
        let (mut engine, dir) = claimed("nosecret");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        let state = std::fs::read_to_string(dir.join(STATE_FILE)).unwrap();
        assert!(!state.contains(&secret(1)), "{state}");
        // Still told apart from a different invite, and known again after a restart.
        let mut again = engine_in(&dir);
        assert_eq!(invite(&mut again, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        refused(invite(&mut again, GRANT, 2, NOW + DAY, &["guest"]), "conflict");
    }

    #[test]
    fn a_state_file_from_before_has_its_secrets_swapped_for_ids_and_keeps_working() {
        let (mut engine, dir) = claimed("oldstate");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        // Write the file as the first version did: the secret in the fingerprint, no expiry.
        let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(STATE_FILE)).unwrap()).unwrap();
        let grant = &mut value["grants"][0];
        grant["tags"] = serde_json::json!(format!("{}|{}|guest|", secret(1), NOW + DAY));
        grant.as_object_mut().unwrap().remove("expires_at");
        std::fs::write(dir.join(STATE_FILE), value.to_string()).unwrap();
        let mut again = engine_in(&dir);
        let state = std::fs::read_to_string(dir.join(STATE_FILE)).unwrap();
        assert!(!state.contains(&secret(1)), "the secret is gone from the file at once: {state}");
        assert_eq!(again.grants[0].expires_at, Some(NOW + DAY));
        assert_eq!(invite(&mut again, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        refused(invite(&mut again, GRANT, 2, NOW + DAY, &["guest"]), "conflict");
    }

    #[test]
    fn a_grant_is_forgotten_a_day_after_its_invite_expires_and_a_replay_is_then_expired() {
        let (mut engine, _) = claimed("prune");
        let expiry = NOW + DAY;
        let tags = || invite_tags(GRANT, 1, expiry, &["guest"]);
        assert_eq!(send(&mut engine, &keys(2), KIND_INVITE, tags(), NOW).unwrap()["status"], "active");
        // Within the margin it is still known: a repeat is `active`.
        let within = expiry + DAY - 1;
        assert_eq!(send(&mut engine, &keys(2), KIND_INVITE, tags(), within).unwrap()["status"], "active");
        assert_eq!(engine.grants.len(), 1);
        // A day past the expiry it is let go, and a replay is refused `expired`.
        let after = expiry + DAY;
        refused(send(&mut engine, &keys(2), KIND_INVITE, tags(), after), "expired");
        assert!(engine.grants.is_empty());
        // The same for one that was revoked.
        let other = "22222222222222222222222222222222";
        assert_eq!(send(&mut engine, &keys(2), KIND_INVITE, invite_tags(other, 2, after + DAY, &["guest"]), after).unwrap()["status"], "active");
        assert_eq!(send(&mut engine, &keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", other])], after).unwrap()["status"], "revoked");
        assert_eq!(engine.grants.len(), 1);
        let later = after + 2 * DAY + 1;
        refused(send(&mut engine, &keys(2), KIND_INVITE, invite_tags(other, 2, after + DAY, &["guest"]), later), "expired");
        assert!(engine.grants.is_empty());
    }

    #[test]
    fn a_full_grant_table_makes_room_by_forgetting_the_run_out() {
        let (mut engine, _) = claimed("prune-full");
        for n in 0..MAX_GRANTS {
            engine.grants.push(Grant {
                grant: format!("{n:032x}"),
                invite: String::new(),
                state: GrantState::Revoked,
                tags: String::new(),
                expires_at: Some(NOW - 2 * DAY),
                recorded_at: None,
            });
        }
        // All run out: a new invite is not `full`.
        assert_eq!(invite(&mut engine, "ffffffffffffffffffffffffffffffff", 9, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        assert_eq!(engine.grants.len(), 1);
        // Ones that have not run out still fill it.
        for n in 0..MAX_GRANTS - 1 {
            engine.grants.push(Grant {
                grant: format!("{n:032x}"),
                invite: String::new(),
                state: GrantState::Revoked,
                tags: String::new(),
                expires_at: None,
                recorded_at: Some(NOW),
            });
        }
        refused(invite(&mut engine, "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", 8, NOW + DAY, &["guest"]), "full");
    }

    #[test]
    fn a_revoke_that_came_first_is_kept_for_a_year_and_a_day() {
        let (mut engine, _) = claimed("prune-first");
        assert_eq!(revoke(&mut engine, GRANT).unwrap()["status"], "revoked");
        let tags = |now: u64| invite_tags(GRANT, 1, now + DAY, &["guest"]);
        let almost = NOW + MAX_EXPIRY_SECS + PRUNE_MARGIN_SECS - 1;
        refused(send(&mut engine, &keys(2), KIND_INVITE, tags(almost), almost), "revoked");
        let past = NOW + MAX_EXPIRY_SECS + PRUNE_MARGIN_SECS;
        assert_eq!(send(&mut engine, &keys(2), KIND_INVITE, tags(past), past).unwrap()["status"], "active");
    }

    #[test]
    fn the_same_grant_with_different_tags_is_a_conflict() {
        let (mut engine, _) = claimed("conflict");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        refused(invite(&mut engine, GRANT, 1, NOW + 2 * DAY, &["guest"]), "conflict");
        refused(invite(&mut engine, GRANT, 2, NOW + DAY, &["guest"]), "conflict");
        refused(invite(&mut engine, GRANT, 1, NOW + DAY, &["p", &keys(7).public_key().to_hex()]), "conflict");
        let mut named = invite_tags(GRANT, 1, NOW + DAY, &["guest"]);
        named.push(tag(&["name", "Ada"]));
        refused(send(&mut engine, &keys(2), KIND_INVITE, named, NOW), "conflict");
        assert_eq!(engine.invites.views(NOW).len(), 1, "the first invite stands");
    }

    #[test]
    fn a_secret_already_in_the_table_is_a_conflict_for_another_grant() {
        let (mut engine, _) = claimed("secretclash");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        refused(invite(&mut engine, "33333333333333333333333333333333", 1, NOW + DAY, &["guest"]), "conflict");
        // One the status page made is no different.
        engine.invites.create_guest(None, NOW).unwrap();
        let page_made = engine.invites.live(NOW).into_iter().find(|(_, s)| *s != [1u8; 32]).unwrap().1;
        let tags = vec![keep_tag(), tag(&["grant", "44444444444444444444444444444444"]), tag(&["secret", &hex_encode(&page_made)]), tag(&["expiration", &(NOW + DAY).to_string()]), tag(&["guest"])];
        refused(send(&mut engine, &keys(2), KIND_INVITE, tags, NOW), "conflict");
    }

    #[test]
    fn expiry_is_checked_against_now_and_a_year() {
        let (mut engine, _) = claimed("expiry");
        refused(invite(&mut engine, GRANT, 1, NOW, &["guest"]), "expired");
        refused(invite(&mut engine, GRANT, 1, NOW - DAY, &["guest"]), "expired");
        refused(invite(&mut engine, "22222222222222222222222222222222", 2, NOW + 365 * DAY + 1, &["guest"]), "expiry_too_long");
        assert!(engine.invites.views(NOW).is_empty() && engine.grants.is_empty(), "a refused invite leaves nothing");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + 1, &["guest"]).unwrap()["status"], "active");
        assert_eq!(invite(&mut engine, "22222222222222222222222222222222", 2, NOW + 365 * DAY, &["guest"]).unwrap()["status"], "active");
    }

    #[test]
    fn a_malformed_invite_with_a_readable_grant_is_refused_and_one_without_is_dropped() {
        let (mut engine, _) = claimed("malformed");
        let friend = keys(7).public_key().to_hex();
        let exp = (NOW + DAY).to_string();
        let base = |extra: Vec<Tag>| {
            let mut tags = vec![keep_tag(), tag(&["grant", GRANT])];
            tags.extend(extra);
            tags
        };
        let secret = secret(1);
        let cases: Vec<(&str, Vec<Tag>)> = vec![
            ("neither p nor guest", base(vec![tag(&["secret", &secret]), tag(&["expiration", &exp])])),
            ("both p and guest", base(vec![tag(&["secret", &secret]), tag(&["expiration", &exp]), tag(&["p", &friend]), tag(&["guest"])])),
            ("two p", base(vec![tag(&["secret", &secret]), tag(&["expiration", &exp]), tag(&["p", &friend]), tag(&["p", &friend])])),
            ("bad persona", base(vec![tag(&["secret", &secret]), tag(&["expiration", &exp]), tag(&["p", "xyz"])])),
            ("no secret", base(vec![tag(&["expiration", &exp]), tag(&["guest"])])),
            ("short secret", base(vec![tag(&["secret", "abcd"]), tag(&["expiration", &exp]), tag(&["guest"])])),
            ("no expiration", base(vec![tag(&["secret", &secret]), tag(&["guest"])])),
            ("bad expiration", base(vec![tag(&["secret", &secret]), tag(&["expiration", "soon"]), tag(&["guest"])])),
            ("long name", base(vec![tag(&["secret", &secret]), tag(&["expiration", &exp]), tag(&["guest"]), tag(&["name", &"x".repeat(41)])])),
            ("two secrets", base(vec![tag(&["secret", &secret]), tag(&["secret", &secret]), tag(&["expiration", &exp]), tag(&["guest"])])),
        ];
        for (what, tags) in cases {
            let reply = send(&mut engine, &keys(2), KIND_INVITE, tags, NOW);
            assert_eq!(reply.as_ref().map(|r| (r["status"].as_str(), r["reason"].as_str(), r["grant"].as_str())), Some(("refused", "malformed", GRANT)), "{what}");
        }
        // A name of exactly 40 is fine.
        let ok = base(vec![tag(&["secret", &secret]), tag(&["expiration", &exp]), tag(&["guest"]), tag(&["name", &"x".repeat(40)])]);
        assert_eq!(send(&mut engine, &keys(2), KIND_INVITE, ok, NOW).unwrap()["status"], "active");
        // Nothing to answer to when the grant itself cannot be read.
        for grant in [vec![], vec![tag(&["grant", "short"])], vec![tag(&["grant", GRANT]), tag(&["grant", GRANT])]] {
            let mut tags = vec![keep_tag(), tag(&["secret", &secret]), tag(&["expiration", &exp]), tag(&["guest"])];
            tags.extend(grant);
            assert!(send(&mut engine, &keys(2), KIND_INVITE, tags, NOW).is_none());
        }
    }

    #[test]
    fn a_full_table_refuses_and_remembers_nothing() {
        let (mut engine, _) = claimed("full");
        for n in 0..decented_net::MAX_INVITES {
            let grant = format!("{n:032x}");
            let mut sec = [0u8; 32];
            sec[..8].copy_from_slice(&(n as u64 + 1).to_le_bytes());
            let tags = vec![keep_tag(), tag(&["grant", &grant]), tag(&["secret", &hex_encode(&sec)]), tag(&["expiration", &(NOW + DAY).to_string()]), tag(&["guest"])];
            assert_eq!(send(&mut engine, &keys(2), KIND_INVITE, tags, NOW).unwrap()["status"], "active", "{n}");
        }
        let extra = "ffffffffffffffffffffffffffffffff";
        refused(invite(&mut engine, extra, 250, NOW + DAY, &["guest"]), "full");
        assert_eq!(engine.grants.len(), decented_net::MAX_INVITES);
        // Revoking one makes room, and the refused grant can be sent again.
        assert_eq!(revoke(&mut engine, &format!("{:032x}", 0)).unwrap()["status"], "revoked");
        assert_eq!(invite(&mut engine, extra, 250, NOW + DAY, &["guest"]).unwrap()["status"], "active");
    }

    #[test]
    fn a_table_that_cannot_be_written_gets_no_ack_and_no_grant() {
        let (mut engine, dir) = claimed("unwritable");
        let invites_file = dir.join("invites.json");
        // The table's file is where a directory has to be: writing it fails.
        std::fs::create_dir(&invites_file).unwrap();
        assert!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).is_none());
        assert!(engine.grants.is_empty(), "the grant is forgotten so a retry can work");
        std::fs::remove_dir(&invites_file).unwrap();
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
    }

    #[test]
    fn a_revoke_the_disk_cannot_save_still_holds_for_this_run_and_is_finished_by_a_retry() {
        let (mut engine, dir) = claimed("revoke-unwritable");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        let invites_file = dir.join("invites.json");
        std::fs::remove_file(&invites_file).unwrap();
        std::fs::create_dir(&invites_file).unwrap();
        // No ack, since it is not saved; but Fray drops the invite and its players are kicked.
        assert!(revoke(&mut engine, GRANT).is_none());
        assert!(live_secrets(&engine).is_empty(), "the invite is dead for this run");
        assert_eq!(engine.invites.take_revoked(), vec![InviteId::of(&[1u8; 32])]);
        assert_eq!(engine.grants[0].state, GrantState::Revoked, "the owner state has it");
        // The disk comes back: the retry saves it and is acknowledged.
        std::fs::remove_dir(&invites_file).unwrap();
        assert_eq!(revoke(&mut engine, GRANT).unwrap()["status"], "revoked");
        let reread = Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap();
        assert!(reread.live(NOW).is_empty(), "revoked on disk too");
    }

    /// Make `owner-state.json` unwritable (a directory where the file goes), as a full disk does.
    fn break_state_file(dir: &Path) {
        let file = dir.join(STATE_FILE);
        let _ = std::fs::remove_file(&file);
        std::fs::create_dir(&file).unwrap();
    }

    fn mend_state_file(dir: &Path) {
        std::fs::remove_dir(dir.join(STATE_FILE)).unwrap();
    }

    #[test]
    fn a_revoke_the_state_file_cannot_save_still_keeps_the_friend_out_and_is_acknowledged_once_saved() {
        let (mut engine, dir) = claimed("revoke-state-unwritable");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        let id = InviteId::of(&[1u8; 32]);
        assert!(engine.invites.admit(id, &[9u8; 32], NOW).is_ok(), "the friend can join before the revoke");
        engine.invites.take_revoked();
        break_state_file(&dir);
        // No ack, but the revoke holds: the invite is dead, the players are kicked, the grant is revoked.
        assert!(revoke(&mut engine, GRANT).is_none());
        assert!(engine.invites.admit(id, &[9u8; 32], NOW).is_err(), "refused at admit");
        assert!(engine.invites.admit(id, &[8u8; 32], NOW).is_err());
        assert!(live_secrets(&engine).is_empty());
        assert_eq!(engine.invites.take_revoked(), vec![id]);
        assert_eq!(engine.grants[0].state, GrantState::Revoked);
        // The owner's resend is still not acknowledged while the file cannot be saved.
        assert!(revoke(&mut engine, GRANT).is_none());
        refused(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]), "revoked");
        // The disk comes back: the next resend saves the state and is acknowledged.
        mend_state_file(&dir);
        assert_eq!(revoke(&mut engine, GRANT).unwrap()["status"], "revoked");
        assert!(std::fs::read_to_string(dir.join(STATE_FILE)).unwrap().contains("\"revoked\""));
        let reopened = engine_in(&dir);
        assert_eq!(reopened.grants[0].state, GrantState::Revoked);
        assert!(reopened.invites.live(NOW).is_empty());
    }

    #[test]
    fn a_revoke_that_came_first_and_cannot_be_saved_still_refuses_the_late_invite() {
        let (mut engine, dir) = claimed("revoke-first-state-unwritable");
        break_state_file(&dir);
        assert!(revoke(&mut engine, GRANT).is_none(), "not acknowledged");
        assert_eq!(engine.grants.len(), 1, "kept in memory");
        refused(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]), "revoked");
        assert!(live_secrets(&engine).is_empty());
        mend_state_file(&dir);
        assert_eq!(revoke(&mut engine, GRANT).unwrap()["status"], "revoked");
        assert_eq!(engine_in(&dir).grants[0].state, GrantState::Revoked);
    }

    #[test]
    fn a_start_applies_the_revokes_the_invite_table_missed() {
        let (mut engine, dir) = claimed("revoke-reconcile");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        // A crash between the two writes: the owner state says revoked, the table does not.
        let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(STATE_FILE)).unwrap()).unwrap();
        value["grants"][0]["state"] = serde_json::json!("revoked");
        value["grants"][0]["tags"] = serde_json::json!("");
        std::fs::write(dir.join(STATE_FILE), value.to_string()).unwrap();
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        assert_eq!(invites.live(unix_now()).len(), 1);
        let _engine = Engine::open(&dir, keys(1), invites.clone(), CODE).unwrap();
        assert!(invites.live(unix_now()).is_empty(), "revoked at start");
        assert_eq!(invites.take_revoked(), vec![InviteId::of(&[1u8; 32])]);
        let reread = Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap();
        assert_eq!(reread.views(NOW)[0].status, "revoked", "and saved");
    }

    #[test]
    fn an_invite_the_page_cancelled_is_not_acknowledged_active_and_is_not_brought_back() {
        let (mut engine, dir) = claimed("page-cancelled");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        // The page (the Keep had no owner for a moment: an owner reset) cancels it.
        assert!(engine.invites.revoke(&InviteId::of(&[1u8; 32]).to_string()).unwrap());
        // The owner sends it again: told it is revoked, and the grant is closed.
        refused(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]), "revoked");
        assert_eq!(engine.grants[0].state, GrantState::Revoked);
        assert!(live_secrets(&engine).is_empty());
        // Cancelled by the page while the Keep was stopped: seen at the next start.
        let other = "22222222222222222222222222222222";
        assert_eq!(send(&mut engine, &keys(2), KIND_INVITE, invite_tags(other, 2, NOW + DAY, &["guest"]), NOW).unwrap()["status"], "active");
        let page = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        page.revoke(&InviteId::of(&[2u8; 32]).to_string()).unwrap();
        let mut again = Engine::open(&dir, keys(1), page, CODE).unwrap();
        assert!(again.grants.iter().all(|g| g.state == GrantState::Revoked));
        refused(send(&mut again, &keys(2), KIND_INVITE, invite_tags(other, 2, NOW + DAY, &["guest"]), NOW), "revoked");
        assert!(again.invites.live(NOW).is_empty());
    }

    // ─── revoke ─────────────────────────────────────────────────────────────

    #[test]
    fn a_revoke_cancels_the_invite_and_drops_its_players() {
        let (mut engine, _) = claimed("revoke");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        assert!(engine.invites.take_revoked().is_empty());
        let reply = revoke(&mut engine, GRANT).unwrap();
        assert_eq!((reply["status"].as_str(), reply["grant"].as_str()), ("revoked", GRANT));
        assert!(live_secrets(&engine).is_empty(), "Fray is told to drop it");
        assert_eq!(engine.invites.views(NOW)[0].status, "revoked");
        // The room's loop kicks whoever it seated.
        assert_eq!(engine.invites.take_revoked(), vec![InviteId::of(&[1u8; 32])]);
        // Again: the same answer.
        assert_eq!(revoke(&mut engine, GRANT).unwrap()["status"], "revoked");
        // And the invite cannot come back under that grant.
        refused(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]), "revoked");
        assert!(live_secrets(&engine).is_empty());
    }

    #[test]
    fn a_revoke_before_the_invite_is_final() {
        let (mut engine, dir) = claimed("revokefirst");
        assert_eq!(revoke(&mut engine, GRANT).unwrap()["status"], "revoked");
        refused(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]), "revoked");
        assert!(engine.invites.views(NOW).is_empty());
        // It survives a restart.
        let mut again = engine_in(&dir);
        refused(invite(&mut again, GRANT, 1, NOW + DAY, &["guest"]), "revoked");
        // A different grant is unaffected.
        assert_eq!(invite(&mut again, "22222222222222222222222222222222", 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
    }

    #[test]
    fn a_revoke_of_a_malformed_grant_is_dropped() {
        let (mut engine, _) = claimed("revokebad");
        assert!(send(&mut engine, &keys(2), KIND_REVOKE, vec![keep_tag()], NOW).is_none());
        assert!(send(&mut engine, &keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", "nope"])], NOW).is_none());
        assert!(engine.grants.is_empty());
    }

    #[test]
    fn grants_and_the_active_invite_survive_a_restart() {
        let (mut engine, dir) = claimed("restart");
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        let mut again = engine_in(&dir);
        assert_eq!(invite(&mut again, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        refused(invite(&mut again, GRANT, 1, NOW + 2 * DAY, &["guest"]), "conflict");
        assert_eq!(revoke(&mut again, GRANT).unwrap()["status"], "revoked");
        assert!(live_secrets(&again).is_empty());
    }

    // ─── K1e: owner personas, revisions, sections ───────────────────────────

    fn hex_of(n: u8) -> String {
        keys(n).public_key().to_hex()
    }

    /// An owners update: `who` are key numbers, each listed with `games`.
    fn update_tags(rev: &str, who: &[u8]) -> Vec<Tag> {
        let mut tags = vec![keep_tag(), tag(&["rev", rev])];
        tags.extend(who.iter().map(|n| tag(&["persona", &hex_of(*n), "games"])));
        tags
    }

    fn update(engine: &mut Engine, from: u8, rev: u64, who: &[u8]) -> Option<Reply> {
        send(engine, &keys(from), KIND_UPDATE, update_tags(&rev.to_string(), who), NOW)
    }

    fn accept(engine: &mut Engine, from: u8, rev: u64) -> Option<Reply> {
        send(engine, &keys(from), KIND_ACCEPT, vec![keep_tag(), tag(&["rev", &rev.to_string()])], NOW)
    }

    fn invite_from(engine: &mut Engine, from: u8, grant: &str, secret_n: u8) -> Option<Reply> {
        send(engine, &keys(from), KIND_INVITE, invite_tags(grant, secret_n, NOW + DAY, &["guest"]), NOW)
    }

    fn rev_status(reply: Option<Reply>, status: &str, rev: u64) {
        let reply = reply.expect("an ack");
        assert_eq!((reply["status"].as_str(), reply["rev"].as_str()), (status, rev.to_string().as_str()));
        assert!(!reply.contains_key("grant") && !reply.contains_key("claim"));
    }

    fn refused_rev(reply: Option<Reply>, reason: &str, rev: u64) {
        let reply = reply.expect("an ack");
        assert_eq!((reply["status"].as_str(), reply["reason"].as_str(), reply["rev"].as_str()), ("refused", reason, rev.to_string().as_str()));
    }

    /// Owner 2, and 3 added at rev 1 and accepted.
    fn two_owners(name: &str) -> (Engine, PathBuf) {
        let (mut engine, dir) = claimed(name);
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        rev_status(accept(&mut engine, 3, 1), "updated", 1);
        (engine, dir)
    }

    fn file_json(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(OWNER_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn a_claim_stores_its_scope_and_a_bad_scope_is_not_a_claim() {
        for (scope, stored) in [(Some("all"), "all"), (Some("one"), "one"), (None, "one")] {
            let (mut engine, dir) = engine("scope");
            let mut tags = vec![keep_tag(), tag(&["code", &hex_encode(&CODE)])];
            tags.extend(scope.map(|s| tag(&["scope", s])));
            assert_eq!(send(&mut engine, &keys(2), KIND_CLAIM, tags, NOW).unwrap()["status"], "paired");
            assert_eq!(file_json(&dir)["scope"], stored);
            assert_eq!(engine_in(&dir).data.scope, stored);
        }
        for bad in ["both", "ALL", ""] {
            let (mut engine, _) = engine("scope-bad");
            let tags = vec![keep_tag(), tag(&["code", &hex_encode(&CODE)]), tag(&["scope", bad])];
            assert!(send(&mut engine, &keys(2), KIND_CLAIM, tags, NOW).is_none(), "{bad}");
            assert!(!engine.claimed(), "the code is still good");
            assert_eq!(claim(&mut engine, &keys(2), &CODE).unwrap()["status"], "paired");
        }
    }

    #[test]
    fn a_k1d_owner_file_becomes_a_k1e_one_in_place_with_the_claimer_active_at_rev_0() {
        let dir = scratch("migrate");
        let owner_hex = hex_of(2);
        std::fs::write(dir.join(OWNER_FILE), format!("{{\"version\":1,\"owners\":[\"{owner_hex}\"],\"claimed_at\":1234}}")).unwrap();
        let mut engine = engine_in(&dir);
        assert!(engine.claimed() && engine.code.is_none());
        let file = file_json(&dir);
        assert_eq!(file["version"], 2);
        assert_eq!(file["owners"], serde_json::json!([{"persona": owner_hex, "sections": [], "added_rev": 0, "accepted_rev": 0}]));
        assert_eq!((file["claimed_at"].as_u64(), file["rev"].as_u64(), file["scope"].as_str()), (Some(1234), Some(0), Some("one")));
        // The claimer works as before and can move to rev 1.
        assert_eq!(invite_from(&mut engine, 2, GRANT, 1).unwrap()["status"], "active");
        assert_eq!(claim(&mut engine, &keys(2), &[0; 16]).unwrap()["status"], "paired", "a lost claim ack is re-sent");
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        // Opened again it is the K1e file, not migrated a second time.
        let before = std::fs::read(dir.join(OWNER_FILE)).unwrap();
        let again = engine_in(&dir);
        assert_eq!(again.data.rev, 1);
        assert_eq!(std::fs::read(dir.join(OWNER_FILE)).unwrap(), before);
    }

    #[test]
    fn a_k1d_file_the_disk_will_not_rewrite_is_read_as_k1e_and_written_by_the_next_change() {
        let dir = scratch("migrate-unwritable");
        std::fs::write(dir.join(OWNER_FILE), format!("{{\"version\":1,\"owners\":[\"{}\"],\"claimed_at\":1234}}", hex_of(2))).unwrap();
        // A directory where the temp file goes makes the rewrite fail.
        std::fs::create_dir(dir.join(format!(".{OWNER_FILE}.tmp"))).unwrap();
        let mut engine = engine_in(&dir);
        assert!(engine.claimed() && engine.is_active(&keys(2).public_key()), "not a damaged file");
        assert_eq!(file_json(&dir)["version"], 1);
        std::fs::remove_dir(dir.join(format!(".{OWNER_FILE}.tmp"))).unwrap();
        rev_status(update(&mut engine, 2, 1, &[2]), "updated", 1);
        assert_eq!(file_json(&dir)["version"], 2);
    }

    #[test]
    fn a_damaged_k1e_owner_file_is_an_error_and_is_left_alone() {
        let owner = |extra: &str| format!("{{\"version\":2,\"claimed_at\":1,\"scope\":\"one\",\"rev\":0,{extra}}}");
        let persona = |hex: &str, added: u64, accepted: &str| format!("{{\"persona\":\"{hex}\",\"sections\":[],\"added_rev\":{added},\"accepted_rev\":{accepted}}}");
        let (a, b) = (hex_of(2), hex_of(3));
        let cases = [
            owner(&format!("\"owners\":[{}]", persona("zz", 0, "0"))),
            owner(&format!("\"owners\":[{},{}]", persona(&a, 0, "0"), persona(&a, 0, "0"))),
            owner(&format!("\"owners\":[{}]", persona(&a, 1, "null"))),
            owner("\"owners\":[]").to_string(),
            owner(&format!("\"owners\":[{}", persona(&b, 0, "0"))),
            "{\"version\":3,\"owners\":[]}".to_string(),
            "{\"owners\":[]}".to_string(),
        ];
        for text in cases {
            let dir = scratch("damaged-k1e");
            let invites = Arc::new(Invites::open(&dir, [1; 32]).unwrap());
            std::fs::write(dir.join(OWNER_FILE), &text).unwrap();
            let error = Engine::open(&dir, keys(1), invites, CODE).err().expect(&text);
            assert!(error.starts_with(&format!("{OWNER_FILE} couldn't be read")), "{error}");
            assert_eq!(std::fs::read_to_string(dir.join(OWNER_FILE)).unwrap(), text, "left alone");
        }
    }

    #[test]
    fn an_update_from_anyone_but_an_active_owner_is_dropped_without_a_word() {
        // Not before any claim.
        let (mut fresh, _) = engine("update-unclaimed");
        assert!(update(&mut fresh, 2, 1, &[2]).is_none() && accept(&mut fresh, 2, 1).is_none());
        assert!(!fresh.claimed());
        let (mut engine, dir) = claimed("update-nonowner");
        let before = std::fs::read(dir.join(OWNER_FILE)).unwrap();
        assert!(update(&mut engine, 4, 1, &[4]).is_none(), "a stranger");
        // Listed but not yet accepted is not active either.
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        assert!(update(&mut engine, 3, 2, &[3]).is_none(), "listed, no accept yet");
        assert_eq!(engine.data.rev, 1);
        assert!(engine.is_listed(&keys(3).public_key()) && !engine.is_active(&keys(3).public_key()));
        assert_ne!(std::fs::read(dir.join(OWNER_FILE)).unwrap(), before, "only the real update changed the file");
    }

    #[test]
    fn an_update_lists_personas_at_a_new_revision_and_a_listed_persona_must_accept_to_act() {
        let (mut engine, dir) = claimed("update-accept");
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        let file = file_json(&dir);
        assert_eq!(file["rev"], 1);
        assert_eq!(file["owners"][1], serde_json::json!({"persona": hex_of(3), "sections": ["games"], "added_rev": 1, "accepted_rev": null}));
        assert_eq!(file["owners"][0]["added_rev"], 0, "the claimer keeps its place");
        // 3 is listed but cannot invite, revoke or update until it accepts.
        assert!(invite_from(&mut engine, 3, GRANT, 1).is_none());
        assert!(revoke_from(&mut engine, 3, GRANT).is_none());
        assert!(live_secrets(&engine).is_empty());
        rev_status(accept(&mut engine, 3, 1), "updated", 1);
        assert_eq!(invite_from(&mut engine, 3, GRANT, 1).unwrap()["status"], "active");
        assert_eq!(revoke_from(&mut engine, 3, GRANT).unwrap()["status"], "revoked");
        // And 2 still can. A second accept, or an accept from an active persona, only re-acks.
        assert_eq!(invite_from(&mut engine, 2, "22222222222222222222222222222222", 2).unwrap()["status"], "active");
        rev_status(accept(&mut engine, 3, 1), "updated", 1);
        rev_status(accept(&mut engine, 2, 0), "updated", 1);
        // Everything is on disk.
        let again = engine_in(&dir);
        assert!(again.is_active(&keys(2).public_key()) && again.is_active(&keys(3).public_key()));
        assert_eq!(again.data.rev, 1);
    }

    fn revoke_from(engine: &mut Engine, from: u8, grant: &str) -> Option<Reply> {
        send(engine, &keys(from), KIND_REVOKE, vec![keep_tag(), tag(&["grant", grant])], NOW)
    }

    #[test]
    fn a_stale_revision_is_refused_with_the_current_one_and_changes_nothing() {
        let (mut engine, dir) = two_owners("stale");
        let before = std::fs::read(dir.join(OWNER_FILE)).unwrap();
        refused_rev(update(&mut engine, 2, 1, &[2]), "stale", 1);
        refused_rev(update(&mut engine, 3, 0, &[3]), "stale", 1);
        rev_status(update(&mut engine, 2, 5, &[2, 3]), "updated", 5);
        refused_rev(update(&mut engine, 2, 4, &[2]), "stale", 5);
        refused_rev(update(&mut engine, 2, 5, &[2]), "stale", 5);
        assert!(engine.is_active(&keys(3).public_key()), "3 kept its accept through the new revision");
        assert_ne!(std::fs::read(dir.join(OWNER_FILE)).unwrap(), before);
        // rev 0 right after a claim is stale too.
        let (mut fresh, _) = claimed("stale-zero");
        refused_rev(update(&mut fresh, 2, 0, &[2]), "stale", 0);
    }

    #[test]
    fn an_update_identical_to_the_one_that_set_the_revision_is_updated_again_not_stale() {
        let (mut engine, dir) = two_owners("update-repeat");
        // 2's update (rev 1) again, as a lost ack or a replay would bring it.
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        assert_eq!(file_json(&dir)["rev_by"], hex_of(2));
        assert_eq!(engine_in(&dir).data, engine.data);
        // Not the same update: another list, another section, another signer, another revision.
        refused_rev(update(&mut engine, 2, 1, &[2]), "stale", 1);
        refused_rev(update(&mut engine, 2, 1, &[3, 2]), "stale", 1);
        let tags = vec![keep_tag(), tag(&["rev", "1"]), tag(&["persona", &hex_of(2), "games", "extra"]), tag(&["persona", &hex_of(3), "games"])];
        refused_rev(send(&mut engine, &keys(2), KIND_UPDATE, tags, NOW), "stale", 1);
        refused_rev(update(&mut engine, 3, 1, &[2, 3]), "stale", 1);
        refused_rev(update(&mut engine, 2, 0, &[2, 3]), "stale", 1);
        assert_eq!(engine.data.rev, 1, "none of that changed anything");
        // After a restart the Keep still knows who sent it.
        let mut again = engine_in(&dir);
        rev_status(update(&mut again, 2, 1, &[2, 3]), "updated", 1);
        // A newer update moves it on: the old one is stale again.
        rev_status(update(&mut again, 3, 2, &[2, 3]), "updated", 2);
        refused_rev(update(&mut again, 2, 1, &[2, 3]), "stale", 2);
        rev_status(update(&mut again, 3, 2, &[2, 3]), "updated", 2);
        // The signer of the current revision having removed itself may still be told so.
        rev_status(update(&mut again, 2, 3, &[3]), "updated", 3);
        rev_status(update(&mut again, 2, 3, &[3]), "updated", 3);
        assert!(!again.is_active(&keys(2).public_key()));
    }

    #[test]
    fn a_replayed_update_found_at_start_is_dropped_silently_and_a_live_stale_one_is_refused() {
        let (owner, _, network, urls, now) = claimed_owner("replays");
        let t = now - 1000;
        let hub = network.hub(&urls[0]);
        // Rev 1 and rev 2 from 2, in the Keep's state already (applied live).
        for (rev, who) in [("1", &[2u8, 3][..]), ("2", &[2u8][..])] {
            assert!(owner.process(&wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_UPDATE, update_tags(rev, who), now).unwrap(), now).is_some());
        }
        // The relay holds both, and holds rev 2 on a second relay too: the read at start finds them.
        assert!(hub.publish(&wrap_at(&keys(2), KIND_UPDATE, update_tags("1", &[2, 3]), t - 10)));
        assert!(hub.publish(&wrap_at(&keys(2), KIND_UPDATE, update_tags("2", &[2]), t)));
        let relay = network.connect(&urls);
        let (_, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert_eq!(found.len(), 2);
        let before = acks_to(hub.published(), 2).len();
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        // The old rev 1 got nothing; rev 2, the one that set the current revision, was told `updated` again.
        let acks = acks_to(hub.published(), 2);
        assert_eq!(acks.len() - before, 1, "{acks:?}");
        assert_eq!(acks.last().unwrap().0, "updated");
        assert_eq!(owner.lock().data.rev, 2);
        // The same old update arriving live is still refused.
        let live = owner.process(&wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_UPDATE, update_tags("1", &[2, 3]), now).unwrap(), now);
        let (_, rumor) = wire::unwrap(&keys(2), &live.unwrap()).unwrap();
        assert!(rumor.tags.iter().any(|t| t.as_slice() == ["reason", "stale"]));
    }

    #[test]
    fn one_wrap_that_two_relays_hold_is_applied_once() {
        let (owner, _, network, urls, now) = claimed_owner("two-relays");
        let hub = network.hub(&urls[0]);
        let wrap = wrap_at(&keys(2), KIND_UPDATE, update_tags("1", &[2, 3]), now - 1000);
        let relay = network.connect(&urls);
        // Two reads of the same relay stand for two relays: each with its own memory of what it saw.
        assert!(hub.publish(&wrap));
        let mut found = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false).1;
        found.extend(owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false).1);
        assert_eq!(found.len(), 2);
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert_eq!(acks_to(hub.published(), 2).iter().filter(|a| a.0 == "updated").count(), 1);
    }

    #[test]
    fn a_malformed_update_is_refused_with_the_current_revision() {
        let (mut engine, dir) = two_owners("update-malformed");
        let before = std::fs::read(dir.join(OWNER_FILE)).unwrap();
        let sixteen_and_one: Vec<Tag> = std::iter::once(keep_tag()).chain(std::iter::once(tag(&["rev", "2"]))).chain((0..17).map(|_| tag(&["persona", &Keys::generate().public_key().to_hex()]))).collect();
        let cases: Vec<(Vec<Tag>, &str)> = vec![
            (vec![keep_tag(), tag(&["persona", &hex_of(2)])], "malformed"),
            (vec![keep_tag(), tag(&["rev", ""]), tag(&["persona", &hex_of(2)])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "+2"]), tag(&["persona", &hex_of(2)])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "0x2"]), tag(&["persona", &hex_of(2)])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "18446744073709551616"]), tag(&["persona", &hex_of(2)])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "2"]), tag(&["rev", "3"]), tag(&["persona", &hex_of(2)])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "2"])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "2"]), tag(&["persona", "nothex"])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "2"]), tag(&["persona", &hex_of(2)]), tag(&["persona", &hex_of(2).to_uppercase()])], "malformed"),
            (vec![keep_tag(), tag(&["rev", "2"]), tag(&["persona"])], "malformed"),
            (sixteen_and_one, "too_many"),
        ];
        for (tags, reason) in cases {
            refused_rev(send(&mut engine, &keys(2), KIND_UPDATE, tags.clone(), NOW), reason, 1);
        }
        assert_eq!(std::fs::read(dir.join(OWNER_FILE)).unwrap(), before);
        assert_eq!(engine.data.rev, 1);
    }

    #[test]
    fn sixteen_personas_fit_and_a_seventeenth_does_not() {
        let (mut engine, _) = claimed("sixteen");
        let others: Vec<Keys> = (0..15).map(|_| Keys::generate()).collect();
        let mut tags = vec![keep_tag(), tag(&["rev", "1"]), tag(&["persona", &hex_of(2), "games"])];
        tags.extend(others.iter().map(|k| tag(&["persona", &k.public_key().to_hex()])));
        rev_status(send(&mut engine, &keys(2), KIND_UPDATE, tags.clone(), NOW), "updated", 1);
        assert_eq!(engine.data.owners.len(), 16);
        tags[1] = tag(&["rev", "2"]);
        tags.push(tag(&["persona", &Keys::generate().public_key().to_hex()]));
        refused_rev(send(&mut engine, &keys(2), KIND_UPDATE, tags, NOW), "too_many", 1);
        assert_eq!(engine.data.owners.len(), 16);
    }

    #[test]
    fn the_signer_may_remove_itself_only_if_another_persona_is_active_afterwards() {
        let (mut engine, _) = claimed("remove-signer");
        // 3 is listed but has not accepted: dropping the signer would leave nobody able to act.
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        refused_rev(update(&mut engine, 2, 2, &[3]), "malformed", 1);
        assert!(engine.is_active(&keys(2).public_key()) && engine.data.rev == 1, "no change");
        // A list of only new personas, none with an early accept, is the same.
        refused_rev(update(&mut engine, 2, 2, &[4, 5]), "malformed", 1);
        // Once 3 has accepted, 2 may go.
        rev_status(accept(&mut engine, 3, 1), "updated", 1);
        rev_status(update(&mut engine, 2, 2, &[3]), "updated", 2);
        assert_eq!(engine.data.owners.len(), 1);
        assert!(engine.is_active(&keys(3).public_key()) && !engine.is_listed(&keys(2).public_key()));
        // 2 has no rights left: no invites, no revokes, no updates.
        assert!(invite_from(&mut engine, 2, GRANT, 1).is_none());
        assert!(revoke_from(&mut engine, 2, GRANT).is_none());
        assert!(update(&mut engine, 2, 3, &[2]).is_none());
        assert_eq!(invite_from(&mut engine, 3, GRANT, 1).unwrap()["status"], "active");
        // 3 can never empty the list.
        refused_rev(update(&mut engine, 3, 3, &[4]), "malformed", 2);
    }

    #[test]
    fn an_accept_that_comes_before_its_update_is_held_and_used_by_that_update() {
        let (mut engine, dir) = claimed("accept-first");
        assert!(accept(&mut engine, 3, 1).is_none(), "an accept from a persona nobody listed gets no ack");
        assert_eq!(engine.pending.len(), 1);
        assert!(!engine.is_listed(&keys(3).public_key()), "an accept never adds a persona by itself");
        assert!(invite_from(&mut engine, 3, GRANT, 1).is_none());
        // Held in memory only: not written, not there after a restart.
        assert!(file_json(&dir).get("pending_accepts").is_none());
        assert!(engine_in(&dir).pending.is_empty());
        // The update lists 3: it is active at once and the held accept is used up.
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        assert!(engine.is_active(&keys(3).public_key()));
        assert!(engine.pending.is_empty());
        assert_eq!(invite_from(&mut engine, 3, GRANT, 1).unwrap()["status"], "active");
        assert_eq!(file_json(&dir)["owners"][1]["accepted_rev"], 1);
    }

    #[test]
    fn a_held_accept_older_than_the_listing_does_not_count() {
        let (mut engine, _) = claimed("accept-old");
        assert!(accept(&mut engine, 3, 1).is_none());
        rev_status(update(&mut engine, 2, 2, &[2, 3]), "updated", 2);
        assert!(engine.is_listed(&keys(3).public_key()) && !engine.is_active(&keys(3).public_key()), "rev 1 accept, listed at rev 2");
        assert!(engine.pending.is_empty(), "it could never count, so it is dropped");
        rev_status(accept(&mut engine, 3, 2), "updated", 2);
        assert!(engine.is_active(&keys(3).public_key()));
        // A newer held accept wins over an older one for the same persona.
        assert!(accept(&mut engine, 4, 5).is_none());
        assert!(accept(&mut engine, 4, 3).is_none());
        assert_eq!(engine.pending[0].rev, 5);
        rev_status(update(&mut engine, 2, 4, &[2, 3, 4]), "updated", 4);
        assert!(engine.is_active(&keys(4).public_key()), "5 >= 4");
    }

    #[test]
    fn an_accept_of_an_older_listing_from_a_listed_persona_is_held_not_taken() {
        let (mut engine, _) = claimed("accept-listed-old");
        rev_status(update(&mut engine, 2, 3, &[2, 3]), "updated", 3);
        rev_status(accept(&mut engine, 3, 2), "pending", 2);
        assert!(!engine.is_active(&keys(3).public_key()));
        rev_status(accept(&mut engine, 3, 3), "updated", 3);
        assert!(engine.is_active(&keys(3).public_key()));
    }

    #[test]
    fn a_persona_added_again_after_a_removal_needs_a_new_accept() {
        let (mut engine, _) = two_owners("readd");
        assert_eq!(invite_from(&mut engine, 3, GRANT, 1).unwrap()["status"], "active");
        rev_status(update(&mut engine, 2, 2, &[2]), "updated", 2);
        assert!(invite_from(&mut engine, 3, "22222222222222222222222222222222", 2).is_none(), "removed: dropped, no ack");
        // Listed again at rev 3: its old accept (rev 1) is not an acceptance of this listing.
        rev_status(update(&mut engine, 2, 3, &[2, 3]), "updated", 3);
        assert!(engine.is_listed(&keys(3).public_key()) && !engine.is_active(&keys(3).public_key()));
        assert!(invite_from(&mut engine, 3, "22222222222222222222222222222222", 2).is_none());
        // A replayed old accept does not help: it is held, not taken.
        rev_status(accept(&mut engine, 3, 1), "pending", 1);
        rev_status(accept(&mut engine, 3, 2), "pending", 2);
        assert!(!engine.is_active(&keys(3).public_key()));
        rev_status(accept(&mut engine, 3, 3), "updated", 3);
        assert_eq!(invite_from(&mut engine, 3, "22222222222222222222222222222222", 2).unwrap()["status"], "active");
    }

    #[test]
    fn a_removed_personas_invites_stay_and_its_players_are_not_touched() {
        let (mut engine, _) = two_owners("removed-invites");
        assert_eq!(invite_from(&mut engine, 3, GRANT, 1).unwrap()["status"], "active");
        rev_status(update(&mut engine, 2, 2, &[2]), "updated", 2);
        assert_eq!(live_secrets(&engine).len(), 1, "the invite 3 made belongs to the Keep");
        assert!(engine.invites.take_revoked().is_empty());
        // 2 can still revoke it.
        assert_eq!(revoke_from(&mut engine, 2, GRANT).unwrap()["status"], "revoked");
    }

    #[test]
    fn the_re_ack_of_a_claim_is_for_active_personas_only() {
        let (mut engine, _) = two_owners("reclaim-active");
        assert_eq!(claim(&mut engine, &keys(3), &[0; 16]).unwrap()["status"], "paired", "an active persona other than the claimer");
        rev_status(update(&mut engine, 2, 2, &[3]), "updated", 2);
        assert!(claim(&mut engine, &keys(2), &[0; 16]).is_none(), "removed");
        assert!(claim(&mut engine, &keys(4), &CODE).is_none());
    }

    #[test]
    fn the_sections_of_a_persona_are_kept_unknown_ones_too_and_changing_them_keeps_it_active() {
        let (mut engine, dir) = two_owners("sections");
        let tags = vec![keep_tag(), tag(&["rev", "2"]), tag(&["persona", &hex_of(2), "games", "backups", "games", "x-future"]), tag(&["persona", &hex_of(3)])];
        rev_status(send(&mut engine, &keys(2), KIND_UPDATE, tags, NOW), "updated", 2);
        let file = file_json(&dir);
        assert_eq!(file["owners"][0]["sections"], serde_json::json!(["games", "backups", "x-future"]));
        assert_eq!(file["owners"][1]["sections"], serde_json::json!([]));
        assert!(engine.is_active(&keys(2).public_key()) && engine.is_active(&keys(3).public_key()));
        assert_eq!(file["owners"][0]["added_rev"], 0, "the claimer still counts from the claim");
    }

    #[test]
    fn at_most_32_early_accepts_are_held_in_memory_the_oldest_makes_room_and_each_lasts_a_week() {
        let (mut engine, dir) = claimed("pending-cap");
        let before = std::fs::read_to_string(dir.join(OWNER_FILE)).unwrap();
        let senders: Vec<Keys> = (0..34).map(|_| Keys::generate()).collect();
        let accept_from = |engine: &mut Engine, who: &Keys, rev: u64, now: u64| send(engine, who, KIND_ACCEPT, vec![keep_tag(), tag(&["rev", &rev.to_string()])], now);
        let held = |engine: &Engine, who: &Keys| engine.pending.iter().any(|p| p.persona == who.public_key().to_hex());
        for (i, who) in senders[..32].iter().enumerate() {
            assert!(accept_from(&mut engine, who, 1, NOW + i as u64).is_none(), "no ack for a persona nobody listed");
        }
        // The 33rd is held and the oldest (the first) makes room.
        assert!(accept_from(&mut engine, &senders[32], 1, NOW + 40).is_none());
        assert_eq!(engine.pending.len(), 32);
        assert!(!held(&engine, &senders[0]) && held(&engine, &senders[1]) && held(&engine, &senders[32]));
        // One already held may send a newer one: it is then the newest, and the next newcomer pushes out the second sender.
        assert!(accept_from(&mut engine, &senders[1], 7, NOW + 50).is_none());
        assert!(accept_from(&mut engine, &senders[33], 1, NOW + 60).is_none());
        assert!(held(&engine, &senders[1]) && !held(&engine, &senders[2]) && held(&engine, &senders[33]));
        assert_eq!(engine.pending.len(), 32);
        assert_eq!(std::fs::read_to_string(dir.join(OWNER_FILE)).unwrap(), before, "nothing was written");
        assert!(engine_in(&dir).pending.is_empty(), "and nothing survives a restart");
        // A week on, they are gone: the ones received by NOW + 60 have all run out.
        let later = NOW + 60 + PENDING_TTL_SECS;
        assert!(accept_from(&mut engine, &senders[0], 1, later).is_none());
        assert_eq!(engine.pending.len(), 1);
    }

    #[test]
    fn a_revision_above_2_to_the_53_is_malformed_and_2_to_the_53_is_not() {
        let (mut engine, _) = claimed("rev-cap");
        let big = (MAX_REV + 1).to_string();
        let sent = |engine: &mut Engine, from: u8, kind: u16, rev: &str| -> Option<Reply> {
            let mut tags = vec![keep_tag(), tag(&["rev", rev])];
            if kind == KIND_UPDATE {
                tags.push(tag(&["persona", &hex_of(2)]));
            }
            send(engine, &keys(from), kind, tags, NOW)
        };
        refused_rev(sent(&mut engine, 2, KIND_UPDATE, &big), "malformed", 0);
        refused_rev(sent(&mut engine, 2, KIND_UPDATE, "99999999999999999999"), "malformed", 0);
        // An accept with such a number is refused too, to a listed persona; to anyone else it gets no answer.
        refused_rev(sent(&mut engine, 2, KIND_ACCEPT, &big), "malformed", 0);
        assert!(sent(&mut engine, 3, KIND_ACCEPT, &big).is_none());
        assert!(engine.pending.is_empty());
        rev_status(sent(&mut engine, 2, KIND_UPDATE, &MAX_REV.to_string()), "updated", MAX_REV);
        refused_rev(sent(&mut engine, 2, KIND_UPDATE, &big), "malformed", MAX_REV);
    }

    #[test]
    fn an_early_accept_just_inside_its_week_is_still_used() {
        let (mut engine, _) = claimed("pending-week");
        assert!(accept(&mut engine, 3, 1).is_none());
        let later = NOW + PENDING_TTL_SECS - 1;
        let reply = send(&mut engine, &keys(2), KIND_UPDATE, update_tags("1", &[2, 3]), later).unwrap();
        assert_eq!(reply["status"], "updated");
        assert!(engine.is_active(&keys(3).public_key()));
        // Past the week it would not have been.
        let (mut engine, _) = claimed("pending-week-late");
        assert!(accept(&mut engine, 3, 1).is_none());
        let reply = send(&mut engine, &keys(2), KIND_UPDATE, update_tags("1", &[2, 3]), NOW + PENDING_TTL_SECS).unwrap();
        assert_eq!(reply["status"], "updated");
        assert!(!engine.is_active(&keys(3).public_key()));
    }

    #[test]
    fn the_owner_personas_survive_a_restart_with_their_state() {
        let (mut engine, dir) = two_owners("owners-restart");
        assert!(accept(&mut engine, 4, 9).is_none());
        let mut again = engine_in(&dir);
        assert_eq!(again.data, engine.data);
        assert!(again.pending.is_empty(), "an early accept is not kept across a restart");
        // A file the earlier build wrote, with held accepts in it, is read and they are ignored.
        let mut legacy = file_json(&dir);
        legacy["pending_accepts"] = serde_json::json!([{"persona": hex_of(4), "rev": 9, "received_at": NOW}]);
        std::fs::write(dir.join(OWNER_FILE), legacy.to_string()).unwrap();
        let legacy_engine = engine_in(&dir);
        assert!(legacy_engine.pending.is_empty() && legacy_engine.data == engine.data);
        assert!(again.is_active(&keys(3).public_key()));
        assert_eq!(invite_from(&mut again, 3, GRANT, 1).unwrap()["status"], "active");
        refused_rev(update(&mut again, 2, 1, &[2]), "stale", 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join(OWNER_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn an_update_the_disk_cannot_save_holds_for_this_run_and_is_acknowledged_when_its_resend_is_saved() {
        let (mut engine, dir) = two_owners("update-unwritable");
        assert_eq!(invite_from(&mut engine, 3, GRANT, 1).unwrap()["status"], "active");
        // The temp file's place is taken by a directory: no write can succeed.
        let tmp = dir.join(format!(".{OWNER_FILE}.tmp"));
        std::fs::create_dir(&tmp).unwrap();
        // Removing 3 holds at once, but nothing is acknowledged.
        assert!(update(&mut engine, 2, 2, &[2]).is_none());
        assert!(!engine.is_listed(&keys(3).public_key()));
        assert!(invite_from(&mut engine, 3, "22222222222222222222222222222222", 2).is_none(), "3 has no rights");
        assert_eq!(file_json(&dir)["rev"], 1, "the disk still has the old list");
        // Still full: the resend is not acknowledged either, and 3 is still out.
        assert!(update(&mut engine, 2, 2, &[2]).is_none());
        // Another owner message while the disk is down is read with the new list.
        assert!(update(&mut engine, 3, 3, &[3]).is_none());
        // The disk comes back: the resend is `updated` (not stale) and the file has it.
        std::fs::remove_dir(&tmp).unwrap();
        rev_status(update(&mut engine, 2, 2, &[2]), "updated", 2);
        assert_eq!(file_json(&dir)["rev"], 2);
        // A repeat of it is `updated` again; a different list at that revision is stale.
        rev_status(update(&mut engine, 2, 2, &[2]), "updated", 2);
        refused_rev(update(&mut engine, 2, 2, &[2, 3]), "stale", 2);
        assert_eq!(engine_in(&dir).data.owners.len(), 1);
    }

    #[test]
    fn an_owner_file_that_cannot_be_written_leaves_a_claim_and_an_accept_unacknowledged_and_unapplied() {
        let (mut engine, dir) = claimed("accept-unwritable");
        rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
        let tmp = dir.join(format!(".{OWNER_FILE}.tmp"));
        std::fs::create_dir(&tmp).unwrap();
        assert!(accept(&mut engine, 3, 1).is_none());
        assert!(!engine.is_active(&keys(3).public_key()), "adding authority waits for the disk");
        // A stranger's accept needs no disk: held, not answered.
        assert!(accept(&mut engine, 5, 1).is_none());
        assert_eq!(engine.pending.len(), 1);
        std::fs::remove_dir(&tmp).unwrap();
        rev_status(accept(&mut engine, 3, 1), "updated", 1);
        assert!(engine.is_active(&keys(3).public_key()));
    }

    // The wrap cursor moves for what the contract says, and only for that.
    #[test]
    fn owner_updates_and_accepts_from_listed_personas_move_the_cursor_and_strangers_do_not() {
        let (mut engine, _) = claimed("cursor-k1e");
        let before = engine.cursor;
        // Each wrap is a week after the last, so its jitter (two days) cannot blur which one moved the cursor.
        let mut clock = NOW;
        let mut moved = |engine: &mut Engine, from: &Keys, kind: u16, tags: Vec<Tag>| -> bool {
            clock += 7 * DAY;
            let wrap = wire::gift_wrap(from, &keys(1).public_key(), kind, tags, clock).unwrap();
            let was = engine.cursor;
            engine.process(&wrap, clock);
            engine.cursor != was
        };
        let accept_tags = || vec![keep_tag(), tag(&["rev", "1"])];
        // A stranger's accept (held for an update to come), twice: the cursor stays.
        let stranger = Keys::generate();
        assert!(!moved(&mut engine, &stranger, KIND_ACCEPT, accept_tags()));
        assert!(!moved(&mut engine, &stranger, KIND_ACCEPT, accept_tags()), "not even a second time");
        assert_eq!(engine.cursor, before);
        // A stranger's update: dropped, the cursor stays.
        assert!(!moved(&mut engine, &stranger, KIND_UPDATE, update_tags("1", &[2])));
        // The owner's update moves it, and so does a repeat of it (told `updated` again).
        assert!(moved(&mut engine, &keys(2), KIND_UPDATE, update_tags("1", &[2, 3])));
        assert!(moved(&mut engine, &keys(2), KIND_UPDATE, update_tags("1", &[2, 3])));
        // A listed persona's accept moves it.
        assert!(moved(&mut engine, &keys(3), KIND_ACCEPT, accept_tags()));
        // An update from a listed persona that has not accepted does not.
        let (mut other, _) = claimed("cursor-k1e-2");
        rev_status(update(&mut other, 2, 1, &[2, 3]), "updated", 1);
        assert!(!moved(&mut other, &keys(3), KIND_INVITE, invite_tags(GRANT, 1, NOW + DAY, &["guest"])));
        assert!(!moved(&mut other, &keys(3), KIND_UPDATE, update_tags("2", &[3])));
        // A removed persona's wraps no longer move it.
        rev_status(accept(&mut other, 3, 1), "updated", 1);
        rev_status(update(&mut other, 2, 2, &[2]), "updated", 2);
        assert!(!moved(&mut other, &keys(3), KIND_ACCEPT, accept_tags()));
        assert!(moved(&mut other, &keys(2), KIND_UPDATE, update_tags("3", &[2])));
    }

    #[test]
    fn the_loop_takes_an_update_and_an_accept_over_relays_and_acks_each_with_its_revision() {
        let dir = scratch("loop-k1e");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let urls = vec!["wss://one.example".to_string(), "wss://two.example".to_string()];
        let owner = Arc::new(Owner::open(&dir, keys(1), invites.clone(), "Home", &urls).unwrap());
        let code = {
            let link = owner.pairing_link().unwrap();
            link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
        };
        let network = FakeNetwork::new();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let relays: Vec<Arc<dyn Relay>> = urls.iter().map(|u| network.connect(std::slice::from_ref(u))).collect();
            let (owner, stop) = (owner.clone(), stop.clone());
            std::thread::spawn(move || owner.run(relays, || stop.load(std::sync::atomic::Ordering::Relaxed)))
        };
        let mine = network.connect(&urls);
        let mut acks_2 = mine.subscribe(vec![Filter::new().kind(Kind::GiftWrap).pubkey(keys(2).public_key())]);
        let mut acks_3 = mine.subscribe(vec![Filter::new().kind(Kind::GiftWrap).pubkey(keys(3).public_key())]);
        let mut seen_acks = HashSet::new();
        let mut next_ack = |sub: &mut Subscription, me: u8, expect: &str| -> Reply {
            let mut found = None;
            assert!(wait_for(|| {
                while let Some(message) = sub.try_recv() {
                    if let RelayMsg::Event(wrap) = message {
                        if !seen_acks.insert(wrap.id) {
                            continue;
                        }
                        let (_, rumor) = wire::unwrap(&keys(me), &wrap).unwrap();
                        found = Some(rumor.tags.iter().map(|t| (t.as_slice()[0].clone(), t.as_slice().get(1).cloned().unwrap_or_default())).collect::<Reply>());
                        return true;
                    }
                }
                false
            }), "no ack for {expect}");
            let reply = found.unwrap();
            assert_eq!(reply["status"], expect);
            reply
        };
        let now = unix_now();
        let publish = |from: u8, kind: u16, tags: Vec<Tag>| mine.publish(&wire::gift_wrap(&keys(from), &keys(1).public_key(), kind, tags, now).unwrap());
        publish(2, KIND_CLAIM, vec![keep_tag(), tag(&["code", &code]), tag(&["scope", "all"])]);
        assert!(next_ack(&mut acks_2, 2, "paired").contains_key("claim"));
        publish(2, KIND_UPDATE, update_tags("1", &[2, 3]));
        assert_eq!(next_ack(&mut acks_2, 2, "updated")["rev"], "1");
        publish(3, KIND_ACCEPT, vec![keep_tag(), tag(&["rev", "1"])]);
        assert_eq!(next_ack(&mut acks_3, 3, "updated")["rev"], "1");
        publish(3, KIND_INVITE, invite_tags(GRANT, 1, now + DAY, &["guest"]));
        assert_eq!(next_ack(&mut acks_3, 3, "active")["grant"], GRANT);
        publish(2, KIND_UPDATE, update_tags("1", &[2]));
        assert_eq!(next_ack(&mut acks_2, 2, "refused")["reason"], "stale");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
        assert!(owner.lock().is_active(&keys(3).public_key()));
        assert_eq!(owner.lock().data.scope, "all");
    }

    // ─── what is checked on the way in ──────────────────────────────────────

    #[test]
    fn the_seal_must_be_the_senders_and_the_wrap_must_be_ours() {
        let (mut engine, _) = claimed("checks");
        // A rumor claiming to be the owner's, sealed by someone else.
        let rumor_by_owner = nostr::EventBuilder::new(Kind::Custom(KIND_INVITE), "")
            .tags(invite_tags(GRANT, 1, NOW + DAY, &["guest"]))
            .build(keys(2).public_key());
        let intruder = keys(3);
        let sealed = nostr::nips::nip44::encrypt(intruder.secret_key(), &keys(1).public_key(), nostr::JsonUtil::as_json(&rumor_by_owner), nostr::nips::nip44::Version::V2).unwrap();
        let seal = nostr::EventBuilder::new(Kind::Seal, sealed).sign_with_keys(&intruder).unwrap();
        let throwaway = Keys::generate();
        let wrapped = nostr::nips::nip44::encrypt(throwaway.secret_key(), &keys(1).public_key(), nostr::JsonUtil::as_json(&seal), nostr::nips::nip44::Version::V2).unwrap();
        let wrap = nostr::EventBuilder::new(Kind::GiftWrap, wrapped).tag(Tag::public_key(keys(1).public_key())).sign_with_keys(&throwaway).unwrap();
        assert!(engine.process(&wrap, NOW).is_none());
        assert!(live_secrets(&engine).is_empty());
        // A wrap for someone else never reaches the rules.
        let elsewhere = wire::gift_wrap(&keys(2), &keys(9).public_key(), KIND_INVITE, invite_tags(GRANT, 1, NOW + DAY, &["guest"]), NOW).unwrap();
        assert!(engine.process(&elsewhere, NOW).is_none());
        // An ack, or anything else, sent to the Keep is not a command.
        assert!(send(&mut engine, &keys(2), KIND_ACK, vec![keep_tag(), tag(&["status", "active"])], NOW).is_none());
        assert!(send(&mut engine, &keys(2), 1, vec![keep_tag()], NOW).is_none());
    }

    #[test]
    fn the_cursor_moves_with_wraps_never_past_now_and_survives_a_restart() {
        let (mut engine, dir) = engine("cursor");
        assert_eq!(engine.since(NOW), NOW - 3 * DAY, "a first run reads three days back");
        engine.note_wrap(NOW - 500, NOW);
        assert_eq!(engine.since(NOW), NOW - 500 - CURSOR_MARGIN_SECS);
        engine.note_wrap(NOW - 900, NOW);
        assert_eq!(engine.since(NOW), NOW - 500 - CURSOR_MARGIN_SECS, "an older wrap does not move it back");
        // A wrap dated in the future cannot hold real messages out.
        engine.note_wrap(NOW + 10 * DAY, NOW);
        assert_eq!(engine.cursor, Some(NOW));
        engine.flush_cursor();
        assert_eq!(engine_in(&dir).since(NOW + 100), NOW - CURSOR_MARGIN_SECS);
        // The file is private.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join(STATE_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_wrap_the_keep_reads_moves_the_cursor() {
        let (mut engine, _) = claimed("cursor-wrap");
        let wrap = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], NOW).unwrap();
        engine.process(&wrap, NOW).unwrap();
        let at = engine.cursor.unwrap();
        assert!((wrap.created_at.as_secs()..=NOW).contains(&at), "{at}");
    }

    #[test]
    fn a_wrap_that_fails_the_cheap_checks_or_the_signature_moves_nothing() {
        let (mut engine, _) = claimed("cursor-junk");
        let good = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], NOW).unwrap();
        // Changed after it was signed: the signature fails.
        let mut value: serde_json::Value = serde_json::from_str(&nostr::JsonUtil::as_json(&good)).unwrap();
        value["content"] = serde_json::json!("AAAA");
        let forged: Event = nostr::JsonUtil::from_json(value.to_string()).unwrap();
        // Not addressed to this Keep.
        let elsewhere = wire::gift_wrap(&keys(2), &keys(5).public_key(), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], NOW).unwrap();
        // Larger than any owner message.
        let big = nostr::EventBuilder::new(Kind::GiftWrap, "x".repeat(wire::MAX_WRAP_CONTENT + 1))
            .tag(Tag::public_key(keys(1).public_key()))
            .custom_created_at(Timestamp::from_secs(NOW))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        engine.cursor = Some(NOW - 5 * DAY);
        for wrap in [&forged, &elsewhere, &big] {
            assert!(engine.process(wrap, NOW).is_none());
            assert_eq!(engine.cursor, Some(NOW - 5 * DAY), "junk does not move the cursor");
        }
        assert!(engine.process(&good, NOW).is_some());
        assert!(engine.cursor > Some(NOW - 5 * DAY));
    }

    #[test]
    fn only_the_owners_wraps_and_a_claim_that_worked_move_the_cursor() {
        let (mut engine, _) = engine("cursor-owner-only");
        assert_eq!(engine.cursor, None);
        let future = NOW + 30 * DAY;
        // A wrap from anyone, dated far ahead: opens fine, is no owner's message.
        let from_a_stranger = wire::gift_wrap(&keys(7), &keys(1).public_key(), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], future).unwrap();
        // Junk from a throwaway key, addressed to the Keep, dated ahead, that will not open.
        let junk = junk(1, |_| future).remove(0);
        // A claim with the wrong code.
        let wrong_claim = wire::gift_wrap(&keys(7), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &hex_encode(&[0x5b; 16])])], future).unwrap();
        for wrap in [&from_a_stranger, &junk, &wrong_claim] {
            assert!(engine.process(wrap, NOW).is_none());
            assert_eq!(engine.cursor, None, "future-dated junk leaves the cursor alone");
        }
        assert_eq!(engine.since(NOW), NOW - 3 * DAY);
        // The claim that works moves it, and never past the Keep's own clock.
        let claim = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &hex_encode(&CODE)])], NOW).unwrap();
        assert!(engine.process(&claim, NOW).is_some());
        let after_claim = engine.cursor.expect("a claim moves the cursor");
        assert!(after_claim <= NOW);
        // The owner's message dated ahead is capped at now.
        let ahead = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], future).unwrap();
        assert!(engine.process(&ahead, NOW).is_some());
        assert_eq!(engine.cursor, Some(NOW));
        // And junk after that still changes nothing.
        assert!(engine.process(&from_a_stranger, NOW).is_none());
        assert_eq!(engine.cursor, Some(NOW));
    }

    #[test]
    fn damaged_owner_files_are_errors_not_an_unclaimed_keep() {
        for (file, text) in [(OWNER_FILE, "not json"), (OWNER_FILE, "{\"version\":2,\"owners\":[],\"claimed_at\":0}"), (OWNER_FILE, "{\"version\":1,\"owners\":[\"zz\"],\"claimed_at\":0}"), (STATE_FILE, "{")] {
            let dir = scratch("damaged");
            let invites = Arc::new(Invites::open(&dir, [1; 32]).unwrap());
            std::fs::write(dir.join(file), text).unwrap();
            let error = Engine::open(&dir, keys(1), invites, CODE).err().expect(text);
            assert!(error.starts_with(&format!("{file} couldn't be read")), "{error}");
            assert!(!error.contains(dir.to_str().unwrap()), "{error}");
            assert_eq!(std::fs::read_to_string(dir.join(file)).unwrap(), text, "left alone");
        }
    }

    // ─── the link and the loop ──────────────────────────────────────────────

    #[test]
    fn the_pairing_link_is_percent_encoded_with_at_most_three_wss_relays() {
        let relays: Vec<String> = ["wss://a.example", "wss://b.example:8443/path?x=1", "wss://c.example", "wss://d.example"].map(String::from).to_vec();
        let link = pairing_link(&"ab".repeat(32), &CODE, "Home Keep & more", &relays);
        assert_eq!(
            link,
            format!("signet-keep:pair?k={}&c={}&n=Home%20Keep%20%26%20more&r=wss%3A%2F%2Fa.example&r=wss%3A%2F%2Fb.example%3A8443%2Fpath%3Fx%3D1&r=wss%3A%2F%2Fc.example", "ab".repeat(32), "5a".repeat(16))
        );
    }

    #[test]
    fn an_owner_shows_a_link_only_until_it_is_claimed() {
        let dir = scratch("link");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let relays = vec!["wss://relay.example".to_string(), "ws://insecure.example".to_string()];
        let owner = Owner::open(&dir, keys(1), invites.clone(), "My Keep", &relays).unwrap();
        assert_eq!(owner.relays(), ["wss://relay.example"], "wss only");
        let link = owner.pairing_link().unwrap();
        assert!(link.starts_with(&format!("signet-keep:pair?k={}&c=", keys(1).public_key().to_hex())) && link.contains("&n=My%20Keep&r=wss%3A%2F%2Frelay.example"), "{link}");
        let code = link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string();
        assert_eq!(code.len(), 32);
        let wrap = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], NOW).unwrap();
        assert!(owner.process(&wrap, NOW).is_some());
        assert!(owner.claimed() && owner.pairing_link().is_none());
        assert!(Owner::open(&dir, keys(1), invites, "x", &["ws://x".to_string()]).is_err());
    }

    #[test]
    fn only_owner_files_that_cannot_be_read_or_an_existing_owner_shut_the_page() {
        let dir = scratch("open-errors");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        // A scheme in capitals is a wss relay all the same; the link has it in lower case.
        let upper = Owner::open(&dir, keys(1), invites.clone(), "x", &["WSS://Relay.example".to_string()]).unwrap();
        assert_eq!(upper.relays(), ["wss://Relay.example"]);
        // No usable relay on a Keep nobody has claimed: owner messages are off, the page is not.
        let none = Owner::open(&dir, keys(1), invites.clone(), "x", &["ws://x".to_string()]).err().unwrap();
        assert!(matches!(none, OpenError::Other(_)) && !none.fails_closed(&dir), "{none:?}");
        assert!(!OpenError::Other("no key".into()).fails_closed(&dir));
        // The same on a Keep that has an owner: the page stays shut.
        std::fs::write(dir.join(OWNER_FILE), "{\"version\":1,\"owners\":[],\"claimed_at\":0}").unwrap();
        assert!(none.fails_closed(&dir));
        // A damaged owner file is `Files`, which fails closed.
        std::fs::write(dir.join(OWNER_FILE), "not json").unwrap();
        let bad = Owner::open(&dir, keys(1), invites, "x", &["wss://relay.example".to_string()]).err().unwrap();
        assert!(matches!(&bad, OpenError::Files(m) if m.contains(OWNER_FILE)) && bad.fails_closed(&dir), "{bad:?}");
    }

    /// Wait up to ten seconds for `check` to hold.
    fn wait_for(mut check: impl FnMut() -> bool) -> bool {
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < end {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn the_loop_claims_invites_and_revokes_over_relays_and_replies_to_each() {
        let dir = scratch("loop");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let urls = vec!["wss://one.example".to_string(), "wss://two.example".to_string()];
        let owner = Arc::new(Owner::open(&dir, keys(1), invites.clone(), "Home", &urls).unwrap());
        let code = {
            let link = owner.pairing_link().unwrap();
            link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
        };
        let network = FakeNetwork::new();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            // One connection to each relay, as the Keep makes them.
            let relays: Vec<Arc<dyn Relay>> = urls.iter().map(|u| network.connect(std::slice::from_ref(u))).collect();
            let (owner, stop) = (owner.clone(), stop.clone());
            std::thread::spawn(move || owner.run(relays, || stop.load(std::sync::atomic::Ordering::Relaxed)))
        };
        // The owner side: its own connection, listening for acks.
        let mine = network.connect(&urls);
        let mut acks = mine.subscribe(vec![Filter::new().kind(Kind::GiftWrap).pubkey(keys(2).public_key())]);
        // Two relays carry each ack: read it once.
        let mut seen_acks = HashSet::new();
        let mut next_ack = |expect: &str| -> Reply {
            let mut found = None;
            assert!(wait_for(|| {
                while let Some(message) = acks.try_recv() {
                    if let RelayMsg::Event(wrap) = message {
                        if !seen_acks.insert(wrap.id) {
                            continue;
                        }
                        let (sender, rumor) = wire::unwrap(&keys(2), &wrap).unwrap();
                        assert_eq!(sender, keys(1).public_key());
                        found = Some(rumor.tags.iter().map(|t| (t.as_slice()[0].clone(), t.as_slice().get(1).cloned().unwrap_or_default())).collect::<Reply>());
                        return true;
                    }
                }
                false
            }), "no ack for {expect}");
            let reply = found.unwrap();
            assert_eq!(reply["status"], expect);
            reply
        };
        let now = unix_now();
        let publish = |kind: u16, tags: Vec<Tag>| mine.publish(&wire::gift_wrap(&keys(2), &keys(1).public_key(), kind, tags, now).unwrap());
        publish(KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])]);
        assert!(next_ack("paired").contains_key("claim"));
        publish(KIND_INVITE, invite_tags(GRANT, 1, now + DAY, &["guest"]));
        assert_eq!(next_ack("active")["grant"], GRANT);
        assert_eq!(invites.live(now).len(), 1);
        publish(KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])]);
        assert_eq!(next_ack("revoked")["grant"], GRANT);
        assert!(invites.live(now).is_empty());
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
        assert!(owner.claimed());
        // Every answer went to every relay of the link, not only to some of them.
        for url in &urls {
            let acks = network.hub(url).published().into_iter().filter(|e| e.kind == Kind::GiftWrap).count();
            assert_eq!(acks, 3 + 3, "{url}: the owner's three messages and the Keep's three answers");
        }
    }

    /// Wraps for the Keep from throwaway keys, all made at `at(i)`.
    fn junk(count: usize, at: impl Fn(usize) -> u64) -> Vec<Event> {
        (0..count)
            .map(|i| {
                nostr::EventBuilder::new(Kind::GiftWrap, format!("junk {i}"))
                    .tag(Tag::public_key(keys(1).public_key()))
                    .custom_created_at(Timestamp::from_secs(at(i)))
                    .sign_with_keys(&Keys::generate())
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn a_flood_of_newer_junk_cannot_hide_an_older_revoke_from_the_read_at_start() {
        // Junk at one instant (a relay answers ties in its own order) and junk spread out.
        for spread in [false, true] {
            let dir = scratch(&format!("flood-{spread}"));
            let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
            let urls = vec!["wss://one.example".to_string()];
            let owner = Owner::open(&dir, keys(1), invites.clone(), "Home", &urls).unwrap();
            let code = {
                let link = owner.pairing_link().unwrap();
                link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
            };
            let now = unix_now();
            let claim = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], now).unwrap();
            assert!(owner.process(&claim, now).is_some());
            // An invite and its revoke from half a day ago (the jitter keeps them inside the first read's three days).
            let then = now - DAY / 2;
            let network = FakeNetwork::new();
            let hub = network.hub(&urls[0]);
            let tags = invite_tags(GRANT, 1, now + DAY, &["guest"]);
            assert!(hub.publish(&wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_INVITE, tags, then).unwrap()));
            assert!(hub.publish(&wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], then).unwrap()));
            // Three pages and a bit of junk, all newer.
            let limit = 10;
            for wrap in junk(3 * limit + 1, |i| if spread { now - 10 - i as u64 } else { now - 10 }) {
                assert!(hub.publish(&wrap));
            }
            // What the relay would send on one plain request is only junk.
            let filter = Filter::new().kind(Kind::GiftWrap).pubkey(keys(1).public_key()).limit(limit);
            assert!(hub.query(&[filter]).iter().all(|e| e.created_at.as_secs() >= now - 10 - 3 * limit as u64));
            let relay = network.connect(&urls);
            let mut seen = Seen::default();
            let (reached, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, limit, STRANGER_CAP, &mut seen, &|| false);
            assert!(reached);
            assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
            assert!(invites.live(now).is_empty(), "the revoke was read ({spread})");
            assert_eq!(owner.lock().grants.iter().map(|g| g.state).collect::<Vec<_>>(), vec![GrantState::Revoked]);
        }
    }

    /// A claimed Keep (owner 2) with a hub, an owner and an engine to read it.
    fn claimed_owner(name: &str) -> (Owner, Arc<Invites>, FakeNetwork, Vec<String>, u64) {
        let dir = scratch(name);
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let urls = vec!["wss://one.example".to_string()];
        let owner = Owner::open(&dir, keys(1), invites.clone(), "Home", &urls).unwrap();
        let code = {
            let link = owner.pairing_link().unwrap();
            link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
        };
        let now = unix_now();
        let claim = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], now).unwrap();
        assert!(owner.process(&claim, now).is_some());
        (owner, invites, FakeNetwork::new(), urls, now)
    }

    /// A wrap like `wire::gift_wrap` makes, but with no jitter: the rumor and
    /// the wrap are both made at `at`, so a test knows which a relay hands over first.
    fn wrap_at(from: &Keys, kind: u16, tags: Vec<Tag>, at: u64) -> Event {
        use nostr::nips::nip44::{self, Version};
        use nostr::{EventBuilder, JsonUtil};
        let to = keys(1).public_key();
        let rumor: UnsignedEvent = EventBuilder::new(Kind::Custom(kind), "").tags(tags).custom_created_at(Timestamp::from_secs(at)).build(from.public_key());
        let sealed = nip44::encrypt(from.secret_key(), &to, rumor.as_json(), Version::V2).unwrap();
        let seal = EventBuilder::new(Kind::Seal, sealed).custom_created_at(Timestamp::from_secs(at)).sign_with_keys(from).unwrap();
        let throwaway = Keys::generate();
        let wrapped = nip44::encrypt(throwaway.secret_key(), &to, seal.as_json(), Version::V2).unwrap();
        EventBuilder::new(Kind::GiftWrap, wrapped).tag(Tag::public_key(to)).custom_created_at(Timestamp::from_secs(at)).sign_with_keys(&throwaway).unwrap()
    }

    /// A well-formed accept from a throwaway key, made at `at`: openable, and
    /// shaped like one the Keep acts on.
    fn stranger_accept(at: u64) -> Event {
        wrap_at(&Keys::generate(), KIND_ACCEPT, vec![keep_tag(), tag(&["rev", "1"])], at)
    }

    #[test]
    fn a_message_is_held_only_if_it_has_the_shape_of_one_and_only_with_the_tags_the_keep_reads() {
        let keep_hex = keys(1).public_key().to_hex();
        let from = keys(2).public_key();
        let id = EventId::all_zeros();
        let rumor = |content: &str, tags: Vec<Tag>| UnsignedEvent::new(from, Timestamp::from_secs(NOW), Kind::Custom(KIND_ACCEPT), tags, content);
        let hold = |rumor: &UnsignedEvent| Backlog::hold(id, NOW, from, rumor, &keep_hex);
        let good = vec![keep_tag(), tag(&["rev", "3"]), tag(&["note", "unknown tags are not kept"])];
        let held = hold(&rumor("", good.clone())).expect("a plain accept is held");
        assert_eq!((held.kind, held.created_at, held.tags.len()), (KIND_ACCEPT, NOW, 2));
        assert_eq!(held.tag("rev"), Some("3"));
        let rebuilt = held.rumor().unwrap();
        assert_eq!((rebuilt.pubkey, rebuilt.kind, rebuilt.content.as_str(), parse_rev(&rebuilt)), (from, Kind::Custom(KIND_ACCEPT), "", Some(3)));
        // Content, too many tags, a long part, too many parts, another Keep, another kind.
        assert!(hold(&rumor("hello", good.clone())).is_none());
        let many: Vec<Tag> = good.iter().cloned().chain((0..MAX_RUMOR_TAGS).map(|i| tag(&["x", &i.to_string()]))).collect();
        assert!(hold(&rumor("", many)).is_none());
        assert!(hold(&rumor("", vec![keep_tag(), tag(&["rev", &"9".repeat(MAX_TAG_PART_BYTES + 1)])])).is_none());
        assert!(hold(&rumor("", vec![keep_tag(), tag(&["persona", "a", "b", "c", "d", "e", "f"])])).is_none());
        // Only `persona` may have more than three parts, and its sections are short.
        assert!(hold(&rumor("", vec![keep_tag(), tag(&["rev", "1", "x", "y"])])).is_none());
        assert!(hold(&rumor("", vec![keep_tag(), tag(&["p", &hex_of(2), "wss://relay.example", "extra"])])).is_none());
        assert!(hold(&rumor("", vec![keep_tag(), tag(&["persona", &hex_of(2), "games", "backups", "x-future"])])).is_some());
        assert!(hold(&rumor("", vec![keep_tag(), tag(&["persona", &hex_of(2), &"s".repeat(MAX_SECTION_BYTES + 1)])])).is_none());
        assert!(hold(&rumor("", vec![keep_tag(), tag(&["p", &hex_of(2), "wss://relay.example"])])).is_some());
        assert!(hold(&rumor("", vec![tag(&["keep", &hex_of(9)]), tag(&["rev", "1"])])).is_none());
        let other = UnsignedEvent::new(from, Timestamp::from_secs(NOW), Kind::Custom(1), good, "");
        assert!(hold(&other).is_none());
        // The most an owners update says is within the bounds.
        let mut tags = vec![keep_tag(), tag(&["rev", "7"])];
        tags.extend((0..16).map(|_| tag(&["persona", &Keys::generate().public_key().to_hex(), "games"])));
        tags.push(tag(&["name", &"é".repeat(invites::MAX_LABEL)]));
        let update = UnsignedEvent::new(from, Timestamp::from_secs(NOW), Kind::Custom(KIND_UPDATE), tags, "");
        let held = hold(&update).unwrap();
        assert_eq!(held.tags.len(), 19);
        // The most any held record can weigh, counting each string's bytes and a 24-byte header:
        // 16 persona tags (key and four sections), one label, seven other tags of three parts.
        let persona_max = 24 + 2 * 24 + MAX_TAG_PART_BYTES + 4 * (24 + MAX_SECTION_BYTES);
        let name_max = 24 + 3 * (24 + MAX_NAME_BYTES);
        let other_max = 24 + 3 * (24 + MAX_TAG_PART_BYTES);
        let worst = MAX_PERSONA_TAGS * persona_max + name_max + (MAX_OTHER_TAGS - 1) * other_max;
        assert!(worst < 9_000, "{worst}");
        // Twelve thousand (3 relays x 4,000) of the very largest: about 100 MB, where it was about 250.
        assert!(3 * STRANGER_CAP * worst < 110_000_000);
        // More than 16 persona tags, or more than 8 others, are not a shape the contract has.
        let many_personas: Vec<Tag> = std::iter::once(keep_tag()).chain((0..17).map(|_| tag(&["persona", &hex_of(2), "games"]))).collect();
        assert!(hold(&rumor("", many_personas)).is_none());
        let many_others: Vec<Tag> = std::iter::once(keep_tag()).chain((0..8).map(|i| tag(&["grant", &i.to_string()]))).collect();
        assert!(hold(&rumor("", many_others)).is_none());
    }

    #[test]
    fn a_flood_of_tag_heavy_openable_wraps_is_not_held_and_cannot_hide_an_older_owner_revoke() {
        let (owner, invites, network, urls, now) = claimed_owner("flood-heavy");
        let hub = network.hub(&urls[0]);
        let then = now - DAY / 2;
        let tags = invite_tags(GRANT, 1, now + DAY, &["guest"]);
        assert!(hub.publish(&wrap_at(&keys(2), KIND_INVITE, tags, then)));
        assert!(hub.publish(&wrap_at(&keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], then)));
        // Far more than the cap, all newer than the revoke, from new keys: accepts padded
        // with tags, and accepts with a long tag value.
        let (limit, cap) = (10, 20);
        for i in 0..4 * cap {
            let mut padded = vec![keep_tag(), tag(&["rev", "1"])];
            if i % 2 == 0 {
                padded.extend((0..2 * MAX_RUMOR_TAGS).map(|n| tag(&["pad", &n.to_string()])));
            } else {
                padded.push(tag(&["rev", &"7".repeat(MAX_TAG_PART_BYTES + 1)]));
            }
            assert!(hub.publish(&wrap_at(&Keys::generate(), KIND_ACCEPT, padded, now - 10 - i as u64)));
        }
        let relay = network.connect(&urls);
        let mut seen = Seen::default();
        let (reached, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, limit, cap, &mut seen, &|| false);
        assert!(reached);
        assert_eq!(found.len(), 2, "none of the padded ones is held, so none counts against the cap");
        // The read went on to the end. Each padded one is dropped for good (no relay's
        // copy can be held), so it is marked seen; the two held are not, until applied.
        assert_eq!(seen.ids.len(), 4 * cap, "and the read went on to the end");
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert!(invites.live(now).is_empty(), "the older revoke was read and applied");
        assert_eq!(owner.lock().grants.iter().map(|g| g.state).collect::<Vec<_>>(), vec![GrantState::Revoked]);
    }

    #[test]
    fn well_formed_wraps_from_strangers_are_held_up_to_a_cap_and_the_relay_is_read_on() {
        let (owner, invites, network, urls, now) = claimed_owner("flood-cap");
        let hub = network.hub(&urls[0]);
        let (limit, cap) = (10, 20);
        // The owner's invite and revoke sit among the flood, newer than all but a few of it.
        let tags = invite_tags(GRANT, 1, now + DAY, &["guest"]);
        assert!(hub.publish(&wrap_at(&keys(2), KIND_INVITE, tags, now - 1000)));
        assert!(hub.publish(&wrap_at(&keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], now - 1000)));
        let flood = 20 * cap;
        for i in 0..flood {
            // Mostly older than the owner's two; the first few (newest) are not.
            let at = if i < 4 { now - 100 - i as u64 } else { now - 2000 - i as u64 };
            assert!(hub.publish(&stranger_accept(at)));
        }
        let relay = network.connect(&urls);
        let mut seen = Seen::default();
        let (reached, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, limit, cap, &mut seen, &|| false);
        assert!(reached);
        let others = found.iter().filter(|b| b.sender != keys(2).public_key()).count();
        assert_eq!(others, cap, "strangers are held up to the cap and no further");
        assert_eq!(found.len(), cap + 2, "the owner's invite and revoke come on top of it");
        assert!(hub.reqs().len() > (flood + 2) / limit, "the whole relay was read");
        assert!(seen.ids.is_empty(), "nothing held is marked seen before it is applied");
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert!(invites.live(now).is_empty());
        assert_eq!(owner.lock().grants.iter().map(|g| g.state).collect::<Vec<_>>(), vec![GrantState::Revoked]);
        // The held strangers are accepts nobody listed: none became an owner, and none was answered.
        assert_eq!(owner.lock().data.owners.len(), 1);
    }

    #[test]
    fn listed_personas_messages_are_held_outside_the_stranger_cap() {
        // A Keep with owners 2 and 3 listed and 3 active: 3's messages are not capped.
        let (owner, _, network, urls, now) = claimed_owner("flood-listed");
        {
            let mut engine = owner.lock();
            rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
            rev_status(accept(&mut engine, 3, 1), "updated", 1);
        }
        let hub = network.hub(&urls[0]);
        let (limit, cap) = (10, 5);
        for i in 0..6 * cap {
            assert!(hub.publish(&stranger_accept(now - 100 - i as u64)));
        }
        for i in 0..3 * cap {
            let tags = invite_tags(&format!("{:032x}", i + 1), (i + 1) as u8, now + DAY, &["guest"]);
            assert!(hub.publish(&wrap_at(&keys(3), KIND_INVITE, tags, now - 1000 - i as u64)));
        }
        let relay = network.connect(&urls);
        let mut seen = Seen::default();
        let (_, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, limit, cap, &mut seen, &|| false);
        let by_3 = found.iter().filter(|b| b.sender == keys(3).public_key()).count();
        assert_eq!(found.len() - by_3, cap);
        assert_eq!(by_3, 3 * cap, "the strangers' cap was full, and the older messages of a listed persona were still read");
        // Reading the other way round: listed messages newer than the strangers' are all held.
        let hub2 = network.hub("wss://two.example");
        for i in 0..3 * cap {
            let tags = invite_tags(&format!("{:032x}", i + 1), (i + 1) as u8, now + DAY, &["guest"]);
            assert!(hub2.publish(&wrap_at(&keys(3), KIND_INVITE, tags, now - 100 - i as u64)));
        }
        for i in 0..6 * cap {
            assert!(hub2.publish(&stranger_accept(now - 5000 - i as u64)));
        }
        let relay2 = network.connect(&["wss://two.example".to_string()]);
        let (_, found) = owner.read_pages(relay2.as_ref(), now - 3 * DAY, limit, cap, &mut Seen::default(), &|| false);
        assert_eq!(found.iter().filter(|b| b.sender == keys(3).public_key()).count(), 3 * cap);
        assert_eq!(found.len(), 3 * cap + cap);
    }

    #[test]
    fn more_than_the_real_cap_of_well_formed_fresh_key_messages_cannot_bury_an_older_owner_revoke() {
        let (owner, invites, network, urls, now) = claimed_owner("flood-real-cap");
        let hub = network.hub(&urls[0]);
        let then = now - 3600;
        assert!(hub.publish(&wrap_at(&keys(2), KIND_INVITE, invite_tags(GRANT, 1, now + DAY, &["guest"]), then)));
        assert!(hub.publish(&wrap_at(&keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], then)));
        let flood = STRANGER_CAP + 200;
        for i in 0..flood {
            assert!(hub.publish(&stranger_accept(now - 10 - (i / 50) as u64)));
        }
        let relay = network.connect(&urls);
        let (reached, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, PAGE_LIMIT, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert!(reached);
        assert_eq!(found.len(), STRANGER_CAP + 2, "held: the cap's worth of strangers, and the owner's two");
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert!(invites.live(now).is_empty(), "the older revoke was applied");
        assert_eq!(owner.lock().grants.iter().map(|g| g.state).collect::<Vec<_>>(), vec![GrantState::Revoked]);
    }

    #[test]
    fn the_claim_with_the_right_code_is_held_outside_the_stranger_cap_and_a_wrong_one_is_not() {
        let dir = scratch("flood-claim");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let urls = vec!["wss://one.example".to_string()];
        let owner = Owner::open(&dir, keys(1), invites, "Home", &urls).unwrap();
        let code = owner.pairing_link().unwrap().split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let now = unix_now();
        let network = FakeNetwork::new();
        let hub = network.hub(&urls[0]);
        let (limit, cap) = (10, 5);
        // The claim is newer than a flood that fills the cap and is held anyway; one with a wrong code is just another stranger.
        assert!(hub.publish(&wrap_at(&keys(2), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], now - 50)));
        assert!(hub.publish(&wrap_at(&keys(3), KIND_CLAIM, vec![keep_tag(), tag(&["code", &hex_encode(&[9; 16])])], now - 60)));
        for i in 0..6 * cap {
            assert!(hub.publish(&stranger_accept(now - 100 - i as u64)));
        }
        let relay = network.connect(&urls);
        let (_, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, limit, cap, &mut Seen::default(), &|| false);
        assert_eq!(found.iter().filter(|b| b.kind == KIND_CLAIM && b.sender == keys(2).public_key()).count(), 1);
        assert_eq!(found.len(), cap + 1, "the wrong claim took one of the strangers' places");
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert!(owner.claimed() && owner.lock().is_active(&keys(2).public_key()));
    }

    #[test]
    fn a_backlog_is_read_whole_and_applied_oldest_rumor_first() {
        // (rumor ages in days of: update, accept, invite). In the second case all three are made in one second.
        for (name, ages) in [("apart", [5 * DAY, 5 * DAY, DAY]), ("one-second", [DAY, DAY, DAY])] {
            let dir = scratch(&format!("backlog-{name}"));
            let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
            let urls = vec!["wss://one.example".to_string()];
            let owner = Owner::open(&dir, keys(1), invites.clone(), "Home", &urls).unwrap();
            let code = {
                let link = owner.pairing_link().unwrap();
                link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
            };
            let now = unix_now();
            let claim = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], now).unwrap();
            assert!(owner.process(&claim, now).is_some());
            let network = FakeNetwork::new();
            let hub = network.hub(&urls[0]);
            let to_keep = |from: u8, kind: u16, tags: Vec<Tag>, age: u64| wire::gift_wrap(&keys(from), &keys(1).public_key(), kind, tags, now - age).unwrap();
            let invite = to_keep(3, KIND_INVITE, invite_tags(GRANT, 1, now + 2 * DAY, &["guest"]), ages[2]);
            let update = to_keep(2, KIND_UPDATE, update_tags("1", &[2, 3]), ages[0]);
            let accept = to_keep(3, KIND_ACCEPT, vec![keep_tag(), tag(&["rev", "1"])], ages[1]);
            // Read newest page first, the invite comes before the update and the accept. Make
            // sure of it: with a page of two and junk newer than all of them, the pages split them.
            for wrap in junk(3, |i| now - 10 - i as u64) {
                assert!(hub.publish(&wrap));
            }
            for wrap in [&invite, &update, &accept] {
                assert!(hub.publish(wrap));
            }
            let relay = network.connect(&urls);
            let mut seen = Seen::default();
            let (reached, found) = owner.read_pages(relay.as_ref(), now - 10 * DAY, 2, STRANGER_CAP, &mut seen, &|| false);
            assert!(reached);
            assert_eq!(found.len(), 3, "only what the Keep acts on is held");
            if name == "apart" {
                let first_invite = found.iter().position(|b| b.kind == KIND_INVITE).unwrap();
                let update_at = found.iter().position(|b| b.kind == KIND_UPDATE).unwrap();
                assert!(first_invite < update_at, "read in the order that would fail if applied as read");
            }
            let relays = [network.connect(&urls)];
            assert!(owner.apply_backlog(found, &mut Seen::default(), &relays, &|| false));
            assert!(owner.lock().is_active(&keys(3).public_key()), "{name}: B was added and accepted");
            assert_eq!(invites.live(now).len(), 1, "{name}: B's invite was judged after that");
            // The three answers went out: updated, updated, active.
            let sent = hub.published().into_iter().filter(|e| e.kind == Kind::GiftWrap && e.tags.public_keys().any(|p| *p == keys(3).public_key())).count();
            assert_eq!(sent, 2, "{name}: the accept and the invite were answered to B");
        }
    }

    /// The published wraps addressed to `who`, opened: (status, grant, reason).
    fn acks_to(published: Vec<Event>, who: u8) -> Vec<(String, Option<String>, Option<String>)> {
        published
            .into_iter()
            .filter(|e| e.kind == Kind::GiftWrap && e.tags.public_keys().any(|p| *p == keys(who).public_key()))
            .filter_map(|e| wire::unwrap(&keys(who), &e).ok())
            .map(|(_, rumor)| {
                let find = |name: &str| rumor.tags.iter().find(|t| t.as_slice()[0] == name).and_then(|t| t.as_slice().get(1).cloned());
                (find("status").unwrap(), find("grant"), find("reason"))
            })
            .collect()
    }

    #[test]
    fn a_persona_removed_in_the_backlog_cannot_back_date_an_invite_in_and_its_older_revokes_still_count() {
        let (owner, invites, network, urls, now) = claimed_owner("backdate");
        let (g2, g3, g4) = ("22222222222222222222222222222222", "33333333333333333333333333333333", "44444444444444444444444444444444");
        {
            let mut engine = owner.lock();
            rev_status(update(&mut engine, 2, 1, &[2, 3]), "updated", 1);
            rev_status(accept(&mut engine, 3, 1), "updated", 1);
            assert_eq!(invite(&mut engine, g2, 2, NOW + DAY, &["guest"]).unwrap()["status"], "active");
            assert_eq!(invite(&mut engine, g3, 3, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        }
        assert_eq!(invites.live(NOW).len(), 2);
        // While the Keep was off: 2 removed 3 at t; 3 (whose key is gone bad) then
        // sent an invite dated just before, a revoke dated just before and one just after.
        let t = now - 1000;
        let hub = network.hub(&urls[0]);
        assert!(hub.publish(&wrap_at(&keys(2), KIND_UPDATE, update_tags("2", &[2]), t)));
        assert!(hub.publish(&wrap_at(&keys(3), KIND_INVITE, invite_tags(g4, 4, now + DAY, &["guest"]), t - 1)));
        assert!(hub.publish(&wrap_at(&keys(3), KIND_REVOKE, vec![keep_tag(), tag(&["grant", g2])], t - 1)));
        assert!(hub.publish(&wrap_at(&keys(3), KIND_REVOKE, vec![keep_tag(), tag(&["grant", g3])], t + 1)));
        let relay = network.connect(&urls);
        let (_, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert_eq!(found.len(), 4);
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        let engine = owner.lock();
        assert!(!engine.is_listed(&keys(3).public_key()), "3 was removed");
        let state = |g: &str| engine.grants.iter().find(|x| x.grant == g).map(|x| x.state);
        assert_eq!(state(g2), Some(GrantState::Revoked), "3 was an owner persona when it revoked this");
        assert_eq!(state(g3), Some(GrantState::Active), "and was not when it revoked this");
        assert_eq!(state(g4), None, "the back-dated invite never got in");
        drop(engine);
        // 3 heard about the one revoke it was allowed, and nothing about its invite.
        assert_eq!(acks_to(hub.published(), 3), vec![("revoked".to_string(), Some(g2.to_string()), None)]);
    }

    #[test]
    fn a_revoke_counts_if_its_signer_is_an_owner_persona_at_the_end_of_the_backlog() {
        let (owner, invites, network, urls, now) = claimed_owner("revoke-late-owner");
        let g5 = "55555555555555555555555555555555";
        let mut tags = invite_tags(g5, 5, now + DAY, &["guest"]);
        tags.retain(|t| t.as_slice()[0] != "keep");
        tags.insert(0, keep_tag());
        assert!(owner.process(&wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_INVITE, tags, now).unwrap(), now).is_some());
        assert_eq!(invites.live(now).len(), 1);
        // 4 revokes before the update that lists it, and accepts after: at the end it is an owner persona.
        let t = now - 1000;
        let hub = network.hub(&urls[0]);
        assert!(hub.publish(&wrap_at(&keys(4), KIND_REVOKE, vec![keep_tag(), tag(&["grant", g5])], t - 5)));
        assert!(hub.publish(&wrap_at(&keys(2), KIND_UPDATE, update_tags("1", &[2, 4]), t)));
        assert!(hub.publish(&wrap_at(&keys(4), KIND_ACCEPT, vec![keep_tag(), tag(&["rev", "1"])], t + 1)));
        let relay = network.connect(&urls);
        let (_, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert!(owner.lock().is_active(&keys(4).public_key()));
        assert!(invites.live(now).is_empty(), "the revoke from 4 was applied");
        // A stranger's revoke is not.
        let g6 = "66666666666666666666666666666666";
        let tags = invite_tags(g6, 6, now + DAY, &["guest"]);
        assert!(owner.process(&wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_INVITE, tags, now).unwrap(), now).is_some());
        assert!(hub.publish(&wrap_at(&keys(7), KIND_REVOKE, vec![keep_tag(), tag(&["grant", g6])], t)));
        let (_, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert_eq!(invites.live(now).len(), 1, "a key that is no owner persona at any point revokes nothing");
    }

    #[test]
    fn strangers_accepts_in_a_backlog_cost_no_write_and_no_answer_and_still_count_for_a_listing_in_the_same_run() {
        let (owner, _, network, urls, now) = claimed_owner("backlog-accepts");
        let dir_file = owner.lock().dir.join(OWNER_FILE);
        let before = std::fs::read_to_string(&dir_file).unwrap();
        let hub = network.hub(&urls[0]);
        let t = now - 1000;
        // 3's accept is older than the update that lists it; a crowd of strangers' accepts (fewer than the 32 held, or they would push it out) come between.
        assert!(hub.publish(&wrap_at(&keys(3), KIND_ACCEPT, vec![keep_tag(), tag(&["rev", "1"])], t - 5)));
        for i in 0..25 {
            assert!(hub.publish(&stranger_accept(t - 4 + i / 10)));
        }
        let relay = network.connect(&urls);
        let (_, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert_eq!(found.len(), 26);
        let published = hub.published().len();
        assert!(owner.apply_backlog(found, &mut Seen::default(), &[network.connect(&urls)], &|| false));
        assert_eq!(hub.published().len(), published, "not one answer, and the file did not change");
        assert_eq!(std::fs::read_to_string(&dir_file).unwrap(), before);
        assert!(owner.lock().pending.len() <= MAX_PENDING && !owner.lock().is_listed(&keys(3).public_key()));
        // The listing arrives (in the same run, here a later backlog): the held accept is used.
        assert!(owner.process(&wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_UPDATE, update_tags("1", &[2, 3]), now).unwrap(), now).is_some());
        assert!(owner.lock().is_active(&keys(3).public_key()), "its accept was held in memory");
    }

    #[test]
    fn the_saved_cursor_waits_for_the_whole_window_to_be_read() {
        let (mut engine, dir) = claimed("paging-cursor");
        engine.flush_cursor();
        let saved = engine.cursor;
        assert!(saved.is_some());
        engine.set_paging(true);
        engine.note_wrap(NOW, NOW + DAY);
        assert_eq!(engine.cursor, Some(NOW));
        // Neither a step, a flush, nor a save for a grant writes it.
        engine.note_wrap(NOW + 500, NOW + DAY);
        engine.flush_cursor();
        assert_eq!(invite(&mut engine, GRANT, 1, NOW + DAY, &["guest"]).unwrap()["status"], "active");
        assert_eq!(engine_in(&dir).cursor, saved, "a crash now loses nothing");
        // Done: it is saved.
        engine.set_paging(false);
        assert_eq!(engine_in(&dir).cursor, Some(NOW + 500));
    }

    #[test]
    fn a_relay_that_was_down_and_is_up_again_is_read_again_once() {
        let mut down = false;
        assert!(!came_back(&mut down, &SubStatus::Connecting));
        assert!(!came_back(&mut down, &SubStatus::Up));
        assert!(!came_back(&mut down, &SubStatus::Down("x".into())));
        assert!(!came_back(&mut down, &SubStatus::Down("x".into())));
        assert!(came_back(&mut down, &SubStatus::Up));
        assert!(!came_back(&mut down, &SubStatus::Up));
    }

    // ─── cross-check fixtures (throwaway keys: Keep sk 0..01, owner sk 0..02) ───

    const FIXTURE_GRANT: &str = "0123456789abcdef0123456789abcdef";
    const FIXTURE_NOW: u64 = 1_800_000_000;

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/stampede-headless/fixtures")
    }

    /// Make `fixtures/k-ack-active.json` with the Keep's own wrap code. Run on
    /// purpose, `cargo test -p race-session --bin stampede-headless make_the_ack_fixture -- --ignored`;
    /// the committed file is what the test below reads.
    #[test]
    #[ignore = "writes the committed fixture"]
    fn make_the_ack_fixture() {
        let (engine, _) = engine("fixture");
        let ack = Ack { to: keys(2).public_key(), status: "active", subject: Subject::Grant(FIXTURE_GRANT.to_string()), reason: None };
        let wrap = engine.ack_wrap(&ack, FIXTURE_NOW).unwrap();
        std::fs::write(fixture_dir().join("k-ack-active.json"), nostr::JsonUtil::as_json(&wrap) + "\n").unwrap();
    }

    /// Make `fixtures/k-updated.json`: the `updated` ack (rev 1) to the owner
    /// `00..02`, from the Keep `00..01`, with the Keep's own wrap code.
    #[test]
    #[ignore = "writes the committed fixture"]
    fn make_the_updated_ack_fixture() {
        let (engine, _) = engine("fixture-updated");
        let ack = Ack::rev(keys(2).public_key(), "updated", 1);
        let wrap = engine.ack_wrap(&ack, FIXTURE_NOW).unwrap();
        std::fs::write(fixture_dir().join("k-updated.json"), nostr::JsonUtil::as_json(&wrap) + "\n").unwrap();
    }

    #[test]
    fn the_committed_updated_ack_fixture_unwraps_for_the_owner_with_every_field() {
        let text = include_str!("fixtures/k-updated.json");
        let wrap: Event = nostr::JsonUtil::from_json(text.trim()).unwrap();
        assert_eq!(wrap.kind, Kind::GiftWrap);
        assert!(wrap.verify().is_ok());
        assert_ne!(wrap.pubkey, keys(1).public_key());
        assert_eq!(wrap.tags.public_keys().copied().collect::<Vec<_>>(), vec![keys(2).public_key()]);
        assert!((FIXTURE_NOW - wire::JITTER_SECS..=FIXTURE_NOW).contains(&wrap.created_at.as_secs()));
        let seal_json = nostr::nips::nip44::decrypt(keys(2).secret_key(), &wrap.pubkey, &wrap.content).unwrap();
        let seal: Event = nostr::JsonUtil::from_json(seal_json).unwrap();
        assert_eq!((seal.kind, seal.pubkey), (Kind::Seal, keys(1).public_key()));
        assert!(seal.verify().is_ok());
        assert!((FIXTURE_NOW - wire::JITTER_SECS..=FIXTURE_NOW).contains(&seal.created_at.as_secs()));
        let (sender, rumor) = wire::unwrap(&keys(2), &wrap).unwrap();
        assert_eq!(sender, keys(1).public_key());
        assert_eq!((rumor.pubkey, rumor.kind, rumor.content.as_str(), rumor.created_at.as_secs()), (keys(1).public_key(), Kind::Custom(27663), "", FIXTURE_NOW));
        let tags: Vec<Vec<String>> = rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        assert_eq!(tags, vec![vec!["keep".to_string(), keys(1).public_key().to_hex()], vec!["status".into(), "updated".into()], vec!["rev".into(), "1".into()]]);
    }

    #[test]
    fn the_committed_ack_fixture_unwraps_for_the_owner_with_every_field() {
        let text = include_str!("fixtures/k-ack-active.json");
        let wrap: Event = nostr::JsonUtil::from_json(text.trim()).unwrap();
        // The wrap: kind 1059, a throwaway key, addressed to the owner, signed.
        assert_eq!(wrap.kind, Kind::GiftWrap);
        assert!(wrap.verify().is_ok());
        assert_ne!(wrap.pubkey, keys(1).public_key());
        assert_eq!(wrap.tags.public_keys().copied().collect::<Vec<_>>(), vec![keys(2).public_key()]);
        let at = wrap.created_at.as_secs();
        assert!((FIXTURE_NOW - wire::JITTER_SECS..=FIXTURE_NOW).contains(&at), "wrap jitter {at}");
        // The seal, opened by hand so each check is seen: kind 13, signed by the Keep,
        // jittered, the rumor inside by the same key.
        let seal_json = nostr::nips::nip44::decrypt(keys(2).secret_key(), &wrap.pubkey, &wrap.content).unwrap();
        let seal: Event = nostr::JsonUtil::from_json(seal_json).unwrap();
        assert_eq!(seal.kind, Kind::Seal);
        assert!(seal.verify().is_ok());
        assert_eq!(seal.pubkey, keys(1).public_key());
        assert!((FIXTURE_NOW - wire::JITTER_SECS..=FIXTURE_NOW).contains(&seal.created_at.as_secs()));
        let (sender, rumor) = wire::unwrap(&keys(2), &wrap).unwrap();
        assert_eq!(sender, keys(1).public_key());
        assert_eq!((rumor.pubkey, rumor.kind, rumor.content.as_str(), rumor.created_at.as_secs()), (keys(1).public_key(), Kind::Custom(27663), "", FIXTURE_NOW));
        let tags: Vec<Vec<String>> = rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        assert_eq!(tags, vec![vec!["keep".to_string(), keys(1).public_key().to_hex()], vec!["status".into(), "active".into()], vec!["grant".into(), FIXTURE_GRANT.into()]]);
        // The Keep's key is the one in its links.
        assert_eq!(keys(1).public_key().to_hex(), "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798");
    }

    /// The Keep's clock for My Signet's fixtures: their rumors' `created_at`.
    const M_NOW: u64 = 1_792_657_600;

    fn m_fixture(text: &str) -> Event {
        nostr::JsonUtil::from_json(text.trim()).unwrap()
    }

    #[test]
    fn my_signets_update_and_accept_fixtures_unwrap_and_are_accepted_by_the_rules() {
        let claim = m_fixture(include_str!("fixtures/m-claim.json"));
        let update = m_fixture(include_str!("fixtures/m-update.json"));
        let accept = m_fixture(include_str!("fixtures/m-accept.json"));
        // Opened by hand first: every field of the contract.
        let (sender, rumor) = wire::unwrap(&keys(1), &update).unwrap();
        assert_eq!((sender, rumor.pubkey, rumor.kind, rumor.content.as_str(), rumor.created_at.as_secs()), (keys(2).public_key(), keys(2).public_key(), Kind::Custom(27664), "", M_NOW));
        let tags: Vec<Vec<String>> = rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        assert_eq!(
            tags,
            vec![
                vec!["keep".to_string(), keys(1).public_key().to_hex()],
                vec!["rev".into(), "1".into()],
                vec!["persona".into(), hex_of(2), "games".into()],
                vec!["persona".into(), hex_of(3), "games".into()],
            ]
        );
        let (sender, rumor) = wire::unwrap(&keys(1), &accept).unwrap();
        assert_eq!((sender, rumor.pubkey, rumor.kind, rumor.content.as_str(), rumor.created_at.as_secs()), (keys(3).public_key(), keys(3).public_key(), Kind::Custom(27665), "", M_NOW));
        let tags: Vec<Vec<String>> = rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        assert_eq!(tags, vec![vec!["keep".to_string(), keys(1).public_key().to_hex()], vec!["rev".into(), "1".into()]]);
        // And through the rules: claim (no scope = one), update, accept; each answered with the revision.
        let dir = scratch("mfixtures-k1e");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let code = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let mut engine = Engine::open(&dir, keys(1), invites, code).unwrap();
        assert!(engine.process(&claim, M_NOW).is_some());
        assert_eq!(engine.data.scope, "one");
        let reply = |who: u8, ack: Event| -> Reply {
            let (sender, rumor) = wire::unwrap(&keys(who), &ack).unwrap();
            assert_eq!(sender, keys(1).public_key());
            rumor.tags.iter().map(|t| (t.as_slice()[0].clone(), t.as_slice().get(1).cloned().unwrap_or_default())).collect()
        };
        let ack = reply(2, engine.process(&update, M_NOW).expect("updated"));
        assert_eq!((ack["status"].as_str(), ack["rev"].as_str()), ("updated", "1"));
        assert!(engine.is_listed(&keys(3).public_key()) && !engine.is_active(&keys(3).public_key()));
        let ack = reply(3, engine.process(&accept, M_NOW).expect("updated"));
        assert_eq!((ack["status"].as_str(), ack["rev"].as_str()), ("updated", "1"));
        assert!(engine.is_active(&keys(3).public_key()));
        assert_eq!(engine.data.owners[1].sections, vec!["games".to_string()]);
    }

    #[test]
    fn my_signets_claim_and_invite_fixtures_unwrap_and_are_accepted_by_the_rules() {
        let claim = m_fixture(include_str!("fixtures/m-claim.json"));
        let invite = m_fixture(include_str!("fixtures/m-invite.json"));
        // Opened by hand first: every field of the contract.
        let (sender, rumor) = wire::unwrap(&keys(1), &claim).unwrap();
        assert_eq!((sender, rumor.pubkey, rumor.kind, rumor.content.as_str()), (keys(2).public_key(), keys(2).public_key(), Kind::Custom(27660), ""));
        assert_eq!(one_tag(&rumor, "keep"), Ok(Some(keys(1).public_key().to_hex().as_str())));
        assert_eq!(one_tag(&rumor, "code"), Ok(Some("00112233445566778899aabbccddeeff")));
        let (sender, rumor) = wire::unwrap(&keys(1), &invite).unwrap();
        assert_eq!((sender, rumor.kind, rumor.content.as_str()), (keys(2).public_key(), Kind::Custom(27661), ""));
        assert_eq!(one_tag(&rumor, "grant"), Ok(Some("ffeeddccbbaa99887766554433221100")));
        assert_eq!(one_tag(&rumor, "expiration"), Ok(Some("1795249600")));
        assert_eq!(one_tag(&rumor, "name"), Ok(Some("Dave")));
        assert!(!has_tag(&rumor, "guest"));
        // And through the rules, with the code those fixtures carry.
        let dir = scratch("mfixtures");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let code = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let mut engine = Engine::open(&dir, keys(1), invites, code).unwrap();
        let ack = engine.process(&invite, M_NOW);
        assert!(ack.is_none(), "an invite before the claim is dropped");
        let ack = engine.process(&claim, M_NOW).expect("paired");
        let (_, rumor) = wire::unwrap(&keys(2), &ack).unwrap();
        assert_eq!(one_tag(&rumor, "status"), Ok(Some("paired")));
        assert!(has_tag(&rumor, "claim"));
        let ack = engine.process(&invite, M_NOW).expect("active");
        let (_, rumor) = wire::unwrap(&keys(2), &ack).unwrap();
        assert_eq!((one_tag(&rumor, "status"), one_tag(&rumor, "grant")), (Ok(Some("active")), Ok(Some("ffeeddccbbaa99887766554433221100"))));
        let view = &engine.invites.views(M_NOW)[0];
        assert_eq!(
            (view.kind, view.source, view.label.as_deref(), view.holder.as_deref(), view.expires_at),
            ("personal", "owner", Some("Dave"), Some(keys(3).public_key().to_hex().as_str()), 1_795_249_600)
        );
        let secret: Vec<u8> = (0u8..32).collect();
        assert_eq!(engine.invites.live(M_NOW)[0].1.as_slice(), secret.as_slice());
    }
    fn made(from: &Keys, kind: u16, tags: Vec<Tag>) -> UnsignedEvent {
        nostr::EventBuilder::new(Kind::Custom(kind), "").tags(tags).custom_created_at(Timestamp::from_secs(NOW)).build(from.public_key())
    }

    #[test]
    fn a_handled_message_gets_one_line_with_its_kind_outcome_rev_and_short_sender() {
        let (mut engine, _dir) = claimed("msgline");
        let owner = keys(2);
        let prefix: String = owner.public_key().to_hex().chars().take(6).collect();
        let line_of = |engine: &mut Engine, kind: u16, tags: Vec<Tag>, source: Source| {
            let rumor = made(&owner, kind, tags);
            let ack = engine.handle(owner.public_key(), &rumor, NOW, source);
            message_line(&rumor, &owner.public_key(), source, ack.as_ref()).expect("a line")
        };
        // An invite: kind, sender, source and the outcome; the secret and grant stay out.
        let line = line_of(&mut engine, KIND_INVITE, invite_tags(GRANT, 7, NOW + DAY, &["guest"]), Source::Live);
        assert_eq!(line, format!("[keep] owner invite from {prefix} (live): active"), "{line}");
        assert!(!line.contains(&secret(7)) && !line.contains(GRANT) && !line.contains(&owner.public_key().to_hex()));
        // A refusal says why; one read from a start-up read says so.
        let line = line_of(&mut engine, KIND_INVITE, invite_tags(GRANT, 8, NOW + DAY, &["guest"]), Source::Backlog { revoke_ok: false });
        assert_eq!(line, format!("[keep] owner invite from {prefix} (backlog): refused (conflict)"), "{line}");
        let line = line_of(&mut engine, KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], Source::Live);
        assert!(line.starts_with(&format!("[keep] owner revoke from {prefix} (live): ")), "{line}");
        // An update names the revision it carries, and a stale one the Keep's own.
        let line = line_of(&mut engine, KIND_UPDATE, update_tags("1", &[2, 3]), Source::Live);
        assert_eq!(line, format!("[keep] owner update from {prefix} (live) rev 1: updated"), "{line}");
        let line = line_of(&mut engine, KIND_UPDATE, update_tags("1", &[2]), Source::Live);
        assert!(line.starts_with(&format!("[keep] owner update from {prefix} (live) rev 1: ")), "{line}");
        let line = line_of(&mut engine, KIND_UPDATE, update_tags("0", &[2]), Source::Live);
        assert_eq!(line, format!("[keep] owner update from {prefix} (live) rev 0: refused (stale), the Keep is at rev 1"), "{line}");
        let line = line_of(&mut engine, KIND_ACCEPT, vec![keep_tag(), tag(&["rev", "1"])], Source::Live);
        assert!(line.starts_with(&format!("[keep] owner accept from {prefix} (live) rev 1: ")), "{line}");
        // Said nothing back, and a kind the Keep does not act on.
        let rumor = made(&owner, KIND_UPDATE, vec![keep_tag(), tag(&["rev", "x"])]);
        let silent = message_line(&rumor, &owner.public_key(), Source::Live, None).unwrap();
        assert_eq!(silent, format!("[keep] owner update from {prefix} (live) rev ?: no ack"));
        assert!(message_line(&made(&owner, 1, vec![]), &owner.public_key(), Source::Live, None).is_none());
    }

    #[test]
    fn the_ack_line_names_the_receiver_by_a_short_key_and_the_relays() {
        let (engine, _dir) = claimed("ackline");
        let ack = Ack::rev(keys(2).public_key(), "updated", 1);
        let wrap = engine.ack_wrap(&ack, NOW).unwrap();
        let prefix: String = keys(2).public_key().to_hex().chars().take(6).collect();
        assert_eq!(ack_sent_line(&wrap, 3), format!("[keep] ack to {prefix} sent to 3 relays"));
    }
    // ─── how hard the relays are leaned on ──────────────────────────────────

    #[test]
    fn a_relay_is_read_again_after_waits_that_double_and_start_over_after_a_calm_spell() {
        let pacing = Pacing { page_gap: Duration::ZERO, redial_base: Duration::from_secs(15), redial_max: Duration::from_secs(600), calm: Duration::from_secs(1200) };
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut redial = Redial::new();
        assert!(redial.ready(at(0)), "the first read is not held up");
        redial.started(at(0), &pacing);
        assert!(!redial.ready(at(14)) && redial.ready(at(15)));
        // It comes back at once and again: 30 s, 60 s, ... up to ten minutes.
        let mut now = 15;
        let mut waits = Vec::new();
        for _ in 0..9 {
            redial.started(at(now), &pacing);
            let next = redial.next.unwrap();
            let wait = next.duration_since(at(now)).as_secs();
            waits.push(wait);
            now += wait;
        }
        assert_eq!(waits, [30, 60, 120, 240, 480, 600, 600, 600, 600]);
        // Left alone for twenty minutes, the next read starts over at the short wait.
        redial.started(at(now + 1200), &pacing);
        assert_eq!(redial.next.unwrap().duration_since(at(now + 1200)).as_secs(), 15);
    }

    #[test]
    fn a_long_read_waits_between_pages() {
        let (owner, _invites, network, urls, now) = claimed_owner("page-gap");
        let owner = owner.with_pacing(Pacing { page_gap: Duration::from_millis(60), ..Pacing::NONE });
        let hub = network.hub(&urls[0]);
        let limit = 10;
        for wrap in junk(3 * limit + 1, |i| now - 10 - i as u64) {
            assert!(hub.publish(&wrap));
        }
        let relay = network.connect(&urls);
        let before = hub.reqs().len();
        let started = Instant::now();
        let (reached, _) = owner.read_pages(relay.as_ref(), now - 3 * DAY, limit, STRANGER_CAP, &mut Seen::default(), &|| false);
        let took = started.elapsed();
        let pages = hub.reqs().len() - before;
        assert!(reached && pages >= 4, "{pages} pages");
        assert!(took >= Duration::from_millis(60) * (pages as u32 - 1), "{pages} pages in {took:?}");
        // A stop ends the wait too.
        let stopped = Instant::now();
        let _ = owner.read_pages(relay.as_ref(), now - 3 * DAY, limit, STRANGER_CAP, &mut Seen::default(), &|| stopped.elapsed() > Duration::from_millis(20));
        assert!(stopped.elapsed() < Duration::from_millis(400));
    }

    #[test]
    fn a_relay_that_closes_the_read_is_not_taken_for_the_end_of_the_window() {
        let (owner, _invites, network, urls, now) = claimed_owner("closed-read");
        let hub = network.hub(&urls[0]);
        for wrap in junk(5, |i| now - 10 - i as u64) {
            assert!(hub.publish(&wrap));
        }
        let relay = network.connect(&urls);
        hub.set_closing(Some("rate-limited: slow down"));
        let (reached, found) = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert!(!reached && found.is_empty(), "a refused read is not a read window");
        hub.set_closing(None);
        let (reached, _) = owner.read_pages(relay.as_ref(), now - 3 * DAY, 10, STRANGER_CAP, &mut Seen::default(), &|| false);
        assert!(reached);
    }

    #[test]
    fn a_relay_that_closes_the_request_is_asked_again_slowly_and_heard_once_it_lets_go() {
        let dir = scratch("loop-closed");
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let urls = vec!["wss://one.example".to_string()];
        let pacing = Pacing { page_gap: Duration::ZERO, redial_base: Duration::from_millis(100), redial_max: Duration::from_millis(400), calm: Duration::from_secs(60) };
        let owner = Arc::new(Owner::open(&dir, keys(1), invites, "Home", &urls).unwrap().with_pacing(pacing));
        let code = {
            let link = owner.pairing_link().unwrap();
            link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
        };
        let network = FakeNetwork::new();
        let hub = network.hub(&urls[0]);
        hub.set_closing(Some("rate-limited: slow down"));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let relays: Vec<Arc<dyn Relay>> = vec![network.connect(&urls)];
            let (owner, stop) = (owner.clone(), stop.clone());
            std::thread::spawn(move || owner.run(relays, || stop.load(std::sync::atomic::Ordering::Relaxed)))
        };
        // Refused for a second: asked again, but a few times (waits of 0.1, 0.2, 0.4 s), not in a stream.
        std::thread::sleep(Duration::from_millis(1000));
        let asked = hub.reqs().len();
        assert!((4..=14).contains(&asked), "{asked} requests in a second of refusal");
        // It lets go: the next ask is answered, and the Keep hears a claim again.
        hub.set_closing(None);
        let now = unix_now();
        let claim = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], now).unwrap();
        let mine = network.connect(&urls);
        assert!(wait_for(|| {
            mine.publish(&claim);
            owner.claimed()
        }), "the Keep never heard the claim after the relay let go");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }
    // ─── a read cut short (review of the pacing) ────────────────────────────

    /// A relay that lets `allow` requests through and then CLOSEs every one,
    /// until it is opened.
    struct Gate {
        inner: Arc<dyn Relay>,
        hub: decented_signet::relay::FakeRelayHub,
        allow: std::sync::atomic::AtomicIsize,
        open: std::sync::atomic::AtomicBool,
    }

    impl Gate {
        fn open(&self) {
            self.open.store(true, std::sync::atomic::Ordering::SeqCst);
            self.hub.set_closing(None);
        }
    }

    impl Relay for Gate {
        fn publish(&self, ev: &Event) {
            self.inner.publish(ev)
        }
        fn subscribe(&self, filters: Vec<Filter>) -> Subscription {
            use std::sync::atomic::Ordering::SeqCst;
            if self.allow.fetch_sub(1, SeqCst) <= 0 && !self.open.load(SeqCst) {
                self.hub.set_closing(Some("rate-limited: slow down"));
            }
            self.inner.subscribe(filters)
        }
        fn urls(&self) -> Vec<String> {
            self.inner.urls()
        }
    }

    /// A Keep claimed ten days ago and off since. Its relay holds, newest first
    /// in pages of 10: an update that removes persona 3 (and 25 strangers'
    /// wraps), then three days of nothing, then persona 3's revoke of a grant
    /// and invite, persona 3's accept and the update that listed it.
    struct Week {
        dir: PathBuf,
        owner: Arc<Owner>,
        network: FakeNetwork,
        urls: Vec<String>,
        now: u64,
    }

    fn week_off(name: &str, pacing: Pacing) -> Week {
        let dir = scratch(name);
        let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
        let urls = vec!["wss://one.example".to_string()];
        let owner = Arc::new(Owner::open(&dir, keys(1), invites, "Home", &urls).unwrap().with_pacing(pacing));
        let code = {
            let link = owner.pairing_link().unwrap();
            link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
        };
        let now = unix_now();
        let then = now - 10 * DAY;
        let claim = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], then).unwrap();
        assert!(owner.process(&claim, then).is_some());
        let network = FakeNetwork::new();
        let hub = network.hub(&urls[0]);
        let put = |from: u8, kind: u16, tags: Vec<Tag>, at: u64| assert!(hub.publish(&wrap_at(&keys(from), kind, tags, at)));
        put(2, KIND_UPDATE, update_tags("1", &[2, 3]), now - 9 * DAY);
        put(3, KIND_ACCEPT, vec![keep_tag(), tag(&["rev", "1"])], now - 9 * DAY + 60);
        put(3, KIND_INVITE, invite_tags(GRANT, 1, now + DAY, &["guest"]), now - 8 * DAY);
        put(3, KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], now - 7 * DAY);
        for wrap in junk(25, |i| now - 3 * DAY - i as u64) {
            assert!(hub.publish(&wrap));
        }
        put(2, KIND_UPDATE, update_tags("2", &[2]), now - DAY);
        Week { dir, owner, network, urls, now }
    }

    fn revoked(owner: &Owner) -> bool {
        owner.lock().grants.iter().any(|g| g.grant == GRANT && g.state == GrantState::Revoked)
    }

    fn gated(week: &Week, allow: isize) -> Arc<Gate> {
        gate_on(week.network.connect(&week.urls), week.network.hub(&week.urls[0]), allow)
    }

    fn gate_on(inner: Arc<dyn Relay>, hub: decented_signet::relay::FakeRelayHub, allow: isize) -> Arc<Gate> {
        Arc::new(Gate { inner, hub, allow: std::sync::atomic::AtomicIsize::new(allow), open: std::sync::atomic::AtomicBool::new(false) })
    }

    fn run_in_thread(owner: &Arc<Owner>, relay: Arc<dyn Relay>, stop: &Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
        let (owner, stop) = (owner.clone(), stop.clone());
        std::thread::spawn(move || owner.run_with(&[relay], 10, STRANGER_CAP, &|| stop.load(std::sync::atomic::Ordering::Relaxed)))
    }

    #[test]
    fn a_read_cut_short_holds_the_cursor_and_the_newer_pages_until_the_rest_is_read() {
        let pacing = Pacing { page_gap: Duration::ZERO, redial_base: Duration::from_millis(100), redial_max: Duration::from_millis(100), calm: Duration::from_secs(60) };
        let week = week_off("cut-resume", pacing);
        // The standing request and the first page get through; the second page is CLOSED.
        let gate = gated(&week, 2);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_in_thread(&week.owner, gate.clone(), &stop);
        let hub = week.network.hub(&week.urls[0]);
        assert!(wait_for(|| hub.reqs().len() >= 4), "no retry was made");
        std::thread::sleep(Duration::from_millis(300));
        {
            let engine = week.owner.lock();
            // The update on the first page (which removes persona 3) is held, not applied:
            // persona 3's revoke on a page not read yet must be judged first.
            assert_eq!(engine.data.rev, 0);
            assert!(engine.cursor.unwrap() < week.now - 9 * DAY, "the cursor moved past the unread pages");
            assert!(engine.cursor_saved.unwrap() < week.now - 9 * DAY);
        }
        gate.open();
        assert!(wait_for(|| revoked(&week.owner) && week.owner.lock().data.rev == 2), "the rest of the window was never read");
        {
            let engine = week.owner.lock();
            // Judged in its place: persona 3 could revoke then, and is gone since.
            assert!(!engine.is_active(&keys(3).public_key()));
        }
        // The cursor is saved after the pages are applied; wait for it rather than look once.
        assert!(wait_for(|| wait_for_cursor(&week.owner.lock(), week.now - 2 * DAY)), "the cursor was never saved past the window");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    fn wait_for_cursor(engine: &Engine, at_least: u64) -> bool {
        engine.cursor_saved.is_some_and(|saved| saved >= at_least)
    }

    #[test]
    fn a_read_cut_short_leaves_the_saved_cursor_so_a_restart_still_reads_the_older_revoke() {
        let pacing = Pacing { page_gap: Duration::ZERO, redial_base: Duration::from_millis(100), redial_max: Duration::from_millis(100), calm: Duration::from_secs(60) };
        let week = week_off("cut-restart", pacing);
        let gate = gated(&week, 2);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_in_thread(&week.owner, gate, &stop);
        let hub = week.network.hub(&week.urls[0]);
        assert!(wait_for(|| hub.reqs().len() >= 4));
        // A live message while the window is unread must not save the cursor either.
        let live = wrap_at(&keys(2), KIND_INVITE, invite_tags("22222222222222222222222222222222", 2, week.now + DAY, &["guest"]), week.now - 60);
        assert!(week.owner.process(&live, week.now).is_some());
        assert!(week.owner.lock().cursor.unwrap() > week.now - 120);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
        // The Keep starts again from what was saved.
        let invites = Arc::new(Invites::open(&week.dir, keys(1).public_key().to_bytes()).unwrap());
        let again = Arc::new(Owner::open(&week.dir, keys(1), invites, "Home", &week.urls).unwrap());
        assert!(again.lock().since(week.now) < week.now - 10 * DAY, "the saved cursor moved past the unread pages");
        hub.set_closing(None);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_in_thread(&again, week.network.connect(&week.urls), &stop);
        assert!(wait_for(|| revoked(&again)), "the revoke on the older page was lost");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    #[test]
    fn the_replay_of_an_old_request_is_not_applied_while_the_ordered_read_is_owed() {
        use tokio::sync::{mpsc, watch};
        // The first request (the standing one) is the test's to drive; the rest go to the hub.
        struct Scripted {
            inner: Arc<dyn Relay>,
            first: Mutex<Option<Subscription>>,
        }
        impl Relay for Scripted {
            fn publish(&self, ev: &Event) {
                self.inner.publish(ev)
            }
            fn subscribe(&self, filters: Vec<Filter>) -> Subscription {
                match self.first.lock().unwrap().take() {
                    Some(sub) => sub,
                    None => self.inner.subscribe(filters),
                }
            }
            fn urls(&self) -> Vec<String> {
                self.inner.urls()
            }
        }
        let pacing = Pacing { page_gap: Duration::ZERO, redial_base: Duration::from_millis(900), redial_max: Duration::from_millis(900), calm: Duration::from_secs(60) };
        let (owner, _invites, network, urls, now) = claimed_owner("replay-owed");
        let owner = Arc::new(owner.with_pacing(pacing));
        let update = wrap_at(&keys(2), KIND_UPDATE, update_tags("1", &[2, 3]), now - 5);
        let (tx, rx) = mpsc::channel(16);
        let (status, status_rx) = watch::channel(SubStatus::Up);
        let scripted = Scripted { inner: network.connect(&urls), first: Mutex::new(Some(Subscription::new(rx, 1, None).with_status(status_rx))) };
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_in_thread(&owner, Arc::new(scripted), &stop);
        std::thread::sleep(Duration::from_millis(150));
        // The relay drops and comes back, and replays the old request's window,
        // which now holds an update the ordered read has yet to see.
        assert!(network.hub(&urls[0]).publish(&update));
        status.send_replace(SubStatus::Down("gone".into()));
        std::thread::sleep(Duration::from_millis(150));
        status.send_replace(SubStatus::Up);
        tx.try_send(RelayMsg::Event(Box::new(update))).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(owner.lock().data.rev, 0, "the replay was applied before the read it waits for");
        assert!(wait_for(|| owner.lock().data.rev == 1), "the ordered read never applied the update");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    // ─── a relay that wants NIP-42 authentication ───────────────────────────

    const CHALLENGE: &str = "auth-test-challenge";

    /// What the loopback relay says to the answer to its challenge.
    #[derive(Clone, Copy, PartialEq)]
    enum Okay {
        Accept,
        /// OK false, as relay.damus.io does today.
        Refuse,
        /// No OK at all.
        Silent,
    }

    /// What the loopback relay saw: one entry per REQ (`REQ` or `REQ closed`),
    /// and `AUTH <pubkey>` for each answer to its challenge.
    type RelayLog = Arc<Mutex<Vec<String>>>;

    /// A relay in front of `network`'s hub for `urls[0]` that challenges on
    /// connect and closes every REQ `auth-required` until the client has
    /// answered with a good kind 22242 event.
    fn serve_auth(network: &FakeNetwork, hub_url: &str, reply: Okay) -> (String, RelayLog) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let seen: RelayLog = Arc::default();
        let (url_tx, url_rx) = std::sync::mpsc::channel();
        let (network, hub_url, log) = (network.clone(), hub_url.to_string(), seen.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("ws://{}", listener.local_addr().unwrap());
                url_tx.send(url.clone()).unwrap();
                while let Ok((tcp, _)) = listener.accept().await {
                    let (network, hub_url, log, url) = (network.clone(), hub_url.clone(), log.clone(), url.clone());
                    tokio::spawn(async move {
                        let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else { return };
                        let (mut sink, mut stream) = ws.split();
                        let (out, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                        tokio::spawn(async move {
                            while let Some(text) = out_rx.recv().await {
                                if sink.send(Message::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                        });
                        let relay = network.connect(std::slice::from_ref(&hub_url));
                        let mut authed = false;
                        let mut challenged = false;
                        let mut subs: Vec<tokio::task::JoinHandle<()>> = Vec::new();
                        while let Some(Ok(Message::Text(text))) = stream.next().await {
                            let Ok(serde_json::Value::Array(a)) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                            match a.first().and_then(|v| v.as_str()) {
                                Some("AUTH") => {
                                    let ev: Event = serde_json::from_value(a[1].clone()).unwrap();
                                    let tag = |name: &str| ev.tags.iter().find(|t| t.as_slice().first().map(String::as_str) == Some(name)).and_then(|t| t.as_slice().get(1).cloned());
                                    authed = reply == Okay::Accept
                                        && ev.kind.as_u16() == 22242
                                        && ev.verify().is_ok()
                                        && tag("challenge").as_deref() == Some(CHALLENGE)
                                        && tag("relay").is_some_and(|r| r.trim_end_matches('/') == url);
                                    log.lock().unwrap().push(format!("AUTH {}", ev.pubkey.to_hex()));
                                    if reply != Okay::Silent {
                                        let _ = out.send(serde_json::json!(["OK", ev.id.to_hex(), authed, if authed { "" } else { "error: no" }]).to_string());
                                    }
                                }
                                Some("REQ") => {
                                    let id = a[1].as_str().unwrap_or("").to_string();
                                    if !authed {
                                        // The challenge comes just before the first refusal.
                                        if !std::mem::replace(&mut challenged, true) {
                                            let _ = out.send(serde_json::json!(["AUTH", CHALLENGE]).to_string());
                                        }
                                        log.lock().unwrap().push("REQ closed".into());
                                        let _ = out.send(serde_json::json!(["CLOSED", id, "ERROR: auth-required: requested filter requires authentication"]).to_string());
                                        continue;
                                    }
                                    log.lock().unwrap().push("REQ".into());
                                    let filters: Vec<Filter> = a[2..].iter().filter_map(|f| serde_json::from_value(f.clone()).ok()).collect();
                                    let mut sub = relay.subscribe(filters);
                                    let out = out.clone();
                                    subs.push(tokio::spawn(async move {
                                        // The hub's own subscription has no waker: poll it.
                                        loop {
                                            while let Some(m) = sub.try_recv() {
                                                let frame = match m {
                                                    RelayMsg::Event(ev) => serde_json::json!(["EVENT", id, ev]),
                                                    RelayMsg::Eose => serde_json::json!(["EOSE", id]),
                                                };
                                                if out.send(frame.to_string()).is_err() {
                                                    return;
                                                }
                                            }
                                            tokio::time::sleep(Duration::from_millis(5)).await;
                                        }
                                    }));
                                }
                                _ => {}
                            }
                        }
                        for sub in subs {
                            sub.abort();
                        }
                    });
                }
            });
        });
        (url_rx.recv().unwrap(), seen)
    }

    /// The Keep reads a window from a relay that wants authentication first. With the
    /// Keep's own connections, the challenge is answered, the requests it closed are
    /// asked again, and the whole window is read, the saved cursor moving only as the
    /// ordinary rules allow. With a plain connection the relay is never read, and the
    /// saved cursor stays where it was.
    #[test]
    fn a_relay_that_wants_authentication_is_read_once_the_keep_has_answered() {
        // Whether the Keep's own connections are used (else a plain one), and what the relay says.
        let read = |authenticate: bool, reply: Okay| {
            let week = week_off("auth", quick_redial());
            let (url, seen) = serve_auth(&week.network, &week.urls[0], reply);
            let connector: Box<dyn Connector> = if authenticate { Box::new(AuthConnector::new(auth_signer(keys(1)))) } else { Box::new(decented_signet::relay::WsConnector) };
            let relay = connector.connect(std::slice::from_ref(&url));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread = run_in_thread(&week.owner, relay, &stop);
            let read_all = || revoked(&week.owner) && week.owner.lock().data.rev == 2;
            let done = if reply == Okay::Accept && authenticate {
                wait_for(read_all)
            } else {
                // Nothing is read: long enough to see that, and for a page to time out.
                (0..200).any(|_| (std::thread::sleep(Duration::from_millis(20)), read_all()).1)
            };
            if done {
                assert!(wait_for(|| week.owner.lock().cursor_saved.is_some_and(|saved| saved >= week.now - 2 * DAY)), "the cursor was not saved after the whole window was read");
            }
            let (cursor, rev, paging) = {
                let engine = week.owner.lock();
                (engine.cursor_saved.unwrap(), engine.data.rev, engine.paging)
            };
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            thread.join().unwrap();
            let seen = seen.lock().unwrap().clone();
            (done, cursor, rev, paging, seen, week.now)
        };

        let (done, cursor, _, _, seen, now) = read(true, Okay::Accept);
        assert!(done, "the window was never read: {seen:?}");
        assert!(cursor >= now - 2 * DAY);
        // Closed for want of authentication, answered once, as the Keep, then asked again.
        assert!(seen.iter().filter(|s| *s == "REQ closed").count() >= 1, "{seen:?}");
        assert_eq!(seen.iter().filter(|s| s.starts_with("AUTH ")).collect::<Vec<_>>(), [&format!("AUTH {}", keys(1).public_key().to_hex())], "{seen:?}");
        assert!(seen.iter().filter(|s| *s == "REQ").count() >= 2, "{seen:?}");

        // A plain connection, a relay that says no (damus), and one that says nothing:
        // none is read, and none moves the saved cursor.
        for (authenticate, reply) in [(false, Okay::Accept), (true, Okay::Refuse), (true, Okay::Silent)] {
            let (done, cursor, rev, paging, seen, now) = read(authenticate, reply);
            assert!(paging, "the window was taken for read ({authenticate}): {seen:?}");
            assert!(!done && rev == 0, "a relay that was not authenticated to was read ({authenticate}): {seen:?}");
            assert!(cursor < now - 9 * DAY, "the cursor moved though no window was read ({authenticate}): {seen:?}");
            assert!(seen.iter().all(|s| s == "REQ closed" || s.starts_with("AUTH ")), "{seen:?}");
        }
    }

    /// A page that never finishes is not the end of the window: the saved cursor
    /// stays where it was, and the read is made again.
    #[test]
    fn a_page_that_times_out_is_a_cut_read_not_a_whole_window() {
        let week = week_off("page-timeout", quick_redial());
        let hub = week.network.hub(&week.urls[0]);
        hub.set_mute(true);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_in_thread(&week.owner, week.network.connect(&week.urls), &stop);
        // Past the page wait, so the read has timed out at least once.
        std::thread::sleep(PAGE_WAIT * 3);
        {
            let engine = week.owner.lock();
            assert_eq!(engine.data.rev, 0);
            // The first page is asked for again after each wait; no later page is.
            let reqs = hub.reqs();
            assert!(reqs.len() >= 4, "the first page was not asked for again: {}", reqs.len());
            assert!(reqs.iter().all(|r| r.filters.iter().all(|f| f.until.is_none())), "a page that timed out was followed by an older one");
            assert!(engine.paging, "a page that timed out was taken for the end of the window");
            assert!(engine.cursor_saved.unwrap() < week.now - 9 * DAY, "the cursor moved past a window that was not read");
        }
        hub.set_mute(false);
        assert!(wait_for(|| revoked(&week.owner) && week.owner.lock().data.rev == 2), "the window was not read once the relay answered");
        assert!(wait_for(|| week.owner.lock().cursor_saved.is_some_and(|saved| saved >= week.now - 2 * DAY)));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    // ─── what a cut relay holds, and what came while it was cut ─────────────

    fn quick_redial() -> Pacing {
        Pacing { page_gap: Duration::ZERO, redial_base: Duration::from_millis(100), redial_max: Duration::from_millis(100), calm: Duration::from_secs(60) }
    }

    fn run_relays(owner: &Arc<Owner>, relays: Vec<Arc<dyn Relay>>, stop: &Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
        let (owner, stop) = (owner.clone(), stop.clone());
        std::thread::spawn(move || owner.run_with(&relays, 10, STRANGER_CAP, &|| stop.load(std::sync::atomic::Ordering::Relaxed)))
    }

    /// Acks the Keep has sent to persona 2 through `hub`.
    fn acks_to_2(hub: &decented_signet::relay::FakeRelayHub) -> usize {
        let to = keys(2).public_key().to_hex();
        hub.published().iter().filter(|e| e.tags.iter().any(|t| t.as_slice().first().map(String::as_str) == Some("p") && t.as_slice().get(1) == Some(&to))).count()
    }

    /// A Keep (owner 2) with a live grant, and relay one holding the owner's revoke of
    /// it on its newest page with fifteen junk wraps behind: a relay that lets
    /// the standing request and that page through and refuses the rest holds
    /// the revoke unapplied.
    fn revoke_behind_a_refusal(name: &str) -> (Arc<Owner>, Arc<Invites>, FakeNetwork, Event, u64) {
        let (owner, invites, network, urls, now) = claimed_owner(name);
        let owner = Arc::new(owner.with_pacing(quick_redial()));
        let made = wrap_at(&keys(2), KIND_INVITE, invite_tags(GRANT, 1, now + DAY, &["guest"]), now - 5000);
        assert!(owner.process(&made, now).is_some());
        assert!(!invites.live(now).is_empty());
        let revoke = wrap_at(&keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], now - 10);
        let hub = network.hub(&urls[0]);
        assert!(hub.publish(&revoke));
        for wrap in junk(15, |i| now - 100 - i as u64) {
            assert!(hub.publish(&wrap));
        }
        (owner, invites, network, revoke, now)
    }

    #[test]
    fn a_revoke_held_by_a_refusing_relay_is_applied_from_another_relays_read() {
        let (owner, invites, network, revoke, now) = revoke_behind_a_refusal("held-other-read");
        let two = network.hub("wss://two.example");
        assert!(two.publish(&revoke));
        let gate = gate_on(network.connect(&["wss://one.example".to_string()]), network.hub("wss://one.example"), 2);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_relays(&owner, vec![gate.clone(), network.connect(&["wss://two.example".to_string()])], &stop);
        assert!(wait_for(|| revoked(&owner)), "the other relay's copy of the revoke was skipped");
        assert!(invites.live(now).is_empty());
        assert!(owner.lock().paging, "relay one is still cut, so the saved cursor is held");
        // Relay one lets go: its held copy is the same wrap, and is not applied again.
        gate.open();
        assert!(wait_for(|| !owner.lock().paging), "relay one's window was never read whole");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(acks_to_2(&two), 1, "the revoke was answered once");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    #[test]
    fn a_revoke_held_by_a_refusing_relay_is_applied_from_another_relays_live_feed() {
        let (owner, invites, network, revoke, now) = revoke_behind_a_refusal("held-other-live");
        let two = network.hub("wss://two.example");
        let gate = gate_on(network.connect(&["wss://one.example".to_string()]), network.hub("wss://one.example"), 2);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_relays(&owner, vec![gate.clone(), network.connect(&["wss://two.example".to_string()])], &stop);
        // Relay two is read (after relay one) and has nothing; then the revoke comes to it live.
        assert!(wait_for(|| two.reqs().len() >= 2));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!revoked(&owner));
        assert!(two.publish(&revoke));
        assert!(wait_for(|| revoked(&owner)), "the live copy of the revoke was skipped");
        assert!(invites.live(now).is_empty());
        gate.open();
        assert!(wait_for(|| !owner.lock().paging), "relay one's window was never read whole");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(acks_to_2(&two), 1, "the revoke was answered once");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    /// Page reads go to the relay; a standing request (no `limit`) hears nothing
    /// at all, so only the paged reads can find what came while a read was cut.
    struct PagesOnly {
        inner: Arc<dyn Relay>,
        quiet: Mutex<Vec<(tokio::sync::mpsc::Sender<RelayMsg>, tokio::sync::watch::Sender<SubStatus>)>>,
    }

    impl Relay for PagesOnly {
        fn publish(&self, ev: &Event) {
            self.inner.publish(ev)
        }
        fn subscribe(&self, filters: Vec<Filter>) -> Subscription {
            if filters.iter().any(|f| f.limit.is_some()) {
                return self.inner.subscribe(filters);
            }
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let (status, status_rx) = tokio::sync::watch::channel(SubStatus::Up);
            self.quiet.lock().unwrap().push((tx, status));
            Subscription::new(rx, 1, None).with_status(status_rx)
        }
        fn urls(&self) -> Vec<String> {
            self.inner.urls()
        }
    }

    #[test]
    fn a_resumed_read_finds_what_was_sent_while_it_was_cut_even_dated_into_the_part_read_before() {
        let (owner, invites, network, urls, now) = claimed_owner("cut-arrivals");
        let owner = Arc::new(owner.with_pacing(quick_redial()));
        let hub = network.hub(&urls[0]);
        // A day old, so the first page's top is a day back.
        for wrap in junk(25, |i| now - DAY - i as u64) {
            assert!(hub.publish(&wrap));
        }
        let pages = Arc::new(PagesOnly { inner: network.connect(&urls), quiet: Mutex::new(Vec::new()) });
        let gate = gate_on(pages, hub.clone(), 2);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_relays(&owner, vec![gate.clone()], &stop);
        assert!(wait_for(|| hub.reqs().len() >= 2), "the first page was not read");
        // While the read is cut the owner sends an invite. Its wrap is dated (as NIP-59
        // allows) inside the first page, below that page's top: a resumed read that
        // only went on down from where it stopped, or only read above the first
        // page's top, would never see it.
        let late = wrap_at(&keys(2), KIND_INVITE, invite_tags(GRANT, 1, now + DAY, &["guest"]), now - DAY - 5);
        assert!(hub.publish(&late));
        gate.open();
        assert!(wait_for(|| !owner.lock().paging), "the window was never read whole");
        assert!(wait_for(|| !invites.live(now).is_empty()), "the invite sent while the read was cut was missed");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    #[test]
    fn a_stop_part_way_through_applying_the_backlog_leaves_the_saved_cursor_so_a_restart_applies_the_rest() {
        let dir = scratch("stop-mid-apply");
        let urls = vec!["wss://one.example".to_string()];
        let open = || {
            let invites = Arc::new(Invites::open(&dir, keys(1).public_key().to_bytes()).unwrap());
            Arc::new(Owner::open(&dir, keys(1), invites, "Home", &urls).unwrap())
        };
        let owner = open();
        let code = {
            let link = owner.pairing_link().unwrap();
            link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string()
        };
        // Claimed ten days ago, with a live grant, and off since.
        let now = unix_now();
        let then = now - 10 * DAY;
        let claim = wire::gift_wrap(&keys(2), &keys(1).public_key(), KIND_CLAIM, vec![keep_tag(), tag(&["code", &code])], then).unwrap();
        assert!(owner.process(&claim, then).is_some());
        let made = wrap_at(&keys(2), KIND_INVITE, invite_tags(GRANT, 1, now + DAY, &["guest"]), then + 60);
        assert!(owner.process(&made, then + 60).is_some());
        // The relay holds the owner's revoke, and an update made three days after it.
        let network = FakeNetwork::new();
        let hub = network.hub(&urls[0]);
        assert!(hub.publish(&wrap_at(&keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], now - 4 * DAY)));
        assert!(hub.publish(&wrap_at(&keys(2), KIND_UPDATE, update_tags("1", &[2]), now - DAY)));
        // The Keep stops once the update is applied: the revoke (pass 2) is not.
        let watched = owner.clone();
        owner.run_with(&[network.connect(&urls)], 10, STRANGER_CAP, &|| watched.lock().data.rev >= 1);
        assert!(!revoked(&owner), "the stop came before the revoke");
        drop(owner);
        drop(watched);
        // The next start still reads back to the revoke.
        let again = open();
        assert!(again.lock().since(now) < now - 4 * DAY, "the saved cursor moved past the unapplied revoke");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = run_in_thread(&again, network.connect(&urls), &stop);
        assert!(wait_for(|| revoked(&again)), "the revoke was lost");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        thread.join().unwrap();
    }

    #[test]
    fn a_cut_read_keeps_as_done_only_what_nothing_sent_since_can_be_dated_into() {
        let started = NOW;
        let floor = NOW - CURSOR_MARGIN_SECS;
        let band = |read: Read| match read {
            Read::Cut(rest) => rest.done,
            Read::Whole(_) => panic!("a cut read"),
        };
        // Stopped below the margin: from there up to the margin is done.
        assert_eq!(band(Resume::fresh(0).cut(Some(floor - 500), started, Vec::new())), Some((floor - 500, floor)));
        // Stopped inside the margin, or before the first page: nothing is.
        assert_eq!(band(Resume::fresh(0).cut(Some(floor + 1), started, Vec::new())), None);
        assert_eq!(band(Resume::fresh(0).cut(None, started, Vec::new())), None);
        // With a band done before: stopped above it, it stands; past it, it grows to this read's margin.
        let before = Resume { since: 0, done: Some((floor - 9000, floor - 5000)), held: Vec::new() };
        assert_eq!(band(before.cut(Some(floor - 4000), started + 60, Vec::new())), Some((floor - 9000, floor - 5000)));
        let before = Resume { since: 0, done: Some((floor - 9000, floor - 5000)), held: Vec::new() };
        assert_eq!(band(before.cut(Some(floor - 9500), started + 60, Vec::new())), Some((floor - 9500, floor + 60)));
        let before = Resume { since: 0, done: Some((floor - 9000, floor - 5000)), held: Vec::new() };
        assert_eq!(band(before.cut(None, started + 60, Vec::new())), Some((floor - 9000, floor - 5000)));
    }

    #[test]
    fn a_resumed_read_passes_over_what_was_done_and_reads_the_rest() {
        let (owner, _invites, network, urls, now) = claimed_owner("pass-over");
        let hub = network.hub(&urls[0]);
        let (lo, hi) = (now - 2 * DAY, now - 3600);
        // Five recent wraps, twenty inside the band, and the owner's revoke below it.
        for wrap in junk(5, |i| now - 10 - i as u64) {
            assert!(hub.publish(&wrap));
        }
        for wrap in junk(20, |i| now - DAY - i as u64) {
            assert!(hub.publish(&wrap));
        }
        assert!(hub.publish(&wrap_at(&keys(2), KIND_REVOKE, vec![keep_tag(), tag(&["grant", GRANT])], now - 2 * DAY - 500)));
        let relay = network.connect(&urls);
        let rest = Resume { since: now - 3 * DAY, done: Some((lo, hi)), held: Vec::new() };
        let Read::Whole(found) = owner.read_window(relay.as_ref(), rest, 10, STRANGER_CAP, &mut Seen::default(), &|| false) else { panic!("cut") };
        assert_eq!(found.len(), 1, "the revoke below the band was read");
        let untils: Vec<Option<u64>> = hub.reqs().iter().map(|r| r.filters[0].until.map(|t| t.as_secs())).collect();
        assert_eq!(untils[0], None, "the read starts at the top: what came since can be dated anywhere above the band");
        assert_eq!(untils[1], Some(lo), "after the top page the read goes on under the band");
    }

    #[test]
    fn the_wait_before_the_next_read_counts_from_the_end_of_the_last() {
        let pacing = Pacing { page_gap: Duration::ZERO, redial_base: Duration::from_secs(15), redial_max: Duration::from_secs(600), calm: Duration::from_secs(1200) };
        let t0 = Instant::now();
        let mut redial = Redial::new();
        redial.started(t0, &pacing);
        // A read of a flood that took 200 s: the relay is not read again at once.
        let end = t0 + Duration::from_secs(200);
        redial.finished(end, &pacing);
        assert!(!redial.ready(end + Duration::from_secs(14)) && redial.ready(end + Duration::from_secs(15)));
        // A short read does not shorten a wait that is already longer.
        let mut redial = Redial::new();
        redial.started(t0, &pacing);
        redial.started(t0 + Duration::from_secs(20), &pacing);
        redial.finished(t0 + Duration::from_secs(21), &pacing);
        assert!(!redial.ready(t0 + Duration::from_secs(49)) && redial.ready(t0 + Duration::from_secs(50)));
    }

    #[test]
    fn a_relays_words_are_printed_without_control_characters_and_short() {
        assert_eq!(clean("rate-limited: slow down"), "rate-limited: slow down");
        assert_eq!(clean("x\n[keep] owner invite from aaaaaa (live): active\r"), "x[keep] owner invite from aaaaaa (live): active");
        assert_eq!(clean(&"a".repeat(500)).len(), 60);
        assert_eq!(clean(&"\u{e9}".repeat(500)).chars().count(), 60);
    }
}
