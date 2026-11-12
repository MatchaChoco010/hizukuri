//! The script layer in the real binary: drawing,
//! hit testing and window management written in a script, on a headless
//! backend, on each device listed in `HIZUKURI_TEST_DEVICES`.
//!
//! `tests/headless.rs` runs the drawing scenarios that any compositor has,
//! with the script drawing. The scenarios here are about what only the
//! script layer has.

mod common;

use std::os::fd::AsFd;

use common::{DRAW_ONLY_SESSION, Device, Session, options, script_project, window};
use proto_color::{AlphaMode, CompositeSpace, DecodeTransform, ImageDescription, Primaries};
use proto_compositor::control::Request;
use proto_compositor::image::{CompositeImage, EncodedImage};
use proto_test_client::{
    ClientEvent, ClientOptions, ConstraintRequest, DialogOptions, ExternalDmabuf, GestureKind,
    InputMethodMode, Pattern, PixelLayout, PopupOptions, SurfaceKind, TestClient, Transform,
    create_udmabuf,
};
use serde_json::{Value, json};

const RED: [u8; 4] = [210, 60, 50, 255];
const GREEN: [u8; 4] = [60, 190, 70, 255];
const BLUE: [u8; 4] = [50, 80, 210, 255];
const WHITE: [u8; 4] = [235, 235, 235, 255];
const QUADRANTS: Pattern = Pattern::Quadrants([RED, GREEN, BLUE, WHITE]);

device_tests!(
    a_mixed_scene_matches_the_cpu_reference,
    the_working_space_is_the_scripts_choice,
    a_script_sees_what_the_core_knows_about_a_surface,
    real_dmabufs_become_textures_without_a_copy,
    imported_dmabufs_are_shown_upright,
    frames_are_drawn_only_when_something_changed,
    the_icon_of_a_drag_is_drawn_by_the_script,
    the_popup_of_an_input_method_is_drawn_by_the_cursor,
    a_transformed_window_is_drawn_and_hit_where_the_script_put_it,
    a_press_holds_the_pointer_on_a_transformed_window,
    popups_follow_the_transform_of_their_window,
    the_script_decides_what_a_point_hits,
    windows_are_tiled_by_the_script,
    a_window_manager_script_hears_what_happens,
    a_script_that_fails_to_draw_leaves_a_usable_session,
    holding_the_last_frame_while_the_script_fails,
    a_script_that_fails_on_events_is_taken_out_of_charge,
    a_script_is_reloaded_while_windows_stay_open,
    a_module_that_does_not_compile_is_not_taken,
    the_script_decides_when_the_pointer_is_held,
    colors_set_through_the_protocol_are_drawn,
    the_script_takes_the_gestures_it_wants,
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn move_window(session: &mut Session, client: &mut TestClient, title: &str, x: i32, y: i32) {
    let id = window(&session.state(), title).expect("window exists")["id"]
        .as_u64()
        .unwrap();
    session.send(Request::MoveWindow { window: id, x, y });
    session.wait(&mut [client], "moved", |s| {
        window(s, title).unwrap()["position"] == json!([x, y])
    });
}

#[track_caller]
fn assert_rgb(image: &EncodedImage, x: u32, y: u32, expected: [u8; 4], tolerance: i32, what: &str) {
    let actual = image.pixel(x, y);
    let worst = (0..3)
        .map(|i| (actual[i] as i32 - expected[i] as i32).abs())
        .max()
        .unwrap();
    assert!(
        worst <= tolerance,
        "{what}: pixel ({x}, {y}) is {actual:?}, expected {expected:?} within {tolerance}"
    );
}

/// A 2D affine transform, laid out like the script's: `(x, y)` goes to
/// `(a*x + b*y + tx, c*x + d*y + ty)`.
#[derive(Debug, Clone, Copy)]
struct Affine([f64; 6]);

impl Affine {
    fn translate(x: f64, y: f64) -> Affine {
        Affine([1.0, 0.0, 0.0, 1.0, x, y])
    }

    fn scale(x: f64, y: f64) -> Affine {
        Affine([x, 0.0, 0.0, y, 0.0, 0.0])
    }

    fn rotate(degrees: f64) -> Affine {
        let (s, c) = degrees.to_radians().sin_cos();
        Affine([c, -s, s, c, 0.0, 0.0])
    }

    fn shear(kx: f64) -> Affine {
        Affine([1.0, kx, 0.0, 1.0, 0.0, 0.0])
    }

    /// `self` after `first`.
    fn after(self, first: Affine) -> Affine {
        let (m, n) = (self.0, first.0);
        Affine([
            m[0] * n[0] + m[1] * n[2],
            m[0] * n[1] + m[1] * n[3],
            m[2] * n[0] + m[3] * n[2],
            m[2] * n[1] + m[3] * n[3],
            m[0] * n[4] + m[1] * n[5] + m[4],
            m[2] * n[4] + m[3] * n[5] + m[5],
        ])
    }

    fn apply(self, (x, y): (f64, f64)) -> (f64, f64) {
        let m = self.0;
        (m[0] * x + m[1] * y + m[4], m[2] * x + m[3] * y + m[5])
    }

    fn invert(self) -> Affine {
        let m = self.0;
        let det = m[0] * m[3] - m[1] * m[2];
        let (a, b, c, d) = (m[3] / det, -m[1] / det, -m[2] / det, m[0] / det);
        Affine([a, b, c, d, -(a * m[4] + b * m[5]), -(c * m[4] + d * m[5])])
    }

    fn json(self) -> Value {
        json!(self.0)
    }
}

/// A window turned by 30 degrees, sheared, and scaled differently in x and
/// y, around the point (`cx`, `cy`) of the window, which lands on
/// (`x`, `y`) of the screen.
fn skewed(cx: f64, cy: f64, x: f64, y: f64) -> Affine {
    Affine::translate(x, y)
        .after(Affine::rotate(30.0))
        .after(Affine::shear(0.3))
        .after(Affine::scale(1.5, 1.2))
        .after(Affine::translate(-cx, -cy))
}

fn pointer_to(session: &mut Session, (x, y): (f64, f64)) {
    session.send(Request::PointerMotion { x, y });
}

fn button(session: &mut Session, pressed: bool) {
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed,
    });
}

const BTN_LEFT: u32 = 0x110;

/// The surface-local position the pointer was last reported at.
fn pointer_position(client: &TestClient) -> Option<(f64, f64)> {
    client.events().iter().rev().find_map(|e| match e {
        ClientEvent::PointerEnter { x, y } | ClientEvent::PointerMotion { x, y } => Some((*x, *y)),
        _ => None,
    })
}

/// Moves the pointer to a point of the screen and returns where the client
/// sees it: on which of its surfaces, and at which surface-local position.
fn point_at(
    session: &mut Session,
    client: &mut TestClient,
    global: (f64, f64),
) -> Option<(SurfaceKind, (f64, f64))> {
    client.clear_events();
    pointer_to(session, global);
    // The answer to a control request is sent after the events it caused.
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerFrame)
        .unwrap()?;
    client
        .pump(Some(std::time::Duration::from_millis(20)))
        .unwrap();
    let kind = client.pointer_focus()?;
    Some((kind, pointer_position(client)?))
}

/// Wayland sends positions as 24.8 fixed point; the transform is computed
/// in single precision on the way.
#[track_caller]
fn assert_near(actual: (f64, f64), expected: (f64, f64), what: &str) {
    assert!(
        (actual.0 - expected.0).abs() <= 0.02 && (actual.1 - expected.1).abs() <= 0.02,
        "{what}: the client saw {actual:?}, expected {expected:?}"
    );
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// One scene with an opaque window with a subsurface, a translucent window
/// with its own image description and an opacity on top of it, and the
/// cursor.
fn draw_scene(name: &str, device: Device, args: &[&str]) -> (EncodedImage, CompositeImage) {
    let mut all = vec!["--output-size", "320x240", "--cursor", "on"];
    all.extend_from_slice(args);
    let mut session = Session::start(name, device, &all);

    let (mut back, _) = session.mapped_client(ClientOptions {
        layout: PixelLayout::Xrgb8888,
        pattern: QUADRANTS,
        ..options("back", 160, 120, WHITE)
    });
    move_window(&mut session, &mut back, "back", 20, 20);
    back.add_subsurface(100, 30, 90, 40, [200, 200, 200, 0])
        .unwrap();
    session.wait(&mut [&mut back], "subsurface mapped", |s| {
        let surfaces = &window(s, "back").unwrap()["surfaces"];
        surfaces.as_array().unwrap().len() == 2 && surfaces[1]["buffer_size"].is_array()
    });

    // Premultiplied, half transparent.
    let (mut glass, glass_id) =
        session.mapped_client(options("glass", 120, 90, [120, 60, 30, 128]));
    move_window(&mut session, &mut glass, "glass", 100, 80);
    session.send(Request::SetWindowOpacity {
        window: glass_id,
        opacity: 0.7,
    });
    let surface = window(&session.state(), "glass").unwrap()["surface"]
        .as_u64()
        .unwrap();
    session.send(Request::SetSurfaceColor {
        surface,
        image_description: "display-p3".into(),
        alpha_mode: None,
    });
    session.wait(&mut [&mut glass], "description applied", |s| {
        window(s, "glass").unwrap()["surfaces"][0]["image_description"]["name"] == "display-p3"
    });
    // Over the translucent window, between pixel centers.
    session.send(Request::PointerMotion {
        x: 150.5,
        y: 100.25,
    });

    let capture = session.screenshot("scene");
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
    capture
}

/// The scene of [`draw_scene`] against the CPU reference in `proto-color`,
/// on four outputs: where the background, an opaque window, and a
/// translucent window with its own image description and an opacity are
/// seen, the compositing buffer and the encoded output hold what the
/// reference computes.
fn a_mixed_scene_matches_the_cpu_reference(device: Device) {
    use proto_color::{EncodeTransform, blend_over, quantize_unorm};
    const SPACE: CompositeSpace = CompositeSpace::BT2020_LINEAR;
    let decode = |description: &ImageDescription, rgba: [u8; 4]| {
        DecodeTransform::new(description, AlphaMode::PremultipliedElectrical, &SPACE)
            .unwrap()
            .apply(rgba.map(|v| v as f32 / 255.0))
    };
    let unmanaged = ImageDescription::SRGB_GAMMA22;
    let background = decode(&unmanaged, [0x20, 0x20, 0x20, 0xff]);
    let red = decode(&unmanaged, RED);
    let white = decode(&unmanaged, WHITE);
    // Premultiplied Display P3, drawn with opacity 0.7.
    let glass = decode(&ImageDescription::DISPLAY_P3, [120, 60, 30, 128]).map(|v| v * 0.7);
    // (what, logical point, expected compositing value)
    let points = [
        ("background", (5.0, 5.0), background),
        ("opaque window", (30.0, 30.0), red),
        (
            "glass over a window",
            (130.0, 110.0),
            blend_over(white, glass),
        ),
        (
            "glass over the background",
            (200.0, 150.0),
            blend_over(background, glass),
        ),
    ];

    /// One output setup.
    struct Case {
        label: &'static str,
        args: &'static [&'static str],
        output: ImageDescription,
        bits: u32,
        /// Allowed difference in code values. At 16 bit the half float
        /// error of the compositing buffer is several code values; at 8 bit,
        /// a pure power curve stretches the error near zero to up to 3.
        tolerance: u32,
        scale: f64,
    }
    let sixteen_bit = |label, description, output| Case {
        label,
        args: description,
        output,
        bits: 16,
        tolerance: 64,
        scale: 1.0,
    };
    let cases = [
        Case {
            label: "sdr",
            args: &[],
            output: unmanaged,
            bits: 8,
            tolerance: 3,
            scale: 1.0,
        },
        sixteen_bit(
            "p3-16bit",
            &[
                "--output-image-description",
                "display-p3",
                "--output-format",
                "rgba16",
            ],
            ImageDescription::DISPLAY_P3,
        ),
        sixteen_bit(
            "pq",
            &[
                "--output-image-description",
                "bt2020-pq",
                "--output-format",
                "rgba16",
            ],
            ImageDescription::BT2020_PQ,
        ),
        Case {
            label: "scale-1.5",
            args: &["--output-scale", "1.5"],
            output: unmanaged,
            bits: 8,
            tolerance: 3,
            scale: 1.5,
        },
    ];
    for Case {
        label,
        args,
        output,
        bits,
        tolerance,
        scale,
    } in cases
    {
        let (encoded, composite) = draw_scene(&format!("reference-{label}"), device, args);
        let at = |(x, y): (f64, f64)| ((x * scale) as u32, (y * scale) as u32);
        for (what, point, expected) in points {
            let (x, y) = at(point);
            let actual = composite.pixel(x, y);
            for i in 0..4 {
                let allowed = 2e-5 + expected[i].abs() * 1e-3;
                assert!(
                    (actual[i] - expected[i]).abs() <= allowed,
                    "{label}, {what}: compositing value {actual:?}, expected {expected:?}"
                );
            }
            let code = EncodeTransform::new(&SPACE, &output)
                .unwrap()
                .apply([expected[0], expected[1], expected[2]])
                .map(|v| quantize_unorm(v, bits));
            let pixel = encoded.pixel(x, y);
            let worst = (0..3)
                .map(|i| (pixel[i] as i64 - code[i] as i64).unsigned_abs())
                .max()
                .unwrap();
            assert!(
                worst <= tolerance as u64,
                "{label}, {what}: pixel {pixel:?}, expected {code:?} within {tolerance}"
            );
        }
        // The cursor is in the scene, over the glass.
        let pixel = |point| {
            let (x, y) = at(point);
            encoded.pixel(x, y)
        };
        assert_ne!(
            pixel((152.0, 104.0)),
            pixel((140.0, 104.0)),
            "{label}: cursor"
        );
    }
}

/// Waits for the answer to image description `index`; panics if it failed.
fn ready(session: &mut Session, client: &mut TestClient, index: usize) {
    let answered = |client: &TestClient| {
        client.events().iter().find_map(|e| match e {
            ClientEvent::ImageDescriptionReady { index: i, .. } if *i == index => Some(Ok(())),
            ClientEvent::ImageDescriptionFailed {
                index: i, message, ..
            } if *i == index => Some(Err(message.clone())),
            _ => None,
        })
    };
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while answered(client).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "no answer for {index}"
        );
        session.wait(&mut [client], "a pump", |_| true);
    }
    if let Some(Err(message)) = answered(client) {
        panic!("image description {index} failed: {message}");
    }
}

/// Image descriptions set through color-management-v1 reach the pixels:
/// a surface described through the protocol is drawn like one described
/// through the control interface with the same description, and both like
/// the CPU reference of `proto-color`. Where the control interface has no
/// preset for it (HLG, BT.1886, the Windows scRGB encoding, a power
/// function with luminances of its own), against the reference alone.
fn colors_set_through_the_protocol_are_drawn(device: Device) {
    const TF_BT1886: u32 = 1;
    const TF_EXT_LINEAR: u32 = 5;
    const TF_PQ: u32 = 11;
    const TF_HLG: u32 = 13;
    const TF_COMPOUND_POWER_2_4: u32 = 14;
    const PRIMARIES_SRGB: u32 = 1;
    const PRIMARIES_BT2020: u32 = 6;
    const PRIMARIES_DISPLAY_P3: u32 = 9;

    let mut session = Session::start("protocol-colors", device, &["--output-size", "256x128"]);
    let color = [180u8, 120, 60, 255];
    let protocol_options = ClientOptions {
        color_manager_version: Some(3),
        ..options("protocol", 80, 60, color)
    };
    let (mut protocol, _) = session.mapped_client(protocol_options);
    let (mut control, _) = session.mapped_client(options("control", 80, 60, color));
    move_window(&mut session, &mut protocol, "protocol", 20, 30);
    move_window(&mut session, &mut control, "control", 140, 30);
    let control_surface = window(&session.state(), "control").unwrap()["surface"]
        .as_u64()
        .unwrap();
    protocol
        .create_color_surface(proto_test_client::ColorTarget::Window)
        .unwrap();

    use proto_color::{Luminance, TransferFunction};
    use proto_test_client::CreatorStep as S;
    let windows_scrgb = ImageDescription {
        primaries: Primaries::SRGB,
        transfer: TransferFunction::Linear,
        luminance: Luminance {
            min: 0.0,
            max: 80.0,
            reference: 203.0,
        },
        mastering: None,
    };
    let adobe = [
        640000, 330000, 210000, 710000, 150000, 60000, 312700, 329000,
    ];
    let custom = ImageDescription {
        primaries: Primaries::ADOBE_RGB,
        transfer: TransferFunction::Power(1.8),
        luminance: Luminance {
            min: 0.0,
            max: 200.0,
            reference: 100.0,
        },
        mastering: None,
    };
    // (label, how the protocol client makes it, the control preset, what
    // it is).
    type Steps = Option<Vec<proto_test_client::CreatorStep>>;
    let cases: Vec<(&str, Steps, Option<&str>, ImageDescription)> = vec![
        (
            "display-p3",
            Some(vec![
                S::TfNamed(TF_COMPOUND_POWER_2_4),
                S::PrimariesNamed(PRIMARIES_DISPLAY_P3),
            ]),
            Some("display-p3"),
            ImageDescription::DISPLAY_P3,
        ),
        (
            "bt2020-pq",
            Some(vec![S::TfNamed(TF_PQ), S::PrimariesNamed(PRIMARIES_BT2020)]),
            Some("bt2020-pq"),
            ImageDescription::BT2020_PQ,
        ),
        (
            "linear-bt709",
            Some(vec![
                S::TfNamed(TF_EXT_LINEAR),
                S::PrimariesNamed(PRIMARIES_SRGB),
            ]),
            Some("scrgb-linear"),
            ImageDescription::SCRGB_LINEAR,
        ),
        ("windows-scrgb", None, None, windows_scrgb),
        (
            "hlg",
            Some(vec![
                S::TfNamed(TF_HLG),
                S::PrimariesNamed(PRIMARIES_BT2020),
            ]),
            None,
            ImageDescription::new(Primaries::BT2020, TransferFunction::Hlg),
        ),
        (
            "bt1886",
            Some(vec![
                S::TfNamed(TF_BT1886),
                S::PrimariesNamed(PRIMARIES_SRGB),
            ]),
            None,
            ImageDescription::new(Primaries::SRGB, TransferFunction::Bt1886),
        ),
        (
            "power-1.8-adobe",
            Some(vec![
                S::TfPower(18000),
                S::Primaries(adobe),
                S::Luminances(0, 200, 100),
            ]),
            None,
            custom,
        ),
    ];
    let space = CompositeSpace::default();
    for (label, steps, preset, description) in cases {
        let index = match steps {
            Some(steps) => protocol.create_image_description(&steps).unwrap(),
            None => protocol.create_windows_description(false).unwrap(),
        };
        ready(&mut session, &mut protocol, index);
        protocol.set_image_description(index, 0).unwrap();
        protocol.commit().unwrap();
        let pipeline =
            DecodeTransform::describe(&description, AlphaMode::PremultipliedElectrical, &space);
        session.wait(&mut [&mut protocol], "the description applied", |s| {
            let surface = &window(s, "protocol").unwrap()["surfaces"][0];
            surface["color_source"] == "protocol" && surface["decode_pipeline"] == pipeline
        });
        if let Some(preset) = preset {
            session.send(Request::SetSurfaceColor {
                surface: control_surface,
                image_description: preset.into(),
                alpha_mode: None,
            });
        }
        let (encoded, composite) = session.screenshot(label);
        let expected =
            DecodeTransform::new(&description, AlphaMode::PremultipliedElectrical, &space)
                .unwrap()
                .apply(color.map(|v| v as f32 / 255.0));
        let actual = composite.pixel(60, 60);
        for i in 0..4 {
            assert!(
                (actual[i] - expected[i]).abs() <= 2e-5 + expected[i].abs() * 2e-3,
                "{label}: the compositing buffer holds {actual:?}, expected {expected:?}"
            );
        }
        if preset.is_some() {
            let (p, c) = (composite.pixel(60, 60), composite.pixel(180, 60));
            for i in 0..4 {
                assert!(
                    (p[i] - c[i]).abs() <= 1e-3 * p[i].abs().max(1.0),
                    "{label}: protocol {p:?}, control {c:?}"
                );
            }
            let (p, c) = (encoded.pixel(60, 60), encoded.pixel(180, 60));
            assert_eq!(p, c, "{label}: protocol and control encode differently");
        }
        println!("{label}: {actual:?}");
    }
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// The script decides which primaries it composites in. The compositing
/// buffer then holds other numbers for the same color; the output does not
/// change (for colors inside every gamut involved).
fn the_working_space_is_the_scripts_choice(device: Device) {
    let mut session = Session::start("working-space", device, &["--output-size", "256x192"]);
    let color = [200u8, 100, 50, 255];
    let (mut client, _) = session.mapped_client(options("fill", 100, 60, color));
    move_window(&mut session, &mut client, "fill", 40, 30);
    assert_eq!(session.script("working_space", json!([])), "bt2020");

    let expected = |primaries: Primaries| {
        DecodeTransform::new(
            &ImageDescription::SRGB_GAMMA22,
            AlphaMode::PremultipliedElectrical,
            &CompositeSpace { primaries },
        )
        .unwrap()
        .apply(color.map(|v| v as f32 / 255.0))
    };
    let mut outputs = Vec::new();
    for (name, primaries) in [
        ("bt2020", Primaries::BT2020),
        ("srgb", Primaries::SRGB),
        ("display-p3", Primaries::DISPLAY_P3),
        ("adobe-rgb", Primaries::ADOBE_RGB),
    ] {
        session.script("set_working_space", json!([name]));
        assert_eq!(session.script("working_space", json!([])), name);
        let (encoded, composite) = session.screenshot(name);
        assert_eq!(composite.space.primaries, primaries, "{name}");
        let (actual, wanted) = (composite.pixel(90, 60), expected(primaries));
        for i in 0..4 {
            assert!(
                (actual[i] - wanted[i]).abs() <= 2e-5 + wanted[i].abs() * 1e-3,
                "{name}: the compositing buffer holds {actual:?}, expected {wanted:?}"
            );
        }
        assert_rgb(&encoded, 90, 60, color, 1, name);
        assert_rgb(&encoded, 5, 5, [0x20, 0x20, 0x20, 255], 0, "background");
        outputs.push(wanted);
    }
    // The numbers really are different between the spaces.
    assert!((outputs[0][0] - outputs[1][0]).abs() > 0.05);

    // Primaries that are not a named set: a table of chromaticities (here
    // ACEScg, AP1 with its own white point).
    let ap1 = json!([0.713, 0.293, 0.165, 0.830, 0.128, 0.044, 0.32168, 0.33767]);
    session.script("set_working_space", json!([ap1]));
    let (encoded, composite) = session.screenshot("ap1");
    assert!((composite.space.primaries.red.x - 0.713).abs() < 1e-6);
    assert_rgb(&encoded, 90, 60, color, 1, "AP1");

    // An unknown name is an error the caller sees, and changes nothing.
    let error = session
        .try_send(Request::ScriptCall {
            function: "set_working_space".into(),
            args: vec![json!("no-such-space")],
        })
        .unwrap_err();
    assert!(error.contains("unknown primaries"), "{error}");
    // The calls refused above were recorded.
    session.assert_script_running(1);
    session.assert_drawing_clean();
    session.quit();
}

/// What the core shows a script about a surface: its texture, sizes, scale,
/// what its values mean, what changed with an update and when it updated.
fn a_script_sees_what_the_core_knows_about_a_surface(device: Device) {
    let project = script_project(
        "inspect",
        device,
        &[
            ("session", DRAW_ONLY_SESSION),
            (
                "main",
                r#"
            hz.include("session")
            function inspect(title)
                for _, w in hz.windows() do
                    if w.title == title then
                        local surfaces = hz.surfaces(w.id)
                        for _, s in surfaces do
                            s.damage = hz.damage(s.id)
                            s.color_info = hz.color_info(s.color)
                        end
                        return { window = w, surfaces = surfaces, outputs = hz.outputs() }
                    end
                end
                return nil
            end
            function gfx_destroy(handle)
                gfx.destroy(handle)
            end
            "#,
            ),
        ],
    );
    let mut session = Session::start(
        "inspect",
        device,
        &[
            "--output-size",
            "512x384",
            "--output-scale",
            "2",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let (mut client, id) = session.mapped_client(ClientOptions {
        buffer_scale: 2,
        ..options("seen", 100, 60, RED)
    });
    move_window(&mut session, &mut client, "seen", 40, 30);
    // A frame was drawn with it.
    session.screenshot("first");

    let seen = session.script("inspect", json!(["seen"]));
    let (w, s) = (&seen["window"], &seen["surfaces"][0]);
    assert_eq!(w["id"], id);
    assert_eq!((&w["x"], &w["y"]), (&json!(40), &json!(30)));
    assert_eq!((&w["width"], &w["height"]), (&json!(100), &json!(60)));
    assert_eq!(w["focused"], true);
    assert_eq!(w["opacity"], 1.0);
    assert_eq!(s["role"], "toplevel");
    // Logical size, buffer size and the scale between them.
    assert_eq!((&s["width"], &s["height"]), (&json!(100), &json!(60)));
    assert_eq!(
        (&s["buffer_width"], &s["buffer_height"]),
        (&json!(200), &json!(120))
    );
    assert_eq!(s["scale"], 2);
    assert_eq!(s["kind"], "shm");
    assert_eq!(s["opaque"], false);
    // The whole texture is shown, upright.
    assert_eq!(s["uv"], json!([1, 0, 0, 1, 0, 0]));
    // What its values mean.
    assert_eq!(s["alpha_mode"], "premultiplied-electrical");
    assert_eq!(s["color_info"]["name"], "srgb-gamma22");
    assert_eq!(s["color_info"]["transfer"], "gamma22");
    assert_eq!(s["color_info"]["primaries"], "srgb");
    // What the client marked as changed: the whole buffer.
    assert_eq!(s["damage"], json!([[0, 0, 200, 120]]));
    let (generation, serial) = (
        s["generation"].as_u64().unwrap(),
        s["serial"].as_u64().unwrap(),
    );
    assert!(s["texture"].is_number() && s["image"].is_number());
    // The output it is drawn to.
    let output = &seen["outputs"][0];
    assert_eq!(
        (&output["width"], &output["height"]),
        (&json!(512), &json!(384))
    );
    assert_eq!(output["scale"], 2.0);
    assert_eq!(output["logical_width"], 256);
    assert_eq!(output["format"], "rgba8unorm");
    assert_eq!(output["color"]["name"], "srgb-gamma22");

    // New pixels of the same shape: the same texture, one upload later.
    session.recolor(&mut client, "seen", GREEN);
    session.screenshot("recolored");
    let s2 = session.script("inspect", json!(["seen"]))["surfaces"][0].take();
    assert_eq!(s2["texture"], s["texture"]);
    assert_eq!(s2["generation"].as_u64().unwrap(), generation);
    assert_eq!(s2["serial"].as_u64().unwrap(), serial + 1);

    // Another size: a new texture.
    session.redraw(&mut client, "seen", |o| o.buffer_size = Some((100, 60)));
    session.screenshot("resized");
    let s3 = session.script("inspect", json!(["seen"]))["surfaces"][0].take();
    assert_eq!(s3["generation"].as_u64().unwrap(), generation + 1);
    assert_eq!(
        (&s3["buffer_width"], &s3["buffer_height"]),
        (&json!(100), &json!(60))
    );

    // Its meaning changes without new pixels.
    session.send(Request::SetSurfaceColor {
        surface: s["id"].as_u64().unwrap(),
        image_description: "bt2020-pq".into(),
        alpha_mode: None,
    });
    session.screenshot("pq");
    let s4 = session.script("inspect", json!(["seen"]))["surfaces"][0].take();
    assert_eq!(s4["color_info"]["name"], "bt2020-pq");
    assert_eq!(s4["color_info"]["transfer"], "pq");
    assert_ne!(s4["color"], s["color"]);

    // A script cannot destroy or overwrite what belongs to the core.
    let texture = s4["texture"].clone();
    let image = s4["image"].clone();
    for handle in [texture, image] {
        let error = session
            .try_send(Request::ScriptCall {
                function: "gfx_destroy".into(),
                args: vec![handle],
            })
            .unwrap_err();
        assert!(error.contains("belongs to the compositor"), "{error}");
    }
    // The texture is still there and still drawn.
    let (encoded, _) = session.screenshot("still-there");
    assert_ne!(encoded.pixel(100, 80), encoded.pixel(5, 5));
    // The calls refused above were recorded.
    session.assert_script_running(2);
    session.assert_drawing_clean();
    session.quit();
}

/// A real dma-buf (system memory behind `/dev/udmabuf`) is imported into the
/// device and sampled; no pixels are copied on the CPU. What looks like a
/// dma-buf on the wire but is not one is read through a mapping instead.
fn real_dmabufs_become_textures_without_a_copy(device: Device) {
    const SIZE: (u32, u32) = (64, 48);
    let mut pixels = Vec::new();
    for y in 0..SIZE.1 {
        for x in 0..SIZE.0 {
            let index = (y * 2 / SIZE.1) * 2 + (x * 2 / SIZE.0);
            // XRGB8888 in memory: B, G, R, X.
            let [r, g, b, _] = [RED, GREEN, BLUE, WHITE][index as usize];
            pixels.extend_from_slice(&[b, g, r, 0]);
        }
    }
    if !common::udmabuf_wanted() {
        return;
    }
    let dmabuf = create_udmabuf(&pixels)
        .expect("creating a udmabuf")
        .expect("/dev/udmabuf can be opened");

    let mut session = Session::start(
        "dmabuf-import",
        device,
        &["--output-size", "256x192", "--dmabuf", "gpu"],
    );
    let (mut client, _) = session.mapped_client(options("gpu-buffer", SIZE.0, SIZE.1, WHITE));
    move_window(&mut session, &mut client, "gpu-buffer", 40, 30);
    let before = session.script_state()["dmabuf"].take();
    assert_eq!(before["imported"], 0);

    let xrgb = u32::from_le_bytes(*b"XR24");
    let attach = |client: &mut TestClient| {
        client
            .attach_dmabuf(&ExternalDmabuf {
                fd: dmabuf.as_fd(),
                width: SIZE.0,
                height: SIZE.1,
                fourcc: xrgb,
                modifier: 0, // DRM_FORMAT_MOD_LINEAR
                offset: 0,
                stride: SIZE.0 * 4,
                transform: Transform::Normal,
                y_invert: false,
            })
            .unwrap();
    };
    let generation = |s: &Value| {
        window(s, "gpu-buffer").unwrap()["surfaces"][0]["generation"]
            .as_u64()
            .unwrap()
    };
    let first = generation(&session.state());
    attach(&mut client);
    let state = session.wait(&mut [&mut client], "the dma-buf arrived", |s| {
        generation(s) > first
    });
    assert_eq!(
        window(&state, "gpu-buffer").unwrap()["surfaces"][0]["buffer_kind"],
        "dmabuf"
    );
    let (encoded, _) = session.screenshot("imported");
    for ((fx, fy), color) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)]
        .into_iter()
        .zip([RED, GREEN, BLUE, WHITE])
    {
        let x = 40 + (SIZE.0 as f64 * fx) as u32;
        let y = 30 + (SIZE.1 as f64 * fy) as u32;
        assert_rgb(&encoded, x, y, color, 1, "imported quadrant");
    }
    let after = session.script_state()["dmabuf"].take();
    assert_eq!(after["imported"], 1, "{after}");
    assert_eq!(after["fallbacks"], 0, "{after}");
    assert_eq!(after["held_buffers"], 1, "{after}");

    // The same buffer again (what a client with one buffer does): the
    // import is found again, not made again.
    let second = generation(&session.state());
    attach(&mut client);
    session.wait(&mut [&mut client], "the dma-buf arrived again", |s| {
        generation(s) > second
    });
    session.screenshot("again");
    let again = session.script_state()["dmabuf"].take();
    assert_eq!(
        (&again["imported"], &again["reused"]),
        (&json!(1), &json!(1)),
        "{again}"
    );

    // Back to wl_shm: the import goes, and the dma-buf is released.
    client.clear_events();
    session.recolor(&mut client, "gpu-buffer", GREEN);
    let (encoded, _) = session.screenshot("shm-again");
    assert_rgb(&encoded, 50, 40, GREEN, 1, "wl_shm after a dma-buf");
    let end = session.script_state()["dmabuf"].take();
    assert_eq!(
        (&end["live_imports"], &end["held_buffers"]),
        (&json!(0), &json!(0)),
        "{end}"
    );

    // A memfd sent through the dma-buf protocol is not a dma-buf. It is not
    // handed to the device; its pixels are read through a mapping.
    session.redraw(&mut client, "gpu-buffer", |o| {
        o.buffer_kind = proto_test_client::BufferKind::Dmabuf;
        o.pattern = QUADRANTS;
        o.layout = PixelLayout::Argb8888;
    });
    let (encoded, _) = session.screenshot("mapped");
    assert_rgb(&encoded, 40 + 16, 30 + 12, RED, 1, "mapped quadrant");
    assert_rgb(&encoded, 40 + 48, 30 + 36, WHITE, 1, "mapped quadrant");
    let mapped = session.script_state()["dmabuf"].take();
    assert_eq!(mapped["imported"], 1, "{mapped}");
    assert!(mapped["fallbacks"].as_u64().unwrap() >= 1, "{mapped}");

    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// An imported buffer is the client's memory as it is: when the client
/// turned the image inside it (`set_buffer_transform`) or stored it bottom
/// up, nothing rearranges the pixels. The core folds that into the texture
/// coordinates it hands the script. Checked against the path that does
/// rearrange pixels: the same image sent through `wl_shm` must look the same.
fn imported_dmabufs_are_shown_upright(device: Device) {
    const SIZE: (u32, u32) = (64, 48);
    let quadrant = |x: u32, y: u32| {
        [RED, GREEN, BLUE, WHITE][((y * 2 / SIZE.1) * 2 + (x * 2 / SIZE.0)) as usize]
    };
    let mut pixels = Vec::new();
    for y in 0..SIZE.1 {
        for x in 0..SIZE.0 {
            let [r, g, b, _] = quadrant(x, y);
            pixels.extend_from_slice(&[b, g, r, 0]);
        }
    }
    if !common::udmabuf_wanted() {
        return;
    }
    let dmabuf = create_udmabuf(&pixels)
        .expect("creating a udmabuf")
        .expect("/dev/udmabuf can be opened");
    let mut session = Session::start(
        "dmabuf-upright",
        device,
        &["--output-size", "256x192", "--dmabuf", "gpu"],
    );
    let (mut client, _) = session.mapped_client(ClientOptions {
        layout: PixelLayout::Xrgb8888,
        ..options("turned", SIZE.0, SIZE.1, WHITE)
    });
    move_window(&mut session, &mut client, "turned", 40, 30);
    let generation = |s: &Value| {
        window(s, "turned").unwrap()["surfaces"][0]["generation"]
            .as_u64()
            .unwrap()
    };

    for transform in Transform::ALL {
        // The reference: the same quadrants through wl_shm, where the
        // protocol layer turns the pixels upright before the upload.
        session.redraw(&mut client, "turned", |o| {
            o.pattern = QUADRANTS;
            o.buffer_size = Some(SIZE);
            o.buffer_transform = transform;
        });
        let (reference, _) = session.screenshot(&format!("shm-{transform:?}"));
        let imports = session.script_state()["dmabuf"]["imported"]
            .as_u64()
            .unwrap()
            + session.script_state()["dmabuf"]["reused"].as_u64().unwrap();

        let before = generation(&session.state());
        client
            .attach_dmabuf(&ExternalDmabuf {
                fd: dmabuf.as_fd(),
                width: SIZE.0,
                height: SIZE.1,
                fourcc: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
                offset: 0,
                stride: SIZE.0 * 4,
                transform,
                y_invert: false,
            })
            .unwrap();
        session.wait(&mut [&mut client], "the dma-buf arrived", |s| {
            generation(s) > before
        });
        let (imported, _) = session.screenshot(&format!("dmabuf-{transform:?}"));
        let state = session.script_state();
        assert_eq!(
            state["dmabuf"]["imported"].as_u64().unwrap()
                + state["dmabuf"]["reused"].as_u64().unwrap(),
            imports + 1,
            "{transform:?}: the buffer was not imported"
        );
        assert_eq!(state["dmabuf"]["fallbacks"], 0);
        assert_eq!(
            imported.data, reference.data,
            "{transform:?}: the imported buffer is not shown like the uploaded one"
        );
        // And it is not trivially the same picture every time: the top left
        // of the window shows what this transform puts there.
        let _ = imported.pixel(45, 35);
    }

    // Bottom-up rows: the quadrants trade places vertically.
    let before = generation(&session.state());
    client
        .attach_dmabuf(&ExternalDmabuf {
            fd: dmabuf.as_fd(),
            width: SIZE.0,
            height: SIZE.1,
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier: 0,
            offset: 0,
            stride: SIZE.0 * 4,
            transform: Transform::Normal,
            y_invert: true,
        })
        .unwrap();
    session.wait(&mut [&mut client], "the inverted dma-buf arrived", |s| {
        generation(s) > before
    });
    let (inverted, _) = session.screenshot("dmabuf-y-invert");
    for ((x, y), color) in [(56, 42), (88, 42), (56, 66), (88, 66)]
        .into_iter()
        .zip([BLUE, WHITE, RED, GREEN])
    {
        assert_rgb(&inverted, x, y, color, 1, "bottom-up quadrant");
    }
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// Nothing is drawn while nothing changes; a commit, a moved window or a
/// request of the script each cause exactly the frames they need. Clients
/// get their frame callbacks either way.
fn frames_are_drawn_only_when_something_changed(device: Device) {
    let mut session = Session::start("pacing", device, &["--output-size", "128x96"]);
    let (mut client, _) = session.mapped_client(options("pacing", 40, 30, RED));
    let frames = |state: &Value| state["script"]["frames"].as_u64().unwrap();
    let frame_done = |client: &mut TestClient| {
        client
            .wait_for(common::TIMEOUT, |e| {
                matches!(e, ClientEvent::FrameDone { .. })
            })
            .unwrap()
            .expect("a frame callback");
    };
    // "Idle" is told by the compositor, not by a pause: no frame is owed to
    // an output. That is the starting point however long the startup took
    // (the first frame includes compiling the shaders).
    frame_done(&mut client);
    let state = session.wait(&mut [&mut client], "nothing left to draw", |s| {
        frames(s) > 0 && s["redraw_pending"] == false
    });
    let settled = frames(&state);

    // Several refresh periods later: no frame was drawn, none is owed.
    let idle = |session: &mut Session, client: &mut TestClient| {
        for _ in 0..5 {
            client
                .pump(Some(std::time::Duration::from_millis(40)))
                .unwrap();
        }
        session.state()
    };
    let state = idle(&mut session, &mut client);
    assert_eq!(frames(&state), settled, "frames were drawn while idle");
    assert_eq!(state["redraw_pending"], false);

    // One commit: one frame, and the frame callback for it.
    client.clear_events();
    session.recolor(&mut client, "pacing", GREEN);
    frame_done(&mut client);
    let state = session.wait(&mut [&mut client], "the commit is drawn", |s| {
        frames(s) > settled && s["redraw_pending"] == false
    });
    assert_eq!(frames(&state), settled + 1);
    let state = idle(&mut session, &mut client);
    assert_eq!(
        frames(&state),
        settled + 1,
        "more than one frame for one commit"
    );
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// The core does not draw the icon a client attaches to a drag: it hands
/// the surfaces to the script, which draws them at the pointer.
fn the_icon_of_a_drag_is_drawn_by_the_script(device: Device) {
    const ICON: [u8; 4] = [240, 220, 40, 255];
    let mut session = Session::start("drag-icon", device, &["--output-size", "256x192"]);
    let (mut source, _) = session.mapped_client(options("source", 80, 60, RED));
    move_window(&mut session, &mut source, "source", 20, 20);
    let (mut target, _) = session.mapped_client(options("target", 80, 60, GREEN));
    move_window(&mut session, &mut target, "target", 150, 100);

    pointer_to(&mut session, (40.0, 40.0));
    button(&mut session, true);
    source
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::PointerButton { pressed: true, .. })
        })
        .unwrap()
        .expect("the press arrived");
    source
        .start_drag(&["text/plain"], "dragged", Some(ICON))
        .unwrap();
    session.wait(
        &mut [&mut source, &mut target],
        "the drag has an icon",
        |s| {
            s["drag_icon"]
                .as_array()
                .is_some_and(|icon| icon.first().is_some_and(|i| i["buffer_size"].is_array()))
        },
    );

    // Over the background, then over the target: the 16x16 icon is at the
    // pointer, on top of what is there, and nowhere else.
    for (x, y) in [(120.0, 40.0), (190.0, 130.0)] {
        pointer_to(&mut session, (x, y));
        let (encoded, _) = session.screenshot(&format!("drag-{x}"));
        let (px, py) = (x as u32, y as u32);
        assert_rgb(&encoded, px + 8, py + 8, ICON, 1, "the icon at the pointer");
        assert_rgb(&encoded, px + 15, py + 15, ICON, 1, "the icon's far corner");
        assert_ne!(
            encoded.pixel(px + 17, py + 8)[..3],
            [ICON[0] as u16, ICON[1] as u16, ICON[2] as u16],
            "right of the icon"
        );
        assert_rgb(
            &encoded,
            40,
            40,
            RED,
            1,
            "the source window, no icon left behind",
        );
    }
    // Dropped: the icon is gone.
    button(&mut session, false);
    session.wait(&mut [&mut source, &mut target], "the drag ended", |s| {
        s["drag_icon"].as_array().is_some_and(Vec::is_empty)
    });
    let (encoded, _) = session.screenshot("dropped");
    assert_rgb(&encoded, 198, 138, GREEN, 1, "the target, without the icon");
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// The candidate window of an input method is a surface of the window
/// whose text field has the focus, placed by the core below the cursor
/// the application reported, and drawn by the script with the window. It
/// hangs out of the window, and is gone when the field is no longer
/// enabled.
fn the_popup_of_an_input_method_is_drawn_by_the_cursor(device: Device) {
    const CANDIDATES: [u8; 4] = [240, 220, 40, 255];
    const BACKGROUND: [u8; 4] = [0x20, 0x20, 0x20, 255];
    let mut session = Session::start("input-popup", device, &["--output-size", "320x240"]);
    let mut ime = session.client(ClientOptions {
        window: false,
        input_method: Some(InputMethodMode::Manual),
        ..Default::default()
    });
    let (mut app, _) = session.mapped_client(ClientOptions {
        text_input: true,
        ..options("field", 160, 80, BLUE)
    });
    move_window(&mut session, &mut app, "field", 20, 20);
    session.wait(&mut [&mut app, &mut ime], "an input method", |s| {
        s["input_method"]["bound"] == true
    });
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while !app.text_input_entered() {
        assert!(
            std::time::Instant::now() < deadline,
            "the field never got the text input"
        );
        app.pump(Some(std::time::Duration::from_millis(10)))
            .unwrap();
    }
    // The cursor is near the bottom of the window.
    app.text_input_enable(Some((30, 60, 2, 16))).unwrap();
    session.wait(&mut [&mut app, &mut ime], "active", |s| {
        s["input_method"]["active"] == true
    });
    ime.ime_show_popup(60, 24, CANDIDATES).unwrap();
    let state = session.wait(&mut [&mut app, &mut ime], "the popup is drawn", |s| {
        window(s, "field").is_some_and(|w| {
            w["surfaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["role"] == "input-popup" && n["has_contents"] == true)
        })
    });
    assert_eq!(state["input_method"]["popup"]["position"], json!([30, 76]));

    // The window spans 20..180 x 20..100; the popup 50..110 x 96..120.
    let (encoded, _) = session.screenshot("candidates");
    assert_rgb(
        &encoded,
        80,
        108,
        CANDIDATES,
        1,
        "the popup below the window",
    );
    assert_rgb(&encoded, 80, 98, CANDIDATES, 1, "the popup over the window");
    assert_rgb(&encoded, 40, 98, BLUE, 1, "the window beside the popup");
    assert_rgb(&encoded, 80, 125, BACKGROUND, 1, "below the popup");

    app.text_input_disable().unwrap();
    session.wait(&mut [&mut app, &mut ime], "the popup is hidden", |s| {
        window(s, "field").is_some_and(|w| {
            !w["surfaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["role"] == "input-popup")
        })
    });
    let (encoded, _) = session.screenshot("hidden");
    assert_rgb(
        &encoded,
        80,
        108,
        BACKGROUND,
        1,
        "no popup once the field is disabled",
    );
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

// ---------------------------------------------------------------------------
// Transformed windows and input
// ---------------------------------------------------------------------------

/// A window with a subsurface that sticks out to the right, quadrants in
/// four colors, drawn with a transform that rotates, shears and scales.
fn skewed_window(session: &mut Session, title: &str, transform: Affine) -> (TestClient, u64) {
    let (mut client, id) = session.mapped_client(ClientOptions {
        layout: PixelLayout::Xrgb8888,
        pattern: QUADRANTS,
        ..options(title, 100, 60, WHITE)
    });
    client
        .add_subsurface(60, 10, 80, 30, [40, 40, 200, 255])
        .unwrap();
    session.wait(&mut [&mut client], "subsurface mapped", |s| {
        let surfaces = &window(s, title).unwrap()["surfaces"];
        surfaces.as_array().unwrap().len() == 2 && surfaces[1]["buffer_size"].is_array()
    });
    session.script("set_transform", json!([id, transform.json()]));
    (client, id)
}

/// The script draws a window rotated, sheared and scaled. The pixels are
/// where the transform puts them, and a pointer position on the screen
/// reaches the client as the position inside its surface that is drawn
/// there.
fn a_transformed_window_is_drawn_and_hit_where_the_script_put_it(device: Device) {
    let mut session = Session::start("transform", device, &["--output-size", "400x300"]);
    let transform = skewed(50.0, 30.0, 200.0, 150.0);
    let (mut client, _) = skewed_window(&mut session, "skewed", transform);

    // Drawn: the centers of the quadrants, and the part of the subsurface
    // that sticks out of the window.
    let (encoded, _) = session.screenshot("skewed");
    for (local, color) in [
        ((25.0, 15.0), RED),
        ((75.0, 45.0), WHITE),
        ((25.0, 45.0), BLUE),
        ((40.0, 8.0), RED),
        ((120.0, 25.0), [40, 40, 200, 255]),
    ] {
        let (x, y) = transform.apply(local);
        assert_rgb(
            &encoded,
            x as u32,
            y as u32,
            color,
            1,
            &format!("{local:?}"),
        );
    }
    // Where the window would be without the transform there is nothing.
    assert_rgb(
        &encoded,
        40,
        40,
        [0x20, 0x20, 0x20, 255],
        0,
        "the untransformed place",
    );
    // Just outside the top left and bottom right corners.
    for local in [(-3.0, -3.0), (103.0, 63.0), (50.0, -3.0)] {
        let (x, y) = transform.apply(local);
        assert_rgb(
            &encoded,
            x as u32,
            y as u32,
            [0x20, 0x20, 0x20, 255],
            0,
            "outside",
        );
    }

    // Hit: points of the window, given on the screen.
    for local in [
        (25.0, 15.0),
        (3.25, 2.5),
        (99.0, 59.0),
        (1.0, 58.0),
        (40.0, 8.0),
    ] {
        let (kind, seen) = point_at(&mut session, &mut client, transform.apply(local))
            .unwrap_or_else(|| panic!("nothing at {local:?}"));
        assert_eq!(kind, SurfaceKind::Window, "{local:?}");
        assert_near(seen, local, "window");
    }
    // Points of the subsurface, which is at (60, 10) of the window: where
    // it covers the window, and where it sticks out.
    for local in [(70.0, 20.0), (130.0, 35.0), (139.0, 11.0)] {
        let (kind, seen) = point_at(&mut session, &mut client, transform.apply(local))
            .unwrap_or_else(|| panic!("nothing at {local:?}"));
        assert_eq!(kind, SurfaceKind::Subsurface, "{local:?}");
        assert_near(seen, (local.0 - 60.0, local.1 - 10.0), "subsurface");
    }
    // Outside, close to the edges: nothing is hit.
    for local in [
        (-1.0, 30.0),
        (50.0, 61.0),
        (101.0, 50.0),
        (141.0, 25.0),
        (50.0, -1.0),
    ] {
        client.clear_events();
        pointer_to(&mut session, transform.apply(local));
        client
            .pump(Some(std::time::Duration::from_millis(30)))
            .unwrap();
        assert_eq!(client.pointer_focus(), None, "{local:?} is outside");
    }
    assert_eq!(session.state()["focus"]["pointer_surface"], Value::Null);

    // The transform changes: the same point of the screen is another point
    // of the window, without the pointer moving.
    let moved = Affine::translate(30.0, 0.0).after(transform);
    let global = transform.apply((50.0, 30.0));
    point_at(&mut session, &mut client, global).unwrap();
    client.clear_events();
    let id = window(&session.state(), "skewed").unwrap()["id"]
        .as_u64()
        .unwrap();
    session.script("set_transform", json!([id, moved.json()]));
    client
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::PointerMotion { .. })
        })
        .unwrap()
        .expect("the pointer is told its new place in the window");
    assert_near(
        pointer_position(&client).unwrap(),
        moved.invert().apply(global),
        "after the transform changed",
    );

    // Without a transform the window is back at its position.
    session.script("set_transform", json!([id, Value::Null]));
    let (encoded, _) = session.screenshot("plain");
    let position = window(&session.state(), "skewed").unwrap()["position"].clone();
    let (x, y) = (
        position[0].as_u64().unwrap() as u32,
        position[1].as_u64().unwrap() as u32,
    );
    assert_rgb(&encoded, x + 25, y + 15, RED, 1, "untransformed");
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// While a button is held the surface it was pressed on keeps the pointer,
/// in its own (transformed) coordinates, even far outside the window. After
/// the release the pointer belongs to whatever is under it.
fn a_press_holds_the_pointer_on_a_transformed_window(device: Device) {
    let mut session = Session::start("grab", device, &["--output-size", "400x300"]);
    let (mut other, other_id) = session.mapped_client(options("other", 60, 60, GREEN));
    move_window(&mut session, &mut other, "other", 330, 10);
    let transform = skewed(50.0, 30.0, 180.0, 160.0);
    let (mut client, id) = skewed_window(&mut session, "held", transform);

    point_at(&mut session, &mut client, transform.apply((20.0, 20.0))).unwrap();
    button(&mut session, true);
    assert_eq!(session.state()["focus"]["window"], id);

    // Over the background, then over the other window: the events keep
    // going to the pressed surface.
    for global in [(5.0, 290.0), (350.0, 30.0), (399.0, 1.0)] {
        other.clear_events();
        let (kind, seen) = point_at(&mut session, &mut client, global).expect("still held");
        assert_eq!(kind, SurfaceKind::Window);
        assert_near(seen, transform.invert().apply(global), "held");
        other
            .pump(Some(std::time::Duration::from_millis(20)))
            .unwrap();
        assert_eq!(
            other.pointer_focus(),
            None,
            "the other window heard of the pointer"
        );
    }
    // The second button does not end the hold when the first is released.
    session.send(Request::PointerButton {
        button: BTN_LEFT + 1,
        pressed: true,
    });
    button(&mut session, false);
    let (kind, _) = point_at(&mut session, &mut client, (351.0, 31.0)).expect("still held");
    assert_eq!(kind, SurfaceKind::Window);

    // Released over the other window: the pointer leaves here and enters
    // there, at the right place.
    client.clear_events();
    session.send(Request::PointerButton {
        button: BTN_LEFT + 1,
        pressed: false,
    });
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerLeave)
        .unwrap()
        .expect("the pointer leaves the held window");
    other
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::PointerEnter { .. })
        })
        .unwrap()
        .expect("the pointer enters the window under it");
    assert_near(
        pointer_position(&other).unwrap(),
        (21.0, 21.0),
        "after the hold",
    );
    // The press focused the held window; the release elsewhere did not
    // change that.
    assert_eq!(session.state()["focus"]["window"], id);
    let _ = other_id;
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// A popup belongs to its window: it is drawn with the window's transform,
/// the pointer reaches it in its own coordinates, and a grabbing popup is
/// dismissed by a press outside the client, not by one on the turned popup.
fn popups_follow_the_transform_of_their_window(device: Device) {
    let mut session = Session::start("popup", device, &["--output-size", "400x300"]);
    let (mut other, _) = session.mapped_client(options("other", 60, 60, GREEN));
    move_window(&mut session, &mut other, "other", 330, 10);
    let transform = skewed(50.0, 30.0, 180.0, 160.0);
    let (mut client, _) = skewed_window(&mut session, "menu", transform);
    const POPUP: [u8; 4] = [230, 200, 40, 255];
    let popup = |grab| PopupOptions {
        x: 20,
        y: 40,
        width: 50,
        height: 40,
        color: POPUP,
        grab,
        slide: false,
    };
    let popup_mapped = |s: &Value| {
        window(s, "menu").unwrap()["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["role"] == "popup" && n["buffer_size"].is_array())
    };

    // Without a grab: placed, drawn turned, and hit.
    client.open_popup(popup(false)).unwrap();
    session.wait(&mut [&mut client], "popup mapped", popup_mapped);
    let (encoded, _) = session.screenshot("popup");
    // The popup's center, and its lower part, which hangs below the window
    // (the window is 60 high, the popup spans 40 to 80).
    for local in [(45.0, 60.0), (30.0, 75.0), (65.0, 78.0)] {
        let (x, y) = transform.apply(local);
        assert_rgb(
            &encoded,
            x as u32,
            y as u32,
            POPUP,
            1,
            &format!("popup at {local:?}"),
        );
    }
    for local in [(45.0, 60.0), (21.0, 79.0), (69.0, 41.0)] {
        let (kind, seen) = point_at(&mut session, &mut client, transform.apply(local))
            .unwrap_or_else(|| panic!("nothing at {local:?}"));
        assert_eq!(kind, SurfaceKind::Popup, "{local:?}");
        assert_near(seen, (local.0 - 20.0, local.1 - 40.0), "popup");
    }
    client.close_popup();
    session.wait(&mut [&mut client], "popup gone", |s| !popup_mapped(s));

    // With a grab, as a menu does it: opened by a click on the window.
    point_at(&mut session, &mut client, transform.apply((10.0, 10.0))).unwrap();
    button(&mut session, true);
    button(&mut session, false);
    client
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::PointerButton { pressed: false, .. })
        })
        .unwrap()
        .expect("the click arrived");
    client.open_popup(popup(true)).unwrap();
    session.wait(&mut [&mut client], "grabbing popup mapped", popup_mapped);
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while client.keyboard_focus() != Some(SurfaceKind::Popup) {
        assert!(
            std::time::Instant::now() < deadline,
            "the popup never got the keyboard"
        );
        client
            .pump(Some(std::time::Duration::from_millis(10)))
            .unwrap();
    }

    // A press on the turned popup goes to the popup.
    let (kind, seen) = point_at(&mut session, &mut client, transform.apply((45.0, 60.0))).unwrap();
    assert_eq!(kind, SurfaceKind::Popup);
    assert_near(seen, (25.0, 20.0), "grabbing popup");
    client.clear_events();
    button(&mut session, true);
    button(&mut session, false);
    client
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::PointerButton { pressed: false, .. })
        })
        .unwrap()
        .expect("the click on the popup arrived");
    assert!(client.has_popup(), "a click on the popup dismissed it");

    // The pointer over another client during the grab: that client hears
    // nothing.
    other.clear_events();
    pointer_to(&mut session, (350.0, 30.0));
    other
        .pump(Some(std::time::Duration::from_millis(30)))
        .unwrap();
    assert_eq!(other.pointer_focus(), None);

    // A press there dismisses the popup.
    client.clear_events();
    button(&mut session, true);
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PopupDone)
        .unwrap()
        .expect("the popup is dismissed by a press outside");
    button(&mut session, false);
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// What a point of the screen hits is the script's answer. Here a script
/// makes one window transparent to the pointer, which the core alone would
/// never do: the window is on top and takes input everywhere.
fn the_script_decides_what_a_point_hits(device: Device) {
    let project = script_project(
        "ghost",
        device,
        &[
            ("session", DRAW_ONLY_SESSION),
            (
                "main",
                r#"
            hz.include("session")
            local usual = hit_test
            local ghost
            function set_ghost(title)
                ghost = title
            end
            function hit_test(x, y, accepts)
                local skip = {}
                for _, w in hz.windows() do
                    if w.title == ghost then
                        for _, s in hz.surfaces(w.id) do
                            skip[s.id] = true
                        end
                    end
                end
                return usual(x, y, function(surface, sx, sy)
                    return not skip[surface] and accepts(surface, sx, sy)
                end)
            end
            "#,
            ),
        ],
    );
    let mut session = Session::start(
        "ghost",
        device,
        &[
            "--output-size",
            "256x192",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let (mut below, _) = session.mapped_client(options("below", 100, 80, GREEN));
    move_window(&mut session, &mut below, "below", 20, 20);
    let (mut above, _) = session.mapped_client(options("above", 100, 80, RED));
    move_window(&mut session, &mut above, "above", 60, 40);

    // The usual answer: the window on top.
    let (kind, seen) = point_at(&mut session, &mut above, (80.0, 60.0)).unwrap();
    assert_eq!(kind, SurfaceKind::Window);
    assert_near(seen, (20.0, 20.0), "above");
    assert_eq!(below.pointer_focus(), None);

    // The script's answer: through it, to the window below.
    session.script("set_ghost", json!(["above"]));
    let (kind, seen) = point_at(&mut session, &mut below, (81.0, 61.0)).unwrap();
    assert_eq!(kind, SurfaceKind::Window);
    assert_near(seen, (61.0, 41.0), "below, through the ghost");
    above
        .pump(Some(std::time::Duration::from_millis(20)))
        .unwrap();
    assert_eq!(above.pointer_focus(), None);
    // Where only the ghost is, nothing is hit.
    below.clear_events();
    pointer_to(&mut session, (150.0, 110.0));
    below
        .pump(Some(std::time::Duration::from_millis(30)))
        .unwrap();
    assert_eq!(below.pointer_focus(), None);
    // It is still drawn on top.
    let (encoded, _) = session.screenshot("ghost");
    assert_rgb(&encoded, 80, 60, RED, 1, "the ghost is drawn");
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

// ---------------------------------------------------------------------------
// Window management
// ---------------------------------------------------------------------------

const KEY_Q: u32 = 16;
const KEY_H: u32 = 35;
const KEY_L: u32 = 38;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;

fn key(session: &mut Session, keycode: u32, pressed: bool) {
    session.send(Request::Key { keycode, pressed });
}

/// Presses and releases `keycode` with the logo key (and shift) held.
fn logo_key(session: &mut Session, keycode: u32, shift: bool) {
    key(session, KEY_LEFTMETA, true);
    if shift {
        key(session, KEY_LEFTSHIFT, true);
    }
    key(session, keycode, true);
    key(session, keycode, false);
    if shift {
        key(session, KEY_LEFTSHIFT, false);
    }
    key(session, KEY_LEFTMETA, false);
}

/// Waits until the client's window has the keyboard.
#[track_caller]
fn wait_keyboard(client: &mut TestClient) {
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while client.keyboard_focus() != Some(SurfaceKind::Window) {
        assert!(
            std::time::Instant::now() < deadline,
            "the window never got the keyboard: {:?}",
            client.events()
        );
        client
            .pump(Some(std::time::Duration::from_millis(10)))
            .unwrap();
    }
}

/// Position and size of a window in the state, once it has that size.
fn geometry(state: &Value, title: &str) -> Option<([i64; 2], [i64; 2])> {
    let w = window(state, title)?;
    let pair = |v: &Value| Some([v[0].as_i64()?, v[1].as_i64()?]);
    Some((pair(&w["position"])?, pair(&w["size"])?))
}

/// Waits until the windows have the given positions and sizes (the clients
/// have to redraw at the size they were configured with).
fn wait_tiles(
    session: &mut Session,
    clients: &mut [&mut TestClient],
    expected: &[(&str, [i64; 2], [i64; 2])],
) -> Value {
    session.wait(clients, &format!("tiles {expected:?}"), |s| {
        s["windows"].as_array().unwrap().len() == expected.len()
            && expected
                .iter()
                .all(|(title, position, size)| geometry(s, title) == Some((*position, *size)))
    })
}

/// The built-in tiling script: windows side by side, sharing the output;
/// the logo key with H and L moves the focus, with shift swaps neighbours,
/// with Q closes. Positions, sizes (through configure events), focus and
/// stacking are the script's.
fn windows_are_tiled_by_the_script(device: Device) {
    let mut session = Session::start_builtin("tile", device, &["--output-size", "640x400"]);
    assert_eq!(session.state()["window_manager"], "script");
    // 640 wide with a gap of 8: one window is 624 wide, two are 308, three
    // are 202. All are 384 high.
    let mut a = session.client(options("a", 100, 60, RED));
    wait_tiles(&mut session, &mut [&mut a], &[("a", [8, 8], [624, 384])]);
    let mut b = session.client(options("b", 100, 60, GREEN));
    wait_tiles(
        &mut session,
        &mut [&mut a, &mut b],
        &[("a", [8, 8], [308, 384]), ("b", [324, 8], [308, 384])],
    );
    let mut c = session.client(options("c", 100, 60, BLUE));
    let three = [
        ("a", [8, 8], [202, 384]),
        ("b", [218, 8], [202, 384]),
        ("c", [428, 8], [202, 384]),
    ];
    let state = wait_tiles(&mut session, &mut [&mut a, &mut b, &mut c], &three);
    let id = |state: &Value, title: &str| window(state, title).unwrap()["id"].as_u64().unwrap();
    let (ida, idb, idc) = (id(&state, "a"), id(&state, "b"), id(&state, "c"));
    assert_eq!(
        session.script("tile_order", json!([])),
        json!([ida, idb, idc])
    );

    // The clients were told their size, and the newest one has the focus.
    assert!(a.events().contains(&ClientEvent::Configure {
        width: 202,
        height: 384,
        activated: false
    }));
    assert!(c.events().iter().any(|e| matches!(
        e,
        ClientEvent::Configure {
            width: 202,
            height: 384,
            activated: true
        }
    )));
    assert_eq!(state["focus"]["window"], idc);
    wait_keyboard(&mut c);

    // On screen: three columns, gaps between them.
    let (encoded, _) = session.screenshot("three");
    for (x, color) in [(100, RED), (320, GREEN), (530, BLUE)] {
        assert_rgb(&encoded, x, 200, color, 1, "column");
    }
    for x in [4, 213, 424, 634] {
        assert_rgb(&encoded, x, 200, [0x20, 0x20, 0x20, 255], 0, "gap");
    }

    // Logo+H: the focus goes left. The key is the window manager's; no
    // client sees an H.
    for client in [&mut a, &mut b, &mut c] {
        client.clear_events();
    }
    logo_key(&mut session, KEY_H, false);
    session.wait(&mut [&mut a, &mut b, &mut c], "focus on b", |s| {
        s["focus"]["window"] == idb
    });
    wait_keyboard(&mut b);
    logo_key(&mut session, KEY_H, false);
    session.wait(&mut [&mut a, &mut b, &mut c], "focus on a", |s| {
        s["focus"]["window"] == ida
    });
    // At the left end nothing happens.
    logo_key(&mut session, KEY_H, false);
    logo_key(&mut session, KEY_L, false);
    session.wait(&mut [&mut a, &mut b, &mut c], "focus on b again", |s| {
        s["focus"]["window"] == idb
    });
    wait_keyboard(&mut b);
    // The windows were told who is active: the last word to each.
    let activated = |client: &TestClient| {
        client.events().iter().rev().find_map(|e| match e {
            ClientEvent::Configure { activated, .. } => Some(*activated),
            _ => None,
        })
    };
    assert_eq!(activated(&b), Some(true));
    assert_eq!(activated(&a), Some(false));
    assert_eq!(activated(&c), Some(false));
    for client in [&a, &b, &c] {
        assert!(
            !client.events().iter().any(|e| matches!(
                e,
                ClientEvent::Key { keycode, .. } if *keycode == KEY_H || *keycode == KEY_L
            )),
            "a client saw a window manager key: {:?}",
            client.events()
        );
    }
    // An ordinary key still goes to the focused client.
    key(&mut session, 30, true);
    key(&mut session, 30, false);
    b.wait_for(common::TIMEOUT, |e| {
        matches!(
            e,
            ClientEvent::Key {
                keycode: 30,
                pressed: false,
                ..
            }
        )
    })
    .unwrap()
    .expect("the focused window gets ordinary keys");

    // Logo+Shift+L: b and c trade places. The focus stays with b.
    logo_key(&mut session, KEY_L, true);
    let state = wait_tiles(
        &mut session,
        &mut [&mut a, &mut b, &mut c],
        &[
            ("a", [8, 8], [202, 384]),
            ("c", [218, 8], [202, 384]),
            ("b", [428, 8], [202, 384]),
        ],
    );
    assert_eq!(state["focus"]["window"], idb);
    assert_eq!(
        session.script("tile_order", json!([])),
        json!([ida, idc, idb])
    );
    let (encoded, _) = session.screenshot("swapped");
    for (x, color) in [(100, RED), (320, BLUE), (530, GREEN)] {
        assert_rgb(&encoded, x, 200, color, 1, "column after the swap");
    }

    // A click focuses the window under the pointer (the script defines no
    // pointer handler, so the core's default applies).
    pointer_to(&mut session, (100.0, 100.0));
    button(&mut session, true);
    button(&mut session, false);
    session.wait(&mut [&mut a, &mut b, &mut c], "focus on a by click", |s| {
        s["focus"]["window"] == ida
    });

    // Logo+Q: the focused window is asked to close, and the others share
    // the room.
    logo_key(&mut session, KEY_Q, false);
    a.wait_for(common::TIMEOUT, |e| *e == ClientEvent::CloseRequested)
        .unwrap()
        .expect("the focused window is asked to close");
    let state = wait_tiles(
        &mut session,
        &mut [&mut a, &mut b, &mut c],
        &[("c", [8, 8], [308, 384]), ("b", [324, 8], [308, 384])],
    );
    // The neighbour that moved into its place has the focus.
    assert_eq!(state["focus"]["window"], idc);

    // A client that simply goes away.
    drop(c);
    let state = wait_tiles(
        &mut session,
        &mut [&mut a, &mut b],
        &[("b", [8, 8], [624, 384])],
    );
    assert_eq!(state["focus"]["window"], idb);
    assert_eq!(session.script_state()["failures"], 0);
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// A window manager written from nothing: it hears of windows opening,
/// changing size and closing, of the focus moving, and of the outputs, and
/// what it decides (place, size, focus, stacking, close) happens.
fn a_window_manager_script_hears_what_happens(device: Device) {
    let project = script_project(
        "own-wm",
        device,
        &[(
            "main",
            r#"
            -- Drawing and hit testing as usual; window management is ours.
            local render = hz.include("render")
            function init() render.init() end
            function frame(output, target) render.frame(output, target) end
            hit_test = render.hit_test
            locate = render.locate

            local log = {}
            local function note(what, id)
                table.insert(log, { what, id })
            end
            function heard()
                local result = log
                log = {}
                return result
            end

            -- Every window goes to a fixed place, 150x100, stacked in the
            -- order of their titles; the focus stays where the script
            -- puts it.
            function window_opened(id)
                note("opened", id)
                local count = #hz.windows()
                hz.place(id, 10 + 40 * count, 20)
                hz.configure(id, 150, 100)
                if count == 1 then
                    hz.focus(id)
                end
            end
            function window_changed(id) note("changed", id) end
            function window_closed(id) note("closed", id) end
            function focus_changed(id) note("focus", id) end
            function outputs_changed() note("outputs") end

            -- F1 (59) puts the bottom window on top; F2 (60) closes the
            -- top one; everything else is not ours.
            function key(keycode, pressed, mods)
                if keycode == 59 or keycode == 60 then
                    if pressed then
                        local windows = hz.windows()
                        if keycode == 59 then
                            hz.raise(windows[1].id)
                        else
                            hz.close(windows[#windows].id)
                        end
                    end
                    return true
                end
                return false
            end

            -- Clicks never change the focus here.
            function pointer_button(button, pressed, window, surface)
                note(pressed and "press" or "release", window)
                return true
            end

            function misuse(what)
                if what == "place" then hz.place(424242, 0, 0)
                elseif what == "size" then hz.configure(hz.windows()[1].id, -5, 10)
                end
            end
            "#,
        )],
    );
    let mut session = Session::start(
        "own-wm",
        device,
        &[
            "--output-size",
            "400x300",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let what = |heard: &Value| -> Vec<(String, Option<u64>)> {
        heard
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|e| (e[0].as_str().unwrap().to_string(), e[1].as_u64()))
                    .collect()
            })
            .unwrap_or_default()
    };
    // The output was announced before any window.
    let heard = what(&session.script("heard", json!([])));
    assert!(heard.contains(&("outputs".to_string(), None)), "{heard:?}");

    let mut first = session.client(options("first", 80, 50, RED));
    let state = wait_tiles(
        &mut session,
        &mut [&mut first],
        &[("first", [50, 20], [150, 100])],
    );
    let id1 = window(&state, "first").unwrap()["id"].as_u64().unwrap();
    let mut second = session.client(options("second", 80, 50, GREEN));
    let state = wait_tiles(
        &mut session,
        &mut [&mut first, &mut second],
        &[
            ("first", [50, 20], [150, 100]),
            ("second", [90, 20], [150, 100]),
        ],
    );
    let id2 = window(&state, "second").unwrap()["id"].as_u64().unwrap();
    // The script kept the focus on the first window; the core's default
    // (focus the newest) did not win.
    assert_eq!(state["focus"]["window"], id1);
    let heard = what(&session.script("heard", json!([])));
    let position = |entry: (&str, Option<u64>)| {
        heard
            .iter()
            .position(|(w, id)| (w.as_str(), *id) == entry)
            .unwrap_or_else(|| panic!("{entry:?} is missing in {heard:?}"))
    };
    assert!(position(("opened", Some(id1))) < position(("opened", Some(id2))));
    // Each window reported the size it drew at.
    assert!(position(("changed", Some(id1))) > position(("opened", Some(id1))));
    position(("changed", Some(id2)));

    // Stacking: the second is on top where they overlap, until F1 raises
    // the first.
    let (encoded, _) = session.screenshot("stacked");
    assert_rgb(&encoded, 150, 60, GREEN, 1, "second on top");
    key(&mut session, 59, true);
    key(&mut session, 59, false);
    let (encoded, _) = session.screenshot("raised");
    assert_rgb(&encoded, 150, 60, RED, 1, "first raised");
    let order: Vec<u64> = session.state()["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .collect();
    assert_eq!(order, [id2, id1]);

    // A click on the second window: the script takes it, the focus stays.
    pointer_to(&mut session, (230.0, 60.0));
    button(&mut session, true);
    button(&mut session, false);
    assert_eq!(session.state()["focus"]["window"], id1);
    let heard = what(&session.script("heard", json!([])));
    assert!(
        heard.contains(&("press".to_string(), Some(id2))),
        "{heard:?}"
    );
    // The client still got its click.
    second
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::PointerButton { pressed: false, .. })
        })
        .unwrap()
        .expect("the click reaches the client");

    // A focus change from outside is told to the script.
    session.send(Request::FocusWindow { window: Some(id2) });
    let heard = what(&session.script("heard", json!([])));
    assert!(
        heard.contains(&("focus".to_string(), Some(id2))),
        "{heard:?}"
    );

    // Mistakes are errors for the script, and change nothing.
    for (what, expected) in [("place", "unknown window"), ("size", "invalid size")] {
        let error = session
            .try_send(Request::ScriptCall {
                function: "misuse".into(),
                args: vec![json!(what)],
            })
            .unwrap_err();
        assert!(error.contains(expected), "{error}");
    }

    // F2 closes the top window (the second: focusing it from outside
    // raised it); the script hears of it, and of the focus moving on.
    key(&mut session, 60, true);
    key(&mut session, 60, false);
    session.wait(&mut [&mut first, &mut second], "one window left", |s| {
        s["windows"].as_array().unwrap().len() == 1
    });
    let heard = what(&session.script("heard", json!([])));
    assert!(
        heard.contains(&("closed".to_string(), Some(id2))),
        "{heard:?}"
    );
    assert!(
        heard.contains(&("focus".to_string(), Some(id1))),
        "{heard:?}"
    );
    assert_eq!(session.state()["focus"]["window"], id1);
    // The calls refused above were recorded.
    session.assert_script_running(2);
    session.assert_drawing_clean();
    session.quit();
}

// ---------------------------------------------------------------------------
// Failures and reloading
// ---------------------------------------------------------------------------

/// The built-in session with switches that make its callbacks fail in
/// chosen ways.
const FAULTY: &str = r#"
hz.include("session")
local usual = { frame = frame, hit_test = hit_test, key = key, window_opened = window_opened }
local fault = {}

function set_fault(name, kind)
    fault[name] = kind
end

local function trip(kind)
    if kind == "error" then
        error("deliberate failure")
    elseif kind == "loop" then
        while true do end
    elseif kind == "gpu" then
        gfx.draw(3) -- not inside a render pass
    elseif kind == "stale" then
        gfx.destroy(123456) -- not a handle
    elseif kind == "memory" then
        local hoard = {}
        while true do
            table.insert(hoard, string.rep("x", 1048576))
        end
    end
end

function frame(output, target)
    local kind = fault.frame
    if kind == "once" then
        fault.frame = nil
        error("failed once")
    elseif kind == "open-pass" then
        gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 1, 0, 1, 1 } } } })
        return
    elseif kind == "half" then
        -- Scribble over the image, then fail.
        gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 1, 0, 1, 1 } } } })
        gfx.end_pass()
        error("failed after drawing")
    end
    trip(kind)
    usual.frame(output, target)
end

function hit_test(x, y, accepts)
    trip(fault.hit_test)
    return usual.hit_test(x, y, accepts)
end

if usual.key then
    function key(keycode, pressed, mods)
        trip(fault.key)
        return usual.key(keycode, pressed, mods)
    end
end

if usual.window_opened then
    function window_opened(id)
        trip(fault.window_opened)
        return usual.window_opened(id)
    end
end
"#;

/// The bar the built-in fallback draws along the top edge.
#[track_caller]
fn assert_fallback_marker(image: &EncodedImage, present: bool, what: &str) {
    let pixel = image.pixel(image.width / 2, 1);
    let is_marker = pixel[0] == 255 && pixel[1] == 0 && (127..=128).contains(&pixel[2]);
    assert_eq!(is_marker, present, "{what}: the top edge is {pixel:?}");
}

/// A client still works: it can draw (and the new contents arrive), and
/// keys reach it.
#[track_caller]
fn assert_client_alive(
    session: &mut Session,
    client: &mut TestClient,
    title: &str,
    color: [u8; 4],
) {
    session.recolor(client, title, color);
    client.clear_events();
    key(session, 30, true);
    key(session, 30, false);
    client
        .wait_for(common::TIMEOUT, |e| {
            matches!(
                e,
                ClientEvent::Key {
                    keycode: 30,
                    pressed: false,
                    ..
                }
            )
        })
        .unwrap()
        .expect("keys still reach the client");
}

/// The drawing part of the script fails in every way it can: an exception,
/// a wrong GPU call, a render pass left open, a failure after scribbling
/// over the output, running out of memory, and not returning at all. Each
/// time the compositor stays up, the client stays connected and usable, and
/// the screen shows the windows (drawn by the built-in fallback, marked as
/// such). Putting the script back in charge restores the normal picture.
fn a_script_that_fails_to_draw_leaves_a_usable_session(device: Device) {
    let project = script_project(
        "faulty-frame",
        device,
        &[("session", DRAW_ONLY_SESSION), ("main", FAULTY)],
    );
    let mut session = Session::start(
        "faulty-frame",
        device,
        &[
            "--output-size",
            "256x192",
            "--script",
            project.to_str().unwrap(),
            // Long next to what a frame takes, also on a slow machine:
            // only the fault that never returns is to run into it.
            "--script-budget-ms",
            "2000",
            "--script-memory-mb",
            "24",
        ],
    );
    let (mut client, _) = session.mapped_client(options("victim", 100, 60, RED));
    move_window(&mut session, &mut client, "victim", 40, 30);
    let good = session.screenshot_png("good");
    assert_rgb(&good, 90, 60, RED, 1, "before");
    assert_fallback_marker(&good, false, "before");
    assert_eq!(session.script_state()["mode"], "running");

    let mut failures = 0;
    for (kind, reported) in [
        ("error", "runtime"),
        ("gpu", "gfx"),
        ("stale", "gfx"),
        ("open-pass", "runtime"),
        ("half", "runtime"),
        ("memory", "memory"),
        ("loop", "timeout"),
    ] {
        let started = std::time::Instant::now();
        session.script("set_fault", json!(["frame", kind]));
        let broken = session.screenshot_png(&format!("broken-{kind}"));
        let took = started.elapsed();
        // The windows are there, drawn by the fallback.
        assert_fallback_marker(&broken, true, kind);
        assert_rgb(&broken, 90, 60, RED, 0, kind);
        assert_rgb(&broken, 5, 100, [0x20, 0x20, 0x20, 255], 0, kind);
        // Not returning is cut off at the time budget, and not much later
        // (the allowance is for a busy machine, not for the cut).
        if kind == "loop" {
            let budget = std::time::Duration::from_secs(2);
            assert!(took >= budget, "{kind} took {took:?}");
            assert!(
                took < budget + std::time::Duration::from_secs(60),
                "{kind} took {took:?}"
            );
        }
        let script = session.script_state();
        assert_eq!(script["mode"], "fallback", "{kind}");
        failures += 1;
        assert_eq!(script["failures"], failures, "{kind}: {script}");
        let last = script["last_failures"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["what"], "frame", "{kind}");
        assert_eq!(last["kind"], reported, "{kind}: {last}");

        // The session is usable: the client draws and gets its keys, and
        // what it draws is shown.
        assert_client_alive(&mut session, &mut client, "victim", GREEN);
        let shown = session.screenshot_png(&format!("alive-{kind}"));
        assert_rgb(&shown, 90, 60, GREEN, 0, kind);
        assert_fallback_marker(&shown, true, kind);
        // The pointer finds the window (the core's own hit test).
        let (surface, seen) = point_at(&mut session, &mut client, (50.0, 40.0)).unwrap();
        assert_eq!(surface, SurfaceKind::Window);
        assert_near(seen, (10.0, 10.0), kind);

        // Back: the fault is gone and the script is put in charge again.
        session.script("set_fault", json!(["frame", Value::Null]));
        session.send(Request::ScriptMode {
            mode: "running".into(),
        });
        session.recolor(&mut client, "victim", RED);
        let again = session.screenshot_png(&format!("recovered-{kind}"));
        assert_eq!(session.script_state()["mode"], "running", "{kind}");
        assert_eq!(
            again.data, good.data,
            "{kind}: the picture is not what it was"
        );
    }
    // Nothing leaked on the way: the script has the objects it had.
    let state = session.state();
    assert!(
        state["script"]["gpu"]["images"].as_u64().unwrap() <= 8,
        "{state:#}"
    );
    session.assert_drawing_clean();
    session.quit();
}

/// The other answer to a script that fails to draw: keep showing the last
/// good frame and ask the script again later. A frame that failed half-way
/// never reaches the screen. Clients are not stalled meanwhile.
fn holding_the_last_frame_while_the_script_fails(device: Device) {
    let project = script_project(
        "faulty-hold",
        device,
        &[("session", DRAW_ONLY_SESSION), ("main", FAULTY)],
    );
    let mut session = Session::start(
        "faulty-hold",
        device,
        &[
            "--output-size",
            "256x192",
            "--script",
            project.to_str().unwrap(),
            "--script-on-error",
            "hold",
            "--script-retry-ms",
            "300",
        ],
    );
    let (mut client, _) = session.mapped_client(options("held", 100, 60, RED));
    move_window(&mut session, &mut client, "held", 40, 30);
    let good = session.screenshot_png("good");

    // The script scribbles over the output and fails: the screen keeps the
    // last good frame, bit for bit.
    session.script("set_fault", json!(["frame", "half"]));
    let held = session.screenshot_png("held");
    assert_eq!(held.data, good.data, "a failed frame reached the screen");
    let script = session.script_state();
    assert_eq!(script["mode"], "hold");
    assert!(script["kept_frames"].as_u64().unwrap() >= 1);
    assert_eq!(script["fallback_frames"], 0);

    // The client draws meanwhile. Its new contents are not shown (the
    // script cannot draw them), but it gets its frame callback and its
    // buffer back, so it does not hang.
    client.clear_events();
    session.recolor(&mut client, "held", GREEN);
    client
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::FrameDone { .. })
        })
        .unwrap()
        .expect("a frame callback while the frame is held");
    let still = session.screenshot_png("still-held");
    assert_rgb(&still, 90, 60, RED, 1, "the held frame");

    // The script is asked again after the retry time, without anything
    // else happening; while it still fails the frame stays.
    let failures = session.script_state()["failures"].as_u64().unwrap();
    session.wait(&mut [&mut client], "the script is tried again", |s| {
        s["script"]["failures"].as_u64().unwrap() > failures
    });
    let still = session.screenshot_png("retried");
    assert_rgb(&still, 90, 60, RED, 1, "the held frame after a retry");
    assert_eq!(session.script_state()["mode"], "hold");

    // The fault goes away: at the next retry the script draws again, and
    // what the client drew meanwhile appears.
    session.script("set_fault", json!(["frame", Value::Null]));
    session.wait(&mut [&mut client], "the script draws again", |s| {
        s["script"]["mode"] == "running"
    });
    let back = session.screenshot_png("back");
    assert_rgb(&back, 90, 60, GREEN, 1, "after the script recovered");
    assert_eq!(session.script_state()["mode"], "running");

    // A script that fails once recovers without anyone doing anything: one
    // frame is kept, and the retry draws. (Whether the retry has already
    // happened when this looks depends on the machine, so the mode is not
    // asserted in between.)
    let before = session.script_state();
    session.script("set_fault", json!(["frame", "once"]));
    session.screenshot_png("once");
    let after = session.script_state();
    assert_eq!(
        after["failures"].as_u64().unwrap(),
        before["failures"].as_u64().unwrap() + 1
    );
    assert!(after["kept_frames"].as_u64().unwrap() > before["kept_frames"].as_u64().unwrap());
    session.recolor(&mut client, "held", BLUE);
    session.wait(
        &mut [&mut client],
        "the script draws again by itself",
        |s| s["script"]["mode"] == "running",
    );
    let back = session.screenshot_png("once-back");
    assert_rgb(&back, 90, 60, BLUE, 1, "after one failure");
    session.assert_drawing_clean();
    session.quit();
}

/// The parts of the script that answer events fail: the window manager's
/// handlers and the hit test. One failure costs one event its scripted
/// answer (the core's default applies). A handler that does not return, or
/// that fails every time, is taken out of charge, so that input does not
/// crawl. Windows stay reachable throughout.
fn a_script_that_fails_on_events_is_taken_out_of_charge(device: Device) {
    let project = script_project("faulty-events", device, &[("main", FAULTY)]);
    let mut session = Session::start(
        "faulty-events",
        device,
        &[
            "--output-size",
            "640x400",
            "--script",
            project.to_str().unwrap(),
            // Long next to what a handler takes, also on a slow machine:
            // only the handler that never returns is to run into it.
            "--script-budget-ms",
            "2000",
        ],
    );
    let mut a = session.client(options("a", 100, 60, RED));
    wait_tiles(&mut session, &mut [&mut a], &[("a", [8, 8], [624, 384])]);

    // `window_opened` throws: the new window is not tiled, but it is there,
    // in the core's default place, with the focus.
    session.script("set_fault", json!(["window_opened", "error"]));
    let (mut b, idb) = session.mapped_client(options("b", 100, 60, GREEN));
    let state = session.state();
    assert_eq!(
        geometry(&state, "b"),
        Some(([64, 64], [100, 60])),
        "{state:#}"
    );
    assert_eq!(state["focus"]["window"], idb);
    wait_keyboard(&mut b);
    let script = session.script_state();
    assert_eq!(script["mode"], "running", "one failure is not the end");
    assert_eq!(script["last_failures"][0]["what"], "window_opened");
    session.script("set_fault", json!(["window_opened", Value::Null]));

    // The hit test throws: the pointer still finds the window, by the
    // core's own rule (positions without transforms).
    session.script("set_fault", json!(["hit_test", "error"]));
    let (surface, seen) = point_at(&mut session, &mut b, (70.0, 70.0)).unwrap();
    assert_eq!(surface, SurfaceKind::Window);
    assert_near(seen, (6.0, 6.0), "flat hit test");
    assert_eq!(session.script_state()["mode"], "running");
    // It keeps throwing: after a few pointer motions the script is out.
    for step in 0..10 {
        pointer_to(&mut session, (70.0 + step as f64, 70.0));
    }
    let script = session.script_state();
    assert_eq!(script["mode"], "fallback", "{script}");
    let encoded = session.screenshot_png("out-of-charge");
    assert_fallback_marker(&encoded, true, "after repeated failures");
    assert_rgb(&encoded, 100, 100, GREEN, 0, "b in the fallback");

    // Back in charge, without the fault.
    session.script("set_fault", json!(["hit_test", Value::Null]));
    session.send(Request::ScriptMode {
        mode: "running".into(),
    });
    assert_eq!(session.script_state()["mode"], "running");

    // A key handler that never returns: that one key takes the time
    // budget, and then the script is out at once. The next keys are not
    // put to the handler at all, and reach the client.
    let key_calls = |session: &mut Session| {
        session.script_state()["timing"]["key"]["calls"]
            .as_u64()
            .unwrap_or(0)
    };
    let calls_before = key_calls(&mut session);
    session.script("set_fault", json!(["key", "loop"]));
    b.clear_events();
    let started = std::time::Instant::now();
    key(&mut session, 30, true);
    key(&mut session, 30, false);
    let first = started.elapsed();
    assert_eq!(session.script_state()["mode"], "fallback");
    assert!(first >= std::time::Duration::from_secs(2), "{first:?}");
    assert_eq!(key_calls(&mut session), calls_before + 1);
    for _ in 0..10 {
        key(&mut session, 31, true);
        key(&mut session, 31, false);
    }
    assert_eq!(
        key_calls(&mut session),
        calls_before + 1,
        "the handler was asked again after it timed out"
    );
    let count = |client: &TestClient, code: u32| {
        client
            .events()
            .iter()
            .filter(|e| matches!(e, ClientEvent::Key { keycode, pressed: true, .. } if *keycode == code))
            .count()
    };
    b.wait_for(common::TIMEOUT, |e| {
        matches!(
            e,
            ClientEvent::Key {
                keycode: 31,
                pressed: false,
                ..
            }
        )
    })
    .unwrap();
    b.pump(Some(std::time::Duration::from_millis(50))).unwrap();
    assert_eq!(
        count(&b, 30),
        1,
        "the key the handler choked on still arrived"
    );
    assert_eq!(count(&b, 31), 10);

    // Without the script in charge windows still open, get the focus, can
    // be clicked and closed.
    let (mut c, idc) = session.mapped_client(options("c", 100, 60, BLUE));
    assert_eq!(session.state()["focus"]["window"], idc);
    wait_keyboard(&mut c);
    pointer_to(&mut session, (20.0, 20.0));
    button(&mut session, true);
    button(&mut session, false);
    let ida = window(&session.state(), "a").unwrap()["id"]
        .as_u64()
        .unwrap();
    assert_eq!(session.state()["focus"]["window"], ida);
    session.send(Request::CloseWindow { window: idc });
    c.wait_for(common::TIMEOUT, |e| *e == ClientEvent::CloseRequested)
        .unwrap()
        .expect("windows can still be closed");

    // A reload puts a working script back: the windows are tiled again.
    // The script that was out of charge handed nothing over, so the new
    // one starts from what the core knows: where the windows are, left to
    // right (a is still in its tile, b where the core put it).
    session.script("set_fault", json!(["key", Value::Null]));
    session.send(Request::ScriptReload { path: None });
    assert_eq!(session.script_state()["mode"], "running");
    wait_tiles(
        &mut session,
        &mut [&mut a, &mut b, &mut c],
        &[("a", [8, 8], [308, 384]), ("b", [324, 8], [308, 384])],
    );
    session.assert_drawing_clean();
    session.quit();
}

/// The script is replaced while the session runs: edited on disk and read
/// again, with the windows open. What the old script hands over (here the
/// order of the tiles, a window's transform, the working space) is there in
/// the new one. A script that does not compile is not taken; one that
/// fails while starting leaves the built-in fallback, until the next
/// reload.
fn a_script_is_reloaded_while_windows_stay_open(device: Device) {
    let project = script_project("reload", device, &[("main", "hz.include(\"session\")\n")]);
    let write = |source: &str| std::fs::write(project.join("main.luau"), source).unwrap();
    let mut session = Session::start(
        "reload",
        device,
        &[
            "--output-size",
            "640x400",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let mut a = session.client(options("a", 100, 60, RED));
    wait_tiles(&mut session, &mut [&mut a], &[("a", [8, 8], [624, 384])]);
    let mut b = session.client(options("b", 100, 60, GREEN));
    let state = wait_tiles(
        &mut session,
        &mut [&mut a, &mut b],
        &[("a", [8, 8], [308, 384]), ("b", [324, 8], [308, 384])],
    );
    let ida = window(&state, "a").unwrap()["id"].as_u64().unwrap();
    let idb = window(&state, "b").unwrap()["id"].as_u64().unwrap();
    // State only the script has: b before a, and another working space.
    logo_key(&mut session, KEY_H, true);
    wait_tiles(
        &mut session,
        &mut [&mut a, &mut b],
        &[("b", [8, 8], [308, 384]), ("a", [324, 8], [308, 384])],
    );
    session.script("set_working_space", json!(["display-p3"]));
    let before = session.screenshot_png("before");
    for client in [&mut a, &mut b] {
        client.clear_events();
    }

    // The same script, read again: nothing changes on screen, and the
    // clients notice nothing.
    session.send(Request::ScriptReload { path: None });
    let script = session.script_state();
    assert_eq!(
        (&script["mode"], &script["reloads"]),
        (&json!("running"), &json!(1))
    );
    assert_eq!(session.script("tile_order", json!([])), json!([idb, ida]));
    assert_eq!(session.script("working_space", json!([])), "display-p3");
    let after = session.screenshot_png("after");
    assert_eq!(after.data, before.data);
    for client in [&mut a, &mut b] {
        client
            .pump(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        assert!(
            !client.events().iter().any(|e| matches!(
                e,
                ClientEvent::Configure { .. }
                    | ClientEvent::CloseRequested
                    | ClientEvent::KeyboardLeave
            )),
            "a reload disturbed a client: {:?}",
            client.events()
        );
    }

    // An edited script: wider gaps. The windows move; their order stays.
    write("local tile = hz.include(\"tile\")\ntile.gap = 20\nhz.include(\"session\")\n");
    session.send(Request::ScriptReload { path: None });
    // (640 - 3 * 20) / 2 = 290 wide, 400 - 2 * 20 = 360 high.
    wait_tiles(
        &mut session,
        &mut [&mut a, &mut b],
        &[("b", [20, 20], [290, 360]), ("a", [330, 20], [290, 360])],
    );
    let encoded = session.screenshot_png("edited");
    assert_rgb(&encoded, 100, 200, GREEN, 1, "b on the left");
    assert_rgb(
        &encoded,
        12,
        200,
        [0x20, 0x20, 0x20, 255],
        0,
        "the wider gap",
    );
    assert_eq!(session.script_state()["reloads"], 2);

    // A script that does not compile: refused, and the one that runs
    // stays.
    write("this is not a script (\n");
    let error = session
        .try_send(Request::ScriptReload { path: None })
        .unwrap_err();
    assert!(error.contains("does not compile"), "{error}");
    let script = session.script_state();
    assert_eq!(
        (&script["mode"], &script["reloads"]),
        (&json!("running"), &json!(2))
    );
    assert_eq!(session.script("tile_order", json!([])), json!([idb, ida]));
    logo_key(&mut session, KEY_L, true);
    wait_tiles(
        &mut session,
        &mut [&mut a, &mut b],
        &[("a", [20, 20], [290, 360]), ("b", [330, 20], [290, 360])],
    );

    // A script that compiles but fails when it starts: the old one is
    // gone by then. The built-in fallback takes over; the windows stay
    // where they are and stay usable.
    write(
        "hz.include(\"session\")\nlocal usual = init\nfunction init(saved)\n    usual(saved)\n    error(\"cannot start\")\nend\n",
    );
    let error = session
        .try_send(Request::ScriptReload { path: None })
        .unwrap_err();
    assert!(error.contains("cannot start"), "{error}");
    assert_eq!(session.script_state()["mode"], "fallback");
    let encoded = session.screenshot_png("failed-start");
    assert_fallback_marker(&encoded, true, "after a failed start");
    assert_rgb(&encoded, 100, 200, RED, 0, "a, drawn by the fallback");
    // (b has the focus: it was the one moved with the keyboard.)
    assert_client_alive(&mut session, &mut b, "b", WHITE);
    session.recolor(&mut b, "b", GREEN);

    // Fixed and reloaded: back to normal. What the script that failed
    // could not hand over is lost (the order is the default again); the
    // windows are not.
    write("hz.include(\"session\")\n");
    session.send(Request::ScriptReload { path: None });
    assert_eq!(session.script_state()["mode"], "running");
    let state = session.wait(&mut [&mut a, &mut b], "tiled again", |s| {
        s["windows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["size"] == json!([308, 384]))
    });
    assert_eq!(state["windows"].as_array().unwrap().len(), 2);
    let encoded = session.screenshot_png("fixed");
    assert_fallback_marker(&encoded, false, "after the fix");

    // The emergency keys: Ctrl+Alt+Shift+F leaves the script, +R reloads.
    const CTRL: u32 = 29;
    const ALT: u32 = 56;
    const KEY_R: u32 = 19;
    const KEY_F: u32 = 33;
    let chord = |session: &mut Session, code: u32| {
        for k in [CTRL, ALT, KEY_LEFTSHIFT, code] {
            key(session, k, true);
        }
        for k in [code, KEY_LEFTSHIFT, ALT, CTRL] {
            key(session, k, false);
        }
    };
    // The focus on the left tile, which is below the right one. The
    // fallback raises the focused window; reloaded without a hand-over, the
    // script still finds the tiles in the order they are on screen.
    let order = session.script("tile_order", json!([]));
    logo_key(&mut session, KEY_H, false);
    let left = order[0].as_u64().unwrap();
    session.wait(&mut [&mut a, &mut b], "the focus on the left tile", |s| {
        s["focus"]["window"] == left
    });
    chord(&mut session, KEY_F);
    assert_eq!(session.script_state()["mode"], "fallback");
    let stacking: Vec<u64> = session.state()["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .collect();
    assert_eq!(stacking.last(), Some(&left), "the focused window is on top");
    let reloads = session.script_state()["reloads"].as_u64().unwrap();
    chord(&mut session, KEY_R);
    let script = session.script_state();
    assert_eq!(script["mode"], "running");
    assert_eq!(script["reloads"].as_u64().unwrap(), reloads + 1);
    assert_eq!(session.script("tile_order", json!([])), order);

    // Another project altogether, by path.
    let other = script_project(
        "reload-other",
        device,
        &[(
            "main",
            "hz.include(\"session\")\nfunction which() return \"other\" end\n",
        )],
    );
    session.send(Request::ScriptReload {
        path: Some(other.clone()),
    });
    assert_eq!(session.script("which", json!([])), "other");
    assert_eq!(session.script_state()["project"], other.to_str().unwrap());
    session.assert_drawing_clean();
    session.quit();
}

/// A reload checks every module of the project before the running script
/// goes, not only `main`: one that does not compile leaves the running
/// script in charge, and the answer says which file and line are wrong.
/// The modules are the `NAME.luau` files of the project directory, the
/// ones `main` includes and the others. A module that compiles but fails
/// while the script loads is found only by loading it, after the running
/// script is gone: the built-in fallback takes over, until the next
/// reload.
fn a_module_that_does_not_compile_is_not_taken(device: Device) {
    let main = "local gaps = hz.include(\"gaps\")\n\
                local tile = hz.include(\"tile\")\n\
                tile.gap = gaps.gap\n\
                function gap() return gaps.gap end\n\
                hz.include(\"session\")\n";
    let project = script_project(
        "reload-module",
        device,
        &[("main", main), ("gaps", "return { gap = 20 }\n")],
    );
    let write = |file: &str, source: &str| std::fs::write(project.join(file), source).unwrap();
    let mut session = Session::start(
        "reload-module",
        device,
        &[
            "--output-size",
            "640x400",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let mut a = session.client(options("a", 100, 60, RED));
    wait_tiles(&mut session, &mut [&mut a], &[("a", [20, 20], [600, 360])]);

    let refused = |session: &mut Session, file: &str| {
        let error = session
            .try_send(Request::ScriptReload { path: None })
            .unwrap_err();
        assert!(error.contains("does not compile"), "{error}");
        assert!(error.contains("the running one was kept"), "{error}");
        assert!(error.contains(file), "{error}");
        error
    };
    let assert_kept = |session: &mut Session, reloads: u64| {
        let script = session.script_state();
        assert_eq!(
            (&script["mode"], &script["reloads"]),
            (&json!("running"), &json!(reloads))
        );
        assert_eq!(session.script("gap", json!([])), 20);
    };

    // A syntax error on the second line of a module `main` includes.
    write("gaps.luau", "return {\n    gap = = 30,\n}\n");
    let error = refused(&mut session, "\"gaps.luau\"]:2:");
    assert!(error.contains("syntax error"), "{error}");
    assert_kept(&mut session, 0);
    let encoded = session.screenshot_png("module-refused");
    assert_fallback_marker(&encoded, false, "after a refused reload");
    assert_rgb(&encoded, 300, 200, RED, 1, "a, drawn by the script");
    wait_tiles(&mut session, &mut [&mut a], &[("a", [20, 20], [600, 360])]);

    // Fixed: the new module is taken.
    write("gaps.luau", "return { gap = 30 }\n");
    session.send(Request::ScriptReload { path: None });
    wait_tiles(&mut session, &mut [&mut a], &[("a", [30, 30], [580, 340])]);
    let script = session.script_state();
    assert_eq!(
        (&script["mode"], &script["reloads"]),
        (&json!("running"), &json!(1))
    );
    assert_eq!(session.script("gap", json!([])), 30);

    // A module nothing includes is checked as well. A file whose name is
    // not a module name is not a module, and is not read.
    write("scratch.luau", "local unfinished =\n");
    refused(&mut session, "\"scratch.luau\"]:");
    write("scratch.luau", "return nil\n");
    write("gaps.old.luau", "this is not a script (\n");
    session.send(Request::ScriptReload { path: None });
    assert_eq!(session.script_state()["reloads"], 2);

    // An error in `main` itself is answered the same way.
    write("main.luau", "this is not a script (\n");
    refused(&mut session, "\"main.luau\"]:1:");
    write("main.luau", main);

    // A module that compiles but fails while the script loads: the
    // running script is gone by then, and the fallback draws.
    write("gaps.luau", "error(\"no gaps today\")\n");
    let error = session
        .try_send(Request::ScriptReload { path: None })
        .unwrap_err();
    assert!(error.contains("failed to load"), "{error}");
    assert!(error.contains("no gaps today"), "{error}");
    assert_eq!(session.script_state()["mode"], "fallback");
    let encoded = session.screenshot_png("module-failed");
    assert_fallback_marker(&encoded, true, "after a module failed to load");
    assert_client_alive(&mut session, &mut a, "a", WHITE);

    // Fixed and reloaded: the script is back.
    write("gaps.luau", "return { gap = 20 }\n");
    session.send(Request::ScriptReload { path: None });
    assert_eq!(session.script_state()["mode"], "running");
    let encoded = session.screenshot_png("module-fixed");
    assert_fallback_marker(&encoded, false, "after the fix");
    session.assert_drawing_clean();
    session.quit();
}

// ---------------------------------------------------------------------------
// Pointer constraints
// ---------------------------------------------------------------------------

/// Whether a lock may be in effect is the script's to decide, and the way
/// out is a key of its own (logo+Escape in the built-in tiling).
fn the_script_decides_when_the_pointer_is_held(device: Device) {
    let project = script_project(
        "pointer-held",
        device,
        &[(
            "main",
            r#"
            hz.include("session")

            -- Locks only once the script says so; confinements never.
            local allow = false
            local asked = {}
            local changes = {}
            function pointer_constraint(c)
                table.insert(asked, c.kind .. ":" .. tostring(c.window ~= nil))
                return c.kind == "lock" and allow
            end
            function pointer_constraint_changed(c)
                table.insert(changes, c and (c.kind .. ":" .. tostring(c.active)) or "none")
            end
            function allow_locks()
                allow = true
                hz.review_pointer_constraints()
            end
            function report()
                local current = hz.pointer_constraint()
                local result = {
                    asked = asked,
                    changes = changes,
                    current = current and current.kind or "none",
                }
                asked = {}
                changes = {}
                return result
            end
            "#,
        )],
    );
    let mut session = Session::start(
        "pointer-held",
        device,
        &[
            "--output-size",
            "400x300",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let (mut client, _) = session.mapped_client(options("game", 100, 80, RED));
    wait_keyboard(&mut client);
    let state = session.wait(&mut [&mut client], "the window tiled", |state| {
        geometry(state, "game").is_some()
    });
    let (position, size) = geometry(&state, "game").unwrap();
    let center = (
        (position[0] + size[0] / 2) as f64,
        (position[1] + size[1] / 2) as f64,
    );
    client
        .constrain_pointer(ConstraintRequest::Lock, None, true)
        .unwrap();
    session.wait(&mut [&mut client], "the lock is asked for", |state| {
        state["pointer_constraints"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
    });
    pointer_to(&mut session, center);

    // Asked, and refused.
    let report = session.script("report", json!([]));
    assert_eq!(report["asked"], json!(["lock:true"]), "{report}");
    assert_eq!(report["current"], "none");
    assert_eq!(session.state()["pointer_constraints"][0]["active"], false);

    // The script changes its mind.
    session.script("allow_locks", json!([]));
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerLocked)
        .unwrap()
        .expect("the pointer is locked");
    let report = session.script("report", json!([]));
    assert_eq!(report["changes"], json!(["lock:true"]), "{report}");
    assert_eq!(report["current"], "lock");

    // The way out: the built-in tiling's logo+Escape.
    logo_key(&mut session, 1, false);
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerUnlocked)
        .unwrap()
        .expect("the pointer is free again");
    let report = session.script("report", json!([]));
    assert_eq!(report["changes"], json!(["none"]), "{report}");
    // Not offered again while nothing changes: the pointer moves freely.
    pointer_to(&mut session, (center.0 + 5.0, center.1));
    let report = session.script("report", json!([]));
    // An empty table comes back as an empty JSON object.
    assert_eq!(report["asked"], json!({}), "{report}");
    assert_eq!(session.state()["pointer"]["position"][0], center.0 + 5.0);

    // Off the window and back: asked again, and in effect.
    pointer_to(&mut session, (1.0, 1.0));
    pointer_to(&mut session, center);
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerLocked)
        .unwrap()
        .expect("the pointer is locked again");
    session.assert_script_healthy();
    session.quit();
}

// ---------------------------------------------------------------------------
// Touch and touchpad gestures
// ---------------------------------------------------------------------------

/// Pumps a client until `done`.
#[track_caller]
fn wait_client(client: &mut TestClient, what: &str, done: impl Fn(&TestClient) -> bool) {
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while !done(client) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            client.events()
        );
        client
            .pump(Some(std::time::Duration::from_millis(10)))
            .unwrap();
    }
}

fn gestures(client: &TestClient) -> Vec<ClientEvent> {
    client
        .events()
        .iter()
        .filter(|e| {
            matches!(
                e,
                ClientEvent::GestureBegin { .. }
                    | ClientEvent::GestureUpdate { .. }
                    | ClientEvent::GestureEnd { .. }
            )
        })
        .cloned()
        .collect()
}

/// The built-in tiling takes three finger swipes for itself: they move the
/// focus, and no application sees any of them. Every other gesture goes to
/// the application under the pointer. A script that answers `touch_down`
/// decides the focus of a touch; the touch reaches the application anyway.
fn the_script_takes_the_gestures_it_wants(device: Device) {
    let project = script_project(
        "gestures",
        device,
        &[(
            "main",
            r#"
            hz.include("session")

            -- What the tiling is asked about gestures, and every touch,
            -- which leaves the focus alone.
            local tile_gesture_begin = gesture_begin
            local asked = {}
            local touches = {}
            function gesture_begin(g)
                table.insert(asked, g.kind .. ":" .. g.fingers .. ":" .. tostring(g.window))
                return tile_gesture_begin(g)
            end
            function touch_down(slot, x, y, window, surface)
                table.insert(touches, { slot = slot, x = x, y = y, window = window })
                return true
            end
            function report()
                local result = { asked = asked, touches = touches }
                asked = {}
                touches = {}
                return result
            end
            "#,
        )],
    );
    let mut session = Session::start(
        "gestures",
        device,
        &[
            "--output-size",
            "640x400",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let (mut a, ida) = session.mapped_client(options("a", 100, 60, RED));
    let (mut b, idb) = session.mapped_client(options("b", 100, 60, GREEN));
    wait_tiles(
        &mut session,
        &mut [&mut a, &mut b],
        &[("a", [8, 8], [308, 384]), ("b", [324, 8], [308, 384])],
    );
    for client in [&mut a, &mut b] {
        client.request_touch();
        wait_client(client, "a wl_touch", |c| c.has_touch());
        client.request_pointer_gestures().unwrap();
    }
    wait_keyboard(&mut b);
    pointer_to(&mut session, (478.0, 200.0));
    a.clear_events();
    b.clear_events();

    // Three fingers to the left: the focus goes to the left tile.
    for request in [
        Request::GestureBegin {
            kind: "swipe".into(),
            fingers: 3,
        },
        Request::GestureUpdate {
            kind: "swipe".into(),
            dx: -40.0,
            dy: 3.0,
            scale: 1.0,
            rotation: 0.0,
        },
        Request::GestureUpdate {
            kind: "swipe".into(),
            dx: -40.0,
            dy: 0.0,
            scale: 1.0,
            rotation: 0.0,
        },
    ] {
        session.send(request);
    }
    assert_eq!(
        session.state()["touch"]["gesture"],
        json!({ "kind": "swipe", "owner": "window_manager" })
    );
    session.send(Request::GestureEnd {
        kind: "swipe".into(),
        cancelled: false,
    });
    session.wait(&mut [&mut a, &mut b], "focus on a by a swipe", |s| {
        s["focus"]["window"] == ida
    });
    wait_keyboard(&mut a);

    // A pinch and a four finger swipe are the application's.
    session.send(Request::GestureBegin {
        kind: "pinch".into(),
        fingers: 2,
    });
    session.send(Request::GestureUpdate {
        kind: "pinch".into(),
        dx: 0.0,
        dy: 0.0,
        scale: 1.5,
        rotation: -2.0,
    });
    session.send(Request::GestureEnd {
        kind: "pinch".into(),
        cancelled: false,
    });
    session.send(Request::GestureBegin {
        kind: "swipe".into(),
        fingers: 4,
    });
    session.send(Request::GestureEnd {
        kind: "swipe".into(),
        cancelled: true,
    });
    b.wait_for(common::TIMEOUT, |e| {
        matches!(
            e,
            ClientEvent::GestureEnd {
                kind: GestureKind::Swipe,
                ..
            }
        )
    })
    .unwrap()
    .expect("the four finger swipe reaches b");
    // Nothing of the three finger swipe reached anyone; it came first.
    assert_eq!(
        gestures(&b),
        [
            ClientEvent::GestureBegin {
                kind: GestureKind::Pinch,
                fingers: 2,
                surface: SurfaceKind::Window
            },
            ClientEvent::GestureUpdate {
                kind: GestureKind::Pinch,
                dx: 0.0,
                dy: 0.0,
                scale: 1.5,
                rotation: -2.0
            },
            ClientEvent::GestureEnd {
                kind: GestureKind::Pinch,
                cancelled: false
            },
            ClientEvent::GestureBegin {
                kind: GestureKind::Swipe,
                fingers: 4,
                surface: SurfaceKind::Window
            },
            ClientEvent::GestureEnd {
                kind: GestureKind::Swipe,
                cancelled: true
            },
        ]
    );
    a.pump(Some(std::time::Duration::from_millis(20))).unwrap();
    assert_eq!(gestures(&a), []);
    let report = session.script("report", json!([]));
    assert_eq!(
        report["asked"],
        json!([
            format!("swipe:3:{idb}"),
            format!("pinch:2:{idb}"),
            format!("swipe:4:{idb}")
        ]),
        "{report}"
    );

    // A touch on b: the script keeps the focus on a; b gets the touch.
    session.send(Request::TouchDown {
        slot: 0,
        x: 400.0,
        y: 100.0,
    });
    session.send(Request::TouchFrame);
    b.wait_for(common::TIMEOUT, |e| {
        matches!(e, ClientEvent::TouchDown { .. })
    })
    .unwrap()
    .expect("the touch reaches b");
    assert!(b.events().contains(&ClientEvent::TouchDown {
        id: 0,
        surface: SurfaceKind::Window,
        x: 76.0,
        y: 92.0
    }));
    session.send(Request::TouchUp { slot: 0 });
    session.send(Request::TouchFrame);
    let report = session.script("report", json!([]));
    let touches = report["touches"].as_array().expect("one touch");
    assert_eq!(touches.len(), 1, "{report}");
    let field = |name: &str| touches[0][name].as_f64();
    assert_eq!(
        [field("slot"), field("x"), field("y"), field("window")],
        [Some(0.0), Some(400.0), Some(100.0), Some(idb as f64)],
        "{report}"
    );
    assert_eq!(session.state()["focus"]["window"], ida);

    // A dialog of b floats above the tiles and has the focus. A swipe to
    // the left goes from its parent's tile, b, to the tile on its left, a;
    // floating windows are passed over.
    session.send(Request::FocusWindow { window: Some(idb) });
    b.open_dialog(DialogOptions::default()).unwrap();
    let state = session.wait(&mut [&mut a, &mut b], "the dialog has the focus", |s| {
        window(s, "dialog").is_some_and(|w| w["mapped"] == true)
            && s["focus"]["window"] == window(s, "dialog").unwrap()["id"]
    });
    let dialog = window(&state, "dialog").unwrap()["id"].as_u64().unwrap();
    for request in [
        Request::GestureBegin {
            kind: "swipe".into(),
            fingers: 3,
        },
        Request::GestureUpdate {
            kind: "swipe".into(),
            dx: -90.0,
            dy: 0.0,
            scale: 1.0,
            rotation: 0.0,
        },
        Request::GestureEnd {
            kind: "swipe".into(),
            cancelled: false,
        },
    ] {
        session.send(request);
    }
    session.wait(&mut [&mut a, &mut b], "focus on a from the dialog", |s| {
        s["focus"]["window"] == ida
    });
    assert_ne!(dialog, ida);
    session.assert_script_healthy();
    session.quit();
}
