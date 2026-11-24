//! Relative pointer motion (`zwp_relative_pointer_manager_v1`) and pointer
//! constraints (`zwp_pointer_constraints_v1`).
//!
//! Smithay keeps the protocol state of a constraint (the objects, the
//! double-buffered region and cursor position hint, `locked` sent once,
//! oneshot constraints dropped after they end) in the surface's data, keyed
//! by the seat's pointer. It sends nothing on its own except when the
//! pointer leaves the surface: then an active constraint ends. When to turn
//! one on, and when to end it otherwise, is left to the compositor; this
//! module only offers the means, by surface id.

use smithay::backend::input::InputTime;
use smithay::input::pointer::{PointerHandle, RelativeMotionEvent};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Point;
use smithay::wayland::compositor::{SurfaceAttributes, with_states};
use smithay::wayland::pointer_constraints::{
    ConstraintRemove, PointerConstraint as SmithayConstraint, PointerConstraintsHandler,
    with_pointer_constraint,
};

use crate::state::State;
use crate::surface::{surface_id, surface_size};
use crate::{PointerConstraint, PointerConstraintKind, SurfaceId, WlEvent};

impl State {
    fn pointer(&self) -> Option<PointerHandle<State>> {
        self.seat.get_pointer()
    }

    pub fn pointer_relative_motion(
        &mut self,
        delta: (f64, f64),
        unaccelerated: (f64, f64),
        time_us: u64,
    ) {
        let Some(pointer) = self.pointer() else {
            return;
        };
        // The relative pointer shares the focus of the wl_pointer. Smithay
        // takes the focus again with every relative motion and keeps it as
        // where the pointer is, for when a grab ends: it has to be what
        // `pointer_motion` was last told, not the focus a grab holds (a
        // popup's), or the end of the grab puts the pointer back there.
        let focus = self
            .pointer_target
            .clone()
            .map(|surface| (surface, Point::from((0.0, 0.0))));
        pointer.relative_motion(
            self,
            focus,
            &RelativeMotionEvent {
                delta: Point::from(delta),
                delta_unaccel: Point::from(unaccelerated),
                time: InputTime::from_micros(time_us),
            },
        );
    }

    /// Reads a constraint without changing it.
    fn describe_constraint(&self, surface: &WlSurface) -> Option<PointerConstraint> {
        let pointer = self.pointer()?;
        let id = surface_id(surface)?;
        let window = self.owning_window(surface);
        with_pointer_constraint(surface, &pointer, |constraint| {
            let constraint = constraint?;
            let (kind, cursor_hint) = match &*constraint {
                SmithayConstraint::Locked(locked) => (
                    PointerConstraintKind::Lock,
                    locked.cursor_position_hint().map(|p| (p.x, p.y)),
                ),
                SmithayConstraint::Confined(_) => (PointerConstraintKind::Confine, None),
            };
            Some(PointerConstraint {
                surface: id,
                window,
                kind,
                active: constraint.is_active(),
                cursor_hint,
            })
        })
    }

    pub fn pointer_constraints(&self) -> Vec<PointerConstraint> {
        self.constrained_surfaces
            .iter()
            .filter_map(|surface| self.describe_constraint(surface))
            .collect()
    }

    pub fn pointer_constraint(&self, id: SurfaceId) -> Option<PointerConstraint> {
        self.describe_constraint(self.surfaces.get(&id)?)
    }

    pub fn pointer_constraint_region_contains(&self, id: SurfaceId, x: f64, y: f64) -> bool {
        let (Some(surface), Some(pointer)) = (self.surfaces.get(&id), self.pointer()) else {
            return false;
        };
        let (w, h) = surface_size(surface);
        if x < 0.0 || y < 0.0 || x >= w as f64 || y >= h as f64 {
            return false;
        }
        let point = (x.floor() as i32, y.floor() as i32);
        let in_constraint_region = with_pointer_constraint(surface, &pointer, |constraint| {
            constraint.is_some_and(|c| c.region().is_none_or(|region| region.contains(point)))
        });
        // The protocol uses the intersection with the input region.
        in_constraint_region
            && with_states(surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                guard
                    .current()
                    .input_region
                    .as_ref()
                    .is_none_or(|region| region.contains(point))
            })
    }

    pub fn set_pointer_constraint_active(&mut self, id: SurfaceId, active: bool) -> bool {
        let (Some(surface), Some(pointer)) = (self.surfaces.get(&id).cloned(), self.pointer())
        else {
            return false;
        };
        // `locked` promises the client that it has the pointer focus.
        if active && pointer.current_focus().as_ref() != Some(&surface) {
            return false;
        }
        let changed = with_pointer_constraint(&surface, &pointer, |constraint| {
            let Some(constraint) = constraint else {
                return false;
            };
            match (active, constraint.is_active()) {
                (true, false) => {
                    constraint.activate();
                    true
                }
                (false, true) => {
                    // Ends a oneshot constraint for good.
                    constraint.deactivate();
                    true
                }
                // Already as wanted.
                _ => true,
            }
        });
        if changed {
            self.prune_constraints();
        }
        changed
    }

    /// Forgets surfaces whose constraint is gone (withdrawn, oneshot and
    /// ended, or the surface destroyed).
    fn prune_constraints(&mut self) {
        let Some(pointer) = self.pointer() else {
            return;
        };
        self.constrained_surfaces.retain(|surface| {
            surface.is_alive() && with_pointer_constraint(surface, &pointer, |c| c.is_some())
        });
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        if !self.constrained_surfaces.contains(surface) {
            self.constrained_surfaces.push(surface.clone());
        }
        self.events.push(WlEvent::PointerConstraintsChanged);
    }

    fn remove_constraint(
        &mut self,
        surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        removed: ConstraintRemove,
    ) {
        match removed {
            ConstraintRemove::Destroyed(constraint) => {
                // A lock the client withdrew while it was in effect: the
                // compositor may put the pointer where the client drew its
                // cursor.
                if let SmithayConstraint::Locked(locked) = &constraint
                    && constraint.is_active()
                    && let Some(id) = surface_id(surface)
                {
                    self.events.push(WlEvent::PointerLockWithdrawn {
                        surface: id,
                        hint: locked.cursor_position_hint().map(|p| (p.x, p.y)),
                    });
                }
            }
            // The pointer left the surface and Smithay ended the
            // constraint. Despite the name, a persistent one stays.
            ConstraintRemove::PointerLeave(_) => {}
        }
        self.prune_constraints();
        self.events.push(WlEvent::PointerConstraintsChanged);
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, smithay::utils::Logical>,
    ) {
        // Read when the lock ends (`PointerConstraint::cursor_hint`).
    }
}
