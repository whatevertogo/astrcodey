#!/usr/bin/env bash
# 检测并配置最快的可用 linker 到用户全局 Cargo 配置。
# Cargo 会自动合并 ~/.cargo/config.toml 和项目级 .cargo/config.toml。
#
# 用法: ./scripts/setup-fast-compile.sh
# 效果: 在 ~/.cargo/config.toml 中追加 linker 配置（如果尚未配置）

set -euo pipefail

GLOBAL_CONFIG="${CARGO_HOME:-$HOME/.cargo}/config.toml"

detect_linker() {
    if command -v mold &>/dev/null; then
        if command -v clang &>/dev/null; then
            echo "mold-clang"
        else
            echo "mold-gcc"
        fi
    elif command -v lld &>/dev/null && command -v clang &>/dev/null; then
        echo "lld"
    else
        echo "default"
    fi
}

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        LINKER=$(detect_linker)
        if [ "$ARCH" = "x86_64" ]; then
            TARGET="x86_64-unknown-linux-gnu"
        elif [ "$ARCH" = "aarch64" ]; then
            TARGET="aarch64-unknown-linux-gnu"
        else
            echo "⚠ 未知架构: $ARCH，跳过 linker 配置"
            exit 0
        fi

        # 检查是否已经配置过
        if [ -f "$GLOBAL_CONFIG" ] && grep -q "\[target\.$TARGET\]" "$GLOBAL_CONFIG" 2>/dev/null; then
            echo "✓ $GLOBAL_CONFIG 已包含 [$TARGET] 配置，跳过"
            exit 0
        fi

        case "$LINKER" in
            mold-clang)
                echo "✓ 检测到 mold + clang，配置快速链接"
                mkdir -p "$(dirname "$GLOBAL_CONFIG")"
                cat >> "$GLOBAL_CONFIG" << EOF

# --- astrcode fast-compile (auto-generated) ---
[target.$TARGET]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
EOF
                ;;
            mold-gcc)
                echo "✓ 检测到 mold + gcc，配置快速链接"
                mkdir -p "$(dirname "$GLOBAL_CONFIG")"
                cat >> "$GLOBAL_CONFIG" << EOF

# --- astrcode fast-compile (auto-generated) ---
[target.$TARGET]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
EOF
                ;;
            lld)
                echo "✓ 检测到 lld + clang，配置快速链接"
                mkdir -p "$(dirname "$GLOBAL_CONFIG")"
                cat >> "$GLOBAL_CONFIG" << EOF

# --- astrcode fast-compile (auto-generated) ---
[target.$TARGET]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
EOF
                ;;
            *)
                echo "⚠ 未检测到快速 linker，使用系统默认"
                echo "  建议安装: sudo apt install mold"
                exit 0
                ;;
        esac
        echo "✓ 已写入 $GLOBAL_CONFIG"
        ;;
    Darwin)
        echo "✓ macOS 默认 linker 已较快，无需额外配置"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        echo "✓ Windows 使用 MSVC 默认 linker，无需额外配置"
        ;;
    *)
        echo "⚠ 未知系统: $OS"
        ;;
esac
