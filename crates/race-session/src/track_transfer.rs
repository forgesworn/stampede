//! Canonical custom tracks, bounded before reassembly or geometry building.

use race_core::{custom, CustomTrack, Track, TrackDef};
use std::sync::Arc;

pub(crate) const CHUNK_BYTES: usize = 900;
// The cap enforced by CustomTrack::from_bytes (600 points, 64 features, hills byte).
pub(crate) const MAX_BYTES: usize = custom::MAX_BYTES;
pub(crate) const MAX_CHUNKS: usize = MAX_BYTES.div_ceil(CHUNK_BYTES);

#[derive(Clone)]
pub(crate) struct SelectedTrack {
    pub track: CustomTrack,
    pub hash: u64,
    pub def: TrackDef,
    pub built: Arc<Track>,
    pub bytes: Vec<u8>,
}

impl SelectedTrack {
    pub fn new(track: CustomTrack) -> Result<Self, String> {
        let def = custom::to_def(&track).map_err(|e| e.to_string())?;
        let bytes = track.to_bytes();
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err("The track is too large.".into());
        }
        Ok(Self { hash: track.hash(), built: Arc::new(Track::build(&def)), track, def, bytes })
    }
}

#[derive(Clone)]
pub(crate) struct Download {
    hash: u64,
    chunks: Vec<Option<Vec<u8>>>,
    size: usize,
}

impl Download {
    pub fn new(hash: u64, total: u8) -> Result<Self, String> {
        if total == 0 || total as usize > MAX_CHUNKS {
            return Err("Bad track chunk count.".into());
        }
        Ok(Self { hash, chunks: vec![None; total as usize], size: 0 })
    }

    pub fn push(&mut self, hash: u64, idx: u8, total: u8, bytes: Vec<u8>) -> Result<Option<SelectedTrack>, String> {
        if hash != self.hash || total as usize != self.chunks.len() || idx as usize >= self.chunks.len() {
            return Err("Inconsistent track chunks.".into());
        }
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES || (idx + 1 < total && bytes.len() != CHUNK_BYTES) {
            return Err("Bad track chunk size.".into());
        }
        let slot = &mut self.chunks[idx as usize];
        if let Some(old) = slot {
            if old != &bytes {
                return Err("Conflicting track chunks.".into());
            }
            return Ok(None);
        }
        if self.size + bytes.len() > MAX_BYTES {
            return Err("The track is too large.".into());
        }
        self.size += bytes.len();
        *slot = Some(bytes);
        if self.chunks.iter().any(Option::is_none) {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(self.size);
        for part in self.chunks.iter().flatten() {
            bytes.extend_from_slice(part);
        }
        let track = CustomTrack::from_bytes(&bytes).map_err(|e| e.to_string())?;
        if track.hash() != self.hash {
            return Err("The track hash does not match.".into());
        }
        SelectedTrack::new(track).map(Some)
    }
}

#[cfg(test)]
pub(crate) fn sample_track(name: &str, radius: f32) -> CustomTrack {
    let raw: Vec<_> = (0..=500).map(|i| {
        let a = i as f32 / 500.0 * std::f32::consts::TAU;
        race_core::Vec2::new(a.cos() * radius, a.sin() * radius)
    }).collect();
    let shaped = race_core::shape(&raw, false, 18.0).unwrap();
    race_core::custom_track_from(&shaped, name, race_core::TrackId::Oasis, 7, 180, vec![], 9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{decode, encode, ToClient, MAX_RELIABLE_LEN};

    fn assemble(track: &CustomTrack, hash: u64) -> Result<SelectedTrack, String> {
        let bytes = track.to_bytes();
        let total = bytes.len().div_ceil(CHUNK_BYTES) as u8;
        let mut download = Download::new(hash, total)?;
        for (idx, bytes) in bytes.chunks(CHUNK_BYTES).enumerate() {
            if let Some(t) = download.push(hash, idx as u8, total, bytes.to_vec())? { return Ok(t); }
        }
        Err("incomplete".into())
    }

    #[test]
    fn bounded_chunks_round_trip_out_of_order_and_duplicates_do_not_count_twice() {
        let t = sample_track("Long loop", 440.0);
        let bytes = t.to_bytes();
        let parts: Vec<_> = bytes.chunks(CHUNK_BYTES).map(|p| p.to_vec()).collect();
        assert!(parts.len() >= 3);
        let total = parts.len() as u8;
        let mut d = Download::new(t.hash(), total).unwrap();
        assert!(d.push(t.hash(), 1, total, parts[1].clone()).unwrap().is_none());
        assert!(d.push(t.hash(), 1, total, parts[1].clone()).unwrap().is_none());
        assert_eq!(d.size, CHUNK_BYTES);
        assert!(d.push(t.hash(), 0, total, parts[0].clone()).unwrap().is_none());
        let back = d.push(t.hash(), 2, total, parts[2].clone()).unwrap().unwrap();
        assert_eq!(back.track, t);
        assert_eq!(back.built.name, t.name);
        let worst = ToClient::TrackData { hash: u64::MAX, idx: u8::MAX, total: u8::MAX, bytes: vec![255; CHUNK_BYTES] };
        let wire = encode(&worst);
        assert!(wire.len() <= MAX_RELIABLE_LEN);
        assert_eq!(decode::<ToClient>(&wire), Some(worst));
    }

    #[test]
    fn hostile_chunk_headers_lengths_conflicts_and_total_allocations_are_bounded() {
        for total in [0, 5, 255] { assert!(Download::new(1, total).is_err()); }
        let mut d = Download::new(1, 4).unwrap();
        assert!(d.push(1, 4, 4, vec![0; CHUNK_BYTES]).is_err());
        assert!(d.push(2, 0, 4, vec![0; CHUNK_BYTES]).is_err());
        assert!(d.push(1, 0, 3, vec![0; CHUNK_BYTES]).is_err());
        assert!(d.push(1, 0, 4, vec![]).is_err());
        assert!(d.push(1, 0, 4, vec![0; CHUNK_BYTES + 1]).is_err());
        assert!(d.push(1, 0, 4, vec![0; CHUNK_BYTES - 1]).is_err());
        assert_eq!(d.size, 0);
        d.push(1, 0, 4, vec![0; CHUNK_BYTES]).unwrap();
        assert!(d.push(1, 0, 4, vec![1; CHUNK_BYTES]).is_err());
        assert_eq!(d.size, CHUNK_BYTES);
        d.push(1, 1, 4, vec![0; CHUNK_BYTES]).unwrap();
        d.push(1, 2, 4, vec![0; CHUNK_BYTES]).unwrap();
        assert!(d.push(1, 3, 4, vec![0; MAX_BYTES - 2700 + 1]).is_err());
        assert_eq!(d.size, 2700);
    }

    #[test]
    fn a_hilly_track_travels_as_format_2() {
        let mut t = sample_track("Hilly loop", 300.0);
        t.hills = 3;
        assert_eq!(t.to_bytes()[0], 2);
        let back = assemble(&t, t.hash()).unwrap();
        assert_eq!(back.track, CustomTrack { v: 2, ..t.clone() });
        assert_eq!(back.hash, t.hash());
    }

    #[test]
    fn receiving_requires_the_hash_canonical_bytes_and_valid_geometry() {
        let t = sample_track("Loop", 95.0);
        assert!(assemble(&t, t.hash() ^ 1).err().unwrap().contains("hash"));
        let mut invalid = t.clone();
        invalid.points.swap(2, 3);
        assert!(invalid.validate().is_ok());
        assert!(assemble(&invalid, invalid.hash()).is_err(), "to_def checks geometry after the matching hash");
        let mut d = Download::new(1, 1).unwrap();
        assert!(d.push(1, 0, 1, vec![0xff; 100]).is_err());
        assert!(d.push(1, 0, 1, vec![0xff; 100]).unwrap().is_none());
    }

    #[test]
    fn wire_decode_rejects_oversized_byte_vectors_before_reserving_capacity() {
        let too_long = encode(&ToClient::TrackData { hash: 1, idx: 0, total: 1, bytes: vec![0; CHUNK_BYTES + 1] });
        assert!(too_long.len() < MAX_RELIABLE_LEN);
        assert!(decode::<ToClient>(&too_long).is_none());
        let mut announced = encode(&ToClient::TrackData { hash: 1, idx: 0, total: 1, bytes: vec![] });
        announced.pop(); // Empty Vec's length; replace it with a u64 varint.
        announced.push(253);
        announced.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(announced.len() < 32);
        assert!(decode::<ToClient>(&announced).is_none());
    }
}
