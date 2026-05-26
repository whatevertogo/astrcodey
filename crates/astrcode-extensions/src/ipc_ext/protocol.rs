//! IPC 扩展 JSON-RPC 方法名与协议版本。

/// `extension.json` 中 `protocol.ipc` 的值。
pub const IPC_VERSION: &str = "1.0";

/// 宿主 → 子进程：握手，返回注册 manifest。
pub const METHOD_INITIALIZE: &str = "extension/initialize";

/// 宿主 → 子进程：调用 handler（工具 / 命令 / 钩子）。
pub const METHOD_HANDLER_INVOKE: &str = "extension/handler.invoke";

/// 子进程 → 宿主：调用 `astrcode.*` 能力（可重入）。
pub const METHOD_HOST_INVOKE: &str = "host/invoke";

/// 宿主 → 子进程：健康检查。
pub const METHOD_PING: &str = "extension/ping";

/// 宿主 → 子进程：优雅关闭通知（无响应）。
pub const METHOD_SHUTDOWN: &str = "extension/shutdown";
