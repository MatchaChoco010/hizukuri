//! Text input inside a nested compositor, through the parent's input
//! method.
//!
//! The parent is a headless `proto-compositor` the test starts itself, with
//! a test client as its input method. Inside the nested compositor, test
//! clients with text fields play the applications.

mod common;

use std::time::{Duration, Instant};

use common::{Device, Session, TIMEOUT};
use proto_compositor::control::Request;
use proto_test_client::{ClientEvent, ClientOptions, ImeEvent, InputMethodMode, TestClient};
use serde_json::{Value, json};

const NESTED_APP_ID: &str = "hizukuri.nested";
/// Where the nested window is put in the parent, and the field's window in
/// the nested compositor.
const NESTED_AT: (i32, i32) = (100, 50);
const FIELD_AT: (i32, i32) = (40, 30);
/// The cursor rectangle the field reports, relative to its surface.
const CURSOR: (i32, i32, i32, i32) = (10, 20, 2, 16);

fn nested_window(state: &Value) -> Option<u64> {
    state["windows"]
        .as_array()?
        .iter()
        .find(|w| {
            w["app_id"] == NESTED_APP_ID
                && w["mapped"] == true
                && w["surfaces"][0]["size"].is_array()
        })
        .and_then(|w| w["id"].as_u64())
}

/// Pumps the clients until `done` holds.
fn until(
    clients: &mut [&mut TestClient],
    what: &str,
    mut done: impl FnMut(&[&mut TestClient]) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        for client in clients.iter_mut() {
            client
                .pump(Some(Duration::from_millis(5)))
                .expect("pumping a client");
        }
        if done(clients) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
    }
}

fn ime_count(ime: &TestClient, wanted: &ImeEvent) -> usize {
    ime.ime_events().iter().filter(|e| *e == wanted).count()
}

/// The activations and deactivations the parent's input method saw, in
/// order, from the first activation. The parent deactivates its input
/// method on every change of its keyboard focus, active or not: repeats
/// are taken as one.
fn activity(ime: &TestClient) -> Vec<&'static str> {
    let mut seen: Vec<&'static str> = Vec::new();
    for event in ime.ime_events() {
        let name = match event {
            ImeEvent::Activate => "activate",
            ImeEvent::Deactivate if !seen.is_empty() => "deactivate",
            _ => continue,
        };
        if seen.last() != Some(&name) {
            seen.push(name);
        }
    }
    seen
}

struct Rig {
    parent: Session,
    nested: Session,
    ime: TestClient,
    nested_window: u64,
}

/// A parent with an input method, and a nested compositor in it that has
/// the parent's keyboard. `nested_args` go to the nested compositor.
fn start(name: &str, device: Device, nested_args: &[&str]) -> Rig {
    let mut parent = Session::start(
        &format!("{name}-parent"),
        device,
        &["--output-size", "800x600"],
    );
    let mut ime = parent.client(ClientOptions {
        window: false,
        input_method: Some(InputMethodMode::Manual),
        ..Default::default()
    });
    parent.wait(&mut [&mut ime], "the input method", |state| {
        state["input_method"]["bound"] == true
    });
    let mut arguments = vec!["--output-size", "400x300", "--cursor", "off"];
    arguments.extend_from_slice(nested_args);
    let mut nested = Session::start_nested(&format!("{name}-child"), device, &parent, &arguments);
    let state = parent.wait(&mut [&mut ime], "the nested window", |state| {
        nested_window(state).is_some()
    });
    let nested_window = nested_window(&state).unwrap();
    parent.send(Request::MoveWindow {
        window: nested_window,
        x: NESTED_AT.0,
        y: NESTED_AT.1,
    });
    parent.send(Request::FocusWindow {
        window: Some(nested_window),
    });
    // The parent enters the window's text input: the nested compositor
    // takes the parent's input method as its own.
    nested.wait(&mut [&mut ime], "the parent's text input", |state| {
        state["backend_status"]["text_input"]["entered"] == true
            && state["outside_input_method"] == true
    });
    Rig {
        parent,
        nested,
        ime,
        nested_window,
    }
}

impl Rig {
    /// A window with a text field in the nested compositor, at `FIELD_AT`,
    /// with the keyboard focus. With `enable_on_enter`, the field enables
    /// itself whenever it is entered.
    fn field(&mut self, title: &str, enable_on_enter: bool) -> (TestClient, u64) {
        let (mut client, id) = self.nested.mapped_client(ClientOptions {
            title: title.to_string(),
            width: 200,
            height: 100,
            color: [200, 200, 200, 255],
            text_input: true,
            text_input_enable_on_enter: enable_on_enter.then_some(CURSOR),
            ..Default::default()
        });
        self.nested.send(Request::MoveWindow {
            window: id,
            x: FIELD_AT.0,
            y: FIELD_AT.1,
        });
        self.nested.send(Request::FocusWindow { window: Some(id) });
        until(
            &mut [&mut client, &mut self.ime],
            "the field entered",
            |c| c[0].text_input_entered(),
        );
        (client, id)
    }

    /// Waits until the parent's input method is active, and shows its
    /// popup (which is what the parent tells the cursor rectangle to).
    fn wait_active(&mut self, field: &mut TestClient, activations: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while !(ime_count(&self.ime, &ImeEvent::Activate) >= activations && self.ime.ime_active()) {
            field.pump(Some(Duration::from_millis(5))).unwrap();
            self.ime.pump(Some(Duration::from_millis(5))).unwrap();
            if Instant::now() > deadline {
                let nested = self.nested.state();
                let parent = self.parent.state();
                panic!(
                    "the parent's input method is not active\nfield: {:?}\nime: {:?}\nnested: {:#}\nparent input method: {:#}",
                    field.events(),
                    self.ime.ime_events(),
                    json!({
                        "backend_status": nested["backend_status"]["text_input"],
                        "outside": nested["outside_input_method"],
                        "input_method": nested["input_method"],
                        "focus": nested["focus"],
                    }),
                    json!({ "input_method": parent["input_method"], "focus": parent["focus"] }),
                );
            }
        }
    }

    fn parent_cursor(&mut self, field: &mut TestClient, wanted: [i32; 4]) {
        let parent = &mut self.parent;
        parent.wait(
            &mut [field, &mut self.ime],
            "the cursor rectangle in the parent",
            |state| state["input_method"]["popup"]["cursor_rectangle"] == json!(wanted),
        );
    }

    fn quit(self) {
        self.nested.quit();
        self.parent.quit();
    }
}

fn done_matches(events: &[ClientEvent]) -> Option<bool> {
    events.iter().rev().find_map(|e| match e {
        ClientEvent::TextInputDone { serial, commits } => Some(serial == commits),
        _ => None,
    })
}

/// What the parent's input method composes reaches the field inside; what
/// the field reports reaches the parent's input method, the cursor in the
/// parent's coordinates.
fn text_typed_inside_goes_through_the_parents_input_method(device: Device) {
    let mut rig = start("nested-ime", device, &[]);
    let (mut field, _) = rig.field("field", false);
    field.text_input_enable(Some(CURSOR)).unwrap();
    rig.wait_active(&mut field, 1);
    until(&mut [&mut rig.ime], "the content type", |c| {
        c[0].ime_events()
            .iter()
            .any(|e| matches!(e, ImeEvent::ContentType { .. }))
    });

    // The cursor: the field's rectangle, moved by where its window is
    // inside (the nested window's surface is the nested output).
    rig.ime
        .ime_show_popup(120, 30, [40, 160, 220, 255])
        .unwrap();
    rig.parent_cursor(
        &mut field,
        [
            FIELD_AT.0 + CURSOR.0,
            FIELD_AT.1 + CURSOR.1,
            CURSOR.2,
            CURSOR.3,
        ],
    );

    // Composing, then finished text.
    field.clear_events();
    rig.ime.ime_preedit("ka", 2, 2).unwrap();
    until(&mut [&mut field, &mut rig.ime], "the preedit inside", |c| {
        c[0].events().contains(&ClientEvent::Preedit {
            text: "ka".to_string(),
            cursor_begin: 2,
            cursor_end: 2,
        }) && done_matches(c[0].events()).is_some()
    });
    assert_eq!(
        done_matches(field.events()),
        Some(true),
        "{:?}",
        field.events()
    );
    field.clear_events();
    rig.ime.ime_commit_string("か").unwrap();
    until(&mut [&mut field, &mut rig.ime], "the commit inside", |c| {
        c[0].events().contains(&ClientEvent::CommitString {
            text: "か".to_string(),
        }) && done_matches(c[0].events()).is_some()
    });
    // The empty preedit that came with it ends the composing.
    assert!(
        field.events().contains(&ClientEvent::Preedit {
            text: String::new(),
            cursor_begin: 0,
            cursor_end: 0,
        }),
        "{:?}",
        field.events()
    );

    // The text around the cursor goes out; a deletion comes back.
    field.text_input_set_surrounding("abcか", 6, 6).unwrap();
    until(
        &mut [&mut field, &mut rig.ime],
        "the surrounding text outside",
        |c| {
            c[1].ime_events().contains(&ImeEvent::SurroundingText {
                text: "abcか".to_string(),
                cursor: 6,
                anchor: 6,
            })
        },
    );
    field.clear_events();
    rig.ime.ime_delete_surrounding(3, 0).unwrap();
    until(
        &mut [&mut field, &mut rig.ime],
        "the deletion inside",
        |c| {
            c[0].events().contains(&ClientEvent::DeleteSurroundingText {
                before: 3,
                after: 0,
            })
        },
    );

    // The cursor moves.
    field.text_input_set_cursor((30, 4, 2, 16)).unwrap();
    rig.parent_cursor(&mut field, [FIELD_AT.0 + 30, FIELD_AT.1 + 4, 2, 16]);

    // A commit the input method based on an older state is passed on as
    // such: the field applies the text, not the state.
    field.clear_events();
    rig.ime.ime_commit_with_serial(9999).unwrap();
    until(
        &mut [&mut field, &mut rig.ime],
        "the stale done inside",
        |c| done_matches(c[0].events()).is_some(),
    );

    // Disabled inside, disabled outside.
    field.text_input_disable().unwrap();
    until(
        &mut [&mut field, &mut rig.ime],
        "deactivated outside",
        |c| !c[1].ime_active(),
    );
    assert_eq!(activity(&rig.ime), ["activate", "deactivate"]);
    rig.nested.assert_script_healthy();
    rig.quit();
}

/// Moving the focus inside from one field to another disables the first
/// on the parent before the second is enabled.
fn moving_the_focus_inside_disables_then_enables_on_the_parent(device: Device) {
    let mut rig = start("nested-ime-focus", device, &[]);
    let (mut first, first_id) = rig.field("first", true);
    rig.wait_active(&mut first, 1);
    let (mut second, _) = rig.field("second", true);
    until(
        &mut [&mut first, &mut second, &mut rig.ime],
        "the second field active outside",
        |c| ime_count(c[2], &ImeEvent::Activate) >= 2 && c[2].ime_active(),
    );
    assert!(!first.text_input_entered());
    assert_eq!(activity(&rig.ime), ["activate", "deactivate", "activate"]);

    // And back.
    rig.nested.send(Request::FocusWindow {
        window: Some(first_id),
    });
    until(
        &mut [&mut first, &mut second, &mut rig.ime],
        "the first field active outside again",
        |c| ime_count(c[2], &ImeEvent::Activate) >= 3 && c[2].ime_active(),
    );
    assert_eq!(
        activity(&rig.ime),
        [
            "activate",
            "deactivate",
            "activate",
            "deactivate",
            "activate"
        ]
    );
    assert!(first.text_input_entered() && !second.text_input_entered());
    rig.quit();
}

/// The nested window losing the parent's keyboard ends the text input
/// inside; getting it back starts it again.
fn the_parents_focus_leaving_and_coming_back(device: Device) {
    let mut rig = start("nested-ime-parent-focus", device, &[]);
    let (mut field, _) = rig.field("field", true);
    rig.wait_active(&mut field, 1);

    let (mut other, other_id) = rig.parent.mapped_client(ClientOptions {
        title: "other".to_string(),
        width: 100,
        height: 100,
        color: [0, 0, 200, 255],
        ..Default::default()
    });
    rig.parent.send(Request::FocusWindow {
        window: Some(other_id),
    });
    until(
        &mut [&mut field, &mut other, &mut rig.ime],
        "the field left",
        |c| !c[0].text_input_entered() && !c[2].ime_active(),
    );
    let state = rig.nested.state();
    assert_eq!(state["outside_input_method"], false, "{state:#}");
    assert_eq!(
        state["backend_status"]["text_input"]["entered"], false,
        "{state:#}"
    );

    rig.parent.send(Request::FocusWindow {
        window: Some(rig.nested_window),
    });
    until(
        &mut [&mut field, &mut other, &mut rig.ime],
        "the field entered and active again",
        |c| {
            c[0].text_input_entered()
                && ime_count(c[2], &ImeEvent::Activate) >= 2
                && c[2].ime_active()
        },
    );
    assert_eq!(activity(&rig.ime), ["activate", "deactivate", "activate"]);
    rig.quit();
}

/// An input method that connects inside takes the text input; when it
/// goes, the parent's has it again.
fn an_input_method_inside_takes_precedence(device: Device) {
    let mut rig = start("nested-ime-inside", device, &[]);
    let (mut field, _) = rig.field("field", true);
    rig.wait_active(&mut field, 1);

    let mut inside = rig.nested.client(ClientOptions {
        window: false,
        input_method: Some(InputMethodMode::Manual),
        ..Default::default()
    });
    until(
        &mut [&mut field, &mut inside, &mut rig.ime],
        "the input method inside active",
        |c| c[1].ime_active() && !c[2].ime_active(),
    );
    assert_eq!(rig.nested.state()["outside_input_method"], false);
    field.clear_events();
    inside.ime_commit_string("inside").unwrap();
    until(
        &mut [&mut field, &mut inside, &mut rig.ime],
        "text from inside",
        |c| {
            c[0].events().contains(&ClientEvent::CommitString {
                text: "inside".to_string(),
            })
        },
    );

    drop(inside);
    until(
        &mut [&mut field, &mut rig.ime],
        "the parent's input method active again",
        |c| ime_count(c[1], &ImeEvent::Activate) >= 2 && c[1].ime_active(),
    );
    assert_eq!(rig.nested.state()["outside_input_method"], true);
    rig.quit();
}

/// A project that draws windows elsewhere than at their position says
/// where a point of a surface is seen (`project`): the parent is told the
/// cursor there.
fn a_project_says_where_the_cursor_is_seen(device: Device) {
    let project = common::script_project(
        "nested-ime-project",
        device,
        &[(
            "main",
            r#"
hz.include("session")
-- Every surface seen at half its size, from (7, 9).
function project(surface, x, y)
    return 7 + x * 0.5, 9 + y * 0.5
end
"#,
        )],
    );
    let mut rig = start(
        "nested-ime-project",
        device,
        &["--script", project.to_str().unwrap()],
    );
    let (mut field, _) = rig.field("field", true);
    rig.wait_active(&mut field, 1);
    rig.ime
        .ime_show_popup(120, 30, [40, 160, 220, 255])
        .unwrap();
    // (10, 20, 2, 16) seen as (12, 19) to (13, 27).
    rig.parent_cursor(&mut field, [12, 19, 1, 8]);
    rig.nested.assert_script_healthy();
    rig.quit();
}

/// In the 3D room, the cursor is told where it is seen: on the monitor
/// that shows the field's window, through the camera.
fn in_the_room_the_cursor_is_where_the_monitor_shows_it(device: Device) {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("projects/room");
    common::build_shaders(&project);
    let mut rig = start(
        "nested-ime-room",
        device,
        &["--script", project.to_str().unwrap()],
    );
    rig.nested.script("room_set", json!(["parallax", 0]));
    let (mut field, _) = rig.field("field", true);
    rig.wait_active(&mut field, 1);
    rig.ime
        .ime_show_popup(120, 30, [40, 160, 220, 255])
        .unwrap();
    // The first window is shown on the main monitor (1), at its size: a
    // point of the field is that point of the monitor's pixels. Once the
    // camera has stopped, the parent has the box around the four corners.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        field.pump(Some(Duration::from_millis(5))).unwrap();
        rig.ime.pump(Some(Duration::from_millis(5))).unwrap();
        let (x, y, w, h) = CURSOR;
        let corners: Vec<(f64, f64)> = [(x, y), (x + w, y), (x, y + h), (x + w, y + h)]
            .iter()
            .filter_map(|&(cx, cy)| {
                let point = rig.nested.script("monitor_point", json!([1, cx, cy]));
                Some((point[0].as_f64()?, point[1].as_f64()?))
            })
            .collect();
        let wanted = (corners.len() == 4).then(|| {
            let left = corners
                .iter()
                .map(|c| c.0)
                .fold(f64::INFINITY, f64::min)
                .floor();
            let top = corners
                .iter()
                .map(|c| c.1)
                .fold(f64::INFINITY, f64::min)
                .floor();
            let right = corners
                .iter()
                .map(|c| c.0)
                .fold(f64::NEG_INFINITY, f64::max)
                .ceil();
            let bottom = corners
                .iter()
                .map(|c| c.1)
                .fold(f64::NEG_INFINITY, f64::max)
                .ceil();
            json!([
                left as i32,
                top as i32,
                ((right - left) as i32).max(1),
                ((bottom - top) as i32).max(1)
            ])
        });
        let got = rig.parent.state()["input_method"]["popup"]["cursor_rectangle"].clone();
        if let Some(wanted) = &wanted
            && *wanted == got
        {
            // Seen smaller than it is, far from where it would be flat.
            let rect: Vec<i64> = got
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect();
            assert_ne!(rect, vec![10, 20, 2, 16]);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the cursor in the room: wanted {}, the parent has {got}",
            wanted.unwrap_or(Value::Null),
        );
    }
    rig.nested.assert_script_healthy();
    rig.quit();
}

device_tests!(
    text_typed_inside_goes_through_the_parents_input_method,
    moving_the_focus_inside_disables_then_enables_on_the_parent,
    the_parents_focus_leaving_and_coming_back,
    an_input_method_inside_takes_precedence,
    a_project_says_where_the_cursor_is_seen,
    in_the_room_the_cursor_is_where_the_monitor_shows_it,
);
