//! Text input and input methods: the client as an application with a text
//! field (`zwp_text_input_v3`), and the client as the input method itself
//! (`zwp_input_method_v2` with its keyboard grab and popup surface, and
//! `zwp_virtual_keyboard_v1` for the keys it hands back).
//!
//! Both roles are driven by the test, request by request. The input method
//! can also run on its own ([`InputMethodMode::Echo`]), the way a real one
//! does: it grabs the keyboard while it is active, collects letters as the
//! text being composed, commits it in capitals on Return, and hands every
//! other key back through its virtual keyboard.

use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use serde::Serialize;
use wayland_client::protocol::{wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2, zwp_input_method_manager_v2, zwp_input_method_v2,
    zwp_input_popup_surface_v2,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use xkbcommon::xkb;

use crate::{ClientError, ClientEvent, PixelLayout, State, TestClient, create_buffer, features};

/// How the client acts as an input method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMethodMode {
    /// Only what the test asks for.
    Manual,
    /// On its own, like a real input method (see the module documentation).
    Echo,
}

/// What the input method side receives, beyond the events of the
/// application side ([`ClientEvent`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ImeEvent {
    Activate,
    Deactivate,
    SurroundingText {
        text: String,
        cursor: u32,
        anchor: u32,
    },
    ContentType {
        hint: u32,
        purpose: u32,
    },
    TextChangeCause {
        cause: u32,
    },
    /// `zwp_input_method_v2.done`, with how many have been received.
    Done {
        count: u32,
    },
    Unavailable,
    GrabKeymap {
        size: u32,
    },
    GrabKey {
        keycode: u32,
        pressed: bool,
        keysym: String,
    },
    GrabModifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
    GrabRepeatInfo {
        rate: i32,
        delay: i32,
    },
    /// `zwp_input_popup_surface_v2.text_input_rectangle`.
    PopupRectangle {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
}

/// The popup surface of the input method.
pub(crate) struct ImePopup {
    surface: wl_surface::WlSurface,
    role: zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
}

#[derive(Default)]
pub(crate) struct ImeState {
    pub text_input_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    pub text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    /// The surface the text input was entered on, if any.
    pub text_input_entered: bool,
    /// Commits sent on the text input: what `done` should carry.
    pub text_input_commits: u32,

    pub input_method_manager: Option<zwp_input_method_manager_v2::ZwpInputMethodManagerV2>,
    pub virtual_keyboard_manager:
        Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    pub input_method: Option<zwp_input_method_v2::ZwpInputMethodV2>,
    pub grab: Option<zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2>,
    pub virtual_keyboard: Option<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1>,
    pub popup: Option<ImePopup>,
    /// The keymap the grab was given, and an xkb state to read its keys.
    pub keymap: Option<Vec<u8>>,
    pub xkb: Option<xkb::State>,
    /// `done` events received.
    pub done_count: u32,
    /// `activate` and `deactivate` are double buffered: this is what the
    /// next `done` makes current.
    pub pending_active: Option<bool>,
    pub active: bool,
    /// The text being composed in echo mode.
    pub composing: String,
    pub events: Vec<ImeEvent>,
}

impl State {
    /// Binds the globals of text input and input methods. Returns whether
    /// `interface` was one of them.
    pub(crate) fn bind_ime_global(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        interface: &str,
        handle: &QueueHandle<State>,
    ) -> bool {
        match interface {
            "zwp_text_input_manager_v3"
                if self.options.text_input || self.options.text_input_enable_on_enter.is_some() =>
            {
                self.ime.text_input_manager = Some(registry.bind(name, 1, handle, ()));
            }
            "zwp_input_method_manager_v2" if self.options.input_method.is_some() => {
                self.ime.input_method_manager = Some(registry.bind(name, 1, handle, ()));
            }
            "zwp_virtual_keyboard_manager_v1" if self.options.input_method.is_some() => {
                self.ime.virtual_keyboard_manager = Some(registry.bind(name, 1, handle, ()));
            }
            "zwp_text_input_manager_v3"
            | "zwp_input_method_manager_v2"
            | "zwp_virtual_keyboard_manager_v1" => {}
            _ => return false,
        }
        true
    }

    /// Creates the text input and the input method once their globals and
    /// the seat are there.
    pub(crate) fn advance_ime(&mut self, handle: &QueueHandle<State>) {
        let Some(seat) = &self.globals.seat else {
            return;
        };
        if self.ime.text_input.is_none()
            && let Some(manager) = &self.ime.text_input_manager
        {
            self.ime.text_input = Some(manager.get_text_input(seat, handle, ()));
        }
        if self.ime.input_method.is_none()
            && let Some(manager) = &self.ime.input_method_manager
        {
            self.ime.input_method = Some(manager.get_input_method(seat, handle, ()));
        }
    }

    /// Creates the virtual keyboard, with the keymap of the grab. Nothing
    /// happens before the grab has a keymap.
    fn ensure_virtual_keyboard(&mut self, handle: &QueueHandle<State>) -> Result<(), ClientError> {
        if self.ime.virtual_keyboard.is_some() {
            return Ok(());
        }
        let (Some(manager), Some(seat), Some(keymap)) = (
            &self.ime.virtual_keyboard_manager,
            &self.globals.seat,
            &self.ime.keymap,
        ) else {
            return Err(ClientError::Protocol(
                "the virtual keyboard needs its global and the keymap of the grab".to_string(),
            ));
        };
        let keyboard = manager.create_virtual_keyboard(seat, handle, ());
        let file = features::memory_file(keymap)?;
        keyboard.keymap(
            wayland_client::protocol::wl_keyboard::KeymapFormat::XkbV1 as u32,
            file.as_fd(),
            keymap.len() as u32,
        );
        self.ime.virtual_keyboard = Some(keyboard);
        Ok(())
    }

    fn ime_commit(&mut self) {
        if let Some(input_method) = &self.ime.input_method {
            input_method.commit(self.ime.done_count);
        }
    }

    /// The echo input method's answer to a key of the grab.
    fn echo_key(&mut self, keycode: u32, pressed: bool, handle: &QueueHandle<State>) {
        let keysym = self
            .ime
            .xkb
            .as_ref()
            .map(|s| s.key_get_one_sym(xkb::Keycode::new(keycode + 8)))
            .unwrap_or(xkb::Keysym::NoSymbol);
        let letter =
            char::from_u32(xkb::keysym_to_utf32(keysym)).filter(|c| c.is_ascii_lowercase());
        let composing = !self.ime.composing.is_empty();
        let used = match (letter, keysym) {
            (Some(letter), _) => {
                if pressed {
                    self.ime.composing.push(letter);
                }
                true
            }
            (None, xkb::Keysym::Return) if composing => {
                if pressed {
                    let text = std::mem::take(&mut self.ime.composing).to_uppercase();
                    if let Some(input_method) = &self.ime.input_method {
                        input_method.commit_string(text);
                    }
                }
                true
            }
            (None, xkb::Keysym::BackSpace) if composing => {
                if pressed {
                    self.ime.composing.pop();
                }
                true
            }
            _ => false,
        };
        if used {
            if pressed {
                let text = self.ime.composing.clone();
                let end = text.len() as i32;
                if let Some(input_method) = &self.ime.input_method {
                    input_method.set_preedit_string(text, end, end);
                }
                self.ime_commit();
                if !self.ime.composing.is_empty() && self.ime.popup.is_none() {
                    let _ = self.show_ime_popup(handle, 120, 30, [40, 160, 220, 255]);
                }
            }
            return;
        }
        if self.ensure_virtual_keyboard(handle).is_ok()
            && let Some(keyboard) = &self.ime.virtual_keyboard
        {
            keyboard.key(0, keycode, u32::from(pressed));
        }
    }

    fn show_ime_popup(
        &mut self,
        handle: &QueueHandle<State>,
        width: u32,
        height: u32,
        color: [u8; 4],
    ) -> Result<(), ClientError> {
        let (Some(compositor), Some(shm), Some(input_method)) = (
            &self.globals.compositor,
            &self.globals.shm,
            &self.ime.input_method,
        ) else {
            return Err(ClientError::Protocol(
                "the popup needs wl_compositor, wl_shm and the input method".to_string(),
            ));
        };
        let surface = compositor.create_surface(handle, ());
        let role = input_method.get_input_popup_surface(&surface, handle, ());
        let buffer = create_buffer(shm, handle, width, height, color, PixelLayout::Argb8888)?;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();
        self.ime.popup = Some(ImePopup { surface, role });
        Ok(())
    }
}

impl TestClient {
    // -- the application side ----------------------------------------------

    /// Whether the text input has been entered (`zwp_text_input_v3.enter`)
    /// and not left since.
    pub fn text_input_entered(&self) -> bool {
        self.state.ime.text_input_entered
    }

    fn text_input(&self) -> Result<&zwp_text_input_v3::ZwpTextInputV3, ClientError> {
        self.state.ime.text_input.as_ref().ok_or_else(|| {
            ClientError::Protocol("there is no text input (ClientOptions::text_input)".to_string())
        })
    }

    fn commit_text_input(&mut self) -> Result<(), ClientError> {
        self.text_input()?.commit();
        self.state.ime.text_input_commits += 1;
        Ok(())
    }

    /// Enables the text input, with the cursor rectangle (x, y, width,
    /// height, relative to the window's surface), and commits.
    pub fn text_input_enable(
        &mut self,
        cursor: Option<(i32, i32, i32, i32)>,
    ) -> Result<(), ClientError> {
        let text_input = self.text_input()?;
        text_input.enable();
        text_input.set_content_type(
            zwp_text_input_v3::ContentHint::None,
            zwp_text_input_v3::ContentPurpose::Normal,
        );
        text_input.set_surrounding_text(String::new(), 0, 0);
        if let Some((x, y, w, h)) = cursor {
            text_input.set_cursor_rectangle(x, y, w, h);
        }
        self.commit_text_input()
    }

    /// Moves the cursor rectangle and commits.
    pub fn text_input_set_cursor(
        &mut self,
        cursor: (i32, i32, i32, i32),
    ) -> Result<(), ClientError> {
        let (x, y, w, h) = cursor;
        self.text_input()?.set_cursor_rectangle(x, y, w, h);
        self.commit_text_input()
    }

    /// Sends the text around the cursor (cursor and anchor in bytes) and
    /// commits.
    pub fn text_input_set_surrounding(
        &mut self,
        text: &str,
        cursor: i32,
        anchor: i32,
    ) -> Result<(), ClientError> {
        self.text_input()?
            .set_surrounding_text(text.to_string(), cursor, anchor);
        self.commit_text_input()
    }

    /// Disables the text input and commits.
    pub fn text_input_disable(&mut self) -> Result<(), ClientError> {
        self.text_input()?.disable();
        self.commit_text_input()
    }

    // -- the input method side ---------------------------------------------

    /// What the input method side received so far, oldest first.
    pub fn ime_events(&self) -> &[ImeEvent] {
        &self.state.ime.events
    }

    /// Whether the input method was activated by the latest `done`.
    pub fn ime_active(&self) -> bool {
        self.state.ime.active
    }

    /// Whether the input method holds a keyboard grab.
    pub fn ime_grabbed(&self) -> bool {
        self.state.ime.grab.is_some()
    }

    fn input_method(&self) -> Result<&zwp_input_method_v2::ZwpInputMethodV2, ClientError> {
        self.state.ime.input_method.as_ref().ok_or_else(|| {
            ClientError::Protocol(
                "there is no input method (ClientOptions::input_method)".to_string(),
            )
        })
    }

    /// Grabs the keyboard (`zwp_input_method_v2.grab_keyboard`).
    pub fn ime_grab_keyboard(&mut self) -> Result<(), ClientError> {
        let grab = self.input_method()?.grab_keyboard(&self.handle, ());
        self.state.ime.grab = Some(grab);
        Ok(())
    }

    /// Releases the keyboard grab.
    pub fn ime_release_keyboard(&mut self) {
        if let Some(grab) = self.state.ime.grab.take() {
            grab.release();
        }
    }

    /// Sends the text being composed, and commits.
    pub fn ime_preedit(
        &mut self,
        text: &str,
        cursor_begin: i32,
        cursor_end: i32,
    ) -> Result<(), ClientError> {
        self.input_method()?
            .set_preedit_string(text.to_string(), cursor_begin, cursor_end);
        self.state.ime_commit();
        Ok(())
    }

    /// Sends finished text (with an empty preedit), and commits.
    pub fn ime_commit_string(&mut self, text: &str) -> Result<(), ClientError> {
        let input_method = self.input_method()?;
        input_method.commit_string(text.to_string());
        input_method.set_preedit_string(String::new(), 0, 0);
        self.state.ime_commit();
        Ok(())
    }

    /// Asks for bytes around the cursor to be deleted, and commits.
    pub fn ime_delete_surrounding(&mut self, before: u32, after: u32) -> Result<(), ClientError> {
        self.input_method()?.delete_surrounding_text(before, after);
        self.state.ime_commit();
        Ok(())
    }

    /// Sends `commit` with a serial of the test's choosing.
    pub fn ime_commit_with_serial(&mut self, serial: u32) -> Result<(), ClientError> {
        self.input_method()?.commit(serial);
        Ok(())
    }

    /// Hands a key back through the virtual keyboard, which is created
    /// with the keymap of the grab on first use.
    pub fn ime_virtual_key(&mut self, keycode: u32, pressed: bool) -> Result<(), ClientError> {
        self.state.ensure_virtual_keyboard(&self.handle)?;
        if let Some(keyboard) = &self.state.ime.virtual_keyboard {
            keyboard.key(0, keycode, u32::from(pressed));
        }
        Ok(())
    }

    /// Sends the modifiers through the virtual keyboard, as an input method
    /// does with those its grab receives.
    pub fn ime_virtual_modifiers(
        &mut self,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) -> Result<(), ClientError> {
        self.state.ensure_virtual_keyboard(&self.handle)?;
        if let Some(keyboard) = &self.state.ime.virtual_keyboard {
            keyboard.modifiers(depressed, latched, locked, group);
        }
        Ok(())
    }

    /// Shows a popup surface (the candidate window) filled with `color`.
    pub fn ime_show_popup(
        &mut self,
        width: u32,
        height: u32,
        color: [u8; 4],
    ) -> Result<(), ClientError> {
        self.state
            .show_ime_popup(&self.handle, width, height, color)
    }

    /// Destroys the popup surface.
    pub fn ime_destroy_popup(&mut self) {
        if let Some(popup) = self.state.ime.popup.take() {
            popup.role.destroy();
            popup.surface.destroy();
        }
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
            Event::Enter { .. } => {
                state.ime.text_input_entered = true;
                state.events.push(ClientEvent::TextInputEnter);
                if let (Some(cursor), Some(text_input)) = (
                    state.options.text_input_enable_on_enter,
                    &state.ime.text_input,
                ) {
                    text_input.enable();
                    text_input.set_cursor_rectangle(cursor.0, cursor.1, cursor.2, cursor.3);
                    text_input.commit();
                    state.ime.text_input_commits += 1;
                }
            }
            Event::Leave { .. } => {
                state.ime.text_input_entered = false;
                state.events.push(ClientEvent::TextInputLeave);
            }
            Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => state.events.push(ClientEvent::Preedit {
                text: text.unwrap_or_default(),
                cursor_begin,
                cursor_end,
            }),
            Event::CommitString { text } => state.events.push(ClientEvent::CommitString {
                text: text.unwrap_or_default(),
            }),
            Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => state.events.push(ClientEvent::DeleteSurroundingText {
                before: before_length,
                after: after_length,
            }),
            Event::Done { serial } => state.events.push(ClientEvent::TextInputDone {
                serial,
                commits: state.ime.text_input_commits,
            }),
            _ => {}
        }
    }
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_input_method_v2::ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        use zwp_input_method_v2::Event;
        let ime = &mut state.ime;
        match event {
            Event::Activate => {
                ime.pending_active = Some(true);
                ime.events.push(ImeEvent::Activate);
            }
            Event::Deactivate => {
                ime.pending_active = Some(false);
                ime.events.push(ImeEvent::Deactivate);
            }
            Event::SurroundingText {
                text,
                cursor,
                anchor,
            } => ime.events.push(ImeEvent::SurroundingText {
                text,
                cursor,
                anchor,
            }),
            Event::ContentType { hint, purpose } => ime.events.push(ImeEvent::ContentType {
                hint: match hint {
                    WEnum::Value(hint) => hint.bits(),
                    WEnum::Unknown(raw) => raw,
                },
                purpose: match purpose {
                    WEnum::Value(purpose) => purpose as u32,
                    WEnum::Unknown(raw) => raw,
                },
            }),
            Event::TextChangeCause { cause } => ime.events.push(ImeEvent::TextChangeCause {
                cause: match cause {
                    WEnum::Value(cause) => cause as u32,
                    WEnum::Unknown(raw) => raw,
                },
            }),
            Event::Done => {
                ime.done_count += 1;
                ime.events.push(ImeEvent::Done {
                    count: ime.done_count,
                });
                if let Some(active) = ime.pending_active.take() {
                    ime.active = active;
                    if active {
                        ime.composing.clear();
                    }
                    if state.options.input_method == Some(InputMethodMode::Echo) {
                        // As fcitx5 does: hold the keyboard while active.
                        if active && ime.grab.is_none() {
                            if let Some(input_method) = &ime.input_method {
                                ime.grab = Some(input_method.grab_keyboard(handle, ()));
                            }
                        } else if !active && let Some(grab) = ime.grab.take() {
                            grab.release();
                        }
                    }
                }
            }
            Event::Unavailable => ime.events.push(ImeEvent::Unavailable),
            _ => {}
        }
    }
}

impl Dispatch<zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2,
        event: zwp_input_method_keyboard_grab_v2::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        use zwp_input_method_keyboard_grab_v2::Event;
        match event {
            Event::Keymap { fd, size, .. } => {
                // The file may be positioned at its end (the compositor wrote
                // it just before): read from the start.
                let mut contents = vec![0; size as usize];
                let read = std::fs::File::from(fd).read_exact_at(&mut contents, 0);
                if read.is_ok() {
                    // The file ends with a NUL that xkb does not want.
                    let text =
                        String::from_utf8_lossy(contents.strip_suffix(&[0]).unwrap_or(&contents))
                            .into_owned();
                    state.ime.xkb = xkb::Keymap::new_from_string(
                        &state.keyboard.context,
                        text,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                    .map(|keymap| xkb::State::new(&keymap));
                    state.ime.keymap = Some(contents);
                }
                state.ime.events.push(ImeEvent::GrabKeymap { size });
            }
            Event::Key {
                key,
                state: pressed,
                ..
            } => {
                let pressed = pressed
                    == WEnum::Value(wayland_client::protocol::wl_keyboard::KeyState::Pressed);
                let keysym = state
                    .ime
                    .xkb
                    .as_ref()
                    .map(|s| xkb::keysym_get_name(s.key_get_one_sym(xkb::Keycode::new(key + 8))))
                    .unwrap_or_default();
                state.ime.events.push(ImeEvent::GrabKey {
                    keycode: key,
                    pressed,
                    keysym,
                });
                if state.options.input_method == Some(InputMethodMode::Echo) {
                    state.echo_key(key, pressed, handle);
                }
            }
            Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = &mut state.ime.xkb {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
                state.ime.events.push(ImeEvent::GrabModifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                });
                // A real input method mirrors them on its virtual keyboard.
                if state.options.input_method == Some(InputMethodMode::Echo)
                    && let Some(keyboard) = &state.ime.virtual_keyboard
                {
                    keyboard.modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            }
            Event::RepeatInfo { rate, delay } => {
                state
                    .ime
                    .events
                    .push(ImeEvent::GrabRepeatInfo { rate, delay });
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2,
        event: zwp_input_popup_surface_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_input_popup_surface_v2::Event::TextInputRectangle {
            x,
            y,
            width,
            height,
        } = event
        {
            state.ime.events.push(ImeEvent::PopupRectangle {
                x,
                y,
                width,
                height,
            });
        }
    }
}

wayland_client::delegate_noop!(State: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
wayland_client::delegate_noop!(State: ignore zwp_input_method_manager_v2::ZwpInputMethodManagerV2);
wayland_client::delegate_noop!(State: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
