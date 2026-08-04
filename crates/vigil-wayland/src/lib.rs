//! Wayland session-lock stack for vigil-lock (DESIGN.md §12).
//!
//! Owns everything Wayland: the ext-session-lock-v1 lifecycle (surfaces per
//! output including hotplug, the configure/commit dance, unlock + roundtrip),
//! wl_shm buffer presentation into [`vigil_core::FrameTarget`]s, fractional
//! scale (wp_fractional_scale_v1 + wp_viewporter — every output on the
//! reference machine is fractionally scaled), and seat input translated to
//! [`vigil_core::InputEvent`]s.
//!
//! The binary supplies a [`LockSession`] (its composition of vigil-ui
//! windows + auth) and calls [`run`]; the event loop lives here because the
//! Wayland connection *is* the loop for a lock client.

use std::time::Duration;

use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::calloop::{
        EventLoop,
        timer::{TimeoutAction, Timer},
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use vigil_core::{FrameTarget, InputEvent, OutputId, OutputInfo};

/// What the binary implements: its composition of theme windows and auth.
pub trait LockSession {
    /// An output has its first configured size; create its scene.
    fn output_ready(&mut self, id: OutputId, info: &OutputInfo);
    /// The output's pixel size or scale changed.
    fn output_resized(&mut self, id: OutputId, info: &OutputInfo);
    fn output_gone(&mut self, id: OutputId);
    /// The pointer entered this output (panel-follows-pointer signal).
    fn focus_output(&mut self, id: OutputId);
    fn input(&mut self, event: InputEvent);
    fn caps_lock(&mut self, on: bool);
    /// The compositor confirmed the session is locked.
    fn locked(&mut self);
    /// ~16ms cadence: drain auth events, clock, Slint timers.
    fn tick(&mut self);
    /// Draw this output's scene if dirty; return whether pixels changed.
    fn render(&mut self, id: OutputId, target: FrameTarget<'_>) -> bool;
    /// Checked after every tick: true once auth succeeded.
    fn wants_unlock(&self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    /// Auth succeeded; the unlock was round-tripped to the compositor.
    Unlocked,
    /// The compositor refused the lock (another locker running?).
    Denied,
    /// The compositor invalidated the lock after it was held.
    Invalidated,
}

#[derive(Debug)]
pub struct LockError(pub String);

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lock: {}", self.0)
    }
}
impl std::error::Error for LockError {}

fn err(e: impl std::fmt::Display) -> LockError {
    LockError(e.to_string())
}

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

struct Entry {
    id: OutputId,
    output: wl_output::WlOutput,
    surface: wl_surface::WlSurface,
    _lock_surface: SessionLockSurface,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// Scale in 120ths (wp_fractional_scale units); 120 = 1.0.
    scale120: u32,
    logical: (u32, u32),
    px: (u32, u32),
    pool: Option<SlotPool>,
    configured: bool,
}

struct App<S: LockSession + 'static> {
    session: S,
    conn: Connection,
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    seats: SeatState,
    shm: Shm,
    lock_state: SessionLockState,
    lock: Option<SessionLock>,
    viewporter: Option<WpViewporter>,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    entries: Vec<Entry>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    got_locked: bool,
    outcome: Option<LockOutcome>,
    error: Option<LockError>,
}

impl<S: LockSession> App<S> {
    fn output_info(&self, idx: usize) -> OutputInfo {
        let entry = &self.entries[idx];
        let connector = self
            .outputs
            .info(&entry.output)
            .and_then(|i| i.name)
            .unwrap_or_else(|| format!("output-{}", entry.id.0));
        OutputInfo {
            connector,
            width: entry.px.0,
            height: entry.px.1,
            refresh_mhz: 0,
            scale: entry.scale120 as f32 / 120.0,
        }
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Some(lock) = self.lock.as_ref() else {
            return;
        };
        let id = OutputId(self.outputs.info(&output).map(|i| i.id).unwrap_or_default());
        if self.entries.iter().any(|e| e.id == id) {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, qh, ()));
        let fractional = self
            .fractional_mgr
            .as_ref()
            .map(|m| m.get_fractional_scale(&surface, qh, id));
        let lock_surface = lock.create_lock_surface(surface.clone(), &output, qh);
        self.entries.push(Entry {
            id,
            output,
            surface,
            _lock_surface: lock_surface,
            viewport,
            fractional,
            scale120: 120,
            logical: (0, 0),
            px: (0, 0),
            pool: None,
            configured: false,
        });
    }

    fn apply_geometry(&mut self, idx: usize) {
        let (lw, lh) = self.entries[idx].logical;
        if lw == 0 || lh == 0 {
            return;
        }
        let scale120 = self.entries[idx].scale120;
        let px = (
            (lw * scale120).div_ceil(120).max(1),
            (lh * scale120).div_ceil(120).max(1),
        );
        self.entries[idx].px = px;
        if let Some(viewport) = &self.entries[idx].viewport {
            viewport.set_destination(lw as i32, lh as i32);
        }
        let len = px.0 as usize * px.1 as usize * 4;
        match self.entries[idx].pool.as_mut() {
            Some(pool) => {
                let _ = pool.resize(len * 2);
            }
            None => self.entries[idx].pool = SlotPool::new(len * 2, &self.shm).ok(),
        }
        let first = !self.entries[idx].configured;
        self.entries[idx].configured = true;
        let id = self.entries[idx].id;
        let info = self.output_info(idx);
        if first {
            self.session.output_ready(id, &info);
        } else {
            self.session.output_resized(id, &info);
        }
        // Invariant: a configured lock surface gets a buffer promptly.
        self.present(idx);
    }

    /// Render if the scene is dirty and commit the new buffer.
    fn present(&mut self, idx: usize) {
        let entry = &mut self.entries[idx];
        if !entry.configured {
            return;
        }
        let (w, h) = entry.px;
        let Some(pool) = entry.pool.as_mut() else {
            return;
        };
        let stride = w as usize * 4;
        let Ok((buffer, canvas)) =
            pool.create_buffer(w as i32, h as i32, stride as i32, wl_shm::Format::Xrgb8888)
        else {
            return;
        };
        let id = entry.id;
        let drew = self.session.render(
            id,
            FrameTarget {
                buffer: canvas,
                width: w,
                height: h,
                stride,
            },
        );
        let entry = &self.entries[idx];
        if drew {
            let _ = buffer.attach_to(&entry.surface);
            entry.surface.damage_buffer(0, 0, w as i32, h as i32);
            entry.surface.commit();
        }
    }

    fn entry_idx_by_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.entries.iter().position(|e| &e.surface == surface)
    }

    fn tick(&mut self) {
        self.session.tick();
        if self.session.wants_unlock() {
            self.finish_unlock();
            return;
        }
        for idx in 0..self.entries.len() {
            self.present(idx);
        }
    }

    fn finish_unlock(&mut self) {
        if let Some(lock) = self.lock.take() {
            lock.unlock();
            // The unlock must reach the compositor before we exit, or the
            // session may stay locked forever.
            if let Err(e) = self.conn.roundtrip() {
                self.error = Some(err(format!("roundtrip after unlock: {e}")));
            }
        }
        self.outcome = Some(LockOutcome::Unlocked);
    }
}

/// Lock the session and run until unlocked/denied/invalidated.
pub fn run<S: LockSession + 'static>(session: S) -> Result<LockOutcome, LockError> {
    let conn = Connection::connect_to_env().map_err(err)?;
    let (globals, event_queue) = registry_queue_init(&conn).map_err(err)?;
    let qh: QueueHandle<App<S>> = event_queue.handle();
    let mut event_loop: EventLoop<App<S>> = EventLoop::try_new().map_err(err)?;

    let mut app = App {
        session,
        conn: conn.clone(),
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).map_err(err)?,
        seats: SeatState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(err)?,
        lock_state: SessionLockState::new(&globals, &qh),
        lock: None,
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        fractional_mgr: globals.bind(&qh, 1..=1, ()).ok(),
        entries: Vec::new(),
        keyboard: None,
        pointer: None,
        got_locked: false,
        outcome: None,
        error: None,
    };

    app.lock = Some(
        app.lock_state
            .lock(&qh)
            .map_err(|e| LockError(format!("ext-session-lock-v1 unavailable: {e}")))?,
    );

    // Outputs known now; later arrivals come via OutputHandler::new_output.
    let outputs: Vec<_> = app.outputs.outputs().collect();
    for output in outputs {
        app.add_output(&qh, output);
    }

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(err)?;
    event_loop
        .handle()
        .insert_source(
            Timer::from_duration(FRAME_INTERVAL),
            |_, _, app: &mut App<S>| {
                app.tick();
                TimeoutAction::ToDuration(FRAME_INTERVAL)
            },
        )
        .map_err(err)?;

    while app.outcome.is_none() && app.error.is_none() {
        event_loop
            .dispatch(Duration::from_millis(16), &mut app)
            .map_err(err)?;
    }
    match (app.error, app.outcome) {
        (Some(e), _) => Err(e),
        (None, Some(outcome)) => Ok(outcome),
        (None, None) => unreachable!(),
    }
}

impl<S: LockSession> SessionLockHandler for App<S> {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.got_locked = true;
        self.session.locked();
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.lock = None;
        self.outcome = Some(if self.got_locked {
            LockOutcome::Invalidated
        } else {
            LockOutcome::Denied
        });
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        lock_surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(idx) = self.entry_idx_by_surface(lock_surface.wl_surface()) else {
            return;
        };
        self.entries[idx].logical = configure.new_size;
        self.apply_geometry(idx);
    }
}

impl<S: LockSession> KeyboardHandler for App<S> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.session.input(InputEvent::Key {
            keysym: event.keysym.raw(),
            utf8: event.utf8,
            pressed: true,
        });
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.session.input(InputEvent::Key {
            keysym: event.keysym.raw(),
            utf8: event.utf8,
            pressed: true,
        });
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.session.input(InputEvent::Key {
            keysym: event.keysym.raw(),
            utf8: None,
            pressed: false,
        });
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _layout: u32,
    ) {
        self.session.caps_lock(modifiers.caps_lock);
    }
}

impl<S: LockSession> PointerHandler for App<S> {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(idx) = self.entry_idx_by_surface(&event.surface) else {
                continue;
            };
            let scale = self.entries[idx].scale120 as f64 / 120.0;
            let id = self.entries[idx].id;
            match event.kind {
                PointerEventKind::Enter { .. } => self.session.focus_output(id),
                PointerEventKind::Motion { .. } => {
                    self.session.input(InputEvent::PointerAbsolute {
                        x: event.position.0 * scale,
                        y: event.position.1 * scale,
                    });
                }
                PointerEventKind::Press { button, .. } => {
                    self.session.input(InputEvent::PointerButton {
                        button,
                        pressed: true,
                    });
                }
                PointerEventKind::Release { button, .. } => {
                    self.session.input(InputEvent::PointerButton {
                        button,
                        pressed: false,
                    });
                }
                PointerEventKind::Leave { .. } | PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

impl<S: LockSession> SeatHandler for App<S> {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard if self.keyboard.is_none() => {
                self.keyboard = self.seats.get_keyboard(qh, &seat, None).ok();
            }
            Capability::Pointer if self.pointer.is_none() => {
                self.pointer = self.seats.get_pointer(qh, &seat).ok();
            }
            _ => {}
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => self.keyboard = None,
            Capability::Pointer => self.pointer = None,
            _ => {}
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl<S: LockSession> CompositorHandler for App<S> {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // Integer fallback path: only honored when fractional scale is absent.
        if self.fractional_mgr.is_some() {
            return;
        }
        if let Some(idx) = self.entry_idx_by_surface(surface) {
            self.entries[idx].scale120 = (new_factor.max(1) as u32) * 120;
            self.entries[idx].surface.set_buffer_scale(new_factor);
            self.apply_geometry(idx);
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl<S: LockSession> OutputHandler for App<S> {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_output(qh, output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(idx) = self.entries.iter().position(|e| e.output == output) {
            let entry = self.entries.remove(idx);
            if let Some(fractional) = &entry.fractional {
                fractional.destroy();
            }
            if let Some(viewport) = &entry.viewport {
                viewport.destroy();
            }
            self.session.output_gone(entry.id);
        }
    }
}

// Raw protocol plumbing for fractional scale + viewporter (sctk has no
// helpers for these; the manual impls don't overlap the Dispatch2 blanket
// because our user-data types don't implement Dispatch2).

impl<S: LockSession> Dispatch<WpFractionalScaleV1, OutputId> for App<S> {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &OutputId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(idx) = state.entries.iter().position(|e| e.id == *id)
            && state.entries[idx].scale120 != scale
        {
            state.entries[idx].scale120 = scale;
            state.apply_geometry(idx);
        }
    }
}

wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore WpViewporter);
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore WpViewport);
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore wl_buffer::WlBuffer);

impl<S: LockSession> ProvidesRegistryState for App<S> {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

impl<S: LockSession> ShmHandler for App<S> {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

smithay_client_toolkit::delegate_registry!(@<S: LockSession + 'static> App<S>);
smithay_client_toolkit::delegate_dispatch2!(@<S: LockSession + 'static> App<S>);
