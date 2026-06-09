# AstrCode 内核化嵌入边界

AstrCode 的可嵌入形态分为三层：

| 层级 | Crate | 角色 |
| --- | --- | --- |
| 契约层 | `astrcode-core` | LLM、工具、prompt、context、storage、extension 事件等共享 trait / 数据契约 |
| 内核层 | `astrcode-kernel`、`astrcode-session` | 运行时装配、session 生命周期、turn loop、工具调度、事件持久化 |
| 默认实现层 | `astrcode-context`、`astrcode-tools`、`astrcode-extensions`、`astrcode-server` | first-party prompt/context/工具/扩展/server 装配 |

`astrcode-session` 不依赖 first-party 默认实现；宿主通过
`SessionHostServices` 注入所需能力。这个边界对普通依赖和测试依赖都成立，避免
session 单测通过默认 context/tools/extensions 间接锁死内核。

## 最小宿主装配

嵌入方必须提供：

- `LlmProvider`
- `ContextAssembler`
- `PromptProvider`
- `PromptFileProvider`
- `EffectiveConfig`
- `EventStore`

其余能力默认可为空：

- extension runtime：`NoopExtensionRuntime`
- post-compact enrichment：`NoopPostCompactEnricher`
- tool packs：空列表

推荐从 `SessionHostServices::embedded(...)` 开始，再按需打开能力：

```rust
let host_services = SessionHostServices::embedded(
    context_assembler,
    prompt_provider,
    prompt_file_provider,
)
.with_tool_packs(vec![my_tool_pack]);

let services = Arc::new(SessionRuntimeServices::new(
    llm.clone(),
    small_llm,
    effective_config,
    host_services,
));
```

可运行示例：

```powershell
cargo run -p astrcode-session --example embedded_kernel
```

该示例只使用 `astrcode-core`、`astrcode-kernel`、`astrcode-session` 和测试用
in-memory storage，演示自定义 `ContextAssembler`、`PromptProvider` 和
`ToolPack` 的最小宿主装配。

## 可替换能力

| 能力 | 契约 | 默认实现 |
| --- | --- | --- |
| context window / compact | `astrcode_core::context::ContextAssembler` | `astrcode_context::context_assembler::LlmContextAssembler` |
| post-compact enrichment | `astrcode_core::context::PostCompactEnricher` | `astrcode_context::post_compact_enricher::DefaultPostCompactEnricher` |
| prompt assembly | `astrcode_core::prompt::PromptProvider` | `astrcode_context::prompt_engine::DefaultPromptProvider` |
| prompt files/rules | `astrcode_core::prompt::PromptFileProvider` | `astrcode_context::prompt_engine::DefaultPromptFileProvider` |
| extension hooks | `astrcode_kernel::ExtensionRuntime` | `astrcode_extensions::runner::ExtensionRunner` |
| tools | `astrcode_kernel::ToolPack` | `astrcode_tools::registry::default_tool_packs()` |

The server binary exposes its first-party composition as
`astrcode_server::default_host::first_party_host_services(...)`. That profile is
just a host preset built from the same injectable contracts; it is not required
by `astrcode-session`.

## 与 pi-mono 的对应关系

pi-mono 的 `coding-agent` 通过 core/session services 暴露可嵌入入口，CLI/TUI 只是
默认装配层。AstrCode 的对应结构是：

| pi-mono 概念 | AstrCode 对应 |
| --- | --- |
| core/session runtime | `astrcode-session` + `astrcode-kernel` |
| session services | `SessionRuntimeServices` / `SessionHostServices` |
| default resource loader / tool factories | `astrcode-server::default_host` + `astrcode-tools` |
| extension runner | `astrcode_kernel::ExtensionRuntime` |
| custom tools | `astrcode_kernel::ToolPack` |

迁移到其他场景时，优先依赖 `astrcode-core`、`astrcode-kernel`、`astrcode-session`
和所需 storage 实现；不要从 `astrcode-server` 反向拿默认能力，除非目标场景就是
复用 AstrCode 的 first-party profile。

## 边界规则

- 内核 crate 不直接依赖默认工具、扩展 loader、server 或 prompt/context 默认实现。
- 默认实现可以依赖核心契约，但不能反向要求宿主使用 first-party server 或 bundled extensions。
- 新增跨边界数据时放入 `astrcode-core`；新增可替换行为时优先定义 trait，再由默认实现 crate 实现。
- 内置插件不能依赖项目其他内容，只能依赖插件系统。

## 边界验证

修改内核或默认实现装配后至少检查：

```powershell
python scripts\check-deps.py
cargo tree -p astrcode-session | Select-String "astrcode-context|astrcode-extensions|astrcode-tools"
cargo run -p astrcode-session --example embedded_kernel
cargo test -p astrcode-session --test embedded_host
```

`check-deps.py` 会在 CI 中执行，并禁止 `astrcode-session` 通过普通依赖或测试依赖
重新依赖 first-party 默认实现。`cargo tree` 命令应无输出；`embedded_kernel`
和 `embedded_host` 证明 session 可以在没有 first-party context/tools/extensions
的宿主中启动、组装 prompt 并注册自定义工具。
