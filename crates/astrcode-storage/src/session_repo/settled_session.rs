use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use astrcode_core::{
    event::{EVENT_FORMAT_VERSION, Phase},
    types::SessionId,
};
use astrcode_session_projection::SessionSummary;
use serde::{Deserialize, Serialize};

use crate::durable_write::spawn_blocking_storage;

pub(super) const FILE_NAME: &str = "settled-session.json";
// Bump when summary projection or startup settlement rules change.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct SettledSession {
    version: u32,
    event_format_version: u32,
    log: LogStamp,
    summary: SessionSummary,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LogStamp {
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    modified: (i64, i64),
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(windows)]
    volume: u64,
    #[cfg(windows)]
    id: [u8; 16],
    #[cfg(windows)]
    modified: i64,
    #[cfg(windows)]
    changed: i64,
}

impl LogStamp {
    #[cfg(unix)]
    fn read(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;

        let meta = fs::metadata(path).ok()?;
        meta.is_file().then(|| Self {
            len: meta.len(),
            device: meta.dev(),
            inode: meta.ino(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }

    #[cfg(windows)]
    fn read(path: &Path) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
        };

        let file = fs::File::open(path).ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() {
            return None;
        }
        let mut basic = FILE_BASIC_INFO::default();
        let mut identity = FILE_ID_INFO::default();
        // SAFETY: both output buffers match the requested information classes and the file owns
        // the handle for both calls.
        let available = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                std::ptr::from_mut(&mut basic).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            ) != 0
                && GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    std::ptr::from_mut(&mut identity).cast(),
                    std::mem::size_of::<FILE_ID_INFO>() as u32,
                ) != 0
        };
        (available && basic.ChangeTime != 0 && identity.FileId.Identifier != [0; 16]).then_some(
            Self {
                len: meta.len(),
                volume: identity.VolumeSerialNumber,
                id: identity.FileId.Identifier,
                modified: basic.LastWriteTime,
                changed: basic.ChangeTime,
            },
        )
    }

    #[cfg(not(any(unix, windows)))]
    fn read(_path: &Path) -> Option<Self> {
        None
    }
}

fn load(path: &Path, session_id: &SessionId) -> Option<SettledSession> {
    let entry: SettledSession = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (entry.version == VERSION
        && entry.event_format_version == EVENT_FORMAT_VERSION
        && &entry.summary.session_id == session_id
        && matches!(entry.summary.phase, Phase::Idle | Phase::Error)
        && entry.summary.latest_cursor.parse::<u64>().is_ok())
    .then_some(entry)
}

pub(super) fn read(dir: &Path, log_path: &Path, session_id: &SessionId) -> Option<SessionSummary> {
    let entry = load(&dir.join(FILE_NAME), session_id)?;
    (LogStamp::read(log_path)? == entry.log).then_some(entry.summary)
}

pub(super) async fn capture_log_stamp(path: &Path) -> Option<LogStamp> {
    let path = path.to_path_buf();
    spawn_blocking_storage("event log stamp", move || Ok(LogStamp::read(&path)))
        .await
        .ok()
        .flatten()
}

pub(super) async fn record(
    dir: &Path,
    log_path: PathBuf,
    before_replay: LogStamp,
    summary: SessionSummary,
) {
    let dir = dir.to_path_buf();
    let result = spawn_blocking_storage("settled session write", move || {
        // A truncation, rewrite, or replacement during replay invalidates this proof.
        if LogStamp::read(&log_path).as_ref() != Some(&before_replay) {
            return Ok(());
        }
        let path = dir.join(FILE_NAME);
        if load(&path, &summary.session_id).is_some_and(|entry| entry.log == before_replay) {
            return Ok(());
        }
        let entry = SettledSession {
            version: VERSION,
            event_format_version: EVENT_FORMAT_VERSION,
            log: before_replay,
            summary,
        };
        // This cache is rebuildable; the log was already synced before replay.
        let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
        temporary.write_all(&serde_json::to_vec(&entry)?)?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    })
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "could not record settled session");
    }
}
