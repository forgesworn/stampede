//! Acts as the owner (My Signet's side of the K1d contract) against a running
//! headless Keep, over the real relays in its pairing link:
//! claim -> invite -> revoke, reading each ack.
//!
//! The wraps are made and opened with nostr's own NIP-59 code, not the Keep's
//! hand-written code, so this also cross-checks the Keep's wrap format.
//!
//!     cargo run -p race-session --example k1d_owner_smoke -- 'signet-keep:pair?k=...'
//!
//! Prints `step ... ok` lines and exits 0 when the Keep answered `paired`, `active`
//! and `revoked`; exits 1 on the first step that gets no answer in time.

use std::time::{Duration, Instant};

use decented_signet::relay::{Connector, RelayMsg, WsConnector};
use nostr::nips::nip59::UnwrappedGift;
use nostr::{Event, EventBuilder, Filter, Keys, Kind, PublicKey, Tag, Timestamp};

const KIND_CLAIM: u16 = 27660;
const KIND_INVITE: u16 = 27661;
const KIND_REVOKE: u16 = 27662;
const KIND_ACK: u16 = 27663;

/// How long each step waits for its ack.
const STEP_TIMEOUT: Duration = Duration::from_secs(90);

fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Two bytes after the `%`, not two characters: a `%` followed by a
            // multibyte character is left as it is.
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Link {
    keep: PublicKey,
    code: String,
    relays: Vec<String>,
}

fn parse_link(link: &str) -> Result<Link, String> {
    let query = link.strip_prefix("signet-keep:pair?").ok_or("not a signet-keep:pair link")?;
    let (mut keep, mut code, mut relays) = (None, None, Vec::new());
    for part in query.split('&') {
        match part.split_once('=') {
            Some(("k", v)) => keep = Some(PublicKey::from_hex(v).map_err(|e| e.to_string())?),
            Some(("c", v)) => code = Some(v.to_string()),
            Some(("r", v)) => relays.push(unescape(v)),
            _ => {}
        }
    }
    if relays.is_empty() || relays.len() > 3 || relays.iter().any(|r| !r.starts_with("wss://")) {
        return Err("a pairing link carries 1 to 3 wss:// relays".into());
    }
    Ok(Link { keep: keep.ok_or("no k")?, code: code.ok_or("no c")?, relays })
}

fn tag(parts: &[&str]) -> Tag {
    Tag::parse(parts.iter().map(|p| p.to_string()).collect::<Vec<_>>()).expect("tag")
}

struct Ack {
    status: String,
    grant: Option<String>,
    claim: bool,
    reason: Option<String>,
}

fn main() {
    let link = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: k1d_owner_smoke 'signet-keep:pair?k=...&c=...&n=...&r=wss://...'");
        std::process::exit(2);
    });
    let link = parse_link(&link).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let owner = Keys::generate();
    let keep_hex = link.keep.to_hex();
    println!("owner {}", owner.public_key().to_hex());
    println!("keep  {keep_hex}");
    println!("relays {}", link.relays.join(" "));
    let relay = WsConnector.connect(&link.relays);
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let mut inbox = relay.subscribe(vec![Filter::new().kind(Kind::GiftWrap).pubkey(owner.public_key()).since(Timestamp::from_secs(now() - 3 * 86_400))]);
    let mut acks: Vec<Ack> = Vec::new();

    // Send `kind` with `tags` (gift-wrapped by nostr's code) until an ack matching `wanted` arrives.
    let mut step = |name: &str, kind: u16, tags: Vec<Tag>, wanted: &dyn Fn(&Ack) -> bool| -> Ack {
        let started = Instant::now();
        let mut last_sent: Option<Instant> = None;
        loop {
            if last_sent.is_none_or(|t| t.elapsed() > Duration::from_secs(20)) {
                let rumor = EventBuilder::new(Kind::Custom(kind), "").tags(tags.clone()).build(owner.public_key());
                let wrap = rt.block_on(EventBuilder::gift_wrap(&owner, &link.keep, rumor, [])).expect("wrap");
                relay.publish(&wrap);
                last_sent = Some(Instant::now());
                println!("step {name}: sent");
            }
            while let Some(message) = inbox.try_recv() {
                let RelayMsg::Event(wrap) = message else { continue };
                let Some(ack) = read_ack(&rt, &owner, &link.keep, &wrap) else { continue };
                println!("  ack {} grant={} claim={} reason={}", ack.status, ack.grant.as_deref().unwrap_or("-"), ack.claim, ack.reason.as_deref().unwrap_or("-"));
                acks.push(ack);
            }
            if let Some(at) = acks.iter().position(|a| wanted(a)) {
                println!("step {name}: ok");
                return acks.remove(at);
            }
            if started.elapsed() > STEP_TIMEOUT {
                println!("step {name}: FAILED, no matching ack in {} s", STEP_TIMEOUT.as_secs());
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };

    let ack = step("claim", KIND_CLAIM, vec![tag(&["keep", &keep_hex]), tag(&["code", &link.code])], &|a| a.claim && a.status == "paired");
    assert!(ack.grant.is_none());

    let grant = hex16();
    let friend = Keys::generate().public_key().to_hex();
    let invite = vec![
        tag(&["keep", &keep_hex]),
        tag(&["grant", &grant]),
        tag(&["secret", &hex32()]),
        tag(&["expiration", &(now() + 3600).to_string()]),
        tag(&["p", &friend]),
        tag(&["name", "Smoke test"]),
    ];
    step("invite", KIND_INVITE, invite, &|a| a.grant.as_deref() == Some(&grant) && a.status == "active");

    // A guest pass that has already expired is refused with its reason.
    let stale = hex16();
    let expired = vec![
        tag(&["keep", &keep_hex]),
        tag(&["grant", &stale]),
        tag(&["secret", &hex32()]),
        tag(&["expiration", &(now() - 60).to_string()]),
        tag(&["guest"]),
    ];
    let refused = step("expired invite", KIND_INVITE, expired, &|a| a.grant.as_deref() == Some(&stale) && a.status == "refused");
    assert_eq!(refused.reason.as_deref(), Some("expired"));

    step("revoke", KIND_REVOKE, vec![tag(&["keep", &keep_hex]), tag(&["grant", &grant])], &|a| a.grant.as_deref() == Some(&grant) && a.status == "revoked");
    println!("all steps ok: paired, active, refused (expired), revoked");
}

fn read_ack(rt: &tokio::runtime::Runtime, owner: &Keys, keep: &PublicKey, wrap: &Event) -> Option<Ack> {
    let gift = rt.block_on(UnwrappedGift::from_gift_wrap(owner, wrap)).ok()?;
    let rumor = gift.rumor;
    if gift.sender != *keep || rumor.pubkey != *keep || rumor.kind != Kind::Custom(KIND_ACK) || !rumor.content.is_empty() {
        return None;
    }
    let value = |name: &str| rumor.tags.iter().find(|t| t.as_slice().first().map(String::as_str) == Some(name)).map(|t| t.as_slice().get(1).cloned().unwrap_or_default());
    if value("keep")? != keep.to_hex() {
        return None;
    }
    Some(Ack { status: value("status")?, grant: value("grant"), claim: value("claim").is_some(), reason: value("reason") })
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("randomness");
    bytes
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex16() -> String {
    hex(&random::<16>())
}

fn hex32() -> String {
    hex(&random::<32>())
}

#[cfg(test)]
mod tests {
    use super::unescape;

    #[test]
    fn a_percent_before_a_multibyte_character_does_not_panic() {
        assert_eq!(unescape("a%41b"), "aAb");
        assert_eq!(unescape("100%é!"), "100%é!");
        assert_eq!(unescape("%é"), "%é");
        assert_eq!(unescape("50%"), "50%");
        assert_eq!(unescape("%zz1"), "%zz1");
    }
}
