"""Pure fake tests for the Wayland (niri) backend (_wayland.py, _wlinput.py).

Nothing here touches the real session: niri IPC is a scripted JSON-lines
fake, AT-SPI is a fake object graph, virtual input is recorded at the
_wlinput seam (and the wire protocol runs against an in-process fake
compositor over a socketpair), grim runs through the scripted subprocess
seam. Live smokes are read-only and gated behind PRIME_CUA_LIVE=1 plus a
niri session.
"""

from __future__ import annotations

import array
import asyncio
import ctypes
import ctypes.util
import os
import socket
import stat
import struct
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

import fakes
import fakes_wayland
from fakes_linux import X11Script
from fakes_wayland import FakeAccessible, FakeAtspi, FakeNiri, editor_app, fake_wayland, floaty_app, niri_window
import computer_use
from computer_use import _compat, _wayland, _wlinput, diff, policy
from computer_use.errors import ComputerUseError

LIVE = (
    os.environ.get("PRIME_CUA_LIVE") == "1"
    and bool(os.environ.get("WAYLAND_DISPLAY"))
    and bool(os.environ.get("NIRI_SOCKET"))
)
LIVE_SKIP = "live smoke; set PRIME_CUA_LIVE=1 inside a niri Wayland session to enable"

SEAMS = (
    "_click",
    "_current_value",
    "_drag",
    "_focus_window",
    "_focused_is_secure",
    "_is_frontmost",
    "_is_settable",
    "_list_apps",
    "_live_fingerprint",
    "_live_is_secure",
    "_observe",
    "_perform_action",
    "_press_key",
    "_resolve_app",
    "_screen_locked",
    "_screenshot_window",
    "_scroll",
    "_select_text_range",
    "_set_value",
    "_status",
    "_type_text",
    "_window",
    "_window_fingerprint",
)


def run(coroutine: Any) -> Any:
    return asyncio.run(coroutine)


def element_named(observation: Any, name: str) -> int:
    from computer_use import ax

    return next(index for index, element in enumerate(ax._flatten(observation.tree)) if element["title"] == name)


# --- backend selection -------------------------------------------------------------


class SelectionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.sock_path = os.path.join(self.tmp.name, "niri.sock")
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(self.sock_path)
        self.addCleanup(self.listener.close)

    def backend(self, env: dict[str, str], tools: tuple[str, ...] = ("xdotool",)) -> str | None:
        with mock.patch.object(_compat.sys, "platform", "linux"), mock.patch.dict(os.environ, env, clear=True), mock.patch.object(
            _compat.shutil, "which", lambda name: f"/usr/bin/{name}" if name in tools else None
        ):
            return _compat._backend()

    def test_niri_session_selects_wayland_ahead_of_x11(self) -> None:
        self.assertEqual(
            self.backend({"WAYLAND_DISPLAY": "wayland-1", "NIRI_SOCKET": self.sock_path, "DISPLAY": ":0"}),
            "wayland",
        )

    def test_without_niri_the_x11_selection_is_unchanged(self) -> None:
        self.assertEqual(self.backend({"WAYLAND_DISPLAY": "wayland-1", "DISPLAY": ":0"}), "linux")
        self.assertEqual(self.backend({"NIRI_SOCKET": self.sock_path, "DISPLAY": ":0"}), "linux")
        self.assertIsNone(self.backend({"WAYLAND_DISPLAY": "wayland-1"}, tools=()))

    def test_a_stale_or_non_socket_niri_path_is_not_niri(self) -> None:
        regular = os.path.join(self.tmp.name, "plain")
        Path(regular).write_text("x")
        self.assertEqual(self.backend({"WAYLAND_DISPLAY": "w", "NIRI_SOCKET": regular}), "linux")
        self.assertEqual(self.backend({"WAYLAND_DISPLAY": "w", "NIRI_SOCKET": regular + ".gone"}), "linux")

    def test_darwin_still_wins(self) -> None:
        with mock.patch.object(_compat.sys, "platform", "darwin"):
            self.assertEqual(_compat._backend(), "mac")

    def test_require_wayland(self) -> None:
        with mock.patch.object(_compat, "_backend", return_value="linux"):
            with self.assertRaises(ComputerUseError) as caught:
                _compat._require_wayland()
        self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")
        self.assertIn("niri", caught.exception.message)
        with mock.patch.object(_compat, "_backend", return_value="wayland"):
            loaded = _compat._require_wayland()
        self.assertIs(loaded, _wayland)
        for name in SEAMS:
            with self.subTest(seam=name):
                self.assertTrue(callable(getattr(loaded, name, None)))


# --- niri IPC ----------------------------------------------------------------------


class NiriIpcTests(unittest.TestCase):
    def test_transport_speaks_json_lines_over_the_socket(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "niri.sock")
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(path)
            server.listen(1)
            received: list[bytes] = []

            def serve() -> None:
                connection, _ = server.accept()
                with connection:
                    data = b""
                    while not data.endswith(b"\n"):
                        data += connection.recv(4096)
                    received.append(data)
                    connection.sendall(b'{"Ok":{"FocusedWindow":null}}\n')

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            with mock.patch.dict(os.environ, {"NIRI_SOCKET": path}):
                self.assertIsNone(_wayland._focused_window_id())
            thread.join(2)
            server.close()
        self.assertEqual(received, [b'"FocusedWindow"\n'])

    def test_missing_socket_env_is_transport_error(self) -> None:
        with mock.patch.dict(os.environ, {"NIRI_SOCKET": ""}):
            with self.assertRaises(ComputerUseError) as caught:
                _wayland._niri("Windows")
        self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")

    def test_err_and_garbage_replies_are_transport_errors(self) -> None:
        for reply in (b'{"Err":"nope"}\n', b"not json\n", b'{"Weird":1}\n'):
            with self.subTest(reply=reply), mock.patch.object(_wayland, "_niri_transport", lambda line, reply=reply: reply):
                with self.assertRaises(ComputerUseError) as caught:
                    _wayland._windows()
                self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")

    def test_list_apps_dedups_app_ids(self) -> None:
        with fake_wayland():
            apps = _wayland._list_apps()
        self.assertEqual(
            apps,
            [
                {"id": "org.gnome.TextEditor", "name": "org.gnome.TextEditor", "running": True},
                {"id": "foot", "name": "foot", "running": True},
                {"id": "org.example.Floaty", "name": "org.example.Floaty", "running": True},
            ],
        )

    def test_resolve_orders_focused_then_recent_and_casefolds(self) -> None:
        niri = FakeNiri()
        niri.focus(20)
        with fake_wayland(niri):
            self.assertEqual([w["id"] for w in _wayland._resolve_app("ORG.GNOME.TEXTEDITOR")], [10, 11])
            self.assertEqual([w["id"] for w in _wayland._resolve_app({"bundle_id": "foot"})], [20])
            self.assertEqual(_wayland._resolve_app("nope"), [])
            for spec in ("", {"path": "/x"}, 3):
                with self.subTest(spec=spec), self.assertRaises(ComputerUseError) as caught:
                    _wayland._resolve_app(spec)  # type: ignore[arg-type]
                self.assertEqual(caught.exception.code, "INVALID_ARGUMENT")

    def test_geometry_for_floating_windows_on_active_workspaces(self) -> None:
        with fake_wayland() as env:
            editor = _wayland._geometry(_wayland._window(10))
            floaty = _wayland._geometry(_wayland._window(30))
            tiled = _wayland._geometry(_wayland._window(11))
            env.niri.windows[0]["workspace_id"] = 3  # an inactive workspace
            hidden = _wayland._geometry(_wayland._window(10))
        self.assertEqual(_wayland._rect(editor), (104.0, 56.0, 800.0, 600.0))
        self.assertEqual(editor.output, "eDP-1")
        self.assertEqual(_wayland._rect(floaty), (1932.0, 23.0, 400.0, 300.0))
        self.assertEqual(floaty.output_rect, (1920.0, 0.0, 2560.0, 1440.0))
        self.assertIsNone(_wayland._rect(tiled))
        self.assertIn("only for floating windows", tiled.reason)
        self.assertIsNone(_wayland._rect(hidden))
        self.assertIn("not on screen", hidden.reason)


# --- observation ---------------------------------------------------------------------


class ObserveTests(unittest.TestCase):
    def test_observe_maps_atspi_into_the_element_contract(self) -> None:
        with fake_wayland():
            observation = _wayland._observe(10)
        self.assertEqual(observation.window_title, "Doc - Text Editor")
        self.assertEqual(observation.window_rect, (104.0, 56.0, 800.0, 600.0))
        self.assertEqual(observation.window_id, 10)
        self.assertFalse(observation.truncated)
        self.assertEqual(len(observation.refs), 5)  # the hidden menu and its item are skipped
        save, search, password, panel = observation.tree
        self.assertEqual(
            save,
            {
                "role": "push button",
                "subrole": None,
                "title": "Save",
                "value": None,
                "description": None,
                "placeholder": None,
                "actions": ["click"],
                "position": [10.0, 10.0],
                "size": [80.0, 30.0],
                "children": [],
            },
        )
        self.assertEqual(search["value"], "hello world hello")
        self.assertEqual(password["role"], "password text")
        self.assertIsNone(password["value"])
        self.assertIsNone(panel["children"][0]["value"])  # a label's text repeats its name
        self.assertEqual(observation.focused_index, 1)

    def test_a_password_value_is_never_read_or_rendered(self) -> None:
        app = editor_app()
        password = app.children[0].children[2]
        with fake_wayland(atspi=FakeAtspi([app])):
            observation = _wayland._observe(10)
        self.assertEqual(password.reads, [])
        lines = diff._serialize(observation.tree)
        self.assertIn("[2] password text 'Password' [secure] @ (100, 50) 200x30", lines)
        self.assertNotIn("hunter2", "\n".join(lines))

    def test_the_frame_is_matched_by_title(self) -> None:
        with fake_wayland():
            observation = _wayland._observe(11)
        self.assertEqual(observation.window_title, "Other doc")
        self.assertEqual(observation.tree, [])
        self.assertIsNone(observation.window_rect)

    def test_an_app_off_the_bus_observes_empty(self) -> None:
        with fake_wayland():
            observation = _wayland._observe(20)
        self.assertEqual((observation.window_title, observation.tree, observation.refs), ("shell", [], []))

    def test_a_gone_window_is_app_not_running(self) -> None:
        with fake_wayland():
            with self.assertRaises(ComputerUseError) as caught:
                _wayland._observe(999)
        self.assertEqual(caught.exception.code, "APP_NOT_RUNNING")

    def test_depth_cap(self) -> None:
        node = FakeAccessible("PANEL", "leaf")
        for depth in range(20):
            node = FakeAccessible("PANEL", f"n{depth}", children=[node])
        frame = FakeAccessible("FRAME", "Floaty", children=[node])
        app = FakeAccessible("APPLICATION", "deep", children=[frame], pid=700)
        with fake_wayland(atspi=FakeAtspi([app])):
            observation = _wayland._observe(30)
        self.assertEqual(len(observation.refs), _wayland._MAX_DEPTH)
        self.assertTrue(observation.truncated)

    def test_element_cap(self) -> None:
        frame = FakeAccessible("FRAME", "Floaty", children=[FakeAccessible("LABEL", f"l{i}") for i in range(1600)])
        app = FakeAccessible("APPLICATION", "wide", children=[frame], pid=700)
        with fake_wayland(atspi=FakeAtspi([app])):
            observation = _wayland._observe(30)
        self.assertEqual(len(observation.refs), _wayland._MAX_ELEMENTS)
        self.assertTrue(observation.truncated)

    def test_time_budget(self) -> None:
        with fake_wayland(), mock.patch.object(_wayland, "_MAX_OBSERVE_SECONDS", -1.0):
            observation = _wayland._observe(10)
        self.assertEqual(observation.refs, [])
        self.assertTrue(observation.truncated)

    def test_missing_pygobject_is_transport_error_naming_the_fix(self) -> None:
        with mock.patch.object(_wayland, "_atspi_module", None), mock.patch.dict(sys.modules, {"gi": None}):
            with self.assertRaises(ComputerUseError) as caught:
                _wayland._atspi()
        self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")
        self.assertIn("PyGObject", caught.exception.message)


# --- secure focus --------------------------------------------------------------------


class SecureFocusTests(unittest.TestCase):
    def test_focus_on_an_ordinary_entry_is_not_secure(self) -> None:
        with fake_wayland():
            self.assertIs(_wayland._focused_is_secure(10), False)

    def test_focus_on_a_password_field_is_secure(self) -> None:
        app = editor_app()
        frame = app.children[0]
        frame.children[1].states.discard("FOCUSED")
        frame.children[2].states.add("FOCUSED")
        with fake_wayland(atspi=FakeAtspi([app])):
            self.assertIs(_wayland._focused_is_secure(10), True)
        self.assertEqual(frame.children[2].reads, [])

    def test_no_focus_after_a_complete_search_is_false(self) -> None:
        app = editor_app()
        app.children[0].children[1].states.discard("FOCUSED")
        with fake_wayland(atspi=FakeAtspi([app])):
            self.assertIs(_wayland._focused_is_secure(10), False)

    def test_unverifiable_focus_is_none(self) -> None:
        with fake_wayland():
            self.assertIsNone(_wayland._focused_is_secure(20))  # not on the a11y bus
            self.assertIsNone(_wayland._focused_is_secure(999))  # window gone
            with mock.patch.object(_wayland, "_MAX_ELEMENTS", 1):
                app_unfocused = _wayland._focused_is_secure(10)
        self.assertIsNone(app_unfocused)  # the search hit its bound first
        app = editor_app()
        app.children[0].children[1].get_role = lambda: (_ for _ in ()).throw(RuntimeError("gone"))
        with fake_wayland(atspi=FakeAtspi([app])):
            self.assertIsNone(_wayland._focused_is_secure(10))

    def test_atspi_unavailable_is_none(self) -> None:
        def broken() -> Any:
            raise ComputerUseError("TRANSPORT_ERROR", "no gi")

        with fake_wayland(), mock.patch.object(_wayland, "_atspi", broken):
            self.assertIsNone(_wayland._focused_is_secure(10))


# --- fingerprint ---------------------------------------------------------------------


class FingerprintTests(unittest.TestCase):
    def test_fingerprint_reads_focus_frame_and_value_head(self) -> None:
        with fake_wayland():
            self.assertEqual(
                _wayland._window_fingerprint(10),
                (10, "Doc - Text Editor", 5, "entry", "Search", "hello world hello"),
            )

    def test_fingerprint_never_reads_a_secure_value(self) -> None:
        app = editor_app()
        frame = app.children[0]
        frame.children[1].states.discard("FOCUSED")
        frame.children[2].states.add("FOCUSED")
        with fake_wayland(atspi=FakeAtspi([app])):
            fingerprint = _wayland._window_fingerprint(10)
        self.assertEqual(fingerprint, (10, "Doc - Text Editor", 5, "password text", "Password", ""))
        self.assertEqual(frame.children[2].reads, [])

    def test_fingerprint_changes_with_the_value_and_is_none_when_gone(self) -> None:
        app = editor_app()
        with fake_wayland(atspi=FakeAtspi([app])):
            before = _wayland._window_fingerprint(10)
            app.children[0].children[1].text = "edited"
            after = _wayland._window_fingerprint(10)
            gone = _wayland._window_fingerprint(999)
        self.assertNotEqual(before, after)
        self.assertIsNone(gone)


# --- AT-SPI element operations ------------------------------------------------------


class ElementOperationTests(unittest.TestCase):
    def test_press_action_choice(self) -> None:
        self.assertEqual(_wayland._press_action(["expand or contract", "Activate"]), "Activate")
        self.assertEqual(_wayland._press_action(["click", "press"]), "click")
        self.assertIsNone(_wayland._press_action(["expand or contract"]))

    def test_perform_action_and_refusals(self) -> None:
        app = editor_app()
        save = app.children[0].children[0]
        with fake_wayland(atspi=FakeAtspi([app])):
            _wayland._perform_action(save, "click")
            with self.assertRaises(ComputerUseError) as missing:
                _wayland._perform_action(save, "press")
            save.do_action_result = False
            with self.assertRaises(ComputerUseError) as refused:
                _wayland._perform_action(save, "click")
        self.assertEqual(save.performed, ["click", "click"])
        self.assertEqual(missing.exception.code, "ACTION_UNSUPPORTED")
        self.assertEqual(refused.exception.code, "ACTION_UNSUPPORTED")

    def test_text_operations(self) -> None:
        app = editor_app()
        save, search = app.children[0].children[:2]
        with fake_wayland(atspi=FakeAtspi([app])):
            self.assertTrue(_wayland._is_settable(search, "AXValue"))
            self.assertFalse(_wayland._is_settable(save, "AXValue"))
            _wayland._set_value(search, "new")
            self.assertEqual(_wayland._current_value(search), "new")
            _wayland._select_text_range(search, 1, 2)
            _wayland._select_text_range(search, 0, 1)
            with self.assertRaises(ComputerUseError):
                _wayland._set_value(save, "x")
        self.assertEqual(search.writes, ["new"])
        self.assertEqual(search.selections, [(1, 3), (0, 1)])

    def test_live_fingerprint_and_secure_reads(self) -> None:
        app = editor_app()
        save, _search, password = app.children[0].children[:3]
        with fake_wayland(atspi=FakeAtspi([app])):
            self.assertEqual(_wayland._live_fingerprint(save), ("push button", "Save"))
            self.assertEqual(_wayland._live_fingerprint(password), ("password text", "Password"))
            self.assertIs(_wayland._live_is_secure(password), True)
            self.assertIs(_wayland._live_is_secure(save), False)
            save.broken = True
            self.assertEqual(_wayland._live_fingerprint(save), (None, None))
            self.assertIsNone(_wayland._live_is_secure(save))


# --- virtual input dispatch (backend level) --------------------------------------------


class InputDispatchTests(unittest.TestCase):
    def test_click_maps_onto_the_output_in_logical_coordinates(self) -> None:
        with fake_wayland() as env:
            _wayland._click(30, (100.0, 50.0), "right", 2)
        self.assertEqual(env.niri.actions(), [{"Action": {"FocusWindow": {"id": 30}}}])
        self.assertEqual(
            env.input.calls,
            [("click", _wlinput.PointerTarget("HDMI-A-1", 2560, 1440), (112.0, 73.0), "right", 2, 30)],
        )

    def test_already_focused_window_is_not_refocused(self) -> None:
        with fake_wayland() as env:
            _wayland._click(10, (0.0, 0.0))
        self.assertEqual(env.niri.actions(), [])
        self.assertEqual(env.input.calls[0][2], (104.0, 56.0))

    def test_tiled_windows_refuse_coordinate_input_without_moving_focus(self) -> None:
        with fake_wayland() as env:
            for call in (
                lambda: _wayland._click(11, (1.0, 1.0)),
                lambda: _wayland._drag(11, (1.0, 1.0), (2.0, 2.0)),
                lambda: _wayland._scroll(11, "down", 1, (1.0, 1.0)),
            ):
                with self.assertRaises(ComputerUseError) as caught:
                    call()
                self.assertEqual(caught.exception.code, "ACTION_UNSUPPORTED")
                self.assertIn("floating", caught.exception.message)
        self.assertEqual(env.niri.actions(), [])
        self.assertEqual(env.input.calls, [])

    def test_points_outside_the_window_are_invalid(self) -> None:
        with fake_wayland() as env:
            with self.assertRaises(ComputerUseError) as caught:
                _wayland._click(10, (800.0, 1.0))
        self.assertEqual(caught.exception.code, "INVALID_ARGUMENT")
        self.assertEqual(env.input.calls, [])

    def test_focus_that_does_not_land_refuses_input(self) -> None:
        niri = FakeNiri()
        niri.focus_lands = False
        with fake_wayland(niri) as env:
            for call in (
                lambda: _wayland._click(30, (1.0, 1.0)),
                lambda: _wayland._press_key(30, "Return"),
                lambda: _wayland._type_text(30, "x"),
            ):
                with self.assertRaises(ComputerUseError) as caught:
                    call()
                self.assertEqual(caught.exception.code, "INJECTION_FAILED")
                self.assertIn("focus did not land", caught.exception.message)
        self.assertEqual(env.input.calls, [])

    def test_press_key_translates_chords(self) -> None:
        with fake_wayland() as env:
            _wayland._press_key(10, "cmd+shift+s")
            _wayland._press_key(10, "ctrl+alt+Delete")
            _wayland._press_key(10, "PageDown")
            _wayland._press_key(10, "ctrl+.")
        K = _wlinput.KeyStroke
        self.assertEqual(
            [call[1] for call in env.input.calls],
            [[K("s", 65)], [K("BackSpace", 12)], [K("Next", 0)], [K("U002E", 4)]],
        )
        with fake_wayland(), self.assertRaises(ComputerUseError) as caught:
            _wayland._press_key(10, "hyper+x")
        self.assertEqual(caught.exception.code, "INVALID_ARGUMENT")

    def test_type_text_uses_one_keysym_per_character(self) -> None:
        with fake_wayland() as env:
            _wayland._type_text(10, "Hé 1\n")
            _wayland._type_text(10, "")
        K = _wlinput.KeyStroke
        self.assertEqual(
            env.input.calls,
            [("keys", [K("H"), K("U00E9"), K("U0020"), K("1"), K("Return")], 10)],
        )

    def test_typing_rechecks_the_secure_focus_after_focusing(self) -> None:
        app = floaty_app()
        ok = app.children[0].children[0]
        niri = FakeNiri()
        original = niri.focus

        def focus_onto_password(window_id: int) -> None:
            original(window_id)
            ok.role = "PASSWORD_TEXT"
            ok.states.add("FOCUSED")

        niri.focus = focus_onto_password  # type: ignore[method-assign]
        with fake_wayland(niri, FakeAtspi([editor_app(), app])) as env:
            with self.assertRaises(ComputerUseError) as caught:
                _wayland._type_text(30, "secret")
        self.assertEqual(caught.exception.code, "ACTION_UNSUPPORTED")
        self.assertEqual(caught.exception.details, {"live": True})
        self.assertEqual(env.input.calls, [])

    def test_drag_and_scroll(self) -> None:
        with fake_wayland() as env:
            _wayland._drag(10, (1.0, 2.0), (3.0, 4.0))
            _wayland._scroll(10, "up", 2, (5.0, 6.0))
        target = _wlinput.PointerTarget("eDP-1", 1920, 1200)
        self.assertEqual(
            env.input.calls,
            [
                ("drag", target, (105.0, 58.0), (107.0, 60.0), 10),
                ("scroll", target, (109.0, 62.0), "up", 20, 10),
            ],
        )


# --- screenshots ----------------------------------------------------------------------


class ScreenshotTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.shots = Path(self.tmp.name) / "shots"
        patcher = mock.patch.object(_wayland.capture, "_SCREENSHOTS_DIR", self.shots)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_grim_captures_the_logical_rect_into_the_hardened_dir(self) -> None:
        script = X11Script()
        script.on("-g", png=(1600, 1200))
        with fake_wayland(script=script):
            result, rect = _wayland._screenshot_window(10)
        self.assertEqual(script.calls, [["/usr/bin/grim", "-g", "104,56 800x600", result["path"]]])
        self.assertEqual((result["width"], result["height"]), (1600, 1200))
        self.assertEqual(rect, (104.0, 56.0, 800.0, 600.0))
        self.assertEqual(Path(result["path"]).parent, self.shots)
        self.assertEqual(stat.S_IMODE(os.stat(result["path"]).st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(os.stat(self.shots).st_mode), 0o700)

    def test_tiled_or_overlapped_windows_refuse(self) -> None:
        niri = FakeNiri()
        niri.windows.append(
            niri_window(12, app_id="x", title="over", pid=9, floating=True, tile_pos=(500.0, 300.0), size=(100, 100))
        )
        script = X11Script()
        with fake_wayland(niri, script=script):
            with self.assertRaises(ComputerUseError) as tiled:
                _wayland._screenshot_window(11)
            niri.focus(20)
            with self.assertRaises(ComputerUseError) as overlapped:
                _wayland._screenshot_window(10)
        self.assertEqual(tiled.exception.code, "ACTION_UNSUPPORTED")
        self.assertEqual(overlapped.exception.code, "ACTION_UNSUPPORTED")
        self.assertIn("overlaps", overlapped.exception.message)
        self.assertEqual(script.calls, [])

    def test_a_focused_floating_window_is_on_top(self) -> None:
        niri = FakeNiri()
        niri.windows.append(
            niri_window(12, app_id="x", title="over", pid=9, floating=True, tile_pos=(500.0, 300.0), size=(100, 100))
        )
        script = X11Script()
        script.on("-g", png=(10, 10))
        with fake_wayland(niri, script=script):
            _wayland._screenshot_window(10)
        self.assertEqual(len(script.calls), 1)

    def test_grim_missing_failing_or_bad_png(self) -> None:
        with fake_wayland(tools=()):
            with self.assertRaises(ComputerUseError) as missing:
                _wayland._screenshot_window(10)
        self.assertEqual(missing.exception.code, "TRANSPORT_ERROR")
        self.assertIn("grim", missing.exception.message)
        failing = X11Script()
        failing.on("-g", returncode=1, stderr=b"compositor doesn't support wlr-screencopy")
        with fake_wayland(script=failing):
            with self.assertRaises(ComputerUseError) as failed:
                _wayland._screenshot_window(10)
        self.assertIn("screencopy", failed.exception.message)
        with fake_wayland():  # grim "succeeds" but writes nothing
            with self.assertRaises(ComputerUseError) as empty:
                _wayland._screenshot_window(10)
        self.assertEqual(empty.exception.code, "TRANSPORT_ERROR")

    def test_a_symlinked_capture_dir_is_refused(self) -> None:
        if not str(self.shots).startswith(str(Path.home())):
            outside = Path(self.tmp.name) / "elsewhere"
            outside.mkdir()
            self.shots.symlink_to(outside)
        script = X11Script()
        script.on("-g", png=(10, 10))
        with fake_wayland(script=script):
            with self.assertRaises(ComputerUseError) as caught:
                _wayland._screenshot_window(10)
        self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")
        self.assertIn("symlink", caught.exception.message)
        self.assertEqual(script.calls, [])


# --- lock state and capabilities --------------------------------------------------------


class LockAndStatusTests(unittest.TestCase):
    def lock(self, stdout: bytes, returncode: int = 0, tools: tuple[str, ...] = ("loginctl",)) -> tuple[bool, X11Script]:
        script = X11Script()
        script.on("show-session", stdout=stdout, returncode=returncode)
        with fake_wayland(script=script, tools=tools), mock.patch.dict(os.environ, {"XDG_SESSION_ID": "2"}):
            return _wayland._screen_locked(), script

    def test_locked_hint_parsing_fails_closed(self) -> None:
        unlocked, script = self.lock(b"LockedHint=no\nActive=yes\n")
        self.assertFalse(unlocked)
        self.assertEqual(
            script.calls, [["/usr/bin/loginctl", "show-session", "2", "-p", "LockedHint", "-p", "Active"]]
        )
        self.assertTrue(self.lock(b"LockedHint=yes\nActive=yes\n")[0])
        self.assertTrue(self.lock(b"LockedHint=no\nActive=no\n")[0])
        self.assertTrue(self.lock(b"", returncode=1)[0])
        self.assertTrue(self.lock(b"garbage")[0])
        self.assertTrue(self.lock(b"LockedHint=no\nActive=yes\n", tools=())[0])

    def test_policy_dispatches_to_the_wayland_lock_probe(self) -> None:
        with mock.patch.object(_compat, "_backend", return_value="wayland"), mock.patch.object(
            _wayland, "_screen_locked", return_value=False
        ) as probe:
            self.assertFalse(policy._screen_locked())
        probe.assert_called_once_with()

    def test_status_reports_real_capabilities(self) -> None:
        with fake_wayland():
            status = _wayland._status()
        self.assertEqual(status["accessibility"], "ok")
        self.assertEqual(status["screen_recording"], "ok")
        self.assertEqual(status["input"], {"pointer": "ok", "keyboard": "ok"})
        self.assertIn("floating", status["help"][-1])

    def test_status_names_missing_pieces(self) -> None:
        def no_gi() -> Any:
            raise ComputerUseError("TRANSPORT_ERROR", "AT-SPI needs PyGObject")

        def no_input() -> dict[str, bool]:
            raise ComputerUseError("ACTION_UNSUPPORTED", "no socket")

        with fake_wayland(tools=()), mock.patch.object(_wayland, "_atspi", no_gi), mock.patch.object(
            _wlinput, "_available", no_input
        ):
            status = _wayland._status()
        self.assertEqual(status["accessibility"], "missing")
        self.assertEqual(status["screen_recording"], "missing")
        self.assertEqual(status["input"], {"pointer": "unknown", "keyboard": "unknown"})
        joined = "\n".join(status["help"])
        self.assertIn("PyGObject", joined)
        self.assertIn("grim", joined)
        self.assertIn("zwlr_virtual_pointer_manager_v1", joined)


# --- the App layer on Wayland ------------------------------------------------------------


class AppWaylandTests(unittest.TestCase):
    def bind(self, spec: str = "org.gnome.TextEditor") -> Any:
        return run(computer_use.get_app(spec))

    def test_get_app_binds_the_focused_window_and_observes(self) -> None:
        with fakes_wayland.wayland_app_environment():
            app = self.bind()
            self.assertEqual((app.bundle_id, app.pid), ("org.gnome.TextEditor", 10))
            self.assertIn("org.gnome.TextEditor (org.gnome.TextEditor) — window 'Doc - Text Editor'", app.state)
            self.assertIn("[0] push button 'Save' (actions: click) @ (10, 10) 80x30", app.state)
            self.assertIn("[secure]", app.state)
            self.assertIs(self.bind(), app)

    def test_get_app_gates_and_reports_missing_apps(self) -> None:
        with fakes_wayland.wayland_app_environment(allowed=("foot",)):
            with self.assertRaises(ComputerUseError) as gated:
                self.bind()
            with self.assertRaises(ComputerUseError) as missing:
                self.bind("org.nope")
        self.assertEqual(gated.exception.code, "APP_NOT_ALLOWED")
        self.assertEqual(missing.exception.code, "APP_NOT_RUNNING")

    def test_element_click_runs_the_atspi_action_without_focus_or_pointer(self) -> None:
        atspi = FakeAtspi([editor_app(), floaty_app()])
        with fakes_wayland.wayland_app_environment(atspi=atspi) as env:
            app = self.bind()
            run(app.click(0))
        self.assertEqual(atspi.desktop.children[0].children[0].children[0].performed, ["click"])
        self.assertEqual(env.input.calls, [])
        self.assertEqual(env.niri.actions(), [])

    def test_element_without_an_action_clicks_its_center_through_the_pointer(self) -> None:
        with fakes_wayland.wayland_app_environment() as env:
            app = self.bind()
            run(app.click(1, button="left"))
            run(app.click(0, button="right"))
        target = _wlinput.PointerTarget("eDP-1", 1920, 1200)
        self.assertEqual(
            env.input.calls,
            [("click", target, (304.0, 81.0), "left", 1, 10), ("click", target, (154.0, 81.0), "right", 1, 10)],
        )

    def test_screenshot_points_scale_back_to_logical(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(_wayland.capture, "_SCREENSHOTS_DIR", Path(tmp) / "s"):
                with fakes_wayland.wayland_app_environment() as env:
                    env.script.on("-g", png=(1600, 1200))
                    app = self.bind()
                    run(app.click((10, 20)))  # no screenshot yet: logical
                    shot = run(app.get_screenshot(attach=False))
                    run(app.click((200, 100)))
                    with self.assertRaises(ComputerUseError) as outside:
                        run(app.click((1600, 5)))
        self.assertEqual((shot["width"], shot["height"]), (1600, 1200))
        self.assertEqual([call[2] for call in env.input.calls], [(114.0, 76.0), (204.0, 106.0)])
        self.assertEqual(outside.exception.code, "INVALID_ARGUMENT")

    def test_keyboard_flows_focus_the_window_and_refuse_secure_focus(self) -> None:
        with fakes_wayland.wayland_app_environment() as env:
            app = self.bind("org.example.Floaty")
            run(app.press_key("ctrl+s"))
            run(app.type_text("ok"))
            self.assertEqual(env.niri.actions(), [{"Action": {"FocusWindow": {"id": 30}}}])
            with mock.patch.object(_wayland, "_focused_is_secure", return_value=True):
                with self.assertRaises(ComputerUseError) as secure:
                    run(app.type_text("hunter2"))
            with mock.patch.object(_wayland, "_focused_is_secure", return_value=None):
                with self.assertRaises(ComputerUseError) as unknown:
                    run(app.press_key("Return"))
        self.assertEqual(len(env.input.calls), 2)
        self.assertEqual(secure.exception.code, "ACTION_UNSUPPORTED")
        self.assertEqual(secure.exception.details, {"live": True})
        self.assertEqual(unknown.exception.code, "ACTION_UNSUPPORTED")
        self.assertEqual(unknown.exception.details, {"live": False})

    def test_set_value_select_text_and_secondary_actions_use_atspi(self) -> None:
        atspi = FakeAtspi([editor_app(), floaty_app()])
        frame = atspi.desktop.children[0].children[0]
        with fakes_wayland.wayland_app_environment(atspi=atspi):
            app = self.bind()
            run(app.select_text(1, "world"))
            run(app.set_value(1, "line one\nline two"))
            run(app.perform_secondary_action(0, "click"))
            with self.assertRaises(ComputerUseError) as secure:
                run(app.set_value(2, "hunter2"))
            with self.assertRaises(ComputerUseError) as not_editable:
                run(app.set_value(0, "x"))
            with self.assertRaises(ComputerUseError) as unexposed:
                run(app.perform_secondary_action(0, "press"))
        self.assertEqual(frame.children[1].selections, [(6, 11)])
        self.assertEqual(frame.children[1].writes, ["line one\nline two"])
        self.assertEqual(frame.children[0].performed, ["click"])
        self.assertEqual(frame.children[2].writes, [])
        self.assertEqual(secure.exception.details, {"element_index": 2, "secure": True})
        self.assertEqual(not_editable.exception.code, "ACTION_UNSUPPORTED")
        self.assertEqual(unexposed.exception.code, "ACTION_UNSUPPORTED")

    def test_paste_and_ocr_name_the_wayland_gap(self) -> None:
        with fakes_wayland.wayland_app_environment():
            app = self.bind()
            with fakes.telemetry_recorder() as recorder:
                with self.assertRaises(ComputerUseError) as paste:
                    run(app.paste("x"))
            with self.assertRaises(ComputerUseError) as ocr:
                run(app.get_text_regions())
        self.assertEqual(paste.exception.code, "ACTION_UNSUPPORTED")
        self.assertIn("Wayland backend", paste.exception.message)
        self.assertEqual(recorder.events[-1]["properties"]["error_code"], "ACTION_UNSUPPORTED")
        self.assertEqual(ocr.exception.code, "ACTION_UNSUPPORTED")

    def test_activate_and_is_frontmost(self) -> None:
        with fakes_wayland.wayland_app_environment() as env:
            app = self.bind("org.example.Floaty")
            self.assertFalse(app.is_frontmost())
            run(app.activate())
            self.assertTrue(app.is_frontmost())
        self.assertEqual(env.niri.actions(), [{"Action": {"FocusWindow": {"id": 30}}}])

    def test_guard_rejects_a_gone_or_reassigned_window(self) -> None:
        with fakes_wayland.wayland_app_environment() as env:
            app = self.bind()
            env.niri.windows[0]["app_id"] = "org.other"
            with self.assertRaises(ComputerUseError) as reassigned:
                run(app.get_ax_state())
            env.niri.windows.pop(0)
            with self.assertRaises(ComputerUseError) as gone:
                run(app.click(0))
        self.assertEqual(reassigned.exception.code, "APP_NOT_RUNNING")
        self.assertEqual(gone.exception.code, "APP_NOT_RUNNING")

    def test_element_stale_when_the_live_name_changes(self) -> None:
        atspi = FakeAtspi([editor_app(), floaty_app()])
        with fakes_wayland.wayland_app_environment(atspi=atspi):
            app = self.bind()
            atspi.desktop.children[0].children[0].children[0].name = "Save As"
            with self.assertRaises(ComputerUseError) as caught:
                run(app.click(0))
        self.assertEqual(caught.exception.code, "ELEMENT_STALE")

    def test_actions_settle_on_the_wayland_fingerprint(self) -> None:
        with fakes_wayland.wayland_app_environment() as env:
            app = self.bind()
            before = len(env.niri.requests)
            with mock.patch.object(_wayland, "_window_fingerprint", wraps=_wayland._window_fingerprint) as probe:
                run(app.click(0))
        self.assertGreaterEqual(probe.call_count, 2)
        probe.assert_called_with(10)
        self.assertGreater(len(env.niri.requests), before)

    def test_diff_and_state_and_permissions_report_wayland(self) -> None:
        with fakes_wayland.wayland_app_environment():
            app = self.bind()
            self.assertEqual(run(app.get_ax_state()), "(no changes since the previous observation)")
            state = run(computer_use.get_state(emit=False))
            status = run(computer_use.permissions_status())
        self.assertEqual(state["platform"], "wayland")
        self.assertEqual(state["apps"][0]["id"], "org.gnome.TextEditor")
        self.assertEqual(status["accessibility"], "ok")
        self.assertEqual(status["input"], {"pointer": "ok", "keyboard": "ok"})


# --- the Wayland wire client --------------------------------------------------------------


class FakeCompositor(threading.Thread):
    """An in-process compositor over a socketpair: advertises globals, answers sync, records requests."""

    def __init__(self, globals_: list[tuple[str, int]], outputs: list[str] = (), error_on: tuple[str, int] | None = None) -> None:
        super().__init__(daemon=True)
        self.server, self.client = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        self.client.settimeout(2)
        self.globals = globals_
        self.outputs = list(outputs)
        self.error_on = error_on
        self.objects: dict[int, str] = {1: "wl_display"}
        self.requests: list[tuple[str, int, bytes, list[int]]] = []
        self.keymaps: list[bytes] = []
        self._output_index = 0

    def _send(self, object_id: int, opcode: int, payload: bytes) -> None:
        self.server.sendall(struct.pack("=II", object_id, ((8 + len(payload)) << 16) | opcode) + payload)

    def run(self) -> None:
        buffer = b""
        fds: list[int] = []
        while True:
            fd_array = array.array("i")
            try:
                data, ancillary, _flags, _addr = self.server.recvmsg(65536, socket.CMSG_SPACE(16 * fd_array.itemsize))
            except OSError:
                return
            for level, kind, cdata in ancillary:
                if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                    fd_array.frombytes(cdata[: len(cdata) - (len(cdata) % fd_array.itemsize)])
            fds.extend(fd_array)
            if not data:
                return
            buffer += data
            while len(buffer) >= 8:
                object_id, word = struct.unpack_from("=II", buffer)
                size = word >> 16
                if len(buffer) < size:
                    break
                payload, buffer = buffer[8:size], buffer[size:]
                self._handle(object_id, word & 0xFFFF, payload, fds)

    def _handle(self, object_id: int, opcode: int, payload: bytes, fds: list[int]) -> None:
        interface = self.objects.get(object_id, "?")
        taken: list[int] = []
        if interface == "zwp_virtual_keyboard_v1" and opcode == 0:
            fd = fds.pop(0)
            size = struct.unpack_from("=I", payload, 4)[0]
            self.keymaps.append(os.pread(fd, size, 0))
            os.close(fd)
            taken.append(fd)
        self.requests.append((interface, opcode, payload, taken))
        if self.error_on == (interface, opcode):
            message = _wlinput._string("bad request")
            self._send(1, 0, struct.pack("=II", object_id, 3) + message)
            return
        if interface == "wl_display" and opcode == 1:
            registry = struct.unpack_from("=I", payload)[0]
            self.objects[registry] = "wl_registry"
            for name, (global_interface, version) in enumerate(self.globals, start=1):
                self._send(registry, 0, struct.pack("=I", name) + _wlinput._string(global_interface) + struct.pack("=I", version))
        elif interface == "wl_display" and opcode == 0:
            callback = struct.unpack_from("=I", payload)[0]
            self._send(callback, 0, struct.pack("=I", 0))
        elif interface == "wl_registry" and opcode == 0:
            name = struct.unpack_from("=I", payload)[0]
            bound, offset = _wlinput._read_string(payload, 4)
            _version, new_id = struct.unpack_from("=II", payload, offset)
            self.objects[new_id] = bound
            if bound == "wl_output":
                self._send(new_id, 4, _wlinput._string(self.outputs[self._output_index]))
                self._output_index += 1
        elif interface == "zwlr_virtual_pointer_manager_v1" and opcode in (0, 2):
            new_id = struct.unpack_from("=I", payload, len(payload) - 4)[0]
            self.objects[new_id] = "zwlr_virtual_pointer_v1"
        elif interface == "zwp_virtual_keyboard_manager_v1" and opcode == 0:
            new_id = struct.unpack_from("=I", payload, 4)[0]
            self.objects[new_id] = "zwp_virtual_keyboard_v1"

    def calls(self, interface: str) -> list[tuple[int, tuple[int, ...]]]:
        """(opcode, uint args) for every request on one interface."""
        return [
            (opcode, struct.unpack(f"={len(payload) // 4}I", payload))
            for name, opcode, payload, _fds in self.requests
            if name == interface
        ]


FULL_GLOBALS = [
    ("wl_seat", 9),
    ("wl_output", 4),
    ("wl_output", 4),
    ("zwlr_virtual_pointer_manager_v1", 2),
    ("zwp_virtual_keyboard_manager_v1", 1),
]


class WireClientTests(unittest.TestCase):
    def compositor(self, globals_: list[tuple[str, int]] = FULL_GLOBALS, **kwargs: Any) -> FakeCompositor:
        compositor = FakeCompositor(globals_, outputs=["eDP-1", "HDMI-A-1"], **kwargs)
        compositor.start()
        patcher = mock.patch.object(_wlinput, "_open_socket", lambda path: compositor.client)
        patcher.start()
        self.addCleanup(patcher.stop)
        env = mock.patch.dict(os.environ, {"WAYLAND_DISPLAY": "wayland-9", "XDG_RUNTIME_DIR": "/run/user/1"})
        env.start()
        self.addCleanup(env.stop)
        self.addCleanup(compositor.server.close)
        return compositor

    def test_click_creates_a_pointer_on_the_named_output_and_clicks(self) -> None:
        compositor = self.compositor()
        with mock.patch.object(_wlinput, "_now_ms", return_value=7):
            _wlinput.click(_wlinput.PointerTarget("HDMI-A-1", 2560, 1440), (112.4, 73.0), "right", 2)
        manager = compositor.calls("zwlr_virtual_pointer_manager_v1")
        seat_id = next(i for i, name in compositor.objects.items() if name == "wl_seat")
        hdmi_id = sorted(i for i, name in compositor.objects.items() if name == "wl_output")[1]
        self.assertEqual(manager[0][0], 2)  # create_virtual_pointer_with_output
        self.assertEqual(manager[0][1][:2], (seat_id, hdmi_id))
        self.assertEqual(
            compositor.calls("zwlr_virtual_pointer_v1"),
            [
                (1, (7, 112, 73, 2560, 1440)),
                (4, ()),
                (2, (7, 0x111, 1)),
                (4, ()),
                (2, (7, 0x111, 0)),
                (4, ()),
                (2, (7, 0x111, 1)),
                (4, ()),
                (2, (7, 0x111, 0)),
                (4, ()),
                (8, ()),
            ],
        )

    def test_scroll_sends_discrete_wheel_frames(self) -> None:
        compositor = self.compositor()
        with mock.patch.object(_wlinput, "_now_ms", return_value=1):
            _wlinput.scroll(_wlinput.PointerTarget("eDP-1", 1920, 1200), (5.0, 6.0), "up", 1)
        requests = compositor.calls("zwlr_virtual_pointer_v1")
        self.assertEqual(requests[0], (1, (1, 5, 6, 1920, 1200)))
        self.assertEqual(requests[2], (5, (0,)))
        opcode, args = requests[3]
        self.assertEqual(opcode, 7)
        self.assertEqual(args[:2], (1, 0))
        self.assertEqual(struct.unpack("=ii", struct.pack("=II", *args[2:])), (-15 * 256, -1))

    def test_send_keys_uploads_a_keymap_and_presses_with_modifiers(self) -> None:
        compositor = self.compositor()
        K = _wlinput.KeyStroke
        with mock.patch.object(_wlinput, "_now_ms", return_value=3):
            _wlinput.send_keys([K("s", 4), K("U00E9"), K("s", 4)])
        keymap = compositor.keymaps[0].decode()
        self.assertTrue(compositor.keymaps[0].endswith(b"\x00"))
        self.assertIn("key <K0> {[ s ]};", keymap)
        self.assertIn("key <K1> {[ U00E9 ]};", keymap)
        self.assertNotIn("<K2>", keymap)
        self.assertEqual(
            compositor.calls("zwp_virtual_keyboard_v1"),
            [
                (0, (1, len(compositor.keymaps[0]))),
                (2, (4, 0, 0, 0)),
                (1, (3, 1, 1)),
                (1, (3, 1, 0)),
                (2, (0, 0, 0, 0)),
                (1, (3, 2, 1)),
                (1, (3, 2, 0)),
                (2, (4, 0, 0, 0)),
                (1, (3, 1, 1)),
                (1, (3, 1, 0)),
                (2, (0, 0, 0, 0)),
                (3, ()),
            ],
        )

    def test_missing_globals_refuse_naming_the_protocol(self) -> None:
        self.compositor([("wl_seat", 9), ("wl_output", 4)])
        with self.assertRaises(ComputerUseError) as pointer:
            _wlinput.click(_wlinput.PointerTarget("eDP-1", 1920, 1200), (1.0, 1.0), "left", 1)
        self.assertEqual(pointer.exception.code, "ACTION_UNSUPPORTED")
        self.assertIn("zwlr_virtual_pointer_manager_v1", pointer.exception.message)

    def test_missing_keyboard_manager_and_availability(self) -> None:
        globals_ = [("wl_seat", 9), ("zwlr_virtual_pointer_manager_v1", 2)]
        self.compositor(globals_)
        self.assertEqual(_wlinput._available(), {"pointer": True, "keyboard": False})
        self.compositor(globals_)  # every session is its own connection
        with self.assertRaises(ComputerUseError) as keyboard:
            _wlinput.send_keys([_wlinput.KeyStroke("a")])
        self.assertIn("zwp_virtual_keyboard_manager_v1", keyboard.exception.message)

    def test_unknown_output_refuses(self) -> None:
        self.compositor()
        with self.assertRaises(ComputerUseError) as caught:
            _wlinput.click(_wlinput.PointerTarget("DP-9", 100, 100), (1.0, 1.0), "left", 1)
        self.assertEqual(caught.exception.code, "ACTION_UNSUPPORTED")
        self.assertIn("DP-9", caught.exception.message)

    def test_a_protocol_error_is_injection_failed(self) -> None:
        self.compositor(error_on=("zwlr_virtual_pointer_v1", 2))
        with self.assertRaises(ComputerUseError) as caught:
            _wlinput.click(_wlinput.PointerTarget("eDP-1", 1920, 1200), (1.0, 1.0), "left", 1)
        self.assertEqual(caught.exception.code, "INJECTION_FAILED")
        self.assertIn("bad request", caught.exception.message)

    def test_socket_resolution(self) -> None:
        with mock.patch.dict(os.environ, {"WAYLAND_DISPLAY": "wayland-1", "XDG_RUNTIME_DIR": "/run/user/5"}):
            self.assertEqual(_wlinput._socket_path(), "/run/user/5/wayland-1")
        with mock.patch.dict(os.environ, {"WAYLAND_DISPLAY": "/abs/sock"}):
            self.assertEqual(_wlinput._socket_path(), "/abs/sock")
        with mock.patch.dict(os.environ, {"WAYLAND_DISPLAY": ""}):
            with self.assertRaises(ComputerUseError) as caught:
                _wlinput._socket_path()
        self.assertEqual(caught.exception.code, "ACTION_UNSUPPORTED")


class KeymapTests(unittest.TestCase):
    def test_keysym_names(self) -> None:
        self.assertEqual(
            [_wlinput.keysym_for_char(c) for c in "aZ9 !\n\té😀"],
            ["a", "Z", "9", "U0020", "U0021", "Return", "Tab", "U00E9", "U1F600"],
        )
        for character in ("\x1b", "\r", "\x7f"):
            with self.subTest(character=character), self.assertRaises(ComputerUseError) as caught:
                _wlinput.keysym_for_char(character)
            self.assertEqual(caught.exception.code, "INVALID_ARGUMENT")

    def test_groups_split_at_the_keymap_size(self) -> None:
        strokes = [_wlinput.KeyStroke(f"U{0x4E00 + index:04X}") for index in range(300)]
        groups = list(_wlinput._groups(strokes))
        self.assertEqual([len(group) for group in groups], [240, 60])
        self.assertEqual(sum(groups, []), strokes)

    @unittest.skipUnless(ctypes.util.find_library("xkbcommon"), "libxkbcommon not installed")
    def test_the_generated_keymap_compiles_with_libxkbcommon(self) -> None:
        library = ctypes.CDLL(ctypes.util.find_library("xkbcommon"))
        library.xkb_context_new.restype = ctypes.c_void_p
        library.xkb_keymap_new_from_string.restype = ctypes.c_void_p
        library.xkb_keymap_new_from_string.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
        library.xkb_keymap_mod_get_index.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        library.xkb_keymap_unref.argtypes = [ctypes.c_void_p]
        library.xkb_context_unref.argtypes = [ctypes.c_void_p]
        context = library.xkb_context_new(0)
        keysyms = [_wlinput.keysym_for_char(c) for c in "aZ1 !\n\té€"] + ["BackSpace", "Prior", "F12", "Escape"]
        keymap = library.xkb_keymap_new_from_string(context, _wlinput._keymap_text(keysyms).encode(), 1, 0)
        try:
            self.assertTrue(keymap)
            masks = {name: 1 << library.xkb_keymap_mod_get_index(keymap, name.encode()) for name in ("Shift", "Control", "Mod1", "Mod4")}
            self.assertEqual(
                masks,
                {
                    "Shift": _wlinput.MODIFIER_MASKS["shift"],
                    "Control": _wlinput.MODIFIER_MASKS["ctrl"],
                    "Mod1": _wlinput.MODIFIER_MASKS["alt"],
                    "Mod4": _wlinput.MODIFIER_MASKS["cmd"],
                },
            )
        finally:
            if keymap:
                library.xkb_keymap_unref(keymap)
            library.xkb_context_unref(context)


# --- live (read-only, opt-in) -----------------------------------------------------------------


@unittest.skipUnless(LIVE, LIVE_SKIP)
class LiveWaylandSmokes(unittest.TestCase):
    """Read-only: selection, niri window listing, the lock probe. No input, focus, or capture."""

    def test_backend_is_wayland(self) -> None:
        self.assertEqual(_compat._backend(), "wayland")

    def test_list_apps_shape(self) -> None:
        for app in _wayland._list_apps():
            self.assertEqual(sorted(app), ["id", "name", "running"])

    def test_screen_locked_returns_bool(self) -> None:
        self.assertIsInstance(_wayland._screen_locked(), bool)


if __name__ == "__main__":
    unittest.main()
