//! An output shown through an OCIO display and view, end to end: the real
//! compositor, a client with a known colour, and the pixels of a capture
//! against the same conversion on the CPU (the way in with `proto-color`,
//! the way out with OCIO's CPU processor).

mod common;

use std::path::PathBuf;

use common::{Device, Session, options, window};
use proto_color::{
    AlphaMode, Chromaticity, CompositeSpace, DecodeTransform, EncodeTransform, ImageDescription,
    Primaries,
};
use proto_compositor::control::Request;
use proto_ocio::{Config, DisplayRequest, Dynamic};
use serde_json::Value;

device_tests!(
    an_output_is_shown_through_its_view_and_switches_without_stopping,
    without_slangc_the_output_is_converted_without_ocio,
    a_script_converts_its_own_image_and_chooses_the_view,
    a_pq_output_keeps_the_reference_white,
    a_nested_compositor_is_shown_through_its_view,
    an_output_follows_its_display,
);

const CONFIG: &str = "ocio://studio-config-latest";
const DISPLAY: &str = "sRGB - Display";
const ACES: &str = "ACES 2.0 - SDR 100 nits (Rec.709)";
const PLAIN: &str = "Un-tone-mapped";
/// The primaries a session works in when nothing says otherwise.
const SCRIPT_WORKING: Primaries = proto_compositor::script::DEFAULT_WORKING;
const COLOR: [u8; 4] = [230, 120, 40, 255];

fn cache_dir(name: &str, device: Device) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ocio-output-{name}-{}-{}",
        device.name(),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn ocio_state(session: &mut Session) -> Value {
    session.send(Request::OcioState)["result"].clone()
}

fn set(session: &mut Session, change: impl FnOnce(&mut Request)) -> Value {
    let mut request = Request::OcioSet {
        output: None,
        display: None,
        view: None,
        exposure: None,
        gamma: None,
        clear: false,
    };
    change(&mut request);
    session.send(request)["result"].clone()
}

const OUTPUT: &str = "HEADLESS-1";

/// The entry of the output in the OCIO state: there once the output was
/// presented with a display and view.
fn output(state: &Value) -> &Value {
    state["outputs"]
        .as_array()
        .and_then(|outputs| outputs.iter().find(|o| o["output"] == OUTPUT))
        .unwrap_or(&Value::Null)
}

/// The status of the output; of the setting for every output while the
/// output is not shown through OCIO.
fn status(state: &Value) -> &str {
    output(state)["status"]
        .as_str()
        .or(state["default"]["status"].as_str())
        .unwrap_or("")
}

/// Asks for the state until the output has the status `wanted`. Every
/// request draws a frame, so this also lets the frames go on.
fn wait_for(session: &mut Session, wanted: &str) -> Value {
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    loop {
        let state = ocio_state(session);
        if status(&state) == wanted {
            return state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the output did not become {wanted}: {state:#}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The client's colour as the built-in drawing has it in its buffer.
fn in_script_working(color: [u8; 4]) -> [f32; 4] {
    DecodeTransform::new(
        &proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION,
        AlphaMode::PremultipliedElectrical,
        &CompositeSpace {
            primaries: SCRIPT_WORKING,
        },
    )
    .unwrap()
    .apply(color.map(|v| f32::from(v) / 255.0))
}

/// What the capture should hold for a client colour: the way in on the CPU,
/// the matrix to the config's working colour space, the view on OCIO's CPU.
fn through_view(
    state: &Value,
    view: &str,
    controls: Option<(f64, f64)>,
    color: [u8; 4],
) -> [f64; 3] {
    through(state, DISPLAY, view, controls, 1.0, color).map(|v| v * 255.0)
}

/// Like [`through_view`], for any display, with the scale in front of the
/// conversion, as values from 0 to 1.
fn through(
    state: &Value,
    display: &str,
    view: &str,
    controls: Option<(f64, f64)>,
    scale: f32,
    color: [u8; 4],
) -> [f64; 3] {
    let c: Vec<f64> = state["working_chromaticities"]
        .as_array()
        .expect("the working primaries")
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let at = |i: usize| Chromaticity::new(c[i], c[i + 1]);
    let config_working = Primaries {
        red: at(0),
        green: at(2),
        blue: at(4),
        white: at(6),
    };
    let linear = in_script_working(color);
    let matrix = SCRIPT_WORKING
        .conversion_to(&config_working)
        .unwrap()
        .to_f32();
    let rgb: [f32; 3] = std::array::from_fn(|r| {
        (matrix[r][0] * linear[0] + matrix[r][1] * linear[1] + matrix[r][2] * linear[2]) * scale
    });
    let config = Config::from_file(CONFIG).unwrap();
    let mut request = DisplayRequest::new(state["working"].as_str().unwrap(), display, view);
    request.dynamic_exposure = controls.is_some();
    request.dynamic_gamma = controls.is_some();
    let mut cpu = config
        .display_processor(&request)
        .unwrap()
        .cpu(true)
        .unwrap();
    if let Some((exposure, gamma)) = controls {
        cpu.set_dynamic(Dynamic::Exposure, exposure).unwrap();
        cpu.set_dynamic(Dynamic::Gamma, gamma).unwrap();
    }
    let out = cpu.apply([rgb[0], rgb[1], rgb[2], 1.0]).unwrap();
    std::array::from_fn(|i| f64::from(out[i]).clamp(0.0, 1.0))
}

/// What the capture holds without OCIO.
fn without_ocio(color: [u8; 4], output: &ImageDescription) -> [f64; 3] {
    let linear = in_script_working(color);
    let out = EncodeTransform::new(
        &CompositeSpace {
            primaries: SCRIPT_WORKING,
        },
        output,
    )
    .unwrap()
    .apply([linear[0], linear[1], linear[2]]);
    std::array::from_fn(|i| f64::from(out[i]).clamp(0.0, 1.0) * 255.0)
}

/// The pixel in the middle of the window, from a new capture.
fn window_pixel(session: &mut Session, name: &str, title: &str) -> ([u16; 4], ImageDescription) {
    let state = session.state();
    let w = window(&state, title).expect("the window");
    // Where it is drawn; where it was placed when nothing moved it.
    let position = if w["visual"]["position"].is_array() {
        &w["visual"]["position"]
    } else {
        &w["position"]
    };
    let at = |i: usize| {
        position[i]
            .as_f64()
            .unwrap_or_else(|| panic!("no position of the window: {w:#}"))
    };
    let (x, y) = (at(0) + 32.0, at(1) + 24.0);
    // What the output is driven as, by the output itself (a capture names
    // what its target was made for): the same before and after the capture,
    // so that it is what the captured frame was drawn for.
    let driven_as = |session: &mut Session| {
        let state = session.state();
        let name = state["outputs"][0]["image_description"]["name"]
            .as_str()
            .expect("a preset image description")
            .to_string();
        ImageDescription::from_name(&name).unwrap()
    };
    loop {
        let before = driven_as(session);
        let image = session.screenshot_png(name);
        if driven_as(session) == before {
            return (image.pixel(x as u32, y as u32), before);
        }
    }
}

fn assert_close(what: &str, pixel: [u16; 4], expected: [f64; 3]) {
    println!("{what}: {pixel:?}, expected {expected:?}");
    for c in 0..3 {
        assert!(
            (f64::from(pixel[c]) - expected[c]).abs() <= 1.01,
            "{what}: {pixel:?}, expected {expected:?}"
        );
    }
}

fn an_output_is_shown_through_its_view_and_switches_without_stopping(device: Device) {
    proto_ocio::quiet();
    let cache = cache_dir("view", device);
    let cache_text = cache.to_str().unwrap().to_string();
    let mut session = Session::start(
        "ocio-output-view",
        device,
        &[
            "--ocio-config",
            CONFIG,
            "--display",
            DISPLAY,
            "--view",
            ACES,
            "--ocio-cache",
            &cache_text,
            "--ocio-compile-delay-ms",
            "300",
        ],
    );
    let title = "ocio";
    let (mut client, _) = session.mapped_client(options(title, 64, 48, COLOR));

    // Until the first conversion is compiled, the output is converted
    // without OCIO.
    let state = wait_for(&mut session, "compiling");
    println!("{state:#}");
    assert_eq!(state["working"], "ACEScg");
    assert_eq!(output(&state)["view"], ACES);
    let (pixel, description) = window_pixel(&mut session, "before", title);
    if status(&ocio_state(&mut session)) == "compiling" {
        assert_close(
            "while the first view is compiled",
            pixel,
            without_ocio(COLOR, &description),
        );
    }

    let state = wait_for(&mut session, "ready");
    assert_eq!(state["compiled"], 1);
    assert_eq!(state["from_cache"], 0);
    let aces = through_view(&state, ACES, None, COLOR);
    let (pixel, _) = window_pixel(&mut session, "aces", title);
    assert_close("through the ACES view", pixel, aces);
    // The view does something: the colour is not what it is without it.
    let plain = through_view(&state, PLAIN, None, COLOR);
    assert!(
        (0..3).any(|c| (aces[c] - plain[c]).abs() > 4.0),
        "{aces:?} vs {plain:?}"
    );

    // Another view: compiled behind the frames, which go on through the
    // previous one until it is ready.
    let frames_before = session.state()["frames_rendered"].as_u64().unwrap();
    set(&mut session, |r| {
        if let Request::OcioSet { view, .. } = r {
            *view = Some(PLAIN.into());
        }
    });
    wait_for(&mut session, "compiling");
    let (pixel, _) = window_pixel(&mut session, "switching", title);
    let state = ocio_state(&mut session);
    if status(&state) == "compiling" {
        assert_close("while the next view is compiled", pixel, aces);
        let frames = session.state()["frames_rendered"].as_u64().unwrap();
        assert!(frames > frames_before, "no frame while compiling");
    }
    let state = wait_for(&mut session, "ready");
    assert_eq!(state["compiled"], 2);
    assert!(
        state["draws_while_pending"].as_u64().unwrap() > 0,
        "{state:#}"
    );
    let (pixel, _) = window_pixel(&mut session, "plain", title);
    assert_close("without tone mapping", pixel, plain);

    // Back to the first view: nothing is compiled, it is still there.
    set(&mut session, |r| {
        if let Request::OcioSet { view, .. } = r {
            *view = Some(ACES.into());
        }
    });
    let (pixel, _) = window_pixel(&mut session, "aces-again", title);
    let state = ocio_state(&mut session);
    assert_eq!(status(&state), "ready");
    assert_eq!(state["compiled"], 2);
    assert_close("through the ACES view again", pixel, aces);

    // A reloaded script draws through the same conversions: they are the
    // core's, and nothing is compiled again.
    session.send(Request::ScriptReload { path: None });
    let (pixel, _) = window_pixel(&mut session, "reloaded", title);
    let state = ocio_state(&mut session);
    assert_eq!(status(&state), "ready", "{state:#}");
    assert_eq!(state["compiled"], 2);
    assert_close("after reloading the script", pixel, aces);

    // Exposure and gamma: the first use compiles the view with its
    // controls; after that, a new value is drawn at once.
    set(&mut session, |r| {
        if let Request::OcioSet { exposure, .. } = r {
            *exposure = Some(1.0);
        }
    });
    let state = wait_for(&mut session, "ready");
    assert_eq!(state["compiled"], 3);
    let (pixel, _) = window_pixel(&mut session, "exposure-up", title);
    assert_close(
        "one stop up",
        pixel,
        through_view(&state, ACES, Some((1.0, 1.0)), COLOR),
    );
    set(&mut session, |r| {
        if let Request::OcioSet {
            exposure, gamma, ..
        } = r
        {
            *exposure = Some(-1.5);
            *gamma = Some(1.2);
        }
    });
    let (pixel, _) = window_pixel(&mut session, "exposure-down", title);
    let state = ocio_state(&mut session);
    assert_eq!(status(&state), "ready");
    assert_eq!(state["compiled"], 3, "a changed control compiled something");
    assert_close(
        "one and a half stops down, gamma 1.2",
        pixel,
        through_view(&state, ACES, Some((-1.5, 1.2)), COLOR),
    );

    // New contents go through the view like the first ones.
    let other = [20, 200, 90, 255];
    session.recolor(&mut client, title, other);
    let (pixel, _) = window_pixel(&mut session, "recolored", title);
    assert_close(
        "another colour",
        pixel,
        through_view(&state, ACES, Some((-1.5, 1.2)), other),
    );

    // Without a display and view, the output is converted as before.
    set(&mut session, |r| {
        if let Request::OcioSet { clear, .. } = r {
            *clear = true;
        }
    });
    let (pixel, description) = window_pixel(&mut session, "cleared", title);
    assert_eq!(status(&ocio_state(&mut session)), "off");
    assert_close("without OCIO", pixel, without_ocio(other, &description));

    // The output followed its display (the piecewise sRGB curve) while it
    // had one, and is back to what it was: nothing disagreed.
    assert_eq!(description, ImageDescription::SRGB_GAMMA22);
    let log = std::fs::read_to_string(session.output_dir.join("proto-compositor.log")).unwrap();
    assert!(!log.contains("applications are told"), "{log}");

    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();

    // The next session finds the compiled shaders on disk. This one is told
    // what its output is: that stays, and that the display's signal is
    // another (the piecewise sRGB curve, not gamma 2.2) is said, once.
    let mut session = Session::start(
        "ocio-output-cached",
        device,
        &[
            "--ocio-config",
            CONFIG,
            "--display",
            DISPLAY,
            "--view",
            ACES,
            "--ocio-cache",
            &cache_text,
            "--output-image-description",
            "srgb-gamma22",
        ],
    );
    let (_client, _) = session.mapped_client(options(title, 64, 48, COLOR));
    let state = wait_for(&mut session, "ready");
    assert_eq!(state["compiled"], 0, "{state:#}");
    assert_eq!(state["from_cache"], 1);
    let (pixel, description) = window_pixel(&mut session, "aces", title);
    assert_close("through the cached ACES view", pixel, aces);
    assert_eq!(description, ImageDescription::SRGB_GAMMA22);
    window_pixel(&mut session, "aces-again", title);
    let log = std::fs::read_to_string(session.output_dir.join("proto-compositor.log")).unwrap();
    assert_eq!(
        log.matches("applications are told the output is srgb-gamma22")
            .count(),
        1,
        "{log}"
    );
    assert!(log.contains("the srgb-piecewise transfer function"));
    session.quit();
    let _ = std::fs::remove_dir_all(&cache);
}

fn without_slangc_the_output_is_converted_without_ocio(device: Device) {
    let cache = cache_dir("no-slangc", device);
    let cache_text = cache.to_str().unwrap().to_string();
    let mut session = Session::start(
        "ocio-output-no-slangc",
        device,
        &[
            "--ocio-config",
            CONFIG,
            "--view",
            ACES,
            "--ocio-cache",
            &cache_text,
            "--ocio-slangc",
            "/nonexistent/slangc",
        ],
    );
    let title = "ocio";
    let (_client, _) = session.mapped_client(options(title, 64, 48, COLOR));
    let state = wait_for(&mut session, "failed");
    println!("{state:#}");
    assert!(state["slangc"].is_null());
    assert!(output(&state)["error"].as_str().unwrap().contains("slangc"));
    let (pixel, description) = window_pixel(&mut session, "fallback", title);
    assert_close("without slangc", pixel, without_ocio(COLOR, &description));
    session.assert_script_healthy();
    session.assert_drawing_clean();
    let log = std::fs::read_to_string(session.output_dir.join("proto-compositor.log")).unwrap();
    assert!(log.contains("slangc"), "{log}");
    session.quit();
    let _ = std::fs::remove_dir_all(&cache);
}

/// An output driven with PQ, shown through OCIO's PQ display without tone
/// mapping: the reference white of a window (1.0 in the working space) lands
/// on the output's reference white (203 cd/m²), as it does without OCIO.
/// OCIO's display takes 1.0 as 100 cd/m²; the scale in front makes up for it.
fn a_pq_output_keeps_the_reference_white(device: Device) {
    proto_ocio::quiet();
    let cache = cache_dir("pq", device);
    let mut session = Session::start(
        "ocio-output-pq",
        device,
        &[
            "--output-image-description",
            "bt2020-pq",
            "--output-format",
            "rgba16",
            "--ocio-config",
            CONFIG,
            "--display",
            "Rec.2100-PQ - Display",
            "--view",
            PLAIN,
            "--ocio-cache",
            cache.to_str().unwrap(),
        ],
    );
    let title = "ocio";
    let white = [255, 255, 255, 255];
    let (mut client, _) = session.mapped_client(options(title, 64, 48, white));
    let state = wait_for(&mut session, "ready");
    for color in [white, COLOR, [20, 200, 90, 255]] {
        session.recolor(&mut client, title, color);
        let (pixel, description) = window_pixel(&mut session, "pq", title);
        assert_eq!(description, ImageDescription::BT2020_PQ);
        let pixel: [f64; 3] = std::array::from_fn(|c| f64::from(pixel[c]) / 65535.0);
        let scale = description.luminance.reference / 100.0;
        let ocio = through(&state, "Rec.2100-PQ - Display", PLAIN, None, scale, color);
        let plain = without_ocio(color, &description).map(|v| v / 255.0);
        println!("{color:?}: {pixel:?}, through OCIO {ocio:?}, without OCIO {plain:?}");
        for c in 0..3 {
            // What was drawn is what OCIO's CPU gives ...
            assert!((pixel[c] - ocio[c]).abs() <= 2e-4, "{pixel:?} vs {ocio:?}");
            // ... and what the output shows without OCIO, to a 10-bit code.
            assert!(
                (pixel[c] - plain[c]).abs() <= 1.0 / 1023.0,
                "{pixel:?} vs {plain:?}"
            );
        }
    }
    // White sits at the PQ code of 203 cd/m², not at that of 100.
    let (pixel, _) = {
        session.recolor(&mut client, title, white);
        window_pixel(&mut session, "pq-white", title)
    };
    let code = f64::from(pixel[1]) / 65535.0;
    assert!((code - 0.5807).abs() < 2e-3, "white is at PQ {code}");
    // The display's signal is what the output is driven with: BT.2020 and PQ.
    let state = ocio_state(&mut session);
    assert!(
        output(&state)["signal"]
            .as_str()
            .unwrap()
            .starts_with("bt2020 primaries, the pq transfer function"),
        "{state:#}"
    );
    let log = std::fs::read_to_string(session.output_dir.join("proto-compositor.log")).unwrap();
    assert!(!log.contains("applications are told"), "{log}");
    session.assert_script_healthy();
    session.quit();
    let _ = std::fs::remove_dir_all(&cache);
}

/// A compositor that runs as a window of another one (the nested backend),
/// with a view: what it hands to its parent went through the view. The
/// parent is a headless compositor of the test's own.
fn a_nested_compositor_is_shown_through_its_view(device: Device) {
    proto_ocio::quiet();
    let mut parent = Session::start("ocio-output-nested-parent", device, &[]);
    let mut nested = Session::start_nested(
        "ocio-output-nested-child",
        device,
        &parent,
        &[
            "--output-size",
            "400x300",
            "--cursor",
            "off",
            "--ocio-config",
            CONFIG,
            "--display",
            DISPLAY,
            "--view",
            ACES,
        ],
    );
    let title = "ocio";
    let (_client, _) = nested.mapped_client(options(title, 64, 48, COLOR));
    let state = wait_for(&mut nested, "ready");
    println!("{:#}", state["outputs"]);
    let expected = through_view(&state, ACES, None, COLOR);
    let (pixel, _) = window_pixel(&mut nested, "nested", title);
    assert_close("in the nested compositor", pixel, expected);

    // The same pixels reach the parent: its one window is the nested
    // compositor, and both outputs are driven alike, so the parent shows
    // what it was handed.
    let inner = nested.state();
    let inner = window(&inner, title).unwrap();
    let inner_position = if inner["visual"]["position"].is_array() {
        &inner["visual"]["position"]
    } else {
        &inner["position"]
    };
    let state = parent.wait(&mut [], "the nested compositor's window", |state| {
        state["windows"]
            .as_array()
            .is_some_and(|w| w.len() == 1 && w[0]["surfaces"][0]["buffer_size"].is_array())
    });
    let outer = &state["windows"][0];
    let outer_position = if outer["visual"]["position"].is_array() {
        &outer["visual"]["position"]
    } else {
        &outer["position"]
    };
    let at = |i: usize| {
        (outer_position[i].as_f64().unwrap() + inner_position[i].as_f64().unwrap()) as u32
    };
    // The parent draws the frame the nested compositor last handed over.
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    loop {
        let image = parent.screenshot_png("parent");
        let seen = image.pixel(at(0) + 32, at(1) + 24);
        let close = (0..3).all(|c| (f64::from(seen[c]) - expected[c]).abs() <= 2.01);
        if close {
            println!("in the parent: {seen:?}, expected {expected:?}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the parent shows {seen:?}, expected {expected:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The nested backend cannot be driven as another image description
    // says: the output stays as it was described, and both are said, once.
    let log = std::fs::read_to_string(nested.output_dir.join("proto-compositor.log")).unwrap();
    assert_eq!(log.matches("stays as it is").count(), 1, "{log}");
    assert_eq!(
        log.matches("applications are told the output is srgb-gamma22")
            .count(),
        1,
        "{log}"
    );
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// An output that is given an OCIO display is driven, and described to
/// applications, as that display's signal, when nothing else said what the
/// output is.
fn an_output_follows_its_display(device: Device) {
    use proto_test_client::{ClientEvent, ClientOptions};

    // The numbers of color-management-v1.
    const TF_GAMMA22: u32 = 2;
    // The piecewise sRGB function (`compound_power_2_4`).
    const TF_SRGB: u32 = 14;
    const TF_PQ: u32 = 11;
    const PRIMARIES_SRGB: u32 = 1;
    const PRIMARIES_BT2020: u32 = 6;

    proto_ocio::quiet();
    let cache = cache_dir("follow", device);
    let mut session = Session::start(
        "ocio-output-follow",
        device,
        &[
            "--ocio-config",
            CONFIG,
            "--ocio-cache",
            cache.to_str().unwrap(),
        ],
    );
    let title = "ocio";
    let (mut client, _) = session.mapped_client(ClientOptions {
        color_manager_version: Some(3),
        ..options(title, 64, 48, COLOR)
    });
    let color_output = client.create_color_output(OUTPUT).unwrap();
    // What the client is told the output is: the named transfer function and
    // primaries of its image description.
    let told = |session: &mut Session, client: &mut proto_test_client::TestClient| {
        let index = client.get_output_image_description(color_output).unwrap();
        session.wait(&mut [&mut *client], "the description", |_| true);
        client.get_image_information(index).unwrap();
        let info = client
            .wait_for(
                common::TIMEOUT,
                |e| matches!(e, ClientEvent::ImageDescriptionInfo { index: i, .. } if *i == index),
            )
            .unwrap()
            .expect("the information of the output's image description");
        match info {
            ClientEvent::ImageDescriptionInfo { info, .. } => {
                (info.tf_named, info.primaries_named, info.luminances)
            }
            _ => unreachable!(),
        }
    };
    let changed = |client: &mut proto_test_client::TestClient| {
        client
            .wait_for(common::TIMEOUT, |e| {
                matches!(e, ClientEvent::OutputImageDescriptionChanged { output } if output == OUTPUT)
            })
            .unwrap()
            .is_some()
    };
    let choose = |session: &mut Session, display: &str| {
        set(session, |r| {
            if let Request::OcioSet {
                display: d, view, ..
            } = r
            {
                *d = Some(display.into());
                *view = Some(PLAIN.into());
            }
        });
    };

    // Without OCIO: gamma 2.2, and a window without colour information
    // (taken as gamma 2.2 too) comes out as it went in.
    let (pixel, description) = window_pixel(&mut session, "before", title);
    assert_eq!(description, ImageDescription::SRGB_GAMMA22);
    assert_close("without OCIO", pixel, [230.0, 120.0, 40.0]);
    assert_eq!(
        told(&mut session, &mut client),
        (Some(TF_GAMMA22), Some(PRIMARIES_SRGB), Some([2000, 80, 80]))
    );

    // The sRGB display: the output becomes piecewise sRGB, the client is
    // told, and the conversion without OCIO (while the view is compiled, or
    // when it fails) gives what the view without tone mapping gives.
    choose(&mut session, DISPLAY);
    let state = wait_for(&mut session, "ready");
    assert!(changed(&mut client), "the client was not told");
    let (pixel, description) = window_pixel(&mut session, "srgb", title);
    assert_eq!(description, ImageDescription::SRGB_PIECEWISE);
    let through = through_view(&state, PLAIN, None, COLOR);
    assert_close("through the sRGB display", pixel, through);
    let plain = without_ocio(COLOR, &description);
    println!("sRGB display: through OCIO {through:?}, without OCIO {plain:?}");
    for c in 0..3 {
        assert!(
            (through[c] - plain[c]).abs() < 0.6,
            "{through:?} vs {plain:?}"
        );
    }
    assert_eq!(
        told(&mut session, &mut client),
        (Some(TF_SRGB), Some(PRIMARIES_SRGB), Some([2000, 80, 80]))
    );

    // A PQ display: the output becomes BT.2020 PQ, whose reference white is
    // 203 cd/m², and the white of a window lands there (PQ 0.5807), not at
    // the 100 cd/m² that OCIO's 1.0 stands for (PQ 0.5081).
    session.recolor(&mut client, title, [255, 255, 255, 255]);
    choose(&mut session, "Rec.2100-PQ - Display");
    wait_for(&mut session, "ready");
    assert!(changed(&mut client), "the client was not told");
    let (pixel, description) = window_pixel(&mut session, "pq", title);
    assert_eq!(description, ImageDescription::BT2020_PQ);
    assert_close("white on the PQ display", pixel, [0.5807 * 255.0; 3]);
    assert_eq!(
        told(&mut session, &mut client),
        (Some(TF_PQ), Some(PRIMARIES_BT2020), Some([50, 10000, 203]))
    );

    // A display whose signal cannot be told (HLG), and no display at all:
    // the output is what it was before.
    choose(&mut session, "Rec.2100-HLG - Display");
    wait_for(&mut session, "ready");
    let (_, description) = window_pixel(&mut session, "hlg", title);
    let (_, description_again) = window_pixel(&mut session, "hlg-again", title);
    assert_eq!(
        description_again,
        ImageDescription::SRGB_GAMMA22,
        "{description:?}"
    );
    choose(&mut session, "Rec.2100-PQ - Display");
    wait_for(&mut session, "ready");
    let (_, description) = window_pixel(&mut session, "pq-again", title);
    assert_eq!(description, ImageDescription::BT2020_PQ);
    set(&mut session, |r| {
        if let Request::OcioSet { clear, .. } = r {
            *clear = true;
        }
    });
    window_pixel(&mut session, "cleared", title);
    let (pixel, description) = window_pixel(&mut session, "cleared-again", title);
    assert_eq!(description, ImageDescription::SRGB_GAMMA22);
    assert_close("without OCIO again", pixel, [255.0; 3]);

    let log = std::fs::read_to_string(session.output_dir.join("proto-compositor.log")).unwrap();
    assert!(!log.contains("applications are told"), "{log}");
    session.assert_script_healthy();
    session.quit();
    let _ = std::fs::remove_dir_all(&cache);
}

/// A project that fills an image with one scene-linear colour, converts it
/// with `hz.ocio_apply` into another image, and offers that for capture.
const APPLY_MAIN: &str = r#"
local source, source_slot, result
local request = { display = "sRGB - Display", view = "ACES 2.0 - SDR 100 nits (Rec.709)" }
local last = {}

function set_request(new)
    request = new
    last = {}
end

function last_apply()
    return { ok = last.ok, problem = last.problem }
end

function info()
    return hz.ocio_info()
end

function choose(output, display, view)
    local ok, problem = hz.ocio_set_output(output, display, view)
    return { ok = ok, problem = problem }
end

function controls(output, values)
    local ok, problem = hz.ocio_set_controls(output, values)
    return { ok = ok, problem = problem }
end

function frame(output, target)
    if not source then
        local function image()
            return gfx.create_image({
                format = "rgba32float",
                width = output.width,
                height = output.height,
                usage = { "color_target", "sampled", "copy_src" },
            })
        end
        source, result = image(), image()
        source_slot = gfx.create_texture_slot(source)
    end
    -- Premultiplied: half transparent, the colour (4, 1, 0.25) under it.
    gfx.begin_pass({ color = { { image = source, load = "clear", clear = { 2.0, 0.5, 0.125, 0.5 } } } })
    gfx.end_pass()
    gfx.begin_pass({ color = { { image = result, load = "clear", clear = { 0, 0, 0, 0 } } } })
    gfx.end_pass()
    local args = { input = source_slot, target = result }
    for key, value in request do
        args[key] = value
    end
    last.ok, last.problem = hz.ocio_apply(args)
    hz.debug_composite(result)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    gfx.end_pass()
end
"#;

fn a_script_converts_its_own_image_and_chooses_the_view(device: Device) {
    use serde_json::json;

    proto_ocio::quiet();
    let cache = cache_dir("apply", device);
    let project = common::script_project("ocio-apply", device, &[("main", APPLY_MAIN)]);
    let mut session = Session::start(
        "ocio-output-apply",
        device,
        &[
            "--output-size",
            "64x64",
            "--ocio-config",
            CONFIG,
            "--ocio-cache",
            cache.to_str().unwrap(),
            "--script",
            project.to_str().unwrap(),
        ],
    );
    assert_eq!(session.script_state()["mode"], "running");

    let info = session.script("info", json!([]));
    assert_eq!(info["working"], "ACEScg");
    assert_eq!(info["default_display"], DISPLAY);
    assert_eq!(info["working_chromaticities"].as_array().unwrap().len(), 8);
    let displays = info["displays"].as_array().unwrap();
    let srgb = displays.iter().find(|d| d["name"] == DISPLAY).unwrap();
    assert!(srgb["views"].as_array().unwrap().iter().any(|v| v == ACES));
    assert!(
        info["color_spaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "ACEScct")
    );

    // Asks until the conversion was drawn (false while it is compiled).
    let applied = |session: &mut Session| -> Value {
        let deadline = std::time::Instant::now() + common::TIMEOUT;
        loop {
            let last = session.script("last_apply", json!([]));
            if last["ok"] == true || last["problem"].is_string() {
                return last;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never applied: {last}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let config = Config::from_file(CONFIG).unwrap();
    let source = [4.0f32, 1.0, 0.25];
    let check = |session: &mut Session, name: &str, expected: [f32; 4]| {
        let (_, composite) = session.screenshot(name);
        let pixel = composite.pixel(20, 30);
        println!("{name}: {pixel:?}, expected {expected:?}");
        for c in 0..4 {
            // The capture holds half floats.
            assert!(
                (pixel[c] - expected[c]).abs() <= 2e-3 * expected[c].abs().max(1.0),
                "{name}: {pixel:?}, expected {expected:?}"
            );
        }
    };

    // To a display and view. Not premultiplied: the stored values go in.
    let last = applied(&mut session);
    assert_eq!(last["ok"], true, "{last}");
    let cpu = config
        .display_processor(&DisplayRequest::new("ACEScg", DISPLAY, ACES))
        .unwrap()
        .cpu(true)
        .unwrap();
    check(
        &mut session,
        "view",
        cpu.apply([2.0, 0.5, 0.125, 0.5]).unwrap(),
    );

    // Premultiplied, scaled, with an exposure: the colour under the alpha
    // is converted, and the result is premultiplied again.
    session.script(
        "set_request",
        json!([{
            "display": DISPLAY, "view": ACES,
            "premultiplied": true, "scale": 0.5, "exposure": 1.0, "gamma": 1.0,
        }]),
    );
    let last = applied(&mut session);
    assert_eq!(last["ok"], true, "{last}");
    let mut request = DisplayRequest::new("ACEScg", DISPLAY, ACES);
    request.dynamic_exposure = true;
    request.dynamic_gamma = true;
    let mut cpu = config
        .display_processor(&request)
        .unwrap()
        .cpu(true)
        .unwrap();
    cpu.set_dynamic(Dynamic::Exposure, 1.0).unwrap();
    let straight = cpu
        .apply([source[0] * 0.5, source[1] * 0.5, source[2] * 0.5, 0.5])
        .unwrap();
    check(
        &mut session,
        "premultiplied",
        [straight[0] * 0.5, straight[1] * 0.5, straight[2] * 0.5, 0.5],
    );

    // To a colour space.
    session.script(
        "set_request",
        json!([{ "from": "ACEScg", "to": "ACEScct" }]),
    );
    let last = applied(&mut session);
    assert_eq!(last["ok"], true, "{last}");
    let cpu = config
        .processor("ACEScg", "ACEScct")
        .unwrap()
        .cpu(true)
        .unwrap();
    check(
        &mut session,
        "colorspace",
        cpu.apply([2.0, 0.5, 0.125, 0.5]).unwrap(),
    );

    // What cannot be converted is said, and the script goes on.
    session.script("set_request", json!([{ "to": "no such space" }]));
    let last = applied(&mut session);
    assert_eq!(last["ok"], false);
    println!("{}", last["problem"]);
    assert!(last["problem"].as_str().unwrap().contains("no such space"));

    // The script chooses what the output is shown through.
    let chosen = session.script("choose", json!([OUTPUT, null, PLAIN]));
    assert_eq!(chosen["ok"], true, "{chosen}");
    let state = ocio_state(&mut session);
    assert_eq!(output(&state)["view"], PLAIN, "{state:#}");
    assert_eq!(output(&state)["display"], DISPLAY);
    assert_eq!(output(&state)["own_setting"], true);
    let chosen = session.script("choose", json!([OUTPUT, null, "no such view"]));
    assert_eq!(chosen["ok"], false);
    assert!(chosen["problem"].as_str().unwrap().contains("no such view"));
    let set = session.script("controls", json!([OUTPUT, { "exposure": -2.0 }]));
    assert_eq!(set["ok"], true, "{set}");
    let state = ocio_state(&mut session);
    assert_eq!(output(&state)["exposure"], -2.0);
    assert_eq!(output(&state)["dynamic"], true);

    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
    let _ = std::fs::remove_dir_all(&cache);
}
