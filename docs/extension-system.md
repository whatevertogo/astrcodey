# AstrCode 扩展系统（规范）

> **本文档是 AstrCode 扩展机制的唯一 normative 说明。**  
> 若与 `docs/plugin-system.md`（s5r / AstrBot 起源）冲突，以本文档为准。

---

## 是什么

AstrCode 的扩展（Extension）是主要扩展机制：技能、斜杠命令、自定义 Agent 工具、生命周期钩子均通过 `astrcode-core::Extension` trait 注册，由 `ExtensionRunner` 分发。

| 层级 | 实现 | 信任 |
|------|------|------|
| 内置 | `astrcode-bundled-extensions` + 各 `astrcode-extension-*` | 进程内，`ExtensionHostServices` |
| 磁盘 WASM | `~/.astrcode/extensions/`、`.astrcode/extensions/` | s6r 沙箱 + `host_invoke` 白名单 |
| 外部工具 | `astrcode-extension-mcp` | MCP 子进程/HTTP，非 Extension trait |

## 协议

- **WASM 线缆**：[`plugin-system-wasm-s6r.md`](./plugin-system-wasm-s6r.md)（s6r：`extension_manifest` + `extension_call`）
- **宿主回调**：[`host-invoke-plan.md`](./host-invoke-plan.md)（`host_invoke` import，JSON `{ok, output|error}`）
- **能力枚举**：`astrcode-core::ExtensionCapability`（`session_state`、`small_model`、`workspace_read` 等）

加载流程：`extension.json` 仅含 `library` → 宿主加载 WASM → `extension_manifest()`（此阶段无 `host_invoke` 后端）→ 绑定 manifest 声明的 `ExtensionCapability` 与全局后端 → `extension_call` 时由 `host_invoke::authorize` 校验。

## 不要做什么

- 不要把 `plugin-system.md` 里的 s5r / `plugin_init` / `platform.*` / IPC STDIO 当作 AstrCode 实现目标（与 coding agent 产品面不符）。
- 不要用 IPC 插件重复 MCP 已覆盖的「任意语言外部工具」场景。
- 不要让 bundled 扩展走 WASM（`Arc<dyn LlmProvider>` 与 wasm32 约束见 `host-invoke-plan.md`）。

## 新增 WASM 宿主能力 checklist

1. 在 `ExtensionCapability` 增加枚举项（若需要新声明）
2. 在 `host_invoke::required_capability` 表 + 后端（如 `build_small_llm_invoker`）实现
3. 更新 `host-invoke-plan.md` 与 guest SDK 示例
4. 为 `host_invoke::authorize` 增加单元测试，必要时补 `s6r_e2e_test`

## 相关 crate

| Crate | 职责 |
|-------|------|
| `astrcode-core` | `Extension` trait、`ExtensionCapability` |
| `astrcode-extensions` | `ExtensionRunner`、loader、`wasm_ext`、`host_invoke` |
| `astrcode-extension-sdk` | s6r 类型、Registrar |
| `astrcode-server` | bootstrap 注入 `build_small_llm_invoker` |
