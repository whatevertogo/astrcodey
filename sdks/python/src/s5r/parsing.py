"""Typed argument parsing helpers for handlers.

Mirrors `astrcode_extension_worker::worker::parse_tool_arguments` and
`parse_hook_input`. `cls` may be a dataclass type; passing `None` returns the
raw JSON value.
"""

from __future__ import annotations

import dataclasses
from typing import Any, Mapping, TypeVar

from .errors import ErrorPayload, S5rError, WireErrorCode

T = TypeVar("T")


def parse_tool_arguments(arguments: Any, cls: type[T] | None = None) -> T | Any:
    """Parse a tool invocation's already-validated `arguments`."""
    if cls is None:
        return arguments
    return _build(cls, arguments, "parse tool arguments")


def parse_hook_input(event: Mapping[str, Any], cls: type[T] | None = None) -> T | Any:
    """Parse the `input` payload of a hook event (falls back to the event)."""
    input_value = event.get("input")
    if input_value is None:
        input_value = event
    if cls is None:
        return input_value
    return _build(cls, input_value, "parse hook input")


def _build(cls: type[T], value: Any, what: str) -> T:
    if not dataclasses.is_dataclass(cls):
        raise TypeError(f"{cls!r} is not a dataclass type")
    if not isinstance(value, Mapping):
        raise S5rError(
            ErrorPayload(WireErrorCode.INVALID_INPUT, f"{what}: expected an object")
        )
    fields = {f.name: f for f in dataclasses.fields(cls)}
    unknown = set(value) - set(fields)
    if unknown:
        raise S5rError(
            ErrorPayload(
                WireErrorCode.INVALID_INPUT,
                f"{what}: unknown fields {sorted(unknown)}",
            )
        )
    missing = [
        name
        for name, f in fields.items()
        if name not in value
        and f.default is dataclasses.MISSING
        and f.default_factory is dataclasses.MISSING
    ]
    if missing:
        raise S5rError(
            ErrorPayload(
                WireErrorCode.INVALID_INPUT,
                f"{what}: missing fields {missing}",
            )
        )
    try:
        return cls(**{name: value[name] for name in fields if name in value})
    except TypeError as error:
        raise S5rError(
            ErrorPayload(WireErrorCode.INVALID_INPUT, f"{what}: {error}")
        ) from error
