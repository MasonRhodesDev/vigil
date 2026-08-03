//! Input subsystem (DESIGN.md §5): libinput events through xkbcommon state
//! into normalized [`vigil_core::InputEvent`]s. Key repeat and compose are
//! synthesized HERE — nobody else in the stack does repeat. The full public
//! API exists from M1 even where implementation lands later (compose: M2).
//!
//! Hard requirement (DESIGN.md §1): never EVIOCGRAB — host daemons share the
//! seat's input devices while the greeter is up. libinput does not grab by
//! default; nothing in this crate may change that.

use vigil_core::InputEvent;

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
    _private: (),
}

impl InputSystem {
    pub fn new(_seat: &str) -> Result<Self, InputError> {
        todo!("M1: libinput udev context + xkb keymap from system defaults")
    }

    /// Translate one libinput event; may synthesize repeats via the timer
    /// deadline exposed below.
    pub fn translate(&mut self) -> Vec<InputEvent> {
        todo!("M1")
    }

    /// When the repeat timer should next fire, if a repeating key is held.
    pub fn next_repeat_deadline(&self) -> Option<std::time::Instant> {
        todo!("M1")
    }

    /// Current caps-lock state (theme contract `caps-lock`).
    pub fn caps_lock(&self) -> bool {
        todo!("M1")
    }
}
