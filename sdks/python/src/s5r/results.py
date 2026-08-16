"""Handler results, effects, and tool plans.

Mirrors `astrcode_extension_sdk::wire::effects` and
`astrcode_extension_sdk::s5r::tool_plan`.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


class HandlerEffect:
    OK = "ok"
    TOOL_OUTCOME = "tool_outcome"
    TOOL_PLAN = "tool_plan"
    BLOCK = "block"
    ASK = "ask"
    REPLACE_TOOL_INPUT = "replace_tool_input"
    REPLACE_MESSAGES = "replace_messages"
    APPEND_MESSAGES = "append_messages"
    PROVIDER_CONTRIBUTION = "provider_contribution"
    CONTINUE_ONE_STEP = "continue_one_step"
    PROMPT_CONTRIBUTIONS = "prompt_contributions"
    COMPACT_CONTRIBUTIONS = "compact_contributions"
    HTTP_RESPONSE = "http_response"
    CUSTOM_EVENT_ACK = "custom_event_ack"
    CUSTOM_EVENT_RETRY = "custom_event_retry"
    CUSTOM_EVENT_DEAD_LETTER = "custom_event_dead_letter"


class FileOperation:
    READ = "read"
    SEARCH = "search"
    WRITE = "write"
    READ_WRITE = "read_write"


class HostResource:
    PROCESS = "process"
    TOOL_RESULT_ARTIFACT = "tool_result_artifact"
    SESSION = "session"
    MODEL = "model"
    NETWORK = "network"
    EVENT = "event"
    EXTENSION_HTTP = "extension_http"


@dataclass(frozen=True)
class ResourceAccess:
    """One declared resource access in a tool plan."""

    kind: str
    operation: str | None = None
    path: str | None = None
    recursive: bool = False
    resource: str | None = None

    @classmethod
    def file(cls, operation: str, path: str, recursive: bool = False) -> ResourceAccess:
        return cls(kind="file", operation=operation, path=path, recursive=recursive)

    @classmethod
    def read_file(cls, path: str) -> ResourceAccess:
        return cls.file(FileOperation.READ, path)

    @classmethod
    def host(cls, resource: str) -> ResourceAccess:
        return cls(kind="host", resource=resource)

    @classmethod
    def opaque(cls) -> ResourceAccess:
        return cls(kind="opaque")

    def to_json(self) -> dict[str, Any]:
        if self.kind == "file":
            return {
                "kind": "file",
                "operation": self.operation,
                "path": self.path,
                "recursive": self.recursive,
            }
        if self.kind == "host":
            return {"kind": "host", "resource": self.resource}
        return {"kind": "opaque"}


@dataclass(frozen=True)
class ToolPlan:
    resources: tuple[ResourceAccess, ...] = ()

    def __init__(self, resources: list[ResourceAccess] | tuple[ResourceAccess, ...] = ()):
        object.__setattr__(self, "resources", tuple(resources))

    def to_json(self) -> dict[str, Any]:
        return {"resources": [access.to_json() for access in self.resources]}


@dataclass
class HandlerResult:
    """Successful `handler.invoke` output."""

    effect: str
    data: Any = None
    continuations: list[dict[str, Any]] = field(default_factory=list)

    @classmethod
    def ok(cls) -> HandlerResult:
        return cls(effect=HandlerEffect.OK)

    @classmethod
    def of(cls, effect: str, data: Any) -> HandlerResult:
        return cls(effect=effect, data=data)

    def to_json(self) -> dict[str, Any]:
        value: dict[str, Any] = {"effect": self.effect}
        if self.data is not None:
            value["data"] = self.data
        if self.continuations:
            value["continuations"] = self.continuations
        return value


def tool_text(content: str, is_error: bool = False) -> HandlerResult:
    return HandlerResult.of(
        HandlerEffect.TOOL_OUTCOME, {"content": content, "is_error": is_error}
    )


def hook_continuation(on: str, input: Any = None) -> dict[str, Any]:
    return {"call": "hook", "on": on, "input": input}


def tool_continuation(name: str, input: Any = None) -> dict[str, Any]:
    return {"call": "tool", "name": name, "input": input}
