# vigil — session handoff (2026-08-05)

Working notes for whoever picks this up next. Authoritative sources:
[DESIGN.md](DESIGN.md) for architecture, GitHub issues for the backlog.
This file is a snapshot; delete it when it goes stale.

## Where things stand

vigil is **in production on this machine**: it is the greeter (greetd
runs `/usr/bin/vigil`) and the lockscreen (hypridle's `lock_cmd` and
`before_sleep_cmd` run `vigil-lock --daemonize`). Both ship as packages
— COPR `solaris765/vigil` and the `[mason]` pacman repo — built by CI
from tags. `v0.2.1` is the current release.

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

**Loose end:** `/usr/local/bin/vigil{,-lock}` still exist from the
hand-install era and **shadow the packaged binaries in PATH**. They
should be deleted (Mason's call, not done yet).

## The PM loop (Mason's standing direction)

Work the GitHub backlog in a loop: **research/plan with Claude Fable
subagent forks → implement with `codex exec -m gpt-5.6-sol -c
model_reasoning_effort=low` → PM verifies → ship**. It closed 11 issues
in one day with one transient wedge.

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

## Open issues (8)

Umbrella: **#20**. Nothing is blocked on anything else anymore.

- **#6 hotplug at the greeter**, **#7 suspend/resume at the greeter** —
  validation work; needs harness design (the dock's DP-1 is on the
  second GPU, which makes it the interesting case).
- **#10 login1 integration** — Lock/Unlock signals + `SetLockedHint`;
  also carries a documented residual from #9: invalidate an active
  grace window on `PrepareForSleep(true)`.
- **#14 → #15 → #16 theme track** — runtime theme polish, then the
  styled lmtt theme (visual parity with the retired hyprlock look —
  the thing Mason sees every morning), then light/dark via `lmtt
  switch`.
- **#17 GL presenter**, **#18 status-banner channel** — M3 fidelity.

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

## Testing

- `tests/e2e/run.sh` — full greeter-spec login in QEMU (two virtio-gpu
  cards, username stage, VT round trip, mouse-driven session picker,
  wrong-then-right password). The main safety net; run it for anything
  touching the login flow. Needs `sudo prlimit --memlock=unlimited
  --pid $$` first if QEMU complains about io_uring.
- `tests/vt/run.sh` — on-metal harness, run from a text VT
  (Ctrl+Alt+F3). Fake greetd, password `vigil-test`, 180s failsafe.
- `cargo run -p vigil --example theme_preview -- out.png` — headless
  theme render; also the fastest way to check click targets.
