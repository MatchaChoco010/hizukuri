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
