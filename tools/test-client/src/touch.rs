//! Touch (`wl_touch`) and touchpad gestures (`zwp_pointer_gestures_v1`):
//! what a touch-friendly application, or one that zooms with two fingers,
//! listens to. Both are asked for explicitly, so that other tests see none
//! of these events.

use wayland_client::protocol::{wl_seat, wl_touch};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_hold_v1, zwp_pointer_gesture_pinch_v1, zwp_pointer_gesture_swipe_v1,
    zwp_pointer_gestures_v1,
};

use crate::{ClientError, ClientEvent, State, TestClient};

/// A touchpad gesture, as `zwp_pointer_gestures_v1` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GestureKind {
    Swipe,
    Pinch,
    Hold,
}

#[derive(Default)]
pub(crate) struct TouchState {
    /// The capabilities the seat announced last.
    pub capabilities: Option<wl_seat::Capability>,
    /// Take a `wl_touch` whenever the seat offers touch.
    pub wanted: bool,
    pub touch: Option<wl_touch::WlTouch>,
    pub gestures: Option<Gestures>,
}

/// The gesture objects, kept for as long as the client listens.
pub(crate) struct Gestures {
    _swipe: zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1,
    _pinch: zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1,
    /// Only with version 3.
    _hold: Option<zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1>,
}

impl State {
    /// Follows `wl_seat.capabilities`: takes a `wl_touch` when touch comes
    /// and it is wanted, releases it when touch goes.
    pub(crate) fn touch_capabilities(
        &mut self,
        seat: &wl_seat::WlSeat,
        capabilities: wl_seat::Capability,
        handle: &QueueHandle<State>,
    ) {
        self.touch.capabilities = Some(capabilities);
        let offered = capabilities.contains(wl_seat::Capability::Touch);
        if offered && self.touch.wanted && self.touch.touch.is_none() {
            self.touch.touch = Some(seat.get_touch(handle, ()));
        } else if !offered
            && let Some(touch) = self.touch.touch.take()
            && touch.version() >= 3
        {
            touch.release();
        }
    }
}

impl TestClient {
    /// Whether the seat offers touch, once it said what it offers.
    pub fn seat_has_touch(&self) -> Option<bool> {
        self.state
            .touch
            .capabilities
            .map(|c| c.contains(wl_seat::Capability::Touch))
    }

    /// Whether the client holds a `wl_touch` now.
    pub fn has_touch(&self) -> bool {
        self.state.touch.touch.is_some()
    }

    /// Asks for touch events (`wl_touch`), now if the seat offers touch, or
    /// as soon as it does. They arrive as [`ClientEvent::TouchDown`] and the
    /// other touch events.
    pub fn request_touch(&mut self) {
        self.state.touch.wanted = true;
        let (Some(seat), Some(capabilities)) =
            (&self.state.globals.seat, self.state.touch.capabilities)
        else {
            return;
        };
        if capabilities.contains(wl_seat::Capability::Touch) && self.state.touch.touch.is_none() {
            self.state.touch.touch = Some(seat.get_touch(&self.handle, ()));
        }
    }

    /// Asks for touchpad gestures: swipe, pinch and, with version 3 of the
    /// protocol, hold. They arrive as [`ClientEvent::GestureBegin`],
    /// [`ClientEvent::GestureUpdate`] and [`ClientEvent::GestureEnd`].
    pub fn request_pointer_gestures(&mut self) -> Result<(), ClientError> {
        let state = &mut self.state;
        let (Some(manager), Some(pointer)) =
            (&state.globals.pointer_gestures, &state.pointer_object)
        else {
            return Err(ClientError::Protocol(
                "no zwp_pointer_gestures_v1 or no pointer yet".to_string(),
            ));
        };
        state.touch.gestures = Some(Gestures {
            _swipe: manager.get_swipe_gesture(pointer, &self.handle, ()),
            _pinch: manager.get_pinch_gesture(pointer, &self.handle, ()),
            _hold: (manager.version() >= 3)
                .then(|| manager.get_hold_gesture(pointer, &self.handle, ())),
        });
        Ok(())
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
        match event {
            wl_touch::Event::Down {
                serial,
                surface,
                id,
                x,
                y,
                ..
            } => {
                // A held touch can start a move or a resize, as a held
                // button can.
                state.last_serial = serial;
                state.last_press_serial = serial;
                let surface = state.kind_of(&surface);
                state
                    .events
                    .push(ClientEvent::TouchDown { id, surface, x, y });
            }
            wl_touch::Event::Motion { id, x, y, .. } => {
                state.events.push(ClientEvent::TouchMotion { id, x, y });
            }
            wl_touch::Event::Up { serial, id, .. } => {
                state.last_serial = serial;
                state.events.push(ClientEvent::TouchUp { id });
            }
            wl_touch::Event::Frame => state.events.push(ClientEvent::TouchFrame),
            wl_touch::Event::Cancel => state.events.push(ClientEvent::TouchCancel),
            _ => {}
        }
    }
}

impl Dispatch<zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1,
        event: zwp_pointer_gesture_swipe_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_pointer_gesture_swipe_v1::Event;
        let kind = GestureKind::Swipe;
        match event {
            Event::Begin {
                surface, fingers, ..
            } => {
                let surface = state.kind_of(&surface);
                state.events.push(ClientEvent::GestureBegin {
                    kind,
                    fingers,
                    surface,
                });
            }
            Event::Update { dx, dy, .. } => state.events.push(ClientEvent::GestureUpdate {
                kind,
                dx,
                dy,
                scale: 1.0,
                rotation: 0.0,
            }),
            Event::End { cancelled, .. } => state.events.push(ClientEvent::GestureEnd {
                kind,
                cancelled: cancelled != 0,
            }),
            _ => {}
        }
    }
}

impl Dispatch<zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1,
        event: zwp_pointer_gesture_pinch_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_pointer_gesture_pinch_v1::Event;
        let kind = GestureKind::Pinch;
        match event {
            Event::Begin {
                surface, fingers, ..
            } => {
                let surface = state.kind_of(&surface);
                state.events.push(ClientEvent::GestureBegin {
                    kind,
                    fingers,
                    surface,
                });
            }
            Event::Update {
                dx,
                dy,
                scale,
                rotation,
                ..
            } => state.events.push(ClientEvent::GestureUpdate {
                kind,
                dx,
                dy,
                scale,
                rotation,
            }),
            Event::End { cancelled, .. } => state.events.push(ClientEvent::GestureEnd {
                kind,
                cancelled: cancelled != 0,
            }),
            _ => {}
        }
    }
}

impl Dispatch<zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1,
        event: zwp_pointer_gesture_hold_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_pointer_gesture_hold_v1::Event;
        let kind = GestureKind::Hold;
        match event {
            Event::Begin {
                surface, fingers, ..
            } => {
                let surface = state.kind_of(&surface);
                state.events.push(ClientEvent::GestureBegin {
                    kind,
                    fingers,
                    surface,
                });
            }
            Event::End { cancelled, .. } => state.events.push(ClientEvent::GestureEnd {
                kind,
                cancelled: cancelled != 0,
            }),
            _ => {}
        }
    }
}

delegate_noop!(State: zwp_pointer_gestures_v1::ZwpPointerGesturesV1);
