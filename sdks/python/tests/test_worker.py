"""Tool/hook/command round-trip, host-API, cancel, and conformance tests."""

from __future__ import annotations

import asyncio
import unittest
from dataclasses import dataclass
from unittest import mock

from harness import FakeHostBase
from memory import MemoryTransport
from test_handshake import ALL_FEATURES
from s5r import (
    HandlerResult,
    HostClient,
    HostOperation,
    ExtensionHttpRoute,
    ProtocolError,
    ResourceAccess,
    SlashCommand,
    S5rError,
    ToolDefinition,
    ToolPlan,
    WireErrorCode,
    Worker,
    parse_tool_arguments,
    tool_text,
)
from s5r import worker as worker_module
from s5r.context import CancelToken
from s5r.protocol import (
    CAP_HANDLER_INVOKE,
    CAP_RUNTIME_PING,
    CONFORMANCE_HOST_ECHO,
    CONFORMANCE_NESTED,
    CONFORMANCE_STREAM,
    CONFORMANCE_UNKNOWN_ERROR,
    CONFORMANCE_UNARY,
    CONFORMANCE_WAIT_FOR_CANCEL,
    FEATURE_MODEL_STREAM_V1,
    StreamMsg,
    decode_message,
    encode_message,
)
from s5r.results import FileOperation, HostResource
from s5r.worker import (
    _MAX_IN_FLIGHT_REQUESTS,
    _STREAM_BUFFER_CAPACITY,
    _STREAM_FORWARD_BUFFER_CAPACITY,
    _WRITE_QUEUE_CAPACITY,
    _Driver,
    _StreamPending,
)

EXT_ID = "test-extension"
SCOPE = {"session_id": "session-1", "working_dir": "/workspace"}


class FakeHost(FakeHostBase):
    supported_features = ALL_FEATURES
    required_features = ["nested_invoke_v1"]

    async def invoke(self, request_id: str, operation: str, input, stream: bool = False):
        await self.send(
            {
                "type": "invoke",
                "id": request_id,
                "operation": operation,
                "input": input,
                "stream": stream,
            }
        )

    async def invoke_handler(self, request_id: str, handler_id: str, event: dict):
        await self.invoke(
            request_id,
            CAP_HANDLER_INVOKE,
            {"handler_id": handler_id, "event": event},
        )


def tool_event(arguments, phase: str = "execute", **scope_extra) -> dict:
    return {
        "phase": phase,
        "arguments": arguments,
        "scope": {**SCOPE, **scope_extra},
    }


class ToolRoundTripTest(unittest.IsolatedAsyncioTestCase):
    def test_tool_timeout_is_optional_manifest_policy(self) -> None:
        plain = ToolDefinition(name="plain", description="", parameters={})
        bounded = ToolDefinition(
            name="bounded", description="", parameters={}, timeout_ms=5_000
        )

        self.assertNotIn("timeout_ms", plain.to_manifest())
        self.assertEqual(bounded.to_manifest()["timeout_ms"], 5_000)

    async def test_tool_execute_and_plan(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def planner(arguments, ctx):
            self.assertEqual(ctx.session_id, "session-1")
            return ToolPlan(
                [
                    ResourceAccess.file(FileOperation.READ, "/workspace/probe.txt"),
                    ResourceAccess.host(HostResource.MODEL),
                ]
            )

        @worker.tool(
            ToolDefinition(
                name="echo",
                description="Echo text",
                parameters={"type": "object"},
            ),
            planner=planner,
        )
        async def echo(arguments, ctx):
            self.assertEqual(ctx.extension_id, EXT_ID)
            self.assertEqual(ctx.working_dir, "/workspace")
            return tool_text(f"echo:{arguments['text']}")

        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler(
            "r-1", f"{EXT_ID}:tool:echo", tool_event({"text": "hi"})
        )
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(
            result.output,
            {
                "effect": "tool_outcome",
                "data": {"content": "echo:hi", "is_error": False},
            },
        )
        await host.invoke_handler(
            "r-2", f"{EXT_ID}:tool:echo", tool_event({"text": "hi"}, phase="plan")
        )
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output["effect"], "tool_plan")
        self.assertEqual(
            result.output["data"]["resources"],
            [
                {
                    "kind": "file",
                    "operation": "read",
                    "path": "/workspace/probe.txt",
                    "recursive": False,
                },
                {"kind": "host", "resource": "model"},
            ],
        )
        await host.shutdown()

    async def test_unknown_handler_and_bad_phase_fail(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:ghost", tool_event({}))
        result = await host.recv()
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.UNKNOWN_HANDLER)
        await host.invoke_handler("r-2", "other-extension:tool:echo", tool_event({}))
        result = await host.recv()
        self.assertEqual(result.error.code, WireErrorCode.UNKNOWN_HANDLER)
        await host.invoke_handler("r-3", f"{EXT_ID}:hook:ghost", {})
        result = await host.recv()
        self.assertEqual(result.error.code, WireErrorCode.UNKNOWN_HANDLER)
        await host.shutdown()

    async def test_handler_error_becomes_failure_result(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="fail", description="", parameters={}))
        async def fail(arguments, ctx):
            raise S5rError.of(WireErrorCode.INVALID_INPUT, "bad arguments")

        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:fail", tool_event({}))
        result = await host.recv()
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.INVALID_INPUT)
        self.assertEqual(result.error.message, "bad arguments")
        await host.shutdown()

    async def test_hook_and_command_round_trip(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.on_pre_tool_use
        async def pre_tool_use(event, ctx):
            self.assertEqual(ctx.tool_call_id, "call-1")
            return HandlerResult.ok()

        @worker.command(SlashCommand(name="inspect", description="Inspect"))
        async def inspect_command(ctx):
            self.assertEqual(ctx.argument, "--verbose")
            self.assertEqual(ctx.invocation.kind, "execute")
            return HandlerResult.of("ok", {"done": True})

        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler(
            "r-1",
            f"{EXT_ID}:hook:pre_tool_use",
            {"input": {**SCOPE, "tool_call_id": "call-1"}},
        )
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output, {"effect": "ok"})

        await host.invoke_handler(
            "r-2",
            f"{EXT_ID}:command:inspect",
            {
                "on": "command",
                "input": {
                    **SCOPE,
                    "command_name": "inspect",
                    "argument": "--verbose",
                    "model": {"profile_name": "p", "model": "m", "provider_kind": "k"},
                },
            },
        )
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output["data"], {"done": True})
        await host.shutdown()

    async def test_hook_missing_scope_facts_fails(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.on_pre_tool_use
        async def pre_tool_use(event, ctx):
            return HandlerResult.ok()

        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:hook:pre_tool_use", {"input": {}})
        result = await host.recv()
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.CONTEXT_UNAVAILABLE)
        await host.shutdown()


class HostApiTest(unittest.IsolatedAsyncioTestCase):
    async def test_host_client_round_trip_with_parent(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="lookup", description="", parameters={}))
        async def lookup(arguments, ctx):
            self.assertTrue(HostClient.host_supports(HostOperation.SESSION_STATE_READ))
            state = await HostClient.session_state().read({"key": "goal"})
            return tool_text(f"goal:{state['content']}")

        host = FakeHost(worker, host_operations=[HostOperation.SESSION_STATE_READ])
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:lookup", tool_event({}))
        nested = await host.recv()
        self.assertEqual(nested.operation, HostOperation.SESSION_STATE_READ)
        self.assertEqual(nested.parent_invoke_id, "r-1")
        self.assertEqual(nested.input, {"key": "goal"})
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": nested.id,
                "kind": "invoke",
                "output": {"content": "active"},
            }
        )
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output["data"]["content"], "goal:active")
        await host.shutdown()

    async def test_unsupported_operation_fails_locally(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="lookup", description="", parameters={}))
        async def lookup(arguments, ctx):
            await HostClient.session_state().read({"key": "goal"})
            return HandlerResult.ok()

        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:lookup", tool_event({}))
        result = await host.recv()
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.UNSUPPORTED)
        await host.shutdown()

    async def test_host_client_outside_invocation_is_unavailable(self) -> None:
        with self.assertRaises(S5rError) as raised:
            HostClient.host_supports(HostOperation.SESSION_STATE_READ)
        self.assertEqual(raised.exception.code, WireErrorCode.CONTEXT_UNAVAILABLE)

    async def test_queue_or_start_and_defer_context(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="steer", description="", parameters={}))
        async def steer(arguments, ctx):
            queued = await HostClient.session_control().queue_or_start(
                {"target_session_id": ctx.session_id, "content": "later"}
            )
            deferred = await ctx.defer_context("remember this")
            return tool_text(f"{queued['status']}:{deferred['status']}")

        host = FakeHost(
            worker,
            host_operations=[
                HostOperation.SESSION_CONTROL_QUEUE_OR_START,
                HostOperation.SESSION_CONTROL_DEFER_CONTEXT,
            ],
        )
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:steer", tool_event({}))

        nested = await host.recv()
        self.assertEqual(nested.operation, HostOperation.SESSION_CONTROL_QUEUE_OR_START)
        self.assertEqual(nested.parent_invoke_id, "r-1")
        self.assertEqual(
            nested.input, {"target_session_id": "session-1", "content": "later"}
        )
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": nested.id,
                "kind": "invoke",
                "output": {"status": "queued", "queue_len": 1},
            }
        )

        nested = await host.recv()
        self.assertEqual(nested.operation, HostOperation.SESSION_CONTROL_DEFER_CONTEXT)
        self.assertEqual(
            nested.input,
            {"target_session_id": "session-1", "content": "remember this"},
        )
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": nested.id,
                "kind": "invoke",
                "output": {"status": "injected", "turn_id": "turn-1"},
            }
        )

        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output["data"]["content"], "queued:injected")
        await host.shutdown()

    async def test_no_active_turn_error_passes_through(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="steer", description="", parameters={}))
        async def steer(arguments, ctx):
            await ctx.defer_context("remember this")
            return HandlerResult.ok()

        host = FakeHost(
            worker, host_operations=[HostOperation.SESSION_CONTROL_DEFER_CONTEXT]
        )
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:steer", tool_event({}))
        nested = await host.recv()
        await host.send(
            {
                "type": "result",
                "status": "failure",
                "id": nested.id,
                "kind": "invoke",
                "error": {
                    "code": "no_active_turn",
                    "message": "session has no active turn",
                },
            }
        )
        result = await host.recv()
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.NO_ACTIVE_TURN)
        await host.shutdown()

    async def test_model_stream_round_trip(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="chat", description="", parameters={}))
        async def chat(arguments, ctx):
            output = await HostClient.models().main_chat_collected(
                {"messages": [{"role": "user", "content": "hi"}]}
            )
            return tool_text(output["content"])

        host = FakeHost(worker, host_operations=[HostOperation.LLM_MAIN_CHAT])
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:chat", tool_event({}))
        stream_request = await host.recv()
        self.assertEqual(stream_request.operation, HostOperation.LLM_MAIN_CHAT)
        self.assertTrue(stream_request.stream)
        for event in (
            {"type": "started"},
            {"type": "content_delta", "content": "he"},
            {"type": "content_delta", "content": "llo"},
            {"type": "completed", "output": {"content": "hello"}},
        ):
            await host.send({"type": "stream", "id": stream_request.id, "event": event})
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output["data"]["content"], "hello")
        await host.shutdown()


class CancelTest(unittest.IsolatedAsyncioTestCase):
    async def test_cancel_stops_handler_and_worker_stays_responsive(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="slow", description="", parameters={}))
        async def slow(arguments, ctx):
            await ctx.cancel_token.wait_cancelled()
            raise S5rError.of(WireErrorCode.CANCELLED, "slow tool cancelled")

        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:slow", tool_event({}))
        await asyncio.sleep(0.05)
        await host.send({"type": "cancel", "id": "r-1", "reason": "caller_dropped"})
        await host.invoke("r-2", CAP_RUNTIME_PING, None)
        result = await host.recv()
        # The cancelled invocation produces no result; the next frame is the ping.
        self.assertEqual(result.id, "r-2")
        self.assertEqual(result.output, {"ok": True})
        await host.shutdown()


class ConformanceOpsTest(unittest.IsolatedAsyncioTestCase):
    async def test_builtin_conformance_operations(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        host = FakeHost(worker, host_operations=[CONFORMANCE_HOST_ECHO])
        await host.handshake()

        await host.invoke("r-1", CAP_RUNTIME_PING, None)
        self.assertEqual((await host.recv()).output, {"ok": True})

        fixture = {"fixture": "echo"}
        await host.invoke("r-2", CONFORMANCE_UNARY, fixture)
        self.assertEqual((await host.recv()).output, fixture)

        await host.invoke("r-3", CONFORMANCE_STREAM, fixture, stream=True)
        events = []
        while True:
            message = await host.recv()
            events.append(message.event)
            if message.event["type"] in ("completed", "failed"):
                break
        self.assertEqual(
            events,
            [
                {"type": "started"},
                {"type": "content_delta", "content": "first"},
                {"type": "content_delta", "content": "second"},
                {"type": "completed", "output": fixture},
            ],
        )

        await host.invoke("r-4", CONFORMANCE_NESTED, fixture)
        nested = await host.recv()
        self.assertEqual(nested.operation, CONFORMANCE_HOST_ECHO)
        self.assertEqual(nested.parent_invoke_id, "r-4")
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": nested.id,
                "kind": "invoke",
                "output": fixture,
            }
        )
        self.assertEqual((await host.recv()).output, fixture)

        await host.invoke("r-5", CONFORMANCE_UNKNOWN_ERROR, None)
        result = await host.recv()
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, "future_conformance_error")

        await host.invoke("r-6", CONFORMANCE_WAIT_FOR_CANCEL, None)
        await asyncio.sleep(0.05)
        await host.send({"type": "cancel", "id": "r-6", "reason": "caller_dropped"})
        await host.invoke("r-7", CAP_RUNTIME_PING, None)
        self.assertEqual((await host.recv()).id, "r-7")
        await host.shutdown()


class DisposeRootTest(unittest.IsolatedAsyncioTestCase):
    async def test_dispose_root_ack_round_trip(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.tool(ToolDefinition(name="cleanup", description="", parameters={}))
        async def cleanup(arguments, ctx):
            await HostClient.session_control().dispose_root("root-1")
            return tool_text("disposed")

        host = FakeHost(worker, host_operations=[HostOperation.SESSION_ROOT_DISPOSE])
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:tool:cleanup", tool_event({}))
        nested = await host.recv()
        self.assertEqual(nested.operation, HostOperation.SESSION_ROOT_DISPOSE)
        self.assertEqual(nested.parent_invoke_id, "r-1")
        self.assertEqual(nested.input, {"target_session_id": "root-1"})
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": nested.id,
                "kind": "invoke",
                "output": {"ok": True},
            }
        )
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(result.output["data"]["content"], "disposed")
        await host.shutdown()

    async def test_dispose_root_rejects_bad_ack(self) -> None:
        for output in ({"ok": False}, {"ok": True, "unexpected": 1}, {}):
            with self.subTest(output=output):
                worker = Worker(EXT_ID, "0.1.0")

                @worker.tool(
                    ToolDefinition(name="cleanup", description="", parameters={})
                )
                async def cleanup(arguments, ctx):
                    await HostClient.session_control().dispose_root("root-1")
                    return tool_text("unreachable")

                host = FakeHost(
                    worker, host_operations=[HostOperation.SESSION_ROOT_DISPOSE]
                )
                await host.handshake()
                await host.invoke_handler(
                    "r-1", f"{EXT_ID}:tool:cleanup", tool_event({})
                )
                nested = await host.recv()
                await host.send(
                    {
                        "type": "result",
                        "status": "success",
                        "id": nested.id,
                        "kind": "invoke",
                        "output": output,
                    }
                )
                result = await host.recv()
                self.assertFalse(result.is_success)
                self.assertEqual(result.error.code, WireErrorCode.INVALID_RESPONSE)
                await host.shutdown()


class HttpRouteTest(unittest.IsolatedAsyncioTestCase):
    def test_route_validation(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def noop(request, ctx):
            return {"status": 200, "body": None}

        for route in (
            ExtensionHttpRoute.public("FETCH", "/health"),
            ExtensionHttpRoute(method="GET", path="/health", access="anonymous"),
            ExtensionHttpRoute.public("GET", "/files/../secret"),
            ExtensionHttpRoute.public("GET", "/trailing/"),
            ExtensionHttpRoute.public("GET", "/double//slash"),
            ExtensionHttpRoute.public("GET", "/{id}/{id}"),
            ExtensionHttpRoute.public("GET", "/bad-{param}"),
            ExtensionHttpRoute(method="POST", path="/body", max_body_bytes=0),
            ExtensionHttpRoute(method="POST", path="/body", max_body_bytes=1024 * 1024 + 1),
        ):
            with self.subTest(route=route):
                with self.assertRaises(S5rError) as raised:
                    worker.http_route(route, noop)
                self.assertEqual(raised.exception.code, WireErrorCode.INVALID_HTTP_ROUTE)

    def test_conflicting_route_registration(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def noop(request, ctx):
            return {"status": 200, "body": None}

        worker.http_route(ExtensionHttpRoute.public("GET", "/future-tasks/{id}"), noop)
        with self.assertRaises(S5rError) as raised:
            worker.http_route(ExtensionHttpRoute.public("GET", "/future-tasks/{jobId}"), noop)
        self.assertEqual(raised.exception.code, WireErrorCode.DUPLICATE_REGISTRATION)
        # Different access or method does not conflict.
        worker.http_route(ExtensionHttpRoute.authenticated("GET", "/future-tasks/{jobId}"), noop)
        worker.http_route(ExtensionHttpRoute.public("POST", "/future-tasks/{jobId}"), noop)
        # Same shape but non-overlapping patterns coexist.
        worker.http_route(ExtensionHttpRoute.public("GET", "/notes/{id}"), noop)
        self.assertEqual(len(worker._http_route_manifest), 4)

    async def test_manifest_and_dispatch_round_trip(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        @worker.http_route(ExtensionHttpRoute.public("GET", "/health"))
        async def health(request, ctx):
            self.assertEqual(ctx.extension_id, EXT_ID)
            self.assertEqual(
                request,
                {
                    "method": "GET",
                    "path": "/health",
                    "path_params": {},
                    "query": "verbose=1",
                    "body": None,
                },
            )
            return {"status": 200, "body": {"ok": True}}

        host = FakeHost(worker)
        init = await host.initialize()
        self.assertEqual(
            init.output["manifest"]["http_routes"],
            [
                {
                    "route": {
                        "method": "GET",
                        "path": "/health",
                        "access": "public",
                        "description": "",
                        "max_body_bytes": 64 * 1024,
                    },
                    "handler_id": f"{EXT_ID}:http:route_0",
                }
            ],
        )
        await host.activate()
        await host.invoke_handler(
            "r-1",
            f"{EXT_ID}:http:route_0",
            {"method": "GET", "path": "/health", "query": "verbose=1"},
        )
        result = await host.recv()
        self.assertTrue(result.is_success)
        self.assertEqual(
            result.output,
            {"effect": "http_response", "data": {"status": 200, "body": {"ok": True}}},
        )
        await host.shutdown()

    async def test_unknown_route_and_invalid_payload(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def health(request, ctx):
            return {"status": 200, "body": None}

        worker.http_route(ExtensionHttpRoute.public("GET", "/health"), health)
        host = FakeHost(worker)
        await host.handshake()
        await host.invoke_handler("r-1", f"{EXT_ID}:http:route_9", {})
        result = await host.recv()
        self.assertEqual(result.error.code, WireErrorCode.UNKNOWN_HANDLER)
        for event in (
            "not-an-object",
            {"method": "FETCH", "path": "/health"},
            {"method": "GET", "path": "/health", "headers": {}},
            {"method": "GET", "path": "/health", "path_params": {"id": 42}},
        ):
            await host.invoke_handler("r-2", f"{EXT_ID}:http:route_0", event)
            result = await host.recv()
            self.assertEqual(result.error.code, WireErrorCode.INVALID_INPUT)
        await host.shutdown()

    async def test_invalid_response_fails(self) -> None:
        for response in ({"status": 99, "body": None}, {"status": 200}, "oops"):
            with self.subTest(response=response):
                worker = Worker(EXT_ID, "0.1.0")

                async def broken(request, ctx):
                    return response

                worker.http_route(ExtensionHttpRoute.public("GET", "/health"), broken)
                host = FakeHost(worker)
                await host.handshake()
                await host.invoke_handler(
                    "r-1",
                    f"{EXT_ID}:http:route_0",
                    {"method": "GET", "path": "/health"},
                )
                result = await host.recv()
                self.assertFalse(result.is_success)
                self.assertEqual(
                    result.error.code, WireErrorCode.SERIALIZATION_FAILED
                )
                await host.shutdown()


class ShutdownHookTest(unittest.IsolatedAsyncioTestCase):
    async def test_hook_runs_on_clean_eof_without_host_access(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        observed = []

        async def cleanup():
            with self.assertRaises(S5rError) as raised:
                HostClient.host_supports(HostOperation.SESSION_STATE_READ)
            observed.append(raised.exception.code)

        worker.on_shutdown(cleanup)
        host = FakeHost(worker)
        await host.handshake()
        await host.shutdown()
        self.assertEqual(observed, [WireErrorCode.CONTEXT_UNAVAILABLE])

    async def test_hook_error_surfaces_after_clean_driver_exit(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def cleanup():
            raise S5rError.of(WireErrorCode.STORAGE_IO_ERROR, "flush failed")

        worker.on_shutdown(cleanup)
        host = FakeHost(worker)
        await host.handshake()
        host.transport.close_write()
        with self.assertRaises(S5rError) as raised:
            await host.worker_task
        self.assertEqual(raised.exception.code, WireErrorCode.STORAGE_IO_ERROR)

    async def test_plain_hook_error_is_wrapped(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def cleanup():
            raise RuntimeError("boom")

        worker.on_shutdown(cleanup)
        host = FakeHost(worker)
        await host.handshake()
        host.transport.close_write()
        with self.assertRaises(S5rError) as raised:
            await host.worker_task
        self.assertEqual(raised.exception.code, WireErrorCode.INTERNAL_ERROR)
        self.assertIn("boom", raised.exception.payload.message)

    async def test_hook_runs_but_driver_error_stays_authoritative(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        observed = []

        async def cleanup():
            observed.append("ran")
            raise S5rError.of(WireErrorCode.STORAGE_IO_ERROR, "flush failed")

        worker.on_shutdown(cleanup)
        host = FakeHost(worker)
        await host.handshake()
        # A result for an unknown request is a local protocol violation.
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": "ghost",
                "kind": "invoke",
                "output": None,
            }
        )
        with self.assertRaises(ProtocolError):
            await host.worker_task
        self.assertEqual(observed, ["ran"])

    async def test_hook_does_not_run_when_activation_fails(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        observed = []

        async def activate(config):
            raise S5rError.of(WireErrorCode.INVALID_INPUT, "bad config")

        async def cleanup():
            observed.append("ran")

        worker.on_activate(activate)
        worker.on_shutdown(cleanup)
        host = FakeHost(worker)
        await host.initialize()
        result = await host.activate()
        self.assertFalse(result.is_success)
        with self.assertRaises(S5rError):
            await host.worker_task
        self.assertEqual(observed, [])


class BackgroundHostTest(unittest.IsolatedAsyncioTestCase):
    async def test_delivered_after_activation_with_root_scope(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        host = FakeHost(
            worker,
            host_operations=[
                HostOperation.SESSION_ROOT_CREATE,
                HostOperation.SESSION_ROOT_DISPOSE,
            ],
        )
        background = worker.background_host()
        await host.handshake()
        background_host = await asyncio.wait_for(background, timeout=5)
        self.assertTrue(background_host.host_supports(HostOperation.SESSION_ROOT_CREATE))
        self.assertFalse(background_host.host_supports(HostOperation.SESSION_STATE_READ))

        create = asyncio.create_task(background_host.root_sessions().create_root())
        nested = await host.recv()
        self.assertEqual(nested.operation, HostOperation.SESSION_ROOT_CREATE)
        self.assertIsNone(nested.parent_invoke_id)
        self.assertEqual(nested.input, {})
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": nested.id,
                "kind": "invoke",
                "output": {"session_id": "root-1"},
            }
        )
        self.assertEqual((await create)["session_id"], "root-1")

        dispose = asyncio.create_task(
            background_host.root_sessions().dispose_root("root-1")
        )
        nested = await host.recv()
        self.assertEqual(nested.operation, HostOperation.SESSION_ROOT_DISPOSE)
        self.assertIsNone(nested.parent_invoke_id)
        self.assertEqual(nested.input, {"target_session_id": "root-1"})
        await host.send(
            {
                "type": "result",
                "status": "success",
                "id": nested.id,
                "kind": "invoke",
                "output": {"ok": True},
            }
        )
        await dispose
        await host.shutdown()

    async def test_dispose_root_rejects_bad_ack(self) -> None:
        for output in ({"ok": False}, {"ok": True, "unexpected": 1}, {}):
            with self.subTest(output=output):
                worker = Worker(EXT_ID, "0.1.0")
                host = FakeHost(
                    worker, host_operations=[HostOperation.SESSION_ROOT_DISPOSE]
                )
                background = worker.background_host()
                await host.handshake()
                background_host = await asyncio.wait_for(background, timeout=5)

                dispose = asyncio.create_task(
                    background_host.root_sessions().dispose_root("root-1")
                )
                nested = await host.recv()
                self.assertEqual(nested.operation, HostOperation.SESSION_ROOT_DISPOSE)
                self.assertIsNone(nested.parent_invoke_id)
                self.assertEqual(nested.input, {"target_session_id": "root-1"})
                await host.send(
                    {
                        "type": "result",
                        "status": "success",
                        "id": nested.id,
                        "kind": "invoke",
                        "output": output,
                    }
                )
                with self.assertRaises(S5rError) as raised:
                    await dispose
                self.assertEqual(
                    raised.exception.code, WireErrorCode.INVALID_RESPONSE
                )
                await host.shutdown()

    async def test_repeated_registration_supersedes_previous_future(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        first = worker.background_host()
        second = worker.background_host()
        self.assertTrue(first.cancelled())
        self.assertFalse(second.done())
        host = FakeHost(worker)
        await host.handshake()
        background_host = await asyncio.wait_for(second, timeout=5)
        self.assertFalse(background_host.host_supports(HostOperation.SESSION_ROOT_CREATE))
        await host.shutdown()

    async def test_registration_after_serve_start_fails(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        host = FakeHost(worker)
        await host.handshake()
        with self.assertRaises(S5rError) as raised:
            worker.background_host()
        self.assertEqual(raised.exception.code, WireErrorCode.INVALID_INPUT)
        await host.shutdown()

    async def test_future_fails_when_handshake_fails(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def activate(config):
            raise S5rError.of(WireErrorCode.INVALID_INPUT, "bad config")

        worker.on_activate(activate)
        background = worker.background_host()
        host = FakeHost(worker)
        await host.initialize()
        result = await host.activate()
        self.assertFalse(result.is_success)
        with self.assertRaises(S5rError):
            await background
        with self.assertRaises(S5rError):
            await host.worker_task

    async def test_calls_fail_closed_after_the_driver_stops(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")
        host = FakeHost(worker, host_operations=[HostOperation.SESSION_ROOT_CREATE])
        background = worker.background_host()
        await host.handshake()
        background_host = await asyncio.wait_for(background, timeout=5)
        await host.shutdown()
        with self.assertRaises(S5rError) as raised:
            await background_host.root_sessions().create_root()
        self.assertEqual(raised.exception.code, WireErrorCode.PEER_CLOSED)


class ParseHelperTest(unittest.TestCase):
    def test_parse_tool_arguments_dataclass(self) -> None:
        @dataclass
        class GreetArgs:
            name: str

        args = parse_tool_arguments({"name": "world"}, GreetArgs)
        self.assertEqual(args.name, "world")
        with self.assertRaises(S5rError):
            parse_tool_arguments({"unknown": 1}, GreetArgs)
        with self.assertRaises(S5rError):
            parse_tool_arguments({}, GreetArgs)

    def test_parse_tool_arguments_raw(self) -> None:
        self.assertEqual(parse_tool_arguments({"a": 1}), {"a": 1})


class DriverBackpressureTest(unittest.IsolatedAsyncioTestCase):
    async def test_saturated_stream_queue_retains_the_terminal_failure(self) -> None:
        transport, _ = MemoryTransport.pair()
        driver = _Driver(transport, Worker(EXT_ID, "0.1.0"), set(), [])
        pending = _StreamPending()
        driver._pending["stream-1"] = pending
        for index in range(_STREAM_BUFFER_CAPACITY):
            pending.queue.put_nowait({"type": "content_delta", "content": str(index)})

        driver._fail_stream(
            "stream-1",
            pending,
            S5rError.of(WireErrorCode.PEER_OVERLOADED, "queue full"),
            cancel_reason=None,
        )

        items = [pending.queue.get_nowait() for _ in range(_STREAM_BUFFER_CAPACITY)]
        self.assertIsInstance(items[-1], S5rError)

    async def test_shutdown_does_not_wait_on_a_full_queue_after_writer_exit(self) -> None:
        transport, _ = MemoryTransport.pair()
        driver = _Driver(transport, Worker(EXT_ID, "0.1.0"), set(), [])
        for index in range(_WRITE_QUEUE_CAPACITY):
            driver._write_queue.put_nowait({"index": index})
        writer = asyncio.create_task(asyncio.sleep(0))
        await writer

        await asyncio.wait_for(driver._shutdown(writer), timeout=0.1)


def make_driver(features=()) -> _Driver:
    transport, _ = MemoryTransport.pair()
    return _Driver(transport, Worker(EXT_ID, "0.1.0"), set(features), [])


class OutboundWriteFailureTest(unittest.IsolatedAsyncioTestCase):
    def fill_write_queue(self, driver: _Driver) -> None:
        for index in range(_WRITE_QUEUE_CAPACITY):
            driver._write_queue.put_nowait({"index": index})

    async def test_write_is_fail_fast_when_the_queue_is_full(self) -> None:
        driver = make_driver()
        self.fill_write_queue(driver)
        with self.assertRaises(ProtocolError):
            await driver._write({"type": "noop"})

    async def test_invoke_write_failure_releases_the_pending_entry(self) -> None:
        driver = make_driver()
        self.fill_write_queue(driver)
        with self.assertRaises(ProtocolError):
            await driver.invoke("astrcode.session.state.read", {})
        self.assertEqual(driver._pending, {})

    async def test_invoke_stream_write_failure_releases_the_pending_entry(self) -> None:
        driver = make_driver(features={FEATURE_MODEL_STREAM_V1})
        self.fill_write_queue(driver)
        with self.assertRaises(ProtocolError):
            driver.invoke_stream("astrcode.llm.main_chat", {})
        self.assertEqual(driver._pending, {})


class InboundAdmissionTest(unittest.IsolatedAsyncioTestCase):
    async def test_overloaded_inbound_invoke_is_rejected_not_queued(self) -> None:
        transport, host = MemoryTransport.pair()
        driver = _Driver(transport, Worker(EXT_ID, "0.1.0"), set(), [])
        driver_task = asyncio.create_task(driver.run())
        for index in range(_MAX_IN_FLIGHT_REQUESTS):
            await host.write_frame(
                encode_message(
                    {
                        "type": "invoke",
                        "id": f"in-{index}",
                        "operation": CONFORMANCE_WAIT_FOR_CANCEL,
                        "input": None,
                    }
                )
            )
        await host.write_frame(
            encode_message(
                {
                    "type": "invoke",
                    "id": "overflow",
                    "operation": CAP_RUNTIME_PING,
                    "input": None,
                }
            )
        )
        result = decode_message(await asyncio.wait_for(host.read_frame(), timeout=5))
        self.assertEqual(result.id, "overflow")
        self.assertFalse(result.is_success)
        self.assertEqual(result.error.code, WireErrorCode.PEER_OVERLOADED)
        self.assertEqual(
            result.error.message, "peer has reached its in-flight request limit"
        )
        host.close_write()
        await asyncio.wait_for(driver_task, timeout=5)


class ProducerStreamEventsTest(unittest.IsolatedAsyncioTestCase):
    async def write_events(self, payload) -> list[dict]:
        driver = make_driver()
        await driver._write_stream_events("r-1", CancelToken(), payload)
        events = []
        while not driver._write_queue.empty():
            events.append(driver._write_queue.get_nowait()["event"])
        return events

    async def test_event_before_started_is_replaced_by_a_terminal_failure(self) -> None:
        events = await self.write_events([{"type": "content_delta", "content": "x"}])
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["type"], "failed")
        self.assertEqual(events[0]["error"]["code"], WireErrorCode.INVALID_RESPONSE)
        self.assertEqual(
            events[0]["error"]["message"], "stream event arrived before started"
        )

    async def test_second_started_is_replaced_by_a_terminal_failure(self) -> None:
        events = await self.write_events([{"type": "started"}, {"type": "started"}])
        self.assertEqual([event["type"] for event in events], ["started", "failed"])
        self.assertEqual(events[1]["error"]["code"], WireErrorCode.INVALID_RESPONSE)
        self.assertEqual(
            events[1]["error"]["message"], "stream started more than once"
        )

    async def test_producer_close_without_terminal_synthesizes_stream_closed(self) -> None:
        events = await self.write_events(
            [{"type": "started"}, {"type": "content_delta", "content": "x"}]
        )
        self.assertEqual(
            [event["type"] for event in events],
            ["started", "content_delta", "failed"],
        )
        self.assertEqual(events[-1]["error"]["code"], WireErrorCode.STREAM_CLOSED)

    async def test_valid_stream_passes_through_unchanged(self) -> None:
        payload = [
            {"type": "started"},
            {"type": "content_delta", "content": "x"},
            {"type": "completed", "output": {"ok": True}},
        ]
        self.assertEqual(await self.write_events(payload), payload)

    async def test_idle_producer_fails_with_stream_idle_timeout(self) -> None:
        async def slow_events():
            await asyncio.sleep(5)
            yield {"type": "started"}

        with mock.patch.object(worker_module, "_STREAM_IDLE_TIMEOUT", 0.05):
            events = await self.write_events(slow_events())
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["type"], "failed")
        self.assertEqual(events[0]["error"]["code"], WireErrorCode.STREAM_IDLE_TIMEOUT)


class StreamForwardingTest(unittest.IsolatedAsyncioTestCase):
    async def test_forward_task_delivers_events_until_the_terminal(self) -> None:
        driver = make_driver()
        pending = _StreamPending()
        driver._pending["s-1"] = pending
        task = asyncio.create_task(driver._forward_stream("s-1", pending))
        pending.forward.put_nowait({"type": "started"})
        pending.forward.put_nowait({"type": "completed", "output": {}})
        await asyncio.wait_for(task, timeout=5)
        self.assertEqual(
            [pending.queue.get_nowait() for _ in range(2)],
            [{"type": "started"}, {"type": "completed", "output": {}}],
        )

    async def test_stalled_consumer_fails_with_backpressure_timeout(self) -> None:
        driver = make_driver()
        pending = _StreamPending()
        driver._pending["s-1"] = pending
        for index in range(_STREAM_BUFFER_CAPACITY):
            pending.queue.put_nowait({"type": "content_delta", "content": str(index)})
        pending.forward.put_nowait({"type": "content_delta", "content": "overflow"})
        with mock.patch.object(worker_module, "_STREAM_BACKPRESSURE_TIMEOUT", 0.05):
            task = asyncio.create_task(driver._forward_stream("s-1", pending))
            await asyncio.wait_for(task, timeout=5)
        self.assertNotIn("s-1", driver._pending)
        items = [pending.queue.get_nowait() for _ in range(_STREAM_BUFFER_CAPACITY)]
        self.assertIsInstance(items[-1], S5rError)
        self.assertEqual(items[-1].code, WireErrorCode.BACKPRESSURE_TIMEOUT)
        cancel = driver._write_queue.get_nowait()
        self.assertEqual(
            cancel, {"type": "cancel", "id": "s-1", "reason": "backpressure_timeout"}
        )

    async def test_full_forward_buffer_fails_with_peer_overloaded(self) -> None:
        driver = make_driver()
        pending = _StreamPending()
        driver._pending["s-1"] = pending
        pending.started = True
        for index in range(_STREAM_FORWARD_BUFFER_CAPACITY):
            pending.forward.put_nowait({"type": "content_delta", "content": str(index)})
        driver._route_stream(
            StreamMsg(id="s-1", event={"type": "content_delta", "content": "x"})
        )
        self.assertNotIn("s-1", driver._pending)
        item = pending.queue.get_nowait()
        self.assertIsInstance(item, S5rError)
        self.assertEqual(item.code, WireErrorCode.PEER_OVERLOADED)
        cancel = driver._write_queue.get_nowait()
        self.assertEqual(cancel["reason"], "stream_forward_queue_full")


class HookPriorityTest(unittest.TestCase):
    def test_priority_reaches_the_emitted_manifest(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def handler(event, ctx):
            return HandlerResult.ok()

        worker.on_pre_tool_use(handler, priority=5)
        worker.on_pre_compact(handler, priority=7)
        worker.on_continue_after_stop(-1, handler, priority=1)
        worker.hook("turn_end", "non_blocking", priority=3)(handler)
        worker.on_post_compact(handler)

        hooks = worker._manifest_json()["hooks"]
        self.assertEqual([hook.get("priority") for hook in hooks], [5, 7, 1, 3, None])
        self.assertNotIn("priority", hooks[4])
        self.assertEqual(hooks[2]["options"], {"max_per_turn": -1})

    def test_negative_priority_rejected(self) -> None:
        worker = Worker(EXT_ID, "0.1.0")

        async def handler(event, ctx):
            return HandlerResult.ok()

        for register in (
            lambda: worker.on_pre_tool_use(handler, priority=-1),
            lambda: worker.hook("turn_end", "non_blocking", handler, priority=-2),
        ):
            with self.assertRaises(S5rError) as caught:
                register()
            self.assertEqual(caught.exception.code, WireErrorCode.INVALID_HOOK_REGISTRATION)


if __name__ == "__main__":
    unittest.main()
