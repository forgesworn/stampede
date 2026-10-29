//! The renderer: pipelines, render targets, per-frame data assembly and the
//! main / mirror / composite passes.
//!
//! GLES 3 notes: no storage buffers, no dynamic uniform offsets, no base
//! vertex or first-instance draws (indices are absolute, instance data is
//! addressed by slicing the vertex buffer), all uniform structs are
//! vec4-aligned.

use crate::color::srgb3_to_linear;
use crate::decor::{decor_layout, DecorVertex};
use crate::mesh::{vertex_layout, Vertex};
use crate::meshes::{MeshId, MeshStore, MESH_COUNT};
use crate::ostrich::ostrich_parts;
use crate::particles::{ParticleInst, ParticleSystem, KIND_AURA, KIND_GLOW};
use crate::types::*;
use crate::world::{bake, ChunkInfo, GroundQuery, PadVertex};
use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3, Vec4};
use wgpu::util::DeviceExt;

/// Format of the internal scaled scene targets (gamma encoded in the shaders).
const SCENE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;
const SLOTS: usize = 3;
const SLOT_BYTES: u64 = 1 << 20;
pub const MIRROR_W: u32 = 384;
pub const MIRROR_H: u32 = 160;
const MIRROR_FAR: f32 = 170.0;
/// Side of the baked dust-puff noise tile.
const DUST_TEX: u32 = 64;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    cam_right: [f32; 4],
    cam_up: [f32; 4],
    sun_dir: [f32; 4],
    sun_color: [f32; 4],
    amb_sky: [f32; 4],
    amb_ground: [f32; 4],
    fog: [f32; 4],
    sky_top: [f32; 4],
    sky_hor: [f32; 4],
    hill: [f32; 4],
    screen: [f32; 4],
    fx0: [f32; 4],
    fx1: [f32; 4],
    fx2: [f32; 4],
    fx3: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Inst {
    m: [[f32; 4]; 4],
    t0: [f32; 4],
    t1: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CompU {
    params: [f32; 4],
    rect: [f32; 4],
    misc: [f32; 4],
}

const INST_ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
    3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4, 7 => Float32x4, 8 => Float32x4
];
const I3_ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
const PAD_ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2, 2 => Float32x2];

fn inst_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Inst>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &INST_ATTRS,
    }
}
fn i3_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ParticleInst>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &I3_ATTRS,
    }
}
fn pad_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<PadVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &PAD_ATTRS,
    }
}

struct Shaders {
    lit: wgpu::ShaderModule,
    sky: wgpu::ShaderModule,
    particles: wgpu::ShaderModule,
    beam: wgpu::ShaderModule,
    shadow: wgpu::ShaderModule,
    pad: wgpu::ShaderModule,
    decor: wgpu::ShaderModule,
    composite: wgpu::ShaderModule,
}

fn module(device: &wgpu::Device, label: &str, body: &str, with_globals: bool) -> wgpu::ShaderModule {
    let src = if with_globals { format!("{}\n{}", include_str!("shaders/common.wgsl"), body) } else { body.to_string() };
    device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.into()) })
}

impl Shaders {
    fn new(device: &wgpu::Device) -> Shaders {
        Shaders {
            lit: module(device, "lit", include_str!("shaders/lit.wgsl"), true),
            sky: module(device, "sky", include_str!("shaders/sky.wgsl"), true),
            particles: module(device, "particles", include_str!("shaders/particles.wgsl"), true),
            beam: module(device, "beam", include_str!("shaders/beam.wgsl"), true),
            shadow: module(device, "shadow", include_str!("shaders/shadow.wgsl"), true),
            pad: module(device, "pad", include_str!("shaders/pad.wgsl"), true),
            decor: module(device, "decor", include_str!("shaders/decor.wgsl"), true),
            composite: module(device, "composite", include_str!("shaders/composite.wgsl"), false),
        }
    }
}

struct Pipes {
    sky: wgpu::RenderPipeline,
    lit: wgpu::RenderPipeline,
    shield: wgpu::RenderPipeline,
    shadow: wgpu::RenderPipeline,
    part_alpha: wgpu::RenderPipeline,
    part_add: wgpu::RenderPipeline,
    beam: wgpu::RenderPipeline,
    pad: wgpu::RenderPipeline,
    decor: wgpu::RenderPipeline,
}

const ADD_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::SrcAlpha,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
};

fn depth_state(write: bool, compare: wgpu::CompareFunction, bias: i32) -> Option<wgpu::DepthStencilState> {
    Some(wgpu::DepthStencilState {
        format: DEPTH_FORMAT,
        depth_write_enabled: Some(write),
        depth_compare: Some(compare),
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState { constant: bias, slope_scale: if bias != 0 { -1.5 } else { 0.0 }, clamp: 0.0 },
    })
}

#[allow(clippy::too_many_arguments)]
fn pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    vs: &str,
    fs: &str,
    buffers: &[wgpu::VertexBufferLayout<'_>],
    blend: Option<wgpu::BlendState>,
    depth: Option<wgpu::DepthStencilState>,
    cull: Option<wgpu::Face>,
    samples: u32,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState { module, entry_point: Some(vs), compilation_options: Default::default(), buffers },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format: SCENE_FORMAT, blend, write_mask: wgpu::ColorWrites::ALL })],
        }),
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: cull, ..Default::default() },
        depth_stencil: depth,
        multisample: wgpu::MultisampleState { count: samples, mask: !0, alpha_to_coverage_enabled: false },
        multiview_mask: None,
        cache: None,
    })
}

impl Pipes {
    fn new(device: &wgpu::Device, sh: &Shaders, layout: &wgpu::PipelineLayout, samples: u32) -> Pipes {
        use wgpu::CompareFunction as C;
        let alpha = Some(wgpu::BlendState::ALPHA_BLENDING);
        Pipes {
            sky: pipeline(device, "sky", layout, &sh.sky, "vs_sky", "fs_sky", &[], None, depth_state(false, C::LessEqual, 0), None, samples),
            lit: pipeline(
                device,
                "lit",
                layout,
                &sh.lit,
                "vs_lit",
                "fs_lit",
                &[vertex_layout(), inst_layout()],
                None,
                depth_state(true, C::Less, 0),
                Some(wgpu::Face::Back),
                samples,
            ),
            shield: pipeline(
                device,
                "shield",
                layout,
                &sh.lit,
                "vs_lit",
                "fs_shield",
                &[vertex_layout(), inst_layout()],
                alpha,
                depth_state(false, C::LessEqual, 0),
                None,
                samples,
            ),
            shadow: pipeline(device, "shadow", layout, &sh.shadow, "vs_shadow", "fs_shadow", &[i3_layout()], alpha, depth_state(false, C::LessEqual, -3), None, samples),
            part_alpha: pipeline(device, "particles_alpha", layout, &sh.particles, "vs_particle", "fs_particle", &[i3_layout()], alpha, depth_state(false, C::LessEqual, 0), None, samples),
            part_add: pipeline(device, "particles_add", layout, &sh.particles, "vs_particle", "fs_particle", &[i3_layout()], Some(ADD_BLEND), depth_state(false, C::LessEqual, 0), None, samples),
            beam: pipeline(device, "beam", layout, &sh.beam, "vs_beam", "fs_beam", &[i3_layout()], Some(ADD_BLEND), depth_state(false, C::LessEqual, 0), None, samples),
            pad: pipeline(device, "pad", layout, &sh.pad, "vs_pad", "fs_pad", &[pad_layout()], None, depth_state(false, C::LessEqual, -4), None, samples),
            decor: pipeline(device, "decor", layout, &sh.decor, "vs_decor", "fs_decor", &[decor_layout()], None, depth_state(true, C::Less, 0), Some(wgpu::Face::Back), samples),
        }
    }
}

struct FrameSlot {
    dynbuf: wgpu::Buffer,
    globals_main: wgpu::Buffer,
    globals_mirror: wgpu::Buffer,
    bg_main: wgpu::BindGroup,
    bg_mirror: wgpu::BindGroup,
    blit_u: wgpu::Buffer,
    mirror_u: wgpu::Buffer,
}

struct Targets {
    size: (u32, u32),
    samples: u32,
    msaa: Option<wgpu::TextureView>,
    resolve: wgpu::TextureView,
    depth: wgpu::TextureView,
    blit_bgs: Vec<wgpu::BindGroup>,
}

struct MirrorTargets {
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    bgs: Vec<wgpu::BindGroup>,
}

struct WorldGpu {
    vb: wgpu::Buffer,
    ib: wgpu::Buffer,
    chunks: Vec<ChunkInfo>,
    pad_vb: Option<wgpu::Buffer>,
    pad_ib: Option<wgpu::Buffer>,
    pad_count: u32,
    decor_vb: Option<wgpu::Buffer>,
    decor_ib: Option<wgpu::Buffer>,
    decor_chunks: Vec<ChunkInfo>,
    fx: WorldFx,
    road: Vec<RoadSample>,
    pads: Vec<BoostPad>,
    palette: Palette,
    ground: GroundQuery,
    scenery_shadows: Vec<ParticleInst>,
}

/// What went into the dynamic buffer this frame and where.
#[derive(Default)]
struct FrameLayout {
    inst: Vec<(MeshId, u64, u32)>,
    shadows: (u64, u32),
    beams: (u64, u32),
    shields: (u64, u32),
    alpha: (u64, u32),
    additive: (u64, u32),
}

struct PassCam {
    planes: [Vec4; 6],
    pos: Vec3,
    max_dist: f32,
}

/// Measurement knobs (DIAG builds only drive these from the app): toggle passes
/// and override the quality tier so GPU cost can be attributed on a device.
/// `Diag::default()` changes nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Diag {
    /// Render scale override (0 = the tier's).
    pub scale: f32,
    /// MSAA sample count override (0 = the tier's).
    pub msaa: u32,
    pub sky: bool,
    pub world: bool,
    pub meshes: bool,
    pub shadows: bool,
    pub dust: bool,
    pub additive: bool,
    pub blit: bool,
    /// Draw the main pass this many times (amplifies GPU cost above the vsync cap).
    pub repeat: u32,
}

impl Default for Diag {
    fn default() -> Diag {
        Diag { scale: 0.0, msaa: 0, sky: true, world: true, meshes: true, shadows: true, dust: true, additive: true, blit: true, repeat: 1 }
    }
}

impl Diag {
    /// Parse `key=value` words, e.g. `scale=0.8 msaa=1 dust=0 repeat=3`.
    pub fn parse(text: &str) -> Diag {
        let mut d = Diag::default();
        for w in text.split_whitespace() {
            let Some((k, v)) = w.split_once('=') else { continue };
            let on = v != "0";
            match k {
                "scale" => d.scale = v.parse().unwrap_or(0.0),
                "msaa" => d.msaa = v.parse().unwrap_or(0),
                "sky" => d.sky = on,
                "world" => d.world = on,
                "meshes" => d.meshes = on,
                "shadows" => d.shadows = on,
                "dust" => d.dust = on,
                "additive" => d.additive = on,
                "blit" => d.blit = on,
                "repeat" => d.repeat = v.parse().unwrap_or(1u32).clamp(1, 8),
                _ => {}
            }
        }
        d
    }
}

pub struct Renderer {
    out_format: wgpu::TextureFormat,
    quality: Quality,
    diag: Diag,
    size: (u32, u32),
    shaders: Shaders,
    pipe_layout: wgpu::PipelineLayout,
    comp_bgl: wgpu::BindGroupLayout,
    pipes1: Pipes,
    pipes4: Option<Pipes>,
    blit_pipe: wgpu::RenderPipeline,
    mirror_pipe: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    store: MeshStore,
    ident_inst: wgpu::Buffer,
    slots: Vec<FrameSlot>,
    slot_i: usize,
    targets: Option<Targets>,
    mirror: Option<MirrorTargets>,
    world: Option<WorldGpu>,
    default_palette: Palette,
    particles: ParticleSystem,
    stats: RenderStats,
    alpha_tmp: Vec<ParticleInst>,
    add_tmp: Vec<ParticleInst>,
}

pub fn ease_out_elastic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let c4 = std::f32::consts::TAU / 3.0;
    2f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * c4).sin() + 1.0
}

fn frustum_planes(m: &Mat4) -> [Vec4; 6] {
    let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
    [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2]
}

fn aabb_visible(planes: &[Vec4; 6], lo: Vec3, hi: Vec3) -> bool {
    for p in planes {
        let pv = Vec3::new(if p.x >= 0.0 { hi.x } else { lo.x }, if p.y >= 0.0 { hi.y } else { lo.y }, if p.z >= 0.0 { hi.z } else { lo.z });
        if p.truncate().dot(pv) + p.w < 0.0 {
            return false;
        }
    }
    true
}

fn dist_to_aabb(p: Vec3, lo: Vec3, hi: Vec3) -> f32 {
    (p.clamp(lo, hi) - p).length()
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lin4(c: Rgb, w: f32) -> [f32; 4] {
    let l = srgb3_to_linear(c);
    [l[0], l[1], l[2], w]
}

impl Renderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, surface_format: wgpu::TextureFormat, quality: Quality) -> Renderer {
        let shaders = Shaders::new(device);
        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("race_globals_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                // The dust puff noise (read by the particle fragment shader only).
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let noise_tex = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("race_dust_noise"),
                size: wgpu::Extent3d { width: DUST_TEX, height: DUST_TEX, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &crate::particles::dust_noise(DUST_TEX as usize),
        );
        let noise_view = noise_tex.create_view(&Default::default());
        let noise_samp = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("race_dust_sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let pipe_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("race_pipe_layout"),
            bind_group_layouts: &[Some(&globals_bgl)],
            immediate_size: 0,
        });
        let comp_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("race_comp_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let comp_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("race_comp_layout"),
            bind_group_layouts: &[Some(&comp_bgl)],
            immediate_size: 0,
        });
        let comp_pipe = |label: &str, vs: &str, fs: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&comp_layout),
                vertex: wgpu::VertexState { module: &shaders.composite, entry_point: Some(vs), compilation_options: Default::default(), buffers: &[] },
                fragment: Some(wgpu::FragmentState {
                    module: &shaders.composite,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: surface_format, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let blit_pipe = comp_pipe("race_blit", "vs_full", "fs_blit", None);
        let mirror_pipe = comp_pipe("race_mirror", "vs_rect", "fs_mirror", Some(wgpu::BlendState::ALPHA_BLENDING));
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("race_linear_clamp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let pipes1 = Pipes::new(device, &shaders, &pipe_layout, 1);
        let pipes4 = (quality.msaa_samples() > 1).then(|| Pipes::new(device, &shaders, &pipe_layout, 4));
        let store = MeshStore::new(device);
        let ident = Inst { m: Mat4::IDENTITY.to_cols_array_2d(), t0: [1.0, 1.0, 1.0, 0.0], t1: [1.0, 1.0, 1.0, 0.0] };
        let ident_inst = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("race_ident_inst"),
            contents: bytemuck::bytes_of(&ident),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mk_buf = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage, mapped_at_creation: false })
        };
        let slots = (0..SLOTS)
            .map(|i| {
                let globals_main = mk_buf(&format!("race_globals_main{i}"), std::mem::size_of::<Globals>() as u64, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST);
                let globals_mirror = mk_buf(&format!("race_globals_mirror{i}"), std::mem::size_of::<Globals>() as u64, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST);
                let bg = |b: &wgpu::Buffer| {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("race_globals_bg"),
                        layout: &globals_bgl,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: b.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&noise_view) },
                            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&noise_samp) },
                        ],
                    })
                };
                FrameSlot {
                    dynbuf: mk_buf(&format!("race_dyn{i}"), SLOT_BYTES, wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST),
                    bg_main: bg(&globals_main),
                    bg_mirror: bg(&globals_mirror),
                    globals_main,
                    globals_mirror,
                    blit_u: mk_buf(&format!("race_blit_u{i}"), std::mem::size_of::<CompU>() as u64, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST),
                    mirror_u: mk_buf(&format!("race_mirror_u{i}"), std::mem::size_of::<CompU>() as u64, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST),
                }
            })
            .collect();

        Renderer {
            out_format: surface_format,
            quality,
            diag: Diag::default(),
            size: (1280, 720),
            shaders,
            pipe_layout,
            comp_bgl,
            pipes1,
            pipes4,
            blit_pipe,
            mirror_pipe,
            sampler,
            store,
            ident_inst,
            slots,
            slot_i: 0,
            targets: None,
            mirror: None,
            world: None,
            default_palette: Palette::savanna(),
            particles: ParticleSystem::new(quality.particle_scale()),
            stats: RenderStats::default(),
            alpha_tmp: Vec::new(),
            add_tmp: Vec::new(),
        }
    }

    pub fn set_quality(&mut self, q: Quality) {
        self.quality = q;
        self.particles.set_scale(q.particle_scale());
        self.targets = None;
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    /// Measurement knobs (see [`Diag`]); targets rebuild if scale or MSAA changed.
    pub fn set_diag(&mut self, d: Diag) {
        if d != self.diag {
            self.diag = d;
            self.targets = None;
        }
    }

    fn eff_scale(&self) -> f32 {
        if self.diag.scale > 0.0 { self.diag.scale.clamp(0.25, 1.0) } else { self.quality.render_scale() }
    }

    fn eff_samples(&self) -> u32 {
        match self.diag.msaa {
            0 => self.quality.msaa_samples(),
            n => if n >= 4 { 4 } else { 1 },
        }
    }

    /// Size of the output (the surface) in pixels.
    pub fn resize(&mut self, w: u32, h: u32) {
        let s = (w.max(2), h.max(2));
        if s != self.size {
            self.size = s;
            self.targets = None;
        }
    }

    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// Advance the particle system without drawing (menus, paused frames).
    pub fn step_particles(&mut self, scene: &FrameScene, dt: f32) {
        let fallback = GroundQuery::empty();
        let ground = self.world.as_ref().map(|w| &w.ground).unwrap_or(&fallback);
        self.particles.update(dt, scene, scene.camera.eye, &|x, y, z| ground.height_y(x, y, z));
    }

    /// Ground height under (x, z) from the loaded world.
    pub fn ground_height(&self, x: f32, z: f32) -> f32 {
        self.world.as_ref().map_or(0.0, |w| w.ground.height(x, z))
    }

    /// Build the static meshes of a track. Call once per track.
    pub fn load_world(&mut self, device: &wgpu::Device, _queue: &wgpu::Queue, w: &WorldGeometry) {
        let baked = bake(w);
        log::info!("race-render: world baked: {} static tris in {} chunks, {} decor tris in {} chunks", baked.idx.len() / 3, baked.chunks.len(), baked.decor_idx.len() / 3, baked.decor_chunks.len());
        let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("race_world_vb"),
            contents: bytemuck::cast_slice::<Vertex, u8>(&baked.verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("race_world_ib"),
            contents: bytemuck::cast_slice(&baked.idx),
            usage: wgpu::BufferUsages::INDEX,
        });
        let (pad_vb, pad_ib) = if baked.pad_idx.is_empty() {
            (None, None)
        } else {
            (
                Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("race_pad_vb"),
                    contents: bytemuck::cast_slice(&baked.pad_verts),
                    usage: wgpu::BufferUsages::VERTEX,
                })),
                Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("race_pad_ib"),
                    contents: bytemuck::cast_slice(&baked.pad_idx),
                    usage: wgpu::BufferUsages::INDEX,
                })),
            )
        };
        let (decor_vb, decor_ib) = if baked.decor_idx.is_empty() {
            (None, None)
        } else {
            (
                Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("race_decor_vb"),
                    contents: bytemuck::cast_slice::<DecorVertex, u8>(&baked.decor_verts),
                    usage: wgpu::BufferUsages::VERTEX,
                })),
                Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("race_decor_ib"),
                    contents: bytemuck::cast_slice(&baked.decor_idx),
                    usage: wgpu::BufferUsages::INDEX,
                })),
            )
        };
        self.particles.ambient_dust = w.palette.shoulder;
        self.particles.clear();
        let mut fx = w.fx.clone();
        fx.fluids.clear();
        fx.ramps.clear();
        // Cache soft contact shadows with the world; nearby ones share the
        // racers' existing shadow pass and need no per-frame terrain query.
        let scenery_shadows = w.scenery.iter().filter_map(|s| {
            let (radius, aspect, alpha) = match s.kind {
                SceneryKind::Acacia => (2.7, 0.85, 0.25),
                SceneryKind::Baobab => (2.6, 0.9, 0.28),
                SceneryKind::Palm => (1.8, 1.1, 0.22),
                SceneryKind::Cactus => (0.8, 1.2, 0.28),
                SceneryKind::Rock => (1.5, 0.9, 0.30),
                SceneryKind::Bush => (1.2, 0.9, 0.26),
                SceneryKind::HayBale | SceneryKind::Barrel | SceneryKind::TyreStack => (0.8, 0.95, 0.34),
                SceneryKind::Tent => (2.4, 1.0, 0.24),
                SceneryKind::Hoodoo => (1.8, 0.9, 0.28),
                _ => return None,
            };
            let (y, n) = baked.ground.height_normal_y(s.pos.x, Some(s.pos.y), s.pos.z);
            Some(ParticleInst {
                a: [s.pos.x, y, s.pos.z, radius * s.scale.max(0.01)],
                b: [n.x, n.y, n.z, s.yaw],
                c: [alpha, aspect, 0.0, 0.0],
            })
        }).collect();
        self.world = Some(WorldGpu {
            decor_vb,
            decor_ib,
            decor_chunks: baked.decor_chunks,
            fx,
            road: w.road.clone(),
            pads: w.pads.clone(),
            vb,
            ib,
            chunks: baked.chunks,
            pad_vb,
            pad_ib,
            pad_count: baked.pad_idx.len() as u32,
            palette: w.palette.clone(),
            ground: baked.ground,
            scenery_shadows,
        });
    }

    fn palette(&self) -> &Palette {
        self.world.as_ref().map_or(&self.default_palette, |w| &w.palette)
    }

    fn ensure_pipes(&mut self, device: &wgpu::Device) {
        if self.eff_samples() > 1 && self.pipes4.is_none() {
            self.pipes4 = Some(Pipes::new(device, &self.shaders, &self.pipe_layout, 4));
        }
    }

    fn make_view(&self, device: &wgpu::Device, label: &str, w: u32, h: u32, samples: u32, format: wgpu::TextureFormat, sampled: bool) -> wgpu::TextureView {
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if sampled {
            usage |= wgpu::TextureUsages::TEXTURE_BINDING;
        }
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
            .create_view(&Default::default())
    }

    fn comp_bgs(&self, device: &wgpu::Device, view: &wgpu::TextureView, mirror: bool) -> Vec<wgpu::BindGroup> {
        self.slots
            .iter()
            .map(|s| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("race_comp_bg"),
                    layout: &self.comp_bgl,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                        wgpu::BindGroupEntry { binding: 2, resource: (if mirror { &s.mirror_u } else { &s.blit_u }).as_entire_binding() },
                    ],
                })
            })
            .collect()
    }

    fn ensure_targets(&mut self, device: &wgpu::Device) {
        let scale = self.eff_scale();
        let size = (((self.size.0 as f32 * scale).round() as u32).max(16), ((self.size.1 as f32 * scale).round() as u32).max(16));
        let samples = self.eff_samples();
        if self.targets.as_ref().map_or(false, |t| t.size == size && t.samples == samples) {
            return;
        }
        let resolve = self.make_view(device, "race_scene", size.0, size.1, 1, SCENE_FORMAT, true);
        let msaa = (samples > 1).then(|| self.make_view(device, "race_scene_msaa", size.0, size.1, samples, SCENE_FORMAT, false));
        let depth = self.make_view(device, "race_depth", size.0, size.1, samples, DEPTH_FORMAT, false);
        let blit_bgs = self.comp_bgs(device, &resolve, false);
        self.targets = Some(Targets { size, samples, msaa, resolve, depth, blit_bgs });
    }

    fn ensure_mirror(&mut self, device: &wgpu::Device) {
        if self.mirror.is_some() {
            return;
        }
        let color = self.make_view(device, "race_mirror_color", MIRROR_W, MIRROR_H, 1, SCENE_FORMAT, true);
        let depth = self.make_view(device, "race_mirror_depth", MIRROR_W, MIRROR_H, 1, DEPTH_FORMAT, false);
        let bgs = self.comp_bgs(device, &color, true);
        self.mirror = Some(MirrorTargets { color, depth, bgs });
    }

    fn globals(&self, cam: &Camera, aspect: f32, time: f32, px: (u32, u32), fog_scale: f32, start_lights: f32) -> Globals {
        let pal = self.palette();
        let vp = cam.view_proj(aspect);
        let fwd = (cam.target - cam.eye).normalize_or_zero();
        let right = fwd.cross(cam.up).normalize_or_zero();
        let up = right.cross(fwd);
        let sky = self.world.as_ref().map(|w| w.fx.sky).unwrap_or_default();
        let hill = srgb3_to_linear(sky.hill_color.unwrap_or_else(|| crate::color::mul3(crate::color::lerp3(pal.wall, pal.ground, 0.5), 0.6)));
        let hill_id = match sky.hills {
            HillStyle::Rolling => 0.0,
            HillStyle::Dunes => 1.0,
            HillStyle::Mesas => 2.0,
            HillStyle::Peaks => 3.0,
            HillStyle::Skyline => 4.0,
        };
        let sun = pal.sun_dir.normalize_or_zero();
        Globals {
            view_proj: vp.to_cols_array_2d(),
            inv_view_proj: vp.inverse().to_cols_array_2d(),
            cam_pos: [cam.eye.x, cam.eye.y, cam.eye.z, time],
            cam_right: [right.x, right.y, right.z, 0.0],
            cam_up: [up.x, up.y, up.z, 0.0],
            sun_dir: [sun.x, sun.y, sun.z, if pal.night { 1.0 } else { 0.0 }],
            sun_color: lin4(pal.sun_color, 0.0),
            amb_sky: lin4(pal.ambient_sky, 0.0),
            amb_ground: lin4(pal.ambient_ground, 0.0),
            fog: lin4(pal.fog_color, pal.fog_density * fog_scale.max(0.0)),
            sky_top: lin4(pal.sky_top, 0.0),
            sky_hor: lin4(pal.sky_horizon, 0.0),
            hill: [hill[0], hill[1], hill[2], if pal.night { 0.75 } else { 1.0 }],
            screen: [px.0 as f32, px.1 as f32, 1.0 / px.0 as f32, 1.0 / px.1 as f32],
            fx0: [sky.cloud_cover, hill_id, sky.band_strength, sky.sun_scale.max(0.1)],
            fx1: lin4(sky.cloud_color, sky.hill_glow_strength),
            fx2: lin4(sky.band_color, start_lights),
            fx3: lin4(sky.hill_glow, 0.0),
        }
    }

    /// Ambient detail around the camera: drifting motes, pad halos and night-highway light trails.
    /// All additive sprites/beams that go into the existing additive draws (no extra draw calls).
    fn ambient_fx(&self, scene: &FrameScene, cam: Vec3, glows: &mut Vec<ParticleInst>, beams: &mut Vec<ParticleInst>) {
        let Some(w) = &self.world else { return };
        let t = scene.time;
        let gy = w.ground.height_y(cam.x, cam.y, cam.z);
        let glow = |p: Vec3, size: f32, c: [f32; 3], a: f32| ParticleInst { a: [p.x, p.y, p.z, size], b: [0.0, 1.0, KIND_GLOW, 0.0], c: [c[0], c[1], c[2], a] };
        // Motes.
        let (n, bx, by, vel, size, alpha): (i32, f32, f32, Vec3, f32, f32) = match w.fx.motes {
            MoteKind::None => (0, 1.0, 1.0, Vec3::ZERO, 0.0, 0.0),
            MoteKind::Embers => (44, 60.0, 18.0, Vec3::new(0.9, 2.4, 0.4), 0.3, 0.95),
            MoteKind::Fireflies => (30, 36.0, 4.0, Vec3::new(0.15, 0.1, 0.1), 0.26, 1.0),
            MoteKind::Dust => (36, 32.0, 9.0, Vec3::new(0.5, 0.04, 0.2), 0.16, 0.5),
            MoteKind::Sparks => (30, 44.0, 8.0, Vec3::new(0.0, 0.5, 0.0), 0.2, 0.9),
        };
        if n > 0 {
            let col = lin4(w.fx.mote_color, 0.0);
            for i in 0..n {
                let h = |k: i32| crate::mesh::hash01(i, k, 701);
                let sp = 0.6 + 0.8 * h(3);
                let wrap = |v: f32, s: f32| v.rem_euclid(s);
                let mut d = vel * (t * sp);
                if w.fx.motes == MoteKind::Fireflies {
                    d = Vec3::new((t * 0.4 * sp + h(4) * 9.0).sin() * 2.0, (t * 0.7 * sp + h(5) * 9.0).sin() * 0.8, (t * 0.33 * sp + h(6) * 9.0).cos() * 2.0);
                }
                let rx = wrap(h(0) * bx + d.x - cam.x + bx * 0.5, bx) - bx * 0.5;
                let rz = wrap(h(1) * bx + d.z - cam.z + bx * 0.5, bx) - bx * 0.5;
                let ry = if w.fx.motes == MoteKind::Fireflies { 0.6 + h(2) * by + d.y } else { wrap(h(2) * by + d.y, by) };
                let p = Vec3::new(cam.x + rx, gy + ry, cam.z + rz);
                let dist = (p - cam).length();
                let mut a = alpha * smoothstep(2.5, 7.0, dist) * (1.0 - smoothstep(bx * 0.35, bx * 0.5, dist));
                match w.fx.motes {
                    MoteKind::Embers | MoteKind::Sparks => a *= (std::f32::consts::PI * (ry / by)).sin().max(0.0),
                    MoteKind::Fireflies => a *= (0.5 + 0.5 * (t * (1.1 + h(7)) + h(8) * 30.0).sin()).powf(3.0),
                    _ => {}
                }
                if a > 0.02 {
                    glows.push(glow(p, size * (0.7 + 0.6 * h(9)), [col[0], col[1], col[2]], a));
                }
            }
        }
        // Halos over boost pads near the camera.
        for p in &w.pads {
            if (p.pos - cam).length_squared() < 140.0 * 140.0 {
                let pulse = 0.75 + 0.25 * (t * 7.0).sin();
                glows.push(glow(p.pos + Vec3::Y * 0.9, 2.6 + 0.5 * pulse, [1.0, 0.42, 0.04], 0.34 * pulse));
            }
        }
        // Light trails on the far lanes of a night highway.
        if w.fx.light_trails && w.road.len() > 8 {
            if let Some((ci, _)) = w.ground.nearest(cam.x, cam.z) {
                let nroad = w.road.len() as i32;
                for k in 0..14 {
                    let h = |j: i32| crate::mesh::hash01(k, j, 733);
                    let dir = if k % 2 == 0 { 1 } else { -1 };
                    let speed = 0.05 + 0.05 * h(1);
                    let off = ((h(0) + t * speed * dir as f32).rem_euclid(1.0) * 2.0 - 1.0) * 420.0;
                    let idx = (ci as i32 + off as i32).rem_euclid(nroad) as usize;
                    let s = &w.road[idx];
                    let nx = &w.road[(idx + 3) % w.road.len()];
                    let f = (nx.pos - s.pos).normalize_or_zero();
                    let side = if k % 4 < 2 { 1.0 } else { -1.0 };
                    let lat = s.width * 0.5 + (if side > 0.0 { s.shoulder_r } else { s.shoulder_l }) + 9.0 + 5.0 * h(2);
                    let p = s.pos + s.right * side * lat + Vec3::Y * (0.9 + 0.5 * h(3));
                    let tail = p - f * dir as f32 * (14.0 + 14.0 * h(4));
                    let c = if dir > 0 { [1.0, 0.85, 0.5] } else { [1.0, 0.1, 0.07] };
                    let lc = lin4(c, 0.0);
                    if (p - cam).length_squared() > 30.0 * 30.0 {
                        beams.push(ParticleInst { a: [p.x, p.y, p.z, 0.22], b: [tail.x, tail.y, tail.z, 1.0], c: [lc[0], lc[1], lc[2], 0.0] });
                    }
                }
            }
        }
    }

    /// Build the instance lists for the frame.
    fn assemble(&mut self, scene: &FrameScene, cam_pos: Vec3, dt: f32, out: &mut Vec<u8>) -> FrameLayout {
        let time = scene.time;
        let mut buckets: Vec<Vec<Inst>> = (0..MESH_COUNT).map(|_| Vec::new()).collect();
        let mut shadows: Vec<ParticleInst> = Vec::new();
        let mut beams: Vec<ParticleInst> = Vec::new();
        let mut glows: Vec<ParticleInst> = Vec::new();

        let inst = |m: Mat4, t0: [f32; 4], t1: [f32; 4]| Inst { m: m.to_cols_array_2d(), t0, t1 };
        let white = [1.0, 1.0, 1.0, 0.0];
        let (world_ground, ground_fallback) = (self.world.as_ref().map(|w| &w.ground), GroundQuery::empty());
        let ground = world_ground.unwrap_or(&ground_fallback);
        let shadow = |shadows: &mut Vec<ParticleInst>, p: Vec3, radius: f32, aspect: f32, yaw: f32, alpha: f32| {
            let (gy, n) = ground.height_normal_y(p.x, Some(p.y), p.z);
            let h = (p.y - gy).max(0.0);
            let fade = 1.0 / (1.0 + h * 0.35);
            let r = radius * (1.0 + h * 0.08);
            shadows.push(ParticleInst { a: [p.x, gy, p.z, r], b: [n.x, n.y, n.z, yaw], c: [alpha * fade, aspect, 0.0, 0.0] });
        };

        if let Some(w) = &self.world {
            for cached in &w.scenery_shadows {
                let distance = Vec3::new(cached.a[0], cached.a[1], cached.a[2]).distance(cam_pos);
                if distance < 85.0 {
                    let mut s = *cached;
                    s.c[0] *= ((85.0 - distance) / 20.0).clamp(0.0, 1.0);
                    shadows.push(s);
                }
            }
        }

        // Ostriches.
        for p in &scene.ostriches {
            for part in ostrich_parts(p, time) {
                buckets[part.mesh as usize].push(inst(part.model, lin4(part.tint0, 0.0), lin4(part.tint1, 0.0)));
            }
            for fx in crate::ostrich::turbo_fx(p, time, Some(cam_pos)).into_iter().chain(crate::ostrich::trump_fx(p, time)) {
                match fx {
                    crate::ostrich::Fx::Beam { from, to, width, intensity, color } => {
                        let c = lin4(color, 0.0);
                        beams.push(ParticleInst { a: [from.x, from.y, from.z, width], b: [to.x, to.y, to.z, intensity], c });
                    }
                    crate::ostrich::Fx::Glow { pos, size, alpha, color } => {
                        let c = lin4(color, alpha);
                        glows.push(ParticleInst { a: [pos.x, pos.y, pos.z, size], b: [0.0, 1.0, KIND_GLOW, 0.0], c });
                    }
                }
            }
            let sc = p.scale;
            shadow(&mut shadows, p.pos, 0.95 * sc, 1.45, p.yaw + p.visual_yaw, if p.airborne { 0.5 } else { 0.62 });
            if p.laser_on && !p.hide_head {
                for e in crate::ostrich::eye_positions(p) {
                    glows.push(ParticleInst { a: [e.x, e.y, e.z, 0.55], b: [0.0, 1.0, KIND_GLOW, 0.0], c: [1.0, 0.08, 0.04, 0.9] });
                }
            }
        }
        for rug in &scene.rugs {
            for part in crate::riders::rug(rug, time) {
                buckets[part.mesh as usize].push(inst(
                    part.model,
                    lin4(part.tint0, 0.0),
                    lin4(part.tint1, 0.0),
                ));
            }
        }
        for frog in &scene.frogs {
            for part in crate::riders::frog(frog, time) {
                buckets[part.mesh as usize].push(inst(part.model, lin4(part.tint0, 0.0), lin4(part.tint1, 0.0)));
            }
            shadow(&mut shadows, frog.pos, 0.65, 1.15, frog.yaw, 0.5);
        }
        for rose in &scene.roses {
            for part in crate::riders::rose_seller(rose, time) {
                buckets[part.mesh as usize].push(inst(part.model, lin4(part.tint0, 0.0), lin4(part.tint1, 0.0)));
            }
            if rose.leave < 0.9 {
                shadow(&mut shadows, rose.pos, 0.45, 1.3, rose.yaw, 0.45);
            }
        }

        // Drill props and route markers (updated without reloading the world).
        for p in &scene.drill_props {
            let m = crate::props::model(p);
            buckets[crate::props::mesh(p.kind) as usize].push(inst(m, white, white));
            match p.kind {
                DrillPropKind::Dust | DrillPropKind::Finish => {}
                DrillPropKind::Log => shadow(&mut shadows, p.pos, p.size.z * 0.55, p.size.x / p.size.z, p.yaw + std::f32::consts::FRAC_PI_2, 0.45),
                DrillPropKind::Cone | DrillPropKind::RingCone if p.knocked > 0.0 => {
                    let centre = m.transform_point3(Vec3::Y * 0.5);
                    shadow(&mut shadows, centre, p.size.z * 0.5, 1.0 + p.knocked.clamp(0.0, 1.0) * p.size.y / p.size.z, p.yaw, 0.4);
                }
                _ => shadow(&mut shadows, p.pos, p.size.x * 0.6, 1.0, p.yaw, 0.4),
            }
        }
        // Coins.
        for c in &scene.coins {
            if !c.visible {
                continue;
            }
            let m = Mat4::from_translation(c.pos) * Mat4::from_rotation_y(c.spin);
            buckets[MeshId::Coin as usize].push(inst(m, white, white));
            shadow(&mut shadows, c.pos, 0.45, 1.0, 0.0, 0.35);
        }
        // Power Egg targets.
        for t in &scene.targets {
            if !t.alive {
                continue;
            }
            let bob = (time * 2.2 + t.pos.x * 0.7).sin() * 0.035;
            let m = Mat4::from_translation(t.pos + Vec3::Y * bob) * Mat4::from_rotation_y(time * 0.3 + t.pos.z) * Mat4::from_scale(Vec3::splat(0.48));
            let gold = lin4([1.0, 0.78, 0.22], 0.0);
            buckets[MeshId::TargetEgg as usize].push(inst(m, gold, white));
            let pulse = 0.8 + 0.2 * (time * 5.0 + t.pos.x).sin();
            glows.push(ParticleInst { a: [t.pos.x, t.pos.y + bob, t.pos.z, 1.0 * pulse], b: [0.0, 1.0, KIND_GLOW, 0.0], c: [1.0, 0.5, 0.08, 0.34] });
            shadow(&mut shadows, t.pos, 0.4, 1.0, 0.0, 0.4);
        }
        // Eggs.
        for e in &scene.projectiles {
            let roll = time * 16.0;
            let m = Mat4::from_translation(e.pos) * Mat4::from_rotation_y(e.yaw) * Mat4::from_rotation_z(roll);
            match e.kind {
                ProjectileKind::DollarBill => {
                    let flutter = Mat4::from_rotation_z((time * 18.0 + e.pos.x).sin() * 0.45);
                    let note =
                        Mat4::from_translation(e.pos) * Mat4::from_rotation_y(e.yaw) * flutter;
                    buckets[MeshId::Block as usize].push(inst(
                        note * Mat4::from_scale(Vec3::new(1.1, 0.48, 0.035)),
                        lin4([0.76, 0.92, 0.65], 0.0),
                        white,
                    ));
                    buckets[MeshId::Block as usize].push(inst(
                        note * Mat4::from_scale(Vec3::new(0.92, 0.34, 0.045)),
                        lin4([0.12, 0.47, 0.21], 0.0),
                        white,
                    ));
                    buckets[MeshId::SmallBall as usize].push(inst(
                        note * Mat4::from_scale(Vec3::new(0.13, 0.13, 0.03)),
                        lin4([0.75, 0.88, 0.6], 0.0),
                        white,
                    ));
                }
                ProjectileKind::Egg => buckets[MeshId::Egg as usize].push(inst(m, white, white)),
                ProjectileKind::HunterEgg => {
                    buckets[MeshId::HunterEgg as usize].push(inst(m, white, white));
                    glows.push(ParticleInst { a: [e.pos.x, e.pos.y, e.pos.z, 2.0], b: [0.0, 1.0, KIND_GLOW, 0.0], c: [1.0, 0.1, 0.1, 0.5] });
                }
            }
            shadow(&mut shadows, e.pos, 0.6, 1.5, e.yaw, 0.5);
        }
        // Cactus hazards.
        for h in &scene.hazards {
            let yaw = (h.pos.x * 3.7 + h.pos.z * 1.3).sin() * 3.0;
            let m = Mat4::from_translation(h.pos) * Mat4::from_rotation_y(yaw) * Mat4::from_scale(Vec3::splat(1.5));
            buckets[MeshId::CactusHazard as usize].push(inst(m, white, white));
            shadow(&mut shadows, h.pos, 0.8, 1.0, 0.0, 0.5);
        }
        // Shields.
        for s in &scene.shields {
            let pulse = if s.aura { 1.0 + 0.035 * (time * 4.0).sin() } else { 1.0 };
            if s.aura {
                glows.push(ParticleInst { a: [s.pos.x, s.pos.y, s.pos.z, s.radius * 3.1 * pulse], b: [0.0, 1.0, KIND_AURA, 0.0], c: [1.0, 0.18, 0.005, 0.65 * s.alpha] });
                for k in 0..6 {
                    let a = time * 1.8 + k as f32 * std::f32::consts::TAU / 6.0;
                    let p = s.pos + Vec3::new(a.cos() * s.radius, (a * 1.5 + time).sin() * s.radius * 0.65, a.sin() * s.radius);
                    glows.push(ParticleInst { a: [p.x, p.y, p.z, 0.25], b: [0.0, 1.0, KIND_GLOW, 0.0], c: [1.0, 0.38, 0.015, 0.8 * s.alpha] });
                }
            } else {
                let m = Mat4::from_translation(s.pos) * Mat4::from_scale(Vec3::splat(s.radius));
                buckets[MeshId::Shield as usize].push(inst(m, lin4([0.35, 0.85, 1.0], s.alpha.clamp(0.0, 1.0)), white));
            }
        }
        // Beams.
        for b in &scene.beams {
            let w = (b.width * 1.8).clamp(0.05, 1.25);
            beams.push(ParticleInst { a: [b.from.x, b.from.y, b.from.z, w], b: [b.to.x, b.to.y, b.to.z, b.intensity], c: [lin4(b.color, 0.0)[0], lin4(b.color, 0.0)[1], lin4(b.color, 0.0)[2], 0.0] });
            let col = lin4(b.color, 0.0);
            glows.push(ParticleInst { a: [b.to.x, b.to.y, b.to.z, (b.width * 4.0).clamp(0.3, 1.5)], b: [0.0, 1.0, KIND_GLOW, 0.0], c: [col[0], col[1], col[2], 0.7 * b.intensity.min(1.5)] });
            glows.push(ParticleInst { a: [b.from.x, b.from.y, b.from.z, (b.width * 3.0).clamp(0.25, 0.8)], b: [0.0, 1.0, KIND_GLOW, 0.0], c: [col[0], col[1], col[2], 0.5 * b.intensity.min(1.5)] });
        }

        self.ambient_fx(scene, cam_pos, &mut glows, &mut beams);

        // Particles: spawn, step, collect.
        let ground_fn = |x: f32, y: f32, z: f32| ground.height_y(x, y, z);
        self.particles.update(dt, scene, cam_pos, &ground_fn);
        let mut bolts = Vec::new();
        self.particles.bolt_segments(&mut bolts);
        for s in bolts {
            beams.push(ParticleInst { a: [s.from.x, s.from.y, s.from.z, s.width], b: [s.to.x, s.to.y, s.to.z, s.intensity], c: [0.55, 0.75, 1.0, 0.0] });
        }
        let mut alpha = std::mem::take(&mut self.alpha_tmp);
        let mut add = std::mem::take(&mut self.add_tmp);
        self.particles.instances(cam_pos, &mut alpha, &mut add);
        add.extend(glows);

        // Pack into one byte blob.
        out.clear();
        let mut layout = FrameLayout::default();
        let cap_inst = 4096usize;
        let mut total_inst = 0usize;
        for (i, v) in buckets.iter().enumerate() {
            if v.is_empty() || i == MeshId::Shield as usize {
                continue;
            }
            let n = v.len().min(cap_inst - total_inst.min(cap_inst));
            if n == 0 {
                break;
            }
            total_inst += n;
            layout.inst.push((crate::meshes::ALL[i], out.len() as u64, n as u32));
            out.extend_from_slice(bytemuck::cast_slice(&v[..n]));
        }
        let put = |v: &[ParticleInst], max: usize, out: &mut Vec<u8>| -> (u64, u32) {
            let n = v.len().min(max);
            let off = out.len() as u64;
            out.extend_from_slice(bytemuck::cast_slice(&v[..n]));
            (off, n as u32)
        };
        layout.shadows = put(&shadows, 512, out);
        layout.beams = put(&beams, 512, out);
        let sh = &buckets[MeshId::Shield as usize];
        let n = sh.len().min(32);
        layout.shields = (out.len() as u64, n as u32);
        out.extend_from_slice(bytemuck::cast_slice(&sh[..n]));
        layout.alpha = put(&alpha, 4096, out);
        layout.additive = put(&add, 4096, out);
        self.alpha_tmp = alpha;
        self.add_tmp = add;
        debug_assert!((out.len() as u64) <= SLOT_BYTES);
        layout
    }

    /// Draw the whole 3D scene into `target_view` (the app draws egui on top).
    /// One call per `queue.submit` of `encoder`.
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, target_view: &wgpu::TextureView, scene: &FrameScene, dt: f32) {
        self.stats = RenderStats::default();
        self.ensure_pipes(device);
        self.ensure_targets(device);
        let mirror_on = scene.mirror.as_ref().map_or(false, |m| m.scale > 0.005);
        if mirror_on {
            self.ensure_mirror(device);
        }
        self.slot_i = (self.slot_i + 1) % SLOTS;
        let si = self.slot_i;

        // Camera with shake.
        let mut cam = scene.camera;
        cam.eye += scene.shake;
        cam.target += scene.shake;
        let tsize = self.targets.as_ref().unwrap().size;
        let aspect = tsize.0 as f32 / tsize.1 as f32;

        let mut bytes = Vec::with_capacity(256 * 1024);
        let layout = self.assemble(scene, cam.eye, dt, &mut bytes);
        let n_particles = (layout.alpha.1 + layout.additive.1) as u32;
        let slot = &self.slots[si];
        queue.write_buffer(&slot.dynbuf, 0, &bytes);
        let gl_main = self.globals(&cam, aspect, scene.time, tsize, scene.fog_scale, scene.start_lights);
        queue.write_buffer(&slot.globals_main, 0, bytemuck::bytes_of(&gl_main));
        let main_cam = PassCam {
            planes: frustum_planes(&cam.view_proj(aspect)),
            pos: cam.eye,
            max_dist: cam.far.min(self.fog_cutoff() / scene.fog_scale.max(0.05)),
        };

        let mut stats = RenderStats { particles: self.particles.len() as u32, ..Default::default() };
        let _ = n_particles;

        // Mirror pass.
        let mut mirror_info: Option<(MirrorView, f32)> = None;
        if let (true, Some(mv)) = (mirror_on, scene.mirror.as_ref()) {
            let mut mcam = mv.camera;
            mcam.eye += scene.shake * 0.3;
            mcam.target += scene.shake * 0.3;
            mcam.far = mcam.far.min(MIRROR_FAR);
            let m_aspect = MIRROR_W as f32 / MIRROR_H as f32;
            let gl = self.globals(&mcam, m_aspect, scene.time, (MIRROR_W, MIRROR_H), scene.fog_scale, scene.start_lights);
            queue.write_buffer(&slot.globals_mirror, 0, bytemuck::bytes_of(&gl));
            let pc = PassCam { planes: frustum_planes(&mcam.view_proj(m_aspect)), pos: mcam.eye, max_dist: mcam.far };
            let mt = self.mirror.as_ref().unwrap();
            let clear = self.clear_color();
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("race_mirror_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &mt.color,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &mt.depth,
                        depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                        stencil_ops: None,
                    }),
                    ..Default::default()
                });
                self.draw_scene(&mut pass, &self.pipes1, &slot.bg_mirror, slot, &layout, &pc, &mut stats);
            }
            mirror_info = Some((*mv, ease_out_elastic(mv.scale)));
        }

        // Main pass (repeated only by the measurement knob).
        for _ in 0..self.diag.repeat.max(1) {
            let t = self.targets.as_ref().unwrap();
            let clear = self.clear_color();
            let (color_view, resolve) = match &t.msaa {
                Some(m) => (m, Some(&t.resolve)),
                None => (&t.resolve, None),
            };
            let pipes = if t.samples > 1 { self.pipes4.as_ref().unwrap_or(&self.pipes1) } else { &self.pipes1 };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("race_main_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    depth_slice: None,
                    resolve_target: resolve,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: if resolve.is_some() { wgpu::StoreOp::Discard } else { wgpu::StoreOp::Store },
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &t.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            self.draw_scene(&mut pass, pipes, &slot.bg_main, slot, &layout, &main_cam, &mut stats);
        }

        // Composite: scaled scene to the target, then the mirror panel.
        {
            let srgb = if self.out_format.is_srgb() { 1.0 } else { 0.0 };
            let blit = CompU { params: [srgb, 0.0, 0.0, 0.0], rect: [0.0; 4], misc: [self.size.0 as f32, self.size.1 as f32, 0.0, 0.0] };
            queue.write_buffer(&slot.blit_u, 0, bytemuck::bytes_of(&blit));
            if let Some((mv, k)) = &mirror_info {
                let [x, y, w, h] = mv.rect_px;
                let kk = k.max(0.001);
                let (cx, cy) = (x + w * 0.5, y + h * 0.5);
                let (hw, hh) = (w * 0.5 * kk, h * 0.5 * kk);
                let radius = (14.0 * kk).min(hh * 0.5);
                let bevel = (7.0 * kk).max(1.5);
                let mu = CompU { params: [srgb, 0.0, 0.0, 0.0], rect: [cx, cy, hw, hh], misc: [self.size.0 as f32, self.size.1 as f32, radius, bevel] };
                queue.write_buffer(&slot.mirror_u, 0, bytemuck::bytes_of(&mu));
            }
            let t = self.targets.as_ref().unwrap();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("race_composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            if self.diag.blit {
                pass.set_pipeline(&self.blit_pipe);
                pass.set_bind_group(0, &t.blit_bgs[si], &[]);
                pass.draw(0..3, 0..1);
                stats.draw_calls += 1;
                stats.triangles += 1;
            }
            if mirror_info.is_some() {
                pass.set_pipeline(&self.mirror_pipe);
                pass.set_bind_group(0, &self.mirror.as_ref().unwrap().bgs[si], &[]);
                pass.draw(0..6, 0..1);
                stats.draw_calls += 1;
                stats.triangles += 2;
            }
        }
        self.stats = stats;
        log::debug!("race-render: {} draws, {} tris, {} particles", stats.draw_calls, stats.triangles, stats.particles);
    }

    fn clear_color(&self) -> wgpu::Color {
        let f = self.palette().fog_color;
        wgpu::Color { r: f[0] as f64, g: f[1] as f64, b: f[2] as f64, a: 1.0 }
    }

    /// Distance beyond which fog hides everything (99 %).
    fn fog_cutoff(&self) -> f32 {
        let d = self.palette().fog_density.max(1e-4);
        (4.6 / d).max(60.0)
    }

    fn visible_ranges(&self, cam: &PassCam, decor: bool) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = Vec::new();
        let Some(w) = &self.world else { return out };
        for c in if decor { &w.decor_chunks } else { &w.chunks } {
            if dist_to_aabb(cam.pos, c.lo, c.hi) > cam.max_dist || !aabb_visible(&cam.planes, c.lo, c.hi) {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.0 + last.1 == c.first => last.1 += c.count,
                _ => out.push((c.first, c.count)),
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_scene(&self, pass: &mut wgpu::RenderPass<'_>, pipes: &Pipes, globals: &wgpu::BindGroup, slot: &FrameSlot, lay: &FrameLayout, cam: &PassCam, stats: &mut RenderStats) {
        let dynbuf = &slot.dynbuf;
        pass.set_bind_group(0, globals, &[]);
        // Static world (opaque, drawn before the sky so the sky only shades what is left).
        if let Some(w) = self.world.as_ref().filter(|_| self.diag.world) {
            let ranges = self.visible_ranges(cam, false);
            if !ranges.is_empty() {
                pass.set_pipeline(&pipes.lit);
                pass.set_vertex_buffer(0, w.vb.slice(..));
                pass.set_vertex_buffer(1, self.ident_inst.slice(..));
                pass.set_index_buffer(w.ib.slice(..), wgpu::IndexFormat::Uint32);
                for (first, count) in ranges {
                    pass.draw_indexed(first..first + count, 0, 0..1);
                    stats.draw_calls += 1;
                    stats.triangles += count / 3;
                }
            }
            if let (Some(pvb), Some(pib)) = (&w.pad_vb, &w.pad_ib) {
                pass.set_pipeline(&pipes.pad);
                pass.set_vertex_buffer(0, pvb.slice(..));
                pass.set_index_buffer(pib.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..w.pad_count, 0, 0..1);
                stats.draw_calls += 1;
                stats.triangles += w.pad_count / 3;
            }
        }

        // Animated decor (crowd, flags, water, lava, start lights): vertex-animated, one chunked draw.
        if let (Some(w), true) = (self.world.as_ref(), self.diag.world) {
            if let (Some(dvb), Some(dib)) = (&w.decor_vb, &w.decor_ib) {
                let ranges = self.visible_ranges(cam, true);
                if !ranges.is_empty() {
                    pass.set_pipeline(&pipes.decor);
                    pass.set_vertex_buffer(0, dvb.slice(..));
                    pass.set_index_buffer(dib.slice(..), wgpu::IndexFormat::Uint32);
                    for (first, count) in ranges {
                        pass.draw_indexed(first..first + count, 0, 0..1);
                        stats.draw_calls += 1;
                        stats.triangles += count / 3;
                    }
                }
            }
        }

        // Instanced dynamic meshes (ostrich parts, coins, targets, eggs, hazards).
        if !lay.inst.is_empty() && self.diag.meshes {
            pass.set_pipeline(&pipes.lit);
            pass.set_vertex_buffer(0, self.store.vb.slice(..));
            pass.set_index_buffer(self.store.ib.slice(..), wgpu::IndexFormat::Uint32);
            for &(id, off, n) in &lay.inst {
                let (first, count) = self.store.ranges[id as usize];
                let bytes = n as u64 * std::mem::size_of::<Inst>() as u64;
                pass.set_vertex_buffer(1, dynbuf.slice(off..off + bytes));
                pass.draw_indexed(first..first + count, 0, 0..n);
                stats.draw_calls += 1;
                stats.triangles += count / 3 * n;
            }
        }

        // Sky: full-screen at the far plane, depth-tested, so only uncovered pixels run it.
        if self.diag.sky {
            pass.set_pipeline(&pipes.sky);
            pass.draw(0..3, 0..1);
            stats.draw_calls += 1;
            stats.triangles += 1;
        }

        let i3 = std::mem::size_of::<ParticleInst>() as u64;
        // Blob shadows.
        if lay.shadows.1 > 0 && self.diag.shadows {
            pass.set_pipeline(&pipes.shadow);
            pass.set_vertex_buffer(0, dynbuf.slice(lay.shadows.0..lay.shadows.0 + lay.shadows.1 as u64 * i3));
            pass.draw(0..6, 0..lay.shadows.1);
            stats.draw_calls += 1;
            stats.triangles += 2 * lay.shadows.1;
        }
        // Dust (alpha, far to near).
        if lay.alpha.1 > 0 && self.diag.dust {
            pass.set_pipeline(&pipes.part_alpha);
            pass.set_vertex_buffer(0, dynbuf.slice(lay.alpha.0..lay.alpha.0 + lay.alpha.1 as u64 * i3));
            pass.draw(0..6, 0..lay.alpha.1);
            stats.draw_calls += 1;
            stats.triangles += 2 * lay.alpha.1;
        }
        // Shields.
        if lay.shields.1 > 0 {
            pass.set_pipeline(&pipes.shield);
            let (first, count) = self.store.ranges[MeshId::Shield as usize];
            pass.set_vertex_buffer(0, self.store.vb.slice(..));
            pass.set_index_buffer(self.store.ib.slice(..), wgpu::IndexFormat::Uint32);
            let bytes = lay.shields.1 as u64 * std::mem::size_of::<Inst>() as u64;
            pass.set_vertex_buffer(1, dynbuf.slice(lay.shields.0..lay.shields.0 + bytes));
            pass.draw_indexed(first..first + count, 0, 0..lay.shields.1);
            stats.draw_calls += 1;
            stats.triangles += count / 3 * lay.shields.1;
        }
        // Beams and additive particles.
        if lay.beams.1 > 0 {
            pass.set_pipeline(&pipes.beam);
            pass.set_vertex_buffer(0, dynbuf.slice(lay.beams.0..lay.beams.0 + lay.beams.1 as u64 * i3));
            pass.draw(0..6, 0..lay.beams.1);
            stats.draw_calls += 1;
            stats.triangles += 2 * lay.beams.1;
        }
        if lay.additive.1 > 0 && self.diag.additive {
            pass.set_pipeline(&pipes.part_add);
            pass.set_vertex_buffer(0, dynbuf.slice(lay.additive.0..lay.additive.0 + lay.additive.1 as u64 * i3));
            pass.draw(0..6, 0..lay.additive.1);
            stats.draw_calls += 1;
            stats.triangles += 2 * lay.additive.1;
        }
    }
}
