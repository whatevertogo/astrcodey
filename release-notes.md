## v0.3.16

Released: 2026-08-24

### ✨ Features

- feat(worker): worker_prelude 再导出 WireErrorCode (f898d51f)
- feat: introduce hook registration modifiers for tool hooks (0cd77a17)
- feat(worker): Rust worker SDK 的 BackgroundHost 补 fork_root 委托 (5386485e)
- feat(sdk): Python SDK 同步 root 定制与 fork,并更新协议文档 (c6fc33c6)
- feat(extensions): root 会话创建支持定制字段并新增 session.root.fork (a89282a4)
- feat(docs): 添加请求改写链设计文档，定义统一链式原语及相关能力 (8f3b1e9d)
- feat: add workspace_sensitive_paths capability and hook priority support (79a273a3)
- feat(sdk): 补齐 Python SDK 的 HTTP route、shutdown hook 与 BackgroundHost (52e584fd)
- feat(extensions): 补齐后台驱动型扩展的宿主能力并新增类型化 hook 构造器 (efbec865)

### 🐛 Bug Fixes

- fix(sdk): 补齐 SessionRootFork 的进程内类型化客户端覆盖并修测试构造 (12e4eabb)
- fix(extensions): 跳过与全局目录相同 canonical 路径的项目扩展扫描 (cb23e959)

### 🔧 Refactors

- refactor(log): astrcode-log 直接依赖 astrcode-paths 而非 astrcode-core (c6b5777e)
- refactor(session): 谱系遍历与 session ID 校验移出 core 到各自消费方 (fc57f5eb)
- refactor(server): 配置解析与厂商预设目录迁入 server config_manager (d54bd3af)
- refactor(core): 扩展共享原语下沉到 astrcode-core,SDK 保留 re-export (32a28daf)
- refactor(context): token 估算模块从 core 迁到 astrcode-context (36449517)
- refactor(sdk): 收窄 SDK 作者面,wire DTO 走单一来源 (7a0470a4)
- refactor(s5r): S5R peer 运行时下沉为独立 astrcode-s5r-runtime crate (2cf1cd4c)
- refactor(paths): 引入 astrcode-paths 进程级路径原语 crate (0f24c2c8)
- refactor(commands): 统一斜杠命令契约与分发链路 (feeb6f18)
- refactor(sdk): host_operations 收拢为 worker 侧单拷贝 (dc74a3a8)
- refactor(sdk): 收敛 dispose_root 的重复调用-校验逻辑 (054b0fae)
- refactor(ai): 收敛 LLM 传输层重复实现与 crate 内部可见性 (2834d48b)

### 📝 Other

- Remove outdated performance baseline documentation for astrcode-storage. The document included benchmark results and analysis from Phase 0 and Phase 1, which are no longer relevant. Future measurements and conclusions regarding snapshot recovery and performance optimizations will be documented separately. (b5a7e7b6)

### Contributors

- @whatevertogo

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.16`
