//! The renderer half of the KMS render backend: the drawable store, the
//! frame builder, the engine and their helpers. `yserver` keeps the scene,
//! the platform and the `Backend` implementation, and re-exports these
//! modules under its own `kms::render`.

pub(crate) mod batch_resource;
pub mod composite_pool_ring;
pub mod cpu_types;
pub mod cursor;
pub mod cursor_save;
pub(crate) mod descriptor_pool_ring;
pub mod engine;
pub mod frame_builder;
pub mod geometry;
pub(crate) mod glyph_atlas;
pub mod glyph_pixels;
pub mod gpu;
pub mod owned_semaphore;
pub mod present_batch;
pub mod region;
pub mod root_overlay;
pub mod scanout_damage;
pub mod scene;
pub mod scene_diff;
pub mod store;
pub mod stroke;
pub mod submit_group;
pub mod submit_trace;
pub mod target;
pub mod telemetry;
pub mod transform_intermediate;
pub(crate) mod upload_arena;
pub mod window_geometry;
