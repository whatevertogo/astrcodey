## 总原则

不要贪图简洁而牺牲清晰度、可维护性和可扩展性，保持清晰的模块边界和职责划分。

## DTO 规则

只有数据跨边界时才创建 DTO（HTTP 请求/响应、SSE 载荷、前端契约、插件/MCP 边界、版本化持久化格式）。
不要为内部函数调用创建 DTO。新增结构前先检查现有契约是否已满足。

## 映射规则

- 在边界做映射，不要在核心逻辑里映射。
- 需要上下文的转换用显式映射函数；明显、无损的转换才用 `From`。
- 不要为了"未来可能用"添加 `Option<T>` 字段（可留 TODO 注释）。
- 不要把内部 enum 直接暴露成线缆契约。
- `serde(rename_all = "camelCase")` 只用于 protocol/wire 类型和 LLM tool call 参数类型。

## Rust 实现

- 函数保持小而直白，优先清晰领域命名，不滥用 `utils`/`helper`/`manager`。
- 避免过宽的 `pub`，避免不必要的 `clone`/`unwrap`/`expect`/`panic`。
- 不要在 `.await` 时持有锁。不要启动无生命周期、无错误处理、无 tracing 的后台任务。

## 验证

优先最小相关检查：`cargo fmt --check` → `cargo test -p <crate> <test_name>` → `cargo clippy -p <crate> --all-targets -- -D warnings`。
大范围改动：`cargo clippy --all-targets --all-features -- -D warnings` + `cargo test --all-features`。

## 回复要求

每次完成修改后，回复末尾必须附带：
- **下一步建议**：基于当前改动，接下来最值得做的事情（按优先级排列）若无建议则说无
- **剩余风险**：当前改动中已知或潜在的隐患、未覆盖的边界情况，若无风险则说无

## 重要

必须遵守：
- 只写必要的测试。集成测试放 `tests/`，单元测试写在模块下方。
- 只有测试能放 `.unwrap()`。
- 遵循 SOLID 原则：职责单一、trait/enum 组合扩展、行为契约、接口隔离、依赖抽象。
