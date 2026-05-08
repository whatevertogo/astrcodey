# AstrCode


## 项目结构

- `crates/` — Rust workspace（nightly 工具链）
- `frontend/` — React + Vite + TypeScript + Tailwind CSS v4

## 后端（Rust）

```bash
cargo fmt                    # 格式化
cargo fmt --check            # 格式化检查（CI 用）
cargo clippy --workspace     # lint
cargo clippy --workspace -- -D warnings  # lint，警告视为错误
cargo test --workspace       # 运行全部测试
cargo test -p <crate>        # 运行指定 crate 测试
```

配置文件：`rustfmt.toml`、`.clippy.toml`

## 前端（TypeScript / React）

```bash
cd frontend
npm run format               # 格式化（Prettier）
npm run format:check         # 格式化检查（CI 用）
npm run lint                 # lint（ESLint）
npm run typecheck            # 类型检查（tsc --noEmit）
npm run build                # 构建（会同时触发 typecheck）
```

配置文件：`frontend/eslint.config.js`、`frontend/.prettierrc`
