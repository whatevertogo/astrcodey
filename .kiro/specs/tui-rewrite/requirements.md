# TUI 重写 — Requirements

> **范围**：完全**重写** `crates/astrcode-cli/src/tui/`，不保留现有结构。仅保留对外 entry-point 签名 `tui::run() -> io::Result<()>`、与 `astrcode-client::AstrcodeClient<InProcessTransport>` 的契约、以及订阅 `astrcode-protocol::events::ClientNotification` 的事件流。
>
> **设计参考**（实际看过代码后的结论）：
> - **pi-mono `@earendil-works/pi-tui`**：`Component { render(width) -> string[]; handleInput?; invalidate() }` + `Container { children[] }` + TUI 主类持 overlay 栈 + 16ms 节流 `requestRender` + line-diff + CSI 2026。
> - **pi-mono `@earendil-works/pi-coding-agent/extensions`**：`ToolRenderContext<TState, TArgs> { args, toolCallId, invalidate(), lastComponent, state, executionStarted, argsComplete, isPartial, expanded, showImages, isError }` + `ToolDefinition.renderCall / renderResult / renderShell:"default"|"self"` + `MessageRenderer<T>(message, {expanded}, theme) -> Component | undefined` + `registerMessageRenderer(customType, renderer)` 注册表。
> - **codex-rs `codex-rs/tui`**：`Renderable { render(area, buf); desired_height(width); cursor_pos(area); cursor_style(area) }` + `FrameRequester` actor 式 coalescing scheduler (120 FPS cap) + `AdaptiveChunkingPolicy { Smooth(1-line/tick) ↔ CatchUp(drain-all) + hysteresis }` + `StreamState { collector, queued_lines, has_seen_delta }` + `insert_history_lines` 用 DECSTBM scroll region + Zellij fallback + `custom_terminal` inline viewport。
>
> **合成设计决策**：
> - 组件 trait 用 codex 的 `Renderable` 签名（`render(Rect, &mut Buffer)` + `desired_height(u16) -> u16`）而不是 pi 的 `render(width) -> string[]`——因为我们已经用 ratatui Buffer，直接画到 Buffer 享受 cell-diff + Style/Span；不必多一层字符串拼接。
> - 流式输出用 codex 的 `AdaptiveChunkingPolicy`（Smooth/CatchUp + hysteresis）替代现有固定 160-char 切分。
> - 插件渲染用 pi-mono 的 `ToolRenderContext{state, argsComplete, isPartial, isError, expanded}` + `renderCall/renderResult` 分离 + `renderShell` 概念。
> - 帧调度用 codex 的 `FrameRequester` actor 模式（mpsc → broadcast，120 FPS cap）。
> - scrollback 写入保留现有 codex 风格（DECSTBM + Zellij fallback，我们的 `custom_terminal.rs` 已有）。
> - 不抄 pi 的 `CURSOR_MARKER` APC hack（ratatui `frame.set_cursor` 即可）。
> - 不引入 VDOM / Elm / reducer。

## 1. 业务目标

| ID | 目标 | 说明 |
|---|---|---|
| G1 | 重写 TUI 使行为可预期、状态机清晰 | 当前 1240+ 行的 `state.rs` 把会话/消息/composer/slash/scrollback/child-agent 全堆在一起，难以定位 bug |
| G2 | 让插件可注册自定义渲染 | pi-mono 模式：`MessageRenderer<T>` + `ToolRenderer`（流式 partial→complete + state slot） |
| G3 | 不改任何对外接口 | 保留 `tui::run`、CLI subcommand、protocol DTO、client API、命令行参数、环境变量 |
| G4 | 已知体验：inline viewport + 写 scrollback | 保留 codex 风格：底部固定 4 行 composer/footer，消息历史走原生终端 scrollback；CSI 2026 同步输出 |
| G5 | 删除现有所有 tui 文件，从零搭建 | 重写不是重构 |
| G6 | 流式输出 adaptive chunking | 用 codex 的 Smooth/CatchUp 两档 + hysteresis 替代现有固定阈值 |

## 2. 用户故事

### Story 1 — 用户提交 prompt 看到流式输出

**As a** astrcode CLI 用户
**I want** 在 composer 输入 prompt，看到 assistant 文本以自适应速率写入终端 scrollback
**So that** 我能用终端原生的 PageUp/滚轮 翻看历史

**Acceptance Criteria（EARS）**：
1. WHEN 用户按 Enter 提交 prompt THEN 系统 SHALL 立即在 scrollback 中写入一条 user message header + body，并把 `ClientCommand::SubmitPrompt` 发给 client。
2. WHILE turn 处于 streaming THE 系统 SHALL 把 `AssistantTextDelta` 累积到 markdown stream collector，按 newline-complete-source 切分并渲染为 `Vec<Line>`，enqueue 到 `StreamState.queued_lines`。
3. WHEN streaming 进入 Smooth 模式 THEN 系统 SHALL 每 commit-tick 从 queue 前端 drain 1 行写入 scrollback。WHEN queue pressure 触发 CatchUp THEN 系统 SHALL 一次性 drain 全部排队行。
4. WHEN 收到 `AssistantMessageCompleted` THEN 系统 SHALL finalize collector → drain remainder → 附加空行 → 清除该 message_id 的 stream state。
5. IF turn 在 streaming 中且用户按 Esc THEN 系统 SHALL 发 `ClientCommand::Abort`，footer status 改为 `Stopping turn`。
6. WHEN 用户按 Shift+Enter 或 Alt+Enter THEN 系统 SHALL 在 composer 插入换行而不是提交。

### Story 2 — 工具调用展示

**As a** 用户
**I want** 看到 tool 调用从"启动 → 参数到达 → 执行中 → 完成"的稳定展示
**So that** 我能跟上 agent 在做什么，不被中间态闪烁打扰

**Acceptance Criteria**：
1. WHEN 收到 `ToolCallStarted{ call_id, tool_name }` THEN 系统 SHALL 在 active 区（非 scrollback）创建一个 **ToolRow** 组件，调用注册的 `ToolRenderer::render_call(ctx{is_partial:true, args_complete:false, …})` 产出 `RenderSpec`。
2. WHEN 收到 `ToolCallRequested{ call_id, tool_name, arguments }` THEN 系统 SHALL 把 `ctx.args` 设为 arguments、`ctx.args_complete = true`，重新调 `render_call`。
3. WHEN 收到 `ToolCallCompleted{ call_id, result }` THEN 系统 SHALL：
   - 优先看 `result.metadata[UI_RENDER_METADATA_KEY]` → 直接用此 RenderSpec
   - 否则调 `ToolRenderer::render_result(result, ctx{is_partial:false, is_error: result.is_error, …})`
   - renderer 返回 None → 走默认摘要
4. WHEN ToolRow 完成 THEN 系统 SHALL flush 到 scrollback 后移除该 ToolRow 组件。
5. IF tool_name 在 hidden 列表且无 error 且无 RenderSpec THEN 仅 footer status 更新。
6. WHILE tool Running THEN footer 显示 `Running <Label>`。

### Story 3 — 子 agent 委派的可读输出

（保留不变，同前版 requirements.md Story 3）

### Story 4 — Composer：输入历史、bracketed paste、宽字符

（保留不变，同前版 requirements.md Story 4）

### Story 5 — Slash command palette

（保留不变，同前版 requirements.md Story 5）

### Story 6 — 会话生命周期

（保留不变，同前版 requirements.md Story 6）

### Story 7 — Resize / inline viewport

（保留不变，同前版 requirements.md Story 7）

### Story 8 — 插件渲染（重写后立刻可用）

**As a** 扩展作者
**I want** 我的扩展能在不修改 CLI 代码的前提下，让某种 customType 消息或某个 tool 结果走我自己的渲染逻辑
**So that** 我能贴上自定义 UI

**Acceptance Criteria**：
1. SHALL 提供两个 trait：
   - `MessageRenderer`：`fn render(&self, custom_type: &str, payload: &Value, opts: &MessageRenderOpts) -> Option<RenderSpec>`
   - `ToolRenderer`：
     - `fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec`
     - `fn render_result(&self, result: &ToolResult, ctx: &mut ToolRenderCtx) -> Option<RenderSpec>`
2. `ToolRenderCtx` SHALL 至少含：`call_id: &str, args: Option<&Value>, args_complete: bool, is_partial: bool, is_error: bool, expanded: bool, state: &mut Box<dyn Any + Send>`（映射自 pi-mono 的 `ToolRenderContext`）。
3. SHALL 提供两个注册表，按字符串 key 分发（同名后注册覆盖先注册，映射 pi-mono `registerTool` 覆盖语义）。
4. SHALL 在 TUI 启动时注册 8 个内建 ToolRenderer（read/write/edit/find/grep/shell/patch/agent）。
5. SHALL 跨进程扩展（远期）只能提交 `RenderSpec` IR 到 `result.metadata[UI_RENDER_METADATA_KEY]`。
6. SHALL 找不到 renderer 时 fallback：tool → 默认摘要；message → markdown fallback。
7. SHALL 支持 `render_shell` 概念："default"（统一 colored header 框）vs "self"（tool 自己输出完整行），映射 pi-mono `renderShell`。

### Story 9 — 不丢失现有测试覆盖

（保留不变，同前版 requirements.md Story 9）

### Story 10 — 已知 bug 候选区可观测

（保留不变，同前版 requirements.md Story 10）

## 3. 非功能需求

| 类别 | 要求 |
|---|---|
| 性能 | 120 FPS cap (codex FrameRateLimiter)；Smooth 模式 commit-tick ≈ 8ms；CatchUp 一次 drain 全队列；keystroke→屏幕 < 8ms |
| 内存 | StreamState queue drain 每 commit-tick；child-agent tracker 在 tool 完成时移除 |
| 可见性 | tracing::error! 不静默 |
| 安全 | `unwrap`/`expect` 仅 #[cfg(test)]；`_ => Phase::Idle` 兜底 |
| 兼容 | nightly Rust（沿用 CI）；Linux + macOS + Windows；Zellij fallback |
| 依赖 | ratatui 0.30, ratatui-crossterm 0.1, crossterm 0.29, unicode-width 0.2, tokio, futures, tracing, parking_lot, astrcode-{client,core,protocol,support,log}；不引新 crate |

## 4. 范围外

- 不实现 wasm 动态加载。
- 不做内联图片渲染。
- 不做 Markdown inline emphasis 解析。
- 不重构 `astrcode-client/server/core`。
- 不引入 VDOM / Elm reducer。
- `acp`/`server`/`exec` 子命令不动。
- Kitty keyboard protocol 增强（远期 v2）。

## 5. 不可丢失的对外契约

- `pub async fn tui::run() -> io::Result<()>`
- `mod tui` 在 `astrcode-cli/src/main.rs` 顶部
- 外部没有 import 任何 `tui::*` 内部 item

## 6. 验证

`cargo fmt --check`、`cargo clippy -p astrcode-cli --all-targets -- -D warnings`、`cargo test -p astrcode-cli --all-features` 全绿；CI 工作流全通过。
