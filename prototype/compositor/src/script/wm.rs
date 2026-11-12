//! Window management by the script.
//!
//! The table of windows is the fallback's ([`fallback::Windows`]): for every
//! window its logical position, the size it is configured with, its place in
//! the stacking order, and which window has the focus. The script edits that
//! table through `hz.place`, `hz.configure`, `hz.focus`, `hz.raise` and
//! `hz.restack`; the core then makes the protocol side agree (configure
//! events, keyboard focus).
//!
//! Where the script says nothing, the fallback's rules apply: a new window
//! is put in the cascade, on top, and gets the focus unless the script's
//! `window_opened` handled it. That is what a window gets when the project
//! defines no window management, when the callback fails, and in the
//! fallback mode. In the fallback mode the focused window is also kept on
//! top. A broken script therefore cannot make a window unreachable.

use std::cell::RefCell;
use std::rc::Rc;

use crate::wm::{ConfigureHint, OutputId, Placement, Point, Rect, Size, WindowId};
use mlua::{Lua, Table};
use proto_gfx_core::{ErrorCode, GfxError};

use super::{Mode, ScriptRuntime, Shared};
use crate::fallback;
use crate::scene::Hit;
use crate::wm::{ConstraintView, Modifiers, WindowManager, WmRequest};

/// Something that happened, waiting to be told to the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Opened(WindowId),
    Changed(WindowId),
    Closed(WindowId),
    /// The focus moved for a reason the script did not give: a click, a
    /// request from outside, the focused window going away.
    Focus(Option<WindowId>),
    Outputs,
    /// The client of a window asked for something the window manager
    /// decides (fullscreen, activation...).
    Request(WindowId, proto_wl::WindowRequest),
}

/// What the script side keeps besides the table.
#[derive(Default)]
pub(crate) struct WmState {
    /// Whether the script manages windows at all
    /// ([`ScriptOptions::manage_windows`](super::ScriptOptions::manage_windows)).
    active: bool,
    events: Vec<Event>,
    /// The revision of the table the core last applied.
    applied: u64,
    /// The pointer lock or confinement in effect.
    constraint: Option<ConstraintView>,
}

impl WmState {
    pub(crate) fn new(active: bool) -> Self {
        Self {
            active,
            ..Self::default()
        }
    }
}

/// A pointer constraint as the script sees it: `{ surface, window, kind,
/// active }`, `kind` being `"lock"` or `"confine"`.
fn constraint_table(lua: &Lua, constraint: &ConstraintView) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("surface", constraint.surface.0)?;
    t.set("window", constraint.window.map(|w| w.0))?;
    t.set("kind", constraint.kind.name())?;
    t.set("active", constraint.active)?;
    Ok(t)
}

fn wm_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(GfxError {
        code: ErrorCode::InvalidArgument,
        message: message.into(),
    })
}

/// Adds the window management functions to the `hz` table.
pub(crate) fn register(
    lua: &Lua,
    hz: &Table,
    shared: &Rc<RefCell<Shared>>,
    windows: &Rc<RefCell<fallback::Windows>>,
) -> mlua::Result<()> {
    macro_rules! def {
        ($name:literal, |$table:ident, $args:tt : $ty:ty| $body:expr) => {{
            let state = Rc::clone(shared);
            let windows = Rc::clone(windows);
            hz.set(
                $name,
                lua.create_function(move |_, $args: $ty| {
                    if !state.borrow().wm.active {
                        return Err(wm_error(concat!(
                            "hz.",
                            $name,
                            ": windows are not managed by the script in this session"
                        )));
                    }
                    let mut $table = windows.borrow_mut();
                    $body
                })?,
            )?;
        }};
    }
    let unknown = |id: u64| wm_error(format!("unknown window {id}"));

    // Whether this session leaves window management to the script.
    {
        let state = Rc::clone(shared);
        hz.set(
            "manages_windows",
            lua.create_function(move |_, (): ()| Ok(state.borrow().wm.active))?,
        )?;
    }

    // Sets the logical position of a window (global logical coordinates).
    def!("place", |table, (id, x, y): (u64, i32, i32)| {
        if !table.contains(WindowId(id)) {
            return Err(unknown(id));
        }
        table.place(WindowId(id), Point::new(x, y));
        Ok(())
    });

    // Sets the size a window is asked to have. Without a size the client
    // chooses.
    def!("configure", |table,
                       (id, width, height): (
        u64,
        Option<i32>,
        Option<i32>
    )| {
        let size = match (width, height) {
            (Some(w), Some(h)) if w > 0 && h > 0 && w <= 16384 && h <= 16384 => {
                Some(Size::new(w, h))
            }
            (None, None) => None,
            _ => return Err(wm_error(format!("invalid size {width:?} x {height:?}"))),
        };
        if !table.configure(WindowId(id), size) {
            return Err(unknown(id));
        }
        Ok(())
    });

    // Gives the keyboard focus to a window, or to nothing.
    def!("focus", |table, id: Option<u64>| {
        if let Some(id) = id
            && !table.contains(WindowId(id))
        {
            return Err(unknown(id));
        }
        table.set_focus(id.map(WindowId));
        Ok(())
    });

    // Moves a window to the top of the stacking order.
    def!("raise", |table, id: u64| {
        if !table.contains(WindowId(id)) {
            return Err(unknown(id));
        }
        table.raise(WindowId(id));
        Ok(())
    });

    // Sets the stacking order, bottom to top. Windows that are not listed
    // stay below the listed ones, in their current order.
    def!("restack", |table, order: Vec<u64>| {
        if let Some(id) = order.iter().find(|id| !table.contains(WindowId(**id))) {
            return Err(unknown(*id));
        }
        let order: Vec<WindowId> = order.into_iter().map(WindowId).collect();
        table.restack(&order);
        Ok(())
    });

    // Asks a window to close. The client decides whether it does.
    def!("close", |table, id: u64| {
        if !table.contains(WindowId(id)) {
            return Err(unknown(id));
        }
        table.request_close(WindowId(id));
        Ok(())
    });

    // Ends the pointer lock or confinement in effect, whatever the client
    // wants. The way out for the person at the keyboard; the core does not
    // offer the constraint again until the focus moves or the pointer
    // leaves the surface and comes back.
    def!("release_pointer_constraint", |table, (): ()| {
        table.request(WmRequest::ReleasePointerConstraint);
        Ok(())
    });

    // Asks `pointer_constraint` again about every constraint that could be
    // in effect now: for a script that changed its mind.
    def!("review_pointer_constraints", |table, (): ()| {
        table.request(WmRequest::ReviewPointerConstraints);
        Ok(())
    });

    // The pointer lock or confinement in effect, or nil.
    {
        let state = Rc::clone(shared);
        hz.set(
            "pointer_constraint",
            lua.create_function(move |lua, (): ()| {
                let shared = state.borrow();
                match &shared.wm.constraint {
                    Some(constraint) if shared.wm.active => {
                        Ok(mlua::Value::Table(constraint_table(lua, constraint)?))
                    }
                    _ => Ok(mlua::Value::Nil),
                }
            })?,
        )?;
    }

    // Tells a window whether it is maximized and whether it is fullscreen
    // (`{ maximized = bool, fullscreen = bool }`; a missing field is false).
    // Where it goes and how large it is asked to be stay `hz.place` and
    // `hz.configure`.
    def!("set_states", |table, (id, states): (u64, Table)| {
        if !table.contains(WindowId(id)) {
            return Err(unknown(id));
        }
        let flag = |name: &str| -> mlua::Result<bool> {
            Ok(states.get::<Option<bool>>(name)?.unwrap_or(false))
        };
        table.request(WmRequest::SetStates {
            window: WindowId(id),
            maximized: flag("maximized")?,
            fullscreen: flag("fullscreen")?,
        });
        Ok(())
    });
    Ok(())
}

/// The table `window_request` receives: `kind`, and what comes with it.
fn request_table(lua: &Lua, request: proto_wl::WindowRequest) -> mlua::Result<Table> {
    use proto_wl::WindowRequest as R;
    let table = lua.create_table()?;
    let kind = match request {
        R::Maximize => "maximize",
        R::Unmaximize => "unmaximize",
        R::Fullscreen { output } => {
            table.set("output", output.map(|o| o.0))?;
            "fullscreen"
        }
        R::Unfullscreen => "unfullscreen",
        R::Minimize => "minimize",
        R::Move => "move",
        R::Resize { edges } => {
            table.set("top", edges.top)?;
            table.set("bottom", edges.bottom)?;
            table.set("left", edges.left)?;
            table.set("right", edges.right)?;
            "resize"
        }
        R::ShowMenu { position } => {
            table.set("x", position.0)?;
            table.set("y", position.1)?;
            "show_menu"
        }
        R::Activate { from_input } => {
            table.set("from_input", from_input)?;
            "activate"
        }
    };
    table.set("kind", kind)?;
    Ok(table)
}

/// The [`WindowManager`] backed by a script, on top of the fallback's table.
pub struct ScriptWm {
    runtime: Rc<ScriptRuntime>,
}

impl ScriptWm {
    /// The runtime must have been created with
    /// [`ScriptOptions::manage_windows`](super::ScriptOptions::manage_windows).
    pub fn new(runtime: Rc<ScriptRuntime>) -> Result<Self, String> {
        if !runtime.shared.borrow().wm.active {
            return Err("the script was started without window management".to_string());
        }
        Ok(Self { runtime })
    }

    fn table<R>(&self, f: impl FnOnce(&mut fallback::Windows) -> R) -> R {
        f(&mut self.runtime.windows.borrow_mut())
    }

    fn event(&self, event: Event) {
        self.runtime.shared.borrow_mut().wm.events.push(event);
    }
}

impl WindowManager for ScriptWm {
    fn name(&self) -> &'static str {
        "script"
    }

    fn set_output(&mut self, id: OutputId, rect: Rect) {
        self.table(|table| table.set_output(id, rect));
        self.event(Event::Outputs);
    }

    fn remove_output(&mut self, id: OutputId) {
        self.table(|table| table.remove_output(id));
        self.event(Event::Outputs);
    }

    fn add_window(&mut self, id: WindowId) -> Placement {
        if let Some(existing) = self.placement(id) {
            return existing;
        }
        // The default place, in case the script has nothing to say: the
        // fallback's cascade on the first output, on top. Whether it is
        // focused is decided when the script was asked (`dispatch`).
        let placement = self.table(|table| table.insert(id));
        self.event(Event::Opened(id));
        placement
    }

    fn remove_window(&mut self, id: WindowId) {
        let moved = self.table(|table| table.remove(id));
        self.event(Event::Closed(id));
        if moved {
            let focus = self.focus();
            self.event(Event::Focus(focus));
        }
    }

    fn set_window_size(&mut self, id: WindowId, size: Size) -> bool {
        self.event(Event::Changed(id));
        self.table(|table| table.set_size(id, size))
    }

    fn key(&mut self, keycode: u32, pressed: bool, modifiers: Modifiers) -> bool {
        let mods = match self.runtime.host.lua().create_table() {
            Ok(table) => table,
            Err(_) => return false,
        };
        let _ = mods.set("ctrl", modifiers.ctrl);
        let _ = mods.set("alt", modifiers.alt);
        let _ = mods.set("shift", modifiers.shift);
        let _ = mods.set("logo", modifiers.logo);
        matches!(
            self.runtime
                .call::<_, Option<bool>>("key", (keycode, pressed, mods)),
            Some(Ok(Some(true)))
        )
    }

    fn pointer_button(&mut self, button: u32, pressed: bool, hit: Option<&Hit>) -> bool {
        let window = hit.map(|h| h.window.0);
        let surface = hit.map(|h| h.surface.0);
        matches!(
            self.runtime
                .call::<_, Option<bool>>("pointer_button", (button, pressed, window, surface)),
            Some(Ok(Some(true)))
        )
    }

    fn pointer_constraint(&mut self, constraint: &ConstraintView) -> Option<bool> {
        let table = constraint_table(self.runtime.host.lua(), constraint).ok()?;
        // Not defined, failed, or nil: the core's default.
        match self
            .runtime
            .call::<_, Option<bool>>("pointer_constraint", table)
        {
            Some(Ok(answer)) => answer,
            _ => None,
        }
    }

    fn pointer_constraint_changed(&mut self, active: Option<&ConstraintView>) {
        self.runtime.shared.borrow_mut().wm.constraint = active.copied();
        let table = match active {
            Some(constraint) => match constraint_table(self.runtime.host.lua(), constraint) {
                Ok(table) => Some(table),
                Err(_) => return,
            },
            None => None,
        };
        self.runtime
            .call::<_, ()>("pointer_constraint_changed", table);
    }

    fn window_request(&mut self, id: WindowId, request: proto_wl::WindowRequest) {
        self.event(Event::Request(id, request));
    }

    fn dispatch(&mut self) -> bool {
        let events = {
            let mut shared = self.runtime.shared.borrow_mut();
            let mut events = std::mem::take(&mut shared.wm.events);
            events.dedup();
            events
        };
        for event in events {
            // A failure is recorded by `call`; the table keeps what it had.
            match event {
                Event::Opened(id) => {
                    let handled = matches!(
                        self.runtime.call::<_, ()>("window_opened", id.0),
                        Some(Ok(()))
                    );
                    if !handled {
                        // Nobody decided: the fallback's rule, the new
                        // window gets the focus and is on top.
                        self.table(|table| {
                            table.set_focus(Some(id));
                            table.raise(id);
                        });
                    }
                }
                Event::Changed(id) => {
                    self.runtime.call::<_, ()>("window_changed", id.0);
                }
                Event::Closed(id) => {
                    self.runtime.call::<_, ()>("window_closed", id.0);
                }
                Event::Focus(id) => {
                    self.runtime
                        .call::<_, ()>("focus_changed", id.map(|id| id.0));
                }
                Event::Outputs => {
                    self.runtime.call::<_, ()>("outputs_changed", ());
                }
                Event::Request(id, request) => {
                    if let Ok(table) = request_table(self.runtime.host.lua(), request) {
                        self.runtime.call::<_, ()>("window_request", (id.0, table));
                    }
                }
            }
        }
        // Without the script, the fallback keeps the window that gets the
        // keys in front.
        if self.runtime.mode() == Mode::Fallback {
            self.table(|table| table.raise_focused());
        }
        // Everything since the core last looked counts, also what `key`
        // and `pointer_button` changed.
        let revision = self.table(|table| table.revision());
        let mut shared = self.runtime.shared.borrow_mut();
        let changed = revision != shared.wm.applied;
        shared.wm.applied = revision;
        changed
    }

    fn move_window(&mut self, id: WindowId, position: Point) -> bool {
        self.table(|table| table.place(id, position))
    }

    fn focus_window(&mut self, id: Option<WindowId>) -> bool {
        let (focused, raised) = self.table(|table| {
            if id.is_some_and(|id| !table.contains(id)) {
                return (false, false);
            }
            let focused = table.set_focus(id);
            let raised = id.is_some_and(|id| table.raise(id));
            (focused, raised)
        });
        if focused {
            self.event(Event::Focus(id));
        }
        focused || raised
    }

    fn focus(&self) -> Option<WindowId> {
        self.runtime.windows.borrow().focus()
    }

    fn placement(&self, id: WindowId) -> Option<Placement> {
        self.runtime.windows.borrow().placement(id)
    }

    fn placements(&self) -> Vec<Placement> {
        self.runtime.windows.borrow().placements()
    }

    fn configure_hint(&self, id: WindowId) -> ConfigureHint {
        self.runtime.windows.borrow().configure_hint(id)
    }

    fn take_requests(&mut self) -> Vec<WmRequest> {
        self.table(|table| table.take_requests())
    }
}
