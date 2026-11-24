//! The drawing state a 3D scene uses, with Slang shaders: several colour
//! targets with their own blending, culling and the front face, instances
//! (a per-instance vertex buffer, and a storage buffer indexed with the
//! instance), 32-bit indices, a uniform buffer. Needs `slangc`.

use proto_gfx_core::*;

mod slang;
use slang::*;

const SHADERS: &str = r#"
struct Instance { float4 offset_scale; float4 color; };
[[vk::binding(0, 0)]] StructuredBuffer<Instance> instances;
struct Globals { float4 tint; };
[[vk::binding(1, 0)]] ConstantBuffer<Globals> globals;

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float4 color : COLOR;
};

[shader("vertex")]
VsOut vs(
    [[vk::location(0)]] float2 position : POSITION,
    [[vk::location(1)]] float2 offset : OFFSET,
    uint instance : SV_VulkanInstanceID)
{
    Instance i = instances[instance];
    VsOut o;
    o.position = float4(position * i.offset_scale.zw + i.offset_scale.xy + offset, 0, 1);
    o.color = i.color * globals.tint;
    return o;
}

struct FsOut {
    float4 color : SV_Target0;
    float4 count : SV_Target1;
};

[shader("fragment")]
FsOut fs(VsOut i) {
    FsOut o;
    o.color = i.color;
    o.count = float4(0.25, 0.25, 0.25, 0.25);
    return o;
}
"#;

struct Scene {
    gpu: Gpu,
    vs: Handle,
    fs: Handle,
    quad: Handle,
    offsets: Handle,
    indices: Handle,
    instance_slot: Handle,
    globals_slot: Handle,
    color: Handle,
    count: Handle,
}

fn scene() -> Scene {
    let mut gpu = gpu();
    let vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHADERS);
    let fs = shader(&mut gpu, ShaderStage::Fragment, "fs", SHADERS);
    let mut buffer = |usage, bytes: &[u8]| {
        let buffer = gpu
            .create_buffer(&BufferDesc {
                size: bytes.len().max(256) as u64,
                usage,
            })
            .unwrap();
        gpu.write_buffer(buffer, 0, bytes).unwrap();
        buffer
    };
    // A unit quad, counter-clockwise on the screen (y points down).
    let quad = buffer(
        BufferUsage::VERTEX,
        &f32_bytes(&[-1.0, -1.0, -1.0, 1.0, 1.0, 1.0, 1.0, -1.0]),
    );
    // One offset per instance; both zero.
    let offsets = buffer(BufferUsage::VERTEX, &f32_bytes(&[0.0; 4]));
    let indices = buffer(
        BufferUsage::INDEX,
        &[0u32, 1, 2, 0, 2, 3]
            .iter()
            .flat_map(|i| i.to_ne_bytes())
            .collect::<Vec<_>>(),
    );
    // Instance 0: the left half, red. Instance 1: the right half, green.
    let instances = buffer(
        BufferUsage::STORAGE,
        &f32_bytes(&[
            -0.5, 0.0, 0.5, 1.0, 1.0, 0.0, 0.0, 1.0, //
            0.5, 0.0, 0.5, 1.0, 0.0, 1.0, 0.0, 1.0,
        ]),
    );
    let globals = buffer(BufferUsage::UNIFORM, &f32_bytes(&[1.0, 0.5, 1.0, 1.0]));
    let instance_slot = gpu.create_storage_buffer_slot(instances, 0, 64).unwrap();
    let globals_slot = gpu.create_uniform_slot(globals, 0, 16).unwrap();
    let usage = ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC;
    let color = image(&mut gpu, Format::Rgba8Unorm, 4, 4, usage);
    let count = image(&mut gpu, Format::Rgba8Unorm, 4, 4, usage);
    Scene {
        gpu,
        vs,
        fs,
        quad,
        offsets,
        indices,
        instance_slot,
        globals_slot,
        color,
        count,
    }
}

fn desc(scene: &Scene, cull: CullMode) -> RenderPipelineDesc {
    RenderPipelineDesc {
        vertex: scene.vs,
        fragment: scene.fs,
        vertex_buffers: vec![
            VertexBufferLayout {
                stride: 8,
                per_instance: false,
                attributes: vec![VertexAttribute {
                    location: 0,
                    format: VertexFormat::F32x2,
                    offset: 0,
                }],
            },
            VertexBufferLayout {
                stride: 8,
                per_instance: true,
                attributes: vec![VertexAttribute {
                    location: 1,
                    format: VertexFormat::F32x2,
                    offset: 0,
                }],
            },
        ],
        // The second target adds up what is drawn to it.
        targets: vec![
            ColorTarget {
                format: Format::Rgba8Unorm,
                blend: None,
            },
            ColorTarget {
                format: Format::Rgba8Unorm,
                blend: Some(Blend::ADDITIVE),
            },
        ],
        topology: Topology::TriangleList,
        cull,
    }
}

/// Draws the two instances `times` times and returns both targets.
fn draw(
    scene: &mut Scene,
    pipeline: Handle,
    times: u32,
    first_instance: u32,
) -> (Vec<u8>, Vec<u8>) {
    let gpu = &mut scene.gpu;
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![color(scene.color, [0.0; 4]), color(scene.count, [0.0; 4])],
        depth: None,
    })
    .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    gpu.set_vertex_buffer(0, scene.quad, 0).unwrap();
    gpu.set_vertex_buffer(1, scene.offsets, 0).unwrap();
    gpu.set_index_buffer(scene.indices, 0, IndexType::U32)
        .unwrap();
    gpu.bind(0, scene.instance_slot).unwrap();
    gpu.bind(1, scene.globals_slot).unwrap();
    for _ in 0..times {
        gpu.draw_indexed(6, 2, 0, 0, first_instance).unwrap();
    }
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    (
        gpu.read_image(scene.color).unwrap(),
        gpu.read_image(scene.count).unwrap(),
    )
}

#[test]
fn instances_targets_and_culling() {
    let mut scene = scene();
    let both_sides = scene
        .gpu
        .create_render_pipeline(&desc(&scene, CullMode::None))
        .unwrap();
    let (colors, counts) = draw(&mut scene, both_sides, 2, 0);
    // Red times the tint (1, 0.5, 1, 1) on the left; green times it on the
    // right.
    assert_eq!(&colors[..4], [255, 0, 0, 255], "left");
    let right = &colors[12..16];
    assert!(
        right[0] == 0 && right[1].abs_diff(128) <= 1 && right[3] == 255,
        "right: {right:?}"
    );
    // Two draws of 0.25, added up; the first target is not blended.
    assert!(
        counts.iter().all(|c| c.abs_diff(128) <= 1),
        "{:?}",
        &counts[..4]
    );

    // The quad is counter-clockwise: it is the front with the default front
    // face and the back with the other.
    let cull = |scene: &mut Scene, cull, front_face| {
        let pipeline = scene
            .gpu
            .create_render_pipeline_with(
                &desc(scene, cull),
                &PipelineExtra {
                    front_face,
                    ..PipelineExtra::default()
                },
            )
            .unwrap();
        draw(scene, pipeline, 1, 0).0[..4].to_vec()
    };
    let drawn = [255, 0, 0, 255];
    let culled = [0, 0, 0, 0];
    use CullMode::{Back, Front};
    use FrontFace::{Clockwise, CounterClockwise};
    assert_eq!(cull(&mut scene, Back, CounterClockwise), drawn);
    assert_eq!(cull(&mut scene, Front, CounterClockwise), culled);
    assert_eq!(cull(&mut scene, Back, Clockwise), culled);
    assert_eq!(cull(&mut scene, Front, Clockwise), drawn);

    // Instances the buffers do not hold: the per-instance vertex buffer is
    // checked by the API; the index into the storage buffer is clamped in
    // the shader, so the draw is defined (and draws nothing new to check).
    let gpu = &mut scene.gpu;
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![color(scene.color, [0.0; 4]), color(scene.count, [0.0; 4])],
        depth: None,
    })
    .unwrap();
    gpu.set_pipeline(both_sides).unwrap();
    gpu.set_vertex_buffer(0, scene.quad, 0).unwrap();
    gpu.set_vertex_buffer(1, scene.offsets, 0).unwrap();
    gpu.set_index_buffer(scene.indices, 0, IndexType::U32)
        .unwrap();
    gpu.bind(0, scene.instance_slot).unwrap();
    gpu.bind(1, scene.globals_slot).unwrap();
    expect!(gpu.draw_indexed(6, 2, 0, 0, 1_000_000), OutOfRange);
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    no_validation_errors();
}

/// What a lit material reads: a dozen bindings of every kind, one of them
/// at the last binding number, next to an 80-byte push block.
const MATERIAL: &str = r#"
struct Push { float4x4 model; float4 tint; };
[[vk::push_constant]] ConstantBuffer<Push> pc;
struct Scene { float4x4 view_proj; float4 eye; };
[[vk::binding(0, 0)]] ConstantBuffer<Scene> scene;
struct Light { float4 position; float4 color; };
[[vk::binding(1, 0)]] StructuredBuffer<Light> lights;
[[vk::binding(2, 0)]] Texture2D<float4> albedo;
[[vk::binding(3, 0)]] Texture2D<float4> normal_map;
[[vk::binding(4, 0)]] Texture2D<float4> ltc_1;
[[vk::binding(5, 0)]] Texture2D<float4> ltc_2;
[[vk::binding(6, 0)]] TextureCube<float4> environment;
[[vk::binding(7, 0)]] Texture2D<float> shadow_map;
[[vk::binding(8, 0)]] Texture2D<float4> screens[5];
[[vk::binding(9, 0)]] SamplerState smp;
[[vk::binding(10, 0)]] SamplerState other_smp;
[[vk::binding(31, 0)]] Texture3D<float4> lut;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return mul(pc.model, float4(p[i], 0, 1));
}

[shader("fragment")]
float4 fs() : SV_Target0 {
    float2 uv = float2(0.5, 0.5);
    float sum = albedo.SampleLevel(smp, uv, 0).r
        + normal_map.SampleLevel(smp, uv, 0).r
        + ltc_1.SampleLevel(other_smp, uv, 0).r
        + ltc_2.SampleLevel(other_smp, uv, 0).r
        + environment.SampleLevel(smp, float3(0, 1, 0), 0).r
        + shadow_map.SampleLevel(smp, uv, 0)
        + screens[uint(pc.tint.w)].SampleLevel(smp, uv, 0).r
        + lut.SampleLevel(smp, float3(0.5, 0.5, 0.5), 0).r
        + lights[1].color.r
        + scene.eye.x;
    return float4(sum, pc.tint.r, 0, 1);
}
"#;

#[test]
fn a_material_with_many_bindings() {
    let mut gpu = gpu();
    let vs = shader(&mut gpu, ShaderStage::Vertex, "vs", MATERIAL);
    let fs = shader(&mut gpu, ShaderStage::Fragment, "fs", MATERIAL);
    let material = pipeline(
        &mut gpu,
        vs,
        fs,
        &[Format::Rgba32Float],
        &PipelineExtra::default(),
    )
    .unwrap();

    // One-texel images, each holding its own value in the red channel.
    let texel = |gpu: &mut Gpu, kind, layers: u32, value: f32| {
        let image = gpu
            .create_image_with(
                &ImageDesc {
                    format: Format::Rgba32Float,
                    width: 1,
                    height: 1,
                    usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
                },
                &ImageExtra {
                    kind,
                    ..ImageExtra::default()
                },
            )
            .unwrap();
        gpu.begin_frame().unwrap();
        for layer in 0..layers {
            gpu.upload_image_level(
                image,
                Subresource { mip: 0, layer },
                &f32_bytes(&[value, 0.0, 0.0, 1.0]),
            )
            .unwrap();
        }
        gpu.end_frame().unwrap();
        image
    };
    let flat: Vec<Handle> = [1.0, 2.0, 4.0, 8.0]
        .into_iter()
        .map(|value| {
            let image = texel(&mut gpu, ImageKind::D2, 1, value);
            gpu.create_texture_slot(image).unwrap()
        })
        .collect();
    let cube = texel(&mut gpu, ImageKind::Cube, 6, 16.0);
    let cube_slot = gpu.create_texture_slot(cube).unwrap();
    let volume = texel(&mut gpu, ImageKind::D3, 1, 64.0);
    let volume_slot = gpu.create_texture_slot(volume).unwrap();
    let screens: Vec<Handle> = (0..5)
        .map(|i| texel(&mut gpu, ImageKind::D2, 1, 32.0 + i as f32))
        .collect();
    let screen_slots = gpu.create_texture_slot_run(&screens).unwrap();
    // The shadow map: a depth image cleared to 0.5.
    let shadow = image(
        &mut gpu,
        Format::D32Float,
        1,
        1,
        ImageUsage::DEPTH_TARGET | ImageUsage::SAMPLED,
    );
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![],
        depth: Some(depth(shadow, 0.5)),
    })
    .unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    let shadow_slot = gpu.create_texture_slot(shadow).unwrap();

    let mut buffer = |usage, values: &[f32]| {
        let buffer = gpu.create_buffer(&BufferDesc { size: 256, usage }).unwrap();
        gpu.write_buffer(buffer, 0, &f32_bytes(values)).unwrap();
        buffer
    };
    let mut scene_data = vec![0.0; 16];
    scene_data.extend([256.0, 0.0, 0.0, 0.0]);
    let scene_buffer = buffer(BufferUsage::UNIFORM, &scene_data);
    let mut light_data = vec![0.0; 12];
    light_data.extend([128.0, 0.0, 0.0, 0.0]);
    let light_buffer = buffer(BufferUsage::STORAGE, &light_data);
    let scene_slot = gpu.create_uniform_slot(scene_buffer, 0, 80).unwrap();
    let light_slot = gpu.create_storage_buffer_slot(light_buffer, 0, 64).unwrap();
    let sampler = |gpu: &mut Gpu| {
        gpu.create_sampler(&SamplerDesc {
            mag: Filter::Nearest,
            min: Filter::Nearest,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        })
        .unwrap()
    };
    let (smp, other_smp) = (sampler(&mut gpu), sampler(&mut gpu));
    let target = image(
        &mut gpu,
        Format::Rgba32Float,
        1,
        1,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );

    let binds = [
        (0, scene_slot),
        (1, light_slot),
        (2, flat[0]),
        (3, flat[1]),
        (4, flat[2]),
        (5, flat[3]),
        (6, cube_slot),
        (7, shadow_slot),
        (8, screen_slots[0]),
        (9, smp),
        (10, other_smp),
        (31, volume_slot),
    ];
    // The identity, then the tint; its last number picks the screen.
    let mut push = vec![0.0; 20];
    for i in 0..4 {
        push[i * 5] = 1.0;
    }
    push[16] = 0.25;
    push[19] = 3.0;
    let draw = |gpu: &mut Gpu, skip: Option<u32>| {
        gpu.begin_frame().unwrap();
        gpu.begin_pass_desc(&PassDesc {
            color: vec![color(target, [0.0; 4])],
            depth: None,
        })
        .unwrap();
        gpu.set_pipeline(material).unwrap();
        for (binding, slot) in binds {
            if Some(binding) != skip {
                gpu.bind(binding, slot).unwrap();
            }
        }
        gpu.push(0, &f32_bytes(&push)).unwrap();
        let drawn = gpu.draw(3, 1, 0, 0);
        gpu.end_pass().unwrap();
        gpu.end_frame().unwrap();
        drawn
    };
    draw(&mut gpu, None).unwrap();
    let pixel = gpu.read_image(target).unwrap();
    // 1 + 2 + 4 + 8 + 16 + 0.5 + (32 + 3) + 64 + 128 + 256.
    assert_eq!(f32_at(&pixel, 0), 514.5);
    assert_eq!(f32_at(&pixel, 1), 0.25);
    // Every one of them is checked, the last binding number included.
    expect!(draw(&mut gpu, Some(31)), MissingBinding);
    expect!(draw(&mut gpu, Some(7)), MissingBinding);
    expect!(gpu.bind(32, smp), InvalidState);
    no_validation_errors();
}
