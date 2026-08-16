"""Handshake and feature-negotiation tests over a memory transport."""

from __future__ import annotations

import asyncio
import unittest

from memory import MemoryTransport
from s5r.errors import ProtocolError, S5rError, WireErrorCode
from s5r.protocol import (
    FEATURE_CUSTOM_EVENT_V1,
    FEATURE_MODEL_STREAM_V1,
    FEATURE_NESTED_INVOKE_V1,
    decode_message,
    encode_message,
)
from s5r.worker import Worker

ALL_FEATURES = [FEATURE_NESTED_INVOKE_V1, FEATURE_MODEL_STREAM_V1, FEATURE_CUSTOM_EVENT_V1]
EXT_ID = "test-extension"


def initialize_msg(
    extension_id: str = EXT_ID,
    *,
    protocol_version: str = "3.0",
    supported: list[str] | None = None,
    required: list[str] | None = None,
    host_operations: list[str] | None = None,
) -> dict:
    return {
        "type": "initialize",
        "id": "init-1",
        "protocol_version": protocol_version,
        "host": {"name": "test-host"},
        "extension_id": extension_id,
        "supported_features": ALL_FEATURES if supported is None else supported,
        "required_features": ALL_FEATURES if required is None else required,
        "host_operations": [] if host_operations is None else host_operations,
    }


class Handshake:
    """Minimal fake host driver used by the worker tests."""

    def __init__(self, worker: Worker, host_operations: list[str] | None = None):
        self.host_transport, worker_transport = MemoryTransport.pair()
        self.worker = worker
        self.worker_task = asyncio.create_task(worker.serve(worker_transport))
        self.host_operations = [] if host_operations is None else host_operations

    async def send(self, message: dict) -> None:
        await self.host_transport.write_frame(encode_message(message))

    async def recv(self):
        return decode_message(
            await asyncio.wait_for(self.host_transport.read_frame(), timeout=5)
        )

    async def initialize(self, **kwargs):
        await self.send(initialize_msg(host_operations=self.host_operations, **kwargs))
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
        self.host_transport.close_write()
        await asyncio.wait_for(self.worker_task, timeout=5)


class HandshakeTest(unittest.IsolatedAsyncioTestCase):
    async def test_initialize_activate_and_clean_eof(self) -> None:
        host = Handshake(Worker(EXT_ID, "1.2.3"))
        init = await host.initialize()
        self.assertTrue(init.is_success)
        output = init.output
        self.assertEqual(output["protocol_version"], "3.0")
        self.assertEqual(
            output["worker"], {"name": EXT_ID, "version": "1.2.3"}
        )
        self.assertEqual(sorted(output["negotiated_features"]), sorted(ALL_FEATURES))
        self.assertEqual(sorted(output["supported_features"]), sorted(ALL_FEATURES))
        self.assertEqual(output["required_features"], [])
        manifest = output["manifest"]
        self.assertEqual(manifest["required_transport_features"], [])
        self.assertEqual(manifest["tools"], [])
        activate = await host.activate()
        self.assertTrue(activate.is_success)
        self.assertEqual(activate.output, {})
        await host.shutdown()

    async def test_unsupported_protocol_version_fails(self) -> None:
        host = Handshake(Worker(EXT_ID, "0.1.0"))
        result = await host.initialize(protocol_version="2.0")
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.UNSUPPORTED_PROTOCOL_VERSION)
        with self.assertRaises(S5rError):
            await host.worker_task

    async def test_extension_id_mismatch_fails(self) -> None:
        host = Handshake(Worker(EXT_ID, "0.1.0"))
        result = await host.initialize(extension_id="other-extension")
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.INVALID_REQUEST)
        with self.assertRaises(S5rError):
            await host.worker_task

    async def test_unsupported_required_feature_fails(self) -> None:
        host = Handshake(Worker(EXT_ID, "0.1.0"))
        result = await host.initialize(
            supported=["future_feature_v1"], required=["future_feature_v1"]
        )
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.UNSUPPORTED_FEATURE)
        with self.assertRaises(S5rError):
            await host.worker_task

    async def test_business_message_before_activate_is_rejected(self) -> None:
        host = Handshake(Worker(EXT_ID, "0.1.0"))
        await host.initialize()
        await host.send(
            {"type": "invoke", "id": "x-1", "operation": "s5r.runtime.ping"}
        )
        with self.assertRaises(ProtocolError):
            await host.worker_task

    async def test_eof_during_runtime_is_clean(self) -> None:
        host = Handshake(Worker(EXT_ID, "0.1.0"))
        await host.handshake()
        await host.shutdown()


if __name__ == "__main__":
    unittest.main()
