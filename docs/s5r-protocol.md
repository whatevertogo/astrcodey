# S5R 3.0 扩展线缆协议

> 可执行契约位于 `astrcode-extension-contract`。宿主与 worker 都直接依赖该 crate；
> worker 侧组装位于 `astrcode-extension-worker`，宿主不依赖 worker runtime。

S5R 3.0 不兼容 1.0/2.0 worker，也不提供双协议运行。旧 manifest 与 wire 数据在边界拒绝，
但宿主不会主动删除它们。

## 命名由来

**S5R** 是 **S.O.U.L.T.E.R Protocol** 的紧凑写法：保留首尾字母 `S`、`R`，
并以 `5` 代表中间的五个字母 `OULTE`。

## 传输与严格性

- 当前实现只有子进程 stdio；stdout 只允许协议帧，日志写 stderr。DTO、校验和类型状态机不绑定
  stdio；未来即使由 Worker 建立 WebSocket/反向连接，连接建立后仍由 Host 发送 Initialize。
- 帧为 `"<decimal length>\n<JSON>"`，长度是 JSON body 的字节数。
- frame body 硬上限 16 MiB，header 最多 32 bytes；空 header、符号、空格、超长和超限均拒绝。
- envelope 与具体 payload 都严格解析；未知字段被拒绝。`InitializeManifest` 是 initialize
  output 的严格类型字段，不经过通用 `metadata`。只有明确声明为任意 JSON 的业务 input、
  output 与错误 `details` 不由 envelope 解析器限制。
- envelope、manifest、handler input 与 `astrcode.*` DTO 的字段名统一使用 `snake_case`，
  不接受 `camelCase` alias；HTTP/前端 DTO 在各自边界单独映射。
- bundled extension 不经过该编码层，直接使用 typed request/response。

## 初始化与 feature negotiation

宿主是连接发起方：

1. Host 发送唯一一条 `initialize`，携带 `protocol_version = "3.0"`、Host 身份、预期
   `extension_id`、feature 集合和完整 `host_operations` 支持目录。
2. Worker 校验版本与 feature 集合，返回 `kind = "initialize"` 的 `result`；output 中包含
   Worker 身份、feature 集合、双方交集 `negotiated_features` 和严格类型的 `manifest`。
   工具、hook、command 等声明只存在 manifest 中，不再发送通用 handler catalog。
3. Host 复核 feature、身份与 manifest，完成 Registrar 校验及跨扩展冲突检查。此时双方仅为
   `Initialized`，不能创建 runtime 或处理业务消息。
4. Host 发送唯一一条 `activate`；Worker 完成空 `kind = "activate"` 成功响应后，双方进入
   `Ready`，随后才启动 driver 并允许双向 `invoke`。

required feature 必须同时出现在本方 supported 集合中。协商结果严格等于双方 supported 的交集，
并且必须覆盖双方 required 集合。首版 feature：

| feature | 语义 |
|---|---|
| `nested_invoke_v1` | handler 内发起的反向调用携带 `parent_invoke_id` |
| `model_stream_v1` | 增量 model stream 与唯一终态 |
| `custom_event_v1` | typed custom-event 注册、投递与错误语义 |

当前宿主把 `nested_invoke_v1` 设为 required；worker 不额外声明 required feature。
`model_stream_v1` 在流式调用时校验，缺失时返回 `unsupported_feature`。声明 custom
event 或 subscription 的 manifest 必须协商 `custom_event_v1`；不使用这两类能力的
worker 不会因未协商它们而初始化失败。

## 消息

所有消息使用 `type` tagged JSON envelope：

| `type` | 方向 | 说明 |
|---|---|---|
| `initialize` | Host → Worker | 单次初始化与 feature proposal |
| `activate` | Host → Worker | 注册验证完成后的单次激活 |
| `result` | 双向 | `kind = initialize | activate | invoke` 的成功或失败终态 |
| `invoke` | 双向 | operation、input、`stream` 和可选 `parent_invoke_id` |
| `stream` | 响应方 → 调用方 | 单个增量事件 |
| `cancel` | 调用方 → 响应方 | 按 request id 取消，附带非敏感 reason |

`result` 是以 `status = success | failure` 标记的闭合枚举：success 必须携带
`output`，failure 必须携带结构化 `error`，不存在两者同时缺失或同时出现的状态。
handler 返回的 effect 也是 contract enum，未知或与 handler 类别不匹配的 effect
在宿主边界拒绝。

`invoke` request id 在同一 peer 上必须唯一。nested invoke 的 parent 必须仍是当前连接上的活跃
入站请求；未知 parent、未协商 stream/nested feature、重复 request id 都在边界拒绝。

## Streaming 与 cancel

stream buffer 为有界队列。事件必须先 `started`，随后可出现 retry/recovery、content/thinking
delta、tool-call、usage，最后恰好一个 `completed` 或 `failed`。终态后本地 stream fused 为
`None`。调用 future 或 stream 被 drop 会发送 `cancel`；取消后的迟到终态被有界墓碑吸收，
不会误杀 peer，真正未知 request 的 result/stream 仍是协议错误。

`cancel.reason` 是调用方生成的稳定、非敏感原因标识。Worker 可在
`WorkerInvocationContext::cancel_token().reason()` 读取首个取消原因；后续连接关闭等清理不会
覆盖它。reason 用于诊断与选择安全的提前退出路径，不是授权信息，也不应携带用户输入。

握手后的所有帧由单一 FIFO WritePump 写入；协议 driver 不等待物理写入，因此慢写不会阻塞读取
和取消状态推进。内部 `written` 回执只表示对应帧已完整写出；caller 生命周期由独立取消信号表达。

首版内部调优值为 stream buffer 32、peer command/write queue 256、背压 30 s、idle 120 s；
它们不是作者 API。

## Operation、错误与能力

| operation | 用途 |
|---|---|
| `s5r.runtime.ping` | peer runtime 内建 liveness round-trip |
| `handler.invoke` | 宿主调用 worker 注册的 tool、command、hook、HTTP 或 event handler |
| `astrcode.*` | worker 调用宿主能力；由 contract operation catalog 定义 |

`host_operations` 是去重的 operation 名字符串列表，只表示该 Host 版本实现了哪些 operation。
operation 的类型化参数、返回值、能力要求和 stream/cancel 属性由共享 contract 定义，不在握手中
重复传输。Worker 可通过
`HostClient::host_supports(HostOperation)` 查询，typed client 也会在缺失时本地返回
`unsupported`；返回 `true` 不代表 Worker 已获授权、当前 session/workspace context 存在，
也不代表具体 backend 可用。宿主仍固定执行 manifest capability → runtime context/scope →
backend 检查。`WireErrorCode` 单点定义在
`astrcode-extension-contract`，字符串为
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
cargo run -p astrcode-extension-contract --bin s5r-conformance -- \
  --extension-id <worker-extension-id> -- <worker-command> [args...]
```

`--extension-id` 是 Host 在 Initialize 中指定的权威预期身份。套件覆盖 initialize、activate、
unary、stream、cancel、nested invoke、未知错误码、clean shutdown，
以及畸形/超大帧拒绝。Rust 参考 guest 位于
`crates/astrcode-extensions/tests/s5r-guest/`。
