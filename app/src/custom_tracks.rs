//! Saved custom tracks and the track selected for the current race.

use race_core::{custom, CustomTrack};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const MAX_SAVED: usize = 50;
const MAX_FILE: u64 = 64 * 1024;
static ACTIVE: Mutex<Option<CustomTrack>> = Mutex::new(None);

pub fn activate(track: &CustomTrack) -> Result<u64, String> {
    let def = custom::to_def(track).map_err(|e| e.to_string())?;
    custom::install(def);
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(track.clone());
    Ok(track.hash())
}

pub fn active() -> Option<CustomTrack> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn path(data_dir: &Path, hash: u64) -> PathBuf {
    data_dir.join("tracks").join(format!("{hash:016x}.json"))
}

fn read(path: &Path) -> Option<CustomTrack> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE { return None; }
    let track: CustomTrack = serde_json::from_slice(&bytes).ok()?;
    custom::to_def(&track).ok()?;
    Some(track)
}

pub fn load(data_dir: &Path, hash: u64) -> Option<CustomTrack> {
    read(&path(data_dir, hash)).filter(|t| t.hash() == hash)
}

/// Unreadable files stay on disk; they do not appear in the picker.
pub fn list(data_dir: &Path) -> Vec<CustomTrack> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("tracks")) else { return Vec::new() };
    let mut tracks: Vec<_> = entries.flatten().filter_map(|entry| {
        let p = entry.path();
        if p.extension()?.to_str()? != "json" { return None; }
        let hash = u64::from_str_radix(p.file_stem()?.to_str()?, 16).ok()?;
        load(data_dir, hash)
    }).take(MAX_SAVED).collect();
    tracks.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.hash().cmp(&b.hash())));
    tracks
}

pub fn save(data_dir: &Path, track: &CustomTrack) -> Result<u64, String> {
    write_track(data_dir, track, false)
}

/// Write a replacement before removing its old file. A failed write keeps the original.
pub fn replace(data_dir: &Path, old_hash: u64, track: &CustomTrack) -> Result<u64, String> {
    let replacing = load(data_dir, old_hash).is_some();
    let hash = write_track(data_dir, track, replacing)?;
    if replacing && hash != old_hash {
        remove(data_dir, old_hash)?;
    }
    Ok(hash)
}

fn write_track(data_dir: &Path, track: &CustomTrack, replacing: bool) -> Result<u64, String> {
    custom::to_def(track).map_err(|e| e.to_string())?;
    let hash = track.hash();
    let file = path(data_dir, hash);
    if !file.exists() && !replacing {
        let count = std::fs::read_dir(data_dir.join("tracks")).ok().into_iter().flatten()
            .flatten().filter(|e| e.path().extension().is_some_and(|s| s == "json")).count();
        if count >= MAX_SAVED { return Err("My Tracks is full (50 tracks). Remove one before saving.".into()); }
    }
    let bytes = serde_json::to_vec_pretty(track).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE { return Err("This track is too large to save.".into()); }
    std::fs::create_dir_all(data_dir.join("tracks")).map_err(|e| e.to_string())?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, file).map_err(|e| e.to_string())?;
    Ok(hash)
}

pub fn remove(data_dir: &Path, hash: u64) -> Result<(), String> {
    std::fs::remove_file(path(data_dir, hash)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use race_core::{custom_track_from, shape, TrackId, Vec2};

    fn sample() -> CustomTrack {
        let raw: Vec<_> = (0..=160).map(|i| {
            let a = i as f32 / 160.0 * std::f32::consts::TAU;
            Vec2::new(a.cos() * 95.0, a.sin() * 95.0)
        }).collect();
        let shaped = shape(&raw, false, 18.0).unwrap();
        custom_track_from(&shaped, "My loop", TrackId::Savanna, 3, 180, vec![], 1)
    }

    #[test]
    fn saved_tracks_validate_hashes_and_keep_existing_files_at_the_limit() {
        let dir = std::env::temp_dir().join(format!("stampede-tracks-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let track = sample();
        let hash = save(&dir, &track).unwrap();
        assert_eq!(load(&dir, hash), Some(track.clone()));
        assert_eq!(list(&dir), vec![track.clone()]);
        std::fs::copy(path(&dir, hash), path(&dir, hash ^ 1)).unwrap();
        assert!(load(&dir, hash ^ 1).is_none());
        assert_eq!(list(&dir).len(), 1);
        for i in 0..48 { std::fs::write(path(&dir, i), "bad").unwrap(); }
        assert!(save(&dir, &CustomTrack { name: "Another".into(), ..track.clone() }).is_err());
        assert_eq!(save(&dir, &track), Ok(hash));
        remove(&dir, hash).unwrap();
        assert!(load(&dir, hash).is_none());
        std::fs::write(path(&dir, hash), vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert!(load(&dir, hash).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replacing_a_full_slot_keeps_the_original_until_the_new_write_succeeds() {
        let dir = std::env::temp_dir().join(format!("stampede-tracks-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let original = sample();
        let old_hash = save(&dir, &original).unwrap();
        for i in 0..49 { std::fs::write(path(&dir, i), "bad").unwrap(); }
        let edited = CustomTrack { name: "Edited loop".into(), ..original.clone() };
        assert!(save(&dir, &edited).is_err());
        assert!(replace(&dir, 999, &edited).is_err(), "a missing original never bypasses capacity");
        assert!(replace(&dir, 1, &edited).is_err(), "an invalid original never bypasses capacity");
        let invalid = CustomTrack { points: vec![], ..edited.clone() };
        assert!(replace(&dir, old_hash, &invalid).is_err());
        assert_eq!(load(&dir, old_hash), Some(original.clone()));
        let tmp = path(&dir, edited.hash()).with_extension("json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        assert!(replace(&dir, old_hash, &edited).is_err());
        assert_eq!(load(&dir, old_hash), Some(original));
        std::fs::remove_dir(tmp).unwrap();
        let new_hash = replace(&dir, old_hash, &edited).unwrap();
        assert!(load(&dir, old_hash).is_none());
        assert_eq!(load(&dir, new_hash), Some(edited.clone()));
        assert_eq!(replace(&dir, new_hash, &edited), Ok(new_hash));
        assert_eq!(std::fs::read_dir(dir.join("tracks")).unwrap().count(), 50);
        let _ = std::fs::remove_dir_all(dir);
    }

}
