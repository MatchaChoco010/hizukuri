//! Color math for hizukuri.
//!
//! This crate is pure computation: no Wayland, no GPU. It defines
//!
//! - [`ImageDescription`]: what the numbers in a buffer mean (primaries,
//!   transfer function, luminances, mastering information),
//! - [`Primaries`] and the 3x3 matrices that convert between them,
//! - [`TransferFunction`]: the EOTFs and their inverses,
//! - [`DecodeTransform`] / [`EncodeTransform`]: the two halves of the
//!   compositing pipeline (content → compositing space → output).
//!
//! The renderer consumes the same [`DecodeTransform`] and
//! [`EncodeTransform`] values that the CPU reference implementation in this
//! crate evaluates, so GPU results can be compared against CPU results
//! numerically.

mod description;
mod math;
mod pipeline;
mod primaries;
mod transfer;

pub use description::{AlphaMode, ImageDescription, Luminance, MasteringDisplay};
pub use math::Mat3;
pub use pipeline::{
    CompositeSpace, DecodeTransform, EncodeTransform, ToneMapping, blend_over, quantize_unorm,
};
pub use primaries::{Chromaticity, Primaries};
pub use transfer::TransferFunction;
