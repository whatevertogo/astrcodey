## v0.3.19

Released: 2026-11-16

### 🐛 Bug Fixes

- fix: 收紧 macOS 重启路径校验并修复安全审计 (f26c1257)
- fix(storage): 显式释放会话文件锁 (1836f77b)
- fix: 保留大工具输出头尾并修复 macOS 临时路径启动 (d26557a6)
- fix(session): 防止过期流式快照覆盖已结算消息 (2ef5a4d2)
- fix(deps): 升级 quinn-udp 修复 RUSTSEC-2026-0538 (8cafa1f7)
- fix(session): 修复会话历史回放与旧日志提示 (989c392b)
- fix(deps): 升级 rustix 修复 RUSTSEC-2026-0486 (1fdfd4f8)
- fix(shell): 分离等待预算与执行时限并统一轮询生命周期 (2dab7359)
- fix: 完善 compact 锚点兼容性并升级安全依赖 (bd23d3f2)
- fix(compaction): 保留自动压缩与分叉会话的真实用量 (3e29829f)

### 🔧 Refactors

- refactor(session): 统一会话事件与工具调用生命周期 (a11f193b)

### 📝 Other

- perf(session): 优化深历史分页和前端流式增量 (ba1c958f)
- perf(session): 缩短近期事件回放与快照开销 (7526818f)

### Pull Requests

- #53
- #54
- #55
- #56
- #57

### Contributors

- @whatevertogo

---

**Install:** `npm install -g @whatevertogo/astrcode@0.3.19`
