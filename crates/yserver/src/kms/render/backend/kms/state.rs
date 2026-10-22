//! KMS-only backend state: direct scanout, RANDR/CRTC configuration, the
//! vblank arm tag, scanout reads and the dma-buf exports, plus
//! [`KmsBackendState`] that holds them on the backend.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    io,
    os::fd::OwnedFd,
};

use ash::vk;
use yserver_core::backend::{
    CrtcConfigToken, PixmapHandle, PresentScanoutCandidate, identity_ramp, resample_channel,
};

use crate::{
    drm::async_flip::AsyncShape,
    internal_probe::RouteProbeRequest,
    kms::{
        backend::OutputKey,
        render::{
            backend::XBackend,
            platform::{CrtcKey, PlatformBackend, QualifiedScanoutPlan},
            store::DrawableId,
            target::PaintTarget,
        },
        scanout_route::ScanoutRoute,
    },
    platform::drm::ModeIdentity,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::kms::render::backend) enum ScanoutM0Target {
    Cow,
    CowDescendant,
    Unredirected,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::kms::render::backend) enum ScanoutM0Coverage {
    Root,
    Output(usize),
    None,
}

/// Which retained direct frame a single CRTC is scanning out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::kms::render::backend) enum DirectFrameSlot {
    Pending,
    Current,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::kms::render::backend) struct ScanoutM0Shape {
    pub(in crate::kms::render::backend) target: ScanoutM0Target,
    pub(in crate::kms::render::backend) coverage: ScanoutM0Coverage,
    pub(in crate::kms::render::backend) rect: Option<(i32, i32, u32, u32)>,
    pub(in crate::kms::render::backend) source_extent: (u32, u32),
    pub(in crate::kms::render::backend) depth: u8,
    pub(in crate::kms::render::backend) bpp: u8,
    pub(in crate::kms::render::backend) imported: bool,
    pub(in crate::kms::render::backend) fourcc: u32,
    pub(in crate::kms::render::backend) vk_format: vk::Format,
    pub(in crate::kms::render::backend) modifier: u64,
    pub(in crate::kms::render::backend) plane_offset: u64,
    pub(in crate::kms::render::backend) plane_pitch: u32,
    pub(in crate::kms::render::backend) offsets: (i16, i16),
    /// Whether the Present carried a valid / update region, NOT which one.
    ///
    /// Issue #146: these were the region XIDs. A compositor creates a fresh
    /// update region every frame (`update=0x4144dd` → `0x4144eb` →
    /// `0x4144f9` in the reporter's log), so putting the XID in the dedup key
    /// made the key change on every Present by construction — and
    /// `scanout_m0 shape`, which is supposed to log only when the shape
    /// CHANGES, logged once per composited frame instead.
    ///
    /// Only the zero-ness is ever consumed: `regions_ok` tests
    /// `valid_region_xid == 0 && update_region_xid == 0 && update_is_full`.
    /// The identities are printed in the log line straight from the
    /// candidate, so nothing is lost by keeping them out of the key.
    pub(in crate::kms::render::backend) valid_region_present: bool,
    pub(in crate::kms::render::backend) update_region_present: bool,
    pub(in crate::kms::render::backend) update_is_full: bool,
}

pub(in crate::kms::render::backend) struct ScanoutM0Telemetry {
    pub(in crate::kms::render::backend) interval_start: std::time::Instant,
    pub(in crate::kms::render::backend) last_shape_by_dst: HashMap<u32, ScanoutM0Shape>,
    pub(in crate::kms::render::backend) recent_sources_by_dst:
        HashMap<u32, std::collections::VecDeque<DrawableId>>,
    pub(in crate::kms::render::backend) interval_sources: HashSet<DrawableId>,
    pub(in crate::kms::render::backend) presents: u64,
    pub(in crate::kms::render::backend) authoritative: u64,
    pub(in crate::kms::render::backend) root_coverage: u64,
    pub(in crate::kms::render::backend) output_coverage: u64,
    pub(in crate::kms::render::backend) reject_server_owned: u64,
    pub(in crate::kms::render::backend) reject_target: u64,
    pub(in crate::kms::render::backend) reject_geometry: u64,
    pub(in crate::kms::render::backend) reject_offsets: u64,
    pub(in crate::kms::render::backend) reject_regions: u64,
    pub(in crate::kms::render::backend) m1_gate_candidates: u64,
    pub(in crate::kms::render::backend) m1_gate_open: u64,
    pub(in crate::kms::render::backend) m1_gate_reject_crtc: u64,
    pub(in crate::kms::render::backend) m1_gate_reject_vt: u64,
    pub(in crate::kms::render::backend) m1_gate_reject_outputs: u64,
    pub(in crate::kms::render::backend) m1_gate_reject_cursor: u64,
    pub(in crate::kms::render::backend) m1_gate_reject_overlay: u64,
    pub(in crate::kms::render::backend) m1_gate_reject_source: u64,
    pub(in crate::kms::render::backend) m1_gate_reject_import: u64,
    pub(in crate::kms::render::backend) m1_probe_pass: u64,
    pub(in crate::kms::render::backend) m1_probe_reject: u64,
    pub(in crate::kms::render::backend) m1_probe_error: u64,
}

impl Default for ScanoutM0Telemetry {
    fn default() -> Self {
        Self {
            interval_start: std::time::Instant::now(),
            last_shape_by_dst: HashMap::new(),
            recent_sources_by_dst: HashMap::new(),
            interval_sources: HashSet::new(),
            presents: 0,
            authoritative: 0,
            root_coverage: 0,
            output_coverage: 0,
            reject_server_owned: 0,
            reject_target: 0,
            reject_geometry: 0,
            reject_offsets: 0,
            reject_regions: 0,
            m1_gate_candidates: 0,
            m1_gate_open: 0,
            m1_gate_reject_crtc: 0,
            m1_gate_reject_vt: 0,
            m1_gate_reject_outputs: 0,
            m1_gate_reject_cursor: 0,
            m1_gate_reject_overlay: 0,
            m1_gate_reject_source: 0,
            m1_gate_reject_import: 0,
            m1_probe_pass: 0,
            m1_probe_reject: 0,
            m1_probe_error: 0,
        }
    }
}

pub(in crate::kms::render::backend) struct ScanoutM1ProbeEntry {
    /// Retained solely for its FB/GEM lifetime; `Drop` performs teardown.
    pub(in crate::kms::render::backend) _framebuffer:
        Option<crate::drm::modeset::DirectScanoutProbeFramebuffer>,
    /// Exact source allocation shape used by the async suspension ledger.
    pub(in crate::kms::render::backend) async_shape: Option<AsyncShape>,
}

impl ScanoutM1ProbeEntry {
    pub(in crate::kms::render::backend) fn rejected() -> Self {
        Self {
            _framebuffer: None,
            async_shape: None,
        }
    }

    #[cfg(test)]
    pub(in crate::kms::render::backend) fn accepted(
        framebuffer: crate::drm::modeset::DirectScanoutProbeFramebuffer,
    ) -> Self {
        Self {
            _framebuffer: Some(framebuffer),
            async_shape: None,
        }
    }

    pub(in crate::kms::render::backend) fn accepted_with_shape(
        framebuffer: crate::drm::modeset::DirectScanoutProbeFramebuffer,
        async_shape: AsyncShape,
    ) -> Self {
        Self {
            _framebuffer: Some(framebuffer),
            async_shape: Some(async_shape),
        }
    }

    pub(in crate::kms::render::backend) fn framebuffer(
        &self,
    ) -> Option<&crate::drm::modeset::DirectScanoutProbeFramebuffer> {
        self._framebuffer.as_ref()
    }
}

pub(in crate::kms::render::backend) struct ScanoutM1ProbeCache {
    pub(in crate::kms::render::backend) topology_signature: u64,
    pub(in crate::kms::render::backend) entries: HashMap<DrawableId, ScanoutM1ProbeEntry>,
}

impl ScanoutM1ProbeCache {
    pub(in crate::kms::render::backend) fn new() -> Self {
        Self {
            topology_signature: 0,
            entries: HashMap::new(),
        }
    }

    pub(in crate::kms::render::backend) fn remove(&mut self, id: DrawableId) {
        self.entries.remove(&id);
    }

    pub(in crate::kms::render::backend) fn clear(&mut self, reason: &'static str) {
        if !self.entries.is_empty() {
            log::debug!(
                "scanout_m1: dropping {} cached probe framebuffer(s): {reason}",
                self.entries.len()
            );
            self.entries.clear();
        }
    }
}

pub(in crate::kms::render::backend) struct DirectPresentFrame {
    pub(in crate::kms::render::backend) source_pin: u64,
    pub(in crate::kms::render::backend) fallback_target_pin: u64,
    pub(in crate::kms::render::backend) source_id: DrawableId,
    pub(in crate::kms::render::backend) candidate: PresentScanoutCandidate,
    /// Backend receipt time retained across the latest-wins direct successor
    /// slot until its atomic submit.
    pub(in crate::kms::render::backend) present_received_at: std::time::Instant,
    /// Receipt timestamp in CLOCK_MONOTONIC's domain, shared with DRM UST.
    pub(in crate::kms::render::backend) present_received_monotonic: Option<std::time::Duration>,
    pub(in crate::kms::render::backend) fallback_target: PaintTarget,
    pub(in crate::kms::render::backend) event: yserver_core::backend::CompletedPresentEvent,
    /// Output whose CRTC domain owns CompleteNotify/MSC for this Present.
    pub(in crate::kms::render::backend) completion_output_idx: usize,
    /// Exact pageflip sample from the selected/reference CRTC. Xorg waits for
    /// every grouped CRTC to retire before completing, but stamps the event
    /// from this reference sample rather than the last/max output.
    pub(in crate::kms::render::backend) completion_clock:
        Option<yserver_core::backend::PresentClockSample>,
    pub(in crate::kms::render::backend) awaiting_outputs: HashSet<usize>,
    /// Commit accepted by the live grouped direct submission. Test-only
    /// directly constructed frames have no event identity.
    pub(in crate::kms::render::backend) accepted_event_id: Option<u64>,
    pub(in crate::kms::render::backend) async_outcome: crate::drm::async_flip::AsyncOutcome,
}

pub(in crate::kms::render::backend) type DirectFrameSubmitter =
    fn(
        &mut KmsBackend,
        &mut DirectPresentFrame,
        crate::drm::async_flip::FlipTiming,
    ) -> io::Result<crate::drm::async_flip::AsyncOutcome>;

/// Require a short stable run before entering direct ownership. Compositors
/// such as E27 alternate full-root Presents with authoritative region-limited
/// Presents; entering on every eligible member of that stream otherwise
/// causes direct/composed atomic thrash.
pub(in crate::kms::render::backend) const SCANOUT_M2_ELIGIBLE_ROOT_PROBATION: u8 = 8;

pub(in crate::kms::render::backend) struct ScanoutM2State {
    pub(in crate::kms::render::backend) pending: Option<DirectPresentFrame>,
    /// One eligible direct successor, retained while `pending` owns the
    /// hardware transaction. Newer successors replace this slot (latest
    /// wins); there is never more than one not-yet-submitted direct frame.
    pub(in crate::kms::render::backend) queued_successor: Option<DirectPresentFrame>,
    pub(in crate::kms::render::backend) current: Option<DirectPresentFrame>,
    pub(in crate::kms::render::backend) completed:
        Vec<yserver_core::backend::CompletedPresentEvent>,
    /// Coalesced successors cannot overtake the in-flight predecessor's
    /// CompleteNotify. Release their storage immediately, but publish their
    /// Skip completions only when that predecessor retires.
    pub(in crate::kms::render::backend) deferred_successor_skips:
        Vec<yserver_core::backend::CompletedPresentEvent>,
    pub(in crate::kms::render::backend) idled: Vec<yserver_core::backend::CompletedPresentEvent>,
    pub(in crate::kms::render::backend) hold_direct: bool,
    pub(in crate::kms::render::backend) cursor_bound_all: bool,
    pub(in crate::kms::render::backend) unflip_requested: bool,
    /// First request in the current direct-to-composed transition. This is
    /// the causal trigger; later composite ticks must not overwrite it.
    pub(in crate::kms::render::backend) unflip_reason: Option<&'static str>,
    /// Most recent request before submission, useful for distinguishing the
    /// initiating event from the gate that finally drove the atomic unflip.
    pub(in crate::kms::render::backend) unflip_last_reason: Option<&'static str>,
    /// Outputs still showing a direct framebuffer until an accepted
    /// replacement completes. This remains populated before a degraded
    /// replacement has been accepted.
    pub(in crate::kms::render::backend) unflip_outputs_to_replace: HashSet<usize>,
    /// Accepted replacement identity for each output, when one exists.
    pub(in crate::kms::render::backend) unflip_awaiting_outputs: HashMap<usize, u64>,
    pub(in crate::kms::render::backend) reentry_blocked_until_composed: bool,
    pub(in crate::kms::render::backend) eligible_root_streak: u8,
    pub(in crate::kms::render::backend) unflip_fallback_source: Option<DrawableId>,
    pub(in crate::kms::render::backend) unflip_shadow_ready: bool,
    /// The synchronized composed unflip failed and was degraded to the
    /// per-output scene compose path. Keep direct-frame pins held until those
    /// composed flips replace every direct plane.
    pub(in crate::kms::render::backend) degraded_composed_unflip: bool,
    /// The grouped unflip carried freshly composed scene frames; the scene
    /// retires its own BOs on the accepted replacement events.
    pub(in crate::kms::render::backend) fresh_composed_unflip: bool,
}

impl ScanoutM2State {
    pub(in crate::kms::render::backend) fn new() -> Self {
        Self {
            pending: None,
            queued_successor: None,
            current: None,
            completed: Vec::new(),
            deferred_successor_skips: Vec::new(),
            idled: Vec::new(),
            hold_direct: false,
            cursor_bound_all: false,
            unflip_requested: false,
            unflip_reason: None,
            unflip_last_reason: None,
            unflip_outputs_to_replace: HashSet::new(),
            unflip_awaiting_outputs: HashMap::new(),
            reentry_blocked_until_composed: false,
            eligible_root_streak: 0,
            unflip_fallback_source: None,
            unflip_shadow_ready: false,
            degraded_composed_unflip: false,
            fresh_composed_unflip: false,
        }
    }

    pub(in crate::kms::render::backend) fn active(&self) -> bool {
        self.pending.is_some() || self.queued_successor.is_some() || self.current.is_some()
    }

    pub(in crate::kms::render::backend) fn admit_eligible_root(&mut self) -> bool {
        if self.active() {
            return true;
        }
        self.eligible_root_streak = self
            .eligible_root_streak
            .saturating_add(1)
            .min(SCANOUT_M2_ELIGIBLE_ROOT_PROBATION);
        self.eligible_root_streak >= SCANOUT_M2_ELIGIBLE_ROOT_PROBATION
    }

    pub(in crate::kms::render::backend) fn reset_eligible_root_probation(&mut self) {
        self.eligible_root_streak = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::kms::render::backend) enum TearingState {
    Off,
    On {
        window: u32,
        host_window: u32,
        crtc_epoch: u64,
        admission: TearingAdmissionContext,
    },
}

/// Facts from the last Present admitted to tearing. Scene-only passes reuse
/// this candidate and target to revalidate ownership against the live tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::kms::render::backend) struct TearingAdmissionContext {
    pub(in crate::kms::render::backend) candidate: PresentScanoutCandidate,
    pub(in crate::kms::render::backend) target: ScanoutM0Target,
    pub(in crate::kms::render::backend) root_host_xid: u32,
}

pub(in crate::kms::render::backend) struct Tearing {
    pub(in crate::kms::render::backend) state: TearingState,
    pub(in crate::kms::render::backend) probation: u8,
    pub(in crate::kms::render::backend) probation_owner: Option<u32>,
    pub(in crate::kms::render::backend) hide_failed_logged: bool,
    pub(in crate::kms::render::backend) last_cursor_compose: Option<std::time::Instant>,
    pub(in crate::kms::render::backend) deferred_cursor_wake: Option<std::time::Instant>,
}

impl Tearing {
    pub(in crate::kms::render::backend) fn new() -> Self {
        Self {
            state: TearingState::Off,
            probation: 0,
            probation_owner: None,
            hide_failed_logged: false,
            last_cursor_compose: None,
            deferred_cursor_wake: None,
        }
    }

    pub(in crate::kms::render::backend) fn is_on(&self) -> bool {
        matches!(self.state, TearingState::On { .. })
    }

    pub(in crate::kms::render::backend) fn reset_probation(&mut self) {
        self.probation = 0;
        self.probation_owner = None;
    }

    pub(in crate::kms::render::backend) fn note_composed_submit(
        &mut self,
        now: std::time::Instant,
    ) {
        if self.is_on() {
            self.last_cursor_compose = Some(now);
            self.deferred_cursor_wake = None;
        }
    }

    pub(in crate::kms::render::backend) fn cursor_compose_interval(
        &self,
        platform: &PlatformBackend,
    ) -> std::time::Duration {
        let refresh_hz = platform
            .outputs
            .iter()
            .map(|output| output.output.picked.vrefresh)
            .filter(|refresh_hz| *refresh_hz > 0)
            .max()
            .unwrap_or(60);
        std::time::Duration::from_nanos(1_000_000_000 / u64::from(refresh_hz))
    }
}

pub(in crate::kms::render::backend) const TEARING_ENTRY_PROBATION: u8 =
    SCANOUT_M2_ELIGIBLE_ROOT_PROBATION;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::kms::render::backend) struct ScanoutM1OutputGeometry {
    pub(in crate::kms::render::backend) x: i32,
    pub(in crate::kms::render::backend) y: i32,
    pub(in crate::kms::render::backend) width: u32,
    pub(in crate::kms::render::backend) height: u32,
    pub(in crate::kms::render::backend) mode_width: u32,
    pub(in crate::kms::render::backend) mode_height: u32,
}

/// Monotonic device-qualified RANDR connector registry.
///
/// Authoritative store for every connector yserver has ever seen:
/// stable ids, connection state, current config, the persistent
/// `client_configured` bit, and the last-known advertised mode list.
/// The rescan/resume layout-preservation rule and `apply_crtc_config`
/// key off this; the core only sees the resulting `Vec<RandrOutput>`.
#[derive(Debug, Default)]
pub(crate) struct RandrIdAllocator {
    pub(in crate::kms::render::backend) next: u32,
    pub(in crate::kms::render::backend) providers: HashMap<RandrProviderEndpoint, u32>,
    pub(in crate::kms::render::backend) connectors: HashMap<OutputKey, ConnectorEntry>,
    pub(in crate::kms::render::backend) modes: HashMap<ModeIdentity, u32>,
}

/// One endpoint that can own a RANDR provider XID.
///
/// Keep the variants tagged even though both verified renderer identities and
/// KMS identities ultimately contain DRM major/minor pairs. A render node and
/// a primary node with the same raw numbers are different endpoint kinds and
/// must never alias by accident. Same-device provider coalescing is an
/// explicit projection decision based on Vulkan's advertised primary node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::kms::render::backend) enum RandrProviderEndpoint {
    Kms(crate::platform::drm::DrmDeviceKey),
    Render(crate::kms::render::platform::RenderDeviceId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectorIds {
    pub output_id: u32,
    pub crtc_id: u32,
}

/// Owned connector snapshot used while allocating mode XIDs mutably.
pub(in crate::kms::render::backend) struct NotLiveConnector {
    pub(in crate::kms::render::backend) key: OutputKey,
    pub(in crate::kms::render::backend) ids: ConnectorIds,
    pub(in crate::kms::render::backend) connected: bool,
    pub(in crate::kms::render::backend) modes: Vec<crate::platform::drm::Mode>,
    pub(in crate::kms::render::backend) edid: Vec<u8>,
    pub(in crate::kms::render::backend) mm_width: u32,
    pub(in crate::kms::render::backend) mm_height: u32,
    pub(in crate::kms::render::backend) connector_type: String,
}

pub(in crate::kms::render::backend) type ProjectedRandrOutputState =
    (bool, u32, u32, i16, i16, u16, u16);

/// Per-connector current configuration in the registry.
// Consumed by the SetCrtcConfig apply path + rescan/resume layout
// preservation (later RANDR output-management tasks); storage only here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectorConfig {
    /// Connected but not scanning out (mode=None, no CRTC).
    Off,
    /// Scanning out at `(mode_w, mode_h, vrefresh)` placed at `(x, y)`.
    Enabled {
        mode_w: u16,
        mode_h: u16,
        vrefresh: u32,
        x: i32,
        y: i32,
    },
}

/// Owned request handed to the asynchronous PRIME route qualifier.
///
/// The duplicate KMS fd keeps the exact DRM open-file description alive until
/// the worker/helper finishes, even if hotplug or backend teardown retires the
/// platform entry before this job starts. The request itself contains only
/// scalar handles and route identities; live output ownership remains in the
/// pending backend entry.
pub(crate) struct CrtcConfigProbeJob {
    pub(crate) kms_fd: OwnedFd,
    pub(crate) request: RouteProbeRequest,
}

/// One completed asynchronous qualification. A successful plan contains no
/// live Vulkan/DRM object and can therefore be rejected safely if its topology
/// or VT snapshot has gone stale.
pub(crate) struct CrtcConfigProbeCompletion {
    pub(crate) token: CrtcConfigToken,
    pub(crate) result: io::Result<QualifiedScanoutPlan>,
}

/// Injectable owner of the worker/helper transport. Production sends the owned
/// job through the process-isolated qualifier; tests use an in-memory
/// implementation without changing the core-facing token lifecycle.
pub(crate) trait CrtcConfigProbeExecutor {
    fn set_core_sender(&mut self, _sender: yserver_core::core_loop::CoreSender) {}
    /// Queue one process-helper request and return without performing the
    /// qualification or waiting for the child on the core thread.
    fn enqueue(&mut self, job: CrtcConfigProbeJob) -> io::Result<()>;
    /// Non-blockingly drain results already delivered by the helper owner.
    fn drain_ready(&mut self) -> Vec<CrtcConfigProbeCompletion>;
    /// Best-effort, non-blocking cancellation/retirement. Implementations must
    /// tolerate this racing a completed child and repeated calls.
    fn cancel(&mut self, _token: CrtcConfigToken) {}
}

pub(in crate::kms::render::backend) struct PendingCrtcConfigProbe {
    pub(in crate::kms::render::backend) output_id: u32,
    pub(in crate::kms::render::backend) output_key: OutputKey,
    pub(in crate::kms::render::backend) connector: String,
    pub(in crate::kms::render::backend) mode: yserver_core::backend::ModeSpec,
    pub(in crate::kms::render::backend) x: i32,
    pub(in crate::kms::render::backend) y: i32,
    pub(in crate::kms::render::backend) route: ScanoutRoute,
    pub(in crate::kms::render::backend) prepared_output: Option<crate::platform::drm::Output>,
    pub(in crate::kms::render::backend) topology_signature: u64,
    pub(in crate::kms::render::backend) topology_epoch: u64,
    pub(in crate::kms::render::backend) vt_state: crate::vt::state::VtState,
    pub(in crate::kms::render::backend) was_active: bool,
}

/// One connector the backend has ever seen.
// Fields consumed by later RANDR output-management tasks (reprobe,
// SetCrtcConfig apply, layout preservation); storage only here.
#[derive(Debug, Clone)]
pub(crate) struct ConnectorEntry {
    pub ids: ConnectorIds,
    pub connected: bool,
    pub config: ConnectorConfig,
    /// Whether `GetOutputInfo.crtc` retains this connector's former CRTC.
    /// Physical loss retains the association; an explicit client disable
    /// clears it. It cannot be inferred from `connected` or `config`: both
    /// physical loss and a deliberate disable leave the route Off.
    pub crtc_associated: bool,
    /// `true` once a client SetCrtcConfig/SetScreenSize touched this
    /// output. The auto-layout (recompact / boot extend-right) only
    /// ever touches `!client_configured` outputs. A disconnect retains it
    /// (the CRTC stays configured, #273); retiring a route that lost its
    /// usable modes clears it.
    pub client_configured: bool,
    /// Advertised mode list, preferred-first. Empty while disconnected
    /// (Xorg `xf86ProbeOutputModes`); a live CRTC's current mode stays in
    /// the screen mode table through the projection.
    pub modes: Vec<crate::platform::drm::Mode>,
    /// Connector identity from the latest heavy startup/hotplug/resume
    /// snapshot. Lightweight RANDR force queries never refresh these fields;
    /// they invalidate monitor-owned EDID/dimensions when connection or mode
    /// evidence says the previous identity may have departed.
    pub edid: Vec<u8>,
    pub mm_width: u32,
    pub mm_height: u32,
    pub connector_type: String,
}

#[derive(Debug, Default)]
pub(in crate::kms::render::backend) struct ConnectorRegistryDelta {
    /// Outputs whose advertised connection, modes, or monitor identity
    /// changed and therefore need OutputChangeNotify.
    pub(in crate::kms::render::backend) changed_keys: Vec<OutputKey>,
    /// Xorg's config timestamp covers available output/mode configuration,
    /// not EDID, millimeter dimensions, or connector-type metadata alone.
    pub(in crate::kms::render::backend) config_changed: bool,
}

impl ConnectorRegistryDelta {
    pub(in crate::kms::render::backend) fn is_empty(&self) -> bool {
        self.changed_keys.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::kms::render::backend) struct GammaLut {
    pub(in crate::kms::render::backend) red: Vec<u16>,
    pub(in crate::kms::render::backend) green: Vec<u16>,
    pub(in crate::kms::render::backend) blue: Vec<u16>,
}

impl GammaLut {
    pub(in crate::kms::render::backend) fn identity(size: u16) -> Self {
        let ramp = identity_ramp(size);
        Self {
            red: ramp.clone(),
            green: ramp.clone(),
            blue: ramp,
        }
    }

    pub(in crate::kms::render::backend) fn len(&self) -> usize {
        self.red.len()
    }

    pub(in crate::kms::render::backend) fn resampled(&self, size: u16) -> Self {
        let dst_len = usize::from(size);
        Self {
            red: resample_channel(&self.red, dst_len),
            green: resample_channel(&self.green, dst_len),
            blue: resample_channel(&self.blue, dst_len),
        }
    }
}

impl RandrIdAllocator {
    pub(in crate::kms::render::backend) fn fresh(&mut self) -> u32 {
        self.next = self.next.saturating_add(1);
        self.next
    }

    pub(in crate::kms::render::backend) fn provider_id_for(
        &mut self,
        endpoint: RandrProviderEndpoint,
    ) -> u32 {
        if let Some(id) = self.providers.get(&endpoint) {
            return *id;
        }
        let id = self.fresh();
        self.providers.insert(endpoint, id);
        id
    }

    /// Resolve a provider XID back to its tagged endpoint identity.
    ///
    /// The allocator is monotonic, so this may return an endpoint that was
    /// projected earlier in the process lifetime but is not part of the
    /// current provider set. Request paths must additionally validate the
    /// result against [`KmsBackend::randr_provider_endpoints`].
    pub(in crate::kms::render::backend) fn provider_endpoint_for_id(
        &self,
        provider_id: u32,
    ) -> Option<RandrProviderEndpoint> {
        self.providers
            .iter()
            .find_map(|(endpoint, id)| (*id == provider_id).then_some(*endpoint))
    }

    pub(in crate::kms::render::backend) fn ids_for(
        &mut self,
        output_key: &OutputKey,
    ) -> ConnectorIds {
        if let Some(entry) = self.connectors.get(output_key) {
            return entry.ids;
        }
        let ids = ConnectorIds {
            output_id: self.fresh(),
            crtc_id: self.fresh(),
        };
        self.connectors.insert(
            output_key.clone(),
            ConnectorEntry {
                ids,
                connected: false,
                config: ConnectorConfig::Off,
                crtc_associated: false,
                client_configured: false,
                modes: Vec::new(),
                edid: Vec::new(),
                mm_width: 0,
                mm_height: 0,
                connector_type: String::new(),
            },
        );
        ids
    }

    pub(in crate::kms::render::backend) fn mode_id(
        &mut self,
        mode: &crate::platform::drm::Mode,
    ) -> u32 {
        let identity = ModeIdentity::from(mode);
        if let Some(id) = self.modes.get(&identity) {
            return *id;
        }
        let id = self.fresh();
        self.modes.insert(identity, id);
        id
    }

    /// Output keys whose layout a client has explicitly configured
    /// (SetCrtcConfig/SetScreenSize). The auto-layout recompact must
    /// leave these where the client placed them — see Task 5.1.
    pub(in crate::kms::render::backend) fn client_configured_keys(&self) -> HashSet<OutputKey> {
        self.connectors
            .iter()
            .filter(|(_, e)| e.client_configured)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(super) fn connected_keys(&self) -> HashSet<OutputKey> {
        self.connectors
            .iter()
            .filter(|(_, entry)| entry.connected)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(in crate::kms::render::backend) fn known_connectors(
        &self,
    ) -> Vec<(OutputKey, ConnectorIds)> {
        self.connectors
            .iter()
            .map(|(key, entry)| (key.clone(), entry.ids))
            .collect()
    }

    /// Get or create the registry entry for a connector, allocating
    /// stable ids on first sight. New entries default to disconnected,
    /// Off, not-client-configured, empty mode list.
    pub(in crate::kms::render::backend) fn entry_mut(
        &mut self,
        key: &OutputKey,
    ) -> &mut ConnectorEntry {
        if !self.connectors.contains_key(key) {
            // Allocate ids (also inserts the default entry).
            let _ = self.ids_for(key);
        }
        self.connectors.get_mut(key).expect("just inserted")
    }

    #[allow(dead_code)]
    pub(crate) fn entry(&self, key: &OutputKey) -> Option<&ConnectorEntry> {
        self.connectors.get(key)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (&OutputKey, &ConnectorEntry)> {
        self.connectors.iter()
    }
}

/// The backend on DRM/KMS.
pub type KmsBackend = XBackend<PlatformBackend>;

/// Backend state owned by the KMS platform: direct scanout, vblank arming,
/// VT/session, RANDR/CRTC config, DRI3 fences and dma-buf export.
pub struct KmsBackendState {
    /// M0 direct-scanout telemetry. Purely observational: it never owns a
    /// drawable pin, submits DRM work, or affects Present capabilities.
    pub(in crate::kms::render::backend) scanout_m0: ScanoutM0Telemetry,
    pub(in crate::kms::render::backend) scanout_m1: ScanoutM1ProbeCache,
    pub(in crate::kms::render::backend) scanout_m2: ScanoutM2State,
    pub(in crate::kms::render::backend) tearing: Tearing,
    /// Monotonic clock for KMS cursor-composition pacing.
    pub(in crate::kms::render::backend) monotonic_clock:
        std::rc::Rc<dyn Fn() -> std::time::Instant>,
    pub(in crate::kms::render::backend) direct_frame_submitter: DirectFrameSubmitter,
    pub(in crate::kms::render::backend) vt_resume_executor:
        std::rc::Rc<dyn super::session::VtResumeExecutor>,

    /// Per-CRTC armed absolute MSC for idle vblank pacing. Keyed by the
    /// stable `crtc::Handle`; presence means "a `DRM_CRTC_SEQUENCE` is
    /// queued on this CRTC and we are waiting for it" (value `0` is the
    /// relative-next-vblank sentinel this spike uses). A per-CRTC map
    /// rather than a single bool so arming CRTC A can't suppress arming
    /// CRTC B (the dual-monitor permanent-stall class).
    ///
    /// **Invariant (load-bearing):** every code path that drops or
    /// completes an armed sequence MUST clear the matching entry —
    /// `on_crtc_sequence_event` (unconditional clear-arm before
    /// validating), `!scanout_allowed()` in `arm_idle_vblanks_with`,
    /// `run_suspend` / DPMS-off (master loss drops queued sequences), and
    /// output removal (`prune_armed_targets_to_live_outputs`). A stuck
    /// entry = a permanent ~0 fps stall on that CRTC.
    pub(crate) armed_vblank_targets: std::collections::HashMap<CrtcKey, u64>,

    /// Per-CRTC absolute (per-target) armed MSCs for deferred Present
    /// execution (spec §msc-due future-target rule). Keyed by raw
    /// `crtc_id` (not `Handle` — `arm_present_absolute_vblank` deals in
    /// the same `u32` ids `queue_crtc_sequence` takes). Own set,
    /// entirely independent of `armed_vblank_targets`: multiple
    /// in-flight `CRTC_QUEUE_SEQUENCE`s per CRTC are legal, and the
    /// absolute arm must neither consume nor suppress the relative-1
    /// idle arm (which a compositor parks every iteration — sharing the
    /// slot would starve one arm kind or the other). Entries retire in
    /// `on_crtc_sequence_event` when a tagged event's `sequence` reaches
    /// the target; `clear_all_armed_vblank_targets` clears this
    /// alongside `armed_vblank_targets` at the same lifecycle edges.
    pub(crate) absolute_vblank_targets:
        std::collections::HashMap<CrtcKey, std::collections::BTreeSet<u64>>,

    /// Latches true the first time `DRM_IOCTL_CRTC_QUEUE_SEQUENCE` returns
    /// EOPNOTSUPP/ENOTTY (pre-4.14 kernels lack the ioctl). Once set we stop
    /// attempting idle arming and degrade to flip-driven MSC only. Shared
    /// between the relative idle arm (`arm_idle_vblanks_ioctl`) and the
    /// absolute per-target arm (`arm_present_absolute_vblank`) — the ioctl
    /// is either supported or not, independent of which caller issues it.
    /// Logged once on transition; never resets within a process lifetime.
    pub(crate) crtc_queue_sequence_unsupported_devices: HashSet<crate::platform::drm::DrmDeviceKey>,

    /// DRI3 `FenceFromFD` xshmfence-backed fences keyed by the
    /// client's xid. Mesa's loader_dri3 uses xshmfence (memfd +
    /// futex) for idle/sync fences; the mmap'd mapping lets us
    /// `xshmfence_trigger` directly when the X side wants to
    /// signal idle. Mirrors v1's `dri3_xshmfences` field shape.
    pub(crate) dri3_xshmfences: HashMap<u32, std::sync::Arc<crate::kms::xshmfence::FenceMapping>>,
    /// DRI3 sync-fence resources keyed by the client's xid, from
    /// `FenceFromFD` falling through the xshmfence path (sync_file fd →
    /// `VkSemaphore`). Syncobjs live in `dri3_syncobjs`.
    pub(crate) dri3_sync_resources:
        HashMap<u32, std::sync::Arc<crate::kms::render::owned_semaphore::OwnedSemaphore>>,
    /// DRI3 1.4 syncobj resources keyed by the client's xid, imported by
    /// `ImportSyncobj`. The tuple's first element is the importing client —
    /// Xorg models a DRI3 syncobj as a first-class X resource owned by a
    /// client (`dri3_syncobj_type = CreateNewResourceType(...)`), and every
    /// conformance property in the spec's table falls out of that one
    /// decision: `FreeSyncobj` ownership, the disconnect purge, and the
    /// `PresentPixmapSynced` xid checks.
    pub(crate) dri3_syncobjs: HashMap<
        u32,
        (
            yserver_protocol::x11::ClientId,
            std::sync::Arc<dyn crate::kms::render::imported_syncobj::PresentSyncobj>,
        ),
    >,

    /// Result of the one-shot `DRM_IOCTL_SYNCOBJ_EVENTFD` probe: `None` until
    /// probed, then the answer for the rest of the session.
    ///
    /// FreeBSD's drm-kmod reports `DRM_CAP_SYNCOBJ_TIMELINE` (which is what
    /// `Dri3Caps::syncobj` derives from) but does not implement the eventfd
    /// ioctl, which is a much later Linux addition. Without this latch every
    /// synced Present logged a warning and retried a call that cannot
    /// succeed — a warning per frame, on a path where per-frame logging is
    /// known to perturb the timing it reports.
    pub(in crate::kms::render::backend) syncobj_eventfd_supported: Option<bool>,

    /// Latches the first "dma-buf sync-file export unsupported" warning.
    ///
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` is absent on FreeBSD's drm-kmod and
    /// these three sites run per Present: one GhostBSD/MATE session logged 412
    /// of them. Unlike the syncobj eventfd probe this latches only the LOG,
    /// not the behaviour — `ExportedSyncFile::Unsupported` conflates "no such
    /// ioctl" with "this buffer failed", and copying immediately is correct
    /// either way, so suppressing the attempt could disable a working path.
    /// `Cell` permits the source site to record it while still holding a
    /// `&self` borrow of `self.store`; the backend remains on the core thread.
    pub(in crate::kms::render::backend) dmabuf_sync_file_warned: Cell<bool>,

    /// Imported Present sources parked on their producer sync-file. The exact
    /// source `DrawableId` is incref'd while an entry lives here.
    pub(crate) pending_present_source_waits:
        HashMap<u64, crate::kms::render::present_source_wait::PendingPresentSourceWait>,
    pub(crate) next_present_source_wait_id: u64,

    // ── VT switching (Direct/VT_PROCESS). Suspend/resume state machine,
    //    driven by VtRelease/VtAcquire (SIGUSR1/2) → `drive_vt_event`.
    /// State machine for the suspend/resume cycle.
    /// Used by `scanout_allowed()` / `run_suspend`.
    pub(in crate::kms::render::backend) vt_state: crate::vt::state::VtState,
    /// Coalesced counter-events for the state machine.
    /// Consumed by `drive_vt_event`.
    pub(in crate::kms::render::backend) vt_pending: crate::vt::state::VtPending,
    /// Controlling console TTY guard. Present in direct mode when the
    /// process is launched on a real VT; `None` when no controlling
    /// console exists (pty / graphical terminal / test harness).
    /// On non-console platforms this is a unit placeholder (unused).
    #[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
    pub(in crate::kms::render::backend) console_guard: crate::kms::ConsoleGuardOpt,
    /// Whether VT switching has been armed in direct mode.
    pub(in crate::kms::render::backend) vt_switching_armed: bool,
    /// Direct-mode lock-LED sink: the input thread owns the libinput
    /// devices, so LED changes cross via this relay (mask + eventfd in
    /// the thread's epoll). `None` in test fixtures.
    pub(in crate::kms::render::backend) led_relay: Option<std::sync::Arc<crate::input::LedRelay>>,
    /// Last `input::Led` bits pushed — dedup so only lock-state
    /// TRANSITIONS reach the hardware, not every key event.
    pub(in crate::kms::render::backend) leds_sent: u32,
    /// Core-channel sender for backend-originated shutdowns. Handed in via
    /// `set_input_sender` after the channel is created in `lib.rs`.
    pub(in crate::kms::render::backend) input_sender: Option<yserver_core::core_loop::CoreSender>,
    /// Optional asynchronous cross-device scanout qualifier. Absence keeps the
    /// existing synchronous `apply_crtc_config` path unchanged.
    pub(in crate::kms::render::backend) crtc_config_probe_executor:
        Option<Box<dyn CrtcConfigProbeExecutor>>,
    pub(in crate::kms::render::backend) pending_crtc_config_probes:
        HashMap<CrtcConfigToken, PendingCrtcConfigProbe>,
    pub(in crate::kms::render::backend) ready_crtc_config_results:
        HashMap<CrtcConfigToken, io::Result<QualifiedScanoutPlan>>,
    pub(in crate::kms::render::backend) invalidated_crtc_config_probes: HashSet<CrtcConfigToken>,
    pub(in crate::kms::render::backend) ready_crtc_config_announcements: VecDeque<CrtcConfigToken>,
    pub(in crate::kms::render::backend) next_crtc_config_token: u64,
    pub(in crate::kms::render::backend) next_device_config_token: u64,
    /// Invalidates results across any topology quiesce or VT event, including
    /// an ABA transition which happens to restore the same final layout.
    pub(in crate::kms::render::backend) crtc_config_topology_epoch: u64,
    /// Test-only discovery result used to exercise the real `begin` hook with
    /// the fd-less DRM fixture. Production always performs live discovery.
    #[cfg(test)]
    pub(in crate::kms::render::backend) crtc_config_discovery_override:
        Option<crate::platform::drm::Output>,
    /// Direct-mode input-thread control channel. Used by VT release /
    /// acquire to pause and resume the dedicated input thread.
    pub(in crate::kms::render::backend) input_thread_control:
        Option<std::sync::Arc<crate::input_thread::InputThreadControl>>,
    pub(in crate::kms::render::backend) randr_id_alloc: RandrIdAllocator,
    /// Session-persistent PRIME Output Source policy, keyed by the KMS sink.
    ///
    /// Values use the provider's tagged endpoint identity rather than a DRM
    /// primary-node key: the selected renderer may be a distinct render node
    /// or the unverified Vulkan fallback, neither of which can be represented
    /// truthfully as a KMS device. Startup seeds every distinct opened KMS
    /// endpoint once; a later explicit detach removes that entry. Provider
    /// XIDs are only a projection of this policy and may be rebuilt without
    /// changing it.
    pub(in crate::kms::render::backend) provider_output_sources:
        HashMap<crate::platform::drm::DrmDeviceKey, RandrProviderEndpoint>,
    /// Per-output-id identity for RANDR output properties: `(EDID blob,
    /// ConnectorType name)`. Rebuilt by `randr_outputs_and_modes` each
    /// time RANDR state is (re)projected; read by the `EDID`/`EDID_DATA`
    /// /`ConnectorType` output-property handlers via `output_identity`.
    pub(in crate::kms::render::backend) output_identity_by_id:
        std::collections::HashMap<u32, (Vec<u8>, String)>,
    /// Resolve a RANDR output XID back to its owning DRM device and connector.
    /// Connector names are only unique within one DRM device.
    pub(in crate::kms::render::backend) output_key_by_id: std::collections::HashMap<u32, OutputKey>,
    /// RANDR CRTC XID to the owning device-qualified connector. Gamma
    /// requests address the CRTC XID, so connector names alone are ambiguous
    /// once multiple cards can both expose (for example) `DP-1`.
    pub(in crate::kms::render::backend) crtc_key_by_id: std::collections::HashMap<u32, OutputKey>,
    /// Live Present clock-domain generation per RANDR CRTC XID. The epoch is
    /// retained only while that XID resolves to the same active
    /// device-qualified CRTC. Off/removal or a KMS CRTC reassignment allocates
    /// a fresh epoch on the next live projection, letting core discard raw MSC
    /// targets captured in the old counter domain.
    pub(in crate::kms::render::backend) present_crtc_clock_epochs: HashMap<u32, (CrtcKey, u64)>,
    pub(in crate::kms::render::backend) next_present_crtc_clock_epoch: u64,
    pub(in crate::kms::render::backend) hotplug_rescan_deadline: Option<std::time::Instant>,
    pub(in crate::kms::render::backend) gamma_luts: RefCell<HashMap<OutputKey, GammaLut>>,

    /// GLX-TFP (Tasks 2.3 + 2.4): per-`DrawableId` export tracking for
    /// pixmaps shared with a GL consumer via `GLX_EXT_texture_from_pixmap`.
    /// AT MOST ONE entry per drawable regardless of how many times
    /// `BufferFromPixmap` is issued for it (muffin re-exports per damage).
    /// Canonical owner of the export lifetime ref + the dup'd dma-buf fd;
    /// a parallel sync-only fd dup lives on `store.exported_sync` so the
    /// engine's flush chokepoint can reach it.
    pub(in crate::kms::render::backend) exported_dmabufs:
        HashMap<crate::kms::render::store::DrawableId, ExportedBacking>,

    /// Cached result of `probe_dmabuf_export_support` run once at
    /// construction.  When `true`, `ServerState::glx_tfp_supported` is
    /// set and the GLX extension string advertises
    /// `GLX_EXT_texture_from_pixmap`.
    pub(in crate::kms::render::backend) dmabuf_export_supported: bool,

    /// Last `export holders` report, for change detection.
    pub(in crate::kms::render::backend) export_holders:
        crate::kms::render::export_holders::ExportHoldersReporter,
}

/// GLX-TFP export state for one drawable. See `exported_dmabufs`.
pub(in crate::kms::render::backend) struct ExportedBacking {
    /// A dup of the exported dma-buf fd, kept for implicit-sync fence
    /// I/O. `None` until the first `BufferFromPixmap` export (an entry
    /// can exist earlier, created by an `acquire_glx_pixmap_export` from
    /// `glXCreatePixmap`).
    pub(in crate::kms::render::backend) fd: Option<std::os::fd::OwnedFd>,
    /// How many live GLXPixmaps reference this backing. Teardown fires
    /// when this reaches 0.
    pub(in crate::kms::render::backend) glx_refs: u32,
    /// True once the single lifetime ref has been taken (so teardown
    /// releases it exactly once).
    pub(in crate::kms::render::backend) lifetime_ref_held: bool,
    /// The backing's `DrawableId`, captured at ref-take time so teardown
    /// can decref the store entry directly even if the `by_xid` mapping
    /// was detached by an intervening `FreePixmap` (PendingFence).
    pub(in crate::kms::render::backend) backing_id: crate::kms::render::store::DrawableId,
    /// The backing's host-xid handle, captured at ref-take time so
    /// teardown can release the alias_registry counter (keyed by xid)
    /// even after the store entry is gone.
    pub(in crate::kms::render::backend) backing: PixmapHandle,
    /// Which counter holds the lifetime ref: `true` = `alias_registry`
    /// (NameWindowPixmap'd redirect backing); `false` = `DrawableStore`
    /// refcount (plain pixmap). Captured at take time so an alias
    /// torn-down between take and release can't misroute the decref.
    pub(in crate::kms::render::backend) lifetime_via_alias: bool,
}

/// user_data tag for absolute (per-target) sequence arms. The kernel
/// event carries no CRTC; the low 32 bits stay the crtc_id (matching the
/// relative arm's `user_data = u64::from(crtc_id)`), the high bit
/// discriminates the arm kind so `on_crtc_sequence_event` can tell an
/// absolute-arm retirement apart from the single-slot relative idle arm
/// (spec round-4 F2).
pub(in crate::kms::render::backend) const ABSOLUTE_SEQ_TAG: u64 = 1 << 63;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::kms::render::backend) enum ScanoutReadSelection {
    OnScreenOnly,
    PermissiveDump,
}

/// Where an on-screen read of one rect actually has to come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::kms::render::backend) enum ScanoutReadRoute {
    /// The compositor's own scanout BO. `local` is the rect rebased into that
    /// BO's coordinate space.
    Pool {
        pool_idx: usize,
        bo_idx: usize,
        local: vk::Rect2D,
    },
    /// A transformed output's intermediate, which holds root pixels. `local`
    /// is `requested_root ∩ footprint_root − crtc_origin` (spec D6).
    Intermediate {
        output_idx: usize,
        local: vk::Rect2D,
    },
    /// A client drawable flipped directly onto this output's CRTC. `source` is
    /// the rect rebased into that drawable's own coordinate space.
    Direct {
        source_id: DrawableId,
        source_xid: u32,
        depth: u8,
        source: vk::Rect2D,
    },
}

/// Which buffer a completed read came out of. Reported so a diagnostic dump
/// can name it and a stale artifact can never be mistaken for the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::kms::render::backend) enum ScanoutReadOrigin {
    /// Degenerate read of an empty rect; no buffer was touched.
    Empty,
    ComposedPool {
        pool_idx: usize,
        bo_idx: usize,
    },
    TransformIntermediate {
        output_idx: usize,
    },
    /// The scene composed afresh for a root read (`fresh_root_readback`).
    RootReadback {
        output_idx: usize,
    },
    DirectSource {
        source_xid: u32,
    },
}

impl ScanoutReadOrigin {
    /// Short filename-safe tag naming the buffer that was read.
    pub(in crate::kms::render::backend) fn label(self) -> String {
        match self {
            Self::Empty => "empty".to_string(),
            Self::ComposedPool { pool_idx, bo_idx } => {
                format!("composed-pool{pool_idx}-bo{bo_idx}")
            }
            Self::TransformIntermediate { output_idx } => {
                format!("transform-intermediate-out{output_idx}")
            }
            Self::RootReadback { output_idx } => format!("root-readback-out{output_idx}"),
            Self::DirectSource { source_xid } => format!("direct-src-0x{source_xid:x}"),
        }
    }
}

/// Granularity of [`ensure_scanout_readback`] growth: a full-screen read
/// allocates once, and the per-row reads that follow fit.
pub(in crate::kms::render::backend) const SCANOUT_READBACK_GRANULE: u64 = 1024 * 1024;
