# Cross-crate integration tests

Land in M1 (DESIGN.md §8):

- **golden/** — reference PNGs for the headless golden-image matrix
  (default theme x auth states x sizes), rendered via the software path.
- **fake greetd** — an in-process unix socket speaking `greetd_ipc` driving
  `vigil-auth` end-to-end.
- The vkms and QEMU virtio-gpu jobs run the real binary; see
  `.github/workflows/ci.yml`.

Per-crate unit tests live inside each crate.
