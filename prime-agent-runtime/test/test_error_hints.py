"""Suggestion-line unit tests: `rlm.repl._suggestion_line` is the kernel-side
hint the error event appends to the traceback so the model can self-correct
on the common recurring runtime errors (NameError, FileNotFoundError,
ModuleNotFoundError, AttributeError). The ledger audit of 2026-11-10 found
400+ unaddressed NameErrors and 261 unaddressed FileNotFoundErrors; this
test pins the hint's shape so a future refactor can't silently drop it.
"""

from __future__ import annotations

import os
import tempfile
import unittest

from rlm.repl import _suggestion_line


def _raise(exc_factory):
    try:
        exc_factory()
    except BaseException as exc:
        return exc
    raise AssertionError("factory did not raise")


class SuggestionLineTest(unittest.TestCase):
    def test_name_error_hint_names_the_nearest_binding(self):
        def factory():
            unique_binding = 1  # noqa: F841 - lives on the frame for the hint
            # NameError: typo of the local
            unique_bindng  # noqa: F821

        hint = _suggestion_line(_raise(factory))
        self.assertIsNotNone(hint)
        assert hint is not None  # for the type checker
        self.assertIn("unique_binding", hint)
        self.assertIn("nearest names in scope", hint)

    def test_name_error_hint_covers_common_stdlib_names(self):
        def factory():
            # No `import os`; the hint still suggests os because it's in
            # the common-import allowlist and a close match to `o`.
            oss  # noqa: F821

        hint = _suggestion_line(_raise(factory))
        self.assertIsNotNone(hint)
        assert hint is not None
        self.assertIn("os", hint)

    def test_module_not_found_hint_names_the_install_command(self):
        def factory():
            __import__("definitely_not_a_module_xyz123")

        hint = _suggestion_line(_raise(factory))
        self.assertIsNotNone(hint)
        assert hint is not None
        self.assertIn("definitely_not_a_module_xyz123", hint)
        self.assertIn("uv pip install", hint)

    def test_file_not_found_hint_lists_near_siblings(self):
        with tempfile.TemporaryDirectory() as tmp:
            # Create siblings so difflib has something to suggest.
            for name in ("config.yaml", "config.json", "setup.py"):
                open(os.path.join(tmp, name), "w").close()
            target = os.path.join(tmp, "confg.yaml")

            def factory():
                open(target, "r").close()

            hint = _suggestion_line(_raise(factory))
            self.assertIsNotNone(hint)
            assert hint is not None
            self.assertIn("config.yaml", hint)
            self.assertIn(tmp, hint)

    def test_file_not_found_hint_no_parent_skips_cleanly(self):
        # A purely invented parent dir has no siblings to suggest; the hint
        # returns None rather than guessing.
        def factory():
            open("/this/surely/does/not/exist/anywhere.txt", "r").close()

        hint = _suggestion_line(_raise(factory))
        self.assertIsNone(hint)

    def test_attribute_error_hint_lists_nearest_attributes(self):
        class Box:
            def unique_method(self):
                return 1

            def other_method(self):
                return 2

        box = Box()

        def factory():
            box.unique_methd  # noqa: B018 - raises AttributeError

        hint = _suggestion_line(_raise(factory))
        self.assertIsNotNone(hint)
        assert hint is not None
        self.assertIn("unique_method", hint)
        # The `Box` class name identifies the subject.
        self.assertIn("Box", hint)

    def test_other_exception_returns_none(self):
        def factory():
            raise ValueError("unrelated")

        self.assertIsNone(_suggestion_line(_raise(factory)))

    def test_never_raises_on_broken_exc_metadata(self):
        # A broken NameError without a traceback must not crash the hint.
        exc = NameError("missing")
        self.assertIsNone(_suggestion_line(exc))


if __name__ == "__main__":
    unittest.main()
