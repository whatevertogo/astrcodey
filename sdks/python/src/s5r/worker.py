"""Worker runtime: the extension subprocess entry point.

Mirrors `astrcode_extension_worker::worker::Worker` (registration API) and the
worker side of `astrcode_extension_sdk::wire::PeerDriver` (post-handshake I/O).
The conformance operations (`s5r.conformance.*`) and `s5r.runtime.ping` are
built in, exactly as in the Rust worker, so any worker built on this SDK can
serve the `s5r-conformance` suite.
"""

from __future__ import annotations

import asyncio
import inspect
from collections import OrderedDict
from typing import Any, AsyncIterator, Awaitable, Callable, Mapping

from .context import (
    CancelToken,
    WorkerCallContext,
    WorkerCommandContext,
    WorkerCommandInvocation,
    WorkerCustomEventContext,
    WorkerInvocationContext,
    WorkerToolPlanContext,
    _CallFacts,
)
from .errors import ErrorPayload, ProtocolError, S5rError, WireErrorCode
from .frames import FrameTransport, StdioTransport
from .host import BackgroundHost, _HostBinding, _current_binding
from .manifest import (
    ALL_CAPABILITIES,
    ALL_CUSTOM_EVENT_DELIVERIES,
    ALL_EXTENSION_HTTP_METHODS,
    ALL_HOOK_MODES,
    ALL_LIFECYCLE_EVENTS,
    ALL_TRANSPORT_FEATURES,
    CompactEvent,
    CustomEventDeclaration,
    CustomEventSubscription,
    ExtensionHttpRoute,
    FIXED_HOOK_MODES,
    HookMode,
    LifecycleEvent,
    SlashCommand,
    ToolDefinition,
    ToolMode,
    extension_http_route_patterns_conflict,
    hook_mode_is_supported,
    validate_extension_http_route,
)
from .protocol import (
    CAP_HANDLER_INVOKE,
    CAP_RUNTIME_PING,
    CONFORMANCE_HOST_ECHO,
    CONFORMANCE_NESTED,
    CONFORMANCE_STREAM,
    CONFORMANCE_UNARY,
    CONFORMANCE_UNKNOWN_ERROR,
    CONFORMANCE_WAIT_FOR_CANCEL,
    FEATURE_CUSTOM_EVENT_V1,
    FEATURE_MODEL_STREAM_V1,
    FEATURE_NESTED_INVOKE_V1,
    S5R_VERSION,
    TERMINAL_STREAM_EVENTS,
    ActivateMsg,
    CancelMsg,
    InitializeMsg,
    InvokeMsg,
    ResultMsg,
    StreamMsg,
    cancel_message,
    decode_message,
    encode_message,
    invoke_message,
    negotiate_features,
    result_failure,
    result_success,
    stream_message,
)
from .results import HandlerEffect, HandlerResult, ToolPlan

ToolHandlerFn = Callable[[Any, WorkerInvocationContext], Awaitable[HandlerResult]]
ToolPlannerFn = Callable[[Any, WorkerToolPlanContext], Awaitable[ToolPlan]]
HookHandlerFn = Callable[[Any, WorkerInvocationContext], Awaitable[HandlerResult]]
ContinuationHandlerFn = Callable[[Any, WorkerCallContext], Awaitable[HandlerResult]]
CommandHandlerFn = Callable[[WorkerCommandContext], Awaitable[HandlerResult]]
CustomEventHandlerFn = Callable[[Any, WorkerCustomEventContext], Awaitable[HandlerResult]]
HttpHandlerFn = Callable[[Any, WorkerCallContext], Awaitable[Mapping[str, Any]]]
ActivationHandlerFn = Callable[[Any], Awaitable[None]]
ShutdownHandlerFn = Callable[[], Awaitable[None]]

# The Rust worker declares all three v1 features; the conformance host requires
# all of them, so this set is not configurable.
_SUPPORTED_FEATURES = frozenset(
    {FEATURE_NESTED_INVOKE_V1, FEATURE_MODEL_STREAM_V1, FEATURE_CUSTOM_EVENT_V1}
)

_WRITE_QUEUE_CAPACITY = 256
_STREAM_BUFFER_CAPACITY = 32
_STREAM_FORWARD_BUFFER_CAPACITY = 256
_STREAM_BACKPRESSURE_TIMEOUT = 30.0
_STREAM_IDLE_TIMEOUT = 120.0
_CANCELLED_REQUEST_CAPACITY = 256
_MAX_IN_FLIGHT_REQUESTS = 256

_HANDLER_ID_KINDS = frozenset({"tool", "hook", "command", "http", "event"})

MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN = 64

_FIXED_HOOK_HINTS = {
    LifecycleEvent.TOOL_INPUT_TRANSFORM: "use Worker.on_tool_input_transform(...) instead",
    LifecycleEvent.PRE_TOOL_USE: "use Worker.on_pre_tool_use(...) instead",
    LifecycleEvent.AFTER_PROVIDER_RESPONSE: "use Worker.on_after_provider_response(...) instead",
    LifecycleEvent.PROVIDER_CONTRIBUTION: "use Worker.on_provider_contribution(...) instead",
    LifecycleEvent.CONTINUE_AFTER_STOP: "use Worker.on_continue_after_stop(...) instead",
    LifecycleEvent.PROMPT_BUILD: "use Worker.on_prompt_build(...) instead",
}


async def _resolve(value: Any) -> Any:
    if inspect.isawaitable(value):
        return await value
    return value


class Worker:
    """S5R 3.0 worker: registers handlers and serves them over stdio."""

    def __init__(self, extension_id: str, version: str):
        if not extension_id:
            raise ValueError("extension_id must not be empty")
        self._extension_id = extension_id
        self._version = version
        self._activation: ActivationHandlerFn | None = None
        self._shutdown: ShutdownHandlerFn | None = None
        self._background_host_future: asyncio.Future[BackgroundHost] | None = None
        self._serving = False
        self._capabilities: list[str] = []
        self._transport_features: list[str] = []
        self._tools: dict[str, tuple[ToolPlannerFn | None, ToolHandlerFn]] = {}
        self._hooks: dict[str, HookHandlerFn] = {}
        self._continuation_hooks: dict[str, ContinuationHandlerFn] = {}
        self._commands: dict[str, CommandHandlerFn] = {}
        self._custom_events: dict[str, CustomEventHandlerFn] = {}
        self._http_routes: dict[str, HttpHandlerFn] = {}
        self._tool_manifest: list[dict[str, Any]] = []
        self._hook_manifest: list[dict[str, Any]] = []
        self._command_manifest: list[dict[str, Any]] = []
        self._custom_event_manifest: list[dict[str, Any]] = []
        self._custom_event_subscription_manifest: list[dict[str, Any]] = []
        self._http_route_manifest: list[dict[str, Any]] = []

    # ── registration ────────────────────────────────────────────────────────

    def on_activate(self, handler: ActivationHandlerFn) -> None:
        """Handle the host-owned configuration before this worker goes ready."""
        self._activation = handler

    def on_shutdown(self, handler: ShutdownHandlerFn) -> None:
        """Register a best-effort cleanup hook that runs once after serving ends.

        The hook runs only after a successful activation and for any driver
        outcome (clean EOF or error). The host does not wait for it and
        terminates the process tree after a bounded grace period, so keep it
        fast. `HostClient` is unavailable inside the hook: use it for
        worker-local cleanup (flushing files, closing worker-owned
        connections), not for reporting results back to the host.
        """
        self._shutdown = handler

    def background_host(self) -> asyncio.Future[BackgroundHost]:
        """Register the receiver for the post-activation background host handle.

        The returned future resolves once activation completes, before the
        message driver starts, with a `BackgroundHost` that carries no turn
        scope and exposes only the root session domain — safe to hold in
        `asyncio` background tasks (poll loops, external agents driving root
        sessions). Must be called inside the worker's event loop before
        `serve` starts; repeated calls supersede the previous future.
        """
        if self._serving:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                "background_host() must be called before serve() starts",
            )
        previous = self._background_host_future
        if previous is not None and not previous.done():
            previous.cancel()
        future: asyncio.Future[BackgroundHost] = (
            asyncio.get_running_loop().create_future()
        )
        self._background_host_future = future
        return future

    def capability(self, capability: str) -> None:
        if capability not in ALL_CAPABILITIES:
            raise ValueError(f"unknown extension capability {capability!r}")
        if capability not in self._capabilities:
            self._capabilities.append(capability)

    def require_transport(self, feature: str) -> None:
        if feature not in ALL_TRANSPORT_FEATURES:
            raise ValueError(f"unknown transport feature {feature!r}")
        if feature not in self._transport_features:
            self._transport_features.append(feature)

    def tool(
        self,
        definition: ToolDefinition,
        handler: ToolHandlerFn | None = None,
        *,
        planner: ToolPlannerFn | None = None,
    ) -> ToolHandlerFn | Callable[[ToolHandlerFn], ToolHandlerFn]:
        """Register a tool: manifest definition and handlers in one call.

        Usable directly or as a decorator (`@worker.tool(definition)`). The
        planner defaults to declaring no resources.
        """
        if handler is None:
            return lambda fn: self.tool(definition, fn, planner=planner)  # type: ignore[return-value]
        name = definition.name.strip()
        if name in self._tools:
            raise S5rError.of(
                WireErrorCode.DUPLICATE_REGISTRATION, f"duplicate tool registration: {name}"
            )
        if definition.mode not in (ToolMode.PARALLEL, ToolMode.SEQUENTIAL):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, f"unknown tool mode {definition.mode!r}"
            )
        if definition.timeout_ms is not None and definition.timeout_ms <= 0:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, "tool timeout_ms must be greater than zero"
            )
        definition.name = name
        self._tools[name] = (planner, handler)
        self._tool_manifest.append(definition.to_manifest())
        return handler

    def hook(
        self,
        on: str,
        mode: str,
        handler: HookHandlerFn | None = None,
        *,
        priority: int | None = None,
    ) -> HookHandlerFn | Callable[[HookHandlerFn], HookHandlerFn]:
        """Register a mode-flexible lifecycle hook; fixed-mode events reject this.

        `priority` orders hooks across extensions (descending, registration
        order on ties); omit it to keep the default of 0.
        """
        if handler is None:
            return lambda fn: self.hook(on, mode, fn, priority=priority)  # type: ignore[return-value]
        if on not in ALL_LIFECYCLE_EVENTS:
            raise S5rError.of(WireErrorCode.UNSUPPORTED_HOOK, f"unknown lifecycle event {on!r}")
        if on == LifecycleEvent.USER_MESSAGE_ENVELOPE:
            raise S5rError.of(
                WireErrorCode.UNSUPPORTED_HOOK,
                "user_message_envelope is not supported by S5R workers",
            )
        if mode not in ALL_HOOK_MODES:
            raise S5rError.of(WireErrorCode.INVALID_HOOK_MODE, f"unknown hook mode {mode!r}")
        fixed = FIXED_HOOK_MODES.get(on)
        if fixed is not None:
            hint = _FIXED_HOOK_HINTS.get(
                on, "use the dedicated fixed-mode Worker registration method instead"
            )
            raise S5rError.of(
                WireErrorCode.TYPED_HOOK_REQUIRED,
                f"{on} has fixed {fixed} mode; {hint}",
            )
        if not hook_mode_is_supported(on, mode):
            raise S5rError.of(
                WireErrorCode.INVALID_HOOK_MODE, f"{on} does not support {mode} mode"
            )
        declaration = _hook_declaration(on, mode, priority)
        self._insert_hook(on, handler)
        self._hook_manifest.append(declaration)
        return handler

    def on_tool_input_transform(
        self, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._fixed_hook(LifecycleEvent.TOOL_INPUT_TRANSFORM, handler, priority=priority)

    def on_pre_tool_use(
        self, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._fixed_hook(LifecycleEvent.PRE_TOOL_USE, handler, priority=priority)

    def on_after_provider_response(
        self, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._fixed_hook(
            LifecycleEvent.AFTER_PROVIDER_RESPONSE, handler, priority=priority
        )

    def on_provider_contribution(
        self, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._fixed_hook(
            LifecycleEvent.PROVIDER_CONTRIBUTION, handler, priority=priority
        )

    def on_prompt_build(
        self, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._fixed_hook(LifecycleEvent.PROMPT_BUILD, handler, priority=priority)

    def on_pre_compact(
        self, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._compact_hook(CompactEvent.PRE_COMPACT, handler, priority=priority)

    def on_post_compact(
        self, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._compact_hook(CompactEvent.POST_COMPACT, handler, priority=priority)

    def on_continue_after_stop(
        self, max_per_turn: int, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        """`max_per_turn = -1` means unlimited; otherwise a non-negative cap."""
        if max_per_turn < -1:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                "continue_after_stop max_per_turn must be -1 or a non-negative integer",
            )
        declaration = _hook_declaration(
            LifecycleEvent.CONTINUE_AFTER_STOP, HookMode.BLOCKING, priority
        )
        declaration["options"] = {"max_per_turn": max_per_turn}
        self._insert_hook(LifecycleEvent.CONTINUE_AFTER_STOP, handler)
        self._hook_manifest.append(declaration)
        return handler

    def continuation_hook_handler(
        self, on: str, handler: ContinuationHandlerFn
    ) -> ContinuationHandlerFn:
        """Register a handler only reachable via a hook continuation."""
        if on in self._hooks or on in self._continuation_hooks:
            raise S5rError.of(
                WireErrorCode.DUPLICATE_REGISTRATION, f"duplicate hook registration: {on}"
            )
        self._continuation_hooks[on] = handler
        return handler

    def command(
        self,
        command: SlashCommand,
        handler: CommandHandlerFn | None = None,
    ) -> CommandHandlerFn | Callable[[CommandHandlerFn], CommandHandlerFn]:
        if handler is None:
            return lambda fn: self.command(command, fn)  # type: ignore[return-value]
        name = command.name.strip()
        if name in self._commands:
            raise S5rError.of(
                WireErrorCode.DUPLICATE_REGISTRATION,
                f"duplicate command registration: {name}",
            )
        command.name = name
        self._commands[name] = handler
        self._command_manifest.append(command.to_manifest())
        return handler

    def custom_event(self, declaration: CustomEventDeclaration) -> None:
        """Declare an emittable custom-event schema."""
        if declaration.delivery not in ALL_CUSTOM_EVENT_DELIVERIES:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                f"unknown custom event delivery {declaration.delivery!r}",
            )
        declaration.event_type = declaration.event_type.strip()
        self._custom_event_manifest.append(declaration.to_manifest())

    def on_custom_event(
        self, subscription: CustomEventSubscription, handler: CustomEventHandlerFn
    ) -> CustomEventHandlerFn:
        subscription.id = subscription.id.strip()
        subscription.event_type = subscription.event_type.strip()
        reason = _validate_custom_event_subscription(subscription)
        if reason is not None:
            raise S5rError.of(WireErrorCode.INVALID_INPUT, reason)
        if subscription.id in self._custom_events:
            raise S5rError.of(
                WireErrorCode.DUPLICATE_REGISTRATION,
                f"duplicate custom event subscription: {subscription.id}",
            )
        self._custom_events[subscription.id] = handler
        self._custom_event_subscription_manifest.append(subscription.to_manifest())
        return handler

    def http_route(
        self,
        route: ExtensionHttpRoute,
        handler: HttpHandlerFn | None = None,
    ) -> HttpHandlerFn | Callable[[HttpHandlerFn], HttpHandlerFn]:
        """Register an HTTP route: manifest declaration and handler in one call.

        Usable directly or as a decorator (`@worker.http_route(route)`). The
        handler receives the wire `ExtensionHttpRequest` mapping and a
        `WorkerCallContext` (no session scope), and must return an
        `ExtensionHttpResponse` mapping `{"status": int, "body": ...}`.
        """
        if handler is None:
            return lambda fn: self.http_route(route, fn)  # type: ignore[return-value]
        reason = validate_extension_http_route(route)
        if reason is not None:
            raise S5rError.of(WireErrorCode.INVALID_HTTP_ROUTE, reason)
        for entry in self._http_route_manifest:
            existing = entry["route"]
            if (
                existing["access"] == route.access
                and existing["method"] == route.method
                and extension_http_route_patterns_conflict(existing["path"], route.path)
            ):
                raise S5rError.of(
                    WireErrorCode.DUPLICATE_REGISTRATION,
                    f"conflicting HTTP route registration: {route.path}",
                )
        handler_name = f"route_{len(self._http_route_manifest)}"
        self._http_routes[handler_name] = handler
        self._http_route_manifest.append(
            {
                "route": route.to_manifest(),
                "handler_id": f"{self._extension_id}:http:{handler_name}",
            }
        )
        return handler

    def _fixed_hook(
        self, event: str, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._register_hook(event, FIXED_HOOK_MODES[event], handler, priority)

    def _compact_hook(
        self, event: str, handler: HookHandlerFn, *, priority: int | None = None
    ) -> HookHandlerFn:
        return self._register_hook(event, HookMode.BLOCKING, handler, priority)

    def _register_hook(
        self, event: str, mode: str, handler: HookHandlerFn, priority: int | None = None
    ) -> HookHandlerFn:
        declaration = _hook_declaration(event, mode, priority)
        self._insert_hook(event, handler)
        self._hook_manifest.append(declaration)
        return handler

    def _insert_hook(self, on: str, handler: HookHandlerFn) -> None:
        if on in self._hooks or on in self._continuation_hooks:
            raise S5rError.of(
                WireErrorCode.DUPLICATE_REGISTRATION, f"duplicate hook registration: {on}"
            )
        self._hooks[on] = handler

    # ── serving ─────────────────────────────────────────────────────────────

    def run_stdio(self) -> None:
        """Serve over stdin/stdout until the host closes the connection.

        Returns cleanly on EOF. Raises `S5rError` / `ProtocolError` /
        `FrameError` on protocol or framing failures; an uncaught raise exits
        the process non-zero, which is what hosts expect from a failed worker.
        """
        asyncio.run(self._run_stdio())

    async def _run_stdio(self) -> None:
        transport = await StdioTransport.connect()
        await self.serve(transport)

    async def serve(self, transport: FrameTransport) -> None:
        """Handshake, then run the message driver over `transport`."""
        self._serving = True
        try:
            negotiated, host_operations = await self._accept_initialize(transport)
            await self._accept_activation(transport)
        except BaseException as error:
            self._fail_background_host(error)
            raise
        driver = _Driver(
            transport=transport,
            worker=self,
            negotiated_features=negotiated,
            host_operations=host_operations,
        )
        self._deliver_background_host(driver)
        driver_error: BaseException | None = None
        try:
            await driver.run()
        except BaseException as error:
            driver_error = error
        await self._run_shutdown_hook(driver_error)

    def _deliver_background_host(self, driver: _Driver) -> None:
        future = self._background_host_future
        self._background_host_future = None
        if future is None or future.done():
            return
        # Root binding: parent_invoke_id stays None, so the host treats these
        # calls as a detached context.
        future.set_result(
            BackgroundHost(
                invoke=lambda operation, input: driver.invoke(operation, input),
                host_operations=frozenset(driver.host_operations),
            )
        )

    def _fail_background_host(self, error: BaseException) -> None:
        future = self._background_host_future
        self._background_host_future = None
        if future is not None and not future.done():
            future.set_exception(error)

    async def _run_shutdown_hook(self, driver_error: BaseException | None) -> None:
        """Run the shutdown hook after the driver ends.

        The hook's error surfaces only when the driver itself finished
        cleanly, keeping the driver's root-cause error authoritative.
        """
        if self._shutdown is None:
            if driver_error is not None:
                raise driver_error
            return
        shutdown_error: S5rError | None = None
        try:
            await _resolve(self._shutdown())
        except S5rError as error:
            shutdown_error = error
        except Exception as error:
            shutdown_error = S5rError(
                ErrorPayload(
                    WireErrorCode.INTERNAL_ERROR,
                    f"worker shutdown failed: {error}",
                )
            )
        if driver_error is not None:
            raise driver_error
        if shutdown_error is not None:
            raise shutdown_error

    async def _accept_initialize(self, transport: FrameTransport) -> tuple[set[str], list[str]]:
        try:
            payload = await transport.read_frame()
        except EOFError as error:
            raise ProtocolError("host closed the connection before initialize") from error
        message = decode_message(payload)
        if not isinstance(message, InitializeMsg):
            raise ProtocolError("expected initialize request")
        try:
            negotiated = self._validate_initialize(message)
        except S5rError as error:
            await transport.write_frame(
                encode_message(result_failure(message.id, "initialize", error.payload))
            )
            raise
        output = {
            "worker": {"name": self._extension_id, "version": self._version},
            "protocol_version": S5R_VERSION,
            "supported_features": sorted(_SUPPORTED_FEATURES),
            "required_features": [],
            "negotiated_features": sorted(negotiated),
            "manifest": self._manifest_json(),
        }
        await transport.write_frame(
            encode_message(result_success(message.id, "initialize", output))
        )
        return negotiated, message.host_operations

    def _validate_initialize(self, message: InitializeMsg) -> set[str]:
        if not message.id:
            raise S5rError.of(
                WireErrorCode.INVALID_REQUEST, "initialize request id must not be empty"
            )
        if message.protocol_version != S5R_VERSION:
            raise S5rError.of(
                WireErrorCode.UNSUPPORTED_PROTOCOL_VERSION,
                f"unsupported S5R version {message.protocol_version}; expected {S5R_VERSION}",
            )
        if not message.host_name:
            raise S5rError.of(WireErrorCode.INVALID_REQUEST, "peer name must not be empty")
        if message.extension_id != self._extension_id:
            raise S5rError.of(
                WireErrorCode.INVALID_REQUEST,
                f"host expected extension {message.extension_id!r}, "
                f"worker identity is {self._extension_id!r}",
            )
        seen: set[str] = set()
        for operation in message.host_operations:
            if not operation:
                raise S5rError.of(
                    WireErrorCode.INVALID_REQUEST, "host operation name must not be empty"
                )
            if operation in seen:
                raise S5rError.of(
                    WireErrorCode.INVALID_REQUEST, f"duplicate host operation {operation}"
                )
            seen.add(operation)
        return negotiate_features(
            set(_SUPPORTED_FEATURES),
            message.supported_features,
            message.required_features,
        )

    async def _accept_activation(self, transport: FrameTransport) -> None:
        try:
            payload = await transport.read_frame()
        except EOFError as error:
            raise ProtocolError("host closed the connection before activate") from error
        message = decode_message(payload)
        if not isinstance(message, ActivateMsg):
            raise ProtocolError("expected activate request")

        async def fail(error: ErrorPayload) -> None:
            await transport.write_frame(
                encode_message(result_failure(message.id, "activate", error))
            )

        if not message.id:
            error = ErrorPayload(
                WireErrorCode.INVALID_REQUEST, "activate request id must not be empty"
            )
            await fail(error)
            raise S5rError(error)
        if self._activation is not None:
            try:
                await _resolve(self._activation(message.config))
            except S5rError as error:
                await fail(error.payload)
                raise
            except Exception as error:
                payload_out = ErrorPayload(
                    WireErrorCode.INTERNAL_ERROR, f"worker activation failed: {error}"
                )
                await fail(payload_out)
                raise S5rError(payload_out) from error
        await transport.write_frame(
            encode_message(result_success(message.id, "activate", {}))
        )

    def _manifest_json(self) -> dict[str, Any]:
        return {
            "required_transport_features": list(self._transport_features),
            "capabilities": list(self._capabilities),
            "tools": list(self._tool_manifest),
            "hooks": list(self._hook_manifest),
            "commands": list(self._command_manifest),
            "http_routes": list(self._http_route_manifest),
            "custom_events": list(self._custom_event_manifest),
            "custom_event_subscriptions": list(self._custom_event_subscription_manifest),
        }

    # ── inbound dispatch ────────────────────────────────────────────────────

    async def _dispatch(
        self, message: InvokeMsg, token: CancelToken, driver: _Driver
    ) -> tuple[str, Any]:
        operation = message.operation
        if operation == CAP_RUNTIME_PING:
            return "unary", {"ok": True}
        if operation == CONFORMANCE_UNARY:
            return "unary", message.input
        if operation == CONFORMANCE_STREAM:
            return "stream", [
                {"type": "started"},
                {"type": "content_delta", "content": "first"},
                {"type": "content_delta", "content": "second"},
                {"type": "completed", "output": message.input},
            ]
        if operation == CONFORMANCE_NESTED:
            try:
                output = await driver.invoke(
                    CONFORMANCE_HOST_ECHO, message.input, parent_invoke_id=message.id
                )
            except S5rError as error:
                raise S5rError.of(WireErrorCode.NESTED_FAILED, str(error)) from error
            return "unary", output
        if operation == CONFORMANCE_WAIT_FOR_CANCEL:
            await token.wait_cancelled()
            raise S5rError.of(WireErrorCode.CANCELLED, "conformance invocation cancelled")
        if operation == CONFORMANCE_UNKNOWN_ERROR:
            raise S5rError(
                ErrorPayload(
                    "future_conformance_error", "unknown error code preservation probe"
                )
            )
        if operation != CAP_HANDLER_INVOKE:
            raise S5rError.of(
                WireErrorCode.UNKNOWN_CAPABILITY,
                f"worker does not handle capability {operation}",
            )
        binding = _HostBinding(
            invoke=lambda op, inp: driver.invoke(op, inp, parent_invoke_id=message.id),
            invoke_stream=lambda op, inp: driver.invoke_stream(
                op, inp, parent_invoke_id=message.id
            ),
            host_operations=frozenset(driver.host_operations),
        )
        reset = _current_binding.set(binding)
        try:
            result = await self._dispatch_handler_invoke(message.input, token)
        finally:
            _current_binding.reset(reset)
        return "unary", result.to_json()

    async def _dispatch_handler_invoke(self, input: Any, token: CancelToken) -> HandlerResult:
        if token.is_cancelled():
            raise S5rError.of(WireErrorCode.CANCELLED, "handler invocation cancelled")
        if not isinstance(input, Mapping):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, "invalid handler invocation: expected an object"
            )
        unknown = set(input) - {"handler_id", "event"}
        if unknown:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                f"invalid handler invocation: unknown fields {sorted(unknown)}",
            )
        handler_id = input.get("handler_id")
        event = input.get("event")
        parts = _split_handler_id(handler_id)
        if parts is None:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, f"invalid handler id: {handler_id!r}"
            )
        owner, kind, name = parts
        if owner != self._extension_id:
            raise S5rError.of(WireErrorCode.UNKNOWN_HANDLER, f"unknown handler: {handler_id}")
        facts = _CallFacts.from_event(event)
        if kind == "tool":
            return await self._dispatch_tool(name, event, facts, token)
        if kind == "hook":
            return await self._dispatch_hook(name, event, facts, token)
        if kind == "command":
            return await self._dispatch_command(name, event, facts, token)
        if kind == "http":
            return await self._dispatch_http(name, event, token)
        return await self._dispatch_custom_event(name, event, facts, token)

    async def _dispatch_tool(
        self, name: str, event: Any, facts: _CallFacts, token: CancelToken
    ) -> HandlerResult:
        entry = self._tools.get(name)
        if entry is None:
            raise S5rError.of(WireErrorCode.UNKNOWN_HANDLER, f"unknown tool: {name}")
        planner, handler = entry
        if not isinstance(event, Mapping):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, "invalid tool invocation: expected an object"
            )
        unknown = set(event) - {"phase", "arguments", "scope"}
        if unknown:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                f"invalid tool invocation: unknown fields {sorted(unknown)}",
            )
        for required in ("phase", "arguments", "scope"):
            if required not in event:
                raise S5rError.of(
                    WireErrorCode.INVALID_INPUT,
                    f"invalid tool invocation: missing field {required!r}",
                )
        phase = event["phase"]
        if phase not in ("plan", "execute"):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, f"invalid tool invocation phase {phase!r}"
            )
        session_id, working_dir, turn_id, tool_call_id = _parse_tool_scope(event["scope"])
        arguments = event["arguments"]
        if phase == "plan":
            context = WorkerToolPlanContext(
                extension_id=self._extension_id,
                session_id=session_id,
                working_dir=working_dir,
                turn_id=turn_id,
                tool_call_id=tool_call_id,
                cancel_token=token,
            )
            plan = ToolPlan()
            if planner is not None:
                plan = await _resolve(planner(arguments, context))
            if not isinstance(plan, ToolPlan):
                raise S5rError.of(
                    WireErrorCode.SERIALIZATION_FAILED,
                    "tool planner must return a ToolPlan",
                )
            return HandlerResult(effect=HandlerEffect.TOOL_PLAN, data=plan.to_json())
        context = WorkerInvocationContext(
            extension_id=self._extension_id,
            session_id=session_id,
            working_dir=working_dir,
            turn_id=turn_id,
            tool_call_id=tool_call_id,
            cancel_token=token,
        )
        return _ensure_handler_result(await _resolve(handler(arguments, context)))

    async def _dispatch_hook(
        self, name: str, event: Any, facts: _CallFacts, token: CancelToken
    ) -> HandlerResult:
        handler = self._hooks.get(name)
        if handler is not None:
            context = WorkerInvocationContext(
                extension_id=self._extension_id,
                session_id=facts.require("hook", "session_id"),
                working_dir=facts.require("hook", "working_dir"),
                turn_id=facts.turn_id,
                tool_call_id=facts.tool_call_id,
                cancel_token=token,
            )
            return _ensure_handler_result(await _resolve(handler(event, context)))
        continuation = self._continuation_hooks.get(name)
        if continuation is not None:
            context = WorkerCallContext(
                extension_id=self._extension_id, cancel_token=token
            )
            return _ensure_handler_result(await _resolve(continuation(event, context)))
        raise S5rError.of(WireErrorCode.UNKNOWN_HANDLER, f"unknown hook: {name}")

    async def _dispatch_command(
        self, name: str, event: Any, facts: _CallFacts, token: CancelToken
    ) -> HandlerResult:
        handler = self._commands.get(name)
        if handler is None:
            raise S5rError.of(WireErrorCode.UNKNOWN_HANDLER, f"unknown command: {name}")
        session_id = facts.require("command", "session_id")
        working_dir = facts.require("command", "working_dir")
        # `WorkerCommandEvent` in Rust derives Deserialize without
        # deny_unknown_fields: extra keys (e.g. session facts) are tolerated.
        if not isinstance(event, Mapping):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, "invalid command invocation: expected an object"
            )
        on = event.get("on")
        command_input = event.get("input")
        if not isinstance(command_input, Mapping):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, "invalid command invocation input"
            )
        command_name = command_input.get("command_name")
        argument = command_input.get("argument")
        model = command_input.get("model")
        if (
            not isinstance(command_name, str)
            or not isinstance(argument, str)
            or not isinstance(model, Mapping)
        ):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                "invalid command invocation: command_name/argument/model are required",
            )
        if command_name != name:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                f"command invocation name {command_name} does not match handler {name}",
            )
        if on == "command":
            invocation = WorkerCommandInvocation.execute()
        elif on == "command_complete":
            cursor = command_input.get("cursor")
            if not isinstance(cursor, int) or isinstance(cursor, bool):
                raise S5rError.of(
                    WireErrorCode.INVALID_INPUT,
                    "invalid command completion: cursor must be an integer",
                )
            invocation = WorkerCommandInvocation.complete(cursor)
        else:
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT, f"invalid command invocation on {on!r}"
            )
        context = WorkerCommandContext(
            extension_id=self._extension_id,
            session_id=session_id,
            working_dir=working_dir,
            command_name=command_name,
            argument=argument,
            model=dict(model),
            invocation=invocation,
            cancel_token=token,
        )
        return _ensure_handler_result(await _resolve(handler(context)))

    async def _dispatch_custom_event(
        self, name: str, event: Any, facts: _CallFacts, token: CancelToken
    ) -> HandlerResult:
        handler = self._custom_events.get(name)
        if handler is None:
            raise S5rError.of(
                WireErrorCode.UNKNOWN_HANDLER,
                f"unknown custom event subscription: {name}",
            )
        context = WorkerCustomEventContext(
            extension_id=self._extension_id,
            session_id=facts.require("custom event", "session_id"),
            turn_id=facts.turn_id,
            cancel_token=token,
        )
        return _ensure_handler_result(await _resolve(handler(event, context)))

    async def _dispatch_http(
        self, name: str, event: Any, token: CancelToken
    ) -> HandlerResult:
        handler = self._http_routes.get(name)
        if handler is None:
            raise S5rError.of(WireErrorCode.UNKNOWN_HANDLER, f"unknown HTTP route: {name}")
        request = _parse_http_request(event)
        context = WorkerCallContext(
            extension_id=self._extension_id, cancel_token=token
        )
        response = await _resolve(handler(request, context))
        return HandlerResult(
            effect=HandlerEffect.HTTP_RESPONSE, data=_http_response_json(response)
        )


def _split_handler_id(handler_id: Any) -> tuple[str, str, str] | None:
    if not isinstance(handler_id, str):
        return None
    parts = handler_id.split(":", 2)
    if len(parts) != 3:
        return None
    owner, kind, name = parts
    if not owner or not name or kind not in _HANDLER_ID_KINDS:
        return None
    return owner, kind, name


_HTTP_REQUEST_FIELDS = frozenset({"method", "path", "path_params", "query", "body"})


def _parse_http_request(event: Any) -> dict[str, Any]:
    """Strict-decode a wire `ExtensionHttpRequest`, serde defaults applied."""
    if not isinstance(event, Mapping):
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            "invalid HTTP request payload: expected an object",
        )
    unknown = set(event) - _HTTP_REQUEST_FIELDS
    if unknown:
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            f"invalid HTTP request payload: unknown fields {sorted(unknown)}",
        )
    method = event.get("method")
    if method not in ALL_EXTENSION_HTTP_METHODS:
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            f"invalid HTTP request payload: unknown method {method!r}",
        )
    path = event.get("path")
    if not isinstance(path, str):
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            "invalid HTTP request payload: path must be a string",
        )
    path_params = event.get("path_params", {})
    if not isinstance(path_params, Mapping) or any(
        not isinstance(key, str) or not isinstance(value, str)
        for key, value in path_params.items()
    ):
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            "invalid HTTP request payload: path_params must map strings to strings",
        )
    query = event.get("query")
    if query is not None and not isinstance(query, str):
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            "invalid HTTP request payload: query must be a string",
        )
    return {
        "method": method,
        "path": path,
        "path_params": dict(path_params),
        "query": query,
        "body": event.get("body"),
    }


def _http_response_json(response: Any) -> dict[str, Any]:
    """Validate a handler-returned `ExtensionHttpResponse` for the wire."""
    if not isinstance(response, Mapping):
        raise S5rError.of(
            WireErrorCode.SERIALIZATION_FAILED,
            "HTTP handler must return a response mapping with status and body",
        )
    unknown = set(response) - {"status", "body"}
    if unknown:
        raise S5rError.of(
            WireErrorCode.SERIALIZATION_FAILED,
            f"HTTP response has unknown fields {sorted(unknown)}",
        )
    if "status" not in response or "body" not in response:
        raise S5rError.of(
            WireErrorCode.SERIALIZATION_FAILED,
            "HTTP response requires status and body",
        )
    status = response["status"]
    if not isinstance(status, int) or isinstance(status, bool) or not 100 <= status <= 599:
        raise S5rError.of(
            WireErrorCode.SERIALIZATION_FAILED,
            "extension HTTP status must be between 100 and 599",
        )
    return {"status": status, "body": response["body"]}


def _parse_tool_scope(scope: Any) -> tuple[str, str, str | None, str | None]:
    if not isinstance(scope, Mapping):
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT, "invalid tool invocation: scope must be an object"
        )
    unknown = set(scope) - {"session_id", "working_dir", "turn_id", "tool_call_id"}
    if unknown:
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            f"invalid tool invocation scope: unknown fields {sorted(unknown)}",
        )
    session_id = scope.get("session_id")
    working_dir = scope.get("working_dir")
    turn_id = scope.get("turn_id")
    tool_call_id = scope.get("tool_call_id")
    if not isinstance(session_id, str) or not isinstance(working_dir, str):
        raise S5rError.of(
            WireErrorCode.INVALID_INPUT,
            "invalid tool invocation scope: session_id and working_dir are required strings",
        )
    for label, value in (("turn_id", turn_id), ("tool_call_id", tool_call_id)):
        if value is not None and not isinstance(value, str):
            raise S5rError.of(
                WireErrorCode.INVALID_INPUT,
                f"invalid tool invocation scope: {label} must be a string",
            )
    return session_id, working_dir, turn_id, tool_call_id


def _ensure_handler_result(result: Any) -> HandlerResult:
    if not isinstance(result, HandlerResult):
        raise S5rError.of(
            WireErrorCode.SERIALIZATION_FAILED,
            "handler must return a HandlerResult",
        )
    return result


def _validate_custom_event_subscription(subscription: CustomEventSubscription) -> str | None:
    if not subscription.id or len(subscription.id) > MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN:
        return f"invalid custom event subscription id `{subscription.id}`"
    if subscription.consumer_version == 0:
        return "custom event consumer version must be greater than zero"
    if not subscription.event_type:
        return "custom event subscription type cannot be empty"
    source = subscription.source
    if (
        isinstance(source, Mapping)
        and source.get("kind") == "extension"
        and not source.get("extension_id")
    ):
        return "custom event subscription source extension cannot be empty"
    return None


def _hook_declaration(event: str, mode: str, priority: int | None) -> dict[str, Any]:
    if priority is not None and priority < 0:
        raise S5rError.of(
            WireErrorCode.INVALID_HOOK_REGISTRATION,
            "hook priority must be non-negative",
        )
    declaration: dict[str, Any] = {"on": event, "mode": mode}
    if priority is not None:
        declaration["priority"] = priority
    return declaration


def _failed_stream_event(code: str, message: str) -> dict[str, Any]:
    return {"type": "failed", "error": ErrorPayload(code, message).to_json()}


def _validate_outbound_stream_event(
    event: dict[str, Any], started: bool
) -> tuple[dict[str, Any], bool]:
    # Mirrors Rust `validate_outbound_stream_event`: an ordering violation is
    # replaced by a terminal `failed` event instead of reaching the wire.
    event_type = event["type"]
    if event_type == "started":
        if not started:
            return event, True
        return (
            _failed_stream_event(
                WireErrorCode.INVALID_RESPONSE, "stream started more than once"
            ),
            started,
        )
    if event_type == "failed" or started:
        return event, started
    return (
        _failed_stream_event(
            WireErrorCode.INVALID_RESPONSE, "stream event arrived before started"
        ),
        started,
    )


async def _list_event_iterator(events: Any) -> AsyncIterator[dict[str, Any]]:
    for event in events:
        yield event


def _as_event_iterator(payload: Any) -> AsyncIterator[dict[str, Any]]:
    if hasattr(payload, "__aiter__"):
        return aiter(payload)
    return _list_event_iterator(payload)


class _UnaryPending:
    def __init__(self) -> None:
        self.future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()


class _StreamPending:
    def __init__(self) -> None:
        # Mirrors Rust's two-hop forwarding: routed events land in the forward
        # buffer; a per-stream task forwards them into the consumer buffer
        # bounded by the backpressure deadline.
        self.forward: asyncio.Queue[Any] = asyncio.Queue(
            maxsize=_STREAM_FORWARD_BUFFER_CAPACITY
        )
        self.queue: asyncio.Queue[Any] = asyncio.Queue(maxsize=_STREAM_BUFFER_CAPACITY)
        self.started = False
        self.terminal = False
        self.forward_task: asyncio.Task[None] | None = None


class _Driver:
    """Post-handshake I/O: read loop, FIFO write pump, pending calls, inbound tasks."""

    def __init__(
        self,
        transport: FrameTransport,
        worker: Worker,
        negotiated_features: set[str],
        host_operations: list[str],
    ):
        self._transport = transport
        self._worker = worker
        self._negotiated_features = negotiated_features
        self.host_operations = list(host_operations)
        self._write_queue: asyncio.Queue[dict[str, Any] | None] = asyncio.Queue(
            maxsize=_WRITE_QUEUE_CAPACITY
        )
        self._pending: dict[str, _UnaryPending | _StreamPending] = {}
        self._tombstones: OrderedDict[str, None] = OrderedDict()
        self._inbound: dict[str, tuple[asyncio.Task[None], CancelToken]] = {}
        self._next_request_id = 0
        self._inbound_in_flight = 0
        self._write_error: BaseException | None = None
        self._reader_task: asyncio.Task[None] | None = None
        self._closed = False

    async def run(self) -> None:
        self._reader_task = asyncio.current_task()
        writer = asyncio.create_task(self._write_loop())
        try:
            while True:
                payload = await self._transport.read_frame()
                self._handle_message(decode_message(payload))
        except EOFError:
            if self._write_error is not None:
                raise self._write_error
        except asyncio.CancelledError:
            if self._write_error is not None:
                raise self._write_error
            raise
        finally:
            await self._shutdown(writer)

    # ── outbound calls (used by HostClient bindings and conformance) ────────

    async def invoke(
        self, operation: str, input: Any, *, parent_invoke_id: str | None = None
    ) -> Any:
        self._validate_outbound(operation, parent_invoke_id, stream=False)
        request_id = self._allocate_request_id()
        pending = _UnaryPending()
        self._pending[request_id] = pending
        try:
            await self._write(
                invoke_message(
                    request_id, operation, input, parent_invoke_id=parent_invoke_id
                )
            )
        except BaseException:
            # Mirrors Rust `start_invoke_write`: a rejected write drops the
            # pending entry instead of leaking it.
            self._pending.pop(request_id, None)
            raise
        try:
            return await pending.future
        except asyncio.CancelledError:
            if self._pending.pop(request_id, None) is not None:
                self._tombstone(request_id)
                self._enqueue_best_effort(cancel_message(request_id, "caller_dropped"))
            raise

    def invoke_stream(
        self, operation: str, input: Any, *, parent_invoke_id: str | None = None
    ) -> AsyncIterator[dict[str, Any]]:
        self._validate_outbound(operation, parent_invoke_id, stream=True)
        request_id = self._allocate_request_id()
        pending = _StreamPending()
        self._pending[request_id] = pending
        try:
            self._enqueue(
                invoke_message(
                    request_id,
                    operation,
                    input,
                    stream=True,
                    parent_invoke_id=parent_invoke_id,
                )
            )
        except BaseException:
            self._pending.pop(request_id, None)
            raise
        pending.forward_task = asyncio.create_task(
            self._forward_stream(request_id, pending)
        )
        return self._stream_events(request_id, pending)

    async def _stream_events(
        self, request_id: str, pending: _StreamPending
    ) -> AsyncIterator[dict[str, Any]]:
        try:
            while True:
                item = await pending.queue.get()
                if isinstance(item, S5rError):
                    raise item
                yield item
                if item["type"] in TERMINAL_STREAM_EVENTS:
                    return
        finally:
            if self._pending.pop(request_id, None) is not None:
                self._stop_forward(pending)
                self._tombstone(request_id)
                self._enqueue_best_effort(cancel_message(request_id, "stream_dropped"))

    # ── message routing ─────────────────────────────────────────────────────

    def _handle_message(self, message: Any) -> None:
        if isinstance(message, ResultMsg):
            self._route_result(message)
        elif isinstance(message, StreamMsg):
            self._route_stream(message)
        elif isinstance(message, CancelMsg):
            self._route_cancel(message)
        elif isinstance(message, InvokeMsg):
            self._start_inbound(message)
        else:
            raise ProtocolError("expected runtime invoke, result, stream, or cancel message")

    def _route_result(self, message: ResultMsg) -> None:
        if message.kind != "invoke":
            raise ProtocolError("expected invoke result")
        if message.id in self._tombstones:
            del self._tombstones[message.id]
            return
        pending = self._pending.pop(message.id, None)
        if pending is None:
            raise ProtocolError(f"result references unknown request {message.id}")
        if isinstance(pending, _StreamPending):
            self._fail_stream(
                message.id,
                pending,
                S5rError.of(
                    WireErrorCode.INVALID_RESPONSE,
                    "stream request answered with a unary result",
                ),
                cancel_reason=None,
            )
            return
        if message.is_success:
            pending.future.set_result(message.output)
        else:
            pending.future.set_exception(S5rError(message.error))  # type: ignore[arg-type]

    def _route_stream(self, message: StreamMsg) -> None:
        pending = self._pending.get(message.id)
        if pending is None:
            if message.id in self._tombstones:
                return
            raise ProtocolError(f"stream event references unknown request {message.id}")
        if not isinstance(pending, _StreamPending):
            raise ProtocolError(f"stream event references unary request {message.id}")
        if pending.terminal:
            return
        event = message.event
        event_type = event["type"]
        valid = (
            event_type == "failed"
            or (event_type == "started" and not pending.started)
            or (event_type != "started" and pending.started)
        )
        if not valid:
            self._fail_stream(
                message.id,
                pending,
                S5rError.of(
                    WireErrorCode.INVALID_RESPONSE, "stream event ordering is invalid"
                ),
                cancel_reason="invalid_stream_order",
            )
            return
        if event_type == "started":
            pending.started = True
        try:
            pending.forward.put_nowait(event)
        except asyncio.QueueFull:
            self._fail_stream(
                message.id,
                pending,
                S5rError.of(
                    WireErrorCode.PEER_OVERLOADED, "stream forwarding queue is full"
                ),
                cancel_reason="stream_forward_queue_full",
            )
            return
        if event_type in TERMINAL_STREAM_EVENTS:
            pending.terminal = True
            self._pending.pop(message.id, None)

    async def _forward_stream(
        self, request_id: str, pending: _StreamPending
    ) -> None:
        while True:
            event = await pending.forward.get()
            try:
                await asyncio.wait_for(
                    pending.queue.put(event), _STREAM_BACKPRESSURE_TIMEOUT
                )
            except asyncio.TimeoutError:
                self._fail_stream(
                    request_id,
                    pending,
                    S5rError.of(
                        WireErrorCode.BACKPRESSURE_TIMEOUT,
                        "stream consumer did not release capacity before the"
                        " backpressure deadline",
                    ),
                    cancel_reason="backpressure_timeout",
                )
                return
            if event["type"] in TERMINAL_STREAM_EVENTS:
                return

    def _fail_stream(
        self,
        request_id: str,
        pending: _StreamPending,
        error: S5rError,
        cancel_reason: str | None,
    ) -> None:
        pending.terminal = True
        self._pending.pop(request_id, None)
        self._stop_forward(pending)
        self._enqueue_stream_terminal(pending, error)
        if cancel_reason is not None:
            self._tombstone(request_id)
            self._enqueue_best_effort(cancel_message(request_id, cancel_reason))

    @staticmethod
    def _stop_forward(pending: _StreamPending) -> None:
        task = pending.forward_task
        if task is not None and task is not asyncio.current_task():
            task.cancel()

    @staticmethod
    def _enqueue_stream_terminal(pending: _StreamPending, terminal: S5rError) -> None:
        if pending.queue.full():
            pending.queue.get_nowait()
        pending.queue.put_nowait(terminal)

    def _route_cancel(self, message: CancelMsg) -> None:
        entry = self._inbound.get(message.id)
        if entry is None:
            return
        task, token = entry
        token.cancel(message.reason)
        task.cancel()

    def _start_inbound(self, message: InvokeMsg) -> None:
        if message.id in self._inbound:
            self._enqueue(
                result_failure(
                    message.id,
                    "invoke",
                    ErrorPayload(
                        WireErrorCode.DUPLICATE_REQUEST_ID, "duplicate inbound request id"
                    ),
                )
            )
            return
        error = self._validate_inbound(message)
        if error is not None:
            self._enqueue(result_failure(message.id, "invoke", error))
            return
        if self._inbound_in_flight >= _MAX_IN_FLIGHT_REQUESTS:
            self._enqueue(
                result_failure(
                    message.id,
                    "invoke",
                    ErrorPayload(
                        WireErrorCode.PEER_OVERLOADED,
                        "peer has reached its in-flight request limit",
                    ),
                )
            )
            return
        self._inbound_in_flight += 1
        token = CancelToken()
        task = asyncio.create_task(self._run_inbound(message, token))
        self._inbound[message.id] = (task, token)

    def _validate_inbound(self, message: InvokeMsg) -> ErrorPayload | None:
        return self._negotiation_error(
            message.operation,
            message.parent_invoke_id,
            stream=message.stream,
            parent_table=self._pending,
        )

    async def _run_inbound(self, message: InvokeMsg, token: CancelToken) -> None:
        try:
            await self._serve_inbound(message, token)
        except ProtocolError as error:
            # A rejected write is fatal to the driver, as in Rust's write pump.
            if self._write_error is None:
                self._write_error = error
            if self._reader_task is not None:
                self._reader_task.cancel()
        finally:
            self._inbound_in_flight -= 1

    async def _serve_inbound(self, message: InvokeMsg, token: CancelToken) -> None:
        try:
            if token.is_cancelled():
                return
            kind, payload = await self._worker._dispatch(message, token, self)
        except asyncio.CancelledError:
            return
        except S5rError as error:
            await self._write(result_failure(message.id, "invoke", error.payload))
            return
        except Exception as error:
            await self._write(
                result_failure(
                    message.id,
                    "invoke",
                    ErrorPayload(
                        WireErrorCode.INTERNAL_ERROR, f"worker handler failed: {error}"
                    ),
                )
            )
            return
        finally:
            self._inbound.pop(message.id, None)
        if kind == "stream":
            if not message.stream:
                await self._write(
                    result_failure(
                        message.id,
                        "invoke",
                        ErrorPayload(
                            WireErrorCode.INVALID_RESPONSE,
                            "handler response mode does not match invoke mode",
                        ),
                    )
                )
                return
            await self._write_stream_events(message.id, token, payload)
        elif message.stream:
            await self._write(
                result_failure(
                    message.id,
                    "invoke",
                    ErrorPayload(
                        WireErrorCode.INVALID_RESPONSE,
                        "handler response mode does not match invoke mode",
                    ),
                )
            )
        else:
            await self._write(result_success(message.id, "invoke", payload))

    async def _write_stream_events(
        self, request_id: str, token: CancelToken, payload: Any
    ) -> None:
        # Mirrors Rust `run_inbound`: an idle deadline on the producer, a
        # synthesized terminal failure when the producer ends early, and
        # started-first ordering validation before events reach the wire.
        events = _as_event_iterator(payload)
        started = False
        while True:
            if token.is_cancelled():
                return
            try:
                event = await asyncio.wait_for(anext(events), _STREAM_IDLE_TIMEOUT)
            except StopAsyncIteration:
                event = _failed_stream_event(
                    WireErrorCode.STREAM_CLOSED,
                    "stream producer closed before a terminal event",
                )
            except asyncio.TimeoutError:
                event = _failed_stream_event(
                    WireErrorCode.STREAM_IDLE_TIMEOUT,
                    "stream producer exceeded the idle deadline",
                )
            event, started = _validate_outbound_stream_event(event, started)
            await self._write(stream_message(request_id, event))
            if event["type"] in TERMINAL_STREAM_EVENTS:
                return

    # ── plumbing ────────────────────────────────────────────────────────────

    def _validate_outbound(
        self, operation: str, parent_invoke_id: str | None, *, stream: bool
    ) -> None:
        if self._closed:
            raise S5rError.of(WireErrorCode.PEER_CLOSED, "peer driver is closed")
        error = self._negotiation_error(
            operation,
            parent_invoke_id,
            stream=stream,
            parent_table=self._inbound,
        )
        if error is not None:
            raise S5rError.of(error.code, error.message)

    def _negotiation_error(
        self,
        operation: str,
        parent_invoke_id: str | None,
        *,
        stream: bool,
        parent_table: Mapping[str, Any],
    ) -> ErrorPayload | None:
        if not operation:
            return ErrorPayload(WireErrorCode.INVALID_REQUEST, "operation must not be empty")
        if parent_invoke_id is not None:
            if FEATURE_NESTED_INVOKE_V1 not in self._negotiated_features:
                return ErrorPayload(
                    WireErrorCode.UNSUPPORTED_FEATURE, "nested invoke was not negotiated"
                )
            if parent_invoke_id not in parent_table:
                return ErrorPayload(
                    WireErrorCode.UNKNOWN_PARENT_INVOKE,
                    f"parent invoke {parent_invoke_id} is not active",
                )
        if stream and FEATURE_MODEL_STREAM_V1 not in self._negotiated_features:
            return ErrorPayload(
                WireErrorCode.UNSUPPORTED_FEATURE, "model stream was not negotiated"
            )
        return None

    def _allocate_request_id(self) -> str:
        self._next_request_id += 1
        return f"invoke-{self._next_request_id}"

    def _tombstone(self, request_id: str) -> None:
        self._tombstones[request_id] = None
        self._tombstones.move_to_end(request_id)
        while len(self._tombstones) > _CANCELLED_REQUEST_CAPACITY:
            self._tombstones.popitem(last=False)

    async def _write(self, message: dict[str, Any]) -> None:
        # The write pump is uniformly fail-fast (Rust `WritePump::try_write`):
        # a full queue rejects instead of parking the caller.
        self._enqueue(message)

    def _enqueue(self, message: dict[str, Any]) -> None:
        try:
            self._write_queue.put_nowait(message)
        except asyncio.QueueFull as error:
            raise ProtocolError("peer write queue is full") from error

    def _enqueue_best_effort(self, message: dict[str, Any]) -> None:
        try:
            self._write_queue.put_nowait(message)
        except asyncio.QueueFull:
            pass

    async def _write_loop(self) -> None:
        try:
            while True:
                message = await self._write_queue.get()
                if message is None:
                    return
                await self._transport.write_frame(encode_message(message))
        except BaseException as error:
            self._write_error = error
            if self._reader_task is not None:
                self._reader_task.cancel()

    async def _shutdown(self, writer: asyncio.Task[None]) -> None:
        # Reject new outbound calls first: once the driver stops, a queued
        # invoke would never be answered (Rust's PeerHandle fails closed).
        self._closed = True
        for task, token in self._inbound.values():
            token.cancel("peer_driver_stopped")
            task.cancel()
        if self._inbound:
            await asyncio.gather(
                *(task for task, _ in self._inbound.values()), return_exceptions=True
            )
        self._inbound.clear()
        forward_tasks = []
        for pending in self._pending.values():
            if isinstance(pending, _UnaryPending):
                if not pending.future.done():
                    pending.future.set_exception(
                        S5rError.of(
                            WireErrorCode.PEER_CLOSED,
                            "peer closed before the invoke completed",
                        )
                    )
            else:
                self._stop_forward(pending)
                if pending.forward_task is not None:
                    forward_tasks.append(pending.forward_task)
                self._enqueue_stream_terminal(
                    pending,
                    S5rError.of(
                        WireErrorCode.PEER_CLOSED,
                        "peer closed before the stream completed",
                    ),
                )
        self._pending.clear()
        if forward_tasks:
            await asyncio.gather(*forward_tasks, return_exceptions=True)
        if not writer.done():
            if self._write_queue.full():
                self._write_queue.get_nowait()
            self._write_queue.put_nowait(None)
        await asyncio.gather(writer, return_exceptions=True)
        await self._transport.aclose()
