"""S5R 3.0 wire errors.

`WireErrorCode` mirrors the single-point catalog in
`astrcode_extension_sdk::wire::WireErrorCode`. Wire strings are permanent and
never reused; unknown codes are preserved losslessly as plain strings in
`ErrorPayload.code`.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


class WireErrorCode:
    PERMISSION_DENIED = "permission_denied"
    BACKEND_UNAVAILABLE = "backend_unavailable"
    CONTEXT_UNAVAILABLE = "context_unavailable"
    INVALID_INPUT = "invalid_input"
    CANCELLED = "cancelled"
    TIMEOUT = "timeout"
    HOST_NOT_READY = "host_not_ready"
    PEER_BUSY = "peer_busy"
    PEER_CLOSED = "peer_closed"
    TRANSPORT = "transport_error"
    IO_ERROR = "io_error"
    UNKNOWN_CAPABILITY = "unknown_capability"
    STATE_TOO_LARGE = "state_too_large"
    SERIALIZATION_FAILED = "serialization_failed"
    INVALID_RESPONSE = "invalid_host_response"
    UNSUPPORTED = "unsupported"
    HOST_RUNTIME_FAILED = "host_runtime_failed"
    EXTENSION_DRAINING = "extension_draining"
    UNSUPPORTED_FEATURE = "unsupported_feature"
    NETWORK_REQUEST_FAILED = "network_request_failed"
    RESPONSE_TOO_LARGE = "response_too_large"
    SESSION_NOT_FOUND = "session_not_found"
    SESSION_BUSY = "session_busy"
    NO_ACTIVE_TURN = "no_active_turn"
    SESSION_ALREADY_EXISTS = "session_already_exists"
    MAX_DEPTH_EXCEEDED = "max_depth_exceeded"
    INTERNAL_ERROR = "internal_error"
    INVALID_REQUEST = "invalid_request"
    DUPLICATE_REQUEST_ID = "duplicate_request_id"
    READ_FAILED = "read_failed"
    STALE_FILE = "stale_file"
    FILE_TOO_LARGE = "file_too_large"
    PROCESS_FAILED = "process_failed"
    SPAWN_FAILED = "spawn_failed"
    STDIN_FAILED = "stdin_failed"
    STDOUT_FAILED = "stdout_failed"
    STDERR_FAILED = "stderr_failed"
    INVALID_API_KEY = "invalid_api_key"
    MODEL_NOT_FOUND = "model_not_found"
    QUOTA_EXCEEDED = "quota_exceeded"
    CONTEXT_WINDOW_EXCEEDED = "context_window_exceeded"
    RATE_LIMITED = "rate_limited"
    CLIENT_ERROR = "client_error"
    SERVER_ERROR = "server_error"
    STREAM_DISCONNECTED = "stream_disconnected"
    STREAM_PARSE = "stream_parse"
    CONTENT_FILTERED = "content_filtered"
    TOKEN_LIMIT = "token_limit"
    EMPTY_RESPONSE = "empty_response"
    LLM_STREAM_ERROR = "llm_stream_error"
    EMIT_FAILED = "emit_failed"
    DISPATCH_FAILED = "dispatch_failed"
    UNKNOWN_PARENT_INVOKE = "unknown_parent_invoke"
    REENTRANCY_EXCEEDED = "reentrancy_exceeded"
    UNSUPPORTED_PROTOCOL_VERSION = "unsupported_protocol_version"
    STREAM_NOT_SUPPORTED = "stream_not_supported"
    STREAM_CLOSED = "stream_closed"
    BACKPRESSURE_TIMEOUT = "backpressure_timeout"
    STREAM_IDLE_TIMEOUT = "stream_idle_timeout"
    UNKNOWN_HANDLER = "unknown_handler"
    DUPLICATE_REGISTRATION = "duplicate_registration"
    UNSUPPORTED_HOOK = "unsupported_hook"
    TYPED_HOOK_REQUIRED = "typed_hook_required"
    INVALID_HOOK_MODE = "invalid_hook_mode"
    INVALID_HOOK_REGISTRATION = "invalid_hook_registration"
    INVALID_HTTP_ROUTE = "invalid_http_route"
    NESTED_FAILED = "nested_failed"
    PEER_OVERLOADED = "peer_overloaded"
    INVALID_CAPABILITY_REGISTRY = "invalid_capability_registry"
    STORAGE_IO_ERROR = "storage_io_error"
    STORAGE_LOCK_ERROR = "storage_lock_error"
    CORRUPT_SESSION_DATA = "corrupt_session_data"


_ERROR_PAYLOAD_FIELDS = frozenset({"code", "message", "hint", "retryable", "details"})


@dataclass
class ErrorPayload:
    """Structured wire error. Unknown `code` strings round-trip losslessly."""

    code: str
    message: str
    hint: str | None = None
    retryable: bool = False
    details: Any = None

    def to_json(self) -> dict[str, Any]:
        data: dict[str, Any] = {
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
        }
        if self.hint is not None:
            data["hint"] = self.hint
        if self.details is not None:
            data["details"] = self.details
        return data

    @classmethod
    def from_json(cls, value: Any) -> ErrorPayload:
        """Strict decode, mirroring `#[serde(deny_unknown_fields)]`."""
        if not isinstance(value, dict):
            raise ProtocolError("error payload must be an object")
        unknown = set(value) - _ERROR_PAYLOAD_FIELDS
        if unknown:
            raise ProtocolError(f"error payload has unknown fields: {sorted(unknown)}")
        code = value.get("code")
        message = value.get("message")
        if not isinstance(code, str) or not isinstance(message, str):
            raise ProtocolError("error payload requires string code and message")
        hint = value.get("hint")
        retryable = value.get("retryable", False)
        if hint is not None and not isinstance(hint, str):
            raise ProtocolError("error payload hint must be a string")
        if not isinstance(retryable, bool):
            raise ProtocolError("error payload retryable must be a boolean")
        return cls(
            code=code,
            message=message,
            hint=hint,
            retryable=retryable,
            details=value.get("details"),
        )


class S5rError(Exception):
    """An error carrying a structured S5R [`ErrorPayload`].

    Handlers raise this to fail an invocation; `HostClient` raises it for
    remote host failures (the payload is the host's, unknown codes preserved).
    """

    def __init__(self, payload: ErrorPayload):
        super().__init__(f"{payload.code}: {payload.message}")
        self.payload = payload

    @classmethod
    def of(cls, code: str, message: str) -> S5rError:
        return cls(ErrorPayload(code, message))

    @property
    def code(self) -> str:
        return self.payload.code


class ProtocolError(Exception):
    """Local protocol violation: the peer sent something invalid. Fatal."""


class FrameError(Exception):
    """Framing violation (bad header, oversized frame). Fatal."""
