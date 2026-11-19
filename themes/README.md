# Themes

A vigil theme is one `.slint` file that exports a `Window` component.
vigil compiles it at start and falls back to `default/theme.slint`, which
is built into the binary, when the file fails to compile or breaks the
contract. Check a theme with `vigil --theme-check <path>`.

`kit/ui/` holds the shared tokens and controls that the default theme
imports.

## Required contract

`vigil-theme` rejects a theme that lacks any of these.

| Name | Type | Meaning |
|---|---|---|
| `output-name` | `in string` | Connector name of this output. |
| `panel-visible` | `in bool` | This output shows the sign-in card. |
| `background-source` | `in image` | Wallpaper. |
| `clock-text` | `in string` | Formatted clock. |
| `auth-state` | `in string` | `idle`, `prompting`, `busy`, or `error`. |
| `prompt-text` | `in string` | The question from PAM. |
| `prompt-is-secret` | `in bool` | Mask the answer. |
| `info-message`, `error-message` | `in string` | Messages from PAM. |
| `sessions` | `in [string]` | Session names. |
| `selected-session` | `in int` | Index into `sessions`. |
| `caps-lock` | `in bool` | Caps Lock is on. |
| `status-banner` | `in string` | A footer notice, such as a pending approval. |
| `submit(string)` | callback | Send the field's text. |
| `cancel()` | callback | Cancel the attempt. |
| `session-changed(int)` | callback | Pick a session. |
| `power-action(string)` | callback | Run `reboot` or `poweroff` now. |

## Optional properties

vigil sets these when the theme declares them and skips them when it
does not.

| Name | Type | Meaning |
|---|---|---|
| `users`, `selected-user`, `user-name`, `user-initial` | `in` | The user picker. |
| `user-changed(int)` | callback | Pick a user. |
| `power-visible` | `in bool` | Show Restart and Shut Down. vigil-lock sets it to false. |
| `keyboard-focus` | `in string` | The control with keyboard focus. |
| `key-hint` | `in string` | One line that names the keys for the focused control. |
| `confirm-power` | `in-out string` | `reboot` or `poweroff` while a power confirm is open, else empty. |
| `reveal` | `in-out bool` | Show the secret answer as text. |

## Keyboard focus

vigil owns keyboard focus. The theme only draws it. vigil reads the card
state from the theme's properties on every key, moves focus, and runs
the same callbacks that clicks run.

`keyboard-focus` is one of `user`, `field`, `reveal`, `session`,
`reboot`, `poweroff`, `cancel`, or `confirm`. It is empty while the
field has focus and nobody has pressed Tab yet. Draw a ring on the
control that matches it. The ring must show against the control and
against whatever is behind it. The default theme draws `FocusTones.line`
between two thin `FocusTones.edge` lines. They follow `Theme.fg` and
`Theme.page-bg`, which are at least 9:1 apart, so any neighbouring
colour is at least 3:1 from one of them. When a colour kit puts those
two under 9:1, vigil sets `FocusTones.fallback` and the ring turns
white between black lines. Export a `FocusTones` global with an
`in-out bool fallback` to get the same check.
`crates/vigil/tests/keyboard.rs` checks the ratio with and without a
kit.

Keep the ring and the hint out of layout. A ring that pushes its
neighbours, or a hint that wraps to a second line, moves the card on
every key. Show `error-message` and `info-message` in place of the hint
when either is set. PAM's messages must stay visible wherever focus is.

vigil sets `reveal` to false on a new prompt, on submit, on a failure,
when the panel hides, and after 10 seconds without a key or a click.
Bind the field's masking to `reveal` and keep no copy of it.

A theme that declares no `keyboard-focus` keeps the old behaviour.
Typing and Enter reach the field, and keys never reach other controls.
A theme that declares no `confirm-power` keeps Restart and Shut Down out
of the Tab order, so a key can never power off without a visible
confirm. A theme that declares no `reveal` gets no show/hide stop and no
Alt+F8.

Set `confirm-power` from a Restart or Shut Down click to ask first, as
the default theme does. Call `power-action(confirm-power)` from the
confirm button and then clear `confirm-power`. The confirm button can
appear under the pointer, so ignore clicks on it for 500 ms after it
appears. Otherwise the second click of a double click on Shut Down
powers off.
