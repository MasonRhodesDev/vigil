//! `hyprland-surface-v1` client bindings (vendored XML under `protocols/`).
//!
//! `hyprland_surface_v1.set_opacity` is documented to multiply "visual
//! effects such as blur behind the surface in addition to the surface's
//! content" — the one lever that ramps compositor blur strength on Hyprland
//! regardless of `decoration:blur:ignore_opacity`. Optional capability tier;
//! see `SurfaceOpacity` in `lib.rs`.

#![allow(non_upper_case_globals, missing_docs, clippy::all)]

use wayland_client;
use wayland_client::protocol::*;

pub mod __interfaces {
    use wayland_client::protocol::__interfaces::*;
    wayland_scanner::generate_interfaces!("protocols/hyprland-surface-v1.xml");
}
use self::__interfaces::*;

wayland_scanner::generate_client_code!("protocols/hyprland-surface-v1.xml");
