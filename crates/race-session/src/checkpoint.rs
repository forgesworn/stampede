//! Complete canonical game images, shared by native migration and authority.
//! The encoding, schema, domains and validation are unchanged.
use super::{HostAuthority, HostTransport, PeerKey};
use crate::track_transfer::SelectedTrack;
use bincode::Options;
use fray_session::checkpoint::{state_hash, Error, Hash, Result, MAX_IMAGE};
use race_core::{CustomTrack, TrackId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(super) const SCHEMA: u32 = 2;
pub(super) const MAGIC: &[u8] = b"stampede/authority-checkpoint/v1\0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub config: Hash,
    pub content: Hash,
}
#[derive(Serialize)]
pub(super) struct Snapshot<'a> {
    pub(super) schema: u32,
    pub(super) game_protocol: u32,
    pub(super) binding: &'a Binding,
    pub(super) custom: Option<&'a [u8]>,
    pub(super) authority: &'a HostAuthority,
    pub(super) host_only: HostOnly,
}
#[derive(Deserialize)]
struct OwnedSnapshot {
    schema: u32,
    game_protocol: u32,
    binding: Binding,
    custom: Option<Vec<u8>>,
    authority: HostAuthority,
    host_only: HostOnly,
}
/// Host-only simulation state that `race_core` keeps out of its serde form
/// (`#[serde(skip)]`, because it never goes on the wire) but that a successor
/// authority needs to continue the race exactly: the racers' Laser Trump
/// victim masks, their Turbo Feather timers and the frogs' paused path time.
/// Indexed like `World::racers` and `PlayState::frogs`; all empty without a race.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct HostOnly {
    #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_,_,8>")]
    pub(super) trump_hit: Vec<u32>,
    #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_,_,3>")]
    pub(super) frog_paused: Vec<f32>,
    #[serde(deserialize_with = "fray_session::checkpoint::bounded_vec::<_,_,8>")]
    pub(super) feather: Vec<f32>,
}
impl HostOnly {
    pub(super) fn capture(host: &HostAuthority) -> Self {
        host.run.as_ref().map_or_else(Self::default, |run| Self {
            trump_hit: run.world.racers.iter().map(|r| r.trump_hit).collect(),
            frog_paused: run.world.play.frogs.iter().map(|f| f.paused).collect(),
            feather: run.world.racers.iter().map(|r| r.effects.feather).collect(),
        })
    }
    fn apply(&self, host: &mut HostAuthority) -> Result<()> {
        let Some(run) = host.run.as_mut() else {
            return if *self == Self::default() {
                Ok(())
            } else {
                Err(Error::Invalid("race host-only state"))
            };
        };
        let world = &mut run.world;
        let racers = world.racers.len();
        if self.trump_hit.len() != racers
            || self.feather.len() != racers
            || self.frog_paused.len() != world.play.frogs.len()
            || self.trump_hit.iter().any(|&mask| mask >> racers != 0)
            || self.frog_paused.iter().any(|t| !t.is_finite() || *t < 0.0)
            || self.feather.iter().any(|t| !t.is_finite() || *t < 0.0)
        {
            return Err(Error::Invalid("race host-only state"));
        }
        for (racer, &mask) in world.racers.iter_mut().zip(&self.trump_hit) {
            racer.trump_hit = mask;
        }
        for (racer, &t) in world.racers.iter_mut().zip(&self.feather) {
            racer.effects.feather = t;
        }
        for (frog, &paused) in world.play.frogs.iter_mut().zip(&self.frog_paused) {
            frog.paused = paused;
        }
        Ok(())
    }
}
pub(super) fn options() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_big_endian()
        .with_limit(MAX_IMAGE as u64)
        .reject_trailing_bytes()
}
pub(super) fn binding(host: &HostAuthority) -> Result<Binding> {
    let mut config = b"stampede/config/v1\0".to_vec();
    config.extend(
        options()
            .serialize(&(&host.cfg, &host.battle, host.headless, host.le_mans))
            .map_err(|_| Error::Invalid("race config"))?,
    );
    let mut content = b"stampede/content/v1\0".to_vec();
    if let Some(custom) = &host.custom {
        content.extend(&custom.bytes);
    } else {
        content.extend(
            options()
                .serialize(&host.cfg.track)
                .map_err(|_| Error::Invalid("track id"))?,
        );
    }
    Ok(Binding {
        config: state_hash(&config)?,
        content: state_hash(&content)?,
    })
}

/// Capture only at a completed runner boundary. Fray captures the surrounding
/// journals/outbox and input ordering in the same candidate transaction.
pub fn checkpoint(host: &HostAuthority) -> Result<(Binding, Vec<u8>)> {
    validate(host)?;
    let binding = binding(host)?;
    let view = Snapshot {
        schema: SCHEMA,
        game_protocol: crate::proto::PROTO_VERSION,
        binding: &binding,
        custom: host.custom.as_ref().map(|c| c.bytes.as_slice()),
        authority: host,
        host_only: HostOnly::capture(host),
    };
    let size = options()
        .serialized_size(&view)
        .map_err(|_| Error::Bounds)?;
    if size.checked_add(MAGIC.len() as u64).ok_or(Error::Bounds)? > MAX_IMAGE as u64 {
        return Err(Error::Bounds);
    }
    let mut bytes = MAGIC.to_vec();
    bytes.extend(
        options()
            .serialize(&view)
            .map_err(|_| Error::Invalid("race checkpoint"))?,
    );
    Ok((binding, bytes))
}

/// Restore into an inactive fresh authority. The caller must authenticate and
/// rebind each stable seat before allowing a commit or application output.
pub fn restore(
    bytes: &[u8],
    expected: &Binding,
    transport: Box<dyn HostTransport>,
) -> Result<HostAuthority> {
    if bytes.len() > MAX_IMAGE || !bytes.starts_with(MAGIC) {
        return Err(Error::Invalid("race checkpoint bounds/magic"));
    }
    let mut saved: OwnedSnapshot = options()
        .deserialize(&bytes[MAGIC.len()..])
        .map_err(|_| Error::Invalid("race checkpoint encoding"))?;
    if saved.schema != SCHEMA
        || saved.game_protocol != crate::proto::PROTO_VERSION
        || saved.binding != *expected
    {
        return Err(Error::Invalid("race checkpoint binding/version"));
    }
    if let Some(content) = saved.custom {
        if content.len() > crate::track_transfer::MAX_BYTES {
            return Err(Error::Bounds);
        }
        let track =
            CustomTrack::from_bytes(&content).map_err(|_| Error::Invalid("custom track bytes"))?;
        let selected =
            SelectedTrack::new(track).map_err(|_| Error::Invalid("custom track geometry"))?;
        if selected.bytes != content {
            return Err(Error::Invalid("noncanonical custom track"));
        }
        if let Some(run) = saved.authority.run.as_mut() {
            if run.world.cfg.track != TrackId::Custom {
                return Err(Error::Invalid("custom world track"));
            }
            run.world.track = selected.built.clone();
        }
        saved.authority.custom = Some(selected);
    }
    if binding(&saved.authority)? != *expected {
        return Err(Error::Invalid("race config/content mismatch"));
    }
    saved.host_only.apply(&mut saved.authority)?;
    validate(&saved.authority)?;
    // Re-encoding checks canonical map order and full state restoration. Never
    // call update/start or touch a global custom-track registry during restore.
    if checkpoint(&saved.authority)?.1 != bytes {
        return Err(Error::Invalid("race checkpoint roundtrip"));
    }
    saved.authority.transport = transport;
    Ok(saved.authority)
}

pub(super) fn validate(host: &HostAuthority) -> Result<()> {
    fray_session::checkpoint::validate_finite(host)?;
    if host.humans.len() > crate::proto::MAX_SEATS
        || host.by_peer.len() != host.humans.len()
        || host.next_id >= super::BOT_ID_BASE
    {
        return Err(Error::Invalid("race membership"));
    }
    let mut seats = HashSet::new();
    let mut peers = HashSet::new();
    for human in &host.humans {
        if human.id >= super::BOT_ID_BASE
            || !seats.insert(human.id)
            || !peers.insert(human.peer)
            || host.by_peer.get(&human.peer) != Some(&human.id)
            || human.inq.len() > 120
            || human.starved.len() > 33
        {
            return Err(Error::Invalid("race seat/input state"));
        }
        if let Some(index) = human.racer {
            if host
                .run
                .as_ref()
                .is_none_or(|r| index >= r.world.racers.len())
            {
                return Err(Error::Invalid("race racer binding"));
            }
        }
    }
    if host.cfg.track == TrackId::Custom && host.custom.is_none() {
        return Err(Error::Invalid("missing custom content"));
    }
    if let Some(run) = &host.run {
        let world = &run.world;
        let n = world.racers.len();
        let (track, mode) = host
            .battle
            .map(|b| (b.arena, race_core::RaceMode::Battle(b.rules)))
            .unwrap_or((host.cfg.track, race_core::RaceMode::Race));
        let mut expected_tun = race_core::Tunables::default();
        if let Some(b) = host.battle {
            expected_tun.target_respawn = expected_tun.target_respawn.min(b.rules.egg_respawn());
        }
        if n == 0
            || n > crate::proto::MAX_SEATS
            || world.cfg.track != track
            || world.track.id != track
            || world.cfg.mode != mode
            || world.cfg.racers.len() != n
            || world.lap_times.len() != n
            || world.lap_start.len() != n
            || world.max_stuck.len() != n
            || world.bot_mem.len() != n
            || world.lesson.is_some()
            || world.challenge.is_some()
            || world.drill.is_some()
            || world.cfg.drill.is_some()
            || world.tun != expected_tun
        {
            return Err(Error::Invalid("race world/config"));
        }
        let mut gone = HashSet::new();
        if run.left.iter().any(|&r| r as usize >= n || !gone.insert(r)) {
            return Err(Error::Invalid("race leavers"));
        }
        let mut bound = HashSet::new();
        for h in &host.humans {
            if let Some(i) = h.racer {
                if !bound.insert(i) || world.cfg.racers[i].kind != race_core::RacerKind::Human {
                    return Err(Error::Invalid("race human racer binding"));
                }
            }
        }
        for (i, racer) in world.racers.iter().enumerate() {
            if racer.idx as usize != i
                || racer.character != world.cfg.racers[i].character
                || racer.character as usize >= race_core::CHARACTERS.len()
                || racer.laser.on_racer.is_some_and(|j| j as usize >= n)
                || matches!(racer.laser.hit_kind, race_core::LaserHit::Racer(j) if j as usize >= n)
                || matches!(racer.laser.hit_kind, race_core::LaserHit::Target(j) if j as usize >= world.targets.len())
            {
                return Err(Error::Invalid("race racer state"));
            }
        }
        let mut object_ids = HashSet::new();
        for p in &world.projectiles {
            if p.owner as usize >= n
                || p.target.is_some_and(|t| t as usize >= n)
                || !object_ids.insert(p.id)
            {
                return Err(Error::Invalid("race projectile binding"));
            }
        }
        for h in &world.hazards {
            if h.owner as usize >= n || !object_ids.insert(h.id) {
                return Err(Error::Invalid("race hazard binding"));
            }
        }
        for c in &world.loose_coins {
            if c.dropped_by as usize >= n {
                return Err(Error::Invalid("race coin binding"));
            }
        }
        match (&world.battle, host.battle) {
            (None, None) => {}
            (Some(b), Some(setup)) if b.rules == setup.rules => {
                let mut out = HashSet::new();
                if b.feathers.len() != n
                    || b.hits.len() != n
                    || b.taken.len() != n
                    || b.invuln.len() != n
                    || b.out_t.len() != n
                    || b.out_order.iter().any(|i| *i >= n || !out.insert(*i))
                {
                    return Err(Error::Invalid("race battle arrays"));
                }
            }
            _ => return Err(Error::Invalid("race battle config")),
        }
        if let Some(results) = &run.results {
            let mut rows = HashSet::new();
            if results.rows.len() != n
                || results.track != track
                || results
                    .rows
                    .iter()
                    .any(|r| r.racer as usize >= n || !rows.insert(r.racer))
            {
                return Err(Error::Invalid("race result bindings"));
            }
        }
    }
    Ok(())
}

pub(super) fn null_transport() -> Box<dyn HostTransport> {
    Box::new(crate::transport::NullHostTransport)
}

pub(super) mod peer_map {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        map: &HashMap<PeerKey, u8>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        map.iter()
            .map(|(peer, id)| (*peer, *id))
            .collect::<BTreeMap<_, _>>()
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<HashMap<PeerKey, u8>, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = HashMap<PeerKey, u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("at most eight unique peer mappings")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                if map.size_hint().is_some_and(|n| n > crate::proto::MAX_SEATS) {
                    return Err(serde::de::Error::custom("too many peers"));
                }
                let mut value = HashMap::new();
                while let Some((peer, id)) = map.next_entry()? {
                    if value.len() >= crate::proto::MAX_SEATS || value.insert(peer, id).is_some() {
                        return Err(serde::de::Error::custom("duplicate/too many peers"));
                    }
                }
                Ok(value)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}
