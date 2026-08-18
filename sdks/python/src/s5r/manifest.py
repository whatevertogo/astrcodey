"""Manifest declarations and registration-time validation rules.

Mirrors `astrcode_extension_sdk::wire::manifest` plus the shared registration
rules in `astrcode_extension_sdk::extension::registration_validation`.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


class LifecycleEvent:
    SESSION_START = "session_start"
    SESSION_RESUME = "session_resume"
    SESSION_SHUTDOWN = "session_shutdown"
    TURN_START = "turn_start"
    TURN_END = "turn_end"
    TURN_ABORTED = "turn_aborted"
    STEP_START = "step_start"
    STEP_END = "step_end"
    TOOL_INPUT_TRANSFORM = "tool_input_transform"
    PRE_TOOL_USE = "pre_tool_use"
    POST_TOOL_USE = "post_tool_use"
    BEFORE_PROVIDER_REQUEST = "before_provider_request"
    PROVIDER_CONTRIBUTION = "provider_contribution"
    AFTER_PROVIDER_RESPONSE = "after_provider_response"
    CONTINUE_AFTER_STOP = "continue_after_stop"
    USER_PROMPT_SUBMIT = "user_prompt_submit"
    USER_MESSAGE_ENVELOPE = "user_message_envelope"
    PROMPT_BUILD = "prompt_build"
    POST_RECAP = "post_recap"


ALL_LIFECYCLE_EVENTS = frozenset(
    value for key, value in vars(LifecycleEvent).items() if not key.startswith("_")
)


class CompactEvent:
    PRE_COMPACT = "pre_compact"
    POST_COMPACT = "post_compact"


class HookMode:
    BLOCKING = "blocking"
    NON_BLOCKING = "non_blocking"
    ADVISORY = "advisory"


ALL_HOOK_MODES = frozenset({HookMode.BLOCKING, HookMode.NON_BLOCKING, HookMode.ADVISORY})


class ToolMode:
    PARALLEL = "parallel"
    SEQUENTIAL = "sequential"


class ExtensionCapability:
    SESSION_CONTROL = "session_control"
    SESSION_COMMAND = "session_command"
    SESSION_INSPECT = "session_inspect"
    PUBLIC_HTTP = "public_http"
    AUTHENTICATED_HTTP = "authenticated_http"
    PUBLIC_HTTP_DISPATCH = "public_http_dispatch"
    MAIN_MODEL = "main_model"
    SMALL_MODEL = "small_model"
    SESSION_HISTORY = "session_history"
    EMIT_CUSTOM_EVENTS = "emit_custom_events"
    CONSUME_CUSTOM_EVENTS = "consume_custom_events"
    WORKSPACE_READ = "workspace_read"
    WORKSPACE_WRITE = "workspace_write"
    WORKSPACE_SENSITIVE_PATHS = "workspace_sensitive_paths"
    TOOL_RESULT_READ = "tool_result_read"
    PROCESS_SPAWN = "process_spawn"
    NETWORK_CLIENT = "network_client"
    PROVIDER_REQUEST = "provider_request"
    INPUT_DELIVERY = "input_delivery"
    TOOL_INTERCEPT = "tool_intercept"
    TURN_CONTINUATION_CONTROL = "turn_continuation_control"
    LIVE_CONVERSATION = "live_conversation"


ALL_CAPABILITIES = frozenset(
    value for key, value in vars(ExtensionCapability).items() if not key.startswith("_")
)


class TransportFeature:
    AUTHENTICATED_HTTP = "authenticated_http"


ALL_TRANSPORT_FEATURES = frozenset({TransportFeature.AUTHENTICATED_HTTP})


class CustomEventDelivery:
    SESSION_DURABLE = "session_durable"
    SESSION_LIVE = "session_live"
    GLOBAL_LIVE = "global_live"


ALL_CUSTOM_EVENT_DELIVERIES = frozenset(
    {
        CustomEventDelivery.SESSION_DURABLE,
        CustomEventDelivery.SESSION_LIVE,
        CustomEventDelivery.GLOBAL_LIVE,
    }
)


class CommandAvailability:
    ALL_TRANSPORTS = "all_transports"
    INTERACTIVE_ONLY = "interactive_only"


class ExtensionHttpMethod:
    GET = "GET"
    POST = "POST"
    PUT = "PUT"
    PATCH = "PATCH"
    DELETE = "DELETE"


ALL_EXTENSION_HTTP_METHODS = frozenset(
    value for key, value in vars(ExtensionHttpMethod).items() if not key.startswith("_")
)


class ExtensionHttpAccess:
    PUBLIC = "public"
    AUTHENTICATED = "authenticated"


ALL_EXTENSION_HTTP_ACCESSES = frozenset(
    {ExtensionHttpAccess.PUBLIC, ExtensionHttpAccess.AUTHENTICATED}
)

DEFAULT_EXTENSION_HTTP_BODY_BYTES = 64 * 1024
MAX_EXTENSION_HTTP_BODY_BYTES = 1024 * 1024


# Fixed modes from `fixed_hook_mode`; all other lifecycle events are mode-flexible.
FIXED_HOOK_MODES: dict[str, str] = {
    LifecycleEvent.AFTER_PROVIDER_RESPONSE: HookMode.ADVISORY,
    LifecycleEvent.TOOL_INPUT_TRANSFORM: HookMode.BLOCKING,
    LifecycleEvent.PRE_TOOL_USE: HookMode.BLOCKING,
    LifecycleEvent.PROVIDER_CONTRIBUTION: HookMode.BLOCKING,
    LifecycleEvent.CONTINUE_AFTER_STOP: HookMode.BLOCKING,
    LifecycleEvent.USER_MESSAGE_ENVELOPE: HookMode.BLOCKING,
    LifecycleEvent.PROMPT_BUILD: HookMode.BLOCKING,
}

# Non-fixed events that still permit blocking mode
# (`hook_mode_is_supported` + `lifecycle_event_allows_blocking`).
_BLOCKING_ALLOWED = frozenset(
    {
        LifecycleEvent.POST_TOOL_USE,
        LifecycleEvent.BEFORE_PROVIDER_REQUEST,
        LifecycleEvent.TURN_START,
        LifecycleEvent.USER_PROMPT_SUBMIT,
    }
)


def hook_mode_is_supported(event: str, mode: str) -> bool:
    fixed = FIXED_HOOK_MODES.get(event)
    if fixed is not None:
        return mode == fixed
    return mode != HookMode.BLOCKING or event in _BLOCKING_ALLOWED


@dataclass
class ToolDefinition:
    name: str
    description: str
    parameters: dict[str, Any]
    strict: bool = False
    mode: str = ToolMode.SEQUENTIAL
    timeout_ms: int | None = None

    def to_manifest(self) -> dict[str, Any]:
        manifest = {
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
            "strict": self.strict,
            "mode": self.mode,
        }
        if self.timeout_ms is not None:
            manifest["timeout_ms"] = self.timeout_ms
        return manifest


@dataclass
class SlashCommand:
    """Slash command manifest entry. All fields are wire-required."""

    name: str
    description: str
    args_schema: dict[str, Any] | None = None
    requires_idle: bool = False
    argument_completions: bool = False
    priority: int = 0
    availability: str = CommandAvailability.ALL_TRANSPORTS
    execution: dict[str, Any] = field(default_factory=lambda: {"kind": "extension"})

    def to_manifest(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "description": self.description,
            "args_schema": self.args_schema,
            "requires_idle": self.requires_idle,
            "argument_completions": self.argument_completions,
            "priority": self.priority,
            "availability": self.availability,
            "execution": self.execution,
        }


@dataclass
class CustomEventDeclaration:
    event_type: str
    schema_version: int
    delivery: str
    max_payload_bytes: int

    def to_manifest(self) -> dict[str, Any]:
        return {
            "event_type": self.event_type,
            "schema_version": self.schema_version,
            "delivery": self.delivery,
            "max_payload_bytes": self.max_payload_bytes,
        }


@dataclass
class CustomEventSubscription:
    id: str
    consumer_version: int
    event_type: str
    source: dict[str, Any] = field(default_factory=lambda: {"kind": "any"})

    def to_manifest(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "consumer_version": self.consumer_version,
            "event_type": self.event_type,
            "source": self.source,
        }


@dataclass
class ExtensionHttpRoute:
    """HTTP route manifest entry. All fields are wire-required."""

    method: str
    path: str
    access: str = ExtensionHttpAccess.AUTHENTICATED
    description: str = ""
    max_body_bytes: int = DEFAULT_EXTENSION_HTTP_BODY_BYTES

    @classmethod
    def public(cls, method: str, path: str) -> ExtensionHttpRoute:
        return cls(method=method, path=path, access=ExtensionHttpAccess.PUBLIC)

    @classmethod
    def authenticated(cls, method: str, path: str) -> ExtensionHttpRoute:
        return cls(method=method, path=path, access=ExtensionHttpAccess.AUTHENTICATED)

    def to_manifest(self) -> dict[str, Any]:
        return {
            "method": self.method,
            "path": self.path,
            "access": self.access,
            "description": self.description,
            "max_body_bytes": self.max_body_bytes,
        }


def validate_extension_http_route(route: ExtensionHttpRoute) -> str | None:
    """Registration-time route rules; returns the failure reason or `None`."""
    if route.method not in ALL_EXTENSION_HTTP_METHODS:
        return f"unknown extension HTTP method {route.method!r}"
    if route.access not in ALL_EXTENSION_HTTP_ACCESSES:
        return f"unknown extension HTTP access {route.access!r}"
    if not _valid_extension_http_route_path(route.path):
        return f"invalid extension HTTP route path: {route.path}"
    if not 1 <= route.max_body_bytes <= MAX_EXTENSION_HTTP_BODY_BYTES:
        return (
            "extension HTTP max_body_bytes must be between 1 and"
            f" {MAX_EXTENSION_HTTP_BODY_BYTES}"
        )
    return None


def extension_http_route_patterns_conflict(left: str, right: str) -> bool:
    """Two patterns conflict when they can match the same request path."""
    left_segments = _extension_http_path_segments(left)
    right_segments = _extension_http_path_segments(right)
    return len(left_segments) == len(right_segments) and all(
        left_segment == right_segment
        or _extension_http_param_name(left_segment) is not None
        or _extension_http_param_name(right_segment) is not None
        for left_segment, right_segment in zip(left_segments, right_segments)
    )


def _valid_extension_http_route_path(path: str) -> bool:
    if (
        not path.startswith("/")
        or path.endswith("/")
        or "//" in path
        or ".." in path
    ):
        return False
    params: set[str] = set()
    for segment in path.split("/")[1:]:
        if not segment:
            return False
        starts = segment.startswith("{")
        ends = segment.endswith("}")
        if starts and ends:
            name = segment[1:-1]
            if (
                not name
                or not all(
                    character.isascii() and (character.isalnum() or character == "_")
                    for character in name
                )
                or name in params
            ):
                return False
            params.add(name)
        elif starts or ends or "{" in segment or "}" in segment:
            return False
    return True


def _extension_http_path_segments(path: str) -> list[str]:
    return [segment for segment in path.strip("/").split("/") if segment]


def _extension_http_param_name(segment: str) -> str | None:
    if segment.startswith("{") and segment.endswith("}") and len(segment) > 2:
        return segment[1:-1]
    return None


