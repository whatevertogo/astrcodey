## v0.3.12

Released: 2026-07-27

### ✨ Features

- feat(agent): 允许 reviewer 使用 Bash 检查 (efe8cd84)
- feat(extensions): 类型化子会话编排与工具边界 (e705ec20)
- feat: 强化严格工具调用与真实 SWE-bench 评测 (d233f92c)

### 🐛 Bug Fixes

- fix(session): 修复桌面端会话流恢复 (4ae17094)
- fix(sdk): 默认关闭扩展工具 strict (1939f1ed)
- fix(frontend): 升级存在高危漏洞的传递依赖 (7a61a764)

### 🔧 Refactors

- refactor(session): 清理运行时边界并优化工具快照 (93a95299)
- refactor(agent): 用 Markdown 声明内置工具边界 (7440c161)
- refactor(session): 重塑会话运行时与扩展边界 (06f88f9e)

### Pull Requests

- #40
- #41
- #42

### Contributors

- @whatevertogo

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.12`
