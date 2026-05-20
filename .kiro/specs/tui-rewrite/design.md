# TUI 重写 — Design

> 关联：[requirements.md](./requirements.md) · [tasks.md](./tasks.md)
>
> **实际看过代码后的设计决策**

## 0. 设计来源（看过 pi-mono + codex-rs 源码后的提炼）

| 借鉴自 | 抄什么 | 为什么 |
|---|---|---|
| **codex-rs `Renderable` trait** | `render(area, buf)` + `desired_height(width)` + `cursor_pos` + `cursor_style` | 直接画到 ratatui Buffer，享受 cell-diff + Style |
| **codex-rs `FrameRequester`** | actor 式 mpsc→broadcast coalescing scheduler + `FrameRateLimiter` (120 FPS) | 比 pi 的 nextTick+16ms setTimeout 更 Rust-native |
| **codex-rs `AdaptiveChunkingPolicy`** | Smooth (1 line/tick) ↔ CatchUp (drain all) + 滞回 | 替代现有 160-char 固定切分 |
| **codex-rs `StreamState`** | `collector + queued_lines + has_seen_delta` + `drain_n` | 成熟的 queue 管理 |
| **codex-rs `insert_history_lines`** | DECSTBM scroll region + Zellij fallback | 已有搬运（`custom_terminal.rs`） |
| **pi-mono `Component`** | `render(width)->string[]` + `handleInput` + `invalidate()` 概念 | 映射到 Renderable + Component 合体 trait |
| **pi-mono `Container`** | `children: Component[]`, render=concatenate | 映射到 `Container { children: Vec<Box<dyn Component>> }` |
| **pi-mono `TUI.overlayStack`** | `{component, options, preFocus, hidden, focusOrder}` | Overlay 语义直接搬 |
| **pi-mono `ToolRenderContext`** | `args, argsComplete, isPartial, isError, expanded, state, invalidate(), lastComponent` | 映射到 `ToolRenderCtx` struct |
| **pi-mono `renderShell:"default"\|"self"`** | 工具外壳统一/自渲染 | 让 diff tool 自己画 hunk 不要被 box 包 |
| **pi-mono `MessageRenderer`** | `(msg, {expanded}, theme) -> Component\|undefined` | 映射到 trait → Option<RenderSpec> |
| **不抄** | pi 行级字符串 diff / CURSOR_MARKER / jiti 加载 / 同进程 TS | Rust + ratatui 已覆盖前两个；后两个不适用 |
| **不抄** | codex 的 200+ 文件规模 + chatgpt auth + onboarding + voice + realtime | 与我们无关 |

## 1. 核心 trait

```rust
/// 所有可渲染组件实现此 trait（合并 codex Renderable + pi Component）。
pub trait Component: Send {
    /// 画到 ratatui Buffer。
    fn render(&mut self, area: Rect, buf: &mut Buffer);
    /// 期望高度（给 Layout 分配用）。
    fn desired_height(&self, width: u16) -> u16;
    /// 光标位置（仅 focused 组件的会被 terminal 设置）。
    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> { None }
    /// 键盘事件。返回 Handled 表示已消费。
    fn handle_key(&mut self, _key: &KeyEvent) -> KeyOutcome { KeyOutcome::NotHandled }
    /// Paste 文本。
    fn handle_paste(&mut self, _text: &str) -> KeyOutcome { KeyOutcome::NotHandled }
    /// 缓存失效。
    fn invalidate(&mut self) {}
}

pub enum KeyOutcome { Handled, NotHandled, Quit }
```

## 2. Container + Overlay

```rust
pub struct Container {
    children: Vec<Box<dyn Component>>,
}
impl Component for Container {
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        // vertical layout: 按 desired_height 分配 Constraint::Length
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.children.iter().map(|c| c.desired_height(width)).sum()
    }
}

pub struct OverlayEntry {
    pub component: Box<dyn Component>,
    pub anchor: OverlayAnchor,
    pub focus_order: u16,
    pub hidden: bool,
    pub pre_focus: Option<usize>, // index of previously focused
}
pub struct OverlayStack(Vec<OverlayEntry>);
```

Overlay 在主 Container render 之后画：用 `Clear` 清出区域 → `component.render(popup_area, buf)`。

## 3. FrameRequester（搬运 codex 设计）

```rust
pub struct FrameRequester {
    tx: mpsc::UnboundedSender<Instant>,
}
impl FrameRequester {
    pub fn schedule_frame(&self) { let _ = self.tx.send(Instant::now()); }
    pub fn schedule_frame_in(&self, dur: Duration) { let _ = self.tx.send(Instant::now() + dur); }
}
// 内部 FrameScheduler task：coalesce → FrameRateLimiter(8.3ms cap) → broadcast draw signal
```

主循环的 `tokio::select!` 监听此 broadcast + crossterm event + client stream。

## 4. Streaming — AdaptiveChunkingPolicy（搬运 codex 设计）

```
                         ┌─────────────────────┐
  AssistantTextDelta ───▶│ MarkdownCollector    │──newline-complete-source──▶ render ──▶ enqueue
                         └─────────────────────┘
                                                                              │
                         ┌─────────────────────┐          commit tick         ▼
  FrameRequester tick ──▶│ AdaptiveChunking     │◀── queue snapshot ──── StreamState.queued_lines
                         │  Smooth: drain 1     │
                         │  CatchUp: drain all  │──── drained lines ──▶ insert_history_lines
                         └─────────────────────┘
```

Hysteresis 参数（取 codex 默认值，可调）：
- 进入 CatchUp：queue ≥ 8 lines OR oldest age ≥ 300ms
- 退出 CatchUp：queue ≤ 2 lines AND oldest age ≤ 100ms，持续 150ms
- 重新进入抑制：500ms（除非 queue ≥ 20 = severe）

## 5. ToolRenderer + MessageRenderer（合成 pi-mono 设计到 Rust）

```rust
pub struct ToolRenderCtx<'a> {
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub args: Option<&'a serde_json::Value>,
    pub args_complete: bool,
    pub execution_started: bool,
    pub is_partial: bool,
    pub is_error: bool,
    pub expanded: bool,
    pub render_shell: RenderShell,
    /// Per-call persistent state slot (generic Any, survives across render_call→render_result).
    pub state: &'a mut Box<dyn Any + Send>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RenderShell { Default, SelfRendered }

pub trait ToolRenderer: Send + Sync {
    fn tool_name(&self) -> &str;
    fn render_shell(&self) -> RenderShell { RenderShell::Default }
    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec;
    fn render_result(&self, result: &ToolResult, ctx: &mut ToolRenderCtx) -> Option<RenderSpec>;
}

pub trait MessageRenderer: Send + Sync {
    fn custom_type(&self) -> &str;
    fn render(&self, payload: &serde_json::Value, opts: &MessageRenderOpts) -> Option<RenderSpec>;
}

pub struct MessageRenderOpts { pub expanded: bool }

pub struct ToolRendererRegistry {
    by_name: HashMap<String, Arc<dyn ToolRenderer>>,
    fallback: Arc<dyn ToolRenderer>,       // DefaultToolRenderer
}

pub struct MessageRendererRegistry {
    by_type: HashMap<String, Arc<dyn MessageRenderer>>,
}
```

**分发逻辑**（ToolRow 内）：
1. `result.metadata` 含 `UI_RENDER_METADATA_KEY` → 直接用 RenderSpec（远程协议路径）
2. else → `registry.get(tool_name).render_result(result, ctx)`（本地 trait 路径）
3. renderer 返回 None → `fallback.render_result`

**同名覆盖**：`registry.register(r)` 同 key 直接 replace（pi-mono 语义）。

## 6. 模块树

```
crates/astrcode-cli/src/tui/
├── mod.rs                     — pub async fn run(); TerminalSession 生命周期
├── app.rs                     — App 主状态机（按 codex app.rs 拆子模块）
├── app/
│   ├── handle_event.rs        — apply(ClientNotification) 分发
│   ├── handle_session.rs      — session 生命周期事件
│   ├── handle_assistant.rs    — assistant stream 事件
│   ├── handle_tool.rs         — tool 事件
│   ├── handle_extension.rs    — extension command 事件
│   └── input.rs               — dispatch_key / dispatch_paste
├── frame/
│   ├── mod.rs                 — FrameRequester + FrameScheduler
│   ├── rate_limiter.rs        — FrameRateLimiter (120 FPS cap)
│   └── event_stream.rs        — EventBroker / TuiEventStream / TuiEvent
├── component/
│   ├── mod.rs                 — Component trait, Container, OverlayStack
│   ├── composer.rs            — Composer 组件
│   ├── footer.rs              — Footer 组件
│   ├── slash_palette.rs       — Slash palette overlay
│   ├── tool_row.rs            — 单行 ToolRow (active 区)
│   └── transcript.rs          — active tool rows + scrollback flush
├── streaming/
│   ├── mod.rs                 — StreamState (queued_lines + collector)
│   ├── chunking.rs            — AdaptiveChunkingPolicy
│   ├── commit_tick.rs         — run_commit_tick 入口
│   └── controller.rs          — StreamController (push_delta / finalize)
├── render/
│   ├── mod.rs                 — RenderSpec → Vec<Line>
│   └── markdown.rs            — block-level Markdown 子集
├── ext/
│   ├── mod.rs                 — pub use traits + registries
│   ├── tool.rs                — ToolRenderer trait + Registry + ToolRenderCtx
│   ├── message.rs             — MessageRenderer trait + Registry
│   ├── builtin.rs             — 8 个内建 ToolRenderer
│   └── fallback.rs            — DefaultToolRenderer (通用摘要)
├── command/
│   ├── mod.rs                 — SlashCommand 枚举 + parse
│   └── slash.rs               — builtin commands + filtered
├── store/
│   ├── mod.rs                 — TranscriptStore
│   ├── transcript.rs          — Message / MessageBody / ScrollbackEntry
│   └── child_agent.rs         — ChildAgentTracker
├── terminal.rs                — TerminalSession (CSI 2026, inline VP, DECSTBM, resize)
└── theme.rs                   — Theme + detect()
```

## 7. 数据流全图

```
                    ┌──────────────────────────────┐
                    │   astrcode-client subscribe  │
                    └──────────────┬───────────────┘
                                   │ ClientNotification
                                   ▼
 ┌────────────┐ crossterm  ┌──────────────────┐  FrameReq  ┌────────────────┐
 │   stdin    │───event───▶│                  │───────────▶│                │
 └────────────┘            │  main loop       │            │  App           │
                           │  tokio::select!  │◀───cmd─────│  ├─ store      │
 ┌────────────┐            │                  │            │  ├─ component  │
 │ frame tick │───bcast───▶│                  │            │  └─ ext regs   │
 └────────────┘            └────────┬─────────┘            └───────┬────────┘
                                    │                              │
                                    │  on frame tick:              │ render
                                    │  1. commit_tick(chunking)    │
                                    │  2. flush scrollback         ▼
                                    │  3. draw viewport     TerminalSession
                                    └───────────────────────────────────────
```

## 8. 关键设计差异 vs 现有 TUI

| 现有 | 新 | 理由 |
|---|---|---|
| `state.rs` 1240 行 monolith | `app/handle_*.rs` 按事件拆（≤200 行/file） | 可定位 |
| 固定 160-char 切分 | AdaptiveChunking Smooth/CatchUp | 低延迟 + 无碎片 |
| 30Hz `tokio::time::interval` | FrameRequester actor + 120 FPS cap | codex 验证过 |
| `tool_display.rs` 硬编码 match | ToolRenderer 注册表 | 插件扩展 |
| RenderSpec → lines 在 render.rs | 同 + 可被外部 trait 产出 | 双路径 |
| 无 renderShell 概念 | Default/SelfRendered | diff/edit 自绘外框 |

## 9. 错误处理 / 不变量

- `unwrap`/`expect` 仅 `#[cfg(test)]`
- 后台 task（FrameScheduler, EventBroker）有 tracing + panic 记录
- App 状态修改全走 `&mut self`，不在 `.await` 时持锁
- 未知 EventPayload → `_ =>` 默认分支，不 panic
- stdout 不输出 tracing

## 10. 测试策略

- 单元测试就地 `#[cfg(test)] mod tests`
- 重点新测：registry override / fallback / chunking mode transitions / streaming finalize
- 迁移现有 11 个 state 测试 + 5 个 render 测试 + 4 个 composer 测试 + 2 个 slash 测试
- 不写"渲染像素 diff"测试

## 11. Open Questions

- v2 是否用 `inventory`/`linkme` 做 ToolRenderer 自动注册？暂不。
- v2 是否引入 `ratatui-image`？暂不。
- v2 是否加 user keybinding manifest (pi style)？暂不。
