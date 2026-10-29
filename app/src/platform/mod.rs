//! Platform glue: lifecycle, safe-area insets and layout, raw touch and key
//! input, the data directory and profile, and OS services (clipboard, launch
//! links, wake lock, Wi-Fi multicast lock).

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub mod updater_android;
pub mod data_dir;
pub mod lan;
pub mod lifecycle;
pub mod os;
pub mod touch;
pub mod tilt;
pub mod viewport;

pub use os::Platform;
pub use touch::{Keys, Touch, TouchPhase, Touches};
pub use viewport::{Insets, Viewport};
pub use winit::keyboard::KeyCode;

/// The hook that binds a socket talking to local peers to the Wi-Fi network, so
/// a VPN or mobile data does not take the route (Android only; `None` elsewhere).
pub fn lan_socket_hook() -> Option<decented_net::LanSocketHook> {
    #[cfg(target_os = "android")]
    {
        Some(android::lan_socket_hook())
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

/// Whether a VPN is carrying the phone's internet traffic (Android's own answer:
/// any network with TRANSPORT_VPN; true when it cannot be asked). Always false elsewhere.
pub fn vpn_active() -> bool {
    #[cfg(target_os = "android")]
    {
        android::vpn_active()
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

/// The player's relay list as the network code sees it (`Profile.relays`). A
/// process-wide setting: the engine sets it from the profile at start and whenever
/// Settings changes the list, and `net_config` reads it, because the network
/// config is built in several places (host, join, Nearby) that have no profile in hand.
static RELAYS: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// Use `list` for every later network config (see [`net_config`]).
pub fn set_relays(list: Vec<String>) {
    *RELAYS.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
}

/// The relay list in use: what [`set_relays`] was given, else the defaults.
pub fn relays() -> Vec<String> {
    RELAYS.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(data_dir::default_relays)
}

/// Network settings for Stampede: the identity dir, plus the local-socket hook
/// so Nearby play stays on the Wi-Fi even with a VPN up or mobile data preferred.
pub fn net_config(data_dir: &std::path::Path) -> decented_net::NetConfig {
    let mut c = race_session::net_config(data_dir);
    // The player's relays, in their order (Settings, RELAYS).
    c.relays = relays();
    c.vpn_active = vpn_active();
    c.lan_socket_hook = lan_socket_hook();
    c
}

/// True on platforms whose primary input is touch.
pub const TOUCH_PLATFORM: bool = cfg!(target_os = "android");

/// Default logical size of the desktop window: a phone held sideways, wide.
pub const DESKTOP_SIZE: (f32, f32) = (1600.0, 720.0);

/// The scheme of launch links: `stampede://join#CODE`.
pub const LINK_SCHEME: &str = "stampede";

/// The room code in a join link (`stampede://join#CODE`), upper-cased, or
/// `None` if `link` is not one. Room codes are letters and digits only.
pub fn parse_join_link(link: &str) -> Option<String> {
    let rest = link.trim().strip_prefix(LINK_SCHEME)?.strip_prefix("://")?;
    let (host, code) = rest.split_once('#')?;
    if !host.eq_ignore_ascii_case("join") && !host.eq_ignore_ascii_case("join/") {
        return None;
    }
    let code = code.trim();
    if code.is_empty() || code.len() > 16 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(code.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_links() {
        assert_eq!(parse_join_link("stampede://join#abc234"), Some("ABC234".into()));
        assert_eq!(parse_join_link(" stampede://join/#KWTQPM "), Some("KWTQPM".into()));
        assert_eq!(parse_join_link("stampede://join#"), None);
        assert_eq!(parse_join_link("stampede://other#ABC"), None);
        assert_eq!(parse_join_link("twangthang://join#ABC"), None);
        assert_eq!(parse_join_link("stampede://join#AB CD"), None);
    }
}
