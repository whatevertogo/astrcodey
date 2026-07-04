# AstrCode CLI

**English | [中文](#中文)**

AI-powered coding assistant CLI — your intelligent coding companion.

## Installation

```bash
npm i @whatevertogo/astrcode
```

Or install globally:

```bash
npm i -g @whatevertogo/astrcode
```

## Supported Platforms

- Linux (x64, ARM64)
- macOS (x64, ARM64 / Apple Silicon)
- Windows (x64, ARM64)

## Quick Start

After installation, run in your terminal:

```bash
astrcode
```

Before the first run, configure an LLM provider in `~/.astrcode/config.toml`. Legacy `config.json` files are still migrated automatically. See the [Configuration Guide](https://github.com/whatevertogo/astrcodey/blob/main/docs/configuration.md).

## Features

- **Multi-provider AI**: Anthropic, OpenAI-compatible, Google GenAI
- **Structured editing**: read, write, edit, patch tools
- **Code search**: glob, grep
- **Web tools**: built-in `web-search` and `fetch-url` (DuckDuckGo default; Brave/Serper optional)
- **Multiple frontends**: TUI, Web, Desktop GUI (Tauri)
- **Extension system**: plugins, MCP, Skills, disk s5r extensions
- **Session management**: event-sourcing architecture with fork, compact, and goal tracking

## More Information

- GitHub: https://github.com/whatevertogo/astrcodey
- Documentation: https://github.com/whatevertogo/astrcodey/tree/main/docs
- NPM: https://www.npmjs.com/package/@whatevertogo/astrcode

## License

AGPL-3.0

---

## 中文

AI 编程助手 CLI — 你的智能编程助手。

### 安装

```bash
npm i @whatevertogo/astrcode
```

### 快速开始

安装后在终端运行：

```bash
astrcode
```

首次运行前请在 `~/.astrcode/config.toml` 中配置 LLM Provider；旧版 `config.json` 仍会自动迁移。详见[配置指南](https://github.com/whatevertogo/astrcodey/blob/main/docs/configuration.md)。

### 功能特性

- **多 Provider AI**：Anthropic、OpenAI 兼容、Google GenAI
- **智能编辑**：read、write、edit、patch 等结构化工具
- **代码搜索**：glob、grep 快速定位代码
- **Web 工具**：内置 `web-search` 与 `fetch-url`（默认 DuckDuckGo；可选 Brave/Serper）
- **多种前端**：TUI、Web、Desktop GUI
- **扩展系统**：插件、MCP、Skills、s5r 磁盘扩展
- **会话管理**：Event Sourcing 架构，支持 fork、compact 和目标追踪

### 更多信息

- GitHub: https://github.com/whatevertogo/astrcodey
- 文档: https://github.com/whatevertogo/astrcodey/tree/main/docs
