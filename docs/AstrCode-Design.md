# AstrCode 设计概述

Rust 实现的 AI coding agent，~40k 行，17 crate，310 个测试。10 天完成。

核心判断：**EventLog 是事实，Session 是投影，Agent 是无状态运行时。**

---

## 1. 三层架构

### EventLog — append-only 事实层

- 事件不可变，只追加，`seq` 单调递增
- 工具结果超过阈值时持久化到 artifact 文件，日志只留引用，保持轻量
- 会话恢复 = 从 EventLog 重新投影，fork = 从某个 seq 开始重放

### Session — 投影层

- Session 不存状态，是对 EventLog 的回放结果
- `ConversationReadModel`、`ToolRegistry`、扩展状态都从事件推导
- 不需要"保存 session"——事件已经写回了

### Agent — 无状态运行时

- `AgentLoop` 处理完一个回合即丢弃，不持有跨回合状态
- 从 session projection 读取历史，组装本轮工具和扩展，写回新事件，然后消失
- Agent 崩溃不影响会话——事件已持久化，重新投影即可继续

---

## 2. Compact 设计

### 结构化输出 contract

Compact 不是"请总结一下"，而是一个**严格的 XML contract**：

- 模型必须返回 `<analysis>` scratchpad + `<summary>` 块，顺序固定
- Summary 必须包含 9 个固定段标题（Primary Request、Files、Errors 等），缺一则拒绝
- 输出有 token 上限（`COMPACT_OUTPUT_TOKEN_CAP`）
- 解析器容忍外层 markdown fence、大小写不敏感的 XML tag，但不容忍结构缺失

### Forked provider

Compact 使用独立的 `ForkedProviderRunner` 执行 LLM 调用：

- 复用主请求的 system prompt + tools，**共享 prompt cache key**（基于 system prompt + tools 的稳定哈希，忽略 user/assistant 内容），降低计费成本
- Forked 调用强制 `max_turns = 1`，如果模型尝试调用工具则直接报错——compact 不是对话
- 模型返回的 compact 请求如果 prompt-too-long，按 API round 边界丢弃最早的历史重试（最多 3 轮）

### 双路径 + 熔断

- 优先使用 LLM 驱动的 provider-backed compact
- 连续失败 3 次后自动降级到确定性 fallback（基于规则的摘要）
- 失败计数跟随 session fork 转移（`transfer_session`），不会在新 session 里重复踩坑

### Post-compact 上下文恢复

Compact 会丢失操作上下文。`post_compact_context` 自动恢复：

- 从历史消息中提取 compact 前最近 read 过的文件路径
- 排除 retained tail 中仍然可见的路径，避免重复
- 按文件预算（数量 + token 数）截断大文件
- 渲染为 `<post_compact_context>` 块注入到新上下文

### Incremental compact

已有摘要时，compact 不是从零重写，而是 merge 模式：保留旧摘要，只合并新信息。

---

## 3. Prompt 工程

### 管道式组装

System prompt 由 9 个 `PromptContributor` 按固定顺序拼接，每个 contributor 产出 `PromptSection`，按 `PromptSectionOrder` 排序后输出：

```
Identity → System → Task Guidelines → Communication → Environment
→ User Rules → Project Rules → Tool Summary → Extension → Additional
```

稳定部分（Identity、System、Task Guidelines）排在前面，易变部分（Environment、date）排在后面，配合 prompt cache 利用前缀匹配。

### 用户定制

- `~/.astrcode/IDENTITY.md` 覆盖默认身份
- 项目目录下 `AGENTS.md` 作为 project rules（从 working_dir 向上查找，深层覆盖浅层）
- 扩展通过 `PromptBuild` 事件注入 Skills、Agents、PlatformInstructions 等 section

### 设计取舍

借鉴了 Claude Code 的 prompt 结构，但刻意保持精简。对 MoE 稀疏模型（如 GLM），过长的 system prompt 会稀释专家路由效率，因此每个 section 都是简短的规则列表而非长篇叙述。

---

## 4. 工具设计

### 分层工具而非全 bash

6 个内置文件工具（read / write / edit / patch / find / grep）+ shell 工具：

- **为什么不全用 bash**：Claude Code 可以全 bash 是因为模型足够强。对能力较弱的模型，结构化工具（edit 的 oldStr/newStr 精确替换、patch 的 unified diff）比让模型写 shell 命令更可靠
- edit 支持 `edits` 数组做原子多编辑，先全部验证再一次性写回
- 每个工具声明 `ExecutionMode`：read-only 工具（find/grep/read）标记为 Parallel，写入工具（edit/write/shell）标记为 Sequential

### 工具管线

- `ToolPipeline` 管理完整的 预处理 → 执行 → 提交 流程
- 并行执行用 `JoinSet` 做水位控制（MAX_PARALLEL_TOOL_CALLS = 5），一个任务完成立即补位
- LLM 输出的 JSON 参数解析失败时尝试修复（`parse_and_repair_json`），容错弱模型的格式问题
- 工具结果有**全局消息预算**：总字符数超限时按大小降序优先持久化最大的结果

### 后台化

Shell 工具支持 `BackgroundPolicy::AutoAfter`——执行超过阈值时间后自动后台化，不阻塞 agent loop。

---

## 5. 扩展 / Hook 系统

### 生命周期钩子

扩展订阅 agent 生命周期事件，可拦截、修改或阻止操作：

- `PreToolUse` / `PostToolUse` — 检查、修改或阻止工具执行
- `BeforeProviderRequest` / `AfterProviderResponse` — 修改消息或阻止 LLM 调用
- `PreCompact` / `PostCompact` — 注入 compact 指令
- `PromptBuild` — 贡献 system prompt 片段
- `TurnStart` / `TurnEnd` / `UserPromptSubmit` — 会话级生命周期

### 三种钩子模式

- **Blocking**：同步执行，可返回 Block / ModifiedInput / ModifiedResult
- **NonBlocking**：即发即弃，使用快照上下文，不阻塞主流程
- **Advisory**：结果仅记录日志，不强制执行

### 延迟绑定

`ExtensionRuntime` 在 server 完全启动前允许扩展注册工具，等 session projection 就绪后通过 `bind()` 注入实际能力。子 agent 嵌套深度限制为 3 层，原子计数器保护。

### 当前状态

只有内部插件实现（MCP client / Skill / Agent-Tool / Todo），尚未开放外部插件加载。

---

## 6. 前后端分离

借鉴 OpenCode 的架构：

- **后端**：`astrcode-server` 提供 HTTP/SSE API（Axum），JSON-RPC 2.0 协议
- **前端**：可以是 TUI（`astrcode-cli`）或任何 HTTP client
- 路由：sessions CRUD、prompt 提交、compact、abort、fork、SSE 事件流
- SSE 流携带 `cursor`（event seq），客户端断连后可从 cursor 恢复
- broadcast channel 溢出时发送 `RehydrateRequired` delta，通知客户端重新拉取 snapshot

### 传输层

- `astrcode-client`：typed JSON-RPC client + stream subscription
- `astrcode-protocol`：wire types（commands / events / framing / HTTP DTO）独立 crate
- 支持 stdio transport（`astrcode-server --stdio`）用于嵌入式场景

---

## 7. TUI

基于 ratatui 的终端界面：

- 消息写入终端原生 scrollback，底部只渲染固定高度的输入区 + footer
- 用户可以用终端原生滚动（鼠标滚轮、Shift+PageUp）浏览历史
- Slash 命令面板、composer 输入框、工具调用状态展示

### 不足

- **没有多行编辑**：composer 是单行输入，长提示词编辑体验差
- **Markdown 渲染粗糙**：assistant 消息的 markdown 渲染是 block-first 的简化版，代码块、表格等格式支持有限
- **没有会话列表 UI**：不支持在 TUI 内切换会话，需要重启
- **工具输出截断**：大工具输出在 TUI 中只显示 preview，无法交互式展开查看
- **依赖终端能力探测**：`terminal_probe` 检测终端尺寸和能力，但对 Windows Terminal、tmux 等环境的兼容性不完整

---

## 8. 不足与待改进

| 方向 | 现状 | 影响 |
|---|---|---|
| **权限模型** | 无 sandbox / approval，工具执行无需审批 | 不能安全地交给他人使用 |
| **Provider 多样性** | 仅 OpenAI 兼容协议 | 无法接入 Anthropic / Google 原生 API |
| **错误类型** | `AgentError::Internal(String)` 过于收敛 | 上层无法差异化处理（重试/降级/提示） |
| **MCP transport** | 仅 stdio | 不支持 HTTP/SSE，主流 MCP server（Context7 等）无法连接 |
| **外部插件** | 只有内部插件 | 无法加载第三方扩展 |
| **配置验证** | API key 为空时启动不报错 | 首次调用才失败，用户体验差 |
| **测试覆盖** | 核心 crate 有单元测试，缺集成测试和 E2E | 跨 crate 交互的回归风险 |
