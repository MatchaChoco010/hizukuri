//! Depth: a depth image as an attachment, the depth state of a pipeline, and
//! the depth image read back as a texture. Shaders are Slang; needs `slangc`.

use proto_gfx_core::*;

mod slang;
use slang::*;

/// A full-screen triangle at the depth and in the colour of the push data.
const FLAT: &str = r#"
struct Push { float4 color; float z; };
[[vk::push_constant]] ConstantBuffer<Push> pc;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], pc.z, 1);
}

[shader("fragment")]
float4 fs() : SV_Target0 { return pc.color; }

// For passes without a colour attachment.
[shader("fragment")]
void fs_depth_only() {}
"#;

/// Copies a depth texture into the red channel.
const SHOW: &str = r#"
[[vk::binding(0, 0)]] Texture2D<float> depth;
[[vk::binding(1, 0)]] SamplerState smp;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}

[shader("fragment")]
float4 fs(float4 position : SV_Position) : SV_Target0 {
    float d = depth.SampleLevel(smp, position.xy / 4.0, 0);
    return float4(d, 0, 0, 1);
}
"#;

fn depth_state(compare: CompareOp) -> PipelineExtra {
    PipelineExtra {
        depth: Some(DepthState {
            format: Format::D32Float,
            test: true,
            write: true,
            compare,
            bias: None,
        }),
        ..PipelineExtra::default()
    }
}

fn push(gpu: &mut Gpu, color: [f32; 4], z: f32) {
    let mut data = f32_bytes(&color);
    data.extend(z.to_ne_bytes());
    gpu.push(0, &data).unwrap();
}

struct Scene {
    gpu: Gpu,
    vs: Handle,
    fs: Handle,
    color: Handle,
    depth: Handle,
}

fn scene() -> Scene {
    let mut gpu = gpu();
    let vs = shader(&mut gpu, ShaderStage::Vertex, "vs", FLAT);
    let fs = shader(&mut gpu, ShaderStage::Fragment, "fs", FLAT);
    let color = image(
        &mut gpu,
        Format::Rgba8Unorm,
        4,
        4,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );
    let depth = image(
        &mut gpu,
        Format::D32Float,
        4,
        4,
        ImageUsage::DEPTH_TARGET | ImageUsage::SAMPLED | ImageUsage::COPY_SRC,
    );
    Scene {
        gpu,
        vs,
        fs,
        color,
        depth,
    }
}

/// The nearer of two overlapping triangles stays, whatever the order they
/// are drawn in; the depth image holds its depth and can be sampled in a
/// later pass.
#[test]
fn the_depth_test_keeps_the_nearest_fragment() {
    let Scene {
        mut gpu,
        vs,
        fs,
        color: target,
        depth: depth_image,
    } = scene();
    let pipeline = pipeline(
        &mut gpu,
        vs,
        fs,
        &[Format::Rgba8Unorm],
        &depth_state(CompareOp::Less),
    )
    .unwrap();
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![color(target, [0.0; 4])],
        depth: Some(depth(depth_image, 1.0)),
    })
    .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    // Near and red first, then far and green, then nearest and blue in the
    // left half only.
    push(&mut gpu, [1.0, 0.0, 0.0, 1.0], 0.25);
    gpu.draw(3, 1, 0, 0).unwrap();
    push(&mut gpu, [0.0, 1.0, 0.0, 1.0], 0.75);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.set_scissor(0, 0, 2, 4).unwrap();
    push(&mut gpu, [0.0, 0.0, 1.0, 1.0], 0.125);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();

    let pixels = gpu.read_image(target).unwrap();
    assert_eq!(&pixels[..4], [0, 0, 255, 255], "left: the nearest");
    assert_eq!(&pixels[12..16], [255, 0, 0, 255], "right: the near one");
    let depths = gpu.read_image(depth_image).unwrap();
    assert_eq!(depths.len(), 4 * 4 * 4);
    assert_eq!(f32_at(&depths, 0), 0.125);
    assert_eq!(f32_at(&depths, 3), 0.25);

    // The depth image as a texture.
    let show_vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHOW);
    let show_fs = shader(&mut gpu, ShaderStage::Fragment, "fs", SHOW);
    let show = slang::pipeline(
        &mut gpu,
        show_vs,
        show_fs,
        &[Format::Rgba8Unorm],
        &PipelineExtra::default(),
    )
    .unwrap();
    let slot = gpu.create_texture_slot(depth_image).unwrap();
    let sampler = gpu
        .create_sampler(&SamplerDesc {
            mag: Filter::Nearest,
            min: Filter::Nearest,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        })
        .unwrap();
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![color(target, [0.0; 4])],
        depth: None,
    })
    .unwrap();
    gpu.set_pipeline(show).unwrap();
    gpu.bind(0, slot).unwrap();
    gpu.bind(1, sampler).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    let pixels = gpu.read_image(target).unwrap();
    assert_eq!(&pixels[..4], [32, 0, 0, 255], "0.125");
    assert_eq!(&pixels[12..16], [64, 0, 0, 255], "0.25");
    no_validation_errors();
}

/// A pass with only a depth attachment (a shadow map), a compare function
/// other than `less`, a depth bias, and the depth range of the viewport.
#[test]
fn depth_only_passes_and_the_other_depth_state() {
    let Scene {
        mut gpu,
        vs,
        depth: depth_image,
        ..
    } = scene();
    let fs = shader(&mut gpu, ShaderStage::Fragment, "fs_depth_only", FLAT);
    let greater = pipeline(&mut gpu, vs, fs, &[], &depth_state(CompareOp::Greater)).unwrap();
    let biased = pipeline(
        &mut gpu,
        vs,
        fs,
        &[],
        &PipelineExtra {
            depth: Some(DepthState {
                format: Format::D32Float,
                test: true,
                write: true,
                compare: CompareOp::Always,
                bias: Some(DepthBias {
                    constant: 0.0,
                    slope: 0.0,
                    clamp: 0.0,
                }),
            }),
            front_face: FrontFace::Clockwise,
            samples: 1,
        },
    )
    .unwrap();
    let pass = PassDesc {
        color: vec![],
        depth: Some(depth(depth_image, 0.0)),
    };
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&pass).unwrap();
    gpu.set_pipeline(greater).unwrap();
    push(&mut gpu, [0.0; 4], 0.5);
    gpu.draw(3, 1, 0, 0).unwrap();
    push(&mut gpu, [0.0; 4], 0.25);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    assert_eq!(f32_at(&gpu.read_image(depth_image).unwrap(), 5), 0.5);

    // The viewport maps depth 0..1 to 0.5..1: 0.5 becomes 0.75.
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&pass).unwrap();
    gpu.set_pipeline(biased).unwrap();
    gpu.set_viewport_depth(0.0, 0.0, 4.0, 4.0, 0.5, 1.0)
        .unwrap();
    push(&mut gpu, [0.0; 4], 0.5);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    assert_eq!(f32_at(&gpu.read_image(depth_image).unwrap(), 5), 0.75);
    no_validation_errors();
}

#[test]
fn depth_misuse() {
    let Scene {
        mut gpu,
        vs,
        fs,
        color: target,
        depth: depth_image,
    } = scene();
    let with_depth = pipeline(
        &mut gpu,
        vs,
        fs,
        &[Format::Rgba8Unorm],
        &depth_state(CompareOp::Less),
    )
    .unwrap();
    let without_depth = pipeline(
        &mut gpu,
        vs,
        fs,
        &[Format::Rgba8Unorm],
        &PipelineExtra::default(),
    )
    .unwrap();

    // Images: a depth format is not a colour target, and the reverse.
    expect!(
        gpu.create_image(&ImageDesc {
            format: Format::D32Float,
            width: 4,
            height: 4,
            usage: ImageUsage::COLOR_TARGET,
        }),
        Unsupported
    );
    expect!(
        gpu.create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 4,
            height: 4,
            usage: ImageUsage::DEPTH_TARGET,
        }),
        Unsupported
    );
    assert!(gpu.format_supports_usage(Format::D32Float, ImageUsage::DEPTH_TARGET));
    assert!(!gpu.format_supports_usage(Format::D32Float, ImageUsage::COLOR_TARGET));

    // Pipelines: the depth format has to be one, a pipeline needs something
    // to draw to, and the bias has to be finite.
    let bad_depth = |format, bias| PipelineExtra {
        depth: Some(DepthState {
            format,
            test: true,
            write: true,
            compare: CompareOp::Less,
            bias,
        }),
        ..PipelineExtra::default()
    };
    let fs_none = shader(&mut gpu, ShaderStage::Fragment, "fs_depth_only", FLAT);
    expect!(
        pipeline(
            &mut gpu,
            vs,
            fs_none,
            &[],
            &bad_depth(Format::Rgba8Unorm, None)
        ),
        Unsupported
    );
    expect!(
        pipeline(&mut gpu, vs, fs_none, &[], &PipelineExtra::default()),
        InvalidArgument
    );
    // A fragment output that no colour target receives.
    expect!(
        pipeline(&mut gpu, vs, fs, &[], &depth_state(CompareOp::Less)),
        ShaderInterface
    );
    expect!(
        pipeline(
            &mut gpu,
            vs,
            fs_none,
            &[],
            &bad_depth(
                Format::D32Float,
                Some(DepthBias {
                    constant: f32::NAN,
                    slope: 0.0,
                    clamp: 0.0
                })
            )
        ),
        InvalidArgument
    );
    expect!(
        pipeline(
            &mut gpu,
            vs,
            fs_none,
            &[],
            &PipelineExtra {
                samples: 3,
                ..depth_state(CompareOp::Less)
            }
        ),
        InvalidArgument
    );

    let small_depth = image(&mut gpu, Format::D32Float, 2, 2, ImageUsage::DEPTH_TARGET);
    let slot = gpu.create_texture_slot(depth_image).unwrap();
    let sampler = gpu
        .create_sampler(&SamplerDesc {
            mag: Filter::Nearest,
            min: Filter::Nearest,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        })
        .unwrap();
    let show_vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHOW);
    let show_fs = shader(&mut gpu, ShaderStage::Fragment, "fs", SHOW);
    let show = pipeline(
        &mut gpu,
        show_vs,
        show_fs,
        &[Format::Rgba8Unorm],
        &depth_state(CompareOp::Always),
    )
    .unwrap();

    gpu.begin_frame().unwrap();
    // Passes: wrong usage, the same image twice, another size, a clear depth
    // out of range, a mip level the image does not have, no attachment.
    let pass = |color_image, depth_image, clear| PassDesc {
        color: vec![color(color_image, [0.0; 4])],
        depth: Some(depth(depth_image, clear)),
    };
    expect!(
        gpu.begin_pass_desc(&pass(depth_image, depth_image, 1.0)),
        InvalidArgument
    );
    expect!(
        gpu.begin_pass_desc(&pass(target, target, 1.0)),
        InvalidArgument
    );
    expect!(
        gpu.begin_pass_desc(&pass(target, small_depth, 1.0)),
        Mismatch
    );
    expect!(
        gpu.begin_pass_desc(&pass(target, depth_image, 1.5)),
        InvalidArgument
    );
    expect!(
        gpu.begin_pass_desc(&pass(target, depth_image, f32::NAN)),
        InvalidArgument
    );
    expect!(
        gpu.begin_pass_desc(&PassDesc {
            color: vec![color(target, [0.0; 4])],
            depth: Some(PassDepth {
                sub: Subresource { mip: 1, layer: 0 },
                ..depth(depth_image, 1.0)
            }),
        }),
        OutOfRange
    );
    expect!(gpu.begin_pass_desc(&PassDesc::default()), InvalidArgument);
    assert!(!gpu.is_in_pass());

    // A pipeline and a pass have to agree on the depth attachment.
    gpu.begin_pass_desc(&pass(target, depth_image, 1.0))
        .unwrap();
    expect!(gpu.set_pipeline(without_depth), Mismatch);
    gpu.set_pipeline(with_depth).unwrap();
    expect!(
        gpu.set_viewport_depth(0.0, 0.0, 4.0, 4.0, 0.0, 2.0),
        InvalidArgument
    );
    expect!(
        gpu.set_viewport_depth(0.0, 0.0, 4.0, 4.0, f32::NAN, 1.0),
        InvalidArgument
    );
    // The depth attachment of the open pass cannot be sampled in it.
    gpu.set_pipeline(show).unwrap();
    gpu.bind(0, slot).unwrap();
    gpu.bind(1, sampler).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), InvalidState);
    gpu.end_pass().unwrap();

    gpu.begin_pass_desc(&PassDesc {
        color: vec![color(target, [0.0; 4])],
        depth: None,
    })
    .unwrap();
    expect!(gpu.set_pipeline(with_depth), Mismatch);
    gpu.set_pipeline(without_depth).unwrap();
    push(&mut gpu, [1.0, 1.0, 0.0, 1.0], 0.5);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();

    // The device still renders a correct frame.
    assert_eq!(&gpu.read_image(target).unwrap()[..4], [255, 255, 0, 255]);
    no_validation_errors();
}

/// Reads of a shadow map: with a comparison, and without.
const SHADOW: &str = r#"
[[vk::binding(0, 0)]] Texture2D<float> shadow_map;
[[vk::binding(1, 0)]] SamplerComparisonState shadow_sampler;
[[vk::binding(2, 0)]] SamplerState plain_sampler;
[[vk::binding(3, 0)]] Texture2D<float4> color_map;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}

// How much of the texel passes the comparison with 0.5.
[shader("fragment")]
float4 fs_compare(float4 p : SV_Position) : SV_Target0 {
    float lit = shadow_map.SampleCmpLevelZero(shadow_sampler, p.xy / 4.0, 0.5);
    return float4(lit, 0, 0, 1);
}

[shader("fragment")]
float4 fs_plain(float4 p : SV_Position) : SV_Target0 {
    return float4(shadow_map.SampleLevel(plain_sampler, p.xy / 4.0, 0), 0, 0, 1);
}

[shader("fragment")]
float4 fs_compare_color() : SV_Target0 {
    float lit = color_map.SampleCmpLevelZero(shadow_sampler, float2(0.5, 0.5), 0.5);
    return float4(lit, 0, 0, 1);
}
"#;

/// A comparison sampler reads a shadow map; comparison and plain samplers
/// do not stand in for each other.
#[test]
fn a_comparison_sampler_reads_a_shadow_map() {
    let Scene {
        mut gpu,
        vs,
        color: target,
        depth: shadow_map,
        ..
    } = scene();
    // Depth 0.25 in the left half, 0.75 in the right half.
    let depth_only = shader(&mut gpu, ShaderStage::Fragment, "fs_depth_only", FLAT);
    let fill = pipeline(
        &mut gpu,
        vs,
        depth_only,
        &[],
        &depth_state(CompareOp::Always),
    )
    .unwrap();
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![],
        depth: Some(depth(shadow_map, 1.0)),
    })
    .unwrap();
    gpu.set_pipeline(fill).unwrap();
    push(&mut gpu, [0.0; 4], 0.75);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.set_scissor(0, 0, 2, 4).unwrap();
    push(&mut gpu, [0.0; 4], 0.25);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();

    let sampler = |gpu: &mut Gpu, compare| {
        gpu.create_sampler_with(
            &SamplerDesc {
                mag: Filter::Nearest,
                min: Filter::Nearest,
                address_u: AddressMode::ClampToEdge,
                address_v: AddressMode::ClampToEdge,
            },
            &SamplerExtra {
                compare,
                ..SamplerExtra::default()
            },
        )
        .unwrap()
    };
    let less_equal = sampler(&mut gpu, Some(CompareOp::LessOrEqual));
    let greater = sampler(&mut gpu, Some(CompareOp::Greater));
    let plain = sampler(&mut gpu, None);
    let shadow_slot = gpu.create_texture_slot(shadow_map).unwrap();
    let color_image = image(
        &mut gpu,
        Format::Rgba8Unorm,
        4,
        4,
        ImageUsage::SAMPLED | ImageUsage::COLOR_TARGET,
    );
    let color_slot = gpu.create_texture_slot(color_image).unwrap();

    let shadow_vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHADOW);
    let make = |gpu: &mut Gpu, entry| {
        let fs = shader(gpu, ShaderStage::Fragment, entry, SHADOW);
        pipeline(
            gpu,
            shadow_vs,
            fs,
            &[Format::Rgba8Unorm],
            &PipelineExtra::default(),
        )
        .unwrap()
    };
    let compare = make(&mut gpu, "fs_compare");
    let plain_read = make(&mut gpu, "fs_plain");
    let compare_color = make(&mut gpu, "fs_compare_color");
    let draw = |gpu: &mut Gpu, pipeline, binds: &[(u32, Handle)]| -> Result<Vec<u8>> {
        gpu.begin_frame().unwrap();
        gpu.begin_pass_desc(&PassDesc {
            color: vec![color(target, [0.0; 4])],
            depth: None,
        })
        .unwrap();
        gpu.set_pipeline(pipeline).unwrap();
        for (binding, slot) in binds {
            gpu.bind(*binding, *slot).unwrap();
        }
        let drawn = gpu.draw(3, 1, 0, 0);
        gpu.end_pass().unwrap();
        gpu.end_frame().unwrap();
        drawn?;
        gpu.read_image(target)
    };

    // 0.5 <= 0.25 fails on the left; 0.5 <= 0.75 passes on the right.
    let pixels = draw(&mut gpu, compare, &[(0, shadow_slot), (1, less_equal)]).unwrap();
    assert_eq!((pixels[0], pixels[12]), (0, 255), "less-equal");
    let pixels = draw(&mut gpu, compare, &[(0, shadow_slot), (1, greater)]).unwrap();
    assert_eq!((pixels[0], pixels[12]), (255, 0), "greater");
    let pixels = draw(&mut gpu, plain_read, &[(0, shadow_slot), (2, plain)]).unwrap();
    assert_eq!((pixels[0], pixels[12]), (64, 191), "the depth itself");

    // A comparison needs a comparison sampler and a depth texture; a plain
    // read needs a plain sampler.
    expect!(
        draw(&mut gpu, compare, &[(0, shadow_slot), (1, plain)]),
        Mismatch
    );
    expect!(
        draw(&mut gpu, plain_read, &[(0, shadow_slot), (2, less_equal)]),
        Mismatch
    );
    expect!(
        draw(&mut gpu, compare_color, &[(3, color_slot), (1, less_equal)]),
        Mismatch
    );
    no_validation_errors();
}
