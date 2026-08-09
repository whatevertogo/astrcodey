#!/usr/bin/env python3
"""Check workspace crate dependency direction rules.

Layer hierarchy:
  L0 Wire:         astrcode-extension-contract
  L1 Foundation:   astrcode-core, astrcode-desktop
  L2 Primitives:   astrcode-session-projection
  L3 Services:     astrcode-extension-sdk, astrcode-ai, astrcode-context,
                   astrcode-log, astrcode-storage
  L4 Integration:  astrcode-protocol, astrcode-tools, astrcode-extensions,
                   astrcode-extension-*
  L5 Runtime:      astrcode-session, astrcode-client, astrcode-bundled-extensions,
                   astrcode-eval
  L6 Server:       astrcode-server
  L7 CLI:          astrcode-cli

Rule: a crate may only depend on crates at a strictly lower layer.
"""

from __future__ import annotations

import tomllib
import sys
from collections import defaultdict
from pathlib import Path

# ── Layer definitions ──────────────────────────────────────────────

LAYERS: dict[str, int] = {
    # L0 – Wire contract
    "astrcode-extension-contract": 0,
    # L1 – Foundation
    "astrcode-core": 1,
    "astrcode-desktop": 1,
    # L2 – Primitive contracts
    "astrcode-session-projection": 2,
    # L3 – Services
    "astrcode-extension-sdk": 3,
    "astrcode-ai": 3,
    "astrcode-context": 3,
    "astrcode-log": 3,
    "astrcode-storage": 3,
    # L4 – Integration and extension implementations
    "astrcode-protocol": 4,
    "astrcode-tools": 4,
    "astrcode-extensions": 4,
    "astrcode-extension-worker": 4,
    "astrcode-extension-goal": 4,
    "astrcode-extension-agent-tools": 4,
    "astrcode-extension-mcp": 4,
    "astrcode-extension-skill": 4,
    "astrcode-extension-todo-tool": 4,
    "astrcode-extension-mode": 4,
    "astrcode-extension-ask-user": 4,
    "astrcode-extension-memory": 4,
    "astrcode-extension-channels": 4,
    "astrcode-extension-web-tools": 4,
    # L5 – Runtime and composition
    "astrcode-client": 5,
    "astrcode-bundled-extensions": 5,
    "astrcode-eval": 5,
    "astrcode-session": 5,
    # L6 – Server
    "astrcode-server": 6,
    # L7 – CLI
    "astrcode-cli": 7,
}

LAYER_NAMES: dict[int, str] = {
    0: "Wire",
    1: "Foundation",
    2: "Primitives",
    3: "Services",
    4: "Integration",
    5: "Runtime",
    6: "Server",
    7: "CLI",
}

ALLOWED_SAME_LAYER: set[tuple[str, str]] = set()

FORBIDDEN_DEPS: dict[str, set[str]] = {
    # Session owns prompt lifecycle and intentionally depends on context's
    # concrete prompt implementation. It must not depend on higher layers.
    "astrcode-session": {
        "astrcode-tools",
        "astrcode-extensions",
        "astrcode-bundled-extensions",
        "astrcode-server",
    },
}


# ── Workspace discovery ────────────────────────────────────────────

def find_workspace_root() -> Path:
    """Walk upward from this script to find the workspace root."""
    d = Path(__file__).resolve().parent
    for _ in range(10):
        manifest = d / "Cargo.toml"
        if manifest.is_file():
            with open(manifest, "rb") as f:
                data = tomllib.load(f)
            if "workspace" in data:
                return d
        d = d.parent
    sys.exit("error: cannot find workspace root (no Cargo.toml with [workspace])")


def discover_members(root: Path) -> dict[str, Path]:
    """Return {crate_name: manifest_path} for every workspace member."""
    with open(root / "Cargo.toml", "rb") as f:
        data = tomllib.load(f)

    members: dict[str, Path] = {}
    for pat in data["workspace"]["members"]:
        for crate_dir in sorted(root.glob(pat)):
            manifest = crate_dir / "Cargo.toml"
            if not manifest.is_file():
                continue
            with open(manifest, "rb") as f:
                pkg = tomllib.load(f)
            name = pkg["package"]["name"]
            members[name] = manifest
    return members


# ── Dependency extraction ──────────────────────────────────────────

def extract_deps(manifest: Path, all_names: set[str]) -> set[str]:
    """Extract workspace-internal production dependencies."""
    with open(manifest, "rb") as f:
        data = tomllib.load(f)

    deps: set[str] = set()
    for dep_name, spec in data.get("dependencies", {}).items():
        if dep_name in all_names:
            deps.add(dep_name)
    return deps


def extract_deps_from_sections(
    manifest: Path,
    all_names: set[str],
    sections: tuple[str, ...],
) -> set[str]:
    """Extract workspace-internal dependencies from selected manifest sections."""
    with open(manifest, "rb") as f:
        data = tomllib.load(f)

    deps: set[str] = set()
    for section in sections:
        for dep_name in data.get(section, {}):
            if dep_name in all_names:
                deps.add(dep_name)
    return deps


# ── Cycle detection ────────────────────────────────────────────────

def detect_cycles(graph: dict[str, set[str]]) -> list[list[str]]:
    """Return all cycles found via DFS."""
    WHITE, GRAY, BLACK = 0, 1, 2
    color: dict[str, int] = {n: WHITE for n in graph}
    path: list[str] = []
    cycles: list[list[str]] = []

    def dfs(node: str) -> None:
        color[node] = GRAY
        path.append(node)
        for dep in sorted(graph.get(node, [])):
            if color[dep] == GRAY:
                idx = path.index(dep)
                cycles.append(path[idx:] + [dep])
            elif color[dep] == WHITE:
                dfs(dep)
        path.pop()
        color[node] = BLACK

    for node in sorted(graph):
        if color[node] == WHITE:
            dfs(node)
    return cycles


# ── Main ───────────────────────────────────────────────────────────

def main() -> None:
    root = find_workspace_root()
    members = discover_members(root)
    all_names = set(members.keys())

    # Check for crates in LAYERS that no longer exist
    unknown = set(LAYERS.keys()) - all_names
    if unknown:
        print("WARNING: LAYERS contains unknown crates:", ", ".join(sorted(unknown)))

    # Check for crates not in LAYERS
    missing = all_names - set(LAYERS.keys())
    if missing:
        for name in sorted(missing):
            print(f"ERROR: crate '{name}' not defined in LAYERS")
        sys.exit(1)

    # Build dependency graph (production deps only)
    graph: dict[str, set[str]] = {}
    for name, manifest in members.items():
        graph[name] = extract_deps(manifest, all_names)

    violations: list[str] = []

    # Check layer direction
    for crate, deps in sorted(graph.items()):
        crate_layer = LAYERS[crate]
        for dep in sorted(deps):
            dep_layer = LAYERS[dep]
            if dep_layer >= crate_layer:
                if (crate, dep) in ALLOWED_SAME_LAYER:
                    continue
                direction = "same layer" if dep_layer == crate_layer else "higher layer"
                violations.append(
                    f"  {crate} (L{crate_layer}) -> {dep} (L{dep_layer}) [{direction}]"
                )

    # Check explicit crate boundary rules. These include test-only dependencies
    # when the boundary is part of the embeddable public shape.
    for crate, forbidden in sorted(FORBIDDEN_DEPS.items()):
        manifest = members.get(crate)
        if manifest is None:
            continue
        deps = extract_deps_from_sections(
            manifest,
            all_names,
            ("dependencies", "dev-dependencies", "build-dependencies"),
        )
        for dep in sorted(deps & forbidden):
            violations.append(f"  {crate} must not depend on first-party default crate {dep}")

    # Check cycles
    cycles = detect_cycles(graph)
    for cycle in cycles:
        violations.append(f"  cycle: {' -> '.join(cycle)}")

    # Print layer map
    by_layer: dict[int, list[str]] = defaultdict(list)
    for name, layer in sorted(LAYERS.items()):
        by_layer[layer].append(name)

    print("Layer hierarchy:")
    for layer in sorted(by_layer):
        label = LAYER_NAMES.get(layer, "?")
        print(f"  L{layer} {label}: {', '.join(by_layer[layer])}")
    print()

    if violations:
        print(f"Found {len(violations)} violation(s):\n")
        for v in violations:
            print(v)
        print(f"\nDependency direction check FAILED.")
        sys.exit(1)
    else:
        print(f"All {len(members)} crates passed dependency direction checks.")


if __name__ == "__main__":
    main()
