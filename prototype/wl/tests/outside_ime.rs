//! Text input for an input method outside the compositor: the protocol
//! layer acts as the seat's input method while no input method client is
//! bound, reports what the application commits on its text input, and
//! sends in what the input method outside composes. Test clients play the
//! applications, in the same thread as the server; the test plays the core
//! and the input method outside.

use std::os::unix::net::UnixStream;

use proto_test_client::{ClientEvent, ClientOptions, ImeEvent, InputMethodMode, TestClient};
use proto_wl::{OutsideCompose, ServerConfig, WindowId, WlServer};

struct Rig {
    server: WlServer,
    clients: Vec<Option<TestClient>>,
    time: u32,
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
        self.clients[index].as_mut().expect("connected")
    }

    fn step(&mut self) {
        self.server.dispatch_clients().unwrap();
        self.server.drain_events();
        self.time += 16;
        self.server.send_frame_callbacks(None, self.time);
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
        panic!("did not settle: {what}");
    }

    fn application(&mut self, title: &str) -> (usize, WindowId) {
        let client = self.add_client(ClientOptions {
            title: title.to_string(),
            width: 200,
            height: 100,
            text_input: true,
            ..Default::default()
        });
        self.settle("window mapped", |rig| {
            rig.client(client).is_drawn() && window(&rig.server, title).is_some()
        });
        (client, window(&self.server, title).unwrap())
    }
}

fn window(server: &WlServer, title: &str) -> Option<WindowId> {
    server.windows().into_iter().find(|id| {
        server
            .window(*id)
            .is_some_and(|w| w.title.as_deref() == Some(title) && w.mapped)
    })
}

#[test]
fn the_outside_input_method_gets_the_state_and_sends_text_in() {
    let mut rig = Rig::new();
    let (app, window) = rig.application("app");
    rig.server.set_keyboard_focus(Some(window));
    rig.steps_for_focus();
    assert!(
        !rig.client(app).text_input_entered(),
        "no input method: the text input is not entered"
    );

    rig.server.set_outside_input_method(true);
    assert!(rig.server.outside_input_method_serving());
    rig.settle("entered", |rig| rig.client(app).text_input_entered());
    rig.client(app)
        .text_input_enable(Some((40, 30, 2, 16)))
        .unwrap();
    rig.client(app)
        .text_input_set_surrounding("abc", 3, 3)
        .unwrap();
    rig.settle("the state", |rig| {
        rig.server
            .outside_text_input()
            .is_some_and(|t| t.content.surrounding.is_some())
    });
    let text_input = rig.server.outside_text_input().unwrap();
    assert_eq!(text_input.window, window);
    assert_eq!(text_input.content.cursor, Some((40, 30, 2, 16)));
    assert_eq!(
        text_input.content.surrounding,
        Some(("abc".to_string(), 3, 3))
    );
    let activation = text_input.activation;

    rig.client(app).clear_events();
    rig.server.outside_input_method_compose(&OutsideCompose {
        preedit: Some(("ka".to_string(), 2, 2)),
        commit: Some("か".to_string()),
        delete: Some((1, 0)),
        stale: false,
    });
    rig.settle("the text", |rig| {
        rig.client(app)
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::TextInputDone { .. }))
    });
    let events = rig.client(app).events().to_vec();
    for wanted in [
        ClientEvent::Preedit {
            text: "ka".to_string(),
            cursor_begin: 2,
            cursor_end: 2,
        },
        ClientEvent::CommitString {
            text: "か".to_string(),
        },
        ClientEvent::DeleteSurroundingText {
            before: 1,
            after: 0,
        },
    ] {
        assert!(events.contains(&wanted), "{wanted:?} in {events:?}");
    }
    let done = events.iter().find_map(|e| match e {
        ClientEvent::TextInputDone { serial, commits } => Some((*serial, *commits)),
        _ => None,
    });
    assert!(done.is_some_and(|(serial, commits)| serial == commits));

    // Disabled, then enabled again: another activation.
    rig.client(app).text_input_disable().unwrap();
    rig.settle("disabled", |rig| rig.server.outside_text_input().is_none());
    rig.client(app).text_input_enable(None).unwrap();
    rig.settle("enabled again", |rig| {
        rig.server.outside_text_input().is_some()
    });
    assert!(rig.server.outside_text_input().unwrap().activation > activation);

    // The input method outside goes: the application is left.
    rig.server.set_outside_input_method(false);
    rig.settle("left", |rig| !rig.client(app).text_input_entered());
    assert!(rig.server.outside_text_input().is_none());
}

#[test]
fn an_input_method_inside_takes_precedence() {
    let mut rig = Rig::new();
    let (app, window) = rig.application("app");
    rig.server.set_keyboard_focus(Some(window));
    rig.server.set_outside_input_method(true);
    rig.settle("entered", |rig| rig.client(app).text_input_entered());

    let inside = rig.add_client(ClientOptions {
        window: false,
        input_method: Some(InputMethodMode::Manual),
        ..Default::default()
    });
    rig.settle("the input method inside bound", |rig| {
        !rig.server.outside_input_method_serving()
    });
    // The application was entered again for the one inside.
    rig.settle("entered", |rig| rig.client(app).text_input_entered());
    rig.client(app)
        .text_input_enable(Some((1, 2, 3, 4)))
        .unwrap();
    rig.settle("the input method inside active", |rig| {
        rig.client(inside).ime_active()
    });
    assert!(rig.server.outside_text_input().is_none());
    assert!(
        rig.client(inside)
            .ime_events()
            .iter()
            .any(|e| matches!(e, ImeEvent::ContentType { .. }))
    );

    rig.clients[inside] = None;
    rig.settle("serving again", |rig| {
        rig.server.outside_input_method_serving()
    });
    rig.settle("entered", |rig| rig.client(app).text_input_entered());
}

impl Rig {
    fn steps_for_focus(&mut self) {
        for _ in 0..6 {
            self.step();
        }
    }
}
