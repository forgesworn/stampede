# race-core API

Pure race sim for Stampede (spec sections 3-10 and 16-20). No IO, no wall clock, no
threads (one process-wide track cache). Deps: glam 0.29 (serde), serde,
rand 0.8 (no default features), rand_pcg 0.3, bincode 1.3 (ghost files). Every public data type derives
`Clone, Debug, Serialize, Deserialize`. Everything is re-exported at the crate
root (`use race_core::*;`).

The October gameplay revision is specified in SPEC §20. After GO, motion is
always full throttle; throttle/brake fields remain for countdown and wire
compatibility. Lasers hit rivals on a forward ray, never eggs. Eggs are contact
pickups; rows contain 3-8 eggs and exactly one bad egg. `playful::Balance` is
`Tunables.playful`; `Racer.effects` carries burn, splat, pill, trump, laptop, cake eating,
rose seller and tumble timers (the tumble is no longer raised by rugs or Pepe; they unseat the rider), plus the `knock` kind. `World.play` carries the global scrambler and up to
three frogs and at most two player-placed rugs. The Orange Pill no longer steers: the player does, drifting is allowed, and
`Racer::top_speed` is x`playful::PILL_SPEED` (1.25) while `effects.pill > 0` (a constant, not a `Balance` field,
because `Balance` is hashed into ghosts); it stays invincible with the glow and `pill_time` (7 s).
`playful::autosteer` is pure and steers only the running jockey (`effects.dismount`);
`playful::laying_phase` drives the non-collidable egg-layer presentation.

## Coordinates and clock

- Metres, seconds, radians. Right-handed, **Y up**. Yaw 0 faces **+Z**.
- `forward(yaw) = (sin yaw, 0, cos yaw)`, `right(yaw) = (-cos yaw, 0, sin yaw)`.
  Positive steer turns right = yaw decreases.
- A true top-down map is screen `x = -world.x`, screen `y = -world.z` (SVG y down);
  `examples/track_svg.rs` draws it that way.
- Fixed step `DT = 1/60`, `TICK_HZ = 60`. Deterministic for a seed + input stream
  (f32, `Pcg32`, no hash-map order).
- Colours in `Theme` and `Character` are **sRGB 0..1**.
- `Racer::pos` is the **feet**. Body centre is `pos + Y*BODY_HEIGHT` (1.3),
  eyes `pos + Y*EYE_HEIGHT (2.35) + head_dir*EYE_FORWARD (0.55)`.
- Lateral offsets: **+ = right** of the centreline (looking along +s).

## math

```rust
pub const TICK_HZ: u32;  pub const DT: f32;
pub fn forward(yaw: f32) -> Vec3;      // unit heading
pub fn right(yaw: f32) -> Vec3;        // forward x up
pub fn yaw_of(dir: Vec3) -> f32;       // inverse of forward
pub fn wrap_angle(a: f32) -> f32;      // (-PI, PI]
pub fn approach(cur, target, step) -> f32;  pub fn lerp(a, b, t) -> f32;
pub fn smoothstep(e0, e1, x) -> f32;   pub fn flat(v: Vec3) -> Vec3;  // y = 0
pub fn mix64(x: u64) -> u64;           // splitmix, for derived seeds
```

## Input

```rust
pub struct Input { steer: f32 /*-1..1*/, throttle: f32 /*0..1*/, brake, drift, jump, laser, item, bash, look_back: bool }
Input::go() -> Input                      // throttle 1
Input::pack(&self) -> [u8; 4]             // steer i8, throttle u8, button bits, 0
Input::unpack([u8; 4]) -> Input
Input::without_edges(&self) -> Input      // jump/item/bash cleared (repeat a stale input)
```
Buttons are held levels; the sim edge-detects jump, drift and item
against `Racer::prev_input`. The legacy `bash` field is ignored by the world;
players and bots cannot bash. Its enum, profile and pose slots remain reserved
so existing saves keep their indices. Bash training and challenges are hidden.

## Tunables, classes, characters

```rust
pub struct Tunables { ...every number in spec 4-8... }   // #[serde(default)], Default = spec
pub enum CharClass { Light, Medium, Heavy }
CharClass::mods(self) -> ClassMods { accel, turn, top, bash, mass }
pub struct Character { id: u8, name: &'static str, class, body, neck, legs, beak, accent: [f32;3], accessory: Accessory }
pub enum Accessory { Scarf, Goggles, Crown, StarHeadband, VikingHelmet, Mohawk, GoldChainShades, Cap }
pub const CHARACTERS: [Character; 8];     // Dusty, Blaze, Satoshi, Nova, Boomer, Ziggy, Hodl, Pip
Character::get(id: u8) -> &'static Character   // serde: a Character (de)serialises as its id
```
Changed from the spec / added (all tunable): `accel_floor = 0.25` (accel =
`accel x max(1 - v/top, accel_floor)`, so top speed is actually reached in ~3.2 s;
0 restores the pure linear curve, which only gets to 90 % in 3 s and never to
100 %), `grip_redirect = 0.9` (lateral speed removed by grip goes to forward
speed instead of being lost; 0 = pure scrub), `boost_push = 30`, plus every
interaction constant (bash, laser, items, coins, respawn, bot speeds).

## Tracks

```rust
pub enum TrackId { Savanna, Dune, Canyon, Midnight, Training, Baobab, Volcano, Oasis, Halving, DustBowl, MempoolPlaza }
TrackId::RACE: [TrackId; 4];      // Cup 1: Savanna, Dune, Canyon, Midnight (unchanged)
TrackId::CUP2: [TrackId; 4];      // Cup 2: Baobab, Volcano, Oasis, Halving
TrackId::RACE_ALL: [TrackId; 8];  // Cup 1 then Cup 2 (no arenas)
TrackId::ARENAS: [TrackId; 2];    // battle arenas: DustBowl, MempoolPlaza (not in RACE / CUP2 / RACE_ALL)
TrackId::ALL: [TrackId; 11]       // every track, discriminant order; earlier ones keep their indices
TrackId::is_arena(self) -> bool
TrackId::index(self) -> usize;  TrackId::from_index(usize) -> Option<TrackId>
TrackId::def(self) -> TrackDef            // authored data
TrackId::track(self) -> Arc<Track>        // built once per process, cached
pub fn track_defs() -> Vec<TrackDef>      // all 11, in TrackId order
pub fn turtle(segs: &[Seg], width, bank_k) -> (Vec<TrackPoint>, Vec<f32> /*segment start s*/)
pub enum Seg { S(len, y_end), T(deg /*+ left*/, radius, y_end) }   // authoring helper (serde)

pub struct TrackDef { id, name: String, theme: Theme, laps: u8, points: Vec<TrackPoint>, shoulder: [f32;2], features: Vec<Feature>, seed: u64 }
pub struct TrackPoint { pos: Vec3, width: f32, bank: f32 /*rad, + lowers right edge*/ }
pub enum Feature {
    Ramp { s, len, height, lip },        // kicker across the road: rises over len, falls over lip (0 = sheer)
    BoostPad { s, lateral, len },        // PAD_WIDTH (4.5 m) wide
    TargetRow { s, count },              // Power Eggs across the road, TARGET_HEIGHT (0.55 m) up; count is derived from road width
    CoinLine { s, lateral, count, spacing },   // coins at COIN_HEIGHT (1.0 m)
    Gap { s, len },                      // no ground: falling in = respawn
    Hurdle { s },                        // log, HURDLE_HEIGHT 0.6 m (active: World::hurdles_active())
    Gate { s },                          // training arch
}
pub struct Theme { sky_top, sky_horizon, fog: [f32;3], fog_density: f32, sun_dir, sun_colour, ambient_sky,
    ambient_ground, ground, ground_alt, road, road_stripe, shoulder, wall, curb_a, curb_b: [f32;3],
    night: bool, wall_style: WallStyle, scenery: Vec<(SceneryKind, f32 /*per 100 m*/)>,
    terrain_amp, terrain_scale, terrain_rise, terrain_blend: f32 }
pub enum WallStyle { Fence, DuneBerm, RockWall, NeonRail, Paddock }
pub enum SceneryKind { Acacia, Cactus, Rock, Dune, Mesa, NeonPylon, GrassTuft, HayBale }
pub struct SceneryItem { kind, pos: Vec3 /*base on terrain*/, yaw, scale }

pub struct Track { id, name, theme, laps, samples: Vec<TrackSample>, length: f32, ds: f32 /*~1 m*/,
    racing_line: Vec<LinePoint>, features: Vec<Feature>, kill_y: f32, gap_floor: f32, max_half_band: f32, seed }
pub struct TrackSample { s, pos /*road centre, base height*/, tangent, right /*horizontal*/, up, width,
    shoulder_l, shoulder_r, bank, curvature /*1/m, + left*/, ramp /*height above base*/, surface: Surface }
pub enum Surface { Road, Ramp, Gap }
pub struct LinePoint { pos, offset /*lateral*/, curvature }   // one per sample
pub struct Projection { s, lateral, idx /*next hint*/, u, ground_y }

Track::build(&TrackDef) -> Track
Track::project(&self, pos, hint: usize) -> Projection    // walks from hint: O(1), never jumps sections
Track::locate(&self, pos) -> usize                       // brute-force nearest sample (cold start)
Track::project_global(&self, pos) -> Projection
Track::ground_height(&self, pos, hint) -> f32            // road (+bank +ramp), shoulder, gap floor, terrain
Track::ground_at(&self, &Projection, pos) -> f32
Track::terrain_height(&self, x, z) -> f32                // anywhere; pinned to track height near it
Track::distance_to_centreline(&self, x, z) -> f32
Track::sample_at(&self, s) -> TrackSample                // interpolated
Track::point_at(&self, s, lateral) -> Vec3               // on the ground
Track::half_band(&self, s, lateral) -> f32               // half road + shoulder on that side (= wall)
Track::idx_at(&self, s) -> usize;  wrap_s(s);  s_delta(a, b) /*signed, shortest*/;  s_in(s, a, len)
Track::bounds(&self) -> (Vec3, Vec3)                     // drivable band incl. walls
Track::terrain_bounds(&self) -> (Vec3, Vec3)             // + TERRAIN_MARGIN (160 m)
Track::checkpoints(&self) -> [f32; 4]                    // 0, L/4, L/2, 3L/4
Track::scenery(&self) -> Vec<SceneryItem>                // deterministic, outside the walls
Track::n(&self) -> usize
```
Walls stand at `|lateral| = width/2 + shoulder`, vertical, no ceiling. Shoulders
are off road (`offroad_top`) and rise `SHOULDER_RISE` (6 %) per metre.

| Track | Length | Road / shoulder | Character |
|---|---|---|---|
| Savanna Sprint | 1100 m | 18 / 4.5 | sweepers R 62-90, 1 ramp (boost pad before it), 2 pads |
| Dune Drift | 1300 m | 16 / 5 sand | 3 hairpins R 17-18, 2 sand ramps, dunes |
| Red Rock Canyon | 1399 m | 13 / 2.5 | climbs to +17 m and drops, hairpin, S-bends, boost pad + ramp + 11 m gap |
| Mempool Midnight | 1585 m | 16 / 3 | flat, two real chicanes, 7 boost pads, night |
| Training Ground | 491 m | 16 / 4 | oval: 8 gates, 5 hurdles, ramp, pad, 3 target rows, coins |
| Baobab Bayou (Cup 2) | 1362 m | 15 / 3.5 | dusk wetlands, low mist; S-curves, 4-arc esses (R 36), long left sweeper, one R 40 corner, 3 pads, small ramp |
| Volcano Vault (Cup 2) | 1459 m | 12.5 / 2.5 | hardest: two peaks and a crater (hairpins R 25-27), climbs to +19 m, 258 m plunge, **two 11 m gaps** each with a pad before |
| Oasis Run (Cup 2) | 1228 m | 19 / 5 sand | midday desert, fast sweepers R 60-90 (no drifting), 3 ramps, 14 coin lines |
| Halving Highway (Cup 2) | 1898 m | 17 / 3 | night neon; 190-410 m straights, 3 hairpins R 26 + one R 78 sweep, 10 pads, mega ramp (4 m) near the end |
| Dust Bowl (arena) | 364 m | 40 / 3 | day desert bowl: rounded square (4 x R 35) round a raised mesa island (terrain rise 9 m, blend 7 m: the island and the bowl rim are terrain); 2 kickers, 2 sand mounds (soft ramps), 2 pads, 8 rows of 5 eggs, 10 coin lines |
| Mempool Plaza (arena) | 401 m | 40 / 3 | night neon: square loop (4 x R 32) round a pillar island ringed by neon pylons; 8 boost pads, 2 kickers, 8 rows of 5 eggs, 8 coin lines |

## Custom tracks (§19)

`race_core::custom` (types and the main functions are also re-exported at the
crate root). Plan coordinates are metres in a `Vec2`, mapped to the world as
`(x, y) -> (x, height, z = y)`; lateral offsets are `+` = right, as in `Feature`.

```rust
// Data (serde; also a canonical byte form)
pub struct CustomTrack { v: u8 /*1, or 2 with hills*/, name: String /*<=24 chars*/, look: TrackId /*one of RACE_ALL*/, laps: u8 /*1..=9*/,
    width_dm: u8 /*140..=220*/, points: Vec<[i16;2]> /*dm, closed, ~5 m apart, <=600*/, features: Vec<CustomFeature> /*<=64*/, seed: u32,
    hills: u8 /*0..=HILLS_MAX (3); #[serde(default)] so older saves load flat*/ }
pub struct CustomFeature { kind: CustomFeatureKind, s_dm: u16, lateral_dm: i16 }   // s(), lateral() in metres
pub enum CustomFeatureKind { Eggs, Ramp, Gap, Log, Boost, Coins }                   // ::ALL, wire order
CustomTrack::validate(&self) -> Result<(), CustomError>          // ranges and counts only
CustomTrack::to_bytes(&self) -> Vec<u8>                          // canonical, little-endian, stable; what hash() covers
CustomTrack::from_bytes(&[u8]) -> Result<CustomTrack, CustomError>   // rejects truncated/oversized/trailing/invalid, never panics
CustomTrack::hash(&self) -> u64                                  // FNV-1a 64 of to_bytes(): key for bests, ghosts, file name, transfer
CustomTrack::points_m() -> Vec<Vec2>;  width() -> f32;  to_def() -> Result<TrackDef, CustomError>
CustomTrack::shaped() -> Result<Shaped, CustomError> // analyse saved points without resmoothing
pub enum CustomError { Version(u8), Name, Look, Laps, Width, Hills, Points, Feature(usize), Malformed(&'static str), Shape(ShapeError) }

// Shaping (a finger stroke or a recorded run, in metres)
pub fn shape(raw: &[Vec2], auto_close: bool, width: f32 /*m*/) -> Result<Shaped, ShapeError>
pub struct Shaped { points: Vec<Vec2> /*5 m, closed, dm grid, [0] = start facing [1]*/, length, width,
    crossings: Vec<ShapedCrossing { a_idx, b_idx, pos, angle_deg, s_high, s_low }>,   // <= 2; s_high is the branch that rises
    corners: Vec<Corner { idx, radius, inside_sign: i8 /*+1 left; inside lateral = -inside_sign*d*/ }>,
    straights: Vec<(start_idx, end_idx, len)> /*>= 40 m, indices inclusive, may wrap*/ }
Shaped::restart(idx, flip: bool) -> Result<Shaped, ShapeError>   // move the start / reverse direction; re-checks
Shaped::point_at(s) -> Vec2;  dir_at(s) -> Vec2;  nearest(p) -> (s, dist);  spacing() -> f32
pub enum ShapeError { NotClosed{gap}, TooShort{len}, TooLong{len}, TooManyCrossings{at}, ShallowCrossing{at},
    TooClose{at}, TooTight{at}, StartOnBridge{at} }               // at: Vec2 (plan metres); ShapeError::at() -> Option<Vec2>
pub fn min_radius(points: &[Vec2]) -> f32                        // 3-point circumradius; shaped tracks are >= MIN_RADIUS (14)

// Features
pub fn auto_fill(&Shaped, seed: u32) -> Vec<CustomFeature>       // deterministic, sorted by s_dm, every one passes place_ok
pub fn place_ok(&Shaped, existing: &[CustomFeature], kind: CustomFeatureKind, s: f32 /*m*/) -> Result<(), PlaceError>
pub enum PlaceError { OutOfRange, NearStart, NearFeature(usize), NearCrossing, NeedsStraight }

// Saving and converting
pub fn custom_track_from(&Shaped, name: impl Into<String>, look: TrackId, laps: u8, width_dm: u8,
    features: Vec<CustomFeature>, seed: u32) -> CustomTrack       // quantises to dm; clamps laps/width; bad look -> Savanna
pub fn to_def(&CustomTrack) -> Result<TrackDef, CustomError>     // validates (also safe on network input)

// Registry (process-wide, thread-safe)
custom::install(def: TrackDef) -> u64      // forces id = Custom, builds, returns the new generation (starts at 1)
custom::generation() -> u64                // 0 = never installed
custom::current() -> Option<(u64, Arc<Track>)>
TrackId::Custom                            // NOT in TrackId::ALL; index() == ALL.len(); from_index(ALL.len()) == Some(Custom)
TrackId::Custom.def() / .track()           // the installed one; with nothing installed: the Training Ground's

// Run mode
Recorder::new(start: Vec2, dir: Vec2);  push(&mut self, pos: Vec2) -> RecorderStatus;  points() -> &[Vec2];  len();  status();  is_empty()
pub enum RecorderStatus { Recording { len: f32 }, Closed, TooLong }
```

**Shaping** (`shape`): drops near-duplicates and trims a tail that overshoots the
start; ends over 25 m apart need `auto_close` (a smooth Hermite join) or give
`NotClosed`, a smaller gap is always joined smoothly. The loop is denoised,
resampled to 5 m, tiny loops (self-crossings under 60 m along the lap) are cut
out, and only the too-tight spots are smoothed until every 3-point radius is at
least 14 m (checked on the decimetre grid; `TooTight` if it will not converge).
The lap must be 300-3000 m. Up to 2 crossings of at least 25 degrees are kept;
three or more is `TooManyCrossings`, a shallower one `ShallowCrossing`. Two
non-crossing road sections closer than `width + 4` m (checked before smoothing,
so a pinched hairpin reports this and not `TooTight`) give `TooClose`, as does
a crossing too near its own other branch or another crossing for the ramps.
Point 0 is the point nearest the stroke's start and the direction is the
stroke's. `Track::build` on `to_def`'s output finds exactly the crossings
`shape` reported (width + 4 versus the 18.1 rule's width + 1 leaves a margin).

**Bridges** (`to_def`): flat everywhere except at each crossing, where one
branch rises to 7 m (`BRIDGE_HEIGHT`) with a flat crown of +-4 m and cosine ramps
of 80 m (`BRIDGE_RAMP`), grade at most 13.7 %. The rising branch is the later one
along the lap, unless that would put the start line within `BRIDGE_REACH + 30`
(114 m) of the ramp, then the earlier one; if neither fits, `StartOnBridge`
(use `Shaped::restart`). Ramps of different crossings never overlap and neither
branch of one crossing lies on the other's ramp.

**Feature placement**: manual anchors may overlap and can be placed anywhere
along the road, including near the start and bridges. Finite positions, road
width, feature count and geometric/network bounds still apply. The designer
uses a separate Remove tool, so tapping near a feature adds another one.
Automatic filling retains its 40 m spacing, start/bridge clearances and straight
run-ups for gaps. `to_def` mappings: Eggs -> `TargetRow` 4; Ramp -> the Savanna
kicker (`Ramp` 12 m, 1.5 m, lip 1); Gap -> a wrapped 13 m run-up and 11 m gap;
Log -> `Hurdle`; Boost -> a centred 6 m pad covering the full custom-road width;
Coins -> `CoinLine` 6 at 3 m, at the authored lateral position.
`auto_fill`: a boost pad 25 m into the longest straight, a kicker ramp in the
middle of the second-longest if it is at least 80 m, an egg row about every
250 m on straights (at least one per lap), a coin line on the inside of each of
the three tightest corners.

**Hills**: `CustomTrack.hills` is 0 (flat), 1 (rolling), 2 (hilly) or 3 (steep).
Wire format: `hills == 0` encodes exactly as before (version byte 1, so existing
hashes and saves are unchanged); `hills` 1..=3 is version byte 2 with the same
layout plus one trailing byte, the level. `to_bytes` derives the version byte
from `hills` (it ignores `v`); `from_bytes` accepts 1 or 2, rejects a level of 0
or above 3 in version 2 (`Malformed`), and sets `v` to the version it read.
`validate` accepts `v` 1 or 2 and `hills <= HILLS_MAX` (else `Hills`). The
maximum encoded length is 2831 bytes.
`custom_track_from` always returns `hills: 0`; set the field afterwards.

```rust
pub const HILLS_MAX: u8 = 3;
pub fn hill_heights(track: &CustomTrack, shaped: &Shaped) -> Vec<f32>   // one height (m) per shaped point
```

`hill_heights` is deterministic from `seed` and `hills`: zeros when flat, else a
sum of up to eight lap harmonics (`floor(lap / 180 m)`, at least 1), so the
lap closes. It is faded to flat within 50 m (plus a 40 m smoothstep) of the
start line, within `BRIDGE_REACH + 6` m of both branches of each crossing, and
within 30 m of each Gap jump and its run-up. It is then scaled so the
peak-to-peak height and the steepest grade between neighbouring points stay
within the level's limits: level 1 4 m / 5 %, level 2 9 m / 8 %, level 3
15 m / 12 %. A lap with no room for hills (peak-to-peak under 1 mm) gets zeros.
`to_def` adds these to the bridge profile (bridges stay 7 m); `bank` stays 0.

**Recorder**: `push` records a point every 2 m (longer jumps are filled in
along the line). It closes (`Closed`) when the path is at least 300 m and the
runner crosses the start line (going the way the start faces) within 12 m of the
start heading within 70 degrees of the start direction; over 3000 m it is
`TooLong`. Both are sticky. `shape(rec.points(), false, width)` then turns it
into a track.

Notes: a track saved at one width must be shaped for that width (`to_def`
re-runs the checks with the saved `width_dm`). `World::with_tunables` clamps
`RaceConfig.laps` to 1..=5, so `CustomTrack.laps` of 6-9 is capped to 5 in a
race. `install` rebuilds the `Track`, so geometry caches key on `generation()`.

## Racer

```rust
pub struct Racer {
  idx, character: u8, class: CharClass,
  pos, vel: Vec3, yaw, visual_yaw /*body incl. drift angle*/, yaw_rate, speed /*signed, along heading*/,
  on_ground: bool, air_h /*height above ground*/, air_peak, air_t, from_ramp,
  track_idx: u32 /*projection hint*/, s, lateral, ground_y, offroad,
  drift: Drift { active, hop, dir: i8 /*-1 left +1 right*/, charge, tier: u8 /*0-3*/,
    blend_q, carry_q, over_ticks, pending, pending_ticks, pending_charge_q /*0.4.9 feel state, see below*/ },
  boost_t, boost_tier: u8 /*0 full strength, 1-3 = exit boost of that drift tier*/, stun_t /*spun out*/, spin_rate, slow_t, slow_mult, shield_t, stumble_t, mega_t, magnet_t,
  trick_t /*trick anim*/, trick_armed, trick_pending, respawn_t /*fade; frozen while > 0*/, landing_t,
  coins: u32 /*current race only*/, item: Option<Item>, item_uses: u8,
  effects: playful::Effects, attire: [u8; 3] /*colour, ostrich, jockey; visual only*/,
  lap: u8 /*0 on the grid, 1 after crossing the line at the start*/, next_cp, progress, place: u8 /*1-based*/,
  finished_at: Option<f32>,
  laser: Laser { firing, heat /*0..1*/, overheated, lock_t, origin, aim, hit: Vec3, hit_kind: LaserHit, on_racer, on_racer_t },
  bash_cd, bash_t, bash_side, bash_anim /*-1..1 signed by side*/, look_back /*0..1 eased 4/s*/,
  gait_phase /*0..1, stride 2.6 m*/, dust /*0..1*/, wrong_way, wrong_way_t,
  speed_scale, prev_input: Input, dazzle_imm: [u8; 8], stuck_t, launch_t,
}
pub enum LaserHit { None, Air, Wall, Ground, Target(u16), Racer(u8) }

Racer::new(idx, character, pos, yaw, hint) -> Racer
Racer::integrate(&mut self, input: &Input, track: &Track, t: &Tunables) -> MotionFx   // motion only, PURE
Racer::forward/right/body/eye/head_dir(&self) -> Vec3
Racer::top_speed(&self, &Tunables) -> f32;  disabled(&self) -> bool
Racer::grant_boost(&mut self, secs)                           // full-power boost (keeps a longer one; clears boost_tier)
Racer::spin_out(&mut self, dur);  slow(&mut self, time, mult);  respawn(&mut self, &Track, &Tunables)
Racer::countdown_step(&mut self, input: &Input, last: bool, t: &Tunables) -> Option<Launch>  // one grid tick
Racer::reaches_coin(&self, pos: Vec3, t: &Tunables) -> bool    // the coin pick-up test
Racer::pick_coin(&mut self, t: &Tunables) -> u8                // +1 (u32 saturating) and the coin micro-boost
pub enum Launch { StartBoost, Stumble }
Racer::to_net(&self) -> RacerNet;  apply_net(&mut self, &RacerNet)
pub struct MotionFx { jumped, landed, hard_landing, footstep, drift_boost: u8 /*exit boost fired*/, drift_points: f32, drift_tier: u8,
    drift_start, trick, trick_boost, boost_pad, fell, respawned, overheated, wall_hit: f32 }
```
`integrate` covers spec 4.3 steps 1-11: timers, spin-out, speed, steering,
grip, drift (arm, proportional power-slide, ease-in/out, charge, tiers, exit boost), jump and ramp tricks, ground
following (ramps and crests launch), walls, gap falls and respawn, boost pads,
laser heat, look-back easing, body angle, gait and dust. It touches nothing but
`self`. Not in it (World-level): racer bumps, bash, laser hits, items,
projectiles, coins, hurdles, laps.

The per-racer parts of two World steps are `Racer` methods so a client can
predict its own racer with exactly the world's code: `countdown_step` (spec 6
launch: `launch_t` counting, `prev_input`, `look_back = 0`, and on the last
countdown tick, `last = tick + 1 >= go_tick`, the start boost or stumble; the
world turns the result into `StartBoost` / `Stumble` events after `Go`), and
`reaches_coin` + `pick_coin` (the world adds the `CoinPicked` event, the coin's
respawn and, for dropped coins, the 0.8 s "not your own" rule). None of them
raises events.

`RacerNet` is 56 bytes with bincode 1 default options. It carries the existing
compact position, velocity, heading, flags, drift, timers and laser state, plus
`coins: u32`, `effects: [u8; 11]`, and `attire: [u8; 3]`. Burn/splat use half-second
nibbles; the other timers (including the Laser Trump byte) use deciseconds. Printer/chase timers share a byte
(unseating cancels printing), with two signed 1/8-metre jockey offsets. The same
mode/timer byte distinguishes the Le Mans opening run from printer recovery.
The full own `Racer` keeps exact floats for motion replay. Protocol 6 tests keep
eight racers, all effects, three frogs, 512 egg bits and battle counters below
the 1024-byte datagram budget before optional entities are added.

## Items, pickups, hazards

```rust
pub enum Item { Feather, RollingEgg, HunterEgg, Cactus, Shield, Lightning, MegaLaser, Magnet, Laptop, OrangePill, EggScrambler, SharedCake, MoneyPrinter, RugPull, LaserTrump, Rose }
Item::ALL /* 16, wire order */; Item::index(self) -> u8; Item::from_index(u8); Item::name(self) -> &str
Item::is_good(self) -> bool   // acts on the holder: Feather, Shield, Magnet, Orange Pill, Laser Trump, Shared Cake
pub struct ItemRoll { item, uses }
pub const ITEM_ROLLS: [ItemRoll; 8];          // Laser Eyes, Rug, Laptop, Orange Pill, Whisk, Cake, Printer, Rose
pub const ITEM_ODDS: [[u16; 8]; 4];           // base weights by place bucket (data); Rose never at the front
pub const MAX_QUEUE: usize = 8;               // power-ups that can wait behind the held one
pub struct RollGaps { ahead: Option<f32>, behind: Option<f32>, others_hold_coins: bool }  // metres of progress to nearest unfinished racer
pub fn gap_factor(roll: usize, &RollGaps) -> f32;   // 0..=1.5 scale per ITEM_ROLLS column (Printer/Laser: PRINTER_REACH/LASER_REACH)
pub fn trump_chance(place: u8, n: usize, gap_ahead: Option<f32>) -> f32;  // 0..1
pub fn odds_bucket(place: u8, n: usize) -> usize;  pub fn roll_item<R: Rng>(rng, place, n, gaps: RollGaps) -> ItemRoll
pub struct Projectile { kind: ProjectileKind { RollingEgg, HunterEgg, DollarBill }, pos, vel, owner, target: Option<u8>, age, bounces, hint, s, lateral, id: u32 }
pub struct Hazard { kind: HazardKind { Cactus }, pos, owner, age, id }
pub struct CoinSpot { home, pos /*moves toward magnets*/, alive, respawn_t }    // stable index (net bit vector)
pub struct LooseCoin { pos, vel, ttl, dropped_by, age, hint }                   // dropped coins
pub struct Target { pos, row, alive, bad, respawn_t, beam_t }                        // stable index (net bit mask)
```

One timed item at a time: `Racer::active_item(&Tunables, scramble_left) -> Option<ActiveItem
{ item, left, total }>` (`fraction()` = left / total) names the timed item the racer fired that is
still running (Pill, Trump, Printer, Bitcoin Hack, Magnet, Shield, Laser Eyes, Egg Scrambler;
the one with most time left if several; Shared Cake is over for its user at once). `use_item`
ignores a press while it is `Some`: nothing is consumed and no `ItemUsed` event is sent, and
the press is not queued. The slot already empties on use and an egg can be collected while
an item runs (`reward_target` needs only an empty slot), so the next item sits ready, unusable
until the running one ends. The Egg Scrambler touches everybody, so only the user counts it:
`Effects.scramble_mine` (full own racer only; not in `Effects::pack`); a later whisk by someone
else takes the scrambler over.

Shared Cake: "Everyone else eats cake: you take their good power-ups, and their bad luck
ends." Every other racer still racing (not finished, parked or respawning) gets
`effects.cake_eat = cake_time` (buttons locked, speed untouched; the user does not eat). From
each, the user takes, in this order: running good effects as one use each (a Turbo Feather boost,
`effects.feather` with `boost_t`, becomes Feather x1; `shield_t` Shield; `magnet_t` Magnet;
`effects.pill` Orange Pill; `effects.trump` Laser Trump, the victim keeping the rocket's
`TRUMP_EXIT_BOOST`), then held and queued good items with
their uses (`Item::is_good`); the rest stay with the victim. Burn, splat, tumble, stun, slow and
stumble end; a dismounted jockey stays on foot (dismount, chase offsets and its knock kind are
kept), and a rose seller pestering them moves on. The user's `effects.note`
(`ItemNote::Cake(took)`, or `ItemNote::Rose` for a Rose) and `note_t` drive the "Cake! You
took N power-ups" and "Rose seller sent!" lines.

Item queue: `Racer.queue: Vec<ItemRoll>` (at most `MAX_QUEUE`, bounded when deserialised)
waits behind `item`. `Racer::stash(item, uses) -> bool` fills the slot, else the back of the
queue (false when full; a cake takes only what the user has room for, and the rest stays
with its victim, running or held); `refill()` moves the
next one in when the slot empties (`use_item` does this when the uses run out); `clear_items()`
drops both (Lightning victims, battle outs, Elimination). The queue is only ever non-empty
while `item` is `Some`, so pickups (`reward_target` needs an empty slot) are unchanged.
`World::give_item` stashes the same way.

Rose: a rose seller (`PlayState.roses: Vec<RoseSeller>`, at most one per racer, host-run)
spawns beside the user and runs (1.5 x the fastest boosted top speed) to the nearest other
racer still racing by track distance, then the nearest untried one from there. Alongside
it pesters them for `ROSE_SALE` (5 s): a steady sideways shove away from the seller
(`ROSE_PUSH`) and a little drag (`ROSE_DRAG`), never an unseat; the victim's
`effects.rose` counts the sale down ("Want a rose, fren?") and `effects.rose_side` says
which side the seller is on. `Racer::integrate` applies the shove from those two, so a
client predicts it for its own racer. A shield is used up and the
seller moves on; pill/trump racers, finishers and racers who finish mid-sale are skipped.
A racer who is only down (tumbling, unseated, respawning) is waited for alongside, and the
sale starts once they are up; going down mid-sale does not end it.
After the last it runs off the road for `ROSE_LEAVE` (1.5 s) and is gone. A second Rose
while the user's seller is out sends the same seller round everyone again afterwards
(`again`, at most `ROSE_MAX_AGAIN`), or at once if it was already running off.


`LaserTrump` is the catch-up rocket (constants in `playful`, deliberately not in
`Tunables`/`Balance`, whose fields feed `tunables_hash` and so every saved ghost).
Offered only in `RaceMode::Race` with three or more racers, instead of the normal
roll, with chance `trump_chance`: 0.45 x ((place-1)/(n-1))^2 x a gap factor
(`gap_ahead / 40 m`, clamped 0.25..1), so 45% to last of eight before the gap factor, and never
the leader or a racer with nobody unfinished ahead (the world RNG draws once more
only when the chance is above 0). The normal roll is gap-aware too: `ITEM_ODDS`
gives base weights which `gap_factor` scales (see the tuning block in `items.rs`).
Using it sets `Effects.trump = TRUMP_TIME` (3.5 s), cancels spin, slow, drift and
tumble, and `Racer::integrate` then ignores every control: `trump_steer` autopilots
`TRUMP_LOOK` (20 m) ahead, speed ramps in `TRUMP_RAMP` (0.4 s) to
`TRUMP_SPEED` (1.75) x the unboosted class top speed (`Racer::top_speed` reports it),
and turn rate is x`TRUMP_TURN` (2.0) so it follows every built-in track. On expiry
`boost_t` is raised to `TRUMP_EXIT_BOOST` (1 s). `Racer::trumping()` is
`effects.trump > 0`; `Effects::invincible()` (pill or trump) feeds `World::immune`
(so hurdles, eggs, cacti, bills, lightning and lasers all pass through it), frogs, rugs
and egg splats, and racer bumps skip it. Each tick `World::update_trump` spins out
(`HitCause::LaserTrump`, egg spin time and coin drop, shield respected) every other
racer within `TRUMP_REACH` (2.0 m), or in the beam `TRUMP_TAIL` (8 m) behind the tail,
once per rocket (`Racer.trump_hit` bitmask, host side only), skipping the already
spinning; contact victims are also shoved sideways, and cacti and eggs it touches
are swept away. Once the trumper is in the top two, or has finished, the world
cuts `trump` to at most `TRUMP_WIND_DOWN` (0.4 s) or zero. Bots fire it after 0.3 s;
battle bots never hold one. With eight bots and the trumper last, 14 / 20 / 26 m
apart, it gains on average 6.6 / 5.0 / 3.8 places (typically ending 3rd for 20 m).

Printer auto-aim only acquires rivals within 60 degrees ahead and never steers
bills back towards a passed target. Already unseated riders cannot be hit again;
mounting gives `REMOUNT_GUARD` (1.5 s) of protection, and neither a rug nor Pepe
catches a protected rider. The loose ostrich walks forward at 3 m/s, while the
jockey is thrown, lies for `DISMOUNT_FALL` (0.4 s), then runs back at `RUN_SPEED`
(10 m/s; was 0.6 s and 7 m/s) using independent steering, and presses Jump nearby
to mount. A typical dismount-to-riding time is about 1.0 s (was 1.85 s; tested
for the printer, rug and Pepe). Recovery has no automatic timeout, and finishing
requires mounting. The Le Mans run keeps its 14 m/s.

Every come-off records a `KnockKind` in `Effects.knock` (`mischief::KnockKind`:
`Printer`, `Rug`, `Pepe`, `Trump`; `from_cause(HitCause)` maps a hit to its kind).
The kind picks where the jockey lands relative to the ostrich (printer 5 m behind
and 2 m aside, rug 11 m ahead, Pepe 7.5 m aside and 1.5 m behind). Printer and rug
are matched on the road centre (about 1.0 s to remount); Pepe is only bounded,
because he hops at the road edge, where the loose ostrich walks back to the middle
and the jockey has less to run (about 0.6 s there). The kind also drives the renderer's throw and ostrich reaction. It is
cleared once the rider is back and the ostrich is no longer spun out. A Laser
Trump smash spins the ostrich out as before and records `Trump`; the jockey is
thrown up and lands back in the saddle (presentation only). Any spin hit sets the
kind from its own cause, so a cause with no kind (an egg) clears it: an egg landing
in the middle of a Trump spin does not replay the Trump pose.

`RugPull` places an NPC at the roadside at the deployment point. The rug rolls
out in 0.75 seconds, ignores its owner and persists until catching one rival;
it disappears after the pull animation. Jumping clears it. Two simultaneous
rugs keep snapshots bounded; pickups substitute a Feather when the traps are
full or there is no opponent. The victim is unseated (`HitCause::RugPull`,
`KnockKind::Rug`), not tumbled, and loses every coin it holds: the rugger (the
rug's owner) gains exactly that many and `Event::CoinsTaken { from, to, count }`
follows the `Hit`. With no living owner (a synthetic rug, owner 255) the coins
scatter loose instead, in bursts of at most 255 per `CoinsDropped`. A shield stops
the whole thing. Banked savings are never in the simulation, so are untouched.

Pepe (`Frog`) hops along a path that is a pure function of the race clock minus
`Frog.paused`. A racer he meets (grounded, unprotected, not under pill or
Trump) is unseated away from him (the side comes from where the racer sits relative to him,
so it is arbitrary when they overlap exactly, but deterministic and host-only; `HitCause::Pepe`, `by: None`, `KnockKind::Pepe`,
shield respected, no coin loss) and Pepe falls over for `tumble_time` where he is
hit. `paused` (host-only, `serde(skip)`, zero snapshot bytes) grows while he lies
there, so he gets up and carries on from that spot instead of jumping to where
the clock would have put him. Pepes are also cleared by jumping.

## World

```rust
pub struct RaceConfig { attire: [[u8; 3]; 8] /*serde default*/, track: TrackId, laps: u8 /*1-5*/, seed: u64, mode: RaceMode, racers: Vec<RacerSetup> /*<= 8*/, le_mans: bool /*serde default false*/ }
RaceConfig::quick(track, bots, level, seed)      // seat 0 human + bots
RaceConfig::all_bots(track, n, level, seed)
RaceConfig::training(lesson, kind, seed)
RaceConfig::drill(id: DrillId, track, seed)     // §18.2 drills; RaceConfig.drill: Option<DrillId> (serde default None)
RaceConfig::challenge(id: ChallengeId, character: u8, seed)   // seat 0 Human + the challenge's rivals, on its track
pub struct RacerSetup { name: String, character: u8, kind: RacerKind }
pub enum RacerKind { Human, Bot(BotLevel) }
pub enum RaceMode { Race, TimeTrial, Training(LessonId), Attract, Challenge(ChallengeId), Battle(BattleRules) }
RaceMode::battle_rules(self) -> Option<BattleRules>   // Battle(r), and Last Feather (default rules)
RaceConfig::battle(arena: TrackId, racers: Vec<RacerSetup>, rules: BattleRules, seed)   // spread round the loop
RaceConfig::battle_bots(arena, n, level, rules, seed)
pub enum RacePhase { Countdown, Racing, Finished }
pub const GP_POINTS: [u8; 8] = [15, 12, 10, 8, 6, 4, 2, 1];   // Grand Prix points by place

pub struct World { cfg, tun: Tunables, tick: u64, phase, go_tick: u64, ended_at: Option<f32>,
    racers: Vec<Racer>, projectiles: Vec<Projectile>, hazards: Vec<Hazard>, coins: Vec<CoinSpot>,
    loose_coins: Vec<LooseCoin>, targets: Vec<Target>, play: playful::PlayState, track: Arc<Track> /*serde: as TrackId*/,
    lesson: Option<LessonState>, challenge: Option<ChallengeState>, battle: Option<BattleState> /*serde(default)*/,
    lap_times: Vec<Vec<f32>>, lap_start, max_stuck: Vec<f32>,
    bot_mem: Vec<BotMem>, first_human_finish, finish_count, event_count, .. }

World::new(cfg) -> World;  World::with_tunables(cfg, Tunables) -> World
// (every constructor works out the places from the grid, so a drawn back-row seat is not "1st" until GO)
World::with_track(cfg, Arc<Track>) -> World // session-owned geometry; uses track.id
World::step(&mut self, inputs: &[Input])   // one per racer (missing = idle); bot-driven racers replaced by bot_input
World::drain_events(&mut self) -> Vec<Event>
World::standings(&self) -> Vec<usize>       // racer indices, 1st first
World::race_time(&self) -> f32              // 0 until GO
World::countdown_left(&self) -> f32
World::kind(&self, idx) -> RacerKind
World::bot_level_for(&self, idx) -> Option<BotLevel>   // bots, finished humans, everyone after the race
World::hurdles_active(&self) -> bool                   // false in Bash Brawl
World::is_eliminated(&self, idx) -> bool               // Elimination: parked out of sight, out of every interaction
World::is_out(&self, idx) -> bool                      // Battle: lost its last feather (frozen in place, fading)
World::give_item(&mut self, idx, Item, uses)           // tests / debug / scripts
World::bot_rng(&self, idx) -> Pcg32                    // deterministic per (seed, tick, idx)
```

The app enables `le_mans` for game races: jockeys run towards their waiting
ostriches, steer on foot, and press Jump nearby to mount. Lessons and drills
retain their teaching starts. The headless host enables this rule too.
Rules: grid 2 abreast, 6 m apart, behind the line. In `RaceMode::Race` (quick races,
Grand Prix rounds, online) the seats fill the grid in a seeded random order:
`grid_slots(seed, n)` gives the slot of each seat (0 = pole, left front; a Fisher-Yates
shuffle of `cfg.seed`), so host, clients and a rematch with the same seed agree and
seat 0 (the host) is not always on pole. Time trials, lessons, drills, challenges, battles
and the attract race keep seat order. Countdown 3 s (`Countdown{3,2,1}`,
`Go`); start boost if throttle pressed in the last 0.4 s, stumble if held > 1.2 s.
Crossing the line at the start makes lap 1; a lap counts only after the 3 sector
checkpoints in order (crossing backwards un-counts). Progress = (lap-1) x L + s.
Finish after `laps` laps; places by progress, finished racers by finish order.
The race ends when every connected human has finished, without a grace-period
cutoff; with no humans, when every bot finishes; Attract never ends and never
finishes laps; Training ends on lesson done or failed. After a finish a racer
(and after the race, everyone) keeps driving under bot control. Training
The retired HeadBash config retains its dummy layout for save compatibility. Anti-cut: projection walks from the
hint and walls are always solid, so `s` cannot jump; checkpoint crossings
ignore per-tick jumps over 30 m.

```rust
pub enum HitCause { RollingEgg, HunterEgg, Cactus, Lightning, MegaLaser, Bash, Laser, Hurdle, MoneyPrinter, RugPull, LaserTrump, Pepe }
pub enum Event {
  Countdown{n}, Go, StartBoost{racer}, Stumble{racer}, Footstep{racer}, Jump{racer}, DriftHop{racer},
  DriftStart{racer, dir}, DriftTier{racer, tier}, Boost{racer, tier} /*drift mini-turbo*/, BoostPad{racer},
  Trick{racer}, TrickBoost{racer}, Landed{racer, hard}, WallHit{racer, strength}, Fell{racer}, Respawned{racer},
  CoinPicked{racer, coins}, CoinsDropped{racer, n}, TargetDestroyed{racer, target, item: Option<Item>, uses},
  ItemUsed{racer, item}, Hit{victim, by: Option<u8>, cause}, Bash{attacker, victim, stole: Option<Item>},
  BashWhiff{racer}, LaserZap{by, victim}, Overheated{racer}, ShieldUp{racer}, ShieldBlock{racer, by},
  Lightning{by}, MegaLaser{racer}, HurdleCleared{racer}, Gate{racer}, Lap{racer, lap}, FinalLap{racer},
  Finished{racer, place, time}, WrongWay{racer, on}, LessonProgress{progress, goal}, LessonDone{stars},
  LessonFailed, RaceOver,
  // appended for challenges (all after RaceOver):
  DriftPoints{racer, points: f32} /*Drift King only: charge s (to 0.1 s) x tier, with the exit Boost*/,
  Eliminated{racer, place, pos: [f32; 3]}, ChallengeScore{score: f32}, ChallengeDone{medal: u8},
  // appended for battles (spec 17):
  FeatherLost{racer, by: Option<u8>}, FeatherStolen{racer /*thief*/, from}, Out{racer, place, pos: [f32; 3]}, BattleOver,
  // appended for drills (§18.2):
  Drill(DrillEvent),
  // appended for rugs (all after Drill):
  CoinsTaken{from, to, count: u32} /*a rug pull moved every coin from `from` to `to`*/,
}
```
All racer fields are `u8` indices. `Footstep` fires ~10x/s per racer: the
session should not send remote racers' footsteps reliably (derive them from
`gait_phase` on the client instead).

## Bots

```rust
pub enum BotLevel { Easy, Normal, Hard, Dummy }
pub fn bot_input(world: &World, idx: usize, level: BotLevel) -> Input   // pure
pub struct BotMem { stuck_t, reverse_t, hold_t, calm_t }
pub fn update_mem(m: &mut BotMem, r: &Racer, racing: bool)             // World::step calls it
```
Pure pursuit on `racing_line` (lookahead 6 + 0.45 v), curvature speed plan,
drifts long corners (Normal to tier 2, Hard tier 3), tricks off ramp lips,
jumps hurdles, collects beneficial eggs by contact, fires at forward rivals
(stops at 0.8 heat), uses items and never bashes. Stuck recovery steers back towards the line at full throttle. Randomness comes from `World::bot_rng` (derived from seed, tick and
index), memory from `World::bot_mem`, so `bot_input` takes `&World`. Bot top
speed: Easy 0.86 / Normal 0.95 / Hard 1.0, rubber band +7 % / -4 % vs the
leading human (Easy, Normal).

Battle bots (when `World::battle` is `Some`; not Dummy): no racing line. They
always drive the loop the way it runs and steer to a point on the band ahead
(so the island never blocks them), choosing only the lateral: empty-handed, the
nearest live beneficial egg ahead (collected by contact) or an opponent within
18 m ahead to follow; holding an item, the nearest opponent ahead along the loop.
Rolling Eggs fire when an opponent is lined up 3-45 m ahead (or backward, with
`look_back`, 3-30 m behind), Hunter Eggs when anyone is within 90 m ahead,
cacti on a chaser within 20 m, Shield when an egg is close or on the last
feather, Lightning/Magnet at once, Mega Laser when lined up. Cacti are dodged; Hard swerves away
from a close opponent on its last feather when empty-handed. Speed: corner
limit at the bot's lateral; slows to swing round when the heading error is big;
steering recovery at full throttle. No rubber band in battles.

## Battle ("Feather Frenzy", spec 17)

```rust
pub struct BattleRules { feathers: u8 /*3*/, max_feathers: u8 /*5*/, time_limit: u16 /*s, 180*/,
    invuln_ds: u8 /*15 = 1.5 s*/, egg_respawn_ds: u8 /*30 = 3 s*/ }       // Copy + Eq; Default = spec
BattleRules::invuln() / egg_respawn() / time_limit_s() -> f32
pub struct BattleState { rules, time_left: f32, feathers: Vec<u8>, hits: Vec<u16> /*feathers popped*/,
    taken: Vec<u16>, invuln: Vec<f32> /*s left*/, out_order: Vec<usize>, out_t: Vec<f32> /*s since out*/, done: bool }
BattleState::new(rules, n); is_out(i); flashing(i) /*draw flashing*/; fade(i) -> f32 /*1 -> 0 over BATTLE_OUT_FADE*/;
alive() -> Vec<usize>; out_place(i) -> Option<u8>; ranking() -> Vec<usize>; place_of(i) -> u8
pub const BATTLE_OUT_FADE: f32 = 1.5;
pub const BATTLE_ODDS: [u16; 9];  pub fn roll_battle_item<R: Rng>(rng) -> ItemRoll   // place-free odds, mostly eggs
```
`World::battle` is `Some` for `RaceMode::Battle` and the Last Feather challenge;
every battle rule keys on it. Rules:
- Everyone starts with `feathers` (Last Feather: the player 1), spread round the loop.
- A feather is lost to a Rolling/Hunter Egg, a Cactus (your own too: no credit),
  the Mega Laser, Lightning (every opponent still in, not just those "ahead")
  or a successful bash; laser dazzle never pops one. The attacker's `hits`
  goes up (`FeatherLost { racer, by }`). A bash also steals it while the basher
  holds fewer than `max_feathers` (`FeatherStolen`). A shield blocks the hit
  and the feather.
- For `invuln` (1.5 s) after losing one the racer flashes and every attack
  passes through it (eggs fly on, cacti stay, bashes and Mega/Lightning skip
  it; its shield is not spent). One Mega Laser activation (2.5 s) pops each
  victim at most once (`Racer::mega_hit`, battle only; races unchanged).
- At 0 the racer is out: `Out { racer, place, pos }`, place = racers still in
  before it went. The last ostrich standing is never knocked out (if the final
  two lose their last feather on one tick, the one processed last wins at 0
  feathers); Lightning zaps worst-ranked first so the best-ranked of several
  simultaneous outs is the survivor. It is frozen where it stood with `respawn_t` set (so every
  interaction skips it, as for Elimination; a renderer that hides respawning
  racers should draw it fading by `BattleState::fade` instead) and stays in
  `racers`.
- Ends when one is left or the clock runs out: `BattleOver`, then `RaceOver`
  (in Last Feather `BattleOver`, `ChallengeDone`, `RaceOver`). `done` freezes
  feathers. Ranking (`standings()`, `racer.place`): racers still in by
  feathers, then hits landed, then fewest taken, then index; then the out
  racers, latest out first.
- Contact egg rows are relaid every 12 s; `BATTLE_ROLLS` keeps the established
  nine-entry pool and battle odds; Hunter Eggs home on the nearest
  racer ahead along the loop. No laps, checkpoints or WRONG WAY (`lap`,
  `progress` are meaningless); `cfg.laps` is a dummy 3 (a HUD should hide the
  lap counter, as in challenges).

## Ghosts (Time Trial)

```rust
pub const GHOST_MAGIC: [u8; 4] = *b"STGH";  pub const GHOST_VERSION: u16 = 4;  pub const GHOST_EVERY: u64 = 3;
pub struct GhostRecorder;   // records one racer
GhostRecorder::new(&World, racer) ; record(&mut self, &World) /*after every step*/ ; push(&Racer) ; len() ; is_empty()
GhostRecorder::finish(self, time, lap_times) -> Ghost ; finish_world(self, &World) -> Option<Ghost> /*if it finished*/
pub struct Ghost { version, track: TrackId, track_hash: u64, tunables_hash: u64, character: u8, time: f32,
    lap_times: Vec<f32>, frames: Vec<GhostFrame> }                     // serde
Ghost::INTERVAL (0.05 s); duration(); pose_at(t /*race time*/) -> GhostPose
Ghost::to_bytes() -> Vec<u8>; from_bytes(bytes, track, &Tunables) -> Result<Ghost, GhostError>; decode(bytes); check(track, &tun)
pub struct GhostPose { pos, yaw, visual_yaw, gait_phase, speed, airborne, drifting, boosting, trick, spun }
pub struct GhostFrame { pos: [i32; 3] /*cm*/, yaw: u16, vis: i8, gait: u8, speed: i8 /*0.5 m/s*/, flags: u8 }
pub enum GhostError { BadMagic, Version(u16), Corrupt, WrongTrack, TrackChanged, TunablesChanged }
pub fn track_hash(TrackId) -> u64     // geometry only (points, shoulders, features), not the look
pub fn tunables_hash(&Tunables) -> u64
```
Frame `k` is the racer at race time `k x 0.05 s` (from GO). `pose_at` lerps
the position, takes yaws the short way round and the gait forward, holds the
ends, and takes the flags from the nearer frame. Measured: 3 laps of Halving
Highway (145 s, 2894 frames) = 23 KB (8 B/frame), Volcano 19 KB; against the
60 Hz racer on Canyon worst 0.13 m, mean 0.007 m. The file is the magic, the
version (u16 LE) and bincode varint of the header with positions and yaw as
second differences; `from_bytes` rejects another magic / version, garbage, and
a ghost from another track, a changed track layout or changed tunables. The
app stores the best ghost per track and draws `pose_at(world.race_time())`
translucent (it is not a racer: no collisions).

## Training

```rust
pub enum LessonId { RunSteer, DriftBoost, Jump, LaserEyes, HeadBash, CoinsItems }
LessonId::ALL; number() -> u8; title(); instruction(); hint() -> &'static str; control() -> HintControl;
goal() -> u32; time_limit() -> Option<f32>; star_times() -> [f32; 2]
pub enum HintControl { Steer, Drift, Jump, Laser, Bash, Item }
pub struct LessonState { id, progress, goal, done, failed, stars: u8, time, coins, items }
```
`World::lesson` is updated from the sim's own events. RaceConfig::training(id, RacerKind::Human, seed).

### §18.2 implementation notes (open-ground drills, `race_core::drill`)

Sim side only. `RaceConfig::drill(id: DrillId, track: TrackId, seed)` (the app
passes the Paddock) makes one human seat with `RaceConfig.drill = Some(id)`
(`#[serde(default)]`, `None` elsewhere); `World.drill: Option<DrillState>`.
While a drill is live there are no laps, places, WRONG WAY or finish
(`progress_and_laps` and `check_race_end` skip), track hurdles and pickups are
off, and the race ends only on the drill's `Over` (`RaceOver` follows). Bash
Buddy appends one `Bot(Dummy)` seat (index 1) that the drill drives. Drill
events arrive as `Event::Drill(DrillEvent)` (appended last).

```rust
pub enum DrillId { Slalom, DriftRing, Hurdles, LaserRange, BashBuddy }
DrillId::ALL; title(); instruction(); hint(); control() -> HintControl
pub enum DrillEvent { RepClean, Strike, LevelUp(u8), Star(u8), Master, Over }
pub struct DrillState { id, level: u8 /*1-10*/, reps_in_level /*0-3; 4 clean = level up*/, strikes /*3 = Over*/,
    stars /*1 at L3, 2 at L6, 3 at L9*/, master, over, best_level, clean_total, elapsed,
    rep_t, rep_limit: Option<f32>, ring_swept_deg, ring_ready,
    egg: Option<DrillEgg{pos, life, life_max}>, dummy: Option<DummyCmd{goal, speed_frac, respawn}>, fx: DrillFx{stumble, egg_spawned} }
DrillState::new(id, origin: Vec2, start: Vec3, yaw, seed)      // reps appear ahead of the start
DrillState::step(&mut self, &RunnerView, dt, &mut Pcg32) -> Vec<DrillEvent>   // pure, deterministic
DrillState::props() -> Vec<DrillProp>      // current rep + next (+ <= 1.5 s of knocked debris)
DrillState::current_rep() -> Option<RepInfo>   // Gate / Ring / Logs / Egg / Buddy, for arrows and test drivers
DrillState::time_left() -> Option<f32>; difficulty() -> Difficulty; origin()
pub struct DrillProp { kind: PropKind /*Cone|Pylon|Log|RingCone|Flag*/, pos: Vec3, yaw, knocked: f32 /*0 up .. 1 down*/ }
pub struct RunnerView { pos, vel, yaw, grounded, air_h, drifting, spark_tier, boost_released: Option<u8>,
    egg_destroyed, bash_hit, dummy: Option<DummyView{pos, vel, yaw}> }   // RunnerView::at(pos, yaw)
Difficulty::at(level)   // every L1->L10 number of the spec 18.2 table
consts: FIELD_MIN_R 40, FIELD_MAX_R 230, CONE_HEIGHT 2.6, CONE_RADIUS, RING_CONE_RADIUS, PYLON_RADIUS,
        LOG_HALF_LENGTH, LOG_RADIUS, RUNNER_RADIUS, MAX_STRIKES, REPS_PER_LEVEL, MAX_LEVEL, RING_SWEEP_DEG, ...
```
`World::step` builds the `RunnerView` from racer 0 and this tick's `Boost`,
`LaserZap {by: 0, victim: 1}` and `Bash {victim: 1}` events. A log hit calls the
existing hurdle stumble (`Event::Stumble` + `Hit{Hurdle}`). Laser Range places a stationary opponent dummy at the legacy
`DrillState::egg` marker; the marker egg is hidden and hits give no items;
Bash Buddy steers racer 1 at `dummy.goal` at `speed_frac` of top speed and
teleports it on `dummy.respawn`. Reps appear one ahead of the level counter, so
the rep already on the ground when the level changes keeps its old geometry.
Clearing L10 emits `RepClean, Master, Over`. Props are flat in x/z; `pos.y`
is the runner's ground height when the rep was built (the Paddock is flat).

## Challenges

```rust
pub enum ChallengeId { CoinRush, EggHunt, Elimination, DriftKing, BashBrawl, TurboTimeAttack,
    LaserSniper, RampRider, NightCoinRush, LastFeather }
ChallengeId::ALL: [ChallengeId; 10];  number() -> u8 /*1-based*/;  title(), description(), score_label() -> &'static str
ChallengeId::track() -> TrackId;  laps() -> u8;  finishing() -> bool /*ends on laps: DriftKing, Turbo only*/
ChallengeId::time_limit() -> Option<f32>;  goal() -> u32;  rivals() -> Vec<BotLevel>
ChallengeId::higher_is_better() -> bool;  medal_thresholds() -> [f32; 3] /*bronze, silver, gold*/;  medal_for(score) -> u8
pub const ELIMINATION_INTERVAL: f32 = 25.0;
pub const SNIPER_MISS_PENALTY: f32 = 2.0;  pub const SNIPER_MISS_TIME: f32 = 0.6;
pub struct ChallengeState { id, score: f32, time_left: f32, eliminated: Vec<usize>, done: bool, medal: u8 /*0-3*/,
    time: f32 /*since GO*/, progress: u32, goal: u32,
    penalty: f32, misses: u32, miss_t: f32, chain: bool /*serde(default): Laser Sniper / Ramp Rider*/ }
ChallengeState::new(id)
```
`World::challenge` is `Some` in `RaceMode::Challenge`, updated by the sim from its
own events (racer 0 is the player). `RaceConfig::challenge(id, character, seed)`
builds the config; to let a bot play seat 0 set `cfg.racers[0].kind`.

| Challenge | Track | Score | Medals (bronze / silver / gold) | Ends |
|---|---|---|---|---|
| CoinRush | Oasis | coins picked (over the 10-cap too) | 70 / 100 / 130 (higher) | 90 s; coins respawn in 3 s |
| EggHunt | Baobab | time | 80 / 62 / 48 s (lower) | 20th contact egg; gives up at 150 s (no medal); rows are relaid every 12 s |
| Elimination | Dune, 7 Normal bots | player's place | 3 / 2 / 1 (lower) | every 25 s the last-placed is out; ends with one left or the player out |
| DriftKing | Canyon, 2 laps | sum of charge s x tier per exit boost | 15 / 35 / 60 (higher) | finish |
| BashBrawl | Retired; enum/profile index retained | no attacks | unavailable | hidden from menus |
| TurboTimeAttack | Halving, 3 laps | time | 150 / 138 / 128 s (lower) | finish; starts with Feather x3 |
| LaserSniper | Volcano, 7 dummies | time + 2 s per miss | 75 / 62 / 52 s (lower) | 15th rival laser hit; gives up at 150 s (no medal) |
| RampRider | Oasis, 2 laps | tricks landed + 2 x chained tricks | 6 / 9 / 12 (higher; max 16) | finish |
| NightCoinRush | Halving | coins picked (over the cap too) | 95 / 130 / 165 (higher) | 120 s; coins never respawn on a timer, the player's boost pad brings every taken coin back |
| LastFeather | Dust Bowl battle, 5 Normal bots | the player's battle place | 3 / 2 / 1 (lower; gold = win) | player out, one left, or 180 s; player starts with 1 feather, rivals 3 |

- Laser Sniper: a miss is 0.6 s of laser firing (summed across taps) without
  a regular laser hit on a rival since the last hit: +2 s (`penalty`, `misses`). Score
  = time + penalty, running with the clock (no `ChallengeScore`).
- Ramp Rider: +1 per landed trick (`TrickBoost`); a trick started (`Trick`) while
  boosting (any boost) after the previous trick was landed is a chain: +2
  (`misses` counts chains, `chain` is the armed flag). The ramps are ~10 s
  apart, so a chain means carrying a pad, coin, drift or feather boost into the
  next ramp; a literal 0.8 s trick boost cannot reach it.
- Last Feather: `World::battle` is set; `score` is `BattleState::place_of(0)`,
  `progress` rivals out, `time_left` the battle clock, `time` freezes when the
  player is out (time survived).

Calibration (bot in seat 0, seeds 1-4/6): Laser Sniper Normal 56-58 s, Hard
64-66 s (no misses); Ramp Rider Normal/Hard 6 (8 when a chain happens by
chance); Night Coin Rush Normal 97-111, Hard 102-115; Last Feather Normal:
6th, 4th, 1st, 4th.

`time_left`: counts down for Coin Rush, Bash Brawl, Egg Hunt (to the give-up
limit); in Elimination it is the time to the next elimination; else 0. For time
challenges `score` runs with the clock and freezes at the final time; in
Elimination it is the player's current place. `medal` is live for the
higher-is-better challenges, final once `done`. `progress / goal`: eggs 0-20,
eliminations 0-7, coins, scored drifts, bashes landed, laps (Turbo).

Elimination: at 25 s, 50 s ... 175 s the last-placed survivor gets
`Event::Eliminated { racer, place, pos }` (puff at `pos`), is appended to
`ChallengeState::eliminated`, frozen, parked at y = -1000 and ignored by every
interaction (`World::is_eliminated(i)`). Indices never change. Its `place` is
fixed (8th, 7th, ...); `standings()` puts survivors first by progress, then the
eliminated, latest first. The challenge ends when one racer is left or the
player is out (score = player's place). A HUD should hide the lap counter in
every challenge except DriftKing and Turbo.

`Event::ChallengeScore { score }` fires when the score changes (not for the
time challenges); `ChallengeDone { medal }` once, followed by `RaceOver`.

## Client prediction (race-session)

The host runs `World`; a client predicts only its own racer:

1. Keep a ring of `(seq, Input)` for every tick you sent.
2. On a snapshot `{ tick, ack_seq, own: Racer, others: [RacerNet], .. }`:
   set `my_racer = own` (the full `Racer`, including `prev_input`, timers and
   `track_idx`), then for every stored input with `seq > ack_seq`, in order:
   `my_racer.integrate(&input, &track, &tunables)`. Use the same `Arc<Track>`
   (`cfg.track.track()`) and the same `Tunables` as the host.
3. Hide the correction: `visual_offset += old_predicted_pos - new_predicted_pos`,
   ease it to zero over ~120 ms; snap if it is over 4 m.
4. Between snapshots, each tick: `my_racer.integrate(&input, ..)` with the
   input you send. Do **not** set `speed_scale` (humans are 1.0).
5. Others: keep two `RacerNet`s, lerp `pos`/`yaw` 100 ms in the past, and
   `apply_net` the newer one onto a scratch `Racer` for the rest of the pose.
6. World-level effects (bumps, hits, coins, items, laps) only arrive in
   snapshots and `Event`s; prediction does not try to replay them.

`integrate`'s `MotionFx` (jump, landed, drift tier, boost, footstep, trick) can
drive local sounds for the predicted racer immediately; ignore the host's
reliable copies of those events for your own racer to avoid doubles.

## Notes for the app and session

- The first `TrackId::track()` per track builds its terrain grid (a few ms in
  release, more in debug). Warm all eleven on a load screen (`for id in TrackId::ALL { id.track(); }`).
- Arenas are in `TrackId::ALL` but not `RACE_ALL`: track pickers for races
  should use `RACE_ALL`, battle pickers `ARENAS`.
- Determinism is exact on one machine/build. `sin/cos/exp/atan2` come from the
  platform libm, so an x86 host and an ARM client can differ in the last bit;
  prediction self-corrects from `own` snapshots, but do not compare full world
  hashes across architectures.

## Examples

- `cargo run -p race-core --example race_sim --release [-- easy|normal|hard SEED]`:
  all-bot race per race track (all eight, `TrackId::RACE_ALL`): lap times, order, event counts, max stuck time.
- `cargo run -p race-core --example track_svg -- OUT_DIR`: one SVG per track plus
  `contact_sheet.svg` (all eleven in a 3-wide grid).
- `cargo run -p race-core --example challenge_sim --release [-- easy|normal|hard SEEDS]`:
  plays every challenge with a bot in seat 0: score, medal, time, counter.
- `cargo run -p race-core --example battle_sim --release [-- easy|normal|hard BOTS SEEDS]`:
  all-bot battles on both arenas: duration, feathers popped per minute, winner, out order, max stuck.

Drift feel (0.4.9, proportional power-slide). There is no handbrake kick.
Pressing drift arms it (a hop, kept while the button is held, in the air until
landing); it starts when the steering passes 0.25 on the ground above
`drift_min_speed`, and that steering picks the side, so holding drift while
going straight never turns. The slide then follows how far you steer into it:
the turn multiplier is the low-passed `Drift::carry()`, whose target is the
steer's lean (proportional, with a small soft dead zone below 0.2) times a
multiplier that eases from 1 (plain steering, so no snap on the press) up to
`drift_turn_max` (1.40) over `drift_ease_in` (0.60 s; `Drift::blend()`, 0..1).
Grip blends from `grip` to the drift grip with the carry, fully loose from about
half steer. The speed the slide takes (`drift_scrub`) is spread over the ease-in,
and grip never adds speed to a slide (it is also held under `drift_speed_cap` x
top speed). Straight steering lets grip recover: the carry unwinds with
`drift_carry_time` (0.25 s) and the drift ends; counter-steering
(`drift_counter_time`) also raises the drift grip up to `drift_counter_grip` x so
the slip comes down quickly. Letting go eases out over `drift_ease_out` (0.35 s):
grip and the carried turn blend back to normal, and `dir` and the carry survive
`active`. Past `drift_slip_soft` (50 degrees) the heading stops swinging further
round, fully at `drift_slip_cap` (60), so steering alone never reaches the
spin-out angle. The same limit applies in the air, so a long jump with the drift
held and full steering lands at the cap at most. A drift only spins out
(`drift_spin_time`) when the slip stays above `drift_spin_slip` (65 degrees) for
`drift_spin_hold` (0.1 s) at more than `drift_spin_speed`, i.e. after a shove
(bash, bump) or sliding backwards.
Charge is elapsed drift seconds, building only while there is slip
(`drift_charge_slip`; the whole ease-in is grace). Exit boost: a drift held for
`drift_tiers` (0.8/1.6/2.4 s = blue/orange/purple) earns a boost of that tier
(length `drift_boost_time`, strength `drift_boost_power` x `boost_speed`; a pad
is 1.0) in either of two ways. On release it fires if the slip is already under
`drift_boost_slip` (15 degrees); otherwise it waits up to `drift_boost_hold`
(0.6 s) for the slip to come down and lapses if it does not. With the button
still held, straightening up fires it too: slip under `drift_boost_slip` and the
steer centred (under 0.15) for `drift_straight_hold` (0.12 s) ends the drift and
cashes the tier. A drift under 0.8 s earns nothing, and sliding straight earns no
charge. The state (`blend_q`, `carry_q`, the pending boost) is whole numbers so
the world serialises exactly and a snapshot grows by 7 bytes (typical worst case
1008 of 1024). Event: `Boost{tier}` fires with the boost (not on release), and
`MotionFx::drift_points` carries Drift King's points then.

Jump and cake. While `effects.cake_eat > 0` steering and throttle work but the
drift, jump, laser, item and bash buttons are masked (the raw input still goes to
`prev_input`, so a button held through the last bite does not fire as the cake
ends; a fresh press afterwards does). `jump_vy` is 10.8 m/s with gravity 24: about
0.9 s in the air and a 2.3 m apex at top speed. Holding jump after the apex
(falling, not mid-trick, stunned or tumbling) glides: `playful::glide_vy` uses
`GLIDE_GRAVITY` (0.25 x gravity) and caps the fall at `GLIDE_MAX_FALL` (-3 m/s); a
faster fall is eased down to the cap at `GLIDE_FLARE` (40 m/s^2) rather than
snapped. Rising is untouched.
Ordinary races fire laser eyes only through the
Laser Eyes item, along the physical heading without target selection.
`Tunables::practice_lasers` enables free beams only in laser teaching modes.
