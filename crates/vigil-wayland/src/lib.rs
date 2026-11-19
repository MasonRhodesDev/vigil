use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use calloop_wayland_source::WaylandSource;
use slint_idle_runtime::{DirtySet, Metrics, WaitDecision, WakeHandle};
use smithay_client_toolkit::{
    background_effect::{BackgroundEffectHandler, BackgroundEffectState},
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, generic::Generic},
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
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
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
use wayland_protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1, wp_alpha_modifier_v1::WpAlphaModifierV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use vigil_config::Lock;
use vigil_core::{FrameTarget, InputEvent, OutputId, OutputInfo};
pub use vigil_flow::LockOutcome;
use vigil_flow::{FlowCmd, FlowEvent, LockFlow, Now, Signal};

mod hyprland_surface;
use hyprland_surface::hyprland_surface_manager_v1::HyprlandSurfaceManagerV1;
use hyprland_surface::hyprland_surface_v1::HyprlandSurfaceV1;

pub trait LockSession {
    fn set_runtime(
        &mut self,
        _wake: WakeHandle,
        _dirty: Arc<DirtySet<OutputId>>,
        _metrics: Arc<Metrics>,
    ) {
    }
    fn output_ready(&mut self, id: OutputId, info: &OutputInfo);
    fn output_resized(&mut self, id: OutputId, info: &OutputInfo);
    fn output_rebound(&mut self, id: OutputId, info: &OutputInfo) {
        self.output_resized(id, info);
    }
    fn force_repaint(&mut self, _id: OutputId) {}
    fn force_copy_out(&mut self, id: OutputId);
    fn output_gone(&mut self, id: OutputId);
    fn warning_output_ready(&mut self, id: OutputId, info: &OutputInfo) {
        self.output_ready(id, info);
    }
    fn overlay_progress(&mut self, _frost: f32, _wallpaper: f32) {}
    fn overlay_elements(&mut self, _elements: &[vigil_flow::ElementSample]) {}
    fn focus_output(&mut self, id: OutputId);
    fn caps_lock(&mut self, on: bool);
    fn keyboard_left(&mut self) {}
    fn poll_events(&mut self) -> Vec<FlowEvent> {
        Vec::new()
    }
    fn flow_command(&mut self, _cmd: &FlowCmd) {}
    fn tick(&mut self);
    fn render(&mut self, id: OutputId, target: FrameTarget<'_>) -> bool;
    fn scene_needs_present(&mut self, _id: OutputId) -> bool {
        true
    }
    fn wait_decision(&self) -> WaitDecision;
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

fn flow_event_kind(event: &FlowEvent) -> &'static str {
    match event {
        FlowEvent::Input(_) => "Input",
        FlowEvent::PointerEnter { .. } => "PointerEnter",
        FlowEvent::OutputAdded => "OutputAdded",
        FlowEvent::OutputGone => "OutputGone",
        FlowEvent::WallpaperReady(true) => "WallpaperReady:true",
        FlowEvent::WallpaperReady(false) => "WallpaperReady:false",
        FlowEvent::CommitRequested => "CommitRequested",
        FlowEvent::LockConfirmed => "LockConfirmed",
        FlowEvent::LockDenied => "LockDenied",
        FlowEvent::LockInvalidated => "LockInvalidated",
        FlowEvent::AuthOk => "AuthOk",
        FlowEvent::LogindUnlock => "LogindUnlock",
        FlowEvent::RevealOverlaysMapped => "RevealOverlaysMapped",
        FlowEvent::Terminate(Signal::Hangup) => "Terminate:SIGHUP",
        FlowEvent::Terminate(Signal::Interrupt) => "Terminate:SIGINT",
        FlowEvent::Terminate(Signal::Terminate) => "Terminate:SIGTERM",
        FlowEvent::Tick => "Tick",
        _ => "other",
    }
}

enum SurfaceRole {
    Warning(LayerSurface),
    Lock { _surface: SessionLockSurface },
    Reveal(LayerSurface),
}

impl SurfaceRole {
    fn is_lock(&self) -> bool {
        matches!(self, SurfaceRole::Lock { .. })
    }
    fn is_warning(&self) -> bool {
        matches!(self, SurfaceRole::Warning(_))
    }
    fn is_reveal(&self) -> bool {
        matches!(self, SurfaceRole::Reveal(_))
    }
    fn name(&self) -> &'static str {
        match self {
            SurfaceRole::Warning(_) => "warning",
            SurfaceRole::Lock { .. } => "lock",
            SurfaceRole::Reveal(_) => "reveal",
        }
    }
    fn layer(&self) -> Option<&LayerSurface> {
        match self {
            SurfaceRole::Warning(layer) | SurfaceRole::Reveal(layer) => Some(layer),
            SurfaceRole::Lock { .. } => None,
        }
    }
}

fn present_priority(is_lock: bool) -> u8 {
    u8::from(!is_lock)
}

enum SurfaceOpacity {
    Hyprland(HyprlandSurfaceV1),
    AlphaModifier(WpAlphaModifierSurfaceV1),
    None,
}

impl SurfaceOpacity {
    fn is_some(&self) -> bool {
        !matches!(self, SurfaceOpacity::None)
    }
    fn set(&self, value: f32) {
        let value = f64::from(value.clamp(0.0, 1.0));
        match self {
            SurfaceOpacity::Hyprland(surface) => surface.set_opacity(value),
            SurfaceOpacity::AlphaModifier(surface) => {
                surface.set_multiplier((value * f64::from(u32::MAX)).round() as u32);
            }
            SurfaceOpacity::None => {}
        }
    }
    fn destroy(&self) {
        match self {
            SurfaceOpacity::Hyprland(surface) => surface.destroy(),
            SurfaceOpacity::AlphaModifier(surface) => surface.destroy(),
            SurfaceOpacity::None => {}
        }
    }
}

fn ramp_commit_only(
    wallpaper: f32,
    prev_wallpaper: f32,
    scene_synced: bool,
    entries: impl Iterator<Item = (bool, bool, bool)>,
) -> bool {
    if wallpaper != 0.0 || prev_wallpaper != 0.0 || !scene_synced {
        return false;
    }
    let mut overlays = 0;
    for (is_lock, has_lever, committed) in entries {
        if is_lock {
            continue;
        }
        overlays += 1;
        if !has_lever || !committed {
            return false;
        }
    }
    overlays > 0
}

fn overlay_blend(
    canvas: &mut [u8],
    frost: f32,
    wallpaper: f32,
    frost_alpha: f32,
    surface_opacity: bool,
) {
    let wallpaper = wallpaper.clamp(0.0, 1.0);
    let tint_alpha =
        (pixel_frost(frost, surface_opacity) * frost_alpha * (1.0 - wallpaper)).clamp(0.0, 1.0);
    let alpha = wallpaper + tint_alpha;
    if wallpaper == 0.0 {
        let tint_channel = (18.0 * tint_alpha).round() as u8;
        let pixel = [
            tint_channel,
            tint_channel,
            tint_channel,
            (alpha * 255.0).round() as u8,
        ];
        let (chunks, _) = canvas.as_chunks_mut::<4>();
        for target in chunks.iter_mut() {
            *target = pixel;
        }
        return;
    }
    for pixel in canvas.as_chunks_mut::<4>().0 {
        for channel in &mut pixel[..3] {
            let wallpaper_channel = f32::from(*channel) * wallpaper;
            let tint_channel = 18.0 * tint_alpha;
            *channel = (wallpaper_channel + tint_channel).round() as u8;
        }
        pixel[3] = (alpha * 255.0).round() as u8;
    }
}

fn pixel_frost(frost: f32, surface_opacity_available: bool) -> f32 {
    if surface_opacity_available {
        1.0
    } else {
        frost
    }
}

fn surface_role_is_warning(warning_active: bool, lock_taken: bool) -> bool {
    warning_active && !lock_taken
}

fn frame_hash_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VIGIL_FRAME_HASH").is_some_and(|value| value == "1"))
}

fn frame_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn emit_committed_frame_hash(
    id: OutputId,
    role: &'static str,
    px: (u32, u32),
    stride: usize,
    pool: &mut SlotPool,
    buffer: &Buffer,
) {
    if !frame_hash_enabled() {
        return;
    }
    let raw = pool.raw_data_mut(&buffer.slot());
    let Some(bytes) = raw.get(..stride * px.1 as usize) else {
        return;
    };
    tracing::event!(
        name: "frame.hash",
        target: "vigil",
        tracing::Level::INFO,
        output = id.0,
        role = role,
        width = px.0,
        height = px.1,
        hash = frame_hash(bytes).as_str()
    );
}

struct Entry {
    id: OutputId,
    output: wl_output::WlOutput,
    surface: wl_surface::WlSurface,
    role: SurfaceRole,
    background_effect: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
    opacity: SurfaceOpacity,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    scale120: u32,
    logical: (u32, u32),
    px: (u32, u32),
    pool: Option<SlotPool>,
    configured: bool,
    committed: bool,
}

#[derive(Debug, Clone)]
struct FractionalTarget {
    surface: wl_surface::WlSurface,
}

fn initial_scale120(inherited: Option<u32>) -> u32 {
    inherited.unwrap_or(120).max(1)
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
    hyprland_surfaces: Option<HyprlandSurfaceManagerV1>,
    alpha_modifiers: Option<WpAlphaModifierV1>,
    compositor_proxy: wl_compositor::WlCompositor,
    lock: Option<SessionLock>,
    viewporter: Option<WpViewporter>,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    entries: Vec<Entry>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    held_keys: vigil_core::HeldKeys,
    pointer: Option<wl_pointer::WlPointer>,
    got_locked: bool,
    outcome: Option<LockOutcome>,
    error: Option<LockError>,
    wake: WakeHandle,
    dirty: Arc<DirtySet<OutputId>>,
    metrics: Arc<Metrics>,
    qh: QueueHandle<App<S>>,
    present_retry: BTreeMap<OutputId, (std::time::Instant, u32)>,
    pending_scenes: Vec<(OutputId, bool)>,
    flow: LockFlow,
    started: std::time::Instant,
    overlay_progress: (f32, f32),
    frost_alpha: f32,
    reveal_blur: bool,
    pre_lock_overlays: bool,
    scene_ids: BTreeSet<OutputId>,
    initial_outputs_added: bool,
    unlock_sent: bool,
    ramp_scene_synced: bool,
    root_span: tracing::Span,
    phase_span: Option<tracing::Span>,
    protocol_events: Vec<FlowEvent>,
}

// A key released after the keyboard left is never reported, so a held Ctrl
// would otherwise hold back every letter until it was pressed again. This is
// not input, so it never reaches the flow, where it could end a grace window.
fn keyboard_left<S: LockSession>(held: &mut vigil_core::HeldKeys, session: &mut S) {
    *held = vigil_core::HeldKeys::default();
    session.keyboard_left();
}

fn soonest_retry(
    retries: &BTreeMap<OutputId, (std::time::Instant, u32)>,
    live: impl Fn(OutputId) -> bool,
    now: std::time::Instant,
) -> Option<Duration> {
    retries
        .iter()
        .filter(|(id, _)| live(**id))
        .map(|(_, (deadline, _))| deadline.saturating_duration_since(now))
        .min()
}

impl<S: LockSession> App<S> {
    fn deliver_input(&mut self, event: InputEvent) {
        self.raise(FlowEvent::Input(event));
    }

    fn raise(&mut self, event: FlowEvent) {
        self.protocol_events.push(event);
        self.wake.wake();
    }

    fn now(&self) -> Now {
        Now {
            elapsed: self.started.elapsed(),
            mono: std::time::Instant::now(),
            wall: std::time::SystemTime::now(),
        }
    }

    fn run(&mut self, cmds: Vec<FlowCmd>) {
        for cmd in cmds {
            match cmd {
                FlowCmd::RequestSessionLock => self.begin_lock(),
                FlowCmd::OverlayProgress { frost, wallpaper } => {
                    let prev_wallpaper = self.overlay_progress.1;
                    self.overlay_progress = (frost, wallpaper);
                    if ramp_commit_only(
                        wallpaper,
                        prev_wallpaper,
                        self.ramp_scene_synced,
                        self.entries.iter().map(|entry| {
                            (
                                entry.role.is_lock(),
                                entry.opacity.is_some(),
                                entry.committed,
                            )
                        }),
                    ) {
                        for entry in &self.entries {
                            if !entry.role.is_lock() {
                                entry.opacity.set(frost);
                                entry.surface.commit();
                                self.metrics.record_commit();
                                tracing::event!(
                                    name: "ramp.commit",
                                    target: "vigil",
                                    tracing::Level::DEBUG,
                                    output = entry.id.0,
                                    frost = frost
                                );
                            }
                        }
                    } else {
                        self.session.overlay_progress(frost, wallpaper);
                        self.ramp_scene_synced = true;
                        for entry in &self.entries {
                            self.dirty.mark(entry.id);
                        }
                    }
                }
                FlowCmd::OverlayElements(ref elements) => {
                    self.session.overlay_elements(elements);
                }
                FlowCmd::CreateRevealOverlays => {
                    let outputs: Vec<_> = self
                        .entries
                        .iter()
                        .filter(|entry| entry.role.is_lock())
                        .map(|entry| entry.output.clone())
                        .collect();
                    let qh = self.qh.clone();
                    for output in outputs {
                        self.add_reveal(&qh, output);
                    }
                }
                FlowCmd::ReleaseSessionLock => self.send_unlock(),
                FlowCmd::DestroyRevealOverlays => self.drop_overlays(SurfaceRole::is_reveal),
                FlowCmd::Journal(note) => eprintln!("vigil-lock: {note}"),
                FlowCmd::PhaseChanged { from, to } => {
                    let phase = self.phase_span.as_ref().and_then(tracing::Span::id);
                    tracing::event!(
                        name: "flow.transition",
                        target: "vigil",
                        parent: phase,
                        tracing::Level::INFO,
                        from = %from,
                        to = %to
                    );
                    self.phase_span = Some(tracing::info_span!(
                        target: "vigil",
                        parent: self.root_span.id(),
                        "flow.phase",
                        phase = %to
                    ));
                }
                FlowCmd::Exit(outcome) => self.outcome = Some(outcome),
                ref session_cmd @ (FlowCmd::ShowPanel(_)
                | FlowCmd::DispatchInput(_)
                | FlowCmd::StartAuth
                | FlowCmd::ShowAuthError(_)
                | FlowCmd::ShowAccountLocked { .. }
                | FlowCmd::DetachAuth
                | FlowCmd::SetLockedHint(_)
                | FlowCmd::SignalReady) => self.session.flow_command(session_cmd),
            }
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
            make: None,
            model: wayland_info.and_then(|info| info.description),
            scale: entry.scale120 as f32 / 120.0,
        }
    }

    fn create_overlay(
        &self,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
        namespace: &str,
        pass_through: bool,
        blur: bool,
    ) -> Option<(
        LayerSurface,
        Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
        SurfaceOpacity,
    )> {
        let layer_shell = self.layer_shell.as_ref()?;
        let layer = layer_shell.create_layer_surface(
            qh,
            surface.clone(),
            Layer::Overlay,
            Some(namespace),
            Some(output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::RIGHT | Anchor::BOTTOM | Anchor::LEFT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(if pass_through {
            KeyboardInteractivity::None
        } else {
            KeyboardInteractivity::Exclusive
        });
        if pass_through {
            let region = self.compositor_proxy.create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        let effect = blur
            .then(|| {
                self.background_effects
                    .get_background_effect(surface, qh)
                    .ok()
            })
            .flatten();
        if let Some(effect) = &effect {
            let region = self.compositor_proxy.create_region(qh, ());
            region.add(0, 0, i32::MAX, i32::MAX);
            effect.set_blur_region(Some(&region));
            region.destroy();
        }
        let opacity = if let Some(manager) = &self.hyprland_surfaces {
            SurfaceOpacity::Hyprland(manager.get_hyprland_surface(surface, qh, ()))
        } else if let Some(manager) = &self.alpha_modifiers {
            SurfaceOpacity::AlphaModifier(manager.get_surface(surface, qh, ()))
        } else {
            SurfaceOpacity::None
        };
        layer.commit();
        Some((layer, effect, opacity))
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.unlock_sent {
            return;
        }
        let adding_warning = surface_role_is_warning(self.pre_lock_overlays, self.lock.is_some());
        if self
            .entries
            .iter()
            .any(|entry| entry.output == output && entry.role.is_warning() == adding_warning)
        {
            return;
        }
        let id = OutputId(output.id().protocol_id());
        let inherited_scale120 = self
            .entries
            .iter()
            .find(|entry| entry.output == output)
            .map(|entry| entry.scale120);
        let surface = self.compositor.create_surface(qh);
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, qh, ()));
        let fractional = self.fractional_mgr.as_ref().map(|m| {
            m.get_fractional_scale(
                &surface,
                qh,
                FractionalTarget {
                    surface: surface.clone(),
                },
            )
        });
        let (role, background_effect, opacity) = if adding_warning {
            let Some((layer, effect, opacity)) =
                self.create_overlay(qh, &surface, &output, "vigil-warning", false, true)
            else {
                return;
            };
            (SurfaceRole::Warning(layer), effect, opacity)
        } else {
            let Some(lock) = self.lock.as_ref() else {
                return;
            };
            (
                SurfaceRole::Lock {
                    _surface: lock.create_lock_surface(surface.clone(), &output, qh),
                },
                None,
                SurfaceOpacity::None,
            )
        };
        self.entries.push(Entry {
            id,
            output,
            surface,
            role,
            background_effect,
            opacity,
            viewport,
            fractional,
            scale120: initial_scale120(inherited_scale120),
            logical: (0, 0),
            px: (0, 0),
            pool: None,
            configured: false,
            committed: false,
        });
    }

    fn add_reveal(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self
            .entries
            .iter()
            .any(|entry| entry.output == output && entry.role.is_reveal())
        {
            return;
        }
        let id = OutputId(output.id().protocol_id());
        let inherited_scale120 = self
            .entries
            .iter()
            .find(|entry| entry.output == output)
            .map(|entry| entry.scale120);
        let surface = self.compositor.create_surface(qh);
        let viewport = self
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, qh, ()));
        let fractional = self.fractional_mgr.as_ref().map(|m| {
            m.get_fractional_scale(
                &surface,
                qh,
                FractionalTarget {
                    surface: surface.clone(),
                },
            )
        });
        let Some((layer, effect, opacity)) = self.create_overlay(
            qh,
            &surface,
            &output,
            "vigil-reveal",
            true,
            self.reveal_blur,
        ) else {
            return;
        };
        self.entries.push(Entry {
            id,
            output,
            surface,
            role: SurfaceRole::Reveal(layer),
            background_effect: effect,
            opacity,
            viewport,
            fractional,
            scale120: initial_scale120(inherited_scale120),
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
        if first && self.entries[idx].role.is_reveal() {
            let lock_geometry = self
                .entries
                .iter()
                .find(|entry| entry.id == id && entry.role.is_lock())
                .map(|entry| (entry.px, entry.scale120));
            if lock_geometry != Some((px, scale120)) {
                self.pending_scenes.push((id, false));
            }
        } else if first {
            let is_lock = self.entries[idx].role.is_lock();
            if is_lock {
                self.commit_first_frame(idx);
            }
            self.pending_scenes.push((id, is_lock));
        } else if resized {
            self.session.output_resized(id, &info);
        } else {
            self.session.output_rebound(id, &info);
        }
        self.dirty.mark(id);
        self.wake.wake();
        let elapsed = configure_started.elapsed();
        if elapsed >= Duration::from_millis(8) {
            eprintln!("vigil-lock: output {:?} configure: {:?}", id, elapsed);
        }
    }

    fn commit_first_frame(&mut self, idx: usize) {
        let px @ (w, h) = self.entries[idx].px;
        let id = self.entries[idx].id;
        let role = self.entries[idx].role.name();
        self.session.force_copy_out(id);
        let Some(pool) = self.entries[idx].pool.as_mut() else {
            self.schedule_present_retry(id);
            return;
        };
        let stride = w as usize * 4;
        let Ok((buffer, canvas)) =
            pool.create_buffer(w as i32, h as i32, stride as i32, wl_shm::Format::Xrgb8888)
        else {
            self.schedule_present_retry(id);
            return;
        };
        self.metrics.record_buffer_acquire();
        canvas.fill(0);
        let drew = self.session.render(
            id,
            FrameTarget {
                buffer: &mut *canvas,
                width: w,
                height: h,
                stride,
                damage: None,
            },
        );
        if drew {
            self.metrics.record_render();
        }
        let surface = &self.entries[idx].surface;
        let _ = buffer.attach_to(surface);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        self.metrics.record_commit();
        self.entries[idx].committed = true;
        if frame_hash_enabled()
            && let Some(pool) = self.entries[idx].pool.as_mut()
        {
            emit_committed_frame_hash(id, role, px, stride, pool, &buffer);
        }
    }

    fn present(&mut self, idx: usize) {
        if !self.entries[idx].configured {
            return;
        }
        if self.unlock_sent && self.entries[idx].role.is_lock() {
            let id = self.entries[idx].id;
            self.present_retry.remove(&id);
            return;
        }
        let (w, h) = self.entries[idx].px;
        let force = !self.entries[idx].committed;
        let id = self.entries[idx].id;
        let overlay_progress = (!self.entries[idx].role.is_lock()).then_some(self.overlay_progress);
        let overlay = !self.entries[idx].role.is_lock();
        let surface_opacity = self.entries[idx].opacity.is_some();
        let is_reveal = self.entries[idx].role.is_reveal();
        let role = self.entries[idx].role.name();
        let Some(pool) = self.entries[idx].pool.as_mut() else {
            eprintln!("vigil-lock: output {id:?}: no shm pool ({w}x{h})");
            self.schedule_present_retry(id);
            return;
        };
        if !force && !self.session.scene_needs_present(id) {
            self.present_retry.remove(&id);
            return;
        }
        self.metrics.record_buffer_acquire();
        let frame_span = tracing::debug_span!(
            target: "vigil",
            "frame.present",
            output = id.0,
            forced = force
        )
        .entered();
        let stride = w as usize * 4;
        let format = if overlay {
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
        let mut drew = self.session.render(
            id,
            FrameTarget {
                buffer: &mut *canvas,
                width: w,
                height: h,
                stride,
                damage: None,
            },
        );
        if !drew && force {
            self.session.force_repaint(id);
            drew = self.session.render(
                id,
                FrameTarget {
                    buffer: &mut *canvas,
                    width: w,
                    height: h,
                    stride,
                    damage: None,
                },
            );
        }
        if !drew && !force {
            self.present_retry.remove(&id);
            return;
        }
        if let Some((frost, wallpaper)) = overlay_progress {
            let frost_alpha = if is_reveal && !self.reveal_blur {
                0.0
            } else {
                self.frost_alpha
            };
            overlay_blend(canvas, frost, wallpaper, frost_alpha, surface_opacity);
        }
        let render_elapsed = render_started.elapsed();
        if drew {
            self.metrics.record_render();
        }
        if !drew {
            eprintln!("vigil-lock: output {id:?}: first present empty; committing black {w}x{h}");
        }
        if let Some((frost, wallpaper)) = overlay_progress {
            let opacity = if self.entries[idx].role.is_reveal() {
                wallpaper
            } else {
                frost
            };
            self.entries[idx].opacity.set(opacity);
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
        drop(frame_span);
        if frame_hash_enabled()
            && let Some(pool) = self.entries[idx].pool.as_mut()
        {
            emit_committed_frame_hash(id, role, (w, h), stride, pool, &buffer);
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
        for (id, is_lock) in std::mem::take(&mut self.pending_scenes) {
            let Some(idx) = self.entries.iter().position(|entry| {
                entry.id == id && entry.configured && entry.role.is_lock() == is_lock
            }) else {
                continue;
            };
            let info = self.output_info(idx);
            if !is_lock {
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
            self.session.force_repaint(id);
            self.dirty.mark(id);
        }
        self.session.tick();
        let events: Vec<FlowEvent> = std::mem::take(&mut self.protocol_events)
            .into_iter()
            .chain(self.session.poll_events())
            .collect();
        for event in events {
            tracing::event!(
                name: "flow.input",
                target: "vigil",
                tracing::Level::INFO,
                kind = flow_event_kind(&event)
            );
            let cmds = self.flow.step(self.now(), event);
            self.run(cmds);
            if self.outcome.is_some() {
                return;
            }
        }
        let cmds = self.flow.step(self.now(), FlowEvent::Tick);
        self.run(cmds);
        if self.outcome.is_some() {
            return;
        }
        tracing::event!(
            name: "flow.wait",
            target: "vigil",
            tracing::Level::DEBUG,
            wait_ms = self
                .flow
                .next_wake()
                .map_or(-1i64, |d| d.as_millis() as i64)
        );
        let _ = self.dirty.take_all();
        for lock_pass in [0, 1] {
            for idx in 0..self.entries.len() {
                if present_priority(self.entries[idx].role.is_lock()) == lock_pass {
                    self.present(idx);
                }
            }
        }
        if !self.unlock_sent && self.reveal_entries_all_committed() {
            let cmds = self.flow.step(self.now(), FlowEvent::RevealOverlaysMapped);
            self.run(cmds);
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
        self.drop_overlays(SurfaceRole::is_warning);
        self.pre_lock_overlays = false;
    }

    fn drop_overlays(&mut self, role: fn(&SurfaceRole) -> bool) {
        self.entries.retain_mut(|entry| {
            if role(&entry.role) {
                if let Some(effect) = entry.background_effect.take() {
                    effect.destroy();
                }
                entry.opacity.destroy();
                if let Some(fractional) = entry.fractional.take() {
                    fractional.destroy();
                }
                if let Some(viewport) = entry.viewport.take() {
                    viewport.destroy();
                }
                false
            } else {
                true
            }
        });
    }

    fn reveal_entries_all_committed(&self) -> bool {
        let mut reveals = self.entries.iter().filter(|entry| entry.role.is_reveal());
        let mut any = false;
        let all = reveals.all(|entry| {
            any = true;
            entry.configured && entry.committed
        });
        any && all
    }

    fn send_unlock(&mut self) {
        if let Some(lock) = self.lock.take() {
            lock.unlock();
            if let Err(e) = self.conn.roundtrip() {
                self.error = Some(err(format!("roundtrip after unlock: {e}")));
            }
        }
        self.unlock_sent = true;
    }
}

static TERMINATION_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_termination_signal(signal: libc::c_int) {
    let fd = TERMINATION_PIPE.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = signal as u8;
    unsafe {
        libc::write(fd, (&byte as *const u8).cast(), 1);
    }
}

fn arm_termination<'l, S: LockSession + 'static>(
    handle: &LoopHandle<'l, App<S>>,
) -> Result<(), LockError> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(err(format!(
            "termination pipe: {}",
            std::io::Error::last_os_error()
        )));
    }
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    if TERMINATION_PIPE
        .compare_exchange(
            -1,
            write_end.as_raw_fd(),
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return Err(err("termination handling armed twice in one process"));
    }
    std::mem::forget(write_end);
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let handler: extern "C" fn(libc::c_int) = on_termination_signal;
            action.sa_sigaction = handler as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(err(format!(
                    "sigaction({signal}): {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
    }
    handle
        .insert_source(
            Generic::new(File::from(read_end), Interest::READ, Mode::Level),
            |_, pipe, app: &mut App<S>| {
                let mut reader: &File = pipe;
                let mut bytes = [0_u8; 8];
                loop {
                    match reader.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(n) => {
                            for byte in &bytes[..n] {
                                if let Some(signal) = Signal::from_number(i32::from(*byte)) {
                                    app.raise(FlowEvent::Terminate(signal));
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(err)?;
    Ok(())
}

pub fn run<S: LockSession + 'static>(session: S) -> Result<LockOutcome, LockError> {
    let lock = Lock {
        transition: vigil_config::LockTransition::immediate(),
        ..Lock::default()
    };
    run_with_lock(session, &lock, Some(0))
}

pub fn run_with_lock<S: LockSession + 'static>(
    session: S,
    lock_config: &Lock,
    warning_ms_override: Option<u64>,
) -> Result<LockOutcome, LockError> {
    let root = tracing::info_span!(
        target: "vigil",
        "lock.session",
        version = env!("CARGO_PKG_VERSION"),
        outcome = tracing::field::Empty,
        error = tracing::field::Empty
    );
    let _root = root.clone().entered();
    let result = run_with_lock_body(session, lock_config, warning_ms_override);
    match &result {
        Ok(outcome) => root.record("outcome", tracing::field::display(outcome)),
        Err(error) => root.record("error", tracing::field::display(error)),
    };
    result
}

fn run_with_lock_body<S: LockSession + 'static>(
    session: S,
    lock_config: &Lock,
    warning_ms_override: Option<u64>,
) -> Result<LockOutcome, LockError> {
    let root = tracing::Span::current();

    let conn = Connection::connect_to_env().map_err(err)?;
    let (globals, event_queue) = registry_queue_init(&conn).map_err(err)?;
    let qh: QueueHandle<App<S>> = event_queue.handle();
    let mut event_loop: EventLoop<App<S>> = EventLoop::try_new().map_err(err)?;
    if let Err(error) = arm_termination(&event_loop.handle()) {
        eprintln!(
            "vigil-lock: {error}; a signal will still end the process, without closing its trace"
        );
    }
    let signal = event_loop.get_signal();
    let wake_signal = signal.clone();
    let wake = WakeHandle::new(move || wake_signal.wakeup());
    let dirty = Arc::new(DirtySet::new());
    let metrics = Arc::new(Metrics::new());

    let warning_enabled = warning_ms_override.unwrap_or(lock_config.warning.duration_ms) > 0;
    let frost_alpha = lock_config.warning.frost_alpha.clamp(0.0, 1.0);
    let reveal_blur = lock_config.transition.reveal_blurs();
    let started = std::time::Instant::now();
    let mut policy = lock_config.clone();
    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(layer_shell) => Some(layer_shell),
        Err(error) if warning_enabled => return Err(err(error)),
        Err(error) => {
            if policy.transition.ramps_in() || policy.transition.reveals() {
                eprintln!("vigil-lock: layer-shell unavailable ({error}); locking immediately");
            }
            policy.transition = vigil_config::LockTransition::immediate();
            None
        }
    };
    let (flow, boot_cmds) = LockFlow::new(
        Now {
            elapsed: Duration::ZERO,
            mono: started,
            wall: std::time::SystemTime::now(),
        },
        &policy,
        warning_ms_override,
    );
    let pre_lock_overlays = flow.phase() == vigil_flow::FlowPhase::PreLock;
    let phase_span = Some(tracing::info_span!(
        target: "vigil",
        parent: root.id(),
        "flow.phase",
        phase = %flow.phase()
    ));
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
        layer_shell,
        background_effects: BackgroundEffectState::new(&globals, &qh),
        hyprland_surfaces: globals.bind(&qh, 1..=2, ()).ok(),
        alpha_modifiers: globals.bind(&qh, 1..=1, ()).ok(),
        compositor_proxy,
        lock: None,
        viewporter: globals.bind(&qh, 1..=1, ()).ok(),
        fractional_mgr: globals.bind(&qh, 1..=1, ()).ok(),
        entries: Vec::new(),
        keyboard: None,
        held_keys: vigil_core::HeldKeys::default(),
        pointer: None,
        got_locked: false,
        outcome: None,
        error: None,
        wake: wake.clone(),
        dirty: dirty.clone(),
        metrics: metrics.clone(),
        qh: qh.clone(),
        present_retry: BTreeMap::new(),
        pending_scenes: Vec::new(),
        flow,
        started,
        overlay_progress: (0.0, 0.0),
        frost_alpha,
        reveal_blur,
        pre_lock_overlays,
        scene_ids: BTreeSet::new(),
        initial_outputs_added: false,
        unlock_sent: false,
        ramp_scene_synced: false,
        root_span: root.clone(),
        phase_span,
        protocol_events: Vec::new(),
    };
    eprintln!(
        "vigil-lock: frost opacity lever: {}",
        if app.hyprland_surfaces.is_some() {
            "hyprland-surface-v1 (blur strength follows)"
        } else if app.alpha_modifiers.is_some() {
            "wp-alpha-modifier-v1"
        } else {
            "none; per-pixel tint"
        }
    );

    app.run(boot_cmds);
    if let Some(error) = app.error.take() {
        return Err(error);
    }

    let outputs: Vec<_> = app.outputs.outputs().collect();
    for output in outputs {
        app.add_output(&qh, output);
    }
    app.initial_outputs_added = true;

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(err)?;
    app.session.set_runtime(wake, dirty, metrics);

    let started = std::time::Instant::now();
    let metrics_at_start = app.metrics.snapshot();
    while app.outcome.is_none() && app.error.is_none() {
        let mut timeout = match app.session.wait_decision() {
            WaitDecision::Frame(delay) | WaitDecision::Timer(delay) => Some(delay),
            WaitDecision::Indefinite => None,
        };
        if let Some(animation) = app.flow.next_wake() {
            timeout = Some(timeout.map_or(animation, |current| current.min(animation)));
        }
        if let Some(retry) = soonest_retry(
            &app.present_retry,
            |id| {
                app.entries
                    .iter()
                    .any(|e| e.id == id && !(app.unlock_sent && e.role.is_lock()))
            },
            std::time::Instant::now(),
        ) {
            timeout = Some(timeout.map_or(retry, |current| current.min(retry)));
        }
        let _iteration = tracing::debug_span!(target: "vigil", "loop.iteration").entered();
        event_loop.dispatch(timeout, &mut app).map_err(err)?;
        app.wake.acknowledge();
        app.metrics.record_wake();
        app.tick();
    }
    app.phase_span = None;
    report_idle_cost(&metrics_at_start, &app.metrics, started);
    if matches!(app.outcome, Some(LockOutcome::Terminated(_)))
        && let Some(lock) = app.lock.take()
    {
        std::mem::forget(lock);
    }
    match (&app.error, app.outcome) {
        (Some(_), _) => Err(app.error.take().expect("checked above")),
        (None, Some(outcome)) => Ok(outcome),
        (None, None) => unreachable!(),
    }
}

fn report_idle_cost(
    start: &slint_idle_runtime::MetricsSnapshot,
    metrics: &Metrics,
    started: std::time::Instant,
) {
    let end = metrics.snapshot();
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let per_min = |a: u64, b: u64| (b.saturating_sub(a) as f64) * 60.0 / secs;
    eprintln!(
        "vigil-lock: idle cost over {:.0}s: {:.0} wakes/min, {:.0} renders/min, \
         {:.0} commits/min, {:.0} buffer acquires/min",
        secs,
        per_min(start.wake_requests, end.wake_requests),
        per_min(start.renders, end.renders),
        per_min(start.commits, end.commits),
        per_min(start.buffer_acquires, end.buffer_acquires),
    );
}

impl<S: LockSession> SessionLockHandler for App<S> {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.got_locked = true;
        self.raise(FlowEvent::LockConfirmed);
        for entry in &self.entries {
            if entry.role.is_lock() {
                self.dirty.mark(entry.id);
            }
        }
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.lock = None;
        self.raise(if self.got_locked {
            FlowEvent::LockInvalidated
        } else {
            FlowEvent::LockDenied
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
        let Some(idx) = self
            .entries
            .iter()
            .position(|entry| entry.role.layer() == Some(layer))
        else {
            return;
        };
        let id = self.entries[idx].id;
        if self.entries[idx].role.is_reveal() {
            self.entries.remove(idx);
            return;
        }
        eprintln!("vigil-lock: warning surface {id:?} closed");
        self.entries.remove(idx);
        self.raise(FlowEvent::OverlayClosed);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(idx) = self
            .entries
            .iter()
            .position(|entry| entry.role.layer() == Some(layer))
        else {
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
        keyboard_left(&mut self.held_keys, &mut self.session);
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.held_keys
            .press(event.raw_code, event.keysym.raw(), event.utf8.clone());
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
        let (keysym, utf8) = self
            .held_keys
            .pressed(event.raw_code)
            .unwrap_or((event.keysym.raw(), None));
        self.deliver_input(InputEvent::Key {
            keysym,
            utf8,
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
        let keysym = self
            .held_keys
            .release(event.raw_code)
            .unwrap_or(event.keysym.raw());
        self.deliver_input(InputEvent::Key {
            keysym,
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
                    self.raise(FlowEvent::PointerEnter {
                        x: event.position.0 * scale,
                        y: event.position.1 * scale,
                    });
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
            Capability::Keyboard => {
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
                keyboard_left(&mut self.held_keys, &mut self.session);
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
        if self.initial_outputs_added && genuinely_new {
            self.raise(FlowEvent::OutputAdded);
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
        self.raise(FlowEvent::OutputGone);
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
            self.present_retry.remove(&id);
            self.session.output_gone(id);
        }
    }
}

impl<S: LockSession> Dispatch<WpFractionalScaleV1, FractionalTarget> for App<S> {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        target: &FractionalTarget,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(idx) = state
                .entries
                .iter()
                .position(|entry| entry.surface == target.surface)
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
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore HyprlandSurfaceManagerV1);
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore HyprlandSurfaceV1);
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore WpAlphaModifierV1);
wayland_client::delegate_noop!(@<S: LockSession + 'static> App<S>: ignore WpAlphaModifierSurfaceV1);

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use vigil_flow::{FlowCmd, FlowEvent, LockFlow, Now};

    #[derive(Default)]
    struct Session {
        left: u32,
    }

    impl super::LockSession for Session {
        fn output_ready(&mut self, _: super::OutputId, _: &super::OutputInfo) {}
        fn output_resized(&mut self, _: super::OutputId, _: &super::OutputInfo) {}
        fn force_copy_out(&mut self, _: super::OutputId) {}
        fn output_gone(&mut self, _: super::OutputId) {}
        fn focus_output(&mut self, _: super::OutputId) {}
        fn caps_lock(&mut self, _: bool) {}
        fn tick(&mut self) {}
        fn render(&mut self, _: super::OutputId, _: super::FrameTarget<'_>) -> bool {
            false
        }
        fn wait_decision(&self) -> super::WaitDecision {
            super::WaitDecision::Indefinite
        }
        fn keyboard_left(&mut self) {
            self.left += 1;
        }
    }

    #[test]
    fn a_keyboard_leave_drops_held_keys_without_ending_grace() {
        let at = |ms| Now::at(Duration::from_millis(ms));
        let lock = vigil_config::Lock {
            grace_secs: 5,
            ..Default::default()
        };
        let (mut flow, _) = LockFlow::new(at(0), &lock, Some(0));
        let total = lock.transition.in_ms();
        flow.step(at(total), FlowEvent::Tick);
        let cmds = flow.step(at(total + 10), FlowEvent::LockConfirmed);
        assert!(cmds.contains(&FlowCmd::StartAuth), "{cmds:?}");

        let mut held = vigil_core::HeldKeys::default();
        held.press(50, 0xfe08, None);
        let mut session = Session::default();
        super::keyboard_left(&mut held, &mut session);
        assert_eq!(session.left, 1, "the session drops its held modifiers");
        assert_eq!(held.release(50), None, "the held press is forgotten");

        let cmds = flow.step(
            at(total + 20),
            FlowEvent::Input(vigil_core::InputEvent::Key {
                keysym: 0x61,
                utf8: Some("a".into()),
                pressed: true,
            }),
        );
        assert!(
            cmds.contains(&FlowCmd::DetachAuth),
            "the leave was not input, so grace is still there for a key: {cmds:?}"
        );
    }

    #[test]
    fn a_retry_for_a_departed_output_cannot_steer_the_loop() {
        let now = Instant::now();
        let mut retries = super::BTreeMap::new();
        retries.insert(super::OutputId(1), (now - Duration::from_secs(60), 3_u32));
        assert_eq!(
            super::soonest_retry(&retries, |_| false, now),
            None,
            "a retry with no live output must not be considered"
        );
        assert_eq!(
            super::soonest_retry(&retries, |_| true, now),
            Some(Duration::ZERO),
            "a live output past its deadline is due now"
        );
    }

    #[test]
    fn the_soonest_live_deadline_wins() {
        let now = Instant::now();
        let mut retries = super::BTreeMap::new();
        retries.insert(super::OutputId(1), (now + Duration::from_millis(800), 0));
        retries.insert(super::OutputId(2), (now + Duration::from_millis(200), 0));
        assert_eq!(
            super::soonest_retry(&retries, |_| true, now),
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            super::soonest_retry(&retries, |id| id == super::OutputId(1), now),
            Some(Duration::from_millis(800))
        );
    }

    use super::{initial_scale120, pixel_frost, present_priority, surface_role_is_warning};

    #[test]
    fn hotplug_after_lock_creates_a_lock_surface_not_a_warning() {
        assert!(surface_role_is_warning(true, false));
        assert!(!surface_role_is_warning(true, true));
        assert!(!surface_role_is_warning(false, false));
        assert!(!surface_role_is_warning(false, true));
    }

    #[test]
    fn lock_handoff_inherits_fractional_output_scale() {
        assert_eq!(initial_scale120(Some(150)), 150);
        assert_eq!(initial_scale120(None), 120);
    }

    #[test]
    fn the_pure_signal_table_matches_libc() {
        use vigil_flow::Signal;
        assert_eq!(Signal::Hangup.number(), libc::SIGHUP);
        assert_eq!(Signal::Interrupt.number(), libc::SIGINT);
        assert_eq!(Signal::Terminate.number(), libc::SIGTERM);
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            assert_eq!(
                Signal::from_number(signal).map(Signal::number),
                Some(signal)
            );
        }
    }

    #[test]
    fn lock_surface_renders_before_overlapping_overlays() {
        assert!(present_priority(true) < present_priority(false));
    }

    #[test]
    fn surface_opacity_lever_bakes_full_tint() {
        assert_eq!(pixel_frost(0.25, true), 1.0);
        assert_eq!(pixel_frost(0.25, false), 0.25);
    }

    #[test]
    fn the_frame_fingerprint_is_the_published_fnv_1a_constants() {
        assert_eq!(super::frame_hash(b""), "cbf29ce484222325");
        assert_eq!(super::frame_hash(b"a"), "af63dc4c8601ec8c");
        assert_eq!(super::frame_hash(b"foobar"), "85944171f73967e8");
        let black = vec![0_u8; 4096];
        let mut nearly_black = black.clone();
        nearly_black[4095] = 1;
        assert_ne!(super::frame_hash(&black), super::frame_hash(&nearly_black));
    }
}

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

#[cfg(test)]
mod ramp_commit_only_tests {
    use super::ramp_commit_only;

    const OVERLAY_OK: (bool, bool, bool) = (false, true, true);
    const LOCK: (bool, bool, bool) = (true, false, false);

    #[test]
    fn a_settled_lever_ramp_skips_rendering() {
        assert!(ramp_commit_only(
            0.0,
            0.0,
            true,
            [OVERLAY_OK, OVERLAY_OK].into_iter()
        ));
    }

    #[test]
    fn every_condition_alone_forces_the_full_round() {
        assert!(!ramp_commit_only(0.5, 0.0, true, [OVERLAY_OK].into_iter()));
        assert!(!ramp_commit_only(0.0, 0.0, false, [OVERLAY_OK].into_iter()));
        assert!(!ramp_commit_only(
            0.0,
            0.0,
            true,
            [OVERLAY_OK, (false, false, true)].into_iter()
        ));
        assert!(!ramp_commit_only(
            0.0,
            0.0,
            true,
            [OVERLAY_OK, (false, true, false)].into_iter()
        ));
    }

    #[test]
    fn the_first_tick_after_a_wallpaper_fade_renders_a_full_round() {
        assert!(
            !ramp_commit_only(0.0, 0.2, true, [OVERLAY_OK].into_iter()),
            "the tick after a fade must re-render the pure tint"
        );
        assert!(ramp_commit_only(0.0, 0.0, true, [OVERLAY_OK].into_iter()));
    }

    #[test]
    fn lock_surfaces_neither_qualify_nor_disqualify() {
        assert!(ramp_commit_only(
            0.0,
            0.0,
            true,
            [LOCK, OVERLAY_OK].into_iter()
        ));
        assert!(!ramp_commit_only(0.0, 0.0, true, [LOCK].into_iter()));
        assert!(!ramp_commit_only(0.0, 0.0, true, std::iter::empty()));
    }
}

#[cfg(test)]
mod overlay_blend_tests {
    use super::*;

    fn reference(
        canvas: &mut [u8],
        frost: f32,
        wallpaper: f32,
        frost_alpha: f32,
        surface_opacity: bool,
    ) {
        let wallpaper = wallpaper.clamp(0.0, 1.0);
        let tint_alpha =
            (pixel_frost(frost, surface_opacity) * frost_alpha * (1.0 - wallpaper)).clamp(0.0, 1.0);
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

    fn scribble(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn the_fast_path_is_byte_identical_to_the_reference() {
        for &surface_opacity in &[true, false] {
            for &frost in &[0.0, 0.001, 0.35, 0.42, 0.9648, 1.0] {
                for &wallpaper in &[0.0, 0.001, 0.5, 0.97, 1.0, 1.5, -0.2] {
                    for &frost_alpha in &[0.0, 0.35, 1.0] {
                        let mut fast = scribble(64 * 4, 7);
                        let mut slow = fast.clone();
                        overlay_blend(&mut fast, frost, wallpaper, frost_alpha, surface_opacity);
                        reference(&mut slow, frost, wallpaper, frost_alpha, surface_opacity);
                        assert_eq!(
                            fast, slow,
                            "diverged at frost={frost} wallpaper={wallpaper}                              frost_alpha={frost_alpha} lever={surface_opacity}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn frost_only_output_ignores_the_rendered_input() {
        let mut a = scribble(64 * 4, 1);
        let mut b = scribble(64 * 4, 200);
        overlay_blend(&mut a, 0.42, 0.0, 0.35, true);
        overlay_blend(&mut b, 0.42, 0.0, 0.35, true);
        assert_eq!(a, b);
    }

    #[test]
    fn a_reveal_blend_still_depends_on_the_rendered_input() {
        let mut a = scribble(64 * 4, 1);
        let mut b = scribble(64 * 4, 200);
        overlay_blend(&mut a, 1.0, 0.5, 0.35, true);
        overlay_blend(&mut b, 1.0, 0.5, 0.35, true);
        assert_ne!(a, b, "wallpaper blend lost its input dependence");
    }

    #[test]
    fn a_tail_shorter_than_a_pixel_is_left_alone_by_both_paths() {
        let original = scribble(4 * 3 + 2, 9);
        let mut fast = original.clone();
        let mut slow = original.clone();
        overlay_blend(&mut fast, 0.5, 0.0, 0.35, true);
        reference(&mut slow, 0.5, 0.0, 0.35, true);
        assert_eq!(fast, slow);
        assert_eq!(&fast[12..], &original[12..], "tail bytes must survive");
    }
}
