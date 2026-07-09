use std::{io, mem, ptr::NonNull, rc::Rc};

use drm::{
    buffer::{Buffer as _, DrmFourcc},
    control::{Device as ControlDevice, dumbbuffer::DumbBuffer, framebuffer},
};

use crate::drm::Device;

pub struct Buffer {
    device: Rc<Device>,
    dumb: DumbBuffer,
    fb_id: framebuffer::Handle,
    ptr: NonNull<u8>,
    len: usize,
    width: u16,
    height: u16,
    stride: u32,
    /// When `true`, `Drop` early-returns: no destroy_framebuffer,
    /// no munmap, no destroy_dumb_buffer. Resources leak until
    /// process-exit DRM-fd close reaps the kernel-side state.
    /// Set by `disarm()` from the shutdown path when atomic
    /// `disable_output` failed for this Buffer's CRTC — KMS may
    /// still hold the FB, so user-side teardown would corrupt
    /// kernel state.
    ///
    /// **ONLY safe to use at final process exit.** This Drop
    /// short-circuit bypasses fb/dumb cleanup but does NOT prevent
    /// Rust from dropping other fields (like the `Rc<Device>`).
    /// Using disarm at runtime (hotplug, modeset recovery) could
    /// produce a zombie handle when the Device's refcount expires.
    disarmed: bool,
}

// Intentionally `!Send`: the `Rc<Device>` and every clone of it stay on
// the single core/backend thread.

impl Buffer {
    pub fn new(device: Rc<Device>, width: u16, height: u16) -> io::Result<Self> {
        let mut dumb = device.create_dumb_buffer(
            (u32::from(width), u32::from(height)),
            DrmFourcc::Xrgb8888,
            32,
        )?;
        let fb_id = match device.add_framebuffer(&dumb, 24, 32) {
            Ok(fb) => fb,
            Err(err) => {
                let _ = device.destroy_dumb_buffer(dumb);
                return Err(err);
            }
        };
        let mapped = device.map_dumb_buffer(&mut dumb).map(|m| {
            let len = m.len();
            let ptr = NonNull::new(m.as_ptr() as *mut u8).expect("non-null mmap");
            mem::forget(m);
            (ptr, len)
        });
        let (ptr, len) = match mapped {
            Ok(pair) => pair,
            Err(err) => {
                let _ = device.destroy_framebuffer(fb_id);
                let _ = device.destroy_dumb_buffer(dumb);
                return Err(err);
            }
        };

        let stride = dumb.pitch();
        Ok(Self {
            device,
            dumb,
            fb_id,
            ptr,
            len,
            width,
            height,
            stride,
            disarmed: false,
        })
    }

    pub fn fb_id(&self) -> framebuffer::Handle {
        self.fb_id
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        let words = self.len / 4;
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().cast::<u32>(), words) }
    }

    pub fn fill(&mut self, pixel: u32) {
        for word in self.pixels_mut() {
            *word = pixel;
        }
    }

    /// Mark this buffer as "let process-exit clean up." Subsequent
    /// `Drop` is a no-op (no destroy_framebuffer / munmap /
    /// destroy_dumb_buffer). Idempotent. **Only valid at final
    /// process exit** — see field doc.
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if self.disarmed {
            log::warn!(
                "drm Buffer disarmed (atomic disable_output failed); \
                 leaking FB/dumb to be reaped by DRM-fd close"
            );
            return;
        }
        if let Err(err) = self.device.destroy_framebuffer(self.fb_id) {
            log::warn!(
                "destroy_framebuffer 0x{:x} failed: {err}",
                u32::from(self.fb_id)
            );
        }
        unsafe {
            if libc::munmap(self.ptr.as_ptr().cast(), self.len) != 0 {
                log::warn!("munmap dumb buffer failed: {}", io::Error::last_os_error());
            }
        }
        if let Err(err) = self.device.destroy_dumb_buffer(self.dumb) {
            log::warn!("destroy_dumb_buffer failed: {err}");
        }
    }
}
