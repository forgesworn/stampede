//! NIP-59 gift wraps for the owner messages (the K1d contract): rumor (unsigned)
//! -> seal (kind 13, signed by the sender, NIP-44 v2 to the recipient) -> wrap
//! (kind 1059, a throwaway key, `["p", <recipient>]`, NIP-44 v2). Made by hand
//! rather than with nostr's async signer API, so the checks the contract asks of
//! a receiver sit in plain sight in [`unwrap`].

use nostr::nips::nip44::{self, Version};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

/// NIP-59 jitter: seal and wrap `created_at` are up to this far in the past.
pub const JITTER_SECS: u64 = 2 * 86_400;
/// The largest wrap read at all (an owner message is a few hundred bytes).
pub const MAX_WRAP_CONTENT: usize = 16 * 1024;

/// Why a wrap was not opened. Every one is dropped silently by the caller.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Dropped {
    NotAWrap,
    TooLarge,
    NotForUs,
    BadSignature,
    Undecryptable,
    NotASeal,
    BadSeal,
    BadRumor,
    /// The rumor's author is not the seal's signer.
    AuthorMismatch,
}

/// A random time from now back to [`JITTER_SECS`] ago.
fn jittered(now: u64) -> Timestamp {
    let mut bytes = [0u8; 4];
    // Without randomness the time is simply the real one: still a valid wrap.
    let back = if getrandom::fill(&mut bytes).is_ok() { u64::from(u32::from_le_bytes(bytes)) % (JITTER_SECS + 1) } else { 0 };
    Timestamp::from_secs(now.saturating_sub(back))
}

/// Wrap a rumor of `kind` with `tags` (content `""`), written by `sender` at
/// real time `now`, for `to`.
pub fn gift_wrap(sender: &Keys, to: &PublicKey, kind: u16, tags: Vec<Tag>, now: u64) -> Result<Event, String> {
    let rumor: UnsignedEvent = EventBuilder::new(Kind::Custom(kind), "")
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(now))
        .build(sender.public_key());
    let sealed = nip44::encrypt(sender.secret_key(), to, rumor.as_json(), Version::V2).map_err(|e| e.to_string())?;
    let seal = EventBuilder::new(Kind::Seal, sealed)
        .custom_created_at(jittered(now))
        .sign_with_keys(sender)
        .map_err(|e| e.to_string())?;
    let throwaway = Keys::generate();
    let wrapped = nip44::encrypt(throwaway.secret_key(), to, seal.as_json(), Version::V2).map_err(|e| e.to_string())?;
    EventBuilder::new(Kind::GiftWrap, wrapped)
        .tag(Tag::public_key(*to))
        .custom_created_at(jittered(now))
        .sign_with_keys(&throwaway)
        .map_err(|e| e.to_string())
}

/// Open a wrap addressed to `me`: the wrap's signature, the seal's kind and
/// signature, and `rumor.pubkey == seal.pubkey`. Returns the sender (the seal's
/// signer) and the rumor. What the rumor says is the caller's to check.
pub fn unwrap(me: &Keys, wrap: &Event) -> Result<(PublicKey, UnsignedEvent), Dropped> {
    if wrap.kind != Kind::GiftWrap {
        return Err(Dropped::NotAWrap);
    }
    if wrap.content.len() > MAX_WRAP_CONTENT {
        return Err(Dropped::TooLarge);
    }
    let mine = me.public_key().to_hex();
    let addressed = wrap.tags.iter().any(|t| matches!(t.as_slice(), [name, who, ..] if name == "p" && *who == mine));
    if !addressed {
        return Err(Dropped::NotForUs);
    }
    if wrap.verify().is_err() {
        return Err(Dropped::BadSignature);
    }
    let seal_json = nip44::decrypt(me.secret_key(), &wrap.pubkey, &wrap.content).map_err(|_| Dropped::Undecryptable)?;
    let seal = Event::from_json(seal_json).map_err(|_| Dropped::NotASeal)?;
    if seal.kind != Kind::Seal {
        return Err(Dropped::NotASeal);
    }
    if seal.verify().is_err() {
        return Err(Dropped::BadSeal);
    }
    let rumor_json = nip44::decrypt(me.secret_key(), &seal.pubkey, &seal.content).map_err(|_| Dropped::Undecryptable)?;
    let rumor = UnsignedEvent::from_json(rumor_json).map_err(|_| Dropped::BadRumor)?;
    if rumor.pubkey != seal.pubkey {
        return Err(Dropped::AuthorMismatch);
    }
    Ok((seal.pubkey, rumor))
}

/// The value of the only tag named `name`: `Ok(None)` if there is none, `Err`
/// if there are several (or one with no value).
pub fn one_tag<'a>(rumor: &'a UnsignedEvent, name: &str) -> Result<Option<&'a str>, ()> {
    let mut found = None;
    for tag in rumor.tags.iter() {
        if tag.as_slice().first().map(String::as_str) != Some(name) {
            continue;
        }
        if found.is_some() {
            return Err(());
        }
        found = Some(tag.as_slice().get(1).map(String::as_str).ok_or(())?);
    }
    Ok(found)
}

/// Whether the rumor has a tag named `name` (with or without a value).
pub fn has_tag(rumor: &UnsignedEvent, name: &str) -> bool {
    rumor.tags.iter().any(|t| t.as_slice().first().map(String::as_str) == Some(name))
}

/// How many tags are named `name`.
pub fn count_tags(rumor: &UnsignedEvent, name: &str) -> usize {
    rumor.tags.iter().filter(|t| t.as_slice().first().map(String::as_str) == Some(name)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn keys(n: u8) -> Keys {
        Keys::parse(&format!("{}{n:x}", "0".repeat(63))).unwrap()
    }

    fn tag(parts: &[&str]) -> Tag {
        Tag::parse(parts.iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_wrap_opens_for_its_recipient_and_names_the_sender() {
        let (a, b) = (keys(1), keys(2));
        let wrap = gift_wrap(&a, &b.public_key(), 27660, vec![tag(&["keep", "ab"])], NOW).unwrap();
        assert_eq!(wrap.kind, Kind::GiftWrap);
        assert_ne!(wrap.pubkey, a.public_key(), "the wrap is signed by a throwaway key");
        assert_eq!(wrap.tags.public_keys().copied().collect::<Vec<_>>(), vec![b.public_key()]);
        let (sender, rumor) = unwrap(&b, &wrap).unwrap();
        assert_eq!(sender, a.public_key());
        assert_eq!((rumor.kind, rumor.pubkey, rumor.created_at.as_secs(), rumor.content.as_str()), (Kind::Custom(27660), a.public_key(), NOW, ""));
        assert_eq!(one_tag(&rumor, "keep"), Ok(Some("ab")));
        assert_eq!(one_tag(&rumor, "grant"), Ok(None));
    }

    #[test]
    fn seal_and_wrap_times_are_jittered_up_to_two_days_back() {
        let (a, b) = (keys(1), keys(2));
        let mut earlier = 0;
        for _ in 0..40 {
            let wrap = gift_wrap(&a, &b.public_key(), 27660, vec![], NOW).unwrap();
            let at = wrap.created_at.as_secs();
            assert!((NOW - JITTER_SECS..=NOW).contains(&at), "{at}");
            earlier += usize::from(at < NOW - 3600);
        }
        assert!(earlier > 20, "most wraps are well in the past");
    }

    #[test]
    fn a_wrap_for_someone_else_or_damaged_is_dropped() {
        let (a, b, c) = (keys(1), keys(2), keys(3));
        let wrap = gift_wrap(&a, &b.public_key(), 27660, vec![], NOW).unwrap();
        assert_eq!(unwrap(&c, &wrap), Err(Dropped::NotForUs));
        let mut not_wrap = wrap.clone();
        not_wrap.kind = Kind::TextNote;
        assert_eq!(unwrap(&b, &not_wrap), Err(Dropped::NotAWrap));
        let mut big = wrap.clone();
        big.content = "x".repeat(MAX_WRAP_CONTENT + 1);
        assert_eq!(unwrap(&b, &big), Err(Dropped::TooLarge));
        let mut forged = wrap.clone();
        forged.content.push('A');
        assert_eq!(unwrap(&b, &forged), Err(Dropped::BadSignature));
        // Addressed to b by tag, but encrypted to c: b cannot read it.
        let to_c = gift_wrap(&a, &c.public_key(), 27660, vec![], NOW).unwrap();
        let mut retagged = EventBuilder::new(Kind::GiftWrap, to_c.content.clone())
            .tag(Tag::public_key(b.public_key()))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        assert_eq!(unwrap(&b, &retagged), Err(Dropped::Undecryptable));
        retagged.content.clear();
    }

    /// A wrap whose seal and rumor are made by hand, to break one rule at a time.
    fn crafted(me: &Keys, seal_signer: &Keys, rumor_author: &Keys, seal_kind: Kind, forge_seal_sig: bool) -> Event {
        let rumor = EventBuilder::new(Kind::Custom(27660), "").build(rumor_author.public_key());
        let sealed = nip44::encrypt(seal_signer.secret_key(), &me.public_key(), rumor.as_json(), Version::V2).unwrap();
        let mut seal = EventBuilder::new(seal_kind, sealed).sign_with_keys(seal_signer).unwrap();
        if forge_seal_sig {
            seal.content = nip44::encrypt(seal_signer.secret_key(), &me.public_key(), "{}", Version::V2).unwrap();
        }
        let throwaway = Keys::generate();
        let wrapped = nip44::encrypt(throwaway.secret_key(), &me.public_key(), seal.as_json(), Version::V2).unwrap();
        EventBuilder::new(Kind::GiftWrap, wrapped).tag(Tag::public_key(me.public_key())).sign_with_keys(&throwaway).unwrap()
    }

    #[test]
    fn the_seal_must_be_a_signed_seal_by_the_rumors_author() {
        let (me, a, b) = (keys(1), keys(2), keys(3));
        assert!(unwrap(&me, &crafted(&me, &a, &a, Kind::Seal, false)).is_ok());
        // The rumor claims to be b's but a sealed it.
        assert_eq!(unwrap(&me, &crafted(&me, &a, &b, Kind::Seal, false)), Err(Dropped::AuthorMismatch));
        assert_eq!(unwrap(&me, &crafted(&me, &a, &a, Kind::TextNote, false)), Err(Dropped::NotASeal));
        // A seal whose content was changed after signing.
        assert_eq!(unwrap(&me, &crafted(&me, &a, &a, Kind::Seal, true)), Err(Dropped::BadSeal));
    }

    #[test]
    fn repeated_tags_are_told_apart_from_missing_ones() {
        let (a, b) = (keys(1), keys(2));
        let wrap = gift_wrap(&a, &b.public_key(), 27661, vec![tag(&["x", "1"]), tag(&["x", "2"]), tag(&["guest"])], NOW).unwrap();
        let (_, rumor) = unwrap(&b, &wrap).unwrap();
        assert_eq!(one_tag(&rumor, "x"), Err(()));
        assert_eq!(one_tag(&rumor, "guest"), Err(()), "a tag with no value is no value");
        assert!(has_tag(&rumor, "guest") && !has_tag(&rumor, "p"));
        assert_eq!((count_tags(&rumor, "x"), count_tags(&rumor, "p")), (2, 0));
    }
}
