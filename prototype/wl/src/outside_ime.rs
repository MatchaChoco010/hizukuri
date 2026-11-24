//! Text input served by an input method outside the compositor: the one of
//! a parent compositor, when the compositor runs as its window.
//!
//! While no input method client is bound inside, the compositor itself acts
//! as the seat's input method (Smithay's `set_compositor_input_method`):
//! applications get `enter` and can enable their text inputs, and what the
//! input method outside composes is sent into the enabled one. An input
//! method bound inside takes precedence: while there is one, the text
//! inputs are its own, as without an input method outside.
//!
//! Smithay hands the state an application commits on its text input
//! (surrounding text, content type, cursor rectangle) only to an input
//! method client inside, and keeps none of it. To have it for the input
//! method outside, the requests of every text input are watched on their
//! way to Smithay: the object's dispatcher is replaced by one that reads
//! the request, puts Smithay's back for the time Smithay handles it, and
//! takes its place again afterwards.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::protocol::{Argument, Message};
use smithay::reexports::wayland_server::backend::{ClientId, Handle, ObjectData, ObjectId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::input_method::InputMethodSeat;
use smithay::wayland::text_input::TextInputSeat;

use crate::state::State;
use crate::surface::surface_id;
use crate::{SurfaceId, WindowId};

/// What an application last committed on its enabled text input. Lengths
/// and positions are in bytes of UTF-8, as text-input-v3 has them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInputContent {
    /// The text around the cursor, the cursor and the anchor.
    pub surrounding: Option<(String, u32, u32)>,
    /// `zwp_text_input_v3.change_cause`.
    pub change_cause: u32,
    pub content_hint: u32,
    pub content_purpose: u32,
    /// The cursor rectangle (x, y, width, height), relative to the surface
    /// of the text input.
    pub cursor: Option<(i32, i32, i32, i32)>,
}

/// The enabled text input, while the input method outside serves the seat
/// ([`crate::WlServer::outside_text_input`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideTextInput {
    pub window: WindowId,
    /// The surface the text input is on (the one with the keyboard focus).
    pub surface: SurfaceId,
    /// Counts the times a text input was enabled: a new value is another
    /// text field, whose state starts afresh.
    pub activation: u64,
    pub content: TextInputContent,
}

/// What the input method outside sent for the enabled text input, all at
/// once (its `done`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutsideCompose {
    /// The text being composed, with the cursor inside it (begin, end, in
    /// bytes). `None`: no text is being composed.
    pub preedit: Option<(String, i32, i32)>,
    /// Finished text.
    pub commit: Option<String>,
    /// Bytes to delete before and after the cursor.
    pub delete: Option<(u32, u32)>,
    /// The input method worked from a state the application has changed
    /// since: the text is applied, the state is not.
    pub stale: bool,
}

/// Requests of a text input since its last commit.
#[derive(Debug, Default)]
struct Pending {
    enable: Option<bool>,
    surrounding: Option<(String, u32, u32)>,
    change_cause: Option<u32>,
    content_type: Option<(u32, u32)>,
    cursor: Option<(i32, i32, i32, i32)>,
}

pub(crate) struct OutsideImeState {
    /// The core said an input method outside is there.
    wanted: bool,
    /// The compositor acts as the seat's input method for it now: wanted,
    /// and none is bound inside.
    serving: bool,
    /// The text inputs whose requests are watched.
    watched: HashMap<ObjectId, Pending>,
    /// The enabled text input and its state, as Smithay has it: enabled by
    /// a commit, disabled by a commit or by the focus moving.
    active: Option<(ObjectId, TextInputContent)>,
    activations: u64,
    /// The text inputs of the focused client that Smithay sent `enter`
    /// to: those that were there when the focus or the input method last
    /// changed.
    entered: Vec<ObjectId>,
}

impl OutsideImeState {
    pub(crate) fn new() -> Self {
        Self {
            wanted: false,
            serving: false,
            watched: HashMap::new(),
            active: None,
            activations: 0,
            entered: Vec::new(),
        }
    }
}

/// The dispatcher put on a text input: reads each request, then hands it
/// to Smithay's.
struct WatchedTextInput {
    smithay: Arc<dyn ObjectData<State>>,
}

impl ObjectData<State> for WatchedTextInput {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut State,
        client_id: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<State>>> {
        let id = msg.sender_id.clone();
        state.observe_text_input(&id, &msg);
        // Smithay finds its user data through the object's dispatcher:
        // it has to be Smithay's own while Smithay handles the request.
        let _ = handle.set_object_data(id.clone(), self.smithay.clone());
        let created = self.smithay.clone().request(handle, state, client_id, msg);
        let _ = handle.set_object_data(id, self.clone());
        created
    }

    fn destroyed(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut State,
        client_id: ClientId,
        object_id: ObjectId,
    ) {
        state.outside_ime.watched.remove(&object_id);
        if state
            .outside_ime
            .active
            .as_ref()
            .is_some_and(|(id, _)| *id == object_id)
        {
            state.outside_ime.active = None;
        }
        self.smithay
            .clone()
            .destroyed(handle, state, client_id, object_id);
    }
}

fn text(argument: &Argument<ObjectId, OwnedFd>) -> Option<String> {
    match argument {
        Argument::Str(text) => Some(
            text.as_ref()
                .map(|t| t.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

impl State {
    /// Puts the watching dispatcher on the text inputs of the client with
    /// the keyboard focus that do not have it yet. Only the focused
    /// client's requests count; a client that gets the focus is caught
    /// before its next requests are read.
    pub(crate) fn watch_text_inputs(&mut self) -> usize {
        let handle = self.display_handle.backend_handle();
        let mut added = self.focused_text_inputs();
        added.retain(|id| !self.outside_ime.watched.contains_key(id));
        for id in &added {
            let Ok(smithay) = handle.get_object_data::<State>(id.clone()) else {
                continue;
            };
            if smithay.downcast_ref::<WatchedTextInput>().is_some() {
                continue;
            }
            let watched: Arc<dyn ObjectData<State>> = Arc::new(WatchedTextInput { smithay });
            if handle.set_object_data(id.clone(), watched).is_ok() {
                self.outside_ime
                    .watched
                    .insert(id.clone(), Pending::default());
            }
        }
        // Smithay sends `enter` to a text input created after its client
        // got the focus only when an input method client is bound: one the
        // compositor serves gets it here.
        if self.outside_ime.serving {
            let late: Vec<ObjectId> = added
                .iter()
                .filter(|id| !self.outside_ime.entered.contains(id))
                .cloned()
                .collect();
            if !late.is_empty() {
                self.seat
                    .text_input()
                    .with_focused_text_input(|text_input, surface| {
                        if late.contains(&text_input.id()) {
                            text_input.enter(surface);
                        }
                    });
                self.outside_ime.entered.extend(late);
            }
        }
        added.len()
    }

    /// The text inputs of the client with the keyboard focus.
    fn focused_text_inputs(&self) -> Vec<ObjectId> {
        let mut ids = Vec::new();
        self.seat
            .text_input()
            .with_focused_text_input(|text_input, _| ids.push(text_input.id()));
        ids
    }

    /// Whether Smithay handles a request of this text input: an input
    /// method is there (inside, or the compositor itself) and its client
    /// has the keyboard focus.
    fn text_input_counts(&self, id: &ObjectId) -> bool {
        let text_input = self.seat.text_input();
        (self.seat.input_method().has_instance() || text_input.compositor_input_method())
            && text_input
                .focus()
                .is_some_and(|focus| focus.id().same_client_as(id))
    }

    /// A request of a watched text input, before Smithay sees it.
    fn observe_text_input(&mut self, id: &ObjectId, msg: &Message<ObjectId, OwnedFd>) {
        if !self.text_input_counts(id) {
            return;
        }
        let Some(pending) = self.outside_ime.watched.get_mut(id) else {
            return;
        };
        // The requests of zwp_text_input_v3, by opcode.
        match (msg.opcode, msg.args.as_slice()) {
            (1, _) => pending.enable = Some(true),
            (2, _) => pending.enable = Some(false),
            (3, [t, Argument::Int(cursor), Argument::Int(anchor)]) => {
                pending.surrounding = text(t).map(|t| (t, *cursor as u32, *anchor as u32));
            }
            (4, [Argument::Uint(cause)]) => pending.change_cause = Some(*cause),
            (5, [Argument::Uint(hint), Argument::Uint(purpose)]) => {
                pending.content_type = Some((*hint, *purpose));
            }
            (
                6,
                [
                    Argument::Int(x),
                    Argument::Int(y),
                    Argument::Int(w),
                    Argument::Int(h),
                ],
            ) => {
                // Smithay keeps no empty rectangle either.
                if *w > 0 && *h > 0 {
                    pending.cursor = Some((*x, *y, *w, *h));
                }
            }
            (7, _) => {
                let pending = std::mem::take(pending);
                self.commit_text_input(id, pending);
            }
            _ => {}
        }
    }

    /// A commit, taken the way Smithay takes it: only the enabled text
    /// input's counts, and an enable while none is enabled.
    fn commit_text_input(&mut self, id: &ObjectId, pending: Pending) {
        let ime = &mut self.outside_ime;
        let active = ime.active.as_ref().map(|(active, _)| active.clone());
        if active.as_ref().is_some_and(|active| active != id) {
            return;
        }
        match pending.enable {
            Some(true) => {
                ime.activations += 1;
                ime.active = Some((id.clone(), TextInputContent::default()));
            }
            Some(false) => {
                ime.active = None;
                return;
            }
            None if active.is_none() => return,
            None => {}
        }
        let Some((_, content)) = &mut ime.active else {
            return;
        };
        if let Some(surrounding) = pending.surrounding {
            content.surrounding = Some(surrounding);
        }
        if let Some(cause) = pending.change_cause {
            content.change_cause = cause;
        }
        if let Some((hint, purpose)) = pending.content_type {
            content.content_hint = hint;
            content.content_purpose = purpose;
        }
        if let Some(cursor) = pending.cursor {
            content.cursor = Some(cursor);
        }
    }

    /// Smithay enabled a text input. One that was created and enabled in
    /// the same round was not watched yet: it is from now on, and taken as
    /// the enabled one, with the content type that came with the enable.
    pub(crate) fn outside_ime_activated(&mut self, content_type: Option<(u32, u32)>) {
        if self.outside_ime.active.is_some() {
            return;
        }
        let before: Vec<ObjectId> = self.outside_ime.watched.keys().cloned().collect();
        self.watch_text_inputs();
        let fresh: Vec<ObjectId> = self
            .outside_ime
            .watched
            .keys()
            .filter(|id| !before.contains(id))
            .cloned()
            .collect();
        if let [id] = fresh.as_slice() {
            let (hint, purpose) = content_type.unwrap_or((0, 0));
            self.outside_ime.activations += 1;
            self.outside_ime.active = Some((
                id.clone(),
                TextInputContent {
                    content_hint: hint,
                    content_purpose: purpose,
                    ..Default::default()
                },
            ));
        }
    }

    /// The keyboard focus moved: Smithay let go of the enabled text input.
    pub(crate) fn outside_ime_focus_moved(&mut self) {
        self.outside_ime.active = None;
        self.outside_ime.entered = self.focused_text_inputs();
    }

    /// Says whether an input method outside is there.
    pub(crate) fn set_outside_input_method(&mut self, available: bool) {
        self.outside_ime.wanted = available;
        self.reconcile_outside_ime();
    }

    /// Makes the compositor the seat's input method while one outside is
    /// wanted and none is bound inside, and stops it otherwise.
    pub(crate) fn reconcile_outside_ime(&mut self) {
        let inside = self.seat.input_method().has_instance();
        let serving = self.outside_ime.wanted && !inside;
        if serving == self.outside_ime.serving {
            return;
        }
        self.outside_ime.serving = serving;
        self.outside_ime.active = None;
        let text_input = self.seat.text_input();
        // Turning it on sends `enter`, turning it off `leave`, for the
        // text inputs of the focused client.
        text_input.set_compositor_input_method(serving);
        if !serving && inside {
            // An input method that binds inside sends its own `enter`
            // first; the `leave` above undid it.
            text_input.enter();
        }
        self.outside_ime.entered = self.focused_text_inputs();
        self.watch_text_inputs();
        tracing::info!(
            inside,
            "text input goes to the input method {}",
            if serving {
                "outside"
            } else {
                "inside (if any)"
            }
        );
    }

    /// The enabled text input, while the input method outside serves the
    /// seat.
    pub(crate) fn outside_text_input(&self) -> Option<OutsideTextInput> {
        if !self.outside_ime.serving {
            return None;
        }
        let (_, content) = self.outside_ime.active.as_ref()?;
        let surface: WlSurface = self.seat.text_input().focus()?;
        Some(OutsideTextInput {
            window: self.owning_window(&surface)?,
            surface: surface_id(&surface)?,
            activation: self.outside_ime.activations,
            content: content.clone(),
        })
    }

    /// Sends what the input method outside composed into the enabled text
    /// input.
    pub(crate) fn outside_input_method_compose(&mut self, compose: &OutsideCompose) {
        if !self.outside_ime.serving || self.outside_ime.active.is_none() {
            return;
        }
        let text_input = self.seat.text_input();
        text_input.with_active_text_input(|text_input, _| {
            if let Some((before, after)) = compose.delete {
                text_input.delete_surrounding_text(before, after);
            }
            if let Some(text) = &compose.commit {
                text_input.commit_string(Some(text.clone()));
            }
            if let Some((text, begin, end)) = &compose.preedit {
                text_input.preedit_string(Some(text.clone()), *begin, *end);
            }
        });
        text_input.done(compose.stale);
    }

    /// Whether the compositor acts as the input method for one outside.
    pub(crate) fn outside_input_method_serving(&self) -> bool {
        self.outside_ime.serving
    }
}
