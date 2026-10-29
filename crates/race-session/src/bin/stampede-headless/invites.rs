//! The Keep's invite table: who may come in, kept in `--data-dir/invites.json`.
//!
//! The Keep is the only issuer and the only verifier. An invite is a 32-byte
//! random secret (Fray finds the Keep by it and admits whoever proves it); the
//! rules on top (how long, how many uses, which persona) live here and are
//! applied to each joiner after Fray has admitted it. The rules are pure
//! ([`Table`]); [`Invites`] wraps them with the file and a lock so the status
//! page's threads and the room's loop can share one table.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nostr::nips::nip19::ToBech32;
use race_session::invite::{hex_encode, parse_hex32};
use race_session::{format_invite_link, InviteId};
use serde::{Deserialize, Serialize};

use crate::keep::write_private_atomic;

/// File name inside `--data-dir`.
pub const FILE: &str = "invites.json";
/// A guest pass lasts a day. It works for the first person who uses it.
pub const GUEST_TTL_SECS: u64 = 24 * 3600;
/// A personal invite lasts this many days unless told otherwise.
pub const PERSONAL_DEFAULT_DAYS: u32 = 30;
/// The longest a personal invite may last.
pub const MAX_DAYS: u32 = 365;
/// People remembered per invite.
pub const MAX_USED_BY: usize = 16;
/// Longest label, in characters.
pub const MAX_LABEL: usize = 40;
/// Entries kept in the table, live or not.
pub const MAX_ENTRIES: usize = 512;
/// How long after a join the seat's name may still be matched to it.
const NAME_WINDOW: Duration = Duration::from_secs(15);
const PENDING_NAMES: usize = 16;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Guest,
    Personal,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Guest => "guest",
            Kind::Personal => "personal",
        }
    }
}

/// Who made an invite: the status page (on the Keep's network) or the owner,
/// from My Signet.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Page,
    Owner,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Page => "page",
            Source::Owner => "owner",
        }
    }

    fn is_page(&self) -> bool {
        *self == Source::Page
    }
}

/// Someone an invite let in.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsedBy {
    /// Persona public key, hex.
    pub persona: String,
    /// The name they sat down under, when known.
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Invite {
    /// `InviteId` of the secret, hex.
    pub id: String,
    pub kind: Kind,
    /// The secret, hex: needed to add the invite again after a restart.
    pub secret: String,
    /// The persona public key (hex) that may use it. Personal: set when it is
    /// made. Guest: empty until the first person uses it, then that person.
    pub holder: Option<String>,
    pub label: Option<String>,
    pub created_at: u64,
    pub expires_at: u64,
    /// `None` = as often as they like.
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub last_used_at: Option<u64>,
    pub used_by: Vec<UsedBy>,
    pub revoked: bool,
    /// Absent in files from before owner invites: the page.
    #[serde(default, skip_serializing_if = "Source::is_page")]
    pub source: Source,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Live,
    Used,
    Expired,
    Revoked,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Live => "live",
            Status::Used => "used",
            Status::Expired => "expired",
            Status::Revoked => "revoked",
        }
    }
}

impl Invite {
    pub fn status(&self, now: u64) -> Status {
        if self.revoked {
            Status::Revoked
        } else if self.max_uses.is_some_and(|max| self.uses >= max) {
            Status::Used
        } else if now >= self.expires_at {
            Status::Expired
        } else {
            Status::Live
        }
    }

    fn invite_id(&self) -> Option<InviteId> {
        parse_id(&self.id)
    }

    fn secret_bytes(&self) -> Option<[u8; 32]> {
        parse_hex32(&self.secret).ok()
    }
}

/// Why a joiner holding an invite was turned away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    Unknown,
    Revoked,
    Expired,
    Used,
    SomeoneElse,
}

impl Refusal {
    /// For the log line (a kicked client is only told it was kicked).
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::Unknown => "This invite is not valid",
            Refusal::Revoked => "This invite was cancelled",
            Refusal::Expired => "This invite has expired",
            Refusal::Used => "This invite has been used",
            Refusal::SomeoneElse => "This invite is for someone else",
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Refusal::Unknown => "unknown",
            Refusal::Revoked => "revoked",
            Refusal::Expired => "expired",
            Refusal::Used => "used",
            Refusal::SomeoneElse => "someone_else",
        }
    }
}

/// An admission the rules accepted.
#[derive(Clone, Debug, PartialEq)]
pub struct Admitted {
    pub kind: Kind,
    pub label: Option<String>,
    pub uses: u32,
    /// A single-use invite that has now been used up.
    pub exhausted: bool,
}

pub fn parse_id(text: &str) -> Option<InviteId> {
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, pair) in text.as_bytes().chunks(2).enumerate() {
        id[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(InviteId(id))
}

/// The invites and the rules, with no file and no clock of their own.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub invites: Vec<Invite>,
}

impl Table {
    /// Apply the rules to a joiner that proved invite `id` with persona
    /// `persona` (hex), and count the use.
    pub fn admit(&mut self, id: InviteId, persona: &str, now: u64) -> Result<Admitted, Refusal> {
        let invite = self
            .invites
            .iter_mut()
            .find(|i| i.invite_id() == Some(id))
            .ok_or(Refusal::Unknown)?;
        match invite.status(now) {
            Status::Revoked => return Err(Refusal::Revoked),
            Status::Used => return Err(Refusal::Used),
            Status::Expired => return Err(Refusal::Expired),
            Status::Live => {}
        }
        match (invite.kind, invite.holder.as_deref()) {
            // A guest pass belongs to the first persona that uses it; from then
            // on it behaves like a personal invite for that persona.
            (Kind::Guest, None) => invite.holder = Some(persona.to_string()),
            (_, Some(holder)) if holder == persona => {}
            _ => return Err(Refusal::SomeoneElse),
        }
        invite.uses = invite.uses.saturating_add(1);
        invite.last_used_at = Some(now);
        if !invite.used_by.iter().any(|u| u.persona == persona) {
            if invite.used_by.len() >= MAX_USED_BY {
                invite.used_by.remove(0);
            }
            invite.used_by.push(UsedBy {
                persona: persona.to_string(),
                name: String::new(),
            });
        }
        Ok(Admitted {
            kind: invite.kind,
            label: invite.label.clone(),
            uses: invite.uses,
            exhausted: invite.max_uses.is_some_and(|max| invite.uses >= max),
        })
    }

    /// The invites Fray should hold: not revoked, not expired, uses left.
    pub fn live(&self, now: u64) -> Vec<(InviteId, [u8; 32])> {
        self.invites
            .iter()
            .filter(|i| i.status(now) == Status::Live)
            .filter_map(|i| Some((i.invite_id()?, i.secret_bytes()?)))
            .collect()
    }

    /// Whether [`Table::add`] would find room for one more at `now`.
    pub fn can_add(&self, now: u64) -> bool {
        let live = self.invites.iter().filter(|i| i.status(now) == Status::Live).count();
        live < decented_net::MAX_INVITES
            && (self.invites.len() < MAX_ENTRIES || self.invites.iter().any(|i| i.status(now) != Status::Live))
    }

    /// Add an invite, making room by dropping the oldest dead ones.
    pub fn add(&mut self, invite: Invite, now: u64) -> Result<(), String> {
        let live = self.invites.iter().filter(|i| i.status(now) == Status::Live).count();
        if live >= decented_net::MAX_INVITES {
            return Err(format!(
                "Too many live invites (at most {}); revoke one first",
                decented_net::MAX_INVITES
            ));
        }
        while self.invites.len() >= MAX_ENTRIES {
            let oldest_dead = self
                .invites
                .iter()
                .enumerate()
                .filter(|(_, i)| i.status(now) != Status::Live)
                .min_by_key(|(_, i)| i.created_at)
                .map(|(at, _)| at);
            match oldest_dead {
                Some(at) => {
                    self.invites.remove(at);
                }
                None => return Err("Too many invites".into()),
            }
        }
        self.invites.push(invite);
        Ok(())
    }

    /// Mark an invite revoked. `None` if there is no such invite.
    pub fn revoke(&mut self, id: InviteId) -> Option<bool> {
        let invite = self.invites.iter_mut().find(|i| i.invite_id() == Some(id))?;
        let changed = !invite.revoked;
        invite.revoked = true;
        Some(changed)
    }

    fn parse(text: &str) -> Result<Table, String> {
        let file: File = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        if file.version != 1 {
            return Err(format!("version {} is not understood", file.version));
        }
        let mut invites = file.invites;
        for invite in &mut invites {
            let secret = parse_hex32(&invite.secret).map_err(|e| format!("a secret is bad: {e}"))?;
            let id = InviteId::of(&secret);
            if parse_id(&invite.id) != Some(id) {
                return Err(format!("invite {} does not match its secret", invite.id));
            }
            match (invite.kind, &invite.holder) {
                (Kind::Personal, Some(holder)) => {
                    parse_hex32(holder).map_err(|e| format!("a holder is bad: {e}"))?;
                }
                (Kind::Personal, None) => return Err(format!("personal invite {} has no holder", invite.id)),
                (Kind::Guest, Some(holder)) => {
                parse_hex32(holder).map_err(|e| format!("a holder is bad: {e}"))?;
            }
            (Kind::Guest, None) => {}
            }
            // A pass from before guest passes belonged to their first user:
            // it was single-use. Its user becomes its holder.
            if invite.kind == Kind::Guest && invite.max_uses == Some(1) {
                if invite.uses == 0 {
                    invite.max_uses = None;
                } else if let Some(first) = invite.used_by.first() {
                    invite.holder = invite.holder.take().or_else(|| Some(first.persona.clone()));
                    invite.max_uses = None;
                }
            }
            let excess = invite.used_by.len().saturating_sub(MAX_USED_BY);
            invite.used_by.drain(..excess);
        }
        if invites.len() > MAX_ENTRIES {
            return Err("too many invites".into());
        }
        Ok(Table { invites })
    }

    fn render(&self) -> String {
        let file = File {
            version: 1,
            invites: self.invites.clone(),
        };
        serde_json::to_string_pretty(&file).unwrap_or_default() + "\n"
    }
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    invites: Vec<Invite>,
}

/// What Fray should add and remove to match `desired` after `held`.
pub fn plan(
    held: &BTreeSet<InviteId>,
    desired: &[(InviteId, [u8; 32])],
) -> (Vec<(InviteId, [u8; 32])>, Vec<InviteId>) {
    let want: BTreeSet<InviteId> = desired.iter().map(|(id, _)| *id).collect();
    let add = desired.iter().filter(|(id, _)| !held.contains(id)).copied().collect();
    let remove = held.iter().filter(|id| !want.contains(id)).copied().collect();
    (add, remove)
}

/// Clean text a person typed as a label: no control characters, trimmed, at
/// most [`MAX_LABEL`] characters. Empty is no label.
pub fn clean_label(text: &str) -> Option<String> {
    let label: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_LABEL)
        .collect();
    let label = label.trim().to_string();
    (!label.is_empty()).then_some(label)
}

/// A persona as typed: `npub1...` or 64 hex characters. Returns it as hex.
pub fn parse_persona(text: &str) -> Result<String, String> {
    let text = text.trim();
    if let Ok(key) = parse_hex32(text) {
        return Ok(hex_encode(&key));
    }
    if text.starts_with("npub1") {
        if let Ok(key) = nostr::PublicKey::parse(text) {
            return Ok(key.to_hex());
        }
    }
    Err("Use the person's npub (npub1...) or their 64-character hex key".into())
}

/// `npub1abcdefgh…` for a hex persona key; the hex itself if it will not parse.
pub fn short_npub(persona_hex: &str) -> String {
    let npub = parse_hex32(persona_hex)
        .ok()
        .and_then(|bytes| nostr::PublicKey::from_slice(&bytes).ok())
        .and_then(|key| key.to_bech32().ok());
    match npub {
        Some(npub) => format!("{}…", npub.chars().take(13).collect::<String>()),
        None => persona_hex.chars().take(12).collect(),
    }
}

/// An invite as the page and `/status.json` show it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InviteView {
    pub id: String,
    pub kind: &'static str,
    pub label: Option<String>,
    pub holder: Option<String>,
    pub holder_short: Option<String>,
    pub created_at: u64,
    pub expires_at: u64,
    /// Seconds from now (negative once past).
    pub expires_in: i64,
    pub uses: u32,
    pub max_uses: Option<u32>,
    pub last_used_at: Option<u64>,
    pub used_by: Vec<UsedBy>,
    /// `page` or `owner`.
    pub source: &'static str,
    pub status: &'static str,
    /// `unused` until someone has been let in by it, then `used`. A guest pass
    /// that is `used` belongs to that person (`holder`) and still works for them.
    pub usage: &'static str,
    /// The link, for a live invite only.
    pub link: Option<String>,
}

/// Why [`Invites::add_owner`] added nothing.
#[derive(Debug, PartialEq)]
pub enum AddOwnerError {
    /// The table is full (live invites or entries).
    Full,
    /// The file could not be written.
    Write(String),
}

/// A new invite, handed back once to be shown.
#[derive(Clone, Debug, PartialEq)]
pub struct Created {
    pub id: String,
    pub kind: Kind,
    pub link: String,
    pub expires_at: u64,
    pub label: Option<String>,
    pub holder_short: Option<String>,
}

struct Inner {
    table: Table,
    /// Revoked since the room's loop last looked: their players are to be dropped.
    revoked: Vec<InviteId>,
    /// Recent admissions whose seat name is not known yet: (invite, persona, when).
    pending: VecDeque<(String, String, Instant)>,
}

/// The table, its file and the lock the page's threads and the room's loop share.
pub struct Invites {
    inner: Mutex<Inner>,
    path: PathBuf,
    host_key: [u8; 32],
    /// The table changed: the room's loop should make Fray match it.
    dirty: AtomicBool,
}

impl Invites {
    /// Read `dir/invites.json` (none yet is an empty table) for the Keep with
    /// runtime key `host_key`. A file that cannot be read is an error, not
    /// an empty table: replacing it would cancel every invite quietly. The
    /// caller runs the Keep without invites and never writes to the file.
    /// The error names the file (not the directory) and why.
    pub fn open(dir: &std::path::Path, host_key: [u8; 32]) -> Result<Invites, String> {
        let path = dir.join(FILE);
        let table = match std::fs::read_to_string(&path) {
            Ok(text) => Table::parse(&text).map_err(|e| format!("{FILE} couldn't be read: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Table::default(),
            Err(e) => return Err(format!("{FILE} couldn't be read: {e}")),
        };
        Ok(Invites {
            inner: Mutex::new(Inner {
                table,
                revoked: Vec::new(),
                pending: VecDeque::new(),
            }),
            path,
            host_key,
            dirty: AtomicBool::new(true),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Change the table and write it; if it cannot be written the change is undone.
    fn commit<T>(&self, inner: &mut Inner, change: impl FnOnce(&mut Table) -> Result<T, String>) -> Result<T, String> {
        let before = inner.table.clone();
        let done = change(&mut inner.table);
        if done.is_ok() {
            if let Err(e) = write_private_atomic(&self.path, inner.table.render().as_bytes()) {
                inner.table = before;
                return Err(e);
            }
            self.dirty.store(true, Ordering::Release);
        } else {
            inner.table = before;
        }
        done
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        kind: Kind,
        holder: Option<String>,
        label: Option<String>,
        lifetime_secs: u64,
        max_uses: Option<u32>,
        secret: [u8; 32],
        now: u64,
    ) -> Result<Created, String> {
        let id = InviteId::of(&secret).to_string();
        let invite = Invite {
            id: id.clone(),
            kind,
            secret: hex_encode(&secret),
            holder: holder.clone(),
            label: label.clone(),
            created_at: now,
            expires_at: now.saturating_add(lifetime_secs),
            max_uses,
            uses: 0,
            last_used_at: None,
            used_by: Vec::new(),
            revoked: false,
            source: Source::default(),
        };
        let expires_at = invite.expires_at;
        let mut inner = self.lock();
        self.commit(&mut inner, |table| table.add(invite, now))?;
        Ok(Created {
            id,
            kind,
            link: format_invite_link(&self.host_key, &secret),
            expires_at,
            label,
            holder_short: holder.as_deref().map(short_npub),
        })
    }

    /// Add an invite the owner made in My Signet: the secret and the absolute
    /// expiry are theirs. `Ok(true)` if it was added, `Ok(false)` if the table
    /// already holds that secret (nothing changes). The error is
    /// [`AddOwnerError::Full`] when there is no room (the same limits as the
    /// page), or the reason the file could not be written.
    pub fn add_owner(
        &self,
        secret: [u8; 32],
        holder: Option<String>,
        label: Option<String>,
        expires_at: u64,
        now: u64,
    ) -> Result<bool, AddOwnerError> {
        let id = InviteId::of(&secret);
        let mut inner = self.lock();
        if inner.table.invites.iter().any(|i| i.invite_id() == Some(id)) {
            return Ok(false);
        }
        if !inner.table.can_add(now) {
            return Err(AddOwnerError::Full);
        }
        let invite = Invite {
            id: id.to_string(),
            kind: if holder.is_some() { Kind::Personal } else { Kind::Guest },
            secret: hex_encode(&secret),
            holder,
            label,
            created_at: now,
            expires_at,
            max_uses: None,
            uses: 0,
            last_used_at: None,
            used_by: Vec::new(),
            revoked: false,
            source: Source::Owner,
        };
        self.commit(&mut inner, |table| table.add(invite, now)).map_err(AddOwnerError::Write)?;
        Ok(true)
    }

    /// Whether the table holds the invite made from `secret`.
    pub fn contains(&self, id: InviteId) -> bool {
        self.lock().table.invites.iter().any(|i| i.invite_id() == Some(id))
    }

    /// A pass that lasts a day and belongs to the first person who uses it.
    pub fn create_guest(&self, label: Option<String>, now: u64) -> Result<Created, String> {
        self.create(Kind::Guest, None, label, GUEST_TTL_SECS, None, random_secret()?, now)
    }

    /// An invite only `holder` (persona hex) can use, as often as they like.
    pub fn create_personal(
        &self,
        holder: String,
        label: Option<String>,
        days: u32,
        now: u64,
    ) -> Result<Created, String> {
        self.create(
            Kind::Personal,
            Some(holder),
            label,
            u64::from(days) * 86_400,
            None,
            random_secret()?,
            now,
        )
    }

    #[cfg(test)]
    fn create_guest_with(&self, secret: [u8; 32], now: u64) -> Created {
        self.create(Kind::Guest, None, None, GUEST_TTL_SECS, None, secret, now).unwrap()
    }

    /// Cancel an invite. `Ok(false)` if there is no such invite.
    pub fn revoke(&self, id: &str) -> Result<bool, String> {
        let Some(id) = parse_id(id) else { return Ok(false) };
        let mut inner = self.lock();
        let found = self.commit(&mut inner, |table| Ok(table.revoke(id)))?;
        if found.is_some() {
            inner.revoked.push(id);
        }
        Ok(found.is_some())
    }

    /// Cancel an invite so that it holds for this run whatever the disk says:
    /// if the file cannot be written the invite is still marked revoked in
    /// memory (Fray drops it and its players are kicked) and the error comes
    /// back so the caller does not claim it is saved. A later write (this
    /// call again, or any other change to the table) saves it.
    /// `Ok(false)` if there is no such invite.
    pub fn revoke_fail_safe(&self, id: &str) -> Result<bool, String> {
        let Some(id) = parse_id(id) else { return Ok(false) };
        let mut inner = self.lock();
        let saved = self.commit(&mut inner, |table| Ok(table.revoke(id)));
        let found = match saved {
            Ok(found) => found,
            Err(e) => {
                // `commit` put the table back as it was; revoke in memory.
                let found = inner.table.revoke(id);
                if found.is_some() {
                    inner.revoked.push(id);
                    self.dirty.store(true, Ordering::Release);
                }
                return Err(e);
            }
        };
        if found.is_some() {
            inner.revoked.push(id);
        }
        Ok(found.is_some())
    }

    /// Where the table's row for `id` stands at `now`; `None` if it has none.
    pub fn status_of(&self, id: InviteId, now: u64) -> Option<Status> {
        self.lock().table.invites.iter().find(|i| i.invite_id() == Some(id)).map(|i| i.status(now))
    }

    /// Apply the rules to a joiner and, if let in, count and write the use.
    /// A use that cannot be written is refused (it could be spent again after
    /// a restart).
    pub fn admit(&self, id: InviteId, persona: &[u8; 32], now: u64) -> Result<Admitted, Refusal> {
        let persona = hex_encode(persona);
        let mut inner = self.lock();
        let mut refusal = None;
        let done = self.commit(&mut inner, |table| match table.admit(id, &persona, now) {
            Ok(admitted) => Ok(admitted),
            Err(why) => {
                refusal = Some(why);
                Err(String::new())
            }
        });
        match done {
            Ok(admitted) => {
                if inner.pending.len() >= PENDING_NAMES {
                    inner.pending.pop_front();
                }
                inner.pending.push_back((id.to_string(), persona, Instant::now()));
                Ok(admitted)
            }
            Err(e) => {
                if refusal.is_none() {
                    log::warn!("[keep] {e}");
                }
                Err(refusal.unwrap_or(Refusal::Unknown))
            }
        }
    }

    /// A new seat appeared under `name`: give it to the oldest recent admission.
    pub fn claim_name(&self, name: &str) {
        let mut inner = self.lock();
        while let Some((_, _, at)) = inner.pending.front() {
            if at.elapsed() > NAME_WINDOW {
                inner.pending.pop_front();
            } else {
                break;
            }
        }
        let Some((id, persona, _)) = inner.pending.pop_front() else { return };
        let name = clean_label(name).unwrap_or_default();
        let _ = self.commit(&mut inner, |table| {
            let invite = table
                .invites
                .iter_mut()
                .find(|i| i.id == id)
                .ok_or_else(|| "gone".to_string())?;
            let entry = invite
                .used_by
                .iter_mut()
                .find(|u| u.persona == persona)
                .ok_or_else(|| "gone".to_string())?;
            if entry.name == name {
                return Err("same".into());
            }
            entry.name = name;
            Ok(())
        });
    }

    /// True once per change to the table.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Ask for the next look at the table even if nothing changed.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub fn live(&self, now: u64) -> Vec<(InviteId, [u8; 32])> {
        self.lock().table.live(now)
    }

    /// The invites revoked since the last call.
    pub fn take_revoked(&self) -> Vec<InviteId> {
        std::mem::take(&mut self.lock().revoked)
    }

    /// Newest first.
    pub fn views(&self, now: u64) -> Vec<InviteView> {
        let inner = self.lock();
        let mut views: Vec<InviteView> = inner
            .table
            .invites
            .iter()
            .map(|i| {
                let status = i.status(now);
                InviteView {
                    id: i.id.clone(),
                    kind: i.kind.as_str(),
                    label: i.label.clone(),
                    holder: i.holder.clone(),
                    holder_short: i.holder.as_deref().map(short_npub),
                    created_at: i.created_at,
                    expires_at: i.expires_at,
                    expires_in: i.expires_at as i64 - now as i64,
                    uses: i.uses,
                    max_uses: i.max_uses,
                    last_used_at: i.last_used_at,
                    used_by: i.used_by.clone(),
                    source: i.source.as_str(),
                    status: status.as_str(),
                    usage: if i.uses == 0 { "unused" } else { "used" },
                    // An owner's invite link goes from My Signet to its friend;
                    // this page is open to the whole network and never shows it.
                    link: (status == Status::Live && i.source == Source::Page)
                        .then(|| i.secret_bytes().map(|s| format_invite_link(&self.host_key, &s)))
                        .flatten(),
                }
            })
            .collect();
        views.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        views
    }
}

fn random_secret() -> Result<[u8; 32], String> {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| format!("No randomness for a secret: {e}"))?;
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const HOST: [u8; 32] = [7; 32];

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("invites-{name}-{}-{NOW}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn persona(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn guest(table: &mut Table, secret: u8) -> InviteId {
        let secret = [secret; 32];
        let id = InviteId::of(&secret);
        table
            .add(
                Invite {
                    id: id.to_string(),
                    kind: Kind::Guest,
                    secret: hex_encode(&secret),
                    holder: None,
                    label: None,
                    created_at: NOW,
                    expires_at: NOW + GUEST_TTL_SECS,
                    max_uses: None,
                    uses: 0,
                    last_used_at: None,
                    used_by: vec![],
                    revoked: false,
                    source: Source::default(),
                },
                NOW,
            )
            .unwrap();
        id
    }

    fn personal(table: &mut Table, secret: u8, holder: u8, days: u64) -> InviteId {
        let secret = [secret; 32];
        let id = InviteId::of(&secret);
        table
            .add(
                Invite {
                    id: id.to_string(),
                    kind: Kind::Personal,
                    secret: hex_encode(&secret),
                    holder: Some(hex_encode(&persona(holder))),
                    label: Some("Ada".into()),
                    created_at: NOW,
                    expires_at: NOW + days * 86_400,
                    max_uses: None,
                    uses: 0,
                    last_used_at: None,
                    used_by: vec![],
                    revoked: false,
                    source: Source::default(),
                },
                NOW,
            )
            .unwrap();
        id
    }

    fn hex_of(n: u8) -> String {
        hex_encode(&persona(n))
    }

    #[test]
    fn a_guest_pass_belongs_to_the_first_persona_who_uses_it() {
        let mut table = Table::default();
        let id = guest(&mut table, 1);
        assert_eq!(table.invites[0].holder, None);
        let first = table.admit(id, &hex_of(9), NOW + 5).unwrap();
        assert_eq!((first.kind, first.uses, first.exhausted), (Kind::Guest, 1, false));
        assert_eq!(table.invites[0].holder, Some(hex_of(9)));
        // It is still held for the room, and the same persona may rejoin any number of times.
        assert_eq!(table.live(NOW + 6).len(), 1);
        for visit in 2..=30u32 {
            assert_eq!(table.admit(id, &hex_of(9), NOW + 5 + u64::from(visit)).unwrap().uses, visit);
        }
        // Anyone else is turned away, and that is not a use.
        assert_eq!(table.admit(id, &hex_of(8), NOW + 100), Err(Refusal::SomeoneElse));
        let invite = &table.invites[0];
        assert_eq!((invite.uses, invite.last_used_at), (30, Some(NOW + 35)));
        assert_eq!(invite.used_by, vec![UsedBy { persona: hex_of(9), name: String::new() }]);
        assert_eq!(invite.status(NOW + 100), Status::Live);
        // Until it expires or is revoked.
        assert_eq!(table.admit(id, &hex_of(9), NOW + GUEST_TTL_SECS), Err(Refusal::Expired));
        assert_eq!(table.revoke(id), Some(true));
        assert_eq!(table.admit(id, &hex_of(9), NOW + 100), Err(Refusal::Revoked));
    }

    #[test]
    fn a_refused_first_use_does_not_claim_a_guest_pass() {
        let mut table = Table::default();
        let id = guest(&mut table, 1);
        let end = NOW + GUEST_TTL_SECS;
        assert_eq!(table.admit(id, &hex_of(9), end), Err(Refusal::Expired));
        assert_eq!(table.invites[0].holder, None);
        assert!(table.admit(id, &hex_of(8), end - 1).is_ok());
        assert_eq!(table.invites[0].holder, Some(hex_of(8)));
    }

    #[test]
    fn a_guest_pass_from_before_belongs_to_its_user() {
        let old_unused = |max: Option<u32>, uses: u32, used_by: Vec<UsedBy>| {
            let secret = [1u8; 32];
            let invite = Invite {
                id: InviteId::of(&secret).to_string(),
                kind: Kind::Guest,
                secret: hex_encode(&secret),
                holder: None,
                label: None,
                created_at: NOW,
                expires_at: NOW + GUEST_TTL_SECS,
                max_uses: max,
                uses,
                last_used_at: None,
                used_by,
                revoked: false,
                source: Source::default(),
            };
            Table::parse(&Table { invites: vec![invite] }.render()).unwrap().invites.remove(0)
        };
        let unused = old_unused(Some(1), 0, vec![]);
        assert_eq!((unused.max_uses, unused.holder.clone(), unused.status(NOW)), (None, None, Status::Live));
        let used = old_unused(Some(1), 1, vec![UsedBy { persona: hex_of(9), name: "Bo".into() }]);
        assert_eq!((used.max_uses, used.holder.clone(), used.status(NOW)), (None, Some(hex_of(9)), Status::Live));
        let mut table = Table { invites: vec![used.clone()] };
        let id = used.invite_id().unwrap();
        assert!(table.admit(id, &hex_of(9), NOW).is_ok());
        assert_eq!(table.admit(id, &hex_of(8), NOW), Err(Refusal::SomeoneElse));
    }

    #[test]
    fn an_invite_expires_and_the_edge_is_exact() {
        let mut table = Table::default();
        let id = guest(&mut table, 1);
        let end = NOW + GUEST_TTL_SECS;
        assert_eq!(table.live(end - 1).len(), 1);
        assert!(table.live(end).is_empty());
        assert_eq!(table.admit(id, &hex_of(9), end), Err(Refusal::Expired));
        assert_eq!(table.invites[0].uses, 0);
        assert!(table.admit(id, &hex_of(9), end - 1).is_ok());
    }

    #[test]
    fn a_personal_invite_is_for_its_holder_as_often_as_they_like() {
        let mut table = Table::default();
        let id = personal(&mut table, 2, 5, 30);
        assert_eq!(table.admit(id, &hex_of(6), NOW), Err(Refusal::SomeoneElse));
        assert_eq!(table.invites[0].uses, 0, "a refusal is not a use");
        for visit in 1..=40u32 {
            let admitted = table.admit(id, &hex_of(5), NOW + u64::from(visit)).unwrap();
            assert_eq!((admitted.uses, admitted.exhausted), (visit, false));
        }
        assert_eq!(table.live(NOW + 100).len(), 1);
        assert_eq!(table.invites[0].used_by.len(), 1, "the same person is listed once");
        assert_eq!(table.admit(id, &hex_of(5), NOW + 30 * 86_400), Err(Refusal::Expired));
    }

    #[test]
    fn a_revoked_or_unknown_invite_admits_nobody() {
        let mut table = Table::default();
        let id = personal(&mut table, 2, 5, 30);
        assert_eq!(table.admit(InviteId([1; 16]), &hex_of(5), NOW), Err(Refusal::Unknown));
        assert_eq!(table.revoke(id), Some(true));
        assert_eq!(table.revoke(id), Some(false));
        assert_eq!(table.revoke(InviteId([1; 16])), None);
        assert!(table.live(NOW).is_empty());
        assert_eq!(table.admit(id, &hex_of(5), NOW), Err(Refusal::Revoked));
        assert_eq!(table.invites[0].status(NOW), Status::Revoked);
    }

    #[test]
    fn who_used_it_keeps_the_last_sixteen_of_a_longer_list_on_load() {
        let mut table = Table::default();
        guest(&mut table, 1);
        table.invites[0].used_by = (0..20u8)
            .map(|n| UsedBy { persona: hex_of(n), name: String::new() })
            .collect();
        let loaded = Table::parse(&table.render()).unwrap();
        let who: Vec<_> = loaded.invites[0].used_by.iter().map(|u| u.persona.clone()).collect();
        assert_eq!(who.len(), MAX_USED_BY);
        assert_eq!((who[0].as_str(), who[15].as_str()), (hex_of(4).as_str(), hex_of(19).as_str()));
    }

    #[test]
    fn the_table_makes_room_by_dropping_old_dead_invites_and_caps_live_ones() {
        let mut table = Table::default();
        let live_cap = decented_net::MAX_INVITES;
        for n in 0..live_cap {
            let secret = [(n % 250) as u8, (n / 250) as u8, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1];
            let id = InviteId::of(&secret);
            table
                .add(
                    Invite {
                        id: id.to_string(),
                        kind: Kind::Guest,
                        secret: hex_encode(&secret),
                        holder: None,
                        label: None,
                        created_at: NOW + n as u64,
                        expires_at: NOW + GUEST_TTL_SECS,
                        max_uses: None,
                        uses: 0,
                        last_used_at: None,
                        used_by: vec![],
                        revoked: false,
                        source: Source::default(),
                    },
                    NOW,
                )
                .unwrap();
        }
        let extra = table.invites[0].clone();
        assert!(table.add(extra.clone(), NOW).unwrap_err().contains("Too many live invites"));
        // After a day they are all dead; the table never grows past its cap.
        let later = NOW + GUEST_TTL_SECS + 1;
        for n in 0..MAX_ENTRIES + 10 {
            let mut invite = extra.clone();
            invite.id = format!("{n:032x}");
            invite.created_at = NOW + n as u64;
            table.add(invite, later).unwrap();
        }
        assert_eq!(table.invites.len(), MAX_ENTRIES);
        assert!(table.invites.iter().all(|i| i.created_at >= NOW));
    }

    #[test]
    fn what_fray_holds_follows_the_table() {
        let a = InviteId([1; 16]);
        let b = InviteId([2; 16]);
        let c = InviteId([3; 16]);
        let held: BTreeSet<_> = [a, b].into();
        let desired = [(b, [2; 32]), (c, [3; 32])];
        let (add, remove) = plan(&held, &desired);
        assert_eq!((add, remove), (vec![(c, [3; 32])], vec![a]));
        assert_eq!(plan(&BTreeSet::new(), &[]), (vec![], vec![]));
    }

    #[test]
    fn labels_and_personas_are_cleaned() {
        assert_eq!(clean_label("  Cousin <b>Bo</b>\n\t"), Some("Cousin <b>Bo</b>".into()));
        assert_eq!(clean_label(" \n "), None);
        assert_eq!(clean_label(&"é".repeat(100)).unwrap().chars().count(), MAX_LABEL);
        let key = nostr::Keys::generate().public_key();
        let npub = key.to_bech32().unwrap();
        assert_eq!(parse_persona(&npub), Ok(key.to_hex()));
        assert_eq!(parse_persona(&format!("  {}  ", key.to_hex().to_uppercase())), Ok(key.to_hex()));
        for bad in ["", "npub1xyz", "abc", &key.to_hex()[..63], "nsec1abc"] {
            assert!(parse_persona(bad).is_err(), "{bad}");
        }
        assert!(short_npub(&key.to_hex()).starts_with("npub1"));
        assert_eq!(short_npub("zz"), "zz");
    }

    #[test]
    fn the_table_survives_a_restart() {
        let dir = scratch("persist");
        let store = Invites::open(&dir, HOST).unwrap();
        let guest = store.create_guest_with([3; 32], NOW);
        let personal = store
            .create_personal(hex_of(5), Some("Ada".into()), 30, NOW)
            .unwrap();
        assert_eq!(guest.link, format_invite_link(&HOST, &[3; 32]));
        let id = InviteId::of(&[3; 32]);
        store.admit(id, &persona(9), NOW + 1).unwrap();
        store.claim_name("Bo");
        assert!(store.revoke(&personal.id).unwrap());
        drop(store);

        let again = Invites::open(&dir, HOST).unwrap();
        let views = again.views(NOW + 2);
        assert_eq!(views.len(), 2);
        let guest_view = views.iter().find(|v| v.kind == "guest").unwrap();
        assert_eq!((guest_view.uses, guest_view.status, guest_view.usage), (1, "live", "used"));
        assert_eq!(guest_view.holder.as_deref(), Some(hex_of(9).as_str()));
        assert_eq!(guest_view.used_by, vec![UsedBy { persona: hex_of(9), name: "Bo".into() }]);
        assert_eq!(guest_view.link.as_deref(), Some(guest.link.as_str()));
        let personal_view = views.iter().find(|v| v.kind == "personal").unwrap();
        assert_eq!(personal_view.status, "revoked");
        assert_eq!(personal_view.label.as_deref(), Some("Ada"));
        assert_eq!(personal_view.holder.as_deref(), Some(hex_of(5).as_str()));
        // The used guest pass is still live, held for its person.
        assert_eq!(again.live(NOW + 2), vec![(InviteId::of(&[3; 32]), [3; 32])]);
        assert_eq!(again.admit(InviteId::of(&[3; 32]), &persona(8), NOW + 2), Err(Refusal::SomeoneElse));
        assert!(again.admit(InviteId::of(&[3; 32]), &persona(9), NOW + 2).is_ok());
        // A live one comes back with its secret so Fray can hold it again.
        let fresh = again.create_guest_with([4; 32], NOW + 3);
        drop(again);
        let third = Invites::open(&dir, HOST).unwrap();
        let live: Vec<_> = third.live(NOW + 4).into_iter().map(|(id, _)| id).collect();
        assert_eq!(live.len(), 2);
        assert!(live.contains(&InviteId::of(&[4; 32])));
        let fresh_view = third.views(NOW + 4).into_iter().find(|v| v.usage == "unused").unwrap();
        assert_eq!(fresh_view.link.as_deref(), Some(fresh.link.as_str()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(leftovers, vec![FILE.to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_is_an_error_and_is_left_alone() {
        let dir = scratch("damaged");
        let path = dir.join(FILE);
        for text in [
            "not json",
            r#"{"version":2,"invites":[]}"#,
            r#"{"version":1,"invites":[{"id":"00000000000000000000000000000000","kind":"guest","secret":"0101010101010101010101010101010101010101010101010101010101010101","holder":null,"label":null,"created_at":1,"expires_at":2,"max_uses":1,"uses":0,"last_used_at":null,"used_by":[],"revoked":false}]}"#,
        ] {
            std::fs::write(&path, text).unwrap();
            let error = Invites::open(&dir, HOST).err().expect(text);
            assert!(error.starts_with("invites.json couldn't be read: "), "{error}");
            assert!(!error.contains(dir.to_str().unwrap()), "names the file, not the directory: {error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_use_that_cannot_be_written_is_refused_and_not_counted() {
        let dir = scratch("unwritable");
        let store = Invites::open(&dir, HOST).unwrap();
        store.create_guest_with([3; 32], NOW);
        // The directory goes away: the next write fails.
        std::fs::remove_dir_all(&dir).unwrap();
        let id = InviteId::of(&[3; 32]);
        assert!(store.admit(id, &persona(9), NOW + 1).is_err());
        assert!(store.create_guest(None, NOW).is_err());
        let view = &store.views(NOW + 2)[0];
        assert_eq!((view.uses, view.status, view.holder.as_deref()), (0, "live", None));
    }

    #[test]
    fn creating_and_revoking_ask_the_room_to_look() {
        let dir = scratch("dirty");
        let store = Invites::open(&dir, HOST).unwrap();
        assert!(store.take_dirty(), "a fresh open asks for the first look");
        assert!(!store.take_dirty());
        let made = store.create_guest(Some("x".into()), NOW).unwrap();
        assert!(store.take_dirty());
        assert!(store.revoke("nothing").unwrap() == false);
        assert!(!store.take_dirty());
        assert!(store.revoke(&made.id).unwrap());
        assert!(store.take_dirty());
        assert_eq!(store.take_revoked(), vec![parse_id(&made.id).unwrap()]);
        assert!(store.take_revoked().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_seat_name_goes_to_the_oldest_recent_admission() {
        let dir = scratch("names");
        let store = Invites::open(&dir, HOST).unwrap();
        store.create_guest_with([3; 32], NOW);
        store.create_guest_with([4; 32], NOW);
        store.admit(InviteId::of(&[3; 32]), &persona(1), NOW).unwrap();
        store.admit(InviteId::of(&[4; 32]), &persona(2), NOW).unwrap();
        store.claim_name("First");
        store.claim_name("Second");
        store.claim_name("Nobody");
        let views = store.views(NOW);
        let name_of = |p: u8| {
            views
                .iter()
                .flat_map(|v| v.used_by.iter())
                .find(|u| u.persona == hex_of(p))
                .unwrap()
                .name
                .clone()
        };
        assert_eq!((name_of(1).as_str(), name_of(2).as_str()), ("First", "Second"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
