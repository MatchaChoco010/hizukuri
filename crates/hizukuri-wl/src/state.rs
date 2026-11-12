//! The Smithay-facing state and the operations [`crate::WlServer`] exposes.

use std::collections::HashMap;
use std::time::Duration;

use hizukuri_color::{AlphaMode, ImageDescription};
use smithay::backend::allocator::{Format, Fourcc, Modifier};
use smithay::backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState};
use smithay::desktop::PopupManager;
use smithay::input::keyboard::{FilterResult, XkbConfig};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, CursorImageSurfaceData, MotionEvent,
};
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Point, Rectangle, SERIAL_COUNTER, Size, Transform};
use smithay::wayland::compositor::{
    CompositorClientState, CompositorState, SurfaceAttributes, TraversalAction, with_states,
    with_surface_tree_downward,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::{FractionalScaleManagerState, with_fractional_scale};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::presentation::{PresentationFeedbackCachedState, PresentationState, Refresh};
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::ShmState;
use smithay::wayland::viewporter::ViewporterState;

use crate::surface::{collect_tree, surface_id, surface_size, take_buffer, with_surface_state};
use crate::{
    BufferUpdate, CursorState, DmabufConfig, OutputConfig, OutputId, ServerConfig, SurfaceId,
    SurfaceNode, SurfaceRole, WindowId, WindowInfo, WindowStates, WlError, WlEvent,
};

/// `CLOCK_MONOTONIC`, the clock `wp_presentation` timestamps use.
const CLOCK_MONOTONIC: u32 = 1;

/// Per-client data required by Smithay.
#[derive(Default)]
pub(crate) struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub(crate) struct Window {
    pub id: WindowId,
    pub toplevel: ToplevelSurface,
    /// What the compositor last asked for; applied at the next configure.
    pub activated: bool,
    pub requested_size: Option<(i32, i32)>,
    pub states: WindowStates,
    /// The client made a request the protocol wants answered with a
    /// configure, and none was sent since.
    pub reply_owed: bool,
}

pub(crate) struct OutputEntry {
    pub id: OutputId,
    pub output: Output,
    pub global: GlobalId,
    pub scale: f64,
}

pub(crate) struct State {
    pub display_handle: DisplayHandle,
    /// The loop [`crate::WlServer::dispatch_clients`] runs.
    pub loop_handle: calloop::LoopHandle<'static, State>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub dmabuf_state: DmabufState,
    /// Present when `zwp_linux_dmabuf_v1` is offered.
    pub _dmabuf_global: Option<DmabufGlobal>,
    /// The format code and modifier pairs that were offered.
    pub dmabuf_formats: std::collections::HashSet<(u32, u64)>,
    pub seat: Seat<State>,
    // Kept alive for their globals.
    pub _output_manager_state: OutputManagerState,
    pub _viewporter_state: ViewporterState,
    pub _decoration_state: XdgDecorationState,
    pub _presentation_state: PresentationState,
    pub _fractional_scale_state: FractionalScaleManagerState,
    pub _cursor_shape_state: CursorShapeManagerState,

    pub events: Vec<WlEvent>,
    pub windows: Vec<Window>,
    pub popups: Vec<PopupSurface>,
    /// Smithay's bookkeeping of popup grabs. The popups themselves are
    /// also in `popups`, which is what the surface trees are built from.
    pub popup_manager: PopupManager,
    /// The window the compositor wants the keyboard focus on. A popup grab
    /// overrides it while it lasts.
    wanted_keyboard_focus: Option<WindowId>,
    /// The outputs each window was said to be on. A window that is not
    /// listed is taken to be on every output.
    window_outputs: HashMap<WindowId, Vec<OutputId>>,
    /// Where popups of a window should stay, relative to the window
    /// geometry (x, y, width, height).
    popup_bounds: HashMap<WindowId, (i32, i32, i32, i32)>,
    /// The icon surface of a drag in progress.
    pub drag_icon: Option<WlSurface>,
    /// The MIME types of the current selection (clipboard), if a client
    /// owns it.
    pub selection: Option<Vec<String>>,
    /// Popups that committed for the first time and were not told their
    /// place yet. They are told at the next flush.
    pub popups_awaiting_configure: Vec<PopupSurface>,
    /// Popups whose client asked for a new place, with the token to
    /// answer with. Answered at the next flush, like new popups.
    pub repositions: Vec<(PopupSurface, u32)>,
    /// Whether a drag and drop holds the pointer.
    pub dragging: bool,
    pub surfaces: HashMap<SurfaceId, WlSurface>,
    pub outputs: Vec<OutputEntry>,
    pub cursor: CursorState,
    next_id: u64,
}

impl State {
    pub fn new(
        dh: &DisplayHandle,
        loop_handle: calloop::LoopHandle<'static, State>,
        config: &ServerConfig,
    ) -> Result<Self, WlError> {
        let compositor_state = CompositorState::new_v6::<Self>(dh);
        let xdg_shell_state = XdgShellState::new::<Self>(dh);
        let shm_state =
            ShmState::new::<Self>(dh, vec![wl_shm::Format::Abgr8888, wl_shm::Format::Xbgr8888]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(dh);
        let data_device_state = DataDeviceState::new::<Self>(dh);
        let viewporter_state = ViewporterState::new::<Self>(dh);
        let decoration_state = XdgDecorationState::new::<Self>(dh);
        let presentation_state = PresentationState::new::<Self>(dh, CLOCK_MONOTONIC);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(dh);
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(dh);
        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global = match &config.dmabuf {
            Some(dmabuf) => Some(create_dmabuf_global(dh, &mut dmabuf_state, dmabuf)?),
            None => None,
        };

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(dh, "seat0");
        seat.add_keyboard(XkbConfig::default(), 400, 25)
            .map_err(|e| WlError::Keyboard(format!("{e:?}")))?;
        seat.add_pointer();

        Ok(Self {
            display_handle: dh.clone(),
            loop_handle,
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            data_device_state,
            dmabuf_state,
            _dmabuf_global: dmabuf_global,
            dmabuf_formats: config
                .dmabuf
                .iter()
                .flat_map(|dmabuf| &dmabuf.formats)
                .map(|format| (format.fourcc, format.modifier))
                .collect(),
            seat,
            _output_manager_state: output_manager_state,
            _viewporter_state: viewporter_state,
            _decoration_state: decoration_state,
            _presentation_state: presentation_state,
            _fractional_scale_state: fractional_scale_state,
            _cursor_shape_state: cursor_shape_state,
            events: Vec::new(),
            windows: Vec::new(),
            popups: Vec::new(),
            popup_manager: PopupManager::default(),
            wanted_keyboard_focus: None,
            window_outputs: HashMap::new(),
            popup_bounds: HashMap::new(),
            drag_icon: None,
            selection: None,
            popups_awaiting_configure: Vec::new(),
            repositions: Vec::new(),
            dragging: false,
            surfaces: HashMap::new(),
            outputs: Vec::new(),
            cursor: CursorState::Named("default".to_string()),
            next_id: 1,
        })
    }

    pub fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // -- outputs ------------------------------------------------------------

    pub fn add_output(&mut self, config: &OutputConfig) -> OutputId {
        let id = OutputId(self.allocate_id());
        let output = Output::new(
            config.name.clone(),
            PhysicalProperties {
                size: Size::from((0, 0)),
                subpixel: Subpixel::Unknown,
                make: config.make.clone(),
                model: config.model.clone(),
                serial_number: String::new(),
            },
        );
        let mode = Mode {
            size: Size::from(config.mode_size),
            refresh: config.refresh_mhz,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(config.scale)),
            Some(Point::from(config.position)),
        );
        output.set_preferred(mode);
        let global = output.create_global::<State>(&self.display_handle);
        self.outputs.push(OutputEntry {
            id,
            output,
            global,
            scale: config.scale,
        });
        self.announce_outputs_to_all();
        id
    }

    /// Changes the mode, scale or position of an output.
    pub fn update_output(&mut self, id: OutputId, config: &OutputConfig) -> bool {
        let Some(entry) = self.outputs.iter_mut().find(|o| o.id == id) else {
            return false;
        };
        let mode = Mode {
            size: Size::from(config.mode_size),
            refresh: config.refresh_mhz,
        };
        entry.output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Fractional(config.scale)),
            Some(Point::from(config.position)),
        );
        entry.output.set_preferred(mode);
        entry.scale = config.scale;
        self.announce_outputs_to_all();
        true
    }

    /// Withdraws an output from clients.
    pub fn remove_output(&mut self, id: OutputId) -> bool {
        let Some(index) = self.outputs.iter().position(|o| o.id == id) else {
            return false;
        };
        let entry = self.outputs.remove(index);
        entry.output.leave_all();
        self.display_handle.remove_global::<State>(entry.global);
        for outputs in self.window_outputs.values_mut() {
            outputs.retain(|output| *output != id);
        }
        self.announce_outputs_to_all();
        true
    }

    fn announce_outputs_to_all(&self) {
        for surface in self.surfaces.values() {
            self.announce_outputs(surface);
        }
    }

    /// The outputs a surface is on.
    ///
    /// A surface of a window is on the outputs the window was put on, and
    /// on none before that. The cursor image and a drag icon follow the
    /// pointer and are taken to be on every output. A surface that has no
    /// part in anything yet (a new `wl_surface`) is on none.
    fn outputs_of(&self, surface: &WlSurface) -> Vec<OutputId> {
        if let Some(window) = self.owning_window(surface) {
            return self
                .window_outputs
                .get(&window)
                .cloned()
                .unwrap_or_default();
        }
        let follows_pointer = self.drag_icon.as_ref() == Some(surface)
            || matches!(&self.cursor, CursorState::Surface(id) if surface_id(surface) == Some(*id));
        if follows_pointer {
            self.outputs.iter().map(|o| o.id).collect()
        } else {
            Vec::new()
        }
    }

    /// Tells a surface which outputs it is on and which scale to prefer:
    /// the largest scale among them, so it looks sharp wherever it shows.
    pub fn announce_outputs(&self, surface: &WlSurface) {
        let on = self.outputs_of(surface);
        let mut scale: Option<f64> = None;
        for entry in &self.outputs {
            if on.contains(&entry.id) {
                entry.output.enter(surface);
                scale = Some(scale.map_or(entry.scale, |s| s.max(entry.scale)));
            } else {
                entry.output.leave(surface);
            }
        }
        // On no output (not placed yet, or its output was removed): tell it
        // the scale of the first one, so it has something to draw for.
        let Some(scale) = scale.or(self.outputs.first().map(|o| o.scale)) else {
            return;
        };
        with_states(surface, |states| {
            smithay::wayland::compositor::send_surface_state(
                surface,
                states,
                scale.ceil() as i32,
                Transform::Normal,
            );
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }

    /// Says which outputs a window is on. The first one is its main
    /// output: frame callbacks and presentation feedback follow it.
    pub fn set_window_outputs(&mut self, window: WindowId, outputs: &[OutputId]) {
        if self.window_outputs.get(&window).map(Vec::as_slice) == Some(outputs) {
            return;
        }
        self.window_outputs.insert(window, outputs.to_vec());
        for surface in self.surfaces_of_window(window) {
            self.announce_outputs(&surface);
        }
    }

    /// Every surface of a window: its tree and the trees of its popups.
    fn surfaces_of_window(&self, window: WindowId) -> Vec<WlSurface> {
        self.surfaces
            .values()
            .filter(|surface| self.owning_window(surface) == Some(window))
            .cloned()
            .collect()
    }

    /// Whether a surface is shown on an output as its main output. Decides
    /// who gets frame callbacks and feedback when that output shows a
    /// frame.
    fn main_output_is(&self, surface: &WlSurface, output: OutputId) -> bool {
        match self
            .owning_window(surface)
            .and_then(|window| self.window_outputs.get(&window))
        {
            Some(outputs) if !outputs.is_empty() => outputs.first() == Some(&output),
            // Not placed anywhere in particular (the cursor, a drag icon,
            // a window on no output): the first output drives it.
            _ => self.outputs.first().is_some_and(|o| o.id == output),
        }
    }

    // -- windows ------------------------------------------------------------

    pub fn window_for_root(&self, surface: &WlSurface) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|w| w.toplevel.wl_surface() == surface)
            .map(|w| w.id)
    }

    fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The window geometry (`xdg_surface.set_window_geometry`), or the
    /// extent of the root surface when the client did not set one.
    fn geometry(surface: &WlSurface) -> (i32, i32, i32, i32) {
        let explicit = with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
        });
        match explicit {
            Some(rect) => (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h),
            None => {
                let (w, h) = surface_size(surface);
                (0, 0, w, h)
            }
        }
    }

    pub fn window_info(&self, id: WindowId) -> Option<WindowInfo> {
        let window = self.window(id)?;
        let surface = window.toplevel.wl_surface();
        let (title, app_id) = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|data| {
                    let attributes = data.lock().unwrap_or_else(|e| e.into_inner());
                    (attributes.title.clone(), attributes.app_id.clone())
                })
                .unwrap_or((None, None))
        });
        let mapped = surface_size(surface) != (0, 0);
        let geometry = Self::geometry(surface);
        let has = |wanted: xdg_toplevel::State| {
            window
                .toplevel
                .with_committed_state(|state| state.is_some_and(|s| s.states.contains(wanted)))
        };
        let activated = has(xdg_toplevel::State::Activated);
        Some(WindowInfo {
            id,
            surface: surface_id(surface)?,
            title,
            app_id,
            mapped,
            size: mapped.then_some((geometry.2, geometry.3)),
            activated,
            maximized: has(xdg_toplevel::State::Maximized),
            fullscreen: has(xdg_toplevel::State::Fullscreen),
        })
    }

    pub fn window_surfaces(&self, id: WindowId) -> Vec<SurfaceNode> {
        let Some(window) = self.window(id) else {
            return Vec::new();
        };
        let root = window.toplevel.wl_surface();
        let geometry = Self::geometry(root);
        let mut nodes = Vec::new();
        // The root surface sits so that the window geometry starts at (0, 0).
        collect_tree(
            root,
            (-geometry.0, -geometry.1),
            SurfaceRole::Toplevel,
            &mut nodes,
        );
        self.collect_popups(root, (0, 0), &mut nodes);
        nodes
    }

    /// Appends the popups whose parent is `parent`, and their own popups.
    /// `parent_origin` is where the parent's window geometry starts.
    fn collect_popups(
        &self,
        parent: &WlSurface,
        parent_origin: (i32, i32),
        out: &mut Vec<SurfaceNode>,
    ) {
        for popup in &self.popups {
            if popup.get_parent_surface().as_ref() != Some(parent) {
                continue;
            }
            let surface = popup.wl_surface();
            // Position of the popup's window geometry relative to the
            // parent's window geometry.
            let location = popup.with_committed_state(|state| {
                state
                    .map(|s| (s.geometry.loc.x, s.geometry.loc.y))
                    .unwrap_or((0, 0))
            });
            let origin = (parent_origin.0 + location.0, parent_origin.1 + location.1);
            let geometry = Self::geometry(surface);
            collect_tree(
                surface,
                (origin.0 - geometry.0, origin.1 - geometry.1),
                SurfaceRole::Popup,
                out,
            );
            self.collect_popups(surface, origin, out);
        }
    }

    /// Says where the popups of a window should stay: a rectangle relative
    /// to the window geometry, usually the output the window is on.
    pub fn set_popup_bounds(&mut self, window: WindowId, bounds: Option<(i32, i32, i32, i32)>) {
        match bounds {
            Some(bounds) => self.popup_bounds.insert(window, bounds),
            None => self.popup_bounds.remove(&window),
        };
    }

    /// Where a surface's window geometry starts, relative to the window
    /// geometry of the toplevel it belongs to: zero for the toplevel
    /// itself, the accumulated popup positions for a popup.
    fn origin_in_window(&self, surface: &WlSurface) -> (i32, i32) {
        let mut origin = (0, 0);
        let mut current = surface.clone();
        for _ in 0..64 {
            let Some(popup) = self.popups.iter().find(|p| p.wl_surface() == &current) else {
                break;
            };
            // The position that was last sent, or is about to be.
            let location = popup.with_pending_state(|state| state.geometry.loc);
            origin = (origin.0 + location.x, origin.1 + location.y);
            let Some(parent) = popup.get_parent_surface() else {
                break;
            };
            current = parent;
        }
        origin
    }

    /// The geometry of a popup: what its positioner asks for, adjusted the
    /// way the positioner allows (flip, slide, resize) to stay inside the
    /// bounds of its window, when those are known.
    pub fn place_popup(
        &self,
        popup: &PopupSurface,
        positioner: &PositionerState,
    ) -> Rectangle<i32, smithay::utils::Logical> {
        let target = popup.get_parent_surface().and_then(|parent| {
            let window = self.owning_window(&parent)?;
            let (x, y, w, h) = *self.popup_bounds.get(&window)?;
            // The positioner works relative to the parent's geometry.
            let parent_origin = self.origin_in_window(&parent);
            Some(Rectangle::new(
                Point::from((x - parent_origin.0, y - parent_origin.1)),
                Size::from((w, h)),
            ))
        });
        match target {
            Some(target) => positioner.get_unconstrained_geometry(target),
            None => positioner.get_geometry(),
        }
    }

    /// Drops what was kept for a window that is gone.
    pub fn forget_window(&mut self, window: WindowId) {
        self.window_outputs.remove(&window);
        self.popup_bounds.remove(&window);
        if self.wanted_keyboard_focus == Some(window) {
            self.wanted_keyboard_focus = None;
        }
    }

    pub fn configure_window(&mut self, id: WindowId, size: Option<(i32, i32)>, activated: bool) {
        let Some(window) = self.windows.iter_mut().find(|w| w.id == id) else {
            return;
        };
        window.activated = activated;
        window.requested_size = size;
        apply_pending_state(window);
        if window.toplevel.is_initial_configure_sent()
            && window.toplevel.send_pending_configure().is_some()
        {
            window.reply_owed = false;
        }
    }

    pub fn set_window_states(&mut self, id: WindowId, states: WindowStates) {
        let Some(window) = self.windows.iter_mut().find(|w| w.id == id) else {
            return;
        };
        window.states = states;
        apply_pending_state(window);
        if window.toplevel.is_initial_configure_sent()
            && window.toplevel.send_pending_configure().is_some()
        {
            window.reply_owed = false;
        }
    }

    /// Sends the configures that are due at a flush: the first one of new
    /// popups, and the answer to a request of a toplevel that nobody
    /// answered (the protocol asks for a configure after a request to be
    /// maximized or fullscreen, even when nothing changes).
    pub fn send_owed_configures(&mut self) {
        for popup in std::mem::take(&mut self.popups_awaiting_configure) {
            if popup.alive()
                && !popup.is_initial_configure_sent()
                && let Err(error) = popup.send_configure()
            {
                tracing::warn!(?error, "failed to configure a popup");
            }
        }
        for (popup, token) in std::mem::take(&mut self.repositions) {
            if popup.alive() {
                popup.send_repositioned(token);
            }
        }
        for window in &mut self.windows {
            if std::mem::take(&mut window.reply_owed) && window.toplevel.is_initial_configure_sent()
            {
                window.toplevel.send_configure();
            }
        }
    }

    fn popup_of(&self, id: SurfaceId) -> Option<&PopupSurface> {
        let surface = self.surfaces.get(&id)?;
        self.popups.iter().find(|p| p.wl_surface() == surface)
    }

    pub fn popup_request(&self, id: SurfaceId) -> Option<crate::PopupRequest> {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner::ConstraintAdjustment;
        let popup = self.popup_of(id)?;
        let parent = popup.get_parent_surface()?;
        let window = self.owning_window(&parent)?;
        let (positioner, geometry) =
            popup.with_pending_state(|state| (state.positioner, state.geometry));
        let rect =
            |r: Rectangle<i32, smithay::utils::Logical>| (r.loc.x, r.loc.y, r.size.w, r.size.h);
        let allows = |x: ConstraintAdjustment, y: ConstraintAdjustment| {
            (
                positioner.constraint_adjustment.contains(x),
                positioner.constraint_adjustment.contains(y),
            )
        };
        Some(crate::PopupRequest {
            window,
            parent: surface_id(&parent)?,
            parent_origin: self.origin_in_window(&parent),
            anchor_rect: rect(positioner.anchor_rect),
            wanted: rect(positioner.get_geometry()),
            geometry: rect(geometry),
            may_flip: allows(ConstraintAdjustment::FlipX, ConstraintAdjustment::FlipY),
            may_slide: allows(ConstraintAdjustment::SlideX, ConstraintAdjustment::SlideY),
            may_resize: allows(ConstraintAdjustment::ResizeX, ConstraintAdjustment::ResizeY),
        })
    }

    pub fn place_popup_within(
        &mut self,
        id: SurfaceId,
        bounds: (i32, i32, i32, i32),
    ) -> Option<(i32, i32, i32, i32)> {
        let popup = self.popup_of(id)?;
        let target = Rectangle::new(
            Point::from((bounds.0, bounds.1)),
            Size::from((bounds.2, bounds.3)),
        );
        let geometry = popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
            state.geometry
        });
        Some((
            geometry.loc.x,
            geometry.loc.y,
            geometry.size.w,
            geometry.size.h,
        ))
    }

    pub fn set_popup_geometry(&mut self, id: SurfaceId, geometry: (i32, i32, i32, i32)) -> bool {
        let Some(popup) = self.popup_of(id) else {
            return false;
        };
        popup.with_pending_state(|state| {
            state.geometry = Rectangle::new(
                Point::from((geometry.0, geometry.1)),
                Size::from((geometry.2, geometry.3)),
            );
        });
        true
    }

    pub fn close_window(&mut self, id: WindowId) {
        if let Some(window) = self.window(id) {
            window.toplevel.send_close();
        }
    }

    // -- surfaces -----------------------------------------------------------

    pub fn take_buffer_update(&mut self, id: SurfaceId) -> Option<BufferUpdate> {
        take_buffer(self.surfaces.get(&id)?)
    }

    pub fn surface_accepts_input(&self, id: SurfaceId, x: f64, y: f64) -> bool {
        let Some(surface) = self.surfaces.get(&id) else {
            return false;
        };
        let (w, h) = surface_size(surface);
        if x < 0.0 || y < 0.0 || x >= w as f64 || y >= h as f64 {
            return false;
        }
        with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            match &guard.current().input_region {
                // No input region: the whole surface accepts input.
                None => true,
                Some(region) => region.contains((x.floor() as i32, y.floor() as i32)),
            }
        })
    }

    pub fn set_surface_color(
        &mut self,
        id: SurfaceId,
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> bool {
        let Some(surface) = self.surfaces.get(&id) else {
            return false;
        };
        with_states(surface, |states| {
            with_surface_state(states, |state| {
                state.image_description = image_description;
                state.alpha_mode = alpha_mode;
            })
        })
        .is_some()
    }

    /// Sends frame callbacks to the surfaces whose main output is
    /// `output`, or to every surface with `None`.
    pub fn send_frame_callbacks(&mut self, output: Option<OutputId>, time_ms: u32) {
        self.popup_manager.cleanup();
        let wanted = |state: &State, surface: &WlSurface| {
            output.is_none_or(|output| state.main_output_is(surface, output))
        };
        let roots = self.frame_roots();
        for root in roots {
            if !wanted(self, &root) {
                continue;
            }
            with_surface_tree_downward(
                &root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |_, states, _| {
                    let callbacks = std::mem::take(
                        &mut states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .frame_callbacks,
                    );
                    for callback in callbacks {
                        callback.done(time_ms);
                    }
                },
                |_, _, _| true,
            );
        }
    }

    /// The roots of every surface tree that is shown: windows, popups, the
    /// cursor image and the drag icon.
    fn frame_roots(&self) -> Vec<WlSurface> {
        let mut roots: Vec<WlSurface> = self
            .windows
            .iter()
            .map(|w| w.toplevel.wl_surface().clone())
            .chain(self.popups.iter().map(|p| p.wl_surface().clone()))
            .collect();
        if let CursorState::Surface(id) = &self.cursor
            && let Some(surface) = self.surfaces.get(id)
        {
            roots.push(surface.clone());
        }
        roots.extend(self.drag_icon.clone());
        roots
    }

    pub fn send_presented(
        &mut self,
        output: OutputId,
        time: Duration,
        refresh: Duration,
        seq: u64,
    ) {
        let Some(entry) = self.outputs.iter().find(|o| o.id == output) else {
            return;
        };
        let wl_output = entry.output.clone();
        for root in self.frame_roots() {
            if !self.main_output_is(&root, output) {
                continue;
            }
            with_surface_tree_downward(
                &root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |_, states, _| {
                    let callbacks = std::mem::take(
                        &mut states
                            .cached_state
                            .get::<PresentationFeedbackCachedState>()
                            .current()
                            .callbacks,
                    );
                    for callback in callbacks {
                        callback.presented(
                            &wl_output,
                            time,
                            Refresh::fixed(refresh),
                            seq,
                            wp_presentation_feedback::Kind::empty(),
                        );
                    }
                },
                |_, _, _| true,
            );
        }
    }

    // -- input --------------------------------------------------------------

    pub fn set_keyboard_focus(&mut self, window: Option<WindowId>) {
        self.wanted_keyboard_focus = window;
        self.sync_keyboard_focus();
    }

    /// Puts the keyboard focus where the compositor wants it. While a
    /// popup holds a grab, Smithay's grab keeps the focus on the popup and
    /// ignores this; once the grab is over, the same call takes effect.
    /// That is why it is repeated after everything that can end a grab.
    pub fn sync_keyboard_focus(&mut self) {
        let surface = self
            .wanted_keyboard_focus
            .and_then(|id| self.window(id))
            .map(|w| w.toplevel.wl_surface().clone());
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if keyboard.current_focus() == surface {
            return;
        }
        keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
    }

    pub fn keyboard_key(&mut self, keycode: u32, pressed: bool, time_us: u64) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        // xkb key codes are evdev codes plus 8.
        keyboard.input::<(), _>(
            self,
            (keycode + 8).into(),
            state,
            SERIAL_COUNTER.next_serial(),
            InputTime::from_micros(time_us),
            |_, _, _| FilterResult::Forward,
        );
    }

    pub fn pointer_motion(&mut self, target: Option<(SurfaceId, (f64, f64))>, time_us: u64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        // Smithay takes "pointer location" and "surface origin" in one
        // global space and sends their difference to the client. Hit testing
        // is done by the caller, so the surface-local position is passed as
        // the location with the surface origin at zero. Nothing else in
        // Smithay looks at the absolute values.
        let (focus, location) = match target {
            Some((id, local)) => match self.surfaces.get(&id) {
                Some(surface) => (
                    Some((surface.clone(), Point::from((0.0, 0.0)))),
                    Point::from(local),
                ),
                None => (None, Point::from((0.0, 0.0))),
            },
            None => (None, Point::from((0.0, 0.0))),
        };
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_micros(time_us),
            },
        );
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool, time_us: u64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_micros(time_us),
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        );
        // A press outside a grabbing popup has just dismissed it.
        self.sync_keyboard_focus();
    }

    pub fn pointer_axis(
        &mut self,
        horizontal: f64,
        vertical: f64,
        v120: Option<(i32, i32)>,
        source: crate::AxisSource,
        time_us: u64,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let mut frame = AxisFrame::new(InputTime::from_micros(time_us));
        frame = frame.source(match source {
            crate::AxisSource::Wheel => AxisSource::Wheel,
            crate::AxisSource::Finger => AxisSource::Finger,
            crate::AxisSource::Continuous => AxisSource::Continuous,
        });
        if horizontal != 0.0 {
            frame = frame.value(Axis::Horizontal, horizontal);
        }
        if vertical != 0.0 {
            frame = frame.value(Axis::Vertical, vertical);
        }
        if let Some((h, v)) = v120 {
            if h != 0 {
                frame = frame.v120(Axis::Horizontal, h);
            }
            if v != 0 {
                frame = frame.v120(Axis::Vertical, v);
            }
        }
        pointer.axis(self, frame);
    }

    pub fn pointer_frame(&mut self) {
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.frame(self);
        }
    }

    pub fn keyboard_focus(&self) -> Option<SurfaceId> {
        let surface = self.seat.get_keyboard()?.current_focus()?;
        surface_id(&surface)
    }

    pub fn pointer_focus(&self) -> Option<SurfaceId> {
        let surface = self.seat.get_pointer()?.current_focus()?;
        surface_id(&surface)
    }

    /// The surfaces of the icon of a drag in progress, back to front, with
    /// offsets relative to the pointer position.
    pub fn drag_icon_surfaces(&self) -> Vec<SurfaceNode> {
        let mut nodes = Vec::new();
        if let Some(surface) = &self.drag_icon {
            collect_tree(surface, (0, 0), SurfaceRole::DragIcon, &mut nodes);
        }
        nodes
    }

    pub fn cursor_surfaces(&self) -> Vec<SurfaceNode> {
        let CursorState::Surface(id) = &self.cursor else {
            return Vec::new();
        };
        let Some(surface) = self.surfaces.get(id) else {
            return Vec::new();
        };
        let hotspot = with_states(surface, |states| {
            states
                .data_map
                .get::<CursorImageSurfaceData>()
                .map(|data| data.lock().unwrap_or_else(|e| e.into_inner()).hotspot)
        })
        .map(|point| (point.x, point.y))
        .unwrap_or((0, 0));
        let mut nodes = Vec::new();
        collect_tree(
            surface,
            (-hotspot.0, -hotspot.1),
            SurfaceRole::Cursor,
            &mut nodes,
        );
        nodes
    }

    pub fn set_cursor(&mut self, status: CursorImageStatus) {
        let cursor = match status {
            CursorImageStatus::Hidden => CursorState::Hidden,
            CursorImageStatus::Named(icon) => CursorState::Named(icon.name().to_string()),
            CursorImageStatus::Surface(surface) => match surface_id(&surface) {
                Some(id) => CursorState::Surface(id),
                None => CursorState::Hidden,
            },
        };
        if cursor != self.cursor {
            self.cursor = cursor;
            if let CursorState::Surface(id) = &self.cursor
                && let Some(surface) = self.surfaces.get(id)
            {
                self.announce_outputs(surface);
            }
            self.events.push(WlEvent::CursorChanged);
        }
    }
}

/// Creates the `zwp_linux_dmabuf_v1` global: with feedback when the device
/// to allocate on is known, with the plain format list otherwise.
fn create_dmabuf_global(
    dh: &DisplayHandle,
    state: &mut DmabufState,
    config: &DmabufConfig,
) -> Result<DmabufGlobal, WlError> {
    let mut formats = Vec::new();
    for format in &config.formats {
        let code = Fourcc::try_from(format.fourcc).map_err(|_| {
            WlError::Dmabuf(format!("unknown DRM format code {:#x}", format.fourcc))
        })?;
        formats.push(Format {
            code,
            modifier: Modifier::from(format.modifier),
        });
    }
    Ok(match config.main_device {
        Some((major, minor)) => {
            let device = smithay::reexports::rustix::fs::makedev(major, minor);
            let feedback = DmabufFeedbackBuilder::new(device, formats)
                .build()
                .map_err(|e| WlError::Dmabuf(format!("building the feedback: {e}")))?;
            state.create_global_with_default_feedback::<State>(dh, &feedback)
        }
        None => state.create_global::<State>(dh, formats),
    })
}

/// Copies what the compositor asked for into the toplevel's pending state.
pub(crate) fn apply_pending_state(window: &Window) {
    window.toplevel.with_pending_state(|state| {
        state.size = window.requested_size.map(Size::from);
        for (wanted, flag) in [
            (window.activated, xdg_toplevel::State::Activated),
            (window.states.maximized, xdg_toplevel::State::Maximized),
            (window.states.fullscreen, xdg_toplevel::State::Fullscreen),
            (window.states.resizing, xdg_toplevel::State::Resizing),
        ] {
            if wanted {
                state.states.set(flag);
            } else {
                state.states.unset(flag);
            }
        }
    });
}
