# AstrCode 发布指南

本文记录发版前检查、版本同步和发布 workflow。目标是保证 CLI、桌面壳、npm 包、Cargo lockfile、GitHub tag 和 release notes 使用同一个版本号。

## 1. 发布入口

优先使用 GitHub Actions 的 `Release` workflow 手动发版：

1. 打开 `Release` workflow。
2. 选择 `workflow_dispatch`。
3. 选择 `patch`、`minor` 或 `major`。

该 workflow 会计算下一个版本，调用 `scripts/bump-release-version.sh` 同步元数据，提交版本 bump，创建 tag，再构建 CLI、桌面包、npm 包和 GitHub Release。

不要手动创建 tag，除非已经确认仓库里的所有版本文件都等于该 tag 去掉 `v` 后的版本号。tag push 路径只会校验版本一致性，不会自动修改版本文件。

## 2. 同步的版本文件

`scripts/bump-release-version.sh <version>` 会同步：

| 文件 | 内容 |
|------|------|
| `Cargo.toml` | `[workspace.package].version` |
| `Cargo.lock` | workspace 内 `astrcode*` 包版本 |
| `src-tauri/tauri.conf.json` | 桌面应用版本 |
| `src-tauri/Cargo.lock` | `astrcode-desktop` 包版本 |
| `frontend/package.json` / `frontend/package-lock.json` | 桌面前端包版本 |
| `npm/astrcode/package.json` | npm 主包版本和平台包依赖版本 |
| `crates/astrcode-extensions/tests/s5r-guest/Cargo.toml` | s5r fixture 包版本 |
| `crates/astrcode-extensions/tests/s5r-guest/Cargo.lock` | s5r fixture 相关本地包版本 |

脚本只改 AstrCode 自身包的 lockfile 版本，不重新解析第三方依赖，避免发版 bump 带来无关 lockfile churn。

## 3. Release Notes

`release.yml` 会根据上一个 `v*` tag 到当前 HEAD 的 commit 生成 release notes：

- `feat*` 进入 Features
- `fix*` 进入 Bug Fixes
- `refactor*` 进入 Refactors
- 其它非 `chore/docs/ci/test/style` commit 进入 Other

安装命令固定为：

```bash
npm install -g @whatevertogo/astrcode@<version>
```

## 4. npm 分发

`scripts/prepare-npm-packages.sh` 从 CI 构建产物生成平台包：

- `@whatevertogo/astrcode-linux-x64`
- `@whatevertogo/astrcode-linux-arm64`
- `@whatevertogo/astrcode-darwin-x64`
- `@whatevertogo/astrcode-darwin-arm64`
- `@whatevertogo/astrcode-win32-x64`
- `@whatevertogo/astrcode-win32-arm64`

主包 `@whatevertogo/astrcode` 通过 optional dependencies 选择对应平台包。主包和平台包 license 都必须保持 `AGPL-3.0-only`。

## 5. 发版前检查

发布前至少确认：

```bash
cargo fmt --all -- --check
python3 scripts/check-deps.py
cargo check --workspace --all-features --exclude astrcode-desktop
bash -n scripts/bump-release-version.sh scripts/prepare-npm-packages.sh
git diff --check
```

正式发版前建议确认当前 HEAD 的 CI 全绿，包括 clippy、tests、cargo audit、cargo deny、frontend lint/typecheck/format/contract/build 和多平台 release binary build。

## 6. Weekly Release

`Weekly Release` workflow 每周一运行。它会先检查上一个 `v*` tag 到 `HEAD` 是否有新提交：

- 没有新提交：跳过。
- 有新提交：计算 patch/minor/major 版本，调用同一版本同步脚本，提交 bump，打 tag，触发 `Release` workflow。

因此 weekly release 不会发布空版本，也不会再出现 tag 版本与仓库版本文件不一致。
