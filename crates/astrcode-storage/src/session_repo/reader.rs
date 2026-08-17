//! 读端口实现:[`EventReader`] 与 [`SessionReader`]。

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use astrcode_core::{
    event::StoredEvent,
    types::{Cursor, SessionId},
};
use astrcode_session_projection::{SessionReadModel, SessionSummary, replay};
use tokio::sync::Semaphore;

use super::{
    FileSystemSessionRepository, corrupt_projection, parse_cursor, validate_storage_session_id,
};
use crate::{EventReader, SessionReader, StorageError, event_log::EventLog};

/// 冷会话摘要读取的并发上限:每次读取伴随一次 fsync,并发不能无界。
const SUMMARY_READ_CONCURRENCY: usize = 8;

#[async_trait::async_trait]
impl EventReader for FileSystemSessionRepository {
    async fn replay_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        meta.log.replay_all().await
    }

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let cursor = meta.projection.snapshot().await.cursor();
        Ok(Some(cursor))
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let seq = parse_cursor(cursor)?;
        meta.log.replay_after(seq).await
    }

    async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        let seq = parse_cursor(cursor)?;
        meta.log.replay_after_limited(seq, max_events).await
    }

    async fn replay_from_start_limited(
        &self,
        session_id: &SessionId,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        let _permit = meta.acquire_confirmed_commit_lane(session_id).await?;
        meta.log.replay_from_start_limited(max_events).await
    }

    async fn replay_events_active_or_recycled(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        match self.replay_events(session_id).await {
            Ok(events) => Ok(events),
            Err(StorageError::NotFound(_)) => self.recycled_events(session_id).await,
            Err(error) => Err(error),
        }
    }

    async fn replay_from_active_or_recycled_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        match self
            .replay_from_limited(session_id, cursor, max_events)
            .await
        {
            Ok(events) => Ok(events),
            Err(StorageError::NotFound(_)) => {
                let seq = parse_cursor(cursor)?;
                Ok(self
                    .recycled_events(session_id)
                    .await?
                    .into_iter()
                    .filter(|event| event.seq > seq)
                    .take(max_events)
                    .collect())
            },
            Err(error) => Err(error),
        }
    }

    async fn replay_from_start_active_or_recycled_limited(
        &self,
        session_id: &SessionId,
        max_events: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        match self.replay_from_start_limited(session_id, max_events).await {
            Ok(events) => Ok(events),
            Err(StorageError::NotFound(_)) => Ok(self
                .recycled_events(session_id)
                .await?
                .into_iter()
                .take(max_events)
                .collect()),
            Err(error) => Err(error),
        }
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        Ok(self
            .list_root_session_locations()
            .await
            .into_keys()
            .collect())
    }

    async fn list_all_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        Ok(self
            .list_all_session_locations()
            .await?
            .into_keys()
            .collect())
    }
}

#[async_trait::async_trait]
impl SessionReader for FileSystemSessionRepository {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        let meta = self.get_or_open_meta(session_id).await?;
        Ok(meta.projection.snapshot().await)
    }

    async fn recycled_session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        validate_storage_session_id(session_id)?;
        let recycled_dir = self
            .find_recycled_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let events =
            EventLog::replay_read_only(Self::event_log_path(&recycled_dir, session_id)).await?;
        let model = replay(session_id.clone(), &events).map_err(corrupt_projection)?;
        Ok(Arc::new(model))
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let locations = self.list_root_session_locations().await;
        self.summaries_for_locations(locations).await
    }

    async fn list_all_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let locations = self.list_all_session_locations().await?;
        self.summaries_for_locations(locations).await
    }
}

impl FileSystemSessionRepository {
    async fn recycled_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        validate_storage_session_id(session_id)?;
        let recycled_dir = self
            .find_recycled_session_dir(session_id)
            .await
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        EventLog::replay_read_only(Self::event_log_path(&recycled_dir, session_id)).await
    }

    async fn summaries_for_locations(
        &self,
        locations: BTreeMap<SessionId, PathBuf>,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        let mut summaries = Vec::with_capacity(locations.len());
        let mut cold = Vec::new();
        for (session_id, session_dir) in locations {
            match self.sessions.read().await.get(&session_id).cloned() {
                // 已打开的会话直接使用内存中的投影
                Some(meta) => summaries.push(meta.projection.snapshot().await.to_summary()),
                None => cold.push((session_id, session_dir)),
            }
        }
        summaries.extend(read_summaries_from_logs(cold).await?);
        summaries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(summaries)
    }
}

/// 从事件流构造轻量摘要,不构造完整 transcript;冷会话限流并发扫描。
///
/// 解码失败(旧格式或损坏的日志)只跳过该会话并告警:列表边界必须隔离
/// 单会话失败,不能让一个坏日志拖垮整个枚举;直接打开该会话仍按严格解码失败。
/// IO/durability 错误必须传播:跳过会把未确认的写入伪装成会话不存在。
async fn read_summaries_from_logs(
    cold: Vec<(SessionId, PathBuf)>,
) -> Result<Vec<SessionSummary>, StorageError> {
    let concurrency = Arc::new(Semaphore::new(SUMMARY_READ_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    for (session_id, session_dir) in cold {
        let concurrency = Arc::clone(&concurrency);
        tasks.spawn(async move {
            let _permit = concurrency
                .acquire_owned()
                .await
                .map_err(|_| StorageError::Io(std::io::Error::other("summary read lane closed")))?;
            let log_path = FileSystemSessionRepository::event_log_path(&session_dir, &session_id);
            EventLog::read_summary(&log_path, session_id).await
        });
    }

    let mut summaries = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let outcome = joined.map_err(|error| {
            StorageError::Io(std::io::Error::other(format!(
                "summary reader stopped: {error}"
            )))
        })?;
        match outcome {
            Ok(Some(summary)) => summaries.push(summary),
            Ok(None) => {},
            Err(error @ (StorageError::CorruptLog(_) | StorageError::Serialization(_))) => {
                tracing::warn!(%error, "skipping undecodable session event log during listing");
            },
            Err(error) => return Err(error),
        }
    }
    Ok(summaries)
}
