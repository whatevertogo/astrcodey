# s5r3-phase-0 PR 逐文件价值审计

- 审计范围:基线 `b3e66f8`(与 main 的 merge-base)→ 当前 index(已提交 35 个 commit + 暂存改动),共 **552 个文件,+78,451 / −37,352**。
- 审计方式:42 个并行子代理逐文件审读 diff,每个文件一条记录,覆盖 552/552。
- 类别口径:核心功能 / 架构搬移 / 机械适配 / 测试 / 文档 / 生成物 / 配置 / 存疑。
- 注意:未暂存的 `ToolExecutionPolicy` 半成品重构不在本审计范围内。

## 总体结论

PR 的代码量主要由四部分构成,绝大部分有明确价值:

1. **架构搬移(约占增删的大头)**:astrcode-tools 整体删除(~7.9k 行)迁到 astrcode-extension-coding(~2.3k 行重写);sdk 的 runtime/* + worker/*(~4.5k 行)删除,重写为 wire/* 与新 crate astrcode-extension-worker;s5r_ext/session.rs → v3_session.rs;session_repo.rs(1k 行)拆成 session_repo/ 目录;session 的 compaction.rs 拆成 compaction/ 目录。增删双高但净增有限。
2. **核心新功能**:wire/peer_runtime.rs(2.4k)与 wire/* 协议层、host_router/workspace/process_handles、runner 的 custom_event_delivery/host_invoker、session 所有权租约、mid-turn 输入吸收、timeout_ms + presentation intent 全链路、Python SDK(~2.7k 含测试)、conformance 工具。
3. **测试**:runner/tests.rs +2.9k、loader/e2e/server/session/storage 各套件大幅扩充,新增 mid_turn_absorption、provider_contribution_settlement 等——占新增行数约 15–20%,合理。
4. **机械适配与生成物**:frontend generated DTO(~30 文件)、Cargo.lock、s5r-guest/Cargo.lock、README 等。

## 存疑项汇总(10 项,详见各批次)

| 文件 | 疑点 |
|---|---|
| artifacts/tmp/perf-baseline.md | tmp 路径入库 + 含未完成事项,建议挪 docs/ 或移出 PR |
| crates/astrcode-core/src/llm/thinking.rs | 纯 let-chains 风格重写,与主线无关,建议拆出 |
| crates/astrcode-extension-channels/src/lib.rs | 配置热更新被删、telegram working_dir 语义变更,需确认非功能回退 |
| crates/astrcode-extension-mode/src/catalog.rs | PLAN_RESTRICTED_TOOLS 删掉 "terminal",plan 模式可能不再拦截 terminal 工具 |
| crates/astrcode-extension-sdk/src/wire/host/session.rs | 61–63 行残留从 process.rs 复制的错误文档注释,建议删 |
| crates/astrcode-server/src/http/auth.rs | Bearer 鉴权中间件整体删除,安全语义变更,需确认有意 |
| crates/astrcode-server/src/http/routes/extensions.rs | set_enabled 不再重载扩展、reload_errors 恒空,需确认非功能回退 |
| crates/astrcode-session/src/tool_deduplicator.rs | 纯 let-chain 风格 churn,建议拆出 |
| crates/astrcode-storage/src/snapshot.rs | 快照恢复机制删除无替代,长会话冷启动退化为全量重放 |
| crates/astrcode-tools/src/terminal_tool.rs | 持久 PTY 终端工具删除无替代,需确认有意砍功能 |

---

# 批次 01:CI/工作区配置、根级文档与杂项

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| .github/workflows/ci.yml | +67/-15 | 配置 | 固定 nightly-2026-07-27 工具链(dtolnay/rust-toolchain@master + 显式 toolchain);clippy/test/check 不再 exclude astrcode-desktop,改为装 webkit2gtk 依赖 + prepare-sidecar 后全量跑;test 新增 S5R 3.0 worker conformance 步骤;新增拒绝 `#![feature(...)]` 检查和 1.88.0 MSRV 三平台矩阵。与 edition 2024/MSRV 1.88 升级及 desktop 入 workspace 测试范围一致,值得;但 CI 范围扩大(desktop 进 clippy/test)与 S5R 重构本身无直接关系,属于搭车改动,可接受但值得在 PR 描述中说明。 |
| .github/workflows/release.yml | +5/-2 | 配置 | 与 ci.yml 同步固定 RUST_TOOLCHAIN=nightly-2026-07-27,机械跟随,必要。 |
| .gitignore | +11/-0 | 配置 | 为新增 sdks/python 增加 Python 忽略规则(__pycache__、egg-info、dist/venv/pytest/mypy/ruff 缓存),跟随 Python SDK 引入,必要。 |
| Cargo.lock | +231/-178 | 生成物 | 跟随 Cargo.toml:移除 astrcode-tools 及其独占依赖(grep-searcher/grep-regex/grep-matcher、portable-pty、nix、shell-words、serial2、winreg 等),新增 astrcode-extension-coding/-worker/-session-commands 及 criterion 依赖树(criterion、plotters、rayon、ciborium 等)。生成物,与源改动一致,无独立价值判断问题。 |
| Cargo.toml | +8/-2 | 配置 | workspace 删 astrcode-tools、加 astrcode-extension-coding/-worker/-session-commands;edition 2021→2024、rust-version=1.88;新增 serde_path_to_error、criterion 依赖,删 portable-pty。是本次重构的根配置,必要。注意:criterion 放进 workspace.dependencies 是为本 PR 的 storage bench 服务,合理。 |
| README.md | +61/-61 | 文档 | 更新 crate 清单(26→28,删 tools、加 worker/coding/session-commands/ask-user)、架构图与 Key Design Decisions 同步 S5R 3.0 表述;顺手删掉所有「行数」列与 telegram `workingDir` 示例、config.json 迁移说明——这些都对应真实代码删除,文档同步是必要的。 |
| README_CN.md | +61/-61 | 文档 | 与 README.md 完全对应的中文版同步,必要。 |
| artifacts/tmp/perf-baseline.md | +62/-0 | 存疑 | astrcode-storage criterion 基线 + Phase 1 复测 + 「snapshot 恢复是负优化」的测量结论。内容本身有价值(支撑删除 snapshot 的建议),但路径在 `artifacts/tmp/` 下——tmp 目录通常意味临时产物,提交进 git 是否合适存疑;建议确认是有意归档(如是,考虑挪到 docs/ 或 docs/reviews/),还是误提交的本地测量草稿。另外文末「建议移除 snapshot/checkpoint,未执行」属于未完成事项,随本 PR 入库还是留到后续 PR,需要明确。 |
| release-notes.md | +80/-0 | 文档 | Unreleased 段:完整列出 S5R 3.0 breaking changes(协议握手、camelCase→snake_case、错误码收敛、memory 数据目录迁移、事件日志不可降级等)、Features、Performance。与主线改动高度吻合,是 breaking change 繁多的大重构所必需,价值高。 |
| rust-toolchain.toml | +2/-2 | 配置 | nightly → 固定 nightly-2026-07-27(并补文件末尾换行)。与 CI/release 的 RUST_TOOLCHAIN 一致,配合「源码 stable-compatible、工具链固定以复现 fmt/lint」策略,必要。 |
| scripts/check-deps.py | +91/-62 | 核心功能 | 依赖分层检查脚本:层号 0–6 改为 1–7、删 astrcode-tools、加入三个新 crate;新增 CONCRETE_EXTENSION_CRATES 完整性校验(防止新扩展绕过声明)和「具体扩展只能被 bundled-extensions 组合根依赖」的生产依赖规则。这是把「内置插件只经组合根装配」的架构约束固化进 CI,是本次扩展重构的配套护栏,有实质价值(不是机械改名)。 |
| src-tauri/src/instance.rs | +10/-12 | 机械适配 | 两处嵌套 `if let` 改为 edition 2024 let-chains(`if let ... && let ... && ...`),纯语法升级,无行为变化。 |

## 批次小结

这批是重构的「周边支撑面」:根配置(Cargo.toml/lock/toolchain)、CI、依赖护栏脚本和文档同步,整体都与主线改动一一对应,几乎没有可删的部分。两点可商榷:一是 `artifacts/tmp/perf-baseline.md` 的入库路径与「未执行建议」的归属(见存疑);二是 ci.yml 把 astrcode-desktop 纳入 clippy/test 全量矩阵并引入 webkit 依赖与 sidecar 准备步骤,属于范围扩大的搭车改动,正确但建议在 PR 描述中显式说明以免 review 困惑。check-deps.py 的新增约束规则是本批中唯一超出「跟随适配」的实质改动,方向正确。
# 批次 02:astrcode-ai(provider/wire 层适配 S5R3 核心接口)

本批是 astrcode-ai 单个 crate 的 10 个文件,共 +501/-381。改动主线:① LlmProvider 接口收敛(`generate` 默认方法删除、`Vec<LlmMessage>` → `Vec<Arc<LlmMessage>>`);② ToolOrigin 三分(Builtin/Bundled/Extension/Sdk)→ 两分(Bundled/Extension),execution_mode 字段删除;③ 新增 `LlmTokenUsage.input_accounting`(Inclusive/Components)并修复 Anthropic usage 只在 message_delta 读取导致 input 字段丢失的问题;④ 大量 let-chain 语法现代化(`if let … && …`)与测试机械适配。

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-ai/Cargo.toml | +0/-1 | 机械适配 | dev-dependency 删掉已删除的 astrcode-tools,跟随 crate 删除,必须。 |
| crates/astrcode-ai/src/common.rs | +20/-18 | 机械适配 | 新增 `utf8_prefix` 替换 `floor_char_boundary`(应为兼容稳定版 Rust 的 API 回退,值得);其余是 let-chain 改写;测试 client 加 `no_proxy()`(防本地代理干扰测试,合理小修)。 |
| crates/astrcode-ai/src/providers/anthropic.rs | +12/-15 | 机械适配 | 跟随 LlmProvider 接口:删 `generate` 便捷方法、消息改 `Arc<LlmMessage>`;测试改 `generate_request`、`ToolOrigin::Bundled`。纯接口适配,无独立价值但必要。 |
| crates/astrcode-ai/src/providers/openai.rs | +38/-31 | 机械适配 | 同 anthropic.rs:接口适配 + 测试大量 `Arc::new` 包裹,无行为变化。 |
| crates/astrcode-ai/src/strict_tools.rs | +87/-81 | 机械适配 | 主体是 ToolOrigin 三分→两分适配(`Builtin` 断言全改 `Bundled`);测试从已删的 `astrcode_tools::registry::builtin_tools` 改为走 bundled 扩展注册管线收集工具,并放宽「恰好只有 terminal 被降级」的硬断言为「非空 + 校验通过 + 关键 coding 工具在 strict 子集」,这是删除 astrcode-tools 后必然且更稳健的改法;let-chain 改写占一部分行数。 |
| crates/astrcode-ai/src/wire/anthropic/body.rs | +64/-83 | 机械适配 | `&[LlmMessage]` → `&[Arc<LlmMessage>]` 贯穿;let-chain 改写;测试里 `ThinkingCapability` 从 config 字面量内联改为先绑定局部变量(clippy `needless_borrows_for_generic_args`/生命周期适配,无行为变化)。 |
| crates/astrcode-ai/src/wire/anthropic/parser.rs | +74/-27 | 核心功能 | 本批唯一的真实行为修复:usage 改为从 `message_start`(input/cache 字段)与多次 `message_delta`(output 增量)累计合并,在 stop_reason/message_stop 时才发送一次,并打上 `InputTokenAccounting::Components` 标记——旧实现只读 message_delta,会丢掉 message_start 里的 input/cache 统计。测试同步重写,有价值。 |
| crates/astrcode-ai/src/wire/openai/body.rs | +62/-58 | 机械适配 | Arc 化 + `stable_hash_hex` 改从 `astrcode_core::event` 导入(见 serialization.rs);测试机械适配。 |
| crates/astrcode-ai/src/wire/openai/parser.rs | +89/-84 | 核心功能 | 大头是 let-chain 改写(机械);实质改动只有一处:`extract_token_usage` 增加 `input_accounting: Some(Inclusive)`(OpenAI 的 input_tokens 含 cached,与 Anthropic 的 Components 语义区分),测试加断言。行数虚高,核心增量就几行。 |
| crates/astrcode-ai/src/wire/openai/serialization.rs | +13/-25 | 架构搬移 | 删除本地 `stable_hash_hex`(FNV-1a),改为复用 `astrcode_core::event::stable_hash_hex`(已确认存在于 core),纯去重搬移;其余 Arc 化与测试适配。 |

## 批次小结

整体都有价值,没有可删/可推迟的部分: astrcode-ai 是 ToolOrigin 收敛、LlmProvider 接口收敛(Arc 化、删 `generate` 便捷方法)和 astrcode-tools 删除的直接下游,这 10 个文件的适配是不可避免的。真正的新增价值集中在两点:① `LlmTokenUsage.input_accounting` 语义的建立(Anthropic=Components / OpenAI=Inclusive),修正了两家 cached token 口径混淆的隐患;② Anthropic parser 的 usage 累计合并修复了 input/cache 统计丢失。行数大头是 let-chain 语法现代化和测试 `Arc::new` 机械改写——这类风格改写混在功能 PR 里会增加 review 噪音,但均无副作用,不值得为此拆 PR。strict_tools 测试从硬编码「terminal 被降级」放宽为结构性断言是合理的,因为工具集合已从 astrcode-tools 迁到 bundled 扩展,不再稳定。

存疑项:无。
# 批次 03:CLI/TUI 适配与 bundled 扩展注册重构

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-bundled-extensions/Cargo.toml | +6/-1 | 配置 | 新增 coding、session-commands 两个 feature 及依赖、serde_json;配合 astrcode-tools 拆分,必要改动。 |
| crates/astrcode-bundled-extensions/src/lib.rs | +232/-101 | 核心功能 | 把 if-cfg 链式注册改成 BUNDLED_EXTENSION_CATALOG 目录表(id/default_enabled/factory/validate_config),discover 改走 lazy candidate,新增 validate_bundled_extension_configs 统一校验;消重后默认值唯一来源,值得。 |
| crates/astrcode-cli/Cargo.toml | +1/-1 | 机械适配 | 依赖 astrcode-context 换成 astrcode-extension-sdk(handle_event 需要 TransportProfile 等),跟随接口变化。 |
| crates/astrcode-cli/src/main.rs | +13/-5 | 核心功能 | BootstrapOptions 用 transport_profile 替换 disabled_extension_ids(ask-user 禁用逻辑移入 transport profile),server 子命令传 AuthenticatedHttp profile;跟随 bootstrap 接口的语义调整,有价值。 |
| crates/astrcode-cli/src/transport.rs | +1/-1 | 机械适配 | 同 main.rs:disabled_extension_ids → TransportProfile::default(),一行适配。 |
| crates/astrcode-cli/src/tui/app.rs | +17/-22 | 机械适配 | 删掉 sync_slash_filter_pub 包装(改 pub(super) 可见性)、四处 if-let 链式重写;纯风格/可见性整理,无行为变化,价值低但无害。 |
| crates/astrcode-cli/src/tui/app/handle_event.rs | +63/-69 | 核心功能 | ExtensionEvent→CustomEvent 改名适配、tool result 渲染增加 intent renderer 回退、AgentSessionStarted 的 tool_call_id 变 Option、is_compact_summary 从 Option+文本嗅探改为必填 bool;跟随协议/事件模型重构,附带测试更新,值得。 |
| crates/astrcode-cli/src/tui/command/slash.rs | +0/-9 | 核心功能 | 删除 /compact 命令(迁移到 session-commands 扩展),跟随内置命令外移,合理。 |
| crates/astrcode-cli/src/tui/custom_terminal.rs | +4/-4 | 机械适配 | 一处 if-let 链式合并,纯风格,无行为变化。 |
| crates/astrcode-cli/src/tui/ext/builtin.rs | +37/-1 | 核心功能 | 新增 intent_renderer_name:按 ToolResult 的 ToolPresentation intent 映射到内置 renderer(shell/write/grep/read),带单测;ToolPresentation 全链路在 TUI 侧的落点,有价值。 |
| crates/astrcode-cli/src/tui/mod.rs | +2/-11 | 机械适配 | 删 SlashCommand::Compact 执行分支 + sync_slash_filter_pub 改名调用,跟随 slash.rs/app.rs 变化。 |
| crates/astrcode-cli/src/tui/render/scrollback.rs | +14/-14 | 机械适配 | 扩展自定义消息渲染分支 if-let 链式合并,纯风格,无行为变化。 |
| crates/astrcode-cli/src/tui/store/child_agent.rs | +13/-26 | 测试 | 合并/重写两个单测(overflow summary 与错误标记合并为一个测试),覆盖不变,净删测试行数,无源码改动。 |
| crates/astrcode-cli/src/tui/store/session_picker.rs | +15/-23 | 测试 | 三个相对时间格式化单测合并为一个表驱动测试,覆盖等价,纯测试整理。 |
| crates/astrcode-cli/src/tui/terminal.rs | +10/-10 | 机械适配 | pending_viewport_area 的嵌套 if 改 if-let 链,纯风格,无行为变化。 |

## 批次小结

这批改动整体都有价值:真正承载功能的是 bundled-extensions 的目录表化重构(消重 + 统一 config 校验入口)、TUI 侧的 CustomEvent/intent renderer/is_compact_summary 适配,以及 /compact 外移——全部服务于 astrcode-tools 拆分和 ToolPresentation 全链路,属于主线改动的必要落点。占比不小的一批是 if-let 链式重写(app.rs、custom_terminal.rs、scrollback.rs、terminal.rs)和测试合并(child_agent.rs、session_picker.rs),属于风格/测试整理,与重构主线无因果关系,单独成 commit 或拆出去会让主线 diff 更清晰,但保留也无害。没有发现可疑或应删的改动。

存疑项:无。
# 批次 04:astrcode-context 上下文/压缩重构 + astrcode-client/eval 适配

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-client/src/transport.rs | +11/-4 | 核心功能 | 新增 JSON-RPC `jsonrpc != "2.0"` 版本门禁,跳过不符合协议的服务端 stdout 行并 warn;顺带把嵌套 if 改为 let-chain。小但有实际健壮性价值。 |
| crates/astrcode-context/Cargo.toml | +9/-1 | 配置 | tokio 生产特性从 `rt-multi-thread, macros` 收紧为 `rt`(macros 移到 dev-deps),新增 criterion dev-dep 与 `[[bench]] context_snapshot`。为配套新基准,合理。 |
| crates/astrcode-context/benches/context_snapshot.rs | +68/-0 | 测试 | 新增 criterion 微基准,覆盖 `from_transcript`/`from_shared_transcript`/`request_messages`,直接服务本 PR 的 Arc 共享化改动,注释写清了对照意义。有价值。 |
| crates/astrcode-context/src/compaction/assemble.rs | +14/-14 | 架构搬移 | 常量与函数从 `pub`/`pub(crate)` 收紧为 `pub(super)`(压缩边界收敛到 compaction 模块内部),字符串字面量随新版 rustfmt 重排。无行为变化。 |
| crates/astrcode-context/src/compaction/mod.rs | +95/-103 | 核心功能 | `llm_api_failed: bool` 升级为三态 `LlmCompactAttempt`(NotAttempted/Succeeded/Failed),区分「没调 LLM」与「LLM 调用失败」,语义更准确;一批 `pub` 函数随边界收敛降级为私有;测试从旧 mod 级测试迁移合并为两个覆盖 scratchpad 清理与 prompt 契约的新测试(纯搬移,无新覆盖)。 |
| crates/astrcode-context/src/compaction/parse.rs | +8/-6 | 架构搬移 | `ParsedCompactOutput`/`CompactParseError`/`parse_compact_output` 等从 `pub` 收紧为 `pub(super)`,纯可见性收敛。 |
| crates/astrcode-context/src/compaction/post_compact.rs | +151/-376 | 核心功能 | 重写为泛型「typed retained context」:context crate 只负责 `CompactRetainedContext::{File,Note}` 的预算与渲染(`append_compact_retained_context`),删掉 200+ 行的 read/agent 工具消息扫描(`recent_read_paths`、`agent_status_note`、`scan_tool_messages`),领域发现逻辑迁往 `astrcode-extension-coding/src/compact.rs`。边界划分清晰,是 S5R 3.0 扩展化的代表性改动。 |
| crates/astrcode-context/src/context.rs | +192/-102 | 核心功能 | `ContextSnapshot` 改为 `Vec<Arc<LlmMessage>>` + 平行 `origins`,新增 `from_shared_transcript`/`retained_transcript_messages`(compact 保留尾部及其 durable 元数据)和 `estimate_own_input_tokens`(免物化请求 Vec 的锚点快路径);marker 检测函数搬到 `astrcode_core::compaction`;`PostCompactEnricher` trait 删除(迁扩展 hook)。是 Arc 零拷贝共享与 typed compact 边界的主战场,价值高。 |
| crates/astrcode-context/src/context_assembler.rs | +23/-14 | 机械适配 | `ContextPrepareInput.messages` 从 `Vec<LlmMessage>` 改为 `&[Arc<LlmMessage>]`,跟随 core 的 `provider_visible_shared_messages` 归一化,稳态不复制消息体;测试相应改造。跟随主改动的必要适配。 |
| crates/astrcode-context/src/lib.rs | +9/-7 | 架构搬移 | compact marker/is_* 检测改从 `astrcode_core::compaction` re-export;`post_compact_enricher` 模块下线;新增 `CompactRetainedContext` 导出。纯 re-export 层调整。 |
| crates/astrcode-context/src/post_compact_enricher.rs | +0/-470 | 架构搬移 | 整文件删除:原 `DefaultPostCompactEnricher`(plan/skills/agent status 采集)的职责迁到扩展侧(已确认 `astrcode-extension-coding/src/compact.rs` 与 SDK hook 承接 `CompactRetainedContext`)。属边界搬移而非功能丢失。 |
| crates/astrcode-context/src/prompt_engine.rs | +4/-5 | 机械适配 | 测试里 `ToolOrigin::Builtin`→`Bundled`、`ExecutionMode` 字段删除,跟随 tool 定义契约变化。 |
| crates/astrcode-context/src/prompt_engine/provider_messages.rs | +6/-6 | 机械适配 | 嵌套 if 改 let-chain,纯语法现代化。 |
| crates/astrcode-context/src/prompt_engine/tool_summary.rs | +8/-72 | 核心功能 | 删掉 "Builtin Tools" 分组及内置工具的硬编码排序表和一句话描述表(`tool_summary_rank`/`tool_short_description`),builtin 工具并入 Extension Tools 分组——配套内置工具 crate 删除,system prompt 渲染行为真实变化,是有意的产品行为改动。 |
| crates/astrcode-context/src/token_budget.rs | +22/-7 | 机械适配 | `build_prompt_snapshot`/`estimate_request_tokens_with_prompt` 泛化为 `Borrow<LlmMessage>` 以兼容 Arc;新增 `PROVIDER_COUNT_GATE_RATIO = 0.9` 常量(带不变式注释,真正的消费者在本 crate 之外)。主体是适配。 |
| crates/astrcode-eval/src/client.rs | +1/-1 | 机械适配 | 跟随会话 DTO 变化:`body.phase` → `body.control.phase`。 |
| crates/astrcode-eval/src/judge.rs | +12/-12 | 机械适配 | 两处嵌套 if 改 let-chain,无行为变化。 |
| crates/astrcode-eval/src/runner.rs | +4/-4 | 机械适配 | checkpoint 写入的嵌套 if 改 let-chain,无行为变化。 |
| crates/astrcode-eval/src/swebench_instance.rs | +1/-5 | 测试 | 把 `official_image_pull_has_bounded_attempt_duration` 合并进确定性命名测试并改名;覆盖不变,纯整理。 |

## 批次小结

这批改动整体都有价值,是 S5R 3.0 在 context 层的核心落点:一是 `ContextSnapshot`/assembler 的 Arc 消息体共享(配合新基准验证零拷贝收益),二是 post-compact 职责从 context crate 下沉到扩展(typed `CompactRetainedContext` 边界,删除近 1000 行领域采集代码),三是 system prompt 去掉 Builtin Tools 分组。删除的 `post_compact_enricher.rs` 已确认有扩展侧承接,不是功能丢失。可挑剔的点不多:eval 与 provider_messages 的 let-chain 改写属于风格性 churn,可单独成 commit 但不至于要求拆分;`token_budget.rs` 新增的 `PROVIDER_COUNT_GATE_RATIO` 在本 crate 内无消费者,依赖其他 chunk 的文件实际使用,若全 PR 内无人引用则应推迟。无存疑项。
# 批次 05:astrcode-core 领域契约(事件/LLM/工具/配置)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-core/Cargo.toml | +1/-0 | 机械适配 | 新增 `tokio-util` 依赖,供 tool.rs 的 `CancellationToken`(plan/execute 取消信号)使用,跟随主线改动,必要。 |
| crates/astrcode-core/src/compaction.rs | +51/-1 | 架构搬移 | `COMPACT_SUMMARY_MARKER`/`POST_COMPACT_CONTEXT_MARKER` 及 `is_compact_summary_*`/`is_synthetic_context_message` 是从 baseline `astrcode-context/src/context.rs` 原样收拢到 core 的搬移(消费方 import 随之改);真正新增是 `CompactStrategy::trigger()`/`keep_recent_turns()`,把策略→触发源的对应关系收敛进类型,避免调用方传两个可能冲突的事实,有价值。 |
| crates/astrcode-core/src/config/defaults.rs | +12/-6 | 核心功能 | 新增 `extension_data_dir(base, extension_id)` 默认目录原语(扩展数据目录);删除 `DEFAULT_SHELL_TIMEOUT_SECS`(shell 工具随 astrcode-tools 迁出,超时改走 ToolExecutionPolicy);测试里 `set_var` 改 `unsafe` 块是 Rust 2024 API 变更的适配。 |
| crates/astrcode-core/src/config/effective.rs | +1/-4 | 机械适配 | 删除 `AgentSettings.shell_timeout_secs` 字段及默认值,并把文档中 `ExtensionCtx` 改名为 `ExtensionStartContext`,跟随 shell 工具迁出与 SDK 改名。 |
| crates/astrcode-core/src/config/mod.rs | +17/-11 | 机械适配 | `pub use xxx::*` 通配 re-export 改为显式符号列表(API 表面收紧,是好实践);删除 `ConfigStoreError::Serialization` 变体,属 config.json 旧兼容删除主线的收尾;模块文档同步去掉 config.json fallback 描述。 |
| crates/astrcode-core/src/config/raw.rs | +1/-3 | 机械适配 | 删除 `RuntimeSection.shell_timeout_secs` 配置项;注释里 `ctx.config` 改 `ctx.config()`。跟随主线。 |
| crates/astrcode-core/src/config/resolve.rs | +14/-18 | 机械适配 | 删除 shell_timeout_secs 的解析与 overlay merge(主线);但两处 `if let` 嵌套改 let-chains 是纯风格重写,与主线无关,混在同一 diff 里增加 review 噪音。 |
| crates/astrcode-core/src/event.rs | +87/-18 | 核心功能 | 引入 `EventPublisher` trait 与 `EventSender::send_confirmed` + `EventDeliveryReceipt`(区分 Accepted/LivePublished/Persisted),支撑扩展事件的确认投递语义;`EventSendError` 由单元结构体升级为 `Closed/Full/PublishFailed` 枚举;删除 `SystemPromptSource::is_native` 使 `source` 字段总是序列化(持久化格式收紧,读路径本就要求该字段,无回归)。 |
| crates/astrcode-core/src/event/envelope.rs | +10/-1 | 机械适配 | 新增 `EventPayload::custom_event()` 访问器,统一 Durable/Live 两变体的 custom event 取值,跟随 `ExtensionEventData`→`CustomEventData` 改名。 |
| crates/astrcode-core/src/event/fingerprint.rs | +117/-0 | 核心功能 | 新文件:手写 FNV-1a 稳定哈希 + `transcript_prefix_fingerprint`,为 `TranscriptRewritten` 提供跨进程稳定的乐观并发指纹;注释明确序列化形状即持久化契约,三个测试锁定算法与输入格式,质量高。 |
| crates/astrcode-core/src/event/payload.rs | +109/-12 | 核心功能 | 事件契约主干改动:`ExtensionEventData`→`CustomEventData` 并新增 `audience`/`causation_id`/`cascade_depth`(扩展事件归因与级联深度);新增 `StepStarted`/`StepCompleted` durable 事件;`TranscriptRewritten` 增加 `source_fingerprint`;`messages` 类型从 `Vec<LlmMessage>` 升级为 `Vec<TranscriptMessage>`;`AgentSessionSpawned.tool_call_id` 放宽为 `Option`(非工具触发的 spawn);附序列化不变量测试。 |
| crates/astrcode-core/src/event/tests.rs | +19/-0 | 测试 | 新增 `agent_session_spawned_accepts_missing_tool_call_id`,锁定 tool_call_id 可缺省的新契约,与 payload.rs 改动配套。 |
| crates/astrcode-core/src/lib.rs | +2/-0 | 机械适配 | 导出 `session_lineage` 模块及模块文档一行。 |
| crates/astrcode-core/src/llm.rs | +494/-79 | 核心功能 | 本批最大改动,均有明确动机:(1) `TranscriptMessage`/`SharedTranscriptMessage` 给 transcript 消息挂 origin 元数据并以 Arc 跨读模型共享;(2) `provider_visible_entries` 用 trait 抽象让 owned/Arc 两条路径共用归一化逻辑,并修复 mid-tool-round 用户插话被截断的 bug(改为缓冲到工具轮结算后移动,附测试);(3) `LlmInputTokenAccounting`(Inclusive/Components)+ `non_cached_tokens()`,修正不同 provider 缓存计数的口径;(4) `LlmRequest.messages` 改 `Vec<Arc<LlmMessage>>`、`LlmProvider::generate` 旧入口删除;(5) `LlmProviderBindings` 固定一次操作的主/小模型组合。 |
| crates/astrcode-core/src/llm/thinking.rs | +21/-21 | 存疑 | 整个 diff 全是 `if let` 嵌套改 let-chains 的纯风格重写(+21/-21 完全对称),与 S5R 主线无任何关系,属顺手格式化。建议拆到独立风格 PR 或 revert,避免污染本 PR 的 review 面。 |
| crates/astrcode-core/src/llm/token_estimate.rs | +10/-4 | 机械适配 | `estimate_request_tokens` 泛型化为 `Borrow<LlmMessage>` 以同时接受 owned 与 `Arc<LlmMessage>` 切片(跟随 llm.rs 的 Arc 化);测试适配 `ToolOrigin::Builtin→Bundled` 及 `execution_mode` 字段删除。 |
| crates/astrcode-core/src/permission.rs | +2/-2 | 测试 | 测试改名并把 `use super::*` 收窄为 `use super::ApprovalMode`,顺手清理,无行为变化,价值低但无害。 |
| crates/astrcode-core/src/session_lineage.rs | +137/-0 | 架构搬移 | 新文件:`collect_parent_chain` 带环检测的父链上溯原语,是 baseline `astrcode-server/src/child_session.rs` 与 `astrcode-session/src/session_error.rs` 两处重复实现的公共抽取(host/server 错误消息逐字保留,注释已注明是线缆契约);调用方注入 parent 解析策略,测试覆盖环/自环/解析失败,抽取合理。 |
| crates/astrcode-core/src/tool.rs | +263/-45 | 核心功能 | 工具契约主干:`ToolOrigin` 精简为 Bundled/Extension(配合 astrcode-tools 删除);`ToolExecutionPolicy`(mode+timeout)取代 `ToolDefinition.execution_mode`;`Tool::plan()` 取代 `resource_accesses()` 并引入 `ToolPlanningContext`;`ToolPresentation` 呈现 intent 全链路的 core 侧定义;artifact 读取从 path/char 偏移改为 artifact_id/byte 偏移;`SessionOperations` 新增 `queue_or_start`/`defer_context` 默认 Unsupported;`ToolCallScope` 增加 `turn_id`、`ToolExecutionContext` 接 `ResourceLease` 与 cancellation;`CreateSessionRequest` 删 `working_dir`、`tool_call_id` 改 Option。与 PR 主线一一对应。 |
| crates/astrcode-core/src/tool/access.rs | +174/-7 | 核心功能 | 资源租约模型:`ResourceAccess::All` 拆为 `Host(HostResource)`/`Opaque`,新增 `ToolPlan`(plan 阶段产物)与 `ResourceLease`(权限决策后签发、HostRouter 侧 `permits` 再校验)及覆盖判定逻辑,测试覆盖路径递归/操作域边界,是 S5R 权限模型的核心原语。 |
| crates/astrcode-core/src/tool/read_image.rs | +1/-1 | 文档 | 模块注释更新:生产方从已删除的 `astrcode_tools` 改为泛指的 extension-authored tools;顺手把中文注释换成了英文,与文件其余中文注释风格不一,但无实质影响。 |
| crates/astrcode-core/src/tool/selection.rs | +39/-0 | 核心功能 | 新增 `validated_tool_names` + `EmptyToolNameError`:工具名 trim/拒空/去重排序的边界校验从各调用点(HTTP、host invoke)下沉到 core 共享,附测试,符合「先重用」原则。 |

## 批次小结

这批是 PR 的契约地基,整体价值高:core 承担了 S5R 3.0 的全部领域原语——资源租约(access.rs)、工具 plan/execute 分离与呈现 intent(tool.rs)、transcript 消息 origin 与 Arc 共享(llm.rs)、确认投递事件通道(event.rs)、rewrite 指纹(fingerprint.rs)、扩展自定义事件归因(payload.rs)、配置面的 shell_timeout_secs 退场。绝大部分改动与主线清单一一对应,测试(指纹锁定、lease 覆盖、mid-tool-round 插话、序列化不变量)也跟得上。

可斟酌的部分:
- `llm/thinking.rs` 整文件是与主线无关的 let-chains 风格重写,`config/resolve.rs` 也混入两处同类重写——建议拆出或 revert,它们只增加 diff 噪音。
- `permission.rs` 的测试改名/收窄 import 属顺手清理,保留无妨但非必要。
- compaction.rs 的 marker 函数是从 astrcode-context 的纯搬移,审计时应按搬移看待,新增价值仅在 `CompactStrategy` 的两个方法。
- 持久化契约放宽/收紧点(`AgentSessionSpawned.tool_call_id` 变 Option、`source`/`metadata` 字段总是序列化)均有对应测试锁定,未发现兼容性疑点;旧数据侧 baseline 本来就要求这些字段参与反序列化,无回归风险。
# 批次 06:agent-tools / ask-user / channels 扩展适配新 SDK

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-agent-tools/src/agent.rs | +2/-4 | 机械适配 | 删掉手写的 CRLF/BOM 归一化,改用 SDK 的 `frontmatter::normalize_markdown`,复用共享原语,符合规范,纯收益。 |
| crates/astrcode-extension-agent-tools/src/lib.rs | +281/-195 | 核心功能 | 大头是适配新 SDK(manifest builder、`ToolContext`、`ctx.host().session_control()`、`ExtensionToolDefinition`);但夹带实质新功能:新增 `AgentPreCompactHandler` + `agent_status`/`truncate_agent_result`(compaction 时把 agent 任务运行状态注入 retained context,约 100 行)并声明 `SessionHistory` capability。适配之外的新逻辑有测试覆盖,值得保留;唯一可挑剔的是多个老测试被合并成一个大测试,失败定位粒度变差,但覆盖面未减。 |
| crates/astrcode-extension-ask-user/Cargo.toml | +3/-0 | 配置 | 新增 dev-dependency 引入 `astrcode-extension-sdk` 的 `testing` feature,配合 lib.rs 测试改用 `ToolContextBuilder`/`HttpContextBuilder`,必要。 |
| crates/astrcode-extension-ask-user/src/lib.rs | +119/-104 | 机械适配 | 全面切到新 API:manifest builder 声明 transport/capability、`ToolContext`/`HttpContext` 取代散参、事件注册改 `declare_custom_event`、`on_event`→`on_lifecycle`;参数解析失败行为从返回 ToolResult::error 改为 `context.arguments()?` 抛 ExtensionError(错误上抛层级变化,属新契约的既定语义)。测试用 builder 重写,行为断言不变。 |
| crates/astrcode-extension-ask-user/src/model.rs | +1/-2 | 机械适配 | `ToolDefinition` 移除 `execution_mode` 字段(执行策略改由注册侧 `ToolExecutionPolicy` 表达),跟随 SDK 结构变化。 |
| crates/astrcode-extension-ask-user/src/registry.rs | +21/-17 | 机械适配 | `ExtensionEventSink`→`CustomEventEmitter`(同步 `try_emit`),新增 `custom_event_declarations()` 用 builder 声明两个 `GlobalLive` 事件;错误包装从序列化细错误简化为 `Internal(error.to_string())`,信息略损但可接受。 |
| crates/astrcode-extension-channels/src/lib.rs | +199/-200 | 存疑 | 主体是适配:`SessionOperations`→`SessionControlClient`(create_root/submit_root_turn/root_state),capability 从 `SessionControl` 收紧为 `InputDelivery`,配置加载走 `deserialize_or_default` + 新增 `validate_config`,env 测试改 unsafe set_var 加锁串行化,这些都合理。疑点有二:(1) `on_config_changed` 热更新路径整体被删(连同 `update_config`),运行中改 telegram 配置不再生效,需确认 host 是否改用 stop/start 重启扩展来应用配置,否则是功能回退;(2) `TelegramChannelConfig.working_dir` 与 `startup_working_dir` 被删,`create_root()` 不再传工作目录,telegram 会话的 working dir 改由 host 默认决定——测试已断言旧 `workingDir` 配置被拒绝,说明是有意的,但属于用户可见的行为变更,应确认文档/配置说明同步更新。 |

## 批次小结

这批是 bundled 扩展跟随 SDK 重写的适配,整体方向一致且质量在线:manifest builder、context 对象化、custom event 声明化、capability 收紧(channels 从 SessionControl 降到 InputDelivery)都符合 S5R 3.0 的契约演进,测试也随之用新的 testing/builder 脚手架重写,没有发现为省事删测试的情况。真正的新增价值集中在 agent-tools 的 pre-compact 状态注入。可以拆分/推迟的点:agent-tools/lib.rs 把「接口适配」和「PreCompact 新功能」混在一个文件一次提交,review 负担略增但尚可接受;channels 的 `on_config_changed` 删除若属功能回退,应补一个等价机制或在 PR 描述中明说。
# 批次 07:astrcode-extension-coding(内置编码工具迁为扩展)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-coding/Cargo.toml | +13/-0 | 配置 | 新 crate 清单,仅依赖 extension-sdk + serde/regex,符合「内置插件只依赖插件系统」的约束,必要。 |
| crates/astrcode-extension-coding/src/compact.rs | +150/-0 | 核心功能 | 基线无 PreCompactHandler:新增 pre-compact 钩子,把最近成功 read 的文件内容作为 retained_context 注入压缩,是本轮新增的会话上下文保留能力,含单测,有价值。 |
| crates/astrcode-extension-coding/src/files/edit.rs | +226/-0 | 架构搬移 | 旧 astrcode-tools/files/edit.rs(306 行)的实际编辑逻辑迁到 host(HostWorkspaceEditRequest),本文件是瘦适配层;注意旧实现的 clean_quotes 引号归一化被移除,新测试明确断言弯引号原样保留,属有意行为变化。 |
| crates/astrcode-extension-coding/src/files/mod.rs | +79/-0 | 架构搬移 | 6 个文件工具的注册装配 + absolute_path/text_change_metadata 两个共享小函数,随迁移必要的胶水。 |
| crates/astrcode-extension-coding/src/files/patch.rs | +136/-0 | 架构搬移 | 旧 patch.rs 881 行的 diff 解析/应用全部迁到 host(apply_patch + analyze_unified_diff_paths),本文件只做参数校验、plan 资源声明和结果渲染,大幅瘦身合理。 |
| crates/astrcode-extension-coding/src/files/read.rs | +236/-0 | 架构搬移 | 旧 read.rs(344 行)的文件 IO 迁到 host workspace.read,本文件保留行号渲染、char/line 双层分页和 metadata;旧版对持久化 tool-result 路径的透明读取被拆出去(见 tool_result.rs),职责更清晰。 |
| crates/astrcode-extension-coding/src/files/search.rs | +452/-0 | 架构搬移 | 合并旧 glob.rs(235 行)+ grep.rs(569 行)为 host glob/grep 的适配层,保留分页、scan_truncated 提示、fileType 映射等既有语义,渲染与校验逻辑基本是搬移。 |
| crates/astrcode-extension-coding/src/files/tool_result.rs | +121/-0 | 核心功能 | 新增 read_tool_result 工具:基线只有内部按路径读 artifact 的能力,本文件把它暴露成按 artifactId + byteOffset 分页的工具(走 host ToolResultRead capability),是新功能。 |
| crates/astrcode-extension-coding/src/files/write.rs | +83/-0 | 架构搬移 | 旧 write.rs(167 行)的写盘逻辑迁到 host,本文件是纯适配 + metadata 组装。 |
| crates/astrcode-extension-coding/src/lib.rs | +205/-0 | 架构搬移 | 扩展入口:manifest(capability 声明)、register 装配、shellTimeoutSecs 配置校验/应用(1–600s)及两个注册/配置测试;配置热更新是随扩展化新增的小能力,整体属迁移必要骨架。 |
| crates/astrcode-extension-coding/src/process/mod.rs | +17/-0 | 架构搬移 | shell 工具注册胶水,17 行,必要。 |
| crates/astrcode-extension-coding/src/process/shell.rs | +693/-0 | 架构搬移 | 合并旧 shell_tool/*(mod/process/definition/output/background)与 background_shell:pipefail 严格管道、sudo 失败检测、30s 自动转后台等逻辑基线均已有(旧 shell_tool/mod.rs、process.rs),此处改为经 host process start/read/promote API 实现,并带 3 个单测;体量大但几乎全是搬移 + 重接管线。 |

## 批次小结

这批是「astrcode-tools 删除、功能迁到 astrcode-extension-coding」的落地侧,12 个文件全是新增(对应旧 crate 的删除在别的批次)。整体有价值:10/12 是架构搬移,文件逻辑迁到 host 后扩展侧只剩参数校验 + plan + 渲染的薄壳,行数较旧实现明显瘦身(edit 306→226、patch 881→136、write 167→83),符合把内置工具降为普通扩展的目标;真正的新增功能只有两处——compact.rs 的 pre-compact 文件保留钩子和 tool_result.rs 的 read_tool_result 分页读取工具,两者都小而聚焦、带测试,不建议拆出本 PR(它们依赖同 PR 引入的 PreCompactHandler / ToolResultRead host 能力,拆了无法编译)。无可删/可推迟项。两点行为变化值得在 review 中确认是有意的:edit 去掉了 clean_quotes 引号归一化(新测试表明是有意);read 不再透明支持持久化 artifact 路径,需确认没有依赖该隐式行为的调用方。
# 批次 08:extension-goal / extension-mcp 适配新 SDK 与 goal 结算模型

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-goal/Cargo.toml | +0/-1 | 配置 | 删除 parking_lot 依赖:GoalRuntime 的 RwLock 随新 SDK(ExtensionHost 直接注入)一起删掉,依赖确实不再使用,干净的跟随清理。 |
| crates/astrcode-extension-goal/src/lib.rs | +263/-392(共 655 行变动) | 核心功能 | 双重性质:一是机械跟随新 SDK(manifest builder、ToolContext/ctx.arguments() 边界解析参数、删 GoalRuntime 改用 ExtensionHost);二是实质改动——provider 注入从「handle 里 take pending + 立即落盘」改为 prepare/acknowledge 结算模型(pending 变 contribution id,ack 才清除),消除了 prompt 已取出但消息未真正入请求时的状态丢失。删掉了注释已失效的 goal_root_from_session_base,路径改走 ExtensionPaths::session_data_dir。值得。 |
| crates/astrcode-extension-goal/src/store.rs | +97/-74 | 核心功能 | 配合上条:continuation/budget_limit pending 从 bool 改为 contribution id(Option<String>),新增 revision 与 budget_transition_contribution_id,ack 幂等且不会用旧 id 结算新状态(有专门测试覆盖);load/save 复用 hostpaths::read_json_state/write_json_state/update_json_state,删掉手写的读文件+原子写样板。注意:pending 字段类型变化且去掉了 serde(default),旧 goal.json( bool 格式)会反序列化失败——见存疑。 |
| crates/astrcode-extension-mcp/Cargo.toml | +2/-1 | 配置 | tokio features 收敛:rt-multi-thread/macros 挪到 dev-dependencies,库本身只留 rt。合理的编译依赖瘦身,低风险。 |
| crates/astrcode-extension-mcp/src/config.rs | +5/-4 | 机械适配 | 删除 r#type 上冗余的 serde alias="type"(serde 对 raw identifier 本就按 "type" 匹配,alias 无实际作用);resolve_cwd 改为 match 表达式风格。无行为变化。 |
| crates/astrcode-extension-mcp/src/http_client.rs | +8/-8 | 机械适配 | 纯 let-chain(if let ... && ...)语法重写,逻辑等价。 |
| crates/astrcode-extension-mcp/src/lib.rs | +94/-75 | 机械适配 | 跟随新 SDK 接口重写注册面:manifest builder、ExtensionStartContext/StopContext、ToolDiscoveryContext 返回 Result、DiscoveredTool builder(execution_mode 从 ToolDefinition 字段迁到 with_execution_policy(ToolExecutionPolicy::PARALLEL))、tool_metadata 集中注册改为 per-tool prompt_metadata。少量新语义:McpToolHandler::plan 返回 ToolPlan::opaque()(MCP 工具无法声明可信资源契约,审批保守化),有注释说明,方向正确。 |
| crates/astrcode-extension-mcp/src/pool.rs | +61/-29 | 核心功能 | stdio 子进程资源回收的实质修复:spawn 加 kill_on_drop(true);stdout/stderr 任务从「挂起不管」的 _task 句柄改为 Mutex<Option<JoinHandle>>,Drop 时 abort,shutdown 时带超时 join、失败 warn。修复了进程池条目回收后 I/O 任务泄漏的问题,是本批里独立价值最高的一处。 |
| crates/astrcode-extension-mcp/src/protocol.rs | +6/-1 | 机械适配 | JsonRpcError 不再从 astrcode_extension_sdk::protocol 引入(SDK runtime/protocol 删除),改为本地定义同名字结构体。跟随 SDK 裁剪的搬移,字段一致。 |
| crates/astrcode-extension-mcp/src/search.rs | +5/-7 | 测试 | 测试适配:去掉 ExecutionMode import 和定义里的 execution_mode 字段,合并两个小测试。跟随 ToolDefinition 结构变化,无新覆盖。 |

## 批次小结

整批改动都有价值,没有可删的部分。两条主线:(1) goal 扩展借 SDK 重写之机把 provider 提示注入升级为 prepare/acknowledge 结算模型(store.rs 的 contribution id + 幂等 ack + 专项测试),修掉了「pending 被取出但消息未实际入请求」的丢提示窗口,是真实的健壮性提升;(2) mcp 扩展以机械适配为主,但 pool.rs 的 stdio 任务回收(Drop abort + kill_on_drop + 带超时 shutdown)是独立成立的 bug 修复,可考虑拆成独立 commit 以便回溯。config.rs/http_client.rs 的 let-chain 重写和 alias 清理属于顺手 tidy,占比小,可以接受。

### 存疑项

- **crates/astrcode-extension-goal/src/store.rs**:`GoalState.continuation_prompt_pending`/`budget_limit_prompt_pending` 从 `#[serde(default)] bool` 改为无 default 的 `Option<String>`,旧的 `goal.json`(值为 `true/false`)在 `load()` 时会直接 parse 失败并返回错误,导致 `/goal` 与 goal 工具对存量会话全部报错而非静默恢复。GOAL_SCHEMA_VERSION 未 bump、无迁移逻辑。虽然 pending=true 只在崩溃/中断瞬间短暂存在、触发概率低,但按仓库「跨持久化边界的值要重新校验、不要用错误掩盖可恢复场景」的惯例,建议要么给两字段加自定义兼容反序列化(bool→None),要么 bump schema version 让旧文件走明确的 unsupported 报错路径(当前是裸 serde 错误,信息不友好)。
# 批次 09:astrcode-extension-memory / astrcode-extension-mode(SDK 接口迁移 + generation 化改造)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-memory/Cargo.toml | +2/-1 | 配置 | tokio 的 rt-multi-thread 挪到 dev-dependencies(生产只需 rt),依赖收窄,合理。 |
| crates/astrcode-extension-memory/src/config.rs | +4/-6 | 机械适配 | 改用 SDK 新 API `deserialize_or_default()`,语义不变(仍回退默认),纯接口跟随。 |
| crates/astrcode-extension-memory/src/handlers.rs | +349 行 diff(净删) | 核心功能 | ToolHandler 迁到新 ToolContext/plan(ResourceAccess)接口;删除本地 MemoryPipelineCoordinator(移交 workers.rs);memory_save 改为发 memory.created 事件 + workers.request_pipeline。注意:memory.deleted 事件 payload 键从 `deleted_count` 改为 `deletedCount`,属线缆格式变更,若有下游消费者需确认。 |
| crates/astrcode-extension-memory/src/index.rs | +2/-2 | 机械适配 | 删 turn_end serde alias(旧格式兼容层)、收窄 find_similar_record_index 可见性,配合无旧格式包袱的新存储。 |
| crates/astrcode-extension-memory/src/lib.rs | +133 行 diff | 核心功能 | Extension 迁到 manifest()/ExtensionStartContext 新生命周期;服务获取从 OnceLock<ExtensionHostServices> 改为 ctx.host()/ctx.paths();新增 validate_config。主要是接口迁移,价值在于跟上 SDK 新契约。 |
| crates/astrcode-extension-memory/src/pipeline.rs | +107 行 diff | 机械适配 | SessionQuery→SessionInspectClient、LlmProvider→ModelClient(small_chat_collected)接口替换;map_batch_to_candidates 去掉不必要的 Result 包装;extract_conversation 适配新 read model。无行为变化。 |
| crates/astrcode-extension-memory/src/prompts.rs | +10/-16 | 测试 | 三个小测试合并成一个,断言内容不变;纯测试整理,价值一般但无害。 |
| crates/astrcode-extension-memory/src/scope.rs | +1/-2 | 文档 | 模块注释改为「runtime 归属的 extension data 根目录」,跟随路径归属变化,只改注释。 |
| crates/astrcode-extension-memory/src/store.rs | +121 行 diff | 核心功能 | MemoryStorePool 新增 set_root OnceLock,数据根由扩展写死 `~/.astrcode/...` 改为 start 时由 runtime 注入;复用 hostpaths::read_json_state/write_json_state 删重复代码;补 set_root 幂等/冲突测试。属数据根归属架构调整,配套测试到位。 |
| crates/astrcode-extension-memory/src/turn_recall.rs | +165 行 diff | 核心功能 | 内存 ProjectRecallBuffer 改为落盘 ProjectRecallStore(state.json + contribution id + acknowledge 精确撤销),配合新 ProviderContributionHandler prepare/acknowledge 结算模型,热重载/崩溃后不丢注入、旧 ack 不会误清新版;测试改为覆盖 reload 与精确 ack。是本次结算语义重构的实质部分。 |
| crates/astrcode-extension-memory/src/workers.rs | +535/-0(新文件) | 核心功能 | 新增 generation 持有的 MemoryWorkers:store 写操作走 mpsc 单 worker 序列化(upsert/append/delete/preload),pipeline 走 PipelineQueue(ready/pending 合并,Drop 自动 complete);shutdown 后拒绝新命令。把原 handlers.rs 里的 coordinator+spawn 逻辑重写为生命周期清晰的后台任务,含一个覆盖合并与 shutdown 语义的集成测试。有价值。 |
| crates/astrcode-extension-mode/Cargo.toml | +1/-0 | 配置 | 新增 uuid 依赖,用于 PendingModeTransition 的 contribution id,配套改动。 |
| crates/astrcode-extension-mode/src/catalog.rs | +4/-23 | 存疑 | 删无读取方的 description 字段、收窄可见性、删冗余测试,均合理;但 PLAN_RESTRICTED_TOOLS 从 ["write","edit","patch","shell","terminal"] 删掉 "terminal",是行为变更——若某处仍注册名为 terminal 的工具,plan 模式将不再拦截它。需确认 terminal 工具是否已改名/删除,否则应补回。 |
| crates/astrcode-extension-mode/src/lib.rs | +207 行 diff | 核心功能 | 迁到 manifest/ToolContext/ProviderContribution 新接口;路径改由 ctx.paths().session_data_dir() 提供(删 require_session_base);新增 ModePreCompactHandler:compact 时把 plan 内容作为 retained_context 保留,属新增功能;provider 注入改为 prepare/acknowledge 结算。整体是接口迁移+一个实质性新功能。 |
| crates/astrcode-extension-mode/src/store.rs | +98 行 diff | 核心功能 | pending_transition_context 改为带 uuid 的 PendingModeTransition,新增 acknowledge_mode_transition(旧 id 不能清新 transition);删旧 camelCase serde alias;复用 hostpaths::read/write/update_json_state;补「重试保留同一 contribution/旧 ack 不误清」测试。与 memory 的 recall 结算同款,是结算语义核心改动。 |
| crates/astrcode-extension-mode/src/tools.rs | +111 行 diff | 机械适配 | handle_* 入参从 serde_json::Value 改为已反序列化的 Args 结构体(解析上移到 ToolContext::arguments);删 ExecutionMode 字段;测试跟随改 serde_json::from_value。无行为变化。 |

## 批次小结

这批改动整体有价值,是两个扩展对 S5R 3.0 SDK 新契约的完整迁移,并包含三块超出机械适配的实质内容:

1. **generation 化与数据根注入**:memory 的 workers.rs(新增 535 行)把散落的后台任务/coordinator 收敛为 start/stop 生命周期明确的 worker;store.rs 的 set_root 把数据目录从扩展写死改为 runtime 注入,配套测试齐全。
2. **provider 注入的 prepare/acknowledge 结算**:memory/turn_recall.rs 与 mode/store.rs 把「一次性内存 buffer / 注入后即删」改为落盘 + contribution id 精确撤销,修复热重载丢注入和旧 ack 误清新版的竞态,测试直接覆盖失败模式,是高质量改动。
3. **mode 的 PreCompact 保留 plan**(lib.rs 新增 ModePreCompactHandler):小但独立的新功能。

可改进/需跟进项:

- **catalog.rs 删 "terminal" 是唯一存疑点**:plan 模式拦截清单变窄属行为变更,diff 内看不到依据,建议确认 terminal 工具已不存在,否则补回。
- **memory.deleted payload 键 deleted_count→deletedCount**(handlers.rs):线缆格式变化,若有前端/其他扩展消费该事件需同步确认。
- prompts.rs 的测试合并价值边际,但无害,不要求拆分。
- 机械适配文件(config/pipeline/tools/scope 等)都是必要跟随,不建议删或推迟。
# 批次 10:astrcode-extension-sdk 的 extension 契约层(lifecycle/hook 上下文/注册表/authoring builder)

本批是 S5R 3.0 重写中「扩展作者可见契约」的部分:旧 `authoring_runtime.rs` 的 `Extension`/`ExtensionCtx` 被拆成 `lifecycle.rs`(trait)+ `call_context.rs`(host 归因上下文)+ `host` 模块(能力裁剪后的 host 客户端);hook 上下文全面改为 `HookContext<P>`/`HookInput<P>` 泛型(payload 与归因分离,author 不可构造);`Registrar` 重构为集中校验的 `ExtensionRegistrations` 聚合;`builder.rs` 从单个 `handler_fn` 扩成全套声明 builder。

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-sdk/Cargo.toml | +11/-4 | 配置 | 新增 conformance/testing feature 与 s5r-conformance bin 目标;依赖随 wire/host 模块新增(base64/fs2/serde_path_to_error 均有实际使用点)、移除 parking_lot、tokio 特性收窄、删掉名义上的 trusted-bundled 兼容 feature。合理。 |
| crates/astrcode-extension-sdk/src/authoring_runtime.rs | +0/-172 | 架构搬移 | 旧 Extension trait/ExtensionCtx/ExtensionHostServices 整体删除;能力按 capability 裁剪的 `scoped_to` 逻辑由 host 模块、生命周期由 lifecycle.rs、上下文由 call_context.rs 替代。非价值损失,是重写的删除侧。 |
| crates/astrcode-extension-sdk/src/bin/s5r-conformance.rs | +305/0 | 测试 | S5R 3.0 wire 协议一致性驱动:初始化/协商/激活动作套件 + 畸形帧(截断、超 MAX_FRAME_BYTES)拒绝探针,60s 超时与 worker 清理都有。对新协议是必要验收工具,有价值。 |
| crates/astrcode-extension-sdk/src/builder.rs | +728/-80 | 核心功能 | 从单个 handler_fn 扩为完整 authoring 表面:manifest/command/http_route/keybinding/status_item/custom_event 声明 builder、plan/execute 分离的 tool_handler(_args 带 serde_path_to_error 类型化参数)、ExtensionToolDefinition(definition+execution_policy+prompt 三件套)、timeout 覆盖;附充足单测。体积大但与 hook 重设计一一对应。 |
| crates/astrcode-extension-sdk/src/extension/call_context.rs | +422/0 | 核心功能 | 新归因上下文基座:ExtensionCallContext(extension_id/paths/host/events/cancel-on-drop 的 CallCancellation)、ExtensionCall trait 提供方法统一访问器、Workspace/SessionCallContext;compile_fail 测试钉死「普通 call 拿不到 tasks」不变式。是整个上下文体系的锚点。 |
| crates/astrcode-extension-sdk/src/extension/events.rs | +441/-138 | 核心功能 | LifecycleEvent 与 CustomEvent* 类型改为 re-export wire(搬移);新增 CustomEventContext(含 seq/causation/cascade_depth)、CustomEventDisposition(Ack/Retry/DeadLetter durable 消费语义)、CustomEventEmitter(声明归因、author 只能选事件名与 payload)。自定义事件系统是本 PR 的真新增能力。 |
| crates/astrcode-extension-sdk/src/extension/hooks/commands.rs | +41/-4 | 核心功能 | SlashCommand 增加 availability/execution(CommandExecution::Host),新增 SessionCommandIntent(CompactSession/SelectModel)与 HostCommand 结果变体——host 特权命令通道;同时用 deserialize_required_option 把 Option 字段收紧为必须显式出现(可为 null),属刻意的线缆契约收紧。 |
| crates/astrcode-extension-sdk/src/extension/hooks/contexts.rs | +875/-135 | 核心功能 | hook 上下文主干重构:RuntimeHookCallContext(dispatcher 输入,doc(hidden))+ HookInput<P>(可变聚合)/HookContext<P>(per-extension 只读归因视图,Deref 到 payload);CompactContext 拆 Pre/Post,新增 ProviderSettlement、ToolDiscovery/CommandDiscovery、CommandContext 重做。行数大但每个 payload 类型都对应一个新 hook 语义。 |
| crates/astrcode-extension-sdk/src/extension/hooks/handlers.rs | +195/-35 | 核心功能 | ToolHandler 拆 plan(ToolPlanContext)/execute(ToolContext);新增 ToolInputTransformHandler(参数变换从 PreToolUse 准入中分离)、ProviderContributionHandler(prepare/acknowledge 生命周期)、Pre/PostCompactHandler;CommandHandler 改 context 对象签名。与 contexts.rs 配套。 |
| crates/astrcode-extension-sdk/src/extension/hooks/mod.rs | +39/-6 | 机械适配 | 通配 re-export 改为显式清单(含 internal 专用的 Runtime* 类型单独标注)。防泄漏有意义,无独立价值。 |
| crates/astrcode-extension-sdk/src/extension/hooks/results.rs | +59/-7 | 核心功能 | PreToolUseResult 去掉 ModifyInput(移至 ToolInputTransformResult),新增 PreToolUseAdmission/PreToolUseRequirement(多处理器组合 Ask 决策)、PreparedProviderContribution/PreparedProviderEffect(durable-success 确认配套),CompactResult 改名 PreCompactResult。 |
| crates/astrcode-extension-sdk/src/extension/hooks/types.rs | +88/-71 | 架构搬移 | HookMode/CompactEvent/ContinueAfterStopLimit 移到 wire::manifest 后 re-export(搬移);新增 CompactRetainedContext(compact 后必保留的扩展上下文)、ProviderRequestId/ProviderContributionId、ExtensionError 新变体(Config/Path/Host/InvalidInput/Cancelled/Draining)。 |
| crates/astrcode-extension-sdk/src/extension/http.rs | +134/-230 | 架构搬移 | wire 类型 re-export;路由匹配/冲突/校验逻辑迁往 registration_validation.rs,旧 ExtensionManifest 迁出;新增 HttpContext(路由匹配/body 限额/JSON 解析成功后才构造)与 strict-wire deny_unknown_fields 测试。净删 96 行,是瘦身。 |
| crates/astrcode-extension-sdk/src/extension/internal.rs | +433/0 | 核心功能 | doc(hidden) 的 host-only 构造缝:所有 author 上下文的 from_runtime 构造器与 HookInput 变更函数(replace_pre_tool_input 等)集中于此,支撑「author 不可伪造归因」不变式;函数薄而多是 crate 分离(session runtime 与 runner 不同 crate)的必然代价。 |
| crates/astrcode-extension-sdk/src/extension/lifecycle.rs | +35/0 | 核心功能 | 新 Extension trait:manifest()/register/validate_config(纯校验不改状态)/start/stop/health;注释明确「配置变更创建新代际而非原地改」。契约核心,值得。 |
| crates/astrcode-extension-sdk/src/extension/mod.rs | +66/-7 | 机械适配 | extension 模块的显式 re-export 清单与 internal(doc(hidden))出口,跟随上述文件改名。 |
| crates/astrcode-extension-sdk/src/extension/package_manifest.rs | +69/0 | 核心功能 | extension.json 发现清单契约:extension_id/protocol.s5r/command/env,deny_unknown_fields,含正/反例测试;明确「发现身份与运行时声明分离」。 |
| crates/astrcode-extension-sdk/src/extension/paths.rs | +73/0 | 核心功能 | ExtensionPaths:全局/会话数据目录由 runtime 按已验证身份派生,author 拿不到 id 参数;session 目录缺失返回显式错误而非默认值。含测试。 |
| crates/astrcode-extension-sdk/src/extension/registrar.rs | +647/-98 | 核心功能 | Registrar 重构:注册项聚合进 ExtensionRegistrations(finish(manifest) 集中校验后不可变)、注册名 canonical 化、custom event 声明/订阅注册、tool 注册携带 execution_policy/prompt、ToolRegistration 封装。配套 registration_validation 使用。 |
| crates/astrcode-extension-sdk/src/extension/registration_validation.rs | +231/0 | 核心功能 | in-process Registrar 与 worker HandlerRegistry 共用的校验规则层:名称规范化/判重、hook mode 约束(fixed_hook_mode/hook_mode_is_supported)、custom event 订阅校验与匹配、HTTP 路由校验/匹配/冲突。注释说清两处校验时机不同只沉淀规则。消重有价值,含测试。 |
| crates/astrcode-extension-sdk/src/extension/runtime.rs | +353/-157 | 核心功能 | ExtensionCapability 改 re-export wire;ExtensionConfig 带 extension_id 归因 + serde_path_to_error 路径化错误 + deserialize_or_default;新增 ExtensionStopContext;ExtensionTasks 引入 Background/MustFinish 分级与 run_to_completion(不可取消持久化临界区、panic 捕获、关闭预算外继续等 must-finish)。关闭语义是实打实的健壮性改进。 |
| crates/astrcode-extension-sdk/src/extension/tool_context.rs | +142/-24 | 核心功能 | ExtensionToolContext(裸 Deref 到 core 上下文)重写为 ToolContext:host 归因、typed arguments(带路径的错误与修复提示)、require_call_id、available_tools/main/small model id。配合 plan/execute 分离。 |
| crates/astrcode-extension-sdk/src/extension/tool_plan_context.rs | +128/0 | 核心功能 | ToolPlan 阶段专用上下文:刻意不含 host 客户端/事件/任务/持久化路径,注释声明「planning 只解释最终参数」。类型级无副作用保证,是 plan/execute 分离设计的一半。 |

## 批次小结

这批是 PR 的契约主干,整体价值高:归因上下文(call_context/internal)、plan/execute 分离(tool_plan_context/tool_context/handlers)、provider contribution 的 prepare/acknowledge(types/results/handlers)、注册校验消重(registration_validation)和任务关闭语义(runtime.rs 的 MustFinish)都有明确的 invariant 动机,且多数带针对性测试(含 compile_fail 与 strict-wire 反例)。机械适配部分(mod.rs 两处显式 re-export)占比很小。没有明显可删的文件;理论上 builder.rs(728 行新增)可拆成独立 authoring crate 或后续 PR 引入,但它与新契约同步落地、作者文档/示例即用,推迟收益不大。

存疑点:

- **builder.rs `tool()` 默认 origin 改为 Bundled**:SDK 同时服务内置扩展与外部 worker 作者,`worker_tool()` 才是 ToolOrigin::Extension;若外部作者绕过 worker prelude 直接用 `builder::tool()`,工具会被错标为 Bundled。建议确认 worker prelude 是唯一对外的 authoring 入口,或考虑让 SDK 顶层 `tool()` 默认 Extension。
- **commands.rs 的 `deserialize_required_option`**:把 `args_schema`/`keep_recent_turns` 收紧为「字段必须出现、值可为 null」,手写声明(非 builder 生成)少写字段会直接反序列化失败,错误信息不直观。3.0 大版本可接受,但建议在作者文档中明示该契约。
# 批次 11:astrcode-extension-sdk —— host 域客户端、SDK 门面与清单/hostpaths/runtime_ports

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-sdk/src/frontmatter.rs | +8/-0 | 核心功能 | 新增 `normalize_markdown`(去 BOM、统一 CRLF/CR→LF),供扩展文档 frontmatter 分割前归一化;小而明确的共享原语,符合 SDK 职责。 |
| crates/astrcode-extension-sdk/src/host/client.rs | +568/-0 | 测试 | 非测试部分只是 10 个 `TypedXxxClient<ExtensionHost>` 类型别名加 `main_available`/`small_available` 两个薄封装(约 50 行);其余 500+ 行是 domain client 全操作路由覆盖测试与 preflight 权限/上下文/后端不可用三类错误区分测试,直接锁住 SDK 最关键的表面契约,值得。 |
| crates/astrcode-extension-sdk/src/host/domain_client.rs | +622/-0 | 核心功能 | 新 host 域客户端主体:泛型 `HostClientTransport` + `HostOp` 关联类型,把 session_control(含新 queue_or_start/defer_context)、session_history/state/inspect、workspace、process、network、llm 等全部宿主操作封装为类型安全方法,invoke/collect_stream/invoke_ack 统一处理序列化与 ack 校验;是 S5R 3.0 扩展侧调用宿主的唯一入口,价值高。 |
| crates/astrcode-extension-sdk/src/host/error.rs | +96/-0 | 核心功能 | 新增 `HostError`:对 wire `ErrorPayload` 的无损封装(未知 code 以字符串保留、可往返),并配 round-trip 测试;符合「不把内部 enum 直接暴露成线缆契约」的边界映射规则。 |
| crates/astrcode-extension-sdk/src/host/llm_mapping.rs | +218/-0 | 核心功能 | wire `HostLlmMessage/Content` 与 core `LlmMessage` 的双向映射(边界映射,放对了位置),加 `collect_model_stream`(聚合 delta、强制要求 terminal 事件、缺 model 字段/提前关闭分别报 InvalidResponse/StreamClosed);含完整测试。 |
| crates/astrcode-extension-sdk/src/host/mod.rs | +546/-0 | 核心功能 | `ExtensionHost` 门面 + `HostScope` 本地预检(能力授权/操作可用性/session|workspace 上下文三类失败可区分)+ `internal` 运行时构造边界(HostInvoker trait、OutboundNetworkService 端口从旧 `network` 模块迁入)。preflight 只做尽力早失败、HostRouter 仍是权威判定,职责划分清楚。 |
| crates/astrcode-extension-sdk/src/host/workspace_patch.rs | +159/-0 | 核心功能 | 纯函数 unified-diff 路径分析(`analyze_unified_diff_paths`/`normalize_unified_diff_path`),供工具 planner 与宿主 lease 强制共用同一路径视图,避免「授权的路径 ≠ 实际写入的路径」;安全相关的不变式注释到位,有测试。 |
| crates/astrcode-extension-sdk/src/hostpaths.rs | +138/-17 | 核心功能 | `write_file_atomic` 加固:唯一临时文件名(进程 id + 原子计数)、flush+sync_all 后再 rename、失败清理临时文件,修复并发写同名 `.tmp` 互相覆盖的旧缺陷;新增 read/write/update_json_state  trio,用 fs2 文件锁做扩展状态文件的读改写串行化。属真实缺陷修复 + 扩展状态持久化原语,依赖 fs2 已在 Cargo.toml 声明。 |
| crates/astrcode-extension-sdk/src/lib.rs | +118/-88 | 架构搬移 | SDK 门面重排:删 `network`/`trusted`/`state`/`session_query`/`session_inspect`/`runtime`/`worker` 模块与旧 `protocol::JsonRpcError`,新增 `host`/`wire`/`transport`/`model_stream`/`testing`,prelude 从 worker_prelude 双门面合并为单一大 prelude。基本是 re-export 重组,无独立新逻辑,是 runtime→wire 重写必要的出口面适配。 |
| crates/astrcode-extension-sdk/src/manifest.rs | +117/-20 | 核心功能 | `ExtensionManifest` 从 extension 模块迁入并私有化字段(getters 访问,避免 struct literal 变成兼容边界),新增 version/description 必填校验与 id 字符集校验(ASCII 字母数字 + `.-_`,首字符须字母数字),错误类型从手写 Display 换成 thiserror。搬移+校验增强各半,方向正确。 |
| crates/astrcode-extension-sdk/src/model_stream.rs | +47/-0 | 核心功能 | `ModelStream`:在 wire `TerminalStream` 上叠加进程内 cancel-on-drop,保证扩展丢弃流时宿主侧推理被取消;小而有明确并发不变式。 |
| crates/astrcode-extension-sdk/src/runtime_ports.rs | +98/-253 | 核心功能 | 净删 155 行:删掉 `CompositeToolCatalogProvider`(组合/去重逻辑已内聚进 astrcode-extensions runner 的 `ExtensionView`,`ToolCatalogScope` 同步去掉 session_store_dir),端口 trait 全部切到 `Runtime*Context` 内部上下文;新增 provider 请求两段式(prepare/acknowledge,ack 绑定 pinned extension 代际防热重载串线)、`transform_tool_input` 与 pre/post compact 钩子。是新架构的端口面重构而非删功能,值得。 |

## 批次小结

这批是 S5R 3.0 SDK 侧的核心增量,整体都有价值:host/* 六个文件构成扩展调用宿主的完整类型安全入口(能力预检、错误无损映射、流式模型调用),runtime_ports 与 lib.rs 完成 runtime→wire 的端口面切换,hostpaths 的原子写加固修复了真实的并发覆盖缺陷。没有看到可以删除的部分;lib.rs 的 re-export 重排和 client.rs 的测试体量虽大,但分别属于必要的门面适配和对 SDK 最关键契约的锁定。可留意的小点(不构成删改建议):host/client.rs 的非测试代码极薄,若后续批次中 wire/protocol 测试已覆盖操作清单,这里 500 行的操作枚举测试有一定重复维护成本;manifest.rs 的 id 字符集校验是新增约束,需要确认存量扩展 id 全部合规(本批次看不到调用方,交由 manifest 消费方批次核实)。
# 批次 12:astrcode-extension-sdk 的 s5r 线协议旧模块清理 + session 契约搬移 + 新增 testing 脚手架

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-sdk/src/s5r/capabilities.rs | +0/-93 | 架构搬移 | `astrcode.*` 能力名映射整体删除,由 `wire/capability.rs` 的宏生成 `ExtensionCapability::as_str/parse`(snake_case 命名)取代;是 wire/* 重写的一部分,删除合理。 |
| crates/astrcode-extension-sdk/src/s5r/effects.rs | +0/-106 | 架构搬移 | `HandlerResult`/`CallContinuation` 迁至 `wire/effects.rs`(已确认新文件含同名类型),纯搬移。 |
| crates/astrcode-extension-sdk/src/s5r/manifest.rs | +0/-132 | 架构搬移 | `InitializeManifest`/`ManifestTool`/`ManifestHook` 等迁至 `wire/manifest.rs`,纯搬移。 |
| crates/astrcode-extension-sdk/src/s5r/messages.rs | +0/-333 | 架构搬移 | `WireMessage`/`InitializeMsg`/`PeerInfo`/`encode_wire_message` 等帧协议类型迁至 `wire/protocol.rs`,纯搬移。 |
| crates/astrcode-extension-sdk/src/s5r/mod.rs | +19/-9 | 架构搬移 | s5r 模块门面改为 re-export `wire::*` 与新 `tool_plan`,保持旧路径兼容;s5r 目录实质只剩 tool_plan 一个真模块,可考虑后续把 tool_plan 也并入 wire 以彻底拆掉 s5r 目录。 |
| crates/astrcode-extension-sdk/src/s5r/tool_plan.rs | +222/-0 | 核心功能 | 新增 tool plan/execute 调用的严格线契约(`ToolInvocationRequest`/`ToolPlanDto`,`deny_unknown_fields`)及与 `astrcode_core::tool::access` 的双向映射,附 round-trip + 拒绝未知字段测试;是 host 侧 tool_intercept/plan 链路的必要线缆类型,有价值。 |
| crates/astrcode-extension-sdk/src/session.rs | +11/-509 | 架构搬移 | 原 515 行 session 线缆契约(`HostCreateSessionRequest`、`SessionToolSelectionDto` 等)全部迁至 `wire/session.rs`,本文件只剩 re-export;价值在契约归拢,本身无新逻辑。 |
| crates/astrcode-extension-sdk/src/session_inspect.rs | +0/-173 | 架构搬移 | `SessionInspect*` DTO 迁至 `wire/session_inspect.rs`(已确认),纯搬移。 |
| crates/astrcode-extension-sdk/src/session_query.rs | +0/-67 | 架构搬移 | `SessionQuery`/`SessionQueryFactory` trait 删除,能力改由 host session_history domain client 提供(`list_summaries`/`transcript`/`token_usage` 已在 `host/domain_client.rs` 确认);旧的 `extension_data_dir` 查询未见对等物,若无人使用则属合理精简。 |
| crates/astrcode-extension-sdk/src/shell.rs | +11/-4 | 测试 | 仅为测试加固:环境变量读写改 `unsafe`(Rust 2024)并加进程内互斥锁防并发测试竞争;不影响生产代码,值得。 |
| crates/astrcode-extension-sdk/src/testing.rs | +700/-0 | 测试 | 新增 SDK 公开测试脚手架:各 hook/context 的 Builder(Command/Http/Tool/PreToolUse 等),默认无后端、确定性 fixture;体量较大但对扩展作者是核心开发体验,价值成立。 |
| crates/astrcode-extension-sdk/src/testing/harnesses.rs | +473/-0 | 测试 | 新增 `MockExtensionHost`(走真实 preflight 路径,保留权限/后端/scope 失败区分)与注册/生命周期 harness;与 testing.rs 配套,价值成立。 |
| crates/astrcode-extension-sdk/src/transport.rs | +33/-0 | 核心功能 | 新增 `TransportProfile`(transport feature 集合 + `missing()` 检查),用于扩展准入时校验传输能力;小而内聚,是 admission 链路的必要件。 |

## 批次小结

这批改动整体都有价值,且高度内聚:一半是 s5r 旧线协议模块向 `wire/*` 的纯搬移/删除(capabilities、effects、manifest、messages、session、session_inspect、session_query、mod.rs),删除端均已在新位置确认有对等实现,没有发现"删了没搬"的丢失(唯一注意点:旧 `SessionQuery::extension_data_dir` 在新 host client 中未见对等物,但疑属无人使用的清理)。新增部分里 tool_plan.rs(222 行)与 transport.rs(33 行)是新协议的必要线缆类型;testing.rs + harnesses.rs 共 1173 行是全新的扩展作者测试脚手架,体量最大,属于可独立审查/独立合入的一块,如果想拆 PR,这是最自然的一个拆分点(它只依赖 SDK 内部 API,不阻塞 wire 重构主线)。可推迟项:s5r/mod.rs 目前只是 re-export 门面,后续可把 tool_plan 也并入 wire 后删除整个 s5r 目录,当前保留兼容 shim 可以接受。
# 批次 13:extension-sdk wire 协议层(capability / effects / error / frame / host 各域线缆契约)

本批 16 个文件全部是新增文件(0 删除),属于 runtime/* + worker/* 删除后重写的 wire/* 协议层中除 peer/peer_runtime/protocol/operation/manifest/session 之外的部分。对照基线被删文件判断:worker/host.rs(729 行,松散 DTO)、runtime/transport.rs(257 行)、s5r/capabilities.rs(93 行)、s5r/effects.rs(106 行)、s5r/messages.rs(333 行,错误码为散落字符串)。

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-sdk/src/wire/capability.rs | +115/-0 | 架构搬移 | `ExtensionCapability` 从 extension/runtime 迁入 wire 层,并用宏把原先 `astrcode_capability_name`/`capability_to_wire` 两个手写映射表收敛为单一事实来源(含 parse/serde);搬移同时消除了双表漂移风险,值得。 |
| crates/astrcode-extension-sdk/src/wire/custom_event.rs | +145/-0 | 核心功能 | 基线不存在的自定义事件声明/订阅契约(delivery 语义三态、payload 上限、source filter),是 S5R 3.0 新增的扩展间事件模型,缺它整个 custom-event 链路无从谈起;含严格缺字段/未知字段测试。 |
| crates/astrcode-extension-sdk/src/wire/effects.rs | +143/-0 | 核心功能 | 重写旧 s5r/effects.rs 的 `{ok, effect: Option<String>, error}` 松散模型为强类型 `HandlerEffect` enum + `HandlerResult`,`ProviderContributionData`/`ToolOutcome.metadata`(PR 主线 ToolOutcome.metadata 链路)为新增;把 effect 字符串收紧为 enum 是真实的协议强化。 |
| crates/astrcode-extension-sdk/src/wire/error.rs | +128/-0 | 核心功能 | 把基线散落在 peer.rs 等处的临时错误码字符串收敛为 70+ 项 `WireErrorCode` 宏目录(唯一性+round-trip 测试),注释明确"线缆字符串永不复用";这是协议可演进性的地基,新增价值明确。 |
| crates/astrcode-extension-sdk/src/wire/extension_http.rs | +205/-0 | 架构搬移 | `ExtensionHttpMethod/Access/Route/Request/Response` 从 extension/http.rs 抽入 wire 层;新增严格 status(100–599)校验和 `ExtensionHttpDispatchRequest` 及 deny_unknown_fields,属搬移+边界强化。 |
| crates/astrcode-extension-sdk/src/wire/frame.rs | +216/-0 | 架构搬移 | 长度前缀帧传输整体搬自 runtime/transport.rs;`FrameError` 类型化、帧头严格化(仅十进制数字)、加 `FrameTransport` trait 与 tracing。注意:`MAX_FRAME_BYTES` 由 8MB 提到 16MB,是静默的协议上限变更,未见说明,建议在 PR 描述中注明理由。 |
| crates/astrcode-extension-sdk/src/wire/mod.rs | +51/-0 | 架构搬移 | wire 模块声明与精选 re-export,定义了新协议层的公共门面;模块文档声称不依赖 host 领域 crate,与本批内容一致。 |
| crates/astrcode-extension-sdk/src/wire/host/mod.rs | +885/-0 | 核心功能 | host 各域 DTO 的统一 re-export + 有界 UTF-8/usize serde 校验宏(边界验证,符合仓库规范);后 ~650 行是覆盖全部 host 契约的严格 round-trip/拒绝非法形状测试,测试密度高但直接保护线缆契约,可接受。 |
| crates/astrcode-extension-sdk/src/wire/host/event.rs | +53/-0 | 核心功能 | `astrcode.event.emit` 的强类型请求/输出契约(非空 event_type、正 schema_version、三态发布结果),基线只有能力名没有契约,纯新增。 |
| crates/astrcode-extension-sdk/src/wire/host/llm.rs | +82/-0 | 核心功能 | 把旧 `HostClient::main_chat(messages: Value)` 的裸 JSON 换成强类型 `HostLlmChatRequest/HostLlmMessage/HostLlmContent`(camelCase 属线缆类型,合规);消掉了 worker 侧靠手拼 JSON 的隐式契约。 |
| crates/astrcode-extension-sdk/src/wire/host/network.rs | +158/-0 | 核心功能 | network 契约重写:body 由"仅 UTF-8 文本"改为 base64 字节并带上限 serde 校验,新增重定向策略;timeout/max_bytes 边界由 host_router/network.rs:84-93 强制,职责分层清楚,不是文档空话。 |
| crates/astrcode-extension-sdk/src/wire/host/process.rs | +223/-0 | 核心功能 | spawn 请求/响应搬自旧 worker/host.rs(加 stdin 上限校验);`HostProcessStart/Handle/State/Input`(进程句柄生命周期、五态状态机)是 S5R 3.0 新增的长驻进程能力契约,增量真实。 |
| crates/astrcode-extension-sdk/src/wire/host/session.rs | +105/-0 | 存疑 | 内容本身合格(session summary/transcript/token usage 为新增契约,Input/Delivery/Cancel/ExecutionView 搬自旧 worker/host.rs 并把 String status 收紧为 enum)。疑点:第 61–63 行 `HostSessionInputRequest` 上方的文档注释是从 process.rs 复制来的残留(写着 "astrcode.process.spawn 的线缆请求""stdin 最大为 HOST_PROCESS_MAX_STDIN_BYTES"),句子截断且含本模块不存在的 intra-doc 链接,会误导且可能在 docs 构建时报警;建议直接删掉这段错注释。 |
| crates/astrcode-extension-sdk/src/wire/host/session_state.rs | +70/-0 | 核心功能 | 会话内 extension 命名空间 state 读写的新契约,key 格式(禁 `.`/`..`/嵌套路径、128 字符上限)与 value 1MB 上限在反序列化处强制;基线只有裸 Value 调用,属边界收紧的新增价值。 |
| crates/astrcode-extension-sdk/src/wire/host/tool_result.rs | +50/-0 | 核心功能 | 持久化 tool-result artifact 分页读取契约(偏移/窗口、max_bytes 在 4..=20000 内强制),配合 PR 主线的大结果外置存储,新增且必要。 |
| crates/astrcode-extension-sdk/src/wire/host/workspace.rs | +444/-0 | 核心功能 | workspace 六类操作契约:read/write/edit/list/grep/glob 主体搬自旧 worker/host.rs 但普遍加上限校验与 deny_unknown_fields;`ApplyPatch`、`TextChange`(unified diff 摘要)、grep 上下文行、glob 开关为新增;体量大但与 host_router 侧一一对应,未见冗余。 |

## 批次小结

这批是 S5R 3.0 协议层的主体之一,整体有价值:把基线散落的松散 `Value`/字符串契约系统性升级为带边界校验的强类型线缆 DTO,正好落在仓库规范要求的"跨边界值在边界校验"上;DTO 均跨 S5R 边界,不违反 DTO 规则。没有可整文件删除或推迟的部分——custom_event、event emit、tool_result、process 句柄等新增契约都被 host_router/新扩展 crate 实际消费。两点遗留:① host/session.rs 的复制残留错注释应修(唯一的存疑项);② frame.rs 帧上限 8MB→16MB 属静默协议变更,建议在 PR 描述中注明动机,不必回退。host/mod.rs 的契约测试体量(~650 行)虽大,但每个断言对应一个真实的非法形状拒绝路径,不算过度。
# 批次 14:extension-sdk wire/* 新增与 runtime/* 删除(S5R 3.0 线缆层重写)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-sdk/src/wire/manifest.rs | +408/-0 | 核心功能 | 新增 typed `InitializeManifest`(tools/hooks/commands/http_routes/custom_events),是 S5R 3.0 握手声明的线缆契约;`ManifestTool.timeout_ms` 正是 PR 主线 timeout 全链路的起点,`deny_unknown_fields` 边界校验齐全,值得。 |
| crates/astrcode-extension-sdk/src/wire/operation.rs | +705/-0 | 核心功能 | 宏生成的 host operation 目录(~48 个操作,含新增的 `queue_or_start`/`defer_context`/`interrupt_and_submit`),把 wire name、所需 capability、stream/cancel 标记收敛为单一事实来源;宏有一定复杂度但消除了 enum 与 spec 表手工同步的漂移风险,值得。 |
| crates/astrcode-extension-sdk/src/wire/peer.rs | +699/-0 | 核心功能 | 新增 typestate Peer(`Uninitialized→HostInitialized/WorkerInitialized→Ready→into_runtime`),把握手合法性编译期化(附 compile_fail doctest),含握手协商/拒绝/激活的成对测试;是旧 runtime/peer.rs 动态状态机的重写而非搬移,价值明确。 |
| crates/astrcode-extension-sdk/src/wire/protocol.rs | +541/-0 | 核心功能 | S5R 3.0 协议消息与常量(`S5R_VERSION="3.0"`、feature/capability 名),`HandlerId`/`FeatureName` 从裸字符串升级为带解析校验的 newtype;部分消息类型由旧 s5r 模块搬移而来,但校验与 feature 协商是新增,整体按新功能计。 |
| crates/astrcode-extension-sdk/src/wire/session.rs | +422/-0 | 核心功能 | session control/history 的线缆 DTO(工具选择、create/submit/queue_or_start 等请求响应),跨插件边界创建 DTO 符合仓库 DTO 规则;注释中英混杂是小瑕疵,不影响价值。 |
| crates/astrcode-extension-sdk/src/wire/session_inspect.rs | +389/-0 | 核心功能 | `session_inspect` 插件边界契约(list/snapshot/provider_messages 等),带 `deny_unknown_fields` 与非空 session_id 反序列化校验,边界校验到位,值得。 |
| crates/astrcode-extension-sdk/src/wire/stream.rs | +119/-0 | 核心功能 | 新增 `TerminalStream`:terminal 事件恰好一次、提前关闭时合成 `Failed` 的流终止语义原语,供进程内 ModelStream 与 wire PeerStream 共享,附单测;小而清晰。 |
| crates/astrcode-extension-sdk/src/wire/transport.rs | +22/-0 | 核心功能 | 新增 `TransportFeature`(目前仅 `AuthenticatedHttp`),把「准入所需传输特性」与「已授予 capability」显式分开,22 行定义,合理。 |
| crates/astrcode-extension-sdk/src/runtime/cancel.rs | +0/-55 | 架构搬移 | `CancelToken` 删除,等价物为 `wire::InvocationCancellation`(peer_runtime.rs),并由 astrcode-extension-worker 以 `CancelToken` 别名 re-export;纯搬移删除。 |
| crates/astrcode-extension-sdk/src/runtime/mod.rs | +0/-17 | 架构搬移 | runtime 模块入口删除,导出面由 wire/mod.rs 取代;随整体重写删除,无独立价值问题。 |
| crates/astrcode-extension-sdk/src/runtime/peer.rs | +0/-1854 | 架构搬移 | 旧的动态 Peer 状态机(1854 行)整体删除,职责由 wire/peer_runtime.rs(2429 行)+ typestate wire/peer.rs 重写承接;属「删除旧实现」,新价值记在新文件上。 |
| crates/astrcode-extension-sdk/src/runtime/stream.rs | +0/-73 | 架构搬移 | 旧 `EventStream` 删除,由 `ModelEventStream`/`TerminalStream`(wire/stream.rs)取代,且新实现补齐了「关闭前无 terminal 事件合成 Failed」的语义;搬移中带小幅改进。 |
| crates/astrcode-extension-sdk/src/runtime/task_utils.rs | +0/-49 | 架构搬移 | `spawn_traced`(catch_unwind + tracing panic 日志)删除,等价逻辑内联在 extension/runtime.rs:292/350 且保留 panic 测试;无功能损失。 |
| crates/astrcode-extension-sdk/src/runtime/transport.rs | +0/-257 | 架构搬移 | stdio 长度前缀帧传输删除,`frame_payload`/`parse_frame_header` 等原样迁至 wire/frame.rs;纯搬移删除。 |

## 批次小结

这批改动是 S5R 3.0 线缆层重写的核心载体:6 个 runtime/* 旧文件全部删除(共 -2305 行),8 个 wire/* 新文件(共 +3703 行)承接并升级。整体质量高、方向正确:

- 旧 runtime/peer.rs 的运行期状态检查重写为编译期 typestate(wire/peer.rs + peer_runtime.rs),并补了握手正/负路径测试,是这批里最实质的改进。
- operation.rs 把 host 操作目录宏化为单一事实来源,新操作(queue_or_start/defer_context 等)自然落入同一机制,没有散落的字面量。
- wire/stream.rs 的 TerminalStream 显式修复了旧 EventStream 未覆盖的「提前关闭」语义,属于顺带修掉的 real bug 面。
- session.rs/session_inspect.rs/manifest.rs 都是跨边界 DTO,符合仓库「只在边界建 DTO」的规则,且边界校验(deny_unknown_fields、非空 id)齐全。

可挑剔的点很少:operation.rs 宏可读性一般(但换来一致性,可接受);wire/session.rs 注释中英混杂,风格不统一;runtime 删除侧没有任何残留引用(已确认 sdk 内无 `crate::runtime` 引用),删除是干净的。没有建议删除、拆分或推迟的部分;无存疑项。
# 批次 15:extension-sdk wire/peer_runtime(新增)与 worker/*(整体迁出)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-sdk/src/wire/peer_runtime.rs | +2429/-0 | 核心功能 | S5R3 wire 协议运行时核心:PeerDriver/PeerHandle、单一写泵(FIFO 保证 cancel 不越过 invoke)、带 peer 原因的 InvocationCancellation、流式调用的背压/空闲超时(30s/120s)、在途请求信号量上限(256)。替代旧 runtime/peer.rs(1854 行)且近乎全量重写(diff 4000+ 行),其中约 1080 行(1347 行起)是模块内单元测试,覆盖取消原因保持、写/读门控等并发场景。是本次重构的关键路径,价值高。 |
| crates/astrcode-extension-sdk/src/worker/builder.rs | +0/-151 | 架构搬移 | 整体迁往 astrcode-extension-worker/src/worker/builder.rs(+233),且目的地有实质重写(新增 continuation/custom_event/tool_planner handler,parse_tool_arguments 改收已校验的 arguments 并用 WireErrorCode)。本文件的删除是搬迁的一半,价值在目的地体现。 |
| crates/astrcode-extension-sdk/src/worker/host.rs | +0/-729 | 架构搬移 | 迁往 astrcode-extension-worker/src/worker/host.rs(+919),HostApi/HostClient 重写为基于 wire::host::internal Typed*Client 的强类型客户端,并 re-export 大量 Host* 类型。属 S5R3 主机能力 wire 化的一部分。 |
| crates/astrcode-extension-sdk/src/worker/manifest.rs | +0/-174 | 架构搬移 | 内部 ManifestCatalog 删除,功能由 wire/manifest.rs(+408)取代:从 worker 私有构建逻辑升级为 worker/host 共享的 typed InitializeManifest(serde、deny_unknown_fields、capabilities/custom_events/transport features)。属于协议契约上移,方向正确。 |
| crates/astrcode-extension-sdk/src/worker/mod.rs | +0/-268 | 架构搬移 | worker 模块入口迁往 astrcode-extension-worker/src/worker/mod.rs(+449),handler 注册宏/函数随新 wire 类型重写。SDK 只保留 wire 协议层、worker 运行时独立成 crate,分层合理。 |
| crates/astrcode-extension-sdk/src/worker/registry.rs | +0/-361 | 架构搬移 | 迁往 astrcode-extension-worker/src/worker/registry.rs(+1236,体量大幅膨胀),注册逻辑重写为基于 wire::manifest 的 InitializeManifest 构建 + 校验(canonical_registration_name、路由冲突、hook mode、custom event 订阅校验等)。 |

## 批次小结

这批改动是 S5R3 重构的两半拼图:SDK 内 worker/* 五个文件全部删除、迁到新 crate astrcode-extension-worker 并随 wire 协议重写;SDK 内新增 wire/peer_runtime.rs 作为协议运行时核心。方向(SDK=协议层、worker=运行时层)清晰且与 PR 主线一致,整体都有价值:

- peer_runtime.rs 是本批唯一实质新增,也是整个 wire/* 里最重的一块,取消语义、写泵顺序保证、流式背压这些并发不变量都集中在这里,配套测试充分,值得保留。
- 五个 worker/* 删除文件全部确认有对应落点(builder/host/mod/registry → astrcode-extension-worker,manifest → wire/manifest.rs),无功能丢失;它们的价值判定应在对应目的地文件的 chunk 中做,本批只承担「删除」一半。
- 可挑剔的点只有一处:peer_runtime.rs 单文件 2429 行、其中测试占约 45%,把 tests 拆到 tests/ 或子模块会更易读,但属风格问题,不构成删/拆/推迟的理由。
- 无存疑项。
# 批次 16:内置扩展 crate(session-commands / skill / todo-tool / web-tools)适配新 SDK

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-session-commands/Cargo.toml | +10/-0 | 架构搬移 | 新 crate 清单,只依赖 extension-sdk,符合「内置插件只依赖插件系统」约束。 |
| crates/astrcode-extension-session-commands/src/lib.rs | +100/-0 | 架构搬移 | 把 /compact、/model 两个一方会话命令从 host 核心搬出,改为纯 Extension 契约声明(host_command + intent),含参数校验;代码干净,是 S5R3「命令走扩展协议」的样板,值得。 |
| crates/astrcode-extension-skill/Cargo.toml | +1/-0 | 配置 | dev-dependency 加 sdk testing feature,配合测试重写。 |
| crates/astrcode-extension-skill/src/lib.rs | +160/-196 | 机械适配 | 全面适配新 SDK:manifest builder 取代 id()/capabilities()、ToolContext/CommandDiscoveryContext 上下文对象、plan() 返回 ResourceAccess、参数解析交给 ctx.arguments()、normalize_markdown 复用 SDK frontmatter;测试合并精简(删 3 个重叠测试合并成 1 个端到端),净减 36 行,适配质量高。 |
| crates/astrcode-extension-todo-tool/src/lib.rs | +189/-203 | 核心功能 | 除 SDK 适配外有实质行为改动:reminder 改为 prepare/acknowledge 两段式 + revision 乐观并发(解决 provider 请求失败重试时计数器重复推进的问题),schema_version 1→2,读写复用 hostpaths::read/write/update_json_state;新增并发语义测试。改动有价值,但见存疑点。 |
| crates/astrcode-extension-web-tools/Cargo.toml | +4/-2 | 配置 | 删掉不再使用的 tracing(load_config 不再 warn 兜底),加 dev-dep tokio 跑新异步测试;合理。 |
| crates/astrcode-extension-web-tools/src/config.rs | +54/-9 | 核心功能 | load_config 从「反序列化失败静默兜底默认值」改为返回带 hint 的 InvalidInput 错误,并新增 maxOutputChars/summarizerMaxOutputTokens 非零校验 + 测试;符合「不要用默认值掩盖数据损坏」,值得。 |
| crates/astrcode-extension-web-tools/src/fetch_url.rs | +134/-81 | 核心功能 | 适配 host::NetworkClient/ModelClient wire 契约(机械部分),另有实质改进:输出截断统一收口到 truncate_text 并作用于所有非模型路径(含 HTTP 错误、redirect 渲染),summarizer 输入/输出上限分离,新增边界测试;修复了旧代码多条路径绕过 max_output_chars 的问题。 |
| crates/astrcode-extension-web-tools/src/lib.rs | +81/-59 | 机械适配 | manifest builder、ToolPlan/HostResource 声明、start() 改从 ctx.host() 取 NetworkClient/ModelClient、新增 validate_config 钩子;顺带删掉了 on_config_changed(配置变更改由运行时重启/校验处理,属新运行时语义);host_timeout_ms clamp 有测试。 |
| crates/astrcode-extension-web-tools/src/web_search.rs | +26/-23 | 机械适配 | OutboundNetworkService→host::NetworkClient 的类型替换,timeout 改 ms,无逻辑变化。 |

## 存疑

1. **todo-tool 旧 progress.json 兼容性**:`PROGRESS_SCHEMA_VERSION` 从 1 升到 2,同时删掉 `executor` 字段的 `#[serde(default)]` 和「legacy v1 文件迁移为 MainAgent」的测试。磁盘上现存的 v1 progress.json 此后会在 `load_progress` 里持续报「unsupported progress list schema version 1」,且该错误会出现在每次 todoWrite 和 reminder 路径上,没有任何自动清理/迁移。会话级临时数据或许可接受,但建议确认:要么留一次性迁移,要么在版本不匹配时删除重建而不是永久报错。

## 批次小结

这批改动整体都有价值:四个 crate 是 S5R3 SDK 重写的直接适配面,且不止机械替换——todo-tool 的 prepare/acknowledge 并发语义、web-tools 的配置严格校验和输出截断收口都是真实缺陷修复。没有明显可删/可推迟的部分;唯一需要跟进的是 todo-tool 的 v1 持久化文件兼容策略(见存疑)。web-tools 删除 `on_config_changed` 依赖新运行时「配置变更即重启/重校验」的语义,需在 host 侧确认该语义已实现,否则配置热更新会静默失效。
# 批次 17:新 crate astrcode-extension-worker(worker 侧运行时)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extension-worker/Cargo.toml | +23/-0 | 配置 | 新 crate 清单:仅依赖 astrcode-extension-sdk + tokio 等,带 `testing` feature;已加入 workspace members 并被 astrcode-extensions 的 s5r-guest 测试引用,非死代码。 |
| crates/astrcode-extension-worker/src/lib.rs | +80/-0 | 核心功能 | 新 crate 入口:re-export SDK 契约 + `Worker`,并定义 `worker_prelude` 作为扩展作者的统一导入面;是 worker/SDK 边界划分的落点,必要。 |
| crates/astrcode-extension-worker/src/worker/builder.rs | +233/-0 | 核心功能 | 由旧 sdk `worker/builder.rs`(151 行,约 98 行保留)扩展而来:新增 tool_planner/continuation/custom_event/http 等类型化 handler 包装,减少闭包样板;含约 65 行单测。搬移+实质新增,值得。 |
| crates/astrcode-extension-worker/src/worker/mod.rs | +449/-0 | 核心功能 | `Worker` 注册 API + `run_stdio` 重写为基于新 wire `Peer` 的握手/激活/驱动流程(旧实现 268 行基于已删 runtime::peer,仅约 26% 行保留);`tool_result` 新增 metadata 透传(ToolOutcome 全链路的一环)。重写实至名归。 |
| crates/astrcode-extension-worker/src/worker/host.rs | +919/-0 | 核心功能 | 前约 250 行为生产代码:`HostApi` trait + `V3PeerHostApi`(桥接 wire PeerHandle)+ task_local 作用域宿主 API(注释明确不传播到 spawn,防绕过 lease),类型化 client 本体已下沉到 sdk `host/internal`,此处只做绑定;后约 660 行是 wire 契约/路由/能力协商测试,占比偏高但与功能对应。 |
| crates/astrcode-extension-worker/src/worker/registry.rs | +1236/-0 | 核心功能 | handler 注册表+分发:旧版 361 行仅约 17% 行保留,实质重写。新增 plan/execute 两阶段 tool 分发、WorkerToolPlanContext/CustomEventContext 等事实类型、注册名规范化与重名/路由冲突校验(复用 sdk `extension::internal`)、timeout_ms 毫秒域校验;含约 290 行单测。体量最大但与 S5R 3.0 新协议面匹配。 |

## 批次小结

这批是「SDK worker/* 删除 → 独立 worker crate」拆分的承载物,整体有价值:不是纯搬移(逐文件保留率 17%–42%),而是围绕新 wire 协议(peer_runtime、两阶段 tool 调用、能力目录、类型化宿主 client 下沉 SDK)的实质重写,且每个注册/分发路径都有对应测试。crate 已被 workspace 收纳并被宿主侧 s5r-guest 测试真实使用,无死代码。可斟酌而非必须处理的两点:(1) `mod.rs` 的 `V3WorkerInvokeHandler` 把 CONFORMANCE_* 探测操作编进了每个生产 worker 的分发路径,虽只在特定 capability 名触发、无害,但按 feature gate 收起会更干净;(2) `host.rs` 约 2/3 篇幅是测试,若后续继续膨胀可拆到 tests/,当前可接受。不建议删/拆/推迟任何文件。
# 批次 18:astrcode-extensions host_router 与 manifest

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extensions/Cargo.toml | +10/-2 | 配置 | 跟随重构的依赖调整:去掉 astrcode-context、tokio 特性 rt-multi-thread→rt(同步 invoke 已全异步化),新增 base64/ignore/similar/uuid 及 `testing` feature(集成测试经 dev-dependency 自引用打开 feature gate),均有对应主线用途,值得。 |
| crates/astrcode-extensions/src/extension_manifest.rs | +378/-192 | 核心功能 | manifest 归一化从「手写 JSON 解析 + 字符串匹配」重写为 typed wire manifest:HandlerId 归属/kind 校验、hook mode 支持矩阵改由 SDK 判定、tool timeout_ms 归一化(拒绝 0)、custom events 声明/订阅与 custom_event_v1 协商校验。`protocol.s5r` 版本检查移出本文件,已确认由 SDK `wire/peer.rs:387` 握手层承接,无缺口。 |
| crates/astrcode-extensions/src/host_router.rs | +2360/-439 | 核心功能 | 路由核心重写:删除 block_on_async 全局静态 runtime,invoke/invoke_event_stream 全异步化;分发改为 SDK-owned `HostOperationSpec` 驱动;新增 resource lease 强制(enforce_resource_lease/required_resource_accesses)、ExtensionGenerationGate、planning 上下文拒绝、session/workspace 上下文要求校验、custom event 因果链(causation_id/cascade_depth)与 send_confirmed 回执。新增约 1500+ 行测试(lease 越界拒绝、queue_or_start/defer_context、取消终止进程组等),覆盖的是新行为而非存量。注意新增子模块 wire.rs/tool_result.rs/workspace_patch.rs/process_handles.rs 的引用,其本体在其他批次审计。 |
| crates/astrcode-extensions/src/host_router/capability.rs | +78/-638 | 架构搬移 | 本地 capability 元数据注册表(枚举宏、spec 表、JSON schema 拼装)整体删除,元数据迁到 SDK 的 `HOST_OPERATION_SPECS`;本文件只剩 lookup/authorize/backend_available/supported_operation_catalog 的薄适配层。搬移方向正确(单一事实来源在 SDK),净减 560 行。 |
| crates/astrcode-extensions/src/host_router/context.rs | +156/-69 | 核心功能 | session state 读写改 typed wire 请求,写入改原子写(write_file_atomic)+ per-path AsyncMutex 写门(SessionStateWriteGates,Weak 引用防泄漏),读侧加强制大小上限 StateTooLarge;event emit 带 EventDeliveryReceipt(Accepted/LivePublished/Persisted)回执与按 EventSendError 分类的错误码。均为真实健壮性提升。 |
| crates/astrcode-extensions/src/host_router/extension_http.rs | +45/-37 | 机械适配 | 小改:invoke 全异步化(去 block_on_async)、错误码字面量改 WireErrorCode 常量、dispatcher 优先取调用上下文 scoped 的 public_http_dispatcher、补 group 不匹配的 invalid_group_operation 守卫。无独立新功能。 |
| crates/astrcode-extensions/src/host_router/llm.rs | +269/-114 | 核心功能 | LLM host op 重写:typed HostLlmChatRequest 解析(拒绝空 messages、maxOutputTokens=0),新增 invoke_event_stream 把 LlmEvent 全量映射为 ModelStreamEvent(Started/Retrying/Recovered/ToolCall*/Usage/Completed/Failed,provider 提前关闭补 StreamClosed 终态),LlmError→wire payload 保留稳定错误码/retryable/details,支持 LlmProviderBindings 作用域 provider。附错误码稳定性测试。 |
| crates/astrcode-extensions/src/host_router/network.rs | +154/-150 | 核心功能 | 错误模型改 WireErrorCode + retryable 标志(OutboundNetworkError 重构),限值常量收拢到 SDK 共享(HOST_NETWORK_MAX_BYTES 等),deadline/cancel 的 biased select 收敛为 super::run_until_deadline,入参改由 SDK 类型带默认值并显式校验 timeout/max_bytes 范围。含两处行为变更见存疑。 |
| crates/astrcode-extensions/src/host_router/path.rs | +93/-31 | 核心功能 | 新增 canonicalize_host_path:绝对路径不再强制工作区沙箱(仅拒绝 `..` 与 NUL),相对路径维持原沙箱;读写两侧共用 validate_relative_path_components 防错误码漂移,错误码改 WireErrorCode,附绝对路径/穿越/NUL 测试。 |

## 存疑项

- `crates/astrcode-extensions/src/host_router/network.rs`:两处未在 PR 说明中点名的行为变更。其一,Manual redirect 下 3xx 响应原先把 body 置空,现在总是读取并返回 body——放宽了契约,调用方可能开始依赖此前必然为空的 body,建议确认是否有意并在 changelog/文档注明;其二,重定向上限从 `previous.len() >= 10` 改为 `> 10`(即允许恰好 10 跳),虽补了 `redirect_limit_allows_exactly_ten_hops` 测试佐证是 off-by-one 修正,但属于对外可观察行为变化,建议确认 MAX_REDIRECTS 语义(「最多 10 跳」还是「少于 10 跳」)。

## 批次小结

这批是 S5R 3.0 重构的宿主侧核心:capability 元数据单点化到 SDK、host invoke 全异步化、错误码/重试语义统一为 WireErrorCode、资源租约与 generation gate 等生命周期防护,整体价值高且方向一致,没有可删的投机性代码。两个大文件(host_router.rs、capability.rs)的体量主要是搬移+测试,非新增复杂度。可推迟讨论的只有 network.rs 两处行为变更的确认(见存疑),其余建议保留。
# 批次 19:host_router 进程/会话/工作区组(process、session、workspace 及新增子模块)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extensions/src/host_router/process.rs | +352/-212 | 核心功能 | 一次性 spawn 路径从 JSON 手解改为 typed `HostProcessRequest`,错误码统一 `WireErrorCode`;新增 ProcessStart/Read/Input/Status/Promote/Kill/List 分发(长驻句柄,实现在在 process_handles.rs);spawn 改用 `SupervisedCommand`,`run_until_deadline` 上提共用,新增 spawn 前 cancel/deadline 检查;测试同步更新。错误码替换部分属机械适配,但整体是新 host operation 面的一半,值得。 |
| crates/astrcode-extensions/src/host_router/process_handles.rs | +957/-0 | 核心功能 | 全新文件:session 维度的长驻进程句柄——增量输出环形缓冲(1MB 上限)、`promote` 把调用级生命周期提升为 session 级、按 session/extension 清理、句柄数与并发上限。是 S5R 3.0 进程能力的核心新增,体量合理,无搬移成分。 |
| crates/astrcode-extensions/src/host_router/session.rs | +1049/-367 | 核心功能 | invoke 从 Capability 枚举改 HostOperation 全 typed 分发;新增 root session 三件套(create/state/submit_turn + `authorize_owned_root` 归属校验)、`SessionControlQueueOrStart`/`DeferContext`(PR 主线新 host op)、history list/transcript/token_usage/provider_messages、events 游标分页、lineage 可见性过滤(`visible_history_sessions` 环检测)、`wait_for_result` 在 peer IO 线程上的死锁防护。主线新功能 + 类型化重写,值得;`Box::pin` 各分支避免巨型 future,合理。 |
| crates/astrcode-extensions/src/host_router/session_inspect.rs | +111/-89 | 机械适配 | 返回值从 `serde_json::Value` 改 typed DTO、错误码 wire 化、跟随 core 的 `transcript`→`model_context` 重命名与 `list_all_session_summaries` 改名;测试断言从 camelCase 改 snake_case 并补了 agent 状态映射用例。纯跟随接口变化,必要但无独立价值。 |
| crates/astrcode-extensions/src/host_router/tool_result.rs | +74/-0 | 核心功能 | 新文件:session 作用域 tool-result artifact 分页读取(byte_offset/max_bytes),`ToolResultArtifactError`→`WireErrorCode` 边界映射。小而完整,对应 ToolOutcome 持久化链路,值得。 |
| crates/astrcode-extensions/src/host_router/wire.rs | +126/-0 | 核心功能 | 新文件:领域错误(LlmError/SessionApiError/StorageError/OutboundNetworkError)→线缆错误码的单点映射 trait + `wire_payload`,含一致性单测。符合「边界做映射」规则,消除了各 API 各自拼字符串错误码的旧做法,价值明确。 |
| crates/astrcode-extensions/src/host_router/workspace.rs | +1560/-330 | 核心功能 | typed wire 化之外有大量实质新增:read 支持图片(base64)/二进制/文本行分页;write/edit/apply-patch 接入 `FileObservationStore` 读写一致性防护;写路径走 `run_blocking_io_to_completion`(取消后仍完成,避免持久化半成品);`write_file_atomic` 替换 no-follow 写;grep 支持多行/上下文行/path filter 与 ignore-aware walker;放开 workspace 外绝对路径读写。体量大但均为 host workspace API 的能力升级,含 2 个新测试;错误码/类型替换部分属机械适配。 |
| crates/astrcode-extensions/src/host_router/workspace_patch.rs | +497/-0 | 核心功能 | 新文件:host 边界的 unified-diff 应用——自研解析器(hunk/CRLF/ trailing newline 保留)、按文件独立成败的 change 输出、接入 observation 校验。功能值得;自研解析器约 150 行,生态中缺少高质量 apply crate,自研可接受但属长期维护点。 |

## 批次小结

这批是 S5R 3.0 host 路由层的核心承重部分,整体都有价值,没有可删/可推迟项:8 个文件共同完成「Capability 枚举 + 手解 JSON + 字符串错误码」到「HostOperation + typed wire 请求 + `WireErrorCode`」的切换,并在此之上落地了四条主线新能力——长驻进程句柄(process_handles + process.rs 分发)、root session 与 queue_or_start/defer_context(session.rs)、workspace 的 observation 防护/原子写/图片读取(workspace.rs)、unified-diff 应用(workspace_patch.rs)。wire.rs/tool_result.rs 两个小文件边界清晰、职责单一。两类可议但不构成删改建议:(1) workspace_patch.rs 的 unified-diff 解析器为自研,后续 diff 边界 case(如 mode change、binary patch)的维护成本需留意,但当前生态无更好选择;(2) workspace.rs 与 session.rs 体量均已超 1500 行(diff 后),后续若再加 host operation 可考虑按子域再拆,本次不必动。session_inspect.rs 是纯机械适配,已确认无独立逻辑变更。
# 批次 20:astrcode-extensions 加载器与 handler 解析重构

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extensions/src/lib.rs | +5/-6 | 机械适配 | 模块声明与 re-export 跟随重构调整:删 `remote_manifest`/`session_query`,新增私有 `s5r_handler` 与 feature-gated `testing`;`HostBackends` 替代 `build_host_router`,`ExtensionHostServices` 下线。纯接口面收敛,无独立价值但必要。 |
| crates/astrcode-extensions/src/loader.rs | +357/-561 | 核心功能 | S5R 3.0 加载路径重写:`ExtensionRuntime::sync_sources` 的 reconcile-plan/protected_ids 机制改为 `prepare_extension_generation` + runner 源事务(失败整体不发布,替代旧的 protected_ids 保留语义);候选身份从 `extension_id_hint` 变为权威 `extension_id` + `ensure_candidate_identity` 校验;新增 transport feature 准入(`admit_candidate`)、配置参与指纹(`configured_source_fingerprint` 规范化 JSON 哈希);manifest 解析从裸 `serde_json::Value` 换成 `ExtensionPackageManifest` 类型;`ExtensionLoader`/`LoadExtensionsResult` 等死代码删除,净减 200 行。是本次重构的核心收益之一,值得。 |
| crates/astrcode-extensions/src/process_supervision.rs | +4/-0 | 机械适配 | 给 `SupervisedChild` 补一个 `wait()` 访问器,供 `host_router/process_handles.rs:535` 在 select 中等子进程退出。最小且必要。 |
| crates/astrcode-extensions/src/remote_manifest.rs | +0/-193 | 架构搬移 | 旧版基于字符串 effect 名(`s5r::effects::HandlerResult`)的 wire 结果解析整体删除,由 `s5r_handler.rs` 的类型化版本取代;无丢失逻辑,旧的宽松「未知 effect 一律 Allow」语义被有意收紧。 |
| crates/astrcode-extensions/src/s5r_handler.rs | +361/-0 | 核心功能 | remote_manifest 的类型化重写:改用 `wire::HandlerEffect`/`HandlerId` 枚举,未识别 effect 从静默降级为报错;新增 Ask、tool_input_transform、provider_contribution、pre/post_compact 等 S5R 3.0 新 effect 的解析,并带较完整的严格性单测(约 90 行)。虽属搬移,但语义收紧 + 新 effect 支持是实质功能增量,值得;测试占比合理。 |
| crates/astrcode-extensions/src/session_query.rs | +0/-198 | 架构搬移 | `StorageSessionQueryFactory` 随 SDK 侧 `session_query` 模块整体下线而删除;已确认全仓库 `crates/` 下无 `SessionQueryFactory`/`SessionQuery` 残留引用。属于能力移除而非搬移,需依赖 SDK chunk 确认该 host service 是有意下线。 |
| crates/astrcode-extensions/src/testing.rs | +42/-0 | 测试 | 新增 feature-gated(`testing`)一次性装配入口 `extension_runner_with_extensions`,供 astrcode-server 集成测试不用直接碰 gated `register` API;调用方确实存在(server tests 多处)。小而必要。 |

## 批次小结

这批改动整体都有价值,且互相咬合:loader.rs 是 S5R 3.0 的事务化加载 + 传输准入核心,s5r_handler.rs/remote_manifest.rs 是 wire 结果解析的类型化换代(一增一删),lib.rs/process_supervision.rs/testing.rs 是必要的跟随适配。没有发现可以删、拆或推迟的部分;净减约 190 行,方向正确。

两个需要跨 chunk 交叉确认的点(本文件内不构成存疑):一是 session_query 能力下线是否为 SDK 侧有意为之(已确认无残留引用);二是 loader 的「发现/加载失败整体不发布新代」语义替代旧 protected_ids 逐扩展保留,行为差异需 runner 侧事务实现兜底(属 host_router/runner chunk 的审计范围)。
# 批次 21:astrcode-extensions runner 核心(注册/分发/custom event/host 调用)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extensions/src/runner/commands.rs | +224/-200 | 核心功能 | 斜杠命令子系统重写:删除 CommandSource(Extension/Skill 二分)与 dispatch/complete 兼容入口,命令上下文改走 SDK internal 工厂 + RuntimeHookCallContext;新增 host command intent 的声明/能力双重校验(`command_execution_is_authorized`/`admit_command_result`),ResolvedSlashCommand 用 Weak<HandlerIndex> 钉住代际。承载新能力语义,值得。 |
| crates/astrcode-extensions/src/runner/custom_event_control.rs | +309/-0 | 核心功能 | 新文件:custom event 消费者控制面(pause/resume/replay/skip-to-head、状态查询),基于 storage 的 EventConsumerState。新子系统的管理 API,与 delivery 配套,值得。 |
| crates/astrcode-extensions/src/runner/custom_event_delivery.rs | +912/-0 | 核心功能 | 新文件:session 级 custom event 持久化投递子系统——per-consumer lane 串行化、指数退避重试、quarantine 阈值、cascade 深度上限、checkpoint reconciliation、并发信号量与 quiescence。体量大但都是新机制本体,非搬移。 |
| crates/astrcode-extensions/src/runner/diagnostics.rs | +26/-15 | 机械适配 | check_health 改为经 extension_view() 等稳定代际后再快照(底层有 Notify 等待,非忙等);Skipped 收窄为 test/testing-only,record_* 收窄为 pub(crate)。跟随 publication 协议的合理适配。 |
| crates/astrcode-extensions/src/runner/host_invoker.rs | +581/-0 | 核心功能 | 新文件:ExtensionCallContextFactory 统一构造扩展调用上下文,经 HostRouter 路由 host operation,按 capability 门控 event_tx/session_ops;BoundCustomEventSink 带 generation gate/取消/资源租约校验。含从旧 mod.rs 事件 sink 演进的逻辑,但主体是 router 集成的新代码。 |
| crates/astrcode-extensions/src/runner/http.rs | +175/-43 | 核心功能 | 新增 GenerationPublicHttpDispatcher(OnceLock 绑定代际 index,退役后返回 NotFound)与 WeakRunnerPublicHttpDispatcher(避免 host router 反向持有 runner);公共分发逻辑提取为 dispatch_public_http_from_view。修复 dispatcher 生命周期/所有权问题,值得。 |
| crates/astrcode-extensions/src/runner/index.rs | +156/-96 | 核心功能 | HandlerIndex 重构:新增 tool_input_transform/provider_contributions/pre_compact/post_compact/custom_event 槽位;static_tools 改 StaticToolEntry 并携带 Arc<ExtensionGenerationEntry>;per-extension 的 capabilities/tasks/admission/gate 统一收进 extensions map。承载新 hook 类型与代际模型,是本轮重构的索引基座。 |
| crates/astrcode-extensions/src/runner/manifest.rs | +19/-4 | 机械适配 | ResolvedExtensionManifest 改为包 SDK 的 ExtensionManifest + ExtensionRegistrations 并加访问器(id/capabilities/required_transport_features)。纯跟随 SDK manifest 模型变化。 |
| crates/astrcode-extensions/src/runner/mod.rs | +1527/-767 | 核心功能 | runner 主体重写:publication 改 pending_generation + Notify 稳定提交;来源代际两阶段提交(prepare_source_generation/commit_with/abort);start 失败经 retirement 异步回滚并 catch_unwind;unregister 走 supervisor draining;run_recorded_hook 加 admission/draining/取消;新增 transform_tool_input、prepare/acknowledge_provider_request、pre/post_compact 分发;旧 BoundExtensionEventSink 迁往 host_invoker。**注意:删除了 update_extension_configs/notify_config_changed 运行期配置热更新**,配置改为 start 时 validate_config,热更新疑似由整代际 reload 替代,属行为变更需确认刻意。 |

## 批次小结

这批是 S5R3 runner 重构的心脏,整体价值高:custom event 持久化投递(delivery/control)、host 调用统一路由(host_invoker)、代际化注册/退休协议(mod.rs/index.rs/http.rs)都是新架构本体,不是机械适配;commands.rs 顺带补上了 host command 的能力校验,是实质安全增强。manifest.rs/diagnostics.rs 是合理的跟随适配。

可关注的两点(非阻塞):
1. **mod.rs 删除配置热更新**:`update_extension_configs`/`notify_config_changed` 整体移除,运行期改配置只能整代际重启扩展。若产品上仍有「改配置不重启扩展」的承诺,这是回退;若 S5R3 刻意用 reload 语义替代,建议在 PR 描述中明示。
2. 无明显可删/可推迟的部分;custom_event_delivery.rs 近 1k 行但功能内聚(retry/quarantine/checkpoint 都是投递语义必需),不建议拆出本 PR,否则 custom event 只剩半成品。
# 批次 22:astrcode-extensions runner 注册/退役/快照/监督器/工具适配

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extensions/src/runner/registration.rs | +25/-183 | 架构搬移 | 能力校验、生命周期 blocking 校验、扩展内重名/路由自冲突校验全部下沉到 SDK `Registrar::validate`(registrar.rs:432 起,已核实),本文件只保留跨扩展冲突检查(工具名、public 路由 /api 命名空间与跨扩展路由冲突);校验未丢失,搬移干净 |
| crates/astrcode-extensions/src/runner/retirement.rs | +435/-91 | 核心功能 | 退役流程重写:新增 `PendingRegistration` RAII(启动失败/取消时 drop 自动把资源移交退役 barrier,不会泄漏回滚)、`RetirementTicket`(按 retirement_id 隔离错误、可 await 单次退役结果)、错误聚合替代短路返回、退役时 deactivate generation gate + drain supervisor + 清理 host_router 实例资源;是 S5R3 会话代际(reload)正确性的核心 |
| crates/astrcode-extensions/src/runner/snapshot.rs | +77/-34 | 核心功能 | 注册表快照重写:新增 generation/runtime_state(Initializing/Ready/Draining/Failed/Stopped,来自 supervisor)、required_transport_features、custom_events/subscriptions 字段;读取时钉住稳定代际(`is_stable_generation` 循环)避免 reload 中途读到撕裂快照,值得 |
| crates/astrcode-extensions/src/runner/supervisor.rs | +279/-0 | 核心功能 | 新文件:每扩展实例的生命周期监督器——semaphore 准入(容量 1024)、CancellationToken draining、watch 快照、BeginDraining 等待全部在途调用结束的优雅下线;带一个 draining 拒绝新调用的单元测试。是 retirement/snapshot 新语义的基础,必要 |
| crates/astrcode-extensions/src/runner/tests.rs | +2279/-642 | 测试 | 大头是 fixtures 随新 SDK API(Registrar/manifest/StartContext/StopContext)机械重写;新增约 8 个高价值并发回归测试(reload 取消在途调用、取消的 start 移交退役 barrier、panicking start 回滚顺序、retirement ticket 按代际隔离错误、退役只清理本实例资源、未发布扩展不能发 host 事件等,36→44 个 tokio 测试)。fixture 重写不可避免,新增测试直接覆盖本批核心功能 |
| crates/astrcode-extensions/src/runner/tool_adapter.rs | +379/-157 | 核心功能 | HandlerTool 全面重写:执行/plan 走 generation admission(退役期间拒绝/中断)、新增 `plan()` 链路(ToolPlanContext→ToolPlan)、超时改由 `ToolExecutionPolicy.timeout` 驱动、执行上下文改经 wire `tool_context` 构造(资源租约、模型分层、事件声明按能力裁剪);`normalize_stringified_booleans` 搬到 astrcode-session::tool_registry(已核实);错误映射扩展 Draining/Cancelled/Host/InvalidInput 并带 WireErrorCode 元数据(附单测) |
| crates/astrcode-extensions/src/runner/tool_catalog_cache.rs | +221/-0 | 核心功能 | 新文件:工具目录快照按 scope 缓存——LRU(上限 128)、partial 结果 30s 后重试、构建去重(Hit/Wait/Build 三态 + permit drop 唤醒等待者);带完整单测。防止并发会话重复跑 tool discovery,有价值 |

## 批次小结

这批是 S5R3「代际化扩展生命周期」的核心实现:supervisor(准入/排空)→ retirement(ticket/RAII 回滚)→ snapshot(稳定代际视图)→ tool_adapter(所有调用走 admission)四层互相咬合,加上 tool_catalog_cache 和 registration 校验下沉,整体方向一致、无冗余。所有文件都有明确价值,没有可删/可推迟的部分。

两点提示(非阻塞,不构成存疑):
- registration.rs 大幅瘦身依赖「SDK 端 validate 真的覆盖了搬走的全部检查」——已抽查 registrar.rs:432-530,能力校验、blocking 生命周期、扩展内重名均在,HTTP 路由自冲突检查也在 SDK 侧(registrar.rs:642 一带),搬移无损。
- tool_adapter.rs 中 `#[cfg(any(test, feature = "testing"))]` 的 `tool_catalog_snapshot_typed` 与 retirement.rs 中 cfg 门控的 `operation_gates` 表明 direct-register 路径仅供测试,生产路径走别的入口;若其他 chunk 审计到 mod.rs/host_invoker.rs 可交叉确认生产注册路径确实不经这些 cfg 门控代码。
# 批次 23:astrcode-extensions 的 s5r_ext 模块与其测试

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-extensions/src/s5r_ext/mod.rs | +620/-499 | 核心功能 | `S5rExtension` 整体重写为 S5R 3.0 装配层:`load` 改吃类型化的 `ExtensionPackageManifest`,`register` 直接按 session 的 registration 动态注册工具/命令/自定义事件 handler,`manifest()` 由注册信息构建;新增 `DEFAULT_INVOKE_TIMEOUT`(120s)给未声明超时的工具兜底。是 host 侧接入 v3 会话的核心 wiring,值得。 |
| crates/astrcode-extensions/src/s5r_ext/protocol.rs | +0/-4 | 机械适配 | 删除 `S5R_PROTOCOL_VERSION` 常量;3.0 改为特性协商,全库已无引用,删除合理。 |
| crates/astrcode-extensions/src/s5r_ext/session.rs | +0/-749 | 架构搬移 | 旧 stdio Peer 会话删除,由 `v3_session.rs` 承接;不是纯改名,是被重写取代。 |
| crates/astrcode-extensions/src/s5r_ext/session_support.rs | +152/-0 | 核心功能 | 从旧 session.rs 抽出 stderr drain/重入守卫等共享辅助,并新增按 invoke_id 的 `InvokeContext` 映射 + detached 上下文解析(`prepare_host_invoke`/`resolve_host_invoke_context`),支撑 turn 级归因;搬移中含新语义,合理。 |
| crates/astrcode-extensions/src/s5r_ext/v3_session.rs | +761/-0 | 核心功能 | S5R 3.0 宿主侧会话重写:initialize/activate 两段握手、continuation 深度上限(16)、invoke 超时、`PeerHandle` 入站调用与模型流事件转发、进程监管。重写主线,值得。 |
| crates/astrcode-extensions/tests/loader_integration_test.rs | +555/-105 | 测试 | 适配新 loader API(`prepare_extension_generation` + 世代提交),并新增 transport profile 准入、原子重载世代一致性、失败批次丢弃、禁用候选跳过、认证 HTTP 准入等用例;净增覆盖,值得。 |
| crates/astrcode-extensions/tests/s5r-guest/Cargo.lock | +154/-357 | 生成物 | guest demo 锁文件随依赖变更重新生成(去掉 anyhow、引入 astrcode-extension-worker 等),必要产物。 |
| crates/astrcode-extensions/tests/s5r-guest/Cargo.toml | +4/-1 | 配置 | edition 升到 2024、声明 rust-version 1.88,新增 `astrcode-extension-worker`(testing feature)与 `futures-util` 依赖,配合 worker crate 拆分。 |
| crates/astrcode-extensions/tests/s5r-guest/src/main.rs | +292/-99 | 测试 | guest demo 适配新 `worker_prelude` API(类型化 capability、`CustomEventDeclaration`、`ToolPlannerFn` 资源规划),新增 `call_context`、`session_state_roundtrip` 等探针,为 e2e 新用例提供被测体。 |
| crates/astrcode-extensions/tests/s5r_e2e_test.rs | +422/-283 | 测试 | 适配 `HostBackends`/新 runner 装配,删除旧 `s5r_ping_health`,新增 handshake 包 id 不匹配拒绝、工具执行策略整调用超时、session state typed roundtrip、runner 激活前 health 不可用等用例;覆盖 3.0 新行为,值得。 |
| crates/astrcode-extensions/tests/turn_scoped_host_models.rs | +261/-0 | 测试 | 新增:turn 作用域 host 模型测试(TaggedProvider/LiveProvider 模拟 provider 发布与热切换),对应 mid-turn 模型绑定能力,值得。 |
| crates/astrcode-extensions/tests/workspace_read_security.rs | +0/-114 | 测试 | 删除独立集成测试;路径穿越/符号链接逃逸/超限文件的同等断言已移入 `host_router.rs` 与 `host_router/workspace.rs`、`host_router/path.rs` 的单元测试,无覆盖丢失,删除合理。 |

## 批次小结

这批是 S5R 3.0 重写在宿主 crate 的核心落点:`s5r_ext` 三个源码文件(session 删除、v3_session 重写、mod 重新装配)加 session_support 的上下文归因新逻辑,都是主线必要改动;protocol.rs 常量和 workspace_read_security.rs 的删除均有去向(特性协商、host_router 单元测试),没有丢覆盖。测试侧除机械适配外净增了传输准入、原子重载、握手校验、超时、turn 级模型等真实新用例,质量高于纯适配。未发现可删/可推迟的部分;唯一可留意的是 guest demo 锁文件继续入库(此前即如此),属于既有惯例而非本 PR 新引入的问题。
# 批次 24:astrcode-protocol 线缆契约重构

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-protocol/examples/generate-typescript.rs | +21/-9 | 机械适配 | TS 导出清单跟随 wire/http DTO 改名(CommandSource→CommandAvailability/CommandExecution、ExtensionEventDecl→CustomEvent*、新增 AgentSessionUpdateDto/SessionCommandKind)同步更新,无独立逻辑。 |
| crates/astrcode-protocol/fixtures/conversation-reducer.json | +18/-12 | 测试 | reducer fixture 适配新的 `AgentSessionUpdateDto`(spawned/progress 分两条 envelope、cursor 顺延)和 control state 去掉 compactPending/compacting/phase 的改动,属于契约变化的跟随更新。 |
| crates/astrcode-protocol/src/agent_session_link.rs | +97/-143 | 核心功能 | 把「全 Option 的 patch DTO + spawned/completed/failed/phase_only 构造器」重构为全量基线 `AgentSessionLinkDto` + 带 kind 标签的 `AgentSessionUpdateDto` 四 variant 枚举,增量事件只携带能变的字段并加 deny_unknown_fields,类型上杜绝非法 patch,是本轮契约收紧的核心改动之一。 |
| crates/astrcode-protocol/src/events.rs | +5/-11 | 核心功能 | 删除一批 `#[serde(default)]`(让旧字段缺省直接报错而非静默兜底)、`GlobalExtensionEvent`→`GlobalCustomEvent` 改名、命令信息由 `source: CommandSourceDto` 换成 `extension_id: String`,与 wire.rs 的契约收紧配套。 |
| crates/astrcode-protocol/src/framing.rs | +2/-7 | 核心功能 | JSON-RPC 帧缺 method/event 标签时从静默填 "unknown" 改为返回 serde 错误,并去掉 jsonrpc 字段默认值;小而正确的边界收紧。 |
| crates/astrcode-protocol/src/http.rs | +172/-185 | 核心功能 | HTTP 契约大改:新增 CustomEvent 订阅/消费者控制/状态 DTO、TransportFeatureDto、命令 availability/execution 形状;大量删 `serde(default)`;ToolDefinitionDto 去掉 execution_mode;CompactSessionResponse 从 accepted/deferred 简化为 compacted;测试整体迁出到 http/tests.rs。 |
| crates/astrcode-protocol/src/http/tests.rs | +158/-0 | 架构搬移 | http.rs 内联测试模块整体迁为子文件,断言跟随新契约更新(fixture 12→13 条 envelope、ToolDefinitionDto 缺 strict 反序列化报错等),搬移为主、少量适配。 |
| crates/astrcode-protocol/src/wire.rs | +64/-51 | 核心功能 | CommandSourceDto 拆为 CommandAvailabilityDto/SessionCommandKindDto/CommandExecutionDto;ApprovalModeDto 去掉 `Unsupported` 容错 variant 改双向映射;删多个枚举的 `#[default]`;ToolOriginDto 删 Builtin/Sdk(对应 astrcode-tools 删除);能力枚举增 SessionCommand/ToolResultRead、EmitEvents→EmitCustomEvents 改名。 |

## 批次小结

这批是 protocol crate 的契约收紧与重命名,整体方向一致且相互自洽:删 `serde(default)`/`#[default]`/`Unsupported` 容错,改用类型化枚举表达增量,和 PR 主线(内置工具删除、扩展能力重组)完全对齐,没有明显可删或推迟的部分。两个需要整个 PR 层面确认的点(不单列存疑):一是删 default 后旧客户端/旧持久化数据缺字段会直接报错,属于有意的破坏性契约变更,需确认 PR 已接受不向后兼容(如 framed 旧 session 数据、前端旧版本);二是 `agent_session_link.rs` 的测试用 `updates.map(...)` 依赖数组 `map` 返回值的顺序断言,写法偏紧凑但无误。
# 批次 25:astrcode-server — bootstrap / config 发布事务 / child session / handler

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-server/Cargo.toml | +3/-1 | 配置 | 移除已删除的 astrcode-tools 依赖;dev-dependencies 增加 astrcode-extensions 的 testing feature(供测试用 extension runner)。跟随主线重构,必要。 |
| crates/astrcode-server/src/acp/mod.rs | +4/-4 | 机械适配 | 跟随 HandlerError 枚举变化重排错误映射:CompactBlocked 升为中立的 40900,移除已删除的 CompactionSkipped/Compact 分支,新增 ConfigPublication 分支。无语义变化。 |
| crates/astrcode-server/src/bootstrap/mod.rs | +128/-154 | 核心功能 | 引导流程大改:扩展加载从 bootstrap 内联迁到 ConfigManager 的 initialize_extensions(走发布事务);host router 改为 HostBackends 注入;PT Y/后台 shell 清理改为 runner.cleanup_session_resources(HostResourceCleanup);新增 transport_profile;删除 disabled_extension_ids、bootstrap()、assemble_for_test。净减行数,职责更清晰,是本次重构的核心落点。 |
| crates/astrcode-server/src/bootstrap/server_system.rs | +1/-1 | 机械适配 | scheduler() 可见性 pub→pub(crate) 收紧。 |
| crates/astrcode-server/src/child_session.rs | +154/-79 | 核心功能 | 两处实质变化:①父链遍历改用 astrcode-core::session_lineage::collect_parent_chain(含环检测错误映射),去重本地实现;②级联关闭区分 DrainCompleted/AbortRunning 两种 ClaimedCompletionAction,正常完成的子会话不再一律 recycle,Keep 策略下继续调度队列并向父会话投递延迟完成通知(deferred child completion notification)。是 mid-turn 吸收/后台 agent 通知链路的支撑改动,值得。 |
| crates/astrcode-server/src/child_session/completion.rs | +3/-3 | 机械适配 | completion() 尾部表达式化简,纯风格。 |
| crates/astrcode-server/src/config_manager.rs | +282/-92 | 核心功能 | 配置更新改为发布事务:独立扩展候选代(prepare_extension_generation)先验证再落盘再提交,失败保留上一代;panics 捕获(提交阶段 panic 直接 abort 避免混代);新增 drain_transactions 供关闭时排空;移除 shell_timeout_secs 原子量与 notify_extensions_config_changed(shell 超时迁到扩展配置)。是本 PR 里设计含量最高的文件之一。 |
| crates/astrcode-server/src/config_manager/tests.rs | +352/-0 | 测试 | 新增 3 个针对发布事务的测试:验证通过+落盘后才发布候选、请求取消后事务仍持有到 shutdown、禁用扩展配置在 commit 前验证。直接覆盖上面的新机制,必要且对口。 |
| crates/astrcode-server/src/handler/actor.rs | +2/-3 | 机械适配 | auto-recap 分支改 let-chains 写法,无行为变化。 |
| crates/astrcode-server/src/handler/compact.rs | +4/-4 | 机械适配 | 跟随 ManualCompactOutcome→ManualCompactionOutcome 改名。 |
| crates/astrcode-server/src/handler/mod.rs | +2/-1 | 机械适配 | re-export 更新:ManualCompactionOutcome 改从 astrcode_session 引入,新增 CommandOutcome。 |
| crates/astrcode-server/src/handler/model_selection.rs | +9/-0 | 机械适配 | 为 config_manager 新增的 4 个 ConfigUpdateError 变体补错误映射文案,跟随改动。 |
| crates/astrcode-server/src/handler/prompt.rs | +16/-10 | 核心功能 | 行为变化:slash 命令解析上提到 mid-turn 注入之前——turn 运行中输入 "/compact" 等命令现在按命令执行(忙时拒绝)而非注入为 mid-turn 消息;原先只特判 /model。配合新测试,是有意的语义收紧。 |
| crates/astrcode-server/src/handler/recap.rs | +18/-18 | 机械适配 | transcript→model_context、generate→generate_request、ExtensionEvent→LifecycleEvent 改名适配;PostRecap 生命周期发射改复用 astrcode_session::emit_lifecycle_for_read_model,删掉手工拼 LifecycleContext。 |
| crates/astrcode-server/src/handler/session_command.rs | +24/-28 | 核心功能 | 命令调用改走 SessionCommandService::invoke_interactive_command + CommandOutcome:SelectModel 由 host 接管、CompactSession 未执行则报错;keybindings/status_items 改为从统一的 command_list 结果取,不再单独问 runner。配合 prompt.rs 的命令路由收口。 |
| crates/astrcode-server/src/handler/tests.rs | +447/-355 | 测试 | 大量机械适配(generate→generate_request、transcript→model_context、ExtensionEvent→LifecycleEvent、manifest builder),另新增 slash_compact_rejects_running_turn、unknown_slash_command_is_rejected_without_writing_user_input、session_commands_share_extension_resolution_and_transport_admission 及交互命令/busy compact 探针,覆盖 prompt/session_command 的新行为。适配与新增混合,整体值得。 |

## 批次小结

这批改动整体都有价值,且是本次 S5R 3.0 重构在 server 侧的核心落点:bootstrap/mod.rs 与 config_manager.rs 把扩展加载收口进配置发布事务(候选代先验证后提交),child_session.rs 区分正常完成与级联中止并补了父会话延迟通知,handler 层把 slash 命令解析/交互命令路由统一到 session_command 服务。机械适配类(acp、compact、recap、tests 大部分)都是跟随接口改名的必要适配,无冗余。

可斟酌的点(非阻塞):

- `config_manager.rs` 中「提交阶段 panic 直接 `std::process::abort()`」是强不变式保护,行为激进但有注释说明理由(避免混代),建议确认该策略已在文档或 review 中显式认可。
- `prompt.rs` 的行为变化(运行中输入 slash 命令不再注入 mid-turn)属于用户可见语义变更,建议确认 release note / 协议文档同步。
- 未发现可删或可推迟的整块改动。
# 批次 26:astrcode-server HTTP 层(路由/投影/SSE 流)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-server/src/http.rs | +0/-2 | 机械适配 | 删除 `router_with_event_publisher` 的 testing 再导出,跟随 server.rs 中 TestEventPublisher 的移除,合理。 |
| crates/astrcode-server/src/http/auth.rs | +5/-28 | 存疑 | 整个 Bearer 鉴权中间件被删除、服务器不再校验 Authorization 头,仅保留 token 生成兼容 run.json。这是安全语义变更而非重构:本地端口上任何进程都能调 `/api/shutdown`、提交 prompt。若是有意改为"本地信任"模型,应在 PR 描述/配置文档中明示;建议确认是否应由 http_main.rs 传入的 `TransportFeature::AuthenticatedHttp` 在 transport 层重新实施鉴权,否则该 feature 名不副实。 |
| crates/astrcode-server/src/http/projection/args.rs | +21/-75 | 机械适配 | 工具参数单行摘要从按工具名(agent/shell/read/grep/todoWrite 等)特判改为通用主参数键扫描(description/command/path/...),因为工具已外迁到扩展、server 不再认识工具名。方向合理,但展示质量略降(丢失 `$ cmd`、`{pattern} in {path}` 等格式),可接受。 |
| crates/astrcode-server/src/http/projection/blocks.rs | +81/-84 | 机械适配 | 跟随上游重命名:`TranscriptArtifactView`→`SessionArtifactView`、字符串 source→`TranscriptMessageOrigin` 枚举、SystemNote→Recap、`Arc<LlmMessage>`;逻辑等价,另有 let-chain 格式化。测试同步改写并保留覆盖。 |
| crates/astrcode-server/src/http/projection/live.rs | +49/-42 | 机械适配 | `ExtensionEvent`→`CustomEvent`、`AgentSessionLinkDto`→`AgentSessionUpdateDto` 全链路改名适配;顺手删掉 control 中恒为 false 的 `compact_pending`/`compacting` 字段(行为变化需前端配合,属协议契约变更)。 |
| crates/astrcode-server/src/http/projection/replay.rs | +16/-14 | 机械适配 | replay 投影跟随 CustomEvent 重命名,测试补 `source_fingerprint`、`StepStarted/Completed` 等新事件分支。 |
| crates/astrcode-server/src/http/projection/snapshot.rs | +128/-47 | 机械适配 | 适配 SessionReadModel 重构(transcript→model_context/presentation 分拆);新增 recap live delta 与重连 snapshot 形状一致性测试,这是本文件真正的增量价值,值得保留。 |
| crates/astrcode-server/src/http/routes/config.rs | +23/-26 | 机械适配 | 适配 ConfigUpdateError 新增变体(ExtensionValidation/ExtensionCandidate/Transaction)的 HTTP 映射;notify/reload 改由 config transaction 统一处理所以删掉手工调用;reload 时保留 transport_profile 是必要的正确性修正。 |
| crates/astrcode-server/src/http/routes/event_consumers.rs | +118/-0 | 核心功能 | 新文件:自定义事件消费者的 list/control(Pause/Resume/ReplayFromBeginning/SkipToStreamHead)HTTP 端点,错误映射完整(404/409/500 分层),是 S5R 3.0 事件订阅能力的必要对外接口。 |
| crates/astrcode-server/src/http/routes/extensions.rs | +22/-8 | 存疑 | DTO 适配(custom_events/required_transport_features)本身合理;但 `set_enabled` 不再重载扩展,只发 `ExtensionRegistryChanged` 通知且 `reload_errors = Vec::new()` 恒空——响应 DTO 的 `reload_errors` 字段沦为摆设,且扩展启用/禁用是否真正生效依赖 config transaction 隐式触发,需确认该链路确实会重载 registry,否则是功能回退;若确认无虞,建议后续清掉恒空字段。 |
| crates/astrcode-server/src/http/routes/mod.rs | +10/-13 | 机械适配 | 挂接 event_consumers 模块;`update_config` 增加新错误变体映射;删除已被 transaction 取代的 `notify_extensions_config_changed`。 |
| crates/astrcode-server/src/http/routes/models.rs | +6/-1 | 机械适配 | 模型测试调用从 `provider.generate` 改为 `generate_request(LlmRequest::new(...))`,纯接口跟随。 |
| crates/astrcode-server/src/http/routes/sessions.rs | +17/-42 | 机械适配 | 复用 `astrcode_core::tool::validated_tool_names` 替代本地校验(符合"先重用");keybindings/status_items 改从 command_list 取;适配 `ManualCompactionOutcome` 重命名与 CompactSessionResponse DTO 简化;fork_session 删掉 `turn_id` 兜底是协议清理。 |
| crates/astrcode-server/src/http/server.rs | +18/-43 | 核心功能 | 挂接两条 event-consumers 路由;删除 auth middleware 层与 testing 专用 TestEventPublisher(鉴权停用见 auth.rs 存疑项);删除 masked_token 单测属跟随清理。 |
| crates/astrcode-server/src/http/stream.rs | +31/-14 | 机械适配 | SSE 流适配 ReplayError 类型化(区分 cursor 失效 info 级与真实失败 warn 级,略改善可观测性);`ExtensionEvent`→`CustomEvent` 改名;抽 `already_replayed` 小函数,逻辑等价。 |
| crates/astrcode-server/src/http/stream/child_sessions.rs | +5/-5 | 机械适配 | 子会话进度 delta 从 `AgentSessionLinkDto::phase_only` 改为 `AgentSessionUpdateDto::Progress`,纯 DTO 跟随。 |
| crates/astrcode-server/src/http/stream/replay.rs | +59/-43 | 核心功能 | 把 SSE replay 的"出错就 (vec, true)"重写为类型化 `ReplayError`(InvalidCursor/CursorAhead/LimitExceeded/InvalidStoredCursor/Session),配合 `is_cursor_unavailable` 区分可恢复的 rehydrate 与真实故障;行为等价但错误语义清晰,是本批最有质量的改写之一。 |
| crates/astrcode-server/src/http_main.rs | +10/-1 | 核心功能 | 独立 HTTP server 二进制改用 `bootstrap_with` 并声明 `TransportProfile([AuthenticatedHttp])`,是新 transport profile 体系的必要接线(但与 auth.rs 鉴权实际停用之间的语义落差需核对)。 |

## 批次小结

这批改动整体都有价值,大部分是 S5R 3.0 重构(事件体系 CustomEvent 化、SessionReadModel 分拆、LlmRequest、config transaction 化)在 HTTP 层的必然跟随,无法拆出。真正的新增价值集中在三处:event_consumers.rs 新端点、stream/replay.rs 错误类型化、snapshot.rs 的 recap 一致性测试,均值得保留。

两处存疑建议在合并前处理:

1. **auth.rs / server.rs 鉴权停用**:删除 HTTP Bearer 校验是安全语义变更,混在"扩展运行时重构"PR 里容易被忽略;至少应在 PR 描述中声明威胁模型(仅监听 loopback?),并解释 `TransportFeature::AuthenticatedHttp` 与"鉴权已停用"的关系。
2. **extensions.rs `set_enabled` 的 `reload_errors = Vec::new()`**:恒空字段说明要么重载已由 config transaction 隐式覆盖(需确认),要么是功能回退;确认后应清理该 DTO 字段或恢复真实错误回报。

可推迟/拆分项:projection/args.rs 的通用化摘要带来了展示质量回退,可后续由扩展在 ToolPresentation 元数据里提供摘要来补回,不阻塞本 PR。
# 批次 27:astrcode-server 编排层(session_manager / turn_scheduler / command service / event bus)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-server/src/lib.rs | +2/-12 | 机械适配 | 收窄 crate 公共 re-export(ChildSessionCoordinator、ConfigManager、TurnScheduler 等不再导出),配合内部模块私有化;API 面收窄是正向清理。 |
| crates/astrcode-server/src/main.rs | +1/-1 | 机械适配 | bootstrap 选项从 `disabled_extension_ids`(写死禁用 ask-user)换成 `transport_profile`,跟随 S5R3 SDK 传输配置化。 |
| crates/astrcode-server/src/presentation.rs | +1/-1 | 机械适配 | 测试 `use super::*` 收窄为单个函数导入,纯风格 churn,价值近零但无害。 |
| crates/astrcode-server/src/protocol_mapping.rs | +108/-71 | 机械适配 | 边界 DTO 映射跟随契约重写:CommandSource 枚举删除改为 extension_id、custom event declaration/subscription 新映射、capability 改名(EmitEvents→EmitCustomEvents 等)、AgentSessionLink/is_compact_summary 去 Option 化。改动集中在边界映射层,符合 DTO 规则。 |
| crates/astrcode-server/src/queue_drains.rs | +4/-4 | 机械适配 | 嵌套 if 折叠成 let-chain,纯 rustfmt 风格,无行为变化,属可剔除的噪音但无害。 |
| crates/astrcode-server/src/server_event_bus.rs | +77/-74 | 核心功能 | 用通用 `CustomEventAudience::Global` 路由取代写死的 ask-user 事件转发(删 3 个硬编码常量),新增 `downstream_observers` 供 custom event 消费者派发;测试从 ask-user 特例改为 audience 路由矩阵,覆盖面反而更好。 |
| crates/astrcode-server/src/session_command_contract.rs | +36/-20 | 核心功能 | 命令契约重写:CommandSource 删除改 extension_id、新增 `CommandOutcome`(承载 SessionCommandIntent,区分交互/非交互传输)、CommandList 带 keybindings/status_items、删除 ManualCompactOutcome(compaction 归属 session crate)。是 host session command 链路的契约基础。 |
| crates/astrcode-server/src/session_command_service.rs | +168/-206 | 核心功能 | 内置 /compact、/model 命令从硬编码 `builtin_commands()` 迁移为扩展声明的 host command(SessionCommandKind + availability 传输门控);compaction 改调 `compact_manual_session` 并删掉了本地 CompactionStarted/Skipped/Failed live 事件编排(净 -38 行,逻辑下沉到 session crate,属架构搬移+简化);新增 `admit_resolved_command` 传输准入。 |
| crates/astrcode-server/src/session_manager.rs | +618/-87 | 核心功能 | 本批核心:接入 custom event 投递子系统(`bind_custom_event_runner`/`replay_custom_events`/`CustomEventObserver`,用 Weak 避免循环引用);新增 `recover_durability_for_operation`(不确定 durability 的重试恢复);close/delete 失败补偿扩展为 event lane 重激活 + custom event quiesce/resume;`create_for_extension` 透传 source_extension;ExtensionEvent→LifecycleEvent、transcript→model_context 改名;新增 4 个高质量测试(custom event 删除阻塞、嵌套 session replay、uncertain sync 精确重试一次)。 |
| crates/astrcode-server/src/session_operations.rs | +38/-4 | 核心功能 | 实现新 host operation `queue_or_start`/`defer_context`(NoActiveTurn 显式映射为 typed error);`cancel_turn` 返回 bool(是否真有 turn 被取消);create 走 `create_for_extension`。 |
| crates/astrcode-server/src/test_support.rs | +89/-5 | 测试 | 新增集成测试脚手架(ServerRuntimeTestExt、assemble_server_runtime、bind_extension_host_router_for_test 等),刻意复用生产 bootstrap 的 host-router 接线而非重写,注释说明了意图;删除 context_assembler/shell_timeout 参数是跟随 SessionRuntimeServices 瘦身。 |
| crates/astrcode-server/src/turn_registry.rs | +9/-42 | 机械适配 | 删除死代码 `force_kill_and_remove_if_running`,3 个仅测试使用的方法降为 `#[cfg(test)]` 私有,测试适配 LlmRequest/删 context_assembler。净删 33 行的可见性收窄,正向。 |
| crates/astrcode-server/src/turn_scheduler.rs | +20/-10 | 文档 | 主要是模块文档与 enum 注释重写,准确描述了 accepted→absorbed 注入管线(InjectOnly 落 UserInputAccepted、由 turn 在 step 边界吸收);另有一处 transcript→model_context 改名。文档与新行为一致,有价值。 |
| crates/astrcode-server/src/turn_scheduler/delivery.rs | +17/-1 | 核心功能 | `begin_session_operation` 串联 `recover_durability_for_operation`(操作准入前先恢复不确定的持久化);新增 `deliver_child_completion_notification` 把子 session 完成通知排除在递归 abort future 之外,注释说明了动机。 |
| crates/astrcode-server/src/turn_scheduler/lifecycle.rs | +115/-27 | 核心功能 | 新增 `resume_stale_execution`:从 durable step 恢复未完成 turn(已有 finish_reason 则补 finalize,否则补 interrupted tool results 后 resume),`needs_stale_repair` 同步覆盖 active_step;`abort` 返回 bool 级联子 session 取消状态;inject 改写 `UserInputAccepted`(mid-turn 吸收主线);余为 let-chain 格式化。崩溃恢复语义的关键改动。 |

## 批次小结

这批是 server 编排层的主战场,整体价值高:`session_manager.rs`、`turn_scheduler/lifecycle.rs`、`session_command_service.rs`、`server_event_bus.rs` 四处承载了本 PR 的实质新能力——custom event 订阅派发与重启 replay、不确定 durability 的准入恢复、stale turn 从 durable step 断点续跑、内置斜杠命令扩展化。删除量(如 session_command_service 净 -38 行、turn_registry 净 -33 行)多来自逻辑下沉到 session crate 和死代码清理,方向正确。`lib.rs`/`main.rs`/`protocol_mapping.rs` 是契约重写后的必然适配。可挑剔的只有两处纯风格 churn(`queue_drains.rs` 的 let-chain、`presentation.rs` 的 import 收窄),可剔除但无足轻重;没有发现可推迟或应拆出的部分,test_support.rs 的脚手架被本批及 http/handler 测试实际使用,不算投机性新增。
# 批次 28:astrcode-server 集成测试(extension/http/session ops/turn scheduler)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-server/tests/extension_integration_test.rs | +194/-134 | 机械适配 | 主体是跟随扩展 SDK 重写:`id()/capabilities()` → `manifest()`、`ToolHandler::execute` 位置参数 → `ToolContext` + 新增 `plan()`、`PreToolUseResult` → `PreToolUseAdmission`、context 结构体 → `RuntimeHookCallContext` 等 internal 构造器、`extension_runner_with_extensions` 测试装配。有真实增量:nonblocking 生命周期测试由 `sleep(50ms)` 改为 `Notify` 握手,并显式断言 dispatch 不等待 handler,消除了原来的时序脆弱性。值得保留。 |
| crates/astrcode-server/tests/http_routes.rs | +386/-167 | 测试 | 大量机械适配(manifest、`generate`→`generate_request`、`HttpContext`、`read_tool_result_artifact` 改名、TestEventStore 补齐 event-consumer 系列 trait 方法、`transcript`→`model_context`、`CommandSourceDto`→`extension_id`),但核心增量有三:(1) 新增 `event_consumer_http_control_…` 测试,覆盖 event-consumers list + pause/skip_to_stream_head/resume 控制端点,是本次新功能的正面对应测试;(2) `http_routes_require_bearer_token` 反转为 `do_not_require_auth_token`,与 server 侧「HTTP auth is disabled」的有意行为变更一致(已核实 server.rs 注释);(3) runtime 装配改为经 bundled source generation 加载扩展,与生产 reconcile 同 origin,测试更贴近真实路径。另删除了 `active_selection_rejects_unknown_approval_mode_with_structured_error` 负向测试,已核实 `approvalMode` 在 DTO 上改为强类型 `ApprovalModeDto` 枚举、非法值改由 serde 在边界拒绝,`invalid_approval_mode` 错误码已从代码库消失,删除合理;但新的 serde 拒绝路径(状态码/错误体)无测试覆盖,属轻微缺口。 |
| crates/astrcode-server/tests/session_operations_test.rs | +733/-64 | 测试 | 本批价值最高的文件。机械适配之外新增七组行为测试,全部对应 PR 主线功能:`queue_or_start` 空闲启动/FIFO 排队、`defer_context` 绑定活跃 turn/空闲返回 `NoActiveTurn` 且不落盘、`create_root_session` 持久化 `source_extension` 归属、两个 parent-abort 与 completion guard 竞态分类测试(已完成未 drain vs watcher 先 claim)。`inject_message` 测试扩展为验证 UserInputAccepted→step 边界吸收的完整语义(含 `accepted_seq` 回链)。GateLlm 增加 started 信号量,替换了原来的 sleep 轮询,降低 flaky 风险。 |
| crates/astrcode-server/tests/turn_scheduler_behavior_test.rs | +269/-59 | 测试 | 机械适配(`generate_request`、manifest、`model_context` 等)之外:`running_inject` 测试重写为「接受→step 边界吸收」两阶段断言并校验 pending_inputs 清空;`durable_queue_recovers_fifo` 扩展了 step attempt 重放断言和「已完成末步不再调 provider」的 repair_stale 场景;新增 `repair_stale_repairs_trailing_unanswered_tool_call`,验证崩溃尾部孤儿 tool call 补 `ToolCallFailed` + `TurnAbortedContext` 且 provider 可见上下文恢复合法;abort 返回值语义断言(活跃→true,重复 abort→false)。都是针对崩溃恢复/竞态路径的真实覆盖。 |

## 批次小结

这批是 server 侧集成测试,整体价值高:约一半的 diff 行是跟随 SDK/存储接口重写的机械适配(manifest 化、`generate_request`、`SessionStore` event-consumer 方法、`model_context` 改名),属于重构的必要成本;另一半是本次 PR 新功能(queue_or_start/defer_context、mid-turn 吸收、event-consumer 控制端点、repair_stale、abort 竞态分类)的正面对应测试,且多处用 `Notify`/Semaphore 握手替换 sleep 轮询,测试质量有实际提升。没有可删/可推迟的部分。

两处小观察(不构成删改建议):
- http_routes.rs 删除的 `invalid_approval_mode` 负向测试已核实为类型边界化后的合理删除(serde 拒绝非法枚举),但新拒绝路径无覆盖;
- http auth 关闭的行为反转已在 server 侧确认是有意变更,测试跟随正确。
# 批次 29:astrcode-session-projection 读模型拆分与批处理投影

本批是整个 `astrcode-session-projection` crate 的内部重构:原来 `model.rs` + `reducer.rs` 两个大文件按领域拆成 agents / execution / presentation / model_context / error 五个子投影模块,reducer 只做校验与分发;同时新增 `PreparedProjectionBatch`(先校验后应用的批量投影 API,配合 storage 的 `Arc<SessionReadModel>` 快照 copy-on-write)、transcript rewrite 指纹校验、durable custom event audience 校验。

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-session-projection/src/agents.rs | +90/-0 | 架构搬移 | 子 Agent 链接投影从 model.rs/reducer.rs 整体搬出,逻辑不变;唯一行为变化是 `tool_call_id` 由必填改为 `Option<ToolCallId>`,放宽了非工具触发的子会话场景,合理。 |
| crates/astrcode-session-projection/src/error.rs | +44/-0 | 架构搬移 | `ProjectionError` 从 reducer.rs 搬出,新增 `EmptyBatch`/`SequenceOverflow`/`InvalidDurableCustomEventAudience`/指纹不匹配等变体——这些是新校验功能的配套错误类型,值得。 |
| crates/astrcode-session-projection/src/execution.rs | +210/-0 | 核心功能 | 执行态投影从 model.rs/reducer.rs 搬出,但有实质新增:`PendingInput` 增加 `turn_id`(mid-turn steering 输入归属,对应 PR 的输入吸收主线)、新增 `ActiveStepView` 跟踪 StepStarted/Completed、`settle()` 统一清理;含一个针对 turn 归属的单元测试。 |
| crates/astrcode-session-projection/src/lib.rs | +17/-3 | 机械适配 | 模块声明与 re-export 跟随拆分调整;`validate_next_event` 不再对外导出(降为私有),新增导出 `PreparedProjectionBatch` 与新子投影类型,与拆分一致。 |
| crates/astrcode-session-projection/src/model.rs | +24/-305 | 架构搬移 | 瘦身为根组合类型:`SessionReadModel` 改为组合 model_context/presentation/execution 三个子投影,被搬走 ~300 行;注意 `SessionReadModel` 删掉了 `Serialize/Deserialize` derive(读模型只从事件重建、经 `Arc` 共享,storage 侧用法已核实一致),另删掉了 `SessionSummary` 各字段的注释,属可接受的精简。 |
| crates/astrcode-session-projection/src/model_context.rs | +539/-0 | 核心功能 | provider 上下文子投影:接收搬来的 transcript/compaction/usage 逻辑,实质升级为 `Arc<LlmMessage>` 共享 + `Arc::make_mut` 写时复制(llm-message-sharing 设计)、`source: Option<String>` 换成类型化的 `TranscriptMessageOrigin`、新增 rewrite 前缀指纹校验(防 stale compact 覆盖并发 tail);含 usage 锚定单元测试。是本批含金量最高的文件。 |
| crates/astrcode-session-projection/src/presentation.rs | +72/-0 | 架构搬移 | 展示事实(first_user_message、artifacts)从 model.rs 搬出;`TranscriptArtifactView` 改名 `SessionArtifactView`,`SystemNote` 变体改为 `Recap`(对接 `RecapGenerated.source`),改名语义更准确。 |
| crates/astrcode-session-projection/src/reducer.rs | +122/-445 | 核心功能 | 从巨型 match 改为「校验 + 分发到子投影」:新增 `PreparedProjectionBatch::prepare/apply`(批量校验、seq 分配、批内 rewrite 指纹链式校验、`Arc::make_mut` 原地应用),新增 durable custom event 必须 session audience 的校验,seq 计算从 `saturating_add` 改为溢出报错;被拆走的 ~440 行进入各子模块。 |
| crates/astrcode-session-projection/src/reducer/tests.rs | +440/-149 | 测试 | 测试随新 API 重写:旧的 pending-input/usage 测试移到 execution.rs/model_context.rs 模块内,本文件改为覆盖 `PreparedProjectionBatch`(校验失败不变异、未共享快照不克隆、批内连续 rewrite 指纹链)、stale 指纹拒绝、custom event audience 校验等,覆盖的是新引入的失败模式,不是凑数。 |

## 批次小结

这批改动整体价值高,是 PR 主线(读模型支撑 owner_lease/批量持久化、mid-turn 输入、llm-message-sharing)在 projection 层的落地,不是为拆而拆:拆分让每个子投影的 `apply_event` 可独立测试,reducer 的批处理 API 和指纹校验是真实的新增正确性保障。没有明显可删/可推迟的部分;唯一可挑剔的是 `model.rs` 顺手删掉了若干字段注释(精简合理但属附带改动)。无存疑项。
# 批次 30:astrcode-session compaction 拆分与 turn 管线适配

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-session/Cargo.toml | +1/-1 | 配置 | uuid 提为正式依赖、dev-dep 去掉 astrcode-ai,跟随 crate 内依赖变化,合理。 |
| crates/astrcode-session/src/compaction.rs | +0/-800 | 架构搬移 | 800 行单文件删除,内容拆进 compaction/ 目录;不是纯搬移,伴随状态机重写(见 pipeline.rs),删除本身合理。 |
| crates/astrcode-session/src/compaction/circuit_breaker.rs | +230/-0 | 核心功能 | 熔断器在搬移基础上实质增强:新增 CompactAttemptPermit RAII(半开探测被放弃时 Drop 自动归还,修复旧版卡死半开的问题)、configure() 动态调参、NotAttempted 不重置失败计数;含 4 个针对边界的新测试,值得。 |
| crates/astrcode-session/src/compaction/manual.rs | +59/-0 | 核心功能 | idle session manual compact 入口,改为复用 CompactionPipeline 而非旧 compact_idle_session 的独立流程;薄封装,消除第二套状态机,值得。 |
| crates/astrcode-session/src/compaction/mod.rs | +17/-0 | 架构搬移 | 新目录的模块声明与导出收敛(多数 pub(crate)),合理。 |
| crates/astrcode-session/src/compaction/persistence.rs | +39/-0 | 核心功能 | 抽出 durable 提交边界:retained 必须是 transcript 后缀的校验 + prefix fingerprint 计算 + emit_durable_and_sync,把旧 commit_compaction 的持久化段独立成可命名边界,值得。 |
| crates/astrcode-session/src/compaction/pipeline.rs | +427/-0 | 核心功能 | 本批核心:manual/auto/reactive 共用的同步状态机,保证恰好一个 started + 一个 terminal live event,接入 PreCompact contributions/Block 与 PostCompact dispatch;取代旧 run_compaction 的散装流程,值得。 |
| crates/astrcode-session/src/compaction/pipeline/tests.rs | +444/-0 | 测试 | 新测试(含 fake EventLog + fake TurnHooks)验证 durable→hook 顺序与每种 outcome 恰好一个 terminal 事件;覆盖重写的关键不变式,值得,体量与风险匹配。 |
| crates/astrcode-session/src/compaction/turn.rs | +238/-0 | 核心功能 | turn 侧编排:plan_auto_compaction 增加本地估算 gate(低于 PROVIDER_COUNT_GATE_RATIO 不打 provider count_tokens)、breaker permit 与 pipeline 的接线、uncertain 持久化错误上抛;逻辑比旧版清晰,值得。 |
| crates/astrcode-session/src/deferred_tools.rs | +34/-30 | 机械适配 | 跟随 LlmMessage 改 Arc 存储(messages 参数 Vec<Arc<LlmMessage>>)及 ToolOrigin::Builtin→Bundled 重命名;测试合并为单测,价值在适配本身。 |
| crates/astrcode-session/src/early_tool_scheduler.rs | +13/-5 | 核心功能 | schedule() 新增 execute_early 参数,早期执行由 pipeline 的 can_execute_early 判定而非仅看 disposition;其余是测试 fixture 跟随 SharedTurnContext 变化的机械适配。功能点小但真实。 |
| crates/astrcode-session/src/lib.rs | +2/-3 | 机械适配 | 删除 session_compaction/session_tools 模块声明,transcript_rewritten_payload 收敛为 crate 内使用;跟随拆分。 |
| crates/astrcode-session/src/llm_stream.rs | +14/-15 | 机械适配 | 主体是 if-let 链(let-chains)风格改写;唯一实质变更是接线 can_execute_early → scheduler.schedule(prepared, execute_early),与 early_tool_scheduler 配套。 |
| crates/astrcode-session/src/payload.rs | +56/-13 | 核心功能 | transcript_rewritten_payload 增加 source_fingerprint(projection 重算不匹配即拒绝写入的完整性保护)与 retained_messages 保 origin;新增针对该契约的单测,值得。 |
| crates/astrcode-session/src/perf_snapshot.rs | +7/-3 | 机械适配 | 跟随 DurableEventPayload 新增 StepStarted/StepCompleted 及 ExtensionEvent→CustomEvent 重命名的穷尽匹配,必要。 |

## 批次小结

这批改动整体有价值,是本 PR 中质量较高的一块:compaction.rs(800 行)不是简单搬家,而是借拆分把 manual/auto/reactive 三条路径收敛到同一个 CompactionPipeline,并补上两个实质正确性改进——熔断器半开探测的 RAII permit(修复放弃探测后卡死)和 transcript rewrite 的 source_fingerprint 完整性校验,两者都配了直接测试。pipeline/tests.rs 444 行体量不小,但它锁定的是重写引入的「恰好一个 terminal 事件 + durable 先于 hook」不变式,属于新失败模式的覆盖,不算冗余。没有发现可删或可推迟的部分;deferred_tools/llm_stream/lib.rs/perf_snapshot 是跟随全 PR 接口变化(Arc 化消息、payload 枚举演进)的必要机械适配,无法从本 PR 拆出。
# 批次 31:astrcode-session 权限策略层(permission/)重构

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-session/src/permission/runtime.rs | +179/-40 | 核心功能 | 权限链语义重构:`PermissionDecision` 改名 `PolicyDecision`,新增链终态 `PermissionResolution::Ask { requirements }`;`priority()` 数值排序改为 vec 声明顺序;Ask 从"首个胜出"改为按序累积、Deny 覆盖、终态 Allow 结算;会话记忆(history)从链上策略改为 `decide` 内联按 rule_key 结算。测试同步重写,值得。 |
| crates/astrcode-session/src/permission/mod.rs | +47/-35 | 核心功能 | 链组装跟随重构:`build_default_chain` 不再持有 history(改由 `decide` 传入),记忆策略下链;用 `process_resource_ask`/`opaque_resource_ask` 替换 `shell_broad_access_ask`/`cwd_outside_write_ask`;`approval_history_path` 复用 `astrcode_core::config::defaults::extension_data_dir`(符合共享原语规范);测试更新验证多资源 Ask 累积。 |
| crates/astrcode-session/src/permission/session_approval_history.rs | +5/-223 | 核心功能 | 删除 `SessionApprovalHistoryPolicy` 与 `history_lookup_keys`(记忆查找逻辑移入 `runtime.rs::decide`,改为只结算产生该 Ask 的 rule_key,不再穷举候选 key),`ApprovalHistoryStore` 持久化本体保留并新增 `is_allowed_always`/`is_denied_always` 查询。语义收敛、去掉了"key 覆盖契约"这类脆弱测试,值得;顺带一处 let-chain 风格化。 |
| crates/astrcode-session/src/permission/process_resource_ask.rs | +69/-0 | 核心功能 | 新策略:取代按工具名硬编码的 shell 询问,改为按 `ResourceAccess::Host(Process)` 声明驱动,任意进程类工具都会 Ask,prompt 带 command。附带针对性测试,值得。 |
| crates/astrcode-session/src/permission/opaque_resource_ask.rs | +28/-0 | 核心功能 | 新策略:`ResourceAccess::Opaque`(host 无法细分的副作用)触发 Ask,rule_key 带工具名。与新 access 模型配套,值得。 |
| crates/astrcode-session/src/permission/shell_broad_access_ask.rs | +0/-49 | 架构搬移 | 整文件删除,职责由 `process_resource_ask.rs`(资源声明驱动)替代,属模型升级而非功能丢失。 |
| crates/astrcode-session/src/permission/cwd_outside_write_ask.rs | +0/-28 | 架构搬移 | 整文件删除;原检查的 `ResourceAccess::All` 在新 access 模型(File/Host/Opaque)中已不存在(已确认 `access.rs` 无 `All` 变体),删除与上游模型一致。 |
| crates/astrcode-session/src/permission/configured.rs | +7/-15 | 机械适配 | 跟随 trait 变化:删 `priority()`、`PermissionDecision`→`PolicyDecision` 改名;附带删去已失效的 priority 注释。 |
| crates/astrcode-session/src/permission/sensitive_file_ask.rs | +8/-15 | 机械适配 | 同上:删 `priority()`、类型改名,测试断言同步。fail-closed 逻辑未动。 |
| crates/astrcode-session/src/permission/git_path_ask.rs | +12/-12 | 机械适配 | 删 `priority()`、类型改名;另把 `.git` 判定提取为 `is_git_metadata_path` 自由函数,纯搬移原逻辑,无副作用。 |
| crates/astrcode-session/src/permission/default_read_approve.rs | +5/-9 | 机械适配 | 删 `priority()`、类型改名,测试同步。 |
| crates/astrcode-session/src/permission/git_cwd_write_approve.rs | +7/-11 | 机械适配 | 删 `priority()`、类型改名,测试同步。 |
| crates/astrcode-session/src/permission/session_tool_selection.rs | +5/-9 | 机械适配 | 删 `priority()`、类型改名。 |
| crates/astrcode-session/src/permission/fallback_allow.rs | +3/-7 | 机械适配 | 删 `priority()`、类型改名。 |
| crates/astrcode-session/src/permission/yolo_mode_approve.rs | +4/-8 | 机械适配 | 删 `priority()`、类型改名。 |
| crates/astrcode-session/src/permission/paths.rs | +12/-9 | 机械适配 | 与权限重构无直接关系的顺手调整:`push_path_value` 改 let-chain 写法、测试收窄 import 并重命名。改动小且等价,可接受但属夹带。 |

## 批次小结

这批改动整体都有价值,是本 PR 里一处独立的子重构:权限链从"priority 数值 + 首个非 Pass 胜出 + 记忆策略在链上"改为"声明顺序 + Ask 累积成多 requirement + 记忆只结算自己的 rule_key",配合上游 `ResourceAccess` 模型(File/Host/Opaque 替代粗粒度 All)用资源声明驱动的 `process_resource_ask`/`opaque_resource_ask` 替换了按工具名硬编码的 shell 询问和已失效的 cwd-outside 检查。拆分合理:语义改动集中在 `runtime.rs`/`mod.rs`/`session_approval_history.rs` 三个文件,其余 8 个策略文件是删 `priority()` + 类型改名的机械适配,测试覆盖同步到位(Ask 累积、Deny 覆盖、记忆逐 key 结算、多资源独立审批均有新测试)。

可以挑剔的点只有两处,均不阻塞:`paths.rs` 的 let-chain 风格化与主题无关,属夹带但极小;另注意语义边界变化——旧 `cwd_outside_write_ask` 会在资源声明为 `All` 时对一切工具 Ask,新模型下 File 类访问若落在 cwd 之外且未命中敏感/git 策略,会一路落到 `fallback_allow` 直接放行,是否需要在 File access 层补"cwd 外写"检查属于上游 access 模型的设计问题,建议 PR 描述中明确这一行为变化。

无需要删除、拆分或推迟的部分。
# 批次 32:astrcode-session crate 主 src(session 运行时/事件/turn/工具执行)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-session/src/projection_context.rs | +40/-79 | 机械适配 | 跟随 read model `transcript`→`model_context` 与 `SharedTranscriptMessage`(Arc 共享消息)改造;`with_input_token_anchor` 改传计数而非克隆前缀,删掉因此不再需要的 `committed_tool_result_content_len` 及克隆注释,是健康的瘦身。 |
| crates/astrcode-session/src/session.rs | +58/-30 | 核心功能 | `ExtensionEvent`→`LifecycleEvent`、artifact 读取改按 artifact_id+字节区间、删 `checkpoint()`、新增 `emit_durable_and_sync`(配合 sink 的 AppendAndSync)、`emit_live_required` 返回 EventId、lifecycle 派发前 `pin_extension_view`;是 S5R3 接口主线在 session 门面的落点。 |
| crates/astrcode-session/src/session_compaction.rs | +0/-32 | 架构搬移 | 整文件删除:`rewrite_transcript_for_compaction` 随 compaction.rs 拆目录迁到 `compaction/` 模块,无逻辑新增。 |
| crates/astrcode-session/src/session_error.rs | +8/-0 | 核心功能 | 新增 `uncertain_through_seq` 透出,是 sink `retry_uncertain_sync`(不确定写恢复)链路的必要一环,小而有价值。 |
| crates/astrcode-session/src/session_event_sink.rs | +519/-92 | 核心功能 | 本批最大实质改动:durable 提交按同类 batch(上限 32)合并写盘、新增 `AppendAndSync`/`append_and_sync`、`retry_uncertain_sync` 恢复不确定写、best-effort live 事件直发 observer 绕过 lane(有注释说明放弃严格同序的理由)、release 失败时回滚 inactive 标记;附大量新测试。方向正确,是存储层批量/同步语义在 session 侧的对接。 |
| crates/astrcode-session/src/session_extension_ports.rs | +5/-4 | 机械适配 | `ToolCatalogProvider` 从 Noop 占位变成显式构造入参,跟随 SDK runtime_ports 接口变化。 |
| crates/astrcode-session/src/session_lifecycle.rs | +6/-8 | 机械适配 | 事件类型改名适配 + `SpawnChildParams.tool_call_id` 改 `Option`(spawn 不再强制工具调用归属),另有一处 let-chain 格式化。 |
| crates/astrcode-session/src/session_prompt.rs | +42/-88 | 架构搬移 | 删掉 session 侧 base tool registry 缓存/单 flight 逻辑(随 session_tools.rs 迁到 Extension Runtime),`ResolvedToolRegistrySnapshot` 只留 `catalog_revision`;prompt 构建改收 `RuntimeHookCallContext`。净删行,职责上收到扩展运行时,合理。 |
| crates/astrcode-session/src/session_runtime.rs | +121/-20 | 核心功能 | 新增 `SessionScopedEventPublisher` 实现 `EventPublisher`/`EventSender`(session 级事件 ingress,供 queue_or_start 等 host 操作异步生产者使用);`CompactCircuitBreaker` 挂到 runtime state;`SessionToolCache` 引用随缓存迁移删除。 |
| crates/astrcode-session/src/session_runtime_services.rs | +304/-181 | 核心功能 | 引入 `RuntimeGeneration`:effective config、主/小 LLM、context assembler、extension generation 打包原子热替换,`pin_turn_generation` 保证单 turn 不混用不同代运行时对象(含防混代测试);同时删掉 `PostCompactEnricher` 与 session 侧 CompositeToolCatalog 拼装。是本次重构的关键正确性改动。 |
| crates/astrcode-session/src/session_setup.rs | +13/-26 | 机械适配 | prompt 收集改收 `RuntimeHookCallContext`(经 `runtime_prompt_build_context` 映射),删 `ToolCatalogCompleteness` 流转;纯跟随 SDK 接口。 |
| crates/astrcode-session/src/session_tools.rs | +0/-230 | 架构搬移 | 整个 `SessionToolCache`(registry 缓存 + build 单 flight + partial 重试)删除,注释明确「动态发现和 catalog 缓存由 Extension Runtime 负责」,纯职责搬移。 |
| crates/astrcode-session/src/session_turn.rs | +320/-89 | 核心功能 | submit 拆出 `spawn_prepared_turn` 并新增 `resume()`(重驱动 active step 的原 turn,配合 queue/mid-turn 主线);turn 全程固定 `pin_turn_generation` 的代际;hook context 带 turn_id/cancellation/llm_bindings;`TURN_ABORTED_SOURCE` 字符串改 `TranscriptMessageOrigin` 枚举。测试同步大改。 |
| crates/astrcode-session/src/steer.rs | +58/-55 | 核心功能 | mid-turn 输入机制重写:从「step 边界统计 user 消息条数」改为 `UserInputAccepted`→按 turn 归属吸收(absorbed)对账,`absorbable_inputs_for_turn` 按 accepted_seq 序过滤本 turn 输入;正是 PR 主线改动,测试覆盖归属过滤。 |
| crates/astrcode-session/src/test_support.rs | +6/-21 | 测试 | 测试桩跟随接口:`generate`→`generate_request`、删 `CountingPostCompactEnricher`、services 构造改 `new_with_context_assembler`。 |
| crates/astrcode-session/src/tool_deduplicator.rs | +8/-9 | 存疑 | 唯一改动是把嵌套 `if let` 合并成 let-chain,零行为变化、与 PR 主线无关,像编辑器/rustfmt 风格 churn;建议确认是否为统一格式化批次,否则可从 PR 剔除。 |
| crates/astrcode-session/src/tool_exec.rs | +175/-29 | 核心功能 | `TurnToolContext::for_turn` 改用固定代际的 `RuntimeGenerationView`,向 `ToolExecutionContext` 透传 turn_id、`ResourceLease`(来自 ToolPlan)与 cancellation_token,host services 注入 `llm_providers`;新增 context 透传端到端测试。是 ToolDefinition/plan/cancel 全链路在 session 侧的落点。 |

## 批次小结

这批是 S5R3 重构在 astrcode-session 的承重墙,整体价值高:三条主线(runtime generation 原子固定、事件 sink 批量/同步/不确定写恢复、mid-turn accepted→absorbed)都落在真实正确性需求上,且大量改动是净删行(session_prompt、session_setup、projection_context、session_tools、session_compaction 合计净删约 400 行),职责从 session 上收到扩展运行时的方向一致。两处缓存删除(session_tools.rs、session_prompt 内联缓存)依赖 Extension Runtime 侧确实接管了缓存,建议在合并前对照 astrcode-extensions/worker 侧确认无缓存真空导致每次 turn 全量重建工具表(若对方 chunk 已确认接管则无问题)。无可拆分/推迟的大块;唯一可剔除的是 tool_deduplicator.rs 的纯风格改动。session.rs 的 `emit_live` 直发语义弱化(放弃与 in-flight durable 严格同序)有注释与 `publish_live_required` 兜底,可接受。
# 批次 33:astrcode-session 工具管线与 turn 编排

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-session/src/tool_pipeline/commit.rs | +19/-86 | 核心功能 | 删除 `enforce_tool_result_message_budget`(按消息 200KB 总预算从大到小持久化),post hook ctx 改用 `runtime_post_tool_use_context` 构造,artifact 引用从 path 改为 artifactId。净删 67 行,功能简化真实,但删预算的行为影响见存疑。 |
| crates/astrcode-session/src/tool_pipeline/execute.rs | +73/-71 | 核心功能 | `AwaitApproval{prompt,rule_key,source}` 改为 `AwaitApprovals(Vec)` 并循环逐项请求审批(任一拒绝即终止),ApprovalHistory 记录逻辑不变;`max_parallel_tool_calls` 改从 ToolCalls 字段取(来自 RuntimeGenerationView)。是审批组合核心改动的执行侧,值得。 |
| crates/astrcode-session/src/tool_pipeline/mod.rs | +26/-1 | 核心功能 | 新增 `can_execute_early`:基于 `ToolPlan.resources()` 全为 Read/Search 才允许流式早执行(替代原先按工具类别的判断),并透传 `max_parallel_tool_calls`。小而关键的语义改动。 |
| crates/astrcode-session/src/tool_pipeline/prepare.rs | +397/-85 | 核心功能 | 本批核心:prepare 重写为 transform_tool_input → normalize_final_arguments → admission → plan → permission 五段式,admission/plan/permission 消费同一份 canonical 输入;扩展与核心审批要求可组合(`compose_permission_resolution`),session 审批记忆在扩展规则上同样生效。含约 200 行新测试(canonical input 一致性、审批组合),测试直接针对新失败模式,有价值。 |
| crates/astrcode-session/src/tool_registry.rs | +107/-42 | 核心功能 | `definition.execution_mode` 拆为独立 `execution_policy`;`resource_accesses` 同步接口换成 async `plan()`;严格 null 归一化从 execute 前移到 `normalize_final_arguments`,并新增 stringified boolean("true"/"TRUE"→bool)归一。归一前移是 prepare 五段式的前提,值得;测试同步更新。 |
| crates/astrcode-session/src/tool_results.rs | +35/-98 | 核心功能 | 删除按工具名硬编码的差异化内联阈值(read 永不持久化/shell 30K/grep 20K/默认 50K),统一为 30KB 单阈值;持久化摘要文案从"saved file + read 分页"改为"artifactId + read_tool_result 分页"。配合 commit.rs 删预算,是有意的行为简化,注释里给了统一预算的理由。 |
| crates/astrcode-session/src/tool_types.rs | +13/-7 | 机械适配 | `PreparedToolInvocation`/`ExecutableToolInvocation` 增加 `plan: ToolPlan` 字段;`AwaitApproval` 单审批改为 `AwaitApprovals(Vec<PreparedToolApproval>)`。纯类型跟随 prepare/execute 改动。 |
| crates/astrcode-session/src/turn_context.rs | +62/-55 | 机械适配 | SharedTurnContext 增 `turn_id`/`llm_providers`/`cancellation_token`;所有 hook ctx 构造收敛到 `hook_call_context()`(RuntimeHookCallContext + runtime_* 工厂),删除 `from_read_model` 全拒空链构造,改为独立的 `hook_call_context_for_read_model`。SDK wire 重构的适配,顺带消掉了旧构造的误用风险,干净。 |
| crates/astrcode-session/src/turn_publish.rs | +250/-102 | 核心功能 | 事件 ingress 从 unbounded 改有界(256)+ 背压显式返回;新增 `EventPublisher`/`EventDeliveryReceipt`/`send_confirmed`(durable 返回 Persisted{event_id,seq});`ExtensionEvent`→`CustomEvent`、`ExtensionEvents`→`TurnEventBridge` 改名。背压与投递回执是实质能力,测试覆盖 Full/Closed/receipt,值得。 |
| crates/astrcode-session/src/turn_runner.rs | +308/-171 | 核心功能 | 多处实质改动:StepStarted/StepCompleted durable 事件 + 从 active_step 恢复 step_index/attempt;mid-turn 输入由"计数探测"改为按 accepted_seq 吸收为 durable UserMessage(UserInputAccepted→absorbed 链路);provider request 增加 request_id + acknowledgement 结算;连续相同 input_tokens 的 frozen 视图告警;`Vec<Arc<LlmMessage>>` 零拷贝;compact circuit breaker 移到 session runtime;`timed_stage` 性能采样。行数大但每块都有命名触发器,是 turn 编排主干升级。 |
| crates/astrcode-session/src/turn_stages.rs | +67/-21 | 核心功能 | TurnState 增 step_index/attempt 恢复(接 ActiveStepView)、`tools_token_estimate` 缓存(可见集变更才重算,避免逐 step 序列化全部工具 schema)、input_tokens streak 计数;`synced_user_message_count` 计数器随吸收式改造删除。配合 turn_runner 的恢复与性能改动,值得。 |

## 批次小结

这批是 PR 在 session crate 内的主干改动,整体价值高:prepare 五段式 canonical 输入、多审批组合、ToolPlan 驱动的早执行判定、事件背压/回执、mid-turn 吸收式输入、step 恢复,每一块都对应 PR 背景里的主线目标,且关键路径都配了新测试(prepare 的 canonical 一致性、turn_publish 的 receipt/背压)。机械适配部分(turn_context、tool_types)是 SDK wire 重构不可避免的跟随成本,且顺带删掉了 `from_read_model` 空权限链这类隐患构造。

存疑项:

- **commit.rs + tool_results.rs 删除消息级总预算**:`MAX_TOOL_RESULTS_PER_MESSAGE_CHARS`(200KB)及按体积逐出逻辑被删,只留单结果 30KB 阈值。一轮内多个 30KB 以下结果可以无限累积进入 LLM history,原注释明确说这是刻意的总预算;新代码只靠 compaction/token 预算兜底,没有等价替代。建议确认 context 超窗后的 reactive compaction 是否就是设计上的兜底,若是则在 commit.rs 留一句注释说明,若否则属行为退化。
- 无其他可删/可推迟部分;turn_runner 的 `timed_stage` 采样虽小但独立,若要拆也可单独成 PR,不过 20 行体量不值得拆。
# 批次 34:astrcode-session 集成测试(common 脚手架 + compaction/mid-turn/settlement/resume/SSOT)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-session/tests/common/mod.rs | +47/-8 | 测试 | 共享测试脚手架:适配 `SessionRuntimeServices::new_with_context_assembler` 与 4 参 `SessionExtensionPorts::from_immutable_ports` 新签名;新增 `spawn_session_with_llm_assembler`(真实 token 阈值判定)供 compact_gate 测试使用。必要适配,无浪费。 |
| crates/astrcode-session/tests/compact_gate_provider_count.rs | +161/-0 | 测试 | 新增:auto-compact 门控的 provider `count_tokens` 调用纪律——关闭时不打、本地估算远低于阈值时不打、逼近阈值才付费调用。覆盖新门控逻辑的三个关键分支,值得。 |
| crates/astrcode-session/tests/compact_persist_conflict.rs | +19/-120 | 机械适配 | 主体是接口适配(`generate`→`generate_request`、`transcript`→`model_context`/`presentation`、`SystemNote`→`SessionArtifactView::Recap`、idle→manual compaction 重命名);删除 `transcript_rewrite_preserves_new_tail_events` 测试,因其直调的 `rewrite_transcript_for_compaction` API 已随 compaction 重构移除(已 grep 确认仓库无残留),同类竞态仍由保留的 `auto_compact_preserves_concurrent_tail_and_uses_summary` 覆盖。 |
| crates/astrcode-session/tests/mid_turn_absorption.rs | +419/-0 | 测试 | 新增:本 PR 核心功能「mid-turn 输入 accepted→absorbed」的写入侧协议合法性回归——工具轮未结算时插话不落 transcript、吸收发生在 step 边界、provider 第二轮请求中 tool result 紧跟 assistant tool_calls、异 turn 归属输入不被吸收。直接防事故场景,高价值。 |
| crates/astrcode-session/tests/provider_contribution_settlement.rs | +385/-0 | 测试 | 新增:provider contribution 结算纪律——仅 durable 成功后 ack、失败/同步失败/context 溢出/取消均不 ack、prepare 后 handler 重载不串改结算配对。覆盖新 settlement 机制的失败矩阵,高价值。 |
| crates/astrcode-session/tests/session_resume.rs | +83/-25 | 测试 | 大量接口机械适配(`ExtensionEvent`→`LifecycleEvent`、`Tool` 新增 `plan`、`tool_call_id` 改 `Option` 等);其中 `child_tool_selection_stays_within_parent_boundary_and_survives_reopen` 从「读 registry 快照断言为空」升级为真实跑 turn 并用 `RecordingToolsLlm` 断言 provider 实际收到的工具列表,验证强度实质提升。 |
| crates/astrcode-session/tests/ssot_turn_history.rs | +27/-27 | 机械适配 | 纯跟随接口变化:`generate`→`generate_request`、`transcript`→`model_context`、`update_effective`→`publish_runtime_generation_for_extension`、`LlmTokenUsage` 补 `input_accounting` 字段。无语义变化,必要。 |

## 批次小结

这批改动整体都有价值:三个新增测试文件(compact_gate_provider_count、mid_turn_absorption、provider_contribution_settlement)分别对应本 PR 的 compact 门控、mid-turn 吸收、provider contribution 结算三个核心新行为,且都覆盖了失败/竞态分支而非只测 happy path,是 PR 中最该要的测试。common/mod.rs 与 ssot_turn_history.rs 是接口重构的必要跟随适配。compact_persist_conflict.rs 净删 101 行属于删除已失效 API 的过时测试,已确认被删 API 在仓库无残留且竞态场景仍有覆盖,删除合理;session_resume.rs 在适配之外还增强了一个断言路径。无可删/可推迟的部分。唯一可留意的小点:common/mod.rs 中两个 `#[allow(dead_code)]` 是因共享模块被各集成测试 binary 独立引入所致,属测试脚手架常见做法,不算问题。
# 批次 35:astrcode-storage 基础设施(event_log / config_store / in_memory / 基准)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-storage/Cargo.toml | +10/-4 | 配置 | 新增 sha2(tool_artifacts 哈希)、tempfile 提为正式依赖(config_store 用)、dev 增 criterion 并注册 session_repo bench;description 去掉 snapshots(配合 snapshot 模块删除)。与改动配套,值得。 |
| crates/astrcode-storage/benches/session_repo.rs | +199/-0 | 测试 | 新增 criterion 基准:append(1/32/128 批量)、append_and_sync、冷打开(1k/10k 事件)、摘要列表(10/100/500 会话)。直接服务于本次重构的性能主张(批量写、单次扫描冷打开),有留存价值;但属非阻塞工具,若 PR 体积敏感可拆出单独提交。 |
| crates/astrcode-storage/src/config_store.rs | +37/-223 | 核心功能 | 删除 legacy JSON 配置回退/backfill 迁移逻辑与双格式读写,统一为 TOML;写路径改用带 fsync+目录 sync 的 replace_durable_file,崩溃一致性实打实增强。注意:这是行为变更——磁盘上仍只有 config.json 的老用户升级后配置会被静默重置为默认值,无迁移也无告警,值得在 release note 或 PR 说明中显式交代。 |
| crates/astrcode-storage/src/durable_write.rs | +60/-0 | 核心功能 | 新增共享落盘原语:临时文件+fsync+rename+目录 fsync(含父目录新建时的祖父目录 sync、非 unix 降级)。替代 config_store 里原先无 fsync 的 write_atomic,并供 consumer_state/tool_artifacts 复用,是「先重用再写新代码」的正例。 |
| crates/astrcode-storage/src/error.rs | +20/-0 | 核心功能 | 新增 DurabilityUncertain 错误变体(fsync 结果不确定时禁止继续写)+ uncertain_through_seq 访问器 + 批追加短返回的收敛兜底函数。服务于新的持久性契约,有价值。 |
| crates/astrcode-storage/src/event_log.rs | +238/-83 | 核心功能 | 核心改动四块:① 写入路径改为 PreparedProjectionBatch 批量原子追加(seq 连续性校验、批次一次编码一次写);② 冷打开合并为单次扫描(open 返回已校验事件给 projection 恢复,并先 fsync 确认 confirmed_len 再按字节截读,排除未确认的并发/崩溃尾记录);③ read_summary/replay_read_only 同样走 confirmed_len;④ EventLog 可见性收为 pub(crate)(配合 lib.rs 收口)。含 fault-injection(fail_next_sync / fail_next_open_sync)测试钩子,gating 正确(test / testing feature)。重构主线核心,值得。 |
| crates/astrcode-storage/src/event_log/tests.rs | +80/-4 | 测试 | 配套新测试:批量追加原子性+seq 连续、冷读在 confirmed_len 处截断(并发 append 竞态)、open 返回事件数断言;原有 malformed 流用例适配新签名。覆盖的正是新行为,非凑数。 |
| crates/astrcode-storage/src/in_memory.rs | +225/-50 | 机械适配 | InMemoryEventStore 跟随 SessionStore/Journal trait 新契约:append_event→append_events(PreparedProjectionBatch)、checkpoint→event_consumer_* 系列(consumer checkpoint/failure/quarantine/pause/reset 全量内存实现)、tool result 由 path 改 artifact_id;另加 fail_next_sync/sync_count 测试钩子。接口适配占大头,但 consumer 状态机是实打实的新逻辑(约 150 行),介于适配与功能之间,归类偏适配。 |
| crates/astrcode-storage/src/in_memory/tests.rs | +83/-2 | 测试 | 测试 consumer checkpoint 全流程(单调推进、revision 过期拒绝、pause、reset)+ 批量追加一票否决;顺带适配 transcript→model_context 字段改名。有价值。 |
| crates/astrcode-storage/src/lib.rs | +8/-5 | 架构搬移 | 模块可见性收口:event_log 降为私有、snapshot 模块删除(随 session_repo 重构)、新增 testing 模块导出;traits 新增 EventConsumer* 一组导出。纯结构跟随,无独立价值但必要。 |

## 批次小结

这批改动整体质量高、指向明确:event_log 的批量原子写 + 冷打开单扫描 + confirmed_len 截读是本 PR 持久层最有分量的部分,config_store/durable_write 把崩溃一致性从「rename 原子」升级到「fsync 全链」,in_memory/tests/bench 都是必要配套。可商榷的点只有两处:① config_store 删除 legacy JSON 回退是静默行为变更(老 config.json 用户配置重置),建议至少在 PR 描述/release note 中声明,或保留一次性只读迁移;② benches/session_repo.rs(199 行)与 Cargo.toml 的 bench 注册可以拆成独立提交,不影响本 PR 语义。除此之外没有可删或可推迟的部分。
# 批次 36:astrcode-storage session_repo 拆分与 durable consumer 状态

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-storage/src/session_repo.rs | +0/-1026 | 架构搬移 | 原单文件整体拆成 `session_repo/` 目录,删除本身无价值问题,是拆分的必要一半。 |
| crates/astrcode-storage/src/session_repo/mod.rs | +253/-0 | 架构搬移 | 仓库结构与 `get_or_open_meta` 等骨架基本来自原文件;新增 `open_lanes`(冷打开串行)、`canonical_dir`、`uncertain_durability`、`consumer_state_lane` 字段,属于新功能的承载点,合理。 |
| crates/astrcode-storage/src/session_repo/owner_lease.rs | +103/-0 | 架构搬移 | 与原 `session_repo.rs` 内联的 `SessionOwnerLease` 实现几乎逐行一致(全局 Weak 注册表 + `.astrcode-session-owner.lock` OS 排他锁),纯搬移,非新增价值。PR 描述里的「新增 owner_lease」对这批文件而言不准确——基线已有。 |
| crates/astrcode-storage/src/session_repo/projection.rs | +44/-0 | 机械适配 | `SessionProjection` 从原文件搬出,接口从逐事件 `validate/apply` 适配为 `PreparedProjectionBatch::prepare/apply_committed`,跟随 astrcode-session-projection 批处理 API 的机械变化。 |
| crates/astrcode-storage/src/session_repo/artifacts.rs | +70/-0 | 架构搬移 | 读写端口基本搬自原文件;新增 canonical 前缀双重校验(`canonical_dir` 缓存 + artifact 路径不得逃逸会话目录),属于顺带的安全加固,有价值但占比小。 |
| crates/astrcode-storage/src/session_repo/dir_scan.rs | +374/-0 | 架构搬移 | 目录扫描/路径布局大部分搬自原文件;新增 `all_session_roots_strict`、`list_root_session_locations`、`list_all_session_locations`(含嵌套 subagent 枚举、事件日志存在性校验),支撑新的会话列表语义,属于搬移中夹带的实质扩展。 |
| crates/astrcode-storage/src/session_repo/durability.rs | +211/-0 | 核心功能 | 全新:fsync 结果不确定时的 sticky durability 状态机(`PendingProjection`/`Published` marker、精确 seq 匹配取出、确认后发布 pending 批次),错误链路和不变式注释完整,是本批最有价值的改动。 |
| crates/astrcode-storage/src/session_repo/journal.rs | +199/-0 | 核心功能 | `create_session` 为搬移;`append_event` 升级为批量 `append_events` + 不确定同步 marker 流程,新增 `retry_uncertain_sync`/`ensure_no_uncertain_durability`,与 durability.rs 配套,值得。 |
| crates/astrcode-storage/src/session_repo/consumer_state.rs | +166/-0 | 核心功能 | 全新:durable event consumer 状态持久化(sha256(consumer_id) 定长有界文件名、version=3 版本校验、commit/consumer 双 lane 下 read-modify-write、durable 写),支撑扩展订阅者的 checkpoint/quarantine,设计清晰。 |
| crates/astrcode-storage/src/session_repo/store.rs | +311/-0 | 核心功能 | recycle/restore/delete/write_compact_snapshot 为搬移;新增 event consumer 的 checkpoint CAS、失败计数与 quarantine、pause、reset(Beginning/StreamHead + skip 审计)五个端口实现,是新 consumer 模型的主体逻辑。 |
| crates/astrcode-storage/src/session_repo/reader.rs | +182/-0 | 核心功能 | replay/cursor/read_model 端口搬移;新增 `replay_from_start_limited`、`list_all_sessions(summaries)` 与带 8 路并发上限的冷会话摘要扫描 `read_summaries_from_logs`(解码失败隔离、IO 错误传播),摘要路径重写有实质价值。 |
| crates/astrcode-storage/src/session_repo/tests.rs | +399/-42 | 测试 | 新增 consumer state 路径有界性、checkpoint 持久化/过期 revision 拒绝、版本不符拒绝、quarantine 一次性触发与审计持久化、uncertain sync sticky/精确重试、嵌套 lineage 摘要、坏日志跳过等测试;删掉了基线里 snapshot/checkpoint 相关断言(该功能随 SnapshotManager 一并移除)。覆盖与新功能对应,值得。 |

## 批次小结

这批改动整体有价值,是 storage 层配合 S5R 3.0 的实质重构,而非单纯搬文件:

- 真正的新增价值集中在三处:durability.rs + journal.rs 的 fsync 不确定 sticky 状态机;consumer_state.rs + store.rs 的 durable event consumer checkpoint/quarantine 模型;reader.rs/dir_scan.rs 中带并发限流与失败隔离的会话枚举/摘要路径。
- 架构搬移占比约一半(mod/owner_lease/projection/artifacts/dir_scan 大部分、store/reader/journal 各有一部分),拆成按端口与关注点分文件的目录结构清晰,模块职责注释到位,拆分本身合理。
- 一处需要知晓的行为变更:基线的 `SessionStore::checkpoint`、`SnapshotManager`、`restore_from_snapshot` 恢复路径在本批被移除(由 consumer checkpoint 模型取代),属于跨 crate 的配套删除,不在这批文件内无法全量核对,建议主审计确认 `snapshot.rs` 的删除与所有调用方清理在其它批次中闭合。
- 没有明显可删/可推迟的部分;若要压缩 PR,artifacts.rs 的 canonical 前缀加固和 dir_scan.rs 的新增枚举函数理论上可拆成独立 commit,但与拆分同批也可接受。
- 存疑项见下表标注:无「存疑」类别文件;唯一口径问题是 PR 描述把 owner_lease 说成新增,实际基线已存在,仅作搬移。
# 批次 37:astrcode-storage 重构配套 + astrcode-tools 整 crate 删除

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| crates/astrcode-storage/src/snapshot.rs | +0/-191 | 存疑 | 整体删除 projection 快照恢复加速器(SnapshotManager、版本校验、保留 4 份滚动快照)。新 session_repo/ 中没有替代实现,dir_scan.rs:32 只是把遗留 `snapshots/` 目录列为跳过项,恢复路径退化为全量事件重放。需确认:长会话冷启动性能回退是否可接受;若为有意简化建议在 PR 描述中说明,否则应补回等效机制。 |
| crates/astrcode-storage/src/testing.rs | +18/-0 | 测试 | 新增 testing feature 下的辅助入口:构造 FileSystemSessionRepository 与 fail_next_durable_sync 故障注入。小而必要,支撑 durability 测试。 |
| crates/astrcode-storage/src/tool_artifacts.rs | +257/-36 | 核心功能 | tool result 工件重做:文件名改为 sha256(tool_name+call_id) 的不透明 artifact_id 并加严格格式校验(防路径注入)、写入改为 NamedTempFile+persist_noclobber+文件/目录 fsync(耐久性)、分页从 char 偏移改为 byte 偏移并做 UTF-8 边界校验。配套测试覆盖注入、幂等、边界错误。价值高。 |
| crates/astrcode-storage/src/traits.rs | +321/-27 | 核心功能 | trait 层扩展:EventConsumerState(消费者 checkpoint/隔离/跳过审计,带容量与字节上限校验)、append_events 批量原子追加 + append_events_and_sync/retry_uncertain_sync/ensure_no_uncertain_durability 耐久屏障、artifact 读取改 artifact_id+byte 分页、list_all_sessions/list_all_session_summaries、read_model recycled 回退。是 S5R 事件消费者与 fsync 语义的契约基础,默认方法保持向后兼容。附带审计边界单测。 |
| crates/astrcode-storage/src/types.rs | +1/-1 | 机械适配 | ToolResultArtifactRef 字段 `path: Option<String>` 改 `artifact_id: String`,跟随 tool_artifacts 的不透明 id 重构;属持久化契约变更,与旧磁盘数据不兼容(可接受,未见迁移逻辑)。 |
| crates/astrcode-tools/Cargo.toml | +0/-30 | 配置 | crate 清单随整体删除移除;依赖(含 portable-pty)一并下线。 |
| crates/astrcode-tools/src/background_shell/mod.rs | +0/-672 | 架构搬移 | 后台 shell 管理删除,能力以简化形式(run_in_background 参数)并入 astrcode-extension-coding/src/process/shell.rs。非纯搬移:独立 registry/输出分页逻辑被合并重写。 |
| crates/astrcode-tools/src/background_shell/tests.rs | +0/-255 | 架构搬移 | 随 background_shell 删除的测试;后台场景由 extension-coding 侧重写覆盖(非逐条搬移,覆盖点需依赖目标 chunk 确认)。 |
| crates/astrcode-tools/src/files/edit.rs | +0/-306 | 架构搬移 | 文件编辑工具迁至 astrcode-extension-coding/src/files/edit.rs(226 行,经重写精简)。 |
| crates/astrcode-tools/src/files/glob.rs | +0/-235 | 架构搬移 | glob 工具与 grep 合并迁至 extension-coding/src/files/search.rs(GlobHandler)。 |
| crates/astrcode-tools/src/files/grep.rs | +0/-569 | 架构搬移 | 同上,grep 迁至 search.rs(GrepHandler,452 行),改为走 host workspace 请求。 |
| crates/astrcode-tools/src/files/mod.rs | +0/-20 | 架构搬移 | 模块声明文件,随 files/ 迁出删除。 |
| crates/astrcode-tools/src/files/patch.rs | +0/-881 | 架构搬移 | patch 工具迁至 extension-coding/src/files/patch.rs(136 行),体量大幅缩减——原 881 行的应用/校验逻辑是否被等价保留需由 extension-coding chunk 确认,有精简过度风险。 |
| crates/astrcode-tools/src/files/read.rs | +0/-344 | 架构搬移 | read 工具迁至 extension-coding/src/files/read.rs(236 行)。 |
| crates/astrcode-tools/src/files/shared.rs | +0/-643 | 架构搬移 | 共享辅助(分页、run_blocking、输出截断等)随 files 工具迁出;功能分散进 extension-coding files/* 与 tool_result.rs。 |
| crates/astrcode-tools/src/files/tests.rs | +0/-851 | 架构搬移 | files 工具的测试随迁移删除,未逐条搬移;等价覆盖需由 extension-coding chunk 核实。 |
| crates/astrcode-tools/src/files/write.rs | +0/-167 | 架构搬移 | write 工具迁至 extension-coding/src/files/write.rs(83 行)。 |
| crates/astrcode-tools/src/lib.rs | +0/-9 | 架构搬移 | crate 根模块声明,随整体删除。 |
| crates/astrcode-tools/src/registry.rs | +0/-208 | 架构搬移 | 内置工具注册表删除,注册职责由 extension-coding lib.rs 的 Extension::register(Registrar) 承接。 |
| crates/astrcode-tools/src/shell_tool/background.rs | +0/-357 | 架构搬移 | shell 后台执行支撑随 shell_tool 删除,并入 extension-coding process/shell.rs。 |
| crates/astrcode-tools/src/shell_tool/definition.rs | +0/-110 | 架构搬移 | shell 工具定义与参数 schema 迁至 extension-coding process/shell.rs(shell::definition())。 |
| crates/astrcode-tools/src/shell_tool/mod.rs | +0/-440 | 架构搬移 | ShellTool 主体迁至 extension-coding process/shell.rs(693 行,经重写)。 |
| crates/astrcode-tools/src/shell_tool/output.rs | +0/-342 | 架构搬移 | shell 输出收集/截断逻辑随迁移删除,能力并入 shell.rs。 |
| crates/astrcode-tools/src/shell_tool/process.rs | +0/-274 | 架构搬移 | 进程拉起/管道读取逻辑随迁移删除,并入 shell.rs。 |
| crates/astrcode-tools/src/shell_tool/tests.rs | +0/-757 | 架构搬移 | shell 工具测试随迁移删除,未逐条搬移;等价覆盖需由 extension-coding chunk 核实。 |
| crates/astrcode-tools/src/terminal_tool.rs | +0/-712 | 存疑 | 持久化 PTY 交互终端工具(REPL/调试器驱动,ring buffer、空闲超时、每会话上限)被整体删除,全仓无替代实现(portable-pty 依赖一并移除,extension-coding 只有一次性/后台 shell)。需确认是有意砍功能;若保留需求,需在扩展体系重建。 |

## 批次小结

这批改动分两块,方向都符合 PR 主线。storage 侧(traits/tool_artifacts/types/testing)是实打实的核心功能:事件消费者 checkpoint/隔离语义、批量追加 + fsync 耐久屏障、不透明 artifact_id + byte 分页,接口设计与测试都比较完整,价值明确。astrcode-tools 侧是整 crate 删除 + 迁至 astrcode-extension-coding,属于本轮重构的既定架构搬移,删除本身没有问题,但两点需要跟踪:一是 files/patch.rs(881→136 行)、shell_tool(约 1900→693 行)这类大幅缩编不是纯搬移,等价行为与测试覆盖要由 extension-coding 侧的 chunk 交叉确认;二是两处真正的功能净删除——terminal_tool.rs(交互式 PTY,无任何替代)和 snapshot.rs(恢复加速器,无替代机制)——建议作者在 PR 中明确说明是有意裁剪,否则应补回。
# 批次 38:docs/(架构文档、扩展文档、review 记录)

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| docs/TODO.md | +38/-2 | 文档 | 勾选已完成项(custom event 统一发射、HostApi transport seam),并新增「扩展能力对齐」缺口清单(P1 纯新增 + P2 大工程)。缺口清单是真实规划内容,但来源指向 `artifacts/tmp/extension-gap-analysis.md` 工作区临时文件,长期 TODO 引用临时产物略存疑;内容本身值得保留。 |
| docs/architecture.md | +35/-17 | 文档 | 总览同步:publisher→SessionEventSink 改名、compact 三入口单 pipeline + 前缀指纹、ToolInputTransform/PreToolUse 新 hook 链、8 内置工具→Coding Extension。逐项对照主线改动,是必要同步。 |
| docs/architecture/s5r-3-implementation.md | +111/-0 | 文档 | 新增 S5R 3.0 Phase 0 实施决策记录:依赖方向、host-first 激活、stable Rust/MSRV 政策、handler 注册(AsyncFn Send 擦除问题)、runtime authority、协议/持久化边界、可观测性、后续项。是本 PR 核心架构的决策存档,价值高。 |
| docs/architecture/session-persistence-and-event-pipeline.md | +43/-47 | 文档 | 同步 publisher→event sink 改名、删除 projection checkpoint(全量 replay 替代,含 1.25× 基准依据)、owner lease、TranscriptRewritten 前缀指纹 prepare。与 storage/session 主线改动逐项对应,必要。 |
| docs/architecture/unified-extension-tool-runtime.md | +1703/-0 | 文档 | 新增「统一 Extension 工具运行时」总设计文档(astrcode-tools 删除、Coding Extension、plan/execute 两阶段、lease、S5R/SDK 合并等 24 节)。是本次重构的纲领性设计文档;体量大但与重构规模相称,含迁移记录与被拒方案,价值明确。 |
| docs/bundled-extension-authoring-design.md | +1067/-0 | 文档 | 新增 bundled 扩展作者接口规范(manifest/Registrar/context 分层、typed host、testing harness)。与 SDK authoring 面重写对应,是给维护者的规范文档,价值明确。 |
| docs/configuration.md | +43/-21 | 文档 | 配置文档同步:删除 JSON 配置迁移残留说明、`shellTimeoutSecs` 从 core 移入 `[extensions.astrcode-coding]`、memory 数据目录 breaking change、热更新 candidate 语义、strict 预算 Bundled→Extension 顺序。跟随代码行为变化,必要且含用户可见 breaking change 提示。 |
| docs/crates.md | +124/-79 | 文档 | crate 地图重写:删 astrcode-tools、新增 astrcode-extension-worker/coding/session-commands、SDK wire 模块、session 模块清单更新(compaction/ 拆分等)。仓库结构事实文档,必须与代码同步,必要。 |
| docs/extension-author-guide.md | +359/-93 | 文档 | 作者指南重写:worker_prelude 移到新 crate、plan/execute 双 handler、planner 资源声明、typed HostClient 域客户端、s5r 3.0 extension.json。面向外部作者的用户文档,随 ABI 变化必须更新,价值高。 |
| docs/extension-hook-matrix.md | +96/-44 | 文档 | hook 语义契约更新:context 构造边界、capability 改名(emit_custom_events 等)、ToolInputTransform/准入聚合语义、S5R fixed-mode hook 表。是 hook 语义的单一事实来源,随实现同步,必要。 |
| docs/extension-system.md | +266/-39 | 文档 | 扩展系统文档重写:代码地图(wire/worker)、bundled manifest/register/start 示例、ToolContext/公共 context 表、typed host capability 表。与 extension-author-guide 有部分重叠但定位不同(系统 vs 指南),可接受。 |
| docs/llm-message-sharing-design.md | +69/-0 | 文档 | 新增 LlmMessage Arc 共享化设计记录(PR-1/PR-2 已实施、不做的项含基准数据)。记录性能优化的决策与测量,价值明确。 |
| docs/provider-context-pipeline-design.md | +170/-0 | 文档 | 新增 provider 上下文管线设计(durable transcript 写入侧合法性、accepted→absorbed 管线、frozen 检测、迁移步骤含「第 3 步不做」的收官决定)。对应 mid-turn 输入吸收主线改动,是事故复盘+设计,价值明确。 |
| docs/reviews/pr-47-final-cleanliness-pass.md | +239/-0 | 文档 | 新增 PR #47 最终清理轮事实记录:架构不变量(generation publication、turn pin、E2E HTTP)、CI 状态账本。review 过程存档,对审阅者有参考价值。 |
| docs/reviews/pr-47-local-candidate-first-principles-review.md | +1682/-0 | 文档 | 新增 PR #47 从零设计审查与收敛记录(20 节:不变式、依赖方向、修复清单、测试矩阵、merge assessment)。体量最大的 review 存档;对后续维护有参考,但是否应随产品仓库长期保留(而非放 PR 描述/wiki)可商榷。 |
| docs/reviews/s5r3-phase-0-cleanliness-audit.md | +137/-0 | 文档 | 新增 Phase-0 整洁度审计记录:过程日志、已应用修复(fingerprint Result、utf8_prefix MSRV)、不改项及理由。过程存档,体量小,合理。 |
| docs/s5r-protocol.md | +125/-47 | 文档 | 线缆协议文档整体重写为 S5R 3.0:host-first initialize/activate、feature negotiation、严格解析、stream/cancel、operation/错误码、conformance 命令。协议契约文档,随协议重写必须更新,价值高。 |

## 批次小结

这一批 17 个文件全部是文档,合计 +6307/-389,没有代码。整体质量高、与主线改动逐项对应:

- **必须与代码同步的事实文档**(architecture.md、crates.md、configuration.md、extension-system.md、extension-author-guide.md、extension-hook-matrix.md、s5r-protocol.md、session-persistence-and-event-pipeline.md)都正确反映了新架构,属于本 PR 的必要组成部分,没有可以删的。
- **新增设计/决策文档**(unified-extension-tool-runtime、bundled-extension-authoring-design、s5r-3-implementation、llm-message-sharing-design、provider-context-pipeline-design)记录了「为什么」,含被拒方案和基准数据,对这种规模的重构是值得的存档。
- **review 过程存档**(docs/reviews/ 三个文件,合计 +2058 行)是唯一可商榷的部分:它们记录的是一次性审查过程而非长期契约,尤其 pr-47-local-candidate-first-principles-review.md(1682 行)体量接近一篇设计文档。保留在仓库里能让后续维护者追溯决策来源,收益真实;但若项目有「过程文档不入库」的惯例,这三个文件可以移到 PR 描述或 wiki。鉴于 docs/reviews/ 目录已存在且作者明确标注「本文件是过程与结论的持久记录,不是架构文档」,倾向保留。
- 小的存疑点:docs/TODO.md 新增的缺口清单引用 `artifacts/tmp/extension-gap-analysis.md`(工作区临时产物),若该文件不入库,引用会悬空,建议把完整分析一并入库或去掉该引用。

**结论:整批都有价值,无需删/拆;唯一建议处理的是 TODO.md 对临时文件的引用,以及(可选)确认 reviews/ 过程文档入库是项目认可的惯例。**
# 批次 39:前端 Chat 组件与协议契约测试适配

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| frontend/package-lock.json | +3/-3 | 生成物 | 仅 nanoid 3.3.17→3.3.18 的 patch 版本 bump,与本 PR 主题无关,疑似 npm install 的顺带产物;无害但可视为噪音。 |
| frontend/package.json | +2/-1 | 测试 | 新增 `test:tool-renderers` script 并接入 `check` 链路,为新 tool-renderers 测试配套,值得。 |
| frontend/scripts/delta-coalesce.test.mjs | +13/-13 | 机械适配 | 跟随协议变化:`phase` 从 state/control 顶层移除、`extensionEvent`→`customEvent`、control 去掉 compactPending/compacting、agentSession 增 kind/agentName/task 必填字段,纯同步测试夹具。 |
| frontend/scripts/protocol-contract.test.mjs | +81/-55 | 测试 | 契约测试随新协议重写:customEvent 重命名、agentSession 改 kind 判别联合、attachments/agentSessions 等改必填;并新增 5 个严格校验负例(未知 status、非对象 arguments、recap 缺 source 等),把「宽松解码」改成「严格拒绝」的行为固化进测试,有独立价值。 |
| frontend/scripts/session-stream-controller.test.mjs | +0/-2 | 机械适配 | 删除 control 里 compactPending/compacting 两个字段,纯跟随协议。 |
| frontend/scripts/tool-renderers.test.mjs | +90/-0 | 测试 | 新测试:覆盖 read_tool_result 渲染器、shell timeoutSecs 优先于 args.timeout、presentation intent 分发与未知 intent 回退、名称匹配优先于 intent,正是本 PR 新渲染路径的核心测试,值得。 |
| frontend/src/components/Chat/ChatView.tsx | +4/-1 | 机械适配 | `s.phase` 改为 `effectiveConversationPhase(control, compactSubmitting)` 派生,跟随 store 去掉顶层 phase 的重构。 |
| frontend/src/components/Chat/CommandSelector.tsx | +14/-15 | 机械适配 | SlashCommandInfo.source 枚举换成 extensionId,分组从「内置/技能/插件」三分收敛为「技能/插件」两分(内置命令已迁为扩展);硬编码 `SKILL_EXTENSION_ID = 'astrcode-skill'` 是新的跨端字符串耦合点,需与后端扩展 id 保持一致。 |
| frontend/src/components/Chat/InputBar.tsx | +10/-4 | 机械适配 | 同上改用 effectiveConversationPhase 派生 phase,isCompacting/isBusy 不再单独叠 compactSubmitting(已被派生函数吸收)。 |
| frontend/src/components/Chat/RecapBlock.tsx | +2/-2 | 机械适配 | recap.source 从可选改必填,删掉 undefined 兜底分支,跟随协议收紧。 |
| frontend/src/components/Chat/ToolCallBlock.tsx | +1/-1 | 机械适配 | toolIconName 去掉 `terminal` 匹配(terminal 工具已删除),只认 shell。 |
| frontend/src/components/Chat/TopBar.tsx | +8/-8 | 机械适配 | AgentSessionStatus/agentName/task/status 改为必填,删除 `?? 'running'`、`|| '子会话'` 等兜底,类型改用 AgentSessionStatus,跟随协议收紧。 |
| frontend/src/components/Chat/UserMessage.tsx | +1/-1 | 机械适配 | attachments 改必填,去掉 `?? []`。 |
| frontend/src/components/Chat/assistantRunModel.ts | +1/-1 | 机械适配 | toolActivityFor 去掉 `terminal` 工具名分支,跟随 terminal 工具删除。 |
| frontend/src/components/Chat/tools/AgentChildSessionPanel.tsx | +3/-5 | 机械适配 | status/agentName/task 必填化,删兜底;task 从条件渲染改恒渲染(必填后必非空)。 |
| frontend/src/components/Chat/tools/builtinRenderers.tsx | +67/-34 | 核心功能 | ToolPresentation 链路的前端落点:新增 `builtin:presentation-intent` 渲染器按 metadata.presentation 分发到既有内置详情组件;新增 read_tool_result 渲染器;删除已废弃的 terminal 工具渲染器。是 PR 主线功能,值得。 |
| frontend/src/components/Chat/tools/details.tsx | +32/-56 | 核心功能 | 新增 ToolResultDetails(read_tool_result 详情);ShellToolDetails 超时优先读 metadata.timeoutSecs(ToolOutcome.metadata 链路);删除 TerminalToolDetails。功能改动,值得。 |
| frontend/src/components/Chat/tools/helpers.ts | +2/-0 | 核心功能 | paginationLabel 支持新的 nextByteOffset 元数据字段,配合 read_tool_result 分页。 |
| frontend/src/components/Sidebar/Sidebar.tsx | +4/-1 | 机械适配 | 同 ChatView,改用 effectiveConversationPhase 派生 phase。 |

## 批次小结

这批前端改动整体都有价值,可分为两类:一类是 PR 主线功能的前端落点(builtinRenderers/details/helpers 的 ToolPresentation、read_tool_result、metadata.timeoutSecs,以及配套的 tool-renderers 新测试与契约测试负例),属于必要且成对的核心改动;另一类是协议收紧(可选字段改必填、phase 派生化、customEvent 重命名、terminal 工具删除)引发的大面积机械适配,改动小、方向一致、由类型系统兜底,没有可推迟的部分。

可挑剔的点仅两个:package-lock.json 的 nanoid patch bump 与主题无关,属可剔除的噪音(不剔除也无害);CommandSelector.tsx 硬编码 `SKILL_EXTENSION_ID = 'astrcode-skill'` 引入了前后端魔法字符串耦合,若后端扩展 id 有常量定义,值得在协议层共享或至少注释同步约束。没有发现需要拆出或推迟的改动。
# 批次 40:前端 generated 目录(ts-rs 生成的前端契约 DTO)

本批 33 个文件全部是 `frontend/src/services/generated/` 下的 ts-rs 自动生成产物(由 `crates/astrcode-protocol/examples/generate-typescript.rs` 从 Rust wire 类型导出,文件头均标注 "Do not edit this file manually")。改动本身无独立价值,价值取决于其镜像的 Rust 侧契约变更;作为生成物必须随源码同步提交,属必要配套。

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| frontend/src/services/generated/AgentSessionLinkDto.ts | +3/-4 | 生成物 | 子 agent 会话链接从「可增量 patch 的可选字段大杂烩」收窄为仅 snapshot 基线用的完整结构(去掉 status/phase/currentTool 可选增量字段),增量改由新 AgentSessionUpdateDto 承担;镜像 Rust 侧协议拆分,合理。 |
| frontend/src/services/generated/AgentSessionUpdateDto.ts | +7/-0 | 生成物 | 新增:子 agent 会话增量改为 kind 判别联合(spawned/completed/failed/progress),每个 variant 只带能变的字段;比旧的裸可选字段 patch 更类型安全,是实质协议改进的镜像。 |
| frontend/src/services/generated/CommandAvailabilityDto.ts | +3/-0 | 生成物 | 新增枚举 all_transports/interactive_only,配合斜杠命令的可用性声明;镜像新增的命令可用性契约。 |
| frontend/src/services/generated/CommandExecutionDto.ts | +4/-0 | 生成物 | 新增:命令执行体分 extension / host(SessionCommandKindDto)两类;镜像 host 内建命令(如 /compact、/model)改走 host 执行的协议变化。 |
| frontend/src/services/generated/CompactSessionResponse.ts | +1/-1 | 生成物 | compact 响应从 {accepted, deferred, sessionId?, message} 简化为 {compacted, message};镜像 compact 流程(defer_context 等)重构后的响应收敛。 |
| frontend/src/services/generated/ConfigViewResponseDto.ts | +1/-1 | 生成物 | activeSmallProfile/activeSmallModel 从 `?: string \| null` 变为 `?: string`,纯 ts-rs 可选字段序列化形式变化,机械跟随。 |
| frontend/src/services/generated/ConversationBlockDto.ts | +1/-1 | 生成物 | user 块 attachments 变必填数组、去掉 source 字段;镜像 mid-turn 输入吸收重构后 user 块契约收紧,前端消费方需同步适配(在其他批次)。 |
| frontend/src/services/generated/ConversationControlStateDto.ts | +1/-1 | 生成物 | 控制状态去掉 compactPending/compacting 两字段;镜像 compact 状态管理简化。 |
| frontend/src/services/generated/ConversationDeltaDto.ts | +2/-2 | 生成物 | agentSessionUpdated 载荷类型换成 AgentSessionUpdateDto;delta kind extensionEvent 改名 customEvent;两处均为协议重命名的机械镜像。 |
| frontend/src/services/generated/ConversationSnapshotResponseDto.ts | +1/-2 | 生成物 | snapshot 去掉顶层 phase 字段(phase 已在 control 内),消除冗余;合理。 |
| frontend/src/services/generated/CustomEventConsumerActionDto.ts | +3/-0 | 生成物 | 新增:自定义事件消费者控制动作枚举(pause/resume/replay/skip);镜像新增的 custom event 消费控制 API。 |
| frontend/src/services/generated/CustomEventConsumerControlRequest.ts | +4/-0 | 生成物 | 新增:消费者控制请求 DTO(extensionId+subscriptionId+action);同上。 |
| frontend/src/services/generated/CustomEventConsumerListResponseDto.ts | +4/-0 | 生成物 | 新增:消费者列表响应 DTO;同上。 |
| frontend/src/services/generated/CustomEventConsumerStatusDto.ts | +4/-0 | 生成物 | 新增:消费者状态 DTO(paused/checkpoint/pendingEvents/quarantined 等运维字段);镜像 custom event 消费管线状态暴露。 |
| frontend/src/services/generated/CustomEventDeclarationDto.ts | +4/-0 | 生成物 | 新增:事件声明(eventType/schemaVersion/delivery/maxPayloadBytes),取代旧 ExtensionEventDeclDto;delivery 枚举化是改进。 |
| frontend/src/services/generated/CustomEventDeliveryDto.ts | +1/-1 | 生成物 | 由 ExtensionEventDeclDto.ts 重命名而来:旧结构体(durable bool)改为三值枚举 session_durable/session_live/global_live,表达力更强;合理重构。 |
| frontend/src/services/generated/CustomEventSourceFilterDto.ts | +3/-0 | 生成物 | 新增:事件订阅来源过滤(any/指定 extension);镜像订阅机制。 |
| frontend/src/services/generated/CustomEventSubscriptionDto.ts | +4/-0 | 生成物 | 新增:事件订阅 DTO(id/eventType/source);镜像订阅机制。 |
| frontend/src/services/generated/ExtensionCapabilityDto.ts | +1/-1 | 生成物 | 能力枚举改名与增删:emit_events→emit_custom_events、consume_events→consume_custom_events,新增 session_command、tool_result_read;镜像 capability 语义细化。 |
| frontend/src/services/generated/ExtensionDeclarationDto.ts | +4/-2 | 生成物 | 扩展声明新增 requiredTransportFeatures、customEvents、customEventSubscriptions,去掉旧 events 字段;镜像声明契约扩展,是 S5R 3.0 声明面变化的核心镜像。 |
| frontend/src/services/generated/ExtensionDiagnosticsDto.ts | +1/-1 | 生成物 | lastHook 等字段 `?: T \| null` → `?: T`,ts-rs 可选形式的机械变化。 |
| frontend/src/services/generated/ExtensionSlashCommandDto.ts | +3/-3 | 生成物 | 斜杠命令字段 snake_case→camelCase(args_schema→argsSchema 等,打破旧「冻结 snake_case」注释),新增 availability/execution;镜像命令契约 camelCase 统一,符合仓库 DTO 规则。 |
| frontend/src/services/generated/ExtensionStageDiagnosticsDto.ts | +1/-1 | 生成物 | durationMs/error 可选形式机械变化,同上。 |
| frontend/src/services/generated/ExtensionStateDto.ts | +1/-1 | 生成物 | declaration/diagnostics 可选形式机械变化,同上。 |
| frontend/src/services/generated/ForkSessionRequest.ts | +3/-3 | 生成物 | fork 请求去掉 turnId、注释改英文,仅保留 storageSeq;镜像 fork 功能从 501 占位变为按 durable seq 实现。 |
| frontend/src/services/generated/SessionCommandKindDto.ts | +1/-1 | 生成物 | 由 CommandSourceDto.ts 重命名而来:旧来源枚举(builtin/extension/skill)改为 host 命令种类枚举(compact_session/select_model);语义重定义,镜像 host 命令模型变化。 |
| frontend/src/services/generated/ShadowedSlashCommandDto.ts | +1/-2 | 生成物 | 遮蔽诊断字段从 activeSource/shadowedSource(CommandSourceDto)改为 activeExtensionId/shadowedExtensionId;跟随 CommandSourceDto 废弃。 |
| frontend/src/services/generated/SlashCommandInfoDto.ts | +1/-2 | 生成物 | 命令信息去掉 source 字段、新增 extensionId;跟随命令来源模型改为统一走扩展。 |
| frontend/src/services/generated/ToolDefinitionDto.ts | +1/-5 | 生成物 | 工具定义去掉 execution_mode(及 ExecutionModeDto import)、snake_case 注释删除;镜像 ToolDefinition 契约精简(execution_mode 不再暴露给前端)。 |
| frontend/src/services/generated/ToolOriginDto.ts | +1/-1 | 生成物 | 工具来源枚举从 builtin/bundled/extension/sdk 收窄为 bundled/extension;镜像 astrcode-tools 删除后 builtin 工具消失的必然结果。 |
| frontend/src/services/generated/TransportFeatureDto.ts | +6/-0 | 生成物 | 新增 transport feature 枚举(当前仅 authenticated_http);镜像 requiredTransportFeatures 契约。 |
| frontend/src/services/generated/index.ts | +13/-2 | 生成物 | barrel 导出同步:新增 12 个新 DTO 导出、删除 CommandSourceDto/ExtensionEventDeclDto;纯机械同步,必要。 |
| frontend/src/services/generated/wire-values.ts | +4/-3 | 生成物 | 运行时枚举常量同步:COMMAND_SOURCES 删除,新增 COMMAND_AVAILABILITIES/SESSION_COMMAND_KINDS,更新 EXTENSION_CAPABILITIES/TOOL_ORIGINS;机械同步,必要。 |

## 批次小结

这批文件全部是 ts-rs 自动生成的契约镜像,作为生成物必须随 Rust 侧 wire 类型同步提交,不存在「可以删/拆/推迟」的部分——删任何一个都会造成 generated 目录与生成器输出不一致。真正的价值判断应落在 Rust 侧的协议改动上,从镜像看这些改动方向是合理的:AgentSessionUpdateDto 判别联合替代可选字段 patch、CustomEventDelivery 枚举化、斜杠命令 snake_case→camelCase 统一、ToolOrigin 随 astrcode-tools 删除而收窄,都属于协议收紧而非膨胀。需要提醒的两点(不在本批文件上扣分,但值得在别处核对):一,`ConversationBlockDto` 的 user 块 attachments 变必填且删 source、`CompactSessionResponse` 等字段删除属于**破坏性线缆变更**,需确认前端消费代码(本批之外)已全部适配、且无旧客户端兼容诉求;二,`ExtensionSlashCommandDto`/`ToolDefinitionDto` 原有注释明确称 snake_case 是「有意的冻结形状」,本次直接改为 camelCase,需确认这是有意的契约迁移而非顺手为之。另注意 `ExtensionStageDiagnosticsDto` 等可选字段从 `?: T | null` 变 `?: T`,如果后端仍可能序列化出显式 `null`,前端反序列化会有类型偏差,建议在生成器层确认 serde 的 skip_serializing_if 行为一致。
# 批次 41:前端会话协议解码与 store 状态适配

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| frontend/src/services/protocol.ts | +59/-44 | 机械适配 | 解码层对齐新线缆契约:删除 `optionalArrayField`、解码器收紧(可选字段类型错即抛错、去掉 status 的 'running' fallback)、`extensionEvent`→`customEvent` 改名、agentSessionUpdated 载荷从 AgentSessionLink 改为判别联合 AgentSessionUpdate(spawned/completed/failed/progress)。契约收紧是后端协议变化的必要跟随,方向正确(减少静默容错)。 |
| frontend/src/services/types.ts | +9/-18 | 机械适配 | 类型同步:AgentSessionLink 改为 `AgentSessionLinkDto & {phase, currentTool}`,新增 AgentSessionUpdate(复用生成的 AgentSessionUpdateDto);删除 control.compactPending/compacting、snapshot.phase、user block 的 source 字段;attachments 从可选变必填数组、recap.source 变必填。纯契约跟随,复用生成 DTO 符合 DTO 规则。 |
| frontend/src/store/delta/applyDelta.ts | +12/-14 | 机械适配 | reducer 跟随契约:删除顶层 phase patch 与 resolvePhase 调用、sameControlState 去掉 compact 两字段、agentSessionUpdated 分支从 mergeAgentSession 换成 applyAgentSessionUpdate、customEvent 改名。无独立新逻辑。 |
| frontend/src/store/delta/blockHelpers.ts | +47/-55 | 机械适配 | 核心改动:mergeAgentSession(宽松字段合并+终态保护)重写为 applyAgentSessionUpdate(按 update.kind 判别分支构造/更新 AgentSessionLink),删除 phaseFromControl/resolvePhase。逻辑由新协议语义直接决定,写法比原来更清晰;属于协议驱动的重写而非搬移。 |
| frontend/src/store/index.ts | +21/-22 | 机械适配 | store 跟随:删除 AppState.phase 的维护与写入,改用 effectiveConversationPhase(control, compactSubmitting) 即时推导;compact 成功判断从 `response.message === 'compact accepted'` 改为本地 `compactCommand` 标志(不再依赖后端文案,更稳但语义略宽——任何 compact 命令 accepted 都触发 refresh+switch);删去一条解释 HTTP/SSE 命令分流的注释(该信息随改动失效可接受,但分流逻辑本身仍在,注释丢失略有信息损失)。 |
| frontend/src/store/phaseHelpers.ts | +9/-5 | 机械适配 | 新增 effectiveConversationPhase 作为唯一 phase 推导入口(替代 blockHelpers 里的 resolvePhase),isExecutionPhase 签名收窄为只收 Phase。职责归位合理,与 index.ts/applyDelta 的改动配套。 |
| frontend/src/store/types.ts | +0/-2 | 机械适配 | AppState 删除 phase 字段及 Phase import,两行,与上述改动配套。 |

## 批次小结

这批改动是一个连贯的整体:后端把会话快照/控制态契约收紧(phase 单源化到 control、去掉 compactPending/compacting、agent session 更新从宽松合并载荷改为 spawned/completed/failed/progress 判别联合、extensionEvent 改名 customEvent),前端 7 个文件全部是对该契约的跟随适配,没有夹带无关重构,价值成立。两类实质收益值得肯定:解码层从「静默容错」转向「类型错即抛 ProtocolDecodeError」,前端状态从「冗余 phase 字段+resolvePhase 合并」转向「control 单源+effectiveConversationPhase 推导」,都消除了双源不一致的风险。

可注意的小点(不构成删除/推迟理由):

- `store/index.ts` 中 compact 成功判断从匹配后端文案 `'compact accepted'` 改为本地 `compactCommand` 标志,语义略变宽(理论上后端对 compact 命令返回其他 accepted 文案也会触发 refresh+switch),但摆脱文案耦合总体是改进。
- `store/index.ts` 删掉了「内置命令走 HTTP、扩展斜杠命令走 SSE」的说明性注释,该分流逻辑仍然存在,注释丢失属轻微信息损失,可考虑补回一行。
- 建议确认所有 UI 组件中对 `state.phase` / `compactPending` / `compacting` 的引用已在其他批次同步清理(本批次内未见遗留),否则 TS 编译会直接暴露。

无需要拆分或推迟的部分;无存疑项。
# 批次 42:Python SDK(sdks/python 全新 S5R 3.0 薄客户端)

整批 17 个文件全是新增(+3960/-0),为零依赖、仅标准库的 Python SDK,镜像 `astrcode-extension-worker` / `astrcode-extension-sdk::wire` 的语义。Rust 参考实现仍保留在 crates 下,因此本批不是代码搬移,而是第二语言的平行实现。已抽查与 Rust 侧的一致性(错误码目录、`queue_or_start`/`defer_context` 操作名、`HostProcessInputRequest` 的 `{"id", "action": {"kind": ...}}` 形状均吻合),并实际运行测试套件:31 个测试全部通过(0.156s)。

| 文件 | +/- | 类别 | 说明 |
|---|---|---|---|
| sdks/python/README.md | +142/-0 | 文档 | 高质量 SDK 文档:快速上手、API 对标表、协议覆盖/未覆盖(HTTP route)说明、conformance 验收命令;明确声明语义以 Rust 实现为准,边界交代清楚。 |
| sdks/python/examples/echo_extension.py | +37/-0 | 文档 | 最小可运行 echo 示例,同时是 README 中 conformance 二进制的被测 target,一物两用,值得保留。 |
| sdks/python/pyproject.toml | +18/-0 | 配置 | setuptools 打包元数据,零依赖声明属实;version "0.1.0" 与 `__init__.py` 的 `__version__` 双处硬编码,后续发版需同步(见存疑)。 |
| sdks/python/src/s5r/__init__.py | +128/-0 | 核心功能 | 公共导出面,与各子模块实际定义一致,无多余导出。 |
| sdks/python/src/s5r/context.py | +172/-0 | 核心功能 | invocation context 类型 + `CancelToken` + `_CallFacts` 提取,镜像 Rust `worker::registry`;`defer_context` 快捷方式与 Rust `WorkerInvocationContext::defer_context` 对齐。 |
| sdks/python/src/s5r/errors.py | +167/-0 | 核心功能 | `WireErrorCode` 全量目录(含新增 `no_active_turn`)、`ErrorPayload` 严格解码(拒未知字段)、未知 code 无损透传;已抽查与 `wire/error.rs` 一致。 |
| sdks/python/src/s5r/frames.py | +110/-0 | 核心功能 | 长度前缀帧编解码:16 MiB 上限、32 字节 header、拒绝空/带符号/带空格 header,帧中 EOF 视为干净关闭——与 Rust `wire::frame` 不变式逐条对应。 |
| sdks/python/src/s5r/host.py | +396/-0 | 核心功能 | `HostClient` 域客户端(events/models/session_control 等)与 `HostOperation` 操作名目录(含 `queue_or_start`/`defer_context`);用 `contextvars` 复刻 Rust task-local `with_host_api`,handler 外调用抛 `context_unavailable`;`ProcessClient.write/close_stdin` 硬编码的请求形状已核实与 Rust wire DTO 一致。 |
| sdks/python/src/s5r/manifest.py | +242/-0 | 核心功能 | manifest 声明类型(ToolDefinition/SlashCommand/CustomEvent…)+ 固定 hook mode 表与 `hook_mode_is_supported` 校验,镜像 `wire::manifest` 与 `registration_validation`;`ToolDefinition.timeout_ms` 已带本 PR 新字段。 |
| sdks/python/src/s5r/parsing.py | +70/-0 | 核心功能 | `parse_tool_arguments`/`parse_hook_input` dataclass 解析,拒未知/缺失字段,对标 Rust 同名 helper,小而必要。 |
| sdks/python/src/s5r/protocol.py | +352/-0 | 核心功能 | 线缆消息严格解码(envelope 与 stream event 均拒未知字段/未知 type)、feature negotiation 交集语义、result success/failure 互斥校验,与 `wire::protocol` 对应,是协议正确性的核心。 |
| sdks/python/src/s5r/results.py | +135/-0 | 核心功能 | `HandlerResult`/`HandlerEffect`/`ToolPlan`/`ResourceAccess`,镜像 `wire::effects` 与 `tool_plan`,序列化形状经测试验证。 |
| sdks/python/src/s5r/worker.py | +1192/-0 | 核心功能 | 本批核心:注册 API(tool/hook/command/custom event,含固定 mode 校验与重复注册拒绝)、initialize/activate 握手、`handler.invoke` 四类分发、内建 `s5r.conformance.*` 与 ping、`_Driver` 读写泵/墓碑吸收迟到终态/stream 顺序校验/双向 cancel/干净关停。设计决策均有注释对标 Rust 对应物。 |
| sdks/python/tests/memory.py | +37/-0 | 测试 | in-memory 全双工 `FrameTransport` fixture,支撑全部 handshake/worker 测试,无重复造轮子。 |
| sdks/python/tests/test_frames.py | +77/-0 | 测试 | 帧编解码测试:畸形/超长/空 header、超限 payload、帧中 EOF,覆盖 frames.py 全部拒绝分支。 |
| sdks/python/tests/test_handshake.py | +147/-0 | 测试 | 握手测试:正常 initialize/activate/EOF、版本不符、extension_id 不匹配、required feature 不支持、activate 前业务消息拒绝。 |
| sdks/python/tests/test_worker.py | +538/-0 | 测试 | tool plan/execute 双阶段、hook/command 回环、HostClient nested invoke(parent_invoke_id 校验)、`queue_or_start`/`defer_context` 回环、`no_active_turn` 透传、model stream 收集、cancel 后 worker 仍响应、全部内建 conformance 操作——覆盖面与 Rust worker 测试对等;仅两处 `asyncio.sleep(0.05)` 有轻微时序依赖,可接受。 |

## 存疑项

- **sdks/python/pyproject.toml**:`version = "0.1.0"` 与 `src/s5r/__init__.py` 的 `__version__ = "0.1.0"` 双处硬编码,无单一事实来源;建议改用 `importlib.metadata` 或 setuptools dynamic version,否则发版时容易漂移。属轻微维护隐患,不阻塞合入。

## 批次小结

这批改动整体价值高且内聚:它是 S5R 3.0 线缆协议的第二语言薄实现,协议面(帧、握手、invoke、stream、cancel、conformance)与 Rust 参考实现逐点对应,并自带与 Rust worker 对等强度的 31 个测试(已实际跑通)。没有可删除的部分;HTTP route 不支持是显式声明的范围裁剪(README 有交代),不是半成品。唯一可推迟/改进的是版本号双写的工程化小问题(见存疑项)。若仓库接受多语言 SDK 的维护成本,本批应整体保留;唯一值得讨论的前置问题是:Python SDK 是否与 Rust 参考实现共用一份契约测试以防未来漂移——目前靠 README 声明 + conformance 二进制人工验收,CI 中是否挂了该 conformance 命令未在本批文件中体现,建议在合入前确认。

---

## 存疑项处置(2026-08-16)

逐项核实后的结论与动作:

| 文件 | 核实结论 | 处置 |
|---|---|---|
| artifacts/tmp/perf-baseline.md | 内容有价值(它是删除 snapshot 的测量依据),仅路径不当、结尾「未执行」已过时 | 移至 docs/reviews/storage-perf-baseline.md,结尾更新为「已在 s5r3-phase-0 执行」 |
| crates/astrcode-core/src/llm/thinking.rs | 确认 100% 为 let-chains 风格重写,零行为变化 | 已从 PR 剔除(恢复至基线) |
| crates/astrcode-session/src/tool_deduplicator.rs | 同上,纯 let-chain 重写 | 已从 PR 剔除(恢复至基线) |
| crates/astrcode-extension-sdk/src/wire/host/session.rs | 确认 61–63 行是从 process.rs 复制的错误文档注释 | 已删除该注释 |
| crates/astrcode-server/src/http/auth.rs | 有意决策:文件头注释明确「鉴权已停用」,token 仅为兼容客户端保留;无动作 | 保留,建议 PR 描述中明示安全模型变更 |
| crates/astrcode-tools/src/terminal_tool.rs | 有意删除:全仓 `"terminal"` 仅剩 ToolPresentation 枚举值,无任何名为 terminal 的工具注册 | 无需动作 |
| crates/astrcode-extension-mode/src/catalog.rs | 与上一项一致:terminal 工具已不存在,PLAN_RESTRICTED_TOOLS 删除 "terminal" 无拦截缺口 | 无需动作 |
| crates/astrcode-extension-channels/src/lib.rs | `on_config_changed`/`notify_config_changed`/`update_extension_configs` 全仓零引用——配置热更新是 S5R3 整体移除(改配置=整代际重启),非 channels 单独回退 | 无需动作,建议 PR 描述明示该语义变更 |
| crates/astrcode-server/src/http/routes/extensions.rs | 已确认 `reload_extension_registry`(routes/mod.rs)在 config transaction 后真实调用 `reload_extensions()` 并发通知;set_enabled 链路有效 | 无需动作;reload_errors 恒空字段留待后续清理 |
| crates/astrcode-storage/src/snapshot.rs | 有意删除,依据即 storage-perf-baseline.md 的测量(各规模 snapshot 恢复均为净亏) | 无需动作 |
