//! libseat session management + VT switching (wlroots model).
//!
//! Libseat mode: this owns `libseat::Seat` and the list of devices we
//! opened through it. Lives entirely on the core-loop thread (libseat
//! is `!Send`). Direct mode is a marker — no libseat, VT switching is
//! handled separately by the console guard on the direct-mode path.
//!
//! Spec: docs/superpowers/specs/2026-05-27-vt-switching-design.md

// Types and methods here are consumed by Tasks 7/8 (KmsBackendV2 + lib.rs).
// Suppress dead-code lint until those callers exist; remove once wired in.
#![allow(dead_code)]

pub mod state;

use std::{
    cell::RefCell,
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    rc::Rc,
};

use libseat::{Seat as LibSeat, SeatEvent};

use self::state::SeatEventKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// DRM card (primary node). Master is owned by libseat/logind, not
    /// acquired by us (Deviation #5); `is_kms` distinguishes the KMS card
    /// from a render-only node for bookkeeping.
    Drm {
        is_kms: bool,
    },
    Input,
}

// libseat returns `errno::Errno` on failure. `errno` (0.3) provides
// `impl From<Errno> for io::Error`, so map with `.map_err(io::Error::from)`
// — no need to name the type or add `errno` as a direct dep (the trait
// impl is in scope globally; the closure infers `e: errno::Errno`).

/// A device opened through libseat. We OWN the libseat `Device` — it has
/// NO `Drop` impl, so the only way to release it is `Seat::close_device`,
/// which consumes it by value. We hand our owner (drm::Device or
/// libinput) a `dup` of the fd; `handed_fd` is that dup's number, used to
/// find this entry from libinput's `close_restricted(OwnedFd)`. The dup
/// shares the same open-file-description as libseat's fd, so DRM master
/// (which is per-description) and kernel revoke-on-suspend apply to both.
pub struct ManagedDevice {
    pub device: libseat::Device,
    pub path: PathBuf,
    pub handed_fd: RawFd,
    pub kind: DeviceKind,
}

/// The `!Send` libseat core, shared (single-thread) between the KMS
/// backend and the libinput `LibseatInterface`.
pub struct LibseatInner {
    seat: LibSeat,
    pub devices: Vec<ManagedDevice>,
}

impl LibseatInner {
    /// Open a device through libseat and return an `OwnedFd` (a dup of
    /// libseat's fd) for our owner to hold. libseat keeps its own fd
    /// alive until `close_device`; the dup is a distinct fd number over
    /// the same description (see `ManagedDevice`). Mirrors wlroots'
    /// `wlr_session_open_file` (`backend/session/session.c`).
    pub fn open_device(&mut self, path: &Path, kind: DeviceKind) -> io::Result<OwnedFd> {
        let device = self.seat.open_device(&path).map_err(io::Error::from)?;
        let owned = device.as_fd().try_clone_to_owned()?; // dup(2)
        let handed_fd = owned.as_raw_fd();
        self.devices.push(ManagedDevice {
            device,
            path: path.to_path_buf(),
            handed_fd,
            kind,
        });
        Ok(owned)
    }

    /// Close the device whose handed-out fd matches `handed_fd` (called
    /// from libinput's `close_restricted`). Consumes the libseat `Device`
    /// by value. No-op if unknown.
    pub fn close_device_by_fd(&mut self, handed_fd: RawFd) {
        if let Some(idx) = self.devices.iter().position(|d| d.handed_fd == handed_fd) {
            let md = self.devices.remove(idx);
            if let Err(e) = self.seat.close_device(md.device) {
                log::warn!("seat: close_device(handed_fd={handed_fd}) failed: {e}");
            }
        }
    }

    /// Request a VT switch. Fire-and-forget: libseat does not guarantee a
    /// switch will occur, and any state transition arrives later via the
    /// disable callback.
    pub fn switch_session(&mut self, vt: u32) -> io::Result<()> {
        self.seat.switch_session(vt as i32).map_err(io::Error::from)
    }

    /// Ack a disable: tell libseat we've quiesced. Only after this does
    /// the kernel allow the VT switch to proceed.
    pub fn disable(&mut self) -> io::Result<()> {
        self.seat.disable().map_err(io::Error::from)
    }

    pub fn fd(&mut self) -> io::Result<RawFd> {
        self.seat
            .get_fd()
            .map(|b| b.as_raw_fd())
            .map_err(io::Error::from)
    }

    /// Non-blocking dispatch. The enable/disable callback closure runs
    /// inside this call and pushes into the shared `pending_events` queue.
    pub fn dispatch(&mut self) -> io::Result<()> {
        let n = self.seat.dispatch(0).map_err(io::Error::from)?;
        if n > 0 {
            log::debug!("seat: libseat dispatch delivered {n} event(s)");
        }
        Ok(())
    }
}

/// Seat mode. `Direct` is the marker variant for the no-libseat path —
/// it carries no state because device opens go through today's direct
/// code, not this module.
pub enum Seat {
    Libseat {
        inner: Rc<RefCell<LibseatInner>>,
        /// Shared with the libseat callback closure; the closure writes
        /// Enable/Disable here, the backend drains after each dispatch.
        pending_events: Rc<RefCell<Vec<SeatEventKind>>>,
    },
    Direct,
}

impl Seat {
    /// Try to open `seat0` via libseat; fall back to `Direct` on any
    /// error (matches wlroots / the spec's single rule).
    #[must_use]
    pub fn open() -> Self {
        let pending_events: Rc<RefCell<Vec<SeatEventKind>>> = Rc::new(RefCell::new(Vec::new()));
        let cb_events = Rc::clone(&pending_events);
        // The callback is `'static FnMut` and cannot borrow the backend;
        // it only pushes event kinds into the shared queue.
        let seat = LibSeat::open(move |_seat, event| {
            let kind = match event {
                SeatEvent::Enable => SeatEventKind::Enable,
                SeatEvent::Disable => SeatEventKind::Disable,
            };
            log::info!("seat: libseat callback fired — {kind:?} queued");
            cb_events.borrow_mut().push(kind);
        });
        match seat {
            Ok(mut seat) => {
                // Block until the initial Enable (session active) before we
                // open any device, so DRM master is present when we enable
                // atomic caps. wlroots does the same in libseat_open_seat.
                // Bounded to ~5s so a broken backend can't hang startup.
                let mut active = false;
                for _ in 0..50 {
                    if let Err(e) = seat.dispatch(100) {
                        log::warn!("yserver: libseat initial dispatch failed: {e}");
                        break;
                    }
                    if pending_events.borrow().contains(&SeatEventKind::Enable) {
                        active = true;
                        break;
                    }
                }
                if !active {
                    log::warn!("yserver: libseat opened but no initial Enable; using Direct");
                    return Seat::Direct;
                }
                // Consume the initial Enable: the backend starts in
                // SeatState::Active, so this event must not later be
                // re-interpreted as a resume.
                pending_events.borrow_mut().clear();
                log::info!("yserver: libseat session opened + active; VT switching enabled");
                Seat::Libseat {
                    inner: Rc::new(RefCell::new(LibseatInner {
                        seat,
                        devices: Vec::new(),
                    })),
                    pending_events,
                }
            }
            Err(e) => {
                log::info!("yserver: libseat unavailable ({e}); opening devices directly");
                Seat::Direct
            }
        }
    }

    #[must_use]
    pub fn is_libseat(&self) -> bool {
        matches!(self, Seat::Libseat { .. })
    }

    /// Return a clone of the `LibseatInner` `Rc` if in libseat mode.
    #[must_use]
    pub fn libseat_inner(&self) -> Option<Rc<RefCell<LibseatInner>>> {
        match self {
            Seat::Libseat { inner, .. } => Some(Rc::clone(inner)),
            Seat::Direct => None,
        }
    }
}
