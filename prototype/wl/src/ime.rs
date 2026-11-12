//! Input methods: `zwp_text_input_v3` (applications), `zwp_input_method_v2`
//! (the input method: its keyboard grab and its popup) and
//! `zwp_virtual_keyboard_v1` (keys the input method hands back).
//!
//! Smithay copies the state between the two sides of the protocol. What is
//! left here is what Smithay leaves to the compositor:
//!
//! - where the popup of the input method (the candidate window) goes. It is
//!   shown as a surface of the window that has the text input, below the
//!   cursor rectangle the application reported, or above it when there is
//!   no room below inside the window's popup bounds;
//! - what a key of a virtual keyboard does. Keys of the input method that
//!   holds the keyboard grab go to the focused client past the grab (back
//!   into the grab they would loop); keys of any other virtual keyboard are
//!   fed in like those of a real keyboard, under their own keymap.

use std::collections::HashMap;

use smithay::backend::input::{Event as _, InputEvent, KeyState, KeyboardKeyEvent as _};
use smithay::input::keyboard::{FilterResult, KeymapFile};
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{
    ContentHint, ContentPurpose,
};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, SERIAL_COUNTER};
use smithay::wayland::input_method::{InputMethodHandler, InputMethodSeat, PopupSurface};
use smithay::wayland::text_input::TextInputActivation;
use smithay::wayland::virtual_keyboard::{
    VirtualKeyboardBackend, VirtualKeyboardDevice, VirtualKeyboardHandler,
    VirtualKeyboardSpecialEvent,
};

use crate::state::State;
use crate::surface::{collect_tree, surface_id, surface_size};
use crate::{InputMethodStatus, InputPopupStatus, SurfaceNode, SurfaceRole, WindowId, WlEvent};

/// What the protocol layer keeps about input methods.
pub(crate) struct ImeState {
    /// The popup of the input method, while it has a parent (a text input
    /// with the focus). Smithay hands out a new copy of the handle each
    /// time the parent changes; this is the latest.
    popup: Option<PopupSurface>,
    /// The cursor rectangle last sent to the popup, relative to the popup
    /// surface, and where the popup was then.
    sent_rectangle: Option<(i32, i32, i32, i32)>,
    /// Whether the text input with the focus is enabled: between Smithay's
    /// `activated` and `deactivated`, or a change of the keyboard focus
    /// (which deactivates without telling).
    active: bool,
    /// The keymap of the seat. A virtual keyboard with a keymap of its own
    /// replaces it while its keys are handled; a key of a real keyboard
    /// brings it back.
    default_keymap: Option<KeymapFile>,
    foreign_keymap: bool,
    /// Keys of a virtual keyboard that were passed to the focused client
    /// past the grab, by device: their release goes the same way, even
    /// when the grab is gone by then.
    bypassed: HashMap<String, Vec<u32>>,
}

impl ImeState {
    pub(crate) fn new() -> Self {
        Self {
            popup: None,
            sent_rectangle: None,
            active: false,
            default_keymap: None,
            foreign_keymap: false,
            bypassed: HashMap::new(),
        }
    }
}

/// Where the popup of the input method is shown.
struct Placement {
    window: WindowId,
    /// The popup surface's top left corner, relative to the window
    /// geometry.
    offset: (i32, i32),
    /// The cursor rectangle of the text input, relative to the window
    /// geometry.
    cursor: (i32, i32, i32, i32),
}

impl State {
    pub(crate) fn init_ime(&mut self) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            self.ime.default_keymap = Some(keyboard.keymap_file());
        }
    }

    /// The parent of the input method's popup, while the popup is to be
    /// seen: the input method is active and the popup has a parent.
    ///
    /// Smithay calls the popup hooks of [`InputMethodHandler`] while it
    /// holds the lock of the seat's input method, so this must not ask the
    /// input method anything (it would wait on that lock forever). An input
    /// method that went away took its popup surface with it.
    pub(crate) fn input_popup_parent(&self) -> Option<WlSurface> {
        let popup = self.ime.popup.as_ref()?;
        if !self.ime.active || !popup.alive() {
            return None;
        }
        let parent = &popup.get_parent()?.surface;
        parent.is_alive().then(|| parent.clone())
    }

    /// The popup surface of the input method, while it is to be seen.
    pub(crate) fn shown_input_popup(&self) -> Option<WlSurface> {
        self.input_popup_parent()?;
        self.ime.popup.as_ref().map(|p| p.wl_surface().clone())
    }

    fn ime_active(&self) -> bool {
        self.ime.active && self.seat.input_method().has_instance()
    }

    /// The keyboard focus moved: Smithay has deactivated the input method.
    pub(crate) fn ime_focus_moved(&mut self) {
        if self.ime.active {
            let window = self.input_popup_window();
            self.ime.active = false;
            self.note_input_popup(window);
        }
    }

    fn input_popup_window(&self) -> Option<WindowId> {
        self.owning_window(&self.input_popup_parent()?)
    }

    /// The popup was shown, hidden or moved: the window it is (or was)
    /// shown with has to be drawn again.
    fn note_input_popup(&mut self, before: Option<WindowId>) {
        let after = self.input_popup_window();
        for window in [before, after].into_iter().flatten() {
            if self.events.last() != Some(&WlEvent::WindowChanged(window)) {
                self.events.push(WlEvent::WindowChanged(window));
            }
        }
        if let Some(surface) = self.shown_input_popup() {
            self.announce_outputs(&surface);
        }
    }

    /// Where the popup goes: below the cursor rectangle, or above it when
    /// it does not fit below the bounds of the window's popups and does
    /// above; moved sideways to stay inside them.
    fn input_popup_placement(&self, nodes: Option<&[SurfaceNode]>) -> Option<Placement> {
        let parent = self.input_popup_parent()?;
        let popup = self.ime.popup.as_ref()?;
        let window = self.owning_window(&parent)?;
        let parent_id = surface_id(&parent)?;
        let owned;
        let nodes = match nodes {
            Some(nodes) => nodes,
            None => {
                owned = self.window_surfaces_without_input_popup(window);
                &owned
            }
        };
        let parent_offset = nodes.iter().find(|n| n.surface == parent_id)?.offset;
        let rect = popup.text_input_rectangle();
        let cursor = (
            parent_offset.0 + rect.loc.x,
            parent_offset.1 + rect.loc.y,
            rect.size.w,
            rect.size.h,
        );
        let (width, height) = surface_size(popup.wl_surface());
        let (mut x, mut y) = (cursor.0, cursor.1 + cursor.3);
        if let Some(&(bx, by, bw, bh)) = self.popup_bounds.get(&window) {
            if y + height > by + bh && cursor.1 - height >= by {
                y = cursor.1 - height;
            }
            x = x.min(bx + bw - width).max(bx);
        }
        Some(Placement {
            window,
            offset: (x, y),
            cursor,
        })
    }

    /// Appends the popup of the input method to the surfaces of `window`,
    /// in front of everything else, when it is shown with that window.
    pub(crate) fn collect_input_popup(&self, window: WindowId, nodes: &mut Vec<SurfaceNode>) {
        let Some(placement) = self.input_popup_placement(Some(nodes)) else {
            return;
        };
        if placement.window != window {
            return;
        }
        if let Some(popup) = &self.ime.popup {
            collect_tree(
                popup.wl_surface(),
                placement.offset,
                SurfaceRole::InputPopup,
                nodes,
            );
        }
    }

    /// Tells the input method where the text being entered is, relative to
    /// its popup surface (`zwp_input_popup_surface_v2.text_input_rectangle`)
    /// when that changed. Smithay sends the rectangle as the application
    /// reported it, relative to the application's surface; this follows it
    /// with the one the protocol asks for.
    pub(crate) fn send_input_popup_rectangle(&mut self) {
        let Some(placement) = self.input_popup_placement(None) else {
            return;
        };
        let rectangle = (
            placement.cursor.0 - placement.offset.0,
            placement.cursor.1 - placement.offset.1,
            placement.cursor.2,
            placement.cursor.3,
        );
        if self.ime.sent_rectangle == Some(rectangle) {
            return;
        }
        if let Some(popup) = &self.ime.popup {
            popup.surface_role.text_input_rectangle(
                rectangle.0,
                rectangle.1,
                rectangle.2,
                rectangle.3,
            );
            self.ime.sent_rectangle = Some(rectangle);
        }
    }

    pub(crate) fn input_method_status(&self) -> InputMethodStatus {
        let input_method = self.seat.input_method();
        let popup = self
            .ime
            .popup
            .as_ref()
            .filter(|p| p.alive())
            .and_then(|popup| {
                let placement = self.input_popup_placement(None);
                let rect = popup.text_input_rectangle();
                Some(InputPopupStatus {
                    surface: surface_id(popup.wl_surface())?,
                    shown: placement.is_some(),
                    window: placement.as_ref().map(|p| p.window),
                    position: placement.as_ref().map(|p| p.offset),
                    cursor_rectangle: (rect.loc.x, rect.loc.y, rect.size.w, rect.size.h),
                    sent_rectangle: self.ime.sent_rectangle,
                })
            });
        InputMethodStatus {
            bound: input_method.has_instance(),
            active: self.ime_active(),
            keyboard_grab: input_method.keyboard_grabbed(),
            popup,
        }
    }

    /// A key of a real keyboard is about to be handled: a keymap a virtual
    /// keyboard brought in gives way to the seat's own.
    pub(crate) fn restore_keymap(&mut self) {
        if !std::mem::take(&mut self.ime.foreign_keymap) {
            return;
        }
        if let (Some(keyboard), Some(keymap)) =
            (self.seat.get_keyboard(), self.ime.default_keymap.clone())
            && let Err(error) = keyboard.set_keymap_from_keymap_file(self, keymap)
        {
            tracing::warn!(?error, "failed to bring back the keymap of the seat");
        }
    }

    /// Whether a virtual keyboard belongs to the input method that holds
    /// the keyboard grab.
    fn belongs_to_grab_holder(&self, device: &VirtualKeyboardDevice) -> bool {
        let holder = self.seat.input_method().keyboard_grab_client();
        holder.is_some() && holder == device.client()
    }
}

impl TextInputActivation for State {
    fn activated(&mut self, _content_type: Option<(ContentHint, ContentPurpose)>) {
        let before = self.input_popup_window();
        self.ime.active = true;
        self.note_input_popup(before);
    }

    fn deactivated(&mut self) {
        let before = self.input_popup_window();
        self.ime.active = false;
        self.note_input_popup(before);
    }
}

impl InputMethodHandler for State {
    fn new_popup(&mut self, surface: PopupSurface) {
        let before = self.input_popup_window();
        self.ime.popup = Some(surface);
        self.ime.sent_rectangle = None;
        self.note_input_popup(before);
    }

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        let before = self.input_popup_window();
        if self.ime.popup.as_ref() == Some(&surface) {
            self.ime.popup = None;
            self.ime.sent_rectangle = None;
        }
        self.note_input_popup(before);
    }

    fn popup_repositioned(&mut self, surface: PopupSurface) {
        // The application moved its cursor. Smithay has just sent the
        // rectangle the way it came; send the right one at the next flush.
        let before = self.input_popup_window();
        if self.ime.popup.as_ref() == Some(&surface) {
            self.ime.sent_rectangle = None;
        }
        self.note_input_popup(before);
    }

    fn parent_geometry(&self, _parent: &WlSurface) -> Rectangle<i32, Logical> {
        // Smithay keeps this with the popup's parent and never uses it;
        // the place is worked out from the window's surfaces when it is
        // needed (`input_popup_placement`).
        Rectangle::default()
    }
}

impl VirtualKeyboardHandler for State {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        match event {
            InputEvent::DeviceAdded { device }
            | InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { device }) => {
                // The input method sends back the keymap of its grab, which
                // is the seat's: its keys are read under that one.
                if self.belongs_to_grab_holder(&device) {
                    return;
                }
                if let Some(keymap) = device.keymap()
                    && keymap.id() != keyboard.keymap_file().id()
                {
                    match keyboard.set_keymap_from_keymap_file(self, keymap) {
                        Ok(()) => self.ime.foreign_keymap = true,
                        Err(error) => {
                            tracing::warn!(?error, "the keymap of a virtual keyboard is unusable")
                        }
                    }
                }
            }
            InputEvent::Keyboard { event } => {
                let device = event.device();
                let id = smithay::backend::input::Device::id(&device);
                let keycode = event.key_code();
                let key_state = event.state();
                let time = event.time();
                let serial = SERIAL_COUNTER.next_serial();
                let raw = keycode.raw();
                let held_past_grab = self.ime.bypassed.get(&id).is_some_and(|k| k.contains(&raw));
                let past_grab = match key_state {
                    KeyState::Pressed => self.belongs_to_grab_holder(&device),
                    KeyState::Released => held_past_grab,
                };
                if past_grab {
                    let keys = self.ime.bypassed.entry(id.clone()).or_default();
                    match key_state {
                        KeyState::Pressed => keys.push(raw),
                        KeyState::Released => keys.retain(|k| *k != raw),
                    }
                    if keys.is_empty() {
                        self.ime.bypassed.remove(&id);
                    }
                    // The client's modifiers may lag behind: while the grab
                    // holds the keyboard, they went to the grab. They are
                    // sent before the key, which is read with them (with
                    // `mods_changed`, Smithay sends them after the key).
                    keyboard.advertise_modifier_state(self);
                    keyboard.input_forward_bypassing_grab(
                        self, keycode, key_state, serial, time, false,
                    );
                } else {
                    if let Some(keymap) = device.keymap()
                        && keymap.id() != keyboard.keymap_file().id()
                        && keyboard.set_keymap_from_keymap_file(self, keymap).is_ok()
                    {
                        self.ime.foreign_keymap = true;
                    }
                    keyboard.input_from_source::<(), _>(
                        device.source(),
                        self,
                        keycode,
                        key_state,
                        serial,
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers {
                device,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            }) => {
                // An input method mirrors the modifiers of the grab, which
                // are the seat's own: nothing changes then.
                let _ = device;
                keyboard.with_xkb_state(self, |mut context| {
                    context.set_modifier_mask(mods_depressed, mods_latched, mods_locked, group)
                });
            }
            InputEvent::DeviceRemoved { device } => {
                // Smithay released the keys that were still down just
                // before; what is left of them was fed in as a real
                // keyboard's.
                self.ime
                    .bypassed
                    .remove(&smithay::backend::input::Device::id(&device));
                keyboard.release_source(self, device.source());
            }
            _ => {}
        }
    }
}

/// Whether a surface is the popup of the input method.
pub(crate) fn is_input_popup(state: &State, surface: &WlSurface) -> bool {
    state
        .ime
        .popup
        .as_ref()
        .is_some_and(|p| p.wl_surface().id() == surface.id())
}
