# vigil

A multi-monitor, themeable [greetd](https://git.sr.ht/~kennylevinsen/greetd)
greeter that renders directly on KMS/DRM — no compositor in the login path.
One Rust binary takes the seat, drives every connected display at its
preferred mode, and draws an independent [Slint](https://slint.dev) scene on
each: per-output backgrounds, a login panel that follows the pointer, and
runtime `.slint` themes with a compiled-in fallback.

**Status: greeter-spec complete (M1.5).** The full DM flow works
end-to-end in QEMU (`tests/e2e/run.sh`): username entry, wrong-password
error with automatic retry, Escape back to the username stage, a
mouse-driven session picker over the installed
wayland-sessions/xsessions, VT switching away and back (Ctrl+Alt+Fn),
power buttons, and a successful `start_session` handoff with
`XDG_SESSION_*` env. Architecture, interfaces, and milestones are in
[DESIGN.md](DESIGN.md); next up is the on-metal VT test
(`tests/vt/run.sh`), then M2 (hotplug polish, multi-GPU, runtime
themes).

## The pair

vigil is designed as two surfaces of one product: the **greeter** (this
binary, on bare KMS at boot) and **vigil-lock**, a session lockscreen
speaking `ext-session-lock-v1` under your compositor — same theme file,
same contract, same auth seam, so login and lock are one visual identity.
Each works without the other. The locker is specified in
[DESIGN.md §12](DESIGN.md) (milestones L0–L2).

## Why

Graphical Wayland greeters need a compositor, and running a desktop
compositor as the greeter means a routine DE upgrade can break login.
Kiosk compositors (cage) avoid that but can't do per-output scenes. vigil
is its own display backend, so the login path depends only on the kernel,
libseat, libinput, xkbcommon, and greetd — none of which a desktop upgrade
touches.

## Installation

Arch, from the [mason] pacman repo:

```ini
# /etc/pacman.conf
[mason]
# Import the signing key first: https://github.com/MasonRhodesDev/arch-repo#use-it
SigLevel = Required DatabaseRequired
Server = https://masonrhodesdev.github.io/arch-repo/x86_64
```

```bash
sudo pacman -Sy vigil
```

Fedora:

```bash
sudo dnf copr enable solaris765/vigil
sudo dnf install vigil
```

The package depends on greetd. On install it replaces a stock `agreety`
command with `/usr/bin/vigil` and enables greetd as the display manager
when none is set (`graphical.target`). It does not overwrite a custom
greetd command and never restarts greetd (that tears down a live
session). Point hypridle / your lock keybind at `vigil-lock`. An example
`vigil.toml` ships at `/usr/share/vigil/vigil.toml.example`.

For safe UI development inside a running session, use `vigil-sim login`,
`vigil-sim lock`, or `vigil-sim warning`. The simulator has no PAM, logind,
greetd, DRM, power, or session-lock dependencies; its hamburger drawer selects
states and controls the injected warning clock. Its generated fake desktop is
blurred only to preview the compositor-owned frost effect.

For deterministic visual inspection, freeze a warning keyframe and expose its
machine-readable state:

```sh
vigil-sim warning --at-ms 3000 \
  --state-file /tmp/vigil-sim.json \
  --control-socket /tmp/vigil-sim.sock
```

`--at-ms` implies pause. F1/F2/F3 switch to login/lock/warning, Space pauses,
Right advances a paused warning by one second, B toggles simulated compositor
blur, and D opens the state drawer. The state file is rewritten only when its
contents change, so a frozen simulator has no polling or write loop.
The newline command socket accepts `state login|lock|warning`, `lock --wait`, `pause`,
`resume`, `advance MS`, `commit`, `cancel`, `hotplug`, `blur on|off|toggle`,
`export PATH.png`,
and `drawer open|close|toggle`; it replies only after the UI thread applies the
command. `lock --wait` mirrors the production readiness contract and replies
only after the simulated lock frame is presented. `export PATH.png` responds
only after the current rendered frame has been written. This is the stable automation interface for screenshots and agent
driven regression work, independent of the host compositor.

Headless scenario fixtures exercise the same theme, UI renderer, injected
warning clock, and control commands without opening a window or sleeping:

```sh
vigil-sim scenario tests/fixtures/sim/warning-commit.toml
```

The JSON result includes final mode/phase, the event trace, and a stable frame
hash. Optional `expect_mode` and `expect_phase` fields make a fixture fail fast
when its contract changes.

`vigil-lock --wait` detaches and returns success only after the compositor has
confirmed that the session is locked (`--daemonize` remains a compatibility
alias). `vigil-lock --warn 10` presents a cancelable, capture-free idle warning before
locking. It requests `ext-background-effect-v1` blur when available and uses a
tint-only fallback otherwise. `--no-warn`, manual lock, and suspend paths lock
immediately. Activity before commitment exits 3.

## Monitor layout

A greeter with no compositor has no layout to inherit: without help it lights
outputs up in DRM scan order at scale 1.0, which on a HiDPI desk means a login
card the size of a postage stamp on a monitor that may not even be the one you
are looking at.

vigil reads monitor layouts from `/etc/monitor-profiles/` — neutral TOML in the
[monitor-profiles](https://github.com/MasonRhodesDev/monitor-profiles) format,
the same directory the session manager
([hyprstate](https://github.com/MasonRhodesDev/hyprstate)) reads. One layout
definition, applied on both sides of login, so the screen you type your
password into is the screen your desktop appears on.

The package creates the directory as `2775 root:monitor-profiles` so layouts
are editable by group rather than by root:

```sh
sudo gpasswd -a "$USER" monitor-profiles   # re-login for it to take effect
```

No profiles installed is a supported state, not an error: vigil falls back to
scan order with a DPI-derived scale from the monitor's EDID. A profile that is
unreadable, unmatched, or nonsensical degrades the same way — a layout file
must never be what keeps someone from logging in.

## License

[GPL-3.0](LICENSE). Slint is used under its GPLv3 license option — see
[DESIGN.md §12](DESIGN.md).
