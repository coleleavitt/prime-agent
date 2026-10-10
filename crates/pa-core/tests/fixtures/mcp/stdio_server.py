"""A local fake stdio MCP server (stdlib only) for the host session tests.

Behaviour is chosen by environment variables (all optional):

- FIXTURE_PID_FILE: write this process's pid there at startup.
- FIXTURE_EVENTS_FILE: append one JSON line per received notification there
  (``notifications/cancelled`` carries the cancelled request id).
- FIXTURE_STDERR_NOTE: write this line to stderr at startup.
- FIXTURE_TOOLS_PAGES: JSON list of ``[[tool names], next_cursor]`` pages
  served by successive tools/list calls (the last repeats; default: one page with ``fixture/raw.tool``, ``slow``,
  ``denied``, ``other``, ``fail``).
- FIXTURE_HOLD_INITIALIZE: path; ``initialize`` is answered only once it
  exists.
- FIXTURE_INITIALIZE_DELAY_MS: answer ``initialize`` only after this many
  milliseconds (a server with a slow handshake).
- FIXTURE_IGNORE_EOF: keep running after stdin closes (and ignore SIGTERM).

Tools: ``fixture/raw.tool`` echoes argv, cwd, two env values and the
arguments as JSON text; ``slow`` answers only when cancelled (never);
``fail`` returns an ``isError`` result; ``structured`` returns
structuredContent.
"""

import json
import os
import signal
import sys
import threading
import time

if pid_file := os.environ.get("FIXTURE_PID_FILE"):
    with open(pid_file, "w") as handle:
        handle.write(str(os.getpid()))
if note := os.environ.get("FIXTURE_STDERR_NOTE"):
    print(note, file=sys.stderr, flush=True)

lock = threading.Lock()
list_calls = [0]


def send(message):
    with lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def record(event):
    if path := os.environ.get("FIXTURE_EVENTS_FILE"):
        with lock, open(path, "a") as handle:
            handle.write(json.dumps(event) + "\n")


def tool(name):
    return {"name": name, "description": f"{name} fixture", "inputSchema": {"type": "object"}}


pages = json.loads(
    os.environ.get("FIXTURE_TOOLS_PAGES")
    or json.dumps([[["fixture/raw.tool", "slow", "denied", "other", "fail", "structured"], None]])
)


def answer(request):
    method = request.get("method")
    params = request.get("params") or {}
    if method == "initialize":
        time.sleep(int(os.environ.get("FIXTURE_INITIALIZE_DELAY_MS") or 0) / 1000)
        hold = os.environ.get("FIXTURE_HOLD_INITIALIZE")
        while hold and not os.path.exists(hold):
            time.sleep(0.01)
        return {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1"},
        }
    if method == "tools/list":
        record({"method": "tools/list", "cursor": params.get("cursor")})
        names, next_cursor = pages[min(list_calls[0], len(pages) - 1)]
        list_calls[0] += 1
        result = {"tools": [tool(name) for name in names]}
        if next_cursor is not None:
            result["nextCursor"] = next_cursor
        return result
    if method == "tools/call":
        name = params.get("name")
        arguments = params.get("arguments", {})
        record({"method": "tools/call", "name": name})
        if name == "slow":
            return None
        if name == "fail":
            return {"content": [{"type": "text", "text": "redacted failure"}], "isError": True}
        if name == "structured":
            return {"content": [{"type": "text", "text": "ignored"}], "structuredContent": arguments}
        payload = {
            "args": sys.argv[1:],
            "cwd": os.getcwd(),
            "env": os.environ.get("FIXTURE_ENV"),
            "ambient": os.environ.get("UNRELATED"),
            "path": os.environ.get("PATH"),
            "arguments": arguments,
        }
        return {"content": [{"type": "text", "text": json.dumps(payload)}]}
    return {}


def main():
    if os.environ.get("FIXTURE_IGNORE_EOF"):
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    for line in sys.stdin:
        request = json.loads(line)
        if request.get("id") is None:
            record({"method": request.get("method"), "params": request.get("params")})
            continue
        result = answer(request)
        if result is not None:
            send({"jsonrpc": "2.0", "id": request["id"], "result": result})
    while os.environ.get("FIXTURE_IGNORE_EOF"):
        time.sleep(1)


main()
