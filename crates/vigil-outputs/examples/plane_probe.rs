#![forbid(unsafe_code)]
use smithay::reexports::drm::control::{Device as ControlDevice, PlaneType};
use smithay::reexports::drm::{ClientCapability, Device};

struct Card(std::fs::File);

impl std::os::fd::AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

fn dump(card: &Card, label: &str) {
    vigil_log::out!("== {label}");
    let planes = match card.plane_handles() {
        Ok(p) => p,
        Err(e) => {
            vigil_log::out!("  plane_handles: {e}");
            return;
        }
    };
    for handle in planes {
        match card.get_plane(handle) {
            Ok(info) => {
                let ty = card
                    .get_properties(handle)
                    .ok()
                    .and_then(|props| {
                        props.into_iter().find_map(|(prop, value)| {
                            let meta = card.get_property(prop).ok()?;
                            (meta.name().to_str().ok()? == "type").then_some(value)
                        })
                    })
                    .map_or("?".into(), |v| match v {
                        x if x == PlaneType::Primary as u64 => "primary".to_string(),
                        x if x == PlaneType::Cursor as u64 => "cursor".to_string(),
                        x if x == PlaneType::Overlay as u64 => "overlay".to_string(),
                        x => format!("type={x}"),
                    });
                let formats: Vec<String> = info
                    .formats()
                    .iter()
                    .map(|f| f.to_le_bytes().map(|b| (b as char).to_string()).concat())
                    .collect();
                vigil_log::out!(
                    "  plane {:?} {} crtcs={:?} formats=[{}]",
                    handle,
                    ty,
                    info.possible_crtcs(),
                    formats.join(" ")
                );
            }
            Err(e) => vigil_log::out!("  plane {handle:?}: {e}"),
        }
    }
}

fn main() {
    let node = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card0".into());
    let card = Card(
        std::fs::File::options()
            .read(true)
            .write(true)
            .open(&node)
            .expect("open node"),
    );

    let r = card.set_client_capability(ClientCapability::UniversalPlanes, true);
    vigil_log::out!("UniversalPlanes: {r:?}");
    dump(&card, "universal planes only");

    let r = card.set_client_capability(ClientCapability::Atomic, true);
    vigil_log::out!("Atomic: {r:?}");
    let r = card.set_client_capability(ClientCapability::CursorPlaneHotspot, true);
    vigil_log::out!("CursorPlaneHotspot: {r:?}");
    dump(&card, "atomic + hotspot");
    vigil_log::out!("MODESET OK (probe only; nothing was committed)");
}
