//! 3D textures (lookup tables, such as the 3D LUTs of OCIO's shaders): a
//! sampled 3D binding reads a 3D image, and neither kind of image can be
//! bound where the other is declared, since the descriptor's view type must
//! match the shader's declaration.

use proto_gfx_core::*;

mod common;

use common::{ShaderLanguage, ShaderSource};

const FULLSCREEN_VS: &str = r#"
@vertex fn main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(p[i], 0.0, 1.0);
}
"#;

const LUT_FS: &str = r#"
@group(0) @binding(0) var lut: texture_3d<f32>;
@group(0) @binding(1) var smp: sampler;
@fragment fn main() -> @location(0) vec4<f32> {
    return textureSampleLevel(lut, smp, vec3(0.75, 0.25, 0.75), 0.0);
}
"#;

const FLAT_FS: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@fragment fn main() -> @location(0) vec4<f32> {
    return textureSampleLevel(tex, smp, vec2(0.5, 0.5), 0.0);
}
"#;

fn shader(gpu: &mut Gpu, stage: ShaderStage, source: &str) -> Handle {
    let spirv = common::naga_spirv(&ShaderSource {
        stage,
        language: ShaderLanguage::Wgsl,
        source,
        entry: "main",
    })
    .unwrap();
    gpu.create_shader_spirv(stage, "main", &spirv).unwrap()
}

fn pipeline(gpu: &mut Gpu, fs: &str) -> (Handle, Handle) {
    let vs = shader(gpu, ShaderStage::Vertex, FULLSCREEN_VS);
    let fs = shader(gpu, ShaderStage::Fragment, fs);
    let p = gpu
        .create_render_pipeline(&RenderPipelineDesc {
            vertex: vs,
            fragment: fs,
            vertex_buffers: vec![],
            targets: vec![ColorTarget {
                format: Format::Rgba32Float,
                blend: None,
            }],
            topology: Topology::TriangleList,
            cull: CullMode::None,
        })
        .unwrap();
    (p, fs)
}

fn floats(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Draws one pixel with `pipeline`, binding `texture` and `sampler`.
fn draw(gpu: &mut Gpu, pipeline: Handle, texture: Handle, sampler: Handle) -> Result<[f32; 4]> {
    let target = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba32Float,
            width: 1,
            height: 1,
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
    gpu.bind(0, texture).unwrap();
    gpu.bind(1, sampler).unwrap();
    let drawn = gpu.draw(3, 1, 0, 0);
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    drawn?;
    let pixel = gpu.read_image(target).unwrap();
    let v: Vec<f32> = pixel
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok([v[0], v[1], v[2], v[3]])
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

/// A 2×2×2 table whose texel (x, y, z) holds (x, y, z, 1).
fn lut_2x2x2(gpu: &mut Gpu) -> Handle {
    let image = gpu
        .create_image_3d(
            &ImageDesc {
                format: Format::Rgba32Float,
                width: 2,
                height: 2,
                usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
            },
            2,
        )
        .unwrap();
    let mut texels = Vec::new();
    for z in 0..2 {
        for y in 0..2 {
            for x in 0..2 {
                texels.extend([x as f32, y as f32, z as f32, 1.0]);
            }
        }
    }
    gpu.begin_frame().unwrap();
    gpu.upload_image(image, &floats(&texels)).unwrap();
    gpu.end_frame().unwrap();
    image
}

#[test]
fn a_3d_texture_is_sampled() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).expect("gpu");
    let (p, fs) = pipeline(&mut gpu, LUT_FS);
    assert_eq!(
        gpu.shader_reflection(fs).unwrap().bindings[0].kind,
        BindingKind::SampledTexture3D {
            class: NumClass::Float
        }
    );
    let image = lut_2x2x2(&mut gpu);
    let slot = gpu.create_texture_slot(image).unwrap();
    let smp = nearest(&mut gpu);
    assert_eq!(draw(&mut gpu, p, slot, smp).unwrap(), [1.0, 0.0, 1.0, 1.0]);
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}

#[test]
fn flat_and_3d_images_do_not_stand_in_for_each_other() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).expect("gpu");
    let smp = nearest(&mut gpu);
    let lut = lut_2x2x2(&mut gpu);
    let lut_slot = gpu.create_texture_slot(lut).unwrap();
    let flat = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba32Float,
            width: 1,
            height: 1,
            usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
        })
        .unwrap();
    gpu.begin_frame().unwrap();
    gpu.upload_image(flat, &floats(&[0.5; 4])).unwrap();
    gpu.end_frame().unwrap();
    let flat_slot = gpu.create_texture_slot(flat).unwrap();

    let (lut_pipeline, _) = pipeline(&mut gpu, LUT_FS);
    let (flat_pipeline, _) = pipeline(&mut gpu, FLAT_FS);
    let e = draw(&mut gpu, lut_pipeline, flat_slot, smp).unwrap_err();
    assert_eq!(e.code, ErrorCode::Mismatch, "{e}");
    let e = draw(&mut gpu, flat_pipeline, lut_slot, smp).unwrap_err();
    assert_eq!(e.code, ErrorCode::Mismatch, "{e}");
    assert_eq!(
        draw(&mut gpu, flat_pipeline, flat_slot, smp).unwrap(),
        [0.5; 4]
    );

    // A 3D image is only sampled and uploaded to.
    let desc = |usage| ImageDesc {
        format: Format::Rgba32Float,
        width: 2,
        height: 2,
        usage,
    };
    for usage in [
        ImageUsage::SAMPLED | ImageUsage::COLOR_TARGET,
        ImageUsage::SAMPLED | ImageUsage::STORAGE,
        ImageUsage::SAMPLED | ImageUsage::COPY_SRC,
    ] {
        let e = gpu.create_image_3d(&desc(usage), 2).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    }
    let e = gpu
        .create_image_3d(&desc(ImageUsage::SAMPLED), 0)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    let e = gpu.create_opaque_texture_slot(lut).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    let e = gpu.create_texture_slot_run(&[lut]).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    // The upload must cover the whole volume.
    gpu.begin_frame().unwrap();
    let e = gpu.upload_image(lut, &floats(&[0.0; 16])).unwrap_err();
    assert_eq!(e.code, ErrorCode::Mismatch, "{e}");
    gpu.end_frame().unwrap();
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}
