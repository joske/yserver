use super::*;

impl CrtcKey {
    pub(crate) fn new(
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Self {
        Self { device_key, crtc }
    }

    pub(crate) fn for_output(output: &ActiveOutput) -> Self {
        Self::new(output.key.device_key, output.output.crtc)
    }
}

impl RenderDevice {
    #[must_use]
    pub(crate) fn relationship_to(&self, kms: &KmsDevice) -> RenderKmsRelationship {
        match self.advertised_primary_node {
            Some(primary) if primary == kms.key => RenderKmsRelationship::Same,
            Some(_) => RenderKmsRelationship::Different,
            None => RenderKmsRelationship::Unknown,
        }
    }

    #[must_use]
    pub(crate) fn scanout_route_to(&self, kms: &KmsDevice) -> ScanoutRoute {
        ScanoutRoute::new(self.id, kms.key, self.relationship_to(kms))
    }
}

impl PlatformBackend {
    pub(crate) fn fb_dimensions(&self) -> (u16, u16) {
        (self.fb_w, self.fb_h)
    }

    pub(crate) fn take_input_ctx(&mut self) -> Option<crate::input::SendContext> {
        self.input_ctx.take()
    }

    /// The current CRTC transform of live output `idx`, `None` at identity.
    pub(crate) fn output_transform(
        &self,
        idx: usize,
    ) -> Option<&yserver_core::randr::CrtcTransform> {
        self.output_transforms.get(&self.outputs.get(idx)?.key)
    }

    /// Whether any live output scans out through a transform.
    pub(crate) fn any_output_transformed(&self) -> bool {
        (0..self.outputs.len()).any(|idx| self.output_transform(idx).is_some())
    }

    /// The root rectangle live output `idx` shows: its mode at the CRTC
    /// origin, or the transformed footprint there (spec D3).
    pub(crate) fn output_root_rect(&self, idx: usize) -> (i32, i32, u32, u32) {
        let layout = &self.outputs[idx];
        let (w, h) = self
            .output_transform(idx)
            .map_or((layout.width, layout.height), |t| {
                t.footprint(layout.width, layout.height)
            });
        (layout.x, layout.y, u32::from(w), u32::from(h))
    }

    pub(crate) fn primary_device(&self) -> Option<&KmsDevice> {
        self.devices.first()
    }

    pub(crate) fn selected_render_device(&self) -> Option<&RenderDevice> {
        let selected = self.selected_render_device?;
        self.render_devices
            .iter()
            .find(|device| device.id == selected)
    }

    /// Resolve the sink-side renderer for copied scanout without guessing.
    /// A usable sink must be a distinct inventoried Vulkan endpoint whose
    /// advertised DRM primary identity is exactly the target KMS device.
    /// Missing or ambiguous inventory is an unavailable copied candidate,
    /// never a reason to rescore GPUs or reinterpret a render-node identity.
    fn copied_sink_renderer_for_kms(
        &self,
        kms_key: crate::platform::drm::DrmDeviceKey,
    ) -> io::Result<(RenderDeviceId, VulkanDeviceSelector)> {
        let selected = self
            .selected_render_device
            .ok_or_else(|| io::Error::other("copied scanout has no selected source renderer"))?;
        resolve_copied_sink_renderer(&self.render_devices, selected, kms_key)
    }

    /// Return the scalar Vulkan identities needed by an isolated route probe.
    /// Copy-free qualification remains useful when no unambiguous copied sink
    /// exists, so sink-resolution failure is represented as `None` rather than
    /// preventing the worker request.
    pub(crate) fn scanout_qualification_devices_for_kms(
        &self,
        kms_key: crate::platform::drm::DrmDeviceKey,
    ) -> io::Result<(VulkanDeviceSelector, Option<CopiedQualificationSink>)> {
        let source = self.selected_render_device().ok_or_else(|| {
            io::Error::other(format!(
                "scanout qualification for {kms_key} has no selected source renderer"
            ))
        })?;
        let copied_sink = match self.copied_sink_renderer_for_kms(kms_key) {
            Ok((id, selector)) => Some(CopiedQualificationSink { id, selector }),
            Err(error) => {
                log::debug!(
                    "scanout qualification for {kms_key}: copied sink unavailable: {error}"
                );
                None
            }
        };
        Ok((source.selector, copied_sink))
    }

    pub(super) fn copied_sink_context_for_kms(
        &mut self,
        kms_key: crate::platform::drm::DrmDeviceKey,
    ) -> io::Result<(RenderDeviceId, Arc<VkContext>)> {
        let (renderer_id, selector) = self.copied_sink_renderer_for_kms(kms_key)?;
        if let Some(vk) = self.copy_vk_contexts.get(&renderer_id) {
            return Ok((renderer_id, Arc::clone(vk)));
        }
        let vk = VkContext::new_transfer_for_device(selector).map_err(|error| {
            io::Error::other(format!(
                "copied scanout sink Vulkan context for {renderer_id:?}/{kms_key}: {error}"
            ))
        })?;
        self.copy_vk_contexts.insert(renderer_id, Arc::clone(&vk));
        Ok((renderer_id, vk))
    }

    #[cfg(test)]
    pub(crate) fn selected_render_device_mut(&mut self) -> Option<&mut RenderDevice> {
        let selected = self.selected_render_device?;
        self.render_devices
            .iter_mut()
            .find(|device| device.id == selected)
    }

    pub(crate) fn device_for_key(
        &self,
        key: crate::platform::drm::DrmDeviceKey,
    ) -> Option<&KmsDevice> {
        self.devices.iter().find(|device| device.key == key)
    }

    pub(crate) fn device_for_output(&self, key: &OutputKey) -> Option<&KmsDevice> {
        self.device_for_key(key.device_key)
    }

    /// Construct the live renderer-to-KMS route for one display device.
    ///
    /// A missing renderer is accepted only by the explicit Vk-less fixture;
    /// production backends with a Vulkan context must always have a selected
    /// renderer inventory entry.
    pub(crate) fn scanout_route_for_kms(
        &self,
        kms_device_key: crate::platform::drm::DrmDeviceKey,
    ) -> io::Result<ScanoutRoute> {
        let kms = self.device_for_key(kms_device_key).ok_or_else(|| {
            io::Error::other(format!("no KMS device for scanout route {kms_device_key}"))
        })?;
        if let Some(renderer) = self.selected_render_device() {
            return Ok(renderer.scanout_route_to(kms));
        }
        if self.vk.is_none() {
            return Ok(ScanoutRoute::new(
                RenderDeviceId::UnverifiedFallback,
                kms.key,
                RenderKmsRelationship::Unknown,
            ));
        }
        Err(io::Error::other(format!(
            "Vulkan is active but no renderer is selected for KMS device {}",
            kms.key
        )))
    }

    pub(crate) fn output_index_for_crtc(&self, crtc_key: CrtcKey) -> Option<usize> {
        self.outputs.iter().position(|output| {
            output.key.device_key == crtc_key.device_key && output.output.crtc == crtc_key.crtc
        })
    }

    pub(super) fn drm_device_index_for_fd(&self, drm_fd: RawFd) -> Option<usize> {
        self.devices
            .iter()
            .position(|device| device.device.as_fd().as_raw_fd() == drm_fd)
    }
}
