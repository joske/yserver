//! Deferred DRI3 exports: parked while the pixmap is scanned out, answered once the backend reports them ready.

use yserver_protocol::x11::{ClientId, SequenceNumber};

use super::{
    Backend, Dri3ExportToken, PendingBackendRequests, PendingDri3Export, RequestOutcome,
    ResetTrigger, ServerState, XiConfigLane, disconnect_with_pending_cleanup, grant_request_credit,
};

/// A DRI3 export the backend deferred while its pixmap is scanned out.
pub(super) struct ParkedDri3Export {
    pub(super) client_id: ClientId,
    pub(super) sequence: SequenceNumber,
    pub(super) continuation: PendingDri3Export,
    pub(super) request_wire_bytes: usize,
}

impl PendingBackendRequests {
    pub(super) fn park_dri3(&mut self, parked: ParkedDri3Export) -> Result<(), &'static str> {
        let client = parked.client_id;
        let token = parked.continuation.token;
        if self.crtc_by_client.contains_key(&client) || self.dri3_by_client.contains_key(&client) {
            return Err("client already has a pending backend request");
        }
        if self.dri3_by_token.contains_key(&token) {
            return Err("backend reused a live DRI3 export token");
        }
        self.dri3_by_client.insert(client, token);
        self.dri3_by_token.insert(token, parked);
        Ok(())
    }

    pub(super) fn take_dri3(&mut self, token: Dri3ExportToken) -> Option<ParkedDri3Export> {
        let parked = self.dri3_by_token.remove(&token)?;
        self.dri3_by_client.remove(&parked.client_id);
        Some(parked)
    }

    pub(super) fn take_client_dri3(&mut self, client: ClientId) -> Option<Dri3ExportToken> {
        let token = self.dri3_by_client.remove(&client)?;
        self.dri3_by_token.remove(&token);
        Some(token)
    }

    /// Whether `client` waits on a deferred DRI3 export.
    #[cfg(test)]
    pub(crate) fn dri3_export_is_parked_for_test(&self, client: ClientId) -> bool {
        self.dri3_by_client.contains_key(&client)
    }

    pub(super) fn take_all_dri3_tokens(&mut self) -> Vec<Dri3ExportToken> {
        self.dri3_by_client.clear();
        self.dri3_by_token.drain().map(|(token, _)| token).collect()
    }
}

/// Answer every deferred DRI3 export the backend reports ready, then hand
/// its token back so the backend can release the pixmap pin.
pub(super) fn drain_ready_dri3_exports(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    pending: &mut PendingBackendRequests,
    xi_config_lane: &mut XiConfigLane,
    reset_trigger: &mut ResetTrigger,
) {
    for token in backend.drain_ready_dri3_exports() {
        let Some(parked) = pending.take_dri3(token) else {
            backend.release_dri3_export(token);
            continue;
        };
        if !state.clients.contains_key(&parked.client_id.0) {
            backend.release_dri3_export(token);
            continue;
        }
        let result = backend.finish_dri3_export(token);
        let outcome = match crate::core_loop::process_request::complete_dri3_export(
            state,
            parked.client_id,
            parked.sequence,
            parked.continuation,
            result,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                log::warn!(
                    "DRI3 export completion error (client {} token {}): {err}",
                    parked.client_id.0,
                    token.0,
                );
                RequestOutcome::Handled
            }
        };
        backend.release_dri3_export(token);
        match outcome {
            RequestOutcome::Disconnect(client) => {
                disconnect_with_pending_cleanup(
                    state,
                    backend,
                    pending,
                    xi_config_lane,
                    reset_trigger,
                    client,
                );
            }
            RequestOutcome::Handled => {
                grant_request_credit(state, parked.client_id, parked.request_wire_bytes)
            }
            RequestOutcome::PendingCrtcConfig(_)
            | RequestOutcome::PendingDri3Export(_)
            | RequestOutcome::PendingXiConfig(_) => {
                unreachable!("a DRI3 export completion cannot start backend work")
            }
        }
    }
}
