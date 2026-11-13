//! Whether the two example projects (`projects/fancy`, `projects/room`)
//! handle color as the design asks: what a window stored comes back on an
//! output that means the same, every reference white lands on the same
//! light, dimming is done in light, brighter-than-white stays brighter and
//! lights the room more, and what an output cannot show is cut as the exit
//! conversion cuts it.
//!
//! The projects are used as they are. Where a scenario prints numbers, those
//! are the finding; the assertions keep them from getting worse unnoticed.

mod common;

use std::path::PathBuf;

use common::{Device, Session, options, window};
use proto_color::{AlphaMode, CompositeSpace, DecodeTransform, EncodeTransform, ImageDescription};
use proto_compositor::control::Request;
use proto_compositor::image::{CompositeImage, EncodedImage};
use proto_test_client::{ClientOptions, Pattern, TestClient};
use serde_json::{Value, json};

device_tests!(
    fancy_gives_back_what_a_window_stored,
    fancy_keeps_reference_white_and_dims_in_light,
    fancy_cuts_what_an_srgb_output_cannot_show,
    room_up_close_gives_back_nearly_what_a_window_stored,
    room_from_the_chair_shows_a_window_through_a_lens,
    room_shows_hdr_brighter_and_it_lights_the_room_more,
);

const KEY_F: u32 = 33;
const KEY_LEFTMETA: u32 = 125;

/// Stored colors to send through a project: the ends, the dark steps where
/// rounding shows first, greys, saturated and mixed colors.
const COLORS: [[u8; 4]; 24] = [
    [0, 0, 0, 255],
    [1, 1, 1, 255],
    [2, 2, 2, 255],
    [5, 5, 5, 255],
    [16, 16, 16, 255],
    [64, 64, 64, 255],
    [127, 127, 127, 255],
    [128, 128, 128, 255],
    [200, 200, 200, 255],
    [254, 254, 254, 255],
    [255, 255, 255, 255],
    [255, 0, 0, 255],
    [0, 255, 0, 255],
    [0, 0, 255, 255],
    [255, 255, 0, 255],
    [0, 255, 255, 255],
    [255, 0, 255, 255],
    [1, 0, 0, 255],
    [0, 3, 0, 255],
    [0, 0, 7, 255],
    [210, 60, 50, 255],
    [30, 30, 46, 255],
    [17, 133, 249, 255],
    [251, 3, 126, 255],
];

/// The outputs a window is sent to with the same meaning as its own:
/// label, arguments of the output, the preset of the window (none: what
/// content without color information is), bits of the output.
type Meaning = (
    &'static str,
    &'static [&'static str],
    Option<&'static str>,
    u32,
);
const MEANINGS: [Meaning; 3] = [
    ("srgb", &[], None, 8),
    (
        "display-p3",
        &["--output-image-description", "display-p3"],
        Some("display-p3"),
        8,
    ),
    (
        "bt2020-pq",
        &[
            "--output-image-description",
            "bt2020-pq",
            "--output-format",
            "rgba16",
        ],
        Some("bt2020-pq"),
        16,
    ),
];

fn project(name: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("projects")
        .join(name)
}

/// Starts a project on a manual clock.
fn start(project_name: &str, name: &str, device: Device, size: &str, extra: &[&str]) -> Session {
    let project = project(project_name);
    // The scenarios run side by side and share the projects: their shaders
    // are built once.
    static SHADERS: std::sync::Once = std::sync::Once::new();
    SHADERS.call_once(|| {
        common::build_shaders(&self::project("fancy"));
        common::build_shaders(&self::project("room"));
    });
    let mut args = vec![
        "--output-size",
        size,
        "--clock",
        "manual",
        "--script",
        project.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    Session::start(name, device, &args)
}

fn advance(session: &mut Session, seconds: f64) {
    session.send(Request::AdvanceClock {
        milliseconds: seconds * 1000.0,
    });
}

fn logo_key(session: &mut Session, keycode: u32) {
    for (code, pressed) in [
        (KEY_LEFTMETA, true),
        (keycode, true),
        (keycode, false),
        (KEY_LEFTMETA, false),
    ] {
        session.send(Request::Key {
            keycode: code,
            pressed,
        });
    }
}

fn surface_id(session: &mut Session, title: &str) -> u64 {
    window(&session.state(), title).expect("the window exists")["surfaces"][0]["id"]
        .as_u64()
        .unwrap()
}

fn window_id(session: &mut Session, title: &str) -> u64 {
    window(&session.state(), title).expect("the window exists")["id"]
        .as_u64()
        .unwrap()
}

fn set_meaning(session: &mut Session, title: &str, preset: Option<&str>) {
    if let Some(preset) = preset {
        let surface = surface_id(session, title);
        session.send(Request::SetSurfaceColor {
            surface,
            image_description: preset.into(),
            alpha_mode: None,
        });
    }
}

/// What a stored color is on an output of `bits` that means the same.
fn same_code(color: [u8; 4], bits: u32) -> [i64; 3] {
    let scale = if bits == 16 { 257 } else { 1 };
    [0, 1, 2].map(|c| color[c] as i64 * scale)
}

/// The largest difference of the three color channels.
fn worst(actual: [u16; 4], expected: [i64; 3]) -> i64 {
    (0..3)
        .map(|c| (actual[c] as i64 - expected[c]).abs())
        .max()
        .unwrap()
}

fn close(actual: [f32; 4], expected: [f32; 4], tolerance: f32) -> bool {
    (0..4).all(|c| (actual[c] - expected[c]).abs() <= 2e-5 + expected[c].abs() * tolerance)
}

// ----- fancy -------------------------------------------------------------------

fn fancy_state(session: &mut Session) -> Value {
    session.script("fancy_state", json!([]))
}

/// Lets time pass until nothing moves, pumping the clients.
fn fancy_settle(session: &mut Session, clients: &mut [&mut TestClient]) {
    for _ in 0..40 {
        advance(session, 0.5);
        session.wait(clients, "clients served", |_| true);
        session.screenshot_png("settle");
        if fancy_state(session)["moving"] == false {
            return;
        }
    }
    panic!("fancy never came to rest: {:#}", fancy_state(session));
}

fn fancy_open(
    session: &mut Session,
    others: &mut [&mut TestClient],
    options: ClientOptions,
) -> TestClient {
    let title = options.title.clone();
    let mut client = session.client(options);
    let mut all: Vec<&mut TestClient> = others.iter_mut().map(|c| &mut **c).collect();
    all.push(&mut client);
    session.wait(&mut all, &format!("{title} mapped"), |s| {
        window(s, &title).is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
    });
    client
}

/// Where the last frame of fancy drew a window: x, y, width, height in
/// pixels of the output (the scale is 1).
fn fancy_rect(session: &mut Session, title: &str) -> (f64, f64, f64, f64) {
    let id = window_id(session, title);
    let state = fancy_state(session);
    let drawn = state["windows"]
        .as_array()
        .and_then(|windows| windows.iter().find(|w| w["id"] == id))
        .unwrap_or_else(|| panic!("{title} was not drawn: {state:#}"));
    let n = |name: &str| drawn[name].as_f64().unwrap();
    (n("x"), n("y"), n("width"), n("height"))
}

/// The middles of the four quadrants of a rectangle.
fn quadrant_middles((x, y, w, h): (f64, f64, f64, f64)) -> [(u32, u32); 4] {
    [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)]
        .map(|(u, v)| ((x + w * u).round() as u32, (y + h * v).round() as u32))
}

/// Shows `COLORS` four at a time in the window `title` and calls `look`
/// with each picture and the four colors, in the order of the quadrants.
fn each_four(
    session: &mut Session,
    client: &mut TestClient,
    title: &str,
    mut look: impl FnMut(&mut Session, &EncodedImage, &CompositeImage, [[u8; 4]; 4]),
) {
    for (i, &four) in COLORS.as_chunks::<4>().0.iter().enumerate() {
        session.redraw(client, title, |o| o.pattern = Pattern::Quadrants(four));
        let (encoded, composite) = session.screenshot(&format!("colors-{i}"));
        look(session, &encoded, &composite, four);
    }
}

/// A window that is not touched comes back as its client stored it, when
/// the output means the same as the window: the reason the default format
/// of color buffers is RGBA32F. With RGBA16F it does not, on a PQ output.
fn fancy_gives_back_what_a_window_stored(device: Device) {
    for (label, output, preset, bits) in MEANINGS {
        for format in ["rgba32float", "rgba16float"] {
            let name = format!("colors-fancy-{label}-{format}");
            let mut extra = output.to_vec();
            extra.extend_from_slice(&["--color-format", format]);
            let mut session = start("fancy", &name, device, "640x400", &extra);
            let mut client = fancy_open(&mut session, &mut [], options("a", 200, 120, COLORS[5]));
            set_meaning(&mut session, "a", preset);
            fancy_settle(&mut session, &mut [&mut client]);
            let rect = fancy_rect(&mut session, "a");
            let (mut largest, mut wrong) = (0, 0);
            each_four(&mut session, &mut client, "a", |_, encoded, _, four| {
                for (color, (x, y)) in four.iter().zip(quadrant_middles(rect)) {
                    let difference = worst(encoded.pixel(x, y), same_code(*color, bits));
                    largest = largest.max(difference);
                    wrong += (difference != 0) as u32;
                    if difference != 0 {
                        println!(
                            "  {label}, {format}: stored {:?} came out as {:?}",
                            same_code(*color, bits),
                            &encoded.pixel(x, y)[..3]
                        );
                    }
                }
            });
            println!(
                "fancy, {label} window on a {label} output ({bits} bit), {format}: \
                 {wrong} of {} colors differ, by at most {largest} code values",
                COLORS.len()
            );
            // With RGBA32F nothing is lost on an output of 8 bits. On the
            // 16 bit PQ output the conversions themselves (32 bit floats
            // through the PQ curve and back) leave less than a step of a
            // 10 bit signal (64).
            if format == "rgba32float" {
                let limit = if bits == 16 { 64 } else { 0 };
                assert!(
                    largest <= limit,
                    "{label}: a window does not come back as stored (off by {largest})"
                );
            }
            session.assert_script_healthy();
            drop(client);
            session.quit();
        }
    }
}

/// On an HDR output, an SDR window's white and an HDR window's reference
/// white are the same light, and the window without the focus is darker by
/// a factor on its light (not on its code values).
fn fancy_keeps_reference_white_and_dims_in_light(device: Device) {
    let space = CompositeSpace::default();
    let pq = ImageDescription::BT2020_PQ;
    let decode = |description: &ImageDescription, color: [u8; 4]| {
        DecodeTransform::new(description, AlphaMode::PremultipliedElectrical, &space)
            .unwrap()
            .apply(color.map(|v| v as f32 / 255.0))
    };
    let encode = EncodeTransform::new(&space, &pq).unwrap();
    // PQ code 148 of 255 is 201 cd/m²: next to the reference white of 203.
    // Code 200 is about 1000 cd/m².
    let sdr_white = [255u8, 255, 255, 255];
    let hdr_white = [148u8, 148, 148, 255];
    let hdr_bright = [200u8, 200, 200, 255];
    const DIM: f32 = 0.72; // `cold_brightness` of the project

    // The second window opened has the focus.
    for (name, first_is_hdr) in [("sdr-then-hdr", false), ("hdr-then-sdr", true)] {
        let mut session = start(
            "fancy",
            &format!("colors-fancy-white-{name}"),
            device,
            "640x400",
            &[
                "--output-image-description",
                "bt2020-pq",
                "--output-format",
                "rgba16",
            ],
        );
        let hdr_options = ClientOptions {
            pattern: Pattern::Quadrants([hdr_white, hdr_bright, hdr_white, hdr_bright]),
            ..options("hdr", 200, 120, hdr_white)
        };
        let sdr_options = options("sdr", 200, 120, sdr_white);
        let (mut a, mut b) = if first_is_hdr {
            let mut a = fancy_open(&mut session, &mut [], hdr_options);
            let b = fancy_open(&mut session, &mut [&mut a], sdr_options);
            (a, b)
        } else {
            let mut a = fancy_open(&mut session, &mut [], sdr_options);
            let b = fancy_open(&mut session, &mut [&mut a], hdr_options);
            (a, b)
        };
        set_meaning(&mut session, "hdr", Some("bt2020-pq"));
        fancy_settle(&mut session, &mut [&mut a, &mut b]);
        let (encoded, composite) = session.screenshot("white");
        let (sdr_dim, hdr_dim) = if first_is_hdr { (1.0, DIM) } else { (DIM, 1.0) };

        let sdr_at = quadrant_middles(fancy_rect(&mut session, "sdr"))[0];
        let hdr_at = quadrant_middles(fancy_rect(&mut session, "hdr"));
        let dimmed = |c: [f32; 4], k: f32| [c[0] * k, c[1] * k, c[2] * k, c[3]];
        let checks = [
            (
                "the SDR white",
                sdr_at,
                dimmed(decode(&ImageDescription::SRGB_GAMMA22, sdr_white), sdr_dim),
            ),
            (
                "the HDR reference white",
                hdr_at[0],
                dimmed(decode(&pq, hdr_white), hdr_dim),
            ),
            (
                "the HDR highlight",
                hdr_at[1],
                dimmed(decode(&pq, hdr_bright), hdr_dim),
            ),
        ];
        for (what, (x, y), expected) in checks {
            let actual = composite.pixel(x, y);
            assert!(
                close(actual, expected, 2e-3),
                "{name}: {what} is {actual:?} in the working space, expected {expected:?}"
            );
            // And on the output: that light, as PQ.
            let code = encode.apply([expected[0], expected[1], expected[2]]);
            let pixel = encoded.pixel(x, y);
            for c in 0..3 {
                assert!(
                    (pixel[c] as f32 - code[c] * 65535.0).abs() <= 48.0,
                    "{name}: {what} is {pixel:?} on the output, expected {code:?}"
                );
            }
            println!(
                "fancy, {name}: {what}: {actual:?} in the working space, {pixel:?} on the PQ output"
            );
        }
        // The two whites are the same light when neither is dimmed.
        let sdr = composite.pixel(sdr_at.0, sdr_at.1)[1] / sdr_dim;
        let hdr = composite.pixel(hdr_at[0].0, hdr_at[0].1)[1] / hdr_dim;
        assert!((sdr - 1.0).abs() < 2e-3, "{sdr}");
        assert!((hdr / sdr - 1.0).abs() < 0.03, "{hdr} {sdr}");
        session.assert_script_healthy();
        drop((a, b));
        session.quit();
    }
}

/// Colors of a Display P3 window that sRGB does not have, on an sRGB output:
/// the exit conversion cuts each channel to 0..1 by itself. Recorded as what
/// happens now (no gamut mapping).
fn fancy_cuts_what_an_srgb_output_cannot_show(device: Device) {
    let mut session = start("fancy", "colors-fancy-gamut", device, "640x400", &[]);
    let four = [
        [255u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 128, 0, 255],
    ];
    let wide = ClientOptions {
        pattern: Pattern::Quadrants(four),
        ..options("p3", 200, 120, four[0])
    };
    let mut client = fancy_open(&mut session, &mut [], wide);
    set_meaning(&mut session, "p3", Some("display-p3"));
    fancy_settle(&mut session, &mut [&mut client]);
    let (encoded, composite) = session.screenshot("gamut");
    let rect = fancy_rect(&mut session, "p3");

    let space = CompositeSpace::default();
    let srgb = ImageDescription::SRGB_GAMMA22;
    let to_working = DecodeTransform::new(
        &ImageDescription::DISPLAY_P3,
        AlphaMode::PremultipliedElectrical,
        &space,
    )
    .unwrap();
    let to_output = EncodeTransform::new(&space, &srgb).unwrap();
    // The same light in linear sRGB, before anything is cut.
    let uncut = proto_color::Primaries::ACES_AP1
        .conversion_to(&proto_color::Primaries::SRGB)
        .unwrap();
    for (color, (x, y)) in four.iter().zip(quadrant_middles(rect)) {
        let working = to_working.apply(color.map(|v| v as f32 / 255.0));
        let held = composite.pixel(x, y);
        assert!(close(held, working, 2e-3), "{held:?} {working:?}");
        // Nothing of it is negative in the working space.
        assert!(working.iter().all(|v| *v >= -1e-6), "{working:?}");
        let expected = to_output.apply([working[0], working[1], working[2]]);
        let pixel = encoded.pixel(x, y);
        for c in 0..3 {
            assert!(
                (pixel[c] as f32 - expected[c] * 255.0).abs() <= 1.0,
                "{color:?}: {pixel:?} on the output, expected {expected:?}"
            );
        }
        let linear = uncut.mul_vec([working[0] as f64, working[1] as f64, working[2] as f64]);
        println!(
            "fancy, display-p3 {color:?} on an sRGB output: linear sRGB \
             [{:.3}, {:.3}, {:.3}] is shown as {:?}",
            linear[0],
            linear[1],
            linear[2],
            &pixel[..3]
        );
    }
    session.assert_script_healthy();
    drop(client);
    session.quit();
}

// ----- room ----------------------------------------------------------------------

const MAIN: u64 = 1;
const MAIN_SIZE: (u32, u32) = (1920, 1080);
// The settings of the project (`main.luau`): the longest change.
const SETTLE_SECONDS: f64 = 0.6;

fn room_settle(session: &mut Session) {
    advance(session, SETTLE_SECONDS);
    session.screenshot_png("settle");
    session.screenshot_png("settle");
}

/// Opens a window that takes the main monitor.
fn room_open(session: &mut Session, options: ClientOptions) -> TestClient {
    let title = options.title.clone();
    let mut client = session.client(options);
    session.wait(&mut [&mut client], "the window has the monitor", |s| {
        window(s, &title).is_some_and(|w| {
            w["size"] == json!([MAIN_SIZE.0, MAIN_SIZE.1])
                && w["surfaces"][0]["buffer_size"].is_array()
        })
    });
    client
}

/// Where a point of the main monitor (in its pixels) is in the picture.
fn room_point(session: &mut Session, x: f64, y: f64) -> (u32, u32) {
    let point = session.script("monitor_point", json!([MAIN, x, y]));
    (
        point[0].as_f64().unwrap().round() as u32,
        point[1].as_f64().unwrap().round() as u32,
    )
}

fn room_set(session: &mut Session, name: &str, value: f64) {
    session.script("room_set", json!([name, value]));
}

/// Up close (logo+F), the main monitor fills the output: the window on it
/// goes through the monitor's own image, the perspective view of the room
/// with several samples per pixel, and the exit. How far what comes out is
/// from what the client stored, with the project as it is.
fn room_up_close_gives_back_nearly_what_a_window_stored(device: Device) {
    // (the size of the output, the largest difference accepted: in code
    // values of 8 bits, and of 16 bits for the PQ output)
    for (size, limit_8, limit_16) in [("1920x1080", 1, 320), ("960x540", 1, 320)] {
        for (label, output, preset, bits) in MEANINGS {
            let name = format!("colors-room-{label}-{size}");
            let mut session = start("room", &name, device, size, output);
            let mut client = room_open(&mut session, options("a", 100, 60, COLORS[5]));
            set_meaning(&mut session, "a", preset);
            logo_key(&mut session, KEY_F);
            assert_eq!(session.script("room_state", json!([]))["zoomed"], true);
            room_settle(&mut session);
            let (width, height) = {
                let image = session.screenshot_png("zoomed");
                (image.width as f64, image.height as f64)
            };
            let rect = (0.0, 0.0, width, height);
            let (mut largest, mut wrong, mut worst_color) = (0, 0, COLORS[0]);
            each_four(&mut session, &mut client, "a", |_, encoded, _, four| {
                for (color, (x, y)) in four.iter().zip(quadrant_middles(rect)) {
                    let difference = worst(encoded.pixel(x, y), same_code(*color, bits));
                    if difference > largest {
                        (largest, worst_color) = (difference, *color);
                    }
                    wrong += (difference != 0) as u32;
                }
            });
            println!(
                "room up close, {label} window on a {label} output ({bits} bit), {size}: \
                 {wrong} of {} colors differ, by at most {largest} code values (at {worst_color:?})",
                COLORS.len()
            );
            let limit = if bits == 16 { limit_16 } else { limit_8 };
            assert!(largest <= limit, "{label} at {size}: off by {largest}");

            // An edge between white and black, down and across the middle
            // of the monitor: how many pixels of the output are neither.
            if bits == 8 {
                let (white, black) = ([255u8, 255, 255, 255], [0u8, 0, 0, 255]);
                session.redraw(&mut client, "a", |o| {
                    o.pattern = Pattern::Quadrants([white, black, black, white])
                });
                let encoded = session.screenshot_png("edge");
                let mixed = |pixels: Vec<[u16; 4]>| -> Vec<u16> {
                    pixels
                        .iter()
                        .map(|p| p[1])
                        .filter(|g| *g != 0 && *g != 255)
                        .collect()
                };
                let (w, h) = (encoded.width, encoded.height);
                let across = mixed(
                    (w / 2 - 8..w / 2 + 8)
                        .map(|x| encoded.pixel(x, h / 4))
                        .collect(),
                );
                let down = mixed(
                    (h / 2 - 8..h / 2 + 8)
                        .map(|y| encoded.pixel(w / 4, y))
                        .collect(),
                );
                println!(
                    "room up close, {label}, {size}: across the edge between white and black,                      {} pixels are neither ({across:?}); down it, {} ({down:?})",
                    across.len(),
                    down.len()
                );
                assert!(across.len() <= 2 && down.len() <= 2);
            }
            session.assert_script_healthy();
            drop(client);
            session.quit();
        }
    }
}

/// The mean of the green channel over a part of the compositing buffer
/// (fractions of its size).
fn mean_green(image: &CompositeImage, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> f64 {
    let (w, h) = (image.width as f64, image.height as f64);
    let (mut sum, mut count) = (0.0, 0u32);
    for y in (y0 * h) as u32..(y1 * h) as u32 {
        for x in (x0 * w) as u32..(x1 * w) as u32 {
            sum += image.pixel(x, y)[1] as f64;
            count += 1;
        }
    }
    sum / count as f64
}

/// On an HDR output, a window that is brighter than the reference white is
/// shown that bright on its monitor, and as a light it gives the room more.
///
/// How much more depends on the project. Its area lights pass on all of the
/// light: with the screens turned down to a tenth (the surfaces of the room
/// stay dark), the room gets as many times the light as the screen gives.
/// As the project is, the bright parts of its surfaces are bent over
/// towards 1.0 (a curve inside the renderer), and the room gets less.
fn room_shows_hdr_brighter_and_it_lights_the_room_more(device: Device) {
    let space = CompositeSpace::default();
    let pq = ImageDescription::BT2020_PQ;
    let encode = EncodeTransform::new(&space, &pq).unwrap();
    let white = [255u8, 255, 255, 255];
    // PQ code 200 of 255: more than six times the reference white.
    let bright = [200u8, 200, 200, 255];
    let stored = DecodeTransform::new(&pq, AlphaMode::PremultipliedElectrical, &space)
        .unwrap()
        .apply(bright.map(|v| v as f32 / 255.0));
    // Below the monitors: the desk and what stands on it.
    let below = (0.3, 0.82, 0.7, 0.97);

    // (what, the brightness of the screens)
    for (what, name, screens_at) in [
        ("as the project is", "colors-room-hdr", 1.0f32),
        ("with the screens at a tenth", "colors-room-hdr-dim", 0.1),
    ] {
        let mut session = start(
            "room",
            name,
            device,
            "960x540",
            &[
                "--output-image-description",
                "bt2020-pq",
                "--output-format",
                "rgba16",
            ],
        );
        // Only the main monitor lights the room, the picture is taken with
        // a fixed exposure, and nothing spreads the light in the picture.
        // (Every change of a setting makes the project set itself up again,
        // which costs it memory: no more of them than needed.)
        room_set(&mut session, "bloom", 0.0);
        room_set(&mut session, "adapt", 0.0);
        room_set(&mut session, "no_signal_gain", 0.0);
        if screens_at != 1.0 {
            room_set(&mut session, "screen_brightness", screens_at as f64);
        }
        let mut client = room_open(&mut session, options("a", 100, 60, white));
        room_settle(&mut session);
        let middle = room_point(
            &mut session,
            MAIN_SIZE.0 as f64 / 2.0,
            MAIN_SIZE.1 as f64 / 2.0,
        );
        let at = |c: [f32; 4]| {
            [
                c[0] * screens_at,
                c[1] * screens_at,
                c[2] * screens_at,
                c[3],
            ]
        };

        let (_, composite) = session.screenshot("sdr-white");
        let sdr_screen = composite.pixel(middle.0, middle.1);
        let sdr_room = mean_green(&composite, below);
        assert!(
            close(sdr_screen, at([1.0, 1.0, 1.0, 1.0]), 5e-3),
            "{sdr_screen:?}"
        );
        assert!(sdr_room > 0.005, "the room is dark: {sdr_room}");

        set_meaning(&mut session, "a", Some("bt2020-pq"));
        session.redraw(&mut client, "a", |o| o.color = bright);
        session.screenshot_png("hdr");
        let (encoded, composite) = session.screenshot("hdr");
        let hdr_screen = composite.pixel(middle.0, middle.1);
        let hdr_room = mean_green(&composite, below);
        let expected = at(stored);
        assert!(
            close(hdr_screen, expected, 5e-3),
            "the screen holds {hdr_screen:?}, expected {expected:?}"
        );
        // On the output: that light as PQ (the code the client stored, when
        // the screens are not turned down).
        let code = encode.apply([expected[0], expected[1], expected[2]]);
        let pixel = encoded.pixel(middle.0, middle.1);
        for c in 0..3 {
            assert!(
                (pixel[c] as f32 - code[c] * 65535.0).abs() <= 400.0,
                "the screen is {pixel:?} on the output, expected {code:?}"
            );
        }
        let (screens, rooms) = (
            hdr_screen[1] as f64 / sdr_screen[1] as f64,
            hdr_room / sdr_room,
        );
        println!(
            "room, PQ output, {what}: the screen is {:.3} with an SDR white and {:.3} with PQ \
             code 200 ({screens:.3} times); the room below it is {sdr_room:.4} and \
             {hdr_room:.4} ({rooms:.3} times); on the output the screen is {:?} (stored: {})",
            sdr_screen[1],
            hdr_screen[1],
            &pixel[..3],
            bright[0] as u32 * 257
        );
        assert!(screens > 3.0, "{screens}");
        assert!(rooms > 1.5, "{what}: the room got {rooms} times the light");
        if screens_at != 1.0 {
            assert!(
                (rooms / screens - 1.0).abs() < 0.05,
                "{what}: the room got {rooms} times the light, the screen {screens} times"
            );
        }
        session.assert_script_healthy();
        drop(client);
        session.quit();
    }
}

/// In the view of the whole room, the project as it is puts a lens over the
/// picture (light of bright parts spreads, the corners are darker): what a
/// monitor shows there is not what its window stored. How far off, for a
/// white, a grey and a dark window, and that without the lens it is not.
fn room_from_the_chair_shows_a_window_through_a_lens(device: Device) {
    let space = CompositeSpace::default();
    let stored = |color: [u8; 4]| {
        DecodeTransform::new(
            &ImageDescription::SRGB_GAMMA22,
            AlphaMode::PremultipliedElectrical,
            &space,
        )
        .unwrap()
        .apply(color.map(|v| v as f32 / 255.0))
    };
    for lens in [true, false] {
        let name = format!("colors-room-chair-{}", if lens { "lens" } else { "plain" });
        let mut session = start("room", &name, device, "960x540", &[]);
        room_set(&mut session, "parallax", 0.0);
        if !lens {
            room_set(&mut session, "bloom", 0.0);
            room_set(&mut session, "vignette", 0.0);
        }
        let mut client = room_open(&mut session, options("a", 100, 60, [255, 255, 255, 255]));
        room_settle(&mut session);
        let middle = room_point(
            &mut session,
            MAIN_SIZE.0 as f64 / 2.0,
            MAIN_SIZE.1 as f64 / 2.0,
        );
        for color in [
            [255u8, 255, 255, 255],
            [128, 128, 128, 255],
            [16, 16, 16, 255],
        ] {
            session.redraw(&mut client, "a", |o| o.color = color);
            session.screenshot_png("drawn");
            let (encoded, composite) = session.screenshot("chair");
            let (held, wanted) = (composite.pixel(middle.0, middle.1), stored(color));
            let pixel = encoded.pixel(middle.0, middle.1);
            println!(
                "room from the chair, {}: a window of {} is {:.4} in the working space \
                 (stored: {:.4}) and {} on the output",
                if lens { "as it is" } else { "without the lens" },
                color[0],
                held[1],
                wanted[1],
                pixel[1]
            );
            if !lens {
                assert!(close(held, wanted, 5e-3), "{held:?} {wanted:?}");
                assert!((pixel[1] as i64 - color[1] as i64).abs() <= 1, "{pixel:?}");
            }
        }
        session.assert_script_healthy();
        drop(client);
        session.quit();
    }
}
