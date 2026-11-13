//! What clients ask of the window manager, decided by the script: the
//! built-in tiling script lets dialogs and windows of a fixed size float,
//! covers the output with a window that asks to be fullscreen, and follows
//! activation requests that came from input. These are the windows
//! xwayland-satellite makes of X11 windows (transient windows, splash
//! screens, `_NET_WM_STATE_FULLSCREEN`, `_NET_ACTIVE_WINDOW`); the test
//! client makes the same requests.

mod common;

use common::{Device, Session, options, window};
use proto_test_client::{
    ClientEvent, ClientOptions, DialogOptions, InputMethodMode, ToplevelRequest,
};
use serde_json::Value;

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BTN_LEFT: u32 = 0x110;

fn id(state: &Value, title: &str) -> u64 {
    window(state, title).unwrap()["id"].as_u64().unwrap()
}

fn geometry(state: &Value, title: &str) -> Option<([i64; 2], [i64; 2])> {
    let w = window(state, title)?;
    let pair = |v: &Value| Some([v[0].as_i64()?, v[1].as_i64()?]);
    Some((pair(&w["position"])?, pair(&w["size"])?))
}

/// The ids in stacking order, bottom to top.
fn stack(state: &Value) -> Vec<u64> {
    state["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .collect()
}

fn dialogs_float_and_fullscreen_covers_the_output(device: Device) {
    let mut session = Session::start_builtin(
        "script-window-requests",
        device,
        &["--output-size", "640x400"],
    );
    let mut a = session.client(options("a", 100, 60, RED));
    let mut b = session.client(options("b", 100, 60, GREEN));
    session.wait(&mut [&mut a, &mut b], "two tiles", |s| {
        geometry(s, "a") == Some(([8, 8], [308, 384]))
            && geometry(s, "b") == Some(([324, 8], [308, 384]))
    });

    // A dialog of a: centered over a at the size it chose, above the
    // tiles, which stay as they are.
    a.open_dialog(DialogOptions {
        title: "dialog".to_string(),
        width: 120,
        height: 80,
        ..Default::default()
    })
    .unwrap();
    let state = session.wait(&mut [&mut a, &mut b], "the dialog floats", |s| {
        geometry(s, "dialog") == Some(([8 + (308 - 120) / 2, 8 + (384 - 80) / 2], [120, 80]))
    });
    assert_eq!(window(&state, "dialog").unwrap()["parent"], id(&state, "a"));
    assert_eq!(geometry(&state, "a"), Some(([8, 8], [308, 384])));
    assert_eq!(*stack(&state).last().unwrap(), id(&state, "dialog"));
    assert!(a.events().iter().any(|e| matches!(
        e,
        ClientEvent::DialogConfigure {
            width: 0,
            height: 0,
            ..
        }
    )));

    // b asks to be fullscreen: it covers the output, on top, and is told.
    b.request(ToplevelRequest::Fullscreen).unwrap();
    // a has the output to itself among the tiles (once it redrew at its
    // new size).
    let state = session.wait(&mut [&mut a, &mut b], "b is fullscreen", |s| {
        geometry(s, "b") == Some(([0, 0], [640, 400]))
            && window(s, "b").unwrap()["fullscreen"] == true
            && geometry(s, "a") == Some(([8, 8], [624, 384]))
    });
    assert_eq!(*stack(&state).last().unwrap(), id(&state, "b"));
    assert_eq!(state["focus"]["window"], id(&state, "b"));
    assert!(b.events().contains(&ClientEvent::WindowState {
        maximized: false,
        fullscreen: true,
        resizing: false,
    }));

    // And back.
    b.request(ToplevelRequest::Unfullscreen).unwrap();
    session.wait(&mut [&mut a, &mut b], "b is tiled again", |s| {
        geometry(s, "b") == Some(([324, 8], [308, 384]))
            && window(s, "b").unwrap()["fullscreen"] == false
            && geometry(s, "a") == Some(([8, 8], [308, 384]))
    });
    session.assert_script_healthy();
    session.quit();
}

fn activation_follows_input(device: Device) {
    let mut session =
        Session::start_builtin("script-activation", device, &["--output-size", "640x400"]);
    // An input method is around, and a has a text field: the focus moved
    // by an activation goes through what the input method is told.
    let mut ime = session.client(ClientOptions {
        window: false,
        input_method: Some(InputMethodMode::Manual),
        ..Default::default()
    });
    let mut a = session.client(ClientOptions {
        text_input: true,
        ..options("a", 100, 60, RED)
    });
    session.wait(&mut [&mut a, &mut ime], "a", |s| {
        window(s, "a").is_some() && s["input_method"]["bound"] == true
    });
    a.open_dialog(DialogOptions::default()).unwrap();
    let mut b = session.client(options("b", 100, 60, GREEN));
    let state = session.wait(&mut [&mut a, &mut b], "b has the focus", |s| {
        window(s, "dialog").is_some()
            && window(s, "b").is_some()
            && s["focus"]["window"] == id(s, "b")
    });
    let (ida, idb) = (id(&state, "a"), id(&state, "b"));

    // a has no input to show for it: the focus stays with b.
    a.activate(None, true).unwrap();
    for _ in 0..20 {
        a.pump(Some(std::time::Duration::from_millis(5))).unwrap();
    }
    assert_eq!(session.state()["focus"]["window"], idb);

    // The user clicks a; then a activates its dialog with that click.
    let at = window(&session.state(), "a").unwrap()["position"].clone();
    let (x, y) = (
        at[0].as_f64().unwrap() + 20.0,
        at[1].as_f64().unwrap() + 20.0,
    );
    session.send(proto_compositor::control::Request::PointerMotion { x, y });
    session.send(proto_compositor::control::Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    session.send(proto_compositor::control::Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    session.wait(&mut [&mut a, &mut b, &mut ime], "a has the focus", |s| {
        s["focus"]["window"] == ida
    });
    a.wait_for(common::TIMEOUT, |e| {
        matches!(e, ClientEvent::PointerButton { pressed: false, .. })
    })
    .unwrap()
    .expect("a saw the click");
    // Its field takes the text input; the input method becomes active.
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    while !a.text_input_entered() {
        assert!(
            std::time::Instant::now() < deadline,
            "a's field never got the text input"
        );
        a.pump(Some(std::time::Duration::from_millis(10))).unwrap();
    }
    a.text_input_enable(Some((10, 10, 2, 16))).unwrap();
    session.wait(
        &mut [&mut a, &mut b, &mut ime],
        "the input method is active",
        |s| s["input_method"]["active"] == true,
    );
    a.activate(Some(0), true).unwrap();
    session.wait(
        &mut [&mut a, &mut b, &mut ime],
        "the dialog has the focus",
        |s| s["focus"]["window"] == id(s, "dialog") && s["input_method"]["active"] == false,
    );
    // The field of the window that lost the focus was told so.
    a.wait_for(common::TIMEOUT, |e| {
        matches!(e, ClientEvent::TextInputLeave)
    })
    .unwrap()
    .expect("a's field left");
    session.assert_script_healthy();
    session.quit();
}

device_tests!(
    dialogs_float_and_fullscreen_covers_the_output,
    activation_follows_input
);
