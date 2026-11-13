//! Real applications in a headless compositor.
//!
//! The other tests talk to the compositor with the project's own test
//! client, which was written against the same reading of the protocols as
//! the compositor. These use programs other people wrote: a toolkit's menu
//! for popups and their grabs, wl-clipboard for the selection, a GL
//! terminal for fractional scale and viewports.
//!
//! Which applications are used is said by `HIZUKURI_TEST_REAL_CLIENTS`
//! (see `common::real_client_wanted`). By default it is `weston-terminal`,
//! `wl-clipboard` and `foot`. `alacritty` runs when it is listed;
//! `alacritty-dmabuf` also needs a GL driver on a GPU.
//!
//! Nothing here reaches a session that is running where the tests are
//! started: every application gets the test session's own runtime directory
//! and `WAYLAND_DISPLAY`, and no `DISPLAY`.

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{Device, Gpu, Session, TIMEOUT, options, real_client_wanted as wanted};
use proto_compositor::control::Request;
use proto_test_client::{ClientEvent, SurfaceKind};
use serde_json::Value;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// The device these tests run on: one is enough, they are about protocols.
/// The script draws; the fallback places the windows.
fn device() -> Device {
    let hardware = Device::new(Gpu::Hardware);
    if hardware.enabled() {
        hardware
    } else {
        Device::new(Gpu::Lavapipe)
    }
}

/// Polls the compositor's state until `done` returns something.
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

fn mapped_window(state: &Value, app_id: &str) -> Option<Value> {
    state["windows"]
        .as_array()?
        .iter()
        .find(|w| {
            w["app_id"].as_str().is_some_and(|id| id.contains(app_id))
                && w["mapped"] == true
                && w["surfaces"][0]["has_contents"] == true
        })
        .cloned()
}

fn popups(window: &Value) -> Vec<Value> {
    window["surfaces"]
        .as_array()
        .map(|surfaces| {
            surfaces
                .iter()
                .filter(|s| s["role"] == "popup")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn click(session: &mut Session, button: u32, x: f64, y: f64) {
    session.send(Request::PointerMotion { x, y });
    session.send(Request::PointerButton {
        button,
        pressed: true,
    });
    session.send(Request::PointerButton {
        button,
        pressed: false,
    });
}

/// weston-terminal's context menu: a popup with a grab, made by a toolkit
/// that is not ours. It opens where the pointer is, is drawn, and goes
/// away when something else is pressed.
#[test]
fn a_toolkit_menu_opens_as_a_popup_and_is_dismissed_by_a_press_outside() {
    if !wanted("weston-terminal", &["weston-terminal"]) {
        return;
    }
    let mut session = Session::start("real-menu", device(), &["--output-size", "1280x800"]);
    let _terminal = session.spawn_client("weston-terminal", "weston-terminal", &[]);
    let window = wait_state(&mut session, "the terminal's window", |s| {
        mapped_window(s, "weston")
    });
    let id = window["id"].as_u64().unwrap();
    session.send(Request::MoveWindow {
        window: id,
        x: 100,
        y: 100,
    });
    let size = wait_state(&mut session, "the terminal moved", |s| {
        let w = mapped_window(s, "weston")?;
        (w["position"] == serde_json::json!([100, 100])).then(|| w["size"].clone())
    });
    let (width, height) = (size[0].as_i64().unwrap(), size[1].as_i64().unwrap());
    assert!(popups(&window).is_empty());

    // Right click in the middle of the terminal.
    let (x, y) = (100.0 + width as f64 / 2.0, 100.0 + height as f64 / 2.0);
    click(&mut session, BTN_RIGHT, x, y);
    let popup = wait_state(&mut session, "the menu", |s| {
        let window = mapped_window(s, "weston")?;
        popups(&window)
            .into_iter()
            .find(|p| p["has_contents"] == true)
    });
    let menu_size = (
        popup["size"][0].as_i64().unwrap(),
        popup["size"][1].as_i64().unwrap(),
    );
    assert!(menu_size.0 > 20 && menu_size.1 > 20, "{popup:#}");
    // It opened at the pointer: the popup's corner is where the click was,
    // relative to the window (the toolkit adds a margin for its shadow, so
    // allow some slack).
    let offset = (
        popup["offset"][0].as_i64().unwrap(),
        popup["offset"][1].as_i64().unwrap(),
    );
    assert!(
        (offset.0 - width / 2).abs() <= 64 && (offset.1 - height / 2).abs() <= 64,
        "the menu is at {offset:?} in a {width}x{height} window"
    );
    // The menu took the keyboard (a grab): the compositor's idea of the
    // focused window is unchanged, the focused surface is the popup's.
    let state = session.state();
    assert_eq!(state["focus"]["window"], id);
    assert_eq!(state["focus"]["keyboard_surface"], popup["id"]);
    session.screenshot("menu-open");

    // A press on the background dismisses it.
    click(&mut session, BTN_LEFT, 1200.0, 750.0);
    wait_state(&mut session, "the menu to go away", |s| {
        let window = mapped_window(s, "weston")?;
        popups(&window).is_empty().then_some(())
    });
    // The keyboard is back where the compositor wants it. The press was
    // on the background, which takes the focus off the terminal.
    let state = wait_state(&mut session, "the focus to settle", |s| {
        (s["focus"]["keyboard_surface"] != popup["id"]).then(|| s.clone())
    });
    assert_eq!(
        state["focus"]["keyboard_surface"].is_null(),
        state["focus"]["window"].is_null(),
        "the keyboard focus and the focused window disagree: {}",
        state["focus"]
    );

    // Open it again and pick nothing: Escape is the toolkit's own way out,
    // and it only works if the popup really has the keyboard.
    click(&mut session, BTN_RIGHT, x, y);
    wait_state(&mut session, "the menu again", |s| {
        let window = mapped_window(s, "weston")?;
        (!popups(&window).is_empty()).then_some(())
    });
    session.screenshot("menu-open-again");
    session.assert_drawing_clean();
    session.quit();
}

/// wl-clipboard against the selection: `wl-copy` owns it and `wl-paste`
/// reads it; the test client pastes what `wl-copy` copied; `wl-paste`
/// reads what the test client copied. wl-clipboard uses data control
/// (`ext_data_control_manager_v1` or `zwlr_data_control_manager_v1`) when
/// the compositor offers it, as this one does: it opens no window and
/// needs no focus.
#[test]
fn the_selection_works_with_wl_clipboard() {
    if !wanted("wl-clipboard", &["wl-copy", "wl-paste"]) {
        return;
    }
    let mut session = Session::start("real-clipboard", device(), &[]);
    let paste = |session: &Session, args: &[&str]| -> String {
        let output = session
            .client_command("wl-paste")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("running wl-paste");
        assert!(
            output.status.success(),
            "wl-paste {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // wl-copy to wl-paste.
    let copier = session.spawn_client("wl-copy", "wl-copy", &["--foreground", "copied by wl-copy"]);
    // Wait until it owns the selection, by looking at the compositor, not
    // by trying to paste. Without data control, wl-clipboard gets at the
    // selection through a window of its own that takes the keyboard focus,
    // and two of them racing for the focus can lose a copy for good.
    wait_state(&mut session, "wl-copy to own the selection", |s| {
        s["selection"]
            .as_array()
            .is_some_and(|types| types.iter().any(|t| t == "text/plain;charset=utf-8"))
            .then_some(())
    });
    // Data control: no window.
    assert_eq!(session.state()["windows"], serde_json::json!([]));
    let types = paste(&session, &["--list-types"]);
    assert!(
        types.lines().any(|t| t == "text/plain;charset=utf-8"),
        "{types}"
    );
    assert_eq!(paste(&session, &["--no-newline"]), "copied by wl-copy");

    // wl-copy to our client: it sees the offer when it gets the keyboard.
    let (mut client, _) = session.mapped_client(options("paster", 100, 100, [255; 4]));
    client
        .wait_for(TIMEOUT, |e| {
            matches!(e, ClientEvent::Selection { mime_types }
                if mime_types.iter().any(|t| t == "text/plain;charset=utf-8"))
        })
        .unwrap()
        .expect("the selection is offered to the focused client");
    assert!(
        client
            .request_selection("text/plain;charset=utf-8")
            .unwrap()
    );
    let received = client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::Received { .. }))
        .unwrap();
    assert_eq!(
        received,
        Some(ClientEvent::Received {
            mime_type: "text/plain;charset=utf-8".to_string(),
            text: "copied by wl-copy".to_string(),
        })
    );

    // Our client to wl-paste. A key press gives the client a serial to
    // claim the selection with.
    assert_eq!(client.keyboard_focus(), Some(SurfaceKind::Window));
    session.send(Request::Key {
        keycode: 30,
        pressed: true,
    });
    session.send(Request::Key {
        keycode: 30,
        pressed: false,
    });
    client
        .wait_for(TIMEOUT, |e| matches!(e, ClientEvent::Key { .. }))
        .unwrap()
        .expect("the key arrives");
    client
        .set_selection(&["text/plain;charset=utf-8"], "copied by the test client")
        .unwrap();
    // The request has to arrive while our client has the focus, so it is
    // sent and seen to have taken effect before `wl-paste` runs.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.pump(Some(Duration::from_millis(10))).unwrap();
        if session.state()["selection"] == serde_json::json!(["text/plain;charset=utf-8"]) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the test client never owned the selection"
        );
    }
    // Our client has to answer while wl-paste waits for the data, so it is
    // pumped from here.
    let mut child = session
        .client_command("wl-paste")
        .arg("--no-newline")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starting wl-paste");
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        client.pump(Some(Duration::from_millis(10))).unwrap();
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("wl-paste did not finish: {:?}", client.events());
        }
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "copied by the test client"
    );
    // wl-copy was told that it lost the selection, and ended.
    drop(copier);
    session.quit();
}

/// A terminal that draws with GL and follows `wp_fractional_scale_v1`: on
/// an output of scale 1.5 its buffer is 1.5 times its surface, sized back
/// with a viewport, and its content shows up.
fn fractional_terminal(name: &str, program: &str, app_id: &str, dmabuf: bool) {
    let mut args = vec!["--output-size", "1500x1050", "--output-scale", "1.5"];
    if dmabuf {
        args.extend(["--dmabuf", "cpu"]);
    }
    let mut session = Session::start(name, device(), &args);
    let _terminal = session.spawn_client(program, program, &[]);
    let window = wait_state(&mut session, "the terminal's window", |s| {
        mapped_window(s, app_id)
    });
    let surface = &window["surfaces"][0];
    let size = (
        surface["size"][0].as_f64().unwrap(),
        surface["size"][1].as_f64().unwrap(),
    );
    let buffer = (
        surface["buffer_size"][0].as_f64().unwrap(),
        surface["buffer_size"][1].as_f64().unwrap(),
    );
    assert_eq!(surface["buffer_scale"], 1, "{surface:#}");
    assert!(
        (buffer.0 - size.0 * 1.5).abs() <= 1.0 && (buffer.1 - size.1 * 1.5).abs() <= 1.0,
        "a {size:?} surface with a {buffer:?} buffer is not at scale 1.5"
    );
    if dmabuf {
        // With hardware GL the buffers come as dmabufs, which this driver
        // lets the compositor map.
        assert_eq!(surface["buffer_kind"], "dmabuf", "{surface:#}");
    }
    session.screenshot("terminal");
    session.assert_drawing_clean();
    session.quit();
}

#[test]
fn alacritty_follows_the_fractional_scale() {
    if wanted("alacritty", &["alacritty"]) {
        fractional_terminal("real-alacritty", "alacritty", "Alacritty", false);
    }
}

#[test]
fn alacritty_draws_into_dmabufs_the_compositor_can_read() {
    // Needs a GPU whose GL driver hands out buffers that can be mapped; on
    // software GL the buffers are wl_shm anyway.
    if wanted("alacritty-dmabuf", &["alacritty"]) {
        fractional_terminal("real-alacritty-dmabuf", "alacritty", "Alacritty", true);
    }
}

#[test]
fn foot_follows_the_fractional_scale() {
    if wanted("foot", &["foot"]) {
        fractional_terminal("real-foot", "foot", "foot", false);
    }
}

// ---------------------------------------------------------------------------
// Isolation from a session that is already running
// ---------------------------------------------------------------------------

/// What a real application started by a test finds in its environment:
/// nothing that leads to a session running where the tests are started,
/// whatever the test process itself inherited.
#[test]
fn applications_are_started_without_a_way_to_the_desktop() {
    let session = Session::start("real-isolation", device(), &[]);
    // As if the test had been started from a terminal of a running session:
    // the standard variables, and variables of programs the tests know
    // nothing about that name sockets in that session's runtime directory.
    // `isolate` runs after these and must win.
    let mut command = std::process::Command::new("env");
    for (variable, value) in [
        ("WAYLAND_DISPLAY", "wayland-1"),
        ("WAYLAND_SOCKET", "7"),
        ("DISPLAY", ":0"),
        ("XAUTHORITY", "/nonexistent/xauthority"),
        (
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/runtime/bus",
        ),
        (
            "SOME_COMPOSITOR_SOCKET",
            "/nonexistent/runtime/compositor.sock",
        ),
        ("SOME_AGENT_ADDRESS", "unix:path=/nonexistent/runtime/agent"),
        ("XDG_RUNTIME_DIR", "/nonexistent/runtime"),
        (
            "UNRELATED_PATH",
            "/nonexistent/runtime-of-something-else/file",
        ),
    ] {
        command.env(variable, value);
    }
    common::isolate(&mut command, &session.runtime_dir);
    let output = command.output().expect("running env");
    let environment = String::from_utf8_lossy(&output.stdout).into_owned();
    let value_of = |name: &str| {
        environment
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
    };
    for variable in [
        "WAYLAND_DISPLAY",
        "WAYLAND_SOCKET",
        "DISPLAY",
        "XAUTHORITY",
        "DBUS_SESSION_BUS_ADDRESS",
        "SOME_COMPOSITOR_SOCKET",
        "SOME_AGENT_ADDRESS",
    ] {
        assert_eq!(value_of(variable), None, "{variable} leaked");
    }
    // What does not lead into that session is left alone.
    assert_eq!(
        value_of("UNRELATED_PATH"),
        Some("/nonexistent/runtime-of-something-else/file")
    );
    assert_eq!(
        value_of("XDG_RUNTIME_DIR"),
        session.runtime_dir.to_str(),
        "the runtime directory is not the session's"
    );

    // The command for applications adds this session's socket and a home
    // of its own.
    let output = session.client_command("env").output().expect("running env");
    let environment = String::from_utf8_lossy(&output.stdout).into_owned();
    let value_of = |name: &str| {
        environment
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
    };
    assert_eq!(value_of("WAYLAND_DISPLAY"), Some("hizukuri-test"));
    assert_eq!(value_of("DISPLAY"), None);
    assert_eq!(value_of("WAYLAND_SOCKET"), None);
    assert_eq!(value_of("DBUS_SESSION_BUS_ADDRESS"), None);
    let runtime = session.runtime_dir.to_str().unwrap();
    for variable in ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
        assert!(
            value_of(variable).is_some_and(|value| value.starts_with(runtime)),
            "{variable} is {:?}",
            value_of(variable)
        );
    }
    // Neither a terminal nor its shell reads the startup files or the
    // configuration of whoever runs the tests.
    assert_eq!(value_of("SHELL"), Some("/bin/sh"));
    assert_eq!(value_of("XDG_CONFIG_DIRS"), Some("/etc/xdg"));
    for variable in ["ZDOTDIR", "ENV", "BASH_ENV"] {
        assert_eq!(value_of(variable), None, "{variable} leaked");
    }
    session.quit();
}

/// With the compositor gone, an application started the same way finds no
/// Wayland server at all: it fails, instead of turning up somewhere else.
#[test]
fn an_application_without_its_compositor_fails_instead_of_reaching_the_desktop() {
    if !wanted("wl-clipboard", &["wl-copy", "wl-paste"]) {
        return;
    }
    let session = Session::start("real-isolation-gone", device(), &[]);
    let command = |session: &Session| {
        let mut command = session.client_command("wl-paste");
        // Types only: should this ever connect somewhere it must not, it
        // does not read anyone's clipboard contents.
        command
            .arg("--list-types")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };
    let mut gone = command(&session);
    session.quit();
    let output = gone.output().expect("running wl-paste");
    assert!(
        !output.status.success(),
        "wl-paste reached a compositor that is not ours: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let message = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        message.contains("connect") || message.contains("wayland"),
        "unexpected failure: {message}"
    );
}
