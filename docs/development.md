# 开发指南

## 前置条件

- Rust 1.80+
- Cargo

## 构建

```bash
# 编译所有 crate
cargo build --workspace

# Release 构建
cargo build --workspace --release

# 检查（不生成二进制）
cargo check --workspace
```

## 测试

```bash
# 运行全部测试
cargo test --workspace

# 运行特定 crate 的测试
cargo test -p astrcode-core
cargo test -p astrcode-tools

# 显示输出
cargo test -- --nocapture
```

## Linting

```bash
cargo clippy --workspace
cargo fmt --check
```

## 项目结构

```
crates/
├── astrcode-core/        # 共享类型和 trait
├── astrcode-support/     # 宿主环境工具
├── astrcode-ai/          # LLM 提供者
├── astrcode-prompt/      # Prompt 组装
├── astrcode-tools/       # 内置工具
├── astrcode-storage/     # 会话持久化
├── astrcode-context/     # 上下文管理
├── astrcode-extensions/  # 扩展系统
├── astrcode-server/      # 后端服务
├── astrcode-protocol/    # 通信协议
├── astrcode-client/      # RPC 客户端
├── astrcode-tui/         # 终端 UI
├── astrcode-exec/        # 无头执行
└── astrcode-cli/         # CLI 入口
```

## 添加新 crate

1. 在 `crates/` 下创建目录
2. 添加 `Cargo.toml`
3. 在根 `Cargo.toml` 的 `[workspace].members` 中添加
4. 遵循分层规则：只能依赖同层或下层

## 添加新扩展

1. 实现 `Extension` trait
2. 放在 `~/.astrcode/extensions/`（全局）或 `.astrcode/extensions/`（项目）
3. 订阅生命周期事件，注册工具/命令/上下文提供者

## TODO

- [ ] Sandbox 执行环境
- [ ] WebSocket 传输层
- [ ] Web UI 前端
- [ ] MCP 集成
- [ ] TUI 完整实现（ratatui 集成）
- [ ] Session 快照增量恢复
- [ ] 完整的 extension 文件系统加载
