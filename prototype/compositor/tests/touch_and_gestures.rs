//! Protocol tests of touch (`wl_touch`) and touchpad gestures
//! (`zwp_pointer_gestures_v1`).
//!
//! Like `protocol.rs`: the compositor core and the test clients run in one
//! thread, connected by socket pairs, and nothing is drawn. Windows are
//! managed by the fallback, so the core's defaults apply: a touch focuses
//! the window it touches, and every gesture goes to the client under the
//! pointer. What a script decides instead is checked in `script.rs`.

use std::os::unix::net::UnixStream;

use proto_backend::InputEvent;
use proto_backend::headless::{HeadlessBackend, HeadlessConfig, HeadlessOutputConfig};
use proto_compositor::compositor::{Compositor, CompositorOptions};
use proto_compositor::control::Request;
use proto_compositor::scene::NullScene;
use proto_test_client::{
    ClientEvent, ClientOptions, GestureKind, PopupOptions, SurfaceKind, TestClient,
};
use serde_json::Value;

const BTN_LEFT: u32 = 0x110;

struct Fixture {
    compositor: Compositor<HeadlessBackend>,
    clients: Vec<Option<TestClient>>,
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
        }
    }

    /// Adds a client with a 200x100 window at `position` that listens to
    /// touches and gestures. Returns the client index and the window id.
    fn add_window(&mut self, title: &str, position: (i32, i32)) -> (usize, u64) {
        let options = ClientOptions {
            title: title.to_string(),
            width: 200,
            height: 100,
            ..Default::default()
        };
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.compositor.wl().insert_client(server_end).unwrap();
        let mut client = TestClient::connect(client_end, options).unwrap();
        client.request_touch();
        self.clients.push(Some(client));
        let index = self.clients.len() - 1;
        self.settle("window mapped", |f| {
            f.client(index).is_drawn()
                && f.client(index).has_touch()
                && window_id(&f.state(), title).is_some()
        });
        self.client(index).request_pointer_gestures().unwrap();
        let window = window_id(&self.state(), title).unwrap();
        self.control(Request::MoveWindow {
            window,
            x: position.0,
            y: position.1,
        });
        self.steps(4);
        (index, window)
    }

    fn client(&mut self, index: usize) -> &mut TestClient {
        self.clients[index]
            .as_mut()
            .expect("the client is connected")
    }

    fn step(&mut self) {
        self.compositor.dispatch_clients().unwrap();
        self.compositor.backend().tick();
        self.compositor.process().unwrap();
        for client in self.clients.iter_mut().flatten() {
            client.pump(None).unwrap();
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

    /// Sends touch requests and closes them with a frame.
    fn touch(&mut self, requests: &[Request]) {
        for request in requests {
            self.control(request.clone());
        }
        self.control(Request::TouchFrame);
        self.steps(2);
    }

    fn clear(&mut self) {
        for client in self.clients.iter_mut().flatten() {
            client.clear_events();
        }
    }

    /// The touch and gesture events a client got, in order.
    fn received(&mut self, index: usize) -> Vec<ClientEvent> {
        self.client(index)
            .events()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ClientEvent::TouchDown { .. }
                        | ClientEvent::TouchMotion { .. }
                        | ClientEvent::TouchUp { .. }
                        | ClientEvent::TouchFrame
                        | ClientEvent::TouchCancel
                        | ClientEvent::GestureBegin { .. }
                        | ClientEvent::GestureUpdate { .. }
                        | ClientEvent::GestureEnd { .. }
                )
            })
            .cloned()
            .collect()
    }

    fn move_pointer(&mut self, x: f64, y: f64) {
        self.control(Request::PointerMotion { x, y });
        self.steps(2);
    }

    fn gesture(&mut self, requests: &[Request]) {
        for request in requests {
            self.control(request.clone());
        }
        self.steps(2);
    }
}

fn window_id(state: &Value, title: &str) -> Option<u64> {
    state["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"] == title && w["mapped"] == true)?["id"]
        .as_u64()
}

fn down(slot: u32, x: f64, y: f64) -> Request {
    Request::TouchDown { slot, x, y }
}

fn motion(slot: u32, x: f64, y: f64) -> Request {
    Request::TouchMotion { slot, x, y }
}

fn up(slot: u32) -> Request {
    Request::TouchUp { slot }
}

fn begin(kind: &str, fingers: u32) -> Request {
    Request::GestureBegin {
        kind: kind.to_string(),
        fingers,
    }
}

fn update(kind: &str, dx: f64, dy: f64, scale: f64, rotation: f64) -> Request {
    Request::GestureUpdate {
        kind: kind.to_string(),
        dx,
        dy,
        scale,
        rotation,
    }
}

fn end(kind: &str, cancelled: bool) -> Request {
    Request::GestureEnd {
        kind: kind.to_string(),
        cancelled,
    }
}

fn touch_down(id: i32, x: f64, y: f64) -> ClientEvent {
    ClientEvent::TouchDown {
        id,
        surface: SurfaceKind::Window,
        x,
        y,
    }
}

// ---------------------------------------------------------------------------
// Touch
// ---------------------------------------------------------------------------

/// The seat offers touch while a touchscreen is there (the headless backend
/// counts its control interface as one). Clients take a `wl_touch` then,
/// and let it go when touch is withdrawn.
#[test]
fn touch_is_offered_while_a_touchscreen_is_there() {
    let mut f = Fixture::new();
    let (a, _) = f.add_window("a", (100, 100));
    assert_eq!(f.client(a).seat_has_touch(), Some(true));
    assert!(f.client(a).has_touch());
    assert_eq!(f.state()["touch"]["available"], true);

    f.compositor.handle_input(InputEvent::TouchAvailable(false));
    f.compositor.process().unwrap();
    f.settle("touch withdrawn", |f| {
        f.client(a).seat_has_touch() == Some(false)
    });
    assert!(!f.client(a).has_touch());
    assert_eq!(f.state()["touch"]["available"], false);
    // Nothing to touch with: a touch goes nowhere.
    f.clear();
    f.touch(&[down(0, 150.0, 130.0)]);
    assert_eq!(f.received(a), []);
    assert_eq!(f.state()["touch"]["points"], serde_json::json!([]));
    f.touch(&[up(0)]);

    f.compositor.handle_input(InputEvent::TouchAvailable(true));
    f.compositor.process().unwrap();
    f.settle("touch offered again", |f| f.client(a).has_touch());
    f.clear();
    f.touch(&[down(0, 150.0, 130.0)]);
    assert_eq!(
        f.received(a),
        [touch_down(0, 50.0, 30.0), ClientEvent::TouchFrame]
    );
}

/// A touch goes to the surface it went down on, in that surface's
/// coordinates, until it comes up: also when the finger moves over another
/// window.
#[test]
fn a_touch_stays_with_the_surface_it_went_down_on() {
    let mut f = Fixture::new();
    let (a, _) = f.add_window("a", (100, 100));
    let (b, _) = f.add_window("b", (400, 100));
    f.clear();

    f.touch(&[down(0, 150.0, 130.0)]);
    assert_eq!(
        f.received(a),
        [touch_down(0, 50.0, 30.0), ClientEvent::TouchFrame]
    );
    assert_eq!(f.received(b), []);
    let state = f.state();
    let surface_a = state["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["title"] == "a")
        .unwrap()["surface"]
        .clone();
    assert_eq!(
        state["touch"]["points"],
        serde_json::json!([{ "slot": 0, "surface": surface_a }])
    );

    // Over b, and off every window: still a's, in a's coordinates.
    f.clear();
    f.touch(&[motion(0, 450.0, 120.0)]);
    f.touch(&[motion(0, 700.0, 600.0)]);
    assert_eq!(
        f.received(a),
        [
            ClientEvent::TouchMotion {
                id: 0,
                x: 350.0,
                y: 20.0
            },
            ClientEvent::TouchFrame,
            ClientEvent::TouchMotion {
                id: 0,
                x: 600.0,
                y: 500.0
            },
            ClientEvent::TouchFrame,
        ]
    );
    assert_eq!(f.received(b), []);

    f.clear();
    f.touch(&[up(0)]);
    assert_eq!(
        f.received(a),
        [ClientEvent::TouchUp { id: 0 }, ClientEvent::TouchFrame]
    );
    assert_eq!(f.state()["touch"]["points"], serde_json::json!([]));
    // Once up, a motion of that slot goes nowhere.
    f.clear();
    f.touch(&[motion(0, 160.0, 130.0)]);
    assert_eq!(f.received(a), []);
}

/// Two fingers on two windows: each window gets its own finger, and one
/// frame for the set of events it took part in.
#[test]
fn two_fingers_on_two_windows_go_to_their_own_window() {
    let mut f = Fixture::new();
    let (a, _) = f.add_window("a", (100, 100));
    let (b, _) = f.add_window("b", (400, 100));
    f.clear();

    f.touch(&[down(0, 150.0, 130.0), down(1, 410.0, 150.0)]);
    assert_eq!(
        f.received(a),
        [touch_down(0, 50.0, 30.0), ClientEvent::TouchFrame]
    );
    assert_eq!(
        f.received(b),
        [touch_down(1, 10.0, 50.0), ClientEvent::TouchFrame]
    );

    // Both move in one frame; then only the second.
    f.clear();
    f.touch(&[motion(0, 155.0, 130.0), motion(1, 420.0, 150.0)]);
    f.touch(&[motion(1, 430.0, 150.0)]);
    assert_eq!(
        f.received(a),
        [
            ClientEvent::TouchMotion {
                id: 0,
                x: 55.0,
                y: 30.0
            },
            ClientEvent::TouchFrame,
        ]
    );
    assert_eq!(
        f.received(b),
        [
            ClientEvent::TouchMotion {
                id: 1,
                x: 20.0,
                y: 50.0
            },
            ClientEvent::TouchFrame,
            ClientEvent::TouchMotion {
                id: 1,
                x: 30.0,
                y: 50.0
            },
            ClientEvent::TouchFrame,
        ]
    );

    // A third finger on a, while the others are down: a's again, with
    // both of a's fingers told apart.
    f.clear();
    f.touch(&[down(2, 290.0, 190.0)]);
    f.touch(&[up(0), up(1)]);
    f.touch(&[up(2)]);
    assert_eq!(
        f.received(a),
        [
            touch_down(2, 190.0, 90.0),
            ClientEvent::TouchFrame,
            ClientEvent::TouchUp { id: 0 },
            ClientEvent::TouchFrame,
            ClientEvent::TouchUp { id: 2 },
            ClientEvent::TouchFrame,
        ]
    );
    assert_eq!(
        f.received(b),
        [ClientEvent::TouchUp { id: 1 }, ClientEvent::TouchFrame]
    );
}

/// Touching a window gives it the focus, as clicking it does, when nobody
/// decides otherwise. Touching nothing changes nothing.
#[test]
fn a_touch_focuses_the_window_it_touches() {
    let mut f = Fixture::new();
    let (a, window_a) = f.add_window("a", (100, 100));
    let (_b, window_b) = f.add_window("b", (400, 100));
    assert_eq!(f.state()["focus"]["window"], window_b);

    f.touch(&[down(0, 150.0, 130.0)]);
    assert_eq!(f.state()["focus"]["window"], window_a);
    f.settle("a has the keyboard", |f| {
        f.client(a).keyboard_focus() == Some(SurfaceKind::Window)
    });
    f.touch(&[up(0)]);

    f.touch(&[down(0, 1000.0, 600.0)]);
    assert_eq!(f.state()["focus"]["window"], window_a);
    f.touch(&[up(0)]);

    f.touch(&[down(3, 450.0, 150.0)]);
    assert_eq!(f.state()["focus"]["window"], window_b);
}

/// A cancel voids every touch, on every window, also those whose last event
/// was in an earlier frame. Their ups are not sent afterwards.
#[test]
fn a_cancel_voids_every_touch() {
    let mut f = Fixture::new();
    let (a, _) = f.add_window("a", (100, 100));
    let (b, _) = f.add_window("b", (400, 100));
    f.touch(&[down(0, 150.0, 130.0)]);
    f.touch(&[down(1, 450.0, 130.0)]);
    f.clear();

    f.control(Request::TouchCancel);
    f.steps(2);
    for client in [a, b] {
        let received = f.received(client);
        assert_eq!(
            received.last(),
            Some(&ClientEvent::TouchCancel),
            "{received:?}"
        );
        assert_eq!(
            received
                .iter()
                .filter(|e| **e == ClientEvent::TouchCancel)
                .count(),
            1
        );
        // Nothing but the cancel moves: a motion to where the finger is
        // may come before it.
        assert!(
            received.iter().all(|e| matches!(
                e,
                ClientEvent::TouchCancel | ClientEvent::TouchMotion { .. }
            )),
            "{received:?}"
        );
    }
    assert_eq!(f.state()["touch"]["points"], serde_json::json!([]));

    f.clear();
    f.touch(&[up(0), up(1)]);
    assert_eq!(f.received(a), []);
    assert_eq!(f.received(b), []);
    // Touching works as before.
    f.touch(&[down(0, 150.0, 130.0)]);
    assert_eq!(
        f.received(a),
        [touch_down(0, 50.0, 30.0), ClientEvent::TouchFrame]
    );
}

/// The window a finger is on goes away: its motions go nowhere, its up
/// still reaches the client, and the next touch works.
#[test]
fn a_window_that_goes_away_under_a_finger() {
    let mut f = Fixture::new();
    let (a, window_a) = f.add_window("a", (100, 100));
    let (b, _) = f.add_window("b", (400, 100));
    f.touch(&[down(0, 150.0, 130.0)]);
    f.control(Request::CloseWindow { window: window_a });
    f.settle("a is gone", |f| window_id(&f.state(), "a").is_none());
    f.clear();

    f.touch(&[motion(0, 160.0, 130.0)]);
    assert!(
        !f.received(a)
            .iter()
            .any(|e| matches!(e, ClientEvent::TouchMotion { .. }))
    );
    // The client is still told the finger came up, so that it can forget
    // the touch point.
    f.touch(&[up(0)]);
    assert_eq!(
        f.received(a),
        [ClientEvent::TouchUp { id: 0 }, ClientEvent::TouchFrame]
    );
    assert_eq!(f.state()["touch"]["points"], serde_json::json!([]));
    // Where the window was, there is nothing now; b takes touches as before.
    f.touch(&[down(0, 450.0, 130.0)]);
    assert_eq!(
        f.received(b),
        [touch_down(0, 50.0, 30.0), ClientEvent::TouchFrame]
    );
    assert!(
        !f.received(a)
            .iter()
            .any(|e| matches!(e, ClientEvent::TouchDown { .. }))
    );
}

/// While a menu holds a popup grab, touching another client dismisses the
/// menu and still reaches that client; touching the menu's own client does
/// not dismiss it.
#[test]
fn a_touch_outside_a_grabbing_popup_dismisses_it() {
    let mut f = Fixture::new();
    let (other, _) = f.add_window("other", (500, 0));
    let (menu, _) = f.add_window("menu", (0, 0));
    // The grab needs the serial of an input event: the click that opens
    // the menu.
    for pressed in [true, false] {
        f.control(Request::PointerMotion { x: 10.0, y: 10.0 });
        f.control(Request::PointerButton {
            button: BTN_LEFT,
            pressed,
        });
    }
    f.settle("menu focused", |f| {
        f.client(menu).keyboard_focus() == Some(SurfaceKind::Window)
    });
    f.client(menu)
        .open_popup(PopupOptions {
            x: 50,
            y: 20,
            width: 80,
            height: 50,
            color: [0, 0, 255, 255],
            grab: true,
            slide: false,
        })
        .unwrap();
    f.settle("the popup has the keyboard", |f| {
        f.client(menu).keyboard_focus() == Some(SurfaceKind::Popup)
    });
    f.clear();

    // On the menu's own window, and on the popup: delivered, not dismissed.
    f.touch(&[down(0, 150.0, 80.0)]);
    f.touch(&[up(0)]);
    f.touch(&[down(0, 60.0, 30.0)]);
    f.touch(&[up(0)]);
    assert_eq!(
        f.received(menu)[..2],
        [touch_down(0, 150.0, 80.0), ClientEvent::TouchFrame]
    );
    assert!(f.received(menu).contains(&ClientEvent::TouchDown {
        id: 0,
        surface: SurfaceKind::Popup,
        x: 10.0,
        y: 10.0
    }));
    assert!(!f.client(menu).events().contains(&ClientEvent::PopupDone));
    assert!(f.client(menu).has_popup());

    // On the other client: dismissed, and the touch is the other's.
    f.touch(&[down(0, 550.0, 50.0)]);
    f.settle("dismissed", |f| {
        f.client(menu).events().contains(&ClientEvent::PopupDone)
    });
    assert_eq!(
        f.received(other)[..2],
        [touch_down(0, 50.0, 50.0), ClientEvent::TouchFrame]
    );
    // The keyboard follows the touch to the other window.
    f.settle("the other client has the keyboard", |f| {
        f.client(other).keyboard_focus() == Some(SurfaceKind::Window)
    });
}

// ---------------------------------------------------------------------------
// Gestures
// ---------------------------------------------------------------------------

/// Swipe, pinch and hold reach the client under the pointer, from begin to
/// end, with the number of fingers and the motion. The other client hears
/// nothing.
#[test]
fn gestures_reach_the_client_under_the_pointer() {
    let mut f = Fixture::new();
    let (a, _) = f.add_window("a", (100, 100));
    let (b, _) = f.add_window("b", (400, 100));
    f.move_pointer(150.0, 150.0);
    f.clear();

    f.gesture(&[
        begin("swipe", 3),
        update("swipe", 4.0, -2.0, 1.0, 0.0),
        update("swipe", 6.0, 0.5, 1.0, 0.0),
        end("swipe", false),
    ]);
    f.gesture(&[
        begin("pinch", 2),
        update("pinch", 1.0, 1.0, 1.25, 5.0),
        end("pinch", true),
    ]);
    f.gesture(&[begin("hold", 4), end("hold", false)]);
    let begin_event = |kind, fingers| ClientEvent::GestureBegin {
        kind,
        fingers,
        surface: SurfaceKind::Window,
    };
    assert_eq!(
        f.received(a),
        [
            begin_event(GestureKind::Swipe, 3),
            ClientEvent::GestureUpdate {
                kind: GestureKind::Swipe,
                dx: 4.0,
                dy: -2.0,
                scale: 1.0,
                rotation: 0.0
            },
            ClientEvent::GestureUpdate {
                kind: GestureKind::Swipe,
                dx: 6.0,
                dy: 0.5,
                scale: 1.0,
                rotation: 0.0
            },
            ClientEvent::GestureEnd {
                kind: GestureKind::Swipe,
                cancelled: false
            },
            begin_event(GestureKind::Pinch, 2),
            ClientEvent::GestureUpdate {
                kind: GestureKind::Pinch,
                dx: 1.0,
                dy: 1.0,
                scale: 1.25,
                rotation: 5.0
            },
            ClientEvent::GestureEnd {
                kind: GestureKind::Pinch,
                cancelled: true
            },
            begin_event(GestureKind::Hold, 4),
            ClientEvent::GestureEnd {
                kind: GestureKind::Hold,
                cancelled: false
            },
        ]
    );
    assert_eq!(f.received(b), []);
    assert_eq!(f.state()["touch"]["gesture"], Value::Null);
}

/// While a gesture is in progress the pointer comes onto another window
/// (here: the window under it moves away). The first client is told the
/// gesture was cancelled; the second never saw its begin and hears nothing
/// of the rest.
#[test]
fn a_gesture_ends_cancelled_when_the_pointer_leaves_the_surface() {
    let mut f = Fixture::new();
    let (a, window_a) = f.add_window("a", (100, 100));
    let (b, _) = f.add_window("b", (400, 100));
    f.move_pointer(150.0, 150.0);
    f.clear();

    f.gesture(&[begin("swipe", 3), update("swipe", 1.0, 0.0, 1.0, 0.0)]);
    assert_eq!(
        f.state()["touch"]["gesture"],
        serde_json::json!({ "kind": "swipe", "owner": "client" })
    );
    f.move_pointer(450.0, 150.0);
    f.gesture(&[update("swipe", 2.0, 0.0, 1.0, 0.0), end("swipe", false)]);
    let received = f.received(a);
    assert_eq!(
        received.last(),
        Some(&ClientEvent::GestureEnd {
            kind: GestureKind::Swipe,
            cancelled: true
        }),
        "{received:?}"
    );
    assert_eq!(received.len(), 3, "{received:?}");
    assert_eq!(f.received(b), []);

    // The window under the pointer moving away does the same.
    f.move_pointer(150.0, 150.0);
    f.clear();
    f.gesture(&[begin("pinch", 2)]);
    f.control(Request::MoveWindow {
        window: window_a,
        x: 700,
        y: 400,
    });
    f.steps(2);
    f.gesture(&[end("pinch", false)]);
    assert_eq!(
        f.received(a),
        [
            ClientEvent::GestureBegin {
                kind: GestureKind::Pinch,
                fingers: 2,
                surface: SurfaceKind::Window
            },
            ClientEvent::GestureEnd {
                kind: GestureKind::Pinch,
                cancelled: true
            },
        ]
    );
}

/// Pieces of a gesture without its begin, or of another kind than the one
/// in progress, go nowhere. A begin while one is in progress ends that one
/// first, as cancelled.
#[test]
fn gesture_events_out_of_order_are_dropped() {
    let mut f = Fixture::new();
    let (a, _) = f.add_window("a", (100, 100));
    f.move_pointer(150.0, 150.0);
    f.clear();

    f.gesture(&[update("swipe", 1.0, 0.0, 1.0, 0.0), end("pinch", false)]);
    assert_eq!(f.received(a), []);

    f.gesture(&[begin("swipe", 3), update("pinch", 1.0, 0.0, 2.0, 0.0)]);
    f.gesture(&[begin("pinch", 2), end("pinch", false)]);
    assert_eq!(
        f.received(a),
        [
            ClientEvent::GestureBegin {
                kind: GestureKind::Swipe,
                fingers: 3,
                surface: SurfaceKind::Window
            },
            ClientEvent::GestureEnd {
                kind: GestureKind::Swipe,
                cancelled: true
            },
            ClientEvent::GestureBegin {
                kind: GestureKind::Pinch,
                fingers: 2,
                surface: SurfaceKind::Window
            },
            ClientEvent::GestureEnd {
                kind: GestureKind::Pinch,
                cancelled: false
            },
        ]
    );
    // An unknown kind is refused by the control interface.
    let response = f.compositor.control(&begin("rotate", 2));
    assert_eq!(response["ok"], false, "{response}");
}
