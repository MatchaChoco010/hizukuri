//! color-management-v1 (`wp_color_manager_v1`): clients describe what the
//! values of their surfaces mean, and learn what the outputs expect.
//!
//! Smithay does not implement this protocol, so the requests are handled
//! here. Everything the protocol describes maps onto
//! [`proto_color::ImageDescription`]; the conversion itself is not done
//! here but by whoever draws (the image description of a surface is part of
//! [`crate::SurfaceNode`]).
//!
//! - Only parametric image descriptions are offered (`parametric`,
//!   `set_primaries`, `set_tf_power`, `set_luminances`,
//!   `set_mastering_display_primaries`, `extended_target_volume`), plus the
//!   two Windows encodings. ICC profiles are not: `icc_v2_v4` is not
//!   advertised, so `create_icc_creator` is the protocol error
//!   `unsupported_feature`.
//! - The image description set on a surface is double-buffered state in
//!   Smithay's cached state of the surface, so it follows
//!   `wl_surface.commit` and the synchronization of subsurfaces.
//! - The preferred image description of a surface is that of the output
//!   the surface is mainly on.

use std::sync::{Arc, Mutex};

use proto_color::{
    AlphaMode, Chromaticity, CompositeSpace, DecodeTransform, ImageDescription, Luminance,
    MasteringDisplay, Primaries, TransferFunction,
};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_icc_v1::{self, WpImageDescriptionCreatorIccV1},
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
    wp_image_description_v1::{self, WpImageDescriptionV1},
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, DisplayHandle, New, Resource, WEnum, Weak,
};
use smithay::wayland::compositor::{Cacheable, with_states};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::state::State;
use crate::surface::with_surface_state;
use crate::{OutputId, RenderIntent};

use wp_color_manager_v1::{
    Feature, Primaries as CmPrimaries, RenderIntent as CmIntent, TransferFunction as CmTf,
};

/// The version of `wp_color_manager_v1` offered.
pub(crate) const VERSION: u32 = 3;

/// An image description record: what a `wp_image_description_v1` refers
/// to. Several protocol objects can share one (an output's description is
/// handed out to every client that asks).
#[derive(Debug)]
pub(crate) struct Record {
    /// Never 0, never reused (the protocol asks that of version 2).
    pub identity: u64,
    pub description: ImageDescription,
}

/// The double-buffered image description of a surface, as set through
/// `wp_color_management_surface_v1`. `None`: not set.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct SurfaceColor {
    pub description: Option<(ImageDescription, RenderIntent)>,
}

impl Cacheable for SurfaceColor {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        // The pending state stays: it is state, not an event.
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// The protocol's side of the state.
pub(crate) struct ColorState {
    _global: GlobalId,
    next_identity: u64,
    /// The preferred image description of surfaces when there is no
    /// output: what surfaces without one are taken to be.
    pub fallback: Arc<Record>,
    /// Live `wp_color_management_output_v1` objects.
    outputs: Vec<WpColorManagementOutputV1>,
    /// Live `wp_color_management_surface_feedback_v1` objects.
    feedbacks: Vec<WpColorManagementSurfaceFeedbackV1>,
    /// `get_information` answers still to be sent.
    pending_info: Vec<(WpImageDescriptionInfoV1, Arc<Record>)>,
}

impl ColorState {
    pub fn new(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<State, WpColorManagerV1, _>(VERSION, ManagerGlobal);
        let mut state = Self {
            _global: global,
            next_identity: 1,
            fallback: Arc::new(Record {
                identity: 0,
                description: crate::DEFAULT_SURFACE_IMAGE_DESCRIPTION,
            }),
            outputs: Vec::new(),
            feedbacks: Vec::new(),
            pending_info: Vec::new(),
        };
        state.fallback = state.record(crate::DEFAULT_SURFACE_IMAGE_DESCRIPTION);
        state
    }

    /// A new record with a fresh identity.
    pub fn record(&mut self, description: ImageDescription) -> Arc<Record> {
        let identity = self.next_identity;
        self.next_identity += 1;
        Arc::new(Record {
            identity,
            description,
        })
    }

    /// Tells the clients that asked about `output` that its image
    /// description changed. `wl_output.done` follows from the caller.
    pub fn output_changed(&self, output: OutputId) {
        for object in &self.outputs {
            if object.data::<OutputData>().and_then(|d| d.output) == Some(output) {
                object.image_description_changed();
            }
        }
    }
}

impl State {
    /// Sends the answers to `get_information` that came in during the last
    /// dispatch.
    pub fn send_image_information(&mut self) {
        for (info, record) in std::mem::take(&mut self.color.pending_info) {
            if info.is_alive() {
                send_information(&info, &record.description);
            }
        }
    }

    /// The record of the image description preferred for a surface: that
    /// of the output the surface is mainly on (the first output of its
    /// window, or the first output for surfaces of no window).
    fn preferred_record(&self, surface: &WlSurface) -> Arc<Record> {
        let output = self
            .owning_window(surface)
            .and_then(|window| self.window_outputs_of(window).first().copied())
            .and_then(|id| self.outputs.iter().find(|o| o.id == id))
            .or_else(|| self.outputs.first());
        match output {
            Some(entry) => entry.color.clone(),
            None => self.color.fallback.clone(),
        }
    }

    /// Sends `preferred_changed` to the feedback objects whose surface now
    /// prefers another image description. Cheap; called whenever outputs
    /// or the outputs of windows may have changed.
    pub fn refresh_color_feedback(&mut self) {
        let feedbacks = self.color.feedbacks.clone();
        for feedback in feedbacks {
            let Some(data) = feedback.data::<FeedbackData>() else {
                continue;
            };
            let Ok(surface) = data.surface.upgrade() else {
                continue;
            };
            let identity = self.preferred_record(&surface).identity;
            let mut sent = data.sent.lock().unwrap_or_else(|e| e.into_inner());
            if *sent != identity {
                *sent = identity;
                send_preferred_changed(&feedback, identity);
            }
        }
    }
}

fn send_preferred_changed(feedback: &WpColorManagementSurfaceFeedbackV1, identity: u64) {
    if feedback.version() >= 2 {
        feedback.preferred_changed2((identity >> 32) as u32, identity as u32);
    } else {
        feedback.preferred_changed(identity as u32);
    }
}

// -- what is advertised -------------------------------------------------------

/// The rendering intents offered. Both mean the same here: white is mapped
/// to white (chromatic adaptation) and what lies outside the output is
/// clipped, which is what media-relative colorimetric asks for and one of
/// the choices perceptual leaves to the compositor.
const INTENTS: [(CmIntent, RenderIntent); 2] = [
    (CmIntent::Perceptual, RenderIntent::Perceptual),
    (CmIntent::Relative, RenderIntent::Relative),
];

fn features(version: u32) -> Vec<Feature> {
    let mut features = vec![
        Feature::Parametric,
        Feature::SetPrimaries,
        Feature::SetTfPower,
        Feature::SetLuminances,
        Feature::SetMasteringDisplayPrimaries,
        // Values outside the primary color volume survive decoding (linear
        // and the mirrored power functions) and the floating point
        // compositing buffer.
        Feature::ExtendedTargetVolume,
        Feature::WindowsScrgb,
    ];
    // The request came with version 3 (the enum entry does not say so).
    if version >= 3 {
        features.push(Feature::WindowsBt2100);
    }
    features
}

/// The named transfer functions offered to a client of `version`, and what
/// they are here. A name deprecated in the bound version is not offered,
/// nor one that came later.
fn transfer_functions(version: u32) -> Vec<(CmTf, TransferFunction)> {
    let mut list = vec![
        (CmTf::Bt1886, TransferFunction::Bt1886),
        (CmTf::Gamma22, TransferFunction::Gamma22),
        (CmTf::Gamma28, TransferFunction::Gamma28),
        (CmTf::ExtLinear, TransferFunction::Linear),
        (CmTf::St2084Pq, TransferFunction::Pq),
        (CmTf::Hlg, TransferFunction::Hlg),
    ];
    if version >= 2 {
        list.push((CmTf::CompoundPower24, TransferFunction::SrgbPiecewise));
    } else {
        // Version 1's ambiguous "sRGB" is taken as the display it is meant
        // for, a 2.2 power function: what surfaces without an image
        // description are taken to be, too.
        list.push((CmTf::Srgb, TransferFunction::Gamma22));
    }
    list
}

const PRIMARIES: [(CmPrimaries, Primaries); 10] = [
    (CmPrimaries::Srgb, Primaries::SRGB),
    (CmPrimaries::PalM, Primaries::PAL_M),
    (CmPrimaries::Pal, Primaries::PAL),
    (CmPrimaries::Ntsc, Primaries::NTSC),
    (CmPrimaries::GenericFilm, Primaries::GENERIC_FILM),
    (CmPrimaries::Bt2020, Primaries::BT2020),
    (CmPrimaries::Cie1931Xyz, Primaries::CIE1931_XYZ),
    (CmPrimaries::DciP3, Primaries::DCI_P3),
    (CmPrimaries::DisplayP3, Primaries::DISPLAY_P3),
    (CmPrimaries::AdobeRgb, Primaries::ADOBE_RGB),
];

// -- image descriptions, sent -------------------------------------------------

/// Answers a new image description object: `ready` (or `ready2`), or
/// `failed` with `cause`.
fn answer(
    object: &WpImageDescriptionV1,
    result: Result<&Record, (wp_image_description_v1::Cause, &str)>,
) {
    match result {
        Ok(record) => {
            if object.version() >= 2 {
                object.ready2((record.identity >> 32) as u32, record.identity as u32);
            } else {
                object.ready(record.identity as u32);
            }
        }
        Err((cause, message)) => object.failed(cause, message.to_string()),
    }
}

/// The transfer function as the info events of `version` carry it, or
/// `None` when that version has no event for it (the piecewise sRGB
/// function has a name only since version 2).
enum InfoTf {
    Named(CmTf),
    Power(u32),
}

fn info_tf(transfer: TransferFunction, version: u32) -> Option<InfoTf> {
    Some(match transfer {
        TransferFunction::Linear => InfoTf::Named(CmTf::ExtLinear),
        TransferFunction::SrgbPiecewise if version >= 2 => InfoTf::Named(CmTf::CompoundPower24),
        TransferFunction::SrgbPiecewise => return None,
        TransferFunction::Gamma22 => InfoTf::Named(CmTf::Gamma22),
        TransferFunction::Gamma28 => InfoTf::Named(CmTf::Gamma28),
        TransferFunction::Gamma24 => InfoTf::Power(24000),
        TransferFunction::Power(g) => InfoTf::Power((g as f64 * 10000.0).round() as u32),
        TransferFunction::Pq => InfoTf::Named(CmTf::St2084Pq),
        TransferFunction::Bt1886 => InfoTf::Named(CmTf::Bt1886),
        TransferFunction::Hlg => InfoTf::Named(CmTf::Hlg),
    })
}

/// Answers an image description that allows `get_information`: an
/// output's, or a preferred one. Fails with `low_version` when the
/// client's version cannot carry it.
fn answer_with_information(object: &WpImageDescriptionV1, record: &Record) {
    if info_tf(record.description.transfer, object.version()).is_none() {
        answer(
            object,
            Err((
                wp_image_description_v1::Cause::LowVersion,
                "the transfer function has no name in this version",
            )),
        );
    } else {
        answer(object, Ok(record));
    }
}

fn coordinate(v: f64) -> i32 {
    (v * 1_000_000.0).round() as i32
}

fn chromaticities(p: &Primaries) -> [i32; 8] {
    [
        coordinate(p.red.x),
        coordinate(p.red.y),
        coordinate(p.green.x),
        coordinate(p.green.y),
        coordinate(p.blue.x),
        coordinate(p.blue.y),
        coordinate(p.white.x),
        coordinate(p.white.y),
    ]
}

fn send_information(info: &WpImageDescriptionInfoV1, description: &ImageDescription) {
    let d = description;
    let [rx, ry, gx, gy, bx, by, wx, wy] = chromaticities(&d.primaries);
    info.primaries(rx, ry, gx, gy, bx, by, wx, wy);
    if let Some((named, _)) = PRIMARIES.iter().find(|(_, p)| *p == d.primaries) {
        info.primaries_named(*named);
    }
    match info_tf(d.transfer, info.version()) {
        Some(InfoTf::Named(tf)) => info.tf_named(tf),
        Some(InfoTf::Power(eexp)) => info.tf_power(eexp),
        // Answered with `low_version` before it came to this.
        None => {}
    }
    let l = &d.luminance;
    let max = match d.transfer {
        TransferFunction::Pq => l.min + 10000.0,
        _ => l.max,
    };
    info.luminances(
        (l.min as f64 * 10000.0).round() as u32,
        max.round() as u32,
        l.reference.round() as u32,
    );
    let (target, target_min, target_max) = match &d.mastering {
        Some(m) => (m.primaries, m.min_luminance, m.max_luminance),
        None => (d.primaries, l.min, max),
    };
    let [rx, ry, gx, gy, bx, by, wx, wy] = chromaticities(&target);
    info.target_primaries(rx, ry, gx, gy, bx, by, wx, wy);
    info.target_luminance(
        (target_min as f64 * 10000.0).round() as u32,
        target_max.round() as u32,
    );
    if let Some(m) = &d.mastering {
        if let Some(cll) = m.max_cll {
            info.target_max_cll(cll.round() as u32);
        }
        if let Some(fall) = m.max_fall {
            info.target_max_fall(fall.round() as u32);
        }
    }
    info.done();
}

// -- wp_color_manager_v1 ------------------------------------------------------

pub(crate) struct ManagerGlobal;

impl GlobalDispatch2<WpColorManagerV1, State> for ManagerGlobal {
    fn bind(
        &self,
        _state: &mut State,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        let manager = data_init.init(resource, ManagerData);
        let version = manager.version();
        for (intent, _) in INTENTS {
            manager.supported_intent(intent);
        }
        for feature in features(version) {
            manager.supported_feature(feature);
        }
        for (tf, _) in transfer_functions(version) {
            manager.supported_tf_named(tf);
        }
        for (primaries, _) in PRIMARIES {
            manager.supported_primaries_named(primaries);
        }
        manager.done();
    }
}

pub(crate) struct ManagerData;

impl Dispatch2<WpColorManagerV1, State> for ManagerData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        manager: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_manager_v1::Request;
        match request {
            Request::Destroy => {}
            Request::GetOutput { id, output } => {
                let output = Output::from_resource(&output)
                    .and_then(|o| state.outputs.iter().find(|e| e.output == o))
                    .map(|e| e.id);
                let object = data_init.init(id, OutputData { output });
                state.color.outputs.push(object);
            }
            Request::GetSurface { id, surface } => {
                let exists = with_states(&surface, |states| {
                    with_surface_state(states, |s| std::mem::replace(&mut s.color_managed, true))
                })
                .unwrap_or(false);
                data_init.init(
                    id,
                    SurfaceData {
                        surface: surface.downgrade(),
                    },
                );
                if exists {
                    manager.post_error(
                        wp_color_manager_v1::Error::SurfaceExists,
                        "the surface has a wp_color_management_surface_v1 already",
                    );
                }
            }
            Request::GetSurfaceFeedback { id, surface } => {
                let identity = state.preferred_record(&surface).identity;
                let feedback = data_init.init(
                    id,
                    FeedbackData {
                        surface: surface.downgrade(),
                        sent: Mutex::new(identity),
                    },
                );
                // Clients ask with get_preferred right away; the event
                // says which record that will be.
                send_preferred_changed(&feedback, identity);
                state.color.feedbacks.push(feedback);
            }
            Request::CreateIccCreator { obj } => {
                data_init.init(obj, IccCreatorData);
                manager.post_error(
                    wp_color_manager_v1::Error::UnsupportedFeature,
                    "ICC profiles are not supported (icc_v2_v4 is not advertised)",
                );
            }
            Request::CreateParametricCreator { obj } => {
                data_init.init(
                    obj,
                    CreatorData {
                        params: Mutex::new(Params::default()),
                    },
                );
            }
            Request::CreateWindowsScrgb { image_description } => {
                // sRGB primaries, extended linear, 1.0 is 80 cd/m² and 0.0
                // is 0 cd/m². The reference white is unknown; the protocol
                // says to assume 203 cd/m² (2.5375).
                let description = ImageDescription {
                    primaries: Primaries::SRGB,
                    transfer: TransferFunction::Linear,
                    luminance: Luminance {
                        min: 0.0,
                        max: 80.0,
                        reference: 203.0,
                    },
                    mastering: None,
                };
                create_fixed(state, image_description, description, data_init);
            }
            Request::CreateWindowsBt2100 { image_description } => {
                // BT.2020 primaries, PQ from 0 cd/m², reference white
                // unknown: assumed at 203 cd/m².
                let description = ImageDescription {
                    primaries: Primaries::BT2020,
                    transfer: TransferFunction::Pq,
                    luminance: Luminance {
                        min: 0.0,
                        max: 10000.0,
                        reference: 203.0,
                    },
                    mastering: None,
                };
                create_fixed(state, image_description, description, data_init);
            }
            Request::GetImageDescription {
                image_description, ..
            } => {
                // References come from other protocols, none of which this
                // compositor implements.
                let object = data_init.init(
                    image_description,
                    DescriptionData {
                        record: None,
                        information: false,
                    },
                );
                answer(
                    &object,
                    Err((
                        wp_image_description_v1::Cause::Unsupported,
                        "no image description references are made here",
                    )),
                );
            }
            _ => {}
        }
    }
}

fn create_fixed(
    state: &mut State,
    id: New<WpImageDescriptionV1>,
    description: ImageDescription,
    data_init: &mut DataInit<'_, State>,
) {
    let record = state.color.record(description);
    let object = data_init.init(
        id,
        DescriptionData {
            record: Some(record.clone()),
            information: false,
        },
    );
    answer(&object, Ok(&record));
}

// -- wp_color_management_output_v1 --------------------------------------------

pub(crate) struct OutputData {
    /// `None` when the output was gone already when the object was made.
    /// An output that goes later is no longer found by its id (ids are
    /// not reused): the object is inert either way.
    output: Option<OutputId>,
}

impl Dispatch2<WpColorManagementOutputV1, State> for OutputData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        _object: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_output_v1::Request;
        if let Request::GetImageDescription { image_description } = request {
            let record = self
                .output
                .and_then(|id| state.outputs.iter().find(|o| o.id == id))
                .map(|o| o.color.clone());
            let object = data_init.init(
                image_description,
                DescriptionData {
                    record: record.clone(),
                    information: true,
                },
            );
            match record {
                Some(record) => answer_with_information(&object, &record),
                None => answer(
                    &object,
                    Err((
                        wp_image_description_v1::Cause::NoOutput,
                        "the output is gone",
                    )),
                ),
            }
        }
    }

    fn destroyed(&self, state: &mut State, _client: ClientId, object: &WpColorManagementOutputV1) {
        state.color.outputs.retain(|o| o != object);
    }
}

// -- wp_color_management_surface_v1 -------------------------------------------

pub(crate) struct SurfaceData {
    surface: Weak<WlSurface>,
}

fn set_pending(surface: &WlSurface, description: Option<(ImageDescription, RenderIntent)>) {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceColor>()
            .pending()
            .description = description;
    });
}

impl Dispatch2<WpColorManagementSurfaceV1, State> for SurfaceData {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        object: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_surface_v1::{Error, Request};
        let surface = match &request {
            Request::Destroy => return,
            _ => match self.surface.upgrade() {
                Ok(surface) => surface,
                Err(_) => {
                    object.post_error(Error::Inert, "the wl_surface is gone");
                    return;
                }
            },
        };
        match request {
            Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                let record = image_description
                    .data::<DescriptionData>()
                    .and_then(|d| d.record.clone());
                let Some(record) = record else {
                    object.post_error(
                        Error::ImageDescription,
                        "the image description is not ready",
                    );
                    return;
                };
                let intent = match render_intent {
                    WEnum::Value(value) => {
                        INTENTS.iter().find(|(i, _)| *i == value).map(|(_, i)| *i)
                    }
                    WEnum::Unknown(_) => None,
                };
                let Some(intent) = intent else {
                    object.post_error(
                        Error::RenderIntent,
                        "the rendering intent is not advertised",
                    );
                    return;
                };
                set_pending(&surface, Some((record.description, intent)));
            }
            Request::UnsetImageDescription => set_pending(&surface, None),
            _ => {}
        }
    }

    fn destroyed(
        &self,
        _state: &mut State,
        _client: ClientId,
        _object: &WpColorManagementSurfaceV1,
    ) {
        // Destroying does what unset_image_description does, and lets the
        // client create another one.
        if let Ok(surface) = self.surface.upgrade() {
            set_pending(&surface, None);
            with_states(&surface, |states| {
                with_surface_state(states, |s| s.color_managed = false)
            });
        }
    }
}

// -- wp_color_management_surface_feedback_v1 ----------------------------------

pub(crate) struct FeedbackData {
    surface: Weak<WlSurface>,
    /// The identity last announced with `preferred_changed`.
    sent: Mutex<u64>,
}

impl Dispatch2<WpColorManagementSurfaceFeedbackV1, State> for FeedbackData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        object: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_surface_feedback_v1::{Error, Request};
        let id = match request {
            Request::GetPreferred { image_description }
            | Request::GetPreferredParametric { image_description } => image_description,
            _ => return,
        };
        let surface = self.surface.upgrade().ok();
        let record = surface.as_ref().map(|s| state.preferred_record(s));
        let description = data_init.init(
            id,
            DescriptionData {
                record: record.clone(),
                information: true,
            },
        );
        match record {
            // Every image description here is parametric.
            Some(record) => answer_with_information(&description, &record),
            None => object.post_error(Error::Inert, "the wl_surface is gone"),
        }
    }

    fn destroyed(
        &self,
        state: &mut State,
        _client: ClientId,
        object: &WpColorManagementSurfaceFeedbackV1,
    ) {
        state.color.feedbacks.retain(|f| f != object);
    }
}

// -- wp_image_description_creator_icc_v1 --------------------------------------

/// Exists only for the moment between `create_icc_creator` and the
/// protocol error that answers it.
pub(crate) struct IccCreatorData;

impl Dispatch2<WpImageDescriptionCreatorIccV1, State> for IccCreatorData {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _object: &WpImageDescriptionCreatorIccV1,
        _request: wp_image_description_creator_icc_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }
}

// -- wp_image_description_creator_params_v1 -----------------------------------

#[derive(Default)]
struct Params {
    transfer: Option<TransferFunction>,
    primaries: Option<Primaries>,
    luminance: Option<Luminance>,
    mastering_primaries: Option<Primaries>,
    mastering_luminance: Option<(f32, f32)>,
    max_cll: Option<f32>,
    max_fall: Option<f32>,
}

pub(crate) struct CreatorData {
    params: Mutex<Params>,
}

fn primaries_from(coordinates: [i32; 8]) -> Primaries {
    let c = |i: usize| {
        Chromaticity::new(
            coordinates[i] as f64 / 1_000_000.0,
            coordinates[i + 1] as f64 / 1_000_000.0,
        )
    };
    Primaries {
        red: c(0),
        green: c(2),
        blue: c(4),
        white: c(6),
    }
}

impl Dispatch2<WpImageDescriptionCreatorParamsV1, State> for CreatorData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        creator: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_image_description_creator_params_v1::{Error, Request};
        let mut params = self.params.lock().unwrap_or_else(|e| e.into_inner());
        let version = creator.version();
        // Sets a property once; a second time is `already_set`.
        macro_rules! set_once {
            ($field:ident, $value:expr, $what:literal) => {
                if params.$field.is_some() {
                    creator.post_error(Error::AlreadySet, concat!($what, " is set already"));
                    return;
                }
                params.$field = Some($value);
            };
        }
        match request {
            Request::SetTfNamed { tf } => {
                let transfer = match tf {
                    WEnum::Value(tf) => transfer_functions(version)
                        .into_iter()
                        .find(|(name, _)| *name == tf)
                        .map(|(_, t)| t),
                    WEnum::Unknown(_) => None,
                };
                let Some(transfer) = transfer else {
                    creator.post_error(Error::InvalidTf, "the transfer function is not advertised");
                    return;
                };
                set_once!(transfer, transfer, "the transfer function");
            }
            Request::SetTfPower { eexp } => {
                if !(10000..=100000).contains(&eexp) {
                    creator.post_error(Error::InvalidTf, "the exponent is not within 1.0 .. 10.0");
                    return;
                }
                set_once!(
                    transfer,
                    TransferFunction::Power(eexp as f32 / 10000.0),
                    "the transfer function"
                );
            }
            Request::SetPrimariesNamed { primaries } => {
                let named = match primaries {
                    WEnum::Value(p) => PRIMARIES.iter().find(|(n, _)| *n == p).map(|(_, p)| *p),
                    WEnum::Unknown(_) => None,
                };
                let Some(named) = named else {
                    creator.post_error(
                        Error::InvalidPrimariesNamed,
                        "the primaries are not advertised",
                    );
                    return;
                };
                set_once!(primaries, named, "the primaries");
            }
            Request::SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                let p = primaries_from([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
                set_once!(primaries, p, "the primaries");
            }
            Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                let min = min_lum as f64 / 10000.0;
                if max_lum as f64 <= min || reference_lum as f64 <= min {
                    creator.post_error(
                        Error::InvalidLuminance,
                        "max_lum and reference_lum must be above min_lum",
                    );
                    return;
                }
                set_once!(
                    luminance,
                    Luminance {
                        min: min as f32,
                        max: max_lum as f32,
                        reference: reference_lum as f32,
                    },
                    "the luminances"
                );
            }
            Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                let p = primaries_from([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]);
                set_once!(mastering_primaries, p, "the mastering display primaries");
            }
            Request::SetMasteringLuminance { min_lum, max_lum } => {
                let min = min_lum as f64 / 10000.0;
                if max_lum as f64 <= min {
                    creator.post_error(Error::InvalidLuminance, "max_lum must be above min_lum");
                    return;
                }
                set_once!(
                    mastering_luminance,
                    (min as f32, max_lum as f32),
                    "the mastering luminance"
                );
            }
            Request::SetMaxCll { max_cll } => {
                set_once!(max_cll, max_cll as f32, "max_cll");
            }
            Request::SetMaxFall { max_fall } => {
                set_once!(max_fall, max_fall as f32, "max_fall");
            }
            Request::Create { image_description } => {
                let (Some(transfer), Some(primaries)) = (params.transfer, params.primaries) else {
                    data_init.init(
                        image_description,
                        DescriptionData {
                            record: None,
                            information: false,
                        },
                    );
                    creator.post_error(
                        Error::IncompleteSet,
                        "the transfer function and the primaries are required",
                    );
                    return;
                };
                let description = match build(&params, transfer, primaries, version) {
                    Ok(description) => description,
                    Err(Invalid::Luminance(message)) => {
                        data_init.init(
                            image_description,
                            DescriptionData {
                                record: None,
                                information: false,
                            },
                        );
                        creator.post_error(Error::InvalidLuminance, message);
                        return;
                    }
                    Err(Invalid::Unsupported(message)) => {
                        let object = data_init.init(
                            image_description,
                            DescriptionData {
                                record: None,
                                information: false,
                            },
                        );
                        answer(
                            &object,
                            Err((wp_image_description_v1::Cause::Unsupported, message)),
                        );
                        return;
                    }
                };
                let record = state.color.record(description);
                let object = data_init.init(
                    image_description,
                    DescriptionData {
                        record: Some(record.clone()),
                        information: false,
                    },
                );
                answer(&object, Ok(&record));
            }
            _ => {}
        }
    }
}

enum Invalid {
    /// The protocol error `invalid_luminance`.
    Luminance(&'static str),
    /// Valid, but not something this compositor can convert: `failed`
    /// with the cause `unsupported`.
    Unsupported(&'static str),
}

/// The image description a complete parameter set stands for.
fn build(
    params: &Params,
    transfer: TransferFunction,
    primaries: Primaries,
    version: u32,
) -> Result<ImageDescription, Invalid> {
    let luminance = match params.luminance {
        // The given maximum is ignored: PQ swings 10000 cd/m².
        Some(l) if transfer == TransferFunction::Pq => Luminance {
            max: l.min + 10000.0,
            ..l
        },
        Some(l) => l,
        // For PQ the default says 10000 for the maximum with a minimum of
        // 0.005; the swing is 10000 all the same (Luminance::swing).
        None => Luminance::default_for(transfer),
    };
    if let (Some(cll), Some(fall)) = (params.max_cll, params.max_fall)
        && fall > cll
    {
        return Err(Invalid::Luminance("max_fall is above max_cll"));
    }
    let (target_min, target_max) = params
        .mastering_luminance
        .unwrap_or((luminance.min, luminance.max));
    if version < 2 {
        // Version 1 also wants the light levels inside the mastering
        // luminance range.
        for level in [params.max_cll, params.max_fall].into_iter().flatten() {
            if level <= target_min || level > target_max {
                return Err(Invalid::Luminance(
                    "max_cll and max_fall must lie within the mastering luminance range",
                ));
            }
        }
    }
    let mastering = (params.mastering_primaries.is_some()
        || params.mastering_luminance.is_some()
        || params.max_cll.is_some()
        || params.max_fall.is_some())
    .then(|| MasteringDisplay {
        primaries: params.mastering_primaries.unwrap_or(primaries),
        min_luminance: target_min,
        max_luminance: target_max,
        max_cll: params.max_cll,
        max_fall: params.max_fall,
    });
    let description = ImageDescription {
        primaries,
        transfer,
        luminance,
        mastering,
    };
    if let Some(m) = &mastering
        && m.primaries.rgb_to_xyz().is_none()
    {
        return Err(Invalid::Unsupported(
            "the mastering display primaries are degenerate",
        ));
    }
    // Whatever can be decoded into the compositing space can be shown.
    if DecodeTransform::new(
        &description,
        AlphaMode::PremultipliedElectrical,
        &CompositeSpace::default(),
    )
    .is_none()
    {
        return Err(Invalid::Unsupported("the primaries are degenerate"));
    }
    Ok(description)
}

// -- wp_image_description_v1 --------------------------------------------------

pub(crate) struct DescriptionData {
    /// `None` when the object failed (it never becomes ready).
    record: Option<Arc<Record>>,
    /// Whether `get_information` is allowed.
    information: bool,
}

impl Dispatch2<WpImageDescriptionV1, State> for DescriptionData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        object: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_image_description_v1::{Error, Request};
        if let Request::GetInformation { information } = request {
            let info = data_init.init(information, InfoData);
            match &self.record {
                None => object.post_error(Error::NotReady, "the image description is not ready"),
                Some(_) if !self.information => object.post_error(
                    Error::NoInformation,
                    "get_information is not allowed for this image description",
                ),
                // Sent after the dispatch: `done` destroys the object,
                // which wayland-server does not allow while the request
                // that creates it is being handled.
                Some(record) => state.color.pending_info.push((info, record.clone())),
            }
        }
    }
}

// -- wp_image_description_info_v1 ---------------------------------------------

pub(crate) struct InfoData;

impl Dispatch2<WpImageDescriptionInfoV1, State> for InfoData {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _object: &WpImageDescriptionInfoV1,
        _request: wp_image_description_info_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }
}
