//! What xwayland-satellite asks of the protocol layer, in its order, played
//! by the test client: X11 windows that belong to another (dialogs), size
//! limits, menus and tooltips placed where the X11 window is, and
//! activation (xdg-activation). The test plays the window manager.

use std::os::unix::net::UnixStream;

use proto_test_client::{ClientEvent, ClientOptions, DialogOptions, TestClient};
use proto_wl::{ServerConfig, WindowId, WindowRequest, WlEvent, WlServer};

const KEY_A: u32 = 30;

struct Rig {
    server: WlServer,
    clients: Vec<TestClient>,
    events: Vec<WlEvent>,
    time: u64,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            server: WlServer::new(&ServerConfig::default()).unwrap(),
            clients: Vec::new(),
            events: Vec::new(),
            time: 0,
        }
    }

    fn add_client(&mut self, options: ClientOptions) -> usize {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.server.insert_client(server_end).unwrap();
        self.clients
            .push(TestClient::connect(client_end, options).unwrap());
        self.clients.len() - 1
    }

    fn step(&mut self) {
        self.server.dispatch_clients().unwrap();
        self.events.extend(self.server.drain_events());
        self.time += 16;
        self.server.send_frame_callbacks(None, self.time as u32);
        self.server.flush_clients().unwrap();
        for client in &mut self.clients {
            client.pump(None).unwrap();
        }
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

    fn window(&self, title: &str) -> Option<WindowId> {
        self.server.windows().into_iter().find(|id| {
            self.server
                .window(*id)
                .is_some_and(|w| w.mapped && w.title.as_deref() == Some(title))
        })
    }

    fn mapped(&mut self, title: &str) -> WindowId {
        let title = title.to_string();
        self.settle(&format!("{title} mapped"), |rig| {
            rig.window(&title).is_some()
        });
        self.window(&title).unwrap()
    }

    fn activations(&self) -> Vec<(WindowId, bool)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                WlEvent::WindowRequest {
                    window,
                    request: WindowRequest::Activate { from_input },
                } => Some((*window, *from_input)),
                _ => None,
            })
            .collect()
    }
}

fn options(title: &str) -> ClientOptions {
    ClientOptions {
        title: title.to_string(),
        width: 300,
        height: 200,
        ..Default::default()
    }
}

/// An X11 window that is transient for another arrives as a second
/// toplevel of the same client with that one as its parent; X11 size
/// hints arrive as size limits. Both are reported with the window, and so
/// is the process of the client (how the compositor tells the windows of
/// the xwayland-satellite it started from others).
#[test]
fn dialogs_report_their_parent_and_windows_their_limits_and_client() {
    let mut rig = Rig::new();
    let client = rig.add_client(ClientOptions {
        min_size: Some((100, 50)),
        max_size: Some((640, 480)),
        ..options("main")
    });
    let main = rig.mapped("main");
    let info = rig.server.window(main).unwrap();
    assert_eq!(info.parent, None);
    assert_eq!((info.min_size, info.max_size), ((100, 50), (640, 480)));
    // Both ends of the socket pair belong to this process.
    assert_eq!(info.client_pid, Some(std::process::id() as i32));

    // A dialog, and a splash screen whose size is fixed (minimum and
    // maximum the same), without a parent.
    rig.clients[client]
        .open_dialog(DialogOptions {
            title: "dialog".to_string(),
            ..Default::default()
        })
        .unwrap();
    rig.clients[client]
        .open_dialog(DialogOptions {
            title: "splash".to_string(),
            parent: false,
            min_size: Some((200, 100)),
            max_size: Some((200, 100)),
            ..Default::default()
        })
        .unwrap();
    let dialog = rig.mapped("dialog");
    let splash = rig.mapped("splash");
    let dialog = rig.server.window(dialog).unwrap();
    let splash = rig.server.window(splash).unwrap();
    assert_eq!(dialog.parent, Some(main));
    assert_eq!(dialog.size, Some((120, 80)), "it chose its own size");
    assert_eq!(splash.parent, None);
    assert_eq!((splash.min_size, splash.max_size), ((200, 100), (200, 100)));
    assert_eq!(dialog.client_pid, info.client_pid);
    assert!(
        rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::DialogConfigure { dialog: 0, .. }))
    );

    // The X11 window is unmapped: its toplevel goes, the others stay.
    rig.clients[client].close_dialog(0);
    rig.settle("the dialog is gone", |rig| rig.window("dialog").is_none());
    assert!(rig.window("splash").is_some());
}

/// A menu (an override-redirect X11 window) is a popup anchored to the
/// whole parent and moved by the distance between the two X11 windows. It
/// lands there, takes no grab, and slides back into the window's bounds
/// when it would leave them.
#[test]
fn x11_menus_land_at_their_offset_and_slide_into_the_bounds() {
    let mut rig = Rig::new();
    let client = rig.add_client(options("menus"));
    let window = rig.mapped("menus");
    let configured = |rig: &Rig| {
        rig.clients[client]
            .events()
            .iter()
            .rev()
            .find_map(|e| match e {
                ClientEvent::PopupConfigure {
                    x,
                    y,
                    width,
                    height,
                } => Some((*x, *y, *width, *height)),
                _ => None,
            })
    };

    rig.clients[client]
        .open_x11_popup(40, 30, 120, 80, [0, 0, 255, 255])
        .unwrap();
    rig.settle("the menu", |rig| configured(rig).is_some());
    assert_eq!(configured(&rig), Some((40, 30, 120, 80)));
    let surfaces = rig.server.window_surfaces(window);
    let popup = surfaces.last().unwrap();
    assert_eq!((popup.offset, popup.size), ((40, 30), (120, 80)));
    // Without a grab the keyboard stays with the window.
    assert_eq!(
        rig.server.keyboard_focus(),
        None,
        "nobody gave the keyboard to anything"
    );
    rig.clients[client].close_popup();

    // A menu that would stick out of the output slides back in.
    rig.server.set_popup_bounds(window, Some((0, 0, 400, 300)));
    rig.clients[client]
        .open_x11_popup(350, 260, 120, 80, [0, 0, 255, 255])
        .unwrap();
    rig.settle("the second menu", |rig| {
        configured(rig).is_some_and(|c| c.2 == 120 && c != (40, 30, 120, 80))
    });
    assert_eq!(configured(&rig), Some((280, 220, 120, 80)));
    assert!(
        !rig.clients[client]
            .events()
            .contains(&ClientEvent::PopupDone)
    );
}

/// An X11 client asking for one of its windows to be activated
/// (`_NET_ACTIVE_WINDOW`) makes xwayland-satellite activate it with a token
/// carrying its latest keyboard serial. The request reaches the window
/// manager, which learns whether the token came from input of the focused
/// client.
#[test]
fn activation_requests_say_whether_they_came_from_input() {
    let mut rig = Rig::new();
    let client = rig.add_client(options("asker"));
    let main = rig.mapped("asker");
    rig.clients[client]
        .open_dialog(DialogOptions::default())
        .unwrap();
    let dialog = rig.mapped("dialog");
    let other = rig.add_client(options("other"));
    let other_window = rig.mapped("other");

    // The asking client has the keyboard and was just typed into.
    rig.server.set_keyboard_focus(Some(main));
    rig.step();
    rig.server.keyboard_key(KEY_A, true, 1000);
    rig.server.keyboard_key(KEY_A, false, 2000);
    rig.settle("the key", |rig| {
        rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::Key { pressed: false, .. }))
    });
    rig.clients[client].activate(Some(0), true).unwrap();
    rig.settle("the activation", |rig| rig.activations().len() == 1);
    assert_eq!(rig.activations(), [(dialog, true)]);

    // A token without a serial, and one from a client that does not have
    // the keyboard, are reported as not coming from input.
    rig.clients[client].activate(None, false).unwrap();
    rig.clients[other].activate(None, true).unwrap();
    rig.settle("two more", |rig| rig.activations().len() == 3);
    let mut rest = rig.activations()[1..].to_vec();
    rest.sort_by_key(|(window, _)| window.0);
    let mut expected = vec![(main, false), (other_window, false)];
    expected.sort_by_key(|(window, _)| window.0);
    assert_eq!(rest, expected);
    // Nothing was done about them: the window manager decides.
    assert_eq!(
        rig.server.keyboard_focus(),
        rig.server.window(main).map(|w| w.surface)
    );
}
