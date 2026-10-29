//! Acts as the owner personas (My Signet's side of the K1e contract) against a
//! running headless Keep, over the real relays in its pairing link:
//!
//! claim (scope all) as A -> owners update rev 1 listing A and B -> accept from B
//! -> an invite signed by B is `active` -> owners update rev 2 without B -> an
//! invite from B gets no answer -> the same update again is `updated` again ->
//! another list at that revision is refused as stale.
//!
//! The wraps are made and opened with nostr's own NIP-59 code, not the Keep's
//! hand-written code, so this also cross-checks the Keep's wrap format.
//!
//!     cargo run -p race-session --example k1e_owner_smoke -- 'signet-keep:pair?k=...'
//!
//! Prints `step ... ok` lines and exits 0 when every step went as above; exits 1
//! on the first step that does not.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use decented_signet::relay::{Connector, RelayMsg, WsConnector};
use nostr::nips::nip59::UnwrappedGift;
use nostr::{Event, EventBuilder, Filter, Keys, Kind, PublicKey, Tag, Timestamp};

const KIND_CLAIM: u16 = 27660;
const KIND_INVITE: u16 = 27661;
const KIND_UPDATE: u16 = 27664;
const KIND_ACCEPT: u16 = 27665;
const KIND_ACK: u16 = 27663;

/// How long each step waits for its ack.
const STEP_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a message that must get no answer is waited on.
const SILENCE: Duration = Duration::from_secs(30);

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
    to: PublicKey,
    status: String,
    grant: Option<String>,
    claim: bool,
    rev: Option<String>,
    reason: Option<String>,
}

fn main() {
    let link = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: k1e_owner_smoke 'signet-keep:pair?k=...&c=...&n=...&r=wss://...'");
        std::process::exit(2);
    });
    let link = parse_link(&link).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let (a, b) = (Keys::generate(), Keys::generate());
    let keep_hex = link.keep.to_hex();
    println!("owner A {}", a.public_key().to_hex());
    println!("owner B {}", b.public_key().to_hex());
    println!("keep    {keep_hex}");
    println!("relays  {}", link.relays.join(" "));
    let relay = WsConnector.connect(&link.relays);
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let since = Timestamp::from_secs(now() - 3 * 86_400);
    let inbox_a = RefCell::new(relay.subscribe(vec![Filter::new().kind(Kind::GiftWrap).pubkey(a.public_key()).since(since)]));
    let inbox_b = RefCell::new(relay.subscribe(vec![Filter::new().kind(Kind::GiftWrap).pubkey(b.public_key()).since(since)]));
    let acks: RefCell<Vec<Ack>> = RefCell::new(Vec::new());

    let publish = |from: &Keys, kind: u16, tags: &[Tag]| {
        let rumor = EventBuilder::new(Kind::Custom(kind), "").tags(tags.to_vec()).build(from.public_key());
        let wrap = rt.block_on(EventBuilder::gift_wrap(from, &link.keep, rumor, [])).expect("wrap");
        relay.publish(&wrap);
    };
    let collect = || {
        for (inbox, me) in [(&inbox_a, &a), (&inbox_b, &b)] {
            while let Some(message) = inbox.borrow_mut().try_recv() {
                let RelayMsg::Event(wrap) = message else { continue };
                let Some(ack) = read_ack(&rt, me, &link.keep, &wrap) else { continue };
                println!(
                    "  ack to {} {} grant={} claim={} rev={} reason={}",
                    if ack.to == a.public_key() { "A" } else { "B" },
                    ack.status,
                    ack.grant.as_deref().unwrap_or("-"),
                    ack.claim,
                    ack.rev.as_deref().unwrap_or("-"),
                    ack.reason.as_deref().unwrap_or("-")
                );
                acks.borrow_mut().push(ack);
            }
        }
    };

    // Send `kind` with `tags` from `from` (again every 20 s) until an ack matching `wanted` arrives.
    let step = |name: &str, from: &Keys, kind: u16, tags: Vec<Tag>, wanted: &dyn Fn(&Ack) -> bool| -> Ack {
        let started = Instant::now();
        let mut last_sent: Option<Instant> = None;
        loop {
            if last_sent.is_none_or(|t| t.elapsed() > Duration::from_secs(20)) {
                publish(from, kind, &tags);
                last_sent = Some(Instant::now());
                println!("step {name}: sent");
            }
            collect();
            let found = acks.borrow().iter().position(|a| wanted(a));
            if let Some(at) = found {
                println!("step {name}: ok");
                return acks.borrow_mut().remove(at);
            }
            if started.elapsed() > STEP_TIMEOUT {
                println!("step {name}: FAILED, no matching ack in {} s", STEP_TIMEOUT.as_secs());
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };

    let personas = |who: &[&Keys]| -> Vec<Tag> { who.iter().map(|k| tag(&["persona", &k.public_key().to_hex(), "games"])).collect() };
    let update = |rev: u64, who: &[&Keys]| -> Vec<Tag> {
        let mut tags = vec![tag(&["keep", &keep_hex]), tag(&["rev", &rev.to_string()])];
        tags.extend(personas(who));
        tags
    };

    let ack = step("claim (scope all)", &a, KIND_CLAIM, vec![tag(&["keep", &keep_hex]), tag(&["code", &link.code]), tag(&["scope", "all"])], &|k| k.claim && k.status == "paired");
    assert!(ack.grant.is_none());

    let (apk, bpk) = (a.public_key(), b.public_key());
    let ack = step("update rev 1 (A, B)", &a, KIND_UPDATE, update(1, &[&a, &b]), &|k| k.to == apk && k.rev.as_deref() == Some("1") && (k.status == "updated" || k.reason.as_deref() == Some("stale")));
    assert_eq!(ack.status, "updated");

    let ack = step("accept from B (rev 1)", &b, KIND_ACCEPT, vec![tag(&["keep", &keep_hex]), tag(&["rev", "1"])], &|k| k.to == bpk && k.rev.as_deref() == Some("1") && k.status == "updated");
    assert!(ack.grant.is_none());

    let invite = |grant: &str| -> Vec<Tag> {
        vec![tag(&["keep", &keep_hex]), tag(&["grant", grant]), tag(&["secret", &hex32()]), tag(&["expiration", &(now() + 3600).to_string()]), tag(&["guest"]), tag(&["name", "Smoke test (B)"])]
    };
    let grant = hex16();
    step("invite signed by B", &b, KIND_INVITE, invite(&grant), &|k| k.to == bpk && k.grant.as_deref() == Some(&grant) && k.status == "active");

    step("update rev 2 (A only)", &a, KIND_UPDATE, update(2, &[&a]), &|k| k.to == apk && k.rev.as_deref() == Some("2") && (k.status == "updated" || k.reason.as_deref() == Some("stale")));

    // B is no longer an owner persona: its invite is dropped without a word.
    let dropped = hex16();
    publish(&b, KIND_INVITE, &invite(&dropped));
    println!("step invite from removed B: sent, waiting {} s for silence", SILENCE.as_secs());
    let started = Instant::now();
    while started.elapsed() < SILENCE {
        collect();
        if acks.borrow().iter().any(|k| k.grant.as_deref() == Some(&dropped)) {
            println!("step invite from removed B: FAILED, the Keep answered");
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("step invite from removed B: ok, no ack");

    // Acks that came twice (several relays) must not stand in for the next step's.
    collect();
    acks.borrow_mut().clear();
    // The Keep is still listening: the same update again (rev 2, A only) is told `updated` again ...
    let ack = step("repeated update (rev 2 again)", &a, KIND_UPDATE, update(2, &[&a]), &|k| k.to == apk && k.status == "updated" && k.rev.as_deref() == Some("2"));
    assert_eq!(ack.rev.as_deref(), Some("2"));
    // ... and a different list at that revision is refused with the current revision.
    let ack = step("stale update (rev 2, another list)", &a, KIND_UPDATE, update(2, &[&a, &b]), &|k| k.to == apk && k.status == "refused" && k.reason.as_deref() == Some("stale"));
    assert_eq!(ack.rev.as_deref(), Some("2"));
    println!("all steps ok: paired (scope all), updated rev 1, updated (accept), active (B), updated rev 2, B dropped, updated again (repeat), refused (stale)");
}

fn read_ack(rt: &tokio::runtime::Runtime, me: &Keys, keep: &PublicKey, wrap: &Event) -> Option<Ack> {
    let gift = rt.block_on(UnwrappedGift::from_gift_wrap(me, wrap)).ok()?;
    let rumor = gift.rumor;
    if gift.sender != *keep || rumor.pubkey != *keep || rumor.kind != Kind::Custom(KIND_ACK) || !rumor.content.is_empty() {
        return None;
    }
    let value = |name: &str| rumor.tags.iter().find(|t| t.as_slice().first().map(String::as_str) == Some(name)).map(|t| t.as_slice().get(1).cloned().unwrap_or_default());
    if value("keep")? != keep.to_hex() {
        return None;
    }
    Some(Ack { to: me.public_key(), status: value("status")?, grant: value("grant"), claim: value("claim").is_some(), rev: value("rev"), reason: value("reason") })
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
