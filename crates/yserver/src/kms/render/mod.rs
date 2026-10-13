//! KMS render backend (historical spec filename:
//! `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`).
//!
//! The sole rendering backend since v1 retired 2026-05-26 (Phase
//! B.3 close). Implements the `Backend` trait directly; `lib.rs`
//! constructs `KmsBackend` at startup.

mod backend;
pub(crate) mod completion_poller;
pub(crate) mod export_holders;
pub(crate) mod imported_syncobj;
pub(crate) mod platform;
pub(crate) mod pointer_confine;
pub(crate) mod present_completion;
pub(crate) mod present_source_wait;
pub(crate) mod probe_executor;

pub use backend::KmsBackend;
pub use yserver_render::kms::render::cpu_types;
pub(crate) use yserver_render::kms::render::{
    cursor, engine, frame_builder, geometry, glyph_pixels, gpu, owned_semaphore, present_batch,
    region, root_overlay, scene, store, stroke, submit_group, submit_trace, target, telemetry,
    window_geometry,
};
