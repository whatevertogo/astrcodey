# 代码冗余清理

## [低] 6 个 extension crate 冗余 `crate-type = ["rlib"]`

**位置**：
- `crates/astrcode-extension-agent-tools/Cargo.toml:8-9`
- `crates/astrcode-extension-mcp/Cargo.toml:8-9`
- `crates/astrcode-extension-memory/Cargo.toml:8-9`
- `crates/astrcode-extension-mode/Cargo.toml:8-9`
- `crates/astrcode-extension-skill/Cargo.toml:8-9`
- `crates/astrcode-extension-todo-tool/Cargo.toml:8-9`

**问题**：`rlib` 是 library crate 的默认 crate-type，这 6 个 `[lib]` 段仅含 `crate-type = ["rlib"]`，整个 `[lib]` 段无任何作用。

**方案**：移除这 6 个 Cargo.toml 中的 `[lib]` 段。
