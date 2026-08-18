"""Drift guard: Python mirror catalogs must match the Rust sources of truth.

Parses the Rust constant tables with stdlib regexes (no codegen): wire error
codes, extension capabilities, host operation wire names, and the hook mode
tables. Any manual-mirror drift on either side fails this test.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

from s5r.errors import WireErrorCode
from s5r.host import HostOperation
from s5r.manifest import _BLOCKING_ALLOWED, ALL_CAPABILITIES, FIXED_HOOK_MODES

RUST_SDK = (
    Path(__file__).resolve().parents[3] / "crates" / "astrcode-extension-sdk" / "src"
)


def _block(text: str, anchor: str) -> str:
    """Return the brace-delimited block following the first `anchor` occurrence."""
    open_brace = text.index("{", text.index(anchor))
    depth = 0
    for index in range(open_brace, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return text[open_brace + 1 : index]
    raise AssertionError(f"unterminated block after {anchor!r}")


def _snake(name: str) -> str:
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name).lower()


def _class_strings(cls: type) -> set[str]:
    return {value for key, value in vars(cls).items() if not key.startswith("_")}


def _rust_macro_wires(path: Path, invocation: str) -> set[str]:
    body = _block(path.read_text(encoding="utf-8"), invocation)
    return set(re.findall(r'\w+ => "([^"]+)"', body))


def _rust_fixed_hook_modes(text: str) -> dict[str, str]:
    body = _block(text, "pub fn fixed_hook_mode")
    fixed: dict[str, str] = {}
    pending: list[str] = []
    for kind, name in re.findall(r"(LifecycleEvent|HookMode)::(\w+)", body):
        if kind == "LifecycleEvent":
            pending.append(_snake(name))
        else:
            for event in pending:
                fixed[event] = _snake(name)
            pending = []
    return fixed


def _rust_blocking_allowed(text: str) -> set[str]:
    events = set(
        re.findall(
            r"LifecycleEvent::(\w+)",
            _block(text, "fn lifecycle_event_allows_blocking"),
        )
    )
    events |= set(
        re.findall(r"LifecycleEvent::(\w+)", _block(text, "pub fn hook_mode_is_supported"))
    )
    return {_snake(event) for event in events}


class RustParityTest(unittest.TestCase):
    def test_wire_error_codes_match(self) -> None:
        rust = _rust_macro_wires(RUST_SDK / "wire" / "error.rs", "wire_error_codes! {")
        self.assertEqual(_class_strings(WireErrorCode), rust)

    def test_capabilities_match(self) -> None:
        rust = _rust_macro_wires(
            RUST_SDK / "wire" / "capability.rs", "extension_capabilities! {"
        )
        self.assertEqual(set(ALL_CAPABILITIES), rust)

    def test_host_operation_wire_names_match(self) -> None:
        body = _block(
            (RUST_SDK / "wire" / "operation.rs").read_text(encoding="utf-8"),
            "host_operations! {",
        )
        rust = set(re.findall(r'name: "([^"]+)"', body))
        self.assertEqual(_class_strings(HostOperation), rust)

    def test_hook_mode_tables_match(self) -> None:
        text = (RUST_SDK / "extension" / "registration_validation.rs").read_text(
            encoding="utf-8"
        )
        self.assertEqual(FIXED_HOOK_MODES, _rust_fixed_hook_modes(text))
        self.assertEqual(set(_BLOCKING_ALLOWED), _rust_blocking_allowed(text))


if __name__ == "__main__":
    unittest.main()
