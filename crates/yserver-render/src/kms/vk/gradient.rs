//! Pre-rendered gradient pictures (sub-phase 4.1.5 prep).
//!
//! Replaces `pixman_image_create_linear_gradient` /
//! `pixman_image_create_radial_gradient`. Both linear and radial
//! pictures CPU-evaluate at creation time into a small `B8G8R8A8_UNORM`
//! image; at Composite time the existing 4.1.4.6 pipeline samples them
//! like any other Drawable source. The picture's per-call affine
//! transform handles the dst → gradient-space projection so gradients
//! don't need a custom shader.
//!
//! Construction does no GPU work: it allocates the image and returns
//! the evaluated pixels. The caller records the upload
//! ([`GradientPicture::record_initial_upload`]) into a command buffer
//! that is submitted before — or is — the first one that samples the
//! picture, and keeps both the pixels' staging memory and a picture
//! clone alive until that command buffer retires (#214: a synchronous
//! upload cost one GPU round trip per `CreateLinearGradient`).
//!
//! ## Linear gradient
//!
//! Stored as a `256×1` LUT keyed on the gradient parameter `t ∈ [0, 1]`.
//! The composite-time transform composes the picture's transform with
//! the **axis projection**:
//!
//! ```text
//!   t(P) = ((P - p1) · v) / (v · v)        v = p2 - p1
//! ```
//!
//! `axis_projection_xform` builds the affine 2×3 that maps a dst-pixel
//! `(px, py)` to LUT-pixel `(256 * t, 0.5)`, so the existing Composite
//! shader (with `repeat_mode = Pad`) gives the right colour with no
//! shader changes. Caller multiplies on the picture's user transform
//! (if any).
//!
//! ## Radial gradient
//!
//! Pre-rendered into a `256×256` 2D image whose internal coordinate
//! space is the outer circle's bounding box (centered at the inner
//! circle's centre, scaled so the outer-circle radius lands at the
//! image edge). Per-pixel evaluation runs the standard radial
//! parameter equation; outside the gradient (`t < 0` or `t > 1`,
//! singular cases) the pixel is transparent.
//!
//! For radial, `axis_projection_xform` maps dst-pixel into the
//! image's pixel space; transform composition with the picture's
//! user transform happens at the Composite call site.

use std::sync::Arc;

use ash::vk;

use super::{device::VkContext, ops::render::AffineXform};

#[derive(Debug, thiserror::Error)]
pub enum GradientError {
    #[error("vulkan: {0:?}")]
    Vk(vk::Result),
    #[error("no memory type matches gradient image requirements")]
    NoMemoryType,
}

impl From<vk::Result> for GradientError {
    fn from(r: vk::Result) -> Self {
        GradientError::Vk(r)
    }
}

const LUT_LEN: u32 = 256;
const RADIAL_SIDE: u32 = 256;

/// One stop in a gradient. `pos` is X11 fixed-point 16.16; colour
/// channels are 16-bit *straight* (non-premultiplied) — `XRenderColor`
/// stops arrive that way on the `RenderCreateLinearGradient` /
/// `RenderCreateRadialGradient` wire (see rendercheck
/// `t_gradient.c:119-123` — it sends raw `stop_list[]` colours
/// without the premultiply that `main.c:337-345` does for solid
/// fills). The server premultiplies when evaluating the gradient
/// LUT below.
#[derive(Debug, Clone, Copy)]
pub struct Stop {
    pub pos: i32,
    pub r: u16,
    pub g: u16,
    pub b: u16,
    pub a: u16,
}

/// Shared-ownership inner resources for a [`GradientPicture`].
///
/// **Lifetime contract (B.3 hotfix 2):** Vk handles are destroyed in
/// this struct's `Drop`. `GradientPicture` is an
/// `Arc<GradientPictureResources>` newtype so the picture can be
/// cloned onto [`super::super::render::frame_builder::RecordedTrapSrcKind::Gradient`]
/// at append time. The engine's `picture_paint` map holds one clone;
/// each recorded deferred op holds another. Both are routed through
/// `adopt_retired_resource_for_gpu_retirement`; Vk handles are only
/// destroyed when the LAST Arc clone drops after the GPU fence fires.
struct GradientPictureResources {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    /// dst-pixel → gradient-image-pixel affine.
    axis_projection: AffineXform,
}

// SAFETY: raw Vk handles are valid to send across threads under
// single-threaded-engine + retire-path lock discipline.
unsafe impl Send for GradientPictureResources {}
unsafe impl Sync for GradientPictureResources {}

impl Drop for GradientPictureResources {
    fn drop(&mut self) {
        // B.3 hotfix 2: safe because the last Arc clone only drops
        // after the GPU fence has fired (engine's clone lives in
        // pins.retired_resources; recorded-op clone is moved there at
        // close). No CB references the view at this point.
        unsafe {
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

/// Evaluated gradient picture — shared-ownership handle wrapping
/// `Arc<GradientPictureResources>`.
///
/// **B.3 hotfix 2:** `Clone` increments the inner Arc; the engine's
/// `picture_paint` map and each deferred `RecordedTrapSrcKind::Gradient`
/// both hold a clone. Both clones are routed through
/// `adopt_retired_resource_for_gpu_retirement` so Vk handles outlive
/// their last GPU use. Drop of the last clone triggers
/// `GradientPictureResources::drop`, which destroys the Vk handles.
#[derive(Clone)]
pub struct GradientPicture(Arc<GradientPictureResources>);

impl std::fmt::Debug for GradientPicture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GradientPicture")
            .field("image", &self.0.image)
            .field("view", &self.0.view)
            .field("extent", &self.0.extent)
            .field("axis_projection", &self.0.axis_projection)
            .finish()
    }
}

impl GradientPicture {
    pub fn extent(&self) -> vk::Extent2D {
        self.0.extent
    }

    pub fn image_view(&self) -> vk::ImageView {
        self.0.view
    }

    /// dst-pixel → gradient-image-pixel affine projection.
    pub fn axis_projection(&self) -> AffineXform {
        self.0.axis_projection
    }

    pub fn image(&self) -> vk::Image {
        self.0.image
    }

    /// Weak handle to the shared resources, for tests that prove the
    /// Vk handles are released once every clone is gone.
    #[cfg(test)]
    pub(crate) fn downgrade(&self) -> std::sync::Weak<impl Sized + use<>> {
        Arc::downgrade(&self.0)
    }

    /// Create a linear gradient between `p1` and `p2` (both X11
    /// fixed-point 16.16) with the given stops. Renders the colour
    /// LUT on the CPU and returns the picture together with the LUT
    /// bytes; the image is NOT initialized until the caller records
    /// [`Self::record_initial_upload`] with those bytes.
    pub fn new_linear(
        vk: Arc<VkContext>,
        p1: (i32, i32),
        p2: (i32, i32),
        stops: &[Stop],
    ) -> Result<(Self, Vec<u8>), GradientError> {
        let (image, view, memory) = allocate_image(
            &vk,
            vk::Extent2D {
                width: LUT_LEN,
                height: 1,
            },
        )?;

        // Fill LUT row. Stops interpolate in straight-alpha space
        // (matches rendercheck `gradientPixel`), then premultiply to
        // match X RENDER's premul-storage convention.
        let mut lut = vec![0u8; (LUT_LEN as usize) * 4];
        for i in 0..LUT_LEN as usize {
            let t = i as f32 / (LUT_LEN - 1) as f32;
            let (r, g, b, a) = sample_stops(stops, t);
            let (r_pm, g_pm, b_pm) = premultiply(r, g, b, a);
            // BGRA, premultiplied.
            lut[i * 4] = (b_pm >> 8) as u8;
            lut[i * 4 + 1] = (g_pm >> 8) as u8;
            lut[i * 4 + 2] = (r_pm >> 8) as u8;
            lut[i * 4 + 3] = (a >> 8) as u8;
        }

        // Build axis-projection affine: t(P) = ((P - p1) · v) / |v|²,
        // u_in_pixels = LUT_LEN * t.
        let p1x = (p1.0 as f32) / 65536.0;
        let p1y = (p1.1 as f32) / 65536.0;
        let p2x = (p2.0 as f32) / 65536.0;
        let p2y = (p2.1 as f32) / 65536.0;
        let vx = p2x - p1x;
        let vy = p2y - p1y;
        let v_dot_v = (vx * vx + vy * vy).max(f32::EPSILON);
        let scale = LUT_LEN as f32 / v_dot_v;
        let row0 = [scale * vx, scale * vy, -scale * (vx * p1x + vy * p1y), 0.0];
        // y row: always sample row 0 (LUT is 1 pixel tall). 0.5 centres
        // the sample on that row to dodge any fp rounding into row 1.
        let row1 = [0.0, 0.0, 0.5, 0.0];

        let picture = Self(Arc::new(GradientPictureResources {
            vk,
            image,
            view,
            memory,
            extent: vk::Extent2D {
                width: LUT_LEN,
                height: 1,
            },
            axis_projection: AffineXform { row0, row1 },
        }));
        Ok((picture, lut))
    }

    /// Create a radial gradient. `inner = (cx, cy, r)` and
    /// `outer = (cx, cy, r)`, both X11 fixed-point 16.16. Stops are
    /// evaluated CPU-side into a `256×256` square whose internal
    /// pixel space spans the outer-circle bbox; the axis-projection
    /// transform then maps dst-pixel coordinates into that bbox.
    /// Returns the picture and its pixels, like [`Self::new_linear`].
    pub fn new_radial(
        vk: Arc<VkContext>,
        inner: (i32, i32, i32),
        outer: (i32, i32, i32),
        stops: &[Stop],
    ) -> Result<(Self, Vec<u8>), GradientError> {
        let (image, view, memory) = allocate_image(
            &vk,
            vk::Extent2D {
                width: RADIAL_SIDE,
                height: RADIAL_SIDE,
            },
        )?;

        let icx = (inner.0 as f32) / 65536.0;
        let icy = (inner.1 as f32) / 65536.0;
        let ir = (inner.2 as f32) / 65536.0;
        let ocx = (outer.0 as f32) / 65536.0;
        let ocy = (outer.1 as f32) / 65536.0;
        let or = (outer.2 as f32) / 65536.0;

        // Image span: a square of side `2 * or` centred at outer.
        // Each image pixel `(px, py)` corresponds to `(ocx - or +
        // (px + 0.5) * (2*or / RADIAL_SIDE), ocy - or + …)` in dst-
        // space units. We evaluate the radial parameter at each
        // image pixel and write the LUT colour.
        let span = 2.0 * or;
        let mut img = vec![0u8; (RADIAL_SIDE * RADIAL_SIDE * 4) as usize];
        for py in 0..RADIAL_SIDE as usize {
            for px in 0..RADIAL_SIDE as usize {
                let x = ocx - or + (px as f32 + 0.5) * span / RADIAL_SIDE as f32;
                let y = ocy - or + (py as f32 + 0.5) * span / RADIAL_SIDE as f32;
                let t = radial_parameter(x, y, icx, icy, ir, ocx, ocy, or);
                let off = (py * RADIAL_SIDE as usize + px) * 4;
                if let Some(t) = t {
                    let (r, g, b, a) = sample_stops(stops, t.clamp(0.0, 1.0));
                    let (r_pm, g_pm, b_pm) = premultiply(r, g, b, a);
                    img[off] = (b_pm >> 8) as u8;
                    img[off + 1] = (g_pm >> 8) as u8;
                    img[off + 2] = (r_pm >> 8) as u8;
                    img[off + 3] = (a >> 8) as u8;
                } else {
                    // Outside the gradient locus → transparent.
                    img[off] = 0;
                    img[off + 1] = 0;
                    img[off + 2] = 0;
                    img[off + 3] = 0;
                }
            }
        }

        // Affine: dst-pixel → image pixel.
        // image_x = (dst_x - (ocx - or)) * RADIAL_SIDE / span
        let s = RADIAL_SIDE as f32 / span;
        let row0 = [s, 0.0, -s * (ocx - or), 0.0];
        let row1 = [0.0, s, -s * (ocy - or), 0.0];

        let picture = Self(Arc::new(GradientPictureResources {
            vk,
            image,
            view,
            memory,
            extent: vk::Extent2D {
                width: RADIAL_SIDE,
                height: RADIAL_SIDE,
            },
            axis_projection: AffineXform { row0, row1 },
        }));
        Ok((picture, img))
    }
}

impl crate::kms::render::batch_resource::BatchResource for GradientPicture {
    fn release(self: Box<Self>, _vk: &VkContext) {
        // B.3 hotfix 2: dropping the Box drops this Arc clone.
        // If this was the last clone, GradientPictureResources::drop
        // fires and destroys the Vk handles. If another clone
        // (from a recorded op) is still alive, the handles remain
        // live until that clone also drops after the CB retires.
        // _vk is unused — destruction is in Drop, not here.
        drop(self);
    }
}

/// Evaluate the gradient stop array at `t ∈ [0, 1]`. Stops are
/// straight-alpha 16-bit per channel; the return is the same.
fn sample_stops(stops: &[Stop], t: f32) -> (u16, u16, u16, u16) {
    if stops.is_empty() {
        return (0, 0, 0, 0);
    }
    let target = (t.clamp(0.0, 1.0) * 65536.0) as i64;
    // Find bracketing stops.
    let mut lo: Option<&Stop> = None;
    let mut hi: Option<&Stop> = None;
    for s in stops {
        if (s.pos as i64) <= target {
            lo = Some(s);
        }
        if (s.pos as i64) >= target {
            hi = Some(s);
            break;
        }
    }
    match (lo, hi) {
        (Some(l), Some(h)) if l.pos == h.pos => (l.r, l.g, l.b, l.a),
        (Some(l), Some(h)) => {
            let span = (h.pos - l.pos) as f32;
            let local = ((target - l.pos as i64) as f32) / span;
            (
                lerp_u16(l.r, h.r, local),
                lerp_u16(l.g, h.g, local),
                lerp_u16(l.b, h.b, local),
                lerp_u16(l.a, h.a, local),
            )
        }
        (Some(l), None) => (l.r, l.g, l.b, l.a),
        (None, Some(h)) => (h.r, h.g, h.b, h.a),
        (None, None) => (0, 0, 0, 0),
    }
}

/// Premultiply 16-bit straight RGB by 16-bit alpha. Rounded division
/// `(c * a + 32767) / 65535` keeps the boundary-mapping clean
/// (`a == 65535` → identity, `a == 0` → 0) without drifting on the
/// interior values.
fn premultiply(r: u16, g: u16, b: u16, a: u16) -> (u16, u16, u16) {
    let mul = |c: u16| -> u16 {
        ((u32::from(c) * u32::from(a) + 32767) / 65535)
            .min(65535)
            .try_into()
            .expect("clamped above")
    };
    (mul(r), mul(g), mul(b))
}

fn lerp_u16(a: u16, b: u16, t: f32) -> u16 {
    let a = a as f32;
    let b = b as f32;
    (a + (b - a) * t).round().clamp(0.0, 65535.0) as u16
}

/// Compute the radial gradient parameter `t` at world position
/// `(x, y)` for the standard X11 RENDER two-circle radial. Returns
/// `None` if the locus is empty (no real solution in `[0, 1]`).
#[allow(clippy::too_many_arguments)]
fn radial_parameter(
    x: f32,
    y: f32,
    icx: f32,
    icy: f32,
    ir: f32,
    ocx: f32,
    ocy: f32,
    or: f32,
) -> Option<f32> {
    // Standard cairo-style two-circle radial gradient. Solve for t in
    // |P - (C0 + t*(C1 - C0))|² = (r0 + t*(r1 - r0))² for the
    // greater real root in [0, 1].
    let cx_d = ocx - icx;
    let cy_d = ocy - icy;
    let r_d = or - ir;
    let px = x - icx;
    let py = y - icy;

    let a = cx_d * cx_d + cy_d * cy_d - r_d * r_d;
    let b = -2.0 * (px * cx_d + py * cy_d + ir * r_d);
    let c = px * px + py * py - ir * ir;

    if a.abs() < f32::EPSILON {
        // Linear in t: b*t + c = 0 ⇒ t = -c / b.
        if b.abs() < f32::EPSILON {
            return None;
        }
        let t = -c / b;
        return Some(t);
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let sqrt_disc = disc.sqrt();
    let t1 = (-b + sqrt_disc) / (2.0 * a);
    let t2 = (-b - sqrt_disc) / (2.0 * a);
    // Pick the larger root that gives a non-negative effective radius.
    let candidates = [t1, t2];
    candidates
        .iter()
        .copied()
        .filter(|&t| ir + t * r_d >= 0.0)
        .fold(None::<f32>, |acc, t| match acc {
            None => Some(t),
            Some(prev) => Some(prev.max(t)),
        })
}

fn allocate_image(
    vk: &VkContext,
    extent: vk::Extent2D,
) -> Result<(vk::Image, vk::ImageView, vk::DeviceMemory), GradientError> {
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { vk.device.create_image(&image_info, None)? };

    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let mt = (0..mem_props.memory_type_count).find(|&i| {
        mem_reqs.memory_type_bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    });
    let mt = match mt {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(GradientError::NoMemoryType);
        }
    };
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mt)
        .push_next(&mut dedicated);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory_as(
        &vk.device,
        &alloc_info,
        crate::kms::vk::mem_accounting::MemCategory::Other,
        crate::kms::vk::mem_accounting::ChurnClass::Gradient,
        &mem_props,
    ) {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e.into());
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
            vk.device.destroy_image(image, None);
        }
        return Err(e.into());
    }
    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let view = match unsafe { vk.device.create_image_view(&view_info, None) } {
        Ok(v) => v,
        Err(e) => {
            unsafe {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                vk.device.destroy_image(image, None);
            }
            return Err(e.into());
        }
    };
    Ok((image, view, memory))
}

impl GradientPicture {
    /// Record the picture's one-time initialization into `cb`: the image
    /// goes `UNDEFINED → TRANSFER_DST`, receives the whole extent from
    /// `buffer` at `offset` (tightly packed BGRA, as returned by the
    /// constructor), and ends in `SHADER_READ_ONLY_OPTIMAL` with the copy
    /// made visible to fragment-shader sampling. The barrier's second
    /// scope covers every later command on the queue, so a sampler in
    /// this command buffer or in any later submission is ordered after
    /// the copy. Record it exactly once, before the first sampler.
    pub fn record_initial_upload(
        &self,
        device: &ash::Device,
        cb: vk::CommandBuffer,
        buffer: vk::Buffer,
        offset: vk::DeviceSize,
    ) {
        let image = self.0.image;
        let extent = self.0.extent;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let to_dst = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(image)
            .subresource_range(range)];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        let regions = [vk::BufferImageCopy::default()
            .buffer_offset(offset)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D::default())
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })];
        unsafe {
            crate::vk_count!(cmd_copy_buffer_to_image);
            device.cmd_copy_buffer_to_image(
                cb,
                buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
        }
        let to_read = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(image)
            .subresource_range(range)];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
        crate::vk_count!(cmd_pipeline_barrier2);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_alpha_zero_zeros_rgb() {
        assert_eq!(premultiply(65535, 65535, 65535, 0), (0, 0, 0));
    }

    #[test]
    fn premultiply_alpha_full_is_identity() {
        assert_eq!(
            premultiply(0x4321, 0x1234, 0xabcd, 65535),
            (0x4321, 0x1234, 0xabcd)
        );
    }

    #[test]
    fn premultiply_half_alpha_halves_rgb() {
        // 0x8000 / 65535 ≈ 0.500008; rounded division should land
        // within 1 unit of half the input.
        let (r, g, b) = premultiply(60000, 40000, 20000, 0x8000);
        assert!((r as i32 - 30000).abs() <= 1, "r={r}");
        assert!((g as i32 - 20000).abs() <= 1, "g={g}");
        assert!((b as i32 - 10000).abs() <= 1, "b={b}");
    }

    #[test]
    fn premultiply_matches_rendercheck_gradient_pixel() {
        // t_gradient.c stop_list[3]: stops {0., {0,0,1.0,0}}, {.5, {0,1.0,0,.75}}, {1., {1.0,0,0,.5}}.
        // After interpolation at t = 1.0 the colour is (1.0, 0, 0, 0.5);
        // rendercheck's gradientPixel premultiplies → (0.5, 0, 0, 0.5).
        // Our LUT-fill must produce the same byte values (top-byte of u16).
        let r = 65535_u16;
        let g = 0_u16;
        let b = 0_u16;
        let a = 32768_u16; // ~0.5
        let (r_pm, g_pm, b_pm) = premultiply(r, g, b, a);
        // Expect ≈ 0.5 in each premultiplied channel.
        let r_byte = (r_pm >> 8) as u8;
        let a_byte = (a >> 8) as u8;
        assert_eq!(g_pm, 0);
        assert_eq!(b_pm, 0);
        assert!(
            (i32::from(r_byte) - i32::from(a_byte)).abs() <= 1,
            "r_byte={r_byte} a_byte={a_byte}"
        );
    }
}
