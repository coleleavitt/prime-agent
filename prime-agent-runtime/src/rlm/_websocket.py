"""A receive-only WebSocket client (RFC 6455) for ``rlm.monitor(url=...)``.

A text message is a line of events, a binary message one placeholder line. The
server's pings are answered and its close ends the stream. The client sends
nothing else, so this is the whole protocol a monitor needs: no extensions,
no subprotocols, no data frames out.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import hashlib
import os
import ssl
import struct
from dataclasses import dataclass
from urllib.parse import urlsplit

_ACCEPT_GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_MESSAGE_BYTES = 1 << 20
_MAX_HEAD_BYTES = 16 * 1024

_CONTINUATION, _TEXT, _BINARY, _CLOSE, _PING, _PONG = 0x0, 0x1, 0x2, 0x8, 0x9, 0xA
_NORMAL_CLOSURE = 1000


class WebSocketError(RuntimeError):
    """The connection failed or the server broke the protocol."""


@dataclass(frozen=True)
class Closed:
    """The server's close frame: its status code and reason (no code: it sent none)."""

    code: int | None
    reason: str


def _control_frame(opcode: int, payload: bytes = b"") -> bytes:
    """A client-to-server control frame: masked, as the protocol requires."""
    mask = os.urandom(4)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return bytes([0x80 | opcode, 0x80 | len(payload)]) + mask + masked


class WebSocket:
    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        self._reader = reader
        self._writer = writer

    async def receive(self) -> str | bytes | Closed:
        """The next message (text, or binary bytes), or the server's close."""
        kind: int | None = None
        parts: list[bytes] = []
        size = 0
        try:
            while True:
                first, second = await self._reader.readexactly(2)
                final, opcode = bool(first & 0x80), first & 0x0F
                if first & 0x70:
                    raise WebSocketError("the server set reserved bits no extension was negotiated for")
                if second & 0x80:
                    raise WebSocketError("the server sent a masked frame")
                length = second & 0x7F
                if length == 126:
                    (length,) = struct.unpack(">H", await self._reader.readexactly(2))
                elif length == 127:
                    (length,) = struct.unpack(">Q", await self._reader.readexactly(8))
                if opcode >= _CLOSE:
                    if length > 125 or not final:
                        raise WebSocketError("the server sent a malformed control frame")
                elif size + length > MAX_MESSAGE_BYTES:
                    raise WebSocketError(f"a message passed {MAX_MESSAGE_BYTES} bytes")
                payload = await self._reader.readexactly(length)
                if opcode == _PING:
                    self._writer.write(_control_frame(_PONG, payload))
                    await self._writer.drain()
                elif opcode == _CLOSE:
                    code = struct.unpack(">H", payload[:2])[0] if len(payload) >= 2 else None
                    with contextlib.suppress(OSError):
                        self._writer.write(_control_frame(_CLOSE, payload[:2]))
                        await self._writer.drain()
                    return Closed(code, payload[2:].decode("utf-8", errors="replace"))
                elif opcode == _PONG:
                    continue
                else:
                    if opcode == _CONTINUATION:
                        if kind is None:
                            raise WebSocketError("a continuation frame came with no message to continue")
                    elif opcode in (_TEXT, _BINARY) and kind is None:
                        kind = opcode
                    else:
                        raise WebSocketError(f"unexpected frame type {opcode}")
                    parts.append(payload)
                    size += length
                    if final:
                        message = b"".join(parts)
                        return message.decode("utf-8", errors="replace") if kind == _TEXT else message
        except (asyncio.IncompleteReadError, ConnectionError):
            return Closed(None, "connection lost")

    async def close(self) -> None:
        with contextlib.suppress(OSError):
            self._writer.write(_control_frame(_CLOSE, struct.pack(">H", _NORMAL_CLOSURE)))
            await self._writer.drain()
        self._writer.close()
        with contextlib.suppress(OSError):
            await self._writer.wait_closed()


async def connect(url: str, *, timeout: float) -> WebSocket:
    """Open ``url`` (``ws://`` or ``wss://``) and finish the opening handshake."""
    parts = urlsplit(url)
    if parts.scheme not in ("ws", "wss") or not parts.hostname:
        raise ValueError("url must be a ws:// or wss:// address")
    secure = parts.scheme == "wss"
    port = parts.port or (443 if secure else 80)
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_connection(
                parts.hostname,
                port,
                ssl=ssl.create_default_context() if secure else None,
                server_hostname=parts.hostname if secure else None,
                limit=_MAX_HEAD_BYTES,
            ),
            timeout,
        )
    except (OSError, ssl.SSLError, asyncio.TimeoutError) as error:
        raise WebSocketError(f"could not connect to {parts.hostname}:{port}: {error or 'timed out'}") from error
    key = base64.b64encode(os.urandom(16)).decode()
    host = f"[{parts.hostname}]" if ":" in parts.hostname else parts.hostname
    if port != (443 if secure else 80):
        host += f":{port}"
    target = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
    writer.write(
        (
            f"GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    try:
        await writer.drain()
        head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), timeout)
    except (OSError, asyncio.IncompleteReadError, asyncio.LimitOverrunError, asyncio.TimeoutError) as error:
        writer.close()
        raise WebSocketError(f"the opening handshake failed: {error or 'timed out'}") from error
    status, *lines = head.decode("latin-1").split("\r\n")
    headers = {name.strip().lower(): value.strip() for name, _, value in (line.partition(":") for line in lines)}
    expected = base64.b64encode(hashlib.sha1(key.encode() + _ACCEPT_GUID).digest()).decode()
    if status.split(" ")[1:2] != ["101"] or headers.get("sec-websocket-accept") != expected:
        writer.close()
        raise WebSocketError(f"the server did not accept the upgrade ({status.strip()})")
    return WebSocket(reader, writer)
