//! Proves that the validation layer is alive and would catch what the API's
//! own checks stop: with the checks switched off, a misuse reaches Vulkan and
//! the layer reports it.
//!
//! Runs only on the software rasterizer, where an invalid call cannot disturb
//! a real GPU, and only with `GFX_SPIKE_VALIDATION=1`.

use proto_gfx_core::*;

mod common;

through_both_entries!(validation_layer_sees_misuse_when_checks_are_off);

const SHADER: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@vertex fn vs(@location(0) p: vec2<f32>) -> @builtin(position) vec4<f32> {
    return vec4(p, 0.0, 1.0);
}
@fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(tex, vec2<i32>(p.xy), 0);
}
"#;

fn validation_layer_sees_misuse_when_checks_are_off() {
    let config = GpuConfig::for_tests();
    if !config.validation {
        eprintln!("skipped: GFX_SPIKE_VALIDATION is not set");
        return;
    }
    let mut gpu = Gpu::new(config).unwrap();
    if !gpu.device_name().contains("llvmpipe") {
        eprintln!(
            "skipped: only runs on llvmpipe, not on {}",
            gpu.device_name()
        );
        return;
    }
    let shader = |gpu: &mut Gpu, stage, entry| {
        common::create_shader(
            gpu,
            &ShaderSource {
                stage,
                language: ShaderLanguage::Wgsl,
                source: SHADER,
                entry,
            },
        )
        .unwrap()
    };
    let vs = shader(&mut gpu, ShaderStage::Vertex, "vs");
    let fs = shader(&mut gpu, ShaderStage::Fragment, "fs");
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
    let target = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 8,
            height: 8,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::SAMPLED,
        })
        .unwrap();
    let slot = gpu.create_texture_slot(target).unwrap();
    let vertices = gpu
        .create_buffer(&BufferDesc {
            size: 24,
            usage: BufferUsage::VERTEX,
        })
        .unwrap();

    let other_format = gpu
        .create_image(&ImageDesc {
            format: Format::Bgra8Unorm,
            width: 8,
            height: 8,
            usage: ImageUsage::COLOR_TARGET,
        })
        .unwrap();
    let source = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 8,
            height: 8,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::SAMPLED,
        })
        .unwrap();
    let source_slot = gpu.create_texture_slot(source).unwrap();
    let pass = |image| {
        [ColorAttachment {
            image,
            load: LoadOp::Clear([0.0; 4]),
            store: true,
        }]
    };

    // With the checks on, the API refuses both misuses; Vulkan never sees them.
    gpu.begin_frame().unwrap();
    gpu.begin_pass(&pass(other_format)).unwrap();
    let error = gpu.set_pipeline(pipeline).unwrap_err();
    assert_eq!(error.code, ErrorCode::Mismatch, "{error}");
    gpu.end_pass().unwrap();
    gpu.begin_pass(&pass(target)).unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    gpu.set_vertex_buffer(0, vertices, 0).unwrap();
    gpu.bind(0, slot).unwrap();
    let error = gpu.draw(3, 1, 0, 0).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidState, "{error}");
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    gpu.wait_idle().unwrap();
    assert_eq!(validation_error_count(), 0, "{:#?}", validation_messages());

    // SAFETY: deliberate misuse, on the software rasterizer only.
    unsafe { gpu.set_checks(false) };

    // Misuse A: a pipeline for rgba8unorm in a pass on a bgra8unorm target.
    gpu.begin_frame().unwrap();
    gpu.begin_pass(&pass(other_format)).unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    gpu.set_vertex_buffer(0, vertices, 0).unwrap();
    gpu.bind(0, source_slot).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    gpu.wait_idle().unwrap();
    let after_a = validation_error_count();
    eprintln!("misuse A (attachment format mismatch): {after_a} validation message(s)");
    for message in validation_messages() {
        eprintln!("  {}", &message[..message.len().min(300)]);
    }
    assert!(after_a > 0, "the validation layer did not notice misuse A");

    // Misuse B: sampling, through a heap descriptor, the image that is the
    // attachment of the open pass. Recorded, not asserted: it shows how much
    // the layer knows about descriptor heap contents.
    gpu.begin_frame().unwrap();
    gpu.begin_pass(&pass(target)).unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    gpu.set_vertex_buffer(0, vertices, 0).unwrap();
    gpu.bind(0, slot).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    gpu.wait_idle().unwrap();
    let after_b = validation_error_count();
    eprintln!(
        "misuse B (sampling the open pass's attachment via the heap): {} validation message(s)",
        after_b - after_a
    );
}
