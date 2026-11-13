//! The parent's selections: the nested backend as a client of the parent's
//! `wl_data_device` and `zwp_primary_selection_device_v1`.
//!
//! What a client inside copies is set in the parent as a source of ours
//! with the same MIME types; when something in the parent reads it, the
//! reader's pipe goes to the core ([`SelectionEvent::Wanted`]), which hands
//! it to the owner inside. What the parent announces is offered inside
//! ([`SelectionEvent::Outer`]); when a client inside reads it, its pipe is
//! handed to the parent's offer as it is
//! ([`State::receive_outer_selection`]). Nothing here reads or writes the
//! data. When to do what is decided by [`SelectionBridge`].

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Mutex;

use wayland_client::protocol::{
    wl_callback, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_display,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1, zwp_primary_selection_device_v1,
    zwp_primary_selection_offer_v1, zwp_primary_selection_source_v1,
};

use super::bridge::{Action, SelectionBridge};
use super::client::State;
use crate::BackendEvent;
use crate::selection::{SelectionCrossing, SelectionEvent, SelectionKind, TransferFd};

/// The MIME types an offer of the parent announced so far.
type OfferTypes = Mutex<Vec<String>>;

enum Source {
    Data(wl_data_source::WlDataSource),
    Primary(zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1),
}

impl Source {
    fn destroy(&self) {
        match self {
            Source::Data(source) => source.destroy(),
            Source::Primary(source) => source.destroy(),
        }
    }
}

enum Offer {
    Data(wl_data_offer::WlDataOffer),
    Primary(zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1),
}

impl Offer {
    fn destroy(&self) {
        match self {
            Offer::Data(offer) => offer.destroy(),
            Offer::Primary(offer) => offer.destroy(),
        }
    }

    fn receive(&self, mime_type: &str, fd: &OwnedFd) {
        // The descriptor is duplicated into the request.
        match self {
            Offer::Data(offer) => offer.receive(mime_type.to_string(), fd.as_fd()),
            Offer::Primary(offer) => offer.receive(mime_type.to_string(), fd.as_fd()),
        }
    }
}

/// The source behind a sync that follows setting it.
#[derive(Debug, Clone, Copy)]
pub(super) struct SourceSync {
    kind: SelectionKind,
    token: u64,
}

/// A source of ours in the parent.
#[derive(Debug, Clone, Copy)]
pub(super) struct SourceData {
    kind: SelectionKind,
    token: u64,
}

/// One selection as the parent sees it.
struct Side {
    bridge: SelectionBridge,
    /// Our sources the parent has not cancelled, by token.
    sources: Vec<(u64, Source)>,
    /// The parent's offer that is offered inside, with its generation.
    imported: Option<(u64, Offer)>,
}

impl Side {
    fn new(crossing: SelectionCrossing) -> Self {
        Self {
            bridge: SelectionBridge::new(crossing.to_outside, crossing.from_outside),
            sources: Vec::new(),
            imported: None,
        }
    }
}

/// The selections of the nested window in the parent.
pub(super) struct Selections {
    crossing: SelectionCrossing,
    primary_wanted: bool,
    display: Option<wl_display::WlDisplay>,
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    primary_manager:
        Option<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,
    data_device: Option<wl_data_device::WlDataDevice>,
    primary_device: Option<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,
    clipboard: Side,
    primary: Side,
    /// The offer of a drag over the window: never accepted.
    drag: Option<wl_data_offer::WlDataOffer>,
    /// The serial of the latest input event of the parent: what a client
    /// of the parent sets a selection with.
    pub serial: u32,
}

impl Default for Selections {
    fn default() -> Self {
        Self::new(SelectionCrossing::NONE, false)
    }
}

impl Selections {
    pub fn new(crossing: SelectionCrossing, primary: bool) -> Self {
        Self {
            crossing,
            primary_wanted: primary,
            display: None,
            data_manager: None,
            primary_manager: None,
            data_device: None,
            primary_device: None,
            clipboard: Side::new(crossing),
            primary: Side::new(if primary {
                crossing
            } else {
                SelectionCrossing::NONE
            }),
            drag: None,
            serial: 0,
        }
    }

    /// Before the globals are bound: which way selections cross, and the
    /// display for syncs.
    pub fn configure(
        &mut self,
        crossing: SelectionCrossing,
        primary: bool,
        display: wl_display::WlDisplay,
    ) {
        *self = Self::new(crossing, primary);
        self.display = Some(display);
    }

    fn side(&mut self, kind: SelectionKind) -> &mut Side {
        match kind {
            SelectionKind::Clipboard => &mut self.clipboard,
            SelectionKind::Primary => &mut self.primary,
        }
    }

    fn has_device(&self, kind: SelectionKind) -> bool {
        match kind {
            SelectionKind::Clipboard => self.data_device.is_some(),
            SelectionKind::Primary => self.primary_device.is_some(),
        }
    }

    /// For the state dump.
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "crossing": self.crossing.name(),
            "clipboard": self.data_device.as_ref().map(|_| self.clipboard.bridge.describe()),
            "primary": self.primary_device.as_ref().map(|_| self.primary.bridge.describe()),
        })
    }
}

impl State {
    /// Binds the selection globals of the parent that are to be used.
    /// Returns whether `interface` was one of them.
    pub(super) fn bind_selection_global(
        &mut self,
        registry: &wayland_client::protocol::wl_registry::WlRegistry,
        name: u32,
        interface: &str,
        version: u32,
        qh: &QueueHandle<Self>,
    ) -> bool {
        let selections = &mut self.selections;
        let crossing = selections.crossing != SelectionCrossing::NONE;
        match interface {
            "wl_data_device_manager" if crossing => {
                selections.data_manager = Some(registry.bind(name, version.min(3), qh, ()));
            }
            "zwp_primary_selection_device_manager_v1" if crossing && selections.primary_wanted => {
                selections.primary_manager = Some(registry.bind(name, 1, qh, ()));
            }
            _ => return false,
        }
        self.ensure_selection_devices(qh);
        true
    }

    /// Creates the devices once the managers and the seat are there.
    pub(super) fn ensure_selection_devices(&mut self, qh: &QueueHandle<Self>) {
        let Some(seat) = self.seat.clone() else {
            return;
        };
        let selections = &mut self.selections;
        if selections.data_device.is_none()
            && let Some(manager) = &selections.data_manager
        {
            selections.data_device = Some(manager.get_data_device(&seat, qh, ()));
        }
        if selections.primary_device.is_none()
            && let Some(manager) = &selections.primary_manager
        {
            selections.primary_device = Some(manager.get_device(&seat, qh, ()));
        }
    }

    /// The window got or lost the parent's keyboard focus.
    pub(super) fn selection_focus(&mut self, focused: bool) {
        for kind in SelectionKind::ALL {
            if !self.selections.has_device(kind) {
                continue;
            }
            let actions = self.selections.side(kind).bridge.focus(focused);
            self.apply_selection(kind, actions, None);
        }
    }

    /// A client inside set the selection, or there is none any more.
    pub(super) fn inner_selection_changed(
        &mut self,
        kind: SelectionKind,
        mime_types: Option<Vec<String>>,
    ) {
        if !self.selections.has_device(kind) {
            return;
        }
        let actions = self.selections.side(kind).bridge.inner_changed(mime_types);
        self.apply_selection(kind, actions, None);
    }

    /// A client inside reads the parent's selection `generation`: the
    /// parent's offer is asked to have it written into `fd`.
    pub(super) fn receive_outer_selection(
        &mut self,
        kind: SelectionKind,
        generation: u64,
        mime_type: &str,
        fd: OwnedFd,
    ) -> bool {
        let side = self.selections.side(kind);
        match &side.imported {
            Some((current, offer)) if *current == generation => {
                offer.receive(mime_type, &fd);
                // Ours is closed; the parent's owner holds the one sent.
                drop(fd);
                true
            }
            _ => false,
        }
    }

    /// Does what the bridge asked for. `offer` is the parent's offer the
    /// actions answer (an announced selection): kept when it is to be
    /// offered inside, destroyed otherwise.
    fn apply_selection(&mut self, kind: SelectionKind, actions: Vec<Action>, offer: Option<Offer>) {
        let mut offer = offer;
        for action in actions {
            match action {
                Action::SetParent { token, mime_types } => {
                    self.set_parent_selection(kind, token, &mime_types)
                }
                Action::Withdraw { token } => {
                    let side = self.selections.side(kind);
                    if let Some(index) = side.sources.iter().position(|(t, _)| *t == token) {
                        let (_, source) = side.sources.remove(index);
                        source.destroy();
                    }
                }
                Action::SetInner {
                    generation,
                    mime_types,
                } => {
                    let Some(new) = offer.take() else {
                        continue;
                    };
                    let side = self.selections.side(kind);
                    if let Some((_, old)) = side.imported.replace((generation, new)) {
                        old.destroy();
                    }
                    self.events
                        .push_back(BackendEvent::Selection(SelectionEvent::Outer {
                            kind,
                            mime_types: Some(mime_types),
                            generation,
                        }));
                }
                Action::ClearInner => {
                    let side = self.selections.side(kind);
                    if let Some((_, old)) = side.imported.take() {
                        old.destroy();
                    }
                    self.events
                        .push_back(BackendEvent::Selection(SelectionEvent::Outer {
                            kind,
                            mime_types: None,
                            generation: 0,
                        }));
                }
            }
        }
        if let Some(offer) = offer {
            offer.destroy();
        }
    }

    fn set_parent_selection(&mut self, kind: SelectionKind, token: u64, mime_types: &[String]) {
        let qh = self.qh.clone();
        let serial = self.selections.serial;
        let selections = &mut self.selections;
        let data = SourceData { kind, token };
        let source = match kind {
            SelectionKind::Clipboard => {
                let (Some(manager), Some(device)) =
                    (&selections.data_manager, &selections.data_device)
                else {
                    return;
                };
                let source = manager.create_data_source(&qh, data);
                for mime_type in mime_types {
                    source.offer(mime_type.clone());
                }
                device.set_selection(Some(&source), serial);
                Source::Data(source)
            }
            SelectionKind::Primary => {
                let (Some(manager), Some(device)) =
                    (&selections.primary_manager, &selections.primary_device)
                else {
                    return;
                };
                let source = manager.create_source(&qh, data);
                for mime_type in mime_types {
                    source.offer(mime_type.clone());
                }
                device.set_selection(Some(&source), serial);
                Source::Primary(source)
            }
        };
        tracing::debug!(
            kind = kind.name(),
            ?mime_types,
            "offering the selection inside to the parent"
        );
        selections.side(kind).sources.push((token, source));
        // The parent echoes before it answers this when it took the source.
        if let Some(display) = &selections.display {
            display.sync(&qh, SourceSync { kind, token });
        }
    }

    /// The parent announced a selection (its offer and types), or none.
    fn parent_selection(
        &mut self,
        kind: SelectionKind,
        offer: Option<Offer>,
        types: Option<Vec<String>>,
    ) {
        let actions = self.selections.side(kind).bridge.parent_changed(types);
        self.apply_selection(kind, actions, offer);
    }

    /// Someone in the parent reads source `data`.
    fn source_send(&mut self, data: SourceData, mime_type: String, fd: OwnedFd) {
        if !self
            .selections
            .side(data.kind)
            .bridge
            .is_current(data.token)
        {
            // A source the parent still had for a moment: nothing to read.
            return;
        }
        self.events
            .push_back(BackendEvent::Selection(SelectionEvent::Wanted {
                kind: data.kind,
                mime_type,
                fd: TransferFd::new(fd),
            }));
    }

    /// The parent cancelled source `data`.
    fn source_cancelled(&mut self, data: SourceData) {
        let side = self.selections.side(data.kind);
        side.bridge.cancelled(data.token);
        if let Some(index) = side.sources.iter().position(|(t, _)| *t == data.token) {
            let (_, source) = side.sources.remove(index);
            source.destroy();
        }
    }
}

fn types_of<I: Proxy>(offer: &I) -> Vec<String> {
    offer
        .data::<OfferTypes>()
        .map(|types| types.lock().unwrap().clone())
        .unwrap_or_default()
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_data_device_manager::WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1, ()>
    for State
{
    fn event(
        _: &mut Self,
        _: &zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
        _: zwp_primary_selection_device_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_device::Event;
        match event {
            Event::Selection { id } => {
                let types = id.as_ref().map(types_of);
                state.parent_selection(SelectionKind::Clipboard, id.map(Offer::Data), types);
            }
            // Drags from the parent are not taken (not accepted, so the
            // parent drops nothing here).
            Event::Enter { id, .. } => {
                if let Some(old) = std::mem::replace(&mut state.selections.drag, id) {
                    old.destroy();
                }
            }
            Event::Leave | Event::Drop => {
                if let Some(offer) = state.selections.drag.take() {
                    offer.destroy();
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, OfferTypes::default()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferTypes> for State {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        types: &OfferTypes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            types.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, SourceData> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                state.source_send(*data, mime_type, fd)
            }
            wl_data_source::Event::Cancelled => state.source_cancelled(*data),
            _ => {}
        }
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
        if let zwp_primary_selection_device_v1::Event::Selection { id } = event {
            let types = id.as_ref().map(types_of);
            state.parent_selection(SelectionKind::Primary, id.map(Offer::Primary), types);
        }
    }

    wayland_client::event_created_child!(State, zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, OfferTypes::default()),
    ]);
}

impl Dispatch<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, OfferTypes> for State {
    fn event(
        _: &mut Self,
        _: &zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        types: &OfferTypes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event {
            types.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1, SourceData> for State {
    fn event(
        state: &mut Self,
        _: &zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_source_v1::Event::Send { mime_type, fd } => {
                state.source_send(*data, mime_type, fd)
            }
            zwp_primary_selection_source_v1::Event::Cancelled => state.source_cancelled(*data),
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, SourceSync> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        sync: &SourceSync,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            let actions = state
                .selections
                .side(sync.kind)
                .bridge
                .confirmed(sync.token);
            state.apply_selection(sync.kind, actions, None);
        }
    }
}
