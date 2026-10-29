//! Keyless Fray adapter over the same lobby/race implementation as native play.
//! The trusted shell admits and binds players, supplies public RTT/local-seat
//! metadata, and keeps owner operations, Keep passes and migration services.
use super::{checkpoint, HostAuthority, HostConfig};
use crate::proto::{BattleSetup, MAX_SEATS};
use crate::transport::{HostNetEvent, HostTransport, LinkPath, PeerKey, LOCAL_PEER_BASE};
use bincode::Options;
use fray_session::authority::{
    checkpoint_hash, validate_command_batch, Authority, AuthorityError, Command, CommandLimits,
    Delivery, ErrorCode, Event, PlayerId, StepOrder, Target, Time, MAX_CHECKPOINT_BYTES,
    MAX_COMMANDS,
};
use race_core::{BotLevel, CustomTrack, TrackId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

const CONFIG_BYTES: usize = 16 * 1024;
const MESSAGE_BYTES: usize = 1024;
const BATCH_BYTES: usize = 256 * 1024;
const MAGIC: &[u8] = b"stampede/fray-authority/v1\0";
const SCHEMA: u32 = 1;

fn options(limit: usize) -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_big_endian()
        .with_limit(limit as u64)
        .reject_trailing_bytes()
}
fn bad(code: ErrorCode, message: &'static str) -> AuthorityError {
    AuthorityError::new(code, message)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthorityConfig {
    pub schema: u32,
    pub track: TrackId,
    pub laps: u8,
    pub bots: u8,
    pub bot_level: BotLevel,
    pub auto_start: bool,
    pub headless: bool,
    pub le_mans: bool,
    #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_,u8,CONFIG_BYTES>")]
    pub custom: Vec<u8>,
    pub battle: Option<BattleSetup>,
}
impl Default for AuthorityConfig {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            track: TrackId::Savanna,
            laps: 3,
            bots: 5,
            bot_level: BotLevel::Normal,
            auto_start: true,
            headless: true,
            le_mans: true,
            custom: Vec::new(),
            battle: None,
        }
    }
}
impl AuthorityConfig {
    pub fn to_bytes(&self) -> Result<Vec<u8>, AuthorityError> {
        options(CONFIG_BYTES)
            .serialize(self)
            .map_err(|_| bad(ErrorCode::InvalidConfig, "config encoding"))
    }
    fn parse(bytes: &[u8]) -> Result<Self, AuthorityError> {
        if bytes.len() > CONFIG_BYTES {
            return Err(bad(ErrorCode::InvalidConfig, "config bounds"));
        }
        let config: Self = options(CONFIG_BYTES)
            .deserialize(bytes)
            .map_err(|_| bad(ErrorCode::InvalidConfig, "config encoding"))?;
        if config.schema != SCHEMA {
            return Err(bad(ErrorCode::UnsupportedVersion, "config schema"));
        }
        if !(1..=9).contains(&config.laps)
            || config.bots > if config.headless { 8 } else { 7 }
            || (config.custom.is_empty()
                && (config.track == TrackId::Custom
                    || config.track == TrackId::Training
                    || config.track.is_arena()
                    || config.laps > 5))
            || (!config.custom.is_empty()
                && (config.track != TrackId::Custom || config.battle.is_some()))
            || config.to_bytes()?.as_slice() != bytes
        {
            return Err(bad(
                ErrorCode::InvalidConfig,
                "config values or canonical encoding",
            ));
        }
        if !config.custom.is_empty() {
            let track = CustomTrack::from_bytes(&config.custom)
                .map_err(|_| bad(ErrorCode::InvalidConfig, "custom track"))?;
            if track.laps != config.laps {
                return Err(bad(ErrorCode::InvalidConfig, "custom laps"));
            }
            // Validate every fallible geometry/encoding check without baking
            // and discarding the full racing line and terrain grid.
            race_core::custom::to_def(&track)
                .map_err(|_| bad(ErrorCode::InvalidConfig, "custom geometry"))?;
            let canonical = track.to_bytes();
            if canonical.is_empty() || canonical.len() > crate::track_transfer::MAX_BYTES {
                return Err(bad(ErrorCode::InvalidConfig, "custom geometry"));
            }
        }
        if config.battle.is_some_and(|battle| !battle.arena.is_arena()) {
            return Err(bad(ErrorCode::InvalidConfig, "battle arena"));
        }
        Ok(config)
    }
    fn make_host(
        &self,
        seed: u64,
        link: Box<dyn HostTransport>,
    ) -> Result<HostAuthority, AuthorityError> {
        let cfg = HostConfig {
            track: self.track,
            laps: self.laps,
            bots: self.bots,
            bot_level: self.bot_level,
            seed,
            auto_start: self.auto_start,
        };
        let mut host = if self.headless {
            HostAuthority::new_headless(link, cfg)
        } else {
            HostAuthority::new(link, cfg)
        };
        host.set_le_mans_start(self.le_mans);
        if !self.custom.is_empty() {
            let track = CustomTrack::from_bytes(&self.custom)
                .map_err(|_| bad(ErrorCode::InvalidConfig, "custom track"))?;
            if track.laps != self.laps {
                return Err(bad(ErrorCode::InvalidConfig, "custom laps"));
            }
            host.set_custom_track(&track)
                .map_err(|_| bad(ErrorCode::InvalidConfig, "custom geometry"))?;
        }
        if self.battle.is_some() && !host.set_battle(self.battle) {
            return Err(bad(ErrorCode::InvalidConfig, "battle config"));
        }
        Ok(host)
    }
}

/// Authenticated host-shell data; a remote game's payload cannot supply this.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PeerInfo {
    pub local: bool,
    pub rtt_ms: Option<u32>,
}
impl PeerInfo {
    pub fn to_bytes(self) -> Vec<u8> {
        options(MESSAGE_BYTES)
            .serialize(&self)
            .expect("fixed peer metadata")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Control {
    StartRace,
    ToLobby,
    SetTrack(TrackId),
    SetLaps(u8),
    SetBots(u8),
    SetBotLevel(BotLevel),
    SetLeMans(bool),
    SetBattle(Option<BattleSetup>),
    SetCustomTrack(
        #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_,u8,CONFIG_BYTES>")]
        Vec<u8>,
    ),
    Kick(u64),
    PeerRtt {
        player: u64,
        rtt_ms: Option<u32>,
    },
}
impl Control {
    pub fn to_bytes(&self) -> Result<Vec<u8>, AuthorityError> {
        options(CONFIG_BYTES)
            .serialize(self)
            .map_err(|_| AuthorityError::fault("control bounds"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Member {
    player: u64,
    logical: PeerKey,
    identity: Option<[u8; 32]>,
    info: PeerInfo,
}
#[derive(Serialize, Deserialize)]
struct Image {
    schema: u32,
    seed: u64,
    completed_tick: u64,
    next_peer: u32,
    #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_,u8,CONFIG_BYTES>")]
    config: Vec<u8>,
    #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_,Member,MAX_SEATS>")]
    members: Vec<Member>,
    binding: checkpoint::Binding,
    #[serde(
        deserialize_with = "fray_session::checkpoint::bounded_vec::<_,u8,MAX_CHECKPOINT_BYTES>"
    )]
    game: Vec<u8>,
}

enum Effect {
    Send(PeerKey, Delivery, Vec<u8>),
    Disconnect(PeerKey, &'static str),
    Open(bool),
    Shutdown,
}
#[derive(Default)]
struct Capture {
    effects: Vec<Effect>,
    rtts: BTreeMap<PeerKey, Option<u32>>,
    bytes: usize,
    overflow: bool,
}
impl Capture {
    fn push(&mut self, effect: Effect) {
        let size = match &effect {
            Effect::Send(_, _, bytes) => bytes.len(),
            _ => 0,
        };
        if self.overflow
            || self.effects.len() >= MAX_COMMANDS
            || size > MESSAGE_BYTES
            || self.bytes.checked_add(size).is_none_or(|n| n > BATCH_BYTES)
        {
            self.overflow = true;
            return;
        }
        self.bytes += size;
        self.effects.push(effect);
    }
}
struct CaptureLink(Arc<Mutex<Capture>>);
impl HostTransport for CaptureLink {
    fn poll(&mut self) -> Option<HostNetEvent> {
        None
    }
    fn send_reliable(&mut self, p: PeerKey, b: Vec<u8>) {
        self.0
            .lock()
            .unwrap()
            .push(Effect::Send(p, Delivery::Reliable, b));
    }
    fn send_lossy(&mut self, p: PeerKey, b: Vec<u8>) {
        self.0
            .lock()
            .unwrap()
            .push(Effect::Send(p, Delivery::Lossy, b));
    }
    fn kick(&mut self, p: PeerKey) {
        self.0.lock().unwrap().push(Effect::Disconnect(p, "kicked"));
    }
    fn disconnect(&mut self, p: PeerKey) {
        self.0
            .lock()
            .unwrap()
            .push(Effect::Disconnect(p, "disconnected"));
    }
    fn confirm_timeout(&mut self, p: PeerKey) {
        self.0
            .lock()
            .unwrap()
            .push(Effect::Disconnect(p, "confirmation timeout"));
    }
    fn set_open(&mut self, b: bool) {
        self.0.lock().unwrap().push(Effect::Open(b));
    }
    fn set_open_keeping_token(&mut self, b: bool) {
        self.set_open(b);
    }
    fn rtt_ms(&self, p: PeerKey) -> Option<u32> {
        self.0.lock().unwrap().rtts.get(&p).copied().flatten()
    }
    fn shutdown(&mut self) {
        self.0.lock().unwrap().push(Effect::Shutdown);
    }
}

pub struct RaceAuthority {
    host: HostAuthority,
    capture: Arc<Mutex<Capture>>,
    order: StepOrder,
    config: Vec<u8>,
    seed: u64,
    members: BTreeMap<u64, Member>,
    next_peer: u32,
    in_frame: bool,
    frame_commands: usize,
    frame_bytes: usize,
}
impl RaceAuthority {
    fn prepare_track(host: &HostAuthority) {
        if host.custom.is_none() {
            // The cache belongs to this native process or Wasm instance. Warm
            // only the selected track; subsequent live ticks use that same Arc.
            let track = host.battle.map_or(host.cfg.track, |battle| battle.arena);
            let _ = track.track();
        }
    }
    fn begin(&mut self, time: Time) -> Result<(), AuthorityError> {
        self.order.on_event(time)?;
        if !self.in_frame {
            if !self.host.begin_update(time.elapsed_us / 1000) {
                return Err(AuthorityError::fault("authority is frozen"));
            }
            self.in_frame = true;
        }
        Ok(())
    }
    fn logical(&self, player: u64) -> Result<PeerKey, AuthorityError> {
        self.members
            .get(&player)
            .map(|m| m.logical)
            .ok_or_else(|| AuthorityError::fault("unknown player"))
    }
    fn drain(&mut self) -> Result<Vec<Command>, AuthorityError> {
        let effects = {
            let mut capture = self.capture.lock().unwrap();
            if capture.overflow {
                return Err(AuthorityError::fault("native output bounds"));
            }
            capture.bytes = 0;
            std::mem::take(&mut capture.effects)
        };
        let player = |peer| {
            self.members
                .values()
                .find(|m| m.logical == peer)
                .map(|m| PlayerId(m.player))
                .ok_or_else(|| AuthorityError::fault("unknown output target"))
        };
        let mut commands = Vec::with_capacity(effects.len());
        for effect in effects {
            commands.push(match effect {
                Effect::Send(peer, delivery, bytes) => Command::Send {
                    target: Target::Player(player(peer)?),
                    delivery,
                    bytes,
                },
                Effect::Disconnect(peer, reason) => Command::Disconnect {
                    player: player(peer)?,
                    reason: reason.into(),
                },
                Effect::Open(open) => Command::SetOpen(open),
                Effect::Shutdown => {
                    return Err(AuthorityError::fault("shutdown belongs to the shell"))
                }
            });
        }
        validate_command_batch(
            &commands,
            |id| self.members.contains_key(&id.0),
            CommandLimits {
                commands: MAX_COMMANDS,
                message_bytes: MESSAGE_BYTES,
                batch_bytes: BATCH_BYTES,
                reason_bytes: 200,
            },
        )?;
        self.frame_commands = self
            .frame_commands
            .checked_add(commands.len())
            .filter(|n| *n <= MAX_COMMANDS)
            .ok_or_else(|| AuthorityError::fault("step command count"))?;
        for command in &commands {
            let size = match command {
                Command::Send { bytes, .. } => bytes.len(),
                Command::Disconnect { reason, .. } => reason.len(),
                _ => 0,
            };
            self.frame_bytes = self
                .frame_bytes
                .checked_add(size)
                .filter(|n| *n <= BATCH_BYTES)
                .ok_or_else(|| AuthorityError::fault("step command bytes"))?;
        }
        Ok(commands)
    }
    fn control(&mut self, time: Time, bytes: &[u8]) -> Result<(), AuthorityError> {
        if bytes.len() > CONFIG_BYTES {
            return Err(AuthorityError::fault("control bounds"));
        }
        let control: Control = options(CONFIG_BYTES)
            .deserialize(bytes)
            .map_err(|_| AuthorityError::fault("control encoding"))?;
        match control {
            Control::StartRace => {
                self.host.start_race(time.elapsed_us / 1000);
            }
            Control::ToLobby => {
                self.host.to_lobby();
            }
            Control::SetTrack(v) => {
                self.host.set_track(v);
            }
            Control::SetLaps(v) => {
                self.host.set_laps(v);
            }
            Control::SetBots(v) => {
                self.host.set_bots(v);
            }
            Control::SetBotLevel(v) => {
                self.host.set_bot_level(v);
            }
            Control::SetLeMans(v) => {
                self.host.set_le_mans_start(v);
            }
            Control::SetBattle(v) => {
                self.host.set_battle(v);
            }
            Control::SetCustomTrack(bytes) => {
                let track = CustomTrack::from_bytes(&bytes)
                    .map_err(|_| AuthorityError::fault("control custom track"))?;
                if self.host.phase != super::HostPhase::Lobby {
                    return Ok(());
                }
                self.host
                    .set_custom_track(&track)
                    .map_err(|_| AuthorityError::fault("control custom geometry"))?;
            }
            Control::Kick(id) => {
                if let Some(member) = self.members.get(&id) {
                    if let Some(seat) = self.host.by_peer.get(&member.logical).copied() {
                        self.host.kick(seat);
                    }
                }
            }
            Control::PeerRtt { player, rtt_ms } => {
                if let Some(member) = self.members.get_mut(&player) {
                    member.info.rtt_ms = rtt_ms;
                    self.capture
                        .lock()
                        .unwrap()
                        .rtts
                        .insert(member.logical, rtt_ms);
                }
            }
        }
        Self::prepare_track(&self.host);
        Ok(())
    }
    fn event(&mut self, time: Time, event: Event) -> Result<Vec<Command>, AuthorityError> {
        self.begin(time)?;
        let mut leaving = None;
        match event {
            Event::Joined(player) => {
                if self.members.contains_key(&player.id.0)
                    || self.members.len() >= MAX_SEATS
                    || self.next_peer >= LOCAL_PEER_BASE
                {
                    return Err(AuthorityError::fault("invalid admitted membership"));
                }
                if player.metadata.len() > MESSAGE_BYTES {
                    return Err(AuthorityError::fault("peer metadata bounds"));
                }
                let info: PeerInfo = options(MESSAGE_BYTES)
                    .deserialize(&player.metadata)
                    .map_err(|_| AuthorityError::fault("peer metadata"))?;
                let logical = self.next_peer | if info.local { LOCAL_PEER_BASE } else { 0 };
                self.next_peer += 1;
                self.capture
                    .lock()
                    .unwrap()
                    .rtts
                    .insert(logical, info.rtt_ms);
                self.members.insert(
                    player.id.0,
                    Member {
                        player: player.id.0,
                        logical,
                        identity: player.identity,
                        info,
                    },
                );
                self.host.on_net_event(HostNetEvent::Joined {
                    peer: logical,
                    path: if info.local {
                        LinkPath::Local
                    } else {
                        LinkPath::Direct
                    },
                });
            }
            Event::Left { player, .. } => {
                let peer = self.logical(player.0)?;
                self.host.on_net_event(HostNetEvent::Left { peer });
                leaving = Some((player.0, peer));
            }
            Event::Message {
                player,
                delivery,
                bytes,
            } => {
                let peer = self.logical(player.0)?;
                if bytes.len() > MESSAGE_BYTES {
                    return Err(AuthorityError::fault("input exceeds transport bound"));
                }
                self.host.on_net_event(match delivery {
                    Delivery::Reliable => HostNetEvent::Reliable { peer, bytes },
                    Delivery::Lossy => HostNetEvent::Lossy { peer, bytes },
                });
            }
            Event::Control(bytes) => self.control(time, &bytes)?,
        }
        let commands = self.drain()?;
        if let Some((id, peer)) = leaving {
            self.members.remove(&id);
            self.capture.lock().unwrap().rtts.remove(&peer);
        }
        Ok(commands)
    }
    fn failed(&self) {
        self.order.fault();
        self.capture.lock().unwrap().effects.clear();
    }
    fn encode_checkpoint(&self) -> Result<Vec<u8>, AuthorityError> {
        let completed_tick = self.order.checkpoint_tick()?;
        let pending = !self.capture.lock().unwrap().effects.is_empty();
        if pending {
            return Err(AuthorityError::fault("unpublished initial effects"));
        }
        let (binding, game) = checkpoint::checkpoint(&self.host)
            .map_err(|_| AuthorityError::fault("game checkpoint"))?;
        let image = Image {
            schema: SCHEMA,
            seed: self.seed,
            completed_tick,
            next_peer: self.next_peer,
            config: self.config.clone(),
            members: self.members.values().cloned().collect(),
            binding,
            game,
        };
        let size = options(MAX_CHECKPOINT_BYTES)
            .serialized_size(&image)
            .map_err(|_| AuthorityError::fault("checkpoint size"))?;
        if size + MAGIC.len() as u64 > MAX_CHECKPOINT_BYTES as u64 {
            return Err(AuthorityError::fault("checkpoint bounds"));
        }
        let mut bytes = MAGIC.to_vec();
        bytes.extend(
            options(MAX_CHECKPOINT_BYTES)
                .serialize(&image)
                .map_err(|_| AuthorityError::fault("checkpoint encoding"))?,
        );
        Ok(bytes)
    }
}
impl Authority for RaceAuthority {
    fn init(config: &[u8], seed: u64) -> Result<Self, AuthorityError> {
        let parsed = AuthorityConfig::parse(config)?;
        let capture = Arc::new(Mutex::new(Capture::default()));
        let host = parsed.make_host(seed, Box::new(CaptureLink(capture.clone())))?;
        Self::prepare_track(&host);
        Ok(Self {
            host,
            capture,
            order: StepOrder::new(60)?,
            config: config.to_vec(),
            seed,
            members: BTreeMap::new(),
            next_peer: 1,
            in_frame: false,
            frame_commands: 0,
            frame_bytes: 0,
        })
    }
    fn on_event(&mut self, time: Time, event: Event) -> Result<Vec<Command>, AuthorityError> {
        let result = self.event(time, event);
        if result.is_err() {
            self.failed();
        }
        result
    }
    fn tick(&mut self, time: Time) -> Result<Vec<Command>, AuthorityError> {
        let result = (|| {
            self.begin(time)?;
            self.host.finish_update();
            let commands = self.drain()?;
            self.order.finish_tick(time)?;
            self.in_frame = false;
            self.frame_commands = 0;
            self.frame_bytes = 0;
            Ok(commands)
        })();
        if result.is_err() {
            self.failed();
        }
        result
    }
    fn checkpoint(&self) -> Result<Vec<u8>, AuthorityError> {
        let result = self.encode_checkpoint();
        if result.is_err() {
            self.failed();
        }
        result
    }
    fn restore(config: &[u8], bytes: &[u8]) -> Result<Self, AuthorityError> {
        let parsed = AuthorityConfig::parse(config)?;
        if bytes.len() > MAX_CHECKPOINT_BYTES || !bytes.starts_with(MAGIC) {
            return Err(bad(ErrorCode::InvalidCheckpoint, "checkpoint bounds/magic"));
        }
        let image: Image = options(MAX_CHECKPOINT_BYTES)
            .deserialize(&bytes[MAGIC.len()..])
            .map_err(|_| bad(ErrorCode::InvalidCheckpoint, "checkpoint encoding"))?;
        if image.schema != SCHEMA {
            return Err(bad(ErrorCode::UnsupportedVersion, "checkpoint schema"));
        }
        if image.config != config || image.next_peer == 0 || image.next_peer >= LOCAL_PEER_BASE {
            return Err(bad(
                ErrorCode::InvalidCheckpoint,
                "checkpoint config/counter",
            ));
        }
        let capture = Arc::new(Mutex::new(Capture::default()));
        let host = checkpoint::restore(
            &image.game,
            &image.binding,
            Box::new(CaptureLink(capture.clone())),
        )
        .map_err(|_| bad(ErrorCode::InvalidCheckpoint, "game checkpoint"))?;
        let expected_time = Time::at_tick(image.completed_tick, 60)
            .map_err(|_| bad(ErrorCode::InvalidCheckpoint, "checkpoint clock overflow"))?;
        if host.cfg.seed != image.seed
            || host.headless != parsed.headless
            || host.cfg.auto_start != parsed.auto_start
            || host.now != expected_time.elapsed_us / 1000
        {
            return Err(bad(ErrorCode::InvalidCheckpoint, "seed/mode binding"));
        }
        let mut members = BTreeMap::new();
        let mut peers = BTreeSet::new();
        for member in image.members {
            if member.logical & !LOCAL_PEER_BASE == 0
                || member.logical & !LOCAL_PEER_BASE >= image.next_peer
                || (member.logical >= LOCAL_PEER_BASE) != member.info.local
                || !peers.insert(member.logical)
                || members.contains_key(&member.player)
            {
                return Err(bad(ErrorCode::InvalidCheckpoint, "membership mapping"));
            }
            capture
                .lock()
                .unwrap()
                .rtts
                .insert(member.logical, member.info.rtt_ms);
            members.insert(member.player, member);
        }
        if host
            .humans
            .iter()
            .any(|h| !peers.contains(&h.peer) || h.host != (h.peer >= LOCAL_PEER_BASE))
        {
            return Err(bad(ErrorCode::InvalidCheckpoint, "game membership"));
        }
        let authority = Self {
            host,
            capture,
            order: StepOrder::after_tick(60, image.completed_tick)?,
            config: config.to_vec(),
            seed: image.seed,
            members,
            next_peer: image.next_peer,
            in_frame: false,
            frame_commands: 0,
            frame_bytes: 0,
        };
        Self::prepare_track(&authority.host);
        if authority.checkpoint()?.as_slice() != bytes {
            return Err(bad(ErrorCode::InvalidCheckpoint, "noncanonical checkpoint"));
        }
        Ok(authority)
    }
    fn state_hash(&self) -> Result<[u8; 32], AuthorityError> {
        checkpoint_hash(&self.checkpoint()?)
    }
}
