//! Plain-data types the app hands to the renderer. No dependency on race-core.
//!
//! Conventions (SPEC section 3): metres, right-handed, Y up, yaw 0 faces +Z,
//! `forward = (sin yaw, 0, cos yaw)`, `right = (-cos yaw, 0, sin yaw)`.
//! Colours are `[r, g, b]` in 0..1, written as you would a hex colour (sRGB);
//! the renderer converts to linear light internally.

use glam::{Mat4, Vec2, Vec3};

/// An sRGB colour, 0..1 per channel.
pub type Rgb = [f32; 3];

/// Graphics quality (SPEC section 11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Quality {
    /// Render scale 0.7, no MSAA, half the particles.
    Battery,
    /// Render scale 0.85, MSAA 4x.
    #[default]
    Balanced,
    /// Render scale 1.0, MSAA 4x.
    Max,
}

impl Quality {
    pub fn render_scale(self) -> f32 {
        match self {
            Quality::Battery => 0.7,
            Quality::Balanced => 0.85,
            Quality::Max => 1.0,
        }
    }
    pub fn msaa_samples(self) -> u32 {
        match self {
            Quality::Battery => 1,
            _ => 4,
        }
    }
    pub fn particle_scale(self) -> f32 {
        match self {
            Quality::Battery => 0.5,
            _ => 1.0,
        }
    }
}

/// Drawn stats of the last `render` call (both passes if the mirror is shown).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub draw_calls: u32,
    pub triangles: u32,
    pub particles: u32,
}

// ---------------------------------------------------------------- world ----

/// `RoadSample::flags` bits.
pub mod road_flags {
    /// Red/white curb on the left edge.
    pub const CURB_L: u32 = 1;
    /// Red/white curb on the right edge.
    pub const CURB_R: u32 = 2;
    /// Curbs on both edges.
    pub const CURB: u32 = CURB_L | CURB_R;
    /// No road here (canyon gap): no surface, no walls.
    pub const GAP: u32 = 4;
}

/// One centreline sample (about every metre).
#[derive(Clone, Copy, Debug)]
pub struct RoadSample {
    /// Centreline point on the road surface.
    pub pos: Vec3,
    /// Unit vector to the racer's right (`forward x up`).
    pub right: Vec3,
    /// Unit road normal (banked).
    pub up: Vec3,
    /// Full width of the tarmac (m).
    pub width: f32,
    /// Sand/shoulder width beyond the left edge (m).
    pub shoulder_l: f32,
    /// Sand/shoulder width beyond the right edge (m).
    pub shoulder_r: f32,
    /// [`road_flags`].
    pub flags: u32,
}

/// Height grid around the track. `heights[z * w + x]` is the height at
/// `(origin.x + x * cell, origin.y + z * cell)` (`origin.y` is the world Z).
#[derive(Clone, Debug)]
pub struct Terrain {
    pub origin: Vec2,
    pub cell: f32,
    pub w: usize,
    pub h: usize,
    pub heights: Vec<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WallStyle {
    /// Jagged rock faces (canyon).
    RockWall,
    /// Wooden fence posts and rails (savanna).
    #[default]
    FencePosts,
    /// Low barrier with a glowing neon top (night).
    NeonRail,
    /// Smooth sand berm (dunes).
    DuneBerm,
    None,
}

/// Theme colours and lighting.
#[derive(Clone, Debug)]
pub struct Palette {
    pub sky_top: Rgb,
    pub sky_horizon: Rgb,
    pub fog_color: Rgb,
    /// Exponential fog density (1/m). About 0.004..0.012.
    pub fog_density: f32,
    /// Direction **towards** the sun (need not be normalised).
    pub sun_dir: Vec3,
    pub sun_color: Rgb,
    pub ambient_sky: Rgb,
    pub ambient_ground: Rgb,
    pub ground: Rgb,
    pub ground_alt: Rgb,
    pub road: Rgb,
    pub road_stripe: Rgb,
    pub shoulder: Rgb,
    pub wall: Rgb,
    pub curb_a: Rgb,
    pub curb_b: Rgb,
    /// Stars, glowing pylons and rails, dim light.
    pub night: bool,
}

fn rgb(hex: u32) -> Rgb {
    [((hex >> 16) & 255) as f32 / 255.0, ((hex >> 8) & 255) as f32 / 255.0, (hex & 255) as f32 / 255.0]
}

impl Palette {
    /// Golden savanna, noon sun.
    pub fn savanna() -> Palette {
        Palette {
            sky_top: rgb(0x3f8fe0),
            sky_horizon: rgb(0xcfe6f2),
            fog_color: rgb(0xd9e6e0),
            fog_density: 0.0055,
            sun_dir: Vec3::new(-0.35, 0.8, 0.45),
            sun_color: rgb(0xfff0d0),
            ambient_sky: rgb(0xa9bbd6),
            ambient_ground: rgb(0x9a8a58),
            ground: rgb(0xc2b04e),
            ground_alt: rgb(0x9fae48),
            road: rgb(0x55524f),
            road_stripe: rgb(0xf2efe6),
            shoulder: rgb(0xd8b878),
            wall: rgb(0x7a5a38),
            curb_a: rgb(0xd8352a),
            curb_b: rgb(0xf5f2ea),
            night: false,
        }
    }
    /// Orange desert, heat haze.
    pub fn dunes() -> Palette {
        Palette {
            sky_top: rgb(0x5a9ad8),
            sky_horizon: rgb(0xffd9a0),
            fog_color: rgb(0xf3cf9c),
            fog_density: 0.0075,
            sun_dir: Vec3::new(0.4, 0.65, -0.5),
            sun_color: rgb(0xffe2b0),
            ambient_sky: rgb(0xb8b0c8),
            ambient_ground: rgb(0xb06a38),
            ground: rgb(0xe0a458),
            ground_alt: rgb(0xd08a44),
            road: rgb(0x5e5650),
            road_stripe: rgb(0xf5ebd8),
            shoulder: rgb(0xe8c080),
            wall: rgb(0xd69c58),
            curb_a: rgb(0xd8352a),
            curb_b: rgb(0xf5f2ea),
            night: false,
        }
    }
    /// Red mesas at dusk.
    pub fn canyon() -> Palette {
        Palette {
            sky_top: rgb(0x4a4f9a),
            sky_horizon: rgb(0xffa064),
            fog_color: rgb(0xd88a68),
            fog_density: 0.008,
            sun_dir: Vec3::new(-0.7, 0.22, 0.5),
            sun_color: rgb(0xffb070),
            ambient_sky: rgb(0x8a78b0),
            ambient_ground: rgb(0x8a4a38),
            ground: rgb(0xb4583c),
            ground_alt: rgb(0x9a4430),
            road: rgb(0x4e4a4a),
            road_stripe: rgb(0xf0e6d0),
            shoulder: rgb(0xc88a60),
            wall: rgb(0xa8482c),
            curb_a: rgb(0xd8352a),
            curb_b: rgb(0xf5f2ea),
            night: false,
        }
    }
    /// Night salt flats, bitcoin-orange neon.
    pub fn neon_night() -> Palette {
        Palette {
            sky_top: rgb(0x050818),
            sky_horizon: rgb(0x24164a),
            fog_color: rgb(0x1a1238),
            fog_density: 0.0085,
            sun_dir: Vec3::new(0.35, 0.5, -0.8),
            sun_color: rgb(0x7f90d8),
            ambient_sky: rgb(0x3a3f8a),
            ambient_ground: rgb(0x1c1432),
            ground: rgb(0x8c88a8),
            ground_alt: rgb(0x6c6890),
            road: rgb(0x2a2a38),
            road_stripe: rgb(0xe0e8ff),
            shoulder: rgb(0x77739a),
            wall: rgb(0x2c2848),
            curb_a: rgb(0xf7931a),
            curb_b: rgb(0x1a1a28),
            night: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneryKind {
    Acacia,
    Cactus,
    Rock,
    Dune,
    Mesa,
    NeonPylon,
    GrassTuft,
    Fence,
    Bush,
    /// Training log hurdle: a low striped log on posts, `scale` = span across the road in metres
    /// (0.6 m tall, matching race-core's `HURDLE_HEIGHT`). `pos` is the centre of the span on the ground.
    Hurdle,
    /// Training gate arch: two bitcoin-orange posts and a banner with the bitcoin sign, `scale` = distance
    /// between the posts in metres. `pos` is the centre of the span on the ground.
    Gate,
    // ---- decoration kits (render-side only; placed by the app from the track) ----
    /// Fat-trunked baobab with a flat crown (`scale` is uniform, 1 = about 7 m tall).
    Baobab,
    /// Curved palm with drooping fronds (1 = about 6 m).
    Palm,
    /// A clump of reeds with cattail heads (1 = about 1.8 m).
    Reed,
    /// Round hay bale on its side with a strap (1 = 1.2 m across).
    HayBale,
    /// Striped oil drum (1 = 1 m tall).
    Barrel,
    /// Stack of three tyres with a white band (1 = 1.2 m tall).
    TyreStack,
    /// Market tent with a striped roof and awning (1 = 4 m square).
    Tent,
    /// Bleached rib cage and skull half-buried in the sand (1 = 2 m).
    Bones,
    /// Stacked rock pillar (1 = about 9 m).
    Hoodoo,
    /// A big glowing bitcoin sign on twin posts, facing +Z (1 = about 11 m tall).
    BtcSign,
    /// Skyscraper with lit windows and a neon crown (1 = 12 m wide, 60 m tall).
    Tower,
    /// Halving billboard on posts: glowing 3.125 and the bitcoin sign, facing +Z (1 = 16 m wide).
    Billboard,
    /// Dark rock with glowing lava cracks (1 = 2.2 m).
    LavaRock,
    /// A column of dark smoke puffs rising from a vent (1 = about 40 m).
    Smoke,
    /// Lily pads with a flower, flat on water (1 = 3 m patch).
    LilyPad,
    /// Animated: a cheering spectator ostrich (1 = about 2 m tall, bobs).
    Spectator,
    /// Animated: a pennant on a pole (1 = 4.5 m pole), flutters. The cloth is bitcoin orange with a white sign.
    Flag,
    /// Animated: a chequered flag on a pole (1 = 4.5 m pole).
    CheckFlag,
    /// Animated: a tall banner with a big white bitcoin sign on bitcoin orange (1 = 7 m pole).
    Banner,
    /// A three-step terraced grandstand, 8 m wide, front edge at the origin facing +Z, steps rising backwards
    /// (each 1.4 m deep and 0.5 m higher), with a roof and a bitcoin-orange back panel.
    Stands,
}

#[derive(Clone, Copy, Debug)]
pub struct Scenery {
    pub kind: SceneryKind,
    pub pos: Vec3,
    pub yaw: f32,
    pub scale: f32,
}

/// A boost pad lying on the road. `pos` is its centre on the road surface.
#[derive(Clone, Copy, Debug)]
pub struct BoostPad {
    pub pos: Vec3,
    pub yaw: f32,
    pub length: f32,
    pub width: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct StartLine {
    /// Centre of the line on the road surface.
    pub pos: Vec3,
    /// Unit vector to the racers' right.
    pub right: Vec3,
    /// Road width.
    pub width: f32,
}

/// An overpass: the stretch of `road` that is a bridge deck (spec 18.1). The renderer gives it an
/// underside, deeper side faces, a parapet and pillars down to the terrain, keeps the plain wall
/// off it, and shades the lower road in its shadow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bridge {
    /// First road sample of the deck.
    pub first: usize,
    /// Last road sample of the deck; `last >= first`, and indices wrap modulo `road.len()`.
    pub last: usize,
}

/// Open ground in place of a road (the Paddock): `road` is only the physics band, so no ribbon,
/// stripe, curb or start gantry is drawn. A fence ring runs along the band edges (the normal wall);
/// the centre island gets a windmill and the ground gets a mown pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OpenField {
    /// Centre of the field on the ground.
    pub centre: Vec3,
    /// Radius of the unmown centre island (m).
    pub island_r: f32,
    /// Radius of the outer fence (m).
    pub outer_r: f32,
    /// Whether the windmill stands on the island (off for a bare field, such as a run being recorded).
    pub landmark: bool,
}

/// Everything static about a track; built once per track by `load_world`.
#[derive(Clone, Debug)]
pub struct WorldGeometry {
    pub road: Vec<RoadSample>,
    /// The last sample connects back to the first.
    pub closed: bool,
    pub terrain: Option<Terrain>,
    pub wall: WallStyle,
    pub palette: Palette,
    pub scenery: Vec<Scenery>,
    pub pads: Vec<BoostPad>,
    pub start: Option<StartLine>,
    /// Sky look, fluids and ambient detail (all default to "plain").
    pub fx: WorldFx,
    /// Overpass decks (empty on tracks that never cross themselves).
    pub bridges: Vec<Bridge>,
    /// Set for open ground instead of a road.
    pub field: Option<OpenField>,
}

/// Silhouette style of the distant hills drawn by the sky shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HillStyle {
    /// Gentle rolling hills.
    #[default]
    Rolling,
    /// Soft dune crests.
    Dunes,
    /// Flat-topped mesas with steps.
    Mesas,
    /// Sharp volcanic peaks.
    Peaks,
    /// City skyline with lit windows.
    Skyline,
}

/// Sky detail per theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyFx {
    /// Cloud coverage 0..1 (0 = none).
    pub cloud_cover: f32,
    pub cloud_color: Rgb,
    /// A layered haze band above the horizon (0 = none).
    pub band_strength: f32,
    pub band_color: Rgb,
    /// Sun / moon disc size multiplier.
    pub sun_scale: f32,
    pub hills: HillStyle,
    /// Hill colour (`None` = derived from the palette).
    pub hill_color: Option<Rgb>,
    /// Glow at the foot of the hills (lava, city lights), with its strength.
    pub hill_glow: Rgb,
    pub hill_glow_strength: f32,
}

impl Default for SkyFx {
    fn default() -> SkyFx {
        SkyFx {
            cloud_cover: 0.0,
            cloud_color: [1.0, 1.0, 1.0],
            band_strength: 0.0,
            band_color: [1.0, 1.0, 1.0],
            sun_scale: 1.0,
            hills: HillStyle::Rolling,
            hill_color: None,
            hill_glow: [0.0, 0.0, 0.0],
            hill_glow_strength: 0.0,
        }
    }
}

/// Drifting ambient specks around the camera (additive, tiny).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MoteKind {
    #[default]
    None,
    /// Rising embers.
    Embers,
    /// Slow blinking fireflies.
    Fireflies,
    /// Lazy dust motes.
    Dust,
    /// Cold neon sparks.
    Sparks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FluidKind {
    /// Rippling water with a sky tint and sun glints.
    Water,
    /// Glowing lava.
    Lava,
}

/// A flat, animated, opaque fluid patch (water or lava). `pos` is the centre on the surface.
#[derive(Clone, Copy, Debug)]
pub struct Fluid {
    pub kind: FluidKind,
    pub pos: Vec3,
    /// Rotation about Y (like [`Scenery::yaw`]).
    pub yaw: f32,
    /// Half extents: x across (right), y along (forward).
    pub half: Vec2,
    /// An ellipse instead of a rectangle.
    pub round: bool,
    /// Deep colour (water) / crust colour (lava).
    pub deep: Rgb,
    /// Shallow colour (water) / hot colour (lava).
    pub shallow: Rgb,
}

/// Everything render-side that the sim does not know about.
#[derive(Clone, Debug, Default)]
pub struct WorldFx {
    pub sky: SkyFx,
    pub motes: MoteKind,
    pub mote_color: Rgb,
    pub fluids: Vec<Fluid>,
    /// Streaks of distant headlights along the road (night highway).
    pub light_trails: bool,
    /// Ramps get chevron markings (same footprint as a pad: centre, yaw, length, width).
    pub ramps: Vec<BoostPad>,
    /// Rock walls get horizontal strata bands in several tones.
    pub wall_strata: bool,
    /// Rock walls get glowing veins (lava cracks) in this colour.
    pub wall_glow: Option<Rgb>,
}

// ---------------------------------------------------------------- frame ----

#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub eye: Vec3,
    pub target: Vec3,
    pub up: Vec3,
    /// Vertical field of view (radians).
    pub fov_y: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            eye: Vec3::new(0.0, 4.0, -8.0),
            target: Vec3::ZERO,
            up: Vec3::Y,
            fov_y: 70f32.to_radians(),
            near: 0.15,
            far: 600.0,
        }
    }
}

impl Camera {
    pub fn view(&self) -> Mat4 {
        Mat4::look_at_rh(self.eye, self.target, self.up)
    }
    pub fn proj(&self, aspect: f32) -> Mat4 {
        Mat4::perspective_rh(self.fov_y, aspect.max(0.01), self.near, self.far)
    }
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        self.proj(aspect) * self.view()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Accessory {
    #[default]
    None,
    Scarf,
    Goggles,
    Crown,
    StarHeadband,
    VikingHelmet,
    Mohawk,
    ChainShades,
    Cap,
}

/// How a rider came off, which picks the throw and the ostrich's reaction.
/// One style per cause, and each looks different at a glance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KnockStyle {
    /// Not knocked off.
    #[default]
    None,
    /// Dollar bill: the ostrich wobbles, the jockey slides off backwards.
    Printer,
    /// Rug pull: the ostrich skids onto its chest, the jockey is flung up and
    /// forward, end over end, in a puff of dust.
    Rug,
    /// Pepe: the ostrich is bowled sideways and rolls once, the jockey cartwheels aside.
    Pepe,
    /// Laser Trump smash: the ostrich spins flat like a coin, the jockey is thrown straight up.
    Trump,
}

impl KnockStyle {
    /// Every real style.
    pub const ALL: [KnockStyle; 4] = [
        KnockStyle::Printer,
        KnockStyle::Rug,
        KnockStyle::Pepe,
        KnockStyle::Trump,
    ];
}

/// Everything the renderer needs to draw one ostrich.
#[derive(Clone, Copy, Debug)]
pub struct OstrichPose {
    /// Racing riders; egg-laying NPCs set this false.
    pub jockey: bool,
    /// Small deterministic variations in the rider's build and clothes.
    pub rider: u8,
    /// Purely visual ostrich and rider attire.
    pub attire: [u8; 2],
    /// Typing, eating and shell fragments on both heads.
    pub laptop: bool,
    pub cake: bool,
    pub shells: bool,
    pub burning: bool,
    /// 0..1 forced Sonic leg wheel (drifting, Laser Trump). The legs blur into
    /// a fast spinning wheel whatever the speed; 0 leaves it to the speed.
    pub wheel: f32,
    /// Drift spark tier tinting the wheel effects: 0 none, 1 blue, 2 orange, 3 purple.
    pub drift_tier: u8,
    /// Laser Trump: a laser beam out of the tail, gas puffs, legs in full wheel.
    pub trumping: bool,
    /// Normalised pratfall progress; zero means racing normally.
    pub tumble: f32,
    /// Hand-cranked printer carried by the jockey.
    pub printing: bool,
    /// The jockey is on foot; its world offset and seconds since being unseated.
    pub dismounted: bool,
    pub chase_offset: Vec3,
    pub chase_age: f32,
    pub chase_yaw: f32,
    /// How the rider came off, and seconds since (the throw lasts
    /// [`crate::riders::THROW_SECS`], the ostrich's reaction a little longer).
    /// `knock_side` is +1 or -1: which way the ostrich is bowled (right or left).
    pub knock: KnockStyle,
    pub knock_age: f32,
    pub knock_side: f32,
    /// Opening run, facing the waiting ostrich rather than the course.
    pub grid_start: bool,
    /// Remount leap progress (0: inactive) and its previous ground offset.
    pub boarding: f32,
    pub boarding_from: Vec3,
    /// Ground-contact point under the body (feet level, centre of the body).
    pub pos: Vec3,
    /// Sim heading.
    pub yaw: f32,
    /// Body yaw offset relative to `yaw` (the drift angle, signed: positive
    /// turns the body towards the racer's left like a positive yaw).
    pub visual_yaw: f32,
    /// Roll in radians; positive leans to the racer's right.
    pub lean: f32,
    /// Slope pitch in radians; positive tips the nose down.
    pub pitch: f32,
    /// Gait cycle, 1.0 = two steps (SPEC 4.3.11). Only the fraction matters.
    pub gait_phase: f32,
    /// Ground speed (m/s): stride length, neck stretch, wing streaming.
    pub speed: f32,
    pub airborne: bool,
    /// 0..1 legs tucked (jump).
    pub tuck: f32,
    /// Signed 0..1 head swing: positive to the racer's right, negative left.
    pub bash: f32,
    /// 0..1: the head turns 180 degrees over the neck.
    pub look_back: f32,
    /// Eyes glow red and fire.
    pub laser_on: bool,
    /// Extra spin about the vertical axis (spin-out), radians.
    pub spin: f32,
    /// 0..1 forward flip (ramp trick).
    pub trick_flip: f32,
    /// Uniform model scale (1.0 = normal).
    pub scale: f32,
    pub body: Rgb,
    pub neck: Rgb,
    pub legs: Rgb,
    pub beak: Rgb,
    pub accent: Rgb,
    pub accessory: Accessory,
    /// Hide the head, eyes and accessory (first-person camera).
    pub hide_head: bool,
}

impl Default for OstrichPose {
    fn default() -> Self {
        OstrichPose {
            jockey: true,
            rider: 0,
            attire: [0; 2],
            laptop: false,
            cake: false,
            shells: false,
            burning: false,
            wheel: 0.0,
            drift_tier: 0,
            trumping: false,
            tumble: 0.0,
            printing: false,
            dismounted: false,
            chase_offset: Vec3::ZERO,
            chase_age: 0.0,
            chase_yaw: 0.0,
            knock: KnockStyle::None,
            knock_age: 0.0,
            knock_side: 1.0,
            grid_start: false,
            boarding: 0.0,
            boarding_from: Vec3::ZERO,
            pos: Vec3::ZERO,
            yaw: 0.0,
            visual_yaw: 0.0,
            lean: 0.0,
            pitch: 0.0,
            gait_phase: 0.0,
            speed: 0.0,
            airborne: false,
            tuck: 0.0,
            bash: 0.0,
            look_back: 0.0,
            laser_on: false,
            spin: 0.0,
            trick_flip: 0.0,
            scale: 1.0,
            body: [0.86, 0.72, 0.5],
            neck: [0.93, 0.66, 0.6],
            legs: [0.93, 0.66, 0.6],
            beak: [0.98, 0.74, 0.25],
            accent: [0.85, 0.15, 0.12],
            accessory: Accessory::None,
            hide_head: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CoinInst {
    /// Centre of the coin.
    pub pos: Vec3,
    /// Spin about the vertical axis (radians).
    pub spin: f32,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct TargetInst {
    /// Centre of the floating Power Egg.
    pub pos: Vec3,
    pub alive: bool,
    /// 0..1 white flash while being lasered.
    pub hit_flash: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectileKind {
    Egg,
    HunterEgg,
    DollarBill,
}

#[derive(Clone, Copy, Debug)]
pub struct Projectile {
    pub kind: ProjectileKind,
    /// Centre of the egg.
    pub pos: Vec3,
    pub yaw: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Hazard {
    /// Base of the cactus on the ground.
    pub pos: Vec3,
}

#[derive(Clone, Copy, Debug)]
pub struct Beam {
    pub from: Vec3,
    pub to: Vec3,
    pub width: f32,
    pub color: Rgb,
    /// 0..2: 1 = normal, above 1 blooms to white.
    pub intensity: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Shield {
    pub pos: Vec3,
    pub radius: f32,
    pub alpha: f32,
    /// Orange pill: a soft, glowing orange aura round ostrich and jockey.
    pub aura: bool,
}

/// Per-ostrich particle sources, one per frame per racer that emits.
#[derive(Clone, Copy, Debug)]
pub struct Emitter {
    /// Index into `FrameScene::ostriches`.
    pub racer: usize,
    /// 0..1 (more is fine) dust level from the sim (SPEC 4.3.10).
    pub dust: f32,
    pub dust_color: Rgb,
    /// Drift spark tier: 0 none, 1 blue, 2 orange, 3 purple.
    pub drift_tier: u8,
    /// Drifting (bigger, more dust).
    pub drifting: bool,
    /// 0..1 Sonic leg wheel: an extra plume of dust thrown from the wheel,
    /// back and out of the turn. Use the same value as `OstrichPose::wheel`.
    pub wheel: f32,
    /// On sand or grass (bigger, more dust).
    pub offroad: bool,
    /// Boost flames from the tail.
    pub boost: bool,
    /// Upward tail flames and a grey smoke trail.
    pub burning: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurstKind {
    TargetShatter,
    CoinSparkle,
    HitPuff,
    LandingDust,
    Lightning,
}

/// A one-shot effect. Pass each burst in exactly one frame.
#[derive(Clone, Copy, Debug)]
pub struct Burst {
    pub kind: BurstKind,
    pub pos: Vec3,
    pub count: u32,
}

/// The rear-view mirror panel.
#[derive(Clone, Copy, Debug)]
pub struct MirrorView {
    /// Camera at the head looking backwards.
    pub camera: Camera,
    /// `[x, y, w, h]` in target pixels: the panel at full size.
    pub rect_px: [f32; 4],
    /// 0..1 open progress (linear). The renderer applies the elastic ease and
    /// scales the panel about the rect centre. 0 draws nothing.
    pub scale: f32,
}

/// A drill obstacle or a marker on a recorded route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrillPropKind {
    Cone,
    Pylon,
    Log,
    RingCone,
    Flag,
    /// A flat patch of trodden dust, lying `size.y` above `pos` (a recorded run's trail).
    Dust,
    /// A finish gate across `size.x`: two posts, a chequered banner, a chequered stripe on the
    /// ground and arrows beyond it showing the way through (+Z after the yaw).
    Finish,
}

#[derive(Clone, Copy, Debug)]
pub struct DrillPropInst {
    pub kind: DrillPropKind,
    /// Base on the ground, in world metres.
    pub pos: Vec3,
    /// Yaw zero faces +Z; a log lies along the perpendicular axis.
    pub yaw: f32,
    /// Cone fall progress, 0 standing to 1 down. Other props stay upright.
    pub knocked: f32,
    /// Full width, height and depth in metres; the app supplies the sim dimensions.
    pub size: Vec3,
}

#[derive(Clone, Copy, Debug)]
pub struct FrogPose {
    pub pos: Vec3,
    pub yaw: f32,
    pub tumble: f32,
}

/// A rose seller running beside the road.
#[derive(Clone, Copy, Debug)]
pub struct RosePose {
    /// Feet on the ground.
    pub pos: Vec3,
    pub yaw: f32,
    /// Which side (local +x is +1) the racer being sold to is on: the rose is
    /// held out that way.
    pub toward: f32,
    /// Alongside a racer and offering the rose.
    pub selling: bool,
    /// Running off the road at the end: 0 not yet, rising to 1 as it goes.
    pub leave: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct RugPose {
    pub pos: Vec3,
    pub yaw: f32,
    /// Road normal here (tilted by bank): the rug lies on the road, not level.
    pub up: Vec3,
    /// Road slope along its length (sine of the climb angle).
    pub slope: f32,
    pub width: f32,
    pub extension: f32,
    pub pulling: bool,
    pub side: f32,
}

#[derive(Clone, Debug)]
pub struct FrameScene {
    pub camera: Camera,
    /// Animation clock in seconds (boost pad chevrons, coin glints, twinkle).
    pub time: f32,
    pub ostriches: Vec<OstrichPose>,
    pub frogs: Vec<FrogPose>,
    pub roses: Vec<RosePose>,
    pub rugs: Vec<RugPose>,
    pub coins: Vec<CoinInst>,
    pub targets: Vec<TargetInst>,
    pub projectiles: Vec<Projectile>,
    pub hazards: Vec<Hazard>,
    pub drill_props: Vec<DrillPropInst>,
    pub beams: Vec<Beam>,
    pub shields: Vec<Shield>,
    pub emitters: Vec<Emitter>,
    pub bursts: Vec<Burst>,
    pub mirror: Option<MirrorView>,
    /// World-space camera shake offset added to the eye and target.
    pub shake: Vec3,
    /// Multiplier on the palette's fog density (1 = as authored). Use a small
    /// value for far-away cameras (overview, attract orbit, podium).
    pub fog_scale: f32,
    /// Start gantry lights: 0 = all dark, 1..=5 = that many red lights lit, 6 = green (GO), negative = dark.
    pub start_lights: f32,
}

impl Default for FrameScene {
    fn default() -> Self {
        FrameScene {
            camera: Camera::default(),
            time: 0.0,
            ostriches: Vec::new(),
            frogs: Vec::new(),
            roses: Vec::new(),
            rugs: Vec::new(),

            coins: Vec::new(),
            targets: Vec::new(),
            projectiles: Vec::new(),
            hazards: Vec::new(),
            drill_props: Vec::new(),
            beams: Vec::new(),
            shields: Vec::new(),
            emitters: Vec::new(),
            bursts: Vec::new(),
            mirror: None,
            shake: Vec3::ZERO,
            fog_scale: 1.0,
            start_lights: 0.0,
        }
    }
}
