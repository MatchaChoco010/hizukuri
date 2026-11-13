//! The room (`projects/room`): monitors in a 3D scene that show windows.
//!
//! The sessions run on a manual clock (`--clock manual`), so the camera and
//! the pictures of the monitors are those of an exact time.

mod common;

use std::path::PathBuf;

use common::{Device, Session, options, window};
use proto_compositor::control::Request;
use proto_compositor::image::EncodedImage;
use proto_test_client::{
    ClientEvent, ClientOptions, InputMethodMode, Pattern, PopupPositioner, TestClient,
};
use serde_json::{Value, json};

device_tests!(
    windows_take_monitors_and_tab_chooses_among_the_waiting,
    the_pointer_reaches_windows_through_the_view_ray,
    monitors_trade_windows_and_return_starts_the_terminal,
    up_close_a_monitor_fills_the_output,
    the_launcher_searches_and_starts_applications,
    windows_that_wait_are_told_they_are_not_seen,
    popups_stay_on_the_monitor_that_shows_their_window,
    the_candidates_of_an_input_method_stay_on_the_monitor,
);

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

/// A window that waits for a monitor is not seen (`hz.set_visible`): its
/// client is told it is suspended. Tab brings it back, and the one it
/// replaces sleeps once the change of picture is over. Up close on one
/// monitor, the windows of the others are not seen either.
fn windows_that_wait_are_told_they_are_not_seen(device: Device) {
    let mut session = start("room-unseen", device, "640x360");
    let v6 = |title: &str, color| ClientOptions {
        xdg_wm_base_version: 6,
        ..options(title, 100, 60, color)
    };
    let (mut a, _) = open_on(&mut session, &mut [], v6("a", RED), MAIN);
    let (mut b, _) = open_on(&mut session, &mut [&mut a], v6("b", GREEN), LEFT);
    let (mut c, _) = open_on(&mut session, &mut [&mut a, &mut b], v6("c", BLUE), RIGHT);
    let (mut d, idd) = open_on(
        &mut session,
        &mut [&mut a, &mut b, &mut c],
        v6("d", YELLOW),
        TOP,
    );
    settle(&mut session, "four");
    for title in ["a", "b", "c", "d"] {
        assert_eq!(seen(&mut session, title), (true, false), "{title} is shown");
    }

    // A fifth takes the focused monitor (the top one): d waits, and is not
    // seen once the change of picture is over.
    let (mut e, _) = open_on(
        &mut session,
        &mut [&mut a, &mut b, &mut c, &mut d],
        v6("e", WHITE),
        TOP,
    );
    assert_eq!(waiting(&mut session), [idd]);
    assert_eq!(seen(&mut session, "d"), (true, false), "d slides out");
    let mut all = [&mut a, &mut b, &mut c, &mut d, &mut e];
    settle(&mut session, "five");
    session.wait(&mut all, "d hidden", |s| {
        window(s, "d").unwrap()["visible"] == false
    });
    assert_eq!(seen(&mut session, "d"), (false, true), "d waits");
    all[3]
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::Suspended { suspended: true })
        })
        .unwrap()
        .expect("d is told it is suspended");
    assert_eq!(room(&mut session)["hidden"], json!([idd]));

    // Tab: d is back on the top monitor at once, e waits.
    logo_key(&mut session, KEY_TAB, false);
    session.wait(&mut all, "d seen", |s| {
        window(s, "d").unwrap()["visible"] == true
    });
    settle(&mut session, "tab");
    session.wait(&mut all, "e hidden", |s| {
        window(s, "e").unwrap()["visible"] == false
    });
    assert_eq!(seen(&mut session, "d"), (true, false), "d is shown");
    assert_eq!(seen(&mut session, "e"), (false, true), "e waits");

    // Up close on the main monitor: only a is seen. Back in the chair:
    // all four that are shown.
    logo_key(&mut session, KEY_1, false);
    logo_key(&mut session, KEY_F, false);
    settle(&mut session, "close");
    session.wait(&mut all, "the others hidden", |s| {
        window(s, "b").unwrap()["visible"] == false
    });
    assert_eq!(room(&mut session)["focused"], MAIN);
    assert_eq!(seen(&mut session, "a"), (true, false), "a fills the view");
    for title in ["b", "c", "d", "e"] {
        assert!(
            !seen(&mut session, title).0,
            "{title} is out of the picture"
        );
    }
    logo_key(&mut session, KEY_F, false);
    session.wait(&mut all, "seen again", |s| {
        window(s, "b").unwrap()["visible"] == true
    });
    settle(&mut session, "back");
    for title in ["a", "b", "c", "d"] {
        assert_eq!(seen(&mut session, title), (true, false), "{title} is shown");
    }
    assert!(!seen(&mut session, "e").0, "e still waits");
    session.assert_script_healthy();
    session.quit();
}

const RED: [u8; 4] = [210, 60, 50, 255];
const GREEN: [u8; 4] = [60, 190, 70, 255];
const BLUE: [u8; 4] = [50, 80, 220, 255];
const YELLOW: [u8; 4] = [220, 200, 40, 255];
const WHITE: [u8; 4] = [235, 235, 235, 255];

const KEY_ESC: u32 = 1;
const KEY_1: u32 = 2;
const KEY_BACKSPACE: u32 = 14;
const KEY_P: u32 = 25;
const KEY_A: u32 = 30;
const KEY_D: u32 = 32;
const KEY_G: u32 = 34;
const KEY_N: u32 = 49;
const KEY_LEFTCTRL: u32 = 29;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_Q: u32 = 16;
const KEY_F: u32 = 33;
const KEY_V: u32 = 47;
const KEY_H: u32 = 35;
const KEY_L: u32 = 38;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_UP: u32 = 103;
const KEY_DOWN: u32 = 108;
const KEY_LEFTMETA: u32 = 125;
const BTN_LEFT: u32 = 272;

// The settings of the project (`main.luau`).
const CAMERA_SECONDS: f64 = 0.55;
const SWITCH_SECONDS: f64 = 0.4;

// The monitors of the scene (`scene.luau`): index (from 1) and resolution.
const MAIN: u64 = 1;
const LEFT: u64 = 2;
const RIGHT: u64 = 3;
const TOP: u64 = 4;
const RESOLUTIONS: [(u32, u32); 4] = [(1920, 1080), (1280, 720), (720, 1280), (1280, 720)];

fn project() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("projects/room")
}

/// A session for a test: without what makes positions and colors on the
/// screen differ from those of the scene.
fn start(name: &str, device: Device, size: &str) -> Session {
    let mut session = start_as_shown(name, device, size);
    // The view moves with the pointer; the tests compare positions on the
    // screen from before and after they move it. The lens spreads light
    // over the colors they look at.
    session.script("room_set", json!(["parallax", 0]));
    session.script("room_set", json!(["bloom", 0]));
    session.script("room_set", json!(["vignette", 0]));
    session
}

/// The project, with its shaders built.
fn built_project() -> PathBuf {
    let project = project();
    // The tests of this file run side by side and share the project: its
    // shaders are built once, before any session looks at them.
    static SHADERS: std::sync::Once = std::sync::Once::new();
    SHADERS.call_once(|| common::build_shaders(&project));
    project
}

/// A session with the project as it is.
fn start_as_shown(name: &str, device: Device, size: &str) -> Session {
    start_as_shown_with_env(name, device, size, &[])
}

/// Like [`start_as_shown`], with variables set in the compositor's
/// environment.
fn start_as_shown_with_env(
    name: &str,
    device: Device,
    size: &str,
    env: &[(&'static str, String)],
) -> Session {
    let project = built_project();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut session = Session::start_with_env(
        name,
        device,
        &[
            "--output-size",
            size,
            "--clock",
            "manual",
            "--script",
            project.to_str().unwrap(),
        ],
        &env,
    );
    assert_eq!(session.state()["clock"]["kind"], "manual");
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

fn room(session: &mut Session) -> Value {
    session.script("room_state", json!([]))
}

/// The windows that wait for a monitor. (An empty list of the script
/// arrives as an empty object.)
fn waiting(session: &mut Session) -> Vec<u64> {
    room(session)["waiting"]
        .as_array()
        .map(|list| list.iter().map(|id| id.as_u64().unwrap()).collect())
        .unwrap_or_default()
}

/// What the monitor `index` shows: a window id, or `false`.
fn shown(session: &mut Session, index: u64) -> Value {
    room(session)["monitors"][index as usize - 1]["window"].clone()
}

/// Lets every move and change that is under way finish, and draws a frame
/// of the result.
fn settle(session: &mut Session, name: &str) -> EncodedImage {
    advance(session, CAMERA_SECONDS.max(SWITCH_SECONDS) + 0.05);
    // The frame that ends the changes, then one of the settled state.
    session.screenshot_png(name);
    session.screenshot_png(name)
}

/// Opens a window and waits until it has the size of `monitor` and is shown
/// there. Returns the client and the id of the window.
fn open_on(
    session: &mut Session,
    others: &mut [&mut TestClient],
    options: ClientOptions,
    monitor: u64,
) -> (TestClient, u64) {
    let title = options.title.clone();
    let (width, height) = RESOLUTIONS[monitor as usize - 1];
    let mut client = session.client(options);
    let mut clients: Vec<&mut TestClient> = others.iter_mut().map(|c| &mut **c).collect();
    clients.push(&mut client);
    let state = session.wait(&mut clients, &format!("{title} at {width}x{height}"), |s| {
        window(s, &title).is_some_and(|w| {
            w["size"] == json!([width, height]) && w["surfaces"][0]["buffer_size"].is_array()
        })
    });
    let id = window(&state, &title).unwrap()["id"].as_u64().unwrap();
    assert_eq!(
        shown(session, monitor),
        id,
        "{title} is on monitor {monitor}"
    );
    (client, id)
}

/// Where a point of a monitor (in its pixels) is on the screen.
fn on_screen(session: &mut Session, monitor: u64, x: f64, y: f64) -> (f64, f64) {
    let point = session.script("monitor_point", json!([monitor, x, y]));
    (point[0].as_f64().unwrap(), point[1].as_f64().unwrap())
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

#[track_caller]
fn assert_close(actual: (f64, f64), expected: (f64, f64), tolerance: f64, what: &str) {
    assert!(
        (actual.0 - expected.0).abs() <= tolerance && (actual.1 - expected.1).abs() <= tolerance,
        "{what}: at {actual:?}, expected {expected:?}"
    );
}

/// Asserts that a pixel has roughly the hue of `expected`: the screens are
/// seen at an angle and resampled, and the working space is not the
/// output's, so the values are not exact.
#[track_caller]
fn assert_rgb_near(image: &EncodedImage, x: f64, y: f64, expected: [u8; 4], what: &str) {
    let actual = image.pixel(x.round() as u32, y.round() as u32);
    let worst = (0..3)
        .map(|i| (actual[i] as i32 - expected[i] as i32).abs())
        .max()
        .unwrap();
    assert!(
        worst <= 12,
        "{what}: pixel ({x}, {y}) is {actual:?}, expected about {expected:?}"
    );
}

/// New windows take the monitors without a signal, the focused one first;
/// with every monitor taken, a new window takes the focused monitor and the
/// one that was there waits. Logo+Tab walks the waiting windows on the
/// focused monitor. A monitor whose window closes shows the next waiting
/// one, or has no signal.
fn windows_take_monitors_and_tab_chooses_among_the_waiting(device: Device) {
    let mut session = start("room-monitors", device, "640x360");
    let state = room(&mut session);
    assert_eq!(state["monitors"].as_array().unwrap().len(), 4);
    assert!(
        state["monitors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["signal"] == false),
        "{state:#}"
    );
    assert_eq!(state["focused"], MAIN);

    let (mut a, ida) = open_on(&mut session, &mut [], options("a", 100, 60, RED), MAIN);
    let (mut b, idb) = open_on(
        &mut session,
        &mut [&mut a],
        options("b", 100, 60, GREEN),
        LEFT,
    );
    assert_eq!(room(&mut session)["focused"], LEFT, "the focus follows");
    assert_eq!(session.state()["focus"]["window"], idb);
    let (mut c, idc) = open_on(
        &mut session,
        &mut [&mut a, &mut b],
        options("c", 100, 60, BLUE),
        RIGHT,
    );
    let (mut d, idd) = open_on(
        &mut session,
        &mut [&mut a, &mut b, &mut c],
        options("d", 100, 60, YELLOW),
        TOP,
    );
    assert_eq!(waiting(&mut session), Vec::<u64>::new());
    settle(&mut session, "four");

    // Every monitor is taken: the fifth takes the focused one (top).
    let (mut e, ide) = open_on(
        &mut session,
        &mut [&mut a, &mut b, &mut c, &mut d],
        options("e", 100, 60, WHITE),
        TOP,
    );
    assert_eq!(waiting(&mut session), [idd]);
    assert_eq!(session.state()["focus"]["window"], ide);
    assert_eq!(room(&mut session)["monitors"][3]["switching"], true);
    settle(&mut session, "five");
    assert_eq!(room(&mut session)["monitors"][3]["switching"], false);

    // Tab: d comes back, e waits. Again: e.
    logo_key(&mut session, KEY_TAB, false);
    assert_eq!(shown(&mut session, TOP), idd);
    assert_eq!(waiting(&mut session), [ide]);
    assert_eq!(session.state()["focus"]["window"], idd);
    logo_key(&mut session, KEY_TAB, true);
    assert_eq!(shown(&mut session, TOP), ide);
    // The other monitors keep theirs.
    assert_eq!(shown(&mut session, MAIN), ida);
    assert_eq!(shown(&mut session, LEFT), idb);
    assert_eq!(shown(&mut session, RIGHT), idc);

    // The focus moves between the monitors: top -> (left of it) -> ...
    logo_key(&mut session, KEY_H, false);
    assert_eq!(room(&mut session)["focused"], LEFT);
    assert_eq!(session.state()["focus"]["window"], idb);
    logo_key(&mut session, KEY_L, false);
    assert_eq!(room(&mut session)["focused"], MAIN);
    assert_eq!(session.state()["focus"]["window"], ida);

    // The window of the main monitor closes: the waiting one (d) takes it.
    logo_key(&mut session, KEY_Q, false);
    a.wait_for(common::TIMEOUT, |e| *e == ClientEvent::CloseRequested)
        .unwrap()
        .expect("a is asked to close");
    drop(a);
    session.wait(&mut [&mut b, &mut c, &mut d, &mut e], "a gone", |s| {
        window(s, "a").is_none()
    });
    assert_eq!(shown(&mut session, MAIN), idd);
    session.wait(
        &mut [&mut b, &mut c, &mut d, &mut e],
        "d at 1920x1080",
        |s| window(s, "d").is_some_and(|w| w["size"] == json!([1920, 1080])),
    );
    assert_eq!(session.state()["focus"]["window"], idd);
    settle(&mut session, "closed");

    // And that one too: nothing waits, so the monitor has no signal.
    logo_key(&mut session, KEY_Q, false);
    d.wait_for(common::TIMEOUT, |e| *e == ClientEvent::CloseRequested)
        .unwrap()
        .expect("d is asked to close");
    drop(d);
    session.wait(&mut [&mut b, &mut c, &mut e], "d gone", |s| {
        window(s, "d").is_none()
    });
    let state = room(&mut session);
    assert_eq!(state["monitors"][0]["signal"], false, "{state:#}");
    assert_eq!(session.state()["focus"]["window"], Value::Null);
    settle(&mut session, "no-signal");
    // What a closed window last showed is not kept past the change.
    let state = session.state();
    assert_eq!(state["script"]["closed_windows"], json!([]), "{state:#}");

    // The clients of the three windows that are left go away at the same
    // moment, without being asked: every monitor ends without a signal.
    // The focus is on one of them (the left monitor's).
    logo_key(&mut session, KEY_H, false);
    assert_eq!(session.state()["focus"]["window"], idb);
    drop(c);
    drop(e);
    drop(b);
    session.wait(&mut [], "every window gone", |s| {
        s["windows"].as_array().is_some_and(|w| w.is_empty())
    });
    let state = room(&mut session);
    assert!(
        state["monitors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["signal"] == false),
        "{state:#}"
    );
    settle(&mut session, "all-gone");
    session.assert_script_healthy();
    session.quit();
}

/// The pointer is over a monitor that is seen at an angle: the window it
/// shows gets the point of the monitor under the pointer, in its own
/// coordinates. The picture agrees: the same point of the window is drawn
/// there.
fn the_pointer_reaches_windows_through_the_view_ray(device: Device) {
    let mut session = start("room-pointer", device, "1280x720");
    let quadrants = |title: &str| ClientOptions {
        pattern: Pattern::Quadrants([RED, GREEN, BLUE, YELLOW]),
        ..options(title, 100, 60, WHITE)
    };
    let (mut a, _) = open_on(&mut session, &mut [], quadrants("a"), MAIN);
    let (mut b, idb) = open_on(&mut session, &mut [&mut a], quadrants("b"), LEFT);
    let (mut c, _) = open_on(&mut session, &mut [&mut a, &mut b], quadrants("c"), RIGHT);
    // Back to the main monitor, so that the others are seen from the side.
    logo_key(&mut session, KEY_L, false);
    logo_key(&mut session, KEY_H, false);
    assert_eq!(room(&mut session)["focused"], MAIN);
    let image = settle(&mut session, "three");

    for (monitor, client, name) in [
        (MAIN, &mut a, "a"),
        (LEFT, &mut b, "b"),
        (RIGHT, &mut c, "c"),
    ] {
        let (width, height) = RESOLUTIONS[monitor as usize - 1];
        let (width, height) = (width as f64, height as f64);
        // The middle of each quadrant: the pointer lands there, and the
        // picture has the quadrant's color there.
        for (qx, qy, color) in [
            (0.25, 0.25, RED),
            (0.75, 0.25, GREEN),
            (0.25, 0.75, BLUE),
            (0.75, 0.75, YELLOW),
        ] {
            let target = (width * qx, height * qy);
            let screen = on_screen(&mut session, monitor, target.0, target.1);
            let at = point_at(&mut session, client, screen);
            // The pointer travels as 24.8 fixed point on the way in and on
            // the way out, and a pixel of the screen covers more than one of
            // the monitor.
            assert_close(at, target, 0.05, &format!("{name} at {qx}, {qy}"));
            assert_rgb_near(
                &image,
                screen.0,
                screen.1,
                color,
                &format!("{name} at {qx}, {qy}"),
            );
        }
    }

    // A click on another monitor gives it the focus, and the button reaches
    // its window.
    let screen = on_screen(&mut session, LEFT, 300.0, 200.0);
    point_at(&mut session, &mut b, screen);
    b.clear_events();
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    b.wait_for(common::TIMEOUT, |e| {
        *e == ClientEvent::PointerButton {
            button: BTN_LEFT,
            pressed: false,
        }
    })
    .unwrap()
    .expect("the click reaches b");
    assert_eq!(room(&mut session)["focused"], LEFT);
    assert_eq!(session.state()["focus"]["window"], idb);

    // What is typed now goes to that window, and to no other.
    a.clear_events();
    b.clear_events();
    c.clear_events();
    key(&mut session, KEY_A, true);
    key(&mut session, KEY_A, false);
    let typed = |e: &ClientEvent| {
        matches!(
            e,
            ClientEvent::Key {
                keycode: KEY_A,
                pressed: false,
                ..
            }
        )
    };
    b.wait_for(common::TIMEOUT, typed)
        .unwrap()
        .expect("the key reaches b");
    for (other, name) in [(&mut a, "a"), (&mut c, "c")] {
        other
            .pump(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        assert!(
            !other
                .events()
                .iter()
                .any(|e| matches!(e, ClientEvent::Key { .. })),
            "{name} got a key: {:?}",
            other.events()
        );
    }
    session.assert_script_healthy();
    session.quit();
}

/// Logo+shift+direction trades what two monitors show; each window is asked
/// for the size of its new monitor, and the focus goes along. Logo+Return
/// starts the terminal.
fn monitors_trade_windows_and_return_starts_the_terminal(device: Device) {
    let mut session = start("room-trade", device, "640x360");
    let (mut a, ida) = open_on(&mut session, &mut [], options("a", 100, 60, RED), MAIN);
    settle(&mut session, "one");

    // To the right, where there is no signal: the window moves there.
    logo_key(&mut session, KEY_L, true);
    assert_eq!(shown(&mut session, RIGHT), ida);
    assert_eq!(shown(&mut session, MAIN), false);
    assert_eq!(room(&mut session)["focused"], RIGHT);
    assert_eq!(session.state()["focus"]["window"], ida);
    session.wait(&mut [&mut a], "a at 720x1280", |s| {
        window(s, "a").is_some_and(|w| w["size"] == json!([720, 1280]))
    });
    settle(&mut session, "moved");

    // A second window takes the main monitor (the first one without a
    // signal); trading back to the left swaps the two.
    let (mut b, idb) = open_on(
        &mut session,
        &mut [&mut a],
        options("b", 100, 60, GREEN),
        MAIN,
    );
    logo_key(&mut session, KEY_L, false);
    assert_eq!(room(&mut session)["focused"], RIGHT);
    logo_key(&mut session, KEY_H, true);
    assert_eq!(shown(&mut session, MAIN), ida);
    assert_eq!(shown(&mut session, RIGHT), idb);
    assert_eq!(room(&mut session)["focused"], MAIN);
    assert_eq!(session.state()["focus"]["window"], ida);
    session.wait(&mut [&mut a, &mut b], "the sizes traded", |s| {
        window(s, "a").is_some_and(|w| w["size"] == json!([1920, 1080]))
            && window(s, "b").is_some_and(|w| w["size"] == json!([720, 1280]))
    });
    settle(&mut session, "traded");

    // The script is loaded again: the windows keep their monitors, and the
    // focus stays where it was.
    session.send(Request::ScriptReload { path: None });
    assert_eq!(shown(&mut session, MAIN), ida);
    assert_eq!(shown(&mut session, RIGHT), idb);
    assert_eq!(room(&mut session)["focused"], MAIN);
    assert_eq!(session.state()["focus"]["window"], ida);
    session.script("room_set", json!(["parallax", 0]));
    settle(&mut session, "reloaded");

    // The terminal: whatever the project is told to start.
    let mark = session.runtime_dir.join("room-terminal");
    let command = format!(
        "echo \"$WAYLAND_DISPLAY\" > '{0}.tmp' && mv '{0}.tmp' '{0}'",
        mark.display()
    );
    session.script("room_terminal", json!([["sh", "-c", command]]));
    logo_key(&mut session, KEY_ENTER, false);
    let text = common::wait_for_file(&mark);
    assert_eq!(text.trim(), "hizukuri-test");
    session.assert_script_healthy();
    session.quit();
}

/// Logo+F moves the camera to where the focused monitor fills the output,
/// straight on: the corners of the monitor are the corners of the output,
/// and the pointer maps to the window in proportion.
fn up_close_a_monitor_fills_the_output(device: Device) {
    let mut session = start("room-zoom", device, "960x540");
    let quadrants = ClientOptions {
        pattern: Pattern::Quadrants([RED, GREEN, BLUE, YELLOW]),
        ..options("a", 100, 60, WHITE)
    };
    let (mut a, _) = open_on(&mut session, &mut [], quadrants, MAIN);
    settle(&mut session, "room");
    let corner = on_screen(&mut session, MAIN, 0.0, 0.0);
    assert!(corner.0 > 100.0 && corner.1 > 50.0, "{corner:?}");

    logo_key(&mut session, KEY_F, false);
    assert_eq!(room(&mut session)["zoomed"], true);
    // Half way, the monitor is larger than before, and not yet full.
    advance(&mut session, CAMERA_SECONDS / 2.0);
    session.screenshot_png("half");
    let half = on_screen(&mut session, MAIN, 0.0, 0.0);
    assert!(
        half.0 < corner.0 && half.0 > 0.0,
        "{half:?} from {corner:?}"
    );

    let image = settle(&mut session, "full");
    assert_close(
        on_screen(&mut session, MAIN, 0.0, 0.0),
        (0.0, 0.0),
        1e-6,
        "top left",
    );
    assert_close(
        on_screen(&mut session, MAIN, 1920.0, 1080.0),
        (960.0, 540.0),
        1e-6,
        "bottom right",
    );
    assert_rgb_near(&image, 240.0, 135.0, RED, "top left quadrant");
    assert_rgb_near(&image, 720.0, 135.0, GREEN, "top right quadrant");
    assert_rgb_near(&image, 240.0, 405.0, BLUE, "bottom left quadrant");
    assert_rgb_near(&image, 720.0, 405.0, YELLOW, "bottom right quadrant");
    let at = point_at(&mut session, &mut a, (100.25, 300.5));
    assert_close(at, (200.5, 601.0), 0.01, "the pointer, up close");

    // Up close on a window that stands still, nothing is drawn: the other
    // monitors have no signal, but their moving pictures are out of sight.
    session.wait(&mut [&mut a], "no frame owed", |s| {
        s["redraw_pending"] == false
    });
    let frames = |session: &mut Session| session.state()["script"]["frames"].as_u64().unwrap();
    let before = frames(&mut session);
    for _ in 0..10 {
        a.pump(Some(std::time::Duration::from_millis(20))).unwrap();
    }
    assert_eq!(
        frames(&mut session),
        before,
        "frames were drawn with nothing to show"
    );

    // Again: back to the room.
    logo_key(&mut session, KEY_F, false);
    settle(&mut session, "back");
    assert_close(
        on_screen(&mut session, MAIN, 0.0, 0.0),
        corner,
        1e-6,
        "back",
    );
    session.assert_script_healthy();
    session.quit();
}

/// Directories of desktop entries for one test (the compositor's
/// `XDG_DATA_HOME` and `XDG_DATA_DIRS`), and a stand-in for the terminal
/// that writes down what it was asked to run. Removed at the end.
struct Applications {
    root: PathBuf,
}

/// A keymap that does not depend on the machine.
const KEYMAP: &[(&str, &str)] = &[
    ("XKB_DEFAULT_RULES", "evdev"),
    ("XKB_DEFAULT_MODEL", "pc105"),
    ("XKB_DEFAULT_LAYOUT", "us"),
    ("XKB_DEFAULT_VARIANT", ""),
    ("XKB_DEFAULT_OPTIONS", ""),
];

impl Applications {
    fn new(name: &str, device: Device) -> Applications {
        let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("room-applications")
            .join(format!("{name}-{}", device.name()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/applications")).unwrap();
        std::fs::create_dir_all(root.join("system/applications")).unwrap();
        let terminal = root.join("terminal");
        let mark = root.join("terminal-ran");
        std::fs::write(
            &terminal,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}.tmp' && mv '{0}.tmp' '{0}'\n",
                mark.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&terminal, std::fs::Permissions::from_mode(0o755)).unwrap();
        Applications { root }
    }

    /// The compositor's environment that makes it read these directories
    /// (and only these), and start the stand-in as its terminal.
    fn env(&self) -> Vec<(&'static str, String)> {
        let terminal = self.root.join("terminal").display().to_string();
        assert!(
            !terminal.contains(' '),
            "{terminal}: the variable is split at spaces"
        );
        let mut env = vec![
            (
                "XDG_DATA_HOME",
                self.root.join("home").display().to_string(),
            ),
            (
                "XDG_DATA_DIRS",
                self.root.join("system").display().to_string(),
            ),
            ("XDG_CURRENT_DESKTOP", "hizukuri-test".to_owned()),
            ("LC_ALL", "".to_owned()),
            ("LC_MESSAGES", "".to_owned()),
            ("LANG", "C".to_owned()),
            ("HIZUKURI_TERMINAL", terminal),
        ];
        env.extend(KEYMAP.iter().map(|(k, v)| (*k, (*v).to_owned())));
        env
    }

    fn write(&self, file: &str, text: &str) {
        let path = self.root.join("system/applications").join(file);
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, text).unwrap();
        std::fs::rename(&temporary, &path).unwrap();
    }

    /// An application that is the test client, showing a window with the
    /// title `title` in a solid color (`RRGGBBAA`), on the session's socket.
    fn write_client(&self, session: &Session, file: &str, name: &str, title: &str, color: &str) {
        let client = std::path::Path::new(env!("CARGO_BIN_EXE_proto-compositor"))
            .with_file_name("proto-test-client");
        assert!(
            client.exists(),
            "{} is missing: the workspace's binaries are built by `cargo test --workspace` \
             (or `cargo build -p proto-test-client`)",
            client.display()
        );
        let socket = session.runtime_dir.join("hizukuri-test");
        self.write(
            file,
            &format!(
                "[Desktop Entry]\nType=Application\nName={name}\n\
                 Exec=\"{}\" --socket \"{}\" --title {title} --color {color} --duration 120\n",
                client.display(),
                socket.display()
            ),
        );
    }

    /// What the stand-in for the terminal was asked to run, once it ran.
    fn terminal_ran(&self) -> Vec<String> {
        common::wait_for_file(&self.root.join("terminal-ran"))
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for Applications {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Stops what the project started (by the process ids the compositor
/// reports).
fn stop_started(session: &mut Session) {
    let state = session.state();
    let own = std::process::id();
    for started in state["script"]["spawned"].as_array().into_iter().flatten() {
        if let Some(pid) = started["pid"].as_u64().filter(|pid| *pid as u32 != own) {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

fn launcher(session: &mut Session) -> Value {
    room(session)["launcher"].clone()
}

/// The desktop file IDs the launcher lists, in its order.
fn listed(session: &mut Session) -> Vec<String> {
    launcher(session)["results"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|id| id.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Presses and releases keys, one after the other.
fn type_keys(session: &mut Session, keycodes: &[u32]) {
    for keycode in keycodes {
        key(session, *keycode, true);
        key(session, *keycode, false);
    }
}

/// Logo+D opens the launcher on the focused monitor; while it is open it
/// has the keyboard: typing searches, the arrows (and Ctrl+N / Ctrl+P)
/// choose, Enter starts the application and closes it, Escape closes it.
/// A started window takes a monitor as any other; an application that runs
/// in a terminal is started in the terminal, with `-e`. What was started
/// last comes first.
fn the_launcher_searches_and_starts_applications(device: Device) {
    let apps = Applications::new("launcher", device);
    let mut session = start_as_shown_with_env("room-launcher", device, "960x540", &apps.env());
    for setting in ["parallax", "bloom", "vignette"] {
        session.script("room_set", json!([setting, 0]));
    }
    apps.write_client(
        &session,
        "alpha.desktop",
        "Alpha Viewer",
        "alpha",
        "d23c32ff",
    );
    apps.write_client(&session, "beta.desktop", "Beta Paint", "beta", "3cbe46ff");
    apps.write(
        "gamma.desktop",
        "[Desktop Entry]\nType=Application\nName=Gamma Shell\nGenericName=Terminal tool\n\
         Exec=gamma-tool --flag\nTerminal=true\n",
    );
    let (mut x, idx) = open_on(&mut session, &mut [], options("x", 100, 60, BLUE), MAIN);
    settle(&mut session, "window");
    assert_eq!(launcher(&mut session)["open"], false);

    logo_key(&mut session, KEY_D, false);
    let state = launcher(&mut session);
    assert_eq!(state["open"], true, "{state:#}");
    assert_eq!(state["monitor"], MAIN);
    assert_eq!(state["selected"], 1);
    assert_eq!(
        listed(&mut session),
        ["alpha.desktop", "beta.desktop", "gamma.desktop"]
    );
    settle(&mut session, "launcher-open");

    // What is typed searches, and reaches no client: a word that starts
    // with it comes before a name that has it inside. (The logo key itself
    // went to the window: the launcher takes the keys after it.)
    x.pump(Some(std::time::Duration::from_millis(100))).unwrap();
    x.clear_events();
    type_keys(&mut session, &[KEY_P]);
    assert_eq!(launcher(&mut session)["query"], "p");
    assert_eq!(listed(&mut session), ["beta.desktop", "alpha.desktop"]);
    settle(&mut session, "launcher-search");
    type_keys(&mut session, &[KEY_BACKSPACE]);
    assert_eq!(launcher(&mut session)["query"], "");
    assert_eq!(listed(&mut session).len(), 3);
    x.pump(Some(std::time::Duration::from_millis(100))).unwrap();
    assert!(
        !x.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::Key { keycode, .. } if *keycode != KEY_LEFTMETA)),
        "the window got keys while the launcher was open: {:?}",
        x.events()
    );
    assert_eq!(session.state()["focus"]["window"], idx);

    // Escape closes it; then keys reach the window again.
    type_keys(&mut session, &[KEY_ESC]);
    assert_eq!(launcher(&mut session)["open"], false);
    x.clear_events();
    type_keys(&mut session, &[KEY_A]);
    x.wait_for(common::TIMEOUT, |e| {
        matches!(
            e,
            ClientEvent::Key {
                keycode: KEY_A,
                pressed: false,
                ..
            }
        )
    })
    .unwrap()
    .expect("the key reaches the window after the launcher closed");
    settle(&mut session, "launcher-closed");

    // An application that runs in a terminal.
    logo_key(&mut session, KEY_D, false);
    type_keys(&mut session, &[KEY_G, KEY_A]);
    assert_eq!(listed(&mut session), ["gamma.desktop"]);
    type_keys(&mut session, &[KEY_ENTER]);
    assert_eq!(launcher(&mut session)["open"], false);
    assert_eq!(apps.terminal_ran(), ["-e", "gamma-tool", "--flag"]);

    // Again: the one started last is first. Down, Ctrl+N, Ctrl+P: the
    // second line, which Enter starts. Its window takes a monitor without
    // a signal, and the focus.
    logo_key(&mut session, KEY_D, false);
    assert_eq!(
        listed(&mut session),
        ["gamma.desktop", "alpha.desktop", "beta.desktop"]
    );
    type_keys(&mut session, &[KEY_DOWN]);
    key(&mut session, KEY_LEFTCTRL, true);
    type_keys(&mut session, &[KEY_N, KEY_P]);
    key(&mut session, KEY_LEFTCTRL, false);
    assert_eq!(launcher(&mut session)["selected"], 2);
    type_keys(&mut session, &[KEY_ENTER]);
    assert_eq!(launcher(&mut session)["open"], false);
    let state = session.wait(&mut [&mut x], "alpha at 1280x720", |s| {
        window(s, "alpha").is_some_and(|w| {
            w["size"] == json!([1280, 720]) && w["surfaces"][0]["buffer_size"].is_array()
        })
    });
    let ida = window(&state, "alpha").unwrap()["id"].as_u64().unwrap();
    assert_eq!(shown(&mut session, LEFT), ida);
    assert_eq!(shown(&mut session, MAIN), idx);
    assert_eq!(room(&mut session)["focused"], LEFT);
    settle(&mut session, "launcher-started");

    // Logo+D twice: open, and closed again, on the monitor with the focus.
    logo_key(&mut session, KEY_D, false);
    assert_eq!(launcher(&mut session)["monitor"], LEFT);
    logo_key(&mut session, KEY_D, false);
    assert_eq!(launcher(&mut session)["open"], false);
    settle(&mut session, "launcher-done");

    // A click on a line starts that application: the third (beta; the two
    // started are first). Its window takes the next monitor without a
    // signal.
    logo_key(&mut session, KEY_D, false);
    assert_eq!(
        listed(&mut session),
        ["alpha.desktop", "gamma.desktop", "beta.desktop"]
    );
    settle(&mut session, "launcher-on-left");
    let line = session.script("room_launcher_line", json!([3]));
    assert_eq!(line[0], LEFT);
    let screen = on_screen(
        &mut session,
        LEFT,
        line[1].as_f64().unwrap(),
        line[2].as_f64().unwrap(),
    );
    session.send(Request::PointerMotion {
        x: screen.0,
        y: screen.1,
    });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    session.send(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    assert_eq!(launcher(&mut session)["open"], false);
    let state = session.wait(&mut [&mut x], "beta at 720x1280", |s| {
        window(s, "beta").is_some_and(|w| w["size"] == json!([720, 1280]))
    });
    let idb = window(&state, "beta").unwrap()["id"].as_u64().unwrap();
    assert_eq!(shown(&mut session, RIGHT), idb);
    settle(&mut session, "launcher-clicked");

    // What was started is remembered over a reload of the script.
    session.send(Request::ScriptReload { path: None });
    logo_key(&mut session, KEY_D, false);
    assert_eq!(
        listed(&mut session),
        ["beta.desktop", "alpha.desktop", "gamma.desktop"]
    );
    type_keys(&mut session, &[KEY_ESC]);
    assert_eq!(launcher(&mut session)["open"], false);
    settle(&mut session, "launcher-reloaded");

    session.assert_script_healthy();
    stop_started(&mut session);
    drop(x);
    session.quit();
}

/// Pictures of the room, for looking at: a terminal on the main monitor,
/// test windows on two others, one monitor without a signal; the camera at
/// each monitor, up close, and a change of window half way. Not a test: run
/// by hand,
///
/// ```sh
/// cargo test -p proto-compositor --test room -- --ignored pictures
/// ```
///
/// The size of the output is `HIZUKURI_ROOM_PICTURE_SIZE` (default
/// `960x540`). The pictures are in `target/test-output/room-pictures-*`.
#[test]
#[ignore = "pictures to look at, taken by hand (see the documentation)"]
fn pictures() {
    use std::time::{Duration, Instant};

    let hardware = std::env::var("HIZUKURI_TEST_DEVICES").is_ok_and(|v| v == "gpu");
    let device = Device::new(if hardware {
        common::Gpu::Hardware
    } else {
        common::Gpu::Lavapipe
    });
    let size = std::env::var("HIZUKURI_ROOM_PICTURE_SIZE").unwrap_or_else(|_| "960x540".into());
    let mut session = start_as_shown("room-pictures", device, &size);
    advance(&mut session, 3.0);
    session.screenshot_png("01-no-signal");

    // A terminal with something colorful in it.
    let script = "printf '\\n  hizukuri: a room with monitors\\n\\n'; \
                  for r in 0 1 2 3 4 5; do printf '  '; \
                  for c in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 \
                  24 25 26 27 28 29 30 31 32 33 34 35; do \
                  printf '\\033[48;5;%dm    ' $((16 + r * 36 + c)); done; \
                  printf '\\033[0m\\n'; done; \
                  printf '\\n'; ls --color=always -l / | head -n 14; printf '\\n$ '; exec sleep 600";
    let _terminal = session.spawn_client(
        "foot",
        "foot",
        &["-f", "monospace:size=17", "-e", "sh", "-c", script],
    );
    let deadline = Instant::now() + common::TIMEOUT;
    loop {
        let state = session.state();
        let ready = state["windows"].as_array().is_some_and(|windows| {
            windows.iter().any(|w| {
                w["app_id"] == "foot"
                    && w["size"][0] == 1920
                    && w["surfaces"][0]["has_contents"] == true
            })
        });
        if ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the terminal did not come up: {state:#}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let the terminal draw what the shell printed.
    std::thread::sleep(Duration::from_millis(800));
    settle(&mut session, "02-terminal");

    let quadrants = |title: &str, colors: [[u8; 4]; 4]| ClientOptions {
        pattern: Pattern::Quadrants(colors),
        ..options(title, 100, 60, WHITE)
    };
    let warm = [
        [230, 90, 30, 255],
        [250, 180, 40, 255],
        [200, 40, 60, 255],
        [240, 120, 90, 255],
    ];
    let cool = [
        [30, 60, 220, 255],
        [40, 170, 230, 255],
        [90, 40, 200, 255],
        [30, 200, 160, 255],
    ];
    let green = [
        [20, 160, 60, 255],
        [160, 220, 40, 255],
        [20, 110, 90, 255],
        [90, 200, 120, 255],
    ];
    let (mut b, _) = open_on(&mut session, &mut [], quadrants("b", warm), LEFT);
    let (mut c, _) = open_on(&mut session, &mut [&mut b], quadrants("c", cool), RIGHT);
    // The camera back at the main monitor.
    logo_key(&mut session, KEY_H, false);
    assert_eq!(room(&mut session)["focused"], MAIN);
    settle(&mut session, "03-room");

    logo_key(&mut session, KEY_F, false);
    advance(&mut session, CAMERA_SECONDS / 2.0);
    session.screenshot_png("04-moving-up-close");
    settle(&mut session, "05-up-close");
    logo_key(&mut session, KEY_F, false);
    settle(&mut session, "06-back");

    logo_key(&mut session, KEY_H, false);
    settle(&mut session, "07-left");
    logo_key(&mut session, KEY_L, false);
    logo_key(&mut session, KEY_L, false);
    settle(&mut session, "08-right");
    logo_key(&mut session, KEY_UP, false);
    settle(&mut session, "09-top");

    // More windows than monitors: the next ones wait, and Tab brings them.
    let paper = options("d", 100, 60, [240, 240, 235, 255]);
    let (mut d, _) = open_on(&mut session, &mut [&mut b, &mut c], paper, TOP);
    let (mut e, _) = open_on(
        &mut session,
        &mut [&mut b, &mut c, &mut d],
        quadrants("e", green),
        TOP,
    );
    settle(&mut session, "10-green-on-top");
    logo_key(&mut session, KEY_TAB, false);
    advance(&mut session, SWITCH_SECONDS * 0.45);
    session.screenshot_png("11-switching");
    settle(&mut session, "12-switched");
    logo_key(&mut session, KEY_DOWN, false);
    settle(&mut session, "13-room-with-white");
    // The way out through a view that maps tones, and back. The view is
    // compiled in the background: it takes a moment to come on.
    logo_key(&mut session, KEY_V, false);
    std::thread::sleep(Duration::from_millis(2500));
    settle(&mut session, "14-tone-mapped");
    logo_key(&mut session, KEY_V, false);
    settle(&mut session, "15-plain-again");
    for client in [&mut b, &mut c, &mut d, &mut e] {
        client.pump(Some(Duration::from_millis(0))).unwrap();
    }
    let state = session.script_state();
    println!("timing: {:#}", state["timing"]);
    session.assert_script_healthy();
    session.quit();
}

/// Pictures of the launcher, for looking at: open on the main monitor,
/// searching, another line chosen, an application starting and its window,
/// the launcher up close, and on the monitor that stands upright. The
/// applications are the test client. Not a test: run by hand,
///
/// ```sh
/// cargo test -p proto-compositor --test room -- --ignored launcher_pictures
/// ```
///
/// The size of the output is `HIZUKURI_ROOM_PICTURE_SIZE` (default
/// `1280x720`). The pictures are in `target/test-output/room-launcher-pictures-*`.
#[test]
#[ignore = "pictures to look at, taken by hand (see the documentation)"]
fn launcher_pictures() {
    let hardware = std::env::var("HIZUKURI_TEST_DEVICES").is_ok_and(|v| v == "gpu");
    let device = Device::new(if hardware {
        common::Gpu::Hardware
    } else {
        common::Gpu::Lavapipe
    });
    let size = std::env::var("HIZUKURI_ROOM_PICTURE_SIZE").unwrap_or_else(|_| "1280x720".into());
    let apps = Applications::new("launcher-pictures", device);
    let mut session = start_as_shown_with_env("room-launcher-pictures", device, &size, &apps.env());
    for (file, name, about, color) in [
        ("files", "Files", "Browse the file system", "c8a03cff"),
        ("browser", "Web Browser", "Browse the web", "3c78dcff"),
        ("editor", "Text Editor", "Edit text files", "50b45aff"),
        ("viewer", "Image Viewer", "Look at pictures", "dc5a3cff"),
        ("music", "Music Player", "Play music", "b43cc8ff"),
        ("calculator", "Calculator", "Do sums", "e6e6e6ff"),
        ("monitor", "System Monitor", "See what runs", "3cc8b4ff"),
        ("paint", "Paint", "Draw pictures", "f0c828ff"),
        ("mail", "Mail", "Read and write mail", "2850a0ff"),
        ("settings", "Settings", "Change the system", "787878ff"),
    ] {
        apps.write_client(&session, &format!("{file}.desktop"), name, file, color);
        // A second line under the name.
        let path = apps
            .root
            .join("system/applications")
            .join(format!("{file}.desktop"));
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("Comment={about}\n"));
        apps.write(&format!("{file}.desktop"), &text);
    }
    apps.write(
        "terminal.desktop",
        "[Desktop Entry]\nType=Application\nName=Terminal Tasks\nComment=Watch the processes\n\
         Exec=top\nTerminal=true\n",
    );
    let warm = [
        [230, 90, 30, 255],
        [250, 180, 40, 255],
        [200, 40, 60, 255],
        [240, 120, 90, 255],
    ];
    let cool = [
        [30, 60, 220, 255],
        [40, 170, 230, 255],
        [90, 40, 200, 255],
        [30, 200, 160, 255],
    ];
    let quadrants = |title: &str, colors: [[u8; 4]; 4]| ClientOptions {
        pattern: Pattern::Quadrants(colors),
        ..options(title, 100, 60, WHITE)
    };
    let (mut a, _) = open_on(
        &mut session,
        &mut [],
        options("a", 100, 60, [28, 30, 36, 255]),
        MAIN,
    );
    let (mut b, _) = open_on(&mut session, &mut [&mut a], quadrants("b", warm), LEFT);
    let (mut c, _) = open_on(
        &mut session,
        &mut [&mut a, &mut b],
        quadrants("c", cool),
        RIGHT,
    );
    logo_key(&mut session, KEY_H, false);
    assert_eq!(room(&mut session)["focused"], MAIN);
    settle(&mut session, "00-room");

    logo_key(&mut session, KEY_D, false);
    advance(&mut session, 0.08);
    session.screenshot_png("01-opening");
    settle(&mut session, "02-open");
    type_keys(&mut session, &[KEY_P]);
    settle(&mut session, "03-search");
    type_keys(&mut session, &[KEY_DOWN]);
    settle(&mut session, "04-chosen");
    type_keys(&mut session, &[KEY_BACKSPACE, KEY_BACKSPACE]);
    type_keys(&mut session, &[KEY_DOWN, KEY_DOWN, KEY_DOWN, KEY_DOWN]);
    settle(&mut session, "05-music");
    type_keys(&mut session, &[KEY_ENTER]);
    advance(&mut session, 0.06);
    session.screenshot_png("06-starting");
    for client in [&mut a, &mut b, &mut c] {
        client
            .pump(Some(std::time::Duration::from_millis(0)))
            .unwrap();
    }
    session.wait(&mut [&mut a, &mut b, &mut c], "the music player", |s| {
        window(s, "music").is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
    });
    settle(&mut session, "07-started");

    // Up close on the main monitor.
    logo_key(&mut session, KEY_DOWN, false);
    logo_key(&mut session, KEY_F, false);
    settle(&mut session, "08-up-close");
    logo_key(&mut session, KEY_D, false);
    type_keys(&mut session, &[KEY_A]);
    settle(&mut session, "09-up-close-launcher");
    type_keys(&mut session, &[KEY_ESC]);
    logo_key(&mut session, KEY_F, false);

    // On the monitor that stands upright.
    logo_key(&mut session, KEY_L, false);
    settle(&mut session, "10-right");
    logo_key(&mut session, KEY_D, false);
    settle(&mut session, "11-right-launcher");
    type_keys(&mut session, &[KEY_ESC]);
    settle(&mut session, "12-closed");
    session.assert_script_healthy();
    stop_started(&mut session);
    session.quit();
}

/// How fast the room is drawn as a window of another compositor: frames per
/// second that reach the parent, and what the script's `frame` costs. The
/// parent is a headless compositor started here. Not a test: run by hand,
/// on the device to measure (`HIZUKURI_TEST_DEVICES=gpu` for a hardware
/// device, with the Vulkan loader pointed at its driver),
///
/// ```sh
/// cargo test -p proto-compositor --test room -- --ignored speed --nocapture
/// ```
///
/// `HIZUKURI_ROOM_PICTURE_SIZE` is the size of the nested output (default
/// `1920x1080`), `HIZUKURI_ROOM_SAMPLES` overrides the samples per pixel.
#[test]
#[ignore = "a measurement, run by hand (see the documentation)"]
fn speed() {
    use std::time::{Duration, Instant};

    let hardware = std::env::var("HIZUKURI_TEST_DEVICES").is_ok_and(|v| v == "gpu");
    let device = Device::new(if hardware {
        common::Gpu::Hardware
    } else {
        common::Gpu::Lavapipe
    });
    let size = std::env::var("HIZUKURI_ROOM_PICTURE_SIZE").unwrap_or_else(|_| "1920x1080".into());
    let project = project();
    common::build_shaders(&project);
    let parent = Session::start(
        "room-speed-parent",
        device,
        &["--output-size", &size, "--dmabuf", "gpu"],
    );
    let mut session = Session::start_nested(
        "room-speed",
        device,
        &parent,
        &[
            "--output-size",
            &size,
            "--script",
            project.to_str().unwrap(),
        ],
    );
    if let Ok(samples) = std::env::var("HIZUKURI_ROOM_SAMPLES") {
        session.script(
            "room_set",
            json!(["samples", samples.parse::<u32>().unwrap()]),
        );
    }
    let frames = |session: &mut Session| {
        let state = session.state();
        (
            state["backend_status"]["output"]["frames_presented"]
                .as_u64()
                .unwrap_or(0),
            state["script"]["timing"]["frame"]["calls"]
                .as_f64()
                .unwrap_or(0.0),
            state["script"]["timing"]["frame"]["mean_us"]
                .as_f64()
                .unwrap_or(0.0),
        )
    };
    let measure = |session: &mut Session, clients: &mut [&mut TestClient], what: &str| {
        // Let it settle, then count over three seconds.
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            for client in clients.iter_mut() {
                client.pump(Some(Duration::from_millis(5))).unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (frames0, calls0, mean0) = frames(session);
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            for client in clients.iter_mut() {
                client.pump(Some(Duration::from_millis(5))).unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let seconds = started.elapsed().as_secs_f64();
        let (frames1, calls1, mean1) = frames(session);
        let script = (mean1 * calls1 - mean0 * calls0) / (calls1 - calls0).max(1.0);
        println!(
            "{what}: {:.1} frames per second to the parent, {:.1} frames drawn per second, \
             `frame` {:.0} us",
            (frames1 - frames0) as f64 / seconds,
            (calls1 - calls0) / seconds,
            script
        );
    };

    // Every monitor shows the NO SIGNAL picture, which moves: every frame
    // draws four screens, their mip levels, and the room.
    measure(&mut session, &mut [], "four monitors without a signal");
    let quadrants = |title: &str| ClientOptions {
        pattern: Pattern::Quadrants([RED, GREEN, BLUE, YELLOW]),
        ..options(title, 100, 60, WHITE)
    };
    let (mut a, _) = open_on(&mut session, &mut [], quadrants("a"), MAIN);
    let (mut b, _) = open_on(&mut session, &mut [&mut a], quadrants("b"), LEFT);
    measure(
        &mut session,
        &mut [&mut a, &mut b],
        "two windows that stand still, two monitors without a signal",
    );
    logo_key(&mut session, KEY_F, false);
    measure(&mut session, &mut [&mut a, &mut b], "up close");
    let state = session.state();
    println!("device: {}", state["gpu"]);
    println!("backend: {:#}", state["backend_status"]);
    session.assert_script_healthy();
    drop(a);
    drop(b);
    session.quit();
    parent.quit();
}

/// Waits for where the compositor puts the popup that was opened last
/// (relative to the window geometry, and its size).
fn popup_configured(client: &mut TestClient) -> (i32, i32, i32, i32) {
    match client
        .wait_for(common::TIMEOUT, |e| {
            matches!(e, ClientEvent::PopupConfigure { .. })
        })
        .unwrap()
    {
        Some(ClientEvent::PopupConfigure {
            x,
            y,
            width,
            height,
        }) => (x, y, width, height),
        _ => panic!("the popup was not configured: {:?}", client.events()),
    }
}

/// Whether the window `title` shows a popup at `offset` (relative to the
/// window geometry) with contents.
fn popup_shown(state: &Value, title: &str, offset: (i32, i32)) -> bool {
    window(state, title).is_some_and(|w| {
        w["surfaces"].as_array().unwrap().iter().any(|s| {
            s["role"] == "popup"
                && s["buffer_size"].is_array()
                && s["offset"] == json!([offset.0, offset.1])
        })
    })
}

/// The popups of a window are kept on the monitor that shows it, not on
/// the output: on the portrait monitor (720x1280), a menu near the bottom
/// of the window, far below the output's height, goes where it asked to;
/// one that would leave the monitor is flipped above its anchor. The popup
/// is drawn on the monitor. When the window moves to another monitor, a
/// reactive popup (`xdg_positioner.set_reactive`) is placed again on the
/// new one at once; another one stays where it was until its client asks
/// (`xdg_popup.reposition`).
fn popups_stay_on_the_monitor_that_shows_their_window(device: Device) {
    let mut session = start("room-popup", device, "1280x720");
    let (mut a, ida) = open_on(&mut session, &mut [], options("a", 100, 60, RED), MAIN);
    // Onto the portrait monitor at the right.
    logo_key(&mut session, KEY_L, true);
    assert_eq!(shown(&mut session, RIGHT), ida);
    session.wait(&mut [&mut a], "a on the portrait monitor", |s| {
        window(s, "a").is_some_and(|w| {
            w["size"] == json!([720, 1280]) && w["popup_bounds"] == json!([0, 0, 720, 1280])
        })
    });
    settle(&mut session, "popup-00-window");

    // Below a point 1000 down: 1001 to 1151, inside the monitor. (Kept
    // inside the 1280x720 output instead, it would have been slid up to
    // 570.)
    let low = PopupPositioner::menu_at(100, 1000, 200, 150);
    a.clear_events();
    a.open_positioned_popup(low, YELLOW, false).unwrap();
    assert_eq!(popup_configured(&mut a), (101, 1001, 200, 150));
    session.wait(&mut [&mut a], "the popup drawn", |s| {
        popup_shown(s, "a", (101, 1001))
    });
    let image = settle(&mut session, "popup-01-low");
    // It is drawn on the monitor, where the window has it.
    let middle = on_screen(&mut session, RIGHT, 201.0, 1076.0);
    assert_rgb_near(&image, middle.0, middle.1, YELLOW, "the popup");
    let beside = on_screen(&mut session, RIGHT, 500.0, 1076.0);
    assert_rgb_near(&image, beside.0, beside.1, RED, "the window beside it");
    a.close_popup();

    // Below a point 1200 down it would leave the monitor: flipped above.
    a.clear_events();
    a.open_positioned_popup(PopupPositioner::menu_at(100, 1200, 200, 150), YELLOW, false)
        .unwrap();
    assert_eq!(popup_configured(&mut a), (101, 1050, 200, 150));
    session.wait(&mut [&mut a], "the flipped popup drawn", |s| {
        popup_shown(s, "a", (101, 1050))
    });
    settle(&mut session, "popup-02-flipped");
    a.close_popup();

    // A reactive popup, open while the window moves to the main monitor
    // (1920x1080): 1001 to 1151 is below that monitor, so it is placed
    // again, flipped above the point, without being asked.
    let reactive = PopupPositioner {
        reactive: true,
        ..low
    };
    a.clear_events();
    a.open_positioned_popup(reactive, YELLOW, false).unwrap();
    assert_eq!(popup_configured(&mut a), (101, 1001, 200, 150));
    session.wait(&mut [&mut a], "the reactive popup drawn", |s| {
        popup_shown(s, "a", (101, 1001))
    });
    a.clear_events();
    logo_key(&mut session, KEY_H, true);
    assert_eq!(shown(&mut session, MAIN), ida);
    assert_eq!(popup_configured(&mut a), (101, 850, 200, 150));
    session.wait(&mut [&mut a], "the reactive popup moved along", |s| {
        window(s, "a").is_some_and(|w| w["popup_bounds"] == json!([0, 0, 1920, 1080]))
            && popup_shown(s, "a", (101, 850))
    });
    let image = settle(&mut session, "popup-03-reactive-moved");
    let middle = on_screen(&mut session, MAIN, 201.0, 925.0);
    assert_rgb_near(&image, middle.0, middle.1, YELLOW, "the reactive popup");
    // Back on the portrait monitor, it goes back below the point.
    a.clear_events();
    logo_key(&mut session, KEY_L, true);
    assert_eq!(shown(&mut session, RIGHT), ida);
    assert_eq!(popup_configured(&mut a), (101, 1001, 200, 150));
    session.wait(&mut [&mut a], "the reactive popup moved back", |s| {
        popup_shown(s, "a", (101, 1001))
    });
    a.close_popup();

    // One that is not reactive stays where it was put, partly off the main
    // monitor, until its client asks for a new place.
    a.clear_events();
    a.open_positioned_popup(low, YELLOW, false).unwrap();
    assert_eq!(popup_configured(&mut a), (101, 1001, 200, 150));
    session.wait(&mut [&mut a], "the popup drawn again", |s| {
        popup_shown(s, "a", (101, 1001))
    });
    a.clear_events();
    logo_key(&mut session, KEY_H, true);
    assert_eq!(shown(&mut session, MAIN), ida);
    session.wait(&mut [&mut a], "a on the main monitor", |s| {
        window(s, "a").is_some_and(|w| {
            w["size"] == json!([1920, 1080]) && w["popup_bounds"] == json!([0, 0, 1920, 1080])
        })
    });
    let image = settle(&mut session, "popup-04-moved");
    a.pump(Some(std::time::Duration::from_millis(50))).unwrap();
    assert!(
        !a.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PopupConfigure { .. })),
        "{:?}",
        a.events()
    );
    let top = on_screen(&mut session, MAIN, 201.0, 1040.0);
    assert_rgb_near(&image, top.0, top.1, YELLOW, "what is left of the popup");
    a.clear_events();
    a.reposition_popup(low, 7).unwrap();
    a.wait_for(common::TIMEOUT, |e| {
        *e == ClientEvent::PopupRepositioned { token: 7 }
    })
    .unwrap()
    .expect("the popup is repositioned");
    assert_eq!(popup_configured(&mut a), (101, 850, 200, 150));
    session.wait(&mut [&mut a], "the repositioned popup drawn", |s| {
        popup_shown(s, "a", (101, 850))
    });
    let image = settle(&mut session, "popup-05-repositioned");
    let middle = on_screen(&mut session, MAIN, 201.0, 925.0);
    assert_rgb_near(&image, middle.0, middle.1, YELLOW, "the repositioned popup");
    a.close_popup();
    session.assert_script_healthy();
    session.quit();
}

/// The candidate window of an input method is kept on the monitor of the
/// window that has the text input, as popups are: on the portrait monitor
/// it goes below a cursor far below the output's height, above one at the
/// bottom of the monitor, and inside the monitor's width.
fn the_candidates_of_an_input_method_stay_on_the_monitor(device: Device) {
    let mut session = start("room-input-popup", device, "1280x720");
    let mut ime = session.client(ClientOptions {
        window: false,
        input_method: Some(InputMethodMode::Manual),
        ..Default::default()
    });
    let (mut app, ida) = open_on(
        &mut session,
        &mut [&mut ime],
        ClientOptions {
            text_input: true,
            ..options("field", 100, 60, RED)
        },
        MAIN,
    );
    logo_key(&mut session, KEY_L, true);
    assert_eq!(shown(&mut session, RIGHT), ida);
    session.wait(
        &mut [&mut app, &mut ime],
        "the field on the portrait monitor",
        |s| {
            window(s, "field").is_some_and(|w| w["popup_bounds"] == json!([0, 0, 720, 1280]))
                && s["input_method"]["bound"] == true
        },
    );
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while !app.text_input_entered() {
        assert!(
            std::time::Instant::now() < deadline,
            "the field never got the text input"
        );
        app.pump(Some(std::time::Duration::from_millis(10)))
            .unwrap();
        ime.pump(Some(std::time::Duration::from_millis(1))).unwrap();
    }
    app.text_input_enable(Some((100, 1000, 2, 20))).unwrap();
    session.wait(&mut [&mut app, &mut ime], "active", |s| {
        s["input_method"]["active"] == true
    });
    ime.ime_show_popup(120, 40, YELLOW).unwrap();
    let candidates_at = |session: &mut Session,
                         app: &mut TestClient,
                         ime: &mut TestClient,
                         what: &str,
                         at: [i32; 2]| {
        session.wait(&mut [app, ime], what, |s| {
            s["input_method"]["popup"]["position"] == json!(at)
                && window(s, "field").is_some_and(|w| {
                    w["surfaces"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|n| n["role"] == "input-popup" && n["has_contents"] == true)
                })
        });
    };
    // Below the cursor (1020 to 1060): far below the output's 720, inside
    // the monitor.
    candidates_at(
        &mut session,
        &mut app,
        &mut ime,
        "below the cursor",
        [100, 1020],
    );
    let image = settle(&mut session, "popup-06-candidates");
    let middle = on_screen(&mut session, RIGHT, 160.0, 1040.0);
    assert_rgb_near(&image, middle.0, middle.1, YELLOW, "the candidates");
    // At the bottom of the monitor: above the cursor.
    app.text_input_set_cursor((100, 1250, 2, 20)).unwrap();
    candidates_at(
        &mut session,
        &mut app,
        &mut ime,
        "above the cursor",
        [100, 1210],
    );
    // At the right edge of the monitor: moved in, to end at 720.
    app.text_input_set_cursor((700, 500, 2, 20)).unwrap();
    candidates_at(
        &mut session,
        &mut app,
        &mut ime,
        "inside the width",
        [600, 520],
    );
    settle(&mut session, "popup-07-candidates-right");
    app.text_input_disable().unwrap();
    session.assert_script_healthy();
    session.quit();
}
