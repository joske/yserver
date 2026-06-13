//! Messages flowing into the single-threaded core loop.
//!
//! See `docs/superpowers/plans/2026-05-06-single-threaded-core.md` Phase B.

use std::os::{fd::OwnedFd, unix::net::UnixStream};

use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

use crate::host_x11::HostKeyEvent;

/// Snapshot of a libinput device's identity and touchpad configuration at
/// device-add time.  Plain data — no libinput handles, safe to send across
/// thread boundaries.  Collected by the input layer and forwarded to the core
/// so that Task 2 can seed the XI2 device-property registry.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Human-readable name (e.g. `"SynPS/2 Synaptics TouchPad"`).
    pub name: String,
    /// Evdev device node (e.g. `/dev/input/event4`).
    pub device_node: String,
    /// libinput sysname (e.g. `"event4"`).
    pub sysname: String,
    pub vendor_id: u32,
    pub product_id: u32,
    /// True when libinput classifies the device as a touchpad
    /// (tap finger count > 0).
    pub is_touchpad: bool,
    /// Full libinput config snapshot (meaningful only when is_touchpad).
    pub config: LibinputConfigSnapshot,
}

/// One libinput boolean config item: whether it's available on the device,
/// its current value, and its default. `Copy`/`Send` — no libinput handles.
#[derive(Debug, Clone, Copy, Default)]
pub struct BoolSetting {
    pub available: bool,
    pub current: bool,
    pub default: bool,
}

/// Accel speed (FLOAT). `available` + `current` + `default`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FloatSetting {
    pub available: bool,
    pub current: f32,
    pub default: f32,
}

/// 32-bit unsigned scalar setting (used for the scroll-button number).
#[derive(Debug, Clone, Copy, Default)]
pub struct U32Setting {
    pub available: bool,
    pub current: u32,
    pub default: u32,
}

/// One-hot over 2 slots: `current`/`default` are the active index (0 or 1) or
/// `None`. `available` indicates the device exposes this setting at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct OneHot2 {
    pub available: bool,
    pub current: Option<u8>,
    pub default: Option<u8>,
}

/// One-hot over 3 slots; `available_mask` is the bitmask of which slots
/// the device exposes (bit i set ⇒ slot i is supported). `current`/
/// `default` are the active index (0/1/2) or `None`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OneHot3 {
    pub available_mask: u8,
    pub current: Option<u8>,
    pub default: Option<u8>,
}

/// Bitflags over 2 slots. `available_mask` is which bits the device supports;
/// `current_mask`/`default_mask` are the active set.
#[derive(Debug, Clone, Copy, Default)]
pub struct BitFlags2 {
    pub available_mask: u8,
    pub current_mask: u8,
    pub default_mask: u8,
}

/// Full touchpad/pointer libinput config snapshot, gathered at DeviceAdded.
///
/// One `BoolSetting`/`FloatSetting`/... per libinput property group. All
/// fields are `Copy`, so the snapshot is `Copy`/`Send` and can flow over
/// the core channel verbatim.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibinputConfigSnapshot {
    pub tap: BoolSetting,
    pub tap_drag: BoolSetting,
    pub tap_drag_lock: BoolSetting,
    pub natural_scroll: BoolSetting,
    pub dwt: BoolSetting,
    pub left_handed: BoolSetting,
    pub middle_emulation: BoolSetting,
    pub scroll_button_lock: BoolSetting,
    /// Accel speed (FLOAT). available + current + default.
    pub accel: FloatSetting,
    /// Button-scrolling button number (CARD32).
    pub scroll_button: U32Setting,
    /// Tapping button map: available + which of {LRM=0, LMR=1} is current/default.
    pub tap_button_map: OneHot2,
    /// Scroll methods: bit0=2fg, bit1=edge, bit2=button. available_mask/current/default.
    pub scroll_method: OneHot3,
    /// Click methods: bit0=buttonareas, bit1=clickfinger.
    pub click_method: OneHot2,
    /// Accel profiles: bit0=adaptive, bit1=flat (custom excluded — feature-gated).
    pub accel_profile: OneHot2,
    /// Send-events: bitflags bit0=disabled, bit1=disabled-on-external-mouse.
    pub send_events: BitFlags2,
}

/// All inbound messages multiplexed onto the core thread.
///
/// Reader threads, the libinput thread, the signalfd watcher, and setup
/// threads all turn their respective fds into `Message`s and send them
/// through the unbounded `crossbeam-channel`. The core's mio poller
/// owns directly-attached fds (listener, client writers, drm, libinput,
/// host-X11, signalfd) and never reads bytes off them via the channel.
#[derive(Debug)]
pub enum Message {
    /// Sent by a setup thread mid-handshake. The core allocates resource
    /// IDs and snapshots screen geometry, replies via `response_tx`, and
    /// never blocks.
    SetupAllocate {
        id: ClientId,
        response_tx: crossbeam_channel::Sender<SetupAllocateResponse>,
    },
    /// Setup thread finished writing `setup_success`; hands the
    /// (still-blocking) stream to the core for split / register / reader
    /// spawn (D4).
    ClientSetupComplete {
        id: ClientId,
        stream: UnixStream,
        resource_id_base: u32,
        resource_id_mask: u32,
        byte_order: ClientByteOrder,
    },
    /// One framed X11 request from a client reader thread.
    Request {
        id: ClientId,
        sequence: SequenceNumber,
        header: RequestHeader,
        body: Vec<u8>,
        attached_fd: Option<OwnedFd>,
    },
    /// Reader thread (or write-side disconnect detection) noticed the
    /// client socket is gone.
    ClientDisconnected {
        id: ClientId,
        reason: std::io::Error,
    },
    /// Host input event (KMS libinput producer, or host-X11 dispatch
    /// after F2).
    HostInput(HostInputEvent),
    /// DRM completion fd is readable; backend should drain page-flip
    /// completions and submit the next composite if needed.
    PageFlipReady,
    /// signalfd readable.
    Shutdown,
    /// SIGUSR1 / VT release on direct-mode backends; diagnostic scanout
    /// dump fallback on backends that have not armed VT switching.
    VtRelease,
    /// SIGUSR2 / VT acquire on direct-mode backends; diagnostic drawable
    /// dump fallback on backends that have not armed VT switching.
    VtAcquire,
    /// `Ctrl+Alt+F<n>` detected by the input thread → request a switch to
    /// VT `n`. The backend calls `VT_ACTIVATE` on the core thread (we run
    /// the keyboard in K_OFF, so the kernel won't switch on its own — the
    /// server must initiate, like Xorg). No-op if VT switching isn't armed.
    SwitchVt(u32),
    /// SIGUSR1 received — the backend should dump the current scanout
    /// buffer to a file in cwd for offline inspection. Diagnostic-only;
    /// no-op for backends that don't drive their own composite.
    DumpScanout,
    /// SIGUSR2 received — the backend should dump the storage content
    /// of every "interesting" drawable (root, COW, every redirected
    /// backing) to files in cwd. Splits the Stage 4d "shadow only"
    /// hover-flicker bug into "is B empty / is B correct but COW
    /// wrong / is B+COW correct but scanout stale" — without storage
    /// dumps the three are indistinguishable from the scanout alone.
    /// Diagnostic-only; no-op for backends that don't redirect.
    DumpDrawables,
}

#[derive(Debug)]
pub enum HostInputEvent {
    PointerMotion {
        x: i32,
        y: i32,
        time: u32,
    },
    PointerButton {
        /// Linux input button code (`BTN_LEFT = 0x110`, `BTN_RIGHT = 0x111`,
        /// `BTN_MIDDLE = 0x112`, etc.). u16 because libinput codes are
        /// always < 0x200 and u8 would silently truncate `BTN_LEFT` to
        /// `0x10` — the KMS backend's `0x110 => 1` mapping then never
        /// matched and clicks were dropped.
        ///
        /// Scroll wheel "clicks" arrive here too, using yserver-synthetic
        /// codes (`SYNTH_SCROLL_*` below). libinput models scroll as axis
        /// events, but X11 only models scroll as button-4/5/6/7 click
        /// events — the libinput thread accumulates axis deltas into
        /// press+release pairs of these synthetic codes, and backends
        /// translate them to X11 buttons.
        button: u16,
        pressed: bool,
        time: u32,
    },
    Key(HostKeyEvent),
    /// A new input device has been enumerated by libinput.  Carries a
    /// snapshot of its identity and touchpad configuration so the core can
    /// seed per-device state (Task 2: XI2 property registry).
    DeviceAdded(DeviceInfo),
    /// An input device has been removed.  `device_node` is the evdev path
    /// that was reported at add time and can be used to look up the device.
    DeviceRemoved {
        device_node: String,
    },
}

/// Synthetic Linux-style input codes for scroll-wheel "buttons" carried
/// in `HostInputEvent::PointerButton`. Picked outside the standard Linux
/// BTN_* range (BTN_TASK = 0x117 is the highest real code) so a real
/// device button can never collide.
pub const SYNTH_SCROLL_UP: u16 = 0x180;
pub const SYNTH_SCROLL_DOWN: u16 = 0x181;
pub const SYNTH_SCROLL_LEFT: u16 = 0x182;
pub const SYNTH_SCROLL_RIGHT: u16 = 0x183;

/// Reply from the core to a setup thread's `SetupAllocate` request.
///
/// `resource_id_base == 0` signals the id allocator is exhausted; the
/// setup thread then writes `setup_failed` to its peer and exits.
#[derive(Debug)]
pub struct SetupAllocateResponse {
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub screen_width_px: u16,
    pub screen_height_px: u16,
    pub current_input_masks: u32,
}

/// Derive an evdev device node path from a libinput sysname.
///
/// libinput's sysname for an evdev device is the kernel name under
/// `/dev/input/` (e.g. `"event4"`), so the devnode is always
/// `/dev/input/<sysname>`.  Used as a fallback when the `udev` feature is
/// not available, and also as the canonical form we store.
pub fn device_node_from_sysname(sysname: &str) -> String {
    format!("/dev/input/{sysname}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_variant_matches() {
        assert!(matches!(Message::Shutdown, Message::Shutdown));
    }

    #[test]
    fn libinput_config_snapshot_is_copy_and_default() {
        let s = LibinputConfigSnapshot::default();
        assert!(!s.tap.available);
        let _copy = s; // must be Copy
        let _again = s; // still usable → Copy, not Move
    }

    #[test]
    fn device_node_from_sysname_formats_correctly() {
        assert_eq!(device_node_from_sysname("event0"), "/dev/input/event0");
        assert_eq!(device_node_from_sysname("event12"), "/dev/input/event12");
    }

    #[test]
    fn device_info_is_clone_and_debug() {
        let info = DeviceInfo {
            name: "Test Touchpad".into(),
            device_node: "/dev/input/event4".into(),
            sysname: "event4".into(),
            vendor_id: 0x046d,
            product_id: 0x0000,
            is_touchpad: true,
            config: LibinputConfigSnapshot {
                tap: BoolSetting {
                    available: true,
                    current: true,
                    default: false,
                },
                dwt: BoolSetting {
                    available: true,
                    current: true,
                    default: false,
                },
                ..Default::default()
            },
        };
        let info2 = info.clone();
        assert_eq!(info.name, info2.name);
        // Debug must not panic.
        let _ = format!("{info2:?}");
    }

    #[test]
    fn host_input_event_device_variants() {
        let info = DeviceInfo {
            name: "Mouse".into(),
            device_node: "/dev/input/event1".into(),
            sysname: "event1".into(),
            vendor_id: 1,
            product_id: 2,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        };
        assert!(matches!(
            HostInputEvent::DeviceAdded(info),
            HostInputEvent::DeviceAdded(_)
        ));
        let removed = HostInputEvent::DeviceRemoved {
            device_node: "/dev/input/event1".into(),
        };
        match removed {
            HostInputEvent::DeviceRemoved { device_node } => {
                assert_eq!(device_node, "/dev/input/event1");
            }
            other => panic!("expected DeviceRemoved, got {other:?}"),
        }
    }
}
