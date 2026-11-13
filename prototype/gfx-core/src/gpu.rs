//! The checked GPU API: objects, their lifetime, and the descriptor heaps.
//!
//! Recording (frames, passes, draws, compute) lives in [`crate::record`].
//!
//! Invariants this module maintains so that no call can reach undefined
//! behaviour in the driver:
//!
//! - A Vulkan object is destroyed only after every command buffer that could
//!   refer to it has finished executing ([`Gpu::retire`]).
//! - A heap slot is reused only after the same condition holds, so a recorded
//!   draw never reads a descriptor that was rewritten later.
//! - Between operations every image is in its *rest layout*; the API issues
//!   every layout transition itself.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};

use vulkan::vk;
use vulkan_alloc::{Allocation, AllocationCreateDesc, AllocationScheme, MemoryLocation};

use crate::background::{Background, Inner, PendingPipeline, PipelineJob};
use crate::checked::{CheckedShader, ShaderChecker};
use crate::desc::*;
use crate::device::{DeviceCtx, vk_error};
use crate::error::{ErrorCode, GfxError, Result, bail};
use crate::format::{Format, FormatCaps, NumClass};
use crate::handle::{Handle, Kind, Pool};
use crate::record::{BindTable, PassState};
use crate::shader::{BindingKind, CompiledShader, MAX_BINDINGS, Reflection, ShaderStage};

static NEXT_EPOCH: AtomicU32 = AtomicU32::new(1);

pub(crate) const FRAMES_IN_FLIGHT: usize = 2;
/// Largest SPIR-V module accepted, in bytes.
pub const MAX_SPIRV_BYTES: usize = 1 << 20;
/// Stack of the thread the SPIR-V checks run on. Only touched pages are
/// committed.
const COMPILER_STACK_BYTES: usize = 1 << 30;
pub(crate) const MAX_VERTEX_BUFFERS: usize = 8;
const MAX_COLOR_TARGETS: usize = 8;

pub(crate) struct Buffer {
    pub vk: vk::Buffer,
    pub allocation: Allocation,
    pub mapped: *mut u8,
    pub coherent: bool,
    pub address: u64,
    pub size: u64,
    pub usage: BufferUsage,
}

/// Where the memory of an image comes from.
pub(crate) enum ImageMemory {
    /// From the allocator.
    Allocated(Allocation),
    /// Imported from a dma-buf; the memory object is the image's own.
    Imported(vk::DeviceMemory),
}

pub(crate) struct Image {
    pub vk: vk::Image,
    /// Attachment views, one per mip level and layer, at index
    /// `mip * layers + layer`. Empty unless the image can be an attachment.
    pub views: Vec<vk::ImageView>,
    pub memory: ImageMemory,
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D image; 1 otherwise.
    pub depth: u32,
    /// Array layers: 6 for a cube image, 1 otherwise.
    pub layers: u32,
    pub mip_levels: u32,
    pub samples: u32,
    pub kind: ImageKind,
    pub usage: ImageUsage,
    /// Layout the image is in at the current point of recording. While the
    /// image is an attachment of the open pass, this is the layout of every
    /// subresource except the attached one.
    pub layout: vk::ImageLayout,
    /// Layout the image returns to after every operation.
    pub rest: vk::ImageLayout,
    /// Whether a subresource of the image is an attachment of the open pass.
    pub in_pass: bool,
    /// The attached subresource while `in_pass`.
    pub attached: Subresource,
    /// Memory the image is charged for: every level, layer and sample.
    pub bytes: u64,
}

impl Image {
    /// Every mip level and layer.
    pub fn full_range(&self) -> vk::ImageSubresourceRange {
        vk::ImageSubresourceRange {
            aspect_mask: self.format.aspect(),
            base_mip_level: 0,
            level_count: self.mip_levels,
            base_array_layer: 0,
            layer_count: self.layers,
        }
    }

    pub fn sub_range(&self, sub: Subresource) -> vk::ImageSubresourceRange {
        vk::ImageSubresourceRange {
            aspect_mask: self.format.aspect(),
            base_mip_level: sub.mip,
            level_count: 1,
            base_array_layer: sub.layer,
            layer_count: 1,
        }
    }

    pub fn sub_layers(&self, sub: Subresource) -> vk::ImageSubresourceLayers {
        vk::ImageSubresourceLayers {
            aspect_mask: self.format.aspect(),
            mip_level: sub.mip,
            base_array_layer: sub.layer,
            layer_count: 1,
        }
    }

    /// Width, height and depth of a mip level.
    pub fn level_extent(&self, mip: u32) -> (u32, u32, u32) {
        (
            (self.width >> mip).max(1),
            (self.height >> mip).max(1),
            (self.depth >> mip).max(1),
        )
    }

    /// Bytes of one layer of a mip level, tightly packed.
    pub fn level_bytes(&self, mip: u32) -> u64 {
        let (w, h, d) = self.level_extent(mip);
        u64::from(w) * u64::from(h) * u64::from(d) * u64::from(self.format.bytes_per_pixel())
    }

    /// Checks that `sub` names a mip level and a layer the image has.
    pub fn check_sub(&self, sub: Subresource, image: Handle) -> Result<()> {
        if sub.mip >= self.mip_levels || sub.layer >= self.layers {
            bail!(
                OutOfRange,
                "{image:?} has {} mip level(s) and {} layer(s); mip {} of layer {} does not exist",
                self.mip_levels,
                self.layers,
                sub.mip,
                sub.layer
            );
        }
        Ok(())
    }

    pub fn attachment_view(&self, sub: Subresource) -> vk::ImageView {
        self.views[(sub.mip * self.layers + sub.layer) as usize]
    }
}

/// The part of an image a sampled-texture slot shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotView {
    /// How the shader must declare the texture.
    pub kind: ImageKind,
    pub base_mip: u32,
    pub mip_count: u32,
    pub base_layer: u32,
    pub layer_count: u32,
}

impl SlotView {
    pub fn contains(&self, sub: Subresource) -> bool {
        (self.base_mip..self.base_mip + self.mip_count).contains(&sub.mip)
            && (self.base_layer..self.base_layer + self.layer_count).contains(&sub.layer)
    }
}

pub(crate) struct Shader {
    pub spirv: Vec<u32>,
    pub entry: std::ffi::CString,
    pub reflection: Reflection,
}

impl Shader {
    fn stage(&self) -> StageRef<'_> {
        StageRef {
            spirv: &self.spirv,
            entry: &self.entry,
            reflection: &self.reflection,
        }
    }
}

/// What building a pipeline reads of one of its shaders.
#[derive(Copy, Clone)]
pub(crate) struct StageRef<'a> {
    pub spirv: &'a [u32],
    pub entry: &'a std::ffi::CStr,
    pub reflection: &'a Reflection,
}

/// A copy of the same, for a build that outlives the shader.
pub(crate) struct StageCode {
    spirv: Vec<u32>,
    entry: std::ffi::CString,
    reflection: Reflection,
}

impl StageCode {
    pub fn as_ref(&self) -> StageRef<'_> {
        StageRef {
            spirv: &self.spirv,
            entry: &self.entry,
            reflection: &self.reflection,
        }
    }
}

impl From<StageRef<'_>> for StageCode {
    fn from(stage: StageRef<'_>) -> Self {
        Self {
            spirv: stage.spirv.to_vec(),
            entry: stage.entry.to_owned(),
            reflection: stage.reflection.clone(),
        }
    }
}

/// Bytes from one slot to the next in the two descriptor heaps: what a
/// pipeline's mapping of bindings to the heaps depends on.
#[derive(Copy, Clone)]
pub(crate) struct HeapStrides {
    resource: u32,
    sampler: u32,
}

impl HeapStrides {
    fn of(ctx: &DeviceCtx) -> Self {
        Self {
            resource: ctx.resource_heap.stride as u32,
            sampler: ctx.sampler_heap.stride as u32,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PipelineBinding {
    pub binding: u32,
    pub name: String,
    pub kind: BindingKind,
    /// Byte offset in push data where the heap index of this binding lives.
    pub push_offset: u32,
    pub used: bool,
}

pub(crate) struct PipelineCommon {
    pub vk: vk::Pipeline,
    pub bindings: Vec<PipelineBinding>,
    /// Size of the shader's own push-constant block.
    pub push_user_size: u32,
    /// Size of the block plus the heap indices that follow it.
    pub push_total_size: u32,
    pub sampling_pairs: Vec<(u32, u32)>,
    /// Those of `sampling_pairs` that are used for depth comparisons.
    pub comparison_pairs: Vec<(u32, u32)>,
}

pub(crate) struct VertexBufferRequirement {
    pub stride: u64,
    pub per_instance: bool,
    /// Bytes one element reads, counted from its start.
    pub element_size: u64,
}

pub(crate) struct RenderPipeline {
    pub common: PipelineCommon,
    pub color_formats: Vec<Format>,
    pub depth_format: Option<Format>,
    pub samples: u32,
    pub vertex_buffers: Vec<VertexBufferRequirement>,
}

pub(crate) struct ComputePipeline {
    pub common: PipelineCommon,
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum SlotContent {
    SampledImage { image: Handle, view: SlotView },
    StorageImage { image: Handle },
    UniformBuffer { buffer: Handle, size: u64 },
    StorageBuffer { buffer: Handle, size: u64 },
}

impl SlotContent {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::SampledImage { .. } => "a sampled texture",
            Self::StorageImage { .. } => "a storage texture",
            Self::UniformBuffer { .. } => "a uniform buffer",
            Self::StorageBuffer { .. } => "a storage buffer",
        }
    }
}

pub(crate) struct SamplerSlot {
    /// Whether any of the sampler's filters is linear.
    pub filtering: bool,
    /// Whether an address mode returns the (floating-point) border colour.
    pub border: bool,
    /// Whether the sampler compares (`SamplerExtra::compare`).
    pub compare: bool,
}

pub(crate) enum Garbage {
    Buffer {
        vk: vk::Buffer,
        allocation: Allocation,
        bytes: u64,
    },
    Image {
        vk: vk::Image,
        views: Vec<vk::ImageView>,
        memory: ImageMemory,
        bytes: u64,
    },
    Pipeline(vk::Pipeline),
    ResourceSlot(u32),
    SamplerSlot(u32),
}

pub(crate) struct FrameSlot {
    pub pool: vk::CommandPool,
    pub cb: vk::CommandBuffer,
    pub fence: vk::Fence,
    pub serial: u64,
    pub pending: bool,
}

/// Counters for tests and measurements.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub draws: u64,
    pub dispatches: u64,
    pub barriers: u64,
    pub push_updates: u64,
    pub frames_submitted: u64,
    pub live_buffers: u32,
    pub live_images: u32,
    pub live_slots: u32,
    pub live_samplers: u32,
    pub live_pipelines: u32,
    pub pending_garbage: u32,
    pub memory_bytes: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub kind: ImageKind,
    /// Depth of a 3D image; 1 otherwise.
    pub depth: u32,
    pub mip_levels: u32,
    pub samples: u32,
}

/// A device plus everything a script created on it.
pub struct Gpu {
    pub(crate) ctx: Option<DeviceCtx>,
    pub(crate) config: GpuConfig,
    pub(crate) checks: bool,
    /// Why the device is unusable, if it is.
    pub(crate) lost: Option<(ErrorCode, String)>,
    /// Set when the GPU did not return in time; teardown must not touch it.
    pub(crate) hung: bool,
    /// Objects that belong to the host. See [`Gpu::protect`].
    pub(crate) protected: std::collections::HashSet<Handle>,

    pub(crate) buffers: Pool<Buffer>,
    pub(crate) images: Pool<Image>,
    pub(crate) shaders: Pool<Shader>,
    pub(crate) render_pipelines: Pool<RenderPipeline>,
    pub(crate) compute_pipelines: Pool<ComputePipeline>,
    pub(crate) slots: Pool<SlotContent>,
    pub(crate) samplers: Pool<SamplerSlot>,
    pub(crate) memory_bytes: u64,
    /// Bumped whenever an object is destroyed; invalidates cached validation.
    pub(crate) mutation: u64,

    pub(crate) frames: Vec<FrameSlot>,
    pub(crate) frame_index: usize,
    pub(crate) recording: bool,
    pub(crate) submitted_serial: u64,
    pub(crate) completed_serial: u64,
    pub(crate) garbage: VecDeque<(u64, Garbage)>,
    /// Images that still need their first transition out of `UNDEFINED`.
    pub(crate) pending_init: Vec<Handle>,
    /// Imported images whose contents the foreign owner has (re)written:
    /// ownership is acquired at the start of the next frame.
    pub(crate) pending_acquire: Vec<Handle>,
    util_pool: vk::CommandPool,
    util_cb: vk::CommandBuffer,
    util_fence: vk::Fence,

    pub(crate) pass: Option<PassState>,
    pub(crate) graphics_binds: BindTable,
    pub(crate) compute_binds: BindTable,
    pub(crate) compute_pipeline: Option<Handle>,
    pub(crate) stats: Stats,

    /// Shared with the jobs that build pipelines on other threads.
    background: std::sync::Arc<Background>,
    /// Pipelines that were prepared and not finished yet, by ticket: all of
    /// the pipeline except its Vulkan object.
    pending_pipelines: std::collections::HashMap<u64, RenderPipeline>,
    next_ticket: u64,
}

impl Gpu {
    pub fn new(config: GpuConfig) -> Result<Self> {
        let limits = config.limits;
        let mut ctx = DeviceCtx::create(
            config.validation,
            config.device_filter.as_deref(),
            config.drm_node,
            limits.resource_slots,
            limits.sampler_slots,
        )?;
        // SAFETY: the context is fully created; the queries only read.
        ctx.dmabuf_formats = unsafe { crate::dmabuf::query_import_formats(&ctx) };
        let epoch = (NEXT_EPOCH.fetch_add(1, Ordering::SeqCst) % 255 + 1) as u8;

        // SAFETY: `ctx.device` is valid; the objects are destroyed in `teardown`.
        let created = unsafe { create_frame_objects(&ctx) };
        let (frames, util_pool, util_cb, util_fence) = match created {
            Ok(created) => created,
            Err(e) => {
                // SAFETY: nothing was submitted and no other object exists.
                unsafe {
                    ctx.device.device_wait_idle().ok();
                    ctx.destroy();
                }
                return Err(e);
            }
        };
        let _ = &mut ctx;

        Ok(Self {
            buffers: Pool::new(Kind::Buffer, epoch, limits.max_buffers),
            images: Pool::new(Kind::Image, epoch, limits.max_images),
            shaders: Pool::new(Kind::Shader, epoch, limits.max_shaders),
            render_pipelines: Pool::new(Kind::RenderPipeline, epoch, limits.max_pipelines),
            compute_pipelines: Pool::new(Kind::ComputePipeline, epoch, limits.max_pipelines),
            slots: Pool::new(Kind::Slot, epoch, ctx.resource_heap.slots),
            samplers: Pool::new(Kind::Sampler, epoch, ctx.sampler_heap.slots),
            ctx: Some(ctx),
            config,
            checks: true,
            lost: None,
            hung: false,
            protected: std::collections::HashSet::new(),
            memory_bytes: 0,
            mutation: 0,
            frames,
            frame_index: 0,
            recording: false,
            submitted_serial: 0,
            completed_serial: 0,
            garbage: VecDeque::new(),
            pending_init: Vec::new(),
            pending_acquire: Vec::new(),
            util_pool,
            util_cb,
            util_fence,
            pass: None,
            graphics_binds: BindTable::default(),
            compute_binds: BindTable::default(),
            compute_pipeline: None,
            stats: Stats::default(),
            background: Background::new(),
            pending_pipelines: std::collections::HashMap::new(),
            next_ticket: 0,
        })
    }

    pub(crate) fn ctx(&self) -> &DeviceCtx {
        self.ctx.as_ref().expect("device lives until drop")
    }

    pub fn device_name(&self) -> &str {
        &self.ctx().device_name
    }

    pub fn driver_name(&self) -> &str {
        &self.ctx().driver_name
    }

    /// Descriptor sizes and heap geometry, for the report.
    pub fn heap_info(&self) -> String {
        let ctx = self.ctx();
        format!(
            "descriptor sizes: image {} buffer {} sampler {}; resource heap {} slots x {} bytes (+{} reserved); sampler heap {} slots x {} bytes (+{} reserved); max push data {}",
            ctx.limits.image_descriptor_size,
            ctx.limits.buffer_descriptor_size,
            ctx.limits.sampler_descriptor_size,
            ctx.resource_heap.slots,
            ctx.resource_heap.stride,
            ctx.resource_heap.reserved_size,
            ctx.sampler_heap.slots,
            ctx.sampler_heap.stride,
            ctx.sampler_heap.reserved_size,
            ctx.limits.max_push_data_size,
        )
    }

    pub fn format_caps(&self, format: Format) -> FormatCaps {
        self.ctx().format_caps[format.index()]
    }

    /// Whether images of `format` can be created with `usage` on this
    /// device.
    pub fn format_supports_usage(&self, format: Format, usage: ImageUsage) -> bool {
        let caps = self.format_caps(format);
        usage_table(&caps)
            .iter()
            .all(|(flag, supported, _, _)| !usage.contains(*flag) || *supported)
    }

    /// Turns the per-call validation off or on. Handle resolution stays.
    ///
    /// With checks off a wrong call is forwarded to Vulkan, so this exists
    /// only to measure what the checks cost.
    ///
    /// # Safety
    ///
    /// While checks are off the caller must make only calls that the checked
    /// API would accept.
    pub unsafe fn set_checks(&mut self, enabled: bool) {
        self.checks = enabled;
    }

    pub fn stats(&self) -> Stats {
        Stats {
            live_buffers: self.buffers.live(),
            live_images: self.images.live(),
            live_slots: self.slots.live(),
            live_samplers: self.samplers.live(),
            live_pipelines: self.render_pipelines.live() + self.compute_pipelines.live(),
            pending_garbage: self.garbage.len() as u32,
            memory_bytes: self.memory_bytes,
            ..self.stats
        }
    }

    /// Whether the device has been declared lost.
    pub fn is_lost(&self) -> bool {
        self.lost.is_some()
    }

    /// Marks the device lost without involving the driver, to exercise the
    /// recovery path (drop this `Gpu`, create a new one, re-run the script's
    /// setup).
    pub fn simulate_device_loss(&mut self) {
        self.lost = Some((ErrorCode::DeviceLost, "device loss was simulated".into()));
    }

    pub(crate) fn alive(&self) -> Result<()> {
        match &self.lost {
            None => Ok(()),
            Some((_, reason)) => Err(GfxError::new(
                ErrorCode::DeviceLost,
                format!("the device is lost ({reason}); it must be recreated"),
            )),
        }
    }

    /// Converts a Vulkan error, recording device loss.
    pub(crate) fn fail(&mut self, context: &str, result: vk::VkResult) -> GfxError {
        let error = vk_error(context, result);
        if error.code == ErrorCode::DeviceLost {
            self.lost = Some((ErrorCode::DeviceLost, error.message.clone()));
        }
        error
    }

    // ----- lifetime ---------------------------------------------------------

    /// Queues an object for destruction once the GPU can no longer use it.
    pub(crate) fn retire(&mut self, garbage: Garbage) {
        let serial = if self.recording {
            self.submitted_serial + 1
        } else {
            self.submitted_serial
        };
        if serial <= self.completed_serial {
            self.destroy_garbage(garbage);
        } else {
            self.garbage.push_back((serial, garbage));
        }
    }

    fn destroy_garbage(&mut self, garbage: Garbage) {
        let ctx = self.ctx.as_mut().expect("device lives until drop");
        // SAFETY: the serial check in `retire`/`collect_garbage` guarantees no
        // pending command buffer refers to the object.
        unsafe {
            match garbage {
                Garbage::Buffer {
                    vk,
                    allocation,
                    bytes,
                } => {
                    let _ = ctx.allocator().unmap_memory(&allocation);
                    ctx.device.destroy_buffer(vk, None);
                    let _ = ctx.allocator().free(allocation);
                    self.memory_bytes -= bytes;
                }
                Garbage::Image {
                    vk,
                    views,
                    memory,
                    bytes,
                } => {
                    for view in views {
                        if !view.is_null() {
                            ctx.device.destroy_image_view(view, None);
                        }
                    }
                    ctx.device.destroy_image(vk, None);
                    match memory {
                        ImageMemory::Allocated(allocation) => {
                            let _ = ctx.allocator().free(allocation);
                        }
                        ImageMemory::Imported(memory) => ctx.device.free_memory(memory, None),
                    }
                    self.memory_bytes -= bytes;
                }
                Garbage::Pipeline(pipeline) => ctx.device.destroy_pipeline(pipeline, None),
                Garbage::ResourceSlot(index) => self.slots.release_index(index),
                Garbage::SamplerSlot(index) => self.samplers.release_index(index),
            }
        }
    }

    pub(crate) fn collect_garbage(&mut self) {
        while self
            .garbage
            .front()
            .is_some_and(|(serial, _)| *serial <= self.completed_serial)
        {
            let (_, garbage) = self.garbage.pop_front().expect("checked");
            self.destroy_garbage(garbage);
        }
    }

    /// Notes which submitted frames have finished, without blocking.
    pub(crate) fn poll_frames(&mut self) -> Result<()> {
        for i in 0..self.frames.len() {
            if !self.frames[i].pending {
                continue;
            }
            // SAFETY: the fence is owned by this frame slot.
            let status = unsafe { self.ctx().device.get_fence_status(self.frames[i].fence) };
            match status {
                Ok(vk::VkResult::SUCCESS) => {
                    self.frames[i].pending = false;
                    self.completed_serial = self.completed_serial.max(self.frames[i].serial);
                }
                Ok(_) => {}
                Err(e) => return Err(self.fail("fence status", e)),
            }
        }
        self.collect_garbage();
        Ok(())
    }

    /// Blocks until frame slot `index` has finished, up to the configured timeout.
    pub(crate) fn wait_frame(&mut self, index: usize) -> Result<()> {
        if !self.frames[index].pending {
            return Ok(());
        }
        let timeout = self.config.gpu_timeout.as_nanos().min(u128::from(u64::MAX)) as u64;
        // SAFETY: the fence is owned by this frame slot.
        let result = unsafe {
            self.ctx()
                .device
                .wait_for_fences(&[self.frames[index].fence], true, timeout)
        };
        match result {
            Ok(vk::VkResult::SUCCESS) => {
                self.frames[index].pending = false;
                self.completed_serial = self.completed_serial.max(self.frames[index].serial);
                Ok(())
            }
            Ok(_) => {
                // The GPU is still working on something we submitted. Nothing
                // can be destroyed or reused safely any more.
                let message = format!(
                    "the GPU did not finish a frame within {:?}",
                    self.config.gpu_timeout
                );
                self.lost = Some((ErrorCode::Timeout, message.clone()));
                self.hung = true;
                Err(GfxError::new(ErrorCode::Timeout, message))
            }
            Err(e) => Err(self.fail("wait for frame", e)),
        }
    }

    /// Waits for every submitted frame and frees what became unused.
    pub fn wait_idle(&mut self) -> Result<()> {
        self.alive()?;
        for i in 0..self.frames.len() {
            self.wait_frame(i)?;
        }
        self.collect_garbage();
        Ok(())
    }

    pub(crate) fn charge_memory(&self, bytes: u64, what: &str) -> Result<()> {
        let limit = self.config.limits.max_memory_bytes;
        if self.memory_bytes.saturating_add(bytes) > limit {
            bail!(
                LimitExceeded,
                "{what} of {bytes} bytes exceeds the memory budget ({} of {limit} bytes in use, including objects the GPU has not released yet)",
                self.memory_bytes
            );
        }
        Ok(())
    }

    // ----- buffers ----------------------------------------------------------

    pub fn create_buffer(&mut self, desc: &BufferDesc) -> Result<Handle> {
        self.alive()?;
        if desc.size == 0 {
            bail!(InvalidArgument, "buffer size must not be 0");
        }
        if desc.usage.is_empty() {
            bail!(InvalidArgument, "buffer usage must not be empty");
        }
        let max = self
            .ctx()
            .limits
            .max_buffer_size
            .min(self.ctx().limits.largest_heap_bytes);
        if desc.size > max {
            bail!(
                LimitExceeded,
                "buffer size {} exceeds the device limit {max}",
                desc.size
            );
        }
        self.charge_memory(desc.size, "a buffer")?;
        if self.buffers.live() >= self.buffers.capacity() {
            bail!(
                LimitExceeded,
                "too many buffers (limit {})",
                self.buffers.capacity()
            );
        }

        let mut usage = vk::BufferUsageFlags::empty();
        if desc.usage.contains(BufferUsage::VERTEX) {
            usage |= vk::BufferUsageFlags::VERTEX_BUFFER;
        }
        if desc.usage.contains(BufferUsage::INDEX) {
            usage |= vk::BufferUsageFlags::INDEX_BUFFER;
        }
        if desc.usage.contains(BufferUsage::UNIFORM) {
            usage |= vk::BufferUsageFlags::UNIFORM_BUFFER;
        }
        if desc.usage.contains(BufferUsage::STORAGE) {
            usage |= vk::BufferUsageFlags::STORAGE_BUFFER;
        }
        let needs_address =
            desc.usage.contains(BufferUsage::UNIFORM) || desc.usage.contains(BufferUsage::STORAGE);
        if needs_address {
            usage |= vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS;
        }

        // SAFETY: the create info is valid (non-zero size within limits, known
        // usage bits); partially created objects are released on every error path.
        let buffer = unsafe {
            let ctx = self.ctx.as_mut().expect("device lives until drop");
            let vk_buffer = ctx
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(desc.size)
                        .usage(usage)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(|e| vk_error("create buffer", e));
            let vk_buffer = match vk_buffer {
                Ok(b) => b,
                Err(e) => return Err(self.note_loss(e)),
            };
            let requirements = ctx.device.get_buffer_memory_requirements(vk_buffer);
            let allocation = match ctx.allocator().allocate(AllocationCreateDesc {
                name: Some("script buffer"),
                requirements,
                location: MemoryLocation::CpuToGpu,
                linear: true,
                memory_allocate_flags: if needs_address {
                    vk::MemoryAllocateFlags::DEVICE_ADDRESS
                } else {
                    vk::MemoryAllocateFlags::empty()
                },
                export_handle_types: vk::ExternalMemoryHandleTypeFlags::empty(),
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            }) {
                Ok(a) => a,
                Err(e) => {
                    ctx.device.destroy_buffer(vk_buffer, None);
                    bail!(OutOfMemory, "allocate buffer memory: {e}");
                }
            };
            let mapped = ctx
                .device
                .bind_buffer_memory(vk_buffer, allocation.memory(), allocation.offset())
                .map_err(|e| vk_error("bind buffer memory", e))
                .and_then(|()| {
                    ctx.allocator()
                        .map_memory(&allocation)
                        .map_err(|e| GfxError::new(ErrorCode::Internal, format!("map buffer: {e}")))
                });
            let mapped = match mapped {
                Ok(m) => m.cast::<u8>(),
                Err(e) => {
                    ctx.device.destroy_buffer(vk_buffer, None);
                    let _ = ctx.allocator().free(allocation);
                    return Err(self.note_loss(e));
                }
            };
            // A new buffer reads as zeros, never as leftover memory.
            std::ptr::write_bytes(mapped, 0, desc.size as usize);
            let coherent = ctx
                .allocator()
                .allocation_memory_flags(&allocation)
                .is_ok_and(|f| f.contains(vk::MemoryPropertyFlags::HOST_COHERENT));
            if !coherent {
                let _ = ctx.allocator().flush_allocation(&allocation);
            }
            let address = if needs_address {
                ctx.device.get_buffer_device_address(
                    &vk::BufferDeviceAddressInfo::default().buffer(vk_buffer),
                )
            } else {
                0
            };
            Buffer {
                vk: vk_buffer,
                allocation,
                mapped,
                coherent,
                address,
                size: desc.size,
                usage: desc.usage,
            }
        };
        self.memory_bytes += desc.size;
        match self.buffers.insert(buffer) {
            Ok(handle) => Ok(handle),
            Err((buffer, e)) => {
                self.retire(Garbage::Buffer {
                    vk: buffer.vk,
                    allocation: buffer.allocation,
                    bytes: buffer.size,
                });
                Err(e)
            }
        }
    }

    fn note_loss(&mut self, error: GfxError) -> GfxError {
        if error.code == ErrorCode::DeviceLost {
            self.lost = Some((ErrorCode::DeviceLost, error.message.clone()));
        }
        error
    }

    /// Copies `data` into the buffer at `offset`.
    ///
    /// The write happens immediately, in host memory. Draws that were already
    /// recorded in the current frame read the buffer when the GPU executes
    /// them, so they see the new contents too.
    pub fn write_buffer(&mut self, buffer: Handle, offset: u64, data: &[u8]) -> Result<()> {
        self.alive()?;
        let b = self.buffers.get(buffer)?;
        let end = offset.checked_add(data.len() as u64);
        if end.is_none_or(|end| end > b.size) {
            bail!(
                OutOfRange,
                "write of {} bytes at offset {offset} exceeds {buffer:?} of {} bytes",
                data.len(),
                b.size
            );
        }
        // SAFETY: the range was checked against the buffer size, and the
        // mapping stays valid until the buffer is actually destroyed.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), b.mapped.add(offset as usize), data.len());
            if !b.coherent {
                let _ = self
                    .ctx()
                    .allocator
                    .as_ref()
                    .expect("allocator")
                    .flush_allocation(&b.allocation);
            }
        }
        Ok(())
    }

    pub fn buffer_size(&self, buffer: Handle) -> Result<u64> {
        Ok(self.buffers.get(buffer)?.size)
    }

    // ----- images -----------------------------------------------------------

    pub fn create_image(&mut self, desc: &ImageDesc) -> Result<Handle> {
        self.create_image_with(desc, &ImageExtra::default())
    }

    /// Creates a 3D image of `desc.width × desc.height × depth` texels, for
    /// lookup tables.
    pub fn create_image_3d(&mut self, desc: &ImageDesc, depth: u32) -> Result<Handle> {
        self.create_image_with(
            desc,
            &ImageExtra {
                kind: ImageKind::D3,
                depth,
                ..ImageExtra::default()
            },
        )
    }

    /// Creates an image with mip levels, of another kind than 2D, or with
    /// more than one sample per texel.
    ///
    /// - A 3D image can only be sampled and filled with `upload_image`.
    /// - A cube image is square and has six layers.
    /// - A multisampled image can only be an attachment; a pass resolves it
    ///   into a single-sampled image.
    pub fn create_image_with(&mut self, desc: &ImageDesc, extra: &ImageExtra) -> Result<Handle> {
        self.alive()?;
        let limits = &self.ctx().limits;
        let (max, what) = match extra.kind {
            ImageKind::D2 => (limits.max_image_dimension, "image"),
            ImageKind::D3 => (limits.max_image_dimension_3d, "3D image"),
            ImageKind::Cube => (limits.max_image_dimension_cube, "cube image"),
        };
        if [desc.width, desc.height, extra.depth]
            .iter()
            .any(|d| *d == 0 || *d > max)
        {
            bail!(
                InvalidArgument,
                "{what} size {}x{}x{} is outside 1..={max}",
                desc.width,
                desc.height,
                extra.depth
            );
        }
        if extra.kind != ImageKind::D3 && extra.depth != 1 {
            bail!(InvalidArgument, "only a 3D image has a depth other than 1");
        }
        if extra.kind == ImageKind::Cube && desc.width != desc.height {
            bail!(
                InvalidArgument,
                "a cube image is square, got {}x{}",
                desc.width,
                desc.height
            );
        }
        if desc.usage.is_empty() {
            bail!(InvalidArgument, "image usage must not be empty");
        }
        let full_chain = desc.width.max(desc.height).bit_width();
        if extra.mip_levels == 0 || extra.mip_levels > full_chain {
            bail!(
                InvalidArgument,
                "a {}x{} image has 1..={full_chain} mip levels, got {}",
                desc.width,
                desc.height,
                extra.mip_levels
            );
        }
        if !matches!(extra.samples, 1 | 2 | 4 | 8) {
            bail!(
                InvalidArgument,
                "samples must be 1, 2, 4 or 8, got {}",
                extra.samples
            );
        }
        let attachment = ImageUsage::COLOR_TARGET | ImageUsage::DEPTH_TARGET;
        if extra.kind == ImageKind::D3 {
            let allowed = ImageUsage::SAMPLED | ImageUsage::COPY_DST;
            if desc.usage.0 & !allowed.0 != 0 {
                bail!(
                    InvalidArgument,
                    "a 3D image may only be `sampled` and `copy_dst`"
                );
            }
            if extra.mip_levels != 1 {
                bail!(InvalidArgument, "a 3D image has one mip level");
            }
        }
        if extra.samples > 1 {
            if extra.kind != ImageKind::D2 || extra.mip_levels != 1 {
                bail!(
                    InvalidArgument,
                    "a multisampled image is 2D and has one mip level"
                );
            }
            if desc.usage.0 & !attachment.0 != 0 {
                bail!(
                    InvalidArgument,
                    "a multisampled image may only be `color_target` or `depth_target`; resolve it into another image to read it"
                );
            }
        }
        let caps = self.format_caps(desc.format);
        let name = desc.format.name();
        let mut usage = vk::ImageUsageFlags::empty();
        for (flag, supported, vk_flag, what) in usage_table(&caps) {
            if desc.usage.contains(flag) {
                if !supported {
                    bail!(
                        Unsupported,
                        "format {name} does not support usage `{what}` on this device"
                    );
                }
                usage |= vk_flag;
            }
        }
        let layers = if extra.kind == ImageKind::Cube { 6 } else { 1 };
        let image_type = if extra.kind == ImageKind::D3 {
            vk::ImageType::_3D
        } else {
            vk::ImageType::_2D
        };
        let flags = if extra.kind == ImageKind::Cube {
            vk::ImageCreateFlags::CUBE_COMPATIBLE
        } else {
            vk::ImageCreateFlags::empty()
        };
        // The device has the last word on the combination of format, kind,
        // usage, size, mip levels and samples.
        // SAFETY: the query only reads; every argument is a valid enum value.
        let properties = unsafe {
            let ctx = self.ctx();
            ctx.instance.get_physical_device_image_format_properties(
                ctx.physical_device,
                desc.format.vk(),
                image_type,
                vk::ImageTiling::OPTIMAL,
                usage,
                flags,
            )
        };
        let supported = properties.is_ok_and(|p| {
            desc.width <= p.max_extent.width
                && desc.height <= p.max_extent.height
                && extra.depth <= p.max_extent.depth
                && extra.mip_levels <= p.max_mip_levels
                && layers <= p.max_array_layers
                && p.sample_counts.as_raw() & extra.samples != 0
        });
        if !supported {
            bail!(
                Unsupported,
                "this device cannot create a {}x{}x{} {} image of {name} with {} mip level(s), {} sample(s) and this usage",
                desc.width,
                desc.height,
                extra.depth,
                extra.kind.name(),
                extra.mip_levels,
                extra.samples
            );
        }
        let bytes: u64 = (0..extra.mip_levels)
            .map(|mip| {
                u64::from((desc.width >> mip).max(1))
                    * u64::from((desc.height >> mip).max(1))
                    * u64::from((extra.depth >> mip).max(1))
            })
            .sum::<u64>()
            * u64::from(desc.format.bytes_per_pixel())
            * u64::from(layers)
            * u64::from(extra.samples);
        self.charge_memory(bytes, "an image")?;
        if self.images.live() >= self.images.capacity() {
            bail!(
                LimitExceeded,
                "too many images (limit {})",
                self.images.capacity()
            );
        }

        // SAFETY: format, kind, extent, mip levels, samples and usage were
        // validated against the device above; partially created objects are
        // released on every error path.
        let image = unsafe {
            let ctx = self.ctx.as_mut().expect("device lives until drop");
            let vk_image = match ctx.device.create_image(
                &vk::ImageCreateInfo::default()
                    .flags(flags)
                    .image_type(image_type)
                    .format(desc.format.vk())
                    .extent(vk::Extent3D {
                        width: desc.width,
                        height: desc.height,
                        depth: extra.depth,
                    })
                    .mip_levels(extra.mip_levels)
                    .array_layers(layers)
                    .samples(vk::SampleCountFlagBits::from_raw(extra.samples))
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            ) {
                Ok(i) => i,
                Err(e) => return Err(self.fail("create image", e)),
            };
            let requirements = ctx.device.get_image_memory_requirements(vk_image);
            let allocation = match ctx.allocator().allocate(AllocationCreateDesc {
                name: Some("script image"),
                requirements,
                location: MemoryLocation::GpuOnly,
                linear: false,
                memory_allocate_flags: vk::MemoryAllocateFlags::empty(),
                export_handle_types: vk::ExternalMemoryHandleTypeFlags::empty(),
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            }) {
                Ok(a) => a,
                Err(e) => {
                    ctx.device.destroy_image(vk_image, None);
                    bail!(OutOfMemory, "allocate image memory: {e}");
                }
            };
            // Only attachments need view objects, one per mip level and
            // layer; descriptors are written from a view description.
            let needs_views = desc.usage.0 & attachment.0 != 0;
            let mut views = Vec::new();
            let created = ctx
                .device
                .bind_image_memory(vk_image, allocation.memory(), allocation.offset())
                .and_then(|()| {
                    if !needs_views {
                        return Ok(());
                    }
                    for mip in 0..extra.mip_levels {
                        for layer in 0..layers {
                            views.push(
                                ctx.device.create_image_view(
                                    &vk::ImageViewCreateInfo::default()
                                        .image(vk_image)
                                        .view_type(vk::ImageViewType::_2D)
                                        .format(desc.format.vk())
                                        .subresource_range(vk::ImageSubresourceRange {
                                            aspect_mask: desc.format.aspect(),
                                            base_mip_level: mip,
                                            level_count: 1,
                                            base_array_layer: layer,
                                            layer_count: 1,
                                        }),
                                    None,
                                )?,
                            );
                        }
                    }
                    Ok(())
                });
            if let Err(e) = created {
                for view in views {
                    ctx.device.destroy_image_view(view, None);
                }
                ctx.device.destroy_image(vk_image, None);
                let _ = ctx.allocator().free(allocation);
                return Err(self.fail("create image view", e));
            }
            Image {
                vk: vk_image,
                views,
                memory: ImageMemory::Allocated(allocation),
                format: desc.format,
                width: desc.width,
                height: desc.height,
                depth: extra.depth,
                layers,
                mip_levels: extra.mip_levels,
                samples: extra.samples,
                kind: extra.kind,
                usage: desc.usage,
                layout: vk::ImageLayout::UNDEFINED,
                rest: if desc.usage.contains(ImageUsage::SAMPLED) {
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                } else {
                    vk::ImageLayout::GENERAL
                },
                in_pass: false,
                attached: Subresource::default(),
                bytes,
            }
        };
        self.memory_bytes += bytes;
        match self.images.insert(image) {
            Ok(handle) => {
                self.pending_init.push(handle);
                Ok(handle)
            }
            Err((image, e)) => {
                self.retire(Garbage::Image {
                    vk: image.vk,
                    views: image.views,
                    memory: image.memory,
                    bytes: image.bytes,
                });
                Err(e)
            }
        }
    }

    pub fn image_info(&self, image: Handle) -> Result<ImageInfo> {
        let i = self.images.get(image)?;
        Ok(ImageInfo {
            format: i.format,
            width: i.width,
            height: i.height,
            kind: i.kind,
            depth: i.depth,
            mip_levels: i.mip_levels,
            samples: i.samples,
        })
    }
    // ----- shaders ----------------------------------------------------------

    /// Creates a shader from a SPIR-V module (the bytes of a `.spv` file).
    ///
    /// The module is not trusted: it is validated, its indices are clamped,
    /// and it is checked against what the API supports before the device
    /// sees it (`spirv_input`). The reflection comes from the module itself.
    /// `entry` names its one entry point, which must be of `stage`.
    ///
    /// The checks run here, on the calling thread, and take tens of
    /// milliseconds on a large module. To keep them off this thread, check
    /// the module with [`Gpu::shader_checker`] elsewhere and hand the result
    /// to [`Gpu::create_shader_checked`].
    pub fn create_shader_spirv(
        &mut self,
        stage: ShaderStage,
        entry: &str,
        spirv: &[u8],
    ) -> Result<Handle> {
        self.check_shader_capacity()?;
        let checked = self.shader_checker().check(stage, entry, spirv)?;
        self.create_shader_checked(checked)
    }

    /// What checks SPIR-V modules for this device. It does not borrow the
    /// device: clone it, send it to another thread, and check there.
    pub fn shader_checker(&self) -> ShaderChecker {
        ShaderChecker::new(crate::spirv_check::optional_capabilities(
            &self.ctx().optional,
        ))
    }

    /// Creates a shader from a module that a [`ShaderChecker`] accepted.
    /// Nothing is checked again, so this takes no time to speak of.
    ///
    /// The checker has to be this device's, or one that allows no more than
    /// this device does (such as [`ShaderChecker::baseline`]).
    pub fn create_shader_checked(&mut self, checked: CheckedShader) -> Result<Handle> {
        self.check_shader_capacity()?;
        let allowed = crate::spirv_check::optional_capabilities(&self.ctx().optional);
        if !checked.fits(&allowed) {
            bail!(
                Mismatch,
                "the module was checked for a device with other features than this one"
            );
        }
        let stage = checked.reflection.stage;
        self.insert_shader(
            CompiledShader {
                spirv: checked.spirv,
                entry: checked.entry,
                reflection: checked.reflection,
            },
            stage,
        )
    }

    fn check_shader_capacity(&self) -> Result<()> {
        self.alive()?;
        if self.shaders.live() >= self.shaders.capacity() {
            bail!(
                LimitExceeded,
                "too many shaders (limit {})",
                self.shaders.capacity()
            );
        }
        Ok(())
    }

    fn insert_shader(&mut self, compiled: CompiledShader, stage: ShaderStage) -> Result<Handle> {
        let entry = std::ffi::CString::new(compiled.entry).map_err(|_| {
            GfxError::new(ErrorCode::ShaderCompile, "entry point name contains NUL")
        })?;
        if compiled.reflection.stage != stage {
            bail!(
                ShaderCompile,
                "the SPIR-V checks returned a shader of another stage"
            );
        }
        self.shaders
            .insert(Shader {
                spirv: compiled.spirv,
                entry,
                reflection: compiled.reflection,
            })
            .map_err(|(_, e)| e)
    }

    /// The reflected interface of a shader, for tools and tests.
    pub fn shader_reflection(&self, shader: Handle) -> Result<&Reflection> {
        Ok(&self.shaders.get(shader)?.reflection)
    }

    // ----- descriptor heap slots ---------------------------------------------

    fn write_resource_slot(
        &mut self,
        content: SlotContent,
        info: &vk::ResourceDescriptorInfoEXT<'_>,
    ) -> Result<Handle> {
        let handle = self.slots.insert(content).map_err(|(_, e)| e)?;
        let ctx = self.ctx();
        let heap = &ctx.resource_heap;
        // SAFETY: the slot index is below `heap.slots`, so the destination lies
        // inside the mapped heap and outside its reserved range. No pending
        // command buffer reads this slot: its index was free.
        let result = unsafe {
            let dst = std::slice::from_raw_parts_mut(
                heap.mapped
                    .add(handle.index() as usize * heap.stride as usize),
                heap.stride as usize,
            );
            ctx.device.write_resource_descriptors_ext(
                std::slice::from_ref(info),
                &[vk::HostAddressRangeEXT::default().address(dst)],
            )
        };
        if let Err(e) = result {
            let _ = self.slots.remove(handle);
            self.slots.release_index(handle.index());
            return Err(self.fail("write descriptor", e));
        }
        Ok(handle)
    }

    fn image_slot(
        &mut self,
        image: Handle,
        storage: bool,
        opaque: bool,
        view: &TextureView,
    ) -> Result<Handle> {
        self.alive()?;
        let i = self.images.get(image)?;
        let (needed, what, layout, ty) = if storage {
            (
                ImageUsage::STORAGE,
                "storage",
                vk::ImageLayout::GENERAL,
                vk::DescriptorType::STORAGE_IMAGE,
            )
        } else {
            (
                ImageUsage::SAMPLED,
                "sampled",
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::DescriptorType::SAMPLED_IMAGE,
            )
        };
        if !i.usage.contains(needed) {
            bail!(
                InvalidArgument,
                "{image:?} was not created with usage `{what}`"
            );
        }
        let mut view = *view;
        if storage && view.mip_count.is_none() {
            view.mip_count = Some(1);
        }
        let slot_view = resolve_view(i, image, &view)?;
        if storage && (slot_view.kind != ImageKind::D2 || slot_view.mip_count != 1) {
            bail!(
                InvalidArgument,
                "a storage texture slot shows one mip level of a 2D image or of one layer"
            );
        }
        if opaque && slot_view.kind != ImageKind::D2 {
            bail!(
                InvalidArgument,
                "{image:?} is a {} image; an opaque slot shows a 2D image",
                slot_view.kind.name()
            );
        }
        let mut info = view_info(i, &slot_view);
        if opaque {
            // The alpha channel of the image means nothing (`xrgb`): read 1.
            info = info.components(vk::ComponentMapping::default().a(vk::ComponentSwizzle::ONE));
        }
        let image_info = vk::ImageDescriptorInfoEXT::default()
            .view(&info)
            .layout(layout);
        let info = vk::ResourceDescriptorInfoEXT::default()
            .ty(ty)
            .data(vk::ResourceDescriptorDataEXT::image(&image_info));
        let content = if storage {
            SlotContent::StorageImage { image }
        } else {
            SlotContent::SampledImage {
                image,
                view: slot_view,
            }
        };
        self.write_resource_slot(content, &info)
    }

    /// Writes a sampled-texture descriptor for `image` into a free heap slot.
    ///
    /// The slot shows every mip level. A cube image is shown as a cube, a 3D
    /// image as a volume; the shader's declaration has to match.
    pub fn create_texture_slot(&mut self, image: Handle) -> Result<Handle> {
        self.image_slot(image, false, false, &TextureView::default())
    }

    /// Like [`Gpu::create_texture_slot`], for a part of the image: a range of
    /// mip levels, or one layer of a cube image as a 2D texture.
    ///
    /// A slot that leaves out a mip level can be read while that level is an
    /// attachment of the open pass.
    pub fn create_texture_slot_view(
        &mut self,
        image: Handle,
        view: &TextureView,
    ) -> Result<Handle> {
        self.image_slot(image, false, false, view)
    }

    /// Like [`Gpu::create_texture_slot`], for an image whose alpha channel
    /// carries no information: the shader reads alpha as 1.
    pub fn create_opaque_texture_slot(&mut self, image: Handle) -> Result<Handle> {
        self.image_slot(image, false, true, &TextureView::default())
    }

    /// Writes sampled-texture descriptors for `images` into consecutive heap
    /// slots and returns one handle per slot.
    ///
    /// A shader's texture array of N elements is bound to the first of N
    /// consecutive slots; the shader then picks an element with an index that
    /// comes from its own data.
    pub fn create_texture_slot_run(&mut self, images: &[Handle]) -> Result<Vec<Handle>> {
        self.alive()?;
        if images.is_empty() {
            bail!(InvalidArgument, "a slot run needs at least one image");
        }
        let mut views = Vec::with_capacity(images.len());
        for image in images {
            let i = self.images.get(*image)?;
            if !i.usage.contains(ImageUsage::SAMPLED) {
                bail!(
                    InvalidArgument,
                    "{image:?} was not created with usage `sampled`"
                );
            }
            if i.kind != ImageKind::D2 {
                bail!(
                    InvalidArgument,
                    "{image:?} is a {} image; texture arrays hold 2D images only",
                    i.kind.name()
                );
            }
            views.push(resolve_view(i, *image, &TextureView::default())?);
        }
        let handles = self.slots.insert_run(
            images
                .iter()
                .zip(&views)
                .map(|(image, view)| SlotContent::SampledImage {
                    image: *image,
                    view: *view,
                })
                .collect(),
        )?;
        let ctx = self.ctx();
        let heap = &ctx.resource_heap;
        let mut failure = None;
        for ((handle, image), view) in handles.iter().zip(images).zip(&views) {
            let i = self.images.get(*image).expect("checked above");
            let view = view_info(i, view);
            let image_info = vk::ImageDescriptorInfoEXT::default()
                .view(&view)
                .layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            let info = vk::ResourceDescriptorInfoEXT::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .data(vk::ResourceDescriptorDataEXT::image(&image_info));
            // SAFETY: as in `write_resource_slot`; the indices are fresh.
            let result = unsafe {
                let dst = std::slice::from_raw_parts_mut(
                    heap.mapped
                        .add(handle.index() as usize * heap.stride as usize),
                    heap.stride as usize,
                );
                ctx.device.write_resource_descriptors_ext(
                    std::slice::from_ref(&info),
                    &[vk::HostAddressRangeEXT::default().address(dst)],
                )
            };
            if let Err(e) = result {
                failure = Some(e);
                break;
            }
        }
        if let Some(e) = failure {
            for handle in &handles {
                let _ = self.slots.remove(*handle);
                self.slots.release_index(handle.index());
            }
            return Err(self.fail("write descriptor", e));
        }
        Ok(handles)
    }

    /// Writes a storage-texture descriptor for mip level 0 of `image` into a
    /// free heap slot.
    pub fn create_storage_texture_slot(&mut self, image: Handle) -> Result<Handle> {
        self.image_slot(image, true, false, &TextureView::default())
    }

    /// Like [`Gpu::create_storage_texture_slot`], for another mip level or
    /// one layer of a cube image.
    pub fn create_storage_texture_slot_view(
        &mut self,
        image: Handle,
        view: &TextureView,
    ) -> Result<Handle> {
        self.image_slot(image, true, false, view)
    }
    fn buffer_slot(
        &mut self,
        buffer: Handle,
        offset: u64,
        size: u64,
        storage: bool,
    ) -> Result<Handle> {
        self.alive()?;
        let b = self.buffers.get(buffer)?;
        let (needed, what, ty) = if storage {
            (
                BufferUsage::STORAGE,
                "storage",
                vk::DescriptorType::STORAGE_BUFFER,
            )
        } else {
            (
                BufferUsage::UNIFORM,
                "uniform",
                vk::DescriptorType::UNIFORM_BUFFER,
            )
        };
        if !b.usage.contains(needed) {
            bail!(
                InvalidArgument,
                "{buffer:?} was not created with usage `{what}`"
            );
        }
        if size == 0 || offset.checked_add(size).is_none_or(|end| end > b.size) {
            bail!(
                OutOfRange,
                "range {offset}+{size} is outside {buffer:?} of {} bytes",
                b.size
            );
        }
        // 256 covers the offset alignment of every known implementation.
        if !offset.is_multiple_of(256) {
            bail!(
                InvalidArgument,
                "buffer slot offset must be a multiple of 256"
            );
        }
        let range = vk::DeviceAddressRangeKHR::default()
            .address(b.address + offset)
            .size(size);
        let info = vk::ResourceDescriptorInfoEXT::default()
            .ty(ty)
            .data(vk::ResourceDescriptorDataEXT::address_range(&range));
        let content = if storage {
            SlotContent::StorageBuffer { buffer, size }
        } else {
            SlotContent::UniformBuffer { buffer, size }
        };
        self.write_resource_slot(content, &info)
    }

    /// Writes a uniform-buffer descriptor for a range of `buffer`.
    pub fn create_uniform_slot(
        &mut self,
        buffer: Handle,
        offset: u64,
        size: u64,
    ) -> Result<Handle> {
        self.buffer_slot(buffer, offset, size, false)
    }

    /// Writes a storage-buffer descriptor for a range of `buffer`.
    pub fn create_storage_buffer_slot(
        &mut self,
        buffer: Handle,
        offset: u64,
        size: u64,
    ) -> Result<Handle> {
        self.buffer_slot(buffer, offset, size, true)
    }

    /// Writes a sampler descriptor into a free slot of the sampler heap.
    pub fn create_sampler(&mut self, desc: &SamplerDesc) -> Result<Handle> {
        self.create_sampler_with(desc, &SamplerExtra::default())
    }

    /// Like [`Gpu::create_sampler`], with control over mip levels, the level
    /// of detail, anisotropy, the third address mode and the border colour.
    pub fn create_sampler_with(
        &mut self,
        desc: &SamplerDesc,
        extra: &SamplerExtra,
    ) -> Result<Handle> {
        self.alive()?;
        let filter = |f| match f {
            Filter::Nearest => vk::Filter::NEAREST,
            Filter::Linear => vk::Filter::LINEAR,
        };
        let address = |a| match a {
            AddressMode::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
            AddressMode::Repeat => vk::SamplerAddressMode::REPEAT,
            AddressMode::MirrorRepeat => vk::SamplerAddressMode::MIRRORED_REPEAT,
            AddressMode::ClampToBorder => vk::SamplerAddressMode::CLAMP_TO_BORDER,
        };
        let limits = &self.ctx().limits;
        if !extra.lod_min.is_finite()
            || !extra.lod_max.is_finite()
            || extra.lod_min < 0.0
            || extra.lod_max < extra.lod_min
        {
            bail!(
                InvalidArgument,
                "the level-of-detail range {}..{} is not valid",
                extra.lod_min,
                extra.lod_max
            );
        }
        if !extra.lod_bias.is_finite() || extra.lod_bias.abs() > limits.max_sampler_lod_bias {
            bail!(
                InvalidArgument,
                "lod_bias {} is outside ±{}",
                extra.lod_bias,
                limits.max_sampler_lod_bias
            );
        }
        if !extra.anisotropy.is_finite() || extra.anisotropy < 1.0 {
            bail!(
                InvalidArgument,
                "anisotropy must be at least 1, got {}",
                extra.anisotropy
            );
        }
        let anisotropic = extra.anisotropy > 1.0;
        if anisotropic && !self.ctx().optional.sampler_anisotropy {
            bail!(
                Unsupported,
                "this device does not support anisotropic filtering"
            );
        }
        let info = vk::SamplerCreateInfo::default()
            .mag_filter(filter(desc.mag))
            .min_filter(filter(desc.min))
            .mipmap_mode(match extra.mip {
                Filter::Nearest => vk::SamplerMipmapMode::NEAREST,
                Filter::Linear => vk::SamplerMipmapMode::LINEAR,
            })
            .address_mode_u(address(desc.address_u))
            .address_mode_v(address(desc.address_v))
            .address_mode_w(address(extra.address_w))
            .mip_lod_bias(extra.lod_bias)
            .anisotropy_enable(anisotropic)
            .max_anisotropy(extra.anisotropy.min(limits.max_sampler_anisotropy).max(1.0))
            .border_color(match extra.border {
                BorderColor::TransparentBlack => vk::BorderColor::FLOAT_TRANSPARENT_BLACK,
                BorderColor::OpaqueBlack => vk::BorderColor::FLOAT_OPAQUE_BLACK,
                BorderColor::OpaqueWhite => vk::BorderColor::FLOAT_OPAQUE_WHITE,
            })
            .min_lod(extra.lod_min)
            .max_lod(extra.lod_max.min(vk::LOD_CLAMP_NONE));
        let info = match extra.compare {
            Some(op) => info.compare_enable(true).compare_op(compare_op(op)),
            None => info,
        };
        let border =
            [desc.address_u, desc.address_v, extra.address_w].contains(&AddressMode::ClampToBorder);
        let handle = self
            .samplers
            .insert(SamplerSlot {
                // Anisotropic filtering is a linear filter as far as the
                // format's capabilities are concerned.
                filtering: desc.mag == Filter::Linear
                    || desc.min == Filter::Linear
                    || extra.mip == Filter::Linear
                    || anisotropic,
                border,
                compare: extra.compare.is_some(),
            })
            .map_err(|(_, e)| e)?;
        let ctx = self.ctx();
        let heap = &ctx.sampler_heap;
        // SAFETY: as in `write_resource_slot`, for the sampler heap.
        let result = unsafe {
            let dst = std::slice::from_raw_parts_mut(
                heap.mapped
                    .add(handle.index() as usize * heap.stride as usize),
                heap.stride as usize,
            );
            ctx.device.write_sampler_descriptors_ext(
                std::slice::from_ref(&info),
                &[vk::HostAddressRangeEXT::default().address(dst)],
            )
        };
        if let Err(e) = result {
            let _ = self.samplers.remove(handle);
            self.samplers.release_index(handle.index());
            return Err(self.fail("write sampler descriptor", e));
        }
        Ok(handle)
    }

    // ----- pipelines --------------------------------------------------------

    pub fn create_render_pipeline(&mut self, desc: &RenderPipelineDesc) -> Result<Handle> {
        self.create_render_pipeline_with(desc, &PipelineExtra::default())
    }

    /// Like [`Gpu::create_render_pipeline`], with a depth attachment, a
    /// choice of the front face, or multisampling.
    ///
    /// The driver compiles the shaders here, which takes tens of
    /// milliseconds. [`Gpu::prepare_render_pipeline`] does that part on
    /// another thread.
    pub fn create_render_pipeline_with(
        &mut self,
        desc: &RenderPipelineDesc,
        extra: &PipelineExtra,
    ) -> Result<Handle> {
        let plan = self.plan_render_pipeline(desc, extra)?;
        let vs = self.shaders.get(desc.vertex)?;
        let fs = self.shaders.get(desc.fragment)?;
        // SAFETY: every part of the create info was validated by the plan
        // against the device limits, the format capabilities and the shader
        // interfaces.
        let pipeline = unsafe {
            build_render_pipeline(
                &self.ctx().device,
                HeapStrides::of(self.ctx()),
                desc,
                extra,
                vs.stage(),
                fs.stage(),
                &plan.common.bindings,
            )
        };
        match pipeline {
            Ok(pipeline) => self.insert_render_pipeline(plan, pipeline),
            Err(e) => Err(self.note_loss(e)),
        }
    }

    /// Checks a pipeline description now and leaves the slow part, the
    /// driver's compilation of the shaders, to a job that runs on another
    /// thread.
    ///
    /// Errors in the description come back here, at once. Run the job
    /// anywhere ([`PipelineJob::run`]); ask for the pipeline with
    /// [`Gpu::finish_render_pipeline`]. The job has its own copy of the
    /// shaders, so they may be destroyed meanwhile. A pending pipeline
    /// counts towards the limit on pipelines. It is not an object yet:
    /// [`Gpu::destroy_all`] leaves it alone.
    pub fn prepare_render_pipeline(
        &mut self,
        desc: &RenderPipelineDesc,
        extra: &PipelineExtra,
    ) -> Result<(PendingPipeline, PipelineJob)> {
        let plan = self.plan_render_pipeline(desc, extra)?;
        let vs = self.shaders.get(desc.vertex)?;
        let fs = self.shaders.get(desc.fragment)?;
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let job = PipelineJob::new(Inner {
            ticket,
            shared: std::sync::Arc::clone(&self.background),
            device: self.ctx().device.clone(),
            strides: HeapStrides::of(self.ctx()),
            desc: desc.clone(),
            extra: *extra,
            vertex: vs.stage().into(),
            fragment: fs.stage().into(),
            bindings: plan.common.bindings.clone(),
        });
        self.pending_pipelines.insert(ticket, plan);
        let pending = PendingPipeline {
            ticket,
            owner: std::sync::Arc::clone(&self.background),
        };
        Ok((pending, job))
    }

    /// [`Gpu::prepare_render_pipeline`], with the job run on a thread
    /// started for it.
    pub fn create_render_pipeline_in_background(
        &mut self,
        desc: &RenderPipelineDesc,
        extra: &PipelineExtra,
    ) -> Result<PendingPipeline> {
        let (pending, job) = self.prepare_render_pipeline(desc, extra)?;
        let spawned = std::thread::Builder::new()
            .name("pipeline-build".into())
            .spawn(move || job.run());
        if let Err(e) = spawned {
            // The job went with the closure; its result is an error.
            self.cancel_render_pipeline(pending);
            bail!(
                Internal,
                "cannot start the thread that builds the pipeline: {e}"
            );
        }
        Ok(pending)
    }

    /// The pipeline of a pending one: `None` while its job has not finished,
    /// then the handle (once: the pending pipeline is used up) or the error
    /// of the build. On a lost device it fails like every other creation
    /// and the pipeline stays pending (cancel it, or drop the `Gpu`).
    pub fn finish_render_pipeline(&mut self, pending: &PendingPipeline) -> Result<Option<Handle>> {
        if !std::sync::Arc::ptr_eq(&pending.owner, &self.background)
            || !self.pending_pipelines.contains_key(&pending.ticket)
        {
            bail!(
                InvalidHandle,
                "{pending:?} is not pending on this device (it was finished or cancelled, or belongs to another device)"
            );
        }
        self.alive()?;
        let Some(result) = self.background.take(pending.ticket) else {
            return Ok(None);
        };
        let plan = self
            .pending_pipelines
            .remove(&pending.ticket)
            .expect("checked above");
        match result {
            Ok(pipeline) => self.insert_render_pipeline(plan, pipeline).map(Some),
            Err(e) => Err(self.note_loss(e)),
        }
    }

    /// Gives up on a pending pipeline. Its job may still run to the end;
    /// what it builds is destroyed.
    pub fn cancel_render_pipeline(&mut self, pending: PendingPipeline) {
        if !std::sync::Arc::ptr_eq(&pending.owner, &self.background)
            || self.pending_pipelines.remove(&pending.ticket).is_none()
        {
            return;
        }
        if let Some(pipeline) = self.background.cancel(pending.ticket) {
            self.retire(Garbage::Pipeline(pipeline));
        }
    }

    /// How many pipelines are pending.
    pub fn pending_pipelines(&self) -> usize {
        self.pending_pipelines.len()
    }

    fn insert_render_pipeline(
        &mut self,
        mut plan: RenderPipeline,
        pipeline: vk::Pipeline,
    ) -> Result<Handle> {
        plan.common.vk = pipeline;
        match self.render_pipelines.insert(plan) {
            Ok(handle) => Ok(handle),
            Err((p, e)) => {
                self.retire(Garbage::Pipeline(p.common.vk));
                Err(e)
            }
        }
    }

    /// Checks a pipeline description against the device and the shaders, and
    /// returns everything of the pipeline but its Vulkan object.
    fn plan_render_pipeline(
        &self,
        desc: &RenderPipelineDesc,
        extra: &PipelineExtra,
    ) -> Result<RenderPipeline> {
        self.alive()?;
        let pipelines = self.render_pipelines.live() as usize + self.pending_pipelines.len();
        if pipelines >= self.render_pipelines.capacity() as usize {
            bail!(
                LimitExceeded,
                "too many pipelines (limit {})",
                self.render_pipelines.capacity()
            );
        }
        let vs = self.shaders.get(desc.vertex)?;
        let fs = self.shaders.get(desc.fragment)?;
        if vs.reflection.stage != ShaderStage::Vertex {
            bail!(
                Mismatch,
                "`vertex` is a {} shader",
                vs.reflection.stage.name()
            );
        }
        if fs.reflection.stage != ShaderStage::Fragment {
            bail!(
                Mismatch,
                "`fragment` is a {} shader",
                fs.reflection.stage.name()
            );
        }

        // Color targets against the device and the fragment outputs.
        let max_targets = MAX_COLOR_TARGETS.min(self.ctx().limits.max_color_attachments as usize);
        if desc.targets.len() > max_targets {
            bail!(
                InvalidArgument,
                "a pipeline has at most {max_targets} color targets, got {}",
                desc.targets.len()
            );
        }
        if desc.targets.is_empty() && extra.depth.is_none() {
            bail!(
                InvalidArgument,
                "a pipeline needs a color target or a depth attachment"
            );
        }
        if !self.ctx().optional.independent_blend
            && desc
                .targets
                .iter()
                .any(|t| t.blend != desc.targets[0].blend)
        {
            bail!(
                Unsupported,
                "this device needs the same blend state on every color target"
            );
        }
        if !matches!(extra.samples, 1 | 2 | 4 | 8) {
            bail!(
                InvalidArgument,
                "samples must be 1, 2, 4 or 8, got {}",
                extra.samples
            );
        }
        let limits = &self.ctx().limits;
        let mut sample_counts = u32::MAX;
        if !desc.targets.is_empty() {
            sample_counts &= limits.color_sample_counts;
        }
        if extra.depth.is_some() {
            sample_counts &= limits.depth_sample_counts;
        }
        if sample_counts & extra.samples == 0 {
            bail!(
                Unsupported,
                "this device does not render with {} samples",
                extra.samples
            );
        }
        if let Some(depth) = &extra.depth {
            if !depth.format.is_depth() || !self.format_caps(depth.format).depth_attachment {
                bail!(
                    Unsupported,
                    "{} cannot be a depth attachment on this device",
                    depth.format.name()
                );
            }
            if let Some(bias) = &depth.bias {
                if ![bias.constant, bias.slope, bias.clamp]
                    .iter()
                    .all(|v| v.is_finite())
                {
                    bail!(InvalidArgument, "the depth bias is not finite");
                }
                if bias.clamp != 0.0 && !self.ctx().optional.depth_bias_clamp {
                    bail!(
                        Unsupported,
                        "this device does not support a depth bias clamp"
                    );
                }
            }
        }
        for (i, target) in desc.targets.iter().enumerate() {
            let caps = self.format_caps(target.format);
            if !caps.color_attachment {
                bail!(
                    Unsupported,
                    "target {i}: {} cannot be rendered to on this device",
                    target.format.name()
                );
            }
            if target.blend.is_some() && !caps.blend {
                bail!(
                    Unsupported,
                    "target {i}: {} does not support blending on this device",
                    target.format.name()
                );
            }
            let Some(output) = fs
                .reflection
                .outputs
                .iter()
                .find(|o| o.location == i as u32)
            else {
                bail!(
                    ShaderInterface,
                    "target {i}: the fragment shader has no output at location {i}"
                );
            };
            if output.class != target.format.class() {
                bail!(
                    ShaderInterface,
                    "target {i}: the fragment output is {:?} but {} is {:?}",
                    output.class,
                    target.format.name(),
                    target.format.class()
                );
            }
        }

        // An output that no target receives is a mistake more often than an
        // intention, and the validation layer warns about it.
        if let Some(output) = fs
            .reflection
            .outputs
            .iter()
            .find(|o| o.location as usize >= desc.targets.len())
        {
            bail!(
                ShaderInterface,
                "the fragment shader writes location {} but the pipeline has {} color target(s)",
                output.location,
                desc.targets.len()
            );
        }

        // Vertex buffers against the device limits and the vertex inputs.
        if desc.vertex_buffers.len() > MAX_VERTEX_BUFFERS {
            bail!(
                InvalidArgument,
                "at most {MAX_VERTEX_BUFFERS} vertex buffers are supported"
            );
        }
        let mut attributes: Vec<(u32, NumClass, usize)> = Vec::new();
        let mut requirements = Vec::new();
        for (index, layout) in desc.vertex_buffers.iter().enumerate() {
            if layout.stride == 0 || layout.stride > 2048 || layout.stride % 4 != 0 {
                bail!(
                    InvalidArgument,
                    "vertex buffer {index}: stride {} must be a multiple of 4 in 4..=2048",
                    layout.stride
                );
            }
            let mut element_size = 0;
            for attribute in &layout.attributes {
                let end = attribute.offset.checked_add(attribute.format.size());
                if end.is_none_or(|end| end > layout.stride) || attribute.offset % 4 != 0 {
                    bail!(
                        InvalidArgument,
                        "vertex buffer {index}: attribute at location {} (offset {}, {}) does not fit the stride {}",
                        attribute.location,
                        attribute.offset,
                        attribute.format.name(),
                        layout.stride
                    );
                }
                if attribute.location >= 32
                    || attributes.iter().any(|(l, _, _)| *l == attribute.location)
                {
                    bail!(
                        InvalidArgument,
                        "vertex attribute location {} is out of range or used twice",
                        attribute.location
                    );
                }
                attributes.push((attribute.location, attribute.format.class(), index));
                element_size = element_size.max(u64::from(end.expect("checked")));
            }
            requirements.push(VertexBufferRequirement {
                stride: u64::from(layout.stride),
                per_instance: layout.per_instance,
                element_size,
            });
        }
        for input in &vs.reflection.inputs {
            let Some((_, class, _)) = attributes.iter().find(|(l, _, _)| *l == input.location)
            else {
                bail!(
                    ShaderInterface,
                    "the vertex shader reads location {} but no vertex attribute provides it",
                    input.location
                );
            };
            if *class != input.class {
                bail!(
                    ShaderInterface,
                    "vertex attribute at location {} is {class:?} but the shader expects {:?}",
                    input.location,
                    input.class
                );
            }
        }

        // Vertex outputs against fragment inputs.
        for input in &fs.reflection.inputs {
            let Some(output) = vs
                .reflection
                .outputs
                .iter()
                .find(|o| o.location == input.location)
            else {
                bail!(
                    ShaderInterface,
                    "the fragment shader reads location {} but the vertex shader does not write it",
                    input.location
                );
            };
            if output.class != input.class || output.components < input.components {
                bail!(
                    ShaderInterface,
                    "location {}: the vertex shader writes {:?}x{} but the fragment shader reads {:?}x{}",
                    input.location,
                    output.class,
                    output.components,
                    input.class,
                    input.components
                );
            }
        }

        let (bindings, push_user_size, push_total_size, sampling_pairs) =
            self.merge_interfaces(&[&vs.reflection, &fs.reflection])?;
        let comparison_pairs = merge_comparisons(&[&vs.reflection, &fs.reflection], &bindings)?;
        for b in &bindings {
            let writable = match b.kind {
                BindingKind::StorageTexture { .. } => true,
                BindingKind::StorageBuffer { writable, .. } => writable,
                _ => false,
            };
            if writable {
                bail!(
                    Unsupported,
                    "binding {} (`{}`): storage textures and writable storage buffers are only supported in compute pipelines",
                    b.binding,
                    b.name
                );
            }
        }

        Ok(RenderPipeline {
            common: PipelineCommon {
                vk: vk::Pipeline::null(),
                bindings,
                push_user_size,
                push_total_size,
                sampling_pairs,
                comparison_pairs,
            },
            color_formats: desc.targets.iter().map(|t| t.format).collect(),
            depth_format: extra.depth.map(|d| d.format),
            samples: extra.samples,
            vertex_buffers: requirements,
        })
    }

    pub fn create_compute_pipeline(&mut self, shader: Handle) -> Result<Handle> {
        self.alive()?;
        if self.compute_pipelines.live() >= self.compute_pipelines.capacity() {
            bail!(
                LimitExceeded,
                "too many pipelines (limit {})",
                self.compute_pipelines.capacity()
            );
        }
        let cs = self.shaders.get(shader)?;
        if cs.reflection.stage != ShaderStage::Compute {
            bail!(
                Mismatch,
                "expected a compute shader, got a {} shader",
                cs.reflection.stage.name()
            );
        }
        let (bindings, push_user_size, push_total_size, sampling_pairs) =
            self.merge_interfaces(&[&cs.reflection])?;
        let comparison_pairs = merge_comparisons(&[&cs.reflection], &bindings)?;
        for b in &bindings {
            if let BindingKind::StorageTexture { format, .. } = b.kind
                && !self.format_caps(format).storage
            {
                bail!(
                    Unsupported,
                    "binding {} (`{}`): {} cannot be a storage texture on this device",
                    b.binding,
                    b.name,
                    format.name()
                );
            }
        }
        // SAFETY: the shader passed the SPIR-V checks and its interface is
        // fully mapped to the descriptor heap by `stage_mappings`.
        let pipeline = unsafe {
            let ctx = self.ctx();
            let module = ctx
                .device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&cs.spirv), None)
                .map_err(|e| vk_error("create shader module", e));
            match module {
                Err(e) => Err(e),
                Ok(module) => {
                    let mappings = stage_mappings(HeapStrides::of(ctx), &cs.reflection, &bindings);
                    let mut mapping_info =
                        vk::ShaderDescriptorSetAndBindingMappingInfoEXT::default()
                            .mappings(&mappings);
                    let stage = vk::PipelineShaderStageCreateInfo::default()
                        .stage(vk::ShaderStageFlagBits::COMPUTE)
                        .module(module)
                        .name(&cs.entry)
                        .push_next(&mut mapping_info);
                    let mut flags = vk::PipelineCreateFlags2CreateInfo::default()
                        .flags(vk::PipelineCreateFlags2::DESCRIPTOR_HEAP_EXT);
                    let info = vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .push_next(&mut flags);
                    let result = ctx.device.create_compute_pipelines(
                        vk::PipelineCache::null(),
                        &[info],
                        None,
                    );
                    ctx.device.destroy_shader_module(module, None);
                    take_pipeline(&ctx.device, result)
                }
            }
        };
        let pipeline = match pipeline {
            Ok(p) => p,
            Err(e) => return Err(self.note_loss(e)),
        };
        match self.compute_pipelines.insert(ComputePipeline {
            common: PipelineCommon {
                vk: pipeline,
                bindings,
                push_user_size,
                push_total_size,
                sampling_pairs,
                comparison_pairs,
            },
        }) {
            Ok(handle) => Ok(handle),
            Err((p, e)) => {
                self.retire(Garbage::Pipeline(p.common.vk));
                Err(e)
            }
        }
    }

    /// Merges the resource interfaces of the stages of one pipeline and lays
    /// out push data: the shader's block first, then one `u32` heap index per
    /// binding.
    #[allow(clippy::type_complexity)]
    fn merge_interfaces(
        &self,
        stages: &[&Reflection],
    ) -> Result<(Vec<PipelineBinding>, u32, u32, Vec<(u32, u32)>)> {
        let mut bindings: Vec<PipelineBinding> = Vec::new();
        let mut push_user_size = 0;
        let mut sampling_pairs = Vec::new();
        for reflection in stages {
            push_user_size = push_user_size.max(reflection.push_size);
            for pair in &reflection.sampling_pairs {
                if !sampling_pairs.contains(pair) {
                    sampling_pairs.push(*pair);
                }
            }
            for b in &reflection.bindings {
                match bindings.iter_mut().find(|m| m.binding == b.binding) {
                    None => bindings.push(PipelineBinding {
                        binding: b.binding,
                        name: b.name.clone(),
                        kind: b.kind,
                        push_offset: 0,
                        used: b.used,
                    }),
                    Some(merged) => {
                        merged.kind = match (merged.kind, b.kind) {
                            (a, c) if a == c => a,
                            (
                                BindingKind::UniformBuffer { min_size: a },
                                BindingKind::UniformBuffer { min_size: c },
                            ) => BindingKind::UniformBuffer { min_size: a.max(c) },
                            (a, c) => bail!(
                                ShaderInterface,
                                "binding {} is a {} in one stage and a {} in another",
                                b.binding,
                                a.describe(),
                                c.describe()
                            ),
                        };
                        merged.used |= b.used;
                    }
                }
            }
        }
        bindings.sort_by_key(|b| b.binding);
        debug_assert!(bindings.iter().all(|b| b.binding < MAX_BINDINGS));
        let push_user_size = push_user_size.next_multiple_of(4);
        for (i, b) in bindings.iter_mut().enumerate() {
            b.push_offset = push_user_size + 4 * i as u32;
        }
        let push_total_size = push_user_size + 4 * bindings.len() as u32;
        let max = self.ctx().limits.max_push_data_size;
        if push_total_size > max {
            bail!(
                LimitExceeded,
                "push data needs {push_total_size} bytes ({push_user_size} for the shader's block, 4 per binding); the device allows {max}"
            );
        }
        let capacity = crate::record::PUSH_CAPACITY;
        if push_user_size as usize > capacity {
            bail!(
                LimitExceeded,
                "the shader's push block is {push_user_size} bytes; the limit is {capacity}"
            );
        }
        Ok((bindings, push_user_size, push_total_size, sampling_pairs))
    }

    // ----- destruction ------------------------------------------------------

    /// Destroys any object. The handle is invalid from this call on; the
    /// Vulkan object is released once the GPU has finished using it.
    pub fn destroy(&mut self, handle: Handle) -> Result<()> {
        // Destroying stays possible on a lost device so that scripts can clean up.
        let Some(kind) = handle.kind() else {
            bail!(InvalidHandle, "{handle:?} is not a handle");
        };
        match kind {
            Kind::Buffer => {
                let b = self.buffers.remove(handle)?;
                self.buffers.release_index(handle.index());
                self.retire(Garbage::Buffer {
                    vk: b.vk,
                    allocation: b.allocation,
                    bytes: b.size,
                });
            }
            Kind::Image => {
                let i = self.images.remove(handle)?;
                self.images.release_index(handle.index());
                self.retire(Garbage::Image {
                    vk: i.vk,
                    views: i.views,
                    memory: i.memory,
                    bytes: i.bytes,
                });
            }
            Kind::Shader => {
                self.shaders.remove(handle)?;
                self.shaders.release_index(handle.index());
            }
            Kind::RenderPipeline => {
                let p = self.render_pipelines.remove(handle)?;
                self.render_pipelines.release_index(handle.index());
                self.retire(Garbage::Pipeline(p.common.vk));
            }
            Kind::ComputePipeline => {
                let p = self.compute_pipelines.remove(handle)?;
                self.compute_pipelines.release_index(handle.index());
                self.retire(Garbage::Pipeline(p.common.vk));
            }
            Kind::Slot => {
                self.slots.remove(handle)?;
                self.retire(Garbage::ResourceSlot(handle.index()));
            }
            Kind::Sampler => {
                self.samplers.remove(handle)?;
                self.retire(Garbage::SamplerSlot(handle.index()));
            }
        }
        self.protected.remove(&handle);
        self.mutation += 1;
        Ok(())
    }

    /// Destroys every object scripts created, keeping the device and the
    /// objects of the host ([`Gpu::protect`]). Used when a script is reloaded
    /// or replaced.
    pub fn destroy_all(&mut self) {
        let mut handles = self.slots.live_handles();
        handles.extend(self.samplers.live_handles());
        handles.extend(self.render_pipelines.live_handles());
        handles.extend(self.compute_pipelines.live_handles());
        handles.extend(self.shaders.live_handles());
        handles.extend(self.buffers.live_handles());
        handles.extend(self.images.live_handles());
        for handle in handles {
            if !self.protected.contains(&handle) {
                let _ = self.destroy(handle);
            }
        }
        let images = &self.images;
        self.pending_init.retain(|h| images.get(*h).is_ok());
        self.mutation += 1;
    }

    // ----- host-side readback -----------------------------------------------

    /// Reads back the pixels of an image, tightly packed, row by row.
    ///
    /// Host-side helper for tests and screenshots. Not callable while a frame
    /// is being recorded. Blocks until the GPU is idle.
    pub fn read_image(&mut self, image: Handle) -> Result<Vec<u8>> {
        self.read_image_level(image, Subresource::default())
    }

    /// Like [`Gpu::read_image`], for one mip level of one layer.
    pub fn read_image_level(&mut self, image: Handle, sub: Subresource) -> Result<Vec<u8>> {
        self.alive()?;
        if self.recording {
            bail!(
                InvalidState,
                "read_image is not allowed while a frame is being recorded"
            );
        }
        self.wait_idle()?;
        let (vk_image, layout, rest, extent, bytes, full, layers) = {
            let i = self.images.get(image)?;
            if !i.usage.contains(ImageUsage::COPY_SRC) {
                bail!(
                    InvalidArgument,
                    "{image:?} was not created with usage `copy_src`"
                );
            }
            i.check_sub(sub, image)?;
            (
                i.vk,
                i.layout,
                i.rest,
                i.level_extent(sub.mip),
                i.level_bytes(sub.mip),
                i.full_range(),
                i.sub_layers(sub),
            )
        };
        let timeout = self.config.gpu_timeout.as_nanos().min(u128::from(u64::MAX)) as u64;
        let (util_pool, util_cb, util_fence) = (self.util_pool, self.util_cb, self.util_fence);
        // SAFETY: the GPU is idle, so the utility command buffer and fence are
        // free. The image is alive and its tracked layout is accurate. The
        // temporary buffer is destroyed only after the fence has signalled.
        let result: Result<Vec<u8>> = unsafe {
            let ctx = self.ctx.as_mut().expect("device lives until drop");
            let device = ctx.device.clone();
            let buffer = device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(|e| vk_error("create readback buffer", e))?;
            let requirements = device.get_buffer_memory_requirements(buffer);
            let allocation = match ctx.allocator().allocate(AllocationCreateDesc {
                name: Some("readback"),
                requirements,
                location: MemoryLocation::GpuToCpu,
                linear: true,
                memory_allocate_flags: vk::MemoryAllocateFlags::empty(),
                export_handle_types: vk::ExternalMemoryHandleTypeFlags::empty(),
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            }) {
                Ok(a) => a,
                Err(e) => {
                    device.destroy_buffer(buffer, None);
                    bail!(OutOfMemory, "allocate readback buffer: {e}");
                }
            };
            let run = (|| -> std::result::Result<Vec<u8>, GfxError> {
                device
                    .bind_buffer_memory(buffer, allocation.memory(), allocation.offset())
                    .map_err(|e| vk_error("bind readback buffer", e))?;
                device
                    .reset_command_pool(util_pool, vk::CommandPoolResetFlags::empty())
                    .map_err(|e| vk_error("reset command pool", e))?;
                device
                    .begin_command_buffer(
                        util_cb,
                        &vk::CommandBufferBeginInfo::default()
                            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                    )
                    .map_err(|e| vk_error("begin command buffer", e))?;
                image_barrier(
                    &device,
                    util_cb,
                    vk_image,
                    full,
                    layout,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                );
                let regions = [vk::BufferImageCopy2::default()
                    .image_subresource(layers)
                    .image_extent(vk::Extent3D {
                        width: extent.0,
                        height: extent.1,
                        depth: extent.2,
                    })];
                device.cmd_copy_image_to_buffer2(
                    util_cb,
                    &vk::CopyImageToBufferInfo2::default()
                        .src_image(vk_image)
                        .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                        .dst_buffer(buffer)
                        .regions(&regions),
                );
                image_barrier(
                    &device,
                    util_cb,
                    vk_image,
                    full,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    rest,
                );
                device
                    .end_command_buffer(util_cb)
                    .map_err(|e| vk_error("end command buffer", e))?;
                device
                    .reset_fences(&[util_fence])
                    .map_err(|e| vk_error("reset fence", e))?;
                let cbs = [util_cb];
                device
                    .queue_submit(
                        ctx.queue,
                        &[vk::SubmitInfo::default().command_buffers(&cbs)],
                        util_fence,
                    )
                    .map_err(|e| vk_error("submit readback", e))?;
                match device.wait_for_fences(&[util_fence], true, timeout) {
                    Ok(vk::VkResult::SUCCESS) => {}
                    Ok(_) => {
                        return Err(GfxError::new(
                            ErrorCode::Timeout,
                            "the GPU did not finish a readback in time",
                        ));
                    }
                    Err(e) => return Err(vk_error("wait for readback", e)),
                }
                let mut mapped = ctx.allocator().map(&allocation).map_err(|e| {
                    GfxError::new(ErrorCode::Internal, format!("map readback buffer: {e}"))
                })?;
                let _ = mapped.invalidate();
                Ok(mapped.byte_range(0, bytes as usize).to_vec())
            })();
            if run.as_ref().is_err_and(|e| e.code == ErrorCode::Timeout) {
                // The GPU may still be using the buffer: leave it alone.
                let _ = allocation;
            } else {
                device.destroy_buffer(buffer, None);
                let _ = ctx.allocator().free(allocation);
            }
            run
        };
        match result {
            Ok(pixels) => {
                self.images.get_mut(image)?.layout = rest;
                self.pending_init.retain(|h| *h != image);
                self.stats.barriers += 2;
                Ok(pixels)
            }
            Err(e) => {
                if e.code == ErrorCode::Timeout {
                    self.lost = Some((ErrorCode::Timeout, e.message.clone()));
                    self.hung = true;
                }
                Err(self.note_loss(e))
            }
        }
    }
}

/// Creates the Vulkan pipeline of a plan. Needs the device and nothing else
/// of the `Gpu`, so it can run on any thread.
///
/// # Safety
///
/// `desc`, `extra`, `vs`, `fs` and `bindings` must have passed the checks of
/// `Gpu::plan_render_pipeline` on the device, and the device must stay alive
/// for the call.
pub(crate) unsafe fn build_render_pipeline(
    device: &vulkan::Device,
    strides: HeapStrides,
    desc: &RenderPipelineDesc,
    extra: &PipelineExtra,
    vs: StageRef<'_>,
    fs: StageRef<'_>,
    bindings: &[PipelineBinding],
) -> Result<vk::Pipeline> {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let vs_module = device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(vs.spirv), None)
            .map_err(|e| vk_error("create shader module", e))?;
        let fs_module = match device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(fs.spirv), None)
        {
            Ok(m) => m,
            Err(e) => {
                device.destroy_shader_module(vs_module, None);
                return Err(vk_error("create shader module", e));
            }
        };
        let vs_mappings = stage_mappings(strides, vs.reflection, bindings);
        let fs_mappings = stage_mappings(strides, fs.reflection, bindings);
        let mut vs_mapping_info =
            vk::ShaderDescriptorSetAndBindingMappingInfoEXT::default().mappings(&vs_mappings);
        let mut fs_mapping_info =
            vk::ShaderDescriptorSetAndBindingMappingInfoEXT::default().mappings(&fs_mappings);
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlagBits::VERTEX)
                .module(vs_module)
                .name(vs.entry)
                .push_next(&mut vs_mapping_info),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlagBits::FRAGMENT)
                .module(fs_module)
                .name(fs.entry)
                .push_next(&mut fs_mapping_info),
        ];

        let vertex_bindings: Vec<_> = desc
            .vertex_buffers
            .iter()
            .enumerate()
            .map(|(i, layout)| {
                vk::VertexInputBindingDescription::default()
                    .binding(i as u32)
                    .stride(layout.stride)
                    .input_rate(if layout.per_instance {
                        vk::VertexInputRate::INSTANCE
                    } else {
                        vk::VertexInputRate::VERTEX
                    })
            })
            .collect();
        let vertex_attributes: Vec<_> = desc
            .vertex_buffers
            .iter()
            .enumerate()
            .flat_map(|(i, layout)| {
                layout.attributes.iter().map(move |a| {
                    vk::VertexInputAttributeDescription::default()
                        .location(a.location)
                        .binding(i as u32)
                        .format(a.format.vk())
                        .offset(a.offset)
                })
            })
            .collect();
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_bindings)
            .vertex_attribute_descriptions(&vertex_attributes);
        let input_assembly =
            vk::PipelineInputAssemblyStateCreateInfo::default().topology(match desc.topology {
                Topology::TriangleList => vk::PrimitiveTopology::TRIANGLE_LIST,
                Topology::TriangleStrip => vk::PrimitiveTopology::TRIANGLE_STRIP,
            });
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(match desc.cull {
                CullMode::None => vk::CullModeFlags::NONE,
                CullMode::Front => vk::CullModeFlags::FRONT,
                CullMode::Back => vk::CullModeFlags::BACK,
            })
            .front_face(match extra.front_face {
                FrontFace::CounterClockwise => vk::FrontFace::COUNTER_CLOCKWISE,
                FrontFace::Clockwise => vk::FrontFace::CLOCKWISE,
            })
            .line_width(1.0);
        let bias = extra.depth.and_then(|d| d.bias);
        let rasterization = match bias {
            Some(bias) => rasterization
                .depth_bias_enable(true)
                .depth_bias_constant_factor(bias.constant)
                .depth_bias_slope_factor(bias.slope)
                .depth_bias_clamp(bias.clamp),
            None => rasterization,
        };
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlagBits::from_raw(extra.samples));
        let depth_stencil = match &extra.depth {
            Some(depth) => vk::PipelineDepthStencilStateCreateInfo::default()
                .depth_test_enable(depth.test)
                .depth_write_enable(depth.write)
                .depth_compare_op(compare_op(depth.compare))
                .min_depth_bounds(0.0)
                .max_depth_bounds(1.0),
            None => vk::PipelineDepthStencilStateCreateInfo::default(),
        };
        let blend_attachments: Vec<_> = desc
            .targets
            .iter()
            .map(|t| {
                let state = vk::PipelineColorBlendAttachmentState::default().color_write_mask(
                    vk::ColorComponentFlagBits::R
                        | vk::ColorComponentFlagBits::G
                        | vk::ColorComponentFlagBits::B
                        | vk::ColorComponentFlagBits::A,
                );
                match t.blend {
                    None => state,
                    Some(b) => state
                        .blend_enable(true)
                        .src_color_blend_factor(blend_factor(b.src_color))
                        .dst_color_blend_factor(blend_factor(b.dst_color))
                        .color_blend_op(blend_op(b.color_op))
                        .src_alpha_blend_factor(blend_factor(b.src_alpha))
                        .dst_alpha_blend_factor(blend_factor(b.dst_alpha))
                        .alpha_blend_op(blend_op(b.alpha_op)),
                }
            })
            .collect();
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats: Vec<_> = desc.targets.iter().map(|t| t.format.vk()).collect();
        let mut rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(extra.depth.map_or(vk::Format::UNDEFINED, |d| d.format.vk()));
        let mut flags = vk::PipelineCreateFlags2CreateInfo::default()
            .flags(vk::PipelineCreateFlags2::DESCRIPTOR_HEAP_EXT);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic)
            .push_next(&mut flags)
            .push_next(&mut rendering);
        let result = device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None);
        device.destroy_shader_module(fs_module, None);
        device.destroy_shader_module(vs_module, None);
        take_pipeline(device, result)
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // No job may be inside the driver when the device goes away. A device
        // that hangs is not waited for: it is leaked below, so a job that
        // finishes later still has a device.
        let unclaimed = self.background.close(!self.hung);
        let Some(ctx) = self.ctx.take() else {
            return;
        };
        if self.hung {
            // The GPU never returned. Destroying objects it may still use is
            // undefined, and the driver may block. Leak everything; only a new
            // process gets this memory back.
            ctx.leak();
            return;
        }
        self.ctx = Some(ctx);
        // SAFETY: after `device_wait_idle` nothing is executing, so every
        // object created from the device can be destroyed.
        unsafe {
            let _ = self.ctx().device.device_wait_idle();
            self.completed_serial = u64::MAX;
            self.recording = false;
            self.collect_garbage();
            // Pipelines that were built in the background and never taken.
            for pipeline in unclaimed {
                self.ctx().device.destroy_pipeline(pipeline, None);
            }
            for handle_kind in 0..7 {
                let garbage: Vec<Garbage> = match handle_kind {
                    0 => self
                        .buffers
                        .drain()
                        .into_iter()
                        .map(|b| Garbage::Buffer {
                            vk: b.vk,
                            allocation: b.allocation,
                            bytes: b.size,
                        })
                        .collect(),
                    1 => self
                        .images
                        .drain()
                        .into_iter()
                        .map(|i| Garbage::Image {
                            vk: i.vk,
                            views: i.views,
                            memory: i.memory,
                            bytes: i.bytes,
                        })
                        .collect(),
                    2 => self
                        .render_pipelines
                        .drain()
                        .into_iter()
                        .map(|p| Garbage::Pipeline(p.common.vk))
                        .collect(),
                    3 => self
                        .compute_pipelines
                        .drain()
                        .into_iter()
                        .map(|p| Garbage::Pipeline(p.common.vk))
                        .collect(),
                    _ => Vec::new(),
                };
                for g in garbage {
                    self.destroy_garbage(g);
                }
            }
            let ctx = self.ctx.take().expect("set above");
            for frame in &self.frames {
                ctx.device.destroy_fence(frame.fence, None);
                ctx.device.destroy_command_pool(frame.pool, None);
            }
            ctx.device.destroy_fence(self.util_fence, None);
            ctx.device.destroy_command_pool(self.util_pool, None);
            ctx.destroy();
        }
    }
}

pub(crate) const COLOR_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

/// The texture and sampler pairs the stages of a pipeline use for depth
/// comparisons. A pair has to be used the same way in every stage, and the
/// texture cannot be an array of textures.
fn merge_comparisons(
    stages: &[&Reflection],
    bindings: &[PipelineBinding],
) -> Result<Vec<(u32, u32)>> {
    let mut compared: Vec<(u32, u32)> = Vec::new();
    for reflection in stages {
        for pair in &reflection.comparison_pairs {
            if !compared.contains(pair) {
                compared.push(*pair);
            }
        }
    }
    for reflection in stages {
        for pair in &reflection.sampling_pairs {
            if compared.contains(pair) && !reflection.comparison_pairs.contains(pair) {
                bail!(
                    ShaderInterface,
                    "the texture at binding {} and the sampler at binding {} are used for comparisons in one stage and for plain reads in another",
                    pair.0,
                    pair.1
                );
            }
        }
    }
    for (texture, _) in &compared {
        let array = bindings
            .iter()
            .any(|b| b.binding == *texture && matches!(b.kind, BindingKind::TextureArray { .. }));
        if array {
            bail!(
                Unsupported,
                "binding {texture}: comparisons on an array of textures are not supported"
            );
        }
    }
    Ok(compared)
}

/// Image usages with what the format has to support for each, the Vulkan
/// flag, and the name scripts use.
fn usage_table(caps: &FormatCaps) -> [(ImageUsage, bool, vk::ImageUsageFlags, &'static str); 6] {
    [
        (
            ImageUsage::SAMPLED,
            caps.sampled,
            vk::ImageUsageFlags::SAMPLED,
            "sampled",
        ),
        (
            ImageUsage::COLOR_TARGET,
            caps.color_attachment,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
            "color_target",
        ),
        (
            ImageUsage::DEPTH_TARGET,
            caps.depth_attachment,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
            "depth_target",
        ),
        (
            ImageUsage::STORAGE,
            caps.storage,
            vk::ImageUsageFlags::STORAGE,
            "storage",
        ),
        (
            ImageUsage::COPY_SRC,
            caps.transfer_src,
            vk::ImageUsageFlags::TRANSFER_SRC,
            "copy_src",
        ),
        (
            ImageUsage::COPY_DST,
            caps.transfer_dst,
            vk::ImageUsageFlags::TRANSFER_DST,
            "copy_dst",
        ),
    ]
}

/// Checks a view description against the image and fills in its defaults.
fn resolve_view(i: &Image, image: Handle, view: &TextureView) -> Result<SlotView> {
    let mip_count = view
        .mip_count
        .unwrap_or_else(|| i.mip_levels.saturating_sub(view.base_mip));
    if mip_count == 0
        || view
            .base_mip
            .checked_add(mip_count)
            .is_none_or(|end| end > i.mip_levels)
    {
        bail!(
            OutOfRange,
            "mip levels {}+{mip_count} are outside {image:?}, which has {}",
            view.base_mip,
            i.mip_levels
        );
    }
    let (kind, base_layer, layer_count) = match view.layer {
        None => (i.kind, 0, i.layers),
        Some(_) if i.kind == ImageKind::D3 => {
            bail!(InvalidArgument, "{image:?} is a 3D image; it has no layers")
        }
        Some(layer) if layer >= i.layers => bail!(
            OutOfRange,
            "layer {layer} is outside {image:?}, which has {}",
            i.layers
        ),
        Some(layer) => (ImageKind::D2, layer, 1),
    };
    Ok(SlotView {
        kind,
        base_mip: view.base_mip,
        mip_count,
        base_layer,
        layer_count,
    })
}

/// The view description of a slot, for the descriptor.
fn view_info<'a>(i: &Image, view: &SlotView) -> vk::ImageViewCreateInfo<'a> {
    vk::ImageViewCreateInfo::default()
        .image(i.vk)
        .view_type(match view.kind {
            ImageKind::D2 => vk::ImageViewType::_2D,
            ImageKind::D3 => vk::ImageViewType::_3D,
            ImageKind::Cube => vk::ImageViewType::CUBE,
        })
        .format(i.format.vk())
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: i.format.aspect(),
            base_mip_level: view.base_mip,
            level_count: view.mip_count,
            base_array_layer: view.base_layer,
            layer_count: view.layer_count,
        })
}

pub(crate) fn compare_op(op: CompareOp) -> vk::CompareOp {
    match op {
        CompareOp::Never => vk::CompareOp::NEVER,
        CompareOp::Less => vk::CompareOp::LESS,
        CompareOp::Equal => vk::CompareOp::EQUAL,
        CompareOp::LessOrEqual => vk::CompareOp::LESS_OR_EQUAL,
        CompareOp::Greater => vk::CompareOp::GREATER,
        CompareOp::NotEqual => vk::CompareOp::NOT_EQUAL,
        CompareOp::GreaterOrEqual => vk::CompareOp::GREATER_OR_EQUAL,
        CompareOp::Always => vk::CompareOp::ALWAYS,
    }
}

fn layout_sync(layout: vk::ImageLayout) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    use vk::{AccessFlags2 as A, ImageLayout as L, PipelineStageFlags2 as S};
    match layout {
        L::UNDEFINED => (S::NONE, A::empty()),
        L::SHADER_READ_ONLY_OPTIMAL => (
            S::VERTEX_SHADER | S::FRAGMENT_SHADER | S::COMPUTE_SHADER,
            A::SHADER_SAMPLED_READ,
        ),
        L::COLOR_ATTACHMENT_OPTIMAL => (
            S::COLOR_ATTACHMENT_OUTPUT,
            A::COLOR_ATTACHMENT_READ | A::COLOR_ATTACHMENT_WRITE,
        ),
        L::DEPTH_STENCIL_ATTACHMENT_OPTIMAL => (
            S::EARLY_FRAGMENT_TESTS | S::LATE_FRAGMENT_TESTS,
            A::DEPTH_STENCIL_ATTACHMENT_READ | A::DEPTH_STENCIL_ATTACHMENT_WRITE,
        ),
        // Copies and scaled copies (`generate_mipmaps`).
        L::TRANSFER_SRC_OPTIMAL => (S::COPY | S::BLIT, A::TRANSFER_READ),
        L::TRANSFER_DST_OPTIMAL => (S::COPY | S::BLIT, A::TRANSFER_WRITE),
        _ => (S::ALL_COMMANDS, A::MEMORY_READ | A::MEMORY_WRITE),
    }
}

/// Records a layout transition with the matching synchronization scopes.
///
/// # Safety
///
/// `cb` must be recording outside a render pass, `image` must be alive, and
/// the subresources in `range` must exist and be in layout `old` at this
/// point of the command buffer.
pub(crate) unsafe fn image_barrier(
    device: &vulkan::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    range: vk::ImageSubresourceRange,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
) {
    let (src_stage, src_access) = layout_sync(old);
    let (dst_stage, dst_access) = layout_sync(new);
    let barriers = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .old_layout(old)
        .new_layout(new)
        .image(image)
        .subresource_range(range)];
    // SAFETY: guaranteed by the caller.
    unsafe {
        device.cmd_pipeline_barrier2(
            cb,
            &vk::DependencyInfo::default().image_memory_barriers(&barriers),
        );
    }
}

fn blend_factor(factor: BlendFactor) -> vk::BlendFactor {
    match factor {
        BlendFactor::Zero => vk::BlendFactor::ZERO,
        BlendFactor::One => vk::BlendFactor::ONE,
        BlendFactor::SrcColor => vk::BlendFactor::SRC_COLOR,
        BlendFactor::OneMinusSrcColor => vk::BlendFactor::ONE_MINUS_SRC_COLOR,
        BlendFactor::DstColor => vk::BlendFactor::DST_COLOR,
        BlendFactor::OneMinusDstColor => vk::BlendFactor::ONE_MINUS_DST_COLOR,
        BlendFactor::SrcAlpha => vk::BlendFactor::SRC_ALPHA,
        BlendFactor::OneMinusSrcAlpha => vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        BlendFactor::DstAlpha => vk::BlendFactor::DST_ALPHA,
        BlendFactor::OneMinusDstAlpha => vk::BlendFactor::ONE_MINUS_DST_ALPHA,
    }
}

fn blend_op(op: BlendOp) -> vk::BlendOp {
    match op {
        BlendOp::Add => vk::BlendOp::ADD,
        BlendOp::Subtract => vk::BlendOp::SUBTRACT,
        BlendOp::ReverseSubtract => vk::BlendOp::REVERSE_SUBTRACT,
        BlendOp::Min => vk::BlendOp::MIN,
        BlendOp::Max => vk::BlendOp::MAX,
    }
}

/// Runs the SPIR-V checks on script input.
///
/// - A panic must not take the process down: it is caught.
/// - The checks recurse over the module, and a deeply nested one could
///   overflow a normal stack, which aborts the process and cannot be caught.
///   Running on a thread with a very large stack, together with the size
///   limit, keeps the inputs measured so far away from that. It is a
///   mitigation, not a proof.
pub(crate) fn run_shader_tool(
    what: &str,
    f: impl FnOnce() -> std::result::Result<CompiledShader, String> + Send,
) -> Result<CompiledShader> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("spirv-checks".into())
            .stack_size(COMPILER_STACK_BYTES)
            .spawn_scoped(scope, f)
            .map_err(|e| -> Box<dyn std::any::Any + Send> {
                Box::new(format!("cannot start the checking thread: {e}"))
            })
            .and_then(|thread| thread.join())
    })
    .map_err(|panic| {
        let detail = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic");
        GfxError::new(
            ErrorCode::ShaderCompile,
            format!("{what} panicked: {detail}"),
        )
    })?
    .map_err(|message| GfxError::new(ErrorCode::ShaderCompile, message))
}

/// Builds the mapping from the `set 0 / binding N` declarations of one stage
/// to the descriptor heaps: the heap index of binding N is the `u32` at the
/// binding's offset in push data.
fn stage_mappings(
    strides: HeapStrides,
    reflection: &Reflection,
    bindings: &[PipelineBinding],
) -> Vec<vk::DescriptorSetAndBindingMappingEXT<'static>> {
    reflection
        .bindings
        .iter()
        .map(|b| {
            let merged = bindings
                .iter()
                .find(|m| m.binding == b.binding)
                .expect("merged from this reflection");
            let stride = match b.kind {
                BindingKind::Sampler => strides.sampler,
                _ => strides.resource,
            };
            // Element `i` of a texture array lives `i` slots after the first.
            let array_stride = match b.kind {
                BindingKind::TextureArray { .. } => stride,
                _ => 0,
            };
            vk::DescriptorSetAndBindingMappingEXT::default()
                .descriptor_set(0)
                .first_binding(b.binding)
                .binding_count(1)
                .resource_mask(vk::SpirvResourceTypeFlagBitsEXT::ALL.into())
                .source(vk::DescriptorMappingSourceEXT::HEAP_WITH_PUSH_INDEX)
                .source_data(vk::DescriptorMappingSourceDataEXT::push_index(
                    vk::DescriptorMappingSourcePushIndexEXT::default()
                        .heap_offset(0)
                        .push_offset(merged.push_offset)
                        .heap_index_stride(stride)
                        .heap_array_stride(array_stride),
                ))
        })
        .collect()
}

/// # Safety
///
/// `result` must come from a pipeline creation call on `device`.
unsafe fn take_pipeline(
    device: &vulkan::Device,
    result: std::result::Result<Vec<vk::Pipeline>, (Vec<vk::Pipeline>, vk::VkResult)>,
) -> Result<vk::Pipeline> {
    match result {
        Ok(pipelines) => pipelines
            .into_iter()
            .next()
            .ok_or_else(|| GfxError::new(ErrorCode::Internal, "no pipeline was returned")),
        Err((pipelines, e)) => {
            for pipeline in pipelines {
                if !pipeline.is_null() {
                    // SAFETY: the pipeline was just created and is unused.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
            }
            Err(vk_error("create pipeline", e))
        }
    }
}

type FrameObjects = (
    Vec<FrameSlot>,
    vk::CommandPool,
    vk::CommandBuffer,
    vk::Fence,
);

/// # Safety
///
/// `ctx.device` must be a valid device.
unsafe fn create_frame_objects(ctx: &DeviceCtx) -> Result<FrameObjects> {
    // SAFETY: guaranteed by the caller. On failure the objects created so far
    // are destroyed with the device by the caller.
    unsafe {
        let make = || -> Result<(vk::CommandPool, vk::CommandBuffer, vk::Fence)> {
            let pool = ctx
                .device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default().queue_family_index(ctx.queue_family),
                    None,
                )
                .map_err(|e| vk_error("create command pool", e))?;
            let cb = ctx
                .device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(|e| vk_error("allocate command buffer", e))?[0];
            let fence = ctx
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|e| vk_error("create fence", e))?;
            Ok((pool, cb, fence))
        };
        let mut frames = Vec::new();
        for _ in 0..FRAMES_IN_FLIGHT {
            let (pool, cb, fence) = make()?;
            frames.push(FrameSlot {
                pool,
                cb,
                fence,
                serial: 0,
                pending: false,
            });
        }
        let (pool, cb, fence) = make()?;
        Ok((frames, pool, cb, fence))
    }
}
