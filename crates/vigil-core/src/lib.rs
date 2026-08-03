//! Cross-crate interface surface for vigil (DESIGN.md §4).
//!
//! Every type or trait that crosses a crate boundary lives here, and nothing
//! else does. This crate depends on no other vigil crate and nothing
//! heavyweight. Subsystem crates depend on `vigil-core` plus their one
//! vendored subsystem and never on each other; only the binary assembles them.

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// Identifier for a connected output, stable for the life of the connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u32);

/// Static facts about an output at the time it was added.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Connector name as reported by DRM, e.g. `DP-1`.
    pub connector: String,
    /// Active mode, physical pixels.
    pub width: u32,
    pub height: u32,
    /// Vertical refresh in millihertz.
    pub refresh_mhz: u32,
    /// HiDPI scale factor for the Slint window on this output.
    pub scale: f32,
}

/// Output lifecycle, emitted by vigil-outputs.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputEvent {
    Added(OutputId, OutputInfo),
    Removed(OutputId),
    NeedsRedraw(OutputId),
}

// ---------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------

/// One frame's render target: XRGB8888 bytes, row-major, `stride` bytes per
/// row (stride may exceed `width * 4`).
pub struct FrameTarget<'a> {
    pub buffer: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

#[derive(Debug)]
pub enum PresentError {
    /// The output vanished (hotplug/VT switch); the caller should drop this
    /// presenter and wait for the next `OutputEvent`.
    DeviceLost,
    /// Anything else, already formatted for the log.
    Backend(String),
}

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresentError::DeviceLost => write!(f, "output device lost"),
            PresentError::Backend(msg) => write!(f, "present backend: {msg}"),
        }
    }
}

impl std::error::Error for PresentError {}

/// How a rendered frame reaches an output. Implemented by vigil-present-dumb
/// (software, permanent baseline) and vigil-present-gl (GL, M3).
pub trait Presenter {
    fn size(&self) -> (u32, u32);

    /// Hand the caller a target to draw this frame into, then submit it
    /// (page flip). Completion is reported via the event loop, not by
    /// blocking here.
    fn with_frame(&mut self, draw: &mut dyn FnMut(FrameTarget<'_>)) -> Result<(), PresentError>;
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

use std::os::fd::OwnedFd;
use std::path::Path;

/// Opens input device nodes on behalf of libinput. The session subsystem
/// implements this so vigil-input does not depend on a particular seat API.
pub trait DeviceOpener {
    fn open(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, String>;
    fn close(&mut self, fd: OwnedFd);
}

/// Normalized input, decoupled from libinput/xkb types. Key repeat is
/// synthesized by vigil-input and arrives as ordinary `Key` events.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key {
        keysym: u32,
        utf8: Option<String>,
        pressed: bool,
    },
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    PointerAbsolute {
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
}

// ---------------------------------------------------------------------------
// Auth ⇄ UI
// ---------------------------------------------------------------------------

/// The auth state machine's view of the UI (implemented by vigil-ui).
pub trait AuthUi {
    fn show_prompt(&mut self, text: &str, secret: bool);
    fn show_info(&mut self, text: &str);
    fn show_error(&mut self, text: &str);
    fn set_busy(&mut self, busy: bool);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Reboot,
    Poweroff,
}

/// Messages flowing back from the UI to the auth state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum UiMessage {
    Respond(String),
    Cancel,
    SelectSession(usize),
    Power(PowerAction),
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Seat/session lifecycle, emitted by vigil-session. On `Pause` the outputs
/// must stop touching DRM; on `Activate` they re-modeset and redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    Pause,
    Activate,
}

// ---------------------------------------------------------------------------
// Backgrounds
// ---------------------------------------------------------------------------

/// Background fit behavior, one per output (DESIGN.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundFit {
    Stretch,
    #[default]
    Fill,
    Fit,
    Center,
    Tile,
}

impl BackgroundFit {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stretch" => Some(Self::Stretch),
            "fill" => Some(Self::Fill),
            "fit" => Some(Self::Fit),
            "center" => Some(Self::Center),
            "tile" => Some(Self::Tile),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_fit_parses_all_documented_modes() {
        for (s, v) in [
            ("stretch", BackgroundFit::Stretch),
            ("fill", BackgroundFit::Fill),
            ("fit", BackgroundFit::Fit),
            ("center", BackgroundFit::Center),
            ("tile", BackgroundFit::Tile),
        ] {
            assert_eq!(BackgroundFit::parse(s), Some(v));
        }
        assert_eq!(BackgroundFit::parse("cover"), None);
    }
}
