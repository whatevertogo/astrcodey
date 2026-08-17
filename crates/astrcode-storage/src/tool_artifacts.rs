//! Tool result artifact file helpers.

use std::{
    fs::{self, File},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Component, Path},
};

use astrcode_core::tool::ToolResultArtifactSlice;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{ToolResultArtifactInput, ToolResultArtifactRef, durable_write::sync_directory};

/// 同一工具结果文件名碰撞时的最大尝试后缀数（磁盘与内存实现共享）。
pub(crate) const MAX_ARTIFACT_NAME_COLLISIONS: usize = 1000;

pub(crate) fn validate_tool_result_artifact_id(artifact_id: &str) -> Result<(), &'static str> {
    let mut components = Path::new(artifact_id).components();
    if artifact_id.is_empty()
        || artifact_id.len() > 255
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err("artifact id must be one non-empty file-name component");
    }
    let Some(stem) = artifact_id
        .strip_prefix("result-")
        .and_then(|value| value.strip_suffix(".txt"))
    else {
        return Err("artifact id has an invalid format");
    };
    let (digest, suffix) = stem
        .split_once('-')
        .map_or((stem, None), |(digest, suffix)| (digest, Some(suffix)));
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || suffix.is_some_and(|suffix| {
            suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err("artifact id has an invalid format");
    }
    Ok(())
}

pub(crate) fn tool_result_artifact_id(tool_name: &str, call_id: &str, suffix: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update([0]);
    hasher.update(call_id.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    if suffix == 0 {
        format!("result-{digest}.txt")
    } else {
        format!("result-{digest}-{suffix}.txt")
    }
}

pub(crate) fn write_tool_result_file(
    dir: &Path,
    input: &ToolResultArtifactInput,
) -> std::io::Result<ToolResultArtifactRef> {
    std::fs::create_dir_all(dir)?;
    let mut temporary = NamedTempFile::new_in(dir)?;
    temporary.write_all(input.content.as_bytes())?;
    temporary.as_file().sync_all()?;

    for suffix in 0..MAX_ARTIFACT_NAME_COLLISIONS {
        let file_name = tool_result_artifact_id(&input.tool_name, &input.call_id, suffix);
        let path = dir.join(&file_name);
        match temporary.persist_noclobber(&path) {
            Ok(file) => {
                file.sync_all()?;
                sync_artifact_directories(dir)?;
                return Ok(ToolResultArtifactRef {
                    bytes: input.content.len(),
                    artifact_id: file_name,
                });
            },
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
                temporary = error.file;
                if fs::read(&path)? == input.content.as_bytes() {
                    File::open(&path)?.sync_all()?;
                    sync_artifact_directories(dir)?;
                    return Ok(ToolResultArtifactRef {
                        bytes: input.content.len(),
                        artifact_id: file_name,
                    });
                }
            },
            Err(error) => return Err(error.error),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "too many tool result artifact filename collisions",
    ))
}

fn sync_artifact_directories(dir: &Path) -> std::io::Result<()> {
    sync_directory(Some(dir))?;
    // Always retry the parent fsync. If the first attempt failed after create_dir_all,
    // directory existence alone cannot prove that its parent entry is durable.
    sync_directory(dir.parent())
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn slice_tool_result_content(
    artifact_id: &str,
    content: &str,
    byte_offset: usize,
    max_bytes: usize,
) -> std::io::Result<ToolResultArtifactSlice> {
    validate_slice_request(
        content.len(),
        content.is_char_boundary(byte_offset),
        byte_offset,
        max_bytes,
    )?;
    let mut end = byte_offset.saturating_add(max_bytes).min(content.len());
    while end > byte_offset && !content.is_char_boundary(end) {
        end -= 1;
    }
    Ok(ToolResultArtifactSlice {
        artifact_id: artifact_id.to_string(),
        bytes: content.len(),
        byte_offset,
        returned_bytes: end - byte_offset,
        next_byte_offset: (end < content.len()).then_some(end),
        has_more: end < content.len(),
        content: content[byte_offset..end].to_owned(),
    })
}

pub(crate) fn read_tool_result_file(
    path: &Path,
    artifact_id: &str,
    byte_offset: usize,
    max_bytes: usize,
) -> std::io::Result<ToolResultArtifactSlice> {
    let mut file = File::open(path)?;
    let bytes = usize::try_from(file.metadata()?.len()).map_err(|_| {
        std::io::Error::new(ErrorKind::InvalidData, "tool result artifact is too large")
    })?;
    validate_slice_request(bytes, true, byte_offset, max_bytes)?;
    if byte_offset > 0 && byte_offset < bytes {
        file.seek(SeekFrom::Start(byte_offset as u64))?;
        let mut first = [0_u8; 1];
        file.read_exact(&mut first)?;
        if first[0] & 0b1100_0000 == 0b1000_0000 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "byte_offset is not on a UTF-8 boundary",
            ));
        }
    }

    file.seek(SeekFrom::Start(byte_offset as u64))?;
    let read_limit = max_bytes.min(bytes - byte_offset);
    let mut buffer = vec![0; read_limit];
    file.read_exact(&mut buffer)?;
    let valid_bytes = match std::str::from_utf8(&buffer) {
        Ok(_) => buffer.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(error) => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("tool result artifact contains invalid UTF-8: {error}"),
            ));
        },
    };
    buffer.truncate(valid_bytes);
    let content = String::from_utf8(buffer).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid UTF-8 artifact: {error}"),
        )
    })?;
    let next_byte_offset = byte_offset + valid_bytes;
    Ok(ToolResultArtifactSlice {
        artifact_id: artifact_id.to_owned(),
        bytes,
        byte_offset,
        returned_bytes: valid_bytes,
        next_byte_offset: (next_byte_offset < bytes).then_some(next_byte_offset),
        has_more: next_byte_offset < bytes,
        content,
    })
}

fn validate_slice_request(
    bytes: usize,
    known_boundary: bool,
    byte_offset: usize,
    max_bytes: usize,
) -> std::io::Result<()> {
    if max_bytes < 4 {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "max_bytes must be at least 4",
        ));
    }
    if byte_offset > bytes {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "byte_offset exceeds the artifact length",
        ));
    }
    if !known_boundary {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "byte_offset is not on a UTF-8 boundary",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn artifact_ids_are_opaque_deterministic_and_strictly_validated() {
        let first = tool_result_artifact_id("shell/../../bad", "../call", 0);
        let retry = tool_result_artifact_id("shell/../../bad", "../call", 0);
        let collision = tool_result_artifact_id("shell/../../bad", "../call", 1);

        assert_eq!(first, retry);
        assert_ne!(first, collision);
        assert!(!first.contains("shell"));
        assert!(!first.contains("call"));
        assert!(validate_tool_result_artifact_id(&first).is_ok());
        assert!(validate_tool_result_artifact_id(&collision).is_ok());
        for invalid in [
            "../result.txt",
            "shell-call.txt",
            "result-ABC.txt",
            "result-.txt",
        ] {
            assert!(
                validate_tool_result_artifact_id(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn writing_same_result_reuses_file_and_collision_uses_suffix() {
        let dir = unique_test_dir("tool-results");
        let input = ToolResultArtifactInput {
            call_id: "call-1".into(),
            tool_name: "shell".into(),
            content: "a界bcdef".into(),
        };

        let first = write_tool_result_file(&dir, &input).unwrap();
        let second = write_tool_result_file(&dir, &input).unwrap();
        assert_eq!(first.artifact_id, second.artifact_id);

        let changed = ToolResultArtifactInput {
            content: "changed".into(),
            ..input
        };
        let third = write_tool_result_file(&dir, &changed).unwrap();
        assert_ne!(first.artifact_id, third.artifact_id);

        let first_path = dir.join(&first.artifact_id);
        let third_path = dir.join(&third.artifact_id);
        assert_eq!(std::fs::read_to_string(&first_path).unwrap(), "a界bcdef");
        assert_eq!(std::fs::read_to_string(&third_path).unwrap(), "changed");
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')),
            "successful writes must not leave temporary files"
        );

        let page = read_tool_result_file(&first_path, &first.artifact_id, 1, 4).unwrap();
        assert_eq!(page.content, "界b");
        assert_eq!(page.next_byte_offset, Some(5));
        assert!(read_tool_result_file(&first_path, &first.artifact_id, 2, 4).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn slices_text_and_reports_the_next_offset() {
        let slice = slice_tool_result_content("call.txt", "a界bcdef", 1, 4).unwrap();
        assert_eq!(slice.content, "界b");
        assert_eq!(slice.returned_bytes, 4);
        assert_eq!(slice.next_byte_offset, Some(5));
        assert!(slice.has_more);

        assert!(slice_tool_result_content("call.txt", "a界", 2, 4).is_err());
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
    }
}
