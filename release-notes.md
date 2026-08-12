## Unreleased

### ⚠️ Breaking changes

- 扩展作者 API 已收敛为 `ExtensionCallContext`、专用 handler context、`Registrar` 和类型化
  `ExtensionHost` 领域客户端；旧 context、事件名与裸宿主服务入口不再保留兼容垫片。
- 删除 SDK capability 历史 helper 和未被授权流程使用的 `grant_name` 平行目录；能力线缆名
  只由 `ExtensionCapability::{as_str, parse}` 定义，operation 授权只由
  `HostOperationSpec.required` 定义。reserved prefix 与 session-control 子动作在所属边界解析。
  bundled session domain 类型不再从
  `astrcode_extension_sdk::session` 重复导出，统一从 `astrcode_extension_sdk::tool` 导入；
  S5R session DTO 仍保留在 `astrcode_extension_sdk::session`。
- S5R 协议升级到 3.0，不兼容 1.0/2.0 worker。磁盘扩展需要迁移握手 manifest、handler context、
  custom event 声明/订阅和 typed host API 后重新构建。
- custom-event capability 使用 `emit_custom_events` / `consume_custom_events`；模型客户端以
  `*_chat_events` 返回渐进式 `ModelStream`，以 `*_chat_collected` 明确返回完成后的最终内容和
  有序 chunks；删除名称与行为不一致的 `*_chat_stream`。
- `ExtensionHttpRoute` 回归纯 wire DTO；路由格式、匹配和冲突检测统一由注册边界执行，删除
  作者侧可绕开注册流程单独调用的 `validate()` 策略入口。

- memory 扩展的数据目录统一为 `~/.astrcode/extension_data/astrcode.memory/`，项目记忆
  放在其 `projects/<key>/` 子目录。不自动读取或迁移旧目录；升级前需手动备份并
  将需要的文件移到新目录。
- channels 扩展删除 Telegram 配置的 `workingDir`；由于配置严格拒绝未知字段，
  升级前必须删除该字段。工作目录统一由宿主按调用上下文归因。
- S5R 3.0 完成协议重命名：事件名 `extension_event` 改为 `custom_event`，
  capability `emit_events` / `consume_events` 改为 `emit_custom_events` /
  `consume_custom_events`，wire DTO 同步更名为 `ConversationDeltaDto::CustomEvent`、
  `ClientNotification::GlobalCustomEvent` 等。这是 breaking change，前端与 stdio
  客户端必须同步升级。
- S5R envelope、manifest、handler input 和全部 host-operation DTO 统一为 `snake_case`；
  原 custom-event / extension-HTTP manifest payload 的 `eventType`、`extensionId`、
  `pathParams` 等字段改为 `event_type`、`extension_id`、`path_params`。HTTP/前端 DTO 的
  `camelCase` 约定不变，server 在该边界显式映射。
- S5R wire 错误码在 3.0 冻结前收敛同义项：不支持的能力统一使用
  `unsupported`，无效调用者输入统一使用 `invalid_input`；不再生产
  `not_supported`、`invalid_parameter` 或 `invalid_arguments`。
- S5R session control 不再接受 worker 自报归因：`HostCreateSessionRequest` 删除
  `working_dir`、`tool_call_id`，`HostSubmitTurnRequest` 删除 `tool_call_id`；宿主从当前
  `parent_invoke_id` 对应的可信调用上下文注入 working directory 与 tool-call 归因。
- 事件日志不可降级：新版本写出的 `custom_event` durable 事件在旧二进制上 replay
  会失败。升级后如需回退，须先处理包含新事件的 session 日志。
- `TranscriptRewritten.source_fingerprint` 现在是必填持久化字段；缺少该字段的过渡期
  session 日志会在 replay 时被拒绝，不再跳过并发指纹校验。
- turn custom-event ingress 改为容量 256 的有界队列。异步 `emit` 会背压并等待发布回执；
  同步释放路径使用的 `try_emit` 在队列满时返回 `Full`。Session 生命周期事件（包括
  `SessionShutdown` 补偿）会在 30 秒预算内等待 extension runtime 稳定，补偿调用方记录
  超时并继续其余清理。
- S5R 3.0 握手只强制 `nested_invoke_v1`；`model_stream_v1` 与 `custom_event_v1` 改为按需
  协商。未协商的流式调用返回 `unsupported_feature`；声明 custom event 或
  subscription 却未协商 `custom_event_v1` 的 worker 在发布注册前被拒绝。

### ✨ Features

- bundled 与 S5R 扩展共用类型化 host 契约、稳定错误码和同一宿主分发目录。
- S5R manifest 中的 capability、tool mode、hook event/mode 和 handler id 均由
  contract enum/newtype 表达；未知值在握手边界直接拒绝。
- custom event 支持发布回执、持久化 consumer checkpoint、顺序重试、raw-event SSE 和
  authenticated consumer 管理接口。

### ⚡ Performance

- durable projection 的普通 append 批次不再每次深拷贝完整 read model；先做无副作用
  校验，event payload 在 prepare → journal → projection 间只移动不克隆；日志提交后
  仅在旧快照仍被持有时 copy-on-write。transcript rewrite 的提交前校验只维护
  system prompt 与 provider transcript，不再构建完整候选 read model。

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
