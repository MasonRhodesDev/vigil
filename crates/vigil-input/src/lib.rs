//! Input subsystem (DESIGN.md §5): libinput events through xkbcommon state
//! into normalized [`vigil_core::InputEvent`]s. Key repeat and compose are
//! synthesized HERE — nobody else in the stack does repeat. The full public
//! API exists from M1 even where implementation lands later (compose: M2).
//!
//! Hard requirement (DESIGN.md §1): never EVIOCGRAB — host daemons share the
//! seat's input devices while the greeter is up. libinput does not grab by
//! default; nothing in this crate may change that.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use smithay::reexports::input;
use smithay::reexports::input::event::keyboard::{KeyState, KeyboardEventTrait};
use smithay::reexports::input::event::pointer::{ButtonState, PointerEvent};
use smithay::reexports::input::{LibinputInterface, event};
use vigil_core::{DeviceOpener, InputEvent};
use xkbcommon::xkb;

const REPEAT_DELAY: Duration = Duration::from_millis(500);
const REPEAT_INTERVAL: Duration = Duration::from_millis(33);

#[derive(Debug)]
pub struct InputError(pub String);

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "input: {}", self.0)
    }
}
impl std::error::Error for InputError {}

/// Owns the libinput context and xkb state for the seat.
pub struct InputSystem {
    libinput: input::Libinput,
    keyboard: KeyboardState,
    compose_locale: Option<String>,
}

impl InputSystem {
    pub fn new(seat: &str, opener: Box<dyn DeviceOpener>) -> Result<Self, InputError> {
        let mut libinput = input::Libinput::new_with_udev(OpenerAdapter(opener));
        libinput
            .udev_assign_seat(seat)
            .map_err(|()| InputError(format!("could not assign libinput to seat {seat}")))?;

        Ok(Self {
            libinput,
            keyboard: KeyboardState::new()?,
            compose_locale: None,
        })
    }

    /// Session paused (VT switch away): seat devices are being revoked.
    pub fn suspend(&mut self) {
        self.libinput.suspend();
    }

    /// Session re-activated: reopen the seat's devices through the opener.
    /// Without this, input stays dead after returning from another VT.
    pub fn resume(&mut self) -> Result<(), InputError> {
        self.libinput
            .resume()
            .map_err(|()| InputError("could not resume libinput after reactivation".into()))
    }

    /// Drain pending libinput events and translate the supported ones.
    pub fn dispatch(&mut self) -> Vec<InputEvent> {
        if self.libinput.dispatch().is_err() {
            return Vec::new();
        }

        let mut translated = Vec::new();
        for input_event in &mut self.libinput {
            match input_event {
                event::Event::Keyboard(event::KeyboardEvent::Key(key)) => {
                    let pressed = key.key_state() == KeyState::Pressed;
                    translated.push(self.keyboard.key(key.key(), pressed, Instant::now()));
                }
                event::Event::Pointer(PointerEvent::Motion(motion)) => {
                    translated.push(InputEvent::PointerMotion {
                        dx: motion.dx(),
                        dy: motion.dy(),
                    });
                }
                event::Event::Pointer(PointerEvent::MotionAbsolute(motion)) => {
                    // A unit transform yields normalized coordinates. The binary
                    // scales these into the complete output-layout space.
                    translated.push(InputEvent::PointerAbsolute {
                        x: motion.absolute_x_transformed(1).clamp(0.0, 1.0),
                        y: motion.absolute_y_transformed(1).clamp(0.0, 1.0),
                    });
                }
                event::Event::Pointer(PointerEvent::Button(button)) => {
                    translated.push(InputEvent::PointerButton {
                        button: button.button(),
                        pressed: button.button_state() == ButtonState::Pressed,
                    });
                }
                _ => {}
            }
        }
        translated
    }

    /// When the repeat timer should next fire, if a repeating key is held.
    pub fn next_repeat_deadline(&self) -> Option<std::time::Instant> {
        self.keyboard.next_repeat_deadline()
    }

    /// Synthesize all repeats due at `now` and advance the repeat timer.
    pub fn tick_repeat(&mut self, now: Instant) -> Vec<InputEvent> {
        self.keyboard.tick_repeat(now)
    }

    /// Configure the locale that the compose table will use. Compose
    /// processing lands in M2; retaining the setting now keeps that work
    /// behind this crate's established API.
    pub fn set_compose_locale(&mut self, locale: Option<&str>) {
        self.compose_locale = locale.map(str::to_owned);
    }

    /// Current caps-lock state (theme contract `caps-lock`).
    pub fn caps_lock(&self) -> bool {
        self.keyboard.caps_lock()
    }
}

impl std::os::fd::AsFd for InputSystem {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: the libinput context owns this fd for the life of self.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

impl AsRawFd for InputSystem {
    fn as_raw_fd(&self) -> RawFd {
        self.libinput.as_raw_fd()
    }
}

struct OpenerAdapter(Box<dyn DeviceOpener>);

impl LibinputInterface for OpenerAdapter {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        self.0.open(path, flags).map_err(|_| 1)
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        self.0.close(fd);
    }
}

/// Pure xkb translation and repeat state. It deliberately contains no
/// libinput types, keeping keyboard behavior headless-testable.
struct KeyboardState {
    keymap: xkb::Keymap,
    state: xkb::State,
    repeat: Option<Repeat>,
}

#[derive(Clone, Copy)]
struct Repeat {
    keycode: xkb::Keycode,
    deadline: Instant,
}

impl KeyboardState {
    fn new() -> Result<Self, InputError> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| InputError("could not compile the system xkb keymap".into()))?;
        let state = xkb::State::new(&keymap);
        Ok(Self {
            keymap,
            state,
            repeat: None,
        })
    }

    fn key(&mut self, evdev_keycode: u32, pressed: bool, now: Instant) -> InputEvent {
        let keycode = xkb::Keycode::new(evdev_keycode + 8);
        self.state.update_key(
            keycode,
            if pressed {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );

        if pressed {
            self.repeat = self.keymap.key_repeats(keycode).then_some(Repeat {
                keycode,
                deadline: now + REPEAT_DELAY,
            });
        } else if self.repeat.is_some_and(|repeat| repeat.keycode == keycode) {
            self.repeat = None;
        }

        self.event(keycode, pressed)
    }

    fn event(&self, keycode: xkb::Keycode, pressed: bool) -> InputEvent {
        let utf8 = self.state.key_get_utf8(keycode);
        InputEvent::Key {
            keysym: self.state.key_get_one_sym(keycode).raw(),
            utf8: (!utf8.is_empty()).then_some(utf8),
            pressed,
        }
    }

    fn next_repeat_deadline(&self) -> Option<Instant> {
        self.repeat.map(|repeat| repeat.deadline)
    }

    fn tick_repeat(&mut self, now: Instant) -> Vec<InputEvent> {
        let Some(mut repeat) = self.repeat else {
            return Vec::new();
        };
        let mut events = Vec::new();
        while repeat.deadline <= now {
            events.push(self.event(repeat.keycode, true));
            repeat.deadline += REPEAT_INTERVAL;
        }
        self.repeat = Some(repeat);
        events
    }

    fn caps_lock(&self) -> bool {
        self.state
            .mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: u32 = 30;
    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_CAPSLOCK: u32 = 58;
    const KEY_ENTER: u32 = 28;

    fn key_parts(event: InputEvent) -> (u32, Option<String>, bool) {
        let InputEvent::Key {
            keysym,
            utf8,
            pressed,
        } = event
        else {
            panic!("expected key event")
        };
        (keysym, utf8, pressed)
    }

    #[test]
    fn translates_letters_shift_and_return() {
        let mut keyboard = KeyboardState::new().unwrap();
        let now = Instant::now();
        assert_eq!(
            key_parts(keyboard.key(KEY_A, true, now)),
            ('a' as u32, Some("a".into()), true)
        );
        keyboard.key(KEY_A, false, now);
        keyboard.key(KEY_LEFTSHIFT, true, now);
        assert_eq!(
            key_parts(keyboard.key(KEY_A, true, now)),
            ('A' as u32, Some("A".into()), true)
        );
        keyboard.key(KEY_A, false, now);
        keyboard.key(KEY_LEFTSHIFT, false, now);

        // xkbcommon represents Return text as carriage return, not line feed.
        assert_eq!(
            key_parts(keyboard.key(KEY_ENTER, true, now)),
            (0xff0d, Some("\r".into()), true)
        );
    }

    #[test]
    fn caps_lock_state_toggles() {
        let mut keyboard = KeyboardState::new().unwrap();
        let now = Instant::now();
        assert!(!keyboard.caps_lock());
        keyboard.key(KEY_CAPSLOCK, true, now);
        keyboard.key(KEY_CAPSLOCK, false, now);
        assert!(keyboard.caps_lock());
        keyboard.key(KEY_CAPSLOCK, true, now);
        keyboard.key(KEY_CAPSLOCK, false, now);
        assert!(!keyboard.caps_lock());
    }

    #[test]
    fn repeat_arms_ticks_rearms_and_releases() {
        let mut keyboard = KeyboardState::new().unwrap();
        let now = Instant::now();
        keyboard.key(KEY_A, true, now);
        let first = now + REPEAT_DELAY;
        assert_eq!(keyboard.next_repeat_deadline(), Some(first));
        assert_eq!(keyboard.tick_repeat(first).len(), 1);
        assert_eq!(
            keyboard.next_repeat_deadline(),
            Some(first + REPEAT_INTERVAL)
        );
        keyboard.key(KEY_A, false, first);
        assert_eq!(keyboard.next_repeat_deadline(), None);
    }

    #[test]
    fn non_repeating_key_never_arms() {
        let mut keyboard = KeyboardState::new().unwrap();
        keyboard.key(KEY_LEFTSHIFT, true, Instant::now());
        assert_eq!(keyboard.next_repeat_deadline(), None);
    }
}
