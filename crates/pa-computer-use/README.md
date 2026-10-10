# pa-computer-use

The host side of the bundled `computer-use` skill: the allowlist gate, permission reports, observation, diffing,
input, capture and the secure-field rules, behind the `computer_use.*` kernel host requests (registered per session
by `pa-core`'s `session_engine::computer_use_host`). The skill's Python package (`skills/computer-use`) is a thin
client: it checks the Python-typed arguments, forwards each call, raises `{"error": ...}` replies as
`ComputerUseError`, and attaches screenshots.

## Layout

- `host`: backend detection (macOS on darwin; on Linux niri Wayland under a live `NIRI_SOCKET`, else X11 when
  `xdotool` is on `PATH`), per-request routing, the wire format (`REQUEST_TYPES`, `{"ok"}` / `{"error"}`).
- `session`: the platform-independent `App` behaviour over the `platform::Platform` seam: binding and the guard,
  element freshness, coordinate mapping, the settle poll, paste, OCR regions, telemetry. `session::fake` is the test
  double below the seam.
- `policy` (allowlist, deny-lists, risk labels), `permissions`, `secure` (typed `Security` verdicts and the
  refusals), `render` (element-indexed text and its difflib-exact diff), `keymap`, `capture` (the hardened
  screenshot directory), `pyfmt` (Python `repr`/`round`/`casefold` text forms the model reads).
- `platform::x11`: `xwininfo`/`xdotool`/`maim`|`scrot` subprocesses.
- `platform::wayland`: niri IPC (plus the computer-use niri fork's `WindowGeometry`, `CaptureWindow` and
  `WindowAt`, each falling back when niri refuses it as unknown), AT-SPI over `zbus`/`atspi-proxies`, the wlr
  virtual pointer and the virtual keyboard over `wayland-client`, `grim` (screenshots on upstream niri).
- `platform::mac`: the AX walk and reads (`mac::ax`, over a raw AX seam), `CGEvent` sequences (`mac::events`),
  `NSWorkspace`/Spotlight/`open`/`screencapture` flows; `mac::sys` is the objc2 FFI — the crate's only `unsafe`
  module (`unsafe_code = "deny"` everywhere else), every block with a `SAFETY:` comment.

## Testing

Every backend is tested over test doubles below its OS seam (scripted subprocesses, a fake niri, a fake AT-SPI
tree, an in-process fake Wayland compositor, a scripted AX tree and a recording desktop); no test touches a live
desktop. `mac::sys` is compile- and clippy-checked for `aarch64-apple-darwin` but cannot run off macOS. The
end-to-end client path (a real kernel, the Python client, these host requests) is
`crates/pa-core/tests/computer_use_kernel.rs`, with every backend hidden from detection.
