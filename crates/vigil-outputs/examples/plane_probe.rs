//! Print every DRM plane the kernel exposes to this client, with type and
//! formats — the ground truth for cursor-plane availability (#25).
//!
//! Run through the GPU harness (needs DRM master):
//!
//!   cargo build -p vigil-outputs --example plane_probe
//!   tests/gpu/run.sh -- target/debug/examples/plane_probe
//!
//! Prints the plane list twice: once with only universal planes enabled,
//! once after also declaring DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT and
//! ATOMIC — virtualized drivers hide their cursor plane from atomic
//! clients without the hotspot declaration (kernel 6.8+).

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
    println!("== {label}");
    let planes = match card.plane_handles() {
        Ok(p) => p,
        Err(e) => {
            println!("  plane_handles: {e}");
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
                println!(
                    "  plane {:?} {} crtcs={:?} formats=[{}]",
                    handle,
                    ty,
                    info.possible_crtcs(),
                    formats.join(" ")
                );
            }
            Err(e) => println!("  plane {handle:?}: {e}"),
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
    println!("UniversalPlanes: {r:?}");
    dump(&card, "universal planes only");

    // Order matters: the kernel refuses CURSOR_PLANE_HOTSPOT with EINVAL
    // unless the client is already atomic (drm_ioctl.c, drm_setclientcap).
    let r = card.set_client_capability(ClientCapability::Atomic, true);
    println!("Atomic: {r:?}");
    let r = card.set_client_capability(ClientCapability::CursorPlaneHotspot, true);
    println!("CursorPlaneHotspot: {r:?}");
    dump(&card, "atomic + hotspot");
    println!("MODESET OK (probe only; nothing was committed)");
}
