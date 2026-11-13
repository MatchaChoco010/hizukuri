//! The selections (clipboard and primary selection) as the caller takes
//! part in them.
//!
//! Clients own selections: one copies, another pastes, and Smithay passes
//! the reader's pipe to the owner. The caller hears of each selection a
//! client sets ([`SelectionEvent::Changed`]), and can own one itself
//! ([`WlServer::provide_selection`]): for something outside the clients
//! (the parent compositor of a nested session, or the compositor itself).
//! Reads of that one come to the caller ([`SelectionEvent::ProvidedWanted`])
//! with the reader's pipe, to be handed on. The caller can also have the
//! owner of a client's selection write into a pipe of its own
//! ([`WlServer::write_selection`]). Data never passes through here.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use smithay::wayland::selection::{SelectionTarget, data_device, primary_selection};

use crate::state::State;
use crate::{WlEvent, WlServer};

/// Which selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionKind {
    /// What copy and paste use (`wl_data_device`, and data control).
    Clipboard,
    /// What selecting text and a middle click use
    /// (`zwp_primary_selection_device_v1`, and data control).
    Primary,
}

impl SelectionKind {
    pub fn name(self) -> &'static str {
        match self {
            SelectionKind::Clipboard => "clipboard",
            SelectionKind::Primary => "primary",
        }
    }

    fn of(target: SelectionTarget) -> Self {
        match target {
            SelectionTarget::Clipboard => SelectionKind::Clipboard,
            SelectionTarget::Primary => SelectionKind::Primary,
        }
    }
}

/// The write end of a transfer, handed through an event (events are
/// `Clone` and `PartialEq`, which a descriptor is not): copies share it,
/// and two are equal when they are the same one.
#[derive(Debug, Clone)]
pub struct TransferFd(Arc<OwnedFd>);

impl TransferFd {
    pub fn new(fd: OwnedFd) -> Self {
        Self(Arc::new(fd))
    }

    /// The descriptor, to hand it on. A duplicate when a copy of the event
    /// still holds it; it is closed when the last one goes.
    pub fn into_owned(self) -> std::io::Result<OwnedFd> {
        match Arc::try_unwrap(self.0) {
            Ok(fd) => Ok(fd),
            Err(shared) => shared.try_clone(),
        }
    }
}

impl PartialEq for TransferFd {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What happened to a selection.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionEvent {
    /// A client set the selection (these types, in the order offered),
    /// cleared it, or its owner went away (`None`). Not sent for a
    /// selection the caller provides.
    Changed {
        kind: SelectionKind,
        mime_types: Option<Vec<String>>,
    },
    /// A client reads the selection the caller provides as `id`
    /// ([`WlServer::provide_selection`]), as `mime_type`: what is to be
    /// read goes into `fd`, which is then closed.
    ProvidedWanted {
        kind: SelectionKind,
        id: u64,
        mime_type: String,
        fd: TransferFd,
    },
}

/// Who owns a selection now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOwner {
    None,
    Client,
    /// The caller ([`WlServer::provide_selection`]).
    Provided,
}

impl SelectionOwner {
    pub fn name(self) -> &'static str {
        match self {
            SelectionOwner::None => "none",
            SelectionOwner::Client => "client",
            SelectionOwner::Provided => "provided",
        }
    }
}

/// Why [`WlServer::write_selection`] did not hand the pipe on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelectionWriteError {
    #[error("no client owns the selection")]
    NoSelection,
    #[error("the selection is the one provided by the caller")]
    Provided,
    #[error("the owner does not offer that type")]
    NotOffered,
}

/// The data Smithay keeps with a selection the caller provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvidedSelection {
    pub id: u64,
}

impl State {
    /// A client set or cleared a selection (from `new_selection`).
    pub(crate) fn client_selection_changed(
        &mut self,
        target: SelectionTarget,
        mime_types: Option<Vec<String>>,
    ) {
        self.events
            .push(WlEvent::Selection(SelectionEvent::Changed {
                kind: SelectionKind::of(target),
                mime_types,
            }));
    }

    /// A client reads a selection the caller provides (from
    /// `send_selection`).
    pub(crate) fn provided_selection_wanted(
        &mut self,
        target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        provided: &ProvidedSelection,
    ) {
        self.events
            .push(WlEvent::Selection(SelectionEvent::ProvidedWanted {
                kind: SelectionKind::of(target),
                id: provided.id,
                mime_type,
                fd: TransferFd::new(fd),
            }));
    }

    fn provided(&self, kind: SelectionKind) -> Option<ProvidedSelection> {
        match kind {
            SelectionKind::Clipboard => {
                data_device::current_data_device_selection_userdata(&self.seat).map(|d| *d)
            }
            SelectionKind::Primary => {
                primary_selection::current_primary_selection_userdata(&self.seat).map(|d| *d)
            }
        }
    }

    fn noted(&mut self, kind: SelectionKind) -> &mut Option<Vec<String>> {
        match kind {
            SelectionKind::Clipboard => &mut self.selection,
            SelectionKind::Primary => &mut self.primary_selection,
        }
    }

    /// Smithay drops a client's selection silently when its source goes
    /// (the client destroys it or disconnects). Called after the clients'
    /// requests were handled, the only time that can happen: reports it as
    /// [`SelectionEvent::Changed`] with `None`.
    pub(crate) fn notice_lost_selections(&mut self) {
        for kind in [SelectionKind::Clipboard, SelectionKind::Primary] {
            let primary = kind == SelectionKind::Primary;
            if self.noted(kind).is_none() || self.provided(kind).is_some() {
                continue;
            }
            if self.selection_mime_types(primary).is_none() {
                *self.noted(kind) = None;
                self.events
                    .push(WlEvent::Selection(SelectionEvent::Changed {
                        kind,
                        mime_types: None,
                    }));
            }
        }
    }
}

impl WlServer {
    /// Makes the caller the owner of a selection, offering these types to
    /// the clients, or takes back the one it provides (`None`; a client's
    /// selection stays). `id` comes back with every read of it
    /// ([`SelectionEvent::ProvidedWanted`]). The owner it replaces, a client,
    /// is cancelled.
    pub fn provide_selection(
        &mut self,
        kind: SelectionKind,
        mime_types: Option<Vec<String>>,
        id: u64,
    ) {
        let state = &mut self.state;
        match mime_types {
            Some(mime_types) => {
                *state.noted(kind) = Some(mime_types.clone());
                let data = ProvidedSelection { id };
                match kind {
                    SelectionKind::Clipboard => data_device::set_data_device_selection(
                        &state.display_handle,
                        &state.seat,
                        mime_types,
                        data,
                    ),
                    SelectionKind::Primary => primary_selection::set_primary_selection(
                        &state.display_handle,
                        &state.seat,
                        mime_types,
                        data,
                    ),
                }
            }
            None => {
                if state.provided(kind).is_none() {
                    return;
                }
                *state.noted(kind) = None;
                match kind {
                    SelectionKind::Clipboard => {
                        data_device::clear_data_device_selection(&state.display_handle, &state.seat)
                    }
                    SelectionKind::Primary => primary_selection::clear_primary_selection(
                        &state.display_handle,
                        &state.seat,
                    ),
                }
            }
        }
    }

    /// Has the client that owns a selection write it, as `mime_type`, into
    /// `fd`. The request reaches it with the next [`Self::flush_clients`].
    pub fn write_selection(
        &mut self,
        kind: SelectionKind,
        mime_type: String,
        fd: OwnedFd,
    ) -> Result<(), SelectionWriteError> {
        let seat = &self.state.seat;
        match kind {
            SelectionKind::Clipboard => {
                use data_device::SelectionRequestError as E;
                data_device::request_data_device_client_selection(seat, mime_type, fd).map_err(
                    |error| match error {
                        E::NoSelection => SelectionWriteError::NoSelection,
                        E::ServerSideSelection => SelectionWriteError::Provided,
                        E::InvalidMimetype => SelectionWriteError::NotOffered,
                    },
                )
            }
            SelectionKind::Primary => {
                use primary_selection::SelectionRequestError as E;
                primary_selection::request_primary_client_selection(seat, mime_type, fd).map_err(
                    |error| match error {
                        E::NoSelection => SelectionWriteError::NoSelection,
                        E::ServerSideSelection => SelectionWriteError::Provided,
                        E::InvalidMimetype => SelectionWriteError::NotOffered,
                    },
                )
            }
        }
    }

    /// Who owns a selection.
    pub fn selection_owner(&self, kind: SelectionKind) -> SelectionOwner {
        if self.state.provided(kind).is_some() {
            return SelectionOwner::Provided;
        }
        let primary = kind == SelectionKind::Primary;
        match self.state.selection_mime_types(primary) {
            Some(_) => SelectionOwner::Client,
            None => SelectionOwner::None,
        }
    }
}
