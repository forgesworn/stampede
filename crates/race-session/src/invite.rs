//! Invite links: how a Keep hands out a way in that holds no address.
//!
//! A link names the Keep by its runtime public key and carries the invite's
//! 32-byte secret, both as lower-case hex:
//!
//! ```text
//! stampede://keep/<64 hex: the Keep's runtime public key>/<64 hex: the secret>
//! ```
//!
//! Pass the two parts to [`crate::net::join_invite`]. Nothing here touches
//! the network.

/// The scheme and authority every invite link starts with.
pub const INVITE_LINK_PREFIX: &str = "stampede://keep/";

pub use fray_keep::hex_encode;

/// Exactly 64 hex characters (either case) as 32 bytes. The error is for the
/// paste box.
pub fn parse_hex32(text: &str) -> Result<[u8; 32], String> {
    fray_keep::parse_hex32(text).ok_or_else(|| {
        if text.len() != 64 {
            format!("expected 64 hex characters, got {}", text.len())
        } else {
            "not hexadecimal".to_string()
        }
    })
}

/// The link text for an invite to the Keep `host_key` with `secret`.
pub fn format_invite_link(host_key: &[u8; 32], secret: &[u8; 32]) -> String {
    fray_keep::format_invite_link(INVITE_LINK_PREFIX, host_key, secret)
}

/// Read a link pasted by a person: leading and trailing whitespace is ignored,
/// the scheme and the hex may be in either case. Returns `(host_key, secret)`.
/// The error is friendly text for the paste box.
pub fn parse_invite_link(text: &str) -> Result<([u8; 32], [u8; 32]), String> {
    if let Some(invite) = fray_keep::parse_invite_link(INVITE_LINK_PREFIX, text) {
        return Ok((invite.host_key, invite.secret));
    }
    // Refused: say why, in the paste box's words.
    let text = text.trim();
    let p = INVITE_LINK_PREFIX;
    let rest = match text.get(..p.len()) {
        Some(head) if head.eq_ignore_ascii_case(p) => &text[p.len()..],
        _ => return Err("This is not a Stampede invite link (it should start with stampede://keep/)".to_string()),
    };
    let (host, secret) = rest.split_once('/').ok_or("This invite link is incomplete")?;
    parse_hex32(host).map_err(|e| format!("This invite link has a bad Keep key: {e}"))?;
    match parse_hex32(secret) {
        Err(e) => Err(format!("This invite link has a bad secret: {e}")),
        Ok(_) => Err("This invite link is incomplete".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: [u8; 32] = [0xab; 32];
    const SECRET: [u8; 32] = [0x01; 32];

    #[test]
    fn a_link_round_trips() {
        let link = format_invite_link(&HOST, &SECRET);
        assert_eq!(link, format!("stampede://keep/{}/{}", "ab".repeat(32), "01".repeat(32)));
        assert_eq!(parse_invite_link(&link), Ok((HOST, SECRET)));
    }

    #[test]
    fn whitespace_and_case_are_forgiven() {
        let link = format_invite_link(&HOST, &SECRET).to_uppercase().replace("STAMPEDE://KEEP/", "stampede://keep/");
        assert_eq!(parse_invite_link(&format!("  \n{link}\t \r\n")), Ok((HOST, SECRET)));
    }

    #[test]
    fn a_bad_link_is_refused() {
        let good = format_invite_link(&HOST, &SECRET);
        let host = "ab".repeat(32);
        let secret = "01".repeat(32);
        let bad = [
            String::new(),
            "stampede://keep/".to_string(),
            good.replacen("stampede", "https", 1),
            good.replacen("keep", "room", 1),
            format!("stampede://keep/{host}"),
            format!("stampede://keep/{host}/"),
            format!("stampede://keep//{secret}"),
            format!("stampede://keep/{}/{secret}", &host[..62]),
            format!("stampede://keep/{host}/{}", &secret[..62]),
            format!("stampede://keep/{host}0/{secret}"),
            format!("stampede://keep/{}g/{secret}", &host[..63]),
            format!("stampede://keep/{host}/{}z", &secret[..63]),
            format!("{good}/extra"),
            format!("{good}#frag"),
            format!("{good} {good}"),
            format!("stampede://keep/{host}/{}\u{e9}", &secret[..62]),
        ];
        for text in bad {
            assert!(parse_invite_link(&text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn hex32_reads_either_case_and_nothing_else() {
        assert_eq!(parse_hex32(&"Ff".repeat(32)), Ok([0xff; 32]));
        assert!(parse_hex32("").is_err());
        assert!(parse_hex32(&"+1".repeat(32)).is_err());
        assert_eq!(hex_encode(&[0, 15, 255]), "000fff");
    }
}
