//! Plain-data descriptions passed to the GPU API.

use crate::format::{Format, VertexFormat};
use crate::handle::Handle;

macro_rules! flags {
    ($(#[$meta:meta])* $name:ident { $($flag:ident = $bit:literal => $text:literal,)* }) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
        pub struct $name(pub u32);

        impl $name {
            $(pub const $flag: Self = Self($bit);)*

            pub fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }

            pub fn is_empty(self) -> bool {
                self.0 == 0
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($text => Some(Self::$flag),)*
                    _ => None,
                }
            }
        }

        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }
    };
}

flags! {
    /// How a buffer may be used.
    BufferUsage {
        VERTEX = 1 => "vertex",
        INDEX = 2 => "index",
        UNIFORM = 4 => "uniform",
        STORAGE = 8 => "storage",
    }
}

flags! {
    /// How an image may be used.
    ImageUsage {
        SAMPLED = 1 => "sampled",
        COLOR_TARGET = 2 => "color_target",
        STORAGE = 4 => "storage",
        COPY_SRC = 8 => "copy_src",
        COPY_DST = 16 => "copy_dst",
        DEPTH_TARGET = 32 => "depth_target",
    }
}

#[derive(Copy, Clone, Debug)]
pub struct BufferDesc {
    pub size: u64,
    pub usage: BufferUsage,
}

#[derive(Copy, Clone, Debug)]
pub struct ImageDesc {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub usage: ImageUsage,
}

/// The dimensionality of an image.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ImageKind {
    /// A 2D image.
    D2,
    /// A 3D image (a volume, such as a colour lookup table).
    D3,
    /// Six square 2D layers that a shader reads as a cube.
    Cube,
}

impl ImageKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "2d" => Some(Self::D2),
            "3d" => Some(Self::D3),
            "cube" => Some(Self::Cube),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::D2 => "2d",
            Self::D3 => "3d",
            Self::Cube => "cube",
        }
    }
}

/// What an image has beyond [`ImageDesc`]. The default is a plain 2D image
/// with one mip level and one sample per texel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ImageExtra {
    pub kind: ImageKind,
    /// Depth of a 3D image in texels; 1 for the other kinds.
    pub depth: u32,
    /// Number of mip levels, from the full size down.
    pub mip_levels: u32,
    /// Samples per texel (1, 2, 4 or 8). More than one makes a multisampled
    /// image, which can only be an attachment.
    pub samples: u32,
}

impl Default for ImageExtra {
    fn default() -> Self {
        Self {
            kind: ImageKind::D2,
            depth: 1,
            mip_levels: 1,
            samples: 1,
        }
    }
}

/// Which part of an image a texture slot shows to a shader.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct TextureView {
    /// First mip level the shader sees (its level 0).
    pub base_mip: u32,
    /// Number of mip levels; `None` for all that remain.
    pub mip_count: Option<u32>,
    /// Shows one layer (a face of a cube image) as a 2D texture.
    pub layer: Option<u32>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    Nearest,
    Linear,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AddressMode {
    ClampToEdge,
    Repeat,
    MirrorRepeat,
    /// Outside the image the sampler returns its border colour.
    ClampToBorder,
}

/// The colour a sampler returns outside the image with
/// [`AddressMode::ClampToBorder`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BorderColor {
    TransparentBlack,
    OpaqueBlack,
    OpaqueWhite,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Never,
    Less,
    Equal,
    LessOrEqual,
    Greater,
    NotEqual,
    GreaterOrEqual,
    Always,
}

impl CompareOp {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "never" => Self::Never,
            "less" => Self::Less,
            "equal" => Self::Equal,
            "less-equal" => Self::LessOrEqual,
            "greater" => Self::Greater,
            "not-equal" => Self::NotEqual,
            "greater-equal" => Self::GreaterOrEqual,
            "always" => Self::Always,
            _ => return None,
        })
    }
}

/// What a sampler has beyond [`SamplerDesc`]. The default reads every mip
/// level of the texture, picking the nearest one.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SamplerExtra {
    /// How two neighbouring mip levels are combined.
    pub mip: Filter,
    pub address_w: AddressMode,
    pub lod_min: f32,
    /// Upper bound of the level of detail. Large values mean "no bound".
    pub lod_max: f32,
    pub lod_bias: f32,
    /// Largest anisotropy ratio; 1 turns anisotropic filtering off.
    pub anisotropy: f32,
    pub border: BorderColor,
    /// Makes a comparison sampler (shadow maps): a read compares a reference
    /// value with the depth in the texture and returns how much of it
    /// passes. Such a sampler can only be used for comparisons, on depth
    /// textures.
    pub compare: Option<CompareOp>,
}

impl Default for SamplerExtra {
    fn default() -> Self {
        Self {
            mip: Filter::Nearest,
            address_w: AddressMode::ClampToEdge,
            lod_min: 0.0,
            lod_max: 1000.0,
            lod_bias: 0.0,
            anisotropy: 1.0,
            border: BorderColor::TransparentBlack,
            compare: None,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SamplerDesc {
    pub mag: Filter,
    pub min: Filter,
    pub address_u: AddressMode,
    pub address_v: AddressMode,
}

#[derive(Copy, Clone, Debug)]
pub struct VertexAttribute {
    pub location: u32,
    pub format: VertexFormat,
    pub offset: u32,
}

#[derive(Clone, Debug)]
pub struct VertexBufferLayout {
    pub stride: u32,
    pub per_instance: bool,
    pub attributes: Vec<VertexAttribute>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlendFactor {
    Zero,
    One,
    SrcColor,
    OneMinusSrcColor,
    DstColor,
    OneMinusDstColor,
    SrcAlpha,
    OneMinusSrcAlpha,
    DstAlpha,
    OneMinusDstAlpha,
}

impl BlendFactor {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "zero" => Self::Zero,
            "one" => Self::One,
            "src" => Self::SrcColor,
            "one-minus-src" => Self::OneMinusSrcColor,
            "dst" => Self::DstColor,
            "one-minus-dst" => Self::OneMinusDstColor,
            "src-alpha" => Self::SrcAlpha,
            "one-minus-src-alpha" => Self::OneMinusSrcAlpha,
            "dst-alpha" => Self::DstAlpha,
            "one-minus-dst-alpha" => Self::OneMinusDstAlpha,
            _ => return None,
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlendOp {
    Add,
    Subtract,
    ReverseSubtract,
    Min,
    Max,
}

impl BlendOp {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "add" => Self::Add,
            "subtract" => Self::Subtract,
            "reverse-subtract" => Self::ReverseSubtract,
            "min" => Self::Min,
            "max" => Self::Max,
            _ => return None,
        })
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Blend {
    pub src_color: BlendFactor,
    pub dst_color: BlendFactor,
    pub color_op: BlendOp,
    pub src_alpha: BlendFactor,
    pub dst_alpha: BlendFactor,
    pub alpha_op: BlendOp,
}

impl Blend {
    /// Source-over for premultiplied alpha.
    pub const PREMULTIPLIED: Self = Self {
        src_color: BlendFactor::One,
        dst_color: BlendFactor::OneMinusSrcAlpha,
        color_op: BlendOp::Add,
        src_alpha: BlendFactor::One,
        dst_alpha: BlendFactor::OneMinusSrcAlpha,
        alpha_op: BlendOp::Add,
    };

    pub const ADDITIVE: Self = Self {
        src_color: BlendFactor::One,
        dst_color: BlendFactor::One,
        color_op: BlendOp::Add,
        src_alpha: BlendFactor::One,
        dst_alpha: BlendFactor::One,
        alpha_op: BlendOp::Add,
    };
}

#[derive(Copy, Clone, Debug)]
pub struct ColorTarget {
    pub format: Format,
    pub blend: Option<Blend>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Topology {
    TriangleList,
    TriangleStrip,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CullMode {
    None,
    Front,
    Back,
}

#[derive(Clone, Debug)]
pub struct RenderPipelineDesc {
    pub vertex: Handle,
    pub fragment: Handle,
    pub vertex_buffers: Vec<VertexBufferLayout>,
    pub targets: Vec<ColorTarget>,
    pub topology: Topology,
    pub cull: CullMode,
}

/// Which winding order is the front of a triangle, as the triangle appears
/// on the screen.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrontFace {
    CounterClockwise,
    Clockwise,
}

/// An offset added to the depth of every fragment (shadow maps).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DepthBias {
    pub constant: f32,
    pub slope: f32,
    /// Bound of the total offset; 0 for none.
    pub clamp: f32,
}

/// The depth attachment a pipeline draws with, and what it does with it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DepthState {
    pub format: Format,
    /// Discard fragments that fail `compare` against the stored depth.
    pub test: bool,
    /// Store the depth of the fragments that pass.
    pub write: bool,
    pub compare: CompareOp,
    pub bias: Option<DepthBias>,
}

/// What a render pipeline has beyond [`RenderPipelineDesc`]. The default is
/// no depth attachment, counter-clockwise fronts and one sample.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PipelineExtra {
    pub depth: Option<DepthState>,
    pub front_face: FrontFace,
    /// Samples per pixel of the attachments the pipeline draws to.
    pub samples: u32,
}

impl Default for PipelineExtra {
    fn default() -> Self {
        Self {
            depth: None,
            front_face: FrontFace::CounterClockwise,
            samples: 1,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum LoadOp {
    Clear([f32; 4]),
    Load,
    DontCare,
}

#[derive(Copy, Clone, Debug)]
pub struct ColorAttachment {
    pub image: Handle,
    pub load: LoadOp,
    pub store: bool,
}

/// One mip level of one layer of an image.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Subresource {
    pub mip: u32,
    pub layer: u32,
}

/// A colour attachment of a pass: one mip level of one layer of an image.
#[derive(Copy, Clone, Debug)]
pub struct PassColor {
    pub image: Handle,
    pub sub: Subresource,
    pub load: LoadOp,
    pub store: bool,
    /// For a multisampled attachment: the single-sampled image that receives
    /// the resolved result when the pass ends.
    pub resolve: Option<(Handle, Subresource)>,
}

impl From<ColorAttachment> for PassColor {
    fn from(a: ColorAttachment) -> Self {
        Self {
            image: a.image,
            sub: Subresource::default(),
            load: a.load,
            store: a.store,
            resolve: None,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum DepthLoadOp {
    Clear(f32),
    Load,
    DontCare,
}

/// The depth attachment of a pass.
#[derive(Copy, Clone, Debug)]
pub struct PassDepth {
    pub image: Handle,
    pub sub: Subresource,
    pub load: DepthLoadOp,
    pub store: bool,
}

/// The attachments of a pass. A pass needs at least one of them.
#[derive(Clone, Debug, Default)]
pub struct PassDesc {
    pub color: Vec<PassColor>,
    pub depth: Option<PassDepth>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IndexType {
    U16,
    U32,
}

impl IndexType {
    pub fn size(self) -> u64 {
        match self {
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }
}

/// Limits enforced by the API, independent of what the driver would allow.
#[derive(Copy, Clone, Debug)]
pub struct Limits {
    pub max_buffers: u32,
    pub max_images: u32,
    pub max_shaders: u32,
    pub max_pipelines: u32,
    /// Slots of the resource descriptor heap.
    pub resource_slots: u32,
    /// Slots of the sampler descriptor heap.
    pub sampler_slots: u32,
    /// Budget for buffer and image memory, in bytes.
    pub max_memory_bytes: u64,
    /// Upper bound of `vertices * instances` (or `indices * instances`) of one
    /// draw. A policy limit: Vulkan itself accepts 2^32 - 1 for each.
    pub max_draw_elements: u64,
    /// Upper bound of `x * y * z` workgroups of one dispatch. A policy limit:
    /// some drivers accept 2^32 - 1 per dimension.
    pub max_dispatch_groups: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_buffers: 4096,
            max_images: 4096,
            max_shaders: 1024,
            max_pipelines: 1024,
            resource_slots: 4096,
            sampler_slots: 256,
            max_memory_bytes: 1 << 30,
            max_draw_elements: 1 << 26,
            max_dispatch_groups: 1 << 22,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GpuConfig {
    /// Enable `VK_LAYER_KHRONOS_validation`. For development only: the API
    /// must stop every misuse before the layer could see it.
    pub validation: bool,
    /// Case-insensitive substring of the device name to pick.
    pub device_filter: Option<String>,
    /// Prefer the device behind this DRM primary node (major, minor): the
    /// GPU that drives the display. Only a preference; any device that meets
    /// the requirements is taken when none matches.
    pub drm_node: Option<(u32, u32)>,
    pub limits: Limits,
    /// How long to wait for the GPU before the device is treated as lost.
    pub gpu_timeout: std::time::Duration,
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            validation: false,
            device_filter: None,
            drm_node: None,
            limits: Limits::default(),
            gpu_timeout: std::time::Duration::from_secs(10),
        }
    }
}

impl GpuConfig {
    /// Reads `GFX_SPIKE_VALIDATION=1` and `GFX_SPIKE_DEVICE=<substring>`.
    pub fn from_env() -> Self {
        Self {
            validation: std::env::var("GFX_SPIKE_VALIDATION").is_ok_and(|v| v == "1"),
            device_filter: std::env::var("GFX_SPIKE_DEVICE")
                .ok()
                .filter(|v| !v.is_empty()),
            ..Self::default()
        }
    }

    /// The configuration the test suites create their device with.
    ///
    /// Like [`Self::from_env`], except for the choice of the device: unless
    /// `GFX_SPIKE_DEVICE` names one, or `HIZUKURI_TEST_DEVICES` lists `gpu`,
    /// the tests run on Mesa's software rasterizer (llvmpipe), so that they
    /// do the same on every machine.
    pub fn for_tests() -> Self {
        let mut config = Self::from_env();
        let hardware = std::env::var("HIZUKURI_TEST_DEVICES")
            .is_ok_and(|list| list.split(',').any(|device| device.trim() == "gpu"));
        if config.device_filter.is_none() && !hardware {
            config.device_filter = Some("llvmpipe".to_string());
        }
        config
    }
}
