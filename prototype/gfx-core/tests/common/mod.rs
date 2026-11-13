//! Shared by the tests of `proto-gfx-core`: shaders that a test gives as
//! short WGSL or GLSL text. naga turns the text into SPIR-V here, in the
//! test, with none of the API's checks and no bounds checks, and the module
//! goes to `Gpu::create_shader_spirv`. Whatever is refused or clamped, the
//! API did.

#![allow(dead_code)]

use proto_gfx_core::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShaderLanguage {
    Wgsl,
    Glsl,
}

/// A shader as a test writes it.
pub struct ShaderSource<'a> {
    pub stage: ShaderStage,
    pub language: ShaderLanguage,
    pub source: &'a str,
    /// Entry point name. GLSL always uses `main`.
    pub entry: &'a str,
}

/// Writes the SPIR-V of `source` and creates the shader from it. What naga
/// refuses comes back as a `ShaderCompile` error, as the build tool's would
/// stop the shader before the API sees it.
pub fn create_shader(gpu: &mut Gpu, source: &ShaderSource<'_>) -> Result<Handle> {
    let spirv =
        naga_spirv(source).map_err(|message| GfxError::new(ErrorCode::ShaderCompile, message))?;
    gpu.create_shader_spirv(source.stage, source.entry, &spirv)
}

/// WGSL or GLSL to SPIR-V with naga alone. When the module has no entry point
/// of `source.entry` and `source.stage`, its first entry point is written
/// instead, so that the API sees, and has to refuse, a module that does not
/// match what it is asked for. naga runs on a large stack and its panics are
/// caught.
pub fn naga_spirv(source: &ShaderSource<'_>) -> std::result::Result<Vec<u8>, String> {
    let owned = (
        source.stage,
        source.language,
        source.source.to_owned(),
        source.entry.to_owned(),
    );
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || {
            let (stage, language, text, entry) = owned;
            naga_spirv_inner(stage, language, &text, &entry)
        })
        .unwrap()
        .join()
        .unwrap_or_else(|panic| {
            let detail = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            Err(format!("naga panicked: {detail}"))
        })
}

fn naga_spirv_inner(
    stage: ShaderStage,
    language: ShaderLanguage,
    text: &str,
    entry: &str,
) -> std::result::Result<Vec<u8>, String> {
    let naga_stage = match stage {
        ShaderStage::Vertex => naga::ShaderStage::Vertex,
        ShaderStage::Fragment => naga::ShaderStage::Fragment,
        ShaderStage::Compute => naga::ShaderStage::Compute,
    };
    let module = match language {
        ShaderLanguage::Wgsl => {
            naga::front::wgsl::parse_str(text).map_err(|e| e.emit_to_string(text))?
        }
        ShaderLanguage::Glsl => naga::front::glsl::Frontend::default()
            .parse(&naga::front::glsl::Options::from(naga_stage), text)
            .map_err(|e| e.emit_to_string(text))?,
    };
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| e.emit_to_string(text))?;
    let (shader_stage, entry_point) = module
        .entry_points
        .iter()
        .find(|e| e.name == entry && e.stage == naga_stage)
        .or_else(|| module.entry_points.first())
        .map(|e| (e.stage, e.name.clone()))
        .ok_or("the module has no entry point")?;
    use naga::back::spv;
    let policy = naga::proc::BoundsCheckPolicy::Unchecked;
    let options = spv::Options {
        lang_version: (1, 4),
        // Names, as a compiler writes them by default; the API's messages
        // use them.
        flags: spv::WriterFlags::LABEL_VARYINGS | spv::WriterFlags::DEBUG,
        bounds_check_policies: naga::proc::BoundsCheckPolicies {
            index: policy,
            buffer: policy,
            image_load: policy,
            binding_array: policy,
        },
        ..Default::default()
    };
    let words = spv::write_vec(
        &module,
        &info,
        &options,
        Some(&spv::PipelineOptions {
            shader_stage,
            entry_point,
        }),
    )
    .map_err(|e| format!("SPIR-V generation failed: {e}"))?;
    Ok(words.iter().flat_map(|w| w.to_le_bytes()).collect())
}
