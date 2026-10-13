//! Module paths mirror `yserver`'s `kms::{vk, render}` so the moved code
//! keeps its `crate::kms::…` paths.

pub mod render;
pub mod vk;

pub(crate) use render::cpu_types;
