//! Transports: the [`HostTransport`] / [`ClientTransport`] traits, the
//! in-process [`LoopbackHub`] (injectable clock, latency, jitter and lossy
//! loss), and [`MuxHostTransport`], which puts the host player's own
//! zero-latency loopback link next to the network peers.
//!
//! Copied from Twang Thang's `twang-session` (presence proofs, personas and the
//! bot link path dropped: Stampede's bots live inside the host's `World`).

/// A peer as numbered by a host transport.
pub type PeerKey = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkPath {
    /// In-process (the host's own player, practice, tests).
    Local,
    Lan,
    Direct,
    /// Over a decented-net virtual link (Bluetooth LE).
    Bluetooth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostNetEvent {
    /// A peer connected.
    Joined { peer: PeerKey, path: LinkPath },
    Left { peer: PeerKey },
    Reliable { peer: PeerKey, bytes: Vec<u8> },
    Lossy { peer: PeerKey, bytes: Vec<u8> },
}

/// Who the joiner reached, as proven by the host's signed welcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostIdentity {
    /// The host's display name (self-chosen: show the npub prefix next to it).
    pub name: String,
    /// The host's public key, hex.
    pub pubkey: String,
    /// The host's public key, `npub1...`.
    pub npub: String,
}

impl HostIdentity {
    /// The 8 characters of the npub after `npub1` (40 bits), for the confirm
    /// screen: players who know the host's npub can compare it.
    pub fn npub_prefix(&self) -> String {
        self.npub.strip_prefix("npub1").unwrap_or(&self.npub).chars().take(8).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientNetEvent {
    /// Free-text progress for the join screen.
    Progress(String),
    /// Linked to the host. `host` is `Some` when the transport proved who the
    /// host is (QUIC): the session then asks the player to confirm before
    /// entering the lobby. In-process transports pass `None`.
    Connected { host: Option<HostIdentity> },
    /// The join failed before connecting (friendly text).
    Failed(String),
    Reliable(Vec<u8>),
    Lossy(Vec<u8>),
    /// The connection to the host ended.
    Disconnected { host_left: bool, reason: String },
}

/// The host side of a match's links. Non-blocking.
pub trait HostTransport: Send {
    fn poll(&mut self) -> Option<HostNetEvent>;
    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>);
    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>);
    fn kick(&mut self, peer: PeerKey);
    /// Drop a peer without banning it (it may come back). Defaults to `kick`.
    fn disconnect(&mut self, peer: PeerKey) {
        self.kick(peer);
    }
    /// Drop a joiner that did not confirm in time (no ban). Its client sees
    /// its own reason, not "kicked". Defaults to `disconnect`.
    fn confirm_timeout(&mut self, peer: PeerKey) {
        self.disconnect(peer);
    }
    /// Advertise the room and admit new peers (lobby) or not (match running).
    fn set_open(&mut self, open: bool);
    /// As [`set_open`](Self::set_open), but a listener that rotates its LAN
    /// admission token on each open and close keeps it. Only for the rejoin
    /// window after a failed host-migration switch, so the players it was
    /// handed to can come back (see `HostAuthority::hold_lan_token_for`).
    /// Required, so a wrapper cannot silently fall back to rotation.
    fn set_open_keeping_token(&mut self, open: bool);
    fn rtt_ms(&self, peer: PeerKey) -> Option<u32>;
    /// Tell every peer the host is leaving.
    fn shutdown(&mut self);
}

/// A client's link to the host. Non-blocking.
pub trait ClientTransport: Send {
    fn poll(&mut self) -> Option<ClientNetEvent>;
    fn send_reliable(&mut self, bytes: Vec<u8>);
    fn send_lossy(&mut self, bytes: Vec<u8>);
    fn rtt_ms(&self) -> Option<u32>;
    /// Leave.
    fn close(&mut self);
}

/// A host transport with no remote side (a room nobody can join): the host's
/// own player still comes through [`MuxHostTransport`].
#[derive(Default)]
pub struct NullHostTransport;

impl HostTransport for NullHostTransport {
    fn poll(&mut self) -> Option<HostNetEvent> {
        None
    }
    fn send_reliable(&mut self, _: PeerKey, _: Vec<u8>) {}
    fn send_lossy(&mut self, _: PeerKey, _: Vec<u8>) {}
    fn kick(&mut self, _: PeerKey) {}
    fn set_open(&mut self, _: bool) {}
    fn set_open_keeping_token(&mut self, _: bool) {}
    fn rtt_ms(&self, _: PeerKey) -> Option<u32> {
        None
    }
    fn shutdown(&mut self) {}
}

pub const LOCAL_PEER_BASE: PeerKey = 1 << 31;

#[cfg(feature = "native")]
mod native;
#[cfg(feature = "native")]
pub use native::{LoopClock, LoopbackConfig, LoopbackHub, LoopbackHost, LoopbackClient, MuxHostTransport};
