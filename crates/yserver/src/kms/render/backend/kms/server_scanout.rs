use crate::kms::render::{
    backend::{
        KmsBackend, KmsBackendState, RotationState, SERVER_SCANOUT_FAILURE_CAP,
        SERVER_SCANOUT_LIVE_CAP, ScanoutM0Target, ScanoutSourceLayout, ServerScanoutDescriptor,
        ServerScanoutPromoteError, ServerScanoutPromotion, ServerScanoutState,
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
    /// Ask the active direct frame to hand the screen back to the scene.
    pub(in crate::kms::render::backend) fn request_direct_unflip(&mut self, reason: &'static str) {
        if !self.direct.root().active() {
            return;
        }
        if !self.direct.root().unflip_requested {
            self.direct.root_mut().unflip_reason = Some(reason);
            *self
                .scanout_m0
                .server
                .unflip_reasons
                .entry(reason)
                .or_default() += 1;
        }
        self.direct.root_mut().unflip_requested = true;
        self.direct.root_mut().unflip_last_reason = Some(reason);
        self.direct.root_mut().hold_direct = false;
    }

    /// A drawable is gone: drop its probe framebuffer, then its scanout
    /// descriptor (D9), and leave any `Complete` rotation it belonged to.
    pub(in crate::kms::render::backend) fn retire_scanout_drawable(&mut self, id: DrawableId) {
        self.scanout_m1.remove(id);
        if self.server_scanout.retire(id) {
            self.request_direct_unflip("server_source_retired");
        }
    }
}

impl KmsBackend {
    /// Whether M2 may flip `source_id` from its cached framebuffer. Imported
    /// sources keep today's path; promoted server-owned sources do not flip
    /// until their writes are fenced (plan Tasks 7–9).
    pub(in crate::kms::render::backend) fn direct_source_flip_admitted(
        &self,
        source_id: DrawableId,
    ) -> bool {
        !self.pstate.server_scanout.is_promoted(source_id)
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
            if self.pstate.server_scanout.is_promoted(source_id) {
                self.disqualify_server_scanout_source(source_id);
            }
            let server = &mut self.pstate.scanout_m0.server;
            server.reject_exported = server.reject_exported.saturating_add(1);
            return;
        }
        if matches!(
            self.pstate.server_scanout.sources.get(&source_id),
            Some(ServerScanoutState::Disqualified)
        ) {
            let server = &mut self.pstate.scanout_m0.server;
            server.reject_exported = server.reject_exported.saturating_add(1);
            return;
        }
        let server = &mut self.pstate.scanout_m0.server;
        server.candidates = server.candidates.saturating_add(1);
        let observation = self.pstate.server_scanout.observe(dest_id, source_id);
        if observation.left_complete {
            self.pstate
                .request_direct_unflip("server_source_rotation_changed");
        }
        if observation.identified {
            self.promote_server_rotation(dest_id);
        }
    }

    /// Server-owned, depth-24 BGRA, root-sized, Presented whole to the COW or
    /// a COW descendant that passes the M2 gate.
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
            && self.direct_candidate_gate(candidate, target, unbordered, true)
            && scanout_m1_outputs_cover_root(root, &self.scanout_m1_output_geometry())
    }

    /// Whether a client holds, or may hold, an fd of `source_id` (D6).
    fn server_scanout_source_exported(&self, source_id: DrawableId) -> bool {
        self.store.is_exported(source_id) || self.pstate.exported_dmabufs.contains_key(&source_id)
    }

    /// A promoted source was handed out: never flip it again, and leave any
    /// complete rotation holding it (D6).
    fn disqualify_server_scanout_source(&mut self, source_id: DrawableId) {
        self.pstate.scanout_m1.remove(source_id);
        self.pstate
            .server_scanout
            .sources
            .insert(source_id, ServerScanoutState::Disqualified);
        let mut left_complete = false;
        for rotation in self.pstate.server_scanout.rotations.values_mut() {
            if rotation.state == RotationState::Complete
                && rotation.members.iter().any(|(id, _)| *id == source_id)
            {
                rotation.state = RotationState::Failed;
                left_complete = true;
            }
        }
        if left_complete {
            self.pstate.request_direct_unflip("server_source_exported");
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
            Some(ServerScanoutState::Disqualified) => Some((*id, "exported")),
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
                    let entry = self.scanout_m1_accepted_entry(framebuffer, &layout);
                    self.pstate.scanout_m1.entries.remove(&id);
                    self.pstate.scanout_m1.entries.insert(id, entry);
                    self.pstate
                        .server_scanout
                        .sources
                        .insert(id, ServerScanoutState::Promoted(descriptor));
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
        let output_geometry = b.scanout_m1_output_geometry();
        let output_indices: Vec<_> = (0..b.platform.outputs.len()).collect();
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
        let plane_states = scanout_m1_plane_states(&platform.outputs);
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
                scanout_m1_validate(
                    &layout,
                    root,
                    (layout.width, layout.height),
                    &output_geometry,
                    &output_modifiers,
                )
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
