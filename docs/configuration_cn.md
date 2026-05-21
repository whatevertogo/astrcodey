# 配置指南

AstrCode 使用分层配置系统，支持 JSON 配置文件和环境变量。配置存储在 `~/.astrcode/config.json`，并支持通过项目级 `.astrcode/config.json` 覆盖全局配置。

## 配置文件位置

- **全局配置**: `~/.astrcode/config.json`
- **项目覆盖**: `<project>/.astrcode/config.json`
- **扩展数据**: `~/.astrcode/projects/<project_key>/extension_data/<extension-id>/`（按项目隔离）

## 配置结构

runtime 里面 null 表示默认值

```json
{
  "version": "1",
  "activeProfile": "deepseek",
  "activeModel": "deepseek-v4-flash",
  "activeSmallProfile": "deepseek",
  "activeSmallModel": "deepseek-v4-flash",
  "runtime": {
    "llmConnectTimeoutSecs": 60,
    "llmReadTimeoutSecs": 120,
    "llmMaxRetries": 3,
    "llmTemperature": 0.7,
    "compactAutoEnabled": true,
    "compactThresholdPercent": 83.5,
    "agentMaxDepth": 3,
    "agentToolMaxParallelCalls": 5
  },
  "profiles": [
    {
      "name": "openai",
      "providerKind": "openai",
      "baseUrl": "https://api.openai.com/v1",
      "apiKey": "${OPENAI_API_KEY}",
      "models": [
        {
          "id": "gpt-4o",
          "maxTokens": 128000,
          "contextLimit": 128000,
          "reasoning": false
        }
      ],
      "apiMode": "chat_completions"
    }
  ]
}
```

## 配置字段说明

### 顶层字段

| 字段 | 类型 | 说明 |
|-----|------|------|
| `version` | string | 配置文件格式版本（当前为 "1"） |
| `activeProfile` | string | 当前激活的 LLM 配置文件名称 |
| `activeModel` | string | 当前使用的模型 ID |
| `activeSmallProfile` | string (可选) | 小模型配置文件名（用于记忆扩展等） |
| `activeSmallModel` | string (可选) | 小模型 ID |
| `runtime` | object | 运行时行为设置（见下文） |
| `profiles` | array | 可用的 LLM 提供者配置列表 |

### 配置文件字段

`profiles` 数组中的每个配置代表一个 LLM 提供者：

| 字段 | 类型 | 说明 |
|-----|------|------|
| `name` | string | 配置文件标识符（被 `activeProfile` 引用） |
| `providerKind` | string | 提供者类型：`openai`、`anthropic`、`google` |
| `baseUrl` | string | API 端点 URL |
| `apiKey` | string | API 密钥或环境变量引用（如 `${OPENAI_API_KEY}`） |
| `models` | array | 该配置文件可用的模型列表 |
| `apiMode` | string | API 模式：`chat_completions` 或 `responses` |

### 模型字段

`models` 数组中的每个模型：

| 字段 | 类型 | 说明 |
|-----|------|------|
| `id` | string | 模型标识符 |
| `maxTokens` | number | 最大输出 token 数 |
| `contextLimit` | number | 上下文窗口大小 |
| `reasoning` | boolean | 是否支持扩展推理 |
| `reasoningSplit` | boolean | 是否请求分离的推理/思考字段 |

### 运行时字段

| 字段 | 类型 | 默认值 | 说明 |
|-----|------|--------|------|
| `llmConnectTimeoutSecs` | number | 60 | LLM 连接超时时间（秒） |
| `llmReadTimeoutSecs` | number | 120 | LLM 读取超时时间（秒） |
| `llmMaxRetries` | number | 3 | 失败请求的最大重试次数 |
| `llmRetryBaseDelayMs` | number | 500 | 指数退避的基础延迟（毫秒） |
| `llmTemperature` | number | null | 采样温度（0.0-2.0），null 使用 API 默认值 |
| `compactAutoEnabled` | boolean | true | 是否启用自动上下文压缩 |
| `compactThresholdPercent` | number | 83.5 | 上下文占用超过此百分比时触发自动压缩 |
| `compactMaxRetryAttempts` | number | 3 | 压缩失败的最大重试次数 |
| `compactMaxOutputTokens` | number | 8000 | LLM 压缩输出的最大 token 数 |
| `postCompactMaxFiles` | number | 10 | 压缩后恢复的最近文件数量上限 |
| `postCompactTokenBudget` | number | 16000 | 文件恢复的总 token 预算 |
| `postCompactMaxTokensPerFile` | number | 4000 | 单个恢复文件的最大 token 数 |
| `agentMaxDepth` | number | 3 | 子 agent 最大嵌套深度（root=0, child=1, ...） |
| `agentToolMaxParallelCalls` | number | 5 | 单轮中允许的最大并行工具调用数 |
| `wasmFuel` | number | 100000000 | WASM 扩展的 fuel 限制（指令数） |
| `wasmMemoryMb` | number | 128 | WASM 扩展的内存限制（MB） |

## 环境变量

API 密钥可以使用 `${VARIABLE_NAME}` 语法在 `apiKey` 字段中引用。系统将在运行时从环境中解析这些变量。

支持的环境变量：
- `OPENAI_API_KEY` - OpenAI API 密钥
- `ANTHROPIC_API_KEY` - Anthropic API 密钥
- `GOOGLE_API_KEY` - Google API 密钥
- 配置中引用的任何自定义变量

## 小模型配置

某些扩展（如 `astrcode.memory`）需要小模型进行高效处理。通过设置 `activeSmallProfile` 和 `activeSmallModel` 来配置：

```json
{
  "activeProfile": "openai",
  "activeModel": "gpt-4o",
  "activeSmallProfile": "openai",
  "activeSmallModel": "gpt-4o-mini"
}
```

如果未设置 `activeSmallProfile`，小模型将回退到主模型配置。

## 项目级覆盖

在项目目录中创建 `.astrcode/config.json` 来覆盖全局设置：

```json
{
  "activeProfile": "project-specific",
  "activeModel": "custom-model",
  "runtime": {
    "llmTemperature": 0.5
  }
}
```

项目覆盖会与全局配置合并，项目值优先级更高。

## 扩展配置

扩展可以**按项目**在 `~/.astrcode/projects/<project_key>/extension_data/<extension-id>/` 中存储自己的数据。

例如，记忆扩展为每个项目分别存储：
- `MEMORY.md` - 干净的 markdown 文件，包含持久化记忆（项目级）
- `contexts/` - 从历史会话中提取的上下文文件（项目级）
- `processed_sessions.json` - 跟踪已处理的会话（项目级）

**注意**：记忆和其他扩展数据现在按项目隔离。每个项目都有自己独立的记忆存储。

## 默认值

所有配置字段都在 [`crates/astrcode-core/src/config/defaults.rs`](../crates/astrcode-core/src/config/defaults.rs) 中定义了合理的默认值。缺失的字段将自动填充为这些默认值。

## 配置热重载

配置更改对新会话立即生效。现有会话继续使用其原始配置。使用 `/new` 命令启动一个使用更新配置的新会话。

## 验证

配置系统会验证：
- 必需字段存在
- 配置文件和模型引用有效
- 数值在可接受范围内
- 环境变量可以解析

无效的配置将导致 AstrCode 启动失败，并显示描述性错误消息。
