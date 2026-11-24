//! The nested backend: a compositor that runs as a window of another one.
//!
//! The parent is always a headless `proto-compositor` the test starts
//! itself. The nested compositor is given the path of the parent's socket
//! and can reach nothing else.

mod common;

use std::time::{Duration, Instant};

use common::{Device, Session, TIMEOUT, options};
use proto_compositor::control::Request;
use proto_compositor::image::EncodedImage;
use proto_test_client::{ClientEvent, ConstraintRequest};
use serde_json::{Value, json};

const KEY_A: u32 = 30;
const KEY_ENTER: u32 = 28;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;
const BTN_LEFT: u32 = 0x110;
const NESTED_APP_ID: &str = "hizukuri.nested";

/// The window of the nested compositor in the parent's state, once it shows
/// a frame.
fn nested_window(state: &Value) -> Option<&Value> {
    state["windows"].as_array()?.iter().find(|w| {
        w["app_id"] == NESTED_APP_ID && w["mapped"] == true && w["surfaces"][0]["size"].is_array()
    })
}

fn wait_for_nested_window(parent: &mut Session) -> u64 {
    let state = parent.wait(&mut [], "the window of the nested compositor", |state| {
        nested_window(state).is_some()
    });
    nested_window(&state).unwrap()["id"].as_u64().unwrap()
}

/// The picture went through two compositors, each converting to its
/// compositing space and back: saturated colors come out a few codes off.
fn close_to(pixel: [u16; 4], wanted: [u16; 3]) -> bool {
    (0..3).all(|c| pixel[c].abs_diff(wanted[c]) <= 6)
}

/// Takes screenshots of the parent until `done` accepts one.
fn wait_for_picture(
    parent: &mut Session,
    name: &str,
    what: &str,
    done: impl Fn(&EncodedImage) -> bool,
) -> EncodedImage {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let image = parent.screenshot_png(name);
        if done(&image) {
            return image;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for: {what} (at 200,125: {:?}; at 110,60: {:?})",
            image.pixel(200, 125),
            image.pixel(110, 60)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A parent, a nested compositor placed at (100, 50) in it, and a red
/// client placed at (40, 30) in the nested one: the client shows in the
/// parent's picture, and input sent to the parent reaches it.
fn frames_and_input(device: Device, parent_args: &[&str], nested_args: &[&str], buffers: &str) {
    let name = format!("nested-{buffers}");
    let mut parent_arguments = vec!["--output-size", "800x600"];
    parent_arguments.extend_from_slice(parent_args);
    let mut parent = Session::start(&format!("{name}-parent"), device, &parent_arguments);
    let mut nested_arguments = vec!["--output-size", "400x300", "--background", "004000"];
    nested_arguments.extend_from_slice(nested_args);
    nested_arguments.extend_from_slice(&["--cursor", "off"]);
    let mut nested =
        Session::start_nested(&format!("{name}-child"), device, &parent, &nested_arguments);

    let nested_state = nested.state();
    assert_eq!(nested_state["backend"], "nested");
    assert_eq!(
        nested_state["backend_status"]["output"]["buffers"], buffers,
        "{nested_state:#}"
    );
    assert_eq!(
        nested_state["outputs"][0]["size"],
        serde_json::json!([400, 300])
    );

    let id = wait_for_nested_window(&mut parent);
    parent.send(Request::MoveWindow {
        window: id,
        x: 100,
        y: 50,
    });
    let (mut client, client_id) = nested.mapped_client(options("inner", 120, 90, [255, 0, 0, 255]));
    nested.send(Request::MoveWindow {
        window: client_id,
        x: 40,
        y: 30,
    });

    // The client's red at its place inside the nested window, the nested
    // background around it, the parent's background outside.
    let image = wait_for_picture(
        &mut parent,
        "parent",
        "the client in the parent's picture",
        // The corners tell the client's final place from where it was
        // first put.
        |i| {
            close_to(i.pixel(100 + 40 + 2, 50 + 30 + 2), [255, 0, 0])
                && close_to(i.pixel(100 + 40 + 117, 50 + 30 + 87), [255, 0, 0])
                && close_to(i.pixel(100 + 10, 50 + 10), [0, 64, 0])
        },
    );
    assert!(close_to(
        image.pixel(100 + 40 + 2, 50 + 30 + 2),
        [255, 0, 0]
    ));
    assert!(close_to(
        image.pixel(100 + 40 + 117, 50 + 30 + 87),
        [255, 0, 0]
    ));
    assert!(close_to(
        image.pixel(100 + 40 + 122, 50 + 30 + 45),
        [0, 64, 0]
    ));
    assert!(close_to(image.pixel(100 + 398, 50 + 298), [0, 64, 0]));
    assert!(close_to(image.pixel(100 + 402, 50 + 150), [32, 32, 32]));
    assert!(close_to(image.pixel(50, 300), [32, 32, 32]));

    // Input: the parent's pointer and keyboard, through the nested window,
    // to the client.
    assert!(
        client
            .wait_for(TIMEOUT, |e| *e == ClientEvent::KeyboardEnter)
            .unwrap()
            .is_some()
    );
    client.clear_events();
    parent.send(Request::PointerMotion {
        x: 100.0 + 40.0 + 10.0,
        y: 50.0 + 30.0 + 20.0,
    });
    let entered = client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::PointerEnter { .. }))
        .unwrap()
        .expect("the pointer enters the client");
    assert_eq!(entered, ClientEvent::PointerEnter { x: 10.0, y: 20.0 });
    // The nested compositor does not draw a cursor here (`--cursor off`):
    // it asks the parent for the default one.
    parent.wait(&mut [], "the parent's default cursor", |state| {
        state["pointer"]["cursor"] == serde_json::json!({ "named": "default" })
    });
    parent.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    parent.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    for pressed in [true, false] {
        let event = client
            .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::PointerButton { .. }))
            .unwrap()
            .expect("a button event");
        assert_eq!(
            event,
            ClientEvent::PointerButton {
                button: BTN_LEFT,
                pressed
            }
        );
    }
    parent.send(Request::PointerAxis {
        horizontal: 0.0,
        vertical: 15.0,
        v120_horizontal: None,
        v120_vertical: Some(120),
    });
    let axis = client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::PointerAxis { .. }))
        .unwrap()
        .expect("a scroll event");
    assert_eq!(
        axis,
        ClientEvent::PointerAxis {
            axis: 0,
            value: 15.0
        }
    );

    // A finger on the parent's screen is a finger inside.
    client.request_touch();
    nested.wait(&mut [&mut client], "the client's wl_touch", |_| true);
    assert!(client.has_touch(), "the nested seat offers touch");
    parent.send(Request::TouchDown {
        slot: 0,
        x: 100.0 + 40.0 + 30.0,
        y: 50.0 + 30.0 + 40.0,
    });
    parent.send(Request::TouchFrame);
    parent.send(Request::TouchUp { slot: 0 });
    parent.send(Request::TouchFrame);
    let touched = client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::TouchDown { .. }))
        .unwrap()
        .expect("a touch");
    match touched {
        ClientEvent::TouchDown { x, y, .. } => assert_eq!((x, y), (30.0, 40.0)),
        other => panic!("unexpected event {other:?}"),
    }
    assert!(
        client
            .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::TouchUp { .. }))
            .unwrap()
            .is_some()
    );

    assert_eq!(
        parent.state()["focus"]["window"].as_u64(),
        Some(id),
        "the nested window has the parent's keyboard focus"
    );
    for pressed in [true, false] {
        parent.send(Request::Key {
            keycode: KEY_A,
            pressed,
        });
    }
    for pressed in [true, false] {
        let event = client
            .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::Key { .. }))
            .unwrap()
            .expect("a key event");
        match event {
            ClientEvent::Key {
                keycode,
                pressed: p,
                ..
            } => assert_eq!((keycode, p), (KEY_A, pressed)),
            other => panic!("unexpected event {other:?}"),
        }
    }

    // A new picture of the client arrives in the parent.
    nested.recolor(&mut client, "inner", [0, 0, 255, 255]);
    wait_for_picture(
        &mut parent,
        "parent-blue",
        "the new color in the parent",
        |i| close_to(i.pixel(100 + 40 + 60, 50 + 30 + 45), [0, 0, 255]),
    );

    // A capture of the nested compositor itself shows the same frame.
    let own = nested.screenshot_png("nested");
    assert_eq!((own.width, own.height), (400, 300));
    assert!(close_to(own.pixel(40 + 60, 30 + 45), [0, 0, 255]));
    assert!(close_to(own.pixel(10, 10), [0, 64, 0]));

    nested.assert_script_healthy();
    nested.assert_drawing_clean();
    nested.quit();
    parent.wait(&mut [], "the nested window to go", |state| {
        nested_window(state).is_none()
    });
    parent.quit();
}

fn frames_and_input_through_shared_memory(device: Device) {
    frames_and_input(device, &[], &[], "shm");
}

/// The same with the frames handed over as dma-bufs: the parent takes
/// dma-bufs, the nested compositor exports its render targets.
fn frames_and_input_through_dmabufs(device: Device) {
    if device.gpu == common::Gpu::Lavapipe && !common::udmabuf_wanted() {
        return;
    }
    frames_and_input(
        device,
        &["--dmabuf", "gpu"],
        &["--nested-buffers", "dmabuf"],
        "dmabuf",
    );
}

/// The parent tiles: a second window in it halves the nested window, the
/// nested output follows, and the client inside is configured again.
fn the_output_follows_the_window(device: Device) {
    let mut parent = Session::start_builtin(
        "nested-resize-parent",
        device,
        &["--output-size", "800x600"],
    );
    let mut nested = Session::start_nested_builtin(
        "nested-resize-child",
        device,
        &parent,
        &["--output-size", "400x300"],
    );
    wait_for_nested_window(&mut parent);
    let full = nested.wait(&mut [], "the tiled size", |state| {
        state["outputs"][0]["size"] != serde_json::json!([400, 300])
    });
    let full_size = full["outputs"][0]["size"].clone();
    let (mut client, _) = nested.mapped_client(options("inner", 120, 90, [255, 0, 0, 255]));
    client.clear_events();

    let (_other, _) = parent.mapped_client(options("other", 100, 100, [0, 255, 0, 255]));
    let state = nested.wait(&mut [&mut client], "the smaller output", |state| {
        state["outputs"][0]["size"] != full_size
    });
    let size = &state["outputs"][0]["size"];
    let (width, height) = (size[0].as_i64().unwrap(), size[1].as_i64().unwrap());
    let (full_width, full_height) = (
        full_size[0].as_i64().unwrap(),
        full_size[1].as_i64().unwrap(),
    );
    assert!(
        width < full_width || height < full_height,
        "{size} is not smaller than {full_size}"
    );
    // The client inside is tiled into the new output.
    let configured = client
        .wait_for(TIMEOUT, |e| {
            matches!(e, ClientEvent::Configure { width: w, height: h, .. }
                if i64::from(*w) <= width && i64::from(*h) <= height && *w > 0)
        })
        .unwrap();
    assert!(configured.is_some(), "the client was not configured again");
    // The parent shows the nested window at its new size.
    let parent_state = parent.wait(
        &mut [&mut client],
        "the resized window in the parent",
        |state| {
            nested_window(state).is_some_and(|w| {
                w["surfaces"][0]["buffer_size"] == serde_json::json!([width, height])
            })
        },
    );
    assert!(nested_window(&parent_state).is_some());
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// The nested output has the scale the parent shows the window at: the
/// buffers have the parent's physical pixels, and positions agree.
fn the_output_takes_the_scale_of_the_parent(device: Device) {
    let mut parent = Session::start(
        "nested-scale-parent",
        device,
        &["--output-size", "1200x900", "--output-scale", "1.5"],
    );
    let mut nested = Session::start_nested(
        "nested-scale-child",
        device,
        &parent,
        &["--output-size", "400x300"],
    );
    let id = wait_for_nested_window(&mut parent);
    let state = nested.wait(&mut [], "the parent's scale", |state| {
        state["outputs"][0]["scale"] == 1.5
    });
    assert_eq!(state["outputs"][0]["size"], serde_json::json!([600, 450]));
    assert_eq!(
        state["outputs"][0]["logical_size"],
        serde_json::json!([400, 300])
    );
    // The parent gets buffers of its physical pixels for a window of the
    // same logical size.
    parent.wait(&mut [], "buffers at the parent's scale", |state| {
        nested_window(state).is_some_and(|w| {
            w["surfaces"][0]["buffer_size"] == serde_json::json!([600, 450])
                && w["surfaces"][0]["size"] == serde_json::json!([400, 300])
        })
    });
    parent.send(Request::MoveWindow {
        window: id,
        x: 100,
        y: 50,
    });
    let (mut client, client_id) = nested.mapped_client(options("inner", 120, 90, [255, 0, 0, 255]));
    nested.send(Request::MoveWindow {
        window: client_id,
        x: 40,
        y: 30,
    });
    client.clear_events();
    // Logical coordinates of the parent are logical coordinates inside.
    parent.send(Request::PointerMotion {
        x: 100.0 + 40.0 + 10.0,
        y: 50.0 + 30.0 + 20.0,
    });
    let entered = client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::PointerEnter { .. }))
        .unwrap()
        .expect("the pointer enters the client");
    assert_eq!(entered, ClientEvent::PointerEnter { x: 10.0, y: 20.0 });
    // The nested compositor draws its own cursor: the parent's is hidden.
    parent.wait(&mut [], "the hidden cursor", |state| {
        state["pointer"]["cursor"] == "hidden"
    });
    // In the parent's picture (physical pixels) the client is where its
    // logical place says.
    wait_for_picture(&mut parent, "parent", "the client at 1.5x", |i| {
        close_to(i.pixel(150 + 60 + 90, 75 + 45 + 67), [255, 0, 0])
    });
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// Presses `key` in the parent with `modifier` held.
fn press_with(parent: &mut Session, modifier: u32, key: u32) {
    for (keycode, pressed) in [
        (modifier, true),
        (key, true),
        (key, false),
        (modifier, false),
    ] {
        parent.send(Request::Key { keycode, pressed });
    }
}

/// The built-in project, which also notes the logo keys it is told of.
const LOGO_KEYS_MAIN: &str = r#"
hz.include("session")

local told = {}

function logo_key_changed(key)
    table.insert(told, key)
end

-- Whether the logo key was held at the last press of each key.
local logo_held = {}
local tile_key = key

function key(keycode, pressed, mods)
    if pressed then
        logo_held[tostring(keycode)] = mods.logo
    end
    return tile_key and tile_key(keycode, pressed, mods)
end

function logo_keys()
    return { now = hz.logo_key(), told = table.concat(told, " "), logo_held = logo_held }
end
"#;

/// Starts a nested compositor with [`LOGO_KEYS_MAIN`] and waits until the
/// parent passes its shortcuts to it.
fn start_with_logo_keys(
    name: &str,
    device: Device,
    parent: &mut Session,
    args: &[&str],
) -> Session {
    let project = common::script_project(name, device, &[("main", LOGO_KEYS_MAIN)]);
    let mut args = args.to_vec();
    args.extend(["--script", project.to_str().unwrap()]);
    let mut nested = Session::start_nested(name, device, parent, &args);
    let id = wait_for_nested_window(parent);
    assert_eq!(parent.state()["focus"]["window"].as_u64(), Some(id));
    nested.wait(&mut [], "the shortcuts of the parent", |state| {
        state["backend_status"]["shortcuts_inhibited"] == true
    });
    nested
}

/// Asks the script of `nested` until `hz.logo_key()` is `key`, and answers
/// the names `logo_key_changed` was called with, in order.
fn wait_for_logo_key(nested: &mut Session, key: &str) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let keys = nested.script("logo_keys", json!([]));
        if keys["now"] == key {
            return keys["told"].as_str().unwrap_or_default().to_string();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {key} as the logo key: {keys}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `--nested-logo-key auto` (the default): Super is the logo key while the
/// parent passes its shortcuts to the window, Alt while it does not, and
/// the script is told each time.
fn the_logo_key_follows_the_parent(device: Device) {
    common::use_terminal_stand_in();
    let mut parent = Session::start("nested-auto-logo-parent", device, &[]);
    let mut nested = start_with_logo_keys("nested-auto-logo", device, &mut parent, &[]);
    wait_for_logo_key(&mut nested, "super");
    assert_eq!(nested.state()["backend_status"]["logo_key_choice"], "auto");
    let terminal = nested.runtime_dir.join("terminal");
    // Super+Return in the parent is logo+Return inside.
    press_with(&mut parent, KEY_LEFTMETA, KEY_ENTER);
    common::wait_for_file(&terminal);
    std::fs::remove_file(&terminal).unwrap();

    // The parent handles its shortcuts itself again (its toggle key): Alt
    // stands in for the logo key.
    parent.send(Request::SetShortcutsInhibit { active: false });
    let told = wait_for_logo_key(&mut nested, "alt");
    assert!(told.ends_with("alt"), "told: {told:?}");
    let state = nested.state();
    assert_eq!(state["backend_status"]["shortcuts_inhibited"], false);
    assert_eq!(state["backend_status"]["logo_key"], "alt");
    press_with(&mut parent, KEY_LEFTALT, KEY_ENTER);
    common::wait_for_file(&terminal);
    std::fs::remove_file(&terminal).unwrap();

    // And passes them on again while Alt is held as the logo key: Super is
    // back, and Alt is let go of as the logo key it was pressed as.
    parent.send(Request::Key {
        keycode: KEY_LEFTALT,
        pressed: true,
    });
    parent.send(Request::SetShortcutsInhibit { active: true });
    let told = wait_for_logo_key(&mut nested, "super");
    assert!(told.ends_with("alt super"), "told: {told:?}");
    parent.send(Request::Key {
        keycode: KEY_LEFTALT,
        pressed: false,
    });
    for pressed in [true, false] {
        parent.send(Request::Key {
            keycode: KEY_A,
            pressed,
        });
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let held = nested.script("logo_keys", json!([]))["logo_held"][KEY_A.to_string()].clone();
        if held == false {
            break;
        }
        assert!(held.is_null(), "the logo key is still held: {held}");
        assert!(Instant::now() < deadline, "the key did not arrive");
        std::thread::sleep(Duration::from_millis(20));
    }
    press_with(&mut parent, KEY_LEFTMETA, KEY_ENTER);
    common::wait_for_file(&terminal);
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// `--nested-logo-key super` stays Super when the parent stops passing its
/// shortcuts: the script is told of no change.
fn a_fixed_logo_key_does_not_follow_the_parent(device: Device) {
    common::use_terminal_stand_in();
    let mut parent = Session::start("nested-super-logo-parent", device, &[]);
    let mut nested = start_with_logo_keys(
        "nested-super-logo",
        device,
        &mut parent,
        &["--nested-logo-key", "super"],
    );
    parent.send(Request::SetShortcutsInhibit { active: false });
    nested.wait(&mut [], "the parent to keep its shortcuts", |state| {
        state["backend_status"]["shortcuts_inhibited"] == false
    });
    // The parent of the test takes no keys itself: Super+Return reaches the
    // window, and is logo+Return there.
    press_with(&mut parent, KEY_LEFTMETA, KEY_ENTER);
    common::wait_for_file(&nested.runtime_dir.join("terminal"));
    let state = nested.state();
    assert_eq!(state["backend_status"]["logo_key"], "super");
    assert_eq!(state["backend_status"]["logo_key_choice"], "super");
    assert_eq!(wait_for_logo_key(&mut nested, "super"), "");
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// `--nested-logo-key alt`: the parent's Alt is the logo key inside, also
/// while the parent passes its shortcuts.
fn another_key_stands_in_for_the_logo_key(device: Device) {
    common::use_terminal_stand_in();
    let mut parent = Session::start("nested-logo-parent", device, &[]);
    let mut nested = start_with_logo_keys(
        "nested-logo",
        device,
        &mut parent,
        &["--nested-logo-key", "alt"],
    );
    wait_for_logo_key(&mut nested, "alt");
    // Alt+Return in the parent is logo+Return inside: the built-in project
    // starts its terminal.
    press_with(&mut parent, KEY_LEFTALT, KEY_ENTER);
    let text = common::wait_for_file(&nested.runtime_dir.join("terminal"));
    assert_eq!(text.trim(), "hizukuri-test");
    assert_eq!(nested.state()["backend_status"]["logo_key_choice"], "alt");
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// `--nested-keep-parent-shortcuts`: nothing is asked of the parent, so
/// `auto` makes Alt the logo key from the start.
fn without_the_parents_shortcuts_alt_is_the_logo_key(device: Device) {
    common::use_terminal_stand_in();
    let mut parent = Session::start("nested-keep-parent", device, &[]);
    let mut nested = Session::start_nested_builtin(
        "nested-keep-child",
        device,
        &parent,
        &["--nested-keep-parent-shortcuts"],
    );
    let id = wait_for_nested_window(&mut parent);
    assert_eq!(parent.state()["focus"]["window"].as_u64(), Some(id));
    let state = nested.wait(&mut [], "Alt as the logo key", |state| {
        state["backend_status"]["logo_key"] == "alt"
    });
    assert_eq!(state["backend_status"]["shortcuts_inhibited"], false);
    press_with(&mut parent, KEY_LEFTALT, KEY_ENTER);
    common::wait_for_file(&nested.runtime_dir.join("terminal"));
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// A parent with shortcuts of its own (the built-in tiling) passes them to
/// the nested window, which asked for them: logo+Return starts the
/// terminal of the compositor inside, not the parent's.
fn the_parent_passes_its_shortcuts(device: Device) {
    common::use_terminal_stand_in();
    let mut parent = Session::start_builtin("nested-inhibit-parent", device, &[]);
    let mut nested = Session::start_nested_builtin("nested-inhibit-child", device, &parent, &[]);
    let id = wait_for_nested_window(&mut parent);
    assert_eq!(parent.state()["focus"]["window"].as_u64(), Some(id));
    let state = nested.wait(&mut [], "the shortcuts of the parent", |state| {
        state["backend_status"]["shortcuts_inhibited"] == true
    });
    // The keymap of the parent arrived with the keyboard.
    assert_eq!(state["backend_status"]["parent_keymap"], true);
    for (keycode, pressed) in [
        (KEY_LEFTMETA, true),
        (KEY_ENTER, true),
        (KEY_ENTER, false),
        (KEY_LEFTMETA, false),
    ] {
        parent.send(Request::Key { keycode, pressed });
    }
    let text = common::wait_for_file(&nested.runtime_dir.join("terminal"));
    assert_eq!(text.trim(), "hizukuri-test");
    assert!(
        !parent.runtime_dir.join("terminal").exists(),
        "the parent took the shortcut too"
    );
    nested.assert_script_healthy();
    parent.assert_script_healthy();
    nested.quit();
    // Without the nested window the parent's shortcut is its own again.
    parent.wait(&mut [], "the nested window to go", |state| {
        nested_window(state).is_none()
    });
    for (keycode, pressed) in [
        (KEY_LEFTMETA, true),
        (KEY_ENTER, true),
        (KEY_ENTER, false),
        (KEY_LEFTMETA, false),
    ] {
        parent.send(Request::Key { keycode, pressed });
    }
    common::wait_for_file(&parent.runtime_dir.join("terminal"));
    parent.quit();
}

/// A client inside locks the pointer: the nested compositor asks the parent
/// to hold its pointer too, and the parent's relative motion is the
/// client's.
fn a_pointer_lock_reaches_the_parent(device: Device) {
    let mut parent = Session::start("nested-lock-parent", device, &[]);
    let mut nested = Session::start_nested(
        "nested-lock-child",
        device,
        &parent,
        &["--output-size", "400x300"],
    );
    let id = wait_for_nested_window(&mut parent);
    parent.send(Request::MoveWindow {
        window: id,
        x: 100,
        y: 50,
    });
    let (mut client, client_id) = nested.mapped_client(options("inner", 120, 90, [255, 0, 0, 255]));
    nested.send(Request::MoveWindow {
        window: client_id,
        x: 40,
        y: 30,
    });
    parent.send(Request::PointerMotion {
        x: 100.0 + 40.0 + 10.0,
        y: 50.0 + 30.0 + 20.0,
    });
    assert!(
        client
            .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::PointerEnter { .. }))
            .unwrap()
            .is_some()
    );
    client.request_relative_pointer().unwrap();
    client
        .constrain_pointer(ConstraintRequest::Lock, None, true)
        .unwrap();
    let active = |state: &Value| {
        state["pointer_constraints"].as_array().is_some_and(|all| {
            all.iter()
                .any(|c| c["active"] == true && c["kind"] == "lock")
        })
    };
    nested.wait(&mut [&mut client], "the lock inside", active);
    parent.wait(&mut [&mut client], "the lock in the parent", active);
    nested.wait(&mut [&mut client], "the parent's answer", |state| {
        state["backend_status"]["pointer_locked"] == true
    });
    let before = nested.state()["pointer"]["position"].clone();
    client.clear_events();
    parent.send(Request::PointerMotionRelative {
        dx: 5.0,
        dy: -3.0,
        dx_unaccel: Some(10.0),
        dy_unaccel: Some(-6.0),
    });
    let moved = client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::RelativeMotion { .. }))
        .unwrap()
        .expect("relative motion");
    match moved {
        ClientEvent::RelativeMotion {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            ..
        } => assert_eq!((dx, dy, dx_unaccel, dy_unaccel), (5.0, -3.0, 10.0, -6.0)),
        other => panic!("unexpected event {other:?}"),
    }
    // The pointer stands still, inside and outside.
    assert_eq!(nested.state()["pointer"]["position"], before);

    client.withdraw_pointer_constraint();
    nested.wait(&mut [&mut client], "the lock inside to end", |state| {
        !active(state)
    });
    parent.wait(
        &mut [&mut client],
        "the lock in the parent to end",
        |state| !active(state),
    );
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// `--nested-fullscreen`: a parent that grants it (the built-in tiling)
/// gives the window its whole output.
fn the_window_asks_for_fullscreen(device: Device) {
    let mut parent = Session::start_builtin(
        "nested-fullscreen-parent",
        device,
        &["--output-size", "800x600"],
    );
    let mut nested = Session::start_nested(
        "nested-fullscreen-child",
        device,
        &parent,
        &["--output-size", "400x300", "--nested-fullscreen"],
    );
    wait_for_nested_window(&mut parent);
    nested.wait(&mut [], "the size of the parent's output", |state| {
        state["outputs"][0]["size"] == serde_json::json!([800, 600])
    });
    let state = parent.state();
    assert_eq!(nested_window(&state).unwrap()["fullscreen"], true);
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// A session that draws the cursor (the nested backend does, a headless one
/// when told to): a project may ask for the cursor while a render pass is
/// open. The built-in arrow, which is made when it is first needed, is
/// ready by then.
fn a_project_asks_for_the_cursor_inside_a_render_pass(device: Device) {
    let project = common::script_project(
        "cursor-in-pass",
        device,
        &[(
            "main",
            r#"
hz.include("session")
-- Ask inside a pass of this project's own, before the built-in drawing
-- (which asks outside its pass) runs.
local draw = frame
local asked_inside = 0
function frame(output, target)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    if hz.cursor() then
        asked_inside += 1
    end
    gfx.end_pass()
    draw(output, target)
end
function asked()
    return asked_inside
end
"#,
        )],
    );
    let mut session = Session::start(
        "cursor-in-pass",
        device,
        &["--cursor", "on", "--script", project.to_str().unwrap()],
    );
    session.send(Request::PointerMotion { x: 100.0, y: 80.0 });
    session.screenshot_png("with-cursor");
    assert!(
        session.script("asked", serde_json::json!([])).as_u64() > Some(0),
        "the project did not get a cursor inside its pass"
    );
    session.assert_script_healthy();
    session.quit();
}

/// Closing the window in the parent ends the nested compositor.
fn closing_the_window_ends_the_compositor(device: Device) {
    let mut parent = Session::start("nested-close-parent", device, &[]);
    let nested = Session::start_nested("nested-close-child", device, &parent, &[]);
    let id = wait_for_nested_window(&mut parent);
    parent.send(Request::CloseWindow { window: id });
    nested.expect_clean_exit();
    parent.quit();
}

/// A parent that dies takes the nested compositor with it: it ends instead
/// of waiting for frames that never come.
fn a_parent_that_dies_ends_the_compositor(device: Device) {
    let mut parent = Session::start("nested-killed-parent", device, &[]);
    let nested = Session::start_nested("nested-killed-child", device, &parent, &[]);
    wait_for_nested_window(&mut parent);
    parent.signal("KILL");
    nested.expect_clean_exit();
    let left = parent.abandon();
    let _ = std::fs::remove_dir_all(left);
}

/// Without a parent there is no nested compositor: the start fails with a
/// message instead of falling back to some other display.
#[test]
fn a_missing_parent_is_an_error() {
    let directory = std::env::temp_dir().join(format!("hzk-nested-none-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_proto-compositor"));
    command.args(["--backend", "nested", "--no-drawing", "--exit-after", "5"]);
    common::isolate(&mut command, &directory);
    let output = command.output().unwrap();
    let _ = std::fs::remove_dir_all(&directory);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no parent compositor"), "{stderr}");
}

device_tests!(
    frames_and_input_through_shared_memory,
    frames_and_input_through_dmabufs,
    the_output_follows_the_window,
    the_output_takes_the_scale_of_the_parent,
    another_key_stands_in_for_the_logo_key,
    the_logo_key_follows_the_parent,
    a_fixed_logo_key_does_not_follow_the_parent,
    without_the_parents_shortcuts_alt_is_the_logo_key,
    the_parent_passes_its_shortcuts,
    the_window_asks_for_fullscreen,
    a_pointer_lock_reaches_the_parent,
    a_project_asks_for_the_cursor_inside_a_render_pass,
    closing_the_window_ends_the_compositor,
    a_parent_that_dies_ends_the_compositor,
);
