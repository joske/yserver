//! Why there is no usable Vulkan device, for the error a user sees when the
//! server cannot start: no driver, the Basic Display Adapter, a GPU whose
//! driver is too old, a VM.

use std::io;

use ash::vk;

/// Where to get the software fallback.
const FALLBACK: &str = "Without a usable GPU, Mesa's software renderer lavapipe works: \
     get mesa-dist-win (https://github.com/pal1000/mesa-dist-win), and set \
     VK_DRIVER_FILES to the full path of the lvp_icd json file in its folder for this CPU";

/// `err` with its cause named and the fallback, if it is a Vulkan error.
pub(super) fn explain(err: io::Error) -> io::Error {
    if !err.to_string().starts_with("vulkan") {
        return err;
    }
    io::Error::new(err.kind(), format!("{err}. {}. {FALLBACK}", diagnose()))
}

/// What the loader finds.
fn diagnose() -> String {
    // SAFETY: loads vulkan-1.dll; no Vulkan object outlives this function.
    let entry = match unsafe { ash::Entry::load() } {
        Ok(entry) => entry,
        Err(e) => {
            return format!(
                "No Vulkan loader ({e}): no GPU driver with Vulkan support is installed"
            );
        }
    };
    // SAFETY: a loaded entry; the instance is destroyed below.
    let instance = match unsafe { entry.create_instance(&vk::InstanceCreateInfo::default(), None) }
    {
        Ok(instance) => instance,
        Err(e) => {
            return format!(
                "No Vulkan driver ({e}): the GPU driver has no Vulkan support, or this is the \
                 Microsoft Basic Display Adapter or a VM without a Vulkan GPU"
            );
        }
    };
    // SAFETY: a valid instance; destroyed once, after its last use.
    let devices = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
    let listing = devices
        .iter()
        .map(|&pd| {
            // SAFETY: a device of this instance.
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let name = props
                .device_name_as_c_str()
                .map_or_else(|_| "<unnamed>".into(), |s| s.to_string_lossy().into_owned());
            let v = props.api_version;
            format!(
                "{name} (Vulkan {}.{}.{})",
                vk::api_version_major(v),
                vk::api_version_minor(v),
                vk::api_version_patch(v)
            )
        })
        .collect::<Vec<_>>();
    // SAFETY: see above.
    unsafe { instance.destroy_instance(None) };
    if listing.is_empty() {
        "The Vulkan driver reports no GPU".to_owned()
    } else {
        format!(
            "Vulkan devices: {}; yserver needs Vulkan 1.3, or 1.2 with \
             VK_KHR_dynamic_rendering and VK_KHR_synchronization2, which a newer GPU driver \
             may add",
            listing.join(", ")
        )
    }
}
