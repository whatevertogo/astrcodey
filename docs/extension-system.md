# AstrCode 扩展系统

> 以当前代码为准（`astrcode-core`、`astrcode-extension-sdk`、`astrcode-extensions`、`astrcode-server`）。

---

## 1. 概览

| 层级 | 实现 | 说明 |
|------|------|------|
| **内置扩展** | `astrcode-bundled-extensions` + 各 `astrcode-extension-*` | 进程内 Rust，`ExtensionHostServices` 满能力 |
| **磁盘扩展** | IPC 子进程 | `~/.astrcode/extensions/`、`<project>/.astrcode/extensions/` |
| **外部工具** | `astrcode-extension-mcp` | MCP 子进程/HTTP，**不**实现 `Extension` trait |

磁盘扩展只有一种加载方式：**stdio JSON-RPC / JSONL**（`protocol.ipc`）。

---

## 2. 代码地图

| 模块 | 职责 |
|------|------|
| `astrcode-core::extension` | `Extension` trait、能力、钩子、Registrar |
| `astrcode-extensions::loader` | 发现 `extension.json`、启动 IPC 子进程 |
| `astrcode-extensions::ipc_ext` | `IpcExtension`、JSON-RPC 会话、`host/invoke` |
| `astrcode-extensions::host_router` | 唯一 `astrcode.*` 宿主能力实现 |
| `astrcode-extensions::remote_manifest` | manifest 构建、HandlerResult 解析 |
| `astrcode-extension-sdk::s5r` | 共享类型：`HandlerResult`、事件名、能力 wire 名 |
| `astrcode-protocol::framing` | JSON-RPC 2.0 / JSONL 帧 |

参考实现：`crates/astrcode-extensions/tests/ipc-guest/`  
E2E：`cargo test -p astrcode-extensions --test ipc_e2e_test`

---

## 3. 内置扩展（进程内）

实现 `Extension` trait，在 `start()` 时通过 `ExtensionCtx` 获取配置与 `host_services`。

`ExtensionCapability` 控制宿主注入的敏感能力（`session_control`、`workspace_read` 等）。

---

## 4. 磁盘 IPC 扩展

### 4.1 目录布局

```
~/.astrcode/extensions/my-ext/
  extension.json
  main.js

<project>/.astrcode/extensions/my-ext/
  extension.json
  ...
```

### 4.2 extension.json

| 字段 | 必填 | 说明 |
|------|------|------|
| `protocol.ipc` | 是 | `"1.0"` |
| `command` | 是 | 字符串数组：`[可执行文件, ...参数]` |
| `env` | 否 | 额外环境变量 |

```json
{
  "protocol": { "ipc": "1.0" },
  "command": ["node", "dist/index.js"]
}
```

### 4.3 IPC 协议

| 方法 | 方向 | 说明 |
|------|------|------|
| `extension/initialize` | 宿主 → 子进程 | 返回注册 manifest |
| `extension/handler.invoke` | 宿主 → 子进程 | 工具 / 命令 / 钩子 |
| `host/invoke` | 子进程 → 宿主 | `astrcode.*` |
| `extension/ping` | 宿主 → 子进程 | 健康检查 |
| `extension/shutdown` | 宿主 → 子进程 | 关闭 |

---

## 5. 宿主能力

见 `HostRouter`；子进程经 `host/invoke` 调用，须在 initialize manifest 中声明 capability。

---

## 6. 编写插件

见 `tests/ipc-guest/src/main.rs` 与 `ipc_e2e_test.rs`。
