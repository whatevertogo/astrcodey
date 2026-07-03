# 配置指南

> 以当前代码为准（`astrcode-core::config`、`astrcode-storage::config_store`、`astrcode-server` 启动流程）。

AstrCode 默认使用 **TOML 配置文件 + 环境变量** 管理 LLM、运行时行为、权限与内置扩展参数。迁移期仍兼容旧的 `config.json`；当同目录同时存在 `config.toml` 和 `config.json` 时优先读取 TOML。首次从旧 JSON 读取主配置、项目覆盖或 last-known-good 快照时，会自动写出对应的 TOML 文件；旧 JSON 会保留为备份。所有用户可见字段使用 **camelCase**；未知字段会导致反序列化失败（`deny_unknown_fields`），拼写错误时错误信息会提示可能的 camelCase 写法。

---

## 1. 配置文件一览

| 文件 | 路径 | 结构类型 | 说明 |
|------|------|----------|------|
| 主配置 | `~/.astrcode/config.toml` | [`Config`] | LLM profile、runtime、permissions、`extensions` |
| 项目覆盖 | `<workspace>/.astrcode/config.toml` | [`ConfigOverlay`] | 仅写需覆盖的字段；**服务启动时**按启动工作目录合并进主配置 |
| 全局 MCP | `~/.astrcode/mcp.json` | `mcpServers` | MCP 客户端（与 `config.toml` 分离） |
| 项目 MCP | `<workspace>/.astrcode/mcp.json` | 同上 | 默认**不加载**；设置 `ASTRCODE_ENABLE_PROJECT_MCP=1` 后启用，同名 server 覆盖全局 |
| 上次可用快照 | `~/.astrcode/.last-known-good.toml` | `Config` | 解析成功时自动写入，启动失败时回退；旧 `.last-known-good.json` 仍可读取 |

[`Config`]: ../crates/astrcode-core/src/config/raw.rs
[`ConfigOverlay`]: ../crates/astrcode-core/src/config/raw.rs

**项目覆盖生效范围**：在 `astrcode-server` / CLI 启动时，对 **启动时的工作目录**（默认 `std::env::current_dir()`）读取 `.astrcode/config.toml` 并合并；旧 `.astrcode/config.json` 在 TOML 不存在时作为 fallback。之后在其他目录创建的 session 仍使用已合并后的全局有效配置；若需按仓库分别覆盖，请在该仓库目录下启动进程。

**热更新**：修改 `~/.astrcode/config.toml` 后可通过设置页或 `POST` 重载；已运行 session 的 per-session 快照需同步（服务端在重载后会调用 `sync_session_model_bindings`）。扩展通过 `on_config_changed()` 接收 `extensions` 段变更。

---

## 2. 主配置结构示例

以下为接近**内置默认值**的示例（首次运行若不存在 `config.toml` 会自动生成类似内容）：

```toml
version = "1"
activeProfile = "deepseek"
activeModel = "deepseek-v4-flash"

[runtime]
llmConnectTimeoutSecs = 10
llmReadTimeoutSecs = 90
llmMaxRetries = 2
llmRetryBaseDelayMs = 250
compactAutoEnabled = true
compactThresholdPercent = 83.5
compactKeepRecentTurns = 1
agentMaxDepth = 2
agentToolMaxParallelCalls = 5
shellTimeoutSecs = 120
approvalMode = "manual"

[runtime.extensionStates]
"astrcode.memory" = true

[permissions]
deny = []
ask = []
allow = []

[[profiles]]
name = "deepseek"
providerKind = "openai"
baseUrl = "https://api.deepseek.com"
apiKey = "env:DEEPSEEK_API_KEY"
wireFormat = "openai_chat_completions"
authScheme = "bearer"

[[profiles.models]]
id = "deepseek-v4-flash"
maxTokens = 393216
contextLimit = 1000000
modelOptions = { reasoning = true }

[extensions.astrcode-web-tools.search]
provider = "duckduckgo"
```

内置 **profiles**（未自定义时）：`deepseek`、`openai`、`anthropic`、`gemini`（`providerKind`: `google_genai`）。默认激活：`deepseek` / `deepseek-v4-flash`。完整默认常量见 [`defaults.rs`](../crates/astrcode-core/src/config/defaults.rs)。

---

## 3. 顶层字段

| 字段 | 类型 | 说明 |
|------|------|------|
| `version` | string | 配置格式版本，当前 `"1"` |
| `activeProfile` | string | 当前 profile 名称 |
| `activeModel` | string | 当前模型 `id`（须存在于该 profile 的 `models`） |
| `activeSmallProfile` | string? | 小模型 profile；与 `activeSmallModel` **同时**设置才生效 |
| `activeSmallModel` | string? | 小模型 id（memory 提取、部分扩展宿主能力） |
| `runtime` | object | 超时、compact、Agent、审批模式、扩展开关，见 §4 |
| `permissions` | object | Tool Gate DSL，见 §5 |
| `profiles` | array | LLM 提供者列表，见 §6 |
| `extensions` | object | 扩展 id → 扩展自定义配置，见 §8 |

---

## 4. `runtime` 字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `llmConnectTimeoutSecs` | `10` | LLM 连接超时（秒） |
| `llmReadTimeoutSecs` | `90` | LLM 读取超时（秒） |
| `llmMaxRetries` | `2` | 失败重试次数 |
| `llmRetryBaseDelayMs` | `250` | 指数退避基础延迟（毫秒） |
| `compactAutoEnabled` | `true` | 上下文占用超阈值时自动 compact |
| `compactThresholdPercent` | `83.5` | 触发自动 compact 的上下文占用百分比 |
| `compactMaxRetryAttempts` | `3` | compact LLM 调用最大重试 |
| `compactMaxOutputTokens` | `20000` | compact 摘要最大输出 token |
| `compactKeepRecentTurns` | `1` | 自动/反应式 compact 保留的最近完整 user turn 数；省略该字段使用默认语义 |
| `compactCircuitBreakerThreshold` | `3` | 连续 compact 失败后暂停自动 compact |
| `compactCircuitBreakerCooldownSecs` | `60` | 熔断冷却时间（秒） |
| `predictiveCompactEnabled` | `false` | 预测性 compact（当前轮可能溢出前提前压缩） |
| `predictiveCompactBaselineGrowthTokens` | `15000` | 预测增长 token 保底值 |
| `postCompactMaxFiles` | `5` | compact 后恢复的文件数上限 |
| `postCompactTokenBudget` | `50000` | 恢复文件总 token 预算 |
| `postCompactMaxTokensPerFile` | `5000` | 单文件恢复 token 上限 |
| `agentMaxDepth` | `2` | 子 Agent 最大嵌套深度（root=0） |
| `agentToolMaxParallelCalls` | `5` | 单轮并行工具调用上限 |
| `shellTimeoutSecs` | `120` | Shell 工具默认超时（秒）；LLM 可传更短值，上限 600 |
| `allowApiKeyShellCommand` | `false` | 是否允许 `apiKey` 使用 `!command` 从 shell 读取密钥 |
| `approvalMode` | `"manual"` | **全局**审批模式：`"manual"` 需确认；`"yolo"` 跳过 Ask。对所有 session 生效（每轮 turn 从有效配置读取，非「每个 session 单独记忆」）。Web 设置页保存后写入本字段。CLI/TUI 进程内启动时，若未设置此项则**默认 yolo**；`astrcode tui --manual` / `--yolo` 可强制覆盖。HTTP `server` 子命令未设置时仍为 `manual`。 |
| `extensionStates` | `{}` | 扩展启停，见 §7 |

---

## 5. `permissions`（Tool Gate）

```toml
[permissions]
deny = [{ tool = "shell", pattern = "rm -rf *" }]
ask = [{ tool = "write", path = "/etc/**" }]
allow = [{ tool = "read" }]
```

| 字段 | 说明 |
|------|------|
| `deny` / `ask` / `allow` | 规则数组，按链优先级评估（实现见 `astrcode-core::permission`） |
| 规则.`tool` | 工具名 |
| 规则.`pattern` | 可选，匹配工具输入 |
| 规则.`path` | 可选，路径相关工具 |

未匹配任何 allow 且非显式 allow 时 **fail-closed**（拒绝）。

---

## 6. Profile 与模型

### 6.1 Profile

| 字段 | 说明 |
|------|------|
| `name` | 唯一标识，供 `activeProfile` 引用 |
| `providerKind` | 展示/分组用 provider 家族，如 `openai`、`anthropic`、`google_genai` |
| `wireFormat` | 实际协议格式：`openai_chat_completions`、`openai_responses`、`anthropic_messages`、`google_genai` |
| `authScheme` | API key 鉴权方式：`bearer`、`x_api_key`、`x_goog_api_key`、`none` |
| `baseUrl` | API 根 URL；Anthropic 若 URL 无 `/v1` 段会自动补全 |
| `apiKey` | 见 §6.3；可省略，则按 profile 名回退已知环境变量 |
| `models` | 模型列表 |
| `capabilities` | 可选：`supportsPromptCacheKey`、`promptCacheRetention`（`inMemory` / `24h`）、`supportsStreamUsage` |

### 6.2 模型

| 字段 | 说明 |
|------|------|
| `id` | 模型标识 |
| `maxTokens` | 最大输出 token（缺省解析为 `8192`） |
| `contextLimit` | 上下文窗口（缺省 `65536`） |
| `modelOptions.reasoning` | 是否启用推理模式（provider 相关） |
| `modelOptions.thinkingLevel` | `low` / `medium` / `high`（OpenAI Responses `reasoning.effort`） |

### 6.3 API Key 解析

| 写法 | 行为 |
|------|------|
| `"env:VAR_NAME"` | 读取环境变量，缺失则启动解析失败 |
| `"!command"` | 执行 shell，stdout（trim）为 key；需 `runtime.allowApiKeyShellCommand = true` |
| 全大写 `VAR_NAME` | 尝试作环境变量；未设置则 **警告** 并把字符串当作明文 key |
| 其他字符串 | 明文 key（不推荐提交到仓库） |

按 profile 名的环境变量回退：`openai` → `OPENAI_API_KEY`，`deepseek` → `DEEPSEEK_API_KEY`，`anthropic` → `ANTHROPIC_API_KEY`，`gemini` → `GOOGLE_API_KEY` 或 `GEMINI_API_KEY`。

### 6.4 小模型

同时设置 `activeSmallProfile` 与 `activeSmallModel` 时，扩展宿主能力（如 `small_model`）使用独立 provider；否则与主模型相同。

---

## 7. 扩展启停（`runtime.extensionStates`）

| 扩展 ID | 默认 | 说明 |
|---------|------|------|
| `astrcode-agent-tools` | 启用 | 子 Agent 工具 |
| `astrcode-mcp` | 启用 | MCP 客户端 |
| `astrcode-skill` | 启用 | Skill 斜杠命令 |
| `astrcode-todo-tool` | 启用 | Todo 工具 |
| `astrcode-mode` | 启用 | Code / Plan 模式 |
| `astrcode-goal` | 启用 | session goal 与自动续跑 |
| `astrcode-web-tools` | 启用 | `web-search` / `fetch-url` |
| `astrcode.memory` | **关闭** | 项目记忆 |
| `astrcode-channels` | **关闭** | Telegram 通道 |

```toml
[runtime.extensionStates]
"astrcode.memory" = true
"astrcode-mode" = false
```

显式 `false` / `true` 覆盖默认策略。与 [`extension-system.md`](extension-system.md) 中的内置扩展表一致。

---

## 8. `extensions` 段（按扩展 id）

键为扩展 ID，值为扩展自行反序列化的配置值（`ExtensionCtx::config.deserialize()`）。TOML 中无法表达 `null`，需要“未设置”语义时请省略字段。

### 8.1 `astrcode.memory`

| 字段 | 默认 | 说明 |
|------|------|------|
| `maxContexts` | `10` | 每作用域索引记录上限 |
| `autoExtract` | `true` | Session 启动时自动提取 |
| `autoExtractAfterSave` | `true` | `memory_save` 后后台同步变更 session |
| `maxChangedSessions` | `5` | 单次 pipeline 最多处理 session 数 |
| `minConversationChars` | `200` | 短于此次数的会话跳过提取 |
| `maxContextAgeDays` | `90` | `contexts/` 文件保留天数 |
| `injectProjectMemoriesPerTurn` | `true` | turn 末排名，下轮首包注入 |
| `maxInjectedProjectMemories` | `5` | 每轮最多注入条数 |
| `minProjectMemoryScore` | `0.35` | 注入最低相关分（0–1） |
| `maxInjectedMemoryChars` | `1500` | 注入块总字符上限 |
| `minRecallQueryChars` | `12` | 过短 exchange 跳过 turn 末召回 |

**数据目录**（与 config 分离）：

- 用户偏好：`~/.astrcode/memory/`（`user_pref` 类别）
- 项目记忆：`~/.astrcode/projects/<project_key>/extension_data/astrcode.memory/`

### 8.2 `astrcode-web-tools`

| 工具 | 名称 |
|------|------|
| 网页搜索 | `web-search` |
| URL 抓取 | `fetch-url` |

**`search`**

| 字段 | 默认 | 说明 |
|------|------|------|
| `provider` | `duckduckgo` | `duckduckgo` / `brave` / `serper` |
| `braveApiKey` / `braveApiKeyEnv` | — | Brave 密钥或环境变量名 |
| `serperApiKey` / `serperApiKeyEnv` | — | Serper 同上 |
| `defaultMaxResults` | `5` | 默认条数 |
| `requestTimeoutSecs` | `30` | 请求超时 |

**`fetch`**

| 字段 | 默认 | 说明 |
|------|------|------|
| `requestTimeoutSecs` | `60` | 请求超时 |
| `maxContentBytes` | `10485760` | 响应体上限 |
| `maxOutputChars` | `100000` | 返回给模型的字符上限 |
| `userAgent` | AstrCode 默认 UA | HTTP User-Agent |
| `cacheTtlSecs` | `900` | 缓存 TTL |
| `cacheMaxEntries` | `64` | 缓存条目数 |
| `cacheMaxBytes` | `52428800` | 缓存总字节 |
| `maxRedirects` | `10` | 最大重定向 |

`fetch-url` 阻止 localhost 与私网地址（SSRF 防护）。

### 8.3 `astrcode-channels`

```toml
[extensions.astrcode-channels.telegram]
enabled = true
botTokenEnv = "TELEGRAM_BOT_TOKEN"
allowedChatIds = ["123456789"]
allowAllChats = false
registerCommands = false
streaming = false
workingDir = "D:/my-project"
requestTimeoutSecs = 30
pollTimeoutSecs = 25
maxReplyChars = 3500
```

未设置 `allowAllChats: true` 时应配置 `allowedChatIds` 白名单。`botToken` 可直接写 token，更推荐 `botTokenEnv`。

### 8.4 MCP（**不在** `extensions` 内）

MCP 服务器仅通过 `mcp.json` 配置，由 `astrcode-mcp` 扩展加载。

**Stdio：**

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/allowed/path"],
      "env": {},
      "cwd": "."
    }
  }
}
```

**HTTP：**

```json
{
  "mcpServers": {
    "remote": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer TOKEN" }
    }
  }
}
```

`type` 省略时视为 `stdio`。项目级 `cwd` 必须在 workspace 内，否则该 server 被跳过并写入诊断信息。

---

## 9. 项目级覆盖（`ConfigOverlay`）

`<workspace>/.astrcode/config.toml` **只需包含要改的字段**：

```toml
activeProfile = "openai"
activeModel = "gpt-4.1"

[runtime]
llmMaxRetries = 5

[runtime.extensionStates]
"astrcode.memory" = true

[extensions.astrcode-web-tools.search]
provider = "brave"
braveApiKeyEnv = "BRAVE_API_KEY"
```

| 覆盖字段 | 合并方式 |
|----------|----------|
| `activeProfile` / `activeModel` / `activeSmall*` | 替换 |
| `profiles` | **整体替换**列表（非按 name 合并） |
| `runtime` | **按字段**合并；`extensionStates` 同 key 覆盖 |
| `permissions` | **整体替换** |
| `extensions` | 同扩展 id：双方均为 object 时**递归合并**配置字段；类型冲突时以覆盖层为准；异 id 保留 |

---

## 10. 解析与校验

- `Config::effective_from()` 要求 `activeProfile` / `activeModel` 存在且可解析 API key。
- 配置文件含未知字段 → **加载失败**（不自动覆盖你的文件）；修正字段名或删除废弃键后重试。成功加载时可能回写 `config.toml` 以补齐新版本字段（不删除已有自定义段）。
- 解析失败时服务尝试 `.last-known-good.toml`，再 fallback 到旧 `.last-known-good.json`；仍失败则使用 dummy LLM（HTTP 仍可用，但无法对话直至修复配置）。

实现入口：[`resolve.rs`](../crates/astrcode-core/src/config/resolve.rs)、启动 [`bootstrap/mod.rs`](../crates/astrcode-server/src/bootstrap/mod.rs)。

---

## 11. 相关文档

- [扩展系统](extension-system.md)
- [s5r 协议](s5r-protocol.md)（磁盘扩展）
- [扩展作者指南](extension-author-guide.md)
