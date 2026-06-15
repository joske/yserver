//! Rendering-model-v2 backend (spec at
//! `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`).
//!
//! The sole rendering backend since v1 retired 2026-05-26 (Phase
//! B.3 close). Implements the `Backend` trait directly; `lib.rs`
//! constructs `KmsBackendV2` at startup.

mod backend;
pub(crate) mod batch_resource;
pub(crate) mod completion_poller;
pub(crate) mod composite_pool_ring;
pub(crate) mod cursor;
pub(crate) mod descriptor_pool_ring;
pub(crate) mod engine;
pub(crate) mod frame_builder;
pub(crate) mod glyph_atlas;
pub(crate) mod owned_semaphore;
pub(crate) mod platform;
pub(crate) mod present_completion;
pub(crate) mod scene;
pub(crate) mod store;
pub(crate) mod stroke;
pub(crate) mod submit_group;
pub(crate) mod submit_trace;
pub(crate) mod telemetry;

pub use backend::KmsBackendV2;
