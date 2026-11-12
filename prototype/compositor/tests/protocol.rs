//! Protocol tests (test layer 2).
//!
//! The compositor core and the test clients run in one thread, connected by
//! socket pairs. The test steps both by hand, so the order of events is
//! fixed and no test waits on a clock. There is no renderer: these tests
//! check what clients are told and what the compositor believes, not
//! pixels. The headless backend's frame clock is ticked explicitly.

use std::os::unix::net::UnixStream;

use proto_backend::headless::{HeadlessBackend, HeadlessConfig, HeadlessOutputConfig};
use proto_compositor::compositor::{Compositor, CompositorOptions};
use proto_compositor::control::Request;
use proto_compositor::scene::FlatScene;
use proto_test_client::{ClientEvent, ClientOptions, PopupOptions, SurfaceKind, TestClient};
use serde_json::Value;

const KEY_A: u32 = 30;
const KEY_LEFTSHIFT: u32 = 42;
const BTN_LEFT: u32 = 0x110;

struct Fixture {
    compositor: Compositor<HeadlessBackend>,
    clients: Vec<TestClient>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_output(HeadlessOutputConfig::default())
    }

    fn with_output(output: HeadlessOutputConfig) -> Self {
        Self::with_outputs(vec![output])
    }

    fn with_outputs(outputs: Vec<HeadlessOutputConfig>) -> Self {
        Self::with_options(outputs, CompositorOptions::default())
    }

    fn with_options(outputs: Vec<HeadlessOutputConfig>, options: CompositorOptions) -> Self {
        let config = HeadlessConfig {
            outputs,
            ..Default::default()
        };
        let backend = HeadlessBackend::new(&config, None).unwrap();
        let compositor = Compositor::new(options, backend, Box::new(FlatScene::new(None))).unwrap();
        Self {
            compositor,
            clients: Vec::new(),
        }
    }

    /// Connects a client and returns its index.
    fn add_client(&mut self, options: ClientOptions) -> usize {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.compositor.wl().insert_client(server_end).unwrap();
        self.clients
            .push(TestClient::connect(client_end, options).unwrap());
        self.clients.len() - 1
    }

    /// One step: the compositor handles requests and a frame clock tick,
    /// then every client handles what it received.
    fn step(&mut self) {
        self.compositor.dispatch_clients().unwrap();
        self.compositor.backend().tick();
        self.compositor.process().unwrap();
        for client in &mut self.clients {
            client.pump(None).unwrap();
        }
    }

    /// Steps until `done` holds. Panics when it does not within a generous
    /// number of steps: a request and its answer need a few round trips,
    /// never hundreds.
    fn settle(&mut self, what: &str, mut done: impl FnMut(&mut Fixture) -> bool) {
        for _ in 0..200 {
            self.step();
            if done(self) {
                // Let anything still in flight arrive.
                for _ in 0..4 {
                    self.step();
                }
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

    /// Adds a client and waits until its window is mapped. Returns the
    /// client index and the window id.
    fn add_mapped_client(&mut self, options: ClientOptions) -> (usize, u64) {
        let title = options.title.clone();
        let index = self.add_client(options);
        self.settle("window mapped", |f| {
            f.clients[index].is_drawn() && window_by_title(&f.state(), &title).is_some()
        });
        let state = self.state();
        let window = window_by_title(&state, &title).unwrap();
        assert_eq!(window["mapped"], true);
        (index, window["id"].as_u64().unwrap())
    }

    fn events(&self, client: usize) -> &[ClientEvent] {
        self.clients[client].events()
    }
}

fn window_by_title<'a>(state: &'a Value, title: &str) -> Option<&'a Value> {
    state["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"] == title && w["mapped"] == true)
}

fn options(title: &str, width: u32, height: u32) -> ClientOptions {
    ClientOptions {
        title: title.to_string(),
        width,
        height,
        ..Default::default()
    }
}

fn keys(events: &[ClientEvent]) -> Vec<(u32, bool, String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Key {
                keycode,
                pressed,
                keysym,
                text,
            } => Some((*keycode, *pressed, keysym.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn window_is_configured_mapped_and_described() {
    let mut f = Fixture::new();
    let (client, window) = f.add_mapped_client(ClientOptions {
        title: "first".into(),
        app_id: "test.first".into(),
        width: 320,
        height: 240,
        ..Default::default()
    });

    // The client was left to choose its size, and told it is activated.
    let configures: Vec<_> = f
        .events(client)
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Configure {
                width,
                height,
                activated,
            } => Some((*width, *height, *activated)),
            _ => None,
        })
        .collect();
    assert!(!configures.is_empty());
    assert!(configures.iter().all(|c| (c.0, c.1) == (0, 0)));
    assert!(configures.last().unwrap().2);
    assert!(f.events(client).contains(&ClientEvent::Drawn {
        width: 320,
        height: 240
    }));

    let state = f.state();
    let w = window_by_title(&state, "first").unwrap();
    assert_eq!(w["id"], window);
    assert_eq!(w["app_id"], "test.first");
    assert_eq!(w["activated"], true);
    assert_eq!(w["size"], serde_json::json!([320, 240]));
    // First slot of the cascade.
    assert_eq!(w["position"], serde_json::json!([32, 32]));
    assert_eq!(w["visual"]["position"], serde_json::json!([32.0, 32.0]));
    assert_eq!(w["output"], 1);

    let surface = &w["surfaces"][0];
    assert_eq!(surface["role"], "toplevel");
    assert_eq!(surface["size"], serde_json::json!([320, 240]));
    assert_eq!(surface["buffer_size"], serde_json::json!([320, 240]));
    // No color protocol yet: BT.709 primaries, gamma 2.2, premultiplied.
    assert_eq!(surface["image_description"]["name"], "srgb-gamma22");
    assert_eq!(surface["alpha_mode"], "premultiplied-electrical");
    assert_eq!(
        surface["decode_pipeline"],
        "unpremultiply | eotf:gamma22 | primaries:srgb->bt2020 | luminance:x(80/80) | premultiply"
    );

    assert_eq!(state["focus"]["window"], window);
    assert_eq!(state["focus"]["keyboard_surface"], w["surface"]);
    assert_eq!(state["backend"], "headless");
    assert_eq!(state["renderer"], Value::Null);
    assert_eq!(
        state["outputs"][0]["encode_pipeline"],
        "luminance:x(80/80) | primaries:bt2020->srgb | clip[0,1] | inverse-eotf:gamma22"
    );
}

#[test]
fn buffer_is_released_and_frame_callback_fires_after_a_frame() {
    let mut f = Fixture::new();
    let (client, _) = f.add_mapped_client(options("w", 64, 64));
    f.settle("release and frame callback", |f| {
        let events = f.events(client);
        events.contains(&ClientEvent::BufferReleased)
            && events
                .iter()
                .any(|e| matches!(e, ClientEvent::FrameDone { .. }))
    });
    // The buffer is held from the commit until the frame that consumes it,
    // so the release comes after the client drew, not before.
    let events = f.events(client);
    let drawn = events
        .iter()
        .position(|e| matches!(e, ClientEvent::Drawn { .. }))
        .unwrap();
    let released = events
        .iter()
        .position(|e| *e == ClientEvent::BufferReleased)
        .unwrap();
    assert!(released > drawn);

    // Each redraw gets exactly one release and one frame callback.
    f.clients[client].clear_events();
    f.clients[client].set_color([10, 20, 30, 255]);
    f.settle("second release", |f| {
        f.events(client).contains(&ClientEvent::BufferReleased)
    });
    let events = f.events(client);
    assert_eq!(
        events
            .iter()
            .filter(|e| **e == ClientEvent::BufferReleased)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ClientEvent::FrameDone { .. }))
            .count(),
        1
    );
}

#[test]
fn keys_reach_the_focused_client_with_a_usable_keymap() {
    let mut f = Fixture::new();
    let (client, _) = f.add_mapped_client(options("typing", 100, 100));
    f.settle("keyboard enter", |f| {
        f.events(client).contains(&ClientEvent::KeyboardEnter)
    });
    assert!(
        f.events(client)
            .iter()
            .any(|e| matches!(e, ClientEvent::Keymap { size } if *size > 0))
    );
    assert!(f.events(client).contains(&ClientEvent::RepeatInfo {
        rate: 25,
        delay: 400
    }));

    f.clients[client].clear_events();
    for (keycode, pressed) in [
        (KEY_A, true),
        (KEY_A, false),
        (KEY_LEFTSHIFT, true),
        (KEY_A, true),
        (KEY_A, false),
        (KEY_LEFTSHIFT, false),
    ] {
        f.control(Request::Key { keycode, pressed });
    }
    f.settle("keys delivered", |f| keys(f.events(client)).len() == 6);

    let received = keys(f.events(client));
    let brief: Vec<(u32, bool, &str)> = received
        .iter()
        .map(|(code, pressed, _, text)| (*code, *pressed, text.as_str()))
        .collect();
    assert_eq!(
        brief,
        [
            (KEY_A, true, "a"),
            (KEY_A, false, "a"),
            (KEY_LEFTSHIFT, true, ""),
            // Shift is held: the keymap the compositor sent turns the same
            // key into an upper case letter.
            (KEY_A, true, "A"),
            (KEY_A, false, "A"),
            (KEY_LEFTSHIFT, false, ""),
        ]
    );
    assert_eq!(received[0].2, "a");
    assert_eq!(received[2].2, "Shift_L");
    assert_eq!(received[3].2, "A");
    // The shift state was announced as a modifier change.
    assert!(
        f.events(client)
            .iter()
            .any(|e| matches!(e, ClientEvent::Modifiers { depressed, .. } if *depressed != 0))
    );
}

#[test]
fn pointer_events_carry_surface_local_coordinates() {
    let mut f = Fixture::new();
    let (client, window) = f.add_mapped_client(options("pointing", 200, 100));
    f.control(Request::MoveWindow {
        window,
        x: 300,
        y: 150,
    });
    f.clients[client].clear_events();

    f.control(Request::PointerMotion { x: 310.0, y: 175.5 });
    f.control(Request::PointerMotion { x: 499.0, y: 249.0 });
    f.control(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    f.control(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    f.control(Request::PointerAxis {
        horizontal: 0.0,
        vertical: 15.0,
        v120_horizontal: None,
        v120_vertical: Some(120),
    });
    f.control(Request::PointerAxis {
        horizontal: -7.5,
        vertical: 0.0,
        v120_horizontal: None,
        v120_vertical: None,
    });
    // One pixel past the right edge: outside the window.
    f.control(Request::PointerMotion { x: 500.0, y: 200.0 });
    f.settle("pointer leave", |f| {
        f.events(client).contains(&ClientEvent::PointerLeave)
    });

    let events: Vec<ClientEvent> = f
        .events(client)
        .iter()
        .filter(|e| {
            matches!(
                e,
                ClientEvent::PointerEnter { .. }
                    | ClientEvent::PointerLeave
                    | ClientEvent::PointerMotion { .. }
                    | ClientEvent::PointerButton { .. }
                    | ClientEvent::PointerAxis { .. }
                    | ClientEvent::PointerAxisValue120 { .. }
                    | ClientEvent::PointerFrame
            )
        })
        .cloned()
        .collect();
    assert_eq!(
        events,
        [
            ClientEvent::PointerEnter { x: 10.0, y: 25.5 },
            ClientEvent::PointerFrame,
            ClientEvent::PointerMotion { x: 199.0, y: 99.0 },
            ClientEvent::PointerFrame,
            ClientEvent::PointerButton {
                button: BTN_LEFT,
                pressed: true
            },
            ClientEvent::PointerFrame,
            ClientEvent::PointerButton {
                button: BTN_LEFT,
                pressed: false
            },
            ClientEvent::PointerFrame,
            ClientEvent::PointerAxisValue120 {
                axis: 0,
                value: 120
            },
            ClientEvent::PointerAxis {
                axis: 0,
                value: 15.0
            },
            ClientEvent::PointerFrame,
            ClientEvent::PointerAxis {
                axis: 1,
                value: -7.5
            },
            ClientEvent::PointerFrame,
            ClientEvent::PointerLeave,
            ClientEvent::PointerFrame,
        ]
    );

    let state = f.state();
    assert_eq!(state["focus"]["pointer_surface"], Value::Null);
    assert_eq!(
        state["pointer"]["position"],
        serde_json::json!([500.0, 200.0])
    );
}

#[test]
fn relative_motion_moves_the_pointer_and_stays_on_the_output() {
    let mut f = Fixture::new();
    let (client, window) = f.add_mapped_client(options("relative", 100, 100));
    f.control(Request::MoveWindow { window, x: 0, y: 0 });
    f.clients[client].clear_events();

    f.control(Request::PointerMotion { x: 10.0, y: 10.0 });
    let moved = f.control(Request::PointerMotionRelative { dx: 5.0, dy: 2.5 });
    assert_eq!(moved["position"], serde_json::json!([15.0, 12.5]));
    // Far beyond the 1280x720 output: clamped to its last pixel.
    let clamped = f.control(Request::PointerMotionRelative {
        dx: 100000.0,
        dy: 100000.0,
    });
    assert_eq!(clamped["position"], serde_json::json!([1279.0, 719.0]));
    let clamped = f.control(Request::PointerMotionRelative {
        dx: -100000.0,
        dy: -100000.0,
    });
    assert_eq!(clamped["position"], serde_json::json!([0.0, 0.0]));

    f.settle("pointer back on the window", |f| {
        f.events(client)
            .iter()
            .filter(|e| matches!(e, ClientEvent::PointerEnter { .. }))
            .count()
            == 2
    });
    assert!(
        f.events(client)
            .contains(&ClientEvent::PointerMotion { x: 15.0, y: 12.5 })
    );
    assert!(f.events(client).contains(&ClientEvent::PointerLeave));
}

#[test]
fn focus_moves_to_new_windows_and_follows_clicks() {
    let mut f = Fixture::new();
    let (first, first_window) = f.add_mapped_client(options("one", 100, 100));
    f.settle("first has the keyboard", |f| {
        f.events(first).contains(&ClientEvent::KeyboardEnter)
    });

    let (second, second_window) = f.add_mapped_client(options("two", 100, 100));
    f.settle("focus moved to the second window", |f| {
        f.events(first).contains(&ClientEvent::KeyboardLeave)
            && f.events(second).contains(&ClientEvent::KeyboardEnter)
    });
    // The first window was told it is no longer activated.
    assert_eq!(
        f.events(first).iter().rev().find_map(|e| match e {
            ClientEvent::Configure { activated, .. } => Some(*activated),
            _ => None,
        }),
        Some(false)
    );
    let state = f.state();
    assert_eq!(state["focus"]["window"], second_window);
    // Stacking order, bottom to top.
    let order: Vec<u64> = state["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .collect();
    assert_eq!(order, [first_window, second_window]);

    // Keys go to the second window only.
    f.clients[first].clear_events();
    f.clients[second].clear_events();
    f.control(Request::Key {
        keycode: KEY_A,
        pressed: true,
    });
    f.control(Request::Key {
        keycode: KEY_A,
        pressed: false,
    });
    f.settle("key at second", |f| keys(f.events(second)).len() == 2);
    assert!(keys(f.events(first)).is_empty());

    // Place the windows apart and click on the first: it gets the focus
    // and is raised.
    f.control(Request::MoveWindow {
        window: first_window,
        x: 0,
        y: 0,
    });
    f.control(Request::MoveWindow {
        window: second_window,
        x: 400,
        y: 0,
    });
    f.clients[first].clear_events();
    f.clients[second].clear_events();
    f.control(Request::PointerMotion { x: 50.0, y: 50.0 });
    f.control(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
    f.control(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
    f.settle("focus back at the first window", |f| {
        f.events(first).contains(&ClientEvent::KeyboardEnter)
            && f.events(second).contains(&ClientEvent::KeyboardLeave)
    });
    let state = f.state();
    assert_eq!(state["focus"]["window"], first_window);
    let order: Vec<u64> = state["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_u64().unwrap())
        .collect();
    assert_eq!(order, [second_window, first_window]);
    assert_eq!(
        f.events(first).iter().rev().find_map(|e| match e {
            ClientEvent::Configure { activated, .. } => Some(*activated),
            _ => None,
        }),
        Some(true)
    );
    // The click itself reached the window that was clicked.
    assert!(f.events(first).contains(&ClientEvent::PointerButton {
        button: BTN_LEFT,
        pressed: true
    }));
}

#[test]
fn overlapping_windows_send_pointer_input_to_the_topmost() {
    let mut f = Fixture::new();
    let (below, below_window) = f.add_mapped_client(options("below", 200, 200));
    let (above, above_window) = f.add_mapped_client(options("above", 200, 200));
    f.control(Request::MoveWindow {
        window: below_window,
        x: 0,
        y: 0,
    });
    f.control(Request::MoveWindow {
        window: above_window,
        x: 100,
        y: 100,
    });
    f.clients[below].clear_events();
    f.clients[above].clear_events();

    // Inside both rectangles: the upper window gets it, with coordinates
    // relative to its own origin.
    f.control(Request::PointerMotion { x: 150.0, y: 150.0 });
    f.settle("enter above", |f| {
        f.events(above)
            .contains(&ClientEvent::PointerEnter { x: 50.0, y: 50.0 })
    });
    assert!(
        !f.events(below)
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerEnter { .. }))
    );

    // Only inside the lower window.
    f.control(Request::PointerMotion { x: 50.0, y: 50.0 });
    f.settle("enter below", |f| {
        f.events(below)
            .contains(&ClientEvent::PointerEnter { x: 50.0, y: 50.0 })
    });
    assert!(f.events(above).contains(&ClientEvent::PointerLeave));
}

#[test]
fn pointer_focus_is_updated_when_a_window_moves_under_the_pointer() {
    let mut f = Fixture::new();
    let (client, window) = f.add_mapped_client(options("moving", 100, 100));
    f.control(Request::MoveWindow {
        window,
        x: 500,
        y: 500,
    });
    f.control(Request::PointerMotion { x: 20.0, y: 30.0 });
    f.clients[client].clear_events();

    // The pointer stays put; the window comes to it.
    f.control(Request::MoveWindow { window, x: 0, y: 0 });
    f.settle("enter after move", |f| {
        f.events(client)
            .contains(&ClientEvent::PointerEnter { x: 20.0, y: 30.0 })
    });
    f.control(Request::MoveWindow {
        window,
        x: 500,
        y: 500,
    });
    f.settle("leave after move", |f| {
        f.events(client).contains(&ClientEvent::PointerLeave)
    });
}

#[test]
fn closing_a_window_moves_the_focus() {
    let mut f = Fixture::new();
    let (first, first_window) = f.add_mapped_client(options("stays", 100, 100));
    let (second, second_window) = f.add_mapped_client(options("goes", 100, 100));
    f.clients[first].clear_events();

    f.control(Request::CloseWindow {
        window: second_window,
    });
    f.settle("window closed", |f| {
        f.clients[second].is_closed()
            && f.state()["windows"].as_array().unwrap().len() == 1
            && f.events(first).contains(&ClientEvent::KeyboardEnter)
    });
    let state = f.state();
    assert_eq!(state["focus"]["window"], first_window);
    assert_eq!(state["windows"][0]["activated"], true);

    let refused = f.compositor.control(&Request::CloseWindow {
        window: second_window,
    });
    assert_eq!(refused["ok"], false);
}

#[test]
fn a_client_that_disconnects_takes_its_window_with_it() {
    let mut f = Fixture::new();
    let (first, first_window) = f.add_mapped_client(options("stays", 100, 100));
    let (second, _) = f.add_mapped_client(options("vanishes", 100, 100));
    f.clients[first].clear_events();

    // Dropping the client closes its connection without any goodbye.
    drop(f.clients.remove(second));
    f.settle("window of the dead client removed", |f| {
        f.state()["windows"].as_array().unwrap().len() == 1
            && f.events(first).contains(&ClientEvent::KeyboardEnter)
    });
    let state = f.state();
    assert_eq!(state["windows"][0]["id"], first_window);
    assert_eq!(state["focus"]["window"], first_window);
}

#[test]
fn subsurfaces_are_part_of_the_window_and_take_input() {
    let mut f = Fixture::new();
    let (client, window) = f.add_mapped_client(options("parent", 200, 200));
    f.control(Request::MoveWindow { window, x: 0, y: 0 });
    // Partly outside the parent on the right.
    f.clients[client]
        .add_subsurface(150, 20, 100, 50, [0, 255, 0, 255])
        .unwrap();
    f.settle("subsurface in the tree", |f| {
        f.state()["windows"][0]["surfaces"]
            .as_array()
            .unwrap()
            .len()
            == 2
            && f.state()["windows"][0]["surfaces"][1]["buffer_size"] != Value::Null
    });
    let state = f.state();
    let surfaces = state["windows"][0]["surfaces"].as_array().unwrap();
    // Back to front: the parent, then the subsurface above it.
    assert_eq!(surfaces[0]["role"], "toplevel");
    assert_eq!(surfaces[1]["role"], "subsurface");
    assert_eq!(surfaces[1]["offset"], serde_json::json!([150, 20]));
    assert_eq!(surfaces[1]["size"], serde_json::json!([100, 50]));

    // A point over the part of the subsurface that sticks out of the
    // parent still hits the subsurface. The test client reports pointer
    // coordinates of whichever of its surfaces has the pointer.
    f.clients[client].clear_events();
    f.control(Request::PointerMotion { x: 220.0, y: 30.0 });
    f.settle("enter subsurface", |f| {
        f.events(client)
            .contains(&ClientEvent::PointerEnter { x: 70.0, y: 10.0 })
    });
    assert_eq!(f.state()["focus"]["pointer_surface"], surfaces[1]["id"]);
}

#[test]
fn output_scale_is_announced_to_surfaces() {
    let mut f = Fixture::with_output(HeadlessOutputConfig {
        width: 1920,
        height: 1080,
        scale: 2.0,
        ..Default::default()
    });
    let (client, _) = f.add_mapped_client(options("hidpi", 100, 100));
    assert!(
        f.events(client)
            .contains(&ClientEvent::PreferredBufferScale { scale: 2 })
    );
    let state = f.state();
    assert_eq!(state["outputs"][0]["size"], serde_json::json!([1920, 1080]));
    assert_eq!(
        state["outputs"][0]["logical_size"],
        serde_json::json!([960, 540])
    );
    assert_eq!(state["outputs"][0]["scale"], 2.0);
}

#[test]
fn control_requests_report_errors() {
    let mut f = Fixture::new();
    for request in [
        Request::MoveWindow {
            window: 999,
            x: 0,
            y: 0,
        },
        Request::FocusWindow { window: Some(999) },
        Request::SetWindowOpacity {
            window: 999,
            opacity: 0.5,
        },
        Request::SetSurfaceColor {
            surface: 999,
            image_description: "display-p3".into(),
            alpha_mode: None,
        },
        Request::Screenshot {
            path: "/nonexistent/x.png".into(),
            composite_path: None,
            output: None,
        },
    ] {
        let response = f.compositor.control(&request);
        assert_eq!(response["ok"], false, "{request:?} should fail");
        assert!(response["error"].is_string());
    }
    assert_eq!(f.compositor.control(&Request::Ping)["ok"], true);
    assert!(!f.compositor.should_quit());
    assert_eq!(f.compositor.control(&Request::Quit)["ok"], true);
    assert!(f.compositor.should_quit());
}

#[test]
fn surface_color_override_changes_the_decode_pipeline() {
    let mut f = Fixture::new();
    f.add_mapped_client(options("colorful", 50, 50));
    let surface = f.state()["windows"][0]["surface"].as_u64().unwrap();
    f.control(Request::SetSurfaceColor {
        surface,
        image_description: "display-p3".into(),
        alpha_mode: Some("straight".into()),
    });
    let state = f.state();
    let node = &state["windows"][0]["surfaces"][0];
    assert_eq!(node["image_description"]["name"], "display-p3");
    assert_eq!(node["alpha_mode"], "straight");
    assert_eq!(
        node["decode_pipeline"],
        "eotf:srgb-piecewise | primaries:display-p3->bt2020 | luminance:x(80/80) | premultiply"
    );
    let refused = f.compositor.control(&Request::SetSurfaceColor {
        surface,
        image_description: "nope".into(),
        alpha_mode: None,
    });
    assert_eq!(refused["ok"], false);
}

// ---------------------------------------------------------------------------
// Popups
// ---------------------------------------------------------------------------

fn popup(x: i32, y: i32, grab: bool, slide: bool) -> PopupOptions {
    PopupOptions {
        x,
        y,
        width: 80,
        height: 50,
        color: [0, 0, 255, 255],
        grab,
        slide,
    }
}

fn press(f: &mut Fixture, x: f64, y: f64) {
    f.control(Request::PointerMotion { x, y });
    f.control(Request::PointerButton {
        button: BTN_LEFT,
        pressed: true,
    });
}

fn release(f: &mut Fixture) {
    f.control(Request::PointerButton {
        button: BTN_LEFT,
        pressed: false,
    });
}

fn click(f: &mut Fixture, x: f64, y: f64) {
    press(f, x, y);
    release(f);
}

fn popup_configure(events: &[ClientEvent]) -> Option<(i32, i32, i32, i32)> {
    events.iter().rev().find_map(|e| match e {
        ClientEvent::PopupConfigure {
            x,
            y,
            width,
            height,
        } => Some((*x, *y, *width, *height)),
        _ => None,
    })
}

/// A popup goes where its positioner says, relative to the window. Near
/// the edge of the output it is moved to stay on the output, but only when
/// the positioner allows that.
#[test]
fn popups_are_placed_by_their_positioner_and_kept_on_the_output() {
    let mut f = Fixture::new();
    let (client, window) = f.add_mapped_client(options("parent", 200, 100));
    f.control(Request::MoveWindow {
        window,
        x: 100,
        y: 100,
    });
    f.settle("moved", |f| {
        window_by_title(&f.state(), "parent").unwrap()["position"] == serde_json::json!([100, 100])
    });

    f.clients[client]
        .open_popup(popup(20, 30, false, false))
        .unwrap();
    f.settle("popup configured", |f| {
        popup_configure(f.events(client)).is_some()
            && window_by_title(&f.state(), "parent").unwrap()["surfaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["role"] == "popup" && s["has_contents"] != Value::Null)
    });
    assert_eq!(popup_configure(f.events(client)), Some((20, 30, 80, 50)));
    let state = f.state();
    let surfaces = window_by_title(&state, "parent").unwrap()["surfaces"]
        .as_array()
        .unwrap()
        .clone();
    let popup_surface = surfaces.iter().find(|s| s["role"] == "popup").unwrap();
    assert_eq!(popup_surface["offset"], serde_json::json!([20, 30]));
    assert_eq!(popup_surface["size"], serde_json::json!([80, 50]));
    // The popup is above the window: the pointer on it goes to the popup,
    // with coordinates of the popup's surface.
    f.clients[client].clear_events();
    f.control(Request::PointerMotion { x: 125.0, y: 135.0 });
    f.settle("pointer on the popup", |f| {
        f.clients[client].pointer_focus() == Some(SurfaceKind::Popup)
    });
    assert!(
        f.events(client)
            .contains(&ClientEvent::PointerEnter { x: 5.0, y: 5.0 })
    );
    // Without a grab the keyboard stays with the window.
    assert_eq!(
        f.clients[client].keyboard_focus(),
        Some(SurfaceKind::Window)
    );
    f.clients[client].close_popup();
    f.settle("popup gone", |f| {
        !window_by_title(&f.state(), "parent").unwrap()["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["role"] == "popup")
    });

    // In the bottom right corner of the 1280x720 output, the same popup
    // would end at (1300, 730).
    f.control(Request::MoveWindow {
        window,
        x: 1200,
        y: 650,
    });
    f.settle("moved to the corner", |f| {
        window_by_title(&f.state(), "parent").unwrap()["position"] == serde_json::json!([1200, 650])
    });
    // A positioner that allows nothing gets what it asked for.
    f.clients[client].clear_events();
    f.clients[client]
        .open_popup(popup(20, 30, false, false))
        .unwrap();
    f.settle("rigid popup", |f| {
        popup_configure(f.events(client)).is_some()
    });
    assert_eq!(popup_configure(f.events(client)), Some((20, 30, 80, 50)));
    f.clients[client].close_popup();
    // One that may slide is slid back onto the output: 20 to the left, 10
    // up.
    f.clients[client].clear_events();
    f.clients[client]
        .open_popup(popup(20, 30, false, true))
        .unwrap();
    f.settle("sliding popup", |f| {
        popup_configure(f.events(client)).is_some()
    });
    assert_eq!(popup_configure(f.events(client)), Some((0, 20, 80, 50)));
}

/// A popup that takes a grab (a menu) gets the keyboard; other clients get
/// no pointer events while it is open; a press outside its client
/// dismisses it, and the press then does what it would have done anyway.
#[test]
fn a_grabbing_popup_takes_the_keyboard_and_is_dismissed_by_a_press_elsewhere() {
    let mut f = Fixture::new();
    let (other, other_window) = f.add_mapped_client(options("other", 100, 100));
    let (menu, menu_window) = f.add_mapped_client(options("menu", 200, 100));
    f.control(Request::MoveWindow {
        window: other_window,
        x: 500,
        y: 0,
    });
    f.control(Request::MoveWindow {
        window: menu_window,
        x: 0,
        y: 0,
    });
    // The grab needs the serial of an input event: the click that opens
    // the menu.
    click(&mut f, 10.0, 10.0);
    f.settle("click at the menu client", |f| {
        f.events(menu)
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: false, .. }))
            && f.clients[menu].keyboard_focus() == Some(SurfaceKind::Window)
    });

    f.clients[menu]
        .open_popup(popup(50, 20, true, false))
        .unwrap();
    f.settle("the popup has the keyboard", |f| {
        f.clients[menu].keyboard_focus() == Some(SurfaceKind::Popup)
    });
    assert!(f.clients[menu].has_popup());
    // Keys go to the popup's client.
    f.clients[menu].clear_events();
    f.control(Request::Key {
        keycode: KEY_A,
        pressed: true,
    });
    f.control(Request::Key {
        keycode: KEY_A,
        pressed: false,
    });
    f.settle("key at the menu", |f| keys(f.events(menu)).len() == 2);
    // The compositor still considers the window focused.
    assert_eq!(f.state()["focus"]["window"], menu_window);

    // The pointer over the other client: it hears nothing.
    f.clients[other].clear_events();
    f.control(Request::PointerMotion { x: 550.0, y: 50.0 });
    for _ in 0..10 {
        f.step();
    }
    assert!(
        !f.events(other)
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerEnter { .. })),
        "{:?}",
        f.events(other)
    );
    // A press inside the menu's own window does not dismiss the popup
    // (the client decides what that means).
    click(&mut f, 150.0, 90.0);
    for _ in 0..10 {
        f.step();
    }
    assert!(f.clients[menu].has_popup());

    // A press on the other client dismisses it.
    f.clients[menu].clear_events();
    press(&mut f, 550.0, 50.0);
    f.settle("dismissed", |f| {
        f.events(menu).contains(&ClientEvent::PopupDone)
            && f.events(other)
                .iter()
                .any(|e| matches!(e, ClientEvent::PointerButton { pressed: true, .. }))
    });
    release(&mut f);
    assert!(!f.clients[menu].has_popup());
    // The press went on to focus the other window, and the keyboard
    // followed: the grab did not leave it behind.
    f.settle("keyboard at the other client", |f| {
        f.clients[other].keyboard_focus() == Some(SurfaceKind::Window)
            && f.clients[menu].keyboard_focus().is_none()
    });
    assert_eq!(f.state()["focus"]["window"], other_window);
    let state = f.state();
    assert!(
        !window_by_title(&state, "menu").unwrap()["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["role"] == "popup")
    );

    // A popup the client closes itself gives the keyboard back to its
    // window.
    click(&mut f, 10.0, 10.0);
    f.settle("menu client focused again", |f| {
        f.clients[menu].keyboard_focus() == Some(SurfaceKind::Window)
    });
    f.clients[menu]
        .open_popup(popup(50, 20, true, false))
        .unwrap();
    f.settle("popup has the keyboard again", |f| {
        f.clients[menu].keyboard_focus() == Some(SurfaceKind::Popup)
    });
    f.clients[menu].close_popup();
    f.settle("keyboard back at the window", |f| {
        f.clients[menu].keyboard_focus() == Some(SurfaceKind::Window)
    });
}

// ---------------------------------------------------------------------------
// Selection, drag and drop
// ---------------------------------------------------------------------------

const TEXT: &str = "text/plain;charset=utf-8";

fn received(events: &[ClientEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        ClientEvent::Received { text, .. } => Some(text.clone()),
        _ => None,
    })
}

/// Copy in one client, paste in another: the selection is offered to the
/// client that has the keyboard, and the data goes from client to client
/// through a pipe.
#[test]
fn the_selection_goes_from_one_client_to_another() {
    let mut f = Fixture::new();
    let (copier, copier_window) = f.add_mapped_client(options("copier", 100, 100));
    let (paster, paster_window) = f.add_mapped_client(options("paster", 100, 100));
    f.control(Request::MoveWindow {
        window: copier_window,
        x: 0,
        y: 0,
    });
    f.control(Request::MoveWindow {
        window: paster_window,
        x: 300,
        y: 0,
    });
    click(&mut f, 50.0, 50.0);
    f.settle("copier focused", |f| {
        f.clients[copier].keyboard_focus() == Some(SurfaceKind::Window)
    });
    f.clients[copier]
        .set_selection(&[TEXT, "text/plain"], "copied text")
        .unwrap();
    // The owner is told about its own selection.
    f.settle("selection set", |f| {
        f.events(copier)
            .iter()
            .any(|e| matches!(e, ClientEvent::Selection { mime_types } if mime_types.len() == 2))
    });
    // The other client does not have the keyboard and is told nothing.
    assert!(
        !f.events(paster)
            .iter()
            .any(|e| matches!(e, ClientEvent::Selection { mime_types } if !mime_types.is_empty()))
    );

    // Focus the other client: now it sees the offer and can ask for it.
    f.clients[copier].clear_events();
    f.clients[paster].clear_events();
    click(&mut f, 350.0, 50.0);
    f.settle("offer at the paster", |f| {
        f.events(paster).contains(&ClientEvent::Selection {
            mime_types: vec![TEXT.to_string(), "text/plain".to_string()],
        })
    });
    assert!(f.clients[paster].request_selection(TEXT).unwrap());
    f.settle("pasted", |f| received(f.events(paster)).is_some());
    assert_eq!(received(f.events(paster)).as_deref(), Some("copied text"));
    assert!(f.events(copier).contains(&ClientEvent::SourceSend {
        mime_type: TEXT.to_string()
    }));

    // A new owner replaces the old one, which is told so.
    f.clients[paster].set_selection(&[TEXT], "second").unwrap();
    f.settle("first source cancelled", |f| {
        f.events(copier).contains(&ClientEvent::SourceCancelled)
    });
    // And back: the first client pastes what the second copied.
    f.clients[copier].clear_events();
    click(&mut f, 50.0, 50.0);
    f.settle("offer at the first client", |f| {
        f.events(copier).contains(&ClientEvent::Selection {
            mime_types: vec![TEXT.to_string()],
        })
    });
    assert!(f.clients[copier].request_selection(TEXT).unwrap());
    f.settle("pasted back", |f| received(f.events(copier)).is_some());
    assert_eq!(received(f.events(copier)).as_deref(), Some("second"));
}

/// Setting the selection takes the keyboard focus at the moment the
/// request is handled, not at the moment of the input event whose serial
/// it carries: Smithay checks the focus, not the serial. A client that
/// lost the focus in between is ignored, without being told.
///
/// This is what made wl-clipboard lose a copy now and then when another
/// window (a second wl-clipboard process) opened in the same instant. It
/// is recorded here as how things are, not as how they should be: weston
/// and wlroots go by the serial.
#[test]
fn a_client_that_lost_the_focus_cannot_set_the_selection() {
    let mut f = Fixture::new();
    let (first, first_window) = f.add_mapped_client(options("first", 100, 100));
    let (second, second_window) = f.add_mapped_client(options("second", 100, 100));
    f.control(Request::MoveWindow {
        window: first_window,
        x: 0,
        y: 0,
    });
    f.control(Request::MoveWindow {
        window: second_window,
        x: 300,
        y: 0,
    });
    // The first client gets the focus and an input serial.
    click(&mut f, 50.0, 50.0);
    f.settle("first focused", |f| {
        f.clients[first].keyboard_focus() == Some(SurfaceKind::Window)
    });
    assert_eq!(f.state()["selection"], Value::Null);

    // The focus moves on before the first client's request is sent.
    click(&mut f, 350.0, 50.0);
    f.compositor.dispatch_clients().unwrap();
    f.compositor.process().unwrap();
    f.clients[first].set_selection(&[TEXT], "too late").unwrap();
    f.settle("second focused", |f| {
        f.clients[second].keyboard_focus() == Some(SurfaceKind::Window)
    });
    assert_eq!(
        f.state()["selection"],
        Value::Null,
        "the stale request was honored"
    );
    assert!(
        !f.events(second)
            .iter()
            .any(|e| matches!(e, ClientEvent::Selection { mime_types } if !mime_types.is_empty()))
    );

    // With the focus, the same request works, and can be seen from
    // outside.
    click(&mut f, 50.0, 50.0);
    f.settle("first focused again", |f| {
        f.clients[first].keyboard_focus() == Some(SurfaceKind::Window)
    });
    f.clients[first].set_selection(&[TEXT], "in time").unwrap();
    f.settle("selection set", |f| {
        f.state()["selection"] == serde_json::json!([TEXT])
    });
}

/// Dragging from one window to another: the window under the pointer is
/// offered the data with surface-local coordinates, a release drops it
/// there, and the data arrives. A release over nothing cancels.
#[test]
fn drag_and_drop_delivers_data_to_the_window_under_the_pointer() {
    let mut f = Fixture::new();
    let (source, source_window) = f.add_mapped_client(options("source", 100, 100));
    let (target, target_window) = f.add_mapped_client(options("target", 100, 100));
    f.control(Request::MoveWindow {
        window: source_window,
        x: 0,
        y: 0,
    });
    f.control(Request::MoveWindow {
        window: target_window,
        x: 300,
        y: 0,
    });

    // Press on the source and start the drag, with an icon.
    press(&mut f, 50.0, 50.0);
    f.settle("press at the source", |f| {
        f.events(source)
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: true, .. }))
    });
    f.clients[source]
        .start_drag(&[TEXT], "dragged text", Some([255, 255, 0, 255]))
        .unwrap();
    f.settle("drag started", |f| {
        f.compositor
            .wl()
            .drag_icon_surfaces()
            .first()
            .is_some_and(|icon| icon.size == (16, 16))
    });
    // The drag is over its own window first; the offer follows the first
    // motion.
    f.control(Request::PointerMotion { x: 55.0, y: 50.0 });
    f.settle("enter at the source", |f| {
        f.events(source)
            .iter()
            .any(|e| matches!(e, ClientEvent::DragEnter { .. }))
    });

    // Over to the target.
    f.clients[source].clear_events();
    f.clients[target].clear_events();
    f.control(Request::PointerMotion { x: 340.0, y: 60.0 });
    f.settle("enter at the target", |f| {
        f.events(target).contains(&ClientEvent::DragEnter {
            x: 40.0,
            y: 60.0,
            mime_types: vec![TEXT.to_string()],
        })
    });
    assert!(f.events(source).contains(&ClientEvent::DragLeave));
    // The target accepted the type. The protocol has an event that tells
    // the source so (`wl_data_source.target`); Smithay at the pinned
    // revision does not send it, so it is not asserted here. Clients work
    // from the negotiated action and the drop instead.
    f.control(Request::PointerMotion { x: 350.0, y: 70.0 });
    f.settle("motion at the target", |f| {
        f.events(target)
            .contains(&ClientEvent::DragMotion { x: 50.0, y: 70.0 })
    });
    // No ordinary pointer events while dragging.
    assert!(
        !f.events(target)
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerEnter { .. }))
    );

    // Drop.
    release(&mut f);
    f.settle("dropped and transferred", |f| {
        received(f.events(target)).is_some()
            && f.events(source).contains(&ClientEvent::SourceFinished)
    });
    assert!(f.events(target).contains(&ClientEvent::DragDrop));
    assert_eq!(received(f.events(target)).as_deref(), Some("dragged text"));
    assert!(f.events(source).contains(&ClientEvent::SourceDropPerformed));
    assert!(f.events(source).contains(&ClientEvent::SourceSend {
        mime_type: TEXT.to_string()
    }));
    assert!(f.compositor.wl().drag_icon_surfaces().is_empty());
    // The pointer is an ordinary pointer again.
    f.clients[target].clear_events();
    f.control(Request::PointerMotion { x: 351.0, y: 70.0 });
    f.settle("pointer back", |f| {
        f.events(target).iter().any(|e| {
            matches!(
                e,
                ClientEvent::PointerEnter { .. } | ClientEvent::PointerMotion { .. }
            )
        })
    });

    // A drag released over the background is cancelled.
    f.clients[source].clear_events();
    press(&mut f, 50.0, 50.0);
    f.settle("press at the source again", |f| {
        f.events(source)
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: true, .. }))
    });
    f.clients[source]
        .start_drag(&[TEXT], "nowhere", None)
        .unwrap();
    for _ in 0..6 {
        f.step();
    }
    f.control(Request::PointerMotion { x: 55.0, y: 50.0 });
    f.settle("second drag started", |f| {
        f.events(source)
            .iter()
            .any(|e| matches!(e, ClientEvent::DragEnter { .. }))
    });
    f.control(Request::PointerMotion { x: 800.0, y: 500.0 });
    release(&mut f);
    f.settle("cancelled", |f| {
        f.events(source).contains(&ClientEvent::SourceCancelled)
    });
}

// ---------------------------------------------------------------------------
// Several outputs
// ---------------------------------------------------------------------------

fn second_output() -> Vec<HeadlessOutputConfig> {
    vec![
        HeadlessOutputConfig {
            name: "HEADLESS-1".to_string(),
            width: 800,
            height: 600,
            ..Default::default()
        },
        HeadlessOutputConfig {
            name: "HEADLESS-2".to_string(),
            width: 1280,
            height: 960,
            scale: 2.0,
            ..Default::default()
        },
    ]
}

/// With two outputs side by side, a window is on the one the layout put it
/// on: its surfaces enter that output only, and are told that output's
/// scale. Moving it across changes both.
#[test]
fn windows_enter_the_output_they_are_on() {
    let mut f = Fixture::with_outputs(second_output());
    let (client, window) = f.add_mapped_client(options("roamer", 100, 100));

    // Both outputs are announced, the second to the right of the first.
    let outputs: Vec<&ClientEvent> = f
        .events(client)
        .iter()
        .filter(|e| matches!(e, ClientEvent::Output { .. }))
        .collect();
    assert!(outputs.contains(&&ClientEvent::Output {
        name: "HEADLESS-1".to_string(),
        width: 800,
        height: 600,
        scale: 1,
        x: 0,
        y: 0,
    }));
    assert!(outputs.contains(&&ClientEvent::Output {
        name: "HEADLESS-2".to_string(),
        width: 1280,
        height: 960,
        scale: 2,
        x: 800,
        y: 0,
    }));

    // New windows start on the first output.
    let entered = |events: &[ClientEvent]| -> Vec<(bool, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                ClientEvent::SurfaceEnter { output } => Some((true, output.clone())),
                ClientEvent::SurfaceLeave { output } => Some((false, output.clone())),
                _ => None,
            })
            .collect()
    };
    let scale_of = |events: &[ClientEvent]| {
        events.iter().rev().find_map(|e| match e {
            ClientEvent::PreferredBufferScale { scale } => Some(*scale),
            _ => None,
        })
    };
    f.settle("on the first output", |f| {
        entered(f.events(client)).contains(&(true, "HEADLESS-1".to_string()))
    });
    // It was never on the second one, not even for a moment.
    assert_eq!(
        entered(f.events(client)),
        [(true, "HEADLESS-1".to_string())]
    );
    // Scale 1 is what a surface assumes; it is only told otherwise.
    assert_eq!(scale_of(f.events(client)).unwrap_or(1), 1);
    let state = f.state();
    assert_eq!(window_by_title(&state, "roamer").unwrap()["output"], 1);

    // Across to the second output.
    f.clients[client].clear_events();
    f.control(Request::MoveWindow {
        window,
        x: 900,
        y: 50,
    });
    f.settle("on the second output", |f| {
        entered(f.events(client))
            == [
                (true, "HEADLESS-2".to_string()),
                (false, "HEADLESS-1".to_string()),
            ]
            || entered(f.events(client))
                == [
                    (false, "HEADLESS-1".to_string()),
                    (true, "HEADLESS-2".to_string()),
                ]
    });
    assert_eq!(scale_of(f.events(client)), Some(2));
    assert!(
        f.events(client)
            .contains(&ClientEvent::FractionalScale { scale_120: 240 })
    );
    let state = f.state();
    assert_eq!(window_by_title(&state, "roamer").unwrap()["output"], 2);

    // Input is in one global space: the second output's (100, 50) logical
    // is (900, 50) globally.
    f.clients[client].clear_events();
    f.control(Request::PointerMotion { x: 910.0, y: 60.0 });
    f.settle("pointer on the moved window", |f| {
        f.events(client)
            .contains(&ClientEvent::PointerEnter { x: 10.0, y: 10.0 })
    });

    // And back.
    f.clients[client].clear_events();
    f.control(Request::MoveWindow {
        window,
        x: 10,
        y: 10,
    });
    f.settle("back on the first output", |f| {
        entered(f.events(client)).contains(&(true, "HEADLESS-1".to_string()))
            && entered(f.events(client)).contains(&(false, "HEADLESS-2".to_string()))
    });
    assert_eq!(scale_of(f.events(client)), Some(1));
}

/// Presentation feedback names the output a window is on, and comes from
/// that output's frames.
#[test]
fn presentation_feedback_names_the_windows_own_output() {
    let mut f = Fixture::with_outputs(second_output());
    let mut client_options = options("timed", 100, 100);
    client_options.presentation_feedback = true;
    let (client, window) = f.add_mapped_client(client_options);
    let presented_on = |events: &[ClientEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                ClientEvent::Presented { output, .. } => output.clone(),
                _ => None,
            })
            .collect()
    };
    f.settle("feedback from the first output", |f| {
        presented_on(f.events(client)).last().map(String::as_str) == Some("HEADLESS-1")
    });
    f.control(Request::MoveWindow {
        window,
        x: 900,
        y: 50,
    });
    f.clients[client].clear_events();
    f.clients[client].set_color([1, 2, 3, 255]);
    f.settle("feedback from the second output", |f| {
        !presented_on(f.events(client)).is_empty()
    });
    assert_eq!(presented_on(f.events(client)), ["HEADLESS-2"]);
}

// ---------------------------------------------------------------------------
// dmabuf: what is accepted
// ---------------------------------------------------------------------------

/// A file with pixels, standing in for a dma-buf at the protocol level.
fn pixel_file(name: &str, width: u32, height: u32) -> std::fs::File {
    use std::io::Write;
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{name}-{}.pixels", std::process::id()));
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(&vec![0x80u8; (width * height * 4) as usize])
        .unwrap();
    let file = std::fs::File::open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    file
}

/// Only the format and modifier pairs that were offered are taken. A buffer
/// that names another modifier, or none at all (`DRM_FORMAT_MOD_INVALID`),
/// is refused when it is created: "no modifier" does not mean "linear", it
/// means that only the driver which made the buffer knows its layout.
///
/// Smithay checks the format code and leaves the modifier to the
/// compositor.
#[test]
fn dmabufs_whose_modifier_was_not_offered_are_refused() {
    use std::os::fd::AsFd;

    use proto_test_client::{ExternalDmabuf, Transform};
    use proto_wl::{DmabufConfig, FOURCC_ARGB8888, MODIFIER_INVALID, MODIFIER_LINEAR};

    let options = CompositorOptions {
        server: proto_wl::ServerConfig {
            dmabuf: Some(DmabufConfig::cpu_readable(None)),
            ..Default::default()
        },
        ..Default::default()
    };
    fn buffer(file: &std::fs::File, modifier: u64) -> ExternalDmabuf<'_> {
        ExternalDmabuf {
            fd: file.as_fd(),
            width: 64,
            height: 32,
            fourcc: FOURCC_ARGB8888,
            modifier,
            offset: 0,
            stride: 64 * 4,
            transform: Transform::Normal,
            y_invert: false,
        }
    }
    // AMD's tiling: a real modifier, but not one that was offered.
    const TILED: u64 = 0x0200_0000_0000_0001;

    for (name, modifier) in [("unstated", MODIFIER_INVALID), ("tiled", TILED)] {
        let mut f = Fixture::with_options(vec![HeadlessOutputConfig::default()], options.clone());
        let (client, _) = f.add_mapped_client(options_of(name));
        let (bystander, _) = f.add_mapped_client(options_of("bystander"));
        let file = pixel_file(name, 64, 32);
        f.clients[client]
            .attach_dmabuf(&buffer(&file, modifier))
            .unwrap();
        // The buffer is refused, which for `create_immed` ends the
        // client's connection with a protocol error.
        let mut refused = None;
        for _ in 0..50 {
            f.compositor.dispatch_clients().unwrap();
            f.compositor.backend().tick();
            f.compositor.process().unwrap();
            f.clients[bystander].pump(None).unwrap();
            if let Err(error) = f.clients[client].pump(None) {
                refused = Some(error.to_string());
                break;
            }
        }
        let refused = refused.unwrap_or_else(|| panic!("{name}: the buffer was accepted"));
        assert!(
            refused
                .to_lowercase()
                .contains("zwp_linux_buffer_params_v1")
                || refused.to_lowercase().contains("invalid"),
            "{name}: {refused}"
        );
        // Nobody else is affected.
        f.clients.remove(client);
        f.settle("the refused client's window is gone", |f| {
            window_by_title(&f.state(), name).is_none()
        });
        assert!(window_by_title(&f.state(), "bystander").is_some());
    }

    // The offered pair is taken.
    let mut f = Fixture::with_options(vec![HeadlessOutputConfig::default()], options);
    let (client, _) = f.add_mapped_client(options_of("linear"));
    let file = pixel_file("linear", 64, 32);
    f.clients[client]
        .attach_dmabuf(&buffer(&file, MODIFIER_LINEAR))
        .unwrap();
    f.settle("the dmabuf is the window's buffer", |f| {
        window_by_title(&f.state(), "linear").unwrap()["surfaces"][0]["buffer_kind"] == "dmabuf"
    });
    assert_eq!(
        window_by_title(&f.state(), "linear").unwrap()["surfaces"][0]["buffer_size"],
        serde_json::json!([64, 32])
    );
}

fn options_of(title: &str) -> ClientOptions {
    options(title, 100, 100)
}
