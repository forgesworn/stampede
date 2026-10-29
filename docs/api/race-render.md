# race-render API

`FrameScene::rugs` carries the two NPC-operated rugs (site, width, extension,
pulling pose and roadside). `OstrichPose` also carries `printing`, `dismounted`,
`chase_offset` and `chase_age` for the hand-cranked printer and running jockey.
`grid_start` faces a starting jockey towards its waiting ostrich; `boarding` and
`boarding_from` animate the remount leap without adding network state.
`knock` (`KnockStyle`: `Printer`, `Rug`, `Pepe`, `Trump`), `knock_age` (seconds since
the come-off) and `knock_side` (+1 when the ostrich is bowled to its right) pick the
procedural throw: `knock_fx(style, age, side)` returns the ostrich's reaction matrix
and the jockey's flight (`travel` along the way to where it lands, `lift`, `tumble`).
The throw lasts `THROW_SECS` (0.4 s, the sim's `DISMOUNT_FALL`); a Trump smash lasts
`SMASH_SECS` (1.0 s, the spin-out time). Rug: ostrich onto its chest, jockey somersaults
up and ahead. Pepe: ostrich rolls once sideways, jockey cartwheels. Printer: ostrich
wobbles, jockey slides off backwards, arms flailing. Trump: ostrich flat and spinning
like a coin, jockey thrown straight up. A dismounted jockey with no recorded cause
falls like a printer victim; `grid_start` is never a knock-off.
While `OstrichPose.cake` is set (and the jockey is mounted) the jockey turns its head side-on
(`riders::CAKE_HEAD_YAW`, 1.35 rad) and holds the plate out in front of the turned face,
beside the helmet, so a camera behind sees the cake, the bites and the open, chewing mouth in
profile; with `cake` unset the head faces forward as before.
`ProjectileKind::DollarBill` draws a fluttering green note. These are presentation
fields; the host decides hits, pulls and remounts.

wgpu 29 3D renderer for Stampede. Plain data in, pixels out: **no dependency on
race-core**; the app maps sim state to the types below. Everything is
procedural (authored mesh builders, no external model or image decoders). Source: `crates/race-render`.

Conventions (SPEC section 3): metres, right-handed, Y up, yaw 0 faces +Z,
`forward = (sin yaw, 0, cos yaw)`, `right = (-cos yaw, 0, sin yaw)`.
Colours are `[f32; 3]` sRGB (as you would write a hex colour).
Vectors/matrices are `glam` 0.29.

## Renderer

```rust
Renderer::new(device: &Device, queue: &Queue, surface_format: TextureFormat, q: Quality) -> Renderer
```
One per output format (sRGB or not, either works; the blit encodes accordingly).

```rust
fn set_quality(&mut self, q: Quality)          // Battery 0.7x no MSAA, half particles / Balanced 0.85x MSAA4 / Max 1.0x MSAA4
fn quality(&self) -> Quality
fn resize(&mut self, w: u32, h: u32)           // output (surface) size in physical pixels; scaled internal targets are rebuilt lazily
fn load_world(&mut self, device: &Device, queue: &Queue, w: &WorldGeometry)   // bake static meshes, once per track (also clears particles)
fn render(&mut self, device: &Device, queue: &Queue, encoder: &mut CommandEncoder,
          target_view: &TextureView, scene: &FrameScene, dt: f32)
fn step_particles(&mut self, scene: &FrameScene, dt: f32)   // spawn+advance particles without drawing
fn stats(&self) -> RenderStats                 // { draw_calls, triangles, particles } of the last render (both passes)
fn ground_height(&self, x: f32, z: f32) -> f32 // road plane / terrain under a point (CPU)
```

Free functions and constants:

```rust
ostrich_parts(pose: &OstrichPose, time: f32) -> Vec<OstrichPart>   // { mesh: MeshId, model: Mat4, tint0, tint1 } world-space, one per drawable piece
head_transform(pose) -> Mat4          // head centre, +Z along the beak, +Y up, scaled by pose.scale
eye_positions(pose) -> [Vec3; 2]      // [left, right] world positions: laser origins
foot_positions(pose) -> [Vec3; 2]     // ground contact of both feet (dust)
first_person_eye(pose) -> FirstPersonEye { eye, forward, up }      // camera frame just above/behind the skull; beak shows at the bottom of the view
ease_out_elastic(t: f32) -> f32       // the curve used for the mirror pop-in
MIRROR_W / MIRROR_H = 384 / 160       // mirror pass resolution
capture::{Offscreen, SHOT_FORMAT}     // headless target + RGBA read-back (write the PNG yourself)
```

## Data

`WorldGeometry { road: Vec<RoadSample>, closed, terrain: Option<Terrain>, wall: WallStyle, palette: Palette, scenery: Vec<Scenery>, pads: Vec<BoostPad>, start: Option<StartLine> }`

- `RoadSample { pos, right, up, width, shoulder_l, shoulder_r, flags }` about 1 m apart; `flags`: `road_flags::{CURB_L, CURB_R, CURB, GAP}` (GAP = no surface, no walls, for the canyon jump). Ramps/hills/banking are just the sample heights and `up` vectors.
- `Terrain { origin: Vec2 (x, z), cell, w, h, heights }` (`heights[z * w + x]`); it is sunk under the road band automatically.
- `WallStyle::{RockWall, FencePosts, NeonRail, DuneBerm, None}`.
- `Palette { sky_top, sky_horizon, fog_color, fog_density, sun_dir (towards the sun), sun_color, ambient_sky, ambient_ground, ground, ground_alt, road, road_stripe, shoulder, wall, curb_a, curb_b, night }` with presets `Palette::{savanna, dunes, canyon, neon_night}()`.
- `Scenery { kind: Acacia|Cactus|Rock|Dune|Mesa|NeonPylon|GrassTuft|Fence|Bush|Hurdle|Gate, pos, yaw, scale }` (`Hurdle` / `Gate`: training meshes, baked and chunk-culled like the rest; `pos` = centre of the span on the ground, `yaw` = road heading, **`scale` = span in metres** across the road, not a uniform scale. Hurdle: log top 0.6 m, orange/cream stripes on posts. Gate: bitcoin-orange posts about 4.8 m with a banner of white bitcoin signs, `scale` = distance between the posts), `BoostPad { pos, yaw, length, width }`, `StartLine { pos, right, width }` (arch + chequered line).

`FrameScene` (Default = empty scene, fog_scale 1):
`camera: Camera { eye, target, up, fov_y (rad), near, far }`, `time`, `ostriches: Vec<OstrichPose>`, `coins: Vec<CoinInst { pos, spin, visible }>`, `targets: Vec<TargetInst { pos, alive, hit_flash }>`, `projectiles: Vec<Projectile { kind: Egg|HunterEgg, pos, yaw }>`, `hazards: Vec<Hazard { pos }>` (cactus, pos = base), `beams: Vec<Beam { from, to, width, color, intensity }>`, `shields: Vec<Shield { pos, radius, alpha, aura }>`, `emitters: Vec<Emitter>`, `bursts: Vec<Burst>`, `mirror: Option<MirrorView>`, `shake: Vec3`, `fog_scale: f32`.

`OstrichPose { pos (ground point under the body), yaw, visual_yaw (body offset relative to yaw = drift angle), lean (roll, + = right), pitch (+ = nose down), gait_phase (1.0 = two steps), speed, airborne, tuck 0..1, bash -1..1 (+ = right), look_back 0..1, laser_on, spin, trick_flip 0..1, scale, wheel 0..1, drift_tier 0..3, trumping, body, neck, legs, beak, accent, accessory, hide_head }`.

`wheel` forces the Sonic leg wheel at any speed (the speed-driven wheel above ~36 m/s is unchanged; the larger of the two wins). Set it from the presenter: ease towards 1 while drifting (about 0.12 s in, 0.25 s out) and hold 1 while trumping. The wheel turns at 5 rev/s or more at any speed (a whole number of turns per `gait_phase` cycle, so keep feeding the sim's gait phase), and the effects are louder than for plain speed: five ghost legs, a denser ring, a bright disc. `drift_tier` (same 0..3 as the emitter) tints the ring, disc glints and streaks blue, orange or purple; tier 0 keeps the leg colour. `trumping` (Laser Trump) forces a full wheel, pitches the body forward, and draws a flickering green-white laser beam 8 m long straight back from the tail along `yaw`, with a glare at the source; the particle system also streams green gas puffs and bright motes from the tail (no emitter needed).
`Accessory::{None, Scarf, Goggles, Crown, StarHeadband, VikingHelmet, Mohawk, ChainShades, Cap}`.
`hide_head` hides skull, eyes and head accessories (first person); the beak stays.

`Emitter { racer (index into ostriches), dust 0..1+, dust_color, drift_tier 0..3 (1 blue, 2 orange, 3 purple), drifting, wheel 0..1, offroad, boost, burning }`. `wheel` (same value as `OstrichPose::wheel`) adds a big dust plume from the wheel, about twice the plain drifting dust again with larger puffs, thrown back and out of the turn (the side opposite `OstrichPose::lean`, random when upright); it needs `dust` > 0, so none in the air.
`Burst { kind: TargetShatter|CoinSparkle|HitPuff|LandingDust|Lightning, pos, count }`.
Egg instances deliberately contain no information about bad contents: every
shell and glow looks alike. `Shield.aura` selects the orange pill's soft orange
light, pulsing rim and orbiting sparks; false keeps the blue shield effect.

`MirrorView { camera, rect_px: [x, y, w, h], scale 0..1 }`.

`FrameScene::drill_props: Vec<DrillPropInst { kind: DrillPropKind, pos, yaw, knocked, size }>`:
`DrillPropKind::{Cone, Pylon, Log, RingCone, Flag}`. `pos` is the base on the ground;
`size: Vec3` is the full width, height and depth in metres, supplied by the app from
the sim's collision dimensions. Logs run across the heading (local X). Cone fall
progress `knocked` runs from 0 standing to 1 resting on its side towards the
heading; it keeps the lowest point on the ground and stays down until removed.
Other props ignore `knocked`. These are instanced each frame, including in the
mirror; changing a rep or route marker does not reload the baked world.

## How to drive it

1. **Startup**: `Renderer::new(&device, &queue, surface_format, quality)`, then `resize(w, h)` with the surface size (call again on every surface resize).
2. **Per track**: `load_world(...)` once (static meshes are baked and chunked; this is the only expensive call, a few ms). Build the `WorldGeometry` from the track samples (race-core `Track`), `Palette` from the theme, scenery from the track seed.
3. **Per frame** (before egui): build a `FrameScene` from the interpolated sim state and the camera, then
   `renderer.render(&device, &queue, &mut encoder, &surface_view, &scene, dt)`.
   The target is cleared and fully covered (scaled scene blit), then the mirror panel is drawn on top, so open egui's pass afterwards with `LoadOp::Load`. `dt` is the real frame time in seconds (it only drives particles); pass 0 to freeze them while paused. One `render` per `queue.submit` is expected; up to 3 calls into the same encoder work.
4. **Cameras are the app's job**: chase/far-chase from the pose, first person from `first_person_eye(&pose)` with `pose.hide_head = true`, a shorter `fog_scale` (e.g. 0.15) for overview/orbit/podium cameras that sit far from the action. `shake` offsets eye and target.
5. **Look back + mirror**: set `pose.look_back` (ease it 0..1 yourself, the neck turns 180 degrees). While it is above 0 (or while the panel is closing) pass `mirror: Some(MirrorView { camera, rect_px, scale })`: camera at `head_transform(&pose)` looking backwards (e.g. eye = head position + 0.15 up, target = eye + back * 20, fov about 60, far 300), `rect_px` the full-size panel in target pixels (SPEC: top-centre, about 440 x 184), `scale` the **linear** open progress 0..1 (the renderer applies the elastic ease and scales about the rect centre; 0 or `None` skips the pass). The mirror pass renders at 384 x 160 with the same scene (including particles), flipped horizontally. It roughly doubles triangles and about doubles draw calls while shown.
6. **Quality**: `set_quality` any time (targets rebuild on the next frame; MSAA pipelines are compiled lazily on the first Balanced/Max frame).
7. **Particles**: the renderer owns them. Give one `Emitter` per racer per frame (dust from the sim's `dust` value, `dust_color` = ground/sand colour under the feet, `drifting`/`offroad` make the clouds bigger, `drift_tier` for sparks, `boost` for tail flames) and put each one-shot event in `bursts` for exactly one frame (target kill = `TargetShatter` at the target, coin pickup = `CoinSparkle`, landing = `LandingDust`, hit = `HitPuff`, lightning = `Lightning` at the victim). Pool is 3600 (1800 on Battery); when full the oldest are recycled. Dust sorts and fades near the camera so your own trail never veils the road.
8. **Animation inputs**: `gait_phase` advances `speed * dt / 2.6`; feet are planted from it and the body height follows the lower foot. Keep `time` monotonic (coin glints, pad chevrons, target bob, scarf flutter).

## Art and materials

Racers use sculpted feather strips, continuous tapered neck sections and eye
highlights. Jockey heads, striped helmets, racing jerseys and saddles, plus
laptop, cake, plate and webbed frog-foot meshes are authored in `assets.rs` and
instanced through the same mesh store. The default rider with the Viking
helmet and active laser remains below the 5,500-triangle rig budget.

`FrameScene.roses: Vec<RosePose { pos, yaw, toward, selling, leave }>` draws the Rose's
seller (`riders::rose_seller`): a cheerful hawker in a turban, waistcoat and bow tie with a
basket of roses, one oversized red rose (`MeshId::RoseBloom`) held high while running
and offered towards the customer's side (`toward`, local +x = +1) while `selling`,
and cartoon legs at `ROSE_CADENCE` strides a second with two fainter trailing ghosts.
`leave` (0..1) runs it off the road and pops it away at the end. The basket is
`MeshId::Basket`.

The signed normal byte stores either positive emission or negative material
sheen, keeping the 20-byte vertex layout. Lit geometry and animated scenery
share wrapped sunlight, hemisphere/bounce light and a soft camera fill.
Polished surfaces add a small highlight; cloth and feathers stay matte.

Scenery contact shadows are cached at world load, culled/faded at 65–85 m,
and use the existing shadow draw. Acacia crowns have overlapping leaf clusters;
canyon strata align by height and road wheel lanes have restrained contrast.
Power-egg shells use the same satin material for all contents. Wide laser
visuals are capped at 1.25 m; simulation range and hit width are unchanged.

## Budgets (showcase, 1280 x 576, Max)

4 racers: about 38 draw calls, 91k triangles. 8 racers all emitting with the mirror up: 75 draw calls, 164k triangles, about 2000 particles. Static world is culled per 64 m chunk (frustum + fog distance) and contiguous visible chunks are merged into one draw.

## GLES 3 notes

Written to the GLES 3 feature set by construction: no storage buffers, no dynamic uniform offsets, no base-vertex or first-instance draws (indices are absolute, per-draw instance data is a sliced vertex buffer), no `instance_index`, uniform structs are vec4-aligned (320 bytes), 9 vertex attributes, Depth24Plus, Rgba8Unorm scene targets with a 4x MSAA resolve. **Verified** only on Vulkan (Intel Iris Xe) and on wgpu's GL backend (`WGPU_BACKEND=gl`, which on this box is desktop OpenGL via Mesa, not an ES 3 context), where the output matches Vulkan. **Not yet run** on an ES 3 context or on a real Adreno: first on-device check should look at MSAA resolve, the mirror pass and the sRGB/non-sRGB blit.

## Notes for the app

- `ostrich_parts(&pose, time) -> Vec<OstrichPart>` differs slightly from SPEC section 11 (`impl Iterator<Item = (MeshId, Mat4, tint)>`): it takes the animation time (scarf and tail flutter) and a part carries two tints (main and accent/beak).
- Ground height for blob shadows and dust is derived by the renderer from the loaded world (`GroundQuery`), so poses carry no `ground_y`.
- `render` and `step_particles` both advance particles: call one of them per frame.

## Performance (OnePlus 8T / Pixel 8)

Measured on device with the DIAG knobs below: on the 8T at Balanced the GPU time was almost all **dust fill**, not MSAA, sky or blit. With the main pass drawn 3x, the busiest moment (8 racers into the first corner on Dune) took 52 ms with dust and fitted in 16.7 ms without it. The dust fragment shader's per-pixel noise (8 hashes, 2 pows) was the cost: replacing it with a flat disc cut the same window to 22 ms.

What changed:
- Dust: the puff noise is baked once into a 64x64 R8 tile (`particles::dust_noise`, bound at group 0 binding 1/2, procedural, no asset). Lighting, fog and gamma are per puff in the vertex shader, so the fragment shader is one fetch and a smoothstep. Corners of the quad outside the disc are discarded.
- Dust near the lens: the on-screen radius is capped (`DUST_MAX_R` in `particles.wgsl`, 0.6 NDC) and the puff gets denser as it shrinks; puffs that are all but invisible (very near, or faded) are not rasterised.
- The sky is drawn after the opaque geometry with a LessEqual depth test, so it only shades uncovered pixels.
- Tiers are unchanged (Battery 0.7 / Balanced 0.85 + MSAA 4x / Max 1.0 + MSAA 4x). After the dust change the 8T holds 60 fps at all three; Max has about 2x headroom on the busiest scenes.

`Renderer::set_diag(Diag)` (measurement only; the app drives it from `diag.txt` in DIAG builds): `scale`, `msaa`, `sky`, `world`, `meshes`, `shadows`, `dust`, `additive`, `blit` toggles and `repeat=N` (draw the main pass N times to amplify GPU cost above the vsync cap). `Diag::default()` changes nothing.

## Track decoration (additive; the track polish pass)

Everything below is render-side: the sim, race-core and the netcode never see it. The app derives it from the track in
`app/src/game/geometry/trackside.rs` (crowds, flags, tyre walls, ponds, lava, towers, the sky look).

- `WorldGeometry.fx: WorldFx` (new field; `WorldFx::default()` = plain, the old look):
  `sky: SkyFx` (clouds, haze bands, sun/moon size, hill silhouette style `HillStyle::{Rolling, Dunes, Mesas, Peaks, Skyline}`,
  hill colour and a glow at the foot of the hills), `motes: MoteKind` + `mote_color` (ambient embers / fireflies / dust /
  sparks, additive sprites around the camera, no extra draw call), `fluids: Vec<Fluid>` (flat opaque animated water or lava
  patches), `light_trails` (headlight streaks on the far lanes of a night highway), `ramps` (chevron markings, same footprint
  as a `BoostPad`), `wall_strata`, `wall_glow` (rock walls: layered strata bands / glowing lava cracks).
- New `SceneryKind`s (all placed with `pos`, `yaw`, uniform `scale`; +Z faces the viewer): `Baobab, Palm, Reed, HayBale,
  Barrel, TyreStack, Tent, Bones, Hoodoo, BtcSign, Tower, Billboard, LavaRock, Smoke, LilyPad, Stands` (static, baked into
  the world chunks) and `Spectator, Flag, CheckFlag, Banner` (animated: the crowd bobs, cloth flutters).
- `FrameScene.start_lights: f32`: 1..=5 red lights lit, 6 green (GO), 0 dark. The gantry at `StartLine` has five pods per face.
- **The decor pipeline** (`decor.rs`, `shaders/decor.wgsl`): animated decor (crowd, flags, banners, ₿ signs, water, lava, the
  start lights) is baked into a second chunked vertex buffer (24-byte vertices with a per-vertex `anim` word) and drawn with
  one extra opaque draw per run of visible chunks (frustum + fog culled like the static world). All animation runs in the
  vertex shader from the time uniform, water and lava shade with a few sines in the fragment shader. No instance buffer, no
  CPU per frame, no blending, no extra pass. `Globals` grew four `vec4`s (sky fx).
- Road surface: five tarmac lanes with tone and patch variation, dashed skid marks along the inside of tight corners,
  glowing edge lines at night; curbs on every corner (curvature above 0.006); boost pads get a pulsing halo (an additive glow
  sprite); ramps carry yellow chevrons (the pad pipeline with a negative `size.x`).
- Dev: `RUST_LOG=race_render=debug` prints `RenderStats` per frame; `world baked` logs the static and decor triangles.
  Shots: `--shot stage_trk_<track>` (HUD-free; `STAMPEDE_VIEW=chase|grid|over|side|mill`, `mill` shows the Paddock windmill; `STAMPEDE_MIRROR=1` for the mirror pass).

Budgets (busiest view, 8 racers): 36-49 draws and 75-158k triangles on the 8T; with the mirror up (desktop shots) 71-85 draws
and 146-256k triangles. The decor (all of it, a whole lap) bakes to about 25k triangles per track.
