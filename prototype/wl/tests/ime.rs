//! Input methods in the protocol layer: an application with a text field
//! (`zwp_text_input_v3`) and an input method (`zwp_input_method_v2`, its
//! keyboard grab and popup, `zwp_virtual_keyboard_v1`), both played by test
//! clients in the same thread as the server. The test plays the part of the
//! compositor core: it gives the keyboard focus and feeds keys.

use std::os::unix::net::UnixStream;

use proto_test_client::{
    ClientEvent, ClientOptions, ImeEvent, InputMethodMode, PopupOptions, SurfaceKind, TestClient,
};
use proto_wl::{InputMethodStatus, ServerConfig, SurfaceRole, WindowId, WlServer};

const KEY_A: u32 = 30;
const KEY_B: u32 = 48;
const KEY_C: u32 = 46;
const KEY_ENTER: u32 = 28;
const KEY_LEFT: u32 = 105;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;
const BTN_LEFT: u32 = 0x110;
/// The cursor rectangle the text field reports.
const CURSOR: (i32, i32, i32, i32) = (40, 30, 2, 16);

struct Rig {
    server: WlServer,
    clients: Vec<Option<TestClient>>,
    time: u64,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            server: WlServer::new(&ServerConfig::default()).unwrap(),
            clients: Vec::new(),
            time: 0,
        }
    }

    fn add_client(&mut self, options: ClientOptions) -> usize {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.server.insert_client(server_end).unwrap();
        self.clients
            .push(Some(TestClient::connect(client_end, options).unwrap()));
        self.clients.len() - 1
    }

    fn client(&mut self, index: usize) -> &mut TestClient {
        self.clients[index]
            .as_mut()
            .expect("the client is connected")
    }

    /// Disconnects a client, as when it crashes.
    fn drop_client(&mut self, index: usize) {
        self.clients[index] = None;
    }

    fn step(&mut self) {
        self.server.dispatch_clients().unwrap();
        self.server.drain_events();
        self.time += 16;
        self.server.send_frame_callbacks(None, self.time as u32);
        self.server.flush_clients().unwrap();
        for client in self.clients.iter_mut().flatten() {
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
        panic!(
            "did not settle: {what}\ninput method: {:#?}",
            self.server.input_method_status()
        );
    }

    /// A few round trips, for checking that something did not happen.
    fn steps(&mut self, count: usize) {
        for _ in 0..count {
            self.step();
        }
    }

    fn status(&self) -> InputMethodStatus {
        self.server.input_method_status()
    }

    /// An application window with a text field, mapped.
    fn add_application(&mut self, title: &str) -> (usize, WindowId) {
        let client = self.add_client(ClientOptions {
            title: title.to_string(),
            width: 200,
            height: 100,
            text_input: true,
            ..Default::default()
        });
        self.settle("window mapped", |rig| {
            rig.client(client).is_drawn() && window_by_title(&rig.server, title).is_some()
        });
        (client, window_by_title(&self.server, title).unwrap())
    }

    fn add_input_method(&mut self, mode: InputMethodMode) -> usize {
        let client = self.add_client(ClientOptions {
            window: false,
            input_method: Some(mode),
            ..Default::default()
        });
        self.steps(4);
        client
    }

    /// The usual start: an input method that holds the keyboard grab, and
    /// a focused application whose text field is enabled.
    fn active(&mut self) -> (usize, usize, WindowId) {
        let ime = self.add_input_method(InputMethodMode::Manual);
        assert!(self.status().bound, "the input method is the seat's");
        let (app, window) = self.add_application("app");
        self.server.set_keyboard_focus(Some(window));
        self.settle("text input entered", |rig| {
            rig.client(app).text_input_entered()
        });
        self.client(app).text_input_enable(Some(CURSOR)).unwrap();
        self.settle("input method activated", |rig| rig.client(ime).ime_active());
        assert!(self.status().active);
        self.client(ime).ime_grab_keyboard().unwrap();
        self.settle("keymap sent to the grab", |rig| {
            rig.client(ime)
                .ime_events()
                .iter()
                .any(|e| matches!(e, ImeEvent::GrabKeymap { .. }))
        });
        assert!(self.status().keyboard_grab);
        (ime, app, window)
    }

    fn key(&mut self, keycode: u32) {
        self.time += 10;
        self.server.keyboard_key(keycode, true, self.time * 1000);
        self.server
            .keyboard_key(keycode, false, self.time * 1000 + 500);
    }
}

fn window_by_title(server: &WlServer, title: &str) -> Option<WindowId> {
    server.windows().into_iter().find(|id| {
        server
            .window(*id)
            .is_some_and(|w| w.title.as_deref() == Some(title) && w.mapped)
    })
}

fn keys(events: &[ClientEvent]) -> Vec<(u32, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::Key {
                keycode, pressed, ..
            } => Some((*keycode, *pressed)),
            _ => None,
        })
        .collect()
}

fn grab_keys(events: &[ImeEvent]) -> Vec<(u32, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            ImeEvent::GrabKey {
                keycode, pressed, ..
            } => Some((*keycode, *pressed)),
            _ => None,
        })
        .collect()
}

fn input_popup(server: &WlServer, window: WindowId) -> Option<(i32, i32)> {
    server
        .window_surfaces(window)
        .iter()
        .find(|n| n.role == SurfaceRole::InputPopup)
        .map(|n| n.offset)
}

fn last_popup_rectangle(events: &[ImeEvent]) -> Option<(i32, i32, i32, i32)> {
    events.iter().rev().find_map(|e| match e {
        ImeEvent::PopupRectangle {
            x,
            y,
            width,
            height,
        } => Some((*x, *y, *width, *height)),
        _ => None,
    })
}

#[test]
fn the_focus_activates_the_input_method_and_keys_go_to_its_grab() {
    let mut rig = Rig::new();
    let (ime, app, _) = rig.active();
    let events = rig.client(ime).ime_events().to_vec();
    // What the text field sent came along with the activation.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ImeEvent::ContentType { .. }))
            && events
                .iter()
                .any(|e| matches!(e, ImeEvent::SurroundingText { .. })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ImeEvent::GrabRepeatInfo { .. })),
        "{events:?}"
    );

    rig.client(app).clear_events();
    rig.key(KEY_A);
    rig.settle("the key reached the grab", |rig| {
        grab_keys(rig.client(ime).ime_events()).contains(&(KEY_A, false))
    });
    assert_eq!(
        keys(rig.client(app).events()),
        vec![],
        "a key the grab took also reached the application"
    );
}

#[test]
fn composed_and_finished_text_reach_the_application() {
    let mut rig = Rig::new();
    let (ime, app, _) = rig.active();
    rig.client(app).clear_events();

    rig.client(ime).ime_preedit("ka", 2, 2).unwrap();
    rig.settle("preedit", |rig| {
        rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TextInputDone { .. }))
    });
    let events = rig.client(app).events().to_vec();
    assert!(
        events.contains(&ClientEvent::Preedit {
            text: "ka".to_string(),
            cursor_begin: 2,
            cursor_end: 2
        }),
        "{events:?}"
    );
    // `done` carries the number of commits the application had sent: the
    // state the input method's change was based on.
    let done = events
        .iter()
        .find_map(|e| match e {
            ClientEvent::TextInputDone { serial, commits } => Some((*serial, *commits)),
            _ => None,
        })
        .unwrap();
    assert_eq!(done.0, done.1, "{events:?}");

    rig.client(app).clear_events();
    rig.client(ime).ime_commit_string("か").unwrap();
    rig.settle("commit", |rig| {
        rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TextInputDone { .. }))
    });
    let events = rig.client(app).events().to_vec();
    assert!(
        events.contains(&ClientEvent::CommitString {
            text: "か".to_string()
        }),
        "{events:?}"
    );

    // A commit based on an outdated state of the input method is passed on
    // with a serial that tells the application so.
    rig.client(app).clear_events();
    rig.client(ime).ime_commit_with_serial(9999).unwrap();
    rig.settle("stale commit", |rig| {
        rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TextInputDone { .. }))
    });
    let stale = rig
        .client(app)
        .events()
        .iter()
        .find_map(|e| match e {
            ClientEvent::TextInputDone { serial, commits } => Some((*serial, *commits)),
            _ => None,
        })
        .unwrap();
    assert_ne!(stale.0, stale.1);
}

#[test]
fn keys_handed_back_reach_the_application_once() {
    let mut rig = Rig::new();
    let (ime, app, _) = rig.active();
    rig.client(app).clear_events();
    let grabbed_before = grab_keys(rig.client(ime).ime_events()).len();

    rig.client(ime).ime_virtual_key(KEY_LEFT, true).unwrap();
    rig.client(ime).ime_virtual_key(KEY_LEFT, false).unwrap();
    rig.settle("the key reached the application", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_LEFT, false))
    });
    assert_eq!(
        keys(rig.client(app).events()),
        vec![(KEY_LEFT, true), (KEY_LEFT, false)]
    );
    // The modifiers go with the key: while the grab holds the keyboard,
    // the application hears of them no other way.
    let events = rig.client(app).events().to_vec();
    let first_key = events
        .iter()
        .position(|e| matches!(e, ClientEvent::Key { .. }))
        .unwrap();
    assert!(
        events[..first_key]
            .iter()
            .any(|e| matches!(e, ClientEvent::Modifiers { .. })),
        "{events:?}"
    );
    assert_eq!(
        grab_keys(rig.client(ime).ime_events()).len(),
        grabbed_before,
        "a key handed back went to the grab again"
    );
}

#[test]
fn a_key_held_into_the_grab_is_released_for_the_application() {
    // A shortcut moved the focus to the application while its modifier was
    // held: the application has the press, the input method grabs the
    // keyboard, and the release goes to the grab. The input method hands
    // the release back; the application must get it, with the modifiers
    // that go with it.
    let mut rig = Rig::new();
    let (ime, app, _) = rig.active();
    rig.client(ime).ime_release_keyboard();
    rig.settle("grab released", |rig| !rig.status().keyboard_grab);
    rig.client(app).clear_events();
    rig.server.keyboard_key(KEY_LEFTMETA, true, 1_000_000);
    rig.settle("press at the application", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_LEFTMETA, true))
    });
    rig.client(ime).ime_grab_keyboard().unwrap();
    rig.settle("grabbed again", |rig| rig.status().keyboard_grab);
    rig.server.keyboard_key(KEY_LEFTMETA, false, 2_000_000);
    rig.settle("release at the grab", |rig| {
        grab_keys(rig.client(ime).ime_events()).contains(&(KEY_LEFTMETA, false))
    });
    assert!(!keys(rig.client(app).events()).contains(&(KEY_LEFTMETA, false)));

    rig.client(ime)
        .ime_virtual_key(KEY_LEFTMETA, false)
        .unwrap();
    rig.settle("release at the application", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_LEFTMETA, false))
    });
    let last_modifiers = rig.client(app).events().iter().rev().find_map(|e| match e {
        ClientEvent::Modifiers { depressed, .. } => Some(*depressed),
        _ => None,
    });
    assert_eq!(
        last_modifiers,
        Some(0),
        "the application still sees a modifier held"
    );
}

#[test]
fn modifiers_mirrored_by_the_input_method_reach_the_application() {
    // While the grab holds the keyboard, a modifier pressed on its own goes
    // to the grab. The input method mirrors the modifiers on its virtual
    // keyboard; that is how the application learns of them.
    let mut rig = Rig::new();
    let (ime, app, _) = rig.active();
    rig.client(ime).ime_virtual_key(KEY_LEFT, true).unwrap();
    rig.client(ime).ime_virtual_key(KEY_LEFT, false).unwrap();
    rig.settle("virtual keyboard ready", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_LEFT, false))
    });
    rig.client(app).clear_events();
    rig.server.keyboard_key(KEY_LEFTSHIFT, true, 1_000_000);
    rig.settle("shift at the grab", |rig| {
        rig.client(ime)
            .ime_events()
            .iter()
            .any(|e| matches!(e, ImeEvent::GrabModifiers { depressed: 1, .. }))
    });
    assert!(
        !rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::Modifiers { depressed: 1, .. })),
        "the application heard of the grab's modifiers directly"
    );
    rig.client(ime).ime_virtual_modifiers(1, 0, 0, 0).unwrap();
    rig.settle("shift at the application", |rig| {
        rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::Modifiers { depressed: 1, .. }))
    });
}

#[test]
fn moving_the_focus_deactivates_the_input_method() {
    let mut rig = Rig::new();
    let (ime, app, _) = rig.active();
    rig.client(ime)
        .ime_show_popup(100, 40, [0, 0, 255, 255])
        .unwrap();
    rig.settle("popup shown", |rig| {
        rig.status().popup.is_some_and(|p| p.shown)
    });
    let (other, other_window) = rig.add_application("other");
    rig.client(ime).clear_events();
    let deactivations = |rig: &mut Rig| {
        rig.client(ime)
            .ime_events()
            .iter()
            .filter(|e| **e == ImeEvent::Deactivate)
            .count()
    };
    rig.server.set_keyboard_focus(Some(other_window));
    rig.settle("deactivated", |rig| !rig.client(ime).ime_active());
    assert!(deactivations(&mut rig) >= 1);
    assert!(!rig.client(app).text_input_entered());
    assert!(rig.client(other).text_input_entered());
    let status = rig.status();
    assert!(!status.active, "{status:?}");
    assert!(
        !status.popup.as_ref().is_some_and(|p| p.shown),
        "{status:?}"
    );

    // The other application enables its field: the input method is
    // active again, and its popup goes with that window.
    rig.client(other).text_input_enable(Some(CURSOR)).unwrap();
    rig.settle("activated again", |rig| rig.client(ime).ime_active());
    rig.settle("popup with the other window", |rig| {
        input_popup(&rig.server, other_window).is_some()
    });
}

#[test]
fn a_crashed_input_method_gives_the_keyboard_back() {
    let mut rig = Rig::new();
    let (ime, app, _) = rig.active();
    rig.client(app).clear_events();
    // A key handed back and still held when the input method goes away.
    rig.client(ime).ime_virtual_key(KEY_B, true).unwrap();
    rig.settle("held key", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_B, true))
    });

    rig.drop_client(ime);
    rig.settle("input method gone", |rig| !rig.status().bound);
    let status = rig.status();
    assert!(!status.keyboard_grab && !status.active, "{status:?}");
    assert!(
        !rig.client(app).text_input_entered(),
        "the text input was not left"
    );
    rig.settle("held key released", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_B, false))
    });

    // Keys reach the application directly again.
    rig.key(KEY_C);
    rig.settle("key reached the application", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_C, false))
    });
}

#[test]
fn the_popup_is_shown_by_the_cursor_and_kept_on_screen() {
    let mut rig = Rig::new();
    let (ime, app, window) = rig.active();
    // The window is at the top left corner of a 400x300 output.
    rig.server.set_popup_bounds(window, Some((0, 0, 400, 300)));
    rig.client(ime)
        .ime_show_popup(100, 40, [0, 0, 255, 255])
        .unwrap();
    rig.settle("popup shown", |rig| {
        input_popup(&rig.server, window).is_some()
    });
    // Below the cursor; the input method learns where the cursor is,
    // relative to the popup.
    assert_eq!(input_popup(&rig.server, window), Some((40, 46)));
    assert_eq!(
        last_popup_rectangle(rig.client(ime).ime_events()),
        Some((0, -16, 2, 16))
    );
    let surfaces = rig.server.window_surfaces(window);
    assert_eq!(
        surfaces.last().map(|n| n.role),
        Some(SurfaceRole::InputPopup),
        "the popup is in front of the window's other surfaces"
    );

    // No room below: above the cursor. Near the right edge: moved left.
    rig.client(app)
        .text_input_set_cursor((380, 280, 2, 16))
        .unwrap();
    rig.settle("popup moved", |rig| {
        input_popup(&rig.server, window) == Some((300, 240))
    });
    rig.settle("rectangle sent", |rig| {
        last_popup_rectangle(rig.client(ime).ime_events()) == Some((80, 40, 2, 16))
    });

    // Disabled field: the popup is hidden.
    rig.client(app).text_input_disable().unwrap();
    rig.settle("popup hidden", |rig| {
        input_popup(&rig.server, window).is_none()
    });
    assert!(!rig.client(ime).ime_active());
}

#[test]
fn a_second_input_method_is_unavailable() {
    let mut rig = Rig::new();
    let first = rig.add_input_method(InputMethodMode::Manual);
    let second = rig.add_input_method(InputMethodMode::Manual);
    rig.settle("unavailable", |rig| {
        rig.client(second)
            .ime_events()
            .contains(&ImeEvent::Unavailable)
    });
    assert!(
        !rig.client(first)
            .ime_events()
            .contains(&ImeEvent::Unavailable)
    );
    // The first one still serves the seat.
    let (app, window) = rig.add_application("app");
    rig.server.set_keyboard_focus(Some(window));
    rig.settle("entered", |rig| rig.client(app).text_input_entered());
    rig.client(app).text_input_enable(Some(CURSOR)).unwrap();
    rig.settle("first activated", |rig| rig.client(first).ime_active());
    assert!(!rig.client(second).ime_active());
}

#[test]
fn an_echo_input_method_composes_commits_and_hands_keys_back() {
    // The input method on its own, as a real one runs: it grabs the
    // keyboard when activated, composes letters, commits them on Return,
    // and hands other keys back.
    let mut rig = Rig::new();
    let ime = rig.add_input_method(InputMethodMode::Echo);
    let (app, window) = rig.add_application("app");
    rig.server.set_keyboard_focus(Some(window));
    rig.settle("entered", |rig| rig.client(app).text_input_entered());
    rig.client(app).text_input_enable(Some(CURSOR)).unwrap();
    rig.settle("grabbed", |rig| {
        rig.client(ime).ime_grabbed() && rig.status().keyboard_grab
    });
    rig.client(app).clear_events();
    for key in [KEY_A, KEY_B, KEY_ENTER, KEY_LEFT] {
        rig.key(key);
        rig.steps(3);
    }
    rig.settle("handed back", |rig| {
        keys(rig.client(app).events()).contains(&(KEY_LEFT, false))
    });
    let events = rig.client(app).events().to_vec();
    assert!(
        events.contains(&ClientEvent::Preedit {
            text: "ab".to_string(),
            cursor_begin: 2,
            cursor_end: 2,
        }),
        "{events:?}"
    );
    assert!(
        events.contains(&ClientEvent::CommitString {
            text: "AB".to_string()
        }),
        "{events:?}"
    );
    assert_eq!(
        keys(&events),
        vec![(KEY_LEFT, true), (KEY_LEFT, false)],
        "only the key the input method did not use reached the application"
    );
    // The popup it showed while composing is by the cursor.
    assert_eq!(input_popup(&rig.server, window), Some((40, 46)));

    // Unfocused: it lets go of the keyboard.
    rig.server.set_keyboard_focus(None);
    rig.settle("released", |rig| !rig.status().keyboard_grab);
}

#[test]
fn a_menu_opens_while_the_input_method_holds_the_keyboard() {
    let mut rig = Rig::new();
    let (ime, app, window) = rig.active();
    let surface = rig.server.window(window).unwrap().surface;
    // A click on the window opens a menu that grabs, as menus do.
    rig.server
        .pointer_motion(Some((surface, (10.0, 10.0))), 1000);
    rig.server.pointer_frame();
    rig.server.pointer_button(BTN_LEFT, true, 2000);
    rig.server.pointer_frame();
    rig.settle("press", |rig| {
        rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: true, .. }))
    });
    rig.client(app)
        .open_popup(PopupOptions {
            x: 20,
            y: 20,
            width: 80,
            height: 50,
            color: [255, 0, 0, 255],
            grab: true,
            slide: false,
        })
        .unwrap();
    rig.settle("menu has the keyboard", |rig| {
        rig.client(app).keyboard_focus() == Some(SurfaceKind::Popup)
    });
    assert!(rig.client(app).has_popup(), "the menu was dismissed");
    assert!(
        !rig.client(app).events().contains(&ClientEvent::PopupDone),
        "the menu was dismissed"
    );
    assert!(
        rig.status().keyboard_grab,
        "the input method lost the keyboard"
    );

    // The focus stays with the menu while it is open, whatever the
    // compositor asks for in the meantime.
    rig.server.set_keyboard_focus(Some(window));
    rig.steps(4);
    assert_eq!(rig.client(app).keyboard_focus(), Some(SurfaceKind::Popup));

    // Closed: the window has the keyboard again.
    rig.server.pointer_button(BTN_LEFT, false, 3000);
    rig.client(app).close_popup();
    rig.settle("window has the keyboard", |rig| {
        rig.client(app).keyboard_focus() == Some(SurfaceKind::Window)
    });
    let _ = ime;
}

/// While the input method holds the keyboard and a menu is open, a touch
/// on another client still dismisses the menu, as it does without an input
/// method, and the touch reaches that client. The input method keeps its
/// grab.
#[test]
fn a_touch_elsewhere_dismisses_a_menu_opened_under_the_input_method() {
    let mut rig = Rig::new();
    rig.server.set_touch_available(true);
    let (ime, app, window) = rig.active();
    let (other, other_window) = rig.add_application("other");
    rig.client(other).request_touch();
    rig.settle("the other client has a wl_touch", |rig| {
        rig.client(other).has_touch()
    });
    let surface = rig.server.window(window).unwrap().surface;
    rig.server
        .pointer_motion(Some((surface, (10.0, 10.0))), 1000);
    rig.server.pointer_frame();
    rig.server.pointer_button(BTN_LEFT, true, 2000);
    rig.server.pointer_frame();
    rig.settle("press", |rig| {
        rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PointerButton { pressed: true, .. }))
    });
    rig.client(app)
        .open_popup(PopupOptions {
            x: 20,
            y: 20,
            width: 80,
            height: 50,
            color: [255, 0, 0, 255],
            grab: true,
            slide: false,
        })
        .unwrap();
    rig.settle("menu has the keyboard", |rig| {
        rig.client(app).keyboard_focus() == Some(SurfaceKind::Popup)
    });
    rig.server.pointer_button(BTN_LEFT, false, 2500);
    rig.server.pointer_frame();
    rig.steps(4);
    assert!(rig.client(app).has_popup(), "the menu closed by itself");

    let other_surface = rig.server.window(other_window).unwrap().surface;
    rig.server
        .touch_down(0, Some((other_surface, (5.0, 6.0))), 3000);
    rig.server.touch_frame();
    rig.settle("the menu is dismissed", |rig| {
        rig.client(app).events().contains(&ClientEvent::PopupDone)
    });
    assert!(
        rig.client(other)
            .events()
            .contains(&ClientEvent::TouchDown {
                id: 0,
                surface: SurfaceKind::Window,
                x: 5.0,
                y: 6.0
            })
    );
    // The keyboard goes back to the window the compositor wants it on.
    rig.client(app).close_popup();
    rig.settle("the window has the keyboard again", |rig| {
        rig.client(app).keyboard_focus() == Some(SurfaceKind::Window)
    });
    assert!(rig.status().keyboard_grab, "the input method lost the grab");
    let _ = ime;
}

/// A touch on the input method's candidate window goes to the input
/// method's client, like any touch on a surface. Nothing else happens: the
/// input method stays active and keeps the grab, the application gets no
/// touch.
#[test]
fn a_touch_on_the_candidate_window_goes_to_the_input_method() {
    let mut rig = Rig::new();
    rig.server.set_touch_available(true);
    let (ime, app, window) = rig.active();
    rig.client(ime).request_touch();
    rig.client(app).request_touch();
    rig.settle("both have a wl_touch", |rig| {
        rig.client(ime).has_touch() && rig.client(app).has_touch()
    });
    rig.client(ime)
        .ime_show_popup(100, 40, [0, 0, 255, 255])
        .unwrap();
    rig.settle("popup shown", |rig| {
        input_popup(&rig.server, window).is_some()
    });
    let popup = rig
        .server
        .window_surfaces(window)
        .into_iter()
        .find(|n| n.role == SurfaceRole::InputPopup)
        .unwrap()
        .surface;
    rig.client(app).clear_events();
    rig.server.touch_down(0, Some((popup, (30.0, 10.0))), 3000);
    rig.server.touch_frame();
    rig.server.touch_up(0, 3100);
    rig.server.touch_frame();
    rig.settle("the touch reaches the input method", |rig| {
        rig.client(ime)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TouchUp { id: 0 }))
    });
    assert!(rig.client(ime).events().contains(&ClientEvent::TouchDown {
        id: 0,
        surface: SurfaceKind::Other,
        x: 30.0,
        y: 10.0
    }));
    assert!(
        !rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TouchDown { .. })),
        "the application got the touch on the candidate window"
    );
    assert!(rig.client(ime).ime_active());
    assert!(rig.status().keyboard_grab);
}
