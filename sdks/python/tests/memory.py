"""In-memory full-duplex FrameTransport for tests."""

from __future__ import annotations

import asyncio


class MemoryTransport:
    def __init__(self) -> None:
        self._inbox: asyncio.Queue[bytes | None] = asyncio.Queue()
        self._peer: MemoryTransport | None = None

    @classmethod
    def pair(cls) -> tuple[MemoryTransport, MemoryTransport]:
        left, right = cls(), cls()
        left._peer = right
        right._peer = left
        return left, right

    async def read_frame(self) -> bytes:
        item = await self._inbox.get()
        if item is None:
            raise EOFError
        return item

    async def write_frame(self, payload: bytes) -> None:
        assert self._peer is not None
        self._peer._inbox.put_nowait(payload)

    def close_write(self) -> None:
        """Signal EOF to the peer's read side (like closing a pipe)."""
        assert self._peer is not None
        self._peer._inbox.put_nowait(None)

    async def aclose(self) -> None:
        if self._peer is not None:
            self._peer._inbox.put_nowait(None)
