//! Shared by the tests that write their shaders in Slang: compiling with
//! `slangc` (the Nix dev shell provides it), and small helpers.

#![allow(dead_code)]

use proto_gfx_core::*;
use proto_slangc::{Compiler, Input, Stage};

/// Compiles one entry point of `source` and hands the SPIR-V to the API.
pub fn try_shader(gpu: &mut Gpu, stage: ShaderStage, entry: &str, source: &str) -> Result<Handle> {
    let compiler = Compiler::find().unwrap_or_else(|e| panic!("{e}"));
    let slang_stage = match stage {
        ShaderStage::Vertex => Stage::Vertex,
        ShaderStage::Fragment => Stage::Fragment,
        ShaderStage::Compute => Stage::Compute,
    };
    let spirv = compiler
        .compile(Input::Source(source), slang_stage, entry)
        .unwrap_or_else(|e| panic!("{entry}: {e}"))
        .spirv;
    gpu.create_shader_spirv(stage, entry, &spirv)
}

pub fn shader(gpu: &mut Gpu, stage: ShaderStage, entry: &str, source: &str) -> Handle {
    try_shader(gpu, stage, entry, source).unwrap_or_else(|e| panic!("{entry}: {e}"))
}

pub fn gpu() -> Gpu {
    Gpu::new(GpuConfig::for_tests()).unwrap()
}

pub fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_ne_bytes()).collect()
}

pub fn f32_at(bytes: &[u8], index: usize) -> f32 {
    f32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
}

/// How far an 8-bit UNORM channel written from a float may be from the
/// round-to-nearest code value. Vulkan converts a float `f` to unsigned
/// normalized fixed-point by taking either of the two code values closest to
/// `f × 255`: rounding to nearest is recommended, not required, and only a
/// product that is exactly an integer must come out unchanged. `0.5` (127.5)
/// may be written as 127 or 128.
pub const UNORM8_CONVERSION: u8 = 1;

pub fn assert_close8(actual: [u8; 4], expected: [u8; 4], tolerance: u8, what: &str) {
    for c in 0..4 {
        assert!(
            actual[c].abs_diff(expected[c]) <= tolerance,
            "{what}: channel {c}: got {actual:?}, expected {expected:?} (tolerance {tolerance})"
        );
    }
}

pub fn image(gpu: &mut Gpu, format: Format, width: u32, height: u32, usage: ImageUsage) -> Handle {
    gpu.create_image(&ImageDesc {
        format,
        width,
        height,
        usage,
    })
    .unwrap()
}

/// A pipeline without vertex buffers.
pub fn pipeline(
    gpu: &mut Gpu,
    vs: Handle,
    fs: Handle,
    targets: &[Format],
    extra: &PipelineExtra,
) -> Result<Handle> {
    gpu.create_render_pipeline_with(
        &RenderPipelineDesc {
            vertex: vs,
            fragment: fs,
            vertex_buffers: vec![],
            targets: targets
                .iter()
                .map(|format| ColorTarget {
                    format: *format,
                    blend: None,
                })
                .collect(),
            topology: Topology::TriangleList,
            cull: CullMode::None,
        },
        extra,
    )
}

pub fn color(image: Handle, clear: [f32; 4]) -> PassColor {
    PassColor {
        image,
        sub: Subresource::default(),
        load: LoadOp::Clear(clear),
        store: true,
        resolve: None,
    }
}

pub fn depth(image: Handle, clear: f32) -> PassDepth {
    PassDepth {
        image,
        sub: Subresource::default(),
        load: DepthLoadOp::Clear(clear),
        store: true,
    }
}

pub fn no_validation_errors() {
    assert_eq!(
        validation_error_count(),
        0,
        "validation layer messages: {:#?}",
        validation_messages()
    );
}

#[macro_export]
macro_rules! expect {
    ($result:expr, $code:ident) => {{
        match $result {
            Ok(_) => panic!("expected {:?}, but the call succeeded", ErrorCode::$code),
            Err(e) => {
                assert_eq!(e.code, ErrorCode::$code, "unexpected error: {e}");
                assert!(!e.message.is_empty());
                e
            }
        }
    }};
}
