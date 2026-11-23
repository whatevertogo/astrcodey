## v0.3.20

Released: 2026-11-23

### ✨ Features

- feat(settings): 新增前端网络搜索配置页并支持 DeepSeek 搜索配置 (f28f1347)
- feat(web-search): 支持 DeepSeek 官方网页搜索 API (c194fa89)

### 🐛 Bug Fixes

- fix(attachments): 修复图片附件链路并按 Codex 策略归一化大图 (#65) (455843e6)
- fix(web-search): 保留有效搜索结果并明确错误和计费提示 (1aabffa2)
- fix(storage): 校验结清会话摘要的完整性 (7eeceed5)

### 🔧 Refactors

- refactor(goal): 按需投递目标续跑提示并完善暂停与预算记账 (5f210f20)
- refactor(deps): 将时间处理迁至 Jiff 并移除活跃 chrono 依赖 (f55551d1)

### 📝 Other

- perf(startup): 跳过已结清会话的重复回放 (654797b2)

### Pull Requests

- #58
- #60
- #61
- #62
- #63
- #64

### Contributors

- @whatevertogo
- @zyl71

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.20`
