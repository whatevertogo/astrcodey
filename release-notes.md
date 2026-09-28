## v0.3.17

Released: 2026-09-28

### ✨ Features

- feat(extensions): 增加跨插件服务与声明式依赖 (77d6e1ec)

### 🐛 Bug Fixes

- fix(extensions): 保持服务发布边界并隔离业务输入 (dd2c7f3a)
- fix(extensions): 补全 worker 作者接口并撤回过期阻塞声明 (ad654e8c)
- fix(deps): 升级 rustls 修复 RUSTSEC-2026-0285 (0b6c0bd6)
- fix(ci): s5r-conformance 改用 astrcode-s5r-runtime 包 (9ce327f6)
- fix(ci): check-deps 补录 astrcode-paths 与 astrcode-s5r-runtime 分层 (f76bac0d)

### 🔧 Refactors

- refactor(extensions): 收拢候选规划与退役资源交接 (df54b834)

### Pull Requests

- #49
- #50

### Contributors

- @whatevertogo

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.17`
