"""S5R length-prefixed framing: `"<decimal length>\\n<JSON>"`.

Mirrors `astrcode_extension_sdk::wire::frame`: the length is the byte count of
the JSON body, frames are capped at 16 MiB, headers at 32 bytes, and empty /
signed / space-padded / oversized headers are rejected.
"""

from __future__ import annotations

import asyncio
import sys
from typing import Protocol

from .errors import FrameError

MAX_FRAME_BYTES = 16 * 1024 * 1024
MAX_FRAME_HEADER_BYTES = 32


def encode_frame(payload: bytes) -> bytes:
    if len(payload) > MAX_FRAME_BYTES:
        raise FrameError(
            f"frame size {len(payload)} exceeds max {MAX_FRAME_BYTES}"
        )
    return str(len(payload)).encode("ascii") + b"\n" + payload


def parse_frame_header(header: bytes) -> int:
    """Parse a header without its trailing newline."""
    if not header:
        raise FrameError("empty frame header")
    if len(header) > MAX_FRAME_HEADER_BYTES:
        raise FrameError(f"frame header exceeds {MAX_FRAME_HEADER_BYTES} bytes")
    if not all(0x30 <= byte <= 0x39 for byte in header):
        raise FrameError("frame header must contain decimal digits only")
    size = int(header)
    if size > MAX_FRAME_BYTES:
        raise FrameError(f"frame size {size} exceeds max {MAX_FRAME_BYTES}")
    return size


class FrameTransport(Protocol):
    """Byte-frame transport seam (stdio in production, memory in tests).

    `read_frame` raises `EOFError` on a clean end of stream — including EOF in
    the middle of a frame, mirroring the Rust worker, which treats
    `UnexpectedEof` as a clean shutdown.
    """

    async def read_frame(self) -> bytes: ...

    async def write_frame(self, payload: bytes) -> None: ...

    async def aclose(self) -> None: ...


class StdioTransport:
    """Worker-side transport over stdin/stdout.

    stdout carries protocol frames only; log to stderr.
    """

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        write_transport: asyncio.WriteTransport,
    ):
        self._reader = reader
        self._writer = writer
        self._write_transport = write_transport

    @classmethod
    async def connect(cls) -> StdioTransport:
        loop = asyncio.get_running_loop()
        reader = asyncio.StreamReader()
        await loop.connect_read_pipe(
            lambda: asyncio.StreamReaderProtocol(reader), sys.stdin
        )
        write_transport, protocol = await loop.connect_write_pipe(
            asyncio.streams.FlowControlMixin, sys.stdout
        )
        writer = asyncio.StreamWriter(write_transport, protocol, reader, loop)
        return cls(reader, writer, write_transport)

    async def read_frame(self) -> bytes:
        header = bytearray()
        while True:
            byte = await self._reader.read(1)
            if not byte:
                raise EOFError
            if byte == b"\n":
                break
            if len(header) == MAX_FRAME_HEADER_BYTES:
                raise FrameError(
                    f"frame header exceeds {MAX_FRAME_HEADER_BYTES} bytes"
                )
            header += byte
        size = parse_frame_header(bytes(header))
        try:
            return await self._reader.readexactly(size)
        except asyncio.IncompleteReadError:
            raise EOFError from None

    async def write_frame(self, payload: bytes) -> None:
        self._writer.write(encode_frame(payload))
        await self._writer.drain()

    async def aclose(self) -> None:
        self._write_transport.close()
