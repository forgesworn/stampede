//! The host-migration setup envelope, both directions: a payload
//! ([`ToClientMigration`](crate::proto::ToClientMigration) or
//! [`ToHostMigration`](crate::proto::ToHostMigration)) is serialized with
//! Stampede's codec, split into at most [`MIGRATION_MAX_PARTS`]
//! [`MigrationPart`]s and reassembled per link, one record at a time (the
//! reliable stream is ordered). A malformed record is dropped, counted and
//! reported; it never panics and never closes the link. The receivers only
//! surface what arrives; they act on nothing.

use crate::proto::{
    decode, encode, MigrationPart, MIGRATION_CHUNK, MIGRATION_MAX_PARTS, MIGRATION_MAX_PAYLOAD,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::VecDeque;

/// Why a migration record was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// `parts` outside `1..=4`, or different from the record in progress.
    Parts,
    /// `part` is not the next one expected (or not below `parts`).
    Order,
    /// A part over [`MIGRATION_CHUNK`] bytes, or a payload over
    /// [`MIGRATION_MAX_PAYLOAD`] bytes.
    TooLarge,
    /// The reassembled payload does not decode.
    Decode,
}

/// What a receiver surfaces from the migration envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationEvent<T> {
    /// A complete, decoded payload.
    Received(T),
    /// A record was dropped (already counted).
    Rejected(WireError),
}

/// Most surfaced events held per peer until the caller takes them. A full
/// peer's newest record is dropped (and counted by the caller); other peers'
/// queues are unaffected, and a game that never reads them stays bounded.
pub const MAX_PENDING_PER_PEER: usize = 4;

/// Queue `peer`'s event in arrival order, within [`MAX_PENDING_PER_PEER`] for
/// that peer. `false`: the peer's queue is full and the event was dropped.
pub(crate) fn queue_event<K: PartialEq, E>(
    pending: &mut VecDeque<(K, E)>,
    peer: K,
    event: E,
) -> bool {
    if pending.iter().filter(|(p, _)| *p == peer).count() >= MAX_PENDING_PER_PEER {
        log::warn!(
            "[migration] setup event dropped: {MAX_PENDING_PER_PEER} already pending for this peer"
        );
        return false;
    }
    pending.push_back((peer, event));
    true
}

/// Serialize `payload` and split it into parts of at most [`MIGRATION_CHUNK`]
/// bytes. Refuses a payload over [`MIGRATION_MAX_PAYLOAD`] bytes.
pub fn chunk<T: Serialize>(payload: &T) -> Result<Vec<MigrationPart>, WireError> {
    let bytes = encode(payload);
    if bytes.is_empty() || bytes.len() > MIGRATION_MAX_PAYLOAD {
        return Err(WireError::TooLarge);
    }
    let parts = bytes.len().div_ceil(MIGRATION_CHUNK) as u8;
    Ok(bytes
        .chunks(MIGRATION_CHUNK)
        .enumerate()
        .map(|(part, bytes)| MigrationPart {
            part: part as u8,
            parts,
            bytes: bytes.to_vec(),
        })
        .collect())
}

#[derive(Clone, Debug)]
struct Record {
    parts: u8,
    next: u8,
    bytes: Vec<u8>,
}

/// One link's reassembly: at most one record in progress, at most
/// [`MIGRATION_MAX_PAYLOAD`] bytes held.
#[derive(Clone, Debug, Default)]
pub struct Reassembler {
    record: Option<Record>,
    rejected: u64,
}

impl Reassembler {
    /// Add the next part. `Ok(Some(payload))` completes a record, `Ok(None)`
    /// waits for more; an error drops the record in progress and counts it.
    pub fn push<T: DeserializeOwned>(
        &mut self,
        part: MigrationPart,
    ) -> Result<Option<T>, WireError> {
        let result = self.accept(part);
        if result.is_err() {
            self.record = None;
            self.rejected += 1;
        }
        result
    }

    fn accept<T: DeserializeOwned>(&mut self, p: MigrationPart) -> Result<Option<T>, WireError> {
        if p.parts == 0 || p.parts as usize > MIGRATION_MAX_PARTS {
            return Err(WireError::Parts);
        }
        if p.bytes.len() > MIGRATION_CHUNK {
            return Err(WireError::TooLarge);
        }
        let record = self.record.get_or_insert(Record {
            parts: p.parts,
            next: 0,
            bytes: Vec::new(),
        });
        if p.parts != record.parts {
            return Err(WireError::Parts);
        }
        if p.part != record.next {
            return Err(WireError::Order);
        }
        if record.bytes.len() + p.bytes.len() > MIGRATION_MAX_PAYLOAD {
            return Err(WireError::TooLarge);
        }
        record.bytes.extend_from_slice(&p.bytes);
        record.next += 1;
        if record.next < record.parts {
            return Ok(None);
        }
        let bytes = self.record.take().map(|r| r.bytes).unwrap_or_default();
        decode::<T>(&bytes).map(Some).ok_or(WireError::Decode)
    }

    /// Records dropped on this link so far.
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    /// Whether a record is part-way through.
    pub fn in_progress(&self) -> bool {
        self.record.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ToClient, ToClientMigration, ToHost, ToHostMigration, MAX_RELIABLE_LEN};

    fn every_client_payload() -> Vec<ToClientMigration> {
        vec![
            ToClientMigration::Ask,
            ToClientMigration::Offer {
                code: "ABC123".into(),
                descriptor: vec![1; 300],
            },
            ToClientMigration::Setup {
                descriptor: vec![2; 200],
                initial: [3; 32],
            },
            ToClientMigration::Armed {
                armed: vec![4; 120],
                certificate: vec![5; 1004],
            },
            ToClientMigration::Standby {
                addr: "x".repeat(64),
                token: [6; 32],
            },
            ToClientMigration::Switch { token: [7; 32] },
            ToClientMigration::Unprotected {
                reason: "r".repeat(200),
            },
            ToClientMigration::Withdrawn {
                reason: "offer cancelled".into(),
            },
        ]
    }

    fn every_host_payload() -> Vec<ToHostMigration> {
        vec![
            ToHostMigration::Willing,
            ToHostMigration::Consent {
                descriptor: vec![7; 250],
                addr: "192.168.1.20:40000".into(),
                token: [8; 32],
            },
            ToHostMigration::Decline,
            ToHostMigration::Ack { ack: vec![9; 180] },
        ]
    }

    fn reassemble<T: DeserializeOwned>(
        parts: Vec<MigrationPart>,
    ) -> (Reassembler, Result<Option<T>, WireError>) {
        let mut r = Reassembler::default();
        let mut last = Ok(None);
        for p in parts {
            last = r.push::<T>(p);
            if !matches!(last, Ok(None)) {
                break;
            }
        }
        (r, last)
    }

    #[test]
    fn every_payload_variant_round_trips_through_the_envelope() {
        for m in every_client_payload() {
            let parts = chunk(&m).unwrap();
            for p in &parts {
                let wire = encode(&ToClient::Migration(p.clone()));
                assert!(wire.len() <= MAX_RELIABLE_LEN, "{}", wire.len());
                assert_eq!(
                    decode::<ToClient>(&wire),
                    Some(ToClient::Migration(p.clone()))
                );
            }
            let (r, back) = reassemble::<ToClientMigration>(parts);
            assert_eq!(back, Ok(Some(m)));
            assert!(!r.in_progress());
            assert_eq!(r.rejected(), 0);
        }
        for m in every_host_payload() {
            let parts = chunk(&m).unwrap();
            for p in &parts {
                let wire = encode(&ToHost::Migration(p.clone()));
                assert!(wire.len() <= MAX_RELIABLE_LEN);
                assert_eq!(decode::<ToHost>(&wire), Some(ToHost::Migration(p.clone())));
            }
            assert_eq!(reassemble::<ToHostMigration>(parts).1, Ok(Some(m)));
        }
    }

    #[test]
    fn an_eight_human_cert0_chunks_and_reassembles() {
        let armed = ToClientMigration::Armed {
            armed: vec![0xA5; 200],
            certificate: vec![0x5A; 1004],
        };
        let parts = chunk(&armed).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts
            .iter()
            .all(|p| p.parts == 2 && p.bytes.len() <= MIGRATION_CHUNK));
        let mut r = Reassembler::default();
        assert_eq!(r.push::<ToClientMigration>(parts[0].clone()), Ok(None));
        assert!(r.in_progress());
        assert_eq!(
            r.push::<ToClientMigration>(parts[1].clone()),
            Ok(Some(armed))
        );
    }

    #[test]
    fn a_switch_carrying_the_lan_token_fits_one_part() {
        let m = ToClientMigration::Switch { token: [0xFF; 32] };
        let parts = chunk(&m).unwrap();
        assert_eq!(parts.len(), 1);
        assert!(parts[0].bytes.len() <= MIGRATION_CHUNK);
        assert_eq!(reassemble::<ToClientMigration>(parts).1, Ok(Some(m)));
    }

    #[test]
    fn the_chunker_refuses_more_than_four_parts() {
        let fits = ToClientMigration::Setup {
            descriptor: vec![0; MIGRATION_MAX_PAYLOAD - 40],
            initial: [0; 32],
        };
        assert_eq!(chunk(&fits).unwrap().len(), MIGRATION_MAX_PARTS);
        let over = ToClientMigration::Setup {
            descriptor: vec![0; MIGRATION_MAX_PAYLOAD],
            initial: [0; 32],
        };
        assert_eq!(chunk(&over), Err(WireError::TooLarge));
        let way_over = ToHostMigration::Ack {
            ack: vec![0; 10_000],
        };
        assert_eq!(chunk(&way_over), Err(WireError::TooLarge));
    }

    fn three_parts() -> Vec<MigrationPart> {
        let parts = chunk(&ToClientMigration::Setup {
            descriptor: vec![1; 2000],
            initial: [2; 32],
        })
        .unwrap();
        assert_eq!(parts.len(), 3);
        parts
    }

    /// After any error the record is gone, the counter moved and a fresh
    /// record on the same link still reassembles.
    fn assert_dropped_then_recovers(r: &mut Reassembler, rejected: u64) {
        assert!(!r.in_progress());
        assert_eq!(r.rejected(), rejected);
        let m = ToClientMigration::Switch { token: [9; 32] };
        let mut back = None;
        for p in chunk(&m).unwrap() {
            back = r.push::<ToClientMigration>(p).unwrap();
        }
        assert_eq!(back, Some(m));
    }

    #[test]
    fn a_part_out_of_order_drops_the_record() {
        let parts = three_parts();
        // Part 1 with no record open.
        let mut r = Reassembler::default();
        assert_eq!(
            r.push::<ToClientMigration>(parts[1].clone()),
            Err(WireError::Order)
        );
        assert_dropped_then_recovers(&mut r, 1);
        // Part 2 straight after part 0: nothing surfaces, even once part 1 follows.
        let mut r = Reassembler::default();
        assert_eq!(r.push::<ToClientMigration>(parts[0].clone()), Ok(None));
        assert_eq!(
            r.push::<ToClientMigration>(parts[2].clone()),
            Err(WireError::Order)
        );
        assert_eq!(
            r.push::<ToClientMigration>(parts[1].clone()),
            Err(WireError::Order)
        );
        assert_dropped_then_recovers(&mut r, 2);
        // A repeated part.
        let mut r = Reassembler::default();
        assert_eq!(r.push::<ToClientMigration>(parts[0].clone()), Ok(None));
        assert_eq!(
            r.push::<ToClientMigration>(parts[0].clone()),
            Err(WireError::Order)
        );
        assert_dropped_then_recovers(&mut r, 1);
        // `part >= parts`.
        let mut r = Reassembler::default();
        let p = MigrationPart {
            part: 2,
            parts: 2,
            bytes: vec![0],
        };
        assert_eq!(r.push::<ToClientMigration>(p), Err(WireError::Order));
        assert_dropped_then_recovers(&mut r, 1);
    }

    #[test]
    fn a_part_with_different_or_invalid_parts_drops_the_record() {
        let parts = three_parts();
        let mut r = Reassembler::default();
        assert_eq!(r.push::<ToClientMigration>(parts[0].clone()), Ok(None));
        let mut wrong = parts[1].clone();
        wrong.parts = 4;
        assert_eq!(r.push::<ToClientMigration>(wrong), Err(WireError::Parts));
        assert_dropped_then_recovers(&mut r, 1);
        for parts in [0u8, 5, 255] {
            let mut r = Reassembler::default();
            let p = MigrationPart {
                part: 0,
                parts,
                bytes: vec![0],
            };
            assert_eq!(r.push::<ToClientMigration>(p), Err(WireError::Parts));
            assert_dropped_then_recovers(&mut r, 1);
        }
    }

    #[test]
    fn an_oversized_part_drops_the_record() {
        let parts = three_parts();
        let mut r = Reassembler::default();
        assert_eq!(r.push::<ToClientMigration>(parts[0].clone()), Ok(None));
        let big = MigrationPart {
            part: 1,
            parts: 3,
            bytes: vec![0; MIGRATION_CHUNK + 1],
        };
        assert_eq!(r.push::<ToClientMigration>(big), Err(WireError::TooLarge));
        assert_dropped_then_recovers(&mut r, 1);
    }

    #[test]
    fn a_payload_that_does_not_decode_drops_the_record() {
        let mut r = Reassembler::default();
        // An unknown variant tag.
        let p = MigrationPart {
            part: 0,
            parts: 1,
            bytes: vec![200],
        };
        assert_eq!(r.push::<ToClientMigration>(p), Err(WireError::Decode));
        assert_dropped_then_recovers(&mut r, 1);
        // Trailing bytes after a valid payload.
        let mut bytes = encode(&ToHostMigration::Decline);
        bytes.push(0);
        let p = MigrationPart {
            part: 0,
            parts: 1,
            bytes,
        };
        assert_eq!(r.push::<ToHostMigration>(p), Err(WireError::Decode));
        // `Ack` announcing a huge length: refused before reserving it.
        let p = MigrationPart {
            part: 0,
            parts: 1,
            bytes: vec![3, 253, 255, 255, 255, 255, 255, 255, 255, 255],
        };
        assert_eq!(r.push::<ToHostMigration>(p), Err(WireError::Decode));
        // An empty record.
        let p = MigrationPart {
            part: 0,
            parts: 1,
            bytes: vec![],
        };
        assert_eq!(r.push::<ToHostMigration>(p), Err(WireError::Decode));
        assert_dropped_then_recovers(&mut r, 4);
    }

    #[test]
    fn decode_bounds_strings_and_byte_fields() {
        let long_code = ToClientMigration::Offer {
            code: "1234567".into(),
            descriptor: vec![],
        };
        assert!(decode::<ToClientMigration>(&encode(&long_code)).is_none());
        let long_addr = ToClientMigration::Standby {
            addr: "a".repeat(65),
            token: [0; 32],
        };
        assert!(decode::<ToClientMigration>(&encode(&long_addr)).is_none());
        let long_reason = ToClientMigration::Withdrawn {
            reason: "a".repeat(201),
        };
        assert!(decode::<ToClientMigration>(&encode(&long_reason)).is_none());
        let long_consent = ToHostMigration::Consent {
            descriptor: vec![],
            addr: "a".repeat(65),
            token: [0; 32],
        };
        assert!(decode::<ToHostMigration>(&encode(&long_consent)).is_none());
        let long_ack = ToHostMigration::Ack {
            ack: vec![0; MIGRATION_MAX_PAYLOAD + 1],
        };
        assert!(decode::<ToHostMigration>(&encode(&long_ack)).is_none());
        let long_part = MigrationPart {
            part: 0,
            parts: 1,
            bytes: vec![0; MIGRATION_CHUNK + 1],
        };
        assert!(decode::<ToClient>(&encode(&ToClient::Migration(long_part))).is_none());
    }
}
