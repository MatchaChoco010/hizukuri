//! The formats this spike exposes, and what the API needs to know about them.

use vulkan::vk;

/// Numeric class of a format as a shader sees it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum NumClass {
    Float,
    Sint,
    Uint,
}

macro_rules! formats {
    ($($variant:ident, $name:literal, $vk:ident, $bytes:literal, $class:ident;)*) => {
        /// Image formats. A small, fixed set: enough for the spike.
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        pub enum Format {
            $($variant,)*
        }

        impl Format {
            pub const ALL: &'static [Format] = &[$(Format::$variant,)*];

            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }

            pub fn vk(self) -> vk::Format {
                match self {
                    $(Self::$variant => vk::Format::$vk,)*
                }
            }

            pub fn bytes_per_pixel(self) -> u32 {
                match self {
                    $(Self::$variant => $bytes,)*
                }
            }

            pub fn class(self) -> NumClass {
                match self {
                    $(Self::$variant => NumClass::$class,)*
                }
            }
        }
    };
}

formats! {
    R8Unorm, "r8unorm", R8_UNORM, 1, Float;
    Rg8Unorm, "rg8unorm", R8G8_UNORM, 2, Float;
    Rgba8Unorm, "rgba8unorm", R8G8B8A8_UNORM, 4, Float;
    Rgba8Srgb, "rgba8unorm-srgb", R8G8B8A8_SRGB, 4, Float;
    Bgra8Unorm, "bgra8unorm", B8G8R8A8_UNORM, 4, Float;
    Bgra8Srgb, "bgra8unorm-srgb", B8G8R8A8_SRGB, 4, Float;
    Rgb10a2Unorm, "rgb10a2unorm", A2B10G10R10_UNORM_PACK32, 4, Float;
    R16Float, "r16float", R16_SFLOAT, 2, Float;
    Rg16Float, "rg16float", R16G16_SFLOAT, 4, Float;
    Rgba16Float, "rgba16float", R16G16B16A16_SFLOAT, 8, Float;
    Rgba16Unorm, "rgba16unorm", R16G16B16A16_UNORM, 8, Float;
    R32Float, "r32float", R32_SFLOAT, 4, Float;
    Rg32Float, "rg32float", R32G32_SFLOAT, 8, Float;
    Rgba32Float, "rgba32float", R32G32B32A32_SFLOAT, 16, Float;
    Rg11b10Float, "rg11b10float", B10G11R11_UFLOAT_PACK32, 4, Float;
    R32Uint, "r32uint", R32_UINT, 4, Uint;
    Rgba8Uint, "rgba8uint", R8G8B8A8_UINT, 4, Uint;
    D32Float, "d32float", D32_SFLOAT, 4, Float;
}

impl Format {
    /// Whether the format holds depth instead of colour.
    pub fn is_depth(self) -> bool {
        matches!(self, Self::D32Float)
    }

    pub(crate) fn aspect(self) -> vk::ImageAspectFlags {
        if self.is_depth() {
            vk::ImageAspectFlags::DEPTH
        } else {
            vk::ImageAspectFlags::COLOR
        }
    }

    /// Index into the per-device table of [`FormatCaps`].
    pub(crate) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|f| *f == self)
            .expect("format is in ALL")
    }
}

/// What the device can do with a format (optimal tiling).
#[derive(Copy, Clone, Debug, Default)]
pub struct FormatCaps {
    pub sampled: bool,
    pub filter_linear: bool,
    pub color_attachment: bool,
    pub blend: bool,
    pub depth_attachment: bool,
    /// Can be read through a comparison sampler.
    pub depth_compare: bool,
    pub storage: bool,
    pub transfer_src: bool,
    pub transfer_dst: bool,
    /// Source and destination of a scaled copy (`generate_mipmaps`).
    pub blit_src: bool,
    pub blit_dst: bool,
}

impl FormatCaps {
    pub(crate) fn from_features(features: vk::FormatFeatureFlags) -> Self {
        use vk::FormatFeatureFlags as F;
        Self {
            sampled: features.contains(F::SAMPLED_IMAGE),
            filter_linear: features.contains(F::SAMPLED_IMAGE_FILTER_LINEAR),
            color_attachment: features.contains(F::COLOR_ATTACHMENT),
            blend: features.contains(F::COLOR_ATTACHMENT_BLEND),
            depth_attachment: features.contains(F::DEPTH_STENCIL_ATTACHMENT),
            // Not among these flags; the device setup fills it in.
            depth_compare: false,
            storage: features.contains(F::STORAGE_IMAGE),
            transfer_src: features.contains(F::TRANSFER_SRC),
            transfer_dst: features.contains(F::TRANSFER_DST),
            blit_src: features.contains(F::BLIT_SRC),
            blit_dst: features.contains(F::BLIT_DST),
        }
    }
}

macro_rules! vertex_formats {
    ($($variant:ident, $name:literal, $vk:ident, $bytes:literal, $class:ident, $components:literal;)*) => {
        /// Vertex attribute formats.
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        pub enum VertexFormat {
            $($variant,)*
        }

        impl VertexFormat {
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }

            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            pub fn vk(self) -> vk::Format {
                match self {
                    $(Self::$variant => vk::Format::$vk,)*
                }
            }

            pub fn size(self) -> u32 {
                match self {
                    $(Self::$variant => $bytes,)*
                }
            }

            pub fn class(self) -> NumClass {
                match self {
                    $(Self::$variant => NumClass::$class,)*
                }
            }

            pub fn components(self) -> u32 {
                match self {
                    $(Self::$variant => $components,)*
                }
            }
        }
    };
}

vertex_formats! {
    F32, "f32", R32_SFLOAT, 4, Float, 1;
    F32x2, "f32x2", R32G32_SFLOAT, 8, Float, 2;
    F32x3, "f32x3", R32G32B32_SFLOAT, 12, Float, 3;
    F32x4, "f32x4", R32G32B32A32_SFLOAT, 16, Float, 4;
    U8x4Unorm, "u8x4unorm", R8G8B8A8_UNORM, 4, Float, 4;
    U32, "u32", R32_UINT, 4, Uint, 1;
    I32, "i32", R32_SINT, 4, Sint, 1;
}
