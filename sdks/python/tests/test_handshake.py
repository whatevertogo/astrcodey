"""Handshake and feature-negotiation tests over a memory transport."""

from __future__ import annotations

import unittest

from harness import FakeHostBase
from s5r.errors import ProtocolError, S5rError, WireErrorCode
from s5r.protocol import (
    FEATURE_CUSTOM_EVENT_V1,
    FEATURE_MODEL_STREAM_V1,
    FEATURE_NESTED_INVOKE_V1,
)
from s5r.worker import Worker

ALL_FEATURES = [FEATURE_NESTED_INVOKE_V1, FEATURE_MODEL_STREAM_V1, FEATURE_CUSTOM_EVENT_V1]
EXT_ID = "test-extension"


class Handshake(FakeHostBase):
    """Minimal fake host driver used by the worker tests."""

    supported_features = ALL_FEATURES
    required_features = ALL_FEATURES


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
