//! The wire protocol (spec 10): reliable messages both ways, 60 Hz lossy
//! `Inputs` up, 30 Hz lossy `Snapshot`s down. bincode (varint) throughout.
//!
//! Limits (decented-net): every reliable message <= [`MAX_RELIABLE_LEN`] bytes,
//! every lossy one <= [`MAX_LOSSY_LEN`]. [`encode`] never checks; the senders
//! do (events are chunked, snapshots are budgeted with [`Snapshot::budget`]).

use bincode::Options;
use race_core::{
    BattleRules, BattleState, BotLevel, Event, Projectile, ProjectileKind, RaceConfig, Racer, RacerNet,
    TrackId, Vec3,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use fray_session::MAX_RELIABLE_LEN;
pub const MAX_LOSSY_LEN: usize = MAX_RELIABLE_LEN;

/// Bumped on any incompatible wire change. A `Hello` with another number is
/// refused. 5: rug traps, money printers and dismounted jockeys. 6: rug
/// positions, remount protection and finisher waits. 7: Laser Trump (`Item` and
/// `HitCause` grow, the packed racer effects gain a trump timer byte). 8: held
/// coins add 1.2 % top speed each again (`coin_bonus`), which clients predict.
/// 9: drift is a proportional power-slide, cake locks the buttons, jumps are
/// bigger and glide; clients predict all of it.
/// 10: five built-in tracks have real hills, and custom tracks may too (track
/// format 2), so heights, grades and jump arcs differ.
/// 11: armed host-migration rooms (session-bound player links, certified
/// outputs).
/// 12: cake takes others' good power-ups and stacks them; the Rose and its seller
/// (`Item` grows, the own racer carries its queue, `PlayState` gains sellers, and
/// the own racer's effects carry the seller's side, so clients predict its shove).
pub const PROTO_VERSION: u32 = 12;
/// Most seats in a race (humans and bots).
pub const MAX_SEATS: usize = 8;
/// Longest display name, in characters.
pub const NAME_CHARS: usize = 12;
/// Sim ticks between snapshots (60 Hz / 2 = 30 Hz).
pub const SNAPSHOT_EVERY_TICKS: u64 = 2;
/// Inputs sent per `Inputs` datagram (the newest, redundantly).
pub const INPUTS_PER_PACKET: usize = 5;
/// Hard cap on a decoded message (alloc guard; real ones are <= 1024).
const DECODE_LIMIT: u64 = 4096;

fn opts() -> impl Options {
    bincode::DefaultOptions::new().with_limit(DECODE_LIMIT)
}

/// Serialise a message.
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    opts().serialize(v).unwrap_or_default()
}

/// Serialised size without allocating.
pub fn encoded_len<T: Serialize>(v: &T) -> usize {
    opts().serialized_size(v).unwrap_or(u64::MAX) as usize
}

/// Parse a message; `None` if it is malformed (never panics).
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    opts().deserialize(bytes).ok()
}

// ─── Reliable: client -> host ───────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToHost {
    Hello { proto: u32, name: String, character: u8 },
    SetCharacter(u8),
    /// Purely visual colour, ostrich and jockey selections, each 0..=3.
    SetAttire([u8; 3]),
    /// Lobby ready flag.
    SetReady(bool),
    /// The current custom track was checked and installed.
    TrackReady { hash: u64 },
    /// Results-screen ready flag (everyone ready starts the rematch 3-2-1).
    RematchReady(bool),
    Leave,
    /// Host clock sync.
    Ping { id: u32, client_t: u64 },
    /// One chunk of a [`ToHostMigration`] (appended last: variant index 8).
    Migration(MigrationPart),
}

// ─── Reliable: host -> client ───────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToClient {
    /// You are in. `seat` is your stable player id in `LobbyView` (not your
    /// racer index, which `RaceStart` gives).
    Welcome { proto: u32, seat: u8 },
    /// The roster (lobby, and during results where `ready` means rematch-ready).
    Lobby(LobbyView),
    /// Build the world from `config`; world tick 0 is host-clock `start_host_ms`
    /// (the 3 s grid countdown follows). `you` is your racer index.
    /// `race` numbers the race (wraps at 256): snapshots and inputs carry it so
    /// stragglers from the previous race are ignored after a rematch.
    RaceStart { config: RaceConfig, custom: Option<u64>, start_host_ms: u64, you: u8, race: u8 },
    /// Canonical custom-track bytes, split into bounded reliable chunks.
    TrackData {
        hash: u64,
        idx: u8,
        total: u8,
        #[serde(deserialize_with = "bounded_track_bytes")]
        bytes: Vec<u8>,
    },
    /// Sim events for sound, FX and HUD (cosmetic high-rate ones are not sent).
    Events(Vec<Event>),
    Results(ResultsView),
    Rematch(RematchMsg),
    Pong { id: u32, client_t: u64, host_t: u64 },
    /// The host ended your link (full, version, kicked, host leaving).
    Closed(String),
    /// One chunk of a [`ToClientMigration`] (variant index 9).
    Migration(MigrationPart),
    /// A Keep's way back in, sent once after admission (appended last: variant
    /// index 10). Today's clients cannot decode a variant they do not know and
    /// drop the message, so it needs no `PROTO_VERSION` bump (a bump would make
    /// every older client refuse the host).
    KeepPass(KeepPass),
    /// A player left the race: their racer drives on as a bot. Unless they had
    /// finished, the results list them as Left, after everyone else, with no
    /// time or points (appended last: variant index 11; older clients drop it
    /// and see an unplaced racer last).
    Left { racer: u8, finished: bool },
}

/// The pair a `stampede://keep/<host key>/<secret>` invite link carries: the
/// Keep's runtime public key and an invite's secret. A Keep sends one to a
/// player it has just admitted so the app can save the Keep to My Keeps.
/// The secret is a way in: `Debug` does not print it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepPass {
    pub host_key: [u8; 32],
    pub secret: [u8; 32],
}

impl std::fmt::Debug for KeepPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeepPass").field("host_key", &self.host_key.iter().map(|byte| format!("{byte:02x}")).collect::<String>()).field("secret", &"<hidden>").finish()
    }
}

// ─── Reliable: host-migration setup (both directions) ───────────────────────

/// Most bytes in one [`MigrationPart`], so the whole message stays under
/// [`MAX_RELIABLE_LEN`].
pub const MIGRATION_CHUNK: usize = 900;
/// Most parts in one migration record.
pub const MIGRATION_MAX_PARTS: usize = 4;
/// Largest serialized migration payload: `MIGRATION_MAX_PARTS * MIGRATION_CHUNK`.
pub const MIGRATION_MAX_PAYLOAD: usize = MIGRATION_MAX_PARTS * MIGRATION_CHUNK;
/// Longest room code in an `Offer`, in bytes.
pub const MIGRATION_CODE_LEN: usize = 6;
/// Longest listener address, in bytes.
pub const MIGRATION_ADDR_LEN: usize = 64;
/// Longest `Unprotected` / `Withdrawn` reason, in bytes.
pub const MIGRATION_REASON_LEN: usize = 200;

/// One chunk of a serialized [`ToClientMigration`] or [`ToHostMigration`]:
/// `1 <= parts <= 4`, `part < parts`, `bytes.len() <= MIGRATION_CHUNK`. Parts
/// arrive in order on the reliable stream; see `migration::wire`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationPart {
    pub part: u8,
    pub parts: u8,
    #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_, u8, MIGRATION_CHUNK>")]
    pub bytes: Vec<u8>,
}

/// Host-migration setup, host -> client. Carried in [`ToClient::Migration`]
/// parts; the client only surfaces it (the flow lives in the lobby modules).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToClientMigration {
    /// Phase 1, unsigned, to the selected device: "will you be the backup?"
    Ask,
    /// Only to the selected device.
    Offer {
        #[serde(deserialize_with = "bounded_string::<_, MIGRATION_CODE_LEN>")]
        code: String,
        #[serde(deserialize_with = "bounded_payload_bytes")]
        descriptor: Vec<u8>,
    },
    /// To every roster human; clients sign their CapabilityAck automatically.
    Setup {
        #[serde(deserialize_with = "bounded_payload_bytes")]
        descriptor: Vec<u8>,
        initial: [u8; 32],
    },
    /// cert0.
    Armed {
        #[serde(deserialize_with = "bounded_payload_bytes")]
        armed: Vec<u8>,
        #[serde(deserialize_with = "bounded_payload_bytes")]
        certificate: Vec<u8>,
    },
    /// The standby's control listener.
    Standby {
        #[serde(deserialize_with = "bounded_string::<_, MIGRATION_ADDR_LEN>")]
        addr: String,
        token: [u8; 32],
    },
    /// Close the ordinary link now and re-dial session-bound: the original
    /// join address, with the host listener's LAN token at the time it sent
    /// this (which may have rotated since the client joined).
    Switch { token: [u8; 32] },
    /// The host tells this client it is unprotected.
    Unprotected {
        #[serde(deserialize_with = "bounded_string::<_, MIGRATION_REASON_LEN>")]
        reason: String,
    },
    /// The offer was cancelled or arming was abandoned.
    Withdrawn {
        #[serde(deserialize_with = "bounded_string::<_, MIGRATION_REASON_LEN>")]
        reason: String,
    },
}

/// Host-migration setup, client -> host. Carried in [`ToHost::Migration`]
/// parts; the host only surfaces it with the sending peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToHostMigration {
    /// Phase 1, unsigned: the person said yes.
    Willing,
    /// The standby's pair listener.
    Consent {
        #[serde(deserialize_with = "bounded_payload_bytes")]
        descriptor: Vec<u8>,
        #[serde(deserialize_with = "bounded_string::<_, MIGRATION_ADDR_LEN>")]
        addr: String,
        token: [u8; 32],
    },
    Decline,
    Ack {
        #[serde(deserialize_with = "bounded_payload_bytes")]
        ack: Vec<u8>,
    },
}

/// A payload byte field: never more than a whole reassembled record.
fn bounded_payload_bytes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    fray_session::checkpoint::bounded_vec::<_, u8, MIGRATION_MAX_PAYLOAD>(d)
}

/// A `String` of at most `N` bytes (bincode's decode limit bounds the read
/// before this check).
fn bounded_string<'de, D: serde::Deserializer<'de>, const N: usize>(
    d: D,
) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    if s.len() > N {
        return Err(serde::de::Error::custom(format!(
            "string exceeds {N} bytes"
        )));
    }
    Ok(s)
}

/// Reject a hostile announced length before serde reserves its Vec capacity.
fn bounded_track_bytes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct Bytes;
    impl<'de> serde::de::Visitor<'de> for Bytes {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "at most {} track bytes", crate::track_transfer::CHUNK_BYTES)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let cap = seq.size_hint().unwrap_or(0);
            if cap > crate::track_transfer::CHUNK_BYTES {
                return Err(serde::de::Error::custom("track chunk exceeds 900 bytes"));
            }
            let mut bytes = Vec::with_capacity(cap);
            while let Some(b) = seq.next_element::<u8>()? {
                if bytes.len() == crate::track_transfer::CHUNK_BYTES {
                    return Err(serde::de::Error::custom("track chunk exceeds 900 bytes"));
                }
                bytes.push(b);
            }
            Ok(bytes)
        }
    }
    d.deserialize_seq(Bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RematchMsg {
    /// Everyone is ready: the rematch starts at this host-clock time.
    Countdown { ends_at_host_ms: u64 },
    /// Someone un-readied or left.
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LobbyPhase {
    Lobby,
    Racing,
    Results,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeatView {
    /// Seat id. Humans: 0..100, given in join order and kept while seated
    /// (key on this for people). Bots: `100 + racer index`, the racer the bot
    /// drives (in the lobby: the index it will take at the start, so a bot's
    /// id shifts when a person joins or leaves the lobby; from `RaceStart` on
    /// it is fixed, and a person who leaves mid-race shows up as the bot
    /// `100 + their racer index`). Use [`SeatView::racer`] during a race.
    pub id: u8,
    pub name: String,
    pub character: u8,
    pub ready: bool,
    pub bot: bool,
    /// The host's own player.
    pub host: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LobbyView {
    pub seats: Vec<SeatView>,
    pub track: TrackId,
    /// The selected custom track's hash; present exactly when `track` is Custom.
    pub custom: Option<u64>,
    pub laps: u8,
    /// Bots wanted (0..=7); fewer sit if humans need the seats.
    pub bots: u8,
    pub bot_level: BotLevel,
    pub phase: LobbyPhase,
    /// `Some`: the next race is a battle on this arena with these rules
    /// (`track` and `laps` are then unused). Set with `HostAuthority::set_battle`.
    pub battle: Option<BattleSetup>,
}

/// A battle chosen in the lobby.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BattleSetup {
    /// One of `TrackId::ARENAS`.
    pub arena: TrackId,
    pub rules: BattleRules,
}

impl BattleSetup {
    /// A battle on `arena` with the default rules (3 feathers, 3 minutes).
    pub fn new(arena: TrackId) -> BattleSetup {
        BattleSetup { arena, rules: BattleRules::default() }
    }
}

impl SeatView {
    /// A bot seat's racer index (`id - 100`). Humans: `None` (their racer
    /// index comes from `RaceStart` / `HostAuthority::racer_of`).
    pub fn racer(&self) -> Option<usize> {
        if self.bot {
            self.id.checked_sub(crate::host::BOT_ID_BASE).map(usize::from)
        } else {
            None
        }
    }
}

impl LobbyView {
    /// Humans seated.
    pub fn humans(&self) -> usize {
        self.seats.iter().filter(|s| !s.bot).count()
    }

    pub fn seat(&self, id: u8) -> Option<&SeatView> {
        self.seats.iter().find(|s| s.id == id)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultRow {
    /// Racer index in the race.
    pub racer: u8,
    pub name: String,
    pub character: u8,
    pub place: u8,
    pub bot: bool,
    /// Finish time in race seconds (`None` = did not finish).
    pub time: Option<f32>,
    pub best_lap: Option<f32>,
    pub coins: u32,
    /// Grand Prix points for this place.
    pub points: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultsView {
    pub track: TrackId,
    pub laps: u8,
    /// Rows in place order (1st first).
    pub rows: Vec<ResultRow>,
}

impl ResultsView {
    /// Racer indices, 1st first.
    pub fn standings(&self) -> Vec<usize> {
        self.rows.iter().map(|r| r.racer as usize).collect()
    }
}

// ─── Lossy ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LossyToHost {
    /// `inputs` are the newest inputs, oldest first; the last one has number
    /// `seq`. Input number `n` is applied by world tick `n - 1` (so the first
    /// input, `seq` 1, drives tick 0): the client runs ahead of the host by
    /// about half an RTT plus the 2-tick jitter buffer.
    Inputs { race: u8, seq: u32, inputs: Vec<[u8; 4]> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LossyToClient {
    Snapshot(Snapshot),
}

/// A flying egg, compact (25 bytes plus varints).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjNet {
    pub id: u32,
    /// 0 rolling, 1 hunter.
    pub kind: u8,
    pub owner: u8,
    /// Hunter target racer, 255 = none.
    pub target: u8,
    pub pos: [f32; 3],
    /// cm/s.
    pub vel: [i16; 3],
}

impl ProjNet {
    pub fn from_projectile(p: &Projectile) -> ProjNet {
        let q = |v: f32| (v * 100.0).round().clamp(-32767.0, 32767.0) as i16;
        ProjNet {
            id: p.id,
            kind: match p.kind {
                ProjectileKind::RollingEgg => 0,
                ProjectileKind::HunterEgg => 1,
                ProjectileKind::DollarBill => 2,
            },
            owner: p.owner,
            target: p.target.unwrap_or(255),
            pos: p.pos.to_array(),
            vel: [q(p.vel.x), q(p.vel.y), q(p.vel.z)],
        }
    }

    pub fn to_projectile(&self) -> Projectile {
        Projectile {
            kind: match self.kind {
                1 => ProjectileKind::HunterEgg,
                2 => ProjectileKind::DollarBill,
                _ => ProjectileKind::RollingEgg,
            },
            pos: Vec3::from_array(self.pos),
            vel: Vec3::new(self.vel[0] as f32, self.vel[1] as f32, self.vel[2] as f32) / 100.0,
            owner: self.owner,
            target: if self.target == 255 { None } else { Some(self.target) },
            age: 0.0,
            bounces: 0,
            hint: 0,
            s: 0.0,
            lateral: 0.0,
            id: self.id,
        }
    }
}

/// Battle state for a snapshot (spec 17): about 60-90 bytes for 8 racers.
/// Vectors are by racer index.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BattleNet {
    /// Clock, deciseconds.
    pub time_left: u16,
    pub feathers: Vec<u8>,
    /// Invulnerability left (flashing), centiseconds.
    pub invuln: Vec<u8>,
    /// Time since going out, deciseconds (saturates at 25.5 s).
    pub out_t: Vec<u8>,
    /// Feathers popped (hits landed).
    pub hits: Vec<u16>,
    /// Feathers lost.
    pub taken: Vec<u16>,
    /// Racers out, in order.
    pub out_order: Vec<u8>,
    pub done: bool,
}

impl BattleNet {
    pub fn from_state(b: &BattleState) -> BattleNet {
        BattleNet {
            time_left: (b.time_left * 10.0).round().clamp(0.0, u16::MAX as f32) as u16,
            feathers: b.feathers.clone(),
            invuln: b.invuln.iter().map(|&t| (t * 100.0).ceil().clamp(0.0, 255.0) as u8).collect(),
            out_t: b.out_t.iter().map(|&t| (t * 10.0).floor().clamp(0.0, 255.0) as u8).collect(),
            hits: b.hits.clone(),
            taken: b.taken.clone(),
            out_order: b.out_order.iter().map(|&i| i as u8).collect(),
            done: b.done,
        }
    }

    /// Writes this onto a client's battle state. Only racer indices the
    /// state has are used (a hostile host cannot make the client index out
    /// of range).
    pub fn apply(&self, b: &mut BattleState) {
        let n = b.feathers.len();
        b.time_left = self.time_left as f32 / 10.0;
        for i in 0..n {
            b.feathers[i] = self.feathers.get(i).copied().unwrap_or(b.feathers[i]);
            b.invuln[i] = self.invuln.get(i).map_or(0.0, |&c| c as f32 / 100.0);
            b.out_t[i] = self.out_t.get(i).map_or(0.0, |&d| d as f32 / 10.0);
            b.hits[i] = self.hits.get(i).copied().unwrap_or(b.hits[i]);
            b.taken[i] = self.taken.get(i).copied().unwrap_or(b.taken[i]);
        }
        b.out_order.clear();
        for &i in &self.out_order {
            let i = i as usize;
            if i < n && !b.out_order.contains(&i) {
                b.out_order.push(i);
            }
        }
        b.done = self.done;
    }

    /// Whether racer `i` is out.
    pub fn is_out(&self, i: usize) -> bool {
        self.out_order.iter().any(|&o| o as usize == i)
    }
}

/// A dropped cactus.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HazNet {
    pub id: u32,
    pub owner: u8,
    pub pos: [f32; 3],
}

/// One recipient's view of the world, 30 Hz. At most [`MAX_LOSSY_LEN`] bytes
/// after [`Snapshot::budget`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Which race (`RaceStart::race`).
    pub race: u8,
    /// World ticks stepped when this was taken: it is the state after tick
    /// `tick - 1`, at host time `start_host_ms + tick * 1000 / 60`.
    pub tick: u32,
    /// The recipient's last applied input number (real or repeated). Because
    /// input `n` drives tick `n - 1`, this equals `tick` while the seat is
    /// being stepped; it is separate so the host may starve a seat.
    pub ack_seq: u32,
    /// Bit 0: the race is over (everyone drives as a bot now).
    pub flags: u8,
    /// The recipient's full racer state.
    pub own: Racer,
    /// Everyone else, by racer index.
    pub others: Vec<RacerNet>,
    pub projectiles: Vec<ProjNet>,
    pub hazards: Vec<HazNet>,
    /// Dropped coins (lowest priority; dropped first when over budget).
    pub loose_coins: Vec<[f32; 3]>,
    /// Bit i = `World::targets[i].alive`, including wide custom-track rows.
    #[serde(deserialize_with = "bounded_egg_bits")]
    pub targets_alive: Vec<u8>,
    /// Shared global items and moving obstacles.
    pub play: race_core::playful::PlayState,
    /// Bit i = `World::coins[i].alive`.
    pub coins_alive: Vec<u8>,
    /// Battle state (battles only; never dropped by the budget).
    pub battle: Option<BattleNet>,
}

pub const SNAP_OVER: u8 = 1;

fn bounded_egg_bits<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct Bits;
    impl<'de> serde::de::Visitor<'de> for Bits {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most 512 egg bits")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            if seq.size_hint().unwrap_or(0) > 64 { return Err(serde::de::Error::custom("too many egg bits")); }
            let mut bits = Vec::with_capacity(64);
            while let Some(b) = seq.next_element()? {
                if bits.len() == 64 { return Err(serde::de::Error::custom("too many egg bits")); }
                bits.push(b);
            }
            Ok(bits)
        }
    }
    d.deserialize_seq(Bits)
}

impl Snapshot {
    /// Serialised size.
    pub fn wire_len(&self) -> usize {
        1 + encoded_len(self)
    }

    /// Trim to the lossy budget: dropped coins go first, then the entities
    /// (eggs, cacti and rose sellers) farthest from `from`. A seller is only
    /// drawn: who it pesters travels in that racer's own effects. Own racer,
    /// others and the target/coin bitsets always stay. Returns how many
    /// entities were dropped.
    pub fn budget(&mut self, from: Vec3) -> usize {
        let d = |p: &[f32; 3]| Vec3::from_array(*p).distance_squared(from);
        self.projectiles.sort_by(|a, b| d(&a.pos).total_cmp(&d(&b.pos)));
        self.hazards.sort_by(|a, b| d(&a.pos).total_cmp(&d(&b.pos)));
        let dr = |r: &race_core::playful::RoseSeller| r.pos.distance_squared(from);
        self.play.roses.sort_by(|a, b| dr(a).total_cmp(&dr(b)));
        self.loose_coins.sort_by(|a, b| d(a).total_cmp(&d(b)));
        self.loose_coins.truncate(24);
        let mut dropped = 0;
        // Sizes: measure once, then subtract per-item sizes as we pop (items are
        // fixed-width except for the varint ids, so re-measure to be exact).
        while self.wire_len() > MAX_LOSSY_LEN {
            if self.loose_coins.pop().is_some() {
                dropped += 1;
                continue;
            }
            // The farthest of the farthest egg, cactus and seller goes.
            let far = [
                self.projectiles.last().map(|p| d(&p.pos)),
                self.hazards.last().map(|h| d(&h.pos)),
                self.play.roses.last().map(dr),
            ]
            .into_iter()
            .enumerate()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
            match far {
                Some((0, _)) => {
                    self.projectiles.pop();
                }
                Some((1, _)) => {
                    self.hazards.pop();
                }
                Some(_) => {
                    self.play.roses.pop();
                }
                None => break,
            }
            dropped += 1;

        }
        dropped
    }

    /// Encode as a lossy datagram.
    pub fn encode(&self) -> Vec<u8> {
        // Same bytes as `encode(&LossyToClient::Snapshot(..))` without the clone
        // (variant 0 is the single byte 0).
        let mut v = vec![0u8];
        v.extend(encode(self));
        v
    }

    pub fn is_over(&self) -> bool {
        self.flags & SNAP_OVER != 0
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

pub fn pack_bits(bits: impl Iterator<Item = bool>) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (i, b) in bits.enumerate() {
        if i % 8 == 0 {
            out.push(0);
        }
        if b {
            *out.last_mut().expect("pushed") |= 1 << (i % 8);
        }
    }
    out
}

pub fn bit_at(bytes: &[u8], i: usize) -> bool {
    bytes.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0)
}

/// Whether the host sends this event over the wire. Footsteps and the
/// countdown/GO beeps are derived on the client (from racer state and the
/// synced clock); training-lesson events do not exist online.
pub fn is_wire_event(e: &Event) -> bool {
    !matches!(
        e,
        Event::Footstep { .. }
            | Event::Countdown { .. }
            | Event::Go
            | Event::LessonProgress { .. }
            | Event::LessonDone { .. }
            | Event::LessonFailed
            | Event::Gate { .. }
    )
}

/// Split events into `Events` messages that each fit [`MAX_RELIABLE_LEN`].
pub fn chunk_events(events: &[Event]) -> Vec<Vec<u8>> {
    // Tag (1) + length varint (<= 3 for these sizes) = 4 bytes of framing.
    const FRAME: usize = 4;
    let mut out = Vec::new();
    let mut cur: Vec<Event> = Vec::new();
    let mut cur_len = FRAME;
    for e in events {
        let l = encoded_len(e);
        if !cur.is_empty() && cur_len + l > MAX_RELIABLE_LEN {
            out.push(encode(&ToClient::Events(std::mem::take(&mut cur))));
            cur_len = FRAME;
        }
        cur.push(e.clone());
        cur_len += l;
    }
    if !cur.is_empty() {
        out.push(encode(&ToClient::Events(cur)));
    }
    out
}

/// Clean a display name: control characters out, trimmed, at most
/// [`NAME_CHARS`] characters; empty becomes `fallback`.
pub fn sanitize_name(raw: &str, fallback: &str) -> String {
    let s: String = raw.chars().filter(|c| !c.is_control()).collect();
    let s: String = s.trim().chars().take(NAME_CHARS).collect();
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use race_core::{BotLevel, Item, RaceConfig, World};

    /// A snapshot as full as it gets: 8 racers, many eggs and cacti with
    /// maximum-width ids, every coin and target alive, loose coins.
    fn worst_case(track: TrackId) -> Snapshot {
        let cfg = RaceConfig::all_bots(track, 8, BotLevel::Hard, 7);
        let mut w = World::new(cfg);
        for _ in 0..400 {
            w.step(&[]);
        }
        let mut own = w.racers[0].clone();
        own.idx = 255;
        own.item = Some(Item::HunterEgg);
        own.item_uses = 255;
        own.queue = vec![race_core::ItemRoll { item: Item::LaserTrump, uses: 255 }; race_core::MAX_QUEUE];
        own.coins = u32::MAX;
        own.effects = race_core::playful::Effects {
            burn: 4.0,
            splat: 3.0,
            pill: 7.0,
            trump: 3.5,
            laptop: 2.0,
            cake_eat: 2.5,
            rose: 5.0,
            rose_side: -1,
            // Host-only: never sent.
            feather: 0.0,
            tumble: 2.8,
            printer: 5.0,
            dismount: 8.0,
            remount_guard: 1.5,
            chase_x: -12.0,
            chase_z: 10.0,
            chase_yaw: 2.0,
            le_mans: true,
            knock: race_core::KnockKind::Trump,
            note: race_core::playful::ItemNote::Cake(255),
            note_t: 2.5,
            scramble_mine: true,
        };
        for r in &mut w.racers {
            r.coins = u32::MAX;
            r.effects = own.effects;
            r.attire = [3; 3];
        }
        own.lap = 255;
        own.finished_at = Some(123456.78);
        let others = w.racers[1..].iter().map(|r| r.to_net()).collect::<Vec<_>>();
        let proj = |i: u32| ProjNet {
            id: u32::MAX - i,
            kind: 1,
            owner: 7,
            target: 3,
            pos: [-1234.5 + i as f32, 99.25, 4321.5],
            vel: [-32767, 32767, -32000],
        };
        let mut s = Snapshot {
            race: 255,
            tick: u32::MAX,
            ack_seq: u32::MAX,
            flags: SNAP_OVER,
            own,
            others,
            projectiles: (0..30).map(proj).collect(),
            hazards: (0..20).map(|i| HazNet { id: u32::MAX - i, owner: 7, pos: [1000.5 + i as f32, 3.0, -2000.25] }).collect(),
            loose_coins: (0..30).map(|i| [i as f32 * 3.0, 1.0, 2.0]).collect(),
            targets_alive: vec![255; 64],
            play: race_core::playful::PlayState {
                scramble: 8.0,
                frogs: vec![
                    race_core::playful::Frog {
                        pos: Vec3::new(1234.5, 98.5, -400.0),
                        yaw: 3.0,
                        tumble: 2.8,
                        victim: 7,
                        paused: 0.0
                    };
                    3
                ],
                rugs: vec![
                    race_core::mischief::Rug {
                        s: 500.0,
                        owner: 0,
                        pull: 36,
                        rest: 45,
                        victim: 7
                    };
                    race_core::mischief::MAX_RUGS
                ],
                roses: (0..race_core::playful::MAX_ROSES as u8)
                    .map(|k| race_core::playful::RoseSeller {
                        pos: Vec3::new(-1234.5, 98.5, 4321.5),
                        yaw: 3.0,
                        s: 6000.0,
                        lateral: -11.5,
                        owner: k,
                        victim: 7 - k,
                        tried: 255,
                        again: race_core::playful::ROSE_MAX_AGAIN,
                        side: -1,
                        t: 4.9,
                    })
                    .collect(),
            },

            coins_alive: pack_bits(std::iter::repeat_n(true, w.coins.len())),
            // Hosts only send battle state for a battle.
            battle: TrackId::ARENAS.contains(&track).then(worst_battle),
        };
        if TrackId::ARENAS.contains(&track) {
            // A battle rolls neither the cake nor the Rose.
            s.own.queue.clear();
            s.play.roses.clear();
        }
        s
    }


    /// Battle state as wide as it gets: 8 racers, maximum counters and clock.
    fn worst_battle() -> BattleNet {
        BattleNet {
            time_left: u16::MAX,
            feathers: vec![255; 8],
            invuln: vec![255; 8],
            out_t: vec![255; 8],
            hits: vec![u16::MAX; 8],
            taken: vec![u16::MAX; 8],
            out_order: (0..8).collect(),
            done: true,
        }
    }

    #[test]
    fn a_knock_off_cause_reaches_remote_racers_in_the_packed_effects() {
        for kind in race_core::KnockKind::ALL {
            for dismount in [0.0, 3.0] {
                let e = race_core::playful::Effects { dismount, knock: kind, ..Default::default() };
                let net = race_core::playful::Effects::unpack(e.pack());
                assert_eq!(net.knock, kind);
                assert_eq!(net.dismount > 0.0, dismount > 0.0);
            }
        }
        assert_eq!(race_core::playful::Effects::default().pack(), [0; 11], "nothing sent when nothing happens");
    }

    #[test]
    fn battle_snapshot_worst_case_fits_and_keeps_the_battle() {
        for arena in TrackId::ARENAS {
            let cfg = RaceConfig::battle_bots(arena, 8, BotLevel::Hard, race_core::BattleRules::default(), 3);
            let mut w = World::new(cfg);
            for _ in 0..900 {
                w.step(&[]);
            }
            let mut s = worst_case(arena);
            let b = w.battle.as_ref().unwrap();
            let real = BattleNet::from_state(b);
            assert!(encoded_len(&real) < encoded_len(&worst_battle()));
            let fixed = {
                let mut f = s.clone();
                f.projectiles.clear();
                f.hazards.clear();
                f.loose_coins.clear();
                f.wire_len()
            };
            s.budget(Vec3::new(-1234.5, 99.0, 4321.0));
            let bytes = s.encode();
            eprintln!(
                "{arena:?} battle snapshot: fixed part {fixed} B (battle {} B worst, {} B real), worst case {} B with {} eggs + {} cacti kept",
                encoded_len(&worst_battle()),
                encoded_len(&real),
                bytes.len(),
                s.projectiles.len(),
                s.hazards.len()
            );
            assert!(bytes.len() <= MAX_LOSSY_LEN, "{} bytes", bytes.len());
            assert_eq!(s.battle, Some(worst_battle()), "the budget never drops the battle");
            // Round trip onto a client's state.
            let mut mine = BattleState::new(race_core::BattleRules::default(), 8);
            real.apply(&mut mine);
            assert_eq!(mine.feathers, b.feathers);
            assert_eq!(mine.out_order, b.out_order);
            assert_eq!(mine.hits, b.hits);
            assert_eq!(mine.ranking(), b.ranking());
            // A hostile host's indices are ignored.
            let mut evil = worst_battle();
            evil.out_order = vec![200, 3, 3];
            evil.feathers = vec![1; 40];
            let mut mine = BattleState::new(race_core::BattleRules::default(), 4);
            evil.apply(&mut mine);
            assert_eq!(mine.out_order, vec![3]);
            assert_eq!(mine.feathers.len(), 4);
        }
    }

    #[test]
    fn snapshot_worst_case_fits_the_lossy_budget() {
        for track in TrackId::ALL {
            let mut s = worst_case(track);
            assert_eq!(s.others.len(), 7);
            let raw = s.wire_len();
            let dropped = s.budget(Vec3::new(-1234.5, 99.0, 4321.0));
            let bytes = s.encode();
            eprintln!(
                "{track:?}: raw {raw} B -> {} B ({dropped} dropped, {} eggs, {} cacti, {} loose coins kept; coin bits {} B)",
                bytes.len(),
                s.projectiles.len(),
                s.hazards.len(),
                s.loose_coins.len(),
                s.coins_alive.len()
            );
            assert!(bytes.len() <= MAX_LOSSY_LEN, "{track:?}: {} bytes", bytes.len());
            // Eggs near the recipient survive. In a battle the fixed part (eight
            // racers with every effect, a full wallet each, plus battle state) may
            // leave no room for even one.
            let egg = encoded_len(&ProjNet {
                id: u32::MAX,
                kind: 1,
                owner: 7,
                target: 3,
                pos: [-1234.5, 99.25, 4321.5],
                vel: [-32767, 32767, -32000],
            });
            assert!(
                !s.projectiles.is_empty()
                    || (TrackId::ARENAS.contains(&track) && bytes.len() + egg > MAX_LOSSY_LEN),
                "{track:?}: eggs near the recipient must survive"
            );
            assert_eq!(s.others.len(), 7);
            // Nearest eggs are the ones kept.
            assert_eq!(bytes, encode(&LossyToClient::Snapshot(s.clone())));
            let back: LossyToClient = decode(&bytes).expect("decodes");
            assert_eq!(back, LossyToClient::Snapshot(s));
        }
    }

    #[test]
    fn typical_snapshot_is_well_under_budget() {
        let mut s = worst_case(TrackId::Savanna);
        s.battle = None;
        s.targets_alive.truncate(8);
        // No seller out and nothing queued: those are the first things to push
        // a crowded snapshot into the budget, which drops far sellers then.
        s.play.roses.clear();
        s.own.queue.clear();
        s.own.coins = 12;
        for r in &mut s.others { r.coins = 12; }
        s.projectiles.truncate(4);
        s.hazards.truncate(2);
        s.loose_coins.truncate(3);
        assert_eq!(s.budget(Vec3::ZERO), 0);
        assert!(s.wire_len() < 1024, "{}", s.wire_len());
    }

    #[test]
    fn placed_rug_bounds_are_enforced_by_the_snapshot_codec() {
        let mut valid = worst_case(TrackId::Savanna);
        valid.budget(Vec3::ZERO);
        assert!(decode::<LossyToClient>(&valid.encode()).is_some());
        for bad in [
            race_core::mischief::Rug { s: f32::NAN, ..valid.play.rugs[0] },
            race_core::mischief::Rug { s: -1.0, ..valid.play.rugs[0] },
            race_core::mischief::Rug { owner: 8, ..valid.play.rugs[0] },
            race_core::mischief::Rug { pull: 37, ..valid.play.rugs[0] },
        ] {
            let mut snapshot = valid.clone();
            snapshot.play.rugs[0] = bad;
            assert!(decode::<LossyToClient>(&snapshot.encode()).is_none());
        }
        valid.play.rugs.push(valid.play.rugs[0]);
        assert!(decode::<LossyToClient>(&valid.encode()).is_none());
    }

    #[test]
    fn oversized_egg_and_frog_vectors_are_rejected() {
        let mut s = worst_case(TrackId::Savanna);
        s.budget(Vec3::ZERO);
        assert!(decode::<LossyToClient>(&s.encode()).is_some());
        s.targets_alive.push(255);
        assert!(decode::<LossyToClient>(&s.encode()).is_none());
        s.targets_alive.pop();
        s.play.frogs.push(s.play.frogs[0]);
        assert!(decode::<LossyToClient>(&s.encode()).is_none());
        s.play.frogs.pop();
        let full = worst_case(TrackId::Savanna);
        s.play.roses = full.play.roses.clone();
        assert!(decode::<LossyToClient>(&s.encode()).is_some());
        s.play.roses.push(full.play.roses[0]);
        assert!(decode::<LossyToClient>(&s.encode()).is_none());
        s.play.roses.pop();
        s.own.queue.push(full.own.queue[0]);
        assert!(decode::<LossyToClient>(&s.encode()).is_none());
        s.own.queue.pop();
        assert!(decode::<LossyToClient>(&s.encode()).is_some());
    }

    #[test]
    fn a_rose_seller_and_a_queue_survive_a_snapshot_round_trip() {
        let mut s = worst_case(TrackId::Savanna);
        s.play.roses.truncate(1);
        s.own.queue.truncate(3);
        s.budget(s.play.roses[0].pos);
        let back: LossyToClient = decode(&s.encode()).expect("decodes");
        let LossyToClient::Snapshot(back) = back;
        assert_eq!(back.play.roses, s.play.roses);
        assert_eq!(back.own.queue.len(), 3);
        assert_eq!(back.own.queue, s.own.queue);
        assert_eq!(back.own.effects.rose, 5.0);
        assert_eq!(back.own.effects.note, race_core::playful::ItemNote::Cake(255));
    }

    #[test]
    fn reliable_messages_fit() {
        // Worst-case names: 12 four-byte characters.
        let name: String = "𝔸".repeat(NAME_CHARS);
        assert_eq!(sanitize_name(&"𝔸".repeat(40), "x").chars().count(), NAME_CHARS);
        let seats: Vec<SeatView> = (0..8)
            .map(|i| SeatView { id: i, name: name.clone(), character: 7, ready: true, bot: false, host: i == 0 })
            .collect();
        let lobby = ToClient::Lobby(LobbyView {
            seats,
            track: TrackId::Midnight,
            custom: None,
            laps: 5,
            bots: 7,
            bot_level: BotLevel::Hard,
            phase: LobbyPhase::Results,
            battle: Some(BattleSetup {
                arena: TrackId::MempoolPlaza,
                rules: BattleRules { feathers: 255, max_feathers: 255, time_limit: u16::MAX, invuln_ds: 255, egg_respawn_ds: 255 },
            }),
        });
        assert!(encoded_len(&lobby) <= MAX_RELIABLE_LEN, "{}", encoded_len(&lobby));

        let mut cfg = RaceConfig::all_bots(TrackId::Midnight, 8, BotLevel::Hard, u64::MAX);
        for r in &mut cfg.racers {
            r.name = name.clone();
        }
        let start = ToClient::RaceStart { config: cfg.clone(), custom: None, start_host_ms: u64::MAX, you: 7, race: 255 };
        assert!(encoded_len(&start) <= MAX_RELIABLE_LEN, "{}", encoded_len(&start));
        let mut battle = cfg;
        battle.track = TrackId::MempoolPlaza;
        battle.mode = race_core::RaceMode::Battle(BattleRules {
            feathers: 255,
            max_feathers: 255,
            time_limit: u16::MAX,
            invuln_ds: 255,
            egg_respawn_ds: 255,
        });
        let start = ToClient::RaceStart { config: battle, custom: None, start_host_ms: u64::MAX, you: 7, race: 255 };
        assert!(encoded_len(&start) <= MAX_RELIABLE_LEN, "{}", encoded_len(&start));
        let evs = vec![
            Event::FeatherLost { racer: 7, by: Some(6) },
            Event::FeatherStolen { racer: 6, from: 7 },
            Event::Out { racer: 7, place: 8, pos: [1e6, -1e6, 1e6] },
            Event::BattleOver,
            Event::CoinsTaken { from: 7, to: 6, count: u32::MAX },
            Event::Hit { victim: 7, by: None, cause: race_core::HitCause::Pepe },
        ];
        for c in chunk_events(&evs) {
            assert!(c.len() <= MAX_RELIABLE_LEN);
            assert_eq!(decode::<ToClient>(&c), Some(ToClient::Events(evs.clone())));
        }

        let rows = (0..8)
            .map(|i| ResultRow {
                racer: i,
                name: name.clone(),
                character: 7,
                place: i + 1,
                bot: false,
                time: Some(1e6),
                best_lap: Some(1e6),
                coins: 255,
                points: 15,
            })
            .collect();
        let res = ToClient::Results(ResultsView { track: TrackId::Midnight, laps: 5, rows });
        assert!(encoded_len(&res) <= MAX_RELIABLE_LEN, "{}", encoded_len(&res));

        let hello = ToHost::Hello { proto: u32::MAX, name, character: 255 };
        assert!(encoded_len(&hello) <= MAX_RELIABLE_LEN);

        let part = MigrationPart {
            part: u8::MAX,
            parts: u8::MAX,
            bytes: vec![255; MIGRATION_CHUNK],
        };
        let up = encode(&ToHost::Migration(part.clone()));
        assert!(up.len() <= MAX_RELIABLE_LEN, "{}", up.len());
        assert_eq!(decode::<ToHost>(&up), Some(ToHost::Migration(part.clone())));
        let down = encode(&ToClient::Migration(part.clone()));
        assert!(down.len() <= MAX_RELIABLE_LEN, "{}", down.len());
        assert_eq!(decode::<ToClient>(&down), Some(ToClient::Migration(part)));
    }

    #[test]
    fn migration_variants_are_appended_last() {
        let part = MigrationPart {
            part: 0,
            parts: 1,
            bytes: vec![],
        };
        // Varint enum tags: ToHost has 8 earlier variants, ToClient 9.
        assert_eq!(encode(&ToHost::Migration(part.clone()))[0], 8);
        assert_eq!(encode(&ToHost::Ping { id: 0, client_t: 0 })[0], 7);
        assert_eq!(encode(&ToClient::Migration(part))[0], 9);
        assert_eq!(encode(&ToClient::Closed(String::new()))[0], 8);
        assert_eq!(encode(&ToClient::KeepPass(KeepPass { host_key: [1; 32], secret: [2; 32] }))[0], 10);
        assert_eq!(encode(&ToClient::Left { racer: 3, finished: false }), vec![11, 3, 0]);
    }

    #[test]
    fn events_are_chunked_under_the_reliable_limit() {
        let evs: Vec<Event> = (0..600)
            .map(|i| Event::TargetDestroyed { racer: 7, target: 60_000 + i as u16, item: Some(Item::Magnet), uses: 255 })
            .collect();
        let chunks = chunk_events(&evs);
        assert!(chunks.len() > 1);
        let mut n = 0;
        for c in &chunks {
            assert!(c.len() <= MAX_RELIABLE_LEN, "{}", c.len());
            let Some(ToClient::Events(v)) = decode::<ToClient>(c) else { panic!("not events") };
            n += v.len();
        }
        assert_eq!(n, evs.len());
        assert!(chunk_events(&[]).is_empty());
    }

    #[test]
    fn garbage_does_not_panic_or_allocate() {
        for len in [0usize, 1, 5, 64, 2000] {
            let junk = vec![0xFFu8; len];
            let _ = decode::<ToClient>(&junk);
            let _ = decode::<ToHost>(&junk);
            let _ = decode::<LossyToHost>(&junk);
            let _ = decode::<LossyToClient>(&junk);
            let _ = decode::<MigrationPart>(&junk);
            let _ = decode::<ToClientMigration>(&junk);
            let _ = decode::<ToHostMigration>(&junk);
        }
        // Claimed huge lengths in a migration part and in payload byte fields.
        let mut b = encode(&ToClient::Migration(MigrationPart {
            part: 0,
            parts: 1,
            bytes: vec![],
        }));
        b.pop();
        b.push(253);
        b.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode::<ToClient>(&b).is_none());
        for tag in 0u8..4 {
            let mut b = vec![tag, 253];
            b.extend_from_slice(&u64::MAX.to_le_bytes());
            assert!(decode::<ToClientMigration>(&b).is_none());
            assert!(decode::<ToHostMigration>(&b).is_none());
        }
        // A claimed huge vec length.
        let mut b = encode(&LossyToHost::Inputs { race: 1, seq: 1, inputs: vec![] });
        *b.last_mut().unwrap() = 250;
        assert!(decode::<LossyToHost>(&b).is_none());
    }

    #[test]
    fn inputs_roundtrip() {
        let m = LossyToHost::Inputs { race: 3, seq: 77, inputs: vec![[1, 2, 3, 0]; INPUTS_PER_PACKET] };
        assert_eq!(decode::<LossyToHost>(&encode(&m)), Some(m));
    }
}
