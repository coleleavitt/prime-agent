"""Prime Agent computer use: observe and operate native desktop apps.

The module is kernel-resident and a thin client of the Prime Agent host:
every observation, input, capture, allowlist and permission decision runs in
the host (the ``computer_use.*`` host requests, served by the
``pa-computer-use`` crate) on macOS, the Linux X11 backend, and the
Wayland (niri) backend. This module validates the Python-typed arguments,
forwards each call, raises the host's failures as ``ComputerUseError``, and
attaches screenshots to the model's context.

get_state reports the platform surface, get_app binds one app and returns an
App whose element-indexed actions re-observe the accessibility tree. Apps
must be on the user-edited allowlist in the settings file; every binding and
every action re-checks the allowlist gate and the locked screen.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, cast

from . import errors  # noqa: F401 - errors is part of the module surface
from .errors import ComputerUseError

try:
    from rlm import host_request as _host_request
    from rlm.repl import host_request_blocking as _host_request_blocking
except ImportError:  # outside the kernel: every call reports the missing host
    _host_request = None
    _host_request_blocking = None

__all__ = ["App", "ComputerUseError", "get_app", "get_state", "list_apps", "permissions_status"]

_PASTE_FORMATS = ("text", "md", "html")
_MAX_CLICK_COUNT = 10
_BLOCKING_TIMEOUT_SECONDS = 30.0
_INDEX_LIMIT = 2**63 - 1

_bound_apps: dict[int, App] = {}


# The host's error for a request type it registers no handler for: every
# current host serves computer_use.*, so the host predates this client.
_UNSERVED_HOST_REQUEST = "is not available in this session"


def _no_host(error: BaseException | None = None) -> ComputerUseError:
    if error is not None and _UNSERVED_HOST_REQUEST in str(error):
        return ComputerUseError(
            "TRANSPORT_ERROR",
            "computer use backend unavailable: this kernel's Prime Agent host does not serve "
            "computer_use requests: the prime-agent binary is older than this computer-use skill "
            "(host/runtime version skew). Reinstall prime-agent so the binary and its bundled "
            "skills match (`cargo install --path crates/pa-cli` from the checkout, or rerun the installer)",
        )
    reason = f": {str(error)[:200]}" if error is not None else ""
    return ComputerUseError(
        "TRANSPORT_ERROR",
        f"computer use backend unavailable: the Prime Agent host did not answer{reason}",
    )


def _unwrap(reply: object) -> Any:
    """Return a reply's result, raising its error as ComputerUseError."""
    if not isinstance(reply, dict):
        raise _no_host()
    reply = cast("dict[str, Any]", reply)
    error = reply.get("error")
    if isinstance(error, dict):
        error = cast("dict[str, Any]", error)
        raise ComputerUseError(str(error.get("code")), str(error.get("message")), error.get("details"))
    if "ok" not in reply:
        raise _no_host()
    return reply["ok"]


async def _request(request_type: str, payload: dict[str, Any]) -> Any:
    """Send one host request and unwrap its reply."""
    if _host_request is None:
        raise _no_host()
    try:
        reply = await _host_request(request_type, payload)
    except ComputerUseError:
        raise
    except Exception as error:
        raise _no_host(error) from error
    return _unwrap(reply)


def _request_blocking(request_type: str, payload: dict[str, Any]) -> Any:
    """Send one host request from synchronous code and unwrap its reply."""
    if _host_request_blocking is None:
        raise _no_host()
    try:
        raw = _host_request_blocking({**payload, "type": request_type}, timeout_s=_BLOCKING_TIMEOUT_SECONDS)
    except Exception as error:
        raise _no_host(error) from error
    if raw.get("status") != "ok":
        raise _no_host(RuntimeError(str(raw.get("error") or f"host request {request_type} failed")))
    return _unwrap(raw.get("result"))


def _json_key(key: object) -> object:
    """A dict key as the error details carry it: JSON primitives as-is, else its repr."""
    return key if isinstance(key, (str, int, float, bool)) or key is None else repr(key)


def _encode_spec(spec: object) -> dict[str, Any]:
    """The get_app spec's shape plus its Python str() and repr()."""
    encoded: dict[str, Any] = {"str": str(spec), "repr": repr(spec)}
    if isinstance(spec, str):
        encoded.update(kind="str", value=spec)
    elif isinstance(spec, dict):
        items = cast("dict[object, object]", spec)
        encoded.update(
            kind="dict",
            entries=[[key, value if isinstance(value, str) else None] for key, value in items.items() if isinstance(key, str)],
            keys=[_json_key(key) for key in sorted(items, key=str)],
        )
    else:
        encoded.update(kind="other", type=type(spec).__name__)
    return encoded


def _is_number(value: object) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _encode_point(point: object) -> dict[str, Any]:
    """An (x, y) tuple of numbers, or anything else by its repr."""
    encoded: dict[str, Any] = {"repr": repr(point)}
    if isinstance(point, tuple):
        pair = cast("tuple[object, ...]", point)
        if len(pair) == 2 and _is_number(pair[0]) and _is_number(pair[1]):
            encoded.update(x=float(cast(float, pair[0])), y=float(cast(float, pair[1])))
    return encoded


def _clamp_index(value: int) -> int:
    return max(min(value, _INDEX_LIMIT), -_INDEX_LIMIT)


def _encode_target(target: object) -> dict[str, Any]:
    """An element index, a window-screenshot point, or the type of anything else."""
    if isinstance(target, int) and not isinstance(target, bool):
        return {"kind": "index", "index": _clamp_index(target)}
    if isinstance(target, tuple):
        return {"kind": "point", "point": _encode_point(cast("tuple[object, ...]", target))}
    return {"kind": "invalid", "type": type(target).__name__}


def _encode_index(index: object) -> dict[str, Any]:
    if isinstance(index, int) and not isinstance(index, bool):
        return {"index": _clamp_index(index)}
    return {"type": type(index).__name__}


def _encode_text(value: object) -> dict[str, Any]:
    if isinstance(value, str):
        return {"text": value}
    return {"type": type(value).__name__}


def _instructions_dir() -> str:
    """Where the per-app guides ship: the packaged copy, else the skill dir."""
    packaged = Path(__file__).resolve().parent / "references" / "app-instructions"
    if packaged.is_dir():
        return str(packaged)
    return str(Path(__file__).resolve().parents[2] / "references" / "app-instructions")


async def _attach(path: str) -> None:
    """Load the screenshot into the model's context, best-effort (a non-vision
    model, a missing attach_image skill, or an older host all skip it)."""
    try:
        from attach_image import run  # pyright: ignore[reportMissingImports]

        await run(path)
    except Exception:
        return


def _print_missing_grants(status: dict[str, Any]) -> None:
    """Print the first-run guidance when grants or backend pieces are missing."""
    missing = [name for name in ("accessibility", "screen_recording") if status.get(name) == "missing"]
    if not missing:
        return
    if "input" in status:  # the Wayland backend reports its own pieces, not macOS grants
        print("Prime Agent computer use is missing Wayland backend pieces:")
    else:
        print("Prime Agent computer use needs macOS permissions before it can drive apps:")
    for line in status.get("help") or []:
        print(line)


async def get_state(emit: bool = True) -> dict[str, Any]:
    """Assemble the discovery snapshot: apps, permissions, allowlist, platform.

    emit=True fires the one-time computer_use_session_started telemetry on the
    first call and prints first-run guidance when grants are missing;
    emit=False is a silent query that returns the same dict.
    """
    state = cast("dict[str, Any]", await _request("computer_use.get_state", {"emit": bool(emit)}))
    if emit:
        _print_missing_grants(cast("dict[str, Any]", state.get("permissions") or {}))
    return state


async def list_apps() -> list[dict[str, Any]]:
    """List the running apps as {"id", "name", "running"} dicts."""
    return cast("list[dict[str, Any]]", await _request("computer_use.list_apps", {}))


async def permissions_status() -> dict[str, Any]:
    """Report the accessibility and screen-recording grants with help text.

    Linux X11 reports both as unknown with a note; Wayland reports AT-SPI,
    grim, and the virtual-input protocols.
    """
    return cast("dict[str, Any]", await _request("computer_use.permissions_status", {}))


async def get_app(app: str | dict[str, str]) -> App:
    """Bind one app by name, bundle id, or a {"bundle_id"|"name"|"path"} dict.

    Runs the allowlist gate, checks the locked screen and the macOS grants,
    launches the app when it is not running (macOS), and loads the first
    accessibility state. On Linux the spec matches a running window's
    WM_CLASS (X11) or app_id (Wayland). Raises ComputerUseError
    TRANSPORT_ERROR without a backend, SCREEN_LOCKED, APP_NOT_ALLOWED,
    AMBIGUOUS_APP, APP_LAUNCH_FAILED, APP_NOT_RUNNING,
    PERMISSIONS_NOT_GRANTED, or INVALID_ARGUMENT.
    """
    bound = cast(
        "dict[str, Any]",
        await _request(
            "computer_use.get_app",
            {"spec": _encode_spec(app), "instructions_dir": _instructions_dir()},
        ),
    )
    handle = int(bound["handle"])
    existing = _bound_apps.get(handle)
    if existing is not None:
        return existing
    instance = App(str(bound["bundle_id"]), str(bound["name"]), int(bound["pid"]))
    instance._handle = handle
    instance._state = bound.get("state")
    _bound_apps[handle] = instance
    return instance


class App:
    """One bound running app with element-indexed accessibility actions.

    Every action re-checks the allowlist gate and the locked screen before it
    dispatches; element indices come from the last get_ax_state snapshot and a
    stale index raises ELEMENT_STALE. On Linux pid carries the bound window id.
    """

    def __init__(self, bundle_id: str, name: str, pid: int) -> None:
        """Wrap one host binding; get_app creates these and sets the binding's handle."""
        self._bundle_id = bundle_id
        self._name = name
        self._pid = pid
        self._handle = -1  # no binding: the host refuses every call
        self._state: str | None = None

    def __repr__(self) -> str:
        return f"<App {self._name} ({self._bundle_id}) pid {self._pid}>"

    @property
    def bundle_id(self) -> str:
        """The bound app's bundle identifier (the WM_CLASS or app_id on Linux)."""
        return self._bundle_id

    @property
    def name(self) -> str:
        """The bound app's localized name."""
        return self._name

    @property
    def pid(self) -> int:
        """The bound app's process identifier, or the window id on Linux."""
        return self._pid

    @property
    def state(self) -> str | None:
        """The last accessibility text get_ax_state returned."""
        return self._state

    async def _call(self, method: str, payload: dict[str, Any] | None = None) -> Any:
        return await _request("computer_use.app", {**(payload or {}), "handle": self._handle, "method": method})

    async def get_ax_state(self, diff: bool = True) -> str:
        """Return the element-indexed accessibility text, diffed against the previous snapshot.

        diff=False returns the full tree.
        """
        text = str(await self._call("get_ax_state", {"diff": bool(diff)}))
        self._state = text
        return text

    async def get_screenshot(self, attach: bool = True) -> dict[str, Any]:
        """Capture the bound window and attach the image to the context.

        Returns {"path", "width", "height"}; attach=False skips the context
        attach.
        """
        result = cast("dict[str, Any]", await self._call("get_screenshot"))
        if attach:
            await _attach(str(result["path"]))
        return result

    async def get_text_regions(self, attach: bool = False) -> dict[str, Any]:
        """Read the focused window with OCR and return its text regions (macOS).

        Regions carry text, confidence, and window-screenshot-relative pixel
        coordinates; attach=True also attaches the screenshot.
        """
        result = cast("dict[str, Any]", await self._call("get_text_regions"))
        path = str(result.pop("path", ""))
        if attach and path:
            await _attach(path)
        return result

    async def get_state_and_screenshot(self, diff: bool = True, attach: bool = True) -> dict[str, Any]:
        """Return {"state", "screenshot"} in one call; the screenshot is None when capture fails."""
        state = await self.get_ax_state(diff)
        try:
            screenshot = await self.get_screenshot(attach)
        except ComputerUseError:
            screenshot = None
        return {"state": state, "screenshot": screenshot}

    async def click(self, target: int | tuple[float, float], button: str = "left", count: int = 1) -> None:
        """Click one element index or a window-screenshot (x, y) point."""
        if button not in ("left", "right", "middle"):
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                f"button must be one of left, right, middle, got {button!r}",
                {"button": str(button)[:32]},
            )
        if not isinstance(count, int) or isinstance(count, bool) or not 1 <= count <= _MAX_CLICK_COUNT:
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                f"count must be an integer from 1 to {_MAX_CLICK_COUNT}, got {count!r}",
                {"count": count},
            )
        await self._call("click", {"target": _encode_target(target), "button": button, "count": count})

    async def drag(self, from_: tuple[float, float], to: tuple[float, float]) -> None:
        """Drag between two window-screenshot (x, y) points."""
        await self._call("drag", {"from": _encode_point(from_), "to": _encode_point(to)})

    async def scroll(self, target: int | tuple[float, float], direction: str, pages: int = 1) -> None:
        """Scroll at one element index or window-screenshot point in one direction."""
        if direction not in ("up", "down", "left", "right"):
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                f"direction must be one of up, down, left, right, got {direction!r}",
                {"direction": str(direction)[:32]},
            )
        if not isinstance(pages, int) or isinstance(pages, bool) or pages < 1:
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                f"pages must be an integer of at least 1, got {pages!r}",
                {"pages": pages},
            )
        await self._call("scroll", {"target": _encode_target(target), "direction": direction, "pages": pages})

    async def press_key(self, key: str) -> None:
        """Post one key chord such as "cmd+shift+f" or "Return" to the app.

        Refuses when the focused element is a secure field.
        """
        await self._call("press_key", {"key": _encode_text(key)})

    async def type_text(self, text: str) -> None:
        """Type literal text into the app; refuses a focused secure field."""
        await self._call("type_text", {"text": _encode_text(text)})

    async def set_value(self, element_index: int, value: str) -> None:
        """Set one element's value through the accessibility API; secure fields are refused."""
        if not isinstance(value, str):
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                f"value must be a string, got {type(value).__name__}",
                {"value": type(value).__name__},
            )
        await self._call("set_value", {"element_index": _encode_index(element_index), "value": value})

    async def select_text(self, element_index: int, text: str, prefix: str | None = None, suffix: str | None = None) -> None:
        """Select one occurrence of text inside the element without writing its content.

        prefix and suffix constrain the match. Raises ELEMENT_STALE when the
        text is not in the element's current text and ACTION_UNSUPPORTED when
        it occurs more than once or cannot be read. Secure fields are refused.
        """
        if not isinstance(text, str) or not isinstance(prefix, (str, type(None))) or not isinstance(suffix, (str, type(None))):
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                "text, prefix, and suffix must be strings",
                {"text": type(text).__name__},
            )
        if not text:
            raise ComputerUseError("INVALID_ARGUMENT", "text must be a non-empty string to select", {"text": ""})
        await self._call(
            "select_text",
            {"element_index": _encode_index(element_index), "text": text, "prefix": prefix, "suffix": suffix},
        )

    async def perform_secondary_action(self, element_index: int, action: str) -> None:
        """Perform one named action the element exposes, such as AXShowMenu."""
        encoded = {"name": action} if isinstance(action, str) else {"str": str(action)}
        await self._call("perform_secondary_action", {"element_index": _encode_index(element_index), "action": encoded})

    async def paste(self, text: str, format: str = "text") -> None:
        """Paste text through the clipboard with cmd+v, restoring the clipboard after (macOS).

        format is one of text, md, or html; only html writes rich clipboard
        data. Refuses when the focused element is a secure field.
        """
        if format not in _PASTE_FORMATS:
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                f"format must be one of text, md, html, got {format!r}",
                {"format": str(format)[:32]},
            )
        if not isinstance(text, str):
            raise ComputerUseError(
                "INVALID_ARGUMENT",
                f"text must be a string, got {type(text).__name__}",
                {"text": type(text).__name__},
            )
        await self._call("paste", {"text": text, "format": format})

    async def activate(self) -> None:
        """Bring the app's frontmost window to the foreground (a visible takeover).

        X11 raises ACTION_UNSUPPORTED; Wayland focuses the bound window through
        niri and fails with INJECTION_FAILED if focus does not land.
        """
        await self._call("activate")

    def is_frontmost(self) -> bool:
        """Report whether the app is the frontmost (key) application."""
        return bool(_request_blocking("computer_use.app", {"handle": self._handle, "method": "is_frontmost"}))


async def run() -> str:
    """Print a one-shot status summary (the skill's console entry point)."""
    state = await get_state(emit=False)
    permissions = state.get("permissions", {})
    running = [app for app in state.get("apps", []) if isinstance(app, dict) and app.get("running")]
    allowlist = state.get("allowlist", {})
    lines = [
        f"platform: {state.get('platform') or 'unavailable'}",
        f"accessibility: {permissions.get('accessibility', 'unknown')}",
        f"screen recording: {permissions.get('screen_recording', 'unknown')}",
        f"running apps: {len(running)}",
        f"allowlist: {len(allowlist.get('allowed', []))} allowed, {len(allowlist.get('blocked', []))} blocked",
    ]
    return "\n".join(lines)
