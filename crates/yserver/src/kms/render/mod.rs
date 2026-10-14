//! KMS render backend (historical spec filename:
//! `docs/superpowers/specs/2026-05-15-rendering-model-v2.md`).
//!
//! The sole rendering backend since v1 retired 2026-05-26 (Phase
//! B.3 close). Implements the `Backend` trait directly; `lib.rs`
//! constructs `KmsBackend` at startup.

// The KMS platform is still the only caller of parts of the portable backend
// (cursor animation, VT input release, pointer re-confinement).
#[cfg_attr(not(kms), allow(dead_code))]
mod backend;
#[cfg(kms)]
pub(crate) mod completion_poller;
#[cfg(kms)]
pub(crate) mod export_holders;
#[cfg(kms)]
pub(crate) mod imported_syncobj;
#[cfg(kms)]
pub(crate) mod platform;
#[cfg_attr(not(kms), allow(dead_code))]
pub(crate) mod pointer_confine;
pub(crate) mod present_completion;
#[cfg(kms)]
pub(crate) mod present_source_wait;
#[cfg(kms)]
pub(crate) mod probe_executor;

#[cfg(kms)]
pub use backend::KmsBackend;
#[cfg(test)]
pub(crate) use backend::seam::inject_resize_fault;
#[cfg(any(not(kms), test))]
pub(crate) use backend::{
    XBackend,
    seam::{Platform, ResizeFault, resize_fault},
};
pub use yserver_render::kms::render::cpu_types;
pub(crate) use yserver_render::kms::render::{
    cursor, engine, frame_builder, geometry, glyph_pixels, gpu, present_batch, root_overlay, scene,
    store, stroke, submit_group, submit_trace, target, telemetry, window_geometry,
};
#[cfg(kms)]
pub(crate) use yserver_render::kms::render::{owned_semaphore, region};
