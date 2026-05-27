# s5r 扩展线缆协议

> 与 `astrcode-extension-sdk` 中 `s5r::messages` 及 `runtime::Peer` 对齐。

## 传输

- **传输层**：子进程 **stdio**，长度前缀帧：`{payload_len}\n` + UTF-8 JSON body
- **编解码**：`metadata.wire_codec = "json"`（当前唯一实现）

## 握手方向

与旧 IPC（宿主先发 `extension/initialize`）不同，s5r 为：

1. **Worker（扩展子进程）** 发送 `Initialize`
2. **Host（AstrCode）** 回复 `Result`（`kind: initialize_result`）

扩展 manifest（`extension_id`、`tools`、`hooks`、`capabilities` 等）放在 `Initialize.metadata` 中；宿主在 `InitializeOutput.capabilities` 中返回已授权的 `astrcode.*` 能力描述。

## 线缆消息（`WireMessage`）

| `type` | 方向 | 说明 |
|--------|------|------|
| `initialize` | Worker → Host | 握手 + manifest |
| `result` | 双向 | `initialize_result` / `invoke_result` |
| `invoke` | 双向 | 能力调用；`stream: true` 时走事件流 |
| `event` | 响应方 → 调用方 | 流式阶段：`started` / `delta` / `completed` / `failed` |
| `cancel` | 调用方 → 响应方 | 取消进行中的 `invoke` |

## 能力命名

| 常量 | 用途 |
|------|------|
| `handler.invoke` | 宿主调用扩展注册的工具 / 命令 / 钩子 |
| `astrcode.*` | 扩展调用宿主（须在 manifest 中声明 capability） |

### 宿主 session 能力（摘要）

| wire 名 | 需声明 capability | 说明 |
|---------|-------------------|------|
| `astrcode.session.control.create` | `session_control` | 创建子 session |
| `astrcode.session.control.submit_turn` | `session_control` | 提交 turn；宿主经 `TurnScheduler` 路由（同 session 同步/后台、子 session untracked） |
| `astrcode.session.control.dispose` | `session_control` | 销毁 session |
| `astrcode.session.read_events` | `session_history` | 读 event log |

参数与返回值示例见 [extension-author-guide.md — 外置 agent-tool](extension-author-guide.md#外置-agent-tool-类插件)；调度语义见 [architecture.md §2](architecture.md#2-server-架构)。

## extension.json（发现阶段）

```json
{
  "protocol": { "s5r": "1.0" },
  "command": ["/path/to/extension-binary"]
}
```

## SDK 入口

| 侧 | Crate 模块 |
|----|------------|
| Host | `astrcode-extensions::s5r_ext` |
| Worker | `astrcode-extension-sdk::worker`（`Worker::run_stdio()`） |

插件作者入门：[extension-author-guide.md](extension-author-guide.md)

## 测试

```bash
cargo test -p astrcode-extensions --test s5r_e2e_test
```

参考 guest：`crates/astrcode-extensions/tests/s5r-guest/`
