//! Selections (the clipboard and the primary selection) that cross the
//! boundary of the session.
//!
//! A backend whose session lives inside another one (the nested backend)
//! has a second set of selections outside: the parent's. What a client
//! inside copies is offered there, and what is copied there is offered
//! inside. The data never passes through the compositor: whoever reads
//! hands over the write end of a pipe ([`TransferFd`]), and it is passed on
//! as it is to whoever owns the selection on the other side, who writes
//! into it directly. MIME types are passed on as they are.
//!
//! - Outside to inside: [`SelectionEvent::Outer`] tells the core what the
//!   parent's selection offers. The core offers it to the clients as a
//!   selection it provides itself; when one of them reads it, the core asks
//!   [`Backend::receive_outer_selection`](crate::Backend::receive_outer_selection)
//!   to have it written.
//! - Inside to outside: the core tells
//!   [`Backend::offer_selection`](crate::Backend::offer_selection) what a
//!   client set. When something outside reads it,
//!   [`SelectionEvent::Wanted`] asks the core to have the owner write it.

use std::os::fd::OwnedFd;
use std::sync::Arc;

/// Which selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionKind {
    /// What copy and paste use (`wl_data_device`).
    Clipboard,
    /// What selecting text and a middle click use
    /// (`zwp_primary_selection_device_v1`).
    Primary,
}

impl SelectionKind {
    pub const ALL: [SelectionKind; 2] = [SelectionKind::Clipboard, SelectionKind::Primary];

    /// `clipboard` or `primary`.
    pub fn name(self) -> &'static str {
        match self {
            SelectionKind::Clipboard => "clipboard",
            SelectionKind::Primary => "primary",
        }
    }
}

/// The write end of a transfer, handed through an event. Events are
/// `Clone` and `PartialEq`, which a descriptor is not: copies share it, and
/// two are equal when they are the same one.
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

/// A selection outside changed, or something outside reads one inside.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionEvent {
    /// The selection outside offers these MIME types now, or nothing.
    /// `generation` names this selection when a client inside reads it.
    Outer {
        kind: SelectionKind,
        mime_types: Option<Vec<String>>,
        generation: u64,
    },
    /// Something outside reads the selection a client inside owns, as
    /// `mime_type`: have the owner write it into `fd`.
    Wanted {
        kind: SelectionKind,
        mime_type: String,
        fd: TransferFd,
    },
}

/// Which way selections cross.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionCrossing {
    /// What is copied inside is offered outside.
    pub to_outside: bool,
    /// What is copied outside is offered inside.
    pub from_outside: bool,
}

impl SelectionCrossing {
    pub const BOTH: Self = Self {
        to_outside: true,
        from_outside: true,
    };
    pub const NONE: Self = Self {
        to_outside: false,
        from_outside: false,
    };

    /// `both`, `to-parent`, `from-parent` or `off`.
    pub fn name(self) -> &'static str {
        match (self.to_outside, self.from_outside) {
            (true, true) => "both",
            (true, false) => "to-parent",
            (false, true) => "from-parent",
            (false, false) => "off",
        }
    }
}
