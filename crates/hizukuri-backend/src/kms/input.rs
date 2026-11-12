//! Turning device input into the backend's [`InputEvent`]s.
//!
//! libinput events are first reduced to [`RawInput`], a plain value with
//! just what is needed. [`InputTranslator`] then does the part that needs
//! thought: the emergency key combinations, which are handled here before
//! anything else sees the key, and mapping absolute positions onto the
//! output. Both steps are free of devices; the second holds all the logic
//! and is unit tested.

use crate::{AxisSource, InputEvent};

// evdev key codes (linux/input-event-codes.h).
const KEY_BACKSPACE: u32 = 14;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_F1: u32 = 59;
const KEY_F10: u32 = 68;
const KEY_F11: u32 = 87;
const KEY_F12: u32 = 88;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_RIGHTALT: u32 = 100;

/// A device event with only the fields the backend uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RawInput {
    Key {
        /// evdev key code.
        code: u32,
        pressed: bool,
        time_us: u64,
    },
    /// Relative motion in device units after libinput's acceleration.
    PointerMotion { dx: f64, dy: f64, time_us: u64 },
    /// Absolute position, each axis normalized to `0.0..1.0` of the
    /// device's range.
    PointerMotionAbsolute { x: f64, y: f64, time_us: u64 },
    PointerButton {
        /// evdev button code.
        button: u32,
        pressed: bool,
        time_us: u64,
    },
    PointerScroll {
        source: AxisSource,
        horizontal: f64,
        vertical: f64,
        v120: Option<(i32, i32)>,
        time_us: u64,
    },
}

/// A key combination the backend acts on itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Ctrl+Alt+Backspace: stop the compositor.
    Quit,
    /// Ctrl+Alt+F<n>: switch to virtual terminal `n`.
    SwitchVt(u32),
}

/// What became of a raw event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Translated {
    /// Pass it on to the compositor.
    Event(InputEvent),
    /// The backend handles it; nothing else gets to see the key.
    Hotkey(Hotkey),
    /// Dropped (for example the release of a key whose press was a hotkey).
    Nothing,
}

/// The area absolute positions map onto: an output in the global logical
/// space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputArea {
    pub position: (i32, i32),
    pub logical_size: (i32, i32),
}

/// See the module documentation.
#[derive(Debug, Default)]
pub struct InputTranslator {
    left_ctrl: bool,
    right_ctrl: bool,
    left_alt: bool,
    right_alt: bool,
    /// Keys whose press was taken as a hotkey; their release is dropped.
    swallowed: Vec<u32>,
    /// Keys and buttons whose press was passed on and whose release was
    /// not yet.
    held_keys: Vec<u32>,
    held_buttons: Vec<u32>,
}

impl InputTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    fn hotkey(&self, code: u32) -> Option<Hotkey> {
        if !((self.left_ctrl || self.right_ctrl) && (self.left_alt || self.right_alt)) {
            return None;
        }
        match code {
            KEY_BACKSPACE => Some(Hotkey::Quit),
            KEY_F1..=KEY_F10 => Some(Hotkey::SwitchVt(code - KEY_F1 + 1)),
            KEY_F11 => Some(Hotkey::SwitchVt(11)),
            KEY_F12 => Some(Hotkey::SwitchVt(12)),
            _ => None,
        }
    }

    /// Forgets which keys are held. Call when the session resumes: keys
    /// released while it was paused never reported their release.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Releases everything that is held, as events to pass on, and forgets
    /// it. Call when the session is paused: the keys of the combination
    /// that switched the VT are still down, their releases happen while
    /// nobody is listening, and without this the compositor and its
    /// clients would see Ctrl and Alt held forever after coming back.
    /// Releases that do arrive later are dropped.
    pub fn release_all(&mut self, time_us: u64) -> Vec<InputEvent> {
        let keys = std::mem::take(&mut self.held_keys);
        let buttons = std::mem::take(&mut self.held_buttons);
        self.reset();
        keys.into_iter()
            .map(|keycode| InputEvent::Key {
                keycode,
                pressed: false,
                time_us,
            })
            .chain(buttons.into_iter().map(|button| InputEvent::PointerButton {
                button,
                pressed: false,
                time_us,
            }))
            .collect()
    }

    /// Keeps the list of held keys or buttons. Returns whether the event
    /// should be passed on: a release is only passed on for a press that
    /// was.
    fn track(held: &mut Vec<u32>, code: u32, pressed: bool) -> bool {
        let position = held.iter().position(|c| *c == code);
        match (pressed, position) {
            (true, None) => {
                held.push(code);
                true
            }
            // A repeated press without a release in between; pass it on.
            (true, Some(_)) => true,
            (false, Some(index)) => {
                held.swap_remove(index);
                true
            }
            (false, None) => false,
        }
    }

    /// Translates one raw event. `output` is where absolute positions go;
    /// without an output they are dropped.
    pub fn translate(&mut self, raw: RawInput, output: Option<OutputArea>) -> Translated {
        match raw {
            RawInput::Key {
                code,
                pressed,
                time_us,
            } => {
                match code {
                    KEY_LEFTCTRL => self.left_ctrl = pressed,
                    KEY_RIGHTCTRL => self.right_ctrl = pressed,
                    KEY_LEFTALT => self.left_alt = pressed,
                    KEY_RIGHTALT => self.right_alt = pressed,
                    _ => {}
                }
                if pressed {
                    if let Some(hotkey) = self.hotkey(code) {
                        if !self.swallowed.contains(&code) {
                            self.swallowed.push(code);
                        }
                        return Translated::Hotkey(hotkey);
                    }
                } else if let Some(index) = self.swallowed.iter().position(|c| *c == code) {
                    self.swallowed.swap_remove(index);
                    return Translated::Nothing;
                }
                if !Self::track(&mut self.held_keys, code, pressed) {
                    return Translated::Nothing;
                }
                Translated::Event(InputEvent::Key {
                    keycode: code,
                    pressed,
                    time_us,
                })
            }
            RawInput::PointerMotion { dx, dy, time_us } => {
                Translated::Event(InputEvent::PointerMotion { dx, dy, time_us })
            }
            RawInput::PointerMotionAbsolute { x, y, time_us } => match output {
                Some(area) => Translated::Event(InputEvent::PointerMotionAbsolute {
                    x: area.position.0 as f64 + x.clamp(0.0, 1.0) * area.logical_size.0 as f64,
                    y: area.position.1 as f64 + y.clamp(0.0, 1.0) * area.logical_size.1 as f64,
                    time_us,
                }),
                None => Translated::Nothing,
            },
            RawInput::PointerButton {
                button,
                pressed,
                time_us,
            } => {
                if !Self::track(&mut self.held_buttons, button, pressed) {
                    return Translated::Nothing;
                }
                Translated::Event(InputEvent::PointerButton {
                    button,
                    pressed,
                    time_us,
                })
            }
            RawInput::PointerScroll {
                source,
                horizontal,
                vertical,
                v120,
                time_us,
            } => Translated::Event(InputEvent::PointerAxis {
                source,
                horizontal,
                vertical,
                v120,
                time_us,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: u32 = 30;

    fn key(code: u32, pressed: bool) -> RawInput {
        RawInput::Key {
            code,
            pressed,
            time_us: 1,
        }
    }

    fn passes(translator: &mut InputTranslator, code: u32, pressed: bool) -> bool {
        matches!(
            translator.translate(key(code, pressed), None),
            Translated::Event(InputEvent::Key { keycode, pressed: p, .. }) if keycode == code && p == pressed
        )
    }

    #[test]
    fn ordinary_keys_pass_through() {
        let mut t = InputTranslator::new();
        assert!(passes(&mut t, KEY_A, true));
        assert!(passes(&mut t, KEY_A, false));
        // Backspace and function keys on their own are ordinary keys.
        assert!(passes(&mut t, KEY_BACKSPACE, true));
        assert!(passes(&mut t, KEY_BACKSPACE, false));
        assert!(passes(&mut t, KEY_F1, true));
        // Ctrl alone or Alt alone is not enough.
        assert!(passes(&mut t, KEY_LEFTCTRL, true));
        assert!(passes(&mut t, KEY_BACKSPACE, true));
        assert!(passes(&mut t, KEY_LEFTCTRL, false));
        assert!(passes(&mut t, KEY_LEFTALT, true));
        assert!(passes(&mut t, KEY_BACKSPACE, true));
    }

    #[test]
    fn ctrl_alt_backspace_quits_and_is_swallowed() {
        let mut t = InputTranslator::new();
        // The modifiers themselves still reach clients.
        assert!(passes(&mut t, KEY_LEFTCTRL, true));
        assert!(passes(&mut t, KEY_RIGHTALT, true));
        assert_eq!(
            t.translate(key(KEY_BACKSPACE, true), None),
            Translated::Hotkey(Hotkey::Quit)
        );
        // Neither the press nor the release of the hotkey key goes on.
        assert_eq!(
            t.translate(key(KEY_BACKSPACE, false), None),
            Translated::Nothing
        );
        // A second release has no press that was passed on: dropped too.
        assert_eq!(
            t.translate(key(KEY_BACKSPACE, false), None),
            Translated::Nothing
        );
        // Without the modifiers it is an ordinary key again.
        assert!(passes(&mut t, KEY_LEFTCTRL, false));
        assert!(passes(&mut t, KEY_RIGHTALT, false));
        assert!(passes(&mut t, KEY_BACKSPACE, true));
        assert!(passes(&mut t, KEY_BACKSPACE, false));
    }

    #[test]
    fn ctrl_alt_function_keys_switch_vt() {
        let mut t = InputTranslator::new();
        t.translate(key(KEY_RIGHTCTRL, true), None);
        t.translate(key(KEY_LEFTALT, true), None);
        for (code, vt) in [
            (KEY_F1, 1),
            (KEY_F1 + 1, 2),
            (KEY_F10, 10),
            (KEY_F11, 11),
            (KEY_F12, 12),
        ] {
            assert_eq!(
                t.translate(key(code, true), None),
                Translated::Hotkey(Hotkey::SwitchVt(vt))
            );
            assert_eq!(t.translate(key(code, false), None), Translated::Nothing);
        }
        // Releasing a modifier ends it.
        t.translate(key(KEY_LEFTALT, false), None);
        assert!(passes(&mut t, KEY_F1, true));
    }

    #[test]
    fn the_release_is_swallowed_even_after_the_modifiers_are_up() {
        let mut t = InputTranslator::new();
        t.translate(key(KEY_LEFTCTRL, true), None);
        t.translate(key(KEY_LEFTALT, true), None);
        assert!(matches!(
            t.translate(key(KEY_F1 + 2, true), None),
            Translated::Hotkey(Hotkey::SwitchVt(3))
        ));
        t.translate(key(KEY_LEFTCTRL, false), None);
        t.translate(key(KEY_LEFTALT, false), None);
        assert_eq!(
            t.translate(key(KEY_F1 + 2, false), None),
            Translated::Nothing
        );
    }

    #[test]
    fn pausing_releases_what_is_held() {
        let mut t = InputTranslator::new();
        assert!(passes(&mut t, KEY_LEFTCTRL, true));
        assert!(passes(&mut t, KEY_LEFTALT, true));
        assert!(passes(&mut t, KEY_A, true));
        assert!(passes(&mut t, KEY_A, false));
        t.translate(
            RawInput::PointerButton {
                button: 272,
                pressed: true,
                time_us: 1,
            },
            None,
        );
        // Ctrl+Alt+F2 switches the VT; the session is paused with Ctrl,
        // Alt and the button down.
        assert!(matches!(
            t.translate(key(KEY_F1 + 1, true), None),
            Translated::Hotkey(Hotkey::SwitchVt(2))
        ));
        let released = t.release_all(77);
        assert_eq!(
            released,
            [
                InputEvent::Key {
                    keycode: KEY_LEFTCTRL,
                    pressed: false,
                    time_us: 77
                },
                InputEvent::Key {
                    keycode: KEY_LEFTALT,
                    pressed: false,
                    time_us: 77
                },
                InputEvent::PointerButton {
                    button: 272,
                    pressed: false,
                    time_us: 77
                },
            ]
        );
        // Nothing is held now, and nothing is released twice: the real
        // releases, should they still arrive, are dropped.
        assert!(t.release_all(78).is_empty());
        for code in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_F1 + 1] {
            assert_eq!(t.translate(key(code, false), None), Translated::Nothing);
        }
        // Back: plain keys are plain again.
        assert!(passes(&mut t, KEY_BACKSPACE, true));
        assert!(passes(&mut t, KEY_BACKSPACE, false));
    }

    #[test]
    fn a_release_without_its_press_is_dropped() {
        // The Enter key that started the compositor from a console is
        // still down when it starts reading input.
        let mut t = InputTranslator::new();
        assert_eq!(t.translate(key(28, false), None), Translated::Nothing);
        assert!(passes(&mut t, 28, true));
        assert!(passes(&mut t, 28, false));
    }

    #[test]
    fn reset_forgets_held_keys() {
        let mut t = InputTranslator::new();
        t.translate(key(KEY_LEFTCTRL, true), None);
        t.translate(key(KEY_LEFTALT, true), None);
        // The VT switch takes the session away; the releases are lost.
        t.reset();
        assert!(passes(&mut t, KEY_BACKSPACE, true));
    }

    #[test]
    fn absolute_positions_map_onto_the_output() {
        let mut t = InputTranslator::new();
        let area = OutputArea {
            position: (100, 50),
            logical_size: (1920, 1080),
        };
        let at = |t: &mut InputTranslator, x, y| {
            t.translate(
                RawInput::PointerMotionAbsolute { x, y, time_us: 7 },
                Some(area),
            )
        };
        assert_eq!(
            at(&mut t, 0.0, 0.0),
            Translated::Event(InputEvent::PointerMotionAbsolute {
                x: 100.0,
                y: 50.0,
                time_us: 7
            })
        );
        assert_eq!(
            at(&mut t, 0.5, 0.25),
            Translated::Event(InputEvent::PointerMotionAbsolute {
                x: 1060.0,
                y: 320.0,
                time_us: 7
            })
        );
        // Out of range values are clamped to the output.
        assert_eq!(
            at(&mut t, 1.5, -0.5),
            Translated::Event(InputEvent::PointerMotionAbsolute {
                x: 2020.0,
                y: 50.0,
                time_us: 7
            })
        );
        // No output: nowhere to put the pointer.
        assert_eq!(
            t.translate(
                RawInput::PointerMotionAbsolute {
                    x: 0.5,
                    y: 0.5,
                    time_us: 7
                },
                None
            ),
            Translated::Nothing
        );
    }

    #[test]
    fn pointer_events_are_passed_on_unchanged() {
        let mut t = InputTranslator::new();
        assert_eq!(
            t.translate(
                RawInput::PointerMotion {
                    dx: 1.5,
                    dy: -2.0,
                    time_us: 3
                },
                None
            ),
            Translated::Event(InputEvent::PointerMotion {
                dx: 1.5,
                dy: -2.0,
                time_us: 3
            })
        );
        assert_eq!(
            t.translate(
                RawInput::PointerButton {
                    button: 0x110,
                    pressed: true,
                    time_us: 4
                },
                None
            ),
            Translated::Event(InputEvent::PointerButton {
                button: 0x110,
                pressed: true,
                time_us: 4
            })
        );
        assert_eq!(
            t.translate(
                RawInput::PointerScroll {
                    source: AxisSource::Wheel,
                    horizontal: 0.0,
                    vertical: 15.0,
                    v120: Some((0, 120)),
                    time_us: 5
                },
                None
            ),
            Translated::Event(InputEvent::PointerAxis {
                source: AxisSource::Wheel,
                horizontal: 0.0,
                vertical: 15.0,
                v120: Some((0, 120)),
                time_us: 5
            })
        );
    }
}
