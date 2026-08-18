//! Backend abstraction. Currently `HostX11Backend` is the sole impl;
//! Phase 6.3+ will add a KMS backend.

pub mod gamma;
pub mod handles;
pub mod params;
mod trait_def;

#[cfg(test)]
pub mod recording;

pub use gamma::{identity_ramp, resample_channel};
pub use handles::{
    AnyHandle, ColormapHandle, CursorHandle, FontHandle, GlyphSetHandle, HandleKind, PictureHandle,
    PixmapHandle, VisualHandle, WindowHandle,
};
pub use params::{
    ArcMode, BgState, CapStyle, ClipState, DrawState, FillRule, FillState, FillStyle, GcFunction,
    JoinStyle, LineStyle, SubwindowMode,
};
pub use trait_def::{
    ActiveCursorImage, Backend, BackendFdKind, CompletedPresentEvent, CrtcConfigApply,
    CrtcConfigToken, Dri3Caps, Dri3PixmapExport, HostSocketStatus, KeymapLoad, ModeSpec,
    PresentCaps, PresentClockSample, PresentClockSource, PresentScanoutCandidate,
    PresentSourceWait, PresentWake, SyncobjHandle, XkbNewKeyboardInfo, XshmfenceHandle,
};

use yserver_protocol::x11::ClientId;

use crate::server::BackendCapabilities;

impl BackendCapabilities {
    /// Snapshot every backend-derived fact `ServerState` needs, at
    /// startup, in one place.
    ///
    /// Adding a capability here is the whole point of the type: the
    /// struct literal below fails to compile until the new field is
    /// filled, which is where that mistake should surface.
    #[must_use]
    pub fn from_backend(backend: &dyn Backend) -> Self {
        Self {
            dpms_capable: backend.dpms_capable(),
            glx_tfp_supported: backend.supports_dmabuf_export(),
            glx_vendor_names: resolve_glx_vendor_names(
                backend.glx_vendor_names(),
                std::env::var("YSERVER_GLX_VENDOR").ok().as_deref(),
            ),
        }
    }
}

/// Resolve the vendor-name list actually sent to clients.
///
/// Precedence: `YSERVER_GLX_VENDOR` > the backend's derived value.
///
/// The env value arrives as a parameter rather than being read here so
/// the accepted spellings stay testable — mutating the process
/// environment races under a parallel test runner.
///
/// Validation is limited to whitespace normalization and a blank-value
/// fallback; no name is ever rejected. A name with no matching
/// `libGLX_<name>.so` needs no server-side check: the client fails to
/// load it and libglvnd falls through to the next entry, which is the
/// intended experimental behaviour. A typo must not keep the display
/// server from starting.
fn resolve_glx_vendor_names(derived: &str, raw_env: Option<&str>) -> String {
    match raw_env {
        Some(raw) if !raw.trim().is_empty() => {
            // libglvnd's `__glXLookupVendorByScreen` splits the reply on
            // a literal single space (`strtok_r(..., " ", ...)`), so an
            // internal tab or newline would otherwise reach the client
            // as one unloadable token with no fallback entry behind it.
            // Normalizing (not rejecting) keeps this inside the
            // no-server-side-validation rule above.
            let chosen = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            log::info!("GLX vendor names overridden by YSERVER_GLX_VENDOR: {chosen}");
            chosen
        }
        Some(_) => {
            log::warn!("YSERVER_GLX_VENDOR is set but blank; using derived value {derived}");
            derived.to_string()
        }
        None => derived.to_string(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OriginContext {
    pub client_id: ClientId,
    pub nested_seq: u16,
    pub opcode: u8,
}

#[cfg(test)]
mod tests {
    use super::BackendCapabilities;
    use crate::backend::recording::RecordingBackend;

    #[test]
    fn from_backend_reads_each_capability_from_its_own_getter() {
        // RecordingBackend's two capabilities differ by default —
        // `dpms_capable()` returns true (recording.rs:1533, a test
        // default so DPMS transition tests have something to drive)
        // while `supports_dmabuf_export()` is not overridden and
        // inherits the trait default, false. That asymmetry is what
        // makes this test able to catch a crossed assignment: swapping
        // the two lines in `from_backend` flips both asserts.
        let backend = RecordingBackend::new();
        let caps = BackendCapabilities::from_backend(&backend);
        assert!(caps.dpms_capable, "must come from dpms_capable()");
        assert!(
            !caps.glx_tfp_supported,
            "must come from supports_dmabuf_export()"
        );
    }

    #[test]
    fn randr_constructors_deposit_capabilities_into_server_state() {
        use crate::server::ServerState;

        let caps = BackendCapabilities {
            dpms_capable: true,
            glx_tfp_supported: false,
            glx_vendor_names: "nvidia mesa".to_string(),
        };
        let state = ServerState::with_randr_outputs(800, 600, Vec::new(), caps);
        assert!(state.dpms.kms_capable, "dpms_capable must reach DpmsState");
        assert!(!state.glx_tfp_supported);
        assert_eq!(state.glx_vendor_names, "nvidia mesa");

        // `with_randr_outputs` forwards to `with_randr_outputs_and_modes`
        // (server.rs:1391); pin that the forward does not drop them. Inputs
        // are inverted from the case above so this call alone still proves
        // all three fields are non-default in at least one direction each.
        let caps = BackendCapabilities {
            dpms_capable: false,
            glx_tfp_supported: true,
            glx_vendor_names: "mesa".to_string(),
        };
        let direct =
            ServerState::with_randr_outputs_and_modes(800, 600, Vec::new(), Vec::new(), caps);
        assert!(!direct.dpms.kms_capable);
        assert!(direct.glx_tfp_supported);
        assert_eq!(direct.glx_vendor_names, "mesa");
    }

    #[test]
    fn resolve_prefers_env_over_derived() {
        assert_eq!(
            super::resolve_glx_vendor_names("nvidia mesa", Some("mesa")),
            "mesa"
        );
    }

    #[test]
    fn resolve_trims_env_value() {
        assert_eq!(
            super::resolve_glx_vendor_names("mesa", Some("  nvidia mesa  ")),
            "nvidia mesa"
        );
    }

    #[test]
    fn resolve_normalizes_internal_whitespace() {
        // libglvnd splits the reply on a literal single space
        // (`strtok_r(..., " ", ...)`); an internal tab or newline
        // surviving to the wire would merge into one unloadable token
        // with no fallback entry behind it.
        assert_eq!(
            super::resolve_glx_vendor_names("mesa", Some("nvidia\tmesa")),
            "nvidia mesa"
        );
    }

    #[test]
    fn resolve_falls_back_when_env_absent_or_blank() {
        // A typo must never keep the display server from starting, so
        // blank input degrades to the derived value rather than erroring.
        assert_eq!(
            super::resolve_glx_vendor_names("nvidia mesa", None),
            "nvidia mesa"
        );
        assert_eq!(
            super::resolve_glx_vendor_names("nvidia mesa", Some("")),
            "nvidia mesa"
        );
        assert_eq!(
            super::resolve_glx_vendor_names("nvidia mesa", Some("   ")),
            "nvidia mesa"
        );
    }

    #[test]
    fn from_backend_takes_vendor_names_from_the_backend() {
        // Set a value that is NOT "mesa" — the trait default, the
        // `with_geometry` default (server.rs:1363), and what a
        // hardcoded `glx::VENDOR_NAMES.to_string()` inside
        // `from_backend` would also produce. A non-default value is
        // the only thing that can catch `from_backend` never calling
        // `backend.glx_vendor_names()` at all.
        //
        // Depends on `YSERVER_GLX_VENDOR` being unset in the test
        // process's environment; if it's exported (e.g. during
        // hardware testing on NVIDIA), this assertion fails
        // spuriously because the env override wins by design.
        let mut backend = RecordingBackend::new();
        backend.glx_vendor_names = "nvidia mesa";
        let caps = BackendCapabilities::from_backend(&backend);
        assert_eq!(caps.glx_vendor_names, "nvidia mesa");
    }
}
