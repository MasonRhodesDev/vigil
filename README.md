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

## License

[GPL-3.0](LICENSE). Slint is used under its GPLv3 license option — see
[DESIGN.md §12](DESIGN.md).
