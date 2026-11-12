//! The whole path from a client's buffer to what KMS scans out, without
//! hardware: the compositor core on the KMS backend, the KMS backend on the
//! mock device, and a real renderer.
//!
//! The mock keeps the memory behind framebuffers, so these tests look at
//! the pixels "on screen": through dumb buffers for the CPU copy path, and
//! through the mapped dma-buf for the dma-buf path where the driver allows
//! mapping (lavapipe does).
//!
//! Devices are chosen like in the renderer's tests: `HIZUKURI_TEST_DEVICES`
//! (default `lavapipe`); lavapipe runs in a child process with
//! `VK_DRIVER_FILES` set.

use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;

use hizukuri::compositor::{Compositor, CompositorOptions};
use hizukuri::control::Request;
use hizukuri::scene::{FlatScene, Scene};
use hizukuri::script::{ScriptOptions, ScriptRuntime, ScriptScene};
use hizukuri::selftest::SelfTest;
use hizukuri_backend::Backend;
use hizukuri_backend::kms::clock::ManualClock;
use hizukuri_backend::kms::input::{Hotkey, RawInput};
use hizukuri_backend::kms::mock::{Displayed, MockDevice, SnapshotBuilder};
use hizukuri_backend::kms::plan::ScanoutPreference;
use hizukuri_backend::kms::{ActiveScanout, KmsBackend, KmsConfig};
use hizukuri_render::{DeviceSelector, Renderer, RendererOptions, find_icd_manifest};
use hizukuri_test_client::{ClientEvent, ClientOptions, TestClient};

const CHILD_ENV: &str = "HIZUKURI_KMS_TEST_CHILD";
type MockBackend = KmsBackend<MockDevice, ManualClock>;

fn device_enabled(name: &str) -> bool {
    let list = std::env::var("HIZUKURI_TEST_DEVICES").unwrap_or_else(|_| "lavapipe".into());
    list.split(',').any(|d| d.trim() == name)
}

#[test]
fn gpu() {
    if !device_enabled("gpu") {
        eprintln!("skipped: gpu is not in HIZUKURI_TEST_DEVICES");
        return;
    }
    run_all(DeviceSelector::Gpu);
}

#[test]
fn lavapipe() {
    if !device_enabled("lavapipe") {
        eprintln!("skipped: lavapipe is not in HIZUKURI_TEST_DEVICES");
        return;
    }
    let manifest = find_icd_manifest("lvp").expect("the lavapipe ICD manifest is installed");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lavapipe_child", "--ignored", "--nocapture"])
        .env("VK_DRIVER_FILES", &manifest)
        .env(CHILD_ENV, "1")
        .output()
        .expect("re-executing the test binary");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "lavapipe child run failed");
}

#[test]
#[ignore = "started by the `lavapipe` test with VK_DRIVER_FILES set"]
fn lavapipe_child() {
    assert!(
        std::env::var(CHILD_ENV).is_ok(),
        "run the `lavapipe` test instead"
    );
    run_all(DeviceSelector::Cpu);
}

fn renderer(selector: &DeviceSelector) -> Renderer {
    Renderer::new(&RendererOptions {
        device: selector.clone(),
        validation: std::env::var("HIZUKURI_TEST_VALIDATION").is_ok_and(|v| v == "1"),
    })
    .expect("creating the renderer")
}

/// What draws: the Rust renderer, or the built-in script project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Draws {
    Rust,
    Script,
}

/// Whether the tests that need `/dev/udmabuf` run.
///
/// The kernel's udmabuf device turns system memory into a dma-buf. The
/// tests make real dma-bufs with it without a GPU, and lavapipe exports its
/// images as dma-bufs through it. These tests fail when the device cannot
/// be opened. Where it cannot be made available, set
/// `HIZUKURI_TEST_SKIP_UDMABUF=1`: the tests are then left out, with a
/// note.
fn udmabuf_wanted() -> bool {
    if std::env::var_os("HIZUKURI_TEST_SKIP_UDMABUF").is_some() {
        eprintln!("skipped: HIZUKURI_TEST_SKIP_UDMABUF is set");
        return false;
    }
    if let Err(error) = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/udmabuf")
    {
        panic!(
            "/dev/udmabuf cannot be opened ({error}). This test needs it; set \
             HIZUKURI_TEST_SKIP_UDMABUF=1 to run without the tests that do"
        );
    }
    true
}

fn run_all(selector: DeviceSelector) {
    for draws in [Draws::Rust, Draws::Script] {
        for scanout in [ScanoutPreference::CpuCopy, ScanoutPreference::Dmabuf] {
            // On lavapipe, the dma-buf path needs /dev/udmabuf.
            if scanout == ScanoutPreference::Dmabuf
                && selector == DeviceSelector::Cpu
                && !udmabuf_wanted()
            {
                continue;
            }
            a_client_window_reaches_the_screen(&selector, draws, scanout);
        }
        the_cursor_is_drawn_at_the_pointer(&selector, draws);
        a_vt_switch_keeps_clients_and_shows_them_again(&selector, draws);
    }
    the_self_test_passes_on_the_mock(&selector);
}

/// The script scene on the device this process sees first (the hardware
/// device, or lavapipe in the child process).
fn script_scene() -> ScriptScene {
    let mut options = ScriptOptions::default();
    options.gpu.validation = std::env::var("HIZUKURI_TEST_VALIDATION").is_ok_and(|v| v == "1");
    ScriptScene::new(ScriptRuntime::new(options).expect("starting the script"))
}

struct Rig {
    compositor: Compositor<MockBackend>,
    device: MockDevice,
    clients: Vec<TestClient>,
}

impl Rig {
    fn new(
        selector: &DeviceSelector,
        draws: Draws,
        scanout: ScanoutPreference,
        draw_cursor: bool,
    ) -> Rig {
        let clock = ManualClock::new(Duration::from_secs(100));
        let device = MockDevice::new(
            SnapshotBuilder::new("mock")
                .connector("Virtual-1", true, &[(320, 240, 60, true)])
                .build(),
            clock.clone(),
        );
        let config = KmsConfig {
            scanout,
            ..Default::default()
        };
        // For the dma-buf path, pretend the GPU that draws owns the (mock)
        // display.
        let on_kms_device = scanout == ScanoutPreference::Dmabuf;
        let options = CompositorOptions {
            draw_cursor,
            ..Default::default()
        };
        let compositor = match draws {
            Draws::Rust => {
                let mut renderer = renderer(selector);
                let backend = KmsBackend::new(
                    device.clone(),
                    clock,
                    config,
                    hizukuri_render::targets_of(Some(&mut renderer)),
                    on_kms_device,
                )
                .expect("creating the KMS backend");
                Compositor::new(options, backend, Box::new(FlatScene::new(Some(renderer)))).unwrap()
            }
            Draws::Script => {
                let mut scene = script_scene();
                let backend = KmsBackend::new(
                    device.clone(),
                    clock,
                    config,
                    scene.targets(),
                    on_kms_device,
                )
                .expect("creating the KMS backend");
                Compositor::with_window_manager(
                    options,
                    backend,
                    Box::new(scene),
                    Box::new(hizukuri_layout::Layout::new()),
                )
                .unwrap()
            }
        };
        Rig {
            compositor,
            device,
            clients: Vec::new(),
        }
    }

    fn add_client(&mut self, options: ClientOptions) -> usize {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.compositor.wl().insert_client(server_end).unwrap();
        self.clients
            .push(TestClient::connect(client_end, options).unwrap());
        self.clients.len() - 1
    }

    /// One turn: clients, compositor, then a vblank and what it brings.
    fn step(&mut self) {
        for client in &mut self.clients {
            client.pump(None).unwrap();
        }
        self.compositor.dispatch_clients().unwrap();
        self.device.vblank();
        self.compositor.backend().handle_device_events().unwrap();
        self.compositor.process().unwrap();
    }

    fn settle(&mut self, what: &str, mut done: impl FnMut(&mut Rig) -> bool) {
        for _ in 0..300 {
            self.step();
            if done(self) {
                for _ in 0..6 {
                    self.step();
                }
                return;
            }
        }
        panic!(
            "did not settle: {what}\n{:#}",
            self.compositor.control(&Request::State)
        );
    }

    fn crtc(&mut self) -> u32 {
        self.compositor.backend().status().output.unwrap().crtc
    }

    fn screen(&mut self) -> Displayed {
        let crtc = self.crtc();
        self.device.displayed(crtc).expect("something is displayed")
    }

    fn control(&mut self, request: Request) {
        let response = self.compositor.control(&request);
        assert_eq!(response["ok"], true, "{request:?}: {response}");
    }
}

fn close(actual: [u8; 3], expected: [u8; 3], tolerance: i32) -> bool {
    (0..3).all(|i| (actual[i] as i32 - expected[i] as i32).abs() <= tolerance)
}

/// A client's solid window shows up in the framebuffer KMS is scanning
/// out, at the right place, on both scanout paths.
fn a_client_window_reaches_the_screen(
    selector: &DeviceSelector,
    draws: Draws,
    scanout: ScanoutPreference,
) {
    let mut rig = Rig::new(selector, draws, scanout, false);
    let expected_path = match scanout {
        ScanoutPreference::Dmabuf => ActiveScanout::Dmabuf,
        _ => ActiveScanout::CpuCopy,
    };
    assert_eq!(
        rig.compositor.backend().status().output.unwrap().scanout,
        expected_path
    );

    let client = rig.add_client(ClientOptions {
        title: "on-screen".into(),
        width: 100,
        height: 60,
        color: [200, 100, 50, 255],
        ..Default::default()
    });
    rig.settle("the window is shown and its frame callback fired", |rig| {
        rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::FrameDone { .. }))
    });
    let state = rig.compositor.control(&Request::State);
    let window = state["windows"][0]["id"].as_u64().unwrap();
    rig.control(Request::MoveWindow {
        window,
        x: 40,
        y: 30,
    });
    rig.settle("the moved window is on screen", |rig| {
        rig.compositor
            .backend()
            .status()
            .output
            .unwrap()
            .frames_presented
            >= 4
    });

    // The backend's own account of itself is in the state dump.
    let state = rig.compositor.control(&Request::State);
    assert_eq!(state["backend"], "kms");
    assert_eq!(state["backend_status"]["output"]["connector"], "Virtual-1");
    assert_eq!(
        state["backend_status"]["output"]["scanout"],
        expected_path.name()
    );
    assert_eq!(state["backend_status"]["failed_commits"], 0);
    let errors = &state["renderer"]["validation_errors"];
    assert!(
        errors.is_null() || errors == 0,
        "validation errors: {errors}"
    );
    assert_eq!(state["script"].is_object(), draws == Draws::Script);
    assert!(state["frame_time_ms"]["frames"].as_u64().unwrap() >= 4);
    assert!(state["frame_time_ms"]["total"]["median"].as_f64().unwrap() > 0.0);
    assert_eq!(state["outputs"][0]["format"], "bgra8");

    let screen = rig.screen();
    assert_eq!((screen.width, screen.height), (320, 240));
    match &screen.pixels {
        Some(_) => {
            // Inside the window (corners and center), within the tolerance
            // of the 16 bit float compositing buffer.
            for (x, y) in [(40, 30), (139, 89), (90, 60)] {
                let pixel = screen.rgb(x, y);
                assert!(close(pixel, [200, 100, 50], 3), "({x}, {y}) is {pixel:?}");
            }
            // Outside: the background.
            for (x, y) in [(39, 30), (140, 89), (0, 0), (319, 239)] {
                assert_eq!(screen.rgb(x, y), [0x20, 0x20, 0x20], "({x}, {y})");
            }
            println!("{draws:?} {scanout:?}: pixels on the (mock) screen checked");
        }
        None => {
            // A dma-buf this driver does not let anyone map: the screen
            // cannot be read here, but the frame was accepted and flipped.
            assert_eq!(scanout, ScanoutPreference::Dmabuf);
            println!("{draws:?} {scanout:?}: the scanout dma-buf cannot be mapped on this device");
        }
    }
    assert!(
        rig.device.rejected().is_empty(),
        "{:?}",
        rig.device.rejected()
    );

    // A capture through the control interface shows the same thing.
    let dir = std::env::temp_dir().join(format!("hzk-kms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{scanout:?}.png"));
    rig.control(Request::Screenshot {
        path: path.clone(),
        composite_path: None,
        output: None,
    });
    let image = hizukuri_render::EncodedImage::read_png(
        &path,
        hizukuri_color::ImageDescription::SRGB_GAMMA22,
    )
    .unwrap();
    let pixel = image.pixel(90, 60);
    assert!(
        close(
            [pixel[0] as u8, pixel[1] as u8, pixel[2] as u8],
            [200, 100, 50],
            3
        ),
        "capture: {pixel:?}"
    );
    assert_eq!(image.pixel(5, 5)[..3], [0x20, 0x20, 0x20]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// With cursor drawing on, the arrow appears with its tip at the pointer
/// and follows it.
fn the_cursor_is_drawn_at_the_pointer(selector: &DeviceSelector, draws: Draws) {
    let mut rig = Rig::new(selector, draws, ScanoutPreference::CpuCopy, true);
    rig.settle("first frame", |rig| {
        rig.compositor
            .backend()
            .status()
            .output
            .unwrap()
            .frames_presented
            >= 1
    });
    // No pointer yet: nothing but background.
    assert_eq!(rig.screen().rgb(100, 80), [0x20, 0x20, 0x20]);

    for (x, y) in [(100.0, 80.0), (200.0, 150.0)] {
        let before = rig
            .compositor
            .backend()
            .status()
            .output
            .unwrap()
            .frames_presented;
        rig.control(Request::PointerMotion { x, y });
        rig.settle("the cursor frame is shown", |rig| {
            rig.compositor
                .backend()
                .status()
                .output
                .unwrap()
                .frames_presented
                > before
        });
        let screen = rig.screen();
        let (x, y) = (x as u32, y as u32);
        // The tip (top left) is outline, the pixel below and right of it
        // is the white inside, and left of the arrow is background.
        assert_eq!(screen.rgb(x, y), [0, 0, 0], "tip at ({x}, {y})");
        assert_eq!(screen.rgb(x + 1, y + 3), [255, 255, 255], "inside");
        assert_eq!(
            screen.rgb(x + 11, y),
            [0x20, 0x20, 0x20],
            "right of the tip"
        );
        assert_eq!(screen.rgb(x - 1, y), [0x20, 0x20, 0x20], "left of the tip");
    }
    // The first position is clean again.
    assert_eq!(rig.screen().rgb(100, 80), [0x20, 0x20, 0x20]);
}

/// The session is paused and resumed (a VT switch): the client stays
/// connected, nothing is committed while away, and the window is back on
/// screen afterwards, including what it drew in the meantime.
fn a_vt_switch_keeps_clients_and_shows_them_again(selector: &DeviceSelector, draws: Draws) {
    let mut rig = Rig::new(selector, draws, ScanoutPreference::CpuCopy, false);
    let client = rig.add_client(ClientOptions {
        title: "survivor".into(),
        width: 80,
        height: 40,
        color: [10, 200, 10, 255],
        ..Default::default()
    });
    rig.settle("window shown", |rig| {
        rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::FrameDone { .. }))
    });
    // The first slot of the cascade is (32, 32).
    assert!(close(rig.screen().rgb(40, 40), [10, 200, 10], 3));

    // Away, the way a person does it: Ctrl+Alt+F2. The backend takes the
    // function key as a hotkey; Ctrl and Alt are still down when the
    // session is paused, and their releases are never seen.
    let raw_key = |code: u32, pressed: bool| RawInput::Key {
        code,
        pressed,
        time_us: 1,
    };
    const KEY_LEFTCTRL: u32 = 29;
    const KEY_LEFTALT: u32 = 56;
    const KEY_F2: u32 = 60;
    for code in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_F2] {
        rig.compositor.backend().push_input(raw_key(code, true));
    }
    assert_eq!(
        rig.compositor.backend().take_hotkeys(),
        [Hotkey::SwitchVt(2)]
    );
    rig.step();
    rig.device.set_master(false);
    rig.compositor.backend().session_paused();
    rig.compositor.process().unwrap();
    let commits = rig.device.commits().len();
    rig.clients[client].set_color([200, 10, 200, 255]);
    for _ in 0..20 {
        rig.step();
    }
    assert_eq!(
        rig.device.commits().len(),
        commits,
        "committed while paused"
    );
    assert!(
        rig.device.rejected().is_empty(),
        "tried to commit while paused"
    );
    let state = rig.compositor.control(&Request::State);
    assert_eq!(state["session_active"], false);
    assert_eq!(state["windows"].as_array().unwrap().len(), 1);

    // And back.
    rig.device.set_master(true);
    rig.compositor.backend().session_resumed();
    rig.settle("the window is back with its new color", |rig| {
        rig.screen().pixels.is_some() && close(rig.screen().rgb(40, 40), [200, 10, 200], 3)
    });
    let after = rig.device.commits();
    assert!(
        after[commits].flags.allow_modeset,
        "resume starts with a modeset"
    );
    assert_eq!(
        rig.compositor.control(&Request::State)["session_active"],
        true
    );
    // Input works again too, and Ctrl and Alt are not stuck: a plain "a"
    // arrives as "a".
    rig.clients[client].clear_events();
    rig.compositor.backend().push_input(raw_key(30, true));
    rig.compositor.backend().push_input(raw_key(30, false));
    rig.settle("key delivered", |rig| {
        rig.clients[client]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::Key { keycode: 30, .. }))
    });
    let text = rig.clients[client]
        .events()
        .iter()
        .find_map(|e| match e {
            ClientEvent::Key {
                keycode: 30,
                pressed: true,
                text,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .expect("the press arrived");
    assert_eq!(text, "a", "modifiers are stuck after the VT switch");
}

/// The self test, driven by the mock instead of a display: it draws its
/// pattern, sees the flips, and reports what a run on hardware would.
fn the_self_test_passes_on_the_mock(selector: &DeviceSelector) {
    for with_renderer in [true, false] {
        let clock = ManualClock::new(Duration::from_secs(100));
        let device = MockDevice::new(
            SnapshotBuilder::new("mock")
                .connector("Virtual-1", true, &[(640, 480, 60, true)])
                .build(),
            clock.clone(),
        );
        let mut renderer = with_renderer.then(|| renderer(selector));
        let mut backend = KmsBackend::new(
            device.clone(),
            clock,
            KmsConfig::default(),
            hizukuri_render::targets_of(renderer.as_mut()),
            false,
        )
        .unwrap();
        let mut test = SelfTest::new();
        test.process(&mut backend, renderer.as_mut());
        for _ in 0..180 {
            device.vblank();
            backend.handle_device_events().unwrap();
            test.process(&mut backend, renderer.as_mut());
        }
        assert!(
            test.frames_presented() >= 170,
            "{}",
            test.frames_presented()
        );

        // What is on the (mock) screen is the pattern.
        let crtc = backend.status().output.unwrap().crtc;
        let screen = device.displayed(crtc).unwrap();
        if with_renderer {
            // White bar at the top left, the blue bar second from the
            // right, background at the bottom.
            assert!(close(screen.rgb(40, 100), [255, 255, 255], 1));
            assert!(close(screen.rgb(6 * 80 + 40, 100), [0, 0, 255], 4));
            assert!(close(screen.rgb(639, 479), [32, 32, 32], 1));
        } else {
            assert_eq!(screen.rgb(40, 100), [0, 0, 0]);
        }

        let report = test.report(&backend, serde_json::json!({ "test": true }));
        assert_eq!(report["passed"], true, "{report:#}");
        let status = |name: &str| {
            report["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == name)
                .unwrap_or_else(|| panic!("no check {name}"))["status"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(status("modeset"), "pass");
        assert_eq!(status("scanout_path"), "pass");
        assert_eq!(status("frames_presented"), "pass");
        assert_eq!(status("page_flip_timing"), "pass");
        assert_eq!(status("commits"), "pass");
        assert_eq!(
            status("frame_contents"),
            if with_renderer { "pass" } else { "skipped" }
        );
        for later in ["hdr_output", "variable_refresh", "tearing"] {
            assert_eq!(status(later), "not_implemented");
        }
        // Frames were timed (the status depends on the machine's speed).
        assert_ne!(status("frame_time"), "fail");
        assert!(report["frame_time_ms"]["total"]["median"].as_f64().unwrap() >= 0.0);
        assert_eq!(report["context"]["test"], true);
        assert_eq!(
            report["backend"]["output"]["mode"],
            serde_json::json!([640, 480])
        );

        Backend::shutdown(&mut backend, hizukuri_render::targets_of(renderer.as_mut()));
        assert_eq!(device.framebuffer_count(), 0);
        if let Some(renderer) = &renderer
            && let Some(errors) = renderer.validation_errors()
        {
            assert_eq!(errors, 0, "Vulkan validation errors");
        }
    }
}
