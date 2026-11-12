//! Backends: where frames go and where input comes from.
//!
//! The compositor core only sees the [`Backend`] trait and the
//! [`BackendEvent`]s a backend produces. The shape follows what a DRM/KMS
//! session needs, because that backend comes right after the headless one:
//!
//! - **Outputs come and go.** A backend announces them with
//!   [`BackendEvent::OutputAdded`] / [`BackendEvent::OutputRemoved`], also
//!   the ones present at startup.
//! - **Frames are paced by the output.** The core asks for a frame with
//!   [`Backend::request_frame`]; the backend answers with
//!   [`BackendEvent::Frame`] when the output can take one (after the pending
//!   page flip on KMS, on the virtual frame clock when headless). The core
//!   then renders into [`Backend::render_target`] and calls
//!   [`Backend::present`]. [`BackendEvent::Presented`] arrives when the
//!   frame actually reached the output.
//! - **The session can be taken away** (VT switch):
//!   [`BackendEvent::SessionPaused`] / [`BackendEvent::SessionResumed`].
//!   While paused the core must not render or present.
//! - **Input** arrives as [`InputEvent`]s in one shape for all backends,
//!   modelled on what libinput reports.
//!
//! Events are collected in the backend and taken with
//! [`Backend::drain_events`]. A backend that is driven by file descriptors
//! or timers registers them on the caller's calloop event loop and calls
//! [`BackendHost::backend_wake`] when new events are waiting.
//!
//! There are two backends: [`headless`] (virtual outputs, nothing shown)
//! and [`kms`] (a DRM/KMS session). The KMS backend is split so that
//! everything except the thin device and session layers runs without
//! hardware; see its module documentation.

pub mod headless;
pub mod kms;
pub mod target;

use std::time::Duration;

use proto_color::ImageDescription;
pub use target::{
    DRM_FORMAT_XRGB8888, DmabufPlane, OutputFormat, TargetDesc, TargetError, TargetId,
    TargetProvider, Targets,
};

/// Identifies an output of a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u64);

/// Errors of a backend.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("render target error: {0}")]
    Target(#[from] TargetError),
    #[error("unknown output")]
    UnknownOutput,
    #[error("invalid output configuration: {0}")]
    InvalidConfig(String),
    #[error("the session is not active")]
    SessionInactive,
    #[error("event loop error: {0}")]
    EventLoop(String),
    #[error("device error: {0}")]
    Device(String),
    #[error("session error: {0}")]
    Session(String),
}

/// An output as the compositor core sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    pub id: OutputId,
    pub name: String,
    pub make: String,
    pub model: String,
    /// Size in physical pixels.
    pub size: (u32, u32),
    /// Factor between logical coordinates and physical pixels.
    pub scale: f64,
    /// Refresh rate in mHz.
    pub refresh_mhz: u32,
    /// Position of the top left corner in the global logical space.
    pub position: (i32, i32),
    /// The image description the output is currently driven with: what the
    /// encoded frame means.
    pub image_description: ImageDescription,
    /// Format of the encoded frame.
    pub format: OutputFormat,
    /// Image descriptions the output could be driven with (its color
    /// capabilities). The current one is among them.
    pub supported_image_descriptions: Vec<ImageDescription>,
}

impl OutputInfo {
    /// Size in logical coordinates.
    pub fn logical_size(&self) -> (i32, i32) {
        (
            (self.size.0 as f64 / self.scale).round() as i32,
            (self.size.1 as f64 / self.scale).round() as i32,
        )
    }

    /// Time between two frames.
    pub fn frame_interval(&self) -> Duration {
        Duration::from_nanos(1_000_000_000_000 / self.refresh_mhz.max(1) as u64)
    }
}

/// What produced a scroll event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisSource {
    /// A wheel with detents. `v120` carries the steps.
    Wheel,
    /// A finger on a touchpad.
    Finger,
    /// Something continuous without a natural end (e.g. a trackpoint).
    Continuous,
}

/// An input event in the shape shared by all backends.
///
/// Key and button codes are Linux evdev codes. Times are in microseconds on
/// a monotonic clock with an arbitrary base.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    Key {
        keycode: u32,
        pressed: bool,
        time_us: u64,
    },
    /// Relative pointer motion in logical pixels, as a mouse reports it.
    /// `dx` and `dy` are after pointer acceleration; `dx_unaccel` and
    /// `dy_unaccel` before it (what games turn into a view rotation).
    PointerMotion {
        dx: f64,
        dy: f64,
        dx_unaccel: f64,
        dy_unaccel: f64,
        time_us: u64,
    },
    /// Absolute pointer position in the global logical coordinate space
    /// (tablets, virtual pointers, injected input).
    PointerMotionAbsolute {
        x: f64,
        y: f64,
        time_us: u64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
        time_us: u64,
    },
    /// Scroll. Positive values scroll down or right. `v120` carries the high
    /// resolution wheel steps (120 per detent) when the source is a wheel.
    PointerAxis {
        source: AxisSource,
        horizontal: f64,
        vertical: f64,
        v120: Option<(i32, i32)>,
        time_us: u64,
    },
    /// Whether a touchscreen is there. Sent when the first one appears and
    /// when the last one goes; a backend whose input can always include
    /// touches (the headless one, through its control interface) sends
    /// `true` at the start.
    TouchAvailable(bool),
    /// A finger touched the screen at a position of the global logical
    /// space. `slot` names the finger until its `TouchUp`; slots are unique
    /// across the touchscreens of the seat.
    TouchDown {
        slot: u32,
        x: f64,
        y: f64,
        time_us: u64,
    },
    TouchMotion {
        slot: u32,
        x: f64,
        y: f64,
        time_us: u64,
    },
    TouchUp {
        slot: u32,
        time_us: u64,
    },
    /// The end of a set of touch events that belong together (fingers that
    /// went down or moved at the same time).
    TouchFrame {
        time_us: u64,
    },
    /// Every touch in progress is void: the device decided they were not
    /// meant (a palm), or the session went away. No `TouchUp` follows.
    TouchCancel {
        time_us: u64,
    },
    /// A touchpad gesture started with this many fingers.
    GestureBegin {
        kind: GestureKind,
        fingers: u32,
        time_us: u64,
    },
    /// The gesture in progress moved. `dx`, `dy` is the motion of the
    /// fingers' center in logical pixels (after acceleration). A pinch also
    /// has `scale`, relative to the start (1.0 at the start), and
    /// `rotation`, in degrees clockwise since the previous update. A hold
    /// has no updates.
    GestureUpdate {
        kind: GestureKind,
        dx: f64,
        dy: f64,
        scale: f64,
        rotation: f64,
        time_us: u64,
    },
    /// The gesture in progress ended. `cancelled`: the fingers did not
    /// complete it (a hold that turned into a swipe, a finger added).
    GestureEnd {
        kind: GestureKind,
        cancelled: bool,
        time_us: u64,
    },
}

/// The touchpad gestures libinput recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GestureKind {
    /// Several fingers moving the same way.
    Swipe,
    /// Fingers moving apart or together, or turning.
    Pinch,
    /// Fingers resting on the touchpad without moving.
    Hold,
}

impl GestureKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Swipe => "swipe",
            Self::Pinch => "pinch",
            Self::Hold => "hold",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "swipe" => Some(Self::Swipe),
            "pinch" => Some(Self::Pinch),
            "hold" => Some(Self::Hold),
            _ => None,
        }
    }
}

/// When a frame reached the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// Presentation time on `CLOCK_MONOTONIC`.
    pub time: Duration,
    /// Time between two frames.
    pub refresh: Duration,
    /// Frame counter of the output.
    pub sequence: u64,
}

/// Something a backend reports to the compositor core.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    /// An output appeared. Also sent for the outputs present at startup.
    OutputAdded(OutputInfo),
    /// Properties of an output changed (mode, scale, image description).
    OutputChanged(OutputInfo),
    /// An output is gone. Its render target is no longer valid.
    OutputRemoved(OutputId),
    /// The output can take a new frame. Sent in answer to
    /// [`Backend::request_frame`].
    Frame {
        output: OutputId,
    },
    /// The frame given to [`Backend::present`] reached the output.
    Presented {
        output: OutputId,
        presentation: Presentation,
    },
    Input(InputEvent),
    /// The session lost its devices (VT switch away). Do not render.
    SessionPaused,
    /// The session has its devices again. Everything needs a redraw.
    SessionResumed,
    /// The set of displays may have changed. The core answers by calling
    /// [`Backend::reconfigure`] with the targets provider, which the backend needs
    /// to create or destroy render targets.
    Reconfigure,
    /// The user asked the compositor to stop with the emergency key
    /// combination, which the backend handles before anything else.
    QuitRequested,
}

/// Implemented by the loop data of the event loop a backend registers its
/// sources on.
pub trait BackendHost {
    /// New events are waiting in [`Backend::drain_events`].
    fn backend_wake(&mut self);
}

/// The interface between the compositor core and a backend.
///
/// Registering event sources is not part of the trait: it is generic over
/// the loop data, and each backend offers it as an inherent method (see
/// [`headless::HeadlessBackend::insert_sources`]).
pub trait Backend {
    /// Short name, e.g. `headless`.
    fn name(&self) -> &'static str;

    /// The outputs that currently exist.
    fn outputs(&self) -> Vec<OutputInfo>;

    /// Whether the session is active. While it is not, rendering and
    /// presenting are not possible.
    fn is_active(&self) -> bool;

    /// The render target for the next frame of an output. Ask again for
    /// every frame: a backend that flips between buffers hands out a
    /// different one each time. `None` when the backend runs without a
    /// target provider, the output is unknown, or the session is paused.
    fn render_target(&mut self, output: OutputId) -> Option<TargetId>;

    /// Asks for a [`BackendEvent::Frame`] as soon as the output can take a
    /// new frame. Asking again before it arrives has no further effect.
    fn request_frame(&mut self, output: OutputId);

    /// Shows the frame last rendered into the output's render target.
    /// [`BackendEvent::Presented`] follows when it reached the output.
    fn present(&mut self, targets: Targets<'_>, output: OutputId) -> Result<(), BackendError>;

    /// Brings the outputs in line with the displays that are there now,
    /// creating and destroying render targets as needed. Called by the core
    /// in answer to [`BackendEvent::Reconfigure`]. Resulting changes are
    /// reported as `OutputAdded` / `OutputRemoved` events.
    fn reconfigure(&mut self, _targets: Targets<'_>) -> Result<(), BackendError> {
        Ok(())
    }

    /// Releases the outputs for a clean exit: turns them off where that is
    /// the backend's job and frees their render targets.
    fn shutdown(&mut self, _targets: Targets<'_>) {}

    /// Drives an output with another image description, reported with
    /// [`BackendEvent::OutputChanged`]. For tests; a backend that cannot
    /// refuses.
    fn set_output_image_description(
        &mut self,
        _output: OutputId,
        _description: ImageDescription,
    ) -> Result<(), BackendError> {
        Err(BackendError::InvalidConfig(format!(
            "the {} backend cannot change the image description of an output",
            self.name()
        )))
    }

    /// Describes the backend's own state for state dumps and diagnostics.
    fn describe(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    /// When a frame handed to [`Backend::present`] now is expected to reach
    /// the output, on the clock of its [`Presentation`] times. `None` when
    /// the backend cannot tell (no frame has been shown yet).
    fn next_presentation(&self, _output: OutputId) -> Option<Duration> {
        None
    }

    /// Takes the queued events, oldest first.
    fn drain_events(&mut self) -> Vec<BackendEvent>;
}

/// The clock frames are timed with: what [`Presentation`] times and the
/// times scripts see are read from.
///
/// `CLOCK_MONOTONIC` normally. For tests, a clock that stands still until it
/// is moved ([`kms::clock::ManualClock`]), so that whatever is computed from
/// the time (an animation) comes out the same on every run.
#[derive(Debug, Clone, Default)]
pub enum TimeSource {
    #[default]
    Monotonic,
    Manual(kms::clock::ManualClock),
}

impl TimeSource {
    /// A manual clock that starts at the current monotonic time.
    pub fn manual() -> Self {
        TimeSource::Manual(kms::clock::ManualClock::new(monotonic_now()))
    }

    pub fn now(&self) -> Duration {
        match self {
            TimeSource::Monotonic => monotonic_now(),
            TimeSource::Manual(clock) => kms::clock::Clock::now(clock),
        }
    }

    pub fn is_manual(&self) -> bool {
        matches!(self, TimeSource::Manual(_))
    }

    /// Moves a manual clock forward. Returns the new time, or `None` for the
    /// monotonic clock, which nobody moves.
    pub fn advance(&self, by: Duration) -> Option<Duration> {
        match self {
            TimeSource::Monotonic => None,
            TimeSource::Manual(clock) => {
                clock.advance(by);
                Some(kms::clock::Clock::now(clock))
            }
        }
    }
}

impl kms::clock::Clock for TimeSource {
    fn now(&self) -> Duration {
        TimeSource::now(self)
    }
}

/// The current time on `CLOCK_MONOTONIC`.
pub fn monotonic_now() -> Duration {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}
