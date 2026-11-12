//! Importing dma-bufs as sampled images. Host only.
//!
//! A Wayland client that renders with the GPU hands the compositor a dma-buf
//! per frame. The host imports it here and gets an image scripts can sample
//! like any other, without copying the pixels.
//!
//! What can be imported is decided by the device: [`Gpu::dmabuf_import_formats`]
//! lists the (DRM format, modifier) pairs, and the host advertises exactly
//! those to clients. Only single-plane RGB formats are handled.
//!
//! Ownership: the memory belongs to another device or process ("foreign"
//! queue family). Before the first use, and again every time the producer
//! has written new contents, ownership is acquired with a queue family
//! transfer from `VK_QUEUE_FAMILY_FOREIGN_EXT`. The specification does not
//! say which layout a foreign producer leaves an image in; `GENERAL` is used
//! as the source layout because it is the one layout that never implies the
//! contents may be discarded.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};

use vulkan::vk;

use crate::desc::ImageUsage;
use crate::device::{DeviceCtx, vk_error};
use crate::error::{ErrorCode, GfxError, Result, bail};
use crate::format::Format;
use crate::gpu::{COLOR_RANGE, Garbage, Gpu, Image, ImageMemory};
use crate::handle::Handle;

/// `DMA_BUF_MAGIC` of `linux/magic.h`: the filesystem type of dma-bufs.
const DMA_BUF_MAGIC: u64 = 0x444d_4142;

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// DRM formats that can be imported, with the image format they are sampled
/// as and whether their fourth channel carries alpha.
const FORMATS: [(u32, Format, bool); 5] = [
    (fourcc(b"AR24"), Format::Bgra8Unorm, true),
    (fourcc(b"XR24"), Format::Bgra8Unorm, false),
    (fourcc(b"AB24"), Format::Rgba8Unorm, true),
    (fourcc(b"XB24"), Format::Rgba8Unorm, false),
    (fourcc(b"AB4H"), Format::Rgba16Float, true),
];

/// A (DRM format, modifier) pair the device can import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifier: u64,
    /// Largest image of this kind the device takes.
    pub max_width: u32,
    pub max_height: u32,
}

/// One plane of a dma-buf to import.
#[derive(Debug)]
pub struct DmabufPlaneDesc<'a> {
    pub fd: BorrowedFd<'a>,
    pub offset: u32,
    pub stride: u32,
}

/// A dma-buf to import.
#[derive(Debug)]
pub struct DmabufDesc<'a> {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: &'a [DmabufPlaneDesc<'a>],
}

/// An imported dma-buf.
#[derive(Clone, Copy, Debug)]
pub struct ImportedImage {
    pub image: Handle,
    pub format: Format,
    /// Whether the fourth channel carries alpha. When it does not, sample
    /// through [`Gpu::create_opaque_texture_slot`].
    pub has_alpha: bool,
}

/// Queries what the device can import. Called once at device creation.
pub(crate) unsafe fn query_import_formats(ctx: &DeviceCtx) -> Vec<DmabufFormat> {
    let mut formats = Vec::new();
    if !ctx.dmabuf_import {
        return formats;
    }
    let needed =
        vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
    for (code, format, _) in FORMATS {
        // SAFETY: property queries on valid handles; the second call passes
        // an array of the size the first call reported.
        let modifiers = unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            {
                let mut properties = vk::FormatProperties2::default().push_next(&mut list);
                ctx.instance.get_physical_device_format_properties2(
                    ctx.physical_device,
                    format.vk(),
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
                        format.vk(),
                        &mut properties,
                    );
                }
                list.drm_format_modifier_count as usize
            };
            modifiers.truncate(written);
            modifiers
        };
        for m in modifiers {
            if m.drm_format_modifier_plane_count != 1
                || !m.drm_format_modifier_tiling_features.contains(needed)
            {
                continue;
            }
            // Can an image like this be created over imported memory, and
            // how large?
            let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(m.drm_format_modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlagBits::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format.vk())
                .ty(vk::ImageType::_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .push_next(&mut modifier_info)
                .push_next(&mut external_info);
            let mut external = vk::ExternalImageFormatProperties::default();
            // SAFETY: a property query with a well-formed chain.
            let (result, extent) = unsafe {
                let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external);
                let result = ctx.instance.get_physical_device_image_format_properties2(
                    ctx.physical_device,
                    &info,
                    &mut properties,
                );
                (result, properties.image_format_properties.max_extent)
            };
            if result.is_err()
                || !external
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            {
                continue;
            }
            formats.push(DmabufFormat {
                fourcc: code,
                modifier: m.drm_format_modifier,
                max_width: extent.width.min(ctx.limits.max_image_dimension),
                max_height: extent.height.min(ctx.limits.max_image_dimension),
            });
        }
    }
    formats
}

impl Gpu {
    /// The (DRM format, modifier) pairs [`Gpu::import_dmabuf`] accepts.
    /// Empty when the device cannot import dma-bufs.
    pub fn dmabuf_import_formats(&self) -> &[DmabufFormat] {
        &self.ctx().dmabuf_formats
    }

    /// Imports a dma-buf as a sampled image. The file descriptor is
    /// duplicated; the caller keeps its own.
    ///
    /// The contents are whatever the producer wrote. The caller has to make
    /// sure the producer is done writing before the image is sampled, and
    /// has to call [`Gpu::dmabuf_contents_changed`] when the producer has
    /// written again.
    pub fn import_dmabuf(&mut self, desc: &DmabufDesc<'_>) -> Result<ImportedImage> {
        self.alive()?;
        let Some((_, format, has_alpha)) = FORMATS.iter().find(|(code, ..)| *code == desc.fourcc)
        else {
            bail!(
                Unsupported,
                "dma-buf format {:#x} cannot be imported",
                desc.fourcc
            );
        };
        let (format, has_alpha) = (*format, *has_alpha);
        let Some(supported) = self
            .ctx()
            .dmabuf_formats
            .iter()
            .find(|f| f.fourcc == desc.fourcc && f.modifier == desc.modifier)
            .copied()
        else {
            bail!(
                Unsupported,
                "the device cannot import dma-buf format {:#x} with modifier {:#x}",
                desc.fourcc,
                desc.modifier
            );
        };
        if desc.width == 0
            || desc.height == 0
            || desc.width > supported.max_width
            || desc.height > supported.max_height
        {
            bail!(
                InvalidArgument,
                "dma-buf size {}x{} is outside 1..={}x{}",
                desc.width,
                desc.height,
                supported.max_width,
                supported.max_height
            );
        }
        let [plane] = desc.planes else {
            bail!(
                Unsupported,
                "a dma-buf with {} planes cannot be imported",
                desc.planes.len()
            );
        };
        let row = u64::from(desc.width) * u64::from(format.bytes_per_pixel());
        if u64::from(plane.stride) < row {
            bail!(
                InvalidArgument,
                "the stride {} is too small for a row of {row} bytes",
                plane.stride
            );
        }
        if self.images.live() >= self.images.capacity() {
            bail!(
                LimitExceeded,
                "too many images (limit {})",
                self.images.capacity()
            );
        }
        // A client names any descriptor it likes. Vulkan requires a real
        // dma-buf (passing anything else is not an error it has to report),
        // so that is checked here: dma-bufs live on their own filesystem.
        let is_dmabuf =
            rustix::fs::fstatfs(plane.fd).is_ok_and(|fs| fs.f_type as u64 == DMA_BUF_MAGIC);
        if !is_dmabuf {
            bail!(InvalidArgument, "the descriptor is not a dma-buf");
        }
        // The size of the buffer behind the descriptor: the image must fit.
        let owned = plane.fd.try_clone_to_owned().map_err(|e| GfxError {
            code: ErrorCode::InvalidArgument,
            message: format!("duplicating the dma-buf descriptor failed: {e}"),
        })?;
        let mut file = File::from(owned);
        let size = file.seek(SeekFrom::End(0)).map_err(|e| GfxError {
            code: ErrorCode::InvalidArgument,
            message: format!("the descriptor is not a dma-buf: {e}"),
        })?;
        let dma_buf = vk::ExternalMemoryHandleTypeFlagBits::DMA_BUF_EXT;

        // SAFETY: the device has the import extensions enabled (the format
        // list is empty otherwise). Each failure path destroys what was
        // created before it; the descriptor is consumed by Vulkan only when
        // the allocation succeeds.
        let created = unsafe {
            let ctx = self.ctx.as_ref().expect("device lives until drop");
            let device = &ctx.device;
            let layouts = [vk::SubresourceLayout {
                offset: u64::from(plane.offset),
                size: 0,
                row_pitch: u64::from(plane.stride),
                array_pitch: 0,
                depth_pitch: 0,
            }];
            let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(desc.modifier)
                .plane_layouts(&layouts);
            let mut external =
                vk::ExternalMemoryImageCreateInfo::default().handle_types(dma_buf.into());
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::_2D)
                .format(format.vk())
                .extent(vk::Extent3D {
                    width: desc.width,
                    height: desc.height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlagBits::_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut modifier)
                .push_next(&mut external);
            (|| -> Result<(vk::Image, vk::DeviceMemory)> {
                let vk_image = device
                    .create_image(&info, None)
                    .map_err(|e| vk_error("create an image for a dma-buf", e))?;
                let cleanup = |e: GfxError| {
                    device.destroy_image(vk_image, None);
                    e
                };
                let requirements = device.get_image_memory_requirements(vk_image);
                if requirements.size > size {
                    return Err(cleanup(GfxError {
                        code: ErrorCode::InvalidArgument,
                        message: format!(
                            "the dma-buf has {size} bytes, the image needs {}",
                            requirements.size
                        ),
                    }));
                }
                let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
                device
                    .get_memory_fd_properties_khr(dma_buf, file.as_raw_fd(), &mut fd_properties)
                    .map_err(|e| cleanup(vk_error("query the dma-buf", e)))?;
                let usable = requirements.memory_type_bits & fd_properties.memory_type_bits;
                if usable == 0 {
                    return Err(cleanup(GfxError {
                        code: ErrorCode::Unsupported,
                        message: "no memory type fits both the image and the dma-buf".into(),
                    }));
                }
                let raw_fd = file.into_raw_fd();
                let mut import = vk::ImportMemoryFdInfoKHR::default()
                    .handle_type(dma_buf)
                    .fd(raw_fd);
                let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(vk_image);
                let allocate = vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(usable.trailing_zeros())
                    .push_next(&mut import)
                    .push_next(&mut dedicated);
                let memory = match device.allocate_memory(&allocate, None) {
                    Ok(memory) => memory,
                    Err(e) => {
                        // The import failed, so the descriptor is still ours.
                        drop(OwnedFd::from_raw_fd(raw_fd));
                        return Err(cleanup(vk_error("import the dma-buf", e)));
                    }
                };
                if let Err(e) = device.bind_image_memory(vk_image, memory, 0) {
                    device.destroy_image(vk_image, None);
                    device.free_memory(memory, None);
                    return Err(vk_error("bind the dma-buf", e));
                }
                Ok((vk_image, memory))
            })()
        };
        let (vk_image, memory) = match created {
            Ok(created) => created,
            Err(e) => {
                if e.code == ErrorCode::DeviceLost {
                    self.lost = Some((ErrorCode::DeviceLost, e.message.clone()));
                }
                return Err(e);
            }
        };
        let image = Image {
            vk: vk_image,
            view: vk::ImageView::null(),
            memory: ImageMemory::Imported(memory),
            format,
            width: desc.width,
            height: desc.height,
            usage: ImageUsage::SAMPLED,
            layout: vk::ImageLayout::UNDEFINED,
            rest: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            in_pass: false,
            // The memory is the producer's; it does not count against the
            // budget of this device's objects.
            bytes: 0,
        };
        match self.images.insert(image) {
            Ok(handle) => {
                self.pending_acquire.push(handle);
                self.flush_pending_acquire();
                Ok(ImportedImage {
                    image: handle,
                    format,
                    has_alpha,
                })
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

    /// Tells the device that the producer of an imported dma-buf has written
    /// new contents since the image was last used.
    pub fn dmabuf_contents_changed(&mut self, image: Handle) -> Result<()> {
        let i = self.images.get(image)?;
        if !matches!(i.memory, ImageMemory::Imported(_)) {
            bail!(InvalidArgument, "{image:?} is not an imported dma-buf");
        }
        if !self.pending_acquire.contains(&image) {
            self.pending_acquire.push(image);
        }
        self.flush_pending_acquire();
        Ok(())
    }

    /// Acquires imported images from their foreign owner. Runs at the start
    /// of a frame, and at once when a frame is being recorded outside a
    /// pass.
    pub(crate) fn flush_pending_acquire(&mut self) {
        if self.pending_acquire.is_empty() || !self.is_recording() || self.pass.is_some() {
            return;
        }
        let cb = self.cb();
        let family = self.ctx().queue_family;
        let pending = std::mem::take(&mut self.pending_acquire);
        for handle in pending {
            let Ok(image) = self.images.get_mut(handle) else {
                continue;
            };
            let stage = vk::PipelineStageFlags2::FRAGMENT_SHADER
                | vk::PipelineStageFlags2::COMPUTE_SHADER
                | vk::PipelineStageFlags2::VERTEX_SHADER;
            let mut barriers = Vec::with_capacity(2);
            if image.layout != vk::ImageLayout::UNDEFINED {
                // Used before: hand it back first, so that the acquire below
                // is the counterpart of a release.
                barriers.push(
                    vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(stage)
                        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                        .dst_stage_mask(vk::PipelineStageFlags2::NONE)
                        .dst_access_mask(vk::AccessFlags2::NONE)
                        .old_layout(image.rest)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(family)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                        .image(image.vk)
                        .subresource_range(COLOR_RANGE),
                );
            }
            barriers.push(
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(stage)
                    .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(image.rest)
                    .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .dst_queue_family_index(family)
                    .image(image.vk)
                    .subresource_range(COLOR_RANGE),
            );
            // SAFETY: recording outside a pass; the image is alive. Each
            // barrier is its own command so that the release precedes the
            // acquire.
            unsafe {
                let device = &self.ctx.as_ref().expect("device").device;
                for barrier in &barriers {
                    device.cmd_pipeline_barrier2(
                        cb,
                        &vk::DependencyInfo::default()
                            .image_memory_barriers(std::slice::from_ref(barrier)),
                    );
                }
            }
            image.layout = image.rest;
            self.stats.barriers += barriers.len() as u64;
        }
    }
}
