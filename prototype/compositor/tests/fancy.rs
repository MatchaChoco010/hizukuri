//! The `fancy` project (`projects/fancy`): workspaces on a strip, a camera
//! that travels between them, windows on springs.
//!
//! The sessions run on a manual clock (`--clock manual`), so every picture
//! and every position below belongs to an exact time.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{Device, Session, options, window};
use proto_compositor::control::Request;
use proto_compositor::image::EncodedImage;
use proto_test_client::{
    ClientEvent, ClientOptions, DialogOptions, InputMethodMode, PopupOptions, SurfaceKind,
    TestClient, ToplevelRequest,
};
use serde_json::{Value, json};

const KEY_1: u32 = 2;
const KEY_TAB: u32 = 15;
const KEY_Q: u32 = 16;
const KEY_H: u32 = 35;
const KEY_LEFTCTRL: u32 = 29;
const KEY_L: u32 = 38;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_SPACE: u32 = 57;
const BTN_LEFT: u32 = 272;
const KEY_LEFTMETA: u32 = 125;

const RED: [u8; 4] = [196, 64, 54, 255];
const GREEN: [u8; 4] = [72, 160, 96, 255];
const BLUE: [u8; 4] = [64, 110, 190, 255];
const SAND: [u8; 4] = [214, 190, 140, 255];
const SLATE: [u8; 4] = [70, 78, 96, 255];

fn project() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("projects/fancy")
}

fn start(name: &str, device: Device, size: &str) -> Session {
    start_with(name, device, size, &[])
}

fn start_with(name: &str, device: Device, size: &str, more: &[&str]) -> Session {
    let project = project();
    // The tests of this file run side by side: the project's shaders are
    // built once, before any of them starts a session (which would each
    // build them too).
    static SHADERS: std::sync::Once = std::sync::Once::new();
    SHADERS.call_once(|| common::build_shaders(&project));
    // The compositor keeps the environment of whoever runs the tests (their
    // home), and so would a terminal it starts: logo+Return in any of these
    // sessions starts the stand-in, never a real terminal.
    common::use_terminal_stand_in();
    let mut arguments = vec![
        "--output-size",
        size,
        "--clock",
        "manual",
        "--script",
        project.to_str().unwrap(),
    ];
    arguments.extend(more);
    let mut session = Session::start(name, device, &arguments);
    assert_eq!(session.state()["clock"]["kind"], "manual");
    session
}

/// A session for the tests that look at the colors of windows: windows
/// without the focus are drawn as bright as the others.
fn start_plain(name: &str, device: Device, size: &str) -> Session {
    let mut session = start(name, device, size);
    session.script("fancy_setting", json!(["cold_brightness", 1.0]));
    session
}

fn advance(session: &mut Session, seconds: f64) {
    session.send(Request::AdvanceClock {
        milliseconds: seconds * 1000.0,
    });
}

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

/// What the last frame showed (`fancy_state`).
fn stage(session: &mut Session) -> Value {
    session.script("fancy_state", json!([]))
}

/// Opens a window of one color and waits until the script has placed it.
fn open(
    session: &mut Session,
    others: &mut [&mut TestClient],
    title: &str,
    color: [u8; 4],
) -> TestClient {
    let mut client = session.client(options(title, 200, 120, color));
    let mut all: Vec<&mut TestClient> = others.iter_mut().map(|c| &mut **c).collect();
    all.push(&mut client);
    session.wait(&mut all, &format!("{title} mapped"), |s| {
        window(s, title).is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
    });
    client
}

/// Starts a real terminal (`foot`) in the session, with `options`, running
/// `script` with `sh -c`. It reads no configuration file, and what runs in
/// it is that script, not a shell that reads startup files, so nothing of
/// whoever runs the tests comes in (the session gives it a home of its own,
/// too).
fn spawn_foot(
    session: &Session,
    name: &str,
    options: &[&str],
    script: &str,
) -> common::ClientProcess {
    let mut args = vec!["--config=/dev/null"];
    args.extend_from_slice(options);
    args.extend_from_slice(&["sh", "-c", script]);
    session.spawn_client(name, "foot", &args)
}

/// Lets time pass until nothing moves, pumping the clients.
fn settle(session: &mut Session, clients: &mut [&mut TestClient]) {
    for _ in 0..40 {
        advance(session, 0.5);
        session.wait(clients, "clients served", |_| true);
        session.screenshot_png("settle");
        if stage(session)["moving"] == false {
            return;
        }
    }
    panic!("the stage never came to rest: {:#}", stage(session));
}

device_tests!(
    a_workspace_switch_travels_and_comes_to_rest,
    a_switch_turned_half_way_keeps_its_speed,
    a_swap_moves_the_windows_and_input_follows,
    windows_open_and_close_over_time,
    the_overview_shows_the_strip_and_a_click_chooses,
    a_window_is_carried_with_the_pointer,
    a_swipe_carries_the_camera,
    windows_that_go_together_are_all_forgotten,
    a_window_without_the_focus_is_dimmer,
    menus_dialogs_and_fullscreen_work,
    the_candidates_of_an_input_method_follow_their_window,
    a_reload_keeps_everything_where_it_is,
    typing_reaches_the_terminal_with_the_focus,
    logo_return_starts_the_terminal,
    windows_out_of_view_are_told_they_are_not_seen,
);

/// Opens a window of one color whose client knows the `suspended` state
/// (xdg-shell 6) and animates (draws on every frame callback).
fn open_animated(
    session: &mut Session,
    others: &mut [&mut TestClient],
    title: &str,
    color: [u8; 4],
) -> TestClient {
    let mut client = session.client(ClientOptions {
        xdg_wm_base_version: 6,
        redraw_on_frame: true,
        ..options(title, 200, 120, color)
    });
    let mut all: Vec<&mut TestClient> = others.iter_mut().map(|c| &mut **c).collect();
    all.push(&mut client);
    session.wait(&mut all, &format!("{title} mapped"), |s| {
        window(s, title).is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
    });
    client
}

/// Whether the core is told a window is seen, and whether its client is
/// told it is suspended.
fn seen(session: &mut Session, title: &str) -> (bool, bool) {
    let state = session.state();
    let w = window(&state, title).expect("the window exists");
    (
        w["visible"].as_bool().unwrap(),
        w["suspended"].as_bool().unwrap(),
    )
}

/// The windows of a workspace out of view are not seen (`hz.set_visible`):
/// their clients are told they are suspended. A switch wakes the windows
/// it goes to at once, and puts the ones it leaves to sleep when they are
/// out of the picture; the overview shows them all; a window sent away
/// sleeps once it has flown out of the picture.
fn windows_out_of_view_are_told_they_are_not_seen(device: Device) {
    let mut session = start_plain("fancy-unseen", device, "640x400");
    let mut a = open_animated(&mut session, &mut [], "a", RED);
    logo_key(&mut session, KEY_1 + 1, false);
    let mut b = open_animated(&mut session, &mut [&mut a], "b", GREEN);
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_eq!(
        seen(&mut session, "a"),
        (false, true),
        "a, a workspace away"
    );
    assert_eq!(seen(&mut session, "b"), (true, false), "b in view");
    a.wait_for(common::TIMEOUT, |e| {
        matches!(e, ClientEvent::Suspended { suspended: true })
    })
    .unwrap()
    .expect("a is told it is suspended");

    // Back to the first workspace: a wakes as the camera sets off; b
    // stays awake while it is in the picture.
    logo_key(&mut session, KEY_1, false);
    session.wait(&mut [&mut a, &mut b], "a seen", |s| {
        window(s, "a").unwrap()["visible"] == true
    });
    assert_eq!(
        seen(&mut session, "b"),
        (true, false),
        "b still in the picture"
    );
    advance(&mut session, 0.1);
    session.wait(&mut [&mut a, &mut b], "clients served", |_| true);
    assert_eq!(seen(&mut session, "b"), (true, false), "b on its way out");
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_eq!(seen(&mut session, "a"), (true, false), "a in view");
    assert_eq!(seen(&mut session, "b"), (false, true), "b out of view");

    // The overview shows every workspace: every window is seen.
    logo_key(&mut session, KEY_TAB, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_eq!(seen(&mut session, "b"), (true, false), "b in the overview");
    logo_key(&mut session, KEY_TAB, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_eq!(
        seen(&mut session, "b"),
        (false, true),
        "b out of view again"
    );

    // a, sent to the third workspace while the camera stays: seen while
    // it flies out of the picture, not once it is gone.
    logo_key(&mut session, KEY_1 + 2, true);
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_eq!(session.script("fancy_spaces", json!([]))["active"], 1);
    assert_eq!(seen(&mut session, "a"), (false, true), "a sent away");
    // A hidden client gets about one frame callback a second of the frame
    // clock: two seconds, at most a few.
    a.clear_events();
    for _ in 0..4 {
        advance(&mut session, 0.5);
        session.wait(&mut [&mut a, &mut b], "clients served", |_| true);
    }
    let frames = a
        .events()
        .iter()
        .filter(|e| matches!(e, ClientEvent::FrameDone { .. }))
        .count();
    assert!(
        frames <= 3,
        "a hidden window got {frames} frame callbacks in two seconds"
    );
    session.assert_script_healthy();
    session.quit();
}

/// Logo+Return starts the terminal the environment names
/// (`HIZUKURI_TERMINAL`), in this session.
fn logo_return_starts_the_terminal(device: Device) {
    const KEY_ENTER: u32 = 28;
    let mut session = start("fancy-terminal", device, "640x400");
    logo_key(&mut session, KEY_ENTER, false);
    let text = common::wait_for_file(&session.runtime_dir.join("terminal"));
    assert_eq!(text.trim(), "hizukuri-test");
    session.assert_script_healthy();
    session.quit();
}

/// What is typed goes to the terminal that has the focus: the one that
/// opened last, then the one logo+H moves to, then, once that one has
/// closed, the one that is left. With real terminals (`foot`), each
/// running a shell that writes the line it reads to a file.
fn typing_reaches_the_terminal_with_the_focus(device: Device) {
    if !common::real_client_wanted("foot", &["foot"]) {
        return;
    }
    const KEY_A: u32 = 30;
    const KEY_B: u32 = 48;
    const KEY_ENTER: u32 = 28;
    let mut session = start("fancy-typing", device, "640x400");
    let lines = [
        session.runtime_dir.join("first.txt"),
        session.runtime_dir.join("second.txt"),
    ];
    let mut terminals = Vec::new();
    for (index, file) in lines.iter().enumerate() {
        let script = format!("read line; echo \"$line\" > {}", file.display());
        terminals.push(spawn_foot(&session, &format!("foot-{index}"), &[], &script));
        wait_windows(&mut session, index + 1);
    }
    rest(&mut session);
    let ids: Vec<u64> = session.script("fancy_spaces", json!([]))["spaces"][0]["order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(session.state()["focus"]["window"], ids[1]);

    let type_line = |session: &mut Session, keycode: u32| {
        for code in [keycode, keycode, KEY_ENTER] {
            key(session, code, true);
            key(session, code, false);
        }
    };
    let wait_for_line = |file: &std::path::Path, wanted: &str| {
        let deadline = std::time::Instant::now() + common::TIMEOUT;
        loop {
            if std::fs::read_to_string(file).is_ok_and(|text| text.trim() == wanted) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{} never held {wanted:?}",
                file.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // To the first terminal, and a line there.
    logo_key(&mut session, KEY_H, false);
    assert_eq!(session.state()["focus"]["window"], ids[0]);
    type_line(&mut session, KEY_A);
    wait_for_line(&lines[0], "aa");
    assert!(!lines[1].exists(), "the other terminal got the line too");
    // Its shell is done and its window closes: the focus goes to the one
    // that is left, and so does what is typed.
    let state = wait_until(&mut session, "the first terminal is gone", |s| {
        s["windows"].as_array().is_some_and(|w| w.len() == 1)
    });
    assert_eq!(state["focus"]["window"], ids[1]);
    type_line(&mut session, KEY_B);
    wait_for_line(&lines[1], "bb");
    rest(&mut session);
    session.assert_script_healthy();
    drop(terminals);
    session.quit();
}

/// Polls the state until `done` accepts it.
fn wait_until(session: &mut Session, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    loop {
        let state = session.state();
        if done(&state) {
            return state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reloading the script (Ctrl+Alt+Shift+R) keeps the workspaces, the
/// windows on them and the one in view; nothing opens again.
fn a_reload_keeps_everything_where_it_is(device: Device) {
    let mut session = start_plain("fancy-reload", device, "640x400");
    let (mut a, mut b, ida, idb) = two_workspaces(&mut session);
    let before = session.script("fancy_spaces", json!([]));

    session.send(Request::ScriptReload { path: None });
    session.script("fancy_setting", json!(["cold_brightness", 1.0]));
    let after = session.script("fancy_spaces", json!([]));
    assert_eq!(before, after);
    // The frame after the reload: b in view, as it was, not opening.
    let image = session.screenshot_png("reloaded");
    assert_rgb(&image, 320, 190, GREEN, "b in view");
    let at = drawn(&mut session, idb);
    assert_near(&at["x"], WHOLE.0, 0.0, "b");
    assert_near(&at["opacity"], 1.0, 0.0, "b");
    assert_near(&at["scale"], 1.0, 0.0, "b");
    assert_near(&drawn(&mut session, ida)["x"], WHOLE.0 - PITCH, 0.0, "a");
    // And the keys still work.
    logo_key(&mut session, KEY_1, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    let image = session.screenshot_png("switched");
    assert_rgb(&image, 320, 190, RED, "a in view");
    let script = session.script_state();
    assert_eq!(script["mode"], "running", "{script:#}");
    assert_eq!(script["failures"], 0, "{script:#}");
    assert_eq!(script["reloads"], 1, "{script:#}");
    session.quit();
}

/// The candidate window of an input method is drawn below the cursor of
/// the text field, with its window: also while the camera travels.
fn the_candidates_of_an_input_method_follow_their_window(device: Device) {
    const CANDIDATES: [u8; 4] = [240, 220, 40, 255];
    let mut session = start_plain("fancy-ime", device, "640x400");
    let mut ime = session.client(ClientOptions {
        window: false,
        input_method: Some(InputMethodMode::Manual),
        ..Default::default()
    });
    let (mut app, id) = session.mapped_client(ClientOptions {
        text_input: true,
        ..options("field", 200, 120, BLUE)
    });
    session.wait(&mut [&mut app, &mut ime], "an input method", |s| {
        s["input_method"]["bound"] == true
    });
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while !app.text_input_entered() {
        assert!(
            std::time::Instant::now() < deadline,
            "the field never got the text input"
        );
        app.pump(Some(Duration::from_millis(10))).unwrap();
    }
    // The cursor at (30, 60), 16 high: the candidates go below it.
    app.text_input_enable(Some((30, 60, 2, 16))).unwrap();
    session.wait(&mut [&mut app, &mut ime], "active", |s| {
        s["input_method"]["active"] == true
    });
    ime.ime_show_popup(60, 24, CANDIDATES).unwrap();
    let state = session.wait(&mut [&mut app, &mut ime], "the candidates are there", |s| {
        window(s, "field").is_some_and(|w| {
            w["surfaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["role"] == "input-popup" && n["has_contents"] == true)
        })
    });
    assert_eq!(state["input_method"]["popup"]["position"], json!([30, 76]));
    settle(&mut session, &mut [&mut app, &mut ime]);
    let image = session.screenshot_png("candidates");
    // The middle of the candidates: (30 + 30, 76 + 12) in the window.
    assert_rgb(
        &image,
        (WHOLE.0 + 60.0) as u32,
        (WHOLE.1 + 88.0) as u32,
        CANDIDATES,
        "the candidates",
    );
    assert_rgb(
        &image,
        (WHOLE.0 + 60.0) as u32,
        (WHOLE.1 + 60.0) as u32,
        BLUE,
        "the field above them",
    );

    // On the way into the overview (the window keeps the focus, so the
    // candidates stay) they are where the window is drawn.
    logo_key(&mut session, KEY_TAB, false);
    session.screenshot_png("leaving");
    advance(&mut session, 0.08);
    let image = session.screenshot_png("travelling");
    let w = drawn(&mut session, id);
    let scale = w["width"].as_f64().unwrap() / WHOLE.2;
    assert!(scale < 0.9, "the window is drawn at {scale}");
    let (x, y) = (w["x"].as_f64().unwrap(), w["y"].as_f64().unwrap());
    assert_rgb(
        &image,
        (x + 60.0 * scale).round() as u32,
        (y + 88.0 * scale).round() as u32,
        CANDIDATES,
        "the candidates, travelling",
    );
    session.assert_script_healthy();
    session.quit();
}

/// The everyday things with this project in charge: a menu is drawn and
/// hit where its window is drawn, also while the camera travels; a dialog
/// floats over its window; a window that asks to be fullscreen covers the
/// output, with its dialog still above it.
fn menus_dialogs_and_fullscreen_work(device: Device) {
    let mut session = start_plain("fancy-basics", device, "640x400");
    let (mut a, mut b, ida, _) = two_workspaces(&mut session);

    // A menu of a (on the first workspace), 90x60 at (250, 40) in a.
    logo_key(&mut session, KEY_1, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    a.open_popup(PopupOptions {
        x: 250,
        y: 40,
        width: 90,
        height: 60,
        color: SAND,
        grab: false,
        slide: false,
    })
    .unwrap();
    session.wait(&mut [&mut a, &mut b], "the menu is there", |s| {
        window(s, "a").is_some_and(|w| {
            w["surfaces"].as_array().is_some_and(|l| {
                l.iter()
                    .any(|x| x["role"] == "popup" && x["buffer_size"].is_array())
            })
        })
    });
    let image = session.screenshot_png("menu");
    assert_rgb(
        &image,
        (WHOLE.0 + 295.0) as u32,
        (WHOLE.1 + 70.0) as u32,
        SAND,
        "the menu",
    );
    let at = point_at(&mut session, &mut a, (WHOLE.0 + 295.0, WHOLE.1 + 70.0));
    assert_eq!(a.pointer_focus(), Some(SurfaceKind::Popup));
    assert!(
        (at.0 - 45.0).abs() < 0.02 && (at.1 - 30.0).abs() < 0.02,
        "{at:?}"
    );

    // While the camera travels to the second workspace, the menu is where
    // its window is drawn: smaller, and on its way out.
    logo_key(&mut session, KEY_1 + 1, false);
    session.screenshot_png("leaving");
    advance(&mut session, 0.06);
    session.screenshot_png("menu-travelling");
    let wa = drawn(&mut session, ida);
    let scale = wa["width"].as_f64().unwrap() / WHOLE.2;
    assert!(scale < 0.97, "a is drawn at {scale}");
    let (ax, ay) = (wa["x"].as_f64().unwrap(), wa["y"].as_f64().unwrap());
    let at = point_at(
        &mut session,
        &mut a,
        (ax + 295.0 * scale, ay + 70.0 * scale),
    );
    assert_eq!(a.pointer_focus(), Some(SurfaceKind::Popup));
    assert!(
        (at.0 - 45.0).abs() < 0.05 && (at.1 - 30.0).abs() < 0.05,
        "{at:?}"
    );
    a.close_popup();
    logo_key(&mut session, KEY_1, false);
    settle(&mut session, &mut [&mut a, &mut b]);

    // A dialog of a: over the middle of a, at the size it chose, with the
    // focus.
    a.open_dialog(DialogOptions {
        color: BLUE,
        ..Default::default()
    })
    .unwrap();
    session.wait(&mut [&mut a, &mut b], "the dialog is there", |s| {
        window(s, "dialog").is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
    });
    settle(&mut session, &mut [&mut a, &mut b]);
    let dialog = id_of(&mut session, "dialog");
    let wd = drawn(&mut session, dialog);
    assert_near(
        &wd["x"],
        WHOLE.0 + (WHOLE.2 - 120.0) / 2.0,
        0.0,
        "the dialog",
    );
    assert_near(
        &wd["y"],
        WHOLE.1 + (WHOLE.3 - 80.0) / 2.0,
        0.0,
        "the dialog",
    );
    assert_near(&wd["width"], 120.0, 0.0, "the dialog");
    assert_eq!(session.state()["focus"]["window"], dialog);
    let image = session.screenshot_png("dialog");
    assert_rgb(&image, 320, 189, BLUE, "the dialog over a");
    assert_rgb(&image, 100, 100, RED, "a around it");

    // a asks to be fullscreen: the whole output, square corners, and the
    // dialog still above it.
    a.request(ToplevelRequest::Fullscreen).unwrap();
    session.wait(&mut [&mut a, &mut b], "a is fullscreen", |s| {
        window(s, "a").is_some_and(|w| w["fullscreen"] == true && w["size"] == json!([640, 400]))
    });
    settle(&mut session, &mut [&mut a, &mut b]);
    let wa = drawn(&mut session, ida);
    assert_near(&wa["x"], 0.0, 0.0, "a, fullscreen");
    assert_near(&wa["width"], 640.0, 0.0, "a, fullscreen");
    assert_near(&wa["height"], 400.0, 0.0, "a, fullscreen");
    let image = session.screenshot_png("fullscreen");
    assert_rgb(&image, 1, 1, RED, "a's corner");
    assert_rgb(&image, 638, 398, RED, "a's other corner");
    assert_rgb(&image, 320, 189, BLUE, "the dialog over a");

    // And back to its tile.
    a.request(ToplevelRequest::Unfullscreen).unwrap();
    session.wait(&mut [&mut a, &mut b], "a is a tile again", |s| {
        window(s, "a").is_some_and(|w| w["fullscreen"] == false && w["size"] == json!([604, 342]))
    });
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_near(&drawn(&mut session, ida)["x"], WHOLE.0, 0.0, "a, a tile");
    assert_near(
        &drawn(&mut session, ida)["width"],
        WHOLE.2,
        0.0,
        "a, a tile",
    );
    session.assert_script_healthy();
    session.quit();
}

/// Three windows whose clients disconnect in one go: the script hears of
/// them one at a time, and must not name one that is gone already.
fn windows_that_go_together_are_all_forgotten(device: Device) {
    let mut session = start_plain("fancy-gone", device, "640x400");
    let mut a = open(&mut session, &mut [], "a", RED);
    let mut b = open(&mut session, &mut [&mut a], "b", GREEN);
    let c = open(&mut session, &mut [&mut a, &mut b], "c", BLUE);
    settle(&mut session, &mut [&mut a, &mut b]);
    drop(c);
    drop(b);
    drop(a);
    session.wait(&mut [], "all gone", |s| {
        s["windows"].as_array().is_some_and(|w| w.is_empty())
    });
    settle(&mut session, &mut []);
    let spaces = session.script("fancy_spaces", json!([]));
    assert_eq!(
        spaces["spaces"][0]["order"].as_array().map_or(0, Vec::len),
        0
    );
    // A new window is placed as the first one was.
    let mut d = open(&mut session, &mut [], "d", SAND);
    settle(&mut session, &mut [&mut d]);
    let idd = id_of(&mut session, "d");
    assert_near(&drawn(&mut session, idd)["width"], WHOLE.2, 0.0, "d");
    session.assert_script_healthy();
    session.quit();
}

/// The window with the focus is drawn as it is; the other one darker
/// (`cold_brightness`), in linear light.
fn a_window_without_the_focus_is_dimmer(device: Device) {
    let mut session = start("fancy-dim", device, "640x400");
    let mut a = open(&mut session, &mut [], "a", SAND);
    let mut b = open(&mut session, &mut [&mut a], "b", SAND);
    settle(&mut session, &mut [&mut a, &mut b]);
    let image = session.screenshot_png("two");
    // b has the focus: the right tile.
    assert_rgb(&image, 500, 190, SAND, "b, with the focus");
    let dimmed: Vec<u8> = (0..3)
        .map(|i| {
            let linear = (SAND[i] as f64 / 255.0).powf(2.2) * 0.72;
            (linear.powf(1.0 / 2.2) * 255.0).round() as u8
        })
        .collect();
    assert_rgb(
        &image,
        100,
        190,
        [dimmed[0], dimmed[1], dimmed[2], 255],
        "a, without it",
    );
    session.assert_script_healthy();
    session.quit();
}

// 640x400 with gaps of 18 and 22 left free below: the tiles share
// 604x342 at (18, 18). One workspace is 640 + 96 further along the strip.
const WHOLE: (f64, f64, f64, f64) = (18.0, 18.0, 604.0, 342.0);
const PITCH: f64 = 736.0;
// Two columns (the `columns` layout): 293 wide, at x 18 and 329.
const LEFT: f64 = 18.0;
const RIGHT: f64 = 329.0;
const COLUMN: f64 = 293.0;
// The camera's spring (`stage.settings.camera`): critically damped.
const CAMERA_OMEGA: f64 = 13.0;

/// A critically damped spring from 1 to 0, `t` seconds after it was aimed
/// from rest: how much of the way is left.
fn left_of_the_way(omega: f64, t: f64) -> f64 {
    (-omega * t).exp() * (1.0 + omega * t)
}

#[track_caller]
fn assert_near(actual: &Value, expected: f64, tolerance: f64, what: &str) {
    let actual = actual
        .as_f64()
        .unwrap_or_else(|| panic!("{what}: not a number"));
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual}, expected {expected}"
    );
}

#[track_caller]
fn assert_rgb(image: &EncodedImage, x: u32, y: u32, expected: [u8; 4], what: &str) {
    let actual = image.pixel(x, y);
    let worst = (0..3)
        .map(|i| (actual[i] as i32 - expected[i] as i32).abs())
        .max()
        .unwrap();
    assert!(
        worst <= 2,
        "{what}: pixel ({x}, {y}) is {actual:?}, expected {expected:?}"
    );
}

/// The id of the window with a title.
fn id_of(session: &mut Session, title: &str) -> u64 {
    window(&session.state(), title).expect("the window exists")["id"]
        .as_u64()
        .unwrap()
}

/// Where the last frame drew a window (`fancy_state`).
fn drawn(session: &mut Session, id: u64) -> Value {
    // An empty list arrives as an empty object.
    stage(session)["windows"]
        .as_array()
        .and_then(|windows| windows.iter().find(|w| w["id"] == id))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Once the stage is at rest, no more frames are drawn.
fn assert_idle(session: &mut Session, clients: &mut [&mut TestClient]) {
    settle(session, clients);
    // The calls into the script above ask for a frame each; those are drawn
    // first.
    session.wait(clients, "no frame owed", |s| s["redraw_pending"] == false);
    let frames = |session: &mut Session| session.state()["script"]["frames"].as_u64().unwrap();
    let before = frames(session);
    for _ in 0..10 {
        for client in clients.iter_mut() {
            client.pump(Some(Duration::from_millis(20))).unwrap();
        }
    }
    let state = session.state();
    assert_eq!(
        state["script"]["frames"].as_u64().unwrap(),
        before,
        "frames were drawn with nothing moving"
    );
    assert_eq!(state["redraw_pending"], false);
}

/// A window alone on the first workspace and one alone on the second, the
/// second in view, everything at rest.
fn two_workspaces(session: &mut Session) -> (TestClient, TestClient, u64, u64) {
    let mut a = open(session, &mut [], "a", RED);
    logo_key(session, KEY_1 + 1, false);
    let mut b = open(session, &mut [&mut a], "b", GREEN);
    settle(session, &mut [&mut a, &mut b]);
    let (ida, idb) = (id_of(session, "a"), id_of(session, "b"));
    let spaces = session.script("fancy_spaces", json!([]));
    assert_eq!(spaces["active"], 2);
    assert_eq!(spaces["spaces"][0]["order"], json!([ida]));
    assert_eq!(spaces["spaces"][1]["order"], json!([idb]));
    // b fills the screen's tile; a is one workspace to the left, out of
    // sight.
    let at = drawn(session, idb);
    assert_near(&at["x"], WHOLE.0, 1e-3, "b");
    assert_near(&at["width"], WHOLE.2, 1e-3, "b");
    let away = drawn(session, ida);
    assert_near(&away["x"], WHOLE.0 - PITCH, 1e-3, "a, a workspace away");
    assert_eq!(away["visible"], false);
    (a, b, ida, idb)
}

/// Logo+1 from the second workspace: the camera travels along the strip on
/// its spring, pulled back on the way, and stops exactly on the first
/// workspace. Then nothing more is drawn.
fn a_workspace_switch_travels_and_comes_to_rest(device: Device) {
    let mut session = start_plain("fancy-switch", device, "640x400");
    let (mut a, mut b, ida, idb) = two_workspaces(&mut session);
    let image = session.screenshot_png("before");
    assert_rgb(&image, 320, 190, GREEN, "b in view");

    logo_key(&mut session, KEY_1, false);
    // The frame at the time of the key: nothing has moved yet.
    let image = session.screenshot_png("start");
    assert_rgb(&image, 320, 190, GREEN, "b still in view");
    let state = stage(&mut session);
    assert_near(&state["camera"], 2.0, 1e-6, "the camera at the start");
    assert_near(&state["zoom"], 1.0, 1e-6, "the zoom at the start");
    assert_eq!(state["moving"], true);

    // A tenth of a second on: between the two, pulled back.
    advance(&mut session, 0.1);
    session.screenshot_png("travelling");
    let state = stage(&mut session);
    let camera = 1.0 + left_of_the_way(CAMERA_OMEGA, 0.1);
    assert_near(&state["camera"], camera, 1e-4, "the camera on its way");
    assert!(state["pull"].as_f64().unwrap() > 0.8, "{state:#}");
    assert!(state["zoom"].as_f64().unwrap() < 0.9, "{state:#}");
    let (wa, wb) = (drawn(&mut session, ida), drawn(&mut session, idb));
    assert_eq!(wa["visible"], true, "a has come into sight");
    let (ax, bx) = (wa["x"].as_f64().unwrap(), wb["x"].as_f64().unwrap());
    assert!(ax > WHOLE.0 - PITCH && ax < WHOLE.0, "a at {ax}");
    assert!(bx > WHOLE.0 && bx < WHOLE.0 + PITCH, "b at {bx}");
    // Both are drawn smaller than they are.
    assert!(wa["width"].as_f64().unwrap() < WHOLE.2 * 0.9);

    // At rest: exactly on the first workspace, 1:1.
    settle(&mut session, &mut [&mut a, &mut b]);
    let image = session.screenshot_png("arrived");
    assert_rgb(&image, 320, 190, RED, "a in view");
    assert_rgb(&image, 30, 30, RED, "a's corner region");
    let state = stage(&mut session);
    assert_near(&state["camera"], 1.0, 0.0, "the camera at rest");
    assert_near(&state["zoom"], 1.0, 0.0, "the zoom at rest");
    let wa = drawn(&mut session, ida);
    assert_near(&wa["x"], WHOLE.0, 0.0, "a at rest");
    assert_near(&wa["y"], WHOLE.1, 0.0, "a at rest");
    assert_near(&wa["width"], WHOLE.2, 0.0, "a at rest");
    assert_near(&wa["height"], WHOLE.3, 0.0, "a at rest");
    assert_eq!(drawn(&mut session, idb)["visible"], false);
    // The keyboard is a's now.
    assert_eq!(window(&session.state(), "a").unwrap()["activated"], true);
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// A switch back in the middle of a switch: the camera does not stop, let
/// alone jump. It carries on a little the way it was going, turns, and
/// comes back.
fn a_switch_turned_half_way_keeps_its_speed(device: Device) {
    let mut session = start_plain("fancy-turn", device, "640x400");
    let (mut a, mut b, _, _) = two_workspaces(&mut session);

    logo_key(&mut session, KEY_1, false);
    session.screenshot_png("start");
    advance(&mut session, 0.06);
    session.screenshot_png("on-the-way");
    let before = stage(&mut session)["camera"].as_f64().unwrap();
    assert_near(
        &json!(before),
        1.0 + left_of_the_way(CAMERA_OMEGA, 0.06),
        1e-4,
        "the camera on its way",
    );

    // Back to the second workspace.
    logo_key(&mut session, KEY_1 + 1, false);
    session.screenshot_png("turned");
    assert_near(
        &stage(&mut session)["camera"],
        before,
        1e-6,
        "the camera right after the turn",
    );
    // It was moving towards 1, and still is for a moment.
    advance(&mut session, 0.02);
    session.screenshot_png("carrying-on");
    let after = stage(&mut session)["camera"].as_f64().unwrap();
    assert!(
        after < before,
        "the camera stopped dead: {before} -> {after}"
    );
    // Then it turns and arrives where it came from.
    advance(&mut session, 0.15);
    session.screenshot_png("coming-back");
    let back = stage(&mut session)["camera"].as_f64().unwrap();
    assert!(back > after && back < 2.0, "the camera at {back}");
    settle(&mut session, &mut [&mut a, &mut b]);
    let image = session.screenshot_png("back");
    assert_rgb(&image, 320, 190, GREEN, "b in view again");
    assert_near(&stage(&mut session)["camera"], 2.0, 0.0, "the camera");
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.quit();
}

/// Moves the pointer and returns where the client was last told it is.
fn point_at(session: &mut Session, client: &mut TestClient, (x, y): (f64, f64)) -> (f64, f64) {
    client.clear_events();
    session.send(Request::PointerMotion { x, y });
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerFrame)
        .unwrap()
        .expect("a pointer frame");
    client
        .events()
        .iter()
        .rev()
        .find_map(|e| match e {
            ClientEvent::PointerEnter { x, y } | ClientEvent::PointerMotion { x, y } => {
                Some((*x, *y))
            }
            _ => None,
        })
        .expect("the pointer reached the client")
}

/// Logo+shift+H swaps the focused window with the one before it. Both move
/// on springs; the pointer finds each where it is drawn on the way.
fn a_swap_moves_the_windows_and_input_follows(device: Device) {
    let mut session = start_plain("fancy-swap", device, "640x400");
    let mut a = open(&mut session, &mut [], "a", RED);
    let mut b = open(&mut session, &mut [&mut a], "b", GREEN);
    // Side by side, the same size.
    logo_key(&mut session, KEY_SPACE, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    let (ida, idb) = (id_of(&mut session, "a"), id_of(&mut session, "b"));
    assert_near(&drawn(&mut session, ida)["x"], LEFT, 0.0, "a");
    assert_near(&drawn(&mut session, idb)["x"], RIGHT, 0.0, "b");
    assert_near(&drawn(&mut session, idb)["width"], COLUMN, 0.0, "b");
    // The focus is b's: it is hot, a is not.
    assert_near(&drawn(&mut session, idb)["heat"], 1.0, 0.0, "b's heat");
    assert_near(&drawn(&mut session, ida)["heat"], 0.0, 0.0, "a's heat");

    logo_key(&mut session, KEY_H, true);
    let image = session.screenshot_png("start");
    assert_rgb(&image, 100, 190, RED, "a where it was");
    assert_rgb(&image, 500, 190, GREEN, "b where it was");

    advance(&mut session, 0.08);
    let image = session.screenshot_png("middle");
    let ax = drawn(&mut session, ida)["x"].as_f64().unwrap();
    let bx = drawn(&mut session, idb)["x"].as_f64().unwrap();
    assert!(ax > LEFT + 40.0 && ax < RIGHT - 40.0, "a at {ax}");
    assert!(bx > LEFT + 40.0 && bx < RIGHT - 40.0, "b at {bx}");
    assert_near(&json!(ax + bx), LEFT + RIGHT, 1e-3, "they pass each other");
    // b, which has the focus, is drawn above a where they overlap.
    assert_rgb(&image, 320, 190, GREEN, "b over a");
    // Input lands where they are drawn.
    let at = point_at(&mut session, &mut a, (ax + 10.0, 100.0));
    assert!(
        (at.0 - 10.0).abs() < 0.02 && (at.1 - (100.0 - WHOLE.1)).abs() < 0.02,
        "a was told {at:?}"
    );
    let at = point_at(&mut session, &mut b, (bx + 200.0, 300.0));
    assert!(
        (at.0 - 200.0).abs() < 0.02 && (at.1 - (300.0 - WHOLE.1)).abs() < 0.02,
        "b was told {at:?}"
    );

    settle(&mut session, &mut [&mut a, &mut b]);
    let image = session.screenshot_png("end");
    assert_rgb(&image, 100, 190, GREEN, "b on the left");
    assert_rgb(&image, 500, 190, RED, "a on the right");
    assert_near(&drawn(&mut session, ida)["x"], RIGHT, 0.0, "a at rest");
    assert_near(&drawn(&mut session, idb)["x"], LEFT, 0.0, "b at rest");
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.quit();
}

/// A window that opens grows in and becomes opaque; one that closes is
/// still drawn, fading, after its client destroyed it, and is then let go.
fn windows_open_and_close_over_time(device: Device) {
    let mut session = start_plain("fancy-open-close", device, "640x400");
    let mut a = open(&mut session, &mut [], "a", RED);
    let ida = id_of(&mut session, "a");
    session.screenshot_png("opening");
    let opening = drawn(&mut session, ida);
    assert_near(&opening["opacity"], 0.0, 1e-6, "a when it opens");
    assert_near(&opening["scale"], 0.9, 1e-6, "a when it opens");
    advance(&mut session, 0.05);
    session.screenshot_png("opening-more");
    let opening = drawn(&mut session, ida);
    let opacity = opening["opacity"].as_f64().unwrap();
    assert!(opacity > 0.1 && opacity < 0.9, "a's opacity is {opacity}");
    settle(&mut session, &mut [&mut a]);
    let open_now = drawn(&mut session, ida);
    assert_near(&open_now["opacity"], 1.0, 0.0, "a once open");
    assert_near(&open_now["scale"], 1.0, 0.0, "a once open");
    let image = session.screenshot_png("open");
    assert_rgb(&image, 320, 190, RED, "a");

    logo_key(&mut session, KEY_Q, false);
    a.wait_for(common::TIMEOUT, |e| *e == ClientEvent::CloseRequested)
        .unwrap()
        .expect("a is asked to close");
    let state = session.wait(&mut [&mut a], "a gone", |s| window(s, "a").is_none());
    assert_eq!(
        state["script"]["closed_windows"].as_array().unwrap().len(),
        1
    );
    let image = session.screenshot_png("closed");
    assert_rgb(&image, 320, 190, RED, "what a last showed");
    let closing = drawn(&mut session, ida);
    assert_eq!(closing["closing"], true);
    assert_near(&closing["opacity"], 1.0, 1e-6, "a right after closing");
    advance(&mut session, 0.05);
    session.screenshot_png("fading");
    let opacity = drawn(&mut session, ida)["opacity"].as_f64().unwrap();
    assert!(opacity > 0.1 && opacity < 0.9, "a's opacity is {opacity}");
    settle(&mut session, &mut []);
    assert_eq!(drawn(&mut session, ida), Value::Null);
    assert_eq!(
        session.state()["script"]["closed_windows"],
        json!([]),
        "the closed window is let go"
    );
    assert_idle(&mut session, &mut []);
    session.assert_script_healthy();
    session.quit();
}

/// Logo+Tab pulls the camera back until the workspaces lie side by side. A
/// click on another workspace goes there; the windows get no input
/// meanwhile.
fn the_overview_shows_the_strip_and_a_click_chooses(device: Device) {
    let mut session = start_plain("fancy-overview", device, "640x400");
    let (mut a, mut b, ida, idb) = two_workspaces(&mut session);

    logo_key(&mut session, KEY_TAB, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    session.screenshot_png("overview");
    let state = stage(&mut session);
    assert_near(&state["overview"], 1.0, 0.0, "the overview");
    // Five workspaces in two rows, three in the upper one: together they
    // take 0.94 of the width, with gaps of 0.022 of it.
    let zoom = (640.0 * 0.94 - 2.0 * 640.0 * 0.022) / (3.0 * 640.0);
    assert_near(&state["zoom"], zoom, 1e-9, "the zoom");
    let (wa, wb) = (drawn(&mut session, ida), drawn(&mut session, idb));
    assert_eq!(wa["visible"], true);
    assert_eq!(wb["visible"], true);
    assert_near(&wb["width"], WHOLE.2 * zoom, 1e-6, "b, small");
    // a's workspace lies to the left of b's, in the same row.
    let (ax, bx) = (wa["x"].as_f64().unwrap(), wb["x"].as_f64().unwrap());
    assert_near(
        &json!(bx - ax),
        640.0 * zoom + 640.0 * 0.022,
        1e-6,
        "a workspace apart",
    );
    assert_near(&wa["y"], wb["y"].as_f64().unwrap(), 1e-6, "the same row");
    let ay = wa["y"].as_f64().unwrap();

    // The pointer over a: the client is not told.
    a.clear_events();
    session.send(Request::PointerMotion {
        x: ax + 50.0,
        y: ay + 20.0,
    });
    session.wait(&mut [&mut a, &mut b], "clients served", |_| true);
    assert!(
        !a.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerEnter { .. })),
        "a got the pointer in the overview"
    );
    // A click there goes to a's workspace and gives a the focus.
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    settle(&mut session, &mut [&mut a, &mut b]);
    let state = stage(&mut session);
    assert_near(&state["overview"], 0.0, 0.0, "the overview is left");
    assert_near(&state["camera"], 1.0, 0.0, "the camera");
    assert_eq!(session.script("fancy_spaces", json!([]))["active"], 1);
    assert_eq!(window(&session.state(), "a").unwrap()["activated"], true);
    let image = session.screenshot_png("chosen");
    assert_rgb(&image, 320, 190, RED, "a in view");
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.quit();
}

/// With the logo key held, the left button picks a window up: the client
/// sees neither the button nor the pointer any more, the window follows the
/// pointer, and changes places with the tile it is carried over. Released,
/// it settles into its new place.
fn a_window_is_carried_with_the_pointer(device: Device) {
    let mut session = start_plain("fancy-carry", device, "640x400");
    let mut a = open(&mut session, &mut [], "a", RED);
    let mut b = open(&mut session, &mut [&mut a], "b", GREEN);
    logo_key(&mut session, KEY_SPACE, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    let (ida, idb) = (id_of(&mut session, "a"), id_of(&mut session, "b"));

    // The pointer on a, 82 from its left edge.
    point_at(&mut session, &mut a, (100.0, 190.0));
    a.clear_events();
    key(&mut session, KEY_LEFTMETA, true);
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    a.wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerLeave)
        .unwrap()
        .expect("the pointer leaves a");
    assert!(
        !a.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { .. })),
        "a saw the button"
    );
    // a has the focus now.
    assert_eq!(session.state()["focus"]["window"], ida);

    // Carried to the right, over b's tile: they change places, and a is
    // drawn under the pointer, a little smaller, above b.
    session.send(Request::PointerMotion { x: 500.0, y: 200.0 });
    advance(&mut session, 0.5);
    let image = session.screenshot_png("carried");
    let spaces = session.script("fancy_spaces", json!([]));
    assert_eq!(spaces["spaces"][0]["order"], json!([idb, ida]));
    let carried = drawn(&mut session, ida);
    assert_near(&carried["scale"], 0.88, 1e-3, "a in hand");
    assert_rgb(&image, 500, 200, RED, "a under the pointer");
    // The point of a that was picked up is still under the pointer.
    let scale = carried["scale"].as_f64().unwrap();
    let grabbed = carried["x"].as_f64().unwrap() + (100.0 - LEFT) * scale;
    assert_near(&json!(grabbed), 500.0, 1e-6, "the grabbed point");
    // b is on its way to (or at) the left tile.
    assert!(drawn(&mut session, idb)["x"].as_f64().unwrap() < RIGHT - 100.0);
    assert!(
        !a.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerMotion { .. })),
        "a saw the pointer move while it was carried"
    );

    // Dropped: a settles on the right tile, and gets the pointer back.
    a.clear_events();
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    key(&mut session, KEY_LEFTMETA, false);
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_near(&drawn(&mut session, ida)["x"], RIGHT, 0.0, "a at rest");
    assert_near(&drawn(&mut session, ida)["scale"], 1.0, 0.0, "a at rest");
    assert_near(&drawn(&mut session, idb)["x"], LEFT, 0.0, "b at rest");
    let image = session.screenshot_png("dropped");
    assert_rgb(&image, 100, 190, GREEN, "b on the left");
    assert_rgb(&image, 500, 190, RED, "a on the right");
    assert!(
        a.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerEnter { .. })),
        "a did not get the pointer back"
    );
    assert!(
        !a.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { .. })),
        "a saw the release"
    );
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.quit();
}

/// Three fingers on the touchpad move the camera as far as they travel;
/// lifted, it settles on the nearest workspace.
fn a_swipe_carries_the_camera(device: Device) {
    let mut session = start_plain("fancy-swipe", device, "640x400");
    let (mut a, mut b, _, _) = two_workspaces(&mut session);
    let swipe = || "swipe".to_string();

    session.send(Request::GestureBegin {
        kind: swipe(),
        fingers: 3,
    });
    // 420 logical pixels are one workspace: 252 to the right bring the
    // workspace on the left 0.6 of the way in.
    session.send(Request::GestureUpdate {
        kind: swipe(),
        dx: 252.0,
        dy: 0.0,
        scale: 1.0,
        rotation: 0.0,
    });
    session.screenshot_png("held");
    let state = stage(&mut session);
    assert_near(&state["camera"], 1.4, 1e-9, "the camera under the fingers");
    assert!(state["zoom"].as_f64().unwrap() < 0.9, "{state:#}");
    // It stays there for as long as the fingers do.
    advance(&mut session, 1.0);
    session.screenshot_png("still-held");
    assert_near(&stage(&mut session)["camera"], 1.4, 1e-9, "the camera");

    session.send(Request::GestureEnd {
        kind: swipe(),
        cancelled: false,
    });
    assert_eq!(session.script("fancy_spaces", json!([]))["active"], 1);
    settle(&mut session, &mut [&mut a, &mut b]);
    assert_near(
        &stage(&mut session)["camera"],
        1.0,
        0.0,
        "the camera at rest",
    );
    let image = session.screenshot_png("arrived");
    assert_rgb(&image, 320, 190, RED, "a in view");
    assert_eq!(window(&session.state(), "a").unwrap()["activated"], true);
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.quit();
}

/// Pictures of the main scenes, for looking at. Not a test of anything but
/// that the scenes can be drawn: run by hand,
///
/// ```sh
/// cargo test -p proto-compositor --test fancy -- --ignored gallery --nocapture
/// ```
///
/// and look at `target/test-output/fancy-gallery-*/`.
#[test]
#[ignore = "pictures to look at, made by hand"]
fn gallery() {
    let device = Device::new(common::Gpu::Lavapipe);
    let mut session = start("fancy-gallery", device, "1280x800");
    let mut a = open(&mut session, &mut [], "a", SLATE);
    let mut b = open(&mut session, &mut [&mut a], "b", SAND);
    advance(&mut session, 0.05);
    session.screenshot_png("01-opening");
    let mut c = open(&mut session, &mut [&mut a, &mut b], "c", BLUE);
    settle(&mut session, &mut [&mut a, &mut b, &mut c]);
    session.screenshot_png("02-tiles");

    // A second workspace with two windows.
    logo_key(&mut session, KEY_1 + 1, false);
    let mut d = open(&mut session, &mut [&mut a, &mut b, &mut c], "d", GREEN);
    let mut e = open(
        &mut session,
        &mut [&mut a, &mut b, &mut c, &mut d],
        "e",
        RED,
    );
    settle(&mut session, &mut [&mut a, &mut b, &mut c, &mut d, &mut e]);
    session.screenshot_png("03-second");

    // Back to the first: the camera travels.
    logo_key(&mut session, KEY_1, false);
    for (index, step) in [0.04, 0.05, 0.05, 0.06, 0.08, 0.12].into_iter().enumerate() {
        advance(&mut session, step);
        session.screenshot_png(&format!("04-switch-{index}"));
    }
    settle(&mut session, &mut [&mut a, &mut b, &mut c, &mut d, &mut e]);

    // A swap, half way.
    logo_key(&mut session, KEY_H, true);
    advance(&mut session, 0.07);
    session.screenshot_png("05-swap");
    settle(&mut session, &mut [&mut a, &mut b, &mut c, &mut d, &mut e]);

    // The overview.
    logo_key(&mut session, KEY_TAB, false);
    advance(&mut session, 0.12);
    session.screenshot_png("06-overview-opening");
    settle(&mut session, &mut [&mut a, &mut b, &mut c, &mut d, &mut e]);
    session.screenshot_png("07-overview");
    logo_key(&mut session, KEY_TAB, false);
    settle(&mut session, &mut [&mut a, &mut b, &mut c, &mut d, &mut e]);

    // Closing a window.
    logo_key(&mut session, KEY_Q, false);
    session.wait(
        &mut [&mut a, &mut b, &mut c, &mut d, &mut e],
        "closed",
        |s| s["windows"].as_array().unwrap().len() == 4,
    );
    advance(&mut session, 0.05);
    session.screenshot_png("08-closing");
    settle(&mut session, &mut [&mut a, &mut b, &mut c, &mut d, &mut e]);
    session.screenshot_png("09-closed");

    session.assert_script_healthy();
    println!("pictures in {}", session.output_dir.display());
    session.quit();
}

/// Waits until `count` windows have contents.
fn wait_windows(session: &mut Session, count: usize) {
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    loop {
        let state = session.state();
        let shown = state["windows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|w| w["surfaces"][0]["buffer_size"].is_array())
            .count();
        if shown >= count {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {count} windows: {state:#}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Lets time pass until nothing moves (no test clients to pump).
fn rest(session: &mut Session) {
    // The springs are functions of time: a long step is as good as many
    // short ones. The terminals need real time to draw at a new size.
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(250));
        advance(session, 10.0);
        session.screenshot_png("settle");
        if stage(session)["moving"] == false {
            return;
        }
    }
    panic!("the stage never came to rest: {:#}", stage(session));
}

/// Like `gallery`, with real terminals (`foot`) as the windows.
#[test]
#[ignore = "pictures to look at, made by hand"]
fn gallery_terminals() {
    if !common::real_client_wanted("foot", &["foot"]) {
        return;
    }
    let device = Device::new(common::Gpu::Lavapipe);
    let mut session = start("fancy-terminals", device, "1920x1080");
    let list = "ls -la --color=always";
    let scripts = [
        format!("{list} /; exec sleep 1000"),
        format!(
            "head -n 44 {}; exec sleep 1000",
            project().join("main.luau").display()
        ),
        format!(
            "for r in 1 2 3; do for i in 0 1 2 3 4 5 6 7; do printf '\\033[4%sm      \\033[0m' $i; done; echo; done; echo; {list} /usr; exec sleep 1000"
        ),
        format!(
            "head -n 30 {}; exec sleep 1000",
            project().join("spring.luau").display()
        ),
        format!("{list} /usr; exec sleep 1000"),
    ];
    let mut terminals = Vec::new();
    for (index, script) in scripts.iter().take(3).enumerate() {
        // One with a background that lets the wall through.
        let options: &[&str] = if index == 2 {
            &["-o", "colors-dark.alpha=0.75"]
        } else {
            &[]
        };
        terminals.push(spawn_foot(
            &session,
            &format!("foot-{index}"),
            options,
            script,
        ));
        wait_windows(&mut session, index + 1);
        // Each opens a little after the one before.
        advance(&mut session, 0.5);
    }
    rest(&mut session);
    session.screenshot_png("01-terminals");

    logo_key(&mut session, KEY_1 + 1, false);
    advance(&mut session, 2.0);
    for (index, script) in scripts.iter().enumerate().skip(3) {
        terminals.push(spawn_foot(&session, &format!("foot-{index}"), &[], script));
        wait_windows(&mut session, index + 1);
        advance(&mut session, 0.5);
    }
    rest(&mut session);
    session.screenshot_png("02-second");

    logo_key(&mut session, KEY_1, false);
    for (index, step) in [0.05, 0.05, 0.05, 0.07, 0.10].into_iter().enumerate() {
        advance(&mut session, step);
        session.screenshot_png(&format!("03-switch-{index}"));
    }
    rest(&mut session);

    // A new terminal opening.
    terminals.push(spawn_foot(&session, "foot-late", &[], &scripts[4]));
    wait_windows(&mut session, 6);
    advance(&mut session, 0.06);
    std::thread::sleep(Duration::from_millis(200));
    session.screenshot_png("04-opening");
    rest(&mut session);
    session.screenshot_png("05-four");

    logo_key(&mut session, KEY_TAB, false);
    advance(&mut session, 0.10);
    session.screenshot_png("06-overview-opening");
    rest(&mut session);
    session.screenshot_png("07-overview");
    logo_key(&mut session, KEY_TAB, false);
    rest(&mut session);

    // A terminal picked up with the pointer and carried over another.
    session.send(Request::PointerMotion { x: 500.0, y: 400.0 });
    key(&mut session, KEY_LEFTMETA, true);
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    session.send(Request::PointerMotion { x: 900.0, y: 520.0 });
    advance(&mut session, 0.5);
    session.screenshot_png("08-carried");
    session.send(Request::PointerMotion {
        x: 1500.0,
        y: 300.0,
    });
    advance(&mut session, 0.09);
    session.screenshot_png("09-carried-over");
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    key(&mut session, KEY_LEFTMETA, false);
    advance(&mut session, 0.07);
    session.screenshot_png("10-dropped");
    rest(&mut session);

    // On to a workspace with nothing on it.
    logo_key(&mut session, KEY_1 + 2, false);
    advance(&mut session, 0.11);
    session.screenshot_png("11-to-empty");
    rest(&mut session);
    session.screenshot_png("12-empty");

    // Back, and the focused terminal sent to the second workspace: it
    // flies off along the strip while the others close the gap.
    logo_key(&mut session, KEY_1, false);
    rest(&mut session);
    logo_key(&mut session, KEY_1 + 1, true);
    advance(&mut session, 0.09);
    session.screenshot_png("13-sent");
    rest(&mut session);
    session.screenshot_png("14-after-sending");
    // And one taken along to the second workspace (logo+ctrl+shift+L).
    key(&mut session, KEY_LEFTMETA, true);
    key(&mut session, KEY_LEFTCTRL, true);
    key(&mut session, KEY_LEFTSHIFT, true);
    key(&mut session, KEY_L, true);
    key(&mut session, KEY_L, false);
    key(&mut session, KEY_LEFTSHIFT, false);
    key(&mut session, KEY_LEFTCTRL, false);
    key(&mut session, KEY_LEFTMETA, false);
    advance(&mut session, 0.1);
    session.screenshot_png("15-taken-along");
    rest(&mut session);
    session.screenshot_png("16-arrived");

    session.assert_script_healthy();
    println!("pictures in {}", session.output_dir.display());
    drop(terminals);
    session.quit();
}

/// A few of the pictures on an output of scale 1.5 (1920x1200 pixels,
/// 1280x800 logical): the terminals draw at that scale, and everything of
/// this project is in logical units.
#[test]
#[ignore = "pictures to look at, made by hand"]
fn gallery_scaled() {
    if !common::real_client_wanted("foot", &["foot"]) {
        return;
    }
    let device = Device::new(common::Gpu::Lavapipe);
    let mut session = start_with(
        "fancy-scaled",
        device,
        "1920x1200",
        &["--output-scale", "1.5"],
    );
    let mut terminals = Vec::new();
    for index in 0..3 {
        terminals.push(spawn_foot(
            &session,
            &format!("foot-{index}"),
            &[],
            "ls -la --color=always /usr; exec sleep 1000",
        ));
        wait_windows(&mut session, index + 1);
        advance(&mut session, 0.5);
    }
    rest(&mut session);
    session.screenshot_png("01-tiles");
    logo_key(&mut session, KEY_1 + 1, false);
    advance(&mut session, 0.1);
    session.screenshot_png("02-switch");
    rest(&mut session);
    logo_key(&mut session, KEY_TAB, false);
    rest(&mut session);
    session.screenshot_png("03-overview");
    session.assert_script_healthy();
    println!("pictures in {}", session.output_dir.display());
    drop(terminals);
    session.quit();
}

/// What a frame costs the script with eight windows on two workspaces,
/// all of them in sight and moving (the clock stands still half way
/// through a switch): the work of `stage.frame` alone (`fancy_bench`, no
/// drawing), and the whole `frame` callback (which also records the
/// drawing). Not a test: run by hand, optimized, on a quiet machine:
///
/// ```sh
/// cargo test --release -p proto-compositor --test fancy -- --ignored fancy_cost --nocapture
/// ```
#[test]
#[ignore = "a measurement, run by hand"]
fn fancy_cost() {
    use std::time::Instant;

    let device = Device::new(common::Gpu::Lavapipe);
    let mut session = start("fancy-cost", device, "1920x1080");
    let mut clients: Vec<TestClient> = Vec::new();
    for index in 0..8 {
        if index == 4 {
            logo_key(&mut session, KEY_1 + 1, false);
        }
        let title = format!("w{index}");
        let mut client = session.client(options(&title, 200, 120, SLATE));
        session.wait(&mut [&mut client], "mapped", |s| {
            window(s, &title).is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
        });
        clients.push(client);
    }
    {
        let mut all: Vec<&mut TestClient> = clients.iter_mut().collect();
        settle(&mut session, &mut all);
    }
    logo_key(&mut session, KEY_1, false);
    advance(&mut session, 0.1);
    session.screenshot_png("measured");
    let state = stage(&mut session);
    assert_eq!(state["moving"], true);
    let visible = state["windows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["visible"] == true)
        .count();

    let call = |session: &mut Session, rounds: u32| {
        let started = Instant::now();
        session.script("fancy_bench", json!([rounds]));
        started.elapsed().as_secs_f64() * 1e6
    };
    let rounds = 2000;
    let empty = (0..5)
        .map(|_| call(&mut session, 0))
        .fold(f64::MAX, f64::min);
    let full = (0..5)
        .map(|_| call(&mut session, rounds))
        .fold(f64::MAX, f64::min);
    let alone = (full - empty) / rounds as f64;

    let timing = |session: &mut Session| {
        let t = session.script_state()["timing"]["frame"].take();
        let calls = t["calls"].as_f64().unwrap();
        (calls, t["mean_us"].as_f64().unwrap() * calls)
    };
    let (calls0, total0) = timing(&mut session);
    for _ in 0..50 {
        for client in clients.iter_mut() {
            client.pump(Some(Duration::from_millis(0))).unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (calls1, total1) = timing(&mut session);
    let whole = (total1 - total0) / (calls1 - calls0).max(1.0);
    println!(
        "8 windows ({visible} in sight): stage.frame alone {alone:.1} us, the whole frame callback {whole:.1} us over {} frames",
        calls1 - calls0
    );
    drop(clients);
    session.quit();
}

/// The project as a window of another compositor, on a hardware device:
/// eight terminals on two workspaces, switching back and forth and opening
/// the overview in real time, counting the frames that reach the parent.
/// The keys go through the parent, with Alt standing in for the logo key
/// (`--nested-logo-key alt`). Not a test: run by hand, for a short while,
///
/// ```sh
/// HIZUKURI_TEST_DEVICES=gpu cargo test -p proto-compositor --test fancy -- --ignored nested_on_a_hardware_device --nocapture
/// ```
#[test]
#[ignore = "a measurement on a hardware device, run by hand"]
fn nested_on_a_hardware_device() {
    use std::time::Instant;

    const KEY_LEFTALT: u32 = 56;
    let device = Device::new(common::Gpu::Hardware);
    assert!(device.enabled(), "needs gpu in HIZUKURI_TEST_DEVICES");
    let drivers = std::env::var("HIZUKURI_ICD_DIRS").expect("HIZUKURI_ICD_DIRS names the drivers");
    let project = project();
    common::build_shaders(&project);
    let mut parent = Session::start(
        "fancy-nested-parent",
        device,
        &[
            "--output-size",
            "1920x1080",
            "--dmabuf",
            "gpu",
            "--vulkan-icd",
            &drivers,
        ],
    );
    let mut nested = Session::start_nested(
        "fancy-nested",
        device,
        &parent,
        &[
            "--output-size",
            "1920x1080",
            "--dmabuf",
            "gpu",
            "--vulkan-icd",
            &drivers,
            "--nested-logo-key",
            "alt",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    parent.wait(&mut [], "the window of the nested compositor", |s| {
        s["windows"]
            .as_array()
            .is_some_and(|w| w.iter().any(|w| w["mapped"] == true))
    });
    // A key with Alt held in the parent: logo and the key inside.
    let alt_key = |parent: &mut Session, keycode: u32| {
        for (code, pressed) in [
            (KEY_LEFTALT, true),
            (keycode, true),
            (keycode, false),
            (KEY_LEFTALT, false),
        ] {
            parent.send(Request::Key {
                keycode: code,
                pressed,
            });
        }
    };

    let mut terminals = Vec::new();
    for index in 0..8 {
        if index == 4 {
            alt_key(&mut parent, KEY_1 + 1);
            std::thread::sleep(Duration::from_millis(200));
            let spaces = nested.script("fancy_spaces", json!([]));
            assert_eq!(spaces["active"], 2, "Alt+2 in the parent switched inside");
        }
        terminals.push(spawn_foot(
            &nested,
            &format!("foot-{index}"),
            &[],
            "ls -la --color=always /usr; exec sleep 1000",
        ));
        wait_windows(&mut nested, index + 1);
    }
    std::thread::sleep(Duration::from_millis(1500));

    let presented = |nested: &mut Session| {
        nested.state()["backend_status"]["output"]["frames_presented"]
            .as_u64()
            .unwrap_or(0)
    };
    let frame_timing = |nested: &mut Session| {
        let t = nested.script_state()["timing"]["frame"].take();
        (
            t["calls"].as_f64().unwrap(),
            t["mean_us"].as_f64().unwrap(),
            t["max_us"].as_f64().unwrap(),
        )
    };
    // Six seconds of switching every 400 ms, the overview in between.
    let (frames0, (calls0, mean0, _)) = (presented(&mut nested), frame_timing(&mut nested));
    let started = Instant::now();
    let mut step = 0;
    let mut per_step = Vec::new();
    while started.elapsed() < Duration::from_secs(6) {
        let keycode = match step % 6 {
            0 | 2 => KEY_1,
            1 | 3 => KEY_1 + 1,
            _ => KEY_TAB,
        };
        alt_key(&mut parent, keycode);
        step += 1;
        std::thread::sleep(Duration::from_millis(400));
        per_step.push((
            started.elapsed().as_millis(),
            presented(&mut nested),
            stage(&mut nested)["moving"] == true,
        ));
    }
    println!("after each step (ms, frames, moving): {per_step:?}");
    let seconds = started.elapsed().as_secs_f64();
    let (frames1, (calls1, mean1, max1)) = (presented(&mut nested), frame_timing(&mut nested));
    let state = nested.state();
    println!(
        "nested, hardware device, 8 terminals: {:.1} frames per second reached the parent over {seconds:.1} s ({} frames); \
         the frame callback of the script took {:.0} us on average over {} calls (longest since the start: {max1:.0} us)",
        (frames1 - frames0) as f64 / seconds,
        frames1 - frames0,
        (mean1 * calls1 - mean0 * calls0) / (calls1 - calls0).max(1.0),
        calls1 - calls0,
    );
    println!(
        "frame times of the nested compositor (ms): {}",
        state["frame_time_ms"]
    );
    // Pictures of the parent, which holds it up for a while: not part of
    // what is measured.
    alt_key(&mut parent, KEY_1 + 1);
    std::thread::sleep(Duration::from_millis(110));
    parent.screenshot_png("switching");
    alt_key(&mut parent, KEY_TAB);
    std::thread::sleep(Duration::from_millis(900));
    parent.screenshot_png("overview");
    alt_key(&mut parent, KEY_TAB);
    std::thread::sleep(Duration::from_millis(900));
    parent.screenshot_png("tiles");
    // With the fire kept moving and nothing else going on: every frame is
    // drawn, and nothing is asked of either compositor meanwhile.
    nested.script("fancy_setting", json!(["stir_time", 1000.0]));
    alt_key(&mut parent, KEY_1);
    std::thread::sleep(Duration::from_millis(1500));
    let (frames0, started) = (presented(&mut nested), Instant::now());
    std::thread::sleep(Duration::from_secs(3));
    let (frames1, seconds) = (presented(&mut nested), started.elapsed().as_secs_f64());
    println!(
        "left alone, drawing every frame: {:.1} frames per second",
        (frames1 - frames0) as f64 / seconds
    );
    let parent_state = parent.state();
    println!(
        "frame times of the parent (ms): {}",
        parent_state["frame_time_ms"]
    );
    println!("gpu: {}", state["gpu"]);
    println!("backend: {}", state["backend_status"]);
    nested.assert_script_healthy();
    println!("pictures in {}", parent.output_dir.display());
    drop(terminals);
    nested.quit();
    parent.quit();
}
