"""The host-side stderr tee: raw fd-2 bytes reach the host's stderr pipe even
when the kernel dies right after writing them (native exit()/abort()), while
protocol stream events and drain tokens stay exactly as before."""

from __future__ import annotations

import os
import select
import subprocess
import sys
import time
import unittest

from test_repl import ReplProcess, one, stream_text

SRC = os.path.join(os.path.dirname(__file__), "..", "src")
sys.path.insert(0, SRC)

from rlm import repl as repl_module  # noqa: E402

FATAL_LINE = b"idalib: database load failed, exiting\n"


def kill_quietly(pid: int) -> None:
    try:
        os.kill(pid, 9)
    except ProcessLookupError:
        pass


def read_host_stderr(fd: int, deadline: float, until_eof: bool = False) -> tuple[bytes, bool]:
    """Collect bytes off the host's read end until ``deadline`` (or EOF when asked); reports EOF."""
    chunks: list[bytes] = []
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return b"".join(chunks), False
        readable, _, _ = select.select([fd], [], [], remaining)
        if not readable:
            if until_eof:
                continue
            return b"".join(chunks), False
        chunk = os.read(fd, 65536)
        if not chunk:
            return b"".join(chunks), True
        chunks.append(chunk)


@unittest.skipUnless(hasattr(os, "fork"), "the forked stderr tee is POSIX-only")
class StderrTeeProcessTest(unittest.TestCase):
    def setUp(self) -> None:
        self.repl = ReplProcess(stderr=subprocess.PIPE)
        self.addCleanup(self.repl.close)
        assert self.repl.proc.stderr is not None
        self.host_fd = self.repl.proc.stderr.fileno()
        ready, _ = self.repl.ready()
        self.assertEqual(ready["event"], "ready")

    def test_fatal_exit_after_raw_stderr_write_reaches_host(self):
        for label, exit_call in (
            ("os._exit", "os._exit(1)"),
            ("libc exit", "import ctypes; ctypes.CDLL(None).exit(1)"),
        ):
            with self.subTest(exit_call=label):
                if label != "os._exit":
                    self.repl.close()
                    self.setUp()
                self.repl.send(
                    {"type": "execute", "id": "fatal", "code": f"import os\nos.write(2, {FATAL_LINE!r})\n{exit_call}"}
                )
                self.assertEqual(self.repl.proc.wait(timeout=30), 1)
                # The host destroys its stderr stream one event-loop turn after
                # 'exit': the bytes must already be in (or about to land in) the pipe.
                received, _ = read_host_stderr(self.host_fd, time.monotonic() + 2.0)
                self.assertIn(FATAL_LINE, received)
                self.assertNotIn(b"\xff<drain:", received)
                # No done ever arrives for the crashed cell; the stream just ends.
                while True:
                    try:
                        event = self.repl.read_event()
                    except EOFError:
                        break
                    self.assertNotEqual(event.get("event"), "done")

    def test_cell_stderr_stays_on_protocol_and_host_copy_has_no_tokens(self):
        code = "\n".join(
            [
                "import os, sys",
                "sys.stderr.write('py-err\\n')",
                "os.write(2, b'fd-err\\n')",
                "sys.stderr.buffer.write(b'buf-err\\n')",
                "os.write(1, b'fd-out\\n')",
            ]
        )
        events = self.repl.execute("io", code)
        self.assertEqual(events[-1]["event"], "done")
        self.assertEqual(one(events, "done")["status"], "ok")
        err = stream_text(events, "stderr")
        self.assertIn("py-err", err)
        self.assertIn("fd-err", err)
        self.assertIn("buf-err", err)
        for event in events:
            if event.get("event") != "stderr":
                continue
            self.assertEqual(event["id"], "io" if "py-" in event["text"] else None)
        # Lookalikes exercise the token filter: a bare 0xff, a token head with a
        # bad body, and a chunk ending in a genuine token prefix.
        lookalikes = b"\xff<drain:not-hex>\xff|\xff|tail\xff<drain:0123"
        events = self.repl.execute("lookalike", f"import os\nos.write(2, {lookalikes!r})")
        self.assertEqual(one(events, "done")["status"], "ok")
        # Every cell drains both pumps, so each of these writes a token to fd 2.
        for i in range(5):
            self.assertEqual(one(self.repl.execute(f"quiet-{i}", "None"), "done")["status"], "ok")
        self.assertEqual(self.repl.shutdown(), 0)
        # The tee exits on fd-2 EOF once the kernel is gone, closing the host pipe.
        received, eof = read_host_stderr(self.host_fd, time.monotonic() + 10.0, until_eof=True)
        self.assertTrue(eof)
        self.assertEqual(received, b"fd-err\nbuf-err\n" + lookalikes)
        self.assertNotIn(b"fd-out", received)

    def test_tee_exits_after_kernel_death_despite_grandchild_holding_fd2(self):
        code = "\n".join(
            [
                "import os, subprocess",
                "child = subprocess.Popen(['sleep', '30'])",
                "os.write(2, b'before-kill\\n')",
                "child.pid",
            ]
        )
        events = self.repl.execute("spawn", code)
        self.assertEqual(one(events, "done")["status"], "ok")
        grandchild = int(one(events, "result")["text"])
        self.addCleanup(kill_quietly, grandchild)
        self.repl.proc.kill()
        self.assertEqual(self.repl.proc.wait(timeout=10), -9)
        # sleep inherited the kernel's fd 2, so the tee never sees EOF; it must
        # notice the dead parent and close the host pipe on its own.
        received, eof = read_host_stderr(self.host_fd, time.monotonic() + 10.0, until_eof=True)
        self.assertTrue(eof)
        self.assertEqual(received, b"before-kill\n")

    def test_tee_exits_after_kernel_death_despite_chatty_grandchild(self):
        code = "\n".join(
            [
                "import subprocess",
                "child = subprocess.Popen(['sh', '-c', 'exec yes noise >&2'])",
                "child.pid",
            ]
        )
        events = self.repl.execute("spawn", code)
        self.assertEqual(one(events, "done")["status"], "ok")
        grandchild = int(one(events, "result")["text"])
        self.addCleanup(kill_quietly, grandchild)
        self.repl.proc.kill()
        self.assertEqual(self.repl.proc.wait(timeout=10), -9)
        # Post-mortem copying is bounded, so the never-idle writer cannot pin the tee.
        _, eof = read_host_stderr(self.host_fd, time.monotonic() + 10.0, until_eof=True)
        self.assertTrue(eof)


class DrainTokenFilterTest(unittest.TestCase):
    def token(self, hex_body: str = "0123456789abcdef0123456789abcdef") -> bytes:
        return b"\xff<drain:" + hex_body.encode() + b">\xff"

    def test_tokens_are_removed_and_other_bytes_kept(self):
        token = self.token()
        data = b"before" + token + b"middle" + token + b"after"
        self.assertEqual(repl_module._strip_drain_tokens(data), (b"beforemiddleafter", b""))
        self.assertEqual(repl_module._strip_drain_tokens(token), (b"", b""))
        self.assertEqual(repl_module._strip_drain_tokens(b"plain text"), (b"plain text", b""))

    def test_lookalikes_survive(self):
        for data in (
            b"\xff",
            b"a\xffb",
            b"\xff<drain:ZZZZ",
            b"\xff<drain:" + b"0" * 32 + b">!",
            b"\xff<drain:" + b"0" * 31 + b"g>\xff",
            b"\xff<Drain:" + b"0" * 32 + b">\xff",
        ):
            clean, held = repl_module._strip_drain_tokens(data + b"x")
            self.assertEqual((clean, held), (data + b"x", b""), data)

    def test_token_split_across_reads_is_held_then_dropped(self):
        token = self.token()
        for cut in (1, 8, 9, 20, 40, 41):
            with self.subTest(cut=cut):
                clean, held = repl_module._strip_drain_tokens(b"head" + token[:cut])
                self.assertEqual(clean, b"head")
                self.assertEqual(held, token[:cut])
                clean, held = repl_module._strip_drain_tokens(held + token[cut:] + b"tail")
                self.assertEqual((clean, held), (b"tail", b""))

    def test_held_prefix_that_turns_out_not_to_be_a_token_is_released(self):
        clean, held = repl_module._strip_drain_tokens(b"x\xff<drain:0123")
        self.assertEqual((clean, held), (b"x", b"\xff<drain:0123"))
        clean, held = repl_module._strip_drain_tokens(held + b"zz\xff")
        self.assertEqual((clean, held), (b"\xff<drain:0123zz", b"\xff"))


class InProcessPumpTeeTest(unittest.TestCase):
    """The Windows / fork-failure fallback: the pump thread itself tees."""

    def test_pump_tees_raw_bytes_without_drain_tokens(self):
        src_r, src_w = os.pipe()
        host_r, host_w = os.pipe()
        for fd in (src_r, src_w, host_r, host_w):
            self.addCleanup(lambda fd=fd: os.close(fd) if fd in open_fds else None)
        open_fds = {src_r, src_w, host_r, host_w}
        pump = repl_module._Pump(src_r, src_w, "stderr", tee_fd=host_w)
        os.write(src_w, b"first\xff")
        pump.drain()
        os.write(src_w, b"second\n")
        pump.drain()
        os.close(src_w)
        os.close(pump._token_fd)
        open_fds.discard(src_w)
        pump._thread.join(timeout=5)
        self.assertFalse(pump._thread.is_alive())
        os.close(host_w)
        open_fds.discard(host_w)
        received = b""
        while True:
            chunk = os.read(host_r, 65536)
            if not chunk:
                break
            received += chunk
        self.assertEqual(received, b"first\xffsecond\n")


if __name__ == "__main__":
    unittest.main()
