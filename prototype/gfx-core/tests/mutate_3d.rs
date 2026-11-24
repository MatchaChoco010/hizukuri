//! Damaged copies of a shader that uses what 3D drawing added (a cube, a
//! 3D texture, a comparison sampler on a depth texture, explicit levels of
//! detail, 16-bit floats, 64-bit integers): each one is refused, or is
//! accepted and draws, with the validation layer watching. The process
//! never goes down. The shader is Slang; needs `slangc`.

use std::collections::VecDeque;

use proto_gfx_core::*;
use proto_slangc::{Compiler, Input, Stage};

mod slang;
use slang::*;

const SHADER: &str = r#"
struct Push { float4 tint; uint index; };
[[vk::push_constant]] ConstantBuffer<Push> pc;
[[vk::binding(0, 0)]] TextureCube<float4> environment;
[[vk::binding(1, 0)]] SamplerState smp;
[[vk::binding(2, 0)]] Texture2D<float> shadow_map;
[[vk::binding(3, 0)]] SamplerComparisonState shadow_sampler;
[[vk::binding(4, 0)]] Texture3D<float4> lut;
[[vk::binding(5, 0)]] Texture2D<float4> levels;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}

[shader("fragment")]
float4 fs(float4 p : SV_Position) : SV_Target0 {
    float2 uv = p.xy / 2.0;
    float4 env = environment.SampleLevel(smp, float3(uv - 0.5, 1.0), 0);
    float lit = shadow_map.SampleCmpLevelZero(shadow_sampler, uv, 0.5);
    float4 graded = lut.SampleLevel(smp, env.rgb, 0);
    float4 detail = levels.SampleLevel(smp, uv, float(pc.index));
    half h = half(lit) * 0.5h;
    int64_t big = int64_t(pc.index) << 33;
    float weights[3] = { 0.25, 0.5, 1.0 };
    return (graded + detail) * pc.tint * float(h) * weights[pc.index] + float(big >> 33);
}
"#;

fn every_bit(word: u32) -> Vec<u32> {
    (0..32)
        .map(|bit| word ^ (1 << bit))
        .chain(0..16)
        .chain([0xffff_ffff, 0x8000_0000, 0x7fff_ffff])
        .collect()
}

fn words_of(source: &str, stage: Stage, entry: &str) -> Vec<u32> {
    Compiler::find()
        .unwrap()
        .compile(Input::Source(source), stage, entry)
        .unwrap()
        .spirv
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| u32::from_le_bytes(*w))
        .collect()
}

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// The mutants handed to `draw` last. When the test fails on the way (the
/// device is lost, the driver gives an error), they are printed: a frame is
/// waited for only frames later, so the mutant that did it is one of these.
struct LastDrawn {
    entry: String,
    /// The word, its value in the valid module and in the mutant.
    mutants: VecDeque<(usize, u32, u32)>,
}

impl Drop for LastDrawn {
    fn drop(&mut self) {
        if std::thread::panicking() {
            for (at, was, value) in &self.mutants {
                eprintln!(
                    "{}: drawn shortly before the failure: word {at} {was:#010x} replaced by {value:#010x}",
                    self.entry
                );
            }
        }
    }
}

/// Replaces each word of a valid module, one at a time, by each value of
/// `every_bit`. Each mutant is refused, or is accepted and handed to `draw`,
/// which builds a pipeline with it and draws (and says whether the draw was
/// recorded).
fn mutate(
    gpu: &mut Gpu,
    words: &[u32],
    stage: ShaderStage,
    entry: &str,
    draw: &mut dyn FnMut(&mut Gpu, Handle) -> bool,
) {
    let original = gpu
        .create_shader_spirv(stage, entry, &bytes(words))
        .unwrap();
    assert!(draw(gpu, original), "the unchanged module draws");
    let (mut refused, mut accepted, mut drawn) = (0, 0, 0);
    let mut last = LastDrawn {
        entry: entry.to_owned(),
        mutants: VecDeque::new(),
    };
    // The header (5 words) is left alone: it only says which version and
    // generator wrote the module.
    for at in 5..words.len() {
        for value in every_bit(words[at]) {
            if value == words[at] {
                continue;
            }
            let mut mutant = words.to_vec();
            mutant[at] = value;
            let Ok(shader) = gpu.create_shader_spirv(stage, entry, &bytes(&mutant)) else {
                refused += 1;
                continue;
            };
            accepted += 1;
            if last.mutants.len() == 3 {
                last.mutants.pop_front();
            }
            last.mutants.push_back((at, words[at], value));
            if draw(gpu, shader) {
                drawn += 1;
            }
            gpu.destroy(shader).unwrap();
        }
    }
    gpu.wait_idle().unwrap();
    eprintln!(
        "{entry}, {} words: {refused} mutants refused, {accepted} accepted, {drawn} drawn",
        words.len()
    );
    assert!(refused > 0 && accepted > 0 && drawn > 0);
}

#[test]
fn mutants_of_a_3d_shader_are_refused_or_valid() {
    let mut gpu = gpu();
    let vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHADER);
    let words = words_of(SHADER, Stage::Fragment, "fs");

    // What the shader binds.
    let with = |gpu: &mut Gpu, format, size, usage, extra: ImageExtra| {
        gpu.create_image_with(
            &ImageDesc {
                format,
                width: size,
                height: size,
                usage,
            },
            &extra,
        )
        .unwrap()
    };
    let sampled = ImageUsage::SAMPLED | ImageUsage::COPY_DST;
    let cube = with(
        &mut gpu,
        Format::Rgba8Unorm,
        1,
        sampled,
        ImageExtra {
            kind: ImageKind::Cube,
            ..ImageExtra::default()
        },
    );
    let volume = with(
        &mut gpu,
        Format::Rgba8Unorm,
        2,
        sampled,
        ImageExtra {
            kind: ImageKind::D3,
            depth: 2,
            ..ImageExtra::default()
        },
    );
    let mipped = with(
        &mut gpu,
        Format::Rgba8Unorm,
        4,
        sampled | ImageUsage::COPY_SRC,
        ImageExtra {
            mip_levels: 3,
            ..ImageExtra::default()
        },
    );
    let shadow = image(
        &mut gpu,
        Format::D32Float,
        2,
        2,
        ImageUsage::DEPTH_TARGET | ImageUsage::SAMPLED,
    );
    gpu.begin_frame().unwrap();
    for layer in 0..6 {
        gpu.upload_image_level(cube, Subresource { mip: 0, layer }, &[200, 100, 50, 255])
            .unwrap();
    }
    gpu.upload_image(volume, &[128; 2 * 2 * 2 * 4]).unwrap();
    gpu.upload_image(mipped, &[90; 4 * 4 * 4]).unwrap();
    gpu.generate_mipmaps(mipped).unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![],
        depth: Some(depth(shadow, 0.75)),
    })
    .unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    let sampler = |gpu: &mut Gpu, compare| {
        gpu.create_sampler_with(
            &SamplerDesc {
                mag: Filter::Linear,
                min: Filter::Linear,
                address_u: AddressMode::Repeat,
                address_v: AddressMode::ClampToEdge,
            },
            &SamplerExtra {
                mip: Filter::Linear,
                compare,
                ..SamplerExtra::default()
            },
        )
        .unwrap()
    };
    let slots = [
        gpu.create_texture_slot(cube).unwrap(),
        sampler(&mut gpu, None),
        gpu.create_texture_slot(shadow).unwrap(),
        sampler(&mut gpu, Some(CompareOp::LessOrEqual)),
        gpu.create_texture_slot(volume).unwrap(),
        gpu.create_texture_slot(mipped).unwrap(),
    ];
    let target = image(
        &mut gpu,
        Format::Rgba8Unorm,
        2,
        2,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );

    let mut draw = |gpu: &mut Gpu, fs: Handle| -> bool {
        let Ok(pipeline) = pipeline(
            gpu,
            vs,
            fs,
            &[Format::Rgba8Unorm],
            &PipelineExtra::default(),
        ) else {
            return false;
        };
        let push = gpu.shader_reflection(fs).unwrap().push_size;
        gpu.begin_frame().unwrap();
        gpu.begin_pass_desc(&PassDesc {
            color: vec![color(target, [0.0; 4])],
            depth: None,
        })
        .unwrap();
        gpu.set_pipeline(pipeline).unwrap();
        let data: Vec<u8> = (0..push / 4).flat_map(|_| 7u32.to_ne_bytes()).collect();
        gpu.push(0, &data).unwrap();
        for (binding, slot) in slots.iter().enumerate() {
            gpu.bind(binding as u32, *slot).unwrap();
        }
        // A draw the API refuses (a mutant that wants other bindings) still
        // leaves the frame usable.
        let drawn = gpu.draw(3, 1, 0, 0).is_ok();
        gpu.end_pass().unwrap();
        gpu.end_frame().unwrap();
        gpu.destroy(pipeline).unwrap();
        drawn
    };
    mutate(&mut gpu, &words, ShaderStage::Fragment, "fs", &mut draw);
    no_validation_errors();
}

const VERTEX: &str = r#"
struct Instance { float4 offset_scale; float4 color; };
[[vk::binding(0, 0)]] StructuredBuffer<Instance> instances;

struct VsOut {
    float4 position : SV_Position;
    float clip : SV_ClipDistance0;
    [[vk::location(0)]] float4 color : COLOR;
};

[shader("vertex")]
VsOut vs([[vk::location(0)]] float2 position : POSITION, uint vertex : SV_VertexID, uint instance : SV_InstanceID) {
    Instance i = instances[instance + vertex / 3];
    VsOut o;
    o.position = float4(position * i.offset_scale.zw + i.offset_scale.xy, 0, 1);
    o.clip = o.position.x + 0.5;
    o.color = i.color;
    return o;
}

[shader("fragment")]
float4 fs([[vk::location(0)]] float4 color : COLOR, uint s : SV_SampleIndex) : SV_Target0 {
    return color * float(s + 1);
}
"#;

#[test]
fn mutants_of_a_vertex_shader_are_refused_or_valid() {
    let mut gpu = gpu();
    let fs = shader(&mut gpu, ShaderStage::Fragment, "fs", VERTEX);
    let words = words_of(VERTEX, Stage::Vertex, "vs");
    let vertices = gpu
        .create_buffer(&BufferDesc {
            size: 256,
            usage: BufferUsage::VERTEX,
        })
        .unwrap();
    gpu.write_buffer(vertices, 0, &f32_bytes(&[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0]))
        .unwrap();
    let instances = gpu
        .create_buffer(&BufferDesc {
            size: 256,
            usage: BufferUsage::STORAGE,
        })
        .unwrap();
    gpu.write_buffer(
        instances,
        0,
        &f32_bytes(&[0.0, 0.0, 1.0, 1.0, 1.0, 0.5, 0.25, 1.0]),
    )
    .unwrap();
    let slot = gpu.create_storage_buffer_slot(instances, 0, 64).unwrap();
    let target = image(
        &mut gpu,
        Format::Rgba8Unorm,
        2,
        2,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );
    let mut draw = |gpu: &mut Gpu, vs: Handle| -> bool {
        let Ok(pipeline) = gpu.create_render_pipeline(&RenderPipelineDesc {
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
        }) else {
            return false;
        };
        gpu.begin_frame().unwrap();
        gpu.begin_pass_desc(&PassDesc {
            color: vec![color(target, [0.0; 4])],
            depth: None,
        })
        .unwrap();
        gpu.set_pipeline(pipeline).unwrap();
        gpu.set_vertex_buffer(0, vertices, 0).unwrap();
        gpu.bind(0, slot).unwrap();
        let drawn = gpu.draw(3, 2, 0, 1).is_ok();
        gpu.end_pass().unwrap();
        gpu.end_frame().unwrap();
        gpu.destroy(pipeline).unwrap();
        drawn
    };
    mutate(&mut gpu, &words, ShaderStage::Vertex, "vs", &mut draw);
    no_validation_errors();
}
