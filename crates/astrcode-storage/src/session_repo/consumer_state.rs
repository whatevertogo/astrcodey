//! Durable event consumer 状态:`event-consumers/<sha256(consumer_id)>.state.json`。
//!
//! 每个 consumer 一个状态文件,read-modify-write 周期在 commit lane 与
//! consumer state lane 双重串行下执行(见 [`SessionMeta::edit_consumer_state`])。

use std::path::{Path, PathBuf};

use astrcode_core::types::SessionId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::SessionMeta;
use crate::{EventConsumerState, StorageError, durable_write::replace_durable_file};

const EVENT_CONSUMER_STATE_VERSION: u8 = 3;

pub(super) fn event_consumer_state_path(
    session_dir: &Path,
    consumer_id: &str,
) -> Result<PathBuf, StorageError> {
    crate::traits::validate_event_consumer_id(consumer_id)?;
    let digest = Sha256::digest(consumer_id.as_bytes());
    Ok(session_dir
        .join("event-consumers")
        .join(format!("{digest:x}.state.json")))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedEventConsumerState {
    version: u8,
    cursor: Option<u64>,
    paused: bool,
    revision: u64,
    consecutive_failures: u32,
    quarantined_count: u64,
    skipped_count: u64,
    quarantined: Vec<crate::EventConsumerQuarantine>,
    skips: Vec<crate::EventConsumerSkip>,
}

impl TryFrom<PersistedEventConsumerState> for EventConsumerState {
    type Error = StorageError;

    fn try_from(state: PersistedEventConsumerState) -> Result<Self, Self::Error> {
        let state = Self {
            checkpoint: state.cursor,
            paused: state.paused,
            revision: state.revision,
            consecutive_failures: state.consecutive_failures,
            quarantined_count: state.quarantined_count,
            skipped_count: state.skipped_count,
            quarantined: state.quarantined,
            skips: state.skips,
        };
        state.validate_audit_bounds()?;
        Ok(state)
    }
}

impl From<&EventConsumerState> for PersistedEventConsumerState {
    fn from(state: &EventConsumerState) -> Self {
        Self {
            version: EVENT_CONSUMER_STATE_VERSION,
            cursor: state.checkpoint,
            paused: state.paused,
            revision: state.revision,
            consecutive_failures: state.consecutive_failures,
            quarantined_count: state.quarantined_count,
            skipped_count: state.skipped_count,
            quarantined: state.quarantined.clone(),
            skips: state.skips.clone(),
        }
    }
}

pub(super) async fn read_event_consumer_state(
    path: &Path,
) -> Result<EventConsumerState, StorageError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EventConsumerState::default());
        },
        Err(error) => return Err(StorageError::Io(error)),
    };
    let state = serde_json::from_slice::<PersistedEventConsumerState>(&bytes).map_err(|error| {
        StorageError::CorruptLog(format!(
            "invalid event consumer state {}: {error}",
            path.display()
        ))
    })?;
    if state.version != EVENT_CONSUMER_STATE_VERSION {
        return Err(StorageError::CorruptLog(format!(
            "unsupported event consumer state version {} in {}",
            state.version,
            path.display()
        )));
    }
    state.try_into()
}

pub(super) async fn write_event_consumer_state(
    path: &Path,
    state: &EventConsumerState,
) -> Result<(), StorageError> {
    state.validate_audit_bounds()?;
    let bytes = serde_json::to_vec(&PersistedEventConsumerState::from(state))?;
    let path = path.to_owned();
    crate::durable_write::spawn_blocking_storage("event consumer state writer", move || {
        replace_durable_file(&path, &bytes).map_err(StorageError::Io)
    })
    .await
}

/// consumer 状态编辑的结果:`Changed` 先把变更后的状态落盘再返回,
/// `Unchanged` 直接返回、不写磁盘。
pub(super) enum ConsumerStateEdit<T> {
    Changed(T),
    Unchanged(T),
}

impl SessionMeta {
    /// 在 commit lane 与 consumer state lane 下执行一个 consumer 状态的
    /// read-modify-write 周期。
    ///
    /// `seq_bound` 给出错误文案名词与上界 seq:越过当前事件日志末尾的
    /// checkpoint/failure 直接拒绝。闭包收到可变的内存状态与当前事件数。
    pub(super) async fn edit_consumer_state<T>(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        seq_bound: Option<(&'static str, u64)>,
        edit: impl FnOnce(&mut EventConsumerState, u64) -> Result<ConsumerStateEdit<T>, StorageError>,
    ) -> Result<T, StorageError> {
        let _commit_permit = self.acquire_confirmed_commit_lane(session_id).await?;
        let _consumer_permit = self.consumer_state_lane.acquire().await.map_err(|_| {
            StorageError::Io(std::io::Error::other("event consumer state lane closed"))
        })?;
        let event_count = self.log.count();
        if let Some((noun, seq)) = seq_bound
            && seq >= event_count
        {
            return Err(StorageError::InvalidId(format!(
                "event consumer {noun} {seq} is beyond the session event log"
            )));
        }
        let path = event_consumer_state_path(&self.dir, consumer_id)?;
        let mut state = read_event_consumer_state(&path).await?;
        match edit(&mut state, event_count)? {
            ConsumerStateEdit::Changed(value) => {
                write_event_consumer_state(&path, &state).await?;
                Ok(value)
            },
            ConsumerStateEdit::Unchanged(value) => Ok(value),
        }
    }
}
