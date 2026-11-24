//! color-management-v1: telling the compositor what the values of the
//! window mean, and reading what the outputs expect.
//!
//! The client binds `wp_color_manager_v1` only when asked to
//! ([`crate::ClientOptions::color_manager_version`]). Image descriptions
//! are kept in a list; their index is what the methods take and what the
//! events name. Named values are passed as the protocol's numbers, so that
//! a test can also send ones the compositor did not advertise.

use serde::Serialize;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_output_v1, wp_color_management_surface_feedback_v1,
    wp_color_management_surface_v1, wp_color_manager_v1, wp_image_description_creator_icc_v1,
    wp_image_description_creator_params_v1, wp_image_description_info_v1, wp_image_description_v1,
};

use crate::{ClientError, ClientEvent, State, TestClient};

/// One request to a `wp_image_description_creator_params_v1`, with the
/// protocol's values (chromaticities and luminances scaled as the protocol
/// says).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreatorStep {
    TfNamed(u32),
    TfPower(u32),
    PrimariesNamed(u32),
    Primaries([i32; 8]),
    /// min × 10000, max, reference.
    Luminances(u32, u32, u32),
    MasteringPrimaries([i32; 8]),
    /// min × 10000, max.
    MasteringLuminance(u32, u32),
    MaxCll(u32),
    MaxFall(u32),
}

/// Which surface a color management request is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorTarget {
    /// The window's surface.
    Window,
    /// A surface without a role made with [`TestClient::create_bare_surface`].
    Bare(usize),
}

/// What `wp_image_description_info_v1` delivered. `None`: not sent.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ImageInfo {
    pub primaries: Option<[i32; 8]>,
    pub primaries_named: Option<u32>,
    pub tf_named: Option<u32>,
    pub tf_power: Option<u32>,
    pub luminances: Option<[u32; 3]>,
    pub target_primaries: Option<[i32; 8]>,
    pub target_luminance: Option<[u32; 2]>,
    pub target_max_cll: Option<u32>,
    pub target_max_fall: Option<u32>,
}

#[derive(Default)]
pub(crate) struct ColorState {
    pub manager: Option<wp_color_manager_v1::WpColorManagerV1>,
    /// What the manager advertised, until `done`.
    support: Support,
    descriptions: Vec<Option<wp_image_description_v1::WpImageDescriptionV1>>,
    /// Newest last.
    surfaces: Vec<wp_color_management_surface_v1::WpColorManagementSurfaceV1>,
    feedbacks: Vec<wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1>,
    outputs: Vec<wp_color_management_output_v1::WpColorManagementOutputV1>,
    bare: Vec<Option<wl_surface::WlSurface>>,
    infos: Vec<ImageInfo>,
}

#[derive(Default)]
struct Support {
    intents: Vec<u32>,
    features: Vec<u32>,
    transfer_functions: Vec<u32>,
    primaries: Vec<u32>,
}

fn raw<T: Into<u32>>(value: WEnum<T>) -> u32 {
    match value {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(v) => v,
    }
}

fn unknown(what: &str, value: u32) -> ClientError {
    ClientError::Protocol(format!("{what} {value} is not in the protocol"))
}

impl TestClient {
    fn color_manager(&self) -> Result<&wp_color_manager_v1::WpColorManagerV1, ClientError> {
        self.state
            .color
            .manager
            .as_ref()
            .ok_or_else(|| ClientError::Protocol("wp_color_manager_v1 is not bound".to_string()))
    }

    /// The version `wp_color_manager_v1` was bound with, once it is.
    pub fn color_manager_version(&self) -> Option<u32> {
        self.state.color.manager.as_ref().map(|m| m.version())
    }

    fn new_description(&mut self) -> usize {
        self.state.color.descriptions.push(None);
        self.state.color.descriptions.len() - 1
    }

    /// Creates a parametric image description with these requests, in
    /// this order, and `create`. Returns its index; it answers with
    /// [`ClientEvent::ImageDescriptionReady`] or
    /// [`ClientEvent::ImageDescriptionFailed`].
    pub fn create_image_description(
        &mut self,
        steps: &[CreatorStep],
    ) -> Result<usize, ClientError> {
        use wp_color_manager_v1::{Primaries, TransferFunction};
        let creator = self
            .color_manager()?
            .create_parametric_creator(&self.handle, ());
        for step in steps {
            match *step {
                CreatorStep::TfNamed(tf) => creator.set_tf_named(
                    TransferFunction::try_from(tf).map_err(|_| unknown("transfer function", tf))?,
                ),
                CreatorStep::TfPower(eexp) => creator.set_tf_power(eexp),
                CreatorStep::PrimariesNamed(p) => creator.set_primaries_named(
                    Primaries::try_from(p).map_err(|_| unknown("primaries", p))?,
                ),
                CreatorStep::Primaries(c) => {
                    creator.set_primaries(c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7])
                }
                CreatorStep::Luminances(min, max, reference) => {
                    creator.set_luminances(min, max, reference)
                }
                CreatorStep::MasteringPrimaries(c) => creator.set_mastering_display_primaries(
                    c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
                ),
                CreatorStep::MasteringLuminance(min, max) => {
                    creator.set_mastering_luminance(min, max)
                }
                CreatorStep::MaxCll(v) => creator.set_max_cll(v),
                CreatorStep::MaxFall(v) => creator.set_max_fall(v),
            }
        }
        let index = self.new_description();
        let description = creator.create(&self.handle, index);
        self.state.color.descriptions[index] = Some(description);
        Ok(index)
    }

    /// `create_windows_scrgb` (or, with `bt2100`, `create_windows_bt2100`).
    pub fn create_windows_description(&mut self, bt2100: bool) -> Result<usize, ClientError> {
        let index = self.new_description();
        let manager = self.color_manager()?;
        let description = if bt2100 {
            manager.create_windows_bt2100(&self.handle, index)
        } else {
            manager.create_windows_scrgb(&self.handle, index)
        };
        self.state.color.descriptions[index] = Some(description);
        Ok(index)
    }

    /// `create_icc_creator`, which this compositor answers with an error.
    pub fn create_icc_creator(&mut self) -> Result<(), ClientError> {
        let _creator = self.color_manager()?.create_icc_creator(&self.handle, ());
        Ok(())
    }

    /// `get_information` on an image description. The answer arrives as
    /// [`ClientEvent::ImageDescriptionInfo`].
    pub fn get_image_information(&mut self, index: usize) -> Result<(), ClientError> {
        let description = self
            .state
            .color
            .descriptions
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| ClientError::Protocol(format!("no image description {index}")))?;
        description.get_information(&self.handle, index);
        Ok(())
    }

    pub fn destroy_image_description(&mut self, index: usize) {
        if let Some(description) = self
            .state
            .color
            .descriptions
            .get_mut(index)
            .and_then(Option::take)
        {
            description.destroy();
        }
    }

    fn target_surface(&self, target: ColorTarget) -> Result<wl_surface::WlSurface, ClientError> {
        match target {
            ColorTarget::Window => self.state.window.as_ref().map(|w| w.surface.clone()),
            ColorTarget::Bare(index) => self.state.color.bare.get(index).cloned().flatten(),
        }
        .ok_or_else(|| ClientError::Protocol(format!("no surface {target:?}")))
    }

    /// A `wl_surface` without a role or contents, for requests about a
    /// surface that is then destroyed.
    pub fn create_bare_surface(&mut self) -> Result<usize, ClientError> {
        let compositor = self
            .state
            .globals
            .compositor
            .as_ref()
            .ok_or_else(|| ClientError::Protocol("no wl_compositor yet".to_string()))?;
        let surface = compositor.create_surface(&self.handle, ());
        self.state.color.bare.push(Some(surface));
        Ok(self.state.color.bare.len() - 1)
    }

    pub fn destroy_bare_surface(&mut self, index: usize) {
        if let Some(surface) = self.state.color.bare.get_mut(index).and_then(Option::take) {
            surface.destroy();
        }
    }

    /// `get_surface`: a new `wp_color_management_surface_v1`, which the
    /// following requests use.
    pub fn create_color_surface(&mut self, target: ColorTarget) -> Result<(), ClientError> {
        let surface = self.target_surface(target)?;
        let object = self
            .color_manager()?
            .get_surface(&surface, &self.handle, ());
        self.state.color.surfaces.push(object);
        Ok(())
    }

    fn color_surface(
        &self,
    ) -> Result<&wp_color_management_surface_v1::WpColorManagementSurfaceV1, ClientError> {
        self.state
            .color
            .surfaces
            .last()
            .ok_or_else(|| ClientError::Protocol("no wp_color_management_surface_v1".to_string()))
    }

    /// `set_image_description` on the newest color surface, without a
    /// commit. `intent` is the protocol's number (0 is perceptual).
    pub fn set_image_description(&mut self, index: usize, intent: u32) -> Result<(), ClientError> {
        use wp_color_manager_v1::RenderIntent;
        let description = self
            .state
            .color
            .descriptions
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| ClientError::Protocol(format!("no image description {index}")))?;
        let intent = RenderIntent::try_from(intent).map_err(|_| unknown("intent", intent))?;
        self.color_surface()?
            .set_image_description(description, intent);
        Ok(())
    }

    /// `unset_image_description`, without a commit.
    pub fn unset_image_description(&mut self) -> Result<(), ClientError> {
        self.color_surface()?.unset_image_description();
        Ok(())
    }

    /// Destroys the newest color surface (which unsets, at the next
    /// commit).
    pub fn destroy_color_surface(&mut self) {
        if let Some(object) = self.state.color.surfaces.pop() {
            object.destroy();
        }
    }

    /// Commits the window's surface as it is.
    pub fn commit(&mut self) -> Result<(), ClientError> {
        self.target_surface(ColorTarget::Window)?.commit();
        Ok(())
    }

    /// `get_surface_feedback` for the window. Changes of the preferred
    /// image description arrive as [`ClientEvent::PreferredChanged`].
    pub fn create_color_feedback(&mut self) -> Result<(), ClientError> {
        let surface = self.target_surface(ColorTarget::Window)?;
        let feedback = self
            .color_manager()?
            .get_surface_feedback(&surface, &self.handle, ());
        self.state.color.feedbacks.push(feedback);
        Ok(())
    }

    /// `get_preferred` (or `get_preferred_parametric`) on the newest
    /// feedback. Returns the index of the new image description.
    pub fn get_preferred(&mut self, parametric: bool) -> Result<usize, ClientError> {
        let index = self.new_description();
        let feedback = self.state.color.feedbacks.last().ok_or_else(|| {
            ClientError::Protocol("no wp_color_management_surface_feedback_v1".to_string())
        })?;
        let description = if parametric {
            feedback.get_preferred_parametric(&self.handle, index)
        } else {
            feedback.get_preferred(&self.handle, index)
        };
        self.state.color.descriptions[index] = Some(description);
        Ok(index)
    }

    /// `get_output` for the `wl_output` named `name`. Returns the index of
    /// the new object, for [`Self::get_output_image_description`].
    /// [`ClientEvent::OutputImageDescriptionChanged`] tells of changes.
    pub fn create_color_output(&mut self, name: &str) -> Result<usize, ClientError> {
        let id = self
            .state
            .outputs
            .iter()
            .find(|(_, entry)| entry.name == name)
            .map(|(id, _)| id.clone())
            .ok_or_else(|| ClientError::Protocol(format!("no wl_output {name:?}")))?;
        let output = wl_output::WlOutput::from_id(&self.connection, id)
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        let object = self
            .color_manager()?
            .get_output(&output, &self.handle, name.to_string());
        self.state.color.outputs.push(object);
        Ok(self.state.color.outputs.len() - 1)
    }

    /// `get_image_description` on a color output. Returns the index of the
    /// new image description.
    pub fn get_output_image_description(&mut self, output: usize) -> Result<usize, ClientError> {
        let index = self.new_description();
        let object = self.state.color.outputs.get(output).ok_or_else(|| {
            ClientError::Protocol(format!("no wp_color_management_output_v1 {output}"))
        })?;
        let description = object.get_image_description(&self.handle, index);
        self.state.color.descriptions[index] = Some(description);
        Ok(index)
    }
}

impl Dispatch<wp_color_manager_v1::WpColorManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        manager: &wp_color_manager_v1::WpColorManagerV1,
        event: wp_color_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wp_color_manager_v1::Event;
        let support = &mut state.color.support;
        match event {
            Event::SupportedIntent { render_intent } => support.intents.push(raw(render_intent)),
            Event::SupportedFeature { feature } => support.features.push(raw(feature)),
            Event::SupportedTfNamed { tf } => support.transfer_functions.push(raw(tf)),
            Event::SupportedPrimariesNamed { primaries } => support.primaries.push(raw(primaries)),
            Event::Done => {
                let support = std::mem::take(support);
                state.events.push(ClientEvent::ColorManagerSupport {
                    version: manager.version(),
                    intents: support.intents,
                    features: support.features,
                    transfer_functions: support.transfer_functions,
                    primaries: support.primaries,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_image_description_v1::WpImageDescriptionV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &wp_image_description_v1::WpImageDescriptionV1,
        event: wp_image_description_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wp_image_description_v1::Event;
        let index = *index;
        match event {
            Event::Ready { identity } => state.events.push(ClientEvent::ImageDescriptionReady {
                index,
                identity: identity as u64,
                version: 1,
            }),
            Event::Ready2 {
                identity_hi,
                identity_lo,
            } => state.events.push(ClientEvent::ImageDescriptionReady {
                index,
                identity: ((identity_hi as u64) << 32) | identity_lo as u64,
                version: 2,
            }),
            Event::Failed { cause, msg } => {
                state.events.push(ClientEvent::ImageDescriptionFailed {
                    index,
                    cause: raw(cause),
                    message: msg,
                })
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_image_description_info_v1::WpImageDescriptionInfoV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &wp_image_description_info_v1::WpImageDescriptionInfoV1,
        event: wp_image_description_info_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wp_image_description_info_v1::Event;
        let infos = &mut state.color.infos;
        while infos.len() <= *index {
            infos.push(ImageInfo::default());
        }
        let info = &mut infos[*index];
        match event {
            Event::Primaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => info.primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]),
            Event::PrimariesNamed { primaries } => info.primaries_named = Some(raw(primaries)),
            Event::TfNamed { tf } => info.tf_named = Some(raw(tf)),
            Event::TfPower { eexp } => info.tf_power = Some(eexp),
            Event::Luminances {
                min_lum,
                max_lum,
                reference_lum,
            } => info.luminances = Some([min_lum, max_lum, reference_lum]),
            Event::TargetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => info.target_primaries = Some([r_x, r_y, g_x, g_y, b_x, b_y, w_x, w_y]),
            Event::TargetLuminance { min_lum, max_lum } => {
                info.target_luminance = Some([min_lum, max_lum])
            }
            Event::TargetMaxCll { max_cll } => info.target_max_cll = Some(max_cll),
            Event::TargetMaxFall { max_fall } => info.target_max_fall = Some(max_fall),
            Event::Done => {
                let info = std::mem::take(info);
                state.events.push(ClientEvent::ImageDescriptionInfo {
                    index: *index,
                    info,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1, ()>
    for State
{
    fn event(
        state: &mut Self,
        _: &wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1,
        event: wp_color_management_surface_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wp_color_management_surface_feedback_v1::Event;
        let identity = match event {
            Event::PreferredChanged { identity } => identity as u64,
            Event::PreferredChanged2 {
                identity_hi,
                identity_lo,
            } => ((identity_hi as u64) << 32) | identity_lo as u64,
            _ => return,
        };
        state
            .events
            .push(ClientEvent::PreferredChanged { identity });
    }
}

impl Dispatch<wp_color_management_output_v1::WpColorManagementOutputV1, String> for State {
    fn event(
        state: &mut Self,
        _: &wp_color_management_output_v1::WpColorManagementOutputV1,
        event: wp_color_management_output_v1::Event,
        name: &String,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_color_management_output_v1::Event::ImageDescriptionChanged = event {
            state
                .events
                .push(ClientEvent::OutputImageDescriptionChanged {
                    output: name.clone(),
                });
        }
    }
}

delegate_noop!(State: ignore wp_color_management_surface_v1::WpColorManagementSurfaceV1);
delegate_noop!(State: ignore wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1);
delegate_noop!(State: ignore wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1);
