# Computer Use API Reference

Every call is async. Targets are `element_index` integers from the latest AX
snapshot, or `(x, y)` tuples in window-screenshot coordinates. This is the
complete module surface; [safety.md](safety.md) governs *when* to act.

## Module functions

| Signature | Returns | Notes |
|---|---|---|
| `await get_state(emit: bool = True)` | `dict` | Grants, app inventory, allowlist, platform. Prints fix-it guidance when a macOS grant is missing. The first call per process also emits the `computer_use_session_started` telemetry event; `emit=False` skips it. Discovery calls do not raise `TRANSPORT_ERROR` off darwin: `get_state` reports `"platform": None` and `permissions_status` reports `unknown` grants — read those fields to detect a missing backend; the *action* calls are the ones that raise. |
| `await list_apps()` | `list[dict]` | One `{"id": bundle_id, "name": display, "running": bool}` record per app. Use it to resolve names and check `running` before binding. Unlike `get_state`/`permissions_status`, it *does* raise `TRANSPORT_ERROR` off darwin (reading the workspace needs the backend). |
| `await get_app(app: str \| dict)` | `App` | Binds by display name or bundle id; dicts: `{"bundle_id": ...}`, `{"path": ...}`, `{"name": ...}`. Returns the app with its first AX state already loaded (`app.state`). |
| `await permissions_status()` | `dict` | `{"accessibility": ..., "screen_recording": ..., "help": [lines]}`; each status is `ok`, `missing`, or `unknown`. On Wayland it also carries `"input"` (see Platforms). |

`get_state()` returns `{"apps": [...], "permissions": {...}, "allowlist":
{...}, "platform": "mac" | "wayland" | "linux" | None}`. `apps` carries the `list_apps()`
records; `permissions` mirrors `permissions_status()`.

`get_app` raises `APP_NOT_ALLOWED` (allowlist gate), `PERMISSIONS_NOT_GRANTED`,
`APP_NOT_RUNNING`, `AMBIGUOUS_APP`, `APP_LAUNCH_FAILED`, and `SCREEN_LOCKED`.
Binding an installed app that is not running launches it in the background
(`open -g`): the app binds and observes without taking over the screen, and
only an explicit `App.activate()` brings it forward. A failed start raises
`APP_LAUNCH_FAILED`.

## App object

Properties: `bundle_id` (`str`), `name` (`str`), `state` (`str | None` — the
last observed AX text).

| Signature | Returns | Notes |
|---|---|---|
| `await get_ax_state(diff: bool = True)` | `str` | Element-indexed AX text. `diff=True` shows only what changed since the previous snapshot; `diff=False` returns the full tree. Indices on current entries refer to the latest snapshot; indices shown on removed (`-`) entries are from the previous snapshot and are informational only, not valid action targets. |
| `await get_screenshot(attach: bool = True)` | `dict` | `{"path", "width", "height"}` — the PNG's own pixel dimensions, which on a Retina capture are 2x the window's logical bounds; `click`/`drag`/`scroll` scale the screenshot's pixels back to the window automatically. `attach=True` loads the image into your context (attach failures are swallowed; the dict is still returned); `attach=False` skips attaching. |
| `await get_state_and_screenshot(diff: bool = True, attach: bool = True)` | `dict` | One snapshot combining the two calls above: `{"state": ..., "screenshot": ...}`. If the screenshot capture fails, the error is swallowed and `screenshot` is `None`. |
| `await get_text_regions()` | `list[dict]` | Window-scoped OCR over the focused window: a normalized list of text regions with window-relative pixel coordinates — the same space as screenshot `(x, y)` targets, so a region can be clicked directly. The screen-reading path for models that cannot see images (see Non-vision models in SKILL.md); also reads image and canvas text the AX tree cannot expose. |
| `await click(target, button: str = "left", count: int = 1)` | `None` | `target` is an `element_index` or an `(x, y)` screenshot-coordinate tuple. `button`: `"left"` (default), `"right"`, `"middle"`; `count=2` double-clicks. |
| `await drag(from_, to)` | `None` | Two `(x, y)` screenshot-coordinate points. |
| `await scroll(target, direction: str, pages: int = 1)` | `None` | `direction` is one of `up`, `down`, `left`, `right`; `target` is an index or `(x, y)`. |
| `await press_key(key: str)` | `None` | Chord names: `"cmd+shift+f"`, `"Return"`, `"super+c"`. |
| `await type_text(text: str)` | `None` | Literal keystrokes; every newline presses Return. See typing hazards in SKILL.md. |
| `await set_value(element_index: int, value: str)` | `None` | Sets an editable element's value in one call; multiline-safe. Non-editable or secure elements raise `ACTION_UNSUPPORTED`. |
| `await select_text(element_index: int, text: str, prefix: str \| None = None, suffix: str \| None = None)` | `None` | Selects an exact run of `text` inside the element. `prefix` and `suffix` disambiguate repeated occurrences. |
| `await perform_secondary_action(element_index: int, action: str)` | `None` | Runs an AX action the element exposes. The element's available `actions` appear in its AX text — never guess a name. |
| `await paste(text: str, format: str = "text")` | `None` | Writes through the clipboard in `text`, `md`, or `html` form — only `html` writes rich data, the others paste plain text. The previous clipboard content is restored when the pasteboard still holds the payload; a copy made during the paste window is kept. |
| `await activate()` | `None` | Brings the app's frontmost window to the foreground and makes the app key. Keystrokes always reach the bound app's process, but app-scoped shortcuts (menus, quick switchers, composer keys) only fire while the app is key — call this before any shortcut-driven flow. Never fake focus from bash with `open -a`, osascript, or `screencapture`. |
| `is_frontmost()` | `bool` | Whether the app is the frontmost (key) application. Synchronous, no `await`. The pre-flight check before a shortcut flow: when it returns `True`, app-scoped shortcuts fire without `activate()`. |

Every `App` action can raise `ELEMENT_STALE`, `ACTION_UNSUPPORTED`,
`INVALID_ARGUMENT`, `INJECTION_FAILED`, `TRANSPORT_ERROR`, `SCREEN_LOCKED`,
`APP_NOT_ALLOWED` (the gate is re-checked on every action), and
`PERMISSIONS_NOT_GRANTED` (the Accessibility grant is re-checked on every
action, so a grant revoked mid-session reports itself instead of surfacing
as an injection failure).

Actions that inject input or perform an AX action (`click`, `drag`,
`scroll`, `press_key`, `type_text`, `paste`, `perform_secondary_action`)
wait for the app to settle before returning — a bounded poll of the focused
window's live fingerprint — so the next `get_ax_state()` shows the settled
UI. `set_value` and `select_text` are synchronous AX attribute writes and
return once the write has completed.

Web views (Electron apps) often omit per-element geometry; an element click
there raises `ACTION_UNSUPPORTED`. Switch to keyboard navigation or `(x, y)`
window-screenshot coordinates — `get_text_regions()` supplies those
coordinates for models that cannot see screenshots.

## AX text

`get_ax_state` walks the focused window's AX tree. Each element reports its
`element_index` plus `role`, `subrole`, `title`, `value`, `description`,
`placeholder`, `actions`, `position`, and `size`. The walk is bounded
(1500 elements, depth 12, 3 seconds); a bounded-away walk is reported in the
text as `— TRUNCATED: the observation stopped at its element/depth/time
bounds, some controls are hidden`, so a missing control is never confused
with one the bounds cut off — re-observe after narrowing the app's view (a
dialog, a sidebar collapse) when a target is not shown. Use the text fields to
identify the element and the `element_index` to target it. Secure fields
(`AXTextField` with subrole `AXSecureTextField`) refuse typing and
`set_value` with `ACTION_UNSUPPORTED` and a hand-off message; ask the user to
enter credentials themselves.

## Error codes

Every failure raises `computer_use.errors.ComputerUseError` with a `code`:

| Code | Meaning | Recovery |
|---|---|---|
| `APP_NOT_ALLOWED` | App absent from the allowlist, blocked, or system-denied — the reason text says which. | Absent: tell the user to add the bundle id to `apps.allowed` in `~/.prime/agent/settings/computer-use.toml` (never edit it yourself). Blocked: adding it to `apps.allowed` does NOT help — the user must remove it from `apps.blocked` first. System-denied (loginwindow, screensaver, OS-auth dialogs): always refused, there is no user override. |
| `PERMISSIONS_NOT_GRANTED` | A required macOS grant is missing, or the Accessibility grant was revoked mid-session. | Run `permissions_status()`, relay the `help` lines, wait for the user to grant (see [permissions.md](permissions.md)); a mid-session revoke needs the user to re-grant and Prime Agent restarted, then re-bind with `get_app`. |
| `PERMISSIONS_PENDING` | A grant is mid-flight. | Ask the user to finish granting, then re-check with `permissions_status()`. |
| `SCREEN_LOCKED` | The screen is locked. | Stop and ask the user to unlock — the skill never unlocks. After unlocking, re-observe; indices are stale. |
| `USER_STOPPED` | The user stopped or intervened. | Stop the task and ask how to proceed; do not immediately retry. |
| `ELEMENT_STALE` | The index belongs to an old snapshot. | Call `get_ax_state()` and act on fresh indices; never retry the same index. |
| `AMBIGUOUS_APP` | The name matches several apps. | Call `list_apps()` and bind by the exact bundle id. |
| `APP_NOT_RUNNING` | The target is not running and could not be attached. | Check the `running` flag via `list_apps()`; have the user start the app, or bind by bundle id or path. |
| `APP_LAUNCH_FAILED` | The start attempt failed. | Verify the app name or path with the user; start the app manually and bind again. |
| `ACTION_UNSUPPORTED` | The element or backend does not support the action (non-editable `set_value`, secure field, unlisted secondary action). | Read the element's `role` and `actions` in the AX text; use an alternative control path; secure fields: hand off to the user. |
| `INJECTION_FAILED` | Posting the input event failed. | Re-observe, then retry once with fresh targeting; if it repeats, report it. |
| `TRANSPORT_ERROR` | Backend or transport failure — including no backend on this platform. | Confirm the platform is supported; re-observe; report persistent failures to the user. |
| `INVALID_ARGUMENT` | Malformed argument: bad chord, unknown direction, bad coordinates, wrong index type. | Fix the call to match the signatures above. |

## Policy files

The skill's on-disk paths — settings, approvals, and screenshot tmp files —
all derive from the agent state dir: `~/.prime/agent` by default, or the dir
named by the `PRIME_AGENT_CODING_AGENT_DIR` environment variable when set.

- Settings: `~/.prime/agent/settings/computer-use.toml` — `apps = {allowed =
  [...], blocked = [...]}`, `system_deny = [bundle ids]`, and `risk =
  {"com.apple.Safari" = "low"}` — quote every dotted bundle-id key (unquoted,
  TOML nests them silently); values are `low`, `medium`, `high`. Blocked and
  system-denied apps are hard refusals; anything not in `allowed` raises
  `APP_NOT_ALLOWED` with instructions for the user. The file is user-edited
  only; the skill never writes settings.
- Approvals: `~/.prime/agent/state/computer-use/approvals.json` — reserved
  for the follow-on approval surface (the elicitation lane); v1 neither
  reads nor writes it, so the allowlist file is the only active policy
  surface today.
- Lock check: actions consult the login-session state. A locked screen fails
  closed with `SCREEN_LOCKED`; a session state that cannot be read is
  treated as locked, so binding and input injection never proceed on an
  unverifiable desktop.

## Telemetry

Emission is best-effort and never raises; properties are primitives only.

- `computer_use_session_started` — `{platform}`, once per process on the
  first `get_state()` call (`emit=True`).
- `computer_use_action` — `{action, outcome, duration_ms}` per action.
  `action` is one of `click`, `drag`, `scroll`, `press_key`, `type_text`,
  `set_value`, `select_text`, `secondary`, `paste`, `activate`, `get_state`;
  `outcome` is `ok` or `error`, and failures carry their frozen code in the separate
  `error_code` property (for example `error_code: "INJECTION_FAILED"`).

## Platforms

`backend()` resolves, in order: `"mac"` on macOS; `"wayland"` under a niri
session (`WAYLAND_DISPLAY` set and `NIRI_SOCKET` naming a live socket);
`"linux"` (X11) when `xdotool` is on PATH; otherwise `None`, in which case
API calls raise `TRANSPORT_ERROR` ("computer use backend unavailable:
\<reason\>"). The Wayland check runs before the X11 one because a niri
session usually also exports an XWayland `DISPLAY` that sees only XWayland
clients.

### Wayland (niri)

| Piece | Source | Notes |
|---|---|---|
| Apps and windows | niri IPC (`NIRI_SOCKET`, JSON lines) | The app identity is the Wayland `app_id` (the allowlist key); `App.pid` carries niri's window id. Binding picks the focused, else most recently focused, window of the app. No launch: a spec without a window raises `APP_NOT_RUNNING`. Every action re-checks that the window still exists with the bound `app_id`. |
| AX text | AT-SPI (libatspi via PyGObject) | The app is found on the accessibility bus by the window's pid, its frame by the window title. Elements carry the AT-SPI role name (`push button`, `entry`, ...), `title` (name), `value` (text or numeric value), `description`, `actions` (AT-SPI action names), and WINDOW-relative `position`/`size`. Only showing elements are walked (same 1500/12/3 s bounds). |
| Secure fields | AT-SPI `ROLE_PASSWORD_TEXT` | Rendered as role `password text` with `[secure]`; their value is never read. `type_text`/`press_key` read the live focus and refuse a password field, and refuse when the focus cannot be verified (app not on the bus, search bounds hit) — the macOS fail-closed rule. |
| Element actions | AT-SPI | `click(i)` (left, single) runs the element's `click`/`press`/`activate`/`jump`/`toggle`/`open` action, except on a text or password field, where it focuses the field like a real click: AT-SPI GrabFocus where the toolkit has it, otherwise (GTK 4) a real pointer click at the field's center, which needs a floating window; the field must report focus or the click fails with INJECTION_FAILED. `set_value` uses EditableText; `select_text` uses Text selections; `perform_secondary_action` runs any listed action. Apart from the field focus, none of these move focus. After `get_screenshot()`, `(x, y)` points are screenshot pixels. |
| Keyboard | `zwp_virtual_keyboard_v1` | Focus-bound: the window is focused through niri first and the input is refused (`INJECTION_FAILED`) if niri does not report it focused. Text is typed with an uploaded keymap holding one keysym per character, so it does not depend on the user's layout. `cmd` maps to Super. |
| Pointer | `zwlr_virtual_pointer_v1` | Pixel-exact in logical coordinates, mapped onto the window's output; focus as above. Needs the window's screen position, which niri reports only for floating windows on an active workspace; tiled windows raise `ACTION_UNSUPPORTED`. |
| Screenshots | `grim -g` (wlr-screencopy) | Captures the window's logical rect into the same hardened directory; the PNG is at the output scale (2x on a 2x output) and `(x, y)` targets scale back automatically. Refused for tiled windows and when another floating window overlaps an unfocused bound window. |
| Locked screen | logind `LockedHint` + `Active` (via `loginctl`) | niri maintains `LockedHint`; an unreadable or inactive session counts as locked. |

`permissions_status()` on Wayland returns `{"accessibility", "screen_recording",
"input": {"pointer", "keyboard"}, "help"}` — AT-SPI, grim, and the two
virtual-input managers, each `ok`, `missing`, or `unknown`.

Setup: the kernel bootstrap installs PyGObject with this skill on Linux (it
builds from source, so the system needs the gobject-introspection and cairo
development headers; without them the skill fails to install and the
bootstrap warns). The host also needs the Atspi 2.0 typelib and at-spi2-core
running, `grim` on PATH, and apps
exposing AT-SPI (Firefox: accessibility enabled; Chromium/Electron:
`--force-renderer-accessibility`). The virtual-input protocols need no
setup: niri offers them to every client outside a sandboxed security context.

Known gaps on Wayland:

- Coordinate input and screenshots need a floating window: niri's IPC does
  not expose the scrolling layout's view offset, so a tiled window's screen
  position is unknown.
- Input is focus-bound, not window-targeted: there is a short race between
  the focus check and delivery if the user changes focus in between.
- Pointer clicks land on whatever surface is topmost at the point
  (layer-shell bars and notifications are not checked); screenshots of the
  rect include such overlays.
- AT-SPI WINDOW coordinates of client-side-decorated apps can be offset by
  their shadow margins relative to niri's window geometry.
- `paste` (clipboard transaction) and `get_text_regions` (OCR) are
  macOS-only; ydotool is deliberately not used (its socket lets any process
  type as the user, its absolute motion is not pixel-accurate, and its
  typing is US-ASCII only).
