"""Web search and web fetch (Claude Code's WebSearch and WebFetch).

``rlm.web_search(query)`` searches the web through the host's configured
backend (Anthropic's server-side web search when the user has an Anthropic
login or key, else Brave or Exa with an API key) and returns the hits with
links. ``rlm.web_fetch(url)`` fetches one page, converted to Markdown; with
``prompt`` the host's small model answers the prompt over the page instead.

Both are served by the host's ``web`` feature (``web.search`` / ``web.fetch``
host requests); a build without it raises the standard unregistered-request
error. Printing a result prints the text the model should read: the search
text ends with Claude Code's Sources reminder, and fetched page text is
fenced as untrusted data.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class WebSearchHit:
    title: str
    url: str
    snippet: str | None = None


@dataclass(frozen=True)
class WebSearchResult:
    """One search: its hits (deduplicated across the backend's searches),
    the backend and model that ran it, and ``text``, the rendered result."""

    query: str
    backend: str
    results: list[WebSearchHit]
    text: str
    model: str | None = None
    searches: list[dict[str, Any]] = field(default_factory=list)
    commentary: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    search_count: int = 0
    duration_ms: int = 0

    def __str__(self) -> str:
        return self.text


@dataclass(frozen=True)
class WebFetchResult:
    """One fetch. ``content`` is the shown window of the page as Markdown
    (``next_offset`` says where the next window starts when the page was
    longer); ``answer`` is the small model's answer to ``prompt`` when one
    ran; ``redirect_url`` names a redirect to another host, which is not
    followed. ``text`` is what to read: the answer, or the fenced page."""

    url: str
    final_url: str
    code: int
    code_text: str
    kind: str
    text: str
    content: str = ""
    content_type: str = ""
    bytes: int = 0
    total_chars: int = 0
    offset: int = 0
    next_offset: int | None = None
    answer: str | None = None
    answer_model: str | None = None
    redirect_url: str | None = None
    cached: bool = False
    duration_ms: int = 0

    def __str__(self) -> str:
        return self.text


def _domains(label: str, value: Any) -> list[str] | None:
    if value is None:
        return None
    if isinstance(value, str):
        value = [value]
    if not isinstance(value, (list, tuple)) or not all(isinstance(item, str) for item in value):
        raise TypeError(f"{label} must be a list of domain names")
    return list(value)


def _int(value: Any, default: int = 0) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else default


async def web_search(
    query: str,
    allowed_domains: list[str] | None = None,
    blocked_domains: list[str] | None = None,
) -> WebSearchResult:
    """Search the web for ``query``.

    ``allowed_domains`` keeps only results from those domains (and their
    subdomains); ``blocked_domains`` drops them; pass at most one of the two.
    After answering from the results, end with a "Sources:" list of the URLs
    you used as markdown links.
    """
    if not isinstance(query, str):
        raise TypeError(f"query must be str, got {type(query).__name__}")
    if len(query.strip()) < 2:
        raise ValueError("query must have at least 2 characters")
    allowed = _domains("allowed_domains", allowed_domains)
    blocked = _domains("blocked_domains", blocked_domains)
    if allowed and blocked:
        raise ValueError("Cannot specify both allowed_domains and blocked_domains in the same request")
    from . import host_request

    payload: dict[str, Any] = {"query": query}
    if allowed is not None:
        payload["allowed_domains"] = allowed
    if blocked is not None:
        payload["blocked_domains"] = blocked
    reply = await host_request("web.search", payload)
    hits = [
        WebSearchHit(
            title=str(hit.get("title") or ""),
            url=str(hit.get("url") or ""),
            snippet=hit.get("snippet") if isinstance(hit.get("snippet"), str) else None,
        )
        for hit in reply.get("results") or []
        if isinstance(hit, dict) and hit.get("url")
    ]
    return WebSearchResult(
        query=str(reply.get("query") or query),
        backend=str(reply.get("backend") or ""),
        results=hits,
        text=str(reply.get("text") or ""),
        model=reply.get("model") if isinstance(reply.get("model"), str) else None,
        searches=[item for item in reply.get("searches") or [] if isinstance(item, dict)],
        commentary=[str(item) for item in reply.get("commentary") or []],
        errors=[str(item) for item in reply.get("errors") or []],
        search_count=_int(reply.get("search_count")),
        duration_ms=_int(reply.get("duration_ms")),
    )


async def web_fetch(url: str, prompt: str | None = None, *, offset: int = 0) -> WebFetchResult:
    """Fetch ``url`` (http is upgraded to https) and return it as Markdown.

    With ``prompt``, the host's small model (``web.fetchModel`` or the
    ``auxiliaryModel`` setting) answers it over the page; without one
    configured the page text comes back. A long page is windowed: call again
    with ``offset=result.next_offset`` to read on (pages are cached for 15
    minutes). A redirect to another host is reported, not followed.
    """
    if not isinstance(url, str) or not url.strip():
        raise TypeError("url must be a non-empty str")
    if prompt is not None and not isinstance(prompt, str):
        raise TypeError(f"prompt must be str or None, got {type(prompt).__name__}")
    if not isinstance(offset, int) or isinstance(offset, bool) or offset < 0:
        raise ValueError("offset must be a non-negative int")
    from . import host_request

    payload: dict[str, Any] = {"url": url}
    if prompt is not None:
        payload["prompt"] = prompt
    if offset:
        payload["offset"] = offset
    reply = await host_request("web.fetch", payload)
    next_offset = reply.get("next_offset")
    return WebFetchResult(
        url=str(reply.get("url") or url),
        final_url=str(reply.get("final_url") or reply.get("url") or url),
        code=_int(reply.get("code")),
        code_text=str(reply.get("code_text") or ""),
        kind=str(reply.get("kind") or ""),
        text=str(reply.get("text") or ""),
        content=str(reply.get("content") or ""),
        content_type=str(reply.get("content_type") or ""),
        bytes=_int(reply.get("bytes")),
        total_chars=_int(reply.get("total_chars")),
        offset=_int(reply.get("offset")),
        next_offset=next_offset if isinstance(next_offset, int) and not isinstance(next_offset, bool) else None,
        answer=reply.get("answer") if isinstance(reply.get("answer"), str) else None,
        answer_model=reply.get("answer_model") if isinstance(reply.get("answer_model"), str) else None,
        redirect_url=reply.get("redirect_url") if isinstance(reply.get("redirect_url"), str) else None,
        cached=reply.get("cached") is True,
        duration_ms=_int(reply.get("duration_ms")),
    )
