# AstrCode 设计概述

Rust 实现的 AI coding agent，~45k 行（Rust ~41k + TypeScript ~4k），18 crates，支持 TUI、CLI 和 Desktop GUI 三种前端。

核心判断：**EventLog 是事实，Session 是投影，Agent 是无状态运行时。**

---

## 1. 架构

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
- 失败计数基于 session ID 在全局 `AutoCompactFailureTracker` 中跟踪，compact continuation children 共享同一条 session 线

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

只有内部插件实现（MCP client / Skill / Agent-Tool / Todo / Mode），尚未开放外部插件加载。

---

## 6. 前后端分离

借鉴 OpenCode 的架构：

- **后端**：`astrcode-server` 提供 HTTP/SSE API（Axum），JSON-RPC 2.0 协议
- **前端**：可以是 TUI（`astrcode-cli`）、CLI 或 Desktop GUI（Tauri + React）
- 路由：sessions CRUD、prompt 提交、compact、abort、fork、SSE 事件流
- SSE 流携带 `cursor`（event seq），客户端断连后可从 cursor 恢复
- broadcast channel 溢出时发送 `RehydrateRequired` delta，通知客户端重新拉取 snapshot

### 传输层

- `astrcode-client`：typed JSON-RPC client + stream subscription
- `astrcode-protocol`：wire types（commands / events / framing / HTTP DTO）独立 crate
- 支持 stdio transport（`astrcode-server --stdio`）用于嵌入式场景

### Desktop GUI (Tauri)

- **Sidecar 模式**：嵌入 `astrcode-http-server` 作为 sidecar 进程
- **通信方式**：HTTP API + SSE（本地动态端口）
- **技术栈**：Tauri v2 + React 18 + TypeScript + Tailwind CSS v4
- **状态管理**：Zustand
- **安全**：CSP 配置限制外部连接

---

## 7. 运行模式

### Code 模式（默认）

- 完整工具访问权限
- 支持文件读写、编辑、shell 执行
- 适合实际编码任务

### Plan 模式

- 只读工具限制（find/grep/read）
- 专用 Plan 管理工具
- Exit Gate 自检：完成前必须经过自我审查检查清单
- 计划持久化到 `<session>/plan/plan.md`
- 适合复杂任务的前期规划

---

## 8. 项目统计

| 指标 | 数值 |
|------|------|
| Rust 代码行数 | ~41k |
| TypeScript/TSX 代码行数 | ~4k |
| Crates 数量 | 18 |
| 前端组件数量 | 15+ |
| 内置工具数量 | 7 |
| 扩展 crate 数量 | 6 |

---

## 9. 关键设计决策

### Session-First 事件溯源

Session 是唯一的持久事实来源。所有状态变化都以不可变事件形式写入 JSONL。恢复时从事件重放重建状态，支持 fork/branch/replay。

### Extension-First 架构

核心只保留必须通用的能力（agent loop、hooks、context compaction、built-in tools）。其他能力（skills、MCP、自定义工具）通过扩展接入。

### 工具-First 而非 Plugin-First

工具是运行时基础能力，extension、SDK、MCP 都只是 tool source。所有工具走同一条执行路径，确保可观测性和统一调度。

### 前后端分离

前端不负责业务逻辑，只负责交互和渲染。后端通过 HTTP/SSE 提供标准化 API，支持多种前端形态（TUI/GUI/Headless）。
