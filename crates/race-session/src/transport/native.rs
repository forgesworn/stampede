use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rand::{Rng, SeedableRng};
use rand_pcg::Pcg64Mcg;

// ─── Loopback ───────────────────────────────────────────────────────────────

/// Where the loopback reads the time: the wall clock (practice, headless) or a
/// manual clock a test advances.
#[derive(Clone, Debug)]
pub enum LoopClock {
    Real(Instant),
    Manual(Arc<AtomicU64>),
}

impl LoopClock {
    pub fn real() -> Self {
        LoopClock::Real(Instant::now())
    }

    pub fn manual() -> (Self, Arc<AtomicU64>) {
        let a = Arc::new(AtomicU64::new(0));
        (LoopClock::Manual(Arc::clone(&a)), a)
    }

    pub fn now_ms(&self) -> u64 {
        match self {
            LoopClock::Real(t) => t.elapsed().as_millis() as u64,
            LoopClock::Manual(a) => a.load(Ordering::Relaxed),
        }
    }
}

/// Link impairments for one loopback peer (applied in both directions).
#[derive(Clone, Copy, Debug)]
pub struct LoopbackConfig {
    /// One-way delay.
    pub latency_ms: u32,
    /// Extra random one-way delay on lossy messages only (reliable stays ordered).
    pub jitter_ms: u32,
    /// Probability a lossy message is dropped. Reliable messages are never dropped.
    pub lossy_loss: f32,
}

impl LoopbackConfig {
    pub const PERFECT: LoopbackConfig = LoopbackConfig {
        latency_ms: 0,
        jitter_ms: 0,
        lossy_loss: 0.0,
    };
}

struct Pending<E> {
    at: u64,
    seq: u64,
    ev: E,
}

struct PeerLink {
    cfg: LoopbackConfig,
    /// Reliable messages are delivered no earlier than the previous one, per direction.
    last_up: u64,
    last_down: u64,
    to_client: Vec<Pending<ClientNetEvent>>,
    open: bool,
}

struct HubInner {
    clock: LoopClock,
    rng: Pcg64Mcg,
    seq: u64,
    next_peer: PeerKey,
    to_host: Vec<Pending<HostNetEvent>>,
    peers: HashMap<PeerKey, PeerLink>,
    host_open: bool,
    host_gone: bool,
    lossy_sent: u64,
    lossy_dropped: u64,
}

impl HubInner {
    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    fn push_host(&mut self, at: u64, ev: HostNetEvent) {
        self.seq += 1;
        let seq = self.seq;
        self.to_host.push(Pending { at, seq, ev });
    }

    fn push_client(&mut self, peer: PeerKey, at: u64, ev: ClientNetEvent) {
        self.seq += 1;
        let seq = self.seq;
        if let Some(p) = self.peers.get_mut(&peer) {
            p.to_client.push(Pending { at, seq, ev });
        }
    }

    /// Delivery time for a reliable message in one direction (keeps order).
    fn reliable_at(&mut self, peer: PeerKey, up: bool) -> Option<u64> {
        let now = self.now();
        let p = self.peers.get_mut(&peer)?;
        let at = now + p.cfg.latency_ms as u64;
        let last = if up { &mut p.last_up } else { &mut p.last_down };
        let at = at.max(*last);
        *last = at;
        Some(at)
    }

    /// Delivery time for a lossy message, or `None` if it is dropped.
    fn lossy_at(&mut self, peer: PeerKey) -> Option<u64> {
        let now = self.now();
        let cfg = self.peers.get(&peer)?.cfg;
        self.lossy_sent += 1;
        if cfg.lossy_loss > 0.0 && self.rng.gen::<f32>() < cfg.lossy_loss {
            self.lossy_dropped += 1;
            return None;
        }
        let jitter = if cfg.jitter_ms > 0 {
            self.rng.gen_range(0..=cfg.jitter_ms)
        } else {
            0
        };
        Some(now + cfg.latency_ms as u64 + jitter as u64)
    }
}

fn pop_due<E>(q: &mut Vec<Pending<E>>, now: u64) -> Option<E> {
    let idx = q
        .iter()
        .enumerate()
        .filter(|(_, p)| p.at <= now)
        .min_by_key(|(_, p)| (p.at, p.seq))
        .map(|(i, _)| i)?;
    Some(q.swap_remove(idx).ev)
}

/// An in-process network: one host end, any number of client ends.
#[derive(Clone)]
pub struct LoopbackHub {
    inner: Arc<Mutex<HubInner>>,
}

impl LoopbackHub {
    pub fn new(clock: LoopClock, seed: u64) -> Self {
        LoopbackHub {
            inner: Arc::new(Mutex::new(HubInner {
                clock,
                rng: Pcg64Mcg::seed_from_u64(seed),
                seq: 0,
                next_peer: 1,
                to_host: Vec::new(),
                peers: HashMap::new(),
                host_open: true,
                host_gone: false,
                lossy_sent: 0,
                lossy_dropped: 0,
            })),
        }
    }

    /// The host end. Call once.
    pub fn host(&self) -> LoopbackHost {
        LoopbackHost {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Connect a new client with these impairments. The host sees `Joined`
    /// after one latency; the client sees `Connected` after a round trip.
    pub fn connect(&self, cfg: LoopbackConfig) -> LoopbackClient {
        self.connect_via(cfg, LinkPath::Local)
    }

    /// As [`LoopbackHub::connect`], with the host told the link came over `path`.
    pub fn connect_via(&self, cfg: LoopbackConfig, path: LinkPath) -> LoopbackClient {
        let mut g = self.inner.lock().expect("hub");
        let peer = g.next_peer;
        g.next_peer += 1;
        let now = g.now();
        g.peers.insert(
            peer,
            PeerLink {
                cfg,
                last_up: 0,
                last_down: 0,
                to_client: Vec::new(),
                open: true,
            },
        );
        if g.host_gone || !g.host_open {
            let at = now + 2 * cfg.latency_ms as u64;
            let reason = if g.host_gone {
                "no host"
            } else {
                "room closed"
            };
            g.push_client(peer, at, ClientNetEvent::Failed(reason.into()));
            g.peers.get_mut(&peer).expect("just inserted").open = false;
        } else {
            let at = g.reliable_at(peer, true).expect("peer exists");
            g.push_host(at, HostNetEvent::Joined { peer, path });
            let back = g.reliable_at(peer, false).expect("peer exists") + cfg.latency_ms as u64;
            g.push_client(peer, back, ClientNetEvent::Connected { host: None });
            if let Some(p) = g.peers.get_mut(&peer) {
                p.last_down = back;
            }
        }
        LoopbackClient {
            inner: Arc::clone(&self.inner),
            peer,
        }
    }

    /// `(lossy sent, lossy dropped)` so far.
    pub fn lossy_counts(&self) -> (u64, u64) {
        let g = self.inner.lock().expect("hub");
        (g.lossy_sent, g.lossy_dropped)
    }
}

pub struct LoopbackHost {
    inner: Arc<Mutex<HubInner>>,
}

impl LoopbackHost {
    /// Close a peer's link, telling its client `reason`.
    fn drop_peer(&mut self, peer: PeerKey, reason: &str) {
        let Ok(mut g) = self.inner.lock() else { return };
        if let Some(at) = g.reliable_at(peer, false) {
            if let Some(p) = g.peers.get_mut(&peer) {
                if !p.open {
                    return;
                }
                p.open = false;
            }
            g.push_client(
                peer,
                at,
                ClientNetEvent::Disconnected {
                    host_left: false,
                    reason: reason.into(),
                },
            );
        }
    }
}

impl HostTransport for LoopbackHost {
    fn poll(&mut self) -> Option<HostNetEvent> {
        let mut g = self.inner.lock().ok()?;
        let now = g.now();
        pop_due(&mut g.to_host, now)
    }

    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        let Ok(mut g) = self.inner.lock() else { return };
        if !g.peers.get(&peer).is_some_and(|p| p.open) {
            return;
        }
        if let Some(at) = g.reliable_at(peer, false) {
            g.push_client(peer, at, ClientNetEvent::Reliable(bytes));
        }
    }

    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        let Ok(mut g) = self.inner.lock() else { return };
        if !g.peers.get(&peer).is_some_and(|p| p.open) {
            return;
        }
        if let Some(at) = g.lossy_at(peer) {
            g.push_client(peer, at, ClientNetEvent::Lossy(bytes));
        }
    }

    fn kick(&mut self, peer: PeerKey) {
        self.drop_peer(peer, "kicked");
    }

    fn confirm_timeout(&mut self, peer: PeerKey) {
        // The same string the QUIC client surfaces (`DisconnectReason` Debug).
        self.drop_peer(peer, "ConfirmTimeout");
    }

    fn set_open(&mut self, open: bool) {
        if let Ok(mut g) = self.inner.lock() {
            g.host_open = open;
        }
    }

    fn set_open_keeping_token(&mut self, open: bool) {
        self.set_open(open);
    }

    fn rtt_ms(&self, peer: PeerKey) -> Option<u32> {
        let g = self.inner.lock().ok()?;
        g.peers.get(&peer).map(|p| 2 * p.cfg.latency_ms)
    }

    fn shutdown(&mut self) {
        let Ok(mut g) = self.inner.lock() else { return };
        g.host_gone = true;
        let peers: Vec<PeerKey> = g
            .peers
            .iter()
            .filter(|(_, p)| p.open)
            .map(|(k, _)| *k)
            .collect();
        for peer in peers {
            if let Some(at) = g.reliable_at(peer, false) {
                g.push_client(
                    peer,
                    at,
                    ClientNetEvent::Disconnected {
                        host_left: true,
                        reason: "host left".into(),
                    },
                );
            }
            if let Some(p) = g.peers.get_mut(&peer) {
                p.open = false;
            }
        }
    }
}

impl Drop for LoopbackHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct LoopbackClient {
    inner: Arc<Mutex<HubInner>>,
    peer: PeerKey,
}

impl ClientTransport for LoopbackClient {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        let mut g = self.inner.lock().ok()?;
        let now = g.now();
        let p = g.peers.get_mut(&self.peer)?;
        pop_due(&mut p.to_client, now)
    }

    fn send_reliable(&mut self, bytes: Vec<u8>) {
        let Ok(mut g) = self.inner.lock() else { return };
        if g.host_gone || !g.peers.get(&self.peer).is_some_and(|p| p.open) {
            return;
        }
        if let Some(at) = g.reliable_at(self.peer, true) {
            g.push_host(
                at,
                HostNetEvent::Reliable {
                    peer: self.peer,
                    bytes,
                },
            );
        }
    }

    fn send_lossy(&mut self, bytes: Vec<u8>) {
        let Ok(mut g) = self.inner.lock() else { return };
        if g.host_gone || !g.peers.get(&self.peer).is_some_and(|p| p.open) {
            return;
        }
        if let Some(at) = g.lossy_at(self.peer) {
            g.push_host(
                at,
                HostNetEvent::Lossy {
                    peer: self.peer,
                    bytes,
                },
            );
        }
    }

    fn rtt_ms(&self) -> Option<u32> {
        let g = self.inner.lock().ok()?;
        g.peers.get(&self.peer).map(|p| 2 * p.cfg.latency_ms)
    }

    fn close(&mut self) {
        let Ok(mut g) = self.inner.lock() else { return };
        let was_open = g.peers.get(&self.peer).is_some_and(|p| p.open);
        if !was_open {
            return;
        }
        if let Some(at) = g.reliable_at(self.peer, true) {
            g.push_host(at, HostNetEvent::Left { peer: self.peer });
        }
        if let Some(p) = g.peers.get_mut(&self.peer) {
            p.open = false;
            p.to_client.clear();
        }
    }
}

impl Drop for LoopbackClient {
    fn drop(&mut self) {
        self.close();
    }
}

// ─── Mux: the host's own player + remote peers ──────────────────────────────

/// Peer keys at or above this belong to the local (loopback) side.

/// Merges a local loopback host end (the host player's own field) with a
/// remote transport (QUIC). Local peer keys are offset by `1 << 31`.
pub struct MuxHostTransport {
    local: LoopbackHost,
    remote: Box<dyn HostTransport>,
}

impl MuxHostTransport {
    pub fn new(local: LoopbackHost, remote: Box<dyn HostTransport>) -> Self {
        MuxHostTransport { local, remote }
    }
}

fn shift(ev: HostNetEvent) -> HostNetEvent {
    match ev {
        HostNetEvent::Joined { peer, path } => HostNetEvent::Joined {
            peer: peer + LOCAL_PEER_BASE,
            path,
        },
        HostNetEvent::Left { peer } => HostNetEvent::Left {
            peer: peer + LOCAL_PEER_BASE,
        },
        HostNetEvent::Reliable { peer, bytes } => HostNetEvent::Reliable {
            peer: peer + LOCAL_PEER_BASE,
            bytes,
        },
        HostNetEvent::Lossy { peer, bytes } => HostNetEvent::Lossy {
            peer: peer + LOCAL_PEER_BASE,
            bytes,
        },
    }
}

impl HostTransport for MuxHostTransport {
    fn poll(&mut self) -> Option<HostNetEvent> {
        self.local.poll().map(shift).or_else(|| self.remote.poll())
    }

    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        if peer >= LOCAL_PEER_BASE {
            self.local.send_reliable(peer - LOCAL_PEER_BASE, bytes)
        } else {
            self.remote.send_reliable(peer, bytes)
        }
    }

    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        if peer >= LOCAL_PEER_BASE {
            self.local.send_lossy(peer - LOCAL_PEER_BASE, bytes)
        } else {
            self.remote.send_lossy(peer, bytes)
        }
    }

    fn kick(&mut self, peer: PeerKey) {
        if peer >= LOCAL_PEER_BASE {
            self.local.kick(peer - LOCAL_PEER_BASE)
        } else {
            self.remote.kick(peer)
        }
    }

    fn disconnect(&mut self, peer: PeerKey) {
        if peer >= LOCAL_PEER_BASE {
            self.local.disconnect(peer - LOCAL_PEER_BASE)
        } else {
            self.remote.disconnect(peer)
        }
    }

    fn confirm_timeout(&mut self, peer: PeerKey) {
        if peer >= LOCAL_PEER_BASE {
            self.local.confirm_timeout(peer - LOCAL_PEER_BASE)
        } else {
            self.remote.confirm_timeout(peer)
        }
    }

    fn set_open(&mut self, open: bool) {
        self.remote.set_open(open);
    }

    fn set_open_keeping_token(&mut self, open: bool) {
        self.remote.set_open_keeping_token(open);
    }

    fn rtt_ms(&self, peer: PeerKey) -> Option<u32> {
        if peer >= LOCAL_PEER_BASE {
            Some(0)
        } else {
            self.remote.rtt_ms(peer)
        }
    }

    fn shutdown(&mut self) {
        self.remote.shutdown();
        self.local.shutdown();
    }
}

// ─── Null host ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reliable_is_delayed_ordered_and_never_dropped() {
        let (clock, t) = LoopClock::manual();
        let hub = LoopbackHub::new(clock, 1);
        let mut host = hub.host();
        let mut c = hub.connect(LoopbackConfig {
            latency_ms: 100,
            jitter_ms: 50,
            lossy_loss: 0.5,
        });
        assert_eq!(host.poll(), None, "nothing before the latency elapses");
        t.store(100, Ordering::Relaxed);
        let Some(HostNetEvent::Joined { peer, .. }) = host.poll() else {
            panic!()
        };
        for i in 0..200u8 {
            c.send_reliable(vec![i]);
        }
        t.store(199, Ordering::Relaxed);
        assert_eq!(host.poll(), None);
        t.store(1000, Ordering::Relaxed);
        let got: Vec<u8> = std::iter::from_fn(|| host.poll())
            .map(|e| match e {
                HostNetEvent::Reliable { bytes, .. } => bytes[0],
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(got, (0..200).collect::<Vec<u8>>());
        host.send_reliable(peer, vec![9]);
        t.store(1100, Ordering::Relaxed);
        assert_eq!(c.poll(), Some(ClientNetEvent::Connected { host: None }));
        assert_eq!(c.poll(), Some(ClientNetEvent::Reliable(vec![9])));
    }

    #[test]
    fn lossy_loss_rate_is_roughly_as_configured() {
        let (clock, t) = LoopClock::manual();
        let hub = LoopbackHub::new(clock, 7);
        let mut host = hub.host();
        let mut c = hub.connect(LoopbackConfig {
            latency_ms: 10,
            jitter_ms: 5,
            lossy_loss: 0.05,
        });
        for _ in 0..10_000 {
            c.send_lossy(vec![1]);
        }
        t.store(1000, Ordering::Relaxed);
        let n = std::iter::from_fn(|| host.poll())
            .filter(|e| matches!(e, HostNetEvent::Lossy { .. }))
            .count();
        assert!((9300..=9700).contains(&n), "{n} of 10000 delivered");
    }

    #[test]
    fn close_and_shutdown_are_seen_by_the_other_side() {
        let (clock, t) = LoopClock::manual();
        let hub = LoopbackHub::new(clock, 1);
        let mut host = hub.host();
        let mut a = hub.connect(LoopbackConfig::PERFECT);
        let mut b = hub.connect(LoopbackConfig::PERFECT);
        t.store(1, Ordering::Relaxed);
        a.close();
        let evs: Vec<_> = std::iter::from_fn(|| host.poll()).collect();
        assert!(evs.contains(&HostNetEvent::Left { peer: 1 }), "{evs:?}");
        host.shutdown();
        let evs: Vec<_> = std::iter::from_fn(|| b.poll()).collect();
        assert!(evs.iter().any(|e| matches!(
            e,
            ClientNetEvent::Disconnected {
                host_left: true,
                ..
            }
        )));
    }
}
