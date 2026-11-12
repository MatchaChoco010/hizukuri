//! Recording: frames, render passes, draws, compute and copies.
//!
//! Every call validates its arguments and the current state before it records
//! anything. A call that returns an error has recorded nothing.

use vulkan::vk;
use vulkan_alloc::{AllocationCreateDesc, AllocationScheme, MemoryLocation};

use crate::desc::*;
use crate::device::vk_error;
use crate::error::{ErrorCode, GfxError, Result, bail};
use crate::format::Format;
use crate::gpu::{
    COLOR_LAYERS, Garbage, Gpu, Image, MAX_VERTEX_BUFFERS, PipelineCommon, SamplerSlot,
    SlotContent, image_barrier,
};
use crate::handle::{Handle, Kind, Pool};
use crate::shader::{BindingKind, MAX_BINDINGS};

/// Upper bound of push data the API handles (the device limit may be lower).
const PUSH_CAPACITY: usize = 256;

/// The heap slots and push-constant bytes a script has set for one bind point.
pub(crate) struct BindTable {
    slots: [Handle; MAX_BINDINGS as usize],
    user: [u8; PUSH_CAPACITY],
    /// Push data must be sent again before the next draw or dispatch.
    dirty: bool,
    /// The bindings must be validated again before the next draw or dispatch.
    unvalidated: bool,
    /// `Gpu::mutation` when the bindings were last validated.
    validated_mutation: u64,
}

impl Default for BindTable {
    fn default() -> Self {
        Self {
            slots: [Handle::NULL; MAX_BINDINGS as usize],
            user: [0; PUSH_CAPACITY],
            dirty: true,
            unvalidated: true,
            validated_mutation: 0,
        }
    }
}

#[derive(Copy, Clone)]
struct BoundBuffer {
    handle: Handle,
    /// Bytes available after the bind offset.
    available: u64,
}

pub(crate) struct PassState {
    /// Attachments with the raw image and its rest layout. The raw handles stay
    /// valid until the frame has executed even if the script destroys the image.
    attachments: Vec<(Handle, vk::Image, vk::ImageLayout)>,
    formats: Vec<Format>,
    width: u32,
    height: u32,
    pipeline: Option<Handle>,
    vertex_buffers: [Option<BoundBuffer>; MAX_VERTEX_BUFFERS],
    index_buffer: Option<(BoundBuffer, IndexType)>,
}

impl Gpu {
    pub(crate) fn cb(&self) -> vk::CommandBuffer {
        self.frames[self.frame_index].cb
    }

    fn recording(&self) -> Result<()> {
        self.alive()?;
        if !self.recording {
            bail!(InvalidState, "no frame is being recorded");
        }
        Ok(())
    }

    fn outside_pass(&self, what: &str) -> Result<()> {
        self.recording()?;
        if self.pass.is_some() {
            bail!(InvalidState, "{what} is not allowed inside a render pass");
        }
        Ok(())
    }

    fn in_pass(&self, what: &str) -> Result<()> {
        self.recording()?;
        if self.pass.is_none() {
            bail!(InvalidState, "{what} is only allowed inside a render pass");
        }
        Ok(())
    }

    // ----- frames -----------------------------------------------------------

    /// Starts recording a frame. Blocks if the GPU is still busy with the
    /// frame that used the same command buffer.
    pub fn begin_frame(&mut self) -> Result<()> {
        self.alive()?;
        if self.recording {
            bail!(InvalidState, "a frame is already being recorded");
        }
        let index = self.frame_index;
        self.wait_frame(index)?;
        self.poll_frames()?;
        let (pool, cb, fence) = {
            let f = &self.frames[index];
            (f.pool, f.cb, f.fence)
        };
        // SAFETY: the frame slot is idle (waited above), so its pool, command
        // buffer and fence are not in use. The heaps outlive every frame.
        let result = unsafe {
            let ctx = self.ctx();
            let d = &ctx.device;
            (|| {
                d.reset_fences(&[fence])?;
                d.reset_command_pool(pool, vk::CommandPoolResetFlags::empty())?;
                d.begin_command_buffer(
                    cb,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                for (heap, sampler) in [(&ctx.resource_heap, false), (&ctx.sampler_heap, true)] {
                    let info = vk::BindHeapInfoEXT::default()
                        .heap_range(
                            vk::DeviceAddressRangeKHR::default()
                                .address(heap.address)
                                .size(heap.size),
                        )
                        .reserved_range_offset(heap.reserved_offset)
                        .reserved_range_size(heap.reserved_size);
                    if sampler {
                        d.cmd_bind_sampler_heap_ext(cb, &info);
                    } else {
                        d.cmd_bind_resource_heap_ext(cb, &info);
                    }
                }
                Ok(())
            })()
        };
        if let Err(e) = result {
            return Err(self.fail("begin frame", e));
        }
        self.recording = true;
        self.pass = None;
        self.graphics_binds = BindTable::default();
        self.compute_binds = BindTable::default();
        self.compute_pipeline = None;
        self.flush_pending_init();
        self.flush_pending_acquire();
        Ok(())
    }

    /// Ends the frame and submits it. Does not wait for the GPU.
    ///
    /// If a render pass is still open it is closed first, the frame is
    /// submitted as recorded so far, and an `InvalidState` error is returned.
    /// This is the path a host takes after a script threw mid-frame.
    pub fn end_frame(&mut self) -> Result<()> {
        self.recording()?;
        let pass_was_open = self.pass.is_some();
        if pass_was_open {
            self.end_pass()?;
        }
        let (cb, fence) = {
            let f = &self.frames[self.frame_index];
            (f.cb, f.fence)
        };
        // SAFETY: the command buffer is recording and complete (no open pass);
        // everything it refers to stays alive until the fence signals.
        let result = unsafe {
            let ctx = self.ctx();
            let cbs = [cb];
            ctx.device.end_command_buffer(cb).and_then(|()| {
                ctx.device.queue_submit(
                    ctx.queue,
                    &[vk::SubmitInfo::default().command_buffers(&cbs)],
                    fence,
                )
            })
        };
        self.recording = false;
        if let Err(e) = result {
            return Err(self.fail("submit frame", e));
        }
        self.submitted_serial += 1;
        let index = self.frame_index;
        self.frames[index].serial = self.submitted_serial;
        self.frames[index].pending = true;
        self.frame_index = (index + 1) % self.frames.len();
        self.stats.frames_submitted += 1;
        self.poll_frames()?;
        if pass_was_open {
            bail!(
                InvalidState,
                "the frame ended while a render pass was open; the pass was closed and the frame submitted"
            );
        }
        Ok(())
    }

    /// Whether a frame is being recorded.
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// Whether a render pass is open.
    pub fn is_in_pass(&self) -> bool {
        self.pass.is_some()
    }

    /// Moves newly created images out of `UNDEFINED` into their rest layout.
    fn flush_pending_init(&mut self) {
        if self.pending_init.is_empty() {
            return;
        }
        let cb = self.cb();
        let pending = std::mem::take(&mut self.pending_init);
        for handle in pending {
            let Ok(image) = self.images.get_mut(handle) else {
                continue;
            };
            if image.layout != vk::ImageLayout::UNDEFINED {
                continue;
            }
            // SAFETY: recording outside a pass; the tracked layout is accurate.
            unsafe {
                image_barrier(
                    &self.ctx.as_ref().expect("device").device,
                    cb,
                    image.vk,
                    image.layout,
                    image.rest,
                );
            }
            image.layout = image.rest;
            self.stats.barriers += 1;
        }
    }

    // ----- copies -----------------------------------------------------------

    /// Replaces the contents of an image with tightly packed pixel data.
    pub fn upload_image(&mut self, image: Handle, data: &[u8]) -> Result<()> {
        self.outside_pass("upload_image")?;
        let (vk_image, rest, layout, width, height, bytes) = {
            let i = self.images.get(image)?;
            if !i.usage.contains(ImageUsage::COPY_DST) {
                bail!(
                    InvalidArgument,
                    "{image:?} was not created with usage `copy_dst`"
                );
            }
            (i.vk, i.rest, i.layout, i.width, i.height, i.bytes)
        };
        if data.len() as u64 != bytes {
            bail!(
                Mismatch,
                "{image:?} is {width}x{height} and needs exactly {bytes} bytes, got {}",
                data.len()
            );
        }
        let cb = self.cb();
        // SAFETY: the staging buffer is exactly `bytes` long and is retired
        // with this frame's serial, so it outlives the copy. The image is
        // alive and its tracked layout is accurate.
        let staging = unsafe {
            let ctx = self.ctx.as_mut().expect("device lives until drop");
            let device = ctx.device.clone();
            let buffer = match device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            ) {
                Ok(b) => b,
                Err(e) => return Err(self.fail("create staging buffer", e)),
            };
            let requirements = device.get_buffer_memory_requirements(buffer);
            let allocation = match ctx.allocator().allocate(AllocationCreateDesc {
                name: Some("upload staging"),
                requirements,
                location: MemoryLocation::CpuToGpu,
                linear: true,
                memory_allocate_flags: vk::MemoryAllocateFlags::empty(),
                export_handle_types: vk::ExternalMemoryHandleTypeFlags::empty(),
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            }) {
                Ok(a) => a,
                Err(e) => {
                    device.destroy_buffer(buffer, None);
                    bail!(OutOfMemory, "allocate staging buffer: {e}");
                }
            };
            let prepared = device
                .bind_buffer_memory(buffer, allocation.memory(), allocation.offset())
                .map_err(|e| vk_error("bind staging buffer", e))
                .and_then(|()| {
                    ctx.allocator().map_memory(&allocation).map_err(|e| {
                        GfxError::new(ErrorCode::Internal, format!("map staging buffer: {e}"))
                    })
                });
            let mapped = match prepared {
                Ok(m) => m.cast::<u8>(),
                Err(e) => {
                    device.destroy_buffer(buffer, None);
                    let _ = ctx.allocator().free(allocation);
                    return Err(e);
                }
            };
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapped, data.len());
            let _ = ctx.allocator().flush_allocation(&allocation);

            image_barrier(
                &device,
                cb,
                vk_image,
                layout,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let regions = [vk::BufferImageCopy2::default()
                .image_subresource(COLOR_LAYERS)
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            device.cmd_copy_buffer_to_image2(
                cb,
                &vk::CopyBufferToImageInfo2::default()
                    .src_buffer(buffer)
                    .dst_image(vk_image)
                    .dst_image_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .regions(&regions),
            );
            image_barrier(
                &device,
                cb,
                vk_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                rest,
            );
            Garbage::Buffer {
                vk: buffer,
                allocation,
                bytes: 0,
            }
        };
        self.retire(staging);
        self.images.get_mut(image)?.layout = rest;
        self.stats.barriers += 2;
        Ok(())
    }

    /// Copies all of `src` into `dst`. Both must have the same size and the
    /// same bytes per pixel.
    pub fn copy_image(&mut self, src: Handle, dst: Handle) -> Result<()> {
        self.outside_pass("copy_image")?;
        if src == dst {
            bail!(InvalidArgument, "source and destination are the same image");
        }
        let s = self.images.get(src)?;
        let d = self.images.get(dst)?;
        if !s.usage.contains(ImageUsage::COPY_SRC) {
            bail!(
                InvalidArgument,
                "{src:?} was not created with usage `copy_src`"
            );
        }
        if !d.usage.contains(ImageUsage::COPY_DST) {
            bail!(
                InvalidArgument,
                "{dst:?} was not created with usage `copy_dst`"
            );
        }
        if (s.width, s.height) != (d.width, d.height) {
            bail!(
                Mismatch,
                "source is {}x{} but destination is {}x{}",
                s.width,
                s.height,
                d.width,
                d.height
            );
        }
        if s.format.bytes_per_pixel() != d.format.bytes_per_pixel() {
            bail!(
                Mismatch,
                "{} and {} have different texel sizes",
                s.format.name(),
                d.format.name()
            );
        }
        let cb = self.cb();
        // SAFETY: both images are alive, distinct, of equal extent and of
        // size-compatible formats; their tracked layouts are accurate.
        unsafe {
            let device = &self.ctx().device;
            image_barrier(
                device,
                cb,
                s.vk,
                s.layout,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            );
            image_barrier(
                device,
                cb,
                d.vk,
                d.layout,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            let regions = [vk::ImageCopy2::default()
                .src_subresource(COLOR_LAYERS)
                .dst_subresource(COLOR_LAYERS)
                .extent(vk::Extent3D {
                    width: s.width,
                    height: s.height,
                    depth: 1,
                })];
            device.cmd_copy_image2(
                cb,
                &vk::CopyImageInfo2::default()
                    .src_image(s.vk)
                    .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .dst_image(d.vk)
                    .dst_image_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .regions(&regions),
            );
            image_barrier(
                device,
                cb,
                s.vk,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                s.rest,
            );
            image_barrier(
                device,
                cb,
                d.vk,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                d.rest,
            );
        }
        let (s_rest, d_rest) = (s.rest, d.rest);
        self.images.get_mut(src)?.layout = s_rest;
        self.images.get_mut(dst)?.layout = d_rest;
        self.stats.barriers += 4;
        Ok(())
    }

    // ----- render passes ----------------------------------------------------

    /// Opens a render pass on one or more color attachments.
    pub fn begin_pass(&mut self, color: &[ColorAttachment]) -> Result<()> {
        self.outside_pass("begin_pass")?;
        let max = 8.min(self.ctx().limits.max_color_attachments as usize);
        if color.is_empty() || color.len() > max {
            bail!(
                InvalidArgument,
                "a pass needs 1..={max} color attachments, got {}",
                color.len()
            );
        }
        let mut attachments = Vec::with_capacity(color.len());
        let mut formats = Vec::with_capacity(color.len());
        let mut extent = None;
        for (i, attachment) in color.iter().enumerate() {
            let image = self.images.get(attachment.image)?;
            if !image.usage.contains(ImageUsage::COLOR_TARGET) {
                bail!(
                    InvalidArgument,
                    "attachment {i}: {:?} was not created with usage `color_target`",
                    attachment.image
                );
            }
            if color[..i].iter().any(|c| c.image == attachment.image) {
                bail!(
                    InvalidArgument,
                    "attachment {i}: the image is attached twice"
                );
            }
            match extent {
                None => extent = Some((image.width, image.height)),
                Some(e) if e != (image.width, image.height) => bail!(
                    Mismatch,
                    "attachment {i} is {}x{} but attachment 0 is {}x{}",
                    image.width,
                    image.height,
                    e.0,
                    e.1
                ),
                Some(_) => {}
            }
            if let LoadOp::Clear(c) = attachment.load
                && c.iter().any(|v| !v.is_finite())
            {
                bail!(
                    InvalidArgument,
                    "attachment {i}: the clear color is not finite"
                );
            }
            attachments.push((attachment.image, image.vk, image.rest));
            formats.push(image.format);
        }
        let (width, height) = extent.expect("at least one attachment");
        let cb = self.cb();

        let mut infos = Vec::with_capacity(color.len());
        for attachment in color {
            let image = self.images.get_mut(attachment.image)?;
            // SAFETY: recording outside a pass; the tracked layout is accurate.
            unsafe {
                image_barrier(
                    &self.ctx.as_ref().expect("device").device,
                    cb,
                    image.vk,
                    image.layout,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                );
            }
            image.layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
            image.in_pass = true;
            let (load_op, clear) = match attachment.load {
                LoadOp::Clear(c) => (vk::AttachmentLoadOp::CLEAR, c),
                LoadOp::Load => (vk::AttachmentLoadOp::LOAD, [0.0; 4]),
                LoadOp::DontCare => (vk::AttachmentLoadOp::DONT_CARE, [0.0; 4]),
            };
            infos.push(
                vk::RenderingAttachmentInfo::default()
                    .image_view(image.view)
                    .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .load_op(load_op)
                    .store_op(if attachment.store {
                        vk::AttachmentStoreOp::STORE
                    } else {
                        vk::AttachmentStoreOp::DONT_CARE
                    })
                    .clear_value(vk::ClearValue::color(vk::ClearColorValue::float32(clear))),
            );
        }
        self.stats.barriers += color.len() as u64;
        // SAFETY: the attachments are alive, in the attachment layout, of equal
        // extent, and the render area equals that extent.
        unsafe {
            let device = &self.ctx().device;
            device.cmd_begin_rendering(
                cb,
                &vk::RenderingInfo::default()
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D { width, height },
                    })
                    .layer_count(1)
                    .color_attachments(&infos),
            );
            device.cmd_set_viewport(
                cb,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: width as f32,
                    height: height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            device.cmd_set_scissor(
                cb,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D { width, height },
                }],
            );
        }
        self.pass = Some(PassState {
            attachments,
            formats,
            width,
            height,
            pipeline: None,
            vertex_buffers: [None; MAX_VERTEX_BUFFERS],
            index_buffer: None,
        });
        self.graphics_binds = BindTable::default();
        Ok(())
    }

    /// Closes the render pass.
    pub fn end_pass(&mut self) -> Result<()> {
        self.in_pass("end_pass")?;
        let pass = self.pass.take().expect("checked");
        let cb = self.cb();
        // SAFETY: a render pass is open on `cb`. The raw image handles are
        // still valid: destruction is deferred past this frame.
        unsafe {
            let device = &self.ctx().device;
            device.cmd_end_rendering(cb);
            for (_, image, rest) in &pass.attachments {
                image_barrier(
                    device,
                    cb,
                    *image,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    *rest,
                );
            }
        }
        self.stats.barriers += pass.attachments.len() as u64;
        for (handle, _, rest) in &pass.attachments {
            if let Ok(image) = self.images.get_mut(*handle) {
                image.layout = *rest;
                image.in_pass = false;
            }
        }
        Ok(())
    }

    pub fn set_viewport(&mut self, x: f32, y: f32, width: f32, height: f32) -> Result<()> {
        self.in_pass("set_viewport")?;
        let max = self.ctx().limits.max_image_dimension as f32;
        let ok = [x, y, width, height].iter().all(|v| v.is_finite())
            && width > 0.0
            && width <= max
            && height > 0.0
            && height <= max
            && x.abs() <= 2.0 * max
            && y.abs() <= 2.0 * max;
        if !ok {
            bail!(
                InvalidArgument,
                "viewport ({x}, {y}, {width}, {height}) is not valid"
            );
        }
        // SAFETY: recording inside a pass; the viewport is within device limits.
        unsafe {
            self.ctx().device.cmd_set_viewport(
                self.cb(),
                0,
                &[vk::Viewport {
                    x,
                    y,
                    width,
                    height,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
        }
        Ok(())
    }

    pub fn set_scissor(&mut self, x: u32, y: u32, width: u32, height: u32) -> Result<()> {
        self.in_pass("set_scissor")?;
        let pass = self.pass.as_ref().expect("checked");
        let fits = x.checked_add(width).is_some_and(|r| r <= pass.width)
            && y.checked_add(height).is_some_and(|b| b <= pass.height);
        if !fits {
            bail!(
                OutOfRange,
                "scissor ({x}, {y}, {width}, {height}) is outside the {}x{} target",
                pass.width,
                pass.height
            );
        }
        // SAFETY: recording inside a pass; the rectangle lies inside the render area.
        unsafe {
            self.ctx().device.cmd_set_scissor(
                self.cb(),
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D {
                        x: x as i32,
                        y: y as i32,
                    },
                    extent: vk::Extent2D { width, height },
                }],
            );
        }
        Ok(())
    }

    // ----- state shared by draws and dispatches -------------------------------

    /// Binds a render pipeline (inside a pass) or a compute pipeline (outside).
    pub fn set_pipeline(&mut self, pipeline: Handle) -> Result<()> {
        self.recording()?;
        match pipeline.kind() {
            Some(Kind::RenderPipeline) => {
                let p = self.render_pipelines.get(pipeline)?;
                let Some(pass) = self.pass.as_mut() else {
                    bail!(
                        InvalidState,
                        "a render pipeline can only be set inside a render pass"
                    );
                };
                if self.checks && p.color_formats != pass.formats {
                    bail!(
                        Mismatch,
                        "the pipeline renders to [{}] but the pass has [{}]",
                        format_list(&p.color_formats),
                        format_list(&pass.formats)
                    );
                }
                pass.pipeline = Some(pipeline);
                // SAFETY: recording; the pipeline is alive and compatible with the pass.
                unsafe {
                    self.ctx.as_ref().expect("device").device.cmd_bind_pipeline(
                        self.frames[self.frame_index].cb,
                        vk::PipelineBindPoint::GRAPHICS,
                        p.common.vk,
                    );
                }
                self.graphics_binds.dirty = true;
                self.graphics_binds.unvalidated = true;
            }
            Some(Kind::ComputePipeline) => {
                let p = self.compute_pipelines.get(pipeline)?;
                if self.pass.is_some() {
                    bail!(
                        InvalidState,
                        "a compute pipeline cannot be set inside a render pass"
                    );
                }
                // SAFETY: recording outside a pass; the pipeline is alive.
                unsafe {
                    self.ctx().device.cmd_bind_pipeline(
                        self.cb(),
                        vk::PipelineBindPoint::COMPUTE,
                        p.common.vk,
                    );
                }
                self.compute_pipeline = Some(pipeline);
                self.compute_binds.dirty = true;
                self.compute_binds.unvalidated = true;
            }
            _ => {
                // Produces InvalidHandle / WrongKind with a useful message.
                self.render_pipelines.get(pipeline)?;
            }
        }
        Ok(())
    }

    fn bind_table(&mut self) -> &mut BindTable {
        if self.pass.is_some() {
            &mut self.graphics_binds
        } else {
            &mut self.compute_binds
        }
    }

    /// Points shader binding `binding` at a heap slot or a sampler.
    ///
    /// Applies to draws when a pass is open, to dispatches otherwise.
    pub fn bind(&mut self, binding: u32, slot: Handle) -> Result<()> {
        self.recording()?;
        if binding >= MAX_BINDINGS {
            bail!(
                OutOfRange,
                "binding {binding} is out of range (must be below {MAX_BINDINGS})"
            );
        }
        if self.checks {
            match slot.kind() {
                Some(Kind::Slot) => {
                    self.slots.get(slot)?;
                }
                Some(Kind::Sampler) => {
                    self.samplers.get(slot)?;
                }
                _ => {
                    self.slots.get(slot)?;
                }
            }
        }
        let table = self.bind_table();
        table.slots[binding as usize] = slot;
        table.dirty = true;
        table.unvalidated = true;
        Ok(())
    }

    /// Writes bytes into the shader's push-constant block.
    ///
    /// Applies to draws when a pass is open, to dispatches otherwise. The
    /// block starts zeroed, so bytes that were never pushed read as zero.
    pub fn push(&mut self, offset: u32, data: &[u8]) -> Result<()> {
        self.recording()?;
        let limit = match (&self.pass, self.compute_pipeline) {
            (Some(pass), _) => match pass.pipeline {
                Some(p) => self.render_pipelines.get(p)?.common.push_user_size,
                None => bail!(InvalidState, "set a pipeline before pushing data"),
            },
            (None, Some(p)) => self.compute_pipelines.get(p)?.common.push_user_size,
            (None, None) => bail!(InvalidState, "set a pipeline before pushing data"),
        };
        let end = (offset as usize).checked_add(data.len());
        if end.is_none_or(|end| end > limit as usize) {
            bail!(
                OutOfRange,
                "push of {} bytes at offset {offset} exceeds the pipeline's push block of {limit} bytes",
                data.len()
            );
        }
        let table = self.bind_table();
        table.user[offset as usize..end.expect("checked")].copy_from_slice(data);
        table.dirty = true;
        Ok(())
    }

    pub fn set_vertex_buffer(&mut self, index: u32, buffer: Handle, offset: u64) -> Result<()> {
        self.in_pass("set_vertex_buffer")?;
        if index as usize >= MAX_VERTEX_BUFFERS {
            bail!(
                OutOfRange,
                "vertex buffer index {index} is out of range (must be below {MAX_VERTEX_BUFFERS})"
            );
        }
        let b = self.buffers.get(buffer)?;
        if !b.usage.contains(BufferUsage::VERTEX) {
            bail!(
                InvalidArgument,
                "{buffer:?} was not created with usage `vertex`"
            );
        }
        if offset > b.size || !offset.is_multiple_of(4) {
            bail!(
                OutOfRange,
                "offset {offset} is not a multiple of 4 inside {buffer:?} of {} bytes",
                b.size
            );
        }
        // SAFETY: recording; the buffer is alive, has vertex usage, and the offset is inside it.
        unsafe {
            self.ctx
                .as_ref()
                .expect("device")
                .device
                .cmd_bind_vertex_buffers(
                    self.frames[self.frame_index].cb,
                    index,
                    &[b.vk],
                    &[offset],
                );
        }
        self.pass.as_mut().expect("checked").vertex_buffers[index as usize] = Some(BoundBuffer {
            handle: buffer,
            available: b.size - offset,
        });
        Ok(())
    }

    pub fn set_index_buffer(
        &mut self,
        buffer: Handle,
        offset: u64,
        index_type: IndexType,
    ) -> Result<()> {
        self.in_pass("set_index_buffer")?;
        let b = self.buffers.get(buffer)?;
        if !b.usage.contains(BufferUsage::INDEX) {
            bail!(
                InvalidArgument,
                "{buffer:?} was not created with usage `index`"
            );
        }
        if offset > b.size || !offset.is_multiple_of(index_type.size()) {
            bail!(
                OutOfRange,
                "offset {offset} is not a multiple of the index size inside {buffer:?} of {} bytes",
                b.size
            );
        }
        // SAFETY: recording; the buffer is alive, has index usage, and the offset is inside it.
        unsafe {
            self.ctx
                .as_ref()
                .expect("device")
                .device
                .cmd_bind_index_buffer(
                    self.frames[self.frame_index].cb,
                    b.vk,
                    offset,
                    match index_type {
                        IndexType::U16 => vk::IndexType::UINT16,
                        IndexType::U32 => vk::IndexType::UINT32,
                    },
                );
        }
        self.pass.as_mut().expect("checked").index_buffer = Some((
            BoundBuffer {
                handle: buffer,
                available: b.size - offset,
            },
            index_type,
        ));
        Ok(())
    }

    // ----- draws ------------------------------------------------------------

    pub fn draw(
        &mut self,
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    ) -> Result<()> {
        self.prepare_draw(
            Some((first_vertex, vertex_count)),
            (first_instance, instance_count),
            None,
        )?;
        // SAFETY: `prepare_draw` validated the pipeline, its bindings, the
        // vertex buffers and their ranges, and sent the push data.
        unsafe {
            self.ctx().device.cmd_draw(
                self.cb(),
                vertex_count,
                instance_count,
                first_vertex,
                first_instance,
            );
        }
        self.stats.draws += 1;
        Ok(())
    }

    pub fn draw_indexed(
        &mut self,
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        vertex_offset: i32,
        first_instance: u32,
    ) -> Result<()> {
        // Vertex indices come from buffer contents, which are not inspected:
        // out-of-range vertex fetches are made harmless by robustBufferAccess.
        self.prepare_draw(
            None,
            (first_instance, instance_count),
            Some((first_index, index_count)),
        )?;
        // SAFETY: as in `draw`, plus the index buffer and its range.
        unsafe {
            self.ctx().device.cmd_draw_indexed(
                self.cb(),
                index_count,
                instance_count,
                first_index,
                vertex_offset,
                first_instance,
            );
        }
        self.stats.draws += 1;
        Ok(())
    }

    fn prepare_draw(
        &mut self,
        vertices: Option<(u32, u32)>,
        instances: (u32, u32),
        indices: Option<(u32, u32)>,
    ) -> Result<()> {
        self.recording()?;
        let Some(pass) = self.pass.as_ref() else {
            bail!(InvalidState, "draw is only allowed inside a render pass");
        };
        let Some(pipeline) = pass.pipeline else {
            bail!(InvalidState, "no pipeline is set");
        };
        let p = self.render_pipelines.get(pipeline)?;
        let table = &mut self.graphics_binds;

        if self.checks {
            let per_instance = vertices.or(indices).map_or(0, |(_, count)| count);
            let elements = u64::from(per_instance) * u64::from(instances.1);
            let limit = self.config.limits.max_draw_elements;
            if elements > limit {
                bail!(
                    LimitExceeded,
                    "the draw processes {elements} vertices; the limit is {limit}"
                );
            }
            if table.unvalidated || table.validated_mutation != self.mutation {
                validate_bindings(
                    &p.common,
                    table,
                    &self.slots,
                    &self.samplers,
                    &self.images,
                    &self.buffers,
                    &self.ctx.as_ref().expect("device").format_caps,
                    None,
                )?;
                table.unvalidated = false;
                table.validated_mutation = self.mutation;
            }
            for (i, requirement) in p.vertex_buffers.iter().enumerate() {
                let Some(bound) = pass.vertex_buffers[i] else {
                    bail!(
                        MissingBinding,
                        "the pipeline reads vertex buffer {i} but none is set"
                    );
                };
                // The buffer may have been destroyed since it was bound.
                self.buffers.get(bound.handle)?;
                let range = if requirement.per_instance {
                    Some(instances)
                } else {
                    vertices
                };
                if let Some((first, count)) = range
                    && count > 0
                {
                    let last = u64::from(first) + u64::from(count) - 1;
                    let needed = last * requirement.stride + requirement.element_size;
                    if needed > bound.available {
                        bail!(
                            OutOfRange,
                            "vertex buffer {i}: elements {first}..{} need {needed} bytes but only {} are bound",
                            u64::from(first) + u64::from(count),
                            bound.available
                        );
                    }
                }
            }
            if let Some((first, count)) = indices {
                let Some((bound, index_type)) = pass.index_buffer else {
                    bail!(MissingBinding, "draw_indexed needs an index buffer");
                };
                self.buffers.get(bound.handle)?;
                let needed = (u64::from(first) + u64::from(count)) * index_type.size();
                if needed > bound.available {
                    bail!(
                        OutOfRange,
                        "indices {first}..{} need {needed} bytes but only {} are bound",
                        u64::from(first) + u64::from(count),
                        bound.available
                    );
                }
            }
        }

        if table.dirty {
            send_push_data(
                &self.ctx.as_ref().expect("device").device,
                self.frames[self.frame_index].cb,
                &p.common,
                table,
            );
            table.dirty = false;
            self.stats.push_updates += 1;
        }
        Ok(())
    }

    // ----- compute ----------------------------------------------------------

    pub fn dispatch(&mut self, x: u32, y: u32, z: u32) -> Result<()> {
        self.outside_pass("dispatch")?;
        let Some(pipeline) = self.compute_pipeline else {
            bail!(InvalidState, "no compute pipeline is set");
        };
        let max = self.ctx().limits.max_compute_group_count;
        if x > max[0] || y > max[1] || z > max[2] {
            bail!(
                OutOfRange,
                "dispatch ({x}, {y}, {z}) exceeds the device limit ({}, {}, {})",
                max[0],
                max[1],
                max[2]
            );
        }
        // The device limit can be 2^32 - 1 per dimension. A dispatch that
        // large is valid Vulkan and would keep the GPU busy for minutes.
        let groups = u128::from(x) * u128::from(y) * u128::from(z);
        let limit = self.config.limits.max_dispatch_groups;
        if groups > u128::from(limit) {
            bail!(
                LimitExceeded,
                "dispatch ({x}, {y}, {z}) runs {groups} workgroups; the limit is {limit}"
            );
        }
        let p = self.compute_pipelines.get(pipeline)?;
        let table = &mut self.compute_binds;
        let mut storage_images = Vec::new();
        // Storage images change layout around the dispatch, so their state is
        // always re-validated here; it is cheap compared with a dispatch.
        if self.checks {
            validate_bindings(
                &p.common,
                table,
                &self.slots,
                &self.samplers,
                &self.images,
                &self.buffers,
                &self.ctx.as_ref().expect("device").format_caps,
                Some(&mut storage_images),
            )?;
        } else {
            for b in &p.common.bindings {
                if let BindingKind::StorageTexture { .. } = b.kind
                    && let Ok(SlotContent::StorageImage { image }) =
                        self.slots.get(table.slots[b.binding as usize])
                {
                    storage_images.push(*image);
                }
            }
        }
        let writes_buffers = p
            .common
            .bindings
            .iter()
            .any(|b| matches!(b.kind, BindingKind::StorageBuffer { writable: true, .. }));
        let device = &self.ctx.as_ref().expect("device").device;
        let cb = self.frames[self.frame_index].cb;
        // SAFETY: recording outside a pass. Every binding the shader uses was
        // validated; storage images are moved to GENERAL for the dispatch and
        // back afterwards, which also orders the writes before later reads.
        unsafe {
            for handle in &storage_images {
                let image = self.images.get(*handle).expect("validated");
                image_barrier(device, cb, image.vk, image.layout, vk::ImageLayout::GENERAL);
            }
            if table.dirty {
                send_push_data(device, cb, &p.common, table);
                table.dirty = false;
                self.stats.push_updates += 1;
            }
            device.cmd_dispatch(cb, x, y, z);
            for handle in &storage_images {
                let image = self.images.get(*handle).expect("validated");
                image_barrier(device, cb, image.vk, vk::ImageLayout::GENERAL, image.rest);
            }
            if writes_buffers {
                let barriers = [vk::MemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(vk::AccessFlags2::SHADER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .dst_access_mask(
                        vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE,
                    )];
                device.cmd_pipeline_barrier2(
                    cb,
                    &vk::DependencyInfo::default().memory_barriers(&barriers),
                );
            }
        }
        self.stats.barriers += 2 * storage_images.len() as u64 + u64::from(writes_buffers);
        for handle in storage_images {
            let image = self.images.get_mut(handle).expect("validated");
            image.layout = image.rest;
        }
        self.stats.dispatches += 1;
        Ok(())
    }
}

fn format_list(formats: &[Format]) -> String {
    formats
        .iter()
        .map(|f| f.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Sends the whole push block: the script's bytes, then the heap index of
/// every binding at the offset the pipeline's mapping reads it from.
fn send_push_data(
    device: &vulkan::Device,
    cb: vk::CommandBuffer,
    common: &PipelineCommon,
    table: &BindTable,
) {
    if common.push_total_size == 0 {
        return;
    }
    let mut data = [0u8; PUSH_CAPACITY + 4 * MAX_BINDINGS as usize];
    let user = common.push_user_size as usize;
    data[..user].copy_from_slice(&table.user[..user]);
    for b in &common.bindings {
        let index = table.slots[b.binding as usize].index();
        let at = b.push_offset as usize;
        data[at..at + 4].copy_from_slice(&index.to_ne_bytes());
    }
    // SAFETY: `cb` is recording; the data covers exactly the push block the
    // pipeline was created with, which is within the device limit.
    unsafe {
        device.cmd_push_data_ext(
            cb,
            &vk::PushDataInfoEXT::default().data(
                vk::HostAddressRangeConstEXT::default()
                    .address(&data[..common.push_total_size as usize]),
            ),
        );
    }
}

/// Checks that every binding the shaders use points at a live slot of the
/// right kind, backed by a live resource in a usable state.
///
/// For compute, `storage_images` receives the images bound as storage textures.
#[allow(clippy::too_many_arguments)]
fn validate_bindings(
    common: &PipelineCommon,
    table: &BindTable,
    slots: &Pool<SlotContent>,
    samplers: &Pool<SamplerSlot>,
    images: &Pool<Image>,
    buffers: &Pool<crate::gpu::Buffer>,
    format_caps: &[crate::format::FormatCaps],
    mut storage_images: Option<&mut Vec<Handle>>,
) -> Result<()> {
    let context = |b: &crate::gpu::PipelineBinding, e: GfxError| {
        GfxError::new(
            e.code,
            format!("binding {} (`{}`): {}", b.binding, b.name, e.message),
        )
    };
    let mut sampled: [Option<(Handle, Format)>; MAX_BINDINGS as usize] =
        [None; MAX_BINDINGS as usize];
    let mut filtering = [false; MAX_BINDINGS as usize];
    for b in &common.bindings {
        if !b.used {
            continue;
        }
        let handle = table.slots[b.binding as usize];
        if handle.is_null() {
            bail!(
                MissingBinding,
                "binding {} (`{}`) needs a {} but nothing is bound",
                b.binding,
                b.name,
                b.kind.describe()
            );
        }
        if let BindingKind::Sampler = b.kind {
            let sampler = samplers.get(handle).map_err(|e| context(b, e))?;
            filtering[b.binding as usize] = sampler.filtering;
            continue;
        }
        let content = slots.get(handle).map_err(|e| context(b, e))?;
        let mismatch = || {
            GfxError::new(
                ErrorCode::Mismatch,
                format!(
                    "binding {} (`{}`) needs a {} but the slot holds {}",
                    b.binding,
                    b.name,
                    b.kind.describe(),
                    content.describe()
                ),
            )
        };
        if let BindingKind::TextureArray { class, count } = b.kind {
            // The shader can reach any of the `count` slots that follow the
            // bound one, so each of them must be a usable texture.
            for element in 0..count {
                let index = handle.index() + element;
                let Some(SlotContent::SampledImage { image }) = slots.get_by_index(index) else {
                    bail!(
                        MissingBinding,
                        "binding {} (`{}`): element {element} of the texture array (heap slot {index}) is not a live texture slot",
                        b.binding,
                        b.name
                    );
                };
                let i = images.get(*image).map_err(|e| context(b, e))?;
                if i.format.class() != class {
                    bail!(
                        Mismatch,
                        "binding {} (`{}`): element {element} samples {class:?} but the image is {}",
                        b.binding,
                        b.name,
                        i.format.name()
                    );
                }
                if i.in_pass || i.layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
                    bail!(
                        InvalidState,
                        "binding {} (`{}`): element {element} is an attachment of the open pass or has no contents yet",
                        b.binding,
                        b.name
                    );
                }
            }
            continue;
        }
        match (b.kind, *content) {
            (BindingKind::SampledTexture { class }, SlotContent::SampledImage { image }) => {
                let i = images.get(image).map_err(|e| context(b, e))?;
                if i.format.class() != class {
                    bail!(
                        Mismatch,
                        "binding {} (`{}`) samples {class:?} but the image is {}",
                        b.binding,
                        b.name,
                        i.format.name()
                    );
                }
                if i.in_pass {
                    bail!(
                        InvalidState,
                        "binding {} (`{}`): the image is an attachment of the open pass",
                        b.binding,
                        b.name
                    );
                }
                if i.layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
                    bail!(
                        InvalidState,
                        "binding {} (`{}`): the image was created inside this pass and cannot be sampled yet",
                        b.binding,
                        b.name
                    );
                }
                sampled[b.binding as usize] = Some((image, i.format));
            }
            (BindingKind::StorageTexture { format, .. }, SlotContent::StorageImage { image }) => {
                let i = images.get(image).map_err(|e| context(b, e))?;
                if i.format != format {
                    bail!(
                        Mismatch,
                        "binding {} (`{}`) is declared {} but the image is {}",
                        b.binding,
                        b.name,
                        format.name(),
                        i.format.name()
                    );
                }
                match storage_images.as_deref_mut() {
                    Some(list) => {
                        if !list.contains(&image) {
                            list.push(image);
                        }
                    }
                    None => bail!(
                        Unsupported,
                        "storage textures are only supported in compute"
                    ),
                }
            }
            (
                BindingKind::UniformBuffer { min_size },
                SlotContent::UniformBuffer { buffer, size },
            )
            | (
                BindingKind::StorageBuffer { min_size, .. },
                SlotContent::StorageBuffer { buffer, size },
            ) => {
                buffers.get(buffer).map_err(|e| context(b, e))?;
                if size < min_size {
                    bail!(
                        Mismatch,
                        "binding {} (`{}`) needs at least {min_size} bytes but the slot covers {size}",
                        b.binding,
                        b.name
                    );
                }
            }
            _ => return Err(mismatch()),
        }
    }
    for (texture, sampler) in &common.sampling_pairs {
        if let Some((_, format)) = sampled[*texture as usize]
            && filtering[*sampler as usize]
        {
            let index = Format::ALL
                .iter()
                .position(|f| *f == format)
                .expect("format is in ALL");
            if !format_caps[index].filter_linear {
                bail!(
                    Mismatch,
                    "binding {texture}: {} cannot be sampled with a linear filter on this device",
                    format.name()
                );
            }
        }
    }
    if let Some(list) = storage_images {
        for image in list.iter() {
            if sampled.iter().flatten().any(|(s, _)| s == image) {
                bail!(
                    InvalidState,
                    "an image is bound both as a sampled texture and as a storage texture"
                );
            }
        }
    }
    Ok(())
}
