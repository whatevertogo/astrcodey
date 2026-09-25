# AstrCodey crate 维护性评估

日期：2026-09-25。分析基线：`4008d4f2`；提交前已同步 `main`：`d9ee9bd7`，分支 `whatevertogo/fix-project-creation`。

本次检查覆盖 31 个 workspace 成员的依赖与模块入口，并沿新建会话、工具发现、扩展能力调用、会话持久化等路径深入检查。以下记录诊断依据和已经实施的整理，不代表对所有文件逐行审计，也不代表所有 crate 都存在功能缺陷。未发现必要改动的 crate 保持现有边界。

## 已修复的新建项目等待

本机日志显示，2026-09-24 20:54:48.308 收到 `feilian-pojie` 的创建请求，20:55:18.364 成功返回。期间 MCP `jshookmcp` 预热失败，工具发现触发 30 秒 hook 超时。该目录在请求前已经存在，因此这次现象不是 mkdir 权限失败。

根因是创建会话为了持久化初始提示词，同步构建完整工具表，进而等待外部 MCP 初始化。

修复用 `ToolCatalogMode::{RegisteredOnly, WithDiscovery}` 明确区分创建与正式对话的工具发现策略：

- 创建时保留已注册工具和初始提示词的持久化，不等待动态发现。
- 对话准备仍获取完整工具表，并更新提示词。
- 模式属于缓存键，初始工具表不会覆盖完整工具表。
- 原有 MCP 预热和超时仍保留；故障 MCP 仍可能拖慢首轮对话。

相关入口：`crates/astrcode-session/src/session_prompt.rs`、`crates/astrcode-extensions/src/runner/tool_adapter.rs`、`crates/astrcode-extension-sdk/src/runtime_ports.rs`。

临时真实 HTTP 服务验收：在 MCP 子进程已启动并持续不响应的条件下，新建会话返回 HTTP 200，单次耗时 26.3 ms。这是隔离环境的行为验证，不是桌面发布包的延迟基准。

## 已实施的整理

### 1. astrcode-extension-mcp：收敛重复初始化和重试

整理前：`src/lib.rs` 的 `refresh` 先执行 `pool.pre_warm`，再执行 `discover_from_pool`；后者调用 `pool.list_tools`，又通过 `pooled_entry` 保证连接可用。预热失败后，发现阶段会再次尝试连接。同一加载流程还维护 `cache`、`refresh_locks`、`warm_gates` 三组按目录索引的状态。

原来的另一个冗余：生产路径只在 `mark_warm_complete` 中创建 WarmGate，并立即标记完成；`await_initial_warm` 遇到不存在的 gate 或已经完成的 gate 都直接返回。测试会手工插入未完成的 gate，但正常生产路径没有用它记录正在进行的预热，因此 90 秒 gate 等待并未承担预期的初始化同步职责。

已删除 WarmGate、90 秒等待和预热专用方法。现有 `refresh_locks` 是 Single Flight（相同目录的一次加载由多个调用方共同等待）的入口，预热和发现共同执行 `list_tools`，由连接池负责建立连接。同一次刷新不再先预热失败、随后再次初始化。原有工具调用的重连判定、不同工作区配置和缓存指纹语义保持不变。

回归测试覆盖成功与初始化失败、并发刷新、缓存命中和指纹变化：同一轮初始化一次，配置变化后允许再次尝试。

### 2. astrcode-extensions：共享能力调用的前置校验

整理前：`src/host_router.rs` 的 `invoke` 和 `invoke_event_stream` 重复执行调用存活检查、禁止 planning 阶段调用、能力查找与授权、上下文检查、resource lease（本次工具获准访问的资源范围）检查。

已提取 `validate_invoke`，普通和流式调用复用同一检查顺序，并返回已确认的能力描述。现有按 workspace/process/network/session 划分的后端保持不变，沿用 Facade（统一入口）与 Adapter（边界适配）。

已保留流式支持检查、进程取消后的等待回收，以及普通调用与流调用不同的返回契约。`host_router.rs` 大量行数来自测试，不能只据总行数认定生产实现过大。

### 3. astrcode-extension-sdk：明确作者 API 与宿主内部契约

证据：`src/lib.rs` 同时公开作者 API、`wire`、`runtime_ports`；`runtime_ports.rs` 除会话消费的 trait，还承载请求确认信息和内部 handler 身份。会话依赖这些抽象端口，由宿主注入实现，依赖方向本身是合理的。

已把携带 handler 身份和确认状态的实现移入 `runtime_ports/provider_request.rs`，`runtime_ports.rs` 保留宿主注入会话所需的接口，并明确它与扩展作者接口的区别。旧公开路径通过 re-export 保留，继续采用 Ports and Adapters（会话依赖接口，宿主提供实现），未新增 crate。

保留 typed host client（有类型的宿主客户端）和 wire DTO（跨进程数据契约）的边界，不为减少映射而暴露内部 enum。SDK 的 re-export 是兼容入口，不能当作重复实现删除。

### 4. astrcode-server：保持单一生命周期入口，整理内部事务

证据：`src/session_manager.rs` 同时承载 create/open/fork、close/recycle/restore、失败补偿及 `SessionTransitions`；`SessionCommandService` 已经把带 session id 的请求从全局交互 actor 中分离。

已把根会话创建及初始模型校验移入 `session_manager/creation.rs`，fork 准备和阶段补偿移入 `session_manager/fork.rs`。`SessionManager` 仍是唯一生命周期 Facade（统一入口）；关闭、恢复和转换门控保持原有所有权。复用现有 RAII guard（离开作用域自动收尾的对象），未新增 manager 或事务框架。

root、child、fork 在父链接发布、继承提示词、存储补偿上存在真实差异。它们相似的流程不等于可以整段合并。转发存储读取的薄方法是否保留，要看它是否代表生命周期访问入口，不能机械全部删除。

### 5. astrcode-session / astrcode-core：只复用语义一致的原语

证据：`session/src/session_setup.rs` 的 system prompt 指纹使用原始字节 FNV-1a；`core/src/event/fingerprint.rs` 的 `stable_hash_hex` 会在每段后加入 `0xff`。两者算法相近，但输出不等价。

已将原始提示词指纹移入 `core::event::system_prompt_fingerprint`，复用已有 `fnv1a_update` 字节算法；原始文本与分段文本仍各自保留格式。固定值回归测试覆盖空串、普通文本和中文，验证旧指纹不变，也验证其不同于带分隔符的 `stable_hash_hex`。

同样，`extensions/src/host_router/workspace.rs::write_file_atomic` 已转发到 core 的实现，不存在第二套原子写算法；最多是很低优先级的薄别名整理。storage 的 durable write（保证持久落盘的写入）还涉及目录同步等保证，不应与普通原子替换混为一谈。

### 6. astrcode-cli / astrcode-extension-channels / astrcode-eval：按行为拆模块

- CLI：`src/tui/app/handle_event.rs` 同时处理主会话事件、子会话投影、工具摘要和恢复快照。已拆出 `handle_event/child_events.rs` 和 `handle_event/tool_summary.rs`，分别负责子会话事件归约和纯展示摘要，保留 TUI 自己的展示状态。
- channels：`src/lib.rs` 集中 Telegram 配置、轮询状态、会话映射、API 客户端。沿现有 `TelegramApi` 接口拆出 `telegram.rs`（HTTP 与协议）和 `polling.rs`（游标、轮询与取消）；配置及会话映射仍由原运行时持有。
- eval：`src/swebench_instance.rs` 集中容器网络、仓库准备、服务就绪、补丁收集和结果解析。已拆出 `swebench_instance/docker.rs`，集中 Docker 进程调用及容器/网络回收；`ContainerGuard` 的所有权、退出顺序和补偿行为保持不变。

这些是职责拆分。对拆分涉及的五组模块做了函数实现对比：忽略格式、注释和可见性前缀后，原有 308 个函数（包含测试）内容一致。

## 全部 31 个 crate 的建议

| Crate | 本轮建议 |
|---|---|
| astrcode-core | 已复用字节哈希实现并锁定两种指纹格式 |
| astrcode-paths | 保留小型叶子 crate，避免轻量日志为目录函数依赖整个 core |
| astrcode-log | 保持日志初始化和保留策略的单一职责 |
| astrcode-protocol | 保留前端/HTTP/RPC 契约；DTO 映射不是多余复制 |
| astrcode-session-projection | 保持纯状态归约，不并入 storage 或 server |
| astrcode-storage | 保持持久化和并发保证；不要为缩短代码删除边界复验 |
| astrcode-ai | 保留 provider 与 wire 适配分层；本轮未确认必须删除的重复算法 |
| astrcode-context | 保留摘要策略和提示词装配；与 session 的持久化编排职责不同 |
| astrcode-session | 保留创建修复；已复用 core 的提示词指纹实现 |
| astrcode-extension-sdk | 已分离请求确认实现并明确宿主接口，保留公开路径 |
| astrcode-s5r-runtime | 保留 host/worker 共用的 peer 状态机和 I/O driver，不挪回 SDK |
| astrcode-extension-worker | 保留远程调用适配；task-local 与显式绑定 host 的生命周期不同 |
| astrcode-extensions | 已共享普通/流式调用校验，保留回收状态机 |
| astrcode-bundled-extensions | 保留 Composition Root（集中选择和装配内置扩展的入口） |
| astrcode-extension-agent-tools | 保持代理委派领域独立，不向通用工具桶合并 |
| astrcode-extension-coding | 保留通过 SDK host 调用文件/进程的边界 |
| astrcode-extension-mcp | 已删除 WarmGate 和刷新前的重复预热 |
| astrcode-extension-skill | 可按配置/发现/命令入口拆内部模块；尚无必须删除的重复实现结论 |
| astrcode-extension-session-commands | 保留小型声明式扩展；体积小不构成合并理由 |
| astrcode-extension-todo-tool | 保持 session-local 状态边界；只在变更相关功能时整理内部模块 |
| astrcode-extension-mode | 保持模式和 plan artifact 职责，不合并 todo/goal 状态机 |
| astrcode-extension-ask-user | 保留等待注册表、HTTP 响应与取消清理边界 |
| astrcode-extension-goal | 保留预算与自动续跑的独立状态机，不抽象成通用任务状态 |
| astrcode-extension-memory | 保留索引、文件存储和提取流程差异；按现有模块继续局部整理 |
| astrcode-extension-channels | 已拆轮询与协议传输；会话映射保留原所有者 |
| astrcode-extension-web-tools | 保留搜索和 URL 抓取的不同结果与错误语义 |
| astrcode-server | 已拆根会话创建和 fork；补偿语义保持不变 |
| astrcode-client | 保留类型化 RPC 客户端；不与 server 合并 |
| astrcode-cli | 已拆子会话事件归约和纯工具摘要 |
| astrcode-eval | 已拆 Docker 进程调用与容器回收 |
| astrcode-desktop | 保持薄 Tauri 壳和 sidecar 生命周期，不移入会话业务 |

## 验证范围与实施顺序

`scripts/check-deps.py` 已确认 31 个 crate 的依赖方向符合当前规则。这里验证的是声明依赖，不替代运行时状态和职责检查。

根 `Cargo.toml` 设置 `default-members = ["crates/astrcode-cli"]`，因此要覆盖整个项目，应明确执行：

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
python3 scripts/check-deps.py
```

上述必要整理已完成。后续最值得做的是桌面打包验收以及 Windows/Linux 原生验证；继续合并领域状态机、移动持久化检查或增加通用框架，目前没有充分理由。


2026-09-25 整理后的最终验证结果：

- `cargo fmt --check`、`git diff --check`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace --all-features`：1096 通过，0 失败，4 忽略（仓库原有标记）。
- `python3 scripts/check-deps.py`：31 个 crate 通过。
- 前端源码未修改；沿用上一轮构建资源，完整 workspace 检查包含桌面壳。
- 新增必要回归：MCP 单次初始化和持久化指纹固定值。
- 整理后真实 HTTP 验收：MCP 初始化期间创建返回 200，耗时 42.5 ms；同一路径的初始化失败后，观测到 1 次初始化尝试。
- 针对创建的回归覆盖：动态发现挂起时仍可创建；初始提示词仍持久化；首轮对话包含动态工具；初始工具缓存不覆盖完整发现结果。

缓存仍按工作目录字符串隔离；目录别名的自动归一化不在本次整理中。HTTP 验收使用同一规范路径，避免将不同缓存作用域的初始化混算。

以上是本机 macOS 验证，未替换当前运行的桌面安装包，未做 Windows/Linux 原生验收。
