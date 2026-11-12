//! The renderer: textures, render targets, the two passes, readback.
//!
//! Everything is synchronous: each upload, render and readback submits one
//! command buffer and waits for it. That keeps the prototype deterministic;
//! a real frame loop will need per-frame resources instead.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};

use proto_color::{
    AlphaMode, CompositeSpace, DecodeTransform, EncodeTransform, ImageDescription, ToneMapping,
};
use vulkan::vk;
use vulkan_alloc::{
    Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc,
    MemoryLocation,
};

use crate::RenderError;
use crate::device::{DeviceInfo, DeviceSelector, Gpu};
use crate::image::{CompositeImage, EncodedImage};

/// Format of the compositing buffer.
const COMPOSITE_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// Descriptor sets per pool. More pools are created on demand.
const SETS_PER_POOL: u32 = 256;

/// Options for [`Renderer::new`].
#[derive(Debug, Clone, Default)]
pub struct RendererOptions {
    pub device: DeviceSelector,
    /// Enable `VK_LAYER_KHRONOS_validation`. Creating the renderer fails
    /// when the layer is not installed.
    pub validation: bool,
}

/// Memory layout of texture pixels (byte order in memory).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// `wl_shm` `argb8888` on little endian.
    Bgra8,
    /// `wl_shm` `xrgb8888` on little endian. The fourth byte is undefined.
    Bgrx8,
    /// `wl_shm` `abgr8888` on little endian.
    Rgba8,
    /// `wl_shm` `xbgr8888` on little endian. The fourth byte is undefined.
    Rgbx8,
}

impl PixelFormat {
    pub fn bytes_per_pixel(&self) -> usize {
        4
    }

    /// Whether the format has no meaningful alpha channel.
    pub fn is_opaque(&self) -> bool {
        matches!(self, PixelFormat::Bgrx8 | PixelFormat::Rgbx8)
    }

    fn vk_format(&self) -> vk::Format {
        match self {
            PixelFormat::Bgra8 | PixelFormat::Bgrx8 => vk::Format::B8G8R8A8_UNORM,
            PixelFormat::Rgba8 | PixelFormat::Rgbx8 => vk::Format::R8G8B8A8_UNORM,
        }
    }
}

/// Format of the encoded output image of a render target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    Rgba8Unorm,
    Rgba16Unorm,
    /// Bytes in memory are B, G, R, A. This is DRM `XRGB8888` / `ARGB8888`,
    /// the format KMS primary planes take.
    Bgra8Unorm,
}

impl OutputFormat {
    pub const NAMED: [(&'static str, OutputFormat); 3] = [
        ("rgba8", OutputFormat::Rgba8Unorm),
        ("rgba16", OutputFormat::Rgba16Unorm),
        ("bgra8", OutputFormat::Bgra8Unorm),
    ];

    pub fn from_name(name: &str) -> Option<OutputFormat> {
        Self::NAMED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, f)| *f)
    }

    pub fn name(&self) -> &'static str {
        match self {
            OutputFormat::Rgba8Unorm => "rgba8",
            OutputFormat::Rgba16Unorm => "rgba16",
            OutputFormat::Bgra8Unorm => "bgra8",
        }
    }

    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            OutputFormat::Rgba8Unorm | OutputFormat::Bgra8Unorm => 4,
            OutputFormat::Rgba16Unorm => 8,
        }
    }

    /// Bits per channel.
    pub fn bits(&self) -> u32 {
        match self {
            OutputFormat::Rgba8Unorm | OutputFormat::Bgra8Unorm => 8,
            OutputFormat::Rgba16Unorm => 16,
        }
    }

    /// Largest code value of a channel.
    pub fn max_code(&self) -> u16 {
        match self {
            OutputFormat::Rgba8Unorm | OutputFormat::Bgra8Unorm => 255,
            OutputFormat::Rgba16Unorm => 65535,
        }
    }

    fn vk_format(&self) -> vk::Format {
        match self {
            OutputFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
            OutputFormat::Rgba16Unorm => vk::Format::R16G16B16A16_UNORM,
            OutputFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
        }
    }
}

/// DRM fourcc of `XRGB8888`.
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// The linear DRM format modifier.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// The single plane of a render target exported as a dma-buf, with what a
/// KMS framebuffer needs to know about it.
#[derive(Debug)]
pub struct DmabufPlane {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    /// DRM fourcc of the pixel format.
    pub fourcc: u32,
    /// DRM format modifier of the memory layout.
    pub modifier: u64,
    /// Bytes per row.
    pub stride: u32,
    /// Offset of the first pixel in the dma-buf.
    pub offset: u32,
}

/// Description of a texture: its size, memory layout and what its values
/// mean.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextureDesc {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub image_description: ImageDescription,
    pub alpha_mode: AlphaMode,
}

/// Description of a render target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TargetDesc {
    pub width: u32,
    pub height: u32,
    pub format: OutputFormat,
    /// What the encoded output values mean.
    pub image_description: ImageDescription,
    /// The space surfaces are blended in.
    pub composite_space: CompositeSpace,
}

/// Handle of a texture owned by a [`Renderer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId(u64);

/// Handle of a render target owned by a [`Renderer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetId(u64);

impl TargetId {
    /// Makes an id from a number. For [`TargetProvider`]s other than
    /// [`Renderer`], which number their targets themselves.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// What a backend needs from whatever draws the frames: places to draw into
/// that it can show.
///
/// [`Renderer`] is one provider. The script layer's drawing is another; it
/// owns a different Vulkan device, so a backend must not assume more than
/// this trait.
pub trait TargetProvider {
    /// Creates an offscreen target. Frames are taken out of it with
    /// [`Self::read_encoded_into`].
    fn create_target(&mut self, desc: &TargetDesc) -> Result<TargetId, RenderError>;

    /// The single-plane DRM format modifiers a scanout target can have.
    /// Empty when the provider cannot export dma-bufs.
    fn scanout_modifiers(&self) -> Vec<u64>;

    /// Creates a target whose image is exported as a dma-buf for scanout.
    /// See [`Renderer::create_scanout_target`].
    fn create_scanout_target(
        &mut self,
        desc: &TargetDesc,
        allowed_modifiers: &[u64],
    ) -> Result<(TargetId, DmabufPlane), RenderError>;

    fn destroy_target(&mut self, id: TargetId);

    /// Copies the encoded image of the last frame drawn into a target to
    /// caller-provided memory. See [`Renderer::read_encoded_into`].
    fn read_encoded_into(
        &mut self,
        target: TargetId,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), RenderError>;
}

/// A borrowed, optional [`TargetProvider`], as backends receive it. `None`
/// when the compositor runs without anything that draws.
///
/// The object type is `'static` so that the borrow can be shortened and
/// passed on (`as_deref_mut`), which a `&mut dyn Trait` with an elided
/// object lifetime does not allow.
pub type Targets<'a> = Option<&'a mut (dyn TargetProvider + 'static)>;

/// Borrows an optional [`Renderer`] as [`Targets`].
pub fn targets_of(renderer: Option<&mut Renderer>) -> Targets<'_> {
    renderer.map(|r| r as &mut (dyn TargetProvider + 'static))
}

impl TargetProvider for Renderer {
    fn create_target(&mut self, desc: &TargetDesc) -> Result<TargetId, RenderError> {
        Renderer::create_target(self, desc)
    }

    fn scanout_modifiers(&self) -> Vec<u64> {
        Renderer::scanout_modifiers(self)
    }

    fn create_scanout_target(
        &mut self,
        desc: &TargetDesc,
        allowed_modifiers: &[u64],
    ) -> Result<(TargetId, DmabufPlane), RenderError> {
        Renderer::create_scanout_target(self, desc, allowed_modifiers)
    }

    fn destroy_target(&mut self, id: TargetId) {
        Renderer::destroy_target(self, id)
    }

    fn read_encoded_into(
        &mut self,
        target: TargetId,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), RenderError> {
        Renderer::read_encoded_into(self, target, destination, stride)
    }
}

/// Axis-aligned rectangle. Units depend on the use (target pixels for
/// destinations, texels for sources).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// One textured quad of a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Element {
    pub texture: TextureId,
    /// Destination in target pixels, origin at the top left.
    pub dst: Rect,
    /// Source in texels. `None` uses the whole texture.
    pub src: Option<Rect>,
    /// Multiplies color and alpha after conversion to the compositing space.
    pub opacity: f32,
}

/// The color a target is cleared to before the elements are drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Background {
    /// Straight (non-premultiplied) RGBA, encoded per `image_description`.
    pub color: [f32; 4],
    pub image_description: ImageDescription,
}

/// What to draw into a target. Elements are drawn in order, back to front.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub background: Background,
    pub elements: &'a [Element],
}

/// Push constants of the surface pass. Keep in sync with `surface.wgsl`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SurfacePush {
    dst_rect: [f32; 4],
    src_rect: [f32; 4],
    matrix: [[f32; 4]; 3],
    tf: u32,
    tf_param: f32,
    alpha_mode: u32,
    opacity: f32,
    flags: u32,
    _pad: [u32; 3],
}

/// Push constants of the encode pass. Keep in sync with `encode.wgsl`.
#[repr(C)]
#[derive(Clone, Copy)]
struct EncodePush {
    matrix: [[f32; 4]; 3],
    tf: u32,
    tf_param: f32,
    flags: u32,
    _pad: u32,
}

const SURFACE_FLAG_OPAQUE: u32 = 1;
const ENCODE_FLAG_CLIP: u32 = 1;

/// Views a push constant struct as bytes.
///
/// # Safety
///
/// `T` must be `repr(C)` and made only of 4-byte plain values, so it has no
/// padding and every byte is initialized.
unsafe fn push_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: guaranteed by the caller.
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn matrix_rows(m: &[[f32; 3]; 3]) -> [[f32; 4]; 3] {
    [
        [m[0][0], m[0][1], m[0][2], 0.0],
        [m[1][0], m[1][1], m[1][2], 0.0],
        [m[2][0], m[2][1], m[2][2], 0.0],
    ]
}

struct ImageResource {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Option<Allocation>,
}

struct HostBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    size: u64,
}

struct Texture {
    desc: TextureDesc,
    image: ImageResource,
    descriptor_set: vk::DescriptorSet,
    descriptor_pool: vk::DescriptorPool,
    /// Whether pixel data has been uploaded at least once.
    has_contents: bool,
}

struct Target {
    desc: TargetDesc,
    composite: ImageResource,
    composite_set: vk::DescriptorSet,
    composite_pool: vk::DescriptorPool,
    encoded: ImageResource,
    /// The encoded image is shared as a dma-buf (a scanout target). Between
    /// frames it is in `GENERAL` layout and owned by the foreign queue
    /// family, i.e. by whoever scans it out.
    scanout: bool,
    /// Whether a frame has been rendered (the images are then in their
    /// resting layout: `TRANSFER_SRC_OPTIMAL`, or `GENERAL` for the encoded
    /// image of a scanout target).
    rendered: bool,
}

/// The Vulkan renderer. See the crate documentation.
pub struct Renderer {
    textures: HashMap<u64, Texture>,
    targets: HashMap<u64, Target>,
    next_id: u64,

    staging: Option<HostBuffer>,
    readback: Option<HostBuffer>,

    encode_pipelines: HashMap<OutputFormat, vk::Pipeline>,
    surface_pipeline: vk::Pipeline,
    surface_layout: vk::PipelineLayout,
    encode_layout: vk::PipelineLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pools: Vec<vk::DescriptorPool>,
    sampler: vk::Sampler,

    fence: vk::Fence,
    command_buffer: vk::CommandBuffer,
    command_pool: vk::CommandPool,

    allocator: Allocator,
    // Dropped last: destroys the device and the instance.
    gpu: Gpu,
}

impl Renderer {
    pub fn new(options: &RendererOptions) -> Result<Self, RenderError> {
        let gpu = Gpu::new(&options.device, options.validation)?;

        // SAFETY: property queries on valid handles.
        let (memory_properties, device_properties) = unsafe {
            (
                gpu.instance
                    .get_physical_device_memory_properties(gpu.physical_device),
                gpu.instance
                    .get_physical_device_properties(gpu.physical_device),
            )
        };
        let allocator = Allocator::new(AllocatorCreateDesc::new(
            &gpu.device,
            memory_properties,
            device_properties,
        ));

        // The partially built renderer is assembled first with null handles
        // so that Drop cleans up whatever was created if a later step fails.
        let mut renderer = Renderer {
            textures: HashMap::new(),
            targets: HashMap::new(),
            next_id: 1,
            staging: None,
            readback: None,
            encode_pipelines: HashMap::new(),
            surface_pipeline: vk::Pipeline::null(),
            surface_layout: vk::PipelineLayout::null(),
            encode_layout: vk::PipelineLayout::null(),
            descriptor_set_layout: vk::DescriptorSetLayout::null(),
            descriptor_pools: Vec::new(),
            sampler: vk::Sampler::null(),
            fence: vk::Fence::null(),
            command_buffer: vk::CommandBuffer::null(),
            command_pool: vk::CommandPool::null(),
            allocator,
            gpu,
        };
        // SAFETY: the device is valid and nothing else uses it yet.
        unsafe { renderer.init()? };
        Ok(renderer)
    }

    /// Creates the long-lived Vulkan objects.
    ///
    /// # Safety
    ///
    /// Must be called exactly once, right after construction.
    unsafe fn init(&mut self) -> Result<(), RenderError> {
        self.require_format(
            COMPOSITE_FORMAT,
            vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::SAMPLED_IMAGE,
            "R16G16B16A16_SFLOAT",
        )?;

        let device = &self.gpu.device;
        // SAFETY: all create-info structs below point to data that outlives
        // the calls; the device is valid.
        unsafe {
            let pool_info = vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(self.gpu.queue_family);
            self.command_pool = device.create_command_pool(&pool_info, None)?;

            let allocate_info = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            self.command_buffer = device
                .allocate_command_buffers(&allocate_info)?
                .into_iter()
                .next()
                .expect("one command buffer was requested");

            self.fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;

            let sampler_info = vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
            self.sampler = device.create_sampler(&sampler_info, None)?;

            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            ];
            let layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
            self.descriptor_set_layout = device.create_descriptor_set_layout(&layout_info, None)?;

            let set_layouts = [self.descriptor_set_layout];
            let surface_ranges = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(size_of::<SurfacePush>() as u32)];
            let surface_layout_info = vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&set_layouts)
                .push_constant_ranges(&surface_ranges);
            self.surface_layout = device.create_pipeline_layout(&surface_layout_info, None)?;

            let encode_ranges = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .offset(0)
                .size(size_of::<EncodePush>() as u32)];
            let encode_layout_info = vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&set_layouts)
                .push_constant_ranges(&encode_ranges);
            self.encode_layout = device.create_pipeline_layout(&encode_layout_info, None)?;

            self.surface_pipeline = self.create_pipeline(PipelineKind::Surface)?;
        }
        Ok(())
    }

    /// The device the renderer runs on.
    pub fn device_info(&self) -> &DeviceInfo {
        &self.gpu.info
    }

    /// Number of validation errors reported so far, or `None` when the
    /// validation layer is not enabled.
    pub fn validation_errors(&self) -> Option<u32> {
        self.gpu.validation_errors()
    }

    /// Number of validation warnings reported so far, or `None` when the
    /// validation layer is not enabled.
    pub fn validation_warnings(&self) -> Option<u32> {
        self.gpu.validation_warnings()
    }

    // ---------------------------------------------------------------------
    // Textures
    // ---------------------------------------------------------------------

    /// Creates a texture. It has no contents until [`Self::upload_texture`].
    pub fn create_texture(&mut self, desc: &TextureDesc) -> Result<TextureId, RenderError> {
        check_size(desc.width, desc.height)?;
        // SAFETY: the device is valid; resources are tracked in `self`.
        unsafe {
            let mut image = self.create_image(
                desc.width,
                desc.height,
                desc.format.vk_format(),
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
                "texture",
            )?;
            let (descriptor_set, descriptor_pool) = match self.allocate_image_set(image.view) {
                Ok(v) => v,
                Err(e) => {
                    self.destroy_image(&mut image);
                    return Err(e);
                }
            };
            let id = self.next_id;
            self.next_id += 1;
            self.textures.insert(
                id,
                Texture {
                    desc: *desc,
                    image,
                    descriptor_set,
                    descriptor_pool,
                    has_contents: false,
                },
            );
            Ok(TextureId(id))
        }
    }

    /// Returns the description of a texture.
    pub fn texture_desc(&self, id: TextureId) -> Option<&TextureDesc> {
        self.textures.get(&id.0).map(|t| &t.desc)
    }

    /// Changes what the values of a texture mean, without touching its
    /// pixels.
    pub fn set_texture_color(
        &mut self,
        id: TextureId,
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> Result<(), RenderError> {
        let texture = self
            .textures
            .get_mut(&id.0)
            .ok_or(RenderError::UnknownTexture)?;
        texture.desc.image_description = image_description;
        texture.desc.alpha_mode = alpha_mode;
        Ok(())
    }

    /// Replaces the whole contents of a texture.
    ///
    /// `data` holds `height` rows of `stride` bytes (the last row may be
    /// shorter, as long as it covers the pixels).
    pub fn upload_texture(
        &mut self,
        id: TextureId,
        data: &[u8],
        stride: u32,
    ) -> Result<(), RenderError> {
        let texture = self
            .textures
            .get(&id.0)
            .ok_or(RenderError::UnknownTexture)?;
        let desc = texture.desc;
        let image = texture.image.image;
        let bpp = desc.format.bytes_per_pixel() as u32;
        if stride < desc.width * bpp || !stride.is_multiple_of(bpp) {
            return Err(RenderError::InvalidStride {
                stride,
                width: desc.width,
            });
        }
        let needed = stride as usize * (desc.height as usize - 1) + (desc.width * bpp) as usize;
        if data.len() < needed {
            return Err(RenderError::DataTooShort {
                actual: data.len(),
                expected: needed,
            });
        }

        // SAFETY: the staging buffer is large enough for `needed` bytes, the
        // copy region lies inside the image, and the command buffer is
        // submitted and awaited before returning.
        unsafe {
            self.ensure_host_buffer(BufferRole::Staging, needed as u64)?;
            let staging = self.staging.as_ref().expect("just ensured");
            let staging_buffer = staging.buffer;
            {
                let allocation = staging.allocation.as_ref().expect("live buffer");
                // The guard unmaps the memory at the end of this block.
                let mut mapped = self.allocator.map(allocation).map_err(alloc_error)?;
                mapped
                    .byte_range_mut(0, needed)
                    .copy_from_slice(&data[..needed]);
                mapped.flush().map_err(alloc_error)?;
            }
            let device = &self.gpu.device;
            let cb = self.command_buffer;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            device.begin_command_buffer(cb, &begin)?;
            // The whole image is overwritten, so the old contents can be
            // discarded (old layout UNDEFINED).
            image_barrier(
                device,
                cb,
                image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
            );
            let region = [vk::BufferImageCopy::default()
                .buffer_row_length(stride / bpp)
                .image_subresource(color_layers())
                .image_extent(
                    vk::Extent3D::default()
                        .width(desc.width)
                        .height(desc.height)
                        .depth(1),
                )];
            device.cmd_copy_buffer_to_image(
                cb,
                staging_buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            );
            image_barrier(
                device,
                cb,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            );
            device.end_command_buffer(cb)?;
            self.submit_and_wait()?;
        }

        self.textures
            .get_mut(&id.0)
            .expect("checked above")
            .has_contents = true;
        Ok(())
    }

    /// Destroys a texture. Unknown ids are ignored.
    pub fn destroy_texture(&mut self, id: TextureId) {
        if let Some(mut texture) = self.textures.remove(&id.0) {
            // SAFETY: every use of the texture has completed (rendering is
            // synchronous), and the set came from the recorded pool.
            unsafe {
                let _ = self
                    .gpu
                    .device
                    .free_descriptor_sets(texture.descriptor_pool, &[texture.descriptor_set]);
                self.destroy_image(&mut texture.image);
            }
        }
    }

    // ---------------------------------------------------------------------
    // Render targets
    // ---------------------------------------------------------------------

    /// Creates an offscreen render target.
    pub fn create_target(&mut self, desc: &TargetDesc) -> Result<TargetId, RenderError> {
        self.prepare_target(desc)?;
        // SAFETY: the device is valid; the image is tracked by the target.
        let encoded = unsafe {
            self.create_image(
                desc.width,
                desc.height,
                desc.format.vk_format(),
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                "encoded output",
            )?
        };
        self.finish_target(desc, encoded, false)
    }

    /// Whether [`Self::create_scanout_target`] can work on this device: the
    /// dma-buf extensions are there and `B8G8R8A8_UNORM` can be rendered to
    /// with at least one single-plane DRM format modifier.
    pub fn supports_scanout_export(&self) -> bool {
        !self.scanout_modifiers().is_empty()
    }

    /// The single-plane DRM format modifiers a scanout target can have.
    pub fn scanout_modifiers(&self) -> Vec<u64> {
        if !self.gpu.dmabuf_export {
            return Vec::new();
        }
        let format = OutputFormat::Bgra8Unorm.vk_format();
        // SAFETY: property queries on valid handles; the second call passes
        // an array of the size the first call reported.
        unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            {
                let mut properties = vk::FormatProperties2::default().push_next(&mut list);
                self.gpu.instance.get_physical_device_format_properties2(
                    self.gpu.physical_device,
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
                    self.gpu.instance.get_physical_device_format_properties2(
                        self.gpu.physical_device,
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

    /// Creates a render target whose encoded image can be scanned out: the
    /// image is allocated by Vulkan and exported as a dma-buf.
    ///
    /// `desc.format` must be [`OutputFormat::Bgra8Unorm`] (DRM `XRGB8888`).
    /// `allowed_modifiers` are the DRM format modifiers the consumer takes;
    /// pass `[DRM_FORMAT_MOD_LINEAR]` for a KMS driver that does not report
    /// modifiers.
    ///
    /// After each [`Self::render`] the image belongs to the consumer until
    /// the next `render` into the same target. Rendering waits for the GPU,
    /// so the contents are complete when `render` returns.
    pub fn create_scanout_target(
        &mut self,
        desc: &TargetDesc,
        allowed_modifiers: &[u64],
    ) -> Result<(TargetId, DmabufPlane), RenderError> {
        if desc.format != OutputFormat::Bgra8Unorm {
            return Err(RenderError::UnsupportedFormat(
                "scanout targets must be bgra8 (DRM XRGB8888)",
            ));
        }
        let candidates: Vec<u64> = self
            .scanout_modifiers()
            .into_iter()
            .filter(|m| allowed_modifiers.contains(m))
            .collect();
        if candidates.is_empty() {
            return Err(RenderError::ScanoutUnsupported);
        }
        self.prepare_target(desc)?;

        let format = desc.format.vk_format();
        let dma_buf = vk::ExternalMemoryHandleTypeFlagBits::DMA_BUF_EXT;
        // SAFETY: the device is valid and has the dma-buf extensions
        // enabled (checked through `scanout_modifiers`). Each failure path
        // destroys what was created before it.
        let (encoded, plane) = unsafe {
            let device = &self.gpu.device;
            let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                .drm_format_modifiers(&candidates);
            let mut external =
                vk::ExternalMemoryImageCreateInfo::default().handle_types(dma_buf.into());
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::_2D)
                .format(format)
                .extent(
                    vk::Extent3D::default()
                        .width(desc.width)
                        .height(desc.height)
                        .depth(1),
                )
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlagBits::_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut modifier_list)
                .push_next(&mut external);
            let image = device.create_image(&info, None)?;
            let requirements = device.get_image_memory_requirements(image);
            // An exported handle refers to a whole memory object, so the
            // image gets its own.
            let allocation = match self.allocator.allocate(AllocationCreateDesc {
                name: Some("scanout image"),
                requirements,
                location: MemoryLocation::GpuOnly,
                linear: false,
                memory_allocate_flags: vk::MemoryAllocateFlags::empty(),
                export_handle_types: dma_buf.into(),
                allocation_scheme: AllocationScheme::DedicatedImage(image),
            }) {
                Ok(allocation) => allocation,
                Err(e) => {
                    device.destroy_image(image, None);
                    return Err(alloc_error(e));
                }
            };
            let mut resource = ImageResource {
                image,
                view: vk::ImageView::null(),
                allocation: Some(allocation),
            };
            let allocation = resource.allocation.as_ref().expect("just set");
            let result = (|| -> Result<DmabufPlane, RenderError> {
                device.bind_image_memory(image, allocation.memory(), allocation.offset())?;
                let view_info = vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::_2D)
                    .format(format)
                    .subresource_range(color_range());
                resource.view = device.create_image_view(&view_info, None)?;

                let mut chosen = vk::ImageDrmFormatModifierPropertiesEXT::default();
                device.get_image_drm_format_modifier_properties_ext(image, &mut chosen)?;
                let layout = device.get_image_subresource_layout(
                    image,
                    &vk::ImageSubresource {
                        aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                        mip_level: 0,
                        array_layer: 0,
                    },
                );
                let raw_fd = device.get_memory_fd_khr(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(allocation.memory())
                        .handle_type(dma_buf),
                )?;
                Ok(DmabufPlane {
                    // SAFETY: Vulkan returns a new file descriptor that the
                    // caller owns.
                    fd: OwnedFd::from_raw_fd(raw_fd),
                    width: desc.width,
                    height: desc.height,
                    fourcc: DRM_FORMAT_XRGB8888,
                    modifier: chosen.drm_format_modifier,
                    stride: layout.row_pitch as u32,
                    offset: (allocation.offset() + layout.offset) as u32,
                })
            })();
            match result {
                Ok(plane) => (resource, plane),
                Err(e) => {
                    if resource.view.is_null() {
                        // destroy_image destroys the view too; a null view
                        // is fine to destroy.
                    }
                    self.destroy_image(&mut resource);
                    return Err(e);
                }
            }
        };
        let id = self.finish_target(desc, encoded, true)?;
        Ok((id, plane))
    }

    /// Checks a target description and makes sure its encode pipeline
    /// exists.
    fn prepare_target(&mut self, desc: &TargetDesc) -> Result<(), RenderError> {
        check_size(desc.width, desc.height)?;
        self.require_format(
            desc.format.vk_format(),
            vk::FormatFeatureFlags::COLOR_ATTACHMENT,
            desc.format.name(),
        )?;
        // Fail early on image descriptions the pipeline cannot convert.
        EncodeTransform::new(&desc.composite_space, &desc.image_description)
            .ok_or(RenderError::DegeneratePrimaries)?;
        if !self.encode_pipelines.contains_key(&desc.format) {
            // SAFETY: the pipeline layouts exist after init().
            let pipeline = unsafe { self.create_pipeline(PipelineKind::Encode(desc.format))? };
            self.encode_pipelines.insert(desc.format, pipeline);
        }
        Ok(())
    }

    /// Creates the compositing buffer for a target and registers it.
    /// Takes ownership of `encoded` (destroys it on failure).
    fn finish_target(
        &mut self,
        desc: &TargetDesc,
        mut encoded: ImageResource,
        scanout: bool,
    ) -> Result<TargetId, RenderError> {
        // SAFETY: the device is valid; resources are tracked in `self`, and
        // partially created ones are destroyed on error.
        unsafe {
            let mut composite = match self.create_image(
                desc.width,
                desc.height,
                COMPOSITE_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC,
                "compositing buffer",
            ) {
                Ok(image) => image,
                Err(e) => {
                    self.destroy_image(&mut encoded);
                    return Err(e);
                }
            };
            let (composite_set, composite_pool) = match self.allocate_image_set(composite.view) {
                Ok(v) => v,
                Err(e) => {
                    self.destroy_image(&mut composite);
                    self.destroy_image(&mut encoded);
                    return Err(e);
                }
            };

            let id = self.next_id;
            self.next_id += 1;
            self.targets.insert(
                id,
                Target {
                    desc: *desc,
                    composite,
                    composite_set,
                    composite_pool,
                    encoded,
                    scanout,
                    rendered: false,
                },
            );
            Ok(TargetId(id))
        }
    }

    /// Returns the description of a render target.
    pub fn target_desc(&self, id: TargetId) -> Option<&TargetDesc> {
        self.targets.get(&id.0).map(|t| &t.desc)
    }

    /// Destroys a render target. Unknown ids are ignored.
    pub fn destroy_target(&mut self, id: TargetId) {
        if let Some(mut target) = self.targets.remove(&id.0) {
            // SAFETY: rendering is synchronous, so the target is idle.
            unsafe {
                let _ = self
                    .gpu
                    .device
                    .free_descriptor_sets(target.composite_pool, &[target.composite_set]);
                self.destroy_image(&mut target.composite);
                self.destroy_image(&mut target.encoded);
            }
        }
    }

    // ---------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------

    /// Renders a frame into a target: blends the elements over the
    /// background in the compositing space, then encodes the result for the
    /// target's image description.
    ///
    /// Elements whose texture is unknown or has no contents yet are skipped.
    pub fn render(&mut self, target: TargetId, frame: &Frame<'_>) -> Result<(), RenderError> {
        let target_state = self
            .targets
            .get(&target.0)
            .ok_or(RenderError::UnknownTarget)?;
        let desc = target_state.desc;
        let space = desc.composite_space;

        let background = DecodeTransform::new(
            &frame.background.image_description,
            AlphaMode::Straight,
            &space,
        )
        .ok_or(RenderError::DegeneratePrimaries)?
        .apply(frame.background.color);

        // Resolve the elements up front so recording cannot fail half way.
        let mut draws = Vec::with_capacity(frame.elements.len());
        for element in frame.elements {
            let Some(texture) = self.textures.get(&element.texture.0) else {
                continue;
            };
            if !texture.has_contents {
                continue;
            }
            let decode = DecodeTransform::new(
                &texture.desc.image_description,
                texture.desc.alpha_mode,
                &space,
            )
            .ok_or(RenderError::DegeneratePrimaries)?;
            let (tw, th) = (texture.desc.width as f32, texture.desc.height as f32);
            let src = element.src.unwrap_or(Rect::new(0.0, 0.0, tw, th));
            let (w, h) = (desc.width as f32, desc.height as f32);
            let push = SurfacePush {
                dst_rect: [
                    element.dst.x * 2.0 / w - 1.0,
                    element.dst.y * 2.0 / h - 1.0,
                    element.dst.width * 2.0 / w,
                    element.dst.height * 2.0 / h,
                ],
                src_rect: [src.x / tw, src.y / th, src.width / tw, src.height / th],
                matrix: matrix_rows(&decode.matrix),
                tf: decode.transfer.shader_id(),
                tf_param: decode.transfer.shader_param(),
                alpha_mode: decode.alpha_mode.shader_id(),
                opacity: element.opacity,
                flags: if texture.desc.format.is_opaque() {
                    SURFACE_FLAG_OPAQUE
                } else {
                    0
                },
                _pad: [0; 3],
            };
            draws.push((texture.descriptor_set, push));
        }

        let encode = EncodeTransform::new(&space, &desc.image_description)
            .ok_or(RenderError::DegeneratePrimaries)?;
        let clip = match encode.tone_mapping {
            ToneMapping::Clip => !encode.transfer.is_extended_range(),
        };
        let encode_push = EncodePush {
            matrix: matrix_rows(&encode.matrix),
            tf: encode.transfer.shader_id(),
            tf_param: encode.transfer.shader_param(),
            flags: if clip { ENCODE_FLAG_CLIP } else { 0 },
            _pad: 0,
        };
        let encode_pipeline = *self
            .encode_pipelines
            .get(&desc.format)
            .expect("created with the target");

        // SAFETY: all handles belong to this renderer and are alive; the
        // command buffer is submitted and awaited before returning.
        unsafe {
            let device = &self.gpu.device;
            let cb = self.command_buffer;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            device.begin_command_buffer(cb, &begin)?;

            let extent = vk::Extent2D::default()
                .width(desc.width)
                .height(desc.height);
            let render_area = vk::Rect2D::default()
                .offset(vk::Offset2D::default())
                .extent(extent);
            let viewport = [vk::Viewport::default()
                .width(desc.width as f32)
                .height(desc.height as f32)
                .max_depth(1.0)];
            let scissor = [render_area];

            // Pass 1: blend the surfaces into the compositing buffer.
            // Both attachments are fully overwritten each frame, so their
            // previous contents are discarded.
            image_barrier(
                device,
                cb,
                target_state.composite.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
            );
            let clear = vk::ClearValue::color(vk::ClearColorValue::float32(background));
            let attachments = [vk::RenderingAttachmentInfo::default()
                .image_view(target_state.composite.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(clear)];
            let rendering = vk::RenderingInfo::default()
                .render_area(render_area)
                .layer_count(1)
                .color_attachments(&attachments);
            device.cmd_begin_rendering(cb, &rendering);
            device.cmd_set_viewport(cb, 0, &viewport);
            device.cmd_set_scissor(cb, 0, &scissor);
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, self.surface_pipeline);
            for (set, push) in &draws {
                device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.surface_layout,
                    0,
                    &[*set],
                    &[],
                );
                device.cmd_push_constants(
                    cb,
                    self.surface_layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    push_bytes(push),
                );
                device.cmd_draw(cb, 4, 1, 0, 0);
            }
            device.cmd_end_rendering(cb);

            // Pass 2: encode the compositing buffer for the output.
            image_barrier(
                device,
                cb,
                target_state.composite.image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            );
            // A scanout target that was shown is taken back from the
            // foreign queue family (its consumer); everything else starts
            // from scratch.
            let reacquire = target_state.scanout && target_state.rendered;
            image_barrier_transfer(
                device,
                cb,
                target_state.encoded.image,
                if reacquire {
                    vk::ImageLayout::GENERAL
                } else {
                    vk::ImageLayout::UNDEFINED
                },
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                vk::AccessFlags2::empty(),
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                reacquire.then_some((vk::QUEUE_FAMILY_FOREIGN_EXT, self.gpu.queue_family)),
            );
            let attachments = [vk::RenderingAttachmentInfo::default()
                .image_view(target_state.encoded.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::DONT_CARE)
                .store_op(vk::AttachmentStoreOp::STORE)];
            let rendering = vk::RenderingInfo::default()
                .render_area(render_area)
                .layer_count(1)
                .color_attachments(&attachments);
            device.cmd_begin_rendering(cb, &rendering);
            device.cmd_set_viewport(cb, 0, &viewport);
            device.cmd_set_scissor(cb, 0, &scissor);
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, encode_pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::GRAPHICS,
                self.encode_layout,
                0,
                &[target_state.composite_set],
                &[],
            );
            device.cmd_push_constants(
                cb,
                self.encode_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                push_bytes(&encode_push),
            );
            device.cmd_draw(cb, 3, 1, 0, 0);
            device.cmd_end_rendering(cb);

            // Leave both images ready for readback.
            image_barrier(
                device,
                cb,
                target_state.composite.image,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_READ,
            );
            if target_state.scanout {
                // Hand the image to its consumer.
                image_barrier_transfer(
                    device,
                    cb,
                    target_state.encoded.image,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::GENERAL,
                    vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                    vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags2::ALL_COMMANDS,
                    vk::AccessFlags2::empty(),
                    Some((self.gpu.queue_family, vk::QUEUE_FAMILY_FOREIGN_EXT)),
                );
            } else {
                image_barrier(
                    device,
                    cb,
                    target_state.encoded.image,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                    vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags2::COPY,
                    vk::AccessFlags2::TRANSFER_READ,
                );
            }
            device.end_command_buffer(cb)?;
            self.submit_and_wait()?;
        }

        self.targets
            .get_mut(&target.0)
            .expect("checked above")
            .rendered = true;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Readback
    // ---------------------------------------------------------------------

    /// Reads back the encoded output of the last rendered frame.
    pub fn read_encoded(&mut self, target: TargetId) -> Result<EncodedImage, RenderError> {
        let desc = *self.target_desc(target).ok_or(RenderError::UnknownTarget)?;
        let row = desc.width as usize * desc.format.bytes_per_pixel();
        let mut data = vec![0u8; row * desc.height as usize];
        self.read_encoded_into(target, &mut data, row)?;
        Ok(EncodedImage {
            width: desc.width,
            height: desc.height,
            format: desc.format,
            image_description: desc.image_description,
            data,
        })
    }

    /// Reads back the encoded output of the last rendered frame into
    /// caller-provided memory with rows `stride` bytes apart (for example a
    /// mapped KMS dumb buffer). Bytes between the end of a row and the next
    /// row are left untouched.
    pub fn read_encoded_into(
        &mut self,
        target: TargetId,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), RenderError> {
        let state = self
            .targets
            .get(&target.0)
            .ok_or(RenderError::UnknownTarget)?;
        if !state.rendered {
            return Err(RenderError::TargetNotRendered);
        }
        let desc = state.desc;
        let image = state.encoded.image;
        // A scanout target rests in GENERAL (see `Target::scanout`). Reading
        // it while its consumer nominally owns it is only done for
        // verification captures.
        let layout = if state.scanout {
            vk::ImageLayout::GENERAL
        } else {
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL
        };
        let row = desc.width as usize * desc.format.bytes_per_pixel();
        let height = desc.height as usize;
        let needed = stride * (height - 1) + row;
        if stride < row {
            return Err(RenderError::InvalidStride {
                stride: stride as u32,
                width: desc.width,
            });
        }
        if destination.len() < needed {
            return Err(RenderError::DataTooShort {
                actual: destination.len(),
                expected: needed,
            });
        }
        // SAFETY: the image is alive and in `layout` after render().
        unsafe {
            self.read_image(
                image,
                layout,
                desc.width,
                desc.height,
                row * height,
                |bytes| {
                    if stride == row {
                        destination[..bytes.len()].copy_from_slice(bytes);
                    } else {
                        for y in 0..height {
                            destination[y * stride..y * stride + row]
                                .copy_from_slice(&bytes[y * row..(y + 1) * row]);
                        }
                    }
                },
            )
        }
    }

    /// Reads back the compositing buffer of the last rendered frame.
    pub fn read_composite(&mut self, target: TargetId) -> Result<CompositeImage, RenderError> {
        let state = self
            .targets
            .get(&target.0)
            .ok_or(RenderError::UnknownTarget)?;
        if !state.rendered {
            return Err(RenderError::TargetNotRendered);
        }
        let desc = state.desc;
        let image = state.composite.image;
        let size = desc.width as usize * desc.height as usize * 8;
        let mut data = Vec::new();
        // SAFETY: the image is in TRANSFER_SRC_OPTIMAL after render().
        unsafe {
            self.read_image(
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                desc.width,
                desc.height,
                size,
                |bytes| {
                    data = bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                        .collect();
                },
            )?;
        }
        Ok(CompositeImage {
            width: desc.width,
            height: desc.height,
            space: desc.composite_space,
            data,
        })
    }

    /// Copies an image to host memory and hands the tightly packed bytes to
    /// `consume`.
    ///
    /// # Safety
    ///
    /// `image` must be alive and in `layout`, and `size` must be the byte
    /// size of its single mip level.
    unsafe fn read_image(
        &mut self,
        image: vk::Image,
        layout: vk::ImageLayout,
        width: u32,
        height: u32,
        size: usize,
        consume: impl FnOnce(&[u8]),
    ) -> Result<(), RenderError> {
        // SAFETY: guaranteed by the caller; the readback buffer is at least
        // `size` bytes and the command buffer is awaited before mapping.
        unsafe {
            self.ensure_host_buffer(BufferRole::Readback, size as u64)?;
            let buffer = self.readback.as_ref().expect("just ensured").buffer;

            let device = &self.gpu.device;
            let cb = self.command_buffer;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            device.begin_command_buffer(cb, &begin)?;
            let region = [vk::BufferImageCopy::default()
                .image_subresource(color_layers())
                .image_extent(vk::Extent3D::default().width(width).height(height).depth(1))];
            device.cmd_copy_image_to_buffer(cb, image, layout, buffer, &region);
            let barriers = [vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)
                .buffer(buffer)
                .offset(0)
                .size(size as u64)];
            let dependency = vk::DependencyInfo::default().buffer_memory_barriers(&barriers);
            device.cmd_pipeline_barrier2(cb, &dependency);
            device.end_command_buffer(cb)?;
            self.submit_and_wait()?;

            let readback = self.readback.as_ref().expect("just ensured");
            let allocation = readback.allocation.as_ref().expect("live buffer");
            // The guard unmaps the memory when it goes out of scope.
            let mut mapped = self.allocator.map(allocation).map_err(alloc_error)?;
            mapped.invalidate().map_err(alloc_error)?;
            consume(mapped.byte_range(0, size));
            Ok(())
        }
    }

    // ---------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------

    fn require_format(
        &self,
        format: vk::Format,
        features: vk::FormatFeatureFlags,
        name: &'static str,
    ) -> Result<(), RenderError> {
        // SAFETY: property query on valid handles.
        let properties = unsafe {
            self.gpu
                .instance
                .get_physical_device_format_properties(self.gpu.physical_device, format)
        };
        if properties.optimal_tiling_features.contains(features) {
            Ok(())
        } else {
            Err(RenderError::UnsupportedFormat(name))
        }
    }

    /// # Safety
    ///
    /// The command buffer must be fully recorded and ended.
    unsafe fn submit_and_wait(&self) -> Result<(), RenderError> {
        let device = &self.gpu.device;
        let command_buffers = [self.command_buffer];
        let submit = [vk::SubmitInfo::default().command_buffers(&command_buffers)];
        // SAFETY: guaranteed by the caller; the fence is unsignaled.
        unsafe {
            device.queue_submit(self.gpu.queue, &submit, self.fence)?;
            device.wait_for_fences(&[self.fence], true, u64::MAX)?;
            device.reset_fences(&[self.fence])?;
        }
        Ok(())
    }

    /// # Safety
    ///
    /// The device must be valid. The returned image must be destroyed with
    /// [`Self::destroy_image`].
    unsafe fn create_image(
        &mut self,
        width: u32,
        height: u32,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        name: &str,
    ) -> Result<ImageResource, RenderError> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::_2D)
            .format(format)
            .extent(vk::Extent3D::default().width(width).height(height).depth(1))
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlagBits::_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: guaranteed by the caller; each failure path destroys what
        // was created before it.
        unsafe {
            let device = &self.gpu.device;
            let image = device.create_image(&info, None)?;
            let requirements = device.get_image_memory_requirements(image);
            let allocation = match self.allocator.allocate(AllocationCreateDesc {
                name: Some(name),
                requirements,
                location: MemoryLocation::GpuOnly,
                linear: false,
                memory_allocate_flags: vk::MemoryAllocateFlags::empty(),
                export_handle_types: vk::ExternalMemoryHandleTypeFlags::empty(),
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            }) {
                Ok(allocation) => allocation,
                Err(e) => {
                    device.destroy_image(image, None);
                    return Err(alloc_error(e));
                }
            };
            if let Err(e) =
                device.bind_image_memory(image, allocation.memory(), allocation.offset())
            {
                let _ = self.allocator.free(allocation);
                device.destroy_image(image, None);
                return Err(e.into());
            }
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::_2D)
                .format(format)
                .subresource_range(color_range());
            let view = match device.create_image_view(&view_info, None) {
                Ok(view) => view,
                Err(e) => {
                    device.destroy_image(image, None);
                    let _ = self.allocator.free(allocation);
                    return Err(e.into());
                }
            };
            Ok(ImageResource {
                image,
                view,
                allocation: Some(allocation),
            })
        }
    }

    /// # Safety
    ///
    /// The image must not be in use by pending commands.
    unsafe fn destroy_image(&mut self, image: &mut ImageResource) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.gpu.device.destroy_image_view(image.view, None);
            self.gpu.device.destroy_image(image.image, None);
            if let Some(allocation) = image.allocation.take() {
                let _ = self.allocator.free(allocation);
            }
        }
    }

    /// Makes sure the staging or readback buffer holds at least `size` bytes.
    ///
    /// # Safety
    ///
    /// The buffer being replaced must not be in use by pending commands.
    unsafe fn ensure_host_buffer(
        &mut self,
        role: BufferRole,
        size: u64,
    ) -> Result<(), RenderError> {
        let slot = match role {
            BufferRole::Staging => &mut self.staging,
            BufferRole::Readback => &mut self.readback,
        };
        if slot.as_ref().is_some_and(|b| b.size >= size) {
            return Ok(());
        }
        // SAFETY: guaranteed by the caller; each failure path destroys what
        // was created before it.
        unsafe {
            if let Some(mut old) = slot.take() {
                self.gpu.device.destroy_buffer(old.buffer, None);
                if let Some(allocation) = old.allocation.take() {
                    let _ = self.allocator.free(allocation);
                }
            }
            let (usage, location, name) = match role {
                BufferRole::Staging => (
                    vk::BufferUsageFlags::TRANSFER_SRC,
                    MemoryLocation::CpuToGpu,
                    "staging buffer",
                ),
                BufferRole::Readback => (
                    vk::BufferUsageFlags::TRANSFER_DST,
                    MemoryLocation::GpuToCpu,
                    "readback buffer",
                ),
            };
            // Round up so a growing surface does not reallocate every time.
            let size = size.next_power_of_two().max(64 * 1024);
            let info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let device = &self.gpu.device;
            let buffer = device.create_buffer(&info, None)?;
            let requirements = device.get_buffer_memory_requirements(buffer);
            let allocation = match self.allocator.allocate(AllocationCreateDesc {
                name: Some(name),
                requirements,
                location,
                linear: true,
                memory_allocate_flags: vk::MemoryAllocateFlags::empty(),
                export_handle_types: vk::ExternalMemoryHandleTypeFlags::empty(),
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            }) {
                Ok(allocation) => allocation,
                Err(e) => {
                    device.destroy_buffer(buffer, None);
                    return Err(alloc_error(e));
                }
            };
            if let Err(e) =
                device.bind_buffer_memory(buffer, allocation.memory(), allocation.offset())
            {
                let _ = self.allocator.free(allocation);
                device.destroy_buffer(buffer, None);
                return Err(e.into());
            }
            let slot = match role {
                BufferRole::Staging => &mut self.staging,
                BufferRole::Readback => &mut self.readback,
            };
            *slot = Some(HostBuffer {
                buffer,
                allocation: Some(allocation),
                size,
            });
        }
        Ok(())
    }

    /// Allocates a descriptor set for sampling `view` and writes it.
    ///
    /// # Safety
    ///
    /// `view` must be a valid image view of this device.
    unsafe fn allocate_image_set(
        &mut self,
        view: vk::ImageView,
    ) -> Result<(vk::DescriptorSet, vk::DescriptorPool), RenderError> {
        let layouts = [self.descriptor_set_layout];
        // SAFETY: guaranteed by the caller; pools and layouts are alive.
        unsafe {
            let device = &self.gpu.device;
            let mut allocated = None;
            if let Some(pool) = self.descriptor_pools.last().copied() {
                let info = vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts);
                match device.allocate_descriptor_sets(&info) {
                    Ok(sets) => allocated = Some((sets[0], pool)),
                    Err(vk::VkResult::ERROR_OUT_OF_POOL_MEMORY)
                    | Err(vk::VkResult::ERROR_FRAGMENTED_POOL) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let (set, pool) = match allocated {
                Some(v) => v,
                None => {
                    let sizes = [
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLED_IMAGE)
                            .descriptor_count(SETS_PER_POOL),
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLER)
                            .descriptor_count(SETS_PER_POOL),
                    ];
                    let pool_info = vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(SETS_PER_POOL)
                        .pool_sizes(&sizes);
                    let pool = device.create_descriptor_pool(&pool_info, None)?;
                    self.descriptor_pools.push(pool);
                    let info = vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(&layouts);
                    (device.allocate_descriptor_sets(&info)?[0], pool)
                }
            };

            let image_info = [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let sampler_info = [vk::DescriptorImageInfo::default().sampler(self.sampler)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&sampler_info),
            ];
            device.update_descriptor_sets(&writes, &[]);
            Ok((set, pool))
        }
    }

    /// # Safety
    ///
    /// The pipeline layouts must have been created.
    unsafe fn create_pipeline(&self, kind: PipelineKind) -> Result<vk::Pipeline, RenderError> {
        let (vert_spv, frag_spv, layout, format, topology, blend) = match kind {
            PipelineKind::Surface => (
                &include_bytes!(concat!(env!("OUT_DIR"), "/surface.vert.spv"))[..],
                &include_bytes!(concat!(env!("OUT_DIR"), "/surface.frag.spv"))[..],
                self.surface_layout,
                COMPOSITE_FORMAT,
                vk::PrimitiveTopology::TRIANGLE_STRIP,
                true,
            ),
            PipelineKind::Encode(format) => (
                &include_bytes!(concat!(env!("OUT_DIR"), "/encode.vert.spv"))[..],
                &include_bytes!(concat!(env!("OUT_DIR"), "/encode.frag.spv"))[..],
                self.encode_layout,
                format.vk_format(),
                vk::PrimitiveTopology::TRIANGLE_LIST,
                false,
            ),
        };

        let device = &self.gpu.device;
        // SAFETY: guaranteed by the caller; the shader modules are destroyed
        // after pipeline creation whether or not it succeeds.
        unsafe {
            let vert = create_shader_module(device, vert_spv)?;
            let frag = match create_shader_module(device, frag_spv) {
                Ok(m) => m,
                Err(e) => {
                    device.destroy_shader_module(vert, None);
                    return Err(e);
                }
            };

            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlagBits::VERTEX)
                    .module(vert)
                    .name(c"vs_main"),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlagBits::FRAGMENT)
                    .module(frag)
                    .name(c"fs_main"),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let input_assembly =
                vk::PipelineInputAssemblyStateCreateInfo::default().topology(topology);
            let viewport_state = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::empty())
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlagBits::_1);
            let write_mask = vk::ColorComponentFlags::R
                | vk::ColorComponentFlags::G
                | vk::ColorComponentFlags::B
                | vk::ColorComponentFlags::A;
            // Premultiplied "source over".
            let blend_attachment = [if blend {
                vk::PipelineColorBlendAttachmentState::default()
                    .blend_enable(true)
                    .src_color_blend_factor(vk::BlendFactor::ONE)
                    .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .color_blend_op(vk::BlendOp::ADD)
                    .src_alpha_blend_factor(vk::BlendFactor::ONE)
                    .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                    .alpha_blend_op(vk::BlendOp::ADD)
                    .color_write_mask(write_mask)
            } else {
                vk::PipelineColorBlendAttachmentState::default().color_write_mask(write_mask)
            }];
            let color_blend =
                vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
            let formats = [format];
            let mut rendering =
                vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&raster)
                .multisample_state(&multisample)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic)
                .layout(layout)
                .push_next(&mut rendering);
            let result = device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None);
            device.destroy_shader_module(vert, None);
            device.destroy_shader_module(frag, None);
            match result {
                Ok(pipelines) => Ok(pipelines[0]),
                Err((pipelines, e)) => {
                    for pipeline in pipelines {
                        if !pipeline.is_null() {
                            device.destroy_pipeline(pipeline, None);
                        }
                    }
                    Err(e.into())
                }
            }
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: waits for the device to go idle, then destroys every
        // object created from it. `gpu` (dropped afterwards) destroys the
        // device itself.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();

            let texture_ids: Vec<u64> = self.textures.keys().copied().collect();
            for id in texture_ids {
                self.destroy_texture(TextureId(id));
            }
            let target_ids: Vec<u64> = self.targets.keys().copied().collect();
            for id in target_ids {
                self.destroy_target(TargetId(id));
            }
            for mut buffer in [self.staging.take(), self.readback.take()]
                .into_iter()
                .flatten()
            {
                self.gpu.device.destroy_buffer(buffer.buffer, None);
                if let Some(allocation) = buffer.allocation.take() {
                    let _ = self.allocator.free(allocation);
                }
            }

            let device = &self.gpu.device;
            for (_, pipeline) in self.encode_pipelines.drain() {
                device.destroy_pipeline(pipeline, None);
            }
            if !self.surface_pipeline.is_null() {
                device.destroy_pipeline(self.surface_pipeline, None);
            }
            if !self.surface_layout.is_null() {
                device.destroy_pipeline_layout(self.surface_layout, None);
            }
            if !self.encode_layout.is_null() {
                device.destroy_pipeline_layout(self.encode_layout, None);
            }
            for pool in self.descriptor_pools.drain(..) {
                device.destroy_descriptor_pool(pool, None);
            }
            if !self.descriptor_set_layout.is_null() {
                device.destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            }
            if !self.sampler.is_null() {
                device.destroy_sampler(self.sampler, None);
            }
            if !self.fence.is_null() {
                device.destroy_fence(self.fence, None);
            }
            if !self.command_pool.is_null() {
                device.destroy_command_pool(self.command_pool, None);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PipelineKind {
    Surface,
    Encode(OutputFormat),
}

#[derive(Clone, Copy)]
enum BufferRole {
    Staging,
    Readback,
}

fn check_size(width: u32, height: u32) -> Result<(), RenderError> {
    // 16384 is comfortably inside maxImageDimension2D of the devices the
    // prototype targets, and keeps size arithmetic far from overflow.
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        Err(RenderError::InvalidSize { width, height })
    } else {
        Ok(())
    }
}

fn alloc_error(e: vulkan_alloc::AllocationError) -> RenderError {
    RenderError::Allocation(format!("{e:?}"))
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

/// Records an image layout transition.
///
/// # Safety
///
/// `cb` must be in the recording state and `image` must be alive.
#[allow(clippy::too_many_arguments)]
unsafe fn image_barrier(
    device: &vulkan::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        image_barrier_transfer(
            device, cb, image, old_layout, new_layout, src_stage, src_access, dst_stage,
            dst_access, None,
        )
    }
}

/// Like [`image_barrier`], optionally transferring the image between queue
/// families (`(from, to)`).
///
/// # Safety
///
/// `cb` must be in the recording state and `image` must be alive.
#[allow(clippy::too_many_arguments)]
unsafe fn image_barrier_transfer(
    device: &vulkan::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
    families: Option<(u32, u32)>,
) {
    let (src_family, dst_family) =
        families.unwrap_or((vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED));
    let barriers = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(src_family)
        .dst_queue_family_index(dst_family)
        .image(image)
        .subresource_range(color_range())];
    let dependency = vk::DependencyInfo::default().image_memory_barriers(&barriers);
    // SAFETY: guaranteed by the caller.
    unsafe { device.cmd_pipeline_barrier2(cb, &dependency) };
}

/// # Safety
///
/// `device` must be valid and `bytes` must be SPIR-V.
unsafe fn create_shader_module(
    device: &vulkan::Device,
    bytes: &[u8],
) -> Result<vk::ShaderModule, RenderError> {
    let code: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    // SAFETY: guaranteed by the caller.
    Ok(unsafe { device.create_shader_module(&info, None) }?)
}
