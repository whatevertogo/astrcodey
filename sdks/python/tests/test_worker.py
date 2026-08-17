"""Tool/hook/command round-trip, host-API, cancel, and conformance tests."""

from __future__ import annotations

import asyncio
import unittest
from dataclasses import dataclass

from harness import FakeHostBase
from memory import MemoryTransport
from test_handshake import ALL_FEATURES
from s5r import (
    HandlerResult,
    HostClient,
    HostOperation,
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
from s5r.protocol import (
    CAP_HANDLER_INVOKE,
    CAP_RUNTIME_PING,
    CONFORMANCE_HOST_ECHO,
    CONFORMANCE_NESTED,
    CONFORMANCE_STREAM,
    CONFORMANCE_UNKNOWN_ERROR,
    CONFORMANCE_UNARY,
    CONFORMANCE_WAIT_FOR_CANCEL,
)
from s5r.results import FileOperation, HostResource
from s5r.worker import (
    _STREAM_BUFFER_CAPACITY,
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

if __name__ == "__main__":
    unittest.main()
