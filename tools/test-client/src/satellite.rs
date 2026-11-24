//! What xwayland-satellite asks of a compositor, beyond one window, in the
//! order it asks: windows that belong to another (an X11 window that is
//! transient for another becomes an `xdg_toplevel` with a parent), popups
//! placed where the X11 window is (an override-redirect window becomes an
//! `xdg_popup` anchored to the whole parent, moved by an offset), and
//! activation through `xdg_activation_v1` (an X11 client asking for one of
//! its windows to be activated).

use wayland_client::protocol::wl_surface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_positioner, xdg_surface, xdg_toplevel};

use crate::{ClientError, ClientEvent, State, TestClient, create_buffer};

/// A window that belongs to the client's main window.
#[derive(Debug, Clone)]
pub struct DialogOptions {
    pub title: String,
    /// Size used when the compositor lets the client choose.
    pub width: u32,
    pub height: u32,
    pub color: [u8; 4],
    /// `xdg_toplevel.set_parent` with the main window. xwayland-satellite
    /// sets it for an X11 window that is transient for another.
    pub parent: bool,
    /// `set_min_size` and `set_max_size`. xwayland-satellite sends both as
    /// the window's size for a splash screen, and the X11 size hints
    /// otherwise.
    pub min_size: Option<(i32, i32)>,
    pub max_size: Option<(i32, i32)>,
}

impl Default for DialogOptions {
    fn default() -> Self {
        Self {
            title: "dialog".to_string(),
            width: 120,
            height: 80,
            color: [255, 255, 255, 255],
            parent: true,
            min_size: None,
            max_size: None,
        }
    }
}

pub(crate) struct Dialog {
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    options: DialogOptions,
    /// Size from the latest `xdg_toplevel.configure`.
    pending: (i32, i32),
}

#[derive(Default)]
pub(crate) struct SatelliteState {
    pub activation: Option<xdg_activation_v1::XdgActivationV1>,
    /// Indexed by what `open_dialog` returned; `None` once closed.
    dialogs: Vec<Option<Dialog>>,
}

/// User data of a dialog's `xdg_surface` and `xdg_toplevel`: its index.
pub(crate) struct DialogTag(usize);

/// User data of an activation token: the surface to activate with it.
pub(crate) struct TokenTarget(wl_surface::WlSurface);

impl TestClient {
    /// Opens another toplevel of this client, by xwayland-satellite's steps:
    /// the surface commits without a buffer before it gets its role, the
    /// toplevel gets its size limits, title and parent before the first
    /// commit, and draws once configured. Returns its index.
    pub fn open_dialog(&mut self, options: DialogOptions) -> Result<usize, ClientError> {
        let handle = &self.handle;
        let state = &mut self.state;
        let (Some(compositor), Some(wm_base), Some(window)) = (
            &state.globals.compositor,
            &state.globals.wm_base,
            &state.window,
        ) else {
            return Err(ClientError::Protocol(
                "the main window does not exist yet".to_string(),
            ));
        };
        let index = state.satellite.dialogs.len();
        let surface = compositor.create_surface(handle, ());
        surface.attach(None, 0, 0);
        surface.commit();
        let xdg_surface = wm_base.get_xdg_surface(&surface, handle, DialogTag(index));
        let toplevel = xdg_surface.get_toplevel(handle, DialogTag(index));
        if let Some((width, height)) = options.min_size {
            toplevel.set_min_size(width, height);
        }
        if let Some((width, height)) = options.max_size {
            toplevel.set_max_size(width, height);
        }
        toplevel.set_title(options.title.clone());
        if options.parent {
            toplevel.set_parent(Some(&window.toplevel));
        }
        surface.commit();
        state.satellite.dialogs.push(Some(Dialog {
            surface,
            xdg_surface,
            toplevel,
            options,
            pending: (0, 0),
        }));
        Ok(index)
    }

    /// Destroys a dialog (the X11 window was unmapped).
    pub fn close_dialog(&mut self, index: usize) {
        if let Some(dialog) = self
            .state
            .satellite
            .dialogs
            .get_mut(index)
            .and_then(Option::take)
        {
            dialog.toplevel.destroy();
            dialog.xdg_surface.destroy();
            dialog.surface.destroy();
        }
    }

    /// Opens a popup where xwayland-satellite puts an override-redirect X11
    /// window (a menu, a tooltip): anchored to the whole main window, top
    /// left corner to top left corner, moved by `(x, y)`, the distance
    /// between the two X11 windows; allowed to slide. No grab: X11 clients
    /// grab inside the X server. One popup at a time.
    pub fn open_x11_popup(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        color: [u8; 4],
    ) -> Result<(), ClientError> {
        let parent = self
            .state
            .window
            .as_ref()
            .and_then(|w| w.drawn)
            .ok_or_else(|| ClientError::Protocol("the main window is not drawn".to_string()))?;
        self.open_popup_with((width, height), color, false, |positioner| {
            positioner.set_offset(x, y);
            positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
            positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
            positioner.set_anchor_rect(0, 0, parent.0 as i32, parent.1 as i32);
            positioner.set_constraint_adjustment(
                xdg_positioner::ConstraintAdjustment::SlideX
                    | xdg_positioner::ConstraintAdjustment::SlideY,
            );
        })
    }

    /// Asks for an activation token and activates the main window
    /// (`dialog` is `None`) or a dialog with it. With `with_serial` the
    /// token carries the seat and the serial of the latest input event this
    /// client received, and the main window as the surface that asks, as
    /// xwayland-satellite's tokens do.
    pub fn activate(
        &mut self,
        dialog: Option<usize>,
        with_serial: bool,
    ) -> Result<(), ClientError> {
        let state = &self.state;
        let activation = state.satellite.activation.as_ref().ok_or_else(|| {
            ClientError::Protocol("the compositor has no xdg_activation_v1".to_string())
        })?;
        let window = state
            .window
            .as_ref()
            .ok_or_else(|| ClientError::Protocol("no main window".to_string()))?;
        let target = match dialog {
            None => window.surface.clone(),
            Some(index) => state
                .satellite
                .dialogs
                .get(index)
                .and_then(Option::as_ref)
                .map(|d| d.surface.clone())
                .ok_or_else(|| ClientError::Protocol(format!("no dialog {index}")))?,
        };
        let token = activation.get_activation_token(&self.handle, TokenTarget(target));
        if with_serial && let Some(seat) = &state.globals.seat {
            token.set_serial(state.last_serial, seat);
        }
        token.set_surface(&window.surface);
        token.commit();
        Ok(())
    }
}

impl State {
    /// Draws a dialog at its configured size (or its own when the
    /// compositor leaves it to the client).
    fn draw_dialog(&mut self, index: usize, handle: &QueueHandle<State>) {
        let (Some(Some(dialog)), Some(shm)) =
            (self.satellite.dialogs.get(index), &self.globals.shm)
        else {
            return;
        };
        let width = if dialog.pending.0 > 0 {
            dialog.pending.0 as u32
        } else {
            dialog.options.width
        };
        let height = if dialog.pending.1 > 0 {
            dialog.pending.1 as u32
        } else {
            dialog.options.height
        };
        if let Ok(buffer) = create_buffer(
            shm,
            handle,
            width,
            height,
            dialog.options.color,
            self.options.layout,
        ) {
            dialog.surface.attach(Some(&buffer), 0, 0);
            dialog
                .surface
                .damage_buffer(0, 0, width as i32, height as i32);
            dialog.surface.commit();
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, DialogTag> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        tag: &DialogTag,
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            let Some(Some(dialog)) = state.satellite.dialogs.get(tag.0) else {
                return;
            };
            state.events.push(ClientEvent::DialogConfigure {
                dialog: tag.0,
                width: dialog.pending.0,
                height: dialog.pending.1,
            });
            state.draw_dialog(tag.0, handle);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, DialogTag> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        tag: &DialogTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event
            && let Some(Some(dialog)) = state.satellite.dialogs.get_mut(tag.0)
        {
            dialog.pending = (width, height);
        }
    }
}

impl Dispatch<xdg_activation_v1::XdgActivationV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_activation_v1::XdgActivationV1,
        _: xdg_activation_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, TokenTarget> for State {
    fn event(
        state: &mut Self,
        token_object: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        target: &TokenTarget,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            if let Some(activation) = &state.satellite.activation
                && target.0.is_alive()
            {
                activation.activate(token, &target.0);
            }
            token_object.destroy();
        }
    }
}
