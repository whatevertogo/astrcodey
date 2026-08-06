# s5r 扩展线缆协议

> 与 `astrcode-extension-sdk` 中 `s5r::messages` 及 `runtime::Peer` 对齐。
>
> 当前版本：2.0。本版本将模型消息与网络 body 升级为 SDK 的类型化契约，
> 不兼容 1.0 worker；宿主只接受顶层 `protocol_version` 与
> `metadata.protocol.s5r` 均为 `"2.0"` 的握手。

## 命名由来

**s5r** 是 **S.O.U.L.T.E.R Protocol** 的紧凑写法：保留首尾字母 `S`、`R`，
并以 `5` 代表中间的五个字母 `OULTE`。这个名字致敬传奇 AstrBot 作者
**Soulter**。

## 传输

- **传输层**：子进程 **stdio**，长度前缀帧：`{payload_len}\n` + UTF-8 JSON body
- **编解码**：`metadata.wire_codec = "json"`（当前唯一实现）

## 握手方向

与旧 IPC（宿主先发 `extension/initialize`）不同，s5r 为：

1. **Worker（扩展子进程）** 发送 `Initialize`
2. **Host（AstrCode）** 回复 `Result`（`kind: initialize_result`）

扩展 manifest（`extension_id`、`tools`、`hooks`、`capabilities` 等）放在 `Initialize.metadata` 中；宿主在 `InitializeOutput.capabilities` 中返回已授权的 `astrcode.*` 能力描述。
其中 `extension_id` 必须与 `extension.json` 的权威发现期身份完全一致；宿主在启动进程前即用
该 ID 完成 enable/disable 与 replacement retirement 判定。

每条连接的每个入站方向只接受一次 `Initialize`。第一次尝试在进入 handler 前即占用握手机会，
无论成功或失败都不能在同一连接重试；重复请求返回 `duplicate_initialize`，重试必须建立新连接。
`Initialize`、`PeerInfo` 和 `HandlerDescriptor` 拒绝未知字段；`handlers` 必须与规范化后的
manifest 完整、精确一致。非法线缆对象会立即产生协议错误并关闭连接，不会被静默丢弃到
握手或调用超时。可扩展数据只能放在显式的 `metadata` 对象中。

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
| `s5r.runtime.ping` | Peer runtime 内建的 liveness round-trip；不依赖作者注册工具或 handler |
| `handler.invoke` | 宿主调用扩展注册的工具 / 命令 / 钩子 |
| `astrcode.*` | 扩展调用宿主（除默认 session state API 外，须在 manifest 中声明 capability） |

## Hook manifest

`metadata.hooks[]` 使用 `{ "on": "...", "mode": "blocking|non_blocking|advisory" }`。
`continue_after_stop` 是 typed decision hook，必须为 `blocking`，可通过
`options.max_per_turn` 声明每 turn 自动续跑上限；缺省与 `-1` 都表示不限制，非负数表示限制次数。

`user_message_envelope` 目前只支持进程内 Rust 扩展的 `Registrar` typed API，
不支持 s5r manifest 声明。

## extension.json（发现阶段）

```json
{
  "extension_id": "my-extension",
  "protocol": { "s5r": "2.0" },
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
