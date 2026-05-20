#!/usr/bin/env bash
# 根据已构建的二进制生成平台 npm 包。
# 用法: VERSION=0.1.0 ./scripts/prepare-npm-packages.sh <artifacts-dir> <output-dir>
#
# artifacts-dir 结构:
#   astrcode-x86_64-linux/astrcode-x86_64-linux.tar.gz
#   astrcode-aarch64-linux/astrcode-aarch64-linux.tar.gz
#   astrcode-x86_64-macos/astrcode-x86_64-macos.tar.gz
#   astrcode-aarch64-macos/astrcode-aarch64-macos.tar.gz
#   astrcode-x86_64-windows/astrcode-x86_64-windows.zip
#   astrcode-aarch64-windows/astrcode-aarch64-windows.zip

set -euo pipefail

ARTIFACTS_DIR="${1:?Usage: prepare-npm-packages.sh <artifacts-dir> <output-dir>}"
OUTPUT_DIR="${2:?Usage: prepare-npm-packages.sh <artifacts-dir> <output-dir>}"
VERSION="${VERSION:?VERSION env var required}"

declare -A PACKAGES=(
  ["cli-linux-x64"]="astrcode-x86_64-linux.tar.gz:astrcode:linux:x64"
  ["cli-linux-arm64"]="astrcode-aarch64-linux.tar.gz:astrcode:linux:arm64"
  ["cli-darwin-x64"]="astrcode-x86_64-macos.tar.gz:astrcode:darwin:x64"
  ["cli-darwin-arm64"]="astrcode-aarch64-macos.tar.gz:astrcode:darwin:arm64"
  ["cli-win32-x64"]="astrcode-x86_64-windows.zip:astrcode.exe:win32:x64"
  ["cli-win32-arm64"]="astrcode-aarch64-windows.zip:astrcode.exe:win32:arm64"
)

for pkg_name in "${!PACKAGES[@]}"; do
  IFS=':' read -r archive binary os cpu <<< "${PACKAGES[$pkg_name]}"
  pkg_dir="${OUTPUT_DIR}/@astrcode/${pkg_name}"
  mkdir -p "$pkg_dir"

  # Extract binary
  archive_path=$(find "$ARTIFACTS_DIR" -name "$archive" | head -1)
  if [ -z "$archive_path" ]; then
    echo "WARNING: $archive not found, skipping $pkg_name"
    continue
  fi

  if [[ "$archive" == *.tar.gz ]]; then
    tar xzf "$archive_path" -C "$pkg_dir"
  else
    unzip -qo "$archive_path" -d "$pkg_dir"
  fi

  # Write package.json
  cat > "$pkg_dir/package.json" <<EOF
{
  "name": "@astrcode/${pkg_name}",
  "version": "${VERSION}",
  "description": "astrcode CLI binary for ${os}-${cpu}",
  "license": "MIT",
  "os": ["${os}"],
  "cpu": ["${cpu}"],
  "files": ["${binary}"]
}
EOF
  echo "Prepared @astrcode/${pkg_name} (${archive})"
done

# Update main package version
mkdir -p "${OUTPUT_DIR}/astrcode"
jq --arg v "$VERSION" '.version = $v | .optionalDependencies |= with_entries(.value = $v)' \
  npm/astrcode/package.json > "${OUTPUT_DIR}/astrcode/package.json.tmp"
mv "${OUTPUT_DIR}/astrcode/package.json.tmp" "${OUTPUT_DIR}/astrcode/package.json"
cp npm/astrcode/install.js "${OUTPUT_DIR}/astrcode/"
mkdir -p "${OUTPUT_DIR}/astrcode/bin"
cp npm/astrcode/bin/astrcode "${OUTPUT_DIR}/astrcode/bin/"

echo "All npm packages prepared in ${OUTPUT_DIR}"
