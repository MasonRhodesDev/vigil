# Vigil 0.3 metal acceptance (#46) + mixed-scale handoff validation (#40)

The final gates before tagging v0.3.0. Run on the real machine with recovery
access available (`loginctl unlock-session` from SSH or another VT — vigil-lock
honors logind unlock without auth). Build: `cargo build --release -p vigil-lock`.

Everything below the nested-suite line is what sway-headless CANNOT exercise;
protocol ordering, join, cancel, no-capture, and teardown invariants are
already gated by `tests/nested/run.sh` in CI.

## #40 — mixed-scale warning-to-lock handoff

Topology: three outputs including one fractional-scale (eDP 2560x1600@1.25).

```sh
./target/release/vigil-lock --wait --warn 10
```

- [ ] All outputs lock together: no desktop reveal, no black first frame.
- [ ] eDP stays 2560x1600@1.25 through the handoff (no drop to 2048x1280@1.00).
- [ ] Prepared background assets are cache hits (`background ready ... (cache hit)`
      in stderr; requires `lmtt wallpaper publish` to have run — check
      `/var/lib/appearance-profiles/users/$USER/bundle.toml` exists).
- [ ] `--wait` returns only after the compositor confirms the lock.
- [ ] Unlock restores input, cursor, focus, and output state.

## #46 — acceptance matrix

- [ ] Blur present: Hyprland advertises `ext_background_effect_manager_v1`
      (`hyprctl globals | grep background_effect` or check the warning shows
      real frost, not tint-only) and the warning frosts the live desktop
      without any capture bind.
- [ ] Capability loss: disable the blur global mid-warning if possible
      (compositor config reload) — warning must fall back to tint, not die.
- [ ] Real hypridle idle path: wait out the idle timeout → warning appears →
      cancel with input → session stays unlocked; wait it out again → locks.
- [ ] Lid-close / `before_sleep_cmd` while unlocked: suspend proceeds only
      after the lock is confirmed; mouse nudge during lid-close must NOT
      cancel (hypr-DE ships `--no-warn` on that path as of 0.2.15).
- [ ] Second invocation while locked: exits 0 immediately (join), no PAM
      noise in the journal (issue #36), `pgrep vigil-lock` shows ONE process.
- [ ] Unlock: `pgrep vigil-lock` empty within 5 s (issue #49 watchdog).
- [ ] Connector-level dock hotplug while locked: every output shows a lock
      surface promptly (DESIGN §12 inv. 4); undock while locked ditto.
- [ ] Static warning and locked idle CPU: near-zero when idle (event-driven
      rendering; no continuous render loop).
- [ ] Greeter after upgrade: reboot, greeter paints and accepts input
      (0.2.16's NeedsRedraw wedge is the regression to watch).
- [ ] Attach: versions (`vigil-lock --version` if available, package
      versions), output topology, journal excerpts, timings.

Record results in #46/#40 and close them; then tag v0.3.0 (release CI re-runs
the nested suite as `nested-gate` before packaging).
