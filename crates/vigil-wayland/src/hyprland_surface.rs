#![allow(non_upper_case_globals, missing_docs, clippy::all)]

use wayland_client;
use wayland_client::protocol::*;

pub mod __interfaces {
    use wayland_client::protocol::__interfaces::*;
    wayland_scanner::generate_interfaces!("protocols/hyprland-surface-v1.xml");
}
use self::__interfaces::*;

wayland_scanner::generate_client_code!("protocols/hyprland-surface-v1.xml");
