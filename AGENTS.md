## 总原则

只做最小、正确、可维护的 Rust 修改。  
不要主动重构、泛化、加层、造框架，除非任务明确要求。
不要把项目搞得一堆石山，把代码放在该放的位置，保持清晰的模块边界和职责划分。

## 修改前

- 先读所属模块、调用点、测试和现有命名风格。
- 修根因，不修表象。
- 默认不要新增文件、trait、DTO、依赖、配置项或公开 API。
- 不确定时继续读代码，不要脑补抽象。

## 架构边界

PROJECT_ARCHITECTURE.md 描述了 astrcode 的目标架构和设计原则。

## DTO 规则

只有数据跨边界时才创建 DTO：

- HTTP 请求 / 响应
- SSE / 事件流载荷
- 前端线缆契约
- 插件 / MCP / 外部进程边界
- 明确需要版本化的持久化格式

不要为内部函数调用创建 DTO。

## DTO 命名

名字必须体现边界用途：

- `CreateSessionRequest`
- `CreateSessionResponse`
- `SessionEventPayload`
- `CapabilityDescriptor`

不要为同一个概念同时创建多个近义结构，例如：

- `SessionDto`
- `SessionView`
- `SessionData`
- `SessionInfo`
- `SessionModel`

新增结构前，先检查现有 request / response / payload 是否已经拥有这个契约。

## 映射规则

- 在边界做映射，不要在核心逻辑里映射。
- 需要上下文的转换，用显式映射函数。
- 只有明显、无损、无需上下文的转换才用 `From`。
- 不要为了“未来可能用”添加 `Option<T>` 字段。但是可以留下TODO注释说明未来可能添加。
- 不要把内部 enum 直接暴露成线缆契约，除非它本来就是稳定协议。
- `serde(rename_all = "camelCase")` 只应出现在 protocol / wire 类型中，不要随意加到内部结构体上。

## Rust 实现

- 函数保持小而直白。
- 优先使用清晰的领域命名，不要滥用 `utils`、`helper`、`manager`。
- 避免过宽的 `pub`。
- 避免不必要的 `clone`、`unwrap`、`expect`、`panic`。
- 不要在 `.await` 时持有锁。
- 不要启动无生命周期、无错误处理、无 tracing 的后台任务。

## 验证

优先运行最小相关检查：

```bash
cargo fmt --check
cargo test -p <crate> <test_name>
cargo clippy -p <crate> --all-targets -- -D warnings
```

大范围改动再运行：

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```