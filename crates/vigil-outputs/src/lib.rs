//! Output manager (DESIGN.md §5): udev GPU discovery, DRM device/surface
//! ownership, connector hotplug via `DrmScanner`, modesetting, and
//! suspend/resume re-modeset. Emits [`vigil_core::OutputEvent`]s and hands
//! each output a `Presenter`.
//!
//! Architectural commitment: multi-output is the core object model — a
//! single monitor is the N=1 case of the same code (DESIGN.md §3).

use vigil_core::{OutputEvent, OutputId};

#[derive(Debug)]
pub struct OutputsError(pub String);

impl std::fmt::Display for OutputsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "outputs: {}", self.0)
    }
}
impl std::error::Error for OutputsError {}

/// Owns every connected output: DRM surface, presenter, per-output state.
pub struct OutputManager {
    _private: (),
}

impl OutputManager {
    /// Open the primary GPU for `_seat` and modeset all connected outputs.
    pub fn new(_seat: &str) -> Result<Self, OutputsError> {
        todo!("M1: udev primary GPU + DrmDevice + initial connector scan")
    }

    /// React to a udev change event (connector hotplug); returns lifecycle
    /// events for the binary to route to the UI.
    pub fn handle_hotplug(&mut self) -> Vec<OutputEvent> {
        todo!("M1: DrmScanner rescan -> Added/Removed")
    }

    /// Session paused (VT switch/suspend): stop touching DRM.
    pub fn pause(&mut self) {
        todo!("M1")
    }

    /// Session activated: re-modeset everything and request full redraws.
    pub fn activate(&mut self) -> Vec<OutputEvent> {
        todo!("M1")
    }

    /// Iterate the live output ids.
    pub fn ids(&self) -> Vec<OutputId> {
        todo!("M1")
    }
}
