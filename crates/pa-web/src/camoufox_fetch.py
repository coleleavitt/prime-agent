#!/usr/bin/env python3
"""Headless-camoufox page fetch for prime-agent's pa-web.

Reads one JSON request on argv[1] (or stdin): {"url": str, "timeout_ms": int,
"profile": str}. Navigates a headless Camoufox (Firefox) to the URL -- getting
past bot-blocks (Cloudflare, "Just a moment...") that refuse a plain HTTP GET --
waits for any interstitial to clear, and prints one JSON object on stdout:

    {"final_url": str, "status": int, "content_type": str, "html": str}
    {"error": "<message>"}  on failure

An ephemeral (non-persistent) context is used on purpose: a persistent profile
takes a single-writer lock, so two concurrent fetches (or a fetch running beside
a camoufox search) would collide. Cloudflare clearance is re-solved per unique
URL instead; the Rust caller's 15-minute page cache keeps that cost down.
"""

import json
import sys
import time


def main() -> int:
    raw = sys.argv[1] if len(sys.argv) > 1 else sys.stdin.read()
    try:
        req = json.loads(raw)
    except Exception as error:  # noqa: BLE001 - report any malformed request
        print(json.dumps({"error": f"bad request: {error}"}))
        return 2

    url = (req.get("url") or "").strip()
    if not url:
        print(json.dumps({"error": "empty url"}))
        return 2
    timeout_ms = int(req.get("timeout_ms") or 45000)

    try:
        from camoufox.sync_api import Camoufox
    except Exception as error:  # noqa: BLE001
        print(json.dumps({"error": f"camoufox not importable: {error}"}))
        return 3

    try:
        with Camoufox(headless=True) as browser:
            page = browser.new_page()
            response = page.goto(url, wait_until="domcontentloaded", timeout=timeout_ms)
            status = response.status if response else 0
            content_type = ""
            if response:
                try:
                    content_type = response.headers.get("content-type", "")
                except Exception:  # noqa: BLE001 - headers are best effort
                    content_type = ""
            # Wait out a Cloudflare interstitial ("Just a moment..."): Camoufox
            # solves the JS challenge itself, then the page navigates to the real
            # content. Both the title check and content() race that navigation, so
            # each is retried until the page settles (or the deadline passes).
            deadline = time.time() + min(25.0, timeout_ms / 1000.0)
            html = ""
            final_url = url
            while time.time() < deadline:
                try:
                    page.wait_for_load_state("domcontentloaded", timeout=3000)
                except Exception:  # noqa: BLE001 - best effort
                    pass
                try:
                    title = (page.title() or "").lower()
                except Exception:  # noqa: BLE001 - navigating; try again
                    page.wait_for_timeout(500)
                    continue
                if "just a moment" in title or "checking your browser" in title:
                    page.wait_for_timeout(1000)
                    continue
                try:
                    candidate = page.content()
                except Exception:  # noqa: BLE001 - navigating; try again
                    page.wait_for_timeout(500)
                    continue
                html = candidate
                final_url = page.url
                break
            if not html:
                # A last settled attempt; a failure here surfaces to the caller.
                page.wait_for_load_state("domcontentloaded", timeout=5000)
                html = page.content()
                final_url = page.url
    except Exception as error:  # noqa: BLE001 - surface to the Rust caller
        print(json.dumps({"error": f"{type(error).__name__}: {error}"}))
        return 1

    print(
        json.dumps(
            {
                "final_url": final_url,
                "status": status,
                "content_type": content_type,
                "html": html,
            }
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
