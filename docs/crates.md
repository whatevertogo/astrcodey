# Crate 快速参考

| Crate | 层 | 用途 | 关键类型 |
|-------|-----|------|---------|
| `astrcode-core` | 0 | 共享类型和 trait | SessionId, Tool, LlmProvider, EventStore, Config, Extension |
| `astrcode-support` | 0 | 宿主环境工具 | hostpaths, ShellFamily, persist_tool_result |
| `astrcode-ai` | 1 | LLM 提供者 | OpenAiProvider, RetryPolicy, CacheTracker, Utf8StreamDecoder |
| `astrcode-prompt` | 1 | Prompt 组装 | PromptContributor, PromptComposer, LayeredPromptBuilder, PromptTemplate |
| `astrcode-tools` | 1 | 内置工具 | ToolRegistry, ReadFileTool, ShellTool, SpawnTool |
| `astrcode-storage` | 1 | 会话持久化 | EventLog, FileSystemSessionRepository, FileConfigStore |
| `astrcode-context` | 1 | 上下文管理 | TokenUsageTracker, CompactConfig, ToolResultBudget, FileAccessTracker |
| `astrcode-extensions` | 2 | 扩展系统 | ExtensionRunner, ExtensionLoader, ExtensionContext |
| `astrcode-server` | 3 | 后端服务 | SessionManager, Agent, ConfigService, StdioTransport |
| `astrcode-protocol` | 3 | 通信协议 | ClientCommand, ServerEvent, JsonRpcMessage |
| `astrcode-client` | 4 | RPC 客户端 | AstrcodeClient, ClientTransport, ConversationStream |
| `astrcode-tui` | 4 | 终端 UI | AppController, CliState, Theme |
| `astrcode-exec` | 4 | 无头执行 | ExecConfig, execute() |
| `astrcode-cli` | 4 | CLI 入口 | Commands (Tui/Exec/Server/Version/Config) |

## 依赖方向

```
astrcode-cli → tui + exec + server
astrcode-tui → client + protocol
astrcode-exec → client + protocol
astrcode-client → protocol
astrcode-server → protocol + ai + prompt + tools + storage + context + extensions
astrcode-extensions → core
astrcode-ai/prompt/tools/storage/context → core + support
astrcode-support → core
astrcode-protocol → core
```
