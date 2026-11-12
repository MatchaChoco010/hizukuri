//! Tests that run the real `proto-compositor` binary headless, on each device
//! listed in `HIZUKURI_TEST_DEVICES` (test layers 3 and 4: what is on
//! screen, as numbers).
//!
//! Colors are checked numerically against the CPU reference in
//! `proto-color`. Screenshots stay in `target/test-output/<test>-<device>/`.

mod common;

use std::process::Command;
use std::time::Duration;

use common::{Device, Session, TIMEOUT, options, window};
use proto_color::{
    AlphaMode, CompositeSpace, DecodeTransform, EncodeTransform, ImageDescription, blend_over,
    quantize_unorm,
};
use proto_compositor::control::Request;
use proto_compositor::image::{CompositeImage, EncodedImage};
use proto_test_client::{
    BufferKind, ClientEvent, ClientOptions, Pattern, PixelLayout, TestClient, Transform, Viewport,
};

const SPACE: CompositeSpace = CompositeSpace::BT2020_LINEAR;
/// What a surface without color information is.
const UNMANAGED: ImageDescription = ImageDescription::SRGB_GAMMA22;
/// Default background of the compositor (`--background 202020`).
const BACKGROUND: [u8; 4] = [0x20, 0x20, 0x20, 0xff];

const KEY_A: u32 = 30;
const BTN_LEFT: u32 = 0x110;

device_tests!(
    unmanaged_fill_comes_back_unchanged_on_a_gamma22_output,
    display_p3_output_matches_the_cpu_reference,
    translucent_windows_blend_in_linear_light,
    surface_image_description_is_honored,
    pq_output_puts_sdr_white_at_the_reference_white,
    hidpi_output_scales_surfaces,
    subsurfaces_and_opaque_formats_are_drawn,
    input_injected_through_the_control_socket_reaches_the_client,
    proto_ctl_drives_a_session,
    buffer_transforms_are_undone,
    viewports_crop_and_scale,
    fractional_scale_is_announced_and_a_matching_buffer_is_shown_exactly,
    presentation_feedback_reports_each_frame,
    dmabuf_buffers_are_read_and_shown,
    dmabuf_is_not_offered_unless_asked,
    two_outputs_each_show_what_is_on_them,
    sigterm_ends_the_compositor_cleanly,
    a_killed_compositor_does_not_block_the_next_one,
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalized(rgba: [u8; 4]) -> [f32; 4] {
    rgba.map(|v| v as f32 / 255.0)
}

/// Stored bytes of a `wl_shm` buffer -> premultiplied linear value in the
/// compositing space, on the CPU.
fn decode(description: &ImageDescription, rgba: [u8; 4]) -> [f32; 4] {
    DecodeTransform::new(description, AlphaMode::PremultipliedElectrical, &SPACE)
        .unwrap()
        .apply(normalized(rgba))
}

/// Compositing value -> code values of an output, on the CPU.
fn encode(output: &ImageDescription, composite: [f32; 4], bits: u32) -> [u32; 3] {
    EncodeTransform::new(&SPACE, output)
        .unwrap()
        .apply([composite[0], composite[1], composite[2]])
        .map(|v| quantize_unorm(v, bits))
}

fn max_difference(actual: [u16; 4], expected: [u32; 3]) -> u32 {
    (0..3)
        .map(|i| (actual[i] as i64 - expected[i] as i64).unsigned_abs() as u32)
        .max()
        .unwrap()
}

#[track_caller]
fn assert_pixel(
    image: &EncodedImage,
    x: u32,
    y: u32,
    expected: [u32; 3],
    tolerance: u32,
    what: &str,
) {
    let actual = image.pixel(x, y);
    assert!(
        max_difference(actual, expected) <= tolerance,
        "{what}: pixel ({x}, {y}) is {actual:?}, expected {expected:?} within {tolerance}"
    );
    assert_eq!(
        actual[3],
        image.format.max_code(),
        "{what}: output alpha at ({x}, {y})"
    );
}

/// Compositing buffer values are half floats: 11 significant bits. Allow
/// twice the rounding error plus a small absolute term near zero.
#[track_caller]
fn assert_composite(image: &CompositeImage, x: u32, y: u32, expected: [f32; 4], what: &str) {
    let actual = image.pixel(x, y);
    for i in 0..4 {
        let tolerance = 2e-5 + expected[i].abs() * 1e-3;
        assert!(
            (actual[i] - expected[i]).abs() <= tolerance,
            "{what}: compositing value at ({x}, {y}) is {actual:?}, expected {expected:?}"
        );
    }
}

/// Tolerance in 8 bit code values between the output and the CPU pipeline
/// for an output with a pure power transfer function.
///
/// The compositing buffer is RGBA16F in BT.2020. Rounding to half floats and
/// converting back to narrower primaries leaves about 1e-4 (linear) in
/// channels that should be zero next to a bright one, and a power function
/// has unbounded slope at zero: that is up to 3 code values at 8 bit.
/// Gray levels are not affected and are checked exactly.
const POWER_OUTPUT_TOLERANCE: u32 = 3;

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// A surface without color information on an output of the same kind
/// (BT.709 primaries, gamma 2.2): the values come back as they went in.
fn unmanaged_fill_comes_back_unchanged_on_a_gamma22_output(device: Device) {
    let mut session = Session::start("unmanaged-fill", device, &["--output-size", "256x192"]);
    let (mut client, id) = session.mapped_client(options("fill", 100, 60, [0, 0, 0, 255]));
    session.send(Request::MoveWindow {
        window: id,
        x: 40,
        y: 30,
    });

    let state = session.state();
    assert_eq!(
        state["outputs"][0]["image_description"]["name"],
        "srgb-gamma22"
    );
    assert_eq!(
        window(&state, "fill").unwrap()["surfaces"][0]["image_description"]["name"],
        "srgb-gamma22"
    );

    // Gray levels: exact.
    for level in [0u8, 1, 2, 17, 64, 128, 200, 254, 255] {
        session.recolor(&mut client, "fill", [level, level, level, 255]);
        let (encoded, composite) = session.screenshot(&format!("gray-{level}"));
        assert_eq!((encoded.width, encoded.height), (256, 192));
        let expected = [level as u32; 3];
        // The four corners of the window, and its center.
        for (x, y) in [(40, 30), (139, 30), (40, 89), (139, 89), (90, 60)] {
            assert_pixel(&encoded, x, y, expected, 0, "gray fill");
        }
        // Just outside every edge: background, untouched.
        for (x, y) in [(39, 30), (140, 30), (40, 29), (40, 90), (0, 0), (255, 191)] {
            assert_pixel(&encoded, x, y, [0x20; 3], 0, "background");
        }
        assert_composite(
            &composite,
            90,
            60,
            decode(&UNMANAGED, [level, level, level, 255]),
            "gray fill",
        );
    }

    // Colors: within the documented tolerance of the 16 bit float buffer.
    let mut worst = 0;
    for color in [
        [255u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [200, 100, 50, 255],
        [10, 20, 30, 255],
        [254, 128, 3, 255],
    ] {
        session.recolor(&mut client, "fill", color);
        let (encoded, composite) = session.screenshot(&format!(
            "color-{:02x}{:02x}{:02x}",
            color[0], color[1], color[2]
        ));
        let expected = [color[0] as u32, color[1] as u32, color[2] as u32];
        assert_pixel(
            &encoded,
            90,
            60,
            expected,
            POWER_OUTPUT_TOLERANCE,
            "color fill",
        );
        worst = worst.max(max_difference(encoded.pixel(90, 60), expected));
        assert_composite(&composite, 90, 60, decode(&UNMANAGED, color), "color fill");
    }
    println!(
        "{}: worst color difference {worst} code value(s)",
        device.name()
    );

    session.assert_drawing_clean();
    session.quit();
}

/// The same content on a Display P3 output: the renderer's result matches
/// what `proto-color` computes on the CPU.
fn display_p3_output_matches_the_cpu_reference(device: Device) {
    let output = ImageDescription::DISPLAY_P3;
    let mut session = Session::start(
        "display-p3-output",
        device,
        &[
            "--output-size",
            "256x192",
            "--output-image-description",
            "display-p3",
        ],
    );
    let (mut client, id) = session.mapped_client(options("fill", 100, 60, [255, 0, 0, 255]));
    session.send(Request::MoveWindow {
        window: id,
        x: 40,
        y: 30,
    });
    let state = session.state();
    assert_eq!(
        state["outputs"][0]["image_description"]["name"],
        "display-p3"
    );
    assert_eq!(
        state["outputs"][0]["encode_pipeline"],
        "luminance:x(79.8/79.8) | primaries:bt2020->display-p3 | clip[0,1] | inverse-eotf:srgb-piecewise"
    );

    // sRGB red is color(display-p3 0.9175 0.2003 0.1386): 234, 51, 35.
    let (encoded, _) = session.screenshot("red");
    assert_pixel(&encoded, 90, 60, [234, 51, 35], 1, "sRGB red on Display P3");

    let mut worst = 0;
    for color in [
        [255u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 255, 255],
        [128, 128, 128, 255],
        [30, 220, 240, 255],
        [200, 100, 50, 255],
        [1, 2, 3, 255],
    ] {
        session.recolor(&mut client, "fill", color);
        let (encoded, composite) = session.screenshot(&format!(
            "color-{:02x}{:02x}{:02x}",
            color[0], color[1], color[2]
        ));
        let linear = decode(&UNMANAGED, color);
        assert_composite(&composite, 90, 60, linear, "fill");
        // The piecewise sRGB curve of Display P3 has a finite slope at
        // zero, so one code value is enough.
        let expected = encode(&output, linear, 8);
        assert_pixel(&encoded, 90, 60, expected, 1, "fill on Display P3");
        worst = worst.max(max_difference(encoded.pixel(90, 60), expected));
    }
    println!("{}: worst difference {worst} code value(s)", device.name());

    // The background went through the same conversion.
    let expected_background = encode(&output, decode(&UNMANAGED, BACKGROUND), 8);
    assert_pixel(
        &encoded_background(&mut session),
        5,
        5,
        expected_background,
        1,
        "background",
    );

    session.assert_drawing_clean();
    session.quit();
}

fn encoded_background(session: &mut Session) -> EncodedImage {
    session.screenshot("background").0
}

/// Translucent surfaces over opaque ones: the result is the blend of
/// linear light values, not of encoded values.
fn translucent_windows_blend_in_linear_light(device: Device) {
    let mut session = Session::start("linear-blend", device, &["--output-size", "256x192"]);
    let green = [0u8, 255, 0, 255];
    // Premultiplied: 50% red is (128, 0, 0, 128), 50% white (128, 128, 128, 128).
    let half_red = [128u8, 0, 0, 128];
    let half_white = [128u8, 128, 128, 128];

    let (mut below, below_id) = session.mapped_client(options("below", 100, 100, green));
    let (mut above, above_id) = session.mapped_client(options("above", 100, 100, half_red));
    session.send(Request::MoveWindow {
        window: below_id,
        x: 20,
        y: 20,
    });
    session.send(Request::MoveWindow {
        window: above_id,
        x: 70,
        y: 50,
    });
    let state = session.wait(&mut [&mut below, &mut above], "both placed", |s| {
        window(s, "above").is_some_and(|w| w["position"] == serde_json::json!([70, 50]))
    });
    // "above" was created last, so it is on top.
    assert_eq!(state["windows"][1]["title"], "above");

    let (encoded, composite) = session.screenshot("red-over-green");
    let green_linear = decode(&UNMANAGED, green);
    let red_linear = decode(&UNMANAGED, half_red);
    let background_linear = decode(&UNMANAGED, BACKGROUND);

    // Only the lower window.
    assert_composite(&composite, 40, 40, green_linear, "green alone");
    assert_pixel(
        &encoded,
        40,
        40,
        [0, 255, 0],
        POWER_OUTPUT_TOLERANCE,
        "green alone",
    );

    // The overlap: red over green.
    let over_green = blend_over(green_linear, red_linear);
    assert_composite(&composite, 100, 100, over_green, "red over green");
    let expected = encode(&UNMANAGED, over_green, 8);
    assert_pixel(&encoded, 100, 100, expected, 1, "red over green");
    // In linear light the half covered channels land at 186; blending the
    // encoded values would have given 128.
    assert_eq!(expected, [186, 186, 0]);

    // The upper window over the background only.
    let over_background = blend_over(background_linear, red_linear);
    assert_composite(&composite, 150, 130, over_background, "red over background");
    assert_pixel(
        &encoded,
        150,
        130,
        encode(&UNMANAGED, over_background, 8),
        1,
        "red over background",
    );

    // Same with white over green: every channel takes part.
    session.recolor(&mut above, "above", half_white);
    let (encoded, composite) = session.screenshot("white-over-green");
    let white_linear = decode(&UNMANAGED, half_white);
    let over_green = blend_over(green_linear, white_linear);
    assert_composite(&composite, 100, 100, over_green, "white over green");
    assert_pixel(
        &encoded,
        100,
        100,
        encode(&UNMANAGED, over_green, 8),
        1,
        "white over green",
    );

    // Window opacity is visual state: it scales the premultiplied value of
    // an opaque window the same way.
    session.recolor(&mut above, "above", [255, 255, 255, 255]);
    session.send(Request::SetWindowOpacity {
        window: above_id,
        opacity: 0.25,
    });
    let (encoded, composite) = session.screenshot("opacity");
    let quarter_white = [0.25, 0.25, 0.25, 0.25];
    let over_green = blend_over(green_linear, quarter_white);
    assert_composite(&composite, 100, 100, over_green, "25% white over green");
    assert_pixel(
        &encoded,
        100,
        100,
        encode(&UNMANAGED, over_green, 8),
        1,
        "25% white over green",
    );
    // The logical state is untouched by the visual change.
    let state = session.state();
    let above_state = window(&state, "above").unwrap();
    assert_eq!(above_state["visual"]["opacity"], 0.25);
    assert_eq!(above_state["position"], serde_json::json!([70, 50]));

    session.assert_drawing_clean();
    session.quit();
}

/// A surface whose values are declared to be Display P3 (through the
/// control interface; `script.rs` does it through color-management-v1) is
/// converted accordingly.
fn surface_image_description_is_honored(device: Device) {
    let mut session = Session::start("surface-description", device, &["--output-size", "256x192"]);
    let color = [255u8, 64, 32, 255];
    let (mut client, id) = session.mapped_client(options("p3", 100, 60, color));
    session.send(Request::MoveWindow {
        window: id,
        x: 40,
        y: 30,
    });
    let surface = window(&session.state(), "p3").unwrap()["surface"]
        .as_u64()
        .unwrap();

    // Before: taken as BT.709 + gamma 2.2.
    let (_, composite) = session.screenshot("unmanaged");
    assert_composite(&composite, 90, 60, decode(&UNMANAGED, color), "unmanaged");

    session.send(Request::SetSurfaceColor {
        surface,
        image_description: "display-p3".into(),
        alpha_mode: None,
    });
    let state = session.wait(&mut [&mut client], "description applied", |s| {
        window(s, "p3").unwrap()["surfaces"][0]["image_description"]["name"] == "display-p3"
    });
    assert_eq!(
        window(&state, "p3").unwrap()["surfaces"][0]["decode_pipeline"],
        "unpremultiply | eotf:srgb-piecewise | primaries:display-p3->bt2020 | luminance:x(79.8/79.8) | premultiply"
    );

    let (encoded, composite) = session.screenshot("display-p3");
    let linear = decode(&ImageDescription::DISPLAY_P3, color);
    assert_composite(&composite, 90, 60, linear, "Display P3 surface");
    // The same bytes mean a different, more saturated color now.
    let unmanaged = decode(&UNMANAGED, color);
    assert!((linear[0] - unmanaged[0]).abs() > 0.05);
    // On the sRGB-like output that color is out of gamut: it clips.
    let expected = encode(&UNMANAGED, linear, 8);
    assert_eq!(expected[0], 255);
    assert_pixel(
        &encoded,
        90,
        60,
        expected,
        POWER_OUTPUT_TOLERANCE,
        "Display P3 surface",
    );

    session.assert_drawing_clean();
    session.quit();
}

/// On a BT.2100 PQ output, SDR white sits at the reference white
/// (203 cd/m², PQ code 0.5807), not at the PQ maximum.
fn pq_output_puts_sdr_white_at_the_reference_white(device: Device) {
    let output = ImageDescription::BT2020_PQ;
    let mut session = Session::start(
        "pq-output",
        device,
        &[
            "--output-size",
            "256x192",
            "--output-image-description",
            "bt2020-pq",
            "--output-format",
            "rgba16",
        ],
    );
    let white = [255u8, 255, 255, 255];
    let (mut client, id) = session.mapped_client(options("sdr", 100, 60, white));
    session.send(Request::MoveWindow {
        window: id,
        x: 40,
        y: 30,
    });
    let state = session.state();
    assert_eq!(state["outputs"][0]["format"], "rgba16");
    assert_eq!(
        state["outputs"][0]["encode_pipeline"],
        "luminance:x(202.995/10000) | primaries:bt2020->bt2020 | clip[0,1] | inverse-eotf:pq"
    );

    let (encoded, composite) = session.screenshot("white");
    assert_composite(&composite, 90, 60, [1.0, 1.0, 1.0, 1.0], "SDR white");
    let code = encoded.pixel_normalized(90, 60);
    for channel in &code[..3] {
        assert!(
            (channel - 0.5807).abs() < 5e-4,
            "SDR white should be PQ 0.5807, got {code:?}"
        );
    }

    // Colors against the CPU reference. At 16 bit the half float error of
    // the compositing buffer is several code values (measured: 5).
    for color in [[255u8, 0, 0, 255], [0, 128, 255, 255], [40, 40, 40, 255]] {
        session.recolor(&mut client, "sdr", color);
        let (encoded, composite) = session.screenshot(&format!(
            "color-{:02x}{:02x}{:02x}",
            color[0], color[1], color[2]
        ));
        let linear = decode(&UNMANAGED, color);
        assert_composite(&composite, 90, 60, linear, "SDR color");
        assert_pixel(
            &encoded,
            90,
            60,
            encode(&output, linear, 16),
            8,
            "SDR color on PQ",
        );
    }

    session.assert_drawing_clean();
    session.quit();
}

/// On an output with scale 2, a 100x60 logical window covers 200x120
/// physical pixels, at twice its logical position.
fn hidpi_output_scales_surfaces(device: Device) {
    let mut session = Session::start(
        "hidpi",
        device,
        &["--output-size", "512x384", "--output-scale", "2"],
    );
    let gray = [128u8, 128, 128, 255];
    let (mut client, id) = session.mapped_client(options("scaled", 100, 60, gray));
    session.send(Request::MoveWindow {
        window: id,
        x: 40,
        y: 30,
    });
    let state = session.wait(&mut [&mut client], "moved", |s| {
        window(s, "scaled").unwrap()["position"] == serde_json::json!([40, 30])
    });
    assert_eq!(
        state["outputs"][0]["logical_size"],
        serde_json::json!([256, 192])
    );
    assert!(
        client
            .events()
            .contains(&ClientEvent::PreferredBufferScale { scale: 2 })
    );

    let (encoded, _) = session.screenshot("scaled");
    assert_eq!((encoded.width, encoded.height), (512, 384));
    // Physical corners: (80, 60) to (279, 179).
    for (x, y) in [(80, 60), (279, 60), (80, 179), (279, 179), (180, 120)] {
        assert_pixel(&encoded, x, y, [128; 3], 0, "scaled window");
    }
    for (x, y) in [(79, 60), (280, 60), (80, 59), (80, 180)] {
        assert_pixel(&encoded, x, y, [0x20; 3], 0, "background");
    }

    // Input is in logical coordinates.
    client.clear_events();
    session.send(Request::PointerMotion { x: 45.0, y: 40.0 });
    let entered = client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::PointerEnter { .. }))
        .unwrap();
    assert_eq!(entered, Some(ClientEvent::PointerEnter { x: 5.0, y: 10.0 }));

    session.assert_drawing_clean();
    session.quit();
}

/// Subsurfaces are drawn at their offset above the parent, and `x` formats
/// ignore their fourth byte.
fn subsurfaces_and_opaque_formats_are_drawn(device: Device) {
    let mut session = Session::start("subsurface", device, &["--output-size", "256x192"]);
    // Xrgb with a zero fourth byte: must still be opaque.
    let (mut client, id) = session.mapped_client(ClientOptions {
        layout: PixelLayout::Xrgb8888,
        ..options("parent", 100, 80, [64, 128, 192, 0])
    });
    session.send(Request::MoveWindow {
        window: id,
        x: 20,
        y: 20,
    });
    client
        .add_subsurface(60, 10, 80, 30, [200, 200, 200, 0])
        .unwrap();
    session.wait(&mut [&mut client], "subsurface mapped", |s| {
        let surfaces = &window(s, "parent").unwrap()["surfaces"];
        surfaces.as_array().unwrap().len() == 2 && surfaces[1]["buffer_size"].is_array()
    });

    let (encoded, _) = session.screenshot("subsurface");
    assert_pixel(
        &encoded,
        30,
        30,
        [64, 128, 192],
        POWER_OUTPUT_TOLERANCE,
        "parent",
    );
    // The subsurface covers (80, 30) to (159, 59): partly over the parent,
    // partly sticking out to the right of it.
    for (x, y) in [(80, 30), (119, 59), (130, 40), (159, 59)] {
        assert_pixel(&encoded, x, y, [200; 3], 0, "subsurface");
    }
    assert_pixel(
        &encoded,
        79,
        30,
        [64, 128, 192],
        POWER_OUTPUT_TOLERANCE,
        "parent left of it",
    );
    assert_pixel(&encoded, 160, 40, [0x20; 3], 0, "background right of it");
    assert_pixel(&encoded, 130, 60, [0x20; 3], 0, "background below it");

    session.assert_drawing_clean();
    session.quit();
}

/// Keys and pointer events injected through the control socket of the real
/// binary reach the client, paced by the real event loop.
fn input_injected_through_the_control_socket_reaches_the_client(device: Device) {
    let mut session = Session::start("input", device, &["--output-size", "256x192"]);
    let (mut client, id) = session.mapped_client(options("input", 100, 60, [90, 90, 90, 255]));
    session.send(Request::MoveWindow {
        window: id,
        x: 40,
        y: 30,
    });
    assert!(
        client
            .wait_for(TIMEOUT, |e| *e == ClientEvent::KeyboardEnter)
            .unwrap()
            .is_some()
    );
    // The frame loop runs: the buffer comes back and the frame callback
    // fires without anyone asking for a screenshot.
    assert!(
        client
            .wait_for(TIMEOUT, |e| *e == ClientEvent::BufferReleased)
            .unwrap()
            .is_some()
    );
    client.clear_events();

    session.send(Request::Key {
        keycode: KEY_A,
        pressed: true,
    });
    session.send(Request::Key {
        keycode: KEY_A,
        pressed: false,
    });
    session.send(Request::PointerMotion { x: 50.0, y: 45.0 });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    session.send(Request::PointerAxis {
        horizontal: 0.0,
        vertical: 15.0,
        v120_horizontal: None,
        v120_vertical: Some(120),
    });
    session.send(Request::PointerMotion { x: 200.0, y: 150.0 });
    assert!(
        client
            .wait_for(TIMEOUT, |e| *e == ClientEvent::PointerLeave)
            .unwrap()
            .is_some()
    );
    client.pump(Some(Duration::from_millis(50))).unwrap();

    let received: Vec<ClientEvent> = client
        .events()
        .iter()
        .filter(|e| {
            !matches!(
                e,
                ClientEvent::FrameDone { .. }
                    | ClientEvent::Modifiers { .. }
                    | ClientEvent::Configure { .. }
            )
        })
        .cloned()
        .collect();
    assert_eq!(
        received,
        [
            ClientEvent::Key {
                keycode: KEY_A,
                pressed: true,
                keysym: "a".into(),
                text: "a".into()
            },
            ClientEvent::Key {
                keycode: KEY_A,
                pressed: false,
                keysym: "a".into(),
                text: "a".into()
            },
            ClientEvent::PointerEnter { x: 10.0, y: 15.0 },
            ClientEvent::PointerFrame,
            ClientEvent::PointerButton {
                button: BTN_LEFT,
                pressed: true
            },
            ClientEvent::PointerFrame,
            ClientEvent::PointerButton {
                button: BTN_LEFT,
                pressed: false
            },
            ClientEvent::PointerFrame,
            ClientEvent::PointerAxisValue120 {
                axis: 0,
                value: 120
            },
            ClientEvent::PointerAxis {
                axis: 0,
                value: 15.0
            },
            ClientEvent::PointerFrame,
            ClientEvent::PointerLeave,
            ClientEvent::PointerFrame,
        ]
    );

    let state = session.state();
    assert_eq!(state["focus"]["window"], id);
    assert_eq!(
        state["pointer"]["position"],
        serde_json::json!([200.0, 150.0])
    );
    session.assert_drawing_clean();
    session.quit();
}

/// The `proto-ctl` binary talks to a session: state, a refused request
/// (failure status), screenshot, quit.
fn proto_ctl_drives_a_session(device: Device) {
    let mut session = Session::start("ctl", device, &["--output-size", "128x96"]);
    let (_client, id) = session.mapped_client(options("ctl", 40, 30, [255, 255, 255, 255]));
    let ctl = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_proto-ctl"))
            .arg("--socket")
            .arg(&session.control_socket)
            .args(args)
            .output()
            .expect("running proto-ctl")
    };

    let output = ctl(&["state"]);
    assert!(output.status.success());
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["ok"], true);
    assert_eq!(state["windows"][0]["id"], id);
    assert_eq!(state["outputs"][0]["size"], serde_json::json!([128, 96]));

    let output = ctl(&["move-window", "9999", "0", "0"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown window"));

    let png = session.output_dir.join("ctl.png");
    let output = ctl(&["move-window", &id.to_string(), "10", "10"]);
    assert!(output.status.success());
    let output = ctl(&["screenshot", png.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let image = EncodedImage::read_png(&png, UNMANAGED).unwrap();
    assert_eq!((image.width, image.height), (128, 96));
    assert_eq!(image.pixel(20, 20), [255, 255, 255, 255]);
    assert_eq!(image.pixel(5, 5), [0x20, 0x20, 0x20, 0xff]);

    session.assert_drawing_clean();
    session.quit();
}

// ---------------------------------------------------------------------------
// Ending
// ---------------------------------------------------------------------------

/// SIGTERM (what `systemctl stop` and a logout send) ends the run like a
/// quit request: exit status 0, sockets removed. Vulkan drivers have worker
/// threads; the signal must not be able to land on one of them and kill
/// the process outright, which on KMS would leave the display unrestored.
///
/// SIGHUP (the terminal or the shell the compositor was started from went
/// away) is handled the same way.
fn sigterm_ends_the_compositor_cleanly(device: Device) {
    for signal in ["TERM", "TERM", "TERM", "HUP"] {
        let mut session = Session::start("sigterm", device, &[]);
        // Something is drawn, so the driver's threads are surely running.
        let (_client, _) = session.mapped_client(options("w", 64, 64, [255, 0, 0, 255]));
        session.screenshot("frame");
        session.signal(signal);
        session.expect_clean_exit();
    }
}

/// A compositor that was killed leaves its sockets behind. The next one on
/// the same names starts anyway, and a second one next to a live one is
/// still refused.
fn a_killed_compositor_does_not_block_the_next_one(device: Device) {
    let session = Session::start("killed", device, &[]);
    session.signal("KILL");
    let runtime_dir = session.abandon();
    assert!(
        runtime_dir.join("hizukuri-test.control").exists(),
        "a killed compositor leaves its control socket"
    );

    let mut session = Session::start_in("killed-next", device, &[], runtime_dir.clone());
    assert_eq!(session.state()["backend"], "headless");

    // The live one is not pushed aside.
    let mut second = std::process::Command::new(env!("CARGO_BIN_EXE_proto-compositor"));
    second
        .args(["--backend", "headless", "--socket", "hizukuri-test"])
        .args(["--no-drawing", "--exit-after", "5"]);
    common::isolate(&mut second, &runtime_dir);
    let output = second.output().unwrap();
    assert!(!output.status.success(), "a second compositor started");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(session.state()["backend"], "headless");
    session.quit();
}

// ---------------------------------------------------------------------------
// Buffer state: transform, scale, viewport
// ---------------------------------------------------------------------------

// Four colors that are easy to tell apart. They are not the pure
// primaries: a channel at zero comes back a few code values off, because
// the 16 bit float compositing buffer in BT.2020 is coarse near zero once
// the 2.2 curve stretches it out again. These come back within
// `QUADRANT_TOLERANCE`.
const RED: [u8; 4] = [210, 60, 50, 255];
const GREEN: [u8; 4] = [60, 190, 70, 255];
const BLUE: [u8; 4] = [50, 80, 210, 255];
const WHITE: [u8; 4] = [235, 235, 235, 255];
const QUADRANT_TOLERANCE: u32 = 1;
/// The quadrants of the test buffers, as they are in buffer memory: top
/// left, top right, bottom left, bottom right.
const QUADRANTS: Pattern = Pattern::Quadrants([RED, GREEN, BLUE, WHITE]);

/// Moves a window and returns the centers of its four quadrants on an
/// output of the given scale: top left, top right, bottom left, bottom
/// right, in physical pixels.
fn place(
    session: &mut Session,
    client: &mut TestClient,
    title: &str,
    id: u64,
    position: (i32, i32),
    size: (u32, u32),
    scale: f64,
) -> [(u32, u32); 4] {
    session.send(Request::MoveWindow {
        window: id,
        x: position.0,
        y: position.1,
    });
    session.wait(&mut [client], "moved", |s| {
        window(s, title).unwrap()["position"] == serde_json::json!([position.0, position.1])
    });
    let at = |fx: f64, fy: f64| {
        (
            ((position.0 as f64 + size.0 as f64 * fx) * scale) as u32,
            ((position.1 as f64 + size.1 as f64 * fy) * scale) as u32,
        )
    };
    [
        at(0.25, 0.25),
        at(0.75, 0.25),
        at(0.25, 0.75),
        at(0.75, 0.75),
    ]
}

fn rgb(color: [u8; 4]) -> [u32; 3] {
    [color[0] as u32, color[1] as u32, color[2] as u32]
}

/// `wl_surface.set_buffer_transform`: the client says how it turned the
/// image inside the buffer, and the compositor shows it upright.
///
/// The expected corners are worked out from the wording of the protocol
/// (the buffer holds the image rotated counter-clockwise by the angle, the
/// flipped ones mirrored around the vertical axis first), not from the
/// implementation.
fn buffer_transforms_are_undone(device: Device) {
    let mut session = Session::start("transform", device, &[]);
    let mut client_options = options("turned", 120, 80, WHITE);
    client_options.pattern = QUADRANTS;
    let (mut client, id) = session.mapped_client(client_options);
    let points = place(
        &mut session,
        &mut client,
        "turned",
        id,
        (40, 30),
        (120, 80),
        1.0,
    );

    // (transform, what is on screen at: top left, top right, bottom left,
    // bottom right)
    let cases = [
        (Transform::Normal, [RED, GREEN, BLUE, WHITE]),
        (Transform::Rotate90, [BLUE, RED, WHITE, GREEN]),
        (Transform::Rotate180, [WHITE, BLUE, GREEN, RED]),
        (Transform::Rotate270, [GREEN, WHITE, RED, BLUE]),
        (Transform::Flipped, [GREEN, RED, WHITE, BLUE]),
        (Transform::Flipped90, [RED, BLUE, GREEN, WHITE]),
        (Transform::Flipped180, [BLUE, WHITE, RED, GREEN]),
        (Transform::Flipped270, [WHITE, GREEN, BLUE, RED]),
    ];
    for (transform, expected) in cases {
        let state = session.redraw(&mut client, "turned", |o| o.buffer_transform = transform);
        // The window stays 120x80 on screen. For the quarter turns the
        // client allocates the buffer the other way around.
        let surface = &window(&state, "turned").unwrap()["surfaces"][0];
        assert_eq!(
            surface["size"],
            serde_json::json!([120, 80]),
            "{transform:?}"
        );
        let buffer = if transform.swaps_axes() {
            [80, 120]
        } else {
            [120, 80]
        };
        assert_eq!(
            surface["buffer_size"],
            serde_json::json!(buffer),
            "{transform:?}"
        );

        let (encoded, _) = session.screenshot(&format!("transform-{}", transform as u32));
        for (point, color) in points.iter().zip(expected) {
            assert_pixel(
                &encoded,
                point.0,
                point.1,
                rgb(color),
                QUADRANT_TOLERANCE,
                &format!("{transform:?} at {point:?}"),
            );
        }
        // The window's extent did not change either.
        assert_pixel(&encoded, 39, 30, [0x20; 3], 0, "left of the window");
        assert_pixel(&encoded, 160, 109, [0x20; 3], 0, "right of the window");
        assert_pixel(
            &encoded,
            159,
            109,
            rgb(expected[3]),
            QUADRANT_TOLERANCE,
            "last pixel",
        );
    }
    session.assert_drawing_clean();
    session.quit();
}

/// `wp_viewport`: a source rectangle crops the buffer, a destination size
/// scales it, and together with a buffer scale and a transform they still
/// pick the right part.
fn viewports_crop_and_scale(device: Device) {
    let mut session = Session::start("viewport", device, &[]);
    let mut client_options = options("cropped", 100, 60, WHITE);
    client_options.pattern = QUADRANTS;
    let (mut client, id) = session.mapped_client(client_options);
    let points = place(
        &mut session,
        &mut client,
        "cropped",
        id,
        (40, 30),
        (100, 60),
        1.0,
    );
    let size_of = |state: &serde_json::Value| {
        window(state, "cropped").unwrap()["surfaces"][0]["size"].clone()
    };
    let check = |session: &mut Session, name: &str, expected: [[u8; 4]; 4]| {
        let (encoded, _) = session.screenshot(name);
        for (point, color) in points.iter().zip(expected) {
            assert_pixel(
                &encoded,
                point.0,
                point.1,
                rgb(color),
                QUADRANT_TOLERANCE,
                name,
            );
        }
        assert_pixel(&encoded, 140, 89, [0x20; 3], 0, "right of the window");
        assert_pixel(&encoded, 139, 90, [0x20; 3], 0, "below the window");
    };

    // Destination only: a 20x12 buffer stretched over 100x60.
    let state = session.redraw(&mut client, "cropped", |o| {
        o.buffer_size = Some((20, 12));
        o.viewport = Some(Viewport {
            source: None,
            destination: Some((100, 60)),
        });
    });
    assert_eq!(size_of(&state), serde_json::json!([100, 60]));
    check(&mut session, "stretched", [RED, GREEN, BLUE, WHITE]);

    // Source only: the top right quadrant of a 200x120 buffer, at its own
    // size.
    let state = session.redraw(&mut client, "cropped", |o| {
        o.buffer_size = Some((200, 120));
        o.viewport = Some(Viewport {
            source: Some((100.0, 0.0, 100.0, 60.0)),
            destination: None,
        });
    });
    assert_eq!(size_of(&state), serde_json::json!([100, 60]));
    check(&mut session, "top-right", [GREEN; 4]);

    // Both, with a buffer scale: the source is in surface coordinates, so
    // on a 400x240 buffer of scale 2 the bottom left quadrant is
    // (0, 60, 100, 60).
    let state = session.redraw(&mut client, "cropped", |o| {
        o.buffer_size = Some((400, 240));
        o.buffer_scale = 2;
        o.viewport = Some(Viewport {
            source: Some((0.0, 60.0, 100.0, 60.0)),
            destination: Some((100, 60)),
        });
    });
    assert_eq!(size_of(&state), serde_json::json!([100, 60]));
    check(&mut session, "bottom-left-scaled", [BLUE; 4]);

    // A source that straddles all four quadrants, scaled down: the middle
    // half of the buffer in each direction.
    session.redraw(&mut client, "cropped", |o| {
        o.buffer_size = Some((200, 120));
        o.buffer_scale = 1;
        o.viewport = Some(Viewport {
            source: Some((50.0, 30.0, 100.0, 60.0)),
            destination: Some((100, 60)),
        });
    });
    check(&mut session, "middle", [RED, GREEN, BLUE, WHITE]);

    // With a transform, the source is in the coordinates of the upright
    // image: the top right quadrant of what is shown, which for a half
    // turn is the buffer's bottom left.
    session.redraw(&mut client, "cropped", |o| {
        o.buffer_transform = Transform::Rotate180;
        o.viewport = Some(Viewport {
            source: Some((100.0, 0.0, 100.0, 60.0)),
            destination: None,
        });
    });
    check(&mut session, "top-right-of-half-turn", [BLUE; 4]);

    session.assert_drawing_clean();
    session.quit();
}

/// `wp_fractional_scale_v1`: on an output of scale 1.5 the client is told
/// 180/120, and a client that follows it (a buffer of 1.5 times the surface
/// size, sized down with a viewport, which is how such clients work) is
/// shown pixel for pixel.
fn fractional_scale_is_announced_and_a_matching_buffer_is_shown_exactly(device: Device) {
    let mut session = Session::start(
        "fractional",
        device,
        &["--output-size", "600x450", "--output-scale", "1.5"],
    );
    let mut client_options = options("fractional", 200, 100, WHITE);
    client_options.pattern = QUADRANTS;
    let (mut client, id) = session.mapped_client(client_options);
    assert!(
        client
            .events()
            .contains(&ClientEvent::FractionalScale { scale_120: 180 }),
        "{:?}",
        client.events()
    );
    // Clients without the protocol are told the next integer.
    assert!(
        client
            .events()
            .contains(&ClientEvent::PreferredBufferScale { scale: 2 })
    );

    let state = session.redraw(&mut client, "fractional", |o| {
        o.buffer_size = Some((300, 150));
        o.viewport = Some(Viewport {
            source: None,
            destination: Some((200, 100)),
        });
    });
    assert_eq!(
        state["outputs"][0]["logical_size"],
        serde_json::json!([400, 300])
    );
    let points = place(
        &mut session,
        &mut client,
        "fractional",
        id,
        (40, 20),
        (200, 100),
        1.5,
    );
    let (encoded, _) = session.screenshot("fractional");
    assert_eq!((encoded.width, encoded.height), (600, 450));
    for (point, color) in points.iter().zip([RED, GREEN, BLUE, WHITE]) {
        assert_pixel(
            &encoded,
            point.0,
            point.1,
            rgb(color),
            QUADRANT_TOLERANCE,
            "quadrant",
        );
    }
    // Physical extent: (60, 30) to (359, 179). The boundary between the
    // quadrants is sharp, because buffer pixels and screen pixels coincide.
    for (x, y, color) in [
        (60, 30, RED),
        (209, 104, RED),
        (210, 104, GREEN),
        (209, 105, BLUE),
        (210, 105, WHITE),
        (359, 179, WHITE),
    ] {
        assert_pixel(
            &encoded,
            x,
            y,
            rgb(color),
            QUADRANT_TOLERANCE,
            "exact pixel",
        );
    }
    for (x, y) in [(59, 30), (360, 179), (60, 29), (359, 180)] {
        assert_pixel(&encoded, x, y, [0x20; 3], 0, "background");
    }
    session.assert_drawing_clean();
    session.quit();
}

/// `wp_presentation`: every frame the client asks about is reported once,
/// with a time that moves forward, the output's refresh period, and the
/// output it was shown on.
fn presentation_feedback_reports_each_frame(device: Device) {
    let mut session = Session::start("presentation", device, &["--output-refresh", "50"]);
    let mut client_options = options("timed", 64, 64, WHITE);
    client_options.presentation_feedback = true;
    let (mut client, _) = session.mapped_client(client_options);
    for color in [RED, GREEN, BLUE] {
        session.recolor(&mut client, "timed", color);
    }
    // Four frames were drawn (the first one and three recolors).
    let deadline = std::time::Instant::now() + TIMEOUT;
    let presented = loop {
        client.pump(Some(Duration::from_millis(20))).unwrap();
        let presented: Vec<_> = client
            .events()
            .iter()
            .filter_map(|e| match e {
                ClientEvent::Presented {
                    time_ns,
                    refresh_ns,
                    sequence,
                    output,
                } => Some((*time_ns, *refresh_ns, *sequence, output.clone())),
                _ => None,
            })
            .collect();
        let discarded = client
            .events()
            .iter()
            .filter(|e| matches!(e, ClientEvent::PresentationDiscarded))
            .count();
        // A frame replaced before it was shown is reported as discarded.
        if presented.len() + discarded >= 4 {
            break presented;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "feedback did not arrive: {:?}",
            client.events()
        );
    };
    assert!(!presented.is_empty());
    for (time_ns, refresh_ns, _, output) in &presented {
        assert!(*time_ns > 0);
        assert_eq!(*refresh_ns, 20_000_000, "50 Hz");
        assert_eq!(output.as_deref(), Some("HEADLESS-1"));
    }
    for pair in presented.windows(2) {
        assert!(pair[1].0 > pair[0].0, "time went backwards: {presented:?}");
        assert!(
            pair[1].2 > pair[0].2,
            "sequence did not advance: {presented:?}"
        );
    }
    session.quit();
}

// ---------------------------------------------------------------------------
// dmabuf
// ---------------------------------------------------------------------------

/// With `--dmabuf cpu` clients may send dmabufs of the four 8 bit RGB
/// layouts in linear memory. The compositor maps and copies them. The
/// client's buffers here are memfds, which stand in for the dmabufs of a
/// driver that allows mapping; no GPU memory is involved.
fn dmabuf_buffers_are_read_and_shown(device: Device) {
    let mut session = Session::start("dmabuf", device, &["--dmabuf", "cpu"]);
    let mut client_options = options("gpu-buffer", 120, 80, WHITE);
    client_options.buffer_kind = BufferKind::Dmabuf;
    client_options.pattern = QUADRANTS;
    let (mut client, id) = session.mapped_client(client_options);

    // What was offered: the four layouts with the linear modifier. A
    // renderer on a DRM device also names that device, with feedback.
    let linear: Vec<[u64; 2]> = ["AR24", "XR24", "AB24", "XB24"]
        .iter()
        .map(|code| {
            [
                u32::from_le_bytes(code.as_bytes().try_into().unwrap()) as u64,
                0,
            ]
        })
        .collect();
    let feedback = client.events().iter().find_map(|e| match e {
        ClientEvent::DmabufFeedback {
            main_device,
            formats,
        } => Some((*main_device, formats.clone())),
        _ => None,
    });
    let mut modifiers: Vec<[u64; 2]> = client
        .events()
        .iter()
        .filter_map(|e| match e {
            ClientEvent::DmabufModifier { fourcc, modifier } => Some([*fourcc as u64, *modifier]),
            _ => None,
        })
        .collect();
    match feedback {
        Some((main_device, mut formats)) => {
            // The DRM character device major.
            assert_eq!(main_device[0], 226, "{main_device:?}");
            formats.sort();
            let mut expected = linear.clone();
            expected.sort();
            assert_eq!(formats, expected);
        }
        None => {
            // No device to name (lavapipe): the plain list of formats.
            assert_eq!(
                device.gpu_name(),
                "lavapipe",
                "a GPU renderer names its device"
            );
            modifiers.sort();
            let mut expected = linear.clone();
            expected.sort();
            assert_eq!(modifiers, expected);
        }
    }

    let points = place(
        &mut session,
        &mut client,
        "gpu-buffer",
        id,
        (40, 30),
        (120, 80),
        1.0,
    );
    let (encoded, _) = session.screenshot("dmabuf");
    for (point, color) in points.iter().zip([RED, GREEN, BLUE, WHITE]) {
        assert_pixel(
            &encoded,
            point.0,
            point.1,
            rgb(color),
            QUADRANT_TOLERANCE,
            "dmabuf quadrant",
        );
    }
    // The buffer is released once it was copied, like a wl_shm buffer.
    client
        .wait_for(TIMEOUT, |e| *e == ClientEvent::BufferReleased)
        .unwrap()
        .expect("the dmabuf is released");

    // Every layout, a transform, and translucency go through the same path
    // as wl_shm afterwards.
    for layout in [
        PixelLayout::Xrgb8888,
        PixelLayout::Abgr8888,
        PixelLayout::Xbgr8888,
    ] {
        session.redraw(&mut client, "gpu-buffer", |o| {
            o.layout = layout;
            o.buffer_transform = Transform::Rotate180;
        });
        let (encoded, _) = session.screenshot(&format!("dmabuf-{layout:?}"));
        for (point, color) in points.iter().zip([WHITE, BLUE, GREEN, RED]) {
            assert_pixel(
                &encoded,
                point.0,
                point.1,
                rgb(color),
                QUADRANT_TOLERANCE,
                &format!("{layout:?}"),
            );
        }
    }
    session.assert_drawing_clean();
    session.quit();
}

/// Without `--dmabuf` the global is not there, so clients fall back to
/// `wl_shm` instead of sending buffers the renderer cannot take.
fn dmabuf_is_not_offered_unless_asked(device: Device) {
    let mut session = Session::start("no-dmabuf", device, &[]);
    let (mut client, _) = session.mapped_client(options("plain", 64, 64, WHITE));
    assert!(!client.events().iter().any(|e| matches!(
        e,
        ClientEvent::DmabufFeedback { .. } | ClientEvent::DmabufModifier { .. }
    )));
    client.redraw_with(|o| o.buffer_kind = BufferKind::Dmabuf);
    let error = client.pump(Some(Duration::from_millis(50))).unwrap_err();
    assert!(error.to_string().contains("zwp_linux_dmabuf_v1"), "{error}");
    session.quit();
}

// ---------------------------------------------------------------------------
// Several outputs
// ---------------------------------------------------------------------------

/// Two outputs side by side, the second with scale 2. Each is drawn with
/// the windows that are on it, at its own scale; a window that straddles
/// the boundary shows up on both.
fn two_outputs_each_show_what_is_on_them(device: Device) {
    let mut session = Session::start(
        "two-outputs",
        device,
        &[
            "--output-size",
            "400x300",
            "--output-size",
            "300x200",
            "--output-scale",
            "1",
            "--output-scale",
            "2",
        ],
    );
    let state = session.state();
    let outputs = state["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0]["name"], "HEADLESS-1");
    assert_eq!(outputs[1]["name"], "HEADLESS-2");
    assert_eq!(outputs[1]["position"], serde_json::json!([400, 0]));
    assert_eq!(outputs[1]["logical_size"], serde_json::json!([150, 100]));

    let (mut left, left_id) = session.mapped_client(options("left", 100, 60, RED));
    let (mut right, right_id) = session.mapped_client(options("right", 60, 40, BLUE));
    place(
        &mut session,
        &mut left,
        "left",
        left_id,
        (20, 30),
        (100, 60),
        1.0,
    );
    // Global (420, 10) is (20, 10) on the second output.
    place(
        &mut session,
        &mut right,
        "right",
        right_id,
        (420, 10),
        (60, 40),
        1.0,
    );
    let state = session.wait(&mut [&mut left, &mut right], "placed on outputs", |s| {
        window(s, "right").unwrap()["output"] == 2
    });
    assert_eq!(window(&state, "left").unwrap()["output"], 1);
    // The window on the scale 2 output was told so.
    right
        .wait_for(TIMEOUT, |e| {
            *e == ClientEvent::PreferredBufferScale { scale: 2 }
        })
        .unwrap()
        .expect("the right window is told the scale of its output");

    let (first, _) = session.screenshot_of("first", Some("HEADLESS-1"));
    assert_eq!((first.width, first.height), (400, 300));
    assert_pixel(&first, 20, 30, rgb(RED), QUADRANT_TOLERANCE, "left window");
    assert_pixel(&first, 119, 89, rgb(RED), QUADRANT_TOLERANCE, "left window");
    assert_pixel(&first, 120, 89, [0x20; 3], 0, "background");
    // Nothing of the right window, which is entirely on the other output.
    for (x, y) in [(399, 10), (399, 49), (380, 30)] {
        assert_pixel(&first, x, y, [0x20; 3], 0, "background at the edge");
    }

    let (second, _) = session.screenshot_of("second", Some("HEADLESS-2"));
    assert_eq!((second.width, second.height), (300, 200));
    // Logical (20, 10) to (79, 49) on this output: physical (40, 20) to
    // (159, 99).
    for (x, y) in [(40, 20), (159, 99), (100, 60)] {
        assert_pixel(&second, x, y, rgb(BLUE), QUADRANT_TOLERANCE, "right window");
    }
    for (x, y) in [
        (39, 20),
        (160, 99),
        (40, 19),
        (159, 100),
        (0, 0),
        (299, 199),
    ] {
        assert_pixel(&second, x, y, [0x20; 3], 0, "background");
    }

    // A window across the boundary: 100 wide from global x 350, so 50 on
    // each side.
    place(
        &mut session,
        &mut left,
        "left",
        left_id,
        (350, 30),
        (100, 60),
        1.0,
    );
    let (first, _) = session.screenshot_of("first-straddle", Some("HEADLESS-1"));
    assert_pixel(
        &first,
        350,
        30,
        rgb(RED),
        QUADRANT_TOLERANCE,
        "straddling window",
    );
    assert_pixel(
        &first,
        399,
        89,
        rgb(RED),
        QUADRANT_TOLERANCE,
        "straddling window",
    );
    assert_pixel(&first, 349, 30, [0x20; 3], 0, "left of it");
    let (second, _) = session.screenshot_of("second-straddle", Some("HEADLESS-2"));
    // Logical x 0..50 on the second output, y 30..90: physical x 0..100,
    // y 60..180.
    assert_pixel(
        &second,
        0,
        60,
        rgb(RED),
        QUADRANT_TOLERANCE,
        "its right half",
    );
    assert_pixel(
        &second,
        99,
        179,
        rgb(RED),
        QUADRANT_TOLERANCE,
        "its right half",
    );
    assert_pixel(&second, 100, 179, [0x20; 3], 0, "right of it");
    assert_pixel(&second, 99, 180, [0x20; 3], 0, "below it");

    // The pointer works across outputs in the one global space.
    right.clear_events();
    session.send(Request::PointerMotion { x: 425.0, y: 15.0 });
    let entered = right
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::PointerEnter { .. }))
        .unwrap();
    assert_eq!(entered, Some(ClientEvent::PointerEnter { x: 5.0, y: 5.0 }));

    session.assert_drawing_clean();
    session.quit();
}
