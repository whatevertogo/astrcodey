//! Tool result artifact file helpers.

use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::Path,
};

use astrcode_core::storage::{
    BackgroundTaskOutputSlice, ToolResultArtifactInput, ToolResultArtifactRef,
    ToolResultArtifactSlice,
};

/// 生成 artifact 文件名。
pub fn tool_result_file_name(tool_name: &str, call_id: &str) -> String {
    let safe_tool = sanitize_for_filename(tool_name);
    let safe_call = sanitize_for_filename(call_id);
    format!("{safe_tool}-{safe_call}.txt")
}

/// 写入工具结果 artifact 正文。
pub fn write_tool_result_file(
    dir: &Path,
    input: &ToolResultArtifactInput,
) -> std::io::Result<ToolResultArtifactRef> {
    std::fs::create_dir_all(dir)?;
    for suffix in 0..1000 {
        let file_name = tool_result_file_name_with_suffix(&input.tool_name, &input.call_id, suffix);
        let path = dir.join(file_name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(input.content.as_bytes())?;
                return Ok(ToolResultArtifactRef {
                    bytes: input.content.len(),
                    path: Some(path.display().to_string()),
                });
            },
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if fs::read(&path)? == input.content.as_bytes() {
                    return Ok(ToolResultArtifactRef {
                        bytes: input.content.len(),
                        path: Some(path.display().to_string()),
                    });
                }
            },
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "too many tool result artifact filename collisions",
    ))
}

/// 从 artifact 正文中读取一段字符切片。
pub fn slice_tool_result(
    path: &str,
    content: &str,
    char_offset: usize,
    max_chars: usize,
) -> ToolResultArtifactSlice {
    let mut iter = content.chars().skip(char_offset);
    let text: String = iter.by_ref().take(max_chars).collect();
    let returned_chars = text.chars().count();
    let has_more = iter.next().is_some();
    ToolResultArtifactSlice {
        path: path.to_string(),
        bytes: content.len(),
        char_offset,
        returned_chars,
        next_char_offset: has_more.then_some(char_offset.saturating_add(returned_chars)),
        has_more,
        content: text,
    }
}

fn sanitize_for_filename(input: &str) -> String {
    let sanitized = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect::<String>();
    if sanitized.is_empty() {
        "result".to_string()
    } else {
        sanitized
    }
}

/// 写入后台任务输出到 `{dir}/{task_id}.output`。
///
/// `task_id` 被视为全局唯一，不做碰撞处理。
/// 如果文件已存在则覆盖（允许重写）。
pub fn write_background_task_file(
    dir: &Path,
    task_id: &str,
    content: &str,
) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    let safe_id = sanitize_for_filename(task_id);
    let file_name = format!("{safe_id}.output");
    let path = dir.join(file_name);
    std::fs::write(&path, content.as_bytes())?;
    Ok(content.len())
}

/// 读取后台任务输出的分页切片。
///
/// 复用已有的 `slice_tool_result` 函数进行字符级分页。
/// 文件不存在时返回 `io::ErrorKind::NotFound`。
pub fn read_background_task_file(
    dir: &Path,
    task_id: &str,
    char_offset: usize,
    max_chars: usize,
) -> std::io::Result<BackgroundTaskOutputSlice> {
    let safe_id = sanitize_for_filename(task_id);
    let file_name = format!("{safe_id}.output");
    let path = dir.join(file_name);
    let content = std::fs::read_to_string(&path)?;
    let bytes = content.len();
    let mut iter = content.chars().skip(char_offset);
    let text: String = iter.by_ref().take(max_chars).collect();
    let returned_chars = text.chars().count();
    let has_more = iter.next().is_some();
    Ok(BackgroundTaskOutputSlice {
        task_id: task_id.to_string(),
        bytes,
        char_offset,
        returned_chars,
        next_char_offset: has_more.then_some(char_offset.saturating_add(returned_chars)),
        has_more,
        content: text,
    })
}

fn tool_result_file_name_with_suffix(tool_name: &str, call_id: &str, suffix: usize) -> String {
    let base = tool_result_file_name(tool_name, call_id);
    if suffix == 0 {
        return base;
    }
    let stem = base.trim_end_matches(".txt");
    format!("{stem}-{suffix}.txt")
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn file_name_filters_path_segments() {
        assert_eq!(
            tool_result_file_name("shell/../../bad", "../call"),
            "shellbad-call.txt"
        );
    }

    #[test]
    fn writing_same_result_reuses_file_and_collision_uses_suffix() {
        let dir = unique_test_dir("tool-results");
        let input = ToolResultArtifactInput {
            call_id: "call-1".into(),
            tool_name: "shell".into(),
            content: "abcdef".into(),
        };

        let first = write_tool_result_file(&dir, &input).unwrap();
        let second = write_tool_result_file(&dir, &input).unwrap();
        assert_eq!(first.path, second.path);

        let changed = ToolResultArtifactInput {
            content: "changed".into(),
            ..input
        };
        let third = write_tool_result_file(&dir, &changed).unwrap();
        assert_ne!(first.path, third.path);

        let first_path = PathBuf::from(first.path.unwrap());
        let third_path = PathBuf::from(third.path.unwrap());
        assert_eq!(std::fs::read_to_string(first_path).unwrap(), "abcdef");
        assert_eq!(std::fs::read_to_string(third_path).unwrap(), "changed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn slices_text_with_next_offset() {
        let slice = slice_tool_result("D:/sessions/session/tool-results/call.txt", "abcdef", 2, 3);

        assert_eq!(slice.content, "cde");
        assert_eq!(slice.next_char_offset, Some(5));
        assert!(slice.has_more);
    }

    fn unique_test_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
    }
}
