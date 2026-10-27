#!/usr/bin/env python3
"""Headless-camoufox web search backend for prime-agent's pa-web.

Reads one JSON request on argv[1] (or stdin): {"query": str, "count": int,
"profile": str}. Drives a headless Camoufox (Firefox) over DuckDuckGo's HTML
endpoint and prints one JSON object on stdout:

    {"results": [{"title": str, "url": str, "snippet": str}, ...]}
    {"error": "<message>"}  on failure

DuckDuckGo (not Google) is the target on purpose: Google wraps result links in
opaque `goto?url=...` redirect tokens that do not decode to real URLs, so its
results are useless to a search tool. DuckDuckGo's HTML endpoint returns the real
destination in each link's `uddg=` parameter, needs no JavaScript, and does not
gate on consent or CAPTCHA, so the scrape is clean and stable.

A persistent profile keeps cookies (and the drawn fingerprint) across searches.
"""

import json
import os
import sys
import urllib.parse


def main() -> int:
    raw = sys.argv[1] if len(sys.argv) > 1 else sys.stdin.read()
    try:
        req = json.loads(raw)
    except Exception as error:  # noqa: BLE001 - report any malformed request
        print(json.dumps({"error": f"bad request: {error}"}))
        return 2

    query = (req.get("query") or "").strip()
    count = int(req.get("count") or 10)
    if not query:
        print(json.dumps({"error": "empty query"}))
        return 2

    profile = req.get("profile") or os.path.expanduser(
        "~/.prime/agent/web-search/camoufox-profile"
    )
    os.makedirs(profile, exist_ok=True)

    try:
        from camoufox.sync_api import Camoufox
    except Exception as error:  # noqa: BLE001
        print(json.dumps({"error": f"camoufox not importable: {error}"}))
        return 3

    url = "https://html.duckduckgo.com/html/?q=" + urllib.parse.quote(query)
    try:
        with Camoufox(headless=True, persistent_context=True, user_data_dir=profile) as browser:
            page = browser.new_page()
            page.goto(url, wait_until="domcontentloaded", timeout=30000)
            try:
                page.wait_for_selector("a.result__a", timeout=12000)
            except Exception:  # noqa: BLE001 - no results is not fatal
                pass
            hits = page.evaluate(
                """(max) => {
                    const out = [];
                    for (const r of document.querySelectorAll('.result, .web-result')) {
                        const a = r.querySelector('a.result__a');
                        if (!a) continue;
                        let href = a.href || '';
                        const m = href.match(/[?&]uddg=([^&]+)/);
                        if (m) { try { href = decodeURIComponent(m[1]); } catch (_) {} }
                        if (!/^https?:\\/\\//.test(href)) continue;
                        const sn = r.querySelector('.result__snippet');
                        out.push({
                            title: (a.innerText || '').trim(),
                            url: href,
                            snippet: (sn ? sn.innerText : '').trim().slice(0, 300),
                        });
                        if (out.length >= max) break;
                    }
                    return out;
                }""",
                max(1, count),
            )
    except Exception as error:  # noqa: BLE001 - surface to the Rust caller
        print(json.dumps({"error": f"{type(error).__name__}: {error}"}))
        return 1

    print(json.dumps({"results": hits}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
