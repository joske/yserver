//! Stable endpoint identity for one renderer-to-display scanout route.
//!
//! Render nodes and DRM primary nodes live in different identity namespaces:
//! a render-node `st_rdev` identifies the Vulkan endpoint, while a primary-node
//! `st_rdev` identifies the KMS endpoint.  Their raw major/minor pairs must
//! never be compared to decide whether the endpoints belong to the same GPU.

use crate::platform::drm::DrmDeviceKey;

/// Stable renderer endpoint identity. Verified devices are keyed by their DRM
/// render node. The explicit unverified fallback covers a selected Vulkan
/// device for which the ICD supplied no usable render-node identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RenderDeviceId {
    DrmRender(DrmDeviceKey),
    UnverifiedFallback,
}

/// What Vulkan's optional primary-node metadata says about a renderer and a
/// KMS endpoint.
///
/// `Different` is a valid split render/display topology (for example Asahi),
/// not a renderer-selection failure. `Unknown` must remain distinct from it:
/// missing metadata cannot prove either a local or a cross-device route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RenderKmsRelationship {
    Same,
    Different,
    Unknown,
}

/// Device endpoints used to render and display one scanout buffer.
///
/// The route always describes the semantic flow "renderer writes, KMS scans
/// out". Allocation ownership is independent: a pool may contain a GBM-owned
/// image imported into Vulkan or a Vulkan-owned image imported into KMS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ScanoutRoute {
    pub(crate) render_device_id: RenderDeviceId,
    pub(crate) kms_device_key: DrmDeviceKey,
    pub(crate) relationship: RenderKmsRelationship,
}

impl ScanoutRoute {
    #[must_use]
    pub(crate) const fn new(
        render_device_id: RenderDeviceId,
        kms_device_key: DrmDeviceKey,
        relationship: RenderKmsRelationship,
    ) -> Self {
        Self {
            render_device_id,
            kms_device_key,
            relationship,
        }
    }
}
