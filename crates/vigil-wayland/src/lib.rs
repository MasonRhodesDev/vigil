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
//!
//! `VIGIL_FRAME_HASH=1` adds an `event=frame.hash` record (output, role,
//! size, FNV-1a of the pixels) for every frame this crate commits, on the
//! journald stream beside `frame.present`. It answers "was that first lock
//! frame actually the picture, or was it black" — the question issue #86
//! came down to — from a capture-free trace. Off by default and free when
//! off; hashing walks the whole buffer, so a hashed run's timings are not
//! comparable with an unhashed one's.

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

use vigil_config::Lock;
use vigil_core::{FrameTarget, InputEvent, OutputId, OutputInfo};
pub use vigil_flow::LockOutcome;
use vigil_flow::{FlowCmd, FlowEvent, LockFlow, Now};

mod hyprland_surface;
use hyprland_surface::hyprland_surface_manager_v1::HyprlandSurfaceManagerV1;
use hyprland_surface::hyprland_surface_v1::HyprlandSurfaceV1;

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
    /// Re-arm this output's next present *without* asking the scene to
    /// redraw: the retained scene is already what the user is looking at,
    /// and only the freshly acquired buffer is empty.
    ///
    /// Split from `force_repaint` because the handoff calls it from inside
    /// a configure callback (issue #86): the first lock buffer must cost a
    /// shadow copy-out and never a scene draw there (issue #37). The
    /// default is the conservative one — correct, just costlier.
    fn force_copy_out(&mut self, id: OutputId) {
        self.force_repaint(id);
    }
    fn output_gone(&mut self, id: OutputId);
    /// A pre-lock warning surface is ready. The default creates the same scene
    /// as a lock surface; implementations can keep authentication controls
    /// hidden until `locked`.
    fn warning_output_ready(&mut self, id: OutputId, info: &OutputInfo) {
        self.output_ready(id, info);
    }
    /// Overlay compositing values for this frame (frame-grid quantized by
    /// the controller, so this is called at most once per frame).
    fn overlay_progress(&mut self, _frost: f32, _wallpaper: f32) {}
    fn overlay_elements(&mut self, _elements: &[vigil_flow::ElementSample]) {}
    /// The pointer entered this output (panel-follows-pointer signal).
    fn focus_output(&mut self, id: OutputId);
    fn caps_lock(&mut self, on: bool);
    /// Drain asynchronous session state (auth, logind, join socket, asset
    /// readiness) into controller events. Called once per tick, before the
    /// controller steps.
    fn poll_events(&mut self) -> Vec<FlowEvent> {
        Vec::new()
    }
    /// Execute one session-side controller command (panel, auth, logind
    /// hint, readiness signal, UI input). Wayland-side commands never reach
    /// here — the event loop owns those.
    fn flow_command(&mut self, _cmd: &FlowCmd) {}
    /// ~16ms cadence: clock, Slint timers.
    fn tick(&mut self);
    /// Draw this output's scene if dirty; return whether pixels changed.
    fn render(&mut self, id: OutputId, target: FrameTarget<'_>) -> bool;
    /// Whether this output owes a present, answered without a buffer.
    ///
    /// The presenter asks this *before* acquiring one: a buffer acquired
    /// and dropped un-attached on a clean scene makes the compositor reply
    /// `delete_id`, which wakes the loop, which acquires another (#65).
    /// Conservative default: unknown means present.
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

/// Stable, inert names for [`FlowEvent`]s in diagnostic records.
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
        FlowEvent::Tick => "Tick",
        _ => "other",
    }
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
    /// Stable label for trace records. A frame fingerprint is only readable
    /// next to which surface produced it: during the handoff two roles share
    /// one output id and the hashes interleave.
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

/// Whether an OverlayProgress tick may skip rendering and only latch the
/// surface opacity with a bare commit.
///
/// Pure, because this is the decision the ramp's smoothness rides on and it
/// has four ANDed conditions - exactly the shape where an untested arm
/// hides. True only when: the frost is running alone (wallpaper == 0, so
/// the buffer is input-independent), a full session round has already
/// synced the scene, and every overlay surface both has an opacity lever
/// and has committed its first real buffer (#35/#37: a bare commit on an
/// uncommitted surface would map it empty).
fn ramp_commit_only(
    wallpaper: f32,
    prev_wallpaper: f32,
    scene_synced: bool,
    entries: impl Iterator<Item = (bool, bool, bool)>,
) -> bool {
    // Both this tick's wallpaper AND the previous one must be zero. The
    // invariant commit-only rides on is a property of the buffer already
    // committed on the surface - and on the reveal's descending edge
    // (wallpaper > 0 -> 0.0, which Reveal::sample produces on every
    // unlock) that buffer still carries the half-dissolved wallpaper. One
    // full round rebakes it; from then on prev is 0.0 and the latch
    // resumes. The same edge covers a mid-warning hotplug driving
    // wallpaper-ready back to false.
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

/// Composite the frost tint (and, during reveal, the lock wallpaper) into a
/// rendered overlay buffer. wl_shm ARGB8888 is premultiplied. With a
/// whole-surface opacity lever the tint is baked at full strength and
/// `frost` drives the surface — and, on Hyprland, the blur strength too.
///
/// Split out of `present` and kept pure so the fast path below can be held
/// byte-identical to the arithmetic it replaces by a test, instead of by
/// trust. The measurement that forced this, labelled by build: at
/// 3840x2160 the per-pixel loop cost 46 ms per frame in release and
/// 375-405 ms in debug, against 4-58 ms for the Slint render beside it -
/// so a machine rendering three outputs serially shed most of a 1.5 s
/// ease's 45 steps, and under the debug build the snap was first
/// captured on, collapsed to two visible frames.
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
        // Frost-only: the output does not depend on the rendered input at
        // all - every pixel becomes the same premultiplied tint, computed
        // once and written per 4-byte chunk instead of recomputed through
        // ~25M per-channel float round-trips at 4K. Measured, labelled by
        // build: release 46 ms -> 4 ms per 4K frame; debug 375-405 ms ->
        // ~15 ms (debug is the build the snap was first captured under).
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

/// Whether `VIGIL_FRAME_HASH=1` asked for a fingerprint of every committed
/// frame.
///
/// Read once. This sits on the present path, and `var_os` takes the env lock
/// and scans the whole environment per call — the cost of the diagnostic
/// must be zero when nobody asked for it.
fn frame_hash_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VIGIL_FRAME_HASH").is_some_and(|value| value == "1"))
}

/// FNV-1a over the pixels about to be committed.
///
/// Stable rather than `DefaultHasher`, whose algorithm is deliberately
/// unspecified across builds — the same rationale (and the same constants)
/// as `vigil-sim`'s `frame_hash`. The question this answers is "is the lock
/// surface's first frame the same picture the warning surface last showed",
/// and two runs of two binaries have to agree on the answer. Fingerprint,
/// not a security digest.
fn frame_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Record what this frame looks like, beside the `frame.present` span.
///
/// INFO, not DEBUG like `frame.present` itself: the span-lines layer hints
/// `LevelFilter::INFO` at the default detail, so a DEBUG record would need
/// `SPAN_LINES=frames` as well and `VIGIL_FRAME_HASH=1` alone would print
/// nothing. One opt-in knob, or the diagnostic is a trap. Volume is bounded
/// by the operator having asked for it.
///
/// Capture-free: this hashes vigil's own buffer. ADR 0004 binds only
/// vigil's registry globals, and nothing here reads the compositor's
/// framebuffer.
fn emit_frame_hash(id: OutputId, role: &'static str, px: (u32, u32), canvas: &[u8]) {
    if !frame_hash_enabled() {
        return;
    }
    tracing::event!(
        name: "frame.hash",
        target: "vigil",
        tracing::Level::INFO,
        output = id.0,
        role = role,
        width = px.0,
        height = px.1,
        hash = frame_hash(canvas).as_str()
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
    /// The lifecycle controller: every transition decision lives here, and
    /// this loop only executes its commands (issue #43).
    flow: LockFlow,
    started: std::time::Instant,
    /// Latest overlay compositing values from the controller. Only one
    /// overlay generation animates at a time (warning surfaces are retired
    /// before reveal surfaces exist), so one value serves both; an overlay
    /// still mapped after its ramp ended holds the final value.
    overlay_progress: (f32, f32),
    frost_alpha: f32,
    /// The opt-in reveal blurs the uncovered desktop (frost_out_ms > 0). Off
    /// by default: the reveal carries no blur region and no tint.
    reveal_blur: bool,
    /// Pre-lock overlays should exist (a ramp is running and cleanup has
    /// not retired them).
    pre_lock_overlays: bool,
    scene_ids: BTreeSet<OutputId>,
    initial_outputs_added: bool,
    unlock_sent: bool,
    /// A session.overlay_progress round has been REQUESTED since startup -
    /// not proof one landed - and it is never reset. The resets that force
    /// a full round after scene changes are carried elsewhere: by
    /// entry.committed (cleared on every configure) and by the
    /// previous-wallpaper guard in ramp_commit_only. Known reliance
    /// (review F7): a mid-ramp PointerEnter can re-show the panel and
    /// cursor, and only the frost-only fill overwriting every pixel keeps
    /// them invisible - main re-hid them on every tick, the latch path
    /// does not.
    ramp_scene_synced: bool,
    /// The lock's root span, kept so phases can be parented to it
    /// explicitly. A phase is created from inside `tick`, where the current
    /// span is whichever `loop.iteration` is running, so without this the
    /// tree would hang phases off arbitrary wakes.
    root_span: tracing::Span,
    /// Span covering the phase the controller is currently in. Replaced -
    /// and so emitted - on every `FlowCmd::PhaseChanged`.
    ///
    /// Held but deliberately *not* entered; children are given this span
    /// as an explicit parent instead. Not because entering loses records -
    /// it does not: an entered phase swapped from inside `tick` exits out
    /// of stack order, but the subscriber removes an exited span by id
    /// wherever it sits on the thread's stack and still closes it, so its
    /// record is still written (measured: two `flow.phase` records at both
    /// tiers either way, over repeated runs; an earlier "frames lost them"
    /// observation had compared a clean exit against a killed run, and a
    /// kill loses whatever is still open at every tier). What entering
    /// does corrupt is frames-detail parentage: an entered phase becomes
    /// the contextual parent of every `loop.iteration`, and the
    /// replacement phase created inside `tick` nests under whichever
    /// iteration happens to be running (measured: `flow.phase` with a
    /// `loop.iteration` parent). Session detail hides the damage only
    /// because filtered ancestors are skipped. Explicit parents keep
    /// phases and iterations siblings under the root at every tier.
    phase_span: Option<tracing::Span>,
    /// Controller events raised inside protocol callbacks. Drained at the
    /// top of the next tick so no callback creates surfaces or tears the
    /// lock down mid-dispatch — the same rule presentation follows, and the
    /// ordering the pre-controller code had (callbacks set flags; begin_lock
    /// and the unlock ran from tick).
    protocol_events: Vec<FlowEvent>,
}

impl<S: LockSession> App<S> {
    fn deliver_input(&mut self, event: InputEvent) {
        // The controller decides whether input cancels a warning, dismisses
        // grace, or reaches the UI (FlowCmd::DispatchInput). Deferred: a
        // grace dismissal releases the session lock, which must not run
        // inside a wl_keyboard callback.
        self.raise(FlowEvent::Input(event));
    }

    /// Raise a controller event from a protocol callback (deferred, see
    /// [`App::protocol_events`]).
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

    /// Execute controller commands: Wayland effects here, session effects
    /// through the [`LockSession`] adapter.
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
                        // Frost-only tick on lever surfaces: the buffer is
                        // already the full-strength tint, so a render would
                        // reproduce it byte for byte (overlay_blend's
                        // fast-path property). Latch the new opacity with a
                        // bare commit - no buffer, no render - and the ramp
                        // runs at grid rate even where a present round
                        // costs hundreds of milliseconds.
                        for entry in &self.entries {
                            if !entry.role.is_lock() {
                                entry.opacity.set(frost);
                                entry.surface.commit();
                                // The metric's contract is commits, not
                                // commits that carried a buffer (review F5:
                                // the wire showed 46 while the counter saw
                                // 4, in exactly the phase it measures).
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
                        // The session pass hides the panel and cursor; once
                        // a full round has run, later frost-only ticks need
                        // only the opacity latch above.
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
                // Diagnostics belong to the loop, not the adapter: routed
                // through the session it is silently dropped by every
                // implementation that does not override flow_command —
                // exactly the silence the variant exists to prevent.
                FlowCmd::Journal(note) => eprintln!("vigil-lock: {note}"),
                // Explicit, not left to the catch-all below: routed to the
                // session it would reach a `flow_command` that most
                // implementations do not override, and be dropped.
                FlowCmd::PhaseChanged { from, to } => {
                    // Parented at the outgoing phase, so the transition's
                    // timestamp falls inside the interval of the phase it
                    // leaves rather than the one it enters.
                    let phase = self.phase_span.as_ref().and_then(tracing::Span::id);
                    tracing::event!(
                        name: "flow.transition",
                        target: "vigil",
                        parent: phase,
                        tracing::Level::INFO,
                        from = %from,
                        to = %to
                    );
                    // Dropping the outgoing span is what emits it, so a
                    // phase's duration closes at the transition rather than
                    // whenever the field is next touched.
                    self.phase_span = Some(tracing::info_span!(
                        target: "vigil",
                        parent: self.root_span.id(),
                        "flow.phase",
                        phase = %to
                    ));
                }
                FlowCmd::Exit(outcome) => self.outcome = Some(outcome),
                // Listed, not caught by a wildcard. A `_ =>` arm here would
                // route a future variant to `flow_command`, whose trait
                // default is empty - so the next command added would be
                // dropped silently, which is exactly what the comment above
                // `Journal` warns about. Naming them makes the compiler
                // raise it instead.
                ref session_cmd @ (FlowCmd::ShowPanel(_)
                | FlowCmd::DispatchInput(_)
                | FlowCmd::StartAuth
                | FlowCmd::ShowAuthError(_)
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
            // An empty input region: pointer events reach the restored
            // desktop underneath for the whole fade.
            let region = self.compositor_proxy.create_region(qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        // Blur is a pre-lock warning signal only: the reveal overlay carries
        // no blur region, so unlocking uncovers a sharp desktop.
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
            // The session is already released; nothing new may map.
            return;
        }
        // The initial snapshot and later metadata callback can name the same
        // object. Object identity is authoritative even before metadata exists.
        let adding_warning = surface_role_is_warning(self.pre_lock_overlays, self.lock.is_some());
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
                // configure now with a copy-out of the retained scene (the
                // warning's last frame) — or black if there is none; the
                // scene is built or rebound on the next tick and paints
                // over it.
                self.commit_first_frame(idx);
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
        // iteration. Repainting stays outside protocol callbacks so redraws
        // coalesce and no scene is ever constructed here (issue #37); the
        // first-frame commit above is the bounded exception, a copy-out of a
        // scene that already exists.
        self.dirty.mark(id);
        self.wake.wake();
        let elapsed = configure_started.elapsed();
        if elapsed >= Duration::from_millis(8) {
            eprintln!("vigil-lock: output {:?} configure: {:?}", id, elapsed);
        }
    }

    /// Satisfy a lock-surface configure with its first buffer, before the
    /// tick that builds or rebinds the scene has run.
    ///
    /// The warning overlay and the lock surface of one output share a single
    /// retained `OutputWindow` (`output_rebound`), so when a warning ran
    /// here the window's shadow already holds a fully rendered scene — the
    /// exact picture on screen at this instant. Copy it out and the
    /// warn→lock cut has no black frame in it (issue #86). Black is the
    /// fallback, for the cases where no scene exists yet (`--immediate`, a
    /// hotplug while locked, a warning that never painted) and for a
    /// configure at a size the retained scene cannot fill — `render`
    /// answers false for both.
    ///
    /// Kept next to present() so buffer acquisition stays auditable in one
    /// place. The invariant is not "exactly two call sites": it is that a
    /// protocol callback constructs no scene and does no unbounded work
    /// (issue #37 — building the Slint scene inline here serialized the
    /// reveal across outputs). Copying an already-built scene's shadow into
    /// an already-sized buffer is a bounded memcpy (2.2 ms at 4K) and does
    /// neither. Committing at the acked configure size is DESIGN §12
    /// invariant 1; `px` is that size, set by the caller.
    fn commit_first_frame(&mut self, idx: usize) {
        let px @ (w, h) = self.entries[idx].px;
        let id = self.entries[idx].id;
        let role = self.entries[idx].role.name();
        // Re-arm the copy-out before asking for it: a settled scene reports
        // it owes nothing, and the shadow is settled precisely when the
        // warning left a finished frame in it. `force_copy_out`, not
        // `force_repaint` — the scene is already correct, only the freshly
        // acquired buffer is empty, and a redraw request here would put
        // scene work back inside the configure callback.
        self.session.force_copy_out(id);
        let Some(pool) = self.entries[idx].pool.as_mut() else {
            self.schedule_present_retry(id);
            return;
        };
        let stride = w as usize * 4;
        // XRGB: a lock surface is opaque, and the compositor composites
        // nothing under it.
        let Ok((buffer, canvas)) =
            pool.create_buffer(w as i32, h as i32, stride as i32, wl_shm::Format::Xrgb8888)
        else {
            self.schedule_present_retry(id);
            return;
        };
        self.metrics.record_buffer_acquire();
        let drew = self.session.render(
            id,
            FrameTarget {
                buffer: &mut *canvas,
                width: w,
                height: h,
                stride,
            },
        );
        if drew {
            self.metrics.record_render();
        } else {
            canvas.fill(0);
        }
        emit_frame_hash(id, role, px, canvas);
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
        // Probe before acquiring. A buffer taken and dropped un-attached on
        // a clean scene is not free: the compositor answers every
        // `wl_buffer.destroy` with `delete_id`, which makes the Wayland fd
        // readable, which defeats the timeout the loop just armed, which
        // brings us straight back here for another buffer. Measured at 24k
        // iterations/s against Hyprland with nothing ever drawn, and at one
        // per timeout against wlroots -- which does not reply eagerly, so
        // the nested suite could never see it (#65).
        //
        // A forced present still acquires: an output that has not committed
        // yet must get a frame even over a quiescent scene (#35/#37).
        if !force && !self.session.scene_needs_present(id) {
            // Nothing owed, so no attempt is outstanding. Clearing here
            // keeps the retry map's invariant positional-proof: a stale
            // past-deadline entry clamps the loop timeout to zero and
            // spins, which is the same family of bug as #65 itself.
            self.present_retry.remove(&id);
            return;
        }
        self.metrics.record_buffer_acquire();
        // Opened after the dirtiness probe: a present that owes nothing is
        // not a frame, and counting it as one would bury the real frames in
        // the answer to "what happened during this transition". Dropped
        // when `present` returns, on every path including the early ones,
        // so a failed buffer acquisition still reads as a frame that cost
        // time.
        let _frame = tracing::debug_span!(
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
        if !drew && !force {
            self.present_retry.remove(&id);
            return;
        }
        if let Some((frost, wallpaper)) = overlay_progress {
            // The reveal is blur- AND tint-free: its buffer is pure
            // wallpaper. pixel_frost() forces full frost on lever surfaces
            // (the surface opacity carries the ramp there), so a nonzero
            // frost_alpha would still bake a gray tint even with frost
            // pinned 0. Zero the tint explicitly for the reveal role.
            // A blur-free reveal renders zero tint: pixel_frost() forces full
            // frost on lever surfaces, so a nonzero frost_alpha would bake a
            // gray tint even with frost pinned 0. A blurring reveal keeps the
            // configured alpha so its frost fade carries a tint like the lock.
            let frost_alpha = if is_reveal && !self.reveal_blur {
                0.0
            } else {
                self.frost_alpha
            };
            overlay_blend(canvas, frost, wallpaper, frost_alpha, surface_opacity);
        }
        // Captured after the overlay blend: the full-buffer premultiply is
        // the cost this diagnostic exists to surface on slow compositors.
        let render_elapsed = render_started.elapsed();
        if drew {
            self.metrics.record_render();
        }
        if !drew {
            eprintln!("vigil-lock: output {id:?}: first present empty; committing black {w}x{h}");
        }
        // After the blend, before the attach: this is the fingerprint of the
        // bytes the compositor is about to show, not of an intermediate.
        // Outside the timing capture above so the diagnostic never inflates
        // the number the diagnostic above reports.
        emit_frame_hash(id, role, (w, h), canvas);
        if let Some((frost, wallpaper)) = overlay_progress {
            // The warning fades via frost (surface opacity ramps the blur
            // strength in); the reveal fades via the lock wallpaper's
            // opacity, with frost pinned 0 - so unlock has no blur or tint.
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
        // Drain async session state only after session.tick() has pumped
        // it: readiness read before the pump left the timeline waiting on a
        // wallpaper that had already arrived, and with no wakeup scheduled
        // the warning never committed and the session never locked.
        let events: Vec<FlowEvent> = std::mem::take(&mut self.protocol_events)
            .into_iter()
            .chain(self.session.poll_events())
            .collect();
        for event in events {
            // Diagnostic record: which inputs actually reach the flow, at
            // what elapsed time. Session tier: a handful per lock.
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
        // What the flow armed after this tick - the fact whose absence a
        // 123 s live hang came down to, and which nothing recorded.
        tracing::event!(
            name: "flow.wait",
            target: "vigil",
            tracing::Level::DEBUG,
            wait_ms = self
                .flow
                .next_wake()
                .map_or(-1i64, |d| d.as_millis() as i64)
        );
        // The DirtySet is advisory, not the render gate. The software adapter
        // cannot intercept Slint's request_redraw (the slint::Window belongs to
        // the inner MinimalSoftwareWindow, so core never calls the tracking
        // wrapper), which left animations and input-driven changes unpresented
        // on metal. Every configured surface is offered a present each tick;
        // render_if_needed is a no-op for a clean scene and present() then
        // commits nothing.
        let _ = self.dirty.take_all();
        // During handoff both surfaces deliberately share one output ID and
        // one retained OutputWindow. Render the fresh lock surfaces first;
        // otherwise an overlay consumes the one pending software frame and
        // the lock surface commits its black fallback. Two index passes: no
        // per-tick allocation or sort (present() never adds/removes entries).
        for lock_pass in [0, 1] {
            for idx in 0..self.entries.len() {
                if present_priority(self.entries[idx].role.is_lock()) == lock_pass {
                    self.present(idx);
                }
            }
        }
        if !self.unlock_sent && self.reveal_entries_all_committed() {
            // The overlays just mapped: tell the controller so the lock is
            // released now rather than at the map deadline.
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
}

/// Lock the session immediately and run until unlocked/denied/invalidated.
pub fn run<S: LockSession + 'static>(session: S) -> Result<LockOutcome, LockError> {
    let lock = Lock {
        transition: vigil_config::LockTransition::immediate(),
        ..Lock::default()
    };
    run_with_lock(session, &lock, Some(0))
}

/// Run the locker under `lock` policy: an optional capture-free warning
/// (cancelable, `duration_ms > 0`) or the short non-cancelable transition
/// before acquiring session-lock, and the reveal fade after unlock.
/// `warning_ms_override` carries `--warn`/`--no-warn`.
pub fn run_with_lock<S: LockSession + 'static>(
    session: S,
    lock_config: &Lock,
    warning_ms_override: Option<u64>,
) -> Result<LockOutcome, LockError> {
    // The root of the whole lock. Entered for the duration of the body, so
    // every span and event below becomes its descendant without a parent
    // being threaded through the executor.
    //
    // `outcome` and `error` are declared Empty and recorded here, after the
    // body returns: a span's fields are fixed at creation, and how the lock
    // ended is the one attribute that cannot be known then. The body is a
    // separate function so its `?` early returns - every setup failure from
    // connect_to_env through WaylandSource::insert - still land on this
    // recording; recording at the body's own tail left a failed setup
    // emitting a `lock.session` with neither field, indistinguishable from
    // a session that ended without an outcome.
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
        Ok(outcome) => root.record("outcome", tracing::field::debug(outcome)),
        Err(error) => root.record("error", tracing::field::display(error)),
    };
    result
}

/// The fallible body of [`run_with_lock`]. The caller has entered the
/// `lock.session` root span and records `outcome`/`error` from this
/// function's return value, so a `?` here never skips the recording.
fn run_with_lock_body<S: LockSession + 'static>(
    session: S,
    lock_config: &Lock,
    warning_ms_override: Option<u64>,
) -> Result<LockOutcome, LockError> {
    // The entered `lock.session` root, reborrowed for explicit parenting.
    let root = tracing::Span::current();

    let conn = Connection::connect_to_env().map_err(err)?;
    let (globals, event_queue) = registry_queue_init(&conn).map_err(err)?;
    let qh: QueueHandle<App<S>> = event_queue.handle();
    let mut event_loop: EventLoop<App<S>> = EventLoop::try_new().map_err(err)?;
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
            // No overlays are possible: neither ramp nor reveal can run.
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
    // The phase the controller starts in is never announced - there is no
    // transition into it - so open its span here or the first phase of every
    // lock is missing from the trace, and the frames before the first
    // transition sit inside no phase at all.
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

    // The controller's opening commands (hide the panel for a ramp, or take
    // the session lock at once on the immediate path).
    app.run(boot_cmds);
    if let Some(error) = app.error.take() {
        return Err(error);
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
        if let Some(animation) = app.flow.next_wake() {
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
        // One span per wake, at `frames` detail. This is what places a
        // transition relative to the loop that produced it: every
        // frame.present sits inside the loop.iteration that drove it, and a
        // flow.transition's t_us falls inside exactly one of them.
        let _iteration = tracing::debug_span!(target: "vigil", "loop.iteration").entered();
        event_loop.dispatch(timeout, &mut app).map_err(err)?;
        app.wake.acknowledge();
        app.metrics.record_wake();
        // A frame deadline, Wayland input, or an asynchronous worker wakeup
        // all converge here. No expensive client work belongs in callbacks.
        app.tick();
    }
    // Close the phase before the session, so the tree nests the way it
    // happened rather than by whatever order the fields drop in.
    app.phase_span = None;
    match (&app.error, app.outcome) {
        (Some(_), _) => Err(app.error.take().expect("checked above")),
        (None, Some(outcome)) => Ok(outcome),
        (None, None) => unreachable!(),
    }
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
            // Cosmetic: the fade simply loses this output.
            self.entries.remove(idx);
            return;
        }
        eprintln!("vigil-lock: warning surface {id:?} closed");
        self.entries.remove(idx);
        // A cancelable warning ends here; a transition must still lock.
        // The controller knows which it is running.
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
        // Deferred: a transition commits on topology change, and
        // begin_lock must not create lock surfaces for an output sctk has
        // not finished removing.
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
    use super::{initial_scale120, pixel_frost, present_priority, surface_role_is_warning};

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
    fn surface_opacity_lever_bakes_full_tint() {
        assert_eq!(pixel_frost(0.25, true), 1.0);
        assert_eq!(pixel_frost(0.25, false), 0.25);
    }

    #[test]
    fn the_frame_fingerprint_is_the_published_fnv_1a_constants() {
        // Pinned to the algorithm, not to whatever this build's hasher does:
        // the point of the record is comparing a warning frame's hash to a
        // lock frame's hash, possibly from two different binaries. Vectors
        // from the FNV-1a 64-bit reference.
        assert_eq!(super::frame_hash(b""), "cbf29ce484222325");
        assert_eq!(super::frame_hash(b"a"), "af63dc4c8601ec8c");
        assert_eq!(super::frame_hash(b"foobar"), "85944171f73967e8");
        // A one-byte difference anywhere must move it: a black first frame
        // and a copied-out warning frame must never fingerprint alike.
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

    // (is_lock, has_lever, committed)
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
        // wallpaper > 0: the blend depends on the rendered input.
        assert!(!ramp_commit_only(0.5, 0.0, true, [OVERLAY_OK].into_iter()));
        // Scene not yet synced: panel/cursor hiding has not been applied.
        assert!(!ramp_commit_only(0.0, 0.0, false, [OVERLAY_OK].into_iter()));
        // An overlay without a lever needs its pixels re-blended per tick.
        assert!(!ramp_commit_only(
            0.0,
            0.0,
            true,
            [OVERLAY_OK, (false, false, true)].into_iter()
        ));
        // An uncommitted overlay must not receive a bare commit: it would
        // map with no buffer (#35/#37).
        assert!(!ramp_commit_only(
            0.0,
            0.0,
            true,
            [OVERLAY_OK, (false, true, false)].into_iter()
        ));
    }

    #[test]
    fn the_first_tick_after_a_wallpaper_fade_renders_a_full_round() {
        // The invariant is about the buffer already committed on the
        // surface, not the value arriving this tick. On the reveal's
        // descending edge (wallpaper > 0 -> 0.0) the committed buffer still
        // carries the half-dissolved wallpaper; skipping the render there
        // freezes it for the whole frost-out. Reproduced in pixels: with a
        // linear 165/1500 reveal the composite stuck at 141 where main
        // reached 172.
        assert!(
            !ramp_commit_only(0.0, 0.2, true, [OVERLAY_OK].into_iter()),
            "the tick after a fade must re-render the pure tint"
        );
        // ... and only that one tick: once a full round has rebaked the
        // buffer, prev is 0.0 and commit-only resumes.
        assert!(ramp_commit_only(0.0, 0.0, true, [OVERLAY_OK].into_iter()));
    }

    #[test]
    fn lock_surfaces_neither_qualify_nor_disqualify() {
        // The lock surface has no lever and is not part of the ramp; its
        // state must be invisible to this decision.
        assert!(ramp_commit_only(
            0.0,
            0.0,
            true,
            [LOCK, OVERLAY_OK].into_iter()
        ));
        // ... and a world with only lock surfaces has nothing to latch.
        assert!(!ramp_commit_only(0.0, 0.0, true, [LOCK].into_iter()));
        assert!(!ramp_commit_only(0.0, 0.0, true, std::iter::empty()));
    }
}

#[cfg(test)]
mod overlay_blend_tests {
    use super::*;

    /// The arithmetic present() shipped before the fast path existed,
    /// transcribed verbatim. The fast path must be byte-identical to this
    /// on every input, or a "performance" change is silently a visual one.
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

    /// Deterministic junk that exercises every byte value.
    fn scribble(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn the_fast_path_is_byte_identical_to_the_reference() {
        // The frost-only fill is valid precisely because the output ignores
        // the rendered input; the wallpaper>0 loop must keep depending on
        // it. Cover both, across the ramp and both lever states, on
        // buffers whose every input byte differs.
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
        // The property the fill relies on, asserted directly rather than
        // implied: two entirely different renders blend to the same bytes
        // when wallpaper is zero.
        let mut a = scribble(64 * 4, 1);
        let mut b = scribble(64 * 4, 200);
        overlay_blend(&mut a, 0.42, 0.0, 0.35, true);
        overlay_blend(&mut b, 0.42, 0.0, 0.35, true);
        assert_eq!(a, b);
    }

    #[test]
    fn a_reveal_blend_still_depends_on_the_rendered_input() {
        // The discrimination sibling: wallpaper>0 must NOT collapse to a
        // constant, or the reveal would fade a grey card instead of the
        // lock wallpaper.
        let mut a = scribble(64 * 4, 1);
        let mut b = scribble(64 * 4, 200);
        overlay_blend(&mut a, 1.0, 0.5, 0.35, true);
        overlay_blend(&mut b, 1.0, 0.5, 0.35, true);
        assert_ne!(a, b, "wallpaper blend lost its input dependence");
    }

    #[test]
    fn a_tail_shorter_than_a_pixel_is_left_alone_by_both_paths() {
        // as_chunks_mut::<4> ignores a ragged tail; the fill must too.
        let original = scribble(4 * 3 + 2, 9);
        let mut fast = original.clone();
        let mut slow = original.clone();
        overlay_blend(&mut fast, 0.5, 0.0, 0.35, true);
        reference(&mut slow, 0.5, 0.0, 0.35, true);
        assert_eq!(fast, slow);
        assert_eq!(&fast[12..], &original[12..], "tail bytes must survive");
    }
}
