//! Touch (`wl_touch`) and touchpad gestures (`zwp_pointer_gestures_v1`).
//!
//! A touch goes to the surface it went down on, until it comes up, wherever
//! the finger moves; each finger on its own surface. Which surface that is
//! comes from the caller's hit testing, as for the pointer, and so do the
//! surface-local positions of the motions.
//!
//! Smithay keeps the protocol side (the `wl_touch` objects, the serials of
//! the downs, one `frame` per client). Two things are done differently
//! here:
//!
//! - Smithay's implicit grab sends every further finger to the surface of
//!   the first one. The grab used here keeps the surface each finger went
//!   down on ([`TouchPointsGrab`]).
//! - Smithay sends `cancel` only to the touches that had an event since the
//!   last `frame`. To cancel every touch, each is given a motion to where it
//!   is first.
//!
//! Gestures go to the surface the pointer is on, through Smithay's pointer.
//! Smithay ends a gesture as cancelled for a surface the pointer leaves.

use smithay::backend::input::{InputTime, TouchSlot};
use smithay::desktop::PopupUngrabStrategy;
use smithay::input::pointer::{
    GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
    GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
};
use smithay::input::touch::{
    DownEvent, GrabStartData, MotionEvent, OrientationEvent, ShapeEvent, TouchGrab,
    TouchInnerHandle, UpEvent,
};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::state::State;
use crate::surface::surface_id;
use crate::{GestureKind, SurfaceId};

/// A finger on the screen, as this layer remembers it.
pub(crate) struct TouchPoint {
    /// The surface it went down on; `None` when it went down on nothing.
    pub surface: Option<WlSurface>,
    /// Where it was last reported, in that surface's coordinates.
    pub position: (f64, f64),
}

fn slot(id: u32) -> TouchSlot {
    Some(id).into()
}

impl State {
    /// Offers or withdraws the touch capability of the seat.
    pub fn set_touch_available(&mut self, available: bool) {
        match (available, self.seat.get_touch().is_some()) {
            (true, false) => {
                self.seat.add_touch();
            }
            (false, true) => {
                self.seat.remove_touch();
                self.touch_points.clear();
                self.touch_frame_owed = false;
            }
            _ => {}
        }
    }

    pub fn touch_available(&self) -> bool {
        self.seat.get_touch().is_some()
    }

    /// While a popup holds a grab, a touch on a surface of another client
    /// (or on nothing) dismisses it, as a pointer press there does. The
    /// touch itself goes on to where it went down.
    fn dismiss_popup_grab_outside(&mut self, surface: Option<&WlSurface>) {
        let Some(grab) = &mut self.popup_grab else {
            return;
        };
        if grab.has_ended() {
            self.popup_grab = None;
            return;
        }
        let inside = match (surface, grab.current_grab()) {
            (Some(surface), Some(grabbing)) => grabbing.id().same_client_as(&surface.id()),
            _ => false,
        };
        if !inside {
            let _ = grab.ungrab(PopupUngrabStrategy::All);
            self.popup_grab = None;
            // The keyboard goes back to where the compositor wants it.
            self.sync_keyboard_focus();
        }
    }

    pub fn touch_down(&mut self, id: u32, target: Option<(SurfaceId, (f64, f64))>, time_us: u64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let (surface, position) = match target {
            Some((surface, local)) => (self.surfaces.get(&surface).cloned(), local),
            None => (None, (0.0, 0.0)),
        };
        self.dismiss_popup_grab_outside(surface.as_ref());
        // As for the pointer: the surface-local position is passed as the
        // location, with the surface's origin at zero.
        let focus = surface
            .clone()
            .map(|surface| (surface, Point::<f64, Logical>::from((0.0, 0.0))));
        self.touch_points
            .insert(id, TouchPoint { surface, position });
        self.touch_frame_owed = true;
        touch.down(
            self,
            focus,
            &DownEvent {
                slot: slot(id),
                location: Point::from(position),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_micros(time_us),
            },
        );
    }

    pub fn touch_motion(&mut self, id: u32, position: (f64, f64), time_us: u64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let Some(point) = self.touch_points.get_mut(&id) else {
            return;
        };
        point.position = position;
        self.touch_frame_owed = true;
        touch.motion(
            self,
            None,
            &MotionEvent {
                slot: slot(id),
                location: Point::from(position),
                time: InputTime::from_micros(time_us),
            },
        );
    }

    pub fn touch_up(&mut self, id: u32, time_us: u64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        if self.touch_points.remove(&id).is_none() {
            return;
        }
        self.touch_frame_owed = true;
        touch.up(
            self,
            &UpEvent {
                slot: slot(id),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_micros(time_us),
            },
        );
    }

    pub fn touch_frame(&mut self) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        // Smithay complains about a frame with nothing in it.
        if std::mem::take(&mut self.touch_frame_owed) {
            touch.frame(self);
        }
    }

    pub fn touch_cancel(&mut self, time_us: u64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        if self.touch_points.is_empty() {
            return;
        }
        // Smithay cancels only the touches that are part of the frame being
        // built. Make every touch part of it, without moving it.
        let points: Vec<(u32, (f64, f64))> = self
            .touch_points
            .iter()
            .map(|(id, point)| (*id, point.position))
            .collect();
        for (id, position) in points {
            touch.motion(
                self,
                None,
                &MotionEvent {
                    slot: slot(id),
                    location: Point::from(position),
                    time: InputTime::from_micros(time_us),
                },
            );
        }
        self.touch_points.clear();
        self.touch_frame_owed = false;
        touch.cancel(self);
    }

    /// The fingers on the screen: slot and surface, ordered by slot.
    pub fn touch_points(&self) -> Vec<(u32, Option<SurfaceId>)> {
        let mut points: Vec<(u32, Option<SurfaceId>)> = self
            .touch_points
            .iter()
            .map(|(id, point)| (*id, point.surface.as_ref().and_then(surface_id)))
            .collect();
        points.sort();
        points
    }

    // -- gestures -------------------------------------------------------------

    pub fn gesture_begin(&mut self, kind: GestureKind, fingers: u32, time_us: u64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::from_micros(time_us);
        match kind {
            GestureKind::Swipe => pointer.gesture_swipe_begin(
                self,
                &GestureSwipeBeginEvent {
                    serial,
                    time,
                    fingers,
                },
            ),
            GestureKind::Pinch => pointer.gesture_pinch_begin(
                self,
                &GesturePinchBeginEvent {
                    serial,
                    time,
                    fingers,
                },
            ),
            GestureKind::Hold => pointer.gesture_hold_begin(
                self,
                &GestureHoldBeginEvent {
                    serial,
                    time,
                    fingers,
                },
            ),
        }
    }

    pub fn gesture_update(
        &mut self,
        kind: GestureKind,
        delta: (f64, f64),
        scale: f64,
        rotation: f64,
        time_us: u64,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let time = InputTime::from_micros(time_us);
        match kind {
            GestureKind::Swipe => pointer.gesture_swipe_update(
                self,
                &GestureSwipeUpdateEvent {
                    time,
                    delta: Point::from(delta),
                },
            ),
            GestureKind::Pinch => pointer.gesture_pinch_update(
                self,
                &GesturePinchUpdateEvent {
                    time,
                    delta: Point::from(delta),
                    scale,
                    rotation,
                },
            ),
            // A hold does not move.
            GestureKind::Hold => {}
        }
    }

    pub fn gesture_end(&mut self, kind: GestureKind, cancelled: bool, time_us: u64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::from_micros(time_us);
        match kind {
            GestureKind::Swipe => pointer.gesture_swipe_end(
                self,
                &GestureSwipeEndEvent {
                    serial,
                    time,
                    cancelled,
                },
            ),
            GestureKind::Pinch => pointer.gesture_pinch_end(
                self,
                &GesturePinchEndEvent {
                    serial,
                    time,
                    cancelled,
                },
            ),
            GestureKind::Hold => pointer.gesture_hold_end(
                self,
                &GestureHoldEndEvent {
                    serial,
                    time,
                    cancelled,
                },
            ),
        }
    }
}

/// The implicit grab of a touch: from the first finger down until the last
/// one is up. Unlike Smithay's, every finger keeps the surface it went down
/// on. The grab is what an interactive move or resize started by a touch
/// is checked against (its serial is that of the first down).
pub(crate) struct TouchPointsGrab {
    start_data: GrabStartData<State>,
    points: usize,
}

impl TouchPointsGrab {
    pub(crate) fn new(start_data: GrabStartData<State>) -> Self {
        Self {
            start_data,
            points: 1,
        }
    }
}

impl TouchGrab<State> for TouchPointsGrab {
    fn down(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &DownEvent,
    ) {
        handle.down(data, focus, event);
        self.points += 1;
    }

    fn up(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>, event: &UpEvent) {
        handle.up(data, event);
        self.points = self.points.saturating_sub(1);
        if self.points == 0 {
            handle.unset_grab(self, data);
        }
    }

    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // The focus of a motion is not used: each touch keeps its own.
        handle.motion(data, focus, event);
    }

    fn frame(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn cancel(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>) {
        handle.cancel(data);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &ShapeEvent,
    ) {
        handle.shape(data, event);
    }

    fn orientation(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &OrientationEvent,
    ) {
        handle.orientation(data, event);
    }

    fn start_data(&self) -> &GrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut State) {}
}
