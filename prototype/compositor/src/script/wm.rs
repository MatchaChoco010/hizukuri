//! Window management by the script.
//!
//! The core keeps a small table: for every window its logical position, the
//! size it is configured with, its place in the stacking order, and which
//! window has the focus. The script edits that table through `hz.place`,
//! `hz.configure`, `hz.focus`, `hz.raise` and `hz.restack`; the core then
//! makes the protocol side agree (configure events, keyboard focus).
//!
//! The table has sensible contents without the script: a new window is put
//! in a cascade, and it is focused unless the script's `window_opened`
//! handled it. That is what a window gets when the script defines no
//! `window_opened`, when the callback fails, and in the fallback mode. A
//! broken script therefore cannot make a window unreachable.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table};
use proto_gfx_core::{ErrorCode, GfxError};
use proto_layout::{ConfigureHint, OutputId, Placement, Point, Rect, Size, WindowId};

use super::{ScriptRuntime, Shared};
use crate::scene::Hit;
use crate::wm::{Modifiers, WindowManager, WmRequest};

const CASCADE_STEP: i32 = 32;
const CASCADE_SLOTS: i32 = 8;

#[derive(Debug, Clone, Copy)]
struct Window {
    placement: Placement,
    /// The size the window is configured with. `None`: the client chooses.
    configure: Option<Size>,
}

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
}

/// The table described in the module documentation.
#[derive(Default)]
pub(crate) struct WmState {
    /// Whether the script manages windows at all (`--wm script`).
    active: bool,
    /// Stacking order, bottom to top.
    windows: Vec<Window>,
    focus: Option<WindowId>,
    outputs: Vec<(OutputId, Rect)>,
    events: Vec<Event>,
    requests: Vec<WmRequest>,
    placed: i32,
    /// Bumped by every change, so the core can tell whether to re-apply.
    revision: u64,
    /// The revision the core last applied.
    applied: u64,
}

impl WmState {
    pub(crate) fn new(active: bool) -> Self {
        Self {
            active,
            ..Self::default()
        }
    }

    fn window(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.placement.window == id)
    }

    fn raise(&mut self, id: WindowId) -> bool {
        match self.windows.iter().position(|w| w.placement.window == id) {
            Some(index) if index + 1 != self.windows.len() => {
                let window = self.windows.remove(index);
                self.windows.push(window);
                self.revision += 1;
                true
            }
            _ => false,
        }
    }

    fn focus(&mut self, id: Option<WindowId>) -> bool {
        if id.is_some_and(|id| self.window(id).is_none()) || self.focus == id {
            return false;
        }
        self.focus = id;
        self.revision += 1;
        true
    }

    /// Moves a window. It then belongs to the output its top left corner is
    /// on; a window moved to where no output is keeps the one it had (the
    /// rule of the Rust layout).
    fn place(&mut self, id: WindowId, position: Point) -> bool {
        let output = self
            .outputs
            .iter()
            .find(|(_, rect)| rect.contains(position))
            .map(|(id, _)| *id);
        match self.window(id) {
            Some(window) if window.placement.position != position => {
                window.placement.position = position;
                if output.is_some() {
                    window.placement.output = output;
                }
                self.revision += 1;
                true
            }
            _ => false,
        }
    }
}

fn wm_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(GfxError {
        code: ErrorCode::InvalidArgument,
        message: message.into(),
    })
}

/// Adds the window management functions to the `hz` table.
pub(crate) fn register(lua: &Lua, hz: &Table, shared: &Rc<RefCell<Shared>>) -> mlua::Result<()> {
    macro_rules! def {
        ($name:literal, |$wm:ident, $args:tt : $ty:ty| $body:expr) => {{
            let state = Rc::clone(shared);
            hz.set(
                $name,
                lua.create_function(move |_, $args: $ty| {
                    let mut shared = state.borrow_mut();
                    let $wm = &mut shared.wm;
                    if !$wm.active {
                        return Err(wm_error(concat!(
                            "hz.",
                            $name,
                            ": windows are not managed by the script in this session"
                        )));
                    }
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
    def!("place", |wm, (id, x, y): (u64, i32, i32)| {
        if wm.window(WindowId(id)).is_none() {
            return Err(unknown(id));
        }
        wm.place(WindowId(id), Point::new(x, y));
        Ok(())
    });

    // Sets the size a window is asked to have. Without a size the client
    // chooses.
    def!("configure", |wm,
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
        let window = wm.window(WindowId(id)).ok_or_else(|| unknown(id))?;
        if window.configure != size {
            window.configure = size;
            wm.revision += 1;
        }
        Ok(())
    });

    // Gives the keyboard focus to a window, or to nothing.
    def!("focus", |wm, id: Option<u64>| {
        if let Some(id) = id
            && wm.window(WindowId(id)).is_none()
        {
            return Err(unknown(id));
        }
        wm.focus(id.map(WindowId));
        Ok(())
    });

    // Moves a window to the top of the stacking order.
    def!("raise", |wm, id: u64| {
        if wm.window(WindowId(id)).is_none() {
            return Err(unknown(id));
        }
        wm.raise(WindowId(id));
        Ok(())
    });

    // Sets the stacking order, bottom to top. Windows that are not listed
    // stay below the listed ones, in their current order.
    def!("restack", |wm, order: Vec<u64>| {
        for id in &order {
            if wm.window(WindowId(*id)).is_none() {
                return Err(unknown(*id));
            }
        }
        let before: Vec<WindowId> = wm.windows.iter().map(|w| w.placement.window).collect();
        let listed = |w: &Window| order.contains(&w.placement.window.0);
        let mut stack: Vec<Window> = wm.windows.iter().copied().filter(|w| !listed(w)).collect();
        for id in &order {
            if let Some(window) = wm.windows.iter().find(|w| w.placement.window.0 == *id)
                && !stack.iter().any(|w| w.placement.window.0 == *id)
            {
                stack.push(*window);
            }
        }
        wm.windows = stack;
        if before
            != wm
                .windows
                .iter()
                .map(|w| w.placement.window)
                .collect::<Vec<_>>()
        {
            wm.revision += 1;
        }
        Ok(())
    });

    // Asks a window to close. The client decides whether it does.
    def!("close", |wm, id: u64| {
        if wm.window(WindowId(id)).is_none() {
            return Err(unknown(id));
        }
        wm.requests.push(WmRequest::Close(WindowId(id)));
        Ok(())
    });
    Ok(())
}

/// The [`WindowManager`] backed by a script.
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

    fn with<R>(&self, f: impl FnOnce(&mut WmState) -> R) -> R {
        f(&mut self.runtime.shared.borrow_mut().wm)
    }
}

impl WindowManager for ScriptWm {
    fn name(&self) -> &'static str {
        "script"
    }

    fn set_output(&mut self, id: OutputId, rect: Rect) {
        self.with(|wm| {
            match wm.outputs.iter_mut().find(|(o, _)| *o == id) {
                Some(output) => output.1 = rect,
                None => wm.outputs.push((id, rect)),
            }
            wm.events.push(Event::Outputs);
        });
    }

    fn remove_output(&mut self, id: OutputId) {
        self.with(|wm| {
            wm.outputs.retain(|(o, _)| *o != id);
            let first = wm.outputs.first().map(|(o, _)| *o);
            for window in &mut wm.windows {
                if window.placement.output == Some(id) {
                    window.placement.output = first;
                }
            }
            wm.events.push(Event::Outputs);
            wm.revision += 1;
        });
    }

    fn add_window(&mut self, id: WindowId) -> Placement {
        self.with(|wm| {
            if let Some(existing) = wm.window(id) {
                return existing.placement;
            }
            // The default place, in case the script has nothing to say: a
            // cascade on the first output, on top. Whether it is focused is
            // decided when the script was asked (`dispatch`).
            let output = wm.outputs.first().copied();
            let origin = output.map(|(_, rect)| rect.origin()).unwrap_or_default();
            let slot = wm.placed % CASCADE_SLOTS;
            wm.placed += 1;
            let placement = Placement {
                window: id,
                output: output.map(|(o, _)| o),
                position: Point::new(
                    origin.x + CASCADE_STEP * (slot + 1),
                    origin.y + CASCADE_STEP * (slot + 1),
                ),
                size: None,
            };
            wm.windows.push(Window {
                placement,
                configure: None,
            });
            wm.events.push(Event::Opened(id));
            wm.revision += 1;
            placement
        })
    }

    fn remove_window(&mut self, id: WindowId) {
        self.with(|wm| {
            wm.windows.retain(|w| w.placement.window != id);
            wm.events.push(Event::Closed(id));
            if wm.focus == Some(id) {
                wm.focus = wm.windows.last().map(|w| w.placement.window);
                wm.events.push(Event::Focus(wm.focus));
            }
            wm.revision += 1;
        });
    }

    fn set_window_size(&mut self, id: WindowId, size: Size) -> bool {
        self.with(|wm| {
            wm.events.push(Event::Changed(id));
            match wm.window(id) {
                Some(window) if window.placement.size != Some(size) => {
                    window.placement.size = Some(size);
                    wm.revision += 1;
                    true
                }
                _ => false,
            }
        })
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

    fn dispatch(&mut self) -> bool {
        let events = self.with(|wm| {
            let mut events = std::mem::take(&mut wm.events);
            events.dedup();
            events
        });
        for event in events {
            // A failure is recorded by `call`; the table keeps what it had.
            match event {
                Event::Opened(id) => {
                    let handled = matches!(
                        self.runtime.call::<_, ()>("window_opened", id.0),
                        Some(Ok(()))
                    );
                    if !handled {
                        // Nobody decided: the new window gets the focus.
                        self.with(|wm| {
                            wm.focus(Some(id));
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
            }
        }
        // Everything since the core last looked counts, also what `key`
        // and `pointer_button` changed.
        self.with(|wm| {
            let changed = wm.revision != wm.applied;
            wm.applied = wm.revision;
            changed
        })
    }

    fn move_window(&mut self, id: WindowId, position: Point) -> bool {
        self.with(|wm| wm.place(id, position))
    }

    fn focus_window(&mut self, id: Option<WindowId>) -> bool {
        self.with(|wm| {
            let focused = wm.focus(id);
            if focused {
                wm.events.push(Event::Focus(id));
            }
            let raised = id.is_some_and(|id| wm.raise(id));
            focused || raised
        })
    }

    fn focus(&self) -> Option<WindowId> {
        self.runtime.shared.borrow().wm.focus
    }

    fn placement(&self, id: WindowId) -> Option<Placement> {
        self.runtime
            .shared
            .borrow()
            .wm
            .windows
            .iter()
            .find(|w| w.placement.window == id)
            .map(|w| w.placement)
    }

    fn placements(&self) -> Vec<Placement> {
        self.runtime
            .shared
            .borrow()
            .wm
            .windows
            .iter()
            .map(|w| w.placement)
            .collect()
    }

    fn configure_hint(&self, id: WindowId) -> ConfigureHint {
        let shared = self.runtime.shared.borrow();
        ConfigureHint {
            size: shared
                .wm
                .windows
                .iter()
                .find(|w| w.placement.window == id)
                .and_then(|w| w.configure),
            activated: shared.wm.focus == Some(id),
        }
    }

    fn take_requests(&mut self) -> Vec<WmRequest> {
        self.with(|wm| std::mem::take(&mut wm.requests))
    }
}
