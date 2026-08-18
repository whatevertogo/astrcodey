//! Session ID validation for filesystem use.
//!
//! Moved from `astrcode-core::types`: the only consumer is the storage
//! boundary, which must reject IDs that could escape the session directory.

/// 标识符验证错误类型。
#[derive(Debug, Clone, thiserror::Error)]
pub enum IdError {
    /// ID 中包含无效字符。
    #[error("Invalid characters in ID: {0}")]
    InvalidCharacters(String),
    /// ID 中存在路径遍历尝试。
    #[error("Path traversal attempt in ID: {0}")]
    PathTraversal(String),
}

/// 验证会话 ID 是否可安全用于文件系统操作。
///
/// 仅允许 ASCII 字母数字、连字符和下划线。
/// 拒绝 `.` 和 `:` 以防止路径遍历攻击。
pub fn validate_session_id(id: &str) -> Result<(), IdError> {
    if id.is_empty() {
        return Err(IdError::InvalidCharacters("empty ID".into()));
    }
    // 检查路径遍历和路径分隔符
    if id.contains("..") || id.contains('/') || id.contains('\\') {
        return Err(IdError::PathTraversal(id.into()));
    }
    // 逐字符检查，仅允许安全字符
    for ch in id.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' {
            return Err(IdError::InvalidCharacters(format!(
                "character '{}' not allowed in ID",
                ch
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_safe_session_ids() {
        for id in ["abc123", "abc-123_xyz", "ABC"] {
            assert!(validate_session_id(id).is_ok(), "{id} should pass");
        }
    }

    #[test]
    fn rejects_traversal_and_unsafe_characters() {
        for id in ["", "..", "a/b", "a\\b", "a:b", "a.b", "a b", "a?b"] {
            assert!(validate_session_id(id).is_err(), "{id} should fail");
        }
    }
}
