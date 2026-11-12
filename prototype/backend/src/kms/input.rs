//! Turning device input into the backend's [`InputEvent`]s.
//!
//! libinput events are first reduced to [`RawInput`], a plain value with
//! just what is needed. [`InputTranslator`] then does the part that needs
//! thought: the emergency key combinations, which are handled here before
//! anything else sees the key, mapping absolute positions and touches onto
//! the output, keeping touches and gestures whole across a pause of the
//! session, and telling whether a touchscreen is there. Both steps are free
//! of devices; the second holds all the logic and is unit tested.

use crate::{AxisSource, GestureKind, InputEvent};

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
    /// Relative motion: after libinput's acceleration (`dx`, `dy`, the
    /// pixels of a standardized mouse), and before it (`dx_unaccel`,
    /// `dy_unaccel`, raw device units, so in the device's own resolution).
    PointerMotion {
        dx: f64,
        dy: f64,
        dx_unaccel: f64,
        dy_unaccel: f64,
        time_us: u64,
    },
    /// Absolute position, each axis normalized to `0.0..1.0` of the
    /// device's range.
    PointerMotionAbsolute {
        x: f64,
        y: f64,
        time_us: u64,
    },
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
    /// An input device appeared or went away; `touch` when it is a
    /// touchscreen.
    DeviceAdded {
        touch: bool,
    },
    DeviceRemoved {
        touch: bool,
    },
    /// A touch, at a position normalized to `0.0..1.0` of the device's
    /// range on each axis (after the device's calibration). `slot` is the
    /// seat-wide slot.
    TouchDown {
        slot: u32,
        x: f64,
        y: f64,
        time_us: u64,
    },
    TouchMotion {
        slot: u32,
        x: f64,
        y: f64,
        time_us: u64,
    },
    TouchUp {
        slot: u32,
        time_us: u64,
    },
    TouchFrame {
        time_us: u64,
    },
    TouchCancel {
        time_us: u64,
    },
    GestureBegin {
        kind: GestureKind,
        fingers: u32,
        time_us: u64,
    },
    GestureUpdate {
        kind: GestureKind,
        dx: f64,
        dy: f64,
        scale: f64,
        rotation: f64,
        time_us: u64,
    },
    GestureEnd {
        kind: GestureKind,
        cancelled: bool,
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
    /// Touches that went down and did not come up yet.
    held_touches: Vec<u32>,
    /// A touch event was passed on since the last frame.
    touch_frame_owed: bool,
    /// The gesture that began and did not end yet.
    gesture: Option<GestureKind>,
    /// How many touchscreens there are. Kept across pauses: libinput
    /// reports its devices as removed and added again by itself.
    touch_devices: u32,
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
        *self = Self {
            touch_devices: self.touch_devices,
            ..Self::default()
        };
    }

    /// Releases everything that is held, as events to pass on, and forgets
    /// it. Call when the session is paused: the keys of the combination
    /// that switched the VT are still down, their releases happen while
    /// nobody is listening, and without this the compositor and its
    /// clients would see Ctrl and Alt held forever after coming back.
    /// Releases that do arrive later are dropped.
    ///
    /// Touches in progress are cancelled, and a gesture in progress ends as
    /// cancelled, for the same reason.
    pub fn release_all(&mut self, time_us: u64) -> Vec<InputEvent> {
        let keys = std::mem::take(&mut self.held_keys);
        let buttons = std::mem::take(&mut self.held_buttons);
        let touching = !self.held_touches.is_empty();
        let gesture = self.gesture.take();
        self.reset();
        let touch = touching.then_some(InputEvent::TouchCancel { time_us });
        let gesture = gesture.map(|kind| InputEvent::GestureEnd {
            kind,
            cancelled: true,
            time_us,
        });
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
            .chain(touch)
            .chain(gesture)
            .collect()
    }

    /// Where a normalized position of an absolute device lands on the
    /// output.
    fn on_output(area: OutputArea, x: f64, y: f64) -> (f64, f64) {
        (
            area.position.0 as f64 + x.clamp(0.0, 1.0) * area.logical_size.0 as f64,
            area.position.1 as f64 + y.clamp(0.0, 1.0) * area.logical_size.1 as f64,
        )
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
            RawInput::PointerMotion {
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
                time_us,
            } => Translated::Event(InputEvent::PointerMotion {
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
                time_us,
            }),
            RawInput::PointerMotionAbsolute { x, y, time_us } => match output {
                Some(area) => {
                    let (x, y) = Self::on_output(area, x, y);
                    Translated::Event(InputEvent::PointerMotionAbsolute { x, y, time_us })
                }
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
            RawInput::DeviceAdded { touch } => {
                if !touch {
                    return Translated::Nothing;
                }
                self.touch_devices += 1;
                if self.touch_devices == 1 {
                    Translated::Event(InputEvent::TouchAvailable(true))
                } else {
                    Translated::Nothing
                }
            }
            RawInput::DeviceRemoved { touch } => {
                if !touch || self.touch_devices == 0 {
                    return Translated::Nothing;
                }
                self.touch_devices -= 1;
                if self.touch_devices == 0 {
                    Translated::Event(InputEvent::TouchAvailable(false))
                } else {
                    Translated::Nothing
                }
            }
            // A touchscreen covers the output, as an absolute pointer does.
            // With several outputs it would have to be decided which one a
            // touchscreen belongs to; this backend drives only one.
            RawInput::TouchDown {
                slot,
                x,
                y,
                time_us,
            } => {
                let Some(area) = output else {
                    return Translated::Nothing;
                };
                if !self.held_touches.contains(&slot) {
                    self.held_touches.push(slot);
                }
                self.touch_frame_owed = true;
                let (x, y) = Self::on_output(area, x, y);
                Translated::Event(InputEvent::TouchDown {
                    slot,
                    x,
                    y,
                    time_us,
                })
            }
            RawInput::TouchMotion {
                slot,
                x,
                y,
                time_us,
            } => match output {
                // Only for a touch whose down was passed on.
                Some(area) if self.held_touches.contains(&slot) => {
                    self.touch_frame_owed = true;
                    let (x, y) = Self::on_output(area, x, y);
                    Translated::Event(InputEvent::TouchMotion {
                        slot,
                        x,
                        y,
                        time_us,
                    })
                }
                _ => Translated::Nothing,
            },
            RawInput::TouchUp { slot, time_us } => {
                if !Self::track(&mut self.held_touches, slot, false) {
                    return Translated::Nothing;
                }
                self.touch_frame_owed = true;
                Translated::Event(InputEvent::TouchUp { slot, time_us })
            }
            RawInput::TouchFrame { time_us } => {
                if std::mem::take(&mut self.touch_frame_owed) {
                    Translated::Event(InputEvent::TouchFrame { time_us })
                } else {
                    Translated::Nothing
                }
            }
            RawInput::TouchCancel { time_us } => {
                self.touch_frame_owed = false;
                if self.held_touches.is_empty() {
                    return Translated::Nothing;
                }
                self.held_touches.clear();
                Translated::Event(InputEvent::TouchCancel { time_us })
            }
            RawInput::GestureBegin {
                kind,
                fingers,
                time_us,
            } => {
                self.gesture = Some(kind);
                Translated::Event(InputEvent::GestureBegin {
                    kind,
                    fingers,
                    time_us,
                })
            }
            RawInput::GestureUpdate {
                kind,
                dx,
                dy,
                scale,
                rotation,
                time_us,
            } => {
                // Only for the gesture whose begin was passed on.
                if self.gesture != Some(kind) {
                    return Translated::Nothing;
                }
                Translated::Event(InputEvent::GestureUpdate {
                    kind,
                    dx,
                    dy,
                    scale,
                    rotation,
                    time_us,
                })
            }
            RawInput::GestureEnd {
                kind,
                cancelled,
                time_us,
            } => {
                if self.gesture != Some(kind) {
                    return Translated::Nothing;
                }
                self.gesture = None;
                Translated::Event(InputEvent::GestureEnd {
                    kind,
                    cancelled,
                    time_us,
                })
            }
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

    const AREA: OutputArea = OutputArea {
        position: (100, 50),
        logical_size: (1000, 500),
    };

    fn touch_down(slot: u32, x: f64, y: f64) -> RawInput {
        RawInput::TouchDown {
            slot,
            x,
            y,
            time_us: 9,
        }
    }

    #[test]
    fn touchscreens_are_announced_when_the_first_comes_and_the_last_goes() {
        let mut t = InputTranslator::new();
        let added = |touch| RawInput::DeviceAdded { touch };
        let removed = |touch| RawInput::DeviceRemoved { touch };
        // A keyboard or a mouse says nothing.
        assert_eq!(t.translate(added(false), None), Translated::Nothing);
        assert_eq!(
            t.translate(added(true), None),
            Translated::Event(InputEvent::TouchAvailable(true))
        );
        assert_eq!(t.translate(added(true), None), Translated::Nothing);
        assert_eq!(t.translate(removed(true), None), Translated::Nothing);
        // A pause does not forget the devices: libinput removes and adds
        // them itself.
        t.release_all(1);
        t.reset();
        assert_eq!(
            t.translate(removed(true), None),
            Translated::Event(InputEvent::TouchAvailable(false))
        );
        // More removals than there were devices.
        assert_eq!(t.translate(removed(true), None), Translated::Nothing);
        assert_eq!(t.translate(removed(false), None), Translated::Nothing);
    }

    #[test]
    fn touches_map_onto_the_output_and_need_their_down() {
        let mut t = InputTranslator::new();
        // No output: nowhere to touch.
        assert_eq!(
            t.translate(touch_down(0, 0.5, 0.5), None),
            Translated::Nothing
        );
        // Without its down, a motion or an up is dropped, and so is a frame
        // that would close nothing.
        let motion = RawInput::TouchMotion {
            slot: 0,
            x: 0.1,
            y: 0.1,
            time_us: 9,
        };
        assert_eq!(t.translate(motion, Some(AREA)), Translated::Nothing);
        assert_eq!(
            t.translate(
                RawInput::TouchUp {
                    slot: 0,
                    time_us: 9
                },
                Some(AREA)
            ),
            Translated::Nothing
        );
        assert_eq!(
            t.translate(RawInput::TouchFrame { time_us: 9 }, Some(AREA)),
            Translated::Nothing
        );

        assert_eq!(
            t.translate(touch_down(3, 0.5, 0.25), Some(AREA)),
            Translated::Event(InputEvent::TouchDown {
                slot: 3,
                x: 600.0,
                y: 175.0,
                time_us: 9
            })
        );
        assert_eq!(
            t.translate(touch_down(4, 1.5, -1.0), Some(AREA)),
            Translated::Event(InputEvent::TouchDown {
                slot: 4,
                x: 1100.0,
                y: 50.0,
                time_us: 9
            })
        );
        assert_eq!(
            t.translate(RawInput::TouchFrame { time_us: 10 }, Some(AREA)),
            Translated::Event(InputEvent::TouchFrame { time_us: 10 })
        );
        // One frame per set of events.
        assert_eq!(
            t.translate(RawInput::TouchFrame { time_us: 10 }, Some(AREA)),
            Translated::Nothing
        );
        assert_eq!(
            t.translate(
                RawInput::TouchMotion {
                    slot: 3,
                    x: 0.0,
                    y: 1.0,
                    time_us: 11
                },
                Some(AREA)
            ),
            Translated::Event(InputEvent::TouchMotion {
                slot: 3,
                x: 100.0,
                y: 550.0,
                time_us: 11
            })
        );
        assert_eq!(
            t.translate(
                RawInput::TouchUp {
                    slot: 3,
                    time_us: 12
                },
                Some(AREA)
            ),
            Translated::Event(InputEvent::TouchUp {
                slot: 3,
                time_us: 12
            })
        );
        // Once up, the slot is free again.
        assert_eq!(
            t.translate(
                RawInput::TouchUp {
                    slot: 3,
                    time_us: 12
                },
                Some(AREA)
            ),
            Translated::Nothing
        );
        // A cancel voids what is still down, once.
        assert_eq!(
            t.translate(RawInput::TouchCancel { time_us: 13 }, Some(AREA)),
            Translated::Event(InputEvent::TouchCancel { time_us: 13 })
        );
        assert_eq!(
            t.translate(RawInput::TouchCancel { time_us: 13 }, Some(AREA)),
            Translated::Nothing
        );
        assert_eq!(
            t.translate(
                RawInput::TouchUp {
                    slot: 4,
                    time_us: 14
                },
                Some(AREA)
            ),
            Translated::Nothing
        );
    }

    #[test]
    fn gestures_pass_from_begin_to_end() {
        let mut t = InputTranslator::new();
        let update = |kind| RawInput::GestureUpdate {
            kind,
            dx: 2.0,
            dy: -1.0,
            scale: 1.5,
            rotation: 3.0,
            time_us: 2,
        };
        // Nothing began: dropped.
        assert_eq!(
            t.translate(update(GestureKind::Swipe), None),
            Translated::Nothing
        );
        assert_eq!(
            t.translate(
                RawInput::GestureBegin {
                    kind: GestureKind::Pinch,
                    fingers: 2,
                    time_us: 1
                },
                None
            ),
            Translated::Event(InputEvent::GestureBegin {
                kind: GestureKind::Pinch,
                fingers: 2,
                time_us: 1
            })
        );
        assert_eq!(
            t.translate(update(GestureKind::Pinch), None),
            Translated::Event(InputEvent::GestureUpdate {
                kind: GestureKind::Pinch,
                dx: 2.0,
                dy: -1.0,
                scale: 1.5,
                rotation: 3.0,
                time_us: 2
            })
        );
        // Not the gesture in progress.
        assert_eq!(
            t.translate(update(GestureKind::Swipe), None),
            Translated::Nothing
        );
        let end = RawInput::GestureEnd {
            kind: GestureKind::Pinch,
            cancelled: false,
            time_us: 3,
        };
        assert_eq!(
            t.translate(end, None),
            Translated::Event(InputEvent::GestureEnd {
                kind: GestureKind::Pinch,
                cancelled: false,
                time_us: 3
            })
        );
        assert_eq!(t.translate(end, None), Translated::Nothing);
    }

    #[test]
    fn pausing_cancels_touches_and_the_gesture() {
        let mut t = InputTranslator::new();
        t.translate(touch_down(0, 0.5, 0.5), Some(AREA));
        t.translate(touch_down(1, 0.6, 0.5), Some(AREA));
        t.translate(
            RawInput::GestureBegin {
                kind: GestureKind::Swipe,
                fingers: 3,
                time_us: 1,
            },
            None,
        );
        assert_eq!(
            t.release_all(5),
            [
                InputEvent::TouchCancel { time_us: 5 },
                InputEvent::GestureEnd {
                    kind: GestureKind::Swipe,
                    cancelled: true,
                    time_us: 5
                },
            ]
        );
        // What arrives of them afterwards is dropped.
        assert_eq!(
            t.translate(
                RawInput::TouchUp {
                    slot: 0,
                    time_us: 6
                },
                Some(AREA)
            ),
            Translated::Nothing
        );
        assert_eq!(
            t.translate(
                RawInput::GestureEnd {
                    kind: GestureKind::Swipe,
                    cancelled: false,
                    time_us: 6
                },
                None
            ),
            Translated::Nothing
        );
        assert!(t.release_all(7).is_empty());
    }

    #[test]
    fn pointer_events_are_passed_on_unchanged() {
        let mut t = InputTranslator::new();
        assert_eq!(
            t.translate(
                RawInput::PointerMotion {
                    dx: 1.5,
                    dy: -2.0,
                    dx_unaccel: 1.0,
                    dy_unaccel: -1.25,
                    time_us: 3
                },
                None
            ),
            Translated::Event(InputEvent::PointerMotion {
                dx: 1.5,
                dy: -2.0,
                dx_unaccel: 1.0,
                dy_unaccel: -1.25,
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
