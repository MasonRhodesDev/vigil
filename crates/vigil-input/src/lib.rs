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
use vigil_core::{DeviceOpener, InputEvent, KeymapSettings};
use xkbcommon::xkb;
use xkbcommon::xkb::compose;

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
    pub fn new(
        seat: &str,
        opener: Box<dyn DeviceOpener>,
        keymap: &KeymapSettings,
    ) -> Result<Self, InputError> {
        let mut libinput = input::Libinput::new_with_udev(OpenerAdapter(opener));
        libinput
            .udev_assign_seat(seat)
            .map_err(|()| InputError(format!("could not assign libinput to seat {seat}")))?;

        Ok(Self {
            libinput,
            keyboard: KeyboardState::new(keymap)?,
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
                    if let Some(event) = self.keyboard.key(key.key(), pressed, Instant::now()) {
                        translated.push(event);
                    }
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

    /// Rebuilds the compose table immediately; None restores the environment default.
    pub fn set_compose_locale(&mut self, locale: Option<&str>) {
        self.compose_locale = locale.map(str::to_owned);
        let locale = locale
            .map(str::to_owned)
            .unwrap_or_else(default_compose_locale);
        self.keyboard.compose = compose_state(&locale);
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
    compose: Option<compose::State>,
    repeat: Option<Repeat>,
}

#[derive(Clone, Copy)]
struct Repeat {
    keycode: xkb::Keycode,
    deadline: Instant,
}

fn compile_keymap(context: &xkb::Context, settings: &KeymapSettings) -> Option<xkb::Keymap> {
    let options = (!settings.options.is_empty()).then(|| settings.options.clone());
    xkb::Keymap::new_from_names(
        context,
        &settings.rules,
        &settings.model,
        &settings.layout,
        &settings.variant,
        options,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
}

/// Build a compose state for `locale`, or None (logged) if the table
/// does not load — typing must work without compose.
fn compose_state(locale: &str) -> Option<compose::State> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    match compose::Table::new_from_locale(
        &context,
        std::ffi::OsStr::new(locale),
        compose::COMPILE_NO_FLAGS,
    ) {
        Ok(table) => Some(compose::State::new(&table, compose::STATE_NO_FLAGS)),
        Err(()) => {
            eprintln!("vigil-input: no compose table for locale {locale}; compose disabled");
            None
        }
    }
}

/// LC_ALL > LC_CTYPE > LANG > "C", the glibc resolution order.
fn default_compose_locale() -> String {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".into())
}

impl KeyboardState {
    fn new(settings: &KeymapSettings) -> Result<Self, InputError> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // A bad user-supplied layout must never brick the greeter: fall back
        // to the system keymap with a log line (same philosophy as config
        // and theme fallbacks).
        let keymap = compile_keymap(&context, settings)
            .or_else(|| {
                if *settings != KeymapSettings::default() {
                    eprintln!(
                        "vigil-input: keymap {settings:?} did not compile; using system default"
                    );
                    compile_keymap(&context, &KeymapSettings::default())
                } else {
                    None
                }
            })
            .ok_or_else(|| InputError("could not compile the system xkb keymap".into()))?;
        let state = xkb::State::new(&keymap);
        Ok(Self {
            keymap,
            state,
            compose: compose_state(&default_compose_locale()),
            repeat: None,
        })
    }

    fn key(&mut self, evdev_keycode: u32, pressed: bool, now: Instant) -> Option<InputEvent> {
        let keycode = xkb::Keycode::new(evdev_keycode + 8);
        self.state.update_key(
            keycode,
            if pressed {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );

        // Compose sees only presses; releases pass straight through (their
        // keysym text is filtered downstream anyway).
        if pressed
            && let Some(state) = &mut self.compose
            && state.feed(self.state.key_get_one_sym(keycode)) == compose::FeedResult::Accepted
        {
            match state.status() {
                compose::Status::Composing => {
                    // Mid-sequence keys are swallowed and never arm repeat.
                    self.repeat = None;
                    return None;
                }
                compose::Status::Composed => {
                    let utf8 = state.utf8();
                    let keysym = state.keysym().map(|k| k.raw());
                    state.reset();
                    // A composed character does not repeat.
                    self.repeat = None;
                    return Some(InputEvent::Key {
                        keysym: keysym.unwrap_or_else(|| self.state.key_get_one_sym(keycode).raw()),
                        utf8: utf8.filter(|text| !text.is_empty()),
                        pressed: true,
                    });
                }
                compose::Status::Cancelled => {
                    state.reset();
                    self.repeat = None;
                    return None;
                }
                compose::Status::Nothing => {}
            }
        }

        if pressed {
            self.repeat = self.keymap.key_repeats(keycode).then_some(Repeat {
                keycode,
                deadline: now + REPEAT_DELAY,
            });
        } else if self.repeat.is_some_and(|repeat| repeat.keycode == keycode) {
            self.repeat = None;
        }

        Some(self.event(keycode, pressed))
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
    const KEY_GRAVE: u32 = 41;
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

    fn intl_keyboard() -> KeyboardState {
        KeyboardState::new(&KeymapSettings {
            layout: "us".into(),
            variant: "intl".into(),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn translates_letters_shift_and_return() {
        let mut keyboard = KeyboardState::new(&KeymapSettings::default()).unwrap();
        let now = Instant::now();
        assert_eq!(
            key_parts(keyboard.key(KEY_A, true, now).expect("event")),
            ('a' as u32, Some("a".into()), true)
        );
        let _ = keyboard.key(KEY_A, false, now);
        let _ = keyboard.key(KEY_LEFTSHIFT, true, now);
        assert_eq!(
            key_parts(keyboard.key(KEY_A, true, now).expect("event")),
            ('A' as u32, Some("A".into()), true)
        );
        let _ = keyboard.key(KEY_A, false, now);
        let _ = keyboard.key(KEY_LEFTSHIFT, false, now);

        // xkbcommon represents Return text as carriage return, not line feed.
        assert_eq!(
            key_parts(keyboard.key(KEY_ENTER, true, now).expect("event")),
            (0xff0d, Some("\r".into()), true)
        );
    }

    #[test]
    fn caps_lock_state_toggles() {
        let mut keyboard = KeyboardState::new(&KeymapSettings::default()).unwrap();
        let now = Instant::now();
        assert!(!keyboard.caps_lock());
        let _ = keyboard.key(KEY_CAPSLOCK, true, now);
        let _ = keyboard.key(KEY_CAPSLOCK, false, now);
        assert!(keyboard.caps_lock());
        let _ = keyboard.key(KEY_CAPSLOCK, true, now);
        let _ = keyboard.key(KEY_CAPSLOCK, false, now);
        assert!(!keyboard.caps_lock());
    }

    #[test]
    fn repeat_arms_ticks_rearms_and_releases() {
        let mut keyboard = KeyboardState::new(&KeymapSettings::default()).unwrap();
        let now = Instant::now();
        let _ = keyboard.key(KEY_A, true, now);
        let first = now + REPEAT_DELAY;
        assert_eq!(keyboard.next_repeat_deadline(), Some(first));
        assert_eq!(keyboard.tick_repeat(first).len(), 1);
        assert_eq!(
            keyboard.next_repeat_deadline(),
            Some(first + REPEAT_INTERVAL)
        );
        let _ = keyboard.key(KEY_A, false, first);
        assert_eq!(keyboard.next_repeat_deadline(), None);
    }

    #[test]
    fn non_repeating_key_never_arms() {
        let mut keyboard = KeyboardState::new(&KeymapSettings::default()).unwrap();
        let _ = keyboard.key(KEY_LEFTSHIFT, true, Instant::now());
        assert_eq!(keyboard.next_repeat_deadline(), None);
    }

    #[test]
    fn bad_layout_falls_back_to_defaults() {
        let mut keyboard = KeyboardState::new(&KeymapSettings {
            layout: "definitely-not-a-real-layout".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            key_parts(keyboard.key(KEY_A, true, Instant::now()).expect("event")),
            ('a' as u32, Some("a".into()), true)
        );
    }

    #[test]
    fn explicit_layout_compiles() {
        let mut keyboard = KeyboardState::new(&KeymapSettings {
            layout: "us".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            key_parts(keyboard.key(KEY_A, true, Instant::now()).expect("event")),
            ('a' as u32, Some("a".into()), true)
        );
    }

    #[test]
    fn dead_key_composes_accent() {
        let mut kb = intl_keyboard();
        if kb.compose.is_none() {
            return;
        }
        let now = Instant::now();
        assert_eq!(kb.key(KEY_GRAVE, true, now), None);
        assert!(kb.key(KEY_GRAVE, false, now).is_some());
        assert_eq!(
            key_parts(kb.key(KEY_A, true, now).expect("event")),
            (0x00e0, Some("à".into()), true)
        );
    }

    #[test]
    fn cancelled_sequence_swallows() {
        let mut kb = intl_keyboard();
        if kb.compose.is_none() {
            return;
        }
        let now = Instant::now();
        assert_eq!(kb.key(KEY_GRAVE, true, now), None);
        assert_eq!(kb.key(KEY_ENTER, true, now), None);
        assert_eq!(
            key_parts(kb.key(KEY_ENTER, true, now).expect("event")),
            (0xff0d, Some("\r".into()), true)
        );
    }

    #[test]
    fn plain_typing_unaffected_with_compose() {
        let mut kb = intl_keyboard();
        if kb.compose.is_none() {
            return;
        }
        assert_eq!(
            key_parts(kb.key(KEY_A, true, Instant::now()).expect("event")),
            ('a' as u32, Some("a".into()), true)
        );
    }

    #[test]
    fn composing_never_arms_repeat() {
        let mut kb = intl_keyboard();
        if kb.compose.is_none() {
            return;
        }
        let _ = kb.key(KEY_GRAVE, true, Instant::now());
        assert!(kb.next_repeat_deadline().is_none());
    }

    #[test]
    fn bad_locale_disables_compose() {
        let mut kb = KeyboardState::new(&KeymapSettings::default()).unwrap();
        kb.compose = compose_state("xx_NOT_A_LOCALE.UTF-8");
        assert_eq!(
            key_parts(kb.key(KEY_A, true, Instant::now()).expect("event")),
            ('a' as u32, Some("a".into()), true)
        );
    }
}
