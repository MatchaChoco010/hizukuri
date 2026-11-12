//! Protocol tests of color-management-v1 (`wp_color_manager_v1`).
//!
//! Like `protocol.rs`: the compositor core and the test clients run in one
//! thread, connected by socket pairs, and nothing is drawn. What a surface's
//! image description is taken to be is read from the state dump; that it
//! reaches the pixels is checked in `script.rs`.

use std::os::unix::net::UnixStream;

use proto_backend::headless::{HeadlessBackend, HeadlessConfig, HeadlessOutputConfig};
use proto_color::ImageDescription;
use proto_compositor::compositor::{Compositor, CompositorOptions};
use proto_compositor::control::Request;
use proto_compositor::scene::NullScene;
use proto_test_client::{
    ClientEvent, ClientOptions, ColorTarget, CreatorStep, ImageInfo, TestClient,
};
use serde_json::Value;

// The protocol's numbers.
const TF_BT1886: u32 = 1;
const TF_GAMMA22: u32 = 2;
const TF_GAMMA28: u32 = 3;
const TF_ST240: u32 = 4;
const TF_EXT_LINEAR: u32 = 5;
const TF_SRGB: u32 = 9;
const TF_PQ: u32 = 11;
const TF_HLG: u32 = 13;
const TF_COMPOUND_POWER_2_4: u32 = 14;
const PRIMARIES_SRGB: u32 = 1;
const PRIMARIES_BT2020: u32 = 6;
const PRIMARIES_DISPLAY_P3: u32 = 9;
const INTENT_PERCEPTUAL: u32 = 0;
const INTENT_RELATIVE: u32 = 1;
const INTENT_SATURATION: u32 = 2;
const FEATURE_WINDOWS_BT2100: u32 = 8;
const CAUSE_LOW_VERSION: u32 = 0;
const CAUSE_UNSUPPORTED: u32 = 1;

const OUTPUT: &str = "HEADLESS-1";

struct Fixture {
    compositor: Compositor<HeadlessBackend>,
    clients: Vec<TestClient>,
    /// The protocol error a client was disconnected with, if any.
    errors: Vec<Option<String>>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_output(ImageDescription::SRGB_GAMMA22)
    }

    fn with_output(image_description: ImageDescription) -> Self {
        let config = HeadlessConfig {
            outputs: vec![HeadlessOutputConfig {
                image_description,
                ..Default::default()
            }],
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
            errors: Vec::new(),
        }
    }

    /// Adds a client that binds `wp_color_manager_v1` with `version` and
    /// has a window, and waits until the window is mapped and the
    /// manager's advertisement is in.
    fn add(&mut self, title: &str, version: u32) -> usize {
        let options = ClientOptions {
            title: title.to_string(),
            color_manager_version: Some(version),
            ..Default::default()
        };
        let (server_end, client_end) = UnixStream::pair().unwrap();
        self.compositor.wl().insert_client(server_end).unwrap();
        self.clients
            .push(TestClient::connect(client_end, options).unwrap());
        self.errors.push(None);
        let index = self.clients.len() - 1;
        let title = title.to_string();
        self.settle("window mapped and colors advertised", |f| {
            f.clients[index].is_drawn()
                && surface_of(&f.state(), &title).is_some()
                && support(&f.clients[index]).is_some()
        });
        index
    }

    fn step(&mut self) {
        self.compositor.dispatch_clients().unwrap();
        self.compositor.backend().tick();
        self.compositor.process().unwrap();
        for (client, error) in self.clients.iter_mut().zip(&mut self.errors) {
            if error.is_none()
                && let Err(e) = client.pump(None)
            {
                *error = Some(e.to_string());
            }
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

    fn surface(&mut self, title: &str) -> Value {
        surface_of(&self.state(), title).expect("the window is there")
    }

    /// Waits for the answer to image description `index` of a client.
    fn answer(&mut self, client: usize, index: usize) -> ClientEvent {
        let find = |f: &Fixture| {
            f.clients[client]
                .events()
                .iter()
                .find(|e| match e {
                    ClientEvent::ImageDescriptionReady { index: i, .. }
                    | ClientEvent::ImageDescriptionFailed { index: i, .. } => *i == index,
                    _ => false,
                })
                .cloned()
        };
        self.settle("an image description answered", |f| find(f).is_some());
        find(self).unwrap()
    }

    fn ready(&mut self, client: usize, index: usize) -> u64 {
        match self.answer(client, index) {
            ClientEvent::ImageDescriptionReady { identity, .. } => identity,
            other => panic!("not ready: {other:?}"),
        }
    }

    fn information(&mut self, client: usize, index: usize) -> ImageInfo {
        self.clients[client].get_image_information(index).unwrap();
        let find = |f: &Fixture| {
            f.clients[client]
                .events()
                .iter()
                .rev()
                .find_map(|e| match e {
                    ClientEvent::ImageDescriptionInfo { index: i, info } if *i == index => {
                        Some(info.clone())
                    }
                    _ => None,
                })
        };
        self.settle("information delivered", |f| find(f).is_some());
        find(self).unwrap()
    }

    /// Steps until the client is disconnected, and returns the error.
    fn error(&mut self, client: usize) -> String {
        self.settle("the client disconnected", |f| f.errors[client].is_some());
        self.errors[client].clone().unwrap()
    }
}

fn surface_of(state: &Value, title: &str) -> Option<Value> {
    state["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"] == title && w["mapped"] == true)
        .map(|w| w["surfaces"][0].clone())
}

/// What `wp_color_manager_v1` advertised: the bound version, the intents,
/// features, transfer functions and primaries.
type Support = (u32, Vec<u32>, Vec<u32>, Vec<u32>, Vec<u32>);

fn support(client: &TestClient) -> Option<Support> {
    client.events().iter().find_map(|e| match e {
        ClientEvent::ColorManagerSupport {
            version,
            intents,
            features,
            transfer_functions,
            primaries,
        } => Some((
            *version,
            intents.clone(),
            features.clone(),
            transfer_functions.clone(),
            primaries.clone(),
        )),
        _ => None,
    })
}

fn pq_bt2020() -> Vec<CreatorStep> {
    vec![
        CreatorStep::TfNamed(TF_PQ),
        CreatorStep::PrimariesNamed(PRIMARIES_BT2020),
    ]
}

#[test]
fn advertises_what_it_converts_by_bound_version() {
    let mut f = Fixture::new();
    let v1 = f.add("v1", 1);
    let v2 = f.add("v2", 2);
    let v3 = f.add("v3", 3);
    for (client, version) in [(v1, 1), (v2, 2), (v3, 3)] {
        let (bound, intents, features, tfs, primaries) = support(&f.clients[client]).unwrap();
        assert_eq!(bound, version);
        assert_eq!(intents, vec![INTENT_PERCEPTUAL, INTENT_RELATIVE]);
        // parametric, set_primaries, set_tf_power, set_luminances,
        // set_mastering_display_primaries, extended_target_volume,
        // windows_scrgb; windows_bt2100 since version 3. Never icc_v2_v4.
        let mut expected = vec![1, 2, 3, 4, 5, 6, 7];
        if version >= 3 {
            expected.push(FEATURE_WINDOWS_BT2100);
        }
        assert_eq!(features, expected, "version {version}");
        let mut expected = vec![
            TF_BT1886,
            TF_GAMMA22,
            TF_GAMMA28,
            TF_EXT_LINEAR,
            TF_PQ,
            TF_HLG,
        ];
        // srgb is deprecated in version 2; compound_power_2_4 came with it.
        expected.push(if version >= 2 {
            TF_COMPOUND_POWER_2_4
        } else {
            TF_SRGB
        });
        assert_eq!(tfs, expected, "version {version}");
        assert_eq!(primaries, (1..=10).collect::<Vec<_>>());
    }
}

#[test]
fn surface_description_takes_effect_at_commit() {
    let mut f = Fixture::new();
    let c = f.add("hdr", 3);
    let before = f.surface("hdr");
    assert_eq!(before["color_source"], "default");
    assert_eq!(before["image_description"]["name"], "srgb-gamma22");

    let index = f.clients[c].create_image_description(&pq_bt2020()).unwrap();
    let identity = f.ready(c, index);
    assert_ne!(identity, 0);
    assert!(matches!(
        f.answer(c, index),
        ClientEvent::ImageDescriptionReady { version: 2, .. }
    ));
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.clients[c]
        .set_image_description(index, INTENT_PERCEPTUAL)
        .unwrap();
    f.steps(4);
    // Double-buffered: nothing changes before the commit.
    assert_eq!(f.surface("hdr")["color_source"], "default");

    f.clients[c].commit().unwrap();
    f.steps(4);
    let s = f.surface("hdr");
    assert_eq!(s["color_source"], "protocol");
    assert_eq!(s["render_intent"], "perceptual");
    assert_eq!(s["image_description"]["name"], "bt2020-pq");
    assert_eq!(
        s["decode_pipeline"],
        "unpremultiply | eotf:pq | primaries:bt2020->bt2020 | luminance:x(10000/202.995) | premultiply"
    );

    // unset_image_description is double-buffered too.
    f.clients[c].unset_image_description().unwrap();
    f.steps(4);
    assert_eq!(f.surface("hdr")["color_source"], "protocol");
    f.clients[c].commit().unwrap();
    f.steps(4);
    assert_eq!(f.surface("hdr")["color_source"], "default");

    // Destroying the color surface unsets as well, and lets the client
    // make another.
    f.clients[c]
        .set_image_description(index, INTENT_RELATIVE)
        .unwrap();
    f.clients[c].commit().unwrap();
    f.steps(4);
    assert_eq!(f.surface("hdr")["render_intent"], "relative");
    f.clients[c].destroy_color_surface();
    f.clients[c].commit().unwrap();
    f.steps(4);
    assert_eq!(f.surface("hdr")["color_source"], "default");
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.steps(4);
    assert!(f.errors[c].is_none(), "{:?}", f.errors[c]);
}

#[test]
fn setting_copies_the_description() {
    let mut f = Fixture::new();
    let c = f.add("copy", 2);
    let index = f.clients[c]
        .create_image_description(&[
            CreatorStep::TfNamed(TF_COMPOUND_POWER_2_4),
            CreatorStep::PrimariesNamed(PRIMARIES_DISPLAY_P3),
        ])
        .unwrap();
    f.ready(c, index);
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.clients[c]
        .set_image_description(index, INTENT_PERCEPTUAL)
        .unwrap();
    // Gone before the commit: the surface keeps what was set.
    f.clients[c].destroy_image_description(index);
    f.clients[c].commit().unwrap();
    f.steps(4);
    let s = f.surface("copy");
    assert_eq!(s["image_description"]["name"], "display-p3");
    assert!(f.errors[c].is_none(), "{:?}", f.errors[c]);
}

// Version 1's ambiguous sRGB is the 2.2 power function, like a surface
// without a description; version 1 gets `ready`, not `ready2`.
#[test]
fn version_1_srgb_is_gamma_22() {
    let mut f = Fixture::new();
    let c = f.add("old", 1);
    let index = f.clients[c]
        .create_image_description(&[
            CreatorStep::TfNamed(TF_SRGB),
            CreatorStep::PrimariesNamed(PRIMARIES_SRGB),
        ])
        .unwrap();
    assert!(matches!(
        f.answer(c, index),
        ClientEvent::ImageDescriptionReady { version: 1, .. }
    ));
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.clients[c]
        .set_image_description(index, INTENT_PERCEPTUAL)
        .unwrap();
    f.clients[c].commit().unwrap();
    f.steps(4);
    let s = f.surface("old");
    assert_eq!(s["color_source"], "protocol");
    assert_eq!(s["image_description"]["name"], "srgb-gamma22");
}

#[test]
fn parameters_carry_through() {
    let mut f = Fixture::new();
    let c = f.add("params", 3);
    let index = f.clients[c]
        .create_image_description(&[
            CreatorStep::TfPower(24000),
            // Display P3 by its chromaticities.
            CreatorStep::Primaries([
                680000, 320000, 265000, 690000, 150000, 60000, 312700, 329000,
            ]),
            CreatorStep::Luminances(5000, 400, 100),
            CreatorStep::MasteringPrimaries([
                680000, 320000, 265000, 690000, 150000, 60000, 312700, 329000,
            ]),
            CreatorStep::MasteringLuminance(10, 350),
            CreatorStep::MaxCll(300),
            CreatorStep::MaxFall(120),
        ])
        .unwrap();
    f.ready(c, index);
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.clients[c]
        .set_image_description(index, INTENT_PERCEPTUAL)
        .unwrap();
    f.clients[c].commit().unwrap();
    f.steps(4);
    let d = f.surface("params")["image_description"].clone();
    assert_eq!(d["primaries"], "display-p3");
    assert_eq!(d["transfer"], "Power(2.4)");
    assert_eq!(d["luminance"]["min"], 0.5);
    assert_eq!(d["luminance"]["max"], 400.0);
    assert_eq!(d["luminance"]["reference"], 100.0);
    let m = &d["mastering"];
    assert_eq!(m["primaries"], "display-p3");
    assert_eq!(m["max_luminance"], 350.0);
    assert_eq!(m["max_cll"], 300.0);
    assert_eq!(m["max_fall"], 120.0);

    // PQ ignores the maximum of set_luminances.
    let index = f.clients[c]
        .create_image_description(&[
            CreatorStep::TfNamed(TF_PQ),
            CreatorStep::PrimariesNamed(PRIMARIES_BT2020),
            CreatorStep::Luminances(0, 1000, 100),
        ])
        .unwrap();
    f.ready(c, index);
    f.clients[c]
        .set_image_description(index, INTENT_PERCEPTUAL)
        .unwrap();
    f.clients[c].commit().unwrap();
    f.steps(4);
    let d = f.surface("params")["image_description"].clone();
    assert_eq!(d["luminance"]["max"], 10000.0);
    assert_eq!(d["luminance"]["reference"], 100.0);
}

#[test]
fn windows_descriptions() {
    let mut f = Fixture::new();
    let c = f.add("windows", 3);
    let scrgb = f.clients[c].create_windows_description(false).unwrap();
    let bt2100 = f.clients[c].create_windows_description(true).unwrap();
    f.ready(c, scrgb);
    f.ready(c, bt2100);
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.clients[c]
        .set_image_description(scrgb, INTENT_PERCEPTUAL)
        .unwrap();
    f.clients[c].commit().unwrap();
    f.steps(4);
    let d = f.surface("windows")["image_description"].clone();
    assert_eq!(d["primaries"], "srgb");
    assert_eq!(d["transfer"], "linear");
    // 1.0 is 80 cd/m² from 0; the reference white is assumed at 203.
    assert_eq!(d["luminance"]["min"], 0.0);
    assert_eq!(d["luminance"]["max"], 80.0);
    assert_eq!(d["luminance"]["reference"], 203.0);
    // Neither allows get_information.
    f.clients[c].get_image_information(scrgb).unwrap();
    let error = f.error(c);
    assert!(error.contains("get_information is not allowed"), "{error}");
}

#[test]
fn control_interface_wins_over_the_protocol() {
    let mut f = Fixture::new();
    let c = f.add("override", 3);
    let index = f.clients[c].create_image_description(&pq_bt2020()).unwrap();
    f.ready(c, index);
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.clients[c]
        .set_image_description(index, INTENT_PERCEPTUAL)
        .unwrap();
    f.clients[c].commit().unwrap();
    f.steps(4);
    let surface = f.surface("override")["id"].as_u64().unwrap();
    f.control(Request::SetSurfaceColor {
        surface,
        image_description: "display-p3".into(),
        alpha_mode: None,
    });
    f.steps(2);
    let s = f.surface("override");
    assert_eq!(s["color_source"], "control");
    assert_eq!(s["image_description"]["name"], "display-p3");
    // Later commits from the client do not take it back.
    f.clients[c].commit().unwrap();
    f.steps(4);
    assert_eq!(f.surface("override")["color_source"], "control");
}

#[test]
fn output_description_and_its_changes() {
    let mut f = Fixture::new();
    let c = f.add("output", 3);
    let output = f.clients[c].create_color_output(OUTPUT).unwrap();
    let index = f.clients[c].get_output_image_description(output).unwrap();
    let first = f.ready(c, index);
    let info = f.information(c, index);
    let srgb = [
        640000, 330000, 300000, 600000, 150000, 60000, 312700, 329000,
    ];
    assert_eq!(
        info,
        ImageInfo {
            primaries: Some(srgb),
            primaries_named: Some(PRIMARIES_SRGB),
            tf_named: Some(TF_GAMMA22),
            tf_power: None,
            luminances: Some([2000, 80, 80]),
            target_primaries: Some(srgb),
            target_luminance: Some([2000, 80]),
            target_max_cll: None,
            target_max_fall: None,
        }
    );
    // The same record for the same, unchanged output.
    let again = f.clients[c].get_output_image_description(output).unwrap();
    assert_eq!(f.ready(c, again), first);

    f.clients[c].clear_events();
    f.control(Request::SetOutputImageDescription {
        output: OUTPUT.into(),
        image_description: "bt2020-pq".into(),
    });
    f.settle("told of the change", |f| {
        f.clients[c]
            .events()
            .contains(&ClientEvent::OutputImageDescriptionChanged {
                output: OUTPUT.into(),
            })
    });
    // Followed by wl_output.done.
    let events = f.clients[c].events().to_vec();
    let changed = events
        .iter()
        .position(|e| matches!(e, ClientEvent::OutputImageDescriptionChanged { .. }))
        .unwrap();
    assert!(
        events[changed..]
            .iter()
            .any(|e| matches!(e, ClientEvent::Output { name, .. } if name == OUTPUT)),
        "{events:?}"
    );
    let index = f.clients[c].get_output_image_description(output).unwrap();
    let second = f.ready(c, index);
    assert_ne!(second, first);
    let info = f.information(c, index);
    assert_eq!(info.primaries_named, Some(PRIMARIES_BT2020));
    assert_eq!(info.tf_named, Some(TF_PQ));
    assert_eq!(info.luminances, Some([50, 10000, 203]));
    assert_eq!(info.target_luminance, Some([50, 10000]));
}

// The piecewise sRGB function has no name in version 1: an output driven
// with it cannot be described to such a client.
#[test]
fn version_1_cannot_receive_the_piecewise_srgb_function() {
    let mut f = Fixture::with_output(ImageDescription::DISPLAY_P3);
    let old = f.add("old", 1);
    let new = f.add("new", 2);
    let output = f.clients[old].create_color_output(OUTPUT).unwrap();
    let index = f.clients[old].get_output_image_description(output).unwrap();
    assert!(matches!(
        f.answer(old, index),
        ClientEvent::ImageDescriptionFailed {
            cause: CAUSE_LOW_VERSION,
            ..
        }
    ));
    let output = f.clients[new].create_color_output(OUTPUT).unwrap();
    let index = f.clients[new].get_output_image_description(output).unwrap();
    f.ready(new, index);
    let info = f.information(new, index);
    assert_eq!(info.tf_named, Some(TF_COMPOUND_POWER_2_4));
    assert_eq!(info.primaries_named, Some(PRIMARIES_DISPLAY_P3));
}

#[test]
fn preferred_description_follows_the_output() {
    let mut f = Fixture::new();
    let c = f.add("preferred", 2);
    f.clients[c].create_color_feedback().unwrap();
    f.settle("told the preferred description", |f| {
        f.clients[c]
            .events()
            .iter()
            .any(|e| matches!(e, ClientEvent::PreferredChanged { .. }))
    });
    let announced = |client: &TestClient| {
        client.events().iter().rev().find_map(|e| match e {
            ClientEvent::PreferredChanged { identity } => Some(*identity),
            _ => None,
        })
    };
    let first = announced(&f.clients[c]).unwrap();
    let preferred = f.clients[c].get_preferred(false).unwrap();
    assert_eq!(f.ready(c, preferred), first);
    // It is the output's.
    let output = f.clients[c].create_color_output(OUTPUT).unwrap();
    let of_output = f.clients[c].get_output_image_description(output).unwrap();
    assert_eq!(f.ready(c, of_output), first);
    assert_eq!(f.information(c, preferred).tf_named, Some(TF_GAMMA22));

    f.control(Request::SetOutputImageDescription {
        output: OUTPUT.into(),
        image_description: "display-p3".into(),
    });
    f.settle("told again", |f| announced(&f.clients[c]) != Some(first));
    let second = announced(&f.clients[c]).unwrap();
    let preferred = f.clients[c].get_preferred(true).unwrap();
    assert_eq!(f.ready(c, preferred), second);
    let info = f.information(c, preferred);
    assert_eq!(info.tf_named, Some(TF_COMPOUND_POWER_2_4));
    assert_eq!(info.primaries_named, Some(PRIMARIES_DISPLAY_P3));
}

// What the protocol calls `failed` with `unsupported`: valid, but nothing
// that can be converted.
#[test]
fn degenerate_primaries_fail_gracefully() {
    let mut f = Fixture::new();
    let c = f.add("degenerate", 3);
    let index = f.clients[c]
        .create_image_description(&[
            CreatorStep::TfNamed(TF_GAMMA22),
            CreatorStep::Primaries([
                300000, 300000, 300000, 300000, 300000, 300000, 312700, 329000,
            ]),
        ])
        .unwrap();
    assert!(matches!(
        f.answer(c, index),
        ClientEvent::ImageDescriptionFailed {
            cause: CAUSE_UNSUPPORTED,
            ..
        }
    ));
    // A description that never became ready cannot be set.
    f.clients[c]
        .create_color_surface(ColorTarget::Window)
        .unwrap();
    f.clients[c]
        .set_image_description(index, INTENT_PERCEPTUAL)
        .unwrap();
    let error = f.error(c);
    assert!(
        error.contains("wp_color_management_surface_v1") && error.contains("not ready"),
        "{error}"
    );
}

/// Runs `misuse` on a new client and returns the protocol error it is
/// disconnected with.
fn protocol_error(version: u32, misuse: impl FnOnce(&mut Fixture, usize)) -> String {
    let mut f = Fixture::new();
    let c = f.add("misuse", version);
    misuse(&mut f, c);
    f.error(c)
}

#[test]
fn protocol_errors() {
    const CREATOR: &str = "wp_image_description_creator_params_v1";
    type Misuse = Box<dyn FnOnce(&mut Fixture, usize)>;
    let cases: Vec<(&str, u32, Misuse, &[&str])> = vec![
        (
            "transfer function set twice",
            3,
            Box::new(|f, c| {
                f.clients[c]
                    .create_image_description(&[
                        CreatorStep::TfNamed(TF_PQ),
                        CreatorStep::TfPower(22000),
                    ])
                    .unwrap();
            }),
            &[CREATOR, "set already"],
        ),
        (
            "no primaries",
            3,
            Box::new(|f, c| {
                f.clients[c]
                    .create_image_description(&[CreatorStep::TfNamed(TF_PQ)])
                    .unwrap();
            }),
            &[CREATOR, "required"],
        ),
        (
            "srgb in version 2",
            2,
            Box::new(|f, c| {
                f.clients[c]
                    .create_image_description(&[
                        CreatorStep::TfNamed(TF_SRGB),
                        CreatorStep::PrimariesNamed(PRIMARIES_SRGB),
                    ])
                    .unwrap();
            }),
            &[CREATOR, "not advertised"],
        ),
        (
            "compound_power_2_4 in version 1",
            1,
            Box::new(|f, c| {
                f.clients[c]
                    .create_image_description(&[CreatorStep::TfNamed(TF_COMPOUND_POWER_2_4)])
                    .unwrap();
            }),
            &[CREATOR, "not advertised"],
        ),
        (
            "a transfer function not offered",
            3,
            Box::new(|f, c| {
                f.clients[c]
                    .create_image_description(&[CreatorStep::TfNamed(TF_ST240)])
                    .unwrap();
            }),
            &[CREATOR, "not advertised"],
        ),
        (
            "an exponent below 1",
            3,
            Box::new(|f, c| {
                f.clients[c]
                    .create_image_description(&[CreatorStep::TfPower(5000)])
                    .unwrap();
            }),
            &[CREATOR, "exponent"],
        ),
        (
            "reference white at the black level",
            3,
            Box::new(|f, c| {
                f.clients[c]
                    .create_image_description(&[CreatorStep::Luminances(1_000_000, 200, 100)])
                    .unwrap();
            }),
            &[CREATOR, "above min_lum"],
        ),
        (
            "max_fall above max_cll",
            3,
            Box::new(|f, c| {
                let mut steps = pq_bt2020();
                steps.extend([CreatorStep::MaxCll(100), CreatorStep::MaxFall(200)]);
                f.clients[c].create_image_description(&steps).unwrap();
            }),
            &[CREATOR, "max_fall"],
        ),
        (
            "version 1: max_cll outside the mastering range",
            1,
            Box::new(|f, c| {
                let mut steps = pq_bt2020();
                steps.extend([
                    CreatorStep::MasteringLuminance(0, 1000),
                    CreatorStep::MaxCll(2000),
                ]);
                f.clients[c].create_image_description(&steps).unwrap();
            }),
            &[CREATOR, "mastering luminance range"],
        ),
        (
            "ICC",
            3,
            Box::new(|f, c| f.clients[c].create_icc_creator().unwrap()),
            &["wp_color_manager_v1", "ICC"],
        ),
        (
            "two color surfaces",
            3,
            Box::new(|f, c| {
                f.clients[c]
                    .create_color_surface(ColorTarget::Window)
                    .unwrap();
                f.clients[c]
                    .create_color_surface(ColorTarget::Window)
                    .unwrap();
            }),
            &["wp_color_manager_v1", "already"],
        ),
        (
            "a rendering intent not offered",
            3,
            Box::new(|f, c| {
                let index = f.clients[c].create_image_description(&pq_bt2020()).unwrap();
                f.ready(c, index);
                f.clients[c]
                    .create_color_surface(ColorTarget::Window)
                    .unwrap();
                f.clients[c]
                    .set_image_description(index, INTENT_SATURATION)
                    .unwrap();
            }),
            &["wp_color_management_surface_v1", "rendering intent"],
        ),
        (
            "a surface that is gone",
            3,
            Box::new(|f, c| {
                let index = f.clients[c].create_image_description(&pq_bt2020()).unwrap();
                f.ready(c, index);
                let bare = f.clients[c].create_bare_surface().unwrap();
                f.clients[c]
                    .create_color_surface(ColorTarget::Bare(bare))
                    .unwrap();
                f.clients[c].destroy_bare_surface(bare);
                f.steps(2);
                f.clients[c]
                    .set_image_description(index, INTENT_PERCEPTUAL)
                    .unwrap();
            }),
            &["wp_color_management_surface_v1", "gone"],
        ),
        (
            "information of a client's own description",
            3,
            Box::new(|f, c| {
                let index = f.clients[c].create_image_description(&pq_bt2020()).unwrap();
                f.ready(c, index);
                f.clients[c].get_image_information(index).unwrap();
            }),
            &["wp_image_description_v1", "not allowed"],
        ),
    ];
    for (what, version, misuse, expected) in cases {
        let error = protocol_error(version, misuse);
        for part in expected {
            assert!(error.contains(part), "{what}: {error}");
        }
    }
}

// Within version 2, the light levels need not lie in the mastering range.
#[test]
fn version_2_takes_light_levels_outside_the_mastering_range() {
    let mut f = Fixture::new();
    let c = f.add("levels", 2);
    let mut steps = pq_bt2020();
    steps.extend([
        CreatorStep::MasteringLuminance(0, 1000),
        CreatorStep::MaxCll(2000),
    ]);
    let index = f.clients[c].create_image_description(&steps).unwrap();
    f.ready(c, index);
}
