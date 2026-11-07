//! DRI3 buffers the renderer cannot import as they lie, mapped instead.
//!
//! A renderer without `VK_EXT_image_drm_format_modifier` (lavapipe) imports
//! a LINEAR buffer as a `VK_IMAGE_TILING_LINEAR` image whose row pitch and
//! padding it chooses itself. A buffer laid out another way would be read
//! sheared, and one the renderer would read past the end of fails to
//! import: lavapipe pads a LINEAR image's height (974 rows ask for 976), so
//! the import of a client's 1920x974 swapchain image failed, its
//! `PixmapFromBuffers` got BadAlloc, and Mesa's swapchain creation failed.
//!
//! Such a buffer is mapped read-only, and its pixmap is an ordinary one,
//! filled from the mapping when a copy or `GetImage` reads it
//! ([`KmsBackend::refresh_mapped_pixmap`]). The fill reads the mapping
//! between `DMA_BUF_IOCTL_SYNC` start and end, and only once the client's
//! writes to the buffer are done. It never waits for them: a Present waits
//! from the loop first (`arm_present_source_wait`), and any other read of a
//! buffer still being written sees what the pixmap last held.

use std::{
    io,
    os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd},
};

use ash::vk;
use yserver_core::backend::{Backend as _, PixmapHandle};

use crate::kms::render::{backend::KmsBackend, target::Dst};

/// A client's LINEAR DRI3 buffer, mapped read-only.
pub(crate) struct Dri3Mapped {
    pub(in crate::kms::render::backend) fd: OwnedFd,
    map: *mut libc::c_void,
    len: usize,
    layout: MappedLayout,
    depth: u8,
}

/// Where a mapped buffer's pixels lie: `height` rows of `width` pixels at
/// 32 bits, `stride` bytes apart, from `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::kms::render::backend) struct MappedLayout {
    pub offset: usize,
    pub stride: usize,
    pub width: u16,
    pub height: u16,
}

impl MappedLayout {
    /// The bytes the layout covers, from the buffer's start: refused when
    /// the client's buffer is shorter, or its length unknown, so that no
    /// read goes past its end (a short dma-buf's mapping faults there).
    pub(in crate::kms::render::backend) fn checked_len(
        self,
        buffer_len: Option<u64>,
    ) -> io::Result<usize> {
        let row = usize::from(self.width) * 4;
        if self.stride < row {
            return Err(io::Error::other(format!(
                "DRI3 import: stride {} is below a row of {} pixels",
                self.stride, self.width
            )));
        }
        let len = match self.height {
            0 => self.offset,
            rows => self
                .stride
                .checked_mul(usize::from(rows) - 1)
                .and_then(|before_last| before_last.checked_add(row))
                .and_then(|pixels| pixels.checked_add(self.offset))
                .ok_or_else(|| io::Error::other("DRI3 import: the layout overflows"))?,
        };
        match buffer_len {
            Some(size) if size >= len as u64 => Ok(len),
            Some(size) => Err(io::Error::other(format!(
                "DRI3 import: a buffer of {size} bytes for a layout of {len}"
            ))),
            None => Err(io::Error::other(
                "DRI3 import: the buffer's size is unknown, so it is not mapped",
            )),
        }
    }
}

impl Dri3Mapped {
    /// The image's rows as they lie in the buffer.
    fn rows(&self) -> &[u8] {
        // SAFETY: `map` is a live read-only mapping of `len` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(self.map.cast::<u8>(), self.len) };
        &bytes[self.layout.offset..]
    }

    /// The image, its rows packed to `width` pixels: an X `ZPixmap` at 32
    /// bits per pixel.
    fn packed(&self) -> Vec<u8> {
        let row = usize::from(self.layout.width) * 4;
        let rows = self.rows();
        let mut out = Vec::with_capacity(row * usize::from(self.layout.height));
        for y in 0..usize::from(self.layout.height) {
            let start = y * self.layout.stride;
            out.extend_from_slice(&rows[start..start + row]);
        }
        out
    }
}

// SAFETY: the mapping is read-only memory the value owns; nothing about it
// is tied to a thread.
unsafe impl Send for Dri3Mapped {}

impl Drop for Dri3Mapped {
    fn drop(&mut self) {
        // SAFETY: the mapping this value made, which nothing else holds.
        unsafe {
            libc::munmap(self.map, self.len);
        }
    }
}

/// Whether the client's writes to `fd` are done, asked without waiting: a
/// dma-buf polls readable once its write fences have signalled.
pub(in crate::kms::render::backend) fn writes_done(fd: BorrowedFd<'_>) -> bool {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, no timeout.
    let ready = unsafe { libc::poll(&raw mut pfd, 1, 0) };
    ready > 0 && pfd.revents & libc::POLLIN != 0
}

// `struct dma_buf_sync { __u64 flags; }`, `_IOW('b', 0, ...)`. `libc::Ioctl`
// is `c_int` on musl; the cast keeps the bit pattern.
#[cfg(target_os = "linux")]
const DMA_BUF_IOCTL_SYNC: libc::Ioctl = 0x4008_6200_u32 as libc::Ioctl;
#[cfg(not(target_os = "linux"))]
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// A CPU read of a dma-buf, between `DMA_BUF_IOCTL_SYNC` start and end.
struct CpuRead<'a>(BorrowedFd<'a>);

impl<'a> CpuRead<'a> {
    fn begin(fd: BorrowedFd<'a>) -> Self {
        sync(fd, DMA_BUF_SYNC_READ);
        Self(fd)
    }
}

impl Drop for CpuRead<'_> {
    fn drop(&mut self) {
        sync(self.0, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END);
    }
}

/// One `DMA_BUF_IOCTL_SYNC`. A descriptor that is no dma-buf refuses it,
/// which changes nothing for a read of plain memory.
fn sync(fd: BorrowedFd<'_>, flags: u64) {
    let mut arg = flags;
    // SAFETY: the ioctl takes a pointer to one u64 on a live descriptor.
    let _ = unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &raw mut arg) };
}

impl KmsBackend {
    /// Whether the renderer's own LINEAR layout of a `width` x `height`
    /// image in `format` is `offset` and `pitch`, and the memory it asks for
    /// that image fits in the client's buffer of `buffer_len` bytes: what a
    /// `VK_IMAGE_TILING_LINEAR` import needs to read the buffer as it lies.
    pub(in crate::kms::render::backend) fn linear_layout_is(
        vk: &crate::kms::vk::device::VkContext,
        format: vk::Format,
        (width, height): (u16, u16),
        offset: u32,
        pitch: u32,
        buffer_len: Option<u64>,
    ) -> bool {
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .push_next(&mut external)
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: u32::from(width),
                height: u32::from(height),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(crate::kms::vk::dri3::CLIENT_IMPORT_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: a valid create-info; the image is destroyed below and
        // never bound or used.
        let Ok(image) = (unsafe { vk.device.create_image(&info, None) }) else {
            return false;
        };
        // SAFETY: `image` is a live LINEAR image of this device.
        let layout = unsafe {
            vk.device.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    array_layer: 0,
                },
            )
        };
        // SAFETY: as above.
        let needs = unsafe { vk.device.get_image_memory_requirements(image) }.size;
        // SAFETY: as above; nothing else holds it.
        unsafe { vk.device.destroy_image(image, None) };
        layout.offset == u64::from(offset)
            && layout.row_pitch == u64::from(pitch)
            && linear_import_fits(needs, offset, buffer_len)
    }

    /// The length of the file `fd` names: a dma-buf's is its buffer's size.
    /// `None` when it cannot be read or is zero.
    pub(in crate::kms::render::backend) fn fd_len(fd: &OwnedFd) -> Option<u64> {
        // SAFETY: fstat into a zeroed stat on a descriptor we hold.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        (unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } == 0)
            .then(|| u64::try_from(stat.st_size).ok())
            .flatten()
            .filter(|&len| len > 0)
    }

    /// A DRI3 buffer mapped, with an ordinary pixmap filled from it
    /// ([`Dri3Mapped`]).
    pub(in crate::kms::render::backend) fn dri3_import_mapped(
        &mut self,
        fd: OwnedFd,
        (width, height): (u16, u16),
        stride: u32,
        offset: u32,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        let layout = MappedLayout {
            offset: offset as usize,
            stride: stride as usize,
            width,
            height,
        };
        let buffer_len = Self::fd_len(&fd);
        let needed = layout.checked_len(buffer_len)?;
        // The whole last stride too where the buffer has it, so that the
        // fill can read rows in place ([`Self::refresh_mapped_pixmap`]).
        let len = buffer_len
            .and_then(|size| usize::try_from(size).ok())
            .map_or(needed, |size| {
                size.min(layout.offset + layout.stride * usize::from(height))
            })
            .max(needed);
        // SAFETY: a shared read-only mapping of `len` bytes of the client's
        // buffer, which is at least that long; `Dri3Mapped` unmaps it.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::other(format!(
                "DRI3 import: mapping the buffer: {}",
                io::Error::last_os_error()
            )));
        }
        let mapped = Dri3Mapped {
            fd,
            map,
            len,
            layout,
            depth,
        };
        let handle = self.create_pixmap(None, depth, width, height)?;
        log::info!(
            "DRI3: pixmap 0x{:x} ({width}x{height}, stride {stride}) is mapped and copied as it \
             is read: the renderer imports LINEAR only at its own pitch and size",
            handle.as_raw()
        );
        let _ = self.pstate.dri3_mapped.insert(handle.as_raw(), mapped);
        self.refresh_mapped_pixmap(handle.as_raw());
        Ok(handle)
    }

    /// Fill a mapped DRI3 buffer's pixmap from the buffer, if the client's
    /// writes to it are done. Nothing waits here: a Present waited for them
    /// from the loop (`arm_present_source_wait`), and a read of a buffer
    /// still being written leaves the pixmap as it last was.
    pub(in crate::kms::render::backend) fn refresh_mapped_pixmap(&mut self, host_xid: u32) {
        let Some(mapped) = self.pstate.dri3_mapped.get(&host_xid) else {
            return;
        };
        if !writes_done(mapped.fd.as_fd()) {
            log::debug!("DRI3: mapped pixmap 0x{host_xid:x} is still being written; not refilled");
            return;
        }
        let Some(id) = self.store.lookup(host_xid) else {
            return;
        };
        let _read = CpuRead::begin(mapped.fd.as_fd());
        // Straight from the mapping at the client's stride: the upload's
        // source rows are `stride / 4` pixels apart and the copy is clamped
        // to the pixmap. A stride that is no whole number of pixels, or a
        // buffer that ends before its last row's padding, is packed first.
        let packed;
        let layout = mapped.layout;
        let in_place = layout.stride * usize::from(layout.height);
        let (bytes, row_pixels) = match u16::try_from(layout.stride / 4) {
            Ok(row_pixels) if layout.stride % 4 == 0 && mapped.rows().len() >= in_place => {
                (&mapped.rows()[..in_place], row_pixels)
            }
            _ => {
                packed = mapped.packed();
                (packed.as_slice(), layout.width)
            }
        };
        let extent = vk::Extent2D {
            width: u32::from(row_pixels),
            height: u32::from(layout.height),
        };
        if let Err(err) = self.engine.put_image(
            &mut self.store,
            &mut self.platform.gpu,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            extent,
            bytes,
            mapped.depth,
        ) {
            log::warn!("DRI3: filling mapped pixmap 0x{host_xid:x}: {err:?}");
        }
    }
}

/// Whether memory of `needs` bytes, bound at `offset`, lies inside a
/// client's buffer of `buffer_len` bytes ([`KmsBackend::linear_layout_is`]).
/// A buffer whose length is unknown is taken to fit, as before.
pub(in crate::kms::render::backend) fn linear_import_fits(
    needs: u64,
    offset: u32,
    buffer_len: Option<u64>,
) -> bool {
    buffer_len.is_none_or(|len| u64::from(offset).saturating_add(needs) <= len)
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, os::unix::net::UnixStream};

    use super::*;

    /// A LINEAR import binds the renderer's whole memory requirement in the
    /// client's buffer: a requirement past the buffer's end (a 1920x974
    /// image on lavapipe, 976 rows) sends the buffer down the mapped path.
    #[test]
    fn a_linear_import_must_fit_in_the_clients_buffer() {
        let anv = Some(7_483_392); // 7680 x 974, rounded to a page
        assert!(linear_import_fits(7_480_320, 0, anv));
        assert!(!linear_import_fits(7_680 * 976, 0, anv));
        assert!(!linear_import_fits(7_480_320, 4_096, anv));
    }

    /// A buffer shorter than its layout, or of unknown size, is not mapped:
    /// the mapping would fault past its end.
    #[test]
    fn a_mapped_layout_must_fit_in_a_buffer_of_known_size() {
        let layout = MappedLayout {
            offset: 64,
            stride: 7_680,
            width: 1_900,
            height: 974,
        };
        // The last row ends at its last pixel, not at the stride.
        let len = 64 + 7_680 * 973 + 1_900 * 4;
        assert_eq!(layout.checked_len(Some(len as u64)).unwrap(), len);
        assert!(layout.checked_len(Some(len as u64 - 1)).is_err());
        assert!(layout.checked_len(None).is_err());
        let narrow = MappedLayout {
            stride: 1_900 * 4 - 4,
            ..layout
        };
        assert!(narrow.checked_len(Some(u64::MAX)).is_err());
    }

    /// A buffer is read only once it reads as ready, and asking never
    /// waits: a socket with nothing in it stands for a buffer still being
    /// written.
    #[test]
    fn a_buffer_still_being_written_is_not_read() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        assert!(!writes_done(reader.as_fd()));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        writer.write_all(&[1]).unwrap();
        assert!(writes_done(reader.as_fd()));
    }
}
