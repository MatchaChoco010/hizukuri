//! Shared by the tests of `proto-gfx-core`: creating a shader through either
//! of the API's two entry points, so that one test body checks both.
//!
//! - [`Entry::Source`]: `Gpu::create_shader` with the WGSL or GLSL source.
//! - [`Entry::Spirv`]: naga turns the source into SPIR-V here, in the test,
//!   with none of the API's checks and no bounds checks, and the module goes
//!   to `Gpu::create_shader_spirv`. Whatever the API rejects or clamps, the
//!   SPIR-V entry point did.

#![allow(dead_code)]

use std::cell::Cell;

use proto_gfx_core::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Source,
    Spirv,
}

thread_local! {
    static ENTRY: Cell<Entry> = const { Cell::new(Entry::Source) };
}

/// The entry point the current test uses.
pub fn entry() -> Entry {
    ENTRY.with(Cell::get)
}

/// Runs `body` with shaders created through `entry`.
pub fn run(entry: Entry, body: fn()) {
    ENTRY.with(|e| e.set(entry));
    body();
}

/// Defines, for each named function, a module with two tests: `source` and
/// `spirv`, which run the function through each entry point.
#[macro_export]
macro_rules! through_both_entries {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                #[test]
                fn source() {
                    super::common::run(super::common::Entry::Source, super::$name);
                }
                #[test]
                fn spirv() {
                    super::common::run(super::common::Entry::Spirv, super::$name);
                }
            }
        )*
    };
}

/// Creates a shader through the entry point of the current test.
pub fn create_shader(gpu: &mut Gpu, source: &ShaderSource<'_>) -> Result<Handle> {
    match entry() {
        Entry::Source => gpu.create_shader(source),
        Entry::Spirv => {
            let spirv = naga_spirv(source)
                .map_err(|message| GfxError::new(ErrorCode::ShaderCompile, message))?;
            gpu.create_shader_spirv(source.stage, source.entry, &spirv)
        }
    }
}

/// WGSL or GLSL to SPIR-V with naga alone. When the module has no entry point
/// of `source.entry` and `source.stage`, its first entry point is written
/// instead, so that the API sees, and has to refuse, a module that does not
/// match what it is asked for. naga runs on a large stack and its panics are
/// caught, as in `Gpu::create_shader`.
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
