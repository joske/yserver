//! The Vulkan renderer of yserver: device bring-up, pipelines and ops
//! (`kms::vk`), and the drawable store, frame builder and engine
//! (`kms::render`). It knows nothing of DRM/KMS or of the X server core;
//! `yserver` drives it and re-exports its modules under the same paths.

pub mod kms;
