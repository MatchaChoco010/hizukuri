//! Vulkan renderer of the verification compositor.
//!
//! The renderer imports client buffers as textures, blends them in a linear
//! light floating point buffer (the compositing space), and encodes the
//! result for an output. Every texture and every render target carries a
//! [`proto_color::ImageDescription`]; nothing here assumes sRGB.
//!
//! ```text
//! textures (encoded, any image description)
//!    └─ surface pass ─▶ compositing buffer (RGBA16F, linear, premultiplied)
//!                          └─ encode pass ─▶ output image (encoded for the target)
//! ```
//!
//! Both the compositing buffer and the encoded output can be read back, so
//! results can be checked numerically against the CPU reference in
//! `proto-color`.
//!
//! This crate does not depend on Smithay. Together with `proto-backend`
//! it is the only crate that uses `vulkan-rs` directly.

mod device;
mod image;
mod renderer;

pub use device::{DeviceInfo, DeviceKind, DeviceSelector, find_icd_manifest};
pub use image::{CompositeImage, EncodedImage, ImageIoError, half_to_f32};
pub use renderer::{
    Background, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DmabufPlane, Element, Frame,
    OutputFormat, PixelFormat, Rect, Renderer, RendererOptions, TargetDesc, TargetId,
    TargetProvider, Targets, TextureDesc, TextureId, targets_of,
};

/// Errors of the renderer.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("failed to load the Vulkan loader: {0}")]
    Loader(String),
    #[error("Vulkan call failed: {0}")]
    Vulkan(#[from] vulkan::vk::VkResult),
    #[error("Vulkan memory allocation failed: {0}")]
    Allocation(String),
    #[error(
        "the Vulkan validation layer was requested but VK_LAYER_KHRONOS_validation or \
         VK_EXT_debug_utils is not available"
    )]
    ValidationUnavailable,
    #[error(
        "no suitable Vulkan device for selector {selector:?} (Vulkan 1.3 with dynamic rendering \
         is required). Devices visible to the loader: {available:?}. The set of drivers the \
         loader sees can be changed with VK_DRIVER_FILES."
    )]
    NoSuitableDevice {
        selector: DeviceSelector,
        available: Vec<String>,
    },
    #[error("the device cannot use format {0} the way the renderer needs")]
    UnsupportedFormat(&'static str),
    #[error("unknown texture")]
    UnknownTexture,
    #[error("unknown render target")]
    UnknownTarget,
    #[error("the render target has not been rendered to yet")]
    TargetNotRendered,
    #[error("invalid size {width}x{height}")]
    InvalidSize { width: u32, height: u32 },
    #[error("pixel data is too short: {actual} bytes, {expected} needed")]
    DataTooShort { actual: usize, expected: usize },
    #[error("invalid stride {stride} for width {width}")]
    InvalidStride { stride: u32, width: u32 },
    #[error("the primaries of an image description are degenerate")]
    DegeneratePrimaries,
    #[error(
        "the device cannot export a render target as a dma-buf with a format modifier the \
         consumer accepts"
    )]
    ScanoutUnsupported,
    /// An error of a [`TargetProvider`] that is not this renderer.
    #[error("{0}")]
    Provider(String),
}
