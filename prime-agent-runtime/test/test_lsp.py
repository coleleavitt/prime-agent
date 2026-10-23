from __future__ import annotations

import asyncio
import importlib
import os
import unittest
from unittest.mock import AsyncMock, patch

rlm_module = importlib.import_module("rlm")


class RlmLspTest(unittest.TestCase):
    def test_definition_sends_an_absolute_path_and_reads_the_reply(self) -> None:
        reply = {
            "operation": "lsp.definition",
            "path": "/repo/src/a.rs",
            "server": "rust",
            "available": True,
            "text": "Defined in src/b.rs:3:4",
            "items": [{"path": "/repo/src/b.rs", "line": 3, "character": 4}, "junk"],
            "result_count": 1,
            "file_count": 1,
        }
        host_request = AsyncMock(return_value=reply)
        with patch.object(rlm_module, "host_request", host_request):
            result = asyncio.run(rlm_module.rlm.lsp.definition("src/a.rs", 10, 5))

        host_request.assert_awaited_once_with(
            "lsp.definition",
            {"path": os.path.abspath("src/a.rs"), "line": 10, "character": 5},
        )
        self.assertEqual(str(result), "Defined in src/b.rs:3:4")
        self.assertEqual(result.server, "rust")
        self.assertTrue(result.available)
        self.assertEqual(result.items, [{"path": "/repo/src/b.rs", "line": 3, "character": 4}])
        self.assertEqual((result.result_count, result.file_count), (1, 1))

    def test_every_operation_names_its_request(self) -> None:
        host_request = AsyncMock(return_value={"text": "t"})
        lsp = rlm_module.lsp
        path = os.path.abspath("a.py")
        with patch.object(rlm_module, "host_request", host_request):
            asyncio.run(lsp.references("a.py", 1, 2, include_declaration=False))
            asyncio.run(lsp.hover("a.py", 1, 2))
            asyncio.run(lsp.implementation("a.py", 1, 2))
            asyncio.run(lsp.document_symbols("a.py"))
            asyncio.run(lsp.workspace_symbols("Foo"))
            asyncio.run(lsp.workspace_symbols("Foo", path="a.py"))
            asyncio.run(lsp.call_hierarchy("a.py", 1, 2, direction="outgoing"))
            asyncio.run(lsp.diagnostics("a.py"))
        calls = [call.args for call in host_request.await_args_list]
        self.assertEqual(
            calls,
            [
                ("lsp.references", {"path": path, "line": 1, "character": 2, "include_declaration": False}),
                ("lsp.hover", {"path": path, "line": 1, "character": 2}),
                ("lsp.implementation", {"path": path, "line": 1, "character": 2}),
                ("lsp.document_symbols", {"path": path}),
                ("lsp.workspace_symbols", {"query": "Foo"}),
                ("lsp.workspace_symbols", {"query": "Foo", "path": path}),
                ("lsp.call_hierarchy", {"path": path, "line": 1, "character": 2, "direction": "outgoing"}),
                ("lsp.diagnostics", {"path": path}),
            ],
        )

    def test_an_unserved_file_reads_as_unavailable(self) -> None:
        reply = {"available": False, "text": "No LSP server available for file type: .txt", "items": []}
        with patch.object(rlm_module, "host_request", AsyncMock(return_value=reply)):
            result = asyncio.run(rlm_module.lsp.hover("notes.txt", 1, 1))
        self.assertFalse(result.available)
        self.assertEqual(str(result), "No LSP server available for file type: .txt")

    def test_refuses_bad_input_before_asking_the_host(self) -> None:
        host_request = AsyncMock()
        lsp = rlm_module.lsp
        with patch.object(rlm_module, "host_request", host_request):
            with self.assertRaises(ValueError):
                asyncio.run(lsp.definition("a.rs", 0, 1))
            with self.assertRaises(ValueError):
                asyncio.run(lsp.definition("a.rs", 1, True))
            with self.assertRaises(TypeError):
                asyncio.run(lsp.hover(3, 1, 1))  # type: ignore[arg-type]
            with self.assertRaises(ValueError):
                asyncio.run(lsp.call_hierarchy("a.rs", 1, 1, direction="sideways"))
            with self.assertRaises(ValueError):
                asyncio.run(lsp.workspace_symbols(" "))
        host_request.assert_not_awaited()

    def test_servers_returns_the_host_reply(self) -> None:
        reply = {"text": "rust (rust-analyzer) running", "servers": [], "errors": []}
        host_request = AsyncMock(return_value=reply)
        with patch.object(rlm_module, "host_request", host_request):
            self.assertEqual(asyncio.run(rlm_module.rlm.lsp.servers()), reply)
        host_request.assert_awaited_once_with("lsp.servers", {})


if __name__ == "__main__":
    unittest.main()
