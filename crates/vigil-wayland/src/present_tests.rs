use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1;
use smithay_client_toolkit::shm::slot::SlotPool;
use wayland_backend::protocol::{Argument, Message};
use wayland_backend::rs::server::{
    Backend, ClientId, GlobalHandler, GlobalId, Handle, ObjectData, ObjectId,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_compositor, wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, EventQueue, Proxy};
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::ExtSessionLockManagerV1;

use super::{App, FRAME, LockSession, OutputId, OutputInfo, Runtime, WaitDecision};

// A compositor that never answers: the client may bind and create whatever
// it asks for, and every commit that carries a buffer is written down.
#[derive(Default)]
struct Frames {
    attached: std::collections::BTreeSet<u32>,
    committed: Vec<u32>,
}

struct Mute(Arc<Mutex<Frames>>);

impl GlobalHandler<()> for Mute {
    fn bind(
        self: Arc<Self>,
        _: &Handle,
        _: &mut (),
        _: ClientId,
        _: GlobalId,
        _: ObjectId,
    ) -> Arc<dyn ObjectData<()>> {
        self
    }
}

impl ObjectData<()> for Mute {
    fn request(
        self: Arc<Self>,
        _: &Handle,
        _: &mut (),
        _: ClientId,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<()>>> {
        if msg.sender_id.interface().name == wl_surface::WlSurface::interface().name {
            let surface = msg.sender_id.protocol_id();
            let mut frames = self.0.lock().unwrap();
            if msg.opcode == wl_surface::REQ_ATTACH_OPCODE {
                frames.attached.insert(surface);
            } else if msg.opcode == wl_surface::REQ_COMMIT_OPCODE
                && frames.attached.remove(&surface)
            {
                frames.committed.push(surface);
            }
        }
        msg.args
            .iter()
            .any(|arg| matches!(arg, Argument::NewId(_)))
            .then_some(self as Arc<dyn ObjectData<()>>)
    }

    fn destroyed(self: Arc<Self>, _: &Handle, _: &mut (), _: ClientId, _: ObjectId) {}
}

#[derive(Default)]
struct Scene {
    changed: bool,
}

impl LockSession for Scene {
    fn output_ready(&mut self, _: OutputId, _: &OutputInfo) {}
    fn output_resized(&mut self, _: OutputId, _: &OutputInfo) {}
    fn force_copy_out(&mut self, _: OutputId) {}
    fn output_gone(&mut self, _: OutputId) {}
    fn focus_output(&mut self, _: OutputId) {}
    fn caps_lock(&mut self, _: bool) {}
    fn tick(&mut self) {}
    fn scene_needs_present(&mut self, _: OutputId) -> bool {
        self.changed
    }
    fn render(&mut self, _: OutputId, _: vigil_core::FrameTarget<'_>) -> bool {
        std::mem::take(&mut self.changed)
    }
    fn wait_decision(&self) -> WaitDecision {
        WaitDecision::Indefinite
    }
}

struct Harness {
    app: App<Scene>,
    queue: EventQueue<App<Scene>>,
    frames: Arc<Mutex<Frames>>,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn new() -> Self {
        let (client, server_end) = UnixStream::pair().unwrap();
        let frames = Arc::new(Mutex::new(Frames::default()));
        let mut backend = Backend::<()>::new().unwrap();
        let mut handle = backend.handle();
        handle.insert_client(server_end, Arc::new(())).unwrap();
        for (interface, version) in [
            (wl_compositor::WlCompositor::interface(), 6),
            (wl_shm::WlShm::interface(), 1),
            (wl_output::WlOutput::interface(), 4),
            (ZwlrLayerShellV1::interface(), 4),
            (ExtSessionLockManagerV1::interface(), 1),
        ] {
            handle.create_global::<()>(interface, version, Arc::new(Mute(frames.clone())));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let running = stop.clone();
        let server = std::thread::spawn(move || {
            while !running.load(Ordering::Relaxed) {
                let _ = backend.dispatch_all_clients(&mut ());
                let _ = backend.flush(None);
                std::thread::sleep(Duration::from_micros(200));
            }
        });

        let conn = Connection::from_socket(client).unwrap();
        let (globals, queue) = registry_queue_init(&conn).unwrap();
        let qh = queue.handle();
        let lock = vigil_config::Lock {
            transition: vigil_config::LockTransition::immediate(),
            ..Default::default()
        };
        let (app, _) = App::new(
            Scene::default(),
            &conn,
            &globals,
            &qh,
            Runtime {
                wake: slint_idle_runtime::WakeHandle::new(|| {}),
                dirty: Arc::default(),
                metrics: Arc::default(),
                root: tracing::Span::none(),
            },
            &lock,
            Some(0),
        )
        .unwrap();
        let mut harness = Self {
            app,
            queue,
            frames,
            stop,
            server: Some(server),
        };
        harness.sync();
        harness
    }

    fn output(&self) -> wl_output::WlOutput {
        self.app.outputs.outputs().next().unwrap()
    }

    fn warning(&mut self) -> usize {
        self.app.pre_lock_overlays = true;
        self.app.add_output(&self.app.qh.clone(), self.output());
        self.configure(self.app.entries.len() - 1)
    }

    fn lock_surface(&mut self) -> usize {
        self.app.pre_lock_overlays = false;
        if self.app.lock.is_none() {
            self.app.lock = Some(self.app.lock_state.lock(&self.app.qh).unwrap());
        }
        self.app.add_output(&self.app.qh.clone(), self.output());
        self.configure(self.app.entries.len() - 1)
    }

    fn reveal(&mut self) -> usize {
        self.app.add_reveal(&self.app.qh.clone(), self.output());
        self.configure(self.app.entries.len() - 1)
    }

    fn configure(&mut self, idx: usize) -> usize {
        let entry = &mut self.app.entries[idx];
        entry.logical = (8, 8);
        entry.px = (8, 8);
        entry.pool = Some(SlotPool::new(8 * 8 * 4 * 2, &self.app.shm).unwrap());
        entry.configured = true;
        idx
    }

    fn sync(&mut self) {
        self.queue.roundtrip(&mut self.app).unwrap();
    }

    fn commits(&mut self, idx: usize) -> usize {
        self.sync();
        let surface = self.app.entries[idx].surface.id().protocol_id();
        self.frames
            .lock()
            .unwrap()
            .committed
            .iter()
            .filter(|id| **id == surface)
            .count()
    }

    fn change(&mut self) {
        self.app.session.changed = true;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

const MS: Duration = Duration::from_millis(1);

#[test]
fn a_held_present_waits_for_its_frame_and_wakes_the_loop_then() {
    let mut h = Harness::new();
    let lock = h.lock_surface();
    let t0 = Instant::now();
    h.app.present_all(t0);
    assert_eq!(h.commits(lock), 1, "the first frame went out at once");

    h.change();
    h.app.present_all(t0 + 5 * MS);
    assert_eq!(
        h.commits(lock),
        1,
        "a change 5 ms after a commit was drawn at once"
    );
    assert_eq!(
        h.app.next_timeout(t0 + 5 * MS),
        Some(FRAME - 5 * MS),
        "the loop does not wake when the held frame is due"
    );

    h.app.present_all(t0 + FRAME);
    assert_eq!(h.commits(lock), 2, "the held change never went out");
    assert_eq!(
        h.app.next_timeout(t0 + FRAME),
        None,
        "a present that went out still wakes the loop"
    );
}

#[test]
fn a_held_lock_surface_keeps_its_frame_from_an_overlay_on_its_output() {
    let mut h = Harness::new();
    let warning = h.warning();
    let t0 = Instant::now();
    h.change();
    h.app.present_all(t0 - Duration::from_secs(1));
    let lock = h.lock_surface();
    h.app.present_all(t0);
    assert_eq!(h.app.entries[warning].id, h.app.entries[lock].id);
    assert_eq!(
        (h.commits(lock), h.commits(warning)),
        (1, 1),
        "the lock surface's first frame went out"
    );

    h.change();
    h.app.present_all(t0 + 5 * MS);
    h.app.present_all(t0 + FRAME);
    assert_eq!(
        h.commits(warning),
        1,
        "the warning overlay drew the change the held lock surface was waiting for"
    );
    assert_eq!(
        h.commits(lock),
        2,
        "the lock surface never showed the change"
    );
}

#[test]
fn a_reveal_is_not_held_back_by_a_lock_surface_that_stopped_presenting() {
    let mut h = Harness::new();
    let lock = h.lock_surface();
    let t0 = Instant::now();
    h.app.present_all(t0);
    h.change();
    h.app.present_all(t0 + 5 * MS);
    assert!(h.app.entries[lock].pacer.due().is_some());

    h.app.unlock_sent = true;
    let reveal = h.reveal();
    h.change();
    h.app.present_all(t0 + 6 * MS);
    assert_eq!(
        h.commits(reveal),
        1,
        "the reveal's first frame waited on the unlocked lock surface"
    );
    h.change();
    h.app.present_all(t0 + Duration::from_secs(1));
    assert_eq!(
        h.commits(reveal),
        2,
        "the reveal stopped drawing after unlock"
    );
}

#[test]
fn a_failed_forced_present_leaves_no_stale_deadline() {
    let mut h = Harness::new();
    let lock = h.lock_surface();
    let t0 = Instant::now();
    h.app.present_all(t0);
    h.change();
    h.app.present_all(t0 + 5 * MS);

    h.app.entries[lock].committed = false;
    h.app.entries[lock].pool = None;
    h.app.present_all(t0 + 6 * MS);
    assert_eq!(
        h.app.next_timeout(t0 + 6 * MS),
        Some(100 * MS),
        "a forced present that failed still waits on the frame it was never held for"
    );
    let later = t0 + Duration::from_secs(1);
    h.app.present_all(later);
    assert_eq!(
        h.app.next_timeout(later),
        Some(200 * MS),
        "the loop waits on a frame deadline that passed instead of the retry"
    );
}

#[test]
fn an_overlay_present_keeps_the_lock_surface_retry() {
    let mut h = Harness::new();
    let warning = h.warning();
    let t0 = Instant::now();
    h.change();
    h.app.present_all(t0 - Duration::from_secs(1));
    let lock = h.lock_surface();
    h.app.entries[lock].pool = None;

    h.change();
    h.app.present_all(t0);
    assert_eq!(h.commits(warning), 2);
    assert_eq!(
        h.app.next_timeout(t0),
        Some(Duration::from_millis(100)),
        "the warning's present dropped the lock surface's retry"
    );
}

#[test]
fn the_soonest_live_retry_wakes_the_loop() {
    let mut h = Harness::new();
    let warning = h.warning();
    let lock = h.lock_surface();
    let now = Instant::now();
    h.app.entries[warning].retry = Some((now + 800 * MS, 0));
    h.app.entries[lock].retry = Some((now + 200 * MS, 0));
    assert_eq!(h.app.next_timeout(now), Some(200 * MS));
    h.app.unlock_sent = true;
    assert_eq!(
        h.app.next_timeout(now),
        Some(800 * MS),
        "a lock surface that stopped presenting still steers the loop"
    );
}

#[test]
fn a_retry_for_a_departed_output_cannot_steer_the_loop() {
    let mut h = Harness::new();
    let lock = h.lock_surface();
    let now = Instant::now();
    h.app.entries[lock].retry = Some((now - Duration::from_secs(60), 3));
    assert_eq!(
        h.app.next_timeout(now),
        Some(Duration::ZERO),
        "a live output past its deadline is due now"
    );
    let (conn, qh, output) = (h.app.conn.clone(), h.app.qh.clone(), h.output());
    smithay_client_toolkit::output::OutputHandler::output_destroyed(&mut h.app, &conn, &qh, output);
    assert_eq!(
        h.app.next_timeout(now),
        None,
        "a retry with no live output must not be considered"
    );
}
