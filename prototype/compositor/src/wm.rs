//! Window management: where windows are, which one has the focus, what they
//! are told to be.
//!
//! [`WindowManager`] is the second seam of the core, next to
//! [`Scene`](crate::scene::Scene). The core tells the window manager what
//! happens (windows and outputs come and go, keys and buttons are pressed)
//! and reads back the placement, the stacking order, the focus and the size
//! each window should be configured with. It then makes the protocol side
//! agree.
//!
//! [`ScriptWm`](crate::script::ScriptWm) asks a script.
//! [`fallback::Windows`](crate::fallback::Windows) is what the core does
//! where nobody else decides; it runs without a script and without a GPU.
//!
//! This module also holds the vocabulary both use: window and output ids,
//! [`Placement`] and [`ConfigureHint`], on the integer logical geometry of
//! [`crate::geometry`].

pub use crate::geometry::{Point, Rect, Size};

/// Identifies a window. Allocated by the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// Identifies an output. Allocated by the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u64);

/// Where a window is, logically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub window: WindowId,
    /// The output the window was placed on, if any output exists.
    pub output: Option<OutputId>,
    /// Position of the window geometry in global logical coordinates.
    pub position: Point,
    /// Size of the window geometry. `None` until the window reports one.
    pub size: Option<Size>,
}

impl Placement {
    /// The rectangle the window occupies, once its size is known.
    pub fn rect(&self) -> Option<Rect> {
        self.size.map(|size| Rect::from_parts(self.position, size))
    }
}

/// What a window is to be told when it is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigureHint {
    /// The size it is asked to have. `None` lets the client choose.
    pub size: Option<Size>,
    /// Whether the window has the focus.
    pub activated: bool,
}

use crate::scene::Hit;

/// Which modifier keys are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl Modifiers {
    /// Updates the state for a key event. `keycode` is a Linux evdev code.
    pub fn update(&mut self, keycode: u32, pressed: bool) {
        match keycode {
            29 | 97 => self.ctrl = pressed,
            56 | 100 => self.alt = pressed,
            42 | 54 => self.shift = pressed,
            125 | 126 => self.logo = pressed,
            _ => {}
        }
    }
}

/// Something a window manager asks the core to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmRequest {
    /// Ask a window to close.
    Close(WindowId),
    /// End the pointer lock or confinement in effect, whatever the client
    /// wants: the way out for the person at the keyboard. It is not
    /// offered again until the keyboard focus moves or the pointer leaves
    /// the surface and comes back.
    ReleasePointerConstraint,
    /// Ask [`WindowManager::pointer_constraint`] again about every
    /// constraint that could be in effect now, as if nothing had been
    /// decided: for a window manager that changed its mind.
    ReviewPointerConstraints,
}

/// A pointer constraint (lock or confinement) a client asked for, as the
/// window manager sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConstraintView {
    /// The window of the surface, if it belongs to one.
    pub window: Option<WindowId>,
    pub surface: proto_wl::SurfaceId,
    pub kind: proto_wl::PointerConstraintKind,
    /// Whether it is in effect now.
    pub active: bool,
}

/// See the module documentation.
pub trait WindowManager {
    /// Short name for state dumps.
    fn name(&self) -> &'static str;

    // -- what happened --------------------------------------------------------

    /// An output exists now, or its rectangle changed.
    fn set_output(&mut self, id: OutputId, rect: Rect);
    fn remove_output(&mut self, id: OutputId);
    /// A window was created. It has no contents and no size yet.
    fn add_window(&mut self, id: WindowId) -> Placement;
    fn remove_window(&mut self, id: WindowId);
    /// The client committed a window geometry of this size.
    fn set_window_size(&mut self, id: WindowId, size: Size) -> bool;
    /// A key was pressed or released. Returns `true` when the window
    /// manager used it; the key then does not reach a client.
    fn key(&mut self, _keycode: u32, _pressed: bool, _modifiers: Modifiers) -> bool {
        false
    }
    /// A pointer button was pressed or released over `hit`. Returns `true`
    /// when the window manager decided the focus itself; otherwise the core
    /// focuses the window under the pointer on a press.
    fn pointer_button(&mut self, _button: u32, _pressed: bool, _hit: Option<&Hit>) -> bool {
        false
    }
    /// The client of a window asked for it to be maximized, fullscreen,
    /// minimized, moved, resized, or for its window menu. Nothing has been
    /// done about it; a window manager that does nothing is within the
    /// protocol.
    fn window_request(&mut self, _id: WindowId, _request: proto_wl::WindowRequest) {}
    /// Whether a pointer constraint may be in effect now. Asked when the
    /// pointer is on its surface, inside its region, and something changed
    /// since the last answer (the constraint is new, the pointer came back
    /// onto the surface, the keyboard focus moved). `None`: no opinion, the
    /// core's default applies (in effect while its window has the keyboard
    /// focus).
    fn pointer_constraint(&mut self, _constraint: &ConstraintView) -> Option<bool> {
        None
    }
    /// The constraint in effect changed: one started, or the one there was
    /// ended (`None`).
    fn pointer_constraint_changed(&mut self, _active: Option<&ConstraintView>) {}
    /// Runs what the events above queued, once the scene has the current
    /// state of the core. Returns `true` when placement, stacking, focus or
    /// configure hints may have changed.
    fn dispatch(&mut self) -> bool {
        false
    }

    // -- requests from outside (the control interface) --------------------------

    fn move_window(&mut self, id: WindowId, position: Point) -> bool;
    /// Gives the focus to a window and raises it, or clears the focus.
    fn focus_window(&mut self, id: Option<WindowId>) -> bool;

    // -- what the core reads back ----------------------------------------------

    fn focus(&self) -> Option<WindowId>;
    fn placement(&self, id: WindowId) -> Option<Placement>;
    /// All placements in stacking order, bottom to top.
    fn placements(&self) -> Vec<Placement>;
    /// What a window should be told when it is configured.
    fn configure_hint(&self, id: WindowId) -> ConfigureHint;
    fn take_requests(&mut self) -> Vec<WmRequest> {
        Vec::new()
    }
}
