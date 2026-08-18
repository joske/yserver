//! `RecordingBackend` — test double for the `Backend` trait. Records
//! every method call into a per-instance log so unit tests can assert
//! the exact host-side request sequence produced by a `nested.rs`
//! request-handler hot-path.
//!
//! Methods that the existing tests don't exercise are
//! `unimplemented!()` — calling them in a test fails loudly. Adding a
//! new test that drives one is the cheap path: implement the recorder
//! variant + impl block inline.
//!
//! The methods we DO implement are picked to cover the
//! CreateWindow → MapWindow → DestroyWindow lifecycle (Phase 3.6
//! invariant: every InputOutput sub-window goes through host
//! create/map/destroy) plus the helpers needed to make the lifecycle
//! tests run end-to-end (`window_id` so `nested::run` can resolve
//! ROOT_WINDOW's host xid; `set_container_background_pixel` because
//! `nested::handle_request`'s ChangeWindowAttributes path on
//! ROOT_WINDOW pokes the container).

use std::{io, sync::Mutex};

use yserver_protocol::x11::{ClipRectangles, FontMetrics, ResourceId, glx, xfixes};

use crate::{
    backend::{
        AnyHandle, Backend, ClipState, CompletedPresentEvent, CrtcConfigApply, CrtcConfigToken,
        CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle, ModeSpec, OriginContext,
        PictureHandle, PixmapHandle, PresentScanoutCandidate, PresentSourceWait, WindowHandle,
    },
    host_x11::{HostSubwindowConfig, HostSubwindowVisual, HostXidMap, PointerPosition},
};

/// Records each method call. Variants are added on demand; tests
/// assert against `Vec<RecordedCall>` snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedCall {
    CreateSubwindow {
        parent: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    },
    DestroySubwindow(u32),
    MapSubwindow(u32),
    UnmapSubwindow(u32),
    ConfigureSubwindow {
        host_xid: u32,
        config: HostSubwindowConfig,
    },
    ReparentSubwindow {
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    },
    ChangeSubwindowAttributes {
        host_xid: u32,
        value_mask: u32,
        values: Vec<u32>,
    },
    UpdateHostEventMask {
        host_xid: u32,
        mask: u32,
        enabled: bool,
    },
    RegisterTopLevel {
        nested_id: ResourceId,
        host_xid: u32,
    },
    RegisterSubwindow {
        nested_id: ResourceId,
        host_xid: u32,
    },
    UnregisterHostWindow(u32),
    CreatePixmap {
        depth: u8,
        width: u16,
        height: u16,
    },
    FreePixmap(u32),
    SetContainerBackgroundPixel(u32),
    SetContainerBackgroundPixmap(u32),
    OpenFont(String),
    CloseFont(u32),
    Ping,
    ReleaseRedirectedBacking(u32),
    RetainBackingStorage(u32),
    DropBackingStorage(u32),
    AllocateRedirectedBacking {
        host_window: u32,
        width: u16,
        height: u16,
        depth: u8,
    },
    SetWindowSceneParticipation {
        host_window: u32,
        participating: bool,
    },
    SetBackingSceneParticipation {
        backing: u32,
        participating: bool,
    },
    CopyArea {
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    },
    DefineCursor {
        host_window_xid: u32,
        cursor_host_xid: u32,
    },
    RecolorCursor {
        host_xid: u32,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    },
    SetDpmsPower(u8),
    SetProviderOutputSource {
        provider: u32,
        source_provider: Option<u32>,
    },
    ApplyCrtcConfig {
        output_id: u32,
        connector: String,
        mode: Option<ModeSpec>,
        x: i32,
        y: i32,
    },
    /// GLX-TFP Task 3.4: `acquire_glx_pixmap_export(host_xid)` called.
    AcquireGlxPixmapExport(u32),
    /// GLX-TFP Task 3.4: `release_glx_pixmap_export(host_xid)` called.
    ReleaseGlxPixmapExport(u32),
    /// GLX-TFP Task 3.5: `promote_pixmap_exportable(host_xid)` called
    /// (the lightweight bind hook — does NOT touch the lifetime refcount).
    PromotePixmapExportable(u32),
    /// `set_shape_rectangles(host_xid, kind, rects)` called. `rects`
    /// captures shape *presence*: `None` = unset (backend drops the
    /// entry), `Some(0)` = explicit empty region (distinct from unset),
    /// `Some(n)` = a concrete region of `n` rects. The `None`-vs-`Some(0)`
    /// distinction is the DRIFT 1 fix — tests assert it directly.
    SetShapeRectangles {
        host_xid: u32,
        kind: u8,
        rects: Option<usize>,
    },
    /// Task 4: `mark_dirty()` called. Trait default is a no-op
    /// (`trait_def.rs:583`); recorded here so ordering tests (e.g. against
    /// `MaybeComposite`) can read it straight off the shared `calls` log.
    MarkDirty,
    /// `flush_before_damage_notify()` called before externally-observable
    /// damage output may drain.
    FlushBeforeDamageNotify,
    /// Task 4: `maybe_composite()` called. Trait default is a no-op
    /// (`trait_def.rs:603`); recorded so `run_iteration_tail` tests can
    /// assert it runs after the drain that feeds it.
    MaybeComposite,
    /// Task 4: `drain_completed_present_events()` called, before the
    /// queued completions (if any) are handed back.
    DrainCompletedPresentEvents,
    /// Task 4 fix-forward: `arm_present_completion_idle_vblanks()` called.
    /// Trait default is `Ok(0)` (`trait_def.rs:2117`); recorded so a test
    /// can pin this running strictly after `maybe_composite` (the KMS gate
    /// this arm hides behind only clears post-compose).
    ArmPresentCompletionIdleVblanks,
}

type GammaTriplet = (Vec<u16>, Vec<u16>, Vec<u16>);

/// Test double for `Backend`. Auto-allocates host xids from a private
/// counter so create-then-destroy round trips read back the same xid.
pub struct RecordingBackend {
    pub calls: Mutex<Vec<RecordedCall>>,
    next_handle: Mutex<u32>,
    fake_window_id: u32,
    fake_root_visual_xid: u32,
    /// Phase 6.3 Step 4: shared `host_xid → ResourceId` map exposed
    /// through `Backend::xid_map`. Tests inspect it via `Backend`'s
    /// trait surface.
    xid_map: HostXidMap,
    /// E3 liveness counter — incremented every time
    /// `on_page_flip_ready` is invoked.
    pub page_flip_count: std::sync::atomic::AtomicU32,
    /// Exact DRM fds delivered to `on_page_flip_ready`. Multi-device
    /// core-loop tests use this to verify same-kind poll sources remain
    /// distinguishable.
    pub page_flip_fds: Mutex<Vec<std::os::fd::RawFd>>,
    /// Number of copied-scanout render-completion readiness callbacks.
    pub scanout_render_completion_count: std::sync::atomic::AtomicU32,
    /// Stable test-owned fd inventory returned by `poll_fds`.
    poll_sources: Vec<(std::os::fd::RawFd, crate::backend::BackendFdKind)>,
    /// Optional test notification sent after a DRM fd is dispatched,
    /// allowing a test thread to wait without timing-dependent sleeps.
    page_flip_ready_tx: Option<crossbeam_channel::Sender<std::os::fd::RawFd>>,
    /// Optional notification sent after copied-scanout completion dispatch.
    scanout_render_completion_tx: Option<crossbeam_channel::Sender<()>>,
    /// Counter — incremented every time `before_block` is invoked. Tests
    /// assert the core loop drives per-iteration reclamation even when no
    /// page-flip ever occurs (project_reclamation_starvation_leak).
    pub before_block_count: std::sync::atomic::AtomicU32,
    /// Stage 4d COW: lets tests pretend this backend tracks COW
    /// lifecycle. When true, the next `release_overlay_window` call
    /// returns `Ok(true)` (final release, COW destroyed); otherwise
    /// the default `Ok(false)` no-op semantics apply. Reset to false
    /// after consumed. Plain `bool` not `AtomicBool` — `Backend`
    /// methods take `&mut self`, so the test thread already has
    /// exclusive access.
    pub cow_next_release_is_final: bool,
    /// Stage 4e COW: tracks whether `get_overlay_window` has
    /// materialised the COW (refcount > 0) so the override can
    /// signal the 0→1 transition to the core handler. Mirrors
    /// `KmsBackend`'s `core.cow_refcount`-based logic; the
    /// `RecordingBackend` doesn't own GPU storage so a plain bool
    /// suffices. Reset by `release_overlay_window` on the
    /// final-release branch (controlled by
    /// `cow_next_release_is_final`).
    pub cow_materialized: bool,
    /// Phase 2 (reparent reconciliation): lets tests opt in to
    /// claiming `supports_redirect_activation = true` so the
    /// production reconciliation block in `handle_reparent_window`
    /// (gated on the trait method) actually runs. Default `false`
    /// matches the trait default — v1 / host-X11 semantics.
    pub redirect_activation_supported: bool,
    /// KeyButMask returned by `query_pointer` (lets tests model a held
    /// pointer button — e.g. `Button1Mask = 0x0100` — so the
    /// XIQueryPointer reply's button state can be asserted).
    pub query_pointer_mask: u16,
    /// Toggled by tests that want to exercise the ynest path
    /// (kms_capable=false) — default true.
    pub dpms_capable: bool,
    /// Value returned by `glx_vendor_names()`. Defaults to the trait
    /// default (`glx::VENDOR_NAMES`, "mesa"); tests that need to prove
    /// a value actually flows through from the backend (rather than a
    /// coincidental default matching elsewhere) set it to something
    /// else first.
    pub glx_vendor_names: &'static str,
    /// When set, `set_dpms_power` returns Err; tests assert the
    /// transition helper advances state anyway.
    pub dpms_set_returns_err: bool,
    /// Result controls for `set_provider_output_source`. Successful calls
    /// return `provider_output_source_changed`; an error kind takes
    /// precedence and lets request-layer tests pin protocol error mapping.
    pub provider_output_source_changed: bool,
    pub provider_output_source_error: Option<io::ErrorKind>,
    /// Test controls and observations for asynchronous CRTC configuration.
    /// `None` preserves the synchronous `apply_crtc_config` path.
    pub pending_crtc_config: Option<CrtcConfigToken>,
    pub ready_crtc_configs: Vec<CrtcConfigToken>,
    pub crtc_config_results:
        std::collections::HashMap<CrtcConfigToken, Result<bool, io::ErrorKind>>,
    pub finished_crtc_configs: Vec<CrtcConfigToken>,
    pub cancelled_crtc_configs: Vec<CrtcConfigToken>,
    /// Startup input-probe model. Each inner `Vec` is one "dispatch
    /// round" the fake libinput would yield; `probe_input_devices`
    /// consumes the front round per iteration and seeds the registry,
    /// mirroring the KMS backend's bounded drain (stop after two
    /// consecutive empty rounds or `PROBE_MAX_ROUNDS`). Empty by
    /// default → the override is a no-op returning 0, matching the
    /// trait default for backends with no on-core libinput.
    pub probe_rounds: std::collections::VecDeque<Vec<crate::core_loop::DeviceInfo>>,
    /// Number of dispatch rounds `probe_input_devices` actually ran —
    /// lets tests assert the bounded loop terminated rather than
    /// spinning to the ceiling.
    pub probe_rounds_run: std::cell::Cell<usize>,
    /// Last `warp_pointer_root` call target; `None` if never called.
    /// Tests assert a screen shrink warps a stranded cursor into bounds.
    pub warped_to: Option<(i32, i32)>,
    /// In-memory per-RANDR-CRTC gamma LUT for unit tests (size 256).
    pub gamma: std::cell::RefCell<std::collections::HashMap<u32, GammaTriplet>>,
    /// Active RMLVO `[rules, model, layout, variant, options]` returned by
    /// `current_xkb_rules_names`. `None` (the default) models a backend
    /// without a real keymap; tests that exercise `_XKB_RULES_NAMES`
    /// publishing set it via `with_xkb_rules_names`.
    pub xkb_rules_names: Option<[String; 5]>,
    /// Return value for `set_keymap_rmlvo`. `None` (the default) models
    /// a backend without a real keymap (matches the trait default);
    /// tests that need `apply_rules_names_change` to take its recompile
    /// branch set it via `with_keymap_rmlvo_result`.
    pub keymap_rmlvo_result: Option<(u8, u8)>,
    /// Return value for `xkb_get_kbd_by_name`, seeded via
    /// `with_kbd_by_name_result`. `None` (the default) matches the trait
    /// default for backends without a real keymap.
    pub kbd_by_name_result: Option<(Vec<u8>, Option<crate::backend::XkbNewKeyboardInfo>)>,
    /// Modifier state `(effective, base, latched, locked)` returned by
    /// `current_xkb_mods`. Tests set it to drive `XkbStateNotify` emission.
    pub xkb_mods: (u8, u8, u8, u8),
    /// Configurable region returned by RENDER paint methods so core
    /// tests can assert exact damage plumbing without a real backend.
    pub render_return_region: Vec<xfixes::RegionRect>,
    /// Test controls for the asynchronous Present source-wait bridge.
    pub present_source_wait: PresentSourceWait,
    pub present_syncobj_wait: PresentSourceWait,
    pub armed_present_syncobj_waits: Vec<(u32, u32, u64)>,
    pub ready_present_source_waits: Vec<u64>,
    pub finished_present_source_waits: Vec<u64>,
    /// DRI3 fence xids passed to `dri3_trigger_fence`, in call order, so
    /// teardown/lifecycle tests can assert idle-fence release.
    pub triggered_dri3_fences: Vec<u32>,
    /// `(syncobj_xid, value)` passed to `dri3_signal_syncobj`, in call
    /// order — Task 8: lets a `PixmapSynced` supersession/copy-failure
    /// release be asserted the same way `triggered_dri3_fences` covers
    /// the `Pixmap` variant. Unlike the other Present recorders, the
    /// trait default for `dri3_signal_syncobj` returns `Err` (no real
    /// syncobj backing here), so this override also returns `Ok(())`.
    pub signalled_dri3_syncobjs: std::sync::Arc<std::sync::Mutex<Vec<(u32, u64)>>>,
    /// Identity-bearing DRI3 syncobj registry for wire-level lifetime tests.
    pub(crate) dri3_syncobj_owners: std::collections::HashMap<
        u32,
        (
            yserver_protocol::x11::ClientId,
            std::sync::Arc<RecordingSyncobjHandle>,
        ),
    >,
    /// DRI3 capability surface. Defaults to `Dri3Caps::unsupported()` and is
    /// a FIELD, not a hardcoded override: the existing
    /// `dri3_hidden_when_caps_unsupported` test (process_request.rs:37076)
    /// requires the DEFAULT RecordingBackend to stay unsupported (DRI3 absent
    /// from QueryExtension/ListExtensions). The syncobj conformance tests
    /// flip this to a (1, 4)/`syncobj: true` surface so the `IMPORT_SYNCOBJ` /
    /// `FREE_SYNCOBJ` handlers pass their `caps.syncobj` gate.
    pub(crate) dri3_caps: crate::backend::Dri3Caps,
    /// `present_id`s passed to `signal_present_wake`, in call order, so
    /// vblank-pacing tests can assert the deferred wake fired.
    pub signalled_present_wakes: Vec<u64>,
    /// Completions returned (and drained) by `drain_completed_present_events`,
    /// so tests can drive the vblank-pacing park/fire path.
    pub completed_present_events_to_drain: Vec<CompletedPresentEvent>,
    pub retired_present_idle_events_to_drain: Vec<CompletedPresentEvent>,
    /// Monotonic counter backing `pin_present_source`'s returned tokens.
    next_present_source_pin: u64,
    /// `(pin_id, host_xid)` recorded by `pin_present_source`, in call
    /// order, so entry-pin lifecycle tests (Task 5 — unified pending
    /// store) can assert a pin was taken for the right source drawable.
    pub pinned_present_sources: Vec<(u64, u32)>,
    /// `pin_id`s passed to `release_present_source`, in call order, so
    /// tests can assert a pin is released exactly once (never leaked,
    /// never double-released).
    pub released_present_sources: Vec<u64>,
    /// Canned `(msc, ust)` returned by `present_get_ust_msc` (and, absent
    /// `present_completion_clock` below, `present_get_completion_clock`
    /// too via the trait default). Default `(0, 0)` matches a backend with
    /// no real vblank clock; tests that need `fire_due_present_completions`
    /// to actually sweep (its early-return guard bails whenever
    /// `clock.msc == 0`) set this to a nonzero MSC.
    pub present_ust_msc: (u64, u64),
    /// Per-CRTC overrides for `present_ust_msc`; missing domains fall back to
    /// the legacy scalar above so existing single-domain tests stay terse.
    pub present_ust_msc_by_crtc: std::collections::HashMap<u32, (u64, u64)>,
    /// Overrides `present_get_completion_clock` independently of
    /// `present_ust_msc`. `None` (default) falls back to the trait's
    /// derive-from-`present_get_ust_msc` behavior — the two clocks read
    /// identical unless a test sets this, which is what the one-clock-
    /// contract test (Task 7 Step 4 vii) needs: the general clock
    /// (`present_ust_msc`) ahead of a deliberately stale completion clock,
    /// to prove `classify_msc_due`'s caller reads the former only.
    pub present_completion_clock: Option<crate::backend::PresentClockSample>,
    /// Per-CRTC completion-clock overrides; missing domains fall back to the
    /// legacy scalar override/general clock.
    pub present_completion_clock_by_crtc:
        std::collections::HashMap<u32, crate::backend::PresentClockSample>,
    /// Task 7: canned return for `present_flip_in_flight`. Default `false`
    /// matches the trait default.
    pub present_flip_in_flight: bool,
    pub present_flip_in_flight_by_crtc: std::collections::HashMap<u32, bool>,
    /// Task 7: canned return for `present_display_idle`. Default `true`
    /// matches the trait default.
    pub present_display_idle: bool,
    pub present_display_idle_by_crtc: std::collections::HashMap<u32, bool>,
    pub present_crtc_clock_epoch_by_crtc: std::collections::HashMap<u32, u64>,
    /// Task 7: canned return for `present_absolute_vblank_arm_supported`.
    /// Default `false` matches the trait default.
    pub present_absolute_vblank_arm_supported: bool,
    pub present_absolute_vblank_arm_supported_by_crtc: std::collections::HashMap<u32, bool>,
    /// Task 7: canned return for `present_scanout_blackout`. Default
    /// `false` matches the trait default.
    pub present_scanout_blackout: bool,
    /// Controls what `arm_present_absolute_vblank` returns. `None`
    /// (default) mimics a real always-succeeds arm: covers every target
    /// it's given. `Some(Ok(n))` / `Some(Err(kind))` let a test pin the
    /// `Ok(0)` / `Err` idle_fallback paths (spec §msc-due future-target
    /// rung 1).
    pub arm_present_absolute_vblank_result: Option<Result<usize, io::ErrorKind>>,
    /// `targets` recorded per `arm_present_absolute_vblank` call, in call
    /// order, so a test can assert exactly what was armed — the `-1` from
    /// `eff` is core-side (Task 7), so this should read `eff - 1`, never
    /// the raw `effective_target_msc`.
    pub armed_absolute_vblank_targets: Vec<Vec<u64>>,
    pub armed_absolute_vblank_crtcs: Vec<u32>,
    /// Domain-qualified calls made by the two idle-arm paths.
    pub armed_idle_vblank_targets: Vec<(u32, Vec<u64>)>,
    pub armed_completion_idle_vblank_targets: Vec<(u32, Vec<u64>)>,
    pub arm_idle_vblanks_result: Option<Result<usize, io::ErrorKind>>,
    /// Task 8 (copy-failure reroute): when `true`, `copy_area` still
    /// records the call (so a test can see it was attempted) but returns
    /// `Err` instead of `Ok(())`, driving
    /// `execute_present_pixmap_copy_or_reroute`'s failure arm. Default
    /// `false` matches today's always-succeeds behavior.
    pub fail_copy_area: bool,
    /// Canned direct-Present result plus observed candidates. A successful
    /// result lets request-layer tests prove M2b bypasses Copy entirely.
    pub present_direct_result: bool,
    pub present_direct_candidates: Vec<PresentScanoutCandidate>,
    /// Adversarial-review fix (arm-before-scrap): when `Some(kind)`,
    /// `arm_present_syncobj_wait` still records the call but returns
    /// `Err(io::Error::from(kind))` instead of `Ok(present_syncobj_wait)`,
    /// letting a test drive the client-reachable failure of a
    /// successor's arm *after* supersession has already scrapped its
    /// covered victims. Default `None` matches today's always-succeeds
    /// behavior.
    pub arm_present_syncobj_wait_result: Option<std::io::ErrorKind>,
    /// Task 10 telemetry: incremented once per `note_present_skip` call —
    /// one per pending Present scrapped by same-target supersession.
    /// Lets a test assert exactly one call per victim.
    pub present_skip_count: u32,
    /// `(device_node, change)` pairs passed to `apply_device_config`, in
    /// call order, so xinput property-write tests can assert exactly what
    /// reached the backend (the trait's default impl is a no-op and
    /// doesn't record anything).
    pub applied_device_configs: Vec<(String, crate::xinput::libinput_props::DeviceConfigChange)>,
}

impl Default for RecordingBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next_handle: Mutex::new(0x0001_0000),
            fake_window_id: 0x0000_0100,
            fake_root_visual_xid: 0x0000_0021,
            xid_map: HostXidMap::new(),
            page_flip_count: std::sync::atomic::AtomicU32::new(0),
            page_flip_fds: Mutex::new(Vec::new()),
            scanout_render_completion_count: std::sync::atomic::AtomicU32::new(0),
            poll_sources: Vec::new(),
            page_flip_ready_tx: None,
            scanout_render_completion_tx: None,
            before_block_count: std::sync::atomic::AtomicU32::new(0),
            cow_next_release_is_final: false,
            cow_materialized: false,
            redirect_activation_supported: false,
            query_pointer_mask: 0,
            dpms_capable: true,
            glx_vendor_names: glx::VENDOR_NAMES,
            dpms_set_returns_err: false,
            provider_output_source_changed: true,
            provider_output_source_error: None,
            pending_crtc_config: None,
            ready_crtc_configs: Vec::new(),
            crtc_config_results: std::collections::HashMap::new(),
            finished_crtc_configs: Vec::new(),
            cancelled_crtc_configs: Vec::new(),
            probe_rounds: std::collections::VecDeque::new(),
            probe_rounds_run: std::cell::Cell::new(0),
            warped_to: None,
            gamma: std::cell::RefCell::new(std::collections::HashMap::new()),
            xkb_rules_names: None,
            keymap_rmlvo_result: None,
            kbd_by_name_result: None,
            xkb_mods: (0, 0, 0, 0),
            render_return_region: Vec::new(),
            present_source_wait: PresentSourceWait::Ready,
            present_syncobj_wait: PresentSourceWait::Ready,
            armed_present_syncobj_waits: Vec::new(),
            ready_present_source_waits: Vec::new(),
            finished_present_source_waits: Vec::new(),
            triggered_dri3_fences: Vec::new(),
            signalled_dri3_syncobjs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            dri3_syncobj_owners: std::collections::HashMap::new(),
            dri3_caps: crate::backend::Dri3Caps::unsupported(),
            signalled_present_wakes: Vec::new(),
            completed_present_events_to_drain: Vec::new(),
            retired_present_idle_events_to_drain: Vec::new(),
            next_present_source_pin: 1,
            pinned_present_sources: Vec::new(),
            released_present_sources: Vec::new(),
            present_ust_msc: (0, 0),
            present_ust_msc_by_crtc: std::collections::HashMap::new(),
            present_completion_clock: None,
            present_completion_clock_by_crtc: std::collections::HashMap::new(),
            present_flip_in_flight: false,
            present_flip_in_flight_by_crtc: std::collections::HashMap::new(),
            present_display_idle: true,
            present_display_idle_by_crtc: std::collections::HashMap::new(),
            present_crtc_clock_epoch_by_crtc: std::collections::HashMap::new(),
            present_absolute_vblank_arm_supported: false,
            present_absolute_vblank_arm_supported_by_crtc: std::collections::HashMap::new(),
            present_scanout_blackout: false,
            arm_present_absolute_vblank_result: None,
            armed_absolute_vblank_targets: Vec::new(),
            armed_absolute_vblank_crtcs: Vec::new(),
            armed_idle_vblank_targets: Vec::new(),
            armed_completion_idle_vblank_targets: Vec::new(),
            arm_idle_vblanks_result: None,
            fail_copy_area: false,
            present_direct_result: false,
            present_direct_candidates: Vec::new(),
            arm_present_syncobj_wait_result: None,
            present_skip_count: 0,
            applied_device_configs: Vec::new(),
        }
    }

    /// Configure the backend-owned fds exposed to the core poller and a
    /// notification channel for observing DRM readiness dispatch.
    #[must_use]
    pub fn with_poll_sources(
        mut self,
        sources: Vec<(std::os::fd::RawFd, crate::backend::BackendFdKind)>,
        page_flip_ready_tx: crossbeam_channel::Sender<std::os::fd::RawFd>,
    ) -> Self {
        self.poll_sources = sources;
        self.page_flip_ready_tx = Some(page_flip_ready_tx);
        self
    }

    /// Configure a test notification for copied-scanout completion dispatch.
    #[must_use]
    pub fn with_scanout_render_completion_notification(
        mut self,
        tx: crossbeam_channel::Sender<()>,
    ) -> Self {
        self.scanout_render_completion_tx = Some(tx);
        self
    }

    /// Seed the RMLVO returned by `current_xkb_rules_names`, so a test
    /// can drive `_XKB_RULES_NAMES` publishing without a real keymap.
    #[must_use]
    pub fn with_xkb_rules_names(mut self, names: [String; 5]) -> Self {
        self.xkb_rules_names = Some(names);
        self
    }

    /// Seed the value `set_keymap_rmlvo` returns, so a test can drive
    /// `apply_rules_names_change` through its recompile branch without a
    /// real keymap backend.
    #[must_use]
    pub fn with_keymap_rmlvo_result(mut self, result: (u8, u8)) -> Self {
        self.keymap_rmlvo_result = Some(result);
        self
    }

    /// Seed the value `xkb_get_kbd_by_name` returns, so a test can drive
    /// `handle_xkb_request`'s minor==23 (XkbGetKbdByName) branch through
    /// its notify-fanout path without a real keymap backend.
    #[must_use]
    pub fn with_kbd_by_name_result(
        mut self,
        bytes: Vec<u8>,
        notify: Option<crate::backend::XkbNewKeyboardInfo>,
    ) -> Self {
        self.kbd_by_name_result = Some((bytes, notify));
        self
    }

    /// Phase 2: opt in to claiming
    /// `supports_redirect_activation = true`. Used by tests that
    /// exercise the reparent-redirect-reconciliation path
    /// (`handle_reparent_window` gates its reconciliation block
    /// on `backend.supports_redirect_activation()`).
    #[must_use]
    pub fn with_redirect_activation(mut self) -> Self {
        self.redirect_activation_supported = true;
        self
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: RecordedCall) {
        self.calls.lock().unwrap().push(call);
    }

    fn allocate_handle(&self) -> u32 {
        let mut n = self.next_handle.lock().unwrap();
        let h = *n;
        *n = n.wrapping_add(1);
        h
    }

    pub(crate) fn seed_dri3_syncobj_for_test(
        &mut self,
        xid: u32,
        owner: yserver_protocol::x11::ClientId,
    ) -> std::sync::Arc<dyn crate::backend::SyncobjHandle> {
        let handle = std::sync::Arc::new(RecordingSyncobjHandle {
            xid,
            signals: self.signalled_dri3_syncobjs.clone(),
        });
        self.dri3_syncobj_owners
            .insert(xid, (owner, handle.clone()));
        handle
    }
}

#[derive(Debug)]
pub(crate) struct RecordingSyncobjHandle {
    xid: u32,
    signals: std::sync::Arc<std::sync::Mutex<Vec<(u32, u64)>>>,
}

impl crate::backend::SyncobjHandle for RecordingSyncobjHandle {
    fn signal(&self, value: u64) -> std::io::Result<()> {
        self.signals.lock().unwrap().push((self.xid, value));
        Ok(())
    }
}

impl Backend for RecordingBackend {
    // State accessors — return fixed sentinels so the call sites that
    // need a real number get a real number; record nothing.

    fn window_id(&self) -> u32 {
        self.fake_window_id
    }

    fn root_visual_xid(&self) -> u32 {
        self.fake_root_visual_xid
    }

    fn arm_present_source_wait(
        &mut self,
        _src_pixmap_host_xid: u32,
        _dst_window_host_xid: u32,
    ) -> std::io::Result<PresentSourceWait> {
        Ok(self.present_source_wait)
    }

    fn arm_present_syncobj_wait(
        &mut self,
        src_pixmap_host_xid: u32,
        _dst_window_host_xid: u32,
        acquire_syncobj: u32,
        acquire_value: u64,
    ) -> std::io::Result<PresentSourceWait> {
        self.armed_present_syncobj_waits.push((
            src_pixmap_host_xid,
            acquire_syncobj,
            acquire_value,
        ));
        if let Some(kind) = self.arm_present_syncobj_wait_result {
            return Err(std::io::Error::from(kind));
        }
        Ok(self.present_syncobj_wait)
    }

    fn drain_ready_present_source_waits(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.ready_present_source_waits)
    }

    fn finish_present_source_wait(&mut self, wait_id: u64) {
        self.finished_present_source_waits.push(wait_id);
    }

    fn pin_present_source(&mut self, host_xid: u32) -> Option<u64> {
        let pin_id = self.next_present_source_pin;
        self.next_present_source_pin += 1;
        self.pinned_present_sources.push((pin_id, host_xid));
        Some(pin_id)
    }

    fn release_present_source(&mut self, pin_id: u64) {
        self.released_present_sources.push(pin_id);
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> std::io::Result<()> {
        self.triggered_dri3_fences.push(fence_xid);
        Ok(())
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> std::io::Result<()> {
        let handle = self
            .dri3_syncobj_owners
            .get(&syncobj_xid)
            .map(|(_, handle)| handle.clone())
            .ok_or_else(|| std::io::Error::other("unknown recording syncobj"))?;
        crate::backend::SyncobjHandle::signal(handle.as_ref(), value)
    }

    fn dri3_capabilities(&self) -> crate::backend::Dri3Caps {
        self.dri3_caps
    }

    fn dri3_import_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
        _fd: std::os::fd::OwnedFd,
    ) -> std::io::Result<()> {
        if self.dri3_syncobj_owners.contains_key(&syncobj_xid) {
            return Err(std::io::Error::other(format!(
                "DRI3 ImportSyncobj: syncobj 0x{syncobj_xid:x} already imported"
            )));
        }
        let handle = std::sync::Arc::new(RecordingSyncobjHandle {
            xid: syncobj_xid,
            signals: self.signalled_dri3_syncobjs.clone(),
        });
        self.dri3_syncobj_owners
            .insert(syncobj_xid, (client_id, handle));
        Ok(())
    }

    fn dri3_free_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> std::io::Result<()> {
        // Mirror the wire split Xorg gets from dixLookupResourceByType:
        // unknown -> BadValue (io::Error::other), not the owner -> BadAccess
        // (PermissionDenied). The handler maps the error kind to the code.
        let Some(owner) = self.dri3_syncobj_owners.get(&syncobj_xid) else {
            return Err(std::io::Error::other(format!(
                "DRI3 FreeSyncobj: unknown syncobj 0x{syncobj_xid:x}"
            )));
        };
        if owner.0 != client_id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("DRI3 FreeSyncobj: 0x{syncobj_xid:x} owned by another client"),
            ));
        }
        self.dri3_syncobj_owners.remove(&syncobj_xid);
        Ok(())
    }

    fn dri3_syncobj_handle(
        &self,
        syncobj_xid: u32,
    ) -> Option<std::sync::Arc<dyn crate::backend::SyncobjHandle>> {
        self.dri3_syncobj_owners
            .get(&syncobj_xid)
            .map(|(_, handle)| handle.clone() as std::sync::Arc<dyn crate::backend::SyncobjHandle>)
    }

    fn dri3_syncobj_owned(
        &self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> bool {
        self.dri3_syncobj_owners
            .get(&syncobj_xid)
            .is_some_and(|(owner, _)| *owner == client_id)
    }

    fn apply_device_config(
        &mut self,
        device_node: &str,
        change: crate::xinput::libinput_props::DeviceConfigChange,
    ) -> Result<(), crate::xinput::libinput_props::DeviceConfigError> {
        self.applied_device_configs
            .push((device_node.to_owned(), change));
        Ok(())
    }

    fn signal_present_wake(&mut self, present_id: u64) {
        self.signalled_present_wakes.push(present_id);
    }

    fn present_get_ust_msc(&self, crtc_id: u32) -> (u64, u64) {
        self.present_ust_msc_by_crtc
            .get(&crtc_id)
            .copied()
            .unwrap_or(self.present_ust_msc)
    }

    fn present_get_completion_clock(&self, crtc_id: u32) -> crate::backend::PresentClockSample {
        self.present_completion_clock_by_crtc
            .get(&crtc_id)
            .copied()
            .or(self.present_completion_clock)
            .unwrap_or_else(|| {
                let (msc, ust) = self.present_get_ust_msc(crtc_id);
                crate::backend::PresentClockSample {
                    msc,
                    ust,
                    source: crate::backend::PresentClockSource::BackendVblank,
                }
            })
    }

    fn present_flip_in_flight(&self, crtc_id: u32) -> bool {
        self.present_flip_in_flight_by_crtc
            .get(&crtc_id)
            .copied()
            .unwrap_or(self.present_flip_in_flight)
    }

    fn present_display_idle(&self, crtc_id: u32) -> bool {
        self.present_display_idle_by_crtc
            .get(&crtc_id)
            .copied()
            .unwrap_or(self.present_display_idle)
    }

    fn present_crtc_clock_epoch(&self, crtc_id: u32) -> u64 {
        self.present_crtc_clock_epoch_by_crtc
            .get(&crtc_id)
            .copied()
            .unwrap_or(0)
    }

    fn present_absolute_vblank_arm_supported(&self, crtc_id: u32) -> bool {
        self.present_absolute_vblank_arm_supported_by_crtc
            .get(&crtc_id)
            .copied()
            .unwrap_or(self.present_absolute_vblank_arm_supported)
    }

    fn arm_present_absolute_vblank(&mut self, crtc_id: u32, targets: &[u64]) -> io::Result<usize> {
        self.armed_absolute_vblank_crtcs.push(crtc_id);
        self.armed_absolute_vblank_targets.push(targets.to_vec());
        match self.arm_present_absolute_vblank_result {
            None => Ok(targets.len()),
            Some(Ok(n)) => Ok(n),
            Some(Err(kind)) => Err(io::Error::from(kind)),
        }
    }

    fn present_scanout_blackout(&self) -> bool {
        self.present_scanout_blackout
    }

    fn drain_completed_present_events(&mut self) -> Vec<CompletedPresentEvent> {
        self.record(RecordedCall::DrainCompletedPresentEvents);
        std::mem::take(&mut self.completed_present_events_to_drain)
    }

    fn drain_retired_present_idle_events(&mut self) -> Vec<CompletedPresentEvent> {
        std::mem::take(&mut self.retired_present_idle_events_to_drain)
    }

    fn mark_dirty(&mut self) {
        self.record(RecordedCall::MarkDirty);
    }

    fn flush_before_damage_notify(&mut self) {
        self.record(RecordedCall::FlushBeforeDamageNotify);
    }

    fn note_present_skip(&mut self) {
        self.present_skip_count += 1;
    }

    fn try_present_direct(
        &mut self,
        candidate: PresentScanoutCandidate,
        _event: CompletedPresentEvent,
    ) -> io::Result<bool> {
        self.present_direct_candidates.push(candidate);
        Ok(self.present_direct_result)
    }

    fn maybe_composite(&mut self) -> io::Result<()> {
        self.record(RecordedCall::MaybeComposite);
        Ok(())
    }

    fn arm_present_completion_idle_vblanks(
        &mut self,
        crtc_id: u32,
        target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        self.armed_completion_idle_vblank_targets
            .push((crtc_id, target_mscs.to_vec()));
        self.record(RecordedCall::ArmPresentCompletionIdleVblanks);
        Ok(0)
    }

    fn arm_idle_vblanks(&mut self, crtc_id: u32, target_mscs: &[u64]) -> std::io::Result<usize> {
        self.armed_idle_vblank_targets
            .push((crtc_id, target_mscs.to_vec()));
        match self.arm_idle_vblanks_result {
            None => Ok(0),
            Some(Ok(n)) => Ok(n),
            Some(Err(kind)) => Err(std::io::Error::from(kind)),
        }
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        None
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        None
    }

    fn render_opcode(&self) -> Option<u8> {
        None
    }

    fn xkb_opcode(&self) -> Option<u8> {
        None
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        None
    }

    fn current_xkb_mods(&self) -> (u8, u8, u8, u8) {
        self.xkb_mods
    }

    fn current_xkb_rules_names(&self) -> Option<[String; 5]> {
        self.xkb_rules_names.clone()
    }

    fn set_keymap_rmlvo(
        &mut self,
        _rules: &str,
        _model: &str,
        _layout: &str,
        _variant: &str,
        _options: Option<&str>,
    ) -> Option<(u8, u8)> {
        self.keymap_rmlvo_result
    }

    fn xkb_get_kbd_by_name(
        &mut self,
        _body: &[u8],
        _intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> Option<(Vec<u8>, Option<crate::backend::XkbNewKeyboardInfo>)> {
        self.kbd_by_name_result.clone()
    }

    fn composite_opcode(&self) -> Option<u8> {
        None
    }

    fn supports_redirect_activation(&self) -> bool {
        self.redirect_activation_supported
    }

    fn render_format_for_ynest_id(&self, _ynest_fmt: u32) -> Option<u32> {
        None
    }

    fn ping(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        self.record(RecordedCall::Ping);
        Ok(())
    }

    fn on_host_input(
        &mut self,
        _state: &mut crate::server::ServerState,
        _ev: crate::core_loop::HostInputEvent,
    ) {
    }

    fn on_page_flip_ready(
        &mut self,
        _state: &mut crate::server::ServerState,
        drm_fd: std::os::fd::RawFd,
    ) {
        self.page_flip_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.page_flip_fds.lock().unwrap().push(drm_fd);
        if let Some(tx) = self.page_flip_ready_tx.as_ref() {
            let _ = tx.send(drm_fd);
        }
    }

    fn on_scanout_render_completion(&mut self, _state: &mut crate::server::ServerState) {
        self.scanout_render_completion_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(tx) = self.scanout_render_completion_tx.as_ref() {
            let _ = tx.send(());
        }
    }

    fn before_block(&mut self) {
        self.before_block_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn set_provider_output_source(
        &mut self,
        _state: &mut crate::server::ServerState,
        provider: u32,
        source_provider: Option<u32>,
    ) -> io::Result<bool> {
        self.record(RecordedCall::SetProviderOutputSource {
            provider,
            source_provider,
        });
        if let Some(kind) = self.provider_output_source_error {
            return Err(io::Error::from(kind));
        }
        Ok(self.provider_output_source_changed)
    }

    fn crtc_gamma_size(&self, _crtc: u32) -> u16 {
        256
    }

    fn apply_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        self.record(RecordedCall::ApplyCrtcConfig {
            output_id,
            connector: connector.to_string(),
            mode,
            x,
            y,
        });
        Ok(false)
    }

    fn begin_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<CrtcConfigApply> {
        let Some(token) = self.pending_crtc_config.take() else {
            return self
                .apply_crtc_config(output_id, connector, mode, x, y)
                .map(CrtcConfigApply::Applied);
        };
        self.record(RecordedCall::ApplyCrtcConfig {
            output_id,
            connector: connector.to_string(),
            mode,
            x,
            y,
        });
        Ok(CrtcConfigApply::Pending(token))
    }

    fn drain_ready_crtc_configs(&mut self) -> Vec<CrtcConfigToken> {
        std::mem::take(&mut self.ready_crtc_configs)
    }

    fn finish_crtc_config(&mut self, token: CrtcConfigToken) -> io::Result<bool> {
        self.finished_crtc_configs.push(token);
        self.crtc_config_results
            .remove(&token)
            .unwrap_or(Ok(false))
            .map_err(io::Error::from)
    }

    fn cancel_crtc_config(&mut self, token: CrtcConfigToken) {
        self.cancelled_crtc_configs.push(token);
        self.crtc_config_results.remove(&token);
    }

    fn set_crtc_gamma(
        &mut self,
        crtc: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        self.gamma
            .borrow_mut()
            .insert(crtc, (red.to_vec(), green.to_vec(), blue.to_vec()));
        Ok(())
    }

    fn get_crtc_gamma(&self, crtc: u32) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        if let Some(lut) = self.gamma.borrow().get(&crtc) {
            return lut.clone();
        }
        let ramp = crate::backend::gamma::identity_ramp(256);
        (ramp.clone(), ramp.clone(), ramp)
    }

    fn probe_input_devices(&mut self, state: &mut crate::server::ServerState) -> usize {
        // Mirror the KMS backend's bounded-drain contract over the
        // test-configured `probe_rounds`: at most `PROBE_MAX_ROUNDS`
        // iterations, stop after two consecutive empty rounds, never
        // block. With no rounds configured this returns 0 immediately,
        // matching the trait default for a backend with no on-core
        // libinput.
        const PROBE_MAX_ROUNDS: usize = 8;
        let mut seeded = 0usize;
        let mut empty_rounds = 0usize;
        let mut rounds_run = 0usize;
        for _ in 0..PROBE_MAX_ROUNDS {
            rounds_run += 1;
            let batch = self.probe_rounds.pop_front().unwrap_or_default();
            if batch.is_empty() {
                empty_rounds += 1;
                if empty_rounds >= 2 {
                    break;
                }
                continue;
            }
            empty_rounds = 0;
            for info in batch {
                seeded += 1;
                state.xi_seed_touchpad(&info);
            }
        }
        self.probe_rounds_run.set(rounds_run);
        seeded
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, crate::backend::BackendFdKind)> {
        self.poll_sources.clone()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    // Subwindow lifecycle

    fn create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        _visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::CreateSubwindow {
            parent: host_parent.as_raw(),
            x,
            y,
            width,
            height,
            border_width,
            background_pixel,
            background_pixmap,
        });
        Ok(WindowHandle::from_raw_panicking(xid))
    }

    fn destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::DestroySubwindow(host_xid));
        Ok(())
    }

    fn map_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::MapSubwindow(host_xid));
        Ok(())
    }

    fn unmap_subwindow(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::UnmapSubwindow(host_xid));
        Ok(())
    }

    fn configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        self.record(RecordedCall::ConfigureSubwindow { host_xid, config });
        Ok(())
    }

    fn reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        self.record(RecordedCall::ReparentSubwindow {
            host_xid,
            host_parent,
            x,
            y,
        });
        Ok(())
    }

    fn change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        self.record(RecordedCall::ChangeSubwindowAttributes {
            host_xid,
            value_mask,
            values: values.to_vec(),
        });
        Ok(())
    }

    fn update_host_event_mask(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()> {
        self.record(RecordedCall::UpdateHostEventMask {
            host_xid,
            mask,
            enabled,
        });
        Ok(())
    }

    fn register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        self.record(RecordedCall::RegisterTopLevel {
            nested_id,
            host_xid,
        });
        Ok(())
    }

    fn register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.xid_map.insert(host_xid, nested_id);
        self.record(RecordedCall::RegisterSubwindow {
            nested_id,
            host_xid,
        });
        Ok(())
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.xid_map.remove(&host_xid);
        self.record(RecordedCall::UnregisterHostWindow(host_xid));
    }

    fn allocate_redirected_backing(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::AllocateRedirectedBacking {
            host_window: host_window.as_raw(),
            width,
            height,
            depth,
        });
        Ok(PixmapHandle::from_raw_panicking(xid))
    }

    fn release_redirected_backing(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.record(RecordedCall::ReleaseRedirectedBacking(backing.as_raw()));
        Ok(())
    }

    fn retain_backing_storage(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.record(RecordedCall::RetainBackingStorage(backing.as_raw()));
        Ok(())
    }

    fn drop_backing_storage(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.record(RecordedCall::DropBackingStorage(backing.as_raw()));
        Ok(())
    }

    fn set_window_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetWindowSceneParticipation {
            host_window: host_window.as_raw(),
            participating,
        });
        Ok(())
    }

    fn set_backing_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetBackingSceneParticipation {
            backing: backing.as_raw(),
            participating,
        });
        Ok(())
    }

    fn xid_map(&self) -> &HostXidMap {
        &self.xid_map
    }

    fn name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        unimplemented!("RecordingBackend: name_window_pixmap not implemented for the current tests")
    }

    fn create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::CreatePixmap {
            depth,
            width,
            height,
        });
        Ok(PixmapHandle::from_raw_panicking(xid))
    }

    fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::FreePixmap(host_xid));
        Ok(())
    }

    fn open_font(
        &mut self,
        _origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        let xid = self.allocate_handle();
        self.record(RecordedCall::OpenFont(name.to_string()));
        // FontMetrics is private to the protocol crate; return a Default-ish
        // value via Default::default(). If FontMetrics has no Default we fall
        // back to a zero-initialised one in the unimplemented branch below.
        Ok((FontHandle::from_raw_panicking(xid), FontMetrics::default()))
    }

    fn close_font(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.record(RecordedCall::CloseFont(host_xid));
        Ok(())
    }

    fn create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_pixmap: PixmapHandle,
        _mask_pixmap: Option<PixmapHandle>,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
        _hot_x: u16,
        _hot_y: u16,
    ) -> io::Result<CursorHandle> {
        let xid = self.allocate_handle();
        Ok(CursorHandle::from_raw_panicking(xid))
    }

    fn create_glyph_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _source_font: FontHandle,
        _mask_font: Option<FontHandle>,
        _source_char: u16,
        _mask_char: u16,
        _fore: (u16, u16, u16),
        _back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        let xid = self.allocate_handle();
        Ok(CursorHandle::from_raw_panicking(xid))
    }

    fn recolor_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<()> {
        self.record(RecordedCall::RecolorCursor {
            host_xid,
            fore,
            back,
        });
        Ok(())
    }

    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::DefineCursor {
            host_window_xid,
            cursor_host_xid,
        });
        Ok(())
    }

    fn set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetContainerBackgroundPixel(pixel));
        Ok(())
    }

    fn set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.record(RecordedCall::SetContainerBackgroundPixmap(host_pixmap_xid));
        Ok(())
    }

    // GC state — silently no-op for tests that drive lifecycle paths.

    fn clear_clip_rectangles(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_clip_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _clip_x_origin: i16,
        _clip_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_solid(&mut self, _origin: Option<OriginContext>) -> io::Result<()> {
        Ok(())
    }

    fn set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pixmap: u32,
        _tile_x_origin: i16,
        _tile_y_origin: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_clip_state(
        &mut self,
        _origin: Option<OriginContext>,
        _clip: &ClipState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        _fill: &FillState,
    ) -> io::Result<()> {
        Ok(())
    }

    fn apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        _state: &DrawState,
    ) -> io::Result<()> {
        Ok(())
    }

    // Drawing primitives — `unimplemented!()` so a test that
    // accidentally drives a draw path will surface loudly. Add an
    // implementation when adding a draw-path test.

    fn copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.record(RecordedCall::CopyArea {
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        });
        if self.fail_copy_area {
            return Err(io::ErrorKind::Other.into());
        }
        Ok(())
    }

    fn copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        _src_host_xid: u32,
        _dst_host_xid: u32,
        _src_x: i16,
        _src_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
        _plane: u32,
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: copy_plane")
    }

    fn put_image(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _depth: u8,
        _width: u16,
        _height: u16,
        _dst_x: i16,
        _dst_y: i16,
        _data: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: put_image")
    }

    fn get_image(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _format: u8,
        _x: i16,
        _y: i16,
        _width: u16,
        _height: u16,
        _plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn poly_line(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coordinate_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_line")
    }

    fn poly_segment(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _segments: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_segment")
    }

    fn poly_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _rectangles: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_rectangle")
    }

    fn poly_arc(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _arcs: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_arc")
    }

    fn poly_point(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coordinate_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_point")
    }

    fn poly_fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _rectangles: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_fill_rectangle")
    }

    fn poly_fill_arc(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _arcs: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_fill_arc")
    }

    fn fill_poly(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _coord_mode: u8,
        _points: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: fill_poly")
    }

    fn fill_rectangle(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _x: i16,
        _y: i16,
        _width: u16,
        _height: u16,
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: fill_rectangle")
    }

    fn poly_text8(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_text8")
    }

    fn poly_text16(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: poly_text16")
    }

    fn image_text8(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _background: u32,
        _text_len: u8,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: image_text8")
    }

    fn image_text16(
        &mut self,
        _origin: Option<OriginContext>,
        _host_xid: u32,
        _foreground: u32,
        _background: u32,
        _text_len: u8,
        _body: &[u8],
    ) -> io::Result<()> {
        unimplemented!("RecordingBackend: image_text16")
    }

    // RENDER — `unimplemented!()`; render_opcode() returns None so call
    // sites fast-path out before reaching these.

    fn render_create_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_drawable: AnyHandle,
        _ynest_format: u32,
        _value_mask: u32,
        _values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_change_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_free_picture(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_create_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        Ok(None)
    }

    fn render_free_glyphset(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_add_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _body_tail: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_free_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _host_gs: u32,
        _glyph_ids: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_composite(
        &mut self,
        _origin: Option<OriginContext>,
        _op: u8,
        _host_src: u32,
        _host_mask: u32,
        _host_dst: u32,
        _src_x: i16,
        _src_y: i16,
        _mask_x: i16,
        _mask_y: i16,
        _dst_x: i16,
        _dst_y: i16,
        _width: u16,
        _height: u16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Ok(self.render_return_region.clone())
    }

    fn render_composite_glyphs(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _mask_fmt: u32,
        _host_gs: u32,
        _src_x: i16,
        _src_y: i16,
        _items: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Ok(self.render_return_region.clone())
    }

    fn render_fill_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_dst: u32,
        _op: u8,
        _color: [u8; 8],
        _rects: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_trapezoids(
        &mut self,
        _origin: Option<OriginContext>,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _traps: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Ok(self.render_return_region.clone())
    }

    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _primitives: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        Ok(self.render_return_region.clone())
    }

    fn render_create_solid_fill(
        &mut self,
        _origin: Option<OriginContext>,
        _color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_linear_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_radial_gradient(
        &mut self,
        _origin: Option<OriginContext>,
        _body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        Ok(None)
    }

    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_src_pic: PictureHandle,
        _x: u16,
        _y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        Ok(None)
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_set_picture_filter(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_set_picture_transform(
        &mut self,
        _origin: Option<OriginContext>,
        _host_pic: u32,
        _body: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_query_version(&mut self, _origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        Ok((0, 11))
    }

    fn xkb_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _body: &[u8],
        _intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        _origin: Option<OriginContext>,
        _host_cursor_xid: u32,
        _name_bytes: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    fn set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: Option<&[xfixes::RegionRect]>,
    ) -> io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::SetShapeRectangles {
                host_xid,
                kind,
                rects: rects.map(<[_]>::len),
            });
        Ok(())
    }

    fn warp_pointer(
        &mut self,
        _origin: Option<OriginContext>,
        _dst_host_xid: u32,
        _dst_x: i16,
        _dst_y: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn warp_pointer_root(&mut self, _state: &mut crate::server::ServerState, x: i32, y: i32) {
        self.warped_to = Some((x, y));
    }

    fn query_pointer(&mut self, _origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        Ok(PointerPosition {
            same_screen: true,
            win_x: 0,
            win_y: 0,
            mask: self.query_pointer_mask,
        })
    }

    fn list_fonts_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
    ) -> io::Result<Vec<u8>> {
        // 32-byte stub reply header that downstream parsers can ignore.
        Ok(vec![0u8; 32])
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        _origin: Option<OriginContext>,
        _max_names: u16,
        _pattern: &str,
        _intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>> {
        Ok(Vec::new())
    }

    fn get_atom_name(
        &mut self,
        _origin: Option<OriginContext>,
        _atom: u32,
    ) -> io::Result<Option<String>> {
        Ok(None)
    }

    fn get_keyboard_mapping(
        &mut self,
        _origin: Option<OriginContext>,
        _first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        // Two keysyms per code, all set to NoSymbol.
        Ok((2, vec![0; usize::from(count) * 2]))
    }

    fn get_modifier_mapping(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<(u8, Vec<u8>)> {
        Ok((0, Vec::new()))
    }

    /// Stage 4e COW: override to model the 0→1 transition so the
    /// core handler can drive `materialize_cow_resource`. Returns
    /// `Ok(true)` on first claim (cow_materialized was false),
    /// `Ok(false)` on subsequent claims. Mirrors `KmsBackend`'s
    /// semantics — single backend hook owns the full COW lifecycle.
    fn get_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        if self.cow_materialized {
            return Ok(false);
        }
        self.cow_materialized = true;
        Ok(true)
    }

    /// Stage 4e COW: return the well-known COW host xid while
    /// materialised. The core handler reads this to populate the
    /// resources COW record's `host_xid` after `get_overlay_window`'s
    /// 0→1 return.
    fn cow_host_xid(&self) -> Option<u32> {
        if self.cow_materialized {
            Some(crate::resources::COMPOSITE_OVERLAY_WINDOW.0)
        } else {
            None
        }
    }

    /// Stage 4d COW: override only to honor the
    /// `cow_next_release_is_final` knob set by tests. Default trait
    /// impl returns `Ok(false)` ("I didn't destroy anything"); tests
    /// that exercise the handler-side teardown path flip the knob
    /// first. On final release also clears `cow_materialized` so
    /// `cow_host_xid` reverts to `None` and the next
    /// `get_overlay_window` re-signals a 0→1 transition.
    fn release_overlay_window(&mut self, _origin: Option<OriginContext>) -> io::Result<bool> {
        let final_release = self.cow_next_release_is_final;
        self.cow_next_release_is_final = false;
        if final_release {
            self.cow_materialized = false;
        }
        Ok(final_release)
    }

    fn dpms_capable(&self) -> bool {
        // Test default: pretend we can drive DPMS so tests can
        // exercise the wake/transition path. Individual tests
        // override by mutating a field on the backend if they need
        // the ynest path.
        self.dpms_capable
    }

    fn glx_vendor_names(&self) -> &'static str {
        self.glx_vendor_names
    }

    fn set_dpms_power(&mut self, level: u8) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::SetDpmsPower(level));
        if self.dpms_set_returns_err {
            Err(std::io::Error::other("test-injected dpms error"))
        } else {
            Ok(())
        }
    }

    fn acquire_glx_pixmap_export(&mut self, host_xid: u32) {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::AcquireGlxPixmapExport(host_xid));
    }

    fn release_glx_pixmap_export(&mut self, host_xid: u32) {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::ReleaseGlxPixmapExport(host_xid));
    }

    fn promote_pixmap_exportable(&mut self, host_xid: u32) -> bool {
        self.calls
            .lock()
            .unwrap()
            .push(RecordedCall::PromotePixmapExportable(host_xid));
        // RecordingBackend has no real GPU storage; report not-exportable.
        // Tests assert on the recorded call, not the return value.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dyn-coercion smoke test: confirm the recorder can be driven
    /// through `&mut dyn Backend` — exactly the ownership shape the core
    /// loop uses (`run_core` owns one backend by `&mut dyn Backend` on
    /// the single core-loop thread). Existence proof that the trait carve
    /// works for non-HostX11 impls.
    #[test]
    fn recording_backend_is_dyn_safe() {
        let mut rec = RecordingBackend::new();
        // Drive a few methods through the dyn pointer to confirm vtable
        // dispatch works at runtime.
        let g: &mut dyn Backend = &mut rec;
        let parent = WindowHandle::from_raw_panicking(g.window_id());
        let child = g
            .create_subwindow(
                None,
                parent,
                10,
                20,
                100,
                80,
                0,
                HostSubwindowVisual::CopyFromParent,
                None,
                None,
            )
            .unwrap();
        g.map_subwindow(None, child.as_raw()).unwrap();
        g.unmap_subwindow(None, child.as_raw()).unwrap();
        g.destroy_subwindow(None, child.as_raw()).unwrap();
    }

    #[test]
    fn recording_backend_records_basic_lifecycle() {
        let mut rec = RecordingBackend::new();
        let parent = WindowHandle::from_raw_panicking(rec.window_id());
        let a = rec
            .create_subwindow(
                None,
                parent,
                0,
                0,
                50,
                50,
                0,
                HostSubwindowVisual::CopyFromParent,
                None,
                None,
            )
            .unwrap();
        let b = rec
            .create_subwindow(
                None,
                parent,
                0,
                0,
                30,
                30,
                1,
                HostSubwindowVisual::CopyFromParent,
                Some(0xff0000),
                None,
            )
            .unwrap();
        rec.map_subwindow(None, a.as_raw()).unwrap();
        rec.map_subwindow(None, b.as_raw()).unwrap();
        rec.destroy_subwindow(None, a.as_raw()).unwrap();

        assert_ne!(a.as_raw(), b.as_raw(), "fresh handles each create");
        let calls = rec.calls();
        assert_eq!(calls.len(), 5, "5 calls recorded, got {calls:#?}");
        assert!(matches!(
            calls[0],
            RecordedCall::CreateSubwindow {
                width: 50,
                height: 50,
                ..
            }
        ));
        assert!(matches!(
            calls[1],
            RecordedCall::CreateSubwindow {
                background_pixel: Some(0xff0000),
                ..
            }
        ));
        assert!(matches!(calls[2], RecordedCall::MapSubwindow(_)));
        assert!(matches!(calls[3], RecordedCall::MapSubwindow(_)));
        assert!(matches!(calls[4], RecordedCall::DestroySubwindow(_)));
    }

    /// Phase 6.3 Step 4: `register_top_level` records the call AND
    /// inserts into the shared `xid_map` so the dispatcher's sink
    /// sees the new mapping. Replicates the contract `nested::run`
    /// relies on after the merge.
    #[test]
    fn register_top_level_updates_xid_map_and_records() {
        let mut rec = RecordingBackend::new();
        let nested_id = ResourceId(0x100);
        let host_xid = 0xdead_beef;
        rec.register_top_level(None, nested_id, host_xid)
            .expect("register_top_level");
        // xid_map sees the new entry.
        let map = rec.xid_map();
        assert_eq!(map.get(&host_xid).copied(), Some(nested_id));
        // Call is recorded with the same nested_id / host_xid.
        let calls = rec.calls();
        assert!(matches!(
            calls.last().unwrap(),
            RecordedCall::RegisterTopLevel {
                nested_id: r,
                host_xid: h
            } if *r == nested_id && *h == host_xid
        ));
    }

    /// Same shape for sub-windows — separate call variant so tests
    /// can distinguish the top-level vs sub-window path.
    #[test]
    fn register_subwindow_updates_xid_map_and_records() {
        let mut rec = RecordingBackend::new();
        let nested_id = ResourceId(0x200);
        let host_xid = 0xc0ff_eecc;
        rec.register_subwindow(None, nested_id, host_xid)
            .expect("register_subwindow");
        let map = rec.xid_map();
        assert_eq!(map.get(&host_xid).copied(), Some(nested_id));
        let calls = rec.calls();
        assert!(matches!(
            calls.last().unwrap(),
            RecordedCall::RegisterSubwindow {
                nested_id: r,
                host_xid: h
            } if *r == nested_id && *h == host_xid
        ));
    }

    /// `unregister_host_window` clears the xid_map entry — stale
    /// host events on a destroyed xid never resolve to a defunct
    /// ResourceId.
    #[test]
    fn unregister_host_window_clears_xid_map_entry() {
        let mut rec = RecordingBackend::new();
        let nested_id = ResourceId(0x300);
        let host_xid = 0xfeed_face;
        rec.register_top_level(None, nested_id, host_xid).unwrap();
        rec.unregister_host_window(host_xid);
        let map = rec.xid_map();
        assert!(map.get(&host_xid).is_none());
        let calls = rec.calls();
        assert!(matches!(
            calls.last().unwrap(),
            RecordedCall::UnregisterHostWindow(h) if *h == host_xid
        ));
    }

    #[test]
    fn recording_backend_gamma_roundtrip_and_seed() {
        let mut backend = RecordingBackend::new();
        assert_eq!(backend.crtc_gamma_size(2), 256);

        let (red, green, blue) = backend.get_crtc_gamma(2);
        assert_eq!(red.len(), 256);
        assert_eq!(red[0], 0);
        assert_eq!(red[255], 65535);
        assert_eq!((green[255], blue[255]), (65535, 65535));

        let red = vec![1u16; 256];
        let green = vec![2u16; 256];
        let blue = vec![3u16; 256];
        backend
            .set_crtc_gamma(2, &red, &green, &blue)
            .expect("set_crtc_gamma");
        assert_eq!(backend.get_crtc_gamma(2), (red, green, blue));
    }
}
