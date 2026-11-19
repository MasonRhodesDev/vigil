#![forbid(unsafe_code)]

use std::os::fd::OwnedFd;
use std::path::Path;

use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionSignal, Session};
use smithay::reexports::rustix::fs::OFlags;
use vigil_core::{DeviceOpener, SessionEvent};

pub use smithay::backend::session::libseat::LibSeatSessionNotifier;

#[derive(Debug)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session: {}", self.0)
    }
}
impl std::error::Error for SessionError {}

pub struct SessionManager {
    session: LibSeatSession,
}

impl SessionManager {
    pub fn new() -> Result<(Self, LibSeatSessionNotifier), SessionError> {
        let (session, notifier) = LibSeatSession::new().map_err(|e| SessionError(e.to_string()))?;
        Ok((Self { session }, notifier))
    }

    pub fn seat_name(&self) -> String {
        self.session.seat()
    }

    pub fn open_device(&mut self, path: &Path) -> Result<OwnedFd, SessionError> {
        self.session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|e| SessionError(e.to_string()))
    }

    pub fn is_active(&self) -> bool {
        self.session.is_active()
    }

    pub fn change_vt(&mut self, vt: i32) -> Result<(), SessionError> {
        self.session
            .change_vt(vt)
            .map_err(|e| SessionError(e.to_string()))
    }
}

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

pub fn translate(signal: SessionSignal) -> SessionEvent {
    match signal {
        SessionSignal::PauseSession => SessionEvent::Pause,
        SessionSignal::ActivateSession => SessionEvent::Activate,
    }
}
