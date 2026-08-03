//! The vigil binary: calloop wiring ONLY (DESIGN.md §5). Every subsystem
//! lives in its crate; this file assembles them behind vigil-core seams.
//!
//! M1 wiring order: SessionManager -> OutputManager + InputSystem on the
//! seat -> VigilPlatform + OutputWindows per output -> AuthMachine -> run
//! the loop (session/udev/libinput/greetd/timer sources).

fn main() {
    // Scaffold: exit nonzero so a vigil-launch wrapper falls through to the
    // fallback greeter instead of presenting a half-built login screen.
    eprintln!(
        "vigil {}: scaffolding only — M1 implementation pending",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(1);
}
