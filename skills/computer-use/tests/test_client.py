# unittest fixtures are set in setUp, not __init__.
# pyright: reportUninitializedInstanceVariable=false
"""Tests for the kernel-side thin client over the computer_use.* host requests.

The behaviour behind every call (policy, permissions, observation, input,
capture, the secure-field rules) runs in the host and is tested there (the
pa-computer-use crate). These tests pin the client's half: the request wire
form, the Python-typed argument checks that run before a request is sent,
the error mapping, App identity, screenshot attachment, and the first-run
guidance. A fake host answers every request; nothing touches a display.
"""

from __future__ import annotations

import contextlib
import io
import shutil
import sys
import tomllib
import types
import unittest
from pathlib import Path
from typing import Any

SRC = Path(__file__).resolve().parents[1] / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

import computer_use  # noqa: E402
from computer_use import errors  # noqa: E402
from computer_use.errors import ComputerUseError  # noqa: E402


class FakeHost:
    """Records every request and answers from a queue (or a default)."""

    def __init__(self) -> None:
        self.requests: list[tuple[str, dict[str, Any]]] = []
        self.replies: list[Any] = []
        self.default: Any = {"ok": None}
        self.raises: BaseException | None = None

    def answer(self, request_type: str, payload: dict[str, Any]) -> Any:
        self.requests.append((request_type, dict(payload)))
        if self.raises is not None:
            raise self.raises
        return self.replies.pop(0) if self.replies else self.default

    async def __call__(self, request_type: str, payload: dict[str, Any] | None = None) -> Any:
        return self.answer(request_type, payload or {})

    def blocking(self, request: dict[str, Any], *, timeout_s: float | None = None) -> dict[str, Any]:
        payload = dict(request)
        request_type = payload.pop("type")
        return {"status": "ok", "result": self.answer(request_type, payload)}


BOUND = {"handle": 7, "bundle_id": "com.example.app", "name": "Example", "pid": 4242, "state": "Example (com.example.app)"}


class ClientTestCase(unittest.IsolatedAsyncioTestCase):
    host: FakeHost
    attached: list[str]

    def setUp(self) -> None:
        self.host = FakeHost()
        saved = (computer_use._host_request, computer_use._host_request_blocking, dict(computer_use._bound_apps))
        computer_use._host_request = self.host
        computer_use._host_request_blocking = self.host.blocking
        computer_use._bound_apps.clear()

        def restore() -> None:
            computer_use._host_request, computer_use._host_request_blocking = saved[0], saved[1]
            computer_use._bound_apps.clear()
            computer_use._bound_apps.update(saved[2])

        self.addCleanup(restore)
        self.attached: list[str] = []
        attach_module = types.ModuleType("attach_image")

        async def run(path: str) -> None:
            self.attached.append(path)

        attach_module.run = run  # pyright: ignore[reportAttributeAccessIssue]
        saved_attach = sys.modules.get("attach_image")
        sys.modules["attach_image"] = attach_module
        self.addCleanup(lambda: sys.modules.pop("attach_image", None) if saved_attach is None else sys.modules.__setitem__("attach_image", saved_attach))

    async def bind(self) -> computer_use.App:
        self.host.replies.append({"ok": dict(BOUND)})
        app = await computer_use.get_app("com.example.app")
        self.host.requests.clear()
        return app

    def last(self) -> tuple[str, dict[str, Any]]:
        return self.host.requests[-1]

    def assert_invalid(self, error: ComputerUseError, message: str, details: Any) -> None:
        self.assertEqual((error.code, error.message, error.details), ("INVALID_ARGUMENT", message, details))


class ModuleCallTests(ClientTestCase):
    async def test_get_state_sends_emit_and_prints_missing_grants(self) -> None:
        self.host.replies.append(
            {"ok": {"apps": [], "permissions": {"accessibility": "missing", "screen_recording": "ok", "help": ["line one", "line two"]}, "allowlist": {}, "platform": "mac"}}
        )
        captured = io.StringIO()
        with contextlib.redirect_stdout(captured):
            state = await computer_use.get_state()
        self.assertEqual(self.host.requests, [("computer_use.get_state", {"emit": True})])
        self.assertEqual(state["platform"], "mac")
        self.assertEqual(
            captured.getvalue(),
            "Prime Agent computer use needs macOS permissions before it can drive apps:\nline one\nline two\n",
        )

    async def test_get_state_emit_false_is_silent_and_wayland_names_its_pieces(self) -> None:
        status = {"accessibility": "missing", "screen_recording": "missing", "input": {}, "help": ["fix it"]}
        self.host.replies.extend([{"ok": {"permissions": status}}, {"ok": {"permissions": status}}])
        captured = io.StringIO()
        with contextlib.redirect_stdout(captured):
            await computer_use.get_state(emit=False)
            self.assertEqual(captured.getvalue(), "")
            await computer_use.get_state()
        self.assertEqual(self.host.requests[0], ("computer_use.get_state", {"emit": False}))
        self.assertEqual(captured.getvalue(), "Prime Agent computer use is missing Wayland backend pieces:\nfix it\n")

    async def test_list_apps_and_permissions_status_forward(self) -> None:
        self.host.replies.extend([{"ok": [{"id": "a", "name": "A", "running": True}]}, {"ok": {"accessibility": "ok"}}])
        self.assertEqual(await computer_use.list_apps(), [{"id": "a", "name": "A", "running": True}])
        self.assertEqual(await computer_use.permissions_status(), {"accessibility": "ok"})
        self.assertEqual([request for request, _ in self.host.requests], ["computer_use.list_apps", "computer_use.permissions_status"])

    async def test_host_errors_raise_as_computer_use_errors(self) -> None:
        self.host.replies.append({"error": {"code": "APP_NOT_ALLOWED", "message": "not on the allowlist", "details": {"bundle_id": "x"}}})
        with self.assertRaises(ComputerUseError) as caught:
            await computer_use.get_app("x")
        self.assertEqual((caught.exception.code, caught.exception.message, caught.exception.details), ("APP_NOT_ALLOWED", "not on the allowlist", {"bundle_id": "x"}))
        self.assertEqual(str(caught.exception), "App not allowed (APP_NOT_ALLOWED): not on the allowlist")

    async def test_a_missing_or_failing_host_is_a_transport_error(self) -> None:
        self.host.raises = RuntimeError("no handler registered for computer_use.get_state")
        with self.assertRaises(ComputerUseError) as caught:
            await computer_use.get_state()
        self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")
        self.assertIn("computer use backend unavailable", caught.exception.message)
        computer_use._host_request = None
        with self.assertRaises(ComputerUseError) as caught:
            await computer_use.list_apps()
        self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")

    async def test_a_host_that_does_not_serve_computer_use_names_the_skew(self) -> None:
        self.host.raises = RuntimeError('host request type "computer_use.get_state" is not available in this session')
        with self.assertRaises(ComputerUseError) as caught:
            await computer_use.get_state()
        self.assertEqual(caught.exception.code, "TRANSPORT_ERROR")
        self.assertIn("does not serve computer_use requests", caught.exception.message)
        self.assertIn("host/runtime version skew", caught.exception.message)
        self.assertIn("Reinstall prime-agent", caught.exception.message)

    async def test_run_summarizes_the_state(self) -> None:
        self.host.replies.append(
            {"ok": {"apps": [{"id": "a", "running": True}], "permissions": {"accessibility": "ok"}, "allowlist": {"allowed": ["a"], "blocked": []}, "platform": "linux"}}
        )
        self.assertEqual(
            await computer_use.run(),
            "platform: linux\naccessibility: ok\nscreen recording: unknown\nrunning apps: 1\nallowlist: 1 allowed, 0 blocked",
        )


class GetAppTests(ClientTestCase):
    async def test_specs_travel_with_their_shape_str_and_repr(self) -> None:
        cases = [
            ("Slack", {"kind": "str", "value": "Slack", "str": "Slack", "repr": "'Slack'"}),
            (
                {"bundle_id": "com.x", "name": 5, 3: "y"},
                {
                    "kind": "dict",
                    "entries": [["bundle_id", "com.x"], ["name", None]],
                    "keys": [3, "bundle_id", "name"],
                    "str": "{'bundle_id': 'com.x', 'name': 5, 3: 'y'}",
                    "repr": "{'bundle_id': 'com.x', 'name': 5, 3: 'y'}",
                },
            ),
            (5, {"kind": "other", "type": "int", "str": "5", "repr": "5"}),
        ]
        for index, (spec, expected) in enumerate(cases):
            with self.subTest(spec=spec):
                self.host.replies.append({"ok": dict(BOUND, handle=100 + index)})
                await computer_use.get_app(spec)  # pyright: ignore[reportArgumentType]
                request_type, payload = self.last()
                self.assertEqual(request_type, "computer_use.get_app")
                self.assertEqual(payload["spec"], expected)
                self.assertTrue(payload["instructions_dir"].endswith("references/app-instructions"))

    async def test_the_bound_app_carries_the_binding_and_rebinding_reuses_it(self) -> None:
        app = await self.bind()
        self.assertEqual((app.bundle_id, app.name, app.pid, app.state), ("com.example.app", "Example", 4242, "Example (com.example.app)"))
        self.assertEqual(repr(app), "<App Example (com.example.app) pid 4242>")
        self.host.replies.append({"ok": dict(BOUND)})
        self.assertIs(await computer_use.get_app("Example"), app)
        self.host.replies.append({"ok": dict(BOUND, handle=8, pid=5555)})
        self.assertIsNot(await computer_use.get_app("Example"), app)


class AppCallTests(ClientTestCase):
    async def test_every_method_sends_its_wire_form(self) -> None:
        app = await self.bind()
        await app.click(2)
        await app.click((10, 20.5), button="right", count=2)
        await app.click("2")  # pyright: ignore[reportArgumentType]
        await app.drag((1, 2), [3, 4])  # pyright: ignore[reportArgumentType]
        await app.scroll(0, "down", 2)
        await app.press_key("cmd+s")
        await app.type_text(42)  # pyright: ignore[reportArgumentType]
        await app.set_value(True, "x")  # pyright: ignore[reportArgumentType]
        await app.select_text(1, "ue", prefix="q")
        await app.perform_secondary_action(3, "AXShowMenu")
        await app.perform_secondary_action(3, 5)  # pyright: ignore[reportArgumentType]
        await app.paste("p", format="html")
        await app.activate()
        payloads = [payload for _, payload in self.host.requests]
        self.assertTrue(all(request == "computer_use.app" for request, _ in self.host.requests))
        self.assertTrue(all(payload.pop("handle") == 7 for payload in payloads))
        self.assertEqual(
            payloads,
            [
                {"method": "click", "target": {"kind": "index", "index": 2}, "button": "left", "count": 1},
                {"method": "click", "target": {"kind": "point", "point": {"repr": "(10, 20.5)", "x": 10.0, "y": 20.5}}, "button": "right", "count": 2},
                {"method": "click", "target": {"kind": "invalid", "type": "str"}, "button": "left", "count": 1},
                {"method": "drag", "from": {"repr": "(1, 2)", "x": 1.0, "y": 2.0}, "to": {"repr": "[3, 4]"}},
                {"method": "scroll", "target": {"kind": "index", "index": 0}, "direction": "down", "pages": 2},
                {"method": "press_key", "key": {"text": "cmd+s"}},
                {"method": "type_text", "text": {"type": "int"}},
                {"method": "set_value", "element_index": {"type": "bool"}, "value": "x"},
                {"method": "select_text", "element_index": {"index": 1}, "text": "ue", "prefix": "q", "suffix": None},
                {"method": "perform_secondary_action", "element_index": {"index": 3}, "action": {"name": "AXShowMenu"}},
                {"method": "perform_secondary_action", "element_index": {"index": 3}, "action": {"str": "5"}},
                {"method": "paste", "text": "p", "format": "html"},
                {"method": "activate"},
            ],
        )

    async def test_points_with_non_numbers_travel_by_repr_only(self) -> None:
        app = await self.bind()
        for point in ((1,), (1, "2"), (1, True), (1, 2, 3)):
            with self.subTest(point=point):
                await app.click(point)  # pyright: ignore[reportArgumentType]
                self.assertEqual(self.last()[1]["target"], {"kind": "point", "point": {"repr": repr(point)}})

    async def test_get_ax_state_updates_the_state(self) -> None:
        app = await self.bind()
        self.host.replies.append({"ok": "~[1] AXTextField 'Search'"})
        self.assertEqual(await app.get_ax_state(diff=False), "~[1] AXTextField 'Search'")
        self.assertEqual(self.last()[1], {"diff": False, "handle": 7, "method": "get_ax_state"})
        self.assertEqual(app.state, "~[1] AXTextField 'Search'")

    async def test_screenshots_attach_unless_told_not_to_and_swallow_attach_failures(self) -> None:
        app = await self.bind()
        shot = {"path": "/tmp/shot.png", "width": 400, "height": 300}
        self.host.replies.extend([{"ok": dict(shot)}, {"ok": dict(shot)}])
        self.assertEqual(await app.get_screenshot(), shot)
        self.assertEqual(await app.get_screenshot(attach=False), shot)
        self.assertEqual(self.attached, ["/tmp/shot.png"])

        async def failing(path: str) -> None:
            raise RuntimeError("non-vision model")

        sys.modules["attach_image"].run = failing  # pyright: ignore[reportAttributeAccessIssue]
        self.host.replies.append({"ok": dict(shot)})
        self.assertEqual((await app.get_screenshot())["width"], 400)

    async def test_text_regions_drop_the_path_and_attach_on_request(self) -> None:
        app = await self.bind()
        reply = {"regions": [], "width": 4, "height": 3, "path": "/tmp/ocr.png"}
        self.host.replies.extend([{"ok": dict(reply)}, {"ok": dict(reply)}])
        self.assertEqual(await app.get_text_regions(), {"regions": [], "width": 4, "height": 3})
        self.assertEqual(self.attached, [])
        await app.get_text_regions(attach=True)
        self.assertEqual(self.attached, ["/tmp/ocr.png"])

    async def test_state_and_screenshot_swallows_a_capture_failure(self) -> None:
        app = await self.bind()
        self.host.replies.extend([{"ok": "full"}, {"error": {"code": "TRANSPORT_ERROR", "message": "capture failed", "details": None}}])
        self.assertEqual(await app.get_state_and_screenshot(diff=False), {"state": "full", "screenshot": None})

    async def test_is_frontmost_is_a_synchronous_host_call(self) -> None:
        app = await self.bind()
        self.host.replies.append({"ok": True})
        self.assertTrue(app.is_frontmost())
        self.assertEqual(self.last(), ("computer_use.app", {"handle": 7, "method": "is_frontmost"}))
        self.host.replies.append({"error": {"code": "ACTION_UNSUPPORTED", "message": "focus control is not available", "details": {"platform": "linux"}}})
        with self.assertRaises(ComputerUseError) as caught:
            app.is_frontmost()
        self.assertEqual(caught.exception.code, "ACTION_UNSUPPORTED")


class ArgumentCheckTests(ClientTestCase):
    """The Python-typed checks run before any request, with the Python reprs."""

    async def test_click_buttons_and_counts(self) -> None:
        app = await self.bind()
        with self.assertRaises(ComputerUseError) as caught:
            await app.click(0, button="side")
        self.assert_invalid(caught.exception, "button must be one of left, right, middle, got 'side'", {"button": "side"})
        for count in (0, 11, True, "2"):
            with self.subTest(count=count), self.assertRaises(ComputerUseError) as caught:
                await app.click(0, count=count)  # pyright: ignore[reportArgumentType]
            self.assert_invalid(caught.exception, f"count must be an integer from 1 to 10, got {count!r}", {"count": count})
        self.assertEqual(self.host.requests, [])

    async def test_scroll_directions_and_pages(self) -> None:
        app = await self.bind()
        with self.assertRaises(ComputerUseError) as caught:
            await app.scroll(0, "diagonal")
        self.assert_invalid(caught.exception, "direction must be one of up, down, left, right, got 'diagonal'", {"direction": "diagonal"})
        for pages in (0, 1.5, False):
            with self.subTest(pages=pages), self.assertRaises(ComputerUseError) as caught:
                await app.scroll(0, "up", pages)  # pyright: ignore[reportArgumentType]
            self.assert_invalid(caught.exception, f"pages must be an integer of at least 1, got {pages!r}", {"pages": pages})
        self.assertEqual(self.host.requests, [])

    async def test_value_select_and_paste_arguments(self) -> None:
        app = await self.bind()
        with self.assertRaises(ComputerUseError) as caught:
            await app.set_value(1, 42)  # pyright: ignore[reportArgumentType]
        self.assert_invalid(caught.exception, "value must be a string, got int", {"value": "int"})
        with self.assertRaises(ComputerUseError) as caught:
            await app.select_text(1, "x", prefix=3)  # pyright: ignore[reportArgumentType]
        self.assert_invalid(caught.exception, "text, prefix, and suffix must be strings", {"text": "str"})
        with self.assertRaises(ComputerUseError) as caught:
            await app.select_text(1, "")
        self.assert_invalid(caught.exception, "text must be a non-empty string to select", {"text": ""})
        with self.assertRaises(ComputerUseError) as caught:
            await app.paste("text", format="rtf")
        self.assert_invalid(caught.exception, "format must be one of text, md, html, got 'rtf'", {"format": "rtf"})
        with self.assertRaises(ComputerUseError) as caught:
            await app.paste(5)  # pyright: ignore[reportArgumentType]
        self.assert_invalid(caught.exception, "text must be a string, got int", {"text": "int"})
        self.assertEqual(self.host.requests, [])


class PackagedInstructionsTests(unittest.TestCase):
    packaged_dir: Path
    created_root: bool

    def setUp(self) -> None:
        self.packaged_dir = Path(computer_use.__file__).resolve().parent / "references" / "app-instructions"
        self.created_root = not self.packaged_dir.parent.exists()
        self.addCleanup(self._cleanup)

    def _cleanup(self) -> None:
        if self.created_root and self.packaged_dir.parent.exists():
            shutil.rmtree(self.packaged_dir.parent)

    def test_the_packaged_guides_win_and_the_skill_dir_is_the_fallback(self) -> None:
        if self.created_root:
            self.assertTrue(computer_use._instructions_dir().endswith("skills/computer-use/references/app-instructions"))
            self.packaged_dir.mkdir(parents=True)
        self.assertEqual(computer_use._instructions_dir(), str(self.packaged_dir))

    def test_the_skill_dir_guides_are_keyed_by_bundle_id(self) -> None:
        guides = Path(__file__).resolve().parents[1] / "references" / "app-instructions"
        self.assertTrue((guides / "com.tinyspeck.slackmacgap.md").read_text().strip())
        self.assertTrue((guides / "notion.id.md").read_text().strip())

    def test_the_guides_ship_in_the_wheel(self) -> None:
        pyproject = Path(__file__).resolve().parents[1] / "pyproject.toml"
        with pyproject.open("rb") as handle:
            config = tomllib.load(handle)
        mapping = config["tool"]["hatch"]["build"]["targets"]["wheel"]["force-include"]
        self.assertEqual(mapping["references/app-instructions"], "computer_use/references/app-instructions")


class ModuleSurfaceTests(unittest.TestCase):
    def test_the_public_surface_is_unchanged(self) -> None:
        self.assertEqual(
            sorted(computer_use.__all__),
            ["App", "ComputerUseError", "get_app", "get_state", "list_apps", "permissions_status"],
        )
        self.assertIs(computer_use.ComputerUseError, errors.ComputerUseError)


if __name__ == "__main__":
    unittest.main()
