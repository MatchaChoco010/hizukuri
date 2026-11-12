//! The protocol layer on its own: a [`WlServer`] and test clients in one
//! thread, without the compositor core. What the server reports and what
//! clients are told is checked directly, and the test plays the part of
//! whoever manages windows.

use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use hizukuri_test_client::{
    ClientEvent, ClientOptions, ExternalDmabuf, PopupOptions, TestClient, ToplevelRequest,
    Transform,
};
use hizukuri_wl::{
    BufferKind, DmabufConfig, FOURCC_ARGB8888, MODIFIER_LINEAR, ResizeEdges, ServerConfig,
    WindowId, WindowRequest, WindowStates, WlEvent, WlServer,
};

const BTN_LEFT: u32 = 0x110;

struct Rig {
    server: WlServer,
    clients: Vec<TestClient>,
    /// Every event the server reported, in order.
    events: Vec<WlEvent>,
    time: u64,
}

impl Rig {
    fn new(config: ServerConfig) -> Rig {
        Rig {
            server: WlServer::new(&config).unwrap(),
            clients: Vec::new(),
            events: Vec::new(),
            time: 0,
        }
    }

    fn add_client(&mut self, title: &str) -> usize {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.server.insert_client(server_end).unwrap();
        let options = ClientOptions {
            title: title.to_string(),
            ..Default::default()
        };
        self.clients
            .push(TestClient::connect(client_end, options).unwrap());
        self.clients.len() - 1
    }

    /// The server's half of a step: requests in, events out. Nothing is
    /// sent to clients yet.
    fn receive(&mut self) -> Vec<WlEvent> {
        self.server.dispatch_clients().unwrap();
        let events = self.server.drain_events();
        self.events.extend(events.iter().cloned());
        events
    }

    /// The other half: frame callbacks, the flush, and the clients' turn.
    fn send(&mut self) {
        self.time += 16;
        self.server.send_frame_callbacks(None, self.time as u32);
        self.server.flush_clients().unwrap();
        for client in &mut self.clients {
            client.pump(None).unwrap();
        }
    }

    fn step(&mut self) {
        self.receive();
        self.send();
    }

    fn settle(&mut self, what: &str, mut done: impl FnMut(&mut Rig) -> bool) {
        for _ in 0..200 {
            self.step();
            if done(self) {
                for _ in 0..4 {
                    self.step();
                }
                return;
            }
        }
        panic!("did not settle: {what}\nevents: {:#?}", self.events);
    }

    /// Adds a client and waits until its window has contents.
    fn add_mapped_client(&mut self, title: &str) -> (usize, WindowId) {
        let client = self.add_client(title);
        self.settle("window mapped", |rig| {
            rig.server
                .windows()
                .iter()
                .any(|id| rig.server.window(*id).is_some_and(|w| w.mapped))
                && rig.clients[client].is_drawn()
        });
        let window = self
            .server
            .windows()
            .into_iter()
            .find(|id| {
                self.server
                    .window(*id)
                    .is_some_and(|w| w.title.as_deref() == Some(title))
            })
            .expect("the window exists");
        (client, window)
    }

    fn requests(&self) -> Vec<WindowRequest> {
        self.events
            .iter()
            .filter_map(|event| match event {
                WlEvent::WindowRequest { request, .. } => Some(*request),
                _ => None,
            })
            .collect()
    }

    fn press(&mut self, window: WindowId, pressed: bool) {
        let surface = self.server.window(window).unwrap().surface;
        self.time += 1;
        self.server
            .pointer_motion(Some((surface, (10.0, 10.0))), self.time * 1000);
        self.server
            .pointer_button(BTN_LEFT, pressed, self.time * 1000);
        self.server.pointer_frame();
    }
}

fn configures(events: &[ClientEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, ClientEvent::Configure { .. }))
        .count()
}

/// What a client asks of the window manager arrives as events, and nothing
/// is done about it. An answer reaches the client; and when there is no
/// answer, the configure the protocol asks for is sent anyway.
#[test]
fn window_requests_are_reported_and_left_to_the_window_manager() {
    let mut rig = Rig::new(ServerConfig::default());
    let (client, window) = rig.add_mapped_client("asker");
    assert_eq!(rig.requests(), []);

    // Maximize, with nobody answering: the request is reported, the window
    // is told nothing new, but it does get its configure.
    let before = configures(rig.clients[client].events());
    rig.clients[client]
        .request(ToplevelRequest::Maximize)
        .unwrap();
    rig.settle("the request is reported", |rig| {
        rig.requests() == [WindowRequest::Maximize]
    });
    assert_eq!(
        rig.events
            .iter()
            .filter(|e| matches!(e, WlEvent::WindowRequest { window: w, .. } if *w == window))
            .count(),
        1
    );
    assert_eq!(configures(rig.clients[client].events()), before + 1);
    assert!(
        !rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::WindowState { .. }))
    );
    assert!(!rig.server.window(window).unwrap().maximized);

    // The same request, answered: a size and the maximized state.
    rig.clients[client]
        .request(ToplevelRequest::Maximize)
        .unwrap();
    rig.settle("the second request", |rig| rig.requests().len() == 2);
    rig.server.configure_window(window, Some((800, 600)), true);
    rig.server.set_window_states(
        window,
        WindowStates {
            maximized: true,
            ..Default::default()
        },
    );
    rig.settle("the window is maximized", |rig| {
        rig.server
            .window(window)
            .is_some_and(|w| w.maximized && w.size == Some((800, 600)))
    });
    assert!(
        rig.clients[client]
            .events()
            .contains(&ClientEvent::WindowState {
                maximized: true,
                fullscreen: false,
                resizing: false,
            })
    );

    // The others are reported the same way.
    for request in [
        ToplevelRequest::Unmaximize,
        ToplevelRequest::Fullscreen,
        ToplevelRequest::Unfullscreen,
        ToplevelRequest::Minimize,
        ToplevelRequest::WindowMenu { x: 12, y: 34 },
    ] {
        rig.clients[client].request(request).unwrap();
    }
    rig.settle("five more requests", |rig| rig.requests().len() == 7);
    assert_eq!(
        rig.requests()[2..],
        [
            WindowRequest::Unmaximize,
            WindowRequest::Fullscreen { output: None },
            WindowRequest::Unfullscreen,
            WindowRequest::Minimize,
            WindowRequest::ShowMenu { position: (12, 34) },
        ]
    );
    // Fullscreen, answered; the window keeps what it was told until it is
    // told otherwise.
    rig.server.set_window_states(
        window,
        WindowStates {
            fullscreen: true,
            ..Default::default()
        },
    );
    rig.settle("fullscreen", |rig| {
        rig.server
            .window(window)
            .is_some_and(|w| w.fullscreen && !w.maximized)
    });
}

/// An interactive move or resize can only start from a pointer button that
/// is still down on the window. Without one the request is not reported.
#[test]
fn moves_and_resizes_need_a_held_button() {
    let mut rig = Rig::new(ServerConfig::default());
    let (client, window) = rig.add_mapped_client("dragged");

    // No press at all.
    rig.clients[client].request(ToplevelRequest::Move).unwrap();
    for _ in 0..10 {
        rig.step();
    }
    assert_eq!(rig.requests(), []);

    // Pressed and held.
    rig.press(window, true);
    rig.settle("the press reaches the client", |rig| {
        rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: true, .. }))
    });
    rig.clients[client].request(ToplevelRequest::Move).unwrap();
    rig.clients[client]
        .request(ToplevelRequest::Resize {
            top: false,
            bottom: true,
            left: false,
            right: true,
        })
        .unwrap();
    rig.settle("move and resize are reported", |rig| {
        rig.requests().len() == 2
    });
    assert_eq!(
        rig.requests(),
        [
            WindowRequest::Move,
            WindowRequest::Resize {
                edges: ResizeEdges {
                    bottom: true,
                    right: true,
                    ..Default::default()
                }
            },
        ]
    );

    // Released: the old serial is no good any more.
    rig.press(window, false);
    rig.settle("the release reaches the client", |rig| {
        rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: false, .. }))
    });
    rig.clients[client].request(ToplevelRequest::Move).unwrap();
    for _ in 0..10 {
        rig.step();
    }
    assert_eq!(rig.requests().len(), 2);
}

/// A commit that attaches a dmabuf which is not ready is held back until it
/// is. Nothing of it is visible before, the server is not kept waiting, and
/// its descriptor tells the caller when to come back.
///
/// The "dmabuf" is one end of a socket pair: it becomes readable when the
/// test writes to the other end, the way a dmabuf does when the GPU is done
/// with it.
#[test]
fn a_commit_waits_until_its_dmabuf_is_ready() {
    let mut rig = Rig::new(ServerConfig {
        dmabuf: Some(DmabufConfig::cpu_readable(None)),
        ..Default::default()
    });
    let (client, window) = rig.add_mapped_client("gpu");
    let kind = |rig: &Rig| rig.server.window_surfaces(window)[0].buffer_kind;
    let generation = |rig: &Rig| rig.server.window_surfaces(window)[0].generation;
    assert_eq!(kind(&rig), Some(BufferKind::Shm));
    let before = generation(&rig);

    let (gate, mut key) = UnixStream::pair().unwrap();
    rig.clients[client]
        .attach_dmabuf(&ExternalDmabuf {
            fd: gate.as_fd(),
            width: 64,
            height: 32,
            fourcc: FOURCC_ARGB8888,
            modifier: MODIFIER_LINEAR,
            offset: 0,
            stride: 64 * 4,
            transform: Transform::Normal,
            y_invert: false,
        })
        .unwrap();
    rig.events.clear();
    for _ in 0..20 {
        rig.step();
    }
    // The commit arrived and is waiting: the window still has what it had.
    assert_eq!(kind(&rig), Some(BufferKind::Shm));
    assert_eq!(generation(&rig), before);
    assert!(
        !rig.events
            .iter()
            .any(|e| matches!(e, WlEvent::WindowChanged(_))),
        "{:?}",
        rig.events
    );
    assert!(!readable(&mut rig.server), "nothing to do, yet readable");

    // The buffer becomes ready. The server's descriptor says so, and the
    // next dispatch applies the commit.
    key.write_all(&[1]).unwrap();
    assert!(readable(&mut rig.server), "the server was not woken");
    let events = rig.receive();
    assert!(
        events.contains(&WlEvent::WindowChanged(window)),
        "{events:?}"
    );
    assert_eq!(kind(&rig), Some(BufferKind::Dmabuf));
    assert_eq!(
        rig.server.window_surfaces(window)[0].buffer_size,
        Some((64, 32))
    );
    assert!(generation(&rig) > before);
    rig.send();
}

/// Whether the server's descriptor is readable right now.
fn readable(server: &mut WlServer) -> bool {
    let fd = server.display_fd().try_clone_to_owned().unwrap();
    let mut event_loop: calloop::EventLoop<bool> = calloop::EventLoop::try_new().unwrap();
    event_loop
        .handle()
        .insert_source(
            calloop::generic::Generic::new(fd, calloop::Interest::READ, calloop::Mode::Level),
            |_, _, seen| {
                *seen = true;
                Ok(calloop::PostAction::Continue)
            },
        )
        .unwrap();
    let mut seen = false;
    event_loop
        .dispatch(Duration::from_millis(50), &mut seen)
        .unwrap();
    seen
}

fn popup(x: i32, y: i32, slide: bool) -> PopupOptions {
    PopupOptions {
        x,
        y,
        width: 80,
        height: 50,
        color: [0, 0, 255, 255],
        grab: false,
        slide,
    }
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

/// A new popup is announced with the place it was given, and the client is
/// only told at the flush. In between, whoever draws the windows can place
/// it inside the part of the parent it knows to be visible, or anywhere.
#[test]
fn a_popup_can_be_placed_before_the_client_is_told() {
    let mut rig = Rig::new(ServerConfig::default());
    let (client, window) = rig.add_mapped_client("parent");

    // Left alone, it goes where its positioner says.
    rig.clients[client].open_popup(popup(20, 30, true)).unwrap();
    rig.settle("configured", |rig| {
        popup_configure(rig.clients[client].events()).is_some()
    });
    assert_eq!(
        popup_configure(rig.clients[client].events()),
        Some((20, 30, 80, 50))
    );
    let announced: Vec<_> = rig
        .events
        .iter()
        .filter_map(|e| match e {
            WlEvent::PopupPlacement { window: w, popup } => Some((*w, *popup)),
            _ => None,
        })
        .collect();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].0, window);
    let request = rig.server.popup_request(announced[0].1).unwrap();
    assert_eq!(request.window, window);
    assert_eq!(request.parent, rig.server.window(window).unwrap().surface);
    assert_eq!(request.parent_origin, (0, 0));
    assert_eq!(request.anchor_rect, (20, 30, 1, 1));
    assert_eq!(request.wanted, (20, 30, 80, 50));
    assert_eq!(request.geometry, (20, 30, 80, 50));
    assert_eq!(request.may_slide, (true, true));
    assert_eq!(request.may_flip, (false, false));
    assert_eq!(request.may_resize, (false, false));
    rig.clients[client].close_popup();
    rig.settle("closed", |rig| {
        rig.server.window_surfaces(window).len() == 1
    });

    // Placed inside bounds that only the caller knows: say the window is
    // drawn so that only its left 90 by 70 are on screen. The popup may
    // slide, so it slides.
    for (name, place, expected) in [
        ("within", Some((0, 0, 90, 70)), (10, 20, 80, 50)),
        ("exact", None, (5, 6, 70, 40)),
    ] {
        rig.clients[client].clear_events();
        rig.events.clear();
        rig.clients[client].open_popup(popup(20, 30, true)).unwrap();
        // Step by hand: the event arrives, the client knows nothing yet.
        let mut placed = false;
        for _ in 0..20 {
            for event in rig.receive() {
                if let WlEvent::PopupPlacement { popup, .. } = event {
                    assert!(
                        popup_configure(rig.clients[client].events()).is_none(),
                        "{name}: the client was told before anyone could place the popup"
                    );
                    match place {
                        Some(bounds) => {
                            assert_eq!(
                                rig.server.place_popup_within(popup, bounds),
                                Some(expected),
                                "{name}"
                            );
                        }
                        None => assert!(rig.server.set_popup_geometry(popup, expected)),
                    }
                    assert_eq!(rig.server.popup_request(popup).unwrap().geometry, expected);
                    placed = true;
                }
            }
            rig.send();
            if popup_configure(rig.clients[client].events()).is_some() {
                break;
            }
        }
        assert!(placed, "{name}: no placement event");
        assert_eq!(
            popup_configure(rig.clients[client].events()),
            Some(expected),
            "{name}"
        );
        // And it is drawn there.
        rig.settle("popup drawn", |rig| {
            rig.server
                .window_surfaces(window)
                .iter()
                .any(|node| node.offset == (expected.0, expected.1) && node.buffer_size.is_some())
        });
        rig.clients[client].close_popup();
        rig.settle("closed again", |rig| {
            rig.server.window_surfaces(window).len() == 1
        });
    }
}
