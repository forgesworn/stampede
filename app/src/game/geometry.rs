//! `race_core::Track` -> `race_render::WorldGeometry`, built once per track and
//! cached. Also the synthetic studio (garage) and podium sets.

use glam::{Vec2, Vec3};
use race_core as core;
use race_core::math::yaw_of;
use race_core::{Feature, Surface, Track, TrackId};
use race_render as rr;
use race_render::{road_flags, BoostPad, Bridge, OpenField, Palette, RoadSample, Scenery, SceneryKind, StartLine, Terrain, WallStyle, WorldGeometry};
use std::cell::RefCell;
use std::sync::Arc;

mod trackside;

thread_local! {
    static CACHE: RefCell<Vec<Option<Arc<WorldGeometry>>>> = const { RefCell::new(Vec::new()) };
    static CUSTOM_CACHE: RefCell<Option<(u64, Arc<WorldGeometry>)>> = const { RefCell::new(None) };
}

/// The tracks with hurdles and gates: the training venues (the oval and the Figure Eight).
fn has_obstacles(track: TrackId) -> bool {
    matches!(track, TrackId::Training | TrackId::Eight)
}

/// Which of a track's obstacle features the world draws: the Training Ground has
/// hurdles and gates, but a race there only uses them in some modes (see
/// `World::hurdles_active`), and drawing ones that do nothing is misleading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Course {
    pub hurdles: bool,
    pub gates: bool,
    /// The Paddock with nothing on it (no windmill, hay or tufts): the ground for a run being
    /// recorded. Ignored on other tracks.
    pub bare: bool,
}

impl Course {
    /// Everything the track has.
    pub const FULL: Course = Course { hurdles: true, gates: true, bare: false };
    /// Neither (podium, scenery only).
    pub const NONE: Course = Course { hurdles: false, gates: false, bare: false };

    /// What a race in `mode` on `track` uses. Tracks without such features always give [`Course::FULL`],
    /// so changing mode there never reloads the world.
    pub fn of(track: TrackId, mode: core::RaceMode) -> Course {
        if !has_obstacles(track) {
            return Course::FULL;
        }
        use core::{ChallengeId, LessonId, RaceMode};
        let hurdles = match mode {
            RaceMode::Training(id) => id == LessonId::Jump,
            RaceMode::Challenge(id) => id != ChallengeId::BashBrawl,
            _ => true,
        };
        Course { hurdles, gates: matches!(mode, RaceMode::Training(_)), bare: false }
    }

    /// The same, on bare ground (the Paddock only).
    pub fn bare(self) -> Course {
        Course { bare: true, ..self }
    }

    fn normalised(self, track: TrackId) -> Course {
        if has_obstacles(track) {
            Course { bare: false, ..self }
        } else {
            Course { bare: self.bare && track == TrackId::Field, ..Course::FULL }
        }
    }

    fn bits(self) -> usize {
        self.hurdles as usize | (self.gates as usize) << 1 | (self.bare as usize) << 2
    }
}

/// The cached geometry for a track with everything it has (built on first use).
pub fn cached(id: TrackId) -> Arc<WorldGeometry> {
    cached_course(id, Course::FULL)
}

/// The cached geometry for a track with only the obstacles in `course`.
pub fn cached_course(id: TrackId, course: Course) -> Arc<WorldGeometry> {
    let course = course.normalised(id);
    if id == TrackId::Custom {
        return CUSTOM_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let generation = core::custom::generation();
            if cache.as_ref().is_none_or(|(g, _)| *g != generation) {
                *cache = Some((generation, Arc::new(build_course(&id.track(), course))));
            }
            cache.as_ref().expect("just filled").1.clone()
        });
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() < TrackId::ALL.len() * 8 {
            c.resize(TrackId::ALL.len() * 8, None);
        }
        let slot = &mut c[id.index() * 8 + course.bits()];
        if slot.is_none() {
            // A bare Paddock is the open arena the run is driven on (no rim, the middle drivable).
            let track = if course.bare { core::Track::open_arena() } else { id.track() };
            *slot = Some(Arc::new(build_course(&track, course)));
        }
        slot.as_ref().expect("just filled").clone()
    })
}

/// Build every track (and race-core's terrain grids). Logs the time.
pub fn warm_all() {
    let t0 = std::time::Instant::now();
    for id in TrackId::ALL {
        let t1 = std::time::Instant::now();
        let _ = cached_course(id, Course::of(id, core::RaceMode::Race));
        log::info!("stage: geometry {:?} built in {:.1} ms", id, t1.elapsed().as_secs_f32() * 1000.0);
    }
    log::info!("stage: warm_up built all tracks in {:.1} ms", t0.elapsed().as_secs_f32() * 1000.0);
}

fn v3(a: [f32; 3]) -> Vec3 {
    Vec3::from_array(a)
}

pub fn palette_of(t: &core::Theme) -> Palette {
    Palette {
        sky_top: t.sky_top,
        sky_horizon: t.sky_horizon,
        fog_color: t.fog,
        fog_density: t.fog_density,
        sun_dir: v3(t.sun_dir),
        sun_color: t.sun_colour,
        ambient_sky: t.ambient_sky,
        ambient_ground: t.ambient_ground,
        ground: t.ground,
        ground_alt: t.ground_alt,
        road: t.road,
        road_stripe: t.road_stripe,
        shoulder: t.shoulder,
        wall: t.wall,
        curb_a: t.curb_a,
        curb_b: t.curb_b,
        night: t.night,
    }
}

fn wall_of(w: core::WallStyle) -> WallStyle {
    match w {
        core::WallStyle::Fence | core::WallStyle::Paddock => WallStyle::FencePosts,
        core::WallStyle::DuneBerm => WallStyle::DuneBerm,
        core::WallStyle::RockWall => WallStyle::RockWall,
        core::WallStyle::NeonRail => WallStyle::NeonRail,
    }
}

fn scenery_kind(k: core::SceneryKind) -> SceneryKind {
    match k {
        core::SceneryKind::Acacia => SceneryKind::Acacia,
        core::SceneryKind::Cactus => SceneryKind::Cactus,
        core::SceneryKind::Rock => SceneryKind::Rock,
        core::SceneryKind::Dune => SceneryKind::Dune,
        core::SceneryKind::Mesa => SceneryKind::Mesa,
        core::SceneryKind::NeonPylon => SceneryKind::NeonPylon,
        core::SceneryKind::GrassTuft => SceneryKind::GrassTuft,
        core::SceneryKind::HayBale => SceneryKind::Bush,
    }
}

/// The road ribbon samples: ramp heights folded into `pos`, banking into
/// `right`/`up`, curbs on tight corners, gaps flagged.
fn road_samples(track: &Track) -> Vec<RoadSample> {
    let n = track.samples.len();
    let curb_at: Vec<bool> = track.samples.iter().map(|s| s.curvature.abs() > 0.006).collect();
    (0..n)
        .map(|i| {
            let s = &track.samples[i];
            let mut flags = 0;
            // Curb a corner, with a little run-in so single samples don't flicker.
            if (0..9).any(|d| curb_at[(i + n + d - 4) % n]) && !matches!(s.surface, Surface::Gap) {
                flags |= road_flags::CURB;
            }
            if s.surface == Surface::Gap {
                flags |= road_flags::GAP;
            }
            let right = s.right * s.bank.cos() - Vec3::Y * s.bank.sin();
            RoadSample { pos: s.pos + Vec3::Y * s.ramp, right, up: s.up, width: s.width, shoulder_l: s.shoulder_l, shoulder_r: s.shoulder_r, flags }
        })
        .collect()
}

fn terrain_of(track: &Track) -> Terrain {
    let (lo, hi) = track.terrain_bounds();
    let ext = Vec2::new(hi.x - lo.x, hi.z - lo.z);
    let cell = (ext.max_element() / 130.0).max(6.0);
    let w = (ext.x / cell).ceil() as usize + 1;
    let h = (ext.y / cell).ceil() as usize + 1;
    let origin = Vec2::new(lo.x, lo.z);
    let mut heights = Vec::with_capacity(w * h);
    for zi in 0..h {
        for xi in 0..w {
            heights.push(track.terrain_height(origin.x + xi as f32 * cell, origin.y + zi as f32 * cell));
        }
    }
    // The Paddock's whole band is drivable ground at the road height: no hills inside the fence.
    if track.id.is_open_field() {
        let y0 = track.samples[0].pos.y;
        let p0 = track.samples[0].pos;
        let outer = p0.x.hypot(p0.z) + track.max_half_band + 0.1 * cell;
        for zi in 0..h {
            for xi in 0..w {
                let (x, z) = (origin.x + xi as f32 * cell, origin.y + zi as f32 * cell);
                let r = x.hypot(z);
                if r <= outer {
                    heights[zi * w + xi] = y0;
                } else if r < outer + 2.0 * cell {
                    // Short blend into the hills so the fence line does not stand on a step.
                    let t = (r - outer) / (2.0 * cell);
                    let hh = &mut heights[zi * w + xi];
                    *hh = y0 + (*hh - y0) * t * t * (3.0 - 2.0 * t);
                }
            }
        }
    }
    // Carve a trench under each gap so the jump reads (race-core's terrain is
    // pinned to the road height near the track).
    for s in track.samples.iter().filter(|s| s.surface == Surface::Gap) {
        let r = s.width * 0.5 + s.shoulder_l.max(s.shoulder_r) + cell;
        let (x0, x1) = (((s.pos.x - r - origin.x) / cell).floor().max(0.0) as usize, (((s.pos.x + r - origin.x) / cell).ceil() as usize).min(w - 1));
        let (z0, z1) = (((s.pos.z - r - origin.y) / cell).floor().max(0.0) as usize, (((s.pos.z + r - origin.y) / cell).ceil() as usize).min(h - 1));
        for zi in z0..=z1 {
            for xi in x0..=x1 {
                let p = Vec2::new(origin.x + xi as f32 * cell, origin.y + zi as f32 * cell);
                if (p - Vec2::new(s.pos.x, s.pos.z)).length() <= r {
                    let hh = &mut heights[zi * w + xi];
                    *hh = hh.min(s.pos.y - 11.0);
                }
            }
        }
    }
    Terrain { origin, cell, w, h, heights }
}

pub fn build(track: &Track) -> WorldGeometry {
    build_course(track, Course::FULL)
}

fn build_course(track: &Track, course: Course) -> WorldGeometry {
    let road = road_samples(track);
    let mut scenery: Vec<Scenery> = track.scenery().iter().map(|s| Scenery { kind: scenery_kind(s.kind), pos: s.pos, yaw: s.yaw, scale: s.scale }).collect();
    let mut pads = Vec::new();
    for f in &track.features {
        match *f {
            Feature::BoostPad { s, lateral, len } => {
                let sm = track.sample_at(s + len * 0.5);
                pads.push(BoostPad { pos: track.point_at(s + len * 0.5, lateral), yaw: yaw_of(sm.tangent), length: len, width: if track.id == TrackId::Custom { sm.width } else { core::PAD_WIDTH } });
            }
            // Proper meshes in race-render: `scale` is the span in metres.
            Feature::Hurdle { s } if course.hurdles => {
                let sm = track.sample_at(s);
                scenery.push(Scenery { kind: SceneryKind::Hurdle, pos: track.point_at(s, 0.0), yaw: yaw_of(sm.tangent), scale: sm.width });
            }
            Feature::Gate { s } if course.gates => {
                let sm = track.sample_at(s);
                scenery.push(Scenery { kind: SceneryKind::Gate, pos: track.point_at(s, 0.0), yaw: yaw_of(sm.tangent), scale: sm.width + 1.6 });
            }
            _ => {}
        }
    }
    let s0 = track.sample_at(0.0);
    let mut palette = palette_of(&track.theme);
    if track.id == TrackId::Oasis {
        // The pale sand clips to white under the noon sun: pull it down a touch, warmer.
        let tone = |c: [f32; 3]| [c[0] * 0.86, c[1] * 0.82, c[2] * 0.76];
        palette.ground = tone(palette.ground);
        palette.ground_alt = tone(palette.ground_alt);
        palette.shoulder = tone(palette.shoulder);
        palette.wall = tone(palette.wall);
    }
    let mut terrain = terrain_of(track);
    let fx = trackside::decorate(track, track.id, &mut terrain, &mut scenery);
    if course.bare {
        // Nothing to run round: the trail and the gate are the only marks on the ground.
        scenery.clear();
    }
    let bridges: Vec<Bridge> = track
        .bridges
        .iter()
        .map(|b| {
            let first = (b.s0 / track.ds).floor() as usize;
            let last = (b.s1 / track.ds).ceil() as usize;
            Bridge { first, last: last.max(first + 1) }
        })
        .collect();
    let field = track.id.is_open_field().then(|| {
        let p0 = s0.pos;
        let rc = p0.x.hypot(p0.z);
        OpenField { centre: Vec3::new(0.0, p0.y, 0.0), island_r: rc - track.max_half_band, outer_r: rc + track.max_half_band, landmark: !course.bare }
    });
    WorldGeometry {
        road,
        closed: true,
        terrain: Some(terrain),
        // No fence round a bare field: nothing to see or to bump into.
        wall: if course.bare { WallStyle::None } else { wall_of(track.theme.wall_style) },
        palette,
        scenery,
        pads,
        start: (!track.id.is_open_field()).then_some(StartLine { pos: s0.pos, right: s0.right, width: s0.width }),
        fx,
        bridges,
        field,
    }
}

/// A spot on a track for staging ostriches: ground point, heading and right vector.
#[derive(Clone, Copy, Debug)]
pub struct Spot {
    pub pos: Vec3,
    pub yaw: f32,
    pub right: Vec3,
}

/// The track's world with three podium blocks on the start straight, plus the
/// spot they stand at (1st in the middle; `right` points to the camera's right
/// when it looks back along `-forward`).
pub fn podium_spot(id: TrackId) -> Spot {
    let track = id.track();
    let s = track.length - 26.0;
    let sm = track.sample_at(s);
    Spot { pos: track.point_at(s, 0.0), yaw: yaw_of(sm.tangent), right: sm.right }
}

pub fn podium(id: TrackId) -> (WorldGeometry, Spot) {
    let track = id.track();
    let mut g = (*cached_course(id, Course::NONE)).clone();
    let spot = podium_spot(id);
    let (s, sm, centre) = (track.length - 26.0, track.sample_at(track.length - 26.0), spot.pos);
    // Blocks: stepped mesas read as podium steps. (scale -> height 16*scale)
    for (k, (off, sc)) in [(0.0f32, 0.09f32), (2.7, 0.065), (-2.7, 0.045)].into_iter().enumerate() {
        let _ = k;
        let p = centre + sm.right * off;
        g.scenery.push(Scenery { kind: SceneryKind::Mesa, pos: Vec3::new(p.x, track.point_at(s, off).y, p.z), yaw: 0.0, scale: sc });
    }
    (g, spot)
}

/// Pedestal heights of the podium blocks (1st, 2nd, 3rd), metres.
pub const PODIUM_H: [f32; 3] = [16.0 * 0.09, 16.0 * 0.065, 16.0 * 0.045];

/// A plain studio: a wide dark road disc under a sunset sky, a few rocks.
pub fn studio() -> WorldGeometry {
    let n = 64;
    let r = 45.0f32;
    let mut road = Vec::new();
    for i in 0..n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        let pos = Vec3::new(r * a.sin(), 0.0, r * a.cos());
        // Tangent direction of a counter-clockwise loop seen from above.
        let fwd = Vec3::new(a.cos(), 0.0, -a.sin());
        let right = fwd.cross(Vec3::Y).normalize();
        road.push(RoadSample { pos, right, up: Vec3::Y, width: 60.0, shoulder_l: 30.0, shoulder_r: 30.0, flags: 0 });
    }
    let pal = Palette {
        sky_top: [0.10, 0.07, 0.30],
        sky_horizon: [0.95, 0.45, 0.18],
        fog_color: [0.55, 0.26, 0.30],
        fog_density: 0.010,
        sun_dir: Vec3::new(-0.5, 0.35, 0.6),
        sun_color: [1.0, 0.78, 0.55],
        ambient_sky: [0.55, 0.45, 0.70],
        ambient_ground: [0.45, 0.28, 0.25],
        ground: [0.36, 0.20, 0.28],
        ground_alt: [0.30, 0.17, 0.26],
        road: [0.30, 0.18, 0.26],
        road_stripe: [0.30, 0.18, 0.26],
        shoulder: [0.38, 0.22, 0.28],
        wall: [0.35, 0.20, 0.30],
        curb_a: [0.97, 0.58, 0.10],
        curb_b: [0.97, 0.58, 0.10],
        night: false,
    };
    let (cell, w, h) = (10.0, 40, 40);
    let origin = Vec2::new(-200.0, -200.0);
    let mut scenery = Vec::new();
    for k in 0..26 {
        let a = k as f32 * 2.399;
        let rad = 55.0 + (k % 5) as f32 * 14.0;
        let kind = if k % 3 == 0 { SceneryKind::Mesa } else if k % 3 == 1 { SceneryKind::Rock } else { SceneryKind::Dune };
        let scale = match kind {
            SceneryKind::Mesa => 1.2,
            SceneryKind::Dune => 2.0,
            _ => 2.5,
        };
        scenery.push(Scenery { kind, pos: Vec3::new(rad * a.sin() * 1.0, 0.0, rad * a.cos() * 1.0), yaw: a, scale });
    }
    WorldGeometry {
        road,
        closed: true,
        terrain: Some(Terrain { origin, cell, w, h, heights: vec![0.0; w * h] }),
        wall: WallStyle::None,
        palette: pal,
        scenery,
        pads: vec![],
        start: None,
        fx: rr::WorldFx::default(),
        bridges: Vec::new(),
        field: None,
    }
}

#[allow(unused)]
fn _use(_: rr::Quality) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn dump_tracks() {
        for id in TrackId::ALL {
            let t = id.track();
            let th = &t.theme;
            println!("== {:?} len {:.0} n {} width0 {:.1} wall {:?} amp {} scale {} rise {} blend {} kill {} gapfloor {}", id, t.length, t.samples.len(), t.samples[0].width, th.wall_style, th.terrain_amp, th.terrain_scale, th.terrain_rise, th.terrain_blend, t.kill_y, t.gap_floor);
            println!("  scenery {:?}", th.scenery);
            let (lo, hi) = t.terrain_bounds();
            println!("  bounds {:?} {:?}", lo, hi);
            // corners
            let n = t.samples.len();
            let mut i = 0;
            while i < n {
                if t.samples[i].curvature.abs() > 0.008 {
                    let s0 = i;
                    let mut pk = 0.0f32;
                    while i < n && t.samples[i].curvature.abs() > 0.008 {
                        pk = pk.max(t.samples[i].curvature.abs());
                        i += 1;
                    }
                    println!("  corner s {}..{} len {} peak R {:.0} dir {}", s0, i, i - s0, 1.0 / pk, if t.samples[s0].curvature > 0.0 { "L" } else { "R" });
                }
                i += 1;
            }
            for f in &t.features {
                println!("  {:?}", f);
            }
        }
    }

    #[test]
    fn geometry_matches_the_track() {
        for id in TrackId::ALL {
            let t = id.track();
            let g = cached(id);
            assert_eq!(g.road.len(), t.samples.len());
            assert!(g.terrain.as_ref().is_some_and(|t| t.heights.iter().all(|h| h.is_finite())));
            assert!(g.road.iter().all(|r| r.pos.is_finite() && r.right.is_finite()));
            assert_eq!(g.pads.len(), t.features.iter().filter(|f| matches!(f, Feature::BoostPad { .. })).count());
        }
        // The canyon has a gap.
        let g = cached(TrackId::Canyon);
        assert!(g.road.iter().any(|r| r.flags & road_flags::GAP != 0));
    }

    #[test]
    fn only_the_training_ground_has_obstacles_and_races_draw_what_they_use() {
        use race_core::{ChallengeId, LessonId, RaceMode};
        for id in TrackId::ALL {
            let has = id.track().features.iter().any(|f| matches!(f, Feature::Hurdle { .. } | Feature::Gate { .. }));
            assert_eq!(has, has_obstacles(id), "{id:?}: Course::of assumes only the training venues have hurdles and gates");
        }
        let count = |c: Course, k: SceneryKind| cached_course(TrackId::Training, c).scenery.iter().filter(|s| s.kind == k).count();
        let bash = Course::of(TrackId::Training, RaceMode::Challenge(ChallengeId::BashBrawl));
        assert_eq!(bash, Course::NONE);
        assert_eq!(count(bash, SceneryKind::Hurdle) + count(bash, SceneryKind::Gate), 0);
        let jump = Course::of(TrackId::Training, RaceMode::Training(LessonId::Jump));
        assert!(count(jump, SceneryKind::Hurdle) > 0 && count(jump, SceneryKind::Gate) > 0);
        // A quick race uses hurdles but never gates; a steering lesson uses gates but no hurdles.
        let quick = Course::of(TrackId::Training, RaceMode::Race);
        assert!(count(quick, SceneryKind::Hurdle) > 0 && count(quick, SceneryKind::Gate) == 0);
        let steer = Course::of(TrackId::Training, RaceMode::Training(LessonId::RunSteer));
        assert!(count(steer, SceneryKind::Hurdle) == 0 && count(steer, SceneryKind::Gate) > 0);
        // Other tracks never reload for a mode change.
        assert_eq!(Course::of(TrackId::Dune, RaceMode::Attract), Course::of(TrackId::Dune, RaceMode::Race));
    }

    #[test]
    fn every_track_gets_its_trackside_decoration() {
        use race_render::FluidKind;
        for id in TrackId::ALL {
            let g = cached(id);
            let kinds = |k: SceneryKind| g.scenery.iter().filter(|s| s.kind == k).count();
            if id == TrackId::Field {
                assert_eq!(kinds(SceneryKind::Stands) + kinds(SceneryKind::Spectator) + kinds(SceneryKind::Flag) + kinds(SceneryKind::Banner), 0);
                assert!(kinds(SceneryKind::HayBale) >= 9);
            } else {
                // Crowd, stands, flags and banners at the start, whatever the theme.
                assert!(kinds(SceneryKind::Stands) >= 4, "{id:?}");
                assert!(kinds(SceneryKind::Spectator) >= 40, "{id:?}");
                assert!(kinds(SceneryKind::Flag) + kinds(SceneryKind::Banner) >= 6, "{id:?}");
            }
            assert!(g.scenery.iter().all(|s| s.pos.is_finite() && s.scale.is_finite() && s.scale > 0.0), "{id:?}");
            assert!(g.fx.fluids.iter().all(|f| f.pos.is_finite() && f.half.is_finite() && f.half.min_element() > 0.0), "{id:?}");
            assert!(g.scenery.len() < 4000, "{id:?}: {} props", g.scenery.len());
        }
        let water = |id| cached(id).fx.fluids.iter().filter(|f| f.kind == FluidKind::Water).count();
        let lava = |id| cached(id).fx.fluids.iter().filter(|f| f.kind == FluidKind::Lava).count();
        assert!(water(TrackId::Baobab) >= 3 && water(TrackId::Oasis) >= 2);
        // Volcano: one lava surface per gap jump, plus lakes.
        assert!(lava(TrackId::Volcano) >= 3);
        assert!(cached(TrackId::Halving).fx.light_trails);
        assert!(cached(TrackId::Canyon).fx.wall_strata && cached(TrackId::Volcano).fx.wall_glow.is_some());
        assert!(!cached(TrackId::Dune).fx.ramps.is_empty());
    }

    #[test]
    fn a_bare_paddock_has_no_scenery_and_other_tracks_ignore_bare() {
        let bare = cached_course(TrackId::Field, Course::FULL.bare());
        assert!(bare.scenery.is_empty());
        assert!(bare.field.is_some_and(|f| !f.landmark));
        assert!(cached(TrackId::Field).field.is_some_and(|f| f.landmark));
        // No fence on the bare field; the Paddock itself keeps its rails.
        assert_eq!(bare.wall, WallStyle::None);
        assert_eq!(cached(TrackId::Field).wall, WallStyle::FencePosts);
        // The middle is drivable ground: no island, and the terrain is flat there.
        let f = bare.field.expect("a field");
        assert!(f.island_r < 0.0 && f.outer_r > 300.0);
        let t = bare.terrain.as_ref().expect("terrain");
        let y0 = t.heights[(t.h / 2) * t.w + t.w / 2];
        let mid = |dx: f32| {
            let x = ((dx - t.origin.x) / t.cell).round() as usize;
            let z = ((0.0 - t.origin.y) / t.cell).round() as usize;
            t.heights[z * t.w + x]
        };
        assert!([0.0f32, 30.0, 100.0, 200.0, 300.0].iter().all(|&x| (mid(x) - y0).abs() < 0.01));
        assert!(!cached(TrackId::Field).scenery.is_empty());
        assert_eq!(Course::FULL.bare().normalised(TrackId::Dune), Course::FULL);
        assert!(!Course::FULL.bare().normalised(TrackId::Training).bare);
    }

    #[test]
    fn podium_blocks_added() {
        let (g, spot) = podium(TrackId::Savanna);
        assert!(g.scenery.len() > cached(TrackId::Savanna).scenery.len());
        assert!(spot.pos.is_finite());
    }
}
