#[cfg(not(windows))]
use std::fs::File;
use std::{
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use astrcode_core::StoredEvent;
use astrcode_host_session::ports::{RecoveredSessionState, SessionRecoveryCheckpoint};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    iterator::EventLogIterator,
    paths::{
        checkpoint_snapshot_path, checkpoint_snapshot_path_from_projects_root,
        latest_checkpoint_marker_path, latest_checkpoint_marker_path_from_projects_root,
        resolve_existing_session_path, resolve_existing_session_path_from_projects_root,
        snapshots_dir, snapshots_dir_from_projects_root,
    },
};
use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LatestCheckpointMarker {
    checkpoint_storage_seq: u64,
    file_name: String,
}

pub(crate) fn recover_session(
    projects_root: Option<&Path>,
    session_id: &str,
) -> Result<RecoveredSessionState> {
    let event_log_path = match projects_root {
        Some(projects_root) => {
            resolve_existing_session_path_from_projects_root(projects_root, session_id)?
        },
        None => resolve_existing_session_path(session_id)?,
    };
    let Some(checkpoint) = load_active_checkpoint(projects_root, session_id)? else {
        return Ok(RecoveredSessionState {
            checkpoint: None,
            tail_events: EventLogIterator::from_path(&event_log_path)?
                .collect::<Result<Vec<_>>>()?,
        });
    };
    let tail_events = EventLogIterator::from_path(&event_log_path)?
        .filter_map(|result| match result {
            Ok(stored) if stored.storage_seq > checkpoint.checkpoint_storage_seq => {
                Some(Ok(stored))
            },
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecoveredSessionState {
        checkpoint: Some(checkpoint),
        tail_events,
    })
}

pub(crate) fn persist_checkpoint(
    projects_root: Option<&Path>,
    event_log_path: &Path,
    session_id: &str,
    checkpoint: &SessionRecoveryCheckpoint,
) -> Result<()> {
    let snapshot_dir = match projects_root {
        Some(projects_root) => snapshots_dir_from_projects_root(projects_root, session_id)?,
        None => snapshots_dir(session_id)?,
    };
    fs::create_dir_all(&snapshot_dir).map_err(|error| {
        crate::io_error(
            format!(
                "failed to create snapshots directory '{}'",
                snapshot_dir.display()
            ),
            error,
        )
    })?;

    let checkpoint_path = match projects_root {
        Some(projects_root) => checkpoint_snapshot_path_from_projects_root(
            projects_root,
            session_id,
            checkpoint.checkpoint_storage_seq,
        )?,
        None => checkpoint_snapshot_path(session_id, checkpoint.checkpoint_storage_seq)?,
    };
    let marker_path = match projects_root {
        Some(projects_root) => {
            latest_checkpoint_marker_path_from_projects_root(projects_root, session_id)?
        },
        None => latest_checkpoint_marker_path(session_id)?,
    };
    let snapshot_tmp = temp_path(&checkpoint_path, "snapshot");
    let marker_tmp = temp_path(&marker_path, "marker");
    let log_tmp = temp_path(event_log_path, "rewrite");
    let log_backup = event_log_path.with_extension("jsonl.bak");

    let tail_events = EventLogIterator::from_path(event_log_path)?
        .filter_map(|result| match result {
            Ok(stored) if stored.storage_seq > checkpoint.checkpoint_storage_seq => {
                Some(Ok(stored))
            },
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;

    write_json_file(&snapshot_tmp, checkpoint)?;
    write_json_file(
        &marker_tmp,
        &LatestCheckpointMarker {
            checkpoint_storage_seq: checkpoint.checkpoint_storage_seq,
            file_name: checkpoint_path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    crate::internal_io_error(format!(
                        "checkpoint path '{}' has no valid file name",
                        checkpoint_path.display()
                    ))
                })?
                .to_string(),
        },
    )?;
    write_events_file(&log_tmp, &tail_events)?;

    fs::rename(&snapshot_tmp, &checkpoint_path).map_err(|error| {
        crate::io_error(
            format!(
                "failed to commit checkpoint snapshot '{}' -> '{}'",
                snapshot_tmp.display(),
                checkpoint_path.display()
            ),
            error,
        )
    })?;
    sync_dir(&snapshot_dir)?;

    replace_file(&marker_tmp, &marker_path)?;
    sync_dir(&snapshot_dir)?;

    if log_backup.exists() {
        fs::remove_file(&log_backup).map_err(|error| {
            crate::io_error(
                format!(
                    "failed to remove stale log backup '{}'",
                    log_backup.display()
                ),
                error,
            )
        })?;
    }

    fs::rename(event_log_path, &log_backup).map_err(|error| {
        crate::io_error(
            format!(
                "failed to rotate event log '{}' -> '{}'",
                event_log_path.display(),
                log_backup.display()
            ),
            error,
        )
    })?;
    if let Err(error) = fs::rename(&log_tmp, event_log_path) {
        restore_rotated_event_log(&log_backup, event_log_path).map_err(|restore_error| {
            crate::internal_io_error(format!(
                "failed to restore rotated event log '{}' -> '{}' after promote failure: {}",
                log_backup.display(),
                event_log_path.display(),
                restore_error
            ))
        })?;
        return Err(crate::io_error(
            format!(
                "failed to promote rewritten event log '{}' -> '{}'",
                log_tmp.display(),
                event_log_path.display()
            ),
            error,
        ));
    }
    sync_dir(
        event_log_path
            .parent()
            .ok_or_else(|| crate::internal_io_error("event log path missing parent"))?,
    )?;

    fs::remove_file(&log_backup).map_err(|error| {
        crate::io_error(
            format!(
                "failed to remove old event log backup '{}'",
                log_backup.display()
            ),
            error,
        )
    })?;

    Ok(())
}

fn load_active_checkpoint(
    projects_root: Option<&Path>,
    session_id: &str,
) -> Result<Option<SessionRecoveryCheckpoint>> {
    let marker_path = match projects_root {
        Some(projects_root) => {
            latest_checkpoint_marker_path_from_projects_root(projects_root, session_id)?
        },
        None => latest_checkpoint_marker_path(session_id)?,
    };
    if !marker_path.exists() {
        return Ok(None);
    }
    let marker = read_json_file::<LatestCheckpointMarker>(&marker_path)?;
    let snapshot_dir = match projects_root {
        Some(projects_root) => snapshots_dir_from_projects_root(projects_root, session_id)?,
        None => snapshots_dir(session_id)?,
    };
    let checkpoint_path = snapshot_dir.join(marker.file_name);
    if !checkpoint_path.exists() {
        return Err(crate::internal_io_error(format!(
            "checkpoint marker '{}' points to missing snapshot '{}'",
            marker_path.display(),
            checkpoint_path.display()
        )));
    }
    Ok(Some(read_json_file(&checkpoint_path)?))
}

fn read_json_file<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path)
        .map_err(|error| crate::io_error(format!("failed to read '{}'", path.display()), error))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| crate::parse_error(format!("failed to parse '{}'", path.display()), error))
}

fn write_json_file<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| crate::io_error(format!("failed to open '{}'", path.display()), error))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        crate::parse_error(format!("failed to serialize '{}'", path.display()), error)
    })?;
    writer
        .flush()
        .map_err(|error| crate::io_error(format!("failed to flush '{}'", path.display()), error))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| crate::io_error(format!("failed to sync '{}'", path.display()), error))?;
    sync_dir(
        path.parent()
            .ok_or_else(|| crate::internal_io_error("json file path missing parent"))?,
    )?;
    Ok(())
}

fn write_events_file(path: &Path, events: &[StoredEvent]) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| crate::io_error(format!("failed to open '{}'", path.display()), error))?;
    let mut writer = BufWriter::new(file);
    for stored in events {
        serde_json::to_writer(&mut writer, stored).map_err(|error| {
            crate::parse_error(format!("failed to serialize '{}'", path.display()), error)
        })?;
        writeln!(writer).map_err(|error| {
            crate::io_error(format!("failed to write '{}'", path.display()), error)
        })?;
    }
    writer
        .flush()
        .map_err(|error| crate::io_error(format!("failed to flush '{}'", path.display()), error))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| crate::io_error(format!("failed to sync '{}'", path.display()), error))?;
    sync_dir(
        path.parent()
            .ok_or_else(|| crate::internal_io_error("event file path missing parent"))?,
    )?;
    Ok(())
}

fn replace_file(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        fs::remove_file(to).map_err(|error| {
            crate::io_error(
                format!("failed to remove existing file '{}'", to.display()),
                error,
            )
        })?;
    }
    fs::rename(from, to).map_err(|error| {
        crate::io_error(
            format!(
                "failed to rename '{}' -> '{}'",
                from.display(),
                to.display()
            ),
            error,
        )
    })?;
    Ok(())
}

fn restore_rotated_event_log(log_backup: &Path, event_log_path: &Path) -> Result<()> {
    if event_log_path.exists() {
        fs::remove_file(event_log_path).map_err(|error| {
            crate::io_error(
                format!(
                    "failed to remove partially promoted event log '{}'",
                    event_log_path.display()
                ),
                error,
            )
        })?;
    }
    fs::rename(log_backup, event_log_path).map_err(|error| {
        crate::io_error(
            format!(
                "failed to restore event log backup '{}' -> '{}'",
                log_backup.display(),
                event_log_path.display()
            ),
            error,
        )
    })?;
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // TODO: Windows 目录刷盘目前只能 best-effort 跳过；后续需要补平台专用实现，
        // 以收紧 checkpoint/rename 在断电或进程崩溃场景下的元数据持久化保证。
        let _ = path;
        Ok(())
    }

    #[cfg(not(windows))]
    let dir = File::open(path).map_err(|error| {
        crate::io_error(
            format!("failed to open directory '{}'", path.display()),
            error,
        )
    })?;
    #[cfg(not(windows))]
    {
        dir.sync_all().map_err(|error| {
            crate::io_error(
                format!("failed to sync directory '{}'", path.display()),
                error,
            )
        })
    }
}

fn temp_path(base: &Path, label: &str) -> PathBuf {
    let suffix = Uuid::new_v4();
    let file_name = base
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("checkpoint");
    base.with_file_name(format!("{file_name}.{label}.{suffix}.tmp"))
}
