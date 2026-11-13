//! What a 3D shader written in Slang asks of the device beyond the basics:
//! capabilities the API enables when the device has them (each one drawn
//! with here), and the ones it refuses. Needs `slangc`.

use proto_gfx_core::*;

mod slang;
use slang::*;

const SHADERS: &str = r#"
[[vk::binding(2, 0)]] TextureCubeArray<float4> cubes;
[[vk::binding(3, 0)]] SamplerState smp;

float2 corner(uint i) {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return p[i];
}

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position { return float4(corner(i), 0, 1); }

// SV_VertexID and SV_InstanceID count from the first vertex and instance of
// the draw (DrawParameters).
struct Ids {
    float4 position : SV_Position;
    [[vk::location(0)]] float instance : INSTANCE;
};
[shader("vertex")]
Ids vs_ids(uint i : SV_VertexID, uint instance : SV_InstanceID) {
    Ids o;
    o.position = float4(corner(i), 0, 1);
    o.instance = float(instance);
    return o;
}
[shader("fragment")]
float4 fs_ids(Ids i) : SV_Target0 { return float4((i.instance + 1.0) / 4.0, 0, 0, 1); }

// A clip plane at x = 0 (ClipDistance).
struct Clipped {
    float4 position : SV_Position;
    float clip : SV_ClipDistance0;
};
[shader("vertex")]
Clipped vs_clip(uint i : SV_VulkanVertexID) {
    Clipped o;
    o.position = float4(corner(i), 0, 1);
    o.clip = o.position.x;
    return o;
}
[shader("fragment")]
float4 fs_white() : SV_Target0 { return float4(1, 1, 1, 1); }

// Arithmetic in 16-bit floats and 64-bit integers.
[shader("fragment")]
float4 fs_types(float4 p : SV_Position) : SV_Target0 {
    half h = half(p.w) * 0.5h;
    int64_t big = int64_t(p.w) << 40;
    return float4(float(h), float(big >> 40), 0, 1);
}

// Shading per sample (SampleRateShading).
[shader("fragment")]
float4 fs_sample(uint s : SV_SampleIndex) : SV_Target0 { return float4(float(s) / 4.0, 0, 0, 1); }

// Refused: cube arrays.
[shader("fragment")]
float4 fs_cube_array() : SV_Target0 { return cubes.SampleLevel(smp, float4(1, 0, 0, 0), 0); }
"#;

/// Draws with the two entry points into a new 4x4 target and returns its
/// pixels.
fn draw(gpu: &mut Gpu, vs: &str, fs: &str, samples: u32, draw: impl FnOnce(&mut Gpu)) -> Vec<u8> {
    let vs = shader(gpu, ShaderStage::Vertex, vs, SHADERS);
    let fs = shader(gpu, ShaderStage::Fragment, fs, SHADERS);
    let pipeline = pipeline(
        gpu,
        vs,
        fs,
        &[Format::Rgba8Unorm],
        &PipelineExtra {
            samples,
            ..PipelineExtra::default()
        },
    )
    .unwrap();
    let target = image(
        gpu,
        Format::Rgba8Unorm,
        4,
        4,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );
    let attachment = if samples == 1 {
        color(target, [0.0; 4])
    } else {
        let multisampled = gpu
            .create_image_with(
                &ImageDesc {
                    format: Format::Rgba8Unorm,
                    width: 4,
                    height: 4,
                    usage: ImageUsage::COLOR_TARGET,
                },
                &ImageExtra {
                    samples,
                    ..ImageExtra::default()
                },
            )
            .unwrap();
        PassColor {
            resolve: Some((target, Subresource::default())),
            ..color(multisampled, [0.0; 4])
        }
    };
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![attachment],
        depth: None,
    })
    .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    draw(gpu);
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    gpu.read_image(target).unwrap()
}

#[test]
fn optional_capabilities_draw() {
    let mut gpu = gpu();
    // Two instances, starting at instance 3 and vertex 0: SV_InstanceID is
    // 0 and 1, and the second one is drawn last.
    let pixels = draw(&mut gpu, "vs_ids", "fs_ids", 1, |gpu| {
        gpu.draw(3, 2, 0, 3).unwrap()
    });
    assert!(pixels[0].abs_diff(128) <= 1, "{:?}", &pixels[..4]);

    // The left half is clipped away.
    let pixels = draw(&mut gpu, "vs_clip", "fs_white", 1, |gpu| {
        gpu.draw(3, 1, 0, 0).unwrap()
    });
    assert_eq!(&pixels[..4], [0, 0, 0, 0], "left");
    assert_eq!(&pixels[12..16], [255, 255, 255, 255], "right");

    let pixels = draw(&mut gpu, "vs", "fs_types", 1, |gpu| {
        gpu.draw(3, 1, 0, 0).unwrap()
    });
    assert!(
        pixels[0].abs_diff(128) <= 1 && pixels[1] == 255,
        "{:?}",
        &pixels[..4]
    );

    // The four samples of a pixel hold 0, 1/4, 2/4 and 3/4.
    let pixels = draw(&mut gpu, "vs", "fs_sample", 4, |gpu| {
        gpu.draw(3, 1, 0, 0).unwrap()
    });
    assert!(pixels[0].abs_diff(96) <= 2, "{:?}", &pixels[..4]);
    no_validation_errors();
}

#[test]
fn what_the_api_cannot_check_is_refused() {
    let mut gpu = gpu();
    expect!(
        try_shader(&mut gpu, ShaderStage::Fragment, "fs_cube_array", SHADERS),
        ShaderCompile
    );
    no_validation_errors();
}

/// What the device supports is asked, not assumed.
#[test]
fn formats_are_queried() {
    let gpu = gpu();
    for format in Format::ALL {
        let caps = gpu.format_caps(*format);
        assert_eq!(
            gpu.format_supports_usage(*format, ImageUsage::COLOR_TARGET),
            caps.color_attachment,
            "{}",
            format.name()
        );
        assert!(!(caps.depth_attachment && caps.color_attachment));
        assert_eq!(Format::from_name(format.name()), Some(*format));
    }
    // Every device has these.
    for format in [
        Format::Rgba8Unorm,
        Format::Rgba16Float,
        Format::Rg11b10Float,
    ] {
        assert!(
            gpu.format_supports_usage(format, ImageUsage::SAMPLED),
            "{format:?}"
        );
    }
    assert!(gpu.format_supports_usage(
        Format::Rgba16Float,
        ImageUsage::SAMPLED | ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC
    ));
    assert!(gpu.format_caps(Format::D32Float).depth_attachment);
    assert!(!gpu.format_supports_usage(Format::D32Float, ImageUsage::STORAGE));
}
