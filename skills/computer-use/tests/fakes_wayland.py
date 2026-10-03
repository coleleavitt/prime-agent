"""Fakes and fixtures for the Wayland (niri) backend tests.

Nothing here touches a real session: niri IPC is a scripted JSON-lines
server injected through _wayland._niri_transport (the reply shapes follow
`niri msg --json` on niri 26.04: windows carry id, title, app_id, pid,
workspace_id, is_focused, is_floating, layout, focus_timestamp), AT-SPI is a
fake object graph injected through _wayland._atspi (no a11y bus), virtual
input is recorded at the computer_use._wlinput seam, and grim runs through
the scripted subprocess seam from fakes_linux.
"""

from __future__ import annotations

import contextlib
import copy
import json
import os
import tempfile
import types
from typing import Any, Iterator
from unittest import mock

import fakes_linux  # noqa: F401 - pins sys.path to this worktree's src tree
from fakes_linux import X11Script

OUTPUTS: dict[str, Any] = {
    "eDP-1": {"name": "eDP-1", "logical": {"x": 0, "y": 0, "width": 1920, "height": 1200, "scale": 2.0}},
    "HDMI-A-1": {"name": "HDMI-A-1", "logical": {"x": 1920, "y": 0, "width": 2560, "height": 1440, "scale": 1.0}},
}

WORKSPACES: list[dict[str, Any]] = [
    {"id": 1, "idx": 1, "output": "eDP-1", "is_active": True, "is_focused": True},
    {"id": 2, "idx": 1, "output": "HDMI-A-1", "is_active": True, "is_focused": False},
    {"id": 3, "idx": 2, "output": "eDP-1", "is_active": False, "is_focused": False},
]


def niri_window(
    window_id: int,
    *,
    app_id: str,
    title: str,
    pid: int,
    workspace_id: int = 1,
    focused: bool = False,
    floating: bool = False,
    tile_pos: tuple[float, float] | None = None,
    offset: tuple[float, float] = (0.0, 0.0),
    size: tuple[int, int] = (800, 600),
    focus_secs: int = 100,
) -> dict[str, Any]:
    """Build one niri window record in the IPC shape."""
    return {
        "id": window_id,
        "title": title,
        "app_id": app_id,
        "pid": pid,
        "workspace_id": workspace_id,
        "is_focused": focused,
        "is_floating": floating,
        "is_urgent": False,
        "layout": {
            "pos_in_scrolling_layout": None if floating else [1, 1],
            "tile_size": [float(size[0]), float(size[1])],
            "window_size": [size[0], size[1]],
            "tile_pos_in_workspace_view": list(tile_pos) if tile_pos is not None else None,
            "window_offset_in_tile": list(offset),
        },
        "focus_timestamp": {"secs": focus_secs, "nanos": 0},
    }


def default_windows() -> list[dict[str, Any]]:
    """The default session: a floating editor (focused), its tiled second window, a terminal, a floating app on HDMI."""
    return [
        niri_window(10, app_id="org.gnome.TextEditor", title="Doc - Text Editor", pid=501, focused=True,
                    floating=True, tile_pos=(100.0, 50.0), offset=(4.0, 6.0), focus_secs=300),
        niri_window(11, app_id="org.gnome.TextEditor", title="Other doc", pid=501, focus_secs=200),
        niri_window(20, app_id="foot", title="shell", pid=600, focus_secs=100),
        niri_window(30, app_id="org.example.Floaty", title="Floaty", pid=700, workspace_id=2,
                    floating=True, tile_pos=(10.0, 20.0), offset=(2.0, 3.0), size=(400, 300), focus_secs=50),
    ]


class FakeNiri:
    """Scripted niri IPC: answers request lines and records them.

    focus_lands=False makes FocusWindow actions leave focus where it was, so
    the backend's focus verification can be exercised.
    """

    def __init__(self, windows: list[dict[str, Any]] | None = None) -> None:
        self.windows = windows if windows is not None else default_windows()
        self.workspaces = copy.deepcopy(WORKSPACES)
        self.outputs = copy.deepcopy(OUTPUTS)
        self.requests: list[Any] = []
        self.focus_lands = True
        self.fail: str | None = None

    def focused_id(self) -> int | None:
        return next((window["id"] for window in self.windows if window.get("is_focused")), None)

    def focus(self, window_id: int) -> None:
        for window in self.windows:
            window["is_focused"] = window["id"] == window_id

    def actions(self) -> list[Any]:
        return [request for request in self.requests if isinstance(request, dict) and "Action" in request]

    def __call__(self, line: bytes) -> bytes:
        assert line.endswith(b"\n")
        request = json.loads(line)
        self.requests.append(request)
        if self.fail is not None:
            return (json.dumps({"Err": self.fail}) + "\n").encode()
        if request == "Windows":
            ok: Any = {"Windows": self.windows}
        elif request == "Workspaces":
            ok = {"Workspaces": self.workspaces}
        elif request == "Outputs":
            ok = {"Outputs": self.outputs}
        elif request == "FocusedWindow":
            focused = next((window for window in self.windows if window.get("is_focused")), None)
            ok = {"FocusedWindow": focused}
        elif isinstance(request, dict) and "Action" in request:
            target = request["Action"]["FocusWindow"]["id"]
            if self.focus_lands:
                self.focus(target)
            ok = "Handled"
        else:
            return b'{"Err":"error parsing request"}\n'
        return (json.dumps({"Ok": ok}) + "\n").encode()


# --- fake AT-SPI ----------------------------------------------------------------


class _Enum:
    """A named enum member, compared by identity like the gi enums."""

    def __init__(self, name: str) -> None:
        self.name = name

    def __repr__(self) -> str:
        return f"<{self.name}>"


ROLES = {name: _Enum(name) for name in ("FRAME", "PUSH_BUTTON", "ENTRY", "PASSWORD_TEXT", "PANEL", "LABEL", "MENU", "MENU_ITEM", "APPLICATION", "DIALOG")}
ROLE_NAMES = {
    "FRAME": "frame",
    "PUSH_BUTTON": "push button",
    "ENTRY": "entry",
    "PASSWORD_TEXT": "password text",
    "PANEL": "panel",
    "LABEL": "label",
    "MENU": "menu",
    "MENU_ITEM": "menu item",
    "APPLICATION": "application",
    "DIALOG": "dialog",
}
STATES = {name: _Enum(name) for name in ("FOCUSED", "ACTIVE", "EDITABLE", "SHOWING", "VISIBLE")}


class FakeStateSet:
    def __init__(self, names: set[str]) -> None:
        self.names = names

    def contains(self, state: _Enum) -> bool:
        return state.name in self.names


class FakeRect:
    def __init__(self, x: int, y: int, width: int, height: int) -> None:
        self.x, self.y, self.width, self.height = x, y, width, height


class FakeAccessible:
    """One fake Atspi.Accessible with the interface methods the backend calls.

    reads records every text read so tests can assert a secure field's value
    is never read; broken=True makes every call raise like a defunct object.
    """

    def __init__(
        self,
        role: str,
        name: str | None = None,
        *,
        children: list[FakeAccessible] | None = None,
        states: set[str] | None = None,
        actions: list[str] | None = None,
        text: str | None = None,
        editable: bool = False,
        extents: tuple[int, int, int, int] | None = None,
        description: str | None = None,
        pid: int | None = None,
        value: float | None = None,
    ) -> None:
        self.role = role
        self.name = name
        self.children = children or []
        self.states = states if states is not None else {"SHOWING", "VISIBLE"}
        self.actions = actions or []
        self.text = text
        self.editable = editable
        self.extents = extents
        self.description = description
        self.pid = pid
        self.value = value
        self.broken = False
        self.reads: list[str] = []
        self.performed: list[str] = []
        self.writes: list[str] = []
        self.selections: list[tuple[int, int]] = []
        self.do_action_result = True

    def _check(self) -> None:
        if self.broken:
            raise RuntimeError("defunct accessible")

    def get_process_id(self) -> int | None:
        self._check()
        return self.pid

    def get_child_count(self) -> int:
        self._check()
        return len(self.children)

    def get_child_at_index(self, index: int) -> FakeAccessible:
        self._check()
        return self.children[index]

    def get_role(self) -> _Enum:
        self._check()
        return ROLES[self.role]

    def get_role_name(self) -> str:
        self._check()
        return ROLE_NAMES[self.role]

    def get_name(self) -> str | None:
        self._check()
        return self.name

    def get_description(self) -> str | None:
        self._check()
        return self.description

    def get_state_set(self) -> FakeStateSet:
        self._check()
        return FakeStateSet(self.states)

    def get_action_iface(self) -> Any:
        self._check()
        if not self.actions:
            return None
        node = self
        return types.SimpleNamespace(
            get_n_actions=lambda: len(node.actions),
            get_action_name=lambda index: node.actions[index],
            do_action=lambda index: (node.performed.append(node.actions[index]), node.do_action_result)[1],
        )

    def get_component_iface(self) -> Any:
        self._check()
        if self.extents is None:
            return None
        node = self

        def get_extents(coord: Any) -> FakeRect:
            assert coord is FakeAtspi.CoordType.WINDOW, "the backend reads WINDOW-relative extents"
            return FakeRect(*node.extents)

        return types.SimpleNamespace(get_extents=get_extents)

    def get_text_iface(self) -> Any:
        self._check()
        if self.text is None:
            return None
        node = self

        def get_text(start: int, end: int) -> str:
            node.reads.append("text")
            return node.text[start:end]

        def set_selection(number: int, start: int, end: int) -> bool:
            node.selections.append((start, end))
            return True

        def add_selection(start: int, end: int) -> bool:
            node.selections.append((start, end))
            return True

        return types.SimpleNamespace(
            get_character_count=lambda: len(node.text),
            get_text=get_text,
            get_n_selections=lambda: len(node.selections),
            set_selection=set_selection,
            add_selection=add_selection,
        )

    def get_editable_text_iface(self) -> Any:
        self._check()
        if not self.editable:
            return None
        node = self

        def set_text_contents(value: str) -> bool:
            node.writes.append(value)
            node.text = value
            return True

        return types.SimpleNamespace(set_text_contents=set_text_contents)

    def get_value_iface(self) -> Any:
        self._check()
        if self.value is None:
            return None
        node = self
        return types.SimpleNamespace(get_current_value=lambda: node.value)


class FakeDesktop(FakeAccessible):
    def __init__(self, apps: list[FakeAccessible]) -> None:
        super().__init__("FRAME", "main", children=apps)


class FakeAtspi:
    """The fake Atspi module: enums, get_desktop, init, set_timeout."""

    Role = types.SimpleNamespace(**ROLES)
    StateType = types.SimpleNamespace(**STATES)
    CoordType = types.SimpleNamespace(WINDOW=_Enum("WINDOW"), SCREEN=_Enum("SCREEN"))

    def __init__(self, apps: list[FakeAccessible]) -> None:
        self.desktop = FakeDesktop(apps)

    def get_desktop(self, index: int) -> FakeDesktop:
        assert index == 0
        return self.desktop


def editor_app() -> FakeAccessible:
    """The fake AT-SPI tree of the editor (pid 501): two frames, the first matching window 10."""
    save = FakeAccessible("PUSH_BUTTON", "Save", actions=["click"], extents=(10, 10, 80, 30))
    search = FakeAccessible(
        "ENTRY", "Search", text="hello world hello", editable=True,
        states={"SHOWING", "VISIBLE", "EDITABLE", "FOCUSED"}, extents=(100, 10, 200, 30),
    )
    password = FakeAccessible(
        "PASSWORD_TEXT", "Password", text="hunter2", editable=True,
        states={"SHOWING", "VISIBLE", "EDITABLE"}, extents=(100, 50, 200, 30),
    )
    label = FakeAccessible("LABEL", "Status", text="Status", extents=(10, 100, 50, 20))
    panel = FakeAccessible("PANEL", None, children=[label], extents=(0, 90, 400, 40))
    hidden_item = FakeAccessible("MENU_ITEM", "Quit", actions=["click"], states={"VISIBLE"})
    hidden_menu = FakeAccessible("MENU", "File", children=[hidden_item], states={"VISIBLE"})
    frame = FakeAccessible(
        "FRAME", "Doc - Text Editor", children=[save, search, password, panel, hidden_menu],
        states={"SHOWING", "VISIBLE", "ACTIVE"}, extents=(0, 0, 800, 600),
    )
    other = FakeAccessible("FRAME", "Other doc", children=[], extents=(0, 0, 800, 600))
    return FakeAccessible("APPLICATION", "gnome-text-editor", children=[frame, other], pid=501)


def floaty_app() -> FakeAccessible:
    """The fake AT-SPI tree of the floating app on HDMI (pid 700)."""
    ok = FakeAccessible("PUSH_BUTTON", "OK", actions=["press"], extents=(20, 30, 60, 20))
    frame = FakeAccessible("FRAME", "Floaty", children=[ok], states={"SHOWING", "VISIBLE"}, extents=(0, 0, 400, 300))
    return FakeAccessible("APPLICATION", "floaty", children=[frame], pid=700)


class InputRecorder:
    """Records every virtual-input call the backend makes at the _wlinput seam."""

    def __init__(self, niri: FakeNiri) -> None:
        self.niri = niri
        self.calls: list[tuple[Any, ...]] = []

    def _focused(self) -> int | None:
        return self.niri.focused_id()

    def click(self, target: Any, point: Any, button: str, count: int) -> None:
        self.calls.append(("click", target, point, button, count, self._focused()))

    def drag(self, target: Any, start: Any, end: Any) -> None:
        self.calls.append(("drag", target, start, end, self._focused()))

    def scroll(self, target: Any, point: Any, direction: str, clicks: int) -> None:
        self.calls.append(("scroll", target, point, direction, clicks, self._focused()))

    def send_keys(self, strokes: list[Any]) -> None:
        self.calls.append(("keys", list(strokes), self._focused()))

    def available(self) -> dict[str, bool]:
        return {"pointer": True, "keyboard": True}


@contextlib.contextmanager
def fake_wayland(
    niri: FakeNiri | None = None,
    atspi: FakeAtspi | None = None,
    script: X11Script | None = None,
    tools: tuple[str, ...] = ("grim", "loginctl"),
) -> Iterator[types.SimpleNamespace]:
    """Patch the _wayland module's seams: niri IPC, AT-SPI, virtual input, tools."""
    from computer_use import _wayland, _wlinput

    niri = niri or FakeNiri()
    atspi = atspi or FakeAtspi([editor_app(), floaty_app()])
    script = script or X11Script()
    recorder = InputRecorder(niri)
    patchers = [
        mock.patch.object(_wayland, "_niri_transport", niri),
        mock.patch.object(_wayland, "_atspi", lambda: atspi),
        mock.patch.object(_wayland, "_FOCUS_WAIT_SECONDS", 0.05),
        mock.patch.object(_wayland.subprocess, "run", script),
        mock.patch.object(_wayland, "_TOOL_PATHS", {}),
        mock.patch.object(_wayland.shutil, "which", lambda name: f"/usr/bin/{name}" if name in tools else None),
        mock.patch.object(_wlinput, "_click", recorder.click),
        mock.patch.object(_wlinput, "_drag", recorder.drag),
        mock.patch.object(_wlinput, "_scroll", recorder.scroll),
        mock.patch.object(_wlinput, "_send_keys", recorder.send_keys),
        mock.patch.object(_wlinput, "_available", recorder.available),
    ]
    for patcher in patchers:
        patcher.start()
    try:
        yield types.SimpleNamespace(niri=niri, atspi=atspi, script=script, input=recorder)
    finally:
        for patcher in patchers:
            patcher.stop()


@contextlib.contextmanager
def wayland_app_environment(
    niri: FakeNiri | None = None,
    atspi: FakeAtspi | None = None,
    *,
    allowed: tuple[str, ...] = ("org.gnome.TextEditor", "org.example.Floaty", "foot"),
    blocked: tuple[str, ...] = (),
) -> Iterator[types.SimpleNamespace]:
    """Patch the App layer for Wayland end-to-end tests on the fake session.

    backend() reports wayland, the compat loader returns the real _wayland
    module running on the fakes, policy reads a fixture settings file, the
    screen reads unlocked, and App-layer module state resets around the test.
    """
    import computer_use
    from computer_use import _wayland, policy

    import fakes

    settings_tmp = tempfile.TemporaryDirectory()
    settings_path = fakes.write_settings(settings_tmp.name, allowed=allowed, blocked=blocked)
    with fake_wayland(niri, atspi) as env:
        patchers = [
            mock.patch.object(computer_use, "_backend", lambda: "wayland"),
            mock.patch.object(computer_use, "_require_wayland", lambda: _wayland),
            mock.patch.object(policy, "SETTINGS_PATH", settings_path),
            mock.patch.object(policy, "_screen_locked", lambda: False),
            mock.patch.dict(computer_use._bound_apps, {}, clear=True),
            mock.patch.object(computer_use, "_instruction_shown", set()),
            mock.patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": settings_tmp.name}),
        ]
        for patcher in patchers:
            patcher.start()
        try:
            yield env
        finally:
            for patcher in patchers:
                patcher.stop()
            settings_tmp.cleanup()
