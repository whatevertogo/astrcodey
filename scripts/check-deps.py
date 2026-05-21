#!/usr/bin/env python3
"""Check workspace crate dependency direction rules.

Layer hierarchy:
  L0 Foundation:   astrcode-core, astrcode-desktop
  L1 Infrastructure: astrcode-support, astrcode-protocol, astrcode-ai
  L2 Domain:       astrcode-log, astrcode-storage, astrcode-context,
                   astrcode-tools, astrcode-extensions, astrcode-client
  L3 Extensions:   astrcode-extension-*, astrcode-bundled-extensions
  L4 Session:      astrcode-session
  L5 Server:       astrcode-server
  L6 CLI:          astrcode-cli

Rule: a crate may only depend on crates at a strictly lower layer.
Exception: astrcode-bundled-extensions (L3) may depend on same-layer
           astrcode-extension-* crates (aggregation).
"""

from __future__ import annotations

import tomllib
import sys
from collections import defaultdict
from pathlib import Path

# ── Layer definitions ──────────────────────────────────────────────

LAYERS: dict[str, int] = {
    # L0 – Foundation
    "astrcode-core": 0,
    "astrcode-desktop": 0,
    # L1 – Infrastructure
    "astrcode-support": 1,
    "astrcode-protocol": 1,
    "astrcode-ai": 1,
    "astrcode-eval": 1,
    # L2 – Domain Services
    "astrcode-log": 2,
    "astrcode-storage": 2,
    "astrcode-context": 2,
    "astrcode-tools": 2,
    "astrcode-extensions": 2,
    "astrcode-client": 2,
    # L3 – Extensions
    "astrcode-extension-agent-tools": 3,
    "astrcode-extension-mcp": 3,
    "astrcode-extension-skill": 3,
    "astrcode-extension-todo-tool": 3,
    "astrcode-extension-mode": 3,
    "astrcode-extension-memory": 3,
    "astrcode-extension-model": 3,
    "astrcode-bundled-extensions": 3,
    # L4 – Session
    "astrcode-session": 4,
    # L5 – Server
    "astrcode-server": 5,
    # L6 – CLI
    "astrcode-cli": 6,
}

LAYER_NAMES: dict[int, str] = {
    0: "Foundation",
    1: "Infrastructure",
    2: "Domain",
    3: "Extensions",
    4: "Session",
    5: "Server",
    6: "CLI",
}

ALLOWED_SAME_LAYER: set[tuple[str, str]] = {
    (dep, ext)
    for ext in (
        "astrcode-extension-agent-tools",
        "astrcode-extension-mcp",
        "astrcode-extension-skill",
        "astrcode-extension-todo-tool",
        "astrcode-extension-mode",
        "astrcode-extension-memory",
        "astrcode-extension-model",
    )
    for dep in ("astrcode-bundled-extensions",)
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
