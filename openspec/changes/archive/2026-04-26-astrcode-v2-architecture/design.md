## Context

当前 astrcode 代码库由 6 个 adapter crate + 5 个参考 crate 组成，缺乏统一架构骨架。v2 采用极简核心哲学：**核心只保留 hooks、agent loop、compaction、内置 tools 四个能力，其余一切交给扩展实现**。借鉴 pi-mono 的 extension 设计和 codex 的 app-server 分层。

**约束**：
- 纯 Rust 实现
- 一期通信 stdio (JSON-RPC 2.0)
- 不做沙箱、不做 MCP
- 存储：JSONL + 快照
- 命名：`astrcode-*` 前缀，全部放入 `crates/`

## Goals / Non-Goals

**Goals:**
- 极简核心：agent loop + hooks + compaction + built-in tools
- 扩展优先：skills、agent profiles、自定义工具、slash 命令全部由扩展注册
- 前后端分离：server 通过 stdio JSON-RPC 对外服务
- 多 Session 并发 + Session 树（fork/branch/switch）

**Non-Goals:**
- MCP 集成
- WebSocket / Web UI（预留扩展点）
- 执行沙箱（留 TODO）
- skills、agent profiles 作为核心功能（由扩展实现）

## Decisions

### 1. 扩展优先的 Crate 分层

核心只保留 14 个 crate，5 层：

```
Layer 0: Foundation     astrcode-core, astrcode-support
Layer 1: Services       astrcode-ai, astrcode-prompt, astrcode-tools,
                        astrcode-storage, astrcode-context
Layer 2: Extensions     astrcode-extensions     ← 核心可扩展性来源
Layer 3: Server         astrcode-server, astrcode-protocol
Layer 4: Frontend       astrcode-client, astrcode-tui, astrcode-exec, astrcode-cli
```

**被移除的 crate**（改为扩展实现）：
- ~~astrcode-skills~~ → skill 加载由扩展注册，两阶段加载逻辑在扩展中
- ~~astrcode-agents~~ → agent profile 由扩展注册，核心只保留内置 explore/reviewer/execute

**依赖流向**：上层可依赖下层，下层不可依赖上层。

### 2. 四大核心能力

#### 2a. Agent Loop（`astrcode-server`）

核心执行管线：

```
UserInput → [UserPromptSubmit hooks]
         → PromptComposer.assemble()
         → LlmProvider.generate() → 流式 MessageDelta 事件
         → 解析 tool calls
         → 对每个 tool call: [BeforeToolCall hooks] → Tool.execute() → [AfterToolCall hooks]
         → 追加事件到 Session
         → [TurnEnd hooks]
```

Agent 是临时的：从 Session 事件日志重建，处理完写回事件，可随时丢弃和重建。

#### 2b. Hooks（`astrcode-extensions`）

**12 个核心生命周期事件**：SessionStart、SessionBeforeFork、SessionBeforeCompact、SessionShutdown、AgentStart、AgentEnd、TurnStart、TurnEnd、BeforeToolCall、AfterToolCall、MessageDelta、UserPromptSubmit。

**3 种 HookMode**：Blocking（可阻断）、NonBlocking（不可阻断）、Advisory（仅供参考）。

**加载策略**：全局（`~/.astrcode/extensions/`）+ 项目级（`.astrcode/extensions/`）合并，项目级优先。

**扩展能力注册**：
- 事件处理器（核心）
- 自定义工具（Tool trait 实现）
- Slash 命令
- 上下文提供者（PromptContributor）
- Skill 定义和加载逻辑
- Agent profile 定义

**设计意图**：skills、agent profiles、code review、deploy 等全部功能都作为扩展加载，核心不硬编码任何特定功能。

#### 2c. Compaction（`astrcode-context`）

流水线：token 估算 → 工具结果预算 → 微压缩 → 修剪 → LLM 驱动压缩 → 文件恢复。

由 `RuntimeConfig` 的 ~20 个参数控制行为。

#### 2d. Built-in Tools（`astrcode-tools`）

**13 个内置工具**：readFile、writeFile、editFile、applyPatch、findFiles、grep、shell、toolSearch、skillTool、taskWrite、enterPlanMode、exitPlanMode、upsertSessionPlan。

**4 个 Agent 协作工具**：spawn、send、observe、close。

扩展可通过 `ExtensionCapabilities.tools` 注册新工具。

### 3. Session-First 事件溯源

- Session 是只追加 JSONL 事件日志，定期创建快照
- Agent 从 Session 事件重放构建，处理 turn 后写回事件
- Session 树：fork 创建新日志（含父引用），branch/switch 基于此

### 4. stdio JSON-RPC 通信

- ClientCommand（前端→后端）、ServerEvent（后端→前端）
- UI 请求子协议：confirm/select/input/notify
- 版本协商

### 5. 配置架构

四层加载（Defaults → User → Project → Env）。Profile 机制、RuntimeConfig(~30 参数)、AgentConfig(6 参数)。API key 解析（`env:VAR`/optional/literal）。热重载。

### 6. 扩展如何替代 skills 和 agents

**Skills 作为扩展**：一个 "skill-loader" 扩展在 SessionStart 时扫描 `~/.astrcode/skills/` 和 `.astrcode/skills/`，为每个 skill 注册一个工具（skillTool handler），在 prompt 中注入 skill 摘要。两阶段加载逻辑完全在扩展内。

**Agent profiles 作为扩展**：一个 "agent-loader" 扩展在 SessionStart 时扫描 agent 定义文件，注册到 prompt contributor，注入 agent 摘要。内置 explore/reviewer/execute 由核心的默认 contributor 处理。

## Risks / Trade-offs

- **[Risk] 扩展加载失败影响核心稳定性** → **Mitigation**: 扩展加载隔离，单个扩展失败不影响 agent loop 和其他扩展
- **[Risk] 扩展间冲突**（两个扩展注册同名工具）→ **Mitigation**: 后加载的覆盖先加载的，emit 警告
- **[Risk] JSONL 追加性能** → **Mitigation**: BatchAppender 合并 50ms 窗口
- **[Risk] Extension 阻断超时** → **Mitigation**: 30s 超时 + 可配置超时策略
- **[Risk] 长 session 事件重放慢** → **Mitigation**: 快照 + 尾部增量

## Migration Plan

1. 创建 workspace 和 crate 骨架
2. Layer 0 → Layer 1 → Layer 2 → Layer 3 → Layer 4 递进实现
3. Skills 和 agents 作为首批扩展实现（验证扩展系统）
4. 移除旧 adapter 目录
