# PROJECT_ARCHITECTURE

## 状态与范围

本文描述的是 `astrcode-v2-architecture` 提案定义的 v2 目标架构。
它是一份架构总览，不表示仓库当前已经完整实现了这里的全部内容。


## 架构方向

astrcode v2 的目标是一套 session-first、extension-first、核心极简的 coding system。
它的整体形态是：

- 一个后端 server
- 多种 frontend
- 基于追加事件的会话持久化
- 由 session 历史重建的临时 agent
- 少量内置核心能力
- 其余能力全部通过扩展接入

这套设计明确避免把系统做成一个硬编码功能不断堆积的单体。
核心只保留那些必须对所有场景都成立的能力：

- agent loop
- hooks 与 extension runtime
- context compaction
- built-in tools

skills、agent profiles、自定义工具、prompt context providers 都不作为核心内建能力看待，而是统一通过扩展系统加载。

## 核心原则

### 1. Session-first

Session 是系统唯一的持久事实来源。
所有关键状态变化都以不可变事件的形式写入持久层。
任何时候都可以通过事件日志和快照重建 session 状态。

这意味着两件事：

- 持久化单元是 session，不是 agent
- 恢复、fork、branch、replay 首先是存储与事件模型问题

### 2. Agent 是临时处理器

Agent 不是持久对象，而是从 session 历史中重建出来的临时处理器。
它负责处理一个 turn，发射事件，落盘，然后可以被丢弃。

这样做能把长期状态从内存对象里剥离出来，让重启恢复、并发会话和故障恢复都更清晰。

### 3. Extension-first

扩展系统是整个架构的主要定制边界。
核心只暴露生命周期事件和注册点，工作流相关、领域相关、团队相关的能力都应该放在扩展里，而不是继续侵入核心。

### 4. 前后端分离

Frontend 不负责业务核心逻辑。
它们通过 stdio 上的类型化协议与 server 通信，主要承担交互、渲染和连接职责。

### 5. 基础设施保持克制

v2 首期刻意选择保守、明确、容易落地的基础设施组合：

- 纯 Rust workspace
- JSON-RPC 2.0 over stdio
- JSONL 事件日志加快照
- 核心不做 MCP
- v2 范围内不做执行沙箱

## 系统拓扑

```text
┌─────────────────────────────────────────────────────────────────┐
│                           Frontends                            │
│                                                                 │
│  astrcode-tui              astrcode-exec              astrcode-cli │
└───────────────────────────────┬─────────────────────────────────┘
                                │
                                │ JSON-RPC 2.0 over stdio
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                         astrcode-server                         │
│                                                                 │
│  SessionManager                                                  │
│  Agent Loop                                                      │
│  Config Service                                                  │
│  Transport Handler                                               │
└───────────────┬───────────────────────────────┬─────────────────┘
                │                               │
                │                               │
                ▼                               ▼
┌──────────────────────────────┐   ┌──────────────────────────────┐
│ Runtime Services             │   │ Extension Runtime            │
│                              │   │                              │
│ astrcode-prompt              │   │ astrcode-extensions          │
│ astrcode-tools               │   │                              │
│ astrcode-ai                  │   │ 注册 tools、commands、       │
│ astrcode-context             │   │ context providers、skills、  │
│ astrcode-storage             │   │ agent profiles               │
└───────────────┬──────────────┘   └──────────────────────────────┘
                │
                ▼
┌─────────────────────────────────────────────────────────────────┐
│                         Session Storage                         │
│                                                                 │
│        append-only JSONL event log + snapshots + file locks     │
└─────────────────────────────────────────────────────────────────┘
```

整体运行模型可以概括为：一个后端，多种前端，session 作为持久真相。

## Workspace 与分层

v2 设计把系统拆成 `crates/` 下的 14 个 crate，并分为五层。
依赖方向严格单向：上层可以依赖下层，下层不能反向依赖上层。

### Layer 0：Foundation

- `astrcode-core`：共享领域类型与核心 trait，例如 tool、LLM provider、config 抽象、extension contract
- `astrcode-support`：宿主环境集成辅助能力，例如路径解析、shell 检测、tool result 持久化工具

### Layer 1：Services

- `astrcode-ai`：OpenAI 兼容 provider、SSE 流解析、重试、缓存追踪
- `astrcode-prompt`：基于 contributor 的 prompt 组装、分层缓存、诊断
- `astrcode-tools`：内置工具、工具注册表、执行包装、agent 协作工具
- `astrcode-storage`：JSONL event log、snapshot、config 持久化、锁
- `astrcode-context`：token 估算、tool result 预算、裁剪、压缩、文件恢复

### Layer 2：Extensions

- `astrcode-extensions`：扩展加载、生命周期分发、hook 执行策略、超时处理、能力注册

这一层是 v2 的主要定制边界。

### Layer 3：Server

- `astrcode-protocol`：类型化 JSON-RPC 命令、事件、UI 子协议、版本协商
- `astrcode-server`：session 生命周期、agent 编排、config service、transport handling、并发控制

### Layer 4：Frontend

- `astrcode-client`：面向 transport 的类型化 client 抽象
- `astrcode-tui`：交互式终端前端
- `astrcode-exec`：无头单次执行前端
- `astrcode-cli`：用户入口，负责分发到 server、tui 或 exec 流程

## 核心运行模型

### Session 模型

Session 是基于 JSONL 和定期 snapshot 的追加事件流。
它记录：

- 用户输入
- assistant 输出增量与最终消息
- tool call 与 tool result
- compaction 相关事件
- 恢复与协作用到的生命周期事件

每个 session 都以 `SessionStart` 事件开头。
fork 会基于某个 cursor 复制历史并创建新的 session，同时带上父 session 引用。

### Agent 模型

Agent 自身不持有持久状态。
每次 turn 开始时，server 从 session 的持久事件中重建 agent 状态。
turn 结束后，新事实再写回 session。

这个模型直接支持：

- 重启后的确定性恢复
- 多个独立 session 并存
- session tree 上的 fork / branch 操作
- 处理逻辑与持久化边界清晰分离

### 并发模型

Server 支持多个 session 同时活跃。
并发规则是：

- 单个 session 内一次只处理一个 turn
- 不同 session 之间可以并行处理

这样既能保证每个 session 内部事件历史线性一致，又能保留整体吞吐。

## Turn 处理流水线

Agent loop 是整个运行时的中心。
一个 turn 的高层流程如下：

```text
submit_prompt
  -> load or rebuild agent from session
  -> UserPromptSubmit hooks
  -> assemble prompt from contributors
  -> call LLM provider and stream deltas
  -> parse requested tool calls
  -> BeforeToolCall hooks
  -> execute tools
  -> AfterToolCall hooks
  -> append and broadcast events
  -> loop back into LLM if tool results require continuation
  -> TurnEnd hooks
```

这条流水线有几个关键特征：

- 全流程事件化，frontend 可以增量渲染
- tool execution 属于 turn 主流程，不是旁路能力
- tool error 会以结构化结果回送给模型
- abort 会保留终止点之前已经完成的结果

## Prompt 组装

Prompt 组装被视为独立子系统，而不是简单的字符串拼接。
设计采用 contributor 模式，由各 contributor 产出带元数据的 block，例如优先级、条件、依赖和缓存层级。

Prompt composer 负责：

- 收集内置 contributor 和扩展 contributor 产出的 block
- 去重与排序
- 解析模板变量
- 校验依赖关系
- 生成最终用于 LLM 调用的 prompt plan

Prompt cache 被拆成四层：

- Stable
- SemiStable
- Inherited
- Dynamic

这样可以在减少重复组装成本的同时，保留每个 turn 所需的动态上下文。

## 扩展系统

扩展系统不是附属功能，而是架构核心设计的一部分。
它定义了非通用能力如何进入系统。

扩展可以订阅生命周期事件，包括：

- session 生命周期
- turn 生命周期
- tool 执行前后
- message 流式输出
- prompt 提交

每个订阅都声明一种 hook mode：

- `Blocking`：可以放行或阻断操作
- `NonBlocking`：异步执行，不能阻塞主路径
- `Advisory`：只提供信息，不接管执行流

扩展可以注册：

- 自定义 tools
- slash commands
- prompt/context contributors
- skill 加载行为
- agent profile 定义

加载策略分层：

- 全局扩展：`~/.astrcode/extensions/`
- 项目扩展：`.astrcode/extensions/`

项目级行为在当前 session 中优先。

## Tool 模型

工具系统只内置少量核心工具，覆盖文件访问、编辑、搜索、shell 执行、计划管理和 agent 协作。
自定义工具与内置工具走同一条执行路径。

也就是说，每个 tool call 都会经过同样的架构链路：

- 类型化 tool definition
- registry lookup
- lifecycle hooks
- 必要时的流式执行事件
- 持久化结果
- 在返回模型前进行可选后处理

这点很重要，因为系统不希望出现绕过可观测性、策略检查和统一调度的“特殊工具”。

## 存储与恢复

存储设计追求的是运行时清晰性，而不是引入复杂数据库。

核心持久化选择包括：

- 每个 session 一个 JSONL append-only event log
- 定期 snapshot，加速重建
- 基于 snapshot 之后 tail 的增量回放
- 基于文件的 turn lock
- 原子 config 写入

因此 session 恢复流程很直接：

1. 如果存在 snapshot，先加载最新 snapshot
2. 回放 snapshot 之后的剩余事件
3. 重建内存中的 session state
4. 在下一次 turn 启动时重建 agent

## 协议边界

Frontend 与 server 之间使用类型化 JSON-RPC 2.0 over stdio。
每条协议消息都是单行 JSON 对象，以换行分隔。

协议边界覆盖三类通信：

- frontend 发给 server 的 commands
- server 发给 frontend 的流式 events
- confirm / select / input / notify 等交互型 UI requests

这个边界让 server 可以复用于多种 frontend，也让 headless 执行成为自然能力，而不是嵌在 UI 里的特殊流程。

## 配置模型

配置采用多层叠加，并参与运行时行为控制。
优先级为：

`Defaults -> User -> Project -> Env`

配置模型包括：

- 基于 profile 的 model/provider 选择
- compaction、budget、retry、concurrency 等运行时参数
- sub-agent 限制
- 基于环境变量的 secret 解析
- 面向长生命周期 server 的热重载

因此 config 不只是启动参数，而是运行时编排的一部分。

## 明确不在 v2 核心范围内的内容

v2 架构明确不把以下内容纳入当前核心范围：

- MCP 集成
- 执行沙箱
- 第一阶段的 WebSocket transport
- 第一阶段的 Web UI
- 硬编码的 skills 和 agents 子系统

这些能力未来可以追加，但当前架构决策不依赖它们存在。

## 取舍

这套架构做了几项明确取舍：

- 事件溯源会增加 replay 成本，但换来恢复能力、可审计性和分支能力
- extension-first 会提高灵活性，但要求更清晰的优先级规则与故障隔离
- stdio-first 会限制初期接入面，但能显著简化协议和进程模型
- 极简核心降低耦合，但也把更多责任转移到扩展契约和 crate 边界设计上

