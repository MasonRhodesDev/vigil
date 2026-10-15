//! Output manager (DESIGN.md §5): DRM device/surface ownership, connector
//! hotplug via `DrmScanner`, modesetting, and suspend/resume re-modeset.
//! Emits [`vigil_core::OutputEvent`]s; surfaces are handed to the binary,
//! which pairs each with a `Presenter`.
//!
//! Architectural commitment: multi-output is the core object model — a
//! single monitor is the N=1 case of the same code (DESIGN.md §3).

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::OnceLock;

use smithay::backend::drm::{DrmDevice, DrmDeviceNotifier};
use smithay::reexports::drm::control::{
    Device as ControlDevice, Mode, ModeTypeFlags, connector, crtc,
};
use smithay::reexports::drm::{ClientCapability, Device as _};
use smithay::utils::DeviceFd;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use vigil_core::{OutputEvent, OutputId, OutputInfo};

/// Re-exported opaquely so the binary can hand surfaces to a presenter
/// without naming smithay itself.
pub use smithay::backend::drm::{DrmDeviceFd, DrmEvent, DrmSurface};

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
        // Virtualized drivers (virtio-gpu, vmwgfx) hide their cursor plane
        // from atomic clients unless the client declares it will supply
        // cursor hotspots (DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT, kernel
        // 6.8+). Declare before smithay enumerates planes. Our hotspot is
        // the arrow tip at (0, 0) — the property's default — so declaring
        // costs nothing. Both best-effort: legacy-only devices refuse the
        // atomic cap, older kernels the hotspot cap, and either way the
        // plane is simply absent, which the GL policy already handles.
        //
        // Order matters: the kernel refuses the hotspot declaration with
        // EINVAL unless the client is ALREADY atomic (verified with
        // examples/plane_probe.rs in the GPU harness — hotspot-then-atomic
        // leaves virtio-gpu's cursor plane hidden). Setting atomic here is
        // idempotent with smithay doing it again inside DrmDevice::new.
        {
            use std::os::fd::AsFd;
            let raw = RawDrm(fd.as_fd());
            let _ = raw.set_client_capability(ClientCapability::Atomic, true);
            let _ = raw.set_client_capability(ClientCapability::CursorPlaneHotspot, true);
        }
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

    /// Every mode the connector reports, as `(width, height, refresh_hz)`.
    /// The binary picks one for a profile and calls [`Self::set_mode`].
    pub fn modes(&self, id: OutputId) -> Vec<(u32, u32, u32)> {
        self.entries
            .get(&id)
            .and_then(|e| self.scanner.connectors().get(&e.connector))
            .map(|conn| {
                conn.modes()
                    .iter()
                    .map(|m| {
                        let (w, h) = m.size();
                        (w as u32, h as u32, m.vrefresh())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Choose the mode used by the next [`Self::create_surface`]. Must be one
    /// of [`Self::modes`]; an unknown mode is refused so a profile typo cannot
    /// hand DRM a mode the connector never advertised. Updates the cached
    /// `OutputInfo` so the binary sees the geometry it will actually get.
    pub fn set_mode(&mut self, id: OutputId, mode: (u32, u32, u32)) -> Result<(), OutputsError> {
        let connector = self
            .entries
            .get(&id)
            .map(|e| e.connector)
            .ok_or_else(|| OutputsError(format!("unknown output {id:?}")))?;
        let found = self
            .scanner
            .connectors()
            .get(&connector)
            .and_then(|conn| {
                conn.modes().iter().copied().find(|m| {
                    let (w, h) = m.size();
                    (w as u32, h as u32, m.vrefresh()) == mode
                })
            })
            .ok_or_else(|| OutputsError(format!("output {id:?} has no mode {mode:?}")))?;
        let entry = self.entries.get_mut(&id).expect("checked above");
        let (w, h) = found.size();
        entry.mode = found;
        entry.info.width = w as u32;
        entry.info.height = h as u32;
        entry.info.refresh_mhz = found.vrefresh() * 1000;
        Ok(())
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

    /// The DRM device this manager owns.
    ///
    /// A GL presenter allocates its buffers on the same device and creates
    /// framebuffers against it. Cloning shares the open file description,
    /// which is what DRM master rides on -- opening the node again would not
    /// be master.
    pub fn device_fd(&self) -> smithay::backend::drm::DrmDeviceFd {
        self.device.device_fd().clone()
    }

    /// Whether the device does atomic modesetting (the cursor plane path
    /// needs it; smithay's legacy fallback drives only the primary plane).
    pub fn is_atomic(&self) -> bool {
        self.device.is_atomic()
    }

    /// Whether the kernel still backs this device. False after a surprise
    /// removal (dock GPU unplugged): every ioctl on the fd is ENODEV from
    /// then on, and the manager is only good for dropping.
    pub fn alive(&self) -> bool {
        use std::os::fd::AsFd;
        RawDrm(self.device.device_fd().as_fd())
            .get_driver_capability(smithay::reexports::drm::DriverCapability::DumbBuffer)
            .is_ok()
    }

    /// Live output ids.
    pub fn ids(&self) -> Vec<OutputId> {
        self.entries.keys().copied().collect()
    }
}

/// Minimal DRM handle for setting client caps before smithay owns the fd.
struct RawDrm<'a>(std::os::fd::BorrowedFd<'a>);

impl std::os::fd::AsFd for RawDrm<'_> {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0
    }
}

impl smithay::reexports::drm::Device for RawDrm<'_> {}

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

/// hwdata's PNP registry: the table Hyprland's description comes from.
/// aquamarine snapshots this file when it is built and substitutes the vendor
/// name for the EDID's three-letter id; reading the installed copy gives the
/// same answer on the same distro.
const PNP_IDS_PATH: &str = "/usr/share/hwdata/pnp.ids";

/// Used only when the registry is missing, so the ids that already matched
/// before vigil read hwdata keep matching on a box without it.
const PNP_FALLBACK: &[(&str, &str)] = &[("DEL", "Dell Inc."), ("GSM", "LG Electronics")];

/// `pnp.ids` lines are `ID<TAB>Vendor name`.
fn parse_pnp_ids(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(id, vendor)| id.len() == 3 && !vendor.trim().is_empty())
        .map(|(id, vendor)| (id.to_owned(), vendor.trim().to_owned()))
        .collect()
}

/// The vendor name for a PNP id, as Hyprland spells it.
fn pnp_vendor(id: &str) -> Option<String> {
    static TABLE: OnceLock<Option<HashMap<String, String>>> = OnceLock::new();
    match TABLE.get_or_init(|| {
        std::fs::read_to_string(PNP_IDS_PATH)
            .ok()
            .map(|t| parse_pnp_ids(&t))
    }) {
        Some(table) => table.get(id).cloned(),
        None => PNP_FALLBACK
            .iter()
            .find(|(pnp, _)| *pnp == id)
            .map(|(_, name)| (*name).to_owned()),
    }
}

/// `(make, model)` from an EDID block, built the way Hyprland builds an
/// output's description so a `desc:` selector copied from `hyprctl monitors`
/// matches here: aquamarine's make (the vendor name for the PNP id packed
/// into bytes 8-9, or the id itself), then libdisplay-info's model and
/// serial. The serial rides on the model, since vigil keeps two fields.
/// Every malformed input yields `None`s rather than panicking — this parses
/// bytes a monitor supplied.
fn parse_edid(edid: &[u8]) -> (Option<String>, Option<String>) {
    parse_edid_with(edid, pnp_vendor)
}

fn parse_edid_with(
    edid: &[u8],
    vendor: impl Fn(&str) -> Option<String>,
) -> (Option<String>, Option<String>) {
    if edid.len() < 128 {
        return (None, None);
    }
    let packed = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |shift: u16| -> Option<char> {
        let v = ((packed >> shift) & 0x1f) as u8;
        (1..=26).contains(&v).then(|| (b'A' + v - 1) as char)
    };
    let make = match (letter(10), letter(5), letter(0)) {
        (Some(a), Some(b), Some(c)) => {
            let pnp = format!("{a}{b}{c}");
            Some(vendor(&pnp).unwrap_or(pnp))
        }
        _ => None,
    };
    // di_info_get_model: the product name descriptor, else the product code.
    let mut model = descriptor(edid, 0xfc)
        .unwrap_or_else(|| format!("0x{:04X}", u16::from_le_bytes([edid[10], edid[11]])));
    // di_info_get_serial: the serial descriptor, else the numeric serial
    // when it is not zero.
    let numeric = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);
    let serial =
        descriptor(edid, 0xff).or_else(|| (numeric != 0).then(|| format!("0x{numeric:08X}")));
    if let Some(serial) = serial {
        model.push(' ');
        model.push_str(&serial);
    }
    (make, Some(model))
}

/// Text from one of the four 18-byte display descriptors.
fn descriptor(edid: &[u8], tag: u8) -> Option<String> {
    if edid.len() < 128 {
        return None;
    }
    (0..4)
        .map(|i| 54 + i * 18)
        .filter(|&at| at + 18 <= edid.len())
        .find(|&at| edid[at..at + 3] == [0, 0, 0] && edid[at + 3] == tag)
        .and_then(|at| {
            let text: String = edid[at + 5..at + 18]
                .iter()
                .take_while(|&&b| b != 0x0a)
                .map(|&b| b as char)
                .collect();
            let text = text.trim().to_owned();
            (!text.is_empty()).then_some(text)
        })
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

    fn edid(make: [u8; 2], product: [u8; 2], name: Option<&str>, serial: Option<&str>) -> Vec<u8> {
        let mut block = vec![0; 128];
        block[8..10].copy_from_slice(&make);
        block[10..12].copy_from_slice(&product);
        for (slot, (tag, text)) in [(0, (0xfc, name)), (1, (0xff, serial))] {
            let Some(text) = text else { continue };
            let at = 54 + slot * 18;
            block[at..at + 5].copy_from_slice(&[0, 0, 0, tag, 0]);
            block[at + 5..at + 18].fill(b' ');
            let bytes = text.as_bytes();
            block[at + 5..at + 5 + bytes.len()].copy_from_slice(bytes);
            block[at + 5 + bytes.len()] = 0x0a;
        }
        block
    }

    /// A slice of hwdata's pnp.ids, tab-separated like the real file.
    const PNP: &str = "BOE\tBOE\nDEL\tDell Inc.\nGSM\tLG Electronics\nHPN\tHP Inc.\n";

    fn parse(edid: &[u8]) -> (Option<String>, Option<String>) {
        let table = parse_pnp_ids(PNP);
        parse_edid_with(edid, |id| table.get(id).cloned())
    }

    fn description(edid: &[u8]) -> String {
        let (make, model) = parse(edid);
        [make, model]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// mason-work's desk, read from /sys/class/drm/*/edid, against the
    /// descriptions `hyprctl monitors` printed for the same panels. The HP
    /// is the one vigil used to spell "HPN HP E243 ...", which no profile
    /// written against Hyprland could match.
    #[test]
    fn real_edids_match_hyprland_descriptions() {
        for (edid, hyprland) in [
            (
                &include_bytes!("../testdata/mason-work-DP-1.edid")[..],
                "Dell Inc. DELL S2721HGF 85YFP83",
            ),
            (
                &include_bytes!("../testdata/mason-work-DP-4.edid")[..],
                "HP Inc. HP E243 CNK7510Y4B",
            ),
            (
                &include_bytes!("../testdata/mason-work-eDP-2.edid")[..],
                "BOE 0x0BC9",
            ),
        ] {
            assert_eq!(description(edid), hyprland);
        }
    }

    #[test]
    fn builds_libdisplay_info_description() {
        assert_eq!(
            parse(&edid(
                [0x10, 0xac],
                [0, 0],
                Some("DELL S2725QC"),
                Some("5DGMS84")
            )),
            (
                Some("Dell Inc.".to_owned()),
                Some("DELL S2725QC 5DGMS84".to_owned())
            )
        );
    }

    #[test]
    fn falls_back_to_product_code() {
        assert_eq!(
            parse(&edid([0x09, 0xe5], [0xc9, 0x0b], None, None)),
            (Some("BOE".to_owned()), Some("0x0BC9".to_owned()))
        );
    }

    /// No serial descriptor but a numeric serial: libdisplay-info prints it
    /// as eight hex digits, which is how Hyprland names an LG TV
    /// ("LG Electronics LG TV 0x01010101").
    #[test]
    fn numeric_serial_without_descriptor() {
        let mut block = edid([0x1e, 0x6d], [0, 0], Some("LG TV"), None);
        block[12..16].copy_from_slice(&[1, 1, 1, 1]);
        assert_eq!(description(&block), "LG Electronics LG TV 0x01010101");
    }

    /// The serial descriptor wins over a numeric serial (the HP has both).
    #[test]
    fn serial_descriptor_beats_numeric_serial() {
        let mut block = edid([0x22, 0x0e], [0, 0], Some("HP E243"), Some("CNK7510Y4B"));
        block[12..16].copy_from_slice(&[1, 1, 1, 1]);
        assert_eq!(description(&block), "HP Inc. HP E243 CNK7510Y4B");
    }

    #[test]
    fn unknown_pnp_passes_through() {
        assert_eq!(
            parse(&edid([0x09, 0xd1], [0, 0], Some("BenQ Monitor"), None)),
            (Some("BNQ".to_owned()), Some("BenQ Monitor".to_owned()))
        );
    }

    #[test]
    fn pnp_ids_parse_skips_comments_and_junk() {
        let table =
            parse_pnp_ids("# comment\nHPN\tHP Inc.\nTOOLONG\tx\nAAA\t\n\nDEL\tDell Inc.  \n");
        assert_eq!(table.len(), 2);
        assert_eq!(table["HPN"], "HP Inc.");
        assert_eq!(table["DEL"], "Dell Inc.");
    }

    #[test]
    fn short_edid_is_none() {
        assert_eq!(parse(&[0u8; 40]), (None, None));
        assert_eq!(parse(&[]), (None, None));
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
