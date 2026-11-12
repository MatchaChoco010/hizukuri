//! Vulkan instance and device creation, and the choice of physical device.

use std::ffi::{CStr, c_char, c_void};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use vulkan::{Device, Entry, Instance, vk};

use crate::RenderError;

/// Which physical device the renderer should use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceSelector {
    /// Prefer a discrete GPU, then an integrated GPU, then anything else.
    #[default]
    Auto,
    /// A hardware GPU (discrete, integrated or virtual). Fails when only a
    /// software rasterizer is available.
    Gpu,
    /// A software rasterizer (device type `CPU`, e.g. lavapipe).
    Cpu,
    /// A device whose name, driver name or driver description contains this
    /// text (case-insensitive). `lavapipe` also matches Mesa's `llvmpipe`.
    Name(String),
    /// The device that drives a DRM device node, given by the device number
    /// of the node (primary or render). This is how a KMS backend asks for
    /// "the GPU this card belongs to".
    DrmNode { major: u32, minor: u32 },
}

impl FromStr for DeviceSelector {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" => DeviceSelector::Auto,
            "gpu" => DeviceSelector::Gpu,
            "cpu" => DeviceSelector::Cpu,
            other => DeviceSelector::Name(other.to_string()),
        })
    }
}

/// The kind of a physical device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Discrete,
    Integrated,
    Virtual,
    Cpu,
    Other,
}

impl DeviceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceKind::Discrete => "discrete",
            DeviceKind::Integrated => "integrated",
            DeviceKind::Virtual => "virtual",
            DeviceKind::Cpu => "cpu",
            DeviceKind::Other => "other",
        }
    }
}

/// Description of a physical device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
    pub driver_name: String,
    pub driver_info: String,
    /// `(major, minor, patch)` of the Vulkan version the device supports.
    pub api_version: (u32, u32, u32),
    /// Device number `(major, minor)` of the DRM primary node (`cardN`) the
    /// device drives, from `VK_EXT_physical_device_drm`. `None` for devices
    /// without one (software rasterizers) or drivers without the extension.
    pub drm_primary: Option<(u32, u32)>,
    /// Device number of the DRM render node (`renderDN`).
    pub drm_render: Option<(u32, u32)>,
}

impl DeviceInfo {
    fn matches(&self, selector: &DeviceSelector) -> bool {
        match selector {
            DeviceSelector::Auto => true,
            DeviceSelector::Gpu => matches!(
                self.kind,
                DeviceKind::Discrete | DeviceKind::Integrated | DeviceKind::Virtual
            ),
            DeviceSelector::Cpu => self.kind == DeviceKind::Cpu,
            DeviceSelector::Name(text) => {
                let haystack = format!("{} {} {}", self.name, self.driver_name, self.driver_info)
                    .to_ascii_lowercase();
                let needle = text.to_ascii_lowercase();
                haystack.contains(&needle)
                    || (needle == "lavapipe" && haystack.contains("llvmpipe"))
            }
            DeviceSelector::DrmNode { major, minor } => {
                let node = Some((*major, *minor));
                self.drm_primary == node || self.drm_render == node
            }
        }
    }

    fn rank(&self) -> u32 {
        match self.kind {
            DeviceKind::Discrete => 4,
            DeviceKind::Integrated => 3,
            DeviceKind::Virtual => 2,
            DeviceKind::Other => 1,
            DeviceKind::Cpu => 0,
        }
    }
}

/// Finds the manifest of an installed Vulkan driver (ICD) by the prefix of
/// its file name, e.g. `"lvp"` for lavapipe.
///
/// The Vulkan loader only sees the drivers listed in `VK_DRIVER_FILES` when
/// that variable is set. To run on a driver that the environment hides, set
/// `VK_DRIVER_FILES` to the returned path before the process starts.
///
/// The directories in `HIZUKURI_ICD_DIRS` (separated by colons) are
/// searched before the usual system locations, for environments that keep
/// manifests elsewhere (a Nix store, a Mesa build tree).
pub fn find_icd_manifest(prefix: &str) -> Option<std::path::PathBuf> {
    let extra = std::env::var_os("HIZUKURI_ICD_DIRS");
    let extra = extra
        .as_deref()
        .map(|dirs| std::env::split_paths(dirs).collect::<Vec<_>>())
        .unwrap_or_default();
    find_icd_manifest_in(prefix, &icd_search_dirs(&extra))
}

/// The directories to look for manifests in, in order: the given extra
/// ones, then the system locations.
fn icd_search_dirs(extra: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    const SYSTEM: [&str; 3] = [
        "/usr/share/vulkan/icd.d",
        "/usr/local/share/vulkan/icd.d",
        "/etc/vulkan/icd.d",
    ];
    extra
        .iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .cloned()
        .chain(SYSTEM.iter().map(std::path::PathBuf::from))
        .collect()
}

fn find_icd_manifest_in(prefix: &str, dirs: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    // Multi-arch installs carry one manifest per architecture.
    const OTHER_ARCHES: [&str; 4] = ["i686", "i386", "armhf", "x86_64"];
    let foreign = |name: &str| {
        OTHER_ARCHES
            .iter()
            .filter(|arch| **arch != std::env::consts::ARCH)
            .any(|arch| name.contains(arch))
    };
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut candidates: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".json") && !foreign(n))
            })
            .collect();
        candidates.sort();
        if let Some(path) = candidates.into_iter().next() {
            return Some(path);
        }
    }
    None
}

/// Counts validation messages so callers (and tests) can assert that a run
/// was clean.
#[derive(Debug, Default)]
pub(crate) struct ValidationCounters {
    pub errors: AtomicU32,
    pub warnings: AtomicU32,
}

pub(crate) struct Gpu {
    // Field order matters for nothing here; destruction is explicit in Drop.
    _entry: Entry,
    pub instance: Instance,
    debug_messenger: vk::DebugUtilsMessengerEXT,
    pub physical_device: vk::PhysicalDevice,
    pub device: Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub info: DeviceInfo,
    /// Whether the extensions for exporting images as dma-bufs are enabled.
    pub dmabuf_export: bool,
    pub validation: Option<Arc<ValidationCounters>>,
}

const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";
const REQUIRED_API: (u32, u32) = (1, 3);
/// Device extensions needed to hand a render target to KMS as a dma-buf.
const DMABUF_EXPORT_EXTENSIONS: [&CStr; 4] = [
    vk::KHR_EXTERNAL_MEMORY_FD_NAME,
    vk::EXT_EXTERNAL_MEMORY_DMA_BUF_NAME,
    vk::EXT_IMAGE_DRM_FORMAT_MODIFIER_NAME,
    vk::EXT_QUEUE_FAMILY_FOREIGN_NAME,
];

impl Gpu {
    pub fn new(selector: &DeviceSelector, validation: bool) -> Result<Self, RenderError> {
        // SAFETY: the loader found by the platform default name is the
        // system Vulkan loader, which is ABI-compatible with the bindings.
        let entry = unsafe { Entry::load() }.map_err(|e| RenderError::Loader(e.to_string()))?;

        let mut layers: Vec<vk::CStrPtr<'static>> = Vec::new();
        let mut extensions: Vec<vk::CStrPtr<'static>> = Vec::new();
        let mut counters = None;
        if validation {
            // SAFETY: plain enumeration calls on a valid entry.
            let has_layer = unsafe { entry.enumerate_instance_layer_properties() }?
                .iter()
                .any(|l| c_chars_to_cstr(&l.layer_name) == VALIDATION_LAYER);
            // SAFETY: as above.
            let has_debug_utils = unsafe { entry.enumerate_instance_extension_properties(None) }?
                .iter()
                .any(|e| c_chars_to_cstr(&e.extension_name) == vk::EXT_DEBUG_UTILS_NAME);
            if has_layer && has_debug_utils {
                layers.push(vk::CStrPtr::new(VALIDATION_LAYER));
                extensions.push(vk::CStrPtr::new(vk::EXT_DEBUG_UTILS_NAME));
                counters = Some(Arc::new(ValidationCounters::default()));
            } else {
                return Err(RenderError::ValidationUnavailable);
            }
        }

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"hizukuri")
            .engine_name(c"proto-render")
            .api_version(vk::make_api_version(0, REQUIRED_API.0, REQUIRED_API.1, 0));
        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_layer_names(&layers)
            .enabled_extension_names(&extensions);
        // SAFETY: every pointer in create_info refers to data that outlives
        // the call.
        let instance = unsafe { entry.create_instance(&create_info, None) }?;

        let debug_messenger = match &counters {
            Some(counters) => {
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
                // The builder's `user_data` wants `&mut c_void`; the counters
                // are only ever accessed through atomics, so pass the pointer
                // through the raw field instead of forging a unique borrow.
                let mut info = info;
                info.p_user_data = Arc::as_ptr(counters) as *mut c_void;
                // SAFETY: the user data pointer stays valid because `Gpu`
                // keeps the Arc alive until the messenger is destroyed.
                match unsafe { instance.create_debug_utils_messenger_ext(&info, None) } {
                    Ok(messenger) => messenger,
                    Err(e) => {
                        // SAFETY: nothing else was created from the instance.
                        unsafe { instance.destroy_instance(None) };
                        return Err(e.into());
                    }
                }
            }
            None => vk::DebugUtilsMessengerEXT::null(),
        };

        // From here on, `partial` destroys the instance on early return.
        let partial = PartialInstance {
            instance: Some(instance),
            debug_messenger,
        };
        let instance = partial.instance.as_ref().expect("just set");

        let (physical_device, queue_family, info) = pick_physical_device(instance, selector)?;

        // Exporting a render target as a dma-buf (for KMS scanout) needs
        // these. They are optional: without them only readback works.
        // SAFETY: enumeration on a valid physical device.
        let available: Vec<std::ffi::CString> =
            unsafe { instance.enumerate_device_extension_properties(physical_device, None) }?
                .iter()
                .map(|e| c_chars_to_cstr(&e.extension_name).to_owned())
                .collect();
        let dmabuf_export = DMABUF_EXPORT_EXTENSIONS
            .iter()
            .all(|name| available.iter().any(|a| a.as_c_str() == *name));
        let device_extensions: Vec<vk::CStrPtr<'static>> = if dmabuf_export {
            DMABUF_EXPORT_EXTENSIONS
                .iter()
                .map(|name| vk::CStrPtr::new(name))
                .collect()
        } else {
            Vec::new()
        };
        tracing::info!(
            device = %info.name,
            kind = info.kind.as_str(),
            driver = %info.driver_name,
            driver_info = %info.driver_info,
            drm_primary = ?info.drm_primary,
            dmabuf_export,
            "selected Vulkan device"
        );

        let priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let create_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_extensions)
            .push_next(&mut features13);
        // SAFETY: the physical device belongs to this instance and the
        // requested features were checked in pick_physical_device.
        let device = unsafe { instance.create_device(physical_device, &create_info, None) }?;
        // SAFETY: the queue family index and queue index were requested above.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let (instance, debug_messenger) = partial.release();
        Ok(Gpu {
            _entry: entry,
            instance,
            debug_messenger,
            physical_device,
            device,
            queue,
            queue_family,
            info,
            dmabuf_export,
            validation: counters,
        })
    }

    pub fn validation_errors(&self) -> Option<u32> {
        self.validation
            .as_ref()
            .map(|c| c.errors.load(Ordering::Relaxed))
    }

    pub fn validation_warnings(&self) -> Option<u32> {
        self.validation
            .as_ref()
            .map(|c| c.warnings.load(Ordering::Relaxed))
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: the renderer destroys every object created from the device
        // before dropping `Gpu`; the messenger and instance are destroyed
        // last, in that order.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            if !self.debug_messenger.is_null() {
                self.instance
                    .destroy_debug_utils_messenger_ext(self.debug_messenger, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

/// Destroys the instance if device creation fails half way.
struct PartialInstance {
    instance: Option<Instance>,
    debug_messenger: vk::DebugUtilsMessengerEXT,
}

impl PartialInstance {
    fn release(mut self) -> (Instance, vk::DebugUtilsMessengerEXT) {
        (
            self.instance.take().expect("instance is present"),
            self.debug_messenger,
        )
    }
}

impl Drop for PartialInstance {
    fn drop(&mut self) {
        if let Some(instance) = self.instance.take() {
            // SAFETY: no device was created from this instance (or creating
            // it failed), so only the messenger remains.
            unsafe {
                if !self.debug_messenger.is_null() {
                    instance.destroy_debug_utils_messenger_ext(self.debug_messenger, None);
                }
                instance.destroy_instance(None);
            }
        }
    }
}

fn pick_physical_device(
    instance: &Instance,
    selector: &DeviceSelector,
) -> Result<(vk::PhysicalDevice, u32, DeviceInfo), RenderError> {
    let mut available = Vec::new();
    let mut best: Option<(vk::PhysicalDevice, u32, DeviceInfo)> = None;

    // SAFETY: enumeration and property queries on a valid instance.
    for physical_device in unsafe { instance.enumerate_physical_devices() }? {
        let info = unsafe { describe(instance, physical_device) };
        available.push(format!("{} ({})", info.name, info.kind.as_str()));

        if !info.matches(selector) {
            continue;
        }
        if (info.api_version.0, info.api_version.1) < REQUIRED_API {
            tracing::debug!(device = %info.name, "skipped: Vulkan 1.3 is required");
            continue;
        }
        let mut features13 = vk::PhysicalDeviceVulkan13Features::default();
        {
            let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut features13);
            // SAFETY: the chain is valid for the duration of the call.
            unsafe { instance.get_physical_device_features2(physical_device, &mut features2) };
        }
        if features13.dynamic_rendering != vk::TRUE || features13.synchronization2 != vk::TRUE {
            tracing::debug!(device = %info.name, "skipped: missing Vulkan 1.3 features");
            continue;
        }
        // SAFETY: as above.
        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let Some(queue_family) = families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        else {
            continue;
        };

        if best
            .as_ref()
            .is_none_or(|(_, _, current)| info.rank() > current.rank())
        {
            best = Some((physical_device, queue_family as u32, info));
        }
    }

    best.ok_or(RenderError::NoSuitableDevice {
        selector: selector.clone(),
        available,
    })
}

/// # Safety
///
/// `physical_device` must belong to `instance`.
unsafe fn describe(instance: &Instance, physical_device: vk::PhysicalDevice) -> DeviceInfo {
    // SAFETY: guaranteed by the caller.
    let properties = unsafe { instance.get_physical_device_properties(physical_device) };
    let api = properties.api_version;
    let api_version = (
        vk::api_version_major(api),
        vk::api_version_minor(api),
        vk::api_version_patch(api),
    );

    // SAFETY: guaranteed by the caller.
    let has_drm = unsafe { instance.enumerate_device_extension_properties(physical_device, None) }
        .is_ok_and(|extensions| {
            extensions
                .iter()
                .any(|e| c_chars_to_cstr(&e.extension_name) == vk::EXT_PHYSICAL_DEVICE_DRM_NAME)
        });
    let (drm_primary, drm_render) = if has_drm && (api_version.0, api_version.1) >= (1, 1) {
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        {
            let mut properties2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: the chain is valid for the duration of the call and
            // the device supports the extension.
            unsafe { instance.get_physical_device_properties2(physical_device, &mut properties2) };
        }
        (
            (drm.has_primary != 0).then_some((drm.primary_major as u32, drm.primary_minor as u32)),
            (drm.has_render != 0).then_some((drm.render_major as u32, drm.render_minor as u32)),
        )
    } else {
        (None, None)
    };

    let (driver_name, driver_info) = if (api_version.0, api_version.1) >= (1, 2) {
        let mut driver = vk::PhysicalDeviceDriverProperties::default();
        {
            let mut properties2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
            // SAFETY: the chain is valid for the duration of the call, and
            // the driver properties struct is core in Vulkan 1.2.
            unsafe { instance.get_physical_device_properties2(physical_device, &mut properties2) };
        }
        (
            c_chars_to_cstr(&driver.driver_name)
                .to_string_lossy()
                .into_owned(),
            c_chars_to_cstr(&driver.driver_info)
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        (String::new(), String::new())
    };

    let kind = match properties.device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => DeviceKind::Discrete,
        vk::PhysicalDeviceType::INTEGRATED_GPU => DeviceKind::Integrated,
        vk::PhysicalDeviceType::VIRTUAL_GPU => DeviceKind::Virtual,
        vk::PhysicalDeviceType::CPU => DeviceKind::Cpu,
        _ => DeviceKind::Other,
    };

    DeviceInfo {
        name: c_chars_to_cstr(&properties.device_name)
            .to_string_lossy()
            .into_owned(),
        kind,
        driver_name,
        driver_info,
        api_version,
        drm_primary,
        drm_render,
    }
}

/// Reads a NUL-terminated string out of a fixed-size `c_char` array.
fn c_chars_to_cstr(chars: &[c_char]) -> &CStr {
    // SAFETY: c_char and u8 have the same size and alignment.
    let bytes = unsafe { std::slice::from_raw_parts(chars.as_ptr().cast::<u8>(), chars.len()) };
    CStr::from_bytes_until_nul(bytes).unwrap_or(c"")
}

unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagBitsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    data: *const vk::DebugUtilsMessengerCallbackDataEXT<'_>,
    user_data: *mut c_void,
) -> u32 {
    // SAFETY: Vulkan passes a valid callback data pointer; the message is a
    // NUL-terminated string when non-null.
    let message = unsafe {
        if data.is_null() || (*data).p_message.is_null() {
            std::borrow::Cow::Borrowed("(no message)")
        } else {
            CStr::from_ptr((*data).p_message).to_string_lossy()
        }
    };
    // SAFETY: user_data is the ValidationCounters kept alive by `Gpu`.
    let counters = unsafe { (user_data as *const ValidationCounters).as_ref() };
    if severity == vk::DebugUtilsMessageSeverityFlagBitsEXT::ERROR {
        if let Some(counters) = counters {
            counters.errors.fetch_add(1, Ordering::Relaxed);
        }
        tracing::error!(target: "vulkan", "{message}");
    } else {
        if let Some(counters) = counters {
            counters.warnings.fetch_add(1, Ordering::Relaxed);
        }
        tracing::warn!(target: "vulkan", "{message}");
    }
    vk::FALSE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_icd_directories_are_searched_before_the_system_ones() {
        let base = std::env::temp_dir().join(format!("hzk-icd-{}", std::process::id()));
        let (first, second) = (base.join("first"), base.join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("lvp_icd.x86_64.json"), "{}").unwrap();
        std::fs::write(second.join("lvp_icd.i686.json"), "{}").unwrap();
        std::fs::write(second.join("other_icd.json"), "{}").unwrap();

        let dirs = icd_search_dirs(&[first.clone(), std::path::PathBuf::new(), second.clone()]);
        assert_eq!(dirs[..2], [first.clone(), second.clone()]);
        assert!(dirs.contains(&std::path::PathBuf::from("/usr/share/vulkan/icd.d")));

        // Nothing in `first`: the one in `second` wins over the system's,
        // and the manifest of another architecture is passed over.
        let native = format!("lvp_icd.{}.json", std::env::consts::ARCH);
        std::fs::write(second.join(&native), "{}").unwrap();
        assert_eq!(
            find_icd_manifest_in("lvp", &dirs),
            Some(second.join(&native))
        );
        std::fs::write(first.join("lvp_icd.json"), "{}").unwrap();
        assert_eq!(
            find_icd_manifest_in("lvp", &dirs),
            Some(first.join("lvp_icd.json"))
        );
        assert_eq!(find_icd_manifest_in("nonexistent-driver", &dirs[..2]), None);
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn info(name: &str, kind: DeviceKind, driver: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_string(),
            kind,
            driver_name: driver.to_string(),
            driver_info: String::new(),
            api_version: (1, 4, 0),
            drm_primary: None,
            drm_render: None,
        }
    }

    #[test]
    fn selector_parses_keywords_and_names() {
        assert_eq!("auto".parse(), Ok(DeviceSelector::Auto));
        assert_eq!("GPU".parse(), Ok(DeviceSelector::Gpu));
        assert_eq!("cpu".parse(), Ok(DeviceSelector::Cpu));
        assert_eq!(
            "LLVMpipe".parse(),
            Ok(DeviceSelector::Name("llvmpipe".to_string()))
        );
    }

    #[test]
    fn selector_matches_devices() {
        let hardware = info(
            "Example GPU 1234 (EXAMPLE DRIVER)",
            DeviceKind::Discrete,
            "example",
        );
        let lvp = info(
            "llvmpipe (LLVM 1.2.3, 256 bits)",
            DeviceKind::Cpu,
            "llvmpipe",
        );

        assert!(hardware.matches(&DeviceSelector::Auto));
        assert!(hardware.matches(&DeviceSelector::Gpu));
        assert!(!hardware.matches(&DeviceSelector::Cpu));
        assert!(hardware.matches(&DeviceSelector::Name("example".into())));
        assert!(hardware.matches(&DeviceSelector::Name("1234".into())));
        assert!(!hardware.matches(&DeviceSelector::Name("lavapipe".into())));

        assert!(lvp.matches(&DeviceSelector::Cpu));
        assert!(!lvp.matches(&DeviceSelector::Gpu));
        assert!(lvp.matches(&DeviceSelector::Name("lavapipe".into())));
        assert!(lvp.matches(&DeviceSelector::Name("llvmpipe".into())));
        assert!(hardware.rank() > lvp.rank());
    }

    #[test]
    fn drm_node_selector_matches_primary_and_render_nodes() {
        let mut gpu = info("gpu", DeviceKind::Discrete, "example");
        gpu.drm_primary = Some((226, 1));
        gpu.drm_render = Some((226, 129));
        let lvp = info("llvmpipe", DeviceKind::Cpu, "llvmpipe");

        let node = |major, minor| DeviceSelector::DrmNode { major, minor };
        assert!(gpu.matches(&node(226, 1)));
        assert!(gpu.matches(&node(226, 129)));
        assert!(!gpu.matches(&node(226, 0)));
        assert!(!lvp.matches(&node(226, 1)));
    }
}
