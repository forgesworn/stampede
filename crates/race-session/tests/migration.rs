//! Real game-consumer network checks. The real subprocess kill trial (a
//! SIGKILLed `stampede-headless --migration-host`) is in `subprocess_kill`.

use decented_net::migration::*;
use decented_net::{Identity, NetConfig, PeerId, RoomInfo};
use race_core::{Input, RacePhase, RacerKind};
use race_session::{
    migration, ClientNetEvent, ClientSession, ClientTransport, HostAuthority, HostConfig,
    HostNetEvent, HostTransport, PeerKey, QuicClient, QuicHost,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "migration/subprocess_kill.rs"]
mod subprocess_kill;

#[derive(Clone)]
struct HostLink(Arc<Mutex<QuicHost>>, Arc<Mutex<BTreeSet<PeerKey>>>);
impl HostTransport for HostLink {
    fn poll(&mut self) -> Option<HostNetEvent> {
        let event = self.0.lock().unwrap().poll()?;
        if let HostNetEvent::Joined { peer, .. } = &event {
            self.1.lock().unwrap().insert(*peer);
        }
        Some(event)
    }
    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_reliable(peer, bytes);
    }
    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_lossy(peer, bytes);
    }
    fn kick(&mut self, peer: PeerKey) {
        self.0.lock().unwrap().kick(peer);
    }
    fn set_open(&mut self, open: bool) {
        self.0.lock().unwrap().set_open(open);
    }
    fn set_open_keeping_token(&mut self, open: bool) {
        self.0.lock().unwrap().set_open_keeping_token(open);
    }
    fn rtt_ms(&self, peer: PeerKey) -> Option<u32> {
        self.0.lock().unwrap().rtt_ms(peer)
    }
    fn shutdown(&mut self) {
        self.0.lock().unwrap().shutdown();
    }
}
#[derive(Clone)]
struct ClientLink(Arc<Mutex<QuicClient>>);
impl ClientTransport for ClientLink {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        self.0.lock().unwrap().poll()
    }
    fn send_reliable(&mut self, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_reliable(bytes);
    }
    fn send_lossy(&mut self, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_lossy(bytes);
    }
    fn rtt_ms(&self) -> Option<u32> {
        self.0.lock().unwrap().rtt_ms()
    }
    fn close(&mut self) {
        self.0.lock().unwrap().close();
    }
}

fn approved() -> Identity {
    let persona = decented_net::Keys::generate();
    let runtime = Identity::ephemeral();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let event = decented_signet::attestation::unsigned_attestation(
        race_session::APP_TAG,
        &persona.public_key(),
        &runtime.pubkey(),
        now,
    )
    .sign_with_keys(&persona)
    .unwrap();
    runtime.with_attestation(serde_json::to_string(&event).unwrap())
}
#[track_caller]
fn bounded(started: Instant) {
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "real QUIC setup deadline"
    );
    std::thread::sleep(Duration::from_millis(1));
}

/// Drive the original (and, through `other`, the standby end) until the
/// original has learned over the pair link the standby's certificate for its
/// own installed receipt, and return that certificate. The original's side
/// never reads the standby's certificate in process.
fn original_cert(
    live: &mut migration::LivePrimary,
    mut other: impl FnMut(),
) -> MembershipCertificate {
    let t = Instant::now();
    loop {
        if let Some(cert) = live.membership_certificate() {
            if cert.receipt == *live.primary().receipt() {
                return cert.clone();
            }
        }
        other();
        for event in live.poll().unwrap() {
            assert!(
                matches!(event, migration::RuntimeEvent::Membership(_)),
                "only a certificate may arrive while waiting for one"
            );
        }
        bounded(t);
    }
}

struct Lobby<const N: usize = 2> {
    cfg: NetConfig,
    original: Identity,
    identities: [Identity; N],
    network: HostLink,
    links: [ClientLink; N],
    host: HostAuthority,
    clients: [ClientSession; N],
    peers: [PeerKey; N],
    /// A phone host's own player (its loopback client), if the original plays.
    host_client: Option<ClientSession>,
    /// The LAN token every client joined with. A lobby that filled up has
    /// rotated the listener's token since (the native close).
    joined_token: String,
    /// A Bluetooth room's link queue, kept open for the room's lifetime.
    _radio: Option<decented_net::VirtualLinkSender>,
}
impl Lobby<2> {
    fn new() -> Self {
        Self::with_humans()
    }
}
impl<const N: usize> Lobby<N> {
    /// `N` approved humans (the first is the standby device) and `8 - N` bots.
    fn with_humans() -> Self {
        Self::build(false)
    }
    /// As [`Self::with_humans`]; with `phone` the original also seats its own
    /// player over the native loopback (a playing phone host), one bot fewer.
    fn build(phone: bool) -> Self {
        Self::build_in(phone, false)
    }
    /// As [`Self::build`]; with `online` the room is also published under an
    /// online code (an in-memory relay, so nothing leaves this machine). The
    /// players still join over the LAN address.
    fn build_in(phone: bool, online: bool) -> Self {
        Self::build_room(phone, online, false)
    }
    /// As [`Self::build_in`]; with `bluetooth` the room is a virtual-link
    /// (Bluetooth) host and every player joins over its own virtual link.
    fn build_room(phone: bool, online: bool, bluetooth: bool) -> Self {
        let mut cfg = NetConfig::new(
            std::env::temp_dir().join("stampede-migration-unused"),
            race_session::APP_TAG,
        )
        .lan_only();
        if N == 8 && !phone {
            // A dedicated (headless) host: all eight seats are remote. The
            // smaller lobbies keep the default, which has room for them.
            cfg.host_has_player = false;
        }
        let original = approved();
        let identities: [Identity; N] = std::array::from_fn(|_| approved());
        let room = RoomInfo {
            name: "Consent test".into(),
            max_players: 8,
        };
        let mut radio = None;
        let handle = if bluetooth {
            // A virtual host withholds its attestation by default; Stampede
            // joins require it.
            cfg.withhold_host_attestation = Some(false);
            let (tx, rx) = decented_net::virtual_link_queue(16);
            radio = Some(tx);
            decented_net::host_virtual(&cfg, &original, room, rx)
        } else if online {
            let mut online_cfg = cfg.clone();
            online_cfg.relays = vec!["wss://relay.invalid".into()];
            decented_net::host_with_relay(
                &online_cfg,
                &original,
                room,
                Some(Box::new(decented_net::relay::FakeRelayHub::new().client())),
            )
        } else {
            decented_net::host(&cfg, &original, room)
        };
        let network = HostLink(
            Arc::new(Mutex::new(QuicHost::new(handle))),
            Arc::new(Mutex::new(BTreeSet::new())),
        );
        let started = Instant::now();
        let (addr, token) = loop {
            let mut net = network.0.lock().unwrap();
            assert!(net.poll().is_none());
            assert!(net.failure().is_none(), "listener: {:?}", net.failure());
            if net.is_ready() {
                let h = net.handle().unwrap();
                break (
                    h.local_addr()
                        .map(|a| std::net::SocketAddr::from(([127, 0, 0, 1], a.port()))),
                    h.lan_token(),
                );
            }
            drop(net);
            bounded(started);
        };
        let links = identities.each_ref().map(|id| {
            let handle = match (&radio, addr) {
                (Some(radio), _) => {
                    let (joiner, host_end) = decented_net::virtual_pair("BLE", "host", 256);
                    radio.try_send(host_end).expect("the host takes the link");
                    decented_net::join_virtual(&cfg, id, joiner)
                }
                (None, Some(addr)) => decented_net::join_addr(&cfg, id, addr, &token),
                (None, None) => panic!("a LAN room has an address"),
            };
            ClientLink(Arc::new(Mutex::new(QuicClient::new(handle))))
        });
        let names = [
            "Backup", "Other", "Third", "Fourth", "Fifth", "Sixth", "Seventh", "Eighth",
        ];
        let mut clients: [ClientSession; N] = std::array::from_fn(|i| {
            ClientSession::new(Box::new(links[i].clone()), names[i], i as u8)
        });
        let config = HostConfig {
            bots: (8 - N - usize::from(phone)) as u8,
            auto_start: false,
            ..HostConfig::default()
        };
        let (mut host, mut host_client) = if phone {
            use race_session::transport::{LoopClock, LoopbackConfig, LoopbackHub};
            let hub = LoopbackHub::new(LoopClock::real(), 1);
            let mux = race_session::transport::MuxHostTransport::new(
                hub.host(),
                Box::new(network.clone()),
            );
            let own = ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Host", 5);
            (HostAuthority::new(Box::new(mux), config), Some(own))
        } else {
            (
                HostAuthority::new_headless(Box::new(network.clone()), config),
                None,
            )
        };
        loop {
            let now = started.elapsed().as_millis() as u64;
            host.update(now);
            for client in clients.iter_mut().chain(host_client.as_mut()) {
                client.update(now, Input::default());
                assert!(!client.phase().is_dead(), "join: {:?}", client.phase());
            }
            if host.human_count() == N + usize::from(phone)
                && clients
                    .iter()
                    .chain(host_client.as_ref())
                    .all(|c| matches!(c.phase(), race_session::ClientPhase::Lobby))
            {
                break;
            }
            bounded(started);
        }
        // The following tests freeze native updates here. Raw ordinary frames
        // below carry only setup records; this is not a production app driver.
        let peers = identities.each_ref().map(|id| {
            *network
                .1
                .lock()
                .unwrap()
                .iter()
                .find(|&&peer| {
                    network.0.lock().unwrap().migration_peer_key(peer).unwrap()
                        == id.pubkey().to_bytes()
                })
                .unwrap()
        });
        Self {
            cfg,
            original,
            identities,
            network,
            links,
            host,
            clients,
            peers,
            host_client,
            joined_token: token,
            _radio: radio,
        }
    }
    fn offer(&self) -> migration::LobbyOffer {
        migration::LobbyOffer::prepare(
            &self.host,
            &self.network.0.lock().unwrap(),
            self.original.keys(),
            self.peers[0],
        )
        .unwrap()
    }
    fn consent(&self, offer: &migration::LobbyOffer) -> Descriptor {
        migration::consent_lobby_offer(
            &self.links[0].0.lock().unwrap(),
            &self.identities[0],
            self.network.0.lock().unwrap().code().unwrap(),
            &offer.descriptor().encode().unwrap(),
        )
        .unwrap()
    }
    fn connect_pair(
        &self,
        descriptor: &Descriptor,
        standby: &Identity,
    ) -> (QuicHost, QuicClient, PeerKey) {
        self.connect_pair_as(descriptor, standby, &self.original)
    }
    fn connect_pair_as(
        &self,
        descriptor: &Descriptor,
        standby: &Identity,
        original: &Identity,
    ) -> (QuicHost, QuicClient, PeerKey) {
        let started = Instant::now();
        let root = &descriptor.root;
        let code = self.network.0.lock().unwrap().code().unwrap().to_owned();
        let mut listener = QuicHost::new(
            decented_net::host_standby(&self.cfg, standby, &code, descriptor.clone()).unwrap(),
        );
        let (addr, token) = loop {
            assert!(listener.poll().is_none());
            if listener.is_ready() {
                let h = listener.handle().unwrap();
                break (
                    std::net::SocketAddr::from(([127, 0, 0, 1], h.local_addr().unwrap().port())),
                    h.lan_token(),
                );
            }
            bounded(started);
        };
        let binding = ConnectionBinding {
            session: root.session,
            descriptor: root.digest().unwrap(),
            term: 0,
            client: LinkRole::Original,
            server: LinkRole::Standby,
        };
        let mut link = QuicClient::new(
            decented_net::join_session_addr(
                &self.cfg,
                original,
                addr,
                &token,
                binding,
                standby.pubkey(),
            )
            .unwrap(),
        );
        let mut peer = None;
        loop {
            if let Some(HostNetEvent::Joined { peer: p, .. }) = listener.poll() {
                peer = Some(p);
            }
            let _ = link.poll();
            if peer.is_some() && link.migration_keys().is_ok() {
                break;
            }
            bounded(started);
        }
        let peer = peer.unwrap();
        (listener, link, peer)
    }
    fn activation(
        &self,
        original: &Identity,
    ) -> (migration::LobbyActivation, migration::LobbyStandby) {
        let (_, source, sink) = self.activation_for(original);
        (source, sink)
    }
    fn activation_for(
        &self,
        original: &Identity,
    ) -> (
        Descriptor,
        migration::LobbyActivation,
        migration::LobbyStandby,
    ) {
        let offer = self.offer();
        let consent = self.consent(&offer);
        let consented = offer
            .accept(
                &self.host,
                &self.network.0.lock().unwrap(),
                self.peers[0],
                consent,
            )
            .unwrap();
        let descriptor = consented.descriptor().clone();
        let initial = consented.initial_hash().unwrap();
        let (listener, link, peer) =
            self.connect_pair_as(&descriptor, &self.identities[0], original);
        let sink = migration::LobbyStandby::new(descriptor.clone(), listener, peer, 0).unwrap();
        let mut source = consented.into_activation(link, 0).unwrap();
        let own = self.host_client.is_some().then_some(&self.original);
        for identity in self.identities.iter().chain(own) {
            let member = descriptor
                .root
                .members
                .iter()
                .find(|m| m.key == Some(identity.pubkey().to_bytes()))
                .unwrap();
            source
                .acknowledge(
                    CapabilityAck::sign(
                        &descriptor.root,
                        member.player,
                        initial,
                        1,
                        identity.keys(),
                    )
                    .unwrap(),
                    0,
                )
                .unwrap();
        }
        (descriptor, source, sink)
    }
}

#[test]
fn lobby_consent_binds_admitted_keys_selected_peer_and_unchanged_native_state() {
    let mut lobby = Lobby::new();
    assert_eq!(lobby.clients[0].lobby().unwrap().humans(), 2);
    let offer = lobby.offer();
    let root = offer.descriptor().root.clone();
    assert_eq!(root.members.len(), 2); // six native bots need no runtime votes/seats
    assert_eq!(root.host, lobby.original.pubkey().to_bytes());
    assert_eq!(root.standby, lobby.identities[0].pubkey().to_bytes());
    assert!(offer.descriptor().verify().is_err());
    assert!(offer.descriptor().encode().unwrap().len() <= decented_net::MAX_RELIABLE_LEN);
    let code = lobby.network.0.lock().unwrap().code().unwrap().to_owned();
    let consent = lobby.consent(&offer);
    assert!(migration::consent_lobby_offer(
        &lobby.links[0].0.lock().unwrap(),
        &lobby.identities[1],
        &code,
        &offer.descriptor().encode().unwrap(),
    )
    .is_err());
    assert!(migration::consent_lobby_offer(
        &lobby.links[1].0.lock().unwrap(),
        &lobby.identities[1],
        &code,
        &offer.descriptor().encode().unwrap(),
    )
    .is_err());
    assert!(migration::consent_lobby_offer(
        &lobby.links[0].0.lock().unwrap(),
        &lobby.identities[0],
        "ABCDEF",
        &offer.descriptor().encode().unwrap(),
    )
    .is_err());
    let mut impostor = root.clone();
    impostor.host = lobby.identities[1].pubkey().to_bytes();
    impostor.controller = impostor.host;
    let impostor = Descriptor::offer(impostor, lobby.identities[1].keys()).unwrap();
    assert!(migration::consent_lobby_offer(
        &lobby.links[0].0.lock().unwrap(),
        &lobby.identities[0],
        &code,
        &impostor.encode().unwrap(),
    )
    .is_err());
    assert!(lobby
        .offer()
        .accept(
            &lobby.host,
            &lobby.network.0.lock().unwrap(),
            lobby.peers[1],
            consent.clone(),
        )
        .is_err());
    let mut changed = consent.clone();
    changed.root.roster_revision += 1;
    assert!(lobby
        .offer()
        .accept(
            &lobby.host,
            &lobby.network.0.lock().unwrap(),
            lobby.peers[0],
            changed,
        )
        .is_err());
    let offer = lobby.offer();
    let consent = lobby.consent(&offer);
    lobby.host.set_laps(2);
    assert!(offer
        .accept(
            &lobby.host,
            &lobby.network.0.lock().unwrap(),
            lobby.peers[0],
            consent,
        )
        .is_err());
}

#[test]
fn selected_peer_loss_before_consent_acceptance_cannot_unlock_the_image() {
    let lobby = Lobby::new();
    let offer = lobby.offer();
    let consent = lobby.consent(&offer);
    lobby.links[0].0.lock().unwrap().close();
    let started = Instant::now();
    while lobby
        .network
        .0
        .lock()
        .unwrap()
        .handle()
        .unwrap()
        .peer_key(PeerId(lobby.peers[0]))
        .is_some()
    {
        bounded(started);
    }
    // Do not poll the native host: its roster and approved map still describe
    // the old connection. The live Identify check must reject that stale state.
    assert!(offer
        .accept(
            &lobby.host,
            &lobby.network.0.lock().unwrap(),
            lobby.peers[0],
            consent,
        )
        .is_err());
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// Real signed credentials and the unchanged verifier, without a fake UTC
// parameter or a production test bypass. Mint near the end of the normal TTL.
fn shortly_expiring(identity: &Identity) -> (Identity, u64) {
    let persona = decented_net::Keys::generate();
    let until = unix_now() + 4;
    let from = until - decented_signet::attestation::VALIDITY_SECS;
    let event = decented_signet::attestation::unsigned_attestation(
        race_session::APP_TAG,
        &persona.public_key(),
        &identity.pubkey(),
        from,
    )
    .sign_with_keys(&persona)
    .unwrap();
    (
        identity
            .clone()
            .with_attestation(serde_json::to_string(&event).unwrap()),
        until,
    )
}

#[test]
fn delayed_approval_expiry_blocks_image_staging_and_queued_image_release() {
    let lobby = Lobby::new();
    let prepare = || {
        let offer = lobby.offer();
        let consent = lobby.consent(&offer);
        offer
            .accept(
                &lobby.host,
                &lobby.network.0.lock().unwrap(),
                lobby.peers[0],
                consent,
            )
            .unwrap()
    };
    let unstaged = prepare();
    let queued = prepare();
    let (expiring, until) = shortly_expiring(&lobby.identities[0]);
    let (mut first_listener, first_link, _) = lobby.connect_pair(unstaged.descriptor(), &expiring);
    let (mut second_listener, second_link, _) = lobby.connect_pair(queued.descriptor(), &expiring);
    assert!(first_link.migration_keys().is_ok());
    assert!(second_link.migration_keys().is_ok());
    let mut queued = queued.into_activation(second_link, 0).unwrap();
    let started = Instant::now();
    while unix_now() < until {
        assert!(!matches!(
            first_listener.poll(),
            Some(HostNetEvent::Reliable { .. })
        ));
        assert!(!matches!(
            second_listener.poll(),
            Some(HostNetEvent::Reliable { .. })
        ));
        bounded(started);
    }
    assert!(matches!(
        unstaged.into_activation(first_link, 0),
        Err(Error::Unauthorized)
    ));
    // Even though the image has already been queued privately, no source
    // frame is returned or sent once actual UTC invalidates admission.
    assert_eq!(
        queued.poll(started.elapsed().as_millis() as u64),
        Err(Error::Unauthorized)
    );
    assert_eq!(
        queued.poll(started.elapsed().as_millis() as u64),
        Err(Error::Closed)
    );
    assert!(!matches!(
        first_listener.poll(),
        Some(HostNetEvent::Reliable { .. })
    ));
    assert!(!matches!(
        second_listener.poll(),
        Some(HostNetEvent::Reliable { .. })
    ));
    assert!(matches!(
        queued.finish(started.elapsed().as_millis() as u64),
        Err(Error::Closed)
    ));
}

#[test]
fn receiver_expiry_blocks_native_arm_and_the_private_armed_reply() {
    let lobby = Lobby::new();
    let (expiring, until) = shortly_expiring(&lobby.original);
    let (mut source, mut receiver) = lobby.activation(&expiring);
    let (mut second_source, mut second_receiver) = lobby.activation(&expiring);
    let started = Instant::now();
    let mut first_ready = false;
    let mut second_ready = false;
    while !(first_ready && second_ready) {
        let now = started.elapsed().as_millis() as u64;
        assert!(!source.poll(now).unwrap());
        assert!(!second_source.poll(now).unwrap());
        first_ready |= receiver.poll(now).unwrap();
        second_ready |= second_receiver.poll(now).unwrap();
        bounded(started);
    }
    let mut armed = second_receiver
        .arm(
            lobby.identities[0].keys(),
            started.elapsed().as_millis() as u64,
        )
        .unwrap();
    while unix_now() < until {
        bounded(started);
    }
    let now = started.elapsed().as_millis() as u64;
    assert!(matches!(
        receiver.arm(lobby.identities[0].keys(), now),
        Err(Error::Unauthorized)
    ));
    // The certificate was created privately while admitted. Expiration still
    // prevents the first framed reply and the sequencer/listener handoff.
    assert_eq!(armed.poll(now), Err(Error::Unauthorized));
    assert!(matches!(armed.finish(now), Err(Error::Closed)));
    assert!(source.finish(now).is_err());
    assert!(second_source.finish(now).is_err());
}

#[test]
fn quiet_receiver_expires_at_the_original_deadline_and_cannot_be_rearmed() {
    let lobby = Lobby::new();
    let (_source, mut receiver) = lobby.activation(&lobby.original);
    assert_eq!(receiver.poll(5000), Err(Error::Closed));
    assert!(matches!(
        receiver.arm(lobby.identities[0].keys(), 5000),
        Err(Error::Closed)
    ));
}

#[test]
fn buffered_exact_retries_survive_receiver_arm_and_listener_handoff() {
    let lobby = Lobby::new();
    let offer = lobby.offer();
    let consent = lobby.consent(&offer);
    let consented = offer
        .accept(
            &lobby.host,
            &lobby.network.0.lock().unwrap(),
            lobby.peers[0],
            consent,
        )
        .unwrap();
    let descriptor = consented.descriptor().clone();
    let root = &descriptor.root;
    let (_, game) = migration::checkpoint_batch(&lobby.host).unwrap();
    let routes: Vec<_> = root
        .members
        .iter()
        .map(|member| {
            let index = lobby
                .identities
                .iter()
                .position(|id| member.key == Some(id.pubkey().to_bytes()))
                .unwrap();
            (member.player, lobby.peers[index])
        })
        .collect();
    let image = SessionImage::initial(root, &routes, game, 0, consented.logical_ms()).unwrap();
    migration::validate_initial(root, &image).unwrap();
    let initial = state_hash(&image.encode(root).unwrap()).unwrap();
    assert_eq!(initial, consented.initial_hash().unwrap());
    let (listener, mut link, peer) = lobby.connect_pair(&descriptor, &lobby.identities[0]);
    let policy = EndpointPolicy {
        descriptor: descriptor.clone(),
        role: LinkRole::Standby,
        promotion: None,
        membership: None,
    };
    // Deliberately own the sending SDK channel here to inject exact retries.
    // Production uses the admitted source wrapper instead.
    let mut sending = link.migration_channel(&policy).unwrap();
    let mut receiver = migration::LobbyStandby::new(descriptor.clone(), listener, peer, 0).unwrap();
    let acks: Vec<_> = lobby
        .identities
        .iter()
        .map(|identity| {
            let member = root
                .members
                .iter()
                .find(|m| m.key == Some(identity.pubkey().to_bytes()))
                .unwrap();
            CapabilityAck::sign(root, member.player, initial, 1, identity.keys())
                .unwrap()
                .encode(root)
                .unwrap()
        })
        .collect();
    sending
        .enqueue(PacketKind::InitialImage, &image.encode(root).unwrap())
        .unwrap();
    for ack in &acks {
        sending.enqueue(PacketKind::Control, ack).unwrap();
    }
    while let Some(bytes) = sending.poll_send(0).unwrap() {
        link.send_reliable(bytes);
    }
    assert_eq!(sending.queued_bytes(), 0);
    // This suffix exceeds the receiver's first64KiB ingress allowance. Sender
    // and receiver have distinct local clocks; these are protocol-capacity
    // samples, not a real pacing or recovery-performance measurement.
    for now in 100..500 {
        sending.enqueue(PacketKind::Control, &acks[0]).unwrap();
        while let Some(bytes) = sending.poll_send(now).unwrap() {
            link.send_reliable(bytes);
        }
        // Pump ordinary ingress as well: overflowing Fray's unchanged256-event
        // transport queue is a different (fail-closed) scenario.
        receiver.poll(0).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(sending.queued_bytes(), 0);
    let started = Instant::now();
    loop {
        if receiver.poll(0).unwrap() && receiver.buffered_bytes() > 0 {
            break;
        }
        bounded(started);
    }
    let retained = receiver.buffered_bytes();
    let mut receiver = receiver.arm(lobby.identities[0].keys(), 0).unwrap();
    assert!(receiver.poll(100).unwrap());
    let mut bootstrap = receiver.finish(100).unwrap();
    assert!(bootstrap.ingress.queued_bytes() >= retained);
    let receipt = bootstrap.sequencer.receipt().clone();
    let mut actor = StandbyActor::new(
        descriptor.clone(),
        bootstrap.sequencer,
        bootstrap.channel,
        100,
    )
    .unwrap();
    let mut decoded = 0;
    let mut transfer = None;
    while let Some(record) = bootstrap
        .ingress
        .poll(|wire| {
            let frame = Frame::decode(wire)?;
            assert_eq!(frame.kind, PacketKind::Control);
            assert_eq!(frame.payload, acks[0]);
            if let Some(previous) = transfer {
                assert_eq!(frame.transfer, previous + 1);
            }
            transfer = Some(frame.transfer);
            decoded += 1;
            actor.receive(
                wire,
                100,
                lobby.identities[0].keys(),
                Activity::Idle,
                |_, _, _| panic!("archived ACK cannot restore/tick game"),
            )
        })
        .unwrap()
    {
        assert!(record.is_none());
    }
    assert!(decoded > 0);
    assert_eq!(bootstrap.ingress.queued_bytes(), 0);
    assert_eq!(actor.image(), image.encode(root).unwrap());
    assert_eq!(actor.receipt(), &receipt);
    assert!(actor.expired(1000).unwrap());
}

#[test]
fn consented_lobby_transfers_and_restores_native_state_over_admitted_quic() {
    let lobby = Lobby::new();
    let offer = lobby.offer();
    let initial_hash = offer.initial_hash().unwrap();
    let offer_wire = offer.descriptor().encode().unwrap();
    // Deliver metadata on the actual original encrypted link. No native state
    // has been exposed, and an unconsented descriptor cannot open a standby.
    lobby
        .network
        .0
        .lock()
        .unwrap()
        .send_reliable(lobby.peers[0], offer_wire.clone());
    assert!(decented_net::host_standby(
        &lobby.cfg,
        &lobby.identities[0],
        lobby.network.0.lock().unwrap().code().unwrap(),
        offer.descriptor().clone(),
    )
    .is_err());
    let started = Instant::now();
    let consent = loop {
        let event = lobby.links[0].0.lock().unwrap().poll();
        if let Some(ClientNetEvent::Reliable(bytes)) = event {
            if bytes == offer_wire {
                break migration::consent_lobby_offer(
                    &lobby.links[0].0.lock().unwrap(),
                    &lobby.identities[0],
                    lobby.network.0.lock().unwrap().code().unwrap(),
                    &bytes,
                )
                .unwrap();
            }
        }
        bounded(started);
    };
    lobby.links[0]
        .0
        .lock()
        .unwrap()
        .send_reliable(consent.encode().unwrap());
    let consent = loop {
        if let Some(HostNetEvent::Reliable { peer, bytes }) = lobby.network.0.lock().unwrap().poll()
        {
            if peer == lobby.peers[0] && bytes == consent.encode().unwrap() {
                break Descriptor::decode(&bytes).unwrap();
            }
        }
        bounded(started);
    };
    let consented = offer
        .accept(
            &lobby.host,
            &lobby.network.0.lock().unwrap(),
            lobby.peers[0],
            consent,
        )
        .unwrap();
    assert_eq!(consented.initial_hash().unwrap(), initial_hash);
    let descriptor = consented.descriptor().clone();
    let root = descriptor.root.clone();
    let (listener, link, peer) = lobby.connect_pair(&descriptor, &lobby.identities[0]);
    let mut sink = migration::LobbyStandby::new(descriptor, listener, peer, 0).unwrap();
    let mut source = consented.into_activation(link, 0).unwrap();
    assert_eq!(source.initial_hash().unwrap(), initial_hash);
    for identity in &lobby.identities {
        let member = root
            .members
            .iter()
            .find(|m| m.key == Some(identity.pubkey().to_bytes()))
            .unwrap();
        source
            .acknowledge(
                CapabilityAck::sign(&root, member.player, initial_hash, 1, identity.keys())
                    .unwrap(),
                0,
            )
            .unwrap();
    }
    let transfer_started = Instant::now();
    loop {
        let now = transfer_started.elapsed().as_millis() as u64;
        assert!(!source.poll(now).unwrap());
        if sink.poll(now).unwrap() {
            break;
        }
        bounded(transfer_started);
    }
    let mut sink = sink
        .arm(
            lobby.identities[0].keys(),
            transfer_started.elapsed().as_millis() as u64,
        )
        .unwrap();
    loop {
        let now = transfer_started.elapsed().as_millis() as u64;
        assert!(sink.poll(now).unwrap());
        if source.poll(now).unwrap() {
            break;
        }
        bounded(transfer_started);
    }
    let bootstrap = sink
        .finish(transfer_started.elapsed().as_millis() as u64)
        .unwrap();
    assert!(bootstrap.pending.is_empty());
    let (sequencer, armed) = (bootstrap.sequencer, bootstrap.armed);
    let primary = source
        .finish(transfer_started.elapsed().as_millis() as u64)
        .unwrap()
        .primary;
    assert_eq!(primary.committed_image(), sequencer.image());
    assert_eq!(primary.receipt(), &armed.receipt);
    let installed = SessionImage::decode(&root, sequencer.image()).unwrap();
    assert_eq!(installed.members.len(), 2);
    assert_eq!(
        state_hash(&installed.encode(&root).unwrap()).unwrap(),
        initial_hash
    );
    assert!(installed
        .members
        .iter()
        .all(|m| m.key != Some(lobby.original.pubkey().to_bytes())));
    println!("LOBBY ACTIVATION completed=true humans=2 bots=6 host_player_seats=0 exact_native_restore=true actual_quic=true");
}

// Owned original runtime. The pair link, Fray actors and native authority are
// real; each player's Fray channel is a synthetic bound channel (no player
// QUIC transport yet), and the standby side is a test-driven StandbyActor.

/// Logical milliseconds per batch: two world ticks, within the 50 ms cap.
const STEP: u64 = 33;
/// Inputs the test clients keep ahead of the host (Stampede's jitter buffer).
const LEAD: u32 = 2;

fn complete_bootstrap<const N: usize>(
    lobby: &Lobby<N>,
) -> (
    Descriptor,
    migration::PrimaryBootstrap,
    migration::StandbyBootstrap,
    migration::RunnerClock,
) {
    complete_bootstrap_as(lobby, &lobby.original)
}
/// The same bootstrap, with `original` (the original's runtime key, possibly
/// with a different signed approval) presented on the pair link.
///
/// Also returns the one runner clock the test's runners share; every time the
/// bootstrap hands Fray is a sample of it, so the runners' own samples later
/// never go behind the bootstrap.
fn complete_bootstrap_as<const N: usize>(
    lobby: &Lobby<N>,
    original: &Identity,
) -> (
    Descriptor,
    migration::PrimaryBootstrap,
    migration::StandbyBootstrap,
    migration::RunnerClock,
) {
    let (descriptor, mut source, mut sink) = lobby.activation_for(original);
    let clock = migration::RunnerClock::start();
    let started = Instant::now();
    let sink = loop {
        let now = clock.now_ms();
        assert!(!source.poll(now).unwrap());
        if sink.poll(now).unwrap() {
            break sink;
        }
        bounded(started);
    };
    let mut sink = sink
        .arm(lobby.identities[0].keys(), clock.now_ms())
        .unwrap();
    loop {
        let now = clock.now_ms();
        assert!(sink.poll(now).unwrap());
        if source.poll(now).unwrap() {
            break;
        }
        bounded(started);
    }
    let now = clock.now_ms();
    let receiver = sink.finish(now).unwrap();
    let source = source.finish(now).unwrap();
    (descriptor, source, receiver, clock)
}

/// The standby end of the same activation link, driven directly.
struct Replica {
    actor: StandbyActor,
    network: QuicHost,
    peer: PeerKey,
    ingress: ReceiveQueue,
    keys: decented_net::Keys,
    root: Root,
}
impl Replica {
    fn new(
        descriptor: &Descriptor,
        bootstrap: migration::StandbyBootstrap,
        keys: decented_net::Keys,
        now: u64,
    ) -> Self {
        assert!(bootstrap.pending.is_empty());
        Self {
            actor: StandbyActor::new(
                descriptor.clone(),
                bootstrap.sequencer,
                bootstrap.channel,
                now,
            )
            .unwrap(),
            network: bootstrap.network,
            peer: bootstrap.original_peer,
            ingress: bootstrap.ingress,
            keys,
            root: descriptor.root.clone(),
        }
    }
    fn intake(&mut self) {
        for _ in 0..MAX_BATCH {
            let Some(event) = self.network.poll() else {
                break;
            };
            match event {
                HostNetEvent::Reliable { peer, bytes } if peer == self.peer => {
                    self.ingress.enqueue(bytes).unwrap()
                }
                other => panic!("unexpected pair event {other:?}"),
            }
        }
    }
    /// Install queued frames; returns the newest receipt index produced.
    fn receive(&mut self, now: u64) -> Option<u64> {
        let mut newest = None;
        let (root, keys) = (&self.root, &self.keys);
        while let Some(receipt) = self
            .ingress
            .poll(|wire| {
                self.actor.receive(
                    wire,
                    now,
                    keys,
                    Activity::Advancing,
                    // Fray passes (candidate, new image, previous image).
                    |candidate, _, previous| {
                        migration::validate_candidate(root, previous, candidate)
                    },
                )
            })
            .unwrap()
        {
            if let Some(receipt) = receipt {
                newest = Some(receipt.index);
            }
        }
        newest
    }
    fn flush(&mut self, now: u64) {
        while let Some(wire) = self.actor.poll_send(now).unwrap() {
            self.network.send_reliable(self.peer, wire);
        }
    }
}

/// One human's verified Fray input queue on the original.
struct Seat {
    player: u64,
    peer: PeerKey,
    keys: decented_net::Keys,
    cert: MembershipCertificate,
    tls: [u8; 32],
    queue: PlayerQueue,
    control: u64,
    lossy: u64,
}
impl Seat {
    fn sign(&mut self, root: &Root, action: PlayerAction) -> Vec<u8> {
        self.control += 1;
        PlayerControl::sign(
            root,
            &self.cert,
            0,
            self.control,
            self.player,
            self.tls,
            action,
            &self.keys,
        )
        .unwrap()
        .encode(root)
        .unwrap()
    }
    fn view(&mut self, root: &Root, bytes: Vec<u8>) -> Vec<u8> {
        self.lossy += 1;
        let number = self.lossy;
        self.sign(root, PlayerAction::Lossy { number, bytes })
    }
}
/// `cert` is the players' own (client-side) certificate; the original's
/// server channels and queues use the one it learned over the pair link.
fn seats(
    lobby: &Lobby,
    descriptor: &Descriptor,
    live: &mut migration::LivePrimary,
    cert: &MembershipCertificate,
) -> Vec<Seat> {
    let root = &descriptor.root;
    let original = live.membership_certificate().unwrap().clone();
    assert_eq!(original.receipt, *live.primary().receipt());
    let mut seats = Vec::new();
    // The standby device's own seat (identities[0]) shares the standby's
    // runtime key and so its replay sequence; it stays frozen here.
    for (i, identity) in lobby.identities.iter().enumerate().skip(1) {
        let key = identity.pubkey().to_bytes();
        let member = root.members.iter().find(|m| m.key == Some(key)).unwrap();
        let tls = [0x40 + i as u8; 32];
        let policy = EndpointPolicy {
            descriptor: descriptor.clone(),
            role: LinkRole::Original,
            promotion: None,
            membership: Some(original.clone()),
        };
        let binding = ConnectionBinding {
            session: root.session,
            descriptor: root.digest().unwrap(),
            term: 0,
            client: LinkRole::Player,
            server: LinkRole::Original,
        };
        let channel = Channel::new(&policy, binding, ChannelSide::Server, key, tls).unwrap();
        seats.push(Seat {
            player: member.player,
            peer: lobby.peers[i],
            keys: identity.keys().clone(),
            cert: cert.clone(),
            tls,
            queue: live.attach_player(&channel, original.clone()).unwrap(),
            // Sequence 1 was this player's signed activation capability ACK.
            control: 1,
            lossy: 0,
        });
    }
    seats
}

/// What the plain host would put on its own transport.
#[derive(Default)]
struct Wire {
    events: VecDeque<HostNetEvent>,
    sent: Vec<(PeerKey, bool, Vec<u8>)>,
    controls: Vec<String>,
}
#[derive(Clone, Default)]
struct Script(Arc<Mutex<Wire>>);
impl Script {
    fn deliver(&self, event: HostNetEvent) {
        self.0.lock().unwrap().events.push_back(event);
    }
    fn take(&self) -> (Vec<(PeerKey, bool, Vec<u8>)>, Vec<String>) {
        let mut wire = self.0.lock().unwrap();
        assert!(wire.events.is_empty());
        (
            std::mem::take(&mut wire.sent),
            std::mem::take(&mut wire.controls),
        )
    }
    fn control(&self, effect: migration::Effect) {
        self.0.lock().unwrap().controls.push(format!("{effect:?}"));
    }
}
impl HostTransport for Script {
    fn poll(&mut self) -> Option<HostNetEvent> {
        self.0.lock().unwrap().events.pop_front()
    }
    fn send_reliable(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0.lock().unwrap().sent.push((peer, false, bytes));
    }
    fn send_lossy(&mut self, peer: PeerKey, bytes: Vec<u8>) {
        self.0.lock().unwrap().sent.push((peer, true, bytes));
    }
    fn kick(&mut self, peer: PeerKey) {
        self.control(migration::Effect::Kick(peer));
    }
    fn disconnect(&mut self, peer: PeerKey) {
        self.control(migration::Effect::Disconnect(peer));
    }
    fn confirm_timeout(&mut self, peer: PeerKey) {
        self.control(migration::Effect::ConfirmTimeout(peer));
    }
    fn set_open(&mut self, open: bool) {
        self.control(migration::Effect::Open(open));
    }
    fn set_open_keeping_token(&mut self, open: bool) {
        self.set_open(open);
    }
    fn rtt_ms(&self, _: PeerKey) -> Option<u32> {
        None
    }
    fn shutdown(&mut self) {
        self.control(migration::Effect::Shutdown);
    }
}
/// The existing, non-migration host path from the same checkpoint: its own
/// `HostAuthority::update`, input queue, starvation and late-press handling.
fn plain_host(host: &HostAuthority, script: &Script) -> HostAuthority {
    let (binding, bytes) = migration::checkpoint(host).unwrap();
    migration::restore(&bytes, &binding, Box::new(script.clone())).unwrap()
}

/// A Stampede `Inputs` datagram exactly as the client builds it: the newest
/// `INPUTS_PER_PACKET` inputs, ending at `seq`.
fn datagram(seq: u32, input: impl Fn(u32) -> [u8; 4]) -> Vec<u8> {
    let first = seq.saturating_sub(4).max(1);
    race_session::proto::encode(&race_session::proto::LossyToHost::Inputs {
        race: 1,
        seq,
        inputs: (first..=seq).map(input).collect(),
    })
}
/// Steer straight (a Le Mans jockey runs to its ostrich); throttle numbers
/// each input so overlapping copies are distinguishable.
fn pad(n: u32, jump: bool) -> [u8; 4] {
    Input {
        steer: 0.0,
        throttle: (n % 8) as f32 / 8.0,
        jump,
        ..Input::default()
    }
    .pack()
}

/// Hold the standby's receipt and prove that nothing is published and the
/// authority is still `before` (taken before staging), then release it.
/// Returns what the runtime published and whether the standby's receipt for
/// `index` existed when the Committed event fired.
fn publish(
    live: &mut migration::LivePrimary,
    replica: &mut Replica,
    index: u64,
    before: &(migration::Binding, Vec<u8>),
    wall: &dyn Fn() -> u64,
) -> (
    Vec<CertifiedOutput>,
    Vec<migration::Effect>,
    Vec<(u64, u64)>,
    bool,
) {
    let started = Instant::now();
    loop {
        // Only the previous receipt's trailing certificate may arrive.
        assert!(
            live.poll().unwrap().iter().all(|event| matches!(
                event,
                migration::RuntimeEvent::Membership(cert) if cert.receipt.index < index
            )),
            "published before its receipt"
        );
        assert_eq!(
            &migration::checkpoint(live.authority()).unwrap(),
            before,
            "native authority adopted before its receipt"
        );
        replica.intake();
        if let Some(receipt) = replica.receive(wall()) {
            assert_eq!(receipt, index);
            break;
        }
        bounded(started);
    }
    assert_eq!(live.primary().receipt().index, index - 1);
    replica.flush(wall());
    loop {
        for event in live.poll().unwrap() {
            match event {
                migration::RuntimeEvent::Committed {
                    outputs,
                    controls,
                    selected_views,
                } => {
                    // The standby had already installed and receipted it.
                    let receipt_first = replica.actor.receipt().index == index;
                    assert!(receipt_first, "published without the standby receipt");
                    assert_eq!(live.primary().receipt().index, index);
                    for output in &outputs {
                        output.verify(&replica.root.context().unwrap()).unwrap();
                        assert_eq!(output.receipt.index, index);
                    }
                    // Its certificate trails on a later frame.
                    original_cert(live, || {
                        replica.intake();
                        replica.receive(wall());
                        replica.flush(wall());
                    });
                    return (outputs, controls, selected_views, receipt_first);
                }
                migration::RuntimeEvent::Promoted(_) => panic!("unexpected promotion"),
                migration::RuntimeEvent::Membership(_) => {}
            }
        }
        bounded(started);
    }
}

// The standby probes the original from its first poll, which can follow its
// `Armed` so closely that both reach the original in one drain, before the
// original has finished its activation. The activation reads up to `Armed`
// and leaves the probe queued for the armed primary, which answers it; it
// must not refuse the probe as a foreign control.
#[test]
fn activation_leaves_a_probe_behind_armed_for_the_primary() {
    let lobby = Lobby::new();
    let (descriptor, mut source, mut sink) = lobby.activation_for(&lobby.original);
    let clock = migration::RunnerClock::start();
    let wall = || clock.now_ms();
    let started = Instant::now();
    let sink = loop {
        assert!(!source.poll(wall()).unwrap());
        if sink.poll(wall()).unwrap() {
            break sink;
        }
        bounded(started);
    };
    let mut sink = sink.arm(lobby.identities[0].keys(), wall()).unwrap();
    // `Armed` (and its certificate) leave; the original does not poll yet.
    assert!(sink.poll(wall()).unwrap());
    let receiver = sink.finish(wall()).unwrap();
    let mut replica = Replica::new(
        &descriptor,
        receiver,
        lobby.identities[0].keys().clone(),
        wall(),
    );
    replica.actor.probe(wall()).unwrap();
    replica.flush(wall());
    std::thread::sleep(Duration::from_millis(200));
    loop {
        if source.poll(wall()).unwrap() {
            break;
        }
        bounded(started);
    }
    let bootstrap = source.finish(wall()).unwrap();
    let mut live = migration::LivePrimary::new(
        descriptor.clone(),
        bootstrap,
        lobby.host,
        lobby.original.clone(),
        clock.clone(),
    )
    .unwrap();
    // The armed primary takes the probe that arrived during activation.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        live.poll().unwrap();
        replica.intake();
        replica.receive(wall());
        replica.flush(wall());
    }
    assert!(live.failure().is_none(), "{:?}", live.failure());
    assert!(replica.actor.failure().is_none(), "{:?}", replica.actor.failure());
}

// A trailing certificate never shares a poll with the next receipt: the
// standby queues receipt N and its certificate together, but they can reach
// the original in two drains. If the original has already published N and
// staged N+1, certificate N and receipt N+1 can then land in one drain. The
// runtime must hand the certificate back before it installs N+1, so a caller
// that certifies on `Membership` (after `poll` returns) still finds it paired
// with the installed receipt; otherwise `certify` reads it as `Stale`.
#[test]
fn owned_primary_reports_a_trailing_certificate_before_the_next_receipt() {
    let lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let root = descriptor.root.clone();
    let wall = || clock.now_ms();
    let mut replica = Replica::new(
        &descriptor,
        receiver,
        lobby.identities[0].keys().clone(),
        wall(),
    );
    let mut live = migration::LivePrimary::new(
        descriptor.clone(),
        source,
        lobby.host,
        lobby.original.clone(),
        clock,
    )
    .unwrap();
    original_cert(&mut live, || {
        replica.intake();
        replica.receive(wall());
        replica.flush(wall());
    });
    let base = SessionImage::decode(&root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let started = Instant::now();
    // The standby installs `index` and queues its receipt and certificate.
    let install = |live: &mut migration::LivePrimary, replica: &mut Replica, index: u64| {
        assert!(live
            .stage(base + index * STEP, &[], &BTreeMap::new(), |_| Ok(()))
            .unwrap());
        loop {
            replica.intake();
            if replica.receive(wall()) == Some(index) {
                break;
            }
            bounded(started);
        }
    };
    // Send exactly one queued pair-link frame of the standby.
    let send_one = |replica: &mut Replica| {
        let wire = replica.actor.poll_send(wall()).unwrap().unwrap();
        replica.network.send_reliable(replica.peer, wire);
    };
    // Batch 1: only its receipt reaches the original, which publishes it.
    install(&mut live, &mut replica, 1);
    send_one(&mut replica);
    loop {
        let events = live.poll().unwrap();
        assert!(events
            .iter()
            .all(|e| !matches!(e, migration::RuntimeEvent::Membership(_))));
        if events
            .iter()
            .any(|e| matches!(e, migration::RuntimeEvent::Committed { .. }))
        {
            break;
        }
        bounded(started);
    }
    assert_eq!(live.primary().receipt().index, 1);
    // Batch 2 is staged and installed; certificate 1 and receipt 2 leave
    // together and both arrive before the original's next poll.
    install(&mut live, &mut replica, 2);
    send_one(&mut replica);
    send_one(&mut replica);
    std::thread::sleep(Duration::from_millis(200));
    let mut certified = false;
    while live.primary().receipt().index < 2 || !certified {
        for event in live.poll().unwrap() {
            if let migration::RuntimeEvent::Membership(cert) = event {
                assert_eq!(
                    cert.receipt.index,
                    live.primary().receipt().index,
                    "a certificate was reported after the next receipt was installed"
                );
                certified |= cert.receipt.index == 1;
            }
        }
        bounded(started);
    }
}

#[test]
fn owned_primary_commits_race_ticks_like_the_plain_host_and_refuses_the_closed_term() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let root = descriptor.root.clone();
    let wall = || clock.now_ms();
    let original_tls = source.channel.tls();
    let script = Script::default();
    let mut plain = plain_host(&lobby.host, &script);
    let mut replica = Replica::new(
        &descriptor,
        receiver,
        lobby.identities[0].keys().clone(),
        wall(),
    );
    let host = std::mem::replace(
        &mut lobby.host,
        HostAuthority::new_headless(Box::new(Script::default()), HostConfig::default()),
    );
    let mut live = migration::LivePrimary::new(
        descriptor.clone(),
        source,
        host,
        lobby.original.clone(),
        clock,
    )
    .unwrap();
    original_cert(&mut live, || {
        replica.intake();
        replica.receive(wall());
        replica.flush(wall());
    });
    // The players' own (client-side) certificate.
    let cert = replica.actor.membership_certificate(&replica.keys).unwrap();
    let mut seats = seats(&lobby, &descriptor, &mut live, &cert);
    let other = 0;
    let other_native = (seats[other].player - 1) as u8;
    let other_racer = |host: &HostAuthority| {
        let world = host.world().unwrap();
        world.racers[host.racer_of(other_native).unwrap()].clone()
    };
    let base = SessionImage::decode(&root, live.primary().committed_image())
        .unwrap()
        .logical_ms;

    let mut index = 0u64;
    let mut racing_from = None;
    let mut other_sent = 0u32;
    let mut stall_last = None;
    let mut jumps = BTreeSet::new();
    let mut counterfactual: Option<(Script, HostAuthority)> = None;
    let mut starved_before_stall = None;
    let mut racing_at = None;
    let mut other_starved_on_time = None;
    let mut starved_in_stall = 0;
    let mut late_numbers_applied = false;
    let mut late_press_mounted = false;
    let mut stall_at = None;
    let mut fray_duplicates_dropped = false;
    let mut every_view_queued = false;
    let mut burst = Vec::new();
    let mut burst_delivered = false;
    let mut transient_attempts = 0;
    let mut native_equal = true;
    let mut plain_equal = true;
    let mut outputs_equal = true;
    let mut outputs_total = 0;
    let mut no_host_seat = true;
    let mut gated_commits = 0;
    loop {
        index += 1;
        let now = base + index * STEP;
        let world = plain.world();
        let tick = world.map_or(0, |w| w.tick) as u32;
        if racing_from.is_none() && world.is_some_and(|w| w.phase == RacePhase::Racing) {
            racing_from = Some(index);
            racing_at = Some((plain.stats().starved_ticks, tick));
        }
        let racing = racing_from.map(|from| index - from);
        // Other's datagrams are lost once its Le Mans jockey has reached its
        // ostrich, where a jump press would mount it.
        if let Some(r) = racing {
            let racer = other_racer(&plain);
            if stall_at.is_none()
                && r >= 3
                && racer.effects.le_mans
                && racer.chase_offset().length() < 1.5
            {
                stall_at = Some(r);
            }
            assert!(stall_at.is_some() || r < 400, "jockey never reached");
        }
        let since_stall = racing.zip(stall_at).map(|(r, s)| r - s);
        if since_stall == Some(10) {
            break;
        }
        // From the late datagram on, a counterfactual plain host receives the
        // same datagrams with the late jump presses removed.
        if since_stall == Some(3) {
            let cf = Script::default();
            counterfactual = Some((cf.clone(), plain_host(&plain, &cf)));
        }
        // Client datagrams for this batch. Both paths receive them all; the
        // migration path receives them through verified Fray lossy views.
        let send = |seat: &mut Seat,
                    live: &migration::LivePrimary,
                    bytes: Vec<u8>,
                    stripped: Option<Vec<u8>>| {
            let wire = seat.view(&root, bytes.clone());
            assert!(seat.queue.receive(&wire, live.primary()).unwrap());
            script.deliver(HostNetEvent::Lossy {
                peer: seat.peer,
                bytes: bytes.clone(),
            });
            if let Some((cf, _)) = &counterfactual {
                cf.deliver(HostNetEvent::Lossy {
                    peer: seat.peer,
                    bytes: stripped.unwrap_or(bytes),
                });
            }
            wire
        };
        let seq = tick + 2 + LEAD;
        if world.is_some() {
            let other_input = |n: u32| pad(n, jumps.contains(&n));
            match (racing, since_stall) {
                // Two datagrams in one batch: Fray keeps both views, in order.
                (Some(1), _) => {
                    send(
                        &mut seats[other],
                        &live,
                        datagram(seq - 1, other_input),
                        None,
                    );
                    send(&mut seats[other], &live, datagram(seq, other_input), None);
                    let newest = seats[other].lossy;
                    every_view_queued = seats[other]
                        .queue
                        .lossy_views()
                        .map(|(n, _)| n)
                        .eq([newest - 1, newest]);
                    other_sent = seq;
                }
                // A replayed control and a re-signed stale lossy number carry
                // a different view. Neither reaches the race; the plain host
                // never sees such transport duplicates.
                (Some(2), _) => {
                    let wire = send(&mut seats[other], &live, datagram(seq, other_input), None);
                    let seat = &mut seats[other];
                    let replayed = seat.queue.receive(&wire, live.primary()).unwrap();
                    let tampered = datagram(seq, |n| pad(n, true));
                    let stale = seat.sign(
                        &root,
                        PlayerAction::Lossy {
                            number: seat.lossy,
                            bytes: tampered.clone(),
                        },
                    );
                    let same_number = seat.queue.receive(&stale, live.primary()).unwrap();
                    let older = seat.sign(
                        &root,
                        PlayerAction::Lossy {
                            number: seat.lossy - 1,
                            bytes: tampered,
                        },
                    );
                    let older_number = seat.queue.receive(&older, live.primary()).unwrap();
                    fray_duplicates_dropped = !replayed && !same_number && !older_number;
                    assert!(fray_duplicates_dropped);
                    other_sent = seq;
                }
                // Other's datagrams are lost for three batches. It held jump
                // for the first three inputs the host will starve on.
                (_, Some(0)) => {
                    let (starved, at) = racing_at.unwrap();
                    other_starved_on_time =
                        Some(plain.stats().starved_ticks - starved - u64::from(tick - at));
                    stall_last = Some(other_sent);
                    jumps.extend(other_sent + 1..=other_sent + 3);
                    starved_before_stall = Some((plain.stats().starved_ticks, tick));
                }
                (_, Some(1 | 2)) => {}
                // The delayed datagram arrives after those ticks ran, in one
                // batch with five newer ones. The newest datagram's five-input
                // window no longer holds any starved jump number.
                (_, Some(3)) => {
                    let last = stall_last.unwrap();
                    late_numbers_applied = jumps.iter().all(|&n| n <= tick);
                    assert!(late_numbers_applied, "stall inputs were not starved");
                    // The frozen standby seat starves on every racing tick;
                    // the rest are Other's starved inputs.
                    let (starved, at) = starved_before_stall.unwrap();
                    starved_in_stall = plain.stats().starved_ticks - starved - u64::from(tick - at);
                    for seq in last + 5..=last + 10 {
                        send(
                            &mut seats[other],
                            &live,
                            datagram(seq, other_input),
                            Some(datagram(seq, |n| pad(n, false))),
                        );
                        burst.push(seats[other].lossy);
                    }
                    assert!(jumps.iter().all(|&n| n < last + 10 - 4));
                    other_sent = last + 10;
                }
                _ => {
                    send(&mut seats[other], &live, datagram(seq, other_input), None);
                    other_sent = seq;
                }
            }
        }
        // Stage on the owned runtime. A second call while the receipt is
        // pending runs no native work.
        let mut calls = 0;
        let fault = since_stall == Some(6);
        let queues = [&seats[other].queue];
        let before = migration::checkpoint(live.authority()).unwrap();
        assert!(live
            .stage(now, &queues, &BTreeMap::new(), |h| {
                calls += 1;
                if fault && calls == 1 {
                    return Err(Error::Bounds);
                }
                if index == 1 && !h.start_race(now) {
                    return Err(Error::Invalid("race start"));
                }
                Ok(())
            })
            .unwrap());
        if fault {
            transient_attempts = calls;
        }
        assert_eq!(
            migration::checkpoint(live.authority()).unwrap(),
            before,
            "native authority adopted before its receipt"
        );
        assert!(!live
            .stage(now, &queues, &BTreeMap::new(), |_| panic!(
                "pending stage must not run native work"
            ))
            .unwrap());
        // The plain host processes the same datagrams at the same time.
        for host in std::iter::once(&mut plain).chain(counterfactual.as_mut().map(|(_, h)| h)) {
            if index == 1 {
                assert!(host.start_race(now));
            }
            host.update(now);
        }
        if let Some((cf, cf_host)) = &counterfactual {
            cf.take();
            // Right after the late datagram's batch: the late press mounted
            // the jockey only on the real stream.
            if since_stall == Some(3) {
                late_press_mounted =
                    !other_racer(&plain).effects.le_mans && other_racer(cf_host).effects.le_mans;
            }
        }
        let (outputs, controls, selected, receipt_first) =
            publish(&mut live, &mut replica, index, &before, &wall);
        gated_commits += u64::from(receipt_first);
        let (sent, plain_controls) = script.take();
        // Native state, standby image and the plain host all agree.
        let committed = migration::committed_authority(&root, replica.actor.image()).unwrap();
        let state = migration::checkpoint(live.authority()).unwrap();
        native_equal &= live.primary().committed_image() == replica.actor.image()
            && state == migration::checkpoint(&committed).unwrap();
        plain_equal &= state == migration::checkpoint(&plain).unwrap();
        assert!(
            native_equal,
            "runtime differs from the committed image at {index}"
        );
        assert!(
            plain_equal,
            "runtime differs from the plain host at {index}"
        );
        let image = SessionImage::decode(&root, live.primary().committed_image()).unwrap();
        let peer_of = |player: u64| {
            image
                .members
                .iter()
                .find(|m| m.player == player)
                .unwrap()
                .logical_peer
        };
        let published: Vec<_> = outputs
            .iter()
            .map(|o| {
                (
                    peer_of(o.output.recipient),
                    o.output.delivery == Delivery::Lossy,
                    o.output.bytes.clone(),
                )
            })
            .collect();
        outputs_total += published.len();
        outputs_equal &= published == sent
            && controls
                .iter()
                .map(|c| format!("{c:?}"))
                .collect::<Vec<_>>()
                == plain_controls;
        assert!(
            outputs_equal,
            "outputs differ from the plain host at {index}"
        );
        // Every view of the burst reached the race on its own, in order.
        if since_stall == Some(3) {
            burst_delivered = selected
                .iter()
                .filter(|(player, _)| *player == seats[other].player)
                .map(|(_, number)| *number)
                .eq(burst.iter().copied());
            assert!(burst_delivered, "burst views {selected:?} != {burst:?}");
        }
        no_host_seat &= image.members.len() == 2
            && image.members.iter().all(|m| m.key != Some(root.host))
            && live.authority().human_count() == 2;
        // Refresh verified queues, discard only the selected views, and ack.
        for seat in &mut seats {
            seat.queue.refresh(live.primary()).unwrap();
            for (player, number) in &selected {
                if *player == seat.player {
                    seat.queue.discard_lossy(*number);
                }
            }
            let output = live
                .primary()
                .output_ceiling(seat.player, Delivery::Reliable);
            let ack = seat.sign(
                &root,
                PlayerAction::Applied {
                    output,
                    tail: 0,
                    cut: None,
                },
            );
            if let Err(e) = seat.queue.receive(&ack, live.primary()) {
                panic!(
                    "ack at {index} player {} output {output}: {e:?}",
                    seat.player
                );
            }
        }
    }
    let commits = index - 1;
    assert!(late_press_mounted);
    assert!(starved_in_stall > 0);
    assert_eq!(other_starved_on_time, Some(0));
    assert_eq!(transient_attempts, 2);
    assert!(every_view_queued);
    assert!(burst_delivered);
    assert!(no_host_seat);
    assert_eq!(gated_commits, commits);
    let humans = live.authority().human_count();
    let bots = live.authority().world().unwrap().racers.len() - humans;
    let host_seats = root
        .members
        .iter()
        .filter(|m| m.key == Some(root.host))
        .count();
    println!("STAMPEDE OWNED PRIMARY commits={commits} humans={humans} driven_humans={} bots={bots} host_seats={host_seats} no_phantom_seat={no_host_seat} racing_batches={} receipt_gated_commits={gated_commits} native_equal_each_commit={native_equal} plain_host_equal_each_commit={plain_equal} outputs_equal_plain_host={outputs_equal} outputs={outputs_total} every_view_queued={every_view_queued} burst_views={} burst_delivered={burst_delivered} fray_duplicates_dropped={fray_duplicates_dropped} other_starved_on_time={} other_starved_in_stall={starved_in_stall} late_numbers_applied={late_numbers_applied} late_press_mounted={late_press_mounted} transient_fault_attempts={transient_attempts}", seats.len(), index - racing_from.unwrap(), burst.len(), other_starved_on_time.unwrap());

    // Closed term. The original stages C+1; its candidate frames reach the
    // standby's listener but are never installed. A synthetic watch sample
    // stands in for the original's loss; there is no process kill.
    let frontier = live.primary().receipt().clone();
    let native_before = migration::checkpoint(live.authority()).unwrap();
    let now = base + index * STEP;
    let queues = [&seats[other].queue];
    assert!(live
        .stage(now, &queues, &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    let race_started = Instant::now();
    while replica.ingress.queued_bytes() == 0 {
        assert!(live.poll().unwrap().is_empty());
        replica.intake();
        bounded(race_started);
    }
    let mut held = Vec::new();
    while let Some(wire) = replica.ingress.poll(|wire| Ok(wire.to_vec())).unwrap() {
        held.push(wire);
    }
    let Replica {
        mut actor,
        mut network,
        peer,
        keys,
        ..
    } = replica;
    let mut watch_ms = wall() + 250;
    while !actor.expired(watch_ms).unwrap() {
        watch_ms += 250;
    }
    let (recovery, mut warm) = actor.begin_recovery(&keys, watch_ms, false).unwrap();
    assert_eq!(recovery.promotion().final_receipt, frontier);
    while let Some(wire) = warm.poll_send(watch_ms).unwrap() {
        network.send_reliable(peer, wire);
    }
    loop {
        if live
            .poll()
            .unwrap()
            .iter()
            .any(|event| matches!(event, migration::RuntimeEvent::Promoted(_)))
        {
            break;
        }
        bounded(race_started);
    }
    let pending_dropped = !live.has_pending();
    let native_unchanged = migration::checkpoint(live.authority()).unwrap() == native_before;
    assert!(live.primary().is_closed());
    assert!(pending_dropped);
    assert!(native_unchanged);
    assert_eq!(live.primary().receipt(), &frontier);
    let mut calls = 0;
    assert_eq!(
        live.stage(now, &queues, &BTreeMap::new(), |_| {
            calls += 1;
            Ok(())
        }),
        Err(Error::Closed)
    );
    assert_eq!(calls, 0);
    // Term-0 frames injected towards the closed original: its own queued C+1
    // candidate frames and a genuine receipt for C re-framed in term 0.
    let is_candidate = |wire: &[u8]| {
        matches!(
            Frame::decode(wire).unwrap().kind,
            PacketKind::Candidate | PacketKind::Delta | PacketKind::InitialImage
        )
    };
    let candidate_frames = held.iter().filter(|w| is_candidate(w)).count();
    assert!(candidate_frames > 0);
    let receipt = frontier.encode(&root.context().unwrap()).unwrap();
    let receipt_frames = Frame::split(root.session, 0, 1, PacketKind::Receipt, &receipt).unwrap();
    let other_frames = held.len() - candidate_frames + receipt_frames.len();
    for wire in held {
        network.send_reliable(peer, wire);
    }
    for frame in receipt_frames {
        network.send_reliable(peer, frame.encode().unwrap());
    }
    while live.obsolete().total() < candidate_frames + other_frames && live.queued_bytes() == 0 {
        assert!(live.poll().unwrap().is_empty());
        bounded(race_started);
    }
    assert_eq!(
        live.queued_bytes(),
        0,
        "term-0 frame crossed into original recovery"
    );
    assert_eq!(live.obsolete().candidate_frames, candidate_frames);
    assert_eq!(live.obsolete().other_frames, other_frames);
    assert!(!live.has_pending());
    assert_eq!(live.primary().receipt(), &frontier);
    assert_eq!(
        migration::checkpoint(live.authority()).unwrap(),
        native_before
    );
    let obsolete = live.obsolete();
    let handoff = live.into_recovery().unwrap();
    let same_warm_channel = handoff.channel.tls() == original_tls;
    assert!(same_warm_channel);
    assert_eq!(handoff.ingress.queued_bytes(), 0);
    assert_eq!(handoff.obsolete, obsolete);
    assert_eq!(
        handoff.network.migration_keys().unwrap(),
        (root.host, root.standby)
    );
    println!("STAMPEDE CLOSED TERM final_index={} pending_dropped={pending_dropped} native_unchanged={native_unchanged} stage_refused_native_calls={calls} refused_candidate_frames={} refused_other_frames={} retained_bytes={} same_warm_channel={same_warm_channel}", frontier.index, obsolete.candidate_frames, obsolete.other_frames, handoff.ingress.queued_bytes());
}

#[test]
fn owned_primary_native_fault_restores_once_then_pauses() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let root = descriptor.root.clone();
    let wall = || clock.now_ms();
    let original_tls = source.channel.tls();
    let mut replica = Replica::new(
        &descriptor,
        receiver,
        lobby.identities[0].keys().clone(),
        wall(),
    );
    let host = std::mem::replace(
        &mut lobby.host,
        HostAuthority::new_headless(Box::new(Script::default()), HostConfig::default()),
    );
    let mut live = migration::LivePrimary::new(
        descriptor.clone(),
        source,
        host,
        lobby.original.clone(),
        clock,
    )
    .unwrap();
    original_cert(&mut live, || {
        replica.intake();
        replica.receive(wall());
        replica.flush(wall());
    });
    // The players' own (client-side) certificate.
    let cert = replica.actor.membership_certificate(&replica.keys).unwrap();
    let mut seats = seats(&lobby, &descriptor, &mut live, &cert);
    let base = SessionImage::decode(&root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let before = migration::checkpoint(live.authority()).unwrap();
    {
        let queues = [&seats[0].queue];
        assert!(live
            .stage(base + STEP, &queues, &BTreeMap::new(), |h| {
                if h.start_race(base + STEP) {
                    Ok(())
                } else {
                    Err(Error::Invalid("race start"))
                }
            })
            .unwrap());
    }
    assert!(publish(&mut live, &mut replica, 1, &before, &wall).3);
    seats[0].queue.refresh(live.primary()).unwrap();
    // A verified lossy view waits in Other's queue for the faulting batch.
    let view = datagram(1 + LEAD, |n| pad(n, false));
    let wire = seats[0].view(&root, view.clone());
    assert!(seats[0].queue.receive(&wire, live.primary()).unwrap());
    let retained = seats[0].queue.lossy().map(|(n, b)| (n, b.to_vec()));
    let before = migration::checkpoint(live.authority()).unwrap();
    let frontier = live.primary().receipt().clone();
    let mut fresh = Vec::new();
    let queues = [&seats[0].queue];
    let faulted = live.stage(base + 2 * STEP, &queues, &BTreeMap::new(), |h| {
        fresh.push(migration::checkpoint(h).unwrap());
        Err(Error::Bounds)
    });
    assert!(faulted.is_err());
    let attempts = fresh.len();
    let identical_fresh_restores = fresh.iter().all(|f| *f == before);
    assert_eq!(attempts, 2);
    assert!(identical_fresh_restores);
    let paused = live.failure().is_some();
    assert!(paused);
    assert!(!live.has_pending());
    let native_unchanged = migration::checkpoint(live.authority()).unwrap() == before;
    assert!(native_unchanged);
    assert_eq!(live.primary().receipt(), &frontier);
    // No event was consumed or skipped: the same verified view is still queued.
    let view_retained = seats[0].queue.lossy().map(|(n, b)| (n, b.to_vec())) == retained
        && retained.as_ref().is_some_and(|(_, b)| *b == view);
    assert!(view_retained);
    assert!(live
        .stage(base + 2 * STEP, &queues, &BTreeMap::new(), |_| panic!(
            "paused runtime must not retry native work"
        ))
        .is_err());
    assert!(live.poll().unwrap().is_empty());
    // The paused original still accepts the genuine signed fence.
    let Replica {
        mut actor,
        mut network,
        peer,
        keys,
        ..
    } = replica;
    let mut watch_ms = wall() + 250;
    while !actor.expired(watch_ms).unwrap() {
        watch_ms += 250;
    }
    let (recovery, mut warm) = actor.begin_recovery(&keys, watch_ms, false).unwrap();
    assert_eq!(recovery.promotion().final_receipt, frontier);
    while let Some(wire) = warm.poll_send(watch_ms).unwrap() {
        network.send_reliable(peer, wire);
    }
    let fence_started = Instant::now();
    let fenced = loop {
        if let Some(migration::RuntimeEvent::Promoted(promotion)) = live
            .poll()
            .unwrap()
            .into_iter()
            .find(|event| matches!(event, migration::RuntimeEvent::Promoted(_)))
        {
            break promotion.final_receipt == frontier && live.primary().is_closed();
        }
        bounded(fence_started);
    };
    assert!(fenced);
    let handoff = live.into_recovery().unwrap();
    let same_warm_channel = handoff.channel.tls() == original_tls;
    assert!(same_warm_channel);
    println!("STAMPEDE NATIVE FAULT attempts={attempts} identical_fresh_restores={identical_fresh_restores} native_unchanged={native_unchanged} view_retained={view_retained} paused={paused} signed_fence_after_pause={fenced} same_warm_channel={same_warm_channel}");
}

#[test]
fn owned_primary_own_approval_expiry_suppresses_the_next_native_attempt() {
    let mut lobby = Lobby::new();
    let (descriptor, source, mut receiver, clock) = complete_bootstrap(&lobby);
    let (expiring, until) = shortly_expiring(&lobby.original);
    let host = std::mem::replace(
        &mut lobby.host,
        HostAuthority::new_headless(Box::new(Script::default()), HostConfig::default()),
    );
    let mut live =
        migration::LivePrimary::new(descriptor.clone(), source, host, expiring, clock).unwrap();
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let before = migration::checkpoint(live.authority()).unwrap();
    let mut calls = 0;
    assert!(live
        .stage(base + STEP, &[], &BTreeMap::new(), |_| {
            calls += 1;
            while unix_now() <= until {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(Error::Bounds)
        })
        .is_err());
    let native_attempts = calls;
    assert_eq!(native_attempts, 1);
    let paused = live.failure().is_some();
    assert!(paused);
    assert!(!live.has_pending());
    let native_unchanged = migration::checkpoint(live.authority()).unwrap() == before;
    assert!(native_unchanged);
    assert_eq!(live.primary().receipt().index, 0);
    assert!(live
        .stage(base + STEP, &[], &BTreeMap::new(), |_| {
            calls += 1;
            Ok(())
        })
        .is_err());
    let later_attempts = calls - native_attempts;
    assert_eq!(later_attempts, 0);
    let mut pair_frames = 0;
    while let Some(event) = receiver.network.poll() {
        if matches!(event, HostNetEvent::Reliable { .. }) {
            pair_frames += 1;
        }
    }
    assert_eq!(pair_frames, 0);
    println!("STAMPEDE OWN APPROVAL EXPIRY native_attempts={native_attempts} attempts_after_expiry={later_attempts} paused={paused} native_unchanged={native_unchanged} pair_frames_after_expiry={pair_frames}");
}

// Owned standby runtime. Both ends are the owned runtimes over the actual QUIC
// pair link: `LivePrimary` on the headless original and `LiveStandby` on the
// standby device. Humans stay frozen (their inputs starve). Both runners
// sample one shared `RunnerClock`, so the watch expires on real elapsed time
// once the standby stops seeing progress; there is no process kill.

/// The original's owned runtime over the lobby's native authority.
fn owned_primary<const N: usize>(
    lobby: &mut Lobby<N>,
    descriptor: &Descriptor,
    source: migration::PrimaryBootstrap,
    clock: migration::RunnerClock,
) -> migration::LivePrimary {
    let host = std::mem::replace(
        &mut lobby.host,
        HostAuthority::new_headless(Box::new(Script::default()), HostConfig::default()),
    );
    migration::LivePrimary::new(
        descriptor.clone(),
        source,
        host,
        lobby.original.clone(),
        clock,
    )
    .unwrap()
}

/// Stage batch `index` at logical time `now` and drive both owned runtimes
/// until the original publishes it. Returns whether the standby had installed
/// and returned that receipt before the original's `Committed` event.
fn commit_once(
    live: &mut migration::LivePrimary,
    standby: &mut migration::LiveStandby,
    index: u64,
    now: u64,
) -> bool {
    commit_starting(live, standby, index, now, index == 1)
}

/// As [`commit_once`]; the batch starts the race only when `start` is set.
fn commit_starting(
    live: &mut migration::LivePrimary,
    standby: &mut migration::LiveStandby,
    index: u64,
    now: u64,
    start: bool,
) -> bool {
    let before = migration::checkpoint(live.authority()).unwrap();
    assert!(live
        .stage(now, &[], &BTreeMap::new(), |h| {
            if start && !h.start_race(now) {
                return Err(Error::Invalid("race start"));
            }
            Ok(())
        })
        .unwrap());
    let started = Instant::now();
    let mut installed: Option<Receipt> = None;
    loop {
        for event in live.poll().unwrap() {
            match event {
                migration::RuntimeEvent::Committed { .. } => {
                    assert_eq!(live.primary().receipt().index, index);
                    let first = installed.as_ref().map(|r| r.index) == Some(index);
                    // Its certificate trails on a later frame; take it now so
                    // no later poll reports it.
                    original_cert(live, || {
                        standby.poll().unwrap();
                    });
                    return first;
                }
                migration::RuntimeEvent::Promoted(_) => panic!("unexpected promotion"),
                migration::RuntimeEvent::Membership(_) => {}
            }
        }
        assert_eq!(
            migration::checkpoint(live.authority()).unwrap(),
            before,
            "native authority adopted before its receipt"
        );
        for receipt in standby.poll().unwrap() {
            assert_eq!(receipt.index, index);
            assert_eq!(standby.receipt(), &receipt);
            installed = Some(receipt);
        }
        bounded(started);
    }
}

/// Promote now, then every ~5 ms of real time while the only refusal is the
/// unexpired watch; the standby samples its own runner clock. Every such
/// refusal must hand the standby back. Returns the number of early refusals;
/// the promotion time is [`migration::PromotedStandby::promoted_at_ms`].
fn promote_after_watch(
    mut standby: migration::LiveStandby,
) -> std::result::Result<(migration::PromotedStandby, usize), migration::PromoteRefused> {
    let started = Instant::now();
    let mut refusals = 0;
    loop {
        match standby.promote() {
            Ok(promoted) => return Ok((promoted, refusals)),
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => {
                standby = *back;
                refusals += 1;
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "watch never expired"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(refused) => return Err(refused),
        }
    }
}

#[test]
fn owned_standby_replicates_the_race_and_refuses_the_closed_term() {
    let mut lobby = Lobby::new();
    let (descriptor, source, mut receiver, clock) = complete_bootstrap(&lobby);
    let root = descriptor.root.clone();
    let wall = || clock.now_ms();
    let initial = state_hash(receiver.sequencer.image()).unwrap();
    // An early roster event on the standby listener, buffered before the
    // owned runtime starts. It must survive promotion in order.
    let early = HostNetEvent::Left {
        peer: receiver.original_peer.wrapping_add(1000),
    };
    receiver.pending.push_back(early.clone());
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&root, live.primary().committed_image())
        .unwrap()
        .logical_ms;

    // 1. Owned replication from the lobby through the countdown into the race.
    // Each receipt leaves the standby only after a native fresh restore and
    // installation, before the original can publish its commit.
    let mut index = 0u64;
    let mut racing_from = None;
    let mut ack_after_install = true;
    let mut native_equal = true;
    let mut items_seen = 0;
    while racing_from.is_none_or(|from| index - from < 30) {
        index += 1;
        assert!(index < 400, "race never started");
        let restores = standby.restores();
        ack_after_install &= commit_once(&mut live, &mut standby, index, base + index * STEP);
        assert!(
            ack_after_install,
            "receipt observed before standby installation at {index}"
        );
        assert!(standby.restores() > restores);
        assert_eq!(live.primary().committed_image(), standby.image());
        let restored = standby.committed_authority().unwrap();
        native_equal &= migration::checkpoint(&restored).unwrap()
            == migration::checkpoint(live.authority()).unwrap();
        assert!(
            native_equal,
            "standby native state differs at commit {index}"
        );
        if let Some(world) = live.authority().world() {
            if racing_from.is_none() && world.phase == RacePhase::Racing {
                racing_from = Some(index);
            }
            if racing_from.is_some() {
                items_seen = items_seen.max(
                    world.coins.len()
                        + world.loose_coins.len()
                        + world.projectiles.len()
                        + world.hazards.len(),
                );
            }
        }
    }
    let commits = index;
    let humans = live.authority().human_count();
    let bots = live.authority().world().unwrap().racers.len() - humans;
    let host_seats = root
        .members
        .iter()
        .filter(|m| m.key == Some(root.host))
        .count();
    let no_phantom_seat = host_seats == 0 && root.members.len() == humans;
    assert!(no_phantom_seat);
    assert!(humans >= 1 && bots >= 1);
    assert!(items_seen > 0);
    println!("STAMPEDE OWNED STANDBY COMMITS commits={commits} humans={humans} bots={bots} host_seats={host_seats} no_phantom_seat={no_phantom_seat} racing_batches={} items_in_world={items_seen} ack_after_install={ack_after_install} native_equal_each_commit={native_equal} standby_restores={}", commits - racing_from.unwrap(), standby.restores());

    // 2. Race: the standby asks for a liveness reply, then the original stages
    // C+1 and sends its candidate. The standby only moves those frames into
    // its queue before promoting.
    let frontier = live.primary().receipt().clone();
    let committed = standby.image().to_vec();
    let native_before = migration::checkpoint(live.authority()).unwrap();
    // The standby's own polls probe every 50 ms: wait out that pacing so this
    // probe is a fresh one, not coalesced.
    std::thread::sleep(Duration::from_millis(60));
    standby.probe().unwrap();
    assert!(standby.poll().unwrap().is_empty());
    let next = base + (commits + 1) * STEP;
    assert!(live.stage(next, &[], &BTreeMap::new(), |_| Ok(())).unwrap());
    let restores_before_close = standby.restores();
    let race_started = Instant::now();
    // The candidate leaves from `stage` itself, so frames queue at once; keep
    // the original polling a little longer so its answer to the probe (a
    // term-0 control frame) is sent too.
    while standby.queued_bytes() == 0 || race_started.elapsed() < Duration::from_millis(50) {
        assert!(live.poll().unwrap().is_empty());
        standby.intake().unwrap();
        bounded(race_started);
    }
    let restores_after_close = standby.restores() - restores_before_close;
    assert_eq!(restores_after_close, 0);
    let (mut promoted, early_refusals) = promote_after_watch(standby).unwrap();
    assert_eq!(promoted.recovery.promotion().final_receipt, frontier);
    assert_eq!(promoted.recovery.sequencer().image(), committed.as_slice());
    let closed_hash = state_hash(promoted.recovery.sequencer().image()).unwrap();
    loop {
        promoted.poll(wall()).unwrap();
        if live
            .poll()
            .unwrap()
            .iter()
            .any(|event| matches!(event, migration::RuntimeEvent::Promoted(_)))
        {
            break;
        }
        bounded(race_started);
    }
    assert!(live.primary().is_closed());
    let pending_dropped = !live.has_pending();
    assert!(pending_dropped);
    assert_eq!(live.primary().receipt(), &frontier);
    let mut calls = 0;
    assert_eq!(
        live.stage(next, &[], &BTreeMap::new(), |_| {
            calls += 1;
            Ok(())
        }),
        Err(Error::Closed)
    );
    assert_eq!(calls, 0);
    // Wait for the stale frames to arrive however they are classified, so a
    // missing filter fails on the retained-bytes assertion, not a deadline.
    while (promoted.obsolete().candidate_frames == 0 || promoted.obsolete().other_frames == 0)
        && promoted.retained_bytes() == 0
    {
        promoted.poll(wall()).unwrap();
        bounded(race_started);
    }
    assert_eq!(
        promoted.retained_bytes(),
        0,
        "term-0 frames crossed into recovery"
    );
    let stale = promoted.obsolete();
    assert!(stale.candidate_frames > 0 && stale.other_frames > 0);
    assert_eq!(live.obsolete().total(), 0);
    let mut link = promoted.into_link();

    // 3a. A delayed old receipt for C (genuinely signed by the standby,
    // re-framed in term 0) reaches the closed original. It has no effect.
    let old_receipt = frontier.encode(&root.context().unwrap()).unwrap();
    let receipt_frames =
        Frame::split(root.session, 0, 1, PacketKind::Receipt, &old_receipt).unwrap();
    for frame in &receipt_frames {
        link.network
            .send_reliable(link.peer, frame.encode().unwrap());
    }
    while live.obsolete().total() < receipt_frames.len() && live.queued_bytes() == 0 {
        assert!(live.poll().unwrap().is_empty());
        bounded(race_started);
    }
    assert_eq!(
        live.queued_bytes(),
        0,
        "term-0 frame crossed into original recovery"
    );
    let original_ignored_old_receipt = live.obsolete().other_frames == receipt_frames.len();
    assert!(original_ignored_old_receipt);
    assert!(!live.has_pending());
    assert_eq!(live.primary().receipt(), &frontier);
    let native_unchanged = migration::checkpoint(live.authority()).unwrap() == native_before;
    assert!(native_unchanged);
    let mut handoff = live.into_recovery().unwrap();
    assert_eq!(handoff.ingress.queued_bytes(), 0);

    // 3b. A delayed, genuinely signed term-0 capability ACK reaches the
    // promoted standby over the original's handed-over QUIC link. The
    // headless original casts no vote of its own, so this is the other
    // human's activation vote, re-framed in its old term. It has no effect.
    let other_key = lobby.identities[1].pubkey().to_bytes();
    let other_member = root
        .members
        .iter()
        .find(|m| m.key == Some(other_key))
        .unwrap();
    let old_ack = CapabilityAck::sign(
        &root,
        other_member.player,
        initial,
        1,
        lobby.identities[1].keys(),
    )
    .unwrap()
    .encode(&root)
    .unwrap();
    let ack_frames = Frame::split(root.session, 0, 1, PacketKind::Control, &old_ack).unwrap();
    for frame in &ack_frames {
        handoff.network.send_reliable(frame.encode().unwrap());
    }
    while link.obsolete.other_frames < stale.other_frames + ack_frames.len()
        && link.ingress.queued_bytes() == 0
    {
        link.intake().unwrap();
        handoff.intake().unwrap();
        bounded(race_started);
    }
    assert_eq!(
        link.ingress.queued_bytes(),
        0,
        "old ACK crossed into recovery"
    );
    let standby_ignored_old_ack =
        link.obsolete.other_frames == stale.other_frames + ack_frames.len();
    assert!(standby_ignored_old_ack);
    assert_eq!(link.recovery.sequencer().receipt(), &frontier);
    let hash_unchanged = state_hash(link.recovery.sequencer().image()).unwrap() == closed_hash;
    assert!(hash_unchanged);

    // 4. The original's transport closes after promotion. The early roster
    // event is carried forward as a game event. The original's departure is
    // a pair-link event: visible in the link state, never in game events.
    drop(handoff);
    while link.original != migration::OriginalLink::Left {
        link.intake().unwrap();
        bounded(race_started);
    }
    // Drain anything else already delivered, so a late-routed `Left` would
    // be seen here.
    link.intake().unwrap();
    let events = link.events().cloned().collect::<Vec<_>>();
    let early_kept = events == [early.clone()];
    let left_in_link_state = link.original == migration::OriginalLink::Left;
    let left_in_game_events = events
        .iter()
        .any(|event| *event == HostNetEvent::Left { peer: link.peer });
    assert!(
        early_kept,
        "early roster event not carried alone: {events:?}"
    );
    assert!(left_in_link_state);
    assert!(
        !left_in_game_events,
        "pair-link Left mixed into game events: {events:?}"
    );
    assert_eq!(link.ingress.queued_bytes(), 0);
    let carried = link.take_events();
    assert_eq!(carried, [early]);
    println!("STAMPEDE OBSOLETE INGRESS final_index={} refused_candidate_frames={} standby_ignored_term0_control_frames={} standby_ignored_old_ack={standby_ignored_old_ack} original_ignored_old_receipt={original_ignored_old_receipt} standby_restores_after_close={restores_after_close} retained_bytes={} original_pending_dropped={pending_dropped} original_native_unchanged={native_unchanged} state_hash_unchanged={hash_unchanged} watch_refusals_returned_standby={early_refusals}", frontier.index, stale.candidate_frames, link.obsolete.other_frames, link.ingress.queued_bytes());
    println!("STAMPEDE EVENT CARRY-OVER early_roster_event_kept={early_kept} original_left_in_link_state={left_in_link_state} original_left_in_game_events={left_in_game_events} original_link={:?} events={}", link.original, events.len());
}

// Fault narrowing on the owned standby, over actual QUIC. A frame from the
// original that fails a protocol check only stops replication: the standby
// stays healthy and promotes once the watch expires. A standby-side fault
// latches and promotion is refused, with the standby handed back. The
// latching fault here is an original-delivered replay conflict: a member's
// genuine activation vote is re-sent under the same replay sequence with a
// different state hash, which Fray treats as a permanent replay conflict.
// That it latches although the original delivered it is a filed open
// question. The standby's own approval lapse, a purely standby-side fault,
// is covered by `standby_own_approval_expiry_latches_and_refuses_promotion`.
#[test]
fn standby_side_fault_latches_but_original_frame_rejection_does_not() {
    let mut outcomes = vec![];
    for case in ["frame_rejected", "replay_conflict_from_original"] {
        let lobby = Lobby::new();
        let (descriptor, mut source, receiver, clock) = complete_bootstrap(&lobby);
        let root = descriptor.root.clone();
        let now = || clock.now_ms();
        let mut standby = migration::LiveStandby::new(
            descriptor.clone(),
            receiver,
            lobby.identities[0].clone(),
            clock,
        )
        .unwrap();
        let bytes = match case {
            // Not a control record at all: Fray's named control-kind check.
            "frame_rejected" => vec![0xff; 40],
            _ => {
                let key = lobby.identities[1].pubkey().to_bytes();
                let member = root.members.iter().find(|m| m.key == Some(key)).unwrap();
                CapabilityAck::sign(
                    &root,
                    member.player,
                    [42; 32],
                    1,
                    lobby.identities[1].keys(),
                )
                .unwrap()
                .encode(&root)
                .unwrap()
            }
        };
        source.channel.enqueue(PacketKind::Control, &bytes).unwrap();
        while let Some(wire) = source.channel.poll_send(now()).unwrap() {
            source.network.send_reliable(wire);
        }
        let wait = Instant::now();
        let mut polled = Ok(vec![]);
        while polled.is_ok() && standby.original() == migration::OriginalLink::Live {
            polled = standby.poll();
            bounded(wait);
        }
        let latched = standby.failure().is_some();
        let original = standby.original();
        let receipt = standby.receipt().clone();
        let mut watch_refusals = 0;
        let promoted = match promote_after_watch(standby) {
            Ok((promoted, refusals)) => {
                assert_eq!(promoted.recovery.promotion().final_receipt, receipt);
                watch_refusals = refusals;
                Ok(())
            }
            Err(refused) => Err((refused.error, refused.standby.is_some())),
        };
        match case {
            "frame_rejected" => {
                assert!(polled.is_ok(), "{polled:?}");
                assert!(!latched, "original frame rejection latched the standby");
                assert_eq!(original, migration::OriginalLink::Rejected);
                assert_eq!(promoted, Ok(()));
            }
            _ => {
                assert_eq!(polled, Err(Error::Conflict));
                assert!(latched, "standby-side fault did not latch");
                assert_eq!(original, migration::OriginalLink::Live);
                assert_eq!(promoted, Err((Error::Closed, true)));
            }
        }
        outcomes.push(format!(
            "{case}: latched={latched} original_link={original:?} watch_refusals_returned_standby={watch_refusals} promote={}",
            if promoted.is_ok() {
                "ok"
            } else {
                "refused_standby_returned"
            }
        ));
    }
    println!("STAMPEDE STANDBY FAULT NARROWING {}", outcomes.join(" "));
}

// The original's signed approval on the pair link expires mid-race. The
// standby stops replicating without latching a fault and still promotes on
// its own admission once the watch expires. The original's runtime keeps a
// long-lived approval, so it keeps proposing.
#[test]
fn original_approval_expiry_stops_replication_but_standby_still_promotes() {
    let mut lobby = Lobby::new();
    let (expiring, until) = shortly_expiring(&lobby.original);
    let (descriptor, source, receiver, clock) = complete_bootstrap_as(&lobby, &expiring);
    let wall = || clock.now_ms();
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let mut commits = 0;
    for index in 1..=4 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
        commits = index;
    }
    assert_eq!(standby.receipt().index, commits);
    let frontier = standby.receipt().clone();
    // Keep both sides polling across the expiry instant. The standby keeps
    // probing (Fray allows one probe per 50 ms), so its 200 ms watch stays
    // fresh while the original answers.
    let wait = Instant::now();
    let mut probed = 0;
    while standby.original() == migration::OriginalLink::Live {
        assert!(live.poll().unwrap().is_empty());
        if wall() >= probed + 60 {
            probed = wall();
            standby.probe().unwrap();
        }
        assert!(standby.poll().unwrap().is_empty());
        assert!(wait.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(unix_now() >= until);
    assert_eq!(standby.original(), migration::OriginalLink::Unauthorized);
    assert!(standby.failure().is_none());
    // An unauthorized original is not a closed transport: promotion right
    // after the lapse is refused until the watch expires, and the standby is
    // handed back.
    let attempt = standby.promote();
    let promoted_before_watch = attempt.is_ok();
    assert!(
        !promoted_before_watch,
        "unauthorized original treated as a closed transport"
    );
    let refused = attempt.err().unwrap();
    assert_eq!(refused.error, Error::Stale);
    let promote_refused_while_unauthorized = refused.standby.is_some();
    assert!(promote_refused_while_unauthorized);
    let mut standby = *refused.standby.unwrap();
    assert_eq!(standby.original(), migration::OriginalLink::Unauthorized);
    let restores_at_expiry = standby.restores();
    // The still-running original proposes C+1; the standby never restores it.
    assert!(live
        .stage(base + (commits + 1) * STEP, &[], &BTreeMap::new(), |_| Ok(
            ()
        ))
        .unwrap());
    // The standby no longer answers it, so the original's standby-progress
    // watch may convict the standby (`Unauthorized`, backup lost) once C+1
    // has waited `STANDBY_WATCH_MS`.
    let race = Instant::now();
    while race.elapsed() < Duration::from_millis(200) {
        match live.poll() {
            Ok(events) => assert!(events.is_empty()),
            Err(error) => assert_eq!(error, Error::Unauthorized),
        }
        assert!(standby.poll().unwrap().is_empty());
        std::thread::sleep(Duration::from_millis(2));
    }
    let restores_after_expiry = standby.restores() - restores_at_expiry;
    assert_eq!(restores_after_expiry, 0);
    assert!(live.has_pending());
    assert_eq!(standby.receipt(), &frontier);
    let standby_latched = standby.failure().is_some();
    assert!(!standby_latched);
    // Promotion succeeds once the watch has expired.
    let (mut promoted, _) = promote_after_watch(standby).unwrap();
    assert_eq!(promoted.recovery.promotion().final_receipt, frontier);
    let link_at_promotion = promoted.original();
    assert_eq!(link_at_promotion, migration::OriginalLink::Unauthorized);
    // Nothing, not even Promote, is released to the unauthorized original.
    // Stampede's listener kicks the expired original when it is next polled,
    // so the original may see its transport close instead.
    promoted.poll(wall()).unwrap();
    let quiet = Instant::now();
    while quiet.elapsed() < Duration::from_millis(100) {
        match live.poll() {
            Ok(events) => assert!(events.is_empty()),
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let promote_withheld = !live.primary().is_closed();
    assert!(promote_withheld);
    println!("STAMPEDE ORIGINAL EXPIRY PROMOTION commits_before_expiry={commits} original_link={link_at_promotion:?} standby_latched={standby_latched} promote_refused_while_unauthorized={promote_refused_while_unauthorized} restores_after_expiry={restores_after_expiry} promoted_final_index={} promote_withheld_from_expired_original={promote_withheld} link_after_promoted_poll={:?}", promoted.recovery.promotion().final_receipt.index, promoted.original());
}

// The standby's own signed approval lapses mid-race: a purely standby-side
// fault. The standby latches, never restores the original's next candidate,
// and refuses promotion even after the watch expires, handing itself back.
// The pair link keeps its long-lived approvals, so the original stays live.
#[test]
fn standby_own_approval_expiry_latches_and_refuses_promotion() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let (expiring, until) = shortly_expiring(&lobby.identities[0]);
    let mut standby =
        migration::LiveStandby::new(descriptor.clone(), receiver, expiring, clock).unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let mut commits = 0;
    for index in 1..=4 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
        commits = index;
    }
    let frontier = standby.receipt().clone();
    assert_eq!(frontier.index, commits);
    let wait = Instant::now();
    let poll_error = loop {
        assert!(live.poll().unwrap().is_empty());
        match standby.poll() {
            Ok(installed) => assert!(installed.is_empty()),
            Err(error) => break error,
        }
        assert!(wait.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(unix_now() >= until);
    assert_eq!(poll_error, Error::Unauthorized);
    let latched = standby.failure() == Some(&Error::Unauthorized);
    assert!(latched, "own approval lapse did not latch");
    let original_link = standby.original();
    assert_eq!(original_link, migration::OriginalLink::Live);
    let restores_at_expiry = standby.restores();
    assert!(live
        .stage(base + (commits + 1) * STEP, &[], &BTreeMap::new(), |_| Ok(
            ()
        ))
        .unwrap());
    // The latched standby answers nothing, so the original's standby-progress
    // watch convicts it: the original soft-pauses as backup lost
    // (`backup_lost`, not a latch) once batch 5 has waited
    // `STANDBY_WATCH_MS`, keeps polling, and never adopts it.
    let race = Instant::now();
    let mut original_paused = false;
    while race.elapsed() < Duration::from_millis(300) {
        assert!(live.poll().unwrap().is_empty());
        original_paused |= live.backup_lost();
        assert_eq!(standby.poll(), Err(Error::Closed));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        original_paused,
        "the original never paused on its lost standby"
    );
    assert!(live.backup_lost(), "the pause cleared with no standby");
    assert_eq!(live.failure(), None);
    assert_eq!(live.primary().receipt().index, commits);
    let restores_after_expiry = standby.restores() - restores_at_expiry;
    assert_eq!(restores_after_expiry, 0);
    assert_eq!(standby.receipt(), &frontier);
    // Refused at every watch sample: `promote` checks the latch before the
    // watch, so a latched standby is `Closed` whatever its watch reads.
    let refused = promote_after_watch(standby).err().unwrap();
    assert_eq!(refused.error, Error::Closed);
    let standby_returned = refused.standby.is_some();
    assert!(standby_returned);
    let back = refused.standby.unwrap();
    assert_eq!(back.receipt(), &frontier);
    assert!(back.failure().is_some());
    println!("STAMPEDE STANDBY OWN APPROVAL EXPIRY commits_before_expiry={commits} poll_error={poll_error:?} latched={latched} original_link={original_link:?} restores_after_expiry={restores_after_expiry} promote={:?} standby_returned={standby_returned} receipt_index={}", refused.error, back.receipt().index);
}

// An app that probes every frame (about 10 ms here) must neither latch a
// healthy standby nor pause the original. The wrappers coalesce probes inside
// Fray's 50 ms minimum gap, so the actors see at most one probe per 50 ms
// (Fray itself answers a faster one with `Stale`, which would latch both
// sides). The standby's 200 ms watch stays fresh across 400 ms of fast
// probing only if probes do reach the actor and the original answers, the
// original still commits, and the standby still promotes.
#[test]
fn per_frame_probes_neither_latch_the_standby_nor_pause_the_original() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    let mut attempts = 0u32;
    let fast = Instant::now();
    while fast.elapsed() < Duration::from_millis(400) {
        live.probe()
            .expect("original probe refused: paused by fast probes");
        standby
            .probe()
            .expect("standby probe refused: latched by fast probes");
        attempts += 1;
        assert!(live.poll().unwrap().is_empty());
        assert!(standby.poll().unwrap().is_empty());
        assert!(live.failure().is_none(), "original paused by fast probes");
        assert!(
            standby.failure().is_none(),
            "standby latched by fast probes"
        );
        if fast.elapsed() > Duration::from_millis(250) {
            // Past one 200 ms watch period with no commits: only answered
            // probes keep the standby fresh.
            assert!(
                !standby.expired().unwrap(),
                "probes never reached the actor"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(attempts >= 20, "only {attempts} fast probes");
    assert_eq!(standby.original(), migration::OriginalLink::Live);
    // The original still commits and the standby installs it.
    assert!(commit_once(&mut live, &mut standby, 3, base + 3 * STEP));
    assert!(live.failure().is_none() && standby.failure().is_none());
    let frontier = standby.receipt().clone();
    assert_eq!(frontier.index, 3);
    // The standby still promotes once the watch expires.
    let (promoted, _) = promote_after_watch(standby).unwrap();
    assert_eq!(promoted.recovery.promotion().final_receipt, frontier);
    println!(
        "STAMPEDE FAST PROBES attempts={attempts} commits=3 promoted_final_index={}",
        frontier.index
    );
}

// Pair-link robustness (a): Fray refuses some pair-link frames as `Stale` or
// `Gap` while changing nothing, and the original drops and counts them instead
// of latching. On the actual QUIC pair link: a late certificate for an older
// receipt (refused `Stale`), then a transfer left half-sent across a 600 ms
// poll hitch, so its 500 ms reassembly deadline passes. The original commits
// on, and its certificate keeps up.
//
// Transfer ids: the half transfer never completes, so its id (near `u64::MAX`)
// moves nothing. The late certificate does complete, and a completed id raises
// the receiver's high-water mark: every later frame `MAX_JOURNAL_RECORDS` (512)
// or more below it is ignored, so a completing id near `u64::MAX` would
// silently cut the live stream. It uses 511 instead: never reached by this
// short stream (no collision with a live transfer), and every live id stays
// above `511 - 512`.
#[test]
fn stale_pair_link_frames_are_dropped_and_the_original_still_commits() {
    const HALF_ID: u64 = u64::MAX - 1;
    const LATE_ID: u64 = MAX_JOURNAL_RECORDS as u64 - 1;
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let root = descriptor.root.clone();
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    assert!(commit_once(&mut live, &mut standby, 1, base + STEP));
    let old_certificate = live
        .membership_certificate()
        .unwrap()
        .encode(&root)
        .unwrap();
    assert!(commit_once(&mut live, &mut standby, 2, base + 2 * STEP));
    assert_eq!(live.refused_frames(), 0);

    let half = Frame::split(root.session, 0, HALF_ID, PacketKind::Control, &[0x5a; 1200]).unwrap();
    assert_eq!(half.len(), 2);
    let late = Frame::split(
        root.session,
        0,
        LATE_ID,
        PacketKind::Control,
        &old_certificate,
    )
    .unwrap();
    {
        let (network, peer) = standby.pair_network_mut();
        network.send_reliable(peer, half[0].encode().unwrap());
        for frame in &late {
            network.send_reliable(peer, frame.encode().unwrap());
        }
    }
    let waiting = Instant::now();
    while live.refused_frames() == 0 {
        assert!(live.poll().unwrap().is_empty());
        standby.poll().unwrap();
        bounded(waiting);
    }
    let late_certificate_dropped = live.refused_frames() == 1 && live.failure().is_none();
    assert!(late_certificate_dropped, "a late certificate latched");
    assert_eq!(live.membership_certificate().unwrap().receipt.index, 2);

    // A poll hitch past the half transfer's deadline. Whichever frame reports
    // its expiry (Fray drops it with the partial, or processes it normally),
    // the original neither latches nor stalls: batch 3 commits, by its
    // receipt or by the owed retry.
    std::thread::sleep(Duration::from_millis(600));
    commit_once(&mut live, &mut standby, 3, base + 3 * STEP);
    assert!(live.failure().is_none(), "an expired transfer latched");
    assert_eq!(live.primary().receipt().index, 3);
    assert_eq!(standby.receipt().index, 3);
    commit_once(&mut live, &mut standby, 4, base + 4 * STEP);
    assert!(live.failure().is_none() && standby.failure().is_none());
    assert_eq!(live.membership_certificate().unwrap().receipt.index, 4);
    println!(
        "STAMPEDE STALE PAIR FRAMES late_certificate_dropped={late_certificate_dropped} refused={} retries={} final_index=4",
        live.refused_frames(),
        live.retries()
    );
}

// Pair-link robustness (b): the retry of a waiting batch is owed by time, not
// by a refusal. The standby installs batch 3 and answers; its receipt and
// certificate are swallowed on the original's link before the actor reads
// them, so nothing is ever refused. The original resends batch 3 once
// `RETRY_MIN_GAP_MS` has passed since the stage, the standby answers the
// identical retry again, and batch 3 commits. The retry is due 150 ms after
// the stage, before Fray's 200 ms standby watch, so the room commits within
// about 200 ms with no soft backup-lost pause (the race runs: answered pulses
// do not count as progress, only the commit does).
#[test]
fn a_receipt_dropped_with_no_later_refusal_is_retried() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    let last_commit = Instant::now();
    assert_eq!(live.retries(), 0, "a prompt receipt needs no retry");
    assert!(live
        .stage(base + 3 * STEP, &[], &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    let staged = Instant::now();
    assert!(live.poll().unwrap().is_empty());
    let waiting = Instant::now();
    while standby.poll().unwrap().is_empty() {
        bounded(waiting);
    }
    assert_eq!(standby.receipt().index, 3);
    // Swallow the receipt and the certificate (every chunk of both transfers)
    // before the original's actor can read them.
    let mut transfers: BTreeMap<u64, (PacketKind, BTreeSet<u32>, u32)> = BTreeMap::new();
    let complete = |t: &BTreeMap<u64, (PacketKind, BTreeSet<u32>, u32)>, kind: PacketKind| {
        t.values()
            .any(|(k, seen, count)| *k == kind && seen.len() == *count as usize)
    };
    let waiting = Instant::now();
    while !(complete(&transfers, PacketKind::Receipt) && complete(&transfers, PacketKind::Control))
    {
        while let Some(event) = live.pair_network_mut().poll() {
            let ClientNetEvent::Reliable(bytes) = event else {
                continue;
            };
            let frame = Frame::decode(&bytes).unwrap();
            let entry = transfers.entry(frame.transfer).or_insert((
                frame.kind,
                BTreeSet::new(),
                frame.count,
            ));
            entry.1.insert(frame.ordinal);
        }
        bounded(waiting);
    }
    let swallowed = transfers.len();
    let waiting = Instant::now();
    let committed_after = loop {
        live.probe().unwrap();
        let events = live.poll().unwrap();
        // A pause within 200 ms of the last commit (batch 2) is a conviction
        // before the retry could land; later, under load, it is tolerated.
        assert!(
            !live.backup_lost()
                || last_commit.elapsed() >= Duration::from_millis(migration::STANDBY_WATCH_MS),
            "backup lost {:?} after the last commit, before the retry",
            last_commit.elapsed()
        );
        if events
            .iter()
            .any(|e| matches!(e, migration::RuntimeEvent::Committed { .. }))
        {
            break staged.elapsed();
        }
        standby.poll().unwrap();
        assert!(live.failure().is_none(), "{:?}", live.failure());
        bounded(waiting);
    };
    assert!(
        committed_after < Duration::from_millis(migration::STANDBY_WATCH_MS + 50),
        "committed {committed_after:?} after the stage, not within about 200 ms"
    );
    assert_eq!(live.primary().receipt().index, 3);
    assert_eq!(live.refused_frames(), 0, "no refusal drove the retry");
    assert!(live.retries() >= 1);
    assert!(
        committed_after >= Duration::from_millis(migration::RETRY_MIN_GAP_MS),
        "committed {committed_after:?} after the stage, before any retry was due"
    );
    assert!(commit_once(&mut live, &mut standby, 4, base + 4 * STEP));
    assert!(live.failure().is_none() && standby.failure().is_none());
    println!(
        "STAMPEDE DROPPED RECEIPT RETRIED swallowed_transfers={swallowed} retries={} refused={} committed_after_ms={}",
        live.retries(),
        live.refused_frames(),
        committed_after.as_millis()
    );
}

// `LiveStandby::poll` reports only receipts newly installed in that call. The
// standby installs batch 3 and reports its receipt once; the receipt and
// certificate are swallowed before the original reads them, so the original
// resends the identical batch 3 after `RETRY_MIN_GAP_MS`. The standby answers
// that retry with the installed receipt again (the original then commits),
// but `poll` reports nothing for it: a re-answer is not an install, so no
// consumer of `poll` receipts processes batch 3 twice.
#[test]
fn a_retried_batch_already_installed_yields_no_second_receipt_from_poll() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    assert!(live
        .stage(base + 3 * STEP, &[], &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    assert!(live.poll().unwrap().is_empty());
    let waiting = Instant::now();
    let first = loop {
        let installed = standby.poll().unwrap();
        if !installed.is_empty() {
            break installed;
        }
        bounded(waiting);
    };
    assert_eq!(first.iter().map(|r| r.index).collect::<Vec<_>>(), [3]);
    // Swallow the receipt and the certificate before the original reads them.
    let mut transfers: BTreeMap<u64, (PacketKind, BTreeSet<u32>, u32)> = BTreeMap::new();
    let complete = |t: &BTreeMap<u64, (PacketKind, BTreeSet<u32>, u32)>, kind: PacketKind| {
        t.values()
            .any(|(k, seen, count)| *k == kind && seen.len() == *count as usize)
    };
    let waiting = Instant::now();
    while !(complete(&transfers, PacketKind::Receipt) && complete(&transfers, PacketKind::Control))
    {
        while let Some(event) = live.pair_network_mut().poll() {
            let ClientNetEvent::Reliable(bytes) = event else {
                continue;
            };
            let frame = Frame::decode(&bytes).unwrap();
            transfers
                .entry(frame.transfer)
                .or_insert((frame.kind, BTreeSet::new(), frame.count))
                .1
                .insert(frame.ordinal);
        }
        bounded(waiting);
    }
    // The original retries; the standby's re-answer lets it commit, and no
    // poll reports a receipt again.
    let mut reported = Vec::new();
    let waiting = Instant::now();
    loop {
        live.probe().unwrap();
        let events = live.poll().unwrap();
        if events
            .iter()
            .any(|e| matches!(e, migration::RuntimeEvent::Committed { .. }))
        {
            break;
        }
        reported.extend(standby.poll().unwrap());
        assert!(live.failure().is_none(), "{:?}", live.failure());
        bounded(waiting);
    }
    assert!(live.retries() >= 1, "the batch was not retried");
    assert_eq!(live.primary().receipt().index, 3);
    assert_eq!(standby.receipt().index, 3);
    assert!(
        reported.is_empty(),
        "poll reported a re-answer as an install: {:?}",
        reported.iter().map(|r| r.index).collect::<Vec<_>>()
    );
    // The next batch is reported once, as a new install.
    assert!(commit_once(&mut live, &mut standby, 4, base + 4 * STEP));
    assert!(live.failure().is_none() && standby.failure().is_none());
    println!(
        "STAMPEDE RETRY REANSWER NOT REPORTED retries={} reported_again={}",
        live.retries(),
        reported.len()
    );
}

// Pair-link robustness (c, FIX B): a Promotion that never reaches the original
// must not leave it stalled forever. The standby promotes on its watch while
// the original is silent with batch 3 staged, and its `PromotedStandby` is
// never polled, so the signed Promotion is never sent (its transport stays up:
// no `Left`). The original resumes and sends batch 3 to a closed term; nothing
// answers. Its standby-progress watch reports `Paused`, and the original
// soft-pauses as backup lost (`backup_lost`, the app's "Backup lost — the room
// is paused") on the first poll at least `STANDBY_WATCH_MS` after it resumed:
// about 200 ms plus one frame. Fray defers the verdict that long because the
// original's own stall began within 200 ms of its last probe. The pause is
// not a latch, but nothing ever answers again, so it persists while the
// original keeps polling, probing and retrying.
#[test]
fn a_missed_promotion_pauses_the_original_as_backup_lost() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    assert!(live
        .stage(base + 3 * STEP, &[], &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    // The driver probes every frame; this is its last before the stall.
    live.probe().unwrap();
    let (promoted, _) = promote_after_watch(standby).unwrap();
    assert_eq!(promoted.recovery.promotion().final_receipt.index, 2);
    let resumed = Instant::now();
    let mut previous = resumed;
    let (paused_after, frame_gap) = loop {
        let now = Instant::now();
        live.probe().unwrap();
        let events = live.poll().unwrap();
        assert!(events.is_empty(), "the Promotion was never sent");
        if live.backup_lost() {
            break (now - resumed, now - previous);
        }
        previous = now;
        assert!(
            resumed.elapsed() < Duration::from_secs(2),
            "the original never paused on its missing standby"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(live.failure(), None, "a soft pause, not a latch");
    assert_eq!(live.primary().receipt().index, 2);
    let watch = Duration::from_millis(migration::STANDBY_WATCH_MS);
    // The runner clock and `Instant` are sampled a moment apart: 2 ms slack.
    assert!(
        paused_after + Duration::from_millis(2) >= watch,
        "paused {paused_after:?} after resuming (early)"
    );
    assert!(
        paused_after < watch + frame_gap + Duration::from_millis(20),
        "paused {paused_after:?} after resuming, {frame_gap:?} after the previous poll"
    );
    // Persistent: a second of further polls (probes and retries included,
    // the promoted standby still alive and unpolled) never clears it.
    let held = Instant::now();
    let retries_at_pause = live.retries();
    while held.elapsed() < Duration::from_secs(1) {
        live.probe().unwrap();
        assert!(live.poll().unwrap().is_empty());
        assert!(live.backup_lost(), "the pause cleared with no standby");
        assert_eq!(live.failure(), None);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(live.primary().receipt().index, 2);
    assert!(live.has_pending());
    assert!(live.retries() > retries_at_pause, "retries stopped");
    drop(promoted);
    println!(
        "STAMPEDE MISSED PROMOTION paused_after_ms={} frame_gap_ms={} failure={:?} backup_lost={}",
        paused_after.as_millis(),
        frame_gap.as_millis(),
        live.failure(),
        live.backup_lost()
    );
}

/// Drive the original as the app does (probe, then poll), and poll the
/// standby too when `standby` is given (it probes inside its own poll),
/// about every 5 ms for `ms` of real time with nothing staged by the caller.
/// Returns whether the soft backup-lost pause held at any poll.
fn run_pair(
    live: &mut migration::LivePrimary,
    mut standby: Option<&mut migration::LiveStandby>,
    ms: u64,
) -> bool {
    let mut paused = false;
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(ms) {
        live.probe().unwrap();
        for event in live.poll().unwrap() {
            assert!(matches!(event, migration::RuntimeEvent::Membership(_)));
        }
        paused |= live.backup_lost();
        assert_eq!(live.failure(), None);
        if let Some(standby) = standby.as_mut() {
            assert!(standby.poll().unwrap().is_empty());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    paused
}

/// Stage batch `index` (starting the race when `start`), poll only the
/// original for `hold_ms` (the standby does not run), then both until the
/// batch commits and its certificate arrives. Returns how long after the
/// call (right after the previous commit, or a watch verified live) the
/// soft backup-lost pause was first seen, if at all. Under load only a
/// pause within 200 ms of that is a conviction (`STANDBY_WATCH_MS`).
fn commit_held(
    live: &mut migration::LivePrimary,
    standby: &mut migration::LiveStandby,
    index: u64,
    now: u64,
    start: bool,
    hold_ms: u64,
) -> Option<Duration> {
    let entered = Instant::now();
    let mut paused = None;
    let mut seen = |live: &migration::LivePrimary| {
        if live.backup_lost() && paused.is_none() {
            paused = Some(entered.elapsed());
        }
    };
    assert!(live
        .stage(now, &[], &BTreeMap::new(), |h| {
            if start && !h.start_race(now) {
                return Err(Error::Invalid("race start"));
            }
            Ok(())
        })
        .unwrap());
    while entered.elapsed() < Duration::from_millis(hold_ms) {
        live.probe().unwrap();
        for event in live.poll().unwrap() {
            assert!(matches!(event, migration::RuntimeEvent::Membership(_)));
        }
        seen(live);
        assert_eq!(live.failure(), None);
        std::thread::sleep(Duration::from_millis(5));
    }
    let t = Instant::now();
    'commit: loop {
        live.probe().unwrap();
        for event in live.poll().unwrap() {
            if matches!(event, migration::RuntimeEvent::Committed { .. }) {
                break 'commit;
            }
        }
        seen(live);
        assert_eq!(live.failure(), None);
        standby.poll().unwrap();
        bounded(t);
    }
    seen(live);
    assert_eq!(live.primary().receipt().index, index);
    original_cert(live, || {
        standby.poll().unwrap();
    });
    seen(live);
    paused
}

/// A soft pause seen within 200 ms of the previous commit
/// ([`commit_held`]): a conviction even under load.
fn convicted(paused: Option<Duration>) -> bool {
    paused.is_some_and(|after| after < Duration::from_millis(migration::STANDBY_WATCH_MS))
}

// Soft pause (watch-games item 2): a standby hiccup of about 300 ms while a
// racing batch waits. The original keeps polling and probing; nothing
// answers, so Fray's standby watch reads `Paused` and the original shows the
// soft backup-lost pause, never a latch. When the standby runs again it
// installs the waiting batch, its receipt commits it, the watch reads `Live`
// and the pause clears by itself: the room resumes with no restart.
#[test]
fn a_standby_hiccup_soft_pauses_and_the_room_resumes_by_itself() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    assert_eq!(live.activity(), Activity::Advancing, "the race runs");
    assert!(!live.backup_lost());
    let paused = commit_held(&mut live, &mut standby, 3, base + 3 * STEP, false, 300);
    assert!(
        paused.is_some(),
        "a 300 ms standby hiccup did not pause the room"
    );
    assert!(!live.backup_lost(), "the pause did not clear by itself");
    assert_eq!(live.failure(), None);
    // The resends of batch 3 queued during the hiccup are answered again
    // (the same receipt); the original drops those late copies as `Stale`.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(100) {
        live.probe().unwrap();
        for event in live.poll().unwrap() {
            assert!(matches!(event, migration::RuntimeEvent::Membership(_)));
        }
        for receipt in standby.poll().unwrap() {
            assert_eq!(receipt.index, 3);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // The room plays on.
    assert!(commit_once(&mut live, &mut standby, 4, base + 4 * STEP));
    assert!(!live.backup_lost() && live.failure().is_none());
    assert!(standby.failure().is_none());
    println!(
        "STAMPEDE STANDBY HICCUP paused_after={paused:?} resumed_index={}",
        live.primary().receipt().index
    );
}

/// The game-facing clock an armed driver keeps (the app's session time, the
/// headless host's `SessionTime`): runner time less a shift, to which the
/// driver adds what `LivePrimary::pace` held back, so it stays continuous.
struct DriverTime {
    clock: migration::RunnerClock,
    shift: u64,
}
impl DriverTime {
    fn now(&self) -> u64 {
        self.clock.now_ms().saturating_sub(self.shift)
    }
}

// Soft skew pause (owner decision 2026-10-19, option (a)): mid-race, one
// receipt is held about 300 ms on the pair link (the standby goes offline
// right after the original stages a batch, as a Wi-Fi stall does). The
// commit lag reaches the 250 ms game-clock budget. The original latches
// nothing: it shows the soft behind pause, keeps probing, polling and
// resending, and the driver's clock, paced every frame, holds at the budget
// (never more than 250 ms past the commit) and never jumps. When the
// receipt arrives the room catches up in 50 ms batches, the pause clears by
// itself and the room commits on.
#[test]
fn a_receipt_held_past_the_skew_budget_soft_pauses_and_the_room_commits_on() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let mut time = DriverTime {
        clock,
        shift: clock.now_ms(),
    };
    let shift0 = time.shift;
    const STALL_MS: u64 = 300;
    let mut started = false;
    let mut stall: Option<Instant> = None;
    let mut stall_over: Option<(Instant, u64)> = None;
    let (mut max_gap, mut longest_frame) = (0u64, 0u64);
    let mut last_committed = base;
    let mut commits_after = 0u64;
    let mut frame_at = clock.now_ms();
    let (mut last_game, mut behind_seen, mut cleared) = (base, false, false);
    let t = Instant::now();
    loop {
        // The driver paces its clock first; the shift keeps it continuous.
        let raw = base + time.now();
        let game = live.pace(raw);
        time.shift += raw - game;
        assert!(game >= last_game, "the game clock went back");
        assert!(
            game - last_game <= longest_frame.max(migration::MAX_BATCH_GAME_MS),
            "the game clock jumped {} ms",
            game - last_game
        );
        last_game = game;
        live.probe().unwrap();
        for event in live.poll().unwrap() {
            if let migration::RuntimeEvent::Committed { .. } = event {
                let committed = live.primary().receipt().logical_ms;
                assert!(
                    committed - last_committed <= migration::MAX_BATCH_GAME_MS,
                    "a batch spans over 50 ms of game time"
                );
                last_committed = committed;
                commits_after += u64::from(stall_over.is_some());
            }
        }
        if !live.has_pending() {
            let at = game.min(last_committed + migration::MAX_BATCH_GAME_MS);
            let start = !started;
            match live.stage(game, &[], &BTreeMap::new(), |h| {
                if start && !h.start_race(at) {
                    return Err(Error::Invalid("race start"));
                }
                Ok(())
            }) {
                Ok(staged) => started |= staged,
                Err(e) => panic!(
                    "the original latched {e:?} with game time {game} and commit {last_committed}"
                ),
            }
        }
        assert_eq!(live.failure(), None);
        standby.poll().unwrap();
        // The stall: once racing, right after a batch went on the wire.
        if stall.is_none()
            && live.activity() == Activity::Advancing
            && live.primary().receipt().index >= 10
            && live.has_pending()
        {
            standby.offline_for_test(true);
            stall = Some(Instant::now());
        }
        if stall_over.is_none()
            && stall.is_some_and(|s| s.elapsed() >= Duration::from_millis(STALL_MS))
        {
            standby.offline_for_test(false);
            stall_over = Some((Instant::now(), live.primary().receipt().logical_ms));
        }
        max_gap = max_gap.max(game.saturating_sub(live.primary().receipt().logical_ms));
        behind_seen |= live.behind();
        if stall_over.is_some() && behind_seen && !live.behind() {
            cleared = true;
        }
        // The room commits on: half a second of game time after the stall.
        if cleared
            && stall_over.is_some_and(|(_, at)| live.primary().receipt().logical_ms >= at + 500)
        {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the room stopped committing"
        );
        std::thread::sleep(Duration::from_millis(5));
        let wall = clock.now_ms();
        longest_frame = longest_frame.max(wall - frame_at);
        frame_at = wall;
    }
    assert_eq!(live.failure(), None);
    assert!(standby.failure().is_none());
    assert!(behind_seen, "the held receipt never showed the soft pause");
    assert!(!live.behind(), "the soft pause did not clear by itself");
    assert!(
        max_gap <= migration::MAX_GAME_GAP_MS,
        "the game clock ran {max_gap} ms past the commit"
    );
    assert!(
        commits_after >= 10,
        "only {commits_after} commits after the stall"
    );
    println!(
        "STAMPEDE SOFT SKEW stall_ms={STALL_MS} behind_seen={behind_seen} cleared={cleared} held_ms={} max_gap_ms={max_gap} longest_frame_ms={longest_frame} commits_after={commits_after} retries={} committed_ms={}",
        time.shift - shift0,
        live.retries(),
        live.primary().receipt().logical_ms - base,
    );
}

// The behind pause reflects whether the game actually stops. After an
// opening 300 ms stall, the pair link settles at a steady commit cycle of
// about 60 ms (the standby is polled every 60 ms): over 50 ms, so the commits
// never catch up with real time and the paced clock stays at the 250 ms
// budget, but every cycle commits 50 ms of game time. The game runs on (at
// about 50/60 of real speed), so the pause clears and stays clear (a
// frame that really sees no commit for 100 ms on a loaded machine may show it
// again, so at least 90% of the frames after it cleared), and the room keeps
// committing.
#[test]
fn a_steady_60_ms_commit_cycle_clears_the_behind_pause() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let mut time = DriverTime {
        clock,
        shift: clock.now_ms(),
    };
    const CYCLE_MS: u64 = 60;
    let mut started = false;
    let mut stall: Option<Instant> = None;
    let mut steady: Option<Instant> = None;
    let mut last_standby = Instant::now();
    let (mut behind_seen, mut cleared_at, mut commits) = (false, None::<Instant>, 0u64);
    let (mut frames_after, mut behind_after) = (0u64, 0u64);
    let mut committed_at_clear = 0;
    let t = Instant::now();
    loop {
        let raw = base + time.now();
        let game = live.pace(raw);
        time.shift += raw - game;
        live.probe().unwrap();
        for event in live.poll().unwrap() {
            if let migration::RuntimeEvent::Committed { .. } = event {
                commits += 1;
            }
        }
        if !live.has_pending() {
            let committed = live.primary().receipt().logical_ms;
            let at = game.min(committed + migration::MAX_BATCH_GAME_MS);
            let start = !started;
            match live.stage(game, &[], &BTreeMap::new(), |h| {
                if start && !h.start_race(at) {
                    return Err(Error::Invalid("race start"));
                }
                Ok(())
            }) {
                Ok(staged) => started |= staged,
                Err(e) => panic!("the original latched {e:?}"),
            }
        }
        assert_eq!(live.failure(), None);
        // The pair link: a 300 ms stall once racing, then a 60 ms cycle.
        if stall.is_none()
            && live.activity() == Activity::Advancing
            && live.primary().receipt().index >= 10
            && live.has_pending()
        {
            standby.offline_for_test(true);
            stall = Some(Instant::now());
        }
        if steady.is_none() && stall.is_some_and(|s| s.elapsed() >= Duration::from_millis(300)) {
            standby.offline_for_test(false);
            steady = Some(Instant::now());
        }
        if stall.is_none() || steady.is_some() {
            if steady.is_none() || last_standby.elapsed() >= Duration::from_millis(CYCLE_MS) {
                standby.poll().unwrap();
                last_standby = Instant::now();
            }
        } else {
            standby.poll().unwrap();
        }
        behind_seen |= live.behind();
        if let Some(s) = steady {
            match (live.behind(), cleared_at) {
                (false, None) => {
                    cleared_at = Some(Instant::now());
                    committed_at_clear = live.primary().receipt().logical_ms;
                }
                (behind, Some(_)) => {
                    frames_after += 1;
                    behind_after += u64::from(behind);
                }
                _ => {}
            }
            if cleared_at.is_some_and(|c| c.elapsed() >= Duration::from_secs(2)) {
                break;
            }
            assert!(
                s.elapsed() < Duration::from_secs(4),
                "the behind pause never cleared on a steady {CYCLE_MS} ms cycle"
            );
        }
        assert!(t.elapsed() < Duration::from_secs(20), "the race never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(behind_seen, "the opening stall never showed the pause");
    assert!(
        behind_after * 10 <= frames_after,
        "the pause showed again in {behind_after} of {frames_after} frames"
    );
    let advanced = live.primary().receipt().logical_ms - committed_at_clear;
    assert!(
        advanced >= 1000,
        "only {advanced} ms of game time committed in 2 s after the pause cleared"
    );
    assert!(standby.failure().is_none() && !live.backup_lost());
    println!(
        "STAMPEDE STEADY CYCLE cycle_ms={CYCLE_MS} behind_seen={behind_seen} cleared_after_ms={} game_ms_in_2s={advanced} commits={commits} behind_frames_after={behind_after}/{frames_after}",
        cleared_at.unwrap().duration_since(steady.unwrap()).as_millis()
    );
}

// Activity (watch-games item 1): both runtimes derive `Idle` from a lobby
// checkpoint, the original's driver probes every frame, and the idle standby
// answers with idle pulses. A full second with no commits never pauses the
// room.
#[test]
fn an_idle_lobby_with_probes_never_pauses() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    // The switch window: before the first armed commit nothing advances.
    assert_eq!(live.activity(), Activity::Idle);
    assert_eq!(standby.activity(), Activity::Idle);
    assert!(!run_pair(&mut live, Some(&mut standby), 300));
    // Committed lobby batches (no race start) stay idle on both sides.
    for index in 1..=2 {
        assert!(commit_starting(
            &mut live,
            &mut standby,
            index,
            base + index * STEP,
            false
        ));
    }
    assert_eq!(live.activity(), Activity::Idle);
    assert_eq!(standby.activity(), Activity::Idle);
    let paused = run_pair(&mut live, Some(&mut standby), 1000);
    assert!(!paused, "an idle lobby paused");
    assert!(commit_starting(
        &mut live,
        &mut standby,
        3,
        base + 3 * STEP,
        false
    ));
    assert!(!live.backup_lost() && live.failure().is_none());
    println!("STAMPEDE IDLE LOBBY paused={paused}");
}

// Activity: an idle lobby, then the race starts. The original turns
// `Advancing` when it adopts the race-start batch (the standby when it
// installs it), and the first racing batch, held 100 ms before the standby
// runs, is not convicted: no soft pause at any poll.
#[test]
fn an_idle_to_race_transition_never_convicts_the_first_batch() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    // Under load a slow batch may soft-pause after 200 ms; a pause within
    // 200 ms of the previous commit is a conviction.
    let mut convictions = Vec::new();
    let mut check = |index: u64, paused: Option<Duration>| {
        if convicted(paused) {
            convictions.push((index, paused));
        }
    };
    check(
        1,
        commit_held(&mut live, &mut standby, 1, base + STEP, false, 0),
    );
    let idle_paused = run_pair(&mut live, Some(&mut standby), 400);
    assert!(!idle_paused, "the idle lobby paused");
    assert_eq!(live.activity(), Activity::Idle);
    check(
        2,
        commit_held(&mut live, &mut standby, 2, base + 2 * STEP, true, 0),
    );
    assert_eq!(live.activity(), Activity::Advancing, "the race started");
    assert_eq!(standby.activity(), Activity::Advancing);
    check(
        3,
        commit_held(&mut live, &mut standby, 3, base + 3 * STEP, false, 100),
    );
    for index in 4..=6 {
        let paused = commit_held(
            &mut live,
            &mut standby,
            index,
            base + index * STEP,
            false,
            0,
        );
        check(index, paused);
    }
    assert!(
        convictions.is_empty(),
        "batches convicted after the idle lobby: {convictions:?}"
    );
    assert!(live.failure().is_none() && standby.failure().is_none());
    println!("STAMPEDE IDLE TO RACE convictions={convictions:?}");
}

// Decision (1): the standby probes inside its own poll. Its driver never
// calls `probe()`; in an idle lobby with a healthy original (the original's
// driver probing and polling) its 200 ms watch stays fresh for a second, it
// does not expire, and `promote` is refused (`Stale`) with the standby handed
// back: no false takeover.
#[test]
fn a_standby_whose_driver_never_probes_stays_live_and_does_not_promote() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    assert!(commit_starting(
        &mut live,
        &mut standby,
        1,
        base + STEP,
        false
    ));
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(1000) {
        live.probe().unwrap();
        assert!(live.poll().unwrap().is_empty());
        assert!(standby.poll().unwrap().is_empty());
        if t.elapsed() > Duration::from_millis(250) {
            assert!(!standby.expired().unwrap(), "the standby's watch expired");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(standby.original(), migration::OriginalLink::Live);
    assert!(!live.backup_lost());
    let refused = standby.promote().err().expect("a false takeover");
    assert_eq!(refused.error, Error::Stale);
    let back = refused.standby.expect("the standby handed back");
    assert!(back.failure().is_none());
    assert!(live.failure().is_none());
}

// The standby probes inside its own poll, so an original stall leaves its
// answers to read after their 200 ms window. Fray counts and ignores such a
// late answer (`late_answers`) rather than refusing it as `Stale`, so the
// original stays `Live` (not `Rejected`): replication goes on after the
// stall.
#[test]
fn late_probe_answers_after_an_original_stall_do_not_reject_the_original() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    // The original stalls for 300 ms while the standby polls (and probes).
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        assert!(standby.poll().unwrap().is_empty());
        std::thread::sleep(Duration::from_millis(5));
    }
    // Both run again: the original answers the queued probes late.
    let t = Instant::now();
    while standby.late_answers() == 0 {
        live.probe().unwrap();
        let _ = live.poll().unwrap();
        assert!(standby.poll().unwrap().is_empty());
        assert_eq!(standby.original(), migration::OriginalLink::Live);
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "no late answer was counted"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(standby.original(), migration::OriginalLink::Live);
    assert!(standby.failure().is_none());
    assert!(commit_once(&mut live, &mut standby, 3, base + 3 * STEP));
    assert!(live.failure().is_none() && standby.failure().is_none());
}

// Decision (4): no backup-lost report once promoted. The standby promotes on
// its watch while batch 3 waits; the original, probing and polling with
// nothing answering, shows the soft pause. Then the promoted standby's
// `Promote` reaches it: term 0 is closed, recovery owns the state, and the
// pause is never reported again however long the original keeps polling.
#[test]
fn backup_lost_is_never_reported_after_promote() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    assert!(live
        .stage(base + 3 * STEP, &[], &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    let (mut promoted, _) = promote_after_watch(standby).unwrap();
    // Unpolled, the promoted standby has sent nothing yet: the original
    // soft-pauses.
    let t = Instant::now();
    while !live.backup_lost() {
        live.probe().unwrap();
        assert!(live.poll().unwrap().is_empty());
        assert!(t.elapsed() < Duration::from_secs(2), "never paused");
        std::thread::sleep(Duration::from_millis(5));
    }
    let wall = || clock.now_ms();
    let t = Instant::now();
    'promote: loop {
        promoted.poll(wall()).unwrap();
        live.probe().unwrap();
        for event in live.poll().unwrap() {
            if matches!(event, migration::RuntimeEvent::Promoted(_)) {
                break 'promote;
            }
        }
        bounded(t);
    }
    assert!(!live.backup_lost(), "backup lost reported after Promote");
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(400) {
        live.probe().unwrap();
        let _ = live.poll().unwrap();
        promoted.poll(wall()).unwrap();
        assert!(!live.backup_lost(), "backup lost reported after Promote");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(live.failure().is_none());
}

// Stage before send (watch-games item 4): the candidate leaves from `stage`
// itself, in the frame that staged it. The original is not polled again, yet
// the standby receives and installs the batch.
#[test]
fn a_staged_candidate_is_on_the_wire_in_the_same_frame() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    assert!(live
        .stage(base + 3 * STEP, &[], &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    let t = Instant::now();
    while standby.receipt().index < 3 {
        standby.poll().unwrap();
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "the staged candidate never left without a poll"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let installed_after = t.elapsed();
    assert_eq!(standby.receipt().index, 3);
    let t = Instant::now();
    'commit: loop {
        for event in live.poll().unwrap() {
            if matches!(event, migration::RuntimeEvent::Committed { .. }) {
                break 'commit;
            }
        }
        standby.poll().unwrap();
        bounded(t);
    }
    assert_eq!(live.primary().receipt().index, 3);
    println!(
        "STAMPEDE STAGE BEFORE SEND installed_without_poll_ms={}",
        installed_after.as_millis()
    );
}

// The standby's 200 ms watch runs on real monotonic time that the runner
// samples itself; no caller time reaches it. The original goes silent right
// after sending its last candidate (it is never polled again; its transport
// stays up, so this is not `Left`). Until 200 ms of real time have passed
// since the standby installed that candidate, the watch is unexpired and
// promotion is refused with the standby handed back. After that the standby
// promotes, and the promotion's own clock sample is at least 200 ms after
// the last progress.
#[test]
fn standby_watch_expires_on_real_time_after_the_original_goes_silent() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    for index in 1..=2 {
        assert!(commit_once(
            &mut live,
            &mut standby,
            index,
            base + index * STEP
        ));
    }
    // The last progress: candidate 3, installed inside one standby poll. The
    // marks before that poll are no later than the watch's progress sample;
    // the mark after it is no earlier.
    assert!(live
        .stage(base + 3 * STEP, &[], &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    let wait = Instant::now();
    let (mark, mark_ms, mark_after) = loop {
        assert!(live.poll().unwrap().is_empty());
        let (mark, mark_ms) = (Instant::now(), clock.now_ms());
        let installed = standby.poll().unwrap();
        if let Some(receipt) = installed.last() {
            assert_eq!(receipt.index, 3);
            break (mark, mark_ms, Instant::now());
        }
        bounded(wait);
    };
    // From here the original is silent.
    assert_eq!(standby.original(), migration::OriginalLink::Live);
    assert!(
        !standby.expired().unwrap(),
        "watch expired before 200 ms of real time"
    );
    let refused = standby.promote().err().unwrap();
    let first_refusal_after = mark.elapsed();
    assert!(
        first_refusal_after < Duration::from_millis(200),
        "first promotion attempt came {first_refusal_after:?} after progress"
    );
    assert_eq!(refused.error, Error::Stale);
    let mut standby = *refused.standby.expect("refusal returns the standby");
    let mut refusals = 1u32;
    let mut latest_unexpired = Duration::ZERO;
    // Fray's "a local stall is not peer silence": after a gap over
    // `STALL_GAP_MS` between two watch samples (on the runner clock), the
    // verdict is deferred until 200 ms after the first sample past the gap.
    // Every standby call below samples the watch, so a gap is possible only
    // where the runner clock moved more than `STALL_GAP_MS` from the start of
    // the previous iteration's calls to the end of this iteration's
    // `expired`; the deferral then ends no later than 200 ms after that end.
    // The first gap is counted from the poll that installed the progress.
    let mut previous_start = mark_ms;
    let mut deferred_until: Option<u64> = None;
    let mut max_gap = 0u64;
    let promoted = loop {
        let start = clock.now_ms();
        assert!(standby.poll().unwrap().is_empty());
        let before = mark_after.elapsed();
        let expired = standby.expired().unwrap();
        let end = clock.now_ms();
        let gap = end - previous_start;
        max_gap = max_gap.max(gap);
        if gap > STALL_GAP_MS {
            deferred_until = deferred_until.max(Some(end + 200));
        }
        previous_start = start;
        if !expired {
            // Unexpired means less than 201 ms (whole-ms watch) since
            // progress, unless a measured stall deferred the verdict.
            let in_deferral = deferred_until.is_some_and(|until| clock.now_ms() <= until);
            assert!(
                before < Duration::from_millis(201) || in_deferral,
                "watch still unexpired {before:?} after progress (max sampling gap {max_gap} ms, deferred until {deferred_until:?}, now {} ms)",
                clock.now_ms()
            );
            latest_unexpired = latest_unexpired.max(before);
        }
        match standby.promote() {
            Ok(promoted) => break promoted,
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => {
                standby = *back;
                refusals += 1;
            }
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            mark.elapsed() < Duration::from_secs(5),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let promoted_after = mark.elapsed();
    let promoted_after_ms = promoted.promoted_at_ms() - mark_ms;
    assert!(
        promoted_after_ms >= 200,
        "promoted {promoted_after_ms} runner ms after progress"
    );
    // The watch counts whole runner milliseconds, so real time can be up to
    // 1 ms short of the runner-clock gap asserted exactly above.
    assert!(
        promoted_after >= Duration::from_millis(199),
        "promoted {promoted_after:?} after progress"
    );
    assert!(refusals >= 2, "only {refusals} refusals before expiry");
    assert_eq!(promoted.recovery.promotion().final_receipt.index, 3);
    println!(
        "STAMPEDE REAL-TIME WATCH first_refusal_after_ms={} refusals={refusals} latest_unexpired_ms={} promoted_after_ms={} promoted_after_runner_ms={promoted_after_ms} final_index=3 max_sampling_gap_ms={max_gap} stall_deferral={}",
        first_refusal_after.as_millis(),
        latest_unexpired.as_millis(),
        promoted_after.as_millis(),
        deferred_until.is_some()
    );
}

/// The standby device's own human seat on the original: its real player
/// endpoint (built on the standby's shared sender sequence), the original's
/// side of that player channel, and the original's verified queue for it.
struct OwnLink {
    endpoint: PlayerEndpoint,
    server: Channel,
    queue: PlayerQueue,
    /// Newest own-seat output sequence already framed to the endpoint.
    framed: u64,
    /// Newest output sequence the seat's signed `Applied` ACK covers.
    acked: u64,
    /// Applied own-seat outputs the original pruned when an ACK committed.
    pruned: usize,
}
/// What one own-seat frame did.
#[derive(Default)]
struct OwnStep {
    outputs_applied: usize,
    views: Vec<Vec<u8>>,
    intents_sent: usize,
    /// Signed `Applied` controls sent: one per frame that applied an output.
    acks_sent: usize,
    controls_accepted: usize,
}
/// Build the own seat's player endpoint and the original's queue for it.
/// Fray refuses a standby-key endpoint with its own counter; the endpoint
/// shares the standby's pair-link sender sequence.
fn own_link(
    descriptor: &Descriptor,
    standby: &mut migration::LiveStandby,
    live: &mut migration::LivePrimary,
    keys: &decented_net::Keys,
    initial: Hash,
) -> (OwnLink, bool) {
    let root = &descriptor.root;
    let player = root
        .members
        .iter()
        .find(|m| m.key == Some(root.standby))
        .unwrap()
        .player;
    // The original's side: the certificate it learned over the pair link.
    let original = original_cert(live, || {
        standby.poll().unwrap();
    });
    // The own seat's endpoint (client side) holds the standby's own.
    let cert = standby.membership_certificate().unwrap();
    let policy = EndpointPolicy {
        descriptor: descriptor.clone(),
        role: LinkRole::Original,
        promotion: None,
        membership: Some(cert.clone()),
    };
    let server_policy = EndpointPolicy {
        membership: Some(original.clone()),
        ..policy.clone()
    };
    let binding = ConnectionBinding {
        session: root.session,
        descriptor: root.digest().unwrap(),
        term: 0,
        client: LinkRole::Player,
        server: LinkRole::Original,
    };
    // A synthetic bound player channel pair; the pair link and both owned
    // runtimes are real QUIC.
    let tls = [0x3c; 32];
    let client = || {
        Channel::new_bound(
            &policy,
            binding.clone(),
            ChannelSide::Client,
            root.standby,
            root.host,
            tls,
        )
        .unwrap()
    };
    // The activation vote this player signed (deterministic signature).
    let capability = CapabilityAck::sign(root, player, initial, 1, keys).unwrap();
    let own_counter_refused = PlayerEndpoint::new(
        descriptor.clone(),
        standby.armed(),
        &capability,
        cert.clone(),
        client(),
        keys.clone(),
    )
    .err()
        == Some(Error::Invalid("authority player must share its sender"));
    let endpoint = PlayerEndpoint::new_with_sender(
        descriptor.clone(),
        standby.armed(),
        &capability,
        cert.clone(),
        client(),
        keys.clone(),
        standby.sender_sequence(),
    )
    .unwrap();
    let server = Channel::new(
        &server_policy,
        binding,
        ChannelSide::Server,
        root.standby,
        tls,
    )
    .unwrap();
    let queue = live.attach_player(&server, original).unwrap();
    (
        OwnLink {
            endpoint,
            server,
            queue,
            framed: 0,
            acked: 0,
            pruned: 0,
        },
        own_counter_refused,
    )
}
/// Accelerate straight ahead: the own seat's real controls.
fn own_input() -> Input {
    Input {
        steer: 0.0,
        throttle: 1.0,
        ..Input::default()
    }
}
/// One frame of the own seat at local time `now` (its clock offset was
/// unsynced at activation, so local time is host time). Its committed reliable
/// outputs, except the newest `lag`, reach its endpoint over the player
/// channel and are applied through the client's transactional step, as the
/// endpoint's receive path requires. The client then runs its frame. Its input
/// datagrams go to the original as signed lossy views and, unless withheld,
/// its journaled reliable intents as signed intents. As the seat's route does
/// (`RemotePlayer::frame_with`), a frame that applied outputs then queues one
/// signed cumulative `Applied` ACK, so each applied output is acknowledged by
/// exactly one control. All are allocated from the shared sender sequence and
/// every control must be accepted by the original.
fn own_step(
    own: &mut OwnLink,
    client: &mut ClientSession,
    live: &migration::LivePrimary,
    now: u64,
    wall: u64,
    send_intents: bool,
    lag: usize,
) -> OwnStep {
    let context = live.root().context().unwrap();
    let player = own.queue.player();
    let mut step = OwnStep::default();
    let mut fresh = live
        .primary()
        .outbox()
        .filter(|r| {
            r.output.recipient == player
                && r.output.delivery == Delivery::Reliable
                && r.output.sequence > own.endpoint.journals().0.applied()
        })
        .cloned()
        .collect::<Vec<_>>();
    fresh.sort_by_key(|r| r.output.sequence);
    fresh.truncate(fresh.len().saturating_sub(lag));
    for record in fresh {
        if record.output.sequence > own.framed {
            own.server
                .enqueue(PacketKind::Output, &record.encode(&context).unwrap())
                .unwrap();
            own.framed = record.output.sequence;
        }
    }
    let mut views = Vec::new();
    while let Some(wire) = own.server.poll_send(wall).unwrap() {
        let event = own
            .endpoint
            .receive(&wire, wall, |delivery, bytes, intents| {
                assert_eq!(delivery, Delivery::Reliable);
                let batch = client.migration_step(
                    intents,
                    now,
                    own_input(),
                    &[ClientNetEvent::Reliable(bytes.to_vec())],
                    None,
                    |_| Ok(()),
                )?;
                views.extend(batch.lossy);
                Ok(())
            })
            .unwrap();
        if let Some(PlayerEvent::Output { applied: true, .. }) = event {
            step.outputs_applied += 1;
        }
    }
    let batch = client
        .migration_step(
            own.endpoint.intents_mut().unwrap(),
            now,
            own_input(),
            &[],
            None,
            |_| Ok(()),
        )
        .unwrap();
    views.extend(batch.lossy);
    for view in &views {
        own.endpoint.queue_view(view.clone()).unwrap();
    }
    if send_intents {
        loop {
            let queued = own.endpoint.queue_pending(MAX_BATCH).unwrap();
            if queued == 0 {
                break;
            }
            step.intents_sent += queued;
        }
    }
    let applied = own.endpoint.journals().0.applied();
    if applied > own.acked {
        // The delivery journal is contiguous: this ACK newly covers exactly
        // the outputs this frame applied, none twice and none skipped.
        assert_eq!(
            applied,
            own.acked + step.outputs_applied as u64,
            "own-seat ACK does not cover exactly this frame's outputs"
        );
        own.endpoint.queue_applied().unwrap();
        own.acked = applied;
        step.acks_sent += 1;
    } else {
        assert_eq!(
            step.outputs_applied, 0,
            "applied outputs left unacknowledged"
        );
    }
    while let Some(wire) = own.endpoint.poll_send(wall).unwrap() {
        let Some((kind, _, bytes)) = own.server.receive(&wire, wall).unwrap() else {
            continue;
        };
        assert_eq!(kind, PacketKind::Control);
        match own.queue.receive(&bytes, live.primary()) {
            Ok(true) => step.controls_accepted += 1,
            other => panic!("own-seat control refused by the original: {other:?}"),
        }
    }
    // The seat's ACK is its delivery journal. That the original's queue took
    // it is checked when the batch staging it commits ([`own_ack_committed`]),
    // after the outbox prune, and at C.
    assert_eq!(own.acked, applied);
    step.views = views;
    step
}
/// The own seat's reliable outputs the original still retains that its
/// sent `Applied` ACK covers, when a batch is about to be staged: that commit
/// must prune them. The ACK is the seat's delivery journal (each frame ACKs
/// what it applied before the batch is staged).
fn own_ack_staged(own: &OwnLink, live: &migration::LivePrimary) -> (u64, usize) {
    let ack = own.acked;
    assert_eq!(ack, own.endpoint.journals().0.applied());
    (ack, own_acked_retained(own, live, ack))
}
fn own_acked_retained(own: &OwnLink, live: &migration::LivePrimary, ack: u64) -> usize {
    let player = own.queue.player();
    live.primary()
        .outbox()
        .filter(|r| {
            r.output.recipient == player
                && r.output.delivery == Delivery::Reliable
                && r.output.sequence <= ack
        })
        .count()
}
/// After the batch staged with `staged` (from [`own_ack_staged`]) committed:
/// its ACK committed, every output it covers left the outbox, and the
/// committed watermark, the queued ACK and the delivery journal agree.
fn own_ack_committed(own: &mut OwnLink, live: &migration::LivePrimary, staged: (u64, usize)) {
    let (ack, prunable) = staged;
    own.queue.refresh(live.primary()).unwrap();
    let stale = own_acked_retained(own, live, ack);
    assert_eq!(
        stale,
        0,
        "Applied ACKs did not prune the outbox: {stale} applied own-seat outputs retained after commit {}",
        live.primary().receipt().index
    );
    let mark = live
        .primary()
        .receipt()
        .watermarks
        .iter()
        .find(|m| m.player == own.queue.player())
        .unwrap()
        .output_ack;
    assert_eq!(mark, ack, "the own seat's queued ACK did not commit");
    assert_eq!(own.queue.output_ack(), mark);
    assert_eq!(own.endpoint.journals().0.applied(), mark);
    own.pruned += prunable;
}
/// Stage `index` with the own seat's queue and drive both owned runtimes until
/// the original publishes it. Returns whether the standby had installed that
/// receipt first, and the lossy views the batch selected.
fn commit_own(
    live: &mut migration::LivePrimary,
    standby: &mut migration::LiveStandby,
    queue: &mut PlayerQueue,
    index: u64,
    now: u64,
) -> (bool, Vec<(u64, u64)>) {
    queue.refresh(live.primary()).unwrap();
    let before = migration::checkpoint(live.authority()).unwrap();
    assert!(live
        .stage(now, &[&*queue], &BTreeMap::new(), |h| {
            if index == 1 && !h.start_race(now) {
                return Err(Error::Invalid("race start"));
            }
            Ok(())
        })
        .unwrap());
    let started = Instant::now();
    let mut installed: Option<Receipt> = None;
    loop {
        for event in live.poll().unwrap() {
            match event {
                migration::RuntimeEvent::Committed { selected_views, .. } => {
                    assert_eq!(live.primary().receipt().index, index);
                    queue.refresh(live.primary()).unwrap();
                    for (player, number) in &selected_views {
                        if *player == queue.player() {
                            queue.discard_lossy(*number);
                        }
                    }
                    let first = installed.as_ref().map(|r| r.index) == Some(index);
                    // Its certificate trails on a later frame.
                    original_cert(live, || {
                        standby.poll().unwrap();
                    });
                    return (first, selected_views);
                }
                migration::RuntimeEvent::Promoted(_) => panic!("unexpected promotion"),
                migration::RuntimeEvent::Membership(_) => {}
            }
        }
        assert_eq!(
            migration::checkpoint(live.authority()).unwrap(),
            before,
            "native authority adopted before its receipt"
        );
        for receipt in standby.poll().unwrap() {
            assert_eq!(receipt.index, index);
            installed = Some(receipt);
        }
        bounded(started);
    }
}
fn ping_of(bytes: &[u8]) -> Option<(u32, u64)> {
    match race_session::proto::decode::<race_session::proto::ToHost>(bytes) {
        Some(race_session::proto::ToHost::Ping { id, client_t }) => Some((id, client_t)),
        _ => None,
    }
}
fn pong_of(bytes: &[u8]) -> Option<(u32, u64, u64)> {
    match race_session::proto::decode::<race_session::proto::ToClient>(bytes) {
        Some(race_session::proto::ToClient::Pong {
            id,
            client_t,
            host_t,
        }) => Some((id, client_t, host_t)),
        _ => None,
    }
}

/// Where the own-seat takeover goes after promotion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnClose {
    /// Every unhandled own-seat intent is supplied and drains once.
    Drain,
    /// The seat's first unhandled intent is never supplied.
    MissingIntent,
    /// The own seat's player quits during the race: its `ToHost::Leave` is
    /// journaled after C, unhandled, and drains as its last intent. The other
    /// human stays connected as a remote survivor.
    Leave,
    /// The same Leave, but the other human is absent: after its departure
    /// Fray re-enters Replay, and no human is left in the (headless) room.
    LeaveOtherAbsent,
}

// The standby device's own human seat plays the race through its real client
// and player endpoint, its controls signed from the pair link's shared sender
// sequence. It stops sending its reliable intents once its inputs are flowing;
// C is the first commit after which it holds two withheld clock pings. After
// promotion it recovers as a local survivor and its in-flight pings drain
// exactly once: one native Pong each, at frozen time C.
#[test]
fn owned_standby_drains_own_human_seat_inflight_intent_once() {
    own_seat_takeover(OwnClose::Drain);
}
// The same takeover, but the own seat's first unhandled intent is genuinely
// missing: sealed but never supplied. Recovery stays paused at the hole, never
// skips it, runs no departure and keeps the seat, and fails with the spec's
// reason at the 5000 ms recovery deadline.
#[test]
fn owned_standby_missing_own_seat_intent_stays_paused_past_recovery_deadline() {
    own_seat_takeover(OwnClose::MissingIntent);
}
// The standby device's own player leaves during the race; its `Leave` is in
// flight at C. Recovery replays it exactly once as a normal seat departure: a
// bot takes over its racer, the other human stays, the seat is not reclaimed,
// and the promoted device reaches Ready and keeps hosting without a seat.
#[test]
fn owned_standby_honours_own_seat_leave_in_flight_at_takeover() {
    own_seat_takeover(OwnClose::Leave);
}
// The same Leave with the other human absent. After the Leave drains, the
// other human's departure sends Fray back to Replay; the driver keeps calling
// the retired seat, whose calls are no-op `Ok`s, and recovery reaches Ready.
// No human is left: the headless room keeps racing with bots.
#[test]
fn owned_standby_own_seat_leave_with_absent_human_reenters_replay_and_empties_room() {
    own_seat_takeover(OwnClose::LeaveOtherAbsent);
}

/// The other human as a remote recovery survivor: it answers the resume
/// challenge with its signed cut and seals each input round over a synthetic
/// bound connection. It applies (and acknowledges) every committed output to
/// it; it has no intents in flight.
struct RemoteSurvivor {
    player: u64,
    identity: Identity,
    tls: [u8; 32],
    delivery: DeliveryJournal,
    intents: IntentJournal,
    control: u64,
}
impl RemoteSurvivor {
    fn resume(&mut self, recovery: &mut Recovery, key: &decented_net::Keys, ms: u64) {
        let challenge = recovery
            .challenge(
                7,
                self.tls,
                self.player,
                self.identity.pubkey().to_bytes(),
                key,
                ms,
            )
            .unwrap();
        self.control += 1;
        let proof = ResumeProof::sign(
            &recovery.root().clone(),
            &challenge,
            self.tls,
            self.delivery.applied(),
            self.intents.tail(),
            self.control,
            self.identity.keys(),
        )
        .unwrap();
        recovery.accept(7, self.tls, &proof, ms).unwrap();
    }
    fn deliver(&mut self, recovery: &mut Recovery, key: &decented_net::Keys) {
        let context = recovery.root().context().unwrap();
        for record in recovery.replay().unwrap() {
            if record.output.recipient == self.player {
                self.delivery
                    .apply_certified(&context, self.player, &record, |_| Ok(()))
                    .unwrap();
            }
        }
        recovery
            .acknowledge_outputs(self.player, self.delivery.applied())
            .unwrap();
        recovery.commit_delivery_acks(key).unwrap();
    }
    fn seal(&mut self, recovery: &mut Recovery, key: &decented_net::Keys, ms: u64) {
        let round = recovery
            .issue_input_round(self.player, self.tls, key, ms)
            .unwrap();
        self.control += 1;
        let cut = PlayerControl::seal_round(
            &recovery.root().clone(),
            recovery.promotion(),
            &round,
            self.tls,
            &self.delivery,
            &self.intents,
            self.control,
            self.identity.keys(),
        )
        .unwrap();
        recovery.accept_input_cut(&cut, self.tls, ms).unwrap();
    }
}
/// The own client's frame state that a frozen recovery must not move.
fn frame_state(client: &ClientSession) -> String {
    let view = client.view().map(|v| {
        format!(
            "{:?} {:?} {:?} {:?} {:?}",
            v.world, v.hud, v.standings, v.correction, v.over
        )
    });
    let newest = client.last_ack().unwrap_or(0) + 1;
    format!(
        "{view:?} {:?} {:?} {:?} {:?}",
        client.stats(),
        client.last_ack(),
        client.live_predicted_pos(newest),
        client.phase()
    )
}

fn own_seat_takeover(close: OwnClose) {
    let missing = close == OwnClose::MissingIntent;
    let leaving = matches!(close, OwnClose::Leave | OwnClose::LeaveOtherAbsent);
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let root = descriptor.root.clone();
    let wall = || clock.now_ms();
    let initial = state_hash(receiver.sequencer.image()).unwrap();
    let standby_id = lobby.identities[0].clone();
    let key = standby_id.keys().clone();
    let mut standby =
        migration::LiveStandby::new(descriptor.clone(), receiver, standby_id, clock).unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let (mut own, own_counter_refused) =
        own_link(&descriptor, &mut standby, &mut live, &key, initial);
    assert!(own_counter_refused);
    let other_identity = lobby.identities[1].clone();
    let [mut client, _other] = lobby.clients;
    let own_player = own.queue.player();
    let native_id = client.my_seat().unwrap();
    let base = SessionImage::decode(&root, live.primary().committed_image())
        .unwrap()
        .logical_ms;

    // 1. Normal play. The own seat's endpoint applies its committed outputs
    // two behind (so outputs are in flight at C), runs its client frame and
    // sends every control from the shared sender. Its intents (clock pings)
    // are sent until its input views have been committed for a while; then
    // they are withheld, so later pings are in flight at C.
    let mut index = 0u64;
    let mut racing_from = None;
    let mut withholding = false;
    let mut ack_after_install = true;
    let mut native_equal = true;
    let mut controls_accepted = 0;
    let mut intents_sent = 0;
    let mut views_sent = 0;
    let mut acks_sent = 0;
    let mut views_committed = 0;
    let mut outputs_applied = 0;
    let withheld = loop {
        index += 1;
        assert!(index < 600, "no withheld own-seat pings before commit 600");
        let now = base + index * STEP;
        let step = own_step(&mut own, &mut client, &live, now, wall(), !withholding, 2);
        controls_accepted += step.controls_accepted;
        intents_sent += step.intents_sent;
        views_sent += step.views.len();
        acks_sent += step.acks_sent;
        outputs_applied += step.outputs_applied;
        let restores = standby.restores();
        let staged = own_ack_staged(&own, &live);
        let (first, selected) = commit_own(&mut live, &mut standby, &mut own.queue, index, now);
        own_ack_committed(&mut own, &live, staged);
        ack_after_install &= first;
        assert!(
            ack_after_install,
            "receipt observed before standby installation"
        );
        assert!(standby.restores() > restores);
        views_committed += selected.iter().filter(|(p, _)| *p == own_player).count();
        assert_eq!(live.primary().committed_image(), standby.image());
        let restored = standby.committed_authority().unwrap();
        native_equal &= migration::checkpoint(&restored).unwrap()
            == migration::checkpoint(live.authority()).unwrap();
        assert!(
            native_equal,
            "standby native state differs at commit {index}"
        );
        assert!(live.failure().is_none() && standby.failure().is_none());
        if racing_from.is_none()
            && live
                .authority()
                .world()
                .is_some_and(|w| w.phase == RacePhase::Racing)
        {
            racing_from = Some(index);
        }
        if !withholding && racing_from.is_some_and(|r| index - r >= 20) && views_committed >= 10 {
            withholding = true;
        }
        if withholding {
            let handled = live
                .primary()
                .receipt()
                .watermarks
                .iter()
                .find(|m| m.player == own_player)
                .unwrap()
                .input;
            let pings = own
                .endpoint
                .journals()
                .1
                .pending()
                .filter(|(n, b)| *n > handled && ping_of(b).is_some())
                .map(|(n, b)| (n, b.to_vec()))
                .collect::<Vec<_>>();
            if pings.len() >= 2 {
                break pings;
            }
        }
    };
    let commits = index;
    let frontier = live.primary().receipt().clone();
    let own_mark = |receipt: &Receipt| {
        receipt
            .watermarks
            .iter()
            .find(|m| m.player == own_player)
            .unwrap()
            .clone()
    };
    let handled_at_c = own_mark(&frontier).input;
    // The tracked intent: the newest withheld ping. Its Pong was never
    // produced: the original never received it.
    let (tracked_seq, tracked_bytes) = withheld.last().cloned().unwrap();
    let (ping_id, ping_client_t) = ping_of(&tracked_bytes).unwrap();
    let pong_at_c = live.primary().outbox().any(|r| {
        r.output.recipient == own_player && pong_of(&r.output.bytes).is_some_and(|p| p.0 == ping_id)
    });
    assert!(!pong_at_c);
    assert!(handled_at_c < withheld[0].0);
    let output_in_flight = live
        .primary()
        .outbox()
        .filter(|r| {
            r.output.recipient == own_player
                && r.output.delivery == Delivery::Reliable
                && r.output.sequence > own.endpoint.journals().0.applied()
        })
        .count();
    assert!(output_in_flight >= 1, "no own-seat output in flight at C");
    let pending_at_c = own.endpoint.journals().1.pending().count();
    let world_at_c = live.authority().world().unwrap().clone();
    let own_racer = live.authority().racer_of(native_id).unwrap();
    let racer_at_c = world_at_c.racers[own_racer].clone();
    // Every control the own seat sent was accepted (`own_step` panics on any
    // refusal, which is how the earlier `Conflict` showed): its intents, its
    // views and one signed `Applied` ACK per frame that applied outputs.
    let every_accepted = controls_accepted == intents_sent + views_sent + acks_sent;
    assert!(every_accepted);
    // Its ACKs committed on the original and pruned what it applied: every
    // output applied before C's frame was pruned by exactly one committed ACK,
    // and C's watermark is the seat's delivery journal.
    let own_ack_at_c = own_mark(&frontier).output_ack;
    let own_applied_at_c = own.endpoint.journals().0.applied();
    assert!(
        own.pruned > 0 && own_ack_at_c > 0,
        "Applied ACKs did not prune the outbox: pruned={} committed_ack={own_ack_at_c}",
        own.pruned
    );
    assert_eq!(own.pruned, outputs_applied);
    assert_eq!(own_ack_at_c, own_applied_at_c);
    assert_eq!(own.queue.output_ack(), own_applied_at_c);
    if close == OwnClose::Drain {
        println!("STAMPEDE OWN SEAT APPLIED ACKS signed_applied_controls={acks_sent} outputs_acked_once={outputs_applied} committed_output_ack={own_ack_at_c} applied_at_c={own_applied_at_c} outbox_pruned={}", own.pruned);
        println!("STAMPEDE OWN SEAT IN FLIGHT AT C commits={commits} racing_batches={} native_equal_each_commit={native_equal} ack_after_install={ack_after_install} own_controls_accepted={controls_accepted} own_intents_sent={intents_sent} own_views_sent={views_sent} own_acks_sent={acks_sent} own_views_committed={views_committed} every_own_control_accepted={every_accepted} own_counter_refused={own_counter_refused} live_outputs_applied={outputs_applied} pending_intents={pending_at_c} handled_at_c={handled_at_c} withheld_pings={} tracked_ping_sequence={tracked_seq} pong_at_c={pong_at_c} outputs_in_flight={output_in_flight}", commits - racing_from.unwrap(), withheld.len());
    }

    // 2. The own seat's next frame produces input views for C+1. They reach
    // the original's queue, which stages them into C+1. The standby only
    // queues that candidate's frames before promoting, so the views are in
    // flight at C.
    let next = base + (commits + 1) * STEP;
    let step = own_step(&mut own, &mut client, &live, next, wall(), false, 2);
    assert!(!step.views.is_empty(), "no own input in flight at C");
    // The own player quits: its real leave path runs as one client
    // transaction. `ToHost::Leave` is journaled (never sent: intents are
    // withheld), so it is unhandled at C and is the seat's newest intent.
    let leave = leaving.then(|| {
        let tail = own.endpoint.journals().1.tail();
        let batch = client
            .migration_step(
                own.endpoint.intents_mut().unwrap(),
                next,
                own_input(),
                &[],
                None,
                |c| {
                    c.leave();
                    Ok(())
                },
            )
            .unwrap();
        assert!(batch.closed && batch.lossy.is_empty());
        assert_eq!(batch.reliable.len(), 1, "leave journaled one intent");
        let (sequence, bytes) = batch.reliable[0].clone();
        assert_eq!(sequence, tail + 1);
        assert_eq!(
            race_session::proto::decode::<race_session::proto::ToHost>(&bytes),
            Some(race_session::proto::ToHost::Leave)
        );
        assert_eq!(own.endpoint.journals().1.tail(), sequence);
        assert_eq!(
            *client.phase(),
            race_session::ClientPhase::Closed("left".into())
        );
        (sequence, bytes)
    });
    let in_flight_views = own.queue.lossy_views().count();
    let newest_in_flight_seq = step
        .views
        .iter()
        .filter_map(
            |v| match race_session::proto::decode::<race_session::proto::LossyToHost>(v) {
                Some(race_session::proto::LossyToHost::Inputs { seq, .. }) => Some(seq),
                _ => None,
            },
        )
        .max()
        .unwrap();
    let committed = standby.image().to_vec();
    standby.probe().unwrap();
    assert!(standby.poll().unwrap().is_empty());
    own.queue.refresh(live.primary()).unwrap();
    assert!(live
        .stage(next, &[&own.queue], &BTreeMap::new(), |_| Ok(()))
        .unwrap());
    let race = Instant::now();
    while standby.queued_bytes() == 0 {
        assert!(live.poll().unwrap().is_empty());
        standby.intake().unwrap();
        bounded(race);
    }
    let (mut promoted, _) = promote_after_watch(standby).unwrap();
    let promotion_ms = promoted.promoted_at_ms();
    assert_eq!(promoted.recovery.promotion().final_receipt, frontier);
    assert_eq!(promoted.recovery.sequencer().image(), committed.as_slice());
    loop {
        promoted.poll(wall()).unwrap();
        if live
            .poll()
            .unwrap()
            .iter()
            .any(|e| matches!(e, migration::RuntimeEvent::Promoted(_)))
        {
            break;
        }
        bounded(race);
    }
    assert!(live.primary().is_closed());
    assert!(!live.has_pending());
    let link = promoted.into_link();

    // 3. Recovery at frozen C. The own seat is a local survivor built from its
    // actual client and its endpoint's journals. The other human is absent,
    // except in the Leave run, where it resumes as a remote survivor.
    let mut recovery = link.recovery;
    let frame_at_c = frame_state(&client);
    let clock_at_c = (client.clock_offset_ms(), client.rtt_ms());
    // The whole clock-sync state, including the open round's samples.
    let sync_at_c = format!("{:?}", client.clock_sync());
    assert!(clock_at_c.0.is_some() && clock_at_c.1.is_some());
    let (delivery, intents) = own.endpoint.journals();
    let delivery = DeliveryJournal::restore(&delivery.checkpoint().unwrap()).unwrap();
    let intents = IntentJournal::restore(&intents.checkpoint().unwrap()).unwrap();
    drop(own);
    let mut seat = migration::OwnSeat::new(
        &root,
        recovery.sequencer().image(),
        client,
        delivery,
        intents,
    )
    .unwrap();
    let mut rec_ms = promotion_ms + 250;
    // The device's real local time runs on through recovery, more than one
    // clock-sync period past its last play frame, on its own clock; the
    // recovery deadline runs on the separate synthetic recovery clock. The
    // frozen recovery step runs no frame, so the client produces no datagram,
    // ping or prediction, whatever its local time.
    let advance = race_session::clock::SYNC_EVERY_MS + 100;
    let own_local = |rec_ms: u64| next + advance + (rec_ms - promotion_ms);
    seat.retain(&mut recovery, promotion_ms).unwrap();
    let other_player = root
        .members
        .iter()
        .find(|m| m.key == Some(other_identity.pubkey().to_bytes()))
        .unwrap()
        .player;
    let mut other = (close == OwnClose::Leave).then(|| {
        let mut other = RemoteSurvivor {
            player: other_player,
            identity: other_identity.clone(),
            tls: [0x5d; 32],
            delivery: DeliveryJournal::default(),
            intents: IntentJournal::default(),
            // Its capability ACK was its control 1.
            control: 1,
        };
        other.resume(&mut recovery, &key, promotion_ms);
        other
    });
    let own_retired = |recovery: &Recovery| {
        recovery
            .sequencer()
            .receipt()
            .watermarks
            .iter()
            .find(|m| m.player == own_player)
            .is_none_or(|m| m.retired)
    };
    let tail_at_c = seat.journals().1.tail();
    let retained = seat
        .journals()
        .1
        .pending()
        .any(|(n, b)| n == tracked_seq && b == tracked_bytes.as_slice());
    assert!(retained);
    // Sample every frame: a clock jump would count as a local stall.
    for now in (promotion_ms + 50..rec_ms).step_by(50) {
        recovery.poll(now).unwrap();
    }
    assert!(recovery.seal(rec_ms).unwrap());
    migration::recover_security(&mut recovery, &key, rec_ms).unwrap();
    assert_eq!(recovery.phase(), RecoveryPhase::Replay);
    let authority = |recovery: &Recovery| {
        migration::committed_authority(&root, recovery.sequencer().image()).unwrap()
    };

    // 3a. A genuine delivery gap stays paused: without the seat's first
    // committed output it has not yet applied, nothing is applied or
    // acknowledged, and no input round can begin.
    let records = recovery.replay().unwrap();
    let applied_at_c = seat.journals().0.applied();
    let unapplied = |r: &&CertifiedOutput| {
        r.output.recipient == own_player
            && r.output.delivery == Delivery::Reliable
            && r.output.sequence > applied_at_c
    };
    let replay_records = records.iter().filter(unapplied).count();
    assert!(replay_records > 0, "no own-seat output in flight at C");
    let first = records.iter().position(|r| unapplied(&r)).unwrap();
    let mut gapped = records.clone();
    gapped.remove(first);
    let before_gap = recovery.sequencer().receipt().clone();
    let gap = seat
        .replay(&mut recovery, &key, own_local(rec_ms), &gapped)
        .err();
    let gap_paused = gap == Some(Error::Gap)
        && seat.journals().0.applied() == applied_at_c
        && recovery.sequencer().receipt() == &before_gap
        && recovery.begin_round() == Err(Error::Gap)
        && recovery.phase() == RecoveryPhase::Replay;
    assert!(gap_paused, "own-seat delivery gap did not pause: {gap:?}");

    // 3b. Drive recovery. Every replay is delivered twice and every intent is
    // supplied twice; the second delivery must change nothing. Every own
    // reliable record seen is kept by its output sequence, so a duplicated or
    // lost Pong is visible.
    let mut seen: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut deliver_twice = |seat: &mut migration::OwnSeat, recovery: &mut Recovery, ms: u64| {
        let records = recovery.replay().unwrap();
        for r in &records {
            if r.output.recipient == own_player && r.output.delivery == Delivery::Reliable {
                let old = seen.insert(r.output.sequence, r.output.bytes.clone());
                assert!(old.is_none_or(|b| b == r.output.bytes));
            }
        }
        let first = seat
            .replay(recovery, &key, own_local(ms), &records)
            .unwrap();
        // Frozen: no new journaled intent and no datagram from the client.
        assert_eq!(
            seat.journals().1.tail(),
            tail_at_c,
            "own client journaled an intent during recovery"
        );
        assert_eq!(first.lossy_discarded, 0, "own client sent a datagram");
        let receipt = recovery.sequencer().receipt().clone();
        let again = seat
            .replay(recovery, &key, own_local(ms), &records)
            .unwrap();
        let noop = again.applied == 0
            && again.receipt.is_none()
            && recovery.sequencer().receipt() == &receipt;
        assert!(noop, "duplicate own-seat replay changed state");
        assert_eq!(first.retired, seat.retired(recovery));
        assert_eq!(again.retired, first.retired);
        (first.applied, first.lossy_discarded, noop, first.retired)
    };
    let hole = missing.then(|| {
        seat.journals()
            .1
            .pending()
            .find(|(n, _)| *n > handled_at_c)
            .unwrap()
            .0
    });
    if let Some(hole) = hole {
        assert!(hole < tracked_seq, "the hole must precede the tracked ping");
    }
    let mut duplicates_noop = true;
    let mut own_applied = 0;
    let mut lossy_discarded = 0;
    let mut rounds = 0;
    let mut drains = 0;
    let mut own_drains = 0;
    let mut tracked_drains = 0;
    let mut drained_pings = Vec::new();
    let mut departures_ran = false;
    // The Leave run: what the drain of the own seat's Leave did.
    let mut leave_drains = 0;
    let mut leave_departure = None;
    let mut next_after_leave = None;
    // Calls on the retired seat that returned a no-op `Ok`, and whether one
    // came in a Replay phase Fray re-entered after other departures.
    let mut retired_noops = 0;
    let mut retired_replay_phase = false;
    while recovery.phase() != RecoveryPhase::Ready {
        rounds += 1;
        assert!(rounds < 400, "recovery did not reach Ready");
        rec_ms += 1;
        match recovery.phase() {
            RecoveryPhase::Replay => {
                // After its own Leave the seat is retired; the driver keeps
                // calling it here when Fray returns to Replay after other
                // departures, and the call is a no-op `Ok`.
                let (applied, discarded, noop, retired) =
                    deliver_twice(&mut seat, &mut recovery, rec_ms);
                own_applied += applied;
                lossy_discarded += discarded;
                duplicates_noop &= noop;
                if retired {
                    retired_noops += 1;
                    retired_replay_phase |= departures_ran;
                }
                if let Some(other) = other.as_mut() {
                    other.deliver(&mut recovery, &key);
                }
                recovery.begin_round().unwrap();
            }
            RecoveryPhase::AwaitCuts => {
                if let Some(hole) = hole {
                    // The whole tail is sealed, but the record at the hole is
                    // genuinely missing: it is never supplied.
                    let handled = recovery.histories()[&own_player].handled();
                    recovery
                        .seal_inputs(own_player, seat.journals().1.tail())
                        .unwrap();
                    for (n, b) in seat.journals().1.pending() {
                        if n > handled && n != hole {
                            recovery.supply(own_player, n, b.to_vec()).unwrap();
                        }
                    }
                } else {
                    // A retired seat's seal is a no-op `Ok`.
                    let retired = seat.retired(&recovery);
                    seat.seal(&mut recovery).unwrap();
                    if retired {
                        retired_noops += 1;
                    } else {
                        for (n, b) in seat.journals().1.pending() {
                            recovery.supply(own_player, n, b.to_vec()).unwrap();
                        }
                    }
                }
                if let Some(other) = other.as_mut() {
                    other.seal(&mut recovery, &key, rec_ms);
                }
            }
            RecoveryPhase::Draining => {
                let next = match recovery.next_input() {
                    Err(Error::Gap) if hole.is_some() => break,
                    next => next.unwrap().map(|(p, n, b)| (p, n, b.to_vec())),
                };
                if let Some((player, sequence, bytes)) = next {
                    let before = authority(&recovery);
                    migration::recover_next(&mut recovery, &key, rec_ms).unwrap();
                    drains += 1;
                    if player == own_player {
                        own_drains += 1;
                        if let Some(ping) = ping_of(&bytes) {
                            drained_pings.push(ping);
                        }
                        if sequence == tracked_seq {
                            assert_eq!(bytes, tracked_bytes, "own-seat intent bytes changed");
                            tracked_drains += 1;
                        }
                    }
                    let is_leave =
                        player == own_player && leave.as_ref().is_some_and(|(n, _)| *n == sequence);
                    if is_leave {
                        assert_eq!(Some(&bytes), leave.as_ref().map(|(_, b)| b));
                        leave_drains += 1;
                        // The native host ran its departure for the seat at
                        // this drain: a bot took over its racer, one human
                        // left, and Fray retired the seat's watermark.
                        let after = authority(&recovery);
                        let racers = |h: &HostAuthority| h.world().unwrap().cfg.racers.clone();
                        leave_departure = Some((
                            before.human_count(),
                            after.human_count(),
                            racers(&before).len() == racers(&after).len(),
                            matches!(racers(&before)[own_racer].kind, RacerKind::Human)
                                && matches!(racers(&after)[own_racer].kind, RacerKind::Bot(_)),
                            before.racer_of(native_id) == Some(own_racer)
                                && after.racer_of(native_id).is_none(),
                            own_retired(&recovery),
                        ));
                        assert!(
                            own_retired(&recovery)
                                && after.human_count() + 1 == before.human_count(),
                            "own-seat Leave not honoured at its drain: humans {} -> {}, retired={}",
                            before.human_count(),
                            after.human_count(),
                            own_retired(&recovery)
                        );
                    } else {
                        assert!(
                            !own_retired(&recovery),
                            "own seat departed without its Leave"
                        );
                    }
                    let committed = recovery.sequencer().receipt().clone();
                    let image = recovery.sequencer().image().to_vec();
                    let resupplied = recovery.supply(player, sequence, bytes);
                    let noop = recovery.sequencer().receipt() == &committed
                        && recovery.sequencer().image() == image.as_slice();
                    // After its own departure the seat is no survivor: Fray
                    // refuses the duplicate outright, and nothing changes.
                    if is_leave {
                        assert_eq!(resupplied, Err(Error::Unauthorized));
                        // Not reclaimed: a new intent after the Leave (a real
                        // ping payload) is refused for the same reason.
                        let refused = recovery.supply(player, sequence + 1, tracked_bytes.clone());
                        assert_eq!(recovery.sequencer().receipt(), &committed);
                        next_after_leave = Some(refused);
                    } else {
                        resupplied.unwrap();
                    }
                    assert!(noop, "re-supplied intent changed state");
                    duplicates_noop &= noop;
                    let (applied, discarded, noop, retired) =
                        deliver_twice(&mut seat, &mut recovery, rec_ms);
                    own_applied += applied;
                    lossy_discarded += discarded;
                    duplicates_noop &= noop;
                    retired_noops += usize::from(retired);
                    if let Some(other) = other.as_mut() {
                        other.deliver(&mut recovery, &key);
                    }
                } else {
                    recovery.begin_round().unwrap();
                }
            }
            RecoveryPhase::Departures => {
                departures_ran = true;
                assert!(hole.is_none(), "departures ran past a missing intent");
                migration::recover_departures(&mut recovery, &key, rec_ms).unwrap();
            }
            phase => panic!("unexpected recovery phase {phase:?}"),
        }
        assert_eq!(
            SessionImage::decode(&root, recovery.sequencer().image())
                .unwrap()
                .logical_ms,
            frontier.logical_ms,
            "recovery moved game time"
        );
    }
    let pongs_for = |seen: &BTreeMap<u64, Vec<u8>>, id: u32| {
        seen.values()
            .filter_map(|b| pong_of(b))
            .filter(|p| p.0 == id)
            .collect::<Vec<_>>()
    };
    let world = |recovery: &Recovery| authority(recovery).world().unwrap().clone();
    // The frozen recovery step: the device's local time ran on by more than a
    // clock-sync period and replayed Pongs were applied, yet the client sent
    // no datagram and journaled no intent (checked after every replay), its
    // clock offset and RTT did not move, and its prediction and view are
    // exactly as at C.
    let replayed_pongs = seen
        .iter()
        .filter(|(n, b)| **n > applied_at_c && pong_of(b).is_some())
        .count();
    let local_advance = own_local(rec_ms) - next;
    let clock_now = (seat.client().clock_offset_ms(), seat.client().rtt_ms());
    let sync_now = format!("{:?}", seat.client().clock_sync());
    assert_eq!(
        sync_now, sync_at_c,
        "a replayed Pong fed the client's clock-sync round"
    );
    let clock_unchanged = clock_now == clock_at_c;
    assert!(
        clock_unchanged,
        "recovery moved the client clock: {clock_at_c:?} -> {clock_now:?}"
    );
    let frame_unchanged = frame_state(seat.client()) == frame_at_c;
    assert!(frame_unchanged, "recovery ran a client frame");
    let tail_unchanged = seat.journals().1.tail() == tail_at_c;
    assert!(tail_unchanged);
    assert!(local_advance > race_session::clock::SYNC_EVERY_MS);
    assert!(rec_ms < promotion_ms + 5000);
    if !missing {
        assert!(replayed_pongs >= 1, "no replayed Pong reached the client");
    }
    if close == OwnClose::Drain {
        println!("STAMPEDE OWN SEAT FROZEN RECOVERY local_advance_ms={local_advance} recovery_clock_ms={} replayed_pongs={replayed_pongs} datagrams={lossy_discarded} new_intents={} clock_offset_unchanged={} rtt_unchanged={} clock_sync_state_unchanged={} prediction_and_view_unchanged={frame_unchanged} sealed_tail_is_cut_at_c={tail_unchanged}", rec_ms - promotion_ms, seat.journals().1.tail() - tail_at_c, clock_now.0 == clock_at_c.0, clock_now.1 == clock_at_c.1, sync_now == sync_at_c);
    }
    if let Some(hole) = hole {
        missing_own_intent(MissingOwnIntent {
            recovery,
            seat: &seat,
            root: &root,
            key: &key,
            hole,
            frontier: &frontier,
            promotion_ms,
            rec_ms,
            drains,
            departures_ran,
            tracked_pongs: pongs_for(&seen, ping_id).len(),
        });
        return;
    }
    // Exactly once for the own seat's in-flight intents: the tracked ping
    // drained once with its original bytes and the committed history handled
    // it; the native host answered it with exactly one Pong, stamped at frozen
    // time C and echoing the ping's client time. Every drained ping has
    // exactly one Pong. A second application would add a second Pong; a lost
    // one would leave none.
    let tracked_pongs = pongs_for(&seen, ping_id);
    assert_eq!(
        tracked_pongs.len(),
        1,
        "own-seat intent answered {} times",
        tracked_pongs.len()
    );
    assert_eq!(
        tracked_pongs[0],
        (ping_id, ping_client_t, frontier.logical_ms)
    );
    assert_eq!(
        tracked_drains, 1,
        "own-seat intent drained {tracked_drains} times"
    );
    let every_ping_once = drained_pings
        .iter()
        .all(|(id, _)| pongs_for(&seen, *id).len() == 1);
    assert!(
        every_ping_once,
        "a drained ping was not answered exactly once"
    );
    if let Some((leave_seq, _)) = leave {
        own_leave(OwnLeave {
            recovery,
            seat,
            root: &root,
            own_player,
            other_player,
            other_survives: other.is_some(),
            native_id,
            own_racer,
            racers_at_c: world_at_c.racers.len(),
            frontier: &frontier,
            leave_seq,
            leave_drains,
            leave_departure,
            next_after_leave,
            retired_noops,
            retired_replay_phase,
            departures_ran,
            pings_once: tracked_drains == 1 && tracked_pongs.len() == 1 && every_ping_once,
            duplicates_noop,
            sealed_tail_is_cut: tail_unchanged,
        });
        return;
    }
    let own_handled = recovery.histories()[&own_player].handled();
    assert!(own_handled >= tracked_seq);
    let final_receipt = recovery.sequencer().receipt().clone();
    let mark = own_mark(&final_receipt);
    let own_delivered = seat.journals().0.applied() == mark.output_ack && own_applied > 0;
    assert!(own_delivered);
    // Native world at Ready: no tick ran and the own racer is exactly as
    // committed at C. The input views in flight at C were not applied by
    // recovery (Fray has no lossy recovery path); the client's predictions
    // over them are uncommitted.
    let finished = world(&recovery);
    let world_tick_unchanged = finished.tick == world_at_c.tick;
    let own_racer_unchanged = finished.racers[own_racer] == racer_at_c;
    assert!(world_tick_unchanged && own_racer_unchanged);
    let restored = authority(&recovery);
    // The absent other human departed and a bot took over its racer.
    let bot_takeover = departures_ran
        && restored.human_count() == 1
        && finished.racers.len() == world_at_c.racers.len();
    assert!(bot_takeover);
    let sealed_tail_is_cut = seat.journals().1.tail() == tail_at_c;
    assert!(sealed_tail_is_cut);
    let seat_kept = restored.racer_of(native_id) == Some(own_racer) && !mark.retired;
    assert!(seat_kept);
    let frozen_at_c = SessionImage::decode(&root, recovery.sequencer().image())
        .unwrap()
        .logical_ms
        == frontier.logical_ms;
    assert!(frozen_at_c);
    let own_seat_exactly_once = tracked_drains == 1
        && tracked_pongs.len() == 1
        && every_ping_once
        && own_handled >= tracked_seq;
    assert!(own_seat_exactly_once);
    println!("STAMPEDE OWN SEAT INTENT DRAINED intent=Ping seat=own_human retained_after_close={retained} gap_paused={gap_paused} own_seat_replay_records={replay_records} own_seat_applied={own_applied} own_seat_drains={own_drains} tracked_ping_drains={tracked_drains} tracked_ping_pongs={} pong_host_t_is_c={} every_drained_ping_one_pong={every_ping_once} own_handled={own_handled} sealed_tail_is_cut_at_c={sealed_tail_is_cut} drains={drains} bot_takeover_of_absent_human={bot_takeover} duplicate_deliveries_noop={duplicates_noop} own_seat_exactly_once={own_seat_exactly_once} own_seat_delivered={own_delivered} seat_kept={seat_kept} recovery_frozen_at_c={frozen_at_c} phase={:?}", tracked_pongs.len(), tracked_pongs[0].2 == frontier.logical_ms, recovery.phase());
    println!("STAMPEDE OWN SEAT LOSSY AT C in_flight_views={in_flight_views} newest_in_flight_input={newest_in_flight_seq} world_tick_unchanged={world_tick_unchanged} own_racer_unchanged={own_racer_unchanged} recovery_datagrams_discarded={lossy_discarded}");
}

struct OwnLeave<'a> {
    recovery: Recovery,
    seat: migration::OwnSeat,
    root: &'a Root,
    own_player: u64,
    other_player: u64,
    /// The other human resumed as a remote survivor (else it was absent).
    other_survives: bool,
    native_id: u8,
    own_racer: usize,
    racers_at_c: usize,
    frontier: &'a Receipt,
    leave_seq: u64,
    leave_drains: usize,
    /// At the Leave's drain: humans before and after, racer count kept, a bot
    /// took over the own racer, the seat lost its racer, watermark retired.
    leave_departure: Option<(usize, usize, bool, bool, bool, bool)>,
    /// Fray's answer to a new own-seat intent supplied after the Leave.
    next_after_leave: Option<Result<()>>,
    retired_noops: usize,
    retired_replay_phase: bool,
    departures_ran: bool,
    pings_once: bool,
    duplicates_noop: bool,
    sealed_tail_is_cut: bool,
}
/// The own seat's in-flight `Leave` was honoured exactly once as a normal seat
/// departure, the seat is not reclaimed, the retired seat's calls are no-op
/// `Ok`s, and the promoted device keeps hosting at Ready: its native host
/// ticks on, with the other human (survivor run) or with bots only in a
/// headless room left with no humans (absent run).
fn own_leave(m: OwnLeave<'_>) {
    let OwnLeave {
        mut recovery,
        mut seat,
        root,
        own_player,
        other_player,
        other_survives,
        native_id,
        own_racer,
        racers_at_c,
        frontier,
        leave_seq,
        leave_drains,
        leave_departure,
        next_after_leave,
        retired_noops,
        retired_replay_phase,
        departures_ran,
        pings_once,
        duplicates_noop,
        sealed_tail_is_cut,
    } = m;
    assert_eq!(recovery.phase(), RecoveryPhase::Ready);
    assert_eq!(
        leave_drains, 1,
        "own-seat Leave drained {leave_drains} times"
    );
    let (humans_before, humans_after, racers_kept, bot_took_over, racer_released, retired) =
        leave_departure.unwrap();
    assert_eq!(
        (humans_before, humans_after),
        (2, 1),
        "own-seat Leave did not remove exactly one human"
    );
    assert!(racers_kept && bot_took_over && racer_released && retired);
    // Not reclaimed: after the Leave, Fray refuses a new intent of the seat
    // because it is no longer a survivor.
    assert_eq!(next_after_leave, Some(Err(Error::Unauthorized)));
    // The retired seat's calls were no-op `Ok`s, including (absent run) in
    // the Replay phase Fray re-entered after the other human's departure.
    assert!(retired_noops > 0, "the retired seat was never driven");
    assert_eq!(retired_replay_phase, !other_survives);
    // The departure barrier ran in both runs (empty in the survivor run).
    assert!(departures_ran);
    // At Ready: the seat is gone from the committed membership and its
    // watermark is retired at the Leave.
    let receipt = recovery.sequencer().receipt().clone();
    let mark = receipt
        .watermarks
        .iter()
        .find(|m| m.player == own_player)
        .unwrap();
    let session = SessionImage::decode(root, recovery.sequencer().image()).unwrap();
    let seat_gone = mark.retired
        && mark.input == leave_seq
        && seat.retired(&recovery)
        && !session.members.iter().any(|m| m.player == own_player)
        && !recovery.histories().contains_key(&own_player);
    assert!(seat_gone);
    let other_stays = session.members.iter().any(|m| m.player == other_player)
        && receipt
            .watermarks
            .iter()
            .any(|m| m.player == other_player && !m.retired);
    assert_eq!(other_stays, other_survives);
    let own_delivered = seat.journals().0.applied() == mark.output_ack;
    assert!(own_delivered);
    // A retired seat's seal stays a no-op `Ok` at Ready; nothing changes.
    let retired_seal_noop = seat.seal(&mut recovery) == Ok(())
        && recovery.sequencer().receipt() == &receipt
        && recovery.phase() == RecoveryPhase::Ready;
    assert!(retired_seal_noop);
    let own_phase = seat.client().phase().clone();
    assert_eq!(own_phase, race_session::ClientPhase::Closed("left".into()));
    // The promoted device keeps hosting without a player seat: its restored
    // native host is still racing, the own racer is a bot, and one native
    // tick runs on it. (Successor commits after Ready are out of scope.) The
    // fixture's host is headless, so with no humans left (absent run) it
    // keeps racing with bots instead of resetting to the lobby.
    let humans = usize::from(other_survives);
    let mut host = migration::committed_authority(root, recovery.sequencer().image()).unwrap();
    let tick_at_ready = host.world().unwrap().tick;
    let ready_ok = host.phase() == race_session::HostPhase::Racing
        && host.human_count() == humans
        && host.racer_of(native_id).is_none()
        && host.world().unwrap().racers.len() == racers_at_c
        && matches!(
            host.world().unwrap().cfg.racers[own_racer].kind,
            RacerKind::Bot(_)
        );
    assert!(ready_ok);
    host.update(frontier.logical_ms + STEP);
    let ticked = host.world().unwrap().tick > tick_at_ready;
    let room_healthy = host.phase() == race_session::HostPhase::Racing
        && host.world().unwrap().phase == RacePhase::Racing
        && host.human_count() == humans
        && host.racer_of(native_id).is_none()
        && host.world().unwrap().racers.len() == racers_at_c;
    assert!(
        ticked && room_healthy,
        "promoted host did not tick on healthily"
    );
    let frozen_at_c = session.logical_ms == frontier.logical_ms;
    assert!(frozen_at_c);
    let seat_reclaimed = !seat_gone;
    println!("STAMPEDE OWN SEAT LEAVE AT C other_human={} intent=Leave leave_sequence={leave_seq} leave_drains={leave_drains} humans_before={humans_before} humans_after={humans_after} racer_count_kept={racers_kept} bot_took_over_own_racer={bot_took_over} own_racer_released={racer_released} watermark_retired={retired} next_intent_after_leave={next_after_leave:?} retired_seat_noop_calls={retired_noops} retired_noop_in_reentered_replay={retired_replay_phase} retired_seal_noop_at_ready={retired_seal_noop} duplicate_supplies_noop={duplicates_noop} pings_exactly_once={pings_once} sealed_tail_is_cut_at_c={sealed_tail_is_cut} seat_gone={seat_gone} seat_reclaimed={seat_reclaimed} own_delivered={own_delivered} own_client_phase={own_phase:?} recovery_frozen_at_c={frozen_at_c} phase={:?} humans_at_ready={} native_tick_after_ready={ticked} room_healthy={room_healthy}", if other_survives { "survivor" } else { "absent" }, recovery.phase(), host.human_count());
}

struct MissingOwnIntent<'a> {
    recovery: Recovery,
    seat: &'a migration::OwnSeat,
    root: &'a Root,
    key: &'a decented_net::Keys,
    hole: u64,
    frontier: &'a Receipt,
    promotion_ms: u64,
    rec_ms: u64,
    drains: usize,
    departures_ran: bool,
    tracked_pongs: usize,
}
/// The own seat's sealed tail has a hole that is never supplied. Up to the
/// 5000 ms recovery deadline every step past it is refused with `Gap`: no
/// later intent of the seat drains, no departure runs and the seat is not
/// reclaimed. At the deadline recovery fails with its reason and stays
/// failed. Committed state never moves.
fn missing_own_intent(m: MissingOwnIntent<'_>) {
    let MissingOwnIntent {
        mut recovery,
        seat,
        root,
        key,
        hole,
        frontier,
        promotion_ms,
        mut rec_ms,
        drains,
        departures_ran,
        tracked_pongs,
    } = m;
    let player = seat.player();
    let handled = |recovery: &Recovery| recovery.histories()[&player].handled();
    let supplied_after_hole = seat
        .journals()
        .1
        .pending()
        .filter(|(n, _)| *n > hole)
        .count();
    assert!(supplied_after_hole > 0);
    let receipt = recovery.sequencer().receipt().clone();
    let image = recovery.sequencer().image().to_vec();
    // Both humans' seats and watermarks are still there: nobody departed.
    let seat_kept = |recovery: &Recovery| {
        migration::committed_authority(root, recovery.sequencer().image())
            .unwrap()
            .human_count()
            == 2
            && recovery
                .sequencer()
                .receipt()
                .watermarks
                .iter()
                .any(|m| m.player == player && !m.retired)
    };
    assert_eq!(
        handled(&recovery),
        hole - 1,
        "intent before the hole lost or one skipped"
    );
    assert_eq!(recovery.phase(), RecoveryPhase::Draining);
    assert!(seat_kept(&recovery));
    assert_eq!(tracked_pongs, 0);
    let deadline = promotion_ms + 5000;
    let mut refusals = 0;
    while rec_ms < deadline - 1 {
        rec_ms = (rec_ms + 250).min(deadline - 1);
        recovery.poll(rec_ms).unwrap();
        assert_eq!(recovery.next_input().err(), Some(Error::Gap));
        assert_eq!(
            migration::recover_next(&mut recovery, key, rec_ms).err(),
            Some(Error::Gap)
        );
        assert_eq!(recovery.begin_round(), Err(Error::Gap));
        assert_eq!(recovery.phase(), RecoveryPhase::Draining);
        assert_eq!(recovery.sequencer().receipt(), &receipt);
        assert_eq!(recovery.sequencer().image(), image.as_slice());
        assert_eq!(handled(&recovery), hole - 1);
        refusals += 1;
    }
    let paused_until_deadline = refusals > 0
        && recovery.phase() == RecoveryPhase::Draining
        && handled(&recovery) == hole - 1
        && seat_kept(&recovery);
    assert!(paused_until_deadline);
    let outcome = recovery.poll(deadline);
    assert_eq!(outcome, Err(Error::Invalid("recovery deadline")));
    assert_eq!(recovery.phase(), RecoveryPhase::Failed);
    assert_eq!(recovery.poll(deadline + 1000), Err(Error::Closed));
    assert!(migration::recover_next(&mut recovery, key, deadline + 1000).is_err());
    assert!(migration::recover_departures(&mut recovery, key, deadline + 1000).is_err());
    assert!(recovery.begin_round().is_err());
    let late = seat
        .journals()
        .1
        .pending()
        .find(|(n, _)| *n == hole)
        .map(|(_, b)| b.to_vec())
        .unwrap();
    assert!(recovery.supply(player, hole, late).is_err());
    assert_eq!(recovery.phase(), RecoveryPhase::Failed);
    let committed_unchanged =
        recovery.sequencer().receipt() == &receipt && recovery.sequencer().image() == image;
    assert!(committed_unchanged);
    assert!(!departures_ran);
    let skipped = handled(&recovery) - (hole - 1);
    let reclaimed = !seat_kept(&recovery);
    let frozen_at_c = SessionImage::decode(root, recovery.sequencer().image())
        .unwrap()
        .logical_ms
        == frontier.logical_ms;
    assert!(frozen_at_c && skipped == 0 && !reclaimed);
    println!("STAMPEDE MISSING OWN-SEAT INTENT hole={hole} handled_own_intents={} supplied_after_hole={supplied_after_hole} total_drains={drains} refusals_before_deadline={refusals} paused_until_deadline={paused_until_deadline} skipped_intents={skipped} seat_reclaimed={reclaimed} departures_run={departures_ran} tracked_ping_pongs={tracked_pongs} committed_state_unchanged={committed_unchanged} recovery_frozen_at_c={frozen_at_c} outcome={outcome:?} phase={:?}", handled(&recovery), recovery.phase());
}

// Regression for the earlier `Conflict`: the standby device's own seat signs
// with the standby runtime key, the same key as the pair link's receipts.
// Through its endpoint on the shared sender sequence its controls interleave
// with the pair link's records and every one is accepted while both runtimes
// commit. A control from a separate per-player counter (the old fixture's
// `Seat` numbering, capability ACK = 1 then 2, 3, ...) reuses a sequence the
// original already holds from the pair link and is refused as `Conflict`,
// which permanently fails the original's replay session.
#[test]
fn standby_own_seat_controls_share_the_pair_link_sender() {
    let mut lobby = Lobby::new();
    let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
    let root = descriptor.root.clone();
    let wall = || clock.now_ms();
    let initial = state_hash(receiver.sequencer.image()).unwrap();
    let key = lobby.identities[0].keys().clone();
    let mut standby = migration::LiveStandby::new(
        descriptor.clone(),
        receiver,
        lobby.identities[0].clone(),
        clock,
    )
    .unwrap();
    let mut live = owned_primary(&mut lobby, &descriptor, source, clock);
    let cert = standby.membership_certificate().unwrap();
    let (mut own, own_counter_refused) =
        own_link(&descriptor, &mut standby, &mut live, &key, initial);
    assert!(own_counter_refused);
    let base = SessionImage::decode(&root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    let mut accepted = 0;
    for index in 1..=6u64 {
        let wall_now = wall();
        own.endpoint
            .queue_view(datagram(index as u32, |n| pad(n, false)))
            .unwrap();
        while let Some(wire) = own.endpoint.poll_send(wall_now).unwrap() {
            let (_, _, bytes) = own.server.receive(&wire, wall_now).unwrap().unwrap();
            assert_eq!(own.queue.receive(&bytes, live.primary()), Ok(true));
            accepted += 1;
        }
        let (first, _) = commit_own(
            &mut live,
            &mut standby,
            &mut own.queue,
            index,
            base + index * STEP,
        );
        assert!(first);
    }
    let healthy = live.failure().is_none() && standby.failure().is_none();
    assert!(healthy && accepted == 6);
    // The old numbering: an independent counter after the capability ACK.
    let player = own.queue.player();
    let old = PlayerControl::sign(
        &root,
        &cert,
        0,
        2,
        player,
        own.server.tls(),
        PlayerAction::Lossy {
            number: 1000,
            bytes: datagram(7, |n| pad(n, false)),
        },
        &key,
    )
    .unwrap()
    .encode(&root)
    .unwrap();
    let separate_counter = own.queue.receive(&old, live.primary());
    assert_eq!(separate_counter, Err(Error::Conflict));
    assert_eq!(
        live.sender_sequence().check_live(),
        Err(Error::Closed),
        "a replay conflict fails the original's replay session"
    );
    println!("STAMPEDE OWN SEAT SHARED SENDER commits=6 own_controls_accepted={accepted} every_own_control_accepted={} runtimes_healthy={healthy} own_counter_endpoint_refused={own_counter_refused} separate_counter={separate_counter:?}", accepted == 6);
}

// ─── Real player transport: remote humans on session-bound QUIC links ──────

/// A remote client's QUIC link which also records every reliable wire sent.
#[derive(Clone)]
struct Tap(Arc<Mutex<QuicClient>>, Arc<Mutex<Vec<Vec<u8>>>>);
impl ClientTransport for Tap {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        self.0.lock().unwrap().poll()
    }
    fn send_reliable(&mut self, bytes: Vec<u8>) {
        self.1.lock().unwrap().push(bytes.clone());
        self.0.lock().unwrap().send_reliable(bytes);
    }
    fn send_lossy(&mut self, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_lossy(bytes);
    }
    fn rtt_ms(&self) -> Option<u32> {
        self.0.lock().unwrap().rtt_ms()
    }
    fn close(&mut self) {
        self.0.lock().unwrap().close();
    }
}

fn player_policy(descriptor: &Descriptor, cert: &MembershipCertificate) -> EndpointPolicy {
    EndpointPolicy {
        descriptor: descriptor.clone(),
        role: LinkRole::Original,
        promotion: None,
        membership: Some(cert.clone()),
    }
}

/// An armed four-human room (the standby device plus three remote humans)
/// with four bots. The original's lobby listener is pinned to the session
/// and the three remote humans have reconnected on session-bound player links.
struct ArmedRoom {
    /// What each refused `PlayerHost::new` during the build returned.
    pin_refusals: Vec<Error>,
    cfg: NetConfig,
    original: Identity,
    host_client: Option<ClientSession>,
    addr: std::net::SocketAddr,
    token: String,
    descriptor: Descriptor,
    identities: Vec<Identity>,
    clients: Vec<Option<ClientSession>>,
    networks: Vec<Option<QuicClient>>,
    live: migration::LivePrimary,
    standby: migration::LiveStandby,
    players: migration::PlayerHost,
    cert: MembershipCertificate,
    initial: Hash,
    /// The runner clock both runners share; `wall()` samples it.
    clock: migration::RunnerClock,
    base: u64,
    /// The commit that starts the race (lobby commits before it).
    start_at: u64,
}
impl ArmedRoom {
    fn new() -> Self {
        Self::build(None).0
    }
    /// As [`Self::new`], with a playing phone host: the original's own
    /// player holds a seat too (its client is returned).
    fn phone() -> (Self, ClientSession) {
        let (mut room, _) = Self::build_with(None, true, false);
        let own = room.host_client.take().unwrap();
        (room, own)
    }
    /// As [`Self::new`]; room identity `expiring` joins its session-bound link
    /// with a My Signet approval that expires within seconds (its UTC end).
    fn build(expiring: Option<usize>) -> (Self, u64) {
        Self::build_with(expiring, false, false)
    }
    /// `refuse_first`: the first `PlayerHost::new` names another session's
    /// descriptor and is refused; the listener it hands back is retried.
    fn build_with(expiring: Option<usize>, phone: bool, refuse_first: bool) -> (Self, u64) {
        let lobby = Lobby::<4>::build(phone);
        let (descriptor, source, receiver, clock) = complete_bootstrap(&lobby);
        let initial = state_hash(receiver.sequencer.image()).unwrap();
        let mut standby = migration::LiveStandby::new(
            descriptor.clone(),
            receiver,
            lobby.identities[0].clone(),
            clock,
        )
        .unwrap();
        let Lobby {
            cfg,
            original,
            identities,
            network,
            links,
            host,
            clients,
            host_client,
            ..
        } = lobby;
        // The lobby authority still holds the ordinary listener as its
        // transport; the owned runtime gets an identical copy without it.
        let (binding, bytes) = migration::checkpoint(&host).unwrap();
        drop(host);
        let host = migration::restore(&bytes, &binding, Box::new(Script::default())).unwrap();
        let mut live =
            migration::LivePrimary::new(descriptor.clone(), source, host, original.clone(), clock)
                .unwrap();
        // The players' own (client-side) certificate. The player host takes
        // the original's, learned over the pair link, below.
        let cert = standby.membership_certificate().unwrap();
        // Arming detaches every ordinary connection before the pin.
        for link in &links {
            link.0.lock().unwrap().close();
        }
        let mut network = Arc::try_unwrap(network.0)
            .ok()
            .expect("lobby listener still shared")
            .into_inner()
            .unwrap();
        let t = Instant::now();
        let mut left = 0;
        while left < 4 {
            while let Some(event) = network.poll() {
                if matches!(event, HostNetEvent::Left { .. }) {
                    left += 1;
                }
            }
            bounded(t);
        }
        let (addr, token) = {
            let h = network.handle().unwrap();
            (
                std::net::SocketAddr::from(([127, 0, 0, 1], h.local_addr().unwrap().port())),
                h.lan_token(),
            )
        };
        let mut pin_refusals = Vec::new();
        if refuse_first {
            let mut other = descriptor.clone();
            other.root.host[0] ^= 1;
            match migration::PlayerHost::new(network, &other, &live) {
                Ok(_) => panic!("another session's descriptor pinned the listener"),
                Err(refused) => {
                    pin_refusals.push(refused.error);
                    network = refused.network;
                }
            }
        }
        // Until the original holds the standby's certificate for its receipt
        // (index 0 may still be on the pair link), the player host is refused
        // with the listener handed back; retry after the next certificate.
        let t = Instant::now();
        let players = loop {
            match migration::PlayerHost::new(network, &descriptor, &live) {
                Ok(players) => break players,
                Err(migration::PlayerHostRefused {
                    error: error @ (Error::Gap | Error::Stale),
                    network: back,
                }) => {
                    pin_refusals.push(error);
                    network = back;
                    'membership: loop {
                        for event in live.poll().unwrap() {
                            if matches!(event, migration::RuntimeEvent::Membership(_)) {
                                break 'membership;
                            }
                        }
                        standby.poll().unwrap();
                        bounded(t);
                    }
                }
                Err(refused) => panic!("player host refused: {:?}", refused.error),
            }
        };
        let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
            .unwrap()
            .logical_ms;
        let mut room = Self {
            cfg,
            original,
            host_client,
            addr,
            token,
            descriptor,
            identities: identities.into(),
            clients: clients.into_iter().map(Some).collect(),
            networks: vec![None, None, None, None],
            live,
            standby,
            players,
            cert,
            initial,
            clock,
            base,
            start_at: 1,
            pin_refusals,
        };
        let mut until = u64::MAX;
        for i in 1..4 {
            let mut id = room.identities[i].clone();
            if expiring == Some(i) {
                (id, until) = shortly_expiring(&id);
            }
            room.networks[i] = Some(room.session_join(&id));
        }
        let t = Instant::now();
        while room.players.players().count() < 3 {
            let now = room.wall();
            room.players.poll(&mut room.live, now).unwrap();
            bounded(t);
        }
        (room, until)
    }
    fn wall(&self) -> u64 {
        self.clock.now_ms()
    }
    fn poll_players(&mut self) {
        let now = self.wall();
        self.players.poll(&mut self.live, now).unwrap();
    }
    fn root(&self) -> &Root {
        &self.descriptor.root
    }
    fn player(&self, i: usize) -> u64 {
        let key = self.identities[i].pubkey().to_bytes();
        self.root()
            .members
            .iter()
            .find(|m| m.key == Some(key))
            .unwrap()
            .player
    }
    /// A session-bound player connection, admitted (persona approved) and
    /// polled to `Connected`. The pinned listener must also run meanwhile.
    fn session_join(&mut self, id: &Identity) -> QuicClient {
        let mut link = self.session_dial(id);
        let t = Instant::now();
        loop {
            let now = self.wall();
            self.players.poll(&mut self.live, now).unwrap();
            match link.poll() {
                Some(ClientNetEvent::Connected { .. }) => return link,
                Some(ClientNetEvent::Progress(_)) | None => {}
                other => panic!("session join refused: {other:?}"),
            }
            bounded(t);
        }
    }
    /// The session-bound player connection of [`Self::session_join`], not
    /// yet polled.
    fn session_dial(&self, id: &Identity) -> QuicClient {
        let root = self.root().clone();
        let binding = ConnectionBinding {
            session: root.session,
            descriptor: root.digest().unwrap(),
            term: 0,
            client: LinkRole::Player,
            server: LinkRole::Original,
        };
        let host = decented_net::PublicKey::from_slice(&root.host).unwrap();
        QuicClient::new(
            decented_net::join_session_addr(&self.cfg, id, self.addr, &self.token, binding, host)
                .unwrap(),
        )
    }
    fn capability(&self, i: usize) -> CapabilityAck {
        // The activation vote this player signed (deterministic signature).
        CapabilityAck::sign(
            self.root(),
            self.player(i),
            self.initial,
            1,
            self.identities[i].keys(),
        )
        .unwrap()
    }
    fn remote(&mut self, i: usize) -> migration::RemotePlayer {
        let network = self.networks[i].take().unwrap();
        let client = self.clients[i].take().unwrap();
        migration::RemotePlayer::connect(
            self.descriptor.clone(),
            self.standby.armed(),
            &self.capability(i),
            self.cert.clone(),
            network,
            client,
            self.identities[i].keys().clone(),
        )
        .unwrap()
    }
    /// The same, over a recording tap on the same admitted connection.
    fn tapped_remote(
        &mut self,
        i: usize,
    ) -> (
        migration::RemotePlayer,
        Arc<Mutex<QuicClient>>,
        Arc<Mutex<Vec<Vec<u8>>>>,
    ) {
        let network = Arc::new(Mutex::new(self.networks[i].take().unwrap()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let channel = network
            .lock()
            .unwrap()
            .migration_channel(&player_policy(&self.descriptor, &self.cert))
            .unwrap();
        let remote = migration::RemotePlayer::with_transport(
            self.descriptor.clone(),
            self.standby.armed(),
            &self.capability(i),
            self.cert.clone(),
            channel,
            Box::new(Tap(network.clone(), sent.clone())),
            self.clients[i].take().unwrap(),
            self.identities[i].keys().clone(),
        )
        .unwrap();
        (remote, network, sent)
    }
    fn image(&self) -> SessionImage {
        SessionImage::decode(self.root(), self.live.primary().committed_image()).unwrap()
    }
    fn handled(&self, player: u64) -> u64 {
        self.image().inputs[&player].handled()
    }
    fn output_ack(&self, player: u64) -> u64 {
        self.live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .find(|m| m.player == player)
            .unwrap()
            .output_ack
    }
    fn outbox(&self, player: u64) -> Vec<u64> {
        self.live
            .primary()
            .outbox()
            .filter(|r| r.output.recipient == player)
            .map(|r| r.output.sequence)
            .collect()
    }
    /// Stage `index` over the player host's queues, then drive the original,
    /// the standby and the player transport until the original publishes it.
    /// Outputs are framed to players only on `Committed`, after the standby
    /// installed and receipted the batch.
    fn commit(&mut self, index: u64) -> Vec<CertifiedOutput> {
        self.commit_with(index, None)
    }
    /// As [`Self::commit`], also staging the standby device's own-seat queue
    /// (refreshed to the new receipt, its selected views discarded).
    fn commit_with(&mut self, index: u64, own: Option<&mut PlayerQueue>) -> Vec<CertifiedOutput> {
        self.commit_at(index, self.base + index * STEP, own)
    }
    /// As [`Self::commit_with`], at game time `now` (which may repeat the
    /// last commit's: a stalled game clock).
    fn commit_at(
        &mut self,
        index: u64,
        now: u64,
        own: Option<&mut PlayerQueue>,
    ) -> Vec<CertifiedOutput> {
        let start_at = self.start_at;
        self.commit_op(index, now, own, |h| {
            if index == start_at && !h.start_race(now) {
                return Err(Error::Invalid("race start"));
            }
            Ok(())
        })
    }
    /// As [`Self::commit_at`], with the host's own `operation` in the batch
    /// instead of the race start.
    fn commit_op(
        &mut self,
        index: u64,
        now: u64,
        mut own: Option<&mut PlayerQueue>,
        operation: impl FnMut(&mut HostAuthority) -> Result<()>,
    ) -> Vec<CertifiedOutput> {
        let wall = self.wall();
        if let Some(queue) = own.as_mut() {
            queue.refresh(self.live.primary()).unwrap();
        }
        let extra: Vec<&PlayerQueue> = own.as_deref().into_iter().collect();
        let staged = self
            .players
            .stage_with(&mut self.live, now, &extra, operation);
        assert_eq!(staged, Ok(true), "batch {index} at {now} not staged");
        let t = Instant::now();
        let mut published = None;
        loop {
            let wall = self.wall().max(wall);
            self.players.poll(&mut self.live, wall).unwrap();
            for event in self.live.poll().unwrap() {
                match event {
                    migration::RuntimeEvent::Committed {
                        outputs,
                        selected_views,
                        ..
                    } => {
                        assert!(published.is_none(), "batch {index} committed twice");
                        assert_eq!(
                            self.standby.receipt().index,
                            index,
                            "published before the standby receipt"
                        );
                        self.players
                            .publish(&self.live, &outputs, &selected_views, wall)
                            .unwrap();
                        if let Some(queue) = own.as_mut() {
                            queue.refresh(self.live.primary()).unwrap();
                            for (player, number) in &selected_views {
                                if *player == queue.player() {
                                    queue.discard_lossy(*number);
                                }
                            }
                        }
                        published = Some(outputs);
                    }
                    migration::RuntimeEvent::Promoted(_) => panic!("unexpected promotion"),
                    migration::RuntimeEvent::Membership(cert) => {
                        // Only the certificate for this batch's receipt, which
                        // trails its `Committed`, may be paired with it.
                        if published.is_none() {
                            assert!(cert.receipt.index < index, "certificate before its commit");
                            continue;
                        }
                        assert_eq!(cert.receipt, *self.live.primary().receipt());
                        assert_eq!(self.live.membership_certificate(), Some(&cert));
                        // The standby's certificate at this receipt, learned
                        // over the pair link, lets a dropped player resume
                        // (reconnect challenge).
                        self.players.certify(&self.live, wall).unwrap();
                        return published.unwrap();
                    }
                }
            }
            for receipt in self.standby.poll().unwrap() {
                assert_eq!(receipt.index, index);
            }
            bounded(t);
        }
    }
    /// The native authority's racer for room identity `i`, while the race runs.
    fn racer(&self, i: usize) -> Option<&race_core::Racer> {
        let key = self.identities[i].pubkey().to_bytes();
        let seat = self
            .root()
            .members
            .iter()
            .find(|m| m.key == Some(key))?
            .seat;
        let authority = self.live.authority();
        let world = authority.world()?;
        if world.phase != RacePhase::Racing {
            return None;
        }
        world.racers.get(authority.racer_of(seat)?)
    }
    fn native_equals_standby(&self) -> bool {
        migration::checkpoint(self.live.authority()).unwrap()
            == migration::checkpoint(&self.standby.committed_authority().unwrap()).unwrap()
    }
}

/// Hold the throttle, steer gently and press jump every 20th frame.
fn remote_input(index: u64, r: usize) -> Input {
    Input {
        steer: if r == 0 { 0.2 } else { -0.2 },
        throttle: 1.0,
        jump: index % 20 == 0,
        ..Input::default()
    }
}

/// Per remote human, what its client and the committed outputs saw.
#[derive(Default)]
struct RemoteLedger {
    /// Ping id -> original intent number.
    pings: BTreeMap<u32, u64>,
    /// Pong id -> committed Pong outputs for it.
    pongs: BTreeMap<u32, usize>,
    reliable_applied: usize,
    lossy_applied: usize,
    duplicates: usize,
    intents: u64,
    acks: usize,
    outbox_shrank: bool,
    peak_outbox: usize,
    /// Frames loss detection froze, by native state (lobby, countdown, race).
    frozen: [usize; 3],
    /// Fresh progress carried by a certificate (a commit with no output to
    /// this player), by native state.
    idle: [usize; 3],
}

const ROUNDS: u64 = 240;

/// What one remote human's native racer did while the race ran.
#[derive(Clone, Copy, Debug, Default)]
struct NativeDrive {
    racing_commits: u32,
    /// Commits where the native racer's last input steered as this client
    /// steers (each remote human steers its own way).
    own_steer: u32,
    /// Commits where that input held full throttle.
    throttle: u32,
    max_speed: f32,
    start: Option<race_core::Vec3>,
    /// Furthest distance from the racer's grid position.
    travelled: f32,
    /// A jump press reached the native racer (its input or an airborne racer).
    jumped: bool,
}
impl NativeDrive {
    fn observe(&mut self, racer: &race_core::Racer, input: &Input) {
        self.racing_commits += 1;
        let last = &racer.prev_input;
        if (last.steer - input.steer).abs() < 0.05 {
            self.own_steer += 1;
        }
        if last.throttle > 0.9 {
            self.throttle += 1;
        }
        self.max_speed = self.max_speed.max(racer.speed);
        let start = *self.start.get_or_insert(racer.pos);
        self.travelled = self.travelled.max(racer.pos.distance(start));
        self.jumped |= last.jump || !racer.on_ground;
    }
    fn drove(&self) -> bool {
        self.racing_commits > 0
            && self.own_steer * 10 >= self.racing_commits * 9
            && self.throttle >= 30
            && self.max_speed > 5.0
            && self.travelled > 3.0
            && self.jumped
    }
}

/// Two remote humans on real session-bound QUIC links, each with a real
/// `PlayerEndpoint`, plus the frozen standby seat and four bots, play an armed
/// race through the owned `LivePrimary` while the owned `LiveStandby` commits.
/// Fresh committed progress is a newer commit whose game time advanced (the
/// spec's rule, as Fray's `ProgressWatch`), not "something addressed to me".
/// Mid-race the original keeps committing, but its game clock stalls: every
/// batch is staged at the same game time. The commits still reach each
/// client (pongs, or the signed certificate when there is no output), yet
/// each client freezes on its first frame at or past `FREEZE_AFTER_MS` after
/// the last commit that advanced game time, and stays frozen until game time
/// moves again.
#[test]
fn a_stalled_game_clock_freezes_clients_although_commits_arrive() {
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2)];
    let mut index = 0;
    let frame = |room: &mut ArmedRoom,
                 remotes: &mut [migration::RemotePlayer; 2],
                 index: u64,
                 local: u64| {
        remotes.each_mut().map(|remote| {
            remote
                .frame(local, room.wall(), remote_input(index, 0))
                .unwrap()
        })
    };
    // Play into the race.
    while !room
        .live
        .authority()
        .world()
        .is_some_and(|w| w.phase == RacePhase::Racing && w.tick > 30)
    {
        index += 1;
        let local = room.base + index * STEP;
        frame(&mut room, &mut remotes, index, local);
        room.commit(index);
        assert!(index < 400, "the race never started");
    }
    // Let the last advancing commit reach both clients.
    let advancing = room.base + index * STEP;
    let t = Instant::now();
    loop {
        frame(&mut room, &mut remotes, index, advancing);
        let last = room.live.primary().receipt().index;
        if remotes
            .iter()
            .all(|r| r.newest_receipt() == Some((0, last)))
        {
            break;
        }
        bounded(t);
    }
    let progress = remotes.each_ref().map(|r| r.last_progress().unwrap());
    assert_eq!(progress, [advancing; 2]);
    // The game clock stalls; commits go on, and the client frames go on in
    // local time.
    let stalled_at = index;
    let mut local = advancing;
    let mut freeze: [Option<(u64, u64)>; 2] = [None; 2];
    let mut prev = [local; 2];
    let mut reached = [0usize; 2];
    let mut stalled_commits = 0;
    while local < advancing + 2 * migration::FREEZE_AFTER_MS {
        local += STEP;
        index += 1;
        let steps = frame(&mut room, &mut remotes, index, local);
        for r in 0..2 {
            if freeze[r].is_some() {
                // After the freeze: commits still arriving, none progress.
                reached[r] +=
                    usize::from(steps[r].newest_receipt.is_some()) + steps[r].certificates;
                assert_eq!(steps[r].idle_progress, 0);
            }
            if steps[r].frozen && freeze[r].is_none() {
                freeze[r] = Some((local, prev[r]));
            }
            assert!(
                freeze[r].is_none() || steps[r].frozen,
                "client {r} unfroze while the game clock was stalled"
            );
            prev[r] = local;
        }
        room.commit_at(index, advancing, None);
        stalled_commits += 1;
        assert_eq!(room.live.primary().receipt().logical_ms, advancing);
    }
    for r in 0..2 {
        let (at, before) =
            freeze[r].unwrap_or_else(|| panic!("client {r} never froze on a stalled game clock"));
        assert!(
            at >= progress[r] + migration::FREEZE_AFTER_MS
                && before < progress[r] + migration::FREEZE_AFTER_MS,
            "client {r} froze {} ms after the last advancing commit",
            at - progress[r]
        );
        assert!(reached[r] > 0, "no commit reached frozen client {r}");
    }
    // Game time moves again (one frame's worth): fresh progress. It
    // unfreezes the clients at once, although this stall left their clocks
    // about 230 ms ahead: they step on with their host time pulled down to
    // the commits (`MAX_LEAD_MS`), so a cut keeps moving with C.
    index += 1;
    room.commit_at(index, advancing + STEP, None);
    let t = Instant::now();
    let mut unfrozen = [false; 2];
    while !unfrozen.iter().all(|u| *u) {
        local += STEP;
        let steps = frame(&mut room, &mut remotes, index, local);
        for r in 0..2 {
            unfrozen[r] |= !steps[r].frozen;
        }
        bounded(t);
    }
    println!(
        "STAMPEDE STALLED CLOCK stalled_commits={stalled_commits} from_index={stalled_at} froze_after_ms={:?} constant={} unfrozen_on_advance={} frames_reached_while_frozen={reached:?}",
        freeze.map(|f| f.map(|(at, _)| at - advancing)),
        migration::FREEZE_AFTER_MS,
        unfrozen.iter().all(|u| *u),
    );
}

/// The takeover correction bound after a soft hold, mid-race: see
/// [`held_clock_cut_stays_in_the_cap`].
#[test]
fn a_held_host_clock_keeps_every_racing_cut_within_the_correction_cap() {
    held_clock_cut_stays_in_the_cap(false);
}

/// As above, in the armed lobby (term 0, where no client freezes).
#[test]
fn a_held_host_clock_keeps_every_lobby_cut_within_the_correction_cap() {
    held_clock_cut_stays_in_the_cap(true);
}

/// Two remote humans play armed (in a race, or in the lobby), with synced
/// clocks. Then no commit lands for 450 ms (a stalled pair link): the
/// original's paced game clock holds at the last commit plus 250 ms. When
/// commits come back they catch up to that clock (at most 50 ms of game time
/// a batch, up to three batches a frame) and then run at real time.
///
/// A takeover at any instant would cut each client at its last stepped frame
/// and refuse a correction (the host time that frame recorded, minus C) over
/// 250 ms in either sign,
/// where C is the newest commit. So on every frame, before and after its
/// commits, that correction must be within the cap, and a client's host time
/// must never move by more than the cap between two stepped frames.
fn held_clock_cut_stays_in_the_cap(lobby: bool) {
    const STALL_MS: u64 = 450;
    const AFTER_MS: u64 = 2000;
    let mut room = ArmedRoom::new();
    if lobby {
        room.start_at = u64::MAX;
    }
    let mut remotes = [room.remote(1), room.remote(2)];
    let mut index = 0;
    let mut local;
    // Play until both clocks are synced (and the race runs).
    loop {
        index += 1;
        local = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(local, room.wall(), remote_input(index, r))
                .unwrap();
        }
        room.commit(index);
        let racing = room
            .live
            .authority()
            .world()
            .is_some_and(|w| w.phase == RacePhase::Racing && w.tick > 30);
        let synced = remotes
            .iter()
            .all(|r| r.client().clock_offset_ms().is_some());
        if synced && index > 40 && (lobby || racing) {
            break;
        }
        assert!(index < 600, "never synced (lobby {lobby})");
    }
    assert_eq!(room.live.authority().world().is_none(), lobby);
    let committed = |room: &ArmedRoom| room.live.primary().receipt().logical_ms;
    // The original's paced game clock.
    let mut game = committed(&room);
    let stall_from = local;
    let mut worst = [(0i64, 0i64); 2];
    let mut stepped: [Option<u64>; 2] = [None; 2];
    let mut jump = [(0i64, 0i64); 2];
    let mut frozen_frames = [0usize; 2];
    let check = |room: &ArmedRoom,
                 remotes: &[migration::RemotePlayer; 2],
                 worst: &mut [(i64, i64); 2],
                 at: &str,
                 local: u64| {
        let c = committed(room);
        for r in 0..2 {
            let (Some(cut), Some(host)) = (remotes[r].last_frame(), remotes[r].last_frame_host())
            else {
                continue;
            };
            // The cut: the host time the last stepped frame recorded.
            let delta = host as i64 - c as i64;
            worst[r] = (worst[r].0.min(delta), worst[r].1.max(delta));
            assert!(
                delta.unsigned_abs() <= 250,
                "client {r} {at}: a takeover now would correct {delta} ms (cut {} ms and now {} ms after the stall began; C={c}, newest seen {:?}, lobby {lobby})",
                cut as i64 - stall_from as i64,
                local - stall_from,
                remotes[r].newest_receipt(),
            );
        }
    };
    let mut frame_no = index;
    while local < stall_from + STALL_MS + AFTER_MS {
        frame_no += 1;
        local += STEP;
        let stalled = local < stall_from + STALL_MS;
        game = (game + STEP).min(committed(&room) + 250);
        for (r, remote) in remotes.iter_mut().enumerate() {
            let frame = remote
                .frame(local, room.wall(), remote_input(frame_no, r))
                .unwrap();
            if frame.frozen {
                frozen_frames[r] += 1;
            } else {
                let host = remote.client().host_now(local).unwrap();
                if let Some(prev) = stepped[r] {
                    let moved = host as i64 - prev as i64;
                    jump[r] = (jump[r].0.min(moved), jump[r].1.max(moved));
                    assert!(
                        moved.unsigned_abs() <= 250,
                        "client {r}'s host time moved {moved} ms between stepped frames"
                    );
                }
                stepped[r] = Some(host);
            }
        }
        check(&room, &remotes, &mut worst, "after its frame", local);
        if stalled {
            continue;
        }
        for _ in 0..3 {
            if committed(&room) >= game {
                break;
            }
            index += 1;
            room.commit_at(index, game, None);
            check(&room, &remotes, &mut worst, "after a commit", local);
        }
    }
    println!(
        "STAMPEDE HELD CLOCK CUT lobby={lobby} stall_ms={STALL_MS} correction_min_max={worst:?} stepped_host_jump_min_max={jump:?} frozen_frames={frozen_frames:?} cap=250"
    );
}

/// A client's own `Finished` and the `RaceOver`, as its view surfaced them
/// to the app (which reads `view().events` once per frame), counted over
/// every frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RaceEnd {
    finished: usize,
    race_over: usize,
}
impl RaceEnd {
    const ONCE: RaceEnd = RaceEnd {
        finished: 1,
        race_over: 1,
    };
    fn observe(&mut self, client: &ClientSession) {
        let Some(view) = client.view() else {
            return;
        };
        for event in &view.events {
            match event {
                race_core::Event::Finished { racer, .. } if *racer as usize == view.you => {
                    self.finished += 1
                }
                race_core::Event::RaceOver => self.race_over += 1,
                _ => {}
            }
        }
    }
}

/// The native bot's controls for `seat`'s racer while the race runs.
fn autopilot(authority: &HostAuthority, seat: u8) -> Input {
    match (authority.world(), authority.racer_of(seat)) {
        (Some(w), Some(i)) if i < w.racers.len() && w.phase == RacePhase::Racing => {
            race_core::bot_input(w, i, race_core::BotLevel::Normal)
        }
        _ => Input::default(),
    }
}

/// The app leaves the race screen for the results only on a host event (its
/// own `Finished` or the `RaceOver`), read once per frame from the client's
/// view. An armed client's frame runs one client step per received output and
/// then the frame's own; every one of those events must still reach the view
/// the app reads, once. Two remote humans race one lap under term 0 (the
/// standby's own seat and the idle third human are kicked in the lobby; the
/// native bot drives the two) to the results.
#[test]
fn an_armed_client_surfaces_its_finish_and_the_race_over_once() {
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let root = room.root().clone();
    let seat_of = |p: u64| root.members.iter().find(|m| m.player == p).unwrap().seat;
    let gone = [seat_of(room.player(0)), seat_of(room.player(3))];
    let seats = [seat_of(room.player(1)), seat_of(room.player(2))];
    let mut remotes = [room.remote(1), room.remote(2)];
    let mut ends = [RaceEnd::default(); 2];
    let frame = |room: &ArmedRoom,
                 remotes: &mut [migration::RemotePlayer; 2],
                 ends: &mut [RaceEnd; 2],
                 index: u64| {
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            let input = autopilot(room.live.authority(), seats[r]);
            remote.frame(now, room.wall(), input).unwrap();
            ends[r].observe(remote.client());
        }
        now
    };
    let mut index = 0;
    while index < 30 {
        index += 1;
        frame(&room, &mut remotes, &mut ends, index);
        room.commit(index);
    }
    // One lap, and only the two remote humans race.
    index += 1;
    let now = frame(&room, &mut remotes, &mut ends, index);
    room.commit_op(index, now, None, |h| {
        if gone.iter().all(|s| h.kick(*s)) && h.set_laps(1) {
            Ok(())
        } else {
            Err(Error::Invalid("lobby setup"))
        }
    });
    assert_eq!(room.live.failure(), None, "the lobby setup paused the room");
    index += 1;
    frame(&room, &mut remotes, &mut ends, index);
    room.commit(index);
    index += 1;
    room.start_at = index;
    frame(&room, &mut remotes, &mut ends, index);
    room.commit(index);
    let mut results_at = None;
    loop {
        index += 1;
        frame(&room, &mut remotes, &mut ends, index);
        room.commit(index);
        let shown = room.live.authority().results().is_some()
            && remotes.iter().all(|r| r.client().results().is_some());
        if shown && results_at.is_none() {
            results_at = Some(index);
        }
        // A few frames on the results: nothing surfaces twice.
        if results_at.is_some_and(|at| index >= at + 10) {
            break;
        }
        assert!(index < 6000, "the race never reached the results");
    }
    assert_eq!(
        room.live.authority().phase(),
        race_session::HostPhase::Results
    );
    println!("STAMPEDE ARMED RACE END results_at={results_at:?} surfaced={ends:?}");
    assert_eq!(
        ends,
        [RaceEnd::ONCE; 2],
        "an armed client's view lost or repeated its finish or the race over"
    );
}

/// A takeover's clock cut is the host time a client's last stepped frame ran
/// at, not that frame's local time read through the clock as it stands at the
/// cut. Mid-race the original's game clock stalls while it keeps committing:
/// the clients freeze, and the stalled commits carry the pongs of a sync round
/// each opened just before, which completes while it is frozen and replaces
/// its clock offset. The original then dies. Each survivor's confirmed
/// correction must be its host time at its last stepped frame, as recorded
/// then, minus C, and within the 250 ms cap.
#[test]
fn a_sync_round_completing_while_frozen_does_not_move_the_takeover_cut() {
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let root = room.root().clone();
    let standby_at = loopback(room.standby.listener().unwrap(), root.standby);
    let warm_binding = session_binding(&root, 0, LinkRole::ControlPlayer, LinkRole::Standby);
    let mut warm = Vec::new();
    for i in 1..3 {
        let (id, cfg) = (room.identities[i].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        });
        warm.push(link.expect("prepared control link"));
    }
    let ids = [room.player(1), room.player(2)];
    let own_seat = root
        .members
        .iter()
        .find(|m| m.player == room.player(0))
        .unwrap()
        .seat;
    let mut waiting = [Some(room.remote(1)), Some(room.remote(2))];
    for (r, link) in warm.into_iter().enumerate() {
        waiting[r].as_mut().unwrap().prepare_standby(link).unwrap();
    }
    // Per survivor, its last stepped frame: (local time, host time then,
    // clock offset then).
    let mut stepped: [Option<(u64, u64, i64)>; 2] = [None; 2];
    let frame = |room: &ArmedRoom,
                 waiting: &mut [Option<migration::RemotePlayer>; 2],
                 stepped: &mut [Option<(u64, u64, i64)>; 2],
                 now: u64,
                 index: u64,
                 resync: bool| {
        let mut frozen = [false; 2];
        for (r, remote) in waiting.iter_mut().enumerate() {
            let remote = remote.as_mut().unwrap();
            let f = remote
                .frame_with(now, room.wall(), remote_input(index, r), |c| {
                    if resync {
                        // A fresh round opens in this frame; the offset holds.
                        c.migration_resync_clock();
                    }
                })
                .unwrap();
            frozen[r] = f.frozen;
            let client = remote.client();
            if !f.frozen {
                if let (Some(host), Some(offset)) = (client.host_now(now), client.clock_offset_ms())
                {
                    stepped[r] = Some((now, host, offset));
                }
            }
        }
        frozen
    };
    // The armed lobby (the clocks sync), then the standby device's own seat
    // is kicked, so it takes over seatless.
    let mut index = 0;
    while index < 30 {
        index += 1;
        frame(&room, &mut waiting, &mut stepped, room.base + index * STEP, index, false);
        room.commit(index);
    }
    index += 1;
    let now = room.base + index * STEP;
    frame(&room, &mut waiting, &mut stepped, now, index, false);
    room.commit_op(index, now, None, |h| {
        if h.kick(own_seat) {
            Ok(())
        } else {
            Err(Error::Invalid("kick"))
        }
    });
    // The next batch retires it; the one after starts the race.
    room.start_at = index + 2;
    while !room
        .live
        .authority()
        .world()
        .is_some_and(|w| w.phase == RacePhase::Racing && w.tick > 30)
    {
        index += 1;
        frame(&room, &mut waiting, &mut stepped, room.base + index * STEP, index, false);
        room.commit(index);
        assert!(index < 400, "the race never started");
    }
    // Let the last advancing commit reach both clients.
    let advancing = room.base + index * STEP;
    let t = Instant::now();
    loop {
        frame(&room, &mut waiting, &mut stepped, advancing, index, false);
        let last = room.live.primary().receipt().index;
        if waiting
            .iter()
            .all(|r| r.as_ref().unwrap().newest_receipt() == Some((0, last)))
        {
            break;
        }
        bounded(t);
    }
    // The game clock stalls; commits go on. Each client opens a fresh sync
    // round in the first stalled frame; its pongs come back stamped at the
    // stalled time, mostly after the client froze, and the round completes
    // while it is frozen.
    let mut local = advancing;
    let mut first = true;
    let mut frozen_since: [Option<u64>; 2] = [None; 2];
    let completed_frozen = |waiting: &[Option<migration::RemotePlayer>; 2],
                            stepped: &[Option<(u64, u64, i64)>; 2],
                            frozen_since: &[Option<u64>; 2]| {
        (0..2).all(|r| {
            let client = waiting[r].as_ref().unwrap().client();
            frozen_since[r].is_some()
                && !client.clock_sync().round_open()
                && client.clock_offset_ms() != stepped[r].map(|s| s.2)
        })
    };
    while !completed_frozen(&waiting, &stepped, &frozen_since) {
        local += STEP;
        index += 1;
        let frozen = frame(&room, &mut waiting, &mut stepped, local, index, first);
        first = false;
        for r in 0..2 {
            assert!(
                frozen_since[r].is_none() || frozen[r],
                "client {r} unfroze while the game clock was stalled"
            );
            if frozen[r] {
                frozen_since[r].get_or_insert(local);
            }
        }
        room.commit_at(index, advancing, None);
        assert!(
            local < advancing + 3000,
            "no sync round completed while frozen: frozen since {frozen_since:?}, stepped {stepped:?}"
        );
    }
    let c_receipt = room.standby.receipt().clone();
    let c = c_receipt.logical_ms;
    assert_eq!(c, advancing, "C is not the stalled game time");
    let stepped = stepped.map(Option::unwrap);
    // What the cut read before: the last stepped frame's local time through
    // the clock as it stands now. The test is only meaningful if that moved.
    let current = [0, 1].map(|r| {
        waiting[r]
            .as_ref()
            .unwrap()
            .client()
            .migration_clock_delta(stepped[r].0, c)
    });
    for r in 0..2 {
        assert_eq!(waiting[r].as_ref().unwrap().last_frame(), Some(stepped[r].0));
        assert_ne!(
            current[r],
            stepped[r].1 as i64 - c as i64,
            "client {r}'s completed round did not move its estimate at the last stepped frame"
        );
    }
    let handled_c = ids.map(|p| {
        c_receipt
            .watermarks
            .iter()
            .find(|m| m.player == p)
            .unwrap()
            .input
    });
    let drop_local = local + STEP;
    let w_drop = room.wall();
    let drop_at = Instant::now();
    let ArmedRoom {
        identities,
        live,
        players,
        standby,
        clock,
        ..
    } = room;
    // The original stops dead: never polled again, nothing closed.
    let hung = (live, players);
    let wall = || clock.now_ms();
    let local = |w: u64| drop_local + w.saturating_sub(w_drop);
    let mut devices = SurvivorDevices {
        waiting,
        recovering: [None, None],
        last_frame: [drop_local - STEP; 2],
        freeze: [None; 2],
        frames: [0; 2],
        frozen_by: [None; 2],
        tail: [None; 2],
        pings: [Vec::new(), Vec::new()],
        resumed_local: [None; 2],
        applied: [0; 2],
        duplicates: [0; 2],
        discarded: 0,
    };
    let mut standby = Some(standby);
    let promoted = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        match standby.take().unwrap().promote() {
            Ok(promoted) => break promoted,
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            drop_at.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let keys = identities[0].keys().clone();
    let (mut links, mut recovery) = migration::SurvivorLinks::open(promoted.into_link()).unwrap();
    assert_eq!(recovery.promotion().final_receipt.logical_ms, c);
    let t = Instant::now();
    loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        match links.drive(&mut recovery, None, &keys, local(w), w) {
            Ok(RecoveryPhase::Ready) => break,
            Ok(_) => {}
            Err(error) => panic!("recovery refused: {error:?}"),
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "recovery stalled in {:?}",
            recovery.phase()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(hung);
    let mut barrier = links.barrier(recovery, &keys, wall()).unwrap();
    let t = Instant::now();
    loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        if links.drive_barrier(&mut barrier, wall()).unwrap() {
            break;
        }
        bounded(t);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (_successor, resume) = links.resume(barrier, &keys, wall()).unwrap();
    let t = Instant::now();
    while devices.resumed_local.iter().any(Option::is_none) {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        bounded(t);
    }
    let deltas = [0, 1].map(|r| devices.recovering(r).clock_delta().unwrap());
    let recorded = stepped.map(|(_, host, _)| host as i64 - c as i64);
    println!(
        "STAMPEDE FROZEN ROUND CUT c={c} last_stepped_after_stall_ms={:?} frozen_since_ms={:?} offset_at_last_frame={:?} recorded={recorded:?} through_current_offset={current:?} confirmed={deltas:?} cap=250",
        stepped.map(|s| s.0 - advancing),
        frozen_since.map(|f| f.unwrap() - advancing),
        stepped.map(|s| s.2),
    );
    for r in 0..2 {
        assert_eq!(devices.recovering(r).frozen_local(), stepped[r].0);
        let mark = resume.cuts.iter().find(|m| m.player == ids[r]).unwrap();
        assert_eq!(mark.clock_delta, deltas[r]);
    }
    assert_eq!(
        deltas, recorded,
        "a survivor's cut is not its host time at its last stepped frame"
    );
    assert!(deltas.iter().all(|d| d.unsigned_abs() <= 250), "{deltas:?}");
}

/// Where the digest starts in a migration frame (after its fixed header).
const FRAME_DIGEST: usize = 50;

/// What a [`Faulty`] link does to the wires its remote reads.
#[derive(Default)]
struct Fault {
    /// Wires read ahead from the link, served first.
    queued: VecDeque<ClientNetEvent>,
    /// The last reliable wire served.
    last: Option<Vec<u8>>,
    /// Serve a corrupted copy of `last` once the real wires are read.
    armed: bool,
}

/// A remote client's QUIC link which can append a corrupted reliable wire
/// after the real ones, so the frame which reads them fails part-way.
#[derive(Clone)]
struct Faulty(Arc<Mutex<QuicClient>>, Arc<Mutex<Fault>>);
impl Faulty {
    /// Read what the host has sent so far, so a frame which follows sees all
    /// of it before the corrupted wire.
    fn read_ahead(&self) {
        std::thread::sleep(Duration::from_millis(100));
        let mut fault = self.1.lock().unwrap();
        while let Some(event) = self.0.lock().unwrap().poll() {
            fault.queued.push_back(event);
        }
    }
    fn corrupt_next(&self) {
        self.1.lock().unwrap().armed = true;
    }
}
impl ClientTransport for Faulty {
    fn poll(&mut self) -> Option<ClientNetEvent> {
        let mut fault = self.1.lock().unwrap();
        let event = fault
            .queued
            .pop_front()
            .or_else(|| self.0.lock().unwrap().poll());
        match event {
            Some(ClientNetEvent::Reliable(bytes)) => {
                fault.last = Some(bytes.clone());
                Some(ClientNetEvent::Reliable(bytes))
            }
            None if fault.armed => {
                // A byte of the frame's digest: the transfer then conflicts
                // with the one already received under its number.
                let mut wire = fault.last.clone()?;
                *wire.get_mut(FRAME_DIGEST)? ^= 0xff;
                fault.armed = false;
                Some(ClientNetEvent::Reliable(wire))
            }
            event => event,
        }
    }
    fn send_reliable(&mut self, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_reliable(bytes);
    }
    fn send_lossy(&mut self, bytes: Vec<u8>) {
        self.0.lock().unwrap().send_lossy(bytes);
    }
    fn rtt_ms(&self) -> Option<u32> {
        self.0.lock().unwrap().rtt_ms()
    }
    fn close(&mut self) {
        self.0.lock().unwrap().close();
    }
}

impl ArmedRoom {
    /// As [`Self::remote`], over a [`Faulty`] link on the same admitted
    /// connection.
    fn faulty_remote(&mut self, i: usize) -> (migration::RemotePlayer, Faulty) {
        let network = Arc::new(Mutex::new(self.networks[i].take().unwrap()));
        let channel = network
            .lock()
            .unwrap()
            .migration_channel(&player_policy(&self.descriptor, &self.cert))
            .unwrap();
        let link = Faulty(network, Arc::default());
        let remote = migration::RemotePlayer::with_transport(
            self.descriptor.clone(),
            self.standby.armed(),
            &self.capability(i),
            self.cert.clone(),
            channel,
            Box::new(link.clone()),
            self.clients[i].take().unwrap(),
            self.identities[i].keys().clone(),
        )
        .unwrap();
        (remote, link)
    }
}

/// Two remote humans race one lap under term 0 (as in the test above) until
/// the first one's racer has finished on the host. Its link then appends a
/// corrupted wire after the real ones, so the next frame applies the output
/// carrying the `Finished` and then fails on the corrupt one. Returns the
/// room, the remotes, the next frame's index and that frame's result.
fn armed_frame_failing_after_a_finish() -> (
    ArmedRoom,
    [migration::RemotePlayer; 2],
    u64,
    Result<migration::RemoteFrame>,
) {
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let root = room.root().clone();
    let seat_of = |p: u64| root.members.iter().find(|m| m.player == p).unwrap().seat;
    let gone = [seat_of(room.player(0)), seat_of(room.player(3))];
    let seats = [seat_of(room.player(1)), seat_of(room.player(2))];
    let (first, link) = room.faulty_remote(1);
    let mut remotes = [first, room.remote(2)];
    let mut index = 0;
    while index < 30 {
        index += 1;
        let now = room.base + index * STEP;
        for remote in &mut remotes {
            remote.frame(now, room.wall(), Input::default()).unwrap();
        }
        room.commit(index);
    }
    // One lap, and only the two remote humans race.
    index += 1;
    let now = room.base + index * STEP;
    for remote in &mut remotes {
        remote.frame(now, room.wall(), Input::default()).unwrap();
    }
    room.commit_op(index, now, None, |h| {
        if gone.iter().all(|s| h.kick(*s)) && h.set_laps(1) {
            Ok(())
        } else {
            Err(Error::Invalid("lobby setup"))
        }
    });
    assert_eq!(room.live.failure(), None, "the lobby setup paused the room");
    room.start_at = index + 2;
    let mut armed = false;
    loop {
        index += 1;
        let now = room.base + index * STEP;
        let input = autopilot(room.live.authority(), seats[1]);
        remotes[1].frame(now, room.wall(), input).unwrap();
        let input = autopilot(room.live.authority(), seats[0]);
        let result = remotes[0].frame(now, room.wall(), input);
        if armed {
            return (room, remotes, index, result);
        }
        result.unwrap();
        room.commit(index);
        let authority = room.live.authority();
        let finished = match (authority.world(), authority.racer_of(seats[0])) {
            (Some(w), Some(i)) => w.racers.get(i).is_some_and(|r| r.finished_at.is_some()),
            _ => false,
        };
        if finished {
            link.read_ahead();
            link.corrupt_next();
            armed = true;
        }
        assert!(index < 6000, "the race never reached a finish");
    }
}

/// A frame which commits a step with host events and then fails (here on a
/// corrupt wire after the output carrying the client's `Finished`) still
/// leaves that step's events on the view: the output is applied for good, a
/// replay of it is a duplicate, and the app carries on after the error (the
/// takeover, or the host's and successor's own seat).
#[test]
fn a_frame_failing_part_way_keeps_the_events_of_its_committed_steps() {
    let (_room, remotes, _, result) = armed_frame_failing_after_a_finish();
    assert!(
        result.is_err(),
        "the corrupt wire did not fail the frame: {result:?}"
    );
    assert!(remotes[0].endpoint().failure().is_some());
    let mut end = RaceEnd::default();
    end.observe(remotes[0].client());
    assert_eq!(
        end.finished, 1,
        "a failed frame lost or repeated its committed finish"
    );
}

/// A failure latched in an earlier frame fails the next one at once, with an
/// empty view: the app already read the events of the frame which failed.
#[test]
fn a_frame_after_a_latched_failure_leaves_no_events_to_read_again() {
    let (room, mut remotes, index, result) = armed_frame_failing_after_a_finish();
    assert!(result.is_err());
    let events = |remote: &migration::RemotePlayer| remote.client().view().unwrap().events.len();
    assert!(events(&remotes[0]) > 0, "the failed frame surfaced nothing");
    let now = room.base + (index + 1) * STEP;
    assert!(remotes[0]
        .frame(now, room.wall(), Input::default())
        .is_err());
    assert_eq!(
        events(&remotes[0]),
        0,
        "the app would read the failed frame's events again"
    );
}

/// A pump which runs no frame for a seat (its link was lost, the standby is
/// watched) must not leave the last frame's events to be read again: the app
/// reads the view on every pump, and a failed frame's committed events stay
/// on it.
#[test]
fn a_pump_that_runs_no_frame_leaves_no_events_to_read_again() {
    let (_room, mut remotes, _, result) = armed_frame_failing_after_a_finish();
    assert!(result.is_err());
    let events = |remote: &migration::RemotePlayer| remote.client().view().unwrap().events.len();
    assert!(events(&remotes[0]) > 0, "the failed frame surfaced nothing");
    // The pump which ran the failed frame was read; the next one runs none.
    remotes[0].discard_view_events();
    assert_eq!(
        events(&remotes[0]),
        0,
        "a pump with no frame left the failed frame's events to be read again"
    );
}

// The original learns every standby-signed membership certificate over the
// pair link: index 0 from activation, then one after each receipt (on a frame
// after that receipt's `Committed`), each following the previous one and
// byte for byte the standby's own. A player's `Leave` changes the membership
// once: the certificate drops the seat and moves the revision by one. If the
// Leave's batch frames the leaver nothing, that happens at the Leave's own
// batch; if it addresses the leaver an output (a Pong for a Ping in the same
// frame), the leaver stays a seatless member for that batch and the change
// comes one batch later.
#[test]
fn original_learns_each_membership_certificate_over_the_pair_link() {
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let root = room.root().clone();
    let standby_bytes = |room: &ArmedRoom| {
        room.standby
            .membership_certificate()
            .unwrap()
            .encode(&root)
            .unwrap()
    };
    let first = room.live.membership_certificate().unwrap().clone();
    assert_eq!(first.receipt.index, 0);
    assert_eq!(first.receipt, *room.live.primary().receipt());
    assert_eq!(first.encode(&root).unwrap(), standby_bytes(&room));
    let leaver = room.player(1);
    let mut remote = Some(room.remote(1));
    let mut _left = None;
    let mut leave_number = None;
    // The Leave's batch, and whether it addressed the leaver an output.
    let mut departed: Option<(u64, bool)> = None;
    let mut previous = first.clone();
    let mut learned = 1;
    let mut changed_at = None;
    let mut index = 0;
    while changed_at.is_none() || index < changed_at.unwrap() + 3 {
        index += 1;
        assert!(index <= 40, "the Leave never changed the membership");
        let now = room.base + index * STEP;
        if let Some(r) = remote.as_mut() {
            let leave = index >= 3;
            let step = r
                .frame_with(now, room.wall(), remote_input(index, 0), |c| {
                    if leave {
                        c.leave();
                    }
                })
                .unwrap();
            if leave {
                leave_number = step
                    .intents
                    .iter()
                    .find(|(_, b)| {
                        race_session::proto::decode::<race_session::proto::ToHost>(b)
                            == Some(race_session::proto::ToHost::Leave)
                    })
                    .map(|(n, _)| *n);
                assert!(leave_number.is_some(), "the Leave was not journaled");
                // Kept open (never framed again): shutting the QUIC client
                // down at once can discard the Leave before it is sent.
                _left = remote.take();
            }
        }
        let outputs = room.commit(index);
        assert!(room.live.authority().world().is_none(), "the race started");
        let mark = room
            .live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .find(|m| m.player == leaver)
            .unwrap()
            .clone();
        if departed.is_none() && leave_number.is_some_and(|n| mark.input >= n) {
            let addressed = outputs.iter().any(|o| o.output.recipient == leaver);
            departed = Some((index, addressed));
        }
        let cert = room.live.membership_certificate().unwrap().clone();
        assert_eq!(cert.receipt.index, index);
        assert_eq!(cert.receipt, *room.live.primary().receipt());
        cert.follows(&root, &previous).unwrap();
        assert_eq!(cert.encode(&root).unwrap(), standby_bytes(&room));
        if cert.revision != previous.revision {
            assert!(changed_at.is_none(), "membership changed twice");
            assert_eq!(cert.revision, previous.revision + 1);
            assert!(previous.members.iter().any(|m| m.player == leaver));
            assert!(!cert.members.iter().any(|m| m.player == leaver));
            assert!(mark.retired, "the leaver's watermark was not retired");
            changed_at = Some(index);
        }
        if departed == Some((index, true)) {
            // The seatless window: still a member, mark unretired.
            assert_eq!(cert.revision, previous.revision);
            assert!(cert.members.iter().any(|m| m.player == leaver));
            assert!(!mark.retired);
        }
        learned += 1;
        previous = cert;
    }
    let changed_at = changed_at.unwrap();
    let (departed_at, addressed) = departed.expect("the Leave was never handled");
    assert_eq!(
        changed_at,
        departed_at + u64::from(addressed),
        "the membership change is not where the Leave's batch puts it"
    );
    assert_eq!(previous.revision, first.revision + 1);
    println!("STAMPEDE ORIGINAL MEMBERSHIP OVER PAIR LINK certificates_learned={learned} final_index={index} leave_committed_at={departed_at} leaver_addressed={addressed} membership_changed_at={changed_at} revision={}->{} every_certificate_equals_standby=true", first.revision, previous.revision);
}

// A `Leave` and the connection's close reach the original in the same
// network batch: the frame the closing connection already delivered is read
// into the player's queue before its route is parked, so the signed `Leave`
// is committed (the seat departs) although the player never reconnects.
#[test]
fn a_leave_followed_at_once_by_the_drop_is_committed() {
    let mut room = ArmedRoom::new();
    // In the armed lobby: a race-start batch could also frame the leaver an
    // output, which keeps it a seatless member one batch longer.
    room.start_at = u64::MAX;
    let leaver = room.player(1);
    let mut remote = room.remote(1);
    room.commit(1);
    // Keep the pair link alive but hold the player transport unread.
    let idle = |room: &mut ArmedRoom, ms: u64| {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(ms) {
            for event in room.live.poll().unwrap() {
                assert!(matches!(event, migration::RuntimeEvent::Membership(_)));
            }
            room.standby.poll().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let now = room.base + 2 * STEP;
    let step = remote
        .frame_with(now, room.wall(), remote_input(2, 0), |c| c.leave())
        .unwrap();
    let leave = step
        .intents
        .iter()
        .find(|(_, b)| {
            race_session::proto::decode::<race_session::proto::ToHost>(b)
                == Some(race_session::proto::ToHost::Leave)
        })
        .map(|(n, _)| *n)
        .expect("the Leave was journaled and sent");
    // Alone: no other intent (a clock Ping's Pong) shares the Leave's batch.
    assert_eq!(
        step.intents.len(),
        1,
        "the Leave is the leaver's only intent"
    );
    // The Leave frame reaches the host's event queue, then the connection
    // drops; both wait there unread and arrive in one poll.
    idle(&mut room, 300);
    drop(remote);
    idle(&mut room, 300);
    let received = room.players.ingress().received_frames;
    let wall = room.wall();
    room.players.poll(&mut room.live, wall).unwrap();
    assert!(
        room.players.ingress().received_frames > received,
        "the Leave frame was not in this poll's batch"
    );
    assert_eq!(
        room.players.peer(leaver),
        None,
        "the drop was not in this batch"
    );
    let queued = room
        .players
        .queue(leaver)
        .unwrap()
        .pending()
        .any(|(n, _)| n == leave);
    assert!(queued, "the Leave was lost with the connection");
    let mut index = 2;
    while room.image().members.iter().any(|m| m.player == leaver) {
        assert!(index < 6, "the Leave was never committed");
        room.commit(index);
        index += 1;
    }
    println!("STAMPEDE LEAVE THEN DROP leave_intent={leave} queued_before_park={queued} departed_by_index={}", index - 1);
}

// A refused `PlayerHost::new` hands the listener back: the build's first
// attempt names another session and is refused, the same listener is retried
// (and again on `Gap`/`Stale` until the original's certificate is current),
// and the retried listener then admits the room's players, who play.
#[test]
fn a_refused_player_host_returns_the_listener_for_a_retry() {
    let (mut room, _) = ArmedRoom::build_with(None, false, true);
    let first = room.pin_refusals[0].clone();
    assert!(!matches!(first, Error::Gap | Error::Stale), "{first:?}");
    assert_eq!(
        room.players.players().count(),
        3,
        "players joined the retried listener"
    );
    let player = room.player(1);
    let mut remote = room.remote(1);
    for index in 1..=3 {
        let now = room.base + index * STEP;
        remote
            .frame(now, room.wall(), remote_input(index, 0))
            .unwrap();
        room.commit(index);
    }
    assert!(room.players.attached(player));
    println!(
        "STAMPEDE PLAYER HOST RETRY refusals={:?} players_admitted=3 commits=3",
        room.pin_refusals
    );
}

// A reliable output addressed to a player whose `Leave` is handled in the
// same batch is kept (decided 2026-10-17). The racing leaver's clock Ping and
// then its Leave reach one batch; the host answers the Ping (a Pong to the
// leaver) and then removes the seat. The leaver stays a seatless member with
// an unretired watermark through that batch, so the Pong enters the
// replicated outbox like any other output and the room does not pause. The
// very next batch retires it (the recipient-retirement tombstone): the
// member and its outbox records are gone and the membership revision moves.
#[test]
fn an_output_to_a_player_leaving_in_the_same_batch_is_kept_and_retired_by_the_next_batch() {
    use race_session::proto::{decode, ToHost};
    let mut room = ArmedRoom::new();
    let leaver = room.player(1);
    let mut remote = room.remote(1);
    room.commit(1);
    // Frame the racing client (no stage) until its clock round sends a Ping.
    let t = Instant::now();
    let mut frame = 2;
    let ping = loop {
        let now = room.base + 2 * STEP;
        let step = remote
            .frame(now, room.wall(), remote_input(frame, 0))
            .unwrap();
        frame += 1;
        if let Some((n, _)) = step
            .intents
            .iter()
            .find(|(_, b)| matches!(decode::<ToHost>(b), Some(ToHost::Ping { .. })))
        {
            break *n;
        }
        room.poll_players();
        bounded(t);
    };
    let step = remote
        .frame_with(
            room.base + 2 * STEP,
            room.wall(),
            remote_input(frame, 0),
            |c| c.leave(),
        )
        .unwrap();
    let leave = step
        .intents
        .iter()
        .find(|(_, b)| decode::<ToHost>(b) == Some(ToHost::Leave))
        .map(|(n, _)| *n)
        .unwrap();
    // Both reach the leaver's queue before the next batch is staged.
    let t = Instant::now();
    loop {
        room.poll_players();
        let pending: Vec<u64> = room
            .players
            .queue(leaver)
            .unwrap()
            .pending()
            .map(|(n, _)| n)
            .collect();
        if pending.contains(&ping) && pending.contains(&leave) {
            break;
        }
        bounded(t);
    }
    let revision = room.live.membership_certificate().unwrap().revision;
    let mark = |room: &ArmedRoom| {
        room.live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .find(|m| m.player == leaver)
            .cloned()
            .unwrap()
    };
    let humans = room.live.authority().human_count();
    // Batch N: the Ping and the Leave are handled together and commit.
    room.commit(2);
    assert_eq!(room.live.failure(), None, "the original latched");
    assert_eq!(room.handled(leaver), leave, "the Leave was not handled");
    assert!(!mark(&room).retired, "the leaver's watermark was retired");
    assert!(
        room.image().members.iter().any(|m| m.player == leaver),
        "the leaver is no longer a member"
    );
    assert_eq!(
        room.live.authority().human_count(),
        humans - 1,
        "the leaver still holds a native seat"
    );
    let pongs: Vec<_> = room
        .live
        .primary()
        .outbox()
        .filter(|r| r.output.recipient == leaver && r.output.delivery == Delivery::Reliable)
        .filter_map(|r| pong_of(&r.output.bytes))
        .collect();
    assert!(!pongs.is_empty(), "the Pong to the leaver was dropped");
    let cert = room.live.membership_certificate().unwrap().clone();
    assert_eq!(cert.revision, revision, "batch N moved the revision");
    assert!(cert.members.iter().any(|m| m.player == leaver));
    // During its one-batch window the leaver left explicitly: its client
    // offers no resume proof, and the original refuses a reconnect of its
    // key instead of challenging it.
    let id = room.identities[1].clone();
    assert_eq!(
        remote.reconnect(room.session_dial(&id)),
        Err(Error::Closed),
        "a left client offered a resume proof"
    );
    drop(remote);
    let refused = room.players.ingress().refused_peers;
    let mut link = room.session_dial(&id);
    let t = Instant::now();
    while room.players.ingress().refused_peers == refused {
        room.poll_players();
        if let Some(ClientNetEvent::Failed(_)) = link.poll() {
            // The leaver's old connection is still closing: dial again.
            link = room.session_dial(&id);
        }
        bounded(t);
    }
    assert_eq!(room.players.peer(leaver), None, "the leaver was challenged");
    drop(link);
    // Batch N+1 addresses the leaver nothing and retires it.
    room.commit(3);
    assert_eq!(room.live.failure(), None);
    assert!(
        mark(&room).retired,
        "the next batch did not retire the leaver"
    );
    assert!(!room.image().members.iter().any(|m| m.player == leaver));
    assert!(
        room.outbox(leaver).is_empty(),
        "the leaver's outbox was kept"
    );
    let cert = room.live.membership_certificate().unwrap().clone();
    assert_eq!(cert.revision, revision + 1);
    assert!(!cert.members.iter().any(|m| m.player == leaver));
    assert!(
        room.players.queue(leaver).is_none(),
        "the leaver's route was kept"
    );
    // The room keeps committing.
    for index in 4..=6 {
        room.commit(index);
    }
    assert_eq!(room.live.failure(), None);
    println!(
        "STAMPEDE SAME-BATCH LEAVE OUTPUT ping={ping} leave={leave} pongs_kept={} reconnect_refused=true revision={revision}->{} retired_at=3 commits=6",
        pongs.len(),
        revision + 1
    );
}

// A committed departure that changes the membership reaches the pinned
// listener and the leaver's route. The leaver sends `Leave` and keeps its
// connection open. The commit that retires it closes its queue; the
// standby's certificate for that receipt (revision + 1) is then installed
// with `update_session_membership`, which kicks the lingering link (the
// player host disconnects nobody itself), and on the route, which is
// removed. A redial of the retired key is refused by the listener before it
// reaches the player host. The survivors keep their links and play on.
#[test]
fn a_membership_changing_departure_kicks_the_lingering_link_and_closes_the_route() {
    use race_session::proto::{decode, ToHost};
    let mut room = ArmedRoom::new();
    // In the armed lobby, so the leaver's batch frames it nothing.
    room.start_at = u64::MAX;
    let leaver = room.player(1);
    let (mut remote, link, _) = room.tapped_remote(1);
    let mut survivors = [room.remote(2), room.remote(3)];
    let kept = [room.player(2), room.player(3)];
    let mut index = 0;
    let mut native_equal = play_round(&mut room, &mut survivors, &mut index);
    let step = remote
        .frame_with(
            room.base + index * STEP,
            room.wall(),
            remote_input(index, 0),
            |c| c.leave(),
        )
        .unwrap();
    let leave = step
        .intents
        .iter()
        .find(|(_, b)| decode::<ToHost>(b) == Some(ToHost::Leave))
        .map(|(n, _)| *n)
        .expect("the Leave was journaled and sent");
    let t = Instant::now();
    while !room
        .players
        .queue(leaver)
        .unwrap()
        .pending()
        .any(|(n, _)| n == leave)
    {
        room.poll_players();
        bounded(t);
    }
    let revision = room.live.membership_certificate().unwrap().revision;
    let peers = kept.map(|p| room.players.peer(p).unwrap());
    let before = room.players.ingress();
    while room.image().members.iter().any(|m| m.player == leaver) {
        assert!(index < 8, "the Leave was never committed");
        native_equal &= play_round(&mut room, &mut survivors, &mut index);
    }
    // `commit` returned after `certify` installed this receipt's certificate.
    let cert = room.live.membership_certificate().unwrap().clone();
    assert_eq!(
        cert.revision,
        revision + 1,
        "the departure kept the revision"
    );
    assert!(!cert.members.iter().any(|m| m.player == leaver));
    let route_closed =
        room.players.queue(leaver).is_none() && !room.players.players().any(|p| p == leaver);
    assert!(
        route_closed,
        "the retired player's route outlived the membership change"
    );
    // The lingering link is kicked by the listener's membership update.
    let t = Instant::now();
    let mut kicked = false;
    while !kicked && t.elapsed() < Duration::from_millis(3000) {
        room.poll_players();
        while let Some(event) = link.lock().unwrap().poll() {
            if matches!(
                event,
                ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
            ) {
                kicked = true;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let after = room.players.ingress();
    assert!(kicked, "the departed player's lingering link stayed open");
    assert_eq!(
        after.disconnected, before.disconnected,
        "the player host, not the listener, closed the link"
    );
    // A redial by the retired key never reaches the player host.
    let id = room.identities[1].clone();
    let mut dial = room.session_dial(&id);
    let t = Instant::now();
    let (mut connected, mut ended) = (false, false);
    while !ended && t.elapsed() < Duration::from_millis(3000) {
        room.poll_players();
        while let Some(event) = dial.poll() {
            match event {
                ClientNetEvent::Connected { .. } => connected = true,
                ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. } => ended = true,
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let redial_refused =
        ended && !connected && room.players.ingress().refused_peers == after.refused_peers;
    assert!(
        redial_refused,
        "the retired key reached the player host (connected={connected} ended={ended})"
    );
    // The survivors keep their links and play on.
    for _ in 0..10 {
        native_equal &= play_round(&mut room, &mut survivors, &mut index);
    }
    let survivors_kept = kept
        .iter()
        .zip(peers)
        .all(|(&p, peer)| room.players.peer(p) == Some(peer) && room.players.attached(p))
        && room.players.ingress().disconnected == before.disconnected;
    assert!(survivors_kept, "a survivor's link was disturbed");
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    println!(
        "STAMPEDE MEMBERSHIP DEPARTURE leave={leave} revision={revision}->{} route_closed={route_closed} lingering_link_kicked={kicked} host_disconnects={} redial_refused={redial_refused} survivors_kept={survivors_kept} commits={index}",
        cert.revision,
        after.disconnected - before.disconnected,
    );
}

// A term-0 link drop parks the player and the native silence rule decides
// (decision of 2026-10-17): the native authority is never fed the unarmed
// `Left`. A racer's link drops mid-race; its route parks and it stays
// seated while it sends nothing. Once it has been silent for `SILENCE_MS`
// of game time inside armed batches, `drop_silent` removes it natively (its
// racer becomes a bot), and that departure commits through the membership
// plumbing: the member retires, the revision moves, `certify` installs the
// certificate on the listener and the route is removed. The survivors keep
// their links and race on. The runner stamped the park, and the silence rule
// wins well before the 30 s parked departure (`PARKED_DEPARTURE_MS`): the
// racer keeps its seat until it retires (a kick would leave it seatless for
// a batch) and the park ends with the membership.
#[test]
fn a_parked_racer_silent_for_six_seconds_departs_by_the_native_silence_rule() {
    use race_core::RacerKind;
    use race_session::host::SILENCE_MS;
    let mut room = ArmedRoom::new();
    let leaver = room.player(1);
    let (mut remote, link, _) = room.tapped_remote(1);
    let mut survivors = [room.remote(2), room.remote(3)];
    let kept = [room.player(2), room.player(3)];
    let mut index = 0;
    let mut native_equal = true;
    // Race with all three until the leaver's racer runs.
    while room.racer(1).is_none() {
        assert!(index < 300, "the race never started");
        let now = room.base + (index + 1) * STEP;
        remote
            .frame(now, room.wall(), remote_input(index + 1, 0))
            .unwrap();
        native_equal &= play_round(&mut room, &mut survivors, &mut index);
    }
    let seat = room
        .root()
        .members
        .iter()
        .find(|m| m.player == leaver)
        .unwrap()
        .seat;
    let racer = room.live.authority().racer_of(seat).unwrap();
    let revision = room.live.membership_certificate().unwrap().revision;
    let peers = kept.map(|p| room.players.peer(p).unwrap());
    let before = room.players.ingress();
    // The link drops: the route parks, nothing native happens.
    let dropped_at = room.base + index * STEP;
    link.lock().unwrap().close();
    drop(remote);
    let t = Instant::now();
    while room.players.peer(leaver).is_some() {
        room.poll_players();
        bounded(t);
    }
    let parked_at = room
        .live
        .parked_at(leaver)
        .expect("the runner did not stamp the park");
    // Other silent seats (the standby device's own, never driven here) may
    // depart by the same rule meanwhile: the revision is checked across the
    // leaver's own departure commit.
    let mut revision = revision;
    let mut parked_while_seated = true;
    while room.image().members.iter().any(|m| m.player == leaver) {
        assert!(
            room.base + index * STEP < dropped_at + SILENCE_MS + 2000,
            "the silent racer was never dropped"
        );
        parked_while_seated &= room.players.queue(leaver).is_some()
            && room.players.peer(leaver).is_none()
            && room.live.authority().racer_of(seat) == Some(racer);
        revision = room.live.membership_certificate().unwrap().revision;
        native_equal &= play_round(&mut room, &mut survivors, &mut index);
    }
    let departed_at = room.base + index * STEP;
    let silent_ms = departed_at - dropped_at;
    let parked_ms = room.wall() - parked_at;
    let unparked = room.live.parked_at(leaver).is_none();
    let authority = room.live.authority();
    let botted = authority.phase() == race_session::HostPhase::Racing
        && authority.racer_of(seat).is_none()
        && authority
            .world()
            .is_some_and(|w| matches!(w.cfg.racers[racer].kind, RacerKind::Bot(_)));
    let cert = room.live.membership_certificate().unwrap().clone();
    let retired = cert.revision == revision + 1
        && !cert.members.iter().any(|m| m.player == leaver)
        && room
            .live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .any(|m| m.player == leaver && m.retired);
    let route_removed =
        room.players.queue(leaver).is_none() && !room.players.players().any(|p| p == leaver);
    for _ in 0..10 {
        native_equal &= play_round(&mut room, &mut survivors, &mut index);
    }
    let survivors_kept = kept
        .iter()
        .zip(peers)
        .all(|(&p, peer)| room.players.peer(p) == Some(peer) && room.players.attached(p))
        && room.players.ingress().disconnected == before.disconnected;
    println!(
        "STAMPEDE SILENCE RULE DEPARTURE silent_game_ms={silent_ms} parked_runner_ms={parked_ms} unparked={unparked} parked_while_seated={parked_while_seated} botted={botted} revision={revision}->{} retired={retired} route_removed={route_removed} survivors_kept={survivors_kept} commits={index}",
        cert.revision
    );
    assert!(parked_while_seated, "the drop itself removed the racer");
    assert!(
        silent_ms > SILENCE_MS,
        "dropped after {silent_ms} ms, before the silence rule"
    );
    assert!(
        parked_ms < migration::PARKED_DEPARTURE_MS,
        "the 30 s parked departure could have fired ({parked_ms} ms)"
    );
    assert!(unparked, "the departed racer is still parked");
    assert!(botted, "the silent racer did not become a bot");
    assert!(retired, "the silence departure did not retire the member");
    assert!(
        route_removed,
        "the silence-departed player's route outlived the membership change"
    );
    assert!(survivors_kept, "a survivor's link was disturbed");
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// One armed round: frame each connected remote at the round's game time,
/// commit, and compare the original's native authority with the standby's.
fn armed_round(
    room: &mut ArmedRoom,
    remotes: &mut [&mut migration::RemotePlayer],
    index: &mut u64,
) -> bool {
    *index += 1;
    let now = room.base + *index * STEP;
    for (r, remote) in remotes.iter_mut().enumerate() {
        let step = remote.frame(now, room.wall(), remote_input(*index, r));
        assert!(step.is_ok(), "remote {r} refused a frame: {:?}", step.err());
    }
    room.commit(*index);
    room.native_equals_standby()
}

/// The committed outputs to `player` still in the outbox that are its
/// `Closed("kicked")`.
fn kicked_outputs(room: &ArmedRoom, player: u64) -> usize {
    use race_session::proto::{decode, ToClient};
    room.live
        .primary()
        .outbox()
        .filter(|r| r.output.recipient == player)
        .filter(|r| decode::<ToClient>(&r.output.bytes) == Some(ToClient::Closed("kicked".into())))
        .count()
}

// The 30 s parked departure (owner decision): a player parked for longer
// than `PARKED_DEPARTURE_MS` in an armed room is departed by the host, as an
// explicit host-side departure like a kick, counted from the park on the
// runner's clock. In the armed lobby (no silence rule) B's link drops, then
// A's. B reconnects within a second and resumes, which cancels its
// departure. A's replacement connection, which lands in the same poll as
// its drop, never proves its journals, so A stays parked. A stays seated
// while it has been parked for at
// most 30 s and departs by the native kick in the first batch staged after:
// its `Closed("kicked")` keeps it a seatless member for that batch, and the
// next batch retires it (revision + 1, outbox pruned, route removed by
// `certify`). B, parked earlier than A, is still seated and attached then,
// so the reconnect, not the timing, kept it. Real time: about 31 s.
#[test]
fn a_player_parked_past_thirty_seconds_departs_and_a_reconnect_before_cancels_it() {
    use migration::PARKED_DEPARTURE_MS;
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let (a, b) = (room.player(1), room.player(2));
    let (remote_a, link_a, _) = room.tapped_remote(1);
    let (mut remote_b, link_b, _) = room.tapped_remote(2);
    let mut survivor = room.remote(3);
    let mut index = 0;
    let mut native_equal = true;
    for _ in 0..3 {
        native_equal &= armed_round(&mut room, &mut [&mut remote_b, &mut survivor], &mut index);
    }
    let humans = room.live.authority().human_count();
    let revision = room.live.membership_certificate().unwrap().revision;
    let mark = |room: &ArmedRoom, player: u64| {
        room.live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .find(|m| m.player == player)
            .cloned()
            .unwrap()
    };
    // B's link drops, then A's: both routes park on the runner clock.
    link_b.lock().unwrap().close();
    let t = Instant::now();
    while room.players.peer(b).is_some() {
        room.poll_players();
        bounded(t);
    }
    let parked_b = room.live.parked_at(b).expect("B's park was not stamped");
    std::thread::sleep(Duration::from_millis(50));
    // A's link drops and a replacement for its key connects before the
    // player host is polled again, so the old link's `Left` and the
    // replacement's `Joined` land in one poll: the route goes from attached
    // straight to its challenge, never without a connection. The replacement
    // never proves its journals; the route is still parked.
    link_a.lock().unwrap().close();
    drop(remote_a);
    let mut replacement = room.session_dial(&room.identities[1].clone());
    let t = Instant::now();
    loop {
        match replacement.poll() {
            Some(ClientNetEvent::Connected { .. }) => break,
            Some(ClientNetEvent::Progress(_)) | None => {}
            other => panic!("A's replacement refused: {other:?}"),
        }
        bounded(t);
    }
    std::thread::sleep(Duration::from_millis(50));
    room.poll_players();
    assert!(
        room.players.peer(a).is_some() && !room.players.attached(a),
        "A's replacement did not take the route in the same poll"
    );
    let parked_a = room
        .live
        .parked_at(a)
        .expect("a replacement that has not proved its journals cancelled the park");
    assert!(parked_b < parked_a, "B did not park before A");
    for _ in 0..20 {
        native_equal &= armed_round(&mut room, &mut [&mut survivor], &mut index);
        std::thread::sleep(Duration::from_millis(20));
    }
    // B reconnects (same key, new connection) and proves its journals.
    let link = room.session_join(&room.identities[2].clone());
    remote_b.reconnect(link).unwrap();
    let t = Instant::now();
    while remote_b.resuming() || !room.players.attached(b) {
        native_equal &= armed_round(&mut room, &mut [&mut remote_b, &mut survivor], &mut index);
        bounded(t);
    }
    room.poll_players();
    let reconnected_after = room.wall() - parked_b;
    assert!(reconnected_after < PARKED_DEPARTURE_MS);
    assert_eq!(room.live.parked_at(b), None, "the reconnect kept B parked");
    assert_eq!(room.live.parked_at(a), Some(parked_a));
    // Play on until A's seat goes. While A holds it, no batch was staged
    // with A parked for longer than 30 s.
    let mut rounds = 0;
    let departed_ms = loop {
        let before = room.wall();
        native_equal &= armed_round(&mut room, &mut [&mut remote_b, &mut survivor], &mut index);
        if room.live.authority().human_count() < humans {
            break room.wall() - parked_a;
        }
        assert!(
            before - parked_a <= PARKED_DEPARTURE_MS,
            "A was still seated {} ms after its park",
            before - parked_a
        );
        rounds += 1;
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        departed_ms > PARKED_DEPARTURE_MS,
        "A departed {departed_ms} ms after its park"
    );
    assert_eq!(room.live.authority().human_count(), humans - 1);
    // The departure batch: the kick addressed A its Closed, so A stays a
    // seatless member with an unretired mark and the revision holds.
    let kicked = kicked_outputs(&room, a);
    let kept = room.image().members.iter().any(|m| m.player == a)
        && !mark(&room, a).retired
        && room.live.membership_certificate().unwrap().revision == revision
        && kicked == 1;
    assert!(
        kept,
        "the departure batch did not keep A seatless (kicked={kicked})"
    );
    let b_kept = room.image().members.iter().any(|m| m.player == b)
        && room.players.attached(b)
        && room.live.parked_at(b).is_none();
    assert!(b_kept, "B departed although it reconnected");
    // The next batch retires A.
    native_equal &= armed_round(&mut room, &mut [&mut remote_b, &mut survivor], &mut index);
    let cert = room.live.membership_certificate().unwrap().clone();
    let retired = mark(&room, a).retired
        && !room.image().members.iter().any(|m| m.player == a)
        && cert.revision == revision + 1
        && !cert.members.iter().any(|m| m.player == a)
        && room.outbox(a).is_empty()
        && room.players.queue(a).is_none()
        && room.live.parked_at(a).is_none();
    assert!(retired, "the next batch did not retire A");
    for _ in 0..5 {
        native_equal &= armed_round(&mut room, &mut [&mut remote_b, &mut survivor], &mut index);
    }
    assert!(room.players.attached(b) && room.players.attached(room.player(3)));
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    println!(
        "STAMPEDE PARKED DEPARTURE parked_ms_at_departure={departed_ms} seated_rounds={rounds} kicked_outputs={kicked} kept_seatless=true revision={revision}->{} retired={retired} b_reconnected_after_ms={reconnected_after} b_kept={b_kept} commits={index}",
        cert.revision
    );
}

// A host kick inside an armed batch, in the lobby and while racing, commits
// as an ordinary departure: the native kick removes the seat and addresses
// the player its `Closed("kicked")`, so the player stays a seatless member
// for that batch (keep-then-retire) and the next batch retires it (revision
// + 1, outbox pruned, route removed by `certify`). The room never pauses.
#[test]
fn a_host_kick_in_an_armed_batch_commits_as_a_departure() {
    for racing in [false, true] {
        let mut room = ArmedRoom::new();
        if !racing {
            room.start_at = u64::MAX;
        }
        let leaver = room.player(1);
        let seat = room
            .root()
            .members
            .iter()
            .find(|m| m.player == leaver)
            .unwrap()
            .seat;
        let mut survivors = [room.remote(2), room.remote(3)];
        let mut index = 0;
        let mut native_equal = true;
        for _ in 0..3 {
            native_equal &= play_round(&mut room, &mut survivors, &mut index);
        }
        while racing && room.live.authority().phase() != race_session::HostPhase::Racing {
            assert!(index < 300, "the race never started");
            native_equal &= play_round(&mut room, &mut survivors, &mut index);
        }
        let humans = room.live.authority().human_count();
        let revision = room.live.membership_certificate().unwrap().revision;
        let mark = |room: &ArmedRoom| {
            room.live
                .primary()
                .receipt()
                .watermarks
                .iter()
                .find(|m| m.player == leaver)
                .cloned()
                .unwrap()
        };
        index += 1;
        let now = room.base + index * STEP;
        for (r, remote) in survivors.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        room.commit_op(index, now, None, |h| {
            if h.kick(seat) {
                Ok(())
            } else {
                Err(Error::Invalid("kick"))
            }
        });
        native_equal &= room.native_equals_standby();
        assert_eq!(room.live.failure(), None, "the kick paused the room");
        let kicked = kicked_outputs(&room, leaver);
        let kept = room.image().members.iter().any(|m| m.player == leaver)
            && room.live.authority().human_count() == humans - 1
            && room.live.authority().racer_of(seat).is_none()
            && !mark(&room).retired
            && room.live.membership_certificate().unwrap().revision == revision
            && kicked == 1;
        assert!(
            kept,
            "the kick batch did not keep the leaver seatless (racing={racing})"
        );
        native_equal &= play_round(&mut room, &mut survivors, &mut index);
        let cert = room.live.membership_certificate().unwrap().clone();
        let retired = mark(&room).retired
            && !room.image().members.iter().any(|m| m.player == leaver)
            && cert.revision == revision + 1
            && !cert.members.iter().any(|m| m.player == leaver)
            && room.outbox(leaver).is_empty()
            && room.players.queue(leaver).is_none();
        assert!(
            retired,
            "the next batch did not retire the kicked player (racing={racing})"
        );
        for _ in 0..5 {
            native_equal &= play_round(&mut room, &mut survivors, &mut index);
        }
        assert!(native_equal);
        assert!(room.live.failure().is_none() && room.standby.failure().is_none());
        println!(
            "STAMPEDE KICK IN ARMED BATCH racing={racing} kicked_outputs={kicked} kept_seatless={kept} revision={revision}->{} retired={retired} commits={index}",
            cert.revision
        );
    }
}

// A native removal of the hosting authority's own seat, checked (session
// decision: document only). Twang refuses it with `Conflict`, so its room
// pauses. Stampede has no such gate: the phone host's kick of its own player
// in a term-0 armed batch commits like any departure. The kick addresses
// the own seat its `Closed("kicked")`, so it stays a seatless member for
// that batch; the next batch retires it, the in-process route is removed by
// `certify`, and the original keeps hosting with no seat, as the promoted
// standby does after its own seat's `Leave`. The 30 s parked departure
// never reaches this seat: its route is local and never parks.
#[test]
fn a_kick_of_the_hosts_own_seat_in_an_armed_batch_commits_and_the_original_hosts_on() {
    let (mut room, own_client) = ArmedRoom::phone();
    room.start_at = u64::MAX;
    let mut remotes = [room.remote(1), room.remote(2)];
    let root = room.root().clone();
    let member = root
        .members
        .iter()
        .find(|m| m.key == Some(root.host))
        .unwrap()
        .clone();
    let armed = room.standby.armed().clone();
    let ack =
        CapabilityAck::sign(&root, member.player, room.initial, 1, room.original.keys()).unwrap();
    let mut own = room
        .players
        .attach_own(
            &mut room.live,
            &armed,
            &ack,
            own_client,
            room.original.keys().clone(),
        )
        .unwrap();
    let mut index = 0;
    let mut native_equal = true;
    for _ in 0..3 {
        index += 1;
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        own.frame(now, room.wall(), remote_input(index, 0)).unwrap();
        room.commit(index);
    }
    let humans = room.live.authority().human_count();
    let revision = room.live.membership_certificate().unwrap().revision;
    let retired = |room: &ArmedRoom| {
        room.live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .any(|m| m.player == member.player && m.retired)
    };
    index += 1;
    let now = room.base + index * STEP;
    for (r, remote) in remotes.iter_mut().enumerate() {
        remote
            .frame(now, room.wall(), remote_input(index, r))
            .unwrap();
    }
    room.commit_op(index, now, None, |h| {
        if h.kick(member.seat) {
            Ok(())
        } else {
            Err(Error::Invalid("kick"))
        }
    });
    native_equal &= room.native_equals_standby();
    let kicked = kicked_outputs(&room, member.player);
    let kept = room.live.failure().is_none()
        && room.live.authority().human_count() == humans - 1
        && room
            .image()
            .members
            .iter()
            .any(|m| m.player == member.player)
        && !retired(&room)
        && room.live.membership_certificate().unwrap().revision == revision
        && kicked == 1;
    assert!(kept, "the own-seat kick did not commit as a kept departure");
    native_equal &= play_round(&mut room, &mut remotes, &mut index);
    let cert = room.live.membership_certificate().unwrap().clone();
    let departed = retired(&room)
        && !room
            .image()
            .members
            .iter()
            .any(|m| m.player == member.player)
        && cert.revision == revision + 1
        && room.players.queue(member.player).is_none();
    assert!(departed, "the next batch did not retire the own seat");
    for _ in 0..5 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    println!(
        "STAMPEDE OWN SEAT KICK committed=true kicked_outputs={kicked} kept_seatless={kept} retired_next={departed} revision={revision}->{} humans={humans}->{} paused=false commits={index}",
        cert.revision,
        room.live.authority().human_count()
    );
}

/// Benchmark, not a correctness check: what one signed player control costs
/// the armed original's frame thread in `PlayerHost::poll`. Every human sends
/// about 60 controls a second (one lossy view per tick), so eight humans need
/// about 480 a second on that thread. Fray's `PlayerQueue::receive` verified
/// each control twice, each time re-verifying the whole membership
/// certificate (about 0.7 ms per control in a release build), which took a
/// third of the thread; under load one poll that met a backlog ran past
/// 150 ms and the room paused on the 250 ms game-clock skew. One signature
/// check per control fits well inside the budget. Only polls that read
/// controls are timed. Release only.
#[test]
#[ignore = "release-only timing benchmark: cargo test --release -p race-session --test migration poll_costs -- --ignored"]
fn armed_host_poll_costs_at_most_a_quarter_millisecond_per_player_control() {
    const BUDGET_US: u128 = 250;
    if cfg!(debug_assertions) {
        println!("SKIPPED: armed_host_poll_costs… is a release-only benchmark");
        return;
    }
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2), room.remote(3)];
    let (mut spent, mut controls) = (Duration::ZERO, 0usize);
    for index in 1..=240 {
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        // Read this round's controls in timed polls, then commit.
        let mut quiet = Instant::now();
        while quiet.elapsed() < Duration::from_millis(10) {
            let before = room.players.ingress().accepted_controls;
            let t = Instant::now();
            room.poll_players();
            let took = t.elapsed();
            let read = room.players.ingress().accepted_controls - before;
            if read > 0 {
                spent += took;
                controls += read;
                quiet = Instant::now();
            }
        }
        room.commit(index);
    }
    assert!(controls >= 200, "only {controls} controls were timed");
    let per_control = spent.as_micros() / controls as u128;
    println!("ARMED HOST POLL controls={controls} per_control_us={per_control}");
    assert!(
        per_control <= BUDGET_US,
        "one player control costs the host poll {per_control} us (budget {BUDGET_US} us)"
    );
}

#[test]
fn remote_humans_play_an_armed_race_over_real_player_links() {
    let mut room = ArmedRoom::new();
    // About a second of armed lobby first, then the countdown and the race.
    const LOBBY: u64 = 30;
    room.start_at = LOBBY + 1;
    let a = room.remote(1);
    let (b, b_network, b_sent) = room.tapped_remote(2);
    let mut remotes = [a, b];
    let ids = [room.player(1), room.player(2)];
    let mut ledgers = [RemoteLedger::default(), RemoteLedger::default()];
    let mut committed: Vec<CertifiedOutput> = Vec::new();
    let mut native_equal = true;
    let mut receipt_before_apply = true;
    let mut drive = [NativeDrive::default(), NativeDrive::default()];
    let mut frozen_throttle = false;
    let mut frame = |room: &mut ArmedRoom,
                     remotes: &mut [migration::RemotePlayer; 2],
                     ledgers: &mut [RemoteLedger; 2],
                     index: u64| {
        let now = room.base + index * STEP;
        let state = match room.live.authority().world().map(|w| w.phase) {
            None => 0,
            Some(RacePhase::Countdown) => 1,
            Some(_) => 2,
        };
        for (r, remote) in remotes.iter_mut().enumerate() {
            let step = remote.frame(now, room.wall(), remote_input(index, r));
            assert!(step.is_ok(), "remote {r} refused a frame: {:?}", step.err());
            let step = step.unwrap();
            ledgers[r].frozen[state] += usize::from(step.frozen);
            ledgers[r].idle[state] += step.idle_progress;
            if step
                .newest_receipt
                .is_some_and(|n| n > room.standby.receipt().index)
            {
                receipt_before_apply = false;
            }
            assert!(receipt_before_apply, "output applied before its commit");
            let l = &mut ledgers[r];
            l.reliable_applied += step.reliable_applied;
            l.lossy_applied += step.lossy_applied;
            l.duplicates += step.duplicates;
            l.acks += usize::from(step.acked.is_some());
            for (number, bytes) in &step.intents {
                assert_eq!(*number, l.intents + 1, "intent numbers are contiguous");
                l.intents = *number;
                if let Some((id, _)) = ping_of(bytes) {
                    assert!(l.pings.insert(id, *number).is_none(), "ping sent twice");
                }
            }
        }
    };
    let record = |room: &ArmedRoom,
                  ledgers: &mut [RemoteLedger; 2],
                  outputs: &[CertifiedOutput],
                  committed: &mut Vec<CertifiedOutput>| {
        for output in outputs {
            let Some(r) = ids.iter().position(|p| *p == output.output.recipient) else {
                continue;
            };
            if let Some((id, _, _)) = pong_of(&output.output.bytes) {
                *ledgers[r].pongs.entry(id).or_default() += 1;
            }
            committed.push(output.clone());
        }
        for (r, &player) in ids.iter().enumerate() {
            let outbox = room.outbox(player).len();
            let l = &mut ledgers[r];
            if outbox < l.peak_outbox {
                l.outbox_shrank = true;
            }
            l.peak_outbox = l.peak_outbox.max(outbox);
        }
    };
    let mut index = 0;
    while index < LOBBY + ROUNDS {
        index += 1;
        frame(&mut room, &mut remotes, &mut ledgers, index);
        let outputs = room.commit(index);
        record(&room, &mut ledgers, &outputs, &mut committed);
        native_equal &= room.native_equals_standby();
        assert!(
            native_equal,
            "native state differs from the standby at {index}"
        );
        // The remote humans' views reach their own native racers.
        for (r, d) in drive.iter_mut().enumerate() {
            if let Some(racer) = room.racer(r + 1) {
                d.observe(racer, &remote_input(index, r));
            }
        }
        if let Some(racer) = room.racer(0) {
            frozen_throttle |= racer.prev_input.throttle != 0.0;
        }
    }
    for (r, d) in drive.iter().enumerate() {
        println!("remote {r} native: {d:?}");
    }
    let views_drive = drive.iter().all(|d| d.drove());
    assert!(
        views_drive,
        "remote views did not drive the native racers: {drive:?}"
    );
    assert!(!frozen_throttle, "the frozen standby seat got remote input");
    // Settle: every published output reaches its client, then the clients'
    // pending intents and ACKs reach the original before the last commit.
    let t = Instant::now();
    loop {
        frame(&mut room, &mut remotes, &mut ledgers, index);
        let ceilings = ids.map(|p| room.live.primary().output_ceiling(p, Delivery::Reliable));
        if (0..2).all(|r| remotes[r].endpoint().journals().0.applied() == ceilings[r]) {
            break;
        }
        room.poll_players();
        bounded(t);
    }
    let racing = remotes
        .iter()
        .all(|r| matches!(r.client().phase(), race_session::ClientPhase::Racing));
    assert!(racing, "remote clients did not reach the race");
    let t = Instant::now();
    loop {
        room.poll_players();
        let received = (0..2).all(|r| {
            let queue = room.players.queue(ids[r]).unwrap();
            room.handled(ids[r]) + queue.pending().count() as u64 == ledgers[r].intents
                && queue.output_ack() >= remotes[r].acked()
        });
        if received || t.elapsed() > Duration::from_secs(2) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    index += 1;
    let outputs = room.commit(index);
    record(&room, &mut ledgers, &outputs, &mut committed);
    native_equal &= room.native_equals_standby();

    let mut intents_once = true;
    let mut outputs_once = true;
    let mut acks_prune = true;
    for (r, &player) in ids.iter().enumerate() {
        let l = &ledgers[r];
        let remote = &remotes[r];
        // Every reliable intent the client journaled was applied once.
        intents_once &= room.handled(player) == l.intents
            && remote.endpoint().journals().1.tail() == l.intents
            && l.intents > 0;
        // Each Ping intent produced exactly one native Pong; no Pong without one.
        intents_once &= !l.pings.is_empty()
            && l.pings.keys().all(|id| l.pongs.get(id) == Some(&1))
            && l.pongs.keys().all(|id| l.pings.contains_key(id));
        // Each certified reliable output was applied once, in order.
        outputs_once &= l.reliable_applied as u64 == remote.endpoint().journals().0.applied()
            && l.reliable_applied > 0
            && l.duplicates == 0;
        // The committed ACK is the client's own, and pruned the outbox.
        let outbox = room.outbox(player);
        acks_prune &= l.acks > 0
            && room.output_ack(player) == remote.acked()
            && remote.acked() > 0
            && outbox.iter().all(|&n| n > remote.acked())
            && outbox.len() as u64
                == room
                    .live
                    .primary()
                    .output_ceiling(player, Delivery::Reliable)
                    - remote.acked()
            && l.outbox_shrank;
        println!(
            "remote {r}: intents={} pings={} reliable_applied={} lossy_applied={} acked={} outbox={} peak_outbox={} false_freezes_lobby_countdown_race={:?} certificate_progress_lobby_countdown_race={:?}",
            l.intents,
            l.pings.len(),
            l.reliable_applied,
            l.lossy_applied,
            remote.acked(),
            outbox.len(),
            l.peak_outbox,
            l.frozen,
            l.idle,
        );
    }
    // A healthy armed match never freezes a client: lobby (where term-0
    // detection is not armed), countdown and race.
    let false_freezes = ledgers.each_ref().map(|l| l.frozen);
    assert_eq!(
        false_freezes, [[0; 3]; 2],
        "false freezes in a healthy armed match"
    );
    assert!(intents_once, "a reliable intent was lost or applied twice");
    assert!(outputs_once, "an output was lost or applied twice");
    assert!(acks_prune, "Applied ACKs did not prune the outbox");
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());

    // Duplicate output at the transport: the original frames an output the
    // client already applied and ACKed. The endpoint treats it as a no-op.
    let a_old = committed
        .iter()
        .find(|o| o.output.recipient == ids[0] && o.output.delivery == Delivery::Reliable)
        .unwrap()
        .clone();
    assert!(a_old.output.sequence <= room.output_ack(ids[0]));
    // The last commit may itself have published a reliable output to A:
    // let A apply it first, so only the re-framed output remains.
    let ceiling = room
        .live
        .primary()
        .output_ceiling(ids[0], Delivery::Reliable);
    let t = Instant::now();
    while remotes[0].endpoint().journals().0.applied() < ceiling {
        remotes[0]
            .frame(room.base + index * STEP, room.wall(), Input::default())
            .unwrap();
        bounded(t);
    }
    let applied_before = remotes[0].endpoint().journals().0.applied();
    let wall = room.wall();
    assert_eq!(
        room.players
            .publish(&room.live, &[a_old], &[], wall)
            .unwrap(),
        1
    );
    let t = Instant::now();
    let mut duplicate_noop = false;
    while !duplicate_noop {
        let step = remotes[0]
            .frame(room.base + index * STEP, room.wall(), Input::default())
            .unwrap();
        assert_eq!(step.reliable_applied, 0, "a duplicate output applied again");
        duplicate_noop =
            step.duplicates == 1 && remotes[0].endpoint().journals().0.applied() == applied_before;
        bounded(t);
    }

    // Replay at the transport: B's physical frame carrying an already
    // applied Ping intent is sent again on the same connection.
    let root = room.root().clone();
    let (&ping_id, &ping_number) = ledgers[1].pings.iter().next().unwrap();
    let wire = b_sent
        .lock()
        .unwrap()
        .iter()
        .find(|wire| {
            Frame::decode(wire).is_ok_and(|frame| {
                frame.count == 1
                    && PlayerControl::decode(&root, &frame.payload).is_ok_and(|control| {
                        matches!(control.action, PlayerAction::Intent { number, .. } if number == ping_number)
                    })
            })
        })
        .cloned()
        .expect("the ping's physical frame");
    // A's frames above sent views and ACKs; drain them before the replay.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(100) {
        room.poll_players();
        std::thread::sleep(Duration::from_millis(1));
    }
    let ingress = room.players.ingress();
    let handled = room.handled(ids[1]);
    b_network.lock().unwrap().send_reliable(wire);
    // A late frame of A's (a view or ACK still in flight after the drain on
    // a loaded machine) is received and accepted; the replay is the one
    // frame received and not accepted. Wait for that, not for any frame.
    let unaccepted = |i: &migration::PlayerIngress| {
        (i.received_frames - ingress.received_frames)
            - (i.accepted_controls - ingress.accepted_controls)
    };
    let t = Instant::now();
    while unaccepted(&room.players.ingress()) == 0 {
        room.poll_players();
        bounded(t);
    }
    index += 1;
    let outputs = room.commit(index);
    let replay_pongs = outputs
        .iter()
        .filter(|o| o.output.recipient == ids[1])
        .filter(|o| pong_of(&o.output.bytes).is_some_and(|(id, _, _)| id == ping_id))
        .count();
    let after = room.players.ingress();
    println!(
        "replay: before={ingress:?} after={after:?} handled={handled}->{} pongs={replay_pongs}",
        room.handled(ids[1])
    );
    // The frame arrived; the channel's replay guard consumed it silently.
    let replay_noop = unaccepted(&after) == 1
        && after.refused_frames == ingress.refused_frames
        && after.parked_controls == ingress.parked_controls
        && room.handled(ids[1]) == handled
        && replay_pongs == 0;
    native_equal &= room.native_equals_standby();
    assert!(replay_noop, "a replayed intent frame had an effect");
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    println!(
        "player_transport remotes=2 commits={index} racing={racing} views_drive={views_drive} native_equal={native_equal} receipt_before_apply={receipt_before_apply} intents_once={intents_once} outputs_once={outputs_once} acks_prune={acks_prune} duplicate_output_noop={duplicate_noop} replayed_intent_noop={replay_noop} refused_frames={}",
        room.players.ingress().refused_frames
    );
}

/// One armed frame for both remote humans, then commit the next batch.
fn play_round(
    room: &mut ArmedRoom,
    remotes: &mut [migration::RemotePlayer; 2],
    index: &mut u64,
) -> bool {
    *index += 1;
    let now = room.base + *index * STEP;
    for (r, remote) in remotes.iter_mut().enumerate() {
        let step = remote.frame(now, room.wall(), remote_input(*index, r));
        assert!(step.is_ok(), "remote {r} refused a frame: {:?}", step.err());
    }
    room.commit(*index);
    room.native_equals_standby()
}

/// An admitted member's connection sending wrong-session, oversized and
/// unsigned control frames, plus strangers on a session-bound and on an
/// ordinary join, never reach a `PlayerQueue` and never pause the room.
#[test]
fn hostile_and_unadmitted_player_peers_never_reach_the_queue_or_pause_the_room() {
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2)];
    let hostile_player = room.player(3);
    let mut hostile = room.networks[3].take().unwrap();
    let root = room.root().clone();
    // The admitted member's own client-side framing on its admitted link.
    let mut framing = hostile
        .migration_channel(&player_policy(&room.descriptor, &room.cert))
        .unwrap();
    let mut index = 0;
    let mut native_equal = true;
    for _ in 0..5 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    let before = room.players.ingress();
    let mut wires = Vec::new();
    let mut other = root.session;
    other[0] ^= 0xff;
    // Another session's frame.
    for frame in Frame::split(other, 0, 1, PacketKind::Control, &[1; 100]).unwrap() {
        wires.push(frame.encode().unwrap());
    }
    // A control transfer over the 4096-byte control cap, in valid frames.
    for frame in Frame::split(root.session, 0, 1, PacketKind::Control, &[2; 5000]).unwrap() {
        wires.push(frame.encode().unwrap());
    }
    // Correctly framed on its own bound channel, but no signed player control.
    framing.enqueue(PacketKind::Control, &[3; 300]).unwrap();
    while let Some(wire) = framing.poll_send(room.wall()).unwrap() {
        wires.push(wire);
    }
    let hostile_frames = wires.len();
    for wire in wires {
        hostile.send_reliable(wire);
    }
    // Over the 1024-byte message cap: QUIC drops it before the original.
    hostile.send_reliable(vec![4; race_session::proto::MAX_RELIABLE_LEN + 1]);
    hostile.send_lossy(vec![5; 64]);
    let t = Instant::now();
    while room.players.ingress().refused_frames < before.refused_frames + hostile_frames {
        room.poll_players();
        bounded(t);
    }
    // Strangers with valid My Signet approvals but no roster key.
    let binding = ConnectionBinding {
        session: root.session,
        descriptor: root.digest().unwrap(),
        term: 0,
        client: LinkRole::Player,
        server: LinkRole::Original,
    };
    let host_key = decented_net::PublicKey::from_slice(&root.host).unwrap();
    let mut strangers = [
        QuicClient::new(
            decented_net::join_session_addr(
                &room.cfg,
                &approved(),
                room.addr,
                &room.token,
                binding,
                host_key,
            )
            .unwrap(),
        ),
        QuicClient::new(decented_net::join_addr(
            &room.cfg,
            &approved(),
            room.addr,
            &room.token,
        )),
    ];
    let mut stranger_connected = false;
    let mut stranger_ended = [false; 2];
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(1500) && !stranger_ended.iter().all(|e| *e) {
        for (i, stranger) in strangers.iter_mut().enumerate() {
            while let Some(event) = stranger.poll() {
                match event {
                    ClientNetEvent::Connected { .. } => stranger_connected = true,
                    ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. } => {
                        stranger_ended[i] = true
                    }
                    _ => {}
                }
            }
        }
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    for _ in 0..5 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    let after = room.players.ingress();
    let queue = room.players.queue(hostile_player).unwrap();
    let hostile_reached_queue = queue.pending().count() != 0
        || queue.lossy().is_some()
        || queue.output_ack() != 0
        || room.handled(hostile_player) != 0;
    let paused = room.live.failure().is_some() || room.standby.failure().is_some();
    println!(
        "hostile_peers frames={hostile_frames} refused_frames={} ignored_datagrams={} stranger_connected={stranger_connected} stranger_ended={stranger_ended:?} players={} hostile_reached_queue={hostile_reached_queue} paused={paused} commits={index} native_equal={native_equal}",
        after.refused_frames - before.refused_frames,
        after.ignored_datagrams - before.ignored_datagrams,
        room.players.players().count(),
    );
    assert!(!stranger_connected, "an unadmitted peer was admitted");
    assert_eq!(room.players.players().count(), 3);
    assert!(!hostile_reached_queue, "hostile frames reached the queue");
    assert!(!paused, "a hostile peer paused the room");
    assert!(native_equal);
    assert!(room.players.peer(hostile_player).is_some());
}

/// Clients on every older game protocol (5 to 11, including the earlier
/// migration number 6) are refused with the existing version-mismatch error,
/// and a plain (non-migration) match still runs end to end over real QUIC at
/// the new version.
#[test]
fn old_version_client_is_refused_and_a_plain_match_runs_at_the_new_version() {
    use race_session::proto::{decode, encode, ToClient, ToHost};
    use race_session::PROTO_VERSION;
    assert_eq!(PROTO_VERSION, 12);
    const REFUSED: [u32; 7] = [11, 10, 9, 8, 7, 6, 5];
    let mut lobby = Lobby::<1>::with_humans();
    let (addr, token) = {
        let net = lobby.network.0.lock().unwrap();
        let h = net.handle().unwrap();
        (
            std::net::SocketAddr::from(([127, 0, 0, 1], h.local_addr().unwrap().port())),
            h.lan_token(),
        )
    };
    let started = Instant::now();
    let wall = || started.elapsed().as_millis() as u64;
    let mut refusals = Vec::new();
    for old_proto in REFUSED {
        let mut old = QuicClient::new(decented_net::join_addr(
            &lobby.cfg,
            &approved(),
            addr,
            &token,
        ));
        let mut refusal = None;
        while refusal.is_none() {
            lobby.host.update(wall());
            lobby.clients[0].update(wall(), Input::default());
            while let Some(event) = old.poll() {
                match event {
                    ClientNetEvent::Connected { .. } => old.send_reliable(encode(&ToHost::Hello {
                        proto: old_proto,
                        name: "Old".into(),
                        character: 0,
                    })),
                    ClientNetEvent::Reliable(bytes) => {
                        if let Some(ToClient::Closed(reason)) = decode(&bytes) {
                            refusal = Some(reason);
                        }
                    }
                    _ => {}
                }
            }
            bounded(started);
        }
        let refusal = refusal.unwrap();
        assert_eq!(
            refusal,
            format!("version mismatch (host 12, you {old_proto})")
        );
        assert_eq!(lobby.host.human_count(), 1, "the old client took a seat");
        refusals.push(old_proto);
    }
    let refusal = format!("{refusals:?}");
    assert_eq!(lobby.host.human_count(), 1, "the old client took a seat");
    assert!(lobby.host.start_race(wall()));
    let mut snapshots = 0;
    let mut last_tick = None;
    while snapshots < 30 {
        lobby.host.update(wall());
        let client = &mut lobby.clients[0];
        client.update(
            wall(),
            Input {
                throttle: 1.0,
                ..Input::default()
            },
        );
        assert!(
            !client.phase().is_dead(),
            "plain client: {:?}",
            client.phase()
        );
        let tick = client.view().map(|v| v.world.tick);
        if matches!(client.phase(), race_session::ClientPhase::Racing) && tick > last_tick {
            snapshots += 1;
            last_tick = tick;
        }
        bounded(started);
    }
    let plain_racing =
        matches!(lobby.clients[0].phase(), race_session::ClientPhase::Racing) && snapshots >= 30;
    assert!(plain_racing);
    println!(
        "version proto={PROTO_VERSION} old_refused={refusal} plain_racing={plain_racing} plain_ticks_seen={snapshots}"
    );
}

/// Fray's tagged SHA-256 (`fray/migration/...` labels), for building a
/// receipt the standby signed but this original never installed.
fn tagged(label: &[u8], bytes: &[u8]) -> Hash {
    use nostr::hashes::{sha256, Hash as _};
    let mut all = label.to_vec();
    all.extend(bytes);
    sha256::Hash::hash(&all).to_byte_array()
}

/// `output` alone under a receipt signed with the standby runtime key at
/// `term`/`index`. Its proof verifies; only the original's installed-receipt
/// gate can refuse it.
fn standby_signed(
    room: &ArmedRoom,
    base: &CertifiedOutput,
    term: u64,
    index: u64,
) -> CertifiedOutput {
    let context = room.root().context().unwrap();
    let output = base.output.clone();
    let mut leaf = index.to_be_bytes().to_vec();
    leaf.extend(0u32.to_be_bytes());
    leaf.extend(output.recipient.to_be_bytes());
    leaf.push(match output.delivery {
        Delivery::Reliable => 0,
        Delivery::Lossy => 1,
    });
    leaf.extend(output.sequence.to_be_bytes());
    leaf.extend((output.bytes.len() as u32).to_be_bytes());
    leaf.extend(&output.bytes);
    let mut receipt = base.receipt.clone();
    receipt.term = term;
    receipt.index = index;
    receipt.outputs_count = 1;
    receipt.outputs_root = tagged(b"fray/migration/output/v1\0", &leaf);
    let encoded = receipt.encode(&context).unwrap();
    let body = &encoded[..encoded.len() - 64];
    let digest = tagged(b"fray/migration/control/v1\0", body);
    receipt.signature = room.identities[0]
        .keys()
        .sign_schnorr(&nostr::secp256k1::Message::from_digest(digest))
        .serialize();
    let forged = CertifiedOutput {
        receipt,
        ordinal: 0,
        output,
        siblings: Vec::new(),
    };
    assert!(
        forged.verify(&context).is_ok(),
        "the standby-signed proof does not verify"
    );
    forged
}

/// `publish` frames only outputs proven by a receipt this original
/// installed. A validly signed output under a receipt ahead of the installed
/// one, or in another term, is refused before anything is sent, even next
/// to an installed output in the same call.
#[test]
fn publish_refuses_an_output_whose_receipt_is_not_installed() {
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2)];
    let a = room.player(1);
    let mut index = 0;
    let mut installed = None;
    while installed.is_none() {
        index += 1;
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        installed = room
            .commit(index)
            .into_iter()
            .find(|o| o.output.recipient == a);
        assert!(index < 200, "no output for A");
    }
    let installed = installed.unwrap();
    // A applies everything published so far.
    let ceiling = room.live.primary().output_ceiling(a, Delivery::Reliable);
    let t = Instant::now();
    while remotes[0].endpoint().journals().0.applied() < ceiling
        || t.elapsed() < Duration::from_millis(100)
    {
        remotes[0]
            .frame(room.base + index * STEP, room.wall(), Input::default())
            .unwrap();
        bounded(t);
    }
    let receipt = room.live.primary().receipt().clone();
    let ahead = standby_signed(&room, &installed, receipt.term, receipt.index + 1);
    let other_term = standby_signed(&room, &installed, receipt.term + 1, receipt.index);
    let wall = room.wall();
    let mut refused = Vec::new();
    for outputs in [
        vec![ahead.clone()],
        vec![other_term],
        vec![installed.clone(), ahead],
    ] {
        let result = room.players.publish(&room.live, &outputs, &[], wall);
        assert!(
            matches!(result, Err(Error::Stale)),
            "an output under an uninstalled receipt was framed: {result:?}"
        );
        refused.push(result.is_err());
    }
    // Nothing reached A: not even the installed output framed alongside.
    let t = Instant::now();
    let mut reached = migration::RemoteFrame::default();
    while t.elapsed() < Duration::from_millis(200) {
        let step = remotes[0]
            .frame(room.base + index * STEP, room.wall(), Input::default())
            .unwrap();
        reached.reliable_applied += step.reliable_applied;
        reached.lossy_applied += step.lossy_applied;
        reached.duplicates += step.duplicates;
        room.poll_players();
        std::thread::sleep(Duration::from_millis(2));
    }
    println!(
        "installed_gate installed_index={} refused={refused:?} reached_reliable={} reached_lossy={} reached_duplicates={}",
        receipt.index, reached.reliable_applied, reached.lossy_applied, reached.duplicates
    );
    assert_eq!(
        reached,
        migration::RemoteFrame::default(),
        "a refused publish sent bytes"
    );
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// A hostile second connection with a routed player's key never evicts the
/// live connection or bans the key. When that player's connection then
/// drops, it reconnects while armed, proves its retained journals and
/// keeps playing: every reliable intent is applied once and every output
/// reaches it.
#[test]
fn a_dropped_player_resumes_and_a_second_connection_evicts_nobody() {
    let mut room = ArmedRoom::new();
    let (a, a_network, _) = room.tapped_remote(1);
    let mut remotes = [a, room.remote(2)];
    let p = room.player(1);
    let mut index = 0;
    let mut native_equal = true;
    for _ in 0..30 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    // A second session-bound connection with the same runtime key.
    let live_peer = room.players.peer(p).unwrap();
    let before = room.players.ingress();
    let root = room.root().clone();
    let binding = ConnectionBinding {
        session: root.session,
        descriptor: root.digest().unwrap(),
        term: 0,
        client: LinkRole::Player,
        server: LinkRole::Original,
    };
    let host_key = decented_net::PublicKey::from_slice(&root.host).unwrap();
    let mut second = QuicClient::new(
        decented_net::join_session_addr(
            &room.cfg,
            &room.identities[1],
            room.addr,
            &room.token,
            binding,
            host_key,
        )
        .unwrap(),
    );
    let mut second_ended = false;
    let t = Instant::now();
    while !second_ended && t.elapsed() < Duration::from_millis(3000) {
        while let Some(event) = second.poll() {
            if matches!(
                event,
                ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
            ) {
                second_ended = true;
            }
        }
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    // A's live connection still drives its native racer once the race runs.
    let mut drive = NativeDrive::default();
    while drive.throttle < 10 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        if let Some(racer) = room.racer(1) {
            drive.observe(racer, &remote_input(index, 0));
        }
        assert!(index < 300, "A's views stopped: {drive:?}");
    }
    let after = room.players.ingress();
    let second_evicted_nobody = second_ended
        && room.players.peer(p) == Some(live_peer)
        && room.players.attached(p)
        && drive.own_steer == drive.racing_commits
        && after.banned == before.banned
        && after.disconnected == before.disconnected;
    println!(
        "second_connection ended={second_ended} peer_kept={} a_drives={drive:?} refused_peers={} banned={}",
        room.players.peer(p) == Some(live_peer),
        after.refused_peers - before.refused_peers,
        after.banned
    );
    assert!(
        second_evicted_nobody,
        "a second connection disturbed the live player"
    );

    // The live connection drops. B keeps playing; A's queue stays attached.
    let tail_before_drop = remotes[0].endpoint().journals().1.tail();
    a_network.lock().unwrap().close();
    let t = Instant::now();
    while room.players.peer(p).is_some() {
        room.poll_players();
        bounded(t);
    }
    for _ in 0..20 {
        index += 1;
        let now = room.base + index * STEP;
        remotes[1]
            .frame(now, room.wall(), remote_input(index, 1))
            .unwrap();
        room.commit(index);
        native_equal &= room.native_equals_standby();
    }
    // Same key, new connection: admitted (no ban), challenged, resumed.
    let link = room.session_join(&room.identities[1].clone());
    remotes[0].reconnect(link).unwrap();
    let t = Instant::now();
    while remotes[0].resuming() || !room.players.attached(p) {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        bounded(t);
    }
    for _ in 0..60 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    // Settle: A applies every output; its pending intents reach the queue.
    let t = Instant::now();
    loop {
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        let ceiling = room.live.primary().output_ceiling(p, Delivery::Reliable);
        let queue = room.players.queue(p).unwrap();
        if remotes[0].endpoint().journals().0.applied() == ceiling
            && room.handled(p) + queue.pending().count() as u64
                == remotes[0].endpoint().journals().1.tail()
            && queue.output_ack() >= remotes[0].acked()
        {
            break;
        }
        room.poll_players();
        bounded(t);
    }
    index += 1;
    room.commit(index);
    native_equal &= room.native_equals_standby();
    let tail = remotes[0].endpoint().journals().1.tail();
    let ingress = room.players.ingress();
    let resumed = ingress.resumed == 1
        && room.players.peer(p).is_some_and(|peer| peer != live_peer)
        && room.handled(p) == tail
        && tail > tail_before_drop
        && room.output_ack(p) == remotes[0].acked()
        && ingress.banned == 0;
    println!(
        "reconnect resumed={resumed} intents={tail_before_drop}->{tail} handled={} acked={} output_ack={} ingress={ingress:?} commits={index} native_equal={native_equal}",
        room.handled(p),
        remotes[0].acked(),
        room.output_ack(p),
    );
    assert!(resumed, "the dropped player did not resume losslessly");
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// More signed originals than a player queue holds, sent while no batch
/// commits: the original parks the control the full queue refused (its
/// frame is already consumed) and stops reading that route. Commits free
/// capacity and every original is applied once, with no gap.
#[test]
fn a_full_player_queue_parks_the_control_and_loses_no_intent() {
    use race_session::proto::{encode, ToHost};
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2)];
    let p = room.player(3);
    let mut link = room.networks[3].take().unwrap();
    let root = room.root().clone();
    let mut framing = link
        .migration_channel(&player_policy(&room.descriptor, &room.cert))
        .unwrap();
    let tls = framing.tls();
    let mut index = 0;
    let mut native_equal = true;
    for _ in 0..3 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    let before = room.players.ingress();
    let sent = MAX_JOURNAL_RECORDS as u64 + 40;
    for number in 1..=sent {
        // Ignored by the native race once it runs: no output, no state.
        let bytes = encode(&ToHost::SetAttire([(number % 4) as u8, 0, 0]));
        // Control sequence 1 is the player's signed activation vote.
        let control = PlayerControl::sign(
            &root,
            &room.cert,
            0,
            number + 1,
            p,
            tls,
            PlayerAction::Intent { number, bytes },
            room.identities[3].keys(),
        )
        .unwrap();
        framing
            .enqueue(PacketKind::Control, &control.encode(&root).unwrap())
            .unwrap();
        if number % 32 == 0 || number == sent {
            // Let the original read them (QuicHost holds 256 per peer).
            let t = Instant::now();
            while room.players.ingress().received_frames < before.received_frames + number as usize
            {
                while let Some(wire) = framing.poll_send(room.wall()).unwrap() {
                    link.send_reliable(wire);
                }
                room.poll_players();
                bounded(t);
            }
        }
    }
    room.poll_players();
    let full = room.players.ingress();
    let queued = room.players.queue(p).unwrap().pending().count();
    let parked = room.players.parked(p);
    println!(
        "full_queue sent={sent} queued={queued} parked={parked} parked_controls={} refused_frames={}",
        full.parked_controls - before.parked_controls,
        full.refused_frames - before.refused_frames
    );
    assert_eq!(queued, MAX_JOURNAL_RECORDS);
    assert!(parked, "the refused control was not parked");
    assert_eq!(full.parked_controls, before.parked_controls + 1);
    // Commits free capacity; every original is applied, none skipped.
    let mut rounds = 0;
    while room.handled(p) < sent && rounds < 300 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        rounds += 1;
    }
    let after = room.players.ingress();
    println!(
        "full_queue handled={} rounds={rounds} parked_controls={} refused_frames={} disconnected={} native_equal={native_equal}",
        room.handled(p),
        after.parked_controls - before.parked_controls,
        after.refused_frames - before.refused_frames,
        after.disconnected - before.disconnected
    );
    assert_eq!(room.handled(p), sent, "a capacity refusal lost an intent");
    assert_eq!(after.refused_frames, before.refused_frames);
    assert_eq!(after.disconnected, before.disconnected);
    assert!(room.players.peer(p).is_some() && !room.players.parked(p));
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// A room member's own session-bound link driven by hand: signed originals
/// and views on its own bound channel, with no `PlayerEndpoint`.
struct RawMember {
    link: QuicClient,
    framing: Channel,
    keys: decented_net::Keys,
    player: u64,
    root: Root,
    cert: MembershipCertificate,
    /// Control sequence 1 is the player's signed activation vote.
    sequence: u64,
    intent: u64,
    view: u64,
    /// Pacing clock for collected bursts.
    clock: u64,
    /// Wires released from the framing but not yet sent.
    backlog: Vec<Vec<u8>>,
}
impl RawMember {
    fn new(room: &mut ArmedRoom, i: usize) -> Self {
        let link = room.networks[i].take().unwrap();
        let framing = link
            .migration_channel(&player_policy(&room.descriptor, &room.cert))
            .unwrap();
        Self {
            link,
            framing,
            keys: room.identities[i].keys().clone(),
            player: room.player(i),
            root: room.root().clone(),
            cert: room.cert.clone(),
            sequence: 1,
            intent: 0,
            view: 0,
            clock: room.wall(),
            backlog: Vec::new(),
        }
    }
    fn control(&mut self, action: PlayerAction) {
        self.sequence += 1;
        let control = PlayerControl::sign(
            &self.root,
            &self.cert,
            0,
            self.sequence,
            self.player,
            self.framing.tls(),
            action,
            &self.keys,
        )
        .unwrap();
        // Controls queue with priority (64 KiB); release them as needed.
        if self.framing.queued_bytes() > 32 * 1024 {
            self.hold();
        }
        self.framing
            .enqueue(PacketKind::Control, &control.encode(&self.root).unwrap())
            .unwrap();
    }
    /// Release every queued wire into the backlog, past the sender's pacing.
    fn hold(&mut self) {
        loop {
            match self.framing.poll_send(self.clock).unwrap() {
                Some(wire) => self.backlog.push(wire),
                None if self.framing.queued_bytes() == 0 => break,
                None => self.clock += 100,
            }
        }
    }
    fn intent(&mut self, bytes: Vec<u8>) {
        self.intent += 1;
        let number = self.intent;
        self.control(PlayerAction::Intent { number, bytes });
    }
    fn view(&mut self, bytes: Vec<u8>) {
        self.view += 1;
        let number = self.view;
        self.control(PlayerAction::Lossy { number, bytes });
    }
    /// Every queued wire at once, past the sender's own pacing (a backlog
    /// QUIC delivers in one go after a stall).
    fn burst(&mut self) -> usize {
        self.hold();
        let wires = std::mem::take(&mut self.backlog);
        let sent = wires.len();
        for wire in wires {
            self.link.send_reliable(wire);
        }
        sent
    }
}

/// B's newest input view (race, seq) from its recorded wires.
fn newest_view(root: &Root, sent: &Mutex<Vec<Vec<u8>>>) -> Option<(u8, u32)> {
    use race_session::proto::{decode, LossyToHost};
    sent.lock().unwrap().iter().rev().find_map(|wire| {
        let frame = Frame::decode(wire).ok().filter(|f| f.count == 1)?;
        let control = PlayerControl::decode(root, &frame.payload).ok()?;
        let PlayerAction::Lossy { bytes, .. } = control.action else {
            return None;
        };
        match decode::<LossyToHost>(&bytes)? {
            LossyToHost::Inputs { race, seq, .. } => Some((race, seq)),
        }
    })
}

/// A backlog over the ingress rate limit (64 KiB burst) is delayed, not
/// lost, on both sides: the original keeps the rate-limited frames queued
/// and reads them on later polls; a remote client that stalled reads its
/// backlog over several frames. No intent or output is lost and no
/// connection closes.
#[test]
fn a_burst_over_the_ingress_rate_limit_is_delayed_not_lost() {
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2)];
    let mut raw = RawMember::new(&mut room, 3);
    let mut index = 0;
    let mut native_equal = true;
    for _ in 0..3 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    // Original side: 100 signed originals of 900 bytes (two wires each),
    // undecodable as game messages, so the native race ignores them.
    let before = room.players.ingress();
    let sent = 100;
    for _ in 0..sent {
        raw.intent(vec![0xFF; 900]);
    }
    let wires = raw.burst();
    let t = Instant::now();
    while room.players.ingress().received_frames < before.received_frames + wires {
        room.poll_players();
        bounded(t);
    }
    let mut rounds = 0;
    while room.handled(raw.player) < sent && rounds < 100 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        rounds += 1;
    }
    let after = room.players.ingress();
    let host_recovered = after.rate_limited > before.rate_limited
        && room.handled(raw.player) == sent
        && after.refused_frames == before.refused_frames
        && after.disconnected == before.disconnected;
    println!(
        "burst_original wires={wires} rate_limited_polls={} handled={} refused={} disconnected={}",
        after.rate_limited - before.rate_limited,
        room.handled(raw.player),
        after.refused_frames - before.refused_frames,
        after.disconnected - before.disconnected,
    );
    assert!(host_recovered, "a rate-limited burst lost an original");

    // Client side: A stalls while the room commits, then reads its backlog.
    let a = room.player(1);
    for _ in 0..150 {
        index += 1;
        let now = room.base + index * STEP;
        remotes[1]
            .frame(now, room.wall(), remote_input(index, 1))
            .unwrap();
        room.commit(index);
        native_equal &= room.native_equals_standby();
    }
    let ceiling = room.live.primary().output_ceiling(a, Delivery::Reliable);
    let mut limited_frames = 0;
    let mut frames = 0;
    let mut applied = migration::RemoteFrame::default();
    let t = Instant::now();
    while remotes[0].endpoint().journals().0.applied() < ceiling || frames < 5 {
        let step = remotes[0]
            .frame(
                room.base + index * STEP,
                room.wall(),
                remote_input(index, 0),
            )
            .unwrap();
        limited_frames += usize::from(step.rate_limited);
        applied.reliable_applied += step.reliable_applied;
        applied.lossy_applied += step.lossy_applied;
        applied.duplicates += step.duplicates;
        frames += 1;
        room.poll_players();
        bounded(t);
    }
    let client_recovered = limited_frames > 0
        && remotes[0].endpoint().journals().0.applied() == ceiling
        && remotes[0].endpoint().failure().is_none()
        && applied.duplicates == 0
        && room.players.attached(a);
    println!(
        "burst_client frames={frames} rate_limited_frames={limited_frames} reliable_applied={} lossy_applied={} duplicates={} applied={} ceiling={ceiling} native_equal={native_equal}",
        applied.reliable_applied,
        applied.lossy_applied,
        applied.duplicates,
        remotes[0].endpoint().journals().0.applied(),
    );
    assert!(client_recovered, "a stalled client lost its backlog");
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// Room identity `r + 1` (remote `r`) resumes on a new connection after its
/// connection was closed.
fn rejoin(
    room: &mut ArmedRoom,
    remotes: &mut [migration::RemotePlayer; 2],
    r: usize,
    index: &mut u64,
) -> bool {
    let p = room.player(r + 1);
    let t = Instant::now();
    while room.players.peer(p).is_some() {
        room.poll_players();
        bounded(t);
    }
    let link = room.session_join(&room.identities[r + 1].clone());
    remotes[r].reconnect(link).unwrap();
    let mut native_equal = true;
    let t = Instant::now();
    while remotes[r].resuming() || !room.players.attached(p) {
        native_equal &= play_round(room, remotes, index);
        bounded(t);
    }
    native_equal
}

/// More output than A's channel can queue, framed in one publish: nothing
/// after the first refused output is framed to A (no gap), A's connection
/// is closed visibly without a ban, and A resumes with every reliable
/// output re-framed from the retained outbox.
#[test]
fn a_send_overflow_closes_the_connection_visibly_and_the_player_resumes() {
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2)];
    let a = room.player(1);
    let mut index = 0;
    let mut native_equal = true;
    let mut installed = None;
    while installed.is_none() || index < 20 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        installed = installed.or_else(|| {
            room.live
                .primary()
                .outbox()
                .find(|o| o.output.recipient == a)
                .cloned()
        });
        assert!(index < 300, "no retained output for A");
    }
    let installed = installed.unwrap();
    let before = room.players.ingress();
    let copies = vec![installed; 6000];
    let wall = room.wall();
    let framed = room
        .players
        .publish(&room.live, &copies, &[], wall)
        .unwrap();
    let after = room.players.ingress();
    let closed_visibly = framed > 0
        && framed < copies.len()
        && room.players.peer(a).is_none()
        && after.disconnected == before.disconnected + 1
        && after.banned == before.banned;
    println!(
        "send_overflow copies={} framed={framed} peer_closed={} disconnected={} banned={}",
        copies.len(),
        room.players.peer(a).is_none(),
        after.disconnected - before.disconnected,
        after.banned
    );
    assert!(closed_visibly, "a send overflow stranded A's outputs");
    native_equal &= rejoin(&mut room, &mut remotes, 0, &mut index);
    for _ in 0..30 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    let t = Instant::now();
    loop {
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        let ceiling = room.live.primary().output_ceiling(a, Delivery::Reliable);
        let queue = room.players.queue(a).unwrap();
        if remotes[0].endpoint().journals().0.applied() == ceiling
            && room.handled(a) + queue.pending().count() as u64
                == remotes[0].endpoint().journals().1.tail()
            && queue.output_ack() >= remotes[0].acked()
        {
            break;
        }
        room.poll_players();
        bounded(t);
    }
    index += 1;
    room.commit(index);
    native_equal &= room.native_equals_standby();
    let resumed = room.players.ingress().resumed == 1
        && room.handled(a) == remotes[0].endpoint().journals().1.tail()
        && room.output_ack(a) == remotes[0].acked()
        && room.players.ingress().banned == 0;
    println!(
        "send_overflow resumed={resumed} applied={} acked={} native_equal={native_equal}",
        remotes[0].endpoint().journals().0.applied(),
        remotes[0].acked()
    );
    assert!(resumed, "A did not resume after the overflow");
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// Once a member's My Signet approval expires, `publish` frames nothing more
/// to it (publish admission) and none of its later frames reach a queue
/// (per-frame admission: `QuicHost` closes the expired connection before
/// reading it). The room keeps committing.
#[test]
fn an_expired_approval_gets_no_output_and_no_frame_reaches_the_queue() {
    use race_session::proto::{encode, ToHost};
    let (mut room, until) = ArmedRoom::build(Some(3));
    let mut remotes = [room.remote(1), room.remote(2)];
    let mut raw = RawMember::new(&mut room, 3);
    let p = raw.player;
    let mut index = 0;
    let mut native_equal = true;
    // Admitted: an original from the member applies.
    raw.intent(encode(&ToHost::SetAttire([1, 0, 0])));
    raw.burst();
    let mut output = None;
    while room.handled(p) < 1 || output.is_none() {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        output = output.or_else(|| {
            room.live
                .primary()
                .outbox()
                .find(|o| o.output.recipient == p)
                .cloned()
        });
        assert!(
            index < 300 && unix_now() < until,
            "setup outran the approval"
        );
    }
    // Wait out the approval with the pair link alive but no player poll.
    let t = Instant::now();
    while unix_now() <= until {
        let _ = room.live.probe();
        assert!(room.live.poll().unwrap().is_empty());
        room.standby.poll().unwrap();
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    let before = room.players.ingress();
    let wall = room.wall();
    let framed = room
        .players
        .publish(&room.live, &[output.unwrap()], &[], wall)
        .unwrap();
    let publish_refused =
        framed == 0 && room.players.ingress().unadmitted_outputs == before.unadmitted_outputs + 1;
    // Expired: a later original from the same connection never applies.
    raw.intent(encode(&ToHost::SetAttire([2, 0, 0])));
    raw.burst();
    for _ in 0..30 {
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
    }
    let queue = room.players.queue(p).unwrap();
    let frames_refused =
        room.handled(p) == 1 && queue.pending().count() == 0 && room.players.peer(p).is_none();
    let paused = room.live.failure().is_some() || room.standby.failure().is_some();
    println!(
        "expired_approval publish_framed={framed} publish_refused={publish_refused} handled={} pending={} peer_closed={} paused={paused} native_equal={native_equal}",
        room.handled(p),
        queue.pending().count(),
        room.players.peer(p).is_none(),
    );
    assert!(
        publish_refused,
        "an output was framed to an expired approval"
    );
    assert!(
        frames_refused,
        "an expired approval's frame reached the queue"
    );
    assert!(!paused && native_equal);
}

/// Game-input sanity still runs on views that arrive through the queue: a
/// signed view for another race never drives the member's native racer,
/// while the same view for the current race does.
#[test]
fn a_view_for_another_race_never_drives_the_native_racer() {
    use race_session::proto::{encode, LossyToHost};
    let mut room = ArmedRoom::new();
    let (b, _, b_sent) = room.tapped_remote(2);
    let mut remotes = [room.remote(1), b];
    let mut raw = RawMember::new(&mut room, 3);
    let root = room.root().clone();
    // A hard left: the racer keeps its steer even while dismounted.
    let steer = Input {
        steer: -0.75,
        throttle: 1.0,
        ..Input::default()
    };
    let full = steer.pack();
    let steered = |racer: &race_core::Racer| (racer.prev_input.steer - steer.steer).abs() < 0.02;
    let mut index = 0;
    let mut native_equal = true;
    // The race number comes from B's own views; input numbers follow the
    // native tick (input n drives tick n - 1), just ahead of it.
    let send = |raw: &mut RawMember, room: &ArmedRoom, wrong: bool| {
        let tick = room.live.authority().world().map(|w| w.tick);
        if let (Some((race, _)), Some(tick)) = (newest_view(&root, &b_sent), tick) {
            let race = if wrong { race ^ 0x80 } else { race };
            raw.view(encode(&LossyToHost::Inputs {
                race,
                seq: tick as u32 + 4,
                inputs: vec![full; 5],
            }));
            raw.burst();
        }
    };
    let mut wrong_commits = 0;
    let mut wrong_steered = false;
    while wrong_commits < 40 {
        send(&mut raw, &room, true);
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        if let Some(racer) = room.racer(3) {
            wrong_commits += 1;
            wrong_steered |= steered(racer);
        }
        assert!(index < 400, "the race did not run");
    }
    let mut right_steered = 0;
    for _ in 0..40 {
        send(&mut raw, &room, false);
        native_equal &= play_round(&mut room, &mut remotes, &mut index);
        if room.racer(3).is_some_and(steered) {
            right_steered += 1;
        }
    }
    println!(
        "input_sanity wrong_race_commits={wrong_commits} wrong_race_steered={wrong_steered} right_race_steered_commits={right_steered} native_equal={native_equal}"
    );
    assert!(!wrong_steered, "a view for another race drove the racer");
    assert!(right_steered > 10, "the crafted view never drove the racer");
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// The standby device's own player is an ordinary remote human on the
/// original's listener, admitted only with the standby runtime's shared
/// sender: Fray refuses a separate counter for that key. A second standby-key
/// connection evicts nobody, and the original runtime's key never gets a
/// network player route.
#[test]
fn the_standby_key_gets_a_player_route_only_with_the_shared_sender() {
    let mut room = ArmedRoom::new();
    let mut remotes = [room.remote(1), room.remote(2)];
    let standby_player = room.player(0);
    let id = room.identities[0].clone();
    let network = room.session_join(&id);
    let t = Instant::now();
    while room.players.queue(standby_player).is_none() {
        room.poll_players();
        bounded(t);
    }
    let peer = room.players.peer(standby_player);
    // A separate counter for the standby key is refused before any frame
    // (an in-memory channel with the same binding: Fray checks the key first).
    let root = room.root().clone();
    let channel = Channel::new_bound(
        &player_policy(&room.descriptor, &room.cert),
        session_binding(&root, 0, LinkRole::Player, LinkRole::Original),
        ChannelSide::Client,
        root.standby,
        root.host,
        [9; 32],
    )
    .unwrap();
    let separate = PlayerEndpoint::new(
        room.descriptor.clone(),
        room.standby.armed(),
        &room.capability(0),
        room.cert.clone(),
        channel,
        id.keys().clone(),
    )
    .err();
    assert_eq!(
        separate,
        Some(Error::Invalid("authority player must share its sender"))
    );
    let mut own = migration::RemotePlayer::connect_with_sender(
        room.descriptor.clone(),
        room.standby.armed(),
        &room.capability(0),
        room.cert.clone(),
        network,
        room.clients[0].take().unwrap(),
        id.keys().clone(),
        Some(room.standby.sender_sequence()),
    )
    .unwrap();
    // A second standby-key connection (refused by the transport or the
    // game gate) never evicts the live route.
    let before = room.players.ingress();
    let (cfg, original) = (room.cfg.clone(), room.original.clone());
    let to = (room.addr, room.token.clone(), root.host);
    let binding = session_binding(&root, 0, LinkRole::Player, LinkRole::Original);
    let second = dial(&cfg, &id, to.clone(), binding.clone(), || {
        room.poll_players()
    });
    let mut native_equal = true;
    let mut index = 0;
    let mut sent = 0;
    for _ in 0..12 {
        index += 1;
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        let step = own
            .frame_with(now, room.wall(), Input::default(), |c| {
                if index == 2 {
                    c.set_character(3);
                }
            })
            .unwrap();
        sent += step.intents.len();
        room.commit(index);
        native_equal &= room.native_equals_standby();
    }
    let after = room.players.ingress();
    let host = dial(&cfg, &original, to, binding, || room.poll_players());
    for _ in 0..50 {
        room.poll_players();
        std::thread::sleep(Duration::from_millis(1));
    }
    println!(
        "standby_key routed={} intents_sent={sent} handled={} second_connected={} refused_peers={} host_connected={}",
        room.players.queue(standby_player).is_some(),
        room.handled(standby_player),
        second.is_some(),
        after.refused_peers - before.refused_peers,
        host.is_some()
    );
    drop(second);
    assert_eq!(
        room.players.peer(standby_player),
        peer,
        "second connection evicted"
    );
    assert!(sent > 0 && room.handled(standby_player) >= sent as u64);
    assert!(room.players.queue(standby_player).is_some());
    assert_eq!(room.players.players().count(), 4);
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    // The headless original's key is not seated, so the transport or the
    // game gate refuses it; either way it holds no route.
    assert_eq!(room.players.players().count(), 4);
    drop(host);
}
/// A client session nobody drives (a refused attach consumes it).
fn spare_client() -> ClientSession {
    use race_session::transport::{LoopClock, LoopbackConfig, LoopbackHub};
    let hub = LoopbackHub::new(LoopClock::real(), 2);
    ClientSession::new(Box::new(hub.connect(LoopbackConfig::PERFECT)), "Spare", 0)
}
/// A playing phone host's own player runs through Fray's real player path in
/// process: the self route is accepted only for the original runtime's own
/// key, and its endpoint signs from the original's own sender. Its intents
/// commit, its outputs apply once and its `Applied` ACKs prune its outbox.
/// The original's key still gets no network player route.
#[test]
fn the_phone_hosts_own_player_runs_on_the_originals_key_and_sender() {
    let (mut room, own_client) = ArmedRoom::phone();
    let mut remotes = [room.remote(1), room.remote(2)];
    let root = room.root().clone();
    let host_player = root
        .members
        .iter()
        .find(|m| m.key == Some(root.host))
        .unwrap()
        .player;
    let armed = room.standby.armed().clone();
    let ack = |keys: &decented_net::Keys, player: u64| {
        CapabilityAck::sign(&root, player, room.initial, 1, keys).unwrap()
    };
    let (host_ack, other_ack) = (
        ack(room.original.keys(), host_player),
        ack(room.identities[1].keys(), room.player(1)),
    );
    // Another member's key is refused, before any queue is attached.
    let other = room.players.attach_own(
        &mut room.live,
        &armed,
        &other_ack,
        spare_client(),
        room.identities[1].keys().clone(),
    );
    assert_eq!(other.err(), Some(Error::Unauthorized));
    // The original's key on the network before its own route exists: the
    // transport admits it (it is seated), the member lookup would find it
    // and no route stands in the way, so only the host-key gate refuses it.
    // Without the gate it would get a network player route, and the
    // in-process `attach_own` below would be refused as a second route.
    let (early_dial, early_refused) = dial_host_key(&mut room);
    assert_eq!(early_refused, 1, "the host key was not refused by the gate");
    assert!(room.players.queue(host_player).is_none() && room.players.peer(host_player).is_none());
    let mut own = room
        .players
        .attach_own(
            &mut room.live,
            &armed,
            &host_ack,
            own_client,
            room.original.keys().clone(),
        )
        .unwrap();
    let again = room.players.attach_own(
        &mut room.live,
        &armed,
        &host_ack,
        spare_client(),
        room.original.keys().clone(),
    );
    assert_eq!(again.err(), Some(Error::Conflict));
    let mut sent = 0;
    let mut applied = 0;
    let mut acked = 0;
    let mut native_equal = true;
    for index in 1..=24u64 {
        let now = room.base + index * STEP;
        for (r, remote) in remotes.iter_mut().enumerate() {
            remote
                .frame(now, room.wall(), remote_input(index, r))
                .unwrap();
        }
        let step = own
            .frame_with(now, room.wall(), remote_input(index, 0), |c| {
                if index == 3 {
                    c.set_character(2);
                }
            })
            .unwrap();
        sent += step.intents.len();
        applied += step.reliable_applied;
        acked = acked.max(step.acked.unwrap_or(0));
        room.commit(index);
        native_equal &= room.native_equals_standby();
    }
    // While the in-process route is live, a network dial on the original's
    // key is refused too (by the gate, or as a second connection).
    let (host, refused) = dial_host_key(&mut room);
    assert_eq!(refused, 1);
    let outbox = room.outbox(host_player);
    let ack_mark = room.output_ack(host_player);
    // The endpoint shares the original's sender: one counter, so the next
    // two IDs from either handle are consecutive.
    let a = own.endpoint().sequence().allocate().unwrap();
    let b = room.live.sender_sequence().allocate().unwrap();
    println!(
        "host_seat sent={sent} handled={} applied={applied} acked={acked} output_ack={ack_mark} outbox={outbox:?} host_dial={} refused={refused} ids=({a},{b})",
        room.handled(host_player),
        host
    );
    assert_eq!(b, a + 1, "the host player's endpoint has its own counter");
    assert!(sent > 0 && room.handled(host_player) >= sent as u64);
    assert!(applied > 0 && acked > 0 && ack_mark > 0);
    assert!(
        outbox.iter().all(|&n| n > ack_mark),
        "an ACKed output stayed in the outbox"
    );
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    assert!(room.players.peer(host_player).is_some() && room.players.attached(host_player));
    assert_eq!(room.players.players().count(), 4);
    // The host's own player leaves: its local route detaches (it has no
    // reconnect path), so no connection holds the route any more. A network
    // dial on the original's key now meets an idle route that a remote
    // player's key would resume (a challenge); only the gate refuses it.
    let before = room.players.ingress();
    drop(own.into_parts().unwrap());
    room.poll_players();
    let left = room.players.ingress().disconnected - before.disconnected;
    assert_eq!(left, 1, "the host player's local route did not detach");
    assert!(room.players.peer(host_player).is_none());
    let (late_dial, late_refused) = dial_host_key(&mut room);
    println!(
        "host_key_gate early_dial={early_dial} early_refused={early_refused} live_dial={} live_refused={refused} local_left={left} late_dial={late_dial} late_refused={late_refused}",
        host
    );
    assert_eq!(late_refused, 1, "the host key was not refused by the gate");
    assert!(
        room.players.peer(host_player).is_none(),
        "the host key took the host player's route over the network"
    );
    assert!(room.live.failure().is_none());
}

/// Dial the original's own key as a session-bound player, pump the player
/// host for 50 ms, and return whether the transport connected and how many
/// connections the player host refused meanwhile.
fn dial_host_key(room: &mut ArmedRoom) -> (bool, usize) {
    let root = room.root().clone();
    let before = room.players.ingress();
    let (cfg, original) = (room.cfg.clone(), room.original.clone());
    let to = (room.addr, room.token.clone(), root.host);
    let binding = session_binding(&root, 0, LinkRole::Player, LinkRole::Original);
    let link = dial(&cfg, &original, to, binding, || room.poll_players());
    for _ in 0..50 {
        room.poll_players();
        std::thread::sleep(Duration::from_millis(1));
    }
    let refused = room.players.ingress().refused_peers - before.refused_peers;
    (link.is_some(), refused)
}

/// The phone host's own player sends a burst of reliable intents while the
/// original does not poll. The pipe holds them; the first poll moves only
/// what its route's ingress takes and leaves the rest in order in the pipe
/// (backpressure), with the route still attached. Later polls drain it:
/// every intent is handled once, every ping gets one pong, and the route is
/// never detached.
#[test]
fn the_host_players_pipe_holds_a_burst_while_its_ingress_is_full_and_recovers() {
    let (mut room, own_client) = ArmedRoom::phone();
    let root = room.root().clone();
    let host_player = root
        .members
        .iter()
        .find(|m| m.key == Some(root.host))
        .unwrap()
        .player;
    let armed = room.standby.armed().clone();
    let ack = CapabilityAck::sign(&root, host_player, room.initial, 1, room.original.keys());
    let mut own = room
        .players
        .attach_own(
            &mut room.live,
            &armed,
            &ack.unwrap(),
            own_client,
            room.original.keys().clone(),
        )
        .unwrap();
    let start = room.players.ingress();
    let mut sent = 0u64;
    let mut pings = BTreeMap::new();
    let mut pongs = BTreeMap::<u32, usize>::new();
    let mut record = |intents: &[(u64, Vec<u8>)], sent: &mut u64| {
        for (number, bytes) in intents {
            assert_eq!(*number, *sent + 1, "intent numbers");
            *sent = *number;
            if let Some((id, _)) = ping_of(bytes) {
                assert!(pings.insert(id, *number).is_none(), "ping sent twice");
            }
        }
    };
    let count = |pongs: &mut BTreeMap<u32, usize>, outputs: &[CertifiedOutput]| {
        for output in outputs {
            if output.output.recipient == host_player {
                if let Some((id, _, _)) = pong_of(&output.output.bytes) {
                    *pongs.entry(id).or_default() += 1;
                }
            }
        }
    };
    let mut index = 0;
    for _ in 0..4 {
        index += 1;
        let now = room.base + index * STEP;
        let step = own.frame(now, room.wall(), remote_input(index, 0)).unwrap();
        record(&step.intents, &mut sent);
        let outputs = room.commit(index);
        count(&mut pongs, &outputs);
    }
    // The burst: one reliable intent per frame, 1 ms apart (inside the
    // client's freeze bound), and no original poll in between.
    let burst_at = room.base + index * STEP;
    for k in 1..=180u64 {
        let step = own
            .frame_with(burst_at + k, room.wall(), remote_input(index, 0), |c| {
                c.set_character(1 + (k % 3) as u8)
            })
            .unwrap();
        record(&step.intents, &mut sent);
        std::thread::sleep(Duration::from_millis(1));
    }
    let burst = sent;
    let before = room.players.ingress();
    room.poll_players();
    let after = room.players.ingress();
    let held = after.held_local - before.held_local;
    let moved = after.received_frames - before.received_frames;
    assert!(held > 0, "the burst never filled the route's ingress");
    assert!(room.players.attached(host_player));
    // Recovery: frames and commits resume until every intent is handled.
    let t = Instant::now();
    while room.handled(host_player) < sent || index < 12 {
        index += 1;
        let now = burst_at + 180 + (index - 4) * STEP;
        let step = own.frame(now, room.wall(), remote_input(index, 0)).unwrap();
        record(&step.intents, &mut sent);
        let outputs = room.commit_at(index, room.base + index * STEP, None);
        count(&mut pongs, &outputs);
        bounded(t);
    }
    let end = room.players.ingress();
    let pongs_once = pings.keys().all(|id| pongs.get(id) == Some(&1));
    println!(
        "LOCAL PIPE burst_intents={burst} sent={sent} handled={} held_polls={} first_poll_frames={moved} pings={} pongs_once={pongs_once} disconnected={} refused_frames={}",
        room.handled(host_player),
        end.held_local - start.held_local,
        pings.len(),
        end.disconnected - start.disconnected,
        end.refused_frames - start.refused_frames,
    );
    assert_eq!(
        room.handled(host_player),
        sent,
        "an intent was lost or repeated"
    );
    assert!(pongs_once, "a ping did not get exactly one pong");
    assert_eq!(
        end.disconnected, start.disconnected,
        "the host's own route was detached"
    );
    assert_eq!(end.refused_frames, start.refused_frames);
    assert!(room.players.attached(host_player));
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
}

/// The local-seats room on a playing phone host: the host's own player runs
/// in process through `attach_own` (the original's key and sender), the
/// standby device's own player on an ordinary session-bound route whose
/// endpoint signs from the standby's pair-link sender, and remote humans 1
/// and 2 on their own links, with four bots (remote human 3 is seated and
/// silent). Seats: [standby own, host own, remote 1, remote 2].
fn local_seats(
    room: &mut ArmedRoom,
    host_client: ClientSession,
) -> (Vec<migration::RemotePlayer>, [u64; 4]) {
    let root = room.root().clone();
    let host_player = root
        .members
        .iter()
        .find(|m| m.key == Some(root.host))
        .unwrap()
        .player;
    let armed = room.standby.armed().clone();
    let ack = CapabilityAck::sign(&root, host_player, room.initial, 1, room.original.keys());
    let host = room
        .players
        .attach_own(
            &mut room.live,
            &armed,
            &ack.unwrap(),
            host_client,
            room.original.keys().clone(),
        )
        .unwrap();
    let id = room.identities[0].clone();
    let network = room.session_join(&id);
    let own_player = room.player(0);
    let t = Instant::now();
    while room.players.queue(own_player).is_none() {
        room.poll_players();
        bounded(t);
    }
    let own = migration::RemotePlayer::connect_with_sender(
        room.descriptor.clone(),
        room.standby.armed(),
        &room.capability(0),
        room.cert.clone(),
        network,
        room.clients[0].take().unwrap(),
        id.keys().clone(),
        Some(room.standby.sender_sequence()),
    )
    .unwrap();
    let seats = vec![own, host, room.remote(1), room.remote(2)];
    (
        seats,
        [own_player, host_player, room.player(1), room.player(2)],
    )
}

/// One frame of every local-seats player at `now`, recorded in its ledger.
/// Each also sends a character command (a reliable intent) every 40th frame,
/// besides its clock-sync pings.
fn seats_frame(
    room: &ArmedRoom,
    seats: &mut [migration::RemotePlayer],
    ledgers: &mut [RemoteLedger],
    now: u64,
    index: u64,
) {
    for (s, seat) in seats.iter_mut().enumerate() {
        let character = 1 + ((index / 40 + s as u64) % 3) as u8;
        let step = seat.frame_with(now, room.wall(), remote_input(index, s % 2), |c| {
            if index % 40 == 5 + s as u64 {
                c.set_character(character);
            }
        });
        assert!(step.is_ok(), "seat {s} refused a frame: {:?}", step.err());
        let step = step.unwrap();
        assert!(
            step.newest_receipt
                .is_none_or(|n| n <= room.standby.receipt().index),
            "seat {s} applied an output before its commit"
        );
        let l = &mut ledgers[s];
        l.frozen[0] += usize::from(step.frozen);
        l.reliable_applied += step.reliable_applied;
        l.lossy_applied += step.lossy_applied;
        l.duplicates += step.duplicates;
        l.acks += usize::from(step.acked.is_some());
        for (number, bytes) in &step.intents {
            assert_eq!(*number, l.intents + 1, "seat {s}: intent numbers");
            l.intents = *number;
            if let Some((id, _)) = ping_of(bytes) {
                assert!(l.pings.insert(id, *number).is_none(), "ping sent twice");
            }
        }
    }
}

/// Count each committed Pong per seat and track each seat's outbox.
fn seats_record(
    room: &ArmedRoom,
    ids: &[u64],
    ledgers: &mut [RemoteLedger],
    outputs: &[CertifiedOutput],
) {
    for output in outputs {
        let Some(s) = ids.iter().position(|p| *p == output.output.recipient) else {
            continue;
        };
        if let Some((id, _, _)) = pong_of(&output.output.bytes) {
            *ledgers[s].pongs.entry(id).or_default() += 1;
        }
    }
    for (s, &player) in ids.iter().enumerate() {
        let outbox = room.outbox(player).len();
        let l = &mut ledgers[s];
        l.outbox_shrank |= outbox < l.peak_outbox;
        l.peak_outbox = l.peak_outbox.max(outbox);
    }
}

/// A slow frame on the phone host: no device frame runs for `gap` ms of
/// game time. Frames still in flight are first absorbed by one batch at the
/// same game time, so the gap's batches carry no input and the native world
/// can be compared with a plain host that runs the same elapsed time in one
/// `update`. The gap is staged as back-to-back batches of at most 50 ms.
/// Returns the gap's batch count, whether the native world equals the plain
/// host's, whether it equalled the standby's at every commit, and the
/// committed outputs.
fn slow_frame(
    room: &mut ArmedRoom,
    index: &mut u64,
    now: u64,
    gap: u64,
) -> (u64, bool, bool, Vec<CertifiedOutput>) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(30) {
        room.poll_players();
        std::thread::sleep(Duration::from_millis(1));
    }
    *index += 1;
    let mut outputs = room.commit_at(*index, now, None);
    let mut standby_equal = room.native_equals_standby();
    let (binding, bytes) = migration::checkpoint(room.live.authority()).unwrap();
    let mut plain = migration::restore(&bytes, &binding, Box::new(Script::default())).unwrap();
    let target = now + gap;
    plain.update(target);
    let mut batches = 0;
    while room.live.primary().receipt().logical_ms < target {
        *index += 1;
        batches += 1;
        assert!(batches <= gap, "the slow frame never caught up");
        outputs.extend(room.commit_at(*index, target, None));
        standby_equal &= room.native_equals_standby();
    }
    let live = room.live.authority();
    let plain_equal = same_world(live, &plain);
    // The full native state, including the 30 Hz view schedule, equals a
    // plain host updated at the batch times: views are released per batch.
    let mut stepped = migration::restore(&bytes, &binding, Box::new(Script::default())).unwrap();
    let mut at = now;
    while at < target {
        at = (at + 50).min(target);
        stepped.update(at);
    }
    let stepped_equal =
        migration::checkpoint(live).unwrap() == migration::checkpoint(&stepped).unwrap();
    println!(
        "slow_frame snapshots_sent split={} one_update={}",
        live.stats().snapshots_sent,
        plain.stats().snapshots_sent
    );
    assert!(
        stepped_equal,
        "the split batches diverged from the plain host"
    );
    (batches, plain_equal, standby_equal, outputs)
}

/// The native simulation of two authorities is the same: world (ticks,
/// racers, items, events), results and the tick and starvation counters.
fn same_world(a: &HostAuthority, b: &HostAuthority) -> bool {
    format!("{:?}", a.world()) == format!("{:?}", b.world())
        && format!("{:?}", a.results()) == format!("{:?}", b.results())
        && a.stats().ticks == b.stats().ticks
        && a.stats().starved_ticks == b.stats().starved_ticks
}

/// Items A and B together: an armed match where the standby device's own
/// player (an ordinary `Player`→`Original` route signing from the standby's
/// pair-link sender) and the phone host's own player (`attach_own`, in
/// process on the original's sender) play real inputs and reliable intents
/// with two remote humans on session-bound QUIC and four bots. Every
/// reliable intent applies once, every output applies once, `Applied` ACKs
/// prune each outbox, and the native state equals the standby's at every
/// commit. Mid-race the phone host stalls for 70 ms and then 150 ms: each
/// gap is staged as back-to-back batches of at most 50 ms, the room carries
/// on, the native world equals a plain host's over the same elapsed time and
/// the standby stays in sync.
#[test]
fn both_own_seats_play_an_armed_match_on_real_routes_through_slow_frames() {
    let (mut room, host_client) = ArmedRoom::phone();
    const LOBBY: u64 = 10;
    room.start_at = LOBBY + 1;
    let (mut seats, ids) = local_seats(&mut room, host_client);
    assert_eq!(room.players.players().count(), 5);
    let mut ledgers: Vec<RemoteLedger> = (0..4).map(|_| RemoteLedger::default()).collect();
    let mut native_equal = true;
    let (mut index, mut now) = (0u64, room.base);
    let mut racing = 0u32;
    let mut gaps = [70u64, 150].into_iter().peekable();
    let mut slow = Vec::new();
    loop {
        index += 1;
        now += STEP;
        seats_frame(&room, &mut seats, &mut ledgers, now, index);
        let outputs = room.commit_at(index, now, None);
        seats_record(&room, &ids, &mut ledgers, &outputs);
        native_equal &= room.native_equals_standby();
        assert!(
            native_equal,
            "native state differs from the standby at {index}"
        );
        let in_race = room
            .live
            .authority()
            .world()
            .is_some_and(|w| w.phase == RacePhase::Racing);
        racing += u32::from(in_race);
        if in_race && racing % 30 == 0 && gaps.peek().is_some() {
            let gap = gaps.next().unwrap();
            let (batches, plain_equal, standby_equal, outputs) =
                slow_frame(&mut room, &mut index, now, gap);
            seats_record(&room, &ids, &mut ledgers, &outputs);
            now += gap;
            println!(
                "slow_frame term=0 gap_ms={gap} batches={batches} plain_equal={plain_equal} standby_equal={standby_equal} paused={}",
                room.live.failure().is_some()
            );
            assert!(room.live.failure().is_none() && room.standby.failure().is_none());
            assert_eq!(batches, gap.div_ceil(50), "the gap was not split");
            assert!(plain_equal, "the split gap diverged from the plain host");
            assert!(standby_equal, "the standby fell out of sync");
            slow.push((gap, batches));
        }
        if racing >= 100 {
            break;
        }
        assert!(index < 1000, "the race never ran");
    }
    // Settle: every published output reaches its seat, then the seats'
    // pending intents and ACKs reach the original before the last commit.
    let t = Instant::now();
    loop {
        seats_frame(&room, &mut seats, &mut ledgers, now, index);
        let applied = (0..4).all(|s| {
            seats[s].endpoint().journals().0.applied()
                == room
                    .live
                    .primary()
                    .output_ceiling(ids[s], Delivery::Reliable)
        });
        if applied {
            break;
        }
        room.poll_players();
        bounded(t);
    }
    let t = Instant::now();
    loop {
        room.poll_players();
        let received = (0..4).all(|s| {
            let queue = room.players.queue(ids[s]).unwrap();
            room.handled(ids[s]) + queue.pending().count() as u64 == ledgers[s].intents
                && queue.output_ack() >= seats[s].acked()
        });
        if received || t.elapsed() > Duration::from_secs(2) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    index += 1;
    now += STEP;
    let outputs = room.commit_at(index, now, None);
    seats_record(&room, &ids, &mut ledgers, &outputs);
    native_equal &= room.native_equals_standby();
    let names = ["standby_own", "host_own", "remote_1", "remote_2"];
    let mut intents_once = true;
    let mut outputs_once = true;
    let mut acks_prune = true;
    for (s, &player) in ids.iter().enumerate() {
        let (l, seat) = (&ledgers[s], &seats[s]);
        let (delivery, intents) = seat.endpoint().journals();
        let once = room.handled(player) == l.intents
            && intents.tail() == l.intents
            && l.intents > 0
            && !l.pings.is_empty()
            && l.pings.keys().all(|id| l.pongs.get(id) == Some(&1))
            && l.pongs.keys().all(|id| l.pings.contains_key(id));
        let applied = l.reliable_applied as u64 == delivery.applied()
            && l.reliable_applied > 0
            && l.duplicates == 0;
        let outbox = room.outbox(player);
        let pruned = l.acks > 0
            && room.output_ack(player) == seat.acked()
            && seat.acked() > 0
            && outbox.iter().all(|&n| n > seat.acked())
            && l.outbox_shrank;
        println!(
            "local_seat {}: intents={} pings={} handled={} reliable_applied={} duplicates={} acked={} output_ack={} outbox={} peak_outbox={} frozen={} once={once} applied_once={applied} pruned={pruned}",
            names[s],
            l.intents,
            l.pings.len(),
            room.handled(player),
            l.reliable_applied,
            l.duplicates,
            seat.acked(),
            room.output_ack(player),
            outbox.len(),
            l.peak_outbox,
            l.frozen[0],
        );
        intents_once &= once;
        outputs_once &= applied;
        acks_prune &= pruned;
    }
    let frozen: usize = ledgers.iter().map(|l| l.frozen[0]).sum();
    println!(
        "LOCAL SEATS term=0 seats=4 bots=4 commits={index} slow_frames={slow:?} intents_once={intents_once} outputs_once={outputs_once} acks_prune={acks_prune} native_equal={native_equal} frozen_frames={frozen}"
    );
    assert!(intents_once, "a reliable intent was lost or applied twice");
    assert!(outputs_once, "an output was lost or applied twice");
    assert!(acks_prune, "Applied ACKs did not prune an outbox");
    assert!(native_equal);
    assert_eq!(frozen, 0, "a seat froze in a healthy match");
    assert_eq!(slow.len(), 2);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    // Every playing seat keeps its one route (remote human 3's idle link may
    // have timed out: nobody drives it).
    let routed: Vec<u64> = room.players.players().collect();
    println!("routed={routed:?} seats={ids:?}");
    assert!(ids
        .iter()
        .all(|p| routed.contains(p) && room.players.attached(*p)));
}

/// Dial a session-bound link, pumping `pump` meanwhile. `None` if refused.
fn dial(
    cfg: &NetConfig,
    id: &Identity,
    to: (std::net::SocketAddr, String, [u8; 32]),
    binding: ConnectionBinding,
    mut pump: impl FnMut(),
) -> Option<QuicClient> {
    let server = decented_net::PublicKey::from_slice(&to.2).unwrap();
    let handle = decented_net::join_session_addr(cfg, id, to.0, &to.1, binding, server).ok()?;
    let mut link = QuicClient::new(handle);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        pump();
        match link.poll() {
            Some(ClientNetEvent::Connected { .. }) => return Some(link),
            Some(ClientNetEvent::Progress(_)) | None => {}
            Some(_) => return None,
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    None
}
/// A listener's loopback address, LAN token and pinned key.
fn loopback(
    at: (std::net::SocketAddr, String),
    key: [u8; 32],
) -> (std::net::SocketAddr, String, [u8; 32]) {
    (
        std::net::SocketAddr::from(([127, 0, 0, 1], at.0.port())),
        at.1,
        key,
    )
}
fn listener_of(network: &QuicHost, key: [u8; 32]) -> (std::net::SocketAddr, String, [u8; 32]) {
    let h = network.handle().unwrap();
    loopback((h.local_addr().unwrap(), h.lan_token()), key)
}
fn session_binding(
    root: &Root,
    term: u64,
    client: LinkRole,
    server: LinkRole,
) -> ConnectionBinding {
    ConnectionBinding {
        session: root.session,
        descriptor: root.digest().unwrap(),
        term,
        client,
        server,
    }
}

/// Sync a lobby client's clock through its staged frame (a scratch journal
/// takes anything it sends), with a round whose contents the fixture fully
/// controls. The lobby round left open when arming detached the lobby links
/// may already hold a real lobby pong (its RTT is a few ms on loopback, so
/// it would win the lowest-RTT pick and the offset would be its few ms of
/// measurement error, not the fixture's 0). The client's own round abort
/// (`migration_rebase_clock`, which keeps an unsynced clock unsynced) drops
/// that round and its samples; a frame at `sent` opens a fresh one, and its
/// five ping ids are answered 40 ms later, stamped at the round trip's
/// midpoint: host = local, the lockstep fixture's real relation (commit `i`
/// is at `base + i * STEP`). The round then holds exactly these five
/// samples, so the offset is 0 and the RTT 40, deterministically.
fn sync_clock(client: &mut ClientSession, sent: u64) {
    assert!(client.clock_offset_ms().is_none());
    client.migration_rebase_clock(sent, sent, 0).unwrap();
    assert!(!client.clock_sync().round_open() && client.clock_offset_ms().is_none());
    let mut journal = IntentJournal::default();
    client
        .migration_step(&mut journal, sent, Input::default(), &[], None, |_| Ok(()))
        .unwrap();
    let first = client
        .clock_sync()
        .round_first_id()
        .expect("a fresh round opened at `sent`");
    let pongs: Vec<ClientNetEvent> = (first..first + 5)
        .map(|id| {
            let pong = race_session::proto::ToClient::Pong {
                id,
                client_t: sent,
                host_t: sent + 20,
            };
            ClientNetEvent::Reliable(race_session::proto::encode(&pong))
        })
        .collect();
    client
        .migration_step(
            &mut journal,
            sent + 40,
            Input::default(),
            &pongs,
            None,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        (client.clock_offset_ms(), client.clock_sync().rtt_ms()),
        (Some(0), Some(40)),
        "own seat clock unsynced"
    );
    assert!(!client.clock_sync().round_open());
}

/// The two remote survivors' devices once the original stops. Each keeps
/// calling its frame; the client library's own loss detection freezes it
/// (`FREEZE_AFTER_MS` without fresh committed progress), or the standby's
/// signed `Promote` arrives on its prepared link first. Then it recovers.
struct SurvivorDevices<const N: usize = 2> {
    waiting: [Option<migration::RemotePlayer>; N],
    recovering: [Option<migration::RecoveringPlayer>; N],
    last_frame: [u64; N],
    /// The library's freeze: (frozen at, last fresh progress, the device's
    /// previous frame), local times.
    freeze: [Option<(u64, u64, u64)>; N],
    /// Frames run after the drop frame.
    frames: [u64; N],
    /// What froze the device: "no progress" or "promote".
    frozen_by: [Option<&'static str>; N],
    /// Intent tail and pings in flight (above C's handled) at the freeze.
    tail: [Option<u64>; N],
    pings: [Vec<u32>; N],
    /// The local time each survivor saw `Resume`.
    resumed_local: [Option<u64>; N],
    applied: [usize; N],
    duplicates: [usize; N],
    discarded: usize,
}
impl<const N: usize> SurvivorDevices<N> {
    fn step(&mut self, wall: u64, local: u64, hold: [bool; N], handled_c: [u64; N]) {
        for r in 0..N {
            if hold[r] {
                continue;
            }
            if let Some(player) = self.waiting[r].as_mut() {
                if local >= self.last_frame[r] + STEP {
                    let input = remote_input(10_000 + self.frames[r], r % 2);
                    let frame = player.frame(local, wall, input).unwrap();
                    if frame.frozen {
                        if self.frozen_by[r].is_none() {
                            self.frozen_by[r] = Some("no progress");
                            let progress = player.last_progress().expect("progress");
                            self.freeze[r] = Some((local, progress, self.last_frame[r]));
                        }
                    } else {
                        assert!(self.frozen_by[r].is_none(), "a frozen client ran a frame");
                        self.frames[r] += 1;
                    }
                    self.last_frame[r] = local;
                }
                if let Some(promotion) = player.watch_standby(wall).unwrap() {
                    self.frozen_by[r].get_or_insert("promote");
                    let player = self.waiting[r].take().unwrap();
                    let intents = player.endpoint().journals().1;
                    self.tail[r] = Some(intents.tail());
                    self.pings[r] = intents
                        .pending()
                        .filter(|(n, _)| *n > handled_c[r])
                        .filter_map(|(_, b)| ping_of(b).map(|(id, _)| id))
                        .collect();
                    self.recovering[r] = Some(player.recover(promotion, wall).unwrap());
                }
            }
            if let Some(player) = self.recovering[r].as_mut() {
                let step = player.step(local, wall).unwrap();
                self.applied[r] += step.applied;
                self.duplicates[r] += step.duplicates;
                self.discarded += step.views_discarded;
                if step.resumed {
                    self.resumed_local[r] = Some(local);
                }
            }
        }
    }
    fn recovering(&self, r: usize) -> &migration::RecoveringPlayer {
        self.recovering[r].as_ref().unwrap()
    }
}

/// The receipt index at which the survivors' sealed tails were all drained,
/// and the one at which the absent player's departure committed, read from
/// the committed history after every recovery step.
struct DrainOrder {
    root: Root,
    ids: [u64; 2],
    absent: u64,
    drained_at: Option<u64>,
    retired_at: Option<u64>,
}
impl DrainOrder {
    fn mark(&mut self, recovery: &Recovery, tails: &[Option<u64>; 2]) {
        let receipt = recovery.sequencer().receipt();
        let image = SessionImage::decode(&self.root, recovery.sequencer().image()).unwrap();
        let drained = (0..2)
            .all(|r| tails[r].is_some_and(|tail| image.inputs[&self.ids[r]].handled() == tail));
        if drained && self.drained_at.is_none() {
            self.drained_at = Some(receipt.index);
        }
        let retired = receipt
            .watermarks
            .iter()
            .any(|m| m.player == self.absent && m.retired);
        if retired && self.retired_at.is_none() {
            self.retired_at = Some(receipt.index);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Takeover {
    /// Both survivors answer promptly.
    Healthy,
    /// Survivor B's `RecoveryReady` arrives about 1.5 s after A's.
    LateReady,
    /// Survivor B proves its cut and then never answers its input round.
    Missing,
    /// After `Resume` the successor plays briefly and then stops dead.
    SecondFailure,
    /// As `Healthy` up to `Resume`; the successor then plays briefly with a
    /// 70 ms and a 150 ms slow frame mid-race.
    SlowFrames,
    /// As `Healthy` to the results under the successor; survivor A's link
    /// then drops with no `Leave` and never comes back.
    GhostInResults,
}

#[test]
fn remote_survivors_resume_on_the_promoted_standby_at_frozen_time() {
    survivor_takeover(Takeover::Healthy);
}

#[test]
fn a_survivor_missing_its_sealed_record_keeps_recovery_paused_until_the_deadline() {
    survivor_takeover(Takeover::Missing);
}

#[test]
fn a_lagging_ready_does_not_run_the_other_survivors_clock_past_c() {
    survivor_takeover(Takeover::LateReady);
}

#[test]
fn a_second_host_failure_is_not_taken_over_and_ends_the_session() {
    survivor_takeover(Takeover::SecondFailure);
}

/// The successor's parked departure (the original's 30 s rule,
/// `PARKED_DEPARTURE_MS`): after `Resume` the race plays to results, then
/// survivor A's link drops with no `Leave` and A never reconnects. Everyone
/// else readies. A stays seated while it has been gone for at most 30 s of
/// game time, so the rematch waits; then the successor departs it by the
/// native kick, its seat and outbox retire, and the rematch starts with the
/// remaining humans.
#[test]
fn a_survivor_lost_in_the_results_departs_after_thirty_seconds_and_the_rematch_starts() {
    survivor_takeover(Takeover::GhostInResults);
}

/// Term 1: a slow frame on the promoted device is staged as back-to-back
/// batches of at most 50 ms; the room carries on and the native world equals
/// a plain host's over the same elapsed time.
#[test]
fn a_slow_frame_under_the_successor_is_split_into_50_ms_batches() {
    survivor_takeover(Takeover::SlowFrames);
}

/// Two remote humans (real `PlayerEndpoint`s on session-bound QUIC), the
/// standby device's own seat, a third remote human and four bots race. Each
/// of the two prepared a control link to the standby. The original then
/// stops dead mid-race without closing anything (its runtime and listener are
/// never polled again). The standby promotes on its real 200 ms watch; each
/// survivor frames on until its own loss detection, then recovers over its
/// prepared link at frozen C, while the third human (no prepared link) is
/// absent and departs after the drain.
fn survivor_takeover(mode: Takeover) {
    let missing = mode == Takeover::Missing;
    let mut room = ArmedRoom::new();
    let root = room.root().clone();
    let code = room.players.network().code().unwrap().to_owned();
    let standby_at = loopback(room.standby.listener().unwrap(), root.standby);
    let warm_binding = session_binding(&root, 0, LinkRole::ControlPlayer, LinkRole::Standby);
    let mut warm = Vec::new();
    for i in 1..3 {
        let (id, cfg) = (room.identities[i].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        });
        warm.push(link.expect("prepared control link"));
    }
    let a = room.remote(1);
    let (b, b_net, _) = room.tapped_remote(2);
    let ids = [room.player(1), room.player(2)];
    let mut waiting = [Some(a), Some(b)];
    for (r, link) in warm.into_iter().enumerate() {
        waiting[r].as_mut().unwrap().prepare_standby(link).unwrap();
    }
    let absent = room.player(3);
    // The standby device's own seat keeps a synced clock and plays: its
    // client runs every frame in term 0, and its input views and intents
    // reach the original over the own-seat endpoint (which shares the pair
    // link's sender) and are committed with every batch. So at C its client
    // holds unconfirmed inputs, which it must resend after `Resume`.
    sync_clock(room.clients[0].as_mut().unwrap(), room.base + 1);
    let mut own_client = room.clients[0].take().unwrap();
    let (mut own_t0, _) = {
        let (descriptor, keys, initial) = (
            room.descriptor.clone(),
            room.identities[0].keys().clone(),
            room.initial,
        );
        own_link(
            &descriptor,
            &mut room.standby,
            &mut room.live,
            &keys,
            initial,
        )
    };
    // Race; once B's frame sent pings, B's device stalls (runs no frame) while
    // A plays on, until C carries reliable outputs (pongs) that neither
    // client applied. B's never arrive: they die in transit with the original.
    //
    // The cut is placed by the fixture, not by timing. During the race the
    // only reliable outputs are pongs, so A has unapplied outputs only while
    // its own ping round runs; left alone, A's and B's rounds drift apart
    // (each restarts 2.5 s after its last pong arrived) and B could sit
    // stalled past `FREEZE_AFTER_MS`, freeze in the drop frame and journal no
    // intent at all. So:
    // - both rounds are aligned through the client's own round abort (the
    //   offset is kept), so A and B ping in the same frame and B stalls there;
    // - the cut is taken only while B's drop frame cannot be frozen (its last
    //   fresh progress is under `FREEZE_AFTER_MS` before the drop frame),
    //   which the fixture reads from B's own loss-detection state;
    // - if A's pongs are late (a loaded machine), B is released and both
    //   rounds are aligned again, rather than cutting a frozen B.
    let mut index = 0;
    let mut b_stalled = false;
    let mut stalled_at = 0;
    let mut align = true;
    let mut realigned = 0;
    let mut last_local = [0u64; 2];
    let mut last_progress = [0u64; 2];
    let w_last_commit;
    loop {
        index += 1;
        let now = room.base + index * STEP;
        let racing = room
            .live
            .authority()
            .world()
            .is_some_and(|w| w.phase == RacePhase::Racing);
        let unapplied = (0..2).all(|r| {
            let applied = waiting[r]
                .as_ref()
                .unwrap()
                .endpoint()
                .journals()
                .0
                .applied();
            room.live
                .primary()
                .output_ceiling(ids[r], Delivery::Reliable)
                > applied
        });
        // B's drop frame at `now` runs its client step (so it journals the
        // drop frame's intent) only if B's loss detection is not stale then.
        let b_fresh = waiting[1]
            .as_ref()
            .unwrap()
            .last_progress()
            .is_some_and(|p| now.saturating_sub(p) < migration::FREEZE_AFTER_MS);
        if b_stalled && unapplied && b_fresh {
            // The standby's watch runs from its last proposal, this one.
            w_last_commit = room.wall();
            break;
        }
        if b_stalled && !b_fresh {
            b_stalled = false;
            align = true;
            realigned += 1;
            assert!(
                realigned < 20,
                "A's pongs never arrived within B's freeze window"
            );
        }
        let aligning = align && racing && index > 30;
        for (r, remote) in waiting.iter_mut().enumerate() {
            if r == 1 && b_stalled {
                continue;
            }
            let prev = last_local[r];
            let step = remote
                .as_mut()
                .unwrap()
                .frame_with(now, room.wall(), remote_input(index, r), |client| {
                    if aligning {
                        // Abort the open round at the client's previous frame
                        // time, keeping its offset: this frame opens a fresh
                        // round and sends its first ping.
                        let host = client.host_now(prev).expect("synced remote clock");
                        client.migration_rebase_clock(prev, host, 0).unwrap();
                    }
                })
                .unwrap();
            last_local[r] = now;
            if step.newest_receipt.is_some() {
                last_progress[r] = now;
            }
            let pinged = step.intents.iter().any(|(_, b)| ping_of(b).is_some());
            assert!(!aligning || pinged, "an aligned round sent no ping");
            if r == 1 && aligning {
                b_stalled = true;
                stalled_at = index;
            }
        }
        align &= !aligning;
        // (Its clock was synced at `base + 41`; its frames start after that.)
        if index > 1 {
            own_step(
                &mut own_t0,
                &mut own_client,
                &room.live,
                now,
                room.wall(),
                true,
                0,
            );
        }
        let staged = own_ack_staged(&own_t0, &room.live);
        room.commit_with(index, Some(&mut own_t0.queue));
        own_ack_committed(&mut own_t0, &room.live, staged);
        assert!(index < 1200, "no racing commit with outputs in flight");
    }
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(30) {
        while b_net.lock().unwrap().poll().is_some() {}
        std::thread::sleep(Duration::from_millis(1));
    }
    // The drop frame: each client sends one reliable intent (a character
    // command) and any ping due; none reaches a commit. The original stops
    // right after it.
    let drop_local = room.base + index * STEP;
    let drop_at = Instant::now();
    let w_drop = room.wall();
    let mut drop_frozen = [false; 2];
    for (r, remote) in waiting.iter_mut().enumerate() {
        let character = 2 + r as u8;
        let step = remote
            .as_mut()
            .unwrap()
            .frame_with(drop_local, room.wall(), remote_input(index, r), |client| {
                client.set_character(character)
            })
            .unwrap();
        drop_frozen[r] = step.frozen;
        if step.newest_receipt.is_some() {
            last_progress[r] = drop_local;
        }
    }
    // The own seat's drop frame: its views and intents never commit.
    own_step(
        &mut own_t0,
        &mut own_client,
        &room.live,
        drop_local,
        room.wall(),
        true,
        0,
    );
    // Its unconfirmed window at C: input numbers past C's tick that its
    // client produced (its lead) and the original never ran.
    let own_next_at_c = own_client.next_input().expect("the own seat races");
    let c_receipt = room.standby.receipt().clone();
    assert_eq!(c_receipt.index, index - 1);
    let c_logical = c_receipt.logical_ms;
    let c_session = SessionImage::decode(&root, room.standby.image()).unwrap();
    let c_auth = room.standby.committed_authority().unwrap();
    let ceiling_c = ids.map(|p| room.live.primary().output_ceiling(p, Delivery::Reliable));
    let mark_c = |p: u64| {
        c_receipt
            .watermarks
            .iter()
            .find(|m| m.player == p)
            .unwrap()
            .clone()
    };
    let handled_c = ids.map(|p| mark_c(p).input);
    let journal = |r: usize| waiting[r].as_ref().unwrap().endpoint().journals();
    let applied_at_drop = [journal(0).0.applied(), journal(1).0.applied()];
    let tail_at_c = [journal(0).1.tail(), journal(1).1.tail()];
    let pending: Vec<usize> = (0..2)
        .map(|r| {
            journal(r)
                .1
                .pending()
                .filter(|(n, _)| *n > handled_c[r])
                .count()
        })
        .collect();
    let pings_at_c: Vec<usize> = (0..2)
        .map(|r| {
            journal(r)
                .1
                .pending()
                .filter(|(n, _)| *n > handled_c[r])
                .filter(|(_, b)| ping_of(b).is_some())
                .count()
        })
        .collect();
    assert!(
        pending.iter().all(|n| *n > 0),
        "no intent in flight at C: pending={pending:?} pings={pings_at_c:?} drop_frozen={drop_frozen:?} stalled_at={stalled_at} c={} last_progress={last_progress:?} drop_local={drop_local}",
        index - 1
    );
    assert_eq!(drop_frozen, [false, false], "a client froze before the cut");
    // A applied C's outputs but its ACK is uncommitted; B never got them.
    let ack_in_flight = applied_at_drop[0] > mark_c(ids[0]).output_ack;
    let outputs_in_flight = ceiling_c[1] > applied_at_drop[1];
    assert!(
        ack_in_flight && outputs_in_flight,
        "{applied_at_drop:?} {ceiling_c:?}"
    );
    // The own seat's signed `Applied` ACKs committed through C and pruned
    // what it applied; its drop frame's ACK (if any) is queued, uncommitted.
    let own_ack_at_c = mark_c(own_t0.queue.player()).output_ack;
    assert!(
        own_t0.pruned > 0 && own_ack_at_c > 0,
        "Applied ACKs did not prune the outbox: pruned={} committed_ack={own_ack_at_c}",
        own_t0.pruned
    );
    assert!(own_ack_at_c <= own_t0.endpoint.journals().0.applied());
    assert_eq!(
        own_t0.queue.output_ack(),
        own_t0.endpoint.journals().0.applied()
    );

    // The original stops dead: no close reaches anyone. Its runtime and its
    // player listener stay up but are never polled again; the absent
    // human's device goes away.
    let ArmedRoom {
        cfg,
        descriptor,
        identities,
        clients: _,
        networks,
        live,
        players,
        standby,
        clock,
        ..
    } = room;
    let hung = (live, players);
    drop(networks);
    let wall = || clock.now_ms();
    let local = |w: u64| drop_local + w.saturating_sub(w_drop);
    let _ = last_progress;
    let mut devices = SurvivorDevices {
        waiting,
        recovering: [None, None],
        last_frame: [drop_local; 2],
        freeze: [None; 2],
        frames: [0; 2],
        frozen_by: [None; 2],
        tail: [None; 2],
        pings: [Vec::new(), Vec::new()],
        resumed_local: [None; 2],
        applied: [0; 2],
        duplicates: [0; 2],
        discarded: 0,
    };
    // The standby's real watch on monotonic time; the survivors frame on.
    let mut standby = Some(standby);
    // The own seat's device frames on too, until its own freeze (`FREEZE_AFTER_MS`
    // after C's commit) or its promotion. Its views and intents go to the
    // hung original and never commit: unconfirmed inputs past C's tick.
    let own_freeze = drop_local - STEP + migration::FREEZE_AFTER_MS;
    let mut own_last = drop_local;
    let mut own_frames_after_drop = 0;
    let (promoted, at) = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        let l = local(w);
        if l < own_freeze && l >= own_last + STEP {
            own_step(&mut own_t0, &mut own_client, &hung.0, l, w, true, 0);
            own_last = l;
            own_frames_after_drop += 1;
        }
        match standby.take().unwrap().promote() {
            Ok(promoted) => {
                let at = promoted.promoted_at_ms();
                break (promoted, at);
            }
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            drop_at.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let watch_ms = at - w_drop;
    let rc = |w: u64| w;
    let keys = identities[0].keys().clone();
    let (mut links, mut recovery) = migration::SurvivorLinks::open(promoted.into_link()).unwrap();
    let mut players: Vec<u64> = links.players().collect();
    players.sort();
    let mut routed = ids.to_vec();
    routed.sort();
    assert_eq!(players, routed, "every prepared link is routed");
    let own_next_at_freeze = own_client.next_input().unwrap();
    // The own seat recovers from its actual client and its endpoint's
    // journals since arming.
    let (own_delivery, own_intents) = {
        let (delivery, intents) = own_t0.endpoint.journals();
        (
            DeliveryJournal::restore(&delivery.checkpoint().unwrap()).unwrap(),
            IntentJournal::restore(&intents.checkpoint().unwrap()).unwrap(),
        )
    };
    drop(own_t0);
    let mut own = migration::OwnSeat::new(
        &root,
        recovery.sequencer().image(),
        own_client,
        own_delivery,
        own_intents,
    )
    .unwrap();
    let mut successor_host = migration::start_resume_host(
        &cfg,
        &identities[0],
        "Resumed",
        &code,
        descriptor.clone(),
        recovery.promotion().clone(),
    )
    .unwrap();

    let mut term1: BTreeMap<(u64, u64), Vec<u8>> = BTreeMap::new();
    let mut refusals: Option<[bool; 3]> = None;
    let mut stalled = false;
    let mut paused_phases = BTreeSet::new();
    // The departure must commit after the survivors' drain: watched on the
    // committed history itself, not only through Fray's phase gate.
    let mut order = DrainOrder {
        root: root.clone(),
        ids,
        absent,
        drained_at: None,
        retired_at: None,
    };
    // A late absent client sends a refused (empty) frame mid-recovery.
    let mut bad_late: Option<QuicClient> = None;
    let mut bad_refused: Option<(usize, RecoveryPhase)> = None;
    let failure = loop {
        let w = wall();
        let a_ok = links.accepted(ids[0]);
        let hold_b = refusals.is_none() && !missing && links.challenge_of(ids[1]).is_some();
        let hold = [false, hold_b || stalled];
        devices.step(w, local(w), hold, handled_c);
        if !missing && refusals.is_none() && a_ok && hold_b {
            // B's challenge is still pending on B's own connection.
            let (conn_a, tls_a, _) = links.link_of(ids[0]).unwrap();
            let wrong_key = recovery.challenge(
                conn_a,
                tls_a,
                ids[0],
                identities[2].pubkey().to_bytes(),
                &keys,
                rc(w),
            );
            let replay = links.proof_of(ids[0]).unwrap().to_vec();
            let replayed = links.offer_proof(&mut recovery, ids[0], &replay, &keys, rc(w));
            let challenge = links.challenge_of(ids[1]).unwrap().clone();
            let forged = ResumeProof::sign(
                &root,
                &challenge,
                challenge.tls,
                applied_at_drop[1],
                tail_at_c[1],
                1 << 40,
                identities[2].keys(),
            )
            .unwrap()
            .encode(&root)
            .unwrap();
            let other = links.offer_proof(&mut recovery, ids[0], &forged, &keys, rc(w));
            let checks = [
                wrong_key.err() == Some(Error::Unauthorized),
                replayed == Ok(false),
                other == Ok(false),
            ];
            assert!(checks[0], "wrong key");
            assert!(checks[1], "a replayed proof was accepted");
            assert!(
                checks[2],
                "a proof bound to another connection was accepted"
            );
            assert!(!links.accepted(ids[1]) && links.accepted(ids[0]));
            refusals = Some(checks);
        }
        if mode == Takeover::LateReady
            && bad_late.is_none()
            && !matches!(
                recovery.phase(),
                RecoveryPhase::Collecting | RecoveryPhase::Ready
            )
        {
            // Sealed and mid-recovery (in this run, which is not timed): the
            // absent human's device dials the standby and sends one empty
            // reliable frame. Only that link is
            // closed; recovery is driven (and the survivors held) until the
            // refusal shows, so it lands before Ready.
            let mut late = dial(
                &cfg,
                &identities[3],
                standby_at.clone(),
                warm_binding.clone(),
                || {},
            )
            .expect("late absent link");
            late.send_reliable(Vec::new());
            let before = links.refused;
            let t = Instant::now();
            let mut closed = false;
            while !closed {
                let w = wall();
                links
                    .drive(&mut recovery, Some(&mut own), &keys, local(w), rc(w))
                    .expect("a refused frame failed recovery");
                order.mark(&recovery, &devices.tail);
                while let Some(event) = late.poll() {
                    closed |= matches!(
                        event,
                        ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
                    );
                }
                bounded(t);
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(links.refused > before, "the bad link was not refused");
            assert!(links.link_of(absent).is_none(), "bad link still routed");
            bad_refused = Some((links.refused - before, recovery.phase()));
            bad_late = Some(late);
            continue;
        }
        let phase = links.drive(&mut recovery, Some(&mut own), &keys, local(w), rc(w));
        if missing && links.accepted(ids[1]) {
            stalled = true;
        }
        let phase = match phase {
            Ok(phase) => phase,
            Err(error) => break Some(error),
        };
        if matches!(phase, RecoveryPhase::Replay | RecoveryPhase::Draining) {
            for record in recovery.replay().unwrap() {
                if record.receipt.term == 1 && record.output.delivery == Delivery::Reliable {
                    term1.insert(
                        (record.output.recipient, record.output.sequence),
                        record.output.bytes.clone(),
                    );
                }
            }
        }
        order.mark(&recovery, &devices.tail);
        if phase == RecoveryPhase::Ready {
            break None;
        }
        if missing && stalled {
            paused_phases.insert(format!("{phase:?}"));
            if wall() - at > 600 {
                // Nothing arrives: run the recovery clock to the deadline.
                let late = links.drive(&mut recovery, Some(&mut own), &keys, local(w), at + 5000);
                break late.err();
            }
        }
        assert!(wall() - at < 5000, "recovery stalled in {phase:?}");
        std::thread::sleep(Duration::from_millis(1));
    };
    let history = |recovery: &Recovery, p: u64| {
        SessionImage::decode(&root, recovery.sequencer().image())
            .unwrap()
            .inputs[&p]
            .handled()
    };
    if missing {
        let error = failure.expect("recovery did not fail at the deadline");
        assert_eq!(error, Error::Invalid("recovery deadline"));
        assert_eq!(recovery.phase(), RecoveryPhase::Failed);
        let receipt = recovery.sequencer().receipt().clone();
        let handled = ids.map(|p| history(&recovery, p));
        let a_tail = (handled[0], devices.tail[0], handled_c[0]);
        assert_eq!(handled, handled_c, "a sealed drain ran: {a_tail:?}");
        let retired: Vec<u64> = receipt
            .watermarks
            .iter()
            .filter(|m| m.retired)
            .map(|m| m.player)
            .collect();
        assert!(retired.is_empty(), "a departure ran: {retired:?}");
        let members = SessionImage::decode(&root, recovery.sequencer().image())
            .unwrap()
            .members;
        assert_eq!(members, c_session.members, "a seat changed");
        assert!(!links.accepted(absent));
        println!(
            "STAMPEDE SURVIVOR MISSING RECORD paused_in={paused_phases:?} reason={error:?} phase={:?} drains=0 departures=0 seats_unchanged=true absent_reclaimed=false receipt_index={}",
            recovery.phase(),
            receipt.index
        );
        return;
    }
    assert!(failure.is_none(), "recovery failed: {failure:?}");
    let ready_ms = drop_at.elapsed().as_millis();
    // The original's process is gone for good now.
    drop(hung);
    if mode == Takeover::LateReady {
        let (_, phase) = bad_refused.expect("no bad frame was sent mid-recovery");
        assert_ne!(phase, RecoveryPhase::Ready);
    }
    let (drained_at, retired_at) = (order.drained_at.unwrap(), order.retired_at.unwrap());
    assert!(
        drained_at < retired_at,
        "departure committed at {retired_at}, survivors drained at {drained_at}"
    );

    // A late absent client: its prepared link only arrives now. It is never
    // challenged, cannot be, and gets no final cut.
    drop(bad_late);
    let late = dial(
        &cfg,
        &identities[3],
        standby_at.clone(),
        warm_binding.clone(),
        || {},
    )
    .unwrap();
    let t = Instant::now();
    while links.link_of(absent).is_none() {
        let w = wall();
        links
            .drive(&mut recovery, Some(&mut own), &keys, local(w), rc(w))
            .unwrap();
        bounded(t);
    }
    let (conn3, tls3, key3) = links.link_of(absent).unwrap();
    let late_challenge = recovery.challenge(conn3, tls3, absent, key3, &keys, rc(wall()));
    assert!(late_challenge.is_err(), "late absent client challenged");

    // Final barrier: each survivor's final cut and clock correction.
    let mut barrier = links.barrier(recovery, &keys, rc(wall())).unwrap();
    let late_cut = barrier.issue(absent, tls3, &keys, rc(wall()));
    assert_eq!(
        late_cut.err(),
        Some(Error::Unauthorized),
        "late absent client got a cut"
    );
    // The own seat froze by the same client rule: at its device's promotion
    // or `FREEZE_AFTER_MS` after its last commit (C, one frame before the drop frame).
    let own_frozen = local(at).min(drop_local - STEP + migration::FREEZE_AFTER_MS - 1);
    let own_offset = own.client().clock_offset_ms();
    let over = own.ready(&mut barrier, own_frozen + 300, local(wall()), rc(wall()));
    assert_eq!(over.err(), Some(Error::Unauthorized), "over-cap correction");
    assert_eq!(own.client().clock_offset_ms(), own_offset, "client touched");
    let own_expected = own.client().host_now(own_frozen).unwrap() as i64 - c_logical as i64;
    let own_cut_local = local(wall());
    let own_delta = own
        .ready(&mut barrier, own_frozen, own_cut_local, rc(wall()))
        .unwrap();
    assert_eq!(own_delta, own_expected);
    assert_eq!(own.client().host_now(own_cut_local), Some(c_logical));
    let late_ready = mode == Takeover::LateReady;
    let mut a_cut_wall: Option<u64> = None;
    let mut b_cut_wall: Option<u64> = None;
    let t = Instant::now();
    loop {
        let w = wall();
        let hold_b = late_ready && a_cut_wall.is_none_or(|a| w < a + 1500);
        devices.step(w, local(w), [false, hold_b], handled_c);
        if a_cut_wall.is_none() && devices.recovering(0).clock_delta().is_some() {
            a_cut_wall = Some(w);
        }
        if b_cut_wall.is_none() && devices.recovering(1).clock_delta().is_some() {
            b_cut_wall = Some(w);
        }
        if links.drive_barrier(&mut barrier, rc(wall())).unwrap() {
            break;
        }
        bounded(t);
        std::thread::sleep(Duration::from_millis(1));
    }
    let ready_lag = b_cut_wall.unwrap() as i64 - a_cut_wall.unwrap() as i64;
    let (successor, resume) = links.resume(barrier, &keys, rc(wall())).unwrap();
    let own_resume_local = local(wall());
    own.resumed(own_resume_local).unwrap();
    assert_eq!(own.client().host_now(own_resume_local), Some(c_logical));
    let t = Instant::now();
    while devices.resumed_local.iter().any(Option::is_none) {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        bounded(t);
    }
    let resumed_ms = drop_at.elapsed().as_millis();
    let resumed_after_drop = devices.resumed_local.map(|l| l.unwrap() - drop_local);
    let deltas = [0, 1].map(|r| devices.recovering(r).clock_delta().unwrap());
    let open_at_cut = [0, 1].map(|r| devices.recovering(r).round_open_at_cut().unwrap());
    let frozen_local = [0, 1].map(|r| devices.recovering(r).frozen_local());
    let rounds_open = [0, 1].map(|r| devices.recovering(r).client().clock_sync().round_open());
    let next_at_freeze = [0, 1].map(|r| devices.recovering(r).client().next_input().unwrap());
    let SurvivorDevices {
        recovering,
        applied,
        duplicates,
        discarded,
        frozen_by,
        frames,
        tail,
        pings,
        resumed_local,
        freeze,
        ..
    } = devices;
    let tail = tail.map(Option::unwrap);
    let resumed_local = resumed_local.map(Option::unwrap);
    let mut survivors: Vec<migration::RemotePlayer> = recovering
        .into_iter()
        .map(|r| r.unwrap().into_resumed().unwrap())
        .collect();

    // N2: each survivor's own endpoint journals at Resume: every committed
    // reliable output applied, every intent drained and acknowledged.
    for r in 0..2 {
        let (delivery, intents) = survivors[r].endpoint().journals();
        let ceiling = successor
            .primary()
            .output_ceiling(ids[r], Delivery::Reliable);
        assert_eq!(
            (delivery.applied(), intents.tail(), intents.acknowledged()),
            (ceiling, tail[r], tail[r]),
            "survivor {r} endpoint journals at Resume"
        );
        assert!(survivors[r].under_successor() && !survivors[r].protected());
    }
    // A survivor the library froze by its own rule froze at the constant.
    // The rule, exactly: the freeze lands on the device's first frame at or
    // past progress + `FREEZE_AFTER_MS`, and its previous frame was still
    // under it. How far past depends on when the (wall-paced) device got its
    // next frame, which a loaded machine delays, so that is not bounded here.
    let freeze_after = freeze.map(|f| f.map(|(at, progress, _)| at - progress));
    for (at, progress, prev) in freeze.iter().flatten() {
        assert!(
            *at >= progress + migration::FREEZE_AFTER_MS
                && *prev < progress + migration::FREEZE_AFTER_MS,
            "froze {} ms after its last progress (previous frame {} ms after)",
            at - progress,
            prev.saturating_sub(*progress)
        );
    }

    // Same session, code and match, term 1, still at C.
    let image = SessionImage::decode(&root, successor.image()).unwrap();
    assert_eq!(successor.receipt().term, 1);
    assert_eq!(successor.receipt().logical_ms, c_logical);
    assert_eq!(image.logical_ms, c_logical);
    assert_eq!(image.match_id, c_session.match_id);
    let t = Instant::now();
    while !successor_host.is_ready() {
        assert!(
            successor_host.poll().is_none(),
            "no peer before the room opens"
        );
        bounded(t);
    }
    assert_eq!(successor_host.code(), Some(code.as_str()));
    let resume_open = successor_host.handle().unwrap().is_open();
    // Survivors: every pending intent drained once, outputs applied once.
    let mut pongs_once = true;
    for r in 0..2 {
        let p = ids[r];
        assert_eq!(image.inputs[&p].handled(), tail[r], "survivor {r} drain");
        let ceiling = successor.primary().output_ceiling(p, Delivery::Reliable);
        assert_eq!(
            applied[r] as u64,
            ceiling - applied_at_drop[r],
            "survivor {r} applied once"
        );
        let mark = resume.cuts.iter().find(|c| c.player == p).unwrap();
        assert_eq!(
            (mark.input, mark.output, mark.clock_delta),
            (tail[r], ceiling, deltas[r])
        );
        for id in &pings[r] {
            let n = term1
                .iter()
                .filter(|((q, _), b)| *q == p && pong_of(b).is_some_and(|(i, _, _)| i == *id))
                .count();
            pongs_once &= n == 1;
        }
    }
    assert!(pongs_once, "each in-flight ping got exactly one pong");
    assert!(deltas.iter().all(|d| d.unsigned_abs() <= 250), "{deltas:?}");
    assert_eq!(duplicates, [0, 0], "a replayed output applied twice");
    // B's pongs died in transit: its round was open at the cut, and the
    // rebase aborted it.
    assert!(open_at_cut[1], "no clock-sync round open at the cut");
    assert_eq!(
        rounds_open,
        [false, false],
        "a clock-sync round stayed open"
    );
    assert_eq!(discarded, 0);
    // Native: C plus the drains (pongs only) and the absent departure.
    let end = migration::committed_authority(&root, successor.image()).unwrap();
    let (cw, ew) = (c_auth.world().unwrap(), end.world().unwrap());
    let frozen = cw.tick == ew.tick;
    assert!(frozen, "game time moved");
    assert_eq!(cw.racers.len(), ew.racers.len());
    let unmoved = cw
        .racers
        .iter()
        .zip(&ew.racers)
        .all(|(x, y)| format!("{:?}{:?}", x.pos, x.vel) == format!("{:?}{:?}", y.pos, y.vel));
    assert!(unmoved, "a racer moved at frozen time");
    let departed = end.human_count() + 1 == c_auth.human_count();
    assert!(departed, "absent human not departed");
    // The departed human's racer, read from the authority at C: still racing
    // there, so the move must list exactly it as Left.
    let departed_racer = {
        let seat = c_session
            .members
            .iter()
            .find(|m| m.player == absent)
            .expect("the absent human is in C's roster")
            .seat;
        let racer = c_auth.racer_of(seat).expect("the absent human raced");
        assert!(
            c_auth.world().unwrap().racers[racer].finished_at.is_none(),
            "the absent human had not finished at C"
        );
        racer as u8
    };
    let seats_kept = ids.iter().all(|p| {
        image
            .members
            .iter()
            .find(|m| m.player == *p)
            .map(|m| m.seat)
            == c_session
                .members
                .iter()
                .find(|m| m.player == *p)
                .map(|m| m.seat)
    });
    assert!(seats_kept);
    assert!(!image.members.iter().any(|m| m.player == absent));

    // Strangers and fresh joins on the same code are refused; the room stays.
    let stranger = approved();
    let on_standby = dial(
        &cfg,
        &stranger,
        standby_at.clone(),
        warm_binding.clone(),
        || {},
    );
    let resume_at = listener_of(&successor_host, root.standby);
    let resume_binding = session_binding(&root, 1, LinkRole::ResumePlayer, LinkRole::Successor);
    let on_resume = dial(&cfg, &stranger, resume_at.clone(), resume_binding, || {});
    let fresh = decented_net::join_addr(&cfg, &stranger, resume_at.0, &resume_at.1);
    let mut fresh = QuicClient::new(fresh);
    let t = Instant::now();
    let mut fresh_connected = false;
    while t.elapsed() < Duration::from_millis(1500) {
        match fresh.poll() {
            Some(ClientNetEvent::Connected { .. }) => fresh_connected = true,
            Some(ClientNetEvent::Failed(_)) | Some(ClientNetEvent::Disconnected { .. }) => break,
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(on_standby.is_none() && on_resume.is_none() && !fresh_connected && !resume_open);
    let unharmed =
        successor.failure().is_none() && survivors.iter().all(|s| s.endpoint().failure().is_none());
    assert!(unharmed);
    let same_code =
        successor_host.code() == Some(code.as_str()) && image.match_id == c_session.match_id;
    drop(late);
    let frozen_after = frozen_local.map(|f| f - drop_local);
    println!(
        "STAMPEDE SURVIVOR TAKEOVER mode={mode:?} survivors=2 absent=1 c_index={} c_logical_ms={c_logical} same_code={same_code} resume_host_open={resume_open} intents_in_flight={pending:?} pings_in_flight={pings_at_c:?} ack_in_flight={ack_in_flight} outputs_in_flight={outputs_in_flight} applied={applied:?} duplicates={duplicates:?} pongs_once={pongs_once} handled={:?} round_open_at_cut={open_at_cut:?} rounds_open={rounds_open:?} drained_at={drained_at} departed_at={retired_at} game_time_frozen={frozen} racers_unmoved={unmoved} absent_departed={departed} seats_kept={seats_kept} ready_ms={ready_ms} cut_realigned={realigned} stall_frames={} freeze_after_progress_ms={freeze_after:?} own_next_input_at_c={own_next_at_c} own_frames_after_drop={own_frames_after_drop} own_inputs_after_c={}",
        c_receipt.index,
        ids.map(|p| image.inputs[&p].handled()),
        index - stalled_at,
        own_next_at_freeze - own_next_at_c,
    );
    println!(
        "STAMPEDE SURVIVOR CLOCK mode={mode:?} watch_ms={watch_ms} frozen_by={frozen_by:?} frames_after_drop={frames:?} frozen_ms_after_drop={frozen_after:?} clock_deltas={deltas:?} own_delta={own_delta} cap=250 margin={:?} own_margin={}",
        deltas.map(|d| 250 - d.unsigned_abs() as i64),
        250 - own_delta.unsigned_abs() as i64,
    );
    println!(
        "STAMPEDE SURVIVOR RECOVERY mode={mode:?} ms drop_to_resume={resumed_after_drop:?} all_resumed={resumed_ms} (monotonic from the drop frame, includes the {watch_ms} ms watch) last_commit_to_resume={} ready_lag_ms={ready_lag}",
        resumed_ms as u64 + w_drop - w_last_commit,
    );
    let [wk, rp, oc] = refusals.unwrap();
    println!(
        "STAMPEDE SURVIVOR REFUSALS wrong_key={wk} replayed_proof={rp} other_connection={oc} bad_frame_closed_in={:?} late_absent_challenge={} late_absent_cut=true stranger_standby={} stranger_resume={} fresh_join={} refused={} room_unharmed={unharmed}",
        bad_refused.map(|(_, phase)| phase),
        late_challenge.is_err(),
        on_standby.is_none(),
        on_resume.is_none(),
        !fresh_connected,
        links.refused,
    );

    // Part B: the successor runs the match alone, unprotected. Lockstep at
    // STEP on the devices' local clocks (fast-forwarded past wall time);
    // the playing humans are driven by the native bot as an autopilot.
    let resume_binding = session_binding(&root, 1, LinkRole::ResumePlayer, LinkRole::Successor);
    let mut live = links
        .into_successor(successor, Some(own), Some(successor_host), keys.clone())
        .unwrap();
    assert!(!live.protected());
    let seat_of = |i: usize| {
        let key = identities[i].pubkey().to_bytes();
        root.members
            .iter()
            .find(|m| m.key == Some(key))
            .unwrap()
            .seat
    };
    let seats = [seat_of(0), seat_of(1), seat_of(2)];
    let own_player = {
        let key = identities[0].pubkey().to_bytes();
        root.members
            .iter()
            .find(|m| m.key == Some(key))
            .unwrap()
            .player
    };
    let autopilot = |a: &HostAuthority, seat: u8| match (a.world(), a.racer_of(seat)) {
        (Some(w), Some(i)) if i < w.racers.len() && w.phase == RacePhase::Racing => {
            race_core::bot_input(w, i, race_core::BotLevel::Normal)
        }
        _ => Input::default(),
    };
    // Each survivor's and the own seat's own `Finished` and `RaceOver`, as
    // their views surfaced them to the app, up to the results.
    let ends = std::cell::RefCell::new([RaceEnd::default(); 3]);
    let base_local = resumed_local
        .iter()
        .copied()
        .max()
        .unwrap()
        .max(own_resume_local);
    let humans_at_resume = live.authority().human_count();
    // Starvation after Resume, measured on the successor itself: input
    // numbers above C's tick that the world ran without the seat's input.
    let tick_c = live.authority().world().unwrap().tick as u32;
    let starved_c = live.authority().stats().starved_ticks;
    let mut starved_after: Option<([usize; 3], u64)> = None;
    let mut first_input: [Option<u64>; 2] = [None; 2];
    let mut own_resent = 0usize;
    let mut own_inject: Option<(usize, bool, usize)> = None;
    let mut last_own = 0usize;
    let mut committed = live.receipt().index;
    let mut k = 0u64;
    let mut dups = [0usize; 2];
    let mut first_frame: [Option<(i64, u32, usize, usize)>; 2] = [None; 2];
    let mut own_first_host: Option<i64> = None;
    let mut fresh_round = [false; 2];
    let mut gate_probes = 0;
    let mut max_outbox = 0usize;
    let mut own_views = 0usize;
    let play_wall = Instant::now();
    // A survivor's link drop after Resume, and its reconnect: requested
    // from outside the batch loop, carried out inside it.
    let link_drop = std::cell::Cell::new(false);
    let relink: std::cell::RefCell<Option<QuicClient>> = Default::default();
    let away = std::cell::Cell::new(false);
    let relinked = std::cell::Cell::new(false);
    // The results, rematch and lobby flow: requests and observed facts.
    let ready_all = std::cell::Cell::new(false);
    let lobby_op = std::cell::Cell::new(false);
    let check_results = std::cell::Cell::new(false);
    let results_facts: std::cell::RefCell<Option<(usize, usize, bool, bool)>> = Default::default();
    // (race start, racing, every seat seated, lobby)
    let facts = std::cell::Cell::new((None::<u64>, false, false, false));
    // (humans, committed game time, survivor A a member, A's outbox), for
    // `GhostInResults` only.
    let ghost = std::cell::Cell::new((0usize, 0u64, false, 0usize));
    let frozen_frames = std::cell::Cell::new(0usize);
    // Frozen frames per survivor and native state (lobby, countdown, race,
    // results) over the whole healthy play: none may be false.
    let false_freezes = std::cell::Cell::new([[0usize; 4]; 2]);
    // Survivor 0's link drop: from the drop until a reconnected frame sees
    // fresh progress, its freezes are real (no commit reached it), counted
    // apart.
    let catching_up = std::cell::Cell::new(false);
    let reconnect_freezes = std::cell::Cell::new(0usize);
    let idle_seen = std::cell::Cell::new(0usize);
    // A slow frame on the promoted device (requested from outside the batch
    // loop): no device frame runs for its gap; game time moves on by it.
    let slow_gap: std::cell::Cell<Option<u64>> = Default::default();
    let slow_seen: std::cell::RefCell<Vec<(u64, u64, bool)>> = Default::default();
    let mut shift = 0u64;
    let mut tick = |probe: bool| -> bool {
        if let Some(gap) = slow_gap.take() {
            // Frames in flight are absorbed by one batch at the same game
            // time, so the gap's batches carry no input and compare with a
            // plain host running the same elapsed time in one update.
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(30) {
                live.poll(wall()).unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
            let at = live.receipt().logical_ms;
            assert_eq!(live.stage(at, |_| Ok(())), Ok(true));
            committed = live.commit(wall()).unwrap().index;
            let (binding, bytes) = migration::checkpoint(live.authority()).unwrap();
            let mut plain =
                migration::restore(&bytes, &binding, Box::new(Script::default())).unwrap();
            plain.update(at + gap);
            let mut batches = 0;
            while live.receipt().logical_ms < at + gap {
                let staged = live.stage(at + gap, |_| Ok(()));
                assert_eq!(staged, Ok(true), "a slow frame's batch was not staged");
                committed = live.commit(wall()).unwrap().index;
                batches += 1;
                assert!(batches <= gap, "the slow frame never caught up");
            }
            shift += gap;
            let same = same_world(live.authority(), &plain);
            slow_seen.borrow_mut().push((gap, batches, same));
            return live.authority().results().is_some();
        }
        k += 1;
        let vl = base_local + k * STEP + shift;
        let ready = ready_all.take();
        if link_drop.take() {
            live.disconnect(ids[0]);
            away.set(true);
            catching_up.set(true);
        }
        if let Some(link) = relink.borrow_mut().take() {
            survivors[0].reconnect(link).unwrap();
            away.set(false);
        }
        let mut sent = [false; 2];
        for r in 0..2 {
            if r == 0 && away.get() {
                continue;
            }
            let input = autopilot(live.authority(), seats[r + 1]);
            let f = if ready {
                survivors[r].frame_with(vl, wall(), input, |c| c.set_rematch_ready(true))
            } else {
                survivors[r].frame(vl, wall(), input)
            };
            assert!(f.is_ok(), "survivor {r} refused a successor frame: {f:?}");
            let f = f.unwrap();
            ends.borrow_mut()[r].observe(survivors[r].client());
            if r == 0 && !survivors[0].resuming() {
                relinked.set(true);
            }
            frozen_frames.set(frozen_frames.get() + usize::from(f.frozen));
            if r == 0 && catching_up.get() {
                reconnect_freezes.set(reconnect_freezes.get() + usize::from(f.frozen));
                if !survivors[0].resuming() && survivors[0].last_progress() == Some(vl) {
                    catching_up.set(false);
                }
            } else if f.frozen {
                let a = live.authority();
                let state = match (a.results().is_some(), a.world().map(|w| w.phase)) {
                    (true, _) => 3,
                    (false, None) => 0,
                    (false, Some(RacePhase::Countdown)) => 1,
                    (false, Some(_)) => 2,
                };
                let mut counts = false_freezes.get();
                counts[r][state] += 1;
                false_freezes.set(counts);
            }
            idle_seen.set(idle_seen.get() + f.idle_progress);
            // No client applies an output before its commit returned.
            assert!(
                f.newest_receipt.is_none_or(|n| n <= committed),
                "survivor {r} applied an output at receipt {:?} before its commit (committed {committed})",
                f.newest_receipt
            );
            dups[r] += f.duplicates;
            sent[r] = f.views_sent > 0;
            if first_input[r].is_none()
                && survivors[r].client().next_input().unwrap() > next_at_freeze[r]
            {
                first_input[r] = Some(vl - resumed_local[r]);
            }
            if first_frame[r].is_none() {
                let client = survivors[r].client();
                let host = client.host_now(vl).unwrap() as i64 - c_logical as i64;
                assert_eq!(
                    host,
                    (vl - resumed_local[r]) as i64,
                    "survivor {r} host time"
                );
                let new = client.next_input().unwrap() - next_at_freeze[r];
                fresh_round[r] = client.clock_sync().round_open();
                first_frame[r] = Some((host, new, f.resent, f.views_sent));
            }
        }
        // Each survivor's input views reach its queue before the batch.
        let t = Instant::now();
        loop {
            live.poll(wall()).unwrap();
            let arrived = (0..2).all(|r| {
                !sent[r]
                    || live
                        .queue(ids[r])
                        .is_some_and(|q| q.lossy_views().next().is_some())
            });
            if arrived || t.elapsed() > Duration::from_millis(20) {
                break;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        if own_first_host.is_none() {
            // The own seat's first successor frame: host time is C plus the
            // local time since the seat's `Resume` re-anchor, no more.
            let host = live.own_client().unwrap().host_now(vl).unwrap() as i64 - c_logical as i64;
            assert_eq!(host, (vl - own_resume_local) as i64, "own seat host time");
            own_first_host = Some(host);
        }
        if k == 25 {
            // An injected step failure (a local time behind the client's)
            // must not lose the records it read: the next frame applies
            // them, with no delivery gap.
            let failed = live.own_frame(0, Input::default()).is_err();
            let next = live.own_frame(vl, autopilot(live.authority(), seats[0]));
            assert!(
                next.is_ok(),
                "the own seat jammed after a failed step: {next:?}"
            );
            let next = next.unwrap();
            own_views += next.views;
            own_inject = Some((last_own, failed, next.applied));
        } else {
            let input = autopilot(live.authority(), seats[0]);
            let own = if ready {
                live.own_frame_with(vl, input, |c| c.set_rematch_ready(true))
            } else {
                live.own_frame(vl, input)
            };
            let own = own.unwrap();
            own_views += own.views;
            own_resent += own.resent;
        }
        ends.borrow_mut()[2].observe(live.own_client().unwrap());
        let index = live.receipt().index;
        let to_lobby = lobby_op.take();
        assert!(live
            .stage(c_logical + k * STEP + shift, |a| {
                if to_lobby {
                    a.to_lobby();
                }
                Ok(())
            })
            .unwrap());
        if probe {
            // The commit gate: nothing of this batch reaches a survivor
            // before its commit. Each received output's receipt is checked
            // against the last commit the driver saw return.
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(10) {
                live.flush(wall());
                for r in 0..2 {
                    let f = survivors[r].frame(vl, wall(), Input::default());
                    assert!(
                        f.is_ok(),
                        "survivor {r} refused a frame while staged: {f:?}"
                    );
                    ends.borrow_mut()[r].observe(survivors[r].client());
                    let newest = f.unwrap().newest_receipt;
                    assert!(
                        newest.is_none_or(|n| n <= committed && n <= index),
                        "survivor {r} applied an output at receipt {newest:?} before its commit (committed {committed})"
                    );
                }
                std::thread::sleep(Duration::from_micros(500));
            }
            gate_probes += 1;
        }
        let commit = live.commit(wall()).unwrap();
        committed = commit.index;
        last_own = commit.own;
        if k == 12 {
            let a = live.authority();
            let per_seat = seats.map(|s| a.starved_since(s, tick_c).unwrap());
            starved_after = Some((per_seat, a.stats().starved_ticks - starved_c));
        }
        max_outbox = max_outbox.max(live.primary().outbox().count());
        let a = live.authority();
        if check_results.take() {
            // Native results: the standings follow the world's places, with
            // the racers of players who left before finishing moved last, and
            // both survivors and the own seat show the same results.
            let results = a.results().unwrap().clone();
            let world = a.world().unwrap();
            let mut by_place: Vec<usize> = (0..world.racers.len()).collect();
            by_place.sort_by_key(|&i| world.racers[i].place);
            let (mut by_place, gone): (Vec<usize>, Vec<usize>) =
                by_place.into_iter().partition(|&i| !a.left_racers().contains(&(i as u8)));
            by_place.extend(gone);
            let native = results.standings() == by_place;
            let seen = (0..2).all(|r| survivors[r].client().results() == Some(&results))
                && live.own_client().unwrap().results() == Some(&results);
            // Only the departed human is a leaver: the move lists exactly
            // its racer as Left (so a lost list, or a survivor or the own
            // seat in it, both fail).
            let stayed: Vec<u8> = (0..2)
                .filter_map(|r| survivors[r].client().view().map(|v| v.you as u8))
                .chain(live.own_client().unwrap().view().map(|v| v.you as u8))
                .collect();
            assert_eq!(stayed.len(), 3, "every survivor and the own seat raced");
            assert_eq!(a.left_racers(), &[departed_racer], "stayed {stayed:?}");
            assert!(!stayed.contains(&departed_racer), "the departed racer is not a survivor's or the own seat's");
            let finished = world
                .racers
                .iter()
                .filter(|r| r.finished_at.is_some())
                .count();
            *results_facts.borrow_mut() = Some((results.rows.len(), finished, native, seen));
        }
        if mode == Takeover::GhostInResults {
            let image = SessionImage::decode(&root, live.image()).unwrap();
            ghost.set((
                a.human_count(),
                live.receipt().logical_ms,
                image.members.iter().any(|m| m.player == ids[0]),
                live.primary()
                    .outbox()
                    .filter(|o| o.output.recipient == ids[0])
                    .count(),
            ));
        }
        facts.set((
            a.start_host_ms(),
            a.world().is_some_and(|w| w.phase == RacePhase::Racing),
            seats.iter().all(|s| a.racer_of(*s).is_some()),
            a.world().is_none(),
        ));
        a.results().is_some()
    };
    let to_results = matches!(mode, Takeover::Healthy | Takeover::GhostInResults);
    let play_batches = if to_results { 9000 } else { 30 };
    let mut done_at: Option<u64> = None;
    let mut batches = 0;
    while batches < play_batches {
        batches += 1;
        // After the starvation sample at batch 12: the slow frames starve
        // every seat by design (no device frame runs through them).
        if mode == Takeover::SlowFrames && (batches == 14 || batches == 22) {
            slow_gap.set(Some(if batches == 14 { 70 } else { 150 }));
            tick(false);
        }
        let results = tick(batches <= 20);
        if results && done_at.is_none() {
            done_at = Some(batches);
        }
        if done_at.is_some_and(|d| batches >= d + 15) {
            break;
        }
    }
    if mode == Takeover::SlowFrames {
        let seen = slow_seen.borrow().clone();
        // (Every commit above returned: no pause.)
        println!("slow_frame term=1 gaps_batches_plain_equal={seen:?}");
        assert_eq!(
            seen,
            vec![(70, 2, true), (150, 3, true)],
            "a slow frame was not split, or diverged from the plain host"
        );
    }
    if to_results {
        assert!(done_at.is_some(), "the race never finished");
        // (Before the rematch, which surfaces its own.)
        let ends = *ends.borrow();
        println!("STAMPEDE SUCCESSOR RACE END mode={mode:?} survivors_own_seat={ends:?}");
        assert_eq!(
            ends,
            [RaceEnd::ONCE; 3],
            "a resumed client's view lost or repeated its finish or the race over"
        );
    }
    if mode == Takeover::GhostInResults {
        use migration::PARKED_DEPARTURE_MS;
        // The drop lands before the next batch: it counts from the game
        // time last committed.
        let (humans, dropped_at, member, _) = ghost.get();
        assert!(member, "survivor A is not a member in the results");
        // Survivor A's link drops on the results screen; it never sends a
        // `Leave` and never reconnects.
        link_drop.set(true);
        tick(false);
        let start_before = facts.get().0;
        // Everyone still here readies (survivor B and the own seat).
        ready_all.set(true);
        tick(false);
        let mut departed_at = None;
        let mut n = 0;
        while facts.get().0 == start_before || !facts.get().1 {
            tick(false);
            n += 1;
            let (count, logical, _, _) = ghost.get();
            if departed_at.is_none() && count < humans {
                departed_at = Some(logical);
            }
            if departed_at.is_none() {
                assert!(
                    logical - dropped_at <= PARKED_DEPARTURE_MS + 2 * STEP,
                    "survivor A was still seated {} ms after its link dropped",
                    logical - dropped_at
                );
            }
            assert!(
                n < (PARKED_DEPARTURE_MS + 10_000) / STEP,
                "the rematch never started: survivor A seated={} after {} ms",
                departed_at.is_none(),
                logical - dropped_at
            );
        }
        let departed_after = departed_at.unwrap() - dropped_at;
        assert!(
            departed_after > PARKED_DEPARTURE_MS,
            "survivor A departed {departed_after} ms after its link dropped"
        );
        for _ in 0..5 {
            tick(false);
        }
        let (count, _, member, outbox) = ghost.get();
        assert_eq!(count, humans - 1, "the rematch seats");
        assert!(
            !member && outbox == 0,
            "survivor A was not retired (member={member} outbox={outbox})"
        );
        assert!(facts.get().1, "the rematch is not racing");
        println!(
            "STAMPEDE SUCCESSOR PARKED DEPARTURE departed_after_ms={departed_after} rematch_after_batches={n} humans={humans}->{count}"
        );
        return;
    }
    // Fresh connections after Resume follow the closed-room rule: the
    // absent roster key on the resume role gets no route, a stranger and a
    // plain same-code join are refused, and the room plays on.
    let mut absent_link = None;
    let mut stranger_link = None;
    let mut fresh_connected = false;
    let mut stranger_player = false;
    let mut rematch_batches = None;
    let mut rematch_ok = false;
    let mut lobby_idle = (false, 0, 0);
    let player_binding = session_binding(&root, 1, LinkRole::Player, LinkRole::Successor);
    if mode == Takeover::Healthy {
        absent_link = dial(
            &cfg,
            &identities[3],
            resume_at.clone(),
            resume_binding.clone(),
            || {
                tick(false);
            },
        );
        if let Some(link) = absent_link.as_mut() {
            let t = Instant::now();
            let mut closed = false;
            while !closed {
                tick(false);
                while let Some(event) = link.poll() {
                    closed |= matches!(
                        event,
                        ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
                    );
                }
                assert!(
                    t.elapsed() < Duration::from_secs(5),
                    "absent roster link kept"
                );
            }
        }
        stranger_link = dial(
            &cfg,
            &approved(),
            resume_at.clone(),
            resume_binding.clone(),
            || {
                tick(false);
            },
        );
        let mut fresh = QuicClient::new(decented_net::join_addr(
            &cfg,
            &approved(),
            resume_at.0,
            &resume_at.1,
        ));
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(500) {
            tick(false);
            match fresh.poll() {
                Some(ClientNetEvent::Connected { .. }) => fresh_connected = true,
                Some(ClientNetEvent::Failed(_)) | Some(ClientNetEvent::Disconnected { .. }) => {
                    break
                }
                _ => {}
            }
        }
        for _ in 0..15 {
            tick(false);
        }
        // Survivor 0's link drops after Resume. It reconnects on the resume
        // host's session-bound Player role, proves its retained journals to
        // the successor's fresh challenge and gets its unACKed outputs
        // replayed. A stranger on that role is still refused.
        stranger_player = dial(
            &cfg,
            &approved(),
            resume_at.clone(),
            player_binding.clone(),
            || {
                tick(false);
            },
        )
        .is_none();
        link_drop.set(true);
        for _ in 0..5 {
            tick(false);
        }
        relinked.set(false);
        let link = dial(
            &cfg,
            &identities[1],
            resume_at.clone(),
            player_binding.clone(),
            || {
                tick(false);
            },
        );
        relink.replace(Some(link.expect("survivor 0 reached the resume host")));
        let t = Instant::now();
        while !relinked.get() {
            tick(false);
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "survivor 0 never reconnected"
            );
        }
        for _ in 0..15 {
            tick(false);
        }
        // Results under the successor, then one rematch: the spec keeps the
        // existing ready/rematch permissions and the session armed across
        // rematches. Every human readies (own seat included); the native
        // 3-2-1 starts a new race with the same seats, and it plays on.
        check_results.set(true);
        tick(false);
        let start_before = facts.get().0;
        ready_all.set(true);
        tick(false);
        let mut n = 0;
        while facts.get().0 == start_before || !facts.get().1 {
            tick(false);
            n += 1;
            assert!(n < 400, "the rematch never started");
        }
        rematch_batches = Some(n);
        for _ in 0..60 {
            tick(false);
        }
        rematch_ok = facts.get().1 && facts.get().2;
        // An idle lobby under the successor (host-local `to_lobby`): no
        // game output flows, yet no survivor freezes, because each idle
        // commit reaches it as the successor's certificate.
        lobby_op.set(true);
        tick(false);
        let (frozen_before, idle_before) = (frozen_frames.get(), idle_seen.get());
        for _ in 0..20 {
            tick(false);
        }
        lobby_idle = (
            facts.get().3,
            frozen_frames.get() - frozen_before,
            idle_seen.get() - idle_before,
        );
    }
    drop(tick);
    // Drain: the last commit's outputs reach the survivors (same local time,
    // so no frame makes new input).
    let vl_end = base_local + k * STEP;
    let t = Instant::now();
    // An intent journaled on the last frame (a clock ping, say) still needs
    // a commit: idle batches run until every survivor's tail is handled.
    let undrained = |live: &migration::LiveSuccessor, survivors: &[migration::RemotePlayer]| {
        let image = SessionImage::decode(&root, live.image()).unwrap();
        (0..2)
            .any(|r| image.inputs[&ids[r]].handled() < survivors[r].endpoint().journals().1.tail())
    };
    while (0..2).any(|r| {
        survivors[r].endpoint().journals().0.applied()
            < live.primary().output_ceiling(ids[r], Delivery::Reliable)
    }) || undrained(&live, &survivors[..])
    {
        for r in 0..2 {
            dups[r] += survivors[r]
                .frame(vl_end, wall(), Input::default())
                .unwrap()
                .duplicates;
        }
        live.poll(wall()).unwrap();
        if undrained(&live, &survivors[..]) {
            k += 1;
            assert!(live.stage(c_logical + k * STEP, |_| Ok(())).unwrap());
            live.commit(wall()).unwrap();
        }
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "outputs never reached the survivors"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let play_wall_ms = play_wall.elapsed().as_millis();
    let image = SessionImage::decode(&root, live.image()).unwrap();
    let ingress = live.ingress();
    // N3/N4 and F1/F2: no human seat starves after Resume. Measured on the
    // successor over the first 12 batches (24 ticks, inside the 32-tick
    // memory). Each survivor's and the own seat's resent window (its
    // unconfirmed inputs at C, which include the numbers just past C's tick
    // that its lead had produced) reaches the world before those ticks run.
    // The own seat played in term 0, so its window is not empty.
    let (starved_seats, starved_total) = starved_after.unwrap();
    assert!(
        own_resent > 0 && starved_seats == [0, 0, 0],
        "a seat starved after Resume (own, survivor 0, survivor 1): {starved_seats:?} total {starved_total} own_resent={own_resent}"
    );
    assert_eq!(starved_total, 0, "starved ticks after Resume");
    // Part A's client-side bound, restored: each survivor's first new
    // input comes within its own correction plus two frames.
    let first_input = first_input.map(Option::unwrap);
    for r in 0..2 {
        assert!(
            first_input[r] as i64 <= deltas[r].max(0) + 2 * STEP as i64,
            "survivor {r} starved {first_input:?} {deltas:?}"
        );
    }
    let (inject_queued, inject_failed, inject_applied) = own_inject.unwrap();
    assert!(
        inject_failed && inject_queued > 0 && inject_applied >= inject_queued,
        "own-seat inbox after a failed step: {own_inject:?}"
    );
    let first_frame = first_frame.map(Option::unwrap);
    for r in 0..2 {
        let (_, new, resent, views) = first_frame[r];
        // N3/N4: input resumes on the very first frame (the unconfirmed
        // window is resent), with no burst of future inputs.
        assert!(
            resent > 0 && views > 0,
            "survivor {r} went quiet: {first_frame:?}"
        );
        assert!(new <= 4, "survivor {r} burst: {first_frame:?}");
    }
    assert!(fresh_round.iter().all(|o| *o), "no fresh clock-sync round");
    assert!(gate_probes >= 20);
    assert!(
        live.failure().is_none(),
        "successor failed: {:?}",
        live.failure()
    );
    // Exactly once: every committed reliable output applied once, every
    // intent drained, and the outbox pruned by committed ACKs.
    let mut applied_once = true;
    let mut drained = true;
    let mut outbox_left = Vec::new();
    for r in 0..2 {
        let p = ids[r];
        let (delivery, intents) = survivors[r].endpoint().journals();
        applied_once &= delivery.applied() == live.primary().output_ceiling(p, Delivery::Reliable);
        drained &= image.inputs[&p].handled() == intents.tail();
        outbox_left.push(
            live.primary()
                .outbox()
                .filter(|o| o.output.recipient == p)
                .count(),
        );
    }
    let (own_delivery, own_intents) = live.own_journals().unwrap();
    let own_outbox = live
        .primary()
        .outbox()
        .filter(|o| o.output.recipient == own_player)
        .count();
    let own_ok = image.inputs[&own_player].handled() == own_intents.tail()
        && own_delivery.applied() + own_outbox as u64
            >= live
                .primary()
                .output_ceiling(own_player, Delivery::Reliable);
    let duplicates_play = dups;
    assert!(
        applied_once && drained && own_ok,
        "applied={applied_once} drained={drained} own={own_ok}"
    );
    assert_eq!(duplicates_play, [0, 0], "an output applied twice");
    assert!(
        outbox_left.iter().all(|n| *n <= 8),
        "outbox not pruned: {outbox_left:?}"
    );
    // Same code, session and match; stable seats.
    let seats_stable = [ids[0], ids[1], own_player].iter().all(|p| {
        image
            .members
            .iter()
            .find(|m| m.player == *p)
            .map(|m| m.seat)
            == c_session
                .members
                .iter()
                .find(|m| m.player == *p)
                .map(|m| m.seat)
    });
    assert!(seats_stable, "a seat changed under the successor");
    // The race played to results in C's match; the one rematch is the
    // session's next match (match IDs increase within a session).
    let rematches = u64::from(mode == Takeover::Healthy);
    assert_eq!(
        image.match_id,
        c_session.match_id + rematches,
        "the match changed"
    );
    assert_eq!(live.receipt().term, 1);
    assert_eq!(live.authority().human_count(), humans_at_resume);
    let mut results_line = String::from("not_run");
    if mode == Takeover::Healthy {
        let (rows, finished, native, seen) = results_facts.borrow().unwrap();
        assert!(native, "results disagree with the world");
        assert!(seen, "a survivor or the own seat saw other results");
        assert!(rematch_ok, "the rematch lost a seat or never raced");
        let (in_lobby, lobby_frozen, lobby_certs) = lobby_idle;
        assert!(
            in_lobby && lobby_frozen == 0 && lobby_certs > 0,
            "idle lobby: {lobby_idle:?}"
        );
        // Healthy play under the successor (race, results, the rematch's
        // countdown and race, the idle lobby) never freezes a survivor.
        let false_freezes = false_freezes.get();
        assert_eq!(
            false_freezes, [[0; 4]; 2],
            "false freezes in healthy successor play (lobby, countdown, race, results)"
        );
        results_line = format!(
            "rows={rows} finished={finished} native_order={native} survivors_and_own_see_results={seen} rematch_started_after={rematch_batches:?} rematch_racing_seats_kept={rematch_ok} lobby={in_lobby} lobby_frozen_frames={lobby_frozen} lobby_idle_certificates={lobby_certs} false_freezes_lobby_countdown_race_results={false_freezes:?} reconnect_freezes={}",
            reconnect_freezes.get()
        );
        assert!(!fresh_connected, "a plain same-code join connected");
        assert!(
            stranger_link.is_none(),
            "a stranger connected on the resume role"
        );
        assert!(ingress.refused_joins >= 1 || absent_link.is_none());
        assert!(stranger_player, "a stranger connected on the Player role");
        assert_eq!(ingress.reconnected, 1, "survivor 0 did not reconnect");
    }
    println!(
        "STAMPEDE SUCCESSOR PLAY mode={mode:?} batches={k} results_at={done_at:?} {results_line} applied_once={applied_once} intents_drained={drained} own_seat_once={own_ok} duplicates={duplicates_play:?} outbox_left={outbox_left:?} own_outbox={own_outbox} max_outbox={max_outbox} own_views={own_views} same_code={same_code} same_session=true match_ids_c_end=({},{}) seats_stable={seats_stable} protected={} commit_gate_probes={gate_probes} play_wall_ms={play_wall_ms}",
        c_session.match_id,
        image.match_id,
        live.protected()
    );
    println!(
        "STAMPEDE SUCCESSOR FIRST FRAME mode={mode:?} host_minus_c_new_inputs_resent_views={first_frame:?} own_host_minus_c={own_first_host:?} fresh_round={fresh_round:?} own_resent={own_resent} first_input_ms_after_resume={first_input:?} starved_after_resume_own_s0_s1={starved_seats:?} starved_ticks_total={starved_total} own_failed_step_requeued={own_inject:?}"
    );
    if mode == Takeover::Healthy {
        println!(
            "STAMPEDE SUCCESSOR FRESH JOIN absent_roster_connected={} absent_roster_closed=true stranger_resume={} stranger_player_refused={stranger_player} fresh_code_join={} survivor_reconnected={} refused_joins={} accepted_controls={} room_unharmed={}",
            absent_link.is_some(),
            stranger_link.is_none(),
            !fresh_connected,
            ingress.reconnected,
            ingress.refused_joins,
            ingress.accepted_controls,
            live.failure().is_none(),
        );
    }
    if !matches!(mode, Takeover::SecondFailure | Takeover::Healthy) {
        return;
    }
    // The successor stops dead: no poll, no commit, nothing closed. The
    // room is unprotected: nobody takes over, clients freeze at the
    // constant, and the session ends after the recovery timeout. In the
    // healthy run it hangs in the idle lobby (no game output flows there),
    // in the second-failure run mid-race.
    let state = if mode == Takeover::Healthy {
        "lobby"
    } else {
        "race"
    };
    let hang_local = base_local + k * STEP;
    let w_hang = wall();
    let vlocal = |w: u64| hang_local + (w - w_hang);
    // (frozen at, last fresh progress, the previous frame), local times.
    let mut frozen: [Option<(u64, u64, u64)>; 2] = [None; 2];
    let mut ended: [Option<(u64, Error)>; 2] = [None, None];
    let mut last = [hang_local; 2];
    let mut promote_seen = false;
    // A fresh admitted link to the successor (its resume host): the
    // resumed client still refuses to make it a standby link.
    // Survivor 1's key: survivor 0 already holds a resume-host link after
    // its reconnect in the healthy run.
    let second = dial(
        &cfg,
        &identities[2],
        resume_at.clone(),
        resume_binding.clone(),
        || {},
    );
    let second = second.expect("a link to the successor");
    let second_refused = survivors[0].prepare_standby(second).err()
        == Some(Error::Invalid(migration::SECOND_TAKEOVER_REFUSED));
    assert!(
        second_refused,
        "a resumed survivor prepared a second standby link"
    );
    let t = Instant::now();
    while ended.iter().any(Option::is_none) {
        let w = wall();
        let l = vlocal(w);
        for r in 0..2 {
            if ended[r].is_some() || l < last[r] + STEP {
                continue;
            }
            let prev = last[r];
            last[r] = l;
            promote_seen |= survivors[r].watch_standby(w) != Err(Error::Closed);
            match survivors[r].frame(l, w, Input::default()) {
                Ok(f) => {
                    if f.frozen && frozen[r].is_none() {
                        frozen[r] = Some((l, survivors[r].last_progress().unwrap(), prev));
                    }
                }
                Err(error) => ended[r] = Some((l, error)),
            }
        }
        assert!(
            t.elapsed() < Duration::from_secs(9),
            "a survivor never ended"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let frozen = frozen.map(Option::unwrap);
    let ended = ended.map(Option::unwrap);
    let freeze_ms = frozen.map(|(at, progress, _)| at - progress);
    let end_ms = [0, 1].map(|r| ended[r].0 - frozen[r].0);
    for r in 0..2 {
        // The rule, exactly: the first frame at or past the constant freezes
        // (the previous one was under it). How far past depends on when the
        // wall-paced device got that frame, which load delays.
        let (at, progress, prev) = frozen[r];
        assert!(
            at >= progress + migration::FREEZE_AFTER_MS
                && prev < progress + migration::FREEZE_AFTER_MS,
            "survivor {r} froze {} ms after progress (previous frame {} ms after)",
            freeze_ms[r],
            prev.saturating_sub(progress)
        );
        assert!(end_ms[r] >= migration::SESSION_LOST_AFTER_MS);
        assert_eq!(
            ended[r].1,
            Error::Invalid("session lost: the successor stopped")
        );
    }
    assert!(!promote_seen, "a second takeover began");
    println!(
        "STAMPEDE SECOND FAILURE state={state} freeze_after_progress_ms={freeze_ms:?} constant={} ended_after_freeze_ms={end_ms:?} reason={:?} second_standby_refused={second_refused} promote_seen={promote_seen} protected={}",
        migration::FREEZE_AFTER_MS,
        ended[0].1,
        live.protected()
    );
}

/// A takeover inside a seatless leaver's one-batch window, with a modified
/// leaver client. In the armed lobby the leaver's Ping and Leave commit in
/// one batch N (the Pong to it is kept, so it stays a seatless member); it
/// then journals a `Hello` and the original stops dead before N+1. The
/// leaver bypasses the client library's `Closed("left")` guard: it runs
/// Fray's own endpoint by hand and enters recovery on its prepared standby
/// link directly. The promoted standby never challenges it (its route is
/// closed), so it is absent and the departure commit retires it with no
/// native event; recovery reaches `Ready` and both survivors resume.
#[test]
fn a_modified_leaver_resuming_in_its_window_is_refused_and_the_takeover_completes() {
    use race_session::proto::{encode, ToHost};
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let root = room.root().clone();
    let context = root.context().unwrap();
    let standby_at = loopback(room.standby.listener().unwrap(), root.standby);
    let warm_binding = session_binding(&root, 0, LinkRole::ControlPlayer, LinkRole::Standby);
    let mut warm = Vec::new();
    for i in 1..4 {
        let (id, cfg) = (room.identities[i].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        });
        warm.push(link.expect("prepared control link"));
    }
    let leaver_warm = warm.pop().unwrap();
    let ids = [room.player(1), room.player(2)];
    let leaver = room.player(3);
    let mut waiting = [Some(room.remote(1)), Some(room.remote(2))];
    for (r, link) in warm.into_iter().enumerate() {
        waiting[r].as_mut().unwrap().prepare_standby(link).unwrap();
    }
    // The modified leaver: Fray's endpoint on its own player link, by hand.
    let mut leaver_net = room.networks[3].take().unwrap();
    let mut endpoint = Some(
        PlayerEndpoint::new(
            room.descriptor.clone(),
            room.standby.armed(),
            &room.capability(3),
            room.cert.clone(),
            leaver_net
                .migration_channel(&player_policy(&room.descriptor, &room.cert))
                .unwrap(),
            room.identities[3].keys().clone(),
        )
        .unwrap(),
    );
    let mut send = |endpoint: &mut PlayerEndpoint, intents: &[Vec<u8>], wall: u64| {
        endpoint
            .intents_mut()
            .unwrap()
            .append_batch(intents)
            .unwrap();
        while endpoint.queue_pending(MAX_BATCH).unwrap() > 0 {}
        while let Some(wire) = endpoint.poll_send(wall).unwrap() {
            leaver_net.send_reliable(wire);
        }
    };
    let queued = |room: &ArmedRoom, n: u64| {
        room.players
            .queue(leaver)
            .is_some_and(|q| q.pending().any(|(m, _)| m == n))
    };
    let frame =
        |room: &mut ArmedRoom, waiting: &mut [Option<migration::RemotePlayer>; 2], index: u64| {
            let now = room.base + index * STEP;
            for (r, remote) in waiting.iter_mut().enumerate() {
                remote
                    .as_mut()
                    .unwrap()
                    .frame(now, room.wall(), remote_input(index, r))
                    .unwrap();
            }
        };
    // A second of armed lobby: the survivors' clocks sync.
    let mut index = 0;
    while index < 30 {
        index += 1;
        frame(&mut room, &mut waiting, index);
        room.commit(index);
    }
    // Batch N: the leaver's Ping and Leave, together.
    let ping = encode(&ToHost::Ping {
        id: 1,
        client_t: room.base + index * STEP,
    });
    let wall = room.wall();
    send(
        endpoint.as_mut().unwrap(),
        &[ping, encode(&ToHost::Leave)],
        wall,
    );
    let t = Instant::now();
    while !(queued(&room, 1) && queued(&room, 2)) {
        room.poll_players();
        bounded(t);
    }
    index += 1;
    frame(&mut room, &mut waiting, index);
    room.commit(index);
    let image = room.image();
    assert!(image.members.iter().any(|m| m.player == leaver));
    assert_eq!(room.handled(leaver), 2);
    assert!(!room
        .live
        .primary()
        .receipt()
        .watermarks
        .iter()
        .any(|m| m.player == leaver && m.retired));
    assert!(!room.outbox(leaver).is_empty(), "the Pong was not kept");
    // The modified client journals a Hello after its Leave; it reaches the
    // original's queue, but the original stops dead before N+1.
    let hello = encode(&ToHost::Hello {
        proto: race_session::proto::PROTO_VERSION,
        name: "back".into(),
        character: 0,
    });
    let wall = room.wall();
    send(endpoint.as_mut().unwrap(), &[hello], wall);
    let t = Instant::now();
    while !queued(&room, 3) {
        room.poll_players();
        bounded(t);
    }
    let c_receipt = room.standby.receipt().clone();
    assert_eq!(c_receipt.index, index);
    let c_logical = c_receipt.logical_ms;
    let handled_c = ids.map(|p| {
        c_receipt
            .watermarks
            .iter()
            .find(|m| m.player == p)
            .unwrap()
            .input
    });
    let own_client = room.clients[0].take().unwrap();
    let drop_local = room.base + (index + 1) * STEP;
    let w_drop = room.wall();
    let drop_at = Instant::now();
    let ArmedRoom {
        descriptor,
        identities,
        live,
        players,
        standby,
        clock,
        cert,
        ..
    } = room;
    let hung = (live, players);
    let wall = || clock.now_ms();
    let local = |w: u64| drop_local + w.saturating_sub(w_drop);
    let mut devices = SurvivorDevices {
        waiting,
        recovering: [None, None],
        last_frame: [drop_local - STEP; 2],
        freeze: [None; 2],
        frames: [0; 2],
        frozen_by: [None; 2],
        tail: [None; 2],
        pings: [Vec::new(), Vec::new()],
        resumed_local: [None; 2],
        applied: [0; 2],
        duplicates: [0; 2],
        discarded: 0,
    };
    // The modified leaver's side of its prepared link: it reads the signed
    // `Promote`, enters recovery directly (no `Closed("left")` guard) and
    // answers whatever the standby sends.
    let warm_policy = EndpointPolicy {
        role: LinkRole::Standby,
        ..player_policy(&descriptor, &cert)
    };
    let mut leaver_warm = leaver_warm;
    let mut warm_channel = Some(leaver_warm.migration_channel(&warm_policy).unwrap());
    let mut leaver_recovery: Option<PlayerRecovery> = None;
    let mut leaver_ingress = ReceiveQueue::default();
    let mut leaver_closed = false;
    let mut leaver_step = |w: u64| {
        while let Some(event) = leaver_warm.poll() {
            match event {
                ClientNetEvent::Reliable(bytes) => {
                    let _ = leaver_ingress.enqueue(bytes);
                }
                ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. } => {
                    leaver_closed = true
                }
                _ => {}
            }
        }
        loop {
            let received = match leaver_recovery.as_mut() {
                None => {
                    let channel = warm_channel.as_mut().unwrap();
                    match leaver_ingress.poll(|wire| channel.receive(wire, w)) {
                        Ok(Some(Some((PacketKind::Control, _, bytes)))) => {
                            let promotion = Promotion::decode(&context, &bytes).unwrap();
                            leaver_recovery = Some(
                                endpoint
                                    .take()
                                    .unwrap()
                                    .into_recovery(warm_channel.take().unwrap(), promotion, w)
                                    .unwrap(),
                            );
                            true
                        }
                        Ok(Some(_)) => true,
                        _ => false,
                    }
                }
                Some(recovery) => matches!(
                    leaver_ingress.poll(|wire| recovery.receive(wire, w, |_, _, _| Ok(()))),
                    Ok(Some(_))
                ),
            };
            if !received {
                break;
            }
        }
        if let Some(recovery) = leaver_recovery.as_mut() {
            while let Ok(Some(wire)) = recovery.poll_send(w) {
                leaver_warm.send_reliable(wire);
            }
        }
        leaver_closed
    };
    // The standby's real watch; the survivors frame on.
    let mut standby = Some(standby);
    let (promoted, at) = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        leaver_step(w);
        match standby.take().unwrap().promote() {
            Ok(promoted) => {
                let at = promoted.promoted_at_ms();
                break (promoted, at);
            }
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            drop_at.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let keys = identities[0].keys().clone();
    let (mut links, mut recovery) = migration::SurvivorLinks::open(promoted.into_link()).unwrap();
    let mut own = migration::OwnSeat::new(
        &root,
        recovery.sequencer().image(),
        own_client,
        DeliveryJournal::default(),
        IntentJournal::default(),
    )
    .unwrap();
    // The standby's own seat is seated at C: a seatless drive is refused
    // before anything moves, and the seat is then driven as usual.
    let seatless = links.drive(&mut recovery, None, &keys, local(wall()), wall());
    assert!(
        matches!(seatless, Err(Error::Invalid(why)) if why.contains("own seat")),
        "{seatless:?}"
    );
    assert_eq!(recovery.phase(), RecoveryPhase::Collecting);
    let refused_before = links.refused;
    let mut leaver_accepted = false;
    let failure = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        leaver_step(w);
        let phase = links.drive(&mut recovery, Some(&mut own), &keys, local(w), w);
        leaver_accepted |= links.accepted(leaver) || links.challenge_of(leaver).is_some();
        match phase {
            Ok(RecoveryPhase::Ready) => break None,
            Ok(_) => {}
            Err(error) => break Some(error),
        }
        assert!(
            wall() - at < 5000,
            "recovery stalled in {:?}",
            recovery.phase()
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(failure, None, "recovery failed");
    assert!(!leaver_accepted, "the seatless leaver was challenged");
    assert!(
        links.link_of(leaver).is_none(),
        "the leaver's link stayed routed"
    );
    assert!(
        links.refused > refused_before,
        "the leaver's link was not refused"
    );
    let image = SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
    assert!(!image.members.iter().any(|m| m.player == leaver));
    assert!(recovery
        .sequencer()
        .receipt()
        .watermarks
        .iter()
        .any(|m| m.player == leaver && m.retired));
    assert!(recovery
        .sequencer()
        .outbox()
        .all(|r| r.output.recipient != leaver));
    drop(hung);
    // The survivors' final cuts and `Resume`.
    let mut barrier = links.barrier(recovery, &keys, wall()).unwrap();
    own.ready(&mut barrier, c_logical, local(wall()), wall())
        .unwrap();
    let t = Instant::now();
    loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        if links.drive_barrier(&mut barrier, wall()).unwrap() {
            break;
        }
        bounded(t);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (successor, _) = links.resume(barrier, &keys, wall()).unwrap();
    own.resumed(local(wall())).unwrap();
    let t = Instant::now();
    while devices.resumed_local.iter().any(Option::is_none) {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        bounded(t);
    }
    let image = SessionImage::decode(&root, successor.image()).unwrap();
    assert_eq!(successor.receipt().term, 1);
    assert!(ids
        .iter()
        .all(|p| image.members.iter().any(|m| m.player == *p)));
    assert!(!image.members.iter().any(|m| m.player == leaver));
    println!(
        "STAMPEDE SEATLESS LEAVER TAKEOVER leaver={leaver} challenged=false refused={} ready=true survivors_resumed=2 leaver_link_closed={}",
        links.refused - refused_before,
        leaver_step(wall())
    );
}

/// Twang's rule for a standby seat kept seatless at C. The standby device's
/// own player, on its real own-seat endpoint, sends a Ping and its `Leave`
/// (through its client, which is then `Closed("left")`) in one armed-lobby
/// batch N; the Pong to it keeps it a seatless member with an unretired mark.
/// The original stops dead before N+1. The standby promotes on its real
/// watch. A seatless drive is refused (the seat is a member at C); with its
/// `OwnSeat` (the closed client and the endpoint's journals) recovery reaches
/// `Ready`, Fray's seal included, and the seat is not retired at C. Its cut
/// joins the barrier, both prepared survivors resume, and the successor's
/// first live batch feeds nothing from the seat, retires it and drops it; the
/// successor hosts on seatless while the survivors frame on.
#[test]
fn a_standby_whose_player_left_in_its_window_promotes_with_its_seat_and_the_successor_retires_it() {
    use race_session::proto::{encode, ToHost};
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let root = room.root().clone();
    let standby_at = loopback(room.standby.listener().unwrap(), root.standby);
    let warm_binding = session_binding(&root, 0, LinkRole::ControlPlayer, LinkRole::Standby);
    let mut warm = Vec::new();
    for i in 1..3 {
        let (id, cfg) = (room.identities[i].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        });
        warm.push(link.expect("prepared control link"));
    }
    let ids = [room.player(1), room.player(2)];
    let own_player = room.player(0);
    let mut waiting = [Some(room.remote(1)), Some(room.remote(2))];
    for (r, link) in warm.into_iter().enumerate() {
        waiting[r].as_mut().unwrap().prepare_standby(link).unwrap();
    }
    let mut own_client = room.clients[0].take().unwrap();
    let (mut own_t0, _) = {
        let (descriptor, keys, initial) = (
            room.descriptor.clone(),
            room.identities[0].keys().clone(),
            room.initial,
        );
        own_link(
            &descriptor,
            &mut room.standby,
            &mut room.live,
            &keys,
            initial,
        )
    };
    assert_eq!(own_t0.queue.player(), own_player);
    let frame =
        |room: &mut ArmedRoom, waiting: &mut [Option<migration::RemotePlayer>; 2], index: u64| {
            let now = room.base + index * STEP;
            for (r, remote) in waiting.iter_mut().enumerate() {
                remote
                    .as_mut()
                    .unwrap()
                    .frame(now, room.wall(), remote_input(index, r))
                    .unwrap();
            }
            now
        };
    // A second of armed lobby: the survivors' clocks sync.
    let mut index = 0;
    while index < 30 {
        index += 1;
        frame(&mut room, &mut waiting, index);
        room.commit(index);
    }
    // Batch N: the own player's Ping, then its `Leave` through its client.
    let now = room.base + (index + 1) * STEP;
    let ping = encode(&ToHost::Ping {
        id: 1,
        client_t: now,
    });
    own_t0
        .endpoint
        .intents_mut()
        .unwrap()
        .append_batch(&[ping])
        .unwrap();
    let left = own_client
        .migration_step(
            own_t0.endpoint.intents_mut().unwrap(),
            now,
            Input::default(),
            &[],
            None,
            |c| {
                c.leave();
                Ok(())
            },
        )
        .unwrap();
    assert!(left.closed);
    assert_eq!(
        own_client.phase(),
        &race_session::ClientPhase::Closed("left".into())
    );
    let wall = room.wall();
    while own_t0.endpoint.queue_pending(MAX_BATCH).unwrap() > 0 {}
    while let Some(wire) = own_t0.endpoint.poll_send(wall).unwrap() {
        let Some((kind, _, bytes)) = own_t0.server.receive(&wire, wall).unwrap() else {
            continue;
        };
        assert_eq!(kind, PacketKind::Control);
        assert_eq!(own_t0.queue.receive(&bytes, room.live.primary()), Ok(true));
    }
    assert_eq!(own_t0.queue.pending().count(), 2);
    index += 1;
    frame(&mut room, &mut waiting, index);
    room.commit_with(index, Some(&mut own_t0.queue));
    let mark = |receipt: &Receipt| {
        receipt
            .watermarks
            .iter()
            .find(|m| m.player == own_player)
            .map(|m| m.retired)
    };
    assert_eq!(room.handled(own_player), 2);
    assert!(
        room.image().members.iter().any(|m| m.player == own_player)
            && mark(room.live.primary().receipt()) == Some(false),
        "the Leave batch did not keep the standby's player a member"
    );
    assert!(!room.outbox(own_player).is_empty(), "the Pong was not kept");
    // The original stops dead before N+1: C is the Leave batch.
    let c_receipt = room.standby.receipt().clone();
    assert_eq!(c_receipt.index, index);
    let c_logical = c_receipt.logical_ms;
    let handled_c = ids.map(|p| {
        c_receipt
            .watermarks
            .iter()
            .find(|m| m.player == p)
            .unwrap()
            .input
    });
    let drop_local = room.base + (index + 1) * STEP;
    let w_drop = room.wall();
    let drop_at = Instant::now();
    let ArmedRoom {
        identities,
        live,
        players,
        standby,
        clock,
        ..
    } = room;
    let hung = (live, players);
    let wall = || clock.now_ms();
    let local = |w: u64| drop_local + w.saturating_sub(w_drop);
    let mut devices = SurvivorDevices {
        waiting,
        recovering: [None, None],
        last_frame: [drop_local - STEP; 2],
        freeze: [None; 2],
        frames: [0; 2],
        frozen_by: [None; 2],
        tail: [None; 2],
        pings: [Vec::new(), Vec::new()],
        resumed_local: [None; 2],
        applied: [0; 2],
        duplicates: [0; 2],
        discarded: 0,
    };
    let mut standby = Some(standby);
    let (promoted, at) = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        match standby.take().unwrap().promote() {
            Ok(promoted) => {
                let at = promoted.promoted_at_ms();
                break (promoted, at);
            }
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            drop_at.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let keys = identities[0].keys().clone();
    let (mut links, mut recovery) = migration::SurvivorLinks::open(promoted.into_link()).unwrap();
    let c_session = SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
    assert!(c_session.members.iter().any(|m| m.player == own_player));
    // The seat recovers from its closed client and its endpoint's journals.
    let (own_delivery, own_intents) = {
        let (delivery, intents) = own_t0.endpoint.journals();
        (
            DeliveryJournal::restore(&delivery.checkpoint().unwrap()).unwrap(),
            IntentJournal::restore(&intents.checkpoint().unwrap()).unwrap(),
        )
    };
    drop(own_t0);
    assert_eq!(own_intents.tail(), 2);
    let mut own = migration::OwnSeat::new(
        &root,
        recovery.sequencer().image(),
        own_client,
        own_delivery,
        own_intents,
    )
    .expect("a seatless member at C keeps its OwnSeat");
    // A member at C, though seatless: a seatless drive is refused before
    // anything moves.
    let seatless = links.drive(&mut recovery, None, &keys, local(wall()), wall());
    assert!(
        matches!(&seatless, Err(Error::Invalid(why)) if why.contains("own seat")),
        "{seatless:?}"
    );
    assert_eq!(recovery.phase(), RecoveryPhase::Collecting);
    let failure = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        match links.drive(&mut recovery, Some(&mut own), &keys, local(w), w) {
            Ok(RecoveryPhase::Ready) => break None,
            Ok(_) => {}
            Err(error) => break Some(error),
        }
        assert!(
            wall() - at < 5000,
            "recovery stalled in {:?}",
            recovery.phase()
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(failure, None, "recovery with the seatless own seat failed");
    assert!(!own.retired(&recovery), "recovery retired the own seat");
    let image = SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
    assert!(image.members.iter().any(|m| m.player == own_player));
    assert_eq!(image.inputs[&own_player].handled(), 2);
    assert_eq!(
        own.client().phase(),
        &race_session::ClientPhase::Closed("left".into())
    );
    drop(hung);
    // The final cuts, the own seat's included, and `Resume`.
    let mut barrier = links.barrier(recovery, &keys, wall()).unwrap();
    own.ready(&mut barrier, c_logical, local(wall()), wall())
        .unwrap();
    let t = Instant::now();
    loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        if links.drive_barrier(&mut barrier, wall()).unwrap() {
            break;
        }
        bounded(t);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (successor, _) = links.resume(barrier, &keys, wall()).unwrap();
    own.resumed(local(wall())).unwrap();
    let t = Instant::now();
    while devices.resumed_local.iter().any(Option::is_none) {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        bounded(t);
    }
    let mut survivors: Vec<migration::RemotePlayer> = devices
        .recovering
        .into_iter()
        .map(|r| r.unwrap().into_resumed().unwrap())
        .collect();
    let mut live = links
        .into_successor(successor, Some(own), None, keys)
        .unwrap();
    assert!(
        live.own_client().is_some(),
        "the successor dropped the seat early"
    );
    let mark_at = |live: &migration::LiveSuccessor| mark(live.receipt());
    assert_eq!(mark_at(&live), Some(false));
    // The successor's first live batch retires the seatless seat and drops it.
    let resumed_local = local(wall());
    let mut k = 0u64;
    let successor_step =
        |live: &mut migration::LiveSuccessor, survivors: &mut [migration::RemotePlayer], k: u64| {
            let vl = resumed_local + k * STEP;
            for (r, survivor) in survivors.iter_mut().enumerate() {
                let f = survivor.frame(vl, wall(), remote_input(20_000 + k, r));
                assert!(f.is_ok(), "survivor {r} refused a successor frame: {f:?}");
            }
            live.poll(wall()).unwrap();
            live.own_frame(vl, Input::default()).unwrap();
            assert!(live.stage(c_logical + k * STEP, |_| Ok(())).unwrap());
            live.commit(wall()).unwrap()
        };
    k += 1;
    successor_step(&mut live, &mut survivors, k);
    let image = SessionImage::decode(&root, live.image()).unwrap();
    assert!(
        !image.members.iter().any(|m| m.player == own_player),
        "the first live batch did not retire the seatless own seat"
    );
    assert_eq!(mark_at(&live), Some(true));
    assert!(
        live.own_client().is_none(),
        "the successor kept the retired seat"
    );
    assert!(live
        .primary()
        .outbox()
        .all(|r| r.output.recipient != own_player));
    // The survivors resume and play on under the seatless successor.
    while k < 15 {
        k += 1;
        successor_step(&mut live, &mut survivors, k);
    }
    let image = SessionImage::decode(&root, live.image()).unwrap();
    assert!(ids
        .iter()
        .all(|p| image.members.iter().any(|m| m.player == *p)));
    assert!(survivors.iter().all(|s| s.under_successor()));
    assert!(live.failure().is_none());
    println!(
        "STAMPEDE SEATLESS OWN SEAT AT C member_at_c=true drive_none_refused=true own_retained=true retired_by_first_live_batch=true successor_batches={k} survivors_resumed=2"
    );
}

/// The accepted seatless takeover: the standby device's own seat departed
/// before C (the original's host kick, committed in term 0, and retired by
/// the next batch), so it is no member at C and `SurvivorLinks::drive(None)`
/// is accepted at every step. The standby promotes on its real watch with no
/// `OwnSeat`, recovery reaches `Ready` (an idle third human departs after
/// the drain), the barrier completes with no own cut, both prepared
/// survivors resume, and the successor hosts with no own seat and without
/// the standby's player. (C inside the departure batch's window, the seat a
/// seatless member at C, needs its `OwnSeat`:
/// `a_standby_whose_player_left_in_its_window_promotes_with_its_seat_and_the_successor_retires_it`.)
#[test]
fn a_standby_whose_seat_departed_before_c_takes_over_seatless_and_survivors_resume() {
    let mut room = ArmedRoom::new();
    room.start_at = u64::MAX;
    let root = room.root().clone();
    let standby_at = loopback(room.standby.listener().unwrap(), root.standby);
    let warm_binding = session_binding(&root, 0, LinkRole::ControlPlayer, LinkRole::Standby);
    let mut warm = Vec::new();
    for i in 1..3 {
        let (id, cfg) = (room.identities[i].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        });
        warm.push(link.expect("prepared control link"));
    }
    let ids = [room.player(1), room.player(2)];
    let own_player = room.player(0);
    assert_eq!(
        root.members
            .iter()
            .find(|m| m.player == own_player)
            .unwrap()
            .key,
        Some(root.standby),
        "player 0 is the standby device's own seat"
    );
    let own_seat = root
        .members
        .iter()
        .find(|m| m.player == own_player)
        .unwrap()
        .seat;
    let mut waiting = [Some(room.remote(1)), Some(room.remote(2))];
    for (r, link) in warm.into_iter().enumerate() {
        waiting[r].as_mut().unwrap().prepare_standby(link).unwrap();
    }
    let frame =
        |room: &mut ArmedRoom, waiting: &mut [Option<migration::RemotePlayer>; 2], index: u64| {
            let now = room.base + index * STEP;
            for (r, remote) in waiting.iter_mut().enumerate() {
                remote
                    .as_mut()
                    .unwrap()
                    .frame(now, room.wall(), remote_input(index, r))
                    .unwrap();
            }
            now
        };
    // A second of armed lobby: the survivors' clocks sync.
    let mut index = 0;
    while index < 30 {
        index += 1;
        frame(&mut room, &mut waiting, index);
        room.commit(index);
    }
    // The departure batch: the host kicks the standby device's own seat.
    index += 1;
    let now = frame(&mut room, &mut waiting, index);
    room.commit_op(index, now, None, |h| {
        if h.kick(own_seat) {
            Ok(())
        } else {
            Err(Error::Invalid("kick"))
        }
    });
    assert_eq!(room.live.failure(), None, "the kick paused the room");
    let mark = |room: &ArmedRoom| {
        room.live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .find(|m| m.player == own_player)
            .map(|m| m.retired)
    };
    assert!(
        room.image().members.iter().any(|m| m.player == own_player) && mark(&room) == Some(false),
        "the kick batch did not keep the standby's player seatless"
    );
    // The next batch retires it.
    index += 1;
    frame(&mut room, &mut waiting, index);
    room.commit(index);
    assert!(
        !room.image().members.iter().any(|m| m.player == own_player) && mark(&room) == Some(true),
        "the next batch did not retire the standby's player"
    );
    let c_receipt = room.standby.receipt().clone();
    assert_eq!(c_receipt.index, index);
    let c_session = SessionImage::decode(&root, room.standby.image()).unwrap();
    assert!(!c_session.members.iter().any(|m| m.player == own_player));
    let handled_c = ids.map(|p| {
        c_receipt
            .watermarks
            .iter()
            .find(|m| m.player == p)
            .unwrap()
            .input
    });
    let drop_local = room.base + (index + 1) * STEP;
    let w_drop = room.wall();
    let drop_at = Instant::now();
    let ArmedRoom {
        identities,
        live,
        players,
        standby,
        clock,
        ..
    } = room;
    // The original stops dead: never polled again, nothing closed.
    let hung = (live, players);
    let wall = || clock.now_ms();
    let local = |w: u64| drop_local + w.saturating_sub(w_drop);
    let mut devices = SurvivorDevices {
        waiting,
        recovering: [None, None],
        last_frame: [drop_local - STEP; 2],
        freeze: [None; 2],
        frames: [0; 2],
        frozen_by: [None; 2],
        tail: [None; 2],
        pings: [Vec::new(), Vec::new()],
        resumed_local: [None; 2],
        applied: [0; 2],
        duplicates: [0; 2],
        discarded: 0,
    };
    let mut standby = Some(standby);
    let (promoted, at) = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        match standby.take().unwrap().promote() {
            Ok(promoted) => {
                let at = promoted.promoted_at_ms();
                break (promoted, at);
            }
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            drop_at.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let keys = identities[0].keys().clone();
    let (mut links, mut recovery) = migration::SurvivorLinks::open(promoted.into_link()).unwrap();
    // Seatless at every step: accepted, never `Invalid("own seat …")`.
    let mut steps = 0u32;
    let failure = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        steps += 1;
        match links.drive(&mut recovery, None, &keys, local(w), w) {
            Ok(RecoveryPhase::Ready) => break None,
            Ok(_) => {}
            Err(error) => break Some(error),
        }
        assert!(
            wall() - at < 5000,
            "recovery stalled in {:?}",
            recovery.phase()
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(failure, None, "the seatless drive was refused");
    let image = SessionImage::decode(&root, recovery.sequencer().image()).unwrap();
    assert!(!image.members.iter().any(|m| m.player == own_player));
    assert!(recovery
        .sequencer()
        .outbox()
        .all(|r| r.output.recipient != own_player));
    drop(hung);
    // The survivors' final cuts and `Resume`, with no own cut.
    let mut barrier = links.barrier(recovery, &keys, wall()).unwrap();
    let t = Instant::now();
    loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        if links.drive_barrier(&mut barrier, wall()).unwrap() {
            break;
        }
        bounded(t);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (successor, _) = links.resume(barrier, &keys, wall()).unwrap();
    let t = Instant::now();
    while devices.resumed_local.iter().any(Option::is_none) {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_c);
        bounded(t);
    }
    let image = SessionImage::decode(&root, successor.image()).unwrap();
    assert_eq!(successor.receipt().term, 1);
    assert!(ids
        .iter()
        .all(|p| image.members.iter().any(|m| m.player == *p)));
    assert!(!image.members.iter().any(|m| m.player == own_player));
    // The successor hosts with no own seat.
    let live = links.into_successor(successor, None, None, keys).unwrap();
    assert!(live.own_client().is_none(), "the successor has an own seat");
    println!(
        "STAMPEDE SEATLESS STANDBY TAKEOVER member_at_c=false drive_none_steps={steps} ready=true survivors_resumed=2 successor_own_seat=false"
    );
}

/// Items A and B under a takeover. From the armed local-seats match (the
/// standby device's own player on its real shared-sender route, the phone
/// host's own player on `attach_own`, two remote humans with prepared
/// control links to the standby, an idle third remote human and four bots),
/// every playing seat sends a reliable intent in one last frame and the
/// original hangs: it is never polled again and nothing is closed. The
/// standby promotes on its real watch. Its own seat, framed on until its
/// client's own loss detection froze it (or the promotion came first),
/// recovers from its real endpoint's journals (`into_parts`, then
/// `OwnSeat::new`) and drains its in-flight intents exactly once. The phone
/// host's player died with its device: it departs after the drain, its
/// racer becomes a bot, its watermark retires and nothing stays in the
/// outbox for it, and it has no seat or queue under the successor. Every
/// survivor resumes and the race completes.
#[test]
fn a_takeover_from_both_own_seats_drains_the_standby_seat_and_departs_the_host_player() {
    let (mut room, host_client) = ArmedRoom::phone();
    const LOBBY: u64 = 10;
    room.start_at = LOBBY + 1;
    let root = room.root().clone();
    let code = room.players.network().code().unwrap().to_owned();
    let standby_at = loopback(room.standby.listener().unwrap(), root.standby);
    let warm_binding = session_binding(&root, 0, LinkRole::ControlPlayer, LinkRole::Standby);
    let mut warm = Vec::new();
    for i in 1..3 {
        let (id, cfg) = (room.identities[i].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        });
        warm.push(link.expect("prepared control link"));
    }
    let (mut seats, ids) = local_seats(&mut room, host_client);
    for (r, link) in warm.into_iter().enumerate() {
        seats[2 + r].prepare_standby(link).unwrap();
    }
    // The authority runtimes' own players never prepare a standby link (the
    // standby's seat is local to it; the original's leaves with its device).
    // Both are refused before the offered connection is used, so a
    // connection that never reaches a listener is enough.
    for s in [0, 1] {
        let server = decented_net::PublicKey::from_slice(&root.standby).unwrap();
        let nowhere = std::net::SocketAddr::from(([127, 0, 0, 1], 9));
        let handle = decented_net::join_session_addr(
            &room.cfg,
            &room.identities[3],
            nowhere,
            &standby_at.1,
            warm_binding.clone(),
            server,
        )
        .unwrap();
        let refused = seats[s].prepare_standby(QuicClient::new(handle));
        assert_eq!(
            refused.err(),
            Some(Error::Unauthorized),
            "own seat {s} prepared a standby link"
        );
        assert!(!seats[s].protected());
    }
    let [own_player, host_player, _, _] = ids;
    let remote_ids = [ids[2], ids[3]];
    let absent_remote = room.player(3);
    let mut ledgers: Vec<RemoteLedger> = (0..4).map(|_| RemoteLedger::default()).collect();
    let (mut index, mut now) = (0u64, room.base);
    let mut racing = 0u32;
    loop {
        index += 1;
        now += STEP;
        seats_frame(&room, &mut seats, &mut ledgers, now, index);
        let outputs = room.commit_at(index, now, None);
        seats_record(&room, &ids, &mut ledgers, &outputs);
        assert!(
            room.native_equals_standby(),
            "native state differs from the standby at {index}"
        );
        let in_race = room
            .live
            .authority()
            .world()
            .is_some_and(|w| w.phase == RacePhase::Racing);
        racing += u32::from(in_race);
        let synced = [0, 2, 3]
            .iter()
            .all(|&s| seats[s].client().clock_offset_ms().is_some());
        // Cut while the standby device's own clock round still has pings to
        // send (each due within 40 ms), so one is in flight at C and its
        // drain must give it exactly one pong.
        let clock = seats[0].client().clock_sync();
        let own_round_pending = clock.round_first_id().is_some_and(|first| {
            (ledgers[0].pings.range(first..).count() as u32) < race_session::clock::SYNC_PINGS
        });
        if racing >= 30 && synced && own_round_pending {
            break;
        }
        assert!(index < 1000, "the race never ran");
    }
    // The drop frame: each seat sends a character command (a reliable
    // intent, a character it does not hold) and any ping due; none reaches
    // a commit. The original hangs right after it.
    let drop_local = now + STEP;
    let w_drop = room.wall();
    let drop_at = Instant::now();
    for (s, seat) in seats.iter_mut().enumerate() {
        let last_set = index - (index - (5 + s as u64)) % 40;
        let character = 1 + ((last_set / 40 + s as u64 + 1) % 3) as u8;
        let step = seat
            .frame_with(
                drop_local,
                room.wall(),
                remote_input(index + 1, s % 2),
                |c| c.set_character(character),
            )
            .unwrap();
        assert!(!step.frozen, "seat {s} froze before the cut");
        assert!(
            step.intents.iter().any(|(_, b)| ping_of(b).is_none()),
            "seat {s} sent no character command in the drop frame"
        );
    }
    let c_receipt = room.standby.receipt().clone();
    assert_eq!(c_receipt.index, index);
    let c_logical = c_receipt.logical_ms;
    let c_session = SessionImage::decode(&root, room.standby.image()).unwrap();
    let c_auth = room.standby.committed_authority().unwrap();
    let own_handled_c = c_session.inputs[&own_player].handled();
    let handled_r = remote_ids.map(|p| c_session.inputs[&p].handled());
    let seat_of = |p: u64| {
        c_session
            .members
            .iter()
            .find(|m| m.player == p)
            .unwrap()
            .seat
    };
    let host_seat = seat_of(host_player);
    let host_racer = c_auth.racer_of(host_seat).expect("the host player races");
    let host_human_at_c = matches!(
        c_auth.world().unwrap().cfg.racers[host_racer].kind,
        RacerKind::Human
    );
    assert!(host_human_at_c, "the host player's racer is not human at C");
    let humans_c = c_auth.human_count();

    // The original hangs: its runtime, player host and the phone host's own
    // player are never polled again; the idle third human's device goes away.
    let mut parts = seats.into_iter();
    let mut own_seat = parts.next().unwrap();
    let host_seat_player = parts.next().unwrap();
    let (a, b) = (parts.next().unwrap(), parts.next().unwrap());
    let ArmedRoom {
        cfg,
        descriptor,
        identities,
        networks,
        live,
        players,
        standby,
        clock,
        ..
    } = room;
    let hung = (live, players, host_seat_player);
    drop(networks);
    let wall = || clock.now_ms();
    let local = |w: u64| drop_local + w.saturating_sub(w_drop);
    let mut devices = SurvivorDevices {
        waiting: [Some(a), Some(b)],
        recovering: [None, None],
        last_frame: [drop_local; 2],
        freeze: [None; 2],
        frames: [0; 2],
        frozen_by: [None; 2],
        tail: [None; 2],
        pings: [Vec::new(), Vec::new()],
        resumed_local: [None; 2],
        applied: [0; 2],
        duplicates: [0; 2],
        discarded: 0,
    };
    // The standby's real watch on monotonic time. The remote survivors frame
    // on at 30 Hz; the standby device's own seat frames on at every local
    // millisecond, so its freeze lands on its client's rule (no fresh
    // progress for `FREEZE_AFTER_MS`), unless the promotion comes first.
    let mut standby = Some(standby);
    let mut own_last = drop_local;
    let mut own_frames_after_drop = 0u64;
    let (promoted, at) = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_r);
        let l = local(w);
        if own_seat.frozen_at().is_none() && l > own_last {
            let input = remote_input(20_000 + own_frames_after_drop, 0);
            let f = own_seat.frame(l, w, input).unwrap();
            own_last = l;
            own_frames_after_drop += u64::from(!f.frozen);
        }
        match standby.take().unwrap().promote() {
            Ok(promoted) => {
                let at = promoted.promoted_at_ms();
                break (promoted, at);
            }
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            drop_at.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let watch_ms = at - w_drop;
    let own_frozen_by = if own_seat.frozen_at().is_some() {
        "no progress"
    } else {
        "promote"
    };
    let own_frozen = own_seat.frozen_at().unwrap_or(local(at));
    let own_progress = own_seat.last_progress().expect("own seat progress");
    let keys = identities[0].keys().clone();
    let (mut links, mut recovery) = migration::SurvivorLinks::open(promoted.into_link()).unwrap();
    let mut routed: Vec<u64> = links.players().collect();
    routed.sort();
    let mut prepared = remote_ids.to_vec();
    prepared.sort();
    assert_eq!(routed, prepared, "only the prepared links are routed");
    // The standby device's own seat recovers from its actual client and the
    // journals its real endpoint kept since arming.
    let (own_client, own_delivery, own_intents) = own_seat.into_parts().unwrap();
    let own_tail = own_intents.tail();
    let own_in_flight = own_intents
        .pending()
        .filter(|(n, _)| *n > own_handled_c)
        .count();
    let own_pings: Vec<u32> = own_intents
        .pending()
        .filter(|(n, _)| *n > own_handled_c)
        .filter_map(|(_, b)| ping_of(b).map(|(id, _)| id))
        .collect();
    assert!(
        own_tail > own_handled_c && own_in_flight as u64 == own_tail - own_handled_c,
        "own seat in flight at C: tail={own_tail} handled={own_handled_c} pending={own_in_flight}"
    );
    let mut own = migration::OwnSeat::new(
        &root,
        recovery.sequencer().image(),
        own_client,
        own_delivery,
        own_intents,
    )
    .unwrap();
    let successor_host = migration::start_resume_host(
        &cfg,
        &identities[0],
        "Resumed",
        &code,
        descriptor.clone(),
        recovery.promotion().clone(),
    )
    .unwrap();

    // Frozen recovery at C. The host player's departure must commit after
    // every survivor's drain (the own seat's included), read from the
    // committed history after every step.
    let handled_at = |recovery: &Recovery, p: u64| {
        SessionImage::decode(&root, recovery.sequencer().image())
            .unwrap()
            .inputs[&p]
            .handled()
    };
    let mut term1: BTreeMap<(u64, u64), Vec<u8>> = BTreeMap::new();
    let mut order = DrainOrder {
        root: root.clone(),
        ids: remote_ids,
        absent: host_player,
        drained_at: None,
        retired_at: None,
    };
    let mut own_drained_at: Option<u64> = None;
    let mut own_over: Option<u64> = None;
    let failure = loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_r);
        let phase = match links.drive(&mut recovery, Some(&mut own), &keys, local(w), w) {
            Ok(phase) => phase,
            Err(error) => break Some(error),
        };
        if matches!(phase, RecoveryPhase::Replay | RecoveryPhase::Draining) {
            for record in recovery.replay().unwrap() {
                if record.receipt.term == 1 && record.output.delivery == Delivery::Reliable {
                    term1.insert(
                        (record.output.recipient, record.output.sequence),
                        record.output.bytes.clone(),
                    );
                }
            }
        }
        order.mark(&recovery, &devices.tail);
        let own_handled = handled_at(&recovery, own_player);
        if own_handled > own_tail {
            own_over = Some(own_handled);
        }
        if own_handled == own_tail && own_drained_at.is_none() {
            own_drained_at = Some(recovery.sequencer().receipt().index);
        }
        if phase == RecoveryPhase::Ready {
            break None;
        }
        assert!(wall() - at < 5000, "recovery stalled in {phase:?}");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(failure.is_none(), "recovery failed: {failure:?}");
    assert_eq!(own_over, None, "the own seat drained past its tail");
    // The original's device is gone for good now.
    drop(hung);
    let drained_at = order
        .drained_at
        .expect("the remote survivors never drained")
        .max(own_drained_at.expect("the own seat never drained"));
    let retired_at = order.retired_at;
    assert!(
        retired_at.is_some(),
        "the host player's departure never committed"
    );
    let retired_at = retired_at.unwrap();
    assert!(
        drained_at < retired_at,
        "departure committed at {retired_at}, survivors drained at {drained_at}"
    );

    // Final barrier: each survivor's cut and clock correction; the own seat
    // froze at `own_frozen` by its client's rule (or at its promotion).
    let mut barrier = links.barrier(recovery, &keys, wall()).unwrap();
    let own_expected = own.client().host_now(own_frozen).unwrap() as i64 - c_logical as i64;
    let own_cut_local = local(wall());
    let own_delta = own.ready(&mut barrier, own_frozen, own_cut_local, wall());
    assert!(
        own_delta.is_ok(),
        "own seat cut refused: {own_delta:?} frozen_by={own_frozen_by} frozen_after_progress={} expected_delta={own_expected}",
        own_frozen - own_progress
    );
    let own_delta = own_delta.unwrap();
    assert_eq!(own_delta, own_expected);
    let t = Instant::now();
    loop {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_r);
        if links.drive_barrier(&mut barrier, wall()).unwrap() {
            break;
        }
        bounded(t);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (successor, resume) = links.resume(barrier, &keys, wall()).unwrap();
    let own_resume_local = local(wall());
    own.resumed(own_resume_local).unwrap();
    assert_eq!(own.client().host_now(own_resume_local), Some(c_logical));
    let t = Instant::now();
    while devices.resumed_local.iter().any(Option::is_none) {
        let w = wall();
        devices.step(w, local(w), [false, false], handled_r);
        bounded(t);
    }
    let resumed_ms = drop_at.elapsed().as_millis();

    // At Resume, still at C in term 1.
    let image = SessionImage::decode(&root, successor.image()).unwrap();
    assert_eq!(successor.receipt().term, 1);
    assert_eq!(image.logical_ms, c_logical);
    assert_eq!(image.match_id, c_session.match_id);
    // The own seat: its in-flight intents drained exactly once, its replayed
    // outputs applied, each in-flight ping answered by exactly one pong.
    let own_handled = image.inputs[&own_player].handled();
    let (own_delivery, own_intents) = own.journals();
    let own_ceiling = successor
        .primary()
        .output_ceiling(own_player, Delivery::Reliable);
    let own_journals = (
        own_delivery.applied(),
        own_intents.tail(),
        own_intents.acknowledged(),
    );
    let own_pongs_once = own_pings.iter().all(|id| {
        term1
            .iter()
            .filter(|((q, _), b)| *q == own_player && pong_of(b).is_some_and(|(i, _, _)| i == *id))
            .count()
            == 1
    });
    let own_once = own_handled == own_tail
        && own_journals == (own_ceiling, own_tail, own_tail)
        && own_pongs_once;
    assert!(
        !own_pings.is_empty(),
        "no ping of the own seat was in flight at C"
    );
    // The remote survivors drained to their sealed tails.
    let tail = devices.tail.map(Option::unwrap);
    let remotes_drained = (0..2).all(|r| image.inputs[&remote_ids[r]].handled() == tail[r]);
    assert_eq!(
        devices.duplicates,
        [0, 0],
        "a replayed output applied twice"
    );
    // The host player departed after the drain: native leave rule (its racer
    // a bot, no human seat), its watermark retired, no member, no outputs.
    let end = migration::committed_authority(&root, successor.image()).unwrap();
    let host_bot = matches!(
        end.world().unwrap().cfg.racers[host_racer].kind,
        RacerKind::Bot(_)
    );
    let host_unseated = end.racer_of(host_seat).is_none();
    let host_member = image.members.iter().any(|m| m.player == host_player);
    let retired = |receipt: &Receipt, p: u64| {
        receipt
            .watermarks
            .iter()
            .any(|m| m.player == p && m.retired)
    };
    let host_retired = retired(successor.receipt(), host_player);
    let host_outbox = successor
        .primary()
        .outbox()
        .filter(|r| r.output.recipient == host_player)
        .count();
    let remote3_departed = retired(successor.receipt(), absent_remote)
        && !image.members.iter().any(|m| m.player == absent_remote);
    let frozen = c_auth.world().unwrap().tick == end.world().unwrap().tick;
    let own_cut = resume.cuts.iter().any(|c| c.player == own_player);
    println!(
        "seats_takeover resume own: in_flight={own_in_flight} pings={} handled_c={own_handled_c} tail={own_tail} handled={own_handled} journals={own_journals:?} ceiling={own_ceiling} pongs_once={own_pongs_once} frozen_by={own_frozen_by} frozen_after_progress={} frames_after_drop={own_frames_after_drop} delta={own_delta} margin={} cut={own_cut}",
        own_pings.len(),
        own_frozen - own_progress,
        250 - own_delta.unsigned_abs() as i64,
    );
    println!(
        "seats_takeover resume host: bot={host_bot} unseated={host_unseated} member={host_member} retired={host_retired} outbox={host_outbox} humans_c={humans_c} humans={} remote3_departed={remote3_departed} drained_at={drained_at} retired_at={retired_at} remotes_drained={remotes_drained} frozen={frozen}",
        end.human_count()
    );
    assert!(
        own_once,
        "the own seat's in-flight intents did not drain once"
    );
    assert!(remotes_drained, "a remote survivor did not drain");
    assert!(host_bot, "the host player's racer did not become a bot");
    assert!(host_unseated, "the host player still holds a human seat");
    assert_eq!(end.human_count(), 3, "humans after the departures");
    assert!(!host_member, "the host player is still a member");
    assert!(host_retired, "the host player's outbox did not retire");
    assert_eq!(host_outbox, 0, "outputs retained for the host player");
    assert!(remote3_departed, "the idle third human did not depart");
    assert!(frozen, "game time moved during recovery");

    // Every survivor resumed; the successor then runs the match alone in
    // lockstep at STEP on the devices' local clocks (fast-forwarded past
    // wall time), every playing seat on the native bot as an autopilot.
    let SurvivorDevices {
        recovering,
        resumed_local,
        ..
    } = devices;
    let resumed_local = resumed_local.map(Option::unwrap);
    let mut survivors: Vec<migration::RemotePlayer> = recovering
        .into_iter()
        .map(|r| r.unwrap().into_resumed().unwrap())
        .collect();
    for r in 0..2 {
        let (delivery, intents) = survivors[r].endpoint().journals();
        let ceiling = successor
            .primary()
            .output_ceiling(remote_ids[r], Delivery::Reliable);
        assert_eq!(
            (delivery.applied(), intents.tail(), intents.acknowledged()),
            (ceiling, tail[r], tail[r]),
            "survivor {r} endpoint journals at Resume"
        );
        assert!(survivors[r].under_successor() && !survivors[r].protected());
    }
    let mut live = links
        .into_successor(successor, Some(own), Some(successor_host), keys.clone())
        .unwrap();
    let no_phantom = live.queue(host_player).is_none() && !live.players().any(|p| p == host_player);
    assert!(no_phantom, "a phantom seat for the host player");
    let play_seats = [
        seat_of(own_player),
        seat_of(remote_ids[0]),
        seat_of(remote_ids[1]),
    ];
    let autopilot = |a: &HostAuthority, seat: u8| match (a.world(), a.racer_of(seat)) {
        (Some(w), Some(i)) if i < w.racers.len() && w.phase == RacePhase::Racing => {
            race_core::bot_input(w, i, race_core::BotLevel::Normal)
        }
        _ => Input::default(),
    };
    let base_local = resumed_local
        .iter()
        .copied()
        .max()
        .unwrap()
        .max(own_resume_local);
    let mut committed = live.receipt().index;
    let mut done_at: Option<u64> = None;
    let mut host_outputs = 0usize;
    let mut frozen_frames = 0usize;
    let mut k = 0u64;
    while k < 9000 {
        k += 1;
        let vl = base_local + k * STEP;
        let mut sent = [false; 2];
        for r in 0..2 {
            let input = autopilot(live.authority(), play_seats[r + 1]);
            let f = survivors[r].frame(vl, wall(), input);
            assert!(f.is_ok(), "survivor {r} refused a successor frame: {f:?}");
            let f = f.unwrap();
            assert!(
                f.newest_receipt.is_none_or(|n| n <= committed),
                "survivor {r} applied an output before its commit"
            );
            frozen_frames += usize::from(f.frozen);
            sent[r] = f.views_sent > 0;
        }
        let t = Instant::now();
        loop {
            live.poll(wall()).unwrap();
            let arrived = (0..2).all(|r| {
                !sent[r]
                    || live
                        .queue(remote_ids[r])
                        .is_some_and(|q| q.lossy_views().next().is_some())
            });
            if arrived || t.elapsed() > Duration::from_millis(20) {
                break;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        let input = autopilot(live.authority(), play_seats[0]);
        live.own_frame(vl, input).unwrap();
        assert!(live.stage(c_logical + k * STEP, |_| Ok(())).unwrap());
        committed = live.commit(wall()).unwrap().index;
        host_outputs += live
            .primary()
            .outbox()
            .filter(|r| r.output.recipient == host_player)
            .count();
        if live.authority().results().is_some() && done_at.is_none() {
            done_at = Some(k);
        }
        if done_at.is_some_and(|d| k >= d + 15) {
            break;
        }
    }
    let a = live.authority();
    let finished = a.world().map_or(0, |w| {
        w.racers.iter().filter(|r| r.finished_at.is_some()).count()
    });
    let host_retired_end = retired(live.receipt(), host_player);
    let no_phantom_end =
        live.queue(host_player).is_none() && !live.players().any(|p| p == host_player);
    println!(
        "STAMPEDE SEATS TAKEOVER own_in_flight={own_in_flight} own_drained_once={own_once} own_frozen_by={own_frozen_by} own_delta={own_delta} host_departed={} host_bot={host_bot} host_retired={host_retired_end} host_outputs={host_outputs} phantom={} survivors_resumed=3 remote3_departed={remote3_departed} drained_at={drained_at} departed_at={retired_at} watch_ms={watch_ms} all_resumed_ms={resumed_ms} race_done_batch={done_at:?} finished={finished} frozen_frames={frozen_frames} paused={}",
        !host_member && host_unseated,
        !no_phantom_end,
        live.failure().is_some()
    );
    assert!(done_at.is_some(), "the race never finished");
    assert!(live.failure().is_none(), "the successor paused");
    assert!(
        host_retired_end && no_phantom_end,
        "the host player came back"
    );
    assert_eq!(host_outputs, 0, "an output was queued for the host player");
}

/// One roster member floods its prepared control link to the armed standby.
/// Only that link is closed (no ban): the standby never latches, the original
/// keeps committing through it, another member's prepared link stays up, and
/// the standby can still promote.
#[test]
fn a_flood_on_one_prepared_link_closes_only_that_link() {
    let mut room = ArmedRoom::new();
    let root = room.root().clone();
    let standby_at = loopback(room.standby.listener().unwrap(), root.standby);
    let warm_binding = session_binding(&root, 0, LinkRole::ControlPlayer, LinkRole::Standby);
    let mut links = Vec::new();
    for i in 1..3 {
        let (id, cfg) = (room.identities[i].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        });
        links.push(link.expect("prepared control link"));
    }
    let flood = 4 * migration::PEER_PENDING_RECORDS.max(MAX_JOURNAL_RECORDS / 4);
    for n in 0..flood {
        links[0].send_reliable(vec![n as u8; 64]);
    }
    let t = Instant::now();
    let mut closed = false;
    while !closed {
        let polled = room.standby.poll();
        assert!(
            polled.is_ok() && room.standby.failure().is_none(),
            "the flood latched the standby: {polled:?}"
        );
        while let Some(event) = links[0].poll() {
            closed |= matches!(
                event,
                ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
            );
        }
        bounded(t);
    }
    // A flapping member: every connect and drop leaves nothing held.
    let flaps = 40;
    let mut peak_held = 0;
    for _ in 0..flaps {
        let (id, cfg) = (room.identities[3].clone(), room.cfg.clone());
        let link = dial(&cfg, &id, standby_at.clone(), warm_binding.clone(), || {
            room.standby.poll().unwrap();
        })
        .expect("flapping link");
        drop(link);
        peak_held = peak_held.max(room.standby.pending_events());
    }
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(100) {
        assert!(room.standby.poll().is_ok(), "flapping latched the standby");
        peak_held = peak_held.max(room.standby.pending_events());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        peak_held <= 4,
        "a flapping link left {peak_held} events held"
    );
    // The room keeps committing through the standby.
    let before = room.standby.receipt().index;
    for _ in 0..5 {
        let index = room.standby.receipt().index + 1;
        room.commit(index);
    }
    let committed = room.standby.receipt().index - before;
    assert_eq!(committed, 5, "commits stalled after the flood");
    assert!(room.standby.failure().is_none());
    let overflowed = room.standby.overflowed_peers();
    assert_eq!(overflowed, 1, "only the flooding link overflows");
    // The other member's prepared link is untouched.
    let mut other_up = true;
    while let Some(event) = links[1].poll() {
        other_up &= !matches!(
            event,
            ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
        );
    }
    assert!(other_up, "another member's prepared link was closed");
    // The original stops: the standby still promotes on its watch.
    let ArmedRoom {
        standby,
        live,
        players,
        ..
    } = room;
    let hung = (live, players);
    let mut standby = Some(standby);
    let t = Instant::now();
    let promoted = loop {
        match standby.take().unwrap().promote() {
            Ok(promoted) => break promoted,
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused after the flood: {refused:?}"),
        }
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    drop(hung);
    println!(
        "STAMPEDE PREPARED LINK FLOOD frames={flood} flooder_closed={closed} flaps={flaps} peak_held_events={peak_held} overflowed_peers={overflowed} commits_after={committed} other_link_up={other_up} standby_promoted=true promote_c_index={}",
        promoted.recovery.sequencer().receipt().index
    );
}

// Arming over the room's own messages (slices 6.5 and 6.6): the coordinators
// carry Ask, Offer, Consent, Setup and the ACKs as real migration messages on
// the room's QUIC links. Nothing is handed across in process.

/// Room time for the coordinators: one monotonic clock per test.
fn ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// The host's frame: its native update (a no-op while frozen), then arming.
fn host_frame<const N: usize>(lobby: &mut Lobby<N>, arming: &mut migration::HostArming, now: u64) {
    lobby.host.update(now);
    arming.poll(&mut lobby.host, &mut lobby.network.0.lock().unwrap(), now);
}

/// Every client's frame: its native update, then its arming coordinator (if
/// it runs one). The selected device's person says `yes` when asked.
fn client_frame<const N: usize>(
    lobby: &mut Lobby<N>,
    clients: &mut [Option<migration::ClientArming>],
    now: u64,
    yes: bool,
) {
    for (i, client) in lobby.clients.iter_mut().enumerate() {
        client.update(now, Input::default());
        if let Some(arming) = clients[i].as_mut() {
            let mut link = lobby.links[i].0.lock().unwrap();
            arming.poll(client, &mut link, now);
            if arming.consent_requested() {
                arming.answer(&mut link, yes);
            }
        }
    }
    if let Some(own) = lobby.host_client.as_mut() {
        own.update(now, Input::default());
    }
}

impl<const N: usize> Lobby<N> {
    fn code(&self) -> String {
        self.network.0.lock().unwrap().code().unwrap().to_owned()
    }
    fn start_arming(&mut self) -> migration::HostArming {
        migration::HostArming::start(
            &mut self.host,
            &mut self.network.0.lock().unwrap(),
            &self.cfg,
            &self.original,
            self.peers[0],
        )
    }
    fn arming_client(&self, i: usize) -> Option<migration::ClientArming> {
        Some(migration::ClientArming::new(
            &self.cfg,
            &self.identities[i],
            &self.code(),
        ))
    }
    /// Send a raw ordinary message from client `i`, as a hostile or scripted
    /// peer would.
    fn raw_from(&self, i: usize, message: &race_session::proto::ToHost) {
        self.links[i]
            .0
            .lock()
            .unwrap()
            .send_reliable(race_session::proto::encode(message));
    }
    fn raw_migration_from(&self, i: usize, payload: &race_session::proto::ToHostMigration) {
        for part in migration::wire::chunk(payload).unwrap() {
            self.raw_from(i, &race_session::proto::ToHost::Migration(part));
        }
    }
    fn character_of(&self, i: usize) -> u8 {
        let name = [
            "Backup", "Other", "Third", "Fourth", "Fifth", "Sixth", "Seventh", "Eighth",
        ][i];
        self.host
            .lobby_view()
            .seats
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .character
    }
}

fn image(host: &HostAuthority) -> Vec<u8> {
    migration::checkpoint(host).unwrap().1
}

fn is_set_character(event: &HostNetEvent, peer: PeerKey, character: u8) -> bool {
    matches!(event, HostNetEvent::Reliable { peer: p, bytes }
        if *p == peer
            && race_session::proto::decode::<race_session::proto::ToHost>(bytes)
                == Some(race_session::proto::ToHost::SetCharacter(character)))
}

/// (a) Ask to Armed over the room's links: two-phase consent, the pair link,
/// activation and every human's automatic ACK. The authority image is
/// byte-identical across phase 2, and an ordinary message sent meanwhile is
/// held, then reaches the authority when the boundary ends.
fn arming_reaches_armed(phone: bool) {
    let mut lobby = Lobby::<2>::build(phone);
    let mut arming = lobby.start_arming();
    assert_eq!(arming.state(), &migration::ArmingState::Asking);
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1)];
    let started = Instant::now();
    let mut frozen_image = None;
    loop {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        if arming.state() == &migration::ArmingState::AwaitingConsent && frozen_image.is_none() {
            assert!(lobby.host.migration_frozen());
            frozen_image = Some(image(&lobby.host));
            // A seated player changes character while the lobby is frozen.
            lobby.raw_from(1, &race_session::proto::ToHost::SetCharacter(3));
        }
        if let Some(frozen) = &frozen_image {
            assert_eq!(
                &image(&lobby.host),
                frozen,
                "the authority moved in phase 2"
            );
        }
        if arming.state().is_finished() {
            break;
        }
        client_frame(&mut lobby, &mut clients, now, true);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            arming.state()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(arming.state(), &migration::ArmingState::Armed);
    assert_eq!(arming.ignored(), 0);
    // The standby finishes its side once its Armed reply has been flushed.
    while clients[0].as_ref().unwrap().state() != &migration::ArmingState::Armed {
        client_frame(&mut lobby, &mut clients, ms(started), true);
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(1));
    }
    let armed = arming.take_armed().unwrap();
    let standby = clients[0].as_mut().unwrap().take_standby().unwrap();
    assert_eq!(
        armed.bootstrap.primary.committed_image(),
        standby.sequencer.image()
    );
    assert_eq!(armed.bootstrap.primary.receipt(), &standby.armed.receipt);
    assert_eq!(
        armed.descriptor.root.standby,
        lobby.identities[0].pubkey().to_bytes()
    );
    assert_eq!(armed.own_ack.is_some(), phone);
    // Both humans signed exactly once and kept their bytes.
    for client in &clients {
        assert!(client.as_ref().unwrap().capability_ack().is_some());
    }
    assert_eq!(
        clients[1].as_ref().unwrap().state(),
        &migration::ArmingState::Activating
    );
    // Still frozen at Armed, image unchanged; the held message waits.
    assert!(lobby.host.migration_frozen());
    assert_eq!(image(&lobby.host), frozen_image.clone().unwrap());
    assert_eq!(lobby.character_of(1), 1);
    assert!(armed
        .held
        .iter()
        .any(|e| is_set_character(e, lobby.peers[1], 3)));
    migration::thaw_lobby(&mut lobby.host, armed.held);
    assert!(!lobby.host.migration_frozen());
    assert_eq!(lobby.character_of(1), 1);
    lobby.host.update(ms(started));
    assert_eq!(lobby.character_of(1), 3);
    println!(
        "ARMING OVER THE ROOM phone_host={phone} state=Armed image_unchanged_in_phase_2=true held_event_applied_after=true ms={}",
        ms(started)
    );
}

#[test]
fn arming_runs_from_ask_to_armed_over_room_messages_with_the_lobby_frozen() {
    arming_reaches_armed(false);
}

#[test]
fn a_playing_host_signs_its_own_seat_and_arms_over_room_messages() {
    arming_reaches_armed(true);
}

/// (b) The person says no: the room stays unarmed with a reason, and the
/// lobby was never frozen.
#[test]
fn a_declined_ask_leaves_the_room_unarmed_with_a_reason() {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1)];
    let started = Instant::now();
    while !arming.state().is_finished() {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        assert!(!lobby.host.migration_frozen());
        client_frame(&mut lobby, &mut clients, now, false);
        bounded(started);
    }
    let migration::ArmingState::Refused { reason } = arming.state() else {
        panic!("{:?}", arming.state())
    };
    assert!(reason.contains("Backup declined"), "{reason}");
    assert!(matches!(
        clients[0].as_ref().unwrap().state(),
        migration::ArmingState::Refused { .. }
    ));
    assert!(clients[1].as_ref().unwrap().capability_ack().is_none());
    assert!(arming.take_armed().is_none());
}

/// (c) A room opened with an online code refuses arming with a visible
/// reason, and asks no one.
#[test]
fn an_online_code_room_refuses_arming_with_a_visible_reason() {
    let mut lobby = Lobby::<2>::build_in(false, true);
    let mut arming = lobby.start_arming();
    let migration::ArmingState::Refused { reason } = arming.state().clone() else {
        panic!("{:?}", arming.state())
    };
    assert!(reason.contains("online code"), "{reason}");
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1)];
    let started = Instant::now();
    while ms(started) < 300 {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        client_frame(&mut lobby, &mut clients, now, true);
        assert!(!clients[0].as_ref().unwrap().consent_requested());
        assert_eq!(
            clients[0].as_ref().unwrap().state(),
            &migration::ArmingState::Idle
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!lobby.host.migration_frozen());
}

/// Run until the host's arming finishes, with `script` called on every frame
/// (before the clients) for scripted or hostile peers.
fn run_until_finished<const N: usize>(
    lobby: &mut Lobby<N>,
    arming: &mut migration::HostArming,
    clients: &mut [Option<migration::ClientArming>],
    started: Instant,
    mut script: impl FnMut(&mut Lobby<N>, &migration::HostArming),
) {
    let mut frozen_image: Option<Vec<u8>> = None;
    loop {
        let now = ms(started);
        host_frame(lobby, arming, now);
        if lobby.host.migration_frozen() {
            let now_image = image(&lobby.host);
            match &frozen_image {
                Some(frozen) => assert_eq!(&now_image, frozen, "the authority moved in phase 2"),
                None => frozen_image = Some(now_image),
            }
        }
        if arming.state().is_finished() {
            return;
        }
        script(lobby, arming);
        client_frame(lobby, clients, now, true);
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "{:?}",
            arming.state()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// (d) A capability ACK that does not verify against the frozen image: the
/// room plays unprotected, the reason names the client, and the lobby is
/// released.
#[test]
fn a_bad_capability_ack_leaves_the_room_unprotected_naming_the_client() {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    // "Other" runs no coordinator: the script answers its Setup with an ACK
    // properly signed by its key, over the wrong initial image.
    let mut clients = [lobby.arming_client(0), None];
    let started = Instant::now();
    let mut sent = false;
    run_until_finished(
        &mut lobby,
        &mut arming,
        &mut clients,
        started,
        |lobby, _| {
            for event in lobby.clients[1].take_migration_events() {
                let migration::wire::MigrationEvent::Received(
                    race_session::proto::ToClientMigration::Setup { descriptor, .. },
                ) = event
                else {
                    continue;
                };
                let root = Descriptor::decode(&descriptor).unwrap().root;
                let player = root
                    .members
                    .iter()
                    .find(|m| m.key == Some(lobby.identities[1].pubkey().to_bytes()))
                    .unwrap()
                    .player;
                let ack =
                    CapabilityAck::sign(&root, player, [0x55; 32], 1, lobby.identities[1].keys())
                        .unwrap()
                        .encode(&root)
                        .unwrap();
                lobby.raw_migration_from(1, &race_session::proto::ToHostMigration::Ack { ack });
                sent = true;
            }
        },
    );
    assert!(sent);
    let migration::ArmingState::Unprotected { reason } = arming.state() else {
        panic!("{:?}", arming.state())
    };
    assert!(reason.contains("Other"), "{reason}");
    assert!(reason.contains("capability ACK"), "{reason}");
    assert!(!lobby.host.migration_frozen());
    // The standby is told and drops its half.
    while !clients[0].as_ref().unwrap().state().is_finished() {
        client_frame(&mut lobby, &mut clients, ms(started), true);
        bounded(started);
    }
    assert!(matches!(
        clients[0].as_ref().unwrap().state(),
        migration::ArmingState::Unprotected { .. }
    ));
}

/// (e) A client that never ACKs: after the 5 s window the room is
/// unprotected, naming it, and the boundary is released: the message held
/// meanwhile reaches the authority then, not before.
#[test]
fn a_client_that_never_acks_times_out_unprotected_and_releases_the_lobby() {
    let mut lobby = Lobby::<3>::with_humans();
    let mut arming = lobby.start_arming();
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1), None];
    let started = Instant::now();
    let mut injected = false;
    let mut setup_at = None;
    run_until_finished(
        &mut lobby,
        &mut arming,
        &mut clients,
        started,
        |lobby, arming| {
            if arming.state() == &migration::ArmingState::AwaitingConsent && !injected {
                lobby.raw_from(2, &race_session::proto::ToHost::SetCharacter(5));
                injected = true;
            }
            if setup_at.is_none() && clients_saw_setup(lobby.clients[2].take_migration_events()) {
                setup_at = Some(ms(started));
            }
            if injected {
                assert_eq!(lobby.character_of(2), 2, "held until the boundary ends");
            }
        },
    );
    let migration::ArmingState::Unprotected { reason } = arming.state() else {
        panic!("{:?}", arming.state())
    };
    assert!(reason.contains("Third"), "{reason}");
    assert!(reason.contains("5 s"), "{reason}");
    assert!(ms(started) - setup_at.unwrap() >= 4900);
    assert!(!lobby.host.migration_frozen());
    assert_eq!(lobby.character_of(2), 2);
    lobby.host.update(ms(started));
    assert_eq!(lobby.character_of(2), 5);
    for client in &clients[..2] {
        assert!(client.as_ref().unwrap().capability_ack().is_some());
    }
}

fn clients_saw_setup(
    events: Vec<migration::wire::MigrationEvent<race_session::proto::ToClientMigration>>,
) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            migration::wire::MigrationEvent::Received(
                race_session::proto::ToClientMigration::Setup { .. }
            )
        )
    })
}

/// (f) A hostile seated peer sends a well-formed Consent (signed by the real
/// standby key, for this room) before the selected device answers. It is not
/// from the selected device, so it is refused without failing arming.
#[test]
fn a_forged_consent_from_another_seated_peer_is_refused() {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1)];
    let started = Instant::now();
    while arming.state() != &migration::ArmingState::AwaitingConsent {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        if arming.state() == &migration::ArmingState::AwaitingConsent {
            break;
        }
        client_frame(&mut lobby, &mut clients, now, true);
        bounded(started);
    }
    // Before the selected device has even read the offer.
    let forged = lobby.consent(&lobby.offer()).encode().unwrap();
    lobby.raw_migration_from(
        1,
        &race_session::proto::ToHostMigration::Consent {
            descriptor: forged,
            addr: "127.0.0.1:9".into(),
            token: [7; 32],
        },
    );
    while arming.ignored() == 0 {
        host_frame(&mut lobby, &mut arming, ms(started));
        assert_eq!(arming.state(), &migration::ArmingState::AwaitingConsent);
        bounded(started);
    }
    run_until_finished(&mut lobby, &mut arming, &mut clients, started, |_, _| {});
    assert_eq!(arming.state(), &migration::ArmingState::Armed);
    let armed = arming.take_armed().unwrap();
    assert_eq!(
        armed.descriptor.root.standby,
        lobby.identities[0].pubkey().to_bytes()
    );
}

/// A client signs only for a setup that verifies: a host-signed descriptor
/// without the standby's consent gets no ACK.
#[test]
fn a_client_refuses_to_ack_a_setup_without_the_standby_consent() {
    let mut lobby = Lobby::new();
    let offer = lobby.offer();
    let setup = race_session::proto::ToClientMigration::Setup {
        descriptor: offer.descriptor().encode().unwrap(),
        initial: offer.initial_hash().unwrap(),
    };
    for part in migration::wire::chunk(&setup).unwrap() {
        lobby.network.0.lock().unwrap().send_reliable(
            lobby.peers[1],
            race_session::proto::encode(&race_session::proto::ToClient::Migration(part)),
        );
    }
    let mut clients = [None, lobby.arming_client(1)];
    let started = Instant::now();
    let client = loop {
        client_frame(&mut lobby, &mut clients, ms(started), true);
        let client = clients[1].as_ref().unwrap();
        if client.state().is_finished() || client.capability_ack().is_some() {
            break client;
        }
        bounded(started);
    };
    assert!(
        client.capability_ack().is_none(),
        "signed an unverified setup"
    );
    let migration::ArmingState::Refused { reason } = client.state() else {
        panic!("{:?}", client.state())
    };
    assert!(reason.contains("did not verify"), "{reason}");
}

// Review fixes to the arming handshake: the freeze, failures that reach
// every client, the hold bounds, attempt identity and the timeouts.

use race_session::proto::{ToClient, ToClientMigration, ToHost, ToHostMigration};

/// Send a migration message from the host to client `i`, as a scripted host.
fn raw_to<const N: usize>(lobby: &Lobby<N>, i: usize, payload: &ToClientMigration) {
    for part in migration::wire::chunk(payload).unwrap() {
        lobby.network.0.lock().unwrap().send_reliable(
            lobby.peers[i],
            race_session::proto::encode(&ToClient::Migration(part)),
        );
    }
}

/// Start arming with "Backup" scripted (no coordinator) to say yes, and run
/// the host until the lobby is frozen with the offer out.
fn frozen_with_offer<const N: usize>(
    lobby: &mut Lobby<N>,
    started: Instant,
) -> migration::HostArming {
    let mut arming = lobby.start_arming();
    lobby.raw_migration_from(0, &ToHostMigration::Willing);
    while arming.state() != &migration::ArmingState::AwaitingConsent {
        host_frame(lobby, &mut arming, ms(started));
        assert!(!arming.state().is_finished(), "{:?}", arming.state());
        bounded(started);
    }
    assert!(lobby.host.migration_frozen());
    arming
}

fn reason(state: &migration::ArmingState) -> &str {
    match state {
        migration::ArmingState::Refused { reason }
        | migration::ArmingState::Unprotected { reason } => reason,
        other => panic!("not finished with a reason: {other:?}"),
    }
}

/// Item 1: a failure inside `finish` (here the image check: the app released
/// the boundary under the running attempt and changed the lobby, the only
/// way left to move a frozen authority) tells every client, each resets its
/// coordinator, and a later attempt arms.
#[test]
fn a_failed_finish_tells_every_client_and_a_later_attempt_arms() {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1)];
    let started = Instant::now();
    let mut released = false;
    while !arming.state().is_finished() {
        let now = ms(started);
        if !released && clients[1].as_ref().unwrap().capability_ack().is_some() {
            // Every human has its Setup; before the host reads the ACKs.
            migration::thaw_lobby(&mut lobby.host, Vec::new());
            assert!(lobby.host.set_laps(4));
            released = true;
        }
        if !released {
            lobby.host.update(now);
        }
        arming.poll(&mut lobby.host, &mut lobby.network.0.lock().unwrap(), now);
        if arming.state().is_finished() {
            break;
        }
        client_frame(&mut lobby, &mut clients, now, true);
        bounded(started);
    }
    let migration::ArmingState::Unprotected { reason } = arming.state() else {
        panic!("{:?}", arming.state())
    };
    assert!(reason.contains("changed while it was held"), "{reason}");
    let t = Instant::now();
    while clients.iter().any(|c| {
        !matches!(
            c.as_ref().unwrap().state(),
            migration::ArmingState::Unprotected { .. }
        )
    }) {
        client_frame(&mut lobby, &mut clients, ms(started), true);
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "every client is told: {:?}",
            clients
                .iter()
                .map(|c| c.as_ref().unwrap().state().clone())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    for client in &clients {
        assert!(client.as_ref().unwrap().capability_ack().is_none());
    }
    // A later attempt, with the same coordinators, arms.
    lobby.host.update(ms(started));
    let mut again = lobby.start_arming();
    let restarted = Instant::now();
    run_until_finished(&mut lobby, &mut again, &mut clients, restarted, |_, _| {});
    assert_eq!(again.state(), &migration::ArmingState::Armed);
    let armed = again.take_armed().unwrap();
    migration::thaw_lobby(&mut lobby.host, armed.held);
    assert!(!lobby.host.migration_frozen());
}

fn sample_track() -> race_core::CustomTrack {
    let raw: Vec<_> = (0..=500)
        .map(|i| {
            let a = i as f32 / 500.0 * std::f32::consts::TAU;
            race_core::Vec2::new(a.cos() * 180.0, a.sin() * 180.0)
        })
        .collect();
    let shaped = race_core::shape(&raw, false, 18.0).unwrap();
    race_core::custom_track_from(
        &shaped,
        "Loop",
        race_core::TrackId::Oasis,
        7,
        180,
        vec![],
        9,
    )
}

/// Item 2: while frozen every lobby control is refused, changes nothing and
/// sends nothing; after the boundary they work again.
#[test]
fn every_lobby_control_is_refused_while_the_lobby_is_frozen() {
    use race_core::{BotLevel, TrackId};
    let mut lobby = Lobby::new();
    let started = Instant::now();
    let mut arming = frozen_with_offer(&mut lobby, started);
    let frozen = image(&lobby.host);
    let seat = lobby
        .host
        .lobby_view()
        .seats
        .iter()
        .find(|s| s.name == "Other")
        .unwrap()
        .id;
    let host = &mut lobby.host;
    assert!(!host.set_track(TrackId::Oasis));
    assert!(host.set_custom_track(&sample_track()).is_err());
    assert!(!host.set_laps(4));
    assert!(!host.set_le_mans_start(true));
    assert!(!host.set_bots(2));
    assert!(!host.set_bot_level(BotLevel::Hard));
    assert!(!host.set_battle(Some(race_session::proto::BattleSetup::new(
        TrackId::DustBowl
    ))));
    assert!(!host.kick(seat));
    assert!(!host.start_race(ms(started)));
    assert!(!host.to_lobby());
    assert_eq!(image(&lobby.host), frozen);
    assert_eq!(lobby.host.human_count(), 2);
    // Nothing went out: "Other" is neither kicked nor sent a new lobby.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(200) {
        lobby.clients[1].update(ms(started), Input::default());
        assert!(matches!(
            lobby.clients[1].phase(),
            race_session::ClientPhase::Lobby
        ));
        assert!(lobby.clients[1].lobby().unwrap().custom.is_none());
        std::thread::sleep(Duration::from_millis(1));
    }
    arming.cancel(
        &mut lobby.host,
        &mut lobby.network.0.lock().unwrap(),
        "Cancelled.",
    );
    assert!(!lobby.host.migration_frozen());
    assert!(lobby.host.set_laps(4));
}

/// Item 3: a player who joins during phase 2 leaves the room unarmed with a
/// reason, and is seated natively once the lobby is released. (A Bluetooth
/// peer cannot reach this LAN room: one Fray host has one path, so the
/// newcomer is a LAN joiner; the check is the same for any path.)
#[test]
fn a_player_joining_while_arming_leaves_the_room_unarmed_and_is_seated_after() {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1)];
    let started = Instant::now();
    while arming.state() != &migration::ArmingState::AwaitingConsent {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        if arming.state() == &migration::ArmingState::AwaitingConsent {
            break;
        }
        client_frame(&mut lobby, &mut clients, now, true);
        bounded(started);
    }
    let humans = lobby.host.human_count();
    let newcomer_id = approved();
    let link = {
        let net = lobby.network.0.lock().unwrap();
        let h = net.handle().unwrap();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], h.local_addr().unwrap().port()));
        ClientLink(Arc::new(Mutex::new(QuicClient::new(
            decented_net::join_addr(&lobby.cfg, &newcomer_id, addr, &h.lan_token()),
        ))))
    };
    let mut newcomer = ClientSession::new(Box::new(link.clone()), "Late", 4);
    // Only the host and the newcomer run, so the backup cannot consent yet.
    while link.0.lock().unwrap().link_path().is_none() {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        newcomer.update(now, Input::default());
        bounded(started);
    }
    for _ in 0..50 {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        newcomer.update(now, Input::default());
        bounded(started);
    }
    assert_eq!(arming.state(), &migration::ArmingState::AwaitingConsent);
    run_until_finished(&mut lobby, &mut arming, &mut clients, started, |_, _| {
        newcomer.update(ms(started), Input::default());
    });
    let migration::ArmingState::Unprotected { reason } = arming.state() else {
        panic!("{:?}", arming.state())
    };
    assert!(reason.contains("joined while arming"), "{reason}");
    assert!(!lobby.host.migration_frozen());
    let t = Instant::now();
    loop {
        let now = ms(started);
        lobby.host.update(now);
        newcomer.update(now, Input::default());
        if lobby.host.human_count() == humans + 1
            && matches!(newcomer.phase(), race_session::ClientPhase::Lobby)
        {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "{:?}",
            newcomer.phase()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(lobby
        .host
        .lobby_view()
        .seats
        .iter()
        .any(|s| s.name == "Late"));
}

/// Items 4 and 5: migration parts from an unselected peer count against its
/// own hold bound. Over it, the peer is disconnected, arming ends with its
/// name, and its `Left`, held, replays exactly once at the thaw.
#[test]
fn an_unselected_peer_flooding_migration_parts_is_cut_off_at_its_hold_bound() {
    let mut lobby = Lobby::new();
    let started = Instant::now();
    let mut arming = frozen_with_offer(&mut lobby, started);
    let humans = lobby.host.human_count();
    for _ in 0..migration::HELD_PER_PEER_RECORDS + 16 {
        lobby.raw_migration_from(1, &ToHostMigration::Decline);
    }
    while !arming.state().is_finished() {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        lobby.clients[1].update(now, Input::default());
        bounded(started);
    }
    let migration::ArmingState::Refused { reason } = arming.state() else {
        panic!("{:?}", arming.state())
    };
    assert!(reason.contains("Other left during arming"), "{reason}");
    // Parts past the bound are dropped unread (and when the `Left` lands in
    // the same read, arming ends before the batch's messages are looked at).
    assert!(arming.ignored() <= migration::HELD_PER_PEER_RECORDS as u64);
    assert!(!lobby.host.migration_frozen());
    assert_eq!(lobby.host.human_count(), humans);
    lobby.host.update(ms(started));
    assert_eq!(lobby.host.human_count(), humans - 1);
    lobby.host.update(ms(started));
    assert_eq!(lobby.host.human_count(), humans - 1);
}

/// Item 5: attempts are bound by ordering. A `Willing` that reached the host
/// before this attempt's `Ask` (left over from an earlier one) is discarded;
/// only one that arrives after it counts.
#[test]
fn a_willing_from_before_the_ask_is_ignored() {
    let mut lobby = Lobby::new();
    lobby.raw_migration_from(0, &ToHostMigration::Willing);
    let started = Instant::now();
    while ms(started) < 500 {
        lobby.host.update(ms(started));
        bounded(started);
    }
    let mut arming = lobby.start_arming();
    assert_eq!(arming.ignored(), 1, "the early Willing was discarded");
    while ms(started) < 800 {
        host_frame(&mut lobby, &mut arming, ms(started));
        assert_eq!(arming.state(), &migration::ArmingState::Asking);
        assert!(!lobby.host.migration_frozen());
        bounded(started);
    }
    lobby.raw_migration_from(0, &ToHostMigration::Willing);
    while arming.state() != &migration::ArmingState::AwaitingConsent {
        host_frame(&mut lobby, &mut arming, ms(started));
        bounded(started);
    }
    arming.cancel(
        &mut lobby.host,
        &mut lobby.network.0.lock().unwrap(),
        "Cancelled.",
    );
}

/// Item 5: phase 1 gives up after 60 s without an answer, on the host and
/// on the device (whose person never answers, or whose host never offers).
#[test]
fn phase_one_gives_up_after_60_s_without_an_answer() {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    let mut backup = lobby.arming_client(0).unwrap();
    let started = Instant::now();
    let poll_backup = |lobby: &mut Lobby, backup: &mut migration::ClientArming, at: u64| {
        lobby.clients[0].update(ms(started), Input::default());
        let mut link = lobby.links[0].0.lock().unwrap();
        backup.poll(&mut lobby.clients[0], &mut link, at).clone()
    };
    let mut asked = 0;
    while !backup.consent_requested() {
        asked = ms(started);
        host_frame(&mut lobby, &mut arming, asked);
        poll_backup(&mut lobby, &mut backup, asked);
        bounded(started);
    }
    let window = migration::ANSWER_WINDOW_MS;
    // The device first: the host's Withdrawn would otherwise race it.
    assert_eq!(
        poll_backup(&mut lobby, &mut backup, asked + window - 1),
        migration::ArmingState::Asking
    );
    let state = poll_backup(&mut lobby, &mut backup, asked + window);
    assert!(
        reason(&state).contains("backup did not answer"),
        "{state:?}"
    );
    // Then the host, with no update in between, so the device's Decline is
    // not read yet.
    let net = lobby.network.clone();
    assert_eq!(
        arming.poll(&mut lobby.host, &mut net.0.lock().unwrap(), window / 2),
        &migration::ArmingState::Asking
    );
    let state = arming.poll(&mut lobby.host, &mut net.0.lock().unwrap(), asked + window);
    assert!(reason(state).contains("backup did not answer"), "{state:?}");
    assert!(!lobby.host.migration_frozen());
    // The device's timeout also declined; let it reach the host before the
    // next attempt (one still in flight would be read as that attempt's).
    let mut declined = false;
    while !declined {
        lobby.host.update(ms(started));
        declined = lobby.host.take_migration_events().iter().any(|(_, e)| {
            matches!(
                e,
                migration::wire::MigrationEvent::Received(ToHostMigration::Decline)
            )
        });
        bounded(started);
    }
    // Said yes, and no offer follows: the device gives up too.
    let mut arming = lobby.start_arming();
    while !backup.consent_requested() {
        asked = ms(started);
        host_frame(&mut lobby, &mut arming, asked);
        poll_backup(&mut lobby, &mut backup, asked);
        bounded(started);
    }
    backup.answer(&mut lobby.links[0].0.lock().unwrap(), true);
    assert_eq!(backup.state(), &migration::ArmingState::AwaitingConsent);
    let state = poll_backup(&mut lobby, &mut backup, asked + window);
    assert!(
        reason(&state).contains("did not send the backup offer"),
        "{state:?}"
    );
    arming.cancel(
        &mut lobby.host,
        &mut lobby.network.0.lock().unwrap(),
        "Cancelled.",
    );
}

/// Item 5: an ACK that arrives at or after the 5 s mark is a timeout naming
/// the client, never "invalid".
#[test]
fn a_capability_ack_after_the_window_is_a_timeout_naming_the_client() {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    let mut clients = [lobby.arming_client(0), None];
    let started = Instant::now();
    let (descriptor, initial, setup_at) = loop {
        let now = ms(started);
        host_frame(&mut lobby, &mut arming, now);
        assert!(!arming.state().is_finished(), "{:?}", arming.state());
        client_frame(&mut lobby, &mut clients, now, true);
        let setup = lobby.clients[1]
            .take_migration_events()
            .into_iter()
            .find_map(|e| match e {
                migration::wire::MigrationEvent::Received(ToClientMigration::Setup {
                    descriptor,
                    initial,
                }) => Some((descriptor, initial)),
                _ => None,
            });
        if let Some((descriptor, initial)) = setup {
            break (descriptor, initial, now);
        }
        bounded(started);
    };
    let root = Descriptor::decode(&descriptor).unwrap().root;
    let player = root
        .members
        .iter()
        .find(|m| m.key == Some(lobby.identities[1].pubkey().to_bytes()))
        .unwrap()
        .player;
    let ack = CapabilityAck::sign(&root, player, initial, 1, lobby.identities[1].keys())
        .unwrap()
        .encode(&root)
        .unwrap();
    lobby.raw_migration_from(1, &ToHostMigration::Ack { ack });
    // The ACK (a valid one) is in flight; the host next reads at 6 s.
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(150) {
        client_frame(&mut lobby, &mut clients, ms(started), true);
        std::thread::sleep(Duration::from_millis(1));
    }
    host_frame(
        &mut lobby,
        &mut arming,
        setup_at + migration::ACTIVATION_WINDOW_MS + 1000,
    );
    let migration::ArmingState::Unprotected { reason } = arming.state() else {
        panic!("{:?}", arming.state())
    };
    assert!(
        reason.contains("Other") && reason.contains("within 5 s"),
        "{reason}"
    );
    assert!(!reason.contains("invalid"), "{reason}");
    assert!(!lobby.host.migration_frozen());
}

/// Item 6: a Bluetooth room refuses arming on the host, before anything is
/// sent; a device joined over Bluetooth refuses an offer and declines.
#[test]
fn bluetooth_refuses_arming_on_the_host_and_on_the_device() {
    let mut lobby = Lobby::<2>::build_room(false, false, true);
    let arming = lobby.start_arming();
    assert!(
        reason(arming.state()).contains("Bluetooth"),
        "{:?}",
        arming.state()
    );
    assert!(!lobby.host.migration_frozen());
    // The device's own check, against a host that asks and offers anyway.
    let mut clients = [lobby.arming_client(0), None];
    raw_to(&lobby, 0, &ToClientMigration::Ask);
    let started = Instant::now();
    while clients[0].as_ref().unwrap().state() != &migration::ArmingState::AwaitingConsent {
        client_frame(&mut lobby, &mut clients, ms(started), true);
        bounded(started);
    }
    raw_to(
        &lobby,
        0,
        &ToClientMigration::Offer {
            code: lobby.code(),
            descriptor: vec![0; 8],
        },
    );
    while !clients[0].as_ref().unwrap().state().is_finished() {
        client_frame(&mut lobby, &mut clients, ms(started), true);
        bounded(started);
    }
    let state = clients[0].as_ref().unwrap().state();
    assert!(reason(state).contains("Bluetooth"), "{state:?}");
    let mut answers = Vec::new();
    while !answers.contains(&ToHostMigration::Decline) {
        lobby.host.update(ms(started));
        for (peer, event) in lobby.host.take_migration_events() {
            if let migration::wire::MigrationEvent::Received(message) = event {
                assert_eq!(peer, lobby.peers[0]);
                answers.push(message);
            }
        }
        bounded(started);
    }
    assert_eq!(
        answers,
        [ToHostMigration::Willing, ToHostMigration::Decline]
    );
}

/// An offer whose root differs only in the checkpoint or game version, signed
/// by the real host: what a host on another build of the app would send.
fn offer_from_another_build(
    lobby: &Lobby,
    change: impl Fn(&mut decented_net::migration::Root),
) -> (Vec<u8>, [u8; 32]) {
    let offer = lobby.offer();
    let mut root = offer.descriptor().root.clone();
    change(&mut root);
    let descriptor = Descriptor::offer(root, lobby.original.keys()).unwrap();
    (descriptor.encode().unwrap(), offer.initial_hash().unwrap())
}

/// The builds a mixed room can hold: today's app (checkpoint schema one
/// behind), a later one, and another game protocol.
fn other_builds() -> [(&'static str, fn(&mut decented_net::migration::Root)); 3] {
    [
        ("older checkpoint", |r| r.checkpoint_version -= 1),
        ("newer checkpoint", |r| r.checkpoint_version += 1),
        ("other protocol", |r| r.game_version += 1),
    ]
}

/// Arming across builds is refused up front, and says why: the selected
/// backup declines the offer, so the room is never armed with a standby that
/// could not restore it at takeover.
#[test]
fn a_backup_on_another_build_refuses_the_offer_with_a_visible_reason() {
    for (name, change) in other_builds() {
        let mut lobby = Lobby::new();
        let (descriptor, _) = offer_from_another_build(&lobby, change);
        let mut clients = [lobby.arming_client(0), None];
        raw_to(&lobby, 0, &ToClientMigration::Ask);
        let started = Instant::now();
        while clients[0].as_ref().unwrap().state() != &migration::ArmingState::AwaitingConsent {
            client_frame(&mut lobby, &mut clients, ms(started), true);
            bounded(started);
        }
        raw_to(
            &lobby,
            0,
            &ToClientMigration::Offer {
                code: lobby.code(),
                descriptor,
            },
        );
        while !clients[0].as_ref().unwrap().state().is_finished() {
            client_frame(&mut lobby, &mut clients, ms(started), true);
            bounded(started);
        }
        let state = clients[0].as_ref().unwrap().state();
        assert!(reason(state).contains("different versions"), "{name}: {state:?}");
        assert!(clients[0].as_ref().unwrap().capability_ack().is_none());
        // The host is told at once (a decline), not left to its timeout.
        let mut answers = Vec::new();
        while !answers.contains(&ToHostMigration::Decline) {
            lobby.host.update(ms(started));
            for (_, event) in lobby.host.take_migration_events() {
                if let migration::wire::MigrationEvent::Received(message) = event {
                    answers.push(message);
                }
            }
            bounded(started);
        }
    }
}

/// Any other device in the room refuses a setup from another build too, and
/// signs nothing, so the host never collects every ACK and the room stays
/// unarmed.
#[test]
fn another_device_on_another_build_refuses_the_setup_and_signs_nothing() {
    for (name, change) in other_builds() {
        let mut lobby = Lobby::new();
        let (descriptor, initial) = offer_from_another_build(&lobby, change);
        // A setup carries the backup's consent too: the backup is on that build.
        let mut consented = Descriptor::decode(&descriptor).unwrap();
        consented.consent(lobby.identities[0].keys()).unwrap();
        let descriptor = consented.encode().unwrap();
        raw_to(&lobby, 1, &ToClientMigration::Setup { descriptor, initial });
        let mut clients = [None, lobby.arming_client(1)];
        let started = Instant::now();
        while !clients[1].as_ref().unwrap().state().is_finished() {
            client_frame(&mut lobby, &mut clients, ms(started), true);
            bounded(started);
        }
        let client = clients[1].as_ref().unwrap();
        assert!(client.capability_ack().is_none(), "{name}: signed a setup from another build");
        assert!(reason(client.state()).contains("different versions"), "{name}: {:?}", client.state());
    }
}

/// "Backup" scripted (no coordinator): yes to the Ask, then `on_offer` for
/// the offer. "Other" changes character once the lobby is frozen.
fn phase_two_failure(
    mut on_offer: impl FnMut(&Lobby, String, Vec<u8>),
) -> (Lobby, migration::HostArming, Instant) {
    let mut lobby = Lobby::new();
    let mut arming = lobby.start_arming();
    let mut clients = [None, lobby.arming_client(1)];
    let started = Instant::now();
    let mut injected = false;
    run_until_finished(
        &mut lobby,
        &mut arming,
        &mut clients,
        started,
        |lobby, arming| {
            if arming.state() == &migration::ArmingState::AwaitingConsent && !injected {
                lobby.raw_from(1, &ToHost::SetCharacter(3));
                injected = true;
            }
            for event in lobby.clients[0].take_migration_events() {
                match event {
                    migration::wire::MigrationEvent::Received(ToClientMigration::Ask) => {
                        lobby.raw_migration_from(0, &ToHostMigration::Willing)
                    }
                    migration::wire::MigrationEvent::Received(ToClientMigration::Offer {
                        code,
                        descriptor,
                    }) => on_offer(lobby, code, descriptor),
                    _ => {}
                }
            }
        },
    );
    assert!(injected);
    (lobby, arming, started)
}

/// The boundary was released: the message held meanwhile applies at the
/// next update, not before.
fn assert_released(lobby: &mut Lobby, started: Instant) {
    assert!(!lobby.host.migration_frozen());
    assert_eq!(lobby.character_of(1), 1);
    lobby.host.update(ms(started));
    assert_eq!(lobby.character_of(1), 3);
}

/// Item 6: the selected device declines in phase 2.
#[test]
fn a_decline_in_phase_two_refuses_and_releases_the_lobby() {
    let (mut lobby, arming, started) =
        phase_two_failure(|lobby, _, _| lobby.raw_migration_from(0, &ToHostMigration::Decline));
    assert!(
        reason(arming.state()).contains("could not prepare as the backup"),
        "{:?}",
        arming.state()
    );
    assert!(matches!(
        arming.state(),
        migration::ArmingState::Refused { .. }
    ));
    assert_released(&mut lobby, started);
}

/// Item 6: a valid consent whose pair listener never answers.
#[test]
fn a_pair_link_failure_in_phase_two_refuses_and_releases_the_lobby() {
    let (mut lobby, arming, started) = phase_two_failure(|lobby, code, descriptor| {
        let consent = migration::consent_lobby_offer(
            &lobby.links[0].0.lock().unwrap(),
            &lobby.identities[0],
            &code,
            &descriptor,
        )
        .unwrap();
        lobby.raw_migration_from(
            0,
            &ToHostMigration::Consent {
                descriptor: consent.encode().unwrap(),
                addr: "127.0.0.1:1".into(),
                token: [7; 32],
            },
        );
    });
    assert!(
        reason(arming.state()).contains("pair link"),
        "{:?}",
        arming.state()
    );
    assert!(matches!(
        arming.state(),
        migration::ArmingState::Refused { .. }
    ));
    assert_released(&mut lobby, started);
}

/// Item 6: cancel in phase 2 releases the lobby and withdraws the offer;
/// after Armed it does nothing, and the boundary is the caller's to end.
#[test]
fn cancel_releases_the_lobby_and_after_armed_does_nothing() {
    let mut lobby = Lobby::new();
    let started = Instant::now();
    let mut arming = frozen_with_offer(&mut lobby, started);
    arming.cancel(
        &mut lobby.host,
        &mut lobby.network.0.lock().unwrap(),
        "The host cancelled.",
    );
    assert_eq!(
        arming.state(),
        &migration::ArmingState::Refused {
            reason: "The host cancelled.".into()
        }
    );
    assert!(!lobby.host.migration_frozen());
    let mut withdrawn = false;
    while !withdrawn {
        lobby.clients[0].update(ms(started), Input::default());
        withdrawn = lobby.clients[0].take_migration_events().iter().any(|e| {
            matches!(
                e,
                migration::wire::MigrationEvent::Received(ToClientMigration::Withdrawn { reason })
                    if reason == "The host cancelled."
            )
        });
        bounded(started);
    }
    lobby.host.update(ms(started));
    let mut arming = lobby.start_arming();
    let mut clients = [lobby.arming_client(0), lobby.arming_client(1)];
    run_until_finished(
        &mut lobby,
        &mut arming,
        &mut clients,
        Instant::now(),
        |_, _| {},
    );
    assert_eq!(arming.state(), &migration::ArmingState::Armed);
    arming.cancel(
        &mut lobby.host,
        &mut lobby.network.0.lock().unwrap(),
        "Too late.",
    );
    assert_eq!(arming.state(), &migration::ArmingState::Armed);
    assert!(lobby.host.migration_frozen());
    let armed = arming.take_armed().unwrap();
    migration::thaw_lobby(&mut lobby.host, armed.held);
    assert!(!lobby.host.migration_frozen());
}

// The armed bundle and the switch (slices 6.7 and 6.8): from an armed room
// to protected play over the room's own messages. The host sends Armed with
// cert0, the standby's address and Switch; every client verifies, closes its
// ordinary link, re-dials session-bound and prepares its standby link.
// Nothing is handed across in process.

/// What a switched room's test changes.
#[derive(Clone, Copy, Default)]
struct SwitchPlan {
    /// This remote human's device ignores `Switch` (it never acts on the
    /// bundle; its ordinary link stays up).
    straggler: Option<usize>,
    /// The host forwards a standby token the standby's listener refuses, so
    /// no client can prepare its standby link.
    bad_standby: bool,
    /// A stranger joins the ordinary listener between `Armed` and the pin.
    newcomer: bool,
    /// Before the host's bundle, devices 1 and 2 get a bundle with a
    /// tampered `Armed` (1) or a tampered cert0 (2), then `Switch`.
    tamper: bool,
    /// This remote human's ordinary link drops while the lobby is still
    /// frozen, after `Armed` and before the switch starts: its `Left` waits
    /// at the frozen boundary, the frozen lobby never applies it, and the
    /// device never re-dials.
    leaver: Option<usize>,
    /// A playing phone host: the original seats its own player too (five
    /// humans, three bots). After the switch it is attached in process with
    /// `HostSwitch::attach_own`.
    phone: bool,
    /// The listener's LAN token rotates (a close and reopen) after every
    /// device joined and the room armed, just before the switch starts.
    rotate: bool,
    /// Before the host's bundle, this device gets a genuine bundle and a
    /// `Switch` whose LAN token is not the listener's.
    tamper_token: Option<usize>,
}

/// Send device `i` a genuine bundle, then a `Switch` whose token is not the
/// listener's (ahead of the host's own bundle): the device verifies, closes
/// and re-dials its own join address, and the listener refuses the token.
fn tamper_switch<const N: usize>(
    lobby: &Lobby<N>,
    armed: &migration::ArmedHost,
    standby: &migration::LiveStandby,
    i: usize,
) {
    let root = armed.descriptor.root.clone();
    let armed_record = armed
        .bootstrap
        .armed
        .encode(&root.context().unwrap())
        .unwrap();
    let certificate = standby
        .membership_certificate()
        .unwrap()
        .encode(&root)
        .unwrap();
    let mut token = [0u8; 32];
    for (k, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&lobby.lan_token_now()[2 * k..2 * k + 2], 16).unwrap();
    }
    token[0] ^= 1;
    raw_to(
        lobby,
        i,
        &ToClientMigration::Armed {
            armed: armed_record,
            certificate,
        },
    );
    raw_to(
        lobby,
        i,
        &ToClientMigration::Standby {
            addr: armed.standby.to_string(),
            token: armed.standby_token,
        },
    );
    raw_to(lobby, i, &ToClientMigration::Switch { token });
}

/// A room armed and switched over the network, as an [`ArmedRoom`].
struct Switched {
    room: ArmedRoom,
    /// Each identity's armed route (0: the standby device's own seat).
    seats: Vec<Option<migration::RemotePlayer>>,
    host: migration::SwitchState,
    /// Each device's switch result (`None`: it never switched).
    clients: Vec<Option<migration::SwitchState>>,
    /// Each device's ordinary session, if it kept it (it never switched).
    sessions: Vec<Option<ClientSession>>,
    /// From the host's `HostSwitch::start` to its finished state.
    switch_ms: u64,
    refused_joins: usize,
    /// The stranger's link was closed and never seated.
    newcomer_closed: Option<bool>,
    /// The tampered devices' refusal reasons, with their ordinary links
    /// still open and no switch begun.
    tamper_refused: Vec<(String, bool)>,
    /// A playing phone host's own seat, attached after the switch.
    host_seat: Option<migration::RemotePlayer>,
    /// The listener's LAN token at the switch was not the one the devices
    /// joined with.
    token_rotated: bool,
    /// Each device that handed back a session to rejoin the ordinary lobby.
    fell_back: Vec<bool>,
}

impl<const N: usize> Lobby<N> {
    /// The ordinary listener's loopback address and the LAN token every
    /// client joined with: the original join, which the switch re-dials
    /// session-bound (with the token from `Switch`).
    fn rejoin_at(&self) -> (std::net::SocketAddr, String) {
        let net = self.network.0.lock().unwrap();
        let h = net.handle().unwrap();
        (
            std::net::SocketAddr::from(([127, 0, 0, 1], h.local_addr().unwrap().port())),
            self.joined_token.clone(),
        )
    }
    /// The ordinary listener's LAN token now.
    fn lan_token_now(&self) -> String {
        self.network.0.lock().unwrap().handle().unwrap().lan_token()
    }
}

/// A device frame during arming and the switch: each session's native
/// update at `session_now`, then its arming coordinator at `now`.
fn switch_client_frame<const N: usize>(
    lobby: &mut Lobby<N>,
    clients: &mut [Option<migration::ClientArming>],
    now: u64,
    session_now: u64,
) {
    for (i, client) in lobby.clients.iter_mut().enumerate() {
        client.update(session_now, Input::default());
        if let Some(arming) = clients[i].as_mut() {
            let mut link = lobby.links[i].0.lock().unwrap();
            arming.poll(client, &mut link, now);
            if arming.consent_requested() {
                arming.answer(&mut link, true);
            }
        }
    }
}

/// Game time pauses from the frozen lobby to the first armed commit: the
/// armed room resumes at the frozen image's time. The devices' sessions are
/// held at that time too (their clock offsets, synced in the lobby, stay
/// right); the coordinators and runners run on the real clock.
fn switched(plan: SwitchPlan) -> Switched {
    switched_with::<4>(plan)
}

/// As [`switched`], with `N` remote humans (the first is the standby
/// device) and `8 - N` bots: `N = 8` is a full lobby, which the native host
/// closed when the eighth joined, rotating the LAN token.
fn switched_with<const N: usize>(plan: SwitchPlan) -> Switched {
    // One device clock per process: the coordinators' times and the
    // runners' samples are the same clock, as on a device.
    let clock = migration::RunnerClock::start();
    let mut lobby = Lobby::<N>::build(plan.phone);
    let rejoin = lobby.rejoin_at();
    let mut arming = lobby.start_arming();
    let mut clients: Vec<Option<migration::ClientArming>> = (0..N)
        .map(|i| {
            lobby
                .arming_client(i)
                .map(|c| c.rejoin(rejoin.0, &rejoin.1))
        })
        .collect();
    let t = Instant::now();
    let mut paused = None;
    loop {
        let now = clock.now_ms();
        host_frame(&mut lobby, &mut arming, now);
        if lobby.host.migration_frozen() {
            paused.get_or_insert(now);
        }
        switch_client_frame(&mut lobby, &mut clients, now, paused.unwrap_or(now));
        if let Some(own) = lobby.host_client.as_mut() {
            own.update(paused.unwrap_or(now), Input::default());
        }
        let standby_done = clients[0].as_ref().unwrap().state().is_finished();
        if arming.state().is_finished() && standby_done {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "{:?}",
            arming.state()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(arming.state(), &migration::ArmingState::Armed);
    let paused = paused.unwrap();
    let mut armed = arming.take_armed().unwrap();
    // The standby device's app: its runtime from its own bootstrap and the
    // descriptor it consented to.
    let descriptor = clients[0].as_ref().unwrap().descriptor().unwrap();
    let boot = clients[0].as_mut().unwrap().take_standby().unwrap();
    let mut standby =
        migration::LiveStandby::new(descriptor, boot, lobby.identities[0].clone(), clock).unwrap();
    if plan.bad_standby {
        armed.standby_token[0] ^= 1;
    }
    let mut tamper_refused = Vec::new();
    if plan.tamper {
        let root = armed.descriptor.root.clone();
        let mut bad_armed = armed
            .bootstrap
            .armed
            .encode(&root.context().unwrap())
            .unwrap();
        let mut bad_cert = standby
            .membership_certificate()
            .unwrap()
            .encode(&root)
            .unwrap();
        let (armed_ok, cert_ok) = (bad_armed.clone(), bad_cert.clone());
        *bad_armed.last_mut().unwrap() ^= 1;
        *bad_cert.last_mut().unwrap() ^= 1;
        let standby_at = ToClientMigration::Standby {
            addr: armed.standby.to_string(),
            token: armed.standby_token,
        };
        for (i, armed, certificate) in [(1, bad_armed, cert_ok), (2, armed_ok, bad_cert)] {
            raw_to(&lobby, i, &ToClientMigration::Armed { armed, certificate });
            raw_to(&lobby, i, &standby_at);
            // Refused before its token is ever used.
            raw_to(&lobby, i, &ToClientMigration::Switch { token: [0; 32] });
        }
        let t = Instant::now();
        while [1, 2].iter().any(|&i| {
            !matches!(
                clients[i].as_ref().unwrap().state(),
                migration::ArmingState::Unprotected { .. }
            )
        }) {
            switch_client_frame(&mut lobby, &mut clients, clock.now_ms(), paused);
            standby.poll().unwrap();
            bounded(t);
        }
        for i in [1, 2] {
            let arming = clients[i].as_mut().unwrap();
            let migration::ArmingState::Unprotected { reason } = arming.state().clone() else {
                unreachable!()
            };
            // Refused before anything was closed or dialled: the ordinary
            // link is up and there is no switch to take.
            let open = lobby.links[i].0.lock().unwrap().migration_keys().is_ok();
            let spare = spare_client();
            let no_switch = arming.take_switch(spare, None).is_err();
            assert!(
                open && no_switch,
                "device {i}: a refused bundle closed the link or began a switch ({reason})"
            );
            tamper_refused.push((reason, open && no_switch));
        }
    }
    let newcomer = plan.newcomer.then(|| {
        let stranger = approved();
        let mut link = QuicClient::new(decented_net::join_addr(
            &lobby.cfg, &stranger, rejoin.0, &rejoin.1,
        ));
        let t = Instant::now();
        loop {
            match link.poll() {
                Some(ClientNetEvent::Connected { .. }) => break link,
                Some(ClientNetEvent::Progress(_)) | None => {}
                other => panic!("the newcomer was refused before the switch: {other:?}"),
            }
            bounded(t);
        }
    });
    if let Some(i) = plan.leaver {
        assert!(lobby.host.migration_frozen());
        lobby.links[i].0.lock().unwrap().close();
    }
    if plan.rotate {
        // A native close and reopen after everyone joined: each change
        // rotates the listener's LAN token.
        let before = lobby.lan_token_now();
        {
            let net = lobby.network.0.lock().unwrap();
            let h = net.handle().unwrap();
            h.set_open(false);
            h.set_open(true);
        }
        assert_ne!(
            lobby.lan_token_now(),
            before,
            "the LAN token did not rotate"
        );
    }
    if let Some(i) = plan.tamper_token {
        tamper_switch(&lobby, &armed, &standby, i);
    }
    let token_rotated = lobby.lan_token_now() != rejoin.1;
    let Lobby {
        cfg,
        original,
        identities,
        network,
        links,
        mut host,
        clients: sessions,
        host_client,
        ..
    } = lobby;
    let mut host_client = host_client;
    let mut sessions: Vec<Option<ClientSession>> = sessions.into_iter().map(Some).collect();
    let mut switches: Vec<Option<migration::ClientSwitch>> = (0..N).map(|_| None).collect();
    let mut seats: Vec<Option<migration::RemotePlayer>> = (0..N).map(|_| None).collect();
    let started = Instant::now();
    let mut switch = migration::HostSwitch::start(
        &mut host,
        &mut network.0.lock().unwrap(),
        armed,
        &original,
        clock,
        clock.now_ms(),
    );
    let mut switch_ms = None;
    let mut newcomer_closed = newcomer.as_ref().map(|_| false);
    let mut newcomer = newcomer;
    loop {
        let now = clock.now_ms();
        if switch_ms.is_none() {
            switch.poll(&mut host, &mut network.0.lock().unwrap(), now);
            if switch.state().is_finished() {
                switch_ms = Some(started.elapsed().as_millis() as u64);
            }
        }
        standby.poll().unwrap();
        if let Some(own) = host_client.as_mut() {
            own.update(paused, Input::default());
        }
        for i in 0..N {
            if plan.leaver == Some(i) {
                continue;
            }
            if let Some(session) = sessions[i].as_mut() {
                session.update(paused, Input::default());
                if plan.straggler != Some(i) {
                    let arming = clients[i].as_mut().unwrap();
                    arming.poll(session, &mut links[i].0.lock().unwrap(), now);
                }
            }
            let arming = clients[i].as_mut().unwrap();
            if arming.state() == &migration::ArmingState::Switching && sessions[i].is_some() {
                let session = sessions[i].take().unwrap();
                let own = (i == 0).then_some(&standby);
                match arming.take_switch(session, own) {
                    Ok(started) => switches[i] = Some(started),
                    Err(session) => sessions[i] = Some(session),
                }
            }
            if let Some(device) = switches[i].as_mut() {
                if !device.state().is_finished() {
                    device.poll();
                }
                if device.state().is_finished() && seats[i].is_none() {
                    seats[i] = device.take_player();
                }
            }
        }
        if let Some(link) = newcomer.as_mut() {
            while let Some(event) = link.poll() {
                if matches!(
                    event,
                    ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
                ) {
                    newcomer_closed = Some(true);
                }
            }
        }
        let devices_done = (0..N).all(|i| {
            plan.straggler == Some(i)
                || plan.leaver == Some(i)
                || switches[i]
                    .as_ref()
                    .is_some_and(|s| s.state().is_finished())
        });
        if switch_ms.is_some() && devices_done {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "switch stalled: host {:?} devices {:?}",
            switch.state(),
            switches
                .iter()
                .map(|s| s.as_ref().map(|s| s.state().clone()))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    if let Some(link) = newcomer.as_mut() {
        let t = Instant::now();
        while newcomer_closed == Some(false) && t.elapsed() < Duration::from_secs(3) {
            while let Some(event) = link.poll() {
                if matches!(
                    event,
                    ClientNetEvent::Failed(_) | ClientNetEvent::Disconnected { .. }
                ) {
                    newcomer_closed = Some(true);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let host_state = switch.state().clone();
    let refused_joins = switch.refused_joins();
    let pin_refusals = switch.pin_refusals().to_vec();
    // A playing host's own seat joins the armed room in process, before the
    // first armed commit.
    let host_seat = host_client.map(|client| {
        switch
            .attach_own(client, original.keys().clone())
            .unwrap_or_else(|e| panic!("the host's own seat was refused ({e}): {host_state:?}"))
    });
    let (live, players) = switch
        .take_room()
        .unwrap_or_else(|| panic!("no armed room: the host is {host_state:?}"));
    // The lobby authority is superseded by the armed runtime.
    drop(host);
    drop(network);
    let descriptor = switch.descriptor().clone();
    let cert = standby.membership_certificate().unwrap();
    let initial = state_hash(standby.image()).unwrap();
    let base = SessionImage::decode(&descriptor.root, live.primary().committed_image())
        .unwrap()
        .logical_ms;
    assert_eq!(base, paused, "the armed image is the frozen lobby's");
    let client_states = switches
        .iter()
        .map(|s| s.as_ref().map(|s| s.state().clone()))
        .collect();
    let fell_back = switches
        .iter_mut()
        .map(|s| s.as_mut().is_some_and(|s| s.take_client().is_some()))
        .collect();
    let room = ArmedRoom {
        pin_refusals,
        cfg,
        original,
        host_client: None,
        addr: rejoin.0,
        token: rejoin.1,
        descriptor,
        identities: identities.into(),
        clients: (0..N).map(|_| None).collect(),
        networks: (0..N).map(|_| None).collect(),
        live,
        standby,
        players,
        cert,
        initial,
        clock,
        base,
        start_at: 1,
    };
    Switched {
        room,
        seats,
        host: host_state,
        clients: client_states,
        sessions,
        switch_ms: switch_ms.unwrap(),
        refused_joins,
        newcomer_closed,
        tamper_refused,
        host_seat,
        token_rotated,
        fell_back,
    }
}

fn protected_list(state: &migration::SwitchState) -> Vec<u64> {
    match state {
        migration::SwitchState::Protected { unprotected } => {
            unprotected.iter().map(|u| u.player).collect()
        }
        other => panic!("the host did not reach Protected: {other:?}"),
    }
}

/// (a) and (b): the switched room plays a few armed commits, then the
/// original hangs (never polled again, nothing closed). Every survivor
/// resumes on the promoted standby: the `N` protected remote humans and the
/// standby device's own seat. With `straggler`, remote human 3 ignored
/// `Switch`: the host reported it unprotected, and it departs at takeover.
/// With `phone`, the original also plays: its own seat (attached in process
/// after the switch) plays the armed commits and hangs with its device, and
/// its player departs at takeover.
fn network_takeover<const N: usize>(straggler: bool, phone: bool) {
    let plan = SwitchPlan {
        straggler: straggler.then_some(3),
        phone,
        ..SwitchPlan::default()
    };
    let mut s = switched(plan);
    let mut room = s.room;
    let root = room.root().clone();
    let absent = straggler.then(|| room.player(3));
    // The host's report: only the straggler is unprotected.
    let listed = protected_list(&s.host);
    assert_eq!(
        listed,
        absent.into_iter().collect::<Vec<_>>(),
        "{:?}",
        s.host
    );
    if let migration::SwitchState::Protected { unprotected } = &s.host {
        for u in unprotected {
            assert!(u.reason.contains("did not switch"), "{}", u.reason);
        }
    }
    // Every switching device is protected; the remote humans hold a
    // prepared standby link, the standby device's seat its route.
    for i in 0..=N {
        assert_eq!(
            s.clients[i],
            Some(migration::SwitchState::Protected {
                unprotected: Vec::new()
            }),
            "device {i}"
        );
    }
    let mut told = None;
    if straggler {
        assert!(s.seats[3].is_none() && s.clients[3].is_none());
        // It was told while its ordinary link still existed.
        let session = s.sessions[3].as_mut().unwrap();
        let t = Instant::now();
        while told.is_none() && t.elapsed() < Duration::from_secs(2) {
            session.update(room.wall(), Input::default());
            told = session
                .take_migration_events()
                .into_iter()
                .find_map(|e| match e {
                    migration::wire::MigrationEvent::Received(ToClientMigration::Unprotected {
                        reason,
                    }) => Some(reason),
                    _ => None,
                });
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(told.is_some(), "the straggler was not told");
    }
    let code = room.players.network().code().unwrap().to_owned();
    let mut own = s.seats[0].take().unwrap();
    assert!(!own.protected(), "the standby's own seat dials no standby");
    let mut host_seat = s.host_seat.take();
    assert_eq!(host_seat.is_some(), phone);
    let host_player = phone.then(|| {
        root.members
            .iter()
            .find(|m| m.key == Some(root.host))
            .unwrap()
            .player
    });
    let mut remotes: [Option<migration::RemotePlayer>; N] =
        std::array::from_fn(|r| s.seats[1 + r].take());
    assert!(remotes.iter().all(|r| r.as_ref().unwrap().protected()));
    let ids: [u64; N] = std::array::from_fn(|r| room.player(1 + r));
    let own_player = room.player(0);
    // A few armed commits: the race starts and every seat's clock syncs.
    let (mut index, mut now) = (0u64, room.base);
    let mut racing = 0u32;
    loop {
        index += 1;
        now += STEP;
        let wall = room.wall();
        let f = own.frame(now, wall, remote_input(index, 0));
        assert!(f.is_ok(), "own seat frame: {f:?}");
        if let Some(seat) = host_seat.as_mut() {
            let f = seat.frame(now, wall, remote_input(index, 1));
            assert!(f.is_ok(), "host seat frame: {f:?}");
        }
        for (r, remote) in remotes.iter_mut().enumerate() {
            let f = remote
                .as_mut()
                .unwrap()
                .frame(now, wall, remote_input(index, r % 2));
            assert!(f.is_ok(), "remote {r} frame: {f:?}");
        }
        room.commit_at(index, now, None);
        racing += u32::from(
            room.live
                .authority()
                .world()
                .is_some_and(|w| w.phase == RacePhase::Racing),
        );
        let synced = own.client().clock_offset_ms().is_some()
            && host_seat
                .as_ref()
                .is_none_or(|h| h.client().clock_offset_ms().is_some())
            && remotes
                .iter()
                .all(|r| r.as_ref().unwrap().client().clock_offset_ms().is_some());
        if racing >= 20 && synced {
            break;
        }
        assert!(index < 1000, "the armed race never ran");
    }
    assert!(room.native_equals_standby());
    let commits = index;
    // The original hangs: its runtime and player host are never polled
    // again, and nothing is closed.
    let drop_local = now + STEP;
    let w_drop = room.wall();
    let drop_at = Instant::now();
    let c_session = SessionImage::decode(&root, room.standby.image()).unwrap();
    let c_logical = room.standby.receipt().logical_ms;
    let handled_c: [u64; N] = std::array::from_fn(|r| c_session.inputs[&ids[r]].handled());
    let ArmedRoom {
        cfg,
        descriptor,
        identities,
        live,
        players,
        standby,
        clock,
        ..
    } = room;
    // A playing host's own seat hangs with its device.
    let hung = (live, players, host_seat);
    let wall = || clock.now_ms();
    let local = |w: u64| drop_local + w.saturating_sub(w_drop);
    let mut devices = SurvivorDevices::<N> {
        waiting: remotes,
        recovering: std::array::from_fn(|_| None),
        last_frame: [drop_local; N],
        freeze: [None; N],
        frames: [0; N],
        frozen_by: [None; N],
        tail: [None; N],
        pings: std::array::from_fn(|_| Vec::new()),
        resumed_local: [None; N],
        applied: [0; N],
        duplicates: [0; N],
        discarded: 0,
    };
    let mut standby = Some(standby);
    let mut own_last = drop_local;
    let (promoted, at) = loop {
        let w = wall();
        devices.step(w, local(w), [false; N], handled_c);
        let l = local(w);
        if own.frozen_at().is_none() && l > own_last {
            let _ = own.frame(l, w, remote_input(20_000, 0)).unwrap();
            own_last = l;
        }
        match standby.take().unwrap().promote() {
            Ok(promoted) => {
                let at = promoted.promoted_at_ms();
                break (promoted, at);
            }
            Err(migration::PromoteRefused {
                error: Error::Stale,
                standby: Some(back),
            }) => standby = Some(*back),
            Err(refused) => panic!("promotion refused: {refused:?}"),
        }
        assert!(
            drop_at.elapsed() < Duration::from_secs(2),
            "the watch never expired"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let watch_ms = at - w_drop;
    let own_frozen = own.frozen_at().unwrap_or(local(at));
    let keys = identities[0].keys().clone();
    let (mut links, mut recovery) = migration::SurvivorLinks::open(promoted.into_link()).unwrap();
    let mut routed: Vec<u64> = links.players().collect();
    routed.sort();
    let mut prepared = ids.to_vec();
    prepared.sort();
    assert_eq!(routed, prepared, "every prepared link is routed");
    let (own_client, own_delivery, own_intents) = own.into_parts().unwrap();
    let mut own = migration::OwnSeat::new(
        &root,
        recovery.sequencer().image(),
        own_client,
        own_delivery,
        own_intents,
    )
    .unwrap();
    let _successor_host = migration::start_resume_host(
        &cfg,
        &identities[0],
        "Resumed",
        &code,
        descriptor.clone(),
        recovery.promotion().clone(),
    )
    .unwrap();
    let failure = loop {
        let w = wall();
        devices.step(w, local(w), [false; N], handled_c);
        let phase = match links.drive(&mut recovery, Some(&mut own), &keys, local(w), w) {
            Ok(phase) => phase,
            Err(error) => break Some(error),
        };
        if phase == RecoveryPhase::Ready {
            break None;
        }
        assert!(wall() - at < 5000, "recovery stalled in {phase:?}");
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(failure.is_none(), "recovery failed: {failure:?}");
    drop(hung);
    let mut barrier = links.barrier(recovery, &keys, wall()).unwrap();
    let own_delta = own.ready(&mut barrier, own_frozen, local(wall()), wall());
    assert!(own_delta.is_ok(), "own seat cut refused: {own_delta:?}");
    let t = Instant::now();
    loop {
        let w = wall();
        devices.step(w, local(w), [false; N], handled_c);
        if links.drive_barrier(&mut barrier, wall()).unwrap() {
            break;
        }
        bounded(t);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (successor, _resume) = links.resume(barrier, &keys, wall()).unwrap();
    let own_resume_local = local(wall());
    own.resumed(own_resume_local).unwrap();
    let t = Instant::now();
    while devices.resumed_local.iter().any(Option::is_none) {
        let w = wall();
        devices.step(w, local(w), [false; N], handled_c);
        bounded(t);
    }
    let resumed_ms = drop_at.elapsed().as_millis();
    let per_survivor: Vec<u64> = devices
        .resumed_local
        .iter()
        .map(|l| l.unwrap() - drop_local)
        .collect();
    let own_after = own_resume_local - drop_local;
    let image = SessionImage::decode(&root, successor.image()).unwrap();
    assert_eq!(successor.receipt().term, 1);
    assert_eq!(image.logical_ms, c_logical);
    assert!(image.members.iter().any(|m| m.player == own_player));
    for p in ids {
        assert!(image.members.iter().any(|m| m.player == p), "survivor {p}");
    }
    let departed = absent.map(|p| {
        !image.members.iter().any(|m| m.player == p)
            && successor
                .receipt()
                .watermarks
                .iter()
                .any(|m| m.player == p && m.retired)
    });
    if straggler {
        assert_eq!(departed, Some(true), "the straggler did not depart");
    }
    let host_departed = host_player.map(|p| {
        !image.members.iter().any(|m| m.player == p)
            && successor
                .receipt()
                .watermarks
                .iter()
                .any(|m| m.player == p && m.retired)
            && successor
                .primary()
                .outbox()
                .all(|r| r.output.recipient != p)
    });
    if phone {
        assert_eq!(
            host_departed,
            Some(true),
            "the phone host's player did not depart"
        );
    }
    for (r, d) in devices.duplicates.iter().enumerate() {
        assert_eq!(*d, 0, "survivor {r} applied a replayed output twice");
    }
    println!(
        "STAMPEDE NETWORK TAKEOVER straggler={straggler} phone_host={phone} host_departed={host_departed:?} switch_ms={} host_unprotected={listed:?} straggler_told={told:?} protected_remotes={N} armed_commits={commits} watch_ms={watch_ms} resume_after_drop_local_ms={per_survivor:?} own_seat_resume_ms={own_after} all_resumed_ms={resumed_ms} straggler_departed={departed:?} term={}",
        s.switch_ms,
        successor.receipt().term
    );
}

/// (a) The first fully network-built takeover: arming, the bundle and the
/// switch run over the room's messages; every client is protected; armed
/// commits; the original hangs; every survivor resumes on the standby.
#[test]
fn a_network_built_room_switches_protected_and_every_survivor_resumes_at_takeover() {
    network_takeover::<3>(false, false);
}

/// (b) A client that ignores `Switch` is reported unprotected to the host,
/// told so while its link still exists, and departs at takeover while the
/// others survive.
#[test]
fn a_client_that_ignores_the_switch_is_reported_unprotected_and_departs_at_takeover() {
    network_takeover::<2>(true, false);
}

/// Item 4: a playing phone host through the switch. The host's own seat is
/// not a roster human (no bundle, not listed); after the switch it is
/// attached in process (`HostSwitch::attach_own`, with the ACK it signed at
/// arming) and plays the armed commits beside the standby device's own seat
/// and three protected remote humans. Then the phone host is the one killed
/// (it hangs, its seat with it): the standby promotes, the remote survivors
/// and the standby's own seat resume, and the host's player departs (not a
/// member, its watermark retired, nothing kept for it in the outbox).
#[test]
fn a_playing_phone_host_switches_armed_and_its_player_departs_when_it_dies() {
    network_takeover::<3>(false, true);
}

/// Item 1: a client whose ordinary link drops while the lobby is still
/// frozen for the switch (after `Armed`, before the bundle: its `Left` waits
/// at the frozen boundary and the frozen lobby never applies it) never
/// attaches a route. The host lists it unprotected after the 5 s route
/// window, and it was parked at the pin on the runner clock. The room plays
/// on armed without pausing and departs it a little over 30 s after the pin:
/// the native kick keeps it a seatless member for one batch, and the next
/// batch retires it. Real time: about 36 s.
#[test]
fn a_client_gone_during_the_freeze_is_parked_at_the_pin_and_departs_after_thirty_seconds() {
    use migration::PARKED_DEPARTURE_MS;
    let mut s = switched(SwitchPlan {
        leaver: Some(3),
        ..SwitchPlan::default()
    });
    let gone = s.room.player(3);
    assert_eq!(protected_list(&s.host), vec![gone], "{:?}", s.host);
    let reason = match &s.host {
        migration::SwitchState::Protected { unprotected } => unprotected[0].reason.clone(),
        _ => unreachable!(),
    };
    assert!(reason.contains("did not reconnect"), "{reason}");
    assert!(s.seats[3].is_none() && s.clients[3].is_none());
    let mut seats: Vec<migration::RemotePlayer> =
        (0..3).map(|i| s.seats[i].take().unwrap()).collect();
    let mut room = s.room;
    room.start_at = u64::MAX;
    let parked_at = room
        .live
        .parked_at(gone)
        .expect("the never-attached client was not parked at the pin");
    for i in 0..3 {
        assert_eq!(room.live.parked_at(room.player(i)), None, "seat {i} parked");
    }
    let humans = room.live.authority().human_count();
    let revision = room.live.membership_certificate().unwrap().revision;
    let mark = |room: &ArmedRoom, player: u64| {
        room.live
            .primary()
            .receipt()
            .watermarks
            .iter()
            .find(|m| m.player == player)
            .cloned()
            .unwrap()
    };
    let (mut index, mut native_equal, mut rounds) = (0, true, 0);
    // While it holds its seat, no batch was staged with it parked for
    // longer than 30 s.
    let departed_ms = loop {
        let before = room.wall();
        let mut playing: Vec<&mut migration::RemotePlayer> = seats.iter_mut().collect();
        native_equal &= armed_round(&mut room, &mut playing, &mut index);
        if room.live.authority().human_count() < humans {
            break room.wall() - parked_at;
        }
        assert!(
            before - parked_at <= PARKED_DEPARTURE_MS,
            "the never-attached client was still seated {} ms after the pin",
            before - parked_at
        );
        rounds += 1;
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        departed_ms > PARKED_DEPARTURE_MS,
        "it departed {departed_ms} ms after the pin"
    );
    let kicked = kicked_outputs(&room, gone);
    let kept = room.image().members.iter().any(|m| m.player == gone)
        && !mark(&room, gone).retired
        && room.live.membership_certificate().unwrap().revision == revision
        && kicked == 1;
    assert!(
        kept,
        "the departure batch did not keep it seatless (kicked={kicked})"
    );
    let mut playing: Vec<&mut migration::RemotePlayer> = seats.iter_mut().collect();
    native_equal &= armed_round(&mut room, &mut playing, &mut index);
    let cert = room.live.membership_certificate().unwrap().clone();
    let retired = mark(&room, gone).retired
        && !room.image().members.iter().any(|m| m.player == gone)
        && cert.revision == revision + 1
        && !cert.members.iter().any(|m| m.player == gone)
        && room.outbox(gone).is_empty()
        && room.live.parked_at(gone).is_none();
    assert!(retired, "the next batch did not retire it");
    for i in 0..3 {
        assert!(room.players.attached(room.player(i)), "seat {i} detached");
    }
    assert!(native_equal);
    assert!(room.live.failure().is_none() && room.standby.failure().is_none());
    println!(
        "STAMPEDE SWITCH LEAVER host_listed={:?} reason={reason:?} parked_at_pin=true departed_after_pin_ms={departed_ms} seated_rounds={rounds} kicked_outputs={kicked} revision={revision}->{} retired={retired} commits={index} switch_ms={}",
        [gone],
        cert.revision,
        s.switch_ms
    );
}

/// (c) No client can prepare its standby link (the standby's listener
/// refuses the token the host forwarded): each reports Unprotected with the
/// reason, keeps its route, and the room proceeds with armed commits. The
/// host cannot see this in v0.1 and lists nobody (the documented gap).
#[test]
fn a_client_whose_standby_link_fails_reports_unprotected_and_the_room_proceeds() {
    let mut s = switched(SwitchPlan {
        bad_standby: true,
        ..SwitchPlan::default()
    });
    assert_eq!(protected_list(&s.host), Vec::<u64>::new(), "the known gap");
    assert_eq!(
        s.clients[0],
        Some(migration::SwitchState::Protected {
            unprotected: Vec::new()
        })
    );
    let mut reasons = Vec::new();
    for i in 1..4 {
        match &s.clients[i] {
            Some(migration::SwitchState::Unprotected { reason }) => {
                assert!(reason.contains("standby link"), "{reason}");
                reasons.push(reason.clone());
            }
            other => panic!("device {i}: {other:?}"),
        }
        let seat = s.seats[i].as_ref().expect("the route survives");
        assert!(!seat.protected());
    }
    let mut seats: Vec<migration::RemotePlayer> = s
        .seats
        .iter_mut()
        .map(|seat| seat.take().unwrap())
        .collect();
    let room = &mut s.room;
    let mut now = room.base;
    for index in 1..=10 {
        now += STEP;
        let wall = room.wall();
        for (r, seat) in seats.iter_mut().enumerate() {
            seat.frame(now, wall, remote_input(index, r % 2)).unwrap();
        }
        room.commit_at(index, now, None);
    }
    assert_eq!(room.standby.receipt().index, 10);
    assert!(room.native_equals_standby());
    println!(
        "STAMPEDE SWITCH STANDBY LINK FAILED clients_unprotected={} host_listed=0 commits_after=10 reason={:?}",
        reasons.len(),
        reasons[0]
    );
}

/// (d) A stranger joining the ordinary listener between `Armed` and the pin
/// is closed and never seated in the armed room; everyone else switches.
#[test]
fn a_peer_joining_between_armed_and_the_pin_is_not_seated_armed() {
    let s = switched(SwitchPlan {
        newcomer: true,
        ..SwitchPlan::default()
    });
    assert_eq!(protected_list(&s.host), Vec::<u64>::new());
    assert!(s.refused_joins >= 1, "the stranger was not closed");
    assert_eq!(s.newcomer_closed, Some(true));
    let room = &s.room;
    assert_eq!(room.live.authority().human_count(), 4);
    // The armed roster is the four humans who were seated at `Armed`.
    let keys: Vec<[u8; 32]> = room
        .identities
        .iter()
        .map(|id| id.pubkey().to_bytes())
        .collect();
    assert_eq!(room.root().members.len(), 4);
    assert!(room
        .root()
        .members
        .iter()
        .all(|m| m.key.is_some_and(|k| keys.contains(&k))));
    let attached: Vec<u64> = room.players.players().collect();
    assert_eq!(attached.len(), 4, "{attached:?}");
    println!(
        "STAMPEDE SWITCH NEWCOMER refused_joins={} newcomer_closed=true armed_humans={} routes={} switch_ms={}",
        s.refused_joins,
        room.live.authority().human_count(),
        attached.len(),
        s.switch_ms
    );
}

/// A full lobby: the native host closed the room when the eighth human
/// joined, which rotated the LAN token every device joined with. `Switch`
/// carries the listener's current token, so all eight re-dial their own join
/// address with it and the room switches protected.
#[test]
fn a_full_lobby_of_eight_humans_switches_protected_although_its_token_rotated() {
    let s = switched_with::<8>(SwitchPlan::default());
    assert!(
        s.token_rotated,
        "the full lobby did not rotate its LAN token"
    );
    assert_eq!(protected_list(&s.host), Vec::<u64>::new());
    for (i, state) in s.clients.iter().enumerate() {
        assert_eq!(
            state,
            &Some(migration::SwitchState::Protected {
                unprotected: Vec::new()
            }),
            "device {i}"
        );
        assert!(s.seats[i].is_some(), "device {i} has no armed route");
    }
    let room = &s.room;
    assert_eq!(room.live.authority().human_count(), 8);
    assert_eq!(room.root().members.len(), 8);
    let attached: Vec<u64> = room.players.players().collect();
    assert_eq!(attached.len(), 8, "{attached:?}");
    println!(
        "STAMPEDE SWITCH FULL LOBBY humans=8 token_rotated=true routes={} switch_ms={}",
        attached.len(),
        s.switch_ms
    );
}

/// The LAN token rotated after every device joined (a native close and
/// reopen), just before the switch: the re-dials still land, with the token
/// from `Switch`.
#[test]
fn a_token_rotated_after_the_join_still_re_dials_protected() {
    let s = switched(SwitchPlan {
        rotate: true,
        ..SwitchPlan::default()
    });
    assert!(s.token_rotated);
    assert_eq!(protected_list(&s.host), Vec::<u64>::new());
    for (i, state) in s.clients.iter().enumerate() {
        assert_eq!(
            state,
            &Some(migration::SwitchState::Protected {
                unprotected: Vec::new()
            }),
            "device {i}"
        );
    }
    assert_eq!(s.room.players.players().count(), 4);
    println!(
        "STAMPEDE SWITCH ROTATED TOKEN routes=4 switch_ms={}",
        s.switch_ms
    );
}

/// A `Switch` whose token is not the listener's: the device re-dials its own
/// join address, the listener refuses the token, and the device is
/// unprotected and falls back to an ordinary rejoin. It never gets an armed
/// route; the host lists it, and everyone else switches protected.
#[test]
fn a_tampered_switch_token_fails_the_re_dial_and_the_device_falls_back() {
    let s = switched(SwitchPlan {
        tamper_token: Some(2),
        ..SwitchPlan::default()
    });
    let player = s.room.player(2);
    assert_eq!(protected_list(&s.host), vec![player]);
    let reason = match &s.clients[2] {
        Some(migration::SwitchState::Unprotected { reason }) => reason.clone(),
        other => panic!("device 2: {other:?}"),
    };
    assert!(reason.contains("could not be rejoined"), "{reason}");
    assert!(reason.contains("Rejoining the lobby unarmed"), "{reason}");
    assert!(s.seats[2].is_none(), "device 2 got an armed route");
    assert_eq!(s.fell_back, vec![false, false, true, false]);
    for i in [0, 1, 3] {
        assert_eq!(
            s.clients[i],
            Some(migration::SwitchState::Protected {
                unprotected: Vec::new()
            }),
            "device {i}"
        );
    }
    let attached: Vec<u64> = s.room.players.players().collect();
    assert_eq!(attached.len(), 3, "{attached:?}");
    assert!(!attached.contains(&player));
    println!("STAMPEDE SWITCH TAMPERED TOKEN unprotected={player} reason={reason:?}");
}

/// A room armed over the network and ready for `HostSwitch::start`: the
/// lobby (still frozen), every device's arming coordinator, the armed host
/// and the standby device's running `LiveStandby`. With `ready_during_freeze`
/// that device sets ready while the lobby is frozen: on its ordered link the
/// toggle is ahead of its ACK, which activation waits for, so it is held at
/// the frozen boundary.
struct ReadyToSwitch<const N: usize = 4> {
    lobby: Lobby<N>,
    clients: Vec<Option<migration::ClientArming>>,
    armed: migration::ArmedHost,
    standby: migration::LiveStandby,
    clock: migration::RunnerClock,
    paused: u64,
}

/// `N` remote humans: `N = 8` is a full lobby on a dedicated host.
fn ready_to_switch<const N: usize>(ready_during_freeze: Option<usize>) -> ReadyToSwitch<N> {
    let clock = migration::RunnerClock::start();
    let mut lobby = Lobby::<N>::build(false);
    let rejoin = lobby.rejoin_at();
    let mut arming = lobby.start_arming();
    let mut clients: Vec<Option<migration::ClientArming>> = (0..N)
        .map(|i| {
            lobby
                .arming_client(i)
                .map(|c| c.rejoin(rejoin.0, &rejoin.1))
        })
        .collect();
    let t = Instant::now();
    let mut paused = None;
    loop {
        let now = clock.now_ms();
        host_frame(&mut lobby, &mut arming, now);
        if lobby.host.migration_frozen() && paused.is_none() {
            paused = Some(now);
            if let Some(i) = ready_during_freeze {
                lobby.clients[i].set_ready(true);
            }
        }
        switch_client_frame(&mut lobby, &mut clients, now, paused.unwrap_or(now));
        let standby_done = clients[0].as_ref().unwrap().state().is_finished();
        if arming.state().is_finished() && standby_done {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "{:?}",
            arming.state()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(arming.state(), &migration::ArmingState::Armed);
    let armed = arming.take_armed().unwrap();
    if let Some(i) = ready_during_freeze {
        let peer = lobby.peers[i];
        assert!(
            armed
                .held
                .iter()
                .any(|e| matches!(e, HostNetEvent::Reliable { peer: p, .. } if *p == peer)),
            "the ready toggle was not held at the frozen boundary"
        );
    }
    let descriptor = clients[0].as_ref().unwrap().descriptor().unwrap();
    let boot = clients[0].as_mut().unwrap().take_standby().unwrap();
    let standby =
        migration::LiveStandby::new(descriptor, boot, lobby.identities[0].clone(), clock).unwrap();
    ReadyToSwitch {
        lobby,
        clients,
        armed,
        standby,
        clock,
        paused: paused.unwrap(),
    }
}

/// How a switch is made to fail in the tests below.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SwitchFailure {
    /// The standby's pair link drops in the leave window, before the pin.
    StandbyBeforePin,
    /// Every pin attempt is refused (the test hook), so the host gives up at
    /// the pin deadline.
    PinRefused,
    /// The standby's pair link drops right after the pin.
    StandbyAfterPin,
}

/// What a failed switch left behind.
struct FailedSwitch {
    switch: migration::HostSwitch,
    host: HostAuthority,
    switches: Vec<Option<migration::ClientSwitch>>,
    /// Each switched device's session from `take_client`, if any.
    rejoined: Vec<Option<ClientSession>>,
    /// The devices that never switched kept their ordinary sessions.
    sessions: Vec<Option<ClientSession>>,
    clock: migration::RunnerClock,
    /// The room's listener.
    network: HostLink,
    /// The listener's LAN token when the switch started (the one `Switch`
    /// carried), and whether its room was open then.
    switch_token: String,
    open_at_switch: bool,
    /// When the host gave the lobby back, on the runner clock.
    finished_at: u64,
}

/// Drive a switch that fails as `failure` says, with device `straggler`
/// ignoring `Switch` (and setting ready during the freeze), until the host is
/// finished and every switched device has its outcome: before the pin, until
/// the three that switched are back in the thawed lobby.
fn failed_switch(failure: SwitchFailure, straggler: Option<usize>) -> FailedSwitch {
    failed_switch_with(failure, straggler, false, None)
}

/// As [`failed_switch`]; with `rotate`, the listener's LAN token rotates (a
/// native close and reopen) after the room armed, just before the switch, so
/// the token every device joined with no longer admits it. With `tamper`,
/// that device gets a `Switch` whose token the listener refuses.
fn failed_switch_with(
    failure: SwitchFailure,
    straggler: Option<usize>,
    rotate: bool,
    tamper: Option<usize>,
) -> FailedSwitch {
    failed_switch_n::<4>(failure, straggler, rotate, tamper)
}

/// As [`failed_switch_with`], with `N` remote humans (`N = 8` is a full
/// lobby on a dedicated host).
fn failed_switch_n<const N: usize>(
    failure: SwitchFailure,
    straggler: Option<usize>,
    rotate: bool,
    tamper: Option<usize>,
) -> FailedSwitch {
    let ReadyToSwitch {
        lobby,
        mut clients,
        armed,
        standby,
        clock,
        paused,
    } = ready_to_switch::<N>(straggler);
    if rotate {
        {
            let net = lobby.network.0.lock().unwrap();
            let h = net.handle().unwrap();
            h.set_open(false);
            h.set_open(true);
        }
        assert_ne!(
            lobby.lan_token_now(),
            lobby.rejoin_at().1,
            "the LAN token did not rotate"
        );
    }
    if let Some(i) = tamper {
        tamper_switch(&lobby, &armed, &standby, i);
    }
    let mut standby = Some(standby);
    let Lobby {
        network,
        links,
        mut host,
        clients: sessions,
        original,
        ..
    } = lobby;
    let (switch_token, open_at_switch) = {
        let net = network.0.lock().unwrap();
        let h = net.handle().unwrap();
        (h.lan_token(), h.is_open())
    };
    let mut finished_at = None;
    let mut sessions: Vec<Option<ClientSession>> = sessions.into_iter().map(Some).collect();
    let mut switches: Vec<Option<migration::ClientSwitch>> = (0..N).map(|_| None).collect();
    let mut rejoined: Vec<Option<ClientSession>> = (0..N).map(|_| None).collect();
    let switching: Vec<usize> = (0..N).filter(|&i| Some(i) != straggler).collect();
    let mut all_switched = None;
    let started = Instant::now();
    let mut switch = migration::HostSwitch::start(
        &mut host,
        &mut network.0.lock().unwrap(),
        armed,
        &original,
        clock,
        clock.now_ms(),
    );
    if failure == SwitchFailure::PinRefused {
        switch.refuse_pins_for_test();
    }
    loop {
        let now = clock.now_ms();
        switch.poll(&mut host, &mut network.0.lock().unwrap(), now);
        if switch.state().is_finished() {
            finished_at.get_or_insert(now);
        }
        // Frozen until the switch gives the lobby back.
        host.update(now);
        if let Some(live) = standby.as_mut() {
            live.poll().unwrap();
        }
        for i in 0..N {
            if let Some(session) = sessions[i].as_mut() {
                session.update(paused, Input::default());
                if Some(i) != straggler {
                    let arming = clients[i].as_mut().unwrap();
                    arming.poll(session, &mut links[i].0.lock().unwrap(), now);
                }
            }
            let arming = clients[i].as_mut().unwrap();
            if arming.state() == &migration::ArmingState::Switching && sessions[i].is_some() {
                let session = sessions[i].take().unwrap();
                let own = (i == 0).then_some(standby.as_ref().unwrap());
                match arming.take_switch(session, own) {
                    Ok(started) => switches[i] = Some(started),
                    Err(session) => sessions[i] = Some(session),
                }
            }
            if let Some(device) = switches[i].as_mut() {
                if !device.state().is_finished() {
                    device.poll();
                }
                if device.state().is_finished() && rejoined[i].is_none() {
                    rejoined[i] = device.take_client();
                }
            }
            if let Some(session) = rejoined[i].as_mut() {
                session.update(now, Input::default());
            }
        }
        if standby.is_some() {
            let drop_now = match failure {
                // 1 s after every switching client closed its ordinary link
                // (a straggler keeps its own), well inside the 5 s leave
                // window: the host has read their `Left`s by then, so the
                // thaw itself must depart them.
                SwitchFailure::StandbyBeforePin => {
                    if switching.iter().all(|&i| switches[i].is_some()) {
                        let since = *all_switched.get_or_insert_with(Instant::now);
                        since.elapsed() >= Duration::from_secs(1)
                    } else {
                        false
                    }
                }
                SwitchFailure::PinRefused => false,
                SwitchFailure::StandbyAfterPin => switch.players().is_some(),
            };
            if drop_now {
                drop(standby.take());
            }
        }
        let devices_done = switching.iter().all(|&i| {
            switches[i]
                .as_ref()
                .is_some_and(|s| s.state().is_finished())
        });
        let done = match failure {
            SwitchFailure::StandbyAfterPin => devices_done,
            _ => switching.iter().all(|&i| {
                rejoined[i]
                    .as_ref()
                    .is_some_and(|s| matches!(s.phase(), race_session::ClientPhase::Lobby))
            }),
        };
        if switch.state().is_finished() && done {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "{failure:?} stalled: host {:?}, humans {}, devices {:?}, sessions {:?}",
            switch.state(),
            host.human_count(),
            switches
                .iter()
                .map(|s| s.as_ref().map(|s| s.state().clone()))
                .collect::<Vec<_>>(),
            rejoined
                .iter()
                .map(|s| s.as_ref().map(|s| s.phase().clone()))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    FailedSwitch {
        switch,
        host,
        switches,
        rejoined,
        sessions,
        clock,
        network,
        switch_token,
        open_at_switch,
        finished_at: finished_at.unwrap(),
    }
}

/// Before the pin, a failed switch gives the room back unarmed: the host
/// reports why, its lobby is thawed with no armed room, and each switched
/// device rejoins the ordinary lobby. The thawed lobby seats `expected`
/// (name, ready) humans exactly once each, and an unarmed race then starts
/// on every seated device.
fn assert_unarmed_fallback(mut f: FailedSwitch, reason: &str, expected: &[(&str, bool)]) {
    match f.switch.state() {
        migration::SwitchState::Unprotected { reason: why } => {
            assert!(why.contains(reason), "{why}")
        }
        other => panic!("the host did not fall back: {other:?}"),
    }
    assert!(!f.host.migration_frozen(), "the lobby stayed frozen");
    assert!(f.switch.take_room().is_none() && f.switch.superseded() == 0);
    for (i, device) in f.switches.iter().enumerate() {
        let Some(device) = device else { continue };
        match device.state() {
            migration::SwitchState::Unprotected { reason } => assert!(
                reason.contains("Rejoining the lobby unarmed"),
                "device {i}: {reason}"
            ),
            other => panic!("device {i} did not fall back: {other:?}"),
        }
    }
    let view = f.host.lobby_view();
    let mut humans: Vec<(&str, bool)> = view
        .seats
        .iter()
        .filter(|s| !s.bot)
        .map(|s| (s.name.as_str(), s.ready))
        .collect();
    humans.sort();
    assert_eq!(humans, expected, "the thawed lobby's humans");
    // The room proceeds unarmed: a race starts and reaches every seated
    // device.
    assert!(f.host.start_race(f.clock.now_ms()));
    let t = Instant::now();
    loop {
        let now = f.clock.now_ms();
        f.host.update(now);
        let mut racing = 0;
        for session in f.rejoined.iter_mut().chain(f.sessions.iter_mut()).flatten() {
            session.update(now, Input::default());
            racing += usize::from(matches!(session.phase(), race_session::ClientPhase::Racing));
        }
        if racing == expected.len() {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the unarmed race did not start"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// (h) A switch that fails after `Switch` and before the pin gives the room
/// back unarmed. The standby's pair link drops in the leave window, after
/// every client but a straggler closed its ordinary link. The host thaws the
/// lobby with its held events replayed (the straggler's ready toggle, held at
/// the frozen boundary) and with no ghost seats for the clients that left.
/// Each client's re-dial never completes, so it rejoins the ordinary lobby at
/// the same address and token, and a race then starts unarmed.
#[test]
fn a_switch_failing_after_switch_thaws_the_lobby_and_every_client_rejoins_unarmed() {
    let f = failed_switch(SwitchFailure::StandbyBeforePin, Some(3));
    assert_unarmed_fallback(
        f,
        "pair link failed",
        &[
            ("Backup", false),
            ("Fourth", true),
            ("Other", false),
            ("Third", false),
        ],
    );
}

/// (h2) The same fallback from a pin that is refused until the deadline
/// (the test hook stands in for Fray refusing it). The straggler is closed
/// at the leave deadline and departed by the thaw too, so exactly the three
/// rejoined humans are seated.
#[test]
fn a_pin_refused_until_the_deadline_thaws_the_lobby_and_every_client_rejoins_unarmed() {
    let f = failed_switch(SwitchFailure::PinRefused, Some(3));
    assert!(!f.switch.pin_refusals().is_empty());
    assert_unarmed_fallback(
        f,
        "could not be pinned",
        &[("Backup", false), ("Other", false), ("Third", false)],
    );
}

/// (h2b) A full lobby: eight humans on a dedicated host fill the eight
/// seats, so the room closed when the eighth sat, rotating the LAN token,
/// and `Switch` carries that token. The pin is refused until the deadline,
/// so the switch falls back: the thaw replays the eight `Left`s, which
/// reopen the room. In the fallback's rejoin window that reopen (and the
/// close when the last rejoin fills the room again) keeps the token, so
/// every client's fallback rejoin is admitted and an unarmed race starts.
/// After the window, a seat change rotates the token again. Mutation:
/// rotating in the window refuses both tokens a client rejoins with, and the
/// clients never get back into the lobby ("stalled").
#[test]
fn a_full_lobby_failed_switch_keeps_the_token_for_every_fallback_rejoin() {
    let mut f = failed_switch_n::<8>(SwitchFailure::PinRefused, None, false, None);
    let token_now = |f: &FailedSwitch| {
        let net = f.network.0.lock().unwrap();
        let h = net.handle().unwrap();
        (h.lan_token(), h.is_open())
    };
    assert!(!f.switch.pin_refusals().is_empty());
    assert!(
        !f.open_at_switch,
        "the full lobby's room was open at the switch"
    );
    assert_eq!(f.host.human_count(), 8, "every human is back");
    assert_eq!(
        token_now(&f),
        (f.switch_token.clone(), false),
        "the rejoin window rotated the token, or the full room did not close"
    );
    // Past the window, keep every device's frames running.
    let past = f.finished_at + migration::SWITCH_TOKEN_HOLD_MS + 50;
    let frame = |f: &mut FailedSwitch| {
        let now = f.clock.now_ms();
        f.host.update(now);
        for session in f.rejoined.iter_mut().chain(f.sessions.iter_mut()).flatten() {
            session.update(now, Input::default());
        }
        std::thread::sleep(Duration::from_millis(1));
        now
    };
    while frame(&mut f) < past {}
    assert_eq!(token_now(&f), (f.switch_token.clone(), false));
    // A seat change after the window: one human leaves, the room reopens and
    // the token rotates, as natively.
    let mut leaver = f.rejoined[7].take().expect("device 7 rejoined");
    leaver.leave();
    let t = Instant::now();
    while f.host.human_count() != 7 {
        leaver.update(f.clock.now_ms(), Input::default());
        frame(&mut f);
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the leave was never seen"
        );
    }
    let (after, open) = token_now(&f);
    assert!(open, "the room did not reopen after the leave");
    assert_ne!(
        after, f.switch_token,
        "a seat change past the window kept the token"
    );
    println!(
        "STAMPEDE FULL LOBBY FALLBACK rejoined=8 token_kept_in_window=true rotated_after_window=true"
    );
    assert_unarmed_fallback(
        f,
        "could not be pinned",
        &[
            ("Backup", false),
            ("Fifth", false),
            ("Fourth", false),
            ("Other", false),
            ("Seventh", false),
            ("Sixth", false),
            ("Third", false),
        ],
    );
}

/// (h3) The LAN token rotated after every device joined, and the switch then
/// fails before the pin (the standby's pair link drops in the leave window,
/// held open by a straggler that ignores `Switch`, as in (h)). Each switched
/// device's fallback, an ordinary rejoin at its own join address, uses the
/// token from `Switch`, so the rotated listener admits it: all three are back
/// in the thawed lobby and a race starts unarmed. Mutation: the fallback
/// using the join-time token is refused, and the devices never reach the
/// lobby.
#[test]
fn a_switch_failing_after_a_token_rotation_still_rejoins_every_client_unarmed() {
    let f = failed_switch_with(SwitchFailure::StandbyBeforePin, Some(3), true, None);
    assert_unarmed_fallback(
        f,
        "pair link failed",
        &[
            ("Backup", false),
            ("Fourth", true),
            ("Other", false),
            ("Third", false),
        ],
    );
}

/// (h4) A device gets a `Switch` with a wrong token and the switch then fails
/// before the pin. Its re-dial is refused at admission; its fallback rejoin
/// tries the `Switch` token (refused again), then the token it joined with,
/// both at its own join address only, and lands in the thawed lobby with the
/// others. Mutation: a fallback with the `Switch` token alone strands it
/// ("stalled").
#[test]
fn a_wrong_switch_token_before_a_failed_pin_still_rejoins_with_the_join_token() {
    let f = failed_switch_with(SwitchFailure::StandbyBeforePin, Some(3), false, Some(2));
    match f.switches[2].as_ref().map(|s| s.state()) {
        Some(migration::SwitchState::Unprotected { reason }) => {
            assert!(reason.contains("could not be rejoined"), "{reason}")
        }
        other => panic!("device 2: {other:?}"),
    }
    assert_unarmed_fallback(
        f,
        "pair link failed",
        &[
            ("Backup", false),
            ("Fourth", true),
            ("Other", false),
            ("Third", false),
        ],
    );
}

/// (i) After a successful pin nothing returns to the lobby. The standby's
/// pair link drops right after the pin: the switch still finishes
/// `Protected` and `take_room` hands over the armed room, whose runtime
/// pauses with the standby-loss reason. The lobby authority stays frozen
/// (superseded) with every human still seated: nothing is thawed.
#[test]
fn a_standby_lost_after_the_pin_pauses_the_armed_room_and_nothing_is_thawed() {
    let mut f = failed_switch(SwitchFailure::StandbyAfterPin, None);
    assert!(
        matches!(f.switch.state(), migration::SwitchState::Protected { .. }),
        "{:?}",
        f.switch.state()
    );
    assert!(
        f.host.migration_frozen(),
        "the lobby was thawed after the pin"
    );
    assert_eq!(f.host.human_count(), 4);
    let (mut live, _players) = f
        .switch
        .take_room()
        .expect("the armed room was not handed over after the pin");
    // The armed runtime pauses once it reads the lost pair link.
    let t = Instant::now();
    while live.failure().is_none() {
        let _ = live.poll();
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the armed room did not pause"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(live.poll().is_err(), "a paused room polled on");
}

/// (e) A tampered `Armed` and a tampered cert0 are refused: neither device
/// closes its ordinary link or dials. The host's genuine bundle then
/// switches both.
#[test]
fn a_tampered_armed_or_certificate_is_refused_by_the_client() {
    let s = switched(SwitchPlan {
        tamper: true,
        ..SwitchPlan::default()
    });
    assert_eq!(s.tamper_refused.len(), 2);
    for (reason, untouched) in &s.tamper_refused {
        assert!(reason.contains("refused"), "{reason}");
        assert!(
            untouched,
            "a refused bundle closed the link or began a switch"
        );
    }
    assert!(s.tamper_refused[0].0.contains("Armed"));
    assert!(s.tamper_refused[1].0.contains("certificate"));
    assert_eq!(protected_list(&s.host), Vec::<u64>::new());
    println!(
        "STAMPEDE SWITCH TAMPERED armed_refused={:?} cert_refused={:?} then_protected=true",
        s.tamper_refused[0].0, s.tamper_refused[1].0
    );
}
