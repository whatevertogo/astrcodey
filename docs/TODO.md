# AstrCode 项目待办事项

## 中优先级

- [ ] **插件系统 / 扩展（s5r + SDK）**
  - [ ] **宿主能力补齐（`HostRouter` / wire）**
    - [x] `astrcode.session.control.create` 透出 `tool_selection`（外置 agent 禁嵌套 `agent`）
    - [ ] 外置扩展安全路径下的同步子 Agent（`wait_for_result` 与 peer I/O 线程死锁方案）— 当前仅有 guard（peer 线程拒绝 `wait_for_result: true` 并降级为 `false`），外置扩展无法同步等待子 agent 结果
    - [x] 实现 `astrcode.process.spawn`、`astrcode.network.client`（并发、总超时、取消与 I/O 大小均有上限）
    - [x] 实现 public HTTP 路由与跨插件公开路由分发（含 s5r manifest/handler E2E）
    - [x] 实现 workspace `list` / `grep` / `glob` 与 session 中断、注入、取消、执行视图
  - [ ] **外置扩展与内置能力对齐**
    - [x] `S5rToolHandler` 透传 turn 取消 → `InvokeContext.cancel_token`（`peer.rs` → `registry.rs` → `host_router.rs` → `session.rs` 全链路已打通）
    - [ ] s5r 支持 `tool_metadata` / `ToolDiscovery`（对标 MCP 动态工具、agent prompt 元数据）— 内置扩展完整支持，s5r wire 协议尚无 `tool_metadata` / `ToolDiscovery` 字段
    - [x] extension event 统一经声明校验后的 `EventClient::emit` / `ExtensionEventEmitter::emit` 发射；裸 sink 仅留在 doc-hidden runtime internal boundary
  - [ ] **SDK 开发体验（Worker）**
    - [x] `tool_handler` / `tool_handler_args`、manifest 与 handler 一体注册（`Worker::tool`）
    - [x] Handler 错误类型 `ErrorPayload`；`HostApi` + task-scoped `with_host_api` transport seam 可测
    - [x] [`extension-author-guide.md`](extension-author-guide.md)（含外置 agent-tool 指引）
    - [ ] `#[handler]` 过程宏（可选，进一步减样板）
    - [ ] Handler 运行时进度 / 日志上报通道（协议 + SDK API）
    - [ ] 合并 / 澄清 `prelude` 与 `worker_prelude` 文档入口（README 链到 author guide）— 两个模块都存在且 author guide 已有解释，但 SDK crate 无 README 链接
  - [ ] **内置扩展 vs 外置部署策略**
    - [ ] 明确各内置 crate（agent-tools / mcp / skill / todo / mode / goal / memory / channels / web-tools）的外置替代矩阵与默认开关
    - [ ] MCP 保持独立桥接层，不与 s5r 合并（文档中写清边界）
  - [ ] **测试与 CI**
    - [ ] 外置 agent-tool 最小 E2E（`session_control` + `prompt_build` + 后台 submit_turn）
    - [ ] CI 可选构建 `examples/` 或模板外置扩展工程
- [ ] 审批插件安全流程（通过 hook 实现）
  - [ ] 危险操作确认机制
  - [ ] 策略引擎集成点
  - [ ] 审计日志增强
- [ ] ACP 协议完善
- [ ] CodeGraph

## 较低优先级

- [ ] 会话 Fork 分支点管理
- [ ] 引入 fd、rg 等外部依赖，可选配置工具执行策略（builtin / external / auto）
- [ ] AgentTeam 插件
  - [ ] AgentSendTool
  - [ ] 聊天室
  - [ ] 主 agent task 分发
- [ ] 文档完善
  - [ ] API 文档自动生成
  - [x] 扩展开发指南（[`extension-author-guide.md`](extension-author-guide.md)、[`extension-system.md`](extension-system.md)、[`s5r-protocol.md`](s5r-protocol.md)）
  - [x] README 中英文同步（crate 统计、web-tools 扩展、行数更新）
  - [x] 发布指南（[`release.md`](release.md)，含版本同步、npm/GitHub 分发与 weekly release 行为）
