//! The compositor core.
//!
//! [`Compositor`] owns the Wayland server, the window manager, the scene
//! and the backend, and moves information between them:
//!
//! ```text
//! clients ── proto-wl ──▶ events ──▶ window manager (logical placement)
//!                                          │
//!                                          ▼
//!                     scene (visual state, drawing, hit testing)
//!                                          │
//! backend ◀── present ◀────────────────────┘
//!    │
//!    └─▶ frames, input, outputs ──▶ core ──▶ proto-wl (focus, input)
//! ```
//!
//! The core does not draw and does not decide what is under the pointer.
//! Both belong to the [`Scene`](crate::scene::Scene), which the core reaches
//! through three ports: contents in, frames out, hit testing. That is the
//! place where a different way of drawing can be plugged in.
//!
//! The core has no event loop of its own. Whoever drives it calls
//! [`Compositor::dispatch_clients`] when clients have something to say and
//! [`Compositor::process`] whenever something may have happened. That works
//! from a calloop event loop (the `proto-compositor` binary) as well as from a test
//! that steps the compositor by hand.

mod constraints;

use std::collections::HashMap;
use std::io;
use std::time::Instant;

use crate::wm::{Point, Rect, Size};
use proto_backend::{AxisSource, Backend, BackendError, BackendEvent, InputEvent, OutputInfo};
use proto_color::{
    AlphaMode, CompositeSpace, DecodeTransform, EncodeTransform, ImageDescription, Primaries,
};

use crate::fallback;
use crate::wm::{Modifiers, WindowManager, WmRequest};
use proto_wl::{
    CursorState, OutputConfig, ServerConfig, SurfaceId, SurfaceRole, WlError, WlEvent, WlServer,
};
use serde_json::{Value, json};

use crate::control::{self, Request};
use crate::scene::{
    CursorImage, CursorView, DrawStatus, Hit, Scene, SceneError, SceneRequest, SurfaceView,
    WindowSnapshot, WindowView, WorldSnapshot,
};
use crate::timing::FrameTimes;
use crate::x11::X11Status;

type WmWindowId = crate::wm::WindowId;
type WmOutputId = crate::wm::OutputId;
type WlWindowId = proto_wl::WindowId;
type WlOutputId = proto_wl::OutputId;
type BackendOutputId = proto_backend::OutputId;

/// Errors of the compositor core.
#[derive(Debug, thiserror::Error)]
pub enum CompositorError {
    #[error(transparent)]
    Wayland(#[from] WlError),
    #[error(transparent)]
    Scene(#[from] SceneError),
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Options for [`Compositor::new`].
#[derive(Debug, Clone)]
pub struct CompositorOptions {
    pub server: ServerConfig,
    /// The color outputs are cleared to. Straight RGB, interpreted like
    /// content without color information (BT.709 primaries, gamma 2.2).
    pub background: [f32; 3],
    /// The space surfaces are blended in. Must match the render targets the
    /// backend creates.
    pub composite_space: CompositeSpace,
    /// Draw the pointer cursor into the frame. On a real display there is
    /// no other way to see where the pointer is; for verification captures
    /// it is usually better left out.
    pub draw_cursor: bool,
}

impl Default for CompositorOptions {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            background: [0.125, 0.125, 0.125],
            composite_space: CompositeSpace::default(),
            draw_cursor: false,
        }
    }
}

/// An output, as known to each part of the compositor.
struct OutputBinding {
    info: OutputInfo,
    wl: WlOutputId,
    layout: WmOutputId,
    /// Something changed since the last frame of this output.
    dirty: bool,
}

/// The compositor core. See the module documentation.
pub struct Compositor<B: Backend> {
    wl: WlServer,
    wm: Box<dyn WindowManager>,
    scene: Box<dyn Scene>,
    backend: B,
    outputs: Vec<OutputBinding>,
    options: CompositorOptions,

    /// Pointer position in the global logical space.
    pointer: (f64, f64),
    /// Whether the pointer has been moved at all. Until then it is nowhere,
    /// and no surface gets pointer focus just for appearing at the origin.
    pointer_active: bool,
    /// The modifier keys that are held.
    modifiers: Modifiers,
    /// Keys whose press the window manager (or the core) used. Their
    /// release is not passed to a client either.
    swallowed_keys: Vec<u32>,
    /// How many pointer buttons are held down.
    buttons_down: u32,
    /// The surface that receives pointer events while a button is held: the
    /// one the first button was pressed on.
    pointer_grab: Option<SurfaceId>,
    /// Where the pointer was last reported to clients: the surface and the
    /// position inside it.
    pointer_sent: Option<(SurfaceId, (f64, f64))>,
    /// What each window was last told by a configure.
    configured: HashMap<WmWindowId, (Option<Size>, bool)>,
    start: Instant,
    /// Surfaces whose buffers could not be read, to warn about each once.
    unreadable: std::collections::HashSet<SurfaceId>,
    /// How long recent frames took to draw and to present.
    frame_times: FrameTimes,
    /// What the core remembers about pointer constraints.
    constraints: constraints::ConstraintPolicy,
    /// The X11 display and xwayland-satellite, when the compositor serves
    /// one (see [`crate::x11`]).
    x11: Option<X11Status>,
    quit: bool,
}

fn to_wl(id: WmWindowId) -> WlWindowId {
    proto_wl::WindowId(id.0)
}

fn to_wm(id: WlWindowId) -> WmWindowId {
    crate::wm::WindowId(id.0)
}

impl<B: Backend> Compositor<B> {
    /// Creates the core with the fallback's window management
    /// ([`fallback::Windows`]). `scene` draws and hit tests.
    pub fn new(
        options: CompositorOptions,
        backend: B,
        scene: Box<dyn Scene>,
    ) -> Result<Self, CompositorError> {
        Self::with_window_manager(options, backend, scene, Box::new(fallback::Windows::new()))
    }

    /// Creates the core. `scene` draws and hit tests; `wm` places windows
    /// and decides the focus.
    pub fn with_window_manager(
        options: CompositorOptions,
        backend: B,
        scene: Box<dyn Scene>,
        wm: Box<dyn WindowManager>,
    ) -> Result<Self, CompositorError> {
        let wl = WlServer::new(&options.server)?;
        let mut compositor = Self {
            wl,
            wm,
            scene,
            backend,
            outputs: Vec::new(),
            options,
            pointer: (0.0, 0.0),
            pointer_active: false,
            modifiers: Modifiers::default(),
            swallowed_keys: Vec::new(),
            buttons_down: 0,
            pointer_grab: None,
            pointer_sent: None,
            configured: HashMap::new(),
            start: Instant::now(),
            unreadable: std::collections::HashSet::new(),
            frame_times: FrameTimes::new(),
            constraints: Default::default(),
            x11: None,
            quit: false,
        };
        // Pick up the outputs the backend announced at startup.
        compositor.process()?;
        Ok(compositor)
    }

    pub fn wl(&mut self) -> &mut WlServer {
        &mut self.wl
    }

    pub fn backend(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Whether a `quit` request was received.
    /// What the state dump says about the X11 display. Windows whose
    /// client is the running xwayland-satellite are X11 windows.
    pub fn set_x11_status(&mut self, status: Option<X11Status>) {
        self.x11 = status;
        self.sync_world();
    }

    /// Whether a window belongs to the running xwayland-satellite.
    fn is_x11(&self, info: Option<&proto_wl::WindowInfo>) -> bool {
        let satellite = self.x11.as_ref().and_then(|x11| x11.satellite_pid);
        satellite.is_some() && info.and_then(|i| i.client_pid) == satellite.map(|pid| pid as i32)
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Releases the outputs before the process exits.
    pub fn shutdown(&mut self) {
        self.backend.shutdown(self.scene.targets());
    }

    fn time_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    // -----------------------------------------------------------------------
    // Driving the core
    // -----------------------------------------------------------------------

    /// Accepts waiting clients and handles their requests, then processes
    /// what resulted.
    pub fn dispatch_clients(&mut self) -> Result<(), CompositorError> {
        self.wl.accept_clients()?;
        self.wl.dispatch_clients()?;
        self.process()
    }

    /// Handles everything that is pending: backend events, protocol events,
    /// and whatever those cause. Then sends queued events to clients.
    pub fn process(&mut self) -> Result<(), CompositorError> {
        // Handling one kind of event can produce the other, so loop until
        // both sources are dry. Bounded, to stay responsive if something
        // keeps generating events.
        for round in 0..16 {
            let backend_events = self.backend.drain_events();
            let wl_events = self.wl.drain_events();
            // The first round runs even without events: a control request
            // or an input event may have left something for the window
            // manager and the scene to react to.
            if round > 0 && backend_events.is_empty() && wl_events.is_empty() {
                break;
            }
            for event in backend_events {
                self.handle_backend_event(event)?;
            }
            for event in wl_events {
                self.handle_wl_event(event);
            }
            self.sync_world();
            self.run_window_manager();
            for request in self.scene.take_requests() {
                match request {
                    SceneRequest::Redraw => self.schedule_redraw(),
                }
            }
        }
        self.wl.flush_clients()?;
        Ok(())
    }

    /// Lets the window manager react to what happened, and makes the scene
    /// and the protocol side agree with what it decided.
    fn run_window_manager(&mut self) {
        if self.wm.dispatch() {
            self.sync_scene_with_layout();
            self.apply_focus();
            self.schedule_redraw();
            self.sync_world();
            self.refresh_pointer_focus();
        }
        for request in self.wm.take_requests() {
            match request {
                WmRequest::Close(window) => self.wl.close_window(to_wl(window)),
                WmRequest::ReleasePointerConstraint => self.release_pointer_constraint(),
                WmRequest::ReviewPointerConstraints => self.review_pointer_constraints(),
                WmRequest::SetStates {
                    window,
                    maximized,
                    fullscreen,
                } => self.wl.set_window_states(
                    to_wl(window),
                    proto_wl::WindowStates {
                        maximized,
                        fullscreen,
                        resizing: false,
                    },
                ),
            }
        }
    }

    /// Hands the scene the current state of the core.
    fn sync_world(&mut self) {
        let world = self.world_snapshot();
        self.scene.update_world(&world);
    }

    fn world_snapshot(&self) -> WorldSnapshot {
        let windows = self
            .wm
            .placements()
            .iter()
            .map(|placement| {
                let info = self.wl.window(to_wl(placement.window));
                WindowSnapshot {
                    window: placement.window,
                    title: info.as_ref().and_then(|i| i.title.clone()),
                    app_id: info.as_ref().and_then(|i| i.app_id.clone()),
                    mapped: info.as_ref().is_some_and(|i| i.mapped),
                    size: info.as_ref().and_then(|i| i.size),
                    activated: info.as_ref().is_some_and(|i| i.activated),
                    position: (placement.position.x, placement.position.y),
                    surfaces: self.wl.window_surfaces(to_wl(placement.window)),
                    parent: info.as_ref().and_then(|i| i.parent).map(to_wm),
                    min_size: info.as_ref().map_or((0, 0), |i| i.min_size),
                    max_size: info.as_ref().map_or((0, 0), |i| i.max_size),
                    x11: self.is_x11(info.as_ref()),
                }
            })
            .collect();
        WorldSnapshot {
            windows,
            outputs: self.outputs.iter().map(|o| o.info.clone()).collect(),
            focus: self.wm.focus(),
            pointer: self.pointer_active.then_some(self.pointer),
            cursor: self.cursor_view(),
            drag_icon: self.drag_icon_views(),
            background: self.options.background,
        }
    }

    // -----------------------------------------------------------------------
    // Backend events
    // -----------------------------------------------------------------------

    fn handle_backend_event(&mut self, event: BackendEvent) -> Result<(), CompositorError> {
        match event {
            BackendEvent::OutputAdded(info) => self.add_output(info),
            BackendEvent::OutputChanged(info) => self.change_output(info),
            BackendEvent::OutputRemoved(id) => self.remove_output(id),
            BackendEvent::Frame { output } => self.frame(output)?,
            BackendEvent::Presented {
                output,
                presentation,
            } => {
                // Frame callbacks tell clients it is a good time to draw the
                // next frame: right after one was shown. Both go to the
                // windows this output is the main output of.
                if let Some(binding) = self.outputs.iter().find(|o| o.info.id == output) {
                    self.wl.send_presented(
                        binding.wl,
                        presentation.time,
                        presentation.refresh,
                        presentation.sequence,
                    );
                    self.wl.send_frame_callbacks(
                        Some(binding.wl),
                        self.start.elapsed().as_millis() as u32,
                    );
                }
            }
            BackendEvent::Input(input) => self.handle_input(input),
            BackendEvent::SessionPaused => {
                tracing::info!("session paused");
            }
            BackendEvent::SessionResumed => {
                tracing::info!("session resumed");
                self.schedule_redraw();
            }
            BackendEvent::Reconfigure => {
                // The backend needs what draws to follow the displays.
                self.backend.reconfigure(self.scene.targets())?;
            }
            BackendEvent::QuitRequested => {
                tracing::info!("quit requested by the backend");
                self.quit = true;
            }
        }
        Ok(())
    }

    fn output_config(info: &OutputInfo) -> OutputConfig {
        OutputConfig {
            name: info.name.clone(),
            make: info.make.clone(),
            model: info.model.clone(),
            mode_size: (info.size.0 as i32, info.size.1 as i32),
            refresh_mhz: info.refresh_mhz as i32,
            scale: info.scale,
            position: info.position,
            image_description: info.image_description,
        }
    }

    fn output_rect(info: &OutputInfo) -> Rect {
        let (w, h) = info.logical_size();
        Rect::new(info.position.0, info.position.1, w, h)
    }

    fn add_output(&mut self, info: OutputInfo) {
        let wl = self.wl.add_output(&Self::output_config(&info));
        let layout = crate::wm::OutputId(info.id.0);
        self.wm.set_output(layout, Self::output_rect(&info));
        tracing::info!(
            name = %info.name,
            size = ?info.size,
            scale = info.scale,
            image_description = ?info.image_description.name(),
            "output added"
        );
        self.outputs.push(OutputBinding {
            info,
            wl,
            layout,
            dirty: false,
        });
        self.schedule_redraw();
    }

    fn change_output(&mut self, info: OutputInfo) {
        let Some(binding) = self.outputs.iter_mut().find(|o| o.info.id == info.id) else {
            return;
        };
        self.wl
            .update_output(binding.wl, &Self::output_config(&info));
        self.wm.set_output(binding.layout, Self::output_rect(&info));
        binding.info = info;
        self.sync_scene_with_layout();
        self.schedule_redraw();
    }

    fn remove_output(&mut self, id: BackendOutputId) {
        let Some(index) = self.outputs.iter().position(|o| o.info.id == id) else {
            return;
        };
        let binding = self.outputs.remove(index);
        tracing::info!(name = %binding.info.name, "output removed");
        self.wl.remove_output(binding.wl);
        // The layout moves the windows of the output elsewhere.
        self.wm.remove_output(binding.layout);
        self.sync_scene_with_layout();
        self.schedule_redraw();
    }

    /// Tells the scene the logical placement of every window.
    fn sync_scene_with_layout(&mut self) {
        for placement in self.wm.placements() {
            self.scene.window_moved(
                placement.window,
                (placement.position.x, placement.position.y),
            );
        }
    }

    /// The windows as the scene needs them: stacking order bottom to top,
    /// each with its surfaces back to front.
    fn window_views(&self) -> Vec<WindowView> {
        self.wm
            .placements()
            .iter()
            .map(|placement| WindowView {
                window: placement.window,
                surfaces: self
                    .wl
                    .window_surfaces(to_wl(placement.window))
                    .into_iter()
                    .map(surface_view)
                    .collect(),
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Protocol events
    // -----------------------------------------------------------------------

    fn handle_wl_event(&mut self, event: WlEvent) {
        match event {
            WlEvent::WindowCreated(id) => {
                let placement = self.wm.add_window(to_wm(id));
                self.scene.window_added(
                    placement.window,
                    (placement.position.x, placement.position.y),
                );
                self.apply_focus();
            }
            WlEvent::WindowChanged(id) => {
                if let Some(info) = self.wl.window(id)
                    && let Some((w, h)) = info.size
                {
                    self.wm.set_window_size(to_wm(id), Size::new(w, h));
                }
                self.schedule_redraw();
                self.refresh_pointer_focus();
            }
            WlEvent::WindowDestroyed(id) => {
                let id = to_wm(id);
                self.wm.remove_window(id);
                self.scene.window_removed(id);
                self.configured.remove(&id);
                self.apply_focus();
                self.schedule_redraw();
                self.refresh_pointer_focus();
            }
            WlEvent::SurfaceDestroyed(surface) => {
                self.unreadable.remove(&surface);
                self.scene.surface_removed(surface)
            }
            WlEvent::CursorChanged => {
                if self.options.draw_cursor {
                    self.schedule_redraw();
                }
            }
            // What a client asks to be (maximized, fullscreen, moved...) is
            // the window manager's to decide. It answers through
            // `WlServer::set_window_states` and its configure hints; the
            // protocol layer answers for it when it says nothing.
            WlEvent::WindowRequest { window, request } => {
                self.wm.window_request(to_wm(window), request);
            }
            // The popup has a place already (its positioner's, inside the
            // bounds given in `sync_placements`). A scene that draws windows
            // transformed can still move it here, before the flush at the
            // end of `process`, with `WlServer::place_popup_within`.
            WlEvent::PopupPlacement { .. } => {}
            WlEvent::PointerConstraintsChanged => self.update_pointer_constraints(),
            WlEvent::PointerLockWithdrawn { surface, hint } => {
                if let Some(hint) = hint {
                    self.warp_to_hint(surface, hint);
                }
                self.update_pointer_constraints();
            }
        }
    }

    /// Makes the protocol side agree with the layout about the focus: the
    /// keyboard focus, and the `activated` state of every window.
    fn apply_focus(&mut self) {
        self.wl.set_keyboard_focus(self.wm.focus().map(to_wl));
        let windows: Vec<WmWindowId> = self.wm.placements().iter().map(|p| p.window).collect();
        for window in windows {
            let hint = self.wm.configure_hint(window);
            let wanted = (hint.size, hint.activated);
            if self.configured.get(&window) != Some(&wanted) {
                self.wl.configure_window(
                    to_wl(window),
                    hint.size.map(|s| (s.width, s.height)),
                    hint.activated,
                );
                self.configured.insert(window, wanted);
            }
        }
        // Whether a lock or confinement may be in effect depends on the
        // focus.
        self.update_pointer_constraints();
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Handles an input event as if a device of the backend had produced
    /// it. The control interface injects input through this.
    pub fn handle_input(&mut self, event: InputEvent) {
        match event {
            InputEvent::Key {
                keycode,
                pressed,
                time_us,
            } => {
                self.modifiers.update(keycode, pressed);
                let used = if pressed {
                    let used =
                        self.emergency_key(keycode) || self.wm.key(keycode, true, self.modifiers);
                    if used {
                        self.swallowed_keys.push(keycode);
                    }
                    used
                } else if let Some(index) = self.swallowed_keys.iter().position(|k| *k == keycode) {
                    self.swallowed_keys.remove(index);
                    self.wm.key(keycode, false, self.modifiers);
                    true
                } else {
                    false
                };
                if used {
                    self.sync_world();
                    self.run_window_manager();
                } else {
                    self.wl.keyboard_key(keycode, pressed, time_us);
                }
            }
            InputEvent::PointerMotion {
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
                time_us,
            } => {
                self.relative_pointer_motion((dx, dy), (dx_unaccel, dy_unaccel), time_us);
            }
            InputEvent::PointerMotionAbsolute { x, y, time_us } => {
                self.absolute_pointer_motion((x, y), time_us);
            }
            InputEvent::PointerButton {
                button,
                pressed,
                time_us,
            } => {
                let hit = self.surface_under(self.pointer);
                let decided = self.wm.pointer_button(button, pressed, hit.as_ref());
                if pressed {
                    if !decided {
                        self.focus_window_under_pointer();
                    }
                    self.sync_world();
                    self.run_window_manager();
                    if self.buttons_down == 0 {
                        // The events go to this surface until every button
                        // is released, wherever the pointer moves.
                        self.pointer_grab = self.wl.pointer_focus();
                    }
                    self.buttons_down += 1;
                } else {
                    self.buttons_down = self.buttons_down.saturating_sub(1);
                }
                self.wl.pointer_button(button, pressed, time_us);
                self.wl.pointer_frame();
                if !pressed && self.buttons_down == 0 && self.pointer_grab.take().is_some() {
                    // The grab ended: the pointer may be over something else.
                    self.refresh_pointer_focus();
                }
            }
            InputEvent::PointerAxis {
                source,
                horizontal,
                vertical,
                v120,
                time_us,
            } => {
                let source = match source {
                    AxisSource::Wheel => proto_wl::AxisSource::Wheel,
                    AxisSource::Finger => proto_wl::AxisSource::Finger,
                    AxisSource::Continuous => proto_wl::AxisSource::Continuous,
                };
                self.wl
                    .pointer_axis(horizontal, vertical, v120, source, time_us);
                self.wl.pointer_frame();
            }
        }
    }

    /// Key combinations the core handles itself, before the window manager
    /// and before any client, so that they work whatever state a script is
    /// in. Ctrl+Alt+Shift+R reloads the script; Ctrl+Alt+Shift+F hands
    /// drawing and window management to the built-in fallback.
    fn emergency_key(&mut self, keycode: u32) -> bool {
        const KEY_R: u32 = 19;
        const KEY_F: u32 = 33;
        let m = self.modifiers;
        if !(m.ctrl && m.alt && m.shift) {
            return false;
        }
        let (command, args) = match keycode {
            KEY_R => ("script_reload", json!({})),
            KEY_F => ("script_mode", json!({ "mode": "fallback" })),
            _ => return false,
        };
        match self.scene.control(command, &args) {
            Some(result) => {
                if let Err(error) = result {
                    tracing::warn!("{command} failed: {error}");
                }
                self.schedule_redraw();
                true
            }
            // The scene is not a script: the keys mean nothing here.
            None => false,
        }
    }

    /// Keeps a relative pointer motion inside the area covered by outputs.
    fn clamp_to_outputs(&self, point: (f64, f64)) -> (f64, f64) {
        let mut bounds: Option<(f64, f64, f64, f64)> = None;
        for output in &self.outputs {
            let rect = Self::output_rect(&output.info);
            let (x0, y0) = (rect.x as f64, rect.y as f64);
            let (x1, y1) = (x0 + rect.width as f64, y0 + rect.height as f64);
            bounds = Some(match bounds {
                None => (x0, y0, x1, y1),
                Some(b) => (b.0.min(x0), b.1.min(y0), b.2.max(x1), b.3.max(y1)),
            });
        }
        match bounds {
            Some((x0, y0, x1, y1)) => (
                point.0.clamp(x0, (x1 - 1.0).max(x0)),
                point.1.clamp(y0, (y1 - 1.0).max(y0)),
            ),
            None => point,
        }
    }

    /// The surface the pointer belongs to at a position of the screen, and
    /// the position inside that surface.
    fn pointer_target(&self, position: (f64, f64)) -> Option<(SurfaceId, (f64, f64))> {
        // A lock or confinement in effect holds the pointer on its surface,
        // whatever is drawn above it.
        if let Some(surface) = self.constrained_surface() {
            let views = self.window_views();
            if let Some(local) = self.scene.locate(position, surface, &views) {
                return Some((surface, local));
            }
        }
        // A drag and drop offers the data to whatever is under the pointer.
        let grab = self.pointer_grab.filter(|_| !self.wl.drag_in_progress());
        match grab {
            // While a button is held, the surface it was pressed on keeps
            // getting the events, in its own coordinates.
            Some(surface) => {
                let views = self.window_views();
                self.scene
                    .locate(position, surface, &views)
                    .map(|local| (surface, local))
            }
            None => self
                .surface_under(position)
                .map(|hit| (hit.surface, hit.local)),
        }
    }

    fn move_pointer(&mut self, position: (f64, f64), time_us: u64) {
        self.pointer = position;
        self.pointer_active = true;
        // Whoever answers must know the windows as they are now.
        self.sync_world();
        let target = self.pointer_target(position);
        self.pointer_sent = target;
        self.wl.pointer_motion(target, time_us);
        self.wl.pointer_frame();
        // What follows the pointer has to be drawn at its new place.
        if self.options.draw_cursor || self.wl.drag_in_progress() {
            self.schedule_redraw();
        }
    }

    /// What to draw as the cursor, if anything.
    fn cursor_view(&self) -> Option<CursorView> {
        if !self.options.draw_cursor || !self.pointer_active {
            return None;
        }
        let image = match self.wl.cursor() {
            CursorState::Hidden => return None,
            // Named shapes all get the built-in arrow for now.
            CursorState::Named(_) => CursorImage::Default,
            CursorState::Surface(_) => CursorImage::Surfaces(
                self.wl
                    .cursor_surfaces()
                    .into_iter()
                    .map(surface_view)
                    .collect(),
            ),
        };
        Some(CursorView {
            position: self.pointer,
            image,
        })
    }

    /// The icon of a drag in progress. The core only passes it on: drawing
    /// it is up to the scene.
    fn drag_icon_views(&self) -> Vec<SurfaceView> {
        self.wl
            .drag_icon_surfaces()
            .into_iter()
            .map(surface_view)
            .collect()
    }

    /// Re-evaluates where a pointer that did not move is, after what is on
    /// screen changed: another surface may be under it now, or the same
    /// surface at another place (a window that is moved or turned).
    fn refresh_pointer_focus(&mut self) {
        if !self.pointer_active {
            return;
        }
        self.sync_world();
        let target = self.pointer_target(self.pointer);
        // Positions travel as 24.8 fixed point; less than that is no change.
        let same = match (target, self.pointer_sent) {
            (None, None) => true,
            (Some((a, p)), Some((b, q))) => {
                a == b && (p.0 - q.0).abs() < 1.0 / 256.0 && (p.1 - q.1).abs() < 1.0 / 256.0
            }
            _ => false,
        };
        // A locked pointer does not move on its surface, even when the
        // surface moves under it: no `wl_pointer.motion` while locked.
        let locked = matches!((target, self.pointer_sent),
            (Some((a, _)), Some((b, _))) if a == b && self.locked_to(a));
        if !same && !locked {
            self.pointer_sent = target;
            self.wl.pointer_motion(target, self.time_us());
            self.wl.pointer_frame();
        }
        self.update_pointer_constraints();
    }

    fn focus_window_under_pointer(&mut self) {
        if let Some(hit) = self.surface_under(self.pointer)
            && self.wm.focus_window(Some(hit.window))
        {
            self.apply_focus();
            self.schedule_redraw();
        }
    }

    /// Hit test: the topmost surface at a point of the global logical
    /// space, with the point in surface-local coordinates.
    ///
    /// The scene answers, because only the scene knows where windows were
    /// drawn. The core contributes what the protocol knows: whether a
    /// surface takes input at a surface-local point.
    fn surface_under(&self, point: (f64, f64)) -> Option<Hit> {
        let views = self.window_views();
        self.scene.hit_test(point, &views, &|surface, x, y| {
            self.wl.surface_accepts_input(surface, x, y)
        })
    }

    // -----------------------------------------------------------------------
    // Frames
    // -----------------------------------------------------------------------

    /// Marks every output as needing a new frame and asks the backend for
    /// one.
    fn schedule_redraw(&mut self) {
        self.sync_placements();
        for output in &mut self.outputs {
            output.dirty = true;
            self.backend.request_frame(output.info.id);
        }
    }

    /// Tells the protocol side where the layout put each window: which
    /// output it is on (clients are told, and pick their scale from it),
    /// and the area its popups should stay in (that output).
    fn sync_placements(&mut self) {
        for placement in self.wm.placements() {
            let window = to_wl(placement.window);
            let output = placement
                .output
                .and_then(|id| self.outputs.iter().find(|o| o.layout == id));
            match output {
                Some(output) => {
                    self.wl.set_window_outputs(window, &[output.wl]);
                    let area = Self::output_rect(&output.info);
                    self.wl.set_popup_bounds(
                        window,
                        Some((
                            area.x - placement.position.x,
                            area.y - placement.position.y,
                            area.width,
                            area.height,
                        )),
                    );
                }
                None => {
                    self.wl.set_window_outputs(window, &[]);
                    self.wl.set_popup_bounds(window, None);
                }
            }
        }
    }

    /// The backend says the output can take a frame.
    fn frame(&mut self, output: BackendOutputId) -> Result<(), CompositorError> {
        let dirty = self.outputs.iter().any(|o| o.info.id == output && o.dirty);
        if dirty {
            self.render_output(output)?;
        }
        Ok(())
    }

    /// Has the scene draw an output, and presents the frame.
    fn render_output(&mut self, output: BackendOutputId) -> Result<(), CompositorError> {
        if !self.backend.is_active() {
            // The session is paused: keep the output dirty and try again
            // when it resumes.
            return Ok(());
        }
        let Some(binding) = self.outputs.iter_mut().find(|o| o.info.id == output) else {
            return Ok(());
        };
        binding.dirty = false;
        let info = binding.info.clone();

        let started = Instant::now();
        self.sync_world();
        // Port 1: hand the scene what clients committed. The buffer
        // reference is dropped (and the buffer released to the client) once
        // the scene has taken the pixels.
        let views = self.window_views();
        let cursor = self.cursor_view();
        let cursor_surfaces = match &cursor {
            Some(CursorView {
                image: CursorImage::Surfaces(surfaces),
                ..
            }) => surfaces.as_slice(),
            _ => &[],
        };
        let drag_icon = self.drag_icon_views();
        for surface in views
            .iter()
            .flat_map(|view| view.surfaces.iter())
            .chain(cursor_surfaces)
            .chain(&drag_icon)
        {
            match self.wl.take_buffer_update(surface.surface) {
                // The scene takes the update itself: pixels are uploaded
                // during the call, and a scene that imports dmabufs keeps
                // the update (and with it the client's buffer) for as long
                // as it draws from it.
                Some(update) => match self.scene.surface_buffer(
                    surface.surface,
                    update,
                    surface.image_description,
                    surface.alpha_mode,
                ) {
                    Ok(()) => {}
                    Err(SceneError::Unreadable(error)) => {
                        // A client that sends such buffers sends one per
                        // frame; say it once per surface.
                        if self.unreadable.insert(surface.surface) {
                            tracing::warn!(
                                surface = ?surface.surface,
                                %error,
                                "cannot read a buffer; the surface shows nothing"
                            );
                        }
                        self.scene.surface_removed(surface.surface);
                    }
                    Err(error) => return Err(error.into()),
                },
                None if surface.source.is_none() => {
                    self.scene.surface_removed(surface.surface);
                }
                None => {}
            }
        }

        // Port 2: the scene draws into the target the backend hands out.
        let target = self.backend.render_target(output);
        let status = self.scene.draw(
            &info,
            target,
            self.options.background,
            &views,
            cursor.as_ref(),
        )?;
        let drawn = Instant::now();
        match status {
            DrawStatus::Drawn => {
                self.backend.present(self.scene.targets(), output)?;
                // A frame is when what is on screen changes, also for
                // reasons only the scene knows (an animation).
                self.refresh_pointer_focus();
            }
            DrawStatus::Kept => {
                // Nothing new to show. The output keeps its picture, but
                // clients must not wait forever for a frame callback that
                // only a presented frame would send.
                let wl = self
                    .outputs
                    .iter()
                    .find(|o| o.info.id == output)
                    .map(|o| o.wl);
                self.wl
                    .send_frame_callbacks(wl, self.start.elapsed().as_millis() as u32);
            }
        }
        self.frame_times.record(drawn - started, drawn.elapsed());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Control interface
    // -----------------------------------------------------------------------

    /// Executes a control request and returns the response.
    pub fn control(&mut self, request: &Request) -> Value {
        let response = match self.control_inner(request) {
            Ok(value) => control::ok(value),
            Err(message) => control::error(message),
        };
        // Requests change state; let the consequences play out before the
        // caller sees the response.
        if let Err(error) = self.process() {
            return control::error(format!("failed after handling the request: {error}"));
        }
        response
    }

    fn control_inner(&mut self, request: &Request) -> Result<Value, String> {
        let time_us = self.time_us();
        match request {
            Request::Ping => Ok(json!({})),
            Request::State => Ok(self.state_dump()),
            Request::Screenshot {
                path,
                composite_path,
                output,
            } => self.screenshot(path, composite_path.as_deref(), output.as_deref()),
            Request::Key { keycode, pressed } => {
                self.handle_input(InputEvent::Key {
                    keycode: *keycode,
                    pressed: *pressed,
                    time_us,
                });
                Ok(json!({}))
            }
            Request::PointerMotion { x, y } => {
                self.handle_input(InputEvent::PointerMotionAbsolute {
                    x: *x,
                    y: *y,
                    time_us,
                });
                Ok(json!({}))
            }
            Request::PointerMotionRelative {
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            } => {
                self.handle_input(InputEvent::PointerMotion {
                    dx: *dx,
                    dy: *dy,
                    dx_unaccel: dx_unaccel.unwrap_or(*dx),
                    dy_unaccel: dy_unaccel.unwrap_or(*dy),
                    time_us,
                });
                Ok(json!({ "position": [self.pointer.0, self.pointer.1] }))
            }
            Request::PointerButton { button, pressed } => {
                self.handle_input(InputEvent::PointerButton {
                    button: *button,
                    pressed: *pressed,
                    time_us,
                });
                Ok(json!({}))
            }
            Request::PointerAxis {
                horizontal,
                vertical,
                v120_horizontal,
                v120_vertical,
            } => {
                let v120 = (v120_horizontal.is_some() || v120_vertical.is_some())
                    .then(|| (v120_horizontal.unwrap_or(0), v120_vertical.unwrap_or(0)));
                self.handle_input(InputEvent::PointerAxis {
                    source: if v120.is_some() {
                        AxisSource::Wheel
                    } else {
                        AxisSource::Continuous
                    },
                    horizontal: *horizontal,
                    vertical: *vertical,
                    v120,
                    time_us,
                });
                Ok(json!({}))
            }
            Request::MoveWindow { window, x, y } => {
                let id = crate::wm::WindowId(*window);
                if self.wm.placement(id).is_none() {
                    return Err(format!("unknown window {window}"));
                }
                self.wm.move_window(id, Point::new(*x, *y));
                self.scene.window_moved(id, (*x, *y));
                self.schedule_redraw();
                self.refresh_pointer_focus();
                Ok(json!({}))
            }
            Request::FocusWindow { window } => {
                let id = window.map(crate::wm::WindowId);
                if let Some(id) = id
                    && self.wm.placement(id).is_none()
                {
                    return Err(format!("unknown window {}", id.0));
                }
                self.wm.focus_window(id);
                self.apply_focus();
                self.schedule_redraw();
                self.refresh_pointer_focus();
                Ok(json!({}))
            }
            Request::CloseWindow { window } => {
                let id = crate::wm::WindowId(*window);
                if self.wm.placement(id).is_none() {
                    return Err(format!("unknown window {window}"));
                }
                self.wl.close_window(to_wl(id));
                Ok(json!({}))
            }
            Request::SetWindowOpacity { window, opacity } => {
                if !self
                    .scene
                    .set_window_opacity(crate::wm::WindowId(*window), *opacity)
                {
                    return Err(format!("unknown window {window}"));
                }
                self.schedule_redraw();
                Ok(json!({}))
            }
            Request::SetSurfaceColor {
                surface,
                image_description,
                alpha_mode,
            } => {
                let description = ImageDescription::from_name(image_description)
                    .ok_or_else(|| format!("unknown image description {image_description:?}"))?;
                let alpha = match alpha_mode {
                    Some(name) => AlphaMode::from_name(name)
                        .ok_or_else(|| format!("unknown alpha mode {name:?}"))?,
                    None => AlphaMode::PremultipliedElectrical,
                };
                if !self
                    .wl
                    .set_surface_color(SurfaceId(*surface), description, alpha)
                {
                    return Err(format!("unknown surface {surface}"));
                }
                self.schedule_redraw();
                Ok(json!({}))
            }
            Request::SetOutputImageDescription {
                output,
                image_description,
            } => {
                let description = ImageDescription::from_name(image_description)
                    .ok_or_else(|| format!("unknown image description {image_description:?}"))?;
                let id = self
                    .outputs
                    .iter()
                    .find(|o| &o.info.name == output)
                    .map(|o| o.info.id)
                    .ok_or_else(|| format!("unknown output {output:?}"))?;
                // The backend reports the change (`OutputChanged`), which
                // reaches clients and the scene like any other.
                self.backend
                    .set_output_image_description(id, description)
                    .map_err(|e| e.to_string())?;
                Ok(json!({}))
            }
            Request::ScriptCall { function, args } => {
                self.scene_control("script_call", json!({ "function": function, "args": args }))
            }
            Request::ScriptReload { path } => {
                self.scene_control("script_reload", json!({ "path": path }))
            }
            Request::ScriptMode { mode } => {
                self.scene_control("script_mode", json!({ "mode": mode }))
            }
            Request::Quit => {
                self.quit = true;
                Ok(json!({}))
            }
        }
    }

    /// Passes a request on to the scene and lets the window manager and the
    /// outputs catch up with whatever it changed.
    fn scene_control(&mut self, command: &str, args: Value) -> Result<Value, String> {
        let result = self
            .scene
            .control(command, &args)
            .ok_or("this session does not run a script (it was started with --no-drawing)")?;
        self.sync_world();
        self.run_window_manager();
        self.schedule_redraw();
        self.refresh_pointer_focus();
        result.map(|value| json!({ "result": value }))
    }

    fn screenshot(
        &mut self,
        path: &std::path::Path,
        composite_path: Option<&std::path::Path>,
        output: Option<&str>,
    ) -> Result<Value, String> {
        let binding = match output {
            Some(name) => self.outputs.iter().find(|o| o.info.name == name),
            None => self.outputs.first(),
        }
        .ok_or_else(|| format!("unknown output {output:?}"))?;
        let id = binding.info.id;
        let name = binding.info.name.clone();

        // Pick up anything clients committed, then draw the current state
        // regardless of the frame clock.
        self.process().map_err(|e| e.to_string())?;
        self.render_output(id).map_err(|e| e.to_string())?;

        let target = self
            .backend
            .render_target(id)
            .ok_or("the output has no render target (started with --no-drawing?)")?;
        let capture = self.scene.capture(target).map_err(|e| e.to_string())?;
        let encoded = capture.encoded;
        encoded.write_png(path).map_err(|e| e.to_string())?;
        if let Some(composite_path) = composite_path {
            capture
                .composite
                .as_ref()
                .ok_or("the scene has no compositing buffer to write")?
                .write_exr(composite_path)
                .map_err(|e| e.to_string())?;
        }
        Ok(json!({
            "output": name,
            "width": encoded.width,
            "height": encoded.height,
            "format": encoded.format.name(),
            "image_description": describe_image(&encoded.image_description),
            "path": path,
            "composite_path": composite_path,
        }))
    }

    /// Everything a test may want to know, as JSON.
    fn state_dump(&self) -> Value {
        let space = self.options.composite_space;
        let outputs: Vec<Value> = self
            .outputs
            .iter()
            .map(|o| {
                json!({
                    "id": o.info.id.0,
                    "name": o.info.name,
                    "size": [o.info.size.0, o.info.size.1],
                    "logical_size": [o.info.logical_size().0, o.info.logical_size().1],
                    "position": [o.info.position.0, o.info.position.1],
                    "scale": o.info.scale,
                    "refresh_mhz": o.info.refresh_mhz,
                    "format": o.info.format.name(),
                    "image_description": describe_image(&o.info.image_description),
                    "encode_pipeline": EncodeTransform::describe(&space, &o.info.image_description),
                })
            })
            .collect();

        // Stacking order, bottom to top.
        let windows: Vec<Value> = self
            .wm
            .placements()
            .iter()
            .map(|placement| {
                let info = self.wl.window(to_wl(placement.window));
                let visual = self.scene.visual(placement.window);
                let surfaces: Vec<Value> = self
                    .wl
                    .window_surfaces(to_wl(placement.window))
                    .iter()
                    .map(|node| {
                        json!({
                            "id": node.surface.0,
                            "role": match node.role {
                                SurfaceRole::Toplevel => "toplevel",
                                SurfaceRole::Subsurface => "subsurface",
                                SurfaceRole::Popup => "popup",
                                SurfaceRole::Cursor => "cursor",
                                SurfaceRole::DragIcon => "drag-icon",
                                SurfaceRole::InputPopup => "input-popup",
                            },
                            "offset": [node.offset.0, node.offset.1],
                            "size": [node.size.0, node.size.1],
                            "buffer_size": node.buffer_size.map(|(w, h)| [w, h]),
                            "buffer_scale": node.buffer_scale,
                            "buffer_transform": node.transform.name(),
                            "buffer_kind": node.buffer_kind.map(|kind| kind.name()),
                            "generation": node.generation,
                            "has_contents": self.scene.has_contents(node.surface),
                            "image_description": describe_image(&node.image_description),
                            "color_source": node.color_source.name(),
                            "render_intent": match node.color_source {
                                proto_wl::ColorSource::Protocol(intent) => Some(intent.name()),
                                _ => None,
                            },
                            "alpha_mode": node.alpha_mode.name(),
                            "decode_pipeline": DecodeTransform::describe(
                                &node.image_description,
                                node.alpha_mode,
                                &space,
                            ),
                        })
                    })
                    .collect();
                json!({
                    "id": placement.window.0,
                    "title": info.as_ref().and_then(|i| i.title.clone()),
                    "app_id": info.as_ref().and_then(|i| i.app_id.clone()),
                    "mapped": info.as_ref().is_some_and(|i| i.mapped),
                    "activated": info.as_ref().is_some_and(|i| i.activated),
                    "maximized": info.as_ref().is_some_and(|i| i.maximized),
                    "fullscreen": info.as_ref().is_some_and(|i| i.fullscreen),
                    "parent": info.as_ref().and_then(|i| i.parent).map(|p| p.0),
                    "min_size": info.as_ref().map(|i| [i.min_size.0, i.min_size.1]),
                    "max_size": info.as_ref().map(|i| [i.max_size.0, i.max_size.1]),
                    "client_pid": info.as_ref().and_then(|i| i.client_pid),
                    "x11": self.is_x11(info.as_ref()),
                    "surface": info.as_ref().map(|i| i.surface.0),
                    "output": placement.output.map(|o| o.0),
                    "position": [placement.position.x, placement.position.y],
                    "size": placement.size.map(|s| [s.width, s.height]),
                    "visual": visual.map(|v| json!({
                        "position": [v.position.0, v.position.1],
                        "opacity": v.opacity,
                    })),
                    "surfaces": surfaces,
                })
            })
            .collect();

        let scene = self.scene.describe();

        json!({
            "backend": self.backend.name(),
            "session_active": self.backend.is_active(),
            "backend_status": self.backend.describe(),
            "frame_time_ms": self.frame_times.describe(),
            "socket": self.wl.socket_name(),
            "gpu": scene["gpu"],
            "scene": scene["kind"],
            // Whether a frame is still owed to an output.
            "redraw_pending": self.outputs.iter().any(|o| o.dirty),
            "window_manager": self.wm.name(),
            "script": scene["script"],
            // The icon of a drag in progress (offsets from the pointer).
            "drag_icon": self
                .wl
                .drag_icon_surfaces()
                .iter()
                .map(|node| {
                    json!({
                        "id": node.surface.0,
                        "offset": [node.offset.0, node.offset.1],
                        "size": [node.size.0, node.size.1],
                        "buffer_size": node.buffer_size.map(|(w, h)| [w, h]),
                        "has_contents": self.scene.has_contents(node.surface),
                    })
                })
                .collect::<Vec<_>>(),
            "composite_space": {
                "primaries": describe_primaries(&space.primaries),
                "transfer": "linear",
                "reference_white": 1.0,
            },
            "background": self.options.background,
            "outputs": outputs,
            "windows": windows,
            "focus": {
                "window": self.wm.focus().map(|w| w.0),
                "keyboard_surface": self.wl.keyboard_focus().map(|s| s.0),
                "pointer_surface": self.wl.pointer_focus().map(|s| s.0),
            },
            "selection": self.wl.selection_mime_types(),
            "primary_selection": self.wl.primary_selection_mime_types(),
            // Locks and confinements clients asked for; `active` is in effect.
            "pointer_constraints": self.describe_pointer_constraints(),
            "input_method": input_method_json(&self.wl.input_method_status()),
            "x11": self.x11.as_ref().map(|x11| json!({
                "display": x11.display,
                "satellite_pid": x11.satellite_pid,
                "satellite_starts": x11.starts,
                "satellite_exits": x11.exits,
            })),
            "pointer": {
                "position": self.pointer_active.then_some([self.pointer.0, self.pointer.1]),
                "cursor": match self.wl.cursor() {
                    CursorState::Hidden => json!("hidden"),
                    CursorState::Named(name) => json!({ "named": name }),
                    CursorState::Surface(id) => json!({ "surface": id.0 }),
                },
            },
            "frames_rendered": scene["frames_rendered"],
        })
    }
}

/// The input method part of `state`.
fn input_method_json(status: &proto_wl::InputMethodStatus) -> Value {
    let rect = |r: (i32, i32, i32, i32)| json!([r.0, r.1, r.2, r.3]);
    json!({
        "bound": status.bound,
        "active": status.active,
        "keyboard_grab": status.keyboard_grab,
        "popup": status.popup.as_ref().map(|popup| json!({
            "surface": popup.surface.0,
            "shown": popup.shown,
            "window": popup.window.map(|w| w.0),
            "position": popup.position.map(|(x, y)| json!([x, y])),
            "cursor_rectangle": rect(popup.cursor_rectangle),
            "sent_rectangle": popup.sent_rectangle.map(rect),
        })),
    })
}

fn surface_view(node: proto_wl::SurfaceNode) -> SurfaceView {
    SurfaceView {
        surface: node.surface,
        offset: node.offset,
        size: node.size,
        source: node.source,
        image_description: node.image_description,
        alpha_mode: node.alpha_mode,
    }
}

fn describe_primaries(primaries: &Primaries) -> Value {
    match primaries.name() {
        Some(name) => json!(name),
        None => json!({
            "red": [primaries.red.x, primaries.red.y],
            "green": [primaries.green.x, primaries.green.y],
            "blue": [primaries.blue.x, primaries.blue.y],
            "white": [primaries.white.x, primaries.white.y],
        }),
    }
}

fn describe_image(description: &ImageDescription) -> Value {
    json!({
        "name": description.name(),
        "primaries": describe_primaries(&description.primaries),
        "transfer": match description.transfer.name() {
            Some(name) => json!(name),
            None => json!(format!("{:?}", description.transfer)),
        },
        "luminance": {
            "min": description.luminance.min,
            "max": description.luminance.max,
            "reference": description.luminance.reference,
        },
        "mastering": description.mastering.map(|m| json!({
            "primaries": describe_primaries(&m.primaries),
            "min_luminance": m.min_luminance,
            "max_luminance": m.max_luminance,
            "max_cll": m.max_cll,
            "max_fall": m.max_fall,
        })),
    })
}
