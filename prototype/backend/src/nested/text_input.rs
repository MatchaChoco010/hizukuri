//! Text input through the parent's input method: the window is a client of
//! the parent's `zwp_text_input_v3`.
//!
//! The parent sends `enter` while the window has its keyboard and it has an
//! input method; the backend then tells the core an input method is there
//! ([`BackendEvent::TextInputAvailable`]). The text input the core reports
//! as enabled inside ([`Backend::set_text_input`]) is enabled on the
//! parent, with its state, and what the parent's input method sends with
//! each `done` goes to the core ([`BackendEvent::TextCompose`]).
//!
//! What to send when is decided by [`TextInputBridge`], which knows nothing
//! of Wayland.
//!
//! [`Backend::set_text_input`]: crate::Backend::set_text_input

use wayland_client::protocol::{wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};

use super::client::State;
use crate::{BackendEvent, TextCompose, TextInputState};

/// A request on the parent's text input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Enable,
    Disable,
    SurroundingText(String, i32, i32),
    ChangeCause(u32),
    ContentType(u32, u32),
    CursorRectangle(i32, i32, i32, i32),
    Commit,
}

/// When to send what to the parent's text input (see the module
/// documentation). The cursor of the states it is given is in the
/// coordinates of the window's surface.
#[derive(Debug, Default)]
pub struct TextInputBridge {
    /// Between the parent's `enter` and `leave`: the window has the
    /// parent's keyboard, and the parent an input method.
    entered: bool,
    /// The text input enabled inside, as the core last said.
    wanted: Option<TextInputState>,
    /// What the parent was last given, committed: `None` while disabled.
    sent: Option<TextInputState>,
    /// Commits sent to the parent: what a `done` that answers the latest
    /// state carries.
    commits: u32,
}

impl TextInputBridge {
    /// Whether the parent's input method can be used.
    pub fn entered(&self) -> bool {
        self.entered
    }

    /// Whether a text input is enabled on the parent.
    pub fn enabled(&self) -> bool {
        self.sent.is_some()
    }

    pub fn commits(&self) -> u32 {
        self.commits
    }

    /// The text input enabled inside changed.
    pub fn set_wanted(&mut self, wanted: Option<TextInputState>) -> Vec<Request> {
        self.wanted = wanted;
        self.sync()
    }

    /// The parent's `enter`: what was enabled before is gone on its side.
    pub fn enter(&mut self) -> Vec<Request> {
        self.entered = true;
        self.sent = None;
        self.sync()
    }

    /// The parent's `leave`. It ignores requests until the next `enter`.
    pub fn leave(&mut self) {
        self.entered = false;
        self.sent = None;
    }

    /// The parent's `done`: whether it answers an older state than the one
    /// sent last.
    pub fn is_stale(&self, serial: u32) -> bool {
        serial != self.commits
    }

    fn sync(&mut self) -> Vec<Request> {
        if !self.entered {
            return Vec::new();
        }
        let mut requests = Vec::new();
        match (&self.sent, &self.wanted) {
            (None, None) => {}
            (Some(_), None) => requests.extend([Request::Disable, Request::Commit]),
            (Some(sent), Some(wanted)) if sent.activation == wanted.activation => {
                changes(Some(sent), wanted, &mut requests);
                if !requests.is_empty() {
                    requests.push(Request::Commit);
                }
            }
            // Another text field inside: the one before is disabled first.
            (Some(_), Some(wanted)) => {
                requests.extend([Request::Disable, Request::Commit, Request::Enable]);
                changes(None, wanted, &mut requests);
                requests.push(Request::Commit);
            }
            (None, Some(wanted)) => {
                requests.push(Request::Enable);
                changes(None, wanted, &mut requests);
                requests.push(Request::Commit);
            }
        }
        self.commits += requests.iter().filter(|r| **r == Request::Commit).count() as u32;
        self.sent = self.wanted.clone();
        requests
    }
}

/// The requests that bring the parent from `sent` (`None`: just enabled)
/// to `wanted`.
fn changes(sent: Option<&TextInputState>, wanted: &TextInputState, requests: &mut Vec<Request>) {
    if sent.is_none_or(|s| {
        s.content_hint != wanted.content_hint || s.content_purpose != wanted.content_purpose
    }) {
        requests.push(Request::ContentType(
            wanted.content_hint,
            wanted.content_purpose,
        ));
    }
    if let Some((text, cursor, anchor)) = &wanted.surrounding
        && sent.is_none_or(|s| {
            s.surrounding.as_ref() != wanted.surrounding.as_ref()
                || s.change_cause != wanted.change_cause
        })
    {
        requests.push(Request::SurroundingText(
            text.clone(),
            *cursor as i32,
            *anchor as i32,
        ));
        requests.push(Request::ChangeCause(wanted.change_cause));
    }
    if let Some((x, y, w, h)) = wanted.cursor
        && sent.is_none_or(|s| s.cursor != wanted.cursor)
    {
        requests.push(Request::CursorRectangle(x, y, w, h));
    }
}

/// The parent's text input and what the backend keeps about it.
#[derive(Default)]
pub(super) struct ParentTextInput {
    pub manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    object: Option<zwp_text_input_v3::ZwpTextInputV3>,
    bridge: TextInputBridge,
    /// What the parent sent since its last `done`.
    pending: TextCompose,
    /// `done` events received.
    dones: u64,
}

impl ParentTextInput {
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "bound": self.object.is_some(),
            "entered": self.bridge.entered(),
            "enabled": self.bridge.enabled(),
            "commits": self.bridge.commits(),
            "dones": self.dones,
        })
    }

    fn send(&self, requests: Vec<Request>) {
        let Some(object) = &self.object else {
            return;
        };
        for request in requests {
            match request {
                Request::Enable => object.enable(),
                Request::Disable => object.disable(),
                Request::SurroundingText(text, cursor, anchor) => {
                    object.set_surrounding_text(text, cursor, anchor);
                }
                Request::ChangeCause(cause) => {
                    if let Ok(cause) = zwp_text_input_v3::ChangeCause::try_from(cause) {
                        object.set_text_change_cause(cause);
                    }
                }
                Request::ContentType(hint, purpose) => object.set_content_type(
                    zwp_text_input_v3::ContentHint::from_bits_truncate(hint),
                    zwp_text_input_v3::ContentPurpose::try_from(purpose)
                        .unwrap_or(zwp_text_input_v3::ContentPurpose::Normal),
                ),
                Request::CursorRectangle(x, y, w, h) => object.set_cursor_rectangle(x, y, w, h),
                Request::Commit => object.commit(),
            }
        }
    }
}

impl State {
    /// Creates the parent's text input once its manager and the seat are
    /// there.
    pub(super) fn ensure_text_input(&mut self) {
        if self.text_input.object.is_some() {
            return;
        }
        if let (Some(manager), Some(seat)) = (&self.text_input.manager, &self.seat) {
            let seat: &wl_seat::WlSeat = seat;
            self.text_input.object = Some(manager.get_text_input(seat, &self.qh, ()));
        }
    }

    /// The text input enabled inside, with its cursor in the global logical
    /// space.
    pub fn set_text_input(&mut self, state: Option<&TextInputState>) {
        let origin = self
            .output
            .as_ref()
            .map_or((0, 0), |output| output.info.position);
        let wanted = state.map(|state| {
            let mut state = state.clone();
            state.cursor = state
                .cursor
                .map(|(x, y, w, h)| (x - origin.0, y - origin.1, w.max(1), h.max(1)));
            state
        });
        let requests = self.text_input.bridge.set_wanted(wanted);
        self.text_input.send(requests);
    }

    fn is_window(&self, surface: &wl_surface::WlSurface) -> bool {
        self.surface.as_ref() == Some(surface)
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_text_input_v3::Event;
        match event {
            Event::Enter { surface } if state.is_window(&surface) => {
                let requests = state.text_input.bridge.enter();
                state.text_input.send(requests);
                state.text_input.pending = TextCompose::default();
                state
                    .events
                    .push_back(BackendEvent::TextInputAvailable(true));
            }
            Event::Leave { surface } if state.is_window(&surface) => {
                state.text_input.bridge.leave();
                state.text_input.pending = TextCompose::default();
                state
                    .events
                    .push_back(BackendEvent::TextInputAvailable(false));
            }
            Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                state.text_input.pending.preedit = text.map(|t| (t, cursor_begin, cursor_end));
            }
            Event::CommitString { text } => state.text_input.pending.commit = text,
            Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.text_input.pending.delete = Some((before_length, after_length));
            }
            Event::Done { serial } => {
                let mut compose = std::mem::take(&mut state.text_input.pending);
                compose.stale = state.text_input.bridge.is_stale(serial);
                state.text_input.dones += 1;
                state.events.push_back(BackendEvent::TextCompose(compose));
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);

#[cfg(test)]
mod tests {
    use super::*;

    fn field(activation: u64, cursor: (i32, i32, i32, i32)) -> TextInputState {
        TextInputState {
            activation,
            surrounding: Some(("abc".to_string(), 3, 3)),
            change_cause: 0,
            content_hint: 0,
            content_purpose: 0,
            cursor: Some(cursor),
        }
    }

    #[test]
    fn nothing_is_sent_before_the_parent_enters() {
        let mut bridge = TextInputBridge::default();
        assert_eq!(bridge.set_wanted(Some(field(1, (1, 2, 3, 4)))), vec![]);
        assert_eq!(bridge.commits(), 0);
        // The parent's enter brings what is enabled inside.
        let requests = bridge.enter();
        assert_eq!(
            requests,
            vec![
                Request::Enable,
                Request::ContentType(0, 0),
                Request::SurroundingText("abc".to_string(), 3, 3),
                Request::ChangeCause(0),
                Request::CursorRectangle(1, 2, 3, 4),
                Request::Commit,
            ]
        );
        assert!(bridge.enabled());
        assert!(!bridge.is_stale(1));
        assert!(bridge.is_stale(0));
    }

    #[test]
    fn only_what_changed_is_sent() {
        let mut bridge = TextInputBridge::default();
        bridge.enter();
        bridge.set_wanted(Some(field(1, (1, 2, 3, 4))));
        assert_eq!(bridge.set_wanted(Some(field(1, (1, 2, 3, 4)))), vec![]);
        assert_eq!(
            bridge.set_wanted(Some(field(1, (5, 2, 3, 4)))),
            vec![Request::CursorRectangle(5, 2, 3, 4), Request::Commit]
        );
        assert_eq!(bridge.commits(), 2);
    }

    #[test]
    fn another_field_is_enabled_after_the_first_is_disabled() {
        let mut bridge = TextInputBridge::default();
        bridge.enter();
        bridge.set_wanted(Some(field(1, (1, 2, 3, 4))));
        let requests = bridge.set_wanted(Some(field(2, (1, 2, 3, 4))));
        assert_eq!(
            &requests[..3],
            &[Request::Disable, Request::Commit, Request::Enable]
        );
        assert_eq!(requests.last(), Some(&Request::Commit));
        assert_eq!(
            bridge.set_wanted(None),
            vec![Request::Disable, Request::Commit]
        );
        assert!(!bridge.enabled());
        assert_eq!(bridge.set_wanted(None), vec![]);
        assert_eq!(bridge.commits(), 4);
    }

    #[test]
    fn after_a_leave_the_next_enter_enables_again() {
        let mut bridge = TextInputBridge::default();
        bridge.enter();
        bridge.set_wanted(Some(field(1, (1, 2, 3, 4))));
        bridge.leave();
        assert!(!bridge.enabled());
        // Changes while left wait for the next enter.
        assert_eq!(bridge.set_wanted(Some(field(1, (9, 9, 3, 4)))), vec![]);
        let requests = bridge.enter();
        assert_eq!(requests.first(), Some(&Request::Enable));
        assert!(requests.contains(&Request::CursorRectangle(9, 9, 3, 4)));
        assert_eq!(bridge.commits(), 2);
    }
}
