use proto_gfx_core::*;

/// Checks an 8-bit UNORM pixel written from a float colour. Vulkan converts
/// a float `f` to unsigned normalized fixed-point by taking either of the two
/// code values closest to `f × 255`: rounding to nearest is recommended, not
/// required, and only a product that is exactly an integer must come out
/// unchanged. So a channel may be one code value away from `expected` (the
/// round-to-nearest value); `0.5` (127.5) may be written as 127 or 128.
fn assert_unorm8(actual: &[u8], expected: [u8; 4]) {
    for c in 0..4 {
        assert!(
            actual[c].abs_diff(expected[c]) <= 1,
            "channel {c}: got {actual:?}, expected {expected:?} (give or take 1)"
        );
    }
}

#[test]
fn triangle_smoke() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).expect("gpu");
    eprintln!("device: {} / {}", gpu.device_name(), gpu.driver_name());
    eprintln!("{}", gpu.heap_info());
    let vs = gpu
        .create_shader(&ShaderSource {
            stage: ShaderStage::Vertex,
            language: ShaderLanguage::Wgsl,
            entry: "main",
            source: r#"
            @vertex fn main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
                var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
                return vec4(p[i], 0.0, 1.0);
            }"#,
        })
        .unwrap();
    let fs = gpu
        .create_shader(&ShaderSource {
            stage: ShaderStage::Fragment,
            language: ShaderLanguage::Wgsl,
            entry: "main",
            source: r#"
            struct Push { color: vec4<f32> }
            var<immediate> push: Push;
            @fragment fn main() -> @location(0) vec4<f32> { return push.color; }"#,
        })
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
            width: 8,
            height: 8,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        })
        .unwrap();
    gpu.begin_frame().unwrap();
    gpu.begin_pass(&[ColorAttachment {
        image: target,
        load: LoadOp::Clear([0.0, 0.0, 0.0, 1.0]),
        store: true,
    }])
    .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    let color: [f32; 4] = [1.0, 0.5, 0.25, 1.0];
    let bytes: Vec<u8> = color.iter().flat_map(|c| c.to_ne_bytes()).collect();
    gpu.push(0, &bytes).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    let pixels = gpu.read_image(target).unwrap();
    assert_unorm8(&pixels[..4], [255, 128, 64, 255]);
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}

/// The compiler is behind a trait and takes a language tag: a GLSL fragment
/// shader goes through the same reflection and checks as WGSL.
#[test]
fn glsl_fragment_shader() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).expect("gpu");
    let vs = gpu
        .create_shader(&ShaderSource {
            stage: ShaderStage::Vertex,
            language: ShaderLanguage::Wgsl,
            entry: "main",
            source: r#"
            struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> }
            @vertex fn main(@builtin(vertex_index) i: u32) -> VsOut {
                var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
                var out: VsOut;
                out.position = vec4(p[i], 0.0, 1.0);
                out.uv = p[i] * 0.5 + 0.5;
                return out;
            }"#,
        })
        .unwrap();
    let fs = gpu
        .create_shader(&ShaderSource {
            stage: ShaderStage::Fragment,
            language: ShaderLanguage::Glsl,
            entry: "main",
            source: r#"#version 450
            layout(push_constant) uniform Push { vec4 gain; } pc;
            layout(set = 0, binding = 0) uniform texture2D tex;
            layout(set = 0, binding = 1) uniform sampler smp;
            layout(location = 0) in vec2 uv;
            layout(location = 0) out vec4 color;
            void main() {
                color = texture(sampler2D(tex, smp), uv) * pc.gain;
            }"#,
        })
        .unwrap();
    let reflection = gpu.shader_reflection(fs).unwrap();
    assert_eq!(reflection.push_size, 16);
    assert_eq!(reflection.bindings.len(), 2);
    assert_eq!(reflection.sampling_pairs, vec![(0, 1)]);
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
            mag: Filter::Nearest,
            min: Filter::Nearest,
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
    gpu.bind(0, slot).unwrap();
    gpu.bind(1, sampler).unwrap();
    let gain: Vec<u8> = [1.0f32, 0.5, 0.5, 1.0]
        .iter()
        .flat_map(|v| v.to_ne_bytes())
        .collect();
    gpu.push(0, &gain).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    let pixels = gpu.read_image(target).unwrap();
    // 200/255 × 0.5 × 255 is 100 only up to float rounding, so 99 to 101.
    assert_unorm8(&pixels[..4], [255, 100, 50, 255]);
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}
