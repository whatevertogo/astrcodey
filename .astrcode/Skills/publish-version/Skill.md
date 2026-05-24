---
name: publish-version
description: 发布新版本：检查并同步所有版本号，提交，打 tag，推送到 GitHub 触发 release workflow
---

## 触发条件

用户说「发版」「发布」「publish」「release」或明确要求发布某个版本时触发。

## 发版流程

### 1. 确认目标版本号

询问用户要发布的版本号（如 `0.1.7`），或根据上一个 tag 自动推断 patch bump。

```bash
git tag --sort=-v:refname | head -1
```

### 2. 版本号一致性检查

**逐项检查以下所有位置**，全部必须与目标版本一致。这是最容易出错的一步，不可跳过。

| # | 位置 | 检查命令 | 说明 |
|---|------|----------|------|
| 1 | 所有 crate Cargo.toml（21个） | `grep '^version' crates/*/Cargo.toml` | workspace 下的所有 crate |
| 2 | src-tauri/Cargo.toml | `grep '^version' src-tauri/Cargo.toml` | Tauri 桌面应用的 Rust 依赖，**容易遗漏** |
| 3 | src-tauri/tauri.conf.json | `grep '"version"' src-tauri/tauri.conf.json` | 决定桌面安装包的文件名版本（如 `AstrCode_0.1.7_x64-setup.exe`） |
| 4 | frontend/package.json | `grep '"version"' frontend/package.json` | 前端 npm 包版本 |
| 5 | npm/astrcode/package.json | `grep '"version"' npm/astrcode/package.json` | npm 主包模板，CI 中 `prepare-npm-packages.sh` 用 `$VERSION` 注入，但本地模板也需同步 |
| 6 | npm/astrcode/package.json 依赖 | `grep 'whatevertogo/astrcode-' npm/astrcode/package.json` | 6 个平台依赖的版本号也需要更新 |

**如果发现不一致，先统一更新再继续。** 更新命令（把 `OLD` 替换为当前版本，`NEW` 替换为目标版本）：

```bash
# 1. 所有 crate Cargo.toml
for toml in $(find crates -name "Cargo.toml"); do
  sed -i "0,/^version = \"OLD\"/{s/^version = \"OLD\"/version = \"NEW\"/}" "$toml"
done

# 2. src-tauri/Cargo.toml
sed -i "0,/^version = \"OLD\"/{s/^version = \"OLD\"/version = \"NEW\"/}" src-tauri/Cargo.toml

# 3. src-tauri/tauri.conf.json
sed -i 's/"version": "OLD"/"version": "NEW"/' src-tauri/tauri.conf.json

# 4. frontend/package.json
sed -i 's/"version": "OLD"/"version": "NEW"/' frontend/package.json

# 5 & 6. npm/astrcode/package.json（主版本 + 6个平台依赖）
sed -i 's/"version": "OLD"/"version": "NEW"/g' npm/astrcode/package.json
sed -i 's/"@whatevertogo\/astrcode-\(.*\)": "OLD"/"@whatevertogo\/astrcode-\1": "NEW"/g' npm/astrcode/package.json
```

### 3. 编译验证

版本号更新后必须通过编译和测试：

```bash
cargo check
cargo fmt --check
```

### 4. 提交并打 tag

```bash
git add -A
git commit -m "chore: bump version to <VERSION>"
git tag -a "v<VERSION>" -m "v<VERSION>"
```

### 5. 确认推送

**推送前必须向用户确认**，因为推送 tag 会立即触发 GitHub Actions release workflow（构建 + npm publish + GitHub Release），不可撤销。

确认后：
```bash
git push origin main
git push origin "v<VERSION>"
```

### 6. 发版后提示

告知用户：
- GitHub Actions release workflow 已触发
- 可在 GitHub Actions 页面查看构建进度
- release 产物包括：CLI 二进制（6 平台）、Tauri Desktop 安装包、npm 包、GitHub Release

## 注意事项

- 不要跳过版本号一致性检查，这是最常出错的地方
- `src-tauri/Cargo.toml` 和 `src-tauri/tauri.conf.json` 在 workspace 外，极其容易遗漏
- npm 版本由 `scripts/prepare-npm-packages.sh` 通过 `$VERSION` 环境变量注入，但本地模板 `npm/astrcode/package.json` 也需要同步，否则下次手动检查会混乱
- `eval-tasks/fixtures/` 下的 `Cargo.toml`（version = "0.1.0"）是测试夹具，不需要更新
- 如果用户想重新发布同一版本，需要先删除远程 tag
