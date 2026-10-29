//! Stampede's multiplayer layer (spec 10): lobby, a host-authoritative race
//! with client prediction, over Twang Thang's transport kit.
//!
//! - [`HostAuthority`]: lobby, seats, bots, the race clock, snapshots, results,
//!   rematch. Runs `race_core::World`; bots live in it.
//! - [`ClientSession`]: join, lobby view, and the race with prediction,
//!   reconciliation and interpolation. The host's own player is one of these
//!   on a zero-latency loopback ([`host_with_local_player`]).
//! - [`LocalRace`]: the offline path with the same [`RaceDriver`] / [`RaceView`]
//!   API (solo vs bots, training), so the app has one race screen.
//! - [`transport`]: the `HostTransport` / `ClientTransport` traits, the
//!   loopback hub (virtual clock, latency, jitter, loss) and the mux.
//! - [`net`]: QUIC over `decented-net` (room code online, Nearby on the LAN).
//! - [`proto`]: the wire messages.
//!
//! Every update takes the time explicitly (`now_ms`), so whole races run on a
//! virtual clock in tests. See `docs/api/race-session.md`.

#[cfg(feature = "native")]
pub mod client;
#[cfg(feature = "native")]
pub mod clock;
pub mod host;
#[cfg(feature = "native")]
pub mod invite;
#[cfg(feature = "native")]
pub mod local;
#[cfg(feature = "native")]
pub mod net;
pub mod proto;
pub mod transport;
mod track_transfer;
pub mod view;

#[cfg(all(test, feature = "native"))]
mod tests;

#[cfg(feature = "native")]
pub use client::{ClientPhase, ClientSession, ClientStats, MigrationClientBatch};
pub use host::{HostAuthority, HostConfig, HostPhase, HostStats};
#[cfg(feature = "native")]
pub use local::LocalRace;
#[cfg(feature = "native")]
pub use invite::{format_invite_link, parse_invite_link};
#[cfg(feature = "native")]
pub use net::{
    browse_nearby, host_room, join_code, join_invite, join_nearby, load_identity, net_config, Admission, InviteId, JoinInfo,
    QuicClient, QuicHost, APP_TAG, INVITE_NOT_ADMITTED, INVITE_NO_ANSWER,
};
pub use proto::{BattleNet, BattleSetup, KeepPass, LobbyView, ResultRow, ResultsView, SeatView, PROTO_VERSION};
pub use transport::{
    ClientNetEvent, ClientTransport, HostIdentity, HostNetEvent, HostTransport, LinkPath, NullHostTransport, PeerKey,
};
#[cfg(feature = "native")]
pub use transport::{LoopClock, LoopbackConfig, LoopbackHub, MuxHostTransport};
pub use view::{Hud, RaceDriver, RaceView};

/// Build the host and the host player's own session, joined by a zero-latency
/// loopback beside the (optional) remote transport (QUIC, from [`net`]).
/// Drive both every frame: `host.update(now)` then `me.update(now, input)`.
#[cfg(feature = "native")]
pub fn host_with_local_player(
    remote: Option<Box<dyn HostTransport>>,
    cfg: HostConfig,
    name: &str,
    character: u8,
) -> (HostAuthority, ClientSession) {
    let hub = LoopbackHub::new(LoopClock::real(), cfg.seed);
    let local_end = hub.host();
    let mux = MuxHostTransport::new(local_end, remote.unwrap_or_else(|| Box::new(NullHostTransport)));
    let me = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), name, character);
    (HostAuthority::new(Box::new(mux), cfg), me)
}

/// Complete migration checkpoints; networking and publication stay in Fray.
#[cfg(feature = "native")]
pub use host::migration;

#[cfg(feature = "authority")]
pub use host::authority;
