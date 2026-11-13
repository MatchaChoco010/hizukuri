//! The Wayland client side of the nested backend: the objects of the
//! parent compositor and what their events mean.
//!
//! Event handlers only change [`State`] and queue [`BackendEvent`]s. What
//! needs the target provider (new buffers after a resize) is done by the
//! backend when the core calls it.

use std::collections::VecDeque;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::time::Duration;

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface, wl_touch,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1, zwp_keyboard_shortcuts_inhibitor_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::LogoKeyChoice;
use super::shm::ShmMap;
use crate::target::{DRM_FORMAT_XRGB8888, TargetId};
use crate::{
    AxisSource, BackendEvent, InputEvent, LogoKey, OutputId, OutputInfo, Presentation,
    monotonic_now,
};

/// The modifier that stands for "whatever the driver picks": a buffer with
/// it cannot be described to a consumer that needs to know the layout.
pub(super) const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_RIGHTALT: u32 = 100;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;

fn is_modifier(key: u32) -> bool {
    matches!(
        key,
        KEY_LEFTCTRL
            | KEY_RIGHTCTRL
            | KEY_LEFTSHIFT
            | KEY_RIGHTSHIFT
            | KEY_LEFTALT
            | KEY_RIGHTALT
            | KEY_LEFTMETA
            | KEY_RIGHTMETA
    )
}

/// An output of the parent, as far as the nested window cares.
pub(super) struct ParentOutput {
    pub global: u32,
    pub proxy: wl_output::WlOutput,
    pub scale: i32,
    pub refresh_mhz: i32,
}

/// What a buffer shown to the parent is backed by.
pub(super) enum BufferKind {
    /// Shared memory the frame is copied into.
    Shm(ShmMap),
    /// A render target exported as a dma-buf: the frame is drawn into it.
    Dmabuf(TargetId),
}

pub(super) struct Buffer {
    pub wl: wl_buffer::WlBuffer,
    pub kind: BufferKind,
    pub size: (u32, u32),
    /// The parent may still read it.
    pub busy: bool,
}

/// The output of the nested backend: the window, seen as a display.
pub(super) struct Output {
    pub info: OutputInfo,
    /// Size of the window in the parent's logical coordinates.
    pub logical: (u32, u32),
    /// The target frames are drawn into when they are copied to shared
    /// memory.
    pub offscreen: Option<TargetId>,
    pub dmabuf: bool,
    pub buffers: Vec<Buffer>,
    /// The dma-buf buffer the next frame is drawn into.
    pub back: Option<usize>,
    /// The target that holds the last frame handed to the parent.
    pub shown: Option<TargetId>,
    pub sequence: u64,
    pub last_presented: Option<Duration>,
    /// Refresh interval the parent reported with a presented frame.
    pub refresh: Option<Duration>,
    pub frames_presented: u64,
}

/// The default dma-buf feedback while it arrives.
#[derive(Default)]
struct Feedback {
    table: Vec<(u32, u64)>,
    formats: Vec<(u32, u64)>,
    main_device: Option<(u32, u32)>,
}

pub(super) struct State {
    pub qh: QueueHandle<State>,
    pub logo_choice: LogoKeyChoice,
    /// The key that is the logo key now, once announced to the core.
    pub logo_key: Option<LogoKey>,
    hide_parent_cursor: bool,
    cursor_shape_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    /// The lock asked of the parent for a lock in effect inside.
    locked_pointer: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
    /// The parent holds the pointer: its motion arrives as relative only.
    lock_active: bool,

    // Globals of the parent.
    pub compositor: Option<wl_compositor::WlCompositor>,
    pub wm_base: Option<xdg_wm_base::XdgWmBase>,
    pub shm: Option<wl_shm::WlShm>,
    pub seat: Option<wl_seat::WlSeat>,
    pub viewporter: Option<wp_viewporter::WpViewporter>,
    pub fractional_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    pub presentation: Option<wp_presentation::WpPresentation>,
    pub decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    pub inhibit_manager:
        Option<zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1>,
    pub dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    pub outputs: Vec<ParentOutput>,

    // The window.
    pub surface: Option<wl_surface::WlSurface>,
    pub xdg_surface: Option<xdg_surface::XdgSurface>,
    pub toplevel: Option<xdg_toplevel::XdgToplevel>,
    pub viewport: Option<wp_viewport::WpViewport>,
    pub inhibitor: Option<zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1>,
    pub want_inhibit: bool,
    pub shortcuts_inhibited: bool,
    /// The parent sent a keymap, which was passed on to the core.
    pub parent_keymap: bool,
    /// The globals the parent advertises: (name, interface, version).
    pub parent_globals: Vec<(u32, String, u32)>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    touch: Option<wl_touch::WlTouch>,

    /// The first `xdg_surface.configure` arrived.
    pub configured: bool,
    /// Size from the last `xdg_toplevel.configure`, to take effect with the
    /// `xdg_surface.configure` that follows. Zero: up to the client.
    toplevel_size: (i32, i32),
    /// Size the window should have, in the parent's logical coordinates.
    pub wanted_logical: (u32, u32),
    /// `wp_fractional_scale_v1.preferred_scale`, in 120ths.
    scale_120: Option<u32>,
    /// `wl_surface.preferred_buffer_scale`.
    preferred_buffer_scale: Option<i32>,
    entered: Vec<wl_output::WlOutput>,

    // dma-buf.
    feedback: Feedback,
    pub feedback_done: bool,
    /// (format, modifier) pairs the parent takes.
    pub parent_formats: Vec<(u32, u64)>,
    /// The device the parent composites with (major, minor).
    pub main_device: Option<(u32, u32)>,
    /// The answer to the `zwp_linux_buffer_params_v1.create` in flight:
    /// `Some(None)` when it failed.
    pub params_result: Option<Option<wl_buffer::WlBuffer>>,

    // Frames.
    pub events: VecDeque<BackendEvent>,
    pub output: Option<Output>,
    /// Buffers of a size the window no longer has, which the parent still
    /// holds.
    pub retired: Vec<Buffer>,
    /// Targets of retired buffers the parent let go of, to be destroyed
    /// when the target provider is at hand.
    pub garbage: Vec<TargetId>,
    pub frame_requested: bool,
    callback_serial: u64,
    /// The frame callback the next frame waits for.
    pub callback_pending: Option<u64>,
    /// The window changed size or scale and the buffers do not match yet.
    pub resize_pending: bool,
    /// Clock of `wp_presentation` times.
    presentation_clock: Option<u32>,
    pub closed: bool,
    /// The connection to the parent is gone.
    pub lost: bool,

    // Input.
    /// Keys held, as (key of the parent, key the core was given): a key is
    /// let go of as what it was pressed as, also when the logo key changed
    /// in between.
    pressed_keys: Vec<(u32, u32)>,
    last_time_ms: u32,
    axis: AxisFrame,
    touch_announced: bool,
    /// The parent's text input (see `super::text_input`).
    pub(super) text_input: super::text_input::ParentTextInput,

    /// The parent's clipboard and primary selection.
    pub selections: super::selection::Selections,
}

/// The scroll events of one `wl_pointer.frame`.
#[derive(Default)]
struct AxisFrame {
    horizontal: f64,
    vertical: f64,
    v120: (i32, i32),
    has_v120: bool,
    source: Option<AxisSource>,
    time_ms: u32,
    any: bool,
}

impl State {
    pub fn new(
        qh: QueueHandle<State>,
        logo_choice: LogoKeyChoice,
        want_inhibit: bool,
        hide_parent_cursor: bool,
    ) -> Self {
        Self {
            qh,
            logo_choice,
            logo_key: None,
            hide_parent_cursor,
            cursor_shape_manager: None,
            relative_manager: None,
            relative_pointer: None,
            constraints: None,
            locked_pointer: None,
            lock_active: false,
            compositor: None,
            wm_base: None,
            shm: None,
            seat: None,
            viewporter: None,
            fractional_manager: None,
            presentation: None,
            decoration_manager: None,
            inhibit_manager: None,
            dmabuf: None,
            outputs: Vec::new(),
            surface: None,
            xdg_surface: None,
            toplevel: None,
            viewport: None,
            inhibitor: None,
            want_inhibit,
            shortcuts_inhibited: false,
            parent_keymap: false,
            parent_globals: Vec::new(),
            keyboard: None,
            pointer: None,
            touch: None,
            configured: false,
            toplevel_size: (0, 0),
            wanted_logical: (0, 0),
            scale_120: None,
            preferred_buffer_scale: None,
            entered: Vec::new(),
            feedback: Feedback::default(),
            feedback_done: false,
            parent_formats: Vec::new(),
            main_device: None,
            params_result: None,
            events: VecDeque::new(),
            output: None,
            retired: Vec::new(),
            garbage: Vec::new(),
            frame_requested: false,
            callback_serial: 0,
            callback_pending: None,
            resize_pending: false,
            presentation_clock: None,
            closed: false,
            lost: false,
            pressed_keys: Vec::new(),
            last_time_ms: 0,
            axis: AxisFrame::default(),
            touch_announced: false,
            text_input: Default::default(),
            selections: Default::default(),
        }
    }

    /// The scale the parent shows the window at.
    pub fn scale(&self) -> f64 {
        if self.viewport.is_some()
            && let Some(scale) = self.scale_120
        {
            return f64::from(scale) / 120.0;
        }
        f64::from(self.integer_scale())
    }

    fn integer_scale(&self) -> i32 {
        self.preferred_buffer_scale
            .or_else(|| {
                self.entered
                    .iter()
                    .filter_map(|output| self.outputs.iter().find(|o| &o.proxy == output))
                    .map(|o| o.scale)
                    .max()
            })
            .unwrap_or(1)
            .max(1)
    }

    /// The size of the buffers for the window as it should be now.
    pub fn wanted_physical(&self) -> (u32, u32) {
        let scale = self.scale();
        let size = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
        (size(self.wanted_logical.0), size(self.wanted_logical.1))
    }

    /// Refresh rate of the parent's output the window is on, in mHz.
    pub fn parent_refresh_mhz(&self) -> Option<u32> {
        self.entered
            .iter()
            .filter_map(|output| self.outputs.iter().find(|o| &o.proxy == output))
            .map(|o| o.refresh_mhz)
            .filter(|r| *r > 0)
            .max()
            .map(|r| r as u32)
    }

    /// The window's size or scale may have changed: asks the core for a
    /// reconfiguration when the buffers no longer fit.
    fn geometry_changed(&mut self) {
        let Some(output) = &self.output else {
            return;
        };
        let fits = output.info.size == self.wanted_physical()
            && output.logical == self.wanted_logical
            && output.info.scale == self.scale();
        if fits || self.resize_pending {
            return;
        }
        self.resize_pending = true;
        // A frame granted for the old size would be drawn into the new
        // buffers before the core knows the new size.
        let before = self.events.len();
        self.events
            .retain(|event| !matches!(event, BackendEvent::Frame { .. }));
        if self.events.len() != before {
            self.frame_requested = true;
        }
        self.events.push_back(BackendEvent::Reconfigure);
    }

    /// The refresh rate of the parent's output may have changed.
    fn refresh_changed(&mut self) {
        let Some(refresh) = self.parent_refresh_mhz() else {
            return;
        };
        if let Some(output) = &mut self.output
            && output.info.refresh_mhz != refresh
        {
            output.info.refresh_mhz = refresh;
            self.events
                .push_back(BackendEvent::OutputChanged(output.info.clone()));
        }
    }

    /// Whether a buffer is free for a frame.
    fn can_draw(&self) -> bool {
        match &self.output {
            Some(output) if output.dmabuf => output.buffers.iter().any(|b| !b.busy),
            Some(_) => true,
            None => false,
        }
    }

    /// Grants the requested frame when the parent is ready for one.
    pub fn try_grant(&mut self) {
        if !self.frame_requested
            || self.callback_pending.is_some()
            || self.resize_pending
            || self.lost
            || !self.can_draw()
        {
            return;
        }
        let Some(output) = &self.output else {
            return;
        };
        self.frame_requested = false;
        self.events.push_back(BackendEvent::Frame {
            output: output.info.id,
        });
    }

    /// The next serial for a frame callback and its presentation feedback.
    pub fn next_callback(&mut self) -> u64 {
        self.callback_serial += 1;
        self.callback_serial
    }

    fn presented(&mut self, time: Option<Duration>, refresh: Option<Duration>) {
        let Some(output) = &mut self.output else {
            return;
        };
        let time = time.unwrap_or_else(monotonic_now);
        output.sequence += 1;
        output.frames_presented += 1;
        output.last_presented = Some(time);
        if refresh.is_some() {
            output.refresh = refresh;
        }
        let presentation = Presentation {
            time,
            refresh: output.refresh.unwrap_or(output.info.frame_interval()),
            sequence: output.sequence,
        };
        self.events.push_back(BackendEvent::Presented {
            output: output.info.id,
            presentation,
        });
    }

    fn input(&mut self, event: InputEvent) {
        self.events.push_back(BackendEvent::Input(event));
    }

    fn time_us(&mut self, time_ms: u32) -> u64 {
        self.last_time_ms = time_ms;
        u64::from(time_ms) * 1000
    }

    /// The key that is the logo key now.
    pub fn current_logo_key(&self) -> LogoKey {
        self.logo_choice.resolve(self.shortcuts_inhibited)
    }

    /// Tells the core which key is the logo key, when that is news.
    pub fn announce_logo_key(&mut self) {
        let key = self.current_logo_key();
        if self.logo_key == Some(key) {
            return;
        }
        tracing::info!(
            "the logo key inside is the parent's {} ({})",
            key.name(),
            self.logo_choice.name()
        );
        self.logo_key = Some(key);
        self.events.push_back(BackendEvent::LogoKey(key));
    }

    /// The key code the core sees for a key of the parent.
    fn map_key(&self, key: u32) -> u32 {
        match (self.current_logo_key(), key) {
            (LogoKey::Alt, KEY_LEFTALT) | (LogoKey::Ctrl, KEY_LEFTCTRL) => KEY_LEFTMETA,
            (LogoKey::Alt, KEY_RIGHTALT) | (LogoKey::Ctrl, KEY_RIGHTCTRL) => KEY_RIGHTMETA,
            _ => key,
        }
    }

    fn key(&mut self, key: u32, pressed: bool, time_ms: u32) {
        if pressed {
            if self.pressed_keys.iter().any(|(parent, _)| *parent == key) {
                // A repeat.
                return;
            }
            let keycode = self.map_key(key);
            // Two keys of the parent may be one key inside (Alt standing
            // in for the logo key, and the logo key itself): the core
            // hears of the first press and the last release.
            let held = self.pressed_keys.iter().any(|(_, core)| *core == keycode);
            self.pressed_keys.push((key, keycode));
            if !held {
                let time_us = self.time_us(time_ms);
                self.input(InputEvent::Key {
                    keycode,
                    pressed: true,
                    time_us,
                });
            }
        } else {
            let Some(index) = self
                .pressed_keys
                .iter()
                .position(|(parent, _)| *parent == key)
            else {
                // The release of a key whose press was not ours.
                return;
            };
            let (_, keycode) = self.pressed_keys.remove(index);
            let time_us = self.time_us(time_ms);
            self.release(keycode, time_us);
        }
    }

    /// Tells the core a key it was given is no longer held, unless another
    /// key of the parent still holds it.
    fn release(&mut self, keycode: u32, time_us: u64) {
        if self.pressed_keys.iter().any(|(_, core)| *core == keycode) {
            return;
        }
        self.input(InputEvent::Key {
            keycode,
            pressed: false,
            time_us,
        });
    }

    /// Releases the modifier keys that are held.
    fn release_modifiers(&mut self) {
        let time_us = u64::from(self.last_time_ms) * 1000;
        let mut held = Vec::new();
        self.pressed_keys.retain(|&(parent, core)| {
            if is_modifier(parent) && !held.contains(&core) {
                held.push(core);
            }
            !is_modifier(parent)
        });
        for keycode in held {
            self.release(keycode, time_us);
        }
    }

    /// The window lost the keyboard: no release will come for what is held.
    fn release_keys(&mut self) {
        let time_us = u64::from(self.last_time_ms) * 1000;
        let mut held = Vec::new();
        for (_, core) in std::mem::take(&mut self.pressed_keys) {
            if !held.contains(&core) {
                held.push(core);
            }
        }
        for keycode in held {
            self.release(keycode, time_us);
        }
    }

    fn pointer_position(&mut self, x: f64, y: f64, time_ms: u32) {
        let time_us = self.time_us(time_ms);
        let origin = self
            .output
            .as_ref()
            .map_or((0, 0), |output| output.info.position);
        self.input(InputEvent::PointerMotionAbsolute {
            x: f64::from(origin.0) + x,
            y: f64::from(origin.1) + y,
            time_us,
        });
    }

    fn flush_axis(&mut self) {
        let axis = std::mem::take(&mut self.axis);
        if !axis.any {
            return;
        }
        let source = axis.source.unwrap_or(if axis.has_v120 {
            AxisSource::Wheel
        } else {
            AxisSource::Continuous
        });
        let time_us = self.time_us(axis.time_ms);
        self.input(InputEvent::PointerAxis {
            source,
            horizontal: axis.horizontal,
            vertical: axis.vertical,
            v120: axis.has_v120.then_some(axis.v120),
            time_us,
        });
    }

    /// Whether the parent holds the pointer for a lock inside.
    pub fn pointer_locked(&self) -> bool {
        self.lock_active
    }

    /// Asks the parent to hold the pointer on the window, or to let it go.
    /// Without the protocols for it the pointer keeps moving outside while
    /// it stands still inside.
    pub fn set_pointer_lock(&mut self, locked: bool) {
        if !locked {
            if let Some(lock) = self.locked_pointer.take() {
                lock.destroy();
            }
            self.lock_active = false;
            return;
        }
        if self.locked_pointer.is_some() || self.relative_pointer.is_none() {
            return;
        }
        if let (Some(constraints), Some(surface), Some(pointer)) =
            (&self.constraints, &self.surface, &self.pointer)
        {
            self.locked_pointer = Some(constraints.lock_pointer(
                surface,
                pointer,
                None,
                zwp_pointer_constraints_v1::Lifetime::Persistent,
                &self.qh,
                (),
            ));
        }
    }

    /// Asks the parent to pass its own shortcuts to the window while it has
    /// the keyboard. Needs the window and the seat.
    pub fn inhibit_shortcuts(&mut self) {
        if !self.want_inhibit || self.inhibitor.is_some() {
            return;
        }
        if let (Some(manager), Some(surface), Some(seat)) =
            (&self.inhibit_manager, &self.surface, &self.seat)
        {
            self.inhibitor = Some(manager.inhibit_shortcuts(surface, seat, &self.qh, ()));
        }
    }

    fn release_buffer(&mut self, buffer: &wl_buffer::WlBuffer) {
        if let Some(output) = &mut self.output
            && let Some(found) = output.buffers.iter_mut().find(|b| &b.wl == buffer)
        {
            found.busy = false;
            self.try_grant();
            return;
        }
        if let Some(index) = self.retired.iter().position(|b| &b.wl == buffer) {
            let retired = self.retired.remove(index);
            retired.wl.destroy();
            if let BufferKind::Dmabuf(target) = retired.kind {
                self.garbage.push(target);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state
                    .parent_globals
                    .push((name, interface.clone(), version));
                state.bind_global(registry, name, &interface, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                state
                    .parent_globals
                    .retain(|(global, _, _)| *global != name);
                state.global_removed(name);
            }
            _ => {}
        }
    }
}

impl State {
    /// Binds a global of the parent this backend uses.
    fn bind_global(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        interface: &str,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        if self.bind_selection_global(registry, name, interface, version, qh) {
            return;
        }
        let state = self;
        match interface {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(6), qh, ()));
            }
            "xdg_wm_base" => {
                state.wm_base = Some(registry.bind(name, version.min(5), qh, ()));
            }
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, version.min(8), qh, ()));
                state.ensure_text_input();
                state.ensure_selection_devices(qh);
            }
            "zwp_text_input_manager_v3" => {
                state.text_input.manager = Some(registry.bind(name, 1, qh, ()));
                state.ensure_text_input();
            }
            "wl_output" => {
                let proxy = registry.bind(name, version.min(4), qh, ());
                state.outputs.push(ParentOutput {
                    global: name,
                    proxy,
                    scale: 1,
                    refresh_mhz: 0,
                });
            }
            "wp_viewporter" => state.viewporter = Some(registry.bind(name, 1, qh, ())),
            "wp_fractional_scale_manager_v1" => {
                state.fractional_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "wp_presentation" => {
                state.presentation = Some(registry.bind(name, 1, qh, ()));
            }
            "zwp_relative_pointer_manager_v1" => {
                state.relative_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "zwp_pointer_constraints_v1" => {
                state.constraints = Some(registry.bind(name, 1, qh, ()));
            }
            "wp_cursor_shape_manager_v1" => {
                state.cursor_shape_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "zxdg_decoration_manager_v1" => {
                state.decoration_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                state.inhibit_manager = Some(registry.bind(name, 1, qh, ()));
            }
            // Version 3 lists what the parent takes with events on the
            // global, version 4 with a feedback object.
            "zwp_linux_dmabuf_v1" if version >= 3 => {
                let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 =
                    registry.bind(name, version.min(4), qh, ());
                if version >= 4 {
                    dmabuf.get_default_feedback(qh, ());
                } else {
                    state.feedback_done = true;
                }
                state.dmabuf = Some(dmabuf);
            }
            _ => {}
        }
    }

    /// The parent removed a global.
    fn global_removed(&mut self, name: u32) {
        if let Some(index) = self.outputs.iter().position(|o| o.global == name) {
            let output = self.outputs.remove(index);
            self.entered.retain(|o| o != &output.proxy);
            self.geometry_changed();
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(State: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
delegate_noop!(State: ignore zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
delegate_noop!(State: ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(State: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(State: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
delegate_noop!(State: ignore zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1);

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.iter_mut().find(|o| &o.proxy == proxy) else {
            return;
        };
        match event {
            wl_output::Event::Mode { flags, refresh, .. } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    output.refresh_mhz = refresh;
                }
            }
            wl_output::Event::Scale { factor } => output.scale = factor,
            wl_output::Event::Done => {
                state.geometry_changed();
                state.refresh_changed();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

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

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { output } => {
                if !state.entered.contains(&output) {
                    state.entered.push(output);
                }
            }
            wl_surface::Event::Leave { output } => state.entered.retain(|o| o != &output),
            wl_surface::Event::PreferredBufferScale { factor } => {
                state.preferred_buffer_scale = Some(factor);
            }
            _ => return,
        }
        state.geometry_changed();
        state.refresh_changed();
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
            state.scale_120 = Some(scale.max(1));
            state.geometry_changed();
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
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.toplevel_size = (width, height);
            }
            xdg_toplevel::Event::Close => {
                tracing::info!("the parent closed the window");
                state.closed = true;
                state.events.push_back(BackendEvent::QuitRequested);
            }
            _ => {}
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
            state.configured = true;
            // Zero leaves that dimension to the window.
            let (width, height) = state.toplevel_size;
            if width > 0 {
                state.wanted_logical.0 = width as u32;
            }
            if height > 0 {
                state.wanted_logical.1 = height as u32;
            }
            state.geometry_changed();
        }
    }
}

impl Dispatch<zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                tracing::info!("the parent passes its shortcuts to the window");
                state.shortcuts_inhibited = true;
                state.announce_logo_key();
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                tracing::info!("the parent handles its shortcuts itself again");
                state.shortcuts_inhibited = false;
                state.announce_logo_key();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

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
            state.release_buffer(buffer);
        }
    }
}

/// The frame callback of the commit with this serial.
impl Dispatch<wl_callback::WlCallback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        serial: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if state.presentation.is_none() {
                // Nothing tells when the frame was shown; the parent asking
                // for the next one is the closest there is.
                state.presented(None, None);
            }
            if state.callback_pending == Some(*serial) {
                state.callback_pending = None;
                state.try_grant();
            }
        }
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.presentation_clock = Some(clk_id);
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        const CLOCK_MONOTONIC: u32 = 1;
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                ..
            } => {
                // Times of another clock cannot be compared with the core's.
                let time = (state.presentation_clock == Some(CLOCK_MONOTONIC)).then(|| {
                    Duration::new((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo), tv_nsec)
                });
                let refresh = (refresh > 0).then(|| Duration::from_nanos(u64::from(refresh)));
                state.presented(time, refresh);
            }
            // The frame was replaced or the window is not visible. Clients
            // of the core still wait for their frame callbacks.
            wp_presentation_feedback::Event::Discarded => state.presented(None, None),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// dma-buf
// ---------------------------------------------------------------------------

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
            state.parent_formats.push((
                format,
                (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
            ));
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
        let feedback = &mut state.feedback;
        match event {
            Event::MainDevice { device } => {
                if let Ok(bytes) = <[u8; 8]>::try_from(device.as_slice()) {
                    let device = u64::from_ne_bytes(bytes);
                    feedback.main_device =
                        Some((rustix::fs::major(device), rustix::fs::minor(device)));
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
                            (
                                u32::from_ne_bytes(entry[0..4].try_into().unwrap()),
                                u64::from_ne_bytes(entry[8..16].try_into().unwrap()),
                            )
                        })
                        .collect();
                } else {
                    tracing::warn!("the parent's dma-buf format table cannot be read");
                }
            }
            Event::TrancheFormats { indices } => {
                for index in indices.chunks_exact(2) {
                    let index = u16::from_ne_bytes([index[0], index[1]]) as usize;
                    if let Some(entry) = feedback.table.get(index)
                        && !feedback.formats.contains(entry)
                    {
                        feedback.formats.push(*entry);
                    }
                }
            }
            Event::Done => {
                // Only the first feedback is used: the buffers are made
                // once per size.
                if !state.feedback_done {
                    state.parent_formats = std::mem::take(&mut feedback.formats);
                    state.main_device = feedback.main_device;
                    state.feedback_done = true;
                } else {
                    feedback.formats.clear();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                state.params_result = Some(Some(buffer));
            }
            zwp_linux_buffer_params_v1::Event::Failed => state.params_result = Some(None),
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

impl State {
    /// The modifiers the parent takes for `XRGB8888` buffers whose layout
    /// can be described.
    pub fn parent_modifiers(&self) -> Vec<u64> {
        self.parent_formats
            .iter()
            .filter(|(format, modifier)| {
                *format == DRM_FORMAT_XRGB8888 && *modifier != DRM_FORMAT_MOD_INVALID
            })
            .map(|(_, modifier)| *modifier)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        else {
            return;
        };
        if capabilities.contains(wl_seat::Capability::Keyboard) {
            if state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        } else if let Some(keyboard) = state.keyboard.take() {
            if keyboard.version() >= 3 {
                keyboard.release();
            }
            state.release_keys();
        }
        if capabilities.contains(wl_seat::Capability::Pointer) {
            if state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        } else if let Some(pointer) = state.pointer.take() {
            state.set_pointer_lock(false);
            if let Some(relative) = state.relative_pointer.take() {
                relative.destroy();
            }
            if pointer.version() >= 3 {
                pointer.release();
            }
        }
        // Motion as the device reported it, for while the pointer is held.
        if let (Some(pointer), Some(manager), None) = (
            &state.pointer,
            &state.relative_manager,
            &state.relative_pointer,
        ) {
            state.relative_pointer = Some(manager.get_relative_pointer(pointer, qh, ()));
        }
        let has_touch = capabilities.contains(wl_seat::Capability::Touch);
        if has_touch {
            if state.touch.is_none() {
                state.touch = Some(seat.get_touch(qh, ()));
            }
        } else if let Some(touch) = state.touch.take()
            && touch.version() >= 3
        {
            touch.release();
        }
        if has_touch != state.touch_announced {
            state.touch_announced = has_touch;
            state.input(InputEvent::TouchAvailable(has_touch));
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
            wl_keyboard::Event::Enter { serial, keys, .. } => {
                state.selections.serial = serial;
                state.selection_focus(true);
                // Modifiers that are already held count: the shortcut that
                // brought the focus here often continues with them. Other
                // keys held at that moment were not meant for this window.
                let time_ms = state.last_time_ms;
                for key in keys.chunks_exact(4) {
                    let key = u32::from_ne_bytes([key[0], key[1], key[2], key[3]]);
                    if is_modifier(key) {
                        state.key(key, true, time_ms);
                    }
                }
            }
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                // The text, with a terminating zero.
                let file = File::from(fd);
                let mut bytes = vec![0u8; size as usize];
                match file.read_exact_at(&mut bytes, 0) {
                    Ok(()) => {
                        while bytes.last() == Some(&0) {
                            bytes.pop();
                        }
                        match String::from_utf8(bytes) {
                            Ok(keymap) => {
                                state.parent_keymap = true;
                                state.events.push_back(BackendEvent::Keymap(keymap));
                            }
                            Err(_) => tracing::warn!("the parent's keymap is not text"),
                        }
                    }
                    Err(error) => tracing::warn!("the parent's keymap cannot be read: {error}"),
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                state.release_keys();
                state.selection_focus(false);
            }
            // The parent says no modifier is held. One that is still held
            // here lost its release on the way (a shortcut of the parent
            // took the key in between): let go of it, or it would turn
            // every later key into a shortcut.
            wl_keyboard::Event::Modifiers {
                mods_depressed: 0, ..
            } => state.release_modifiers(),
            wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state: WEnum::Value(key_state),
            } => match key_state {
                wl_keyboard::KeyState::Pressed => {
                    state.selections.serial = serial;
                    state.key(key, true, time)
                }
                wl_keyboard::KeyState::Released => state.key(key, false, time),
                // Repeats are made by whoever receives the key.
                _ => {}
            },
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let framed = pointer.version() >= 5;
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                if state.hide_parent_cursor {
                    // The compositor inside draws its own cursor.
                    pointer.set_cursor(serial, None, 0, 0);
                } else if let Some(manager) = &state.cursor_shape_manager {
                    // Nothing inside draws one: the parent's default.
                    let device = manager.get_pointer(pointer, &state.qh, ());
                    device.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
                    device.destroy();
                }
                let time_ms = state.last_time_ms;
                state.pointer_position(surface_x, surface_y, time_ms);
            }
            wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => state.pointer_position(surface_x, surface_y, time),
            wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: WEnum::Value(button_state),
            } => {
                state.selections.serial = serial;
                let time_us = state.time_us(time);
                state.input(InputEvent::PointerButton {
                    button,
                    pressed: button_state == wl_pointer::ButtonState::Pressed,
                    time_us,
                });
            }
            wl_pointer::Event::Axis {
                time,
                axis: WEnum::Value(axis),
                value,
            } => {
                state.axis.any = true;
                state.axis.time_ms = time;
                match axis {
                    wl_pointer::Axis::HorizontalScroll => state.axis.horizontal += value,
                    _ => state.axis.vertical += value,
                }
                if !framed {
                    state.flush_axis();
                }
            }
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(source),
            } => {
                state.axis.source = Some(match source {
                    wl_pointer::AxisSource::Finger => AxisSource::Finger,
                    wl_pointer::AxisSource::Continuous => AxisSource::Continuous,
                    _ => AxisSource::Wheel,
                });
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => {
                state.axis.has_v120 = true;
                match axis {
                    wl_pointer::Axis::HorizontalScroll => state.axis.v120.0 += discrete * 120,
                    _ => state.axis.v120.1 += discrete * 120,
                }
            }
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => {
                state.axis.has_v120 = true;
                match axis {
                    wl_pointer::Axis::HorizontalScroll => state.axis.v120.0 += value120,
                    _ => state.axis.v120.1 += value120,
                }
            }
            wl_pointer::Event::Frame => state.flush_axis(),
            _ => {}
        }
    }
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => state.lock_active = true,
            zwp_locked_pointer_v1::Event::Unlocked => state.lock_active = false,
            _ => {}
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // While the pointer moves freely its positions say the same.
        if !state.lock_active {
            return;
        }
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        } = event
        {
            let time_us = (u64::from(utime_hi) << 32) | u64::from(utime_lo);
            state.last_time_ms = (time_us / 1000) as u32;
            state.input(InputEvent::PointerMotion {
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
                time_us,
            });
        }
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let origin = state
            .output
            .as_ref()
            .map_or((0, 0), |output| output.info.position);
        let place = |x: f64, y: f64| (f64::from(origin.0) + x, f64::from(origin.1) + y);
        match event {
            wl_touch::Event::Down {
                serial,
                time,
                id,
                x,
                y,
                ..
            } => {
                state.selections.serial = serial;
                let (x, y) = place(x, y);
                let time_us = state.time_us(time);
                state.input(InputEvent::TouchDown {
                    slot: id as u32,
                    x,
                    y,
                    time_us,
                });
            }
            wl_touch::Event::Motion { time, id, x, y } => {
                let (x, y) = place(x, y);
                let time_us = state.time_us(time);
                state.input(InputEvent::TouchMotion {
                    slot: id as u32,
                    x,
                    y,
                    time_us,
                });
            }
            wl_touch::Event::Up { time, id, .. } => {
                let time_us = state.time_us(time);
                state.input(InputEvent::TouchUp {
                    slot: id as u32,
                    time_us,
                });
            }
            wl_touch::Event::Frame => {
                let time_us = u64::from(state.last_time_ms) * 1000;
                state.input(InputEvent::TouchFrame { time_us });
            }
            wl_touch::Event::Cancel => {
                let time_us = u64::from(state.last_time_ms) * 1000;
                state.input(InputEvent::TouchCancel { time_us });
            }
            _ => {}
        }
    }
}

/// The output id of the nested window.
pub(super) const OUTPUT: OutputId = OutputId(1);
