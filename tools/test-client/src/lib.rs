//! A small Wayland client for verifying hizukuri.
//!
//! It opens one `xdg_toplevel` filled with a single color (drawn into a
//! `wl_shm` buffer) and records everything it receives: configures, keyboard
//! and pointer events, frame callbacks, buffer releases.
//!
//! The client never blocks on its own. [`TestClient::pump`] sends pending
//! requests, reads what has arrived (waiting up to a timeout) and handles
//! it. A test that runs the compositor in the same thread alternates between
//! pumping the compositor and pumping the client; a test that talks to a
//! compositor process uses [`TestClient::wait_for`].
//!
//! The client only ever connects to the socket it is given. It never looks
//! at `WAYLAND_DISPLAY`, so it cannot end up on the desktop session by
//! accident.

mod clipboard;
mod color;
mod external;
mod features;
mod ime;
mod pointer;
mod satellite;

use std::collections::HashMap;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device_manager, wl_keyboard, wl_output,
    wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1;
use wayland_protocols::wp::presentation_time::client::wp_presentation;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

pub use clipboard::{DataControlProtocol, SelectionKind};
pub use color::{ColorTarget, CreatorStep, ImageInfo};
pub use external::{ExternalDmabuf, create_udmabuf};
pub use features::{PopupOptions, SurfaceKind, ToplevelRequest};
pub use ime::{ImeEvent, InputMethodMode};
pub use pointer::ConstraintRequest;
pub use satellite::DialogOptions;
use xkbcommon::xkb;

/// Errors of the test client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("failed to connect: {0}")]
    Connect(String),
    #[error("Wayland protocol error: {0}")]
    Protocol(String),
}

/// Memory layout of the pixels the client draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelLayout {
    Argb8888,
    Xrgb8888,
    Abgr8888,
    Xbgr8888,
}

impl PixelLayout {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "argb8888" => PixelLayout::Argb8888,
            "xrgb8888" => PixelLayout::Xrgb8888,
            "abgr8888" => PixelLayout::Abgr8888,
            "xbgr8888" => PixelLayout::Xbgr8888,
            _ => return None,
        })
    }

    fn wl_format(self) -> wl_shm::Format {
        match self {
            PixelLayout::Argb8888 => wl_shm::Format::Argb8888,
            PixelLayout::Xrgb8888 => wl_shm::Format::Xrgb8888,
            PixelLayout::Abgr8888 => wl_shm::Format::Abgr8888,
            PixelLayout::Xbgr8888 => wl_shm::Format::Xbgr8888,
        }
    }

    /// Bytes of one pixel in memory for the stored channel values
    /// `[r, g, b, a]`.
    fn bytes(self, rgba: [u8; 4]) -> [u8; 4] {
        let [r, g, b, a] = rgba;
        match self {
            PixelLayout::Argb8888 | PixelLayout::Xrgb8888 => [b, g, r, a],
            PixelLayout::Abgr8888 | PixelLayout::Xbgr8888 => [r, g, b, a],
        }
    }
}

/// What a buffer is filled with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    /// [`ClientOptions::color`] everywhere.
    Solid,
    /// Four quadrants of the buffer as it is in memory: top left, top
    /// right, bottom left, bottom right. Stored channel values `[r, g, b,
    /// a]`.
    Quadrants([[u8; 4]; 4]),
}

/// How the buffer reaches the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferKind {
    /// `wl_shm`.
    Shm,
    /// `zwp_linux_dmabuf_v1`, with linear memory. The memory comes from a
    /// memfd, which a compositor can map like a dmabuf of a driver that
    /// allows it; no GPU is involved.
    Dmabuf,
}

/// `wl_surface.set_buffer_transform`. The values are those of
/// `wl_output.transform`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transform {
    #[default]
    Normal = 0,
    Rotate90 = 1,
    Rotate180 = 2,
    Rotate270 = 3,
    Flipped = 4,
    Flipped90 = 5,
    Flipped180 = 6,
    Flipped270 = 7,
}

impl Transform {
    pub const ALL: [Transform; 8] = [
        Transform::Normal,
        Transform::Rotate90,
        Transform::Rotate180,
        Transform::Rotate270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    pub fn swaps_axes(self) -> bool {
        (self as u32) % 2 == 1
    }

    pub(crate) fn wl(self) -> wl_output::Transform {
        match self {
            Transform::Normal => wl_output::Transform::Normal,
            Transform::Rotate90 => wl_output::Transform::_90,
            Transform::Rotate180 => wl_output::Transform::_180,
            Transform::Rotate270 => wl_output::Transform::_270,
            Transform::Flipped => wl_output::Transform::Flipped,
            Transform::Flipped90 => wl_output::Transform::Flipped90,
            Transform::Flipped180 => wl_output::Transform::Flipped180,
            Transform::Flipped270 => wl_output::Transform::Flipped270,
        }
    }
}

/// `wp_viewport` state: crop and scale.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Viewport {
    /// `set_source`: x, y, width, height in surface coordinates.
    pub source: Option<(f64, f64, f64, f64)>,
    /// `set_destination`: the size of the surface.
    pub destination: Option<(i32, i32)>,
}

/// What the client shows.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub title: String,
    pub app_id: String,
    /// Size used when the compositor lets the client choose.
    pub width: u32,
    pub height: u32,
    /// Stored channel values `[r, g, b, a]` of every pixel. `wl_shm` buffers
    /// are premultiplied; the client stores exactly these values and does
    /// not multiply for you.
    pub color: [u8; 4],
    pub layout: PixelLayout,
    pub pattern: Pattern,
    pub buffer_kind: BufferKind,
    pub buffer_transform: Transform,
    pub buffer_scale: i32,
    /// Size of the buffer in pixels. `None` derives it from the window
    /// size, the scale and the transform.
    pub buffer_size: Option<(u32, u32)>,
    pub viewport: Option<Viewport>,
    /// Ask for `wp_presentation` feedback with every frame drawn.
    pub presentation_feedback: bool,
    /// Open the window. Without it the client only binds globals (an input
    /// method has no window of its own).
    pub window: bool,
    /// Make a text input (`zwp_text_input_v3`), driven by the test.
    pub text_input: bool,
    /// Make a text input and enable it whenever it is entered, with this
    /// cursor rectangle (x, y, width, height), as an application with a
    /// text field does.
    pub text_input_enable_on_enter: Option<(i32, i32, i32, i32)>,
    /// Be the input method of the seat (`zwp_input_method_v2`).
    pub input_method: Option<InputMethodMode>,
    /// Bind `wp_color_manager_v1` with at most this version. `None`: do
    /// not bind it.
    pub color_manager_version: Option<u32>,
    /// `xdg_toplevel.set_min_size` and `set_max_size`, sent before the
    /// first commit.
    pub min_size: Option<(i32, i32)>,
    pub max_size: Option<(i32, i32)>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            title: "proto-test-client".to_string(),
            app_id: "hizukuri.test-client".to_string(),
            width: 200,
            height: 100,
            color: [255, 255, 255, 255],
            layout: PixelLayout::Argb8888,
            pattern: Pattern::Solid,
            buffer_kind: BufferKind::Shm,
            buffer_transform: Transform::Normal,
            buffer_scale: 1,
            buffer_size: None,
            viewport: None,
            presentation_feedback: false,
            window: true,
            text_input: false,
            text_input_enable_on_enter: None,
            input_method: None,
            color_manager_version: None,
            min_size: None,
            max_size: None,
        }
    }
}

/// Something the client received.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ClientEvent {
    /// `xdg_surface.configure`, with the toplevel state that came with it.
    Configure {
        width: i32,
        height: i32,
        activated: bool,
    },
    /// The client attached a buffer of this size and committed.
    Drawn {
        width: u32,
        height: u32,
    },
    /// A buffer the client attached earlier was released.
    BufferReleased,
    FrameDone {
        time_ms: u32,
    },
    CloseRequested,
    Keymap {
        size: u32,
    },
    KeyboardEnter,
    KeyboardLeave,
    /// A key. `keycode` is the evdev code. `keysym` and `text` come from the
    /// keymap the compositor sent.
    Key {
        keycode: u32,
        pressed: bool,
        keysym: String,
        text: String,
    },
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
    RepeatInfo {
        rate: i32,
        delay: i32,
    },
    PointerEnter {
        x: f64,
        y: f64,
    },
    PointerLeave,
    PointerMotion {
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    /// `axis` is 0 for vertical, 1 for horizontal.
    PointerAxis {
        axis: u32,
        value: f64,
    },
    PointerAxisValue120 {
        axis: u32,
        value: i32,
    },
    PointerFrame,
    /// The window was told it is, or no longer is, maximized, fullscreen
    /// or being resized (`xdg_toplevel.configure` states other than
    /// "activated"). Recorded when they change.
    WindowState {
        maximized: bool,
        fullscreen: bool,
        resizing: bool,
    },
    /// `wl_surface.preferred_buffer_scale`.
    PreferredBufferScale {
        scale: i32,
    },
    /// `wp_fractional_scale_v1.preferred_scale`, in 120ths.
    FractionalScale {
        scale_120: u32,
    },
    /// `wp_presentation_feedback.presented`.
    Presented {
        time_ns: u64,
        refresh_ns: u32,
        sequence: u64,
        /// The output the feedback named (`sync_output`), if any.
        output: Option<String>,
    },
    PresentationDiscarded,
    /// `zwp_linux_dmabuf_v1.modifier` (versions before feedback).
    DmabufModifier {
        fourcc: u32,
        modifier: u64,
    },
    /// The default `zwp_linux_dmabuf_feedback_v1`, once complete. Formats
    /// are `[fourcc, modifier]`.
    DmabufFeedback {
        main_device: [u32; 2],
        formats: Vec<[u64; 2]>,
    },
    /// A `wl_output`, once its description is complete.
    Output {
        name: String,
        width: i32,
        height: i32,
        scale: i32,
        x: i32,
        y: i32,
    },
    OutputRemoved {
        name: String,
    },
    /// `wl_surface.enter` of the window's surface.
    SurfaceEnter {
        output: String,
    },
    SurfaceLeave {
        output: String,
    },
    /// `xdg_popup.configure`: position relative to the parent's window
    /// geometry, and size.
    PopupConfigure {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// `xdg_popup.popup_done`: the compositor dismissed the popup.
    PopupDone,
    /// The selection changed: the MIME types on offer, or none.
    Selection {
        mime_types: Vec<String>,
    },
    /// Data read from the selection or from a drop.
    Received {
        mime_type: String,
        text: String,
    },
    /// Someone asked this client's data source for its data.
    SourceSend {
        mime_type: String,
    },
    SourceCancelled,
    /// Drag and drop, as the source: the target accepted this type (or
    /// nothing).
    SourceTarget {
        mime_type: Option<String>,
    },
    SourceDropPerformed,
    SourceFinished,
    /// Drag and drop, as a possible target.
    DragEnter {
        x: f64,
        y: f64,
        mime_types: Vec<String>,
    },
    DragMotion {
        x: f64,
        y: f64,
    },
    DragLeave,
    DragDrop,
    /// `zwp_relative_pointer_v1.relative_motion`: the motion after
    /// acceleration and before it, with its time in microseconds.
    RelativeMotion {
        time_us: u64,
        dx: f64,
        dy: f64,
        dx_unaccel: f64,
        dy_unaccel: f64,
    },
    /// `zwp_locked_pointer_v1.locked` and `unlocked`.
    PointerLocked,
    PointerUnlocked,
    /// `zwp_confined_pointer_v1.confined` and `unconfined`.
    PointerConfined,
    PointerUnconfined,
    /// The primary selection changed: the MIME types on offer, or none.
    PrimarySelection {
        mime_types: Vec<String>,
    },
    /// Someone asked this client's primary selection source for its data.
    PrimarySourceSend {
        mime_type: String,
    },
    PrimarySourceCancelled,
    /// A data control device saw a selection change.
    ControlSelection {
        protocol: DataControlProtocol,
        selection: SelectionKind,
        mime_types: Vec<String>,
    },
    /// The data control device is no longer valid.
    ControlFinished,
    /// Someone asked a source this client set through data control for its
    /// data.
    ControlSourceSend {
        mime_type: String,
    },
    ControlSourceCancelled,
    /// `zwp_text_input_v3.enter`.
    TextInputEnter,
    TextInputLeave,
    /// `zwp_text_input_v3.preedit_string`: the text being composed.
    Preedit {
        text: String,
        cursor_begin: i32,
        cursor_end: i32,
    },
    /// `zwp_text_input_v3.commit_string`: finished text.
    CommitString {
        text: String,
    },
    DeleteSurroundingText {
        before: u32,
        after: u32,
    },
    /// `zwp_text_input_v3.done`, with the number of commits the client had
    /// sent by then.
    TextInputDone {
        serial: u32,
        commits: u32,
    },
    /// What `wp_color_manager_v1` advertised, as the protocol's numbers,
    /// once `done` came.
    ColorManagerSupport {
        version: u32,
        intents: Vec<u32>,
        features: Vec<u32>,
        transfer_functions: Vec<u32>,
        primaries: Vec<u32>,
    },
    /// An image description became ready. `version` is that of the event
    /// (1: `ready`, 2: `ready2`).
    ImageDescriptionReady {
        index: usize,
        identity: u64,
        version: u32,
    },
    ImageDescriptionFailed {
        index: usize,
        cause: u32,
        message: String,
    },
    /// What `get_information` delivered, once `done` came.
    ImageDescriptionInfo {
        index: usize,
        info: ImageInfo,
    },
    /// `wp_color_management_surface_feedback_v1.preferred_changed` (or
    /// `preferred_changed2`).
    PreferredChanged {
        identity: u64,
    },
    /// `wp_color_management_output_v1.image_description_changed`.
    OutputImageDescriptionChanged {
        output: String,
    },
    /// `xdg_surface.configure` of a dialog (see
    /// [`TestClient::open_dialog`]), with the size of its toplevel
    /// configure.
    DialogConfigure {
        dialog: usize,
        width: i32,
        height: i32,
    },
}

#[derive(Default)]
struct Globals {
    compositor: Option<wl_compositor::WlCompositor>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional_scale: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    presentation: Option<wp_presentation::WpPresentation>,
    data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    relative_pointer_manager: Option<
        wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    >,
    pointer_constraints: Option<
        wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
    >,
    primary_selection_manager: Option<
        wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    >,
    ext_data_control_manager: Option<
        wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1,
    >,
    wlr_data_control_manager: Option<
        wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    >,
}

struct Window {
    surface: wl_surface::WlSurface,
    _xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    viewport: Option<wp_viewport::WpViewport>,
    _fractional_scale: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    /// Size and state from the latest `xdg_toplevel.configure`.
    pending: (i32, i32, bool),
    /// Maximized, fullscreen, resizing: as in the latest
    /// `xdg_toplevel.configure`, and as last reported.
    pending_states: [bool; 3],
    states: [bool; 3],
    /// Size of the buffer currently shown.
    drawn: Option<(u32, u32)>,
    configured: bool,
}

struct Keyboard {
    context: xkb::Context,
    state: Option<xkb::State>,
}

struct State {
    options: ClientOptions,
    globals: Globals,
    window: Option<Window>,
    subsurfaces: Vec<(wl_surface::WlSurface, wl_subsurface::WlSubsurface)>,
    keyboard: Keyboard,
    keyboard_object: Option<wl_keyboard::WlKeyboard>,
    pointer_object: Option<wl_pointer::WlPointer>,
    events: Vec<ClientEvent>,
    /// Redraw with the current options at the next opportunity.
    needs_draw: bool,
    closed: bool,
    /// Serial of the latest input event; requests that need one use it.
    last_serial: u32,
    /// Serial of the latest pointer button press, for starting a drag.
    last_press_serial: u32,
    keyboard_focus: Option<SurfaceKind>,
    pointer_focus: Option<SurfaceKind>,
    popup: Option<features::Popup>,
    outputs: HashMap<ObjectId, features::OutputEntry>,
    data: features::DataState,
    dmabuf_feedback: features::FeedbackState,
    pointer: pointer::PointerState,
    clipboard: clipboard::ClipboardState,
    color: color::ColorState,
    /// Never create a window (a clipboard manager has none).
    windowless: bool,
    ime: ime::ImeState,
    satellite: satellite::SatelliteState,
}

/// The test client. See the crate documentation.
pub struct TestClient {
    connection: Connection,
    queue: EventQueue<State>,
    handle: QueueHandle<State>,
    state: State,
    /// Index of the first event `wait_for` has not looked at yet.
    cursor: usize,
}

impl TestClient {
    /// Connects over an already connected stream (e.g. one end of a socket
    /// pair whose other end was given to the compositor).
    pub fn connect(stream: UnixStream, options: ClientOptions) -> Result<Self, ClientError> {
        let connection =
            Connection::from_socket(stream).map_err(|e| ClientError::Connect(e.to_string()))?;
        let queue: EventQueue<State> = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let state = State {
            options,
            globals: Globals::default(),
            window: None,
            subsurfaces: Vec::new(),
            keyboard: Keyboard {
                context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
                state: None,
            },
            keyboard_object: None,
            pointer_object: None,
            events: Vec::new(),
            needs_draw: false,
            closed: false,
            last_serial: 0,
            last_press_serial: 0,
            keyboard_focus: None,
            pointer_focus: None,
            popup: None,
            outputs: HashMap::new(),
            data: features::DataState::default(),
            dmabuf_feedback: features::FeedbackState::default(),
            pointer: pointer::PointerState::default(),
            clipboard: clipboard::ClipboardState::default(),
            color: color::ColorState::default(),
            windowless: false,
            ime: ime::ImeState::default(),
            satellite: satellite::SatelliteState::default(),
        };
        Ok(Self {
            connection,
            queue,
            handle,
            state,
            cursor: 0,
        })
    }

    /// Connects without ever creating a window, as a clipboard manager
    /// does. It has no surface, so it never has the focus.
    pub fn connect_windowless(stream: UnixStream) -> Result<Self, ClientError> {
        let mut client = Self::connect(stream, ClientOptions::default())?;
        client.state.windowless = true;
        Ok(client)
    }

    /// Connects to the Wayland socket at `path`.
    pub fn connect_path(path: &Path, options: ClientOptions) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path)
            .map_err(|e| ClientError::Connect(format!("{}: {e}", path.display())))?;
        Self::connect(stream, options)
    }

    /// Sends pending requests, reads events (waiting up to `timeout` for
    /// some to arrive; `None` does not wait) and handles them.
    pub fn pump(&mut self, timeout: Option<Duration>) -> Result<(), ClientError> {
        let protocol = |e: wayland_client::DispatchError| ClientError::Protocol(e.to_string());
        let backend =
            |e: wayland_client::backend::WaylandError| ClientError::Protocol(e.to_string());

        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(protocol)?;
        self.state.advance(&self.handle)?;
        self.connection.flush().map_err(backend)?;

        if let Some(guard) = self.connection.prepare_read() {
            let readable = {
                let fd = guard.connection_fd();
                let mut fds = [rustix::event::PollFd::new(
                    &fd,
                    rustix::event::PollFlags::IN,
                )];
                let timeout = timeout.unwrap_or(Duration::ZERO);
                let timespec = rustix::event::Timespec {
                    tv_sec: timeout.as_secs() as i64,
                    tv_nsec: timeout.subsec_nanos() as _,
                };
                match rustix::event::poll(&mut fds, Some(&timespec)) {
                    Ok(n) => n > 0,
                    Err(rustix::io::Errno::INTR) => false,
                    Err(e) => return Err(ClientError::Io(e.into())),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(backend(e)),
                }
            }
        }

        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(protocol)?;
        self.state.advance(&self.handle)?;
        self.connection.flush().map_err(backend)?;
        Ok(())
    }

    /// Pumps until an event that `wait_for` has not returned or skipped yet
    /// matches `predicate`, or `timeout` passes. Events before the match are
    /// skipped for later calls, so consecutive calls see events in order.
    pub fn wait_for(
        &mut self,
        timeout: Duration,
        mut predicate: impl FnMut(&ClientEvent) -> bool,
    ) -> Result<Option<ClientEvent>, ClientError> {
        let deadline = Instant::now() + timeout;
        loop {
            while self.cursor < self.state.events.len() {
                let event = &self.state.events[self.cursor];
                self.cursor += 1;
                if predicate(event) {
                    return Ok(Some(event.clone()));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.pump(Some((deadline - now).min(Duration::from_millis(50))))?;
        }
    }

    /// Everything received so far, oldest first.
    pub fn events(&self) -> &[ClientEvent] {
        &self.state.events
    }

    /// Forgets the events recorded so far.
    pub fn clear_events(&mut self) {
        self.state.events.clear();
        self.cursor = 0;
    }

    /// Whether the window has been configured and has drawn at least once.
    pub fn is_drawn(&self) -> bool {
        self.state
            .window
            .as_ref()
            .is_some_and(|w| w.drawn.is_some())
    }

    /// Whether the compositor asked the window to close.
    pub fn is_closed(&self) -> bool {
        self.state.closed
    }

    /// Changes the fill color (stored channel values) and redraws at the
    /// next pump.
    pub fn set_color(&mut self, color: [u8; 4]) {
        self.state.options.color = color;
        self.state.needs_draw = true;
    }

    /// Replaces what the window shows (pattern, transform, scale, viewport,
    /// buffer kind...) and redraws at the next pump. Title and app id are
    /// not sent again.
    pub fn redraw_with(&mut self, change: impl FnOnce(&mut ClientOptions)) {
        change(&mut self.state.options);
        self.state.needs_draw = true;
    }

    /// Which of this client's surfaces has the keyboard focus.
    pub fn keyboard_focus(&self) -> Option<SurfaceKind> {
        self.state.keyboard_focus
    }

    /// Which of this client's surfaces the pointer is on.
    pub fn pointer_focus(&self) -> Option<SurfaceKind> {
        self.state.pointer_focus
    }

    /// Adds a subsurface with its own solid color at a position relative to
    /// the window. It appears after the next pump.
    pub fn add_subsurface(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        color: [u8; 4],
    ) -> Result<(), ClientError> {
        let state = &mut self.state;
        let (Some(compositor), Some(subcompositor), Some(shm), Some(window)) = (
            &state.globals.compositor,
            &state.globals.subcompositor,
            &state.globals.shm,
            &state.window,
        ) else {
            return Err(ClientError::Protocol(
                "the window does not exist yet".to_string(),
            ));
        };
        let surface = compositor.create_surface(&self.handle, ());
        let subsurface = subcompositor.get_subsurface(&surface, &window.surface, &self.handle, ());
        subsurface.set_position(x, y);
        subsurface.set_desync();
        let buffer = create_buffer(
            shm,
            &self.handle,
            width,
            height,
            color,
            state.options.layout,
        )?;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
        // The position takes effect when the parent commits.
        window.surface.commit();
        state.subsurfaces.push((surface, subsurface));
        Ok(())
    }
}

impl State {
    /// Moves the client forward: creates the window once the globals are
    /// there, and draws when a draw is due.
    fn advance(&mut self, handle: &QueueHandle<State>) -> Result<(), ClientError> {
        // The window is created once; after a close request it stays gone.
        if self.window.is_none()
            && !self.closed
            && !self.windowless
            && self.options.window
            && let (Some(compositor), Some(wm_base), Some(_)) = (
                &self.globals.compositor,
                &self.globals.wm_base,
                &self.globals.shm,
            )
        {
            let surface = compositor.create_surface(handle, ());
            let xdg_surface = wm_base.get_xdg_surface(&surface, handle, ());
            let toplevel = xdg_surface.get_toplevel(handle, ());
            toplevel.set_title(self.options.title.clone());
            toplevel.set_app_id(self.options.app_id.clone());
            if let Some((width, height)) = self.options.min_size {
                toplevel.set_min_size(width, height);
            }
            if let Some((width, height)) = self.options.max_size {
                toplevel.set_max_size(width, height);
            }
            let viewport = self
                .globals
                .viewporter
                .as_ref()
                .map(|viewporter| viewporter.get_viewport(&surface, handle, ()));
            let fractional_scale = self
                .globals
                .fractional_scale
                .as_ref()
                .map(|manager| manager.get_fractional_scale(&surface, handle, ()));
            // The first commit has no buffer; it asks for the configure.
            surface.commit();
            self.window = Some(Window {
                surface,
                _xdg_surface: xdg_surface,
                toplevel,
                viewport,
                _fractional_scale: fractional_scale,
                pending: (0, 0, false),
                pending_states: [false; 3],
                states: [false; 3],
                drawn: None,
                configured: false,
            });
        }

        if self.needs_draw
            && let (Some(window), Some(shm)) = (&mut self.window, &self.globals.shm)
            && window.configured
        {
            let options = &self.options;
            // The size of the window in surface coordinates.
            let width = if window.pending.0 > 0 {
                window.pending.0 as u32
            } else {
                options.width
            };
            let height = if window.pending.1 > 0 {
                window.pending.1 as u32
            } else {
                options.height
            };
            let scale = options.buffer_scale.max(1) as u32;
            let (buffer_width, buffer_height) = options.buffer_size.unwrap_or({
                let scaled = (width * scale, height * scale);
                if options.buffer_transform.swaps_axes() {
                    (scaled.1, scaled.0)
                } else {
                    scaled
                }
            });
            let pixels = features::fill(
                buffer_width,
                buffer_height,
                options.pattern,
                options.color,
                options.layout,
            );
            let buffer = match options.buffer_kind {
                BufferKind::Shm => create_shm_buffer(
                    shm,
                    handle,
                    buffer_width,
                    buffer_height,
                    &pixels,
                    options.layout,
                )?,
                BufferKind::Dmabuf => {
                    let dmabuf = self.globals.dmabuf.as_ref().ok_or_else(|| {
                        ClientError::Protocol(
                            "the compositor does not offer zwp_linux_dmabuf_v1".to_string(),
                        )
                    })?;
                    features::create_dmabuf_buffer(
                        dmabuf,
                        handle,
                        buffer_width,
                        buffer_height,
                        &pixels,
                        options.layout,
                    )?
                }
            };
            window.surface.frame(handle, ());
            if options.presentation_feedback
                && let Some(presentation) = &self.globals.presentation
            {
                presentation.feedback(&window.surface, handle, features::FeedbackData::default());
            }
            window.surface.set_buffer_scale(options.buffer_scale.max(1));
            window
                .surface
                .set_buffer_transform(options.buffer_transform.wl());
            if let Some(viewport) = &window.viewport {
                let wanted = options.viewport.unwrap_or_default();
                match wanted.source {
                    Some((x, y, w, h)) => viewport.set_source(x, y, w, h),
                    None => viewport.set_source(-1.0, -1.0, -1.0, -1.0),
                }
                match wanted.destination {
                    Some((w, h)) => viewport.set_destination(w, h),
                    None => viewport.set_destination(-1, -1),
                }
            }
            window.surface.attach(Some(&buffer), 0, 0);
            window
                .surface
                .damage_buffer(0, 0, buffer_width as i32, buffer_height as i32);
            window.surface.commit();
            window.drawn = Some((width, height));
            self.needs_draw = false;
            self.events.push(ClientEvent::Drawn {
                width: buffer_width,
                height: buffer_height,
            });
        }
        self.advance_features(handle)?;
        self.advance_clipboard(handle);
        self.advance_ime(handle);
        Ok(())
    }
}

/// Creates a `wl_shm` buffer filled with one color.
fn create_buffer(
    shm: &wl_shm::WlShm,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
    color: [u8; 4],
    layout: PixelLayout,
) -> Result<wl_buffer::WlBuffer, ClientError> {
    let pixels = features::fill(width, height, Pattern::Solid, color, layout);
    create_shm_buffer(shm, handle, width, height, &pixels, layout)
}

/// Creates a `wl_shm` buffer with the given tightly packed pixels.
fn create_shm_buffer(
    shm: &wl_shm::WlShm,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
    pixels: &[u8],
    layout: PixelLayout,
) -> Result<wl_buffer::WlBuffer, ClientError> {
    let stride = width * 4;
    let file = features::memory_file(pixels)?;
    let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        layout.wl_format(),
        handle,
        (),
    );
    // The buffer keeps the memory alive on the server side.
    pool.destroy();
    Ok(buffer)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::GlobalRemove { name } = event {
            let gone: Vec<ObjectId> = state
                .outputs
                .iter()
                .filter(|(_, entry)| entry.global == name)
                .map(|(id, _)| id.clone())
                .collect();
            for id in gone {
                if let Some(entry) = state.outputs.remove(&id) {
                    state
                        .events
                        .push(ClientEvent::OutputRemoved { name: entry.name });
                }
            }
            return;
        }
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.globals.compositor =
                        Some(registry.bind(name, version.min(6), handle, ()));
                }
                "xdg_activation_v1" => {
                    state.satellite.activation = Some(registry.bind(name, 1, handle, ()));
                }
                "wl_subcompositor" => {
                    state.globals.subcompositor = Some(registry.bind(name, 1, handle, ()));
                }
                "wl_shm" => {
                    state.globals.shm = Some(registry.bind(name, 1, handle, ()));
                }
                "xdg_wm_base" => {
                    state.globals.wm_base = Some(registry.bind(name, version.min(2), handle, ()));
                }
                "wl_seat" => {
                    state.globals.seat = Some(registry.bind(name, version.min(8), handle, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 =
                        registry.bind(name, version.min(4), handle, ());
                    if version >= 4 {
                        dmabuf.get_default_feedback(handle, ());
                    }
                    state.globals.dmabuf = Some(dmabuf);
                }
                "wp_viewporter" => {
                    state.globals.viewporter = Some(registry.bind(name, 1, handle, ()));
                }
                "wp_fractional_scale_manager_v1" => {
                    state.globals.fractional_scale = Some(registry.bind(name, 1, handle, ()));
                }
                "wp_presentation" => {
                    state.globals.presentation = Some(registry.bind(name, 1, handle, ()));
                }
                "wl_data_device_manager" => {
                    state.globals.data_device_manager =
                        Some(registry.bind(name, version.min(3), handle, ()));
                }
                "zwp_relative_pointer_manager_v1" => {
                    state.globals.relative_pointer_manager =
                        Some(registry.bind(name, 1, handle, ()));
                }
                "zwp_pointer_constraints_v1" => {
                    state.globals.pointer_constraints = Some(registry.bind(name, 1, handle, ()));
                }
                "zwp_primary_selection_device_manager_v1" => {
                    state.globals.primary_selection_manager =
                        Some(registry.bind(name, 1, handle, ()));
                }
                "ext_data_control_manager_v1" => {
                    state.globals.ext_data_control_manager =
                        Some(registry.bind(name, 1, handle, ()));
                }
                "wp_color_manager_v1" => {
                    if let Some(wanted) = state.options.color_manager_version {
                        state.color.manager =
                            Some(registry.bind(name, version.min(wanted), handle, ()));
                    }
                }
                "zwlr_data_control_manager_v1" => {
                    // Version 2 has the primary selection.
                    state.globals.wlr_data_control_manager =
                        Some(registry.bind(name, version.min(2), handle, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput =
                        registry.bind(name, version.min(4), handle, ());
                    state.outputs.insert(
                        output.id(),
                        features::OutputEntry {
                            global: name,
                            ..Default::default()
                        },
                    );
                }
                other => {
                    state.bind_ime_global(registry, name, other, handle);
                }
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if let Some(window) = &mut state.window {
                let (width, height, activated) = window.pending;
                let wanted = (
                    if width > 0 {
                        width as u32
                    } else {
                        state.options.width
                    },
                    if height > 0 {
                        height as u32
                    } else {
                        state.options.height
                    },
                );
                // Draw on the first configure and whenever the size changes.
                if !window.configured || window.drawn != Some(wanted) {
                    state.needs_draw = true;
                }
                window.configured = true;
                state.events.push(ClientEvent::Configure {
                    width,
                    height,
                    activated,
                });
                if window.pending_states != window.states {
                    window.states = window.pending_states;
                    let [maximized, fullscreen, resizing] = window.states;
                    state.events.push(ClientEvent::WindowState {
                        maximized,
                        fullscreen,
                        resizing,
                    });
                    // The acknowledgement takes effect with a commit; when
                    // nothing is drawn for this configure, commit as it is.
                    if !state.needs_draw {
                        window.surface.commit();
                    }
                }
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let states: Vec<u32> = states
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                let has = |wanted: xdg_toplevel::State| states.contains(&(wanted as u32));
                if let Some(window) = &mut state.window {
                    window.pending = (width, height, has(xdg_toplevel::State::Activated));
                    window.pending_states = [
                        has(xdg_toplevel::State::Maximized),
                        has(xdg_toplevel::State::Fullscreen),
                        has(xdg_toplevel::State::Resizing),
                    ];
                }
            }
            xdg_toplevel::Event::Close => {
                state.closed = true;
                state.events.push(ClientEvent::CloseRequested);
                // Unmap, as a well behaved client does.
                if let Some(window) = state.window.take() {
                    window.toplevel.destroy();
                    window._xdg_surface.destroy();
                    window.surface.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let entered = matches!(event, wl_surface::Event::Enter { .. });
        match event {
            wl_surface::Event::PreferredBufferScale { factor } => {
                state
                    .events
                    .push(ClientEvent::PreferredBufferScale { scale: factor });
            }
            wl_surface::Event::Enter { output } | wl_surface::Event::Leave { output }
                if state.window.as_ref().is_some_and(|w| &w.surface == surface) =>
            {
                let name = state
                    .outputs
                    .get(&output.id())
                    .map(|entry| entry.name.clone())
                    .unwrap_or_default();
                state.events.push(if entered {
                    ClientEvent::SurfaceEnter { output: name }
                } else {
                    ClientEvent::SurfaceLeave { output: name }
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            // Each buffer is used for exactly one commit.
            buffer.destroy();
            state.events.push(ClientEvent::BufferReleased);
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { callback_data } = event {
            state.events.push(ClientEvent::FrameDone {
                time_ms: callback_data,
            });
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard)
                && state.keyboard_object.is_none()
            {
                state.keyboard_object = Some(seat.get_keyboard(handle, ()));
            }
            if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer_object.is_none()
            {
                state.pointer_object = Some(seat.get_pointer(handle, ()));
            }
            let _ = seat;
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                    // SAFETY: the compositor sends a file of at least `size`
                    // bytes; the mapping is private and read-only.
                    let keymap = unsafe {
                        xkb::Keymap::new_from_fd(
                            &state.keyboard.context,
                            fd,
                            size as usize,
                            xkb::KEYMAP_FORMAT_TEXT_V1,
                            xkb::KEYMAP_COMPILE_NO_FLAGS,
                        )
                    };
                    state.keyboard.state = keymap.ok().flatten().map(|k| xkb::State::new(&k));
                }
                state.events.push(ClientEvent::Keymap { size });
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.last_serial = serial;
                state.keyboard_focus = Some(state.kind_of(&surface));
                state.events.push(ClientEvent::KeyboardEnter)
            }
            wl_keyboard::Event::Leave { .. } => {
                state.keyboard_focus = None;
                state.events.push(ClientEvent::KeyboardLeave)
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: key_state,
                ..
            } => {
                state.last_serial = serial;
                let pressed = key_state == WEnum::Value(wl_keyboard::KeyState::Pressed);
                let (keysym, text) = match &state.keyboard.state {
                    Some(xkb_state) => {
                        // xkb key codes are evdev codes plus 8.
                        let code = xkb::Keycode::new(key + 8);
                        (
                            xkb::keysym_get_name(xkb_state.key_get_one_sym(code)),
                            xkb_state.key_get_utf8(code),
                        )
                    }
                    None => (String::new(), String::new()),
                };
                state.events.push(ClientEvent::Key {
                    keycode: key,
                    pressed,
                    keysym,
                    text,
                });
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb_state) = &mut state.keyboard.state {
                    xkb_state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
                state.events.push(ClientEvent::Modifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                });
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.events.push(ClientEvent::RepeatInfo { rate, delay });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let axis_index = |axis: WEnum<wl_pointer::Axis>| match axis {
            WEnum::Value(wl_pointer::Axis::HorizontalScroll) => 1,
            _ => 0,
        };
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.last_serial = serial;
                state.pointer_focus = Some(state.kind_of(&surface));
                state.events.push(ClientEvent::PointerEnter {
                    x: surface_x,
                    y: surface_y,
                })
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_focus = None;
                state.events.push(ClientEvent::PointerLeave)
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.events.push(ClientEvent::PointerMotion {
                x: surface_x,
                y: surface_y,
            }),
            wl_pointer::Event::Button {
                serial,
                button,
                state: button_state,
                ..
            } => {
                let pressed = button_state == WEnum::Value(wl_pointer::ButtonState::Pressed);
                state.last_serial = serial;
                if pressed {
                    state.last_press_serial = serial;
                }
                state
                    .events
                    .push(ClientEvent::PointerButton { button, pressed })
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                state.events.push(ClientEvent::PointerAxis {
                    axis: axis_index(axis),
                    value,
                })
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                state.events.push(ClientEvent::PointerAxisValue120 {
                    axis: axis_index(axis),
                    value: value120,
                })
            }
            wl_pointer::Event::Frame => state.events.push(ClientEvent::PointerFrame),
            _ => {}
        }
    }
}

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: wl_subcompositor::WlSubcompositor);
delegate_noop!(State: wl_subsurface::WlSubsurface);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_shm::WlShm);
