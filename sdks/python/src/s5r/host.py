"""Worker-side host API: typed domain clients over `astrcode.*` operations.

Mirrors `astrcode_extension_worker::worker::host`. The host API is bound to the
currently-handled invocation via a `contextvars.ContextVar` (the analogue of
Rust's task-local `with_host_api`); calling it outside a handler raises
`S5rError` with `context_unavailable`.

Request/response payloads are the shared wire DTOs as plain JSON mappings
(see `crates/astrcode-extension-sdk/src/wire/host/`); this SDK stays thin and
does not re-declare them as Python types.
"""

from __future__ import annotations

from contextvars import ContextVar
from dataclasses import dataclass
from typing import Any, AsyncIterator, Awaitable, Callable, Mapping

from .errors import ErrorPayload, S5rError, WireErrorCode


class HostOperation:
    """Stable wire names of every host operation (the handshake catalog)."""

    EVENT_EMIT = "astrcode.event.emit"
    EXTENSION_HTTP_PUBLIC = "astrcode.extension.http.public"
    LLM_MAIN_CHAT = "astrcode.llm.main_chat"
    LLM_SMALL_CHAT = "astrcode.llm.small_chat"
    NETWORK_CLIENT = "astrcode.network.client"
    PROCESS_SPAWN = "astrcode.process.spawn"
    PROCESS_START = "astrcode.process.start"
    PROCESS_READ = "astrcode.process.read"
    PROCESS_INPUT = "astrcode.process.input"
    PROCESS_STATUS = "astrcode.process.status"
    PROCESS_PROMOTE = "astrcode.process.promote"
    PROCESS_KILL = "astrcode.process.kill"
    PROCESS_LIST = "astrcode.process.list"
    SESSION_CONTROL_CANCEL_TURN = "astrcode.session.control.cancel_turn"
    SESSION_CONTROL_CONFIGURE_TOOLS = "astrcode.session.control.configure_tools"
    SESSION_CONTROL_CREATE = "astrcode.session.control.create"
    SESSION_CONTROL_DISPOSE = "astrcode.session.control.dispose"
    SESSION_CONTROL_EXECUTION_VIEW = "astrcode.session.control.execution_view"
    SESSION_CONTROL_INJECT_OR_START = "astrcode.session.control.inject_or_start"
    SESSION_CONTROL_INTERRUPT_AND_SUBMIT = "astrcode.session.control.interrupt_and_submit"
    SESSION_CONTROL_QUEUE_OR_START = "astrcode.session.control.queue_or_start"
    SESSION_CONTROL_DEFER_CONTEXT = "astrcode.session.control.defer_context"
    SESSION_CONTROL_REACTIVATE = "astrcode.session.control.reactivate"
    SESSION_CONTROL_STATE = "astrcode.session.control.state"
    SESSION_CONTROL_SUBMIT_TURN = "astrcode.session.control.submit_turn"
    SESSION_HISTORY_LIST = "astrcode.session.history.list"
    SESSION_HISTORY_PROVIDER_MESSAGES = "astrcode.session.history.provider_messages"
    SESSION_HISTORY_SNAPSHOT = "astrcode.session.history.snapshot"
    SESSION_HISTORY_TOKEN_USAGE = "astrcode.session.history.token_usage"
    SESSION_HISTORY_TRANSCRIPT = "astrcode.session.history.transcript"
    SESSION_INSPECT_LIST = "astrcode.session.inspect.list"
    SESSION_INSPECT_PROVIDER_MESSAGES = "astrcode.session.inspect.provider_messages"
    SESSION_INSPECT_READ_MODEL = "astrcode.session.inspect.read_model"
    SESSION_INSPECT_SNAPSHOT = "astrcode.session.inspect.snapshot"
    SESSION_READ_EVENTS = "astrcode.session.read_events"
    SESSION_ROOT_CREATE = "astrcode.session.root.create"
    SESSION_ROOT_STATE = "astrcode.session.root.state"
    SESSION_ROOT_SUBMIT_TURN = "astrcode.session.root.submit_turn"
    SESSION_STATE_READ = "astrcode.session.state.read"
    SESSION_STATE_WRITE = "astrcode.session.state.write"
    TOOL_RESULT_READ = "astrcode.tool_result.read"
    WORKSPACE_APPLY_PATCH = "astrcode.workspace.apply_patch"
    WORKSPACE_EDIT = "astrcode.workspace.edit"
    WORKSPACE_GLOB = "astrcode.workspace.glob"
    WORKSPACE_GREP = "astrcode.workspace.grep"
    WORKSPACE_LIST = "astrcode.workspace.list"
    WORKSPACE_READ = "astrcode.workspace.read"
    WORKSPACE_WRITE = "astrcode.workspace.write"


@dataclass
class _HostBinding:
    """Task-scoped host API installed while one inbound invocation runs."""

    invoke: Callable[[str, Any], Awaitable[Any]]
    invoke_stream: Callable[[str, Any], AsyncIterator[dict[str, Any]]]
    host_operations: frozenset[str]

    def supports(self, operation: str) -> bool:
        return operation in self.host_operations


_current_binding: ContextVar[_HostBinding | None] = ContextVar(
    "s5r_host_binding", default=None
)


def _binding() -> _HostBinding:
    binding = _current_binding.get()
    if binding is None:
        raise S5rError(
            ErrorPayload(
                WireErrorCode.CONTEXT_UNAVAILABLE,
                "host API is only available while handling an invocation",
            )
        )
    return binding


def _supported_binding(operation: str) -> _HostBinding:
    binding = _binding()
    if not binding.supports(operation):
        raise S5rError(
            ErrorPayload(
                WireErrorCode.UNSUPPORTED,
                f"host does not support operation {operation}",
            )
        )
    return binding


async def _call(operation: str, request: Mapping[str, Any] | None) -> Any:
    binding = _supported_binding(operation)
    return await binding.invoke(operation, dict(request) if request is not None else {})


def _open_stream(
    operation: str, request: Mapping[str, Any] | None
) -> AsyncIterator[dict[str, Any]]:
    binding = _supported_binding(operation)
    return binding.invoke_stream(operation, dict(request) if request is not None else {})


async def _collect(stream: AsyncIterator[dict[str, Any]]) -> Any:
    output = None
    async for event in stream:
        event_type = event.get("type")
        if event_type == "completed":
            output = event.get("output")
        elif event_type == "failed":
            raise S5rError(ErrorPayload.from_json(event["error"]))
    return output


class EventClient:
    async def emit(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.EVENT_EMIT, request)


class ModelClient:
    async def main_chat(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.LLM_MAIN_CHAT, request)

    async def small_chat(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.LLM_SMALL_CHAT, request)

    def main_chat_events(self, request: Mapping[str, Any]) -> AsyncIterator[dict[str, Any]]:
        return _open_stream(HostOperation.LLM_MAIN_CHAT, request)

    def small_chat_events(self, request: Mapping[str, Any]) -> AsyncIterator[dict[str, Any]]:
        return _open_stream(HostOperation.LLM_SMALL_CHAT, request)

    async def main_chat_collected(self, request: Mapping[str, Any]) -> Any:
        return await _collect(self.main_chat_events(request))

    async def small_chat_collected(self, request: Mapping[str, Any]) -> Any:
        return await _collect(self.small_chat_events(request))


class SessionControlClient:
    async def create_root(self) -> Any:
        return await _call(HostOperation.SESSION_ROOT_CREATE, None)

    async def submit_root_turn(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_ROOT_SUBMIT_TURN, request)

    async def root_state(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_ROOT_STATE, request)

    async def inject_or_start(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_INJECT_OR_START, request)

    async def interrupt_and_submit(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_INTERRUPT_AND_SUBMIT, request)

    async def queue_or_start(self, request: Mapping[str, Any]) -> Any:
        """Queue input FIFO behind the active turn, or start a turn when idle."""
        return await _call(HostOperation.SESSION_CONTROL_QUEUE_OR_START, request)

    async def defer_context(self, request: Mapping[str, Any]) -> Any:
        """Inject input at the next step boundary of the active turn.

        Fails with `no_active_turn` when the target session has no active turn.
        """
        return await _call(HostOperation.SESSION_CONTROL_DEFER_CONTEXT, request)

    async def cancel_turn(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_CANCEL_TURN, request)

    async def execution_view(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_EXECUTION_VIEW, request)

    async def state(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_STATE, request)

    async def reactivate(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_REACTIVATE, request)

    async def create_child(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_CREATE, request)

    async def submit_turn(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_SUBMIT_TURN, request)

    async def configure_tools(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_CONFIGURE_TOOLS, request)

    async def recycle(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_CONTROL_DISPOSE, request)


class SessionHistoryClient:
    async def list_summaries(self) -> Any:
        return await _call(HostOperation.SESSION_HISTORY_LIST, None)

    async def transcript(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_HISTORY_TRANSCRIPT, request)

    async def provider_messages(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_HISTORY_PROVIDER_MESSAGES, request)

    async def token_usage(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_HISTORY_TOKEN_USAGE, request)

    async def snapshot(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_HISTORY_SNAPSHOT, request)

    async def events_page(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_READ_EVENTS, request)


class SessionStateClient:
    async def read(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_STATE_READ, request)

    async def write(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_STATE_WRITE, request)


class SessionInspectClient:
    async def list(self) -> Any:
        return await _call(HostOperation.SESSION_INSPECT_LIST, None)

    async def snapshot(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_INSPECT_SNAPSHOT, request)

    async def read_model(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_INSPECT_READ_MODEL, request)

    async def provider_messages(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.SESSION_INSPECT_PROVIDER_MESSAGES, request)


class WorkspaceClient:
    async def read(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.WORKSPACE_READ, request)

    async def write(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.WORKSPACE_WRITE, request)

    async def edit(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.WORKSPACE_EDIT, request)

    async def list(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.WORKSPACE_LIST, request)

    async def grep(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.WORKSPACE_GREP, request)

    async def glob(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.WORKSPACE_GLOB, request)

    async def apply_patch(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.WORKSPACE_APPLY_PATCH, request)


class ToolResultClient:
    async def read(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.TOOL_RESULT_READ, request)


class ProcessClient:
    async def spawn(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.PROCESS_SPAWN, request)

    async def start(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.PROCESS_START, request)

    async def read(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.PROCESS_READ, request)

    async def write(self, process_id: str, data: str) -> Any:
        return await _call(
            HostOperation.PROCESS_INPUT,
            {"id": process_id, "action": {"kind": "write", "input": data}},
        )

    async def close_stdin(self, process_id: str) -> Any:
        return await _call(
            HostOperation.PROCESS_INPUT,
            {"id": process_id, "action": {"kind": "close"}},
        )

    async def status(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.PROCESS_STATUS, request)

    async def promote(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.PROCESS_PROMOTE, request)

    async def kill(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.PROCESS_KILL, request)

    async def list(self) -> Any:
        return await _call(HostOperation.PROCESS_LIST, None)


class NetworkClient:
    async def send(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.NETWORK_CLIENT, request)


class ExtensionHttpClient:
    async def dispatch_public(self, request: Mapping[str, Any]) -> Any:
        return await _call(HostOperation.EXTENSION_HTTP_PUBLIC, request)


_EVENTS = EventClient()
_MODELS = ModelClient()
_SESSION_CONTROL = SessionControlClient()
_SESSION_HISTORY = SessionHistoryClient()
_SESSION_STATE = SessionStateClient()
_SESSION_INSPECT = SessionInspectClient()
_WORKSPACE = WorkspaceClient()
_TOOL_RESULTS = ToolResultClient()
_PROCESS = ProcessClient()
_NETWORK = NetworkClient()
_EXTENSION_HTTP = ExtensionHttpClient()


class HostClient:
    """Worker-side entry point for typed host domains.

    Only usable inside a running handler invocation; see module docstring.
    """

    @staticmethod
    def host_supports(operation: str) -> bool:
        return _binding().supports(operation)

    @staticmethod
    def events() -> EventClient:
        return _EVENTS

    @staticmethod
    def models() -> ModelClient:
        return _MODELS

    @staticmethod
    def session_control() -> SessionControlClient:
        return _SESSION_CONTROL

    @staticmethod
    def session_history() -> SessionHistoryClient:
        return _SESSION_HISTORY

    @staticmethod
    def session_state() -> SessionStateClient:
        return _SESSION_STATE

    @staticmethod
    def session_inspect() -> SessionInspectClient:
        return _SESSION_INSPECT

    @staticmethod
    def workspace() -> WorkspaceClient:
        return _WORKSPACE

    @staticmethod
    def tool_results() -> ToolResultClient:
        return _TOOL_RESULTS

    @staticmethod
    def process() -> ProcessClient:
        return _PROCESS

    @staticmethod
    def network() -> NetworkClient:
        return _NETWORK

    @staticmethod
    def extension_http() -> ExtensionHttpClient:
        return _EXTENSION_HTTP
