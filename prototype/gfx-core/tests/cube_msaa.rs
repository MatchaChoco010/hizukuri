//! Cube images (rendered face by face, sampled as a cube or face by face)
//! and multisampled attachments resolved by the pass. Shaders are Slang;
//! needs `slangc`.

use proto_gfx_core::*;

mod slang;
use slang::*;

const SHADERS: &str = r#"
struct Push { float4 color; float3 dir; };
[[vk::push_constant]] ConstantBuffer<Push> pc;
[[vk::binding(0, 0)]] TextureCube<float4> cube;
[[vk::binding(1, 0)]] SamplerState smp;
[[vk::binding(2, 0)]] Texture2D<float4> face;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0.5, 1);
}

// The upper-left half of the target.
[shader("vertex")]
float4 vs_half(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(1, -1), float2(-1, 1) };
    return float4(p[i], 0.5, 1);
}

[shader("fragment")]
float4 flat() : SV_Target0 { return pc.color; }

[shader("fragment")]
float4 from_cube() : SV_Target0 { return cube.SampleLevel(smp, pc.dir, 0); }

[shader("fragment")]
float4 from_face() : SV_Target0 { return face.SampleLevel(smp, float2(0.5, 0.5), 0); }
"#;

const USAGE: ImageUsage = ImageUsage(
    ImageUsage::SAMPLED.0
        | ImageUsage::COLOR_TARGET.0
        | ImageUsage::COPY_SRC.0
        | ImageUsage::COPY_DST.0,
);

/// The colour of face `layer`: distinct in every face.
fn face_color(layer: u32) -> [f32; 4] {
    [
        (layer & 1) as f32,
        ((layer >> 1) & 1) as f32,
        ((layer >> 2) & 1) as f32,
        1.0,
    ]
}

fn to_u8(color: [f32; 4]) -> [u8; 4] {
    color.map(|c| (c * 255.0) as u8)
}

fn push(gpu: &mut Gpu, color: [f32; 4], dir: [f32; 3]) {
    let mut data = f32_bytes(&color);
    data.extend(f32_bytes(&dir));
    gpu.push(0, &data).unwrap();
}

fn make(gpu: &mut Gpu, vs: &str, fs: &str, extra: &PipelineExtra) -> Handle {
    let vs = shader(gpu, ShaderStage::Vertex, vs, SHADERS);
    let fs = shader(gpu, ShaderStage::Fragment, fs, SHADERS);
    pipeline(gpu, vs, fs, &[Format::Rgba8Unorm], extra).unwrap()
}

fn nearest(gpu: &mut Gpu) -> Handle {
    gpu.create_sampler(&SamplerDesc {
        mag: Filter::Nearest,
        min: Filter::Nearest,
        address_u: AddressMode::ClampToEdge,
        address_v: AddressMode::ClampToEdge,
    })
    .unwrap()
}

fn cube_image(gpu: &mut Gpu, size: u32, mip_levels: u32) -> Handle {
    gpu.create_image_with(
        &ImageDesc {
            format: Format::Rgba8Unorm,
            width: size,
            height: size,
            usage: USAGE,
        },
        &ImageExtra {
            kind: ImageKind::Cube,
            mip_levels,
            ..ImageExtra::default()
        },
    )
    .unwrap()
}

/// Draws one triangle in its own frame and returns the result of the draw
/// and the first pixel of `target`.
fn draw(
    gpu: &mut Gpu,
    pipeline: Handle,
    target: Handle,
    binds: &[(u32, Handle)],
    dir: [f32; 3],
) -> Result<[u8; 4]> {
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
    // Not every shader here has a push block; a refused push records
    // nothing.
    let mut data = f32_bytes(&[0.0; 4]);
    data.extend(f32_bytes(&dir));
    let _ = gpu.push(0, &data);
    let drawn = gpu.draw(3, 1, 0, 0);
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    drawn?;
    Ok(gpu.read_image(target).unwrap()[..4].try_into().unwrap())
}

#[test]
fn a_cube_is_rendered_face_by_face_and_sampled() {
    let mut gpu = gpu();
    let flat = make(&mut gpu, "vs", "flat", &PipelineExtra::default());
    let from_cube = make(&mut gpu, "vs", "from_cube", &PipelineExtra::default());
    let from_face = make(&mut gpu, "vs", "from_face", &PipelineExtra::default());
    let cube = cube_image(&mut gpu, 4, 3);
    let target = image(
        &mut gpu,
        Format::Rgba8Unorm,
        1,
        1,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );
    let smp = nearest(&mut gpu);

    // Faces 0 to 4 are drawn; face 5 is uploaded.
    gpu.begin_frame().unwrap();
    for layer in 0..5 {
        gpu.begin_pass_desc(&PassDesc {
            color: vec![PassColor {
                sub: Subresource { mip: 0, layer },
                ..color(cube, [0.0; 4])
            }],
            depth: None,
        })
        .unwrap();
        gpu.set_pipeline(flat).unwrap();
        push(&mut gpu, face_color(layer), [0.0; 3]);
        gpu.draw(3, 1, 0, 0).unwrap();
        gpu.end_pass().unwrap();
    }
    gpu.upload_image_level(
        cube,
        Subresource { mip: 0, layer: 5 },
        &to_u8(face_color(5)).repeat(16),
    )
    .unwrap();
    gpu.generate_mipmaps(cube).unwrap();
    gpu.end_frame().unwrap();

    let slot = gpu.create_texture_slot(cube).unwrap();
    // +X, -X, +Y, -Y, +Z, -Z.
    let directions = [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    for (layer, dir) in directions.into_iter().enumerate() {
        let pixel = draw(&mut gpu, from_cube, target, &[(0, slot), (1, smp)], dir).unwrap();
        assert_eq!(pixel, to_u8(face_color(layer as u32)), "face {layer}");
    }
    // One face as a 2D texture, and the smallest level of every face.
    for layer in [0, 3, 5] {
        let face = gpu
            .create_texture_slot_view(
                cube,
                &TextureView {
                    base_mip: 2,
                    mip_count: Some(1),
                    layer: Some(layer),
                },
            )
            .unwrap();
        let pixel = draw(
            &mut gpu,
            from_face,
            target,
            &[(2, face), (1, smp)],
            [0.0; 3],
        )
        .unwrap();
        assert_eq!(pixel, to_u8(face_color(layer)), "face {layer} as 2D");
        let level = gpu
            .read_image_level(cube, Subresource { mip: 2, layer })
            .unwrap();
        assert_eq!(level, to_u8(face_color(layer)), "level 2 of face {layer}");
    }

    // The declaration in the shader and what the slot shows have to agree.
    let flat_image = image(&mut gpu, Format::Rgba8Unorm, 4, 4, USAGE);
    let flat_slot = gpu.create_texture_slot(flat_image).unwrap();
    expect!(
        draw(
            &mut gpu,
            from_cube,
            target,
            &[(0, flat_slot), (1, smp)],
            [1.0, 0.0, 0.0]
        ),
        Mismatch
    );
    expect!(
        draw(
            &mut gpu,
            from_face,
            target,
            &[(2, slot), (1, smp)],
            [0.0; 3]
        ),
        Mismatch
    );
    let one_face = gpu
        .create_texture_slot_view(
            cube,
            &TextureView {
                layer: Some(1),
                ..TextureView::default()
            },
        )
        .unwrap();
    expect!(
        draw(
            &mut gpu,
            from_cube,
            target,
            &[(0, one_face), (1, smp)],
            [1.0, 0.0, 0.0]
        ),
        Mismatch
    );

    // Cube images: square, six layers, 2D only in texture arrays.
    let cube_desc = |width, height| ImageDesc {
        format: Format::Rgba8Unorm,
        width,
        height,
        usage: USAGE,
    };
    let cube_extra = ImageExtra {
        kind: ImageKind::Cube,
        ..ImageExtra::default()
    };
    expect!(
        gpu.create_image_with(&cube_desc(4, 2), &cube_extra),
        InvalidArgument
    );
    expect!(
        gpu.create_image_with(
            &cube_desc(4, 4),
            &ImageExtra {
                depth: 2,
                ..cube_extra
            }
        ),
        InvalidArgument
    );
    expect!(
        gpu.create_image_with(
            &ImageDesc {
                usage: ImageUsage::COLOR_TARGET,
                ..cube_desc(4, 4)
            },
            &ImageExtra {
                samples: 4,
                ..cube_extra
            }
        ),
        InvalidArgument
    );
    expect!(
        gpu.create_texture_slot_view(
            cube,
            &TextureView {
                layer: Some(6),
                ..TextureView::default()
            }
        ),
        OutOfRange
    );
    expect!(gpu.create_texture_slot_run(&[cube]), InvalidArgument);
    expect!(gpu.create_opaque_texture_slot(cube), InvalidArgument);
    gpu.begin_frame().unwrap();
    expect!(
        gpu.begin_pass_desc(&PassDesc {
            color: vec![PassColor {
                sub: Subresource { mip: 0, layer: 6 },
                ..color(cube, [0.0; 4])
            }],
            depth: None,
        }),
        OutOfRange
    );
    expect!(gpu.copy_image(cube, flat_image), InvalidArgument);
    gpu.end_frame().unwrap();
    no_validation_errors();
}

#[test]
fn a_multisampled_pass_is_resolved() {
    let mut gpu = gpu();
    let multisampled = |format, usage| {
        (
            ImageDesc {
                format,
                width: 4,
                height: 4,
                usage,
            },
            ImageExtra {
                samples: 4,
                ..ImageExtra::default()
            },
        )
    };
    let (desc, extra) = multisampled(Format::Rgba8Unorm, ImageUsage::COLOR_TARGET);
    let msaa = gpu.create_image_with(&desc, &extra).unwrap();
    let (desc, extra) = multisampled(Format::D32Float, ImageUsage::DEPTH_TARGET);
    let msaa_depth = gpu.create_image_with(&desc, &extra).unwrap();
    assert_eq!(gpu.image_info(msaa).unwrap().samples, 4);
    let resolved = image(
        &mut gpu,
        Format::Rgba8Unorm,
        4,
        4,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC | ImageUsage::SAMPLED,
    );
    let four_samples = PipelineExtra {
        depth: Some(DepthState {
            format: Format::D32Float,
            test: true,
            write: true,
            compare: CompareOp::Less,
            bias: None,
        }),
        samples: 4,
        ..PipelineExtra::default()
    };
    let half = make(&mut gpu, "vs_half", "flat", &four_samples);
    let one_sample = make(&mut gpu, "vs_half", "flat", &PipelineExtra::default());
    let pass = PassDesc {
        color: vec![PassColor {
            store: false,
            resolve: Some((resolved, Subresource::default())),
            ..color(msaa, [0.0; 4])
        }],
        depth: Some(PassDepth {
            store: false,
            ..depth(msaa_depth, 1.0)
        }),
    };
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&pass).unwrap();
    // The pipeline has to draw with the sample count of the pass.
    expect!(gpu.set_pipeline(one_sample), Mismatch);
    gpu.set_pipeline(half).unwrap();
    push(&mut gpu, [1.0, 1.0, 1.0, 1.0], [0.0; 3]);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();

    let pixels = gpu.read_image(resolved).unwrap();
    let at = |x: usize, y: usize| pixels[(y * 4 + x) * 4];
    assert_eq!(at(0, 0), 255, "inside");
    assert_eq!(at(3, 3), 0, "outside");
    // The edge runs through the pixels of the diagonal: some of their
    // samples are covered.
    for i in 0..4 {
        let edge = at(i, 3 - i);
        assert!(edge > 0 && edge < 255, "pixel ({i}, {}) is {edge}", 3 - i);
    }

    // Multisampled images: 1, 2, 4 or 8 samples, attachments only.
    let (desc, extra) = multisampled(Format::Rgba8Unorm, ImageUsage::COLOR_TARGET);
    for samples in [0, 3, 16] {
        expect!(
            gpu.create_image_with(&desc, &ImageExtra { samples, ..extra }),
            InvalidArgument
        );
    }
    for usage in [
        ImageUsage::COLOR_TARGET | ImageUsage::SAMPLED,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        ImageUsage::COLOR_TARGET | ImageUsage::STORAGE,
    ] {
        expect!(
            gpu.create_image_with(&ImageDesc { usage, ..desc }, &extra),
            InvalidArgument
        );
    }
    expect!(
        gpu.create_image_with(
            &desc,
            &ImageExtra {
                mip_levels: 2,
                ..extra
            }
        ),
        InvalidArgument
    );
    // A sample count the device may not have is refused or works.
    match gpu.create_image_with(
        &desc,
        &ImageExtra {
            samples: 8,
            ..extra
        },
    ) {
        Ok(_) => {}
        Err(e) => assert_eq!(e.code, ErrorCode::Unsupported, "{e}"),
    }
    let vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHADERS);
    let fs = shader(&mut gpu, ShaderStage::Fragment, "flat", SHADERS);
    let eight = PipelineExtra {
        samples: 8,
        ..PipelineExtra::default()
    };
    match pipeline(&mut gpu, vs, fs, &[Format::Rgba8Unorm], &eight) {
        Ok(_) => {}
        Err(e) => assert_eq!(e.code, ErrorCode::Unsupported, "{e}"),
    }
    expect!(gpu.read_image(msaa), InvalidArgument);
    expect!(gpu.create_texture_slot(msaa), InvalidArgument);

    // Passes.
    let single = image(
        &mut gpu,
        Format::Rgba8Unorm,
        4,
        4,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );
    let other_format = image(
        &mut gpu,
        Format::Rgba16Float,
        4,
        4,
        ImageUsage::COLOR_TARGET,
    );
    let other_size = image(&mut gpu, Format::Rgba8Unorm, 2, 2, ImageUsage::COLOR_TARGET);
    let (desc, extra) = multisampled(Format::Rgba8Unorm, ImageUsage::COLOR_TARGET);
    let msaa_2 = gpu.create_image_with(&desc, &extra).unwrap();
    let resolving = |image, target| PassDesc {
        color: vec![PassColor {
            resolve: Some((target, Subresource::default())),
            ..color(image, [0.0; 4])
        }],
        depth: None,
    };
    gpu.begin_frame().unwrap();
    // Attachments of different sample counts.
    expect!(
        gpu.begin_pass_desc(&PassDesc {
            color: vec![color(msaa, [0.0; 4]), color(single, [0.0; 4])],
            depth: None,
        }),
        Mismatch
    );
    expect!(
        gpu.begin_pass_desc(&PassDesc {
            color: vec![color(single, [0.0; 4])],
            depth: Some(depth(msaa_depth, 1.0)),
        }),
        Mismatch
    );
    // Resolving: only from a multisampled attachment, into a single-sampled
    // image of the same format and size that is not attached otherwise.
    expect!(
        gpu.begin_pass_desc(&resolving(single, resolved)),
        InvalidArgument
    );
    expect!(gpu.begin_pass_desc(&resolving(msaa, msaa_2)), Mismatch);
    expect!(
        gpu.begin_pass_desc(&resolving(msaa, other_format)),
        Mismatch
    );
    expect!(gpu.begin_pass_desc(&resolving(msaa, other_size)), Mismatch);
    expect!(gpu.begin_pass_desc(&resolving(msaa, msaa)), Mismatch);
    expect!(
        gpu.begin_pass_desc(&PassDesc {
            color: vec![
                PassColor {
                    resolve: Some((resolved, Subresource::default())),
                    ..color(msaa, [0.0; 4])
                },
                PassColor {
                    resolve: Some((resolved, Subresource::default())),
                    ..color(msaa_2, [0.0; 4])
                },
            ],
            depth: None,
        }),
        InvalidArgument
    );
    assert!(!gpu.is_in_pass());
    gpu.end_frame().unwrap();
    no_validation_errors();
}
