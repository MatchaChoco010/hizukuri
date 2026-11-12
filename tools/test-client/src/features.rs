//! The parts of the test client beyond one solid window: buffers with
//! patterns and dmabufs, popups, outputs, the data device (selection and
//! drag and drop), and the feedback protocols.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

use wayland_client::protocol::{
    wl_buffer, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_output,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface};

use crate::{ClientError, ClientEvent, Pattern, PixelLayout, State, TestClient, create_buffer};

// ---------------------------------------------------------------------------
// Buffers
// ---------------------------------------------------------------------------

/// Tightly packed pixels of a buffer.
pub(crate) fn fill(
    width: u32,
    height: u32,
    pattern: Pattern,
    color: [u8; 4],
    layout: PixelLayout,
) -> Vec<u8> {
    match pattern {
        Pattern::Solid => layout.bytes(color).repeat((width * height) as usize),
        Pattern::Quadrants(colors) => {
            let mut pixels = Vec::with_capacity((width * height * 4) as usize);
            for y in 0..height {
                for x in 0..width {
                    let index = (y * 2 / height.max(1)).min(1) * 2 + (x * 2 / width.max(1)).min(1);
                    pixels.extend_from_slice(&layout.bytes(colors[index as usize]));
                }
            }
            pixels
        }
    }
}

/// An anonymous file in memory with the given contents.
pub(crate) fn memory_file(contents: &[u8]) -> Result<File, ClientError> {
    let fd: OwnedFd =
        rustix::fs::memfd_create("proto-test-client", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| ClientError::Io(e.into()))?;
    let mut file = File::from(fd);
    file.write_all(contents)?;
    file.flush()?;
    Ok(file)
}

/// Creates a buffer through `zwp_linux_dmabuf_v1`: one plane of linear
/// memory, without a modifier.
pub(crate) fn create_dmabuf_buffer(
    dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
    pixels: &[u8],
    layout: PixelLayout,
) -> Result<wl_buffer::WlBuffer, ClientError> {
    let file = memory_file(pixels)?;
    let params = dmabuf.create_params(handle, ());
    // Modifier 0 is DRM_FORMAT_MOD_LINEAR.
    params.add(file.as_fd(), 0, 0, width * 4, 0, 0);
    let fourcc = u32::from_le_bytes(match layout {
        PixelLayout::Argb8888 => *b"AR24",
        PixelLayout::Xrgb8888 => *b"XR24",
        PixelLayout::Abgr8888 => *b"AB24",
        PixelLayout::Xbgr8888 => *b"XB24",
    });
    let buffer = params.create_immed(
        width as i32,
        height as i32,
        fourcc,
        zwp_linux_buffer_params_v1::Flags::empty(),
        handle,
        (),
    );
    params.destroy();
    Ok(buffer)
}

/// The default dmabuf feedback while it arrives.
#[derive(Default)]
pub(crate) struct FeedbackState {
    main_device: [u32; 2],
    table: Vec<[u64; 2]>,
    formats: Vec<[u64; 2]>,
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            state.events.push(ClientEvent::DmabufModifier {
                fourcc: format,
                modifier: (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
            });
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_feedback_v1::Event;
        let feedback = &mut state.dmabuf_feedback;
        match event {
            Event::MainDevice { device } => {
                if let Ok(bytes) = <[u8; 8]>::try_from(device.as_slice()) {
                    let device = u64::from_ne_bytes(bytes);
                    feedback.main_device = [rustix::fs::major(device), rustix::fs::minor(device)];
                }
            }
            Event::FormatTable { fd, size } => {
                // Entries of 16 bytes: format, padding, modifier.
                let file = File::from(fd);
                let mut bytes = vec![0u8; size as usize];
                if file.read_exact_at(&mut bytes, 0).is_ok() {
                    feedback.table = bytes
                        .chunks_exact(16)
                        .map(|entry| {
                            [
                                u64::from(u32::from_ne_bytes(entry[0..4].try_into().unwrap())),
                                u64::from_ne_bytes(entry[8..16].try_into().unwrap()),
                            ]
                        })
                        .collect();
                }
            }
            Event::TrancheFormats { indices } => {
                for index in indices.chunks_exact(2) {
                    let index = u16::from_ne_bytes([index[0], index[1]]) as usize;
                    if let Some(entry) = feedback.table.get(index) {
                        feedback.formats.push(*entry);
                    }
                }
            }
            Event::Done => {
                let formats = std::mem::take(&mut feedback.formats);
                state.events.push(ClientEvent::DmabufFeedback {
                    main_device: feedback.main_device,
                    formats,
                });
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Which surface
// ---------------------------------------------------------------------------

/// One of this client's surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Window,
    Subsurface,
    Popup,
    Other,
}

impl State {
    pub(crate) fn kind_of(&self, surface: &wl_surface::WlSurface) -> SurfaceKind {
        if self.window.as_ref().is_some_and(|w| &w.surface == surface) {
            SurfaceKind::Window
        } else if self.popup.as_ref().is_some_and(|p| &p.surface == surface) {
            SurfaceKind::Popup
        } else if self.subsurfaces.iter().any(|(s, _)| s == surface) {
            SurfaceKind::Subsurface
        } else {
            SurfaceKind::Other
        }
    }
}

// ---------------------------------------------------------------------------
// Requests to the window manager
// ---------------------------------------------------------------------------

/// Something a window asks of the compositor: the requests of
/// `xdg_toplevel` that a window manager decides about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToplevelRequest {
    Maximize,
    Unmaximize,
    Fullscreen,
    Unfullscreen,
    Minimize,
    /// Start an interactive move. The serial of the latest pointer button
    /// press is used.
    Move,
    /// Start an interactive resize by these edges (top, bottom, left,
    /// right), with the serial of the latest press.
    Resize {
        top: bool,
        bottom: bool,
        left: bool,
        right: bool,
    },
    /// Ask for the window menu at a position relative to the window
    /// geometry.
    WindowMenu {
        x: i32,
        y: i32,
    },
}

impl TestClient {
    /// Sends a request of the window to the compositor. It goes out with
    /// the next pump.
    pub fn request(&mut self, request: ToplevelRequest) -> Result<(), ClientError> {
        use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;
        let state = &self.state;
        let (Some(window), Some(seat)) = (&state.window, &state.globals.seat) else {
            return Err(ClientError::Protocol(
                "the window does not exist yet".to_string(),
            ));
        };
        let toplevel = &window.toplevel;
        match request {
            ToplevelRequest::Maximize => toplevel.set_maximized(),
            ToplevelRequest::Unmaximize => toplevel.unset_maximized(),
            ToplevelRequest::Fullscreen => toplevel.set_fullscreen(None),
            ToplevelRequest::Unfullscreen => toplevel.unset_fullscreen(),
            ToplevelRequest::Minimize => toplevel.set_minimized(),
            ToplevelRequest::Move => toplevel._move(seat, state.last_press_serial),
            ToplevelRequest::Resize {
                top,
                bottom,
                left,
                right,
            } => {
                let edges = match (top, bottom, left, right) {
                    (true, _, true, _) => ResizeEdge::TopLeft,
                    (true, _, _, true) => ResizeEdge::TopRight,
                    (_, true, true, _) => ResizeEdge::BottomLeft,
                    (_, true, _, true) => ResizeEdge::BottomRight,
                    (true, ..) => ResizeEdge::Top,
                    (_, true, ..) => ResizeEdge::Bottom,
                    (_, _, true, _) => ResizeEdge::Left,
                    (_, _, _, true) => ResizeEdge::Right,
                    _ => ResizeEdge::None,
                };
                toplevel.resize(seat, state.last_press_serial, edges);
            }
            ToplevelRequest::WindowMenu { x, y } => {
                toplevel.show_window_menu(seat, state.last_serial, x, y)
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Popups
// ---------------------------------------------------------------------------

/// A popup to open on the window.
#[derive(Debug, Clone, Copy)]
pub struct PopupOptions {
    /// Where the popup's top left corner goes, relative to the window
    /// geometry.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub color: [u8; 4],
    /// Take an explicit grab (`xdg_popup.grab`), as menus do. The latest
    /// input serial is used.
    pub grab: bool,
    /// Let the compositor move the popup to keep it on screen (slide on
    /// both axes).
    pub slide: bool,
}

pub(crate) struct Popup {
    pub surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    popup: xdg_popup::XdgPopup,
    size: (i32, i32),
    color: [u8; 4],
}

/// User data of a popup's `xdg_surface`, to tell it from the window's.
pub(crate) struct PopupTag;

impl TestClient {
    /// Opens a popup on the window. It appears after a few pumps (the
    /// compositor has to configure it first). One popup at a time.
    pub fn open_popup(&mut self, options: PopupOptions) -> Result<(), ClientError> {
        let state = &mut self.state;
        let (Some(compositor), Some(wm_base), Some(window)) = (
            &state.globals.compositor,
            &state.globals.wm_base,
            &state.window,
        ) else {
            return Err(ClientError::Protocol(
                "the window does not exist yet".to_string(),
            ));
        };
        let positioner = wm_base.create_positioner(&self.handle, ());
        positioner.set_size(options.width, options.height);
        positioner.set_anchor_rect(options.x, options.y, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        if options.slide {
            positioner.set_constraint_adjustment(
                xdg_positioner::ConstraintAdjustment::SlideX
                    | xdg_positioner::ConstraintAdjustment::SlideY,
            );
        }
        let surface = compositor.create_surface(&self.handle, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &self.handle, PopupTag);
        let popup =
            xdg_surface.get_popup(Some(&window._xdg_surface), &positioner, &self.handle, ());
        positioner.destroy();
        if options.grab
            && let Some(seat) = &state.globals.seat
        {
            popup.grab(seat, state.last_serial);
        }
        surface.commit();
        state.popup = Some(Popup {
            surface,
            xdg_surface,
            popup,
            size: (options.width, options.height),
            color: options.color,
        });
        Ok(())
    }

    /// Closes the popup, if one is open.
    pub fn close_popup(&mut self) {
        if let Some(popup) = self.state.popup.take() {
            popup.destroy();
        }
    }

    /// Whether a popup is open (and was not dismissed).
    pub fn has_popup(&self) -> bool {
        self.state.popup.is_some()
    }
}

impl Popup {
    fn destroy(self) {
        self.popup.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

impl Dispatch<xdg_surface::XdgSurface, PopupTag> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &PopupTag,
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if let (Some(popup), Some(shm)) = (&state.popup, &state.globals.shm)
                && let Ok(buffer) = create_buffer(
                    shm,
                    handle,
                    popup.size.0 as u32,
                    popup.size.1 as u32,
                    popup.color,
                    state.options.layout,
                )
            {
                popup.surface.attach(Some(&buffer), 0, 0);
                popup
                    .surface
                    .damage_buffer(0, 0, popup.size.0, popup.size.1);
                popup.surface.commit();
            }
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => state.events.push(ClientEvent::PopupConfigure {
                x,
                y,
                width,
                height,
            }),
            xdg_popup::Event::PopupDone => {
                state.events.push(ClientEvent::PopupDone);
                if let Some(popup) = state.popup.take() {
                    popup.destroy();
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct OutputEntry {
    /// The name of the global in the registry.
    pub global: u32,
    pub name: String,
    pub(crate) mode: (i32, i32),
    pub(crate) scale: i32,
    pub(crate) position: (i32, i32),
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(&output.id()) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => entry.position = (x, y),
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => entry.mode = (width, height),
            wl_output::Event::Scale { factor } => entry.scale = factor,
            wl_output::Event::Name { name } => entry.name = name,
            wl_output::Event::Done => state.events.push(ClientEvent::Output {
                name: entry.name.clone(),
                width: entry.mode.0,
                height: entry.mode.1,
                scale: entry.scale,
                x: entry.position.0,
                y: entry.position.1,
            }),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Feedback protocols
// ---------------------------------------------------------------------------

/// The output a presentation feedback was synchronized to.
#[derive(Default)]
pub(crate) struct FeedbackData(Mutex<Option<String>>);

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, FeedbackData> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        data: &FeedbackData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wp_presentation_feedback::Event;
        match event {
            Event::SyncOutput { output } => {
                *data.0.lock().unwrap() = state
                    .outputs
                    .get(&output.id())
                    .map(|entry| entry.name.clone());
            }
            Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                seq_hi,
                seq_lo,
                ..
            } => {
                let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                state.events.push(ClientEvent::Presented {
                    time_ns: seconds * 1_000_000_000 + u64::from(tv_nsec),
                    refresh_ns: refresh,
                    sequence: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
                    output: data.0.lock().unwrap().clone(),
                });
            }
            Event::Discarded => state.events.push(ClientEvent::PresentationDiscarded),
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state
                .events
                .push(ClientEvent::FractionalScale { scale_120: scale });
        }
    }
}

// ---------------------------------------------------------------------------
// The data device: selection, drag and drop
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct DataState {
    device: Option<wl_data_device::WlDataDevice>,
    /// What this client offers, and the text it hands out.
    source: Option<(wl_data_source::WlDataSource, String)>,
    /// Offers that were introduced and the types they announced.
    offers: Vec<(wl_data_offer::WlDataOffer, Vec<String>)>,
    selection: Option<wl_data_offer::WlDataOffer>,
    /// The offer of a drag that is over one of our surfaces.
    drag: Option<(wl_data_offer::WlDataOffer, u32)>,
    /// The surface kept alive as the drag icon.
    icon: Option<wl_surface::WlSurface>,
    /// Transfers being read: type, read end, bytes so far, and the offer
    /// to finish afterwards (for a drop).
    reads: Vec<Read_>,
}

struct Read_ {
    mime_type: String,
    pipe: File,
    bytes: Vec<u8>,
    finish: Option<wl_data_offer::WlDataOffer>,
}

impl TestClient {
    fn new_source(&mut self, mime_types: &[&str], text: &str) -> Result<(), ClientError> {
        let state = &mut self.state;
        let manager = state.globals.data_device_manager.as_ref().ok_or_else(|| {
            ClientError::Protocol("the compositor has no wl_data_device_manager".to_string())
        })?;
        let source = manager.create_data_source(&self.handle, ());
        for mime_type in mime_types {
            source.offer(mime_type.to_string());
        }
        state.data.source = Some((source, text.to_string()));
        Ok(())
    }

    /// Makes this client the owner of the selection (what "copy" does). It
    /// needs the keyboard focus; the latest input serial is used.
    pub fn set_selection(&mut self, mime_types: &[&str], text: &str) -> Result<(), ClientError> {
        self.new_source(mime_types, text)?;
        let state = &self.state;
        let (Some(device), Some((source, _))) = (&state.data.device, &state.data.source) else {
            return Err(ClientError::Protocol("no data device yet".to_string()));
        };
        device.set_selection(Some(source), state.last_serial);
        Ok(())
    }

    /// Asks for the contents of the selection (what "paste" does). The
    /// answer arrives as [`ClientEvent::Received`]. Returns `false` when
    /// there is no selection.
    pub fn request_selection(&mut self, mime_type: &str) -> Result<bool, ClientError> {
        let Some(offer) = self.state.data.selection.clone() else {
            return Ok(false);
        };
        self.state.receive(&offer, mime_type, None)?;
        Ok(true)
    }

    /// Starts dragging from the window (the pointer button must be down on
    /// it; the serial of the latest press is used). With `icon`, a 16x16
    /// surface of that color follows the pointer.
    pub fn start_drag(
        &mut self,
        mime_types: &[&str],
        text: &str,
        icon: Option<[u8; 4]>,
    ) -> Result<(), ClientError> {
        self.new_source(mime_types, text)?;
        let state = &mut self.state;
        let (Some(device), Some((source, _)), Some(window), Some(compositor), Some(shm)) = (
            &state.data.device,
            &state.data.source,
            &state.window,
            &state.globals.compositor,
            &state.globals.shm,
        ) else {
            return Err(ClientError::Protocol("not ready to drag".to_string()));
        };
        source.set_actions(
            wl_data_device_manager::DndAction::Copy | wl_data_device_manager::DndAction::Move,
        );
        let icon_surface = match icon {
            Some(color) => {
                let surface = compositor.create_surface(&self.handle, ());
                let buffer = create_buffer(shm, &self.handle, 16, 16, color, state.options.layout)?;
                Some((surface, buffer))
            }
            None => None,
        };
        device.start_drag(
            Some(source),
            &window.surface,
            icon_surface.as_ref().map(|(surface, _)| surface),
            state.last_press_serial,
        );
        // The surface gets its role from start_drag; contents come after.
        if let Some((surface, buffer)) = icon_surface {
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, 16, 16);
            surface.commit();
            state.data.icon = Some(surface);
        }
        Ok(())
    }
}

impl State {
    /// Asks an offer for its data and remembers the pipe to read from.
    fn receive(
        &mut self,
        offer: &wl_data_offer::WlDataOffer,
        mime_type: &str,
        finish: Option<wl_data_offer::WlDataOffer>,
    ) -> Result<(), ClientError> {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
            .map_err(|e| ClientError::Io(e.into()))?;
        rustix::fs::fcntl_setfl(&read, rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| ClientError::Io(e.into()))?;
        offer.receive(mime_type.to_string(), write.as_fd());
        // The write end is closed here, once the request is queued (it was
        // duplicated into the message); the sender holds the only one left.
        drop(write);
        self.data.reads.push(Read_ {
            mime_type: mime_type.to_string(),
            pipe: File::from(read),
            bytes: Vec::new(),
            finish,
        });
        Ok(())
    }

    /// The types an offer announced.
    fn types_of(&self, offer: &wl_data_offer::WlDataOffer) -> Vec<String> {
        self.data
            .offers
            .iter()
            .find(|(known, _)| known == offer)
            .map(|(_, types)| types.clone())
            .unwrap_or_default()
    }

    /// Called from every pump: creates the data device once the seat and
    /// the manager are bound, and reads what has arrived on open pipes.
    pub(crate) fn advance_features(
        &mut self,
        handle: &QueueHandle<State>,
    ) -> Result<(), ClientError> {
        if self.data.device.is_none()
            && let (Some(manager), Some(seat)) =
                (&self.globals.data_device_manager, &self.globals.seat)
        {
            self.data.device = Some(manager.get_data_device(seat, handle, ()));
        }

        let mut index = 0;
        while index < self.data.reads.len() {
            let read = &mut self.data.reads[index];
            let mut chunk = [0u8; 4096];
            let finished = loop {
                match read.pipe.read(&mut chunk) {
                    Ok(0) => break true,
                    Ok(count) => read.bytes.extend_from_slice(&chunk[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break false,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(ClientError::Io(error)),
                }
            };
            if finished {
                let read = self.data.reads.remove(index);
                if let Some(offer) = read.finish {
                    offer.finish();
                    offer.destroy();
                }
                self.events.push(ClientEvent::Received {
                    mime_type: read.mime_type,
                    text: String::from_utf8_lossy(&read.bytes).into_owned(),
                });
            } else {
                index += 1;
            }
        }
        Ok(())
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_source::Event;
        match event {
            Event::Send { mime_type, fd } => {
                if let Some((_, text)) = &state.data.source {
                    // Small texts fit into the pipe; the reader may not be
                    // reading yet.
                    let _ = File::from(fd).write_all(text.as_bytes());
                }
                state.events.push(ClientEvent::SourceSend { mime_type });
            }
            Event::Cancelled => {
                if let Some((source, _)) = state.data.source.take() {
                    source.destroy();
                }
                state.data.icon = None;
                state.events.push(ClientEvent::SourceCancelled);
            }
            Event::Target { mime_type } => {
                state.events.push(ClientEvent::SourceTarget { mime_type })
            }
            Event::DndDropPerformed => state.events.push(ClientEvent::SourceDropPerformed),
            Event::DndFinished => {
                if let Some((source, _)) = state.data.source.take() {
                    source.destroy();
                }
                state.data.icon = None;
                state.events.push(ClientEvent::SourceFinished);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && let Some((_, types)) = state
                .data
                .offers
                .iter_mut()
                .find(|(known, _)| known == offer)
        {
            types.push(mime_type);
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_device::Event;
        match event {
            Event::DataOffer { id } => state.data.offers.push((id, Vec::new())),
            Event::Selection { id } => {
                let mime_types = id.as_ref().map(|o| state.types_of(o)).unwrap_or_default();
                // Offers of earlier selections are of no use any more.
                if let Some(old) = state.data.selection.take()
                    && Some(&old) != id.as_ref()
                {
                    state.data.offers.retain(|(known, _)| known != &old);
                    old.destroy();
                }
                state.data.selection = id;
                state.events.push(ClientEvent::Selection { mime_types });
            }
            Event::Enter {
                serial,
                x,
                y,
                id: Some(offer),
                ..
            } => {
                let mime_types = state.types_of(&offer);
                // Take the first type on offer, and prefer to copy.
                offer.accept(serial, mime_types.first().cloned());
                offer.set_actions(
                    wl_data_device_manager::DndAction::Copy
                        | wl_data_device_manager::DndAction::Move,
                    wl_data_device_manager::DndAction::Copy,
                );
                state.data.drag = Some((offer, serial));
                state
                    .events
                    .push(ClientEvent::DragEnter { x, y, mime_types });
            }
            Event::Motion { x, y, .. } => state.events.push(ClientEvent::DragMotion { x, y }),
            Event::Leave => {
                if let Some((offer, _)) = state.data.drag.take() {
                    state.data.offers.retain(|(known, _)| known != &offer);
                    offer.destroy();
                }
                state.events.push(ClientEvent::DragLeave);
            }
            Event::Drop => {
                state.events.push(ClientEvent::DragDrop);
                if let Some((offer, _)) = state.data.drag.take() {
                    let mime_types = state.types_of(&offer);
                    state.data.offers.retain(|(known, _)| known != &offer);
                    if let Some(mime_type) = mime_types.first() {
                        let _ = state.receive(&offer, mime_type, Some(offer.clone()));
                    }
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

delegate_noop!(State: xdg_positioner::XdgPositioner);
delegate_noop!(State: wp_viewporter::WpViewporter);
delegate_noop!(State: wp_viewport::WpViewport);
delegate_noop!(State: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(State: ignore wp_presentation::WpPresentation);
delegate_noop!(State: wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
