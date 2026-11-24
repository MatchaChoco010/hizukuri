//! Touches and touchpad gestures in the core.
//!
//! - **Touch.** A touch goes to the surface it went down on until it comes
//!   up, each finger on its own: the scene's hit test picks the surface at
//!   the down, the scene's `locate` turns every later position into that
//!   surface's coordinates (as for a held pointer button). Whether the touched
//!   window gets the focus is the window manager's to decide
//!   ([`WindowManager::touch_down`]); by default it does, as with a click.
//! - **Gestures.** The window manager is asked at the begin whether it takes
//!   the gesture ([`WindowManager::gesture_begin`]). A gesture it takes is
//!   its own to the end: no client sees any of it. One it does not take goes
//!   to the client the pointer is on, entirely. The decision is not
//!   revisited halfway, so that a client never sees a gesture without its
//!   begin or its end.
//!
//! [`WindowManager::touch_down`]: crate::wm::WindowManager::touch_down
//! [`WindowManager::gesture_begin`]: crate::wm::WindowManager::gesture_begin

use std::collections::BTreeMap;

use proto_backend::{Backend, GestureKind};
use proto_wl::SurfaceId;
use serde_json::{Value, json};

use super::Compositor;
use crate::wm::{GestureStart, GestureStep};

/// Who a gesture in progress belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureOwner {
    /// The window manager took it.
    WindowManager,
    /// It goes to a client.
    Client,
}

/// What the core remembers about touches and gestures between events.
#[derive(Debug, Default)]
pub(super) struct TouchState {
    /// The fingers on the screen: the surface each went down on (`None`:
    /// nothing was there), by slot.
    points: BTreeMap<u32, Option<SurfaceId>>,
    /// The gesture in progress, and who has it.
    gesture: Option<(GestureKind, GestureOwner)>,
}

pub(super) fn wl_kind(kind: GestureKind) -> proto_wl::GestureKind {
    match kind {
        GestureKind::Swipe => proto_wl::GestureKind::Swipe,
        GestureKind::Pinch => proto_wl::GestureKind::Pinch,
        GestureKind::Hold => proto_wl::GestureKind::Hold,
    }
}

impl<B: Backend> Compositor<B> {
    pub(super) fn touch_available(&mut self, available: bool) {
        self.wl.set_touch_available(available);
        if !available {
            self.touch.points.clear();
        }
    }

    pub(super) fn touch_down(&mut self, slot: u32, position: (f64, f64), time_us: u64) {
        // Whoever answers must know the windows as they are now.
        self.sync_world();
        let hit = self.surface_under(position);
        let decided = self.wm.touch_down(slot, position, hit.as_ref());
        if !decided
            && let Some(hit) = &hit
            && self.wm.focus_window(Some(hit.window))
        {
            self.apply_focus();
            self.schedule_redraw();
        }
        self.sync_world();
        self.run_window_manager();
        // A slot that is still down (a missed up) starts over.
        if self.touch.points.contains_key(&slot) {
            self.wl.touch_up(slot, time_us);
        }
        self.touch.points.insert(slot, hit.map(|h| h.surface));
        self.wl
            .touch_down(slot, hit.map(|h| (h.surface, h.local)), time_us);
    }

    pub(super) fn touch_motion(&mut self, slot: u32, position: (f64, f64), time_us: u64) {
        let Some(Some(surface)) = self.touch.points.get(&slot).copied() else {
            return;
        };
        self.sync_world();
        let views = self.window_views();
        // The surface may be gone, or not be on screen any more.
        if let Some(local) = self.scene.locate(position, surface, &views) {
            self.wl.touch_motion(slot, local, time_us);
        }
    }

    pub(super) fn touch_up(&mut self, slot: u32, time_us: u64) {
        if self.touch.points.remove(&slot).is_some() {
            self.wl.touch_up(slot, time_us);
        }
    }

    pub(super) fn touch_frame(&mut self) {
        self.wl.touch_frame();
    }

    pub(super) fn touch_cancel(&mut self, time_us: u64) {
        self.touch.points.clear();
        self.wl.touch_cancel(time_us);
    }

    pub(super) fn gesture_begin(&mut self, kind: GestureKind, fingers: u32, time_us: u64) {
        // A begin without the end of the one before: that one is over.
        if let Some((previous, _)) = self.touch.gesture {
            self.gesture_end(previous, true, time_us);
        }
        self.sync_world();
        let hit = if self.pointer_active {
            self.surface_under(self.pointer)
        } else {
            None
        };
        let taken = self.wm.gesture_begin(&GestureStart {
            kind: wl_kind(kind),
            fingers,
            hit,
        });
        if taken {
            self.touch.gesture = Some((kind, GestureOwner::WindowManager));
            self.sync_world();
            self.run_window_manager();
        } else {
            self.touch.gesture = Some((kind, GestureOwner::Client));
            self.wl.gesture_begin(wl_kind(kind), fingers, time_us);
        }
    }

    pub(super) fn gesture_update(
        &mut self,
        kind: GestureKind,
        delta: (f64, f64),
        scale: f64,
        rotation: f64,
        time_us: u64,
    ) {
        match self.touch.gesture {
            Some((current, GestureOwner::WindowManager)) if current == kind => {
                self.wm.gesture_step(&GestureStep {
                    kind: wl_kind(kind),
                    dx: delta.0,
                    dy: delta.1,
                    scale,
                    rotation,
                });
                self.sync_world();
                self.run_window_manager();
            }
            Some((current, GestureOwner::Client)) if current == kind => {
                self.wl
                    .gesture_update(wl_kind(kind), delta, scale, rotation, time_us);
            }
            // Not the gesture in progress.
            _ => {}
        }
    }

    pub(super) fn gesture_end(&mut self, kind: GestureKind, cancelled: bool, time_us: u64) {
        match self.touch.gesture {
            Some((current, owner)) if current == kind => {
                self.touch.gesture = None;
                match owner {
                    GestureOwner::WindowManager => {
                        self.wm.gesture_end(wl_kind(kind), cancelled);
                        self.sync_world();
                        self.run_window_manager();
                    }
                    GestureOwner::Client => {
                        self.wl.gesture_end(wl_kind(kind), cancelled, time_us);
                    }
                }
            }
            _ => {}
        }
    }

    /// The touches and the gesture in progress, for the state dump.
    pub(super) fn describe_touch(&self) -> Value {
        json!({
            "available": self.wl.touch_available(),
            "points": self
                .wl
                .touch_points()
                .into_iter()
                .map(|(slot, surface)| json!({ "slot": slot, "surface": surface.map(|s| s.0) }))
                .collect::<Vec<_>>(),
            "gesture": self.touch.gesture.map(|(kind, owner)| json!({
                "kind": kind.name(),
                "owner": match owner {
                    GestureOwner::WindowManager => "window_manager",
                    GestureOwner::Client => "client",
                },
            })),
        })
    }
}
