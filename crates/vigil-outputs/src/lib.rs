//! Output manager (DESIGN.md §5): DRM device/surface ownership, connector
//! hotplug via `DrmScanner`, modesetting, and suspend/resume re-modeset.
//! Emits [`vigil_core::OutputEvent`]s; surfaces are handed to the binary,
//! which pairs each with a `Presenter`.
//!
//! Architectural commitment: multi-output is the core object model — a
//! single monitor is the N=1 case of the same code (DESIGN.md §3).

use std::collections::HashMap;
use std::os::fd::OwnedFd;

use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmDeviceNotifier};
use smithay::reexports::drm::control::{Mode, ModeTypeFlags, connector, crtc};
use smithay::utils::DeviceFd;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use vigil_core::{OutputEvent, OutputId, OutputInfo};

/// Re-exported opaquely so the binary can hand surfaces to a presenter
/// without naming smithay itself.
pub use smithay::backend::drm::DrmSurface;

/// Opaque udev monitor: a calloop event source firing on GPU/connector
/// changes. The binary registers it and calls [`OutputManager::scan`] on any
/// event without naming smithay types.
pub use smithay::backend::udev::UdevBackend as UdevMonitor;

/// Path of the seat's primary GPU DRM node.
pub fn primary_gpu_path(seat: &str) -> Result<std::path::PathBuf, OutputsError> {
    smithay::backend::udev::primary_gpu(seat)
        .map_err(err)?
        .ok_or_else(|| OutputsError(format!("no GPU found on seat {seat}")))
}

/// A udev monitor for the seat's DRM subsystem.
pub fn udev_monitor(seat: &str) -> Result<UdevMonitor, OutputsError> {
    smithay::backend::udev::UdevBackend::new(seat).map_err(err)
}

#[derive(Debug)]
pub struct OutputsError(pub String);

impl std::fmt::Display for OutputsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "outputs: {}", self.0)
    }
}
impl std::error::Error for OutputsError {}

fn err(e: impl std::fmt::Display) -> OutputsError {
    OutputsError(e.to_string())
}

struct Entry {
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    info: OutputInfo,
}

/// Owns the DRM device for one GPU and every connected output on it.
pub struct OutputManager {
    device: DrmDevice,
    scanner: DrmScanner,
    entries: HashMap<OutputId, Entry>,
}

impl OutputManager {
    /// Take ownership of an opened DRM node (from vigil-session in
    /// production; opened directly in dev harnesses) and prepare it for
    /// modesetting. Returns the notifier the binary registers for vblank
    /// events.
    pub fn new(fd: OwnedFd) -> Result<(Self, DrmDeviceNotifier), OutputsError> {
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (device, notifier) = DrmDevice::new(fd, true).map_err(err)?;
        Ok((
            Self {
                device,
                scanner: DrmScanner::new(),
                entries: HashMap::new(),
            },
            notifier,
        ))
    }

    /// Rescan connectors (startup and on udev hotplug). Returns lifecycle
    /// events for the binary to act on.
    pub fn scan(&mut self) -> Result<Vec<OutputEvent>, OutputsError> {
        let mut events = Vec::new();
        for scan_event in self.scanner.scan_connectors(&self.device).map_err(err)? {
            match scan_event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    let Some(mode) = preferred_mode(&connector) else {
                        continue;
                    };
                    let id = OutputId(connector.handle().into());
                    let (w, h) = mode.size();
                    let info = OutputInfo {
                        connector: connector_name(&connector),
                        width: w as u32,
                        height: h as u32,
                        refresh_mhz: mode.vrefresh() * 1000,
                        scale: 1.0,
                    };
                    self.entries.insert(
                        id,
                        Entry {
                            connector: connector.handle(),
                            crtc,
                            mode,
                            info: info.clone(),
                        },
                    );
                    events.push(OutputEvent::Added(id, info));
                }
                DrmScanEvent::Disconnected { connector, .. } => {
                    let id = OutputId(connector.handle().into());
                    if self.entries.remove(&id).is_some() {
                        events.push(OutputEvent::Removed(id));
                    }
                }
                // Connected without a free CRTC: more monitors than the GPU
                // can drive; skip (logged by the binary via the event gap).
                DrmScanEvent::Connected { crtc: None, .. } => {}
            }
        }
        Ok(events)
    }

    /// Create the DRM surface for an output; the binary wraps it in a
    /// presenter. Call once per `OutputEvent::Added`.
    pub fn create_surface(&mut self, id: OutputId) -> Result<DrmSurface, OutputsError> {
        let entry = self
            .entries
            .get(&id)
            .ok_or_else(|| OutputsError(format!("unknown output {id:?}")))?;
        self.device
            .create_surface(entry.crtc, entry.mode, &[entry.connector])
            .map_err(err)
    }

    /// Facts about a live output.
    pub fn info(&self, id: OutputId) -> Option<&OutputInfo> {
        self.entries.get(&id).map(|e| &e.info)
    }

    /// Session paused (VT switch/suspend): stop touching DRM.
    pub fn pause(&mut self) {
        self.device.pause();
    }

    /// Session activated: reclaim the device. Surfaces need a fresh modeset;
    /// the binary requests redraws (presenters re-commit on next frame).
    pub fn activate(&mut self) -> Vec<OutputEvent> {
        // `false` = do not reset state; surfaces re-commit themselves.
        if self.device.activate(false).is_err() {
            return Vec::new();
        }
        self.entries
            .keys()
            .map(|id| OutputEvent::NeedsRedraw(*id))
            .collect()
    }

    /// Live output ids.
    pub fn ids(&self) -> Vec<OutputId> {
        self.entries.keys().copied().collect()
    }
}

/// The connector's preferred mode, falling back to its first (largest) mode.
fn preferred_mode(conn: &connector::Info) -> Option<Mode> {
    conn.modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| conn.modes().first())
        .copied()
}

/// Human name matching kernel conventions, e.g. `DP-1`, `eDP-2`, `Virtual-1`.
fn connector_name(conn: &connector::Info) -> String {
    use connector::Interface as I;
    let prefix = match conn.interface() {
        I::HDMIA => "HDMI-A",
        I::HDMIB => "HDMI-B",
        I::DisplayPort => "DP",
        I::EmbeddedDisplayPort => "eDP",
        I::DVID => "DVI-D",
        I::DVII => "DVI-I",
        I::DVIA => "DVI-A",
        I::LVDS => "LVDS",
        I::VGA => "VGA",
        I::Virtual => "Virtual",
        I::DSI => "DSI",
        I::DPI => "DPI",
        _ => "Unknown",
    };
    format!("{}-{}", prefix, conn.interface_id())
}
