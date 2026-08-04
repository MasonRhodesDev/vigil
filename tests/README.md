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
