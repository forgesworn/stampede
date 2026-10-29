//! Stampede's wgpu 3D renderer: procedural low-poly meshes, ostrich rig, particles, cameras, mirror pass.
//!
//! Plain-data API: the app fills a [`WorldGeometry`] once per track
//! ([`Renderer::load_world`]) and a [`FrameScene`] every frame
//! ([`Renderer::render`]). No dependency on race-core. See `docs/api/race-render.md`.

mod assets;
mod bridge;
pub mod capture;
mod color;
mod decor;
mod mesh;
mod meshes;
mod ostrich;
mod riders;
mod particles;
mod props;
mod renderer;
mod types;
mod world;

pub use mesh::{MeshData, Style, Vertex};
pub use meshes::{build_mesh, MeshId};
pub use riders::{knock_fx, KnockFx, SMASH_SECS, THROW_SECS};
pub use ostrich::{eye_positions, first_person_eye, foot_positions, head_transform, ostrich_parts, FirstPersonEye, OstrichPart};
pub use renderer::{ease_out_elastic, Diag, Renderer, MIRROR_H, MIRROR_W};
pub use types::*;
pub use world::GroundQuery;
