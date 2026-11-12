//! Implementations of Smithay's handler traits for [`State`].

use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, find_popup_root_surface,
};
use smithay::input::dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source};
use smithay::input::pointer::{CursorImageStatus, Focus};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::touch::{GrabStartData as TouchGrabStartData, TouchGrab};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
    with_states,
};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::backend::allocator::Buffer as _;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::input_method::InputMethodSeat;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::ext_data_control::{
    DataControlHandler as ExtDataControlHandler, DataControlState as ExtDataControlState,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::wlr_data_control::{
    DataControlHandler as WlrDataControlHandler, DataControlState as WlrDataControlState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

use crate::state::{ClientState, State, Window, apply_pending_state};
use crate::surface::{init_surface, process_commit, surface_id};
use crate::touch::TouchPointsGrab;
use crate::{ResizeEdges, SurfaceId, WindowId, WindowRequest, WlEvent};

impl State {
    fn push_event(&mut self, event: WlEvent) {
        // Commits come in bursts; one notification per burst is enough.
        if self.events.last() != Some(&event) {
            self.events.push(event);
        }
    }

    /// Passes a request of a toplevel on as an event. With `reply`, the
    /// protocol wants a configure in answer; if the window manager sends
    /// none before the next flush, an unchanged one is sent then.
    fn window_request(&mut self, surface: &ToplevelSurface, request: WindowRequest, reply: bool) {
        let Some(window) = self.windows.iter_mut().find(|w| &w.toplevel == surface) else {
            return;
        };
        window.reply_owed |= reply;
        let window = window.id;
        // Not `push_event`: two equal requests in a row are two requests.
        self.events.push(WlEvent::WindowRequest { window, request });
    }

    /// Whether `serial` is that of a pointer button press that is still
    /// held, or of a touch that is still down (the first of the fingers on
    /// the screen): the only things an interactive move or resize may start
    /// from.
    fn is_held_press(&self, seat: &WlSeat, serial: Serial) -> bool {
        let Some(seat) = Seat::<State>::from_resource(seat) else {
            return false;
        };
        seat.get_pointer()
            .is_some_and(|pointer| pointer.has_grab(serial))
            || seat.get_touch().is_some_and(|touch| touch.has_grab(serial))
    }

    fn popup_placement_event(&mut self, popup: &PopupSurface) {
        if let (Some(window), Some(id)) = (
            self.owning_window(popup.wl_surface()),
            surface_id(popup.wl_surface()),
        ) {
            self.events
                .push(WlEvent::PopupPlacement { window, popup: id });
        }
    }

    /// The window a surface belongs to, following subsurface and popup
    /// parents up to a toplevel.
    pub(crate) fn owning_window(&self, surface: &WlSurface) -> Option<WindowId> {
        let mut current = surface.clone();
        // Bounded: a malicious client cannot build a cycle, but stay safe.
        for _ in 0..64 {
            while let Some(parent) = get_parent(&current) {
                current = parent;
            }
            if let Some(id) = self.window_for_root(&current) {
                return Some(id);
            }
            // The popup of an input method belongs to the window whose
            // text input it serves.
            if crate::ime::is_input_popup(self, &current) {
                current = self.input_popup_parent()?;
                continue;
            }
            let popup = self.popups.iter().find(|p| p.wl_surface() == &current)?;
            current = popup.get_parent_surface()?;
        }
        None
    }
}

/// A commit that attaches a dmabuf waits until the buffer can be read: the
/// client's GPU may still be drawing into it. The commit, and those of the
/// same client that depend on it, are applied once the buffer's fences
/// have signalled. The buffer is watched on the server's own event loop.
fn hold_commit_until_the_buffer_is_ready(
    state: &mut State,
    _dh: &DisplayHandle,
    surface: &WlSurface,
) {
    let dmabuf = with_states(surface, |states| {
        match states
            .cached_state
            .get::<SurfaceAttributes>()
            .pending()
            .buffer
            .as_ref()
        {
            Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).cloned().ok(),
            _ => None,
        }
    });
    let Some(dmabuf) = dmabuf else {
        return;
    };
    // `Err` means it is ready already, which is the usual case.
    let Ok((blocker, source)) = dmabuf.generate_blocker(calloop::Interest::READ) else {
        return;
    };
    let Some(client) = surface.client() else {
        return;
    };
    let inserted = state
        .loop_handle
        .insert_source(source, move |_, _, state: &mut State| {
            let dh = state.display_handle.clone();
            state
                .client_compositor_state(&client)
                .blocker_cleared(state, &dh);
            Ok(())
        });
    match inserted {
        Ok(_) => add_blocker(surface, blocker),
        // Without a way to learn when it is ready, do not wait at all.
        Err(error) => {
            tracing::warn!(%error, "cannot watch a dmabuf; its commit is applied at once")
        }
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client is inserted with ClientState")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        let id = SurfaceId(self.allocate_id());
        init_surface(surface, id);
        self.surfaces.insert(id, surface.clone());
        self.announce_outputs(surface);
        // Runs when a commit arrives, before it is applied.
        add_pre_commit_hook::<State, _>(surface, hold_commit_until_the_buffer_is_ready);
    }

    fn commit(&mut self, surface: &WlSurface) {
        // The state of a synchronized subsurface is applied when its parent
        // commits; its buffers are read then.
        if !is_sync_subsurface(surface) {
            process_commit(surface);
        }
        self.popup_manager.commit(surface);
        // By now the surface has its place (a role, a parent); tell it
        // where it is. Nothing is sent when nothing changed.
        self.announce_outputs(surface);

        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(window) = self
            .windows
            .iter()
            .find(|w| w.toplevel.wl_surface() == &root)
        {
            if !window.toplevel.is_initial_configure_sent() {
                apply_pending_state(window);
                window.toplevel.send_configure();
            }
        } else if let Some(popup) = self.popups.iter().find(|p| p.wl_surface() == &root)
            && !popup.is_initial_configure_sent()
            && !self.popups_awaiting_configure.contains(popup)
        {
            // Told at the next flush, so that whoever places popups can
            // still have a say (`WlEvent::PopupPlacement`).
            self.popups_awaiting_configure.push(popup.clone());
        }

        if let Some(window) = self.owning_window(&root) {
            self.push_event(WlEvent::WindowChanged(window));
        } else if let crate::CursorState::Surface(cursor) = &self.cursor
            && surface_id(&root) == Some(*cursor)
        {
            // New contents or a new hotspot for the cursor image.
            self.push_event(WlEvent::CursorChanged);
        }
        // A surface that became part of a window (a subsurface, a popup)
        // now prefers what the window's output does.
        self.refresh_color_feedback();
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        if let Some(id) = surface_id(surface) {
            self.surfaces.remove(&id);
            self.push_event(WlEvent::SurfaceDestroyed(id));
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {
        // Nothing refers to the buffer object itself: pixels are copied
        // out, and dmabufs are handed on as duplicated file descriptors.
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Smithay has checked the planes and that the format code is one
        // of those offered. It does not look at the modifier: a client can
        // name any, or none (`DRM_FORMAT_MOD_INVALID`, where only the
        // driver that made the buffer knows its layout). The importer said
        // which pairs it takes, and "none" is not "linear", so anything
        // else is refused here.
        let format = dmabuf.format();
        let pair = (format.code as u32, u64::from(format.modifier));
        if !self.dmabuf_formats.contains(&pair) {
            tracing::debug!(
                fourcc = format_args!("{:#x}", pair.0),
                modifier = format_args!("{:#x}", pair.1),
                "refusing a dmabuf with a format and modifier that were not offered"
            );
            notifier.failed();
            return;
        }
        // Whether the memory can really be used is only known when the
        // renderer imports it, which happens when the buffer is first
        // shown; a buffer that cannot be used then shows nothing.
        if notifier.successful::<State>().is_err() {
            tracing::warn!("a client went away while creating a dmabuf");
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // Both selections follow the keyboard focus.
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
        // Smithay deactivated the input method on the way.
        self.ime_focus_moved();
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.set_cursor(image);
    }

    fn touch_down_grab(&mut self, start_data: TouchGrabStartData<Self>) -> impl TouchGrab<Self> {
        // Each finger keeps the surface it went down on.
        TouchPointsGrab::new(start_data)
    }
}

impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        // Kept so that who owns the selection can be seen from outside
        // without taking part in it. Finding out by pasting would take the
        // keyboard focus, which is what decides whether a client may set
        // the selection in the first place.
        let mime_types = source.map(|source| source.mime_types());
        match target {
            SelectionTarget::Clipboard => self.selection = mime_types,
            SelectionTarget::Primary => self.primary_selection = mime_types,
        }
    }
}

impl State {
    /// The MIME types of a selection, if a client owns it.
    ///
    /// They are noted at `new_selection`. Smithay does not call it when the
    /// source goes away (its client destroys it, or disconnects): it clears
    /// the selection silently. Whether one is still there is asked with a
    /// type no source can offer (a Wayland string has no NUL in it); that
    /// fails without sending anything, as "no selection" or "not that type".
    pub(crate) fn selection_mime_types(&self, primary: bool) -> Option<Vec<String>> {
        use smithay::wayland::selection::{data_device, primary_selection};
        let noted = if primary {
            self.primary_selection.as_ref()
        } else {
            self.selection.as_ref()
        }?;
        let fd: std::os::fd::OwnedFd = std::fs::File::open("/dev/null").ok()?.into();
        let probe = "\0".to_string();
        let gone = if primary {
            matches!(
                primary_selection::request_primary_client_selection(&self.seat, probe, fd),
                Err(primary_selection::SelectionRequestError::NoSelection)
            )
        } else {
            matches!(
                data_device::request_data_device_client_selection(&self.seat, probe, fd),
                Err(data_device::SelectionRequestError::NoSelection)
            )
        };
        (!gone).then(|| noted.clone())
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl ExtDataControlHandler for State {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control_state
    }
}

impl WlrDataControlHandler for State {
    fn data_control_state(&mut self) -> &mut WlrDataControlState {
        &mut self.wlr_data_control_state
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for State {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.drag_icon = None;
        self.dragging = false;
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        self.drag_icon = None;
        self.dragging = false;
    }
}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        // The drag takes over the pointer from the implicit grab of the
        // button press that started it. Where the pointer is, and so which
        // surface is offered the data, keeps coming from the compositor's
        // own hit testing through `pointer_motion`.
        let pointer = match type_ {
            GrabType::Pointer => seat.get_pointer(),
            // Drag and drop by touch is not supported.
            GrabType::Touch => None,
        };
        let Some((pointer, start_data)) =
            pointer.and_then(|pointer| pointer.grab_start_data().map(|data| (pointer, data)))
        else {
            source.cancel();
            return;
        };
        self.drag_icon = icon;
        self.dragging = true;
        if let Some(icon) = &self.drag_icon {
            self.announce_outputs(icon);
        }
        pointer.set_grab(
            self,
            DnDGrab::new_pointer(&self.display_handle, start_data, source, seat),
            serial,
            Focus::Keep,
        );
    }
}

impl OutputHandler for State {}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = WindowId(self.allocate_id());
        self.windows.push(Window {
            id,
            toplevel: surface,
            activated: false,
            requested_size: None,
            states: Default::default(),
            reply_owed: false,
        });
        self.push_event(WlEvent::WindowCreated(id));
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // Where the positioner asks, relative to the parent's window
        // geometry, kept inside the window's bounds as far as it allows.
        let geometry = self.place_popup(&surface, &positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        if let Err(error) = self
            .popup_manager
            .track_popup(PopupKind::Xdg(surface.clone()))
        {
            tracing::warn!(?error, "a popup was created for a parent that is gone");
        }
        self.announce_outputs(surface.wl_surface());
        self.popups.push(surface.clone());
        self.popup_placement_event(&surface);
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let geometry = self.place_popup(&surface, &positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        // Whoever places popups may still move it before the flush.
        self.popup_placement_event(&surface);
        self.repositions.push((surface, token));
    }

    // -- requests that are the window manager's to decide -------------------

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.window_request(&surface, WindowRequest::Maximize, true);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.window_request(&surface, WindowRequest::Unmaximize, true);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        let output = output.and_then(|wanted| {
            self.outputs
                .iter()
                .find(|entry| entry.output.owns(&wanted))
                .map(|entry| entry.id)
        });
        self.window_request(&surface, WindowRequest::Fullscreen { output }, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.window_request(&surface, WindowRequest::Unfullscreen, true);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        self.window_request(&surface, WindowRequest::Minimize, false);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: WlSeat, serial: Serial) {
        if self.is_held_press(&seat, serial) {
            self.window_request(&surface, WindowRequest::Move, false);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        use xdg_toplevel::ResizeEdge;
        let edges = ResizeEdges {
            top: matches!(
                edges,
                ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
            ),
            bottom: matches!(
                edges,
                ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
            ),
            left: matches!(
                edges,
                ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
            ),
            right: matches!(
                edges,
                ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
            ),
        };
        if self.is_held_press(&seat, serial) {
            self.window_request(&surface, WindowRequest::Resize { edges }, false);
        }
    }

    fn show_window_menu(
        &mut self,
        surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
        location: Point<i32, Logical>,
    ) {
        self.window_request(
            &surface,
            WindowRequest::ShowMenu {
                position: (location.x, location.y),
            },
            false,
        );
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        // A menu: while it is open the keyboard goes to it, pointer events
        // stay with its client, and a press anywhere else dismisses it.
        // Smithay's popup grabs do exactly that.
        let Some(seat) = Seat::<State>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Ok(mut grab) = self.popup_manager.grab_popup(root, kind, &seat, serial) else {
            // Denied (a stale serial, not the topmost popup): Smithay has
            // told the client.
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            // An input method that grabbed the keyboard keeps it: the menu
            // gets the focus, and the keys the input method does not use
            // come back to it through the virtual keyboard. Replacing that
            // grab would leave the input method without keys, and Smithay
            // offers no way to give it back afterwards.
            let input_method_grab = seat.input_method().keyboard_grabbed()
                && keyboard.is_grabbed()
                && (self.menu_grab.is_some()
                    || !(keyboard.has_grab(serial)
                        || keyboard.has_grab(grab.previous_serial().unwrap_or(serial))));
            if input_method_grab {
                self.menu_grab = Some(grab.clone());
                self.set_keyboard_focus_surface(grab.current_grab());
            } else {
                if keyboard.is_grabbed()
                    && !(keyboard.has_grab(serial)
                        || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
                {
                    grab.ungrab(PopupUngrabStrategy::All);
                    return;
                }
                keyboard.set_focus(self, grab.current_grab(), serial);
                keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
            }
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
        // Touches are not grabbed; a touch outside dismisses the popup
        // (`State::touch_down`).
        self.popup_grab = Some(grab);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(index) = self.windows.iter().position(|w| w.toplevel == surface) {
            let window = self.windows.remove(index);
            self.forget_window(window.id);
            self.push_event(WlEvent::WindowDestroyed(window.id));
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        let owner = self.owning_window(surface.wl_surface());
        self.popups.retain(|p| p != &surface);
        self.popup_manager.cleanup();
        if let Some(window) = owner {
            self.push_event(WlEvent::WindowChanged(window));
        }
        // If it held a grab, the keyboard goes back to the window.
        self.sync_keyboard_focus();
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_for_root(surface.wl_surface()) {
            self.push_event(WlEvent::WindowChanged(id));
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_for_root(surface.wl_surface()) {
            self.push_event(WlEvent::WindowChanged(id));
        }
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.window_for_root(surface.wl_surface()) {
            self.push_event(WlEvent::WindowChanged(id));
        }
    }
}

impl XdgDecorationHandler for State {
    // hizukuri asks clients not to draw decorations, whatever they prefer.

    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        set_server_side_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(&toplevel);
    }
}

fn set_server_side_decoration(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ServerSide);
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        self.announce_outputs(&surface);
    }
}

/// Whether an activation token was made for an input event of the client
/// that had the keyboard focus.
struct TokenFromInput(bool);

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Every token is handed out; what it was made for decides how the
        // window manager may treat it. A serial counts when it came after
        // the keyboard focus last moved, to the client that asks.
        let keyboard = self.seat.get_keyboard();
        let from_input = match (&data.serial, &keyboard) {
            (Some((serial, seat)), Some(keyboard)) => {
                let our_seat = self.seat.owns(seat);
                let focused_client = keyboard
                    .current_focus()
                    .and_then(|surface| self.display_handle.get_client(surface.id()).ok())
                    .map(|client| client.id());
                let recent = keyboard
                    .last_enter()
                    .is_some_and(|enter| serial.is_no_older_than(&enter));
                our_seat && recent && focused_client.is_some() && focused_client == data.client_id
            }
            _ => false,
        };
        data.user_data
            .insert_if_missing(|| TokenFromInput(from_input));
        true
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let from_input = data.user_data.get::<TokenFromInput>().is_some_and(|t| t.0);
        // A token is good for one activation.
        self.activation_state.remove_token(&token);
        if let Some(window) = self.window_for_root(&surface) {
            self.push_event(WlEvent::WindowRequest {
                window,
                request: WindowRequest::Activate { from_input },
            });
        }
    }
}

smithay::delegate_dispatch2!(State);
