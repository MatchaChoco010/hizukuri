//! Shaders handed to the API as SPIR-V (`Gpu::create_shader_spirv`): the
//! module is not trusted. Indices out of range land in range, modules the API
//! cannot vouch for are refused, and damaged modules come back as errors.
//!
//! The modules are written here by naga with no bounds checks, or assembled
//! from text, so whatever is clamped or refused, the API did.

use proto_gfx_core::*;
use spirv_tools::assembler::Assembler;

mod common;

use common::{ShaderLanguage, ShaderSource};

fn wgsl_spirv(stage: ShaderStage, source: &str, entry: &str) -> Vec<u8> {
    common::naga_spirv(&ShaderSource {
        stage,
        language: ShaderLanguage::Wgsl,
        source,
        entry,
    })
    .unwrap_or_else(|e| panic!("naga: {e}"))
}

fn assemble(text: &str) -> Vec<u8> {
    spirv_tools::assembler::create(Some(spirv_tools::TargetEnv::Vulkan_1_4))
        .assemble(text, Default::default())
        .unwrap_or_else(|e| panic!("assembler: {e}"))
        .as_words()
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect()
}

const FULLSCREEN_VS: &str = r#"
@vertex fn main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(p[i], 0.0, 1.0);
}
"#;

/// Draws a full-screen triangle with `fs` and returns the first pixel.
fn draw_one(gpu: &mut Gpu, fs: &str, push: u32, setup: &mut dyn FnMut(&mut Gpu)) -> Vec<u8> {
    let vs = gpu
        .create_shader_spirv(
            ShaderStage::Vertex,
            "main",
            &wgsl_spirv(ShaderStage::Vertex, FULLSCREEN_VS, "main"),
        )
        .unwrap();
    let fs = gpu
        .create_shader_spirv(
            ShaderStage::Fragment,
            "main",
            &wgsl_spirv(ShaderStage::Fragment, fs, "main"),
        )
        .unwrap();
    let pipeline = gpu
        .create_render_pipeline(&RenderPipelineDesc {
            vertex: vs,
            fragment: fs,
            vertex_buffers: vec![],
            targets: vec![ColorTarget {
                format: Format::Rgba8Unorm,
                blend: None,
            }],
            topology: Topology::TriangleList,
            cull: CullMode::None,
        })
        .unwrap();
    let target = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 2,
            height: 2,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        })
        .unwrap();
    gpu.begin_frame().unwrap();
    gpu.begin_pass(&[ColorAttachment {
        image: target,
        load: LoadOp::Clear([0.0; 4]),
        store: true,
    }])
    .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    setup(gpu);
    gpu.push(0, &push.to_ne_bytes()).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    let pixels = gpu.read_image(target).unwrap();
    for handle in [target, pipeline, fs, vs] {
        gpu.destroy(handle).unwrap();
    }
    pixels[..4].to_vec()
}

/// Indices from push data, far out of range, into a local array, a uniform
/// array, a storage buffer and a texture array: the API clamps each to the
/// range (an index read as a negative number goes to the first element), and
/// the validation layer stays silent.
#[test]
fn out_of_range_indices_land_in_range() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).unwrap();
    let far = 1_000_000;

    let local = r#"
        struct P { i: u32 }
        var<immediate> pc: P;
        @fragment fn main() -> @location(0) vec4<f32> {
            var c = array<vec4<f32>, 3>(vec4(1.0, 0.0, 0.0, 1.0), vec4(0.0, 1.0, 0.0, 1.0), vec4(0.0, 0.0, 1.0, 1.0));
            return c[pc.i];
        }"#;
    assert_eq!(draw_one(&mut gpu, local, 1, &mut |_| {}), [0, 255, 0, 255]);
    assert_eq!(
        draw_one(&mut gpu, local, far, &mut |_| {}),
        [0, 0, 255, 255]
    );
    assert_eq!(
        draw_one(&mut gpu, local, u32::MAX, &mut |_| {}),
        [255, 0, 0, 255]
    );

    // Eight colours: red k/7, blue 1.
    let buffer = gpu
        .create_buffer(&BufferDesc {
            size: 256,
            usage: BufferUsage::UNIFORM | BufferUsage::STORAGE,
        })
        .unwrap();
    let colors: Vec<u8> = (0..8)
        .flat_map(|k| [k as f32 / 7.0, 0.0, 1.0, 1.0])
        .flat_map(f32::to_ne_bytes)
        .collect();
    gpu.write_buffer(buffer, 0, &colors).unwrap();
    let uniform = gpu.create_uniform_slot(buffer, 0, 128).unwrap();
    let px = draw_one(
        &mut gpu,
        r#"
        struct P { i: u32 }
        var<immediate> pc: P;
        struct U { colors: array<vec4<f32>, 8> }
        @group(0) @binding(0) var<uniform> u: U;
        @fragment fn main() -> @location(0) vec4<f32> { return u.colors[pc.i]; }"#,
        far,
        &mut |gpu| gpu.bind(0, uniform).unwrap(),
    );
    assert_eq!(px, [255, 0, 255, 255], "uniform array");

    // The slot covers 4 of the 8 colours: the index is clamped to the bound
    // range, to colour 3.
    let storage = gpu.create_storage_buffer_slot(buffer, 0, 64).unwrap();
    let px = draw_one(
        &mut gpu,
        r#"
        struct P { i: u32 }
        var<immediate> pc: P;
        @group(0) @binding(0) var<storage, read> data: array<vec4<f32>>;
        @fragment fn main() -> @location(0) vec4<f32> { return data[pc.i]; }"#,
        far,
        &mut |gpu| gpu.bind(0, storage).unwrap(),
    );
    assert!(
        px[0].abs_diff(109) <= 1 && px[1..] == [0, 255, 255],
        "storage buffer: {px:?}"
    );

    let images: Vec<Handle> = (0..4u8)
        .map(|k| {
            let image = gpu
                .create_image(&ImageDesc {
                    format: Format::Rgba8Unorm,
                    width: 1,
                    height: 1,
                    usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
                })
                .unwrap();
            gpu.begin_frame().unwrap();
            gpu.upload_image(image, &[k * 80, 10, 20, 255]).unwrap();
            gpu.end_frame().unwrap();
            image
        })
        .collect();
    let run = gpu.create_texture_slot_run(&images).unwrap();
    let px = draw_one(
        &mut gpu,
        r#"
        enable wgpu_binding_array;
        struct P { i: u32 }
        var<immediate> pc: P;
        @group(0) @binding(0) var texs: binding_array<texture_2d<f32>, 4>;
        @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(texs[pc.i], vec2(0), 0); }"#,
        far,
        &mut |gpu| gpu.bind(0, run[0]).unwrap(),
    );
    assert_eq!(px, [240, 10, 20, 255], "texture array");
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}

const PLAIN_FS: &str = r#"
@fragment fn main() -> @location(0) vec4<f32> { return vec4(1.0); }
"#;

/// A fragment shader in SPIR-V assembly, with extra capabilities and an
/// addressing model, to build modules the WGSL front end cannot write.
fn plain_fs_asm(header: &str, addressing: &str) -> String {
    format!(
        "OpCapability Shader
        {header}
        OpMemoryModel {addressing} GLSL450
        OpEntryPoint Fragment %main \"main\" %out
        OpExecutionMode %main OriginUpperLeft
        OpDecorate %out Location 0
        %void = OpTypeVoid
        %fn = OpTypeFunction %void
        %float = OpTypeFloat 32
        %v4 = OpTypeVector %float 4
        %ptr_out = OpTypePointer Output %v4
        %out = OpVariable %ptr_out Output
        %one = OpConstant %float 1
        %c = OpConstantComposite %v4 %one %one %one %one
        %main = OpFunction %void None %fn
        %l = OpLabel
        OpStore %out %c
        OpReturn
        OpFunctionEnd"
    )
}

/// What the API refuses, and the message it gives. Every refusal is an
/// error value; nothing reaches the device.
#[test]
fn modules_the_api_cannot_vouch_for_are_refused() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).unwrap();
    let valid = wgsl_spirv(ShaderStage::Fragment, PLAIN_FS, "main");
    gpu.create_shader_spirv(ShaderStage::Fragment, "main", &valid)
        .unwrap();
    let swapped: Vec<u8> = valid
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|w| [w[3], w[2], w[1], w[0]])
        .collect();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("empty", vec![], "whole number"),
        ("not a whole number of words", valid[..valid.len() - 1].to_vec(), "whole number"),
        ("not SPIR-V", b"@fragment fn main() {}..........".to_vec(), "magic"),
        ("big-endian", swapped, "big-endian"),
        ("truncated", valid[..valid.len() / 2].to_vec(), "invalid SPIR-V"),
        (
            "an unbounded texture array",
            wgsl_spirv(
                ShaderStage::Fragment,
                "enable wgpu_binding_array;
                 @group(0) @binding(0) var t: binding_array<texture_2d<f32>>;
                 @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(t[0], vec2(0), 0); }",
                "main",
            ),
            "OpTypeRuntimeArray",
        ),
        (
            "64-bit floats (a feature the device does not enable)",
            assemble(&plain_fs_asm("OpCapability Float64", "Logical")),
            "Float64",
        ),
        (
            "a second descriptor set",
            wgsl_spirv(
                ShaderStage::Fragment,
                "@group(1) @binding(0) var t: texture_2d<f32>;
                 @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(t, vec2(0), 0); }",
                "main",
            ),
            "group/set 0",
        ),
        (
            "a binding beyond the table",
            wgsl_spirv(
                ShaderStage::Fragment,
                "@group(0) @binding(32) var t: texture_2d<f32>;
                 @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(t, vec2(0), 0); }",
                "main",
            ),
            "out of range",
        ),
        (
            "a 1D texture",
            wgsl_spirv(
                ShaderStage::Fragment,
                "@group(0) @binding(0) var t: texture_1d<f32>;
                 @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(t, 0, 0); }",
                "main",
            ),
            "2D",
        ),
        (
            "device addresses",
            assemble(&plain_fs_asm(
                "OpCapability PhysicalStorageBufferAddresses
                 OpExtension \"SPV_KHR_physical_storage_buffer\"",
                "PhysicalStorageBuffer64",
            )),
            "",
        ),
        (
            "variable pointers",
            assemble(&plain_fs_asm(
                "OpCapability VariablePointers
                 OpExtension \"SPV_KHR_variable_pointers\"",
                "Logical",
            )),
            "",
        ),
        (
            "an extension the API does not know",
            assemble(&plain_fs_asm(
                "OpExtension \"SPV_KHR_shader_clock\"",
                "Logical",
            )),
            "SPV_KHR_shader_clock",
        ),
    ];
    for (name, spirv, expected) in cases {
        let e = match gpu.create_shader_spirv(ShaderStage::Fragment, "main", &spirv) {
            Ok(_) => panic!("{name}: accepted"),
            Err(e) => e,
        };
        assert_eq!(e.code, ErrorCode::ShaderCompile, "{name}: {e}");
        assert!(e.message.contains(expected), "{name}: {e}");
    }
    // The entry point asked for is not the module's.
    let e = gpu
        .create_shader_spirv(ShaderStage::Vertex, "main", &valid)
        .err()
        .unwrap();
    assert!(e.message.contains("vertex entry point"), "{e}");
    let e = gpu
        .create_shader_spirv(ShaderStage::Fragment, "other", &valid)
        .err()
        .unwrap();
    assert!(e.message.contains("`other`"), "{e}");
    // The device still works.
    gpu.create_shader_spirv(ShaderStage::Fragment, "main", &valid)
        .unwrap();
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}

/// Replaces each word of a valid module, one at a time, by each value that
/// `values` gives for it. Each mutant is refused, or is accepted and then used
/// to build a pipeline and draw, with the validation layer watching. The
/// process never goes down.
fn mutate(source: &str, values: fn(u32) -> Vec<u32>) {
    let fs = wgsl_spirv(ShaderStage::Fragment, source, "main");
    let words: Vec<u32> = fs
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| u32::from_le_bytes(*w))
        .collect();
    let mut gpu = Gpu::new(GpuConfig::for_tests()).unwrap();
    let vs = gpu
        .create_shader_spirv(
            ShaderStage::Vertex,
            "main",
            &wgsl_spirv(ShaderStage::Vertex, FULLSCREEN_VS, "main"),
        )
        .unwrap();
    let target = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 2,
            height: 2,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        })
        .unwrap();
    // What MUTATED_RESOURCES binds: a texture, a sampler, a uniform buffer.
    let texture = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 1,
            height: 1,
            usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
        })
        .unwrap();
    gpu.begin_frame().unwrap();
    gpu.upload_image(texture, &[200, 100, 50, 255]).unwrap();
    gpu.end_frame().unwrap();
    let uniform = gpu
        .create_buffer(&BufferDesc {
            size: 256,
            usage: BufferUsage::UNIFORM,
        })
        .unwrap();
    gpu.write_buffer(uniform, 0, &[0x3f; 256]).unwrap();
    let slots = [
        gpu.create_texture_slot(texture).unwrap(),
        gpu.create_sampler(&SamplerDesc {
            mag: Filter::Linear,
            min: Filter::Linear,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        })
        .unwrap(),
        gpu.create_uniform_slot(uniform, 0, 256).unwrap(),
    ];
    let (mut refused, mut accepted, mut drawn) = (0, 0, 0);
    // The header (5 words) is left alone: it only says which version and
    // generator wrote the module.
    for at in 5..words.len() {
        for value in values(words[at]) {
            if value == words[at] {
                continue;
            }
            let mut mutant = words.clone();
            mutant[at] = value;
            let bytes: Vec<u8> = mutant.iter().flat_map(|w| w.to_le_bytes()).collect();
            let Ok(shader) = gpu.create_shader_spirv(ShaderStage::Fragment, "main", &bytes) else {
                refused += 1;
                continue;
            };
            accepted += 1;
            if let Ok(pipeline) = gpu.create_render_pipeline(&RenderPipelineDesc {
                vertex: vs,
                fragment: shader,
                vertex_buffers: vec![],
                targets: vec![ColorTarget {
                    format: Format::Rgba8Unorm,
                    blend: None,
                }],
                topology: Topology::TriangleList,
                cull: CullMode::None,
            }) {
                let push = gpu.shader_reflection(shader).unwrap().push_size;
                gpu.begin_frame().unwrap();
                gpu.begin_pass(&[ColorAttachment {
                    image: target,
                    load: LoadOp::Clear([0.0; 4]),
                    store: true,
                }])
                .unwrap();
                gpu.set_pipeline(pipeline).unwrap();
                let data: Vec<u8> = (0..push / 4).flat_map(|_| 7u32.to_ne_bytes()).collect();
                gpu.push(0, &data).unwrap();
                for (binding, slot) in slots.iter().enumerate() {
                    gpu.bind(binding as u32, *slot).unwrap();
                }
                // A draw the API refuses (a mutant that wants other bindings)
                // still leaves the frame usable.
                if gpu.draw(3, 1, 0, 0).is_ok() {
                    drawn += 1;
                }
                gpu.end_pass().unwrap();
                gpu.end_frame().unwrap();
                gpu.destroy(pipeline).unwrap();
            }
            gpu.destroy(shader).unwrap();
        }
    }
    gpu.wait_idle().unwrap();
    eprintln!(
        "{} words: {refused} mutants refused, {accepted} accepted, {drawn} drawn",
        words.len()
    );
    assert!(refused > 0 && accepted > 0);
    assert_eq!(validation_error_count(), 0, "{:#?}", validation_messages());
}

/// A local array indexed with push data.
const MUTATED_LOCAL: &str = r#"
struct P { i: u32 }
var<immediate> pc: P;
@fragment fn main() -> @location(0) vec4<f32> {
    var c = array<vec4<f32>, 3>(vec4(1.0, 0.0, 0.0, 1.0), vec4(0.0, 1.0, 0.0, 1.0), vec4(0.0, 0.0, 1.0, 1.0));
    return c[pc.i];
}
"#;

/// A texture, a sampler, a uniform buffer and push data.
const MUTATED_RESOURCES: &str = r#"
struct P { tint: vec4<f32>, i: u32 }
var<immediate> pc: P;
struct U { colors: array<vec4<f32>, 4>, m: mat3x3<f32> }
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
@group(0) @binding(2) var<uniform> u: U;
@fragment fn main(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureSample(t, s, p.xy / 2.0) * u.colors[pc.i];
    return vec4(u.m * c.rgb, c.a) * pc.tint;
}
"#;

fn six_values(word: u32) -> Vec<u32> {
    vec![
        0,
        1,
        0xffff_ffff,
        word ^ 1,
        word.wrapping_add(0x1_0000),
        1_000_000,
    ]
}

fn every_bit(word: u32) -> Vec<u32> {
    (0..32)
        .map(|bit| word ^ (1 << bit))
        .chain(0..16)
        .chain([0xffff_ffff, 0x8000_0000, 0x7fff_ffff])
        .collect()
}

/// Six values per word: zero, one, all ones, a flipped low bit, a changed
/// word count (when the word starts an instruction), a large number.
#[test]
fn mutated_modules_are_refused_or_valid() {
    mutate(MUTATED_LOCAL, six_values);
    mutate(MUTATED_RESOURCES, six_values);
}

/// Every single-bit flip of every word, and the small numbers: about forty
/// times as many mutants. Not part of the checks; run it with
///
///     cargo test -p proto-gfx-core --test spirv_input -- --ignored every_bit
#[test]
#[ignore]
fn mutated_modules_are_refused_or_valid_every_bit() {
    mutate(MUTATED_LOCAL, every_bit);
    mutate(MUTATED_RESOURCES, every_bit);
}
