//! Planning and the KMS backend against snapshots of real DRM devices
//! (`tests/fixtures/`), through the mock device. No hardware is touched.

use std::time::Duration;

use proto_backend::kms::clock::ManualClock;
use proto_backend::kms::mock::MockDevice;
use proto_backend::kms::model::{
    DeviceSnapshot, FOURCC_XRGB8888, MODIFIER_LINEAR, PlaneKind, property,
};
use proto_backend::kms::plan::{
    ModeRequest, OutputRequest, ScanoutFacts, ScanoutPath, ScanoutPreference, choose_scanout_path,
    plan_output,
};
use proto_backend::kms::{KmsBackend, KmsConfig};
use proto_backend::{Backend, BackendEvent};

fn amdgpu() -> DeviceSnapshot {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/amdgpu.kms.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn request(connector: &str) -> OutputRequest {
    OutputRequest {
        connector: Some(connector.to_string()),
        mode: None,
    }
}

#[test]
fn the_snapshot_describes_the_machine() {
    let snapshot = amdgpu();
    assert_eq!(snapshot.driver.name, "amdgpu");
    let names: Vec<&str> = snapshot
        .connectors
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["DP-1", "DP-2", "HDMI-A-1"]);
    assert!(snapshot.connectors.iter().all(|c| c.connected));
    assert_eq!(snapshot.crtcs.len(), 4);
    // What HDR output, VRR and tearing need is there on this hardware: HDR
    // metadata, the colorspace property, VRR, tearing flips.
    let dp1 = snapshot.connector_by_name("DP-1").unwrap();
    for name in [
        "HDR_OUTPUT_METADATA",
        "Colorspace",
        "max bpc",
        "vrr_capable",
    ] {
        assert!(
            property(&dp1.properties, name).is_some(),
            "DP-1 lacks {name}"
        );
    }
    assert_eq!(property(&dp1.properties, "vrr_capable").unwrap().value, 1);
    assert!(snapshot.caps.atomic_async_page_flip);
    assert!(snapshot.caps.addfb2_modifiers);
    assert!(
        snapshot
            .crtcs
            .iter()
            .all(|c| property(&c.properties, "VRR_ENABLED").is_some())
    );
    // The EDIDs are anonymized: anonymizing them again changes nothing, and
    // every block still has a valid checksum.
    for connector in &snapshot.connectors {
        let edid = property(&connector.properties, "EDID").unwrap();
        let proto_backend::kms::model::PropertyKind::Blob { data: Some(hex) } = &edid.kind else {
            panic!("{} has no EDID data", connector.name);
        };
        let bytes = proto_backend::kms::blob::from_hex(hex).unwrap();
        let mut again = bytes.clone();
        proto_backend::kms::blob::anonymize_edid(&mut again);
        assert_eq!(again, bytes, "{}", connector.name);
        for block in bytes.chunks_exact(128) {
            assert_eq!(block.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0);
        }
        let identity = proto_backend::kms::blob::edid_identity(&bytes).unwrap();
        assert_eq!(identity.manufacturer, "XXX");
        assert_eq!(identity.name.as_deref(), Some("Display"));
    }
}

#[test]
fn planning_picks_the_preferred_mode_and_keeps_the_current_routing() {
    let snapshot = amdgpu();

    let dp1 = plan_output(&snapshot, &request("DP-1")).unwrap();
    assert_eq!(dp1.mode.size(), (3840, 2160));
    // 1333.33 MHz over 4000 x 2314: 144.05 Hz.
    assert!(
        (dp1.mode.refresh_mhz() as i64 - 144_050).abs() < 20,
        "{}",
        dp1.mode.refresh_mhz()
    );
    assert_eq!(dp1.physical_size_mm, (600, 340));
    // The CRTC the display is already on is kept.
    let current = property(
        &snapshot.connector(dp1.connector).unwrap().properties,
        "CRTC_ID",
    )
    .unwrap()
    .value as u32;
    assert_eq!(dp1.crtc, current);
    // The primary plane of that CRTC is not the first plane the kernel
    // lists (that one belongs to the last CRTC).
    let plane = snapshot.plane(dp1.primary_plane).unwrap();
    assert_eq!(plane.kind(), Some(PlaneKind::Primary));
    assert!(
        snapshot
            .crtcs_in_mask(plane.possible_crtcs)
            .contains(&dp1.crtc)
    );
    assert_ne!(dp1.primary_plane, snapshot.planes[0].id);
    // The plane reports modifiers, linear among them.
    let modifiers = dp1.plane_modifiers.as_ref().unwrap();
    assert!(modifiers.contains(&MODIFIER_LINEAR));
    assert!(modifiers.len() > 1);
    assert!(dp1.stale_planes.is_empty());

    // Without naming a connector, the first connected one is taken.
    let default = plan_output(&snapshot, &OutputRequest::default()).unwrap();
    assert_eq!(default.connector_name, "DP-1");

    // Each display gets a different CRTC and plane.
    let dp2 = plan_output(&snapshot, &request("DP-2")).unwrap();
    let hdmi = plan_output(&snapshot, &request("HDMI-A-1")).unwrap();
    assert_eq!(dp2.mode.size(), (3840, 2160));
    assert_eq!(dp2.mode.refresh_mhz(), 60_000);
    assert_eq!(hdmi.mode.size(), (1920, 1080));
    let crtcs = [dp1.crtc, dp2.crtc, hdmi.crtc];
    assert!(crtcs[0] != crtcs[1] && crtcs[1] != crtcs[2] && crtcs[0] != crtcs[2]);
}

#[test]
fn a_cursor_left_by_the_previous_compositor_is_found() {
    let snapshot = amdgpu();
    // When the snapshot was taken the pointer was on DP-2: its CRTC has a
    // cursor plane with a framebuffer.
    let dp2 = plan_output(&snapshot, &request("DP-2")).unwrap();
    assert_eq!(dp2.stale_planes.len(), 1);
    let stale = snapshot.plane(dp2.stale_planes[0].plane).unwrap();
    assert_eq!(stale.kind(), Some(PlaneKind::Cursor));
}

#[test]
fn requested_modes_are_found_among_the_real_ones() {
    let snapshot = amdgpu();
    let with_mode = |refresh_mhz| OutputRequest {
        connector: Some("DP-1".to_string()),
        mode: Some(ModeRequest {
            width: 1920,
            height: 1080,
            refresh_mhz,
        }),
    };
    let fastest = plan_output(&snapshot, &with_mode(None)).unwrap();
    assert_eq!(fastest.mode.vrefresh, 144);
    let sixty = plan_output(&snapshot, &with_mode(Some(60_000))).unwrap();
    assert_eq!(sixty.mode.vrefresh, 60);
    assert!(
        plan_output(
            &snapshot,
            &OutputRequest {
                connector: Some("DP-1".to_string()),
                mode: Some(ModeRequest {
                    width: 1234,
                    height: 567,
                    refresh_mhz: None
                }),
            }
        )
        .is_err()
    );
}

#[test]
fn the_scanout_path_on_this_gpu_is_dmabuf_with_shared_modifiers() {
    let snapshot = amdgpu();
    let pipeline = plan_output(&snapshot, &request("DP-1")).unwrap();
    // The modifiers a Vulkan driver for this GPU reports for rendering to
    // B8G8R8A8.
    let renderer = vec![
        0x0200_0000_0008_2305,
        0x0200_0000_0004_2305,
        0x0200_0000_0008_2405,
        0x0200_0000_0008_2205,
        0x0200_0000_0008_2105,
        0x0200_0000_0000_0305,
        0x0200_0000_0000_0a04,
        0x0200_0000_0000_0105,
        MODIFIER_LINEAR,
    ];
    let path = choose_scanout_path(&ScanoutFacts {
        preference: ScanoutPreference::Auto,
        renderer_modifiers: renderer,
        renderer_is_kms_device: true,
        plane_modifiers: pipeline.plane_modifiers.clone(),
        dumb_buffers: snapshot.caps.dumb_buffer,
    })
    .unwrap();
    let ScanoutPath::Dmabuf { modifiers } = path else {
        panic!("expected the dma-buf path, got {path:?}");
    };
    assert!(modifiers.contains(&MODIFIER_LINEAR));
    // At least one tiled layout is shared, not only linear.
    assert!(modifiers.len() > 1, "{modifiers:x?}");
}

#[test]
fn the_backend_takes_over_one_display_and_leaves_the_others() {
    let snapshot = amdgpu();
    let before = snapshot.clone();
    let clock = ManualClock::new(Duration::from_secs(5000));
    let device = MockDevice::new(snapshot, clock.clone());
    let config = KmsConfig {
        output: request("DP-2"),
        ..Default::default()
    };
    let mut backend = KmsBackend::new(device.clone(), clock, config, None, false).unwrap();

    let events = backend.drain_events();
    let [BackendEvent::OutputAdded(output)] = events.as_slice() else {
        panic!("expected one output, got {events:?}");
    };
    assert_eq!(output.name, "DP-2");
    assert_eq!(output.size, (3840, 2160));
    // Make and model come from the EDID (anonymized in the fixture).
    assert_eq!(output.make, "XXX");
    assert_eq!(output.model, "Display");

    // The first frame: a full modeset that the (strict) mock accepts.
    backend.request_frame(output.id);
    assert_eq!(
        backend.drain_events(),
        [BackendEvent::Frame { output: output.id }]
    );
    backend.present(None, output.id).unwrap();
    assert!(device.rejected().is_empty(), "{:?}", device.rejected());
    backend.handle_device_events().unwrap();
    assert!(matches!(
        backend.drain_events().as_slice(),
        [BackendEvent::Presented { .. }]
    ));

    let status = backend.status().output.unwrap();
    assert_eq!(
        device.active_mode(status.crtc).unwrap().size(),
        (3840, 2160)
    );
    let shown = device.displayed(status.crtc).unwrap();
    assert_eq!(
        (shown.width, shown.height, shown.format),
        (3840, 2160, FOURCC_XRGB8888)
    );

    let mut after = device.clone();
    let after = proto_backend::kms::device::KmsDevice::snapshot(&mut after).unwrap();
    // The cursor the previous compositor left on this CRTC is gone.
    let cursor_on_crtc = after.planes.iter().any(|plane| {
        plane.kind() == Some(PlaneKind::Cursor)
            && property(&plane.properties, "CRTC_ID").unwrap().value == status.crtc as u64
    });
    assert!(!cursor_on_crtc);
    // The other two displays are exactly as they were: routing, CRTC
    // state, planes.
    for name in ["DP-1", "HDMI-A-1"] {
        let (old, new) = (
            before.connector_by_name(name).unwrap(),
            after.connector_by_name(name).unwrap(),
        );
        assert_eq!(old, new, "{name} was touched");
        let crtc = property(&old.properties, "CRTC_ID").unwrap().value as u32;
        assert_eq!(
            before.crtc(crtc),
            after.crtc(crtc),
            "CRTC of {name} was touched"
        );
        for (old, new) in before.planes.iter().zip(&after.planes) {
            if property(&old.properties, "CRTC_ID").unwrap().value == crtc as u64 {
                assert_eq!(old, new, "a plane of {name} was touched");
            }
        }
    }

    // Flips keep working on the real device's rules.
    backend.request_frame(output.id);
    backend.drain_events();
    backend.present(None, output.id).unwrap();
    device.vblank();
    backend.handle_device_events().unwrap();
    assert!(device.rejected().is_empty());
    assert_eq!(backend.status().output.unwrap().frames_presented, 2);
}

// ---------------------------------------------------------------------------
// The verification VM's devices, from the `drm_info -j` records in
// `tools/vm/reference/`.
// ---------------------------------------------------------------------------

fn vm_reference(name: &str) -> DeviceSnapshot {
    let path = format!(
        "{}/../../tools/vm/reference/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut devices = proto_backend::kms::drm_info::snapshots_from_drm_info(&text).unwrap();
    assert_eq!(devices.len(), 1, "{name}");
    devices.remove(0).1
}

const VIRTIO_GPU: [&str; 2] = [
    "virtio-gpu-drm_info-6.19.10-300.json",
    "virtio-gpu-drm_info-7.3.3-200.json",
];
const VKMS: [&str; 2] = [
    "vkms-drm_info-6.19.10-300.json",
    "vkms-drm_info-7.3.3-200.json",
];

fn facts(
    preference: ScanoutPreference,
    renderer_modifiers: &[u64],
    renderer_is_kms_device: bool,
    snapshot: &DeviceSnapshot,
) -> ScanoutFacts {
    let pipeline = plan_output(snapshot, &OutputRequest::default()).unwrap();
    ScanoutFacts {
        preference,
        renderer_modifiers: renderer_modifiers.to_vec(),
        renderer_is_kms_device,
        plane_modifiers: pipeline.plane_modifiers,
        dumb_buffers: snapshot.caps.dumb_buffer,
    }
}

#[test]
fn virtio_gpu_is_planned_as_the_vm_measurements_say() {
    for name in VIRTIO_GPU {
        let snapshot = vm_reference(name);
        assert_eq!(snapshot.driver.name, "virtio_gpu", "{name}");
        let pipeline = plan_output(&snapshot, &OutputRequest::default()).unwrap();
        assert_eq!(pipeline.connector_name, "Virtual-1");
        assert_eq!(
            (pipeline.mode.hdisplay, pipeline.mode.vdisplay),
            (1280, 800)
        );
        assert!(pipeline.mode.is_preferred());
        // The EDID names QEMU's monitor.
        let connector = snapshot.connector_by_name("Virtual-1").unwrap();
        let edid = property(&connector.properties, "EDID").unwrap();
        let proto_backend::kms::model::PropertyKind::Blob { data: Some(hex) } = &edid.kind else {
            panic!("no EDID data");
        };
        let identity = proto_backend::kms::blob::edid_identity(
            &proto_backend::kms::blob::from_hex(hex).unwrap(),
        )
        .unwrap();
        assert_eq!(identity.name.as_deref(), Some("QEMU Monitor"), "{name}");

        // The primary plane takes XRGB8888 only and no modifiers; ARGB8888
        // is on the cursor plane alone.
        let primary = snapshot.plane(pipeline.primary_plane).unwrap();
        assert_eq!(primary.kind(), Some(PlaneKind::Primary));
        assert_eq!(primary.formats, [FOURCC_XRGB8888]);
        assert_eq!(pipeline.plane_modifiers, None);
        assert!(!snapshot.caps.addfb2_modifiers);
        assert!(property(&primary.properties, "IN_FENCE_FD").is_some());
        // No tearing, no VRR to speak of, as expected of a virtual device.
        assert!(!snapshot.caps.atomic_async_page_flip);

        // Venus (the renderer is the KMS device, can export linear):
        // dma-buf, restricted to linear because the driver takes no
        // modifiers. A renderer that could only export tiled images must
        // not be used for scanout: virtio-gpu would show garbage.
        const TILED: u64 = 0x0200_0000_0000_0001;
        assert_eq!(
            choose_scanout_path(&facts(
                ScanoutPreference::Auto,
                &[TILED, MODIFIER_LINEAR],
                true,
                &snapshot
            )),
            Ok(ScanoutPath::Dmabuf {
                modifiers: vec![MODIFIER_LINEAR]
            })
        );
        assert_eq!(
            choose_scanout_path(&facts(ScanoutPreference::Auto, &[TILED], true, &snapshot)),
            Ok(ScanoutPath::CpuCopy)
        );
        assert!(
            choose_scanout_path(&facts(ScanoutPreference::Dmabuf, &[TILED], true, &snapshot))
                .is_err()
        );
        // lavapipe (not the KMS device): the CPU copy.
        assert_eq!(
            choose_scanout_path(&facts(
                ScanoutPreference::Auto,
                &[MODIFIER_LINEAR],
                false,
                &snapshot
            )),
            Ok(ScanoutPath::CpuCopy)
        );
    }
}

#[test]
fn vkms_is_planned_around_its_writeback_connector() {
    for name in VKMS {
        let snapshot = vm_reference(name);
        assert_eq!(snapshot.driver.name, "vkms", "{name}");
        // drm_info asks for writeback connectors, so the record has one.
        // It is never "connected", and must not be picked.
        assert!(snapshot.connector_by_name("Writeback-1").is_some());
        let pipeline = plan_output(&snapshot, &OutputRequest::default()).unwrap();
        assert_eq!(pipeline.connector_name, "Virtual-1");
        assert_eq!(
            (pipeline.mode.hdisplay, pipeline.mode.vdisplay),
            (1024, 768)
        );
        let primary = snapshot.plane(pipeline.primary_plane).unwrap();
        assert_eq!(primary.kind(), Some(PlaneKind::Primary));
        assert!(primary.formats.contains(&FOURCC_XRGB8888));
        assert_eq!(pipeline.plane_modifiers, Some(vec![MODIFIER_LINEAR]));
        let overlays = snapshot
            .planes
            .iter()
            .filter(|p| p.kind() == Some(PlaneKind::Overlay))
            .count();
        assert_eq!(overlays, 8);
        // Nothing drives the CRTC in the record: no planes to detach.
        assert!(pipeline.stale_planes.is_empty());
    }
}

/// The backend runs on each VM device (mocked from its record): a modeset,
/// flips at the mode's rate, and nothing the mock's kernel rules reject.
#[test]
fn the_backend_drives_the_vm_devices() {
    for name in VIRTIO_GPU.iter().chain(VKMS.iter()) {
        let snapshot = vm_reference(name);
        let clock = ManualClock::new(Duration::from_secs(10));
        let device = MockDevice::new(snapshot, clock.clone());
        let mut backend = KmsBackend::new(
            device.clone(),
            clock.clone(),
            KmsConfig::default(),
            None,
            false,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let events = backend.drain_events();
        let output = events
            .iter()
            .find_map(|e| match e {
                BackendEvent::OutputAdded(info) => Some(info.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name}: no output"));
        assert_eq!(output.name, "Virtual-1");

        let mut presented = 0;
        backend.request_frame(output.id);
        for _ in 0..30 {
            for event in backend.drain_events() {
                match event {
                    BackendEvent::Frame { output } => {
                        backend.present(None, output).unwrap();
                        backend.request_frame(output);
                    }
                    BackendEvent::Presented { .. } => presented += 1,
                    _ => {}
                }
            }
            device.vblank();
            backend.handle_device_events().unwrap();
        }
        assert!(presented >= 28, "{name}: {presented} frames");
        assert!(
            device.rejected().is_empty(),
            "{name}: {:?}",
            device.rejected()
        );
        let commits = device.commits();
        assert!(commits[0].flags.allow_modeset);
        assert!(commits[1..].iter().all(|c| !c.flags.allow_modeset));
        let status = backend.status();
        assert_eq!(status.output.as_ref().unwrap().scanout.name(), "cpu-copy");
        Backend::shutdown(&mut backend, None);
        assert_eq!(device.dumb_buffer_count(), 0, "{name}");
    }
}

// ---------------------------------------------------------------------------
// The verification VM's devices as `proto-compositor kms-info` itself reports them
// in the guest (kernel 7.3.3).
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> DeviceSnapshot {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}

/// What the compositor's own device code sees is what the planning code
/// was tested against through the `drm_info` record of the same kernel,
/// except for what depends on the capabilities a client asks for.
#[test]
fn the_guests_own_snapshot_agrees_with_the_drm_info_record() {
    let own = fixture("virtio-gpu-7.3.3.kms.json");
    let record = vm_reference("virtio-gpu-drm_info-7.3.3-200.json");
    assert_eq!(own.driver.name, record.driver.name);
    assert_eq!(own.caps, record.caps);

    let own_plan = plan_output(&own, &OutputRequest::default()).unwrap();
    let record_plan = plan_output(&record, &OutputRequest::default()).unwrap();
    assert_eq!(own_plan.connector_name, "Virtual-1");
    assert_eq!(own_plan.connector, record_plan.connector);
    assert_eq!(own_plan.crtc, record_plan.crtc);
    assert_eq!(own_plan.primary_plane, record_plan.primary_plane);
    assert_eq!(own_plan.mode, record_plan.mode);
    assert_eq!(own_plan.plane_modifiers, None);
    assert_eq!(own_plan.properties, record_plan.properties);

    // virtio-gpu is a driver whose cursor plane carries a hotspot. The
    // kernel hides such a plane from atomic clients that do not declare
    // DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT: drm_info declares it and sees
    // the plane, hizukuri does not and sees the primary plane only. Using
    // the cursor plane in a VM will need that capability.
    let cursors = |snapshot: &DeviceSnapshot| {
        snapshot
            .planes
            .iter()
            .filter(|p| p.kind() == Some(PlaneKind::Cursor))
            .count()
    };
    assert_eq!((cursors(&own), cursors(&record)), (0, 1));
    // drm_info also asks for aspect ratio and stereo modes, so it lists
    // more modes; every mode hizukuri sees is among them by size and rate.
    let connector = own.connector_by_name("Virtual-1").unwrap();
    let recorded = record.connector_by_name("Virtual-1").unwrap();
    assert!(connector.modes.len() <= recorded.modes.len());
    for mode in &connector.modes {
        assert!(
            recorded
                .modes
                .iter()
                .any(|m| (m.hdisplay, m.vdisplay, m.vrefresh)
                    == (mode.hdisplay, mode.vdisplay, mode.vrefresh)),
            "{mode:?}"
        );
    }
    // The modes the VM check sets are there.
    for (width, height) in [(1280, 800), (1920, 1080), (3840, 2160)] {
        let request = OutputRequest {
            connector: None,
            mode: Some(ModeRequest {
                width,
                height,
                refresh_mhz: None,
            }),
        };
        let pipeline = plan_output(&own, &request).unwrap();
        assert_eq!(
            (pipeline.mode.hdisplay as u32, pipeline.mode.vdisplay as u32),
            (width, height)
        );
    }
}

/// vkms next to virtio-gpu: connector numbers are global to the kernel,
/// not per device, so its connector is `Virtual-2`. A client that does not
/// ask for writeback connectors is not shown one.
#[test]
fn the_guests_vkms_snapshot_is_planned() {
    let snapshot = fixture("vkms-7.3.3.kms.json");
    assert_eq!(snapshot.driver.name, "vkms");
    let names: Vec<&str> = snapshot
        .connectors
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["Virtual-2"]);
    let pipeline = plan_output(&snapshot, &OutputRequest::default()).unwrap();
    assert_eq!(pipeline.connector_name, "Virtual-2");
    assert_eq!(pipeline.plane_modifiers, Some(vec![MODIFIER_LINEAR]));
    assert!(pipeline.stale_planes.is_empty());
    // lavapipe renders, so frames are copied.
    assert_eq!(
        choose_scanout_path(&facts(
            ScanoutPreference::Auto,
            &[MODIFIER_LINEAR],
            false,
            &snapshot
        )),
        Ok(ScanoutPath::CpuCopy)
    );
}

/// The backend on the mock built from each of the guest's own snapshots.
#[test]
fn the_backend_drives_the_guests_own_snapshots() {
    for (name, connector) in [
        ("virtio-gpu-7.3.3.kms.json", "Virtual-1"),
        ("vkms-7.3.3.kms.json", "Virtual-2"),
    ] {
        let clock = ManualClock::new(Duration::from_secs(10));
        let device = MockDevice::new(fixture(name), clock.clone());
        let mut backend = KmsBackend::new(
            device.clone(),
            clock.clone(),
            KmsConfig {
                output: OutputRequest {
                    connector: None,
                    mode: Some(ModeRequest {
                        width: 1920,
                        height: 1080,
                        refresh_mhz: None,
                    }),
                },
                ..Default::default()
            },
            None,
            false,
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let output = backend
            .drain_events()
            .into_iter()
            .find_map(|e| match e {
                BackendEvent::OutputAdded(info) => Some(info),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name}: no output"));
        assert_eq!(output.name, connector);
        assert_eq!(output.size, (1920, 1080));
        backend.request_frame(output.id);
        let mut presented = 0;
        for _ in 0..10 {
            for event in backend.drain_events() {
                match event {
                    BackendEvent::Frame { output } => {
                        backend.present(None, output).unwrap();
                        backend.request_frame(output);
                    }
                    BackendEvent::Presented { .. } => presented += 1,
                    _ => {}
                }
            }
            device.vblank();
            backend.handle_device_events().unwrap();
        }
        assert!(presented >= 8, "{name}: {presented}");
        assert!(
            device.rejected().is_empty(),
            "{name}: {:?}",
            device.rejected()
        );
        Backend::shutdown(&mut backend, None);
    }
}
