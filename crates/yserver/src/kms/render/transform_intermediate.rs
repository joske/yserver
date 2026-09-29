//! The footprint-sized intermediate a RANDR-transformed output composites
//! into before the scale pass (spec D4). One per transformed output, in root
//! space with its `(0, 0)` at the CRTC origin; freed when the output returns
//! to identity or goes away.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::{
    device::VkContext,
    mem_accounting::{self, MemCategory},
    scale_pipeline::{ScalePassPipeline, ScalePushConsts},
};

/// The format every compose attachment uses (`CompositorPipeline`).
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

pub(crate) struct TransformIntermediate {
    vk: Arc<VkContext>,
    pub(crate) image: vk::Image,
    pub(crate) view: vk::ImageView,
    memory: vk::DeviceMemory,
    pub(crate) extent: vk::Extent2D,
    descriptor_pool: vk::DescriptorPool,
    /// The scale pass's sampler binding of `view`, in `GENERAL`.
    pub(crate) descriptor_set: vk::DescriptorSet,
    /// A compose has been submitted into it, leaving it in `GENERAL`; before
    /// that it is undefined and must not be read back.
    pub(crate) has_content: bool,
}

impl TransformIntermediate {
    pub(crate) fn new(
        vk: Arc<VkContext>,
        extent: vk::Extent2D,
        pipeline: &ScalePassPipeline,
    ) -> Result<Self, vk::Result> {
        let device = &vk.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width: extent.width.max(1),
                height: extent.height.max(1),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            // TRANSFER_SRC: root GetImage reads root pixels from here (D6).
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&info, None)? };
        let requirements = unsafe { device.get_image_memory_requirements(image) };
        let properties = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let Some(memory_type_index) = (0..properties.memory_type_count).find(|&index| {
            requirements.memory_type_bits & (1 << index) != 0
                && properties.memory_types[index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        }) else {
            unsafe { device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match mem_accounting::allocate_memory(
            device,
            &allocation,
            MemCategory::Transform,
            &properties,
        ) {
            Ok(memory) => memory,
            Err(error) => {
                unsafe { device.destroy_image(image, None) };
                return Err(error);
            }
        };
        let destroy_image = |device: &ash::Device| unsafe {
            device.destroy_image(image, None);
            mem_accounting::free_memory(device, memory);
        };
        if let Err(error) = unsafe { device.bind_image_memory(image, memory, 0) } {
            destroy_image(device);
            return Err(error);
        }
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(FORMAT)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let view = match unsafe { device.create_image_view(&view_info, None) } {
            Ok(view) => view,
            Err(error) => {
                destroy_image(device);
                return Err(error);
            }
        };
        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)];
        let descriptor_pool = match unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )
        } {
            Ok(pool) => pool,
            Err(error) => {
                unsafe { device.destroy_image_view(view, None) };
                destroy_image(device);
                return Err(error);
            }
        };
        let layouts = [pipeline.descriptor_set_layout];
        let descriptor_set = match unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&layouts),
            )
        } {
            Ok(sets) => sets[0],
            Err(error) => {
                unsafe {
                    device.destroy_descriptor_pool(descriptor_pool, None);
                    device.destroy_image_view(view, None);
                }
                destroy_image(device);
                return Err(error);
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(view)
            .sampler(pipeline.sampler)
            .image_layout(vk::ImageLayout::GENERAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(Self {
            vk,
            image,
            view,
            memory,
            extent,
            descriptor_pool,
            descriptor_set,
            has_content: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn memory(&self) -> vk::DeviceMemory {
        self.memory
    }
}

impl Drop for TransformIntermediate {
    /// The owner waits for every compose that used this image first.
    fn drop(&mut self) {
        unsafe {
            let device = &self.vk.device;
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            mem_accounting::free_memory(device, self.memory);
        }
    }
}

/// Footprint ∩ root, intermediate-local: the only part of the intermediate
/// the scene composites (spec D3 "Two extents"). `None` when the root does
/// not reach the footprint at all.
pub(crate) fn composite_rect(
    footprint: (i32, i32, u32, u32),
    root: (u32, u32),
) -> Option<vk::Rect2D> {
    let (x, y, w, h) = footprint;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = x
        .saturating_add_unsigned(w)
        .min(i32::try_from(root.0).unwrap_or(i32::MAX));
    let y1 = y
        .saturating_add_unsigned(h)
        .min(i32::try_from(root.1).unwrap_or(i32::MAX));
    (x1 > x0 && y1 > y0).then(|| vk::Rect2D {
        offset: vk::Offset2D {
            x: x0 - x,
            y: y0 - y,
        },
        extent: vk::Extent2D {
            width: (x1 - x0).unsigned_abs(),
            height: (y1 - y0).unsigned_abs(),
        },
    })
}

/// The scale pass constants for `transform` over an intermediate of
/// `extent`. `transform` is a pure scale (spec D2); a missing filter samples
/// nearest, as Xorg's default picture filter.
pub(crate) fn scale_push(
    transform: &yserver_core::randr::CrtcTransform,
    extent: vk::Extent2D,
) -> ScalePushConsts {
    use yserver_core::randr::Filter;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    ScalePushConsts {
        matrix: [transform.matrix[0] as u32, transform.matrix[4] as u32],
        scale: [transform.forward[0] as f32, transform.forward[4] as f32],
        src_size: [extent.width as f32, extent.height as f32],
        nearest: u32::from(!matches!(transform.filter, Some(Filter::Bilinear))),
        _pad: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    #[test]
    fn composite_rect_is_footprint_and_root_intermediate_local() {
        // Scale-down 100%: CRTC 6 at 2560,0, footprint 5120×2880, root 7680×2880.
        assert_eq!(
            composite_rect((2560, 0, 5120, 2880), (7680, 2880)),
            Some(r(0, 0, 5120, 2880))
        );
        // A root cropping the footprint (spec Q3): 3840 wide, 1440 high.
        assert_eq!(
            composite_rect((1920, 0, 3840, 2880), (3840, 1440)),
            Some(r(0, 0, 1920, 1440))
        );
        // A root that does not reach the CRTC at all.
        assert_eq!(composite_rect((2560, 0, 2048, 1152), (2000, 1152)), None);
    }
}
