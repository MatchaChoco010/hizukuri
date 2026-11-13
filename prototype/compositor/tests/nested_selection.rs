//! Copy and paste between a nested compositor and its parent: the clipboard
//! and the primary selection cross both ways, the data going straight from
//! the owner to the reader.
//!
//! The parent is a headless `proto-compositor` the test starts itself, with
//! a test client of its own ("outside"); the nested compositor has one too
//! ("inside").

mod common;

use std::time::{Duration, Instant};

use common::{Device, Session, TIMEOUT, options};
use proto_compositor::control::Request;
use proto_test_client::{ClientEvent, TestClient};
use serde_json::{Value, json};

const NESTED_APP_ID: &str = "hizukuri.nested";
const UTF8: &str = "text/plain;charset=utf-8";
const PLAIN: &str = "text/plain";
const PNG: &str = "image/png";
/// The mark password managers put on what they copy: it must reach the
/// parent's clipboard managers as it is.
const SECRET_HINT: &str = "x-kde-passwordManagerHint";

/// The window of the nested compositor in the parent's state.
fn nested_window(state: &Value) -> Option<u64> {
    state["windows"].as_array()?.iter().find_map(|w| {
        (w["app_id"] == NESTED_APP_ID && w["mapped"] == true && w["surfaces"][0]["size"].is_array())
            .then(|| w["id"].as_u64())
            .flatten()
    })
}

/// A parent with a client, and a nested compositor with a client.
struct Pair {
    parent: Session,
    nested: Session,
    /// The nested compositor's window in the parent.
    window: u64,
    outside: TestClient,
    outside_window: u64,
    inside: TestClient,
}

impl Pair {
    fn start(name: &str, device: Device, nested_args: &[&str]) -> Pair {
        let mut parent = Session::start(
            &format!("{name}-parent"),
            device,
            &["--output-size", "800x600"],
        );
        let mut args = vec!["--output-size", "400x300"];
        args.extend_from_slice(nested_args);
        let mut nested = Session::start_nested(&format!("{name}-child"), device, &parent, &args);
        let state = parent.wait(&mut [], "the window of the nested compositor", |state| {
            nested_window(state).is_some()
        });
        let window = nested_window(&state).unwrap();
        let (mut inside, _) = nested.mapped_client(options("inside", 120, 90, [255, 0, 0, 255]));
        assert!(
            inside
                .wait_for(TIMEOUT, |e| *e == ClientEvent::KeyboardEnter)
                .unwrap()
                .is_some(),
            "the client inside has the keyboard of the nested compositor"
        );
        let (outside, outside_window) =
            parent.mapped_client(options("outside", 100, 100, [0, 0, 255, 255]));
        Pair {
            parent,
            nested,
            window,
            outside,
            outside_window,
            inside,
        }
    }

    /// Gives the parent's keyboard focus to the nested window.
    fn focus_nested(&mut self) {
        self.parent.send(Request::FocusWindow {
            window: Some(self.window),
        });
        let window = self.window;
        self.parent.wait(
            &mut [&mut self.outside],
            "the focus on the nested window",
            |s| s["focus"]["window"].as_u64() == Some(window),
        );
    }

    /// Gives the parent's keyboard focus to the client outside.
    fn focus_outside(&mut self) {
        self.parent.send(Request::FocusWindow {
            window: Some(self.outside_window),
        });
        assert!(
            self.outside
                .wait_for(TIMEOUT, |e| *e == ClientEvent::KeyboardEnter)
                .unwrap()
                .is_some(),
            "the client outside got the parent's keyboard"
        );
    }

    /// Pumps both clients while waiting for the parent's state.
    fn wait_parent(&mut self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        self.parent
            .wait(&mut [&mut self.outside, &mut self.inside], what, done)
    }

    /// Pumps both clients while waiting for the nested compositor's state.
    fn wait_nested(&mut self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        self.nested
            .wait(&mut [&mut self.outside, &mut self.inside], what, done)
    }

    fn finish(mut self) {
        self.nested.assert_script_healthy();
        self.parent.assert_script_healthy();
        self.nested.quit();
        self.parent.quit();
    }
}

/// Waits for `client` to see a selection with these types (pumping
/// `other` too, which may have to write or answer).
fn wait_for_selection(client: &mut TestClient, other: &mut TestClient, types: &[&str]) {
    let wanted: Vec<String> = types.iter().map(|t| t.to_string()).collect();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(found) = client
            .wait_for(
                Duration::from_millis(10),
                |e| matches!(e, ClientEvent::Selection { mime_types } if *mime_types == wanted),
            )
            .unwrap()
        {
            let _ = found;
            return;
        }
        other.pump(Some(Duration::from_millis(1))).unwrap();
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a selection of {types:?}: {:?}",
            client.events()
        );
    }
}

/// Reads what arrives as `mime_type` at `reader`, pumping `writer` (the
/// owner) at the same time.
fn read(reader: &mut TestClient, writer: &mut TestClient, mime_type: &str) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(ClientEvent::Received { text, .. }) = reader
            .wait_for(
                Duration::from_millis(1),
                |e| matches!(e, ClientEvent::Received { mime_type: t, .. } if t == mime_type),
            )
            .unwrap()
        {
            return text;
        }
        writer.pump(Some(Duration::from_millis(1))).unwrap();
        assert!(
            Instant::now() < deadline,
            "timed out reading {mime_type} ({} bytes so far)",
            reader.bytes_being_read()
        );
    }
}

fn types(state: &Value, key: &str) -> Value {
    state[key].clone()
}

/// A copy inside reaches the parent with every type, in order, and the
/// parent's clients read each type from the owner inside. Its own echo
/// does not come back in and replace it.
fn a_copy_inside_reaches_the_parent(device: Device) {
    let mut pair = Pair::start("sel-out", device, &[]);
    pair.focus_nested();
    pair.inside
        .set_selection_types(&[
            (UTF8, "inside, as utf-8".as_bytes()),
            (PLAIN, b"inside, plain"),
            (PNG, b"\x89PNG not really"),
            (SECRET_HINT, b"secret"),
        ])
        .unwrap();
    let all = json!([UTF8, PLAIN, PNG, SECRET_HINT]);
    pair.wait_parent("the copy in the parent", |s| types(s, "selection") == all);
    let state = pair.wait_nested("the parent took the source", |s| {
        s["backend_status"]["selection"]["clipboard"]["to_parent"] == "owned"
    });
    // Still the client's own inside: the echo was not taken.
    assert_eq!(
        state["selection_owners"]["clipboard"], "client",
        "{state:#}"
    );
    assert_eq!(types(&state, "selection"), all);

    pair.focus_outside();
    wait_for_selection(
        &mut pair.outside,
        &mut pair.inside,
        &[UTF8, PLAIN, PNG, SECRET_HINT],
    );
    for (mime_type, wanted) in [
        (PLAIN, "inside, plain"),
        (UTF8, "inside, as utf-8"),
        (PNG, "\u{FFFD}PNG not really"),
    ] {
        assert!(pair.outside.request_selection(mime_type).unwrap());
        assert_eq!(read(&mut pair.outside, &mut pair.inside, mime_type), wanted);
    }
    // The focus coming back sends the selection again: still not taken.
    pair.focus_nested();
    let state = pair.wait_nested("the focus back", |s| {
        s["backend_status"]["selection"]["clipboard"]["focused"] == true
    });
    assert_eq!(state["selection_owners"]["clipboard"], "client");
    assert!(
        !pair.inside.events().contains(&ClientEvent::SourceCancelled),
        "the source inside was cancelled"
    );
    pair.finish();
}

/// A copy in the parent reaches the inside when the nested window has the
/// focus; the client inside reads it from the owner outside.
fn a_copy_in_the_parent_reaches_the_inside(device: Device) {
    let mut pair = Pair::start("sel-in", device, &[]);
    pair.focus_outside();
    pair.outside
        .set_selection_types(&[
            (PLAIN, b"outside, plain"),
            (UTF8, "outside, utf-8".as_bytes()),
        ])
        .unwrap();
    pair.wait_parent("the copy in the parent", |s| {
        types(s, "selection") == json!([PLAIN, UTF8])
    });
    pair.focus_nested();
    wait_for_selection(&mut pair.inside, &mut pair.outside, &[PLAIN, UTF8]);
    let state = pair.nested.state();
    assert_eq!(
        state["selection_owners"]["clipboard"], "provided",
        "{state:#}"
    );
    assert_eq!(types(&state, "selection"), json!([PLAIN, UTF8]));
    for (mime_type, wanted) in [(UTF8, "outside, utf-8"), (PLAIN, "outside, plain")] {
        assert!(pair.inside.request_selection(mime_type).unwrap());
        assert_eq!(read(&mut pair.inside, &mut pair.outside, mime_type), wanted);
    }
    // The parent's selection is still the outside client's.
    let state = pair.parent.state();
    assert_eq!(state["selection_owners"]["clipboard"], "client");
    assert!(
        !pair
            .outside
            .events()
            .contains(&ClientEvent::SourceCancelled),
        "the source outside was cancelled"
    );
    // A copy inside then wins, inside and out.
    pair.inside.set_selection(&[UTF8], "inside again").unwrap();
    pair.wait_parent("the new copy in the parent", |s| {
        types(s, "selection") == json!([UTF8])
    });
    assert!(
        pair.outside
            .wait_for(TIMEOUT, |e| *e == ClientEvent::SourceCancelled)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        pair.nested.state()["selection_owners"]["clipboard"],
        "client"
    );
    pair.finish();
}

/// Megabytes both ways: the compositors hand the pipe on and do not wait
/// for the data. While a transfer is held open, frames still go from the
/// client inside to the parent's picture.
fn large_transfers_do_not_hold_up_the_compositors(device: Device) {
    let mut pair = Pair::start("sel-large", device, &[]);
    let big: Vec<u8> = (0..8 * 1024 * 1024u32)
        .map(|i| b'a' + (i % 26) as u8)
        .collect();
    let big_text = String::from_utf8(big.clone()).unwrap();

    // Inside to outside, held open by the writer first.
    pair.focus_nested();
    pair.inside.set_selection_types(&[(PLAIN, &big)]).unwrap();
    pair.wait_parent("the copy in the parent", |s| {
        types(s, "selection") == json!([PLAIN])
    });
    pair.wait_nested("the parent took the source", |s| {
        s["backend_status"]["selection"]["clipboard"]["to_parent"] == "owned"
    });
    pair.focus_outside();
    wait_for_selection(&mut pair.outside, &mut pair.inside, &[PLAIN]);
    pair.inside.hold_transfers(true);
    assert!(pair.outside.request_selection(PLAIN).unwrap());
    let deadline = Instant::now() + TIMEOUT;
    while pair.inside.transfers_in_progress() == 0 {
        pair.outside.pump(Some(Duration::from_millis(2))).unwrap();
        pair.inside.pump(Some(Duration::from_millis(2))).unwrap();
        assert!(Instant::now() < deadline, "the owner inside was not asked");
    }
    // Both compositors go on: a new picture of the client inside reaches
    // the parent while the reader waits.
    let before = pair.parent.state()["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"].as_u64() == Some(pair.window))
        .unwrap()["surfaces"][0]["generation"]
        .as_u64()
        .unwrap();
    pair.nested
        .recolor(&mut pair.inside, "inside", [0, 255, 0, 255]);
    let window = pair.window;
    pair.parent.wait(
        &mut [&mut pair.outside],
        "a new frame of the nested window",
        |s| {
            s["windows"].as_array().unwrap().iter().any(|w| {
                w["id"].as_u64() == Some(window)
                    && w["surfaces"][0]["generation"].as_u64().unwrap() > before
            })
        },
    );
    assert_eq!(pair.outside.bytes_being_read(), 0);
    pair.inside.hold_transfers(false);
    let text = read(&mut pair.outside, &mut pair.inside, PLAIN);
    assert_eq!(text.len(), big_text.len());
    assert!(text == big_text, "the data changed on the way");

    // Outside to inside, with a reader that stops reading for a while.
    let big: Vec<u8> = big.iter().map(|b| b.to_ascii_uppercase()).collect();
    let big_text = String::from_utf8(big.clone()).unwrap();
    pair.outside.set_selection_types(&[(PLAIN, &big)]).unwrap();
    pair.wait_parent("the copy outside", |s| {
        s["selection_owners"]["clipboard"] == "client" && types(s, "selection") == json!([PLAIN])
    });
    // What the client inside saw of its own copy is of no interest now.
    pair.inside.clear_events();
    pair.focus_nested();
    pair.wait_nested("the parent's selection inside", |s| {
        s["selection_owners"]["clipboard"] == "provided"
    });
    wait_for_selection(&mut pair.inside, &mut pair.outside, &[PLAIN]);
    pair.inside.hold_reads(true);
    assert!(pair.inside.request_selection(PLAIN).unwrap());
    for _ in 0..50 {
        pair.outside.pump(Some(Duration::from_millis(2))).unwrap();
        pair.inside.pump(Some(Duration::from_millis(2))).unwrap();
    }
    // The writer waits on a full pipe; the compositors answer meanwhile.
    assert_eq!(pair.outside.transfers_in_progress(), 1);
    pair.nested.state();
    pair.parent.state();
    pair.inside.hold_reads(false);
    let text = read(&mut pair.inside, &mut pair.outside, PLAIN);
    assert_eq!(text.len(), big_text.len());
    assert!(text == big_text, "the data changed on the way");
    pair.finish();
}

/// Copied inside while the parent's keyboard is elsewhere: the copy waits,
/// and becomes the parent's selection with the next focus. The parent's
/// older selection, sent with that focus, does not replace it inside.
fn a_copy_without_the_parents_focus_waits_for_it(device: Device) {
    let mut pair = Pair::start("sel-wait", device, &[]);
    pair.focus_outside();
    pair.outside
        .set_selection(&[PLAIN], "older, outside")
        .unwrap();
    pair.wait_parent("the copy outside", |s| {
        types(s, "selection") == json!([PLAIN])
    });
    pair.inside.set_selection(&[UTF8], "newer, inside").unwrap();
    pair.wait_nested("the copy to wait", |s| {
        s["backend_status"]["selection"]["clipboard"]["to_parent"] == "waiting"
    });
    assert_eq!(types(&pair.parent.state(), "selection"), json!([PLAIN]));
    pair.focus_nested();
    pair.wait_parent("the copy inside in the parent", |s| {
        types(s, "selection") == json!([UTF8])
    });
    let state = pair.wait_nested("the parent took the source", |s| {
        s["backend_status"]["selection"]["clipboard"]["to_parent"] == "owned"
    });
    assert_eq!(
        state["selection_owners"]["clipboard"], "client",
        "{state:#}"
    );
    assert_eq!(types(&state, "selection"), json!([UTF8]));
    pair.focus_outside();
    wait_for_selection(&mut pair.outside, &mut pair.inside, &[UTF8]);
    assert!(pair.outside.request_selection(UTF8).unwrap());
    assert_eq!(
        read(&mut pair.outside, &mut pair.inside, UTF8),
        "newer, inside"
    );
    pair.finish();
}

/// An owner that goes takes its selection with it, on the other side too.
fn an_owner_that_goes_takes_the_selection_along(device: Device) {
    let mut pair = Pair::start("sel-gone", device, &[]);
    // Inside: the parent's selection goes when the owner inside does.
    pair.focus_nested();
    pair.inside.set_selection(&[PLAIN], "short lived").unwrap();
    pair.wait_parent("the copy in the parent", |s| {
        types(s, "selection") == json!([PLAIN])
    });
    let Pair {
        mut parent,
        mut nested,
        window,
        mut outside,
        outside_window,
        inside,
    } = pair;
    drop(inside);
    parent.wait(&mut [&mut outside], "the selection to go", |s| {
        s["selection"].is_null()
    });
    nested.wait(&mut [], "the selection inside to go", |s| {
        s["selection"].is_null()
    });

    // Outside: the parent's owner goes, and so does the selection inside.
    let (mut inside, _) = nested.mapped_client(options("inside-2", 120, 90, [255, 0, 0, 255]));
    parent.send(Request::FocusWindow {
        window: Some(outside_window),
    });
    assert!(
        outside
            .wait_for(TIMEOUT, |e| *e == ClientEvent::KeyboardEnter)
            .unwrap()
            .is_some()
    );
    outside.set_selection(&[PLAIN], "from outside").unwrap();
    parent.wait(&mut [&mut outside], "the copy outside", |s| {
        types(s, "selection") == json!([PLAIN])
    });
    parent.send(Request::FocusWindow {
        window: Some(window),
    });
    wait_for_selection(&mut inside, &mut outside, &[PLAIN]);
    assert_eq!(nested.state()["selection_owners"]["clipboard"], "provided");
    drop(outside);
    assert!(
        inside
            .wait_for(
                TIMEOUT,
                |e| matches!(e, ClientEvent::Selection { mime_types }
                if mime_types.is_empty())
            )
            .unwrap()
            .is_some(),
        "the selection inside did not go"
    );
    let state = nested.wait(&mut [&mut inside], "no selection inside", |s| {
        s["selection_owners"]["clipboard"] == "none"
    });
    assert!(state["selection"].is_null());
    nested.assert_script_healthy();
    nested.quit();
    parent.quit();
}

/// The primary selection crosses the same way.
fn the_primary_selection_crosses_too(device: Device) {
    let mut pair = Pair::start("sel-primary", device, &[]);
    pair.inside.enable_primary_selection();
    pair.outside.enable_primary_selection();
    pair.focus_nested();
    pair.inside
        .set_primary_selection_types(&[(UTF8, "selected inside".as_bytes())])
        .unwrap();
    pair.wait_parent("the primary selection in the parent", |s| {
        types(s, "primary_selection") == json!([UTF8])
    });
    pair.focus_outside();
    let deadline = Instant::now() + TIMEOUT;
    while !pair.outside.request_primary_selection(UTF8).unwrap() {
        pair.outside.pump(Some(Duration::from_millis(5))).unwrap();
        pair.inside.pump(Some(Duration::from_millis(1))).unwrap();
        assert!(Instant::now() < deadline, "no primary selection outside");
    }
    assert_eq!(
        read(&mut pair.outside, &mut pair.inside, UTF8),
        "selected inside"
    );

    pair.outside
        .set_primary_selection_types(&[(PLAIN, b"selected outside")])
        .unwrap();
    pair.wait_parent("the new primary selection", |s| {
        types(s, "primary_selection") == json!([PLAIN])
    });
    pair.focus_nested();
    pair.wait_nested("the primary selection inside", |s| {
        s["selection_owners"]["primary"] == "provided"
    });
    assert!(
        pair.inside
            .wait_for(
                TIMEOUT,
                |e| matches!(e, ClientEvent::PrimarySelection { mime_types }
                if *mime_types == [PLAIN.to_string()])
            )
            .unwrap()
            .is_some()
    );
    assert!(pair.inside.request_primary_selection(PLAIN).unwrap());
    assert_eq!(
        read(&mut pair.inside, &mut pair.outside, PLAIN),
        "selected outside"
    );
    pair.finish();
}

/// `--nested-clipboard off`: nothing crosses either way.
fn with_the_clipboard_off_nothing_crosses(device: Device) {
    let mut pair = Pair::start("sel-off", device, &["--nested-clipboard", "off"]);
    let state = pair.nested.state();
    assert_eq!(state["backend_status"]["selection"]["crossing"], "off");
    pair.focus_nested();
    pair.inside.set_selection(&[PLAIN], "stays inside").unwrap();
    pair.wait_nested("the copy inside", |s| {
        types(s, "selection") == json!([PLAIN])
    });
    pair.focus_outside();
    pair.outside
        .set_selection(&[UTF8], "stays outside")
        .unwrap();
    pair.wait_parent("the copy outside", |s| {
        types(s, "selection") == json!([UTF8])
    });
    pair.focus_nested();
    // A round trip through both, then neither has the other's.
    pair.wait_parent("the focus on the nested window", |s| {
        s["focus"]["window"].as_u64().is_some()
    });
    let state = pair.nested.state();
    assert_eq!(types(&state, "selection"), json!([PLAIN]));
    assert_eq!(state["selection_owners"]["clipboard"], "client");
    assert_eq!(types(&pair.parent.state(), "selection"), json!([UTF8]));
    pair.finish();
}

/// `--nested-clipboard to-parent`: copies inside go out, the parent's do
/// not come in.
fn only_to_the_parent(device: Device) {
    let mut pair = Pair::start("sel-to", device, &["--nested-clipboard", "to-parent"]);
    pair.focus_outside();
    pair.outside
        .set_selection(&[UTF8], "stays outside")
        .unwrap();
    pair.wait_parent("the copy outside", |s| {
        types(s, "selection") == json!([UTF8])
    });
    pair.focus_nested();
    pair.wait_nested("the focus", |s| {
        s["backend_status"]["selection"]["clipboard"]["focused"] == true
    });
    assert_eq!(pair.nested.state()["selection_owners"]["clipboard"], "none");
    pair.inside.set_selection(&[PLAIN], "goes out").unwrap();
    pair.wait_parent("the copy inside in the parent", |s| {
        types(s, "selection") == json!([PLAIN])
    });
    pair.finish();
}

/// `--nested-clipboard from-parent`: the parent's copies come in, copies
/// inside stay inside, also when the parent sends its selection again with
/// the focus.
fn only_from_the_parent(device: Device) {
    let mut pair = Pair::start("sel-from", device, &["--nested-clipboard", "from-parent"]);
    pair.focus_outside();
    pair.outside.set_selection(&[UTF8], "from outside").unwrap();
    pair.wait_parent("the copy outside", |s| {
        types(s, "selection") == json!([UTF8])
    });
    pair.focus_nested();
    pair.wait_nested("the parent's selection inside", |s| {
        s["selection_owners"]["clipboard"] == "provided"
    });
    pair.inside.set_selection(&[PLAIN], "stays inside").unwrap();
    pair.wait_nested("the copy inside", |s| {
        s["selection_owners"]["clipboard"] == "client"
    });
    // The parent's selection, sent again with the next focus, does not
    // replace it.
    pair.focus_outside();
    pair.focus_nested();
    pair.wait_nested("the focus back", |s| {
        s["backend_status"]["selection"]["clipboard"]["focused"] == true
    });
    let state = pair.nested.state();
    assert_eq!(
        state["selection_owners"]["clipboard"], "client",
        "{state:#}"
    );
    assert_eq!(types(&state, "selection"), json!([PLAIN]));
    assert_eq!(types(&pair.parent.state(), "selection"), json!([UTF8]));
    pair.finish();
}

device_tests!(
    a_copy_inside_reaches_the_parent,
    a_copy_in_the_parent_reaches_the_inside,
    large_transfers_do_not_hold_up_the_compositors,
    a_copy_without_the_parents_focus_waits_for_it,
    an_owner_that_goes_takes_the_selection_along,
    the_primary_selection_crosses_too,
    with_the_clipboard_off_nothing_crosses,
    only_to_the_parent,
    only_from_the_parent,
);
