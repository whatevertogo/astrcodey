//! 领域错误 → 线缆错误码的映射协议。
//!
//! 错误码目录本身由 `astrcode-extension-contract` 单点定义（宏生成、线缆字符串永久），
//! 此处 re-export 供领域层使用；`[`WireError`]` 让每个领域错误类型映射一次，
//! 宿主路由层统一经 `wire_payload` 构造线缆错误，杜绝同一错误在不同 API 上映射不一致。

use std::fmt::Display;

pub use astrcode_extension_contract::WireErrorCode;

/// 领域错误 → 线缆错误码的映射协议。
///
/// 每个领域错误类型（storage、llm、session API）实现一次，宿主路由层统一
/// 经 [`wire_payload`] 构造线缆错误，杜绝同一错误在不同 API 上映射不一致。
pub trait WireError: Display {
    fn wire_code(&self) -> WireErrorCode;

    fn is_retryable(&self) -> bool {
        false
    }
}

impl WireError for crate::llm::LlmError {
    fn wire_code(&self) -> WireErrorCode {
        match self {
            Self::InvalidApiKey { .. } => WireErrorCode::InvalidApiKey,
            Self::ModelNotFound { .. } => WireErrorCode::ModelNotFound,
            Self::InvalidParameter { .. } => WireErrorCode::InvalidParameter,
            Self::QuotaExceeded { .. } => WireErrorCode::QuotaExceeded,
            Self::ContextWindowExceeded { .. } => WireErrorCode::ContextWindowExceeded,
            Self::RateLimited { .. } => WireErrorCode::RateLimited,
            Self::ClientError { .. } => WireErrorCode::ClientError,
            Self::ServerError { .. } => WireErrorCode::ServerError,
            Self::Transport { .. } => WireErrorCode::Transport,
            Self::StreamDisconnected { .. } => WireErrorCode::StreamDisconnected,
            Self::StreamParse { .. } => WireErrorCode::StreamParse,
            Self::ContentFilter { .. } => WireErrorCode::ContentFiltered,
            Self::TokenLimit { .. } => WireErrorCode::TokenLimit,
            Self::EmptyResponse => WireErrorCode::EmptyResponse,
            Self::Interrupted => WireErrorCode::Cancelled,
            Self::Unsupported { .. } => WireErrorCode::Unsupported,
        }
    }

    fn is_retryable(&self) -> bool {
        self.is_retryable()
    }
}

impl WireError for crate::tool::SessionApiError {
    fn wire_code(&self) -> WireErrorCode {
        match self {
            Self::NotFound(_) => WireErrorCode::SessionNotFound,
            Self::PermissionDenied(_) => WireErrorCode::PermissionDenied,
            Self::SessionBusy(_) => WireErrorCode::SessionBusy,
            Self::MaxDepthExceeded { .. } => WireErrorCode::MaxDepthExceeded,
            Self::Unsupported(_) => WireErrorCode::Unsupported,
            Self::Internal(_) => WireErrorCode::InternalError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WireError, WireErrorCode};

    #[test]
    fn domain_errors_map_consistently() {
        use crate::{llm::LlmError, tool::SessionApiError};

        assert_eq!(LlmError::Interrupted.wire_code(), WireErrorCode::Cancelled);
        assert_eq!(
            SessionApiError::NotFound("child".into()).wire_code(),
            WireErrorCode::SessionNotFound
        );
        assert!(
            LlmError::RateLimited {
                status: 429,
                retry_after_ms: None,
                message: "slow down".into(),
            }
            .is_retryable()
        );
        assert!(!SessionApiError::NotFound("child".into()).is_retryable());
    }
}
