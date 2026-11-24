//! Relative pointer motion and pointer constraints: what a game asks for to
//! turn mouse motion into a view rotation.

use wayland_client::protocol::wl_region;
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};

use crate::{ClientError, ClientEvent, State, TestClient};

/// What to ask the compositor to do with the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintRequest {
    /// `lock_pointer`: keep the pointer still.
    Lock,
    /// `confine_pointer`: keep the pointer inside the region.
    Confine,
}

/// A constraint this client asked for.
pub(crate) enum Constraint {
    Locked(zwp_locked_pointer_v1::ZwpLockedPointerV1),
    Confined(zwp_confined_pointer_v1::ZwpConfinedPointerV1),
}

#[derive(Default)]
pub(crate) struct PointerState {
    pub relative: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    /// Newest last.
    pub constraints: Vec<Constraint>,
}

impl TestClient {
    /// Asks for relative motion events (`zwp_relative_pointer_v1`). They
    /// arrive as [`ClientEvent::RelativeMotion`].
    pub fn request_relative_pointer(&mut self) -> Result<(), ClientError> {
        let state = &mut self.state;
        let (Some(manager), Some(pointer)) = (
            &state.globals.relative_pointer_manager,
            &state.pointer_object,
        ) else {
            return Err(ClientError::Protocol(
                "no zwp_relative_pointer_manager_v1 or no pointer yet".to_string(),
            ));
        };
        state.pointer.relative = Some(manager.get_relative_pointer(pointer, &self.handle, ()));
        Ok(())
    }

    /// Asks for the pointer to be locked or confined to the window. With
    /// `region` (x, y, width, height in surface coordinates) only there;
    /// otherwise anywhere on the window. `persistent` constraints may come
    /// back after they end; oneshot ones do not.
    pub fn constrain_pointer(
        &mut self,
        request: ConstraintRequest,
        region: Option<(i32, i32, i32, i32)>,
        persistent: bool,
    ) -> Result<(), ClientError> {
        let state = &mut self.state;
        let (Some(manager), Some(pointer), Some(window), Some(compositor)) = (
            &state.globals.pointer_constraints,
            &state.pointer_object,
            &state.window,
            &state.globals.compositor,
        ) else {
            return Err(ClientError::Protocol(
                "no zwp_pointer_constraints_v1, no pointer or no window yet".to_string(),
            ));
        };
        let region = region.map(|(x, y, w, h)| {
            let region = compositor.create_region(&self.handle, ());
            region.add(x, y, w, h);
            region
        });
        let lifetime = if persistent {
            zwp_pointer_constraints_v1::Lifetime::Persistent
        } else {
            zwp_pointer_constraints_v1::Lifetime::Oneshot
        };
        let constraint = match request {
            ConstraintRequest::Lock => Constraint::Locked(manager.lock_pointer(
                &window.surface,
                pointer,
                region.as_ref(),
                lifetime,
                &self.handle,
                (),
            )),
            ConstraintRequest::Confine => Constraint::Confined(manager.confine_pointer(
                &window.surface,
                pointer,
                region.as_ref(),
                lifetime,
                &self.handle,
                (),
            )),
        };
        if let Some(region) = region {
            region.destroy();
        }
        state.pointer.constraints.push(constraint);
        Ok(())
    }

    /// Withdraws the newest constraint (destroys its object).
    pub fn withdraw_pointer_constraint(&mut self) {
        match self.state.pointer.constraints.pop() {
            Some(Constraint::Locked(locked)) => locked.destroy(),
            Some(Constraint::Confined(confined)) => confined.destroy(),
            None => {}
        }
    }

    /// Sets the cursor position hint of the newest lock and commits the
    /// window, which applies it.
    pub fn set_cursor_position_hint(&mut self, x: f64, y: f64) -> Result<(), ClientError> {
        let state = &self.state;
        let (Some(Constraint::Locked(locked)), Some(window)) =
            (state.pointer.constraints.last(), &state.window)
        else {
            return Err(ClientError::Protocol("no lock and window".to_string()));
        };
        locked.set_cursor_position_hint(x, y);
        window.surface.commit();
        Ok(())
    }

    /// Replaces the region of the newest constraint and commits the window,
    /// which applies it.
    pub fn set_constraint_region(
        &mut self,
        region: Option<(i32, i32, i32, i32)>,
    ) -> Result<(), ClientError> {
        let state = &self.state;
        let (Some(constraint), Some(window), Some(compositor)) = (
            state.pointer.constraints.last(),
            &state.window,
            &state.globals.compositor,
        ) else {
            return Err(ClientError::Protocol(
                "no constraint and window".to_string(),
            ));
        };
        let region = region.map(|(x, y, w, h)| {
            let region = compositor.create_region(&self.handle, ());
            region.add(x, y, w, h);
            region
        });
        match constraint {
            Constraint::Locked(locked) => locked.set_region(region.as_ref()),
            Constraint::Confined(confined) => confined.set_region(region.as_ref()),
        }
        if let Some(region) = region {
            region.destroy();
        }
        window.surface.commit();
        Ok(())
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
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        } = event
        {
            state.events.push(ClientEvent::RelativeMotion {
                time_us: (u64::from(utime_hi) << 32) | u64::from(utime_lo),
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            });
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
            zwp_locked_pointer_v1::Event::Locked => state.events.push(ClientEvent::PointerLocked),
            zwp_locked_pointer_v1::Event::Unlocked => {
                state.events.push(ClientEvent::PointerUnlocked)
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_confined_pointer_v1::ZwpConfinedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        event: zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_confined_pointer_v1::Event::Confined => {
                state.events.push(ClientEvent::PointerConfined)
            }
            zwp_confined_pointer_v1::Event::Unconfined => {
                state.events.push(ClientEvent::PointerUnconfined)
            }
            _ => {}
        }
    }
}

delegate_noop!(State: zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
delegate_noop!(State: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
delegate_noop!(State: wl_region::WlRegion);
