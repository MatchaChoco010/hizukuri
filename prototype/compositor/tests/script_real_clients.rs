//! Real applications in a session drawn (and managed) by the script layer.
//!
//! Like `tests/real_clients.rs`, a test runs only for the names listed in
//! `HIZUKURI_TEST_REAL_CLIENTS` (see `common::real_client_wanted`).
//! `script-tiles` is in the default list. `vkcube` is not: it needs a
//! hardware device (`gpu` in `HIZUKURI_TEST_DEVICES`).
//!
//! ```sh
//! HIZUKURI_TEST_DEVICES=gpu HIZUKURI_TEST_REAL_CLIENTS=vkcube \
//!     cargo test -p proto-compositor --test script_real_clients
//! ```

mod common;

use std::time::{Duration, Instant};

use common::{Device, Gpu, Session, TIMEOUT, real_client_wanted as wanted};
use proto_compositor::control::Request;
use proto_compositor::image::EncodedImage;
use serde_json::{Value, json};

fn wait_state<T>(
    session: &mut Session,
    what: &str,
    mut done: impl FnMut(&Value) -> Option<T>,
) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = session.state();
        if let Some(value) = done(&state) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for: {what}\nstate: {state:#}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A Vulkan application on the hardware GPU: its swapchain images arrive as
/// dma-bufs with a vendor modifier, are imported into the script's device
/// and drawn. Nothing is copied through the CPU, and nothing falls back.
#[test]
fn vkcube_is_shown_through_imported_dmabufs() {
    if !wanted("vkcube", &["vkcube"]) {
        return;
    }
    let device = Device::new(Gpu::Hardware);
    assert!(
        device.enabled(),
        "vkcube needs the hardware device (gpu in HIZUKURI_TEST_DEVICES)"
    );
    let mut session = Session::start(
        "real-vkcube",
        device,
        &["--output-size", "640x560", "--dmabuf", "gpu"],
    );
    let _cube = session.spawn_client("vkcube", "vkcube", &["--wsi", "wayland"]);
    let (position, size) = wait_state(&mut session, "vkcube shows a dma-buf", |state| {
        let window = state["windows"].as_array()?.first()?;
        let surface = &window["surfaces"][0];
        (surface["buffer_kind"] == "dmabuf").then(|| {
            let at = |v: &Value, i: usize| v[i].as_i64().unwrap() as u32;
            (
                (at(&window["position"], 0), at(&window["position"], 1)),
                (at(&window["size"], 0), at(&window["size"], 1)),
            )
        })
    });

    let first = session.screenshot("cube-1").0;
    // The cube turns: wait for a few more buffers, then look again.
    let before = session.script_state()["dmabuf"]["reused"].as_u64().unwrap();
    wait_state(&mut session, "more frames from vkcube", |state| {
        (state["script"]["dmabuf"]["reused"].as_u64()? >= before + 20).then_some(())
    });
    let second = session.screenshot("cube-2").0;

    // Inside the window something other than the compositor's background
    // is drawn, it is not one flat color, and it moves.
    let mut colors = std::collections::HashSet::new();
    let mut changed = 0;
    for y in (position.1..position.1 + size.1).step_by(8) {
        for x in (position.0..position.0 + size.0).step_by(8) {
            colors.insert(second.pixel(x, y));
            if first.pixel(x, y) != second.pixel(x, y) {
                changed += 1;
            }
        }
    }
    let center = second.pixel(position.0 + size.0 / 2, position.1 + size.1 / 2);
    assert_ne!(center[..3], [0x20, 0x20, 0x20], "nothing was drawn");
    assert!(colors.len() > 16, "only {} colors", colors.len());
    assert!(
        changed > 16,
        "the cube does not move ({changed} samples changed)"
    );

    let dmabuf = session.script_state()["dmabuf"].take();
    println!("vkcube: {dmabuf}");
    assert!(dmabuf["imported"].as_u64().unwrap() >= 2, "{dmabuf}");
    assert_eq!(dmabuf["fallbacks"], 0, "{dmabuf}");
    assert_eq!(dmabuf["wait_timeouts"], 0, "{dmabuf}");
    // A swapchain has a handful of images; they are imported once each.
    assert!(dmabuf["imported"].as_u64().unwrap() <= 8, "{dmabuf}");
    assert!(dmabuf["live_imports"].as_u64().unwrap() <= 4, "{dmabuf}");
    let state = session.state();
    assert_eq!(state["script"]["mode"], "running");
    assert_eq!(state["script"]["failures"], 0);
    println!("vkcube: frame time {}", state["frame_time_ms"]["total"]);
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// A window: (id, app id, x, y, width, height).
type Tile = (u64, String, i64, i64, i64, i64);

/// The windows of a session, left to right, once every one of them is
/// mapped with contents.
fn tiles(state: &Value) -> Option<Vec<Tile>> {
    let mut tiles = Vec::new();
    for w in state["windows"].as_array()? {
        if w["mapped"] != true || w["surfaces"][0]["has_contents"] != true {
            return None;
        }
        tiles.push((
            w["id"].as_u64()?,
            w["app_id"].as_str().unwrap_or_default().to_string(),
            w["position"][0].as_i64()?,
            w["position"][1].as_i64()?,
            w["size"][0].as_i64()?,
            w["size"][1].as_i64()?,
        ));
    }
    tiles.sort_by_key(|t| t.2);
    Some(tiles)
}

/// Captures the output once the applications have stopped drawing: two
/// captures a moment apart are the same.
fn settled_screenshot(session: &mut Session, name: &str) -> EncodedImage {
    let deadline = Instant::now() + TIMEOUT;
    let mut last = session.screenshot(name).0;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let next = session.screenshot(name).0;
        if next.data == last.data {
            return next;
        }
        assert!(Instant::now() < deadline, "the screen never settles");
        last = next;
    }
}

/// How many of a grid of sample points inside a rectangle differ between
/// two captures.
fn changed(a: &EncodedImage, b: &EncodedImage, rect: (i64, i64, i64, i64)) -> usize {
    let mut count = 0;
    for y in (rect.1..rect.1 + rect.3).step_by(3) {
        for x in (rect.0..rect.0 + rect.2).step_by(3) {
            if a.pixel(x as u32, y as u32) != b.pixel(x as u32, y as u32) {
                count += 1;
            }
        }
    }
    count
}

fn key(session: &mut Session, keycode: u32, pressed: bool) {
    session.send(Request::Key { keycode, pressed });
}

fn chord(session: &mut Session, modifiers: &[u32], keycode: u32) {
    for m in modifiers {
        key(session, *m, true);
    }
    key(session, keycode, true);
    key(session, keycode, false);
    for m in modifiers.iter().rev() {
        key(session, *m, false);
    }
}

const KEY_H: u32 = 35;
const KEY_L: u32 = 38;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;

/// Real terminals, placed by the tiling script and drawn by the script:
/// they are told their size and take it, the keys of the window
/// manager move the focus and swap them, typing reaches the focused one,
/// and all of them live through a reload and through the fallback.
///
/// On the hardware device the session also takes dma-bufs, so a terminal
/// that draws with GL (alacritty, when it is listed in
/// `HIZUKURI_TEST_REAL_CLIENTS`) is imported instead of copied.
fn terminals_in_tiles(device: Device) {
    let hardware = device.gpu == Gpu::Hardware;
    let alacritty = hardware && wanted("alacritty", &["alacritty"]);
    let mut args = vec!["--output-size", "1280x720"];
    if hardware {
        args.extend(["--dmabuf", "gpu"]);
    }
    let mut session = Session::start_builtin("real-tiles", device, &args);

    // A stand-in for the shell: known text, then an echo of what is typed.
    let shell = session.output_dir.join("fake-shell.sh");
    std::fs::write(
        &shell,
        "#!/bin/sh\nprintf 'hizukuri script tiles\\r\\n'\n\
         printf '\\033[31mred \\033[32mgreen \\033[34mblue \\033[0mdefault\\r\\n'\nexec cat\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let shell_arg = format!("--shell={}", shell.display());
    let mut clients = Vec::new();
    let mut count = 0;
    let mut open = |session: &mut Session, name: &str, program: &str, args: &[&str]| {
        clients.push(session.spawn_client(name, program, args));
        count += 1;
        let wanted = count;
        wait_state(session, "another tile", |s| {
            tiles(s).filter(|t| t.len() == wanted)
        })
    };
    open(&mut session, "terminal-1", "weston-terminal", &[&shell_arg]);
    let mut all = open(&mut session, "terminal-2", "weston-terminal", &[&shell_arg]);
    if alacritty {
        all = open(
            &mut session,
            "alacritty",
            "alacritty",
            &["-e", shell.to_str().unwrap()],
        );
    }
    let n = all.len() as i64;

    // Side by side with a gap of 8: each tile is this wide. A terminal
    // rounds the size it is given to whole character cells (weston-terminal
    // rounds up, alacritty down).
    let tile_width = (1280 - 8 * (n + 1)) / n;
    let settled = wait_state(&mut session, "the terminals take their tiles", |s| {
        let t = tiles(s)?;
        (t.len() as i64 == n
            && t.iter().enumerate().all(|(i, t)| {
                t.2 == 8 + i as i64 * (tile_width + 8)
                    && t.3 == 8
                    && (t.4 - tile_width).abs() < 40
                    && (t.5 - 704).abs() < 40
            }))
        .then_some(t)
    });
    println!("{}: tiles {settled:?}", device.name());
    let first = settled_screenshot(&mut session, "tiled");
    // Every tile shows a terminal, not the compositor's background.
    for t in &settled {
        assert!(
            (0..t.5).step_by(5).any(|dy| {
                (0..t.4)
                    .step_by(5)
                    .any(|dx| first.pixel((t.2 + dx) as u32, (t.3 + dy) as u32)[..3] != [0x20; 3])
            }),
            "the tile of {t:?} is empty"
        );
    }
    if alacritty {
        let dmabuf = session.script_state()["dmabuf"].take();
        println!("{}: dma-bufs {dmabuf}", device.name());
        assert!(dmabuf["imported"].as_u64().unwrap() >= 1, "{dmabuf}");
        assert_eq!(dmabuf["fallbacks"], 0, "{dmabuf}");
    }

    // The newest window has the focus. Typing shows up in its tile and in
    // no other.
    let focused = session.state()["focus"]["window"].as_u64().unwrap();
    assert_eq!(focused, settled.last().unwrap().0);
    // Types "hello" and captures the screen once the application has drawn
    // it: once the picture differs from `before` and has stopped changing.
    // How long the application takes is the machine's business.
    let type_text = |session: &mut Session, name: &str, before: &EncodedImage, script: bool| {
        for code in [35, 18, 38, 38, 24] {
            // h e l l o
            key(session, code, true);
            key(session, code, false);
        }
        let capture = |session: &mut Session| {
            if script {
                settled_screenshot(session, name)
            } else {
                session.screenshot_png(name)
            }
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let image = capture(session);
            if image.data != before.data {
                return capture(session);
            }
            assert!(Instant::now() < deadline, "typing never showed up");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let typed = type_text(&mut session, "typed", &first, true);
    let rect = |t: &Tile| (t.2, t.3, t.4, t.5);
    assert!(changed(&first, &typed, rect(settled.last().unwrap())) > 0);
    assert_eq!(changed(&first, &typed, rect(&settled[0])), 0);

    // Logo+H: the focus moves left; typing goes there now.
    chord(&mut session, &[KEY_LEFTMETA], KEY_H);
    let left = settled[settled.len() - 2].clone();
    wait_state(&mut session, "the focus moved left", |s| {
        (s["focus"]["window"] == left.0).then_some(())
    });
    let typed_left = type_text(&mut session, "typed-left", &typed, true);
    assert!(changed(&typed, &typed_left, rect(&left)) > 0);

    // Logo+Shift+L: it trades places with its right neighbour.
    chord(&mut session, &[KEY_LEFTMETA, KEY_LEFTSHIFT], KEY_L);
    let swapped = wait_state(&mut session, "the swap", |s| {
        let t = tiles(s)?;
        (t.last().unwrap().0 == left.0).then_some(t)
    });
    assert_eq!(swapped.len(), settled.len());
    session.screenshot("swapped");

    // One of them turned, to look at.
    let turned = swapped[0].clone();
    let (cx, cy) = (turned.4 as f64 / 2.0, turned.5 as f64 / 2.0);
    let (sin, cos) = 8.0f64.to_radians().sin_cos();
    // translate(to the tile center) * rotate * scale(0.9) * translate(-center)
    let (a, b, c, d) = (0.9 * cos, -0.9 * sin, 0.9 * sin, 0.9 * cos);
    let (tx, ty) = (
        turned.2 as f64 + cx - (a * cx + b * cy),
        turned.3 as f64 + cy - (c * cx + d * cy),
    );
    session.script("set_transform", json!([turned.0, [a, b, c, d, tx, ty]]));
    session.screenshot("turned");
    session.script("set_transform", json!([turned.0, Value::Null]));
    session.assert_script_healthy();

    // The script is read again: the tiles stay as they are, and typing
    // still works.
    let before = settled_screenshot(&mut session, "before-reload");
    session.send(Request::ScriptReload { path: None });
    assert_eq!(session.script_state()["mode"], "running");
    let after = wait_state(&mut session, "tiles after the reload", tiles);
    assert_eq!(after, swapped);
    let reloaded = settled_screenshot(&mut session, "after-reload");
    assert_eq!(changed(&before, &reloaded, (0, 0, 1280, 720)), 0);
    let typed_again = type_text(&mut session, "typed-after-reload", &reloaded, true);
    assert!(changed(&reloaded, &typed_again, (0, 0, 1280, 720)) > 0);

    // The built-in fallback: the same windows at the same places, with the
    // marker along the top edge; and back.
    session.send(Request::ScriptMode {
        mode: "fallback".into(),
    });
    let fallback = session.screenshot_png("fallback");
    let marker = fallback.pixel(640, 1);
    assert_eq!((marker[0], marker[1]), (255, 0), "{marker:?}");
    let typed_in_fallback = type_text(&mut session, "typed-in-fallback", &fallback, false);
    assert!(changed(&fallback, &typed_in_fallback, (0, 8, 1280, 704)) > 0);
    session.send(Request::ScriptMode {
        mode: "running".into(),
    });
    session.screenshot("back");

    let state = session.state();
    assert_eq!(state["script"]["failures"], 0, "{:#}", state["script"]);
    assert_eq!(state["windows"].as_array().unwrap().len() as i64, n);
    println!(
        "{}: frame time {}",
        device.name(),
        state["frame_time_ms"]["total"]
    );
    session.assert_drawing_clean();
    session.quit();
    drop(clients);
}

#[test]
fn terminals_are_tiled_on_the_hardware_device() {
    if wanted("script-tiles", &["weston-terminal"]) {
        let device = Device::new(Gpu::Hardware);
        if device.enabled() {
            terminals_in_tiles(device);
        } else {
            eprintln!("skipped: gpu is not in HIZUKURI_TEST_DEVICES");
        }
    }
}

#[test]
fn terminals_are_tiled_on_lavapipe() {
    if wanted("script-tiles", &["weston-terminal"]) {
        let device = Device::new(Gpu::Lavapipe);
        if device.enabled() {
            terminals_in_tiles(device);
        } else {
            eprintln!("skipped: lavapipe is not in HIZUKURI_TEST_DEVICES");
        }
    }
}
