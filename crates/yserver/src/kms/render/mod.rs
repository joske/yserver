//! KMS render backend (historical spec filename:
//! `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`).
//!
//! The sole rendering backend since v1 retired 2026-05-26 (Phase
//! B.3 close). Implements the `Backend` trait directly; `lib.rs`
//! constructs `KmsBackend` at startup.

mod backend;
pub(crate) mod batch_resource;
pub(crate) mod completion_poller;
pub(crate) mod composite_pool_ring;
pub(crate) mod cursor;
pub(crate) mod cursor_save;
pub(crate) mod descriptor_pool_ring;
pub(crate) mod engine;
pub(crate) mod export_holders;
pub(crate) mod frame_builder;
pub(crate) mod glyph_atlas;
pub(crate) mod glyph_pixels;
pub(crate) mod imported_syncobj;
pub(crate) mod owned_semaphore;
pub(crate) mod platform;
pub(crate) mod pointer_confine;
pub(crate) mod present_completion;
pub(crate) mod present_source_wait;
pub(crate) mod probe_executor;
pub(crate) mod region;
pub(crate) mod root_overlay;
pub(crate) mod scanout_damage;
pub(crate) mod scene;
pub(crate) mod scene_diff;
pub(crate) mod store;
pub(crate) mod stroke;
pub(crate) mod submit_group;
pub(crate) mod submit_trace;
pub(crate) mod target;
pub(crate) mod telemetry;
pub(crate) mod transform_intermediate;
pub(crate) mod upload_arena;

pub use backend::KmsBackend;
