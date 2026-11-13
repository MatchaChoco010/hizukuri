//! Mip levels and samplers: creating an image with a mip chain, uploading
//! to and rendering into single levels, generating the chain, slots that
//! show a range of levels, and the level-of-detail controls of a sampler.
//! Shaders are Slang; needs `slangc`.

use proto_gfx_core::*;

mod slang;
use slang::*;

const SHADERS: &str = r#"
struct Push { float4 color; float lod; float scale; };
[[vk::push_constant]] ConstantBuffer<Push> pc;
[[vk::binding(0, 0)]] Texture2D<float4> tex;
[[vk::binding(1, 0)]] SamplerState smp;
// Slang gives an integer texture an image format (Rgba32ui here) unless it
// is told not to; the API accepts sampled textures without one only.
[[vk::binding(2, 0)]] [[vk::image_format("unknown")]] Texture2D<uint4> utex;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}

[shader("fragment")]
float4 flat() : SV_Target0 { return pc.color; }

// An explicit level of detail.
[shader("fragment")]
float4 level() : SV_Target0 { return tex.SampleLevel(smp, float2(pc.scale, 0.5), pc.lod); }

// Explicit gradients.
[shader("fragment")]
float4 grad() : SV_Target0 {
    return tex.SampleGrad(smp, float2(0.5, 0.5), float2(pc.lod, 0), float2(0, pc.lod));
}

// The level of detail from the derivatives of the coordinates.
[shader("fragment")]
float4 implicit(float4 p : SV_Position) : SV_Target0 { return tex.Sample(smp, p.xy * pc.scale); }

// Halves the level the slot shows, asking the texture for its size.
[shader("fragment")]
float4 down(float4 p : SV_Position) : SV_Target0 {
    uint w, h;
    tex.GetDimensions(w, h);
    return tex.SampleLevel(smp, p.xy * 2.0 / float2(w, h), 0) * 0.5;
}

[shader("fragment")]
float4 integers() : SV_Target0 { return float4(utex.SampleLevel(smp, float2(2, 2), 0)) / 255.0; }
"#;

const USAGE: ImageUsage = ImageUsage(
    ImageUsage::SAMPLED.0
        | ImageUsage::COLOR_TARGET.0
        | ImageUsage::COPY_SRC.0
        | ImageUsage::COPY_DST.0,
);

fn mipped(
    gpu: &mut Gpu,
    format: Format,
    size: u32,
    mip_levels: u32,
    usage: ImageUsage,
) -> Result<Handle> {
    gpu.create_image_with(
        &ImageDesc {
            format,
            width: size,
            height: size,
            usage,
        },
        &ImageExtra {
            mip_levels,
            ..ImageExtra::default()
        },
    )
}

fn solid(color: [u8; 4], texels: usize) -> Vec<u8> {
    color.repeat(texels)
}

fn sampler(gpu: &mut Gpu, filter: Filter, extra: SamplerExtra) -> Handle {
    gpu.create_sampler_with(
        &SamplerDesc {
            mag: filter,
            min: filter,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        },
        &extra,
    )
    .unwrap()
}

struct Kit {
    gpu: Gpu,
    vs: Handle,
    flat: Handle,
    level: Handle,
    target: Handle,
}

impl Kit {
    fn new() -> Self {
        let mut gpu = gpu();
        let vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHADERS);
        let flat = Self::make(&mut gpu, vs, "flat");
        let level = Self::make(&mut gpu, vs, "level");
        let target = image(
            &mut gpu,
            Format::Rgba8Unorm,
            2,
            2,
            ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        );
        Self {
            gpu,
            vs,
            flat,
            level,
            target,
        }
    }

    fn make(gpu: &mut Gpu, vs: Handle, entry: &str) -> Handle {
        let fs = shader(gpu, ShaderStage::Fragment, entry, SHADERS);
        pipeline(
            gpu,
            vs,
            fs,
            &[Format::Rgba8Unorm],
            &PipelineExtra::default(),
        )
        .unwrap()
    }

    /// Draws one triangle over `target` in its own frame and returns the
    /// result of the draw.
    fn draw(
        &mut self,
        pipeline: Handle,
        target: PassColor,
        binds: &[(u32, Handle)],
        (lod, scale): (f32, f32),
    ) -> Result<()> {
        let gpu = &mut self.gpu;
        gpu.begin_frame().unwrap();
        gpu.begin_pass_desc(&PassDesc {
            color: vec![target],
            depth: None,
        })
        .unwrap();
        gpu.set_pipeline(pipeline).unwrap();
        for (binding, slot) in binds {
            gpu.bind(*binding, *slot).unwrap();
        }
        // Not every shader here has a push block; a refused push records
        // nothing.
        let _ = gpu.push(0, &f32_bytes(&[0.0, 0.0, 1.0, 1.0, lod, scale]));
        let drawn = gpu.draw(3, 1, 0, 0);
        gpu.end_pass().unwrap();
        gpu.end_frame().unwrap();
        drawn
    }

    /// Samples `slot` with `sampler` at a level of detail and returns the
    /// first pixel.
    fn sample(&mut self, pipeline: Handle, slot: Handle, sampler: Handle, lod: f32) -> [u8; 4] {
        let target = color(self.target, [0.0; 4]);
        self.draw(pipeline, target, &[(0, slot), (1, sampler)], (lod, 0.5))
            .unwrap();
        self.gpu.read_image(self.target).unwrap()[..4]
            .try_into()
            .unwrap()
    }
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// An 8x8 image with levels in red (uploaded), green (uploaded), blue
/// (rendered) and white (uploaded).
fn four_levels(kit: &mut Kit) -> Handle {
    let image = mipped(&mut kit.gpu, Format::Rgba8Unorm, 8, 4, USAGE).unwrap();
    let gpu = &mut kit.gpu;
    gpu.begin_frame().unwrap();
    gpu.upload_image(image, &solid(RED, 64)).unwrap();
    gpu.upload_image_level(image, Subresource { mip: 1, layer: 0 }, &solid(GREEN, 16))
        .unwrap();
    gpu.upload_image_level(image, Subresource { mip: 3, layer: 0 }, &solid(WHITE, 1))
        .unwrap();
    gpu.end_frame().unwrap();
    let level_2 = PassColor {
        sub: Subresource { mip: 2, layer: 0 },
        ..color(image, [0.0; 4])
    };
    kit.draw(kit.flat, level_2, &[], (0.0, 0.0)).unwrap();
    image
}

#[test]
fn levels_are_uploaded_rendered_and_sampled() {
    let mut kit = Kit::new();
    let image = four_levels(&mut kit);
    let info = kit.gpu.image_info(image).unwrap();
    assert_eq!((info.mip_levels, info.width, info.height), (4, 8, 8));
    for (mip, texels, expected) in [(0, 64, RED), (1, 16, GREEN), (2, 4, BLUE), (3, 1, WHITE)] {
        let pixels = kit
            .gpu
            .read_image_level(image, Subresource { mip, layer: 0 })
            .unwrap();
        assert_eq!(pixels, solid(expected, texels), "level {mip}");
    }

    let all = kit.gpu.create_texture_slot(image).unwrap();
    let nearest = sampler(&mut kit.gpu, Filter::Nearest, SamplerExtra::default());
    let level = kit.level;
    for (lod, expected) in [
        (0.0, RED),
        (1.0, GREEN),
        (2.0, BLUE),
        (3.0, WHITE),
        (9.0, WHITE),
    ] {
        assert_eq!(kit.sample(level, all, nearest, lod), expected, "lod {lod}");
    }
    // Halfway between two levels, with a linear mip filter.
    let trilinear = sampler(
        &mut kit.gpu,
        Filter::Linear,
        SamplerExtra {
            mip: Filter::Linear,
            ..SamplerExtra::default()
        },
    );
    let mixed = kit.sample(level, all, trilinear, 0.5);
    assert!(
        mixed[0].abs_diff(128) <= 2 && mixed[1].abs_diff(128) <= 2 && mixed[2] == 0,
        "{mixed:?}"
    );
    // The level-of-detail range and the bias of the sampler.
    let from_2 = sampler(
        &mut kit.gpu,
        Filter::Nearest,
        SamplerExtra {
            lod_min: 2.0,
            ..SamplerExtra::default()
        },
    );
    assert_eq!(kit.sample(level, all, from_2, 0.0), BLUE);
    let up_to_1 = sampler(
        &mut kit.gpu,
        Filter::Nearest,
        SamplerExtra {
            lod_max: 1.0,
            lod_bias: 1.0,
            ..SamplerExtra::default()
        },
    );
    assert_eq!(kit.sample(level, all, up_to_1, 0.0), GREEN, "bias");
    assert_eq!(kit.sample(level, all, up_to_1, 3.0), GREEN, "lod_max");
    // A slot that shows levels 1 and 2 only: its level 0 is level 1.
    let middle = kit
        .gpu
        .create_texture_slot_view(
            image,
            &TextureView {
                base_mip: 1,
                mip_count: Some(2),
                layer: None,
            },
        )
        .unwrap();
    assert_eq!(kit.sample(level, middle, nearest, 0.0), GREEN);
    assert_eq!(kit.sample(level, middle, nearest, 5.0), BLUE);

    // Gradients: 4 texels per pixel is level 2. Then the implicit level of
    // detail: the 2x2 target shows the whole 8x8 texture.
    let grad = Kit::make(&mut kit.gpu, kit.vs, "grad");
    assert_eq!(kit.sample(grad, all, nearest, 0.5), BLUE);
    let implicit = Kit::make(&mut kit.gpu, kit.vs, "implicit");
    assert_eq!(kit.sample(implicit, all, nearest, 0.0), BLUE);
    no_validation_errors();
}

/// Level N is drawn from level N-1 of the same image, through a slot that
/// shows only level N-1.
#[test]
fn a_level_is_rendered_from_the_level_above() {
    let mut kit = Kit::new();
    let image = four_levels(&mut kit);
    let down = Kit::make(&mut kit.gpu, kit.vs, "down");
    let linear = sampler(&mut kit.gpu, Filter::Linear, SamplerExtra::default());
    let all = kit.gpu.create_texture_slot(image).unwrap();
    for mip in 1..4 {
        let above = kit
            .gpu
            .create_texture_slot_view(
                image,
                &TextureView {
                    base_mip: mip - 1,
                    mip_count: Some(1),
                    layer: None,
                },
            )
            .unwrap();
        let level = PassColor {
            sub: Subresource { mip, layer: 0 },
            load: LoadOp::DontCare,
            ..color(image, [0.0; 4])
        };
        // A slot that shows the level being drawn cannot be read.
        expect!(
            kit.draw(down, level, &[(0, all), (1, linear)], (0.0, 0.0)),
            InvalidState
        );
        kit.draw(down, level, &[(0, above), (1, linear)], (0.0, 0.0))
            .unwrap();
    }
    // Red, halved three times: 128, 64, 32.
    for (mip, expected) in [(1, 128u8), (2, 64), (3, 32)] {
        let pixels = kit
            .gpu
            .read_image_level(image, Subresource { mip, layer: 0 })
            .unwrap();
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[0].abs_diff(expected) <= 1 && p[1] == 0 && p[2] == 0),
            "level {mip}: {:?}",
            &pixels[..4]
        );
    }
    no_validation_errors();
}

#[test]
fn mip_levels_are_generated() {
    let mut kit = Kit::new();
    let image = mipped(&mut kit.gpu, Format::Rgba8Unorm, 8, 4, USAGE).unwrap();
    // Left half white, right half black.
    let mut texels = Vec::new();
    for _ in 0..8 {
        texels.extend(solid(WHITE, 4));
        texels.extend(solid([0, 0, 0, 255], 4));
    }
    let gpu = &mut kit.gpu;
    gpu.begin_frame().unwrap();
    gpu.upload_image(image, &texels).unwrap();
    gpu.generate_mipmaps(image).unwrap();
    gpu.end_frame().unwrap();
    let level_2 = gpu
        .read_image_level(image, Subresource { mip: 2, layer: 0 })
        .unwrap();
    assert_eq!(&level_2[..4], WHITE);
    assert_eq!(&level_2[4..8], [0, 0, 0, 255]);
    let level_3 = gpu
        .read_image_level(image, Subresource { mip: 3, layer: 0 })
        .unwrap();
    assert!(
        level_3[0].abs_diff(128) <= 1 && level_3[3] == 255,
        "{level_3:?}"
    );
    // The image is back in the layout it is sampled in.
    let all = kit.gpu.create_texture_slot(image).unwrap();
    let nearest = sampler(&mut kit.gpu, Filter::Nearest, SamplerExtra::default());
    let level = kit.level;
    let grey = kit.sample(level, all, nearest, 3.0);
    assert!(grey[0].abs_diff(128) <= 1, "{grey:?}");
    no_validation_errors();
}

#[test]
fn mip_and_sampler_misuse() {
    let mut kit = Kit::new();
    let image = four_levels(&mut kit);
    let gpu = &mut kit.gpu;

    // An 8x8 image has at most 4 levels.
    expect!(
        mipped(gpu, Format::Rgba8Unorm, 8, 0, USAGE),
        InvalidArgument
    );
    expect!(
        mipped(gpu, Format::Rgba8Unorm, 8, 5, USAGE),
        InvalidArgument
    );

    // Slots: levels the image does not have.
    let view = |base_mip, mip_count, layer| TextureView {
        base_mip,
        mip_count,
        layer,
    };
    expect!(
        gpu.create_texture_slot_view(image, &view(4, None, None)),
        OutOfRange
    );
    expect!(
        gpu.create_texture_slot_view(image, &view(2, Some(3), None)),
        OutOfRange
    );
    expect!(
        gpu.create_texture_slot_view(image, &view(0, Some(0), None)),
        OutOfRange
    );
    expect!(
        gpu.create_texture_slot_view(image, &view(0, None, Some(1))),
        OutOfRange
    );
    expect!(
        gpu.create_texture_slot_view(image, &view(u32::MAX, Some(2), None)),
        OutOfRange
    );

    // Samplers.
    let bad = |gpu: &mut Gpu, extra| {
        gpu.create_sampler_with(
            &SamplerDesc {
                mag: Filter::Linear,
                min: Filter::Linear,
                address_u: AddressMode::ClampToBorder,
                address_v: AddressMode::ClampToBorder,
            },
            &extra,
        )
    };
    let defaults = SamplerExtra::default();
    for extra in [
        SamplerExtra {
            lod_min: 2.0,
            lod_max: 1.0,
            ..defaults
        },
        SamplerExtra {
            lod_min: -1.0,
            ..defaults
        },
        SamplerExtra {
            lod_max: f32::NAN,
            ..defaults
        },
        SamplerExtra {
            lod_bias: f32::INFINITY,
            ..defaults
        },
        SamplerExtra {
            lod_bias: 1.0e6,
            ..defaults
        },
        SamplerExtra {
            anisotropy: 0.5,
            ..defaults
        },
        SamplerExtra {
            anisotropy: f32::NAN,
            ..defaults
        },
    ] {
        expect!(bad(gpu, extra), InvalidArgument);
    }
    // More anisotropy than the device has is cut down, not refused.
    bad(
        gpu,
        SamplerExtra {
            anisotropy: 4096.0,
            ..defaults
        },
    )
    .unwrap();
    let border = bad(
        gpu,
        SamplerExtra {
            border: BorderColor::OpaqueWhite,
            ..defaults
        },
    )
    .unwrap();

    let small = image_with(gpu, Format::Rgba8Unorm, 4, USAGE);
    let no_copy = mipped(
        gpu,
        Format::Rgba8Unorm,
        8,
        4,
        ImageUsage::SAMPLED | ImageUsage::COLOR_TARGET,
    )
    .unwrap();
    let integers = mipped(
        gpu,
        Format::Rgba8Uint,
        8,
        4,
        ImageUsage::SAMPLED | ImageUsage::COPY_SRC | ImageUsage::COPY_DST,
    )
    .unwrap();

    gpu.begin_frame().unwrap();
    // Uploads: a level the image does not have, and the size of another level.
    expect!(
        gpu.upload_image_level(image, Subresource { mip: 4, layer: 0 }, &solid(RED, 1)),
        OutOfRange
    );
    expect!(
        gpu.upload_image_level(image, Subresource { mip: 0, layer: 1 }, &solid(RED, 64)),
        OutOfRange
    );
    expect!(
        gpu.upload_image_level(image, Subresource { mip: 1, layer: 0 }, &solid(RED, 64)),
        Mismatch
    );
    gpu.upload_image(integers, &solid([7, 7, 7, 7], 64))
        .unwrap();
    // Generating: needs the copy usages and a format the device can blit
    // with linear filtering. Vulkan leaves the latter to the device, also
    // for an integer format, so the expectation is what the device says.
    expect!(gpu.generate_mipmaps(no_copy), InvalidArgument);
    let caps = gpu.format_caps(Format::Rgba8Uint);
    if caps.blit_src && caps.blit_dst && caps.filter_linear {
        gpu.generate_mipmaps(integers).unwrap();
    } else {
        expect!(gpu.generate_mipmaps(integers), Unsupported);
    }
    // Passes: a level the image does not have; levels of different sizes.
    let at = |image, mip| PassColor {
        sub: Subresource { mip, layer: 0 },
        ..color(image, [0.0; 4])
    };
    let pass = |color| PassDesc { color, depth: None };
    expect!(gpu.begin_pass_desc(&pass(vec![at(image, 4)])), OutOfRange);
    expect!(
        gpu.begin_pass_desc(&pass(vec![at(image, 0), at(small, 0)])),
        Mismatch
    );
    // Level 1 of the 8x8 image is as large as the 4x4 one.
    gpu.begin_pass_desc(&pass(vec![at(image, 1), at(small, 0)]))
        .unwrap();
    expect!(gpu.generate_mipmaps(small), InvalidState);
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();

    // A border colour is a floating-point colour: it works with a
    // normalized texture and is refused with an integer one.
    let all = kit.gpu.create_texture_slot(image).unwrap();
    let level = kit.level;
    assert_eq!(kit.sample(level, all, border, 0.0), RED, "inside");
    let target = color(kit.target, [0.0; 4]);
    kit.draw(level, target, &[(0, all), (1, border)], (0.0, 2.0))
        .unwrap();
    assert_eq!(
        &kit.gpu.read_image(kit.target).unwrap()[..4],
        WHITE,
        "outside"
    );
    let integer_slot = kit.gpu.create_texture_slot(integers).unwrap();
    let on_integers = Kit::make(&mut kit.gpu, kit.vs, "integers");
    let nearest_border = kit
        .gpu
        .create_sampler_with(
            &SamplerDesc {
                mag: Filter::Nearest,
                min: Filter::Nearest,
                address_u: AddressMode::ClampToBorder,
                address_v: AddressMode::ClampToEdge,
            },
            &defaults,
        )
        .unwrap();
    expect!(
        kit.draw(
            on_integers,
            target,
            &[(2, integer_slot), (1, nearest_border)],
            (0.0, 0.0)
        ),
        Mismatch
    );
    let nearest = sampler(&mut kit.gpu, Filter::Nearest, defaults);
    kit.draw(
        on_integers,
        target,
        &[(2, integer_slot), (1, nearest)],
        (0.0, 0.0),
    )
    .unwrap();
    assert_eq!(kit.gpu.read_image(kit.target).unwrap()[0], 7);
    // A linear mip filter is a linear filter.
    let nearest_trilinear = sampler(
        &mut kit.gpu,
        Filter::Nearest,
        SamplerExtra {
            mip: Filter::Linear,
            ..defaults
        },
    );
    expect!(
        kit.draw(
            on_integers,
            target,
            &[(2, integer_slot), (1, nearest_trilinear)],
            (0.0, 0.0)
        ),
        Mismatch
    );
    no_validation_errors();
}

fn image_with(gpu: &mut Gpu, format: Format, size: u32, usage: ImageUsage) -> Handle {
    mipped(gpu, format, size, 1, usage).unwrap()
}
