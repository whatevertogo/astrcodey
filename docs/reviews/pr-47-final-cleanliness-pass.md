# PR #47 Final Cleanliness Pass

> 本文是 PR #47 最终架构清理的事实记录、职责图和验收账本。
> 前置文档:`docs/reviews/pr-47-local-candidate-first-principles-review.md`(设计与逐轮修复记录)。
> 本文只记录由当前源码重新验证的结论;被源码推翻的判断直接修改,不迁就原设计。

## 1. 审查范围与代码快照

- branch:`whatevertogo/s5r3-phase-0`
- HEAD:`d5ccdc87`(与远端 PR head 一致)
- upstream PR head:`d5ccdc87c5bda41fab5601f555ad7e866c501f57`
- working tree:17 个已跟踪文件修改 + 2 个新增生成 DTO(`CustomEventDeliveryDto.ts`、`TransportFeatureDto.ts`),无未跟踪源码
- 审查日期:2026-08-15 至 2026-08-16
- 涉及 crate:`astrcode-extensions`(runner/host_router/loader/s5r_handler)、`astrcode-extension-worker`、
  `astrcode-server`(测试消费面)、frontend 生成协议
- 明确不在范围内:Windows 真实 runner 验收(只能由新 CI 证明)、`ModelBinding` 持久化重构(后续任务)、
  PTY 能力(已作删除决策,不重开)

远端 CI(d5ccdc87)状态:Rust fmt / Dependency Direction / Security Audit / cargo-deny / Frontend 三项 pass;
Check(三平台)、Clippy、MSRV(三平台)、Test、Contract Test fail。四个已知根因(strict attachments fixture、
S5R guest generic fixed-hook、外部测试构造 crate-private `InvokeContext`、Windows `unused_mut`)均已有本地
候选修复,但未推送,不得宣称远端通过。

## 2. 最终架构不变量(源码已确认)

### I-1 Extension generation publication

- 状态 owner:`ExtensionRunner::registry`(`runner/mod.rs:157-164`):`extensions` + `index: ArcSwap<HandlerIndex>` +
  `publication: RuntimePublication` + `publication_stable: Notify`。
- 允许修改者:`PreparedExtensionGeneration::commit_with`(`runner/mod.rs:321-427`)与
  `ExtensionRunner::rebuild_index_before_stable`(`runner/mod.rs:1301` 起,direct register/unregister 路径)。
- 线性化点:`RuntimePublicationGuard::drop`(`runner/mod.rs:208-230`)在最后一个 writer 退出时提交
  `pending_generation` 并 `notify_waiters`;`Stable(epoch)` 由此产生,是唯一可见性边界。
- 顺序(commit_with):build index → `bind_once` + `mark_ready`(mod.rs:395-399)→ 替换 active 列表 →
  `publish(generation)`(core 代)→ `generation_gate.activate()` + tasks 激活(mod.rs:402-405)→
  `index.store(index)`(mod.rs:406)→ drop publication guard(Stable)。
- 可见性证明:等待式读取 `extension_view()`(mod.rs:2015-2032)要求
  `is_stable_generation`(`active_writers == 0 && generation == index.generation`),
  `index.store` 与 guard drop 之间的窗口内 waiter 不会返回 candidate。
  非等待读取 `turn_extension_view()`(mod.rs:2034)可能读到 Updating 中间态,由 Session 侧
  core `Arc` + expected epoch + Runner Stable 双重校验兜底(见 review 文档 12.4)。
  `commit_with` 内同步 probe 已锁定:publish 回调中 `turn_extension_view()` 仍返回旧代
  (`runner/tests.rs:3092-3098`)。
- 失败/取消 owner:prepare 阶段任一失败走 `abort` / `Drop`(mod.rs:429-463),把已启动 candidate
  移交 `StartupFailed` retirement;save 后路径不含可失败分支。

### I-2 turn runtime pin

- turn 在首个 hook 前固定 core `Arc`、expected extension epoch、main/small `LlmProviderBindings`、
  context assembler 与 Extension view;旧 turn 在 reload 后继续旧代,不跳到新代。
- Extension-to-Extension HTTP 属于 pin 的一部分:调用方 context 携带绑定该代 `HandlerIndex` 的
  dispatcher(见 I-3)。

### I-3 Extension-to-Extension public HTTP

- 每个 `HostedExtension` 持有一个 `GenerationPublicHttpDispatcher`(`runner/http.rs:19-25`,
  `runner/mod.rs:243`):`OnceLock<Weak<HandlerIndex>>` + 共享 diagnostics/timeout/factory。
- candidate 创建时未绑定(`for_candidate`,http.rs:186-196;mod.rs:612、1169);commit 时
  `bind_once`(http.rs:198-200)先于 gate 激活。
- `Weak` 避免 index↔dispatcher 强引用环;retirement 后 `upgrade` 失败,fail closed
  (http.rs:202-205,`NotFound`)。
- handler/tool context 固定到 view 的 index:`make_registered_extension_call_context_from_index`
  覆盖 `public_http_dispatcher`(`host_invoker.rs:394-395`);tool adapter 在构造时绑定
  (`tool_adapter.rs:188、303`);startup context 持有该代 dispatcher(mod.rs:640、1190)。
- `custom_event_delivery.rs:453` 的 `None` 只是默认值,随即被 `make_registered_..._from_index`
  覆盖,不存在逃口。
- 外部 HTTP 入口(`dispatch_public_http_route`、runner 的 `PublicHttpDispatcher` impl、
  `WeakRunnerPublicHttpDispatcher`,http.rs:219-298)统一走等待 Stable 的 `extension_view()`。
- 已锁定行为(`extension_http_uses_the_callers_pinned_generation_while_external_http_uses_current`,
  tests.rs:3032-3125):caller G1 retained 进 G2、target 换成 v2 后,旧 turn handler 与
  startup 保存的 Host 都得 `v1`(断言 `"v1/v1"`),外部 HTTP 得 `v2`。
- Retain 语义(重要):`bind_once` 用 `OnceLock::get_or_init`,对 retained 实例的重复绑定是
  **有意 no-op**——StartContext dispatcher 永久绑定实例诞生代。诞生代 index 在所有旧 view/lease
  释放后回收,此后该实例仍存活但其 startup Host 的 extension HTTP 调用 fail closed。
  当前无 bundled extension 在 startup 后长期持有 Host 做 extension HTTP,此语义可接受并已由
  测试锁定;若未来出现该用例,需要在 commit 时对 retained dispatcher 重绑新 index(见第 5 节 F-2)。

### I-4 candidate panic/cancellation

- `start()` panic 由 `AssertUnwindSafe(..).catch_unwind()` 收敛为 typed
  `ExtensionError::Internal("extension {id} start panicked")`(mod.rs:1216-1228)。
- 失败路径同步执行 `pending.retire()` → `retirement.wait().await`(mod.rs:1230-1245),
  等待 stop/task/Host-resource cleanup 完成后才返回 typed error;source transaction
  在清理完成前不释放。
- 回归 `panicking_source_start_finishes_rollback_before_the_next_reconcile`(tests.rs:1730 起):
  `stop(StartupFailed)` 用 `Notify` latch 阻塞,第二次同 ID reconcile 在
  `begin_source_transaction` 上等待且未 start;释放后第一候选返回 typed error,第二候选才启动发布。
  无 sleep 猜时序。

### I-5 Host resource ownership

- process handle owner 是 `(session_id, ExtensionInstanceId)`;reload 新旧 instance 互不可见;
  旧 instance 在 lease drain 与 stop 后只清理自己的 handles(review 文档 12.5 表)。
- cancellation/Drop 只撤权与回收 Host-owned 资源,不回滚已完成外部事实。

### I-6 S5R fixed hook

- `Worker::on_pre_tool_use` / `on_tool_input_transform` 是 fixed-mode hook 的 dedicated API
  (`astrcode-extension-worker/src/worker/mod.rs:154-169`);generic `register_hook` 对 fixed event
  返回 `TypedHookRequired` 并指向 dedicated API(`worker/registry.rs:888-897`)。
- s5r-guest 已迁到 dedicated API(`tests/s5r-guest/src/main.rs:449` 起)。

## 3. 模块与文件职责表(本轮涉及文件)

| 文件/模块 | 唯一职责 | 允许依赖 | 禁止依赖 | 对外接口 | 当前问题 |
|---|---|---|---|---|---|
| `runner/mod.rs` | generation 生命周期、publication、source transaction、direct register 路径 | core/sdk/storage/projection、runner 子模块 | 具体 Extension 实现 | `ExtensionRunner` 公共运行时 API | `register`/`register_with_startup_working_dir`/`unregister` 仍是 production pub(F-1) |
| `runner/http.rs` | Extension HTTP 路由匹配、按代 dispatcher、外部/内部入口 | host_router trait、index | Session/Server | `ExtensionHttpDispatchResult`、runner `public_http_dispatcher()` | 无 |
| `runner/host_invoker.rs` | call context 组装与 Host invoke wiring | host_router、sdk internal | — | crate 内部 | 无 |
| `runner/tests.rs` | runner 行为回归(含 generation HTTP 与 panic rollback) | — | — | 无 | 无 |
| `host_router.rs` | capability/lease/输入边界校验与 operation 分发 | sdk wire、storage | Session/Server | `HostRouter`、`HostBackends`、`InvokeContext` | 无 |
| `host_router/workspace.rs` | workspace 路径/观察/patch/search 原语 | sdk、std | — | crate 内部 | 无;`no_follow_options` 已按 cfg 拆分消 Windows unused_mut |
| `host_router/extension_http.rs` | public HTTP capability 的 dispatcher 选择(context pinned 优先,router 全局兜底) | sdk | — | crate 内部 | 全局兜底只服务无代 context;见 F-2 |
| `loader.rs` | discovery/transport admission/config 候选 | runner、sdk transport | — | `ExtensionLoader` | `ExtensionAdmissionError` 已收窄为私有 |
| `s5r_handler.rs` | S5R handler 结果 ↔ runtime 类型映射 | sdk wire | — | crate 内部 | 无;pre_compact `Ok`+data 现拒绝 |
| `astrcode-extension-worker/src/worker/mod.rs` | 作者侧 worker API | sdk | host runtime | `Worker` | 无 |
| `astrcode-extension-worker/src/worker/registry.rs` | worker handler 注册与 manifest 生成 | sdk wire | — | crate 内部 | 无 |

## 4. 数据与控制流(已验证部分)

### Extension generation publication

见 I-1。唯一写入路径是 `commit_with` / `rebuild_index_before_stable`;唯一线性化点是
publication guard drop;外部 HTTP 与等待式 view 都不会在 Updating 期间看到 candidate。

### Extension call cancellation

call token 由 `linked_call_cancellation`(host_invoker.rs:320-338)链接 generation tasks 与
caller cancellation;handler 返回时 `drop_guard` 结束 call(token 不被外泄 context 续命)。

### candidate panic

见 I-4。

### reviewnow 复核(本轮源码确认)

- generation gate 覆盖全部 Host 入口:`ensure_invoke_active`(host_router.rs:195-201)在
  `invoke`(:412)与 `invoke_event_stream`(:454)首行执行,candidate/retired 代一律 `HostNotReady`。
- old turn 不重读 current generation:`pin_turn_generation`(session_runtime_services.rs:226-254)
  双重校验 core `Arc` identity + Runner Stable epoch;`turn_pin_never_mixes_core_and_extension_publication_epochs`
  与 `turn_pins_extension_and_model_generations_before_first_hook` 两条回归通过。
- server 无 ask-user extension ID/event type 特判:全仓搜索只剩 stream.rs:316 一行注释。
- 无 Arc 强引用环:dispatcher 持 `Weak<HandlerIndex>`;router 经 `WeakRunnerPublicHttpDispatcher`
  持 `Weak<ExtensionRunner>`;`ExtensionCallContextFactory` 不持有 runner。
- `ExtensionInstanceId` 按 instance 清理:replacement 有新 instance id,旧代 cleanup 不误伤新实例
  (retirement 路径只传旧 instance 的 identity)。

### S5R worker/peer lifecycle

本轮未改;沿用 review 文档 4.4(pinned `next_frame`)结论。

### 其余流程(provider contribution、Compact、custom event)

本轮未改代码,沿用 review 文档第 5-13 节结论;最终 code-simplifier 阶段再复核当前源码。

## 5. 审查发现

| ID | 严重度 | 触发条件 | 证据 | 决策 | 状态 |
|---|---|---|---|---|---|
| F-1 | High | 测试或误用代码在 runtime services 固定 expected epoch 后调用 `ExtensionRunner::register`,推进 runner generation → turn admission 永久 `RuntimeUnstable` | `register`/`register_with_startup_working_dir` pub(mod.rs:499、504);`unregister` pub(mod.rs:764)但 production 仅 shutdown 内部使用(mod.rs:877);production 无 register 调用方 | register/register_with_startup_working_dir 收 `#[cfg(any(test, feature = "testing"))]`;unregister 收私有;新增 `testing.rs` 一次性装配 façade;跨 crate 测试迁移 | Verified |
| F-2 | Low | retained 实例的 startup Host 在其诞生代 index 回收后做 extension HTTP | `OnceLock::get_or_init` 重复绑定 no-op(http.rs:198-200);retained 实例不重建 dispatcher;测试锁定 `"v1/v1"` | 接受当前语义并记录;当前无 bundled 消费者长期持有 startup Host 做 extension HTTP;未来有该用例时在 commit 重绑 retained dispatcher | Deferred |
| F-3 | Info | `bind_once` 重复绑定静默忽略不同 index | http.rs:198-200 | 该 no-op 是 Retain 语义的预期行为,由 OnceLock 类型与 pinned-generation 测试共同编码 | Verified |
| F-4 | High | 测试 helper 以 Direct 起源注册 bundled 扩展(如 astrcode-mode),随后任何 config update 的 source reconcile 把同 id 作为 Start 候选,`accepted` 含 Direct 实例 → 自冲突 → HTTP 500 `extension_candidate_failed`;且 reload/publication 会用 config 重建的 provider 覆盖测试注入的 LLM | `prepare_source_generation` accepted 集含 `ExtensionOrigin::Direct`(runner/mod.rs:1062-1066);commit_with 对 Direct 恒 retained;`ConfigManager::publish_to` 用 `build_provider_from_settings` 重建 provider(config_manager.rs:443-456)。失败自 5a0ece23 后存在(远端 Test job 在 conformance 步失败,从未执行 workspace tests,故 CI 未暴露) | http_routes helper 改为:测试自定义扩展走 façade(Direct);bundled 扩展经 `prepare_extension_generation` + `commit_with` 以 Source 起源加载,publication 回调用**测试注入的 LLM** 发布 matching epoch(不覆盖 provider);新增 `test_support::bind_extension_host_router_for_test` 复用生产接线 | Verified |
| F-5 | Low | `InteractiveCommandProbeExtension` 声明 `argument_completions(true)` 但 handler 未 override `supports_argument_completions()` → `InvalidRegistration` | registrar.rs:540 校验;handler/tests.rs:438-455 | fixture 补 `supports_argument_completions() -> true`(handler 本就实现 `complete`) | Verified |
| F-6 | Info | `check_health`/`ExtensionHealthReport` 与 `bind_startup_event_channel`/`startup_event_tx` 在仓内无 production 调用方,但 README.md:466 将其宣传为宿主 API | diagnostics.rs:51-95;runner/mod.rs:1354 | 保留:属于对外 host SPI 而非测试专用接口;startup 事件通道在 production 未接线意味着 startup 期进程级事件当前不送达——记录为已知产品缺口而非本轮删除对象 | Deferred |

## 6. 实施记录

| 变更 | 删除了什么 | 新增了什么 | 为什么属于该 owner | 验证 |
|---|---|---|---|---|
| 收回 direct runner mutation API(F-1) | production 的 `register`/`register_with_startup_working_dir`/`unregister` pub 可见性;随之 dead 的 `ExtensionOrigin::Direct` 相关 arm、`RegistrationPublication`、`publish_registration_locked`/`finish_registration`/`lock_registration_operation`、`ExtensionStageOutcome::Skipped`、`RetirementSupervisor.operation_gates` 在无 testing feature 时一并 gate | `astrcode-extensions` `testing` feature + 自引用 dev-dep;`src/testing.rs::extension_runner_with_extensions`(先 bind router,再逐个启动,重复 ID/任一步失败即 shutdown 并返回 Err,全部成功才返回 runner) | mutation 能力只属于 ConfigManager source transaction 与 runner shutdown;测试装配属于 testing façade | `cargo check -p astrcode-extensions`(无 feature)0 warning;`cargo test -p astrcode-extensions --all-features` 111+7+18+1 全过;server 99+9+36+21+19 全过;两 crate clippy `-D warnings` 通过 |
| http_routes 测试运行时改走 source generation(F-4) | helper 里 `astrcode_extension_mode::extension()` 的 Direct 注册;compact 测试里显式注册 coding 的 Direct 调用 | `test_support::bind_extension_host_router_for_test`(复用生产 bootstrap 接线);helper 末尾 `prepare_extension_generation` + `commit_with` 以 Source 起源加载 bundled 集,publication 回调用测试注入 LLM 发布 matching epoch | bundled 扩展加载的权威路径是 ConfigManager/source generation;测试不应绕过 | `cargo test -p astrcode-server --all-features --test http_routes` 36/36 通过(含 provider_preset、concurrent_config_updates、compact route、turn/SSE 场景) |
| command fixture 补全(F-5) | 无 | `InteractiveCommandProbeHandler::supports_argument_completions() -> true` | 声明与 handler 能力一致是 registrar 契约 | `session_commands_share_extension_resolution_and_transport_admission` 通过;server lib 99/99 |
| session_turn generation pin 测试修复 | mock 初始 `current_generation: 9` 与 envelope hook 里 keep-epoch 的 `publish_runtime_generation`(两者使 expected epoch 永远无法收敛到 runner 代) | 初始代 1;envelope hook 改 `publish_runtime_generation_for_extension` 并携带翻转后的 epoch(对齐生产 commit_with 的 publish 回调) | mock 必须与生产 publication 语义一致,否则测的是不可能状态 | `cargo test -p astrcode-session --all-features` 75+2+1+5+9 全过 |
| code-simplifier 批次 | `HostBackends.public_http_dispatcher` 字段(无生产者,真实 dispatcher 只经 `build_host_router_with_public_http_dispatcher` 独立参数);`build_host_router` 一行 wrapper;`ExtensionRunner::dispatch_public_http_route_from`(零消费者);`SessionRuntimeServices::publish_runtime_generation`(keep-epoch 变体,仅测试消费,生产一律走 `_for_extension`);`ExtensionHttpGroup::new`(无调用方) | `mock_llm_settings()` 提取 http_routes 重复的 20 行 `LlmSettings` 字面量 | 同一事实一个入口;无消费者的公共项删除或收窄 | extensions/session/server 三 crate test+clippy 全过;`cargo check -p astrcode-extensions`(无 feature)0 warning |
| 测试专用 API 收窄 | production 可见性:runner 的 10 个 hook/tool 派发方法、`count`、`registered_extension_ids`、`tool_catalog_snapshot_typed`(runner 与 ExtensionView 两级) | `#[cfg(any(test, feature = "testing"))]` gate;shutdown 改用私有 `snapshot_registered_extension_ids`;`session_ops_ref` 降私有;`record_extension_load_success/failure` 降 `pub(crate)` | production 只经 pinned `ExtensionView` 的 trait port 派发;runner inherent 方法只有测试消费 | 同上;gated 后测试调用方(cfg(test)/testing feature 覆盖)不受影响 |

## 7. 删除的旧路径与兼容分支(本轮)

- 删除外部测试 `crates/astrcode-extensions/tests/workspace_read_security.rs`(构造 crate-private
  `InvokeContext`);场景迁入 owner 单元测试 `host_router.rs::tests::workspace_read_stays_within_root_and_enforces_the_size_bound`。
- S5R guest 的 generic `worker.hook(PreToolUse, Blocking)` 删除,改 dedicated `on_pre_tool_use`。
- production API 删除(收回 testing 边界):`ExtensionRunner::register`、`register_with_startup_working_dir`、
  `unregister`(私有化);production 唯一保留的装配路径是 ConfigManager source transaction + runner shutdown。
- 旧测试模式删除:http_routes helper 的 Direct 注册 bundled 扩展 + 无 host router 接线;
  唯一保留的权威路径是 façade(测试自定义扩展)+ source generation(bundled 扩展)。
- `HostBackends.public_http_dispatcher` 重复入口字段(恒为 None)。
- `build_host_router` 一行 wrapper(测试改用 `Arc::new(HostRouter::from_backends(..))`)。
- `ExtensionRunner::dispatch_public_http_route_from`(零消费者;view 级同名方法保留,
  由 pinned dispatcher 路径消费)。
- `SessionRuntimeServices::publish_runtime_generation`(keep-epoch 变体);两个测试调用点迁移到
  `publish_runtime_generation_for_extension` 并显式携带 epoch。
- `ExtensionHttpGroup::new`(删除字段后无调用方,`Default` 足够)。

## 8. 验证矩阵

| 命令 | 结果 | 对应行为 | 备注 |
|---|---|---|---|
| `cargo fmt --all -- --check` | 通过 | 格式 | |
| `git diff --check` | 通过 | 空白/冲突标记 | |
| `python3 scripts/check-deps.py` | 通过,29 crates | 依赖方向 guard | |
| `cargo test -p astrcode-extensions --all-features extension_http_uses_the_callers_pinned_generation_while_external_http_uses_current` | 1 passed | G1 handler/startup Host 得 v1、外部 HTTP 得 v2、Updating 窗口 probe | 行为验证 |
| `cargo test -p astrcode-extensions --all-features panicking_source_start_finishes_rollback_before_the_next_reconcile` | 1 passed | candidate panic → StartupFailed cleanup 完成后才放行下一次 reconcile | 行为验证,Notify latch 无 sleep |
| `cargo test -p astrcode-extensions --all-features` | 111(lib)+7(loader)+18(s5r_e2e)+1(turn_scoped)全过 | extensions 全量 | |
| `cargo clippy -p astrcode-extensions --all-targets --all-features -- -D warnings` | 通过 | | |
| `cargo check -p astrcode-extensions`(无 feature) | 通过,0 warning | production surface 不含 gated mutation API | |
| `cargo test -p astrcode-server --all-features` | 99(lib)+9(extension_integration)+36(http_routes)+21(session_operations)+19(turn_scheduler)全过 | server 全量,含 config update、compact route、turn/SSE | |
| `cargo clippy -p astrcode-server --all-targets --all-features -- -D warnings` | 通过 | | |
| `cd frontend && npm run generate:protocol` | 通过,无 diff | 生成产物与 Rust DTO 一致 | 幂等 |
| `cd frontend && npm run check` | 全部子项通过(protocol/typecheck/lint/format/contract/各测试) | 前端契约 | |
| `cd frontend && npm run build` | 通过 | 前端构建 | |
| `cd frontend && npm audit --audit-level=high` | 0 vulnerabilities(首次 ECONNRESET,重试成功) | 依赖审计 | |
| `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --all-features -- -D warnings` | 通过 | 全仓静态检查 | 最终批次(含全部 simplifier 改动后) |
| `CARGO_INCREMENTAL=0 cargo test --workspace --all-features --no-fail-fast` | 全部 `test result: ok`,无 FAILED 行 | 全仓行为测试 | 含 S5R E2E 18/18、SDK、session/server/storage 等 |
| `RUSTUP_TOOLCHAIN=1.88.0 cargo check --workspace --all-targets --all-features` + guest check | 通过(workspace 3m04s;guest 2.96s) | MSRV | |
| S5R conformance(`s5r-conformance` + release guest) | 通过:initialize/activate、unary、streaming、nested invoke、cancellation、unknown error、clean shutdown、malformed/oversized frame 拒绝 8 项全过 | S5R wire 契约 | 真实行为输出,非 0 tests(日志中两条 "s5r guest failed" 是负例场景的期望拒绝) |

## 9. 远端 CI

- 当前远端 commit:`d5ccdc87`(与本地 HEAD 相同)。
- 远端失败:Check×3、Clippy、MSRV×3、Test、Contract Test(run 31791253548)。
- 本地候选修复:strict attachments fixture、S5R fixed-hook 迁移、`InvokeContext` 外部测试迁移、
  Windows `unused_mut` cfg 拆分——全部未推送。
- 尚未重新触发的 CI:全部;本工作不涉及 commit/push。
- 真正远端通过的检查:Rust fmt、Dependency Direction、Security Audit、cargo-deny、Frontend
  format/lint/typecheck(均针对 d5ccdc87,不覆盖本地候选)。

## 10. 剩余风险

- F-2:retained 实例 startup Host 在诞生代回收后 fail closed;触发条件:某 extension 在 `start()`
  保存 Host 并在 reload 后跨代调用 extension HTTP。当前无此类 bundled 消费者;若未来出现,
  需要在 commit 时对 retained dispatcher 重绑新 index。
- F-6:`bind_startup_event_channel` 在 production 未接线,startup 期进程级 custom event 当前不送达;
  属已知产品缺口(README 已宣传该宿主 API),需要产品决策接线或下线,不属于本轮架构清理。
- Windows Job Object 进程树清理只能由新 Windows CI 证明(继承自前轮,非本轮改动引入)。
- 本地候选未推送,远端 CI 仍停留在 d5ccdc87 的红色状态;推送并触发新 CI 前不得宣称多平台验收通过。
