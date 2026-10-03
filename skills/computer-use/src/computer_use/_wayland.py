"""Wayland backend for the niri compositor: niri IPC, AT-SPI, virtual input, grim.

This module is the Wayland half of the computer-use backend seams, selected by
_compat._backend() when WAYLAND_DISPLAY is set and the compositor is niri
(NIRI_SOCKET names a live socket). It mirrors the seams the App layer consumes
from the mac modules: observation and element operations carry the same
names as computer_use.ax (_observe, _live_fingerprint, _focused_is_secure,
_live_is_secure, _perform_action, _is_settable, _set_value, _current_value,
_select_text_range, _window_fingerprint), input mirrors the X11 backend
(_click, _drag, _scroll, _press_key, _type_text), and discovery mirrors
apps (_list_apps, _resolve_app). Every name is underscore-private; only the
App layer's dispatch (through _compat._require_wayland) reaches it.

Sources:

- Windows come from niri's IPC socket (JSON lines on NIRI_SOCKET): the app
  identity is the Wayland app_id (the bundle-id analogue the allowlist gates),
  the bound id is niri's window id, and the pid is niri's. niri exposes a
  window's on-screen position only for FLOATING windows
  (layout.tile_pos_in_workspace_view is None for tiled windows - the scrolling
  view offset is not in the IPC), so the absolute window rect is derivable
  only for a floating window on an active workspace: output logical origin +
  tile position + window offset in tile, window_size for the size. For every
  other window window_rect is None and coordinate input and screenshots are
  refused with ACTION_UNSUPPORTED naming the gap.
- Observation, focus, and secure-field detection come from AT-SPI (libatspi
  through PyGObject). The application is found on the a11y bus by the niri
  window's pid (niri's own AccessKit tree belongs to the compositor's pid and
  never matches an app window), the window's frame by its title. Element
  positions are WINDOW-relative logical coordinates (AT-SPI's WINDOW coord
  type): Wayland clients cannot know their global position. A password field
  (ROLE_PASSWORD_TEXT) renders as role "password text", is marked [secure],
  and its value is never read.
- Input: element-targeted actions run through AT-SPI (Action.do_action,
  EditableText.set_text_contents, Text selections) and work without focus.
  Pointer and keyboard input go through the compositor's virtual-input
  protocols (computer_use._wlinput): they are focus-bound, so the bound window
  is focused through niri IPC first and the input is refused when focus did
  not land. ydotool is deliberately not used (its socket lets anything type as
  the user, its absolute motion is not pixel-accurate, its typing is US-ASCII).
- Screenshots: grim over wlr-screencopy with the computed logical rect; niri's
  screenshot-window action is not used because it always copies the image to
  the user's clipboard.

Errors follow the shared taxonomy: TRANSPORT_ERROR when the backend itself is
broken (no niri socket, no AT-SPI bindings, failed grim), APP_NOT_RUNNING when
the bound window is gone, ACTION_UNSUPPORTED for a platform gap, INJECTION_FAILED
when focus did not land or the compositor rejected input.
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, NamedTuple
from uuid import uuid4

from . import _wlinput, capture
from .ax import _ATSPI_SECURE_ROLE, Observation, _cap
from .errors import ComputerUseError
from .keymap import ParsedChord, _parse_chord

_ERROR_LIMIT = 200
_TIMEOUT_SECONDS = 10.0
_NIRI_TIMEOUT_SECONDS = 2.0
_NIRI_REPLY_LIMIT = 8 * 1024 * 1024
_MAX_DEPTH = 12
_MAX_ELEMENTS = 1500
_MAX_OBSERVE_SECONDS = 3.0
_MAX_FOCUS_SECONDS = 1.0
_FINGERPRINT_FOCUS_SECONDS = 0.25  # inside the App's 0.5 s settle budget
_MAX_ACTIONS = 16
_FINGERPRINT_VALUE_CHARS = 200
_ATSPI_TIMEOUT_MS = 1500
_ATSPI_STARTUP_TIMEOUT_MS = 5000
_FOCUS_POLL_SECONDS = 0.02
_FOCUS_WAIT_SECONDS = 0.5
_WHEEL_CLICKS_PER_PAGE = 10

_TOOL_PATHS: dict[str, str] = {"grim": "/usr/bin/grim", "loginctl": "/usr/bin/loginctl"}

_PRESS_ACTIONS = ("click", "press", "activate", "jump", "toggle", "open")

_MODIFIER_ORDER = ("ctrl", "alt", "shift", "cmd")
_KEYSYMS: dict[str, str] = {
    "Return": "Return",
    "Tab": "Tab",
    "Escape": "Escape",
    "Space": "space",
    "Delete": "BackSpace",
    "ForwardDelete": "Delete",
    "Home": "Home",
    "End": "End",
    "PageUp": "Prior",
    "PageDown": "Next",
    "Up": "Up",
    "Down": "Down",
    "Left": "Left",
    "Right": "Right",
}
for _index in range(1, 13):
    _KEYSYMS[f"F{_index}"] = f"F{_index}"

_ATSPI_LOCK = threading.RLock()  # libatspi is not thread-safe; App dispatch runs in worker threads
_atspi_module: Any = None


class _Geometry(NamedTuple):
    """One niri window's derivable geometry, in logical pixels.

    x and y are the window's absolute origin when niri exposes it (floating
    windows on an active workspace), else None; reason names why not.
    """

    x: float | None
    y: float | None
    width: float
    height: float
    output: str | None
    output_rect: tuple[float, float, float, float] | None
    reason: str


# --- niri IPC -----------------------------------------------------------------


def _niri_socket_path() -> str:
    """Return the niri IPC socket path, refusing without one."""
    path = os.environ.get("NIRI_SOCKET") or ""
    if not path:
        raise ComputerUseError(
            "TRANSPORT_ERROR",
            "computer use backend unavailable: NIRI_SOCKET is not set; the Wayland backend needs niri",
        )
    return path


def _niri_transport(line: bytes) -> bytes:
    """Send one JSON request line to niri and return its one reply line."""
    path = _niri_socket_path()
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(_NIRI_TIMEOUT_SECONDS)
    try:
        sock.connect(path)
        sock.sendall(line)
        chunks: list[bytes] = []
        size = 0
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            chunks.append(chunk)
            size += len(chunk)
            if b"\n" in chunk or size > _NIRI_REPLY_LIMIT:
                break
        return b"".join(chunks)
    except OSError as error:
        raise ComputerUseError(
            "TRANSPORT_ERROR", f"niri IPC failed: {str(error)[:_ERROR_LIMIT]}"
        ) from error
    finally:
        sock.close()


def _niri(request: Any) -> Any:
    """Run one niri IPC request and unwrap its {"Ok": ...} reply."""
    raw = _niri_transport((json.dumps(request) + "\n").encode("utf-8"))
    try:
        reply = json.loads(raw.split(b"\n", 1)[0].decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as error:
        raise ComputerUseError("TRANSPORT_ERROR", "niri IPC returned an unreadable reply") from error
    if isinstance(reply, dict) and "Err" in reply:
        raise ComputerUseError("TRANSPORT_ERROR", f"niri IPC refused the request: {str(reply['Err'])[:_ERROR_LIMIT]}")
    if not isinstance(reply, dict) or "Ok" not in reply:
        raise ComputerUseError("TRANSPORT_ERROR", "niri IPC returned an unexpected reply")
    return reply["Ok"]


def _response(request: str) -> Any:
    """Run one unit request ("Windows", ...) and return its payload."""
    ok = _niri(request)
    if not isinstance(ok, dict) or request not in ok:
        raise ComputerUseError("TRANSPORT_ERROR", f"niri IPC returned no {request} payload")
    return ok[request]


def _windows() -> list[dict[str, Any]]:
    """Read every niri window record."""
    windows = _response("Windows")
    return [window for window in windows if isinstance(window, dict)] if isinstance(windows, list) else []


def _window(window_id: int) -> dict[str, Any] | None:
    """Read one niri window record by id, or None when it is gone."""
    return next((window for window in _windows() if window.get("id") == window_id), None)


def _focused_window_id() -> int | None:
    """Read niri's focused window id, or None when nothing is focused."""
    focused = _response("FocusedWindow")
    return focused.get("id") if isinstance(focused, dict) else None


def _window_id(value: int) -> int:
    """Validate one niri window id as an integer."""
    if not isinstance(value, int) or isinstance(value, bool):
        raise ComputerUseError(
            "INVALID_ARGUMENT",
            f"window id must be an integer, got {type(value).__name__}",
            {"window_id": type(value).__name__},
        )
    return value


def _require_window(window_id: int) -> dict[str, Any]:
    """Read the bound window, raising APP_NOT_RUNNING when niri no longer has it."""
    window = _window(_window_id(window_id))
    if window is None:
        raise ComputerUseError(
            "APP_NOT_RUNNING",
            f"window {window_id} is no longer open; call get_app again to re-bind it",
            {"window_id": window_id},
        )
    return window


def _list_apps() -> list[dict[str, Any]]:
    """List running apps as {"id", "name", "running"} dicts, one per distinct app_id."""
    apps: dict[str, dict[str, Any]] = {}
    for window in _windows():
        app_id = window.get("app_id")
        if not isinstance(app_id, str) or not app_id or app_id in apps:
            continue
        apps[app_id] = {"id": app_id, "name": app_id, "running": True}
    return list(apps.values())


def _spec_app_id(spec: str | dict[str, str]) -> str:
    """Extract the app_id to match from one app spec, rejecting unresolvable shapes."""
    if isinstance(spec, str):
        if not spec.strip():
            raise ComputerUseError("INVALID_ARGUMENT", "the app spec must not be empty", {"spec": ""})
        return spec.strip()
    if isinstance(spec, dict):
        for key in ("bundle_id", "name"):
            value = spec.get(key)
            if isinstance(value, str) and value.strip():
                return value.strip()
    raise ComputerUseError(
        "INVALID_ARGUMENT",
        "the app spec must be an app_id or a {\"bundle_id\"|\"name\"} dict; Wayland windows have no launch paths",
        {"spec": str(spec)[:64]},
    )


def _resolve_app(spec: str | dict[str, str]) -> list[dict[str, Any]]:
    """Match one app spec against niri windows by app_id, casefolded.

    Returns every matching window, the focused one first, then the most
    recently focused.
    """
    wanted = _spec_app_id(spec).casefold()
    matches = [
        window
        for window in _windows()
        if isinstance(window.get("app_id"), str) and window["app_id"].casefold() == wanted
    ]

    def recency(window: dict[str, Any]) -> tuple[int, float]:
        stamp = window.get("focus_timestamp") or {}
        seconds = float(stamp.get("secs") or 0) + float(stamp.get("nanos") or 0) / 1e9 if isinstance(stamp, dict) else 0.0
        return (1 if window.get("is_focused") else 0, seconds)

    return sorted(matches, key=recency, reverse=True)


def _geometry(window: dict[str, Any]) -> _Geometry:
    """Derive the window's logical geometry from niri's layout, workspaces, and outputs."""
    layout = window.get("layout") if isinstance(window.get("layout"), dict) else {}
    size = layout.get("window_size") or layout.get("tile_size") or (0, 0)
    width, height = float(size[0]), float(size[1])
    tile_pos = layout.get("tile_pos_in_workspace_view")
    offset = layout.get("window_offset_in_tile") or (0.0, 0.0)
    workspace = next(
        (entry for entry in _response("Workspaces") if isinstance(entry, dict) and entry.get("id") == window.get("workspace_id")),
        None,
    )
    output_name = workspace.get("output") if isinstance(workspace, dict) else None
    outputs = _response("Outputs")
    output = outputs.get(output_name) if isinstance(outputs, dict) and output_name else None
    logical = output.get("logical") if isinstance(output, dict) else None
    output_rect = (
        (float(logical["x"]), float(logical["y"]), float(logical["width"]), float(logical["height"]))
        if isinstance(logical, dict)
        else None
    )
    if tile_pos is None:
        reason = (
            "niri exposes on-screen positions only for floating windows; this window is tiled, "
            "so its screen position is unknown"
        )
        return _Geometry(None, None, width, height, output_name, output_rect, reason)
    if not isinstance(workspace, dict) or not workspace.get("is_active"):
        return _Geometry(None, None, width, height, output_name, output_rect, "the window's workspace is not on screen")
    if output_rect is None:
        return _Geometry(None, None, width, height, output_name, output_rect, "the window's output geometry is unknown")
    x = output_rect[0] + float(tile_pos[0]) + float(offset[0])
    y = output_rect[1] + float(tile_pos[1]) + float(offset[1])
    return _Geometry(x, y, width, height, output_name, output_rect, "")


def _rect(geometry: _Geometry) -> tuple[float, float, float, float] | None:
    """The absolute window rect when the origin is known, else None."""
    if geometry.x is None or geometry.y is None:
        return None
    return (geometry.x, geometry.y, geometry.width, geometry.height)


def _focus_window(window_id: int) -> None:
    """Focus the bound window through niri and verify the focus landed.

    Keyboard and pointer input on Wayland is focus-bound, so input is refused
    (INJECTION_FAILED, nothing sent) unless niri reports the bound window as
    focused within _FOCUS_WAIT_SECONDS.
    """
    window_id = _window_id(window_id)
    if _focused_window_id() == window_id:
        return
    _niri({"Action": {"FocusWindow": {"id": window_id}}})
    deadline = time.monotonic() + _FOCUS_WAIT_SECONDS
    while True:
        if _focused_window_id() == window_id:
            return
        if time.monotonic() >= deadline:
            raise ComputerUseError(
                "INJECTION_FAILED",
                f"focus did not land on window {window_id}; no input was sent. Re-observe and retry",
                {"window_id": window_id},
            )
        time.sleep(_FOCUS_POLL_SECONDS)


def _is_frontmost(window_id: int) -> bool:
    """Report whether niri's focused window is the bound one."""
    return _focused_window_id() == _window_id(window_id)


# --- AT-SPI -------------------------------------------------------------------


def _atspi() -> Any:
    """Import and initialize libatspi through PyGObject, once per process.

    Raises TRANSPORT_ERROR naming the fix when the bindings are missing.
    """
    global _atspi_module
    if _atspi_module is not None:
        return _atspi_module
    try:
        import gi

        gi.require_version("Atspi", "2.0")
        from gi.repository import Atspi
    except (ImportError, ValueError) as error:
        raise ComputerUseError(
            "TRANSPORT_ERROR",
            "computer use backend unavailable: AT-SPI needs PyGObject with the Atspi 2.0 typelib in the "
            "kernel's Python (install PyGObject and libatspi/at-spi2-core, or use a venv created with "
            f"--system-site-packages): {str(error)[:120]}",
        ) from error
    Atspi.init()
    try:
        Atspi.set_timeout(_ATSPI_TIMEOUT_MS, _ATSPI_STARTUP_TIMEOUT_MS)
    except Exception:
        pass
    _atspi_module = Atspi
    return Atspi


def _call(function: Any, *args: Any, default: Any = None) -> Any:
    """Run one AT-SPI call, mapping any D-Bus or binding failure to default."""
    try:
        return function(*args)
    except Exception:
        return default


def _app_accessible(atspi: Any, pid: int) -> Any:
    """Find the accessible application owned by pid on the a11y bus, or None."""
    desktop = _call(atspi.get_desktop, 0)
    if desktop is None:
        return None
    count = _call(desktop.get_child_count, default=0) or 0
    for index in range(count):
        app = _call(desktop.get_child_at_index, index)
        if app is not None and _call(app.get_process_id) == pid:
            return app
    return None


def _states(accessible: Any) -> Any:
    """Read one accessible's state set, or None when unreadable."""
    return _call(accessible.get_state_set)


def _has(atspi: Any, states: Any, name: str) -> bool:
    """Report whether a state set contains one named Atspi.StateType."""
    return states is not None and bool(_call(states.contains, getattr(atspi.StateType, name), default=False))


def _children(accessible: Any) -> list[Any]:
    """Read one accessible's children, skipping unreadable ones."""
    count = _call(accessible.get_child_count, default=0) or 0
    children = []
    for index in range(count):
        child = _call(accessible.get_child_at_index, index)
        if child is not None:
            children.append(child)
    return children


def _frame(atspi: Any, app: Any, window: dict[str, Any]) -> Any:
    """Pick the app's top-level accessible that is the niri window, or None.

    The niri title matches a frame's name; several matches prefer the active
    one. Without a name match, a focused niri window maps onto the active
    frame and a single-frame app onto its only frame.
    """
    frames = _children(app)
    if not frames:
        return None
    title = window.get("title")
    named = [frame for frame in frames if title and _call(frame.get_name) == title]
    active = [frame for frame in frames if _has(atspi, _states(frame), "ACTIVE")]
    if len(named) == 1:
        return named[0]
    if named:
        return next((frame for frame in named if frame in active), named[0])
    if window.get("is_focused") and active:
        return active[0]
    if len(frames) == 1:
        return frames[0]
    return None


def _is_secure_role(atspi: Any, accessible: Any) -> bool | None:
    """Report whether one live accessible is a password field, None when unreadable."""
    try:
        return accessible.get_role() == atspi.Role.PASSWORD_TEXT
    except Exception:
        return None


def _text_value(accessible: Any) -> str | None:
    """Read one accessible's text through its Text interface, capped, or None."""
    text = _call(accessible.get_text_iface)
    if text is None:
        return None
    count = _call(text.get_character_count, default=0) or 0
    if count <= 0:
        return ""
    return _call(text.get_text, 0, min(count, 2000))


def _describe(atspi: Any, accessible: Any) -> dict[str, Any]:
    """Convert one accessible into the element dict contract the diff engine renders."""
    secure = _is_secure_role(atspi, accessible)
    role = _ATSPI_SECURE_ROLE if secure else _cap(_call(accessible.get_role_name))
    title = _cap(_call(accessible.get_name))
    value: str | None = None
    if not secure:
        value = _cap(_text_value(accessible))
        if value is not None and value == title:
            value = None  # labels repeat their name through Text; render it once
        if value is None:
            numeric = _call(accessible.get_value_iface)
            current = _call(numeric.get_current_value) if numeric is not None else None
            value = None if current is None else str(current)
    actions: list[str] = []
    action = _call(accessible.get_action_iface)
    if action is not None:
        for index in range(min(_call(action.get_n_actions, default=0) or 0, _MAX_ACTIONS)):
            name = _call(action.get_action_name, index)
            if name:
                actions.append(str(name))
    position = size = None
    component = _call(accessible.get_component_iface)
    extents = _call(component.get_extents, atspi.CoordType.WINDOW) if component is not None else None
    if extents is not None and extents.width > 0 and extents.height > 0:
        position = [float(extents.x), float(extents.y)]
        size = [float(extents.width), float(extents.height)]
    return {
        "role": role,
        "subrole": None,
        "title": title,
        "value": value,
        "description": _cap(_call(accessible.get_description)) or None,
        "placeholder": None,
        "actions": actions,
        "position": position,
        "size": size,
        "children": [],
    }


class _Walk:
    """Mutable state of one bounded depth-first walk."""

    def __init__(self, deadline: float) -> None:
        self.deadline = deadline
        self.refs: list[Any] = []
        self.focused_index: int | None = None
        self.truncated = False


def _walk(atspi: Any, parent: Any, depth: int, siblings: list[dict[str, Any]], walk: _Walk) -> None:
    """Append parent's showing children into siblings, depth-first, within the caps.

    Elements without STATE_SHOWING (closed menus, hidden tabs) are skipped
    with their subtrees: nothing in them is on screen to act on.
    """
    for child in _children(parent):
        if len(walk.refs) >= _MAX_ELEMENTS or time.monotonic() > walk.deadline:
            walk.truncated = True
            return
        states = _states(child)
        if states is not None and not _has(atspi, states, "SHOWING"):
            continue
        element = _describe(atspi, child)
        if _has(atspi, states, "FOCUSED"):
            walk.focused_index = len(walk.refs)
        walk.refs.append(child)
        siblings.append(element)
        if depth < _MAX_DEPTH:
            _walk(atspi, child, depth + 1, element["children"], walk)
        elif _call(child.get_child_count, default=0):
            walk.truncated = True


def _observe(window_id: int) -> Observation:
    """Snapshot the bound window into the same Observation the mac backend builds.

    window_title is niri's title; window_rect is the absolute logical rect
    when niri exposes the origin (floating windows), else None; the tree is
    the AT-SPI frame's showing descendants (WINDOW-relative positions),
    capped at _MAX_DEPTH, _MAX_ELEMENTS, and _MAX_OBSERVE_SECONDS. An app
    that is not on the a11y bus (or whose frame cannot be matched) observes
    as an empty tree. Raises APP_NOT_RUNNING when the window is gone and
    TRANSPORT_ERROR when niri or AT-SPI is unavailable.
    """
    window = _require_window(window_id)
    rect = _rect(_geometry(window))
    atspi = _atspi()
    with _ATSPI_LOCK:
        app = _app_accessible(atspi, window.get("pid"))
        frame = _frame(atspi, app, window) if app is not None else None
        tree: list[dict[str, Any]] = []
        walk = _Walk(time.monotonic() + _MAX_OBSERVE_SECONDS)
        if frame is not None:
            _walk(atspi, frame, 1, tree, walk)
    return Observation(
        window_title=_cap(window.get("title")),
        tree=tree,
        refs=walk.refs,
        window_rect=rect,
        focused_index=walk.focused_index,
        window_id=window_id,
        truncated=walk.truncated,
    )


def _find_focused(atspi: Any, roots: list[Any], deadline: float) -> tuple[Any, bool]:
    """Search showing descendants of roots for STATE_FOCUSED.

    Returns (element, complete): element is None when none was found, and
    complete is False when the search hit its element or time bound first.
    """
    stack = list(reversed(roots))
    seen = 0
    while stack:
        if seen >= _MAX_ELEMENTS or time.monotonic() > deadline:
            return None, False
        node = stack.pop()
        seen += 1
        states = _states(node)
        if states is not None and not _has(atspi, states, "SHOWING"):
            continue
        if _has(atspi, states, "FOCUSED"):
            return node, True
        stack.extend(reversed(_children(node)))
    return None, True


def _focus_roots(atspi: Any, app: Any, window: dict[str, Any]) -> list[Any]:
    """The frames focus can live in: the bound frame plus the app's active frames (dialogs)."""
    roots: list[Any] = []
    frame = _frame(atspi, app, window)
    if frame is not None:
        roots.append(frame)
    for candidate in _children(app):
        if candidate not in roots and _has(atspi, _states(candidate), "ACTIVE"):
            roots.append(candidate)
    return roots


def _focused_is_secure(window_id: int) -> bool | None:
    """Report whether the app's live focused element is a password field.

    True for ROLE_PASSWORD_TEXT, False when the focused element is anything
    else or the full search found no focused element, and None - callers fail
    closed - when it cannot be verified: AT-SPI unavailable, the app not on
    the a11y bus, the focused element's role unreadable, or a search that hit
    its bounds before finding focus.
    """
    try:
        window = _window(_window_id(window_id))
        if window is None:
            return None
        atspi = _atspi()
    except ComputerUseError:
        return None
    with _ATSPI_LOCK:
        app = _app_accessible(atspi, window.get("pid"))
        if app is None:
            return None
        roots = _focus_roots(atspi, app, window)
        if not roots:
            return None
        focused, complete = _find_focused(atspi, roots, time.monotonic() + _MAX_FOCUS_SECONDS)
        if focused is None:
            return False if complete else None
        return _is_secure_role(atspi, focused)


def _window_fingerprint(window_id: int) -> tuple[Any, ...] | None:
    """Read a live identity of the bound window for the post-action settle.

    niri's focused window id and the window title, plus from AT-SPI the
    frame's child count and the focused element's role, name, and - for a
    non-secure element - its text head (a secure field's value is never
    read). Returns None when the window cannot be read.
    """
    try:
        window = _window(_window_id(window_id))
        focused_window = _focused_window_id()
    except ComputerUseError:
        return None
    if window is None:
        return None
    base = (focused_window, window.get("title"))
    try:
        atspi = _atspi()
    except ComputerUseError:
        return base + (None, None, None, None)
    with _ATSPI_LOCK:
        app = _app_accessible(atspi, window.get("pid"))
        roots = _focus_roots(atspi, app, window) if app is not None else []
        count = _call(roots[0].get_child_count, default=None) if roots else None
        focused, _complete = _find_focused(atspi, roots, time.monotonic() + _FINGERPRINT_FOCUS_SECONDS)
        if focused is None:
            return base + (count, None, None, None)
        secure = _is_secure_role(atspi, focused)
        role = _call(focused.get_role_name)
        name = _call(focused.get_name)
        if secure is not False:
            head = ""  # an unverifiable or secure field's value is never read
        else:
            text = _text_value(focused)
            head = text[:_FINGERPRINT_VALUE_CHARS] if isinstance(text, str) else None
        return base + (count, role, name, head)


def _live_fingerprint(ref: Any) -> tuple[str | None, str | None]:
    """Read one live element's (role, name) for freshness checks, as rendered."""
    try:
        atspi = _atspi()
    except ComputerUseError:
        return (None, None)
    with _ATSPI_LOCK:
        secure = _is_secure_role(atspi, ref)
        if secure is None:
            return (None, None)
        role = _ATSPI_SECURE_ROLE if secure else _cap(_call(ref.get_role_name))
        return (role, _cap(_call(ref.get_name)))


def _live_is_secure(ref: Any) -> bool | None:
    """Report whether one live element is a password field, None when unreadable."""
    try:
        atspi = _atspi()
    except ComputerUseError:
        return None
    with _ATSPI_LOCK:
        return _is_secure_role(atspi, ref)


def _perform_action(ref: Any, action: str) -> None:
    """Run one named AT-SPI action on an element (no focus or pointer involved)."""
    _atspi()
    with _ATSPI_LOCK:
        iface = _call(ref.get_action_iface)
        count = _call(iface.get_n_actions, default=0) if iface is not None else 0
        for index in range(count or 0):
            if _call(iface.get_action_name, index) == action:
                try:
                    performed = iface.do_action(index)
                except Exception as error:
                    raise ComputerUseError(
                        "ACTION_UNSUPPORTED",
                        f"the element did not perform {action}: {str(error)[:_ERROR_LIMIT]}",
                        {"action": str(action)[:32]},
                    ) from error
                if not performed:
                    raise ComputerUseError(
                        "ACTION_UNSUPPORTED", f"the element refused {action}", {"action": str(action)[:32]}
                    )
                return
    raise ComputerUseError(
        "ACTION_UNSUPPORTED", f"the element no longer exposes {action}", {"action": str(action)[:32]}
    )


def _press_action(actions: list[str]) -> str | None:
    """Pick the element's default activation action (the AXPress analogue), or None."""
    lowered = {action.casefold(): action for action in actions}
    for wanted in _PRESS_ACTIONS:
        if wanted in lowered:
            return lowered[wanted]
    return None


def _is_settable(ref: Any, attribute: str = "AXValue") -> bool:
    """Report whether an element accepts text writes: EditableText plus STATE_EDITABLE."""
    atspi = _atspi()
    with _ATSPI_LOCK:
        return _call(ref.get_editable_text_iface) is not None and _has(atspi, _states(ref), "EDITABLE")


def _set_value(ref: Any, value: str) -> None:
    """Replace an editable element's text through EditableText.set_text_contents."""
    _atspi()
    with _ATSPI_LOCK:
        editable = _call(ref.get_editable_text_iface)
        if editable is None:
            raise ComputerUseError("ACTION_UNSUPPORTED", "the element does not accept text writes", {})
        try:
            written = editable.set_text_contents(value)
        except Exception as error:
            raise ComputerUseError(
                "ACTION_UNSUPPORTED", f"setting the text failed: {str(error)[:_ERROR_LIMIT]}", {}
            ) from error
        if not written:
            raise ComputerUseError("ACTION_UNSUPPORTED", "the element refused the text write", {})


def _current_value(ref: Any) -> str | None:
    """Read an element's full current text (uncapped search source), or None."""
    _atspi()
    with _ATSPI_LOCK:
        text = _call(ref.get_text_iface)
        if text is None:
            return None
        count = _call(text.get_character_count, default=0) or 0
        return "" if count <= 0 else _call(text.get_text, 0, count)


def _select_text_range(ref: Any, location: int, length: int) -> None:
    """Select [location, location+length) through the Text selection API."""
    _atspi()
    with _ATSPI_LOCK:
        text = _call(ref.get_text_iface)
        if text is None:
            raise ComputerUseError("ACTION_UNSUPPORTED", "the element exposes no text to select", {})
        try:
            if (_call(text.get_n_selections, default=0) or 0) > 0:
                selected = text.set_selection(0, location, location + length)
            else:
                selected = text.add_selection(location, location + length)
        except Exception as error:
            raise ComputerUseError(
                "ACTION_UNSUPPORTED", f"selecting the text failed: {str(error)[:_ERROR_LIMIT]}", {}
            ) from error
        if not selected:
            raise ComputerUseError(
                "ACTION_UNSUPPORTED",
                "the element refused the selection",
                {"location": location, "length": length},
            )


# --- virtual input ------------------------------------------------------------


def _refuse_secure_live(window_id: int) -> None:
    """Re-check the live focus after focusing the window; refuse a secure or unknown focus."""
    secure = _focused_is_secure(window_id)
    if secure is None:
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            "could not verify that the focused element is not a secure text field; "
            "ask the user to type passwords and other secrets themselves",
            {"live": False},
        )
    if secure:
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            "the focused element is a secure text field; ask the user to type "
            "passwords and other secrets themselves",
            {"live": True},
        )


def _pointer_point(window_id: int, point: tuple[float, float]) -> tuple[_wlinput.PointerTarget, tuple[float, float]]:
    """Map one window-relative logical point onto the window's output for the virtual pointer."""
    if (
        not isinstance(point, tuple)
        or len(point) != 2
        or not all(isinstance(value, (int, float)) and not isinstance(value, bool) for value in point)
    ):
        raise ComputerUseError("INVALID_ARGUMENT", "point must be an (x, y) pair of numbers", {"point": repr(point)[:64]})
    geometry = _geometry(_require_window(window_id))
    if geometry.x is None or geometry.y is None or geometry.output_rect is None:
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            f"coordinate input needs the window's screen position: {geometry.reason}. Use element "
            "actions by index (they run through AT-SPI), or ask the user to make the window floating",
            {"platform": "wayland", "window_id": window_id},
        )
    if not 0 <= float(point[0]) < geometry.width or not 0 <= float(point[1]) < geometry.height:
        raise ComputerUseError(
            "INVALID_ARGUMENT",
            f"point {point!r} is outside the window ({geometry.width:.0f}x{geometry.height:.0f})",
            {"point": repr(point)[:64]},
        )
    out_x, out_y, out_w, out_h = geometry.output_rect
    local = (geometry.x + float(point[0]) - out_x, geometry.y + float(point[1]) - out_y)
    return _wlinput.PointerTarget(geometry.output, int(out_w), int(out_h)), local


def _click(window_id: int, point: tuple[float, float], button: str = "left", count: int = 1) -> None:
    """Focus the window, then click at one window-relative logical point through the virtual pointer."""
    window_id = _window_id(window_id)
    if button not in _wlinput.BUTTONS:
        raise ComputerUseError(
            "INVALID_ARGUMENT", f"button must be one of left, middle, right, got {button!r}", {"button": str(button)[:32]}
        )
    if not isinstance(count, int) or isinstance(count, bool) or count < 1:
        raise ComputerUseError("INVALID_ARGUMENT", f"count must be an integer of at least 1, got {count!r}", {"count": count})
    _pointer_point(window_id, point)  # refuse an unmappable point before moving focus
    _focus_window(window_id)
    target, local = _pointer_point(window_id, point)
    _wlinput._click(target, local, button, count)


def _drag(window_id: int, start: tuple[float, float], end: tuple[float, float]) -> None:
    """Focus the window, then drag with the left button between two window-relative points."""
    window_id = _window_id(window_id)
    _pointer_point(window_id, start)
    _pointer_point(window_id, end)
    _focus_window(window_id)
    target, local_start = _pointer_point(window_id, start)
    _target, local_end = _pointer_point(window_id, end)
    _wlinput._drag(target, local_start, local_end)


def _scroll(window_id: int, direction: str, pages: int = 1, point: tuple[float, float] | None = None) -> None:
    """Focus the window, then send wheel clicks at one window-relative point."""
    window_id = _window_id(window_id)
    if direction not in ("up", "down", "left", "right"):
        raise ComputerUseError(
            "INVALID_ARGUMENT",
            f"direction must be one of up, down, left, right, got {direction!r}",
            {"direction": str(direction)[:32]},
        )
    if not isinstance(pages, int) or isinstance(pages, bool) or pages < 1:
        raise ComputerUseError("INVALID_ARGUMENT", f"pages must be an integer of at least 1, got {pages!r}", {"pages": pages})
    if point is None:
        raise ComputerUseError(
            "INVALID_ARGUMENT", "scrolling on Wayland needs a target point inside the window", {}
        )
    _pointer_point(window_id, point)
    _focus_window(window_id)
    target, local = _pointer_point(window_id, point)
    _wlinput._scroll(target, local, direction, pages * _WHEEL_CLICKS_PER_PAGE)


def _chord_stroke(chord: ParsedChord) -> _wlinput.KeyStroke:
    """Translate one parsed chord into a keysym plus a modifier mask for our keymap."""
    keysym = _KEYSYMS.get(chord.key) or _wlinput._keysym_for_char(chord.key)
    mask = 0
    for modifier in _MODIFIER_ORDER:
        if modifier in chord.modifiers:
            mask |= _wlinput.MODIFIER_MASKS[modifier]
    return _wlinput.KeyStroke(keysym, mask)


def _press_key(window_id: int, key: str) -> None:
    """Focus the window, re-check the live focus for a secure field, then send one chord.

    cmd maps to the Logo (super) modifier. The chord reaches whatever holds
    keyboard focus, which _focus_window just verified is the bound window.
    """
    window_id = _window_id(window_id)
    stroke = _chord_stroke(_parse_chord(key))
    _focus_window(window_id)
    _refuse_secure_live(window_id)
    _wlinput._send_keys([stroke])


def _type_text(window_id: int, text: str) -> None:
    """Focus the window, re-check the live focus for a secure field, then type text.

    Every character is typed through its own keysym in an uploaded keymap,
    so typing does not depend on the user's keyboard layout; a newline
    presses Return and a tab presses Tab. An empty string is a no-op.
    """
    window_id = _window_id(window_id)
    if not isinstance(text, str):
        raise ComputerUseError(
            "INVALID_ARGUMENT", f"text must be a string, got {type(text).__name__}", {"text": type(text).__name__}
        )
    if not text:
        return
    strokes = [_wlinput.KeyStroke(_wlinput._keysym_for_char(character)) for character in text]
    _focus_window(window_id)
    _refuse_secure_live(window_id)
    _wlinput._send_keys(strokes)


# --- screenshots, lock state, capabilities ------------------------------------


def _run(argv: list[str]) -> subprocess.CompletedProcess[bytes]:
    """Run one helper tool, raising TRANSPORT_ERROR for an unrunnable tool or a timeout."""
    try:
        return subprocess.run(argv, capture_output=True, timeout=_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired as error:
        raise ComputerUseError(
            "TRANSPORT_ERROR", f"{Path(argv[0]).name} timed out after {int(_TIMEOUT_SECONDS)} seconds"
        ) from error
    except OSError as error:
        raise ComputerUseError(
            "TRANSPORT_ERROR", f"{Path(argv[0]).name} is not available: {str(error)[:_ERROR_LIMIT]}"
        ) from error


def _optional_tool(name: str) -> str | None:
    """Resolve one tool to its executable path, absolute candidates first, or None."""
    absolute = _TOOL_PATHS.get(name)
    if absolute is not None and os.path.isfile(absolute):
        return absolute
    return shutil.which(name)


def _overlapping(window: dict[str, Any], rect: tuple[float, float, float, float]) -> bool:
    """Report whether another on-screen floating window intersects rect.

    A focused floating window is drawn above the others in niri, so the
    check only matters while the bound window is not focused.
    """
    if window.get("is_focused"):
        return False
    x, y, width, height = rect
    for other in _windows():
        if other.get("id") == window.get("id") or other.get("workspace_id") != window.get("workspace_id"):
            continue
        other_rect = _rect(_geometry(other))
        if other_rect is None:
            continue
        ox, oy, ow, oh = other_rect
        if ox < x + width and x < ox + ow and oy < y + height and y < oy + oh:
            return True
    return False


def _screenshot_window(window_id: int) -> tuple[dict[str, str | int], tuple[float, float, float, float]]:
    """Capture the bound window's on-screen rect with grim into the hardened capture dir.

    Returns ({"path", "width", "height"}, logical rect): the PNG's own pixel
    dimensions (grim captures at the output scale, so a 2x output yields 2x
    pixels) and the logical rect the App layer scales screenshot points by.
    Refuses (ACTION_UNSUPPORTED) when the rect is not derivable or another
    floating window overlaps it - a region capture would include its pixels.
    Raises TRANSPORT_ERROR when grim is missing or fails.
    """
    window_id = _window_id(window_id)
    window = _require_window(window_id)
    geometry = _geometry(window)
    rect = _rect(geometry)
    if rect is None:
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            f"the window cannot be captured: {geometry.reason}; use get_ax_state() instead",
            {"platform": "wayland", "window_id": window_id},
        )
    if _overlapping(window, rect):
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            "another window overlaps the bound window, so a region capture would include its content; "
            "use get_ax_state() instead",
            {"platform": "wayland", "window_id": window_id},
        )
    grim = _optional_tool("grim")
    if grim is None:
        raise ComputerUseError(
            "TRANSPORT_ERROR", "computer use backend unavailable: the Wayland backend needs grim on PATH for screenshots"
        )
    x, y, width, height = (int(round(value)) for value in rect)
    capture._refuse_symlinked(capture._SCREENSHOTS_DIR)
    dir_fd = capture._open_capture_dir()
    try:
        capture._sweep_screenshots(dir_fd)
        path = capture._SCREENSHOTS_DIR / f"{uuid4()}.png"
        capture._refuse_non_regular_target(dir_fd, path.name)
        finished = _run([grim, "-g", f"{x},{y} {width}x{height}", str(path)])
        if finished.returncode != 0:
            reason = (finished.stderr or finished.stdout or b"").decode("utf-8", "replace").strip()[:_ERROR_LIMIT]
            raise ComputerUseError("TRANSPORT_ERROR", f"grim failed with exit code {finished.returncode}: {reason}")
        png_width, png_height = capture._png_dimensions(dir_fd, path.name)
        try:
            os.chmod(path.name, 0o600, dir_fd=dir_fd, follow_symlinks=False)
        except OSError:
            pass
        capture._sweep_screenshots(dir_fd, keep_name=path.name)
        return {"path": str(path), "width": png_width, "height": png_height}, (
            float(x),
            float(y),
            float(width),
            float(height),
        )
    finally:
        os.close(dir_fd)


def _screen_locked() -> bool:
    """Report the session lock through logind's LockedHint, failing closed.

    niri sets LockedHint itself whenever its session lock engages, and a
    session that is not Active (another VT) is treated as locked too. Any
    failure to read the state reads as locked.
    """
    loginctl = _optional_tool("loginctl")
    if loginctl is None:
        return True
    session = os.environ.get("XDG_SESSION_ID") or "auto"
    try:
        finished = _run([loginctl, "show-session", session, "-p", "LockedHint", "-p", "Active"])
    except ComputerUseError:
        return True
    if finished.returncode != 0:
        return True
    values = dict(
        line.split("=", 1) for line in finished.stdout.decode("utf-8", "replace").splitlines() if "=" in line
    )
    return values.get("LockedHint") != "no" or values.get("Active") != "yes"


def _status() -> dict[str, Any]:
    """Report the Wayland backend's real capabilities in the permissions shape.

    accessibility is AT-SPI (ok when the bindings load and the a11y bus
    answers), screen_recording is grim, and input is the compositor's
    virtual pointer/keyboard protocols; each is ok, missing, or unknown, and
    help names the fix for anything not ok.
    """
    help_lines: list[str] = []
    try:
        atspi = _atspi()
        with _ATSPI_LOCK:
            desktop = _call(atspi.get_desktop, 0)
        accessibility = "ok" if desktop is not None else "unknown"
        if desktop is None:
            help_lines.append("AT-SPI: the accessibility bus did not answer; check that at-spi2-core is running")
    except ComputerUseError as error:
        accessibility = "missing"
        help_lines.append(error.message)
    screen = "ok" if _optional_tool("grim") is not None else "missing"
    if screen != "ok":
        help_lines.append("Screenshots: install grim (niri implements wlr-screencopy)")
    try:
        available = _wlinput._available()
        pointer = "ok" if available["pointer"] else "missing"
        keyboard = "ok" if available["keyboard"] else "missing"
    except ComputerUseError:
        pointer = keyboard = "unknown"
    if pointer != "ok" or keyboard != "ok":
        help_lines.append(
            "Input: the compositor must offer zwlr_virtual_pointer_manager_v1 and "
            "zwp_virtual_keyboard_manager_v1 (niri does for clients outside a sandboxed security context)"
        )
    help_lines.append(
        "Wayland (niri): apps must expose AT-SPI (GTK/Qt do; Firefox needs accessibility enabled, "
        "Chromium/Electron need --force-renderer-accessibility); coordinate input and screenshots "
        "need a floating window, because niri does not expose tiled windows' screen positions"
    )
    return {
        "accessibility": accessibility,
        "screen_recording": screen,
        "input": {"pointer": pointer, "keyboard": keyboard},
        "help": help_lines,
    }
