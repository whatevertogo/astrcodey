---
name: publish-version
description: 按开源项目标准发布 AstrCode 新版本。用户说“发版”“发布”“release”“publish”“发 patch/minor/major”“发布 0.x.y”“打 tag”“触发 release workflow”时使用；负责 SemVer 选择、发布前检查、CI gate、GitHub Actions 发版、精确版本 release PR、tag/发布跟踪与失败排查。
---

# Publish Version

## 发布原则

按开源项目的默认标准执行：

- 从受保护的 `main` 发布，不从脏工作区、未推送提交或临时分支发布。
- 所有代码变更先经 PR 合入；发版动作只做版本 bump、tag、构建和发布。
- 优先用 GitHub Actions `Release` workflow；不要本地手写改版本号。
- 推送 tag、触发 release workflow、npm publish 都属于发布动作，执行前必须获得用户明确确认。
- 失败后先看 CI 日志并修正根因；不要直接重跑掩盖问题。

## 分流

1. 用户要求 `patch` / `minor` / `major`：走 workflow dispatch 标准路径。
2. 用户只说“发版”：建议 `patch`，先确认 bump 类型。
3. 用户指定精确版本号：走 release PR 路径；PR 合并后再 tag 或让用户改用 bump workflow。
4. 用户说“检查能不能发版”：只做 preflight，不触发 workflow，不创建 tag。
5. 只有用户明确要求维护者应急发版，才允许本地直接 tag push。

## SemVer 选择

根据已合入 `main` 的变化选择版本：

| bump | 使用场景 |
|------|----------|
| `patch` | bug fix、文档、CI、内部重构、无兼容性影响 |
| `minor` | 新功能、向后兼容的新 API/CLI/配置能力 |
| `major` | 破坏兼容的 CLI、配置、协议、插件 SDK、持久化格式或 npm 分发变化 |

如果 commit 范围中出现 breaking change、协议/SDK/持久化迁移，默认提高到 `major`，除非用户明确决定不这样做。

## Preflight

先收集状态：

```bash
git fetch origin main --tags
git status --short
git branch --show-current
git rev-parse HEAD
git rev-parse origin/main
git tag --sort=-v:refname | head -5
gh run list --branch main --limit 10
```

必须满足：

- `git status --short` 为空。
- 本地 `main` 与 `origin/main` 一致；否则先 fast-forward 或停止。
- 最近一次目标分支 CI 通过，或用户明确接受等待/风险。
- 没有同名 `v<version>` tag。
- 不覆盖、不删除、不 force-push 已发布 tag；需要修复坏版本时发布新 patch。

检查 release notes 输入：

```bash
LAST_TAG=$(git tag --sort=-v:refname | grep '^v' | head -1)
git log "${LAST_TAG}..origin/main" --pretty=format:'%s (%h)' --no-merges
```

确认重要变更会进入 release notes；若发现 breaking change 或迁移说明缺失，先补文档或 release note 内容。

本地最小检查：

```bash
cargo fmt --all -- --check
python3 scripts/check-deps.py
cargo check --workspace --all-features --exclude astrcode-desktop
bash -n scripts/bump-release-version.sh scripts/prepare-npm-packages.sh
git diff --check
```

完整发布前最好由 CI 覆盖：clippy、tests、audit/deny、frontend lint/typecheck/format/contract/build、多平台 release binary build。

## 标准路径：workflow dispatch

适合 `patch` / `minor` / `major`。不要在本地改版本号。

确认用户同意发布后执行：

```bash
gh workflow run release.yml --ref main -f bump=<patch|minor|major>
```

跟踪：

```bash
gh run list --workflow release.yml --limit 3
gh run watch <run-id> --exit-status
```

失败时：

```bash
gh run view <run-id> --log-failed
```

workflow 会计算下一个版本，运行 `scripts/bump-release-version.sh`，提交版本 bump，创建 `v<version>` tag，并发布 GitHub Release / npm 包。

## 精确版本：release PR 路径

适合用户要求“发布 0.x.y”。不要直接在本地 main 打 tag。

1. 校验版本号和 tag：

```bash
VERSION=<version>
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
! git rev-parse "v${VERSION}" >/dev/null 2>&1
```

2. 创建 release 分支并同步版本：

```bash
git switch main
git pull --ff-only origin main
git switch -c release/v${VERSION}
bash scripts/bump-release-version.sh "$VERSION"
```

脚本负责同步 `Cargo.toml`、lockfile、Tauri、frontend、npm 主包与 s5r fixture 的版本。

3. 验证：

```bash
cargo fmt --all -- --check
python3 scripts/check-deps.py
cargo check --workspace --all-features --exclude astrcode-desktop
bash -n scripts/bump-release-version.sh scripts/prepare-npm-packages.sh
git diff --check
```

4. 提交并开 PR：

```bash
git add -A
git commit -m "chore: bump version to ${VERSION}"
git push -u origin release/v${VERSION}
gh pr create --base main --head release/v${VERSION} \
  --title "chore: bump version to ${VERSION}" \
  --body "Release version metadata sync for v${VERSION}."
```

5. 等 PR CI 通过并合并后，再从最新 `main` 创建 tag：

```bash
git switch main
git pull --ff-only origin main
grep -q "^version = \"${VERSION}\"" Cargo.toml
grep -q "\"version\": \"${VERSION}\"" src-tauri/tauri.conf.json
grep -q "\"version\": \"${VERSION}\"" frontend/package.json
grep -q "\"version\": \"${VERSION}\"" npm/astrcode/package.json
git tag -a "v${VERSION}" -m "v${VERSION}"
```

推送 tag 前再次确认用户同意：

```bash
git push origin "v${VERSION}"
```

tag push 触发 `release.yml` 的 tag 路径；该路径只校验版本一致性，不自动 bump。

## 应急路径：直接 tag

只有维护者明确要求绕过 PR 时使用。仍必须保持工作区干净、CI 通过、版本文件已同步，并在最终回复中说明绕过了 PR gate。

```bash
git switch main
git pull --ff-only origin main
bash scripts/bump-release-version.sh "$VERSION"
cargo fmt --all -- --check
python3 scripts/check-deps.py
cargo check --workspace --all-features --exclude astrcode-desktop
git diff --check
git add -A
git commit -m "chore: bump version to ${VERSION}"
git tag -a "v${VERSION}" -m "v${VERSION}"
git push origin HEAD:main
git push origin "v${VERSION}"
```

## 发布后报告

回复用户时包含：

- 版本号和 bump 类型。
- 使用路径：workflow dispatch、release PR + tag，或应急直接 tag。
- GitHub Actions run URL 或 run id。
- 安装命令：

```bash
npm install -g @whatevertogo/astrcode@<version>
```

- 未跑或未通过的检查，以及剩余风险。

## 项目特定易错点

- 不要把 `eval-tasks/fixtures/` 里的 fixture 版本随发版 bump。
- `src-tauri/Cargo.toml` 使用 workspace 版本；桌面展示版本在 `src-tauri/tauri.conf.json`。
- npm 主包和平台包 license 必须是 `AGPL-3.0-only`。
- Release notes 安装命令必须是 `@whatevertogo/astrcode`，不是裸 `astrcode`。
- Weekly Release 只在上一个 `v*` tag 后有新提交时发布，不发布空版本。
