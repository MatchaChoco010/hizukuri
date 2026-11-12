//! Relative pointer motion and pointer constraints in the core.
//!
//! The protocol layer keeps the state of each constraint and only turns one
//! on while the pointer is on its surface. What is decided here:
//!
//! - **What a constraint does to the pointer.** A lock keeps the pointer
//!   where it is: motion turns into relative motion only, and no
//!   `wl_pointer.motion` is sent. A confinement keeps the pointer inside the
//!   region, sliding along its edge. While either is in effect the pointer
//!   stays on the constrained surface whatever is drawn above it.
//! - **When a constraint is in effect.** That is the window manager's to
//!   decide ([`WindowManager::pointer_constraint`]). It is asked whenever a
//!   constraint could start and something changed since its last answer: the
//!   constraint is new, the pointer came onto the surface and into the
//!   region, the keyboard focus moved. Without an answer, the default is: in
//!   effect while the constraint's window has the keyboard focus.
//! - **The way out.** [`WmRequest::ReleasePointerConstraint`] ends the
//!   constraint in effect; it is not offered again until something changes.
//!   The pointer leaving the surface (the window closing, another window
//!   coming under the pointer) ends it as well.
//! - **Where the pointer goes after a lock.** When a lock with a cursor
//!   position hint ends, the pointer is put at the hint, when the scene
//!   places the surface flat enough to tell where that is.
//!
//! [`WindowManager::pointer_constraint`]: crate::wm::WindowManager::pointer_constraint
//! [`WmRequest::ReleasePointerConstraint`]: crate::wm::WmRequest::ReleasePointerConstraint

use std::collections::HashMap;

use proto_backend::Backend;
use proto_wl::{PointerConstraint, PointerConstraintKind, SurfaceId};

use super::{Compositor, WmWindowId, to_wm};
use crate::wm::ConstraintView;

/// What the core remembers about constraints between events.
#[derive(Debug, Default)]
pub(super) struct ConstraintPolicy {
    /// Per constrained surface: the keyboard focus when the window manager
    /// last decided about it. It is asked again once that differs, or once
    /// the entry is gone (the pointer left the surface or the region).
    decided: HashMap<SurfaceId, Option<WmWindowId>>,
    /// The constraint the window manager was last told is in effect.
    announced: Option<ConstraintView>,
}

impl<B: Backend> Compositor<B> {
    fn view(&self, constraint: &PointerConstraint) -> ConstraintView {
        ConstraintView {
            window: constraint.window.map(to_wm),
            surface: constraint.surface,
            kind: constraint.kind,
            active: constraint.active,
        }
    }

    /// The constraint in effect, if any. There is at most one: a constraint
    /// is only in effect while the pointer is on its surface.
    pub(super) fn active_constraint(&self) -> Option<PointerConstraint> {
        self.wl.pointer_constraints().into_iter().find(|c| c.active)
    }

    /// The surface the pointer is held on by a constraint in effect.
    pub(super) fn constrained_surface(&self) -> Option<SurfaceId> {
        self.active_constraint().map(|c| c.surface)
    }

    /// Whether the pointer is locked to `surface`.
    pub(super) fn locked_to(&self, surface: SurfaceId) -> bool {
        self.active_constraint()
            .is_some_and(|c| c.surface == surface && c.kind == PointerConstraintKind::Lock)
    }

    /// A relative motion from a device: the pointer moves as far as the
    /// constraint in effect lets it, and the motion itself goes to the
    /// client under the pointer either way.
    pub(super) fn relative_pointer_motion(
        &mut self,
        delta: (f64, f64),
        unaccelerated: (f64, f64),
        time_us: u64,
    ) {
        let from = self.pointer;
        let wanted = self.clamp_to_outputs((from.0 + delta.0, from.1 + delta.1));
        match self.active_constraint() {
            Some(c) if c.kind == PointerConstraintKind::Lock => {}
            Some(c) => {
                let to = self.confine(c.surface, from, wanted);
                self.move_pointer(to, time_us);
            }
            None => self.move_pointer(wanted, time_us),
        }
        self.wl
            .pointer_relative_motion(delta, unaccelerated, time_us);
        self.update_pointer_constraints();
    }

    /// An absolute position from a device (a tablet, the control
    /// interface). Under a lock the pointer stays and only the difference
    /// is reported, as relative motion.
    pub(super) fn absolute_pointer_motion(&mut self, position: (f64, f64), time_us: u64) {
        let from = self.pointer;
        let was_active = self.pointer_active;
        let delta = (position.0 - from.0, position.1 - from.1);
        match self.active_constraint() {
            Some(c) if c.kind == PointerConstraintKind::Lock => {}
            Some(c) => {
                let to = self.confine(c.surface, from, position);
                self.move_pointer(to, time_us);
            }
            None => self.move_pointer(position, time_us),
        }
        // A pointer that was nowhere did not move from anywhere.
        if was_active && delta != (0.0, 0.0) {
            self.wl.pointer_relative_motion(delta, delta, time_us);
        }
        self.update_pointer_constraints();
    }

    /// Where a confined pointer that wants to go from `from` to `to` ends
    /// up: `to` when that is inside the region, else as far as it can
    /// slide along one axis, else where it was.
    ///
    /// Positions are tried through the scene's `locate`, so a window the
    /// scene draws transformed confines in its own coordinates. Clamping to
    /// the edge exactly would need the way back, from the surface to the
    /// screen, which the scene does not offer.
    fn confine(&self, surface: SurfaceId, from: (f64, f64), to: (f64, f64)) -> (f64, f64) {
        let views = self.window_views();
        let inside = |point: (f64, f64)| {
            self.scene
                .locate(point, surface, &views)
                .is_some_and(|(x, y)| self.wl.pointer_constraint_region_contains(surface, x, y))
        };
        [to, (to.0, from.1), (from.0, to.1)]
            .into_iter()
            .find(|point| inside(*point))
            .unwrap_or(from)
    }

    /// Brings the constraints up to date with the pointer and the focus:
    /// asks the window manager about those that could start, ends those it
    /// no longer wants, and tells it when the one in effect changed.
    pub(super) fn update_pointer_constraints(&mut self) {
        let constraints = self.wl.pointer_constraints();
        let focus = self.wm.focus();
        let pointer_on = self.pointer_sent;
        self.constraints
            .decided
            .retain(|surface, _| constraints.iter().any(|c| c.surface == *surface));
        for constraint in &constraints {
            let surface = constraint.surface;
            let local = pointer_on
                .filter(|(on, _)| *on == surface)
                .map(|(_, local)| local);
            let in_region = local
                .is_some_and(|(x, y)| self.wl.pointer_constraint_region_contains(surface, x, y));
            // A lock holds the pointer still: once in effect, the region
            // no longer matters. A confinement whose region moved away
            // from under the pointer ends.
            let possible = match (constraint.active, constraint.kind) {
                (true, PointerConstraintKind::Lock) => local.is_some(),
                _ => in_region,
            };
            if !possible {
                if constraint.active {
                    self.end_constraint(constraint);
                }
                self.constraints.decided.remove(&surface);
                continue;
            }
            if self.constraints.decided.get(&surface) == Some(&focus) {
                continue;
            }
            let view = self.view(constraint);
            let allowed = self
                .wm
                .pointer_constraint(&view)
                .unwrap_or_else(|| view.window.is_some() && view.window == focus);
            self.constraints.decided.insert(surface, focus);
            match (allowed, constraint.active) {
                (true, false) => {
                    if !self.wl.set_pointer_constraint_active(surface, true) {
                        tracing::debug!(?surface, "a pointer constraint could not start");
                    }
                }
                (false, true) => self.end_constraint(constraint),
                _ => {}
            }
        }
        self.announce_pointer_constraint();
    }

    /// Ends the constraint in effect at the window manager's request. It
    /// is not offered again until the focus moves or the pointer comes back
    /// onto the surface.
    pub(super) fn release_pointer_constraint(&mut self) {
        if let Some(constraint) = self.active_constraint() {
            self.end_constraint(&constraint);
            let focus = self.wm.focus();
            self.constraints.decided.insert(constraint.surface, focus);
        }
        self.announce_pointer_constraint();
    }

    /// Forgets what the window manager decided, and asks it again.
    pub(super) fn review_pointer_constraints(&mut self) {
        self.constraints.decided.clear();
        self.update_pointer_constraints();
    }

    fn end_constraint(&mut self, constraint: &PointerConstraint) {
        self.wl
            .set_pointer_constraint_active(constraint.surface, false);
        if constraint.kind == PointerConstraintKind::Lock
            && let Some(hint) = constraint.cursor_hint
        {
            self.warp_to_hint(constraint.surface, hint);
        }
    }

    /// Puts the pointer where a client that drew its own cursor during a
    /// lock last drew it. The protocol lets the compositor do this, and
    /// sends no relative motion for it.
    ///
    /// The core knows where a surface is only as laid out flat (window
    /// position plus the surface's offset). It moves the pointer there
    /// when the scene agrees that this point is the hint on the surface,
    /// and leaves the pointer alone when the scene draws the surface
    /// elsewhere.
    pub(super) fn warp_to_hint(&mut self, surface: SurfaceId, hint: (f64, f64)) {
        let views = self.window_views();
        let flat = self.wm.placements().into_iter().find_map(|placement| {
            let node = self
                .wl
                .window_surfaces(super::to_wl(placement.window))
                .into_iter()
                .find(|node| node.surface == surface)?;
            Some((
                (placement.position.x + node.offset.0) as f64 + hint.0,
                (placement.position.y + node.offset.1) as f64 + hint.1,
            ))
        });
        let Some(position) = flat else {
            return;
        };
        let agrees = self
            .scene
            .locate(position, surface, &views)
            .is_some_and(|(x, y)| (x - hint.0).abs() < 0.5 && (y - hint.1).abs() < 0.5);
        if agrees {
            let time_us = self.time_us();
            self.move_pointer(self.clamp_to_outputs(position), time_us);
        }
    }

    /// Tells the window manager when the constraint in effect changed.
    fn announce_pointer_constraint(&mut self) {
        let active = self.active_constraint().map(|c| self.view(&c));
        if active != self.constraints.announced {
            self.constraints.announced = active;
            self.wm.pointer_constraint_changed(active.as_ref());
        }
    }

    /// The constraints for the state dump.
    pub(super) fn describe_pointer_constraints(&self) -> serde_json::Value {
        self.wl
            .pointer_constraints()
            .iter()
            .map(|c| {
                serde_json::json!({
                    "surface": c.surface.0,
                    "window": c.window.map(|w| w.0),
                    "kind": c.kind.name(),
                    "active": c.active,
                    "cursor_hint": c.cursor_hint.map(|(x, y)| [x, y]),
                })
            })
            .collect()
    }
}
