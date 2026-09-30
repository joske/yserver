//! Page-flip submission + completion drain.
//!
//! `submit_flip` atomic-commits a new FB_ID on the primary plane with
//! PAGE_FLIP_EVENT | NONBLOCK; the kernel produces a completion event
//! on the DRM fd when scanout latches the new buffer.
//!
//! `drain_events` reads pending events with `Device::receive_events()`
//! and dispatches PageFlip completions to a closure. The drm crate's
//! parser folds the kernel `user_data` field into `crtc` (preferring
//! `crtc_id` from the vblank event when present, else falling back to
//! `user_data`). The closure receives the per-CRTC handle so multi-output
//! callsites can route the completion to the right swapchain.

use std::io;

use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, Event, atomic::AtomicModeReq, crtc, framebuffer,
};

use crate::drm::{
    Device,
    modeset::{Output, PropMap},
};

#[cfg(target_os = "linux")]
type IoctlReq = libc::Ioctl;
#[cfg(not(target_os = "linux"))]
type IoctlReq = libc::c_ulong;

// ── DRM_IOCTL_CRTC_QUEUE_SEQUENCE plumbing ──────────────────────
//
// `drm` 0.15 / `drm-ffi` 0.9 do not wrap this ioctl; we issue it
// raw. Layouts mirror `<drm/drm.h>` exactly (kernel headers, verified
// against /usr/include/drm/drm.h on the build host). All multi-byte
// fields are little-endian on every supported target.
//
// Both flags are passed in the `flags` field; combined or'd.
pub(crate) const DRM_CRTC_SEQUENCE_RELATIVE: u32 = 0x0000_0001;
pub(crate) const DRM_CRTC_SEQUENCE_NEXT_ON_MISS: u32 = 0x0000_0002;

/// kernel `DRM_EVENT_CRTC_SEQUENCE` event type id.
pub(crate) const DRM_EVENT_CRTC_SEQUENCE: u32 = 0x03;

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct drm_crtc_queue_sequence {
    pub crtc_id: u32,
    pub flags: u32,
    /// In: target sequence. Out: actual scheduled sequence.
    pub sequence: u64,
    /// Echoed back verbatim in the resulting `drm_event_crtc_sequence`.
    pub user_data: u64,
}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct drm_event_header {
    pub r#type: u32,
    pub length: u32,
}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct drm_event_crtc_sequence {
    pub base: drm_event_header,
    pub user_data: u64,
    /// CLOCK_MONOTONIC nanoseconds. Signed per kernel header — we
    /// must `u64::try_from` rather than `as u64`.
    pub time_ns: i64,
    pub sequence: u64,
}

// `_IOWR('d', 0x3C, drm_crtc_queue_sequence)` expanded inline so
// the request code is a `const` we can also assert in a unit test.
//   dir = 3 (RW), type = 'd' (0x64), nr = 0x3C, size = 24
//
// Linux libc exposes the request type as `libc::Ioctl` (`c_ulong` on
// glibc, `c_int` on musl/Android; issues #15, #74). FreeBSD's libc does
// not export that alias even though `ioctl` still takes an unsigned-long
// request there, so use a tiny local alias to keep the callsite typed
// correctly on both platforms. The high bit is set (dir = RW), so the
// value overflows a signed 32-bit `Ioctl`; compute the bit pattern in
// `u32` and reinterpret-cast to preserve it.
pub(crate) const DRM_IOCTL_CRTC_QUEUE_SEQUENCE: IoctlReq = ((3u32 << 30)
    | ((std::mem::size_of::<drm_crtc_queue_sequence>() as u32) << 16)
    | (0x64u32 << 8)
    | 0x3C) as IoctlReq;

/// Queue a one-shot CRTC vblank sequence event. `crtc_id` is the
/// **raw KMS object id** (NOT a pipe index — that distinction is
/// the whole reason this helper exists; the legacy `drmWaitVBlank`
/// path used pipe indices and lost the dual-monitor case).
///
/// - `relative = true`  → kernel arms `current_msc + sequence`
///   vblanks from now; pass `sequence = 1` for "next vblank".
/// - `relative = false` → absolute target. **Always pair with
///   `NEXT_ON_MISS`** (set internally) so an already-passed target
///   fires at the next vblank instead of waiting a full 32-bit
///   counter wrap.
///
/// `user_data` is echoed verbatim in the resulting
/// `DRM_EVENT_CRTC_SEQUENCE` — we encode the stable `crtc_id` there.
///
/// Returns the kernel-assigned scheduled sequence on success.
///
/// # Errors
///
/// - `EOPNOTSUPP` on pre-4.14 kernels — caller should fall back
///   to flip-driven MSC only (idle arming disabled).
/// - `EACCES` if we no longer hold DRM master — caller must have
///   pre-gated on `scanout_allowed()`.
pub(crate) fn queue_crtc_sequence(
    device: &Device,
    crtc_id: u32,
    relative: bool,
    sequence: u64,
    user_data: u64,
) -> io::Result<u64> {
    use std::os::{fd::AsFd, unix::io::AsRawFd};

    let mut flags = DRM_CRTC_SEQUENCE_NEXT_ON_MISS;
    if relative {
        flags |= DRM_CRTC_SEQUENCE_RELATIVE;
    }
    let mut req = drm_crtc_queue_sequence {
        crtc_id,
        flags,
        sequence,
        user_data,
    };
    // SAFETY: `req` is a fully-initialised POD of the exact size the
    // kernel expects (24 bytes — pinned by the unit tests below). The
    // device fd is held alive by `device` for the duration of the
    // call; the kernel reads and writes `req` in place.
    let raw_fd = device.as_fd().as_raw_fd();
    let rc = unsafe { libc::ioctl(raw_fd, DRM_IOCTL_CRTC_QUEUE_SEQUENCE, &mut req as *mut _) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(req.sequence)
}

pub fn submit_flip(device: &Device, output: &Output, fb_id: framebuffer::Handle) -> io::Result<()> {
    submit_flip_inner(device, output, fb_id, None, None)
}

/// Atomic commit + explicit-fence flip (Phase 4.1.2.5). Used by the
/// Vulkan-fed scanout path: pass the SYNC_FD payload exported from
/// the bo's signalSemaphore as `in_fence_fd` so KMS waits for GPU
/// before scanning out, and pass `out_fence_holder` so the kernel
/// allocates a release fence we can wait on for retire.
///
/// The kernel takes ownership of `in_fence_fd` on a successful
/// commit (rc=0). On `-EBUSY` (or any other error) the caller still
/// owns the fd and must close it. `out_fence_holder` is written with
/// the new fence fd that the caller owns.
pub fn submit_flip_with_fences(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
    in_fence_fd: i32,
    out_fence_holder: &mut i32,
) -> io::Result<()> {
    submit_flip_inner(
        device,
        output,
        fb_id,
        Some(in_fence_fd),
        Some(out_fence_holder),
    )
}

fn submit_flip_inner(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
    in_fence_fd: Option<i32>,
    out_fence_holder: Option<&mut i32>,
) -> io::Result<()> {
    let mut req = AtomicModeReq::new();
    req.add_raw_property(
        output.plane.into(),
        output.plane_fb_id_prop,
        u64::from(u32::from(fb_id)),
    );
    req.add_raw_property(
        output.plane.into(),
        output.plane_crtc_id_prop,
        u64::from(u32::from(output.crtc)),
    );

    if let Some(fd) = in_fence_fd {
        // IN_FENCE_FD is a plane property. Its value is the fence fd
        // (sign-extended to u64; -1 means "no fence", which differs
        // from "absent").
        let prop = match output.plane_in_fence_fd_prop {
            Some(prop) => prop,
            None => PropMap::for_object(device, output.plane)?.id("IN_FENCE_FD")?,
        };
        req.add_raw_property(output.plane.into(), prop, fd as i64 as u64);
    }
    if let Some(holder) = out_fence_holder {
        // OUT_FENCE_PTR is a CRTC property. Its value is a userspace
        // pointer (cast to u64) where the kernel writes the freshly
        // allocated fence fd on a successful commit.
        let prop = match output.crtc_out_fence_ptr_prop {
            Some(prop) => prop,
            None => PropMap::for_object(device, output.crtc)?.id("OUT_FENCE_PTR")?,
        };
        let ptr_value = (holder as *mut i32) as usize as u64;
        req.add_raw_property(output.crtc.into(), prop, ptr_value);
    }

    device.atomic_commit(
        AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
        req,
    )
}

/// Returns without reading when no event is queued: the DRM fd is blocking,
/// and readiness the core loop polled can already have been consumed by
/// `discard_old_drm_events_after_all_off` earlier in the same iteration.
pub fn drain_events<A, S>(device: &Device, mut on_advance: A, mut on_sequence: S) -> io::Result<()>
where
    A: FnMut(crtc::Handle, u32, std::time::Duration),
    S: FnMut(u64, i64, u64),
{
    if !event_pending(device)? {
        return Ok(());
    }
    for event in device.receive_events()? {
        dispatch_event(event, &mut on_advance, &mut on_sequence);
    }
    Ok(())
}

/// Zero-timeout poll of the DRM fd. Sound as a guard for the blocking read
/// because this process is the fd's only event reader.
fn event_pending(device: &Device) -> io::Result<bool> {
    use std::os::{fd::AsFd, unix::io::AsRawFd};

    let mut poll_fd = libc::pollfd {
        fd: device.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one initialized pollfd, live for the call; the fd is held
        // open by `device`.
        let ready = unsafe { libc::poll(&raw mut poll_fd, 1, 0) };
        if ready >= 0 {
            return Ok(poll_fd.revents & libc::POLLIN != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Dispatch a single drm event.
///
/// - `Event::PageFlip` → `on_advance(crtc, frame /*msc*/, duration /*ust*/)`.
///   The MSC/UST are forwarded for Present vblank pacing — a compositor
///   (picom) drives its frame clock off `PresentNotifyMSC`, which must
///   complete with the real kernel `(msc, ust)` at each pageflip.
/// - `Event::Unknown` matching `DRM_EVENT_CRTC_SEQUENCE` (type==3,
///   length==32) → `on_sequence(user_data_u64, time_ns_i64,
///   sequence_u64)`. **Raw**: `time_ns` is signed and not yet
///   validated; `user_data` is echoed verbatim from the arm call — the
///   backend encodes the crtc_id in the low 32 bits and (for absolute
///   per-target arms) a discriminating tag in the high bit, so the full
///   width must reach the caller undivided. The caller does clear-arm
///   BEFORE any drop on validity check.
/// - Everything else (`Vblank`, other `Unknown`): dropped.
///
/// Factored out of [`drain_events`] so the per-event routing is unit-testable
/// without a real DRM fd (synthetic event values can be constructed via the
/// public `PageFlipEvent` fields / raw byte buffers).
fn dispatch_event<A, S>(event: Event, on_advance: &mut A, on_sequence: &mut S)
where
    A: FnMut(crtc::Handle, u32, std::time::Duration),
    S: FnMut(u64, i64, u64),
{
    match event {
        Event::PageFlip(ev) => {
            on_advance(ev.crtc, ev.frame, ev.duration);
        }
        Event::Unknown(bytes) => {
            // Header: u32 type, u32 length (8 bytes total).
            if bytes.len() < std::mem::size_of::<drm_event_header>() {
                return;
            }
            // SAFETY: length-checked read of a POD header from a kernel
            // event buffer; `read_unaligned` tolerates the Vec's alignment.
            let header: drm_event_header =
                unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<drm_event_header>()) };
            if header.r#type != DRM_EVENT_CRTC_SEQUENCE {
                return;
            }
            if header.length as usize != std::mem::size_of::<drm_event_crtc_sequence>() {
                return;
            }
            if bytes.len() < std::mem::size_of::<drm_event_crtc_sequence>() {
                return;
            }
            // SAFETY: type+length validated above; `read_unaligned` reads a
            // 32-byte POD from a buffer proven to be at least that long.
            let ev: drm_event_crtc_sequence = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr().cast::<drm_event_crtc_sequence>())
            };
            // Pass user_data through undivided — the caller decodes the
            // low-32-bits crtc_id and (for absolute arms) the high tag bit.
            on_sequence(ev.user_data, ev.time_ns, ev.sequence);
        }
        Event::Vblank(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use drm::control::{Event, PageFlipEvent, crtc, from_u32};

    use super::dispatch_event;

    #[test]
    fn dispatch_event_passes_crtc_handle_for_page_flip() {
        let handle: crtc::Handle = from_u32(42).expect("non-zero raw handle");
        let event = Event::PageFlip(PageFlipEvent {
            frame: 7,
            duration: Duration::from_micros(123),
            crtc: handle,
        });

        let mut seen: Vec<(crtc::Handle, u32, Duration)> = Vec::new();
        dispatch_event(
            event,
            &mut |c, frame, dur| seen.push((c, frame, dur)),
            &mut |_, _, _| {},
        );

        assert_eq!(seen, vec![(handle, 7, Duration::from_micros(123))]);
    }

    #[test]
    fn dispatch_event_ignores_unknown() {
        let event = Event::Unknown(Vec::new());
        let mut called = 0u32;
        dispatch_event(event, &mut |_, _, _| called += 1, &mut |_, _, _| {});
        assert_eq!(called, 0);
    }

    #[test]
    fn drm_crtc_queue_sequence_struct_is_24_bytes() {
        // drm.h: __u32 crtc_id; __u32 flags; __u64 sequence; __u64 user_data;
        // → 4+4+8+8 = 24 bytes.
        assert_eq!(std::mem::size_of::<super::drm_crtc_queue_sequence>(), 24);
        assert_eq!(std::mem::align_of::<super::drm_crtc_queue_sequence>(), 8);
    }

    #[test]
    fn drm_event_crtc_sequence_struct_is_32_bytes() {
        // drm.h: struct drm_event base (8B) + __u64 user_data + __s64 time_ns
        // + __u64 sequence = 8 + 8 + 8 + 8 = 32.
        assert_eq!(std::mem::size_of::<super::drm_event_crtc_sequence>(), 32);
        assert_eq!(std::mem::align_of::<super::drm_event_crtc_sequence>(), 8);
    }

    #[test]
    fn drm_crtc_queue_sequence_ioctl_request_code() {
        // _IOWR('d' /*0x64*/, 0x3C, drm_crtc_queue_sequence):
        //   (3 << 30) | (24 << 16) | (0x64 << 8) | 0x3C = 0xC018643C
        assert_eq!(
            super::DRM_IOCTL_CRTC_QUEUE_SEQUENCE,
            0xC018_643C_u32 as super::IoctlReq
        );
    }

    #[test]
    fn queue_sequence_flags_absolute_with_next_on_miss() {
        use super::{DRM_CRTC_SEQUENCE_NEXT_ON_MISS, drm_crtc_queue_sequence};
        let req = drm_crtc_queue_sequence {
            crtc_id: 0x42,
            flags: DRM_CRTC_SEQUENCE_NEXT_ON_MISS,
            sequence: 0x1234_5678_9ABC_DEF0,
            user_data: 0x42,
        };
        assert_eq!(req.flags & 1, 0, "RELATIVE bit clear for absolute target");
        assert_eq!(req.flags & 2, 2, "NEXT_ON_MISS set");
    }

    #[test]
    fn dispatch_event_decodes_crtc_sequence() {
        use super::{DRM_EVENT_CRTC_SEQUENCE, drm_event_crtc_sequence, drm_event_header};
        let raw = drm_event_crtc_sequence {
            base: drm_event_header {
                r#type: DRM_EVENT_CRTC_SEQUENCE,
                length: 32,
            },
            user_data: 0xCAFE_BABE_0000_0042, // bottom 32 bits = crtc_id 0x42
            time_ns: 1_234_567_890_i64,
            sequence: 9_999,
        };
        let bytes: [u8; 32] = unsafe { std::mem::transmute(raw) };
        let event = Event::Unknown(bytes.to_vec());

        let mut advance_calls = Vec::<(crtc::Handle, u32, Duration)>::new();
        let mut seq_calls = Vec::<(u64, i64, u64)>::new();
        dispatch_event(
            event,
            &mut |c, m, u| advance_calls.push((c, m, u)),
            &mut |user_data, t, s| seq_calls.push((user_data, t, s)),
        );

        assert!(
            advance_calls.is_empty(),
            "sequence event must NOT route through advance callback"
        );
        // Full 64-bit user_data must reach the caller undivided — the
        // consumer (KmsBackend::on_crtc_sequence_event) decodes both the
        // low-32-bits crtc_id and the high tag bit itself.
        assert_eq!(
            seq_calls,
            vec![(0xCAFE_BABE_0000_0042u64, 1_234_567_890_i64, 9_999u64)]
        );
    }

    #[test]
    fn dispatch_event_ignores_wrong_length_sequence_event() {
        use super::{DRM_EVENT_CRTC_SEQUENCE, drm_event_header};
        let header = drm_event_header {
            r#type: DRM_EVENT_CRTC_SEQUENCE,
            length: 16,
        };
        let mut bytes = vec![0u8; 16];
        bytes[..8]
            .copy_from_slice(&unsafe { std::mem::transmute::<drm_event_header, [u8; 8]>(header) });
        let event = Event::Unknown(bytes);

        let mut seq_calls = 0usize;
        dispatch_event(event, &mut |_, _, _| {}, &mut |_, _, _| seq_calls += 1);
        assert_eq!(seq_calls, 0);
    }

    #[test]
    fn dispatch_event_ignores_unknown_event_type() {
        use super::drm_event_header;
        let header = drm_event_header {
            r#type: 99,
            length: 32,
        };
        let mut bytes = vec![0u8; 32];
        bytes[..8]
            .copy_from_slice(&unsafe { std::mem::transmute::<drm_event_header, [u8; 8]>(header) });
        let event = Event::Unknown(bytes);
        let mut seq_calls = 0usize;
        dispatch_event(event, &mut |_, _, _| {}, &mut |_, _, _| seq_calls += 1);
        assert_eq!(seq_calls, 0);
    }

    /// A `Device` over one end of a socket pair: `read` on it behaves like the
    /// DRM fd's (blocks while empty), and the peer injects raw event bytes.
    fn device_with_event_feed() -> (crate::drm::Device, std::os::unix::net::UnixStream) {
        let (ours, feed) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let device =
            crate::drm::Device::from_inherited_kms_fd(std::os::fd::OwnedFd::from(ours), "<test>");
        (device, feed)
    }

    /// Readiness reported by the core loop's poll can be consumed by
    /// `discard_old_drm_events_after_all_off` earlier in the same iteration;
    /// the later drain must see an empty fd and return, not block in `read`.
    #[test]
    fn drain_events_returns_when_no_event_is_pending() {
        let (device, feed) = device_with_event_feed();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut flips, mut sequences) = (0usize, 0usize);
            let result =
                super::drain_events(&device, |_, _, _| flips += 1, |_, _, _| sequences += 1);
            let _ = tx.send((result.is_ok(), flips + sequences));
        });
        let outcome = rx.recv_timeout(Duration::from_secs(2));
        drop(feed);
        assert_eq!(
            outcome.ok(),
            Some((true, 0)),
            "drain blocked on an empty DRM fd"
        );
    }

    #[test]
    fn drain_events_dispatches_a_pending_flip_complete() {
        use std::io::Write;
        let (device, mut feed) = device_with_event_feed();
        // struct drm_event_vblank (drm.h): type=DRM_EVENT_FLIP_COMPLETE,
        // length, user_data, tv_sec, tv_usec, sequence, crtc_id.
        let mut raw = Vec::with_capacity(32);
        raw.extend_from_slice(&2u32.to_ne_bytes());
        raw.extend_from_slice(&32u32.to_ne_bytes());
        raw.extend_from_slice(&0u64.to_ne_bytes());
        raw.extend_from_slice(&3u32.to_ne_bytes());
        raw.extend_from_slice(&500u32.to_ne_bytes());
        raw.extend_from_slice(&77u32.to_ne_bytes());
        raw.extend_from_slice(&42u32.to_ne_bytes());
        feed.write_all(&raw).expect("inject event");

        let mut seen: Vec<(crtc::Handle, u32, Duration)> = Vec::new();
        super::drain_events(&device, |c, f, d| seen.push((c, f, d)), |_, _, _| {}).expect("drain");
        let handle: crtc::Handle = from_u32(42).expect("non-zero raw handle");
        assert_eq!(seen, vec![(handle, 77, Duration::new(3, 500_000))]);
    }
}
