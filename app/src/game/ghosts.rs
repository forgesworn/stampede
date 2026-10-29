//! Time Trial ghosts (SPEC 17.2): the best run per track is kept on disk and
//! raced against next time as a pale, glowing ostrich with a live +/- split.
//!
//! Files: `<data_dir>/ghosts/<Track>.bin` for the standard 3-lap run (the same
//! rule as the profile's best time), `<Track>_<n>lap.bin` for other lap counts,
//! so a 1-lap warm-up never replaces a 3-lap best. A file is replaced (atomically,
//! through a temp file) only by a faster run; a missing or unreadable one by any.

use super::present::class_scale;
use crate::flow::GhostResult;
use egui::{pos2, Pos2, Vec2};
use glam::Vec3;
use race_core::math::wrap_angle;
use race_core::{Character, Ghost, GhostPose, Track, TrackId, Tunables};
use race_render::{Beam, Burst, BurstKind, Camera, FrameScene, OstrichPose};
use std::path::{Path, PathBuf};

/// Laps of the run whose ghost is `<Track>.bin`.
pub const STD_LAPS: u8 = 3;

/// The ghost directory under the app's data directory.
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("ghosts")
}

/// `Dune.bin` for the standard run, `Dune_1lap.bin` / `Dune_5lap.bin` for the others.
pub fn file_name(track: TrackId, laps: u8) -> String {
    if track == TrackId::Custom {
        if let Some(track) = crate::custom_tracks::active() {
            let hash = track.hash();
            return if laps == track.laps { format!("custom-{hash:016x}.bin") }
                else { format!("custom-{hash:016x}_{laps}lap.bin") };
        }
    }
    if laps == STD_LAPS {
        format!("{track:?}.bin")
    } else {
        format!("{track:?}_{laps}lap.bin")
    }
}

pub fn path(data_dir: &Path, track: TrackId, laps: u8) -> PathBuf {
    dir(data_dir).join(file_name(track, laps))
}

/// The saved ghost for `track` over `laps`, if there is a readable one for this layout and tuning.
pub fn load(data_dir: &Path, track: TrackId, laps: u8, tun: &Tunables) -> Option<Ghost> {
    let bytes = std::fs::read(path(data_dir, track, laps)).ok()?;
    match Ghost::from_bytes(&bytes, track, tun) {
        Ok(g) if g.lap_times.len() == laps as usize || laps != STD_LAPS => Some(g),
        Ok(_) => None,
        Err(e) => {
            log::warn!("ghost: {} is not usable ({e:?})", file_name(track, laps));
            None
        }
    }
}

/// What [`save_if_best`] did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Saved {
    /// This run is the saved ghost now.
    Yes,
    /// The saved ghost is as fast or faster (its time).
    Slower(f32),
    /// The file could not be written.
    Failed,
}

/// Keep `ghost` as the best for its track and `laps` if it beats the saved one.
pub fn save_if_best(data_dir: &Path, ghost: &Ghost, laps: u8) -> Saved {
    let file = path(data_dir, ghost.track, laps);
    if let Some(old) = std::fs::read(&file).ok().and_then(|b| Ghost::decode(&b).ok()) {
        // A file from another layout or tuning is dead weight ( `load` refuses it): it never blocks a save.
        let same = old.track_hash == ghost.track_hash && old.tunables_hash == ghost.tunables_hash && old.lap_times.len() == ghost.lap_times.len();
        if same && old.time <= ghost.time && old.time.is_finite() && old.time > 0.0 {
            return Saved::Slower(old.time);
        }
    }
    if let Err(e) = write_atomic(&file, &ghost.to_bytes()) {
        log::warn!("ghost: could not save {}: {e}", file.display());
        return Saved::Failed;
    }
    Saved::Yes
}

fn write_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension("bin.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // on disk before the rename, so a crash cannot leave an empty best
        f.sync_all()?;
    }
    std::fs::rename(&tmp, file).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A finished Time Trial: save the ghost if it is the best, and say what happened.
pub fn finish_run(data_dir: &Path, laps: u8, ghost: &Ghost, raced: Option<f32>) -> GhostResult {
    let r = save_if_best(data_dir, ghost, laps);
    GhostResult { time: ghost.time, raced, saved: r == Saved::Yes, failed: r == Saved::Failed }
}

/// A loaded ghost with a progress-along-the-track table, so the HUD can say how far ahead or behind it is.
pub struct Playback {
    pub ghost: Ghost,
    /// Track progress ((lap - 1) x length + s, like `Racer::progress`) at each frame; never decreasing.
    prog: Vec<f32>,
}

impl Playback {
    pub fn new(ghost: Ghost, track: &Track) -> Playback {
        let len = track.length;
        let mut prog = Vec::with_capacity(ghost.frames.len());
        let mut hint = 0usize;
        let (mut lap, mut last_s, mut best) = (0i32, f32::NAN, f32::MIN);
        for (k, f) in ghost.frames.iter().enumerate() {
            let pos = Vec3::new(f.pos[0] as f32, f.pos[1] as f32, f.pos[2] as f32) / 100.0;
            let pr = if k == 0 { track.project_global(pos) } else { track.project(pos, hint) };
            hint = pr.idx;
            let s = pr.s;
            if k == 0 {
                // The grid is behind the line: s is near the end of the loop, which is lap 0.
                lap = if s > len * 0.5 { -1 } else { 0 };
            } else if s < last_s - len * 0.5 {
                lap += 1;
            } else if s > last_s + len * 0.5 {
                lap -= 1;
            }
            last_s = s;
            // Racer progress is (lap - 1) x L + s with lap 1 after the first crossing.
            best = best.max(lap as f32 * len + s);
            prog.push(best);
        }
        Playback { ghost, prog }
    }

    pub fn time(&self) -> f32 {
        self.ghost.time
    }

    /// The ghost's pose at race time `t`.
    pub fn pose(&self, t: f32) -> GhostPose {
        self.ghost.pose_at(t.max(0.0))
    }

    /// Race time at which the ghost reached track progress `p` (None before it started or after it finished).
    pub fn time_at(&self, p: f32) -> Option<f32> {
        if self.prog.len() < 2 {
            return None;
        }
        let (first, last) = (*self.prog.first()?, *self.prog.last()?);
        if p < first || p > last {
            return None;
        }
        let k = self.prog.partition_point(|&q| q < p).clamp(1, self.prog.len() - 1);
        let (a, b) = (self.prog[k - 1], self.prog[k]);
        let u = if b > a { ((p - a) / (b - a)).clamp(0.0, 1.0) } else { 0.0 };
        Some((k as f32 - 1.0 + u) * Ghost::INTERVAL)
    }

    /// How far behind (+) or ahead (-) the player is: their race time minus the ghost's time at the same progress.
    pub fn split(&self, t_now: f32, progress_now: f32) -> Option<f32> {
        let g = self.time_at(progress_now)?;
        (t_now > 0.0).then_some(t_now - g)
    }

    /// Progress of the ghost at race time `t` (for tests and the "ahead" side of the HUD).
    pub fn progress_at(&self, t: f32) -> f32 {
        if self.prog.is_empty() {
            return 0.0;
        }
        let x = (t / Ghost::INTERVAL).max(0.0);
        let k = (x.floor() as usize).min(self.prog.len() - 1);
        let k1 = (k + 1).min(self.prog.len() - 1);
        self.prog[k] + (self.prog[k1] - self.prog[k]) * (x - k as f32).clamp(0.0, 1.0)
    }
}

/// `+0.84` / `-0.31`: behind is plus (red), ahead is minus (green).
pub fn fmt_split(s: f32) -> String {
    let v = (s.abs() * 100.0).round() / 100.0;
    format!("{}{:.2}", if s < 0.0 && v > 0.0 { "-" } else { "+" }, v)
}

/// The ghost ostrich, pale blue-white (the lit pipeline is opaque): the pose to push on the scene.
pub fn ostrich_pose(gp: &GhostPose, character: u8, time: f32) -> OstrichPose {
    let class = Character::get(character.min(7)).class;
    let pale = |a: f32, b: f32, c: f32| [a, b, c];
    OstrichPose {
        pos: gp.pos,
        yaw: gp.yaw,
        visual_yaw: wrap_angle(gp.visual_yaw - gp.yaw),
        gait_phase: gp.gait_phase,
        speed: gp.speed,
        airborne: gp.airborne,
        tuck: if gp.airborne { 1.0 } else { 0.0 },
        trick_flip: if gp.trick { (time * 1.6).fract() } else { 0.0 },
        spin: if gp.spun { time * 9.0 } else { 0.0 },
        wheel: if gp.drifting { 1.0 } else { 0.0 },
        scale: class_scale(class),
        body: pale(0.80, 0.92, 1.0),
        neck: pale(0.72, 0.88, 1.0),
        legs: pale(0.62, 0.82, 1.0),
        beak: pale(0.95, 0.99, 1.0),
        accent: pale(0.45, 0.78, 1.0),
        accessory: race_render::Accessory::None,
        ..Default::default()
    }
}

/// Glow and sparkle for the ghost: a soft additive column and a drifting glint, no dust (it has no emitter).
pub fn add_to_scene(scene: &mut FrameScene, pose: OstrichPose, sparkle: bool) {
    let c = pose.pos;
    scene.beams.push(Beam { from: c + Vec3::Y * 0.05, to: c + Vec3::Y * 2.6, width: 0.9, color: [0.4, 0.7, 1.0], intensity: 0.22 });
    if sparkle {
        scene.bursts.push(Burst { kind: BurstKind::CoinSparkle, pos: c + Vec3::Y * 1.4, count: 3 });
    }
    scene.ostriches.push(pose);
}

/// Where a world point lands on screen (UI points inside a screen of `size`), None behind the camera or far off.
pub fn project(cam: &Camera, size: Vec2, p: Vec3) -> Option<Pos2> {
    let f = (cam.target - cam.eye).normalize_or_zero();
    let r = f.cross(cam.up).normalize_or_zero();
    let u = r.cross(f);
    let d = p - cam.eye;
    let z = d.dot(f);
    if z < 1.0 {
        return None;
    }
    let th = (cam.fov_y * 0.5).tan();
    let aspect = size.x / size.y.max(1.0);
    let nx = d.dot(r) / (z * th * aspect);
    let ny = d.dot(u) / (z * th);
    if nx.abs() > 1.4 || ny.abs() > 1.4 {
        return None;
    }
    Some(pos2((nx * 0.5 + 0.5) * size.x, (0.5 - ny * 0.5) * size.y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::source::{LocalRace, RaceSource};
    use race_core::{BotLevel, Input, RaceConfig, RaceMode};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stampede-ghost-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A bot's one-lap Time Trial on Savanna, recorded.
    fn run(track: TrackId, laps: u8, level: BotLevel, seed: u64) -> (Ghost, race_core::Tunables) {
        let mut cfg = RaceConfig::quick(track, 0, BotLevel::Normal, seed);
        cfg.laps = laps;
        cfg.mode = RaceMode::TimeTrial;
        let mut race = LocalRace::new(cfg, Some(0));
        race.set_autopilot(Some(level), crate::game::source::Forced::default());
        race.record_ghost(true);
        for _ in 0..(60 * 400) {
            race.advance(1.0 / 60.0, &mut Input::default);
            if let Some(g) = race.take_ghost() {
                return (g, race.world().tun.clone());
            }
        }
        panic!("the bot did not finish");
    }

    #[test]
    fn file_names_split_standard_and_other_lap_counts() {
        assert_eq!(file_name(TrackId::Dune, 3), "Dune.bin");
        assert_eq!(file_name(TrackId::Dune, 1), "Dune_1lap.bin");
        assert_ne!(file_name(TrackId::Dune, 2), file_name(TrackId::Dune, 5));
    }

    #[test]
    fn save_load_round_trip_and_only_a_faster_run_replaces_it() {
        let dir = tmp("save");
        let (fast, tun) = run(TrackId::Savanna, 2, BotLevel::Hard, 3);
        let (slow, _) = run(TrackId::Savanna, 2, BotLevel::Easy, 3);
        assert!(slow.time > fast.time + 0.1, "{} vs {}", slow.time, fast.time);
        assert!(load(&dir, TrackId::Savanna, 2, &tun).is_none());
        // the first run is saved whatever its time
        assert_eq!(save_if_best(&dir, &slow, 2), Saved::Yes);
        let back = load(&dir, TrackId::Savanna, 2, &tun).expect("loads");
        assert_eq!(back, slow, "bit-exact round trip");
        // slower or equal does not replace
        assert_eq!(save_if_best(&dir, &slow, 2), Saved::Slower(slow.time));
        // faster does
        assert_eq!(save_if_best(&dir, &fast, 2), Saved::Yes);
        assert_eq!(load(&dir, TrackId::Savanna, 2, &tun).unwrap().time, fast.time);
        assert_eq!(save_if_best(&dir, &slow, 2), Saved::Slower(fast.time), "the slow one cannot come back");
        // no temp file is left behind, other lap counts have their own file
        let names: Vec<String> = std::fs::read_dir(super::dir(&dir)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(names, vec!["Savanna_2lap.bin".to_string()]);
        assert!(load(&dir, TrackId::Savanna, 1, &tun).is_none());
        // garbage on disk is ignored and replaced by the next run
        std::fs::write(path(&dir, TrackId::Savanna, 2), b"nonsense").unwrap();
        assert!(load(&dir, TrackId::Savanna, 2, &tun).is_none());
        assert_eq!(save_if_best(&dir, &slow, 2), Saved::Yes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_file_from_another_layout_never_blocks_a_save() {
        let dir = tmp("stale");
        let (mut old, tun) = run(TrackId::Savanna, 2, BotLevel::Hard, 3);
        let (new, _) = run(TrackId::Savanna, 2, BotLevel::Easy, 3);
        old.tunables_hash ^= 1; // recorded under other tuning, and faster
        assert!(old.time < new.time);
        write_atomic(&path(&dir, TrackId::Savanna, 2), &old.to_bytes()).unwrap();
        assert!(load(&dir, TrackId::Savanna, 2, &tun).is_none(), "refused on load");
        assert_eq!(save_if_best(&dir, &new, 2), Saved::Yes, "so it must be replaceable");
        assert!(load(&dir, TrackId::Savanna, 2, &tun).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_is_reported_not_called_a_win() {
        let dir = tmp("fail");
        let (g, _) = run(TrackId::Savanna, 1, BotLevel::Normal, 2);
        // a file where the ghosts directory should be: nothing can be written
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ghosts"), b"in the way").unwrap();
        let r = finish_run(&dir, 1, &g, None);
        assert!(r.failed && !r.saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_ghost_for_another_tuning_is_refused() {
        let dir = tmp("tun");
        let (g, tun) = run(TrackId::Savanna, 1, BotLevel::Normal, 5);
        save_if_best(&dir, &g, 1);
        let mut other = tun.clone();
        other.top_speed += 1.0;
        assert!(load(&dir, TrackId::Savanna, 1, &other).is_none());
        assert!(load(&dir, TrackId::Savanna, 1, &tun).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playback_progress_follows_the_run_and_the_split_is_zero_against_itself() {
        let (g, _) = run(TrackId::Savanna, 2, BotLevel::Normal, 4);
        let track = TrackId::Savanna.track();
        let pb = Playback::new(g, &track);
        let len = track.length;
        // starts behind the line and ends after two laps
        assert!(pb.progress_at(0.0) < 0.0 && pb.progress_at(0.0) > -30.0, "{}", pb.progress_at(0.0));
        let end = pb.progress_at(pb.time());
        assert!((end - 2.0 * len).abs() < 12.0, "{end} vs {}", 2.0 * len);
        // time_at inverts progress_at
        for t in [3.0, 20.0, pb.time() * 0.5, pb.time() - 5.0] {
            let p = pb.progress_at(t);
            let back = pb.time_at(p).unwrap();
            assert!((back - t).abs() < 0.35, "t {t} -> p {p} -> {back}");
            let s = pb.split(t, p).unwrap();
            assert!(s.abs() < 0.35, "{s}");
        }
        // a player 0.5 s late at the same place is +0.5
        let p = pb.progress_at(30.0);
        assert!((pb.split(30.5, p).unwrap() - 0.5).abs() < 0.35);
        assert_eq!(pb.time_at(-500.0), None);
    }

    #[test]
    fn the_split_follows_a_live_race_against_a_slower_ghost() {
        let (mut slow, _) = run(TrackId::Savanna, 2, BotLevel::Easy, 6);
        // Item rolls can let an Easy bot beat Hard. Use a guaranteed slower
        // playback clock to test the split rather than random race balance.
        slow.frames = slow.frames.iter().flat_map(|f| [*f, *f]).collect();
        slow.time *= 2.0;
        for t in &mut slow.lap_times { *t *= 2.0; }
        let track = TrackId::Savanna.track();
        let pb = Playback::new(slow, &track);
        let mut cfg = RaceConfig::quick(TrackId::Savanna, 0, BotLevel::Normal, 6);
        cfg.laps = 2;
        cfg.mode = RaceMode::TimeTrial;
        let mut race = LocalRace::new(cfg, Some(0));
        race.set_autopilot(Some(BotLevel::Hard), crate::game::source::Forced::default());
        let mut last = None;
        for _ in 0..(60 * 70) {
            race.advance(1.0 / 60.0, &mut Input::default);
            let v = race.view();
            if let Some(s) = pb.split(v.race_time, v.racers[0].progress) {
                last = Some(s);
            }
        }
        let s = last.expect("a split was measured");
        assert!(s < -0.2, "the hard bot pulls ahead of the easy ghost: {s}");
    }

    #[test]
    fn split_text() {
        assert_eq!(fmt_split(0.844), "+0.84");
        assert_eq!(fmt_split(-0.309), "-0.31");
        assert_eq!(fmt_split(-0.001), "+0.00");
        assert_eq!(fmt_split(12.0), "+12.00");
    }

    #[test]
    fn projection_puts_the_look_at_point_in_the_middle_and_rejects_behind() {
        let cam = Camera { eye: Vec3::new(0.0, 3.0, -10.0), target: Vec3::new(0.0, 1.0, 0.0), up: Vec3::Y, fov_y: 1.0, near: 0.1, far: 500.0 };
        let c = project(&cam, Vec2::new(800.0, 360.0), Vec3::new(0.0, 1.0, 0.0)).unwrap();
        assert!((c.x - 400.0).abs() < 1.0 && (c.y - 180.0).abs() < 1.0, "{c:?}");
        assert!(project(&cam, Vec2::new(800.0, 360.0), Vec3::new(0.0, 1.0, -30.0)).is_none());
        let right = project(&cam, Vec2::new(800.0, 360.0), Vec3::new(-3.0, 1.0, 0.0)).unwrap();
        assert!(right.x > 400.0, "yaw 0 faces +Z, so +x is to the left and -x to the right: {right:?}");
    }
}
