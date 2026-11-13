//! What a backend needs from whatever draws the frames: places to draw into
//! that it can show.
//!
//! A backend asks a [`TargetProvider`] for render targets of the size and
//! format an output needs: offscreen ones, whose frames it reads back
//! ([`TargetProvider::read_encoded_into`]), or ones exported as dma-bufs for
//! scanout. The backend defines this interface; whatever draws implements
//! it (the script layer's drawing, and the KMS self test's pattern).

use std::os::fd::OwnedFd;

use proto_color::{CompositeSpace, ImageDescription};

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
}

/// DRM fourcc of `XRGB8888`.
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

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

/// Handle of a render target. The provider numbers its targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetId(u64);

impl TargetId {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Errors of a [`TargetProvider`].
#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error("the device cannot use format {0} the way the provider needs")]
    UnsupportedFormat(&'static str),
    #[error("unknown render target")]
    UnknownTarget,
    #[error("the render target has not been drawn into yet")]
    TargetNotRendered,
    #[error(
        "the device cannot export a render target as a dma-buf with a format modifier the \
         consumer accepts"
    )]
    ScanoutUnsupported,
    /// Anything else that went wrong in the provider.
    #[error("{0}")]
    Provider(String),
}

/// See the module documentation.
pub trait TargetProvider {
    /// Creates an offscreen target. Frames are taken out of it with
    /// [`Self::read_encoded_into`].
    fn create_target(&mut self, desc: &TargetDesc) -> Result<TargetId, TargetError>;

    /// The single-plane DRM format modifiers a scanout target can have.
    /// Empty when the provider cannot export dma-bufs.
    fn scanout_modifiers(&self) -> Vec<u64>;

    /// Creates a target whose image is exported as a dma-buf for scanout,
    /// with one of `allowed_modifiers`. The format must be
    /// [`OutputFormat::Bgra8Unorm`] (DRM `XRGB8888`).
    fn create_scanout_target(
        &mut self,
        desc: &TargetDesc,
        allowed_modifiers: &[u64],
    ) -> Result<(TargetId, DmabufPlane), TargetError>;

    fn destroy_target(&mut self, id: TargetId);

    /// Copies the encoded image of the last frame drawn into a target to
    /// caller-provided memory, `stride` bytes per row.
    fn read_encoded_into(
        &mut self,
        target: TargetId,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), TargetError>;
}

/// A borrowed, optional [`TargetProvider`], as backends receive it. `None`
/// when the compositor runs without anything that draws.
///
/// The object type is `'static` so that the borrow can be shortened and
/// passed on (`as_deref_mut`), which a `&mut dyn Trait` with an elided
/// object lifetime does not allow.
pub type Targets<'a> = Option<&'a mut (dyn TargetProvider + 'static)>;
