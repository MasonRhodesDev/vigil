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
use smithay::reexports::drm::control::{
    Device as ControlDevice, Mode, ModeTypeFlags, connector, crtc,
};
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

/// Every GPU DRM node on the seat, primary (boot_vga) first, the rest in
/// stable path order. Outputs may span cards (laptop dGPU driving a port),
/// so the greeter runs one [`OutputManager`] per entry.
pub fn all_gpu_paths(seat: &str) -> Result<Vec<std::path::PathBuf>, OutputsError> {
    let primary = smithay::backend::udev::primary_gpu(seat).map_err(err)?;
    let mut paths = smithay::backend::udev::all_gpus(seat).map_err(err)?;
    paths.sort();
    if let Some(primary) = primary
        && let Some(pos) = paths.iter().position(|p| *p == primary)
    {
        let primary = paths.remove(pos);
        paths.insert(0, primary);
    }
    if paths.is_empty() {
        return Err(OutputsError(format!("no GPU found on seat {seat}")));
    }
    Ok(paths)
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
    /// Distinguishes this GPU's [`OutputId`]s from other cards' (connector
    /// handles are only unique per device).
    namespace: u32,
}

impl OutputManager {
    /// Take ownership of an opened DRM node (from vigil-session in
    /// production; opened directly in dev harnesses) and prepare it for
    /// modesetting. `namespace` is the GPU's index (0 for a single-GPU
    /// setup) and is folded into every `OutputId`. Returns the notifier the
    /// binary registers for vblank events.
    pub fn new(fd: OwnedFd, namespace: u32) -> Result<(Self, DrmDeviceNotifier), OutputsError> {
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (device, notifier) = DrmDevice::new(fd, true).map_err(err)?;
        Ok((
            Self {
                device,
                scanner: DrmScanner::new(),
                entries: HashMap::new(),
                namespace,
            },
            notifier,
        ))
    }

    fn make_id(&self, handle: connector::Handle) -> OutputId {
        let raw: u32 = handle.into();
        OutputId(self.namespace << 24 | (raw & 0x00ff_ffff))
    }

    /// The GPU index this manager was created with (`OutputId` namespace).
    pub fn namespace(&self) -> u32 {
        self.namespace
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
                    let id = self.make_id(connector.handle());
                    let (w, h) = mode.size();
                    let (make, model) = read_edid(&self.device, connector.handle())
                        .map(|edid| parse_edid(&edid))
                        .unwrap_or((None, None));
                    let info = OutputInfo {
                        connector: connector_name(&connector),
                        width: w as u32,
                        height: h as u32,
                        refresh_mhz: mode.vrefresh() * 1000,
                        make,
                        model,
                        scale: scale_for(w as u32, connector.size()),
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
                    let id = self.make_id(connector.handle());
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

/// Raw EDID bytes from the connector's `EDID` property blob. `None` for any
/// failure: a monitor that reports no EDID must still light up.
fn read_edid(device: &DrmDevice, handle: connector::Handle) -> Option<Vec<u8>> {
    let props = device.get_properties(handle).ok()?;
    for (prop, raw) in props.iter() {
        let info = device.get_property(*prop).ok()?;
        if info.name().to_str() == Ok("EDID") {
            let blob = device.get_property_blob(*raw).ok()?;
            return (!blob.is_empty()).then_some(blob);
        }
    }
    None
}

/// `(make, model)` from an EDID block: the PNP id packed into bytes 8-9
/// (three 5-bit letters, big-endian) and the 0xFC descriptor's model name.
/// Every malformed input yields `None`s rather than panicking — this parses
/// bytes a monitor supplied.
fn parse_edid(edid: &[u8]) -> (Option<String>, Option<String>) {
    if edid.len() < 128 {
        return (None, None);
    }
    let packed = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |shift: u16| -> Option<char> {
        let v = ((packed >> shift) & 0x1f) as u8;
        (1..=26).contains(&v).then(|| (b'A' + v - 1) as char)
    };
    let make = match (letter(10), letter(5), letter(0)) {
        (Some(a), Some(b), Some(c)) => Some(format!("{a}{b}{c}")),
        _ => None,
    };
    // Four 18-byte descriptors start at byte 54; type 0xFC is the model name.
    let model = (0..4)
        .map(|i| 54 + i * 18)
        .filter(|&at| at + 18 <= edid.len())
        .find(|&at| edid[at..at + 3] == [0, 0, 0] && edid[at + 3] == 0xfc)
        .and_then(|at| {
            let text: String = edid[at + 5..at + 18]
                .iter()
                .take_while(|&&b| b != 0x0a)
                .map(|&b| b as char)
                .collect();
            let text = text.trim().to_owned();
            (!text.is_empty()).then_some(text)
        });
    (make, model)
}

/// Scale for a panel of `size_mm` showing `width_px`, snapped to the steps
/// compositors actually offer. Bands chosen against real hardware: 1440p at
/// 27" (109 dpi) stays 1.0, 4K at 27" (163 dpi) and a 16" 2560x1600 panel
/// (188 dpi) both land on 1.5. No physical size (some monitors report 0)
/// means no basis to guess: 1.0.
fn scale_for(width_px: u32, size_mm: Option<(u32, u32)>) -> f32 {
    let Some((mm_w, _)) = size_mm else {
        return 1.0;
    };
    if mm_w == 0 || width_px == 0 {
        return 1.0;
    }
    let dpi = f64::from(width_px) * 25.4 / f64::from(mm_w);
    match dpi {
        d if d < 120.0 => 1.0,
        d if d < 145.0 => 1.25,
        d if d < 195.0 => 1.5,
        d if d < 240.0 => 1.75,
        _ => 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edid(make: [u8; 2], name: &str) -> Vec<u8> {
        let mut block = vec![0; 128];
        block[8..10].copy_from_slice(&make);
        block[54..59].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
        block[59..72].fill(b' ');
        let bytes = name.as_bytes();
        block[59..59 + bytes.len()].copy_from_slice(bytes);
        block[59 + bytes.len()] = 0x0a;
        block
    }

    #[test]
    fn parses_pnp_and_model() {
        assert_eq!(
            parse_edid(&edid([0x10, 0xac], "S2725QC")),
            (Some("DEL".to_owned()), Some("S2725QC".to_owned()))
        );
    }

    #[test]
    fn short_edid_is_none() {
        assert_eq!(parse_edid(&[0u8; 40]), (None, None));
        assert_eq!(parse_edid(&[]), (None, None));
    }

    #[test]
    fn missing_model_descriptor_is_none() {
        let mut block = vec![0; 128];
        block[8..10].copy_from_slice(&[0x10, 0xac]);
        assert_eq!(parse_edid(&block), (Some("DEL".to_owned()), None));
    }

    #[test]
    fn scale_bands_match_real_hardware() {
        assert_eq!(scale_for(2560, Some((597, 336))), 1.0);
        assert_eq!(scale_for(3840, Some((597, 336))), 1.5);
        assert_eq!(scale_for(2560, Some((345, 215))), 1.5);
        assert_eq!(scale_for(1920, Some((531, 299))), 1.0);
    }

    #[test]
    fn scale_without_physical_size_is_one() {
        assert_eq!(scale_for(3840, None), 1.0);
        assert_eq!(scale_for(3840, Some((0, 0))), 1.0);
    }
}
