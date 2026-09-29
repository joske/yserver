//! The pixels under a software cursor, saved by the compose that drew it.
//!
//! Xorg never returns the cursor from a read: `mi/misprite.c` takes a software
//! sprite off the framebuffer before any window source read that overlaps it
//! (`miSpriteSourceValidate`, `miSpriteCopyWindow`), so root `GetImage`,
//! `CopyArea` and RENDER sources all see the screen without it. Here the
//! sprite is the last draw of a compose, so that compose copies the rect
//! beneath it into a host buffer first, and root reads of the image put those
//! pixels back. A HW-cursor frame records nothing.

use std::{mem::ManuallyDrop, sync::Arc};

use ash::vk;

use super::{engine::StagingBuffer, platform::FenceTicket};
use crate::kms::vk::device::VkContext;

/// Allocation granularity, so a cursor that grows by a few pixels reuses the
/// buffer. 64 KiB is a 128×128 sprite.
const GRANULE: u64 = 64 * 1024;

/// Compose images a save may belong to: a pool's BOs and an intermediate.
const MAX_SAVES: usize = 8;

/// What a compose needs to save the rect under its cursor draw.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorSaveTarget {
    pub(crate) buffer: vk::Buffer,
    /// Compose-image-local, inside the image; tightly packed BGRA8 in `buffer`.
    pub(crate) rect: vk::Rect2D,
}

struct CursorSave {
    image: vk::Image,
    /// Where the saved pixels sit in `image`; `None` while `image` holds no
    /// software cursor.
    rect: Option<vk::Rect2D>,
    buffer: ManuallyDrop<StagingBuffer>,
    /// The last compose that wrote `buffer`.
    ticket: Option<FenceTicket>,
    /// A larger buffer for a compose in progress; replaces `buffer` only once
    /// that compose reached the GPU.
    replacement: Option<StagingBuffer>,
}

impl Drop for CursorSave {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take()
            && let Err(error) = ticket.wait(self.buffer.vk())
        {
            log::error!("render cursor save: fence wait failed: {error:?}; leaking its buffer");
            return;
        }
        // SAFETY: dropped once, here, with no compose left writing it.
        unsafe { ManuallyDrop::drop(&mut self.buffer) };
    }
}

/// One output's saves, keyed by the compose image. Every compose into an image
/// passes through [`CursorSaves::prepare`], so an entry always describes the
/// image's last compose, even if a handle is reused.
#[derive(Default)]
pub(crate) struct CursorSaves {
    saves: Vec<CursorSave>,
}

impl CursorSaves {
    /// Start a compose into `image` whose software cursor covers `rect`, or
    /// none. Returns where to save, or `None` when there is no cursor or the
    /// buffer cannot be allocated (reads then show the sprite). Nothing a read
    /// sees changes until [`Self::finish`].
    pub(crate) fn prepare(
        &mut self,
        vk: &Arc<VkContext>,
        image: vk::Image,
        rect: Option<vk::Rect2D>,
    ) -> Option<CursorSaveTarget> {
        let index = self.saves.iter().position(|s| s.image == image);
        let rect = rect?;
        let needed = u64::from(rect.extent.width) * u64::from(rect.extent.height) * 4;
        if let Some(i) = index
            && self.saves[i].buffer.size() >= needed
        {
            return Some(CursorSaveTarget {
                buffer: self.saves[i].buffer.buffer(),
                rect,
            });
        }
        let buffer =
            match StagingBuffer::new_for_readback(Arc::clone(vk), needed.next_multiple_of(GRANULE))
            {
                Ok(buffer) => buffer,
                Err(error) => {
                    log::warn!("render cursor save: {needed}-byte buffer: {error:?}");
                    return None;
                }
            };
        let target = buffer.buffer();
        if let Some(i) = index {
            self.saves[i].replacement = Some(buffer);
        } else {
            if self.saves.len() == MAX_SAVES {
                self.saves.remove(0);
            }
            // A new entry has no save yet, which is what its image holds.
            self.saves.push(CursorSave {
                image,
                rect: None,
                buffer: ManuallyDrop::new(buffer),
                ticket: None,
                replacement: None,
            });
        }
        Some(CursorSaveTarget {
            buffer: target,
            rect,
        })
    }

    /// Finish the compose [`Self::prepare`] returned `save` for. `executed` is
    /// its fence when it reached the GPU: the image then holds that compose,
    /// whatever became of the flip after it, so its save (or absence of one)
    /// replaces the old. A compose that never ran leaves the old save.
    pub(crate) fn finish(
        &mut self,
        image: vk::Image,
        save: Option<CursorSaveTarget>,
        executed: Option<&FenceTicket>,
    ) {
        let Some(entry) = self.saves.iter_mut().find(|s| s.image == image) else {
            return;
        };
        let replacement = entry.replacement.take();
        let Some(ticket) = executed else {
            // Unsubmitted: the GPU never touched the replacement.
            drop(replacement);
            return;
        };
        if let Some(buffer) = replacement {
            if let Some(old) = entry.ticket.take()
                && let Err(error) = old.wait(buffer.vk())
            {
                log::error!("render cursor save: fence wait failed: {error:?}; leaking its buffer");
                entry.buffer = ManuallyDrop::new(buffer);
            } else {
                // SAFETY: the compose that last wrote it has completed.
                unsafe { ManuallyDrop::drop(&mut entry.buffer) };
                entry.buffer = ManuallyDrop::new(buffer);
            }
        }
        entry.ticket = Some(ticket.clone());
        entry.rect = save.map(|s| s.rect);
    }

    /// Put the saved pixels back into `bytes`, a tightly packed BGRA8 read of
    /// `read` (image-local) from `image`. The read must be ordered after the
    /// compose that saved them, as a readback on the same queue is.
    pub(crate) fn restore(&self, image: vk::Image, read: vk::Rect2D, bytes: &mut [u8]) {
        let Some(save) = self.saves.iter().find(|s| s.image == image) else {
            return;
        };
        let Some(rect) = save.rect else {
            return;
        };
        if let Err(error) = save.buffer.invalidate_for_read() {
            log::warn!("render cursor save: invalidate: {error:?}");
            return;
        }
        let len = usize::try_from(u64::from(rect.extent.width) * u64::from(rect.extent.height) * 4)
            .unwrap_or(0);
        // SAFETY: mapped for at least `len` (`prepare`), and the compose that
        // wrote it has completed before the caller's readback.
        let saved = unsafe { std::slice::from_raw_parts(save.buffer.mapped().as_ptr(), len) };
        restore_rect(bytes, read, rect, saved);
    }
}

/// Copy `saved` (tight BGRA8 of `rect`) over the part of `bytes` (tight BGRA8
/// of `read`) the two share.
pub(crate) fn restore_rect(bytes: &mut [u8], read: vk::Rect2D, rect: vk::Rect2D, saved: &[u8]) {
    let right = |r: vk::Rect2D| i64::from(r.offset.x) + i64::from(r.extent.width);
    let bottom = |r: vk::Rect2D| i64::from(r.offset.y) + i64::from(r.extent.height);
    let x0 = i64::from(read.offset.x.max(rect.offset.x));
    let y0 = i64::from(read.offset.y.max(rect.offset.y));
    let x1 = right(read).min(right(rect));
    let y1 = bottom(read).min(bottom(rect));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let to_usize = |v: i64| usize::try_from(v).unwrap_or(0);
    let row = to_usize(x1 - x0) * 4;
    for y in y0..y1 {
        let dst = to_usize(
            ((y - i64::from(read.offset.y)) * i64::from(read.extent.width) + x0
                - i64::from(read.offset.x))
                * 4,
        );
        let src = to_usize(
            ((y - i64::from(rect.offset.y)) * i64::from(rect.extent.width) + x0
                - i64::from(rect.offset.x))
                * 4,
        );
        if let (Some(d), Some(s)) = (bytes.get_mut(dst..dst + row), saved.get(src..src + row)) {
            d.copy_from_slice(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle;

    use super::*;

    fn r(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    #[test]
    fn restore_rect_patches_only_the_overlap() {
        // Read 4×3 at (10, 20); saved 3×2 at (12, 21), whose third column
        // (0xee) hangs off the read's right edge.
        let mut bytes = vec![0u8; 4 * 3 * 4];
        let mut saved = Vec::new();
        for row in [1u8, 9] {
            saved.extend((row..row + 8).chain([0xee; 4]));
        }
        restore_rect(&mut bytes, r(10, 20, 4, 3), r(12, 21, 3, 2), &saved);
        let px = |x: usize, y: usize| bytes[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
        assert_eq!(px(2, 1), vec![1, 2, 3, 4]);
        assert_eq!(px(3, 1), vec![5, 6, 7, 8]);
        assert_eq!(px(2, 2), vec![9, 10, 11, 12]);
        assert_eq!(px(3, 2), vec![13, 14, 15, 16]);
        for (x, y) in [(0, 0), (1, 1), (2, 0), (3, 0), (0, 2), (1, 2)] {
            assert_eq!(px(x, y), vec![0; 4], "({x}, {y}) is outside the save");
        }
    }

    /// A fence ticket whose (empty) submit has already been queued, as a
    /// compose that reached the GPU leaves it.
    fn executed(vk: &Arc<VkContext>, pool: &super::super::platform::FencePool) -> FenceTicket {
        let ticket = pool.acquire().expect("fence");
        unsafe {
            vk.device
                .queue_submit2(vk.graphics_queue, &[], ticket.fence())
                .expect("submit");
        }
        ticket
    }

    /// Stand-in for the compose's copy: the GPU writes `fill` into the save.
    fn gpu_writes(target: CursorSaveTarget, saves: &CursorSaves, fill: u8) {
        let entry = saves.saves.iter().find(|s| {
            s.buffer.buffer() == target.buffer
                || s.replacement
                    .as_ref()
                    .is_some_and(|b| b.buffer() == target.buffer)
        });
        let entry = entry.expect("target belongs to an entry");
        let buffer = entry
            .replacement
            .as_ref()
            .filter(|b| b.buffer() == target.buffer)
            .unwrap_or(&entry.buffer);
        let len = (target.rect.extent.width * target.rect.extent.height * 4) as usize;
        unsafe { std::ptr::write_bytes(buffer.mapped().as_ptr(), fill, len) };
    }

    fn read(saves: &CursorSaves, image: vk::Image) -> Vec<u8> {
        let mut bytes = vec![0u8; 16 * 16 * 4];
        saves.restore(image, r(0, 0, 16, 16), &mut bytes);
        bytes
    }

    /// The save follows what the image holds: a compose that reached the GPU
    /// replaces it (its flip's fate does not enter), one that never ran keeps
    /// the old one, including across a buffer reallocation.
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn a_save_changes_only_when_its_compose_reached_the_gpu() {
        let vk = VkContext::new().expect("vk");
        let pool = super::super::platform::FencePool::new(Arc::clone(&vk));
        let image = vk::Image::from_raw(0x1234);
        let mut saves = CursorSaves::default();
        let px = |bytes: &[u8], x: usize, y: usize| bytes[(y * 16 + x) * 4];

        // Frame 1 draws the cursor at (2, 2) and runs.
        let t = saves.prepare(&vk, image, Some(r(2, 2, 4, 4))).unwrap();
        gpu_writes(t, &saves, 0x11);
        saves.finish(image, Some(t), Some(&executed(&vk, &pool)));
        assert_eq!(px(&read(&saves, image), 3, 3), 0x11);

        // Frame 2 moves it to (8, 8) and runs, but its flip fails: the image
        // holds frame 2, so the read must restore frame 2's rect.
        let t = saves.prepare(&vk, image, Some(r(8, 8, 4, 4))).unwrap();
        gpu_writes(t, &saves, 0x22);
        saves.finish(image, Some(t), Some(&executed(&vk, &pool)));
        let got = read(&saves, image);
        assert_eq!((px(&got, 9, 9), px(&got, 3, 3)), (0x22, 0));

        // Frame 3 never reaches the GPU, with a sprite too large for the
        // buffer: frame 2's save stands.
        let t = saves.prepare(&vk, image, Some(r(0, 0, 200, 200))).unwrap();
        saves.finish(image, Some(t), None);
        let got = read(&saves, image);
        assert_eq!((px(&got, 9, 9), px(&got, 0, 0)), (0x22, 0));

        // Frame 4 runs cursorless: nothing to restore any more.
        assert!(saves.prepare(&vk, image, None).is_none());
        saves.finish(image, None, Some(&executed(&vk, &pool)));
        assert_eq!(px(&read(&saves, image), 9, 9), 0);
    }

    #[test]
    fn restore_rect_ignores_a_disjoint_save() {
        let mut bytes = vec![7u8; 16];
        restore_rect(&mut bytes, r(0, 0, 2, 2), r(2, 0, 2, 2), &[0; 16]);
        assert_eq!(bytes, vec![7u8; 16]);
    }
}
