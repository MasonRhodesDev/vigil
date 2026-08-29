# Cross-crate integration tests

- **Render-state matrix** — `crates/vigil/tests/theme_states.rs`: every auth
  state rendered headlessly with region-level assertions. Not pixel-perfect
  goldens on purpose: text rasterization differs across font stacks, so
  exact PNGs would be flaky until a font is embedded (M3). `golden/` is
  reserved for that.
- **Fake greetd** — in-crate tests in `vigil-auth` drive the full IPC
  conversation over an in-process socket.
- **e2e/** — the full login in QEMU: `tests/e2e/run.sh` boots the host
  kernel via virtme-ng with a virtio-gpu, runs the real `vigil` binary
  against `fake_greetd.py` under seatd, injects a wrong-then-right password
  via QMP `sendkey`, screendumps each stage, and passes on `VIGIL-EXIT:0`.
  Needs KVM; run locally (validated 2026-08-03), not in CI yet.
- **vkms CI job** — best-effort modeset/page-flip smoke on the runner
  kernel's vkms (never blocks; see ci.yml).

Per-crate unit tests live inside each crate.

## journald (`tests/journald/`)

Does a trace survive the transport it is designed for? Every other capture
redirects stderr to a file, which proves the records are well-formed and
proves nothing about journald - and journald is the reason the format
carries no timestamps of its own.

Borrows a headless sway for `ext-session-lock` and the host's own journald
for the transport, running the locker in a transient scope started from a
transient service: the topology `lock-cmd.sh` uses, so stderr is a real
journald stream rather than a pipe. The child gets a private
`XDG_RUNTIME_DIR`, so its logind calls cannot reach the seat you are
sitting at.

    cargo build -p vigil-lock && ./tests/journald/run.sh

Checks one entry per record, full field sets, `seq` contiguous from zero,
`__MONOTONIC_TIMESTAMP` agreeing with `seq` order, no dangling parents, one
`lock.session` carrying its outcome, and no journald suppression. Skips
cleanly without sway, wtype, systemd-run or a user manager.

Cancels the warning so the locker leaves through `span_lines::exit`; a
killed locker loses whatever is still open, which is #79 and not what this
measures.
