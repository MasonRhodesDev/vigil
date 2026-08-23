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
use slint_idle_runtime::{DirtySet, Metrics, WaitDecision, WakeHandle};
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

use vigil_config::{LockTransition, LockWarning};
use vigil_core::{FrameTarget, InputEvent, OutputId, OutputInfo};
use vigil_warning::ElementSample;
use vigil_warning::{Phase as WarningPhase, Reveal, Timeline};

mod hyprland_surface;
use hyprland_surface::hyprland_surface_manager_v1::HyprlandSurfaceManagerV1;
use hyprland_surface::hyprland_surface_v1::HyprlandSurfaceV1;

/// Before `unlock_and_destroy`, wait this long for the reveal overlays'
/// first buffer commit so the desktop is never exposed un-frosted. A
/// compositor that refuses to configure a layer surface while locked falls
/// through to the plain unlock.
const REVEAL_MAP_DEADLINE: Duration = Duration::from_millis(250);
/// The reveal fade is cosmetic: whatever state it is in, the process exits
/// this long after the fade started.
const REVEAL_HARD_DEADLINE: Duration = Duration::from_millis(2_000);
/// Minimum spacing between ramp *propagations* (the dirty-marking that
/// leads to a commit). The event loop wakes faster than the animation clock
/// — every commit earns a `wl_buffer.release`, and the ramp values are
/// continuous, so any earlier wake sees a "changed" progress — and without
/// this floor the ramps self-sustain a commit-per-render-time loop at ~2x
/// the intended rate (issue #53). Sampling itself is pure math and runs on
/// every tick, so phase transitions, cancellation, wallpaper readiness, and
/// join requests are never delayed or swallowed by the floor. Derived from
/// the timeline's own frame period (minus scheduling jitter) so the two
/// cannot drift apart.
const ANIM_PROPAGATE_MIN: Duration = Duration::from_millis(vigil_warning::FRAME_INTERVAL_MS - 5);

/// Whether a fresh ramp value may be propagated to the scene at `now`.
fn anim_propagate_due(last: Option<Duration>, now: Duration) -> bool {
    last.is_none_or(|at| now.saturating_sub(at) >= ANIM_PROPAGATE_MIN)
}

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
    /// Mark this output's scene as needing a full repaint even though no
    /// Slint state changed. Called before a forced present would otherwise
    /// commit an empty buffer (issue #35).
    fn force_repaint(&mut self, _id: OutputId) {}
    fn output_gone(&mut self, id: OutputId);
    /// A pre-lock warning surface is ready. The default creates the same scene
    /// as a lock surface; implementations can keep authentication controls
    /// hidden until `locked`.
    fn warning_output_ready(&mut self, id: OutputId, info: &OutputInfo) {
        self.output_ready(id, info);
    }
    fn warning_progress(&mut self, _frost: f32, _wallpaper: f32) {}
    fn warning_elements(&mut self, _elements: &[ElementSample]) {}
    /// Unlock is authorized and the reveal overlay is fading
    /// (`frost`/`wallpaper` ∈ [0, 1], both falling). The default reuses the
    /// warning hook: hide the card, keep the wallpaper, present.
    fn reveal_progress(&mut self, frost: f32, wallpaper: f32) {
        self.warning_progress(frost, wallpaper);
    }
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
    Lock {
        _surface: SessionLockSurface,
    },
    /// Post-authorization overlay that fades the wallpaper out over the
    /// frosted desktop after `unlock_and_destroy` (issue #52).
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
    fn layer(&self) -> Option<&LayerSurface> {
        match self {
            SurfaceRole::Warning(layer) | SurfaceRole::Reveal(layer) => Some(layer),
            SurfaceRole::Lock { .. } => None,
        }
    }
}

/// During handoff both surfaces share one output ID and one retained
/// OutputWindow: the lock surface renders first or the overlay consumes the
/// one pending software frame and the lock surface commits black.
fn present_priority(is_lock: bool) -> u8 {
    u8::from(!is_lock)
}

/// Whole-surface opacity, the lever that ramps compositor blur strength.
/// `ext-background-effect-v1` only toggles a blur region; Hyprland's
/// `hyprland_surface_v1.set_opacity` is documented to multiply "blur behind
/// the surface in addition to the surface's content", and
/// `wp_alpha_modifier_v1` does the same on compositors whose blur tracks
/// surface alpha. Without either, frost is a per-pixel tint ramp over a
/// constant blur.
enum SurfaceOpacity {
    Hyprland(HyprlandSurfaceV1),
    AlphaModifier(WpAlphaModifierSurfaceV1),
    None,
}

impl SurfaceOpacity {
    fn is_some(&self) -> bool {
        !matches!(self, SurfaceOpacity::None)
    }
    /// Double-buffered: takes effect on the next surface commit.
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

/// Per-pixel frost to bake into the overlay buffer. With a whole-surface
/// opacity lever the buffer carries the full tint and the surface fades.
fn pixel_frost(frost: f32, surface_opacity_available: bool) -> f32 {
    if surface_opacity_available {
        1.0
    } else {
        frost
    }
}

/// Which role a new output surface gets. A warning overlay is only correct
/// while the warning is showing AND the session lock has not been taken:
/// once locked, a warning surface would be destroyed by warning cleanup and
/// leave the output with no lock surface at all — permanently black, DESIGN
/// §12 invariant 4 (issue #35 hotplug case).
fn surface_role_is_warning(warning_active: bool, lock_taken: bool) -> bool {
    warning_active && !lock_taken
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
    pointer: Option<wl_pointer::WlPointer>,
    got_locked: bool,
    outcome: Option<LockOutcome>,
    error: Option<LockError>,
    wake: WakeHandle,
    dirty: Arc<DirtySet<OutputId>>,
    metrics: Arc<Metrics>,
    qh: QueueHandle<App<S>>,
    present_retry: BTreeMap<OutputId, (std::time::Instant, u32)>,
    /// First-configured surfaces whose scenes are built on the next tick,
    /// outside the protocol callback: (output, is-lock-role). Inline scene
    /// construction serialized the lock reveal across outputs (issue #37).
    pending_scenes: Vec<(OutputId, bool)>,
    warning: Option<Timeline>,
    warning_started: std::time::Instant,
    warning_progress: (f32, f32),
    warning_elements: Vec<ElementSample>,
    warning_frost_alpha: f32,
    warning_wait: Option<Duration>,
    scene_ids: BTreeSet<OutputId>,
    initial_outputs_added: bool,
    transition: LockTransition,
    reveal: Option<Reveal>,
    reveal_progress: (f32, f32),
    /// App-clock time at which the reveal overlays were requested; bounds
    /// the wait for their first commit (`REVEAL_MAP_DEADLINE`).
    reveal_entered: Option<Duration>,
    reveal_started_at: Option<Duration>,
    reveal_wait: Option<Duration>,
    unlock_sent: bool,
    /// App-clock time of the last warning/reveal propagation (rate floor).
    anim_propagated_at: Option<Duration>,
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

    /// One ARGB layer-shell overlay over the live desktop: blur region
    /// (compositor policy), whole-surface opacity lever when available.
    /// `pass_through` makes it inert to input (the post-unlock reveal).
    fn create_overlay(
        &self,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
        namespace: &str,
        pass_through: bool,
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
            // An empty input region: pointer events reach the restored
            // desktop underneath for the whole fade.
            let region = self.compositor_proxy.create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        let effect = self
            .background_effects
            .get_background_effect(surface, qh)
            .ok();
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
            // The session is already released; nothing new may map.
            return;
        }
        // The initial snapshot and later metadata callback can name the same
        // object. Object identity is authoritative even before metadata exists.
        let adding_warning = surface_role_is_warning(self.warning.is_some(), self.lock.is_some());
        if self
            .entries
            .iter()
            .any(|entry| entry.output == output && entry.role.is_warning() == adding_warning)
        {
            return;
        }
        let id = OutputId(output.id().protocol_id());
        // Warning and lock surfaces overlap during secure handoff. Fractional
        // scale for the new lock surface may arrive after its first configure,
        // so inherit the already-known scale for this wl_output.
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
                self.create_overlay(qh, &surface, &output, "vigil-warning", false)
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

    /// The post-unlock overlay for one output, created while the session
    /// is still locked so it is mapped (opaque wallpaper) by the time the
    /// compositor reveals the desktop.
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
        let Some((layer, effect, opacity)) =
            self.create_overlay(qh, &surface, &output, "vigil-reveal", true)
        else {
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
        if first && self.entries[idx].role.is_reveal() {
            // The reveal reuses this output's lock scene; it only needs a
            // rebuild if the compositor configured it at a different size.
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
                // All outputs lock together (issue #37): ext-session-lock
                // blanks an output until a buffer matching this configure
                // arrives, and building the Slint scene inline here
                // serialized the reveal across outputs. Satisfy the
                // configure with a solid buffer now; the scene is built on
                // the next tick and paints over it.
                self.commit_solid(idx);
            }
            self.pending_scenes.push((id, is_lock));
        } else if resized {
            self.session.output_resized(id, &info);
        } else {
            // Same-size reconfigure (DPMS wake, VT switch, hotplug
            // re-layout): the scene is quiescent, but this configure
            // invalidated the buffer. Without a repaint request, present()
            // commits a zeroed buffer and the output stays black until the
            // next clock tick (issue #35).
            self.session.output_rebound(id, &info);
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

    /// Commit a solid black buffer to satisfy a lock-surface configure
    /// before its scene exists. Kept next to present() so buffer
    /// acquisition has exactly two audited entry points.
    fn commit_solid(&mut self, idx: usize) {
        let (w, h) = self.entries[idx].px;
        let id = self.entries[idx].id;
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
        canvas.fill(0);
        self.metrics.record_buffer_acquire();
        let surface = &self.entries[idx].surface;
        let _ = buffer.attach_to(surface);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        self.metrics.record_commit();
        self.entries[idx].committed = true;
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
        if self.unlock_sent && self.entries[idx].role.is_lock() {
            // Released server-side by unlock_and_destroy; nothing to paint.
            return;
        }
        let (w, h) = self.entries[idx].px;
        let force = !self.entries[idx].committed;
        let id = self.entries[idx].id;
        // Overlay compositing progress by role; None for the opaque lock
        // surface and for a warning overlay whose timeline has retired.
        let overlay_progress = match self.entries[idx].role {
            SurfaceRole::Warning(_) => self.warning.is_some().then_some(self.warning_progress),
            SurfaceRole::Reveal(_) => Some(self.reveal_progress),
            SurfaceRole::Lock { .. } => None,
        };
        let overlay = !self.entries[idx].role.is_lock();
        let surface_opacity = self.entries[idx].opacity.is_some();
        let Some(pool) = self.entries[idx].pool.as_mut() else {
            eprintln!("vigil-lock: output {id:?}: no shm pool ({w}x{h})");
            self.schedule_present_retry(id);
            return;
        };
        self.metrics.record_buffer_acquire();
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
            },
        );
        if !drew && force {
            // A forced present over a quiescent scene: request a repaint
            // and re-render once before ever committing an empty buffer —
            // the black-until-next-clock-tick failure of issue #35.
            self.session.force_repaint(id);
            drew = self.session.render(
                id,
                FrameTarget {
                    buffer: &mut *canvas,
                    width: w,
                    height: h,
                    stride,
                },
            );
        }
        let render_elapsed = render_started.elapsed();
        if !drew && !force {
            self.present_retry.remove(&id);
            return;
        }
        if let Some((frost, wallpaper)) = overlay_progress {
            // wl_shm ARGB8888 is premultiplied. Fade the rendered lock
            // wallpaper in (or out) over a neutral frost tint while the
            // compositor supplies the live blur behind this translucent
            // surface. With a whole-surface opacity lever the tint is baked
            // at full strength and `frost` drives the surface — and, on
            // Hyprland, the blur strength with it.
            let wallpaper = wallpaper.clamp(0.0, 1.0);
            let tint_alpha = (pixel_frost(frost, surface_opacity)
                * self.warning_frost_alpha
                * (1.0 - wallpaper))
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
        if drew {
            self.metrics.record_render();
        }
        if !drew {
            eprintln!("vigil-lock: output {id:?}: first present empty; committing black {w}x{h}");
        }
        if let Some((frost, _)) = overlay_progress {
            self.entries[idx].opacity.set(frost);
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
        // Build deferred scenes in one batch: every output whose configure
        // landed since the last tick already holds a solid placeholder, so
        // the themed content appears on all of them together (issue #37).
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
            // The placeholder marked the surface committed, so the coming
            // present is not forced: guarantee the fresh scene paints.
            self.session.force_repaint(id);
            self.dirty.mark(id);
        }
        self.session.tick();
        // Sample the warning only after session.tick() has pumped async
        // results: wallpaper readiness is session state, and reading it
        // before the pump left the timeline waiting on a wallpaper that
        // had already arrived — with no further wakeups scheduled, the
        // warning never committed and the session never locked.
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
            // Values are continuous, so any wake sees "changed" progress;
            // propagating (and thus committing) is paced by the frame
            // period while phase handling below stays per-tick (issue #53).
            if anim_propagate_due(self.anim_propagated_at, elapsed) {
                let progress = (sample.frost, sample.wallpaper);
                let elements = timeline.element_samples(elapsed);
                let mut propagated = false;
                if self.warning_progress != progress {
                    self.warning_progress = progress;
                    self.session
                        .warning_progress(sample.frost, sample.wallpaper);
                    for entry in &self.entries {
                        self.dirty.mark(entry.id);
                    }
                    propagated = true;
                }
                if self.warning_elements != elements {
                    self.session.warning_elements(&elements);
                    self.warning_elements = elements;
                    propagated = true;
                }
                if propagated {
                    self.anim_propagated_at = Some(elapsed);
                }
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
        if self.session.wants_unlock() && self.tick_unlock() {
            return;
        }
        // The DirtySet is advisory, not the render gate. The software adapter
        // cannot intercept Slint's request_redraw (the slint::Window belongs to
        // the inner MinimalSoftwareWindow, so core never calls the tracking
        // wrapper), which left animations and input-driven changes unpresented
        // on metal. Every configured surface is offered a present each tick;
        // render_if_needed is a no-op for a clean scene and present() then
        // commits nothing.
        let _ = self.dirty.take_all();
        {
            let mut indices: Vec<usize> = (0..self.entries.len()).collect();
            // During handoff both surfaces deliberately share one output ID
            // and one retained OutputWindow. Render the fresh lock surface
            // first; otherwise the warning surface consumes the one pending
            // software frame and the lock surface commits its black fallback.
            indices.sort_by_key(|idx| present_priority(self.entries[*idx].role.is_lock()));
            for idx in indices {
                self.present(idx);
            }
        }
        if self.reveal.is_some() && !self.unlock_sent && self.reveal_entries_all_committed() {
            // The overlays just mapped: release the lock on the very next
            // tick instead of the next timer deadline.
            self.wake.wake();
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
    }

    /// Destroy every overlay entry matching `role`, with its protocol objects.
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

    /// Auth succeeded (or grace/logind released us). Returns true when the
    /// tick must end here: the plain unlock is final, and while the reveal
    /// overlays are still mapping there is nothing to present yet.
    fn tick_unlock(&mut self) -> bool {
        let elapsed = self.warning_started.elapsed();
        if !self.unlock_sent {
            if self.reveal.is_none() && self.can_reveal() {
                self.begin_reveal(elapsed);
            }
            if self.reveal.is_none() {
                self.finish_unlock();
                return true;
            }
            let mapped = self.reveal_entries_all_committed();
            let expired = self
                .reveal_entered
                .is_some_and(|entered| elapsed.saturating_sub(entered) >= REVEAL_MAP_DEADLINE);
            if !mapped && !expired {
                return false;
            }
            if !mapped {
                eprintln!(
                    "vigil-lock: reveal overlays not mapped within {REVEAL_MAP_DEADLINE:?}; unlocking anyway"
                );
            }
            self.send_unlock();
            self.reveal_started_at = Some(elapsed);
            if let Some(reveal) = self.reveal.as_mut() {
                reveal.start(elapsed);
            }
        }
        let Some(reveal) = self.reveal.as_ref() else {
            return true;
        };
        let sample = reveal.sample(elapsed);
        let overdue = self
            .reveal_started_at
            .is_some_and(|started| elapsed.saturating_sub(started) >= REVEAL_HARD_DEADLINE);
        if sample.done || overdue {
            self.drop_overlays(SurfaceRole::is_reveal);
            self.outcome = Some(LockOutcome::Unlocked);
            return true;
        }
        self.reveal_wait = sample.next_frame;
        let progress = (sample.frost, sample.wallpaper);
        if self.reveal_progress != progress && anim_propagate_due(self.anim_propagated_at, elapsed)
        {
            self.anim_propagated_at = Some(elapsed);
            self.reveal_progress = progress;
            self.session.reveal_progress(sample.frost, sample.wallpaper);
            for entry in &self.entries {
                if entry.role.is_reveal() {
                    self.dirty.mark(entry.id);
                }
            }
        }
        false
    }

    fn can_reveal(&self) -> bool {
        self.layer_shell.is_some()
            && self.got_locked
            && self.lock.is_some()
            && self.transition.reveals()
    }

    fn begin_reveal(&mut self, elapsed: Duration) {
        self.reveal = Some(Reveal::new(
            self.transition.wallpaper_out_ms,
            self.transition.frost_out_ms,
            self.transition.easing,
        ));
        self.reveal_entered = Some(elapsed);
        self.reveal_progress = (1.0, 1.0);
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
        // Hide the card before the first reveal frame renders.
        self.session.reveal_progress(1.0, 1.0);
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

    /// `unlock_and_destroy` + roundtrip. The unlock must reach the
    /// compositor before we exit, or the session may stay locked forever.
    fn send_unlock(&mut self) {
        if let Some(lock) = self.lock.take() {
            lock.unlock();
            if let Err(e) = self.conn.roundtrip() {
                self.error = Some(err(format!("roundtrip after unlock: {e}")));
            }
        }
        self.unlock_sent = true;
    }

    fn finish_unlock(&mut self) {
        self.send_unlock();
        self.outcome = Some(LockOutcome::Unlocked);
    }

    /// Earliest animation wakeup: warning/transition ramp, reveal fade, or
    /// the bounded wait for the reveal overlays to map.
    fn animation_wait(&self) -> Option<Duration> {
        let mut wait = self
            .warning
            .is_some()
            .then_some(self.warning_wait)
            .flatten();
        let mut merge = |candidate: Option<Duration>| {
            if let Some(candidate) = candidate {
                wait = Some(wait.map_or(candidate, |current| current.min(candidate)));
            }
        };
        if self.reveal.is_some() {
            merge(self.reveal_wait);
            if !self.unlock_sent
                && let Some(entered) = self.reveal_entered
            {
                let waited = self.warning_started.elapsed().saturating_sub(entered);
                merge(Some(REVEAL_MAP_DEADLINE.saturating_sub(waited)));
            }
        }
        wait
    }
}

/// Lock the session immediately and run until unlocked/denied/invalidated.
pub fn run<S: LockSession + 'static>(session: S) -> Result<LockOutcome, LockError> {
    run_with_warning(session, LockWarning::default(), LockTransition::immediate())
}

/// Present an optional capture-free warning (cancelable, `duration_ms > 0`)
/// or the short non-cancelable transition before acquiring session-lock,
/// and the reveal fade after unlock.
pub fn run_with_warning<S: LockSession + 'static>(
    session: S,
    warning_config: LockWarning,
    transition: LockTransition,
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
    let gui = warning_config.gui.clone();
    let mut warning = if warning_enabled {
        Some(Timeline::new(warning_config))
    } else if transition.ramps_in() {
        Some(Timeline::new_transition(
            transition.as_warning(warning_frost_alpha, gui),
        ))
    } else {
        None
    };
    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(layer_shell) => Some(layer_shell),
        Err(error) if warning_enabled => return Err(err(error)),
        Err(error) => {
            if warning.is_some() || transition.reveals() {
                eprintln!("vigil-lock: layer-shell unavailable ({error}); locking immediately");
            }
            warning = None;
            None
        }
    };
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
        warning,
        warning_started: std::time::Instant::now(),
        warning_progress: (0.0, 0.0),
        warning_elements: Vec::new(),
        warning_frost_alpha,
        warning_wait: None,
        scene_ids: BTreeSet::new(),
        initial_outputs_added: false,
        transition,
        reveal: None,
        reveal_progress: (1.0, 1.0),
        reveal_entered: None,
        reveal_started_at: None,
        reveal_wait: None,
        unlock_sent: false,
        anim_propagated_at: None,
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

    if app.warning.is_none() {
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
        if let Some(animation) = app.animation_wait() {
            timeout = Some(timeout.map_or(animation, |current| current.min(animation)));
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
            if entry.role.is_lock() {
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
        let Some(idx) = self
            .entries
            .iter()
            .position(|entry| entry.role.layer() == Some(layer))
        else {
            return;
        };
        let id = self.entries[idx].id;
        if self.entries[idx].role.is_reveal() {
            // Cosmetic: the fade simply loses this output.
            self.entries.remove(idx);
            return;
        }
        match self.warning.as_mut() {
            // A transition must still lock: treat the lost overlay as a
            // topology change and commit now.
            Some(timeline) if !timeline.cancelable() => {
                eprintln!(
                    "vigil-lock: warning surface {id:?} closed during transition; committing"
                );
                timeline.request_commit();
                self.entries.remove(idx);
            }
            _ => {
                eprintln!("vigil-lock: warning surface {id:?} closed");
                self.outcome = Some(LockOutcome::Cancelled);
            }
        }
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
    use super::{
        ANIM_PROPAGATE_MIN, anim_propagate_due, initial_scale120, pixel_frost, present_priority,
        surface_role_is_warning,
    };
    use std::time::Duration;

    #[test]
    fn hotplug_after_lock_creates_a_lock_surface_not_a_warning() {
        // The warning timeline object is retained until its GUI completes,
        // so "warning object exists" must not decide the role once the lock
        // is held (issue #35: the warning surface was destroyed by cleanup,
        // leaving the hotplugged output permanently black).
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
    fn lock_surface_renders_before_overlapping_overlays() {
        // Warning and reveal overlays share the lock surface's OutputWindow;
        // the lock surface must consume the pending frame first.
        assert!(present_priority(true) < present_priority(false));
    }

    #[test]
    fn ramp_propagation_has_a_rate_floor() {
        // Buffer-release wakeups arrive faster than the animation clock;
        // without the floor every wake re-dirties the scene (issue #53).
        // Only propagation is floored — sampling and phase handling run on
        // every tick, so cancel/commit/readiness are never swallowed.
        assert!(anim_propagate_due(None, Duration::ZERO));
        let last = Some(Duration::from_millis(100));
        assert!(!anim_propagate_due(last, Duration::from_millis(100)));
        assert!(!anim_propagate_due(
            last,
            Duration::from_millis(100) + ANIM_PROPAGATE_MIN - Duration::from_millis(1)
        ));
        assert!(anim_propagate_due(
            last,
            Duration::from_millis(100) + ANIM_PROPAGATE_MIN
        ));
        // The floor must sit strictly under the timeline's frame period or
        // every second frame would be dropped.
        assert!(ANIM_PROPAGATE_MIN < Duration::from_millis(vigil_warning::FRAME_INTERVAL_MS));
        // A clock hiccup backwards never panics and simply holds.
        assert!(!anim_propagate_due(last, Duration::from_millis(50)));
    }

    #[test]
    fn surface_opacity_lever_bakes_full_tint() {
        assert_eq!(pixel_frost(0.25, true), 1.0);
        assert_eq!(pixel_frost(0.25, false), 0.25);
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
