use crate::kms::render::{
    backend::{
        DirectScope, KmsBackend, KmsBackendState, RotationState, SERVER_SCANOUT_FAILURE_CAP,
        SERVER_SCANOUT_LIVE_CAP, ScanoutM0Target, ScanoutSourceLayout, ServerFenceKind,
        ServerFenceTally, ServerScanoutDescriptor, ServerScanoutPromoteError,
        ServerScanoutPromotion, ServerScanoutState,
        kms::scanout::{
            DRM_FORMAT_XRGB8888, scanout_m1_outputs_cover_root, scanout_m1_plane_states,
            scanout_m1_validate,
        },
    },
    store::DrawableId,
    target::PaintTarget,
};
use ash::vk;
use std::rc::Rc;
use yserver_core::backend::PresentScanoutCandidate;

impl KmsBackendState {
    /// Ask every active direct owner to hand its outputs back to the scene.
    pub(in crate::kms::render::backend) fn request_direct_unflip(&mut self, reason: &'static str) {
        for scope in self.direct.active_scopes() {
            self.request_direct_unflip_for(scope, reason);
        }
    }

    /// Ask the owner of `scope` to hand its outputs back to the scene.
    pub(in crate::kms::render::backend) fn request_direct_unflip_for(
        &mut self,
        scope: DirectScope,
        reason: &'static str,
    ) {
        let Some(owner) = self.direct.get_mut(scope) else {
            return;
        };
        if !owner.active() {
            return;
        }
        if !owner.unflip_requested {
            owner.unflip_reason = Some(reason);
            *self
                .scanout_m0
                .server
                .unflip_reasons
                .entry(reason)
                .or_default() += 1;
        }
        owner.unflip_requested = true;
        owner.unflip_last_reason = Some(reason);
        owner.hold_direct = false;
    }

    /// The active owners whose pending, queued or current frame names `id`
    /// as its source or fallback target.
    pub(in crate::kms::render::backend) fn direct_scopes_referencing_drawable(
        &self,
        id: DrawableId,
    ) -> Vec<DirectScope> {
        self.direct
            .active_scopes()
            .into_iter()
            .filter(|&scope| {
                self.direct.get(scope).is_some_and(|owner| {
                    [
                        owner.pending.as_ref(),
                        owner.queued_successor.as_ref(),
                        owner.current.as_ref(),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|frame| frame.source_id == id || frame.fallback_target.backing_id() == id)
                })
            })
            .collect()
    }

    /// Unflip every owner that holds one of `ids`, and only those.
    pub(in crate::kms::render::backend) fn request_direct_unflip_referencing(
        &mut self,
        ids: &[DrawableId],
        reason: &'static str,
    ) {
        let mut scopes: Vec<DirectScope> = Vec::new();
        for &id in ids {
            for scope in self.direct_scopes_referencing_drawable(id) {
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
        }
        for scope in scopes {
            self.request_direct_unflip_for(scope, reason);
        }
    }

    /// Every member of a `Complete` rotation that holds `id`, and `id`.
    fn complete_rotation_members_with(&self, id: DrawableId) -> Vec<DrawableId> {
        let mut members = vec![id];
        for rotation in self.server_scanout.rotations.values() {
            if rotation.state == RotationState::Complete
                && rotation.members.iter().any(|(member, _)| *member == id)
            {
                members.extend(rotation.members.iter().map(|(member, _)| *member));
            }
        }
        members
    }

    /// A drawable is gone: drop its probe framebuffer, then its scanout
    /// descriptor (D9), and leave any `Complete` rotation it belonged to.
    pub(in crate::kms::render::backend) fn retire_scanout_drawable(&mut self, id: DrawableId) {
        self.scanout_m1.remove(id);
        let members = self.complete_rotation_members_with(id);
        if self.server_scanout.retire(id) {
            self.request_direct_unflip_referencing(&members, "server_source_retired");
        }
    }
}

impl KmsBackend {
    /// Imported sources may flip; a promoted one only while its rotation on `dest_id` is `Complete` (D1).
    pub(in crate::kms::render::backend) fn direct_source_flip_admitted(
        &self,
        source_id: DrawableId,
        dest_id: Option<DrawableId>,
    ) -> bool {
        let server = &self.pstate.server_scanout;
        !server.is_promoted(source_id)
            || dest_id.is_some_and(|dest| server.rotation_complete(dest, source_id))
    }

    /// Track an M0-observed Present of a server-owned source and promote its
    /// destination's rotation once identified (D1, D2, D3, D5, D6).
    pub(in crate::kms::render::backend) fn observe_server_scanout_candidate(
        &mut self,
        candidate: PresentScanoutCandidate,
        source_id: Option<DrawableId>,
        dest_id: Option<DrawableId>,
        paint_target: Option<PaintTarget>,
        target: ScanoutM0Target,
    ) {
        let (Some(source_id), Some(dest_id)) = (source_id, dest_id) else {
            return;
        };
        if !self.server_scanout_source_shaped(candidate, source_id, paint_target, target) {
            return;
        }
        if self.server_scanout_source_exported(source_id) {
            self.disqualify_server_scanout(source_id, "server_source_exported");
            let server = &mut self.pstate.scanout_m0.server;
            server.reject_exported = server.reject_exported.saturating_add(1);
            return;
        }
        if matches!(
            self.pstate.server_scanout.sources.get(&source_id),
            Some(ServerScanoutState::Disqualified(_))
        ) {
            let server = &mut self.pstate.scanout_m0.server;
            server.reject_exported = server.reject_exported.saturating_add(1);
            return;
        }
        let server = &mut self.pstate.scanout_m0.server;
        server.candidates = server.candidates.saturating_add(1);
        let previous_members: Vec<DrawableId> = self
            .pstate
            .server_scanout
            .rotations
            .get(&dest_id)
            .map(|rotation| rotation.members.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default();
        let observation = self.pstate.server_scanout.observe(dest_id, source_id);
        if observation.left_complete {
            self.pstate.request_direct_unflip_referencing(
                &previous_members,
                "server_source_rotation_changed",
            );
        }
        if observation.identified {
            self.promote_server_rotation(dest_id);
        }
    }

    /// Server-owned, depth-24 BGRA, root-sized, Presented whole to the COW or
    /// a COW descendant that passes the M2 gate as a `Root` candidate. A
    /// promoted source is only ever scanned out by the `Root` owner (D11).
    fn server_scanout_source_shaped(
        &self,
        candidate: PresentScanoutCandidate,
        source_id: DrawableId,
        paint_target: Option<PaintTarget>,
        target: ScanoutM0Target,
    ) -> bool {
        let Some(drawable) = self.store.get(source_id) else {
            return false;
        };
        let storage = &drawable.storage;
        let root = (u32::from(self.platform.fb_w), u32::from(self.platform.fb_h));
        let unbordered = paint_target.is_none_or(|target| !target.has_border_clip());
        storage.imported_dmabuf.is_none()
            && storage.imported_drawable.is_none()
            && (!storage.is_exportable() || self.pstate.server_scanout.is_promoted(source_id))
            && drawable.depth == 24
            && storage.format == vk::Format::B8G8R8A8_UNORM
            && (storage.extent.width, storage.extent.height) == root
            && (
                u32::from(candidate.src_width),
                u32::from(candidate.src_height),
            ) == root
            && matches!(
                target,
                ScanoutM0Target::Cow | ScanoutM0Target::CowDescendant
            )
            && self.direct_candidate_scope(candidate, target) == Some(DirectScope::Root)
            && self.direct_candidate_gate(candidate, target, DirectScope::Root, unbordered, true)
            && scanout_m1_outputs_cover_root(
                root,
                &self.scanout_m1_output_geometry(DirectScope::Root),
            )
    }

    /// Whether a client holds, or may hold, an fd of `source_id` (D6).
    fn server_scanout_source_exported(&self, source_id: DrawableId) -> bool {
        self.store.is_exported(source_id) || self.pstate.exported_dmabufs.contains_key(&source_id)
    }

    /// A promoted source is being handed to a client: never flip it again,
    /// leave any complete rotation holding it and unflip (D6). Its probe
    /// framebuffer and descriptor go once no direct frame references it.
    /// A no-op for a source that was never promoted.
    pub(in crate::kms::render::backend) fn disqualify_server_scanout(
        &mut self,
        source_id: DrawableId,
        reason: &'static str,
    ) {
        let Some(state) = self.pstate.server_scanout.sources.get_mut(&source_id) else {
            return;
        };
        let ServerScanoutState::Promoted(_) = state else {
            return;
        };
        let ServerScanoutState::Promoted(descriptor) =
            std::mem::replace(state, ServerScanoutState::Disqualified(None))
        else {
            unreachable!("matched Promoted above");
        };
        *state = ServerScanoutState::Disqualified(Some(descriptor));
        self.store.untrack_scanout_writes(source_id);
        let server = &mut self.pstate.scanout_m0.server;
        server.reject_exported = server.reject_exported.saturating_add(1);
        log::info!(
            target: "yserver::scanout_server",
            "promoted server-owned Present source drawable={} disqualified: {reason}",
            source_id.as_u64(),
        );
        // From here the storage is an external export, not scanout memory.
        if let Some(drawable) = self.store.get(source_id) {
            let memory = drawable.storage.memory;
            crate::kms::vk::mem_accounting::recategorise(
                memory,
                crate::kms::vk::mem_accounting::export_category_for(
                    crate::kms::vk::mem_accounting::category_of(memory),
                ),
            );
        }
        let held = self.pstate.complete_rotation_members_with(source_id);
        let mut left_complete = false;
        for rotation in self.pstate.server_scanout.rotations.values_mut() {
            if rotation.state == RotationState::Complete
                && rotation.members.iter().any(|(id, _)| *id == source_id)
            {
                rotation.state = RotationState::Failed;
                left_complete = true;
            }
        }
        // Unflip whichever owner shows the source, or the rotation it left.
        if left_complete {
            self.pstate.request_direct_unflip_referencing(&held, reason);
        } else {
            self.pstate
                .request_direct_unflip_referencing(&[source_id], reason);
        }
        self.release_unreferenced_server_scanout();
    }

    /// Before a direct commit of promoted `source_id`: submit its unsubmitted
    /// writes, then return the `IN_FENCE_FD` the commit carries (D8) and how
    /// it was obtained. `None` means no fence is needed, either because
    /// nothing wrote the source since promotion or because the CPU already
    /// waited. A failed CPU wait unflips and errs: the Present takes the Copy
    /// path. Nothing is counted until the frame is committed.
    pub(in crate::kms::render::backend) fn server_direct_source_fence(
        &mut self,
        source_id: DrawableId,
        scope: DirectScope,
    ) -> std::io::Result<(Option<std::os::fd::OwnedFd>, ServerFenceTally)> {
        use crate::kms::render::store::ScanoutWriteFence;

        let flush_added = self.store.has_unsubmitted_scanout_write(source_id);
        if flush_added {
            self.flush_for_direct_source()?;
        }
        let tally = |kind| ServerFenceTally { flush_added, kind };
        let fence = self.store.scanout_write_fence(source_id).cloned();
        let explicit = match fence {
            None | Some(ScanoutWriteFence::NoPendingWrite) => {
                return Ok((None, tally(ServerFenceKind::None)));
            }
            // The fence goes on every plane of the owner's commit.
            Some(ScanoutWriteFence::Fence(fd)) => self
                .direct_scope_outputs(scope)
                .iter()
                .all(|&output_idx| {
                    self.platform
                        .outputs
                        .get(output_idx)
                        .is_some_and(|output| output.output.plane_in_fence_fd_prop.is_some())
                })
                .then(|| fd.try_clone().ok())
                .flatten(),
            Some(ScanoutWriteFence::ExportFailed) => None,
        };
        if let Some(fd) = explicit {
            return Ok((Some(fd), tally(ServerFenceKind::Explicit)));
        }
        self.wait_for_direct_source_on_cpu(source_id)?;
        Ok((None, tally(ServerFenceKind::CpuWait)))
    }

    /// Count the D8 fence of a committed server-owned direct frame.
    pub(in crate::kms::render::backend) fn count_server_fence(&mut self, tally: ServerFenceTally) {
        let server = &mut self.pstate.scanout_m0.server;
        if tally.flush_added {
            server.direct_flush_added = server.direct_flush_added.saturating_add(1);
        } else {
            server.direct_flush_reused = server.direct_flush_reused.saturating_add(1);
        }
        let counter = match tally.kind {
            ServerFenceKind::None => &mut server.fence_none,
            ServerFenceKind::Explicit => &mut server.fence_explicit,
            ServerFenceKind::CpuWait => &mut server.fence_cpu_wait,
        };
        *counter = counter.saturating_add(1);
    }

    /// Submit recorded work: render batch, open frame, submit group.
    fn flush_for_direct_source(&mut self) -> std::io::Result<()> {
        self.engine
            .flush_render_batch(
                &mut self.store,
                &mut self.platform.gpu,
                crate::kms::render::engine::RenderFlushReason::Present,
            )
            .map_err(|error| std::io::Error::other(format!("direct source flush: {error:?}")))?;
        self.engine
            .close_open_frame(
                &mut self.store,
                &mut self.platform.gpu,
                crate::kms::render::frame_builder::CloseReason::PresentCompletionSignal,
            )
            .map_err(|error| std::io::Error::other(format!("direct source frame: {error:?}")))?;
        self.engine
            .flush_submit_group(
                &mut self.store,
                &mut self.platform.gpu,
                crate::kms::render::submit_group::FlushReason::PresentCompletionSignal,
            )
            .map_err(|error| std::io::Error::other(format!("direct source submit: {error:?}")))?;
        Ok(())
    }

    /// The D8 fallback: no usable sync_file or no `IN_FENCE_FD` property, so
    /// wait for the source's last render ticket, which may name a later read.
    fn wait_for_direct_source_on_cpu(&mut self, source_id: DrawableId) -> std::io::Result<()> {
        if !std::mem::replace(&mut self.pstate.server_scanout.fence_cpu_wait_warned, true) {
            log::warn!(
                target: "yserver::scanout_server",
                "direct scanout of a server-owned source waits for its writes on the CPU",
            );
        }
        let wait = self.pstate.direct_source_cpu_waiter;
        wait(self, source_id).inspect_err(|_| {
            self.pstate
                .request_direct_unflip_referencing(&[source_id], "server_source_fence_failed");
        })
    }

    /// Production [`DirectSourceCpuWaiter`](crate::kms::render::backend::DirectSourceCpuWaiter).
    pub(in crate::kms::render::backend) fn wait_for_direct_source_ticket_live(
        b: &KmsBackend,
        source_id: DrawableId,
    ) -> std::io::Result<()> {
        let ticket = b
            .store
            .get(source_id)
            .and_then(|drawable| drawable.last_render_ticket.clone());
        match (ticket, b.platform.gpu.vk()) {
            (Some(ticket), Some(vk)) => ticket.wait(vk),
            _ => Ok(()),
        }
        .map_err(|error| std::io::Error::other(format!("direct source fence wait: {error:?}")))
    }

    /// Whether any owner's direct frame (pending, queued or current) names
    /// `id` as its source or fallback target.
    pub(in crate::kms::render::backend) fn direct_frame_references_drawable(
        &self,
        id: DrawableId,
    ) -> bool {
        !self
            .pstate
            .direct_scopes_referencing_drawable(id)
            .is_empty()
    }

    /// Whether `scope`'s owner may scan `source_id`, Presented to `dest_id`,
    /// out from its cached framebuffer.
    pub(in crate::kms::render::backend) fn direct_source_ready(
        &self,
        source_id: DrawableId,
        dest_id: Option<DrawableId>,
        scope: DirectScope,
    ) -> bool {
        let has_framebuffer = self
            .pstate
            .scanout_m1
            .entries
            .get(&(source_id, scope))
            .and_then(crate::kms::render::backend::ScanoutM1ProbeEntry::framebuffer)
            .is_some();
        let imported = self.store.get(source_id).is_some_and(|drawable| {
            drawable.storage.imported_dmabuf.is_some()
                || drawable.storage.imported_drawable.is_some()
        });
        has_framebuffer
            && (imported || self.pstate.server_scanout.is_promoted(source_id))
            && self.direct_source_flip_admitted(source_id, dest_id)
    }

    /// Drop the probe framebuffer, then the descriptor, of every disqualified
    /// source no direct frame references any more (D9), and run every parked
    /// DRI3 export whose source left every plane.
    pub(in crate::kms::render::backend) fn release_unreferenced_server_scanout(&mut self) {
        let released: Vec<DrawableId> = self
            .pstate
            .server_scanout
            .sources
            .iter()
            .filter(|(id, state)| {
                matches!(state, ServerScanoutState::Disqualified(Some(_)))
                    && !self.direct_frame_references_drawable(**id)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in released {
            self.pstate.scanout_m1.remove(id);
            if let Some(ServerScanoutState::Disqualified(descriptor)) =
                self.pstate.server_scanout.sources.get_mut(&id)
            {
                drop(descriptor.take());
            }
        }
        let runnable: Vec<_> = self
            .pstate
            .server_scanout_exports
            .parked
            .iter()
            .filter(|(_, parked)| {
                parked.result.is_none() && !self.direct_frame_references_drawable(parked.id)
            })
            .map(|(token, _)| *token)
            .collect();
        for token in runnable {
            let Some(parked) = self.pstate.server_scanout_exports.parked.get(&token) else {
                continue;
            };
            let (id, host_xid, parked_at) = (parked.id, parked.host_xid, parked.parked_at);
            let exporter = self.pstate.dri3_drawable_exporter;
            let result = exporter(self, id, host_xid);
            let waited_us = u64::try_from(
                self.monotonic_now()
                    .saturating_duration_since(parked_at)
                    .as_micros(),
            )
            .unwrap_or(u64::MAX);
            let server = &mut self.pstate.scanout_m0.server;
            server.export_defer_us_max = server.export_defer_us_max.max(waited_us);
            self.complete_server_scanout_export(token, result);
        }
    }

    fn complete_server_scanout_export(
        &mut self,
        token: yserver_core::backend::Dri3ExportToken,
        result: std::io::Result<yserver_core::backend::Dri3PixmapExport>,
    ) {
        let exports = &mut self.pstate.server_scanout_exports;
        if let Some(parked) = exports.parked.get_mut(&token) {
            parked.result = Some(result);
            exports.ready.push(token);
        }
    }

    /// The DRI3 export of `host_xid`; deferred while a promoted source is
    /// still referenced by a direct frame (D6).
    pub(in crate::kms::render::backend) fn backend_export_dri3_begin_export(
        &mut self,
        host_xid: u32,
    ) -> std::io::Result<yserver_core::backend::Dri3ExportBegin> {
        use yserver_core::backend::{Dri3ExportBegin, Dri3ExportToken};

        let id = self.store.lookup(host_xid).ok_or_else(|| {
            std::io::Error::other(format!("DRI3 export: unknown pixmap 0x{host_xid:x}"))
        })?;
        let scanout_storage = self.pstate.server_scanout.is_promoted(id)
            || self.pstate.server_scanout.awaits_release(id);
        self.disqualify_server_scanout(id, "server_source_exported");
        if !(scanout_storage && self.direct_frame_references_drawable(id)) {
            let exporter = self.pstate.dri3_drawable_exporter;
            return exporter(self, id, host_xid).map(Dri3ExportBegin::Ready);
        }
        let pin = self.pin_direct_source(id);
        let parked_at = self.monotonic_now();
        let exports = &mut self.pstate.server_scanout_exports;
        exports.next_token = exports.next_token.wrapping_add(1).max(1);
        let token = Dri3ExportToken(exports.next_token);
        exports.parked.insert(
            token,
            crate::kms::render::backend::ParkedServerScanoutExport {
                id,
                host_xid,
                pin,
                parked_at,
                deadline: parked_at + crate::kms::render::backend::SERVER_SCANOUT_EXPORT_WAIT,
                result: None,
            },
        );
        let server = &mut self.pstate.scanout_m0.server;
        server.export_deferred = server.export_deferred.saturating_add(1);
        log::debug!(
            target: "yserver::scanout_server",
            "DRI3 export of drawable={} deferred while it is scanned out",
            id.as_u64(),
        );
        Ok(Dri3ExportBegin::Pending(token))
    }

    /// The earliest deadline of a parked export still waiting for the CRTCs.
    pub(in crate::kms::render::backend) fn server_scanout_export_deadline(
        &self,
    ) -> Option<std::time::Instant> {
        self.pstate
            .server_scanout_exports
            .parked
            .values()
            .filter(|parked| parked.result.is_none())
            .map(|parked| parked.deadline)
            .min()
    }

    /// Past its deadline a parked export tears direct scanout down safely and
    /// re-lights the composed frame; the fd goes out only once every plane
    /// left the source, else the export fails with the pins kept (D6).
    pub(in crate::kms::render::backend) fn service_server_scanout_export_deadlines(&mut self) {
        const CONTEXT: &str = "server_source_export_deadline";
        self.release_unreferenced_server_scanout();
        let now = self.monotonic_now();
        let expired: Vec<_> = self
            .pstate
            .server_scanout_exports
            .parked
            .iter()
            .filter(|(_, parked)| parked.result.is_none() && parked.deadline <= now)
            .map(|(token, _)| *token)
            .collect();
        if expired.is_empty() {
            return;
        }
        let server = &mut self.pstate.scanout_m0.server;
        server.export_deadline_teardown = server.export_deadline_teardown.saturating_add(1);
        match self.teardown_direct_before_topology_requery(CONTEXT) {
            Ok(relight) => {
                // A failed relight already released the frame: the fd is safe.
                let _ = self.relight_after_direct_teardown(relight, CONTEXT);
                self.release_unreferenced_server_scanout();
            }
            Err(error) => {
                log::error!(
                    target: "yserver::scanout_server",
                    "deferred DRI3 export: direct teardown failed: {error}",
                );
                for token in expired {
                    let server = &mut self.pstate.scanout_m0.server;
                    server.export_deadline_fail = server.export_deadline_fail.saturating_add(1);
                    self.complete_server_scanout_export(
                        token,
                        Err(std::io::Error::other(format!(
                            "DRI3 export: scanout teardown failed: {error}"
                        ))),
                    );
                }
            }
        }
    }

    pub(in crate::kms::render::backend) fn backend_export_drain_ready_dri3_exports(
        &mut self,
    ) -> Vec<yserver_core::backend::Dri3ExportToken> {
        std::mem::take(&mut self.pstate.server_scanout_exports.ready)
    }

    pub(in crate::kms::render::backend) fn backend_export_finish_dri3_export(
        &mut self,
        token: yserver_core::backend::Dri3ExportToken,
    ) -> std::io::Result<yserver_core::backend::Dri3PixmapExport> {
        self.pstate
            .server_scanout_exports
            .parked
            .get_mut(&token)
            .and_then(|parked| parked.result.take())
            .unwrap_or_else(|| Err(std::io::Error::other("DRI3 export token not ready")))
    }

    /// Forget `token` and release its storage pin.
    pub(in crate::kms::render::backend) fn backend_export_release_dri3_export(
        &mut self,
        token: yserver_core::backend::Dri3ExportToken,
    ) {
        let exports = &mut self.pstate.server_scanout_exports;
        exports.ready.retain(|ready| *ready != token);
        if let Some(parked) = exports.parked.remove(&token) {
            <Self as yserver_core::backend::Backend>::release_present_source(self, parked.pin);
        }
    }

    /// Promote every not-yet-promoted member of `dest`'s identified rotation,
    /// within the D3 caps; all members or none are attempted.
    fn promote_server_rotation(&mut self, dest_id: DrawableId) {
        let Some(rotation) = self.pstate.server_scanout.rotations.get(&dest_id) else {
            return;
        };
        let members: Vec<DrawableId> = rotation.members.iter().map(|(id, _)| *id).collect();
        let sources = &self.pstate.server_scanout.sources;
        if let Some((id, state)) = members.iter().find_map(|id| match sources.get(id) {
            Some(ServerScanoutState::Failed(reason)) => Some((*id, reason.as_str())),
            Some(ServerScanoutState::Disqualified(_)) => Some((*id, "exported")),
            _ => None,
        }) {
            log::debug!(
                target: "yserver::scanout_server",
                "rotation on drawable={} stays composed: member drawable={} {state}",
                dest_id.as_u64(),
                id.as_u64(),
            );
            self.set_rotation_state(dest_id, RotationState::Failed);
            return;
        }
        let needed: Vec<DrawableId> = members
            .into_iter()
            .filter(|id| !self.pstate.server_scanout.is_promoted(*id))
            .collect();
        if needed.is_empty() {
            self.set_rotation_state(dest_id, RotationState::Complete);
            return;
        }

        let epoch = self.scanout_m1_topology_signature();
        let caps = &mut self.pstate.server_scanout;
        if caps.failure_epoch != epoch {
            caps.failure_epoch = epoch;
            caps.failures = 0;
        }
        let server = &mut self.pstate.scanout_m0.server;
        if caps.failures >= SERVER_SCANOUT_FAILURE_CAP {
            server.reject_cap_fail = server.reject_cap_fail.saturating_add(1);
            return;
        }
        if caps.live() + needed.len() > SERVER_SCANOUT_LIVE_CAP {
            server.reject_cap_live = server.reject_cap_live.saturating_add(1);
            return;
        }
        let reserved = u32::try_from(needed.len()).expect("a rotation has at most two members");
        let now = (self.pstate.monotonic_clock)();
        if !self.pstate.server_scanout.tokens.reserve(reserved, now) {
            let server = &mut self.pstate.scanout_m0.server;
            server.reject_cap_success = server.reject_cap_success.saturating_add(1);
            return;
        }
        if !self.sync_scanout_m1_topology() {
            self.pstate.server_scanout.tokens.refund(reserved);
            return;
        }

        let mut spent = 0;
        let mut failed = false;
        for id in needed {
            let server = &mut self.pstate.scanout_m0.server;
            server.promote_attempts = server.promote_attempts.saturating_add(1);
            let started = std::time::Instant::now();
            let result = (self.pstate.server_scanout_promoter)(self, id);
            let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            let server = &mut self.pstate.scanout_m0.server;
            server.promote_us_max = server.promote_us_max.max(elapsed_us);
            match result {
                Ok(ServerScanoutPromotion {
                    descriptor,
                    framebuffer,
                }) => {
                    spent += 1;
                    server.promote_ok = server.promote_ok.saturating_add(1);
                    server.probe_accept = server.probe_accept.saturating_add(1);
                    let layout = descriptor.layout;
                    log::info!(
                        target: "yserver::scanout_server",
                        "promoted server-owned Present source drawable={} {}x{} \
                         modifier={:#x} pitch={} offset={} in {elapsed_us}us",
                        id.as_u64(),
                        layout.width,
                        layout.height,
                        layout.modifier,
                        layout.pitch,
                        layout.offset,
                    );
                    let entry = {
                        let output_indices = self.direct_scope_outputs(DirectScope::Root);
                        let planes = scanout_m1_plane_states(
                            &self.platform.outputs,
                            &output_indices,
                            DirectScope::Root,
                        );
                        KmsBackend::scanout_m1_accepted_entry(framebuffer, &layout, &planes)
                    };
                    self.pstate.scanout_m1.remove(id);
                    self.pstate
                        .scanout_m1
                        .entries
                        .insert((id, DirectScope::Root), entry);
                    self.pstate
                        .server_scanout
                        .sources
                        .insert(id, ServerScanoutState::Promoted(descriptor));
                    self.store.track_scanout_writes(id);
                }
                Err(error) => {
                    server.promote_fail = server.promote_fail.saturating_add(1);
                    if matches!(error, ServerScanoutPromoteError::ProbeRejected(_)) {
                        server.probe_reject = server.probe_reject.saturating_add(1);
                    }
                    log::info!(
                        target: "yserver::scanout_server",
                        "server-owned Present source drawable={} not promoted: {error}",
                        id.as_u64(),
                    );
                    let caps = &mut self.pstate.server_scanout;
                    caps.failures = caps.failures.saturating_add(1);
                    caps.sources
                        .insert(id, ServerScanoutState::Failed(error.to_string()));
                    failed = true;
                    break;
                }
            }
        }
        self.pstate.server_scanout.tokens.refund(reserved - spent);
        self.set_rotation_state(
            dest_id,
            if failed {
                RotationState::Failed
            } else {
                RotationState::Complete
            },
        );
    }

    fn set_rotation_state(&mut self, dest_id: DrawableId, state: RotationState) {
        if let Some(rotation) = self.pstate.server_scanout.rotations.get_mut(&dest_id) {
            rotation.state = state;
        }
    }

    /// Production promoter: linear scanout storage, adopted only after M1
    /// validation, `AddFB2` and atomic `TEST_ONLY` accept the candidate.
    pub(in crate::kms::render::backend) fn promote_server_scanout_live(
        b: &mut KmsBackend,
        id: DrawableId,
    ) -> Result<ServerScanoutPromotion, ServerScanoutPromoteError> {
        let depth = b
            .store
            .get(id)
            .map(|drawable| drawable.depth)
            .ok_or_else(|| ServerScanoutPromoteError::Engine("drawable gone".into()))?;
        let root = (u32::from(b.platform.fb_w), u32::from(b.platform.fb_h));
        if !scanout_m1_outputs_cover_root(root, &b.scanout_m1_output_geometry(DirectScope::Root)) {
            return Err(ServerScanoutPromoteError::ProbeRejected(
                "outputs do not tile the root".into(),
            ));
        }
        let output_indices = b.direct_scope_outputs(DirectScope::Root);
        let cursor_positions = b
            .platform
            .cursor_plane_atomic_positions_for_outputs(&output_indices);
        let primary = b
            .platform
            .primary_device()
            .map(|primary| {
                (
                    Rc::clone(&primary.device),
                    Rc::clone(&primary.atomic_commit_sink),
                )
            })
            .ok_or_else(|| ServerScanoutPromoteError::ProbeRejected("no primary device".into()))?;
        let platform = &mut b.platform;
        let output_modifiers: Vec<&[u64]> = platform
            .outputs
            .iter()
            .map(|output| output.output.scanout_modifiers.as_slice())
            .collect();
        let plane_states =
            scanout_m1_plane_states(&platform.outputs, &output_indices, DirectScope::Root);
        let mut layout_seen = None;
        let promotion = b.engine.promote_drawable_for_scanout(
            &mut platform.gpu,
            &mut b.store,
            id,
            &output_modifiers,
            |candidate| {
                let layout = ScanoutSourceLayout {
                    fourcc: DRM_FORMAT_XRGB8888,
                    vk_format: candidate.format,
                    depth,
                    bpp: 32,
                    width: u16::try_from(candidate.width).map_err(|_| "width".to_string())?,
                    height: u16::try_from(candidate.height).map_err(|_| "height".to_string())?,
                    plane_count: 1,
                    offset: candidate.offset,
                    pitch: candidate.pitch,
                    modifier: candidate.modifier,
                };
                scanout_m1_validate(&layout, root, &output_modifiers)
                    .map_err(|reason| format!("validate: {reason:?}"))?;
                let result = crate::drm::modeset::probe_direct_scanout_test_only(
                    Rc::clone(&primary.0),
                    candidate.fd,
                    candidate.width,
                    candidate.height,
                    layout.fourcc,
                    layout.modifier,
                    layout.offset,
                    layout.pitch,
                    primary.1.as_ref(),
                    &plane_states,
                    &cursor_positions,
                );
                match result {
                    Ok(crate::drm::modeset::DirectScanoutTestResult::Accepted(framebuffer)) => {
                        layout_seen = Some(layout);
                        Ok(framebuffer)
                    }
                    Ok(crate::drm::modeset::DirectScanoutTestResult::Rejected(error)) => {
                        Err(format!("TEST_ONLY rejected: {error}"))
                    }
                    Err(error) => Err(format!("import/probe failed: {error}")),
                }
            },
        );
        match promotion {
            Ok(promotion) => Ok(ServerScanoutPromotion {
                descriptor: ServerScanoutDescriptor {
                    fd: promotion.export.fd,
                    layout: layout_seen.expect("the probe accepted a layout"),
                },
                framebuffer: promotion.accepted,
            }),
            Err(crate::kms::render::engine::ScanoutPromoteError::Probe(reason)) => {
                Err(ServerScanoutPromoteError::ProbeRejected(reason))
            }
            Err(error) => Err(ServerScanoutPromoteError::Engine(format!("{error:?}"))),
        }
    }
}
