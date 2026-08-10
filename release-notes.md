## Unreleased

### ⚠️ Breaking changes

- 扩展作者 API 已收敛为 `ExtensionCallContext`、专用 handler context、`Registrar` 和类型化
  `ExtensionHost` 领域客户端；旧 context、事件名与裸宿主服务入口不再保留兼容垫片。
- 删除 SDK capability 历史 helper；改用 `ExtensionCapability::{as_str, parse, grant_name}`，
  reserved prefix 与 session-control 子动作在所属边界解析。bundled session domain 类型不再从
  `astrcode_extension_sdk::session` 重复导出，统一从 `astrcode_extension_sdk::tool` 导入；
  S5R session DTO 仍保留在 `astrcode_extension_sdk::session`。
- S5R 协议升级到 3.0，不兼容 1.0/2.0 worker。磁盘扩展需要迁移握手 manifest、handler context、
  custom event 声明/订阅和 typed host API 后重新构建。
- custom-event capability 使用 `emit_custom_events` / `consume_custom_events`；模型
  `*_chat_stream` 当前在完成后返回最终内容与有序 collected chunks，不是渐进式 Rust stream。

- memory 扩展不再读取旧数据目录 `~/.astrcode/memory/` 和
  `~/.astrcode/projects/<key>/extension_data/astrcode.memory/`。新目录统一为
  `~/.astrcode/extension_data/astrcode.memory/`，项目记忆放在其 `projects/<key>/`
  子目录。这是刻意的不兼容变更，没有旧目录双读或自动迁移；如需保留尚未迁移的
  数据，升级前手动复制到新目录。

### ✨ Features

- bundled 与 S5R 扩展共用类型化 host 契约、稳定错误码和同一宿主分发目录。
- custom event 支持发布回执、持久化 consumer checkpoint、顺序重试、raw-event SSE 和
  authenticated consumer 管理接口。

## v0.3.14

Released: 2026-08-10

### ✨ Features

- feat(extensions): 支持回收会话查询与重新激活 (0844d0f1)
- feat: 添加 assistantRunCompletedReply 函数并更新相关组件以支持会话分叉功能 (b1356b64)
- feat(extensions): 加固扩展运行时生命周期与并发调度 (e7d2f781)

### 🐛 Bug Fixes

- fix(llm): 安全重试中断的流式响应 (2b935f63)
- fix: update rand import to use RngExt for improved functionality (c5505ff6)

### Contributors

- @whatevertogo

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.14`
