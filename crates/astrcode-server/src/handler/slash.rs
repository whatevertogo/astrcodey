//! 斜杠命令传输层错误映射。

use super::HandlerError;

/// 将 HandlerError 映射为错误码。
pub(in crate::handler) fn command_error_code(error: &HandlerError) -> i32 {
    match error {
        HandlerError::UnknownCommand(_) => 40402,
        _ => -32603,
    }
}
