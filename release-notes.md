## v0.3.15

Released: 2026-08-17

### ✨ Features

- feat: 优化持久化文件写入逻辑，使用独立临时文件以支持并发替换 (57d031ce)
- feat: refine provider normalization logic for tool entries during execution (3ad9e094)
- feat: enhance message list handling with pagination and action block management (3b47e4d3)
- feat: optimize event handling and transcript management in streaming (a979d4ce)
- feat: implement conversation timeline with pagination and state management (5ecb6e69)
- feat(storage): add performance baseline documentation for astrcode-storage (befaee17)
- feat: introduce per-tool execution timeout policy (327e955c)
- feat: update cold open benchmark to support multiple event counts (8b712a9f)
- feat: implement session ownership lease and session projection (a20a7d65)
- feat: Final cleanliness pass for PR #47 (275fea1e)
- feat(tests): enhance extension integration tests with planning context and resource lease (c9c07e58)
- feat: add S5R Phase-0 cleanliness audit documentation (674e7a14)
- feat: enhance early tool execution and session management (64b7bcc7)
- feat: 更新 DurableEventPayload 的序列化逻辑以兼容旧日志格式 (08f18eb0)
- feat: 更新 CustomEventConsumerStatus 和 DTO，调整 quarantined_events 类型为 u64 (600a907c)
- feat: 更新 S5R 3.0 协议，添加效果常量并优化文档 (93b5a09d)
- feat: update extension protocol to S5R 3.0 and improve documentation (e2fd26a4)
- feat: Introduce event consumer state management and custom event handling (24b75dc3)
- feat: Enhance event emission system with new EventPublishReceipt and error handling (d7795ff1)

### 🐛 Bug Fixes

- fix: update parameter type for extension command context and request handling (d5ccdc87)
- fix(extensions): harden S5R lifecycle and clarify ownership (5a0ece23)
- fix(extensions): 收紧 S5R 运行时边界 (86c0dffc)
- fix: 修正持久化自定义事件处理中的变量命名 (ddd4c19a)

### 🔧 Refactors

- refactor: 收敛跨模块重复实现与遗留契约 (1a951cfd)
- Refactor LlmMessage handling to use Arc for shared ownership (564a9fbb)
- Refactor session storage and message handling for improved performance (fe1756d2)
- refactor: rename SessionEventPublisher to SessionEventSink and update related documentation (9919f897)
- Refactor and clean up extension worker and manifest handling (9c1ed550)
- refactor(extensions): 收敛作者契约与宿主分发 (f5d4da30)
- refactor(extensions): 移除 host 能力 JSON Schema 发布 (abbac3c6)
- refactor(wire): 统一跨线缆错误码为单点定义的 WireErrorCode (0bf9dda4)
- refactor(extensions): 统一宿主线缆契约与运行时边界 (50c599df)
- Refactor session context handling and tool execution (3cddb7a0)
- refactor(extensions): 统一扩展作者接口与宿主能力 (64635f22)

### 📝 Other

- Add extension gap analysis document comparing astrcodey and deepseek-harness capabilities (395437ca)
- Add in-memory transport and comprehensive tests for frame handling and handshake protocol (6a5036ce)
- clean (0b0461d9)
- all (808f0ad8)
- clean (df671e21)
- better (5851e2de)
- Update AstrCodey runtime and extension integration (3c0cbf39)

### Pull Requests

- #47

### Contributors

- @whatevertogo

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.15`
