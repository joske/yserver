//! The feature-off half of [`crate::core_loop::xdmcp`], selected by
//! `core_loop::mod`'s `#[cfg]`/`#[path]` pair when the `xdmcp` feature is
//! not enabled.
//!
//! Design: `docs/superpowers/specs/2026-09-15-build-features-design.md`
//! ("The `cfg` boundary"). This exists so `run.rs` needs no `cfg` at all:
//! it is the COMPLETE surface the core loop uses — the ten
//! [`XdmcpService`] methods with inert results, [`XDMCP_TOKEN`], and an
//! [`XdmcpOutcome`] that still admits both variants the loop matches on.
//!
//! [`XdmcpService`] is an empty enum: a build without the feature has no
//! way to construct one, so `build_xdmcp_service` returning `Ok(None)` is
//! not a convention but a type-level guarantee.

use std::{io, time::Instant};

use yserver_protocol::x11::ClientId;

use super::{Generation, auth::AuthState};

/// The XDMCP socket's poll token. Nothing ever registers against it in
/// this build, but `run.rs` still compares it and the token numbering
/// must not shift between configurations. Defined once in `poll_tokens`
/// so this stub cannot drift from the real `xdmcp` module's value.
pub use super::poll_tokens::XDMCP_TOKEN;

/// What the core loop must do about the XDMCP machine's decisions.
///
/// `run.rs` matches both variants by name, so both must exist here even
/// though [`XdmcpService::take_outcome`] never yields one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XdmcpOutcome {
    /// End this generation and re-query.
    Reset,
    /// End the server.
    Terminate,
}

/// The inert stand-in for the real service. Uninhabited.
pub enum XdmcpService {}

impl XdmcpService {
    /// Registers nothing.
    pub fn register(&self, _registry: &mio::Registry) -> io::Result<()> {
        Ok(())
    }

    /// Sends no query.
    pub fn start(&mut self, _auth: &AuthState, _generation: Generation) {}

    /// Sends no re-query after a generation boundary.
    pub fn restart(&mut self, _auth: &AuthState, _generation: Generation) {}

    /// Never arms the loop's poll timeout.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        None
    }

    /// No socket, nothing to drain.
    pub fn handle_readable(&mut self, _auth: &AuthState, _generation: Generation) {}

    /// No deadline, nothing to fire.
    pub fn service_timer(&mut self, _now: Instant, _auth: &AuthState, _generation: Generation) {}

    /// **`false`**: never orphaned. `true` would mean "drop this client",
    /// which would silently disconnect every client in a minimal build.
    #[must_use]
    pub fn note_client_established(
        &mut self,
        _client: ClientId,
        _is_local: bool,
        _auth: &AuthState,
        _generation: Generation,
    ) -> bool {
        false
    }

    /// No session, so no session client.
    #[must_use]
    pub fn live_session_client(&self) -> Option<ClientId> {
        None
    }

    /// Returns `()`, like the real one — it only looks like its `bool`
    /// sibling above (`core_loop/xdmcp.rs:425`).
    pub fn note_session_client_disconnected(
        &mut self,
        _client: ClientId,
        _auth: &AuthState,
        _generation: Generation,
    ) {
    }

    /// Never decides anything.
    pub fn take_outcome(&mut self) -> Option<XdmcpOutcome> {
        None
    }
}
