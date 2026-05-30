//! 运行中 turn 的句柄。
//!
//! # Cancel / Abort 分层
//!
//! - **Abort**（用户意图）：停止当前 turn；由 `TurnScheduler::abort` 等上层 API 表达。
//! - **Shutdown**（机制）：协作式停止，通过 [`CancellationToken`] 在 step boundary 收口。
//! - **Force kill**（机制）：grace period 后仍不退出时，额外调用 [`AbortHandle`] 硬杀 task。
//! - **finish_reason**：`aborted` = 用户停止；`interrupted` = stale repair / 进程恢复。
//!
//! `Session::submit` 返回 [`TurnHandle`]；handle 所有权由调用方持有，析构即放弃。

use astrcode_core::types::TurnId;
use tokio::{
    sync::oneshot,
    task::{AbortHandle, JoinHandle},
};
use tokio_util::sync::CancellationToken;

use crate::turn_runner::RunTurnResult;

/// Turn 停止控制句柄（协作式 shutdown + 可选 force kill）。
#[derive(Clone)]
pub struct TurnShutdownHandle {
    cancellation: CancellationToken,
    abort_handle: AbortHandle,
}

impl TurnShutdownHandle {
    pub fn new(cancellation: CancellationToken, abort_handle: AbortHandle) -> Self {
        Self {
            cancellation,
            abort_handle,
        }
    }

    /// 协作式停止：设置 cancellation token，turn 在下一 checkpoint 自行收口。
    pub fn request_shutdown(&self) {
        self.cancellation.cancel();
    }

    /// 强制终止：cancellation + tokio task abort。
    pub fn force_kill(&self) {
        self.cancellation.cancel();
        self.abort_handle.abort();
    }

    pub fn is_finished(&self) -> bool {
        self.abort_handle.is_finished()
    }
}

/// 一次 turn 的运行时句柄。
pub struct TurnHandle {
    turn_id: TurnId,
    join: JoinHandle<()>,
    shutdown_handle: TurnShutdownHandle,
    completion_rx: oneshot::Receiver<RunTurnResult>,
}

impl TurnHandle {
    pub(crate) fn new(
        turn_id: TurnId,
        join: JoinHandle<()>,
        cancellation: CancellationToken,
        completion_rx: oneshot::Receiver<RunTurnResult>,
    ) -> Self {
        let shutdown_handle = TurnShutdownHandle::new(cancellation, join.abort_handle());
        Self {
            turn_id,
            join,
            shutdown_handle,
            completion_rx,
        }
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn is_running(&self) -> bool {
        !self.join.is_finished()
    }

    pub fn shutdown_handle(&self) -> TurnShutdownHandle {
        self.shutdown_handle.clone()
    }

    /// 请求 turn 自行收口。已完成的 handle 上调用是 no-op。
    pub fn request_shutdown(&self) {
        self.shutdown_handle.request_shutdown();
    }

    /// 强制终止后台 task。已完成的 handle 上调用是 no-op。
    pub fn force_kill(&self) {
        self.shutdown_handle.force_kill();
    }

    /// 等待 turn 结束并返回结果。
    ///
    /// 通道被关闭（例如 task panicked）时返回 `None`。
    pub async fn wait(self) -> Option<RunTurnResult> {
        self.completion_rx.await.ok()
    }
}
