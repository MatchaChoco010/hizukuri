//! Selections across the boundary of the session, in the core: the core
//! only connects the backend (what is outside: the parent compositor of a
//! nested session) with the protocol layer (the clients). Neither data nor
//! the decision of what crosses is the core's; see
//! [`proto_backend::selection`].

use proto_backend::{Backend, SelectionEvent as BackendSelection, SelectionKind as BackendKind};
use proto_wl::{SelectionEvent as ClientSelection, SelectionKind, SelectionWriteError};
use serde_json::{Value, json};

use super::Compositor;

fn to_wl(kind: BackendKind) -> SelectionKind {
    match kind {
        BackendKind::Clipboard => SelectionKind::Clipboard,
        BackendKind::Primary => SelectionKind::Primary,
    }
}

fn to_backend(kind: SelectionKind) -> BackendKind {
    match kind {
        SelectionKind::Clipboard => BackendKind::Clipboard,
        SelectionKind::Primary => BackendKind::Primary,
    }
}

impl<B: Backend> Compositor<B> {
    /// The backend says what the selection outside is, or that something
    /// outside reads one inside.
    pub(super) fn backend_selection(&mut self, event: BackendSelection) {
        match event {
            BackendSelection::Outer {
                kind,
                mime_types,
                generation,
            } => self
                .wl
                .provide_selection(to_wl(kind), mime_types, generation),
            BackendSelection::Wanted {
                kind,
                mime_type,
                fd,
            } => {
                let fd = match fd.into_owned() {
                    Ok(fd) => fd,
                    Err(error) => {
                        tracing::warn!("a selection transfer cannot be handed on: {error}");
                        return;
                    }
                };
                match self.wl.write_selection(to_wl(kind), mime_type, fd) {
                    Ok(()) => {}
                    // The owner inside went away since: there is nothing to
                    // offer outside either.
                    Err(SelectionWriteError::NoSelection) => {
                        self.backend.offer_selection(kind, None)
                    }
                    Err(error) => {
                        tracing::debug!("the {} selection is not read: {error}", kind.name())
                    }
                }
            }
        }
    }

    /// A client set or cleared a selection, or reads the one the core
    /// provides for the backend.
    pub(super) fn client_selection(&mut self, event: ClientSelection) {
        match event {
            ClientSelection::Changed { kind, mime_types } => self
                .backend
                .offer_selection(to_backend(kind), mime_types.as_deref()),
            ClientSelection::ProvidedWanted {
                kind,
                id,
                mime_type,
                fd,
            } => {
                let Ok(fd) = fd.into_owned() else {
                    return;
                };
                // A selection that is gone outside reads as empty: the pipe
                // is closed unwritten.
                self.backend
                    .receive_outer_selection(to_backend(kind), id, &mime_type, fd);
            }
        }
    }

    /// Who owns each selection, for the state dump.
    pub(super) fn describe_selection_owners(&self) -> Value {
        json!({
            "clipboard": self.wl.selection_owner(SelectionKind::Clipboard).name(),
            "primary": self.wl.selection_owner(SelectionKind::Primary).name(),
        })
    }
}
