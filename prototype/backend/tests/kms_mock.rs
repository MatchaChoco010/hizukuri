//! The KMS backend on the mock device: modes, flips, timing, the session
//! going away and coming back, displays being unplugged, failing commits,
//! emergency keys. No hardware, no VT, no VM, and no renderer: the output
//! shows black frames, which is all these tests need.

use std::time::Duration;

use proto_backend::kms::clock::{Clock, ManualClock};
use proto_backend::kms::input::{Hotkey, RawInput};
use proto_backend::kms::mock::{MockDevice, SnapshotBuilder};
use proto_backend::kms::model::{DeviceSnapshot, FOURCC_XRGB8888};
use proto_backend::kms::plan::{ModeRequest, OutputRequest, ScanoutPreference};
use proto_backend::kms::{ActiveScanout, KmsBackend, KmsConfig};
use proto_backend::{
    Backend, BackendError, BackendEvent, InputEvent, OutputId, OutputInfo, Presentation,
};

type TestBackend = KmsBackend<MockDevice, ManualClock>;

struct Rig {
    backend: TestBackend,
    device: MockDevice,
    clock: ManualClock,
}

fn rig_with(snapshot: DeviceSnapshot, config: KmsConfig) -> Result<Rig, BackendError> {
    let clock = ManualClock::new(Duration::from_secs(1000));
    let device = MockDevice::new(snapshot, clock.clone());
    let backend = KmsBackend::new(device.clone(), clock.clone(), config, None, false)?;
    Ok(Rig {
        backend,
        device,
        clock,
    })
}

fn one_display() -> DeviceSnapshot {
    SnapshotBuilder::new("mock")
        .crtcs(2)
        .connector(
            "DP-1",
            true,
            &[(1920, 1080, 60, true), (1280, 720, 60, false)],
        )
        .connector("HDMI-A-1", false, &[])
        .build()
}

fn rig() -> Rig {
    rig_with(one_display(), KmsConfig::default()).unwrap()
}

impl Rig {
    fn events(&mut self) -> Vec<BackendEvent> {
        self.backend.handle_device_events().unwrap();
        self.backend.drain_events()
    }

    /// The output announced at startup.
    fn output(&mut self) -> OutputInfo {
        match self.events().as_slice() {
            [BackendEvent::OutputAdded(info)] => info.clone(),
            other => panic!("expected one OutputAdded, got {other:?}"),
        }
    }

    /// Asks for a frame, expects it to be granted, and presents it.
    fn draw(&mut self, output: OutputId) {
        self.backend.request_frame(output);
        assert_eq!(
            self.events(),
            [BackendEvent::Frame { output }],
            "frame not granted"
        );
        self.backend.present(None, output).unwrap();
    }

    /// Lets the display refresh and returns the presentation it reports.
    fn vblank(&mut self, output: OutputId) -> Presentation {
        self.device.vblank();
        match self.events().as_slice() {
            [
                BackendEvent::Presented {
                    output: reported,
                    presentation,
                },
            ] if *reported == output => *presentation,
            other => panic!("expected one Presented, got {other:?}"),
        }
    }

    /// Like `draw`, for a frame whose commit the device will refuse: the
    /// frame is granted again right away.
    fn draw_expecting_failure(&mut self, output: OutputId) {
        self.backend.request_frame(output);
        assert_eq!(self.events(), [BackendEvent::Frame { output }]);
        self.backend.present(None, output).unwrap();
        assert_eq!(self.events(), [BackendEvent::Frame { output }]);
    }

    fn crtc(&self) -> u32 {
        self.backend.status().output.expect("an output").crtc
    }
}

#[test]
fn startup_announces_the_output_without_touching_the_display() {
    let mut rig = rig();
    let output = rig.output();
    assert_eq!(output.name, "DP-1");
    assert_eq!(output.size, (1920, 1080));
    assert_eq!(
        output.refresh_mhz,
        rig.backend.status().output.unwrap().refresh_mhz
    );
    assert!((output.refresh_mhz as i64 - 60_000).abs() < 100);
    assert_eq!(output.format.name(), "bgra8");
    assert_eq!(rig.backend.outputs(), [output]);
    assert!(rig.backend.is_active());
    assert_eq!(rig.backend.name(), "kms");
    // Nothing is committed until there is a frame to show.
    assert!(rig.device.commits().is_empty());
    let status = rig.backend.status();
    assert_eq!(status.driver, "mock");
    assert_eq!(status.output.unwrap().scanout, ActiveScanout::CpuCopy);
    // Without a renderer there is nothing to render into.
    assert_eq!(rig.backend.render_target(OutputId(1)), None);
}

#[test]
fn first_frame_is_a_modeset_and_later_frames_are_flips() {
    let mut rig = rig();
    let output = rig.output().id;
    let crtc = rig.crtc();

    rig.draw(output);
    let commits = rig.device.commits();
    assert_eq!(commits.len(), 1);
    assert!(commits[0].flags.allow_modeset && !commits[0].flags.nonblock);
    assert_eq!(rig.device.active_mode(crtc).unwrap().size(), (1920, 1080));
    assert_eq!(rig.device.connector_crtc("DP-1"), Some(crtc));
    // Only DP-1 was touched.
    assert_eq!(rig.device.connector_crtc("HDMI-A-1"), None);
    let first = rig.device.displayed(crtc).unwrap();
    assert_eq!(
        (first.width, first.height, first.format),
        (1920, 1080, FOURCC_XRGB8888)
    );
    // No renderer: black.
    assert_eq!(first.rgb(0, 0), [0, 0, 0]);
    assert_eq!(first.rgb(1919, 1079), [0, 0, 0]);

    // The blocking modeset reports its presentation at once.
    let events = rig.events();
    let [BackendEvent::Presented { presentation, .. }] = events.as_slice() else {
        panic!("expected Presented, got {events:?}");
    };
    assert_eq!(presentation.time, Duration::from_secs(1000));

    // The second frame is a plain flip to the other buffer.
    rig.draw(output);
    let commits = rig.device.commits();
    assert_eq!(commits.len(), 2);
    assert!(!commits[1].flags.allow_modeset && commits[1].flags.nonblock);
    assert_eq!(commits[1].request.changes.len(), 1);
    let second = rig.device.displayed(crtc).unwrap();
    assert_ne!(second.framebuffer, first.framebuffer);

    // The third frame goes back to the first buffer.
    rig.vblank(output);
    rig.draw(output);
    assert_eq!(
        rig.device.displayed(crtc).unwrap().framebuffer,
        first.framebuffer
    );
    assert!(rig.device.rejected().is_empty());
}

#[test]
fn frames_are_paced_by_page_flips_and_timestamped_by_the_display() {
    let mut rig = rig();
    let output = rig.output().id;
    rig.draw(output);
    rig.events();
    rig.draw(output);

    // A flip is in flight: a new frame has to wait.
    rig.backend.request_frame(output);
    rig.backend.request_frame(output);
    assert!(rig.events().is_empty());

    // The vblank completes the flip and grants the waiting frame, once.
    let before = rig.clock.now();
    rig.device.vblank();
    let events = rig.events();
    let [
        BackendEvent::Presented { presentation, .. },
        BackendEvent::Frame { output: granted },
    ] = events.as_slice()
    else {
        panic!("expected Presented then Frame, got {events:?}");
    };
    assert_eq!(*granted, output);
    let period = presentation.time - before;
    assert!(
        (period.as_micros() as i64 - 16_667).abs() < 100,
        "{period:?}"
    );
    assert_eq!(presentation.refresh, period);
    assert_eq!(presentation.sequence, 1);

    // The granted frame is presented. Presenting again before its flip
    // completes is out of turn (the core does this for a verification
    // capture): it is not an error, does not reach the device, and the
    // frame is granted again at the flip.
    rig.backend.present(None, output).unwrap();
    let commits = rig.device.commits().len();
    rig.backend.present(None, output).unwrap();
    assert_eq!(rig.device.commits().len(), commits);
    assert!(rig.device.rejected().is_empty());
    rig.device.vblank();
    assert!(matches!(
        rig.events().as_slice(),
        [BackendEvent::Presented { .. }, BackendEvent::Frame { .. }]
    ));
    rig.backend.present(None, output).unwrap();
    rig.vblank(output);

    // Sixty more frames: the statistics see a steady 60 Hz.
    for _ in 0..60 {
        rig.draw(output);
        rig.vblank(output);
    }
    let stats = rig.backend.status().output.unwrap();
    assert!(stats.frames_presented >= 60);
    let flips = stats.flip_statistics;
    assert!((flips.median_ms - 16.667).abs() < 0.01, "{flips:?}");
    assert_eq!(flips.late, 0);
}

#[test]
fn the_requested_connector_and_mode_are_used() {
    let snapshot = SnapshotBuilder::new("mock")
        .crtcs(2)
        .connector("DP-1", true, &[(1920, 1080, 60, true)])
        .connector(
            "DP-2",
            true,
            &[
                (2560, 1440, 144, true),
                (1280, 720, 60, false),
                (1280, 720, 120, false),
            ],
        )
        .build();
    let config = KmsConfig {
        output: OutputRequest {
            connector: Some("DP-2".into()),
            mode: Some(ModeRequest {
                width: 1280,
                height: 720,
                refresh_mhz: Some(120_000),
            }),
        },
        scale: 2.0,
        ..Default::default()
    };
    let mut rig = rig_with(snapshot, config).unwrap();
    let output = rig.output();
    assert_eq!(output.name, "DP-2");
    assert_eq!(output.size, (1280, 720));
    assert_eq!(output.logical_size(), (640, 360));
    assert!((output.refresh_mhz as i64 - 120_000).abs() < 200);
    rig.draw(output.id);
    // DP-1 is left alone.
    assert_eq!(rig.device.connector_crtc("DP-1"), None);
    assert!(rig.device.connector_crtc("DP-2").is_some());
}

#[test]
fn a_connector_that_does_not_exist_is_a_startup_error() {
    let config = KmsConfig {
        output: OutputRequest {
            connector: Some("VGA-9".into()),
            mode: None,
        },
        ..Default::default()
    };
    let error = rig_with(one_display(), config).err().expect("must fail");
    assert!(error.to_string().contains("VGA-9"), "{error}");
    assert!(error.to_string().contains("DP-1"), "{error}");
}

#[test]
fn a_display_can_appear_after_startup() {
    let snapshot = SnapshotBuilder::new("mock")
        .crtcs(1)
        .connector("DP-1", false, &[(800, 600, 60, true)])
        .build();
    let mut rig = rig_with(snapshot, KmsConfig::default()).unwrap();
    assert!(rig.events().is_empty());
    assert!(rig.backend.outputs().is_empty());
    assert_eq!(rig.backend.status().output, None);
    // Nothing to draw on: requests are ignored, presenting is an error.
    rig.backend.request_frame(OutputId(1));
    assert!(rig.events().is_empty());
    assert!(matches!(
        rig.backend.present(None, OutputId(1)),
        Err(BackendError::UnknownOutput)
    ));

    rig.device.set_connected("DP-1", true);
    rig.backend.displays_changed();
    assert_eq!(rig.events(), [BackendEvent::Reconfigure]);
    rig.backend.reconfigure(None).unwrap();
    let output = rig.output();
    assert_eq!((output.name.as_str(), output.size), ("DP-1", (800, 600)));
    rig.draw(output.id);
    assert_eq!(
        rig.device.active_mode(rig.crtc()).unwrap().size(),
        (800, 600)
    );
}

#[test]
fn unplugging_removes_the_output_and_frees_its_buffers() {
    let mut rig = rig();
    let output = rig.output().id;
    let crtc = rig.crtc();
    rig.draw(output);
    rig.events();
    assert_eq!(rig.device.framebuffer_count(), 2);
    assert_eq!(rig.device.dumb_buffer_count(), 2);

    rig.device.set_connected("DP-1", false);
    rig.backend.displays_changed();
    // A second notification before the core reacts changes nothing.
    rig.backend.displays_changed();
    assert_eq!(
        rig.events(),
        [BackendEvent::Reconfigure, BackendEvent::Reconfigure]
    );
    rig.backend.reconfigure(None).unwrap();
    rig.backend.reconfigure(None).unwrap();
    assert_eq!(rig.events(), [BackendEvent::OutputRemoved(output)]);
    assert!(rig.backend.outputs().is_empty());
    // The output was turned off and nothing is left behind.
    assert_eq!(rig.device.active_mode(crtc), None);
    assert_eq!(rig.device.connector_crtc("DP-1"), None);
    assert_eq!(rig.device.framebuffer_count(), 0);
    assert_eq!(rig.device.dumb_buffer_count(), 0);
    // The removed output can no longer be drawn to.
    rig.backend.request_frame(output);
    assert!(rig.events().is_empty());

    // Plugged back: a new output with a new id.
    rig.device.set_connected("DP-1", true);
    rig.backend.displays_changed();
    rig.events();
    rig.backend.reconfigure(None).unwrap();
    let again = rig.output();
    assert_ne!(again.id, output);
    rig.draw(again.id);
    assert_eq!(rig.device.active_mode(crtc).unwrap().size(), (1920, 1080));
}

#[test]
fn another_display_takes_over_when_the_first_is_unplugged() {
    let snapshot = SnapshotBuilder::new("mock")
        .crtcs(2)
        .connector("DP-1", true, &[(1920, 1080, 60, true)])
        .connector("DP-2", true, &[(1024, 768, 60, true)])
        .build();
    let mut rig = rig_with(snapshot, KmsConfig::default()).unwrap();
    let first = rig.output();
    assert_eq!(first.name, "DP-1");
    rig.draw(first.id);
    rig.events();

    rig.device.set_connected("DP-1", false);
    rig.backend.displays_changed();
    rig.events();
    rig.backend.reconfigure(None).unwrap();
    let events = rig.events();
    let [
        BackendEvent::OutputRemoved(removed),
        BackendEvent::OutputAdded(second),
    ] = events.as_slice()
    else {
        panic!("expected removal then addition, got {events:?}");
    };
    assert_eq!(*removed, first.id);
    assert_eq!((second.name.as_str(), second.size), ("DP-2", (1024, 768)));
}

#[test]
fn a_paused_session_is_left_alone_and_resumes_with_a_modeset() {
    let mut rig = rig();
    let output = rig.output().id;
    let crtc = rig.crtc();
    rig.draw(output);
    rig.events();
    rig.draw(output); // a flip is in flight

    // The VT is switched away: master is revoked, then we are told.
    rig.device.set_master(false);
    rig.backend.session_paused();
    rig.backend.session_paused(); // told twice: once is enough
    assert_eq!(rig.events(), [BackendEvent::SessionPaused]);
    assert!(!rig.backend.is_active());
    assert!(!rig.backend.status().session_active);
    assert_eq!(rig.backend.render_target(output), None);
    assert!(matches!(
        rig.backend.present(None, output),
        Err(BackendError::SessionInactive)
    ));
    // Frames asked for while away are remembered, not granted.
    rig.backend.request_frame(output);
    rig.device.vblank();
    assert!(rig.events().is_empty());
    let commits = rig.device.commits().len();
    assert!(rig.device.rejected().is_empty(), "nothing was even tried");

    // Back again.
    rig.device.set_master(true);
    rig.backend.session_resumed();
    rig.backend.session_resumed();
    assert_eq!(
        rig.events(),
        [
            BackendEvent::Reconfigure,
            BackendEvent::SessionResumed,
            BackendEvent::Frame { output }
        ]
    );
    rig.backend.reconfigure(None).unwrap();
    assert!(rig.events().is_empty(), "the display is still there");
    rig.backend.present(None, output).unwrap();
    // The first commit after resuming is a full modeset.
    let all = rig.device.commits();
    assert_eq!(all.len(), commits + 1);
    assert!(all.last().unwrap().flags.allow_modeset);
    assert_eq!(rig.device.active_mode(crtc).unwrap().size(), (1920, 1080));
    assert!(matches!(
        rig.events().as_slice(),
        [BackendEvent::Presented { .. }]
    ));
    // And then flips again.
    rig.draw(output);
    assert!(!rig.device.commits().last().unwrap().flags.allow_modeset);
}

#[test]
fn a_display_unplugged_while_paused_is_noticed_on_resume() {
    let mut rig = rig();
    let output = rig.output().id;
    rig.draw(output);
    rig.events();

    rig.device.set_master(false);
    rig.backend.session_paused();
    rig.device.set_connected("DP-1", false);
    rig.backend.displays_changed();
    rig.events();
    // While paused the device is not touched.
    rig.backend.reconfigure(None).unwrap();
    assert!(rig.events().is_empty());
    assert_eq!(rig.backend.outputs().len(), 1);

    rig.device.set_master(true);
    rig.backend.session_resumed();
    let events = rig.events();
    assert!(events.contains(&BackendEvent::Reconfigure));
    rig.backend.reconfigure(None).unwrap();
    assert_eq!(rig.events(), [BackendEvent::OutputRemoved(output)]);
}

#[test]
fn losing_master_before_being_told_is_survived() {
    let mut rig = rig();
    let output = rig.output().id;
    rig.draw(output);
    rig.events();

    // Master is gone but the pause notification has not arrived yet.
    rig.device.set_master(false);
    rig.backend.request_frame(output);
    assert_eq!(rig.events(), [BackendEvent::Frame { output }]);
    rig.backend.present(None, output).unwrap();
    assert_eq!(rig.device.rejected().len(), 1);
    assert_eq!(rig.backend.status().failed_commits, 1);

    rig.backend.session_paused();
    rig.device.set_master(true);
    rig.backend.session_resumed();
    // The frame that could not be shown is granted again.
    assert!(rig.events().contains(&BackendEvent::Frame { output }));
    rig.backend.present(None, output).unwrap();
    assert!(rig.device.commits().last().unwrap().flags.allow_modeset);
}

#[test]
fn a_failed_flip_is_retried_as_a_modeset() {
    let mut rig = rig();
    let output = rig.output().id;
    rig.draw(output);
    rig.events();

    rig.device.fail_next_commit(22);
    rig.backend.request_frame(output);
    rig.events();
    rig.backend.present(None, output).unwrap();
    // The frame is asked for again right away.
    assert_eq!(rig.events(), [BackendEvent::Frame { output }]);
    rig.backend.present(None, output).unwrap();
    assert!(rig.device.commits().last().unwrap().flags.allow_modeset);

    // When even the modeset fails, the error is reported.
    rig.events();
    rig.device.fail_next_commit(22);
    rig.draw_expecting_failure(output); // the flip fails quietly
    rig.device.fail_next_commit(22);
    let error = rig
        .backend
        .present(None, output)
        .expect_err("modeset failure");
    assert!(error.to_string().contains("modeset"), "{error}");
    // And the next attempt works again.
    rig.backend.present(None, output).unwrap();
    assert!(rig.device.commits().last().unwrap().flags.allow_modeset);
    assert_eq!(rig.backend.status().failed_commits, 3);
}

#[test]
fn a_flip_that_never_completes_times_out() {
    let config = KmsConfig {
        flip_timeout: Duration::from_millis(500),
        ..Default::default()
    };
    let mut rig = rig_with(one_display(), config).unwrap();
    let output = rig.output().id;
    rig.draw(output);
    rig.events();
    rig.draw(output);
    // The flip event gets lost (the mock forgets flips when master
    // changes hands; the backend is not told anything).
    rig.device.set_master(false);
    rig.device.set_master(true);
    rig.backend.request_frame(output);

    rig.clock.advance(Duration::from_millis(400));
    rig.backend.check_timeouts();
    assert!(rig.events().is_empty(), "not overdue yet");

    rig.clock.advance(Duration::from_millis(200));
    rig.backend.check_timeouts();
    assert_eq!(rig.events(), [BackendEvent::Frame { output }]);
    rig.backend.present(None, output).unwrap();
    assert!(rig.device.commits().last().unwrap().flags.allow_modeset);
    // Checking again does nothing.
    rig.backend.check_timeouts();
    assert!(matches!(
        rig.events().as_slice(),
        [BackendEvent::Presented { .. }]
    ));
}

#[test]
fn a_busy_device_makes_the_backend_wait_for_the_flip() {
    let mut rig = rig();
    let output = rig.output().id;
    rig.draw(output);
    rig.events();
    // The device reports a flip in flight that the backend did not expect.
    rig.device.fail_next_commit(16);
    rig.backend.request_frame(output);
    rig.events();
    rig.backend.present(None, output).unwrap();
    assert!(rig.events().is_empty());
    // It waits like for any flip, and gives up after the timeout.
    rig.clock.advance(Duration::from_secs(2));
    rig.backend.check_timeouts();
    assert_eq!(rig.events(), [BackendEvent::Frame { output }]);
}

#[test]
fn emergency_keys_are_taken_before_anything_else_sees_them() {
    let mut rig = rig();
    rig.output();
    let key = |code, pressed| RawInput::Key {
        code,
        pressed,
        time_us: 42,
    };
    const CTRL: u32 = 29;
    const ALT: u32 = 56;
    const BACKSPACE: u32 = 14;
    const F2: u32 = 60;
    const A: u32 = 30;

    rig.backend.push_input(key(A, true));
    rig.backend.push_input(key(CTRL, true));
    rig.backend.push_input(key(ALT, true));
    rig.backend.push_input(key(F2, true));
    rig.backend.push_input(key(F2, false));
    rig.backend.push_input(key(BACKSPACE, true));
    rig.backend.push_input(key(BACKSPACE, false));

    let events = rig.events();
    let keys: Vec<(u32, bool)> = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Input(InputEvent::Key {
                keycode, pressed, ..
            }) => Some((*keycode, *pressed)),
            _ => None,
        })
        .collect();
    // The hotkey keys themselves never show up as input.
    assert_eq!(keys, [(A, true), (CTRL, true), (ALT, true)]);
    assert_eq!(events.last(), Some(&BackendEvent::QuitRequested));
    assert_eq!(rig.backend.take_hotkeys(), [Hotkey::SwitchVt(2)]);
    assert!(rig.backend.take_hotkeys().is_empty());
}

#[test]
fn absolute_pointer_positions_use_the_output_size_and_scale() {
    let config = KmsConfig {
        scale: 2.0,
        ..Default::default()
    };
    let mut rig = rig_with(one_display(), config).unwrap();
    rig.output();
    rig.backend.push_input(RawInput::PointerMotionAbsolute {
        x: 0.5,
        y: 0.5,
        time_us: 1,
    });
    // 1920x1080 at scale 2 is 960x540 logical.
    assert_eq!(
        rig.events(),
        [BackendEvent::Input(InputEvent::PointerMotionAbsolute {
            x: 480.0,
            y: 270.0,
            time_us: 1
        })]
    );
}

#[test]
fn shutdown_turns_the_output_off() {
    let mut rig = rig();
    let output = rig.output().id;
    let crtc = rig.crtc();
    rig.draw(output);
    rig.events();
    rig.backend.shutdown(None);
    assert_eq!(rig.device.active_mode(crtc), None);
    assert_eq!(rig.device.framebuffer_count(), 0);
    assert_eq!(rig.device.dumb_buffer_count(), 0);
    assert_eq!(rig.events(), [BackendEvent::OutputRemoved(output)]);
}

#[test]
fn forcing_the_dmabuf_path_without_a_renderer_fails_clearly() {
    let config = KmsConfig {
        scanout: ScanoutPreference::Dmabuf,
        ..Default::default()
    };
    let error = rig_with(one_display(), config).err().expect("must fail");
    assert!(error.to_string().contains("dma-buf"), "{error}");
}
