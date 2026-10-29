//! Which network carries local play when a phone has several (Wi-Fi, a VPN,
//! cellular). The Android glue reads what the connectivity service says about
//! each `Network` and this picks the one to bind the local sockets to, so the
//! choice is testable on the desktop.

/// What the connectivity service knows about the networks that carry local play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalNetwork {
    /// `Network.getNetworkHandle()` of the Wi-Fi (else Ethernet) network, if any.
    pub handle: Option<i64>,
    /// A VPN network is up.
    pub vpn: bool,
    /// The local network's detailed state is BLOCKED (the lockdown setting).
    pub blocked: bool,
}

/// One network as read over JNI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub handle: i64,
    pub wifi: bool,
    pub ethernet: bool,
    /// TRANSPORT_VPN, or lacking NET_CAPABILITY_NOT_VPN.
    pub vpn: bool,
    pub blocked: bool,
}

/// Pick the Wi-Fi network (else Ethernet) that is not a VPN, and note whether a
/// VPN exists. Wi-Fi wins over Ethernet; a VPN network is never chosen.
pub fn choose_local_network(nets: &[Seen]) -> LocalNetwork {
    let vpn = nets.iter().any(|n| n.vpn);
    let best = nets.iter().filter(|n| !n.vpn && (n.wifi || n.ethernet)).min_by_key(|n| !n.wifi);
    LocalNetwork { handle: best.map(|n| n.handle), vpn, blocked: best.is_some_and(|n| n.blocked) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn net(handle: i64, wifi: bool, ethernet: bool, vpn: bool, blocked: bool) -> Seen {
        Seen { handle, wifi, ethernet, vpn, blocked }
    }

    #[test]
    fn the_wifi_network_is_chosen_over_the_vpn_and_cellular() {
        let nets = [net(1, false, false, false, false), net(2, false, false, true, false), net(3, true, false, false, false)];
        assert_eq!(choose_local_network(&nets), LocalNetwork { handle: Some(3), vpn: true, blocked: false });
    }

    #[test]
    fn a_vpn_that_also_claims_wifi_is_never_chosen() {
        // A VPN over Wi-Fi reports TRANSPORT_WIFI as well as TRANSPORT_VPN.
        let nets = [net(9, true, false, true, false)];
        assert_eq!(choose_local_network(&nets), LocalNetwork { handle: None, vpn: true, blocked: false });
    }

    #[test]
    fn wifi_beats_ethernet_and_ethernet_alone_will_do() {
        let both = [net(4, false, true, false, false), net(5, true, false, false, false)];
        assert_eq!(choose_local_network(&both).handle, Some(5));
        assert_eq!(choose_local_network(&[net(4, false, true, false, false)]).handle, Some(4));
    }

    #[test]
    fn no_local_network_and_a_blocked_one_are_reported() {
        assert_eq!(choose_local_network(&[]), LocalNetwork { handle: None, vpn: false, blocked: false });
        let cellular_only = [net(1, false, false, false, false)];
        assert_eq!(choose_local_network(&cellular_only).handle, None);
        let locked = [net(2, false, false, true, false), net(3, true, false, false, true)];
        assert_eq!(choose_local_network(&locked), LocalNetwork { handle: Some(3), vpn: true, blocked: true });
    }
}
