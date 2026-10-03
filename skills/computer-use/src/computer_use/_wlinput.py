"""Minimal Wayland client for the virtual pointer and virtual keyboard protocols.

Pure standard library: this module speaks the Wayland wire protocol over the
compositor's unix socket and binds exactly the globals the Wayland backend
needs for input - wl_seat, wl_output (for the output a pointer maps onto),
zwlr_virtual_pointer_manager_v1, and zwp_virtual_keyboard_manager_v1. niri
offers both virtual-input managers to every client outside a
security-context sandbox, so no uinput device, no root, and no daemon is
involved.

Semantics the backend relies on (verified against the niri and smithay
sources, not live):

- zwlr_virtual_pointer_v1.motion_absolute(x, y, x_extent, y_extent) maps
  onto the output the pointer was created for (create_virtual_pointer_with_output,
  manager v2), else onto the bounding rectangle of all outputs, in LOGICAL
  coordinates: position = (x * width / x_extent, y * height / y_extent) +
  output origin. So pointer motion is pixel-exact in logical space.
- zwp_virtual_keyboard_v1 uploads the client's OWN xkb keymap; every key is
  delivered to the surface that holds keyboard focus at that moment (the
  compositor swaps the keymap into the focused client, bypassing compositor
  keybindings). Input is therefore focus-bound, never window-targeted: the
  caller focuses and verifies the target window first. Because the keymap is
  ours, text typing is layout-independent: every character gets its own
  keycode bound to its Unicode keysym (the approach wtype uses).

Every session is one short-lived connection: bind, send, then a
wl_display.sync round trip so the compositor has processed every request
before the connection closes. Protocol errors raise INJECTION_FAILED; a
missing socket or missing globals raise ACTION_UNSUPPORTED naming the fix.
"""

from __future__ import annotations

import os
import socket
import struct
import sys
import time
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any, NamedTuple

from .errors import ComputerUseError

_TIMEOUT_SECONDS = 5.0
_ERROR_LIMIT = 200
_HEADER = struct.Struct("=II")
_UINT = struct.Struct("=I")
_INT = struct.Struct("=i")

POINTER_MANAGER = "zwlr_virtual_pointer_manager_v1"
KEYBOARD_MANAGER = "zwp_virtual_keyboard_manager_v1"

# wl_display
_DISPLAY_ID = 1
_DISPLAY_SYNC = 0
_DISPLAY_GET_REGISTRY = 1
_DISPLAY_EVENT_ERROR = 0
# wl_registry
_REGISTRY_BIND = 0
_REGISTRY_EVENT_GLOBAL = 0
# wl_callback
_CALLBACK_EVENT_DONE = 0
# wl_output
_OUTPUT_EVENT_NAME = 4
# zwlr_virtual_pointer_manager_v1
_POINTER_MANAGER_CREATE = 0
_POINTER_MANAGER_CREATE_WITH_OUTPUT = 2
# zwlr_virtual_pointer_v1
_POINTER_MOTION_ABSOLUTE = 1
_POINTER_BUTTON = 2
_POINTER_FRAME = 4
_POINTER_AXIS_SOURCE = 5
_POINTER_AXIS_DISCRETE = 7
_POINTER_DESTROY = 8
# zwp_virtual_keyboard_manager_v1 / zwp_virtual_keyboard_v1
_KEYBOARD_MANAGER_CREATE = 0
_KEYBOARD_KEYMAP = 0
_KEYBOARD_KEY = 1
_KEYBOARD_MODIFIERS = 2
_KEYBOARD_DESTROY = 3

_KEYMAP_FORMAT_XKB_V1 = 1
_KEY_PRESSED = 1
_KEY_RELEASED = 0
_BUTTON_PRESSED = 1
_BUTTON_RELEASED = 0
_AXIS_VERTICAL = 0
_AXIS_HORIZONTAL = 1
_AXIS_SOURCE_WHEEL = 0
_WHEEL_STEP = 15.0

BUTTONS: dict[str, int] = {"left": 0x110, "right": 0x111, "middle": 0x112}

# Real modifier masks of the generated keymap (xkb_types/xkb_compat "complete").
MODIFIER_MASKS: dict[str, int] = {"shift": 1, "ctrl": 4, "alt": 8, "cmd": 64}

_FIRST_KEYCODE = 9  # xkb keycode of our first key; the evdev code sent is keycode - 8
_MAX_KEYS_PER_KEYMAP = 240


class Global(NamedTuple):
    """One advertised registry global."""

    name: int
    interface: str
    version: int


def _socket_path() -> str:
    """Resolve the compositor socket from WAYLAND_DISPLAY and XDG_RUNTIME_DIR."""
    display = os.environ.get("WAYLAND_DISPLAY") or ""
    if not display:
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            "Wayland input is unavailable: WAYLAND_DISPLAY is not set",
            {"platform": "wayland"},
        )
    if os.path.isabs(display):
        return display
    runtime = os.environ.get("XDG_RUNTIME_DIR") or ""
    if not runtime:
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            "Wayland input is unavailable: XDG_RUNTIME_DIR is not set",
            {"platform": "wayland"},
        )
    return os.path.join(runtime, display)


def _string(value: str) -> bytes:
    """Encode one wire string: length with the NUL, bytes, NUL, padded to 4."""
    raw = value.encode("utf-8") + b"\x00"
    padding = (-len(raw)) % 4
    return _UINT.pack(len(raw)) + raw + b"\x00" * padding


def _fixed(value: float) -> bytes:
    """Encode one wl_fixed_t (signed 24.8)."""
    return _INT.pack(int(round(value * 256)))


def _u(*values: int) -> bytes:
    """Encode uint arguments."""
    return b"".join(_UINT.pack(value & 0xFFFFFFFF) for value in values)


def _read_string(payload: bytes, offset: int) -> tuple[str, int]:
    """Decode one wire string at offset, returning it and the next offset."""
    (length,) = _UINT.unpack_from(payload, offset)
    offset += 4
    raw = payload[offset : offset + length]
    offset += length + ((-length) % 4)
    return raw.rstrip(b"\x00").decode("utf-8", "replace"), offset


def _now_ms() -> int:
    """A millisecond timestamp for input events (wraps like the protocol's uint)."""
    return int(time.monotonic() * 1000) & 0xFFFFFFFF


class Connection:
    """One Wayland client connection: id allocation, requests, events, round trips."""

    def __init__(self, sock: Any) -> None:
        self._sock = sock
        self._next_id = 2
        self._buffer = b""
        self.globals: list[Global] = []
        self.output_names: dict[int, str] = {}
        self._registry = 0

    def new_id(self) -> int:
        """Allocate one client-side object id."""
        object_id = self._next_id
        self._next_id += 1
        return object_id

    def send(self, object_id: int, opcode: int, payload: bytes = b"", fds: tuple[int, ...] = ()) -> None:
        """Send one request, with file descriptors as SCM_RIGHTS ancillary data."""
        message = _HEADER.pack(object_id, ((8 + len(payload)) << 16) | opcode) + payload
        try:
            if fds:
                ancillary = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack(f"={len(fds)}i", *fds))]
                self._sock.sendmsg([message], ancillary)
            else:
                self._sock.sendall(message)
        except OSError as error:
            raise ComputerUseError(
                "INJECTION_FAILED", f"the Wayland connection failed: {str(error)[:_ERROR_LIMIT]}"
            ) from error

    def _events(self) -> Iterator[tuple[int, int, bytes]]:
        """Read and yield the next complete events, blocking for more bytes."""
        while True:
            while len(self._buffer) >= 8:
                object_id, word = _HEADER.unpack_from(self._buffer, 0)
                size = word >> 16
                if size < 8:
                    raise ComputerUseError("INJECTION_FAILED", "the compositor sent a malformed Wayland event")
                if len(self._buffer) < size:
                    break
                payload = self._buffer[8:size]
                self._buffer = self._buffer[size:]
                yield object_id, word & 0xFFFF, payload
            try:
                chunk = self._sock.recv(65536)
            except OSError as error:
                raise ComputerUseError(
                    "INJECTION_FAILED", f"the Wayland connection failed: {str(error)[:_ERROR_LIMIT]}"
                ) from error
            if not chunk:
                raise ComputerUseError("INJECTION_FAILED", "the compositor closed the Wayland connection")
            self._buffer += chunk

    def roundtrip(self) -> None:
        """Send wl_display.sync and dispatch events until its callback fires.

        Registry globals and wl_output names are collected on the way; a
        wl_display.error event raises INJECTION_FAILED with the compositor's
        message (a protocol error also ends the connection server-side).
        """
        callback = self.new_id()
        self.send(_DISPLAY_ID, _DISPLAY_SYNC, _u(callback))
        for object_id, opcode, payload in self._events():
            if object_id == callback and opcode == _CALLBACK_EVENT_DONE:
                return
            if object_id == _DISPLAY_ID and opcode == _DISPLAY_EVENT_ERROR:
                failed_object, code = struct.unpack_from("=II", payload, 0)
                message, _ = _read_string(payload, 8)
                raise ComputerUseError(
                    "INJECTION_FAILED",
                    f"the compositor rejected a Wayland request (object {failed_object}, code {code}): "
                    f"{message[:_ERROR_LIMIT]}",
                )
            if object_id == self._registry and opcode == _REGISTRY_EVENT_GLOBAL:
                name, offset = _UINT.unpack_from(payload, 0)[0], 4
                interface, offset = _read_string(payload, offset)
                (version,) = _UINT.unpack_from(payload, offset)
                self.globals.append(Global(name, interface, version))
            elif object_id in self.output_names and opcode == _OUTPUT_EVENT_NAME:
                self.output_names[object_id], _ = _read_string(payload, 0)

    def get_registry(self) -> None:
        """Create the registry and collect the advertised globals."""
        self._registry = self.new_id()
        self.send(_DISPLAY_ID, _DISPLAY_GET_REGISTRY, _u(self._registry))
        self.roundtrip()

    def find(self, interface: str) -> list[Global]:
        """Return the advertised globals for one interface."""
        return [entry for entry in self.globals if entry.interface == interface]

    def bind(self, entry: Global, version: int) -> int:
        """Bind one global at min(advertised, version), returning the new object id."""
        object_id = self.new_id()
        bound = min(entry.version, version)
        self.send(self._registry, _REGISTRY_BIND, _u(entry.name) + _string(entry.interface) + _u(bound, object_id))
        if entry.interface == "wl_output":
            self.output_names[object_id] = ""
        return object_id


def _open_socket(path: str) -> socket.socket:
    """Connect one stream socket to the compositor (the test seam)."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(_TIMEOUT_SECONDS)
    try:
        sock.connect(path)
    except OSError as error:
        sock.close()
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            f"Wayland input is unavailable: cannot connect to the compositor socket ({str(error)[:_ERROR_LIMIT]})",
            {"platform": "wayland"},
        ) from error
    return sock


@contextmanager
def _connect() -> Iterator[Connection]:
    """Open one compositor connection and its registry, closing it afterwards."""
    sock = _open_socket(_socket_path())
    try:
        connection = Connection(sock)
        connection.get_registry()
        yield connection
    finally:
        sock.close()


def _require(connection: Connection, interface: str) -> Global:
    """Return the one global the session needs, or refuse naming the gap."""
    found = connection.find(interface)
    if not found:
        raise ComputerUseError(
            "ACTION_UNSUPPORTED",
            f"the compositor does not offer {interface} to this client (a sandboxed security "
            "context hides it); Wayland pointer and keyboard input are unavailable",
            {"platform": "wayland", "interface": interface},
        )
    return found[0]


def _available() -> dict[str, bool]:
    """Report which virtual-input managers the compositor advertises (no input is sent)."""
    with _connect() as connection:
        return {
            "pointer": bool(connection.find(POINTER_MANAGER)) and bool(connection.find("wl_seat")),
            "keyboard": bool(connection.find(KEYBOARD_MANAGER)) and bool(connection.find("wl_seat")),
        }


class PointerTarget(NamedTuple):
    """Where the pointer maps: one output by name, in that output's logical size."""

    output_name: str | None
    width: int
    height: int


@contextmanager
def _pointer(target: PointerTarget) -> Iterator[_PointerDevice]:
    """Create one virtual pointer mapped onto the named output (or all outputs)."""
    with _connect() as connection:
        manager_global = _require(connection, POINTER_MANAGER)
        seat = connection.bind(_require(connection, "wl_seat"), 1)
        manager = connection.bind(manager_global, 2)
        output_id = 0
        if target.output_name is not None and manager_global.version >= 2:
            outputs = [connection.bind(entry, 4) for entry in connection.find("wl_output") if entry.version >= 4]
            connection.roundtrip()
            output_id = next(
                (object_id for object_id in outputs if connection.output_names.get(object_id) == target.output_name),
                0,
            )
            if not output_id:
                raise ComputerUseError(
                    "ACTION_UNSUPPORTED",
                    f"the compositor did not advertise output {target.output_name!r}; cannot map the pointer onto it",
                    {"platform": "wayland"},
                )
        elif target.output_name is not None:
            raise ComputerUseError(
                "ACTION_UNSUPPORTED",
                "the compositor's virtual pointer manager predates per-output pointers (v2); "
                "coordinate input cannot be mapped exactly",
                {"platform": "wayland"},
            )
        device = connection.new_id()
        if output_id:
            connection.send(manager, _POINTER_MANAGER_CREATE_WITH_OUTPUT, _u(seat, output_id, device))
        else:
            connection.send(manager, _POINTER_MANAGER_CREATE, _u(seat, device))
        pointer = _PointerDevice(connection, device, target)
        try:
            yield pointer
        finally:
            connection.send(device, _POINTER_DESTROY)
        connection.roundtrip()


class _PointerDevice:
    """Requests on one created zwlr_virtual_pointer_v1."""

    def __init__(self, connection: Connection, object_id: int, target: PointerTarget) -> None:
        self._connection = connection
        self._id = object_id
        self._target = target

    def move(self, x: float, y: float) -> None:
        """Move to one point in the target's logical coordinates."""
        width, height = self._target.width, self._target.height
        px = min(max(int(round(x)), 0), width - 1)
        py = min(max(int(round(y)), 0), height - 1)
        self._connection.send(self._id, _POINTER_MOTION_ABSOLUTE, _u(_now_ms(), px, py, width, height))
        self._connection.send(self._id, _POINTER_FRAME)

    def button(self, code: int, pressed: bool) -> None:
        """Press or release one button."""
        state = _BUTTON_PRESSED if pressed else _BUTTON_RELEASED
        self._connection.send(self._id, _POINTER_BUTTON, _u(_now_ms(), code, state))
        self._connection.send(self._id, _POINTER_FRAME)

    def wheel(self, direction: str, clicks: int) -> None:
        """Send discrete wheel clicks in one direction (down/right positive)."""
        axis = _AXIS_VERTICAL if direction in ("up", "down") else _AXIS_HORIZONTAL
        sign = 1 if direction in ("down", "right") else -1
        for _ in range(clicks):
            self._connection.send(self._id, _POINTER_AXIS_SOURCE, _u(_AXIS_SOURCE_WHEEL))
            self._connection.send(
                self._id,
                _POINTER_AXIS_DISCRETE,
                _u(_now_ms(), axis) + _fixed(sign * _WHEEL_STEP) + _INT.pack(sign),
            )
            self._connection.send(self._id, _POINTER_FRAME)


def click(target: PointerTarget, point: tuple[float, float], button: str, count: int) -> None:
    """Move to point (target-logical) and click button count times."""
    code = BUTTONS[button]
    with _pointer(target) as pointer:
        pointer.move(*point)
        for _ in range(count):
            pointer.button(code, True)
            pointer.button(code, False)


def drag(target: PointerTarget, start: tuple[float, float], end: tuple[float, float], steps: int = 12) -> None:
    """Press at start, move through intermediate points to end, release."""
    code = BUTTONS["left"]
    with _pointer(target) as pointer:
        pointer.move(*start)
        pointer.button(code, True)
        for step in range(1, steps + 1):
            fraction = step / steps
            pointer.move(start[0] + (end[0] - start[0]) * fraction, start[1] + (end[1] - start[1]) * fraction)
        pointer.button(code, False)


def scroll(target: PointerTarget, point: tuple[float, float] | None, direction: str, clicks: int) -> None:
    """Optionally move to point, then send discrete wheel clicks."""
    with _pointer(target) as pointer:
        if point is not None:
            pointer.move(*point)
        pointer.wheel(direction, clicks)


def _keymap_text(keysyms: list[str]) -> str:
    """Build an xkb keymap binding keycodes 9.. to one keysym each."""
    last = _FIRST_KEYCODE + max(len(keysyms), 1) - 1
    codes = "".join(f"<K{index}> = {_FIRST_KEYCODE + index};\n" for index in range(len(keysyms)))
    symbols = "".join(f"key <K{index}> {{[ {keysym} ]}};\n" for index, keysym in enumerate(keysyms))
    return (
        "xkb_keymap {\n"
        f'xkb_keycodes "(unnamed)" {{\nminimum = 8;\nmaximum = {max(last, 9)};\n{codes}}};\n'
        'xkb_types "(unnamed)" { include "complete" };\n'
        'xkb_compat "(unnamed)" { include "complete" };\n'
        f'xkb_symbols "(unnamed)" {{\n{symbols}}};\n'
        "};\n"
    )


def _keymap_fd(text: str) -> tuple[int, int]:
    """Write the keymap (NUL-terminated) to a sealed-size memfd; returns (fd, size)."""
    data = text.encode("utf-8") + b"\x00"
    if sys.platform.startswith("linux") and hasattr(os, "memfd_create"):
        fd = os.memfd_create("prime-agent-keymap", os.MFD_CLOEXEC)
    else:  # pragma: no cover - the Wayland backend is Linux-only
        import tempfile

        fd, name = tempfile.mkstemp()
        os.unlink(name)
    os.write(fd, data)
    os.lseek(fd, 0, os.SEEK_SET)
    return fd, len(data)


def keysym_for_char(character: str) -> str:
    """Name the keysym that types one character (newline is Return, tab is Tab)."""
    if character == "\n":
        return "Return"
    if character == "\t":
        return "Tab"
    if character.isascii() and character.isalnum():
        return character
    codepoint = ord(character)
    if codepoint < 0x20 or 0x7F <= codepoint < 0xA0:
        raise ComputerUseError(
            "INVALID_ARGUMENT",
            f"cannot type control character U+{codepoint:04X}",
            {"codepoint": codepoint},
        )
    return f"U{codepoint:04X}"


class KeyStroke(NamedTuple):
    """One key to press and release with the given modifiers held."""

    keysym: str
    modifiers: int = 0


def send_keys(strokes: list[KeyStroke]) -> None:
    """Deliver key strokes to whatever surface holds keyboard focus.

    The strokes are grouped so each uploaded keymap binds at most
    _MAX_KEYS_PER_KEYMAP distinct keysyms; each group uploads its keymap,
    then presses and releases each key with its modifier mask set around it.
    """
    if not strokes:
        return
    with _connect() as connection:
        manager = connection.bind(_require(connection, KEYBOARD_MANAGER), 1)
        seat = connection.bind(_require(connection, "wl_seat"), 1)
        keyboard = connection.new_id()
        connection.send(manager, _KEYBOARD_MANAGER_CREATE, _u(seat, keyboard))
        try:
            for group in _groups(strokes):
                keysyms = list(dict.fromkeys(stroke.keysym for stroke in group))
                fd, size = _keymap_fd(_keymap_text(keysyms))
                try:
                    # the fd argument travels as SCM_RIGHTS ancillary data, so
                    # the payload carries only format and size
                    connection.send(keyboard, _KEYBOARD_KEYMAP, _u(_KEYMAP_FORMAT_XKB_V1, size), fds=(fd,))
                finally:
                    os.close(fd)
                for stroke in group:
                    code = keysyms.index(stroke.keysym) + _FIRST_KEYCODE - 8
                    if stroke.modifiers:
                        connection.send(keyboard, _KEYBOARD_MODIFIERS, _u(stroke.modifiers, 0, 0, 0))
                    connection.send(keyboard, _KEYBOARD_KEY, _u(_now_ms(), code, _KEY_PRESSED))
                    connection.send(keyboard, _KEYBOARD_KEY, _u(_now_ms(), code, _KEY_RELEASED))
                    if stroke.modifiers:
                        connection.send(keyboard, _KEYBOARD_MODIFIERS, _u(0, 0, 0, 0))
                connection.roundtrip()
        finally:
            connection.send(keyboard, _KEYBOARD_DESTROY)
        connection.roundtrip()


def _groups(strokes: list[KeyStroke]) -> Iterator[list[KeyStroke]]:
    """Split strokes into runs whose distinct keysyms fit one keymap."""
    group: list[KeyStroke] = []
    seen: set[str] = set()
    for stroke in strokes:
        if stroke.keysym not in seen and len(seen) >= _MAX_KEYS_PER_KEYMAP:
            yield group
            group, seen = [], set()
        group.append(stroke)
        seen.add(stroke.keysym)
    if group:
        yield group
