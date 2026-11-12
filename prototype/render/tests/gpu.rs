//! Renderer tests that need a Vulkan device (test layer 4: color values).
//!
//! Every scenario runs on each device listed in `HIZUKURI_TEST_DEVICES`
//! (comma separated, default `lavapipe`, so that the tests do the same on
//! every machine):
//!
//! - `gpu`: a hardware device visible to the Vulkan loader in the current
//!   environment.
//! - `lavapipe`: Mesa's software rasterizer. The Vulkan loader only sees it
//!   when its manifest is listed in `VK_DRIVER_FILES` (or nothing is), so
//!   this test re-executes itself with `VK_DRIVER_FILES` pointing at the
//!   lavapipe manifest.
//!
//! A listed device that is not available is a failure, not a skip.

use std::process::Command;

use proto_color::{
    AlphaMode, CompositeSpace, DecodeTransform, EncodeTransform, ImageDescription, blend_over,
    quantize_unorm,
};
use proto_render::{
    Background, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_XRGB8888, DeviceKind, DeviceSelector, Element,
    Frame, OutputFormat, PixelFormat, Rect, RenderError, Renderer, RendererOptions, TargetDesc,
    TargetId, TextureDesc, TextureId, find_icd_manifest,
};

const SPACE: CompositeSpace = CompositeSpace::BT2020_LINEAR;
const CHILD_ENV: &str = "HIZUKURI_RENDER_TEST_CHILD";

fn device_enabled(name: &str) -> bool {
    let list = std::env::var("HIZUKURI_TEST_DEVICES").unwrap_or_else(|_| "lavapipe".into());
    list.split(',').any(|d| d.trim() == name)
}

fn validation_requested() -> bool {
    std::env::var("HIZUKURI_TEST_VALIDATION").is_ok_and(|v| v == "1")
}

#[test]
fn gpu() {
    if !device_enabled("gpu") {
        eprintln!("skipped: gpu is not in HIZUKURI_TEST_DEVICES");
        return;
    }
    run_all(DeviceSelector::Gpu);
}

#[test]
fn lavapipe() {
    if !device_enabled("lavapipe") {
        eprintln!("skipped: lavapipe is not in HIZUKURI_TEST_DEVICES");
        return;
    }
    let manifest = find_icd_manifest("lvp").expect("the lavapipe ICD manifest is installed");
    let exe = std::env::current_exe().expect("test executable path");
    let output = Command::new(exe)
        .args(["--exact", "lavapipe_child", "--ignored", "--nocapture"])
        .env("VK_DRIVER_FILES", &manifest)
        .env(CHILD_ENV, "1")
        .output()
        .expect("re-executing the test binary");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "lavapipe child run failed");
}

/// Runs inside the process started by [`lavapipe`].
#[test]
#[ignore = "started by the `lavapipe` test with VK_DRIVER_FILES set"]
fn lavapipe_child() {
    assert!(
        std::env::var(CHILD_ENV).is_ok(),
        "run the `lavapipe` test instead"
    );
    run_all(DeviceSelector::Cpu);
}

fn run_all(selector: DeviceSelector) {
    let expect_cpu = selector == DeviceSelector::Cpu;
    let mut renderer = Renderer::new(&RendererOptions {
        device: selector,
        validation: validation_requested(),
    })
    .expect("creating the renderer");
    let info = renderer.device_info().clone();
    println!(
        "device: {} ({}, driver {} {})",
        info.name,
        info.kind.as_str(),
        info.driver_name,
        info.driver_info
    );
    assert_eq!(info.kind == DeviceKind::Cpu, expect_cpu);

    solid_fills_match_cpu_reference(&mut renderer);
    unmanaged_fill_round_trips_on_gamma22_output(&mut renderer);
    translucent_surfaces_blend_in_linear_light(&mut renderer);
    opaque_formats_ignore_the_alpha_byte(&mut renderer);
    alpha_modes_agree(&mut renderer);
    stride_and_source_rectangles(&mut renderer);
    opacity_scales_premultiplied_color(&mut renderer);
    errors_are_reported(&mut renderer);
    bgra_target_and_readback_into_strided_memory(&mut renderer);
    scanout_target_exports_a_dmabuf_with_the_frame(&mut renderer);

    if let Some(errors) = renderer.validation_errors() {
        assert_eq!(errors, 0, "Vulkan validation errors were reported");
        println!(
            "validation: 0 errors, {:?} warnings",
            renderer.validation_warnings()
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const BLACK: Background = Background {
    color: [0.0, 0.0, 0.0, 1.0],
    image_description: ImageDescription::SRGB_GAMMA22,
};

fn solid_texture(
    renderer: &mut Renderer,
    size: u32,
    rgba: [u8; 4],
    image_description: ImageDescription,
    alpha_mode: AlphaMode,
) -> TextureId {
    let id = renderer
        .create_texture(&TextureDesc {
            width: size,
            height: size,
            format: PixelFormat::Rgba8,
            image_description,
            alpha_mode,
        })
        .unwrap();
    let data: Vec<u8> = rgba.repeat((size * size) as usize);
    renderer.upload_texture(id, &data, size * 4).unwrap();
    id
}

fn target(
    renderer: &mut Renderer,
    width: u32,
    height: u32,
    format: OutputFormat,
    image_description: ImageDescription,
) -> TargetId {
    renderer
        .create_target(&TargetDesc {
            width,
            height,
            format,
            image_description,
            composite_space: SPACE,
        })
        .unwrap()
}

fn normalized(rgba: [u8; 4]) -> [f32; 4] {
    rgba.map(|v| v as f32 / 255.0)
}

/// Tolerance for values in the compositing buffer. Half floats carry 11
/// significant bits (relative error 2^-11 when rounding to nearest); allow
/// twice that, since the rounding mode is up to the implementation, plus a
/// small absolute term for values near zero.
fn assert_composite_close(actual: [f32; 4], expected: [f32; 4], context: &str) {
    for i in 0..4 {
        let tolerance = 2e-5 + expected[i].abs() * 1e-3;
        assert!(
            (actual[i] - expected[i]).abs() <= tolerance,
            "{context}: composite {actual:?} vs expected {expected:?}"
        );
    }
}

fn code_difference(actual: [u16; 4], expected: [u32; 3]) -> u32 {
    (0..3)
        .map(|i| (actual[i] as i64 - expected[i] as i64).unsigned_abs() as u32)
        .max()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// A grid of solid squares for many combinations of surface and output
/// image descriptions. Checks, per square:
///
/// 1. the compositing buffer against `DecodeTransform` on the CPU,
/// 2. the encoded output against `EncodeTransform` applied to the value read
///    back from the compositing buffer (within 1 code value): this is the
///    encode pass in isolation,
/// 3. the encoded output against the full CPU pipeline, with the tolerance
///    explained at `end_to_end_tolerance`.
fn solid_fills_match_cpu_reference(renderer: &mut Renderer) {
    const COLORS: [[u8; 4]; 12] = [
        [0, 0, 0, 255],
        [255, 255, 255, 255],
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [128, 128, 128, 255],
        [1, 1, 1, 255],
        [10, 20, 30, 255],
        [200, 100, 50, 255],
        [254, 128, 3, 255],
        [64, 0, 128, 255],
        [30, 220, 240, 255],
    ];
    const CELL: u32 = 8;
    let sources = [
        ImageDescription::SRGB_GAMMA22,
        ImageDescription::SRGB_PIECEWISE,
        ImageDescription::DISPLAY_P3,
        ImageDescription::ADOBE_RGB,
        ImageDescription::BT2020_GAMMA24,
        ImageDescription::BT2020_PQ,
    ];
    let outputs = [
        (ImageDescription::SRGB_GAMMA22, OutputFormat::Rgba8Unorm),
        (ImageDescription::SRGB_PIECEWISE, OutputFormat::Rgba8Unorm),
        (ImageDescription::DISPLAY_P3, OutputFormat::Rgba8Unorm),
        (ImageDescription::DISPLAY_P3, OutputFormat::Rgba16Unorm),
        (ImageDescription::ADOBE_RGB, OutputFormat::Rgba8Unorm),
        (ImageDescription::BT2020_PQ, OutputFormat::Rgba16Unorm),
    ];

    for source in sources {
        let textures: Vec<TextureId> = COLORS
            .iter()
            .map(|c| {
                solid_texture(
                    renderer,
                    CELL,
                    *c,
                    source,
                    AlphaMode::PremultipliedElectrical,
                )
            })
            .collect();
        let elements: Vec<Element> = textures
            .iter()
            .enumerate()
            .map(|(i, texture)| Element {
                texture: *texture,
                dst: Rect::new((i as u32 * CELL) as f32, 0.0, CELL as f32, CELL as f32),
                src: None,
                opacity: 1.0,
            })
            .collect();
        let decode =
            DecodeTransform::new(&source, AlphaMode::PremultipliedElectrical, &SPACE).unwrap();

        for (output, format) in outputs {
            let width = CELL * COLORS.len() as u32;
            let id = target(renderer, width, CELL, format, output);
            renderer
                .render(
                    id,
                    &Frame {
                        background: BLACK,
                        elements: &elements,
                    },
                )
                .unwrap();
            let composite = renderer.read_composite(id).unwrap();
            let encoded = renderer.read_encoded(id).unwrap();
            let encode = EncodeTransform::new(&SPACE, &output).unwrap();
            let bits = format.bits();

            let mut worst_end_to_end = 0;
            for (i, color) in COLORS.iter().enumerate() {
                let context = format!(
                    "{:?} on {:?} ({}) color {color:?}",
                    source.name(),
                    output.name(),
                    format.name()
                );
                let expected_composite = decode.apply(normalized(*color));
                // Probe the center and two corners of the square.
                for (dx, dy) in [(CELL / 2, CELL / 2), (0, 0), (CELL - 1, CELL - 1)] {
                    let (x, y) = (i as u32 * CELL + dx, dy);
                    let c = composite.pixel(x, y);
                    assert_composite_close(c, expected_composite, &context);

                    let px = encoded.pixel(x, y);
                    assert_eq!(px[3], format.max_code(), "{context}: alpha");

                    let from_readback = encode
                        .apply([c[0], c[1], c[2]])
                        .map(|v| quantize_unorm(v, bits));
                    assert!(
                        code_difference(px, from_readback) <= 1,
                        "{context}: encoded {px:?} vs {from_readback:?} from the read back \
                         compositing buffer"
                    );

                    let end_to_end = encode
                        .apply([
                            expected_composite[0],
                            expected_composite[1],
                            expected_composite[2],
                        ])
                        .map(|v| quantize_unorm(v, bits));
                    let difference = code_difference(px, end_to_end);
                    worst_end_to_end = worst_end_to_end.max(difference);
                    let tolerance = end_to_end_tolerance(&output, format);
                    assert!(
                        difference <= tolerance,
                        "{context}: encoded {px:?} vs {end_to_end:?} from the CPU pipeline \
                         (tolerance {tolerance})"
                    );
                }
            }
            println!(
                "solid fills: {:>14} -> {:>14} {:>6}: worst end-to-end difference {} code value(s)",
                source.name().unwrap(),
                output.name().unwrap(),
                format.name(),
                worst_end_to_end
            );
            renderer.destroy_target(id);
        }
        for texture in textures {
            renderer.destroy_texture(texture);
        }
    }
}

/// Tolerance, in code values, between the encoded output and the CPU
/// pipeline evaluated in f32.
///
/// The compositing buffer is RGBA16F in BT.2020. Rounding a channel to a
/// half float is a relative error of up to 2^-11. Converting back to
/// narrower primaries mixes the channels with large coefficients of
/// opposite sign, so a channel that should be (near) zero picks up a small
/// absolute error from its bright neighbours (around 1e-4 in linear light
/// for a saturated color). The transfer functions are steep near zero, which
/// turns that into more than one code value:
///
/// - pure power functions (gamma 2.2 / 2.4) have unbounded slope at zero:
///   1e-4 linear is about 4 code values at 8 bit,
/// - the piecewise sRGB function has slope 12.92 at zero: within 1 code
///   value at 8 bit,
/// - 16 bit outputs see the same error at 257 times the resolution.
///
/// Worst cases: 3 code
/// values for 8 bit power-function outputs, 1 for 8 bit piecewise sRGB, 39
/// for 16 bit Display P3, 5 for 16 bit PQ. The values below add a small
/// margin. They document a property of the 16 bit float
/// compositing buffer, not slack in the shaders: step 2 of the caller checks
/// the encode pass against the read back buffer within 1 code value.
fn end_to_end_tolerance(output: &ImageDescription, format: OutputFormat) -> u32 {
    use proto_color::TransferFunction as Tf;
    match (format, output.transfer) {
        (OutputFormat::Rgba8Unorm | OutputFormat::Bgra8Unorm, Tf::SrgbPiecewise) => 1,
        (OutputFormat::Rgba8Unorm | OutputFormat::Bgra8Unorm, _) => 4,
        (OutputFormat::Rgba16Unorm, Tf::Pq) => 8,
        (OutputFormat::Rgba16Unorm, _) => 48,
    }
}

/// Content without color information (BT.709 + gamma 2.2) on an output of
/// the same kind: gray levels come back exactly, every level of them.
fn unmanaged_fill_round_trips_on_gamma22_output(renderer: &mut Renderer) {
    let desc = ImageDescription::SRGB_GAMMA22;
    let id = renderer
        .create_texture(&TextureDesc {
            width: 256,
            height: 1,
            format: PixelFormat::Bgra8,
            image_description: desc,
            alpha_mode: AlphaMode::PremultipliedElectrical,
        })
        .unwrap();
    let data: Vec<u8> = (0..=255u8).flat_map(|v| [v, v, v, 255]).collect();
    renderer.upload_texture(id, &data, 256 * 4).unwrap();
    let out = target(renderer, 256, 1, OutputFormat::Rgba8Unorm, desc);
    renderer
        .render(
            out,
            &Frame {
                background: BLACK,
                elements: &[Element {
                    texture: id,
                    dst: Rect::new(0.0, 0.0, 256.0, 1.0),
                    src: None,
                    opacity: 1.0,
                }],
            },
        )
        .unwrap();
    let encoded = renderer.read_encoded(out).unwrap();
    let mut worst = 0;
    for v in 0..=255u32 {
        let px = encoded.pixel(v, 0);
        worst = worst.max(code_difference(px, [v; 3]));
    }
    println!("unmanaged gray ramp on a gamma 2.2 output: worst difference {worst} code value(s)");
    assert_eq!(worst, 0, "a gray ramp must round trip exactly");
    renderer.destroy_target(out);
    renderer.destroy_texture(id);
}

/// 50% white over black, and 50% red over opaque green: the result is the
/// linear light blend, not the blend of encoded values.
fn translucent_surfaces_blend_in_linear_light(renderer: &mut Renderer) {
    let desc = ImageDescription::SRGB_GAMMA22;
    let mode = AlphaMode::PremultipliedElectrical;
    let black = solid_texture(renderer, 16, [0, 0, 0, 255], desc, mode);
    let green = solid_texture(renderer, 16, [0, 255, 0, 255], desc, mode);
    // Premultiplied: 50% white is (128, 128, 128, 128); 50% red is (128, 0, 0, 128).
    let half_white = solid_texture(renderer, 16, [128, 128, 128, 128], desc, mode);
    let half_red = solid_texture(renderer, 16, [128, 0, 0, 128], desc, mode);

    let out = target(renderer, 32, 16, OutputFormat::Rgba8Unorm, desc);
    let quad = |texture, x: f32| Element {
        texture,
        dst: Rect::new(x, 0.0, 16.0, 16.0),
        src: None,
        opacity: 1.0,
    };
    renderer
        .render(
            out,
            &Frame {
                background: BLACK,
                elements: &[
                    quad(black, 0.0),
                    quad(half_white, 0.0),
                    quad(green, 16.0),
                    quad(half_red, 16.0),
                ],
            },
        )
        .unwrap();
    let composite = renderer.read_composite(out).unwrap();
    let encoded = renderer.read_encoded(out).unwrap();

    let decode = DecodeTransform::new(&desc, mode, &SPACE).unwrap();
    let encode = EncodeTransform::new(&SPACE, &desc).unwrap();
    let cases = [
        (8, [0, 0, 0, 255], [128, 128, 128, 128]),
        (24, [0, 255, 0, 255], [128, 0, 0, 128]),
    ];
    for (x, below, above) in cases {
        let expected = blend_over(
            decode.apply(normalized(below)),
            decode.apply(normalized(above)),
        );
        assert_composite_close(composite.pixel(x, 8), expected, "linear blend");
        let expected_code = encode
            .apply([expected[0], expected[1], expected[2]])
            .map(|v| quantize_unorm(v, 8));
        let px = encoded.pixel(x, 8);
        assert!(
            code_difference(px, expected_code) <= 1,
            "blend at x={x}: {px:?} vs {expected_code:?}"
        );
        println!("blend {above:?} over {below:?}: encoded {px:?}, expected {expected_code:?}");
    }

    // The well known numbers: 50% white over black in linear light is
    // (128/128 = 1.0 white) * 0.502 coverage -> 0.502^(1/2.2) = 0.731 -> 186,
    // where blending the encoded values would give 128.
    let px = encoded.pixel(8, 8);
    assert!(
        (px[0] as i32 - 186).abs() <= 1,
        "expected about 186, got {px:?}"
    );
    assert_ne!(px[0], 128);

    renderer.destroy_target(out);
    for texture in [black, green, half_white, half_red] {
        renderer.destroy_texture(texture);
    }
}

/// `x` formats: the fourth byte is undefined and must not act as alpha.
fn opaque_formats_ignore_the_alpha_byte(renderer: &mut Renderer) {
    let desc = ImageDescription::SRGB_GAMMA22;
    let out = target(renderer, 8, 8, OutputFormat::Rgba8Unorm, desc);
    for (format, bytes, expected) in [
        // Memory order B, G, R, X.
        (PixelFormat::Bgrx8, [10u8, 20, 200, 0], [200u32, 20, 10]),
        // Memory order R, G, B, X.
        (PixelFormat::Rgbx8, [200u8, 20, 10, 0], [200u32, 20, 10]),
        // With a real alpha channel, memory order B, G, R, A.
        (PixelFormat::Bgra8, [10u8, 20, 200, 255], [200u32, 20, 10]),
    ] {
        let id = renderer
            .create_texture(&TextureDesc {
                width: 8,
                height: 8,
                format,
                image_description: desc,
                alpha_mode: AlphaMode::PremultipliedElectrical,
            })
            .unwrap();
        renderer.upload_texture(id, &bytes.repeat(64), 32).unwrap();
        renderer
            .render(
                out,
                &Frame {
                    background: BLACK,
                    elements: &[Element {
                        texture: id,
                        dst: Rect::new(0.0, 0.0, 8.0, 8.0),
                        src: None,
                        opacity: 1.0,
                    }],
                },
            )
            .unwrap();
        let px = renderer.read_encoded(out).unwrap().pixel(4, 4);
        assert!(
            code_difference(px, expected) <= 1,
            "{format:?}: {px:?} vs {expected:?}"
        );
        renderer.destroy_texture(id);
    }
    renderer.destroy_target(out);
}

/// The same translucent color expressed in the three alpha modes produces
/// the same compositing value.
fn alpha_modes_agree(renderer: &mut Renderer) {
    let desc = ImageDescription::SRGB_PIECEWISE;
    let tf = desc.transfer;
    let (rgb, alpha) = ([0.8f32, 0.4, 0.2], 128.0 / 255.0);
    let to_byte = |v: f32| (v * 255.0).round() as u8;

    let straight = [to_byte(rgb[0]), to_byte(rgb[1]), to_byte(rgb[2]), 128];
    let electrical = [
        to_byte(rgb[0] * alpha),
        to_byte(rgb[1] * alpha),
        to_byte(rgb[2] * alpha),
        128,
    ];
    let optical_channel = |c: f32| to_byte(tf.encode(tf.decode(c) * alpha));
    let optical = [
        optical_channel(rgb[0]),
        optical_channel(rgb[1]),
        optical_channel(rgb[2]),
        128,
    ];

    let out = target(renderer, 24, 8, OutputFormat::Rgba8Unorm, desc);
    let cases = [
        (AlphaMode::Straight, straight),
        (AlphaMode::PremultipliedElectrical, electrical),
        (AlphaMode::PremultipliedOptical, optical),
    ];
    let textures: Vec<TextureId> = cases
        .iter()
        .map(|(mode, bytes)| solid_texture(renderer, 8, *bytes, desc, *mode))
        .collect();
    let elements: Vec<Element> = textures
        .iter()
        .enumerate()
        .map(|(i, texture)| Element {
            texture: *texture,
            dst: Rect::new(i as f32 * 8.0, 0.0, 8.0, 8.0),
            src: None,
            opacity: 1.0,
        })
        .collect();
    renderer
        .render(
            out,
            &Frame {
                background: Background {
                    color: [0.0, 0.0, 0.0, 0.0],
                    image_description: desc,
                },
                elements: &elements,
            },
        )
        .unwrap();
    let composite = renderer.read_composite(out).unwrap();

    for (i, (mode, bytes)) in cases.iter().enumerate() {
        // Each mode against its own CPU reference, exactly as uploaded.
        let expected = DecodeTransform::new(&desc, *mode, &SPACE)
            .unwrap()
            .apply(normalized(*bytes));
        let actual = composite.pixel(i as u32 * 8 + 4, 4);
        assert_composite_close(actual, expected, &format!("{mode:?}"));
    }
    // And against each other: the inputs were rounded to 8 bits separately,
    // so allow the error of one code value through the decode.
    let reference = composite.pixel(4, 4);
    for i in 1..3 {
        let actual = composite.pixel(i * 8 + 4, 4);
        for c in 0..4 {
            assert!(
                (actual[c] - reference[c]).abs() < 0.01,
                "alpha modes disagree: {actual:?} vs {reference:?}"
            );
        }
    }

    renderer.destroy_target(out);
    for texture in textures {
        renderer.destroy_texture(texture);
    }
}

/// Rows padded beyond the width (stride) upload correctly, and a source
/// rectangle selects part of a texture.
fn stride_and_source_rectangles(renderer: &mut Renderer) {
    let desc = ImageDescription::SRGB_GAMMA22;
    // 4x2 texture, 6 pixels of stride; padding bytes are 0xee and must not
    // show up. Left half red, right half blue.
    let (width, height, stride_px) = (4u32, 2u32, 6u32);
    let mut data = Vec::new();
    for _ in 0..height {
        for x in 0..stride_px {
            if x >= width {
                data.extend_from_slice(&[0xee; 4]);
            } else if x < 2 {
                data.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                data.extend_from_slice(&[0, 0, 255, 255]);
            }
        }
    }
    let id = renderer
        .create_texture(&TextureDesc {
            width,
            height,
            format: PixelFormat::Rgba8,
            image_description: desc,
            alpha_mode: AlphaMode::PremultipliedElectrical,
        })
        .unwrap();
    renderer.upload_texture(id, &data, stride_px * 4).unwrap();

    let out = target(renderer, 8, 4, OutputFormat::Rgba8Unorm, desc);
    renderer
        .render(
            out,
            &Frame {
                background: BLACK,
                elements: &[
                    // Whole texture, unscaled, at the top left.
                    Element {
                        texture: id,
                        dst: Rect::new(0.0, 0.0, 4.0, 2.0),
                        src: None,
                        opacity: 1.0,
                    },
                    // Only the blue half, at the bottom right.
                    Element {
                        texture: id,
                        dst: Rect::new(6.0, 2.0, 2.0, 2.0),
                        src: Some(Rect::new(2.0, 0.0, 2.0, 2.0)),
                        opacity: 1.0,
                    },
                ],
            },
        )
        .unwrap();
    let encoded = renderer.read_encoded(out).unwrap();
    let red = [255u32, 0, 0];
    let blue = [0u32, 0, 255];
    let black = [0u32, 0, 0];
    // Saturated primaries leave up to a few code values in the other
    // channels (see end_to_end_tolerance).
    let tolerance = end_to_end_tolerance(&desc, OutputFormat::Rgba8Unorm);
    for (x, y, expected) in [
        (0, 0, red),
        (1, 1, red),
        (2, 0, blue),
        (3, 1, blue),
        (4, 0, black),
        (0, 2, black),
        (5, 3, black),
        (6, 2, blue),
        (7, 3, blue),
    ] {
        let px = encoded.pixel(x, y);
        assert!(
            code_difference(px, expected) <= tolerance,
            "pixel ({x}, {y}): {px:?} vs {expected:?}"
        );
    }
    renderer.destroy_target(out);
    renderer.destroy_texture(id);
}

/// Element opacity multiplies the premultiplied compositing value.
fn opacity_scales_premultiplied_color(renderer: &mut Renderer) {
    let desc = ImageDescription::SRGB_GAMMA22;
    let mode = AlphaMode::PremultipliedElectrical;
    let white = solid_texture(renderer, 8, [255, 255, 255, 255], desc, mode);
    let out = target(renderer, 8, 8, OutputFormat::Rgba8Unorm, desc);
    renderer
        .render(
            out,
            &Frame {
                background: Background {
                    color: [0.0, 0.0, 0.0, 0.0],
                    image_description: desc,
                },
                elements: &[Element {
                    texture: white,
                    dst: Rect::new(0.0, 0.0, 8.0, 8.0),
                    src: None,
                    opacity: 0.25,
                }],
            },
        )
        .unwrap();
    let composite = renderer.read_composite(out).unwrap();
    assert_composite_close(composite.pixel(4, 4), [0.25, 0.25, 0.25, 0.25], "opacity");
    renderer.destroy_target(out);
    renderer.destroy_texture(white);
}

fn errors_are_reported(renderer: &mut Renderer) {
    use proto_render::RenderError;
    let desc = TextureDesc {
        width: 4,
        height: 4,
        format: PixelFormat::Bgra8,
        image_description: ImageDescription::SRGB_GAMMA22,
        alpha_mode: AlphaMode::PremultipliedElectrical,
    };
    assert!(matches!(
        renderer.create_texture(&TextureDesc { width: 0, ..desc }),
        Err(RenderError::InvalidSize { .. })
    ));
    let id = renderer.create_texture(&desc).unwrap();
    assert!(matches!(
        renderer.upload_texture(id, &[0; 16], 16),
        Err(RenderError::DataTooShort { .. })
    ));
    assert!(matches!(
        renderer.upload_texture(id, &[0; 64], 8),
        Err(RenderError::InvalidStride { .. })
    ));

    let out = target(
        renderer,
        4,
        4,
        OutputFormat::Rgba8Unorm,
        ImageDescription::SRGB_GAMMA22,
    );
    assert!(matches!(
        renderer.read_encoded(out),
        Err(RenderError::TargetNotRendered)
    ));
    // A texture without contents is skipped, not drawn as garbage.
    renderer
        .render(
            out,
            &Frame {
                background: Background {
                    color: [1.0, 1.0, 1.0, 1.0],
                    image_description: ImageDescription::SRGB_GAMMA22,
                },
                elements: &[Element {
                    texture: id,
                    dst: Rect::new(0.0, 0.0, 4.0, 4.0),
                    src: None,
                    opacity: 1.0,
                }],
            },
        )
        .unwrap();
    assert_eq!(
        renderer.read_encoded(out).unwrap().pixel(2, 2),
        [255, 255, 255, 255]
    );

    renderer.destroy_texture(id);
    renderer.destroy_target(out);
    assert!(matches!(
        renderer.read_encoded(out),
        Err(RenderError::UnknownTarget)
    ));
}

/// The format KMS planes take (`bgra8`, DRM XRGB8888): bytes come out as
/// B, G, R, A, and can be read back into memory whose rows are further
/// apart than the image is wide, as a dumb buffer's are.
fn bgra_target_and_readback_into_strided_memory(renderer: &mut Renderer) {
    let desc = ImageDescription::SRGB_GAMMA22;
    let texture = solid_texture(
        renderer,
        4,
        [200, 100, 50, 255],
        desc,
        AlphaMode::PremultipliedElectrical,
    );
    let out = target(renderer, 6, 4, OutputFormat::Bgra8Unorm, desc);
    renderer
        .render(
            out,
            &Frame {
                background: Background {
                    color: [0.0, 0.0, 1.0, 1.0],
                    image_description: desc,
                },
                elements: &[Element {
                    texture,
                    dst: Rect::new(0.0, 0.0, 4.0, 4.0),
                    src: None,
                    opacity: 1.0,
                }],
            },
        )
        .unwrap();

    // Logical channel order is the same whatever the byte order.
    let image = renderer.read_encoded(out).unwrap();
    assert_eq!(image.format, OutputFormat::Bgra8Unorm);
    assert!(code_difference(image.pixel(1, 1), [200, 100, 50]) <= 1);
    assert!(code_difference(image.pixel(5, 3), [0, 0, 255]) <= 3);

    // Raw bytes into a buffer with 8 pixels of stride for 6 of width.
    let stride = 8 * 4;
    let mut memory = vec![0xeeu8; stride * 4];
    renderer
        .read_encoded_into(out, &mut memory, stride)
        .unwrap();
    for y in 0..4 {
        let row = &memory[y * stride..(y + 1) * stride];
        // B, G, R order in memory.
        assert!((row[0] as i32 - 50).abs() <= 1, "row {y}: {row:?}");
        assert!((row[1] as i32 - 100).abs() <= 1);
        assert!((row[2] as i32 - 200).abs() <= 1);
        // The blue background: B is the first byte.
        assert_eq!(row[5 * 4], 255);
        // Bytes past the end of the row are untouched.
        assert!(row[6 * 4..].iter().all(|b| *b == 0xee), "row {y}: {row:?}");
    }
    assert!(renderer.read_encoded_into(out, &mut memory, 8).is_err());
    assert!(
        renderer
            .read_encoded_into(out, &mut memory[..40], stride)
            .is_err()
    );

    renderer.destroy_target(out);
    renderer.destroy_texture(texture);
}

/// A scanout target is a dma-buf that someone else (KMS) reads. Map the
/// dma-buf like that someone would and check that the frame is in it.
fn scanout_target_exports_a_dmabuf_with_the_frame(renderer: &mut Renderer) {
    // The modifiers depend on the driver: lavapipe offers linear memory,
    // other drivers may offer only their own tiled layouts. The test uses
    // whatever the renderer says it can export.
    let offered = renderer.scanout_modifiers();
    let supported = renderer.supports_scanout_export();
    assert_eq!(supported, !offered.is_empty());
    println!("scanout export: supported={supported} modifiers={offered:x?}");
    let desc = TargetDesc {
        width: 64,
        height: 32,
        format: OutputFormat::Bgra8Unorm,
        image_description: ImageDescription::SRGB_GAMMA22,
        composite_space: SPACE,
    };
    // Consumers that take none of the modifiers the device offers: one with
    // a modifier no driver has, and one with linear memory where the device
    // does not offer it.
    let foreign = 0x00ff_ffff_ffff_fffe_u64;
    assert!(!offered.contains(&foreign));
    for allowed in [&[foreign][..], &[DRM_FORMAT_MOD_LINEAR][..]] {
        if allowed.iter().any(|m| offered.contains(m)) {
            continue;
        }
        assert!(
            matches!(
                renderer.create_scanout_target(&desc, allowed),
                Err(RenderError::ScanoutUnsupported)
            ),
            "a scanout target with modifiers {allowed:x?}, none of them offered"
        );
    }
    if !supported {
        // The CPU copy path is what such a device gets.
        return;
    }
    // Scanout targets are XRGB8888 only.
    assert!(matches!(
        renderer.create_scanout_target(
            &TargetDesc {
                format: OutputFormat::Rgba8Unorm,
                ..desc
            },
            &offered
        ),
        Err(RenderError::UnsupportedFormat(_))
    ));
    // Each offered modifier on its own: a consumer may take only some.
    for &modifier in &offered {
        let (out, plane) = renderer
            .create_scanout_target(&desc, &[modifier])
            .unwrap_or_else(|e| panic!("a scanout target with modifier {modifier:#x}: {e}"));
        assert_eq!(plane.modifier, modifier);
        renderer.destroy_target(out);
    }

    let (out, plane) = renderer.create_scanout_target(&desc, &offered).unwrap();
    assert_eq!(plane.fourcc, DRM_FORMAT_XRGB8888);
    assert!(
        offered.contains(&plane.modifier),
        "modifier {:#x}",
        plane.modifier
    );
    assert_eq!((plane.width, plane.height), (64, 32));
    assert!(plane.stride >= 64 * 4);
    println!(
        "scanout export: stride={} offset={} modifier={:#x}",
        plane.stride, plane.offset, plane.modifier
    );

    let texture = solid_texture(
        renderer,
        8,
        [10, 128, 240, 255],
        ImageDescription::SRGB_GAMMA22,
        AlphaMode::PremultipliedElectrical,
    );
    // Two frames with different contents: the second one reacquires the
    // image from its consumer.
    for (frame, x) in [(0, 0.0f32), (1, 32.0)] {
        renderer
            .render(
                out,
                &Frame {
                    background: Background {
                        color: [0.5, 0.5, 0.5, 1.0],
                        image_description: ImageDescription::SRGB_GAMMA22,
                    },
                    elements: &[Element {
                        texture,
                        dst: Rect::new(x, 8.0, 8.0, 8.0),
                        src: None,
                        opacity: 1.0,
                    }],
                },
            )
            .unwrap();

        // What the renderer reads back...
        let image = renderer.read_encoded(out).unwrap();
        let inside = (x as u32 + 4, 12);
        let outside = (if frame == 0 { 36 } else { 4 }, 12);
        assert!(code_difference(image.pixel(inside.0, inside.1), [10, 128, 240]) <= 3);
        assert!(code_difference(image.pixel(outside.0, outside.1), [128, 128, 128]) <= 1);

        // ...is what is in the dma-buf. Only linear memory can be read
        // row by row; a tiled layout is the driver's own. Whether a dma-buf
        // can be mapped is up to the driver that allocated it: lavapipe
        // allows it, amdgpu refuses (EPERM). Where it can be, compare.
        if plane.modifier != DRM_FORMAT_MOD_LINEAR {
            if frame == 0 {
                println!(
                    "scanout export: modifier {:#x} is not linear; contents checked through readback only",
                    plane.modifier
                );
            }
            continue;
        }
        let length = plane.offset as usize + plane.stride as usize * 32;
        // SAFETY: maps the dma-buf read-only for its used length; the GPU
        // is idle because render() waits for it.
        let mapped = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                length,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::SHARED,
                &plane.fd,
                0,
            )
        };
        let mapped = match mapped {
            Ok(mapped) => mapped,
            Err(error) => {
                if frame == 0 {
                    println!(
                        "scanout export: the dma-buf cannot be mapped here ({error}); contents checked through readback only"
                    );
                }
                continue;
            }
        };
        // SAFETY: the mapping is `length` bytes and stays valid until munmap.
        let bytes = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), length) };
        let pixel = |x: u32, y: u32| {
            let i = plane.offset as usize + y as usize * plane.stride as usize + x as usize * 4;
            // XRGB8888 in memory: B, G, R, X.
            [bytes[i + 2] as u16, bytes[i + 1] as u16, bytes[i] as u16]
        };
        for (x, y) in [inside, outside, (0, 0), (63, 31)] {
            assert_eq!(
                pixel(x, y),
                image.pixel(x, y)[..3],
                "dma-buf pixel ({x}, {y})"
            );
        }
        if frame == 0 {
            println!("scanout export: dma-buf contents match the readback");
        }
        // SAFETY: unmaps exactly what was mapped above; `bytes` is not used
        // afterwards.
        unsafe { rustix::mm::munmap(mapped, length) }.unwrap();
    }

    // The compositing buffer of a scanout target reads back as usual.
    let composite = renderer.read_composite(out).unwrap();
    assert_composite_close(
        composite.pixel(4, 4),
        DecodeTransform::new(&ImageDescription::SRGB_GAMMA22, AlphaMode::Straight, &SPACE)
            .unwrap()
            .apply([0.5, 0.5, 0.5, 1.0]),
        "scanout target background",
    );

    renderer.destroy_target(out);
    renderer.destroy_texture(texture);
}
