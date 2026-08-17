//! fsync 结果不确定时的 sticky durability 状态机。
//!
//! 一次 fsync 返回模糊失败后,该会话不得继续写入新事件,直到通过
//! `retry_uncertain_sync` 精确确认或重新打开。marker 的读取与变更都只发生在
//! 持有 commit lane 的上下文里,因此这里的天真锁内操作不存在并发竞争。

use astrcode_core::{event::StoredEvent, types::SessionId};
use astrcode_session_projection::PreparedProjectionBatch;

use super::SessionMeta;
use crate::StorageError;

/// Initial reason recorded on a durability marker while fsync is in flight.
pub(super) const FSYNC_CONFIRMATION_PENDING: &str = "fsync confirmation is pending";

pub(super) enum UncertainDurability {
    PendingProjection {
        batch: PreparedProjectionBatch,
        through_seq: u64,
        reason: String,
    },
    Published {
        through_seq: u64,
        reason: String,
    },
}

impl UncertainDurability {
    const fn through_seq(&self) -> u64 {
        match self {
            Self::PendingProjection { through_seq, .. } | Self::Published { through_seq, .. } => {
                *through_seq
            },
        }
    }

    fn reason(&self) -> &str {
        match self {
            Self::PendingProjection { reason, .. } | Self::Published { reason, .. } => reason,
        }
    }

    fn set_reason(&mut self, reason: String) {
        match self {
            Self::PendingProjection {
                reason: current, ..
            }
            | Self::Published {
                reason: current, ..
            } => *current = reason,
        }
    }

    pub(super) fn error(&self, session_id: &SessionId) -> StorageError {
        StorageError::DurabilityUncertain {
            session_id: session_id.clone(),
            through_seq: self.through_seq(),
            reason: self.reason().to_owned(),
        }
    }
}

pub(super) fn uncertain_seq_mismatch(expected: u64, actual: u64) -> StorageError {
    StorageError::InvalidEvent(format!(
        "durability confirmation expected pending seq {expected}, found {actual}"
    ))
}

pub(super) fn sequence_overflow() -> StorageError {
    StorageError::CorruptLog("session event sequence overflow".into())
}

pub(super) fn set_uncertain(meta: &SessionMeta, uncertain: UncertainDurability) {
    *meta.uncertain_durability.lock() = Some(uncertain);
}

fn restore_uncertain(meta: &SessionMeta, uncertain: UncertainDurability) {
    set_uncertain(meta, uncertain);
}

pub(super) fn pending_through_seq(meta: &SessionMeta) -> Option<u64> {
    meta.uncertain_durability
        .lock()
        .as_ref()
        .map(UncertainDurability::through_seq)
}

/// fsync 失败后原地更新 marker 的原因,并返回对应的 sticky 错误。
pub(super) fn fail_uncertain(
    meta: &SessionMeta,
    session_id: &SessionId,
    expected_through_seq: u64,
    reason: String,
) -> StorageError {
    let mut state = meta.uncertain_durability.lock();
    let Some(uncertain) = state.as_mut() else {
        return StorageError::CorruptLog(
            "durability marker disappeared before fsync completed".into(),
        );
    };
    if uncertain.through_seq() != expected_through_seq {
        return uncertain_seq_mismatch(expected_through_seq, uncertain.through_seq());
    }
    uncertain.set_reason(reason);
    uncertain.error(session_id)
}

/// 取出精确匹配 `expected_through_seq` 的 marker;不匹配时原样放回。
pub(super) fn take_uncertain(
    meta: &SessionMeta,
    expected_through_seq: u64,
) -> Result<UncertainDurability, StorageError> {
    let mut state = meta.uncertain_durability.lock();
    match state.take() {
        Some(uncertain) if uncertain.through_seq() == expected_through_seq => Ok(uncertain),
        Some(uncertain) => {
            let actual_through_seq = uncertain.through_seq();
            // Put the marker back: only the expected boundary may be taken.
            *state = Some(uncertain);
            Err(uncertain_seq_mismatch(
                expected_through_seq,
                actual_through_seq,
            ))
        },
        None => Err(StorageError::CorruptLog(
            "durability marker disappeared before confirmation completed".into(),
        )),
    }
}

/// The event log must sit exactly at the batch's first seq before an append.
pub(super) async fn ensure_log_positioned_for(
    meta: &SessionMeta,
    prepared: &PreparedProjectionBatch,
) -> Result<(), StorageError> {
    let expected_seq = prepared.first_seq();
    let log_next_seq = meta.log.count();
    if log_next_seq != expected_seq {
        return Err(StorageError::CorruptLog(format!(
            "event log next seq {log_next_seq} does not match projection next seq {expected_seq}"
        )));
    }
    Ok(())
}

/// fsync 确认后,把 pending 批次发布到读模型并清除 marker。
///
/// marker 被整体取出后再校验日志与投影位置,避免克隆批次;校验失败属于
/// corruption,marker 原样放回以保持 sticky。
pub(super) async fn apply_confirmed_projection(
    meta: &SessionMeta,
    expected_through_seq: u64,
) -> Result<Vec<StoredEvent>, StorageError> {
    let uncertain = take_uncertain(meta, expected_through_seq)?;
    let UncertainDurability::PendingProjection {
        batch,
        through_seq,
        reason,
    } = uncertain
    else {
        // Published batches were already visible; only the marker needed cleanup.
        return Ok(Vec::new());
    };

    // A live pending marker implies its append already committed, so the log
    // must sit exactly past the batch; anything else is corruption.
    let log_next_seq = meta.log.count();
    let expected_next_seq = through_seq.checked_add(1).ok_or_else(sequence_overflow)?;
    if log_next_seq != expected_next_seq {
        restore_uncertain(
            meta,
            UncertainDurability::PendingProjection {
                batch,
                through_seq,
                reason,
            },
        );
        return Err(StorageError::CorruptLog(format!(
            "event log next seq {log_next_seq} does not match pending durability boundary \
             {expected_next_seq}"
        )));
    }

    let projection_next_seq = meta
        .projection
        .snapshot()
        .await
        .stats
        .last_seq
        .checked_add(1)
        .ok_or_else(sequence_overflow)?;
    // The commit lane blocks other appends while the marker is alive, so the
    // projection cannot have advanced past the pending batch.
    if projection_next_seq != batch.first_seq() {
        let first_seq = batch.first_seq();
        restore_uncertain(
            meta,
            UncertainDurability::PendingProjection {
                batch,
                through_seq,
                reason,
            },
        );
        return Err(StorageError::CorruptLog(format!(
            "projection next seq {projection_next_seq} does not match pending batch start \
             {first_seq}"
        )));
    }

    Ok(meta.projection.apply_committed(batch).await)
}
