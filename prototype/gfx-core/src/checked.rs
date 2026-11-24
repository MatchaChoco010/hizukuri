//! Checking a SPIR-V module without the device.
//!
//! The checks of [`crate::spirv_input`] take tens of milliseconds on a large
//! module and touch nothing of the device. A [`ShaderChecker`] runs them on
//! any thread; the [`CheckedShader`] it returns is what [`crate::Gpu`] makes
//! a shader from, in no time. A host that learns of a new shader while it is
//! drawing (a colour transform that was just chosen, a shader that was just
//! rebuilt) checks it on another thread and keeps its frames.
//!
//! A `CheckedShader` cannot be made any other way: its fields are private,
//! so having one means the module passed the checks.

use std::sync::Arc;

use rspirv::spirv::Capability;

use crate::error::{ErrorCode, GfxError, Result, bail};
use crate::gpu::{MAX_SPIRV_BYTES, run_shader_tool};
use crate::shader::{Reflection, ShaderStage};

/// Runs the checks of the SPIR-V entry point for one kind of device: the
/// capabilities a module may use depend on the features the device has.
///
/// Cheap to clone and free to send to other threads. Checks may run at the
/// same time on several threads: each one works on its own copy of the
/// module and its own SPIRV-Tools context.
#[derive(Clone, Debug)]
pub struct ShaderChecker {
    optional: Arc<[Capability]>,
}

impl ShaderChecker {
    /// A checker for what every device the API runs on has: a module it
    /// accepts is accepted by every [`crate::Gpu`]. For tools that have no
    /// device.
    pub fn baseline() -> Self {
        Self::new(Vec::new())
    }

    pub(crate) fn new(optional: Vec<Capability>) -> Self {
        Self {
            optional: optional.into(),
        }
    }

    /// Validates, hardens and checks a module (the bytes of a `.spv` file)
    /// whose one entry point is `entry`, of `stage`. Blocks until done.
    pub fn check(&self, stage: ShaderStage, entry: &str, spirv: &[u8]) -> Result<CheckedShader> {
        if spirv.len() > MAX_SPIRV_BYTES {
            bail!(
                ShaderCompile,
                "the SPIR-V module is {} bytes; the limit is {MAX_SPIRV_BYTES}",
                spirv.len()
            );
        }
        if entry.contains('\0') {
            bail!(ShaderCompile, "entry point name contains NUL");
        }
        let words = crate::spirv_input::words_from_bytes(spirv)
            .map_err(|message| GfxError::new(ErrorCode::ShaderCompile, message))?;
        let compiled = run_shader_tool("the SPIR-V checks", || {
            crate::spirv_input::accept_spirv_with(&words, stage, entry, &self.optional)
        })?;
        Ok(CheckedShader {
            spirv: compiled.spirv,
            entry: compiled.entry,
            reflection: compiled.reflection,
            optional: Arc::clone(&self.optional),
        })
    }
}

/// A SPIR-V module that passed the checks of a [`ShaderChecker`]: the
/// hardened module and its interface. [`crate::Gpu::create_shader_checked`]
/// makes a shader from it without looking at the module again.
///
/// It stays valid for as long as it exists, also across the loss and
/// recreation of the device, and can be cloned to create the shader more
/// than once.
#[derive(Clone, Debug)]
pub struct CheckedShader {
    pub(crate) spirv: Vec<u32>,
    pub(crate) entry: String,
    pub(crate) reflection: Reflection,
    /// The optional capabilities the checker allowed.
    pub(crate) optional: Arc<[Capability]>,
}

impl CheckedShader {
    pub fn stage(&self) -> ShaderStage {
        self.reflection.stage
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }

    /// The interface the checks read from the module.
    pub fn reflection(&self) -> &Reflection {
        &self.reflection
    }

    /// Whether a device that allows `allowed` may run the module: the checker
    /// that accepted it must not have allowed more.
    pub(crate) fn fits(&self, allowed: &[Capability]) -> bool {
        self.optional.iter().all(|c| allowed.contains(c))
    }
}
