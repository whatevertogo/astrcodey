## 总原则

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

## 重要

必须遵守：没有遇见bug不准写测试，非复杂逻辑不写测试
项目代码都在crates里面，外置代码不必理会


## TUI style conventions

See `codex-rs/tui/styles.md`.

## TUI code conventions

- Use concise styling helpers from ratatui’s Stylize trait.
  - Basic spans: use "text".into()
  - Styled spans: use "text".red(), "text".green(), "text".magenta(), "text".dim(), etc.
  - Prefer these over constructing styles with `Span::styled` and `Style` directly.
  - Example: patch summary file lines
    - Desired: vec!["  └ ".into(), "M".red(), " ".dim(), "tui/src/app.rs".dim()]

### TUI Styling (ratatui)

- Prefer Stylize helpers: use "text".dim(), .bold(), .cyan(), .italic(), .underlined() instead of manual Style where possible.
- Prefer simple conversions: use "text".into() for spans and vec![…].into() for lines; when inference is ambiguous (e.g., Paragraph::new/Cell::from), use Line::from(spans) or Span::from(text).
- Computed styles: if the Style is computed at runtime, using `Span::styled` is OK (`Span::from(text).set_style(style)` is also acceptable).
- Avoid hardcoded white: do not use `.white()`; prefer the default foreground (no color).
- Chaining: combine helpers by chaining for readability (e.g., url.cyan().underlined()).
- Single items: prefer "text".into(); use Line::from(text) or Span::from(text) only when the target type isn’t obvious from context, or when using .into() would require extra type annotations.
- Building lines: use vec![…].into() to construct a Line when the target type is obvious and no extra type annotations are needed; otherwise use Line::from(vec![…]).
- Avoid churn: don’t refactor between equivalent forms (Span::styled ↔ set_style, Line::from ↔ .into()) without a clear readability or functional gain; follow file‑local conventions and do not introduce type annotations solely to satisfy .into().
- Compactness: prefer the form that stays on one line after rustfmt; if only one of Line::from(vec![…]) or vec![…].into() avoids wrapping, choose that. If both wrap, pick the one with fewer wrapped lines.

### Text wrapping

- Always use textwrap::wrap to wrap plain strings.
- If you have a ratatui Line and you want to wrap it, use the helpers in tui/src/wrapping.rs, e.g. word_wrap_lines / word_wrap_line.
- If you need to indent wrapped lines, use the initial_indent / subsequent_indent options from RtOptions if you can, rather than writing custom logic.
- If you have a list of lines and you need to prefix them all with some prefix (optionally different on the first vs subsequent lines), use the `prefix_lines` helper from line_utils.
