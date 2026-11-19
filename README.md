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
`XDG_SESSION_*` env. Next up is the on-metal VT test
(`tests/vt/run.sh`), then M2 (hotplug polish, multi-GPU, runtime
themes).

## The pair

vigil is designed as two surfaces of one product: the **greeter** (this
binary, on bare KMS at boot) and **vigil-lock**, a session lockscreen
speaking `ext-session-lock-v1` under your compositor — same theme file,
same contract, same auth seam, so login and lock are one visual identity.
Each works without the other.

## Why

Graphical Wayland greeters need a compositor, and running a desktop
compositor as the greeter means a routine DE upgrade can break login.
Kiosk compositors (cage) avoid that but can't do per-output scenes. vigil
is its own display backend, so the login path depends only on the kernel,
libseat, libinput, xkbcommon, and greetd — none of which a desktop upgrade
touches.

## Security tests

The greeter and the lock are attacked by tests that run in CI. Each one
can be run alone.

| What is attacked | Run it |
|---|---|
| `AuthMachine` against a scripted greetd, by generated message and input sequences | `cargo test -p vigil-auth --test attack` |
| The greetd reply decoder, the state file, the config overlay, the keyboard state and the theme loader, by fuzzing | `fuzz/run.sh 300` |
| The PAM conversation callback, by a hostile module under AddressSanitizer | `RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p vigil-pam --target x86_64-unknown-linux-gnu` |
| The pure Rust half of `vigil-pam`, under Miri | `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p vigil-pam` |
| A locked session: signals, memory starvation, input flood, unlock race, compositor death | `tests/nested/attack.sh` |

`VIGIL_ATTACK_CASES` sets how many sequences the `AuthMachine` attack
generates per greetd model (default 1500). A failure prints the
shortest sequence that breaks an invariant; add it to `pinned()` in
`crates/vigil-auth/tests/attack.rs` before fixing it.

`fuzz/run.sh [seconds] [target...]` needs `cargo install cargo-fuzz
--locked` and a toolchain that accepts `-Z` flags: a rustup nightly, or
a stable toolchain that ships the sanitizer runtimes. CI runs every
target for 30 seconds. Seeds live in `fuzz/seeds`; the working corpus in
`fuzz/corpus` and crashes in `fuzz/artifacts` are untracked. To keep a
crash, minimise it with `cargo fuzz tmin <target> <artifact>`, add it to
`fuzz/seeds/<target>` and write a regression test for it.

`tests/nested/attack.sh` needs sway, wtype and the debug builds of
`vigil-lock` and the `lock_probe` example. It signs in as a user that
does not exist, so it never counts a failure against a real account. Both
D-Bus addresses name a socket nobody listens on, so neither the lock
client nor a PAM module reaches the buses of the machine it runs on.

Not covered yet: a VT switch away from a locked session. Headless sway
has no seat session, so there is no VT to switch. The test needs sway on
a DRM device inside the QEMU guest that `tests/e2e` boots, with
`vigil-lock` holding the lock, `chvt` to a second VT and back, and a
framebuffer capture on each VT to show that the session contents never
reach the screen.

Not covered yet: a compositor that restarts in place. Headless sway
cannot restart without ending its session, so A9 shows only that the
lock client exits with its compositor and frees the singleton.

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
tint-only fallback otherwise. Activity before commitment exits 3. `--no-warn`,
manual lock, and suspend paths run a short **non-cancelable** frost transition
instead (`[lock.transition]`, default 400 ms; input is ignored and hotplug
commits) and never exit 3; unlock fades the wallpaper out over the frosted
desktop. `--immediate` skips both ramps. On Hyprland the blur strength follows
the ramp via `hyprland-surface-v1` opacity (`wp-alpha-modifier-v1` where a
compositor ties blur to surface alpha); elsewhere the tint ramps over a fixed
blur.

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

Monitors are matched by the description Hyprland gives them, so a
`desc:` selector copied from `hyprctl monitors` works on both sides of
login. vigil builds it from the EDID the same way (vendor names from
hwdata's `pnp.ids`).

No profiles installed is a supported state, not an error: vigil falls back to
scan order with a DPI-derived scale from the monitor's EDID. A profile that is
unreadable, unmatched, or nonsensical degrades the same way — a layout file
must never be what keeps someone from logging in.

## License

[GPL-3.0](LICENSE). Slint is used under its GPLv3 license option.
