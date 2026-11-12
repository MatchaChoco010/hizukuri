//! The four example scripts, checked by reading pixels back.
//!
//! These use Luau's `buffer` and `vector` types, so they only build for the
//! Luau VM.
#![cfg(feature = "luau")]

mod common;

use common::*;
use proto_gfx_core::{Format, Handle};

/// Example 1: a script builds a pipeline and draws a triangle.
#[test]
fn triangle() {
    let host = host();
    eprintln!("triangle on {}", device_line(&host));
    load_example(&host, "triangle.luau");
    let output = target(&host, Format::Rgba8Unorm, 64, 64);
    host.call_in_frame("frame", output.to_bits()).unwrap();

    let pixels = read(&host, output);
    // Vertices (-0.5,-0.5) (0.5,-0.5) (0,0.5) in clip space, y down:
    // pixels (16,16) (48,16) (32,48). Colour (1, 0.5, 0.25), clear colour
    // (0, 0, 0.5); 0.5 lands on 127 or 128 (`UNORM8_CONVERSION`).
    let inside = [255, 128, 64, 255];
    let clear = [0, 0, 128, 255];
    let tolerance = UNORM8_CONVERSION;
    assert_close8(px8(&pixels, 64, 32, 20), inside, tolerance, "inside");
    assert_close8(px8(&pixels, 64, 32, 44), inside, tolerance, "inside");
    assert_close8(px8(&pixels, 64, 20, 17), inside, tolerance, "inside");
    assert_close8(px8(&pixels, 64, 44, 17), inside, tolerance, "inside");
    assert_close8(
        px8(&pixels, 64, 32, 12),
        clear,
        tolerance,
        "above the top edge",
    );
    assert_close8(
        px8(&pixels, 64, 20, 40),
        clear,
        tolerance,
        "left of the slanted edge",
    );
    assert_close8(
        px8(&pixels, 64, 44, 40),
        clear,
        tolerance,
        "right of the slanted edge",
    );
    assert_close8(px8(&pixels, 64, 2, 2), clear, tolerance, "outside");
    // The triangle covers half of a 32x32 box: 512 pixels, give or take the edges.
    let covered = pixels
        .chunks_exact(4)
        .filter(|p| close8((*p).try_into().unwrap(), inside, tolerance))
        .count();
    assert!((480..=544).contains(&covered), "covered {covered} pixels");
    assert_no_validation_errors();
}

struct Window {
    image: Handle,
    width: u32,
    height: u32,
    quarter_turn: bool,
}

fn windows_table(host: &proto_gfx_lua::ScriptHost, windows: &[Window]) -> mlua::Table {
    let lua = host.lua();
    let list = lua.create_table().unwrap();
    for window in windows {
        let t = lua.create_table().unwrap();
        t.set("image", window.image.to_bits()).unwrap();
        t.set("width", window.width).unwrap();
        t.set("height", window.height).unwrap();
        t.set("quarter_turn", window.quarter_turn).unwrap();
        list.push(t).unwrap();
    }
    list
}

/// Example 2: textures from outside, placed and transformed by the script,
/// with the layout changing every frame.
#[test]
fn windows_follow_the_script_layout() {
    let host = host();
    load_example(&host, "windows.luau");
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let yellow = [255, 255, 0, 255];
    let black = [0, 0, 0, 255];
    let windows = [
        Window {
            image: texture(&host, Format::Rgba8Unorm, 1, 1, &red),
            width: 24,
            height: 16,
            quarter_turn: false,
        },
        Window {
            image: texture(&host, Format::Rgba8Unorm, 1, 1, &green),
            width: 24,
            height: 16,
            quarter_turn: false,
        },
        // Left half blue, right half yellow; drawn rotated by a quarter turn,
        // so blue ends up on top.
        Window {
            image: texture(&host, Format::Rgba8Unorm, 2, 1, &[blue, yellow].concat()),
            width: 24,
            height: 16,
            quarter_turn: true,
        },
    ];
    let output = target(&host, Format::Rgba8Unorm, 160, 96);

    for tick in [0u32, 1, 5] {
        host.call_in_frame(
            "frame",
            (output.to_bits(), windows_table(&host, &windows), tick),
        )
        .unwrap();
        let pixels = read(&host, output);
        // The same layout policy as the script: origin (8,8), spacing (40,6),
        // velocity (4,2) per tick.
        let origin = |i: u32| (8 + 40 * i + 4 * tick, 8 + 6 * i + 2 * tick);

        for (i, color) in [(0, red), (1, green)] {
            let (x, y) = origin(i);
            assert_eq!(
                px8(&pixels, 160, x, y),
                color,
                "tick {tick} window {i} top-left"
            );
            assert_eq!(
                px8(&pixels, 160, x + 23, y + 15),
                color,
                "tick {tick} window {i} bottom-right"
            );
            assert_eq!(
                px8(&pixels, 160, x - 1, y),
                black,
                "tick {tick} window {i} left"
            );
            assert_eq!(
                px8(&pixels, 160, x + 24, y),
                black,
                "tick {tick} window {i} right"
            );
            assert_eq!(
                px8(&pixels, 160, x, y + 16),
                black,
                "tick {tick} window {i} below"
            );
        }

        // The rotated window keeps its centre: a 24x16 quad becomes 16x24.
        let (x, y) = origin(2);
        let (cx, cy) = (x + 12, y + 8);
        assert_eq!(
            px8(&pixels, 160, cx - 8, cy - 12),
            blue,
            "tick {tick} rotated top-left"
        );
        assert_eq!(
            px8(&pixels, 160, cx + 7, cy - 1),
            blue,
            "tick {tick} rotated upper half"
        );
        assert_eq!(
            px8(&pixels, 160, cx - 8, cy),
            yellow,
            "tick {tick} rotated lower half"
        );
        assert_eq!(
            px8(&pixels, 160, cx + 7, cy + 11),
            yellow,
            "tick {tick} rotated bottom-right"
        );
        assert_eq!(
            px8(&pixels, 160, cx - 9, cy),
            black,
            "tick {tick} rotated left"
        );
        assert_eq!(
            px8(&pixels, 160, cx + 8, cy),
            black,
            "tick {tick} rotated right"
        );
        assert_eq!(
            px8(&pixels, 160, cx, cy + 12),
            black,
            "tick {tick} rotated below"
        );
    }
    assert_no_validation_errors();
}

fn srgb_oetf(linear: f64) -> f64 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

fn pq_oetf(nits: f64) -> f64 {
    let (m1, m2) = (0.159_301_757_812_5, 78.843_75);
    let (c1, c2, c3) = (0.835_937_5, 18.851_562_5, 18.687_5);
    let y = (nits / 10_000.0).clamp(0.0, 1.0).powf(m1);
    ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

/// The formulas above are checked against published reference points, so that
/// the test does not merely compare the shader with a copy of itself.
#[test]
fn transfer_function_references() {
    // IEC 61966-2-1: linear 0.5 encodes to about 0.7354, 0.18 to about 0.4614.
    assert!((srgb_oetf(0.5) - 0.735_357).abs() < 1e-5);
    assert!((srgb_oetf(0.18) - 0.461_356).abs() < 1e-5);
    // SMPTE ST 2084: 100 nits -> 0.5081, 1000 nits -> 0.7518, 10000 nits -> 1.
    assert!((pq_oetf(100.0) - 0.508_078).abs() < 1e-5);
    assert!((pq_oetf(1000.0) - 0.751_827).abs() < 1e-5);
    assert!((pq_oetf(10_000.0) - 1.0).abs() < 1e-9);
}

/// Example 3: composite into linear RGBA16F, then encode with the output
/// transfer function in a second pass.
#[test]
fn two_pass_composite_then_encode() {
    let host = host();
    load_example(&host, "two_pass.luau");
    let lua = host.lua();
    // Premultiplied, linear. The first is half-transparent orange; the second
    // is opaque and brighter than 1.0.
    let translucent = [0.5f32, 0.25, 0.0, 0.5];
    let bright = [10.0f32, 1.0, 4.0, 1.0];
    let list = lua.create_table().unwrap();
    for (color, x) in [(translucent, 4), (bright, 20)] {
        let t = lua.create_table().unwrap();
        let image = texture(&host, Format::Rgba16Float, 1, 1, &f16_bytes(&color));
        t.set("image", image.to_bits()).unwrap();
        t.set("x", x).unwrap();
        t.set("y", 4).unwrap();
        t.set("width", 8).unwrap();
        t.set("height", 8).unwrap();
        list.push(t).unwrap();
    }
    let background = [0.25f32, 0.25, 0.25, 1.0];
    // Source-over with premultiplied alpha: src + dst * (1 - src.a).
    let blended = [0.5 + 0.125, 0.25 + 0.125, 0.125, 1.0];

    // --- sRGB output -----------------------------------------------------
    let srgb_out = target(&host, Format::Rgba8Unorm, 32, 16);
    host.call_in_frame("frame", (srgb_out.to_bits(), list.clone(), 0, 1.0))
        .unwrap();
    let linear = Handle::from_bits(lua.globals().get::<i64>("last_linear_image").unwrap());
    let linear_pixels = read(&host, linear);
    assert_close(
        px16f(&linear_pixels, 32, 1, 1),
        background,
        0.0,
        "linear background",
    );
    assert_close(
        px16f(&linear_pixels, 32, 6, 6),
        blended,
        0.0,
        "linear blended",
    );
    assert_close(
        px16f(&linear_pixels, 32, 22, 6),
        bright,
        0.0,
        "linear HDR value",
    );

    let encode8 = |linear: [f32; 4]| -> [u8; 4] {
        std::array::from_fn(|c| {
            let v = if c == 3 {
                f64::from(linear[c])
            } else {
                srgb_oetf(f64::from(linear[c]).clamp(0.0, 1.0))
            };
            (v * 255.0).round() as u8
        })
    };
    let srgb_pixels = read(&host, srgb_out);
    assert_close8(
        px8(&srgb_pixels, 32, 1, 1),
        encode8(background),
        1,
        "sRGB background",
    );
    assert_close8(
        px8(&srgb_pixels, 32, 6, 6),
        encode8(blended),
        1,
        "sRGB blended",
    );
    assert_close8(
        px8(&srgb_pixels, 32, 22, 6),
        encode8(bright),
        1,
        "sRGB clipped HDR",
    );
    assert_eq!(encode8(background)[0], 137, "sRGB(0.25) is 137/255");

    // --- PQ output, with exposure 2 from the uniform buffer ------------------
    let pq_out = target(&host, Format::Rgba16Float, 32, 16);
    host.call_in_frame("frame", (pq_out.to_bits(), list, 1, 2.0))
        .unwrap();
    let pq_pixels = read(&host, pq_out);
    // Linear 1.0 is 100 nits; exposure doubles it.
    let encode_pq = |linear: [f32; 4]| -> [f32; 4] {
        std::array::from_fn(|c| {
            if c == 3 {
                linear[c]
            } else {
                pq_oetf(f64::from(linear[c]) * 2.0 * 100.0) as f32
            }
        })
    };
    assert_close(
        px16f(&pq_pixels, 32, 1, 1),
        encode_pq(background),
        2e-3,
        "PQ background",
    );
    assert_close(
        px16f(&pq_pixels, 32, 6, 6),
        encode_pq(blended),
        2e-3,
        "PQ blended",
    );
    assert_close(
        px16f(&pq_pixels, 32, 22, 6),
        encode_pq(bright),
        2e-3,
        "PQ HDR value",
    );
    // 10.0 * 2 * 100 = 2000 nits, 1.0 * 2 * 100 = 200 nits.
    assert!((px16f(&pq_pixels, 32, 22, 6)[0] - pq_oetf(2000.0) as f32).abs() < 2e-3);
    assert_no_validation_errors();
}

/// Example 4: a compute blur, and a render pass that samples its output.
#[test]
fn compute_blur() {
    let host = host();
    load_example(&host, "blur.luau");
    let mut source = vec![0.0f32; 16 * 16 * 4];
    let at = (8 * 16 + 8) * 4;
    source[at..at + 4].copy_from_slice(&[9.0, 4.5, 0.0, 9.0]);
    let source_image = texture(&host, Format::Rgba16Float, 16, 16, &f16_bytes(&source));
    let blurred = image(
        &host,
        Format::Rgba16Float,
        16,
        16,
        proto_gfx_core::ImageUsage::STORAGE
            | proto_gfx_core::ImageUsage::SAMPLED
            | proto_gfx_core::ImageUsage::COPY_SRC,
    );
    let output = target(&host, Format::Rgba8Unorm, 16, 16);
    host.call_in_frame(
        "frame",
        (
            source_image.to_bits(),
            blurred.to_bits(),
            output.to_bits(),
            1,
        ),
    )
    .unwrap();

    let blurred_pixels = read(&host, blurred);
    let output_pixels = read(&host, output);
    for y in 0..16 {
        for x in 0..16 {
            let near = (7..=9).contains(&x) && (7..=9).contains(&y);
            let expected = if near { [1.0, 0.5, 0.0, 1.0] } else { [0.0; 4] };
            assert_close(
                px16f(&blurred_pixels, 16, x, y),
                expected,
                1e-3,
                &format!("blurred ({x},{y})"),
            );
            let expected8 = if near { [255, 128, 0, 255] } else { [0; 4] };
            assert_close8(
                px8(&output_pixels, 16, x, y),
                expected8,
                1,
                &format!("presented ({x},{y})"),
            );
        }
    }
    assert_eq!(host.gpu().stats().dispatches, 1);
    assert_no_validation_errors();
}

/// Example 5: one instanced draw for many windows. The shader picks the heap
/// slot of each quad from GPU data; the API validates the whole range the
/// shader can reach, and clamps the index when it takes in the shader.
#[test]
fn batched_draw_through_a_texture_array() {
    let host = host();
    host.load("batched.luau", &script("batched.luau")).unwrap();
    let colors: Vec<[u8; 4]> = (0..8u8)
        .map(|i| [30 * i + 20, 255 - 30 * i, 10 * i, 255])
        .collect();
    let textures: Vec<Handle> = colors
        .iter()
        .map(|c| texture(&host, Format::Rgba8Unorm, 1, 1, c))
        .collect();
    let bits: Vec<i64> = textures.iter().map(|t| t.to_bits()).collect();
    host.call_in_frame("init", bits).unwrap();
    let slots: Vec<i64> = host.lua().globals().get("slots").unwrap();
    assert_eq!(slots.len(), 8);
    let first = Handle::from_bits(slots[0]).index();
    for (i, slot) in slots.iter().enumerate() {
        assert_eq!(
            Handle::from_bits(*slot).index(),
            first + i as u32,
            "consecutive slots"
        );
    }

    let output = target(&host, Format::Rgba8Unorm, 64, 64);
    // x, y, width, height, texture index. The last two indices are outside
    // the array of 8. The clamp reads an index as a signed number: 8 becomes
    // element 7, and 4 000 000 000 (negative as an i32) element 0.
    let quads: [[u32; 5]; 6] = [
        [0, 0, 8, 8, 0],
        [16, 0, 8, 8, 3],
        [32, 0, 8, 8, 7],
        [0, 16, 8, 8, 5],
        [16, 16, 8, 8, 8],
        [32, 16, 8, 8, 4_000_000_000],
    ];
    let windows = host.lua().create_table().unwrap();
    for quad in quads {
        windows.push(quad.to_vec()).unwrap();
    }
    host.call_in_frame("frame", (output.to_bits(), windows))
        .unwrap();
    assert_eq!(host.gpu().stats().draws, 1, "one draw for all quads");
    let pixels = read(&host, output);
    for quad in quads {
        let expected = colors[(quad[4] as i32).clamp(0, 7) as usize];
        assert_eq!(
            px8(&pixels, 64, quad[0] + 4, quad[1] + 4),
            expected,
            "quad at ({}, {}) with texture index {}",
            quad[0],
            quad[1],
            quad[4]
        );
    }
    assert_eq!(px8(&pixels, 64, 60, 60), [0, 0, 0, 255]);

    // The array bound two slots further on: its last two elements would lie
    // outside the run. Refused.
    let result = host.call_in_frame("frame_bound_to", (output.to_bits(), slots[2]));
    assert_eq!(
        result
            .as_ref()
            .err()
            .and_then(proto_gfx_lua::ScriptError::gfx_code),
        Some(proto_gfx_core::ErrorCode::MissingBinding),
        "{result:?}"
    );
    // A hole in the run: one slot destroyed. Refused, and the message names it.
    host.gpu().destroy(Handle::from_bits(slots[5])).unwrap();
    let result = host.call_in_frame("frame_bound_to", (output.to_bits(), slots[0]));
    match &result {
        Err(proto_gfx_lua::ScriptError::Gfx(e)) => {
            assert_eq!(e.code, proto_gfx_core::ErrorCode::MissingBinding, "{e}");
            assert!(e.message.contains("element 5"), "{e}");
        }
        other => panic!("expected a gfx error, got {other:?}"),
    }
    // An image of the run destroyed while its slot stays.
    host.gpu().destroy(textures[1]).unwrap();
    let result = host.call_in_frame("frame_bound_to", (output.to_bits(), slots[0]));
    assert!(result.is_err());
    assert_no_validation_errors();
}
