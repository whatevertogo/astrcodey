## 总原则

不要把项目搞得一堆石山，把代码放在该放的位置，保持清晰的模块边界和职责划分。

## DTO 规则

只有数据跨边界时才创建 DTO：

- HTTP 请求 / 响应
- SSE / 事件流载荷
- 前端线缆契约
- 插件 / MCP / 外部进程边界
- 明确需要版本化的持久化格式

不要为内部函数调用创建 DTO。

新增结构前，先检查现有 request / response / payload 是否已经拥有这个契约。

## 映射规则

- 在边界做映射，不要在核心逻辑里映射。
- 需要上下文的转换，用显式映射函数。
- 只有明显、无损、无需上下文的转换才用 `From`。
- 不要为了“未来可能用”添加 `Option<T>` 字段。但是可以留下TODO注释说明未来可能添加。
- 不要把内部 enum 直接暴露成线缆契约，除非它本来就是稳定协议。
- `serde(rename_all = "camelCase")` 只应出现在 protocol / wire 类型中，不要随意加到内部结构体上。
  例外：LLM tool call 参数类型（如 `ShellArgs`、`WriteFileArgs`）虽然只在内部使用，
  但其 JSON schema 定义了 LLM 的调用契约（`camelCase` 字段名），
  因此 `rename_all = "camelCase"` 在这些类型上是合理的。

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

## 回复要求

每次完成修改后，回复末尾必须附带：
- **下一步建议**：基于当前改动，接下来最值得做的事情（按优先级排列）若无建议则说无
- **剩余风险**：当前改动中已知或潜在的隐患、未覆盖的边界情况，若无风险则说无

## 重要

  必须遵守：
- 没有遇见bug不准写测试，非复杂逻辑不写测试
- 集成测试单开一个tests/文件夹存放，单元测试写在下面
