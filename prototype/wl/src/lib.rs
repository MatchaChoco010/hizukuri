//! Wayland server side of the verification compositor.
//!
//! This crate wraps Smithay's protocol layer and turns protocol requests
//! into plain state and events. No Smithay type appears in its public API:
//! surfaces, windows and outputs are identified by ids, pixel data is handed
//! out as bytes, and input is fed in as numbers. That keeps the rest of
//! hizukuri (layout, scene, renderer) independent of Smithay.
//!
//! The crate does not render and does not decide where windows go. The
//! caller
//!
//! 1. watches [`WlServer::listen_fd`] and [`WlServer::display_fd`] and calls
//!    [`WlServer::accept_clients`] / [`WlServer::dispatch_clients`],
//! 2. drains [`WlServer::drain_events`] and reacts (place windows, set the
//!    focus, configure),
//! 3. asks for the surfaces of each window ([`WlServer::window_surfaces`])
//!    and their pixels ([`WlServer::take_buffer_update`]) to draw them,
//!    dropping each [`BufferUpdate`] once the renderer is done with it,
//! 4. reports frames back ([`WlServer::send_frame_callbacks`],
//!    [`WlServer::send_presented`]) and calls [`WlServer::flush_clients`].
//!
//! Pointer input is addressed to a surface with surface-local coordinates.
//! Hit testing is the caller's job, so it works the same whether windows are
//! laid out flat or placed in 3D.

mod buffer;
mod handlers;
mod state;
mod surface;

use std::borrow::Cow;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use proto_color::{AlphaMode, ImageDescription};
use smithay::reexports::wayland_server::{Display, ListeningSocket};

use crate::state::{ClientState, State};

/// What a surface without any color information is taken to be: BT.709
/// primaries with a pure 2.2 power function.
pub const DEFAULT_SURFACE_IMAGE_DESCRIPTION: ImageDescription = ImageDescription::SRGB_GAMMA22;

/// Identifies a `wl_surface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SurfaceId(pub u64);

/// Identifies a toplevel window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// Identifies an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u64);

/// Errors of the Wayland server.
#[derive(Debug, thiserror::Error)]
pub enum WlError {
    #[error("failed to create the Wayland display: {0}")]
    Display(String),
    #[error("failed to bind the Wayland socket {name:?}: {message}")]
    Bind { name: String, message: String },
    #[error("failed to set up the keyboard: {0}")]
    Keyboard(String),
    #[error("failed to offer zwp_linux_dmabuf_v1: {0}")]
    Dmabuf(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Options for [`WlServer::new`].
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    /// How clients reach the server.
    pub socket: SocketConfig,
    /// Which dmabufs clients may send. `None`: `zwp_linux_dmabuf_v1` is not
    /// offered, clients use `wl_shm`.
    pub dmabuf: Option<DmabufConfig>,
}

/// A DRM format code with a modifier, as in `drm_fourcc.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifier: u64,
}

/// `DRM_FORMAT_ARGB8888` and its siblings: the formats that have a
/// `wl_shm` equivalent.
pub const FOURCC_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const FOURCC_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const FOURCC_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
pub const FOURCC_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
/// `DRM_FORMAT_MOD_LINEAR`.
pub const MODIFIER_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: the buffer was allocated without modifiers.
pub const MODIFIER_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// What `zwp_linux_dmabuf_v1` offers. Whoever imports the buffers (the
/// renderer) decides this; the protocol layer only passes it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmabufConfig {
    /// The formats and modifiers that can be imported.
    pub formats: Vec<DmabufFormat>,
    /// Device number (`major`, `minor`) of the DRM device clients should
    /// allocate on: the device that imports. With it the global has
    /// feedback (version 5); without it clients only get the format list
    /// (version 3) and pick a device themselves.
    pub main_device: Option<(u32, u32)>,
}

impl DmabufConfig {
    /// The formats [`BufferUpdate::upright_pixels`] can read by mapping the
    /// buffer: the four 8 bit RGB layouts, in linear memory.
    pub fn cpu_readable(main_device: Option<(u32, u32)>) -> Self {
        Self {
            formats: [
                FOURCC_ARGB8888,
                FOURCC_XRGB8888,
                FOURCC_ABGR8888,
                FOURCC_XBGR8888,
            ]
            .into_iter()
            .map(|fourcc| DmabufFormat {
                fourcc,
                modifier: MODIFIER_LINEAR,
            })
            .collect(),
            main_device,
        }
    }
}

/// Where the listening socket is created.
#[derive(Debug, Clone, Default)]
pub enum SocketConfig {
    /// No listening socket. Clients are added with
    /// [`WlServer::insert_client`] (used by in-process tests).
    #[default]
    None,
    /// A socket with this name in `XDG_RUNTIME_DIR`.
    Name(String),
    /// A socket at this absolute path.
    Path(PathBuf),
}

/// Something that happened on the protocol side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WlEvent {
    /// A client created a toplevel. It has no contents yet.
    WindowCreated(WindowId),
    /// The window, or a surface in its tree, committed new state: contents,
    /// size, title and so on. Query the window again.
    WindowChanged(WindowId),
    /// The toplevel is gone.
    WindowDestroyed(WindowId),
    /// A surface is gone. Resources kept for it (textures) can be dropped.
    SurfaceDestroyed(SurfaceId),
    /// The cursor image requested by the client under the pointer changed.
    CursorChanged,
    /// A client asked for something that is the window manager's to decide:
    /// to be maximized, made fullscreen, minimized, moved or resized by the
    /// user, or to have the window menu shown. Nothing has been done about
    /// it. Whoever manages windows answers with
    /// [`WlServer::set_window_states`] and [`WlServer::configure_window`],
    /// or not at all.
    WindowRequest {
        window: WindowId,
        request: WindowRequest,
    },
    /// A popup was created, or its client asked for it to be placed anew.
    /// It has been given a place by the rules of its positioner, inside
    /// the bounds of its window when those were set
    /// ([`WlServer::set_popup_bounds`]). Until the next
    /// [`WlServer::flush_clients`] the place can still be changed
    /// ([`WlServer::place_popup_within`], [`WlServer::set_popup_geometry`]):
    /// that is when the client is told.
    PopupPlacement { window: WindowId, popup: SurfaceId },
}

/// What a client asked for with a request of `xdg_toplevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRequest {
    Maximize,
    Unmaximize,
    /// `output` is the output the client would like, if it named one.
    Fullscreen {
        output: Option<OutputId>,
    },
    Unfullscreen,
    Minimize,
    /// The user started to drag the window (a press on a title bar the
    /// client draws). Only reported while the button of that press is
    /// still down.
    Move,
    /// The user started to drag an edge or corner of the window. Only
    /// reported while the button of that press is still down.
    Resize {
        edges: ResizeEdges,
    },
    /// The client asks for the window menu (the one a title bar shows on a
    /// right click) at this position relative to the window geometry.
    ShowMenu {
        position: (i32, i32),
    },
}

/// The edges of a window that an interactive resize moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResizeEdges {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

/// The states of a window other than its size and whether it is activated:
/// what the window manager tells a client it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowStates {
    pub maximized: bool,
    pub fullscreen: bool,
    /// The user is resizing the window; the client may draw cheaper frames.
    pub resizing: bool,
}

/// What a popup asked for, and where it was put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupRequest {
    pub window: WindowId,
    /// The surface the popup belongs to: the window's root surface or
    /// another popup.
    pub parent: SurfaceId,
    /// Where the parent's window geometry starts, relative to the window
    /// geometry of the toplevel: zero for a popup of the window itself.
    /// The rectangles below are relative to the parent's geometry.
    pub parent_origin: (i32, i32),
    /// The rectangle of the parent the popup is attached to (x, y, width,
    /// height): a menu button, the pointer position.
    pub anchor_rect: (i32, i32, i32, i32),
    /// Where the positioner puts the popup when nothing is in the way.
    pub wanted: (i32, i32, i32, i32),
    /// Where the popup is put as things stand.
    pub geometry: (i32, i32, i32, i32),
    /// What the positioner allows when `wanted` does not fit.
    pub may_flip: (bool, bool),
    pub may_slide: (bool, bool),
    pub may_resize: (bool, bool),
}

/// Memory layout of pixels in a `wl_shm` buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShmFormat {
    Argb8888,
    Xrgb8888,
    Abgr8888,
    Xbgr8888,
}

/// Pixel data a surface committed, copied out of the client's buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferContents {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    pub format: ShmFormat,
    pub data: Vec<u8>,
}

/// One plane of a dmabuf.
#[derive(Debug)]
pub struct DmabufPlane {
    /// A duplicate of the client's file descriptor; closing it does not
    /// affect the client's buffer.
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A dmabuf a surface committed: everything an importer needs.
#[derive(Debug)]
pub struct DmabufContents {
    pub width: u32,
    pub height: u32,
    /// DRM format code.
    pub fourcc: u32,
    /// DRM format modifier ([`MODIFIER_INVALID`] when the client gave none).
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// The first row in memory is the bottom row of the image.
    pub y_inverted: bool,
}

/// How a client created a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BufferKind {
    /// `wl_shm`: memory shared with the client.
    Shm,
    /// `zwp_linux_dmabuf_v1`: usually GPU memory.
    Dmabuf,
}

impl BufferKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Shm => "shm",
            Self::Dmabuf => "dmabuf",
        }
    }
}

/// Where the contents of a committed buffer are.
#[derive(Debug)]
pub enum BufferSource {
    /// Pixels copied out of a `wl_shm` pool.
    Shm(BufferContents),
    /// A buffer that lives in GPU (or other device) memory. It has to be
    /// imported, and the [`BufferUpdate`] kept for as long as the import is
    /// used: the client may draw into the buffer again once it is released.
    Dmabuf(DmabufContents),
}

/// How the client turned the contents before putting them into the buffer
/// (`wl_surface.set_buffer_transform`). The compositor undoes it.
///
/// The names follow `wl_output.transform`: rotations are counter-clockwise,
/// and the flipped variants mirror around the vertical axis first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BufferTransform {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl BufferTransform {
    /// Whether width and height of the buffer are those of the upright
    /// image swapped.
    pub fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }

    /// The size of the upright image for a buffer of the given size.
    pub fn upright_size(self, buffer: (u32, u32)) -> (u32, u32) {
        if self.swaps_axes() {
            (buffer.1, buffer.0)
        } else {
            buffer
        }
    }

    /// Where a pixel of the upright image is in the buffer. `upright` is
    /// the size of the upright image.
    pub fn buffer_position(self, upright: (u32, u32), x: u32, y: u32) -> (u32, u32) {
        let (w, h) = (upright.0 - 1, upright.1 - 1);
        match self {
            Self::Normal => (x, y),
            Self::Flipped => (w - x, y),
            Self::Rotate90 => (y, w - x),
            Self::Flipped90 => (y, x),
            Self::Rotate180 => (w - x, h - y),
            Self::Flipped180 => (x, h - y),
            Self::Rotate270 => (h - y, x),
            Self::Flipped270 => (h - y, w - x),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Rotate90 => "90",
            Self::Rotate180 => "180",
            Self::Rotate270 => "270",
            Self::Flipped => "flipped",
            Self::Flipped90 => "flipped-90",
            Self::Flipped180 => "flipped-180",
            Self::Flipped270 => "flipped-270",
        }
    }
}

/// Why the pixels of a buffer could not be read on the CPU.
#[derive(Debug, thiserror::Error)]
pub enum PixelError {
    #[error("format {fourcc:#x} with modifier {modifier:#x} cannot be read on the CPU")]
    Unsupported { fourcc: u32, modifier: u64 },
    #[error("the buffer cannot be mapped: {0}")]
    Map(String),
    #[error("the buffer is smaller than its description says")]
    TooSmall,
}

/// What a surface committed, together with the reference to the client
/// buffer it came from.
///
/// The client is told that it may reuse the buffer when this value is
/// dropped. Keep it until the renderer no longer needs the buffer: for
/// pixels that were copied ([`BufferSource::Shm`], or anything read through
/// [`Self::upright_pixels`]) that is until they are uploaded; for an
/// imported dmabuf it is for as long as the import is drawn from.
pub struct BufferUpdate {
    pub source: BufferSource,
    /// The transform the buffer was committed with. [`SurfaceNode`] sizes
    /// and source rectangles describe the upright image.
    pub transform: BufferTransform,
    /// What the client marked as changed since the previous update of this
    /// surface (`wl_surface.damage` and `damage_buffer`), as the client sent
    /// it. Empty when the client marked nothing.
    pub damage: Vec<DamageRect>,
    /// Smithay's handle of the dmabuf, for mapping it.
    dmabuf: Option<smithay::backend::allocator::dmabuf::Dmabuf>,
    _buffer: surface::HeldBuffer,
}

impl BufferUpdate {
    /// The contents as upright pixels in CPU memory.
    ///
    /// `wl_shm` contents are already there; a transform is undone by
    /// rearranging them. A dmabuf is mapped and copied, which only works
    /// for the formats of [`DmabufConfig::cpu_readable`] and for buffers
    /// the driver lets a process map. This is the slow path for renderers
    /// that cannot import dmabufs; a renderer that can takes
    /// [`Self::source`] instead.
    pub fn upright_pixels(&self) -> Result<Cow<'_, BufferContents>, PixelError> {
        let contents = match &self.source {
            BufferSource::Shm(contents) => Cow::Borrowed(contents),
            BufferSource::Dmabuf(description) => {
                let dmabuf = self.dmabuf.as_ref().expect("set for dmabuf sources");
                Cow::Owned(buffer::read_dmabuf(dmabuf, description)?)
            }
        };
        Ok(match self.transform {
            BufferTransform::Normal => contents,
            transform => Cow::Owned(buffer::upright(&contents, transform)),
        })
    }
}

/// A rectangle a client marked as changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageRect {
    /// In surface-local logical coordinates (`wl_surface.damage`).
    Surface {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// In buffer pixels (`wl_surface.damage_buffer`), before the buffer
    /// transform is undone.
    Buffer {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
}

/// What produced a scroll event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisSource {
    Wheel,
    Finger,
    Continuous,
}

/// The role of a surface inside a window's surface tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    Toplevel,
    Subsurface,
    Popup,
    /// The surface a client set as the pointer image.
    Cursor,
    /// The surface a client attached to a drag.
    DragIcon,
}

/// One surface of a window, as the scene needs it to draw and hit test.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceNode {
    pub surface: SurfaceId,
    pub role: SurfaceRole,
    /// Position of the surface's top left corner relative to the top left
    /// corner of the window geometry, in logical coordinates.
    pub offset: (i32, i32),
    /// Size of the surface in logical coordinates. Zero when it has no
    /// contents.
    pub size: (i32, i32),
    /// Size of the attached buffer in pixels, if any, as the client
    /// allocated it (before the transform is undone).
    pub buffer_size: Option<(u32, u32)>,
    /// How the attached buffer was created, if there is one.
    pub buffer_kind: Option<BufferKind>,
    /// How the client turned the contents inside the buffer.
    pub transform: BufferTransform,
    /// The part of the upright image that is shown, in buffer pixels
    /// (x, y, width, height).
    pub source: Option<(f64, f64, f64, f64)>,
    pub buffer_scale: i32,
    /// What the buffer values mean.
    pub image_description: ImageDescription,
    pub alpha_mode: AlphaMode,
    /// Counts content changes; a new value means new pixels are available
    /// from [`WlServer::take_buffer_update`].
    pub generation: u64,
}

/// A toplevel window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: WindowId,
    /// The root surface of the window.
    pub surface: SurfaceId,
    pub title: Option<String>,
    pub app_id: Option<String>,
    /// Whether the window has contents to show.
    pub mapped: bool,
    /// Size of the window geometry in logical coordinates, once mapped.
    pub size: Option<(i32, i32)>,
    /// Whether the last configure told the window it is activated.
    pub activated: bool,
    /// Whether the client was told it is maximized, and acknowledged it.
    pub maximized: bool,
    /// Whether the client was told it is fullscreen, and acknowledged it.
    pub fullscreen: bool,
}

/// Description of an output announced to clients.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    pub name: String,
    pub make: String,
    pub model: String,
    /// Size of the mode in physical pixels.
    pub mode_size: (i32, i32),
    /// Refresh rate in mHz.
    pub refresh_mhz: i32,
    pub scale: f64,
    /// Position of the top left corner in the global logical space.
    pub position: (i32, i32),
}

/// The cursor a client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorState {
    Hidden,
    /// A named cursor shape (e.g. `default`, `text`).
    Named(String),
    /// A client surface.
    Surface(SurfaceId),
}

/// The Wayland server.
pub struct WlServer {
    display: Display<State>,
    /// Sources the server waits on besides client requests: the dmabufs
    /// that commits are held back for.
    event_loop: calloop::EventLoop<'static, State>,
    state: State,
    socket: Option<ListeningSocket>,
    socket_name: Option<String>,
}

impl WlServer {
    pub fn new(config: &ServerConfig) -> Result<Self, WlError> {
        let mut display: Display<State> =
            Display::new().map_err(|e| WlError::Display(e.to_string()))?;
        let event_loop: calloop::EventLoop<'static, State> =
            calloop::EventLoop::try_new().map_err(|e| WlError::Display(e.to_string()))?;
        // The loop's own descriptor becomes readable when clients have
        // sent something, too, so that one descriptor covers everything.
        let display_fd = display
            .backend()
            .poll_fd()
            .try_clone_to_owned()
            .map_err(|e| WlError::Display(e.to_string()))?;
        event_loop
            .handle()
            .insert_source(
                calloop::generic::Generic::new(
                    display_fd,
                    calloop::Interest::READ,
                    calloop::Mode::Level,
                ),
                // Reading the requests is done by `dispatch_clients`.
                |_, _, _| Ok(calloop::PostAction::Continue),
            )
            .map_err(|e| WlError::Display(e.to_string()))?;
        let state = State::new(&display.handle(), event_loop.handle(), config)?;
        let (socket, socket_name) = match &config.socket {
            SocketConfig::None => (None, None),
            SocketConfig::Name(name) => {
                let socket = ListeningSocket::bind(name).map_err(|e| WlError::Bind {
                    name: name.clone(),
                    message: e.to_string(),
                })?;
                (Some(socket), Some(name.clone()))
            }
            SocketConfig::Path(path) => {
                let name = path.display().to_string();
                let socket =
                    ListeningSocket::bind_absolute(path.clone()).map_err(|e| WlError::Bind {
                        name: name.clone(),
                        message: e.to_string(),
                    })?;
                (Some(socket), Some(name))
            }
        };
        Ok(Self {
            display,
            event_loop,
            state,
            socket,
            socket_name,
        })
    }

    /// The socket name (or path) clients connect to, for `WAYLAND_DISPLAY`.
    pub fn socket_name(&self) -> Option<&str> {
        self.socket_name.as_deref()
    }

    // -- event loop integration ---------------------------------------------

    /// Readable when a client is waiting to connect. `None` without a
    /// listening socket.
    pub fn listen_fd(&self) -> Option<BorrowedFd<'_>> {
        self.socket.as_ref().map(|s| s.as_fd())
    }

    /// Readable when [`Self::dispatch_clients`] has something to do:
    /// clients have sent requests, or a buffer that a commit was waiting
    /// for has become ready.
    pub fn display_fd(&mut self) -> BorrowedFd<'_> {
        self.event_loop.as_fd()
    }

    /// Accepts all clients waiting on the listening socket. Returns how many
    /// were accepted.
    pub fn accept_clients(&mut self) -> io::Result<usize> {
        let mut streams = Vec::new();
        if let Some(socket) = &self.socket {
            while let Some(stream) = socket.accept()? {
                streams.push(stream);
            }
        }
        let count = streams.len();
        for stream in streams {
            self.insert_client(stream)?;
        }
        Ok(count)
    }

    /// Adds a client connected over `stream`.
    pub fn insert_client(&mut self, stream: UnixStream) -> io::Result<()> {
        self.display
            .handle()
            .insert_client(stream, Arc::new(ClientState::default()))?;
        Ok(())
    }

    /// Reads and handles pending client requests, and applies the commits
    /// whose buffers have become ready in the meantime.
    ///
    /// A commit that attaches a dmabuf the client's GPU is still drawing
    /// into is not applied when it arrives: it waits until the buffer can
    /// be read, and commits of the same client that depend on it wait with
    /// it. The compositor never sees a half drawn buffer and never has to
    /// stall for one.
    pub fn dispatch_clients(&mut self) -> io::Result<()> {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .map_err(io::Error::other)?;
        self.display.dispatch_clients(&mut self.state)?;
        Ok(())
    }

    /// Sends queued events to the clients. Before that, popups that were
    /// created since the last call are told their place, and requests that
    /// the protocol wants answered are answered if nobody did.
    pub fn flush_clients(&mut self) -> io::Result<()> {
        self.state.send_owed_configures();
        self.display.flush_clients()
    }

    /// Takes the events collected since the last call.
    pub fn drain_events(&mut self) -> Vec<WlEvent> {
        std::mem::take(&mut self.state.events)
    }

    // -- outputs --------------------------------------------------------------

    /// Announces an output to clients.
    pub fn add_output(&mut self, config: &OutputConfig) -> OutputId {
        self.state.add_output(config)
    }

    /// Changes the mode, scale or position of an output. Returns `false`
    /// when the output is unknown. The name, make and model cannot change.
    pub fn update_output(&mut self, id: OutputId, config: &OutputConfig) -> bool {
        self.state.update_output(id, config)
    }

    /// Withdraws an output from clients. Returns `false` when the output is
    /// unknown.
    pub fn remove_output(&mut self, id: OutputId) -> bool {
        self.state.remove_output(id)
    }

    // -- windows --------------------------------------------------------------

    /// All toplevel windows, in creation order.
    pub fn windows(&self) -> Vec<WindowId> {
        self.state.windows.iter().map(|w| w.id).collect()
    }

    pub fn window(&self, id: WindowId) -> Option<WindowInfo> {
        self.state.window_info(id)
    }

    /// The surfaces of a window (root, subsurfaces, popups), back to front.
    /// Empty when the window is unknown.
    pub fn window_surfaces(&self, id: WindowId) -> Vec<SurfaceNode> {
        self.state.window_surfaces(id)
    }

    /// Tells a window its size and whether it is activated. `size` of `None`
    /// lets the client choose. Sends a configure event when something
    /// changed and the window has been configured before; otherwise the
    /// state is used for the initial configure.
    pub fn configure_window(&mut self, id: WindowId, size: Option<(i32, i32)>, activated: bool) {
        self.state.configure_window(id, size, activated);
    }

    /// Says which outputs a window is shown on. Its surfaces enter those
    /// outputs and leave the others, and prefer the largest of their
    /// scales. The first output is the window's main one: its frames drive
    /// the window's frame callbacks and presentation feedback.
    ///
    /// Until this is called for a window, its surfaces are on no output
    /// (they are still told a scale to draw for: that of the first one).
    pub fn set_window_outputs(&mut self, window: WindowId, outputs: &[OutputId]) {
        self.state.set_window_outputs(window, outputs);
    }

    /// Says where the popups of a window should stay: a rectangle (x, y,
    /// width, height) relative to the window geometry, usually the output
    /// the window is on. Popups created afterwards are flipped, slid or
    /// resized into it, as far as their positioner allows. `None`: popups
    /// go exactly where their positioner says.
    pub fn set_popup_bounds(&mut self, window: WindowId, bounds: Option<(i32, i32, i32, i32)>) {
        self.state.set_popup_bounds(window, bounds);
    }

    /// Tells a window whether it is maximized, fullscreen or being
    /// resized. Sends a configure when something changed. The size that
    /// goes with it is the one of [`Self::configure_window`]; set that
    /// first when both change.
    pub fn set_window_states(&mut self, id: WindowId, states: WindowStates) {
        self.state.set_window_states(id, states);
    }

    /// What a popup asked for and where it was put. `None` when the surface
    /// is not a popup.
    pub fn popup_request(&self, popup: SurfaceId) -> Option<PopupRequest> {
        self.state.popup_request(popup)
    }

    /// Places a popup by the rules of its positioner inside `bounds`: a
    /// rectangle (x, y, width, height) relative to the geometry of the
    /// popup's parent. Returns the place it got.
    ///
    /// This is for whoever draws windows somewhere else than where they
    /// are laid out flat (scaled down, moved, rotated): only it knows which
    /// part of the parent's coordinate space is on screen. Call it after
    /// [`WlEvent::PopupPlacement`], before the next
    /// [`Self::flush_clients`]; later the client has been told already.
    pub fn place_popup_within(
        &mut self,
        popup: SurfaceId,
        bounds: (i32, i32, i32, i32),
    ) -> Option<(i32, i32, i32, i32)> {
        self.state.place_popup_within(popup, bounds)
    }

    /// Puts a popup at a rectangle (x, y, width, height) relative to the
    /// geometry of its parent, whatever its positioner says. The same
    /// timing as [`Self::place_popup_within`] applies. Returns `false`
    /// when the surface is not a popup.
    pub fn set_popup_geometry(&mut self, popup: SurfaceId, geometry: (i32, i32, i32, i32)) -> bool {
        self.state.set_popup_geometry(popup, geometry)
    }

    /// Asks a window to close.
    pub fn close_window(&mut self, id: WindowId) {
        self.state.close_window(id);
    }

    // -- surfaces ---------------------------------------------------------------

    /// Takes what a surface committed since the last call, if anything.
    ///
    /// `wl_shm` pixels are read from the client's buffer now, not at commit
    /// time. The buffer stays held until the returned value is dropped.
    pub fn take_buffer_update(&mut self, surface: SurfaceId) -> Option<BufferUpdate> {
        self.state.take_buffer_update(surface)
    }

    /// Whether a point in surface-local logical coordinates is inside the
    /// surface and its input region.
    pub fn surface_accepts_input(&self, surface: SurfaceId, x: f64, y: f64) -> bool {
        self.state.surface_accepts_input(surface, x, y)
    }

    /// Overrides what the values of a surface mean. A stand-in for
    /// color-management-v1 and color-representation-v1 until those protocols
    /// are implemented; meant for tests.
    pub fn set_surface_color(
        &mut self,
        surface: SurfaceId,
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> bool {
        self.state
            .set_surface_color(surface, image_description, alpha_mode)
    }

    /// Calls the frame callbacks of the surfaces whose main output is
    /// `output` (see [`Self::set_window_outputs`]), or of every surface
    /// with `None`. `time_ms` is the frame time in milliseconds on an
    /// arbitrary monotonic base.
    pub fn send_frame_callbacks(&mut self, output: Option<OutputId>, time_ms: u32) {
        self.state.send_frame_callbacks(output, time_ms);
    }

    /// Reports to the `wp_presentation` feedback objects of the surfaces
    /// whose main output is `output` that their contents were shown at
    /// `time` (CLOCK_MONOTONIC).
    pub fn send_presented(
        &mut self,
        output: OutputId,
        time: Duration,
        refresh: Duration,
        seq: u64,
    ) {
        self.state.send_presented(output, time, refresh, seq);
    }

    // -- input ------------------------------------------------------------------

    /// Gives the keyboard focus to the root surface of a window, or to
    /// nothing. While a popup of some window holds a grab (an open menu),
    /// the popup keeps the keyboard; the focus asked for here takes effect
    /// when the grab ends.
    pub fn set_keyboard_focus(&mut self, window: Option<WindowId>) {
        self.state.set_keyboard_focus(window);
    }

    /// Feeds a key. `keycode` is a Linux evdev key code (`KEY_A` is 30).
    /// Input times are in microseconds on an arbitrary monotonic base.
    pub fn keyboard_key(&mut self, keycode: u32, pressed: bool, time_us: u64) {
        self.state.keyboard_key(keycode, pressed, time_us);
    }

    /// Moves the pointer onto a surface at surface-local logical
    /// coordinates, or off every surface with `None`.
    pub fn pointer_motion(&mut self, target: Option<(SurfaceId, (f64, f64))>, time_us: u64) {
        self.state.pointer_motion(target, time_us);
    }

    /// Feeds a pointer button. `button` is a Linux evdev code (`BTN_LEFT`
    /// is 0x110).
    pub fn pointer_button(&mut self, button: u32, pressed: bool, time_us: u64) {
        self.state.pointer_button(button, pressed, time_us);
    }

    /// Feeds a scroll. Values follow `wl_pointer.axis` (positive is down or
    /// right); `v120` is the high resolution wheel value, when the source is
    /// a wheel.
    pub fn pointer_axis(
        &mut self,
        horizontal: f64,
        vertical: f64,
        v120: Option<(i32, i32)>,
        source: AxisSource,
        time_us: u64,
    ) {
        self.state
            .pointer_axis(horizontal, vertical, v120, source, time_us);
    }

    /// Ends a group of pointer events (`wl_pointer.frame`).
    pub fn pointer_frame(&mut self) {
        self.state.pointer_frame();
    }

    /// The surface that has the keyboard focus.
    pub fn keyboard_focus(&self) -> Option<SurfaceId> {
        self.state.keyboard_focus()
    }

    /// The surface the pointer is on.
    pub fn pointer_focus(&self) -> Option<SurfaceId> {
        self.state.pointer_focus()
    }

    /// The surfaces of the icon a client attached to the drag in progress,
    /// back to front, with offsets relative to the pointer position. Empty
    /// when nothing is dragged or the drag has no icon.
    pub fn drag_icon_surfaces(&self) -> Vec<SurfaceNode> {
        self.state.drag_icon_surfaces()
    }

    /// Whether a drag and drop holds the pointer. While it does, what the
    /// compositor passes to [`Self::pointer_motion`] is the surface the data
    /// is offered to, so it must be what is under the pointer, not the
    /// surface the button was pressed on.
    pub fn drag_in_progress(&self) -> bool {
        self.state.dragging
    }

    /// The MIME types of the selection (the clipboard), when a client owns
    /// it. For inspection: reading it this way does not involve the focus.
    pub fn selection_mime_types(&self) -> Option<Vec<String>> {
        self.state.selection.clone()
    }

    /// The cursor requested by the client under the pointer.
    pub fn cursor(&self) -> CursorState {
        self.state.cursor.clone()
    }

    /// The surfaces of the cursor image, when the client set a surface as
    /// the cursor ([`CursorState::Surface`]), back to front. Offsets are
    /// relative to the pointer position: the hotspot is already applied.
    pub fn cursor_surfaces(&self) -> Vec<SurfaceNode> {
        self.state.cursor_surfaces()
    }
}
