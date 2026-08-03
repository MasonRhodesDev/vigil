//! libseat session glue (DESIGN.md §5): acquire the seat, own VT-switch
//! mechanics via smithay's `LibSeatSession`, and surface pause/activate as
//! [`vigil_core::SessionEvent`]s. Logic here is reactive glue only.

use std::os::fd::OwnedFd;
use std::path::Path;

use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionSignal, Session};
use smithay::reexports::rustix::fs::OFlags;
use vigil_core::{DeviceOpener, SessionEvent};

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

/// Cloneable seat-brokered device opener, so vigil-input can own an opener
/// while the SessionManager stays in the event-loop state.
#[derive(Clone)]
pub struct SessionDeviceOpener {
    session: LibSeatSession,
}

impl SessionManager {
    pub fn device_opener(&self) -> SessionDeviceOpener {
        SessionDeviceOpener {
            session: self.session.clone(),
        }
    }
}

impl DeviceOpener for SessionDeviceOpener {
    fn open(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, String> {
        self.session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|error| error.to_string())
    }

    fn close(&mut self, fd: OwnedFd) {
        let _ = self.session.close(fd);
    }
}

impl DeviceOpener for SessionManager {
    fn open(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, String> {
        self.open_device(path).map_err(|error| error.to_string())
    }

    fn close(&mut self, fd: OwnedFd) {
        let _ = self.session.close(fd);
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
