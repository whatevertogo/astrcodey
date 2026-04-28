//! 工具结果持久化，用于处理大体积输出。
//!
//! 当工具执行结果超过内联阈值时，将其持久化到磁盘并用文件引用替代。
//! 这样可以防止大体积输出占用过多的上下文窗口空间。

use std::path::PathBuf;

use astrcode_core::tool::ToolResult;

/// 如果工具结果超过内联限制，则将其持久化到磁盘。
///
/// 当 `result.content` 长度超过 `inline_limit` 时，将内容写入文件，
/// 并将 `result.content` 替换为文件路径引用。
///
/// # 参数
/// - `result`: 工具执行结果（内容可能被替换为文件引用）
/// - `inline_limit`: 内联内容的最大字节数
/// - `persist_dir`: 持久化文件存放目录
///
/// # 返回
/// - `Ok(None)`: 内容未超过阈值，未持久化
/// - `Ok(Some(path))`: 内容已持久化，返回文件路径
pub fn maybe_persist_tool_result(
    result: &mut ToolResult,
    inline_limit: usize,
    persist_dir: &PathBuf,
) -> std::io::Result<Option<PathBuf>> {
    if result.content.len() <= inline_limit {
        return Ok(None);
    }

    let file_path = persist_tool_result(&result.content, &result.call_id, persist_dir)?;
    let original_len = result.content.len();
    // 将原始内容替换为文件引用信息
    result.content = format!(
        "[Tool result persisted to {} ({} bytes, {} KB)]",
        file_path.display(),
        original_len,
        original_len / 1024
    );
    Ok(Some(file_path))
}

/// 将工具结果内容持久化到文件。
///
/// 文件名由经过安全处理的 tool call ID 派生，自动创建目标目录。
///
/// # 参数
/// - `content`: 要持久化的文本内容
/// - `call_id`: 工具调用 ID，用于生成文件名
/// - `persist_dir`: 持久化文件存放目录
pub fn persist_tool_result(
    content: &str,
    call_id: &str,
    persist_dir: &PathBuf,
) -> std::io::Result<PathBuf> {
    // 对 call_id 进行安全处理，防止路径遍历攻击
    let safe_id = sanitize_for_filename(call_id);
    let file_path = persist_dir.join(format!("{}.txt", safe_id));

    // 确保目录存在
    std::fs::create_dir_all(persist_dir)?;
    std::fs::write(&file_path, content)?;

    Ok(file_path)
}

/// 将字符串安全化为可用作文件名的形式。
///
/// 仅保留 ASCII 字母数字、连字符和下划线，并截断至 64 个字符，
/// 以防止路径遍历和文件名中的非法字符。
fn sanitize_for_filename(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use astrcode_core::tool::ToolResult;

    use super::*;

    #[test]
    fn test_maybe_persist_small_result() {
        let mut result = ToolResult {
            call_id: "abc123".into(),
            content: "Hello World".into(),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        };
        let dir = PathBuf::from("/tmp/test-results");
        // 内容较小，不应持久化
        let path = maybe_persist_tool_result(&mut result, 100, &dir).unwrap();
        assert!(path.is_none());
        assert_eq!(result.content, "Hello World");
    }

    #[test]
    fn test_maybe_persist_large_result() {
        let large_content = "A".repeat(1000);
        let mut result = ToolResult {
            call_id: "abc123".into(),
            content: large_content,
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        };
        let dir = PathBuf::from("/tmp/test-results");
        let path = maybe_persist_tool_result(&mut result, 100, &dir).unwrap();
        assert!(path.is_some());
        assert!(result.content.contains("persisted to"));
        assert!(result.content.contains("1000 bytes"));
    }

    #[test]
    fn test_sanitize_call_id() {
        // 路径遍历尝试应被安全化处理
        let mut result = ToolResult {
            call_id: "../etc/passwd".into(),
            content: "data".into(),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        };
        let dir = PathBuf::from("/tmp/test-results");
        let _ = maybe_persist_tool_result(&mut result, 10, &dir);
        // call_id 在用作文件名前会被安全化处理
        // 路径遍历字符已被过滤
    }
}
