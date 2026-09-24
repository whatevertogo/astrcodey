"""Service declarations, wire compatibility and trusted handler context."""
import unittest
from s5r import Worker, ServiceKey, DependencyKind, S5rError
from test_worker import FakeHost, EXT_ID


class ServiceTest(unittest.IsolatedAsyncioTestCase):
    async def test_service_registration_negotiation_and_invocation(self):
        worker = Worker(EXT_ID, "1")
        self.assertNotIn("services", worker._manifest_json())
        key = ServiceKey.parse("memory.entries.list@1")
        for invalid in ("x@0", "x@01", "x..y@1", "x@4294967296"):
            with self.assertRaises(ValueError):
                ServiceKey.parse(invalid)
        worker.dependency(ServiceKey.parse("other@1"), DependencyKind.OPTIONAL)

        @worker.service(key)
        async def echo(input, context):
            return {"caller": context.caller_extension_id, "workspace": context.working_dir, "input": input}

        with self.assertRaises(ValueError):
            worker.dependency(key, DependencyKind.REQUIRED)
        host = FakeHost(worker)
        init = await host.initialize()
        self.assertTrue(init.is_success)
        self.assertEqual(init.output["required_features"], ["extension_services_v1"])
        self.assertEqual(init.output["manifest"]["services"], [str(key)])
        self.assertTrue((await host.activate()).is_success)
        business_input = {"caller_extension_id": "forged", "session_id": 42, "working_dir": {}, "turn_id": [], "tool_call_id": False}
        await host.invoke_handler("service-1", f"{EXT_ID}:service:{key}", {"caller_extension_id": "client", "working_dir": "/trusted", "session_id": None, "input": business_input})
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output["data"]["input"], business_input)
        self.assertEqual(result.output["data"]["caller"], "client")
        self.assertEqual(result.output["data"]["workspace"], "/trusted")
        await host.shutdown()

        old_worker = Worker(EXT_ID, "1")
        old_worker.dependency(key, DependencyKind.OPTIONAL)
        old_host = FakeHost(old_worker)
        rejected = await old_host.initialize(supported=["nested_invoke_v1"], required=["nested_invoke_v1"])
        self.assertFalse(rejected.is_success)
        self.assertIn("extension_services_v1", rejected.error.message)
        with self.assertRaises(S5rError):
            await old_host.worker_task
