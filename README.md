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
keyboard and pointer session picker over the installed
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
generates per greetd model (default 1500). Each run draws a new seed and
prints it first. `VIGIL_ATTACK_SEED=<seed>` with the same case count
draws the same sequences again, so a red CI run replays locally. A
failure prints the shortest sequence that breaks an invariant; add it to
`pinned()` in `crates/vigil-auth/tests/attack.rs` before fixing it.

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
client nor a PAM module reaches the host's D-Bus. The run is otherwise
not sealed off the host: NSS looks the ghost user up through
`/run/systemd/userdb`, and PAM modules write to `/dev/log`. The script
kills only lockers that carry its own `VIGIL_ATTACK_RUN`, so a locker the
user runs from the same build survives it.

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
alias). It exits 1 when a logind unlock lands before the compositor confirms
the lock, and also when the compositor invalidates the lock, so a caller told
the session is locked never sees it unlock first. `vigil-lock --warn 10`
presents a cancelable, capture-free idle warning before locking. It requests `ext-background-effect-v1` blur when available and uses a
tint-only fallback otherwise. Activity before commitment exits 3. `--no-warn`,
manual lock, and suspend paths run a short **non-cancelable** frost transition
instead (`[lock.transition]`, default 400 ms; input is ignored and hotplug
commits) and never exit 3; unlock fades the wallpaper out over the frosted
desktop. `--immediate` skips both ramps. On Hyprland the blur strength follows
the ramp via `hyprland-surface-v1` opacity (`wp-alpha-modifier-v1` where a
compositor ties blur to surface alpha); elsewhere the tint ramps over a fixed
blur.

## Keyboard

Every greeter control works without a pointer. The password or username
field has focus at start, and typing always goes there.

| Key | Where | Does |
|---|---|---|
| Tab, Shift+Tab | anywhere | Move focus through the user picker, the field, show/hide, the session picker, Restart, and Shut Down, in that order, and wrap. |
| Enter, keypad Enter | field | Submit. Enter with nothing typed never answers a hidden question such as a password. |
| Ctrl+A | field | Select the text. Other Ctrl shortcuts do nothing, so Undo can never bring back text the field cleared. |
| Esc | field | Cancel the attempt and start over. |
| Esc | any other control | Return focus to the field. |
| Left, Right, Up, Down, Space | user or session picker | Pick the previous or next entry. |
| Enter | user or session picker | Return focus to the field. |
| Space | show/hide | Show or hide the password. |
| Enter | show/hide | Submit, as in the field. |
| Alt+F8 | anywhere | Show or hide the password. |
| Space, Enter | Restart, Shut Down | Ask to confirm. Focus starts on Cancel. |
| Tab, Left, Right | confirm | Move between Cancel and the action. |
| Space, Enter | confirm | Run the focused choice. |
| Esc | confirm | Close it without acting. Focus returns to the button that opened it, or to the field if a click opened it. |
| a letter or digit | any control but a confirm | Return focus to the field and type there. |
| Ctrl+Alt+F*n* | anywhere | Switch to VT *n*. |

A line under the field names the keys for the focused control. A
two-tone ring marks the focused control once you press Tab. Arrow keys
inside the field keep editing the text, so they never switch users.
Picking a user or a session by keyboard is the same local change as a
click. No PAM conversation opens until you press Enter in the field,
with one exception. Picking a user whose last sign-in asked no question
opens their sign-in at once, by key or by click. If PAM again asks
nothing, that user is signed in.

Enter, keypad Enter and Esc act once per press. Holding a key on a
control acts once, so key repeat never cycles through users and never
flips show/hide. Only Tab, Shift+Tab and typing repeat.

A shown password hides again on a new question, on submit, on a failed
attempt, when the card moves to another screen or after a VT switch,
and after 10 seconds without a key or a click. Ten seconds is enough to
read back what you typed, and short enough that a password does not
stay on a screen someone walked away from.

PAM's messages, such as a failed attempt or a request to touch a
security key, stay on the line under the field while focus is on
another control. The key hint shows only when there is no message. A
PAM info message, such as a request to touch a security key, also shows
in place of "Checking…" while the answer is checked.

A pointer click on Restart or Shut Down asks to confirm too. The confirm
ignores clicks for half a second after it opens, so a double click on
Shut Down never shuts down. vigil-lock has the same field and show/hide
keys and shows no power buttons.

### Known limits

- Enter with nothing typed never answers a hidden question. A PAM module
  that takes an empty hidden answer cannot get one. An optional second
  factor that you skip with an empty Enter cannot be skipped, and an
  account with an empty password cannot sign in.
- The field is not cleared when you stop typing. A half-typed password
  stays in the field, masked, until a submit, an Esc, or a question with
  new text. A retry typed while vigil-lock checks an attempt stays after
  the failure, hidden.
- With Ctrl held, only the `a` keysym passes, so a non-Latin layout gets
  no Select All.

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

## Safety gates

vigil handles passwords, so CI rejects unsafe patterns instead of trusting
review to catch them. `tests/check.sh` runs the checks of CI's check job in
the same order, so a failure shows up before a push.

- **Fence.** `python3 -I tests/gates/gate.py fence` reads every file under
  `crates/` and counts, per file, the word `unsafe` (comments included), the
  word `expose`, and each `allow`, `expect` or `warn` that names an unsafe
  lint, a clippy lint, `warnings`, or a rustc lint group that holds a denied
  lint, such as `rust_2024_compatibility`. `tests/gates/baseline.tsv` records
  those counts per file. A count above its row, or a file with no row, fails.
  After removing some, run `python3 -I tests/gates/gate.py fence --update`
  and commit the smaller baseline. Growing a row takes `--update
  --allow-growth`, and the pull request must say why.
- **Rejected outright.** `include!`, `#[path]`, a build script or proc macro
  in any path package, a path dependency that is not a workspace member under
  `crates/` (a `[patch]` to a path included), a `cfg` on `clippy`, `unsafe`
  inside `macro_rules!`, a symlink under `crates/`, any `clippy.toml`,
  `.cargo/` or `rust-toolchain` file except the root `clippy.toml`, a crate
  that does not take `[lints] workspace = true`, a workspace lint below
  `deny`, `unsafe_op_in_unsafe_fn` below `forbid`, and a binary whose `main`
  does not begin with `vigil_secret::exit_on_panic()`. Line 1 of every crate
  root, examples and tests included, is `#![forbid(unsafe_code)]`, or
  `#![deny(unsafe_code)]` when its module tree holds fenced unsafe code.
- **New dependencies.** The fence lists every registry or git package in
  `Cargo.lock` that the base branch's lock does not have, and marks new
  crates, build scripts and proc macros. It reports them for the reviewer and
  does not fail on them.
- **What the fence cannot see.** It counts per file, so removing one unsafe
  block and adding another in the same file keeps the count. A credential
  that never enters a `Secret` never meets the `expose` ban. Code under
  another target's `cfg`, such as `cfg(target_os = "freebsd")`, is counted
  but never linted, since CI builds only for Linux. Code a dependency's macro
  generates is not read.
- **Canary.** `crates/vigil-secret/examples/lint_canary.rs` breaks every
  unsafe and plaintext rule. `gate.py canary` runs clippy on it and fails
  unless each lint still rejects it, so drift in the lint configuration fails
  loudly.
- **Credentials.** Typed text travels as `vigil_secret::Secret`, which wipes
  itself on drop and never prints. `clippy.toml` disallows `Secret::expose`,
  and `crates/vigil-secret/tests/api.rs` fails to compile if `Secret` gains a
  trait that hands out its text. The few adapters that hand plaintext to
  greetd or PAM carry `#[expect(clippy::disallowed_methods)]`.
- **Process.** Every binary's `main` first installs
  `vigil_secret::exit_on_panic()`, then sets `RLIMIT_CORE` to 0 and clears
  `PR_SET_DUMPABLE`, which keeps its memory out of core dumps and away from
  same-user ptrace. A panic on any thread ends the process at once with
  status 101, so a dead worker cannot leave a live locker that no longer
  authenticates, and the session lock stays held. The report names only the
  file and line, since a panic message can quote typed text. Work wrapped in
  `vigil_secret::cosmetic`, such as a wallpaper render, loses only its result
  when it panics.
- **Dependencies.** `gate.py advisories` parses `deny.toml` and requires every
  advisory ignore to be a table with an `id` and a reason that starts
  `expires YYYY-MM-DD:`, at most 183 days out.
- **Who judges.** On a pull request, CI runs the base branch's copy of the
  gates against the pull request, then the pull request's own copy. A change
  to a gate, the lint configuration, `deny.toml`, the `Secret` crate or a
  baseline row is an ordinary edit. The pull request must justify it, and it
  passes the usual independent review.

## License

[GPL-3.0](LICENSE). Slint is used under its GPLv3 license option.
