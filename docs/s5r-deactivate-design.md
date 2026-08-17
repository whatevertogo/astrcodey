# S5R 扩展停用(deactivate)协议设计草案

> 状态:草案(2026-08-18)。前置已落地:worker `on_shutdown`(driver 结束后的
> best-effort 本地清理钩子)与宿主 stop 编排改造(`s5r_ext/v3_session.rs`:先等
> worker 自愿退出,超时后再 SIGTERM)。

## 问题

`on_activate` 在协议里没有对称物。宿主停止扩展(`Extension::stop` →
`S5rSession::shutdown`)的完整动作是:关闭 admission、取消 driver、等 worker 自愿
退出、SIGTERM。这意味着:

1. **worker 无法感知"为什么被停"**:`StopReason`(reload / disabled / shutdown /
   startup_failed)只存在于宿主侧,worker 拿不到;
2. **清理钩子里没有宿主能力**:`on_shutdown` 运行在 driver 结束之后,task-local
   `HostClient` 已不可用,想 flush 状态只能写本地文件,不能经
   `session_state`/workspace 等 host 域落盘;
3. **窗口是尽力而为**:自愿退出窗口超时即 SIGTERM,慢清理被截断且无从察觉。

## 目标

- 停用前给 worker 一次**有界、可应答**的 deactivate 调用,携带 reason;
- 钩子执行期间 `HostClient` 可用(普通 invoke 作用域);
- 协议向后兼容:旧 worker / 旧宿主任意一方不支持时,退化为现有
  EOF + drain + terminate 流程;
- deactivate 永不阻塞 stop 超过显式上界。

## 非目标

- 不改变 in-process bundled 扩展的 `Extension::stop(ExtensionStopContext)` 契约;
- 不引入"worker 拒绝停用"的语义:deactivate 是通知,不是协商。

## 方案选型

### 方案 A:wire 级 deactivate invoke(推荐)

握手协商一个新 feature,宿主在停止编排的最前面发出一次普通 invoke。

**协议面**(`astrcode-extension-sdk::wire`):

- 新增 `FeatureName::deactivate_v1()`,worker 在 `supported_features` 声明,
  宿主只与协商成功的 worker 使用该路径;
- 新增常量 `CAP_RUNTIME_DEACTIVATE`(与 `CAP_RUNTIME_PING` 同级),payload:
  ```json
  { "reason": "reload" }
  ```
  `reason` 为 `StopReason` 的 snake_case 线值,未知值按 `shutdown` 处理
  (向前兼容宿主未来新增 reason);
- 应答:空输出即成功;错误应答只影响日志,不影响终止流程。

**宿主编排**(`s5r_ext/v3_session.rs::shutdown`):

1. 若协商了 `deactivate_v1`:以 2s 超时 invoke `CAP_RUNTIME_DEACTIVATE`
   (携带 reason,经 `invoke_handler_unadmitted` 特批,绕过已关闭的 admission);
2. 关闭 admission、取消 driver(现有);
3. 等自愿退出(现有 drain 窗口);
4. terminate(现有)。

reason 的来源:`S5rSession::shutdown` 目前不接收 `ExtensionStopContext`,需要把
`StopReason` 从 `S5rExtension::stop` 透传进来(参数化,不改 trait)。

**worker 面**(`astrcode-extension-worker`):

```rust
worker.on_deactivate(|input: DeactivateInput, ctx: WorkerCallContext| async move {
    // 普通 invoke 作用域:HostClient 可用,ctx.cancel_token() 受 2s 上限约束
    HostClient::session_state().write(...).await?;
    Ok(())
});
```

- 注册 `on_deactivate` 的 worker 自动在 manifest `supported_features` 追加
  `deactivate_v1`(与 `custom_event_v1` 等既有 feature 的声明方式一致);
- `on_shutdown` 保留,语义收窄为"driver 结束后的本地兜底清理",在
  `on_deactivate` 之后运行;未协商 deactivate 的旧宿主上,`on_deactivate`
  不触发,只有 `on_shutdown`。

**错误与超时**:

- deactivate invoke 超时/失败/未知 capability → `tracing::warn` 后继续现有终止流程;
- 钩子内 panic/挂起由 invoke 的 cancel token + 2s 超时兜底;
- 总 stop 上界:deactivate(2s)+ drain(2s)+ terminate grace(2s),
  与现状同数量级,仅在协商成功时增加 deactivate 一项。

**死锁评估**:钩子内调用 `HostClient` 会走 nested invoke 回宿主。宿主此时在
stop 路径上,必须确认 `HostRouter` 的 invoke 在 shutdown 期间仍被服务
(host driver 在 deactivate 应答前保持运行)。若 HostRouter 的会话域后端已随
runner 停机而关闭,对应操作返回 `BackendUnavailable` 即可——deactivate 的可用域
集合需要在实施时逐个确认并写进作者文档。

### 方案 B:EOF + drain 窗口(已落地,保留为兜底)

即当前实现:driver 结束 → stdin EOF → worker `on_shutdown` → 自愿退出窗口 →
SIGTERM。成本为零,但钩子无 reason、无 host API、无应答。方案 A 落地后它仍是不
支持 deactivate_v1 时的唯一路径,也是 worker 进程退出前的最后一道清理机会。

### 方案 C:复用 lifecycle hook(不推荐)

把停用建模为 `LifecycleEvent::ExtensionShutdown`。问题:lifecycle hook 走 session
作用域与 manifest 订阅,语义是"某 session 的生命周期",与"扩展代(generation)被
停用"正交;且 hook 注册发生在握手 manifest,停用通知需要的是运行时单次调用,
两种生命周期不匹配,会把 registry 的 dispatch 模型弄复杂。

## 建议路径

1. wire:`FeatureName::deactivate_v1` + `CAP_RUNTIME_DEACTIVATE` +
   `DeactivateInput` DTO(`s5r::hooks` 旁),含未知 reason 的容错测试;
2. 宿主:`S5rExtension::stop` 透传 `StopReason`,`shutdown` 编排插入 deactivate
   步骤,单测覆盖"协商/未协商/超时/错误应答"四种分支;
3. worker:`Worker::on_deactivate` + manifest feature 自动声明 + dispatch;
4. e2e:s5r-guest 注册 `on_deactivate`,经 `HostClient` 写 `session_state`,
   断言 runner shutdown 后内容落盘且 reason 正确;
5. 文档:`docs/extension-author-guide.md` 增加 deactivate 语义、可用 host 域清单、
   与 `on_shutdown` 的分工。

## 风险

- **deactivate 内的 host 调用死锁/超时放大**:stop 路径上宿主部分后端可能已停,
  需要实施时逐个域确认并降级为 typed 错误,而不是挂起到超时;
- **stop 延迟回归**:协商成功后 stop 最坏情况多 2s;runner 批量停机时是串行
  累加,需要评估是否按扩展并行 deactivate;
- **reason 语义膨胀**:未来新增 `StopReason` 时线值集合扩大,旧 worker 必须容错
  (已在 DTO 层按 `shutdown` 兜底)。
