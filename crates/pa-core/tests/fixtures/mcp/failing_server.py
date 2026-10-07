"""A stdio MCP server that crashes during startup after writing a noisy,
secret-bearing stderr (the startup-diagnostic redaction fixture)."""

import os
import sys
from pathlib import Path

Path(sys.argv[1]).write_text(str(os.getpid()))
secret = os.environ["FIXTURE_SECRET"]
print("\x1b[31mImportError:\x00 broken " + secret + "\x1b[0m", file=sys.stderr, flush=True)
for index in range(300):
    print(f"oversized diagnostic line {index:04d} " + "x" * 100, file=sys.stderr)
print("sentinel tail " + secret, file=sys.stderr, flush=True)
raise ImportError("fixture startup crash")
