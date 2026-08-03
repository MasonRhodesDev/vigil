//! libseat session glue (DESIGN.md §5): acquire the seat, own VT-switch
//! mechanics via smithay's `LibSeatSession`, and surface pause/activate as
//! [`vigil_core::SessionEvent`]s. Logic here is reactive glue only.

use vigil_core::SessionEvent;

#[derive(Debug)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session: {}", self.0)
    }
}
impl std::error::Error for SessionError {}

/// Owns the libseat session. Constructed once at startup by the binary;
/// its notifier is registered as a calloop event source.
pub struct SessionManager {
    _private: (),
}

impl SessionManager {
    /// Acquire the seat via libseat (logind or seatd).
    pub fn new() -> Result<Self, SessionError> {
        todo!("M1: LibSeatSession::new + notifier -> SessionEvent translation")
    }

    /// The seat name (e.g. `seat0`), needed by udev and libinput.
    pub fn seat_name(&self) -> &str {
        todo!("M1")
    }

    /// Translate a smithay session signal into the core event.
    pub fn translate(_active: bool) -> SessionEvent {
        todo!("M1")
    }
}
