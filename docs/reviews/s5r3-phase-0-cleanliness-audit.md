# S5R3 Phase-0 代码整洁度审计记录

> 范围：分支 `whatevertogo/s5r3-phase-0` vs `main`（+ 工作区未提交 WIP）。
> 方法：作者本人逐文件/逐改动审阅 + 7 个并行 review agent 覆盖各 crate。
> 日期：2026-08-14。
> 本文件是过程与结论的持久记录，不是架构文档。
> 本文第 6、7 节冻结在该轮审计结束时；当前候选状态与最终验收以
> `pr-47-local-candidate-first-principles-review.md` 为准。

---

## 0. PR 规模（先澄清"为什么这么多代码"）

- 371 文件，**+44,109 / −20,107，净新增 ~24k**。
- 删的 ~20k 是旧 S5R 2.0 peer/adapter（文档明确"transitional S5R 2.0 … have been removed"）。
- 41k Rust 里**大量是内联测试**（当时的 contract/当前 SDK wire 模块测试占比 12–73%；`peer_runtime.rs` 约 2300 行中近千行是 `mod tests`）。
- 这是一次 **S5R 2.0 → 3.0 协议重写**，不是无脑堆量。分层经核验无环、职责清晰。

---

## 1. 过程日志（每步看了什么 / 想法）

| 步 | 动作 | 覆盖范围 | 方式 |
|---|---|---|---|
| 1 | 摸清 PR 全貌 | diff --stat、numstat 分类、新文件清单、crate 依赖图 | 本人 git |
| 2 | 架构深度评审 | extension-contract、extension-sdk、session/compaction/projection/worker | 3 agent |
| 3 | 修复①：fingerprint | `event/fingerprint.rs` + 4 调用点（model_context、persistence、reducer/tests、error） | 本人编辑 |
| 4 | 发现工作区有 4 个非本人 WIP | traits.rs/in_memory.rs/model.rs/process.rs（+ 你随后补 session_repo.rs） | 本人 git diff |
| 5 | 验证你 WIP 的正确性 | model.rs（tail unanswered）、process.rs（vars_os）、storage audit 字段 | 本人读码 |
| 6 | 分析 drain_stderr | s5r_ext/session_support.rs | 本人（结论：不改） |
| 7 | 分析 capability 投机面 | capability.rs + 全仓 rg 消费者 | 本人（结论：仅 LiveConversation） |
| 8 | 全量整洁度审计 | ai/client/eval、cli/context/tools、core/storage/server、bundled extensions | 4 agent |
| 9 | 尝试②：utf8_prefix | 曾改用 `str::floor_char_boundary`；随后被 Rust 1.88 MSRV 验证否决并恢复为有界 UTF-8 helper | 本人编辑 + MSRV 验证 |
| 10 | 验证 | fmt（本人 8 文件）、clippy（core/projection/ai）、fingerprint 测试 3/3 | 本人 cargo |

---

## 2. 已应用的修复（已验证）

### 修复① `astrcode-core/src/event/fingerprint.rs` — expect → Result
- **问题**：`serde_json::to_string(message).expect("infallible")` 在非测试、持久化路径上。`LlmContent` 当前只含 `String/bool/Value`，序列化实际不失败；但该不变量未被类型编码，未来加裸 `f64` 会静默变 panic。
- **改法**：`transcript_prefix_fingerprint` 返回 `Result<String, serde_json::Error>`；4 个调用点分别映射：projection 加 `ProjectionError::TranscriptFingerprintSerialization(String)`；persistence 复用既有 `SessionError::Storage(StorageError::Serialization(_))`（无新枚举面）。
- **验证**：core+projection 编译通过；fingerprint 3 测试通过；fmt/clippy 干净。**你随后把 `.map(|m| serde_json::to_string(m))` 收紧成 `.map(serde_json::to_string)`，已接受。**

### 修复② `astrcode-ai` — 保留 MSRV-safe `utf8_prefix`
- **初始误判**：曾把本地 UTF-8 前缀 helper 改成 `str::floor_char_boundary`，并误记为 Rust 1.80 稳定。
- **真实边界**：该 API 在项目 MSRV Rust 1.88 中不可用；nightly/default toolchain 通过不能替代 MSRV 验证。
- **最终改法**：恢复一个局部、带 UTF-8 边界测试的 `utf8_prefix`，由 common/parser 三条截断路径复用；不复制三份实现。
- **验证**：Rust 1.88 下 `astrcode-ai` 与 workspace all-target check 通过；默认 toolchain fmt/clippy 通过。

---

## 3. 分析后**不改**的项（含理由）

| 项 | 位置 | 为何不改 |
|---|---|---|
| `drain_stderr` 静默丢 stderr | s5r_ext/session_support.rs:147 | agent 建议 `trace!` 打印行内容，但 s5r-3-implementation.md 可观测性策略明确"payloads and secrets are never recorded"；子进程 stderr 非结构化、可能含密。朴素 trace 反而违规。需设计（如重定向到 debug 文件），非一行可修。 |
| `LiveConversation` capability 0 消费者 | capability.rs:65 | 仅出现在 DTO 映射与 wire 定义，无扩展请求、无 backend 授权。但删除 wire 变体是产品/版本决策，发版前由你确认（锁版后不可逆）。`ToolIntercept`/`TurnContinuationControl` 经核验有真实消费者，**不**算投机。 |
| `host_invoker.rs` 命名误导 | extensions/runner/host_invoker.rs | 479 行中 90% 是 call-context factory，真 invoker 只 35 行。但属整洁度、且该区你正在活跃编辑，重构会撞车。 |
| `session_control()` 过度工程 | extension-sdk/host/mod.rs:37-89 | 两个单点 helper、授权反复部分检查。同上：整洁度 + 你在改 extension 层。 |
| `session_data_dir` 重复 | mode/lib.rs:38 + goal/lib.rs:32 | 两处字节级相同 helper。med 级，但 bundled extensions 你正在改。建议抽到 SDK `ExtensionPaths` 上的方法。 |
| `llm_mapping.rs` 平行类型对 | extension-sdk/host/llm_mapping.rs | core `Llm*` 与 contract `HostLlm*` 逐字段对应，映射全是搬运。合并可删整个文件，但跨 crate 大改、属有意识边界债。 |
| 删除的 serde alias | mode/store.rs(currentMode 等)、memory/index.rs(turn_end)、mcp/config.rs(type) | 是 breaking-change 策略的一部分（release-notes："不保留旧格式读取分支"）。与项目"边界拒绝旧数据"哲学一致。需确认无线上 camelCase 存量。 |

---

## 4. 看似 bug、实为**有意决策**（重要）

### `TranscriptRewritten.source_fingerprint` 必填、无 `#[serde(default)]`
- **位置**：`astrcode-core/src/event/payload.rs:215`。
- **agent 标记 HIGH**：与同 PR 中 `SystemPromptConfigured.source`、`ToolCallFailed.metadata` 等**有** `#[serde(default)]` 不一致；升级会破坏"PR 前已 compact 的 session"重放。
- **真相**：**有意为之**。release-notes 明确：*"`source_fingerprint` 现在是必填持久化字段；缺少该字段的过渡期 session 日志会在 replay 时被拒绝，**不再跳过并发指纹校验**"*。加 `#[serde(default)]` 会重新引入"缺字段就跳过指纹校验"的旧不安全路径，**撤销该安全加固**。
- **结论**：不改。但风险真实：PR 前 compact 过的 session 升级后会加载失败（projection 重放错误），且 `SNAPSHOT_VERSION` 4→5 故意忽略旧快照、强制走事件日志重放，使该路径必达。release-notes 已声明，**确保升级说明里写清**。

### 你的 storage audit WIP（`quarantined_count`/`skipped_count`/`revision` + `validate_audit_bounds`）
- agent 核验：**正确且完整**。累计 count 经 `PersistedEventConsumerState` 持久化并回读（`session_repo/tests.rs` 断言 `persisted["skippedCount"] == 1`）；`validate_audit_bounds` 在**两个**磁盘边界都调用（load 的 `TryFrom`、write 的 `write_event_consumer_state`）；`in_memory.rs` 与 `session_repo.rs` 一致（都改走 `record_quarantine`/`record_skip`）。
- 附带修了一个真 bug：`custom_event_control.rs:214` 从 `quarantined.len()`（窗口长度，会少报）改为 `quarantined_count`（真实累计）。
- 小提示：`EVENT_CONSUMER_STATE_VERSION` 2→3 硬拒绝，但真实 v2 文件会先在 serde 因缺字段报错，再到不了版本检查；现有测试用"注入 version=2 但其余 v3 形状"的 JSON，给了关于迁移路径的**虚假信心**（影响低，consumer 状态可重建）。

---

## 5. 全量发现汇总（按严重度）

### Medium（应处理）
| 位置 | 问题 | 状态 |
|---|---|---|
| ai UTF-8 bounded prefix ×3 | nightly 可用的 `floor_char_boundary` 不满足 Rust 1.88 MSRV | ✅ 已恢复共享 helper 并通过 MSRV |
| core `fingerprint.rs:36` expect | 持久化路径 expect | ✅ 已修 |
| mode+goal `session_data_dir` | 跨扩展重复 helper | ⏸ 待你定（活跃编辑区） |

### Low（整洁/可选）
- `llm.rs:476-510` `non_cached_tokens` 与 `context_tokens_after_response` 对 legacy-`None` 分类不对称，加交叉引用注释。
- `session_command_service.rs:206` `abort()` 返回的 bool 被丢弃，接线或加注释。
- `child_session.rs:387` `is_descendant` 现在全链走到根（无早退），可接受。
- `traits.rs:160` `push_recent` 用 `Vec::remove(0)`（O(n) 移位），界内 128 可接受。
- `mcp/pool.rs:159,448` `Mutex::lock().unwrap_or_else(into_inner)` 在 Drop/shutdown 吞 poison，best-effort 可接受但需有意。
- `mcp/lib.rs` `working_dir` 物化方式不统一（`to_string_lossy().into_owned()` vs 直接持 `Cow`）。
- `agent-tools/lib.rs:319` 错误信息中文，与周围英文面不一致；`:169` 一处可省的 clone。
- `ai/common.rs` 旧测试 `thinking_capability` 局部提取 churn（多处 body.rs 测试）。
- `cli/handle_event.rs:530` custom-event 走 `_` 兜底分支，不如显式 arm 可读。
- 若干测试合并降低了失败定位粒度（compaction/mod.rs、child_agent.rs:136）。

### 正向（值得保留）
- `cli/handle_event.rs:489-492` 修了真 bug：旧 `tool_call_id.to_string()` 把 `"Some(..)/None"` 写进 map。
- `context.rs:43,55` 用共享 `provider_transcript` 替代内联过滤。
- `memory/config.rs` `from_extension_config` 改返回 `Result`，不再 default 掩盖损坏。
- `agent-tools/agent.rs:149` 用 SDK `frontmatter::normalize_markdown` 替代手写归一化。
- `openai/body.rs` prompt-cache-key 用 `stable_hash_hex` 字节级等价、双测试锁定。
- `host_router/process.rs`（你 WIP）`vars_os()` 修非 UTF8 env key panic + 正确 `unsafe`/SAFETY。

### 之前评审已记录、本轮复核仍成立
- `peer_runtime.rs`（约 2300 行）仍建议按 handle/driver/inbound/stream/tests 接缝在 SDK `wire/peer_runtime/` 内拆分；独立 contract crate 已删除，不应重新建立平行 crate。
- DTO 命名 `Dto` 后缀不统一；魔法常量（`MAX_REENTRANCY=8` 等）散落。
- `stable_hash_hex` 是 FNV-1a 非密码学，勿用于完整性/安全。

---

## 6. 验证状态

| 命令 | 结果 |
|---|---|
| `rustfmt --check`（本人 8 文件） | ✅ 干净 |
| `cargo test -p astrcode-core fingerprint` | ✅ 3/3 |
| `cargo clippy -p astrcode-core` / `-p astrcode-session-projection` | ✅ 干净 |
| `cargo check -p astrcode-ai` / `clippy -p astrcode-ai --all-targets` | ✅ ai 干净 |
| 全工作区 `clippy --all-targets --all-features` | ❌ **未跑通** — 你的 bundled-extensions WIP（ask-user/goal/web-tools/memory/mcp/extensions）当前编译失败，非本人改动。等你收尾后再跑。 |
| 全工作区 `cargo test --all-features` | ❌ 同上，未跑。 |

---

## 7. 剩余风险

1. **升级破坏**：PR 前 compact 过的 session 升级后加载失败（`source_fingerprint` 必填 + 快照 v4→5 强制重放）。已在 release-notes 声明，确保升级文档醒目。
2. **迁移测试虚假信心**：consumer-state v2→3 的测试未用真实 legacy 形状，真实旧文件会先报 serde 错。
3. **bundled extensions 当前不编译**（你的 WIP），审计基于静态读码，未跑其测试。
4. `LiveConversation` 若随版本发出，wire 名永久保留。
5. `drain_stderr` 仍丢 stderr；磁盘扩展崩溃后缺子进程自诊断。
