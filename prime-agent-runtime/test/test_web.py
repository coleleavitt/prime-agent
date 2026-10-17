from __future__ import annotations

import asyncio
import importlib
import unittest
from unittest.mock import AsyncMock, patch

rlm_module = importlib.import_module("rlm")


class RlmWebSearchTest(unittest.TestCase):
    def test_sends_the_query_and_filters_and_reads_the_reply(self) -> None:
        reply = {
            "query": "rust 2026",
            "backend": "anthropic",
            "model": "anthropic/claude-haiku-4-5",
            "results": [
                {"title": "Rust", "url": "https://www.rust-lang.org/"},
                {"title": "Docs", "url": "https://doc.rust-lang.org/", "snippet": "The book"},
                {"title": "no url"},
            ],
            "searches": [{"query": "rust 2026", "results": []}],
            "commentary": ["Rust is a language."],
            "errors": [],
            "search_count": 1,
            "duration_ms": 12,
            "text": 'Web search results for query: "rust 2026"\n\nREMINDER: ...',
        }
        host_request = AsyncMock(return_value=reply)
        with patch.object(rlm_module, "host_request", host_request):
            result = asyncio.run(rlm_module.rlm.web_search("rust 2026", allowed_domains=["rust-lang.org"]))

        host_request.assert_awaited_once_with(
            "web.search", {"query": "rust 2026", "allowed_domains": ["rust-lang.org"]}
        )
        self.assertEqual(result.backend, "anthropic")
        self.assertEqual(result.model, "anthropic/claude-haiku-4-5")
        self.assertEqual(
            result.results,
            [
                rlm_module.WebSearchHit(title="Rust", url="https://www.rust-lang.org/"),
                rlm_module.WebSearchHit(title="Docs", url="https://doc.rust-lang.org/", snippet="The book"),
            ],
        )
        self.assertEqual(result.search_count, 1)
        self.assertEqual(result.commentary, ["Rust is a language."])
        self.assertEqual(str(result), reply["text"])

    def test_refuses_bad_input_before_asking_the_host(self) -> None:
        host_request = AsyncMock()
        with patch.object(rlm_module, "host_request", host_request):
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.web_search("x"))
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.web_search("rust", ["a.com"], ["b.com"]))
            with self.assertRaises(TypeError):
                asyncio.run(rlm_module.web_search("rust", allowed_domains=[1]))  # type: ignore[list-item]
        host_request.assert_not_awaited()

    def test_a_single_domain_string_is_a_list(self) -> None:
        host_request = AsyncMock(return_value={"text": "t", "results": []})
        with patch.object(rlm_module, "host_request", host_request):
            asyncio.run(rlm_module.web_search("rust", blocked_domains="reddit.com"))  # type: ignore[arg-type]
        host_request.assert_awaited_once_with("web.search", {"query": "rust", "blocked_domains": ["reddit.com"]})


class RlmWebFetchTest(unittest.TestCase):
    def test_sends_the_url_prompt_and_offset_and_reads_the_reply(self) -> None:
        reply = {
            "url": "https://example.com/doc",
            "final_url": "https://example.com/doc",
            "code": 200,
            "code_text": "OK",
            "content_type": "text/html",
            "bytes": 42,
            "kind": "html",
            "content": "# Doc",
            "total_chars": 100005,
            "offset": 100000,
            "next_offset": None,
            "answer": "It documents things.",
            "answer_model": "anthropic/claude-haiku-4-5",
            "redirect_url": None,
            "cached": True,
            "duration_ms": 3,
            "text": "It documents things.",
        }
        host_request = AsyncMock(return_value=reply)
        with patch.object(rlm_module, "host_request", host_request):
            result = asyncio.run(
                rlm_module.rlm.web_fetch("https://example.com/doc", "What is it?", offset=100000)
            )

        host_request.assert_awaited_once_with(
            "web.fetch", {"url": "https://example.com/doc", "prompt": "What is it?", "offset": 100000}
        )
        self.assertEqual(
            result,
            rlm_module.WebFetchResult(
                url="https://example.com/doc",
                final_url="https://example.com/doc",
                code=200,
                code_text="OK",
                kind="html",
                text="It documents things.",
                content="# Doc",
                content_type="text/html",
                bytes=42,
                total_chars=100005,
                offset=100000,
                next_offset=None,
                answer="It documents things.",
                answer_model="anthropic/claude-haiku-4-5",
                redirect_url=None,
                cached=True,
                duration_ms=3,
            ),
        )
        self.assertEqual(str(result), "It documents things.")

    def test_a_plain_fetch_sends_only_the_url(self) -> None:
        host_request = AsyncMock(return_value={"kind": "redirect", "redirect_url": "https://b.com/", "code": 301})
        with patch.object(rlm_module, "host_request", host_request):
            result = asyncio.run(rlm_module.web_fetch("https://a.com/"))
        host_request.assert_awaited_once_with("web.fetch", {"url": "https://a.com/"})
        self.assertEqual(result.redirect_url, "https://b.com/")
        self.assertEqual(result.code, 301)
        self.assertIsNone(result.next_offset)

    def test_refuses_bad_input_before_asking_the_host(self) -> None:
        host_request = AsyncMock()
        with patch.object(rlm_module, "host_request", host_request):
            with self.assertRaises(TypeError):
                asyncio.run(rlm_module.web_fetch(""))
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.web_fetch("https://a.com/", offset=-1))
        host_request.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
