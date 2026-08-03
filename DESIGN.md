# vigil — design

A multi-monitor, themeable greeter for [greetd] that renders directly on
KMS/DRM. No compositor, no toolkit windowing stack: one Rust binary that
takes the seat, drives the displays, and draws a Slint scene on each one.

This document is the founding design. It records the end-state architecture,
the interfaces that make it testable, the decisions already taken (with
their rationale), and the milestones that build it out. Nothing here is a
staging shape to be replaced later: milestones are ordered slices of this
architecture, and the one deliberately disposable artifact (the M0 spike) is
labeled as such.

[greetd]: https://git.sr.ht/~kennylevinsen/greetd

## 1. Problem statement and requirements

greetd greeters today force a bad trade on multi-monitor machines:

- Terminal greeters (agreety, tuigreet) light up one output and mirror or
  black the rest, with no graphical theming.
- Graphical greeters need a compositor to exist on Wayland. Running the
  desktop's own compositor (sway, Hyprland) as the greeter session means a
  routine DE upgrade can break the login path — the single part of the
  system that must never break.
- Purpose-built kiosk compositors (cage + regreet) fix the fragility but
  inherit the kiosk model: one window spanned or cloned, no per-output
  backgrounds, no control over which display carries the login panel.

vigil closes the gap by being its own display backend: it opens the DRM
devices directly, drives every connected output at its preferred mode, and
renders an independent themed scene per output.

### Hard requirements

- **Multi-output native.** Every connected display gets its own scene:
  its own background image and fit mode, its own clock, and the login
  panel appears on exactly one output (following the pointer). Connector
  hotplug and VT switching re-modeset correctly.
- **Runtime themeable.** The look is a `.slint` file loaded at startup
  against a versioned contract; a broken or incompatible theme falls back
  to the compiled-in default. Theming never requires recompiling vigil.
- **Nothing a desktop upgrade can break.** vigil depends on the kernel
  (DRM/KMS), libseat, libinput, xkbcommon, and greetd's IPC socket —
  none of which a compositor or DE package upgrade touches.
- **greetd-native.** vigil is a standard greetd greeter: greetd spawns it
  as the configured greeter user and it authenticates via `greetd_ipc`.
  Seat and DRM access arrive through libseat under greetd's PAM session —
  the identical path cage/wlroots uses today, i.e. proven infrastructure,
  not new ground.
- **Never grab input exclusively.** vigil must not `EVIOCGRAB` evdev
  devices. Host daemons may share the seat's input devices while the
  greeter is on screen (concretely: greetd_game_mode's daemon reads
  gamepads via gilrs at the greeter). libinput does not grab by default;
  this coexistence is status quo with cage and must remain true.
- **Crash-robust in itself.** greetd's crash-loop protection terminates
  greetd when a greeter dies immediately after spawn — on this machine
  class there is no second chance behind us. This is first a design
  constraint on code quality (no panics on missing outputs, absent themes,
  unreadable config, mid-session hotplug): every failure of an optional
  subsystem degrades (compiled-in theme, solid-color background, single
  output) rather than exits. Deployments then add defense in depth with a
  same-process fallback launcher (§11) — belt and suspenders, not a
  license for vigil to be fragile.

### Non-goals (acknowledged, not designed for yet)

Stated honestly so nobody mistakes silence for support:

- Accessibility (AT-SPI, screen readers).
- IME / CJK text input (xkbcommon covers layouts and compose; full input
  methods are out of scope).
- X11 session launching helpers (regreet's `x11_prefix` behavior).
  Sessions are launched by greetd, so X11 sessions likely work unchanged,
  but they are untested and unsupported until someone cares.

These are revisit-later, not never. None of the architecture below blocks
them: input is behind its own crate boundary, and session launch is a
string handed to greetd.

## 2. Long-term goals

The architecture must accommodate all of the following without structural
rework. Each has a named seam it will land behind:

| Goal | Seam it lands behind |
|---|---|
| GL rendering (FemtoVG over GBM/EGL) for full Slint fidelity | `Presenter` trait (§4); `vigil-present-gl` crate reserved from day 1 |
| A runtime theme ecosystem | Versioned theme contract (§6) + `slint-interpreter` loader |
| Session listing and selection from `/usr/share/wayland-sessions` (honoring `NoDisplay=true`) | Contract `sessions` property + `vigil-auth` session launch path |
| On-screen status/banner channel for host integrations (e.g. an approval daemon reporting "sent to your phone…" — impossible with cage+regreet, which has no banner surface) | Reserved `status-banner` contract property + a line-oriented input the binary watches |
| Per-output HiDPI scale | Scale factor is a per-output field in the output manager; Slint windows already carry a scale factor |

## 3. Architecture

Single Rust binary, launched by greetd, structured around one calloop event
loop. Every hard subsystem is a maintained crate; we own only glue and
product logic (~2,000–2,600 bespoke LOC total, budgeted per crate in §5).

```
calloop event loop
├─ LibSeatSession (smithay) ── seat/VT switching; we only react (drm pause/activate + redraw)
├─ UdevBackend + DrmScanner (smithay + smithay-drm-extras) ── GPU + connector hotplug
├─ DrmDevice/DrmSurface (smithay) ── modeset, page flips
├─ Presenter trait ── OWNED SEAM: how a rendered frame reaches an output
│   ├─ DumbBufferPresenter (day 1): Slint SoftwareRenderer → mapped DRM dumb buffer
│   │                               (XRGB8888, dirty-region repaints)
│   └─ GbmGlPresenter (M3): FemtoVG → GBM/EGL surface behind the same trait
├─ LibinputInputBackend (smithay) + xkbcommon ── input; key repeat/compose are ours
├─ greetd_ipc ── auth conversation with greetd's socket
└─ Slint custom Platform ── one full-output Window per output
   ├─ per-output background image: stretch/fill/fit/center/tile (`image` crate)
   ├─ login panel migrates to the pointer's output (per-window `panel-visible`)
   ├─ software cursor as a scene element (no cursor plane required)
   └─ slint-interpreter runtime themes + compiled-in default (same contract)
```

Validated groundwork (each was independently verified before this design
was committed to):

- The Smithay backend crates compose without the Wayland frontend: the
  `backend_drm`, `backend_libinput`, `backend_session_libseat`, and
  `backend_udev` features pull no `wayland-server` dependency
  (feature-graph verified; compiling in a fresh project is milestone M0a).
- Slint renders to targets it does not own via a custom `Platform`
  implementation (proven in the slint-headless experiments, including an
  `OpenGLInterface` impl that the future GL presenter will reuse).
- Slint's own LinuxKMS backend already renders into DRM dumb buffers; its
  `TargetPixel`-into-mapped-buffer technique is ~50 lines we replicate.

### Architectural commitments

These are the rules that keep the first implementation from becoming
throwaway scaffolding:

- **Multi-output is the core object model from commit one.** The output
  manager owns a collection of outputs, each bundling a DRM surface, a
  `Presenter`, a Slint `Window`, and per-output state (background, scale,
  panel-visible). A single monitor is the N=1 case of the same code. There
  is no single-output path that later grows a loop around it.
- **The renderer choice is a seam, not a phase.** The software presenter
  is not interim: it remains forever as the zero-GPU path (CI on vkms,
  headless golden images, recovery on machines with broken GL). GL is
  additive fidelity behind the same trait.
- **The theme contract is complete before theme code exists.** The
  compiled-in default theme implements the full v1 contract; runtime
  themes are validated against the same contract. There is no "hardcoded
  first, contract later".
- **Input interfaces are planned whole.** Key repeat, compose, and layout
  switching are part of `vigil-input`'s public API from the start, even
  where the implementation lands in a later milestone.

## 4. Core interfaces

All cross-module types and traits live in `vigil-core`, which depends on
nothing internal and nothing heavyweight external. Sketches (signatures
will evolve; the shapes and ownership will not):

```rust
/// How a rendered frame reaches an output. Implemented by
/// vigil-present-dumb (software) and vigil-present-gl (GL, M3).
pub trait Presenter {
    fn size(&self) -> (u32, u32);
    /// Hand the caller a target to draw this frame into, then submit it
    /// (page flip). Completion is reported via the event loop, not by
    /// blocking here.
    fn with_frame(&mut self, draw: &mut dyn FnMut(FrameTarget<'_>)) -> Result<(), PresentError>;
}

/// Normalized input, decoupled from libinput/xkb types.
pub enum InputEvent {
    Key { keysym: u32, utf8: Option<String>, pressed: bool },
    PointerMotion { dx: f64, dy: f64 },
    PointerAbsolute { x: f64, y: f64 },
    PointerButton { button: u32, pressed: bool },
    // repeat events are synthesized by vigil-input and arrive as Key
}

/// The auth state machine's view of the UI (implemented by vigil-ui).
pub trait AuthUi {
    fn show_prompt(&mut self, text: &str, secret: bool);
    fn show_info(&mut self, text: &str);
    fn show_error(&mut self, text: &str);
    fn set_busy(&mut self, busy: bool);
}
// UI → auth flows back as messages: Respond(String), Cancel,
// SelectSession(usize), PowerAction(PowerAction).

/// Output lifecycle, emitted by vigil-outputs.
pub enum OutputEvent {
    Added(OutputId, OutputInfo),
    Removed(OutputId),
    NeedsRedraw(OutputId),
}
```

Dependency rules (enforced by the crate graph, checked in review):
subsystem crates depend on `vigil-core` plus their one vendored subsystem
and never on each other; only the binary crate assembles them.

## 5. Repository layout, module isolation, and budgets

A cargo workspace whose crate boundaries are the isolation mechanism:
every subsystem builds, unit-tests, and verifies alone, and the compiler
enforces that modules interact only through `vigil-core` interfaces.

```
vigil/
├─ Cargo.toml                 # workspace manifest
├─ DESIGN.md  README.md  LICENSE
├─ crates/
│  ├─ vigil-core/             # trait seams + shared types ONLY — zero smithay/slint deps
│  ├─ vigil-session/          # libseat session glue (smithay) → session events
│  ├─ vigil-outputs/          # udev scan, DRM device/surface, output manager; consumes Presenter
│  ├─ vigil-present-dumb/     # DumbBufferPresenter (SoftwareRenderer → dumb buffer)
│  ├─ vigil-present-gl/       # GbmGlPresenter — M3 slice, crate reserved from day 1
│  ├─ vigil-input/            # libinput + xkbcommon; repeat/compose → InputEvent
│  ├─ vigil-auth/             # greetd_ipc state machine; drives AuthUi
│  ├─ vigil-theme/            # contract validation, interpreter loader, compiled-in default
│  ├─ vigil-ui/               # Slint Platform glue, per-output windows, backgrounds, cursor
│  └─ vigil/                  # the binary: calloop wiring only
├─ themes/default/            # .slint sources for the compiled-in theme
├─ tests/                     # cross-crate integration: golden images + fake-greetd e2e
│  └─ golden/                 # reference PNGs
└─ .github/workflows/         # per-crate unit jobs, vkms job, QEMU multi-output job
```

Per-crate verification surface — each crate is testable without the rest
of the system:

| Crate | Tested against | Bespoke LOC budget |
|---|---|---|
| `vigil-core` | pure types, plain unit tests | (types only) |
| `vigil-session` | smoke-tested under vkms CI; logic is ~reactive glue | ~100 |
| `vigil-outputs` | vkms (real KMS ioctls, no GPU); QEMU virtio-gpu for multi-output | ~450–600 |
| `vigil-present-dumb` | headless: render into a plain byte buffer, golden-image compare | in outputs/ui budget |
| `vigil-input` | uinput-injected events | ~300–400 |
| `vigil-auth` | in-process fake greetd socket speaking `greetd_ipc` | ~250–300 |
| `vigil-theme` | contract-conformance tests over theme files (valid, broken, stale-version) | ~150–200 |
| `vigil-ui` | fully headless golden images (SoftwareRenderer → PNG) | ~250–350 platform glue + ~150 backgrounds + ~50 cursor |
| `vigil` (binary) | end-to-end under vkms + fake greetd | ~200 |

Total: ~2,000–2,600 LOC that are ours to maintain. Everything else —
modesetting, session/VT handling, udev, libinput plumbing, rendering,
IPC framing — belongs to its upstream crate.

## 6. Theme contract v1

A theme is one `.slint` file exporting a root component. vigil
instantiates it once per output. The contract is versioned
(`contract-version: 1`); the loader validates that every required
property/callback exists and is correctly typed before accepting a theme,
and otherwise logs why and uses the compiled-in default.

Inputs (set by vigil):

| Property | Type | Meaning |
|---|---|---|
| `output-name` | string | connector name, e.g. `DP-1` |
| `panel-visible` | bool | whether this output currently hosts the login panel |
| `background-source` | image | decoded per-output background |
| `clock-text` | string | preformatted clock string |
| `auth-state` | string | `idle` \| `prompting` \| `busy` \| `error` |
| `prompt-text` | string | current PAM prompt |
| `prompt-is-secret` | bool | mask the input field |
| `info-message` / `error-message` | string | PAM info / error surfaces |
| `sessions` | [string] | selectable session names |
| `selected-session` | int | index into `sessions` |
| `caps-lock` | bool | modifier indicator |
| `status-banner` | string | reserved: host integration status line (empty = hidden) |

Outputs (invoked by the theme):

| Callback | Meaning |
|---|---|
| `submit(string)` | answer to the current prompt |
| `cancel()` | restart the auth conversation |
| `session-changed(int)` | user picked a session |
| `power-action(string)` | `reboot` \| `poweroff` (commands configurable) |

Contract constraints (a consequence of the software renderer being the
permanent baseline): themes must not rely on `Path` elements, drop
shadows, or border-radius combined with `clip`; text is western-script
only for now. Themes render through the software path everywhere — GL
(M3) adds fidelity (gradients, opacity blending breadth) but a theme must
degrade gracefully rather than gate on it. These limits are contract
documentation, verified by rendering the default theme in CI golden
images.

Background configuration (per output, from `/etc/vigil/config.toml`):
image path plus fit mode `stretch` | `fill` | `fit` | `center` | `tile`,
decoded and scaled by the `image` crate into the per-output
`background-source`.

## 7. Renderer decision record

**Decision: Slint `SoftwareRenderer` into DRM dumb buffers is the day-1
presenter, and remains the permanent zero-GPU baseline.**

Why software won:

- **No GL/GBM/EGL dependencies** in the login path; nothing to break when
  Mesa updates, works on machines with broken or absent GPU acceleration.
- **CI without GPUs**: the same renderer output is byte-comparable
  headless, and runs under vkms on stock runners.
- **Upstream-blessed**: Slint's own LinuxKMS backend ships exactly this
  path (`internal/backends/linuxkms/renderer/sw.rs`); we replicate a
  known-good technique rather than inventing one.

Alternatives rejected:

- **smithay `DrmCompositor`** — its value is plane assignment and buffer
  management for a compositor's many surfaces. A greeter has one
  fullscreen scene per output; the machinery is dead weight.
- **GL-first** — dependency weight and GPU-dependent CI for no launch
  benefit. GL is not rejected as a capability: it is milestone M3, behind
  the `Presenter` trait, using the already-proven `OpenGLInterface`
  approach — but it is fidelity on top, never the baseline.

## 8. Testing strategy

Designed in from M1, not retrofitted:

- **Golden-image snapshots** (headless, every CI run): render the default
  theme through the software path into PNGs for a matrix of states
  (idle, prompting, secret prompt, error, panel-hidden) and sizes;
  pixel-compare against `tests/golden/`.
- **Fake greetd socket**: `vigil-auth` is exercised against an in-process
  Unix socket speaking `greetd_ipc` — every conversation shape (success,
  bad password, multi-prompt PAM, info/error messages, cancellation)
  without PAM or root.
- **vkms CI**: the kernel's virtual KMS driver gives real modesetting,
  page flips, and writeback connectors with zero GPU (Weston's CI is the
  precedent). Writeback screenshots verify the full outputs→present path.
- **QEMU virtio-gpu with `max_outputs=2`**: multi-output bring-up,
  hotplug (qemu monitor can toggle connectors), and panel-migration
  tests.
- **uinput**: inject keyboard/pointer events through the real libinput
  stack for `vigil-input` (repeat timing, compose sequences, seat
  permissions).

## 9. Milestones

Ordered slices of the architecture in §3. Each leaves production-shaped
code; none is scaffolding to be torn out later.

- **M0 — de-risk spike. ✅ Passed 2026-08-03** (risk register #1–2).
  Explicitly disposable, never merged (validating unknowns is the one
  place throwaway code is correct):
  - *M0a*: a fresh project compiles smithay with only `backend_drm`,
    `backend_libinput`, `backend_session_libseat`, `backend_udev` and
    links no `wayland-server`.
  - *M0b*: two Slint `Window`s with independent `SoftwareRenderer`s under
    one custom `Platform` render distinct scenes at runtime.
  - Version notes for M1: smithay 0.7.0 from crates.io suffices (no git
    pin needed yet). Slint 1.17.1 works — no need for the 1.14 pin the
    slint-headless spike carried; note `software_renderer` now lives in
    the `i-slint-renderer-software` crate and `Rgb8Pixel` is exported at
    the slint crate root, not from the renderer module.
- **M1 — architecture skeleton, end-to-end.** Every module boundary in
  its final shape: workspace + crates as in §5, multi-output-native
  output manager, `Presenter` + `DumbBufferPresenter`, `vigil-input`
  full API (repeat implemented; compose stubbed behind the API), complete
  auth state machine, theme contract v1 + compiled-in default theme.
  Login works end-to-end on real hardware. CI live: golden images, fake
  greetd, vkms.
- **M2 — product completeness.** Runtime theme loading + validation,
  background fit modes, pointer-follows-panel, connector hotplug,
  VT-switch/suspend re-modeset, session list (wayland-sessions +
  `NoDisplay`), input completeness (compose, layout config).
- **M3 — fidelity + integrations.** `GbmGlPresenter`, `status-banner`
  input channel, packaging (PKGBUILD + RPM spec), user/theming docs.

## 10. Risk register

Every unverified assumption has a named task that resolves it:

| # | Risk | Resolved by | Mitigation if it bites |
|---|---|---|---|
| 1 | Smithay backend features don't actually compile standalone in a fresh project | **M0a — PASSED 2026-08-03**: smithay 0.7.0, `default-features = false` + the five backend features compiles and links; `cargo tree -i wayland-server` is empty; libseat runtime path verified | (moot) |
| 2 | Multiple Slint windows under one custom `Platform` misbehave at runtime | **M0b — PASSED 2026-08-03**: slint 1.17.1, two `MinimalSoftwareWindow`s under one custom Platform rendered distinct scenes at distinct sizes with independent dirty tracking (touched window repaints, untouched doesn't) | (moot) |
| 3 | Smithay release cadence / API churn | pin exact version + `cargo vendor` snapshot committed | the backend half churns far less than the wayland half; upgrades can wait indefinitely |
| 4 | vkms (configfs/writeback) unavailable on CI runner kernels | M1 CI bring-up | pure-headless golden images still gate merges; vkms job becomes best-effort |
| 5 | `SoftwareRenderer` opacity/gradient support too narrow for the default theme | M1 golden images | contract limits (§6) already exclude the known gaps; default theme sticks to verified primitives |
| 6 | Hand-rolled input edge cases (repeat, compose, exotic layouts) lock the user out at the greeter | uinput tests from M1; compose behind the API until M2 | deployment fallback launcher (§11) bounds the blast radius to one degraded session |

## 11. greetd integration and deployment

vigil is a standard greetd greeter. The core integration is one line:

```toml
[default_session]
command = "/usr/bin/vigil"
user = "greeter"
```

Host-specific mechanisms layered on greetd — such as greetd_game_mode's
config-symlink swap for autologin game sessions — are orthogonal: they
change *which* greetd config is active, never how vigil works. vigil
needs no knowledge of them.

### Deployment fallback (defense in depth)

greetd's crash-loop protection means a greeter that dies immediately
takes greetd down with it — so hardened deployments point greetd at a
launcher instead of vigil directly:

```toml
command = "/usr/bin/vigil-launch"
```

`vigil-launch` (~40-line sh, shipped by the deployment, not this repo):
run vigil and timestamp the start; on exit, update a crash-streak marker
(a run ≥60s resets it) and `exec` a fallback greeter in the same session
process (`cage -s -m last -d -- regreet`); if the marker already shows ≥3
consecutive fast exits, skip vigil and exec the fallback directly. A
broken vigil build converges to a working (degraded) greeter within
seconds and login always works. This is deployment policy: vigil itself
must still satisfy the crash-robustness requirement in §1.

Concrete touch points for the greetd_game_mode deployment (changed at M1,
not before): `greetd/config_default.toml` + `greetd/game_mode_login.toml`
(`command =` lines, in lockstep), `setup.rs verify_greeter_binaries()`
(add vigil + vigil-launch; keep cage/regreet — they are the fallback),
packaging deps (vigil package; cage + regreet stay forever).

Runtime deps of the vigil binary: libseat, libinput, libxkbcommon
(+ libudev). No GL, no GBM, no Wayland libraries in the MVP.

## 12. License

vigil is GPL-3.0 (this repository's LICENSE). Slint is triple-licensed
(GPLv3 / royalty-free desktop / commercial); vigil links Slint under its
GPLv3 option, which makes the licensing story unambiguous for a system
greeter — no dependence on whether the royalty-free desktop tier covers
login managers.

## 13. Deferred alternatives

Recorded so a future stall has a known exit:

- **sway + swaybg greeter session** — fully designed during the spike
  that produced this document: sway in a locked-down greeter config with
  swaybg for per-output backgrounds and regreet (or similar) as the
  panel. Cheap and functional, rejected because it reintroduces a
  plausible-DE compositor binary into the login path. This is the
  pivot-to option if vigil stalls.
- **cosmic-greeter** — a one-line greetd config swap once its
  multi-display freeze bugs are fixed upstream.
- **Weston desktop-shell** — maximal isolation and all five background
  fit modes on every output, but no IPC of any kind: the panel can never
  follow the pointer (and kiosk-shell cannot draw image backgrounds at
  all). Recorded because it keeps coming up; the answer is no.

## 14. References

- [Smithay's anvil example, `udev.rs`](https://github.com/Smithay/smithay/blob/master/anvil/src/udev.rs)
  — the wiring template for session + udev + DRM + libinput in one
  calloop loop.
- [Slint `internal/backends/linuxkms/renderer/sw.rs`](https://github.com/slint-ui/slint/blob/master/internal/backends/linuxkms/renderer/sw.rs)
  — the dumb-buffer `TargetPixel` rendering technique replicated here.
- [naka](https://codeberg.org/kchibisov/naka) — prior-art minimal DRM
  greeter (proof the no-compositor greeter shape works).
- [greetd_ipc](https://docs.rs/greetd_ipc) — the auth protocol crate.
- slint-headless experiments — custom `Platform` driving external render
  targets, including the `OpenGLInterface` impl the M3 GL presenter will
  reuse.
