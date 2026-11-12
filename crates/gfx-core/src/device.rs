//! Vulkan instance, device and descriptor heap setup.
//!
//! Everything here is internal. Nothing in this module is reachable from a
//! script except through the checked calls in [`crate::gpu`].

use std::ffi::{CStr, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use vulkan::{Device, Entry, Instance, vk};
use vulkan_alloc::{
    Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc,
    MemoryLocation,
};

use crate::error::{ErrorCode, GfxError, Result, bail};
use crate::format::{Format, FormatCaps};

static VALIDATION_ERRORS: AtomicU32 = AtomicU32::new(0);
static VALIDATION_MESSAGES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Number of validation-layer errors and warnings seen by this process.
pub fn validation_error_count() -> u32 {
    VALIDATION_ERRORS.load(Ordering::SeqCst)
}

/// The first validation messages seen by this process (capped).
pub fn validation_messages() -> Vec<String> {
    VALIDATION_MESSAGES.lock().expect("not poisoned").clone()
}

unsafe extern "system" fn debug_callback(
    _severity: vk::DebugUtilsMessageSeverityFlagBitsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    _user_data: *mut c_void,
) -> u32 {
    VALIDATION_ERRORS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the layer passes a valid callback data pointer for the duration of the call.
    let message = unsafe {
        if data.is_null() || (*data).p_message.is_null() {
            String::from("<no message>")
        } else {
            CStr::from_ptr((*data).p_message)
                .to_string_lossy()
                .into_owned()
        }
    };
    eprintln!("[vulkan validation] {message}");
    if let Ok(mut messages) = VALIDATION_MESSAGES.lock()
        && messages.len() < 64
    {
        messages.push(message);
    }
    vk::FALSE
}

/// One of the two descriptor heaps.
pub(crate) struct Heap {
    pub buffer: vk::Buffer,
    pub allocation: Allocation,
    pub mapped: *mut u8,
    pub address: u64,
    /// Total size bound as the heap, including the reserved range.
    pub size: u64,
    /// Bytes from one slot to the next.
    pub stride: u64,
    pub slots: u32,
    pub reserved_offset: u64,
    pub reserved_size: u64,
}

pub(crate) struct DeviceLimits {
    pub max_push_data_size: u32,
    pub max_image_dimension: u32,
    pub max_buffer_size: u64,
    pub max_color_attachments: u32,
    pub max_compute_group_count: [u32; 3],
    pub largest_heap_bytes: u64,
    pub sampler_descriptor_size: u64,
    pub image_descriptor_size: u64,
    pub buffer_descriptor_size: u64,
}

/// The Vulkan objects that live as long as a `Gpu`.
pub(crate) struct DeviceCtx {
    pub _entry: Entry,
    pub instance: Instance,
    pub messenger: vk::DebugUtilsMessengerEXT,
    pub device: Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub allocator: Option<Allocator>,
    pub device_name: String,
    pub driver_name: String,
    /// Whether the device is a software rasterizer.
    pub is_cpu: bool,
    /// The DRM primary node of the device (major, minor), if it has one.
    pub drm_primary: Option<(u32, u32)>,
    /// The DRM render node of the device (major, minor), if it has one.
    pub drm_render: Option<(u32, u32)>,
    /// Whether images can be exported as dma-bufs with a format modifier.
    pub dmabuf_export: bool,
    /// Whether dma-bufs from other devices and processes can be imported.
    pub dmabuf_import: bool,
    /// What can be imported (see `dmabuf.rs`).
    pub dmabuf_formats: Vec<crate::dmabuf::DmabufFormat>,
    pub physical_device: vk::PhysicalDevice,
    pub limits: DeviceLimits,
    pub format_caps: Vec<FormatCaps>,
    pub resource_heap: Heap,
    pub sampler_heap: Heap,
}

pub(crate) fn vk_error(context: &str, result: vk::VkResult) -> GfxError {
    let code = match result {
        vk::VkResult::ERROR_DEVICE_LOST => ErrorCode::DeviceLost,
        vk::VkResult::ERROR_OUT_OF_DEVICE_MEMORY | vk::VkResult::ERROR_OUT_OF_HOST_MEMORY => {
            ErrorCode::OutOfMemory
        }
        _ => ErrorCode::Internal,
    };
    GfxError::new(code, format!("{context}: {result:?}"))
}

fn has_extension(properties: &[vk::ExtensionProperties], name: &CStr) -> bool {
    properties
        .iter()
        .any(|p| p.extension_name_as_c_str() == Ok(name))
}

fn align_up(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment.max(1)) * alignment.max(1)
}

fn lcm(a: u64, b: u64) -> u64 {
    fn gcd(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    }
    let (a, b) = (a.max(1), b.max(1));
    a / gcd(a, b) * b
}

impl DeviceCtx {
    pub fn create(
        validation: bool,
        device_filter: Option<&str>,
        drm_node: Option<(u32, u32)>,
        resource_slots: u32,
        sampler_slots: u32,
    ) -> Result<Self> {
        // SAFETY: the calls below follow the valid usage of instance and device
        // creation; every handle is created here and destroyed in `destroy`.
        unsafe {
            let entry = Entry::load().map_err(|e| {
                GfxError::new(ErrorCode::Unsupported, format!("Vulkan loader: {e}"))
            })?;

            let mut layers = Vec::new();
            let mut extensions = Vec::new();
            if validation {
                let layer = c"VK_LAYER_KHRONOS_validation";
                let available = entry
                    .enumerate_instance_layer_properties()
                    .map_err(|e| vk_error("enumerate layers", e))?;
                if !available
                    .iter()
                    .any(|l| l.layer_name_as_c_str() == Ok(layer))
                {
                    bail!(
                        Unsupported,
                        "validation was requested but VK_LAYER_KHRONOS_validation is not installed"
                    );
                }
                layers.push(vk::CStrPtr::new(layer));
                extensions.push(vk::CStrPtr::new(vk::EXT_DEBUG_UTILS_NAME));
            }
            let app = vk::ApplicationInfo::default()
                .application_name(c"hizukuri-gfx-spike")
                .api_version(vk::make_api_version(0, 1, 4, 0));
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default()
                        .application_info(&app)
                        .enabled_layer_names(&layers)
                        .enabled_extension_names(&extensions),
                    None,
                )
                .map_err(|e| vk_error("create instance", e))?;

            let messenger = if validation {
                let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
                    .message_severity(
                        vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                            | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
                    )
                    .message_type(
                        vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                            | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION,
                    )
                    .pfn_user_callback(Some(debug_callback));
                match instance.create_debug_utils_messenger_ext(&info, None) {
                    Ok(messenger) => messenger,
                    Err(e) => {
                        instance.destroy_instance(None);
                        return Err(vk_error("create debug messenger", e));
                    }
                }
            } else {
                vk::DebugUtilsMessengerEXT::null()
            };

            let cleanup_instance = |instance: &Instance| {
                if !messenger.is_null() {
                    instance.destroy_debug_utils_messenger_ext(messenger, None);
                }
                instance.destroy_instance(None);
            };

            let picked = match pick_physical_device(&instance, device_filter, drm_node) {
                Ok(picked) => picked,
                Err(e) => {
                    cleanup_instance(&instance);
                    return Err(e);
                }
            };

            let priorities = [1.0_f32];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(picked.queue_family)
                .queue_priorities(&priorities)];
            let mut extension_names = vec![
                vk::CStrPtr::new(vk::EXT_DESCRIPTOR_HEAP_NAME),
                vk::CStrPtr::new(vk::KHR_SHADER_UNTYPED_POINTERS_NAME),
            ];
            // Optional: exporting an image as a dma-buf, for scanout.
            if picked.dmabuf_export {
                extension_names
                    .extend(DMABUF_EXPORT_EXTENSIONS.iter().map(|e| vk::CStrPtr::new(e)));
            }
            // Optional: importing the dma-bufs of clients.
            if picked.dmabuf_import {
                extension_names.push(vk::CStrPtr::new(vk::EXT_QUEUE_FAMILY_FOREIGN_NAME));
            }
            // Robust access turns out-of-bounds reads and writes in shaders and
            // in vertex fetch into defined behaviour. The API cannot check those
            // on the CPU, so the device has to.
            let features = vk::PhysicalDeviceFeatures::default()
                .robust_buffer_access(true)
                .shader_sampled_image_array_dynamic_indexing(true);
            let mut heap_features =
                vk::PhysicalDeviceDescriptorHeapFeaturesEXT::default().descriptor_heap(true);
            let mut untyped_features =
                vk::PhysicalDeviceShaderUntypedPointersFeaturesKHR::default()
                    .shader_untyped_pointers(true);
            let mut features12 = vk::PhysicalDeviceVulkan12Features::default()
                .buffer_device_address(true)
                .shader_sampled_image_array_non_uniform_indexing(true);
            let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
                .synchronization2(true)
                .dynamic_rendering(true)
                .robust_image_access(true);
            let mut features14 = vk::PhysicalDeviceVulkan14Features::default().maintenance5(true);
            let device = match instance.create_device(
                picked.physical_device,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queues)
                    .enabled_extension_names(&extension_names)
                    .enabled_features(&features)
                    .push_next(&mut heap_features)
                    .push_next(&mut untyped_features)
                    .push_next(&mut features12)
                    .push_next(&mut features13)
                    .push_next(&mut features14),
                None,
            ) {
                Ok(device) => device,
                Err(e) => {
                    cleanup_instance(&instance);
                    return Err(vk_error("create device", e));
                }
            };
            let queue = device.get_device_queue(picked.queue_family, 0);

            let memory_properties =
                instance.get_physical_device_memory_properties(picked.physical_device);
            let mut allocator = Allocator::new(AllocatorCreateDesc::new(
                &device,
                memory_properties,
                picked.properties,
            ));
            let largest_heap_bytes = memory_properties
                .memory_heaps_as_slice()
                .iter()
                .map(|h| h.size)
                .max()
                .unwrap_or(0);

            let format_caps = Format::ALL
                .iter()
                .map(|f| {
                    FormatCaps::from_features(
                        instance
                            .get_physical_device_format_properties(picked.physical_device, f.vk())
                            .optimal_tiling_features,
                    )
                })
                .collect();

            let hp = &picked.heap_properties;
            let resource_stride = align_up(
                hp.image_descriptor_size.max(hp.buffer_descriptor_size),
                lcm(
                    hp.image_descriptor_alignment,
                    hp.buffer_descriptor_alignment,
                ),
            );
            let sampler_stride =
                align_up(hp.sampler_descriptor_size, hp.sampler_descriptor_alignment);
            let heaps = (|| {
                let resource_heap = create_heap(
                    &device,
                    &mut allocator,
                    resource_slots,
                    resource_stride,
                    lcm(
                        hp.image_descriptor_alignment,
                        hp.buffer_descriptor_alignment,
                    ),
                    hp.resource_heap_alignment,
                    hp.min_resource_heap_reserved_range,
                    hp.max_resource_heap_size,
                    "resource",
                )?;
                match create_heap(
                    &device,
                    &mut allocator,
                    sampler_slots,
                    sampler_stride,
                    hp.sampler_descriptor_alignment,
                    hp.sampler_heap_alignment,
                    hp.min_sampler_heap_reserved_range,
                    hp.max_sampler_heap_size,
                    "sampler",
                ) {
                    Ok(sampler_heap) => Ok((resource_heap, sampler_heap)),
                    Err(e) => {
                        destroy_heap(&device, &mut allocator, resource_heap);
                        Err(e)
                    }
                }
            })();
            let (resource_heap, sampler_heap) = match heaps {
                Ok(heaps) => heaps,
                Err(e) => {
                    drop(allocator);
                    device.destroy_device(None);
                    cleanup_instance(&instance);
                    return Err(e);
                }
            };

            let limits = &picked.properties.limits;
            Ok(Self {
                _entry: entry,
                instance,
                messenger,
                device,
                queue,
                queue_family: picked.queue_family,
                allocator: Some(allocator),
                device_name: picked.device_name,
                driver_name: picked.driver_name,
                is_cpu: picked.properties.device_type == vk::PhysicalDeviceType::CPU,
                drm_primary: picked.drm_primary,
                drm_render: picked.drm_render,
                dmabuf_export: picked.dmabuf_export,
                dmabuf_import: picked.dmabuf_import,
                // Filled in by the caller once the context exists.
                dmabuf_formats: Vec::new(),
                physical_device: picked.physical_device,
                limits: DeviceLimits {
                    max_push_data_size: hp.max_push_data_size.min(u64::from(u32::MAX)) as u32,
                    max_image_dimension: limits.max_image_dimension2_d,
                    max_buffer_size: picked.max_buffer_size,
                    max_color_attachments: limits.max_color_attachments,
                    max_compute_group_count: limits.max_compute_work_group_count,
                    largest_heap_bytes,
                    sampler_descriptor_size: hp.sampler_descriptor_size,
                    image_descriptor_size: hp.image_descriptor_size,
                    buffer_descriptor_size: hp.buffer_descriptor_size,
                },
                format_caps,
                resource_heap,
                sampler_heap,
            })
        }
    }

    pub fn allocator(&mut self) -> &mut Allocator {
        self.allocator
            .as_mut()
            .expect("allocator lives until destroy")
    }

    /// Destroys the heaps, the device and the instance. Every other object must
    /// already be gone.
    ///
    /// # Safety
    ///
    /// The GPU must be idle and no object created from the device may remain.
    pub unsafe fn destroy(mut self) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let mut allocator = self
                .allocator
                .take()
                .expect("allocator lives until destroy");
            destroy_heap(&self.device, &mut allocator, self.resource_heap);
            destroy_heap(&self.device, &mut allocator, self.sampler_heap);
            drop(allocator);
            self.device.destroy_device(None);
            if !self.messenger.is_null() {
                self.instance
                    .destroy_debug_utils_messenger_ext(self.messenger, None);
            }
            self.instance.destroy_instance(None);
        }
    }

    /// Abandons every Vulkan object without destroying it. Used when the GPU
    /// did not come back: destroying objects it may still use is not allowed,
    /// and the driver may block forever.
    pub fn leak(mut self) {
        if let Some(allocator) = self.allocator.take() {
            std::mem::forget(allocator);
        }
    }
}

struct Picked {
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
    properties: vk::PhysicalDeviceProperties,
    heap_properties: vk::PhysicalDeviceDescriptorHeapPropertiesEXT<'static>,
    max_buffer_size: u64,
    device_name: String,
    driver_name: String,
    drm_primary: Option<(u32, u32)>,
    drm_render: Option<(u32, u32)>,
    dmabuf_export: bool,
    dmabuf_import: bool,
}

/// Device extensions needed to export an image as a dma-buf.
const DMABUF_EXPORT_EXTENSIONS: [&CStr; 3] = [
    vk::KHR_EXTERNAL_MEMORY_FD_NAME,
    vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME,
    vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
];

unsafe fn pick_physical_device(
    instance: &Instance,
    filter: Option<&str>,
    drm_node: Option<(u32, u32)>,
) -> Result<Picked> {
    // SAFETY: `instance` is valid; the queries only read.
    unsafe {
        let mut best: Option<(u32, Picked)> = None;
        let mut rejected = Vec::new();
        for physical_device in instance
            .enumerate_physical_devices()
            .map_err(|e| vk_error("enumerate physical devices", e))?
        {
            let properties = instance.get_physical_device_properties(physical_device);
            let device_name = properties
                .device_name_as_c_str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(filter) = filter
                && !device_name.to_lowercase().contains(&filter.to_lowercase())
            {
                continue;
            }
            if properties.api_version < vk::make_api_version(0, 1, 4, 0) {
                rejected.push(format!("{device_name}: Vulkan 1.4 is required"));
                continue;
            }
            let extensions = instance
                .enumerate_device_extension_properties(physical_device, None)
                .map_err(|e| vk_error("enumerate device extensions", e))?;
            if !has_extension(&extensions, vk::EXT_DESCRIPTOR_HEAP_NAME)
                || !has_extension(&extensions, vk::KHR_SHADER_UNTYPED_POINTERS_NAME)
            {
                rejected.push(format!("{device_name}: VK_EXT_descriptor_heap is required"));
                continue;
            }

            let mut heap_features = vk::PhysicalDeviceDescriptorHeapFeaturesEXT::default();
            let mut untyped_features =
                vk::PhysicalDeviceShaderUntypedPointersFeaturesKHR::default();
            let mut features12 = vk::PhysicalDeviceVulkan12Features::default();
            let mut features13 = vk::PhysicalDeviceVulkan13Features::default();
            let mut features14 = vk::PhysicalDeviceVulkan14Features::default();
            let (robust_buffer_access, dynamic_indexing) = {
                let mut features2 = vk::PhysicalDeviceFeatures2::default()
                    .push_next(&mut heap_features)
                    .push_next(&mut untyped_features)
                    .push_next(&mut features12)
                    .push_next(&mut features13)
                    .push_next(&mut features14);
                instance.get_physical_device_features2(physical_device, &mut features2);
                (
                    features2.features.robust_buffer_access,
                    features2
                        .features
                        .shader_sampled_image_array_dynamic_indexing,
                )
            };
            if heap_features.descriptor_heap == 0
                || untyped_features.shader_untyped_pointers == 0
                || features12.buffer_device_address == 0
                || features13.synchronization2 == 0
                || features13.dynamic_rendering == 0
                || features13.robust_image_access == 0
                || features14.maintenance5 == 0
                || robust_buffer_access == 0
                || dynamic_indexing == 0
                || features12.shader_sampled_image_array_non_uniform_indexing == 0
            {
                rejected.push(format!("{device_name}: a required feature is missing"));
                continue;
            }

            let mut heap_properties = vk::PhysicalDeviceDescriptorHeapPropertiesEXT::default();
            let mut properties13 = vk::PhysicalDeviceVulkan13Properties::default();
            let mut properties12 = vk::PhysicalDeviceVulkan12Properties::default();
            {
                let mut properties2 = vk::PhysicalDeviceProperties2::default()
                    .push_next(&mut heap_properties)
                    .push_next(&mut properties13)
                    .push_next(&mut properties12);
                instance.get_physical_device_properties2(physical_device, &mut properties2);
            }
            let driver_name = format!(
                "{} {}",
                properties12
                    .driver_name_as_c_str()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                properties12
                    .driver_info_as_c_str()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );

            let families = instance.get_physical_device_queue_family_properties(physical_device);
            let Some(queue_family) = families.iter().position(|f| {
                f.queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            }) else {
                rejected.push(format!("{device_name}: no graphics+compute queue"));
                continue;
            };
            let dmabuf_export = DMABUF_EXPORT_EXTENSIONS
                .iter()
                .all(|e| has_extension(&extensions, e));
            // Importing needs the same extensions, and a way to take images
            // over from a foreign owner.
            let dmabuf_import =
                dmabuf_export && has_extension(&extensions, vk::EXT_QUEUE_FAMILY_FOREIGN_NAME);
            let (drm_primary, drm_render) =
                if has_extension(&extensions, vk::EXT_PHYSICAL_DEVICE_DRM_NAME) {
                    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                    {
                        let mut properties2 =
                            vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
                        instance.get_physical_device_properties2(physical_device, &mut properties2);
                    }
                    (
                        (drm.has_primary != 0)
                            .then_some((drm.primary_major as u32, drm.primary_minor as u32)),
                        (drm.has_render != 0)
                            .then_some((drm.render_major as u32, drm.render_minor as u32)),
                    )
                } else {
                    (None, None)
                };
            let mut score = match properties.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            if drm_node.is_some() && drm_primary == drm_node {
                score += 10;
            }
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((
                    score,
                    Picked {
                        physical_device,
                        queue_family: queue_family as u32,
                        properties,
                        heap_properties: vk::PhysicalDeviceDescriptorHeapPropertiesEXT {
                            p_next: std::ptr::null_mut(),
                            ..heap_properties
                        },
                        max_buffer_size: properties13.max_buffer_size,
                        device_name,
                        driver_name,
                        drm_primary,
                        drm_render,
                        dmabuf_export,
                        dmabuf_import,
                    },
                ));
            }
        }
        match best {
            Some((_, picked)) => Ok(picked),
            None => Err(GfxError::new(
                ErrorCode::Unsupported,
                format!(
                    "no usable Vulkan device (filter: {filter:?}; rejected: {})",
                    rejected.join("; ")
                ),
            )),
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_heap(
    device: &Device,
    allocator: &mut Allocator,
    slots: u32,
    stride: u64,
    descriptor_alignment: u64,
    heap_alignment: u64,
    reserved_size: u64,
    max_size: u64,
    name: &str,
) -> Result<Heap> {
    let reserved_offset = align_up(u64::from(slots) * stride, descriptor_alignment);
    let size = align_up(reserved_offset + reserved_size, heap_alignment);
    if size > max_size {
        bail!(
            LimitExceeded,
            "{name} heap with {slots} slots needs {size} bytes, the device allows {max_size}"
        );
    }
    // SAFETY: `device` is valid; the buffer and its memory are owned by the returned heap.
    unsafe {
        let buffer = device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(
                        vk::BufferUsageFlags::DESCRIPTOR_HEAP_EXT
                            | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
            .map_err(|e| vk_error("create heap buffer", e))?;
        let requirements = device.get_buffer_memory_requirements(buffer);
        let allocation = match allocator.allocate(AllocationCreateDesc {
            name: Some(name),
            requirements,
            location: MemoryLocation::CpuToGpu,
            linear: true,
            memory_allocate_flags: vk::MemoryAllocateFlags::DEVICE_ADDRESS,
            export_handle_types: vk::ExternalMemoryHandleTypeFlags::empty(),
            allocation_scheme: AllocationScheme::DedicatedBuffer(buffer),
        }) {
            Ok(allocation) => allocation,
            Err(e) => {
                device.destroy_buffer(buffer, None);
                return Err(GfxError::new(
                    ErrorCode::OutOfMemory,
                    format!("allocate {name} heap: {e}"),
                ));
            }
        };
        let fail = |allocator: &mut Allocator, allocation: Allocation, error: GfxError| {
            let _ = allocator.free(allocation);
            device.destroy_buffer(buffer, None);
            Err(error)
        };
        if let Err(e) = device.bind_buffer_memory(buffer, allocation.memory(), allocation.offset())
        {
            return fail(allocator, allocation, vk_error("bind heap memory", e));
        }
        let mapped = match allocator.map_memory(&allocation) {
            Ok(mapped) => mapped.cast::<u8>(),
            Err(e) => {
                return fail(
                    allocator,
                    allocation,
                    GfxError::new(ErrorCode::Internal, format!("map {name} heap: {e}")),
                );
            }
        };
        let address = device
            .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer));
        if !address.is_multiple_of(heap_alignment.max(1)) {
            let _ = allocator.unmap_memory(&allocation);
            return fail(
                allocator,
                allocation,
                GfxError::new(
                    ErrorCode::Internal,
                    format!("{name} heap address is not aligned to {heap_alignment}"),
                ),
            );
        }
        // Start from zeroed memory so that no slot holds leftover bytes.
        std::ptr::write_bytes(mapped, 0, size as usize);
        Ok(Heap {
            buffer,
            allocation,
            mapped,
            address,
            size,
            stride,
            slots,
            reserved_offset,
            reserved_size,
        })
    }
}

unsafe fn destroy_heap(device: &Device, allocator: &mut Allocator, heap: Heap) {
    // SAFETY: the caller guarantees the heap is no longer bound to a pending command buffer.
    unsafe {
        let _ = allocator.unmap_memory(&heap.allocation);
        device.destroy_buffer(heap.buffer, None);
        let _ = allocator.free(heap.allocation);
    }
}
