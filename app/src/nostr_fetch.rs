//! A one-shot read of stored events from nostr relays (the update check and the
//! More games screen). Stampede's stand-in for Twang Thang's
//! `decented_signet::relay::fetch_live`, without the Signet crate.
//!
//! Every URL is dialled at once, on this module's own small runtime. Each relay
//! gets one REQ, and the read ends when every relay has sent EOSE or failed, or
//! at the timeout. Every connect, failure and response time goes to the shared
//! [`RelayPool`], so the health dots in Settings show what these checks saw.
//! An event is kept only if its id and signature verify and it matches one of
//! the filters (kind, author, `#d`), as Twang's reader does, and each relay has
//! its own small budget (message size, kept events, frames inspected), so one
//! relay cannot fill the shared cap or the app's memory and hide another
//! relay's copy. The caller still checks the signer and the event's contents
//! (`update::parse_release`).

use decented_relays::RelayPool;
use futures_util::{SinkExt, StreamExt};
use nostr::util::JsonUtil;
use nostr::filter::MatchEventOptions;
use nostr::{Event, Filter};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// How long one relay gets to accept the connection.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest websocket message accepted from a relay (the transport limit).
const MAX_WS_MESSAGE: usize = 256 * 1024;
/// Largest EVENT frame inspected at all. A release event is a few KiB; anything
/// bigger is dropped before it is parsed.
const MAX_EVENT_BYTES: usize = 64 * 1024;
/// Events kept from one relay (verified and matching the filter). Each relay
/// has its own budget, so one cannot use up the shared cap.
const MAX_PER_RELAY: usize = 32;
/// Frames inspected from one relay before it is hung up on (junk costs time,
/// not memory: it is never kept).
const MAX_FRAMES_PER_RELAY: usize = 2000;
/// Events kept from one read, in all, after the per-relay budgets are merged.
const MAX_EVENTS: usize = 200;
const SUB_ID: &str = "stampede-fetch";

/// What a read of the relays found, and how many of them really answered.
#[derive(Debug, Clone, Default)]
pub struct FetchReport {
    /// Stored events matching the filters, deduplicated by id, newest first.
    pub events: Vec<Event>,
    /// Relays that answered (sent EOSE themselves, or sent an event we kept).
    /// A `CLOSED` is a refusal, not an answer.
    pub answered: usize,
    /// Relays asked.
    pub total: usize,
}

impl FetchReport {
    /// At least one relay answered. With no events, the answer was "nothing
    /// published"; without it, the relays could not be reached.
    pub fn reached(&self) -> bool {
        self.answered > 0
    }
}

/// The result of a background operation, delivered once.
pub struct Task<T> {
    rx: Mutex<mpsc::Receiver<T>>,
}

impl<T> std::fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Task")
    }
}

impl<T: Send + 'static> Task<T> {
    /// The result, once (never blocks).
    pub fn poll(&self) -> Option<T> {
        self.rx.lock().ok()?.try_recv().ok()
    }

    /// Block until the result arrives or `timeout` passes (tests; the frame loop uses [`Task::poll`]).
    pub fn wait(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = self.poll() {
                return Some(v);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A task that is already done (tests).
    pub fn ready(value: T) -> Task<T> {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(value);
        Task { rx: Mutex::new(rx) }
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("stampede-fetch").enable_all().build().expect("stampede: could not start the relay-reader runtime"))
}

/// Read `filters` from `urls` (all at once) on the process-wide pool, then hang up.
pub fn fetch_live(urls: Vec<String>, filters: Vec<Filter>, timeout: Duration) -> Task<FetchReport> {
    let (tx, rx) = mpsc::channel();
    runtime().spawn(async move {
        let report = fetch(RelayPool::global(), urls, filters, timeout).await;
        let _ = tx.send(report);
    });
    Task { rx: Mutex::new(rx) }
}

/// What one relay's task reports back.
struct One {
    events: Vec<Event>,
    answered: bool,
}

/// [`fetch_live`] on a pool of the caller's (tests use their own).
pub async fn fetch(pool: std::sync::Arc<RelayPool>, urls: Vec<String>, filters: Vec<Filter>, timeout: Duration) -> FetchReport {
    // rustls needs a process-wide crypto provider for wss://; installing one fails harmlessly when another crate did.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let total = urls.len();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut set = tokio::task::JoinSet::new();
    for url in urls {
        let (pool, filters) = (pool.clone(), filters.clone());
        set.spawn(async move { one(pool, url, filters, deadline).await });
    }
    let mut per_relay: Vec<Vec<Event>> = Vec::new();
    let mut answered = 0;
    while let Some(r) = set.join_next().await {
        let Ok(r) = r else { continue };
        answered += usize::from(r.answered);
        per_relay.push(r.events);
    }
    FetchReport { events: merge(per_relay), answered, total }
}

/// Merge each relay's kept events: deduplicated by id, newest first, then cut
/// to [`MAX_EVENTS`] (so the cap drops the oldest, never a whole relay's answer).
fn merge(per_relay: Vec<Vec<Event>>) -> Vec<Event> {
    let mut events: Vec<Event> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for ev in per_relay.into_iter().flatten() {
        if seen.insert(ev.id) {
            events.push(ev);
        }
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    events.truncate(MAX_EVENTS);
    events
}

enum Frame {
    Event(Box<Event>),
    Eose,
    Closed,
    Other,
}

/// Classify one relay frame for our subscription. An oversized frame, or one
/// that is not an event, is `Other` (dropped).
fn frame_kind(text: &str) -> Frame {
    // Checked before any parsing, so a big frame costs nothing.
    if text.len() > MAX_EVENT_BYTES {
        return Frame::Other;
    }
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(text) else { return Frame::Other };
    let ours = arr.get(1).and_then(|v| v.as_str()) == Some(SUB_ID);
    match arr.first().and_then(|v| v.as_str()) {
        Some("EVENT") if ours => {
            let Some(v) = arr.get(2) else { return Frame::Other };
            serde_json::from_value::<Event>(v.clone()).map_or(Frame::Other, |e| Frame::Event(Box::new(e)))
        }
        Some("EOSE") if ours => Frame::Eose,
        Some("CLOSED") => Frame::Closed,
        _ => Frame::Other,
    }
}

/// Keep an event only if it is new for this relay, its id and signature verify,
/// and it matches one of our filters (kind, author, `#d`).
fn keep(have: &[Event], ev: &Event, filters: &[Filter]) -> bool {
    !have.iter().any(|e| e.id == ev.id) && ev.verify().is_ok() && filters.iter().any(|f| f.match_event(ev, MatchEventOptions::new()))
}

async fn one(pool: std::sync::Arc<RelayPool>, url: String, filters: Vec<Filter>, deadline: tokio::time::Instant) -> One {
    let mut out = One { events: Vec::new(), answered: false };
    let cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(MAX_WS_MESSAGE)).max_frame_size(Some(MAX_WS_MESSAGE));
    let started = Instant::now();
    let dial = tokio_tungstenite::connect_async_with_config(url.clone(), Some(cfg), false);
    let dial_end = deadline.min(tokio::time::Instant::now() + DIAL_TIMEOUT);
    let mut ws = match tokio::time::timeout_at(dial_end, dial).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            pool.record_failure(&url, &e.to_string().chars().take(120).collect::<String>());
            return out;
        }
        Err(_) => {
            pool.record_failure(&url, "timed out");
            return out;
        }
    };
    pool.record_success(&url, Some(started.elapsed()));
    // ["REQ", sub, filter, filter...]
    let mut req = vec![serde_json::json!("REQ"), serde_json::json!(SUB_ID)];
    for f in &filters {
        match serde_json::from_str::<serde_json::Value>(&f.try_as_json().unwrap_or_default()) {
            Ok(v) => req.push(v),
            Err(_) => return out,
        }
    }
    if tokio::time::timeout_at(deadline, ws.send(Message::Text(serde_json::Value::Array(req).to_string().into()))).await.is_err() {
        return out;
    }
    let mut frames = 0usize;
    loop {
        let msg = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        let Message::Text(text) = msg else { continue };
        frames += 1;
        if frames > MAX_FRAMES_PER_RELAY {
            break;
        }
        match frame_kind(&text) {
            Frame::Event(ev) => {
                if keep(&out.events, &ev, &filters) && out.events.len() < MAX_PER_RELAY {
                    out.answered = true;
                    out.events.push(*ev);
                }
            }
            Frame::Eose => {
                out.answered = true;
                break;
            }
            // A refusal is not an answer.
            Frame::Closed => break,
            Frame::Other => {}
        }
    }
    let _ = ws.close(None).await;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind};
    use tokio::net::TcpListener;

    /// How a fake relay ends its answer.
    #[derive(Clone, Copy)]
    enum Tail {
        Eose,
        Closed,
    }

    /// A relay on localhost that answers any REQ with `events` then EOSE.
    async fn fake_relay(events: Vec<Event>) -> String {
        fake_relay_raw(events.iter().map(|e| serde_json::to_value(e).unwrap()).collect(), Tail::Eose).await
    }

    /// A relay on localhost that answers any REQ with these event objects
    /// (possibly forged), then the `tail`.
    async fn fake_relay_raw(events: Vec<serde_json::Value>, tail: Tail) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let events = events.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                    while let Some(Ok(Message::Text(t))) = ws.next().await {
                        let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                        if v[0] == "REQ" {
                            for e in &events {
                                let f = serde_json::json!(["EVENT", v[1], e]);
                                if ws.send(Message::Text(f.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            let end = match tail {
                                Tail::Eose => serde_json::json!(["EOSE", v[1]]),
                                Tail::Closed => serde_json::json!(["CLOSED", v[1], "blocked: nope"]),
                            };
                            let _ = ws.send(Message::Text(end.to_string().into())).await;
                        }
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn reads_events_dedups_and_reports_health() {
        rt().block_on(async {
            let keys = Keys::generate();
            let ev = EventBuilder::new(Kind::Custom(30063), "hi").sign_with_keys(&keys).unwrap();
            let a = fake_relay(vec![ev.clone()]).await;
            let b = fake_relay(vec![ev.clone()]).await;
            let silent = fake_relay(vec![]).await;
            let pool = std::sync::Arc::new(RelayPool::new());
            let r = fetch(pool.clone(), vec![a.clone(), b, silent], vec![Filter::new().kind(Kind::Custom(30063))], Duration::from_secs(5)).await;
            assert_eq!((r.events.len(), r.answered, r.total), (1, 3, 3), "{r:?}");
            assert!(r.reached());
            assert_eq!(pool.view(&a).dot, decented_relays::Dot::Green);
        });
    }

    #[test]
    fn an_unreachable_relay_is_not_an_answer_and_turns_red() {
        rt().block_on(async {
            // Bind and drop: the port refuses connections.
            let dead = {
                let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                format!("ws://{}", l.local_addr().unwrap())
            };
            let pool = std::sync::Arc::new(RelayPool::new());
            let r = fetch(pool.clone(), vec![dead.clone()], vec![Filter::new().kind(Kind::Custom(30063))], Duration::from_secs(3)).await;
            assert_eq!((r.events.len(), r.answered, r.total), (0, 0, 1));
            assert!(!r.reached());
            assert_eq!(pool.view(&dead).dot, decented_relays::Dot::Red);
            assert!(pool.view(&dead).last_error.is_some());
        });
    }

    const KIND: u16 = 30063;

    fn release_filter(author: &Keys) -> Vec<Filter> {
        vec![Filter::new().kind(Kind::Custom(KIND)).author(author.public_key()).identifier("stampede")]
    }

    fn signed(keys: &Keys, d: &str, content: &str, at: u64) -> Event {
        EventBuilder::new(Kind::Custom(KIND), content).tag(nostr::Tag::identifier(d)).custom_created_at(nostr::Timestamp::from(at)).sign_with_keys(keys).unwrap()
    }

    async fn read(urls: Vec<String>, filters: Vec<Filter>) -> FetchReport {
        fetch(std::sync::Arc::new(RelayPool::new()), urls, filters, Duration::from_secs(5)).await
    }

    #[test]
    fn forged_wrong_author_wrong_d_and_oversized_events_are_dropped() {
        rt().block_on(async {
            let keys = Keys::generate();
            let other = Keys::generate();
            let good = signed(&keys, "stampede", "real", 100);

            // Tampered content: the id no longer matches.
            let mut bad_id = serde_json::to_value(signed(&keys, "stampede", "real", 101)).unwrap();
            bad_id["content"] = "forged".into();
            // A signature lifted from another event: id fine, signature wrong.
            let mut bad_sig = serde_json::to_value(signed(&keys, "stampede", "real", 102)).unwrap();
            bad_sig["sig"] = serde_json::to_value(signed(&other, "stampede", "x", 1)).unwrap()["sig"].clone();
            let wrong_author = signed(&other, "stampede", "real", 103);
            let wrong_d = signed(&keys, "something-else", "real", 104);
            let big = signed(&keys, "stampede", &"x".repeat(MAX_EVENT_BYTES + 1024), 105);
            let wrong_kind = EventBuilder::new(Kind::TextNote, "real").tag(nostr::Tag::identifier("stampede")).sign_with_keys(&keys).unwrap();

            let frames = vec![bad_id, bad_sig, serde_json::to_value(&wrong_author).unwrap(), serde_json::to_value(&wrong_d).unwrap(), serde_json::to_value(&big).unwrap(), serde_json::to_value(&wrong_kind).unwrap()];
            let hostile = fake_relay_raw(frames.clone(), Tail::Eose).await;
            let r = read(vec![hostile.clone()], release_filter(&keys)).await;
            assert!(r.events.is_empty(), "{r:?}");
            assert_eq!(r.answered, 1, "an EOSE is an answer: nothing published");

            // The same junk with the real event among it: only the real one survives.
            let mut mixed = frames;
            mixed.push(serde_json::to_value(&good).unwrap());
            let relay = fake_relay_raw(mixed, Tail::Eose).await;
            let r = read(vec![relay], release_filter(&keys)).await;
            assert_eq!(r.events.iter().map(|e| e.id).collect::<Vec<_>>(), vec![good.id], "{r:?}");
        });
    }

    #[test]
    fn junk_only_is_not_an_answer_and_closed_is_a_refusal() {
        rt().block_on(async {
            let keys = Keys::generate();
            let junk = serde_json::to_value(signed(&Keys::generate(), "stampede", "x", 1)).unwrap();
            // Junk then CLOSED: neither counts as the relay answering.
            let closed = fake_relay_raw(vec![junk], Tail::Closed).await;
            let r = read(vec![closed], release_filter(&keys)).await;
            assert_eq!((r.events.len(), r.answered, r.total), (0, 0, 1), "{r:?}");
            assert!(!r.reached());
        });
    }

    #[test]
    fn a_junk_flooding_relay_cannot_hide_a_release_held_by_another() {
        rt().block_on(async {
            let keys = Keys::generate();
            let good = signed(&keys, "stampede", "real", 100);
            // 500 distinct, validly signed events of other kinds / authors, plus
            // 500 of the right kind and author with the wrong signature.
            let mut flood: Vec<serde_json::Value> = Vec::new();
            for i in 0..250u64 {
                flood.push(serde_json::to_value(signed(&Keys::generate(), "stampede", "junk", 1000 + i)).unwrap());
                flood.push(serde_json::to_value(EventBuilder::text_note(format!("n{i}")).sign_with_keys(&keys).unwrap()).unwrap());
            }
            assert_eq!(flood.len(), 500);
            let flooder = fake_relay_raw(flood.clone(), Tail::Eose).await;
            let honest = fake_relay(vec![good.clone()]).await;
            // The flooder is listed first and answers first.
            let r = read(vec![flooder.clone(), honest], release_filter(&keys)).await;
            assert_eq!(r.events.iter().map(|e| e.id).collect::<Vec<_>>(), vec![good.id], "{r:?}");
            assert_eq!((r.answered, r.total), (2, 2));

            // And even the flooder itself still yields a release that follows its junk.
            flood.push(serde_json::to_value(&good).unwrap());
            let flooder = fake_relay_raw(flood, Tail::Eose).await;
            let r = read(vec![flooder], release_filter(&keys)).await;
            assert_eq!(r.events.iter().map(|e| e.id).collect::<Vec<_>>(), vec![good.id], "{r:?}");
        });
    }

    #[test]
    fn one_relay_keeps_at_most_its_own_budget() {
        rt().block_on(async {
            let keys = Keys::generate();
            // Valid, matching, distinct events: more than one relay may contribute.
            let many: Vec<Event> = (0..(MAX_PER_RELAY as u64 + 20)).map(|i| signed(&keys, "stampede", "old", 10 + i)).collect();
            let relay = fake_relay(many).await;
            let r = read(vec![relay], release_filter(&keys)).await;
            assert_eq!(r.events.len(), MAX_PER_RELAY);
        });
    }
}
