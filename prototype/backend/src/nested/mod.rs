//! The nested backend: the compositor runs as a window of another Wayland
//! compositor (the parent).
//!
//! The window is the one output. Its size follows what the parent
//! configures, its scale the scale the parent shows it at.
//!
//! Frames reach the parent in one of two ways:
//!
//! - **dma-buf**: the render targets are exported as dma-bufs
//!   ([`TargetProvider::create_scanout_target`]) and attached as they are.
//!   A few of them rotate; one is drawn into again only after the parent
//!   released it. The GPU has finished a frame before it is attached, so
//!   nothing else has to synchronize the two sides.
//! - **`wl_shm`**: the frame is read back and copied into shared memory.
//!   Used when the parent or the device cannot do the first.
//!
//! Frames are paced by the parent's frame callbacks: a requested frame is
//! granted when the callback of the previous one arrived. While the parent
//! does not show the window no callbacks come and nothing is drawn.
//!
//! Input comes from the parent's seat. The parent's cursor is hidden over
//! the window when the compositor inside draws its own.
//!
//! Text input goes through the parent's input method, when it offers
//! text-input-v3 (see [`text_input`]).
//!
//! [`TargetProvider::create_scanout_target`]: crate::TargetProvider::create_scanout_target

mod bridge;
mod client;
mod selection;
mod shm;
mod text_input;

use std::cell::RefCell;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction};
use proto_color::{CompositeSpace, ImageDescription};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_buffer, wl_shm};
use wayland_client::{Connection, EventQueue, Proxy};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1;

use self::client::{Buffer, BufferKind, OUTPUT, Output, State};
use self::shm::ShmMap;
pub use crate::LogoKey;
use crate::target::{OutputFormat, TargetDesc, TargetError, TargetId, TargetProvider, Targets};
use crate::{
    Backend, BackendError, BackendEvent, BackendHost, OutputId, OutputInfo, SelectionCrossing,
    SelectionKind, TextInputState, monotonic_now,
};

/// How frames are handed to the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NestedBuffers {
    /// dma-bufs when the parent and the device can, else `wl_shm`.
    Auto,
    Shm,
    /// dma-bufs, or fail.
    Dmabuf,
}

/// Which key of the parent the core is given as the logo (Super) key. A
/// parent that keeps Super combinations to itself would never let them
/// through; another modifier then stands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogoKeyChoice {
    /// Super while the parent passes its shortcuts to the window
    /// (keyboard-shortcuts-inhibit is active), Alt while it does not: it
    /// does not offer the protocol, refused, was not asked, or the person
    /// took the shortcuts back. Follows the parent as that changes.
    #[default]
    Auto,
    /// Always this key.
    Fixed(LogoKey),
}

impl LogoKeyChoice {
    /// The key that is the logo key while the parent does (`inhibited`) or
    /// does not pass its shortcuts to the window.
    pub fn resolve(self, inhibited: bool) -> LogoKey {
        match self {
            LogoKeyChoice::Auto if inhibited => LogoKey::Super,
            LogoKeyChoice::Auto => LogoKey::Alt,
            LogoKeyChoice::Fixed(key) => key,
        }
    }

    /// `auto`, or the name of the fixed key.
    pub fn name(self) -> &'static str {
        match self {
            LogoKeyChoice::Auto => "auto",
            LogoKeyChoice::Fixed(key) => key.name(),
        }
    }
}

/// Configuration of the nested backend.
#[derive(Debug, Clone)]
pub struct NestedConfig {
    /// The Wayland socket of the parent.
    pub socket: PathBuf,
    /// Size of the window when the parent leaves it to the client, in the
    /// parent's logical coordinates.
    pub size: (u32, u32),
    pub title: String,
    pub app_id: String,
    /// What the frames mean. The parent is given no color information, so
    /// it takes them as it takes any such window.
    pub image_description: ImageDescription,
    /// The space surfaces are blended in.
    pub composite_space: CompositeSpace,
    pub buffers: NestedBuffers,
    pub logo_key: LogoKeyChoice,
    /// Ask the parent to pass its own keyboard shortcuts to the window
    /// while it has the keyboard.
    pub inhibit_shortcuts: bool,
    /// Hide the parent's cursor over the window: the compositor inside
    /// draws its own. Otherwise the parent shows its default cursor there.
    pub hide_parent_cursor: bool,
    /// Ask the parent to show the window fullscreen. The parent decides.
    pub fullscreen: bool,
    /// Which way the clipboard crosses between the parent and the inside.
    pub selection: SelectionCrossing,
    /// Whether the primary selection crosses too, the same way.
    pub primary_selection: bool,
}

/// The layout every device reads the same way: rows of pixels.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// How many dma-buf buffers rotate, and at most how many `wl_shm` buffers
/// exist at a time. A parent holds the buffer it shows and, until its GPU
/// is done with it, the one before: with fewer than four, frames wait for
/// a release.
const BUFFER_COUNT: usize = 4;
const MAX_SHM_BUFFERS: usize = 4;

struct Inner {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    config: NestedConfig,
    /// The frames are drawn on another device than the parent composites
    /// with: only a layout every device reads the same way is handed over.
    linear_only: bool,
}

fn device(context: &str, error: impl std::fmt::Display) -> BackendError {
    BackendError::Device(format!("{context}: {error}"))
}

impl Inner {
    fn roundtrip(&mut self) -> Result<(), BackendError> {
        self.queue
            .roundtrip(&mut self.state)
            .map(|_| ())
            .map_err(|e| device("talking to the parent compositor", e))
    }

    fn flush(&mut self) {
        match self.connection.flush() {
            Ok(()) => {}
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => self.lost(&error),
        }
    }

    fn lost(&mut self, error: &dyn std::fmt::Display) {
        if !self.state.lost {
            tracing::error!("the connection to the parent compositor is gone: {error}");
            self.state.lost = true;
            self.state.events.push_back(BackendEvent::QuitRequested);
        }
    }

    /// Reads and handles what the parent sent. Returns whether events for
    /// the core are waiting.
    fn dispatch(&mut self) -> bool {
        for _ in 0..64 {
            if let Err(error) = self.queue.dispatch_pending(&mut self.state) {
                self.lost(&error);
                break;
            }
            let Some(guard) = self.connection.prepare_read() else {
                continue;
            };
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    break;
                }
                Err(error) => {
                    self.lost(&error);
                    break;
                }
            }
        }
        if !self.state.lost {
            if let Err(error) = self.queue.dispatch_pending(&mut self.state) {
                self.lost(&error);
            }
            self.flush();
        }
        !self.state.events.is_empty()
    }

    fn target_desc(&self, size: (u32, u32)) -> TargetDesc {
        TargetDesc {
            width: size.0,
            height: size.1,
            format: OutputFormat::Bgra8Unorm,
            image_description: self.config.image_description,
            composite_space: self.config.composite_space,
        }
    }

    /// One render target exported as a dma-buf, as a buffer of the parent,
    /// and the modifier it got.
    ///
    /// Without `proven`, the parent is asked and its answer awaited: a
    /// buffer it cannot import is then an error to fall back from. With
    /// `proven` (the parent took a buffer made the same way) the buffer is
    /// made without waiting, where a refusal would end the connection.
    fn create_dmabuf_buffer(
        &mut self,
        targets: &mut (dyn TargetProvider + 'static),
        desc: &TargetDesc,
        modifiers: &[u64],
        proven: bool,
    ) -> Result<(Buffer, u64), BackendError> {
        let (target, plane) = targets.create_scanout_target(desc, modifiers)?;
        let dmabuf = self.state.dmabuf.clone().expect("checked by the caller");
        let params = dmabuf.create_params(&self.state.qh, ());
        params.add(
            plane.fd.as_fd(),
            0,
            plane.offset,
            plane.stride,
            (plane.modifier >> 32) as u32,
            plane.modifier as u32,
        );
        if proven {
            let wl = params.create_immed(
                plane.width as i32,
                plane.height as i32,
                plane.fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                &self.state.qh,
                (),
            );
            params.destroy();
            let buffer = Buffer {
                wl,
                kind: BufferKind::Dmabuf(target),
                size: (desc.width, desc.height),
                busy: false,
            };
            return Ok((buffer, plane.modifier));
        }
        self.state.params_result = None;
        params.create(
            plane.width as i32,
            plane.height as i32,
            plane.fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );
        // The answer arrives before the roundtrip's.
        let result = self.roundtrip();
        params.destroy();
        match (result, self.state.params_result.take()) {
            (Ok(()), Some(Some(wl))) => Ok((
                Buffer {
                    wl,
                    kind: BufferKind::Dmabuf(target),
                    size: (desc.width, desc.height),
                    busy: false,
                },
                plane.modifier,
            )),
            (result, _) => {
                targets.destroy_target(target);
                result?;
                Err(BackendError::Device(format!(
                    "the parent compositor refused a dma-buf with modifier {:#x}",
                    plane.modifier
                )))
            }
        }
    }

    /// The rotating dma-buf buffers, or why there are none.
    fn create_dmabuf_buffers(
        &mut self,
        targets: &mut (dyn TargetProvider + 'static),
        desc: &TargetDesc,
    ) -> Result<Vec<Buffer>, BackendError> {
        if self.state.dmabuf.is_none() {
            return Err(BackendError::InvalidConfig(
                "the parent compositor does not offer zwp_linux_dmabuf_v1 (version 3 or later)"
                    .into(),
            ));
        }
        let ours = targets.scanout_modifiers();
        if ours.is_empty() {
            return Err(BackendError::InvalidConfig(
                "the device cannot export render targets as dma-bufs".into(),
            ));
        }
        let mut theirs = self.state.parent_modifiers();
        if self.linear_only {
            theirs.retain(|modifier| *modifier == DRM_FORMAT_MOD_LINEAR);
        }
        let modifiers: Vec<u64> = ours
            .iter()
            .copied()
            .filter(|modifier| theirs.contains(modifier))
            .collect();
        if modifiers.is_empty() {
            return Err(BackendError::InvalidConfig(format!(
                "the parent compositor takes no XRGB8888 dma-buf with a modifier the device can \
                 export (device: {} modifiers, parent: {})",
                ours.len(),
                theirs.len()
            )));
        }
        // The first buffer finds out what the parent takes; the others are
        // made the same way without asking again.
        let mut buffers = Vec::new();
        let mut modifiers = modifiers;
        for index in 0..BUFFER_COUNT {
            match self.create_dmabuf_buffer(targets, desc, &modifiers, index > 0) {
                Ok((buffer, modifier)) => {
                    buffers.push(buffer);
                    modifiers = vec![modifier];
                }
                Err(error) => {
                    for buffer in buffers {
                        buffer.wl.destroy();
                        if let BufferKind::Dmabuf(target) = buffer.kind {
                            targets.destroy_target(target);
                        }
                    }
                    return Err(error);
                }
            }
        }
        Ok(buffers)
    }

    /// Makes the buffers for the window as it is now and brings the output
    /// in line with it. Returns the output's description.
    fn setup_output(&mut self, targets: Targets<'_>) -> Result<OutputInfo, BackendError> {
        let logical = self.state.wanted_logical;
        let size = self.state.wanted_physical();
        let scale = self.state.scale();
        let desc = self.target_desc(size);

        let mut buffers = Vec::new();
        let mut offscreen = None;
        let mut dmabuf = false;
        if let Some(targets) = targets {
            if self.config.buffers != NestedBuffers::Shm {
                match self.create_dmabuf_buffers(targets, &desc) {
                    Ok(created) => {
                        buffers = created;
                        dmabuf = true;
                    }
                    Err(error) if self.config.buffers == NestedBuffers::Dmabuf => {
                        return Err(error);
                    }
                    Err(error) => {
                        tracing::info!("frames are copied to the parent through wl_shm: {error}");
                    }
                }
            }
            if !dmabuf {
                offscreen = Some(targets.create_target(&desc)?);
            }
        }

        let refresh_mhz = self.state.parent_refresh_mhz().unwrap_or(60_000);
        let previous = self.state.output.take();
        let info = OutputInfo {
            id: OUTPUT,
            name: "NESTED-1".to_string(),
            make: "hizukuri".to_string(),
            model: "nested".to_string(),
            size,
            scale,
            refresh_mhz,
            position: (0, 0),
            image_description: self.config.image_description,
            format: OutputFormat::Bgra8Unorm,
            supported_image_descriptions: vec![self.config.image_description],
        };
        self.state.output = Some(Output {
            info: info.clone(),
            logical,
            offscreen,
            dmabuf,
            buffers,
            back: None,
            shown: None,
            sequence: previous.as_ref().map_or(0, |o| o.sequence),
            last_presented: previous.as_ref().and_then(|o| o.last_presented),
            refresh: previous.as_ref().and_then(|o| o.refresh),
            frames_presented: previous.as_ref().map_or(0, |o| o.frames_presented),
        });
        tracing::info!(
            size = ?size,
            scale,
            buffers = if dmabuf { "dmabuf" } else { "shm" },
            "nested output"
        );
        Ok(info)
    }

    /// Lets go of the output's buffers. Those the parent still holds are
    /// destroyed when it releases them.
    fn teardown_output(&mut self, mut targets: Targets<'_>) {
        let Some(output) = &mut self.state.output else {
            return;
        };
        if let (Some(target), Some(targets)) = (output.offscreen.take(), targets.as_deref_mut()) {
            targets.destroy_target(target);
        }
        output.back = None;
        output.shown = None;
        for buffer in std::mem::take(&mut output.buffers) {
            if buffer.busy {
                self.state.retired.push(buffer);
                continue;
            }
            buffer.wl.destroy();
            if let (BufferKind::Dmabuf(target), Some(targets)) =
                (buffer.kind, targets.as_deref_mut())
            {
                targets.destroy_target(target);
            }
        }
        self.collect_garbage(targets);
    }

    fn collect_garbage(&mut self, targets: Targets<'_>) {
        if let Some(targets) = targets {
            for target in self.state.garbage.drain(..) {
                targets.destroy_target(target);
            }
        }
    }

    /// A free `wl_shm` buffer of the output's size, making one if needed.
    fn free_shm_buffer(&mut self) -> Result<Option<usize>, BackendError> {
        let Some(output) = &mut self.state.output else {
            return Ok(None);
        };
        if let Some(index) = output.buffers.iter().position(|b| !b.busy) {
            return Ok(Some(index));
        }
        if output.buffers.len() >= MAX_SHM_BUFFERS {
            return Ok(None);
        }
        let Some(shm) = &self.state.shm else {
            return Err(BackendError::Device(
                "the parent compositor does not offer wl_shm".into(),
            ));
        };
        let (width, height) = output.info.size;
        let stride = width as usize * 4;
        let map = ShmMap::new(stride * height as usize)
            .map_err(|e| device("creating a shared memory buffer", e))?;
        let pool = shm.create_pool(map.fd(), map.len() as i32, &self.state.qh, ());
        let wl = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Xrgb8888,
            &self.state.qh,
            (),
        );
        pool.destroy();
        output.buffers.push(Buffer {
            wl,
            kind: BufferKind::Shm(map),
            size: (width, height),
            busy: false,
        });
        Ok(Some(output.buffers.len() - 1))
    }

    /// Attaches a buffer and commits the window, asking for the callback
    /// the next frame waits for.
    fn commit(&mut self, buffer: &wl_buffer::WlBuffer, size: (u32, u32)) {
        let serial = self.state.next_callback();
        let state = &mut self.state;
        let (Some(surface), Some(output)) = (&state.surface, &state.output) else {
            return;
        };
        match &state.viewport {
            Some(viewport) => {
                viewport.set_destination(output.logical.0 as i32, output.logical.1 as i32);
            }
            None => surface.set_buffer_scale(output.info.scale.round().max(1.0) as i32),
        }
        surface.attach(Some(buffer), 0, 0);
        if surface.version() >= 4 {
            surface.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
        } else {
            surface.damage(0, 0, i32::MAX, i32::MAX);
        }
        surface.frame(&state.qh, serial);
        if let Some(presentation) = &state.presentation {
            presentation.feedback(surface, &state.qh, serial);
        }
        surface.commit();
        state.callback_pending = Some(serial);
    }

    fn present(&mut self, mut targets: Targets<'_>) -> Result<(), BackendError> {
        if self.state.lost || self.state.closed {
            return Ok(());
        }
        self.collect_garbage(targets.as_deref_mut());
        let Some(output) = &self.state.output else {
            return Err(BackendError::UnknownOutput);
        };
        let size = output.info.size;
        let index = if output.dmabuf {
            // Nothing was drawn (no buffer was free when the frame began).
            let Some(index) = output.back else {
                return Ok(());
            };
            index
        } else {
            let Some(index) = self.free_shm_buffer()? else {
                // The parent holds every buffer; the frame is shown with
                // the next one.
                self.state.frame_requested = true;
                return Ok(());
            };
            let output = self.state.output.as_mut().expect("checked above");
            if let (Some(targets), Some(target), BufferKind::Shm(map)) =
                (targets, output.offscreen, &mut output.buffers[index].kind)
            {
                let stride = size.0 as usize * 4;
                match targets.read_encoded_into(target, map.bytes_mut(), stride) {
                    // Nothing was rendered yet: the buffer shows as it is.
                    Ok(()) | Err(TargetError::TargetNotRendered) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            index
        };
        let output = self.state.output.as_mut().expect("checked above");
        let buffer = &mut output.buffers[index];
        buffer.busy = true;
        let (wl, size) = (buffer.wl.clone(), buffer.size);
        output.shown = match buffer.kind {
            BufferKind::Dmabuf(target) => Some(target),
            BufferKind::Shm(_) => output.offscreen,
        };
        output.back = None;
        self.commit(&wl, size);
        self.flush();
        Ok(())
    }

    fn reconfigure(&mut self, mut targets: Targets<'_>) -> Result<(), BackendError> {
        self.collect_garbage(targets.as_deref_mut());
        if !self.state.resize_pending || self.state.lost {
            return Ok(());
        }
        self.teardown_output(targets.as_deref_mut());
        let info = self.setup_output(targets)?;
        self.state.resize_pending = false;
        self.state
            .events
            .push_back(BackendEvent::OutputChanged(info));
        self.state.try_grant();
        self.flush();
        Ok(())
    }
}

/// See the module documentation.
pub struct NestedBackend {
    inner: Rc<RefCell<Inner>>,
}

impl NestedBackend {
    /// Connects to the parent and creates the window, without showing
    /// anything yet. After this the device the parent composites with is
    /// known ([`Self::parent_device`]), which whatever draws should use
    /// too; [`Self::start`] then makes the output.
    pub fn connect(config: &NestedConfig) -> Result<Self, BackendError> {
        if config.size.0 == 0 || config.size.1 == 0 {
            return Err(BackendError::InvalidConfig(
                "the window has an empty size".into(),
            ));
        }
        let stream = UnixStream::connect(&config.socket).map_err(|e| {
            BackendError::Session(format!(
                "cannot connect to the parent compositor at {}: {e}",
                config.socket.display()
            ))
        })?;
        let connection = Connection::from_socket(stream)
            .map_err(|e| BackendError::Session(format!("the parent compositor: {e}")))?;
        let queue: EventQueue<State> = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut inner = Inner {
            connection,
            queue,
            state: State::new(
                qh.clone(),
                config.logo_key,
                config.inhibit_shortcuts,
                config.hide_parent_cursor,
            ),
            config: config.clone(),
            linear_only: false,
        };
        let display = inner.connection.display();
        inner
            .state
            .selections
            .configure(config.selection, config.primary_selection, display);
        // Once for the globals, once for what they announce.
        inner.roundtrip()?;
        inner.roundtrip()?;

        let state = &mut inner.state;
        let missing = |name: &str| {
            BackendError::Session(format!("the parent compositor does not offer {name}"))
        };
        let compositor = state
            .compositor
            .clone()
            .ok_or_else(|| missing("wl_compositor"))?;
        let wm_base = state
            .wm_base
            .clone()
            .ok_or_else(|| missing("xdg_wm_base"))?;
        if state.shm.is_none() {
            return Err(missing("wl_shm"));
        }
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title(config.title.clone());
        toplevel.set_app_id(config.app_id.clone());
        if config.fullscreen {
            toplevel.set_fullscreen(None);
        }
        if let Some(manager) = &state.decoration_manager {
            // The window has no decorations of its own.
            manager
                .get_toplevel_decoration(&toplevel, &qh, ())
                .set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        }
        if let Some(viewporter) = &state.viewporter {
            state.viewport = Some(viewporter.get_viewport(&surface, &qh, ()));
            // A fractional scale can only be followed with a viewport.
            if let Some(manager) = &state.fractional_manager {
                manager.get_fractional_scale(&surface, &qh, ());
            }
        }
        state.wanted_logical = config.size;
        state.surface = Some(surface.clone());
        state.xdg_surface = Some(xdg_surface);
        state.toplevel = Some(toplevel);
        state.inhibit_shortcuts();
        if config.inhibit_shortcuts && state.inhibit_manager.is_none() {
            tracing::warn!(
                "the parent compositor does not offer keyboard-shortcuts-inhibit: it keeps its \
                 own shortcuts"
            );
        }
        surface.commit();
        // The first configure, and the dma-buf feedback. A parent may send
        // the configure some time after the commit that asks for it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            inner.roundtrip()?;
            let ready = inner.state.configured
                && (inner.state.dmabuf.is_none() || inner.state.feedback_done);
            if ready || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // A scale that arrives right after the configure saves making the
        // buffers twice.
        inner.roundtrip()?;
        if !inner.state.configured {
            return Err(BackendError::Session(
                "the parent compositor did not configure the window".into(),
            ));
        }
        // By now the parent said whether it passes its shortcuts, if it
        // did at once; a later answer changes the key again.
        inner.state.announce_logo_key();
        Ok(Self {
            inner: Rc::new(RefCell::new(inner)),
        })
    }

    /// The device the parent composites with (major, minor), when it says.
    pub fn parent_device(&self) -> Option<(u32, u32)> {
        self.inner.borrow().state.main_device
    }

    /// Makes the output and its buffers. Announced with
    /// [`BackendEvent::OutputAdded`]. `on_parent_device`: whatever draws
    /// uses the device of [`Self::parent_device`].
    pub fn start(
        &mut self,
        targets: Targets<'_>,
        on_parent_device: bool,
    ) -> Result<(), BackendError> {
        let mut inner = self.inner.borrow_mut();
        inner.linear_only = inner.state.main_device.is_some() && !on_parent_device;
        if inner.linear_only {
            tracing::info!(
                "drawing on another device than the parent composites with: frames are handed \
                 over in a linear layout, or copied"
            );
        }
        let info = inner.setup_output(targets)?;
        inner
            .state
            .events
            .push_back(BackendEvent::OutputAdded(info));
        inner.flush();
        Ok(())
    }

    /// Registers the connection to the parent on an event loop.
    pub fn insert_sources<D: BackendHost + 'static>(
        &self,
        handle: &LoopHandle<'static, D>,
    ) -> Result<(), BackendError> {
        let fd = self
            .inner
            .borrow()
            .connection
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| BackendError::EventLoop(e.to_string()))?;
        let inner = Rc::clone(&self.inner);
        handle
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                move |_, _, data: &mut D| {
                    // Release the borrow before calling into the host, which
                    // calls back into the backend.
                    let (woke, lost) = {
                        let mut inner = inner.borrow_mut();
                        (inner.dispatch(), inner.state.lost)
                    };
                    if woke {
                        data.backend_wake();
                    }
                    Ok(if lost {
                        PostAction::Remove
                    } else {
                        PostAction::Continue
                    })
                },
            )
            .map(|_| ())
            .map_err(|e| BackendError::EventLoop(e.to_string()))
    }
}

impl Backend for NestedBackend {
    fn name(&self) -> &'static str {
        "nested"
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.inner
            .borrow()
            .state
            .output
            .iter()
            .map(|o| o.info.clone())
            .collect()
    }

    fn is_active(&self) -> bool {
        !self.inner.borrow().state.lost
    }

    fn render_target(&mut self, output: OutputId) -> Option<TargetId> {
        let mut inner = self.inner.borrow_mut();
        let state = inner
            .state
            .output
            .as_mut()
            .filter(|o| o.info.id == output)?;
        if !state.dmabuf {
            return state.offscreen;
        }
        if state.back.is_none() {
            state.back = state.buffers.iter().position(|b| !b.busy);
        }
        match state.buffers[state.back?].kind {
            BufferKind::Dmabuf(target) => Some(target),
            BufferKind::Shm(_) => None,
        }
    }

    fn shown_target(&mut self, output: OutputId) -> Option<TargetId> {
        let shown = {
            let inner = self.inner.borrow();
            let state = inner
                .state
                .output
                .as_ref()
                .filter(|o| o.info.id == output)?;
            state.shown
        };
        shown.or_else(|| self.render_target(output))
    }

    fn request_frame(&mut self, output: OutputId) {
        if output != OUTPUT {
            return;
        }
        let mut inner = self.inner.borrow_mut();
        inner.state.frame_requested = true;
        inner.state.try_grant();
    }

    fn present(&mut self, targets: Targets<'_>, output: OutputId) -> Result<(), BackendError> {
        if output != OUTPUT {
            return Err(BackendError::UnknownOutput);
        }
        self.inner.borrow_mut().present(targets)
    }

    fn reconfigure(&mut self, targets: Targets<'_>) -> Result<(), BackendError> {
        self.inner.borrow_mut().reconfigure(targets)
    }

    fn shutdown(&mut self, targets: Targets<'_>) {
        let mut inner = self.inner.borrow_mut();
        inner.teardown_output(targets);
        inner.state.output = None;
        if !inner.state.lost {
            if let Some(toplevel) = inner.state.toplevel.take() {
                toplevel.destroy();
            }
            if let Some(xdg_surface) = inner.state.xdg_surface.take() {
                xdg_surface.destroy();
            }
            if let Some(surface) = inner.state.surface.take() {
                surface.destroy();
            }
            inner.flush();
        }
    }

    fn set_pointer_lock(&mut self, locked: bool) {
        let mut inner = self.inner.borrow_mut();
        inner.state.set_pointer_lock(locked);
        inner.flush();
    }

    fn set_text_input(&mut self, state: Option<&TextInputState>) {
        let mut inner = self.inner.borrow_mut();
        inner.state.set_text_input(state);
        inner.flush();
    }

    fn offer_selection(&mut self, kind: SelectionKind, mime_types: Option<&[String]>) {
        let mut inner = self.inner.borrow_mut();
        inner
            .state
            .inner_selection_changed(kind, mime_types.map(<[String]>::to_vec));
        inner.flush();
    }

    fn receive_outer_selection(
        &mut self,
        kind: SelectionKind,
        generation: u64,
        mime_type: &str,
        fd: std::os::fd::OwnedFd,
    ) -> bool {
        let mut inner = self.inner.borrow_mut();
        let asked = inner
            .state
            .receive_outer_selection(kind, generation, mime_type, fd);
        inner.flush();
        asked
    }

    fn describe(&self) -> serde_json::Value {
        let inner = self.inner.borrow();
        let state = &inner.state;
        let mut parent_globals: Vec<String> = state
            .parent_globals
            .iter()
            .map(|(_, interface, version)| format!("{interface} {version}"))
            .collect();
        parent_globals.sort();
        serde_json::json!({
            "parent_socket": inner.config.socket,
            "parent_device": state.main_device.map(|(major, minor)| [major, minor]),
            "on_parent_device": !inner.linear_only,
            "connected": !state.lost,
            "shortcuts_inhibited": state.shortcuts_inhibited,
            "logo_key": state.current_logo_key().name(),
            "logo_key_choice": state.logo_choice.name(),
            "parent_keymap": state.parent_keymap,
            "parent_globals": parent_globals,
            "pointer_locked": state.pointer_locked(),
            "text_input": state.text_input.describe(),
            "selection": state.selections.describe(),
            "presentation_feedback": state.presentation.is_some(),
            "fractional_scale": state.viewport.is_some() && state.fractional_manager.is_some(),
            "output": state.output.as_ref().map(|output| serde_json::json!({
                "buffers": if output.dmabuf { "dmabuf" } else { "shm" },
                "buffer_count": output.buffers.len(),
                "busy_buffers": output.buffers.iter().filter(|b| b.busy).count(),
                "logical_size": [output.logical.0, output.logical.1],
                "frames_presented": output.frames_presented,
                "refresh_ns": output.refresh.map(|r| r.as_nanos() as u64),
            })),
        })
    }

    fn next_presentation(&self, output: OutputId) -> Option<Duration> {
        let inner = self.inner.borrow();
        let state = inner
            .state
            .output
            .as_ref()
            .filter(|o| o.info.id == output)?;
        let last = state.last_presented?;
        Some(crate::kms::frame::next_vblank(
            last,
            state.refresh.unwrap_or(state.info.frame_interval()),
            monotonic_now(),
            inner.state.callback_pending.is_some(),
        ))
    }

    fn drain_events(&mut self) -> Vec<BackendEvent> {
        self.inner.borrow_mut().state.events.drain(..).collect()
    }
}
