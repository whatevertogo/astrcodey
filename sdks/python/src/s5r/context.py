"""Invocation contexts handed to worker handlers.

Mirrors `astrcode_extension_worker::worker::registry` context types and the
`WorkerCallFacts::from_event` extraction rules.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Any, Mapping

from .errors import ErrorPayload, S5rError, WireErrorCode


class CancelToken:
    """Cancellation state for one inbound invocation.

    The first cancel reason wins; later cleanup never overwrites it.
    """

    def __init__(self) -> None:
        self._event = asyncio.Event()
        self._reason: str | None = None

    def is_cancelled(self) -> bool:
        return self._event.is_set()

    @property
    def reason(self) -> str | None:
        return self._reason

    async def wait_cancelled(self) -> None:
        await self._event.wait()

    def cancel(self, reason: str) -> None:
        if self._reason is None:
            self._reason = reason
        self._event.set()


@dataclass(frozen=True)
class WorkerCallContext:
    """Facts shared by worker calls without session/workspace scope."""

    extension_id: str
    cancel_token: CancelToken


@dataclass(frozen=True)
class WorkerInvocationContext:
    """Host-attributed facts guaranteed for a session/workspace invocation."""

    extension_id: str
    session_id: str
    working_dir: str
    turn_id: str | None
    tool_call_id: str | None
    cancel_token: CancelToken

    async def defer_context(self, content: str) -> Any:
        """Defer `content` into this invocation's own session (next step boundary).

        Mirrors `WorkerInvocationContext::defer_context` in the Rust worker.
        Raises `S5rError` with `no_active_turn` when the session has no active
        turn.
        """
        from .host import HostClient

        return await HostClient.session_control().defer_context(
            {"target_session_id": self.session_id, "content": content}
        )


@dataclass(frozen=True)
class WorkerToolPlanContext:
    """Side-effect-free facts exposed to a tool planner."""

    extension_id: str
    session_id: str
    working_dir: str
    turn_id: str | None
    tool_call_id: str | None
    cancel_token: CancelToken


@dataclass(frozen=True)
class WorkerCommandInvocation:
    """`execute` or `complete(cursor)` — mirrors `WorkerCommandInvocation`."""

    kind: str
    cursor: int | None = None

    @classmethod
    def execute(cls) -> WorkerCommandInvocation:
        return cls(kind="execute")

    @classmethod
    def complete(cls, cursor: int) -> WorkerCommandInvocation:
        return cls(kind="complete", cursor=cursor)


@dataclass(frozen=True)
class WorkerCommandContext:
    """Host-attributed facts guaranteed for a command invocation."""

    extension_id: str
    session_id: str
    working_dir: str
    command_name: str
    argument: str
    model: dict[str, Any]
    invocation: WorkerCommandInvocation
    cancel_token: CancelToken


@dataclass(frozen=True)
class WorkerCustomEventContext:
    extension_id: str
    session_id: str
    turn_id: str | None
    cancel_token: CancelToken


@dataclass(frozen=True)
class _CallFacts:
    session_id: str | None
    turn_id: str | None
    tool_call_id: str | None
    working_dir: str | None

    @classmethod
    def from_event(cls, event: Any) -> _CallFacts:
        source = event
        if isinstance(event, Mapping):
            if "input" in event:
                source = event["input"]
            elif "scope" in event:
                source = event["scope"]
        if not isinstance(source, Mapping):
            return cls(None, None, None, None)
        return cls(
            session_id=_optional_string(source, "session_id"),
            turn_id=_optional_string(source, "turn_id"),
            tool_call_id=_optional_string(source, "tool_call_id"),
            working_dir=_optional_string(source, "working_dir"),
        )

    def require(self, handler_kind: str, field_name: str) -> str:
        value = getattr(self, field_name)
        if value is None:
            raise S5rError(
                ErrorPayload(
                    WireErrorCode.CONTEXT_UNAVAILABLE,
                    f"worker {handler_kind} call requires {field_name}",
                )
            )
        return value


def _optional_string(source: Mapping[str, Any], field_name: str) -> str | None:
    value = source.get(field_name)
    if value is None:
        return None
    if not isinstance(value, str):
        raise S5rError(
            ErrorPayload(
                WireErrorCode.INVALID_INPUT,
                f"{field_name} must be a string when present",
            )
        )
    return value
