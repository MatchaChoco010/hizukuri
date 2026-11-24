//! Protocol tests of relative pointer motion, pointer constraints, the
//! primary selection and data control.
//!
//! Like `protocol.rs`: the compositor core and the test clients run in one
//! thread, connected by socket pairs, and nothing is drawn. Windows are
//! managed by the fallback, so the core's default rules for pointer
//! constraints apply (a constraint is in effect while its window has the
//! keyboard focus). What a script decides instead is checked in
//! `script.rs`.

use std::os::unix::net::UnixStream;

use proto_backend::headless::{HeadlessBackend, HeadlessConfig, HeadlessOutputConfig};
use proto_compositor::compositor::{Compositor, CompositorOptions};
use proto_compositor::control::Request;
use proto_compositor::scene::NullScene;
use proto_test_client::{
    ClientEvent, ClientOptions, ConstraintRequest, DataControlProtocol, SelectionKind, TestClient,
};
use serde_json::{Value, json};

const KEY_A: u32 = 30;
const TEXT: &str = "text/plain;charset=utf-8";

struct Fixture {
    compositor: Compositor<HeadlessBackend>,
    clients: Vec<Option<TestClient>>,
    /// The protocol error a client was disconnected with, if any.
    errors: Vec<Option<String>>,
}

impl Fixture {
    fn new() -> Self {
        let config = HeadlessConfig {
            outputs: vec![HeadlessOutputConfig::default()],
            ..Default::default()
        };
        let backend = HeadlessBackend::new(&config, None).unwrap();
        let compositor = Compositor::new(
            CompositorOptions::default(),
            backend,
            Box::new(NullScene::new()),
        )
        .unwrap();
        Self {
            compositor,
            clients: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn connect(&mut self, connect: impl FnOnce(UnixStream) -> TestClient) -> usize {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.compositor.wl().insert_client(server_end).unwrap();
        self.clients.push(Some(connect(client_end)));
        self.errors.push(None);
        self.clients.len() - 1
    }

    /// Adds a client with a window, waits until it is mapped, and puts the
    /// window at `position`. Returns the client index and the window id.
    fn add_window(&mut self, title: &str, position: (i32, i32)) -> (usize, u64) {
        let options = ClientOptions {
            title: title.to_string(),
            width: 200,
            height: 100,
            ..Default::default()
        };
        let index = self.connect(|stream| TestClient::connect(stream, options).unwrap());
        self.settle("window mapped", |f| {
            f.client(index).is_drawn() && window_id(&f.state(), title).is_some()
        });
        let window = window_id(&self.state(), title).unwrap();
        self.control(Request::MoveWindow {
            window,
            x: position.0,
            y: position.1,
        });
        self.steps(4);
        (index, window)
    }

    /// Adds a client without a window: a clipboard manager.
    fn add_windowless(&mut self) -> usize {
        self.connect(|stream| TestClient::connect_windowless(stream).unwrap())
    }

    fn client(&mut self, index: usize) -> &mut TestClient {
        self.clients[index]
            .as_mut()
            .expect("the client is connected")
    }

    /// Disconnects a client.
    fn drop_client(&mut self, index: usize) {
        self.clients[index] = None;
    }

    /// One step: the compositor handles requests and a frame clock tick,
    /// then every client handles what it received. A client that was
    /// disconnected for a protocol error keeps the error and is not pumped
    /// again.
    fn step(&mut self) {
        self.compositor.dispatch_clients().unwrap();
        self.compositor.backend().tick();
        self.compositor.process().unwrap();
        for (client, error) in self.clients.iter_mut().zip(&mut self.errors) {
            if let Some(client) = client
                && error.is_none()
                && let Err(e) = client.pump(None)
            {
                *error = Some(e.to_string());
            }
        }
    }

    fn steps(&mut self, count: usize) {
        for _ in 0..count {
            self.step();
        }
    }

    fn settle(&mut self, what: &str, mut done: impl FnMut(&mut Fixture) -> bool) {
        for _ in 0..200 {
            self.step();
            if done(self) {
                self.steps(4);
                return;
            }
        }
        panic!("did not settle: {what}\nstate: {:#}", self.state());
    }

    fn control(&mut self, request: Request) -> Value {
        let response = self.compositor.control(&request);
        assert_eq!(
            response["ok"], true,
            "request {request:?} failed: {response}"
        );
        response
    }

    fn state(&mut self) -> Value {
        self.compositor.control(&Request::State)
    }

    fn pointer(&mut self) -> (f64, f64) {
        let state = self.state();
        let position = &state["pointer"]["position"];
        (position[0].as_f64().unwrap(), position[1].as_f64().unwrap())
    }

    fn move_to(&mut self, x: f64, y: f64) {
        self.control(Request::PointerMotion { x, y });
        self.steps(2);
    }

    fn move_by(&mut self, (dx, dy): (f64, f64), unaccelerated: Option<(f64, f64)>) {
        self.control(Request::PointerMotionRelative {
            dx,
            dy,
            dx_unaccel: unaccelerated.map(|u| u.0),
            dy_unaccel: unaccelerated.map(|u| u.1),
        });
        self.steps(2);
    }

    fn focus(&mut self, window: Option<u64>) {
        self.control(Request::FocusWindow { window });
        self.steps(2);
    }

    /// A key press and release, so that the focused client has a serial to
    /// claim a selection with.
    fn tap_key(&mut self) {
        for pressed in [true, false] {
            self.control(Request::Key {
                keycode: KEY_A,
                pressed,
            });
        }
        self.steps(2);
    }

    fn events(&mut self, index: usize) -> Vec<ClientEvent> {
        self.client(index).events().to_vec()
    }

    fn constraints(&mut self) -> Value {
        self.state()["pointer_constraints"].clone()
    }
}

fn window_id(state: &Value, title: &str) -> Option<u64> {
    state["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"] == title && w["mapped"] == true)?["id"]
        .as_u64()
}

fn relative_motions(events: &[ClientEvent]) -> Vec<(u64, f64, f64, f64, f64)> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::RelativeMotion {
                time_us,
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            } => Some((*time_us, *dx, *dy, *dx_unaccel, *dy_unaccel)),
            _ => None,
        })
        .collect()
}

fn motions(events: &[ClientEvent]) -> Vec<(f64, f64)> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::PointerMotion { x, y } => Some((*x, *y)),
            _ => None,
        })
        .collect()
}

fn count(events: &[ClientEvent], wanted: &ClientEvent) -> usize {
    events.iter().filter(|e| *e == wanted).count()
}

// ---------------------------------------------------------------------------
// Relative motion
// ---------------------------------------------------------------------------

#[test]
fn relative_motion_reaches_the_client_under_the_pointer() {
    let mut f = Fixture::new();
    let (game, window) = f.add_window("game", (100, 100));
    f.client(game).request_relative_pointer().unwrap();
    f.move_to(150.0, 150.0);
    f.client(game).clear_events();

    // The accelerated and the unaccelerated motion are told apart.
    f.move_by((3.0, -2.0), Some((1.5, -1.0)));
    f.move_by((4.0, 0.5), None);
    let events = f.events(game);
    let relative = relative_motions(&events);
    assert_eq!(relative.len(), 2, "{events:?}");
    assert_eq!(
        (relative[0].1, relative[0].2, relative[0].3, relative[0].4),
        (3.0, -2.0, 1.5, -1.0)
    );
    assert_eq!(
        (relative[1].1, relative[1].2, relative[1].3, relative[1].4),
        (4.0, 0.5, 4.0, 0.5)
    );
    // Microseconds, on the clock of the other input events, in order.
    assert!(
        relative[0].0 > 0 && relative[0].0 <= relative[1].0,
        "{relative:?}"
    );
    // The pointer moved as well, and the client was told.
    assert_eq!(motions(&events), [(53.0, 48.0), (57.0, 48.5)]);

    // At the edge of the output the pointer stops; the relative motion
    // does not (the default output is 1280 wide).
    f.control(Request::MoveWindow {
        window,
        x: 1100,
        y: 100,
    });
    f.move_to(1270.0, 150.0);
    f.client(game).clear_events();
    f.move_by((100.0, 0.0), Some((60.0, 0.0)));
    assert_eq!(f.pointer(), (1279.0, 150.0));
    let relative = relative_motions(&f.events(game));
    assert_eq!(
        relative.iter().map(|r| (r.1, r.3)).collect::<Vec<_>>(),
        [(100.0, 60.0)]
    );

    // Off the window, the client hears nothing.
    f.move_to(10.0, 10.0);
    f.client(game).clear_events();
    f.move_by((5.0, 5.0), None);
    assert!(relative_motions(&f.events(game)).is_empty());
}

// ---------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------

#[test]
fn a_lock_holds_the_pointer_while_its_window_has_the_focus() {
    let mut f = Fixture::new();
    let (game, window) = f.add_window("game", (100, 100));
    f.focus(Some(window));
    f.client(game).request_relative_pointer().unwrap();
    f.move_to(20.0, 20.0);
    f.client(game)
        .constrain_pointer(ConstraintRequest::Lock, None, true)
        .unwrap();
    f.steps(4);
    // Asked for, but the pointer is not on the window.
    assert_eq!(
        f.constraints(),
        json!([{ "surface": f.state()["windows"][0]["surface"], "window": window,
                 "kind": "lock", "active": false, "cursor_hint": null }])
    );
    assert_eq!(count(&f.events(game), &ClientEvent::PointerLocked), 0);

    // On the window: the lock starts.
    f.move_to(150.0, 140.0);
    assert_eq!(count(&f.events(game), &ClientEvent::PointerLocked), 1);
    assert_eq!(f.constraints()[0]["active"], true);

    // Motion turns into relative motion only. The pointer stays.
    f.client(game).clear_events();
    f.move_by((30.0, 10.0), Some((20.0, 5.0)));
    f.move_to(400.0, 300.0);
    let events = f.events(game);
    assert!(motions(&events).is_empty(), "{events:?}");
    let relative = relative_motions(&events);
    assert_eq!(
        relative
            .iter()
            .map(|r| (r.1, r.2, r.3, r.4))
            .collect::<Vec<_>>(),
        // An absolute motion is reported as the difference, both ways.
        [(30.0, 10.0, 20.0, 5.0), (250.0, 160.0, 250.0, 160.0)]
    );
    assert_eq!(f.pointer(), (150.0, 140.0));
    // Buttons still arrive.
    f.control(Request::PointerButton {
        button: 0x110,
        pressed: true,
    });
    f.control(Request::PointerButton {
        button: 0x110,
        pressed: false,
    });
    f.steps(2);
    assert!(
        f.events(game)
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: true, .. }))
    );

    // The focus moves away: the lock ends. The pointer is free.
    f.focus(None);
    assert_eq!(count(&f.events(game), &ClientEvent::PointerUnlocked), 1);
    assert_eq!(f.constraints()[0]["active"], false);
    f.move_by((5.0, 0.0), None);
    assert_eq!(f.pointer(), (155.0, 140.0));

    // A persistent lock comes back with the focus.
    f.focus(Some(window));
    assert_eq!(count(&f.events(game), &ClientEvent::PointerLocked), 1);
    // Withdrawn by the client: nothing is left.
    f.client(game).withdraw_pointer_constraint();
    f.steps(4);
    assert_eq!(f.constraints(), json!([]));
    f.move_by((5.0, 0.0), None);
    assert_eq!(f.pointer(), (160.0, 140.0));
}

#[test]
fn a_oneshot_lock_ends_for_good() {
    let mut f = Fixture::new();
    let (game, window) = f.add_window("game", (100, 100));
    f.focus(Some(window));
    f.move_to(150.0, 140.0);
    f.client(game)
        .constrain_pointer(ConstraintRequest::Lock, None, false)
        .unwrap();
    f.steps(4);
    assert_eq!(count(&f.events(game), &ClientEvent::PointerLocked), 1);
    f.focus(None);
    f.focus(Some(window));
    f.move_by((1.0, 1.0), None);
    let events = f.events(game);
    assert_eq!(count(&events, &ClientEvent::PointerLocked), 1, "{events:?}");
    assert_eq!(count(&events, &ClientEvent::PointerUnlocked), 1);
    assert_eq!(f.constraints(), json!([]));
    // A new one may be asked for in its place.
    f.client(game)
        .constrain_pointer(ConstraintRequest::Lock, None, false)
        .unwrap();
    f.steps(4);
    assert_eq!(count(&f.events(game), &ClientEvent::PointerLocked), 2);
    assert!(f.errors[game].is_none());
}

#[test]
fn a_lock_only_starts_inside_its_region() {
    let mut f = Fixture::new();
    let (game, window) = f.add_window("game", (100, 100));
    f.focus(Some(window));
    f.move_to(110.0, 110.0);
    f.client(game)
        .constrain_pointer(ConstraintRequest::Lock, Some((50, 20, 100, 60)), true)
        .unwrap();
    f.steps(4);
    // On the window, but outside the region.
    assert_eq!(count(&f.events(game), &ClientEvent::PointerLocked), 0);
    f.move_to(160.0, 130.0);
    assert_eq!(count(&f.events(game), &ClientEvent::PointerLocked), 1);
}

#[test]
fn a_second_constraint_on_a_surface_is_a_protocol_error() {
    let mut f = Fixture::new();
    let (game, _) = f.add_window("game", (100, 100));
    f.client(game)
        .constrain_pointer(ConstraintRequest::Lock, None, true)
        .unwrap();
    f.client(game)
        .constrain_pointer(ConstraintRequest::Confine, None, true)
        .unwrap();
    f.steps(6);
    let error = f.errors[game].clone().expect("the client was disconnected");
    assert!(error.contains("zwp_pointer_constraints_v1"), "{error}");
}

#[test]
fn ending_a_lock_puts_the_pointer_at_the_cursor_hint() {
    let mut f = Fixture::new();
    let (game, window) = f.add_window("game", (100, 100));
    f.focus(Some(window));
    f.client(game).request_relative_pointer().unwrap();
    f.move_to(150.0, 140.0);
    f.client(game)
        .constrain_pointer(ConstraintRequest::Lock, None, true)
        .unwrap();
    f.steps(4);
    f.client(game).set_cursor_position_hint(20.0, 15.5).unwrap();
    f.steps(4);
    assert_eq!(f.constraints()[0]["cursor_hint"], json!([20.0, 15.5]));

    // Ended by the compositor (the focus went away).
    f.client(game).clear_events();
    f.focus(None);
    assert_eq!(f.pointer(), (120.0, 115.5));
    let events = f.events(game);
    assert_eq!(motions(&events), [(20.0, 15.5)], "{events:?}");
    // No relative motion for the jump.
    assert!(relative_motions(&events).is_empty());

    // Withdrawn by the client while in effect.
    f.focus(Some(window));
    f.move_by((10.0, 10.0), None);
    f.client(game)
        .set_cursor_position_hint(150.0, 80.0)
        .unwrap();
    f.steps(2);
    f.client(game).withdraw_pointer_constraint();
    f.steps(4);
    assert_eq!(f.pointer(), (250.0, 180.0));
}

#[test]
fn a_lock_ends_when_its_window_goes_away() {
    let mut f = Fixture::new();
    let (game, window) = f.add_window("game", (100, 100));
    f.focus(Some(window));
    f.move_to(150.0, 140.0);
    f.client(game)
        .constrain_pointer(ConstraintRequest::Lock, None, true)
        .unwrap();
    f.steps(4);
    assert_eq!(f.constraints()[0]["active"], true);
    f.drop_client(game);
    f.steps(4);
    assert_eq!(f.constraints(), json!([]));
    f.move_by((5.0, 5.0), None);
    assert_eq!(f.pointer(), (155.0, 145.0));
}

// ---------------------------------------------------------------------------
// Confinement
// ---------------------------------------------------------------------------

#[test]
fn a_confined_pointer_stays_inside_its_region() {
    let mut f = Fixture::new();
    let (game, window) = f.add_window("game", (100, 100));
    f.focus(Some(window));
    f.client(game).request_relative_pointer().unwrap();
    // The region: x 10..60, y 10..50 of the window.
    f.client(game)
        .constrain_pointer(ConstraintRequest::Confine, Some((10, 10, 50, 40)), true)
        .unwrap();
    f.steps(4);
    f.move_to(130.0, 130.0);
    assert_eq!(count(&f.events(game), &ClientEvent::PointerConfined), 1);

    // Toward the right, out of the region: it does not move. The relative
    // motion is the whole of it.
    f.client(game).clear_events();
    f.move_by((100.0, 0.0), None);
    assert_eq!(f.pointer(), (130.0, 130.0));
    // Along the diagonal it slides along the edge it meets.
    f.move_by((5.0, 100.0), None);
    assert_eq!(f.pointer(), (135.0, 130.0));
    f.move_by((20.0, 10.0), None);
    assert_eq!(f.pointer(), (155.0, 140.0));
    let events = f.events(game);
    assert_eq!(
        relative_motions(&events)
            .iter()
            .map(|r| (r.1, r.2))
            .collect::<Vec<_>>(),
        [(100.0, 0.0), (5.0, 100.0), (20.0, 10.0)]
    );
    // An absolute position outside stays out.
    f.move_to(500.0, 500.0);
    assert_eq!(f.pointer(), (155.0, 140.0));
    for (x, y) in motions(&f.events(game)) {
        assert!(
            (10.0..60.0).contains(&x) && (10.0..50.0).contains(&y),
            "{x}, {y}"
        );
    }

    // A new region that leaves the pointer outside ends the confinement.
    f.client(game)
        .set_constraint_region(Some((0, 60, 50, 30)))
        .unwrap();
    f.steps(4);
    f.move_by((1.0, 0.0), None);
    assert_eq!(count(&f.events(game), &ClientEvent::PointerUnconfined), 1);
    f.move_by((100.0, 0.0), None);
    assert_eq!(f.pointer(), (256.0, 140.0));
}

// ---------------------------------------------------------------------------
// Primary selection
// ---------------------------------------------------------------------------

#[test]
fn the_primary_selection_goes_from_one_client_to_another() {
    let mut f = Fixture::new();
    let (writer, writer_window) = f.add_window("writer", (0, 0));
    f.client(writer).enable_primary_selection();
    f.focus(Some(writer_window));
    f.tap_key();
    f.client(writer)
        .set_primary_selection(&[TEXT], "selected text")
        .unwrap();
    f.steps(4);
    let state = f.state();
    assert_eq!(state["primary_selection"], json!([TEXT]));
    // The clipboard is another selection.
    assert_eq!(state["selection"], Value::Null);

    let (reader, reader_window) = f.add_window("reader", (300, 0));
    f.client(reader).enable_primary_selection();
    f.steps(2);
    // The offer comes with the focus.
    f.focus(Some(writer_window));
    f.focus(Some(reader_window));
    assert!(f.events(reader).contains(&ClientEvent::PrimarySelection {
        mime_types: vec![TEXT.to_string()]
    }));
    assert!(f.client(reader).request_primary_selection(TEXT).unwrap());
    f.settle("primary selection read", |f| {
        f.events(reader)
            .iter()
            .any(|e| matches!(e, ClientEvent::Received { .. }))
    });
    assert!(f.events(reader).contains(&ClientEvent::Received {
        mime_type: TEXT.to_string(),
        text: "selected text".to_string()
    }));
    assert!(f.events(writer).contains(&ClientEvent::PrimarySourceSend {
        mime_type: TEXT.to_string()
    }));

    // Without the focus, a client cannot take the primary selection.
    f.client(writer)
        .set_primary_selection(&[TEXT], "not taken")
        .unwrap();
    f.steps(4);
    assert_eq!(
        count(&f.events(writer), &ClientEvent::PrimarySourceCancelled),
        0
    );
    assert!(f.client(reader).request_primary_selection(TEXT).unwrap());
    f.settle("primary selection read again", |f| {
        f.events(reader)
            .iter()
            .filter(|e| matches!(e, ClientEvent::Received { .. }))
            .count()
            == 2
    });
    assert_eq!(
        f.events(reader)
            .iter()
            .filter(|e| **e
                == ClientEvent::Received {
                    mime_type: TEXT.to_string(),
                    text: "selected text".to_string()
                })
            .count(),
        2
    );
}

// ---------------------------------------------------------------------------
// Data control
// ---------------------------------------------------------------------------

fn received(events: &[ClientEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Received { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn control_selections(events: &[ClientEvent], which: SelectionKind) -> Vec<Vec<String>> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::ControlSelection {
                selection,
                mime_types,
                ..
            } if *selection == which => Some(mime_types.clone()),
            _ => None,
        })
        .collect()
}

fn a_clipboard_manager_works_without_the_focus(protocol: DataControlProtocol) {
    let mut f = Fixture::new();
    let (app, window) = f.add_window("app", (0, 0));
    f.client(app).enable_primary_selection();
    f.focus(Some(window));
    f.tap_key();
    f.client(app)
        .set_selection(&[TEXT], "copied in the app")
        .unwrap();
    f.steps(4);

    // The manager has no window, so never the focus. It is told the
    // selection when it binds, and reads it.
    let manager = f.add_windowless();
    f.client(manager).enable_data_control(protocol);
    f.settle("the manager sees the selection", |f| {
        !control_selections(&f.events(manager), SelectionKind::Clipboard).is_empty()
    });
    assert!(f.client(manager).has_data_control());
    assert_eq!(
        control_selections(&f.events(manager), SelectionKind::Clipboard),
        [vec![TEXT.to_string()]]
    );
    assert_eq!(
        control_selections(&f.events(manager), SelectionKind::Primary),
        [Vec::<String>::new()]
    );
    assert!(
        f.client(manager)
            .control_request_selection(SelectionKind::Clipboard, TEXT)
            .unwrap()
    );
    f.settle("the manager reads", |f| {
        !received(&f.events(manager)).is_empty()
    });
    assert_eq!(received(&f.events(manager)), ["copied in the app"]);

    // It replaces the clipboard; the focused app gets the new offer and
    // reads it from the manager.
    f.client(manager)
        .control_set_selection(SelectionKind::Clipboard, &[TEXT], "from the manager")
        .unwrap();
    f.settle("the app's source is replaced", |f| {
        f.events(app).contains(&ClientEvent::SourceCancelled)
    });
    assert!(f.client(app).request_selection(TEXT).unwrap());
    f.settle("the app reads", |f| !received(&f.events(app)).is_empty());
    assert_eq!(received(&f.events(app)), ["from the manager"]);
    assert!(f.events(manager).contains(&ClientEvent::ControlSourceSend {
        mime_type: TEXT.to_string()
    }));

    // And the primary selection.
    f.client(manager)
        .control_set_selection(SelectionKind::Primary, &[TEXT], "primary from the manager")
        .unwrap();
    f.steps(4);
    assert_eq!(f.state()["primary_selection"], json!([TEXT]));
    assert!(f.events(app).contains(&ClientEvent::PrimarySelection {
        mime_types: vec![TEXT.to_string()]
    }));

    // The manager goes away: what it owned goes with it.
    f.drop_client(manager);
    f.settle("the selections are gone", |f| {
        let events = f.events(app);
        events.contains(&ClientEvent::Selection {
            mime_types: Vec::new(),
        }) && events.contains(&ClientEvent::PrimarySelection {
            mime_types: Vec::new(),
        })
    });
    let state = f.state();
    assert_eq!(state["selection"], Value::Null);
    assert_eq!(state["primary_selection"], Value::Null);
}

#[test]
fn a_clipboard_manager_works_without_the_focus_ext() {
    a_clipboard_manager_works_without_the_focus(DataControlProtocol::Ext);
}

#[test]
fn a_clipboard_manager_works_without_the_focus_wlr() {
    a_clipboard_manager_works_without_the_focus(DataControlProtocol::Wlr);
}

#[test]
fn a_clipboard_manager_hears_when_the_owner_goes_away() {
    let mut f = Fixture::new();
    let manager = f.add_windowless();
    f.client(manager)
        .enable_data_control(DataControlProtocol::Ext);
    f.steps(4);
    let (app, window) = f.add_window("app", (0, 0));
    f.focus(Some(window));
    f.tap_key();
    f.client(app).set_selection(&[TEXT], "short lived").unwrap();
    f.settle("the manager sees the selection", |f| {
        control_selections(&f.events(manager), SelectionKind::Clipboard)
            .contains(&vec![TEXT.to_string()])
    });
    f.drop_client(app);
    f.settle("the selection is gone", |f| {
        control_selections(&f.events(manager), SelectionKind::Clipboard)
            .last()
            .is_some_and(|types| types.is_empty())
    }); // The state follows, although Smithay says nothing when a source goes.
    assert_eq!(f.state()["selection"], Value::Null);
}
