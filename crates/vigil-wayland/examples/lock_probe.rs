//! Lock-state oracle for the nested-compositor harness (tests/nested/).
//!
//! Requests an ext-session-lock. If the compositor grants it (`locked`
//! event), the session WAS unlocked — the probe immediately unlocks and
//! exits 0. If the compositor answers `finished`, another client holds the
//! lock — the session IS locked — exit 10. Anything else exits 2.
//!
//! Only run at quiescent checkpoints: while a vigil warning is pending the
//! probe would win the lock race and steal the session lock.

use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
    SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_surface;
use wayland_client::{Connection, QueueHandle};

struct Probe {
    registry_state: RegistryState,
    outcome: Option<i32>,
    lock: Option<SessionLock>,
}

impl SessionLockHandler for Probe {
    fn locked(&mut self, conn: &Connection, _qh: &QueueHandle<Self>, session_lock: SessionLock) {
        // Granted: the session was unlocked. Restore it before reporting.
        session_lock.unlock();
        let _ = conn.roundtrip();
        self.outcome = Some(0);
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        // Denied: someone else holds the lock — the session is locked.
        self.outcome = Some(10);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: SessionLockSurface,
        _: SessionLockSurfaceConfigure,
        _: u32,
    ) {
    }
}

impl ProvidesRegistryState for Probe {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

delegate_registry!(Probe);
smithay_client_toolkit::delegate_dispatch2!(Probe);
wayland_client::delegate_noop!(Probe: ignore wl_surface::WlSurface);

fn main() {
    let conn = Connection::connect_to_env().expect("wayland connection");
    let (globals, mut queue) = registry_queue_init::<Probe>(&conn).expect("registry");
    let qh = queue.handle();
    let lock_state = SessionLockState::new(&globals, &qh);
    let mut probe = Probe {
        registry_state: RegistryState::new(&globals),
        outcome: None,
        lock: None,
    };
    probe.lock = match lock_state.lock(&qh) {
        Ok(lock) => Some(lock),
        Err(error) => {
            eprintln!("lock_probe: ext-session-lock-v1 unavailable: {error}");
            std::process::exit(2);
        }
    };
    for _ in 0..100 {
        if queue.blocking_dispatch(&mut probe).is_err() {
            eprintln!("lock_probe: dispatch failed");
            std::process::exit(2);
        }
        if let Some(code) = probe.outcome {
            println!(
                "lock_probe: session was {}",
                if code == 0 { "unlocked" } else { "locked" }
            );
            std::process::exit(code);
        }
    }
    eprintln!("lock_probe: no verdict after 100 dispatches");
    std::process::exit(2);
}
