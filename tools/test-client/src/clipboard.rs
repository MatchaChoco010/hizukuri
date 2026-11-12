//! The primary selection (`zwp_primary_selection_device_manager_v1`: what
//! middle click pastes) and data control (`ext_data_control_manager_v1`,
//! `zwlr_data_control_manager_v1`: what clipboard managers use to read and
//! replace the selections without the keyboard focus).
//!
//! Both are opt-in, so that clients of other tests see nothing new.

use wayland_client::backend::ObjectId;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    ext_data_control_source_v1,
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1, zwp_primary_selection_device_v1,
    zwp_primary_selection_offer_v1, zwp_primary_selection_source_v1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use crate::{ClientError, ClientEvent, State, TestClient};

/// Which data control protocol to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataControlProtocol {
    /// `ext_data_control_manager_v1` (wayland-protocols, staging).
    Ext,
    /// `zwlr_data_control_manager_v1` (wlr-protocols).
    Wlr,
}

/// Which selection: the clipboard, or the primary selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionKind {
    Clipboard,
    Primary,
}

#[derive(Clone, PartialEq)]
enum ControlDevice {
    Ext(ext_data_control_device_v1::ExtDataControlDeviceV1),
    Wlr(zwlr_data_control_device_v1::ZwlrDataControlDeviceV1),
}

#[derive(Clone, PartialEq)]
enum ControlOffer {
    Ext(ext_data_control_offer_v1::ExtDataControlOfferV1),
    Wlr(zwlr_data_control_offer_v1::ZwlrDataControlOfferV1),
}

impl ControlOffer {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(offer) => offer.id(),
            Self::Wlr(offer) => offer.id(),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

#[derive(Default)]
pub(crate) struct ClipboardState {
    primary_wanted: bool,
    primary_device: Option<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,
    /// What this client offered as the primary selection, with the text
    /// of each. An offer the compositor ignored stays until cancelled.
    primary_sources: Vec<(
        zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        String,
    )>,
    /// Offers introduced and the types they announced.
    primary_offers: Vec<(
        zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        Vec<String>,
    )>,
    primary_offer: Option<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,

    control_wanted: Option<DataControlProtocol>,
    control: Option<ControlDevice>,
    control_offers: Vec<(ObjectId, Vec<String>)>,
    /// The offers of the current selection and primary selection.
    control_selection: Option<ControlOffer>,
    control_primary: Option<ControlOffer>,
    /// Sources this client set through data control, with their text.
    ext_sources: Vec<(ext_data_control_source_v1::ExtDataControlSourceV1, String)>,
    wlr_sources: Vec<(zwlr_data_control_source_v1::ZwlrDataControlSourceV1, String)>,
}

impl TestClient {
    /// Takes part in the primary selection: offers arrive as
    /// [`ClientEvent::PrimarySelection`] while the client has the keyboard
    /// focus.
    pub fn enable_primary_selection(&mut self) {
        self.state.clipboard.primary_wanted = true;
        self.state.advance_clipboard(&self.handle);
    }

    /// Makes this client the owner of the primary selection (what selecting
    /// text does). It needs the keyboard focus; the latest input serial is
    /// used.
    pub fn set_primary_selection(
        &mut self,
        mime_types: &[&str],
        text: &str,
    ) -> Result<(), ClientError> {
        let state = &mut self.state;
        let (Some(manager), Some(device)) = (
            &state.globals.primary_selection_manager,
            &state.clipboard.primary_device,
        ) else {
            return Err(ClientError::Protocol(
                "no primary selection device (enable_primary_selection first)".to_string(),
            ));
        };
        let source = manager.create_source(&self.handle, ());
        for mime_type in mime_types {
            source.offer(mime_type.to_string());
        }
        device.set_selection(Some(&source), state.last_serial);
        state
            .clipboard
            .primary_sources
            .push((source, text.to_string()));
        Ok(())
    }

    /// Asks for the contents of the primary selection (what middle click
    /// does). The answer arrives as [`ClientEvent::Received`]. Returns
    /// `false` when there is no primary selection on offer.
    pub fn request_primary_selection(&mut self, mime_type: &str) -> Result<bool, ClientError> {
        let Some(offer) = self.state.clipboard.primary_offer.clone() else {
            return Ok(false);
        };
        self.state
            .read_into_event(mime_type, |fd| offer.receive(mime_type.to_string(), fd))?;
        Ok(true)
    }

    /// Binds a data control device: the selections are offered as
    /// [`ClientEvent::ControlSelection`] whatever has the focus, and can be
    /// replaced with [`Self::control_set_selection`].
    pub fn enable_data_control(&mut self, protocol: DataControlProtocol) {
        self.state.clipboard.control_wanted = Some(protocol);
        self.state.advance_clipboard(&self.handle);
    }

    /// Whether the data control device exists (the global was there).
    pub fn has_data_control(&self) -> bool {
        self.state.clipboard.control.is_some()
    }

    /// Replaces a selection through data control, without the focus.
    pub fn control_set_selection(
        &mut self,
        which: SelectionKind,
        mime_types: &[&str],
        text: &str,
    ) -> Result<(), ClientError> {
        let state = &mut self.state;
        let no_device = || ClientError::Protocol("no data control device".to_string());
        match state.clipboard.control.clone().ok_or_else(no_device)? {
            ControlDevice::Ext(device) => {
                let manager = state
                    .globals
                    .ext_data_control_manager
                    .as_ref()
                    .ok_or_else(no_device)?;
                let source = manager.create_data_source(&self.handle, ());
                for mime_type in mime_types {
                    source.offer(mime_type.to_string());
                }
                match which {
                    SelectionKind::Clipboard => device.set_selection(Some(&source)),
                    SelectionKind::Primary => device.set_primary_selection(Some(&source)),
                }
                state.clipboard.ext_sources.push((source, text.to_string()));
            }
            ControlDevice::Wlr(device) => {
                let manager = state
                    .globals
                    .wlr_data_control_manager
                    .as_ref()
                    .ok_or_else(no_device)?;
                let source = manager.create_data_source(&self.handle, ());
                for mime_type in mime_types {
                    source.offer(mime_type.to_string());
                }
                match which {
                    SelectionKind::Clipboard => device.set_selection(Some(&source)),
                    SelectionKind::Primary => device.set_primary_selection(Some(&source)),
                }
                state.clipboard.wlr_sources.push((source, text.to_string()));
            }
        }
        Ok(())
    }

    /// Reads a selection through data control. The answer arrives as
    /// [`ClientEvent::Received`]. Returns `false` when nothing is on offer.
    pub fn control_request_selection(
        &mut self,
        which: SelectionKind,
        mime_type: &str,
    ) -> Result<bool, ClientError> {
        let offer = match which {
            SelectionKind::Clipboard => self.state.clipboard.control_selection.clone(),
            SelectionKind::Primary => self.state.clipboard.control_primary.clone(),
        };
        let Some(offer) = offer else {
            return Ok(false);
        };
        self.state.read_into_event(mime_type, |fd| match &offer {
            ControlOffer::Ext(offer) => offer.receive(mime_type.to_string(), fd),
            ControlOffer::Wlr(offer) => offer.receive(mime_type.to_string(), fd),
        })?;
        Ok(true)
    }
}

impl State {
    /// Creates the devices that were asked for, once their globals and the
    /// seat are bound.
    pub(crate) fn advance_clipboard(&mut self, handle: &QueueHandle<State>) {
        let Some(seat) = self.globals.seat.clone() else {
            return;
        };
        let clipboard = &mut self.clipboard;
        if clipboard.primary_wanted
            && clipboard.primary_device.is_none()
            && let Some(manager) = &self.globals.primary_selection_manager
        {
            clipboard.primary_device = Some(manager.get_device(&seat, handle, ()));
        }
        if clipboard.control.is_none() {
            clipboard.control = match clipboard.control_wanted {
                Some(DataControlProtocol::Ext) => self
                    .globals
                    .ext_data_control_manager
                    .as_ref()
                    .map(|manager| ControlDevice::Ext(manager.get_data_device(&seat, handle, ()))),
                Some(DataControlProtocol::Wlr) => self
                    .globals
                    .wlr_data_control_manager
                    .as_ref()
                    .map(|manager| ControlDevice::Wlr(manager.get_data_device(&seat, handle, ()))),
                None => None,
            };
        }
    }

    fn control_types(&self, id: &ObjectId) -> Vec<String> {
        self.clipboard
            .control_offers
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, types)| types.clone())
            .unwrap_or_default()
    }

    /// A data control device announced a new selection (or none).
    fn control_selection(
        &mut self,
        protocol: DataControlProtocol,
        which: SelectionKind,
        offer: Option<ControlOffer>,
    ) {
        let mime_types = offer
            .as_ref()
            .map(|o| self.control_types(&o.id()))
            .unwrap_or_default();
        let slot = match which {
            SelectionKind::Clipboard => &mut self.clipboard.control_selection,
            SelectionKind::Primary => &mut self.clipboard.control_primary,
        };
        let old = std::mem::replace(slot, offer.clone());
        // An offer can stand for both selections; only drop one that is
        // used by neither any more.
        if let Some(old) = old
            && Some(&old) != offer.as_ref()
            && self.clipboard.control_selection.as_ref() != Some(&old)
            && self.clipboard.control_primary.as_ref() != Some(&old)
        {
            self.clipboard
                .control_offers
                .retain(|(known, _)| *known != old.id());
            old.destroy();
        }
        self.events.push(ClientEvent::ControlSelection {
            protocol,
            selection: which,
            mime_types,
        });
    }
}

impl Dispatch<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_primary_selection_device_v1::Event;
        match event {
            Event::DataOffer { offer } => state.clipboard.primary_offers.push((offer, Vec::new())),
            Event::Selection { id } => {
                let mime_types = id
                    .as_ref()
                    .and_then(|offer| {
                        state
                            .clipboard
                            .primary_offers
                            .iter()
                            .find(|(known, _)| known == offer)
                            .map(|(_, types)| types.clone())
                    })
                    .unwrap_or_default();
                if let Some(old) = state.clipboard.primary_offer.take()
                    && Some(&old) != id.as_ref()
                {
                    state
                        .clipboard
                        .primary_offers
                        .retain(|(known, _)| known != &old);
                    old.destroy();
                }
                state.clipboard.primary_offer = id;
                state
                    .events
                    .push(ClientEvent::PrimarySelection { mime_types });
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, types)) = state
                .clipboard
                .primary_offers
                .iter_mut()
                .find(|(known, _)| known == offer)
        {
            types.push(mime_type);
        }
    }
}

impl Dispatch<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_primary_selection_source_v1::Event;
        match event {
            Event::Send { mime_type, fd } => {
                if let Some((_, text)) = state
                    .clipboard
                    .primary_sources
                    .iter()
                    .find(|(known, _)| known == source)
                {
                    crate::features::write_text(fd, text);
                }
                state
                    .events
                    .push(ClientEvent::PrimarySourceSend { mime_type });
            }
            Event::Cancelled => {
                state
                    .clipboard
                    .primary_sources
                    .retain(|(known, _)| known != source);
                source.destroy();
                state.events.push(ClientEvent::PrimarySourceCancelled);
            }
            _ => {}
        }
    }
}

impl Dispatch<ext_data_control_device_v1::ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_data_control_device_v1::ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        let protocol = DataControlProtocol::Ext;
        match event {
            Event::DataOffer { id } => state.clipboard.control_offers.push((id.id(), Vec::new())),
            Event::Selection { id } => state.control_selection(
                protocol,
                SelectionKind::Clipboard,
                id.map(ControlOffer::Ext),
            ),
            Event::PrimarySelection { id } => {
                state.control_selection(protocol, SelectionKind::Primary, id.map(ControlOffer::Ext))
            }
            Event::Finished => state.events.push(ClientEvent::ControlFinished),
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ext_data_control_device_v1::ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ext_data_control_offer_v1::ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        let protocol = DataControlProtocol::Wlr;
        match event {
            Event::DataOffer { id } => state.clipboard.control_offers.push((id.id(), Vec::new())),
            Event::Selection { id } => state.control_selection(
                protocol,
                SelectionKind::Clipboard,
                id.map(ControlOffer::Wlr),
            ),
            Event::PrimarySelection { id } => {
                state.control_selection(protocol, SelectionKind::Primary, id.map(ControlOffer::Wlr))
            }
            Event::Finished => state.events.push(ClientEvent::ControlFinished),
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ext_data_control_offer_v1::ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ext_data_control_offer_v1::ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.control_offer_type(offer.id(), mime_type);
        }
    }
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.control_offer_type(offer.id(), mime_type);
        }
    }
}

impl State {
    fn control_offer_type(&mut self, offer: ObjectId, mime_type: String) {
        if let Some((_, types)) = self
            .clipboard
            .control_offers
            .iter_mut()
            .find(|(known, _)| *known == offer)
        {
            types.push(mime_type);
        }
    }

    /// A source set through data control was asked for its data, or was
    /// replaced.
    fn control_source_event(&mut self, id: ObjectId, send: Option<(String, std::os::fd::OwnedFd)>) {
        let text = self
            .clipboard
            .ext_sources
            .iter()
            .find(|(source, _)| source.id() == id)
            .map(|(_, text)| text.clone())
            .or_else(|| {
                self.clipboard
                    .wlr_sources
                    .iter()
                    .find(|(source, _)| source.id() == id)
                    .map(|(_, text)| text.clone())
            });
        match send {
            Some((mime_type, fd)) => {
                if let Some(text) = text {
                    crate::features::write_text(fd, &text);
                }
                self.events
                    .push(ClientEvent::ControlSourceSend { mime_type });
            }
            None => {
                self.clipboard.ext_sources.retain(|(source, _)| {
                    let gone = source.id() == id;
                    if gone {
                        source.destroy();
                    }
                    !gone
                });
                self.clipboard.wlr_sources.retain(|(source, _)| {
                    let gone = source.id() == id;
                    if gone {
                        source.destroy();
                    }
                    !gone
                });
                self.events.push(ClientEvent::ControlSourceCancelled);
            }
        }
    }
}

impl Dispatch<ext_data_control_source_v1::ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ext_data_control_source_v1::ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.control_source_event(source.id(), Some((mime_type, fd)))
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.control_source_event(source.id(), None)
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.control_source_event(source.id(), Some((mime_type, fd)))
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.control_source_event(source.id(), None)
            }
            _ => {}
        }
    }
}

delegate_noop!(State: zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1);
delegate_noop!(State: ext_data_control_manager_v1::ExtDataControlManagerV1);
delegate_noop!(State: zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
