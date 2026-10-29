//! The Keep's status page: a minimal HTTP/1.1 server for the home network.
//!
//! `GET /` is a small page (the room code, who is in, whether the room is
//! online), `GET /status.json` the same data for scripts, and
//! `POST /new-code` asks the room to host again under a new code. With
//! `--data-dir` it also manages invites: `GET /invites` (the forms),
//! `POST /invites/guest`, `POST /invites/personal` and
//! `POST /invites/<id>/revoke`. Everything else is 404. The server reads a
//! request's head (at most 8 KB, within a few seconds) and, for a POST, a
//! form body of at most 4 KB, and never panics on what it reads.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::invites::{self, Created, InviteView, Invites};
use crate::keep::unix_now;
use crate::owner::Owner;

/// The most a request head may take; a bigger one is refused.
pub const MAX_HEAD: usize = 8 * 1024;
/// How long a client has to send its head.
const HEAD_DEADLINE: Duration = Duration::from_secs(3);
/// How long a client has to take the answer.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Connections served at once; more are dropped.
const MAX_CONNECTIONS: usize = 16;
/// The most a form body may take; a bigger one is refused.
pub const MAX_BODY: usize = 4 * 1024;

/// What the page and `/status.json` show, refreshed by the room's loop.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The room code, `None` while a room is starting.
    pub code: Option<String>,
    /// `--online`: the code is for phones anywhere.
    pub online: bool,
    /// Online: a relay has been up long enough to share the code.
    pub code_ready: bool,
    /// A new room is being hosted.
    pub restarting: bool,
    /// Why the room could not be hosted again, while it is trying.
    pub error: Option<String>,
    /// `lobby`, `racing` or `results`.
    pub phase: String,
    /// The room takes joiners now (in the lobby with a free seat).
    pub open: bool,
    pub players: Vec<String>,
    pub races: u64,
    /// Relays `(connected, configured)`.
    pub relays: (usize, usize),
    /// `(kind, address)` the room announces.
    pub addresses: Vec<(String, String)>,
    /// The invites, newest first; `None` when the Keep has no `--data-dir`.
    pub invites: Option<Vec<InviteView>>,
    /// Why the Keep runs without invites although it has a `--data-dir`: a
    /// file there could not be read. Shown in red; `invites` is `None`.
    pub invites_problem: Option<String>,
    /// Unix seconds when `invites` were read.
    pub now: u64,
    /// The Keep has an owner (made in My Signet): the page makes no invites.
    /// Also set when the owner file cannot be read, as the safe reading.
    pub owner_claimed: bool,
    /// Why the owner file could not be read.
    pub owner_problem: Option<String>,
    /// The Keep can be claimed (online, with a data dir) and has not been.
    pub owner_open: bool,
    /// The pairing link: only for a request from this machine, and only while unclaimed.
    pub pairing: Option<String>,
}

impl Snapshot {
    fn state(&self) -> &'static str {
        if self.restarting {
            "starting"
        } else if !self.online {
            "nearby"
        } else if self.code_ready {
            "online"
        } else {
            "waiting_for_relays"
        }
    }
}

/// What the status server and the room's loop share.
pub struct Shared {
    snapshot: Mutex<Snapshot>,
    new_code: AtomicBool,
    /// The invite table, when the Keep keeps one.
    invites: Option<Arc<Invites>>,
    /// Why there is no invite table although there is a `--data-dir`.
    invites_problem: Option<String>,
    /// A name other than an IP address or `localhost` the page may be reached
    /// by (`--status-host`), lower case.
    host_name: Option<String>,
    /// Who owns the Keep, when owner claims are on.
    owner: Option<Arc<Owner>>,
    /// Why the owner files could not be read (the Keep then counts as claimed).
    owner_problem: Option<String>,
}

impl Shared {
    pub fn new(snapshot: Snapshot, host_name: Option<&str>) -> Self {
        Shared {
            snapshot: Mutex::new(snapshot),
            new_code: AtomicBool::new(false),
            invites: None,
            invites_problem: None,
            host_name: host_name.map(str::to_ascii_lowercase),
            owner: None,
            owner_problem: None,
        }
    }

    /// Show the pairing link while unclaimed, and make no invites once claimed.
    pub fn with_owner(mut self, owner: Arc<Owner>) -> Self {
        self.owner = Some(owner);
        self
    }

    /// The owner files could not be read: the page makes no invites, and says why.
    pub fn with_owner_problem(mut self, problem: String) -> Self {
        self.owner_problem = Some(problem);
        self
    }

    /// Serve the invite routes from this table.
    pub fn with_invites(mut self, invites: Arc<Invites>) -> Self {
        self.invites = Some(invites);
        self
    }

    /// Say on the page that invites are off, and why.
    pub fn with_invites_problem(mut self, problem: String) -> Self {
        self.invites_problem = Some(problem);
        self
    }

    /// The Keep has an owner (or its owner files cannot be read): the page makes
    /// and cancels no invites. Asked afresh at each change, not taken from a
    /// request read earlier, so a claim that lands between is not missed.
    fn owner_claimed(&self) -> bool {
        self.owner_problem.is_some() || self.owner.as_ref().is_some_and(|owner| owner.claimed())
    }

    fn lock(&self) -> MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().clone()
    }

    /// The room's loop publishes what it sees. While a new code is asked for
    /// and not yet taken, the page keeps showing "starting": what the loop saw
    /// before it took the request must not bring the old code back.
    pub fn set(&self, snapshot: Snapshot) {
        let mut current = self.lock();
        if self.new_code.load(Ordering::Acquire) {
            return;
        }
        *current = snapshot;
    }

    /// True once per request for a new code.
    pub fn take_new_code(&self) -> bool {
        self.new_code.swap(false, Ordering::AcqRel)
    }
}

/// What a request asks the room to do.
#[derive(Debug, PartialEq)]
pub enum Action {
    NewCode,
    NewGuest { label: Option<String> },
    /// `holder` is the persona's hex key.
    NewPersonal { holder: String, label: Option<String>, days: u32 },
    /// `id` is 32 hex characters; `back` is where the page goes next.
    Revoke { id: String, back: &'static str },
}

/// Do what a request asked. A new code shows as starting at once, so the
/// page that follows the redirect does not show the old code; asking again
/// while a room is starting changes nothing. A request that makes an invite
/// answers 303 to the page that shows its link (`GET /invites/new/<id>`), so a
/// refresh of that page does not make another.
pub fn apply(action: Action, shared: &Shared) -> Option<Response> {
    let table = shared.invites.as_ref();
    let now = unix_now();
    // `respond` refused these once the Keep was claimed, but a claim can land
    // between that and here.
    if !matches!(action, Action::NewCode) && shared.owner_claimed() {
        return Some(error_page(403, "This Keep's invites are managed in My Signet."));
    }
    match action {
        Action::NewGuest { label } => {
            let table = table?;
            Some(created(table.create_guest(label, now)))
        }
        Action::NewPersonal { holder, label, days } => {
            let table = table?;
            Some(created(table.create_personal(holder, label, days, now)))
        }
        Action::Revoke { id, back } => match table?.revoke(&id) {
            Ok(true) => {
                super::emit(json!({"event":"invite_revoked","invite":id}));
                let mut response = Response::text(303);
                response.location = Some(back.to_string());
                Some(response)
            }
            Ok(false) => Some(Response::text(404)),
            Err(e) => Some(error_page(500, &e)),
        },
        Action::NewCode => {
            let mut snapshot = shared.lock();
            if snapshot.restarting {
                return None;
            }
            snapshot.restarting = true;
            snapshot.error = None;
            snapshot.code = None;
            snapshot.code_ready = false;
            snapshot.open = false;
            snapshot.players.clear();
            snapshot.addresses.clear();
            shared.new_code.store(true, Ordering::Release);
            None
        }
    }
}

fn created(result: Result<Created, String>) -> Response {
    match result {
        Ok(made) => {
            super::emit(json!({"event":"invite_created","invite":made.id,"kind":made.kind.as_str(),
                "label":made.label,"expires_at":made.expires_at}));
            let mut response = Response::text(303);
            response.location = Some(format!("/invites/new/{}", made.id));
            response
        }
        Err(e) => error_page(500, &e),
    }
}

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    /// The target without its query.
    pub path: String,
    pub host: Option<String>,
    pub origin: Option<String>,
    /// `Content-Length`, 0 if none.
    pub content_length: usize,
    /// A `Transfer-Encoding` header was sent (a body this server does not decode).
    pub chunked: bool,
    /// The body of a POST, read after the head.
    pub body: Vec<u8>,
}

/// Read a request head: up to the blank line, at most [`MAX_HEAD`] bytes, by
/// `deadline`. `Err` carries the status to answer with, or 0 to just close
/// (the client sent nothing or went away).
#[cfg(test)]
pub fn read_head(input: &mut impl Read, deadline: Instant) -> Result<Vec<u8>, u16> {
    read_head_and_rest(input, deadline).map(|(head, _)| head)
}

/// As [`read_head`], and the bytes that came in after the blank line.
pub fn read_head_and_rest(input: &mut impl Read, deadline: Instant) -> Result<(Vec<u8>, Vec<u8>), u16> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = head_end(&head) {
            if end > MAX_HEAD {
                return Err(431);
            }
            let rest = head.split_off(end);
            return Ok((head, rest));
        }
        if head.len() > MAX_HEAD {
            return Err(431);
        }
        let silent = |status: u16| if head.is_empty() { 0 } else { status };
        if Instant::now() >= deadline {
            return Err(silent(408));
        }
        match input.read(&mut chunk) {
            Ok(0) => return Err(silent(400)),
            Ok(n) => head.extend_from_slice(&chunk[..n]),
            // A read that timed out only lets the deadline be checked again.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(silent(400)),
        }
    }
}

/// Read a body of `length` bytes, some of which (`have`) came with the head,
/// by `deadline`. `Err` carries the status to answer with.
pub fn read_body(input: &mut impl Read, mut have: Vec<u8>, length: usize, deadline: Instant) -> Result<Vec<u8>, u16> {
    have.truncate(length);
    let mut chunk = [0u8; 1024];
    while have.len() < length {
        if Instant::now() >= deadline {
            return Err(408);
        }
        match input.read(&mut chunk[..(length - have.len()).min(1024)]) {
            Ok(0) => return Err(400),
            Ok(n) => have.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(400),
        }
    }
    Ok(have)
}

/// Where the head ends (after its blank line), if it has.
fn head_end(bytes: &[u8]) -> Option<usize> {
    (0..bytes.len()).find_map(|i| {
        let rest = &bytes[i..];
        if rest.starts_with(b"\r\n\r\n") {
            Some(i + 4)
        } else if rest.starts_with(b"\n\n") {
            Some(i + 2)
        } else {
            None
        }
    })
}

/// Parse a request head. `Err` is the status to answer with.
pub fn parse_request(head: &[u8]) -> Result<Request, u16> {
    let text = std::str::from_utf8(head).map_err(|_| 400u16)?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let mut parts = lines.next().ok_or(400u16)?.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(400);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(505);
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(400);
    }
    if !target.starts_with('/') {
        return Err(400);
    }
    let path = target.split(['?', '#']).next().unwrap_or("/").to_string();
    let (mut host, mut origin) = (None, None);
    let (mut content_length, mut chunked) = (None::<usize>, false);
    for line in lines.take_while(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(400);
        };
        let value = value.trim().to_string();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => host = Some(value),
            "origin" => origin = Some(value),
            "content-length" => {
                let length = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| value.parse::<usize>().ok())
                    .flatten()
                    .ok_or(400u16)?;
                if content_length.is_some_and(|seen| seen != length) {
                    return Err(400);
                }
                content_length = Some(length);
            }
            "transfer-encoding" => chunked = true,
            _ => {}
        }
    }
    Ok(Request {
        method: method.to_string(),
        path,
        host,
        origin,
        content_length: content_length.unwrap_or(0),
        chunked,
        body: Vec::new(),
    })
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            content_type,
            location: None,
            body: body.into(),
        }
    }

    /// A plain-text answer for an error.
    pub fn text(status: u16) -> Self {
        Self::new(status, "text/plain; charset=utf-8", format!("{}\n", reason(status)))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            self.status,
            reason(self.status),
            self.content_type,
            self.body.len()
        );
        if let Some(location) = &self.location {
            head.push_str(&format!("Location: {location}\r\n"));
        }
        head.push_str(
            // `same-origin`, not `no-referrer`: with no referrer a browser sends
            // `Origin: null` even on the page's own form post, which `respond` refuses.
            "Cache-Control: no-store\r\nReferrer-Policy: same-origin\r\nX-Content-Type-Options: nosniff\r\n\
             X-Frame-Options: DENY\r\n\
             Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'\r\n\
             Connection: close\r\n\r\n",
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Content Too Large",
        421 => "Misdirected Request",
        408 => "Request Timeout",
        500 => "Internal Server Error",
        431 => "Request Header Fields Too Large",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

/// Answer a request, and say what it asks the room to do.
pub fn respond(
    request: &Request,
    snapshot: &Snapshot,
    host_name: Option<&str>,
) -> (Response, Option<Action>) {
    // A page on another site can make a browser send requests here under its
    // own name (DNS rebinding): only an address or a name we were told is ours.
    if !host_allowed(request.host.as_deref(), host_name) {
        return (Response::text(421), None);
    }
    // Once the Keep is claimed, invites are made in My Signet: nothing here makes
    // or cancels one, and no live link is shown. (The list stays, read-only.)
    if snapshot.owner_claimed {
        let makes = request.method == "POST" && request.path.starts_with("/invites/");
        if makes || request.path.starts_with("/invites/new/") {
            return (error_page(403, "This Keep's invites are managed in My Signet."), None);
        }
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => (
            Response::new(200, "text/html; charset=utf-8", page(snapshot)),
            None,
        ),
        ("GET", "/status.json") => (
            Response::new(200, "application/json", format!("{}\n", status_json(snapshot))),
            None,
        ),
        ("POST", "/new-code") => {
            // A page on another site must not be able to replace the code.
            if !same_site(request) {
                return (Response::text(403), None);
            }
            let mut response = Response::text(303);
            response.location = Some("/".to_string());
            (response, Some(Action::NewCode))
        }
        ("GET", "/invites") => match &snapshot.invites {
            Some(_) => (
                Response::new(200, "text/html; charset=utf-8", invites_page(snapshot)),
                None,
            ),
            None => (Response::text(404), None),
        },
        ("GET", path) if path.starts_with("/invites/new/") => new_invite_page(path, snapshot),
        ("POST", path) if path.starts_with("/invites/") => invite_post(request, path, snapshot),
        _ => (Response::text(404), None),
    }
}

/// `GET /invites/new/<id>`: the link of an invite just made. It is shown only
/// while the invite is live and, for a guest pass, unused; opening it again
/// (a refresh, Back) makes nothing.
fn new_invite_page(path: &str, snapshot: &Snapshot) -> (Response, Option<Action>) {
    let Some(list) = &snapshot.invites else {
        return (Response::text(404), None);
    };
    let id = path.trim_start_matches("/invites/new/");
    let Some(invite) = list.iter().find(|i| i.id.eq_ignore_ascii_case(id)) else {
        return (Response::text(404), None);
    };
    let link = match (&invite.link, invite.kind, invite.usage) {
        (Some(link), "personal", _) | (Some(link), _, "unused") => link,
        _ => {
            let why = "This link isn't shown here any more: the invite was used, has expired or was revoked.";
            return (error_page(404, why), None);
        }
    };
    let response = Response::new(
        200,
        "text/html; charset=utf-8",
        created_page(invite.kind, link, invite.holder_short.as_deref()),
    );
    (response, None)
}

/// A browser form post is from this page (its `Origin` is this address); a
/// script that sends no `Origin` is let through, a page on another site is not.
fn same_site(request: &Request) -> bool {
    request.origin.as_ref().is_none_or(|origin| {
        request
            .host
            .as_ref()
            .is_some_and(|host| origin.eq_ignore_ascii_case(&format!("http://{host}")))
    })
}

/// The invite routes: `/invites/guest`, `/invites/personal`, `/invites/<id>/revoke`.
fn invite_post(request: &Request, path: &str, snapshot: &Snapshot) -> (Response, Option<Action>) {
    if snapshot.invites.is_none() {
        return (Response::text(404), None);
    }
    let route = path.trim_start_matches("/invites/");
    let known = matches!(route, "guest" | "personal") || revoke_id(route).is_some();
    if !known {
        return (Response::text(404), None);
    }
    if !same_site(request) {
        return (Response::text(403), None);
    }
    let form = parse_form(&request.body);
    let field = |name: &str| form.iter().find(|(k, _)| k == name).map_or("", |(_, v)| v.as_str());
    match route {
        "guest" => (
            Response::text(303),
            Some(Action::NewGuest { label: invites::clean_label(field("label")) }),
        ),
        "personal" => {
            let holder = match invites::parse_persona(field("persona")) {
                Ok(holder) => holder,
                Err(e) => return (error_page(400, &e), None),
            };
            let days = match field("days").trim() {
                "" => invites::PERSONAL_DEFAULT_DAYS,
                text => match text.parse::<u32>() {
                    Ok(days) if (1..=invites::MAX_DAYS).contains(&days) => days,
                    _ => {
                        let why = format!("Days must be a whole number from 1 to {}", invites::MAX_DAYS);
                        return (error_page(400, &why), None);
                    }
                },
            };
            (
                Response::text(303),
                Some(Action::NewPersonal { holder, label: invites::clean_label(field("label")), days }),
            )
        }
        _ => {
            let id = revoke_id(route).unwrap_or_default().to_string();
            let back = if field("from") == "invites" { "/invites" } else { "/" };
            (Response::text(303), Some(Action::Revoke { id, back }))
        }
    }
}

/// The id in `<32 hex>/revoke`.
fn revoke_id(route: &str) -> Option<&str> {
    let id = route.strip_suffix("/revoke")?;
    (id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// `application/x-www-form-urlencoded` as (name, value) pairs. Anything that
/// does not decode is kept as it came.
pub fn parse_form(body: &[u8]) -> Vec<(String, String)> {
    let decode = |text: &[u8]| {
        let mut out = Vec::with_capacity(text.len());
        let mut at = 0;
        while at < text.len() {
            match text[at] {
                b'+' => out.push(b' '),
                b'%' if at + 2 < text.len() + 0 && text[at + 1].is_ascii_hexdigit() && text[at + 2].is_ascii_hexdigit() => {
                    let pair = std::str::from_utf8(&text[at + 1..at + 3]).unwrap_or("00");
                    out.push(u8::from_str_radix(pair, 16).unwrap_or(0));
                    at += 2;
                }
                byte => out.push(byte),
            }
            at += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    body.split(|b| *b == b'&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = match pair.iter().position(|b| *b == b'=') {
                Some(at) => (&pair[..at], &pair[at + 1..]),
                None => (pair, &pair[..0]),
            };
            (decode(name), decode(value))
        })
        .collect()
}

/// Whether a `Host` header names this machine by an IP address, `localhost`
/// or `name` (`--status-host`, lower case), with or without a port.
pub fn host_allowed(host: Option<&str>, name: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let port_ok = |port: &str| {
        !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())
    };
    if let Some(rest) = host.strip_prefix('[') {
        let Some((inner, after)) = rest.split_once(']') else {
            return false;
        };
        return inner.parse::<std::net::Ipv6Addr>().is_ok()
            && (after.is_empty() || after.strip_prefix(':').is_some_and(port_ok));
    }
    let hostname = match host.split_once(':') {
        Some((hostname, port)) if port_ok(port) => hostname,
        Some(_) => return false,
        None => host,
    };
    hostname.parse::<std::net::Ipv4Addr>().is_ok()
        || hostname.eq_ignore_ascii_case("localhost")
        || name.is_some_and(|n| hostname.eq_ignore_ascii_case(n))
}

/// The code as the app shows it: `KWT QPM` (`spaced_code` in app/src/online/mod.rs).
pub fn spaced_code(code: &str) -> String {
    let mut out = String::new();
    for (i, c) in code.chars().enumerate() {
        if i == 3 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

pub fn status_json(snapshot: &Snapshot) -> serde_json::Value {
    json!({
        "state": snapshot.state(),
        "error": snapshot.error,
        "online": snapshot.online,
        "code": snapshot.code,
        "code_spaced": snapshot.code.as_deref().map(spaced_code),
        "code_ready": snapshot.code_ready,
        "phase": snapshot.phase,
        "open": snapshot.open,
        "players": snapshot.players,
        "player_count": snapshot.players.len(),
        "races": snapshot.races,
        "relays": {"connected": snapshot.relays.0, "total": snapshot.relays.1},
        "addresses": snapshot.addresses.iter()
            .map(|(kind, addr)| json!({"kind": kind, "addr": addr}))
            .collect::<Vec<_>>(),
        "owner_claimed": snapshot.owner_claimed,
        "invites_enabled": snapshot.invites.is_some(),
        "invites_problem": snapshot.invites_problem,
        "invites": snapshot.invites.clone().unwrap_or_default(),
    })
}

/// What a guest pass is, as the pages say it.
const GUEST_RULE: &str = "A guest pass works for the first person who uses it, until it expires.";

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "lan" => "home network",
        "stun" => "public address",
        "upnp" => "router port",
        "v6" => "IPv6",
        _ => "announced",
    }
}

const STYLE: &str = "body{font:16px/1.4 system-ui,sans-serif;margin:0;padding:16px;background:#14161a;color:#eceff4}main{max-width:30rem;margin:0 auto}h1{font-size:1.1rem;margin:0 0 .25rem}h2{font-size:1rem;margin:1.5rem 0 .25rem}.state{margin:0;color:#9aa4b2}.code{font:700 3rem/1.2 ui-monospace,monospace;letter-spacing:.1em;margin:.75rem 0}dl{margin:0}dl div{display:flex;justify-content:space-between;padding:.2rem 0;border-bottom:1px solid #2a2e36}dt{color:#9aa4b2}dd{margin:0}ul{margin:0;padding-left:1.2rem}code{font-family:ui-monospace,monospace}.dim{color:#9aa4b2}button{font:inherit;padding:.6rem 1rem;border-radius:.4rem;border:0;background:#3b82f6;color:#fff}form{margin-top:2rem}form p{margin:.5rem 0 0;color:#9aa4b2}.invite{list-style:none;margin:.5rem 0;padding:.6rem .75rem;border:1px solid #2a2e36;border-radius:.4rem}.invites{padding:0}.invite form,.make form{margin-top:.5rem}.invite p{margin:.2rem 0 0}.tag{font-size:.8rem;padding:.05rem .4rem;border-radius:.3rem;background:#2a2e36;margin-left:.3rem}.live{background:#14532d}button.small{padding:.3rem .7rem;font-size:.9rem;background:#7f1d1d}input,textarea{font:inherit;box-sizing:border-box;width:100%;padding:.5rem;margin:.25rem 0;border-radius:.4rem;border:1px solid #2a2e36;background:#0e1013;color:#eceff4}label{display:block;margin-top:.5rem;color:#9aa4b2}a{color:#7aa7ff}.bad{color:#f87171;margin:.75rem 0}";

fn page(snapshot: &Snapshot) -> String {
    let state = match snapshot.state() {
        "starting" => "Starting a new room",
        "nearby" => "Nearby only",
        "online" => "Online",
        _ => "Waiting for relays",
    };
    let problem = snapshot.error.as_ref().map_or(String::new(), |reason| {
        format!("<p class=state>Couldn't reopen the room: {}</p>", html_escape(reason))
    });
    let invites_off = snapshot.invites_problem.as_ref().map_or(String::new(), |why| {
        format!(
            "<p class=bad>Invites are off: {}. Fix or move the file and restart.</p>",
            html_escape(why)
        )
    });
    let code = match (&snapshot.code, snapshot.state()) {
        (Some(code), "online" | "nearby") => html_escape(&spaced_code(code)),
        _ => "&hellip;".to_string(),
    };
    let players = if snapshot.players.is_empty() {
        "<p class=dim>Nobody yet.</p>".to_string()
    } else {
        let items: String = snapshot
            .players
            .iter()
            .map(|name| format!("<li>{}</li>", html_escape(name)))
            .collect();
        format!("<ul>{items}</ul>")
    };
    let addresses = if snapshot.addresses.is_empty() {
        "<p class=dim>None yet.</p>".to_string()
    } else {
        let items: String = snapshot
            .addresses
            .iter()
            .map(|(kind, addr)| {
                format!("<li><code>{}</code> <span class=dim>{}</span></li>", html_escape(addr), kind_label(kind))
            })
            .collect();
        format!("<ul>{items}</ul>")
    };
    let relays = if snapshot.online {
        format!(
            "<div><dt>Relays</dt><dd>{} of {} connected</dd></div>",
            snapshot.relays.0, snapshot.relays.1
        )
    } else {
        String::new()
    };
    let owner = owner_section(snapshot);
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
<meta name=viewport content=\"width=device-width,initial-scale=1\">\
<meta http-equiv=refresh content=5><title>Stampede Keep</title><style>\
{style}\
</style></head><body><main><h1>Stampede Keep</h1><p class=state>{state}</p>{problem}{invites_off}{owner}\
<div class=code>{code}</div>\
<dl><div><dt>Phase</dt><dd>{phase}</dd></div>\
<div><dt>Players</dt><dd>{count}</dd></div>\
<div><dt>Races finished</dt><dd>{races}</dd></div>{relays}</dl>\
<h2>Players in the room</h2>{players}\
<h2>Players connect to</h2>{addresses}{invites}\
<form method=post action=/new-code><button>New code</button>\
<p>Players in the room will be disconnected.</p></form></main></body></html>",
        style = STYLE,
        invites = invites_section(snapshot, false),
        phase = html_escape(&snapshot.phase),
        count = snapshot.players.len(),
        races = snapshot.races,
    )
}

/// The owner part of the page: how to claim the Keep, or that it is claimed.
fn owner_section(snapshot: &Snapshot) -> String {
    if let Some(why) = &snapshot.owner_problem {
        return format!("<p class=bad>The owner can't be read: {}. Invites are off until it is fixed.</p>", html_escape(why));
    }
    if snapshot.owner_claimed {
        return "<p class=dim>This Keep has an owner. Its invites are made in My Signet.</p>".to_string();
    }
    if let Some(link) = &snapshot.pairing {
        return format!(
            "<h2>Make this your Keep</h2>\
<p class=dim>In My Signet: Settings, My Keep, Add a Keep, then paste this link. The first to use it becomes the owner, so keep it to yourself.</p>\
<textarea readonly rows=6 spellcheck=false>{}</textarea>",
            html_escape(link)
        );
    }
    if snapshot.owner_open {
        return "<h2>Make this your Keep</h2><p class=dim>The link is shown on the Keep's own computer, and in the Keep's log. Open this page there, or read the log.</p>".to_string();
    }
    String::new()
}

/// How long ago / from now, roughly: `3 min`, `23 h`, `5 days`.
fn span(seconds: u64) -> String {
    match seconds {
        0..=59 => "under a minute".to_string(),
        60..=3_599 => format!("{} min", seconds / 60),
        3_600..=172_799 => format!("{} h", seconds / 3_600),
        _ => format!("{} days", seconds / 86_400),
    }
}

fn invite_row(invite: &InviteView, now: u64, back: &str, can_revoke: bool) -> String {
    let what = match invite.kind {
        "personal" => "Personal invite",
        _ => "Guest pass",
    };
    let mut details = Vec::new();
    if let Some(label) = &invite.label {
        details.push(html_escape(label));
    }
    if let Some(holder) = invite.holder_short.as_ref().filter(|_| invite.kind == "personal") {
        details.push(format!("for <code>{}</code>", html_escape(holder)));
    }
    details.push(format!("made {} ago", span(now.saturating_sub(invite.created_at))));
    details.push(if invite.expires_in > 0 {
        format!("expires in {}", span(invite.expires_in as u64))
    } else {
        format!("expired {} ago", span(invite.expires_in.unsigned_abs()))
    });
    let names: Vec<String> = invite
        .used_by
        .iter()
        .map(|u| {
            if u.name.is_empty() {
                format!("<code>{}</code>", html_escape(&invites::short_npub(&u.persona)))
            } else {
                html_escape(&u.name)
            }
        })
        .collect();
    let mut who = String::new();
    if invite.kind == "guest" {
        // A guest pass is for the first person who uses it.
        details.push(match (invite.usage, names.is_empty()) {
            ("unused", _) => "unused".to_string(),
            (_, true) => "used".to_string(),
            (_, false) => format!("used by {}", names.join(", ")),
        });
    } else {
        details.push(match invite.max_uses {
            Some(max) => format!("used {} of {max}", invite.uses),
            None => format!("used {} times", invite.uses),
        });
        if !names.is_empty() {
            who = format!("<p class=dim>Used by {}</p>", names.join(", "));
        }
    }
    let revoke = if can_revoke && matches!(invite.status, "live" | "used") {
        format!(
            "<form method=post action=/invites/{id}/revoke><input type=hidden name=from value={back}>\
<button class=small>Revoke</button></form>",
            id = html_escape(&invite.id),
        )
    } else {
        String::new()
    };
    format!(
        "<li class=invite><b>{what}</b><span class=\"tag {status}\">{status}</span>\
<p class=dim>{details}</p>{who}{revoke}</li>",
        status = html_escape(invite.status),
        details = details.join(" &middot; "),
    )
}

/// The invites: the list and, with `forms`, the forms that make one.
fn invites_section(snapshot: &Snapshot, forms: bool) -> String {
    let Some(list) = &snapshot.invites else {
        return if snapshot.invites_problem.is_some() {
            "<h2>Invites</h2><p class=dim>Invites are off for this run.</p>".to_string()
        } else {
            "<h2>Invites</h2><p class=dim>Invites need --online and --data-dir.</p>".to_string()
        };
    };
    let back = if forms { "invites" } else { "main" };
    let managed = snapshot.owner_claimed;
    let rows: String = list.iter().map(|invite| invite_row(invite, snapshot.now, back, !managed)).collect();
    let rows = if list.is_empty() {
        "<p class=dim>No invites yet.</p>".to_string()
    } else {
        format!("<ul class=invites>{rows}</ul>")
    };
    let make = if managed {
        "<p class=dim>Invites for this Keep are made and cancelled in My Signet.</p>".to_string()
    } else if forms {
        "<div class=make><h2>New guest pass</h2>\
<form method=post action=/invites/guest><label>Name it (optional)<input name=label maxlength=40></label>\
<button>New guest pass</button><p>A guest pass works for the first person who uses it, until it expires. It lasts a day.</p></form>\
<h2>New personal invite</h2>\
<form method=post action=/invites/personal>\
<label>Their npub or hex key<input name=persona required autocomplete=off></label>\
<label>Name it (optional)<input name=label maxlength=40></label>\
<label>Days it lasts (1&ndash;365)<input name=days value=30 inputmode=numeric></label>\
<button>New personal invite</button>\
<p>Only that person can use it, as often as they like until it expires.</p></form></div>"
            .to_string()
    } else {
        "<div class=make><form method=post action=/invites/guest><button>New guest pass</button></form>\
<p class=dim><a href=/invites>More invite options</a></p></div>"
            .to_string()
    };
    let rule = if managed { String::new() } else { format!("<p class=dim>{GUEST_RULE}</p>") };
    format!("<h2>Invites</h2>{rule}{rows}{make}")
}

fn shell(body: &str, refresh: bool) -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
<meta name=viewport content=\"width=device-width,initial-scale=1\">{refresh}\
<title>Stampede Keep</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>",
        refresh = if refresh { "<meta http-equiv=refresh content=5>" } else { "" },
    )
}

/// `GET /invites`: the forms, on a page that does not reload while you type.
fn invites_page(snapshot: &Snapshot) -> String {
    shell(
        &format!(
            "<h1>Stampede Keep</h1><p class=state><a href=/>Back to the room</a></p>{}",
            invites_section(snapshot, !snapshot.owner_claimed)
        ),
        false,
    )
}

/// The page that shows a new invite's link in a box to copy (`kind` is
/// `guest` or `personal`).
fn created_page(kind: &str, link: &str, holder_short: Option<&str>) -> String {
    let (what, rule) = match kind {
        "personal" => (
            "New personal invite",
            format!(
                "Only {} can use it, as often as they like until it expires.",
                holder_short.map_or("that person".to_string(), |h| format!("<code>{}</code>", html_escape(h))),
            ),
        ),
        _ => ("New guest pass", format!("{GUEST_RULE} It lasts a day.")),
    };
    shell(
        &format!(
            "<h1>Stampede Keep</h1><p class=state>{what}</p>\
<p>Send this link to your guest.</p>\
<textarea readonly rows=5 spellcheck=false>{link}</textarea><p class=dim>{rule}</p>\
<p class=dim>Anyone on your home network who opens this page can see live invite links.</p>\
<p><a href=/invites>Back to invites</a></p>",
            link = html_escape(link),
        ),
        false,
    )
}

fn error_page(status: u16, message: &str) -> Response {
    Response::new(
        status,
        "text/html; charset=utf-8",
        shell(
            &format!(
                "<h1>Stampede Keep</h1><p class=state>{}</p><p><a href=/invites>Back to invites</a></p>",
                html_escape(message)
            ),
            false,
        ),
    )
}

// ─── the server ────────────────────────────────────────────────────────────

/// Serve the status page on its own thread until the process ends.
pub fn spawn(listener: TcpListener, shared: Arc<Shared>) -> std::io::Result<()> {
    serve_on(listener, shared, HEAD_DEADLINE)
}

fn serve_on(listener: TcpListener, shared: Arc<Shared>, deadline: Duration) -> std::io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("status".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    // Out of descriptors or the like: do not spin.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                    active.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let (shared, counter) = (shared.clone(), active.clone());
                let spawned = std::thread::Builder::new().name("status-conn".into()).spawn(move || {
                    handle(stream, &shared, deadline);
                    counter.fetch_sub(1, Ordering::AcqRel);
                });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })
        .map(|_| ())
}

fn handle(mut stream: TcpStream, shared: &Shared, deadline: Duration) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let from_loopback = stream.peer_addr().is_ok_and(|peer| peer.ip().to_canonical().is_loopback());
    let end = Instant::now() + deadline;
    let reply = |stream: &mut TcpStream, response: Response| {
        let _ = stream.write_all(&response.to_bytes());
        let _ = stream.flush();
    };
    // A short read timeout, so the head deadline is checked between reads.
    let _ = stream.set_read_timeout(Some(deadline.min(Duration::from_millis(500))));
    let request = read_head_and_rest(&mut stream, end).and_then(|(head, rest)| {
        let mut request = parse_request(&head)?;
        if request.method == "POST" {
            if request.chunked {
                return Err(411);
            }
            if request.content_length > MAX_BODY {
                return Err(413);
            }
            request.body = read_body(&mut stream, rest, request.content_length, end)?;
        }
        Ok(request)
    });
    let response = match request {
        Ok(request) => {
            let mut snapshot = shared.snapshot();
            snapshot.now = unix_now();
            snapshot.invites = shared.invites.as_ref().map(|table| table.views(snapshot.now));
            snapshot.invites_problem = shared.invites_problem.clone();
            snapshot.owner_problem = shared.owner_problem.clone();
            snapshot.owner_claimed = shared.owner_problem.is_some();
            if let Some(owner) = &shared.owner {
                snapshot.owner_claimed |= owner.claimed();
                snapshot.owner_open = !snapshot.owner_claimed;
                // The link makes whoever holds it the owner: only a request from this
                // machine sees it. (A reverse proxy on the box would look local too.)
                if snapshot.owner_open && from_loopback {
                    snapshot.pairing = owner.pairing_link();
                }
            }
            if snapshot.owner_claimed {
                // Read-only: what exists, with no link to copy.
                for invite in snapshot.invites.iter_mut().flatten() {
                    invite.link = None;
                }
            }
            let (response, action) = respond(&request, &snapshot, shared.host_name.as_deref());
            match action.and_then(|action| apply(action, shared)) {
                Some(answer) => answer,
                None => response,
            }
        }
        Err(0) => return,
        Err(status) => Response::text(status),
    };
    reply(&mut stream, response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample() -> Snapshot {
        Snapshot {
            code: Some("KWTQPM".into()),
            online: true,
            code_ready: true,
            restarting: false,
            error: None,
            phase: "lobby".into(),
            open: true,
            players: vec!["Ada".into(), "<b>Bo</b> & \"co\"".into()],
            races: 3,
            relays: (2, 3),
            addresses: vec![("stun".into(), "203.0.113.9:47800".into())],
            invites: None,
            invites_problem: None,
            now: 0,
            ..Snapshot::default()
        }
    }

    fn head(text: &str) -> Request {
        parse_request(text.as_bytes()).unwrap()
    }

    fn get(path: &str, snapshot: &Snapshot) -> (Response, Option<Action>) {
        respond(&head(&format!("GET {path} HTTP/1.1\r\nHost: 192.168.1.9:8780\r\n\r\n")), snapshot, None)
    }

    fn text(response: &Response) -> String {
        String::from_utf8(response.body.clone()).unwrap()
    }

    #[test]
    fn the_page_shows_the_room() {
        let (response, action) = get("/", &sample());
        assert_eq!((response.status, action), (200, None));
        let html = text(&response);
        assert!(response.content_type.starts_with("text/html"));
        assert!(html.contains("http-equiv=refresh content=5"));
        assert!(html.contains("name=viewport"));
        assert!(html.contains("class=code>KWT QPM<"));
        assert!(html.contains(">Online<"));
        assert!(html.contains("<li>Ada</li>"));
        assert!(html.contains("Races finished</dt><dd>3<"));
        assert!(html.contains("2 of 3 connected"));
        assert!(html.contains("Players connect to"));
        assert!(html.contains("<code>203.0.113.9:47800</code>"));
        assert!(html.contains("<form method=post action=/new-code>"));
        assert!(html.contains("Players in the room will be disconnected."));
    }

    #[test]
    fn names_are_escaped_in_the_page() {
        let html = text(&get("/", &sample()).0);
        assert!(html.contains("<li>&lt;b&gt;Bo&lt;/b&gt; &amp; &quot;co&quot;</li>"));
        assert!(!html.contains("<b>"));
        assert_eq!(html_escape("a<'>\n\u{7}x"), "a&lt;&#39;&gt;  x");
    }

    #[test]
    fn the_page_says_what_the_room_is_waiting_for() {
        let mut waiting = sample();
        waiting.code_ready = false;
        waiting.relays = (0, 3);
        let html = text(&get("/", &waiting).0);
        assert!(html.contains("Waiting for relays"));
        assert!(!html.contains("KWT QPM"));
        let mut starting = sample();
        starting.restarting = true;
        starting.code = None;
        assert!(text(&get("/", &starting).0).contains("Starting a new room"));
        let mut nearby = sample();
        nearby.online = false;
        let html = text(&get("/", &nearby).0);
        assert!(html.contains("Nearby only") && html.contains("KWT QPM") && !html.contains("Relays"));
        assert!(text(&get("/", &Snapshot::default()).0).contains("Nobody yet."));
    }

    #[test]
    fn the_json_matches_the_page() {
        let (response, _) = get("/status.json", &sample());
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
        let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["state"], "online");
        assert_eq!(value["code"], "KWTQPM");
        assert_eq!(value["code_spaced"], "KWT QPM");
        assert_eq!(value["phase"], "lobby");
        assert_eq!(value["open"], true);
        assert_eq!(value["player_count"], 2);
        assert_eq!(value["players"][1], "<b>Bo</b> & \"co\"");
        assert_eq!(value["races"], 3);
        assert_eq!(value["relays"], json!({"connected": 2, "total": 3}));
        assert_eq!(value["addresses"][0], json!({"kind": "stun", "addr": "203.0.113.9:47800"}));
        let mut starting = sample();
        starting.restarting = true;
        starting.code = None;
        let value = status_json(&starting);
        assert_eq!((value["state"].as_str(), value["code"].is_null()), (Some("starting"), true));
    }

    #[test]
    fn a_query_does_not_change_the_route() {
        assert_eq!(get("/?x=1", &sample()).0.status, 200);
        assert_eq!(get("/status.json?pretty", &sample()).0.status, 200);
    }

    #[test]
    fn a_new_code_redirects_and_asks_for_one() {
        let request = head("POST /new-code HTTP/1.1\r\nHost: 192.168.1.9:8780\r\nContent-Length: 0\r\n\r\n");
        let (response, action) = respond(&request, &sample(), None);
        assert_eq!((response.status, response.location.clone()), (303, Some("/".to_string())));
        assert_eq!(action, Some(Action::NewCode));
        assert!(String::from_utf8(response.to_bytes()).unwrap().contains("Location: /\r\n"));
    }

    #[test]
    fn a_new_code_asked_from_another_site_is_refused() {
        let post = |origin: &str| {
            let text = format!("POST /new-code HTTP/1.1\r\nHost: 192.168.1.9:8780\r\nOrigin: {origin}\r\n\r\n");
            respond(&head(&text), &sample(), None)
        };
        let (response, action) = post("http://192.168.1.9:8780");
        assert_eq!((response.status, action), (303, Some(Action::NewCode)));
        for origin in ["http://evil.example", "null", "https://192.168.1.9:8780"] {
            let (response, action) = post(origin);
            assert_eq!((response.status, action), (403, None), "{origin}");
        }
        let no_host = head("POST /new-code HTTP/1.1\r\nOrigin: http://x\r\n\r\n");
        assert_eq!(respond(&no_host, &sample(), None).0.status, 421);
    }

    #[test]
    fn anything_else_is_404() {
        for (method, path) in [
            ("GET", "/new-code"),
            ("POST", "/"),
            ("POST", "/status.json"),
            ("GET", "/favicon.ico"),
            ("GET", "/status.json/"),
            ("HEAD", "/"),
            ("DELETE", "/new-code"),
            ("GET", "/../etc/passwd"),
        ] {
            let request = head(&format!("{method} {path} HTTP/1.1\r\nHost: 192.168.1.9\r\n\r\n"));
            let (response, action) = respond(&request, &sample(), None);
            assert_eq!((response.status, action), (404, None), "{method} {path}");
        }
    }

    #[test]
    fn bad_requests_are_refused_without_a_panic() {
        let bad: [(&[u8], u16); 10] = [
            (b"", 400),
            (b"\r\n\r\n", 400),
            (b"GET\r\n\r\n", 400),
            (b"GET / HTTP/1.1 extra\r\n\r\n", 400),
            (b"get / HTTP/1.1\r\n\r\n", 400),
            (b"GET nowhere HTTP/1.1\r\n\r\n", 400),
            (b"GET / SPDY/3\r\n\r\n", 505),
            (b"GET / HTTP/1.1\r\nno colon here\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\n\xff\xfe: x\r\n\r\n", 400),
            (b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03", 400),
        ];
        for (bytes, status) in bad {
            assert_eq!(parse_request(bytes).err(), Some(status), "{bytes:?}");
        }
        // A bare newline ends lines too, and unknown headers are ignored.
        let request = parse_request(b"GET /status.json HTTP/1.0\nX-Odd: 1\nHost: h\n\n").unwrap();
        assert_eq!((request.path.as_str(), request.host.as_deref()), ("/status.json", Some("h")));
    }

    fn after(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    #[test]
    fn a_head_is_read_up_to_its_blank_line_and_no_further() {
        let mut input = Cursor::new(b"POST /new-code HTTP/1.1\r\nHost: h\r\n\r\nbody=1".to_vec());
        let bytes = read_head(&mut input, after(5)).unwrap();
        assert!(bytes.ends_with(b"\r\n\r\n") && !bytes.ends_with(b"body=1"));
    }

    #[test]
    fn a_head_over_8_kb_is_refused() {
        let mut long = b"GET / HTTP/1.1\r\nX-Pad: ".to_vec();
        long.extend(std::iter::repeat_n(b'a', 20_000));
        assert_eq!(read_head(&mut Cursor::new(long.clone()), after(5)), Err(431));
        // Even with its blank line in the same read.
        long.extend_from_slice(b"\r\n\r\n");
        assert_eq!(read_head(&mut Cursor::new(long), after(5)), Err(431));
        // A head just under the cap is fine.
        let mut ok = b"GET / HTTP/1.1\r\nX-Pad: ".to_vec();
        ok.extend(std::iter::repeat_n(b'a', MAX_HEAD - 40));
        ok.extend_from_slice(b"\r\n\r\n");
        assert!(read_head(&mut Cursor::new(ok), after(5)).is_ok());
    }

    #[test]
    fn a_head_that_stops_halfway_is_refused_and_silence_just_closes() {
        assert_eq!(read_head(&mut Cursor::new(b"GET / HT".to_vec()), after(5)), Err(400));
        assert_eq!(read_head(&mut Cursor::new(Vec::new()), after(5)), Err(0));
        struct Stall(Vec<u8>);
        impl Read for Stall {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                    return Err(std::io::ErrorKind::WouldBlock.into());
                }
                let n = self.0.len().min(out.len());
                out[..n].copy_from_slice(&self.0[..n]);
                self.0.drain(..n);
                Ok(n)
            }
        }
        let late = Instant::now() + Duration::from_millis(50);
        assert_eq!(read_head(&mut Stall(b"GET / HTTP/1.1\r\nHo".to_vec()), late), Err(408));
        assert_eq!(read_head(&mut Stall(Vec::new()), late), Err(0));
    }

    #[test]
    fn a_new_code_shows_as_starting_at_once_and_only_once() {
        let shared = Shared::new(sample(), None);
        assert!(!shared.take_new_code());
        apply(Action::NewCode, &shared);
        let snapshot = shared.snapshot();
        assert!(snapshot.restarting && snapshot.code.is_none() && !snapshot.code_ready);
        assert!(snapshot.players.is_empty() && !snapshot.open);
        assert!(shared.take_new_code());
        assert!(!shared.take_new_code());
        // Asked again while the new room starts: nothing more to do.
        apply(Action::NewCode, &shared);
        assert!(!shared.take_new_code());
        shared.set(sample());
        apply(Action::NewCode, &shared);
        assert!(shared.take_new_code());
    }

    fn ask(address: std::net::SocketAddr, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(request).unwrap();
        let mut out = Vec::new();
        let _ = stream.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn the_server_answers_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::new(sample(), None));
        serve_on(listener, shared.clone(), Duration::from_millis(400)).unwrap();

        let page = ask(address, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n") && page.contains("KWT QPM"));
        assert!(page.contains("Connection: close") && page.contains("Content-Length: "));
        let json = ask(address, b"GET /status.json HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(json.contains("\"code\":\"KWTQPM\""));
        assert!(ask(address, b"GET /nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").starts_with("HTTP/1.1 404"));
        assert!(ask(address, b"\x00\x01garbage\r\n\r\n").starts_with("HTTP/1.1 400"));
        assert!(ask(address, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").contains("200 OK"));

        // A client that stalls is cut off, and does not stop the next one.
        let mut slow = TcpStream::connect(address).unwrap();
        slow.write_all(b"GET / HTTP/1.1\r\nHo").unwrap();
        assert!(ask(address, b"GET / HTTP/1.1\r\nHost: [::1]\r\n\r\n").starts_with("HTTP/1.1 200"));
        slow.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reply = String::new();
        let _ = slow.read_to_string(&mut reply);
        assert!(reply.starts_with("HTTP/1.1 408"), "{reply:?}");

        // A POST with a body is answered and applied.
        let post = ask(address, b"POST /new-code HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\n\r\nabcd");
        assert!(post.starts_with("HTTP/1.1 303 See Other\r\n") && post.contains("Location: /\r\n"));
        assert!(shared.take_new_code());
        assert!(ask(address, b"GET /status.json HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").contains("\"state\":\"starting\""));
    }

    #[test]
    fn only_our_own_names_are_accepted_as_host() {
        for host in [
            "192.168.1.9",
            "192.168.1.9:8780",
            "127.0.0.1:1",
            "localhost",
            "LocalHost:8780",
            "[::1]",
            "[fe80::1]:8780",
            "[2001:db8::5]:80",
        ] {
            assert!(host_allowed(Some(host), None), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:8780",
            "192.168.1.9.evil.example",
            "keep.local",
            "localhost.evil.example",
            "",
            ":8780",
            "192.168.1.9:",
            "192.168.1.9:80:80",
            "192.168.1.9:port",
            "300.1.1.1",
            "[::1",
            "[::1]x",
            "[::1]:",
            "[nonsense]",
            "::1",
            "1.2.3",
            "192.168.1.9 evil.example",
        ] {
            assert!(!host_allowed(Some(host), None), "{host}");
        }
        assert!(!host_allowed(None, None));
        // A name from --status-host, and only that one.
        assert!(host_allowed(Some("keep.home.arpa:8780"), Some("keep.home.arpa")));
        assert!(host_allowed(Some("KEEP.home.arpa"), Some("keep.home.arpa")));
        assert!(!host_allowed(Some("other.home.arpa"), Some("keep.home.arpa")));
        assert!(!host_allowed(Some("x.keep.home.arpa"), Some("keep.home.arpa")));
    }

    #[test]
    fn a_rebinding_host_is_refused_on_every_route() {
        for (method, path) in [("GET", "/"), ("GET", "/status.json"), ("POST", "/new-code"), ("GET", "/nope")] {
            let text = format!(
                "{method} {path} HTTP/1.1\r\nHost: evil.example:8780\r\nOrigin: http://evil.example:8780\r\n\r\n"
            );
            let (response, action) = respond(&head(&text), &sample(), None);
            assert_eq!((response.status, action), (421, None), "{method} {path}");
            assert!(!text_of(&response).contains("KWT"), "{method} {path}");
        }
        // The same request is fine under a name the operator gave.
        let text = "GET /status.json HTTP/1.1\r\nHost: evil.example:8780\r\n\r\n";
        assert_eq!(respond(&head(text), &sample(), Some("evil.example")).0.status, 200);
        let post = "POST /new-code HTTP/1.1\r\nHost: keep.home.arpa\r\nOrigin: http://keep.home.arpa\r\n\r\n";
        assert_eq!(respond(&head(post), &sample(), Some("keep.home.arpa")).1, Some(Action::NewCode));
        assert_eq!(respond(&head(post), &sample(), None).1, None);
    }

    fn text_of(response: &Response) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    #[test]
    fn the_pages_own_form_post_is_accepted_and_a_script_without_origin_too() {
        // A browser form post from a page that says `same-origin`: its own origin.
        let browser = "POST /new-code HTTP/1.1\r\nHost: 192.168.1.9:8780\r\nOrigin: http://192.168.1.9:8780\r\nReferer: http://192.168.1.9:8780/\r\n\r\n";
        assert_eq!(respond(&head(browser), &sample(), None).1, Some(Action::NewCode));
        // curl sends no Origin.
        let script = "POST /new-code HTTP/1.1\r\nHost: 127.0.0.1:8780\r\n\r\n";
        assert_eq!(respond(&head(script), &sample(), None).1, Some(Action::NewCode));
        // The browser would send null if the page said no-referrer: refused.
        let null = "POST /new-code HTTP/1.1\r\nHost: 192.168.1.9:8780\r\nOrigin: null\r\n\r\n";
        assert_eq!(respond(&head(null), &sample(), None).0.status, 403);
    }

    #[test]
    fn the_headers_let_the_form_post_and_stop_framing() {
        let bytes = get("/", &sample()).0.to_bytes();
        let head = String::from_utf8(bytes).unwrap();
        let head = head.split("\r\n\r\n").next().unwrap();
        assert!(head.contains("Referrer-Policy: same-origin"), "{head}");
        assert!(!head.contains("no-referrer"));
        assert!(head.contains("X-Frame-Options: DENY"));
        assert!(head.contains("frame-ancestors 'none'"));
        assert!(head.contains("form-action 'self'") && head.contains("default-src 'none'"));
    }

    #[test]
    fn what_the_loop_saw_before_a_new_code_does_not_undo_it() {
        let shared = Shared::new(sample(), None);
        apply(Action::NewCode, &shared);
        // The loop took no request yet and publishes the old room.
        shared.set(sample());
        let snapshot = shared.snapshot();
        assert!(snapshot.restarting && snapshot.code.is_none());
        // The loop takes the request, hosts again, and publishes the new room.
        assert!(shared.take_new_code());
        let mut fresh = sample();
        fresh.code = Some("NEWONE".into());
        shared.set(fresh);
        let snapshot = shared.snapshot();
        assert!(!snapshot.restarting);
        assert_eq!(snapshot.code.as_deref(), Some("NEWONE"));
    }

    #[test]
    fn a_second_new_code_while_hosting_again_is_ignored() {
        let shared = Shared::new(sample(), None);
        apply(Action::NewCode, &shared);
        assert!(shared.take_new_code());
        // The loop is busy hosting again; the page still says starting.
        apply(Action::NewCode, &shared);
        assert!(!shared.take_new_code());
        // It stays so while the loop reports why it is retrying.
        let mut trying = shared.snapshot();
        trying.error = Some("Address already in use".into());
        shared.set(trying);
        apply(Action::NewCode, &shared);
        assert!(!shared.take_new_code());
    }

    #[test]
    fn a_room_that_cannot_reopen_says_why() {
        let mut stuck = Snapshot { restarting: true, ..Snapshot::default() };
        stuck.error = Some("Address <in> use".into());
        let html = text(&get("/", &stuck).0);
        assert!(html.contains("Couldn't reopen the room: Address &lt;in&gt; use"));
        assert!(html.contains("Starting a new room"));
        assert_eq!(status_json(&stuck)["error"], "Address <in> use");
        assert!(status_json(&sample())["error"].is_null());
    }

    // ─── invites ────────────────────────────────────────────────────────────

    const HOST_KEY: [u8; 32] = [7; 32];

    fn view(kind: &'static str, status: &'static str, label: &str) -> InviteView {
        InviteView {
            id: "0123456789abcdef0123456789abcdef".into(),
            kind,
            label: Some(label.into()),
            holder: (kind == "personal").then(|| "05".repeat(32)),
            holder_short: (kind == "personal").then(|| "npub1abcdefghi…".into()),
            created_at: 1_000,
            expires_at: 1_000 + 3 * 3_600,
            expires_in: 3 * 3_600 - 600,
            uses: 0,
            max_uses: (kind == "guest").then_some(1),
            last_used_at: None,
            used_by: if kind == "guest" {
                vec![]
            } else {
                vec![invites::UsedBy { persona: "05".repeat(32), name: "<i>Bo</i>".into() }]
            },
            source: "page",
            status,
            usage: if kind == "guest" { "unused" } else { "used" },
            link: (status == "live").then(|| "stampede://keep/x/y".to_string()),
        }
    }

    /// A guest pass someone has used: it is theirs now.
    fn used_guest(label: &str) -> InviteView {
        let mut view = view("guest", "live", label);
        view.usage = "used";
        view.uses = 2;
        view.holder = Some("05".repeat(32));
        view.holder_short = Some("npub1abcdefghi…".into());
        view.used_by = vec![invites::UsedBy { persona: "05".repeat(32), name: "<i>Bo</i>".into() }];
        view
    }

    fn with_invites(list: Vec<InviteView>) -> Snapshot {
        Snapshot { invites: Some(list), now: 1_600, ..sample() }
    }

    fn post(path: &str, extra_headers: &str, body: &str) -> Request {
        let mut request = head(&format!(
            "POST {path} HTTP/1.1\r\nHost: 192.168.1.9:8780\r\n{extra_headers}Content-Length: {}\r\n\r\n",
            body.len()
        ));
        request.body = body.as_bytes().to_vec();
        request
    }

    #[test]
    fn without_a_data_dir_the_invite_routes_are_off() {
        let off = sample();
        assert!(text(&get("/", &off).0).contains("Invites need --online and --data-dir"));
        assert_eq!(get("/invites", &off).0.status, 404);
        for path in ["/invites/guest", "/invites/personal", "/invites/0123456789abcdef0123456789abcdef/revoke"] {
            let (response, action) = respond(&post(path, "", "persona=x"), &off, None);
            assert_eq!((response.status, action), (404, None), "{path}");
        }
        let json = status_json(&off);
        assert_eq!((json["invites_enabled"].as_bool(), json["invites"].as_array().map(Vec::len)), (Some(false), Some(0)));
    }

    #[test]
    fn the_page_lists_invites_and_says_what_a_guest_pass_is() {
        let snapshot = with_invites(vec![
            view("guest", "live", "Cousin <b>Bo</b>"),
            view("personal", "revoked", "Ada"),
            used_guest("Pal"),
        ]);
        let html = text(&get("/", &snapshot).0);
        assert!(html.contains("http-equiv=refresh content=5"), "the room page still refreshes");
        assert!(html.contains("A guest pass works for the first person who uses it, until it expires."));
        assert!(!html.contains("can use it once") && !html.contains("only now"));
        assert!(html.contains("<b>Guest pass</b><span class=\"tag live\">live</span>"));
        assert!(html.contains("Cousin &lt;b&gt;Bo&lt;/b&gt;") && !html.contains("<b>Bo</b>"));
        assert!(html.contains("made 10 min ago") && html.contains("expires in 2 h"));
        // An unused guest pass says so; a used one says by whom, and still has a Revoke.
        assert!(html.contains("&middot; unused</p>"), "{html}");
        assert!(html.contains("&middot; used by &lt;i&gt;Bo&lt;/i&gt;</p>") && !html.contains("<i>"));
        assert!(!html.contains("used 0 of 1"));
        assert!(html.contains("<p class=dim>Used by &lt;i&gt;Bo&lt;/i&gt;</p>"), "personal invites still list who");
        assert!(html.contains("<form method=post action=/invites/0123456789abcdef0123456789abcdef/revoke>"));
        // The live ones (unused and used guest passes) can be revoked, the revoked one not; no link is listed.
        assert_eq!(html.matches("/revoke>").count(), 2);
        assert!(!html.contains("stampede://"));
        assert!(html.contains("<form method=post action=/invites/guest><button>New guest pass</button>"));
        assert!(html.contains("for <code>npub1abcdefghi…</code>"));
        assert!(text(&get("/", &with_invites(vec![])).0).contains("No invites yet."));
    }

    #[test]
    fn the_invites_page_has_the_forms_and_does_not_reload() {
        let (response, _) = get("/invites", &with_invites(vec![view("guest", "live", "x")]));
        assert_eq!(response.status, 200);
        let html = text(&response);
        assert!(!html.contains("http-equiv=refresh"));
        for needle in [
            "action=/invites/guest>",
            "action=/invites/personal>",
            "name=persona",
            "name=label",
            "name=days value=30",
            "name=from value=invites",
        ] {
            assert!(html.contains(needle), "{needle}");
        }
        assert!(html.contains("A guest pass works for the first person who uses it, until it expires."));
        assert!(!html.contains("works once"));
    }

    #[test]
    fn the_json_lists_invites_with_links_for_live_ones() {
        let snapshot = with_invites(vec![view("guest", "live", "a"), view("personal", "used", "b")]);
        let json = status_json(&snapshot);
        assert_eq!(json["invites_enabled"], true);
        assert_eq!(json["invites"][0]["link"], "stampede://keep/x/y");
        assert_eq!(json["invites"][0]["kind"], "guest");
        assert_eq!(json["invites"][1]["link"], serde_json::Value::Null);
        assert_eq!(json["invites"][1]["status"], "used");
        assert_eq!(json["invites"][1]["label"], "b");
    }

    #[test]
    fn a_guest_pass_post_asks_for_one() {
        let snapshot = with_invites(vec![]);
        let (response, action) = respond(&post("/invites/guest", "", "label=Cousin+Bo%21&junk=1"), &snapshot, None);
        assert_eq!(response.status, 303);
        assert_eq!(action, Some(Action::NewGuest { label: Some("Cousin Bo!".into()) }));
        let (_, action) = respond(&post("/invites/guest", "", ""), &snapshot, None);
        assert_eq!(action, Some(Action::NewGuest { label: None }));
    }

    #[test]
    fn a_personal_invite_post_checks_the_persona_and_the_days() {
        let snapshot = with_invites(vec![]);
        let key = nostr::Keys::generate().public_key();
        let npub = nostr::nips::nip19::ToBech32::to_bech32(&key).unwrap();
        let ask = |body: &str| respond(&post("/invites/personal", "", body), &snapshot, None);
        let (response, action) = ask(&format!("persona={npub}&label=Ada&days=7"));
        assert_eq!(response.status, 303);
        assert_eq!(
            action,
            Some(Action::NewPersonal { holder: key.to_hex(), label: Some("Ada".into()), days: 7 })
        );
        let (_, action) = ask(&format!("persona=%20{}%20", key.to_hex()));
        assert_eq!(
            action,
            Some(Action::NewPersonal { holder: key.to_hex(), label: None, days: 30 }),
            "hex works and days default to 30"
        );
        for bad in ["persona=", "persona=npub1nonsense", "label=x", &format!("persona={npub}&days=0"),
            &format!("persona={npub}&days=366"), &format!("persona={npub}&days=x"), &format!("persona={npub}&days=-1")] {
            let (response, action) = ask(bad);
            assert_eq!((response.status, action), (400, None), "{bad}");
            assert!(text(&response).contains("Back to invites"));
        }
    }

    #[test]
    fn a_revoke_post_needs_a_well_formed_id() {
        let snapshot = with_invites(vec![]);
        let id = "0123456789abcdef0123456789abcdef";
        let (response, action) = respond(&post(&format!("/invites/{id}/revoke"), "", "from=invites"), &snapshot, None);
        assert_eq!(response.status, 303);
        assert_eq!(action, Some(Action::Revoke { id: id.into(), back: "/invites" }));
        let (_, action) = respond(&post(&format!("/invites/{id}/revoke"), "", ""), &snapshot, None);
        assert_eq!(action, Some(Action::Revoke { id: id.into(), back: "/" }));
        for path in ["/invites/short/revoke", &format!("/invites/{id}z/revoke"), &format!("/invites/{id}"),
            "/invites/", "/invites/guest/revoke", &format!("/invites/{id}/revoke/x"), "/invites/other"] {
            let (response, action) = respond(&post(path, "", ""), &snapshot, None);
            assert_eq!((response.status, action), (404, None), "{path}");
        }
        assert_eq!(get(&format!("/invites/{id}/revoke"), &snapshot).0.status, 404);
        assert_eq!(get("/invites/guest", &snapshot).0.status, 404);
    }

    #[test]
    fn invite_posts_from_another_site_or_name_are_refused() {
        let snapshot = with_invites(vec![]);
        let id = "0123456789abcdef0123456789abcdef";
        let persona = format!("persona={}", "05".repeat(32));
        let routes = [
            ("/invites/guest".to_string(), String::new()),
            ("/invites/personal".to_string(), persona),
            (format!("/invites/{id}/revoke"), String::new()),
        ];
        for (path, body) in &routes {
            for origin in ["http://evil.example", "null", "https://192.168.1.9:8780", "http://192.168.1.9:8781"] {
                let request = post(path, &format!("Origin: {origin}\r\n"), body);
                let (response, action) = respond(&request, &snapshot, None);
                assert_eq!((response.status, action), (403, None), "{path} {origin}");
            }
            // The page's own origin and a script with none are fine.
            for headers in ["Origin: http://192.168.1.9:8780\r\n", ""] {
                let (response, action) = respond(&post(path, headers, body), &snapshot, None);
                assert_eq!(response.status, 303, "{path}");
                assert!(action.is_some());
            }
            // Under another name (DNS rebinding) nothing is answered.
            let mut rebound = post(path, "Origin: http://evil.example\r\n", body);
            rebound.host = Some("evil.example".into());
            rebound.origin = Some("http://evil.example".into());
            let (response, action) = respond(&rebound, &snapshot, None);
            assert_eq!((response.status, action), (421, None), "{path}");
            let mut no_host = post(path, "", body);
            no_host.host = None;
            assert_eq!(respond(&no_host, &snapshot, None).0.status, 421);
        }
        let mut rebound = head("GET /invites HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        assert_eq!(respond(&rebound, &snapshot, None).0.status, 421);
        rebound.host = Some("keep.home.arpa".into());
        assert_eq!(respond(&rebound, &snapshot, Some("keep.home.arpa")).0.status, 200);
    }

    #[test]
    fn form_bodies_decode() {
        let form = parse_form(b"a=1&b=x+y%20z&c&=d&e=%zz%4&f=%C3%A9&&g=%41%42");
        let get = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("a"), Some("1"));
        assert_eq!(get("b"), Some("x y z"));
        assert_eq!(get("c"), Some(""));
        assert_eq!(get(""), Some("d"));
        assert_eq!(get("e"), Some("%zz%4"));
        assert_eq!(get("f"), Some("é"));
        assert_eq!(get("g"), Some("AB"));
        assert!(parse_form(b"").is_empty());
        assert!(!parse_form(b"x=\xff\xfe").is_empty());
    }

    #[test]
    fn a_body_is_read_after_the_head_in_pieces() {
        let mut input = Cursor::new(b"POST /x HTTP/1.1\r\nContent-Length: 10\r\n\r\nabcdefghij".to_vec());
        let (head, rest) = read_head_and_rest(&mut input, after(5)).unwrap();
        assert!(head.ends_with(b"\r\n\r\n") && rest == b"abcdefghij");
        let body = read_body(&mut input, rest, 10, after(5)).unwrap();
        assert_eq!(body, b"abcdefghij");
        // Part came with the head, the rest follows.
        assert_eq!(read_body(&mut Cursor::new(b"fghij".to_vec()), b"abcde".to_vec(), 10, after(5)).unwrap(), b"abcdefghij");
        // More than the length is cut; less is an error.
        assert_eq!(read_body(&mut Cursor::new(Vec::new()), b"abcdefg".to_vec(), 3, after(5)).unwrap(), b"abc");
        assert_eq!(read_body(&mut Cursor::new(b"ab".to_vec()), Vec::new(), 10, after(5)), Err(400));
        assert_eq!(read_body(&mut Cursor::new(Vec::new()), Vec::new(), 1, Instant::now()), Err(408));
        let request = parse_request(b"POST /x HTTP/1.1\r\nContent-Length: 12\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        assert_eq!((request.content_length, request.chunked), (12, true));
        for bad in ["Content-Length: -1", "Content-Length: x", "Content-Length: ", "Content-Length: 1 2"] {
            let text = format!("POST /x HTTP/1.1\r\n{bad}\r\n\r\n");
            assert_eq!(parse_request(text.as_bytes()).err(), Some(400), "{bad}");
        }
        let twice = b"POST /x HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n";
        assert_eq!(parse_request(twice).err(), Some(400));
    }

    fn parse_id_for_test(id: &str) -> race_session::InviteId {
        invites::parse_id(id).unwrap()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("status-{name}-{}-{}", std::process::id(), unix_now()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn invites_are_made_shown_after_a_redirect_listed_and_revoked_over_tcp() {
        let dir = scratch("tcp");
        let table = Arc::new(Invites::open(&dir, HOST_KEY).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::new(sample(), None).with_invites(table.clone()));
        serve_on(listener, shared, Duration::from_millis(400)).unwrap();
        let send = |head: &str, body: &str| {
            ask(address, format!("{head}Host: 127.0.0.1:8780\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
        };

        // A guest pass: the post answers with a redirect, and the page it
        // names holds the link in a read-only box, with no reload.
        let made = send("POST /invites/guest HTTP/1.1\r\n", "label=Cousin+%3Cb%3EBo");
        assert!(made.starts_with("HTTP/1.1 303 See Other\r\n"), "{made}");
        assert!(!made.contains("textarea") && !made.contains("stampede://"), "the post itself carries no link");
        let location = made.lines().find_map(|l| l.strip_prefix("Location: ")).unwrap().to_string();
        let id = location.strip_prefix("/invites/new/").unwrap().to_string();
        assert_eq!(id.len(), 32, "{location}");
        let link = format!("stampede://keep/{}/", "07".repeat(32));
        let shown = send(&format!("GET {location} HTTP/1.1\r\n"), "");
        assert!(shown.starts_with("HTTP/1.1 200 OK\r\n"), "{shown}");
        assert!(shown.contains(&format!("<textarea readonly rows=5 spellcheck=false>{link}")));
        assert!(shown.contains("A guest pass works for the first person who uses it, until it expires."));
        assert!(shown.contains("Anyone on your home network who opens this page can see live invite links."));
        assert!(!shown.contains("only now") && !shown.contains("http-equiv=refresh"));
        let secret = shown.split(&link).nth(1).unwrap().split('<').next().unwrap().to_string();
        assert_eq!(secret.len(), 64);

        // A refresh (or Back) shows the same invite and makes no other.
        let again = send(&format!("GET {location} HTTP/1.1\r\n"), "");
        assert!(again.contains(&format!("{link}{secret}")), "{again}");
        assert_eq!(table.views(unix_now()).len(), 1);
        assert_eq!(table.live(unix_now()).len(), 1);

        // The list shows it without its link; the json carries the link.
        let page = send("GET /invites HTTP/1.1\r\n", "");
        assert!(page.contains("Cousin &lt;b&gt;Bo") && !page.contains(&secret));
        let json = send("GET /status.json HTTP/1.1\r\n", "");
        let value: serde_json::Value = serde_json::from_str(json.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(value["invites"][0]["link"], format!("{link}{secret}"));
        assert_eq!(value["invites"][0]["status"], "live");
        assert_eq!(value["invites"][0]["usage"], "unused");
        assert_eq!(value["invites"][0]["id"], id.as_str());

        // Once someone has used the guest pass it is theirs: it stays live and
        // in /status.json, and the page that shows a new link no longer does.
        table.admit(parse_id_for_test(&id), &[9; 32], unix_now()).unwrap();
        let used = send(&format!("GET {location} HTTP/1.1\r\n"), "");
        assert!(used.starts_with("HTTP/1.1 404") && !used.contains(&secret), "{used}");
        assert!(used.contains("isn&#39;t shown here any more"), "{used}");
        let json = send("GET /status.json HTTP/1.1\r\n", "");
        let value: serde_json::Value = serde_json::from_str(json.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!((value["invites"][0]["status"].as_str(), value["invites"][0]["usage"].as_str()), (Some("live"), Some("used")));
        assert_eq!(value["invites"][0]["link"], format!("{link}{secret}"));
        let list = send("GET /invites HTTP/1.1\r\n", "");
        assert!(list.contains("&middot; used by <code>npub1"), "{list}");
        assert_eq!(table.live(unix_now()).len(), 1);

        // Nothing is shown for an id that is not there, or is not an id.
        for bad in ["/invites/new/00000000000000000000000000000000", "/invites/new/", "/invites/new/x", "/invites/new/../guest"] {
            let missing = send(&format!("GET {bad} HTTP/1.1\r\n"), "");
            assert!(missing.starts_with("HTTP/1.1 404"), "{bad}: {missing}");
        }
        let rebound = ask(address, format!("GET {location} HTTP/1.1\r\nHost: evil.example\r\n\r\n").as_bytes());
        assert!(rebound.starts_with("HTTP/1.1 421"), "{rebound}");

        // A personal invite: a redirect too, and its page shows the link while it is live,
        // however often the person has used it. A bad one makes nothing.
        let key = nostr::Keys::generate().public_key();
        let personal = send("POST /invites/personal HTTP/1.1\r\n", &format!("persona={}&days=5", key.to_hex()));
        assert!(personal.starts_with("HTTP/1.1 303 See Other\r\n"), "{personal}");
        let personal_at = personal.lines().find_map(|l| l.strip_prefix("Location: ")).unwrap().to_string();
        let personal_id = personal_at.strip_prefix("/invites/new/").unwrap().to_string();
        let shown = send(&format!("GET {personal_at} HTTP/1.1\r\n"), "");
        assert!(shown.contains("New personal invite") && shown.contains(&link), "{shown}");
        table.admit(parse_id_for_test(&personal_id), &key.to_bytes(), unix_now()).unwrap();
        assert!(send(&format!("GET {personal_at} HTTP/1.1\r\n"), "").contains("New personal invite"));
        let bad = send("POST /invites/personal HTTP/1.1\r\n", "persona=nope");
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
        assert_eq!(table.views(unix_now()).len(), 2);

        // A forged post from another site changes nothing.
        let forged = ask(address, format!("POST /invites/{id}/revoke HTTP/1.1\r\nHost: 127.0.0.1:8780\r\nOrigin: http://evil.example\r\nContent-Length: 0\r\n\r\n").as_bytes());
        assert!(forged.starts_with("HTTP/1.1 403"), "{forged}");
        assert_eq!(table.live(unix_now()).len(), 2);

        // Revoke, and the room is told to drop its players.
        let revoked = send(&format!("POST /invites/{id}/revoke HTTP/1.1\r\n"), "from=invites");
        assert!(revoked.starts_with("HTTP/1.1 303 See Other\r\n") && revoked.contains("Location: /invites\r\n"), "{revoked}");
        assert_eq!(table.live(unix_now()).len(), 1);
        assert_eq!(table.take_revoked().len(), 1);
        let json = send("GET /status.json HTTP/1.1\r\n", "");
        let value: serde_json::Value = serde_json::from_str(json.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let revoked_one = value["invites"].as_array().unwrap().iter().find(|i| i["id"] == id.as_str()).unwrap();
        assert_eq!((revoked_one["status"].as_str(), revoked_one["link"].is_null()), (Some("revoked"), true));
        assert!(send(&format!("POST /invites/{id}/revoke HTTP/1.1\r\n"), "").starts_with("HTTP/1.1 303"));
        assert!(send("POST /invites/00000000000000000000000000000000/revoke HTTP/1.1\r\n", "").starts_with("HTTP/1.1 404"));

        // Bodies are bounded.
        let big = format!("label={}", "a".repeat(MAX_BODY));
        assert!(send("POST /invites/guest HTTP/1.1\r\n", &big).starts_with("HTTP/1.1 413"));
        let chunked = ask(address, b"POST /invites/guest HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n");
        assert!(chunked.starts_with("HTTP/1.1 411"), "{chunked}");
        let short = ask(address, b"POST /invites/guest HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 50\r\n\r\nlabel=x");
        assert!(short.starts_with("HTTP/1.1 408"), "{short}");
        assert_eq!(table.views(unix_now()).len(), 2, "none of those made an invite");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_keep_whose_invites_are_off_says_so_in_red_and_still_shows_the_code() {
        let reason = "invites.json couldn't be read: not valid JSON: <x>";
        let off = Snapshot { invites_problem: Some(reason.into()), ..sample() };
        let html = text(&get("/", &off).0);
        assert!(html.contains(
            "<p class=bad>Invites are off: invites.json couldn&#39;t be read: not valid JSON: &lt;x&gt;. \
Fix or move the file and restart.</p>"
        ), "{html}");
        assert!(html.contains("class=code>KWT QPM<") && html.contains("Invites are off for this run."));
        assert!(!html.contains("Invites need --online") && !html.contains("<x>"));
        let json = status_json(&off);
        assert_eq!((json["invites_enabled"].as_bool(), json["invites_problem"].as_str()), (Some(false), Some(reason)));
        assert_eq!(json["code"], "KWTQPM");
        assert!(status_json(&sample())["invites_problem"].is_null());
        assert!(!text(&get("/", &sample()).0).contains("Invites are off:"));
        // None of the invite routes work.
        assert_eq!(get("/invites", &off).0.status, 404);
        assert_eq!(get("/invites/new/0123456789abcdef0123456789abcdef", &off).0.status, 404);
        assert_eq!(respond(&post("/invites/guest", "", ""), &off, None).0.status, 404);
    }

    #[test]
    fn the_server_puts_the_reason_invites_are_off_on_the_page() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::new(sample(), None).with_invites_problem("identity.key couldn't be read: x".into()));
        serve_on(listener, shared, Duration::from_millis(400)).unwrap();
        let page = ask(address, b"GET / HTTP/1.1\r\nHost: 127.0.0.1:8780\r\n\r\n");
        assert!(page.contains("Invites are off: identity.key couldn&#39;t be read: x. Fix or move the file and restart."), "{page}");
        let json = ask(address, b"GET /status.json HTTP/1.1\r\nHost: 127.0.0.1:8780\r\n\r\n");
        let value: serde_json::Value = serde_json::from_str(json.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!((value["code"].as_str(), value["invites_enabled"].as_bool()), (Some("KWTQPM"), Some(false)));
        assert_eq!(value["invites_problem"], "identity.key couldn't be read: x");
        let made = ask(address, b"POST /invites/guest HTTP/1.1\r\nHost: 127.0.0.1:8780\r\nContent-Length: 0\r\n\r\n");
        assert!(made.starts_with("HTTP/1.1 404"), "{made}");
    }

    #[test]
    fn an_invite_that_cannot_be_saved_says_so_and_makes_nothing() {
        let dir = scratch("unsaved");
        let table = Arc::new(Invites::open(&dir, HOST_KEY).unwrap());
        let shared = Shared::new(sample(), None).with_invites(table.clone());
        std::fs::remove_dir_all(&dir).unwrap();
        let response = apply(Action::NewGuest { label: None }, &shared).unwrap();
        assert_eq!(response.status, 500);
        assert!(text(&response).contains("Cannot write"));
        assert!(table.views(unix_now()).is_empty());
    }

    // ─── the owner ──────────────────────────────────────────────────────────

    fn claimed_snapshot(list: Vec<InviteView>) -> Snapshot {
        Snapshot { owner_claimed: true, ..with_invites(list) }
    }

    #[test]
    fn once_claimed_the_page_makes_no_invites_and_refuses_the_urls_that_do() {
        let snapshot = claimed_snapshot(vec![view("personal", "live", "Ada")]);
        for (path, body) in [
            ("/invites/guest", "label=x"),
            ("/invites/personal", "persona=npub&days=3"),
            ("/invites/0123456789abcdef0123456789abcdef/revoke", "from=main"),
        ] {
            let (response, action) = respond(&post(path, "", body), &snapshot, None);
            assert_eq!(response.status, 403, "{path}");
            assert!(action.is_none(), "{path}");
        }
        let (response, _) = respond(&head("GET /invites/new/0123456789abcdef0123456789abcdef HTTP/1.1\r\nHost: 192.168.1.9\r\n\r\n"), &snapshot, None);
        assert_eq!(response.status, 403);
        // The same posts are fine before the claim.
        let open = with_invites(vec![]);
        assert_eq!(respond(&post("/invites/guest", "", "label=x"), &open, None).0.status, 303);
        // New code stays.
        let (response, action) = respond(&post("/new-code", "", ""), &snapshot, None);
        assert_eq!((response.status, action), (303, Some(Action::NewCode)));
    }

    #[test]
    fn once_claimed_the_pages_list_invites_read_only() {
        let snapshot = claimed_snapshot(vec![view("personal", "live", "Ada"), used_guest("Pass")]);
        let main = text(&get("/", &snapshot).0);
        let list = text(&get("/invites", &snapshot).0);
        for page in [&main, &list] {
            assert!(page.contains("Ada") && page.contains("Pass"), "the list stays");
            assert!(!page.contains("Revoke") && !page.contains("action=/invites") && !page.contains("<form method=post action=/invites"), "{page}");
            assert!(!page.contains("New guest pass") && !page.contains("New personal invite") && !page.contains("More invite options"));
            assert!(page.contains("made and cancelled in My Signet"));
        }
        assert!(main.contains("This Keep has an owner"));
        assert!(main.contains("action=/new-code"), "New code stays");
        // Before the claim the forms and Revoke are there.
        let before = text(&get("/invites", &with_invites(vec![view("personal", "live", "Ada")])).0);
        assert!(before.contains("New guest pass") && before.contains("Revoke"));
        assert_eq!(status_json(&snapshot)["owner_claimed"], true);
    }

    #[test]
    fn the_pairing_link_is_on_the_page_only_when_given_and_never_in_the_json() {
        let link = "signet-keep:pair?k=aa&c=bb&n=Home&r=wss%3A%2F%2Fr.example";
        let shown = Snapshot { owner_open: true, pairing: Some(link.into()), ..sample() };
        let page = text(&get("/", &shown).0);
        assert!(page.contains("Make this your Keep") && page.contains(&html_escape(link)), "{page}");
        let json = status_json(&shown).to_string();
        assert!(!json.contains("signet-keep") && !json.contains("pair?"), "{json}");
        // Not from this machine: no link, and a hint where to look.
        let remote = Snapshot { owner_open: true, ..sample() };
        let page = text(&get("/", &remote).0);
        assert!(!page.contains("signet-keep") && page.contains("shown on the Keep's own computer, and in the Keep's log"), "{page}");
        // A Keep that cannot be claimed (no owner service) says nothing about it.
        assert!(!text(&get("/", &sample()).0).contains("Make this your Keep"));
        // Unreadable owner files fail closed.
        let broken = Snapshot { owner_claimed: true, owner_problem: Some("owner.json couldn't be read: x".into()), ..sample() };
        assert!(text(&get("/", &broken).0).contains("The owner can't be read: owner.json couldn&#39;t be read: x"));
    }

    /// An address of this machine that is not loopback, if it has one.
    fn lan_address() -> Option<std::net::IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        let ip = socket.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    #[test]
    fn a_page_request_read_before_the_claim_makes_no_invite_after_it() {
        let dir = scratch("owner-race");
        let keep = nostr::Keys::parse(&format!("{}1", "0".repeat(63))).unwrap();
        let table = Arc::new(Invites::open(&dir, keep.public_key().to_bytes()).unwrap());
        let owner = Arc::new(Owner::open(&dir, keep.clone(), table.clone(), "Home", &["wss://relay.example".to_string()]).unwrap());
        let link = owner.pairing_link().unwrap();
        let shared = Shared::new(sample(), None).with_invites(table.clone()).with_owner(owner.clone());
        // Before the claim the page makes one.
        assert!(apply(Action::NewGuest { label: None }, &shared).is_some());
        assert_eq!(table.views(unix_now()).len(), 1);
        let code = link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let tags = vec![nostr::Tag::parse(["keep", &keep.public_key().to_hex()]).unwrap(), nostr::Tag::parse(["code", &code]).unwrap()];
        let claim = crate::wire::gift_wrap(&nostr::Keys::generate(), &keep.public_key(), crate::owner::KIND_CLAIM, tags, unix_now()).unwrap();
        assert!(owner.process(&claim, unix_now()).is_some());
        // A request that was read as unclaimed and applied after the claim: refused.
        for action in [
            Action::NewGuest { label: None },
            Action::NewPersonal { holder: nostr::Keys::generate().public_key().to_hex(), label: None, days: 1 },
            Action::Revoke { id: table.views(unix_now())[0].id.clone(), back: "/invites" },
        ] {
            let answer = apply(action, &shared).expect("an answer");
            assert_eq!(answer.status, 403);
        }
        assert_eq!(table.views(unix_now()).len(), 1, "nothing was made");
        assert_eq!(table.views(unix_now())[0].status, "live", "nothing was cancelled");
    }

    #[test]
    fn the_server_gives_the_pairing_link_to_this_machine_only_and_closes_it_at_the_claim() {
        let dir = scratch("owner");
        let keep = nostr::Keys::parse(&format!("{}1", "0".repeat(63))).unwrap();
        let table = Arc::new(Invites::open(&dir, keep.public_key().to_bytes()).unwrap());
        let owner = Arc::new(Owner::open(&dir, keep.clone(), table.clone(), "Home", &["wss://relay.example".to_string()]).unwrap());
        let link = owner.pairing_link().unwrap();
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared::new(sample(), None).with_invites(table.clone()).with_owner(owner.clone()));
        serve_on(listener, shared, Duration::from_millis(400)).unwrap();
        let get_from = |ip: std::net::IpAddr, path: &str| {
            ask(std::net::SocketAddr::new(ip, port), format!("GET {path} HTTP/1.1\r\nHost: {ip}:{port}\r\n\r\n").as_bytes())
        };
        let local = std::net::IpAddr::from([127, 0, 0, 1]);
        let page = get_from(local, "/");
        assert!(page.contains(&html_escape(&link)), "{page}");
        let json = get_from(local, "/status.json");
        assert!(!json.contains("signet-keep"), "{json}");
        // From the network: the page, with no link.
        match lan_address() {
            Some(ip) => {
                let page = get_from(ip, "/");
                assert!(page.starts_with("HTTP/1.1 200") && !page.contains("signet-keep") && page.contains("in the Keep's log"), "{page}");
            }
            None => eprintln!("no non-loopback address here: the network half of this test is skipped"),
        }
        // The claim closes it, and the page stops making invites.
        let code = link.split("&c=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let tags = vec![
            nostr::Tag::parse(["keep", &keep.public_key().to_hex()]).unwrap(),
            nostr::Tag::parse(["code", &code]).unwrap(),
        ];
        let claim = crate::wire::gift_wrap(&nostr::Keys::generate(), &keep.public_key(), crate::owner::KIND_CLAIM, tags, unix_now()).unwrap();
        assert!(owner.process(&claim, unix_now()).is_some());
        let page = get_from(local, "/");
        assert!(!page.contains("signet-keep") && page.contains("This Keep has an owner"), "{page}");
        let denied = ask(
            std::net::SocketAddr::new(local, port),
            format!("POST /invites/guest HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\n\r\n").as_bytes(),
        );
        assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
        assert!(table.views(unix_now()).is_empty());
    }
}
