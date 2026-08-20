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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use calloop_wayland_source::WaylandSource;
use hypr_slint_runtime::{DirtySet, Metrics, WaitDecision, WakeHandle};
use smithay_client_toolkit::{
    background_effect::{BackgroundEffectHandler, BackgroundEffectState},
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::calloop::EventLoop,
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
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm,
        wl_surface,
    },
};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1, ext_background_effect_surface_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use vigil_config::LockWarning;
use vigil_core::{FrameTarget, InputEvent, OutputId, OutputInfo};
use vigil_warning::ElementSample;
use vigil_warning::{Phase as WarningPhase, Timeline};

/// What the binary implements: its composition of theme windows and auth.
pub trait LockSession {
    /// Install a thread-safe wakeup for asynchronous work. Calling it makes
    /// the Wayland loop run a tick immediately instead of waiting for the
    /// next frame deadline.
    fn set_runtime(
        &mut self,
        _wake: WakeHandle,
        _dirty: Arc<DirtySet<OutputId>>,
        _metrics: Arc<Metrics>,
    ) {
    }
    /// An output has its first configured size; create its scene.
    fn output_ready(&mut self, id: OutputId, info: &OutputInfo);
    /// The output's pixel size or scale changed.
    fn output_resized(&mut self, id: OutputId, info: &OutputInfo);
    /// The same output scene is being rebound from warning layer-shell to
    /// session-lock at unchanged geometry. Preserve decoded assets and only
    /// force a present.
    fn output_rebound(&mut self, id: OutputId, info: &OutputInfo) {
        self.output_resized(id, info);
    }
    fn output_gone(&mut self, id: OutputId);
    /// A pre-lock warning surface is ready. The default creates the same scene
    /// as a lock surface; implementations can keep authentication controls
    /// hidden until `locked`.
    fn warning_output_ready(&mut self, id: OutputId, info: &OutputInfo) {
        self.output_ready(id, info);
    }
    fn warning_progress(&mut self, _frost: f32, _wallpaper: f32) {}
    fn warning_elements(&mut self, _elements: &[ElementSample]) {}
    /// Consume an out-of-band request (second locker, logind, sleep) to skip
    /// the remaining warning and acquire session-lock now.
    fn warning_commit_requested(&mut self) -> bool {
        false
    }
    fn warning_wallpaper_ready(&self) -> bool {
        true
    }
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
    fn wait_decision(&self) -> WaitDecision;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    /// Auth succeeded; the unlock was round-tripped to the compositor.
    Unlocked,
    /// The compositor refused the lock (another locker running?).
    Denied,
    /// The compositor invalidated the lock after it was held.
    Invalidated,
    /// Input or output topology cancelled the warning before session-lock.
    Cancelled,
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

enum SurfaceRole {
    Warning(LayerSurface),
    Lock { _surface: SessionLockSurface },
}

struct Entry {
    id: OutputId,
    output: wl_output::WlOutput,
    surface: wl_surface::WlSurface,
    role: SurfaceRole,
    background_effect: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// Scale in 120ths (wp_fractional_scale units); 120 = 1.0.
    scale120: u32,
    logical: (u32, u32),
    px: (u32, u32),
    pool: Option<SlotPool>,
    configured: bool,
    /// A configured lock surface has committed a buffer of `px` at least
    /// once. Reset on size change so the first present after a rebuild
    /// cannot skip the commit (ext-session-lock blanks that output).
    committed: bool,
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
    layer_shell: Option<LayerShell>,
    background_effects: BackgroundEffectState,
    compositor_proxy: wl_compositor::WlCompositor,
    lock: Option<SessionLock>,
    viewporter: Option<WpViewporter>,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    entries: Vec<Entry>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    got_locked: bool,
    outcome: Option<LockOutcome>,
    error: Option<LockError>,
    wake: WakeHandle,
    dirty: Arc<DirtySet<OutputId>>,
    metrics: Arc<Metrics>,
    qh: QueueHandle<App<S>>,
    present_retry: BTreeMap<OutputId, (std::time::Instant, u32)>,
    warning: Option<Timeline>,
    warning_started: std::time::Instant,
    warning_progress: (f32, f32),
    warning_elements: Vec<ElementSample>,
    warning_frost_alpha: f32,
    warning_wait: Option<Duration>,
    scene_ids: BTreeSet<OutputId>,
    initial_outputs_added: bool,
}

impl<S: LockSession> App<S> {
    fn deliver_input(&mut self, event: InputEvent) {
        if self.lock.is_none()
            && let Some(warning) = self.warning.as_mut()
        {
            warning.input(&event);
        } else {
            self.session.input(event);
        }
    }
    fn schedule_present_retry(&mut self, id: OutputId) {
        let attempt = self
            .present_retry
            .get(&id)
            .map_or(0, |(_, attempt)| attempt.saturating_add(1));
        let delay = Duration::from_millis(100 * (1_u64 << attempt.min(5)));
        self.present_retry
            .insert(id, (std::time::Instant::now() + delay, attempt));
    }

    fn output_info(&self, idx: usize) -> OutputInfo {
        let entry = &self.entries[idx];
        let wayland_info = self.outputs.info(&entry.output);
        let connector = wayland_info
            .as_ref()
            .and_then(|info| info.name.clone())
            .unwrap_or_else(|| format!("output-{}", entry.id.0));
        OutputInfo {
            connector,
            width: entry.px.0,
            height: entry.px.1,
            refresh_mhz: 0,
            // Preserve the compositor's stable description so monitor and
            // prepared-appearance profiles can match across connector moves.
            make: None,
            model: wayland_info.and_then(|info| info.description),
            scale: entry.scale120 as f32 / 120.0,
        }
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // The initial snapshot and later metadata callback can name the same
        // object. Object identity is authoritative even before metadata exists.
        let adding_warning = self.warning.is_some() && self.lock.is_none();
        if self.entries.iter().any(|entry| {
            entry.output == output
                && matches!(entry.role, SurfaceRole::Warning(_)) == adding_warning
        }) {
            return;
        }
        let id = OutputId(output.id().protocol_id());
        let surface = self.compositor.create_surface(qh);
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, qh, ()));
        let fractional = self
            .fractional_mgr
            .as_ref()
            .map(|m| m.get_fractional_scale(&surface, qh, id));
        let (role, background_effect) = if self.warning.is_some() {
            let Some(layer_shell) = self.layer_shell.as_ref() else {
                return;
            };
            let layer = layer_shell.create_layer_surface(
                qh,
                surface.clone(),
                Layer::Overlay,
                Some("vigil-warning"),
                Some(&output),
            );
            layer.set_anchor(Anchor::TOP | Anchor::RIGHT | Anchor::BOTTOM | Anchor::LEFT);
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            let effect = self
                .background_effects
                .get_background_effect(&surface, qh)
                .ok();
            if let Some(effect) = &effect {
                let region = self.compositor_proxy.create_region(qh, ());
                region.add(0, 0, i32::MAX, i32::MAX);
                effect.set_blur_region(Some(&region));
                region.destroy();
            }
            layer.commit();
            (SurfaceRole::Warning(layer), effect)
        } else {
            let Some(lock) = self.lock.as_ref() else {
                return;
            };
            (
                SurfaceRole::Lock {
                    _surface: lock.create_lock_surface(surface.clone(), &output, qh),
                },
                None,
            )
        };
        self.entries.push(Entry {
            id,
            output,
            surface,
            role,
            background_effect,
            viewport,
            fractional,
            scale120: 120,
            logical: (0, 0),
            px: (0, 0),
            pool: None,
            configured: false,
            committed: false,
        });
    }

    fn apply_geometry(&mut self, idx: usize) {
        let configure_started = std::time::Instant::now();
        let (lw, lh) = self.entries[idx].logical;
        if lw == 0 || lh == 0 {
            return;
        }
        let scale120 = self.entries[idx].scale120;
        let px = (
            (lw * scale120).div_ceil(120).max(1),
            (lh * scale120).div_ceil(120).max(1),
        );
        let first = !self.entries[idx].configured;
        let resized = self.entries[idx].configured && self.entries[idx].px != px;
        // ext-session-lock blanks an output until a buffer matching this
        // configure is attached — including same-size configures after
        // DPMS/VT. Always force the next present; only rebuild the scene
        // when the pixel size actually changed.
        self.entries[idx].committed = false;
        self.entries[idx].px = px;
        if let Some(viewport) = &self.entries[idx].viewport {
            viewport.set_destination(lw as i32, lh as i32);
        }
        let len = px.0 as usize * px.1 as usize * 4;
        match self.entries[idx].pool.as_mut() {
            Some(pool) => {
                if pool.resize(len * 2).is_err() {
                    self.entries[idx].pool = SlotPool::new(len * 2, &self.shm).ok();
                }
            }
            None => self.entries[idx].pool = SlotPool::new(len * 2, &self.shm).ok(),
        }
        self.entries[idx].configured = true;
        let id = self.entries[idx].id;
        let info = self.output_info(idx);
        if first {
            if matches!(self.entries[idx].role, SurfaceRole::Warning(_)) {
                if self.scene_ids.insert(id) {
                    self.session.warning_output_ready(id, &info);
                } else {
                    self.session.output_resized(id, &info);
                }
            } else if self.scene_ids.insert(id) {
                self.session.output_ready(id, &info);
            } else {
                self.session.output_rebound(id, &info);
            }
        } else if resized {
            self.session.output_resized(id, &info);
        }
        // Invariant: a configured lock surface gets a buffer in this event-loop
        // iteration. Presentation stays outside protocol callbacks so redraws
        // coalesce and buffer acquisition has one audited entry point.
        self.dirty.mark(id);
        self.wake.wake();
        let elapsed = configure_started.elapsed();
        if elapsed >= Duration::from_millis(8) {
            eprintln!("vigil-lock: output {:?} configure: {:?}", id, elapsed);
        }
    }

    /// Render if the scene is dirty and commit the new buffer.
    ///
    /// After a configure, the compositor blanks the output until a buffer is
    /// attached. Skipping that commit (dirty-flag miss, shm alloc fail,
    /// missing scene, same-size DPMS/VT configure) is the metal black screen.
    fn present(&mut self, idx: usize) {
        if !self.entries[idx].configured {
            return;
        }
        let (w, h) = self.entries[idx].px;
        let force = !self.entries[idx].committed;
        let id = self.entries[idx].id;
        let warning = matches!(self.entries[idx].role, SurfaceRole::Warning(_));
        let Some(pool) = self.entries[idx].pool.as_mut() else {
            eprintln!("vigil-lock: output {id:?}: no shm pool ({w}x{h})");
            self.schedule_present_retry(id);
            return;
        };
        self.metrics.record_buffer_acquire();
        let stride = w as usize * 4;
        let format = if warning {
            wl_shm::Format::Argb8888
        } else {
            wl_shm::Format::Xrgb8888
        };
        let Ok((buffer, canvas)) = pool.create_buffer(w as i32, h as i32, stride as i32, format)
        else {
            eprintln!("vigil-lock: output {id:?}: shm buffer {w}x{h} failed");
            self.schedule_present_retry(id);
            return;
        };
        if force {
            canvas.fill(0);
        }
        let render_started = std::time::Instant::now();
        let drew = self.session.render(
            id,
            FrameTarget {
                buffer: canvas,
                width: w,
                height: h,
                stride,
            },
        );
        if warning && self.warning.is_some() {
            // wl_shm ARGB8888 is premultiplied. Fade the rendered lock
            // wallpaper in over a neutral frost tint while the compositor
            // supplies the live blur behind this translucent surface.
            let wallpaper = self.warning_progress.1.clamp(0.0, 1.0);
            let tint_alpha =
                (self.warning_progress.0 * self.warning_frost_alpha * (1.0 - wallpaper))
                    .clamp(0.0, 1.0);
            let alpha = wallpaper + tint_alpha;
            for pixel in canvas.as_chunks_mut::<4>().0 {
                for channel in &mut pixel[..3] {
                    let wallpaper_channel = f32::from(*channel) * wallpaper;
                    let tint_channel = 18.0 * tint_alpha;
                    *channel = (wallpaper_channel + tint_channel).round() as u8;
                }
                pixel[3] = (alpha * 255.0).round() as u8;
            }
        }
        let render_elapsed = render_started.elapsed();
        if !drew && !force {
            self.present_retry.remove(&id);
            return;
        }
        if drew {
            self.metrics.record_render();
        }
        if !drew {
            eprintln!("vigil-lock: output {id:?}: first present empty; committing black {w}x{h}");
        }
        let surface = &self.entries[idx].surface;
        let commit_started = std::time::Instant::now();
        let _ = buffer.attach_to(surface);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        self.metrics.record_commit();
        self.entries[idx].committed = true;
        self.present_retry.remove(&id);
        let commit_elapsed = commit_started.elapsed();
        if render_elapsed >= Duration::from_millis(8) || commit_elapsed >= Duration::from_millis(8)
        {
            eprintln!(
                "vigil-lock: output {id:?} present: render {:?}, commit {:?}",
                render_elapsed, commit_elapsed
            );
        }
    }

    fn entry_idx_by_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.entries.iter().position(|e| &e.surface == surface)
    }

    fn tick(&mut self) {
        let now = std::time::Instant::now();
        for (&id, &(deadline, _)) in &self.present_retry {
            if deadline <= now {
                self.dirty.mark(id);
            }
        }
        if let Some(timeline) = self.warning.as_mut() {
            let elapsed = self.warning_started.elapsed();
            if self.lock.is_none() {
                timeline.set_wallpaper_ready(self.session.warning_wallpaper_ready(), elapsed);
                if self.session.warning_commit_requested() {
                    timeline.request_commit();
                }
            }
            let sample = timeline.sample(elapsed);
            self.warning_wait = match (sample.next_frame, timeline.next_gui_wake(elapsed)) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (left, right) => left.or(right),
            };
            let progress = (sample.frost, sample.wallpaper);
            let elements = timeline.element_samples(elapsed);
            if self.warning_progress != progress {
                self.warning_progress = progress;
                self.session
                    .warning_progress(sample.frost, sample.wallpaper);
                for entry in &self.entries {
                    self.dirty.mark(entry.id);
                }
            }
            if self.warning_elements != elements {
                self.session.warning_elements(&elements);
                self.warning_elements = elements;
            }
            match sample.phase {
                WarningPhase::Cancelled => {
                    self.outcome = Some(LockOutcome::Cancelled);
                    return;
                }
                _ if sample.should_commit && self.lock.is_none() => {
                    self.begin_lock();
                    return;
                }
                _ => {}
            }
            if self.got_locked && timeline.gui_complete(elapsed) {
                self.warning = None;
            }
        }
        self.session.tick();
        if self.session.wants_unlock() {
            self.finish_unlock();
            return;
        }
        for id in self.dirty.take_all() {
            let indices: Vec<_> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(idx, entry)| (entry.id == id).then_some(idx))
                .collect();
            for idx in indices {
                self.present(idx);
            }
        }
        self.cleanup_warning_surfaces();
    }

    fn begin_lock(&mut self) {
        match self.lock_state.lock(&self.qh) {
            Ok(lock) => self.lock = Some(lock),
            Err(error) => {
                self.error = Some(err(format!("ext-session-lock-v1 unavailable: {error}")));
                return;
            }
        }
        let outputs: Vec<_> = self.outputs.outputs().collect();
        for output in outputs {
            self.add_output(&self.qh.clone(), output);
        }
    }

    fn cleanup_warning_surfaces(&mut self) {
        if !self.got_locked
            || self
                .entries
                .iter()
                .filter(|entry| matches!(entry.role, SurfaceRole::Lock { .. }))
                .any(|entry| !entry.committed)
        {
            return;
        }
        self.entries.retain_mut(|entry| {
            if matches!(entry.role, SurfaceRole::Warning(_)) {
                if let Some(effect) = entry.background_effect.take() {
                    effect.destroy();
                }
                false
            } else {
                true
            }
        });
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

/// Lock the session immediately and run until unlocked/denied/invalidated.
pub fn run<S: LockSession + 'static>(session: S) -> Result<LockOutcome, LockError> {
    run_with_warning(session, LockWarning::default())
}

/// Present an optional capture-free warning before acquiring session-lock.
pub fn run_with_warning<S: LockSession + 'static>(
    session: S,
    warning_config: LockWarning,
) -> Result<LockOutcome, LockError> {
    let conn = Connection::connect_to_env().map_err(err)?;
    let (globals, event_queue) = registry_queue_init(&conn).map_err(err)?;
    let qh: QueueHandle<App<S>> = event_queue.handle();
    let mut event_loop: EventLoop<App<S>> = EventLoop::try_new().map_err(err)?;
    let signal = event_loop.get_signal();
    let wake_signal = signal.clone();
    let wake = WakeHandle::new(move || wake_signal.wakeup());
    let dirty = Arc::new(DirtySet::new());
    let metrics = Arc::new(Metrics::new());

    let warning_enabled = warning_config.duration_ms > 0;
    let warning_frost_alpha = warning_config.frost_alpha.clamp(0.0, 1.0);
    let mut warning = warning_enabled.then(|| Timeline::new(warning_config));
    if let Some(timeline) = warning.as_mut() {
        timeline.start(Duration::ZERO);
    }
    let compositor_proxy: wl_compositor::WlCompositor =
        globals.bind(&qh, 1..=6, ()).map_err(err)?;
    let mut app = App {
        session,
        conn: conn.clone(),
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).map_err(err)?,
        seats: SeatState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(err)?,
        lock_state: SessionLockState::new(&globals, &qh),
        layer_shell: warning_enabled
            .then(|| LayerShell::bind(&globals, &qh))
            .transpose()
            .map_err(err)?,
        background_effects: BackgroundEffectState::new(&globals, &qh),
        compositor_proxy,
        lock: None,
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        fractional_mgr: globals.bind(&qh, 1..=1, ()).ok(),
        entries: Vec::new(),
        keyboard: None,
        pointer: None,
        got_locked: false,
        outcome: None,
        error: None,
        wake: wake.clone(),
        dirty: dirty.clone(),
        metrics: metrics.clone(),
        qh: qh.clone(),
        present_retry: BTreeMap::new(),
        warning,
        warning_started: std::time::Instant::now(),
        warning_progress: (0.0, 0.0),
        warning_elements: Vec::new(),
        warning_frost_alpha,
        warning_wait: None,
        scene_ids: BTreeSet::new(),
        initial_outputs_added: false,
    };

    if !warning_enabled {
        app.lock = Some(
            app.lock_state
                .lock(&qh)
                .map_err(|e| LockError(format!("ext-session-lock-v1 unavailable: {e}")))?,
        );
    }

    // Outputs known now; later arrivals come via OutputHandler::new_output.
    let outputs: Vec<_> = app.outputs.outputs().collect();
    for output in outputs {
        app.add_output(&qh, output);
    }
    app.initial_outputs_added = true;

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(err)?;
    app.session.set_runtime(wake, dirty, metrics);

    while app.outcome.is_none() && app.error.is_none() {
        let mut timeout = match app.session.wait_decision() {
            WaitDecision::Frame(delay) | WaitDecision::Timer(delay) => Some(delay),
            WaitDecision::Indefinite => None,
        };
        if app.warning.is_some()
            && let Some(warning) = app.warning_wait
        {
            timeout = Some(timeout.map_or(warning, |current| current.min(warning)));
        }
        if let Some(retry) = app
            .present_retry
            .values()
            .map(|(deadline, _)| deadline.saturating_duration_since(std::time::Instant::now()))
            .min()
        {
            timeout = Some(timeout.map_or(retry, |current| current.min(retry)));
        }
        event_loop.dispatch(timeout, &mut app).map_err(err)?;
        app.wake.acknowledge();
        app.metrics.record_wake();
        // A frame deadline, Wayland input, or an asynchronous worker wakeup
        // all converge here. No expensive client work belongs in callbacks.
        app.tick();
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
        if let Some(warning) = self.warning.as_mut() {
            warning.locked(self.warning_started.elapsed());
        }
        self.session.locked();
        for entry in &self.entries {
            if matches!(entry.role, SurfaceRole::Lock { .. }) {
                self.dirty.mark(entry.id);
            }
        }
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

impl<S: LockSession> LayerShellHandler for App<S> {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(entry) = self.entries.iter().find(
            |entry| matches!(&entry.role, SurfaceRole::Warning(candidate) if candidate == layer),
        ) {
            eprintln!("vigil-lock: warning surface {:?} closed", entry.id);
        }
        self.outcome = Some(LockOutcome::Cancelled);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(idx) = self.entries.iter().position(
            |entry| matches!(&entry.role, SurfaceRole::Warning(candidate) if candidate == layer),
        ) else {
            return;
        };
        self.entries[idx].logical = configure.new_size;
        self.apply_geometry(idx);
    }
}

impl<S: LockSession> BackgroundEffectHandler for App<S> {
    fn background_effect_state(&mut self) -> &mut BackgroundEffectState {
        &mut self.background_effects
    }

    fn update_capabilities(&mut self) {
        use ext_background_effect_manager_v1::Capability;
        let blur = self
            .background_effects
            .capabilities()
            .is_some_and(|capabilities| capabilities.contains(Capability::Blur));
        eprintln!(
            "vigil-lock: compositor background blur {}",
            if blur {
                "available"
            } else {
                "unavailable; tint fallback active"
            }
        );
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
        self.deliver_input(InputEvent::Key {
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
        self.deliver_input(InputEvent::Key {
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
        self.deliver_input(InputEvent::Key {
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
                PointerEventKind::Enter { .. } => {
                    self.session.focus_output(id);
                    if self.lock.is_none()
                        && let Some(warning) = self.warning.as_mut()
                    {
                        warning.pointer_enter(event.position.0 * scale, event.position.1 * scale);
                    }
                }
                PointerEventKind::Motion { .. } => {
                    self.deliver_input(InputEvent::PointerAbsolute {
                        x: event.position.0 * scale,
                        y: event.position.1 * scale,
                    });
                }
                PointerEventKind::Press { button, .. } => {
                    self.deliver_input(InputEvent::PointerButton {
                        button,
                        pressed: true,
                    });
                }
                PointerEventKind::Release { button, .. } => {
                    self.deliver_input(InputEvent::PointerButton {
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
            // MUST release, not just drop: the compositor keeps delivering
            // events to an unreleased wl_keyboard, so after a seat
            // capability bounce (observed across suspend/resume) the next
            // `new_capability` binds a second one and every keystroke
            // arrives twice — doubled password characters.
            Capability::Keyboard => {
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
            }
            Capability::Pointer => {
                if let Some(pointer) = self.pointer.take() {
                    pointer.release();
                }
            }
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
        let genuinely_new = !self.entries.iter().any(|entry| entry.output == output);
        if self.initial_outputs_added
            && self.lock.is_none()
            && genuinely_new
            && let Some(warning) = self.warning.as_mut()
        {
            warning.hotplug();
        }
        self.add_output(qh, output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.lock.is_none()
            && let Some(warning) = self.warning.as_mut()
        {
            warning.hotplug();
        }
        if let Some(id) = self
            .entries
            .iter()
            .find(|entry| entry.output == output)
            .map(|entry| entry.id)
        {
            eprintln!("vigil-lock: output {:?} gone", id);
            for entry in self.entries.iter().filter(|entry| entry.output == output) {
                if let Some(fractional) = &entry.fractional {
                    fractional.destroy();
                }
                if let Some(viewport) = &entry.viewport {
                    viewport.destroy();
                }
            }
            self.entries.retain(|entry| entry.output != output);
            self.scene_ids.remove(&id);
            self.session.output_gone(id);
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
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore wl_region::WlRegion);

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
