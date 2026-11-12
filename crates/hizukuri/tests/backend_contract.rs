//! The compositor core against the `Backend` trait, with a scripted backend.
//!
//! The headless backend never loses an output or its session. A DRM/KMS
//! session does both, so the paths the core has for them are exercised here
//! with a backend whose events the test writes by hand: outputs that appear,
//! change and disappear, frames that are only granted on request, and a
//! session that is paused and resumed.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use hizukuri::compositor::{Compositor, CompositorOptions};
use hizukuri::control::Request;
use hizukuri::scene::FlatScene;
use hizukuri_backend::{
    Backend, BackendError, BackendEvent, OutputId, OutputInfo, Presentation, monotonic_now,
};
use hizukuri_color::ImageDescription;
use hizukuri_render::{OutputFormat, TargetId, Targets};
use hizukuri_test_client::{ClientEvent, ClientOptions, TestClient};
use serde_json::Value;

#[derive(Default)]
struct ScriptedBackend {
    outputs: Vec<OutputInfo>,
    events: Vec<BackendEvent>,
    paused: bool,
    /// Outputs a frame was requested for and not yet granted.
    frame_requests: Vec<OutputId>,
    presents: u32,
}

impl ScriptedBackend {
    fn add_output(&mut self, info: OutputInfo) {
        self.outputs.push(info.clone());
        self.events.push(BackendEvent::OutputAdded(info));
    }

    fn change_output(&mut self, info: OutputInfo) {
        let slot = self.outputs.iter_mut().find(|o| o.id == info.id).unwrap();
        *slot = info.clone();
        self.events.push(BackendEvent::OutputChanged(info));
    }

    fn remove_output(&mut self, id: OutputId) {
        self.outputs.retain(|o| o.id != id);
        self.frame_requests.retain(|o| *o != id);
        self.events.push(BackendEvent::OutputRemoved(id));
    }

    /// Grants the requested frames, as a vblank would.
    fn vblank(&mut self) {
        for output in std::mem::take(&mut self.frame_requests) {
            self.events.push(BackendEvent::Frame { output });
        }
    }
}

impl Backend for ScriptedBackend {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.outputs.clone()
    }

    fn is_active(&self) -> bool {
        !self.paused
    }

    fn render_target(&mut self, _output: OutputId) -> Option<TargetId> {
        None
    }

    fn request_frame(&mut self, output: OutputId) {
        if !self.frame_requests.contains(&output) {
            self.frame_requests.push(output);
        }
    }

    fn present(&mut self, _renderer: Targets<'_>, output: OutputId) -> Result<(), BackendError> {
        if self.paused {
            return Err(BackendError::SessionInactive);
        }
        self.presents += 1;
        self.events.push(BackendEvent::Presented {
            output,
            presentation: Presentation {
                time: monotonic_now(),
                refresh: Duration::from_millis(16),
                sequence: self.presents as u64,
            },
        });
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<BackendEvent> {
        std::mem::take(&mut self.events)
    }
}

fn output(id: u64, name: &str, position: (i32, i32), scale: f64) -> OutputInfo {
    OutputInfo {
        id: OutputId(id),
        name: name.to_string(),
        make: "test".to_string(),
        model: "scripted".to_string(),
        size: (800, 600),
        scale,
        refresh_mhz: 60_000,
        position,
        image_description: ImageDescription::SRGB_GAMMA22,
        format: OutputFormat::Rgba8Unorm,
        supported_image_descriptions: vec![ImageDescription::SRGB_GAMMA22],
    }
}

struct Fixture {
    compositor: Compositor<ScriptedBackend>,
    client: TestClient,
}

impl Fixture {
    fn new(backend: ScriptedBackend) -> Self {
        let mut compositor = Compositor::new(
            CompositorOptions::default(),
            backend,
            Box::new(FlatScene::new(None)),
        )
        .unwrap();
        let (server_end, client_end) = UnixStream::pair().unwrap();
        compositor.wl().insert_client(server_end).unwrap();
        let client = TestClient::connect(
            client_end,
            ClientOptions {
                title: "w".into(),
                width: 100,
                height: 100,
                ..Default::default()
            },
        )
        .unwrap();
        Self { compositor, client }
    }

    /// One step without a vblank: requests are handled, no frame is granted.
    fn step(&mut self) {
        self.compositor.dispatch_clients().unwrap();
        self.compositor.process().unwrap();
        self.client.pump(None).unwrap();
    }

    fn steps(&mut self, count: usize) {
        for _ in 0..count {
            self.step();
        }
    }

    /// Steps with a vblank each time, until `done` holds.
    fn run_until(&mut self, what: &str, mut done: impl FnMut(&mut Fixture) -> bool) {
        for _ in 0..200 {
            self.compositor.backend().vblank();
            self.step();
            if done(self) {
                self.steps(4);
                return;
            }
        }
        panic!("did not settle: {what}\nstate: {:#}", self.state());
    }

    fn state(&mut self) -> Value {
        self.compositor.control(&Request::State)
    }

    fn count(&self, matches: impl Fn(&ClientEvent) -> bool) -> usize {
        self.client.events().iter().filter(|e| matches(e)).count()
    }
}

#[test]
fn outputs_can_appear_change_and_disappear_at_run_time() {
    // No output at all at startup, as on a machine with no monitor plugged in.
    let mut f = Fixture::new(ScriptedBackend::default());
    f.run_until("window mapped without an output", |f| f.client.is_drawn());
    let state = f.state();
    assert_eq!(state["outputs"].as_array().unwrap().len(), 0);
    assert_eq!(state["windows"][0]["output"], Value::Null);

    // First output: plugged in at an offset.
    f.compositor
        .backend()
        .add_output(output(1, "A", (100, 0), 1.0));
    f.steps(4);
    let state = f.state();
    assert_eq!(state["outputs"][0]["name"], "A");
    assert_eq!(state["outputs"][0]["position"], serde_json::json!([100, 0]));
    // Scale 1 is what a surface assumes anyway, so nothing is announced.
    assert_eq!(
        f.count(|e| matches!(e, ClientEvent::PreferredBufferScale { .. })),
        0
    );

    // Its scale changes; surfaces are told.
    f.compositor
        .backend()
        .change_output(output(1, "A", (100, 0), 2.0));
    f.steps(4);
    assert_eq!(f.state()["outputs"][0]["scale"], 2.0);
    assert_eq!(
        f.state()["outputs"][0]["logical_size"],
        serde_json::json!([400, 300])
    );
    assert!(
        f.client
            .events()
            .contains(&ClientEvent::PreferredBufferScale { scale: 2 })
    );

    // A second output, then the first one goes away: windows move over.
    f.compositor
        .backend()
        .add_output(output(2, "B", (500, 0), 1.0));
    f.steps(4);
    assert_eq!(f.state()["outputs"].as_array().unwrap().len(), 2);
    let window = f.state()["windows"][0]["id"].as_u64().unwrap();
    f.compositor.control(&Request::MoveWindow {
        window,
        x: 120,
        y: 40,
    });
    f.compositor.backend().remove_output(OutputId(1));
    f.steps(4);
    let state = f.state();
    let names: Vec<&str> = state["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["B"]);
    // Visual and logical positions still agree after the change.
    let w = &state["windows"][0];
    assert_eq!(
        w["visual"]["position"],
        serde_json::json!([
            w["position"][0].as_i64().unwrap() as f64,
            w["position"][1].as_i64().unwrap() as f64
        ])
    );

    // The last output goes away too; the compositor keeps running.
    f.compositor.backend().remove_output(OutputId(2));
    f.steps(4);
    assert_eq!(f.state()["outputs"].as_array().unwrap().len(), 0);
    assert_eq!(f.compositor.control(&Request::Ping)["ok"], true);
}

#[test]
fn frames_are_only_drawn_when_the_backend_grants_them() {
    let mut backend = ScriptedBackend::default();
    backend.add_output(output(1, "A", (0, 0), 1.0));
    let mut f = Fixture::new(backend);

    // Without a vblank the client can connect, be configured and draw, but
    // nothing is presented and no frame callback fires.
    f.steps(20);
    assert!(f.client.is_drawn());
    assert_eq!(f.compositor.backend().presents, 0);
    assert_eq!(f.count(|e| matches!(e, ClientEvent::FrameDone { .. })), 0);
    assert_eq!(f.count(|e| *e == ClientEvent::BufferReleased), 0);
    // The core asked for a frame, once.
    assert_eq!(f.compositor.backend().frame_requests, [OutputId(1)]);

    // The vblank comes: one frame, one release, one callback.
    f.compositor.backend().vblank();
    f.steps(6);
    assert_eq!(f.compositor.backend().presents, 1);
    assert_eq!(f.count(|e| matches!(e, ClientEvent::FrameDone { .. })), 1);
    assert_eq!(f.count(|e| *e == ClientEvent::BufferReleased), 1);

    // Nothing changed since: further vblanks draw nothing.
    for _ in 0..5 {
        f.compositor.backend().vblank();
        f.steps(2);
    }
    assert_eq!(f.compositor.backend().presents, 1);
    assert!(f.compositor.backend().frame_requests.is_empty());
}

#[test]
fn a_paused_session_is_not_drawn_to_and_catches_up_on_resume() {
    let mut backend = ScriptedBackend::default();
    backend.add_output(output(1, "A", (0, 0), 1.0));
    let mut f = Fixture::new(backend);
    // Wait for the client's first contents to be shown (the very first
    // frame is the empty output, before the client has drawn).
    f.run_until("first contents shown", |f| {
        f.count(|e| matches!(e, ClientEvent::FrameDone { .. })) == 1
    });
    let presents_before = f.compositor.backend().presents;
    f.client.clear_events();

    // VT switch away.
    f.compositor.backend().paused = true;
    f.compositor
        .backend()
        .events
        .push(BackendEvent::SessionPaused);
    f.steps(2);
    assert_eq!(f.state()["session_active"], false);

    // The client keeps drawing; even with vblanks nothing is presented and
    // the compositor does not fail.
    f.client.set_color([1, 2, 3, 255]);
    for _ in 0..10 {
        f.compositor.backend().vblank();
        f.step();
    }
    assert_eq!(f.compositor.backend().presents, presents_before);
    assert_eq!(f.count(|e| matches!(e, ClientEvent::FrameDone { .. })), 0);
    // The buffer is still held: nothing consumed it.
    assert_eq!(f.count(|e| *e == ClientEvent::BufferReleased), 0);

    // Back again: the pending change is drawn, once.
    f.compositor.backend().paused = false;
    f.compositor
        .backend()
        .events
        .push(BackendEvent::SessionResumed);
    f.run_until("frame after resume", |f| {
        f.compositor.backend().presents == presents_before + 1
    });
    assert_eq!(f.state()["session_active"], true);
    assert_eq!(f.count(|e| matches!(e, ClientEvent::FrameDone { .. })), 1);
    assert_eq!(f.count(|e| *e == ClientEvent::BufferReleased), 1);
}
