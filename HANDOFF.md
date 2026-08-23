# vigil — session handoff (2026-08-23)

Working notes for whoever picks this up next. Authoritative sources:
[DESIGN.md](DESIGN.md) for architecture, GitHub issues for the backlog.
This file is a snapshot; delete it when it goes stale.

## Where things stand

vigil is **in production on this machine**: it is the greeter (greetd
runs `/usr/bin/vigil`) and the lockscreen (hypridle's `lock_cmd` and
`before_sleep_cmd` run `vigil-lock --daemonize`). Both ship as packages
— COPR `solaris765/vigil` and the `[mason]` pacman repo — built by CI
from tags. **`v0.3.2` is the current release** (2026-08-23; ci, Package,
Security, Release all green at 6924216). The Arch reference machine
still has `0.2.16` installed — `pacman -Syu vigil` before any metal
check below, otherwise you are validating the old locker.

0.3 is the capture-free lock: one ARGB overlay per output, frost via
the compositor background-effect protocol (tint-only fallback), explicit
`--wait` readiness, singleton guard, join semantics, and the nested sway
suite gating Release (issue #47 is the umbrella).

Founded 2026-08-03; greeter and lock both went live 2026-08-05.

## What runs where

| Thing | Path | Notes |
|---|---|---|
| Greeter | `/usr/bin/vigil` (pkg) | `/etc/greetd/config_default.toml` + `game_mode_login.toml`; `.pre-vigil` backups beside them |
| Lock | `/usr/bin/vigil-lock` (pkg) | `/usr/share/hypr-de/hypr/hypridle.conf`; `inhibit_sleep=3` closes the suspend race |
| Meta+L | `loginctl lock-session` | hypr-DE `main.lua`; all lock paths converge on hypridle's `lock_cmd` |
| Config | `/etc/greetd/vigil.toml` (not created yet) | example at `/usr/share/vigil/vigil.toml.example` |
| State | `/var/lib/vigil/state.toml` | tmpfiles.d, greeter-writable; last user + session |
| PAM | `/etc/pam.d/vigil-lock` | pass-through hook (`auth include login`) — policy is the operator's |

The hand-install era is fully retired: the `/usr/local/bin` copies are
deleted, everything on PATH is packaged, and `dnf update vigil` is the
upgrade path.

## The PM loop (Mason's standing direction)

Work the GitHub backlog in a loop: **research/plan with Claude Fable
subagent forks → implement → PM verifies → ship**. On the Arch
reference machine the implementer is Fable itself (Mason's direction,
2026-08-13: no codex here — do not chase its auth); the codex-exec
variant (`codex exec -m gpt-5.6-sol -c model_reasoning_effort=low`)
was the Fedora machine's setup. It closed 11 issues in one day with
one transient wedge.

What makes it work:

- **Specs must leave zero design latitude**: exact signatures, exact
  struct definitions, exact test names and assertions, files-touched
  list, gates. Low-reasoning codex executes faithfully but does not
  design. Every spec that had that shape landed first-pass.
- Planning forks earn their keep — they caught a dead-code rule (grace
  on `--daemonize`), a precedence flattening bug in the lock, and an
  API mismatch, all before codex ran.
- **Verify independently**: `cargo clippy --workspace --all-targets`
  (zero warnings), `cargo test --workspace`, and `tests/e2e/run.sh`
  for anything touching the login flow. Codex reports its own gates;
  run them yourself anyway.
- Ship = commit (note codex authorship), push, `gh issue close` with a
  summary, rebuild+install the release binary if deploy-relevant.
- **Codex wedge signature**: 25-minute timeout with zero file edits.
  Kill it plus any `codex-code-mode-host`, retry once; implement
  directly on a third strike. Do NOT build CPU-sampling stall
  detectors — codex's gate runs (long dep compiles in its own process
  group) look identical to a wedge and false-alarm constantly.

## Open issues (18, triaged 2026-08-23)

Umbrella: **#20** (roadmap), **#47** (0.3 release). Closed this session
with evidence: #48 (package CI), #36 (PAM before lock), #44 (nested
suite). Full triage is in the issue comments; the shape:

- **VM-validated and CLOSED 2026-08-23** (vmkit, hypr-de VM, real
  Hyprland 0.56.2, main build): #49 (exits on unlock), #50 (singleton +
  4 ms join, no PAM noise), #38 (reveal shows no caret/pipe). Still
  awaiting metal (multi-output/GPU/suspend, recipes on the issues):
  #37 (outputs lock together), #35 (FALLBACK hotplug round).
- **#53** (new): overlay ramps present at ~57 fps instead of the 33 ms
  timeline cadence — 2× CPU during ramps, starves software GL; fix
  sketch on the issue (dedupe unchanged Slint property sets).
- **Metal gates for 0.3** — #46 (acceptance matrix) and #40
  (mixed-scale handoff): the checklist is `tests/metal/checklist.md`.
  #47 closes when those two do.
- **Greeter metal** — #27 multi-GPU GL docked, #7 real suspend at the
  greeter, #6 dock plug/unplug at the greeter. All three are proven
  VM-impossible (see their threads); pair them with the #46 session.
- **#51** nested S1 flake on CI sway 1.9 only (locally sway 1.12 is
  clean). Diagnostics landed (6243b41/b36a134); the next failure will
  say whether the locker dies post-readiness or sway 1.9 double-grants.
  Re-run the job if Release trips on it.
- **#52 animated frost on manual lock** — IMPLEMENTED on main
  (2026-08-23), nested S1–S9 green; stays open for the metal checklist
  (`tests/metal/checklist.md` "#52") which gates tagging **0.4.0**.
  Key facts: blur strength IS animatable on Hyprland —
  `hyprland_surface_v1.set_opacity` multiplies the blur pass
  (`ElementRenderer.cpp`, `overallA`) regardless of
  `decoration:blur:ignore_opacity` (true by default, so
  `wp_alpha_modifier_v1` alone does NOT drive blur on stock Hyprland).
  Vendored XML + `wayland-scanner` bindings in
  `crates/vigil-wayland/src/hyprland_surface.rs`. ADR 0004 amended in
  desktop-commons. hypr-DE `lock-cmd.sh`/`hypridle.conf` comments still
  say "immediate" — reword on the next hypr-DE sweep (no behaviour change).
  **VM-validated 2026-08-23** (vmkit + hypr-de VM, real Hyprland): blur
  ramps with the lever, reveal restores the desktop pixel-exact, logind
  path end-to-end, locked idle frame-quiet, `--immediate` clean; full
  report on the issue. Metal remainder: multi-output feel (#37/#40/#35),
  suspend, hardware GL. Filed #53 (ramp present cadence) from it.
- **Simulator track** (no blockers): #43 shared controllers is the
  foundation, then #42 failure controls, #41 multi-output presets,
  #45 deterministic scenarios + visual regression.

## Landmines (learned the hard way, all real)

- **Never `systemctl restart greetd` mid-session.** The compositor
  process survives but greetd rebuilds the session supervision chain:
  a fresh uwsm session starts, sockets collide, hypr-DE's
  `wayland-env-guard.path` trips its trigger limit, and XDG autostart
  silently doesn't come up. Config changes take effect at the next
  logout anyway. (Bit Mason on 2026-08-05; recovery is
  `systemctl --user reset-failed wayland-env-guard.path` + rerun
  `/usr/libexec/hypr-de/wayland-env-guard`.)
- **Lockers must not call PAM `acct_mgmt`** — `pam_unix`'s account
  phase needs setuid `unix_chkpwd`, which fails from a systemd user
  service (hypridle), rejecting *correct* passwords.
- **Release `wl_keyboard`/`wl_pointer` on `remove_capability`** — a
  dropped-but-unreleased keyboard keeps delivering, so the post-resume
  rebind doubles every keystroke.
- **Rebuilt scenes start blank.** Anything that recreates an
  `OutputWindow` (VT switch, resume, hotplug, resize) must replay
  `UiSnapshot` or the user sees an empty card.
- **Full-scene software re-render at 4K starves the event loop** and
  the kernel drops real keystrokes. Slint partial-repaints into a
  persistent shadow buffer; presents copy out and composite the
  cursor. Never regress this. Test with release builds.
- **`cargo build -p vigil --example X` does not rebuild the binary** —
  cost an hour of confusion twice.
- **System config writes are classifier-blocked** in this harness:
  stage the file in the scratchpad and hand Mason a `sudo install`
  command, or ask him to switch modes.
- hypr-DE files use `@DATADIR@`/`@BINDIR@`/`@LIBEXECDIR@` templating —
  substitute **all** placeholders (`packaging/substitute.sh` semantics)
  or you silently break unrelated keybinds.

## Session notes

### 2026-08-23
- v0.3.0 → 0.3.2 were cut in one night: 0.3.0/0.3.1 Release runs failed
  on the S1 flake (#51) and a Security-audit trip, 0.3.2 went green on
  every workflow. Don't read the red runs on f23ef4b/b36a134 as code
  regressions.
- `singleton-guard` now lives in desktop-commons (6924216); vigil only
  consumes it. The nested suite's `wait_free` relies on the
  kernel releasing the flock on death — leftover files are fine.
- HANDOFF "Open issues" had rotted to the 08-12 view while 30 commits
  landed; when closing an issue from here, `gh issue close` in the same
  step as the push or it drifts again.

### 2026-08-12 (Arch reference machine)

- **No codex on this machine** (a stale binary with dead auth sits on
  PATH — ignore it). Implementation is Claude Fable directly, per
  Mason.
- e2e on Arch needs `virtme-ng` and `qemu-hw-display-virtio-gpu-pci`
  (both in extra, now installed) — Fedora's qemu bundles the device,
  Arch splits it. rustc had to move to 1.97 for the slint 1.17 lockfile.
- **QEMU's console shows stale fbcon for ~10s** after vigil starts
  presenting (both cards; even VT switches don't show). Not a vigil bug
  — 23 successful draws/flips logged during the freeze. drive.py now
  waits for first paint before driving; don't chase the black frames.
- The e2e guest inherits the HOST's installed theme (shared read-only
  rootfs) — Mason's lmtt wallpaper theme, not the repo default. Frame
  assertions must stay theme-agnostic (check_frames.py explains).

## Testing

- `tests/nested/run.sh` — headless sway (no ext-background-effect, so
  tint tier) runs vigil-lock with `WAYLAND_DEBUG=1`; `check_trace.py`
  asserts locked-before-ready, join, cancel, two-output handoff, no
  capture bind, hotplug, clean teardown. Opt-in locally (needs `sway`,
  `wtype`), **gates Release in CI**. Every scenario runs under
  `timeout`: a hang is the failure.
- `tests/metal/checklist.md` — the 0.3 gate that sway cannot cover
  (#46/#40). Recovery is `loginctl unlock-session` from SSH/another VT.
- `tests/e2e/run.sh` — full greeter-spec login in QEMU (two virtio-gpu
  cards, username stage, VT round trip, mouse-driven session picker,
  wrong-then-right password). The main safety net; run it for anything
  touching the login flow. Needs `sudo prlimit --memlock=unlimited
  --pid $$` first if QEMU complains about io_uring.
- `tests/vt/run.sh` — on-metal harness, run from a text VT
  (Ctrl+Alt+F3). Fake greetd, password `vigil-test`, 180s failsafe.
- `cargo run -p vigil --example theme_preview -- out.png` — headless
  theme render; also the fastest way to check click targets.
