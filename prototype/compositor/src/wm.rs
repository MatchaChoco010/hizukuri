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
//! [`proto_layout::Layout`], the floating layout written in Rust, is only
//! used when asked for (`--wm rust`).

use proto_layout::{ConfigureHint, Layout, OutputId, Placement, Point, Rect, Size, WindowId};

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

impl WindowManager for Layout {
    fn name(&self) -> &'static str {
        "rust-floating"
    }

    fn set_output(&mut self, id: OutputId, rect: Rect) {
        Layout::set_output(self, id, rect)
    }

    fn remove_output(&mut self, id: OutputId) {
        Layout::remove_output(self, id)
    }

    fn add_window(&mut self, id: WindowId) -> Placement {
        Layout::add_window(self, id)
    }

    fn remove_window(&mut self, id: WindowId) {
        Layout::remove_window(self, id)
    }

    fn set_window_size(&mut self, id: WindowId, size: Size) -> bool {
        Layout::set_window_size(self, id, size)
    }

    fn move_window(&mut self, id: WindowId, position: Point) -> bool {
        Layout::move_window(self, id, position)
    }

    fn focus_window(&mut self, id: Option<WindowId>) -> bool {
        Layout::focus_window(self, id)
    }

    fn focus(&self) -> Option<WindowId> {
        Layout::focus(self)
    }

    fn placement(&self, id: WindowId) -> Option<Placement> {
        Layout::placement(self, id)
    }

    fn placements(&self) -> Vec<Placement> {
        Layout::placements(self).to_vec()
    }

    fn configure_hint(&self, id: WindowId) -> ConfigureHint {
        Layout::configure_hint(self, id)
    }
}
