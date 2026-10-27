# pa-web

Claude Code's WebSearch and WebFetch for the kernel harness: the host half of `rlm.web_search` and `rlm.web_fetch`.
The kernel runtime (`prime-agent-runtime/src/rlm/web.py`) sends the `web.search` and `web.fetch` host requests; this
crate serves them in the session worker (never on the TUI's paint path).

Wired by `pa-cli` behind `feature = "web"` (default on). `--no-default-features` registers nothing: the kernel's
`rlm.web_search` / `rlm.web_fetch` fail with the standard unregistered-request error.

## Scope

- `web.search` (`query`, `allowed_domains` or `blocked_domains`): one query on a pluggable backend
  (`search::SearchBackend`).
  - **Anthropic** (`search::anthropic`): Anthropic's server-side `web_search_20250305` tool, run like Claude Code's
    WebSearch: a nested Messages call with the tool attached (`max_uses` 8, the domain filters), forced through
    `tool_choice` unless the model always thinks, Claude Code's system prompt ("You run a web search for another
    Claude model ... Search once, then reply with a brief summary"), and the `server_tool_use` /
    `web_search_tool_result` blocks read off the stream through `pa_ai::types::StreamOptions::on_event`. The model is
    `web.searchModel` when set; else the session's model when it is a first-party `anthropic` Messages model; else the
    cheapest authenticated first-party Anthropic model (haiku, then sonnet). Credentials resolve through the model
    registry, so the shared account store (`pa-anthropic-auth`), `auth.json` and `ANTHROPIC_API_KEY` all serve, with the
    provider's OAuth identity and request hooks.
  - **Camoufox** (`search::camoufox`): a real, keyless headless browser. A bundled Python script drives a headless
    Camoufox (Firefox) over DuckDuckGo's HTML endpoint and returns clean result URLs; the domain filter rides the
    query as `site:` operators and is applied again to the hits. DuckDuckGo, not Google: Google wraps every result
    link in an opaque `goto?url=` redirect token that does not decode to a real URL, while DuckDuckGo's HTML endpoint
    exposes the destination (`uddg=`), needs no JavaScript, and does not gate on consent or CAPTCHA. The browser must
    be installed in the chosen interpreter (`pip install camoufox[geoip] && camoufox fetch`); when it is not, the
    search errors and `auto` falls back to Anthropic.
  - The reply carries the hits (deduplicated), each search's query and hits, the model's commentary, and `text`:
    Claude Code's tool-result text (`Web search results for query: ...`, `Links: [...]`, the Sources reminder).
- `web.fetch` (`url`, optional `prompt`, `offset`): Claude Code's WebFetch, minus its server-side domain preflight.
  - Backend (`fetch::FetchBackend`, `web.fetchBackend`): `auto` (default) does a plain HTTP GET and, when the site
    bot-blocks it (HTTP 403/503 or a reset connection), retries through a headless Camoufox that navigates the page,
    waits out any "Just a moment" interstitial, and returns the rendered HTML (converted to Markdown like any page);
    `http` never launches the browser; `camoufox` always uses the browser (HTTP only when the browser cannot run). The
    browser follows redirects itself, so its final URL is re-validated against the same host rules (an SSRF guard), and
    it needs `camoufox` installed in the configured interpreter (shared with search; an ephemeral context, so no
    profile lock).
  - URL rules (`fetch::validate_url`): http(s) only, at most 2000 characters, no credentials; `http` upgraded to
    `https`; localhost, dotless, `.local`, loopback, private, link-local and CGNAT hosts refused ("use bash() with
    curl"). A hostname that resolves to a private address is not checked.
  - Redirects: followed only within the same host (a `www.` prefix aside, same scheme and port, at most 10); a
    redirect elsewhere is reported (`redirect_url`, Claude Code's REDIRECT DETECTED text), not followed.
  - Limits: a 60 s deadline, a 10 MiB body. Successful pages are cached for 15 minutes (50 MiB, oldest evicted),
    process-wide.
  - HTML is converted to Markdown with `htmd` (script, style, noscript, iframe, template, svg, canvas and head
    dropped; at most the first MiB, then `[Content truncated due to length...]`); Markdown and text types are kept;
    a binary body is described, not returned.
  - The reply shows at most 100,000 characters from `offset`, fenced in `<web-content>` with Claude Code's
    untrusted-content warning, and says how to read on (`next_offset`).
  - `prompt`: answered over that window by the answering model (`web.fetchModel`, else the `auxiliaryModel`
    setting) with Claude Code's WebFetch answering rules; the answer replaces the page text and notes what it did not
    read. Without an answering model (or when it fails), the page text comes back with a note.

## Settings

```json
"web": {
  "searchBackend": "auto",
  "fetchBackend": "auto",
  "searchModel": "anthropic/claude-haiku-4-5",
  "fetchModel": "anthropic/claude-haiku-4-5",
  "camoufoxPython": "python3"
}
```

`searchBackend` is `auto` (Camoufox when its browser is available, else Anthropic when a model resolves), `anthropic`
or `camoufox`; `PRIME_AGENT_WEB_SEARCH_BACKEND` overrides it. `fetchBackend` is `auto` (HTTP, browser on a bot-block),
`http` or `camoufox`; `PRIME_AGENT_WEB_FETCH_BACKEND` overrides it. `camoufoxPython` is the interpreter that has
`camoufox` installed (default `python3`, shared by search and fetch); `PRIME_AGENT_WEB_CAMOUFOX_PYTHON` stands in when
the setting is unset. Every model honours the global `allowedModels` pin.

## Non-goals

- Claude Code's `domain_info` preflight (an Anthropic service) and its preapproved-domain list: permission rules
  belong to the session's policy seams (`review_host_request` sees `web.search` / `web.fetch`).
- Saving binary bodies to files, the CCR proxy routes, citations-mode search result blocks, per-session search
  budgets.

## Public API

`WebFeature` (`new`, `with_overrides(WebOverrides)`), `resolve_backend`, the request and event names
(`SEARCH_REQUEST`, `FETCH_REQUEST`, `SEARCH_EVENT`, `FETCH_EVENT`), and the modules `config`, `fetch`, `html`,
`model`, `render`, `search` (the `SearchBackend` trait, the `camoufox` and `anthropic` backends, and `FallbackSearch`).

## Seams

- `pa_core::features::SessionFeature::register_host_handlers` (`web.search`, `web.fetch`).
- `pa_ai::types::StreamOptions::{on_payload, on_event}`: the server tool is added to the built payload and its result
  blocks are observed on the stream.

## Files

None written. Settings are read from the session's `settings.json` files.

## Telemetry

- `web search` (catalog v4): `backend`, `outcome` (`ok`, `error`, `no_backend`, `invalid`, `cancelled`),
  `domain_filter`, `result_count`, `search_count`, `duration_ms`.
- `web fetch` (catalog v4): `outcome` (`page`, `redirect`, `http_error`, `refused`, `error`, `cancelled`), `kind`,
  `status_code`, `cached`, `truncated`, `prompt` (`none`, `given`, `answered`, `no_model`, `failed`), `duration_ms`.

Never a query, domain, URL, prompt, page or answer.
