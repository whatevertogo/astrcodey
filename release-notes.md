## v0.3.13

Released: 2026-08-03

### ✨ Features

- feat(todos): 更新 todo 写入工具描述，增加执行者决策指导 (cd6a7a8f)
- feat(sdk): 新增 write_file_atomic 与 DiscoveryCache 原语,消除插件重复实现 (3a7126b2)
- feat: 增强 ShellTool 输出，添加进程状态信息 (b9972691)
- feat: 添加字符裁剪功能并更新相关调用，优化文本处理 (75f70bab)
- feat: add recap block to conversation structure and UI (770df691)
- feat: 添加重发待处理消息功能，优化消息发送体验 (430989f5)
- feat: enhance ask-user extension with improved event handling and legacy support (b8400f93)
- feat: implement session command handling and error management (22a9392d)
- feat: enhance turn runner error handling and finalization structure (1bcd4259)
- feat: Introduce SessionResourceStore for managing session resources (a95091b8)
- feat(session-projection): add ForkSourceRef and enhance session handling (1d977aab)
- feat: add serialization module for OpenAI wire format (f97aaeb2)
- feat: 优化会话流式体验并移除 Gemini 适配 (d5099c81)

### 🐛 Bug Fixes

- fix: 移除过时的事件解析逻辑，增强对无效事件行的处理 (e2e0287e)
- fix: 更新 TodoItem 接口以支持 executor、agentType 和 mode 字段 (ab57316e)
- fix: 补齐 askUser 恢复与安全边界 (e9fb8c96)
- fix: 保留复用 callId 的新问题 (a5ae0c34)
- fix: 补齐 askUser 轮询恢复边界 (204ebf8c)
- fix: 完善 askUser 全局恢复与敏感搜索审批 (318bd007)
- fix: 修复 askUser 冷启动恢复与敏感搜索审批 (fe7b50a8)
- fix(server): 避免重复发送当前会话 askUser 事件 (243502ca)
- fix: 修复 askUser 重连恢复与客户端事件丢失 (7268d3ed)
- fix(ask-user): 修正跨会话状态与自动选择竞态 (7aaaaab3)
- fix(server): 对齐 task_utils 门控与调用方,修复 dead_code (e1feae30)
- fix(session): 补齐敏感文件 glob pattern,与 extensions 侧对齐 (72754dd6)
- fix: 修复多处静默吞错与错误可观测性 (e2d191ac)
- fix(ai): 修复流式响应健壮性并收敛 strict tools 重复逻辑 (fe14cc40)
- fix(InputBar): 修复 Enter 键事件处理以支持 WebKit 兼容性 (67836bf1)
- fix(ci): 修复 nightly clippy 的冗余 struct update (c0df65e4)
- fix: 修复 MarkdownContent 组件的渲染格式 (4f8fe6d4)
- fix(server): align prompt and session lifecycle boundaries (826db261)
- fix(session): preserve runtime registration identity (3ed7ea56)

### 🔧 Refactors

- Refactor tool deduplicator error handling and improve session error handling (79dcfbbc)
- refactor: 删除死代码、收窄过宽 pub、合并 TUI 重复逻辑 (dc8c5726)
- refactor(session): 修复压缩熔断器卡死并清理核心链路坏味道 (cd44e8ac)
- refactor: 移除工具调用状态中的错误状态，更新相关文档和类型定义 (32b76437)
- refactor: 更新工具调用状态处理，统一为完成状态 (28cdd8a1)
- refactor: 优化 SequencedLlmMessage 的构造方式，简化代码 (394c7ed2)
- refactor: 重命名输出文本相关方法以提高可读性 (cb2a0e8f)
- refactor: 统一 thinking 配置体系并清理 session 冗余 (31d3ccf4)
- refactor: harden session runtime and extension boundaries (69619dab)
- refactor: remove astrcode-support crate and migrate functionality (d9d1b64c)
- refactor: consolidate compaction logic and remove unused components (f90ead93)
- refactor(dependencies): update ts-rs features to suppress serde warnings (4f24b4b5)
- refactor(core): relocate thinking boundaries (a244e66a)
- refactor(core): complete boundary cleanup (6c3771bb)
- refactor(ui): remove backend-driven UI contracts (f0232c99)
- refactor(ask-user): make user interaction extension-owned (c776f2e6)
- refactor(tool): replace packs and metadata control signals (f2f503bc)
- refactor(extension): move extension contracts into sdk (64f84de7)
- refactor(projection): extract session projection and storage ports (70dfb34b)
- refactor(core): 完善事件、存储与 LLM 契约拆分 (ac4dff64)
- refactor(core): 移植 vbot-core 优点 Phase 1 — 契约层边界清理 (6eaf33f5)
- refactor(session): 重命名会话参数以提高可读性 (e4c5864c)

### 📝 Other

- ✨ feat(ask-user): 待回答问题移出折叠面板，直接展示在消息流中 (fd49a1a8)
- ✨ feat(ask-user): 跨会话可见 + 60s 自动选择推荐选项 (9236fde2)
- clean (e8c7cc27)
- clean (11c1f724)
- clean (c00a33df)
- clean (c722dc43)
- clean (6eb48c13)
- clean (70827142)
- clean (5df03d9b)
- clean (b0407202)

### Pull Requests

- #43
- #44
- #45

### Contributors

- @whatevertogo

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.13`
