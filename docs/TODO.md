# AstrCode 项目待办事项

## 当前进行中




## 高优先级



## 中优先级

- [ ] 会话 Fork/Branch 功能
  - [x] 基础 Fork 实现（SessionManager::fork + HTTP 路由 + ACP/SSE 支持）
  - [ ] 会话树可视化
  - [ ] 分支点管理
  - [ ] 合并/变基支持


- [ ] ACP 协议完善

## 较低优先级

- [ ] 通过 hook 实现审批插件安全流程，权限系统
  - [ ] 危险操作确认机制
  - [ ] 策略引擎集成点
  - [ ] 审计日志增强

- [ ] 引入 fd、rg、sed、cat 等外部依赖
  - [ ] 添加可选配置让 agent 系统优先使用终端指令而非内置工具，并抽离内置工具为插件并隐藏
  - [ ] 工具执行策略配置（builtin / external / auto）

- [ ] AgentTeam插件
  -[ ] AgentSendTool
  -[ ] 聊天室
  -[ ] 主agent task 分发

## 技术债务

- [ ] 测试覆盖率提升
  - [ ] agent loop 集成测试
  - [ ] 扩展系统测试
  - [ ] 端到端测试

- [ ] 文档完善
  - [ ] API 文档自动生成
  - [ ] 扩展开发指南


## 已完成功能

- [x] Eval 评测框架（HTTP 服务器控制、事件日志指标、结构化报告、7 个评测用例）
- [x] Core agent loop 架构
- [x] SSE 流式响应处理
- [x] 基础工具集 (read/write/edit/patch/find/grep/shell/task)
- [x] 上下文自动压缩 (compact，LLM 生成摘要 + 确定性 fallback)
- [x] Extension/Hook 系统
- [x] Session 事件溯源
- [x] TUI 终端界面（codex-style inline viewport 重写）
- [x] HTTP/SSE Server（模块拆分为 routes/projection/stream/auth）
- [x] Desktop GUI (Tauri + React)
- [x] JSON-RPC over stdio 协议
- [x] 运行模式切换 (Code/Plan) — 迁移为插件
- [x] WASM 扩展运行时 (wasmtime)
- [x] 原生扩展加载 (FFI)
- [x] reasoning_split 配置（推理内容分离到独立字段）
- [x] 插件化 Mode 系统（快捷键注册 + 状态栏项注册）
- [x] NPM 分发（跨平台 CLI 二进制）
- [x] Weekly Release 自动化
- [x] TUI slash palette（滑动窗口命令列表）
- [x] TUI 会话选择器
- [x] TUI Ctrl+C 二次确认退出
- [x] 复用稳定系统提示词前缀 KV 缓存
- [x] BackgroundTaskOutput 增加 call_id 原生关联 tool-call block（ACP 使用 ToolCallUpdate，HTTP/SSE 使用 ToolOutput delta）
- [x] 性能优化
  - [x] 启动时间优化
  - [x] 大文件处理优化
  - [x] 内存占用优化
- [x] recap功能
- [x] 前端状态栏实时更新（StatusItemUpdate 通过 SSE 推送）
