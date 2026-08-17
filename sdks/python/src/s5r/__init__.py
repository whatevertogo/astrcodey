"""astrcode S5R 3.0 extension SDK (Python, stdlib only).

Quick start::

    from s5r import HandlerResult, ToolDefinition, Worker, tool_text

    worker = Worker("my-extension", "0.1.0")

    @worker.tool(ToolDefinition(name="echo", description="Echo input", parameters={
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
    }))
    async def echo(arguments, ctx):
        return tool_text(str(arguments["text"]))

    worker.run_stdio()
"""

from .context import (
    CancelToken,
    WorkerCallContext,
    WorkerCommandContext,
    WorkerCommandInvocation,
    WorkerCustomEventContext,
    WorkerInvocationContext,
    WorkerToolPlanContext,
)
from .errors import ErrorPayload, FrameError, ProtocolError, S5rError, WireErrorCode
from .frames import (
    MAX_FRAME_BYTES,
    MAX_FRAME_HEADER_BYTES,
    FrameTransport,
    StdioTransport,
    encode_frame,
    parse_frame_header,
)
from .host import BackgroundHost, BackgroundRootSessionClient, HostClient, HostOperation
from .manifest import (
    CommandAvailability,
    CompactEvent,
    CustomEventDeclaration,
    CustomEventDelivery,
    CustomEventSubscription,
    ExtensionCapability,
    ExtensionHttpAccess,
    ExtensionHttpMethod,
    ExtensionHttpRoute,
    HookMode,
    LifecycleEvent,
    SlashCommand,
    ToolDefinition,
    ToolMode,
    TransportFeature,
)
from .parsing import parse_hook_input, parse_tool_arguments
from .protocol import (
    CAP_HANDLER_INVOKE,
    CAP_RUNTIME_PING,
    FEATURE_CUSTOM_EVENT_V1,
    FEATURE_MODEL_STREAM_V1,
    FEATURE_NESTED_INVOKE_V1,
    S5R_VERSION,
)
from .results import (
    FileOperation,
    HandlerEffect,
    HandlerResult,
    HostResource,
    ResourceAccess,
    ToolPlan,
    tool_text,
)
from .worker import Worker

__version__ = "0.1.0"

__all__ = [
    "CAP_HANDLER_INVOKE",
    "CAP_RUNTIME_PING",
    "BackgroundHost",
    "BackgroundRootSessionClient",
    "CancelToken",
    "CommandAvailability",
    "CompactEvent",
    "CustomEventDeclaration",
    "CustomEventDelivery",
    "CustomEventSubscription",
    "ErrorPayload",
    "ExtensionCapability",
    "ExtensionHttpAccess",
    "ExtensionHttpMethod",
    "ExtensionHttpRoute",
    "FEATURE_CUSTOM_EVENT_V1",
    "FEATURE_MODEL_STREAM_V1",
    "FEATURE_NESTED_INVOKE_V1",
    "FileOperation",
    "FrameError",
    "FrameTransport",
    "HandlerEffect",
    "HandlerResult",
    "HostClient",
    "HostOperation",
    "HostResource",
    "HookMode",
    "LifecycleEvent",
    "MAX_FRAME_BYTES",
    "MAX_FRAME_HEADER_BYTES",
    "ProtocolError",
    "ResourceAccess",
    "S5R_VERSION",
    "S5rError",
    "SlashCommand",
    "StdioTransport",
    "ToolDefinition",
    "ToolMode",
    "ToolPlan",
    "TransportFeature",
    "WireErrorCode",
    "Worker",
    "WorkerCallContext",
    "WorkerCommandContext",
    "WorkerCommandInvocation",
    "WorkerCustomEventContext",
    "WorkerInvocationContext",
    "WorkerToolPlanContext",
    "encode_frame",
    "parse_frame_header",
    "parse_hook_input",
    "parse_tool_arguments",
    "tool_text",
]
