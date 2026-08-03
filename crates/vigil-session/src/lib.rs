//! libseat session glue (DESIGN.md §5): acquire the seat, own VT-switch
//! mechanics via smithay's `LibSeatSession`, and surface pause/activate as
//! [`vigil_core::SessionEvent`]s. Logic here is reactive glue only.

use std::os::fd::OwnedFd;
use std::path::Path;

use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionSignal, Session};
use smithay::reexports::rustix::fs::OFlags;
use vigil_core::SessionEvent;

/// The libseat notifier, re-exported opaquely for the binary to register as
/// a calloop event source. Its events translate via [`translate`].
pub use smithay::backend::session::libseat::LibSeatSessionNotifier;

#[derive(Debug)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session: {}", self.0)
    }
}
impl std::error::Error for SessionError {}

/// Owns the libseat session. Constructed once at startup by the binary;
/// the returned notifier is registered as a calloop event source.
pub struct SessionManager {
    session: LibSeatSession,
}

impl SessionManager {
    /// Acquire the seat via libseat (logind or seatd).
    pub fn new() -> Result<(Self, LibSeatSessionNotifier), SessionError> {
        let (session, notifier) = LibSeatSession::new().map_err(|e| SessionError(e.to_string()))?;
        Ok((Self { session }, notifier))
    }

    /// The seat name (e.g. `seat0`), needed by udev and libinput.
    pub fn seat_name(&self) -> String {
        self.session.seat()
    }

    /// Open a device node through the session (libseat brokers the fd, so no
    /// root and no video-group membership is needed).
    pub fn open_device(&mut self, path: &Path) -> Result<OwnedFd, SessionError> {
        self.session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|e| SessionError(e.to_string()))
    }

    /// Whether the session currently holds the seat (false while VT-switched
    /// away).
    pub fn is_active(&self) -> bool {
        self.session.is_active()
    }
}

/// Translate a smithay session signal into the core event. The binary calls
/// this from its notifier callback so it never names smithay types.
pub fn translate(signal: SessionSignal) -> SessionEvent {
    match signal {
        SessionSignal::PauseSession => SessionEvent::Pause,
        SessionSignal::ActivateSession => SessionEvent::Activate,
    }
}
