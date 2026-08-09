# S5R 3.0 扩展线缆协议

> 可执行契约位于 `astrcode-extension-contract`。宿主与 worker 都直接依赖该 crate；
> worker 侧组装位于 `astrcode-extension-worker`，宿主不依赖 worker runtime。

S5R 3.0 不兼容 1.0/2.0 worker，也不提供双协议运行。旧 manifest 与 wire 数据在边界拒绝，
但宿主不会主动删除它们。

## 命名由来

**S5R** 是 **S.O.U.L.T.E.R Protocol** 的紧凑写法：保留首尾字母 `S`、`R`，
并以 `5` 代表中间的五个字母 `OULTE`。

## 传输与严格性

- 子进程 stdio；stdout 只允许协议帧，日志写 stderr。
- 帧为 `"<decimal length>\n<JSON>"`，长度是 JSON body 的字节数。
- frame body 硬上限 16 MiB，header 最多 32 bytes；空 header、符号、空格、超长和超限均拒绝。
- envelope 与具体 payload 都严格解析；除显式 `metadata` / `details` 外，未知字段被拒绝。
- bundled extension 不经过该编码层，直接使用 typed request/response。

## 初始化与 feature negotiation

宿主是连接发起方：

1. Host 发送唯一一条 `initialize`，携带 `protocol_version = "3.0"`、peer 信息、
   `supported_features`、`required_features`、宿主能力 catalog 和 metadata。
2. Worker 校验版本与 feature 集合，返回 `kind = "initialize"` 的 `result`；output 中包含
   worker 的 feature 集合、双方交集 `negotiated_features`、handler catalog 和 manifest metadata。
3. Host 复核交集、required 集合、身份、manifest 与 handler catalog 的完整性；完成后
   `Peer<Uninitialized>` 才转换为 `Peer<Ready>`。

required feature 必须同时出现在本方 supported 集合中。协商结果严格等于双方 supported 的交集，
并且必须覆盖双方 required 集合。首版 feature：

| feature | 语义 |
|---|---|
| `nested_invoke_v1` | handler 内发起的反向调用携带 `parent_invoke_id` |
| `model_stream_v1` | 增量 model stream 与唯一终态 |
| `custom_event_v1` | typed custom-event 注册、投递与错误语义 |

## 消息

所有消息使用 `type` tagged JSON envelope：

| `type` | 方向 | 说明 |
|---|---|---|
| `initialize` | Host → Worker | 单次初始化与 feature proposal |
| `result` | 双向 | `kind = initialize | invoke` 的成功或失败终态 |
| `invoke` | 双向 | operation、input、`stream` 和可选 `parent_invoke_id` |
| `stream` | 响应方 → 调用方 | 单个增量事件 |
| `cancel` | 调用方 → 响应方 | 按 request id 取消，附带非敏感 reason |

`invoke` request id 在同一 peer 上必须唯一。nested invoke 的 parent 必须仍是当前连接上的活跃
入站请求；未知 parent、未协商 stream/nested feature、重复 request id 都在边界拒绝。

## Streaming 与 cancel

stream buffer 为有界队列。事件必须先 `started`，随后可出现 retry/recovery、content/thinking
delta、tool-call、usage，最后恰好一个 `completed` 或 `failed`。终态后本地 stream fused 为
`None`。调用 future 或 stream 被 drop 会发送 `cancel`；取消后的迟到终态被有界墓碑吸收，
不会误杀 peer，真正未知 request 的 result/stream 仍是协议错误。

首版内部调优值为 stream buffer 32、peer command queue 256、背压 30 s、idle 120 s；
它们不是作者 API。

## Operation、错误与能力

| operation | 用途 |
|---|---|
| `s5r.runtime.ping` | peer runtime 内建 liveness round-trip |
| `handler.invoke` | 宿主调用 worker 注册的 tool、command、hook、HTTP 或 event handler |
| `astrcode.*` | worker 调用宿主能力；由 contract operation catalog 定义 |

宿主固定执行 capability → context/scope → backend 三段检查。`WireErrorCode` 字符串为
snake_case，废弃后不得复用；未知错误码必须在 `ErrorPayload.code: String` / `HostError.code`
中无损透传。generation gate 关闭后，排队和新调用统一返回 `extension_draining`。

## extension.json

```json
{
  "extension_id": "my-extension",
  "protocol": { "s5r": "3.0" },
  "command": ["/path/to/extension-binary"]
}
```

## Conformance

任意语言 worker 都可以运行同一套线缆验收：

```bash
cargo run -p astrcode-extension-contract --bin s5r-conformance -- -- <worker-command> [args...]
```

套件覆盖 initialize、unary、stream、cancel、nested invoke、未知错误码、clean shutdown，
以及畸形/超大帧拒绝。Rust 参考 guest 位于
`crates/astrcode-extensions/tests/s5r-guest/`。
