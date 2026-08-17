"""Shared fake-host driver for worker tests (transport pair, send/recv, handshake)."""

from __future__ import annotations

import asyncio

from memory import MemoryTransport
from s5r.protocol import decode_message, encode_message
from s5r.worker import Worker


class FakeHostBase:
    """Minimal fake host driver shared by the worker tests.

    Subclasses configure the initialize message via the `supported_features`
    and `required_features` class attributes and may add host-side helpers.
    """

    extension_id: str = "test-extension"
    supported_features: list[str]
    required_features: list[str]

    def __init__(self, worker: Worker, host_operations: list[str] | None = None):
        self.transport, worker_transport = MemoryTransport.pair()
        self.worker = worker
        self.worker_task = asyncio.create_task(worker.serve(worker_transport))
        self.host_operations = [] if host_operations is None else host_operations

    async def send(self, message: dict) -> None:
        await self.transport.write_frame(encode_message(message))

    async def recv(self):
        return decode_message(
            await asyncio.wait_for(self.transport.read_frame(), timeout=5)
        )

    async def initialize(
        self,
        extension_id: str | None = None,
        *,
        protocol_version: str = "3.0",
        supported: list[str] | None = None,
        required: list[str] | None = None,
        host_operations: list[str] | None = None,
    ):
        await self.send(
            {
                "type": "initialize",
                "id": "init-1",
                "protocol_version": protocol_version,
                "host": {"name": "test-host"},
                "extension_id": self.extension_id if extension_id is None else extension_id,
                "supported_features": (
                    list(self.supported_features) if supported is None else supported
                ),
                "required_features": (
                    list(self.required_features) if required is None else required
                ),
                "host_operations": (
                    self.host_operations if host_operations is None else host_operations
                ),
            }
        )
        result = await self.recv()
        assert result.kind == "initialize", result
        return result

    async def activate(self):
        await self.send({"type": "activate", "id": "act-1", "config": None})
        result = await self.recv()
        assert result.kind == "activate", result
        return result

    async def handshake(self):
        init = await self.initialize()
        assert init.is_success, init.error
        activate = await self.activate()
        assert activate.is_success, activate.error
        return init

    async def shutdown(self) -> None:
        self.transport.close_write()
        await asyncio.wait_for(self.worker_task, timeout=5)
