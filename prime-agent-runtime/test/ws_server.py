"""A one-connection WebSocket server scripted frame by frame, for the monitor tests.

The handshake is the server half of RFC 6455; after it, ``script(connection)``
decides what to send and read. ``frame`` builds server-to-client frames
(unmasked, as the protocol requires; ``mask=True`` builds the illegal masked
kind to test the client's refusal).
"""

from __future__ import annotations

import base64
import hashlib
import socket
import struct
import threading
from collections.abc import Callable

GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
TEXT, BINARY, CLOSE, PING, PONG = 0x1, 0x2, 0x8, 0x9, 0xA


def frame(opcode: int, payload: bytes = b"", *, fin: bool = True, mask: bool = False, length: int | None = None) -> bytes:
    size = len(payload) if length is None else length
    header = bytes([(0x80 if fin else 0) | opcode])
    flag = 0x80 if mask else 0
    if size < 126:
        header += bytes([flag | size])
    elif size < 1 << 16:
        header += bytes([flag | 126]) + struct.pack(">H", size)
    else:
        header += bytes([flag | 127]) + struct.pack(">Q", size)
    return header + (b"\0\0\0\0" if mask else b"") + payload


def close_frame(code: int, reason: str = "") -> bytes:
    return frame(CLOSE, struct.pack(">H", code) + reason.encode())


def read_frame(connection: socket.socket) -> tuple[int, bool, bytes]:
    """One client frame: its opcode, whether it was masked, its unmasked payload."""

    def exactly(count: int) -> bytes:
        data = b""
        while len(data) < count:
            chunk = connection.recv(count - len(data))
            if not chunk:
                raise ConnectionError("the client went away")
            data += chunk
        return data

    first, second = exactly(2)
    masked, length = bool(second & 0x80), second & 0x7F
    mask = exactly(4) if masked else b""
    payload = exactly(length)
    if masked:
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return first & 0x0F, masked, payload


class ScriptedServer:
    def __init__(self, script: Callable[[socket.socket], None], *, accept: str | None = None) -> None:
        self._script = script
        self._accept = accept
        self._listener = socket.socket()
        self._listener.bind(("127.0.0.1", 0))
        self._listener.listen(1)
        self.port: int = self._listener.getsockname()[1]
        self.head = ""
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def _serve(self) -> None:
        connection, _ = self._listener.accept()
        with connection:
            head = b""
            while not head.endswith(b"\r\n\r\n"):
                chunk = connection.recv(1)
                if not chunk:
                    return
                head += chunk
            self.head = head.decode("latin-1")
            key = next(line.split(":", 1)[1].strip() for line in self.head.split("\r\n") if line.lower().startswith("sec-websocket-key"))
            accept = self._accept or base64.b64encode(hashlib.sha1(key.encode() + GUID).digest()).decode()
            connection.sendall(
                (
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                    f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
                ).encode()
            )
            try:
                self._script(connection)
            except (ConnectionError, OSError):
                pass

    def stop(self) -> None:
        self._listener.close()
        self._thread.join(timeout=5)
