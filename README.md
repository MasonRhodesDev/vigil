# vigil

A multi-monitor, themeable [greetd](https://git.sr.ht/~kennylevinsen/greetd)
greeter that renders directly on KMS/DRM — no compositor in the login path.
One Rust binary takes the seat, drives every connected display at its
preferred mode, and draws an independent [Slint](https://slint.dev) scene on
each: per-output backgrounds, a login panel that follows the pointer, and
runtime `.slint` themes with a compiled-in fallback.

**Status: M1 code complete.** The full login works end-to-end in QEMU
(`tests/e2e/run.sh`): modeset on virtio-gpu, themed login card, keyboard
via libinput/xkb, wrong-password error with automatic retry, and a
successful `start_session` handoff. Architecture, interfaces, and
milestones are in [DESIGN.md](DESIGN.md); next up is the on-metal VT
test, then M2 (hotplug polish, runtime themes, session list).

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
