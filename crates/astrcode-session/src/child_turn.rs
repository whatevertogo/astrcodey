//! 子 agent turn 的统一生命周期管理。
//!
//! `ChildTurnGuard` 是子 session 生命周期的唯一所有者。正常完成和 abort
//! 都通过同一个 guard。后台任务负责等待 turn 完成、写终态事件到父 session，
//! 并发送信号供 server 层处理回收和通知。

use std::sync::Arc;

use astrcode_core::{event::EventPayload, types::SessionId};
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

use crate::turn_handle::TurnHandle;

/// 子 agent 的完成结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutcome {
    /// 正常完成。
    Completed { summary: String },
    /// 执行失败。
    Failed { error: String },
    /// 被 abort 中断。
    Aborted,
    /// 超时未完成，强制终止。
    TimedOut,
}

/// 子 agent 的清理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildCleanup {
    /// 完成后回收 session（ephemeral agent）。
    Recycle,
    /// 保留 session（persistent agent）。
    Keep,
}

/// 启动子 agent 的参数。
#[derive(Debug, Clone)]
pub struct ChildTurnConfig {
    pub child_session_id: SessionId,
    pub parent_session_id: SessionId,
    pub cleanup: ChildCleanup,
    /// 完成后注入父 session 的通知消息（仅正常完成时）。
    pub notify_on_complete: Option<String>,
}

// ═══════════════════════════════════════════════════════════════
// ChildTurnGuard
// ═══════════════════════════════════════════════════════════════

/// 子 agent turn 的唯一生命周期所有者。
///
/// 内部 spawn 一个后台任务等待 turn 完成，完成后直接写
/// `AgentSessionCompleted` / `AgentSessionFailed` 到父 session，
/// 并通过 `completed_tx` 发信号供 server 层处理回收和通知。
///
/// **first-write-wins**：所有路径统一通过 `try_set_outcome` 写入，
/// `send_if_modified` 保证只有第一次写入生效。
pub struct ChildTurnGuard {
    config: ChildTurnConfig,
    outcome_tx: watch::Sender<Option<ChildOutcome>>,
    outcome_rx: watch::Receiver<Option<ChildOutcome>>,
    _abort_handle: tokio::task::AbortHandle,
}

/// 原子地写入终态。只有第一次写入生效（`send_if_modified` 做 CAS）。
fn try_set_outcome(tx: &watch::Sender<Option<ChildOutcome>>, outcome: ChildOutcome) {
    let _ = tx.send_if_modified(|cur| {
        if cur.is_none() {
            *cur = Some(outcome);
            true
        } else {
            false
        }
    });
}

impl ChildTurnGuard {
    /// 启动子 turn 的后台等待。不阻塞。
    ///
    /// 后台任务完成后自动写 `AgentSessionCompleted` / `AgentSessionFailed` 到父 session。
    /// 同时向 `completed_tx` 发信号供 server 层处理回收和通知。
    pub fn spawn(
        handle: TurnHandle,
        config: ChildTurnConfig,
        parent_session: Arc<crate::Session>,
        completed_tx: mpsc::UnboundedSender<SessionId>,
    ) -> Self {
        let (outcome_tx, outcome_rx) = watch::channel(None);
        let outcome_tx_for_task = outcome_tx.clone();
        let abort_handle = handle.abort_handle();
        let parent_sid = config.parent_session_id.clone();
        let child_sid = config.child_session_id.clone();
        let final_sid = config.child_session_id.clone();

        tokio::spawn(async move {
            let result = handle.wait().await;
            let outcome = match result {
                Some(r) => match r.output {
                    Ok(out) => ChildOutcome::Completed {
                        summary: one_line_summary(&out.text),
                    },
                    Err(e) => ChildOutcome::Failed {
                        error: e.to_string(),
                    },
                },
                None => {
                    // handle.abort() 被调用。后台任务自己也写 Aborted——
                    // 外部 abort() 和这里没有顺序保证，谁先写谁赢。
                    try_set_outcome(&outcome_tx_for_task, ChildOutcome::Aborted);
                    let _ = completed_tx.send(parent_sid);
                    return;
                },
            };
            try_set_outcome(&outcome_tx_for_task, outcome.clone());

            // 直接写终态事件到父 session。notify 消息由 server 层
            // process_child_completions 统一处理，不在此写入。
            match outcome {
                ChildOutcome::Completed { summary } => {
                    let _ = parent_session
                        .emit_durable(
                            None,
                            EventPayload::AgentSessionCompleted {
                                child_session_id: child_sid.clone(),
                                final_session_id: final_sid.clone(),
                                summary,
                            },
                        )
                        .await;
                },
                ChildOutcome::Failed { error } => {
                    let _ = parent_session
                        .emit_durable(
                            None,
                            EventPayload::AgentSessionFailed {
                                child_session_id: child_sid,
                                final_session_id: final_sid,
                                error,
                            },
                        )
                        .await;
                },
                ChildOutcome::Aborted => {},
                ChildOutcome::TimedOut => unreachable!("TimedOut only set by external timeout"),
            }

            let _ = completed_tx.send(parent_sid);
        });

        Self {
            config,
            outcome_tx,
            outcome_rx,
            _abort_handle: abort_handle,
        }
    }

    /// 阻塞等待子 turn 完成（或已被 abort），返回结果。幂等。
    pub async fn outcome(&self) -> ChildOutcome {
        // 快路径
        if let Some(outcome) = self.outcome_rx.borrow().clone() {
            return outcome;
        }
        // 慢路径：clone receiver → wait_for → clone 出值 → drop Ref → drop rx。
        // sender 提前 drop（极少见的 guard 被泄漏场景）时视为 Aborted。
        let mut rx = self.outcome_rx.clone();
        let result = rx.wait_for(|v| v.is_some()).await;
        match result {
            Ok(ref_val) => {
                let val: &Option<ChildOutcome> = &ref_val;
                val.clone().unwrap_or(ChildOutcome::Aborted)
            },
            Err(_) => {
                tracing::warn!(
                    child_session_id = %self.config.child_session_id,
                    "ChildTurnGuard outcome channel closed before outcome set; treating as Aborted"
                );
                ChildOutcome::Aborted
            },
        }
    }

    /// 强制终止子 turn。幂等。
    pub fn abort(&self) {
        self._abort_handle.abort();
        try_set_outcome(&self.outcome_tx, ChildOutcome::Aborted);
    }

    /// 将 outcome 强制设为 TimedOut。仅当 outcome 尚未设置时生效。
    pub fn force_timeout(&self) {
        try_set_outcome(&self.outcome_tx, ChildOutcome::TimedOut);
    }

    pub fn child_session_id(&self) -> &SessionId {
        &self.config.child_session_id
    }

    pub fn parent_session_id(&self) -> &SessionId {
        &self.config.parent_session_id
    }

    pub fn cleanup(&self) -> ChildCleanup {
        self.config.cleanup
    }

    pub fn notify_text(&self) -> Option<&str> {
        self.config.notify_on_complete.as_deref()
    }
}

// ═══════════════════════════════════════════════════════════════
// ChildTurnManager
// ═══════════════════════════════════════════════════════════════

/// Per-session 的子 agent 管理器。
pub struct ChildTurnManager {
    guards: Mutex<Vec<Arc<ChildTurnGuard>>>,
}

impl ChildTurnManager {
    pub fn new() -> Self {
        Self {
            guards: Mutex::new(Vec::new()),
        }
    }

    /// 注册一个子 turn guard。
    pub fn register(&self, guard: Arc<ChildTurnGuard>) {
        self.guards.lock().push(guard);
    }

    /// 收集已完成且尚未处理的子 turn，同时从内部集合中移除。
    ///
    /// `collect` 即 `remove`——终态事件已由 guard 后台任务写入，
    /// 调用方负责处理回收和通知。
    pub fn collect_completed(&self) -> Vec<Arc<ChildTurnGuard>> {
        let mut guards = self.guards.lock();
        let (done, pending): (Vec<_>, Vec<_>) = guards
            .drain(..)
            .partition(|g| g.outcome_rx.borrow().is_some());
        *guards = pending;
        done
    }

    /// 强制终止所有直接子 turn（不递归孙子）。幂等。
    ///
    /// 返回所有被移除的 guard。调用方负责等待 outcome + 写事件。
    /// 孙子 turn 的级联由 `astrcode-server` 的 `cascade_abort_children` 处理。
    pub fn abort_all_direct(&self) -> Vec<Arc<ChildTurnGuard>> {
        let guards: Vec<_> = self.guards.lock().drain(..).collect();
        for guard in &guards {
            guard.abort();
        }
        guards
    }

    /// 是否还有未处理的守卫。
    pub fn has_pending(&self) -> bool {
        !self.guards.lock().is_empty()
    }
}

impl Default for ChildTurnManager {
    fn default() -> Self {
        Self::new()
    }
}

fn one_line_summary(text: &str) -> String {
    astrcode_support::text::compact_inline(text, 159)
}
