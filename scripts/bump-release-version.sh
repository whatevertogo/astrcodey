#!/usr/bin/env bash
# Keep release-facing version metadata in sync before tagging.

set -euo pipefail

VERSION="${1:?Usage: scripts/bump-release-version.sh <version>}"
OLD=$(python3 - <<'PY'
import re
from pathlib import Path

text = Path("Cargo.toml").read_text()
match = re.search(r'(?ms)^\[workspace\.package\].*?^version = "([^"]+)"', text)
if not match:
    raise SystemExit("workspace.package version not found")
print(match.group(1))
PY
)

if [[ "$OLD" == "$VERSION" ]]; then
  echo "Version is already ${VERSION}"
  exit 0
fi

python3 - "$VERSION" "$OLD" <<'PY'
import json
import re
import sys
from pathlib import Path

version = sys.argv[1]
old = sys.argv[2]

def replace_workspace_version(path: Path) -> None:
    text = path.read_text()
    updated = re.sub(
        rf'(?ms)(^\[workspace\.package\].*?^version = "){re.escape(old)}(")',
        rf"\g<1>{version}\2",
        text,
        count=1,
    )
    if updated == text:
        raise SystemExit(f"failed to update workspace version in {path}")
    path.write_text(updated)

def replace_package_version(path: Path) -> None:
    text = path.read_text()
    updated = re.sub(
        rf'(?m)(^version = "){re.escape(old)}(")',
        rf"\g<1>{version}\2",
        text,
        count=1,
    )
    if updated == text:
        raise SystemExit(f"failed to update package version in {path}")
    path.write_text(updated)

def update_json_version(path: Path) -> None:
    data = json.loads(path.read_text())
    data["version"] = version
    path.write_text(json.dumps(data, indent=2) + "\n")

def update_npm_package(path: Path) -> None:
    data = json.loads(path.read_text())
    data["version"] = version
    if "optionalDependencies" in data:
        data["optionalDependencies"] = {
            name: version for name in data["optionalDependencies"]
        }
    path.write_text(json.dumps(data, indent=2) + "\n")

replace_workspace_version(Path("Cargo.toml"))
update_json_version(Path("src-tauri/tauri.conf.json"))
update_npm_package(Path("npm/astrcode/package.json"))
replace_package_version(Path("crates/astrcode-extensions/tests/s5r-guest/Cargo.toml"))

for path in [
    Path("Cargo.lock"),
    Path("crates/astrcode-extensions/tests/s5r-guest/Cargo.lock"),
    Path("src-tauri/Cargo.lock"),
]:
    if not path.exists():
        continue
    lines = path.read_text().splitlines(keepends=True)
    current_name = None
    output = []
    for line in lines:
        name_match = re.match(r'name = "([^"]+)"', line)
        if name_match:
            current_name = name_match.group(1)
        if (
            current_name is not None
            and (current_name.startswith("astrcode") or current_name == "s5r-guest-demo")
            and line.startswith('version = "')
        ):
            line = f'version = "{version}"\n'
        output.append(line)
    path.write_text("".join(output))
PY

# Keep frontend package metadata and its lockfile in sync through npm.
npm version "${VERSION}" --no-git-tag-version --prefix frontend

echo "Bumped release metadata from ${OLD} to ${VERSION}"
