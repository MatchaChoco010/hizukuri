//! A checked GPU API for scripts, on Vulkan 1.4 with `VK_EXT_descriptor_heap`.
//!
//! This is spike code for hizukuri. It answers one question:
//! how low-level can the GPU API shown to scripts be while a script mistake
//! still comes back as an error value instead of undefined behaviour?
//!
//! The crate knows nothing about any script language. `proto-gfx-lua` binds it.

mod desc;
mod device;
mod dmabuf;
mod error;
mod format;
mod gpu;
mod handle;
mod host;
mod record;
mod shader;

pub use desc::*;
pub use device::{validation_error_count, validation_messages};
pub use dmabuf::{DmabufDesc, DmabufFormat, DmabufPlaneDesc, ImportedImage};
pub use error::{ErrorCode, GfxError, Result};
pub use format::{Format, FormatCaps, NumClass, VertexFormat};
pub use gpu::{Gpu, ImageInfo, MAX_SHADER_SOURCE_BYTES, Stats};
pub use handle::{Handle, Kind};
pub use host::{DeviceDescription, ScanoutPlane};
pub use shader::{
    BindingInfo, BindingKind, CompiledShader, IoVar, MAX_BINDINGS, MAX_TEXTURE_ARRAY, NagaCompiler,
    Reflection, ShaderCompiler, ShaderLanguage, ShaderSource, ShaderStage, StorageAccess,
};
