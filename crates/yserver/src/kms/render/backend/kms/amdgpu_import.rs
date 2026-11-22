//! Resolve a legacy DRI3 `PixmapFromBuffer` dma-buf's real tiling from the
//! kernel's amdgpu GEM metadata.
//!
//! A DRI3 1.0 `PixmapFromBuffer` carries no modifier, and gbm reports
//! `DRM_FORMAT_MOD_INVALID` for an implicitly imported AMD buffer. The
//! kernel metadata still has the tiling flags, though: `PRIME_FD_TO_HANDLE`
//! imports the dma-buf into the render node, `AMDGPU_GEM_METADATA` exposes
//! `tiling_info`, and `AMDGPU_INFO_DEV_INFO` identifies the GPU family.
//! This module turns those legacy GFX8 (Polaris/Carrizo) flags into the
//! matching `AMD_FMT_MOD` so Vulkan can import the buffer with its real
//! layout instead of assuming linear.
//!
//! Only the legacy GFX6/GFX8 modifier encoding is handled here. Other
//! families return `None` and keep the old "layout unknown" path.

use std::{
    mem::size_of,
    os::fd::{AsRawFd, BorrowedFd},
};

use drm_ffi::gem;

use crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR;

// DRM_COMMAND_BASE (0x40) + DRM_AMDGPU_INFO / DRM_AMDGPU_GEM_METADATA.
const DRM_AMDGPU_INFO_IOCTL_NR: u32 = 0x45;
const DRM_AMDGPU_GEM_METADATA_IOCTL_NR: u32 = 0x46;
const AMDGPU_INFO_DEV_INFO: u32 = 0x16;
const AMDGPU_GEM_METADATA_OP_GET_METADATA: u32 = 2;

// sizeof(struct drm_amdgpu_info_device) and offsetof(..., family).
const DEV_INFO_SIZE: usize = 448;
const DEV_INFO_FAMILY_OFFSET: usize = 16;

// AMDGPU_FAMILY_VI / AMDGPU_FAMILY_CZ from amdgpu_drm.h: GFX8
// (Polaris/Tonga/Iceland and Carrizo/Stoney).
const AMDGPU_FAMILY_VI: u32 = 130;
const AMDGPU_FAMILY_CZ: u32 = 135;

const AMD_FMT_MOD: u64 = 0x0200_0000_0000_0000;

// UAPI struct drm_amdgpu_info: 32 bytes on all supported targets.
#[repr(C)]
struct DrmAmdgpuInfo {
    return_pointer: u64,
    return_size: u32,
    query: u32,
    data: [u64; 2],
}
const _: () = assert!(size_of::<DrmAmdgpuInfo>() == 32);

// UAPI struct drm_amdgpu_gem_metadata: 288 bytes.
#[repr(C)]
struct DrmAmdgpuGemMetadata {
    handle: u32,
    op: u32,
    data: DrmAmdgpuGemMetadataData,
}

#[repr(C)]
struct DrmAmdgpuGemMetadataData {
    flags: u64,
    tiling_info: u64,
    data_size_bytes: u32,
    data: [u32; 64],
}
const _: () = assert!(size_of::<DrmAmdgpuGemMetadata>() == 288);

// `_IOW('d', 0x45, struct drm_amdgpu_info)`.
const DRM_IOCTL_AMDGPU_INFO: nix::sys::ioctl::ioctl_num_type =
    nix::request_code_write!(b'd', DRM_AMDGPU_INFO_IOCTL_NR, size_of::<DrmAmdgpuInfo>());
// `_IOWR('d', 0x46, struct drm_amdgpu_gem_metadata)`.
const DRM_IOCTL_AMDGPU_GEM_METADATA: nix::sys::ioctl::ioctl_num_type = nix::request_code_readwrite!(
    b'd',
    DRM_AMDGPU_GEM_METADATA_IOCTL_NR,
    size_of::<DrmAmdgpuGemMetadata>()
);

/// Resolve the modifier for `dmabuf`, or `None` when this GPU/tiling is not
/// handled by the legacy path.
pub(super) fn resolve_modifier(
    render_node: BorrowedFd<'_>,
    dmabuf: BorrowedFd<'_>,
    width: u32,
    height: u32,
    stride: u32,
) -> Option<u64> {
    // The render node is already open; import the client's dma-buf into a
    // GEM handle so the kernel metadata can be queried. drm-ffi carries the
    // per-target PRIME/GEM ioctl encodings.
    let prime = match gem::fd_to_handle(render_node, dmabuf) {
        Ok(prime) => prime,
        Err(error) => {
            log::debug!("amdgpu import: PRIME_FD_TO_HANDLE failed: {error}");
            return None;
        }
    };
    let handle = prime.handle;

    // SAFETY: zeroed plain-old-data matching the UAPI struct.
    let mut metadata = DrmAmdgpuGemMetadata {
        handle,
        op: AMDGPU_GEM_METADATA_OP_GET_METADATA,
        data: unsafe { std::mem::zeroed() },
    };
    // SAFETY: valid render-node fd, the kernel's UAPI request code, and a
    // correctly sized `drm_amdgpu_gem_metadata`.
    let metadata_rc = unsafe {
        libc::ioctl(
            render_node.as_raw_fd(),
            DRM_IOCTL_AMDGPU_GEM_METADATA as _,
            &mut metadata,
        )
    };
    let tiling_info = if metadata_rc == -1 {
        log::debug!(
            "amdgpu import: AMDGPU_GEM_METADATA failed: {}",
            std::io::Error::last_os_error()
        );
        None
    } else {
        Some(metadata.data.tiling_info)
    };

    // `AMDGPU_INFO_DEV_INFO` writes a fixed 448-byte structure. We only need
    // `family`, which is the fifth u32.
    let mut dev_info = [0u8; DEV_INFO_SIZE];
    let mut info = DrmAmdgpuInfo {
        return_pointer: dev_info.as_mut_ptr() as u64,
        return_size: DEV_INFO_SIZE as u32,
        query: AMDGPU_INFO_DEV_INFO,
        data: [0; 2],
    };
    // SAFETY: valid render-node fd, the kernel's UAPI request code, and a
    // correctly sized `drm_amdgpu_info`; `return_pointer` points at
    // `dev_info`, which outlives the call.
    let info_rc = unsafe {
        libc::ioctl(
            render_node.as_raw_fd(),
            DRM_IOCTL_AMDGPU_INFO as _,
            &mut info,
        )
    };
    let family = if info_rc == -1 {
        log::debug!(
            "amdgpu import: AMDGPU_INFO_DEV_INFO failed: {}",
            std::io::Error::last_os_error()
        );
        None
    } else {
        Some(u32::from_ne_bytes(
            dev_info[DEV_INFO_FAMILY_OFFSET..DEV_INFO_FAMILY_OFFSET + 4]
                .try_into()
                .expect("family is four bytes"),
        ))
    };

    if let Err(error) = gem::close(render_node, handle) {
        log::debug!("amdgpu import: GEM_CLOSE failed: {error}");
    }

    let family = family?;
    let tiling_info = tiling_info?;
    let modifier = modifier_from_tiling_info(family, tiling_info)?;
    log::debug!(
        "amdgpu import: {}x{} stride={} family={family} tiling=0x{tiling_info:x} \
         -> modifier=0x{modifier:x}",
        width,
        height,
        stride,
    );
    Some(modifier)
}

fn field(tiling_info: u64, shift: u32, mask: u64) -> u32 {
    u32::try_from((tiling_info >> shift) & mask).unwrap_or(0)
}

/// Legacy GFX6/GFX8 `AMD_FMT_MOD` for the kernel tiling flags. GFX8
/// (Polaris/Carrizo) uses the CIK macrotile encodings, which are the same
/// numbers stored in `AMDGPU_TILING_{BANK_*,MACRO_TILE_ASPECT,NUM_BANKS}`.
fn modifier_from_tiling_info(family_id: u32, tiling_info: u64) -> Option<u64> {
    if !matches!(family_id, AMDGPU_FAMILY_VI | AMDGPU_FAMILY_CZ) {
        return None;
    }

    let array_mode = field(tiling_info, 0, 0xf);
    if array_mode == 0 {
        return Some(DRM_FORMAT_MOD_LINEAR);
    }
    // 1D_TILED_THIN1 (2) and 2D_TILED_THIN1 (4) are the only legacy modes
    // Mesa exposes as modifiers.
    if !matches!(array_mode, 2 | 4) {
        return None;
    }

    let pipe_config = field(tiling_info, 4, 0x1f);
    let tile_split = field(tiling_info, 9, 0x7);
    let micro_tile_mode = field(tiling_info, 12, 0x7);
    let bank_width = field(tiling_info, 15, 0x3);
    let bank_height = field(tiling_info, 17, 0x3);
    let macro_tile_aspect = field(tiling_info, 19, 0x3);
    let num_banks = field(tiling_info, 21, 0x3);

    // Display (0) and thin (1) microtiling are the only exposed modes.
    if micro_tile_mode > 1 || tile_split > 6 {
        return None;
    }

    // Matches Mesa's `gfx6_calc_modifier`: for GFX7/8 the legacy macrotile
    // fields are the same numbers stored in `AMDGPU_TILING_{BANK_*,
    // MACRO_TILE_ASPECT,NUM_BANKS}`, and `TILE_SPLIT` is the log2 of the
    // split size in 64-byte units.
    let modifier = AMD_FMT_MOD
        | u64::from(array_mode) << 8
        | u64::from(micro_tile_mode) << 14
        | u64::from(pipe_config) << 17
        | u64::from(tile_split) << 22
        | u64::from(bank_width) << 25
        | u64::from(bank_height) << 27
        | u64::from(macro_tile_aspect) << 29
        | u64::from(num_banks) << 31;
    Some(modifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rx580_two_d_tiled_buffer_resolves_to_legacy_modifier() {
        // Captured from a GPU-composited client's 540x600 DRI3 buffer on
        // an RX 580 (Polaris): ARRAY_MODE=4, PIPE_CONFIG=12,
        // TILE_SPLIT=3, MICRO_TILE_MODE=0, BANK_WIDTH=0, BANK_HEIGHT=2,
        // MACRO_TILE_ASPECT=2, NUM_BANKS=3.
        assert_eq!(
            modifier_from_tiling_info(AMDGPU_FAMILY_VI, 0x0074_06c4),
            Some(0x0200_0001_d0d8_0400)
        );
    }

    #[test]
    fn linear_tiling_stays_linear() {
        assert_eq!(
            modifier_from_tiling_info(AMDGPU_FAMILY_VI, 0),
            Some(DRM_FORMAT_MOD_LINEAR)
        );
    }

    #[test]
    fn other_families_are_left_to_the_unknown_layout_path() {
        assert_eq!(modifier_from_tiling_info(141, 0x0074_06c4), None);
    }
}
