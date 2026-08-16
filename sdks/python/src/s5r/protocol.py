"""S5R 3.0 wire messages: strict decode, encode helpers, feature negotiation.

Mirrors `astrcode_extension_sdk::wire::protocol`. Envelopes and nested payloads
are parsed strictly: unknown fields, unknown `type` tags, and malformed values
raise `ProtocolError`. All field names are snake_case.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Mapping, Union

from .errors import ErrorPayload, ProtocolError, S5rError, WireErrorCode

S5R_VERSION = "3.0"

FEATURE_NESTED_INVOKE_V1 = "nested_invoke_v1"
FEATURE_MODEL_STREAM_V1 = "model_stream_v1"
FEATURE_CUSTOM_EVENT_V1 = "custom_event_v1"

CAP_HANDLER_INVOKE = "handler.invoke"
CAP_RUNTIME_PING = "s5r.runtime.ping"

CONFORMANCE_UNARY = "s5r.conformance.unary"
CONFORMANCE_STREAM = "s5r.conformance.stream"
CONFORMANCE_NESTED = "s5r.conformance.nested"
CONFORMANCE_WAIT_FOR_CANCEL = "s5r.conformance.wait_for_cancel"
CONFORMANCE_UNKNOWN_ERROR = "s5r.conformance.unknown_error"
CONFORMANCE_HOST_ECHO = "s5r.conformance.host_echo"

RESULT_KINDS = frozenset({"initialize", "activate", "invoke"})

_MESSAGE_FIELDS: dict[str, frozenset[str]] = {
    "initialize": frozenset(
        {
            "type",
            "id",
            "protocol_version",
            "host",
            "extension_id",
            "supported_features",
            "required_features",
            "host_operations",
        }
    ),
    "activate": frozenset({"type", "id", "config"}),
    "result": frozenset({"type", "status", "id", "kind", "output", "error"}),
    "invoke": frozenset(
        {"type", "id", "operation", "input", "stream", "parent_invoke_id"}
    ),
    "stream": frozenset({"type", "id", "event"}),
    "cancel": frozenset({"type", "id", "reason"}),
}

_STREAM_EVENT_FIELDS: dict[str, frozenset[str]] = {
    "started": frozenset({"type"}),
    "retrying": frozenset({"type", "attempt", "delay_ms"}),
    "recovered": frozenset({"type", "attempt"}),
    "content_delta": frozenset({"type", "content"}),
    "thinking_delta": frozenset({"type", "content"}),
    "tool_call_start": frozenset({"type", "tool_call_id", "name", "arguments"}),
    "tool_call_delta": frozenset({"type", "tool_call_id", "delta"}),
    "tool_call_completed": frozenset({"type", "tool_call_id"}),
    "usage": frozenset({"type", "input_tokens", "output_tokens"}),
    "completed": frozenset({"type", "output"}),
    "failed": frozenset({"type", "error"}),
}

TERMINAL_STREAM_EVENTS = frozenset({"completed", "failed"})


@dataclass
class InitializeMsg:
    id: str
    protocol_version: str
    host_name: str
    host_version: str | None
    extension_id: str
    supported_features: list[str] = field(default_factory=list)
    required_features: list[str] = field(default_factory=list)
    host_operations: list[str] = field(default_factory=list)


@dataclass
class ActivateMsg:
    id: str
    config: Any


@dataclass
class ResultMsg:
    id: str
    kind: str
    output: Any = None
    error: ErrorPayload | None = None

    @property
    def is_success(self) -> bool:
        return self.error is None


@dataclass
class InvokeMsg:
    id: str
    operation: str
    input: Any = None
    stream: bool = False
    parent_invoke_id: str | None = None


@dataclass
class StreamMsg:
    id: str
    event: dict[str, Any]


@dataclass
class CancelMsg:
    id: str
    reason: str


Message = Union[InitializeMsg, ActivateMsg, ResultMsg, InvokeMsg, StreamMsg, CancelMsg]


def encode_message(message: Mapping[str, Any]) -> bytes:
    return json.dumps(message, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def result_success(request_id: str, kind: str, output: Any) -> dict[str, Any]:
    return {
        "type": "result",
        "status": "success",
        "id": request_id,
        "kind": kind,
        "output": output,
    }


def result_failure(request_id: str, kind: str, error: ErrorPayload) -> dict[str, Any]:
    return {
        "type": "result",
        "status": "failure",
        "id": request_id,
        "kind": kind,
        "error": error.to_json(),
    }


def invoke_message(
    request_id: str,
    operation: str,
    input: Any,
    *,
    stream: bool = False,
    parent_invoke_id: str | None = None,
) -> dict[str, Any]:
    message: dict[str, Any] = {
        "type": "invoke",
        "id": request_id,
        "operation": operation,
        "input": input,
        "stream": stream,
    }
    if parent_invoke_id is not None:
        message["parent_invoke_id"] = parent_invoke_id
    return message


def stream_message(request_id: str, event: Mapping[str, Any]) -> dict[str, Any]:
    return {"type": "stream", "id": request_id, "event": dict(event)}


def cancel_message(request_id: str, reason: str) -> dict[str, Any]:
    return {"type": "cancel", "id": request_id, "reason": reason}


def decode_message(payload: bytes) -> Message:
    try:
        value = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProtocolError(f"decode S5R message: {error}") from error
    if not isinstance(value, dict):
        raise ProtocolError("S5R envelope must be a JSON object")
    msg_type = value.get("type")
    allowed = _MESSAGE_FIELDS.get(msg_type if isinstance(msg_type, str) else "")
    if allowed is None:
        raise ProtocolError(f"unknown S5R message type {msg_type!r}")
    unknown = set(value) - allowed
    if unknown:
        raise ProtocolError(f"{msg_type} message has unknown fields: {sorted(unknown)}")
    if msg_type == "initialize":
        return _decode_initialize(value)
    if msg_type == "activate":
        return ActivateMsg(id=_required_str(value, "id", "activate"), config=value.get("config"))
    if msg_type == "result":
        return _decode_result(value)
    if msg_type == "invoke":
        return _decode_invoke(value)
    if msg_type == "stream":
        request_id = _required_str(value, "id", "stream")
        event = value.get("event")
        validate_stream_event(event)
        return StreamMsg(id=request_id, event=event)
    return CancelMsg(
        id=_required_str(value, "id", "cancel"),
        reason=_required_str(value, "reason", "cancel"),
    )


def validate_stream_event(event: Any) -> None:
    if not isinstance(event, dict):
        raise ProtocolError("stream event must be an object")
    event_type = event.get("type")
    allowed = _STREAM_EVENT_FIELDS.get(event_type if isinstance(event_type, str) else "")
    if allowed is None:
        raise ProtocolError(f"unknown stream event type {event_type!r}")
    unknown = set(event) - allowed
    if unknown:
        raise ProtocolError(
            f"{event_type} stream event has unknown fields: {sorted(unknown)}"
        )
    missing = allowed - {"type"} - set(event)
    if missing:
        raise ProtocolError(
            f"{event_type} stream event is missing fields: {sorted(missing)}"
        )
    if event_type == "failed":
        ErrorPayload.from_json(event["error"])


def valid_feature_name(value: str) -> bool:
    return (
        0 < len(value) <= 64
        and all(c.isascii() and (c.islower() or c.isdigit()) or c == "_" for c in value)
    )


def negotiate_features(
    local_supported: set[str],
    remote_supported: list[str],
    remote_required: list[str],
) -> set[str]:
    """Intersection semantics shared by both peers; raises `S5rError` on failure."""
    if len(set(remote_supported)) != len(remote_supported):
        raise S5rError.of(
            WireErrorCode.INVALID_REQUEST, "supported_features contains duplicate values"
        )
    if len(set(remote_required)) != len(remote_required):
        raise S5rError.of(
            WireErrorCode.INVALID_REQUEST, "required_features contains duplicate values"
        )
    remote_supported_set = set(remote_supported)
    for feature in remote_required:
        if feature not in remote_supported_set:
            raise S5rError.of(
                WireErrorCode.INVALID_REQUEST,
                f"required feature {feature} is not declared as supported",
            )
        if feature not in local_supported:
            raise S5rError.of(
                WireErrorCode.UNSUPPORTED_FEATURE,
                f"required feature {feature} is not supported by this peer",
            )
    return local_supported & remote_supported_set


def _required_str(value: Mapping[str, Any], key: str, what: str) -> str:
    field_value = value.get(key)
    if not isinstance(field_value, str):
        raise ProtocolError(f"{what} message requires string field {key!r}")
    return field_value


def _decode_initialize(value: Mapping[str, Any]) -> InitializeMsg:
    host = value.get("host")
    if not isinstance(host, dict):
        raise ProtocolError("initialize message requires object field 'host'")
    unknown_host = set(host) - {"name", "version"}
    if unknown_host:
        raise ProtocolError(f"host peer info has unknown fields: {sorted(unknown_host)}")
    host_name = host.get("name")
    host_version = host.get("version")
    if not isinstance(host_name, str):
        raise ProtocolError("host peer info requires string field 'name'")
    if host_version is not None and not isinstance(host_version, str):
        raise ProtocolError("host peer info version must be a string")
    return InitializeMsg(
        id=_required_str(value, "id", "initialize"),
        protocol_version=_required_str(value, "protocol_version", "initialize"),
        host_name=host_name,
        host_version=host_version,
        extension_id=_required_str(value, "extension_id", "initialize"),
        supported_features=_decode_features(value.get("supported_features", [])),
        required_features=_decode_features(value.get("required_features", [])),
        host_operations=_decode_host_operations(value.get("host_operations", [])),
    )


def _decode_features(value: Any) -> list[str]:
    if not isinstance(value, list):
        raise ProtocolError("feature lists must be arrays")
    for feature in value:
        if not isinstance(feature, str) or not valid_feature_name(feature):
            raise ProtocolError(f"invalid feature name {feature!r}")
    return list(value)


def _decode_host_operations(value: Any) -> list[str]:
    if not isinstance(value, list) or not all(isinstance(op, str) for op in value):
        raise ProtocolError("host_operations must be an array of strings")
    return list(value)


def _decode_result(value: Mapping[str, Any]) -> ResultMsg:
    request_id = _required_str(value, "id", "result")
    kind = _required_str(value, "kind", "result")
    if kind not in RESULT_KINDS:
        raise ProtocolError(f"unknown result kind {kind!r}")
    status = value.get("status")
    if status == "success":
        if "output" not in value:
            raise ProtocolError("successful result must carry output")
        if "error" in value:
            raise ProtocolError("successful result must not carry error")
        return ResultMsg(id=request_id, kind=kind, output=value["output"])
    if status == "failure":
        if "error" not in value:
            raise ProtocolError("failed result must carry error")
        if "output" in value:
            raise ProtocolError("failed result must not carry output")
        return ResultMsg(
            id=request_id, kind=kind, error=ErrorPayload.from_json(value["error"])
        )
    raise ProtocolError(f"unknown result status {status!r}")


def _decode_invoke(value: Mapping[str, Any]) -> InvokeMsg:
    stream = value.get("stream", False)
    parent = value.get("parent_invoke_id")
    if not isinstance(stream, bool):
        raise ProtocolError("invoke stream flag must be a boolean")
    if parent is not None and not isinstance(parent, str):
        raise ProtocolError("invoke parent_invoke_id must be a string")
    return InvokeMsg(
        id=_required_str(value, "id", "invoke"),
        operation=_required_str(value, "operation", "invoke"),
        input=value.get("input"),
        stream=stream,
        parent_invoke_id=parent,
    )
