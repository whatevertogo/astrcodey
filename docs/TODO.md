# AstrCode 项目待办事项

## 当前进行中

- [ ] Desktop GUI 完善
  - [ ] 会话管理界面优化
  - [ ] 设置面板功能完善
  - [ ] 工具调用结果可视化

- [ ] Extension 系统增强
  - [ ] 外部插件加载机制

## 高优先级

- [ ] 引入 fd、rg、sed、cat 等外部依赖
  - [ ] 添加可选配置让 agent 系统优先使用终端指令而非内置工具，并抽离内置工具为插件并隐藏
  - [ ] 工具执行策略配置（builtin / external / auto）

<!-- 
- [ ] MCP (Model Context Protocol) 支持完善
  - [ ] MCP 服务器发现与配置
  - [ ] MCP 工具动态注册 -->

- [ ] 上下文压缩优化
  - [ ] 智能压缩策略（基于语义而非纯规则）
  - [ ] 压缩效果评估指标

- [ ] acp协议 

## 中优先级

- [ ] 多模型支持扩展
  - [ ] Anthropic Claude API 适配
  - [ ] Google Gemini API 适配
  - [ ] 本地模型 (Ollama/LM Studio) 支持


- [ ] 会话 Fork/Branch 功能
  - [ ] 会话树可视化
  - [ ] 分叉点管理
  - [ ] 合并/变基支持

- [ ] 性能优化
  - [ ] 启动时间优化
  - [ ] 大文件处理优化
  - [ ] 内存占用优化

- [ ] Eval框架 

## 较低优先级

- [ ] 通过 hook 实现审批插件安全流程
  - [ ] 危险操作确认机制
  - [ ] 策略引擎集成点
  - [ ] 审计日志增强

## 技术债务

- [ ] 测试覆盖率提升
  - [ ] agent loop 集成测试
  - [ ] 扩展系统测试
  - [ ] 端到端测试

- [ ] 文档完善
  - [ ] API 文档自动生成
  - [ ] 扩展开发指南

## 已完成功能

- [x] Core agent loop 架构
- [x] SSE 流式响应处理
- [x] 基础工具集 (read/write/edit/patch/find/grep/shell)
- [x] 上下文自动压缩 (compact)
- [x] Extension/Hook 系统
- [x] Session 事件溯源
- [x] TUI 终端界面
- [x] HTTP/SSE Server
- [x] Desktop GUI (Tauri + React)
- [x] JSON-RPC over stdio 协议
- [x] 运行模式切换 (Code/Plan)
