"""The receive-only WebSocket client behind ``rlm.monitor(url=...)``."""

from __future__ import annotations

import asyncio
import struct
import unittest

import ws_server
from rlm import _websocket
from rlm._websocket import Closed, WebSocketError


class WebSocketClientTest(unittest.IsolatedAsyncioTestCase):
    def serve(self, script, **kwargs) -> ws_server.ScriptedServer:
        server = ws_server.ScriptedServer(script, **kwargs)
        self.addCleanup(server.stop)
        return server

    async def connect(self, server: ws_server.ScriptedServer, path: str = "/") -> _websocket.WebSocket:
        socket = await _websocket.connect(f"ws://127.0.0.1:{server.port}{path}", timeout=5)
        self.addAsyncCleanup(socket.close)
        return socket

    async def test_messages_arrive_whole_and_the_server_is_answered(self):
        replies: list[tuple[int, bool, bytes]] = []

        def script(connection):
            connection.sendall(ws_server.frame(ws_server.TEXT, b"alpha"))
            connection.sendall(ws_server.frame(ws_server.BINARY, b"\x00\x01"))
            # One message in three frames, a ping between them.
            connection.sendall(ws_server.frame(ws_server.TEXT, b"hel", fin=False))
            connection.sendall(ws_server.frame(ws_server.PING, b"are-you-there"))
            connection.sendall(ws_server.frame(0x0, b"l", fin=False))
            connection.sendall(ws_server.frame(0x0, b"o"))
            replies.append(ws_server.read_frame(connection))
            connection.sendall(ws_server.close_frame(1001, "going away"))
            replies.append(ws_server.read_frame(connection))

        server = self.serve(script)
        socket = await self.connect(server, "/events?stream=1")
        received = [await socket.receive() for _ in range(4)]
        self.assertEqual(received, ["alpha", b"\x00\x01", "hello", Closed(1001, "going away")])
        await asyncio.sleep(0)
        server.stop()
        # The pong echoes the ping's payload and the close echoes the code; a client frame is masked.
        self.assertEqual(
            replies,
            [(ws_server.PONG, True, b"are-you-there"), (ws_server.CLOSE, True, struct.pack(">H", 1001))],
        )
        lines = server.head.split("\r\n")
        self.assertEqual(lines[0], "GET /events?stream=1 HTTP/1.1")
        self.assertIn(f"Host: 127.0.0.1:{server.port}", lines)
        self.assertIn("Sec-WebSocket-Version: 13", lines)

    async def test_a_lost_connection_is_a_close_without_a_code(self):
        server = self.serve(lambda connection: connection.sendall(ws_server.frame(ws_server.TEXT, b"last")))
        socket = await self.connect(server)
        self.assertEqual(await socket.receive(), "last")
        self.assertEqual(await socket.receive(), Closed(None, "connection lost"))

    async def test_a_wrong_accept_key_is_refused(self):
        server = self.serve(lambda connection: None, accept="not-the-key")
        with self.assertRaises(WebSocketError):
            await _websocket.connect(f"ws://127.0.0.1:{server.port}/", timeout=5)

    async def test_the_server_breaking_the_protocol_ends_the_stream_with_an_error(self):
        cases = {
            "masked": ws_server.frame(ws_server.TEXT, b"x", mask=True),
            "reserved bits": bytes([0xC1, 0x01]) + b"x",
            "too large": ws_server.frame(ws_server.TEXT, b"", length=_websocket.MAX_MESSAGE_BYTES + 1),
            "fragmented control": ws_server.frame(ws_server.PING, b"", fin=False),
            "orphan continuation": ws_server.frame(0x0, b"x"),
        }
        for name, raw in cases.items():
            with self.subTest(name):
                server = self.serve(lambda connection, raw=raw: connection.sendall(raw))
                socket = await self.connect(server)
                with self.assertRaises(WebSocketError):
                    await socket.receive()

    async def test_only_websocket_addresses_connect(self):
        for url in ("http://127.0.0.1/", "127.0.0.1:80", "ws:///nohost"):
            with self.subTest(url):
                with self.assertRaises(ValueError):
                    await _websocket.connect(url, timeout=1)


if __name__ == "__main__":
    unittest.main()
