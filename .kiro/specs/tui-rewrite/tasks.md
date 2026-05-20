# TUI 重写 — Tasks

> 关联：[requirements.md](./requirements.md) · [design.md](./design.md)
>
> **执行规则**：每个任务结束前 `cargo check -p astrcode-cli`；阶段 ✅Verify 跑三件套。
> **分支**：`refactor/tui-rewrite-v2`。
> 生产代码禁止 `unwrap`/`expect`/`panic!`（仅 `#[cfg(test)]` 允许）。

---

## Phase 0 — 准备

- [ ] **0.1** 拉新分支 `refactor/tui-rewrite-v2`
- [ ] **0.2** `cargo test -p astrcode-cli --all-features` 记 baseline 测试名单
- [ ] **0.3** 创建 `crates/astrcode-cli/src/tui_v2/`（旧 `tui/` 暂留）

## Phase 1 — Component trait + Container + Terminal

- [ ] **1.1** `component/mod.rs`：`Component` trait + `KeyOutcome` + `Container` + `OverlayStack` + `OverlayEntry` + `OverlayAnchor`
- [ ] **1.2** `theme.rs`：搬运 Theme + detect()
- [ ] **1.3** `terminal.rs`：搬运 TerminalSession（enter/exit raw mode、CSI 2026 同步对、inline viewport、DECSTBM insert_history_lines、resize heuristic）。从旧 `tui.rs` + `custom_terminal.rs` + `insert_history.rs` 提取
- [ ] **1.4** `frame/mod.rs` + `frame/rate_limiter.rs` + `frame/event_stream.rs`：
  - FrameRequester actor (mpsc → broadcast coalescing)
  - FrameRateLimiter (120 FPS cap, 搬运 codex 设计)
  - EventBroker / TuiEventStream / TuiEvent (搬运现 tui_event/)
- ✅Verify：`cargo build -p astrcode-cli` + `cargo clippy -p astrcode-cli --all-targets -- -D warnings`

## Phase 2 — Streaming (AdaptiveChunking)

- [ ] **2.1** `streaming/mod.rs`：StreamState { collector(MarkdownStreamCollector), queued_lines: VecDeque<QueuedLine>, has_seen_delta }
- [ ] **2.2** `streaming/chunking.rs`：AdaptiveChunkingPolicy (Smooth/CatchUp + hysteresis，移植 codex 设计)
- [ ] **2.3** `streaming/commit_tick.rs`：run_commit_tick(policy, controller, scope, now) → CommitTickOutput
- [ ] **2.4** `streaming/controller.rs`：StreamController (push_delta → collector → newline-complete → render → enqueue; finalize → drain remainder)
- [ ] **2.5** 单测：chunking mode transitions / stream finalize / queue drain
- ✅Verify：`cargo test -p astrcode-cli streaming::`

## Phase 3 — Render (RenderSpec → Lines)

- [ ] **3.1** `render/markdown.rs`：搬运 block-level markdown 渲染（ATX heading / list / quote / code fence / hr / wrapping）
- [ ] **3.2** `render/mod.rs`：`render_spec_to_lines(spec, prefix, width, theme)` 全 RenderSpec 变体
- [ ] **3.3** 迁移现有 5 个 render 测试
- ✅Verify：`cargo test -p astrcode-cli render::`

## Phase 4 — Store 数据层

- [ ] **4.1** `store/transcript.rs`：MessageRole / Message / MessageBody / ScrollbackEntry
- [ ] **4.2** `store/child_agent.rs`：ChildAgentTracker + 2 个测试迁移
- [ ] **4.3** `store/mod.rs`：TranscriptStore 组合
- ✅Verify：`cargo test -p astrcode-cli store::`

## Phase 5 — Extension traits + registries

- [ ] **5.1** `ext/tool.rs`：ToolRenderer trait + ToolRendererRegistry + ToolRenderCtx + RenderShell
- [ ] **5.2** `ext/message.rs`：MessageRenderer trait + MessageRendererRegistry
- [ ] **5.3** `ext/fallback.rs`：DefaultToolRenderer（通用 tool 摘要：read→⎿ read N lines，shell→⎿ output: N，etc）
- [ ] **5.4** `ext/builtin.rs`：8 个具名 ToolRenderer impl（Read/Write/Edit/Find/Grep/Shell/Patch/Agent）
- [ ] **5.5** `ext/mod.rs`：`register_builtin()` 入口
- [ ] **5.6** 新增 3 个测试：registry override / fallback / custom_type dispatch
- ✅Verify：`cargo test -p astrcode-cli ext::`

## Phase 6 — Components

- [ ] **6.1** `component/composer.rs`：搬运 ComposerState/Action/VisualLayout + 4 个测试 → impl Component
- [ ] **6.2** `component/slash_palette.rs`：搬运 slash 过滤/解析 + render → impl Component (Overlay)
- [ ] **6.3** `component/footer.rs`：footer render + compact_path + fit_line
- [ ] **6.4** `component/tool_row.rs`：ToolRow 状态机 (Running→Args→Complete/Error)，持有 ToolRenderCtx.state
- [ ] **6.5** `component/transcript.rs`：active_tool_rows + scrollback_queue + flush_into_terminal
- [ ] **6.6** `command/mod.rs` + `command/slash.rs`：SlashCommand enum + parse + builtin_commands + 2 个 slash 测试
- ✅Verify：`cargo test -p astrcode-cli component:: command::`

## Phase 7 — App + Main Loop

- [ ] **7.1** `app.rs`：App struct (store + component + ext regs + session state + flags)
- [ ] **7.2** `app/handle_event.rs`：apply(ClientNotification) 分发
- [ ] **7.3** `app/handle_session.rs`：session started / deleted / resumed / list
- [ ] **7.4** `app/handle_assistant.rs`：stream start / delta → StreamController → commit_tick
- [ ] **7.5** `app/handle_tool.rs`：tool started/requested/output/completed → ToolRow + registry
- [ ] **7.6** `app/handle_extension.rs`：extension command list / result
- [ ] **7.7** `app/input.rs`：dispatch_key + dispatch_paste (global shortcuts + container delegation)
- [ ] **7.8** `mod.rs`：`pub async fn run()` 主循环：
  - client.subscribe_events()
  - TerminalSession::enter()
  - FrameRequester::new()
  - App::new()
  - tokio::select! { crossterm event, client notification, frame broadcast }
  - frame draw: commit_tick → flush scrollback → draw viewport
- [ ] **7.9** main.rs 切换：`mod tui_v2 as tui;`（旧 tui 暂留）
- ✅Verify：手动联调 tui ↔ server 确认基本流程可用

## Phase 8 — 测试回填 + 旧目录删除

- [ ] **8.1** 迁移 baseline 中 11 个 state 测试到 `app/handle_*.rs`
- [ ] **8.2** `cargo test -p astrcode-cli --all-features` 全绿，对比 baseline 名单
- [ ] **8.3** 删除旧 `tui/`，重命名 `tui_v2` → `tui`，main.rs 改回 `mod tui;`
- [ ] **8.4** 全工作区验证：
  - `cargo fmt --check`
  - `cargo clippy --workspace --all-targets --all-features --exclude astrcode-desktop -- -D warnings`
  - `cargo test --workspace --all-features --exclude astrcode-desktop`
  - `python3 scripts/check-deps.py`
- ✅Verify：全绿

## Phase 9 — 文档 + 提交

- [ ] **9.1** 更新 `tui/mod.rs` 顶部 `//!` doc
- [ ] **9.2** 更新 README TUI 章节（如有）
- [ ] **9.3** Commit + push `refactor/tui-rewrite-v2`，开 PR

---

## 风险

| 风险 | 缓解 |
|---|---|
| AdaptiveChunking 参数需要调优 | 先取 codex 默认值，Phase 7.9 联调时人工观察 |
| ToolRenderer 覆盖现有 tool_display 后摘要格式可能微变 | Phase 8.1 测试 exact match |
| FrameRequester actor 的生命周期管理 | drop tx → scheduler task 自动终止 |
| 旧 custom_terminal.rs 搬运后需要适配新 Component trait | Phase 1.3 保留原 API，不大改 |

## 执行顺序

Phase 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9

不跳序。每个 phase 的 ✅Verify 是 gate。
