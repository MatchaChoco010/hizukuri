//! Shaders handed to the GPU API as ready-made SPIR-V.
//!
//! The module comes from outside the core (a build tool that compiled the
//! script's Slang source), so nothing in it is trusted. Before the device
//! sees it:
//!
//! 1. SPIRV-Tools' validator checks it against the Vulkan 1.4 environment.
//!    A module that is not valid SPIR-V never reaches the driver.
//! 2. SPIRV-Tools' optimizer runs two passes: `strip-nonsemantic-info`
//!    (reflection decorations such as `SPV_GOOGLE_user_type`, which would need
//!    device extensions, and non-semantic instructions) and
//!    `graphics-robust-access`, which clamps every index into an array, a
//!    vector or a matrix to its range, whatever the compiler did. The pass
//!    refuses modules it cannot process (device addresses, variable pointers,
//!    unbounded descriptor arrays).
//! 3. The validator runs again on the result.
//! 4. [`crate::spirv_check`] refuses what the API cannot reason about and
//!    describes the interface. It also checks that every index is clamped,
//!    rather than trusting step 2: the pass leaves some constructs alone and
//!    still reports success (untyped pointers, for one).
//!
//! SPIRV-Tools is C++ running in this process. A crash in it takes the
//! process down; it cannot be caught like a panic.

use spirv_tools::TargetEnv;
use spirv_tools::opt::{Optimizer, Passes};
use spirv_tools::val::Validator;

use crate::shader::{CompiledShader, ShaderStage};

/// The SPIR-V magic number, in the byte order of the module.
const MAGIC: u32 = 0x0723_0203;

/// Turns the bytes of a `.spv` file into words. SPIR-V in the other byte
/// order is refused rather than swapped.
pub(crate) fn words_from_bytes(bytes: &[u8]) -> Result<Vec<u32>, String> {
    if !bytes.len().is_multiple_of(4) || bytes.len() < 20 {
        return Err(format!(
            "a SPIR-V module is a whole number of 32-bit words, at least 5; got {} bytes",
            bytes.len()
        ));
    }
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        .collect();
    match words[0] {
        MAGIC => Ok(words),
        m if m.swap_bytes() == MAGIC => {
            Err("SPIR-V in big-endian byte order is not accepted".into())
        }
        _ => Err("not a SPIR-V module (the magic number is wrong)".into()),
    }
}

fn validate(words: &[u32]) -> Result<(), String> {
    spirv_tools::val::create(Some(TargetEnv::Vulkan_1_4))
        .validate(words, None)
        .map_err(|e| format!("invalid SPIR-V: {e}"))
}

/// [`accept_spirv_with`] for a device without any optional feature.
#[cfg(test)]
pub(crate) fn accept_spirv(
    words: &[u32],
    stage: ShaderStage,
    entry: &str,
) -> Result<CompiledShader, String> {
    accept_spirv_with(words, stage, entry, &[])
}

/// Runs steps 1 to 4 on a module received from outside. The result holds the
/// hardened module, which is what the device gets. `optional` lists the
/// capabilities the device has beyond the ones every device has.
pub(crate) fn accept_spirv_with(
    words: &[u32],
    stage: ShaderStage,
    entry: &str,
    optional: &[rspirv::spirv::Capability],
) -> Result<CompiledShader, String> {
    validate(words)?;
    let mut optimizer = spirv_tools::opt::create(Some(TargetEnv::Vulkan_1_4));
    optimizer
        .register_pass(Passes::StripNonSemanticInfo)
        .register_pass(Passes::GraphicsRobustAccess);
    let mut messages = Vec::new();
    let hardened = optimizer
        .optimize(
            words,
            &mut |m: spirv_tools::error::Message| messages.push(m.message),
            Some(spirv_tools::opt::Options {
                preserve_bindings: true,
                ..Default::default()
            }),
        )
        .map_err(|e| {
            let detail = if messages.is_empty() {
                e.to_string()
            } else {
                messages.join("; ")
            };
            format!("the module cannot be made safe to run: {detail}")
        })?;
    let hardened = hardened.as_words().to_vec();
    validate(&hardened)?;
    let reflection = crate::spirv_check::reflect_spirv_with(&hardened, stage, entry, optional)?;
    Ok(CompiledShader {
        spirv: hardened,
        entry: entry.to_owned(),
        reflection,
    })
}
