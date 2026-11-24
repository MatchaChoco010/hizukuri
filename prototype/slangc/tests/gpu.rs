//! What `slangc` writes, through `proto-gfx-core`'s SPIR-V entry point, on
//! lavapipe: the reflection the GPU API builds from Slang's SPIR-V, texture
//! arrays, `discard`, modules, and the compiler's errors and time limit.
//!
//! Needs `slangc` (the Nix dev shell provides it).

use std::path::{Path, PathBuf};
use std::time::Duration;

use proto_gfx_core::*;
use proto_slangc::{Compiler, Error, Input, Stage};

fn shaders() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/shaders")
}

fn compiler() -> Compiler {
    let mut compiler = Compiler::find().unwrap_or_else(|e| panic!("{e}"));
    compiler.include_dirs.push(shaders());
    compiler
}

fn stage(stage: ShaderStage) -> Stage {
    match stage {
        ShaderStage::Vertex => Stage::Vertex,
        ShaderStage::Fragment => Stage::Fragment,
        ShaderStage::Compute => Stage::Compute,
    }
}

fn spirv(input: Input<'_>, shader_stage: ShaderStage, entry: &str) -> Vec<u8> {
    compiler()
        .compile(input, stage(shader_stage), entry)
        .unwrap_or_else(|e| panic!("{e}"))
        .spirv
}

fn create(gpu: &mut Gpu, input: Input<'_>, shader_stage: ShaderStage, entry: &str) -> Handle {
    let spirv = spirv(input, shader_stage, entry);
    gpu.create_shader_spirv(shader_stage, entry, &spirv)
        .unwrap_or_else(|e| panic!("{entry}: {e}"))
}

fn quad() -> PathBuf {
    shaders().join("quad.slang")
}

/// The reflection of Slang's SPIR-V is the one the same shader gets in WGSL
/// through naga.
#[test]
fn reflection_of_slang_spirv() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).unwrap();
    let fs = create(&mut gpu, Input::File(&quad()), ShaderStage::Fragment, "fs");
    let r = gpu.shader_reflection(fs).unwrap();
    assert_eq!(r.push_size, 16);
    let bindings: Vec<_> = r
        .bindings
        .iter()
        .map(|b| (b.binding, b.name.as_str(), b.kind))
        .collect();
    // The unused texture at binding 4 is not in Slang's SPIR-V at all.
    assert_eq!(
        bindings,
        [
            (
                0,
                "tex",
                BindingKind::SampledTexture {
                    class: NumClass::Float
                }
            ),
            (1, "smp", BindingKind::Sampler),
            (2, "globals", BindingKind::UniformBuffer { min_size: 16 }),
        ]
    );
    assert!(r.bindings.iter().all(|b| b.used));
    assert_eq!(r.sampling_pairs, vec![(0, 1)]);
    assert_eq!(
        r.inputs,
        vec![IoVar {
            location: 0,
            class: NumClass::Float,
            components: 2
        }]
    );
    let vs = create(&mut gpu, Input::File(&quad()), ShaderStage::Vertex, "vs");
    let r = gpu.shader_reflection(vs).unwrap();
    assert_eq!(r.inputs.len(), 1);
    assert_eq!(r.outputs.len(), 1);

    let cs = create(
        &mut gpu,
        Input::Source(
            r#"
            [[vk::binding(0, 0)]] [format("rgba8")] RWTexture2D<float4> image;
            [[vk::binding(1, 0)]] StructuredBuffer<float4> colors;
            [[vk::binding(2, 0)]] RWStructuredBuffer<uint> counts;
            [numthreads(8, 4, 1)]
            void main(uint3 id : SV_DispatchThreadID) {
                image[id.xy] = colors[id.x % 4];
                InterlockedAdd(counts[0], 1);
            }"#,
        ),
        ShaderStage::Compute,
        "main",
    );
    let r = gpu.shader_reflection(cs).unwrap();
    assert_eq!(r.workgroup_size, [8, 4, 1]);
    let kinds: Vec<_> = r.bindings.iter().map(|b| b.kind).collect();
    assert_eq!(
        kinds,
        [
            BindingKind::StorageTexture {
                format: Format::Rgba8Unorm,
                access: StorageAccess::ReadWrite
            },
            BindingKind::StorageBuffer {
                min_size: 16,
                writable: false
            },
            BindingKind::StorageBuffer {
                min_size: 4,
                writable: true
            },
        ]
    );
    gpu.create_compute_pipeline(cs).unwrap();
}

const FULLSCREEN_VS: &str = r#"
float4 main(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}
"#;

/// Draws a full-screen triangle and returns the first pixel.
fn draw_one(gpu: &mut Gpu, fs: &str, push: u32, setup: &mut dyn FnMut(&mut Gpu)) -> Vec<u8> {
    let vs = create(
        gpu,
        Input::Source(FULLSCREEN_VS),
        ShaderStage::Vertex,
        "main",
    );
    let fs = create(gpu, Input::Source(fs), ShaderStage::Fragment, "main");
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
    let size = gpu.shader_reflection(fs).unwrap().push_size;
    if size >= 4 {
        gpu.push(0, &push.to_ne_bytes()).unwrap();
    }
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    gpu.read_image(target).unwrap()[..4].to_vec()
}

/// A textured quad with a uniform buffer and push data, as Slang writes it.
#[test]
fn a_textured_quad_draws() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).unwrap();
    let vs = create(&mut gpu, Input::File(&quad()), ShaderStage::Vertex, "vs");
    let fs = create(&mut gpu, Input::File(&quad()), ShaderStage::Fragment, "fs");
    let pipeline = gpu
        .create_render_pipeline(&RenderPipelineDesc {
            vertex: vs,
            fragment: fs,
            vertex_buffers: vec![VertexBufferLayout {
                stride: 8,
                per_instance: false,
                attributes: vec![VertexAttribute {
                    location: 0,
                    format: VertexFormat::F32x2,
                    offset: 0,
                }],
            }],
            targets: vec![ColorTarget {
                format: Format::Rgba8Unorm,
                blend: None,
            }],
            topology: Topology::TriangleList,
            cull: CullMode::None,
        })
        .unwrap();
    let floats = |v: &[f32]| v.iter().flat_map(|x| x.to_ne_bytes()).collect::<Vec<u8>>();
    let vertices = gpu
        .create_buffer(&BufferDesc {
            size: 24,
            usage: BufferUsage::VERTEX,
        })
        .unwrap();
    gpu.write_buffer(vertices, 0, &floats(&[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0]))
        .unwrap();
    let uniform = gpu
        .create_buffer(&BufferDesc {
            size: 256,
            usage: BufferUsage::UNIFORM,
        })
        .unwrap();
    gpu.write_buffer(uniform, 0, &floats(&[1.0, 1.0, 0.5, 1.0]))
        .unwrap();
    let uniform_slot = gpu.create_uniform_slot(uniform, 0, 16).unwrap();
    let texture = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 1,
            height: 1,
            usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
        })
        .unwrap();
    let slot = gpu.create_texture_slot(texture).unwrap();
    let sampler = gpu
        .create_sampler(&SamplerDesc {
            mag: Filter::Linear,
            min: Filter::Linear,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        })
        .unwrap();
    let target = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 4,
            height: 4,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        })
        .unwrap();
    gpu.begin_frame().unwrap();
    gpu.upload_image(texture, &[255, 200, 100, 255]).unwrap();
    gpu.begin_pass(&[ColorAttachment {
        image: target,
        load: LoadOp::Clear([0.0; 4]),
        store: true,
    }])
    .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    gpu.set_vertex_buffer(0, vertices, 0).unwrap();
    gpu.bind(0, slot).unwrap();
    gpu.bind(1, sampler).unwrap();
    gpu.bind(2, uniform_slot).unwrap();
    gpu.push(0, &floats(&[1.0, 0.5, 1.0, 1.0])).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    let px = gpu.read_image(target).unwrap();
    // 255 x 1 x 1, 200 x 0.5 x 1, 100 x 1 x 0.5; give or take 1 (UNORM).
    for (c, expected) in [255u8, 100, 50, 255].into_iter().enumerate() {
        assert!(px[c].abs_diff(expected) <= 1, "{:?}", &px[..4]);
    }
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}

/// A texture array indexed with push data, far out of range, uniform and
/// non-uniform: the element is the last one. `discard` (in SPIR-V 1.6,
/// OpDemoteToHelperInvocation) leaves the target as it was.
#[test]
fn texture_arrays_and_discard() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).unwrap();
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
    let first = gpu.create_texture_slot_run(&images).unwrap()[0];
    for (index, name) in [
        ("pc.i", "uniform"),
        ("NonUniformResourceIndex(pc.i)", "non-uniform"),
    ] {
        let source = format!(
            "struct P {{ uint i; }};
             [[vk::push_constant]] ConstantBuffer<P> pc;
             [[vk::binding(0, 0)]] Texture2D<float4> texs[4];
             float4 main() : SV_Target {{ return texs[{index}].Load(int3(0, 0, 0)); }}"
        );
        let px = draw_one(&mut gpu, &source, 1, &mut |gpu| gpu.bind(0, first).unwrap());
        assert_eq!(px, [80, 10, 20, 255], "{name}, index 1");
        let px = draw_one(&mut gpu, &source, 1_000_000, &mut |gpu| {
            gpu.bind(0, first).unwrap()
        });
        assert_eq!(px, [240, 10, 20, 255], "{name}, index 1000000");
    }
    let discard = r#"
        struct P { uint mode; };
        [[vk::push_constant]] ConstantBuffer<P> pc;
        float4 main() : SV_Target {
            if (pc.mode == 2) discard;
            return float4(1, 1, 1, 1);
        }"#;
    assert_eq!(draw_one(&mut gpu, discard, 2, &mut |_| {}), [0, 0, 0, 0]);
    assert_eq!(draw_one(&mut gpu, discard, 1, &mut |_| {}), [255; 4]);
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}

/// A module found through the include directories, used from source given
/// on standard input. What the module keeps internal cannot be reached.
#[test]
fn modules() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).unwrap();
    let px = draw_one(
        &mut gpu,
        "import tint;
         float4 main() : SV_Target { Tinted t = Tinted(float4(0, 1, 0.5, 1)); return t.apply(float4(1)); }",
        0,
        &mut |_| {},
    );
    assert!(
        px[0] == 0 && px[1] == 255 && px[2].abs_diff(128) <= 1 && px[3] == 255,
        "{px:?}"
    );
    let e = compiler()
        .compile(
            Input::Source(
                "import tint;
                 float4 main() : SV_Target { Tinted t = Tinted(float4(1)); return t.factor; }",
            ),
            Stage::Fragment,
            "main",
        )
        .unwrap_err();
    let text = e.to_string();
    assert!(text.contains("<stdin>:2:"), "{text}");
    assert!(text.contains("not accessible"), "{text}");
    // Without the include directory, there is no module to import.
    let mut alone = compiler();
    alone.include_dirs.clear();
    let e = alone
        .compile(
            Input::Source("import tint;\nfloat4 main() : SV_Target { return 1; }"),
            Stage::Fragment,
            "main",
        )
        .unwrap_err();
    assert!(e.to_string().contains("tint"), "{e}");
}

/// Errors come back with their place, for the author.
#[test]
fn errors_have_a_line_and_a_column() {
    let c = compiler();
    let e = c
        .compile(
            Input::Source("float4 main() : SV_Target {\n    return undefined_name;\n}\n"),
            Stage::Fragment,
            "main",
        )
        .unwrap_err();
    assert!(matches!(e, Error::Failed(_)), "{e:?}");
    assert!(
        e.to_string()
            .starts_with("<stdin>:2:12: error E30015: undefined identifier"),
        "{e}"
    );
    // A file is named by its path.
    let e = c
        .compile(Input::File(&quad()), Stage::Fragment, "nope")
        .unwrap_err();
    assert!(e.to_string().contains("quad.slang:"), "{e}");
    assert!(e.to_string().contains("nope"), "{e}");
}

/// A source the compiler does not finish in time is stopped. 100,000 nested
/// blocks take slangc 2026.21 more than two minutes.
#[test]
fn a_compile_that_does_not_finish_is_stopped() {
    let mut c = compiler();
    c.timeout = Duration::from_secs(1);
    let source = format!(
        "float4 main() : SV_Target {{ {} {} return 1; }}",
        "{".repeat(100_000),
        "}".repeat(100_000)
    );
    let started = std::time::Instant::now();
    let e = c
        .compile(Input::Source(&source), Stage::Fragment, "main")
        .unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e:?}");
    assert!(started.elapsed() >= Duration::from_secs(1));
}

/// A `slangc` that is not there is an error, not a panic.
#[test]
fn a_missing_compiler_is_an_error() {
    let c = Compiler::new(PathBuf::from("/nonexistent/slangc"));
    let e = c
        .compile(Input::Source(FULLSCREEN_VS), Stage::Vertex, "main")
        .unwrap_err();
    assert!(matches!(e, Error::Io(_)), "{e:?}");
}
