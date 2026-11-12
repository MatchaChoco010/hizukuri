//! The session: libseat, udev and libinput, wired to a [`KmsBackend`] over
//! a real DRM device on a calloop event loop.
//!
//! This is the part that cannot run without a seat (a login on a virtual
//! terminal, in a VM or on real hardware). It is kept as thin as possible:
//! every decision is in [`super::backend`], [`super::plan`],
//! [`super::frame`] and [`super::input`], which are tested without devices.
//! What is here only moves events:
//!
//! | source | what happens |
//! | --- | --- |
//! | libseat | session paused / resumed → `KmsBackend::session_paused` / `session_resumed`, libinput suspended / resumed |
//! | DRM device readable | `KmsBackend::handle_device_events` (page flips) |
//! | udev change | `KmsBackend::displays_changed` |
//! | libinput | events → [`RawInput`] → `KmsBackend::push_input`; VT switch hotkeys → libseat |
//! | timer | `KmsBackend::check_timeouts` |

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Dispatcher, EventLoop, Interest, LoopHandle, Mode, PostAction};
use proto_render::Targets;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource as SmithayAxisSource, ButtonState, Event, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::libseat::{LibSeatSession, LibSeatSessionNotifier};
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent, primary_gpu};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;

use super::backend::{KmsBackend, KmsConfig};
use super::clock::MonotonicClock;
use super::device::KmsDevice;
use super::drm_device::DrmDevice;
use super::input::{Hotkey, RawInput};
use crate::{AxisSource, BackendError, BackendHost};

/// The KMS backend on a real device.
pub type RealKmsBackend = KmsBackend<DrmDevice, MonotonicClock>;

/// How long to wait for the seat to become active at startup.
const SEAT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often flips are checked for being overdue.
const TIMEOUT_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// A session with its DRM device open, before the backend is started.
///
/// Opening is separate from starting so that the renderer can be created in
/// between, for the GPU this device belongs to.
pub struct OpenedSession {
    session: LibSeatSession,
    notifier: LibSeatSessionNotifier,
    device: DrmDevice,
    device_path: PathBuf,
}

fn session_error(context: &str, error: impl std::fmt::Display) -> BackendError {
    BackendError::Session(format!("{context}: {error}"))
}

/// Opens the libseat session and, through it, the DRM device.
///
/// `device` is the path of a DRM primary node (`/dev/dri/card0`); `None`
/// takes the seat's primary GPU. The process must be in a session on the
/// foreground virtual terminal.
pub fn open(device: Option<&Path>) -> Result<OpenedSession, BackendError> {
    let (mut session, notifier) =
        LibSeatSession::new().map_err(|e| session_error("opening the libseat session", e))?;

    // The seat may be enabled a moment after it is opened. Run the notifier
    // on a loop of its own until then; it moves to the real loop later.
    let (notifier, active) = wait_until_active(notifier, &session)?;
    if !active {
        return Err(BackendError::Session(format!(
            "the seat {} was not enabled within {} s; is this process in a session on the \
             foreground virtual terminal?",
            session.seat(),
            SEAT_TIMEOUT.as_secs()
        )));
    }

    let device_path = match device {
        Some(path) => path.to_path_buf(),
        None => primary_gpu(session.seat())
            .map_err(|e| session_error("looking for the primary GPU", e))?
            .ok_or_else(|| BackendError::Session("udev reports no GPU for the seat".to_string()))?,
    };
    let fd = session
        .open(
            &device_path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|e| session_error(&format!("opening {}", device_path.display()), e))?;
    let mut device = DrmDevice::new(fd)
        .map_err(|e| BackendError::Device(format!("{}: {e}", device_path.display())))?;
    device.set_probe_connectors(true);
    tracing::info!(
        seat = %session.seat(),
        device = %device_path.display(),
        device_number = ?device.device_number(),
        "session opened"
    );
    Ok(OpenedSession {
        session,
        notifier,
        device,
        device_path,
    })
}

fn wait_until_active(
    notifier: LibSeatSessionNotifier,
    session: &LibSeatSession,
) -> Result<(LibSeatSessionNotifier, bool), BackendError> {
    if session.is_active() {
        return Ok((notifier, true));
    }
    let mut event_loop: EventLoop<()> =
        EventLoop::try_new().map_err(|e| BackendError::EventLoop(e.to_string()))?;
    let dispatcher = Dispatcher::new(notifier, |_, _, _: &mut ()| {});
    let token = event_loop
        .handle()
        .register_dispatcher(dispatcher.clone())
        .map_err(|e| BackendError::EventLoop(e.to_string()))?;
    let deadline = Instant::now() + SEAT_TIMEOUT;
    while !session.is_active() && Instant::now() < deadline {
        event_loop
            .dispatch(Duration::from_millis(100), &mut ())
            .map_err(|e| BackendError::EventLoop(e.to_string()))?;
    }
    event_loop.handle().remove(token);
    drop(event_loop);
    let notifier = Dispatcher::into_source_inner(dispatcher);
    Ok((notifier, session.is_active()))
}

impl OpenedSession {
    /// Device number `(major, minor)` of the DRM device, to pick the Vulkan
    /// device that belongs to it.
    pub fn device_number(&self) -> Option<(u32, u32)> {
        self.device.device_number()
    }

    pub fn device_path(&self) -> &Path {
        &self.device_path
    }

    pub fn seat(&self) -> String {
        self.session.seat()
    }

    /// Runs `f` with the DRM device, before the backend takes it.
    pub fn with_device<T>(&mut self, f: impl FnOnce(&mut DrmDevice) -> T) -> T {
        f(&mut self.device)
    }

    /// Creates the backend and registers the session's event sources on the
    /// event loop.
    pub fn start<D: BackendHost + 'static>(
        self,
        handle: &LoopHandle<'static, D>,
        config: KmsConfig,
        renderer: Targets<'_>,
        renderer_is_kms_device: bool,
    ) -> Result<RealKmsBackend, BackendError> {
        let OpenedSession {
            session,
            notifier,
            device,
            device_path: _,
        } = self;
        let seat = session.seat();
        let loop_error = |e: &dyn std::fmt::Display| BackendError::EventLoop(e.to_string());

        // The DRM device is polled through a duplicate of its descriptor,
        // which the event source can own.
        let drm_fd = device
            .poll_fd()
            .expect("a real device has a descriptor")
            .try_clone_to_owned()
            .map_err(|e| BackendError::Device(format!("duplicating the DRM descriptor: {e}")))?;

        let backend = KmsBackend::new(
            device,
            MonotonicClock,
            config,
            renderer,
            renderer_is_kms_device,
        )?;

        // libinput, opening its devices through the session.
        let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        libinput
            .udev_assign_seat(&seat)
            .map_err(|()| BackendError::Session(format!("libinput refused seat {seat}")))?;

        // Session: pause and resume.
        {
            let backend = backend.clone();
            let mut libinput = libinput.clone();
            handle
                .insert_source(notifier, move |event, _, data: &mut D| {
                    match event {
                        SessionEvent::PauseSession => {
                            libinput.suspend();
                            backend.session_paused();
                        }
                        SessionEvent::ActivateSession => {
                            if libinput.resume().is_err() {
                                tracing::warn!("libinput could not be resumed");
                            }
                            backend.session_resumed();
                        }
                    }
                    data.backend_wake();
                })
                .map_err(|e| loop_error(&e))?;
        }

        // DRM: page flip events.
        {
            let backend = backend.clone();
            handle
                .insert_source(
                    Generic::new(drm_fd, Interest::READ, Mode::Level),
                    move |_, _, data: &mut D| {
                        if let Err(error) = backend.handle_device_events() {
                            tracing::warn!("reading DRM events failed: {error}");
                        }
                        data.backend_wake();
                        Ok(PostAction::Continue)
                    },
                )
                .map_err(|e| loop_error(&e))?;
        }

        // udev: displays plugged and unplugged show up as a change of the
        // DRM device.
        {
            let backend = backend.clone();
            let udev = UdevBackend::new(&seat).map_err(|e| session_error("starting udev", e))?;
            handle
                .insert_source(udev, move |event, _, data: &mut D| {
                    match event {
                        UdevEvent::Changed { .. } => backend.displays_changed(),
                        UdevEvent::Added { device_id, path } => {
                            tracing::info!(device_id, path = %path.display(), "GPU added (ignored)");
                        }
                        UdevEvent::Removed { device_id } => {
                            tracing::info!(device_id, "GPU removed (ignored)");
                        }
                    }
                    data.backend_wake();
                })
                .map_err(|e| loop_error(&e))?;
        }

        // libinput: input events, and the hotkeys the session acts on.
        {
            let backend = backend.clone();
            let mut session = session.clone();
            handle
                .insert_source(
                    LibinputInputBackend::new(libinput),
                    move |event, _, data: &mut D| {
                        if let Some(raw) = raw_input(event) {
                            backend.push_input(raw);
                        }
                        for hotkey in backend.take_hotkeys() {
                            if let Hotkey::SwitchVt(vt) = hotkey {
                                tracing::info!(vt, "switching virtual terminal");
                                if let Err(error) = session.change_vt(vt as i32) {
                                    tracing::warn!("VT switch failed: {error}");
                                }
                            }
                        }
                        data.backend_wake();
                    },
                )
                .map_err(|e| loop_error(&e))?;
        }

        // Timer: flips that never complete.
        {
            let backend = backend.clone();
            handle
                .insert_source(
                    Timer::from_duration(TIMEOUT_CHECK_INTERVAL),
                    move |_, _, data: &mut D| {
                        backend.check_timeouts();
                        data.backend_wake();
                        TimeoutAction::ToDuration(TIMEOUT_CHECK_INTERVAL)
                    },
                )
                .map_err(|e| loop_error(&e))?;
        }

        Ok(backend)
    }
}

/// Reduces a libinput event to what the backend uses. Everything that
/// needs thought happens after this, in `InputTranslator`.
fn raw_input(event: InputEvent<LibinputInputBackend>) -> Option<RawInput> {
    // Absolute positions are normalized to 0..1 by asking libinput to scale
    // them to a known range.
    const RANGE: i32 = 1 << 16;
    match event {
        InputEvent::Keyboard { event } => Some(RawInput::Key {
            // xkb key codes are evdev codes plus 8.
            code: event.key_code().raw().saturating_sub(8),
            pressed: event.state() == KeyState::Pressed,
            time_us: event.time().micros(),
        }),
        InputEvent::PointerMotion { event } => Some(RawInput::PointerMotion {
            dx: event.delta_x(),
            dy: event.delta_y(),
            time_us: event.time().micros(),
        }),
        InputEvent::PointerMotionAbsolute { event } => Some(RawInput::PointerMotionAbsolute {
            x: event.x_transformed(RANGE) / RANGE as f64,
            y: event.y_transformed(RANGE) / RANGE as f64,
            time_us: event.time().micros(),
        }),
        InputEvent::PointerButton { event } => Some(RawInput::PointerButton {
            button: event.button_code(),
            pressed: event.state() == ButtonState::Pressed,
            time_us: event.time().micros(),
        }),
        InputEvent::PointerAxis { event } => {
            let source = match event.source() {
                SmithayAxisSource::Wheel | SmithayAxisSource::WheelTilt => AxisSource::Wheel,
                SmithayAxisSource::Finger => AxisSource::Finger,
                SmithayAxisSource::Continuous => AxisSource::Continuous,
            };
            let v120 = (
                event.amount_v120(Axis::Horizontal),
                event.amount_v120(Axis::Vertical),
            );
            // A wheel click is 15 units of scroll distance; libinput may
            // report only the high resolution value.
            let amount = |axis, v120: Option<f64>| {
                event
                    .amount(axis)
                    .or_else(|| v120.map(|v| v / 120.0 * 15.0))
                    .unwrap_or(0.0)
            };
            Some(RawInput::PointerScroll {
                source,
                horizontal: amount(Axis::Horizontal, v120.0),
                vertical: amount(Axis::Vertical, v120.1),
                v120: (v120.0.is_some() || v120.1.is_some()).then(|| {
                    (
                        v120.0.unwrap_or(0.0).round() as i32,
                        v120.1.unwrap_or(0.0).round() as i32,
                    )
                }),
                time_us: event.time().micros(),
            })
        }
        _ => None,
    }
}
