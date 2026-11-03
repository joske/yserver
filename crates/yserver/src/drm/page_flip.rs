//! Page-flip submission + completion drain.
//!
//! `submit_flip` atomic-commits a new FB_ID on the primary plane with
//! PAGE_FLIP_EVENT | NONBLOCK; the kernel produces a completion event
//! on the DRM fd when scanout latches the new buffer.
//!
//! `drain_events` reads the raw DRM event stream so flip cookies and CRTC
//! handles remain independent through completion dispatch.

use std::io;

use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, atomic::AtomicModeReq, crtc, framebuffer, from_u32,
};

use crate::drm::{
    Device,
    atomic::{
        AtomicCommitEvent, AtomicCommitKind, AtomicCommitRequest, AtomicCommitSink,
        CursorPlanePosition,
    },
    modeset::{
        Output, PendingCrtcProperties, PropMap, append_pending_crtc_properties,
        submit_frame_with_pending_crtc_properties,
    },
};

#[cfg(target_os = "linux")]
pub(crate) type IoctlReq = libc::Ioctl;
#[cfg(not(target_os = "linux"))]
pub(crate) type IoctlReq = libc::c_ulong;

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

/// Kernel `DRM_EVENT_FLIP_COMPLETE` event type id.
pub(crate) const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

/// Bytes requested per read of the DRM fd.
const EVENT_READ_LEN: usize = 1024;

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct drm_event_vblank {
    pub base: drm_event_header,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
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

// ── Legacy DRM_IOCTL_WAIT_VBLANK plumbing ───────────────────────
//
// Fallback for devices that reject CRTC_QUEUE_SEQUENCE, as Xorg
// modesetting's `ms_queue_vblank` / `ms_get_kernel_ust_msc` do. Layouts
// mirror `union drm_wait_vblank` in `<drm/drm.h>`.

/// Kernel `DRM_EVENT_VBLANK` event type id.
pub(crate) const DRM_EVENT_VBLANK: u32 = 0x01;

pub(crate) const DRM_VBLANK_ABSOLUTE: u32 = 0x0000_0000;
pub(crate) const DRM_VBLANK_RELATIVE: u32 = 0x0000_0001;
pub(crate) const DRM_VBLANK_EVENT: u32 = 0x0400_0000;
pub(crate) const DRM_VBLANK_NEXTONMISS: u32 = 0x1000_0000;
pub(crate) const DRM_VBLANK_SECONDARY: u32 = 0x2000_0000;
pub(crate) const DRM_VBLANK_HIGH_CRTC_SHIFT: u32 = 1;

/// Legacy `signal` bit marking the shared Present wake. `signal` is an
/// `unsigned long`, so the tag stays inside 32 bits.
pub(crate) const LEGACY_VBLANK_ABSOLUTE_SIGNAL: u32 = 1 << 31;

/// Sequence-event `user_data` tag the backend decodes as a shared Present wake.
pub(crate) const SEQUENCE_ABSOLUTE_USER_DATA: u64 = 1 << 63;

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct drm_wait_vblank_request {
    pub r#type: u32,
    pub sequence: u32,
    pub signal: libc::c_ulong,
}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct drm_wait_vblank_reply {
    pub r#type: u32,
    pub sequence: u32,
    pub tval_sec: libc::c_long,
    pub tval_usec: libc::c_long,
}

#[allow(non_camel_case_types)]
#[repr(C)]
pub(crate) union drm_wait_vblank {
    pub request: drm_wait_vblank_request,
    pub reply: drm_wait_vblank_reply,
}

// `_IOWR('d', 0x3A, union drm_wait_vblank)`; see the sequence ioctl above.
pub(crate) const DRM_IOCTL_WAIT_VBLANK: IoctlReq = ((3u32 << 30)
    | ((std::mem::size_of::<drm_wait_vblank>() as u32) << 16)
    | (0x64u32 << 8)
    | 0x3A) as IoctlReq;

/// Xorg `drmmode_crtc_vblank_pipe`: the CRTC's index in the device's
/// resource list selects the pipe through the high-CRTC bits.
pub(crate) fn wait_vblank_pipe_bits(pipe: u32) -> u32 {
    match pipe {
        0 => 0,
        1 => DRM_VBLANK_SECONDARY,
        _ => (pipe << DRM_VBLANK_HIGH_CRTC_SHIFT) & 0x3e,
    }
}

/// Index of `crtc` in the device's CRTC list, the legacy vblank pipe.
pub(crate) fn crtc_pipe_index(device: &Device, crtc: crtc::Handle) -> Option<u32> {
    let resources = device.resource_handles().ok()?;
    let index = resources.crtcs().iter().position(|c| *c == crtc)?;
    u32::try_from(index).ok()
}

/// Map a legacy vblank event's `signal` onto the sequence-event `user_data`
/// encoding so both kinds share one completion path.
pub(crate) fn sequence_user_data_from_legacy_signal(signal: u64) -> u64 {
    let crtc_id = signal & u64::from(!LEGACY_VBLANK_ABSOLUTE_SIGNAL);
    if signal & u64::from(LEGACY_VBLANK_ABSOLUTE_SIGNAL) == 0 {
        crtc_id
    } else {
        SEQUENCE_ABSOLUTE_USER_DATA | crtc_id
    }
}

fn wait_vblank_ioctl(
    device: &Device,
    request: drm_wait_vblank_request,
) -> io::Result<drm_wait_vblank_reply> {
    use std::os::{fd::AsFd, unix::io::AsRawFd};

    let mut arg = drm_wait_vblank { request };
    // SAFETY: `arg` is the kernel's union layout, live for the call; the
    // device keeps its fd open.
    let rc = unsafe {
        libc::ioctl(
            device.as_fd().as_raw_fd(),
            DRM_IOCTL_WAIT_VBLANK,
            &raw mut arg,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the kernel wrote the reply member.
    Ok(unsafe { arg.reply })
}

/// Current vblank count and timestamp of `pipe` (relative-0 wait).
///
/// # Errors
///
/// `EOPNOTSUPP`/`ENOTTY`/`EINVAL` when the device has no vblank support.
pub(crate) fn wait_vblank_query(
    device: &Device,
    pipe: u32,
) -> io::Result<(u32, std::time::Duration)> {
    let reply = wait_vblank_ioctl(
        device,
        drm_wait_vblank_request {
            r#type: DRM_VBLANK_RELATIVE | wait_vblank_pipe_bits(pipe),
            sequence: 0,
            signal: 0,
        },
    )?;
    let secs = u64::try_from(reply.tval_sec).unwrap_or(0);
    let micros = u64::try_from(reply.tval_usec).unwrap_or(0);
    Ok((
        reply.sequence,
        std::time::Duration::from_secs(secs) + std::time::Duration::from_micros(micros),
    ))
}

/// Queue a `DRM_EVENT_VBLANK` on `pipe`: relative `+sequence`, or absolute
/// with NEXTONMISS. Returns the kernel's scheduled sequence.
///
/// # Errors
///
/// `EOPNOTSUPP`/`ENOTTY`/`EINVAL` when the device has no vblank support.
pub(crate) fn wait_vblank_queue_event(
    device: &Device,
    pipe: u32,
    relative: bool,
    sequence: u32,
    signal: u32,
) -> io::Result<u32> {
    let kind = if relative {
        DRM_VBLANK_RELATIVE
    } else {
        DRM_VBLANK_ABSOLUTE | DRM_VBLANK_NEXTONMISS
    };
    let reply = wait_vblank_ioctl(
        device,
        drm_wait_vblank_request {
            r#type: kind | DRM_VBLANK_EVENT | wait_vblank_pipe_bits(pipe),
            sequence,
            signal: libc::c_ulong::from(signal),
        },
    )?;
    Ok(reply.sequence)
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

/// Submit one scene flip through its owning device's recorder/timer and append
/// any visible atomic cursor-plane positions for this output. The legacy
/// non-rendering page-flip callers keep using `submit_flip_with_fences`.
pub(crate) fn submit_scene_flip_with_fences(
    device: &Device,
    atomic_commit_sink: &AtomicCommitSink,
    output: &Output,
    fb_id: framebuffer::Handle,
    in_fence_fd: i32,
    out_fence_holder: &mut i32,
    cursor_positions: &[CursorPlanePosition],
    timing: crate::drm::async_flip::FlipTiming,
) -> io::Result<u64> {
    submit_scene_flip_with_fences_tearing(
        device,
        atomic_commit_sink,
        output,
        fb_id,
        in_fence_fd,
        out_fence_holder,
        cursor_positions,
        timing,
        crate::drm::atomic::TearingCommitContext::NONE,
    )
}

pub(crate) fn submit_scene_flip_with_fences_tearing(
    device: &Device,
    atomic_commit_sink: &AtomicCommitSink,
    output: &Output,
    fb_id: framebuffer::Handle,
    in_fence_fd: i32,
    out_fence_holder: &mut i32,
    cursor_positions: &[CursorPlanePosition],
    timing: crate::drm::async_flip::FlipTiming,
    tearing_context: crate::drm::atomic::TearingCommitContext,
) -> io::Result<u64> {
    let cursor_positions = match timing {
        crate::drm::async_flip::FlipTiming::Vsync => cursor_positions,
        crate::drm::async_flip::FlipTiming::Async => &[],
    };
    let mut request = AtomicCommitRequest::new();
    request.add_raw_property(
        output.plane.into(),
        output.plane_fb_id_prop,
        u64::from(u32::from(fb_id)),
    );
    request.add_raw_property(
        output.plane.into(),
        output.plane_crtc_id_prop,
        u64::from(u32::from(output.crtc)),
    );
    let in_fence_prop = match output.plane_in_fence_fd_prop {
        Some(prop) => prop,
        None => PropMap::for_object(device, output.plane)?.id("IN_FENCE_FD")?,
    };
    request.add_raw_property(
        output.plane.into(),
        in_fence_prop,
        in_fence_fd as i64 as u64,
    );
    if timing == crate::drm::async_flip::FlipTiming::Async {
        *out_fence_holder = -1;
    } else {
        let out_fence_prop = match output.crtc_out_fence_ptr_prop {
            Some(prop) => prop,
            None => PropMap::for_object(device, output.crtc)?.id("OUT_FENCE_PTR")?,
        };
        let out_fence_ptr = (out_fence_holder as *mut i32) as usize as u64;
        request.add_raw_property(output.crtc.into(), out_fence_prop, out_fence_ptr);
    }
    for position in cursor_positions {
        request.add_cursor_position(*position);
    }
    let event = AtomicCommitEvent {
        crtc: output.crtc,
        motion_timestamp: cursor_positions
            .iter()
            .find(|position| position.crtc == output.crtc)
            .and_then(|position| position.motion_timestamp),
    };
    let kind = match timing {
        crate::drm::async_flip::FlipTiming::Vsync if cursor_positions.is_empty() => {
            AtomicCommitKind::Scene
        }
        crate::drm::async_flip::FlipTiming::Vsync => AtomicCommitKind::SceneWithCursor,
        crate::drm::async_flip::FlipTiming::Async => AtomicCommitKind::Async,
    };
    let flags = AtomicCommitFlags::PAGE_FLIP_EVENT
        | AtomicCommitFlags::NONBLOCK
        | match timing {
            crate::drm::async_flip::FlipTiming::Vsync => AtomicCommitFlags::empty(),
            crate::drm::async_flip::FlipTiming::Async => AtomicCommitFlags::PAGE_FLIP_ASYNC,
        };
    let crtc_properties = if timing == crate::drm::async_flip::FlipTiming::Vsync {
        append_pending_crtc_properties(
            device,
            atomic_commit_sink,
            &mut request,
            std::iter::once((output, false)),
        )
    } else {
        PendingCrtcProperties::empty()
    };
    submit_frame_with_pending_crtc_properties(
        device,
        atomic_commit_sink,
        request,
        crtc_properties.request_without_crtc_properties,
        crtc_properties.gamma_blobs,
        crtc_properties.vrr_changes,
        |request| {
            atomic_commit_sink.submit_tearing_with_commit_id(
                device,
                flags,
                kind,
                request,
                &[event],
                tearing_context,
            )
        },
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
    A: FnMut(crtc::Handle, u32, std::time::Duration, u64),
    S: FnMut(u64, i64, u64),
{
    if !event_pending(device)? {
        return Ok(());
    }
    let mut buffer = [0u8; EVENT_READ_LEN];
    let amount = read_events(device, &mut buffer)?;
    dispatch_events(&buffer[..amount], &mut on_advance, &mut on_sequence);
    Ok(())
}

fn read_events(device: &Device, buffer: &mut [u8]) -> io::Result<usize> {
    use std::os::{fd::AsFd, unix::io::AsRawFd};

    // SAFETY: `buffer` is a writable live region and `device` keeps its fd open.
    let result = unsafe {
        libc::read(
            device.as_fd().as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    usize::try_from(result).map_err(|_| io::Error::last_os_error())
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

/// Walk every complete event in the read. A malformed length ends the walk
/// because there is no safe next record boundary to use.
fn dispatch_events<A, S>(bytes: &[u8], on_advance: &mut A, on_sequence: &mut S) -> usize
where
    A: FnMut(crtc::Handle, u32, std::time::Duration, u64),
    S: FnMut(u64, i64, u64),
{
    let header_len = std::mem::size_of::<drm_event_header>();
    let mut offset = 0;
    let mut unroutable_flips = 0;
    while bytes.len() - offset >= header_len {
        // SAFETY: the loop guard proves a complete header at `offset`.
        let header = unsafe {
            std::ptr::read_unaligned(bytes[offset..].as_ptr().cast::<drm_event_header>())
        };
        let length = header.length as usize;
        if length < header_len || length > bytes.len() - offset {
            return unroutable_flips;
        }
        let event = &bytes[offset..offset + length];
        match header.r#type {
            DRM_EVENT_FLIP_COMPLETE => {
                unroutable_flips += usize::from(dispatch_flip_complete(event, on_advance));
            }
            DRM_EVENT_CRTC_SEQUENCE => dispatch_crtc_sequence(event, on_sequence),
            DRM_EVENT_VBLANK => dispatch_legacy_vblank(event, on_sequence),
            _ => {}
        }
        offset += length;
    }
    unroutable_flips
}

fn dispatch_flip_complete<A>(event: &[u8], on_advance: &mut A) -> bool
where
    A: FnMut(crtc::Handle, u32, std::time::Duration, u64),
{
    if event.len() < std::mem::size_of::<drm_event_vblank>() {
        return false;
    }
    // SAFETY: the payload length contains the complete ABI structure.
    let vblank = unsafe { std::ptr::read_unaligned(event.as_ptr().cast::<drm_event_vblank>()) };
    if vblank.crtc_id == 0 {
        log::warn!(
            target: "yserver::drm::page_flip",
            "unroutable DRM_EVENT_FLIP_COMPLETE has crtc_id=0 user_data={}",
            vblank.user_data
        );
        return true;
    }
    let Some(crtc) = from_u32::<crtc::Handle>(vblank.crtc_id) else {
        log::warn!(
            target: "yserver::drm::page_flip",
            "unroutable DRM_EVENT_FLIP_COMPLETE has invalid crtc_id={} user_data={}",
            vblank.crtc_id,
            vblank.user_data
        );
        return true;
    };
    let duration = std::time::Duration::from_secs(u64::from(vblank.tv_sec))
        + std::time::Duration::from_micros(u64::from(vblank.tv_usec));
    on_advance(crtc, vblank.sequence, duration, vblank.user_data);
    false
}

fn dispatch_crtc_sequence<S>(event: &[u8], on_sequence: &mut S)
where
    S: FnMut(u64, i64, u64),
{
    if event.len() != std::mem::size_of::<drm_event_crtc_sequence>() {
        return;
    }
    // SAFETY: the length check proves this is a complete sequence event.
    let ev = unsafe { std::ptr::read_unaligned(event.as_ptr().cast::<drm_event_crtc_sequence>()) };
    on_sequence(ev.user_data, ev.time_ns, ev.sequence);
}

/// A legacy vblank event joins the sequence-event stream: its 32-bit
/// counter and timestamp become the `(user_data, time_ns, sequence)` triple.
fn dispatch_legacy_vblank<S>(event: &[u8], on_sequence: &mut S)
where
    S: FnMut(u64, i64, u64),
{
    if event.len() < std::mem::size_of::<drm_event_vblank>() {
        return;
    }
    // SAFETY: the payload length contains the complete ABI structure.
    let vblank = unsafe { std::ptr::read_unaligned(event.as_ptr().cast::<drm_event_vblank>()) };
    let time_ns = i64::from(vblank.tv_sec) * 1_000_000_000 + i64::from(vblank.tv_usec) * 1_000;
    on_sequence(
        sequence_user_data_from_legacy_signal(vblank.user_data),
        time_ns,
        u64::from(vblank.sequence),
    );
}

#[cfg(test)]
mod tests;
