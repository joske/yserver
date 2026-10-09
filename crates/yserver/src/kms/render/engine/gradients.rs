use super::*;

impl RenderEngine {
    /// Stage 3b + B.2 fix + B.3 hotfix 2: drop the engine's
    /// `picture_paint` entry for `host_pic`. Called by
    /// `KmsBackend::render_free_picture` after removing the picture
    /// record from `KmsCore.pictures`.
    ///
    /// **B.2 fix**: routes the `GradientPicture` through
    /// `adopt_retired_resource_for_gpu_retirement`. The engine's
    /// HashMap clone is an Arc clone; `BatchResource::release` drops
    /// it (decrements the Arc). If a recorded deferred op holds
    /// another clone (B.3 hotfix 2 path), the Vk handles stay alive
    /// until BOTH clones drop — after the GPU fence fires.
    ///
    /// **B.3 hotfix 2**: `GradientPicture` is now `Arc`-backed; the
    /// `picture_paint_remove` drop here is safe regardless of any
    /// in-flight recorded ops holding their own clones.
    ///
    /// `SolidFill` variants carry no Vk handles — HashMap::remove
    /// drop with no fence gating needed.
    pub(crate) fn picture_paint_remove(&mut self, host_pic: u32) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let Some(state) = inner.picture_paint.remove(&host_pic) else {
            return;
        };
        match state {
            PicturePaintState::Gradient(gradient) => {
                inner.adopt_retired_resource_for_gpu_retirement(Some(Box::new(gradient)
                    as Box<dyn crate::kms::render::batch_resource::BatchResource>));
            }
        }
    }

    /// Stage 3f.13: build the LUT for a `RenderCreateLinearGradient`
    /// picture and stash it on the engine's `picture_paint` map.
    /// Subsequent `render_composite` calls referencing `host_pic`
    /// as src or mask sample this LUT instead of falling back to
    /// the 3f.12 first-stop SolidFill collapse.
    ///
    /// #214: no GPU round trip — the LUT upload is deferred into the
    /// open frame (see [`Self::insert_gradient_with_deferred_upload`]).
    ///
    /// # Errors
    ///
    /// Returns `NoVk` on the test fixture; `Vk` if the LUT image /
    /// view / memory or the upload staging allocation fails.
    pub(crate) fn build_and_insert_linear_gradient(
        &mut self,
        platform: &mut PlatformBackend,
        host_pic: u32,
        p1: (i32, i32),
        p2: (i32, i32),
        stops: &[crate::kms::vk::gradient::Stop],
    ) -> Result<(), RenderError> {
        let vk = self.inner.as_ref().ok_or(RenderError::NoVk)?.vk.clone();
        let (gradient, pixels) =
            crate::kms::vk::gradient::GradientPicture::new_linear(vk, p1, p2, stops)
                .map_err(gradient_error)?;
        self.insert_gradient_with_deferred_upload(platform, host_pic, gradient, &pixels)
    }

    /// Stage 3f.13: radial-gradient companion of
    /// [`build_and_insert_linear_gradient`]. Sizes the LUT image
    /// at `RADIAL_SIDE × RADIAL_SIDE` and renders the two-circle
    /// radial CPU-side; the upload is deferred like the linear one.
    ///
    /// # Errors
    ///
    /// Returns `NoVk` on the test fixture; `Vk` on allocation
    /// failure.
    pub(crate) fn build_and_insert_radial_gradient(
        &mut self,
        platform: &mut PlatformBackend,
        host_pic: u32,
        inner_circle: (i32, i32, i32),
        outer_circle: (i32, i32, i32),
        stops: &[crate::kms::vk::gradient::Stop],
    ) -> Result<(), RenderError> {
        let vk = self.inner.as_ref().ok_or(RenderError::NoVk)?.vk.clone();
        let (gradient, pixels) = crate::kms::vk::gradient::GradientPicture::new_radial(
            vk,
            inner_circle,
            outer_circle,
            stops,
        )
        .map_err(gradient_error)?;
        self.insert_gradient_with_deferred_upload(platform, host_pic, gradient, &pixels)
    }

    /// #214: register a freshly allocated (still uninitialized) gradient
    /// picture and queue its pixel upload into the open frame, opening
    /// one if needed — instead of a blocking one-shot submit + fence
    /// wait per gradient.
    ///
    /// Ordering: every sampler of the picture is a frame op recorded
    /// after this call, so it lands later in this frame's command
    /// buffer (after the upload, which close emits at the frame head)
    /// or in a later submission on the same queue; the upload's
    /// closing barrier makes the copy visible to fragment sampling for
    /// both. Lifetime: the pixels live in the frame's upload arena and
    /// a picture clone is adopted into the frame's pin set; both are
    /// released only when the frame's fence retires, so a FreePicture
    /// before (or without) any use cannot free the image under the copy.
    fn insert_gradient_with_deferred_upload(
        &mut self,
        platform: &mut PlatformBackend,
        host_pic: u32,
        gradient: crate::kms::vk::gradient::GradientPicture,
        pixels: &[u8],
    ) -> Result<(), RenderError> {
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        if !inner.frame_builder.is_open() {
            let ticket = platform.submit_group_ticket_or_open()?;
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        // Allocation failure leaves the frame untouched; the unused
        // gradient drops here (never referenced by any command buffer).
        let upload_pin = inner.upload_to_frame(
            pixels,
            inner.upload_copy_align,
            crate::kms::vk::mem_accounting::ChurnClass::Gradient,
        )?;
        let open = inner.frame_builder.open.as_mut().expect("opened above");
        open.pins.adopt_retired(Box::new(gradient.clone())
            as Box<dyn crate::kms::render::batch_resource::BatchResource>);
        open.gradient_inits
            .push(crate::kms::render::frame_builder::RecordedGradientInit {
                picture: gradient.clone(),
                upload_pin,
            });
        if let Some(PicturePaintState::Gradient(old)) = inner
            .picture_paint
            .insert(host_pic, PicturePaintState::Gradient(gradient))
        {
            inner.adopt_retired_resource_for_gpu_retirement(Some(
                Box::new(old) as Box<dyn crate::kms::render::batch_resource::BatchResource>
            ));
        }
        Ok(())
    }

    /// Stage 3b test helper: how many picture-paint entries are
    /// currently tracked. Used to assert that
    /// `render_free_picture` drops its slot.
    #[cfg(test)]
    pub(crate) fn picture_paint_len(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.picture_paint.len())
    }
}

fn gradient_error(e: crate::kms::vk::gradient::GradientError) -> RenderError {
    match e {
        crate::kms::vk::gradient::GradientError::Vk(r) => RenderError::Vk(r),
        crate::kms::vk::gradient::GradientError::NoMemoryType => {
            RenderError::Vk(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
        }
    }
}
