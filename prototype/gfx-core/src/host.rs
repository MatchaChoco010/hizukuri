//! Calls for the host only. Nothing here is exposed to scripts.
//!
//! The host (the compositor core) creates objects of its own on the same
//! device: the textures of client surfaces, the images of outputs. Scripts
//! may use them, but must not destroy or overwrite them, and they must
//! survive when a script is replaced. [`Gpu::protect`] marks them.

use std::os::fd::{FromRawFd, OwnedFd};

use vulkan::vk;
use vulkan_alloc::{AllocationCreateDesc, AllocationScheme, MemoryLocation};

use crate::desc::ImageUsage;
use crate::device::vk_error;
use crate::error::{ErrorCode, GfxError, Result, bail};
use crate::format::Format;
use crate::gpu::{COLOR_RANGE, Garbage, Gpu, Image, ImageMemory};
use crate::handle::Handle;

/// What the device is, for state dumps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceDescription {
    pub name: String,
    pub driver: String,
    /// A software rasterizer.
    pub is_cpu: bool,
    /// DRM primary node (major, minor) of the device, if it has one.
    pub drm_primary: Option<(u32, u32)>,
    /// DRM render node (major, minor) of the device, if it has one.
    pub drm_render: Option<(u32, u32)>,
    pub dmabuf_export: bool,
    /// Whether dma-bufs can be imported as textures.
    pub dmabuf_import: bool,
}

/// The dma-buf of a scanout image.
#[derive(Debug)]
pub struct ScanoutPlane {
    pub fd: OwnedFd,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
}

impl Gpu {
    pub fn device_description(&self) -> DeviceDescription {
        let ctx = self.ctx();
        DeviceDescription {
            name: ctx.device_name.clone(),
            driver: ctx.driver_name.clone(),
            is_cpu: ctx.is_cpu,
            drm_primary: ctx.drm_primary,
            drm_render: ctx.drm_render,
            dmabuf_export: ctx.dmabuf_export,
            dmabuf_import: !ctx.dmabuf_formats.is_empty(),
        }
    }

    /// The serial of the frame that is being recorded, or of the last one
    /// submitted when none is. What exists now can be used by frames up to
    /// this one.
    pub fn current_serial(&self) -> u64 {
        if self.is_recording() {
            self.submitted_serial + 1
        } else {
            self.submitted_serial
        }
    }

    /// The serial up to which the GPU has finished every frame, as far as
    /// is known without waiting.
    pub fn completed_serial(&mut self) -> u64 {
        let _ = self.poll_frames();
        self.completed_serial
    }

    /// Changes how long a wait for the GPU may take before the device is
    /// given up on ([`crate::GpuConfig::gpu_timeout`]).
    pub fn set_gpu_timeout(&mut self, timeout: std::time::Duration) {
        self.config.gpu_timeout = timeout;
    }

    /// Marks an object as the host's. The script binding refuses to destroy
    /// or overwrite such objects, and [`Gpu::destroy_all`] keeps them. The
    /// mark ends when the host destroys the object.
    pub fn protect(&mut self, handle: Handle) {
        self.protected.insert(handle);
    }

    pub fn is_protected(&self, handle: Handle) -> bool {
        self.protected.contains(&handle)
    }

    /// Fails with `NotPermitted` when `handle` belongs to the host. For the
    /// script binding.
    pub fn check_not_protected(&self, handle: Handle, action: &str) -> Result<()> {
        if self.is_protected(handle) {
            bail!(
                NotPermitted,
                "{handle:?} belongs to the compositor; a script cannot {action} it"
            );
        }
        Ok(())
    }

    /// The single-plane DRM format modifiers a scanout image can have. Empty
    /// when the device cannot export dma-bufs.
    pub fn scanout_modifiers(&self) -> Vec<u64> {
        let ctx = self.ctx();
        if !ctx.dmabuf_export {
            return Vec::new();
        }
        let format = Format::Bgra8Unorm.vk();
        // SAFETY: property queries on valid handles; the second call passes an
        // array of the size the first call reported.
        unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            {
                let mut properties = vk::FormatProperties2::default().push_next(&mut list);
                ctx.instance.get_physical_device_format_properties2(
                    ctx.physical_device,
                    format,
                    &mut properties,
                );
            }
            let count = list.drm_format_modifier_count as usize;
            let mut modifiers = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
            let written = {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                    .drm_format_modifier_properties(&mut modifiers);
                {
                    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
                    ctx.instance.get_physical_device_format_properties2(
                        ctx.physical_device,
                        format,
                        &mut properties,
                    );
                }
                list.drm_format_modifier_count as usize
            };
            modifiers.truncate(written);
            modifiers
                .iter()
                .filter(|m| {
                    m.drm_format_modifier_plane_count == 1
                        && m.drm_format_modifier_tiling_features
                            .contains(vk::FormatFeatureFlags::COLOR_ATTACHMENT)
                })
                .map(|m| m.drm_format_modifier)
                .collect()
        }
    }

    /// Creates a `bgra8unorm` image that can be rendered to and is exported
    /// as a dma-buf (DRM `XRGB8888`), for scanout by a KMS device.
    ///
    /// The image rests in `GENERAL` layout. After a frame that rendered into
    /// it has finished on the GPU, its contents are complete for the consumer
    /// of the dma-buf.
    pub fn create_scanout_image(
        &mut self,
        width: u32,
        height: u32,
        allowed_modifiers: &[u64],
    ) -> Result<(Handle, ScanoutPlane)> {
        self.alive()?;
        let max = self.ctx().limits.max_image_dimension;
        if width == 0 || height == 0 || width > max || height > max {
            bail!(
                InvalidArgument,
                "image size {width}x{height} is outside 1..={max}"
            );
        }
        let candidates: Vec<u64> = self
            .scanout_modifiers()
            .into_iter()
            .filter(|m| allowed_modifiers.contains(m))
            .collect();
        if candidates.is_empty() {
            bail!(
                Unsupported,
                "the device cannot export a dma-buf with a format modifier the consumer accepts"
            );
        }
        let format = Format::Bgra8Unorm;
        let bytes = u64::from(width) * u64::from(height) * u64::from(format.bytes_per_pixel());
        self.charge_memory(bytes, "a scanout image")?;
        let dma_buf = vk::ExternalMemoryHandleTypeFlagBits::DMA_BUF_EXT;

        // SAFETY: the device has the dma-buf extensions enabled (checked
        // through `scanout_modifiers`). Each failure path destroys what was
        // created before it.
        let created = unsafe {
            let ctx = self.ctx.as_mut().expect("device lives until drop");
            let device = ctx.device.clone();
            let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                .drm_format_modifiers(&candidates);
            let mut external =
                vk::ExternalMemoryImageCreateInfo::default().handle_types(dma_buf.into());
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::_2D)
                .format(format.vk())
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlagBits::_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut modifier_list)
                .push_next(&mut external);
            let vk_image = device
                .create_image(&info, None)
                .map_err(|e| vk_error("create scanout image", e))?;
            let requirements = device.get_image_memory_requirements(vk_image);
            // An exported handle refers to a whole memory object, so the
            // image gets its own.
            let allocation = match ctx.allocator().allocate(AllocationCreateDesc {
                name: Some("scanout image"),
                requirements,
                location: MemoryLocation::GpuOnly,
                linear: false,
                memory_allocate_flags: vk::MemoryAllocateFlags::empty(),
                export_handle_types: dma_buf.into(),
                allocation_scheme: AllocationScheme::DedicatedImage(vk_image),
            }) {
                Ok(allocation) => allocation,
                Err(e) => {
                    device.destroy_image(vk_image, None);
                    bail!(OutOfMemory, "allocate scanout image memory: {e}");
                }
            };
            let mut view = vk::ImageView::null();
            let result = (|| -> std::result::Result<ScanoutPlane, GfxError> {
                device
                    .bind_image_memory(vk_image, allocation.memory(), allocation.offset())
                    .map_err(|e| vk_error("bind scanout image memory", e))?;
                view = device
                    .create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(vk_image)
                            .view_type(vk::ImageViewType::_2D)
                            .format(format.vk())
                            .subresource_range(COLOR_RANGE),
                        None,
                    )
                    .map_err(|e| vk_error("create scanout image view", e))?;
                let mut chosen = vk::ImageDrmFormatModifierPropertiesEXT::default();
                device
                    .get_image_drm_format_modifier_properties_ext(vk_image, &mut chosen)
                    .map_err(|e| vk_error("query the format modifier", e))?;
                let layout = device.get_image_subresource_layout(
                    vk_image,
                    &vk::ImageSubresource {
                        aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                        mip_level: 0,
                        array_layer: 0,
                    },
                );
                let raw_fd = device
                    .get_memory_fd_khr(
                        &vk::MemoryGetFdInfoKHR::default()
                            .memory(allocation.memory())
                            .handle_type(dma_buf),
                    )
                    .map_err(|e| vk_error("export the dma-buf", e))?;
                Ok(ScanoutPlane {
                    // SAFETY: Vulkan returns a new file descriptor that the
                    // caller owns.
                    fd: OwnedFd::from_raw_fd(raw_fd),
                    modifier: chosen.drm_format_modifier,
                    stride: layout.row_pitch as u32,
                    offset: (allocation.offset() + layout.offset) as u32,
                })
            })();
            match result {
                Ok(plane) => Ok((vk_image, view, allocation, plane)),
                Err(e) => {
                    if !view.is_null() {
                        device.destroy_image_view(view, None);
                    }
                    device.destroy_image(vk_image, None);
                    let _ = ctx.allocator().free(allocation);
                    Err(e)
                }
            }
        };
        let (vk_image, view, allocation, plane) = match created {
            Ok(created) => created,
            Err(e) => {
                if e.code == ErrorCode::DeviceLost {
                    self.lost = Some((ErrorCode::DeviceLost, e.message.clone()));
                }
                return Err(e);
            }
        };
        self.memory_bytes += bytes;
        let image = Image {
            vk: vk_image,
            view,
            memory: ImageMemory::Allocated(allocation),
            format,
            width,
            height,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
            layout: vk::ImageLayout::UNDEFINED,
            rest: vk::ImageLayout::GENERAL,
            in_pass: false,
            bytes,
        };
        match self.images.insert(image) {
            Ok(handle) => {
                self.pending_init.push(handle);
                Ok((handle, plane))
            }
            Err((image, e)) => {
                self.retire(Garbage::Image {
                    vk: image.vk,
                    view: image.view,
                    memory: image.memory,
                    bytes: image.bytes,
                });
                Err(e)
            }
        }
    }

    /// Reads back an image into caller-provided memory with rows `stride`
    /// bytes apart (for example a mapped KMS dumb buffer). Bytes between the
    /// end of a row and the next row are left untouched. See
    /// [`Gpu::read_image`].
    pub fn read_image_into(
        &mut self,
        image: Handle,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<()> {
        let info = self.image_info(image)?;
        let row = info.width as usize * info.format.bytes_per_pixel() as usize;
        let height = info.height as usize;
        if stride < row || destination.len() < stride * (height - 1) + row {
            bail!(
                OutOfRange,
                "the destination ({} bytes, stride {stride}) is too small for a {}x{} image",
                destination.len(),
                info.width,
                info.height
            );
        }
        let pixels = self.read_image(image)?;
        if stride == row {
            destination[..pixels.len()].copy_from_slice(&pixels);
        } else {
            for y in 0..height {
                destination[y * stride..y * stride + row]
                    .copy_from_slice(&pixels[y * row..(y + 1) * row]);
            }
        }
        Ok(())
    }
}
